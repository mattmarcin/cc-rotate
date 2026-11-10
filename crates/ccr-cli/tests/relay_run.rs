//! `cc-rotate-ctl relay run` in a scratch data folder. No request here reaches Anthropic: every
//! one is refused before the relay forwards anything.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

use ccr_core::relay::RelayConfig;

/// Kills the relay when the test ends, however it ends.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn relay_run(data: &Path) -> Child {
    common::program(env!("CARGO_BIN_EXE_cc-rotate-ctl"))
        .args(["relay", "run"])
        .env("CC_ROTATE_DATA_DIR", data)
        .env("CC_ROTATE_PROFILE", data.join("no profile"))
        .env_remove("CC_ROTATE_RELAY_TEST_UPSTREAM")
        .spawn()
        .unwrap()
}

fn exits_within(child: &mut Child, limit: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

fn data_folder(root: &Path) -> (std::path::PathBuf, RelayConfig) {
    let data = root.join("data");
    drop(ccr_core::Store::open(&data.join("store.db")).unwrap());
    let cfg = RelayConfig::create(&data.join("relay.json")).unwrap();
    (data, cfg)
}

/// Waits until the relay answers on `port`, failing if it exits or takes longer than `limit`.
fn wait_serving(relay: &mut Running, data: &Path, port: u16, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !ccr_core::relay::answers(port) {
        assert!(
            relay.0.try_wait().unwrap().is_none(),
            "the relay exited: {:?}",
            std::fs::read_to_string(data.join("logs/cc-rotate.log"))
        );
        assert!(Instant::now() < deadline, "the relay never answered");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The relay's log once it has `count` lines containing `needle`: the log is written by a
/// thread of its own, a moment after the event.
fn log_with(data: &Path, needle: &str, count: usize) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let log = std::fs::read_to_string(data.join("logs/relay.log")).unwrap_or_default();
        if log.matches(needle).count() >= count || Instant::now() > deadline {
            return log;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn relay_run_exits_at_once_while_another_process_holds_the_lock() {
    let root = tempfile::tempdir().unwrap();
    let (data, _) = data_folder(root.path());
    let held = std::fs::File::create(data.join("relay.lock")).unwrap();
    held.try_lock().unwrap();
    let mut child = Running(relay_run(&data));
    let status = exits_within(&mut child.0, Duration::from_secs(10)).expect("still running");
    assert!(status.success());
    assert!(!data.join("logs/cc-rotate.log").exists());
}

/// Another program on the relay's port: the relay exits at once and without a word on the
/// terminal, whether a launcher or its service started it, and says why in `relay.log` for
/// `doctor`.
#[test]
fn relay_run_exits_quietly_when_its_port_is_taken() {
    for from_service in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (data, cfg) = data_folder(root.path());
        let _taken = std::net::TcpListener::bind(("127.0.0.1", cfg.port)).unwrap();
        let mut cmd = common::program(env!("CARGO_BIN_EXE_cc-rotate-ctl"));
        cmd.args(["relay", "run"])
            .env("CC_ROTATE_DATA_DIR", &data)
            .env("CC_ROTATE_PROFILE", data.join("no profile"))
            .env_remove("CC_ROTATE_RELAY_TEST_UPSTREAM")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if from_service {
            cmd.env("CC_ROTATE_RELAY_SERVICE", "1");
        } else {
            cmd.env_remove("CC_ROTATE_RELAY_SERVICE");
        }
        let started = Instant::now();
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "service {from_service}");
        assert!(started.elapsed() < Duration::from_secs(9), "it waited");
        assert!(out.stdout.is_empty() && out.stderr.is_empty(), "{out:?}");
        assert!(!data.join("logs/cc-rotate.log").exists());
        let log = std::fs::read_to_string(data.join("logs/relay.log")).unwrap_or_default();
        let line = format!(
            "cannot start: 127.0.0.1:{} is taken by another program",
            cfg.port
        );
        assert!(log.contains(&line), "service {from_service}: {log:?}");
        assert_eq!(log.lines().count(), 1, "{log}");
    }
}

#[cfg(unix)]
#[test]
fn a_killed_relay_leaves_no_lock_behind() {
    let root = tempfile::tempdir().unwrap();
    let (data, cfg) = data_folder(root.path());
    let mut first = Running(relay_run(&data));
    wait_serving(&mut first, &data, cfg.port, Duration::from_secs(20));
    // SIGKILL: nothing in the relay runs on the way out.
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    assert!(data.join("relay.lock").exists());

    let mut second = Running(relay_run(&data));
    wait_serving(&mut second, &data, cfg.port, Duration::from_secs(2));
    assert_eq!(log_with(&data, "started", 2).matches("started").count(), 2);
}

#[test]
fn relay_run_serves_on_its_port_and_refuses_strangers() {
    let root = tempfile::tempdir().unwrap();
    let (data, cfg) = data_folder(root.path());
    let mut relay = Running(relay_run(&data));
    wait_serving(&mut relay, &data, cfg.port, Duration::from_secs(20));

    let mut tcp = TcpStream::connect(("127.0.0.1", cfg.port)).unwrap();
    write!(
        tcp,
        "POST /r/wrong/v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        cfg.port
    )
    .unwrap();
    let mut out = String::new();
    tcp.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 404"), "{out}");

    // A second relay finds the lock held and leaves.
    let mut second = Running(relay_run(&data));
    let status = exits_within(&mut second.0, Duration::from_secs(10)).expect("still running");
    assert!(status.success());
    assert!(relay.0.try_wait().unwrap().is_none());

    let log_path = data.join("logs/relay.log");
    let log = log_with(&data, "refused 404", 1);
    assert!(log.contains("started"), "{log}");
    assert!(log.contains("refused 404"), "{log}");
    assert!(
        !log.contains(&cfg.secret) && !log.contains("wrong"),
        "{log}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&log_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn relay_run_that_cannot_start_says_why_and_waits_before_exiting() {
    let root = tempfile::tempdir().unwrap();
    let (data, cfg) = data_folder(root.path());
    // A store that cannot be opened: the relay cannot resolve the store or the profile.
    std::fs::write(data.join("store.db"), "not a database").unwrap();
    let started = Instant::now();
    let mut child = Running(
        common::program(env!("CARGO_BIN_EXE_cc-rotate-ctl"))
            .args(["relay", "run"])
            .env("CC_ROTATE_DATA_DIR", &data)
            .env_remove("CC_ROTATE_PROFILE")
            .spawn()
            .unwrap(),
    );
    let log = log_with(&data, "cannot start", 1);
    assert!(log.contains("cannot start"), "{log}");
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(
        !ccr_core::relay::answers(cfg.port),
        "it never took the port"
    );
    let status = exits_within(&mut child.0, Duration::from_secs(30)).expect("still running");
    assert!(!status.success());
    assert!(
        started.elapsed() >= Duration::from_secs(9),
        "exited after {:?}: its service would restart it at once",
        started.elapsed()
    );
}

fn ctl(data: &Path, args: &[&str]) -> std::process::Output {
    common::program(env!("CARGO_BIN_EXE_cc-rotate-ctl"))
        .args(args)
        .env("CC_ROTATE_DATA_DIR", data)
        .env("CC_ROTATE_PROFILE", data.join("profile"))
        .output()
        .unwrap()
}

#[test]
fn relay_shows_the_address_but_never_the_secret() {
    let root = tempfile::tempdir().unwrap();
    let (data, cfg) = data_folder(root.path());
    let out = ctl(&data, &["relay"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("Relay mode: off"), "{text}");
    assert!(
        text.contains(&format!("Address: 127.0.0.1:{}", cfg.port)),
        "{text}"
    );
    assert!(text.contains("Answering: no"), "{text}");
    assert!(text.contains("Service: "), "{text}");
    assert!(
        text.contains("Sessions without the relay's address (last 10 minutes): 0"),
        "{text}"
    );
    assert!(!text.contains(&cfg.secret), "{text}");
}

/// Sessions started while relay mode was on keep the relay's address, and get connection errors
/// once the relay leaves, an hour after `relay off`.
#[test]
fn relay_off_says_to_restart_the_relays_sessions() {
    let root = tempfile::tempdir().unwrap();
    let (data, _) = data_folder(root.path());
    let out = ctl(&data, &["relay", "off"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("Relay mode is off."), "{text}");
    assert!(
        text.contains(
            "Restart the sessions started while relay mode was on within the hour: once the relay has removed itself, they get connection errors."
        ),
        "{text}"
    );
}

#[test]
fn relay_on_refuses_an_account_that_cannot_be_home() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    {
        let store = ccr_core::Store::open(&data.join("store.db")).unwrap();
        store
            .insert_account(
                "gone",
                &ccr_core::store::AccountIdentity::default(),
                ccr_core::store::AccountState::NeedsLogin,
            )
            .unwrap();
    }
    for (home, why) in [("gone", "signed out"), ("nobody", "no account matches")] {
        let out = ctl(&data, &["relay", "on", "--home", home, "--no-service"]);
        assert!(!out.status.success());
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(err.contains(why), "{err}");
    }
    assert!(!data.join("relay.json").exists());

    let out = ctl(&data, &["relay", "off"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains("Relay mode is off")
    );
}

#[test]
fn a_relay_the_service_started_waits_for_the_lock_and_then_serves() {
    let root = tempfile::tempdir().unwrap();
    let (data, cfg) = data_folder(root.path());
    // The relay a launcher started holds the lock.
    let held = std::fs::File::create(data.join("relay.lock")).unwrap();
    held.try_lock().unwrap();
    let mut relay = Running(
        common::program(env!("CARGO_BIN_EXE_cc-rotate-ctl"))
            .args(["relay", "run"])
            .env("CC_ROTATE_DATA_DIR", &data)
            .env("CC_ROTATE_PROFILE", data.join("no profile"))
            .env("CC_ROTATE_RELAY_SERVICE", "1")
            .env_remove("CC_ROTATE_RELAY_TEST_UPSTREAM")
            .spawn()
            .unwrap(),
    );
    let log = log_with(&data, "waiting", 1);
    assert!(log.contains("another relay runs; waiting"), "{log}");
    std::thread::sleep(Duration::from_millis(500));
    assert!(relay.0.try_wait().unwrap().is_none(), "it exited");
    assert!(!ccr_core::relay::answers(cfg.port));

    // The launcher's relay ends: the service's takes over.
    drop(held);
    wait_serving(&mut relay, &data, cfg.port, Duration::from_secs(20));
}
