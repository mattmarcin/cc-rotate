//! End-to-end flows over real files: swaps, statusline ticks, write-back, logins.
//! Each test gets its own data and profile directories.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use ccr_core::lock::{DirLock, LockOptions};
use ccr_core::scheduler::Reason;
use ccr_core::statusline::{self, StatusInput};
use ccr_core::store::{
    AccountIdentity, AccountState, Sample, SampleWindow, WeeklyLimitSetting, kind,
};
use ccr_core::swap::{self, SwapOutcome, SwapRequest};
use ccr_core::{Paths, Store, credfile, login, time, usage_check};
use serde_json::{Value, json};

struct Env {
    _root: tempfile::TempDir,
    paths: Paths,
}

impl Env {
    fn new() -> Env {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let profile = root.path().join("profile");
        let home = root.path().join("home");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        Env {
            // The user's other profiles are looked for in a scratch home, never the real one.
            paths: Paths::new(data, profile).with_home(home),
            _root: root,
        }
    }

    /// Usage checks are off: they would run whatever `claude` is on PATH. The tests of checks
    /// turn them on with a stand-in (`fake_claude`).
    fn store(&self) -> Store {
        let store = Store::open(&self.paths.store_db()).unwrap();
        if store.setting("usage_checks").unwrap().is_none() {
            store.set_setting("usage_checks", Some("false")).unwrap();
            // These tests are about the order and limits; spending weekly budget before it
            // resets has its own tests, which turn it on.
            store
                .set_setting("use_expiring_weekly", Some("false"))
                .unwrap();
            // And should one run anyway, it finds no Claude Code.
            let nowhere = self.paths.data_dir.join("no-claude-here");
            store
                .set_setting("claude_path", Some(&nowhere.display().to_string()))
                .unwrap();
        }
        store
    }

    fn cred_file(&self) -> PathBuf {
        self.paths.credentials_file()
    }

    fn file_doc(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.cred_file()).unwrap()).unwrap()
    }
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

/// Pool with accounts `a` (active, its login in the profile file next to an MCP login) and `b`, `c` parked.
fn pool(env: &Env) -> (Store, i64, i64, i64) {
    let store = env.store();
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
    let c = store
        .insert_account("c", &ident("uc"), AccountState::Parked)
        .unwrap();
    store.save_credential(a, &oauth("a1"), "login").unwrap();
    store.save_credential(b, &oauth("b1"), "login").unwrap();
    store.save_credential(c, &oauth("c1"), "login").unwrap();
    std::fs::write(
        env.cred_file(),
        serde_json::to_string(
            &json!({"mcpOAuth": {"srv|1": {"accessToken": "mcp"}}, "claudeAiOauth": oauth("a1")}),
        )
        .unwrap(),
    )
    .unwrap();
    (store, a, b, c)
}

fn window(name: &str, pct: f64, resets_at: i64) -> SampleWindow {
    SampleWindow {
        name: name.into(),
        pct,
        resets_at: Some(resets_at),
    }
}

fn sample(account: i64, five: f64, week: f64) -> Sample {
    Sample {
        ts: time::now_ms(),
        account_id: account,
        profile: "rotate".into(),
        session_id: None,
        model: None,
        windows: vec![
            window("five_hour", five, time::now_s() + 3600),
            window("seven_day", week, time::now_s() + 86_400 * 3),
        ],
    }
}

/// A reading of an account without a weekly limit (a Team seat): only the 5-hour window.
fn five_only(account: i64, five: f64, resets_at: i64, ts: i64) -> Sample {
    Sample {
        ts,
        account_id: account,
        profile: "rotate".into(),
        session_id: None,
        model: None,
        windows: vec![window("five_hour", five, resets_at)],
    }
}

fn manual(to: Option<i64>) -> SwapRequest {
    SwapRequest {
        from: None,
        to,
        reason: Reason::Manual,
        wait: true,
    }
}

fn refresh_of(doc: &Value) -> &str {
    doc["claudeAiOauth"]["refreshToken"].as_str().unwrap()
}

#[test]
fn swap_captures_outgoing_and_preserves_other_keys() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    // Claude Code refreshed account a since we last stored it.
    credfile::write_oauth(&env.cred_file(), &oauth("a2")).unwrap();

    let out = swap::run(&store, &env.paths, &manual(Some(b))).unwrap();
    assert_eq!(
        out,
        SwapOutcome::Swapped {
            from: Some(a),
            to: b
        }
    );

    let doc = env.file_doc();
    assert_eq!(refresh_of(&doc), "refresh-b1");
    assert_eq!(
        doc["mcpOAuth"],
        json!({"srv|1": {"accessToken": "mcp"}}),
        "R3: other keys untouched"
    );
    assert_eq!(
        store.credential(a).unwrap().unwrap().oauth["refreshToken"],
        "refresh-a2",
        "rotated token captured"
    );
    assert_eq!(
        store.account(a).unwrap().unwrap().state,
        AccountState::Parked
    );
    assert_eq!(store.active_account().unwrap().unwrap().id, b);
    assert!(
        !env.paths.refresh_lock().exists() && !env.paths.legacy_refresh_lock().exists(),
        "locks released"
    );
    assert_eq!(
        store.last_event_of(kind::SWAP).unwrap().unwrap().account_to,
        Some(b)
    );
}

#[test]
fn first_swap_of_the_day_keeps_an_encrypted_backup() {
    let env = Env::new();
    let (store, _, b, c) = pool(&env);
    let before = std::fs::read(env.cred_file()).unwrap();
    swap::run(&store, &env.paths, &manual(Some(b))).unwrap();
    swap::run(&store, &env.paths, &manual(Some(c))).unwrap();

    let backups: Vec<_> = std::fs::read_dir(env.paths.backups_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(backups.len(), 1, "one backup per day: {backups:?}");
    assert!(backups[0].extension().is_some_and(|x| x == "ccr"));
    let sealed = std::fs::read(&backups[0]).unwrap();
    assert!(
        !String::from_utf8_lossy(&sealed).contains("refresh-a1"),
        "backups are encrypted"
    );
    assert_eq!(swap::open_backup(&store, &backups[0]).unwrap(), before);
}

#[test]
fn scheduler_follows_the_order_and_skips_exhausted() {
    let env = Env::new();
    let (store, a, b, c) = pool(&env);
    // With strict_order, usage is usage: b comes first in the order even though c has more
    // room left. (Without it, both windows are running and reset together, so the most room
    // would decide.)
    store.set_setting("strict_order", Some("true")).unwrap();
    store.record_sample(&sample(a, 10.0, 40.0)).unwrap();
    store.record_sample(&sample(b, 10.0, 60.0)).unwrap();
    store.record_sample(&sample(c, 50.0, 20.0)).unwrap();
    store.set_order(&[b, c, a]).unwrap();
    let out = swap::run(&store, &env.paths, &manual(None)).unwrap();
    assert_eq!(
        out,
        SwapOutcome::Swapped {
            from: Some(a),
            to: b
        }
    );
    let event = store.last_event_of(kind::SWAP).unwrap().unwrap();
    assert_eq!(
        event.detail.unwrap()["pick"],
        "weekly limit 60% used, 1st in your order"
    );

    // b is over its 5-hour limit, so c is next.
    store.record_sample(&sample(b, 99.0, 60.0)).unwrap();
    assert_eq!(
        swap::run(&store, &env.paths, &manual(None)).unwrap(),
        SwapOutcome::Swapped {
            from: Some(b),
            to: c
        }
    );

    store.record_sample(&sample(a, 10.0, 98.5)).unwrap();
    let before = env.file_doc();
    assert_eq!(
        swap::run(&store, &env.paths, &manual(None)).unwrap(),
        SwapOutcome::PoolExhausted
    );
    assert_eq!(
        env.file_doc(),
        before,
        "R6: nothing changes when the pool is exhausted"
    );
    assert_eq!(store.active_account().unwrap().unwrap().id, c);
}

#[test]
fn stale_from_and_cooldown_skip() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let req = |from| SwapRequest {
        from: Some(from),
        to: None,
        reason: Reason::FiveHour,
        wait: false,
    };
    assert!(matches!(
        swap::run(&store, &env.paths, &req(b)).unwrap(),
        SwapOutcome::Skipped { .. }
    ));
    assert!(matches!(
        swap::run(&store, &env.paths, &req(a)).unwrap(),
        SwapOutcome::Swapped { .. }
    ));
    let now_active = store.active_account().unwrap().unwrap().id;
    let out = swap::run(&store, &env.paths, &req(now_active)).unwrap();
    assert_eq!(
        out,
        SwapOutcome::Skipped {
            why: "too soon after the last swap".into()
        },
        "R5"
    );
    let urgent = SwapRequest {
        from: Some(now_active),
        to: None,
        reason: Reason::StopFailure,
        wait: false,
    };
    assert!(
        matches!(
            swap::run(&store, &env.paths, &urgent).unwrap(),
            SwapOutcome::Swapped { .. }
        ),
        "urgent ignores cooldown"
    );
}

#[test]
fn concurrent_triggers_swap_once() {
    let env = Env::new();
    let (_, a, _, _) = pool(&env);
    let threads = 10;
    let barrier = Arc::new(Barrier::new(threads));
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let paths = env.paths.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = Store::open(&paths.store_db()).unwrap();
                barrier.wait();
                let req = SwapRequest {
                    from: Some(a),
                    to: None,
                    reason: Reason::FiveHour,
                    wait: false,
                };
                swap::run(&store, &paths, &req).unwrap()
            })
        })
        .collect();
    let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let swapped = outcomes
        .iter()
        .filter(|o| matches!(o, SwapOutcome::Swapped { .. }))
        .count();
    assert_eq!(swapped, 1, "R4: exactly one swap, got {outcomes:?}");
    let store = env.store();
    let swaps = store
        .events(100)
        .unwrap()
        .iter()
        .filter(|e| e.kind == kind::SWAP)
        .count();
    assert_eq!(swaps, 1);
}

#[test]
fn swap_waits_for_claude_refresh_lock_and_captures_its_result() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);

    // Simulate Claude Code mid-refresh: it holds the lock, then writes the rotated token.
    let refresh_lock = env.paths.refresh_lock();
    let cred = env.cred_file();
    let holder = DirLock::acquire(&refresh_lock, LockOptions::claude(Duration::ZERO)).unwrap();
    let refresher = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(800));
        credfile::write_oauth(&cred, &oauth("a-rotated")).unwrap();
        drop(holder);
    });

    let started = std::time::Instant::now();
    let out = swap::run(&store, &env.paths, &manual(Some(b))).unwrap();
    refresher.join().unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(700),
        "swap waited for the refresh"
    );
    assert_eq!(
        out,
        SwapOutcome::Swapped {
            from: Some(a),
            to: b
        }
    );
    assert_eq!(
        store.credential(a).unwrap().unwrap().oauth["refreshToken"],
        "refresh-a-rotated",
        "R2: the refresh that finished under the lock is what got stored"
    );
    assert_eq!(refresh_of(&env.file_doc()), "refresh-b1");
}

/// Claude Code holds `.storage-write.lock` around every read-modify-write of the credentials
/// (an MCP login, for example). A swap in the middle would be overwritten with the old login.
#[test]
fn swap_waits_for_claude_credentials_write_lock() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let holder = DirLock::acquire(
        &env.paths.storage_lock(),
        LockOptions::claude_storage(Duration::ZERO),
    )
    .unwrap();
    let cred = env.cred_file();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        let mut doc: Value =
            serde_json::from_str(&std::fs::read_to_string(&cred).unwrap()).unwrap();
        doc["mcpOAuth"]["srv|2"] = json!({"accessToken": "mcp2"});
        std::fs::write(&cred, doc.to_string()).unwrap();
        drop(holder);
    });

    let started = std::time::Instant::now();
    let out = swap::run(&store, &env.paths, &manual(Some(b))).unwrap();
    writer.join().unwrap();
    assert!(started.elapsed() >= Duration::from_millis(500));
    assert_eq!(
        out,
        SwapOutcome::Swapped {
            from: Some(a),
            to: b
        }
    );
    let doc = env.file_doc();
    assert_eq!(refresh_of(&doc), "refresh-b1");
    assert_eq!(doc["mcpOAuth"]["srv|2"]["accessToken"], "mcp2");
    assert!(!env.paths.storage_lock().exists(), "lock released");
}

#[test]
fn tick_records_usage_and_requests_swap_at_threshold() {
    let env = Env::new();
    let (store, a, b, c) = pool(&env);
    // Without readings, b and c would rank above a (an unknown weekly limit ranks above a
    // known one) and the tick would ask to switch to them; say they have one too.
    store.set_weekly_limit(b, WeeklyLimitSetting::Yes).unwrap();
    store.set_weekly_limit(c, WeeklyLimitSetting::Yes).unwrap();
    let input = |pct: f64| {
        StatusInput::parse(&format!(
            r#"{{"session_id":"s","rate_limits":{{"five_hour":{{"used_percentage":{pct},"resets_at":{}}},"seven_day":{{"used_percentage":30,"resets_at":{}}}}}}}"#,
            time::now_s() + 3600,
            time::now_s() + 86_400
        ))
    };
    let r = statusline::tick(&store, &env.paths, &input(50.0), time::now_ms()).unwrap();
    assert!(r.swap.is_none());
    assert_eq!(
        store
            .last_sample(a)
            .unwrap()
            .unwrap()
            .window("five_hour")
            .unwrap()
            .pct,
        50.0
    );

    let r = statusline::tick(&store, &env.paths, &input(98.0), time::now_ms()).unwrap();
    let req = r.swap.expect("swap requested at 98%");
    assert_eq!((req.from, req.reason), (Some(a), Reason::FiveHour));

    let r = statusline::tick(&store, &env.paths, &input(99.0), time::now_ms()).unwrap();
    assert!(
        r.swap.is_none(),
        "requests are throttled while one is pending"
    );
}

#[test]
fn tick_writes_back_refreshed_login() {
    let env = Env::new();
    let (store, a, _, _) = pool(&env);
    statusline::tick(&store, &env.paths, &StatusInput::default(), time::now_ms()).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    credfile::write_oauth(&env.cred_file(), &oauth("a9")).unwrap();
    statusline::tick(&store, &env.paths, &StatusInput::default(), time::now_ms()).unwrap();
    assert_eq!(
        store.credential(a).unwrap().unwrap().oauth["refreshToken"],
        "refresh-a9"
    );
    assert_eq!(
        store
            .last_event_of(kind::SYNC)
            .unwrap()
            .unwrap()
            .account_from,
        Some(a)
    );
}

#[test]
fn dead_login_triggers_swap_and_marks_needs_login() {
    let env = Env::new();
    let (store, a, _, _) = pool(&env);
    statusline::tick(&store, &env.paths, &StatusInput::default(), time::now_ms()).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    credfile::write_oauth(
        &env.cred_file(),
        &json!({"accessToken": "", "refreshToken": "", "expiresAt": 0}),
    )
    .unwrap();

    let r = statusline::tick(&store, &env.paths, &StatusInput::default(), time::now_ms()).unwrap();
    let req = r.swap.expect("dead login requests a swap");
    assert_eq!(req.reason, Reason::DeadLogin);
    swap::run(&store, &env.paths, &req).unwrap();
    assert_eq!(
        store.account(a).unwrap().unwrap().state,
        AccountState::NeedsLogin
    );
    assert_eq!(
        store.credential(a).unwrap().unwrap().oauth["refreshToken"],
        "refresh-a1",
        "dead tokens are never stored"
    );
    assert_ne!(store.active_account().unwrap().unwrap().id, a);
}

#[test]
fn readings_of_the_old_account_after_a_swap_are_ignored() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let old = sample(a, 98.0, 40.0);
    store.record_sample(&old).unwrap();
    swap::run(&store, &env.paths, &manual(Some(b))).unwrap();

    let stale = format!(
        r#"{{"rate_limits":{{"five_hour":{{"used_percentage":98,"resets_at":{}}},"seven_day":{{"used_percentage":40,"resets_at":{}}}}}}}"#,
        old.window("five_hour").unwrap().resets_at.unwrap(),
        old.window("seven_day").unwrap().resets_at.unwrap()
    );
    let r = statusline::tick(
        &store,
        &env.paths,
        &StatusInput::parse(&stale),
        time::now_ms(),
    )
    .unwrap();
    assert!(
        store.last_sample(b).unwrap().is_none(),
        "F5 guard: old account's reading not attributed to b"
    );
    assert!(r.swap.is_none());
}

fn write_staging(dir: &Path, tag: &str, uuid: &str, email: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(".credentials.json"),
        json!({"claudeAiOauth": oauth(tag)}).to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join(".claude.json"),
        json!({"oauthAccount": {"accountUuid": uuid, "emailAddress": email, "organizationUuid": "org", "organizationName": "Org"}}).to_string(),
    )
    .unwrap();
}

#[test]
fn first_login_becomes_active_and_relogin_checks_identity() {
    let env = Env::new();
    let store = env.store();
    let staging = env.paths.data_dir.join("st1");
    write_staging(&staging, "first", "u-jane", "jane@x.test");

    let imported = login::import(
        &store,
        &env.paths,
        &staging,
        &login::LoginRequest::default(),
    )
    .unwrap();
    assert!(imported.created && imported.activated);
    assert_eq!(imported.account.label, "jane");
    assert_eq!(refresh_of(&env.file_doc()), "refresh-first");

    let second = env.paths.data_dir.join("st2");
    write_staging(&second, "second", "u-sam", "sam@x.test");
    let sam = login::import(&store, &env.paths, &second, &login::LoginRequest::default()).unwrap();
    assert!(sam.created && !sam.activated);
    assert_eq!(sam.account.state, AccountState::Parked);

    let wrong = env.paths.data_dir.join("st3");
    write_staging(&wrong, "third", "u-sam", "sam@x.test");
    let err = login::import(
        &store,
        &env.paths,
        &wrong,
        &login::LoginRequest {
            relogin: Some(imported.account.id),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("Signed in as sam@x.test"),
        "R10: {err}"
    );
    assert_eq!(
        store
            .credential(imported.account.id)
            .unwrap()
            .unwrap()
            .oauth["refreshToken"],
        "refresh-first"
    );

    // Adding an account found in another profile: a different browser account is refused.
    let found = env.paths.data_dir.join("st5");
    write_staging(&found, "fifth", "u-ann", "ann@x.test");
    let err = login::import(
        &store,
        &env.paths,
        &found,
        &login::LoginRequest {
            label: Some("work".into()),
            expect_email: Some("work@x.test".into()),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("not work@x.test"), "{err}");
    assert!(store.account_by_label("work").unwrap().is_none());

    let again = env.paths.data_dir.join("st4");
    write_staging(&again, "first-2", "u-jane", "jane@x.test");
    let re = login::import(
        &store,
        &env.paths,
        &again,
        &login::LoginRequest {
            relogin: Some(imported.account.id),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!re.created);
    assert_eq!(
        refresh_of(&env.file_doc()),
        "refresh-first-2",
        "re-login of the active account updates the profile"
    );
}

#[test]
fn manual_login_inside_profile_is_adopted() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let config = env.paths.profile_config();
    std::fs::write(
        &config,
        json!({"oauthAccount": {"accountUuid": "ua"}}).to_string(),
    )
    .unwrap();
    statusline::tick(&store, &env.paths, &StatusInput::default(), time::now_ms()).unwrap();

    // The user ran /login as account b inside the rotation profile.
    std::thread::sleep(Duration::from_millis(20));
    credfile::write_oauth(&env.cred_file(), &oauth("b-manual")).unwrap();
    std::fs::write(
        &config,
        json!({"oauthAccount": {"accountUuid": "ub"}}).to_string(),
    )
    .unwrap();
    statusline::tick(&store, &env.paths, &StatusInput::default(), time::now_ms()).unwrap();

    assert_eq!(store.active_account().unwrap().unwrap().id, b);
    assert_eq!(
        store.credential(b).unwrap().unwrap().oauth["refreshToken"],
        "refresh-b-manual"
    );
    assert_eq!(
        store.credential(a).unwrap().unwrap().oauth["refreshToken"],
        "refresh-a1",
        "a's login not overwritten"
    );
}

/// Two Team seats ordered first and a Max account last: once the first seat's 5-hour window
/// has reset and the second has been in use for the dwell time, a tick asks to switch back,
/// and the swap records why.
#[test]
fn tick_switches_back_to_a_preferred_account() {
    let env = Env::new();
    let (store, a, b, c) = pool(&env);
    // c (Max, weekly limit) is active; a and b are seats without a weekly limit, a first.
    swap::run(&store, &env.paths, &manual(Some(c))).unwrap();
    store.set_order(&[a, b, c]).unwrap();
    let now_s = time::now_s();
    for (account, offset) in [(a, 0), (b, 0)] {
        // Two distinct 5-hour windows in the last week and never a weekly one: no limit.
        store
            .record_sample(&five_only(
                account,
                30.0,
                now_s - 20 * 3600,
                (now_s - 30 * 3600) * 1000 + offset,
            ))
            .unwrap();
        store
            .record_sample(&five_only(
                account,
                5.0,
                now_s + 3 * 3600,
                (now_s - 3600) * 1000,
            ))
            .unwrap();
    }
    store.record_sample(&sample(c, 10.0, 20.0)).unwrap();
    store
        .set_setting("switch_back_dwell_s", Some("600"))
        .unwrap();

    let input = format!(
        r#"{{"model":{{"id":"claude-opus-4-1"}},"rate_limits":{{"five_hour":{{"used_percentage":12,"resets_at":{}}},"seven_day":{{"used_percentage":21,"resets_at":{}}}}}}}"#,
        now_s + 3600,
        now_s + 86_400 * 3
    );
    let tick = |at_ms: i64| {
        statusline::tick(&store, &env.paths, &StatusInput::parse(&input), at_ms).unwrap()
    };

    let r = tick(time::now_ms());
    assert!(r.swap.is_none(), "not before the dwell time");

    let later = time::now_ms() + 601_000;
    let req = tick(later).swap.expect("a better ranked account is ready");
    assert_eq!(
        (req.from, req.reason),
        (Some(c), Reason::PreferredAvailable)
    );
    assert!(tick(later + 1000).swap.is_none(), "one request at a time");

    // The swap itself re-checks the dwell against the real clock, so pretend c has been
    // active for a while.
    store
        .set_meta(swap::META_ACTIVE_SINCE, time::now_ms() - 700_000)
        .unwrap();
    store
        .set_meta(swap::META_LAST_SWAP_AT, time::now_ms() - 700_000)
        .unwrap();
    let out = swap::run(&store, &env.paths, &req).unwrap();
    assert_eq!(
        out,
        SwapOutcome::Swapped {
            from: Some(c),
            to: a
        }
    );
    let event = store.last_event_of(kind::SWAP).unwrap().unwrap();
    assert_eq!(event.reason.as_deref(), Some("preferred_available"));
    let detail = event.detail.unwrap();
    // a and b are seats whose windows run and reset together; a wins on its label.
    let pick = detail["pick"].as_str().unwrap();
    assert!(
        pick.starts_with("no weekly limit, 5-hour window already running, resets in ")
            && pick.ends_with(", tied, 93% room left"),
        "{detail}"
    );
    assert!(
        detail["why"]
            .as_str()
            .unwrap()
            .starts_with("a ranks above c"),
        "{detail}"
    );
    assert_eq!(
        store.meta(swap::META_PROFILE_MODEL).unwrap().as_deref(),
        Some("claude-opus-4-1")
    );

    // a and b rank equally apart from the order, and b is never preferred over a.
    let r = statusline::tick(
        &store,
        &env.paths,
        &StatusInput::default(),
        time::now_ms() + 3_600_000,
    )
    .unwrap();
    assert!(r.swap.is_none());
}

/// A switch back that is no longer warranted when the detached swap runs is skipped, not
/// turned into another kind of swap.
#[test]
fn stale_switch_back_requests_are_skipped() {
    let env = Env::new();
    let (store, a, _, _) = pool(&env);
    store
        .set_meta(swap::META_ACTIVE_SINCE, time::now_ms() - 3_600_000)
        .unwrap();
    let req = SwapRequest {
        from: Some(a),
        to: None,
        reason: Reason::PreferredAvailable,
        wait: false,
    };
    // a is first in the order and nothing ranks above it.
    assert_eq!(
        swap::run(&store, &env.paths, &req).unwrap(),
        SwapOutcome::Skipped {
            why: "no better ranked account is ready".into()
        }
    );
    store
        .set_meta(swap::META_ACTIVE_SINCE, time::now_ms())
        .unwrap();
    assert!(matches!(
        swap::run(&store, &env.paths, &req).unwrap(),
        SwapOutcome::Skipped { .. }
    ));
    assert_eq!(store.active_account().unwrap().unwrap().id, a);
}

/// The app's background sync applies the same switch-back rule without a live reading.
#[test]
fn background_sync_switches_back_too() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    swap::run(&store, &env.paths, &manual(Some(b))).unwrap();
    store
        .set_meta(swap::META_ACTIVE_SINCE, time::now_ms() - 3_600_000)
        .unwrap();
    store
        .set_meta(swap::META_LAST_SWAP_AT, time::now_ms() - 3_600_000)
        .unwrap();
    // a and b are in the same tier, so only strict_order goes back up the order.
    assert!(statusline::sync(&store, &env.paths).unwrap().is_none());
    store.set_setting("strict_order", Some("true")).unwrap();
    let req = statusline::sync(&store, &env.paths)
        .unwrap()
        .expect("a ranks above b");
    assert_eq!(
        (req.from, req.reason),
        (Some(b), Reason::PreferredAvailable)
    );
    assert_eq!(
        swap::run(&store, &env.paths, &req).unwrap(),
        SwapOutcome::Swapped {
            from: Some(b),
            to: a
        }
    );
    assert!(statusline::sync(&store, &env.paths).unwrap().is_none());
}

/// A status line reading of only the 5-hour window.
fn five_hour_input(pct: f64, resets_at: i64) -> StatusInput {
    StatusInput::parse(&format!(
        r#"{{"session_id":"s","rate_limits":{{"five_hour":{{"used_percentage":{pct},"resets_at":{resets_at}}}}}}}"#
    ))
}

/// The account swapped to turns out to be used up already (another profile used it since its
/// last reading). It is left on the next reading instead of after the swap cooldown, and only
/// while it has not had a reading below its threshold since it became active.
#[test]
fn an_account_found_used_up_after_a_swap_is_left_at_once() {
    let env = Env::new();
    let (store, a, b, c) = pool(&env);
    let threshold = |from| SwapRequest {
        from: Some(from),
        to: None,
        reason: Reason::FiveHour,
        wait: false,
    };
    store
        .record_sample(&five_only(a, 99.0, time::now_s() + 1800, time::now_ms()))
        .unwrap();
    assert!(matches!(
        swap::run(&store, &env.paths, &threshold(a)).unwrap(),
        SwapOutcome::Swapped { to, .. } if to == b
    ));

    let now = time::now_ms();
    let tick = statusline::tick(
        &store,
        &env.paths,
        &five_hour_input(100.0, time::now_s() + 7200),
        now,
    )
    .unwrap();
    let req = tick
        .swap
        .expect("a used-up account is left within the cooldown");
    assert_eq!(req.reason, Reason::FiveHour);
    assert_eq!(
        swap::run(&store, &env.paths, &req).unwrap(),
        SwapOutcome::Swapped {
            from: Some(b),
            to: c
        }
    );
    let event = store.last_event_of(kind::SWAP).unwrap().unwrap();
    assert_eq!(event.detail.unwrap()["arrived_over_limit"], true);

    // c is fine at first and only then reaches its threshold: that is ordinary use, and the
    // cooldown applies.
    let resets = time::now_s() + 9000;
    statusline::tick(
        &store,
        &env.paths,
        &five_hour_input(40.0, resets),
        now + 1000,
    )
    .unwrap();
    let tick = statusline::tick(
        &store,
        &env.paths,
        &five_hour_input(99.0, resets),
        now + 40_000,
    )
    .unwrap();
    assert!(tick.swap.is_none(), "within the cooldown");
    assert_eq!(
        swap::run(&store, &env.paths, &threshold(c)).unwrap(),
        SwapOutcome::Skipped {
            why: "too soon after the last swap".into()
        }
    );
}

/// An agent used account b up in b's own profile, so b's last reading in the pool is out of date.
/// Claude Code's cached `/usage` reading in that profile says so, and the swap goes to c.
#[test]
fn a_swap_skips_an_account_its_own_profile_used_up() {
    let env = Env::new();
    let (store, _, b, c) = pool(&env);
    store
        .record_sample(&five_only(
            b,
            10.0,
            time::now_s() + 3600,
            time::now_ms() - 3_600_000,
        ))
        .unwrap();
    let profile = env.paths.home.join(".claude-b");
    std::fs::create_dir_all(&profile).unwrap();
    let fetched_at = time::now_ms() - 60_000;
    std::fs::write(
        profile.join(".claude.json"),
        json!({
            "oauthAccount": {"accountUuid": "ub", "emailAddress": "ub@x.test", "organizationUuid": "org-x"},
            "cachedUsageUtilization": {
                "fetchedAtMs": fetched_at,
                "accountUuid": "ub",
                "utilization": {"limits": [{
                    "kind": "session", "percent": 100,
                    "resets_at": "2099-01-01T00:00:00Z"
                }]}
            }
        })
        .to_string(),
    )
    .unwrap();

    assert_eq!(
        swap::run(&store, &env.paths, &manual(None)).unwrap(),
        SwapOutcome::Swapped {
            from: Some(store.account_by_label("a").unwrap().unwrap().id),
            to: c
        }
    );
    let reading = store.last_sample(b).unwrap().unwrap();
    assert_eq!(reading.profile, ".claude-b");
    assert_eq!(reading.ts, fetched_at);
    assert_eq!(reading.windows[0].pct, 100.0);
}

/// Two swaps in quick succession (a used up, then b found used up on arrival) while another
/// session has been idle since it last used a: its status line keeps reporting a's usage. That
/// reading is neither recorded for c nor taken as proof that c arrived used up.
#[test]
fn an_idle_session_does_not_push_the_rotation_through_the_pool() {
    let env = Env::new();
    let (store, a, b, c) = pool(&env);
    let a_resets = time::now_s() + 1800;
    store
        .record_sample(&five_only(a, 99.0, a_resets, time::now_ms()))
        .unwrap();
    let from = |id| SwapRequest {
        from: Some(id),
        to: None,
        reason: Reason::FiveHour,
        wait: false,
    };
    swap::run(&store, &env.paths, &from(a)).unwrap();
    let now = time::now_ms();
    let req = statusline::tick(
        &store,
        &env.paths,
        &five_hour_input(100.0, time::now_s() + 7200),
        now,
    )
    .unwrap()
    .swap
    .unwrap();
    assert_eq!(
        swap::run(&store, &env.paths, &req).unwrap(),
        SwapOutcome::Swapped {
            from: Some(b),
            to: c
        }
    );

    // The idle session, 40 s later: a's reading, still at 99%.
    let tick = statusline::tick(
        &store,
        &env.paths,
        &five_hour_input(99.0, a_resets),
        now + 40_000,
    )
    .unwrap();
    assert!(tick.swap.is_none(), "{:?}", tick.swap);
    assert!(
        store.last_sample(c).unwrap().is_none(),
        "a's reading is not recorded for c"
    );

    // Past the stale-reading guard (a longer cooldown), the same reading is recorded for c, but
    // it shares a's reset time, so it does not prove c arrived used up: the cooldown applies.
    store.set_setting("swap_cooldown_s", Some("600")).unwrap();
    let tick = statusline::tick(
        &store,
        &env.paths,
        &five_hour_input(99.0, a_resets),
        now + 200_000,
    )
    .unwrap();
    assert!(tick.swap.is_none(), "{:?}", tick.swap);
    assert!(!swap::arrived_over_limit(&store, c, now + 200_000).unwrap());
}

/// Observe mode in the default profile, which keeps its `.claude.json` in the home directory.
#[test]
fn observe_mode_works_in_the_default_profile() {
    let env = Env::new();
    let (store, _, b, _) = pool(&env);
    let default = env.paths.home.join(".claude");
    std::fs::create_dir_all(&default).unwrap();
    let signed_in = |org: &str| {
        std::fs::write(
            env.paths.home.join(".claude.json"),
            json!({"oauthAccount": {"accountUuid": "ub", "organizationUuid": org}}).to_string(),
        )
        .unwrap();
    };
    let observe = |pct: f64| {
        statusline::observe(
            &store,
            &default,
            &env.paths.home,
            &five_hour_input(pct, time::now_s() + 3600),
            time::now_ms(),
        )
        .unwrap()
    };
    signed_in("org-x");
    assert_eq!(observe(55.0).map(|a| a.id), Some(b));
    assert_eq!(store.last_sample(b).unwrap().unwrap().windows[0].pct, 55.0);

    // The same person in another organization (a personal plan next to the Team seat).
    signed_in("personal-org");
    assert!(observe(90.0).is_none());
    assert_eq!(store.last_sample(b).unwrap().unwrap().windows[0].pct, 55.0);
}

/// Record an account's latest reading as known to be its own, as a usage check does.
fn known_as_own(store: &Store, account: i64) {
    let last = store.last_sample(account).unwrap().unwrap();
    swap::note_known_resets(store, account, &last.windows, time::now_s()).unwrap();
}

/// A status line reading from session `session`, whose API time so far is `api_ms`.
fn session_input(session: &str, api_ms: u64, pct: f64, resets_at: i64) -> StatusInput {
    StatusInput::parse(&format!(
        r#"{{"session_id":"{session}","cost":{{"total_api_duration_ms":{api_ms}}},"rate_limits":{{"five_hour":{{"used_percentage":{pct},"resets_at":{resets_at}}}}}}}"#
    ))
}

/// With the default settings (guard and cooldown both 120 s): a session that has been idle
/// since before a swap keeps reporting the old account's usage for as long as it stays idle.
/// With the new account's own reset time known (a check before the swap usually just read it),
/// that reading is never recorded for the new account, so it cannot walk the rotation through
/// the pool; once the session makes a request, its readings count again.
#[test]
fn a_session_idle_since_before_a_swap_is_ignored_however_long_it_idles() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let now = time::now_ms();
    let a_resets = time::now_s() + 3 * 86_400;
    let b_resets = time::now_s() + 4 * 3600;
    store
        .record_sample(&five_only(b, 10.0, b_resets, now - 60_000))
        .unwrap();
    // What a usage check of b before the swap establishes: b's own reset times.
    known_as_own(&store, b);
    // The idle session's last reading on a, before the swap.
    statusline::tick(
        &store,
        &env.paths,
        &session_input("idle", 5000, 99.0, a_resets),
        now,
    )
    .unwrap();
    let swapped = swap::run(
        &store,
        &env.paths,
        &SwapRequest {
            from: Some(a),
            to: None,
            reason: Reason::FiveHour,
            wait: false,
        },
    )
    .unwrap();
    assert_eq!(
        swapped,
        SwapOutcome::Swapped {
            from: Some(a),
            to: b
        }
    );

    for later in [125_000, 600_000, 3_600_000] {
        let tick = statusline::tick(
            &store,
            &env.paths,
            &session_input("idle", 5000, 99.0, a_resets),
            now + later,
        )
        .unwrap();
        assert!(tick.swap.is_none(), "after {later} ms: {:?}", tick.swap);
    }
    assert_eq!(
        store.last_sample(b).unwrap().unwrap().windows[0].pct,
        10.0,
        "a's usage is never recorded as b's"
    );

    // The session makes a request on b: its reading is b's.
    statusline::tick(
        &store,
        &env.paths,
        &session_input("idle", 7000, 12.0, b_resets),
        now + 3_700_000,
    )
    .unwrap();
    assert_eq!(store.last_sample(b).unwrap().unwrap().windows[0].pct, 12.0);
}

/// A session that ran on the old account and whose first request on the new one is refused at
/// the limit: its reading changed (the new account's reset time), so it counts, even though a
/// refused request may add no API time.
#[test]
fn a_changed_reading_counts_even_without_new_api_time() {
    let env = Env::new();
    let (store, a, b, c) = pool(&env);
    let now = time::now_ms();
    let a_resets = time::now_s() + 1800;
    store
        .record_sample(&five_only(a, 99.0, a_resets, now))
        .unwrap();
    statusline::tick(
        &store,
        &env.paths,
        &session_input("busy", 5000, 99.0, a_resets),
        now,
    )
    .unwrap();
    swap::run(
        &store,
        &env.paths,
        &SwapRequest {
            from: Some(a),
            to: None,
            reason: Reason::FiveHour,
            wait: false,
        },
    )
    .unwrap();
    let req = statusline::tick(
        &store,
        &env.paths,
        &session_input("busy", 5000, 100.0, time::now_s() + 7200),
        now + 31_000,
    )
    .unwrap()
    .swap
    .expect("b arrived used up");
    assert_eq!(
        swap::run(&store, &env.paths, &req).unwrap(),
        SwapOutcome::Swapped {
            from: Some(b),
            to: c
        }
    );
}

/// A threshold swap while a turn is still streaming on the old account: the turn finishes after
/// the swap with the old account's latest usage, and the session then idles. Neither reading is
/// recorded for the new account, however long after the swap it comes.
#[test]
fn a_request_that_finishes_after_a_swap_still_counts_for_the_old_account() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let now = time::now_ms();
    let a_resets = time::now_s() + 3 * 86_400;
    store
        .record_sample(&five_only(b, 10.0, time::now_s() + 4 * 3600, now - 60_000))
        .unwrap();
    // What a usage check of b before the swap establishes: b's own reset times.
    known_as_own(&store, b);
    statusline::tick(
        &store,
        &env.paths,
        &session_input("agent", 5000, 98.5, a_resets),
        now,
    )
    .unwrap();
    swap::run(
        &store,
        &env.paths,
        &SwapRequest {
            from: Some(a),
            to: None,
            reason: Reason::FiveHour,
            wait: false,
        },
    )
    .unwrap();
    // "agent" ran on a before; "new" is seen for the first time, its first turn having started
    // on a before the swap.
    for (session, later, api, pct) in [
        ("agent", 20_000, 65_000, 99.0),
        ("new", 25_000, 3_000, 98.9),
        ("agent", 150_000, 65_000, 99.0),
        ("new", 160_000, 3_000, 98.9),
        ("agent", 900_000, 90_000, 99.4),
    ] {
        let tick = statusline::tick(
            &store,
            &env.paths,
            &session_input(session, api, pct, a_resets),
            now + later,
        )
        .unwrap();
        assert!(
            tick.swap.is_none(),
            "{session} after {later} ms: {:?}",
            tick.swap
        );
    }
    assert_eq!(store.last_sample(b).unwrap().unwrap().windows[0].pct, 10.0);
}

/// Windows reset at round times, so the new account's window can reset at the same moment as
/// the old one's. Then a reading cannot be told apart, and it counts: hiding a genuine reading of
/// a used-up account would leave its sessions waiting at the limit.
#[test]
fn a_reading_sharing_the_new_accounts_own_reset_time_counts() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let now = time::now_ms();
    let shared = time::now_s() + 2 * 3600;
    store
        .record_sample(&five_only(b, 20.0, shared, now - 60_000))
        .unwrap();
    // What a usage check of b before the swap establishes: b's own reset times.
    known_as_own(&store, b);
    statusline::tick(
        &store,
        &env.paths,
        &session_input("s", 5000, 99.0, shared),
        now,
    )
    .unwrap();
    swap::run(
        &store,
        &env.paths,
        &SwapRequest {
            from: Some(a),
            to: Some(b),
            reason: Reason::Manual,
            wait: true,
        },
    )
    .unwrap();
    statusline::tick(
        &store,
        &env.paths,
        &session_input("s", 6000, 100.0, shared),
        now + 130_000,
    )
    .unwrap();
    assert_eq!(store.last_sample(b).unwrap().unwrap().windows[0].pct, 100.0);
}

/// Nothing current is known of the new account (its last reading is from a window that has
/// reset) and its window resets at the same moment as the old account's. A reading at its limit
/// may be either account's; it counts, so sessions are not left waiting at the limit, but it does
/// not vouch for the account's reset times.
#[test]
fn with_nothing_known_of_the_new_account_an_ambiguous_reading_counts() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let now = time::now_ms();
    let shared = time::now_s() + 2 * 3600;
    store
        .record_sample(&five_only(
            b,
            20.0,
            time::now_s() - 86_400,
            now - 86_400_000,
        ))
        .unwrap();
    store
        .record_sample(&five_only(a, 98.5, shared, now))
        .unwrap();
    swap::run(
        &store,
        &env.paths,
        &SwapRequest {
            from: Some(a),
            to: Some(b),
            reason: Reason::Manual,
            wait: true,
        },
    )
    .unwrap();
    let tick = statusline::tick(
        &store,
        &env.paths,
        &session_input("s", 6000, 100.0, shared),
        now + 130_000,
    )
    .unwrap();
    assert_eq!(store.last_sample(b).unwrap().unwrap().windows[0].pct, 100.0);
    assert!(tick.swap.is_some(), "b at its limit is left");
    assert!(
        swap::known_resets(&store, b, time::now_s())
            .unwrap()
            .is_empty(),
        "an ambiguous reading does not vouch for b's reset times"
    );
}

/// The old and the new account's windows reset at the same moment, and the new account's reset
/// time is known to be its own. A session still showing the old account repeats that account's
/// latest reading exactly, so it is told apart by its values; the new account's own readings
/// count.
#[test]
fn with_aligned_reset_times_a_copy_of_the_old_accounts_reading_is_not_counted() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let now = time::now_ms();
    let shared = time::now_s() + 2 * 3600;
    store
        .record_sample(&five_only(b, 40.0, shared, now - 60_000))
        .unwrap();
    known_as_own(&store, b);
    statusline::tick(
        &store,
        &env.paths,
        &session_input("idle", 5000, 99.0, shared),
        now,
    )
    .unwrap();
    swap::run(
        &store,
        &env.paths,
        &SwapRequest {
            from: Some(a),
            to: None,
            reason: Reason::FiveHour,
            wait: false,
        },
    )
    .unwrap();
    let tick = statusline::tick(
        &store,
        &env.paths,
        &session_input("idle", 5000, 99.0, shared),
        now + 150_000,
    )
    .unwrap();
    assert!(tick.swap.is_none(), "{:?}", tick.swap);
    assert_eq!(store.last_sample(b).unwrap().unwrap().windows[0].pct, 40.0);

    statusline::tick(
        &store,
        &env.paths,
        &session_input("busy", 9000, 41.0, shared),
        now + 160_000,
    )
    .unwrap();
    assert_eq!(store.last_sample(b).unwrap().unwrap().windows[0].pct, 41.0);
}

/// A swap writes the target's login into the profile before it commits. A status line tick in
/// between still sees the outgoing account as active, and must not store the target's login as
/// the outgoing account's.
#[test]
fn a_tick_during_a_swap_does_not_store_the_targets_login_as_the_outgoing_one() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let target = store.credential(b).unwrap().unwrap();
    // The swap is running: it holds the swap lock.
    let swapping =
        DirLock::acquire(&env.paths.swap_lock(), LockOptions::claude(Duration::ZERO)).unwrap();
    mark_pending(&store, &target.refresh_fp, Some(a), b);
    credfile::write_oauth(&env.cred_file(), &target.oauth).unwrap();

    statusline::sync(&store, &env.paths).unwrap();
    assert_eq!(
        store.credential(a).unwrap().unwrap().oauth["refreshToken"],
        "refresh-a1",
        "a keeps its own login"
    );

    // The swap commits (b active, the mark gone, in one transaction); the login in the file is
    // b's, and a's stays its own.
    store
        .immediate(|s| {
            s.set_state(a, AccountState::Parked)?;
            s.set_state(b, AccountState::Active)?;
            s.delete_meta(swap::META_SWAP_PENDING)
        })
        .unwrap();
    drop(swapping);
    statusline::sync(&store, &env.paths).unwrap();
    assert_eq!(
        store.credential(a).unwrap().unwrap().oauth["refreshToken"],
        "refresh-a1"
    );
    assert_eq!(
        store.credential(b).unwrap().unwrap().oauth["refreshToken"],
        "refresh-b1"
    );
}

fn mark_pending(store: &Store, fp: &str, from: Option<i64>, to: i64) {
    let pending = swap::PendingSwap {
        fp: fp.to_string(),
        from,
        to,
        at: time::now_ms(),
    };
    store
        .set_meta(
            swap::META_SWAP_PENDING,
            serde_json::to_string(&pending).unwrap(),
        )
        .unwrap();
}

/// A swap that died after writing the target's login and before committing (a crash): nobody
/// holds the swap lock any more, the profile has exactly b's login, the store still says a. The
/// next write-back completes the swap instead of storing b's login as a's.
#[test]
fn a_swap_that_died_after_writing_is_completed() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let target = store.credential(b).unwrap().unwrap();
    mark_pending(&store, &target.refresh_fp, Some(a), b);
    credfile::write_oauth(&env.cred_file(), &target.oauth).unwrap();

    statusline::sync(&store, &env.paths).unwrap();
    assert_eq!(store.active_account().unwrap().unwrap().id, b);
    assert_eq!(
        store.credential(a).unwrap().unwrap().oauth["refreshToken"],
        "refresh-a1",
        "a's login is untouched"
    );
    assert!(swap::pending_swap(&store).unwrap().is_none());
    assert_eq!(
        store
            .last_event_of(kind::SWAP)
            .unwrap()
            .unwrap()
            .reason
            .as_deref(),
        Some("completed")
    );
}

/// The profile's login changed since the swap died (Claude Code renewed whichever login it
/// had), so its fingerprint is neither account's. Until Claude Code says whose it is, nothing is
/// stored from the profile, and no swap runs.
#[test]
fn an_unidentified_login_after_a_dead_swap_stops_everything_until_settled() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let target = store.credential(b).unwrap().unwrap();
    mark_pending(&store, &target.refresh_fp, Some(a), b);
    credfile::write_oauth(&env.cred_file(), &oauth("someone-renewed")).unwrap();

    let tick = |at: i64| {
        statusline::tick(
            &store,
            &env.paths,
            &five_hour_input(10.0, time::now_s() + 3600),
            at,
        )
        .unwrap()
    };
    let now = time::now_ms();
    assert!(
        tick(now).settle_login,
        "settling is started in the background"
    );
    assert!(!tick(now + 5_000).settle_login, "at most once a minute");
    assert!(tick(now + 61_000).settle_login);
    statusline::sync(&store, &env.paths).unwrap();
    assert_eq!(
        store.credential(a).unwrap().unwrap().oauth["refreshToken"],
        "refresh-a1"
    );
    assert_eq!(
        store.credential(b).unwrap().unwrap().oauth["refreshToken"],
        "refresh-b1"
    );
    assert_eq!(store.active_account().unwrap().unwrap().id, a);
    assert!(swap::pending_swap(&store).unwrap().is_some());
    assert!(matches!(
        swap::run(&store, &env.paths, &manual(None)).unwrap(),
        SwapOutcome::Skipped { .. }
    ));
}

/// A swap that died before writing: the profile still has a's login. Only the mark goes.
#[test]
fn a_swap_that_died_before_writing_is_forgotten() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let target = store.credential(b).unwrap().unwrap();
    mark_pending(&store, &target.refresh_fp, Some(a), b);
    statusline::sync(&store, &env.paths).unwrap();
    assert_eq!(store.active_account().unwrap().unwrap().id, a);
    assert!(swap::pending_swap(&store).unwrap().is_none());
}

/// The only accounts that could take over are busy (their logins locked by a usage check): the
/// swap is skipped, and the pool is not reported as exhausted.
#[test]
fn busy_accounts_are_not_an_exhausted_pool() {
    let env = Env::new();
    let (store, a, b, c) = pool(&env);
    store.set_setting("lock_timeout_ms", Some("200")).unwrap();
    let _b = DirLock::acquire(
        &env.paths.account_lock(b),
        LockOptions::claude(Duration::ZERO),
    )
    .unwrap();
    let _c = DirLock::acquire(
        &env.paths.account_lock(c),
        LockOptions::claude(Duration::ZERO),
    )
    .unwrap();
    let out = swap::run(
        &store,
        &env.paths,
        &SwapRequest {
            from: Some(a),
            to: None,
            reason: Reason::Manual,
            wait: true,
        },
    )
    .unwrap();
    assert!(matches!(out, SwapOutcome::Skipped { .. }), "{out:?}");
    assert!(store.last_event_of(kind::POOL_EXHAUSTED).unwrap().is_none());
    assert_eq!(store.active_account().unwrap().unwrap().id, a);
}

/// Both accounts at their limit with the same reset time: the new account's genuine reading is
/// an exact copy of the old one's and is dropped, so only its own reading can tell. The first
/// such reading asks for a usage check of the account in use at once; a session idle since the
/// swap repeating the same reading does not keep asking.
#[test]
fn a_dropped_reading_at_a_limit_asks_for_a_confirmation_once_per_account_in_use() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    store.set_setting("usage_checks", Some("true")).unwrap();
    let now = time::now_ms();
    let shared = time::now_s() + 2 * 3600;
    store
        .record_sample(&five_only(b, 40.0, shared, now - 60_000))
        .unwrap();
    known_as_own(&store, b);
    store
        .record_sample(&five_only(a, 100.0, shared, now))
        .unwrap();
    swap::run(&store, &env.paths, &manual(Some(b))).unwrap();

    let tick = |at: i64| {
        statusline::tick(
            &store,
            &env.paths,
            &session_input("s", 9000, 100.0, shared),
            at,
        )
        .unwrap()
    };
    let first = tick(now + 130_000);
    assert!(first.confirm, "the first one asks at once");
    for later in [160_000, 600_000, 3_600_000] {
        assert!(!tick(now + later).confirm, "an exact copy asks only once");
    }
}

/// A swap from a to b stopped half way, and then the profile's login was rejected (blanked):
/// whichever login it was, a's (stored before the write) goes back into the profile, and b is
/// held back until a check shows whether its login still works.
#[test]
fn a_dead_swap_whose_login_was_then_rejected_puts_the_outgoing_login_back() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let target = store.credential(b).unwrap().unwrap();
    mark_pending(&store, &target.refresh_fp, Some(a), b);
    let mut dead = oauth("b1");
    dead["refreshToken"] = json!("");
    dead["accessToken"] = json!("");
    credfile::write_oauth(&env.cred_file(), &dead).unwrap();

    assert_eq!(
        swap::finish_dead_swap(&store, &env.paths, swap::Settler::Locked).unwrap(),
        swap::Settle::Settled
    );
    assert_eq!(refresh_of(&env.file_doc()), "refresh-a1");
    assert_eq!(store.active_account().unwrap().unwrap().id, a);
    assert!(usage_check::state(&store, b).unwrap().held_back);
    assert!(swap::pending_swap(&store).unwrap().is_none());
}

/// While the profile's login is unidentified after a stopped swap, no swap runs, not even one
/// asked for by hand: it would overwrite the only copy of a login nobody can identify.
#[test]
fn no_swap_runs_while_the_profiles_login_is_unidentified() {
    let env = Env::new();
    let (store, a, b, c) = pool(&env);
    let target = store.credential(b).unwrap().unwrap();
    mark_pending(&store, &target.refresh_fp, Some(a), b);
    credfile::write_oauth(&env.cred_file(), &oauth("whose")).unwrap();

    assert!(matches!(
        swap::run(&store, &env.paths, &manual(Some(c))).unwrap(),
        SwapOutcome::Skipped { .. }
    ));
    assert_eq!(refresh_of(&env.file_doc()), "refresh-whose", "left alone");
    assert_eq!(
        store.credential(a).unwrap().unwrap().oauth["refreshToken"],
        "refresh-a1"
    );
    assert!(swap::pending_swap(&store).unwrap().is_some());
}

/// A `/login` in the profile with an account outside the pool: the next swap must not store that
/// login as the outgoing account's.
#[test]
fn a_foreign_login_in_the_profile_is_never_stored_as_the_active_accounts() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    store.set_meta(swap::META_PROFILE_FOREIGN, "1").unwrap();
    credfile::write_oauth(&env.cred_file(), &oauth("stranger")).unwrap();
    assert_eq!(
        swap::run(&store, &env.paths, &manual(Some(b))).unwrap(),
        SwapOutcome::Swapped {
            from: Some(a),
            to: b
        }
    );
    assert_eq!(
        store.credential(a).unwrap().unwrap().oauth["refreshToken"],
        "refresh-a1"
    );
}

/// A fresh login of the active account is not written into the profile while a stopped swap left
/// the profile's login unidentified: that login may be the only copy of another account's.
#[test]
fn a_relogin_does_not_overwrite_an_unidentified_login() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let target = store.credential(b).unwrap().unwrap();
    mark_pending(&store, &target.refresh_fp, Some(a), b);
    credfile::write_oauth(&env.cred_file(), &oauth("whose")).unwrap();
    assert_eq!(
        swap::install_active_login(&store, &env.paths, a, &oauth("a-fresh")).unwrap(),
        swap::ActiveLogin::Unidentified
    );
    assert_eq!(refresh_of(&env.file_doc()), "refresh-whose");

    // Without a stopped swap it is written as before.
    store.delete_meta(swap::META_SWAP_PENDING).unwrap();
    assert_eq!(
        swap::install_active_login(&store, &env.paths, a, &oauth("a-fresh")).unwrap(),
        swap::ActiveLogin::Written
    );
    assert_eq!(refresh_of(&env.file_doc()), "refresh-a-fresh");
}

/// A swap from a to b stopped after writing b's login, and then a is signed in again. Settling
/// the swap makes b active, so a's fresh login is not written over b's: the store and the profile
/// would disagree, and b's renewals would be stored as a's.
#[test]
fn a_relogin_of_an_account_a_settled_swap_left_does_not_overwrite_the_new_active() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let target = store.credential(b).unwrap().unwrap();
    mark_pending(&store, &target.refresh_fp, Some(a), b);
    credfile::write_oauth(&env.cred_file(), &target.oauth).unwrap();
    store
        .save_credential(a, &oauth("a-fresh"), "login")
        .unwrap();

    assert_eq!(
        swap::install_active_login(&store, &env.paths, a, &oauth("a-fresh")).unwrap(),
        swap::ActiveLogin::NoLongerActive
    );
    assert_eq!(store.active_account().unwrap().unwrap().id, b);
    assert_eq!(refresh_of(&env.file_doc()), "refresh-b1");
    assert_eq!(
        store.credential(a).unwrap().unwrap().oauth["refreshToken"],
        "refresh-a-fresh"
    );
}

/// The profile was signed in to an account outside the pool, and then the active account is
/// signed in again: its login is in the profile now, so Claude Code's renewals of it are stored.
#[test]
fn a_relogin_ends_a_foreign_login_in_the_profile() {
    let env = Env::new();
    let (store, a, _, _) = pool(&env);
    store.set_meta(swap::META_PROFILE_FOREIGN, "1").unwrap();
    store
        .save_credential(a, &oauth("a-fresh"), "login")
        .unwrap();
    assert_eq!(
        swap::install_active_login(&store, &env.paths, a, &oauth("a-fresh")).unwrap(),
        swap::ActiveLogin::Written
    );

    credfile::write_oauth(&env.cred_file(), &oauth("a-renewed")).unwrap();
    statusline::sync(&store, &env.paths).unwrap();
    assert_eq!(
        store.credential(a).unwrap().unwrap().oauth["refreshToken"],
        "refresh-a-renewed"
    );
}

/// Seats a and b without a weekly limit (two distinct 5-hour windows and never a weekly one),
/// and c with a weekly limit, 20% used, resetting in three days: spending its 78 points at the
/// guessed pace needs more working hours than three days hold.
fn seats_and_an_expiring_week(env: &Env) -> (Store, i64, i64, i64) {
    let (store, a, b, c) = pool(env);
    store.set_order(&[a, b, c]).unwrap();
    let now_s = time::now_s();
    for account in [a, b] {
        store
            .record_sample(&five_only(
                account,
                30.0,
                now_s - 20 * 3600,
                (now_s - 30 * 3600) * 1000,
            ))
            .unwrap();
        store
            .record_sample(&five_only(
                account,
                5.0,
                now_s + 3 * 3600,
                (now_s - 3600) * 1000,
            ))
            .unwrap();
    }
    store.record_sample(&sample(c, 0.0, 20.0)).unwrap();
    store
        .set_setting("use_expiring_weekly", Some("true"))
        .unwrap();
    (store, a, b, c)
}

/// An account whose weekly budget would reset unused goes ahead of the seats without a weekly
/// limit, with the reason; with the setting off, the seats come first as before.
#[test]
fn a_weekly_budget_that_would_reset_unused_moves_its_account_ahead() {
    let env = Env::new();
    let (store, a, _, c) = seats_and_an_expiring_week(&env);
    assert_eq!(swap::next_target(&store, Some(a)).unwrap().unwrap().id, c);
    let (pool, policy) = swap::load_pool(&store, time::now_s()).unwrap();
    let ranked = ccr_core::scheduler::ranking(&pool, policy, time::now_s());
    let first = ranked.iter().find(|r| r.account_id == c).unwrap();
    assert!(
        first.reason.starts_with("weekly budget expires in"),
        "{}",
        first.reason
    );

    // A swap that takes it for its budget remembers that for this week, so it is taken back
    // after its 5-hour limit.
    assert_eq!(
        swap::run(&store, &env.paths, &manual(None)).unwrap(),
        SwapOutcome::Swapped {
            from: Some(a),
            to: c
        }
    );
    let week_reset = time::now_s() + 86_400 * 3;
    let marked = store
        .meta_i64(&ccr_core::budget::promoted_key(c))
        .unwrap()
        .expect("marked");
    assert!((marked - week_reset).abs() <= 5, "{marked} vs {week_reset}");

    store
        .set_setting("use_expiring_weekly", Some("false"))
        .unwrap();
    assert_eq!(swap::next_target(&store, Some(c)).unwrap().unwrap().id, a);
}

/// The account in use is spending a weekly budget that would otherwise reset unused: the seats
/// that rank above it by the user's order do not take it back.
#[test]
fn the_account_in_use_keeps_spending_a_budget_that_would_reset_unused() {
    let env = Env::new();
    let (store, _, _, c) = seats_and_an_expiring_week(&env);
    swap::run(&store, &env.paths, &manual(Some(c))).unwrap();
    store
        .set_setting("switch_back_dwell_s", Some("600"))
        .unwrap();
    let now_s = time::now_s();
    let input = format!(
        r#"{{"model":{{"id":"claude-opus-4-1"}},"rate_limits":{{"five_hour":{{"used_percentage":12,"resets_at":{}}},"seven_day":{{"used_percentage":21,"resets_at":{}}}}}}}"#,
        now_s + 3600,
        now_s + 86_400 * 3
    );
    let later = time::now_ms() + 601_000;
    let tick = statusline::tick(&store, &env.paths, &StatusInput::parse(&input), later).unwrap();
    assert!(tick.swap.is_none(), "{:?}", tick.swap);
}

/// A weekly cap below the weekly threshold keeps the rest of the account's week for the user:
/// rotation stops using it at the cap.
#[test]
fn a_weekly_cap_keeps_the_rest_of_the_week_for_you() {
    let env = Env::new();
    let (store, a, b, c) = pool(&env);
    // The order alone decides here.
    store
        .set_setting("prefer_no_weekly_limit", Some("false"))
        .unwrap();
    store.set_setting("strict_order", Some("true")).unwrap();
    store.set_order(&[c, b, a]).unwrap();
    store.record_sample(&sample(c, 10.0, 55.0)).unwrap();
    assert_eq!(swap::next_target(&store, Some(a)).unwrap().unwrap().id, c);

    store.set_weekly_cap(c, Some(50.0)).unwrap();
    assert_eq!(swap::next_target(&store, Some(a)).unwrap().unwrap().id, b);
    let (pool, policy) = swap::load_pool(&store, time::now_s()).unwrap();
    let ranked = ccr_core::scheduler::ranking(&pool, policy, time::now_s());
    let capped = ranked.iter().find(|r| r.account_id == c).unwrap();
    assert!(
        capped.reason.starts_with("weekly limit at 55%"),
        "{}",
        capped.reason
    );

    store.set_weekly_cap(c, None).unwrap();
    assert_eq!(swap::next_target(&store, Some(a)).unwrap().unwrap().id, c);
    assert!(store.set_weekly_cap(c, Some(0.5)).is_err());
}

/// The pace is the last hour's use of every account, against each account's usual pace, so it
/// carries across a swap: a used 10 points in its 20 minutes, then b 5 in its 10, at a usual 10
/// an hour each, over 40 minutes in all: 1.5 usual hours in 2/3 of an hour.
#[test]
fn the_pace_counts_every_account_used_in_the_last_hour() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let now = time::now_ms();
    let reset = time::now_s() + 3 * 3600;
    let at = |account: i64, minutes_ago: i64, five: f64| Sample {
        ts: now - minutes_ago * 60_000,
        account_id: account,
        profile: "rotate".into(),
        session_id: None,
        model: None,
        windows: vec![window("five_hour", five, reset)],
    };
    for s in [
        at(a, 40, 10.0),
        at(a, 20, 20.0),
        at(b, 10, 5.0),
        at(b, 0, 10.0),
    ] {
        store.record_sample(&s).unwrap();
    }
    let rates = ccr_core::budget::Rates::default();
    let pace = ccr_core::forecast::pace(&store, &rates, now)
        .unwrap()
        .unwrap();
    assert!((pace - 2.25).abs() < 1e-9, "{pace}");

    // Nothing used in the last hour: no pace.
    let later = now + 2 * 3_600_000;
    assert!(
        ccr_core::forecast::pace(&store, &rates, later)
            .unwrap()
            .is_none()
    );
}

/// One account, two sessions each reporting every 30 seconds for 40 minutes: one working (30%
/// to 33%) and one idle, repeating 30%. The pace is the working one's, 3 points in 40 minutes
/// at a usual 10 an hour (0.45), not every rebound from the idle session's value; and it fades
/// once nothing is used.
#[test]
fn the_pace_ignores_an_idle_session_and_fades_when_work_stops() {
    let env = Env::new();
    let (store, a, _, _) = pool(&env);
    let now = time::now_ms();
    let reset = time::now_s() + 3 * 3600;
    for half_minute in 0..=80i64 {
        let ts = now - (80 - half_minute) * 30_000;
        let working = 30.0 + (3 * half_minute / 80) as f64;
        for (session, pct, offset) in [("w", working, 0), ("i", 30.0, 1_000)] {
            store
                .record_sample(&Sample {
                    ts: ts - offset,
                    account_id: a,
                    profile: "rotate".into(),
                    session_id: Some(session.into()),
                    model: None,
                    windows: vec![window("five_hour", pct, reset)],
                })
                .unwrap();
        }
    }
    let rates = ccr_core::budget::Rates::default();
    let pace = ccr_core::forecast::pace(&store, &rates, now)
        .unwrap()
        .unwrap();
    assert!((pace - 0.45).abs() < 0.05, "{pace}");

    let later = ccr_core::forecast::pace(&store, &rates, now + 20 * 60_000)
        .unwrap()
        .unwrap();
    assert!(later < pace * 0.8, "{later} vs {pace}");
}

/// An idle session repeating an older value of the window (20%) next to a working one (60% to
/// 64% over 80 minutes), readings 15 seconds apart: the pace must not depend on which session's
/// reading happens to be the oldest of the hour.
#[test]
fn the_pace_does_not_depend_on_which_reading_is_oldest() {
    let env = Env::new();
    let (store, a, _, _) = pool(&env);
    let end = time::now_ms() - 60_000;
    let reset = time::now_s() + 3 * 3600;
    for quarter in 0..=320i64 {
        let ts = end - (320 - quarter) * 15_000;
        let (session, pct) = if quarter % 2 == 0 {
            ("w", 60.0 + (4 * quarter / 320) as f64)
        } else {
            ("i", 20.0)
        };
        store
            .record_sample(&Sample {
                ts,
                account_id: a,
                profile: "rotate".into(),
                session_id: Some(session.into()),
                model: None,
                windows: vec![window("five_hour", pct, reset)],
            })
            .unwrap();
    }
    let rates = ccr_core::budget::Rates::default();
    let paces: Vec<f64> = [0, 5_000, 10_000, 20_000, 25_000]
        .iter()
        .map(|d| {
            ccr_core::forecast::pace(&store, &rates, end + d)
                .unwrap()
                .unwrap_or(0.0)
        })
        .collect();
    let (lo, hi) = paces
        .iter()
        .fold((f64::MAX, 0.0f64), |(lo, hi), &p| (lo.min(p), hi.max(p)));
    assert!(hi <= 1.0 && hi - lo < 0.15, "{paces:?}");
}

/// Sessions open and idle for 45 minutes, then 5 minutes of work: use that started less than 15
/// minutes ago shows no pace yet.
#[test]
fn idle_sessions_before_work_do_not_count_as_time_used() {
    let env = Env::new();
    let (store, a, _, _) = pool(&env);
    let now = time::now_ms();
    let reset = time::now_s() + 3 * 3600;
    for minutes_ago in (0..=50i64).rev() {
        let pct = if minutes_ago > 5 {
            20.0
        } else {
            25.0 - minutes_ago as f64
        };
        store
            .record_sample(&Sample {
                ts: now - minutes_ago * 60_000,
                account_id: a,
                profile: "rotate".into(),
                session_id: Some("w".into()),
                model: None,
                windows: vec![window("five_hour", pct, reset)],
            })
            .unwrap();
    }
    let rates = ccr_core::budget::Rates::default();
    assert!(
        ccr_core::forecast::pace(&store, &rates, now)
            .unwrap()
            .is_none()
    );
}

/// An account the user left out of using weekly budget before it resets is used in the order
/// like any other, even when its budget would reset unused; turned back on, it moves ahead again.
#[test]
fn an_account_left_out_is_not_moved_ahead_for_its_budget() {
    let env = Env::new();
    let (store, a, b, c) = seats_and_an_expiring_week(&env);
    assert!(
        store.account(c).unwrap().unwrap().use_expiring,
        "on by default"
    );
    store.set_use_expiring(c, false).unwrap();
    assert_eq!(swap::next_target(&store, Some(a)).unwrap().unwrap().id, b);
    store.set_use_expiring(c, true).unwrap();
    assert_eq!(swap::next_target(&store, Some(a)).unwrap().unwrap().id, c);
}
