//! The herdr entry points (spec section 19): the `SessionStart` hook and the background report
//! it starts.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ccr_core::herdr;

use crate::process;

/// How often a report herdr has not accepted yet is sent again.
const RETRY_INTERVAL: Duration = Duration::from_millis(500);
/// How long to keep trying. herdr accepts the report once it has seen `claude` running in the
/// pane, normally within a second of Claude Code starting.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// `cc-rotate-ctl hook session-start`: outside herdr, do nothing; inside, start the report in the
/// background and return at once, so Claude Code's startup never waits on herdr. It prints
/// nothing, since Claude Code adds a SessionStart hook's output to the session's context.
pub fn session_start(read_stdin: impl FnOnce() -> String) -> Result<()> {
    let Some(pane) = herdr::pane_from_env() else {
        return Ok(());
    };
    let Some(session) = herdr::session_to_report(&read_stdin())? else {
        return Ok(());
    };
    // Validate here too, so a bad value is logged by the hook rather than lost in the background.
    herdr::report_args(&pane.id, &session, 0)?;
    let seq = u64::try_from(ccr_core::time::now_ms()).unwrap_or(0);
    process::spawn_herdr_report(&pane.id, &session, seq)
}

/// `cc-rotate-ctl herdr-report`: tell herdr to resume `session_id` in `pane_id` with
/// `cc-rotate --resume <session_id>`. herdr answers `resume_not_accepted` until it has seen
/// `claude` running in the pane, so that answer is retried until `timeout`.
pub fn report(pane_id: &str, session_id: &str, seq: u64, timeout: Duration) -> Result<()> {
    let args = herdr::report_args(pane_id, session_id, seq)?;
    let bin = herdr::bin_from_env();
    let bin =
        herdr::resolve(&bin).with_context(|| format!("Couldn't find {}", bin.to_string_lossy()))?;
    let started = Instant::now();
    let mut attempts = 0;
    loop {
        attempts += 1;
        let out = Command::new(&bin)
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", bin.display()))?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if herdr::is_not_yet_accepted(&stderr) && started.elapsed() + RETRY_INTERVAL < timeout {
            std::thread::sleep(RETRY_INTERVAL);
            continue;
        }
        bail!(
            "herdr did not take the resume command for pane {pane_id} after {attempts} attempt(s) in {} ms ({}): {}",
            started.elapsed().as_millis(),
            out.status,
            stderr.trim()
        );
    }
}
