//! Directory locks with the same on-disk protocol as the `proper-lockfile` npm package,
//! which Claude Code uses for its OAuth refresh locks.
//!
//! Protocol: the lock is a directory. Creating it acquires the lock. The holder refreshes the
//! directory's mtime every `update` interval. A lock whose mtime is older than `stale` is
//! considered abandoned and may be removed and re-acquired. Releasing removes the directory.
//!
//! Claude Code also writes an owner record so it can take over a lock from a dead process.
//! We deliberately write none: without a record, Claude Code waits for the lock to go stale
//! instead of taking it over, which is the behavior we want while a swap holds it.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use filetime::FileTime;

/// Claude Code's settings for its refresh locks (`stale: 60000`, `update: 5000`).
pub const CLAUDE_STALE: Duration = Duration::from_millis(60_000);
pub const CLAUDE_UPDATE: Duration = Duration::from_millis(5_000);
/// Claude Code's setting for its credentials write lock (`.storage-write`).
pub const CLAUDE_STORAGE_STALE: Duration = Duration::from_millis(15_000);

#[derive(Debug, Clone, Copy)]
pub struct LockOptions {
    pub stale: Duration,
    pub update: Duration,
    /// How long to keep retrying while another holder has the lock. Zero means try once.
    pub timeout: Duration,
}

impl LockOptions {
    pub fn claude(timeout: Duration) -> Self {
        Self {
            stale: CLAUDE_STALE,
            update: CLAUDE_UPDATE,
            timeout,
        }
    }

    /// Claude Code's credentials write lock (`stale: 15000`); updating every 5 s keeps it
    /// well inside that.
    pub fn claude_storage(timeout: Duration) -> Self {
        Self {
            stale: CLAUDE_STORAGE_STALE,
            update: CLAUDE_UPDATE,
            timeout,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("lock {0} is held by another process")]
    Busy(PathBuf),
    #[error("could not acquire lock {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// A held lock. Dropping it stops the mtime updater and removes the lock directory.
#[derive(Debug)]
pub struct DirLock {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    updater: Option<JoinHandle<()>>,
}

impl DirLock {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn acquire(path: &Path, opts: LockOptions) -> Result<DirLock, LockError> {
        let deadline = Instant::now() + opts.timeout;
        let mut delay = Duration::from_millis(25);
        loop {
            match std::fs::create_dir(path) {
                Ok(()) => return Ok(Self::held(path, opts.update)),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    if is_stale(path, opts.stale) && remove_stale(path) {
                        continue;
                    }
                }
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).map_err(|source| LockError::Io {
                            path: path.to_path_buf(),
                            source,
                        })?;
                        continue;
                    }
                    return Err(LockError::Io {
                        path: path.to_path_buf(),
                        source: e,
                    });
                }
                // Windows reports a directory mid-removal by another process as access denied.
                Err(e) if e.kind() == ErrorKind::PermissionDenied => {}
                Err(source) => {
                    return Err(LockError::Io {
                        path: path.to_path_buf(),
                        source,
                    });
                }
            }
            if Instant::now() >= deadline {
                return Err(LockError::Busy(path.to_path_buf()));
            }
            std::thread::sleep(delay.min(deadline.saturating_duration_since(Instant::now())));
            delay = (delay * 2).min(Duration::from_millis(250));
        }
    }

    fn held(path: &Path, update: Duration) -> DirLock {
        let stop = Arc::new(AtomicBool::new(false));
        let updater = {
            let stop = stop.clone();
            let path = path.to_path_buf();
            std::thread::spawn(move || {
                let tick = Duration::from_millis(50);
                let mut since = Duration::ZERO;
                while !stop.load(Ordering::Acquire) {
                    std::thread::sleep(tick);
                    since += tick;
                    if since >= update {
                        since = Duration::ZERO;
                        let _ = filetime::set_file_mtime(&path, FileTime::now());
                    }
                }
            })
        };
        DirLock {
            path: path.to_path_buf(),
            stop,
            updater: Some(updater),
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.updater.take() {
            let _ = handle.join();
        }
        for _ in 0..20 {
            match std::fs::remove_dir(&self.path) {
                Ok(()) => return,
                Err(e) if e.kind() == ErrorKind::NotFound => return,
                Err(_) => std::thread::sleep(Duration::from_millis(25)),
            }
        }
    }
}

fn is_stale(path: &Path, stale: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age > stale)
        .unwrap_or(false)
}

fn remove_stale(path: &Path) -> bool {
    match std::fs::remove_dir(path) {
        Ok(()) => true,
        Err(e) => e.kind() == ErrorKind::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(timeout_ms: u64) -> LockOptions {
        LockOptions {
            stale: Duration::from_millis(60_000),
            update: Duration::from_millis(100),
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    #[test]
    fn acquire_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        {
            let lock = DirLock::acquire(&path, opts(0)).unwrap();
            assert!(lock.path().is_dir());
        }
        assert!(!path.exists());
    }

    #[test]
    fn second_holder_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let _held = DirLock::acquire(&path, opts(0)).unwrap();
        assert!(matches!(
            DirLock::acquire(&path, opts(200)),
            Err(LockError::Busy(_))
        ));
    }

    #[test]
    fn waits_for_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let held = DirLock::acquire(&path, opts(0)).unwrap();
        let p2 = path.clone();
        let waiter = std::thread::spawn(move || DirLock::acquire(&p2, opts(3_000)).is_ok());
        std::thread::sleep(Duration::from_millis(300));
        drop(held);
        assert!(waiter.join().unwrap());
    }

    #[test]
    fn stale_lock_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        std::fs::create_dir(&path).unwrap();
        let old = FileTime::from_system_time(SystemTime::now() - Duration::from_secs(120));
        filetime::set_file_mtime(&path, old).unwrap();
        let lock = DirLock::acquire(&path, opts(0)).unwrap();
        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn holder_keeps_mtime_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let old = SystemTime::now() - Duration::from_secs(1);
        let _lock = DirLock::acquire(&path, opts(0)).unwrap();
        filetime::set_file_mtime(&path, FileTime::from_system_time(old)).unwrap();
        std::thread::sleep(Duration::from_millis(400));
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(modified > old + Duration::from_millis(500));
    }
}
