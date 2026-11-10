//! `cc-rotate-ctl desktop-launch`, the launcher the desktop app runs (spec section 24.1), against
//! a program that prints the environment it was started with.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};

const REMOVED: [&str; 6] = [
    "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
    "CLAUDE_CODE_HOST_AUTH_ENV_VAR",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "CLAUDE_CODE_DISABLE_FAST_MODE",
];

/// The program and arguments that print the environment, one `NAME=value` per line.
fn print_env() -> Vec<String> {
    if cfg!(windows) {
        vec!["cmd.exe".into(), "/d".into(), "/c".into(), "set".into()]
    } else {
        vec!["/usr/bin/env".into()]
    }
}

/// The variables in `env`'s output, by upper-cased name (Windows names ignore case).
fn parse(out: &Output) -> BTreeMap<String, String> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim_end_matches('\r').split_once('='))
        .map(|(k, v)| (k.to_uppercase(), v.to_string()))
        .collect()
}

fn launch(root: &Path, profile: &str, desktop: bool, program: &[String]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cc-rotate-ctl"));
    cmd.arg("desktop-launch")
        .arg("--")
        .args(program)
        .current_dir(root)
        .env("CC_ROTATE_DATA_DIR", root.join("data"))
        .env("CC_ROTATE_PROFILE", profile)
        .env("KEEP_ME", "kept as it was")
        .env("CLAUDE_CONFIG_DIR", root.join("app profile"));
    for name in REMOVED {
        cmd.env(name, "1");
    }
    if desktop {
        cmd.env("CLAUDE_CODE_ENTRYPOINT", "claude-desktop");
    } else {
        cmd.env("CLAUDE_CODE_ENTRYPOINT", "cli");
    }
    cmd.output().unwrap()
}

#[test]
fn a_desktop_session_runs_on_the_rotation_profile() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("rotate profile");
    let out = launch(root.path(), profile.to_str().unwrap(), true, &print_env());
    assert!(out.status.success(), "{out:?}");
    let vars = parse(&out);
    assert_eq!(
        vars.get("CLAUDE_CONFIG_DIR").map(String::as_str),
        profile.to_str()
    );
    assert_eq!(
        vars.get("KEEP_ME").map(String::as_str),
        Some("kept as it was")
    );
    for name in REMOVED {
        assert!(!vars.contains_key(name), "{name} should be removed");
    }
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn anything_else_starts_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("rotate profile");
    let out = launch(root.path(), profile.to_str().unwrap(), false, &print_env());
    assert!(out.status.success(), "{out:?}");
    let vars = parse(&out);
    let app = root.path().join("app profile");
    assert_eq!(
        vars.get("CLAUDE_CONFIG_DIR").map(String::as_str),
        app.to_str()
    );
    for name in REMOVED {
        assert_eq!(vars.get(name).map(String::as_str), Some("1"), "{name}");
    }
}

#[test]
fn a_profile_that_cannot_be_resolved_starts_claude_code_unchanged() {
    let root = tempfile::tempdir().unwrap();
    // A relative profile_dir in the store is refused; so is an unreadable store. Either way the
    // session starts, and fails visibly as not logged in, instead of not starting at all.
    std::fs::create_dir_all(root.path().join("data")).unwrap();
    std::fs::write(root.path().join("data").join("store.db"), "not a database").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_cc-rotate-ctl"))
        .arg("desktop-launch")
        .arg("--")
        .args(print_env())
        .current_dir(root.path())
        .env("CC_ROTATE_DATA_DIR", root.path().join("data"))
        .env_remove("CC_ROTATE_PROFILE")
        .env("CLAUDE_CODE_ENTRYPOINT", "claude-desktop")
        .env("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1")
        .env_remove("CLAUDE_CONFIG_DIR")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let vars = parse(&out);
    assert!(!vars.contains_key("CLAUDE_CONFIG_DIR"));
    assert_eq!(
        vars.get("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST")
            .map(String::as_str),
        Some("1")
    );
    assert!(!out.stdout.is_empty() && out.stderr.is_empty());
}

#[test]
fn the_exit_code_is_claude_codes() {
    let root = tempfile::tempdir().unwrap();
    let program: Vec<String> = if cfg!(windows) {
        ["cmd.exe", "/d", "/c", "exit 7"].map(String::from).to_vec()
    } else {
        ["/bin/sh", "-c", "exit 7"].map(String::from).to_vec()
    };
    let profile = root.path().join("p");
    let out = launch(root.path(), profile.to_str().unwrap(), true, &program);
    assert_eq!(out.status.code(), Some(7));
}

#[test]
fn arguments_with_spaces_and_quotes_pass_through() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("p");
    let tricky = r#"a "quoted" arg with spaces"#;
    let program: Vec<String> = if cfg!(windows) {
        // powershell prints its argument as it received it.
        vec![
            "powershell.exe".into(),
            "-NoProfile".into(),
            "-Command".into(),
            "Write-Output $args[0]".into(),
            tricky.into(),
        ]
    } else {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '%s' \"$1\"".into(),
            "sh".into(),
            tricky.into(),
        ]
    };
    let out = launch(root.path(), profile.to_str().unwrap(), true, &program);
    if cfg!(windows) {
        // PowerShell's own argument parsing drops the inner quotes; what matters is that the
        // launcher passed one argument.
        assert!(String::from_utf8_lossy(&out.stdout).contains("arg with spaces"));
    } else {
        assert_eq!(String::from_utf8_lossy(&out.stdout), tricky);
    }
}

/// Windows has no exec: the launcher waits for Claude Code, and ending the launcher (as the app
/// does to stop a session) ends Claude Code too.
#[cfg(windows)]
#[test]
fn ending_the_launcher_ends_claude_code() {
    use std::time::{Duration, Instant};
    let root = tempfile::tempdir().unwrap();
    let pid_file = root.path().join("pid");
    let started = root.path().join("started");
    // The child records its own id, starts one process of its own, and records that one's.
    let script = format!(
        "$p = Start-Process -PassThru -WindowStyle Hidden ping.exe -ArgumentList '-n','60','127.0.0.1'; $p.Id | Out-File -Encoding ascii '{}'; $PID | Out-File -Encoding ascii '{}'; Start-Sleep 60",
        started.display(),
        pid_file.display()
    );
    let mut launcher = Command::new(env!("CARGO_BIN_EXE_cc-rotate-ctl"))
        .args([
            "desktop-launch",
            "--",
            "powershell.exe",
            "-NoProfile",
            "-Command",
        ])
        .arg(&script)
        .env("CC_ROTATE_DATA_DIR", root.path().join("data"))
        .env("CC_ROTATE_PROFILE", root.path().join("p"))
        .env("CLAUDE_CODE_ENTRYPOINT", "claude-desktop")
        .env("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let pid = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|t| t.trim().parse::<u32>().ok())
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "the child never started");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(ccr_core::process::alive(pid));
    launcher.kill().unwrap();
    let _ = launcher.wait();
    let deadline = Instant::now() + Duration::from_secs(5);
    while ccr_core::process::alive(pid) {
        assert!(
            Instant::now() < deadline,
            "Claude Code outlived the launcher"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // What Claude Code started left the job, as it would outlive an exec on Unix.
    let own: u32 = std::fs::read_to_string(&started)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(
        ccr_core::process::alive(own),
        "a process Claude Code started was ended with the launcher"
    );
    let _ = Command::new("taskkill")
        .args(["/F", "/PID", &own.to_string()])
        .output();
}

/// Resuming a session that lives in the app's default profile (`~/.claude`, no
/// `CLAUDE_CONFIG_DIR`) keeps the variable unset: on macOS Claude Code names its Keychain item
/// after its value, so `CLAUDE_CONFIG_DIR=~/.claude` would miss that profile's login.
#[cfg(unix)]
#[test]
fn a_resumed_older_session_keeps_the_apps_config_dir_unset() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let folder = home
        .join(".claude")
        .join("projects")
        .join("-Users-me-project");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("abc-123.jsonl"), "{}\n").unwrap();
    let profile = root.path().join("rotate profile");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cc-rotate-ctl"));
    cmd.args([
        "desktop-launch",
        "--",
        "/bin/sh",
        "-c",
        "/usr/bin/env",
        "sh",
    ])
    .args(["--resume", "abc-123"])
    .current_dir(root.path())
    .env("HOME", &home)
    .env("CC_ROTATE_DATA_DIR", root.path().join("data"))
    .env("CC_ROTATE_PROFILE", &profile)
    .env_remove("CLAUDE_CONFIG_DIR")
    .env("CLAUDE_CODE_ENTRYPOINT", "claude-desktop");
    for name in REMOVED {
        cmd.env(name, "1");
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let vars = parse(&out);
    assert!(
        !vars.contains_key("CLAUDE_CONFIG_DIR"),
        "CLAUDE_CONFIG_DIR should stay unset, got {:?}",
        vars.get("CLAUDE_CONFIG_DIR")
    );
    for name in REMOVED {
        assert!(!vars.contains_key(name), "{name} should be removed");
    }
}
