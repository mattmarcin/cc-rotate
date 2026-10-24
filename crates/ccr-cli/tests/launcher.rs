//! The `cc-rotate` launcher against a stand-in `claude` that reports what it was given.

use std::path::Path;
use std::process::Command;

/// A fake `claude` on PATH that prints `CLAUDE_CONFIG_DIR` and its arguments, one per line,
/// and exits with 7.
fn fake_claude(bin: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = bin.join("claude");
        std::fs::write(
            &path,
            "#!/bin/sh\nprintf '%s\\n' \"$CLAUDE_CONFIG_DIR\" \"$@\"\nexit 7\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(windows)]
    std::fs::write(
        bin.join("claude.cmd"),
        "@echo off\r\necho %CLAUDE_CONFIG_DIR%\r\nfor %%a in (%*) do echo %%~a\r\nexit /b 7\r\n",
    )
    .unwrap();
}

#[test]
fn starts_claude_on_the_rotation_profile_with_every_argument() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    let profile = root.path().join("rotate profile");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&profile).unwrap();
    fake_claude(&bin);

    let path = std::env::join_paths(
        std::iter::once(bin.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_cc-rotate"))
        .args(["--resume", "abc-123", "--model", "opus"])
        .current_dir(root.path())
        .env("PATH", path)
        .env("CC_ROTATE_DATA_DIR", root.path().join("data"))
        .env("CC_ROTATE_PROFILE", &profile)
        .env_remove("CLAUDE_CONFIG_DIR")
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(7),
        "Claude Code's exit code is kept"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().map(str::trim).collect();
    assert_eq!(
        lines,
        vec![
            profile.display().to_string().as_str(),
            "--resume",
            "abc-123",
            "--model",
            "opus"
        ]
    );
    assert!(
        out.stderr.is_empty(),
        "no warning outside the home folder: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `dirs` reads the home folder from `HOME` on Unix, so it can be pointed at a temp folder.
#[cfg(unix)]
#[test]
fn warns_in_the_home_folder_and_still_starts_claude() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    fake_claude(&bin);
    let out = Command::new(env!("CARGO_BIN_EXE_cc-rotate"))
        .current_dir(root.path())
        .env("HOME", root.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("CC_ROTATE_DATA_DIR", root.path().join("data"))
        .env("CC_ROTATE_PROFILE", root.path().join("profile"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(7));
    assert!(String::from_utf8_lossy(&out.stderr).contains("home folder"));
}
