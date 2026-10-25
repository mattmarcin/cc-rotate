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
    assert_eq!(s.note_session_cost("s1", 0.50, e.a, hour).unwrap(), 0.50);
    assert_eq!(
        s.note_session_cost("s1", 0.75, e.a, hour + 60_000).unwrap(),
        0.25
    );
    // The same total again adds nothing; a lower one is a fresh count.
    assert_eq!(
        s.note_session_cost("s1", 0.75, e.a, hour + 120_000)
            .unwrap(),
        0.0
    );
    assert_eq!(
        s.note_session_cost("s1", 0.10, e.b, hour + 3_600_000)
            .unwrap(),
        0.10
    );
    assert_eq!(s.note_session_cost("s1", f64::NAN, e.a, hour).unwrap(), 0.0);
    assert_eq!(s.note_session_cost("s1", -1.0, e.a, hour).unwrap(), 0.0);

    let from = hour / 1000;
    let a = s
        .account_cost_between(e.a, from, from + 7_200)
        .unwrap()
        .unwrap();
    assert!((a - 0.75).abs() < 1e-9, "{a}");
    assert_eq!(
        s.account_cost_between(e.b, from, from + 3_600).unwrap(),
        None
    );
    let b = s
        .account_cost_between(e.b, from, from + 7_200)
        .unwrap()
        .unwrap();
    assert!((b - 0.10).abs() < 1e-9, "{b}");
}

fn tick_input(session: &str, cost: f64, pct: f64, resets_at: i64) -> StatusInput {
    StatusInput::parse(
        &json!({
            "session_id": session,
            "cost": {"total_cost_usd": cost},
            "rate_limits": {"five_hour": {"used_percentage": pct, "resets_at": resets_at}}
        })
        .to_string(),
    )
}

#[test]
fn the_status_line_counts_cost_for_the_account_that_served_it() {
    let e = env();
    let now = time::now_ms();
    let a_resets = now / 1000 + 3_600;
    statusline::tick(
        &e.store,
        &e.paths,
        &tick_input("s1", 1.00, 20.0, a_resets),
        now,
    )
    .unwrap();
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
    statusline::tick(
        &e.store,
        &e.paths,
        &tick_input("s1", 1.40, 22.0, a_resets),
        later,
    )
    .unwrap();
    // Then it works on b.
    let b_resets = now / 1000 + 9_000;
    statusline::tick(
        &e.store,
        &e.paths,
        &tick_input("s1", 2.00, 5.0, b_resets),
        later + 30_000,
    )
    .unwrap();

    let from = now / 1000 - 3_600;
    let to = now / 1000 + 7_200;
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
