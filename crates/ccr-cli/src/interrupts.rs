//! Holding off Ctrl+C (and on Unix the other termination signals) while a child that receives
//! them too is finishing, so this process cleans up after it instead of ending first.

/// Interrupts do not end this process while it is alive; they still reach the child, which
/// shares the terminal. Dropping it restores the defaults.
pub struct Held {
    /// Once set, the signals do what they do by default again (end the process).
    #[cfg(unix)]
    released: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// A termination signal sent to this process alone (not Ctrl+C) arrived.
    #[cfg(unix)]
    ended: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Held {
    #[cfg(unix)]
    pub fn new() -> Self {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        let released = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(AtomicBool::new(false));
        for sig in [SIGINT, SIGTERM, SIGHUP, SIGQUIT] {
            // Does nothing until released, then ends the process the way the signal would have.
            // A registered handler, unlike an ignored signal, is not inherited by children.
            let _ = signal_hook::flag::register_conditional_default(sig, Arc::clone(&released));
            if sig != SIGINT {
                let _ = signal_hook::flag::register(sig, Arc::clone(&ended));
            }
        }
        Self { released, ended }
    }

    #[cfg(windows)]
    pub fn new() -> Self {
        use windows_sys::Win32::Foundation::TRUE;
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        // SAFETY: registers a handler that only returns TRUE (handled), like the launcher's.
        unsafe { SetConsoleCtrlHandler(Some(ignore), TRUE) };
        Self {}
    }

    /// Whether this process was asked to end (SIGTERM, SIGHUP, SIGQUIT) while held: the child
    /// did not get that signal, so the caller stops it and cleans up.
    pub fn ended(&self) -> bool {
        #[cfg(unix)]
        return self.ended.load(std::sync::atomic::Ordering::Acquire);
        #[cfg(windows)]
        return false;
    }
}

#[cfg(windows)]
unsafe extern "system" fn ignore(_ctrl_type: u32) -> windows_sys::core::BOOL {
    windows_sys::Win32::Foundation::TRUE
}

impl Drop for Held {
    #[cfg(unix)]
    fn drop(&mut self) {
        // Unregistering a handler would leave the signal ignored, not back to its default.
        self.released
            .store(true, std::sync::atomic::Ordering::Release);
    }

    #[cfg(windows)]
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::FALSE;
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        // SAFETY: removes the handler `new` added.
        unsafe { SetConsoleCtrlHandler(Some(ignore), FALSE) };
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const CHILD: &str = "CCR_INTERRUPTS_TEST_CHILD";
    const NAME: &str =
        "interrupts::tests::a_termination_signal_is_held_then_ends_the_process_again";

    /// While held, a termination signal sent to this process leaves it running and is reported,
    /// so the sign-in can be stopped and its staging folder removed; once released, the signal
    /// ends the process as it would have. Run in a copy of the test binary, since the handlers
    /// stay registered for the whole process, where other tests raise signals too.
    #[test]
    fn a_termination_signal_is_held_then_ends_the_process_again() {
        use signal_hook::consts::SIGTERM;
        use std::os::unix::process::ExitStatusExt;
        if std::env::var_os(CHILD).is_some() {
            let held = Held::new();
            signal_hook::low_level::raise(SIGTERM).unwrap();
            assert!(held.ended(), "still running, and told");
            drop(held);
            signal_hook::low_level::raise(SIGTERM).unwrap();
            // Not reached: the default action ended the process.
            std::process::exit(0);
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--test-threads=1", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert_eq!(
            out.status.signal(),
            Some(SIGTERM),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
