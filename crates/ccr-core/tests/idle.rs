//! Sessions that idle across a swap (spec section 8, step 2, and F24): a session repeats the
//! numbers of its last response until its next request, so numbers it has shown unchanged since
//! before the account in use became active are the account it used before. These cases make sure
//! that rule never drops the account in use's own readings, and that each Claude Code process
//! keeps one key (`Store::session_process`).

use std::time::Duration;

use ccr_core::lock::{DirLock, LockOptions};
use ccr_core::scheduler::Reason;
use ccr_core::statusline::{self, StatusInput};
use ccr_core::store::{AccountIdentity, AccountState};
use ccr_core::swap::{self, SwapRequest};
use ccr_core::{Paths, Store, credfile, time};
use serde_json::{Value, json};

struct Env {
    _root: tempfile::TempDir,
    paths: Paths,
    store: Store,
    a: i64,
    b: i64,
}

fn oauth(tag: &str) -> Value {
    json!({
        "accessToken": format!("access-{tag}"),
        "refreshToken": format!("refresh-{tag}"),
        "expiresAt": time::now_ms() + 3_600_000,
        "scopes": ["user:inference"],
        "subscriptionType": "max",
    })
}

/// `a` active with its login in the profile, `b` parked; no usage checks, and no Claude Code to
/// identify logins with.
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
    let nowhere = paths.data_dir.join("no-claude-here");
    store
        .set_setting("claude_path", Some(&nowhere.display().to_string()))
        .unwrap();
    let ident = |u: &str| AccountIdentity {
        account_uuid: Some(u.into()),
        email: Some(format!("{u}@x.test")),
        org_uuid: Some("org-x".into()),
        ..Default::default()
    };
    let a = store
        .insert_account("a", &ident("ua"), AccountState::Active)
        .unwrap();
    let b = store
        .insert_account("b", &ident("ub"), AccountState::Parked)
        .unwrap();
    store.save_credential(a, &oauth("a1"), "login").unwrap();
    store.save_credential(b, &oauth("b1"), "login").unwrap();
    credfile::write_oauth(&paths.credentials_file(), &oauth("a1")).unwrap();
    Env {
        _root: root,
        paths,
        store,
        a,
        b,
    }
}

fn manual(to: i64) -> SwapRequest {
    SwapRequest {
        from: None,
        to: Some(to),
        reason: Reason::Manual,
        wait: true,
    }
}

/// A status line input: the session's total cost, how long its process has run, and its 5-hour
/// numbers.
fn input(session: &str, cost: f64, ran_ms: i64, pct: f64, resets_at: i64) -> StatusInput {
    StatusInput::parse(
        &json!({
            "session_id": session,
            "cost": {"total_cost_usd": cost, "total_duration_ms": ran_ms},
            "rate_limits": {"five_hour": {"used_percentage": pct, "resets_at": resets_at}}
        })
        .to_string(),
    )
}

fn cost(e: &Env, id: i64) -> f64 {
    let from = time::now_s() - 86_400;
    e.store
        .account_cost_between(id, from, from + 2 * 86_400)
        .unwrap()
        .unwrap_or(0.0)
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// Swapped away and back while a session idled: its first response on the account gets the
/// numbers it showed before, since the account was not used meanwhile. They are the account's
/// own, and so is the cost.
#[test]
fn numbers_unchanged_while_parked_are_the_accounts_own_after_a_swap_back() {
    let e = env();
    let now = time::now_ms();
    let resets = now / 1000 + 3600;
    let start = now - 700_000;
    let t0 = now - 600_000;
    statusline::tick(
        &e.store,
        &e.paths,
        &input("s", 1.0, t0 - start, 40.0, resets),
        t0,
    )
    .unwrap();
    swap::run(&e.store, &e.paths, &manual(e.b)).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    swap::run(&e.store, &e.paths, &manual(e.a)).unwrap();

    // Past the 120 s after a swap in which the left account's reset times are dropped anyway.
    let t1 = time::now_ms() + 200_000;
    statusline::tick(
        &e.store,
        &e.paths,
        &input("s", 1.5, t1 - start, 40.0, resets),
        t1,
    )
    .unwrap();
    assert_eq!(e.store.last_sample(e.a).unwrap().unwrap().ts, t1);
    assert!(close(cost(&e, e.a), 0.5), "a {}", cost(&e, e.a));
    assert!(close(cost(&e, e.b), 0.0), "b {}", cost(&e, e.b));
}

/// A status line that takes up to several seconds to start reckons the process's start that
/// much later: still the same process, and its cost is counted once.
#[test]
fn a_slow_status_line_start_keeps_the_process() {
    let e = env();
    let now = time::now_ms();
    let resets = now / 1000 + 3600;
    let start = now - 600_000;
    let mut total = 1.0;
    for i in 0..10 {
        let at = now + i * 30_000;
        let late = if i % 2 == 1 { 6_000 } else { 0 };
        statusline::tick(
            &e.store,
            &e.paths,
            &input("s", total, at - start - late, 10.0, resets),
            at,
        )
        .unwrap();
        total += 0.1;
    }
    assert!(close(cost(&e, e.a), 0.9), "{}", cost(&e, e.a));
}

/// The first tick of a process came slow, the ones after it on time: still one process, counted
/// once.
#[test]
fn a_slow_first_status_line_keeps_the_process() {
    let e = env();
    let now = time::now_ms();
    let resets = now / 1000 + 3600;
    let start = now - 600_000;
    let mut total = 1.0;
    for i in 0..10 {
        let at = now + i * 30_000;
        let late = if i == 0 { 6_000 } else { 0 };
        statusline::tick(
            &e.store,
            &e.paths,
            &input("s", total, at - start - late, 10.0, resets),
            at,
        )
        .unwrap();
        total += 0.1;
    }
    assert!(close(cost(&e, e.a), 0.9), "{}", cost(&e, e.a));
}

/// A `/login` noticed long after `.claude.json` was written (a swap ran in between): the account
/// in use is never recorded as active from before the account before it.
#[test]
fn the_time_an_account_became_active_never_moves_back() {
    let e = env();
    let config = e.paths.profile_config();
    std::fs::write(
        &config,
        json!({"oauthAccount": {"accountUuid": "ub"}}).to_string(),
    )
    .unwrap();
    statusline::tick(&e.store, &e.paths, &StatusInput::default(), time::now_ms()).unwrap();
    swap::run(&e.store, &e.paths, &manual(e.b)).unwrap();
    let swapped = swap::active_since(&e.store).unwrap();
    // Then `.claude.json` names a (a `/login`), its time an hour ago.
    std::fs::write(
        &config,
        json!({"oauthAccount": {"accountUuid": "ua"}}).to_string(),
    )
    .unwrap();
    let an_hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(&config)
        .unwrap()
        .set_modified(an_hour_ago)
        .unwrap();
    statusline::tick(&e.store, &e.paths, &StatusInput::default(), time::now_ms()).unwrap();
    assert_eq!(e.store.active_account().unwrap().unwrap().id, e.a);
    assert!(swap::active_since(&e.store).unwrap() >= swapped);
}

/// A `/login` of another pool account noticed by the tick that first carries its numbers: they
/// are that account's from then on.
#[test]
fn a_login_adopted_in_the_tick_that_first_shows_it_keeps_its_readings() {
    let e = env();
    let config = e.paths.profile_config();
    std::fs::write(
        &config,
        json!({"oauthAccount": {"accountUuid": "ua"}}).to_string(),
    )
    .unwrap();
    let now = time::now_ms();
    let resets = now / 1000 + 3600;
    let start = now - 100_000;
    statusline::tick(
        &e.store,
        &e.paths,
        &input("s", 1.0, now - start, 40.0, resets),
        now,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(20));
    credfile::write_oauth(&e.paths.credentials_file(), &oauth("b-manual")).unwrap();
    std::fs::write(
        &config,
        json!({"oauthAccount": {"accountUuid": "ub"}}).to_string(),
    )
    .unwrap();

    let t = time::now_ms();
    let b_resets = resets + 7200;
    statusline::tick(
        &e.store,
        &e.paths,
        &input("s", 1.2, t - start, 3.0, b_resets),
        t,
    )
    .unwrap();
    assert_eq!(e.store.active_account().unwrap().unwrap().id, e.b);
    assert_eq!(e.store.last_sample(e.b).unwrap().unwrap().ts, t);
    // The same numbers again later (past the minute in which a repeat is not stored again).
    let later = t + 65_000;
    statusline::tick(
        &e.store,
        &e.paths,
        &input("s", 1.3, later - start, 3.0, b_resets),
        later,
    )
    .unwrap();
    assert_eq!(e.store.last_sample(e.b).unwrap().unwrap().ts, later);
}

/// A swap interrupted after it wrote the login and settled later: the account has been in use
/// since the swap wrote it, so readings a session got on it meanwhile stay its own.
#[test]
fn a_swap_settled_later_counts_from_when_it_wrote_the_login() {
    let e = env();
    let now = time::now_ms();
    let resets = now / 1000 + 3600;
    let start = now - 100_000;
    statusline::tick(
        &e.store,
        &e.paths,
        &input("s", 1.0, now - start, 40.0, resets),
        now,
    )
    .unwrap();
    // A swap that wrote b's login and died, its lock still held for a moment.
    let lock = DirLock::acquire(&e.paths.swap_lock(), LockOptions::claude(Duration::ZERO)).unwrap();
    let b_login = e.store.credential(e.b).unwrap().unwrap().oauth;
    let pending = json!({
        "fp": credfile::oauth_fingerprint(&b_login).unwrap(),
        "from": e.a,
        "to": e.b,
        "at": now + 1,
    });
    e.store
        .set_meta(swap::META_SWAP_PENDING, pending.to_string())
        .unwrap();
    credfile::write_oauth(&e.paths.credentials_file(), &b_login).unwrap();
    // The session gets a response on b meanwhile.
    let t = time::now_ms() + 10;
    let b_resets = resets + 7200;
    statusline::tick(
        &e.store,
        &e.paths,
        &input("s", 1.2, t - start, 3.0, b_resets),
        t,
    )
    .unwrap();
    drop(lock);
    std::thread::sleep(Duration::from_millis(20));
    // Another session's tick settles the swap.
    let t2 = time::now_ms();
    statusline::tick(
        &e.store,
        &e.paths,
        &input("other", 0.0, 1000, 3.0, b_resets),
        t2,
    )
    .unwrap();
    assert_eq!(e.store.active_account().unwrap().unwrap().id, e.b);

    let t3 = time::now_ms() + 90_000;
    statusline::tick(
        &e.store,
        &e.paths,
        &input("s", 1.4, t3 - start, 3.0, b_resets),
        t3,
    )
    .unwrap();
    let last = e.store.last_sample(e.b).unwrap().unwrap();
    assert_eq!((last.session_id.as_deref(), last.ts), (Some("s"), t3));
    assert!(close(cost(&e, e.b), 0.2), "b {}", cost(&e, e.b));
}
