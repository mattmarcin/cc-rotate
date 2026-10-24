//! End-to-end flows over real files: swaps, statusline ticks, write-back, logins.
//! Each test gets its own data and profile directories.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use ccr_core::lock::{DirLock, LockOptions};
use ccr_core::scheduler::Reason;
use ccr_core::statusline::{self, StatusInput};
use ccr_core::store::{AccountIdentity, AccountState, Sample, kind};
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
        std::fs::create_dir_all(&profile).unwrap();
        Env {
            paths: Paths::new(data, profile),
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

fn sample(account: i64, five: f64, week: f64) -> Sample {
    Sample {
        ts: time::now_ms(),
        account_id: account,
        profile: "rotate".into(),
        session_id: None,
        five_pct: Some(five),
        five_resets_at: Some(time::now_s() + 3600),
        week_pct: Some(week),
        week_resets_at: Some(time::now_s() + 86_400 * 3),
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
fn scheduler_picks_least_used_and_skips_exhausted() {
    let env = Env::new();
    let (store, a, b, c) = pool(&env);
    store.record_sample(&sample(b, 10.0, 60.0)).unwrap();
    store.record_sample(&sample(c, 50.0, 20.0)).unwrap();
    let out = swap::run(&store, &env.paths, &manual(None)).unwrap();
    assert_eq!(
        out,
        SwapOutcome::Swapped {
            from: Some(a),
            to: c
        }
    );

    store.record_sample(&sample(b, 99.0, 60.0)).unwrap();
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
    let (store, a, _, _) = pool(&env);
    let input = |pct: f64| {
        StatusInput::parse(&format!(
            r#"{{"session_id":"s","rate_limits":{{"five_hour":{{"used_percentage":{pct},"resets_at":{}}},"seven_day":{{"used_percentage":30,"resets_at":{}}}}}}}"#,
            time::now_s() + 3600,
            time::now_s() + 86_400
        ))
    };
    let r = statusline::tick(&store, &env.paths, &input(50.0), time::now_ms()).unwrap();
    assert!(r.swap.is_none());
    assert_eq!(store.last_sample(a).unwrap().unwrap().five_pct, Some(50.0));

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
        old.five_resets_at.unwrap(),
        old.week_resets_at.unwrap()
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
