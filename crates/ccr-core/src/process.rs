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

/// When the process `pid` started, as an opaque text that is the same for the same process and
/// differs once its id is reused. `None` when it cannot be told.
#[cfg(windows)]
pub fn start_time(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: plain Win32 calls on a handle opened and closed here.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        let ok = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user);
        CloseHandle(handle);
        (ok != 0).then(|| {
            (((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64).to_string()
        })
    }
}

/// When the process `pid` started: its start time in clock ticks since boot on Linux, else what
/// `ps` reports. `None` when it cannot be told.
#[cfg(unix)]
pub fn start_time(pid: u32) -> Option<String> {
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // Field 22, counted after the command name, which may itself contain spaces.
        let after = stat.rsplit_once(')')?.1;
        return after.split_whitespace().nth(19).map(str::to_string);
    }
    let out = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// The parent of the process `pid`, while both run.
#[cfg(windows)]
pub fn parent(pid: u32) -> Option<u32> {
    process_entry(pid).map(|(parent, _)| parent)
}

/// The executable name of the process `pid` (`bash.exe`), while it runs.
#[cfg(windows)]
pub fn name(pid: u32) -> Option<String> {
    process_entry(pid).map(|(_, name)| name)
}

/// The parent and executable name of the process `pid`, from a process snapshot.
#[cfg(windows)]
fn process_entry(pid: u32) -> Option<(u32, String)> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    // SAFETY: a snapshot handle opened and closed here, and an entry sized as the API asks.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = None;
        let mut more = Process32FirstW(snapshot, &mut entry);
        while more != 0 {
            if entry.th32ProcessID == pid {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                found = Some((entry.th32ParentProcessID, name));
                break;
            }
            more = Process32NextW(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
        found
    }
}

/// The parent of the process `pid`: from `/proc` on Linux, else what `ps` reports.
#[cfg(unix)]
pub fn parent(pid: u32) -> Option<u32> {
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // Field 4, counted after the command name, which may itself contain spaces.
        return stat
            .rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok();
    }
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// The command name of the process `pid` (`bash`): from `/proc` on Linux, else what `ps`
/// reports.
#[cfg(unix)]
pub fn name(pid: u32) -> Option<String> {
    if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        return Some(comm.trim().to_string()).filter(|c| !c.is_empty());
    }
    let out = Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // macOS prints the path, and a login shell's name starts with "-".
    let base = text
        .rsplit('/')
        .next()
        .unwrap_or(&text)
        .trim_start_matches('-');
    Some(base.to_string()).filter(|b| !b.is_empty())
}

/// Whether a process name is a shell, as Claude Code runs a hook command through one.
pub fn is_shell(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    matches!(
        name,
        "sh" | "bash" | "dash" | "zsh" | "ksh" | "fish" | "ash" | "cmd" | "pwsh" | "powershell"
    )
}

/// This process's nearest ancestors as they were when it started, to tell when the program that
/// started it is gone: a hook outlives a Claude Code session that was killed, since the shell
/// between them waits for it.
#[derive(Debug, Clone)]
pub struct Ancestors(Vec<(u32, Option<String>)>);

impl Ancestors {
    /// For a hook: the shell Claude Code runs it in and Claude Code, or Claude Code alone when it
    /// started the hook directly (a shell that runs a single command often replaces itself with
    /// it), so whatever started Claude Code is never watched.
    pub fn of_hook() -> Ancestors {
        let shell_parent = parent(std::process::id())
            .and_then(name)
            .is_some_and(|n| is_shell(&n));
        Ancestors::of_this_process(if shell_parent { 2 } else { 1 })
    }

    /// The process that started this one, past the shell Claude Code runs a command in, as
    /// "<pid>:<start time>": for a status line or hook, the Claude Code process, which no other
    /// process shares even under a reused id.
    pub fn launcher() -> Option<String> {
        let mut pid = parent(std::process::id())?;
        if name(pid).is_some_and(|n| is_shell(&n)) {
            pid = parent(pid)?;
        }
        (pid > 4).then(|| format!("{pid}:{}", start_time(pid).unwrap_or_default()))
    }

    /// Up to `depth` ancestors.
    pub fn of_this_process(depth: usize) -> Ancestors {
        let mut chain = Vec::new();
        let mut pid = std::process::id();
        for _ in 0..depth {
            match parent(pid) {
                // 0 and 1 (and 4, Windows' System) are the system's, not a program's.
                Some(p) if p > 4 && p != pid => {
                    chain.push((p, start_time(p)));
                    pid = p;
                }
                _ => break,
            }
        }
        Ancestors(chain)
    }

    /// Every ancestor recorded still runs (the same process, not a reused id).
    pub fn all_alive(&self) -> bool {
        self.0.iter().all(|(pid, started)| {
            alive(*pid)
                && match started {
                    Some(s) => start_time(*pid).as_ref() == Some(s),
                    None => true,
                }
        })
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod ancestry_tests {
    use super::*;

    #[test]
    fn finds_this_process_parent_and_its_name() {
        let me = std::process::id();
        let parent = parent(me).expect("a parent");
        assert!(alive(parent));
        assert!(name(me).is_some_and(|n| !n.is_empty()));
        let ancestors = Ancestors::of_this_process(2);
        assert!(!ancestors.is_empty());
        assert!(ancestors.all_alive());
        let launcher = Ancestors::launcher().expect("a launcher");
        assert_eq!(Ancestors::launcher().as_deref(), Some(launcher.as_str()));
    }

    #[test]
    fn knows_shells() {
        for shell in [
            "bash",
            "bash.exe",
            "sh",
            "zsh",
            "pwsh.exe",
            "PowerShell.exe",
            "cmd.exe",
        ] {
            assert!(is_shell(shell), "{shell}");
        }
        for other in ["claude", "claude.exe", "node", "cc-rotate.exe"] {
            assert!(!is_shell(other), "{other}");
        }
    }
}

/// Whether any process of the process group `pgid` is running (Unix; never on Windows, where a
/// check's tree dies with its job).
#[cfg(unix)]
pub fn group_alive(pgid: u32) -> bool {
    let Ok(pgid) = i32::try_from(pgid) else {
        return false;
    };
    // SAFETY: kill with signal 0 only checks; a negative id names the group.
    let rc = unsafe { libc::kill(-pgid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn group_alive(_pgid: u32) -> bool {
    false
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

    #[test]
    fn start_times_identify_a_process() {
        let own = start_time(std::process::id()).expect("this process has a start time");
        assert_eq!(
            start_time(std::process::id()).as_deref(),
            Some(own.as_str())
        );
        assert_eq!(start_time(u32::MAX - 7), None);
    }
}
