//! Sessions stopped at a usage limit (spec section 21): when the `StopFailure` hook wakes a
//! session, and how the status line and `scan` follow it afterwards.

use ccr_core::scheduler::Reason;
use ccr_core::statusline::{self, StatusInput};
use ccr_core::store::{AccountIdentity, AccountState, Sample, SampleWindow, WaitingState, kind};
use ccr_core::swap::{self, SwapRequest};
use ccr_core::waiting::{self, Decision, WakeReason};
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

fn five_hour(account: i64, session: Option<&str>, five: f64, ts: i64, resets_at: i64) -> Sample {
    Sample {
        ts,
        account_id: account,
        profile: "rotate".into(),
        session_id: session.map(str::to_string),
        model: None,
        windows: vec![SampleWindow {
            name: "five_hour".into(),
            pct: five,
            resets_at: Some(resets_at),
        }],
    }
}

/// A reading of the account from any session, with its 5-hour window resetting in an hour.
fn reading(store: &Store, account: i64, five: f64, ts: i64) {
    store
        .record_sample(&five_hour(account, None, five, ts, ts / 1000 + 3600))
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

fn woken(decision: Decision) -> (i64, WakeReason) {
    match decision {
        Decision::Wake(account, reason) => (account.id, reason),
        other => panic!("expected a wake, got {other:?}"),
    }
}

fn waits(decision: Decision) {
    assert!(matches!(decision, Decision::Wait), "{decision:?}");
}

#[test]
fn waits_on_the_account_it_stopped_on_and_wakes_after_a_swap() {
    let e = env();
    let now = time::now_ms();
    reading(&e.store, e.a, 100.0, now - 1_000);
    reading(&e.store, e.b, 10.0, now - 60_000);
    let ws = stop(&e, "s1", now);
    waits(waiting::decide(&e.store, &e.paths, &ws, now).unwrap());

    swap_to_b(&e);
    let (account, reason) =
        woken(waiting::decide(&e.store, &e.paths, &ws, time::now_ms()).unwrap());
    assert_eq!((account, reason), (e.b, WakeReason::Switched));
    let b = e.store.account(e.b).unwrap().unwrap();
    assert!(
        waiting::wake_message(&b, WakeReason::Switched)
            .contains("switched this Claude Code profile to the account b, which has usage left")
    );
}

#[test]
fn other_sessions_readings_never_count_as_room_on_the_account_it_stopped_on() {
    let e = env();
    let now = time::now_ms();
    reading(&e.store, e.a, 97.0, now - 5_000);
    let ws = stop(&e, "s1", now);
    // An idle session repeats its pre-limit numbers with a fresh time: that is no reset.
    reading(&e.store, e.a, 85.0, now + 1_000);
    waits(waiting::decide(&e.store, &e.paths, &ws, now + 2_000).unwrap());
    std::fs::write(
        e.paths.settings_file(),
        r#"{"autoContinueAtUsageLimit": false}"#,
    )
    .unwrap();
    waits(waiting::decide(&e.store, &e.paths, &ws, now + 2_000).unwrap());
}

#[test]
fn wakes_on_the_same_account_after_its_reset_only_with_claude_codes_continue_off() {
    let e = env();
    let now = time::now_ms();
    let ws = stop(&e, "s1", now);
    // The stop's own reading: at the limit until it resets 10 minutes later.
    let resets = now / 1000 + 600;
    e.store
        .record_sample(&five_hour(e.a, Some("s1"), 100.0, now + 500, resets))
        .unwrap();
    let after_reset = (resets + 60) * 1000;
    // With Claude Code's own continue on, it continues the session itself.
    waits(waiting::decide(&e.store, &e.paths, &ws, after_reset).unwrap());

    std::fs::write(
        e.paths.settings_file(),
        r#"{"autoContinueAtUsageLimit": false}"#,
    )
    .unwrap();
    waits(waiting::decide(&e.store, &e.paths, &ws, now + 60_000).unwrap());
    let (account, reason) = woken(waiting::decide(&e.store, &e.paths, &ws, after_reset).unwrap());
    assert_eq!((account, reason), (e.a, WakeReason::LimitReset));
    let a = e.store.account(e.a).unwrap().unwrap();
    assert!(
        waiting::wake_message(&a, WakeReason::LimitReset)
            .contains("usage limit of the account a has reset")
    );
}

#[test]
fn a_swap_that_landed_just_before_the_stop_wakes_at_once() {
    let e = env();
    let now = time::now_ms();
    reading(&e.store, e.a, 100.0, now - 120_000);
    reading(&e.store, e.b, 10.0, now - 120_000);
    swap_to_b(&e);
    // The turn's request still carried a's login: it stops moments after the swap, with b in
    // the profile.
    let since = time::now_ms() + 5_000;
    let ws = e.store.start_waiting("s1", since, Some(e.b), None).unwrap();
    assert_eq!(
        woken(waiting::decide(&e.store, &e.paths, &ws, since).unwrap()).0,
        e.b
    );

    // Long after the swap, a stop on b is b's own limit.
    let later = time::now_ms() + 600_000;
    let ws = e.store.start_waiting("s2", later, Some(e.b), None).unwrap();
    waits(waiting::decide(&e.store, &e.paths, &ws, later).unwrap());
}

#[test]
fn a_first_activation_just_before_the_stop_is_no_reason_to_wake() {
    let e = env();
    let now = time::now_ms();
    reading(&e.store, e.a, 50.0, now - 1_000);
    e.store
        .log_event(kind::SWAP, None, Some(e.a), Some("manual"), None)
        .unwrap();
    let ws = stop(&e, "s1", time::now_ms() + 1_000);
    waits(waiting::decide(&e.store, &e.paths, &ws, ws.since_ms).unwrap());
}

#[test]
fn never_wakes_a_session_twice_on_one_account_while_it_keeps_stopping() {
    let e = env();
    let now = time::now_ms();
    reading(&e.store, e.a, 100.0, now - 1_000);
    reading(&e.store, e.b, 10.0, now - 60_000);
    let ws = stop(&e, "s1", now);
    swap_to_b(&e);
    let b = e.store.account(e.b).unwrap().unwrap();
    assert!(waiting::note_woken(&e.store, &ws, &b, now + 1_000).unwrap());

    // Woken on b, it stops again: b may really be at its limit, so no second wake there, even
    // though the swap to b landed just before.
    let again = e
        .store
        .start_waiting("s1", now + 5_000, Some(e.b), None)
        .unwrap();
    assert_eq!(again.woken_accounts, vec![e.b]);
    waits(waiting::decide(&e.store, &e.paths, &again, now + 6_000).unwrap());

    // The hook of the replaced stop can no longer record a wake.
    assert!(!waiting::note_woken(&e.store, &ws, &b, now + 7_000).unwrap());

    // Long after the last wake, a new stop forgets the episode.
    let much_later = now + ccr_core::store::WAKE_MEMORY_MS + 60_000;
    let fresh = e
        .store
        .start_waiting("s1", much_later, Some(e.b), None)
        .unwrap();
    assert_eq!(fresh.woken_account_id, None);
    assert!(fresh.woken_accounts.is_empty());
}

#[test]
fn a_session_that_worked_again_starts_a_new_episode() {
    let e = env();
    let now = time::now_ms();
    reading(&e.store, e.b, 10.0, now - 60_000);
    let ws = stop(&e, "s1", now);
    swap_to_b(&e);
    let b = e.store.account(e.b).unwrap().unwrap();
    assert!(waiting::note_woken(&e.store, &ws, &b, now + 1_000).unwrap());
    let working = five_hour(e.b, Some("s1"), 20.0, now + 30_000, now / 1000 + 7_200);
    waiting::note_reading(&e.store, "s1", e.b, &working, now + 30_000).unwrap();
    assert_eq!(
        e.store.waiting_session("s1").unwrap().unwrap().state,
        WaitingState::Resumed
    );
    let next = e
        .store
        .start_waiting("s1", now + 600_000, Some(e.b), None)
        .unwrap();
    assert!(next.woken_accounts.is_empty());
}

#[test]
fn a_refused_request_is_no_sign_of_work_but_room_is_even_unwoken() {
    let e = env();
    let now = time::now_ms();
    let b = e.store.account(e.b).unwrap().unwrap();
    let ws = stop(&e, "woken", now);
    assert!(waiting::note_woken(&e.store, &ws, &b, now + 1_000).unwrap());
    // The woken turn was refused: its reading is at the limit.
    let refused = five_hour(e.b, Some("woken"), 100.0, now + 2_000, now / 1000 + 3_600);
    waiting::note_reading(&e.store, "woken", e.b, &refused, now + 2_000).unwrap();
    assert_eq!(
        e.store.waiting_session("woken").unwrap().unwrap().state,
        WaitingState::Woken
    );

    // A session that continued by itself (Claude Code's own continue, or the user) with room is
    // working again too, so its hook stops waiting.
    stop(&e, "itself", now);
    let fine = five_hour(e.a, Some("itself"), 3.0, now + 5_000, now / 1000 + 18_000);
    waiting::note_reading(&e.store, "itself", e.a, &fine, now + 5_000).unwrap();
    assert_eq!(
        e.store.waiting_session("itself").unwrap().unwrap().state,
        WaitingState::Resumed
    );
}

#[test]
fn gives_up_after_the_longest_wait() {
    let e = env();
    let now = time::now_ms();
    let ws = stop(&e, "s1", now);
    assert!(matches!(
        waiting::decide(&e.store, &e.paths, &ws, now + waiting::MAX_WAIT_MS).unwrap(),
        Decision::TimedOut
    ));
}

fn input(session: &str, pct: f64, resets_at: i64) -> StatusInput {
    StatusInput::parse(
        &json!({
            "session_id": session,
            "version": ccr_core::doctor::TESTED_CLAUDE_VERSIONS[0],
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

    // Minutes later, the quiet one has said nothing: the user is told, once, however many
    // scans run.
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
fn state_changes_only_apply_from_the_state_read() {
    let e = env();
    let now = time::now_ms();
    let ws = stop(&e, "s1", now);
    let b = e.store.account(e.b).unwrap().unwrap();
    assert!(waiting::note_woken(&e.store, &ws, &b, now + 1_000).unwrap());
    // A scan that read the stop as waiting, before the wake, cannot end it now.
    assert!(
        !e.store
            .set_waiting_state(
                "s1",
                ws.since_ms,
                &[WaitingState::Waiting],
                WaitingState::Ended,
                None,
                now + 2_000,
            )
            .unwrap()
    );
    // Nor can one that read it as woken mark it stuck once the status line saw it working.
    let working = five_hour(e.b, Some("s1"), 20.0, now + 3_000, now / 1000 + 7_200);
    waiting::note_reading(&e.store, "s1", e.b, &working, now + 3_000).unwrap();
    assert!(
        !e.store
            .set_waiting_state(
                "s1",
                ws.since_ms,
                &[WaitingState::Woken],
                WaitingState::Stuck,
                None,
                now + 4_000,
            )
            .unwrap()
    );
    assert_eq!(
        e.store.waiting_session("s1").unwrap().unwrap().state,
        WaitingState::Resumed
    );
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
