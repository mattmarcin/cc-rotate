//! What each account's use is worth at API prices (spec section 22.1), and its extra usage
//! (22.2): counting the status line's cost, the paid-usage warning, and moving off an account
//! that would bill extra usage.

use ccr_core::scheduler::Reason;
use ccr_core::statusline::{self, StatusInput};
use ccr_core::store::{AccountIdentity, AccountState, ExtraUsage, Sample, SampleWindow, kind};
use ccr_core::{Paths, Store, extra_usage, swap, time};
use serde_json::json;

struct Env {
    _root: tempfile::TempDir,
    paths: Paths,
    store: Store,
    a: i64,
    b: i64,
    c: i64,
}

/// `a` active with its login in the profile, `b` and `c` parked; usage checks off.
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
    let mut ids = Vec::new();
    for (label, state) in [
        ("a", AccountState::Active),
        ("b", AccountState::Parked),
        ("c", AccountState::Parked),
    ] {
        let id = store
            .insert_account(
                label,
                &AccountIdentity {
                    account_uuid: Some(format!("u{label}")),
                    ..Default::default()
                },
                state,
            )
            .unwrap();
        store
            .save_credential(
                id,
                &json!({"accessToken": format!("access-{label}"),
                        "refreshToken": format!("refresh-{label}"),
                        "expiresAt": time::now_ms() + 3_600_000}),
                "login",
            )
            .unwrap();
        ids.push(id);
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
        a: ids[0],
        b: ids[1],
        c: ids[2],
    }
}

fn five_hour(store: &Store, account: i64, pct: f64, resets_in_s: i64) {
    let now = time::now_ms();
    store
        .record_sample(&Sample {
            ts: now,
            account_id: account,
            profile: "rotate".into(),
            session_id: None,
            model: None,
            windows: vec![SampleWindow {
                name: "five_hour".into(),
                pct,
                resets_at: Some(now / 1000 + resets_in_s),
            }],
        })
        .unwrap();
}

fn extra(enabled: bool, used: i64) -> ExtraUsage {
    ExtraUsage {
        enabled,
        monthly_limit: Some(5000),
        used: Some(used),
        utilization: Some(used as f64 / 50.0),
        currency: Some("USD".into()),
        disabled_reason: None,
    }
}

#[test]
fn a_session_counts_only_what_it_adds() {
    let e = env();
    let hour = 1_000_000 * 3_600_000;
    let s = &e.store;
    let s1 = |total: f64, new: bool, account: i64, at: i64| {
        s.note_session_cost("s1", "100:1", total, new, account, at)
            .unwrap()
    };
    // The status line runs as a session starts, at $0: counting starts there.
    assert_eq!(s1(0.0, true, e.a, hour), 0.0);
    assert_eq!(s1(0.50, false, e.a, hour + 30_000), 0.50);
    assert_eq!(s1(0.75, false, e.a, hour + 60_000), 0.25);
    assert_eq!(s1(0.75, false, e.a, hour + 120_000), 0.0);
    // A drop (a /login resets the total) is only where counting goes on from.
    assert_eq!(s1(0.60, false, e.a, hour + 180_000), 0.0);
    let added = s1(0.70, false, e.b, hour + 3_600_000);
    assert!((added - 0.10).abs() < 1e-9, "{added}");
    assert_eq!(s1(f64::NAN, true, e.a, hour), 0.0);
    assert_eq!(s1(-1.0, true, e.a, hour), 0.0);
    // A new session first seen moments after it started counts its first total whole.
    let fresh = s
        .note_session_cost("fresh", "200:1", 0.30, true, e.c, hour + 3_620_000)
        .unwrap();
    assert_eq!(fresh, 0.30);
    // A session that ran a while before it was first seen (running before cc-rotate counted,
    // resumed or forked with the run time it had) carries what it spent before.
    let old = |total: f64, at: i64| {
        s.note_session_cost("old", "300:1", total, false, e.a, at)
            .unwrap()
    };
    assert_eq!(old(80.0, hour), 0.0);
    assert_eq!(old(80.5, hour + 60_000), 0.5);
    // A second process of a session already seen is a resume, however short: a starting point.
    let resumed = s
        .note_session_cost("s1", "400:1", 0.70, true, e.a, hour + 3_700_000)
        .unwrap();
    assert_eq!(resumed, 0.0);

    let from = hour / 1000;
    let a = s
        .account_cost_between(e.a, from, from + 3_600)
        .unwrap()
        .unwrap();
    assert!((a - 1.25).abs() < 1e-9, "{a}");
    assert_eq!(
        s.account_cost_between(e.c, from, from + 3_600).unwrap(),
        None
    );
    let b = s
        .account_cost_between(e.b, from, from + 7_200)
        .unwrap()
        .unwrap();
    assert!((b - 0.10).abs() < 1e-9, "{b}");
}

fn tick_input(session: &str, cost: f64, ran_ms: i64, pct: f64, resets_at: i64) -> StatusInput {
    StatusInput::parse(
        &json!({
            "session_id": session,
            "cost": {"total_cost_usd": cost, "total_duration_ms": ran_ms},
            "rate_limits": {"five_hour": {"used_percentage": pct, "resets_at": resets_at}}
        })
        .to_string(),
    )
}

#[test]
fn the_status_line_counts_cost_for_the_account_that_served_it() {
    let e = env();
    let start = time::now_ms();
    let a_resets = start / 1000 + 3_600;
    let tick = |cost: f64, at: i64, pct: f64, resets: i64| {
        statusline::tick(
            &e.store,
            &e.paths,
            &tick_input("s1", cost, at - start, pct, resets),
            at,
        )
        .unwrap();
    };
    tick(0.0, start, 20.0, a_resets);
    tick(1.00, start + 20_000, 20.0, a_resets);
    swap::run(
        &e.store,
        &e.paths,
        &swap::SwapRequest {
            from: Some(e.a),
            to: Some(e.b),
            reason: Reason::Manual,
            wait: true,
        },
    )
    .unwrap();
    // Its request started on a's login before the swap: the reading is still a's.
    let later = time::now_ms() + 1_000;
    tick(1.40, later, 22.0, a_resets);
    // Then it works on b.
    tick(2.00, later + 30_000, 5.0, start / 1000 + 9_000);

    let from = start / 1000 - 3_600;
    let to = start / 1000 + 7_200;
    let a = e
        .store
        .account_cost_between(e.a, from, to)
        .unwrap()
        .unwrap();
    let b = e
        .store
        .account_cost_between(e.b, from, to)
        .unwrap()
        .unwrap();
    assert!((a - 1.40).abs() < 1e-9, "a {a}");
    assert!((b - 0.60).abs() < 1e-9, "b {b}");
}

/// The status line runs under a process of its own each time, so a session's Claude Code process
/// is told by when it started (`now - total_duration_ms`), which each tick reckons a little
/// differently; two processes of one session (resumed twice) each keep their own count.
#[test]
fn the_status_line_tells_a_sessions_processes_by_when_they_started() {
    let e = env();
    let start = time::now_ms();
    let resets = start / 1000 + 3_600;
    let tick = |cost: f64, at: i64, ran_ms: i64| {
        statusline::tick(
            &e.store,
            &e.paths,
            &tick_input("s1", cost, ran_ms, 20.0, resets),
            at,
        )
        .unwrap();
    };
    // The first process started 120 s before `start`; it ran before cc-rotate counted.
    tick(1.0, start, 120_000);
    tick(1.5, start + 30_000, 150_300);
    // A second one, resumed with the run time the session had, started 90 s before `start`.
    tick(1.5, start + 60_000, 150_000);
    tick(1.8, start + 90_000, 180_200);
    tick(1.7, start + 95_000, 214_800);
    // Hours on both sides, whenever in the hour this runs: the ticks above span up to 95 s.
    let counted = e
        .store
        .account_cost_between(e.a, start / 1000 - 3_600, start / 1000 + 7_200)
        .unwrap()
        .unwrap();
    // 0.5 and 0.2 from the first, 0.3 from the second.
    assert!((counted - 1.0).abs() < 1e-9, "{counted}");
}

#[test]
fn two_processes_on_one_session_each_keep_their_own_count() {
    let e = env();
    let now = time::now_ms();
    // Resumed twice, seconds apart: both start from the $5 the session had.
    let mut counted = 0.0;
    for (process, total, at) in [
        ("first", 5.0, now),
        ("second", 5.0, now + 1_000),
        ("first", 5.5, now + 30_000),
        ("second", 5.2, now + 31_000),
        ("first", 5.6, now + 60_000),
        ("second", 5.3, now + 61_000),
    ] {
        counted += e
            .store
            .note_session_cost("s1", process, total, false, e.a, at)
            .unwrap();
    }
    assert!((counted - 0.90).abs() < 1e-9, "{counted}");
}

#[test]
fn a_newer_extra_usage_reading_wins() {
    let e = env();
    e.store
        .set_extra_usage(e.a, &extra(true, 100), 2_000)
        .unwrap();
    e.store
        .set_extra_usage(e.a, &extra(true, 50), 1_000)
        .unwrap();
    assert_eq!(e.store.extra_usage(e.a).unwrap().unwrap().0.used, Some(100));
    e.store
        .set_extra_usage(e.a, &extra(false, 200), 3_000)
        .unwrap();
    let (latest, read_at) = e.store.extra_usage(e.a).unwrap().unwrap();
    assert!(!latest.enabled);
    assert_eq!(read_at, 3_000);
}

fn paid_events(store: &Store) -> usize {
    store
        .events(100)
        .unwrap()
        .iter()
        .filter(|ev| ev.kind == kind::PAID_USAGE)
        .count()
}

#[test]
fn warns_once_per_episode_of_paid_extra_usage() {
    let e = env();
    e.store
        .set_extra_usage(e.a, &extra(true, 1240), time::now_ms())
        .unwrap();
    // b still has room: no one pays yet.
    five_hour(&e.store, e.a, 100.0, 3_600);
    five_hour(&e.store, e.b, 40.0, 3_600);
    five_hour(&e.store, e.c, 100.0, 7_200);
    extra_usage::scan(&e.store, time::now_ms()).unwrap();
    assert_eq!(paid_events(&e.store), 0);

    // Every account at a limit, and a, in use, has extra usage on.
    five_hour(&e.store, e.b, 100.0, 1_800);
    extra_usage::scan(&e.store, time::now_ms()).unwrap();
    extra_usage::scan(&e.store, time::now_ms()).unwrap();
    assert_eq!(paid_events(&e.store), 1);
    let ev = e.store.last_event_of(kind::PAID_USAGE).unwrap().unwrap();
    assert_eq!(ev.account_from, Some(e.a));
    assert_eq!(ev.detail.unwrap()["used"], 1240);

    // An account free again ends the episode; the next one warns again.
    five_hour(&e.store, e.b, 10.0, 1_800);
    extra_usage::scan(&e.store, time::now_ms()).unwrap();
    five_hour(&e.store, e.b, 100.0, 1_800);
    extra_usage::scan(&e.store, time::now_ms()).unwrap();
    assert_eq!(paid_events(&e.store), 2);
}

#[test]
fn moves_off_paid_extra_usage_only_when_asked_to() {
    let e = env();
    let now = time::now_ms();
    e.store.set_extra_usage(e.a, &extra(true, 10), now).unwrap();
    five_hour(&e.store, e.a, 100.0, 3_600);
    five_hour(&e.store, e.b, 100.0, 1_800);
    five_hour(&e.store, e.c, 100.0, 900);
    let active = e.store.active_account().unwrap().unwrap();
    let (pool, policy) = swap::load_pool(&e.store, now / 1000).unwrap();
    assert!(
        extra_usage::avoid_swap(&e.store, &pool, policy, &active, now / 1000)
            .unwrap()
            .is_none()
    );

    e.store
        .set_setting("avoid_extra_usage", Some("true"))
        .unwrap();
    // No account is known to be without extra usage yet.
    assert!(
        extra_usage::avoid_swap(&e.store, &pool, policy, &active, now / 1000)
            .unwrap()
            .is_none()
    );

    e.store.set_extra_usage(e.b, &extra(false, 0), now).unwrap();
    e.store.set_extra_usage(e.c, &extra(false, 0), now).unwrap();
    let req = extra_usage::avoid_swap(&e.store, &pool, policy, &active, now / 1000)
        .unwrap()
        .unwrap();
    // c frees up first.
    assert_eq!((req.from, req.to), (Some(e.a), Some(e.c)));
    assert_eq!(req.reason, Reason::AvoidExtraUsage);

    // A paused account is left alone.
    e.store.set_state(e.c, AccountState::Paused).unwrap();
    let (pool, policy) = swap::load_pool(&e.store, now / 1000).unwrap();
    let req = extra_usage::avoid_swap(&e.store, &pool, policy, &active, now / 1000)
        .unwrap()
        .unwrap();
    assert_eq!(req.to, Some(e.b));

    // And the swap goes through, to an account that is itself at a limit.
    let out = swap::run(&e.store, &e.paths, &swap::SwapRequest { wait: true, ..req }).unwrap();
    assert!(matches!(out, swap::SwapOutcome::Swapped { .. }), "{out:?}");
    assert_eq!(e.store.active_account().unwrap().unwrap().id, e.b);
}

#[test]
fn an_account_past_its_threshold_but_not_used_up_does_not_bill() {
    let e = env();
    e.store
        .set_extra_usage(e.a, &extra(true, 10), time::now_ms())
        .unwrap();
    // Past cc-rotate's 98% threshold, with 1.5% of its plan still free.
    five_hour(&e.store, e.a, 98.5, 3_600);
    five_hour(&e.store, e.b, 100.0, 1_800);
    five_hour(&e.store, e.c, 100.0, 900);
    extra_usage::scan(&e.store, time::now_ms()).unwrap();
    assert_eq!(paid_events(&e.store), 0);
    five_hour(&e.store, e.a, 100.0, 3_600);
    extra_usage::scan(&e.store, time::now_ms()).unwrap();
    assert_eq!(paid_events(&e.store), 1);
    let detail = e
        .store
        .last_event_of(kind::PAID_USAGE)
        .unwrap()
        .unwrap()
        .detail
        .unwrap();
    assert!(detail["read_at"].is_i64());
}

#[test]
fn no_warning_while_a_swap_away_from_paying_is_due() {
    let e = env();
    let now = time::now_ms();
    e.store.set_extra_usage(e.a, &extra(true, 10), now).unwrap();
    e.store.set_extra_usage(e.b, &extra(false, 0), now).unwrap();
    five_hour(&e.store, e.a, 100.0, 3_600);
    five_hour(&e.store, e.b, 100.0, 1_800);
    five_hour(&e.store, e.c, 100.0, 900);
    e.store
        .set_setting("avoid_extra_usage", Some("true"))
        .unwrap();
    extra_usage::scan(&e.store, now).unwrap();
    assert_eq!(paid_events(&e.store), 0);
    // With nowhere to move to, it warns, and says so.
    e.store.set_state(e.b, AccountState::Paused).unwrap();
    extra_usage::scan(&e.store, now).unwrap();
    assert_eq!(paid_events(&e.store), 1);
    let detail = e
        .store
        .last_event_of(kind::PAID_USAGE)
        .unwrap()
        .unwrap()
        .detail
        .unwrap();
    assert_eq!(detail["avoiding"], true);
}

#[test]
fn a_swap_away_from_paying_that_does_not_land_is_reported() {
    let e = env();
    let now = time::now_ms();
    e.store.set_extra_usage(e.a, &extra(true, 10), now).unwrap();
    e.store.set_extra_usage(e.b, &extra(false, 0), now).unwrap();
    five_hour(&e.store, e.a, 100.0, 3_600);
    five_hour(&e.store, e.b, 100.0, 1_800);
    five_hour(&e.store, e.c, 100.0, 900);
    e.store
        .set_setting("avoid_extra_usage", Some("true"))
        .unwrap();
    extra_usage::scan(&e.store, now).unwrap();
    extra_usage::scan(&e.store, now + 60_000).unwrap();
    assert_eq!(paid_events(&e.store), 0);
    // Still paying two minutes on: the swap did not land.
    extra_usage::scan(&e.store, now + 130_000).unwrap();
    assert_eq!(paid_events(&e.store), 1);
    let detail = e
        .store
        .last_event_of(kind::PAID_USAGE)
        .unwrap()
        .unwrap()
        .detail
        .unwrap();
    assert_eq!(detail["could_move"], true);
}

#[test]
fn an_old_report_of_paid_usage_asks_for_a_fresh_reading_now_and_then() {
    let e = env();
    let now = time::now_ms();
    e.store
        .set_extra_usage(e.a, &extra(true, 10), now - 3_600_000)
        .unwrap();
    five_hour(&e.store, e.a, 100.0, 3_600);
    five_hour(&e.store, e.b, 100.0, 1_800);
    five_hour(&e.store, e.c, 100.0, 900);
    let active = e.store.active_account().unwrap().unwrap();
    let (pool, policy) = swap::load_pool(&e.store, now / 1000).unwrap();
    // Usage checks are off in these tests, and the reading runs as one.
    assert!(!extra_usage::needs_fresh_reading(&e.store, &pool, policy, &active, now).unwrap());
    e.store.set_setting("usage_checks", Some("true")).unwrap();
    assert!(extra_usage::needs_fresh_reading(&e.store, &pool, policy, &active, now).unwrap());
    assert!(
        !extra_usage::needs_fresh_reading(&e.store, &pool, policy, &active, now + 60_000).unwrap()
    );
    // A recent report needs none.
    e.store.set_extra_usage(e.a, &extra(true, 20), now).unwrap();
    assert!(
        !extra_usage::needs_fresh_reading(&e.store, &pool, policy, &active, now + 700_000).unwrap()
    );
}

#[test]
fn a_reading_of_paid_usage_takes_the_place_of_a_swap_with_nowhere_to_go() {
    let e = env();
    let now = time::now_ms();
    e.store.set_setting("usage_checks", Some("true")).unwrap();
    e.store
        .set_extra_usage(e.a, &extra(true, 10), now - 3_600_000)
        .unwrap();
    five_hour(&e.store, e.b, 100.0, 1_800);
    five_hour(&e.store, e.c, 100.0, 900);
    let resets = now / 1000 + 3_600;
    let input = tick_input("s1", 3.0, 600_000, 100.0, resets);
    let result = statusline::tick(&e.store, &e.paths, &input, now).unwrap();
    assert!(result.confirm, "{result:?}");
    assert!(result.swap.is_none(), "{result:?}");
    // Read once; the next ticks go back to asking for swaps.
    let result = statusline::tick(&e.store, &e.paths, &input, now + 40_000).unwrap();
    assert!(!result.confirm);
}
