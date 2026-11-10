//! Child processes: the detached swap, the chained statusline, the desktop app, and logging.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ccr_core::swap::SwapRequest;

#[cfg(windows)]
mod flags {
    pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    pub const DETACHED_PROCESS: u32 = 0x0000_0008;
}

/// Start `cc-rotate-ctl swap` in the background so the statusline returns immediately.
pub fn spawn_swap(req: &SwapRequest, profile: Option<&Path>) -> Result<()> {
    spawn_ctl(
        |cmd| {
            if let Some(p) = profile {
                cmd.arg("--profile").arg(p);
            }
            cmd.args([
                "swap",
                "--no-wait",
                "--quiet",
                "--reason",
                req.reason.as_str(),
            ]);
            if let Some(from) = req.from {
                cmd.args(["--from-id", &from.to_string()]);
            }
            if let Some(to) = req.to {
                cmd.args(["--to-id", &to.to_string()]);
            }
        },
        "starting the background swap",
    )
}

/// Start `cc-rotate-ctl settle-login` in the background: identifying the profile's login runs
/// Claude Code, which takes seconds the statusline does not have.
pub fn spawn_settle_login(profile: Option<&Path>) -> Result<()> {
    spawn_ctl(
        |cmd| {
            if let Some(p) = profile {
                cmd.arg("--profile").arg(p);
            }
            cmd.arg("settle-login");
        },
        "starting the background identification of the profile's login",
    )
}

/// Start `cc-rotate-ctl confirm-active` in the background: a usage check of the account in use,
/// which takes a few seconds the statusline does not have.
pub fn spawn_confirm(profile: Option<&Path>) -> Result<()> {
    spawn_ctl(
        |cmd| {
            if let Some(p) = profile {
                cmd.arg("--profile").arg(p);
            }
            cmd.arg("confirm-active");
        },
        "starting the background usage check",
    )
}

/// Start `cc-rotate-ctl herdr-report` in the background so the SessionStart hook returns at once
/// while the report waits for herdr to see Claude Code running.
pub fn spawn_herdr_report(pane_id: &str, session_id: &str, seq: u64) -> Result<()> {
    spawn_ctl(
        |cmd| {
            cmd.args(["herdr-report", "--seq", &seq.to_string(), "--"])
                .args([pane_id, session_id]);
        },
        "starting the background herdr report",
    )
}

/// Start `cc-rotate-ctl desktop-link-transcripts` in the background for a desktop session on
/// the pool, so the launcher starts Claude Code at once.
pub fn spawn_link_transcripts(
    rotate: &std::path::Path,
    app_profile: &std::path::Path,
    watch_pid: Option<u32>,
) -> Result<()> {
    spawn_ctl(
        |cmd| {
            cmd.arg("--profile")
                .arg(rotate)
                .arg("desktop-link-transcripts")
                .arg("--app-profile")
                .arg(app_profile);
            if let Some(pid) = watch_pid {
                cmd.args(["--watch-pid", &pid.to_string()]);
            }
        },
        "starting the background transcript links",
    )
}

/// Start this executable in the background with the arguments `args` adds.
/// See `ccr_core::process::spawn_detached` for how it is kept apart from the caller.
fn spawn_ctl(args: impl Fn(&mut Command), what: &str) -> Result<()> {
    ccr_core::process::spawn_detached(&std::env::current_exe()?, args, what)
}

/// Run the user's original statusline command with the same stdin and return its stdout.
pub fn run_downstream(command: &str, stdin: &str) -> Option<String> {
    let mut cmd = shell(command);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    if let Some(mut pipe) = child.stdin.take() {
        let _ = pipe.write_all(stdin.as_bytes());
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5))
            }
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(windows)]
fn shell(command: &str) -> Command {
    use std::os::windows::process::CommandExt;
    // By full path: a bare `cmd` is looked up next to this executable first.
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let mut cmd = Command::new(std::path::Path::new(&root).join(r"System32\cmd.exe"));
    cmd.raw_arg(format!("/D /S /C \"{command}\""));
    cmd.creation_flags(flags::CREATE_NO_WINDOW);
    cmd
}

#[cfg(unix)]
fn shell(command: &str) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", command]);
    cmd
}

/// Launch the desktop app that ships next to this executable.
pub fn launch_app() -> Result<()> {
    let exe = std::env::current_exe()?;
    let name = if cfg!(windows) {
        "cc-rotate-app.exe"
    } else {
        "cc-rotate-app"
    };
    let app = exe.with_file_name(name);
    if !app.is_file() {
        bail!(
            "{} not found. Install the desktop app next to cc-rotate-ctl.",
            app.display()
        );
    }
    let mut cmd = Command::new(&app);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(flags::DETACHED_PROCESS | flags::CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
        .with_context(|| format!("starting {}", app.display()))?;
    Ok(())
}

pub use ccr_core::process::log_error;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downstream_gets_the_statusline_input() {
        let command = if cfg!(windows) {
            "findstr session"
        } else {
            "grep session"
        };
        let out = run_downstream(command, "{\"session_id\":\"s\"}\n").unwrap();
        assert_eq!(out.trim(), "{\"session_id\":\"s\"}");
    }

    #[cfg(unix)]
    #[test]
    fn downstream_runs_through_sh() {
        let out = run_downstream("read line; echo \"got $line\" | tr a-z A-Z", "ok\n").unwrap();
        assert_eq!(out, "GOT OK\n");
    }
}
