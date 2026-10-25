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
    pub const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
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
        },
        "starting the background swap",
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

/// Start this executable in the background with the arguments `args` adds.
/// It must outlive the statusline or hook process that starts it, so it leaves Claude Code's job
/// object if allowed (Windows) or its process group (Unix), and a signal sent to that process's
/// group when Claude Code times it out does not reach it. It must not hold Claude Code's pipes
/// either: Claude Code sees the end of a statusline's or hook's output only once every copy of
/// the pipe is closed.
fn spawn_ctl(args: impl Fn(&mut Command), what: &str) -> Result<()> {
    let exe = std::env::current_exe()?;
    let build = || {
        let mut cmd = Command::new(&exe);
        args(&mut cmd);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _private = StdHandlesNotInherited::new();
        let base = flags::CREATE_NO_WINDOW | flags::CREATE_NEW_PROCESS_GROUP;
        if build()
            .creation_flags(base | flags::CREATE_BREAKAWAY_FROM_JOB)
            .spawn()
            .is_ok()
        {
            return Ok(());
        }
        build()
            .creation_flags(base)
            .spawn()
            .context(what.to_string())?;
    }
    #[cfg(unix)]
    {
        // The child's stdio is /dev/null and every other descriptor Rust opens is close-on-exec,
        // so nothing of the caller's pipes reaches it.
        use std::os::unix::process::CommandExt;
        build().process_group(0).spawn().context(what.to_string())?;
    }
    Ok(())
}

/// Windows starts children with every inheritable handle of the parent, whatever their stdio is
/// set to, and the standard handles this process got from Claude Code are inheritable. While one
/// of these exists, this process's standard handles are kept out of the children it starts.
#[cfg(windows)]
struct StdHandlesNotInherited {
    saved: Vec<(windows_sys::Win32::Foundation::HANDLE, u32)>,
}

#[cfg(windows)]
impl StdHandlesNotInherited {
    fn new() -> Self {
        use windows_sys::Win32::Foundation::{
            GetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
        };
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        };
        let mut saved = Vec::new();
        for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: plain Win32 calls on this process's own standard handles, which stay open.
            unsafe {
                let handle = GetStdHandle(id);
                if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                    continue;
                }
                let mut flags = 0u32;
                if GetHandleInformation(handle, &mut flags) == 0 || flags & HANDLE_FLAG_INHERIT == 0
                {
                    continue;
                }
                if SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) != 0 {
                    saved.push((handle, flags & HANDLE_FLAG_INHERIT));
                }
            }
        }
        Self { saved }
    }
}

#[cfg(windows)]
impl Drop for StdHandlesNotInherited {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        for &(handle, flags) in &self.saved {
            // SAFETY: restores the flag cleared in `new` on the same, still open handle.
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, flags) };
        }
    }
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
    let mut cmd = Command::new("cmd");
    cmd.raw_arg(format!("/D /S /C \"{command}\""));
    cmd.creation_flags(flags::CREATE_NO_WINDOW);
    cmd
}

#[cfg(unix)]
fn shell(command: &str) -> Command {
    let mut cmd = Command::new("sh");
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
            "{} not found; install the desktop app next to cc-rotate-ctl",
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

/// Append an error to `<data dir>/logs/cc-rotate.log`, keeping the file under 1 MB.
pub fn log_error(context: &str, error: &anyhow::Error) {
    let Ok(data) = ccr_core::Paths::default_data_dir() else {
        return;
    };
    let dir = data.join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("cc-rotate.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1_000_000) {
        let _ = std::fs::rename(&path, dir.join("cc-rotate.log.1"));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{} {context}: {error:#}", ccr_core::time::now_ms());
    }
}

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
