//! `cc-rotate-ctl desktop-launch -- <claude> <arguments>` (spec section 24.1): the launcher the
//! Claude desktop app runs for each Code session once Claude Code's managed settings name it.
//!
//! A desktop session (`ccr_core::desktop::is_desktop_session`) is moved to the cc-rotate profile
//! with the app's host marks removed, so it runs on the profile's login; anything else the app
//! starts through the launcher is started as it is. Claude Code's launcher rules apply: nothing
//! is written to the terminal or standard output, no input is read, and Claude Code starts at
//! once. Errors go to cc-rotate's log only.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use ccr_core::desktop;

use crate::process;

pub fn run(profile: Option<PathBuf>, command: Vec<OsString>) -> ExitCode {
    let Some((program, args)) = command.split_first() else {
        return ExitCode::FAILURE;
    };
    let mut cmd = Command::new(program);
    cmd.args(args);
    let var = |k: &str| std::env::var_os(k);
    if desktop::is_desktop_session(var) {
        // A profile that can't be resolved leaves the session as the app started it: Claude Code
        // then says it is not logged in, rather than running on an account nobody chose.
        let resolved = ccr_core::resolve_profile_read_only(profile)
            .and_then(|rotate| Ok((rotate, ccr_core::paths::home_dir()?)));
        match resolved {
            Ok((rotate, home)) => {
                let plan = desktop::plan_launch(args, var, &rotate, &home);
                for name in &plan.remove {
                    cmd.env_remove(name);
                }
                if let Some(dir) = &plan.config_dir {
                    cmd.env("CLAUDE_CONFIG_DIR", dir);
                }
            }
            Err(e) => process::log_error("desktop-launch", &e),
        }
    }
    start(cmd)
}

/// Replace this process with Claude Code, so signals and the exit code are its own.
#[cfg(unix)]
fn start(mut cmd: Command) -> ExitCode {
    use std::os::unix::process::CommandExt;
    let err = cmd.exec();
    process::log_error(
        "desktop-launch",
        &anyhow::Error::new(err).context(format!("starting {:?}", cmd.get_program())),
    );
    ExitCode::from(127)
}

/// Windows has no exec: start Claude Code with this process's standard handles, the pipes the app
/// reads and writes, and wait for it. It runs in a job that ends it when this process ends, so a
/// session the app stops by ending the launcher does not keep running; what Claude Code starts
/// leaves the job, as it would outlive an `exec`ing launcher. Ctrl+C and Ctrl+Break are left to
/// Claude Code.
#[cfg(windows)]
fn start(mut cmd: Command) -> ExitCode {
    use windows_sys::Win32::Foundation::TRUE;
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    use windows_sys::core::BOOL;

    unsafe extern "system" fn ignore(_ctrl_type: u32) -> BOOL {
        TRUE
    }
    // SAFETY: registers a handler that only returns TRUE; unlike `SetConsoleCtrlHandler(None,
    // TRUE)` it is not inherited, so Claude Code still sees Ctrl+C.
    unsafe { SetConsoleCtrlHandler(Some(ignore), TRUE) };

    let mut tree = match ccr_core::process::Tree::spawn_child_only(&mut cmd) {
        Ok(tree) => tree,
        Err(e) => {
            process::log_error(
                "desktop-launch",
                &anyhow::Error::new(e).context(format!("starting {:?}", cmd.get_program())),
            );
            return ExitCode::from(127);
        }
    };
    let code = match tree.child_mut().wait() {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => 1,
    };
    drop(tree);
    // Exit codes on Windows are 32-bit; `ExitCode` takes only a byte.
    std::process::exit(code);
}
