//! `cc-rotate-ctl desktop-launch -- <claude> <arguments>` (spec section 24.1): the launcher the
//! Claude desktop app runs for each Code session once Claude Code's managed settings name it.
//!
//! A desktop session (`ccr_core::desktop::is_desktop_session`) is moved to the cc-rotate profile
//! with the app's host marks removed, so it runs on the profile's login; anything else the app
//! starts through the launcher is started as it is. Claude Code's launcher rules apply: nothing
//! is written to the terminal or standard output, no input is read, and Claude Code starts at
//! once. Errors go to cc-rotate's log only.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use ccr_core::{desktop, relay};

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
                let cfg = relay::launch_config_logged("desktop-launch");
                let relay_env = cfg.as_ref().map(relay::session_env);
                let app = desktop::app_profile(var, &home);
                // A session only the app's profile holds (one it forked, say) would run on that
                // profile's own login, out of the rotation's reach.
                if let Some(id) = desktop::resumed_id(args) {
                    desktop::adopt_app_transcript(&id, &rotate, &app);
                }
                let plan = desktop::plan_launch(args, var, &rotate, &home, relay_env);
                for name in &plan.remove {
                    cmd.env_remove(name);
                }
                if let Some(dir) = &plan.config_dir {
                    cmd.env("CLAUDE_CONFIG_DIR", dir);
                }
                cmd.envs(plan.set.iter().map(|(k, v)| (k, v)));
                if let (false, Some(cfg)) = (plan.set.is_empty(), &cfg) {
                    relay::ensure_running("desktop-launch", cfg);
                }
                if cfg!(unix) && plan.config_dir.as_deref() == Some(rotate.as_path()) {
                    // A new session has no transcript yet: it appears with the first message,
                    // so the helper waits for it while this process (Claude Code, once
                    // exec'd) runs. A resumed one may get a new id, and with it a new
                    // transcript, the same way.
                    let watch = Some(std::process::id());
                    if let Err(e) = process::spawn_link_transcripts(&rotate, &app, watch) {
                        process::log_error("desktop-launch", &e);
                    }
                }
            }
            Err(e) => process::log_error("desktop-launch", &e),
        }
    }
    start(cmd)
}

/// How often, and for how long at most, `link_transcripts` looks for a new session's transcript.
const LINK_POLL: Duration = Duration::from_secs(2);
const LINK_WATCH_MAX: Duration = Duration::from_secs(60 * 60);

/// `cc-rotate-ctl desktop-link-transcripts`: link the pool's desktop transcripts into the app's
/// profile (`ccr_core::desktop::link_app_transcripts`); with `watch_pid`, keep looking while
/// that process runs, until something new is linked or an hour has passed.
pub fn link_transcripts(profile: Option<PathBuf>, app_profile: &Path, watch_pid: Option<u32>) {
    let rotate = match ccr_core::resolve_profile_read_only(profile) {
        Ok(rotate) => rotate,
        Err(e) => return process::log_error("desktop-link-transcripts", &e),
    };
    // What this first sweep links is older sessions'; a new session's comes later.
    desktop::link_app_transcripts(&rotate, app_profile);
    let Some(pid) = watch_pid else { return };
    let start = Instant::now();
    while start.elapsed() < LINK_WATCH_MAX && ccr_core::process::alive(pid) {
        std::thread::sleep(LINK_POLL);
        if !desktop::link_app_transcripts(&rotate, app_profile).is_empty() {
            return;
        }
    }
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
