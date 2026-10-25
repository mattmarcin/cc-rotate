//! Usage checks through a real child process: a stand-in `claude` (selected with the
//! `claude_path` setting) that records how it was started and answers like Claude Code's
//! `claude -p /usage` does, by writing a usage reading into the scratch profile's `.claude.json`.

use std::path::{Path, PathBuf};

use ccr_core::scheduler::Reason;
use ccr_core::store::{AccountIdentity, AccountState, Sample, SampleWindow, kind};
use ccr_core::swap::{self, SwapOutcome, SwapRequest};
use ccr_core::usage_check::{self, CheckOutcome, Trigger};
use ccr_core::{Paths, Store, statusline, time};
use serde_json::{Value, json};

struct Env {
    root: tempfile::TempDir,
    paths: Paths,
    bin: PathBuf,
}

/// A stand-in `claude` that writes `seen.txt` (its config dir and arguments), copies the login
/// it was given to `received.json`, then puts `config.json` and `credentials.json` from its own
/// directory into the scratch profile when they exist, and runs `during.sh` (`during.cmd` on
/// Windows) from there if there is one: what happens elsewhere while Claude Code runs.
fn env() -> Env {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths::new(root.path().join("data"), root.path().join("profile"))
        .with_home(root.path().join("home"));
    std::fs::create_dir_all(&paths.profile_dir).unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    #[cfg(unix)]
    let claude = {
        use std::os::unix::fs::PermissionsExt;
        let path = bin.join("claude");
        std::fs::write(
            &path,
            r#"#!/bin/sh
dir=$(dirname "$0")
{ echo "config=$CLAUDE_CONFIG_DIR"; printf 'arg=%s\n' "$@"; } > "$dir/seen.txt"
cp "$CLAUDE_CONFIG_DIR/.credentials.json" "$dir/received.json"
[ -f "$dir/config.json" ] && cp "$dir/config.json" "$CLAUDE_CONFIG_DIR/.claude.json"
[ -f "$dir/credentials.json" ] && cp "$dir/credentials.json" "$CLAUDE_CONFIG_DIR/.credentials.json"
[ -f "$dir/during.sh" ] && sh "$dir/during.sh"
echo '{"type":"result","num_turns":0,"local_command":"usage"}'
exit 0
"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    #[cfg(windows)]
    let claude = {
        let path = bin.join("claude.cmd");
        std::fs::write(
            &path,
            [
                "@echo off",
                "setlocal",
                "set \"dir=%~dp0\"",
                ">\"%dir%seen.txt\" echo config=%CLAUDE_CONFIG_DIR%",
                "for %%a in (%*) do >>\"%dir%seen.txt\" echo arg=%%~a",
                "copy /y \"%CLAUDE_CONFIG_DIR%\\.credentials.json\" \"%dir%received.json\" >nul",
                "if exist \"%dir%config.json\" copy /y \"%dir%config.json\" \"%CLAUDE_CONFIG_DIR%\\.claude.json\" >nul",
                "if exist \"%dir%credentials.json\" copy /y \"%dir%credentials.json\" \"%CLAUDE_CONFIG_DIR%\\.credentials.json\" >nul",
                "if exist \"%dir%during.cmd\" call \"%dir%during.cmd\"",
                "echo {\"type\":\"result\",\"num_turns\":0,\"local_command\":\"usage\"}",
                "exit /b 0",
                "",
            ]
            .join("\r\n"),
        )
        .unwrap();
        path
    };
    let e = Env { root, paths, bin };
    e.store()
        .set_setting("claude_path", Some(&claude.display().to_string()))
        .unwrap();
    e
}

impl Env {
    fn store(&self) -> Store {
        Store::open(&self.paths.store_db()).unwrap()
    }

    /// The reading the stand-in reports: `pct` of the 5-hour limit, stamped with `uuid`.
    fn answer(&self, uuid: &str, pct: f64) {
        let config = json!({
            "oauthAccount": {"accountUuid": uuid},
            "cachedUsageUtilization": {
                "fetchedAtMs": time::now_ms() + 60_000,
                "accountUuid": uuid,
                "utilization": {"limits": [
                    {"kind": "session", "percent": pct, "resets_at": "2099-01-01T00:00:00Z"}
                ]}
            }
        });
        std::fs::write(self.bin.join("config.json"), config.to_string()).unwrap();
    }

    fn seen(&self) -> String {
        std::fs::read_to_string(self.bin.join("seen.txt")).unwrap_or_default()
    }
}

fn oauth(tag: &str) -> Value {
    json!({
        "accessToken": format!("access-{tag}"),
        "refreshToken": format!("refresh-{tag}"),
        "expiresAt": time::now_ms() + 3_600_000,
        "subscriptionType": "team",
    })
}

fn add(store: &Store, label: &str, state: AccountState) -> i64 {
    let ident = AccountIdentity {
        account_uuid: Some(format!("u{label}")),
        email: Some(format!("{label}@x.test")),
        ..Default::default()
    };
    let id = store.insert_account(label, &ident, state).unwrap();
    store
        .save_credential(id, &oauth(&format!("{label}1")), "login")
        .unwrap();
    id
}

fn five_hour(account_id: i64, pct: f64, ts: i64) -> Sample {
    Sample {
        ts,
        account_id,
        profile: "rotate".into(),
        session_id: None,
        model: None,
        windows: vec![SampleWindow {
            name: "five_hour".into(),
            pct,
            resets_at: Some(time::now_s() + 3600),
        }],
    }
}

fn scratch_left(paths: &Paths) -> Vec<PathBuf> {
    std::fs::read_dir(paths.staging_root())
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default()
}

#[test]
fn a_manual_check_runs_claude_code_in_a_scratch_profile() {
    let e = env();
    let store = e.store();
    let a = add(&store, "a", AccountState::Parked);
    e.answer("ua", 42.0);
    std::fs::write(
        e.bin.join("credentials.json"),
        json!({"claudeAiOauth": oauth("a2")}).to_string(),
    )
    .unwrap();

    let out = usage_check::check(&store, &e.paths, a, Trigger::Manual).unwrap();
    assert!(
        matches!(&out, CheckOutcome::Read { windows } if windows[0].pct == 42.0),
        "{out:?}"
    );

    let seen = e.seen();
    let staging = e.paths.staging_root();
    assert!(
        seen.lines()
            .any(|l| l.starts_with("config=") && Path::new(&l[7..]).starts_with(&staging)),
        "{seen}"
    );
    for arg in ["arg=-p", "arg=/usage", "arg=--debug-file"] {
        assert!(seen.lines().any(|l| l == arg), "{arg} missing: {seen}");
    }
    let received: Value =
        serde_json::from_str(&std::fs::read_to_string(e.bin.join("received.json")).unwrap())
            .unwrap();
    assert_eq!(received["claudeAiOauth"]["refreshToken"], "refresh-a1");
    assert_eq!(
        store.credential(a).unwrap().unwrap().oauth["refreshToken"],
        "refresh-a2",
        "the login Claude Code renewed is stored"
    );
    assert!(
        scratch_left(&e.paths).is_empty(),
        "{:?}",
        scratch_left(&e.paths)
    );
    drop(e.root);
}

/// The swap's pick has a reading two hours old; checking it shows it was used up elsewhere, so
/// the swap goes to the next account instead.
#[test]
fn a_swap_checks_an_old_pick_first() {
    let e = env();
    let store = e.store();
    let a = add(&store, "a", AccountState::Active);
    let b = add(&store, "b", AccountState::Parked);
    let c = add(&store, "c", AccountState::Parked);
    std::fs::write(
        e.paths.credentials_file(),
        json!({"claudeAiOauth": oauth("a1")}).to_string(),
    )
    .unwrap();
    store
        .record_sample(&five_hour(a, 99.0, time::now_ms()))
        .unwrap();
    store
        .record_sample(&five_hour(b, 10.0, time::now_ms() - 2 * 3_600_000))
        .unwrap();
    store
        .record_sample(&five_hour(c, 30.0, time::now_ms()))
        .unwrap();
    e.answer("ub", 100.0);

    let out = swap::run(
        &store,
        &e.paths,
        &SwapRequest {
            from: Some(a),
            to: None,
            reason: Reason::FiveHour,
            wait: false,
        },
    )
    .unwrap();
    assert_eq!(
        out,
        SwapOutcome::Swapped {
            from: Some(a),
            to: c
        }
    );
    let reading = store.last_sample(b).unwrap().unwrap();
    assert_eq!(
        (reading.profile.as_str(), reading.windows[0].pct),
        ("check", 100.0)
    );
    let event = store.last_event_of(kind::SWAP).unwrap().unwrap();
    assert_eq!(
        event.detail.unwrap()["checked"],
        json!([{"label": "b", "outcome": "read"}])
    );
    assert!(!e.paths.account_lock(c).exists());
}

/// With checks off, a swap never starts Claude Code.
#[test]
fn checks_can_be_turned_off() {
    let e = env();
    let store = e.store();
    store.set_setting("usage_checks", Some("false")).unwrap();
    let a = add(&store, "a", AccountState::Active);
    let b = add(&store, "b", AccountState::Parked);
    std::fs::write(
        e.paths.credentials_file(),
        json!({"claudeAiOauth": oauth("a1")}).to_string(),
    )
    .unwrap();
    assert_eq!(
        swap::run(
            &store,
            &e.paths,
            &SwapRequest {
                from: Some(a),
                to: None,
                reason: Reason::Manual,
                wait: true,
            },
        )
        .unwrap(),
        SwapOutcome::Swapped {
            from: Some(a),
            to: b
        }
    );
    assert!(e.seen().is_empty(), "Claude Code ran: {}", e.seen());
}

/// An account whose login is in doubt after an interrupted check is not swapped to.
#[test]
fn a_held_back_account_is_passed_over() {
    let e = env();
    let store = e.store();
    store.set_setting("usage_checks", Some("false")).unwrap();
    let a = add(&store, "a", AccountState::Active);
    let b = add(&store, "b", AccountState::Parked);
    let c = add(&store, "c", AccountState::Parked);
    std::fs::write(
        e.paths.credentials_file(),
        json!({"claudeAiOauth": oauth("a1")}).to_string(),
    )
    .unwrap();
    store
        .set_meta(
            &format!("usage_check:{b}"),
            json!({"held_back": true}).to_string(),
        )
        .unwrap();
    assert_eq!(
        swap::run(
            &store,
            &e.paths,
            &SwapRequest {
                from: Some(a),
                to: None,
                reason: Reason::Manual,
                wait: true,
            },
        )
        .unwrap(),
        SwapOutcome::Swapped {
            from: Some(a),
            to: c
        }
    );
    assert!(
        swap::run(
            &store,
            &e.paths,
            &SwapRequest {
                from: None,
                to: Some(b),
                reason: Reason::Manual,
                wait: true,
            },
        )
        .is_err(),
        "not even by hand"
    );
}

/// The status line reported the account in use at its limit, but with a reset time that is not
/// known to be its own (it could be a session still showing the previous account). The swap
/// reads the account's own usage first, finds it fine, and does not swap.
#[test]
fn a_threshold_swap_on_an_unsure_reading_confirms_first() {
    let e = env();
    let store = e.store();
    let a = add(&store, "a", AccountState::Active);
    add(&store, "b", AccountState::Parked);
    std::fs::write(
        e.paths.credentials_file(),
        json!({"claudeAiOauth": oauth("a1")}).to_string(),
    )
    .unwrap();
    store
        .record_sample(&five_hour(a, 99.0, time::now_ms()))
        .unwrap();
    // a's own usage, as Claude Code's /usage reports it.
    e.answer("ua", 30.0);

    let out = swap::run(
        &store,
        &e.paths,
        &SwapRequest {
            from: Some(a),
            to: None,
            reason: Reason::FiveHour,
            wait: false,
        },
    )
    .unwrap();
    assert!(matches!(out, SwapOutcome::Skipped { .. }), "{out:?}");
    let reading = store.last_sample(a).unwrap().unwrap();
    assert_eq!(
        (reading.profile.as_str(), reading.windows[0].pct),
        ("check", 30.0)
    );
    let received: Value =
        serde_json::from_str(&std::fs::read_to_string(e.bin.join("received.json")).unwrap())
            .unwrap();
    assert_eq!(
        received["claudeAiOauth"]["refreshToken"], "",
        "the account in use is read with a login Claude Code cannot renew"
    );
    assert_eq!(store.active_account().unwrap().unwrap().id, a);
}

/// After a swap from a to b died half way, the profile's login was renewed, so its fingerprint
/// is neither account's. Claude Code identifies it (from a copy it cannot renew), and the swap is
/// completed or forgotten accordingly.
#[test]
fn a_dead_swap_is_settled_by_asking_claude_code_whose_login_it_is() {
    for (who, active_after) in [("ub", "b"), ("ua", "a")] {
        let e = env();
        let store = e.store();
        let a = add(&store, "a", AccountState::Active);
        let b = add(&store, "b", AccountState::Parked);
        let target = store.credential(b).unwrap().unwrap();
        store
            .set_meta(
                swap::META_SWAP_PENDING,
                serde_json::to_string(&swap::PendingSwap {
                    fp: target.refresh_fp.clone(),
                    from: Some(a),
                    to: b,
                    at: time::now_ms(),
                })
                .unwrap(),
            )
            .unwrap();
        std::fs::write(
            e.paths.credentials_file(),
            json!({"claudeAiOauth": oauth("renewed")}).to_string(),
        )
        .unwrap();
        e.answer(who, 10.0);

        assert_eq!(
            swap::finish_dead_swap(&store, &e.paths, swap::Settler::Locked).unwrap(),
            swap::Settle::Settled
        );
        assert_eq!(
            store.active_account().unwrap().unwrap().label,
            active_after,
            "the login was {who}'s"
        );
        assert!(swap::pending_swap(&store).unwrap().is_none());
        let received: Value =
            serde_json::from_str(&std::fs::read_to_string(e.bin.join("received.json")).unwrap())
                .unwrap();
        assert_eq!(
            received["claudeAiOauth"]["refreshToken"], "",
            "never renewable"
        );
    }
}

/// A parked account whose login is the one in the profile (left there by a swap that stopped
/// half way) is never checked: Claude Code renewing it in the check would sign the profile's
/// sessions out.
#[test]
fn a_check_never_uses_the_login_the_profile_holds() {
    let e = env();
    let store = e.store();
    add(&store, "a", AccountState::Active);
    let b = add(&store, "b", AccountState::Parked);
    std::fs::write(
        e.paths.credentials_file(),
        json!({"claudeAiOauth": oauth("b1")}).to_string(),
    )
    .unwrap();
    let out = usage_check::check(&store, &e.paths, b, Trigger::Manual).unwrap();
    assert!(matches!(out, CheckOutcome::Skipped { .. }), "{out:?}");
    assert!(e.seen().is_empty(), "Claude Code ran: {}", e.seen());
}

/// No account is checked while a swap is pending: which stored logins are current is not settled.
#[test]
fn no_check_runs_while_a_swap_is_pending() {
    let e = env();
    let store = e.store();
    let a = add(&store, "a", AccountState::Active);
    let b = add(&store, "b", AccountState::Parked);
    let c = add(&store, "c", AccountState::Parked);
    store
        .set_meta(
            swap::META_SWAP_PENDING,
            serde_json::to_string(&swap::PendingSwap {
                fp: "x".into(),
                from: Some(a),
                to: b,
                at: time::now_ms(),
            })
            .unwrap(),
        )
        .unwrap();
    let out = usage_check::check(&store, &e.paths, c, Trigger::Manual).unwrap();
    assert!(matches!(out, CheckOutcome::Skipped { .. }), "{out:?}");
    assert!(e.seen().is_empty());
}

/// Put a login into the profile, the way Claude Code writes it.
fn profile_login(e: &Env, tag: &str) {
    std::fs::write(
        e.paths.credentials_file(),
        json!({"claudeAiOauth": oauth(tag)}).to_string(),
    )
    .unwrap();
}

/// Write the profile's `.claude.json` naming `uuid`, as `/login` does, with a modification time
/// `step` seconds ahead so every write is told apart.
fn profile_config(e: &Env, uuid: &str, step: u64) {
    let path = e.paths.profile_config();
    std::fs::write(
        &path,
        json!({"oauthAccount": {"accountUuid": uuid}}).to_string(),
    )
    .unwrap();
    let file = std::fs::File::options().write(true).open(&path).unwrap();
    file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(step))
        .unwrap();
}

/// Pool a (active, its login in the profile, which the write-back has seen) and b (parked),
/// with the profile's `.claude.json` naming `named`, as it still does after swaps (F14).
fn identified_pool(e: &Env, named: &str) -> (Store, i64, i64) {
    let store = e.store();
    let a = add(&store, "a", AccountState::Active);
    let b = add(&store, "b", AccountState::Parked);
    profile_login(e, "a1");
    profile_config(e, named, 1);
    statusline::sync(&store, &e.paths).unwrap();
    (store, a, b)
}

fn stored_refresh(store: &Store, id: i64) -> Value {
    store.credential(id).unwrap().unwrap().oauth["refreshToken"].clone()
}

fn settle(store: &Store, e: &Env) -> swap::Settle {
    swap::settle_profile(store, &e.paths).unwrap()
}

/// The profile's `.claude.json` still names x, an account outside the pool, from an earlier
/// `/login` (a swap does not update it, F14), and a is active with its own login. A second
/// `/login` with x changes the login but not the account the config names. The write-back does
/// not store x's login as a's; Claude Code identifies it, and the profile is marked foreign.
#[test]
fn a_second_login_of_the_account_the_config_names_is_not_stored_as_the_active_accounts() {
    let e = env();
    let (store, a, _) = identified_pool(&e, "ux");

    profile_login(&e, "x1");
    profile_config(&e, "ux", 2);
    e.answer("ux", 10.0);
    let tick = statusline::tick(
        &store,
        &e.paths,
        &statusline::StatusInput::default(),
        time::now_ms(),
    )
    .unwrap();
    assert!(
        tick.settle_login,
        "identifying the login is started in the background"
    );
    assert_eq!(
        stored_refresh(&store, a),
        "refresh-a1",
        "x's login was stored as a's"
    );

    assert_eq!(settle(&store, &e), swap::Settle::Settled);
    assert_eq!(stored_refresh(&store, a), "refresh-a1");
    assert_eq!(
        store.meta(swap::META_PROFILE_FOREIGN).unwrap().as_deref(),
        Some("1")
    );
    let event = store.last_event_of(kind::FOREIGN_LOGIN).unwrap().unwrap();
    assert_eq!(event.detail.unwrap()["account_uuid"], "ux");
    assert_eq!(settle(&store, &e), swap::Settle::Nothing, "settled once");
}

/// Claude Code renewed the active account's login: once Claude Code says it is a's, it is stored.
#[test]
fn a_renewal_is_stored_once_identified() {
    let e = env();
    let (store, a, _) = identified_pool(&e, "ua");

    profile_login(&e, "a2");
    statusline::sync(&store, &e.paths).unwrap();
    assert_eq!(
        stored_refresh(&store, a),
        "refresh-a1",
        "not before it is identified"
    );

    e.answer("ua", 10.0);
    assert_eq!(settle(&store, &e), swap::Settle::Settled);
    assert_eq!(stored_refresh(&store, a), "refresh-a2");
    assert!(e.seen().contains("arg=/usage"), "{}", e.seen());
    let received: Value =
        serde_json::from_str(&std::fs::read_to_string(e.bin.join("received.json")).unwrap())
            .unwrap();
    assert_eq!(
        received["claudeAiOauth"]["refreshToken"], "",
        "never renewable"
    );
    assert_eq!(store.active_account().unwrap().unwrap().id, a);
}

/// The config still names b, which was active before a swap to a. A `/login` with b changes the
/// login but not the account the config names: once Claude Code says it is b's, b becomes active
/// with that login, and a keeps its own.
#[test]
fn a_login_of_another_pool_account_makes_it_active() {
    let e = env();
    let (store, a, b) = identified_pool(&e, "ub");

    profile_login(&e, "b2");
    profile_config(&e, "ub", 2);
    statusline::sync(&store, &e.paths).unwrap();
    e.answer("ub", 10.0);
    assert_eq!(settle(&store, &e), swap::Settle::Settled);
    assert_eq!(store.active_account().unwrap().unwrap().id, b);
    assert_eq!(stored_refresh(&store, b), "refresh-b2");
    assert_eq!(stored_refresh(&store, a), "refresh-a1");
    assert!(store.last_event_of(kind::ADOPTED_LOGIN).unwrap().is_some());
}

/// A swap identifies a changed login before it stores the profile's login as the outgoing
/// account's.
#[test]
fn a_swap_identifies_a_changed_login_first() {
    let e = env();
    let (store, a, b) = identified_pool(&e, "ux");
    profile_login(&e, "x1");
    e.answer("ux", 10.0);

    let out = swap::run(
        &store,
        &e.paths,
        &SwapRequest {
            from: Some(a),
            to: Some(b),
            reason: Reason::Manual,
            wait: false,
        },
    )
    .unwrap();
    assert_eq!(
        out,
        SwapOutcome::Swapped {
            from: Some(a),
            to: b
        }
    );
    assert_eq!(
        stored_refresh(&store, a),
        "refresh-a1",
        "x's login was stored as a's"
    );
}

/// While Claude Code cannot say whose a changed login is, it is not stored and no swap runs.
/// After `UNIDENTIFIED_FOR_AT_MOST_MS` it is stored as the active account's, as before logins
/// were identified, so a Claude Code that cannot identify logins never stops the rotation.
#[test]
fn a_login_that_cannot_be_identified_is_stored_as_before_after_a_while() {
    let e = env();
    let (store, a, b) = identified_pool(&e, "ua");
    profile_login(&e, "a2");
    statusline::sync(&store, &e.paths).unwrap();

    // The stand-in reports nobody.
    assert_eq!(settle(&store, &e), swap::Settle::Unsure);
    assert_eq!(stored_refresh(&store, a), "refresh-a1");
    let out = swap::run(
        &store,
        &e.paths,
        &SwapRequest {
            from: Some(a),
            to: Some(b),
            reason: Reason::Manual,
            wait: false,
        },
    )
    .unwrap();
    assert!(matches!(out, SwapOutcome::Skipped { .. }), "{out:?}");

    let mut since: swap::Unidentified =
        serde_json::from_str(&store.meta(swap::META_LOGIN_UNIDENTIFIED).unwrap().unwrap()).unwrap();
    since.at -= swap::UNIDENTIFIED_FOR_AT_MOST_MS;
    store
        .set_meta(
            swap::META_LOGIN_UNIDENTIFIED,
            serde_json::to_string(&since).unwrap(),
        )
        .unwrap();
    assert_eq!(settle(&store, &e), swap::Settle::Settled);
    assert_eq!(stored_refresh(&store, a), "refresh-a2");
    let error = store.last_event_of(kind::ERROR).unwrap().unwrap();
    assert_eq!(error.reason.as_deref(), Some("login"));
    assert!(store.meta(swap::META_LOGIN_UNIDENTIFIED).unwrap().is_none());
}

/// With usage checks off, cc-rotate runs no Claude Code of its own: a changed login is stored as
/// the active account's at once, as before logins were identified.
#[test]
fn with_checks_off_a_changed_login_is_stored_at_once() {
    let e = env();
    let (store, a, _) = identified_pool(&e, "ua");
    store.set_setting("usage_checks", Some("false")).unwrap();
    profile_login(&e, "a2");
    statusline::sync(&store, &e.paths).unwrap();
    assert_eq!(stored_refresh(&store, a), "refresh-a2");
    assert!(e.seen().is_empty(), "Claude Code ran: {}", e.seen());
}

/// A `/login` with x, which `.claude.json` already names, lands while a swap checks the account it
/// is about to use: the swap does not store x's login as the outgoing account's, and leaves it
/// to be identified first.
#[test]
fn a_login_during_a_swaps_checks_is_not_stored_as_the_outgoing_accounts() {
    let e = env();
    let (store, a, _) = identified_pool(&e, "ux");
    // While Claude Code checks b, the profile gets x's login.
    let x_login = e.bin.join("x-login.json");
    std::fs::write(&x_login, json!({"claudeAiOauth": oauth("x1")}).to_string()).unwrap();
    let profile = e.paths.credentials_file();
    #[cfg(unix)]
    std::fs::write(
        e.bin.join("during.sh"),
        format!("cp '{}' '{}'\n", x_login.display(), profile.display()),
    )
    .unwrap();
    #[cfg(windows)]
    std::fs::write(
        e.bin.join("during.cmd"),
        format!(
            "@copy /y \"{}\" \"{}\" >nul\r\n",
            x_login.display(),
            profile.display()
        ),
    )
    .unwrap();
    e.answer("ub", 10.0);

    let out = swap::run(
        &store,
        &e.paths,
        &SwapRequest {
            from: Some(a),
            to: None,
            reason: Reason::FiveHour,
            wait: false,
        },
    )
    .unwrap();
    assert!(
        e.seen().contains("arg=/usage"),
        "b was checked before the swap"
    );
    assert!(matches!(out, SwapOutcome::Skipped { .. }), "{out:?}");
    assert_eq!(
        stored_refresh(&store, a),
        "refresh-a1",
        "x's login was stored as a's"
    );
    assert_eq!(store.active_account().unwrap().unwrap().id, a);
}

/// The app asks whether there is anything to identify every few seconds: the write-back's mark
/// answers that from the store, and settling clears it.
#[test]
fn the_write_back_marks_a_changed_login_for_settling() {
    let e = env();
    let (store, _, _) = identified_pool(&e, "ua");
    let now = time::now_ms();
    assert!(!statusline::needs_settling(&store, &e.paths, now).unwrap());

    profile_login(&e, "a2");
    statusline::sync(&store, &e.paths).unwrap();
    assert!(statusline::needs_settling(&store, &e.paths, now).unwrap());

    e.answer("ua", 10.0);
    assert_eq!(settle(&store, &e), swap::Settle::Settled);
    assert!(!statusline::needs_settling(&store, &e.paths, now + 61_000).unwrap());
}

/// When the active account has nothing to compare (no account id, no email), a login Claude
/// Code identifies as another pool account's still makes that account active.
#[test]
fn an_active_account_without_an_identity_does_not_take_another_accounts_login() {
    let e = env();
    let store = e.store();
    let a = store
        .insert_account("a", &AccountIdentity::default(), AccountState::Active)
        .unwrap();
    store.save_credential(a, &oauth("a1"), "login").unwrap();
    let b = add(&store, "b", AccountState::Parked);
    profile_login(&e, "a1");
    profile_config(&e, "ub", 1);
    statusline::sync(&store, &e.paths).unwrap();

    profile_login(&e, "b2");
    statusline::sync(&store, &e.paths).unwrap();
    e.answer("ub", 10.0);
    assert_eq!(settle(&store, &e), swap::Settle::Settled);
    assert_eq!(store.active_account().unwrap().unwrap().id, b);
    assert_eq!(stored_refresh(&store, a), "refresh-a1");
}
