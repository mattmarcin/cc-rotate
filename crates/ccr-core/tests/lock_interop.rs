//! cc-rotate's locks against the real `proper-lockfile` package, which Claude Code bundles for
//! its OAuth refresh locks. Needs Node.js and `npm ci` in `tests/interop`; skipped otherwise.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ccr_core::lock::{DirLock, LockOptions};

fn fixture() -> Option<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/interop");
    if !dir.join("node_modules/proper-lockfile").is_dir() || which::which("node").is_err() {
        eprintln!("skipping lock interop: run `npm ci` in tests/interop (needs Node.js)");
        return None;
    }
    Some(dir.join("lock.js"))
}

fn node(script: &Path, args: &[&str]) -> String {
    let out = Command::new("node")
        .arg(script)
        .args(args)
        .stderr(Stdio::inherit())
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn opts(update_ms: u64, timeout_ms: u64) -> LockOptions {
    LockOptions {
        stale: Duration::from_millis(60_000),
        update: Duration::from_millis(update_ms),
        timeout: Duration::from_millis(timeout_ms),
    }
}

#[test]
fn proper_lockfile_sees_our_lock_as_held() {
    let Some(script) = fixture() else { return };
    let dir = tempfile::tempdir().unwrap();
    let lock = dir.path().join(".oauth_refresh.lock");
    let held = DirLock::acquire(&lock, opts(5_000, 0)).unwrap();
    assert_eq!(node(&script, &["try", lock.to_str().unwrap()]), "ELOCKED");
    drop(held);
    assert_eq!(node(&script, &["try", lock.to_str().unwrap()]), "acquired");
}

#[test]
fn we_wait_for_a_proper_lockfile_holder() {
    let Some(script) = fixture() else { return };
    let dir = tempfile::tempdir().unwrap();
    let lock = dir.path().join(".oauth_refresh.lock");
    let mut child = Command::new("node")
        .arg(&script)
        .args(["hold", lock.to_str().unwrap(), "1500"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !lock.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "node never took the lock"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let waited = Instant::now();
    let ours = DirLock::acquire(&lock, opts(5_000, 10_000)).unwrap();
    assert!(
        waited.elapsed() >= Duration::from_millis(1000),
        "acquired while node held it"
    );
    drop(ours);
    assert!(child.wait().unwrap().success());
}

#[test]
fn our_mtime_updates_keep_the_lock_fresh_for_proper_lockfile() {
    let Some(script) = fixture() else { return };
    let dir = tempfile::tempdir().unwrap();
    let lock = dir.path().join(".oauth_refresh.lock");
    // proper-lockfile with stale=2000 would steal a lock not touched for 2 s; ours updates every 500 ms.
    let held = DirLock::acquire(&lock, opts(500, 0)).unwrap();
    std::thread::sleep(Duration::from_millis(3_000));
    assert_eq!(
        node(&script, &["try", lock.to_str().unwrap(), "2000"]),
        "ELOCKED"
    );
    drop(held);
}

#[test]
fn proper_lockfile_takes_over_when_we_stop_updating() {
    let Some(script) = fixture() else { return };
    let dir = tempfile::tempdir().unwrap();
    let lock = dir.path().join(".oauth_refresh.lock");
    // A crashed holder: the directory exists but nobody refreshes it.
    std::fs::create_dir(&lock).unwrap();
    std::thread::sleep(Duration::from_millis(2_500));
    assert_eq!(
        node(&script, &["try", lock.to_str().unwrap(), "2000"]),
        "acquired"
    );
}
