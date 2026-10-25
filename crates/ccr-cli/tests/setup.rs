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
/// profile, and later ones leave it, and any change made in it, as it is.
#[test]
fn keep_existing_creates_the_profile_once_and_then_leaves_it() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("template")).unwrap();
    std::fs::write(root.path().join("template/settings.json"), "{}").unwrap();

    let first = setup(root.path(), &["--keep-existing"]);
    assert!(first.status.success(), "{first:?}");
    let settings = root.path().join("profile/settings.json");
    std::fs::write(&settings, r#"{"edited": true}"#).unwrap();

    let again = setup(root.path(), &["--keep-existing"]);
    assert!(again.status.success(), "{again:?}");
    assert!(String::from_utf8_lossy(&again.stdout).contains("already set up"));
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        r#"{"edited": true}"#
    );

    // Without it, an existing profile is still an error.
    assert!(!setup(root.path(), &[]).status.success());
}
