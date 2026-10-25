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
use ccr_core::{Paths, Store, credfile, login, time};
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

    fn store(&self) -> Store {
        Store::open(&self.paths.store_db()).unwrap()
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
    // Usage is usage: b comes first in the order even though c has more weekly budget left.
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
            why: "within the swap cooldown".into()
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
        err.to_string().contains("signed in as sam@x.test"),
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
    assert!(
        err.to_string().contains("this sign-in was for work@x.test"),
        "{err}"
    );
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
    assert_eq!(
        detail["pick"], "no weekly limit, 1st in your order",
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
            why: "within the swap cooldown".into()
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

/// A status line reading from session `session`, whose API time so far is `api_ms`.
fn session_input(session: &str, api_ms: u64, pct: f64, resets_at: i64) -> StatusInput {
    StatusInput::parse(&format!(
        r#"{{"session_id":"{session}","cost":{{"total_api_duration_ms":{api_ms}}},"rate_limits":{{"five_hour":{{"used_percentage":{pct},"resets_at":{resets_at}}}}}}}"#
    ))
}

/// With the default settings (guard and cooldown both 120 s): a session that has been idle
/// since before a swap keeps reporting the old account's usage for as long as it stays idle.
/// Its reading is never recorded for the new account, so it cannot walk the rotation through the
/// pool; once it makes a request, its readings count again.
#[test]
fn a_session_idle_since_before_a_swap_is_ignored_however_long_it_idles() {
    let env = Env::new();
    let (store, a, b, _) = pool(&env);
    let now = time::now_ms();
    let a_resets = time::now_s() + 3 * 86_400;
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
    assert!(
        store.last_sample(b).unwrap().is_none(),
        "a's usage is never recorded as b's"
    );

    // The session makes a request on b: its reading is b's.
    statusline::tick(
        &store,
        &env.paths,
        &session_input("idle", 7000, 12.0, time::now_s() + 4 * 3600),
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
