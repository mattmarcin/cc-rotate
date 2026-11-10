//! `cc-rotate-ctl desktop-launch`, the launcher the desktop app runs (spec section 24.1), against
//! a program that prints the environment it was started with.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Output;

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
    let mut cmd = common::program(env!("CARGO_BIN_EXE_cc-rotate-ctl"));
    cmd.arg("desktop-launch")
        .arg("--")
        .args(program)
        .current_dir(root)
        .env("CC_ROTATE_DATA_DIR", root.join("data"))
        .env("CC_ROTATE_PROFILE", profile)
        .env("KEEP_ME", "kept as it was")
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL")
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
    let out = common::program(env!("CARGO_BIN_EXE_cc-rotate-ctl"))
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
    let mut launcher = common::program(env!("CARGO_BIN_EXE_cc-rotate-ctl"))
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
    let _ = std::process::Command::new("taskkill")
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
    let mut cmd = common::program(env!("CARGO_BIN_EXE_cc-rotate-ctl"));
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

/// Resuming a desktop session whose transcript only the app's profile holds (a forked session,
/// say) brings it onto the pool: otherwise it runs on that profile's own login, which no swap
/// reaches, and stops at that account's limit.
#[cfg(unix)]
#[test]
fn a_resumed_desktop_session_only_in_the_app_profile_runs_on_the_pool() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let folder = home
        .join(".claude")
        .join("projects")
        .join("-Users-me-project");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("fork-1.jsonl"),
        "{\"type\":\"user\",\"entrypoint\":\"claude-desktop\"}\n",
    )
    .unwrap();
    let profile = root.path().join("rotate profile");
    let mut cmd = common::program(env!("CARGO_BIN_EXE_cc-rotate-ctl"));
    cmd.args([
        "desktop-launch",
        "--",
        "/bin/sh",
        "-c",
        "/usr/bin/env",
        "sh",
    ])
    .args(["--resume", "fork-1"])
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
    assert_eq!(
        vars.get("CLAUDE_CONFIG_DIR").map(String::as_str),
        Some(profile.to_str().unwrap())
    );
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        std::fs::metadata(profile.join("projects/-Users-me-project/fork-1.jsonl"))
            .unwrap()
            .ino(),
        std::fs::metadata(folder.join("fork-1.jsonl"))
            .unwrap()
            .ino()
    );
}

/// A data folder whose store has relay mode on (home account 1), with `relay.json` when `config`.
///
/// `relay.json` names the port of the listener returned, which the test keeps open: the launcher
/// then finds something answering and starts no relay of its own.
fn relay_data(root: &Path, config: bool) -> Option<std::net::TcpListener> {
    let data = root.join("data");
    let store = ccr_core::Store::open(&data.join("store.db")).unwrap();
    ccr_core::relay::set_mode(&store, Some(1)).unwrap();
    if !config {
        return None;
    }
    let path = data.join("relay.json");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut cfg = ccr_core::relay::RelayConfig::create(&path).unwrap();
    cfg.port = listener.local_addr().unwrap().port();
    std::fs::write(&path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    Some(listener)
}

#[test]
fn relay_mode_gives_a_desktop_session_the_relay_variables() {
    let root = tempfile::tempdir().unwrap();
    let _relay = relay_data(root.path(), true);
    let profile = root.path().join("rotate profile");
    let out = launch(root.path(), profile.to_str().unwrap(), true, &print_env());
    assert!(out.status.success(), "{out:?}");
    let cfg = ccr_core::relay::RelayConfig::read(&root.path().join("data").join("relay.json"))
        .unwrap()
        .unwrap();
    let vars = parse(&out);
    assert_eq!(
        vars.get("ANTHROPIC_BASE_URL"),
        Some(&cfg.base_url()),
        "{vars:?}"
    );
    assert_eq!(
        vars.get("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL")
            .map(String::as_str),
        Some("1")
    );
    assert!(out.stderr.is_empty());
    // The port answered, so no relay was started (one would take `relay.lock`).
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!root.path().join("data").join("relay.lock").exists());
}

#[test]
fn relay_mode_leaves_other_sessions_alone() {
    let root = tempfile::tempdir().unwrap();
    let _relay = relay_data(root.path(), true);
    let profile = root.path().join("rotate profile");
    let out = launch(root.path(), profile.to_str().unwrap(), false, &print_env());
    assert!(!parse(&out).contains_key("ANTHROPIC_BASE_URL"));
}

#[test]
fn relay_mode_without_its_config_starts_without_the_relay_and_says_nothing() {
    let root = tempfile::tempdir().unwrap();
    relay_data(root.path(), false);
    let profile = root.path().join("rotate profile");
    let out = launch(root.path(), profile.to_str().unwrap(), true, &print_env());
    assert!(out.status.success(), "{out:?}");
    assert!(!parse(&out).contains_key("ANTHROPIC_BASE_URL"));
    assert!(out.stderr.is_empty());
    let log = std::fs::read_to_string(root.path().join("data/logs/cc-rotate.log")).unwrap();
    assert!(log.contains("the app account is on but"), "{log}");
    assert!(log.contains("relay.json does not exist"), "{log}");
}

/// A new desktop session's transcript, written once Claude Code has started, is linked into the
/// app's profile, so the app still finds it after it restarts.
#[cfg(unix)]
#[test]
fn a_new_sessions_transcript_is_linked_into_the_app_profile() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("rotate profile");
    let line = r#"{"type":"user","entrypoint":"claude-desktop"}"#;
    let script = format!(
        "sleep 1; d=\"$CLAUDE_CONFIG_DIR/projects/-Users-me-project\"; mkdir -p \"$d/new-id\"; \
         echo '{line}' > \"$d/new-id.jsonl\"; sleep 5"
    );
    let program = ["/bin/sh".to_string(), "-c".to_string(), script];
    let started = std::time::Instant::now();
    let out = launch(root.path(), profile.to_str().unwrap(), true, &program);
    assert!(out.status.success(), "{out:?}");
    // The launcher waited for nothing: Claude Code ran its course.
    assert!(started.elapsed() >= std::time::Duration::from_secs(6));

    let folder = root.path().join("app profile/projects/-Users-me-project");
    let pool = profile.join("projects/-Users-me-project");
    // A hard link: the app refuses a transcript that is a symlink.
    use std::os::unix::fs::MetadataExt;
    let link = std::fs::symlink_metadata(folder.join("new-id.jsonl")).unwrap();
    assert!(link.file_type().is_file());
    assert_eq!(
        link.ino(),
        std::fs::metadata(pool.join("new-id.jsonl")).unwrap().ino()
    );
    assert_eq!(
        std::fs::read_link(folder.join("new-id")).unwrap(),
        pool.join("new-id")
    );
}
