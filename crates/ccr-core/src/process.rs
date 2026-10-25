//! Child processes that must not outlive their purpose: whether a process is alive, and a
//! process tree that can be killed as a whole.

use std::process::{Child, Command};

/// Whether the process `pid` is running.
#[cfg(windows)]
pub fn alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: plain Win32 calls on a handle opened and closed here.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &mut code);
        CloseHandle(handle);
        ok != 0 && code == STILL_ACTIVE as u32
    }
}

/// Whether the process `pid` is running (signal 0 checks without sending anything; `EPERM`
/// means it exists under another user).
#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: kill with signal 0 only checks for the process.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// A child and everything it starts. On Windows the child runs in a job object that kills the
/// whole tree when the job is closed, which also happens when this process dies; on Unix it runs
/// in its own process group.
pub struct Tree {
    child: Child,
    #[cfg(windows)]
    job: Option<windows_sys::Win32::Foundation::HANDLE>,
}

impl Tree {
    pub fn spawn(cmd: &mut Command) -> std::io::Result<Tree> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let child = cmd.spawn()?;
        #[cfg(windows)]
        {
            let job = job_for(&child);
            Ok(Tree { child, job })
        }
        #[cfg(unix)]
        Ok(Tree { child })
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    /// Kill the child and everything it started, and reap the child.
    pub fn kill(&mut self) {
        #[cfg(windows)]
        if let Some(job) = self.job {
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;
            // SAFETY: the job handle is open until drop.
            unsafe { TerminateJobObject(job, 1) };
        }
        #[cfg(unix)]
        if let Ok(pgid) = i32::try_from(self.child.id()) {
            // SAFETY: the group was created for this child (process_group(0)).
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Tree {
    /// Whatever the child started and left running goes too.
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(job) = self.job.take() {
            use windows_sys::Win32::Foundation::CloseHandle;
            // SAFETY: closing the job kills its processes (KILL_ON_JOB_CLOSE).
            unsafe { CloseHandle(job) };
        }
        #[cfg(unix)]
        if let Ok(pgid) = i32::try_from(self.child.id()) {
            // SAFETY: as in `kill`; harmless when the group is already gone.
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }
}

/// A job that kills its processes when closed, with the child in it. `None` when that is not
/// possible; the child is then only killed directly.
#[cfg(windows)]
fn job_for(child: &Child) -> Option<windows_sys::Win32::Foundation::HANDLE> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    // SAFETY: the job is created, configured and closed on failure here; the process handle
    // belongs to `child`, which outlives this call.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if configured == 0 || AssignProcessToJobObject(job, child.as_raw_handle() as _) == 0 {
            CloseHandle(job);
            return None;
        }
        Some(job)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive() {
        assert!(alive(std::process::id()));
        assert!(!alive(u32::MAX - 7));
    }
}
