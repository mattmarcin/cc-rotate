//! `cc-rotate`: start Claude Code on the account pool.
//!
//! Sets `CLAUDE_CONFIG_DIR` to the rotation profile for the child only and passes every
//! argument through, so `cc-rotate --resume <id>` works like `claude --resume <id>`. It is a
//! real executable rather than a shell function so it works the same in every shell, including
//! tools that start sessions by typing a command into a fresh shell.

use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Duration;

use anyhow::{Context, Result};
use ccr_core::login;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("cc-rotate: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let (store, paths) = ccr_core::open(None)?;
    let claude = login::claude_executable(&store)?;
    drop(store);
    // Asked briefly: in a desktop session a locked Keychain shows a dialog, and Claude Code will
    // ask again anyway.
    let locked = || {
        paths
            .credentials()
            .keychain_locked(Duration::from_secs(3))
            .unwrap_or(false)
    };
    if needs_login() && locked() {
        let herdr = ccr_core::herdr::pane_from_env().is_some();
        anyhow::bail!(
            "{}{}",
            ccr_core::doctor::KEYCHAIN_LOCKED,
            if herdr {
                " This session runs in herdr: start `herdr server` from the desktop session, for example as a launch agent (see \"herdr on macOS\" in the README)."
            } else {
                ""
            }
        );
    }
    warn_if_home();

    let mut cmd = ccr_core::paths::program_command(&claude);
    cmd.args(std::env::args_os().skip(1))
        .env("CLAUDE_CONFIG_DIR", &paths.profile_dir);
    run_claude(cmd, &claude)
}

/// Whether Claude Code needs the login for these arguments: everything but printing its version
/// or help and the commands that manage Claude Code itself.
fn needs_login() -> bool {
    const WITHOUT_LOGIN: &[&str] = &[
        "--version",
        "-v",
        "--help",
        "-h",
        "update",
        "doctor",
        "mcp",
        "config",
        "install",
    ];
    let first = std::env::args_os().nth(1);
    !first.is_some_and(|a| WITHOUT_LOGIN.iter().any(|w| a == *w))
}

/// Claude Code does not run status lines in the home folder (trust there is per-run only), so
/// cc-rotate cannot see usage or swap accounts for a session started there.
fn warn_if_home() {
    let canonical = |p: &Path| dunce::canonicalize(p).ok();
    let home = ccr_core::paths::home_dir().ok();
    let cwd = std::env::current_dir().ok();
    if let (Some(home), Some(cwd)) = (home, cwd)
        && canonical(&home).is_some()
        && canonical(&home) == canonical(&cwd)
    {
        eprintln!(
            "warning: cc-rotate can't track usage or swap accounts in your home folder, because Claude Code doesn't run status lines there. Start it in a project folder."
        );
    }
}

/// Replace this process with Claude Code, so signals and the exit code are Claude Code's own.
#[cfg(unix)]
fn run_claude(mut cmd: Command, claude: &Path) -> Result<ExitCode> {
    use std::os::unix::process::CommandExt;
    let err = cmd.exec();
    Err(err).with_context(|| format!("starting {}", claude.display()))
}

/// Windows has no exec: wait for Claude Code and exit with its code. Ctrl+C and Ctrl+Break go
/// to every process on the console; this one ignores them so it keeps waiting while Claude
/// Code handles them.
#[cfg(windows)]
fn run_claude(mut cmd: Command, claude: &Path) -> Result<ExitCode> {
    use windows_sys::Win32::Foundation::TRUE;
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    use windows_sys::core::BOOL;

    unsafe extern "system" fn ignore(_ctrl_type: u32) -> BOOL {
        TRUE
    }
    // SAFETY: registers a handler that only returns TRUE. A handler, unlike
    // `SetConsoleCtrlHandler(None, TRUE)`, is not inherited, so Claude Code still sees Ctrl+C.
    unsafe { SetConsoleCtrlHandler(Some(ignore), TRUE) };

    let status = cmd
        .status()
        .with_context(|| format!("starting {}", claude.display()))?;
    std::process::exit(status.code().unwrap_or(1));
}
