//! Sessions stopped at a usage limit (spec section 21): when the `StopFailure` hook wakes a
//! session, and how the status line and `scan` follow it afterwards.

use ccr_core::scheduler::Reason;
use ccr_core::statusline::{self, StatusInput};
use ccr_core::store::{AccountIdentity, AccountState, Sample, SampleWindow, WaitingState, kind};
use ccr_core::swap::{self, SwapRequest};
use ccr_core::waiting::{self, Decision, GiveUp};
use ccr_core::{Paths, Store, time};
use serde_json::json;

struct Env {
    _root: tempfile::TempDir,
    paths: Paths,
    store: Store,
    a: i64,
    b: i64,
}

/// Accounts `a` (active, its login in the profile) and `b` (parked), with usage checks off.
fn env() -> Env {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let home = root.path().join("home");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let paths = Paths::new(root.path().join("data"), profile).with_home(home);
    let store = Store::open(&paths.store_db()).unwrap();
    store.set_setting("usage_checks", Some("false")).unwrap();
    store
        .set_setting("use_expiring_weekly", Some("false"))
        .unwrap();
    let ident = |u: &str| AccountIdentity {
        account_uuid: Some(u.into()),
        email: Some(format!("{u}@x.test")),
        ..Default::default()
    };
    let a = store
        .insert_account("a", &ident("ua"), AccountState::Active)
        .unwrap();
    let b = store
        .insert_account("b", &ident("ub"), AccountState::Parked)
        .unwrap();
    for (id, tag) in [(a, "a"), (b, "b")] {
        store
            .save_credential(
                id,
                &json!({"accessToken": format!("access-{tag}"), "refreshToken": format!("refresh-{tag}"),
                        "expiresAt": time::now_ms() + 3_600_000}),
                "login",
            )
            .unwrap();
    }
    std::fs::write(
        paths.credentials_file(),
        json!({"claudeAiOauth": {"accessToken": "access-a", "refreshToken": "refresh-a",
                                  "expiresAt": time::now_ms() + 3_600_000}})
        .to_string(),
    )
    .unwrap();
    Env {
        _root: root,
        paths,
        store,
        a,
        b,
    }
}

fn reading(store: &Store, account: i64, five: f64, ts: i64) {
    store
        .record_sample(&Sample {
            ts,
            account_id: account,
            profile: "rotate".into(),
            session_id: None,
            model: None,
            windows: vec![SampleWindow {
                name: "five_hour".into(),
                pct: five,
                resets_at: Some(ts / 1000 + 3600),
            }],
        })
        .unwrap();
}

fn stop(e: &Env, session: &str, since: i64) -> ccr_core::store::WaitingSession {
    e.store
        .start_waiting(session, since, Some(e.a), Some(waiting::this_hook()))
        .unwrap()
}

fn swap_to_b(e: &Env) {
    let out = swap::run(
        &e.store,
        &e.paths,
        &SwapRequest {
            from: Some(e.a),
            to: None,
            reason: Reason::StopFailure,
            wait: true,
        },
    )
    .unwrap();
    assert!(matches!(out, swap::SwapOutcome::Swapped { .. }), "{out:?}");
}

#[test]
fn waits_on_the_account_it_stopped_on_and_wakes_after_a_swap() {
    let e = env();
    let now = time::now_ms();
    reading(&e.store, e.a, 100.0, now - 1_000);
    reading(&e.store, e.b, 10.0, now - 60_000);
    // Long after the last swap: nothing has moved yet.
    e.store
        .set_meta(swap::META_ACTIVE_SINCE, now - 3_600_000)
        .unwrap();
    let ws = stop(&e, "s1", now);
    assert!(matches!(
        waiting::decide(&e.store, &e.paths, &ws, now).unwrap(),
        Decision::Wait
    ));

    swap_to_b(&e);
    match waiting::decide(&e.store, &e.paths, &ws, time::now_ms()).unwrap() {
        Decision::Wake(account) => assert_eq!(account.id, e.b),
        other => panic!("{other:?}"),
    }
    assert!(
        waiting::wake_message(&e.store.account(e.b).unwrap().unwrap())
            .contains("the account b, which has usage left")
    );
}

#[test]
fn a_reading_from_before_the_stop_does_not_count_as_room() {
    let e = env();
    let now = time::now_ms();
    // At 97% just before the turn jumped to its limit: the reading of the jump comes later.
    reading(&e.store, e.a, 97.0, now - 5_000);
    e.store
        .set_meta(swap::META_ACTIVE_SINCE, now - 3_600_000)
        .unwrap();
    let ws = stop(&e, "s1", now);
    assert!(matches!(
        waiting::decide(&e.store, &e.paths, &ws, now).unwrap(),
        Decision::Wait
    ));

    // A reading taken since, with room: its limit has reset, and Claude Code continues the
    // session itself.
    reading(&e.store, e.a, 2.0, now + 1_000);
    assert!(matches!(
        waiting::decide(&e.store, &e.paths, &ws, now + 2_000).unwrap(),
        Decision::GiveUp(GiveUp::LimitReset)
    ));

    // Unless the user turned that off: then cc-rotate wakes it.
    std::fs::write(
        e.paths.settings_file(),
        r#"{"autoContinueAtUsageLimit": false}"#,
    )
    .unwrap();
    assert!(matches!(
        waiting::decide(&e.store, &e.paths, &ws, now + 2_000).unwrap(),
        Decision::Wake(_)
    ));
}

#[test]
fn a_swap_that_landed_just_before_the_stop_wakes_once_the_account_shows_room() {
    let e = env();
    let now = time::now_ms();
    reading(&e.store, e.a, 100.0, now - 120_000);
    reading(&e.store, e.b, 10.0, now - 120_000);
    swap_to_b(&e);
    // The turn's request still carried a's login: it stops a moment after the swap, with b in
    // the profile.
    let since = time::now_ms() + 30_000;
    let ws = e.store.start_waiting("s1", since, Some(e.b), None).unwrap();
    // Until a reading since shows room on b, the stop may as well be b's own limit.
    assert!(matches!(
        waiting::decide(&e.store, &e.paths, &ws, since).unwrap(),
        Decision::Wait
    ));
    reading(&e.store, e.b, 11.0, since + 1_000);
    match waiting::decide(&e.store, &e.paths, &ws, since + 2_000).unwrap() {
        Decision::Wake(account) => assert_eq!(account.id, e.b),
        other => panic!("{other:?}"),
    }

    // A first activation just before the stop, and the account at its limit by a reading since:
    // the stop is its own.
    let at_limit = e
        .store
        .start_waiting("s2", since + 3_000, Some(e.b), None)
        .unwrap();
    reading(&e.store, e.b, 100.0, since + 4_000);
    assert!(matches!(
        waiting::decide(&e.store, &e.paths, &at_limit, since + 5_000).unwrap(),
        Decision::Wait
    ));

    // Long after the swap, room on b by a reading since means its limit reset: Claude Code
    // continues the session itself.
    let later_since = time::now_ms() + 600_000;
    let later = e
        .store
        .start_waiting("s3", later_since, Some(e.b), None)
        .unwrap();
    reading(&e.store, e.b, 3.0, later_since + 1_000);
    assert!(matches!(
        waiting::decide(&e.store, &e.paths, &later, later_since + 2_000).unwrap(),
        Decision::GiveUp(GiveUp::LimitReset)
    ));
}

#[test]
fn never_wakes_a_session_twice_on_one_account() {
    let e = env();
    let now = time::now_ms();
    reading(&e.store, e.a, 100.0, now - 1_000);
    reading(&e.store, e.b, 10.0, now - 60_000);
    let ws = stop(&e, "s1", now);
    swap_to_b(&e);
    let b = e.store.account(e.b).unwrap().unwrap();
    assert!(waiting::note_woken(&e.store, &ws, &b, now + 1_000).unwrap());

    // Woken on b, it stops again: b may really be at its limit, so no second wake there.
    let again = e
        .store
        .start_waiting("s1", now + 5_000, Some(e.b), None)
        .unwrap();
    assert_eq!(again.woken_account_id, Some(e.b));
    assert!(matches!(
        waiting::decide(&e.store, &e.paths, &again, now + 6_000).unwrap(),
        Decision::Wait
    ));

    // The hook of the replaced stop can no longer record a wake.
    assert!(!waiting::note_woken(&e.store, &ws, &b, now + 7_000).unwrap());

    // Long after, a new stop forgets that wake.
    let much_later = now + ccr_core::store::WAKE_MEMORY_MS + 60_000;
    let fresh = e
        .store
        .start_waiting("s1", much_later, Some(e.b), None)
        .unwrap();
    assert_eq!(fresh.woken_account_id, None);
}

#[test]
fn gives_up_after_the_longest_wait() {
    let e = env();
    let now = time::now_ms();
    let ws = stop(&e, "s1", now);
    assert!(matches!(
        waiting::decide(&e.store, &e.paths, &ws, now + waiting::MAX_WAIT_MS).unwrap(),
        Decision::GiveUp(GiveUp::TimedOut)
    ));
}

fn input(session: &str, pct: f64, resets_at: i64) -> StatusInput {
    StatusInput::parse(
        &json!({
            "session_id": session,
            "version": ccr_core::doctor::TESTED_CLAUDE_VERSIONS[0],
            "cost": {"total_api_duration_ms": time::now_ms()},
            "rate_limits": {"five_hour": {"used_percentage": pct, "resets_at": resets_at}}
        })
        .to_string(),
    )
}

#[test]
fn a_woken_session_reporting_usage_is_resumed_and_a_silent_one_is_stuck() {
    let e = env();
    let now = time::now_ms();
    reading(&e.store, e.a, 100.0, now - 1_000);
    reading(&e.store, e.b, 10.0, now - 60_000);
    swap_to_b(&e);
    let b = e.store.account(e.b).unwrap().unwrap();
    let woken_at = time::now_ms();
    for session in ["quiet", "busy"] {
        let ws = e
            .store
            .start_waiting(session, woken_at - 10_000, Some(e.a), None)
            .unwrap();
        assert!(waiting::note_woken(&e.store, &ws, &b, woken_at).unwrap());
    }

    // The busy session's next reply reports b's usage.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let b_resets = time::now_ms() / 1000 + 7_200;
    statusline::tick(
        &e.store,
        &e.paths,
        &input("busy", 12.0, b_resets),
        time::now_ms(),
    )
    .unwrap();
    let busy = e.store.waiting_session("busy").unwrap().unwrap();
    assert_eq!(busy.state, WaitingState::Resumed);

    // Minutes later, the quiet one has said nothing: the user is told, once.
    let later = woken_at + waiting::STUCK_AFTER_MS + 1_000;
    waiting::scan(&e.store, later).unwrap();
    waiting::scan(&e.store, later + 60_000).unwrap();
    assert_eq!(
        e.store.waiting_session("quiet").unwrap().unwrap().state,
        WaitingState::Stuck
    );
    let stuck: Vec<_> = e
        .store
        .events(100)
        .unwrap()
        .into_iter()
        .filter(|ev| ev.kind == kind::SESSION_STUCK)
        .collect();
    assert_eq!(stuck.len(), 1);
    assert_eq!(stuck[0].account_to, Some(e.b));
    assert_eq!(stuck[0].detail.as_ref().unwrap()["session"], "quiet");

    let reasons: Vec<_> = e
        .store
        .events(100)
        .unwrap()
        .into_iter()
        .filter(|ev| ev.kind == kind::SESSION)
        .filter_map(|ev| ev.reason)
        .collect();
    assert!(reasons.contains(&"woken".to_string()));
    assert!(reasons.contains(&"resumed".to_string()));
}

#[test]
fn a_waiting_session_whose_hook_is_gone_is_over() {
    let e = env();
    let now = time::now_ms();
    // A process id that cannot be running.
    e.store
        .start_waiting("gone", now, Some(e.a), Some((u32::MAX - 1, None)))
        .unwrap();
    e.store
        .start_waiting("here", now, Some(e.a), Some(waiting::this_hook()))
        .unwrap();
    waiting::scan(&e.store, now + 1_000).unwrap();
    assert_eq!(
        e.store.waiting_session("gone").unwrap().unwrap().state,
        WaitingState::Ended
    );
    assert_eq!(
        e.store.waiting_session("here").unwrap().unwrap().state,
        WaitingState::Waiting
    );
}
