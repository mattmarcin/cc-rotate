//! `cc-rotate-ctl setup` as the installer runs it, on a fresh install and on an upgrade.

use std::path::Path;
use std::process::{Command, Output};

fn setup(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cc-rotate-ctl"))
        .arg("setup")
        .args(args)
        .arg("--from")
        .arg(root.join("template"))
        .env("CC_ROTATE_DATA_DIR", root.join("data"))
        .env("CC_ROTATE_PROFILE", root.join("profile"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .output()
        .unwrap()
}

/// The installer runs setup with --keep-existing on every upgrade: the first run creates the
/// profile, and later ones keep it and any change made in it, only bringing cc-rotate's own
/// hooks up to date.
#[test]
fn keep_existing_creates_the_profile_once_and_then_only_updates_its_hooks() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("template")).unwrap();
    std::fs::write(root.path().join("template/settings.json"), "{}").unwrap();

    let first = setup(root.path(), &["--keep-existing"]);
    assert!(first.status.success(), "{first:?}");
    let settings_file = root.path().join("profile/settings.json");
    // An edit of the user's, and the StopFailure hook as an older release wrote it.
    let ctl = "cc-rotate-ctl hook stop-failure";
    std::fs::write(
        &settings_file,
        serde_json::json!({
            "edited": true,
            "hooks": {"StopFailure": [{"hooks": [{"type": "command", "command": ctl, "timeout": 60}]}]}
        })
        .to_string(),
    )
    .unwrap();

    let again = setup(root.path(), &["--keep-existing"]);
    assert!(again.status.success(), "{again:?}");
    assert!(String::from_utf8_lossy(&again.stdout).contains("Updated cc-rotate's hooks"));
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_file).unwrap()).unwrap();
    assert_eq!(settings["edited"], true);
    let groups = settings["hooks"]["StopFailure"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{groups:?}");
    assert_eq!(groups[0]["matcher"], "rate_limit");
    assert_eq!(groups[0]["hooks"][0]["asyncRewake"], true);
    assert!(settings["hooks"]["SessionStart"].is_array());

    // Up to date: nothing changes.
    let text = std::fs::read_to_string(&settings_file).unwrap();
    let third = setup(root.path(), &["--keep-existing"]);
    assert!(String::from_utf8_lossy(&third.stdout).contains("Leaving it as it is"));
    assert_eq!(std::fs::read_to_string(&settings_file).unwrap(), text);

    // Without it, an existing profile is still an error.
    assert!(!setup(root.path(), &[]).status.success());
}
