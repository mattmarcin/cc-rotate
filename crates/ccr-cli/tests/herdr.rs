//! The herdr hook and report against a stand-in `herdr` (selected with `HERDR_BIN_PATH`) that
//! records every call and answers `resume_not_accepted` a set number of times first.
//!
//! Every command clears the real `HERDR_*` variables, so running the tests inside a herdr pane
//! never reports anything to the herdr server that pane belongs to.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const SESSION: &str = "0b6f2c1e-6d0e-4b7a-9a52-1c3f4d5e6f70";
const PANE: &str = "w1:p2";

struct Fixture {
    root: tempfile::TempDir,
    herdr: PathBuf,
}

/// A fake `herdr` that appends `---`, each argument on its own line, and `===` to `calls.log`, and
/// fails with `$FAKE_HERDR_ERROR` when set, or with `resume_not_accepted` for the first
/// `$FAKE_HERDR_REJECT` calls, the way the real CLI reports errors (JSON on stderr, exit 1).
fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    #[cfg(unix)]
    let herdr = {
        use std::os::unix::fs::PermissionsExt;
        let path = bin.join("herdr");
        std::fs::write(
            &path,
            r#"#!/bin/sh
dir=$(dirname "$0")
n=$(cat "$dir/count" 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" > "$dir/count"
{ echo ---; printf '%s\n' "$@"; echo ===; } >> "$dir/calls.log"
if [ -n "$FAKE_HERDR_ERROR" ]; then
  echo "{\"id\":\"cli:request\",\"error\":{\"code\":\"$FAKE_HERDR_ERROR\",\"message\":\"fake\"}}" >&2
  exit 1
fi
if [ "$n" -le "${FAKE_HERDR_REJECT:-0}" ]; then
  echo '{"id":"cli:request","error":{"code":"resume_not_accepted","message":"fake"}}' >&2
  exit 1
fi
exit 0
"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    #[cfg(windows)]
    let herdr = {
        let path = bin.join("herdr.cmd");
        std::fs::write(
            &path,
            [
                "@echo off",
                "setlocal EnableDelayedExpansion",
                "set n=0",
                "if exist \"%~dp0count\" set /p n=<\"%~dp0count\"",
                "set /a n=n+1",
                ">\"%~dp0count\" echo !n!",
                ">>\"%~dp0calls.log\" echo ---",
                "for %%a in (%*) do >>\"%~dp0calls.log\" echo %%~a",
                ">>\"%~dp0calls.log\" echo ===",
                "if defined FAKE_HERDR_ERROR (",
                "  >&2 echo {\"id\":\"cli:request\",\"error\":{\"code\":\"!FAKE_HERDR_ERROR!\",\"message\":\"fake\"}}",
                "  exit /b 1",
                ")",
                "if not defined FAKE_HERDR_REJECT set FAKE_HERDR_REJECT=0",
                "if !n! LEQ !FAKE_HERDR_REJECT! (",
                "  >&2 echo {\"id\":\"cli:request\",\"error\":{\"code\":\"resume_not_accepted\",\"message\":\"fake\"}}",
                "  exit /b 1",
                ")",
                "exit /b 0",
                "",
            ]
            .join("\r\n"),
        )
        .unwrap();
        path
    };
    Fixture { root, herdr }
}

impl Fixture {
    /// `cc-rotate-ctl` with the herdr environment cleared and cc-rotate's data in the fixture.
    fn ctl(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cc-rotate-ctl"));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("HERDR_") {
                cmd.env_remove(&name);
            }
        }
        cmd.current_dir(self.root.path())
            .env("CC_ROTATE_DATA_DIR", self.root.path().join("data"))
            .env("CC_ROTATE_PROFILE", self.root.path().join("profile"))
            .env("HERDR_BIN_PATH", &self.herdr)
            .env_remove("FAKE_HERDR_ERROR")
            .env_remove("FAKE_HERDR_REJECT");
        cmd
    }

    /// `ctl` as Claude Code runs a hook in a herdr pane.
    fn hook(&self) -> Command {
        let mut cmd = self.ctl();
        cmd.args(["hook", "session-start"])
            .env("HERDR_ENV", "1")
            .env("HERDR_PANE_ID", PANE);
        cmd
    }

    /// Every call the fake has finished recording, as its argument lists.
    fn calls(&self) -> Vec<Vec<String>> {
        let Ok(log) = std::fs::read_to_string(self.herdr.with_file_name("calls.log")) else {
            return Vec::new();
        };
        let mut calls = Vec::new();
        let mut current: Option<Vec<String>> = None;
        for line in log.lines().map(str::trim_end) {
            match line {
                "---" => current = Some(Vec::new()),
                "===" => calls.extend(current.take()),
                arg => {
                    if let Some(call) = current.as_mut() {
                        call.push(arg.to_string());
                    }
                }
            }
        }
        calls
    }

    fn wait_for_calls(&self, n: usize) -> Vec<Vec<String>> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let calls = self.calls();
            if calls.len() >= n || Instant::now() > deadline {
                return calls;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.root.path().join("data/logs/cc-rotate.log"))
            .unwrap_or_default()
    }
}

fn expected_call(seq: &str) -> Vec<String> {
    [
        "pane",
        "report-agent-session",
        PANE,
        "--source",
        "cc-rotate",
        "--agent",
        "claude",
        "--seq",
        seq,
        "--",
        "cc-rotate",
        "--resume",
        SESSION,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn run_with_stdin(mut cmd: Command, stdin: &str) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The hook may exit without reading its input, as it does outside herdr.
    let _ = child.stdin.take().unwrap().write_all(stdin.as_bytes());
    child.wait_with_output().unwrap()
}

fn hook_input(extra: &str) -> String {
    format!(
        r#"{{"session_id":"{SESSION}","transcript_path":"/x/{SESSION}.jsonl","cwd":"/x","hook_event_name":"SessionStart","source":"startup"{extra}}}"#
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn report_retries_until_herdr_accepts() {
    let f = fixture();
    let started = Instant::now();
    let out = f
        .ctl()
        .args([
            "herdr-report",
            "--seq",
            "1761300000123",
            "--",
            PANE,
            SESSION,
        ])
        .env("FAKE_HERDR_REJECT", "2")
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    assert!(out.status.success(), "log: {}", f.log());
    let calls = f.calls();
    assert_eq!(calls.len(), 3, "two refusals, then accepted: {calls:?}");
    for call in &calls {
        assert_eq!(call, &expected_call("1761300000123"));
    }
    assert!(
        elapsed >= Duration::from_millis(1000),
        "waits 500 ms between attempts: {elapsed:?}"
    );
    assert_eq!(f.log(), "", "nothing logged on success");
}

#[test]
fn report_gives_up_after_the_timeout_and_logs_it() {
    let f = fixture();
    // Long enough for a retry even when starting the stand-in is slow (a loaded CI runner can
    // take a second to start cmd.exe); what matters is that it retries, then stops.
    let started = Instant::now();
    let out = f
        .ctl()
        .args(["herdr-report", "--seq", "7", "--timeout-ms", "4000", "--"])
        .args([PANE, SESSION])
        .env("FAKE_HERDR_REJECT", "1000")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "gave up after {:?}",
        started.elapsed()
    );
    let calls = f.calls();
    // A call at most every 500 ms within the 4 s.
    assert!((2..=9).contains(&calls.len()), "{calls:?}");
    let log = f.log();
    assert!(log.contains("herdr report"), "{log}");
    assert!(log.contains("resume_not_accepted"), "{log}");
}

#[test]
fn other_herdr_errors_are_logged_without_retrying() {
    let f = fixture();
    let out = f
        .ctl()
        .args(["herdr-report", "--seq", "7", "--", PANE, SESSION])
        .env("FAKE_HERDR_ERROR", "pane_not_found")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(f.calls().len(), 1);
    assert!(f.log().contains("pane_not_found"), "{}", f.log());
}

#[test]
fn hook_reports_in_the_background_and_returns_before_it() {
    let f = fixture();
    let before = now_ms();
    let mut hook = f.hook();
    hook.env("FAKE_HERDR_REJECT", "4");
    let started = Instant::now();
    let out = run_with_stdin(hook, &hook_input(""));
    let elapsed = started.elapsed();
    let after = now_ms();
    // The report takes at least two seconds here (four refusals 500 ms apart). The hook must not
    // wait for it, not even through an inherited stdout or stderr handle, which would make the
    // read of its output above end only after the last call.
    let done_when_hook_returned = f.calls().len();
    assert!(
        done_when_hook_returned < 5,
        "the hook waited for the report ({elapsed:?})"
    );

    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "Claude Code would add hook output to the session: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let calls = f.wait_for_calls(5);
    assert_eq!(calls.len(), 5, "log: {}", f.log());
    let seq: u64 = calls[0][8].parse().expect("--seq is a number");
    assert!(
        (before..=after).contains(&seq),
        "--seq is the hook's time in ms"
    );
    for call in &calls {
        assert_eq!(call, &expected_call(&seq.to_string()));
    }
    assert_eq!(f.log(), "");
}

#[test]
fn hook_does_nothing_outside_herdr_or_for_subagents() {
    let f = fixture();

    // Outside a herdr pane (HERDR_BIN_PATH alone is not enough).
    let mut outside = f.ctl();
    outside.args(["hook", "session-start"]);
    let out = run_with_stdin(outside, &hook_input(""));
    assert!(out.status.success());
    // A subagent of a session in a herdr pane.
    let out = run_with_stdin(
        f.hook(),
        &hook_input(r#","agent_id":"a1","agent_type":"Explore""#),
    );
    assert!(out.status.success());
    // A session id herdr could not type back is refused and logged, and still exits 0.
    let out = run_with_stdin(
        f.hook(),
        r#"{"session_id":"x; rm -rf ~","hook_event_name":"SessionStart"}"#,
    );
    assert!(out.status.success());
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
    assert!(f.log().contains("not a plain token"), "{}", f.log());

    // Any report the hooks above started by mistake would have arrived by now.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(f.calls(), Vec::<Vec<String>>::new());
}

/// The fake itself, so a broken fake cannot make the tests above pass.
#[test]
fn fake_herdr_behaves() {
    let f = fixture();
    let run = |f: &Fixture| {
        Command::new(&f.herdr)
            .args(["a", "w1:p2", "--"])
            .env("FAKE_HERDR_REJECT", "1")
            .env_remove("FAKE_HERDR_ERROR")
            .output()
            .unwrap()
    };
    let first = run(&f);
    assert!(!first.status.success());
    assert!(String::from_utf8_lossy(&first.stderr).contains("resume_not_accepted"));
    assert!(run(&f).status.success());
    assert_eq!(f.calls(), vec![vec!["a", "w1:p2", "--"]; 2]);
    assert!(Path::new(&f.herdr).is_file());
}
