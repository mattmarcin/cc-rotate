//! Relay mode's state: whether it is on, which account is home, and `relay.json`.
//!
//! The design is in `docs/specs/2026-11-10-relay-mode.md`.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::store::{Account, Store};

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
/// the user can change with `config`.
pub fn set_mode(store: &Store, home: Option<i64>) -> Result<()> {
    let conn = store.conn();
    let tx = conn.unchecked_transaction()?;
    match home {
        Some(id) => {
            for (key, value) in [
                (SETTING_MODE, "on".to_string()),
                (SETTING_HOME, id.to_string()),
            ] {
                tx.execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2)
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                    params![key, value],
                )?;
            }
        }
        None => {
            tx.execute(
                "DELETE FROM settings WHERE key IN (?1, ?2)",
                params![SETTING_MODE, SETTING_HOME],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
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
}
