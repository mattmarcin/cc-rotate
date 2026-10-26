//! Holding off Ctrl+C (and on Unix the other termination signals) while a child that receives
//! them too is finishing, so this process cleans up after it instead of ending first.

/// Interrupts do not end this process while it is alive; they still reach the child, which
/// shares the terminal. Dropping it restores the defaults.
pub struct Held {
    #[cfg(unix)]
    ids: Vec<signal_hook::SigId>,
}

impl Held {
    #[cfg(unix)]
    pub fn new() -> Self {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        let flag = Arc::new(AtomicBool::new(false));
        // A registered handler replaces the default of ending the process. Unlike an ignored
        // signal, it is not inherited by programs started later.
        let ids = [SIGINT, SIGTERM, SIGHUP, SIGQUIT]
            .into_iter()
            .filter_map(|sig| signal_hook::flag::register(sig, Arc::clone(&flag)).ok())
            .collect();
        Self { ids }
    }

    #[cfg(windows)]
    pub fn new() -> Self {
        use windows_sys::Win32::Foundation::TRUE;
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        // SAFETY: registers a handler that only returns TRUE (handled), like the launcher's.
        unsafe { SetConsoleCtrlHandler(Some(ignore), TRUE) };
        Self {}
    }
}

#[cfg(windows)]
unsafe extern "system" fn ignore(_ctrl_type: u32) -> windows_sys::core::BOOL {
    windows_sys::Win32::Foundation::TRUE
}

impl Drop for Held {
    #[cfg(unix)]
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }

    #[cfg(windows)]
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::FALSE;
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        // SAFETY: removes the handler `new` added.
        unsafe { SetConsoleCtrlHandler(Some(ignore), FALSE) };
    }
}
