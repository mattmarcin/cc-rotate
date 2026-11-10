//! `cc-rotate-ctl relay run` in a scratch data folder. No request here reaches Anthropic: every
//! one is refused before the relay forwards anything.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};
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
    Command::new(env!("CARGO_BIN_EXE_cc-rotate-ctl"))
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

#[test]
fn relay_run_exits_at_once_while_another_relay_holds_the_lock() {
    let root = tempfile::tempdir().unwrap();
    let (data, _) = data_folder(root.path());
    std::fs::create_dir(data.join("relay.lock")).unwrap();
    let mut child = Running(relay_run(&data));
    let status = exits_within(&mut child.0, Duration::from_secs(10)).expect("still running");
    assert!(status.success());
}

#[test]
fn relay_run_serves_on_its_port_and_refuses_strangers() {
    let root = tempfile::tempdir().unwrap();
    let (data, cfg) = data_folder(root.path());
    let mut relay = Running(relay_run(&data));

    let deadline = Instant::now() + Duration::from_secs(20);
    while !ccr_core::relay::answers(cfg.port) {
        assert!(
            relay.0.try_wait().unwrap().is_none(),
            "the relay exited: {:?}",
            std::fs::read_to_string(data.join("logs/cc-rotate.log"))
        );
        assert!(Instant::now() < deadline, "the relay never answered");
        std::thread::sleep(Duration::from_millis(50));
    }

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
    let log = std::fs::read_to_string(&log_path).unwrap();
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
