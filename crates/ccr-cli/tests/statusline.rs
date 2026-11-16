//! `cc-rotate-ctl statusline` as Claude Code runs it: the process it runs for comes from the
//! `CLAUDE_PID` Claude Code puts in its environment (spec F25).

mod common;

use std::io::Write;
use std::path::Path;
use std::process::Stdio;

use ccr_core::store::{AccountIdentity, AccountState, Store};
use ccr_core::time;
use serde_json::json;

/// A store with one account, `a`, in use, and its login in the profile. Usage checks are off and
/// find no Claude Code anyway.
fn fixture(root: &Path) -> (Store, i64) {
    let profile = root.join("profile");
    std::fs::create_dir_all(&profile).unwrap();
    let store = Store::open(&root.join("data").join("store.db")).unwrap();
    store.set_setting("usage_checks", Some("false")).unwrap();
    let nowhere = root.join("no-claude-here").display().to_string();
    store.set_setting("claude_path", Some(&nowhere)).unwrap();
    let now = time::now_ms();
    let id = store
        .insert_account(
            "a",
            &AccountIdentity {
                account_uuid: Some("ua".into()),
                ..Default::default()
            },
            AccountState::Active,
        )
        .unwrap();
    store
        .save_credential(
            id,
            &json!({"accessToken": "access-a", "refreshToken": "refresh-a",
                    "expiresAt": now + 3_600_000}),
            "login",
        )
        .unwrap();
    std::fs::write(
        profile.join(".credentials.json"),
        json!({"claudeAiOauth": {"accessToken": "access-a", "refreshToken": "refresh-a",
                                  "expiresAt": now + 3_600_000}})
        .to_string(),
    )
    .unwrap();
    (store, id)
}

fn tick(root: &Path, claude_pid: Option<&str>, total: f64, ran_ms: i64) {
    let mut command = common::program(env!("CARGO_BIN_EXE_cc-rotate-ctl"));
    command
        .arg("statusline")
        .env("CC_ROTATE_DATA_DIR", root.join("data"))
        .env("CC_ROTATE_PROFILE", root.join("profile"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CLAUDE_PID")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(pid) = claude_pid {
        command.env("CLAUDE_PID", pid);
    }
    let mut child = command.spawn().unwrap();
    let input = json!({
        "session_id": "s1",
        "version": "2.1.318",
        "cost": {"total_cost_usd": total, "total_duration_ms": ran_ms},
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
}

/// A status line run that read the session's numbers before the last one did, and finished
/// after it, reports less run time and an older total. With `CLAUDE_PID` it is known to be the
/// same process's and changes nothing; taken for a `/login` instead, the next tick would count
/// the spend between the two totals again.
#[test]
fn a_late_status_line_run_of_a_named_process_changes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let (store, a) = fixture(root.path());
    tick(root.path(), Some("4242"), 1.5, 5_000);
    tick(root.path(), Some("4242"), 1.0, 2_000);
    tick(root.path(), Some("4242"), 2.0, 8_000);
    let now_s = time::now_ms() / 1000;
    let counted = store
        .account_cost_between(a, now_s - 7_200, now_s + 7_200)
        .unwrap()
        .unwrap();
    assert!((counted - 2.0).abs() < 1e-9, "{counted}");
}
