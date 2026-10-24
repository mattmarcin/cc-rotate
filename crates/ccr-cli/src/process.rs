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
/// It must outlive the statusline process, so it leaves Claude Code's job object if allowed.
pub fn spawn_swap(req: &SwapRequest, profile: Option<&Path>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let build = || {
        let mut cmd = Command::new(&exe);
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
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
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
            .context("starting the background swap")?;
    }
    #[cfg(not(windows))]
    {
        build().spawn().context("starting the background swap")?;
    }
    Ok(())
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

#[cfg(not(windows))]
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
