//! `cc-rotate-ctl hook stop-failure` as Claude Code runs it at a usage limit: in the background,
//! waking the session (exit code 2, a message on stderr) once its swap lands.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use ccr_core::store::{AccountIdentity, AccountState, Sample, SampleWindow, Store, WaitingState};
use ccr_core::time;
use serde_json::json;

/// A store with `a` (active, at its 5-hour limit) and `b` (parked, with room), and a's login
/// in the profile. Usage checks are off and find no Claude Code anyway.
fn fixture(root: &Path) -> Store {
    let profile = root.join("profile");
    std::fs::create_dir_all(&profile).unwrap();
    let store = Store::open(&root.join("data").join("store.db")).unwrap();
    store.set_setting("usage_checks", Some("false")).unwrap();
    let nowhere = root.join("no-claude-here").display().to_string();
    store.set_setting("claude_path", Some(&nowhere)).unwrap();
    let now = time::now_ms();
    let mut ids = Vec::new();
    for (label, state, five) in [
        ("a", AccountState::Active, 100.0),
        ("b", AccountState::Parked, 10.0),
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
                        "expiresAt": now + 3_600_000}),
                "login",
            )
            .unwrap();
        store
            .record_sample(&Sample {
                ts: now - 1_000,
                account_id: id,
                profile: "rotate".into(),
                session_id: None,
                model: None,
                windows: vec![SampleWindow {
                    name: "five_hour".into(),
                    pct: five,
                    resets_at: Some(now / 1000 + 3_600 + id),
                }],
            })
            .unwrap();
        ids.push(id);
    }
    std::fs::write(
        profile.join(".credentials.json"),
        json!({"claudeAiOauth": {"accessToken": "access-a", "refreshToken": "refresh-a",
                                  "expiresAt": now + 3_600_000}})
        .to_string(),
    )
    .unwrap();
    store
}

fn hook(root: &Path, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cc-rotate-ctl"))
        .args(["hook", "stop-failure"])
        .env("CC_ROTATE_DATA_DIR", root.join("data"))
        .env("CC_ROTATE_PROFILE", root.join("profile"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if start.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            panic!("the hook did not finish: {:?}", child.wait_with_output());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn wakes_the_session_once_its_swap_lands() {
    let root = tempfile::tempdir().unwrap();
    let store = fixture(root.path());
    let out = hook(
        root.path(),
        r#"{"hook_event_name":"StopFailure","session_id":"s-1","error":"rate_limit","error_details":"429"}"#,
    );
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("switched this Claude Code profile to the account b"),
        "{stderr}"
    );

    let b = store.account_by_label("b").unwrap().unwrap();
    assert_eq!(b.state, AccountState::Active);
    let ws = store.waiting_session("s-1").unwrap().unwrap();
    assert_eq!(ws.state, WaitingState::Woken);
    assert_eq!(ws.woken_account_id, Some(b.id));
    let profile_login =
        std::fs::read_to_string(root.path().join("profile/.credentials.json")).unwrap();
    assert!(profile_login.contains("access-b"), "{profile_login}");
}

#[test]
fn other_errors_and_sessions_without_an_id_are_not_waited_on() {
    let root = tempfile::tempdir().unwrap();
    let store = fixture(root.path());
    let out = hook(
        root.path(),
        r#"{"hook_event_name":"StopFailure","session_id":"s-2","error":"overloaded"}"#,
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(store.waiting_session("s-2").unwrap().is_none());

    // Without a session id there is nothing to wake, but the swap is still asked for.
    let out = hook(
        root.path(),
        r#"{"hook_event_name":"StopFailure","error":"rate_limit"}"#,
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let start = Instant::now();
    while store.active_account().unwrap().map(|a| a.label) != Some("b".into()) {
        assert!(start.elapsed() < Duration::from_secs(30), "no swap");
        std::thread::sleep(Duration::from_millis(100));
    }
}
