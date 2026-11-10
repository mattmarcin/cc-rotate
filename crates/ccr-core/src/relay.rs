//! Relay mode's state: whether it is on, which account is home, and `relay.json`.
//!
//! The design is in `docs/specs/2026-11-10-relay-mode.md`.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::lock::{DirLock, LockOptions};
use crate::paths::Paths;
use crate::store::{Account, AccountState, Store, kind};
use crate::swap::{self, RelayChange, Settle};

/// Setting that holds `on` while relay mode is on. Written only by `set_mode`.
pub const SETTING_MODE: &str = "relay_mode";
/// Setting that holds the home account's id while relay mode is on. Written only by `set_mode`.
pub const SETTING_HOME: &str = "home_account";

/// Claude Code reads the API address from this variable.
pub const ENV_BASE_URL: &str = "ANTHROPIC_BASE_URL";
/// Tells Claude Code to treat a base URL it does not know as Anthropic's own.
pub const ENV_FIRST_PARTY: &str = "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL";

/// The home account's id when relay mode is on, else `None`.
pub fn home(store: &Store) -> Result<Option<i64>> {
    if store.setting(SETTING_MODE)?.as_deref() != Some("on") {
        return Ok(None);
    }
    Ok(store
        .setting(SETTING_HOME)?
        .and_then(|v| v.trim().parse::<i64>().ok()))
}

/// The account the profile belongs to: the home account in relay mode, else the active one.
pub fn profile_account(store: &Store) -> Result<Option<Account>> {
    match home(store)? {
        Some(id) => store.account(id),
        None => store.active_account(),
    }
}

/// Turn relay mode on with `home` as the home account, or off with `None`.
///
/// Both settings are written in one transaction, straight to the table: neither is a setting
/// the user can change with `config`. Inside a transaction already (a swap's commit), they are
/// written as part of it.
pub fn set_mode(store: &Store, home: Option<i64>) -> Result<()> {
    if store.conn().is_autocommit() {
        store.immediate(|s| write_mode(s, home))
    } else {
        write_mode(store, home)
    }
}

fn write_mode(store: &Store, home: Option<i64>) -> Result<()> {
    let conn = store.conn();
    match home {
        Some(id) => {
            for (key, value) in [
                (SETTING_MODE, "on".to_string()),
                (SETTING_HOME, id.to_string()),
            ] {
                conn.execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2)
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                    params![key, value],
                )?;
            }
        }
        None => {
            conn.execute(
                "DELETE FROM settings WHERE key IN (?1, ?2)",
                params![SETTING_MODE, SETTING_HOME],
            )?;
        }
    }
    Ok(())
}

/// Turn relay mode on with `home` as the home account (spec 4, step 4): under the swap lock, make
/// its login the profile's, without changing which account is active.
///
/// Refused unless the account is in the pool, signed in and not held back by a check, while a
/// swap is pending, and while the profile's login is not identified. When another account's
/// login is in the profile (the active one's, or the previous home account's), it is saved to
/// that account's row and the home account's written in its place (`swap::move_profile_login`).
pub fn turn_on(store: &Store, paths: &Paths, home_id: i64) -> Result<()> {
    let _swap_lock = swap_lock(store, paths)?;
    let account = store
        .account(home_id)?
        .ok_or_else(|| anyhow!("account {home_id} does not exist"))?;
    eligible_home(store, &account)?;
    settled(store, paths)?;
    let profile = profile_account(store)?.ok_or_else(|| {
        anyhow!("no account is in use, so cc-rotate can't tell whose login the profile has")
    })?;
    let change = RelayChange::On { home: home_id };
    if profile.id != home_id {
        return swap::move_profile_login(store, paths, profile.id, home_id, change);
    }
    store.immediate(|s| {
        set_mode(s, Some(home_id))?;
        log_mode(s, home_id, change)
    })
}

/// `cc-rotate-ctl relay on --home <reference>` (spec 4): refuse unless the account can be home,
/// write `relay.json` when it is not there, `install` the relay's service, then `turn_on`.
///
/// Nothing changes when the account is refused. The port and secret of a `relay.json` already
/// there stay, so sessions started earlier keep reaching the relay.
pub fn on(
    store: &Store,
    paths: &Paths,
    reference: &str,
    install: impl FnOnce() -> Result<()>,
) -> Result<(Account, RelayConfig)> {
    let account = store.find_account(reference)?;
    eligible_home(store, &account)?;
    let path = paths.relay_config();
    let cfg = match RelayConfig::read(&path)? {
        Some(cfg) => cfg,
        None => RelayConfig::create(&path)?,
    };
    install().context("installing the relay's service")?;
    turn_on(store, paths, account.id)?;
    Ok((account, cfg))
}

/// Refuse a home account that is signed out or held back by a check (spec 4, step 1).
fn eligible_home(store: &Store, account: &Account) -> Result<()> {
    if account.state == AccountState::NeedsLogin || store.credential_meta(account.id)?.is_none() {
        return Err(anyhow!(
            "{} is signed out. Sign in to it again first.",
            account.label
        ));
    }
    if crate::usage_check::state(store, account.id)?.held_back {
        return Err(anyhow!(
            "{} is on hold until a usage check confirms its login. Check its usage first.",
            account.label
        ));
    }
    Ok(())
}

/// Turn relay mode off (spec 4, `relay off` step 1): under the swap lock, make the active
/// account's login the profile's again, the home account's saved to its row first.
///
/// Nothing to do when it is off. Refused while a swap is pending, and while the profile's login
/// is not identified.
pub fn turn_off(store: &Store, paths: &Paths) -> Result<()> {
    let _swap_lock = swap_lock(store, paths)?;
    if store.setting(SETTING_MODE)?.as_deref() != Some("on") {
        return Ok(());
    }
    settled(store, paths)?;
    let active = store.active_account()?;
    match (home(store)?, active) {
        (Some(home_id), Some(active)) if active.id != home_id => {
            swap::move_profile_login(store, paths, home_id, active.id, RelayChange::Off)
        }
        (home_id, _) => store.immediate(|s| {
            set_mode(s, None)?;
            match home_id {
                Some(id) => log_mode(s, id, RelayChange::Off),
                None => Ok(()),
            }
        }),
    }
}

/// The swap lock, waited for as long as Claude Code's locks are.
fn swap_lock(store: &Store, paths: &Paths) -> Result<DirLock> {
    let timeout = std::time::Duration::from_millis(store.settings()?.lock_timeout_ms);
    DirLock::acquire(&paths.swap_lock(), LockOptions::claude(timeout))
        .context("waiting for a swap to finish")
}

/// Refuse while a swap is pending, and identify a login that changed in the profile, so the
/// profile's login is saved to the right account.
fn settled(store: &Store, paths: &Paths) -> Result<()> {
    if swap::pending_swap(store)?.is_some() {
        return Err(anyhow!(
            "a swap was interrupted and isn't settled yet. cc-rotate settles it within a minute; try again then."
        ));
    }
    if swap::identify_changed_login(store, paths)? == Settle::Unsure {
        return Err(anyhow!(
            "the profile's login changed and isn't identified yet. Try again in a minute."
        ));
    }
    Ok(())
}

/// Log a change of mode that wrote no login: the profile kept `account`'s.
fn log_mode(s: &Store, account: i64, change: RelayChange) -> Result<()> {
    s.log_event(
        kind::RELAY_MODE,
        Some(account),
        Some(account),
        Some(change.as_str()),
        Some(serde_json::json!({ "home": change.home() })),
    )?;
    Ok(())
}

/// Meta key prefix of a session whose status line runs without the relay's address.
pub const META_BYPASS_PREFIX: &str = "relay_bypass:";

/// Meta key prefix of a session whose status line runs with a relay's address while relay mode is
/// off: it was started while relay mode was on.
pub const META_STALE_PREFIX: &str = "relay_stale:";

/// How long a session's report without the relay's address, or with it while relay mode is off,
/// counts (spec 4).
pub const BYPASS_WINDOW_MS: i64 = 10 * 60_000;

/// Record that `session_id`'s status line ran at `now_ms` without the relay's address: the session
/// bypasses the relay and runs on the home account directly.
pub fn note_bypass(store: &Store, session_id: &str, now_ms: i64) -> Result<()> {
    store.set_meta(&format!("{META_BYPASS_PREFIX}{session_id}"), now_ms)
}

/// How many sessions reported without the relay's address in the 10 minutes before `now_ms`.
pub fn bypass_count(store: &Store, now_ms: i64) -> Result<usize> {
    count_marks(store, META_BYPASS_PREFIX, now_ms)
}

/// Record that `session_id`'s status line ran at `now_ms` with a relay's address while relay mode
/// is off: the session still sends its requests to a relay, which leaves for good an hour after
/// `relay off`, so it has to be restarted.
pub fn note_stale(store: &Store, session_id: &str, now_ms: i64) -> Result<()> {
    store.set_meta(&format!("{META_STALE_PREFIX}{session_id}"), now_ms)
}

/// How many sessions reported with a relay's address while relay mode was off, in the 10 minutes
/// before `now_ms`.
pub fn stale_count(store: &Store, now_ms: i64) -> Result<usize> {
    count_marks(store, META_STALE_PREFIX, now_ms)
}

/// How many sessions under `prefix` reported in the 10 minutes before `now_ms`.
fn count_marks(store: &Store, prefix: &str, now_ms: i64) -> Result<usize> {
    let count: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM meta WHERE substr(key, 1, ?1) = ?2
             AND CAST(value AS INTEGER) > ?3 AND CAST(value AS INTEGER) <= ?4",
        params![
            prefix.len() as i64,
            prefix,
            now_ms - BYPASS_WINDOW_MS,
            now_ms
        ],
        |r| r.get(0),
    )?;
    Ok(count as usize)
}

/// Forget the sessions that last reported without the relay's address, or with it while relay
/// mode was off, 10 minutes or more before `now_ms` (`Store::prune`).
pub(crate) fn prune_bypass(store: &Store, now_ms: i64) -> Result<()> {
    for prefix in [META_BYPASS_PREFIX, META_STALE_PREFIX] {
        store.conn().execute(
            "DELETE FROM meta WHERE substr(key, 1, ?1) = ?2 AND CAST(value AS INTEGER) <= ?3",
            params![prefix.len() as i64, prefix, now_ms - BYPASS_WINDOW_MS],
        )?;
    }
    Ok(())
}

/// Whether `base_url` looks like a relay's address, `http://127.0.0.1:<port>/r/...`, whichever
/// relay: after `relay off` the relay's `relay.json` goes when it leaves.
pub fn points_at_relay(base_url: Option<&str>) -> bool {
    let Some(rest) = base_url.and_then(|u| u.strip_prefix("http://127.0.0.1:")) else {
        return false;
    };
    let Some((port, _)) = rest.split_once("/r/") else {
        return false;
    };
    !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())
}

/// Whether a session with this environment bypasses the relay: its `ANTHROPIC_BASE_URL` is not
/// the relay's address. Without a readable `relay.json`, only a session without the variable does.
pub fn bypasses(cfg: Option<&RelayConfig>, base_url: Option<&str>) -> bool {
    match (cfg, base_url) {
        (_, None) => true,
        (Some(cfg), Some(url)) => url != cfg.base_url(),
        (None, Some(_)) => false,
    }
}

/// What `relay.json` holds: the port the relay listens on and the secret in its address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayConfig {
    pub port: u16,
    pub secret: String,
}

impl RelayConfig {
    /// `Ok(None)` when the file does not exist.
    pub fn read(path: &Path) -> Result<Option<RelayConfig>> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is not a valid relay config", path.display()))
                .map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Pick a free port and a new secret, and write them to `path` for the owner alone.
    pub fn create(path: &Path) -> Result<RelayConfig> {
        let port = free_port()?;
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw)
            .map_err(|e| anyhow!("the system random generator failed: {e}"))?;
        let cfg = RelayConfig {
            port,
            secret: hex::encode(raw),
        };
        let dir = path
            .parent()
            .context("relay.json has no parent directory")?;
        crate::secret::create_private_dir(dir)?;
        let tmp = dir.join(format!(".relay.json.{}.tmp", uuid::Uuid::new_v4().simple()));
        let bytes = serde_json::to_vec_pretty(&cfg)?;
        crate::credfile::write_new_private(&tmp, &bytes)
            .with_context(|| format!("creating {}", tmp.display()))?;
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("writing {}", path.display()));
        }
        Ok(cfg)
    }

    /// The address Claude Code sends its requests to.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/r/{}", self.port, self.secret)
    }
}

/// A random port from 49152 to 65535 that nothing listens on right now.
fn free_port() -> Result<u16> {
    for _ in 0..50 {
        let mut raw = [0u8; 2];
        getrandom::fill(&mut raw)
            .map_err(|e| anyhow!("the system random generator failed: {e}"))?;
        let port = 49152 + u16::from_le_bytes(raw) % (65536 - 49152) as u16;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    Err(anyhow!("found no free port after 50 tries"))
}

/// The variables a Claude Code session needs to go through the relay.
pub fn session_env(cfg: &RelayConfig) -> [(&'static str, String); 2] {
    [
        (ENV_BASE_URL, cfg.base_url()),
        (ENV_FIRST_PARTY, "1".to_string()),
    ]
}

/// The relay's config when relay mode is on: `Ok(None)` when it is off, an error when it is on
/// but `relay.json` is missing or unreadable.
///
/// Read-only and whatever the store's schema, for the launchers, which must start quickly and
/// before an upgraded cc-rotate has migrated the store.
pub fn launch_config(data_dir: &Path) -> Result<Option<RelayConfig>> {
    let store = data_dir.join("store.db");
    if !store.exists() {
        return Ok(None);
    }
    if Store::read_setting_any_schema(&store, SETTING_MODE)?.as_deref() != Some("on") {
        return Ok(None);
    }
    let path = data_dir.join("relay.json");
    let cfg = RelayConfig::read(&path)?
        .with_context(|| format!("relay mode is on but {} does not exist", path.display()))?;
    Ok(Some(cfg))
}

/// The variables a new session gets: `Ok(None)` when relay mode is off, an error when it is on
/// but `relay.json` is missing or unreadable.
pub fn launch_env(data_dir: &Path) -> Result<Option<[(&'static str, String); 2]>> {
    Ok(launch_config(data_dir)?.map(|cfg| session_env(&cfg)))
}

/// What a launcher does first: the relay's config when relay mode is on.
/// A failure is written to cc-rotate's log and the session starts without the relay (spec
/// section 10); nothing is printed.
pub fn launch_config_logged(context: &str) -> Option<RelayConfig> {
    let result = crate::Paths::default_data_dir().and_then(|dir| launch_config(&dir));
    match result {
        Ok(cfg) => cfg,
        Err(e) => {
            crate::process::log_error(context, &e);
            None
        }
    }
}

/// Start the relay in the background unless it answers, without waiting for it; a failure is only
/// logged.
pub fn ensure_running(context: &str, cfg: &RelayConfig) {
    if !answers(cfg.port)
        && let Err(e) = spawn_relay()
    {
        crate::process::log_error(context, &e);
    }
}

/// Start `cc-rotate-ctl relay run` in the background, without waiting for it.
///
/// Used by both launchers, so it finds `cc-rotate-ctl` next to the running executable: that is
/// the executable itself in `cc-rotate-ctl`, and its sibling in `cc-rotate`.
pub fn spawn_relay() -> Result<()> {
    let exe = std::env::current_exe().context("finding the running executable")?;
    let name = format!("cc-rotate-ctl{}", std::env::consts::EXE_SUFFIX);
    let ctl = if exe.file_name().is_some_and(|n| *n == *name) {
        exe
    } else {
        exe.with_file_name(name)
    };
    crate::process::spawn_detached(
        &ctl,
        |cmd| {
            cmd.args(["relay", "run"]);
        },
        "starting the relay",
    )
}

/// Whether something accepts a connection on `port` of 127.0.0.1 within 50 ms.
pub fn answers(port: u16) -> bool {
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    use std::time::Duration;
    TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        Duration::from_millis(50),
    )
    .is_ok()
}

/// The version of the relay that answers on `cfg`'s port, from its answer to
/// `GET /r/<secret>/__ccr/version` (`relay_proto::Route::Version`): `None` when nothing answers
/// within a second, or something does but not with a version.
pub fn running_version(cfg: &RelayConfig) -> Option<String> {
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    use std::time::Duration;
    let timeout = Duration::from_secs(1);
    let mut tcp =
        TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, cfg.port)), timeout)
            .ok()?;
    tcp.set_read_timeout(Some(timeout)).ok()?;
    tcp.set_write_timeout(Some(timeout)).ok()?;
    let request = format!(
        "GET /r/{}{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        cfg.secret,
        crate::relay_proto::VERSION_PATH,
        cfg.port
    );
    tcp.write_all(request.as_bytes()).ok()?;
    let mut answer = Vec::new();
    tcp.take(4096).read_to_end(&mut answer).ok()?;
    version_from_answer(&String::from_utf8_lossy(&answer))
}

/// The version in a raw HTTP/1.1 answer: a 200 whose body is one short line of plain text.
fn version_from_answer(answer: &str) -> Option<String> {
    let (head, body) = answer.split_once("\r\n\r\n")?;
    let status = head.lines().next()?;
    if !status.starts_with("HTTP/1.1 200 ") {
        return None;
    }
    let version = body.trim();
    (!version.is_empty() && version.len() <= 64 && !version.contains(char::is_whitespace))
        .then(|| version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AccountIdentity, AccountState};

    fn two_accounts(store: &Store) -> (i64, i64) {
        let a = store
            .insert_account("a", &AccountIdentity::default(), AccountState::Active)
            .unwrap();
        let b = store
            .insert_account("b", &AccountIdentity::default(), AccountState::Parked)
            .unwrap();
        (a, b)
    }

    #[test]
    fn home_off_by_default() {
        let store = Store::open_in_memory().unwrap();
        let (a, _) = two_accounts(&store);
        assert_eq!(home(&store).unwrap(), None);
        assert_eq!(profile_account(&store).unwrap().unwrap().id, a);
    }

    #[test]
    fn home_on() {
        let store = Store::open_in_memory().unwrap();
        let (a, b) = two_accounts(&store);
        set_mode(&store, Some(b)).unwrap();
        assert_eq!(home(&store).unwrap(), Some(b));
        assert_eq!(store.active_account().unwrap().unwrap().id, a);
        assert_eq!(profile_account(&store).unwrap().unwrap().id, b);
        set_mode(&store, None).unwrap();
        assert_eq!(home(&store).unwrap(), None);
        assert_eq!(store.setting(SETTING_HOME).unwrap(), None);
    }

    #[test]
    fn bypass_counted_and_pruned() {
        let store = Store::open_in_memory().unwrap();
        let now = 1_800_000_000_000;
        note_bypass(&store, "s1", now - BYPASS_WINDOW_MS).unwrap();
        note_bypass(&store, "s2", now - 60_000).unwrap();
        note_bypass(&store, "s3", now).unwrap();
        note_bypass(&store, "s3", now).unwrap();
        assert_eq!(bypass_count(&store, now).unwrap(), 2);
        store.prune(now).unwrap();
        assert_eq!(
            store.meta(&format!("{META_BYPASS_PREFIX}s1")).unwrap(),
            None
        );
        assert!(
            store
                .meta(&format!("{META_BYPASS_PREFIX}s2"))
                .unwrap()
                .is_some()
        );
        assert_eq!(bypass_count(&store, now + BYPASS_WINDOW_MS).unwrap(), 0);
    }

    #[test]
    fn stale_counted_and_pruned() {
        let store = Store::open_in_memory().unwrap();
        let now = 1_800_000_000_000;
        note_stale(&store, "s1", now - BYPASS_WINDOW_MS).unwrap();
        note_stale(&store, "s2", now - 60_000).unwrap();
        note_stale(&store, "s3", now).unwrap();
        note_stale(&store, "s3", now).unwrap();
        // Separate from the sessions that bypass the relay.
        note_bypass(&store, "s4", now).unwrap();
        assert_eq!(stale_count(&store, now).unwrap(), 2);
        assert_eq!(bypass_count(&store, now).unwrap(), 1);
        store.prune(now).unwrap();
        assert_eq!(store.meta(&format!("{META_STALE_PREFIX}s1")).unwrap(), None);
        assert!(
            store
                .meta(&format!("{META_STALE_PREFIX}s2"))
                .unwrap()
                .is_some()
        );
        assert_eq!(stale_count(&store, now + BYPASS_WINDOW_MS).unwrap(), 0);
    }

    #[test]
    fn relay_address_rule() {
        let cfg = RelayConfig {
            port: 50_123,
            secret: "ab".repeat(32),
        };
        assert!(points_at_relay(Some(&cfg.base_url())));
        assert!(points_at_relay(Some("http://127.0.0.1:1/r/x")));
        for other in [
            None,
            Some(""),
            Some("https://api.anthropic.com"),
            Some("http://127.0.0.1:50123/v1"),
            Some("http://127.0.0.1:/r/x"),
            Some("http://127.0.0.1:5x/r/x"),
            Some("http://localhost:50123/r/x"),
            Some("https://127.0.0.1:50123/r/x"),
        ] {
            assert!(!points_at_relay(other), "{other:?}");
        }
    }

    #[test]
    fn bypass_rule() {
        let cfg = RelayConfig {
            port: 50000,
            secret: "s".repeat(64),
        };
        let url = cfg.base_url();
        assert!(!bypasses(Some(&cfg), Some(&url)));
        assert!(bypasses(Some(&cfg), None));
        assert!(bypasses(Some(&cfg), Some("https://api.anthropic.com")));
        assert!(bypasses(None, None));
        assert!(!bypasses(None, Some(&url)));
    }

    #[test]
    fn config_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("relay.json");
        assert_eq!(RelayConfig::read(&path).unwrap(), None);
        let cfg = RelayConfig::create(&path).unwrap();
        assert!((49152..=65535).contains(&cfg.port));
        assert_eq!(cfg.secret.len(), 64);
        assert!(cfg.secret.chars().all(|c| c.is_ascii_hexdigit()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(RelayConfig::read(&path).unwrap(), Some(cfg.clone()));
        assert_eq!(
            cfg.base_url(),
            format!("http://127.0.0.1:{}/r/{}", cfg.port, cfg.secret)
        );
    }

    #[test]
    fn session_env_values() {
        let cfg = RelayConfig {
            port: 50000,
            secret: "ab".repeat(32),
        };
        assert_eq!(
            session_env(&cfg),
            [
                ("ANTHROPIC_BASE_URL", cfg.base_url()),
                ("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL", "1".to_string()),
            ]
        );
    }

    fn data_dir_with_store(mode_on: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("store.db")).unwrap();
        if mode_on {
            let (a, _) = two_accounts(&store);
            set_mode(&store, Some(a)).unwrap();
        }
        dir
    }

    #[test]
    fn launch_env_off() {
        let none = tempfile::tempdir().unwrap();
        assert_eq!(launch_env(none.path()).unwrap(), None);
        let dir = data_dir_with_store(false);
        assert_eq!(launch_env(dir.path()).unwrap(), None);
    }

    #[test]
    fn launch_env_on() {
        let dir = data_dir_with_store(true);
        let cfg = RelayConfig::create(&dir.path().join("relay.json")).unwrap();
        assert_eq!(launch_env(dir.path()).unwrap(), Some(session_env(&cfg)));
    }

    #[test]
    fn launch_env_missing_config() {
        let dir = data_dir_with_store(true);
        assert!(launch_env(dir.path()).is_err());
    }

    mod turning {
        use super::super::*;
        use crate::store::AccountState;
        use crate::swap::tests::{Env, oauth};
        use crate::swap::{self, META_SWAP_PENDING, PendingSwap};

        #[test]
        fn turn_on_writes_home() {
            let env = Env::new();
            env.activate(env.b);
            // Claude Code renewed b's login in the profile since it was stored.
            env.write_profile(&oauth("b2"));
            turn_on(&env.store, &env.paths, env.a).unwrap();
            assert_eq!(env.profile_refresh(), "refresh-a1");
            assert_eq!(env.stored_refresh(env.b), "refresh-b2");
            assert_eq!(env.state(env.b), AccountState::Active);
            assert_eq!(env.state(env.a), AccountState::Parked);
            assert_eq!(home(&env.store).unwrap(), Some(env.a));
            assert!(swap::pending_swap(&env.store).unwrap().is_none());
            assert!(!env.paths.refresh_lock().exists());
            assert!(!env.paths.swap_lock().exists());
        }

        #[test]
        fn turn_on_with_home_active_only_sets_the_mode() {
            let env = Env::new();
            let before = env.profile_bytes();
            turn_on(&env.store, &env.paths, env.a).unwrap();
            assert_eq!(home(&env.store).unwrap(), Some(env.a));
            assert_eq!(env.profile_bytes(), before);
            assert_eq!(env.state(env.a), AccountState::Active);
        }

        #[test]
        fn turn_off_writes_active() {
            let env = Env::new();
            set_mode(&env.store, Some(env.a)).unwrap();
            env.activate(env.c);
            // Claude Code renewed the home account's login in the profile.
            env.write_profile(&oauth("a2"));
            turn_off(&env.store, &env.paths).unwrap();
            assert_eq!(env.profile_refresh(), "refresh-c1");
            assert_eq!(env.stored_refresh(env.a), "refresh-a2");
            assert_eq!(home(&env.store).unwrap(), None);
            assert_eq!(env.state(env.c), AccountState::Active);
            assert!(swap::pending_swap(&env.store).unwrap().is_none());
        }

        #[test]
        fn turn_off_with_home_active_only_sets_the_mode() {
            let env = Env::new();
            set_mode(&env.store, Some(env.a)).unwrap();
            let before = env.profile_bytes();
            turn_off(&env.store, &env.paths).unwrap();
            assert_eq!(home(&env.store).unwrap(), None);
            assert_eq!(env.profile_bytes(), before);
        }

        #[test]
        fn turn_on_refused_when_pending() {
            let env = Env::new();
            env.activate(env.b);
            let pending = PendingSwap {
                fp: "x".into(),
                from: Some(env.b),
                to: env.c,
                at: crate::time::now_ms(),
                relay: None,
            };
            env.store
                .set_meta(META_SWAP_PENDING, serde_json::to_string(&pending).unwrap())
                .unwrap();
            assert!(turn_on(&env.store, &env.paths, env.a).is_err());
            assert_eq!(home(&env.store).unwrap(), None);
            env.write_profile(&oauth("b1"));
            assert!(
                turn_off(&env.store, &env.paths).is_ok(),
                "off while off is nothing"
            );
        }

        #[test]
        fn turn_on_refuses_a_signed_out_or_held_back_home() {
            let env = Env::new();
            env.activate(env.b);
            env.store
                .set_state(env.a, AccountState::NeedsLogin)
                .unwrap();
            assert!(turn_on(&env.store, &env.paths, env.a).is_err());
            env.store.set_state(env.a, AccountState::Parked).unwrap();
            crate::usage_check::hold_back(&env.store, env.a).unwrap();
            assert!(turn_on(&env.store, &env.paths, env.a).is_err());
            assert!(turn_on(&env.store, &env.paths, 9999).is_err());
            assert_eq!(home(&env.store).unwrap(), None);
            assert_eq!(env.profile_refresh(), "refresh-a1");
        }

        #[test]
        fn turn_on_refused_while_home_is_locked() {
            let env = Env::new();
            env.activate(env.b);
            env.write_profile(&oauth("b2"));
            env.store
                .set_setting("lock_timeout_ms", Some("200"))
                .unwrap();
            // A check of the home account is renewing its login.
            let _check = crate::lock::DirLock::acquire(
                &env.paths.account_lock(env.a),
                crate::lock::LockOptions::claude(std::time::Duration::ZERO),
            )
            .unwrap();
            let before = env.profile_bytes();
            assert!(turn_on(&env.store, &env.paths, env.a).is_err());
            assert_eq!(env.profile_bytes(), before);
            assert_eq!(env.stored_refresh(env.b), "refresh-b1", "nothing saved");
            assert_eq!(home(&env.store).unwrap(), None);
            assert!(swap::pending_swap(&env.store).unwrap().is_none());
            assert!(!env.paths.refresh_lock().exists());
        }

        #[test]
        fn on_refuses_signed_out() {
            // Refused before anything changes: no relay.json, no service, no mode.
            let env = Env::new();
            env.activate(env.b);
            let label = |id| env.store.account(id).unwrap().unwrap().label;
            env.store
                .set_state(env.a, AccountState::NeedsLogin)
                .unwrap();
            env.store.set_state(env.c, AccountState::Parked).unwrap();
            crate::usage_check::hold_back(&env.store, env.c).unwrap();
            let before = env.profile_bytes();
            let installs = std::cell::Cell::new(0);
            for (reference, why) in [
                (label(env.a), "signed out"),
                (label(env.c), "on hold"),
                ("nobody".to_string(), "no account matches"),
            ] {
                let err = on(&env.store, &env.paths, &reference, || {
                    installs.set(installs.get() + 1);
                    Ok(())
                })
                .unwrap_err();
                assert!(format!("{err:#}").contains(why), "{err:#}");
            }
            assert_eq!(installs.get(), 0);
            assert!(!env.paths.relay_config().exists());
            assert_eq!(home(&env.store).unwrap(), None);
            assert_eq!(env.profile_bytes(), before);
        }

        #[test]
        fn on_writes_config_installs_and_turns_on() {
            let env = Env::new();
            env.activate(env.b);
            let label = env.store.account(env.a).unwrap().unwrap().label;
            let installs = std::cell::Cell::new(0);
            let (account, cfg) = on(&env.store, &env.paths, &label, || {
                installs.set(installs.get() + 1);
                Ok(())
            })
            .unwrap();
            assert_eq!(account.id, env.a);
            assert_eq!(installs.get(), 1);
            assert_eq!(
                RelayConfig::read(&env.paths.relay_config()).unwrap(),
                Some(cfg.clone())
            );
            assert_eq!(home(&env.store).unwrap(), Some(env.a));
            assert_eq!(env.profile_refresh(), "refresh-a1");

            // Again, with another home: the same port and secret, so earlier sessions still
            // reach the relay.
            let other = env.store.account(env.b).unwrap().unwrap().label;
            let (_, again) = on(&env.store, &env.paths, &other, || Ok(())).unwrap();
            assert_eq!(again, cfg);
            assert_eq!(home(&env.store).unwrap(), Some(env.b));
        }

        #[test]
        fn on_stops_when_the_service_does_not_install() {
            let env = Env::new();
            let label = env.store.account(env.a).unwrap().unwrap().label;
            let err = on(&env.store, &env.paths, &label, || {
                Err(anyhow!("launchd said no"))
            })
            .unwrap_err();
            assert!(format!("{err:#}").contains("launchd said no"), "{err:#}");
            assert_eq!(home(&env.store).unwrap(), None);
        }

        #[test]
        fn turn_on_moves_home() {
            // Already on with home a and c active: the new home b's login replaces a's.
            let env = Env::new();
            set_mode(&env.store, Some(env.a)).unwrap();
            env.activate(env.c);
            env.write_profile(&oauth("a2"));
            turn_on(&env.store, &env.paths, env.b).unwrap();
            assert_eq!(env.profile_refresh(), "refresh-b1");
            assert_eq!(env.stored_refresh(env.a), "refresh-a2");
            assert_eq!(home(&env.store).unwrap(), Some(env.b));
            assert_eq!(env.state(env.c), AccountState::Active);
        }
    }

    #[test]
    fn answers_when_something_listens() {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(answers(port));
        drop(l);
        assert!(!answers(port));
    }

    #[test]
    fn running_version_asks_with_the_secret() {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let cfg = RelayConfig {
            port: l.local_addr().unwrap().port(),
            secret: "s".repeat(64),
        };
        let expected = format!("GET /r/{}/__ccr/version HTTP/1.1\r\n", cfg.secret);
        let server = std::thread::spawn(move || {
            let (mut tcp, _) = l.accept().unwrap();
            let mut buf = [0u8; 1024];
            let n = tcp.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            tcp.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 6\r\n\r\n1.2.3\n",
            )
            .unwrap();
            request
        });
        assert_eq!(running_version(&cfg).as_deref(), Some("1.2.3"));
        let request = server.join().unwrap();
        assert!(request.starts_with(&expected), "{request}");
        assert!(request.contains(&format!("Host: 127.0.0.1:{}\r\n", cfg.port)));
    }

    #[test]
    fn version_answers() {
        assert_eq!(
            version_from_answer("HTTP/1.1 200 OK\r\nx: y\r\n\r\n0.1.0").as_deref(),
            Some("0.1.0")
        );
        assert_eq!(version_from_answer("HTTP/1.1 404 Not Found\r\n\r\n"), None);
        assert_eq!(version_from_answer("HTTP/1.1 200 OK\r\n\r\n"), None);
        assert_eq!(
            version_from_answer("HTTP/1.1 200 OK\r\n\r\n<html> page"),
            None
        );
        assert_eq!(version_from_answer("garbage"), None);
    }
}
