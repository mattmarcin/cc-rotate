//! The SQLite store: accounts, encrypted logins, usage samples, events, settings.
//!
//! Many short-lived processes (a statusline tick per session after every reply and every 30
//! seconds) open the store concurrently, so it runs in WAL mode with a busy timeout, and every
//! multi-row change goes through an IMMEDIATE transaction.

use std::cell::OnceCell;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde_json::Value;

use crate::secret::{self, Key};
use crate::{credfile, time};

/// 1: initial schema, logins sealed with Windows DPAPI.
/// 2: logins sealed with the platform-independent format in `secret`.
const SCHEMA_VERSION: i64 = 2;

const SCHEMA_V1: &str = r#"
CREATE TABLE accounts (
    id                INTEGER PRIMARY KEY,
    label             TEXT NOT NULL UNIQUE,
    email             TEXT,
    account_uuid      TEXT UNIQUE,
    org_uuid          TEXT,
    org_name          TEXT,
    subscription_type TEXT,
    rate_limit_tier   TEXT,
    profile_account   TEXT,
    state             TEXT NOT NULL CHECK (state IN ('active', 'parked', 'needs_login', 'paused')),
    priority          INTEGER NOT NULL DEFAULT 0,
    added_at          INTEGER NOT NULL,
    last_login_at     INTEGER
);
CREATE UNIQUE INDEX accounts_one_active ON accounts (state) WHERE state = 'active';

CREATE TABLE credentials (
    account_id         INTEGER PRIMARY KEY REFERENCES accounts (id) ON DELETE CASCADE,
    oauth_blob         BLOB NOT NULL,
    refresh_fp         TEXT NOT NULL,
    expires_at         INTEGER,
    refresh_expires_at INTEGER,
    updated_at         INTEGER NOT NULL,
    source             TEXT NOT NULL
);

CREATE TABLE samples (
    id             INTEGER PRIMARY KEY,
    ts             INTEGER NOT NULL,
    account_id     INTEGER NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    profile        TEXT NOT NULL,
    session_id     TEXT,
    five_pct       REAL,
    five_resets_at INTEGER,
    week_pct       REAL,
    week_resets_at INTEGER
);
CREATE INDEX samples_account_ts ON samples (account_id, ts);
CREATE INDEX samples_ts ON samples (ts);

CREATE TABLE events (
    id           INTEGER PRIMARY KEY,
    ts           INTEGER NOT NULL,
    kind         TEXT NOT NULL,
    account_from INTEGER,
    account_to   INTEGER,
    reason       TEXT,
    detail       TEXT
);
CREATE INDEX events_ts ON events (ts);
CREATE INDEX events_kind_ts ON events (kind, ts);

CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    Active,
    Parked,
    NeedsLogin,
    Paused,
}

impl AccountState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Parked => "parked",
            Self::NeedsLogin => "needs_login",
            Self::Paused => "paused",
        }
    }
}

impl FromStr for AccountState {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "active" => Self::Active,
            "parked" => Self::Parked,
            "needs_login" => Self::NeedsLogin,
            "paused" => Self::Paused,
            other => bail!("unknown account state {other:?}"),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Account {
    pub id: i64,
    pub label: String,
    pub email: Option<String>,
    pub account_uuid: Option<String>,
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    pub state: AccountState,
    pub priority: i64,
    pub added_at: i64,
    pub last_login_at: Option<i64>,
}

const ACCOUNT_COLS: &str = "id, label, email, account_uuid, org_uuid, org_name, subscription_type, \
    rate_limit_tier, state, priority, added_at, last_login_at";

fn account_from_row(row: &Row) -> rusqlite::Result<Account> {
    let state: String = row.get(8)?;
    Ok(Account {
        id: row.get(0)?,
        label: row.get(1)?,
        email: row.get(2)?,
        account_uuid: row.get(3)?,
        org_uuid: row.get(4)?,
        org_name: row.get(5)?,
        subscription_type: row.get(6)?,
        rate_limit_tier: row.get(7)?,
        state: state.parse().map_err(|e: anyhow::Error| {
            rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, e.into())
        })?,
        priority: row.get(9)?,
        added_at: row.get(10)?,
        last_login_at: row.get(11)?,
    })
}

/// Fields for creating or refreshing an account's identity.
#[derive(Debug, Clone, Default)]
pub struct AccountIdentity {
    pub email: Option<String>,
    pub account_uuid: Option<String>,
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
    pub profile_account: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct StoredCredential {
    pub oauth: Value,
    pub refresh_fp: String,
    pub expires_at: Option<i64>,
    pub refresh_expires_at: Option<i64>,
    pub updated_at: i64,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct CredentialMeta {
    pub refresh_fp: String,
    pub expires_at: Option<i64>,
    pub refresh_expires_at: Option<i64>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sample {
    pub ts: i64,
    pub account_id: i64,
    pub profile: String,
    pub session_id: Option<String>,
    pub five_pct: Option<f64>,
    pub five_resets_at: Option<i64>,
    pub week_pct: Option<f64>,
    pub week_resets_at: Option<i64>,
}

fn sample_from_row(row: &Row) -> rusqlite::Result<Sample> {
    Ok(Sample {
        ts: row.get(0)?,
        account_id: row.get(1)?,
        profile: row.get(2)?,
        session_id: row.get(3)?,
        five_pct: row.get(4)?,
        five_resets_at: row.get(5)?,
        week_pct: row.get(6)?,
        week_resets_at: row.get(7)?,
    })
}

const SAMPLE_COLS: &str =
    "ts, account_id, profile, session_id, five_pct, five_resets_at, week_pct, week_resets_at";

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub id: i64,
    pub ts: i64,
    pub kind: String,
    pub account_from: Option<i64>,
    pub account_to: Option<i64>,
    pub reason: Option<String>,
    pub detail: Option<Value>,
}

/// Event kinds written to the `events` table.
pub mod kind {
    pub const SWAP: &str = "swap";
    pub const SWAP_SKIPPED: &str = "swap_skipped";
    pub const LOGIN: &str = "login";
    pub const SYNC: &str = "sync";
    pub const DEAD_LOGIN: &str = "dead_login";
    pub const POOL_EXHAUSTED: &str = "pool_exhausted";
    pub const FOREIGN_LOGIN: &str = "foreign_login";
    pub const ADOPTED_LOGIN: &str = "adopted_login";
    pub const STATE: &str = "state";
    pub const ERROR: &str = "error";
}

/// Typed view of the `settings` table with defaults applied.
#[derive(Debug, Clone, Serialize)]
pub struct Settings {
    pub swap_threshold_5h: f64,
    pub swap_threshold_7d: f64,
    pub swap_cooldown_s: i64,
    pub lock_timeout_ms: u64,
    pub sample_retention_days: i64,
    pub statusline_downstream: Option<String>,
    pub profile_dir: Option<String>,
    pub claude_path: Option<String>,
}

pub const SETTING_KEYS: &[&str] = &[
    "swap_threshold_5h",
    "swap_threshold_7d",
    "swap_cooldown_s",
    "lock_timeout_ms",
    "sample_retention_days",
    "statusline_downstream",
    "profile_dir",
    "claude_path",
];

pub struct Store {
    conn: Connection,
    /// `<data dir>/master.key`; `None` for in-memory stores, whose key is never written.
    key_path: Option<PathBuf>,
    /// Loaded on first use, so ticks that never touch a login never read the key file.
    key: OnceCell<Key>,
}

impl Store {
    /// Open (or create) the store. Its encryption key is `master.key` next to it.
    pub fn open(path: &Path) -> Result<Store> {
        let dir = path
            .parent()
            .context("the store path has no parent directory")?;
        secret::create_private_dir(dir)?;
        let conn =
            Connection::open(path).with_context(|| format!("opening store {}", path.display()))?;
        Self::init(conn, Some(secret::key_path(dir)), OnceCell::new())
    }

    pub fn open_in_memory() -> Result<Store> {
        Self::init(
            Connection::open_in_memory()?,
            None,
            OnceCell::from(Key::ephemeral()?),
        )
    }

    fn init(conn: Connection, key_path: Option<PathBuf>, key: OnceCell<Key>) -> Result<Store> {
        conn.busy_timeout(Duration::from_millis(5000))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Store {
            conn,
            key_path,
            key,
        };
        store.migrate()?;
        Ok(store)
    }

    /// Where the encryption key lives (`None` for an in-memory store).
    pub fn key_path(&self) -> Option<&Path> {
        self.key_path.as_deref()
    }

    /// The encryption key, read from `master.key` or created there on first use.
    pub fn key(&self) -> Result<&Key> {
        if let Some(key) = self.key.get() {
            return Ok(key);
        }
        let path = self
            .key_path
            .as_deref()
            .context("this store has no key file")?;
        let key = match secret::read_key(path)? {
            Some(key) => key,
            // A new key cannot open what an earlier one sealed. Say so, instead of creating
            // one that leaves every stored login unreadable without an explanation.
            None if self.has_sealed_credentials()? => bail!(
                "{} is missing, so the stored logins cannot be decrypted; restore that file, or remove the accounts and sign them in again",
                path.display()
            ),
            None => secret::load_or_create_key(path)?,
        };
        Ok(self.key.get_or_init(|| key))
    }

    fn has_sealed_credentials(&self) -> Result<bool> {
        Ok(self
            .credential_blobs()?
            .iter()
            .any(|(_, blob)| secret::is_sealed(blob)))
    }

    fn credential_blobs(&self) -> Result<Vec<(i64, Vec<u8>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT account_id, oauth_blob FROM credentials")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }
        self.immediate(|s| {
            // Re-check under the write lock: another process may have migrated meanwhile.
            let version: i64 = s
                .conn
                .pragma_query_value(None, "user_version", |r| r.get(0))?;
            if version < 1 {
                s.conn.execute_batch(SCHEMA_V1)?;
                s.conn.pragma_update(None, "user_version", 1)?;
            }
            if version < 2 {
                s.convert_legacy_credentials()?;
                s.conn.pragma_update(None, "user_version", 2)?;
            }
            Ok(())
        })
    }

    /// Schema 2: re-seal every login an older cc-rotate stored with Windows DPAPI, in the same
    /// transaction as the version bump. A login that cannot be converted is left as it is and
    /// logged; reading it later says that the account needs a new sign-in.
    fn convert_legacy_credentials(&self) -> Result<()> {
        for (account_id, blob) in self.credential_blobs()? {
            if secret::is_sealed(&blob) {
                continue;
            }
            match legacy_unseal(&blob) {
                Ok(plain) => {
                    let sealed = self
                        .key()?
                        .seal(&plain, &secret::credential_aad(account_id))?;
                    self.conn.execute(
                        "UPDATE credentials SET oauth_blob = ?2 WHERE account_id = ?1",
                        params![account_id, sealed],
                    )?;
                }
                Err(e) => {
                    self.log_event(
                        kind::ERROR,
                        Some(account_id),
                        None,
                        Some("store_upgrade"),
                        Some(serde_json::json!({
                            "message": format!("could not convert this account's stored login ({e:#}); sign it in again"),
                        })),
                    )?;
                }
            }
        }
        #[cfg(windows)]
        if let Some(dir) = self.key_path.as_deref().and_then(Path::parent) {
            let legacy = crate::swap::legacy_backups(&dir.join("backups"));
            if !legacy.is_empty() {
                crate::swap::convert_legacy_backups(&legacy, self.key()?);
            }
        }
        Ok(())
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Run `f` inside `BEGIN IMMEDIATE`, committing on success and rolling back on error.
    pub fn immediate<T>(&self, f: impl FnOnce(&Store) -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f(self) {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Changes whenever another connection commits; cheap to poll.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "data_version", |r| r.get(0))?)
    }

    // ---- settings and meta -------------------------------------------------------------

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: Option<&str>) -> Result<()> {
        if !SETTING_KEYS.contains(&key) {
            bail!(
                "unknown setting {key:?}; known settings: {}",
                SETTING_KEYS.join(", ")
            );
        }
        match value {
            Some(v) => {
                validate_setting(key, v)?;
                self.conn.execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2)
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                    params![key, v],
                )?;
            }
            None => {
                self.conn
                    .execute("DELETE FROM settings WHERE key = ?1", [key])?;
            }
        }
        Ok(())
    }

    pub fn settings(&self) -> Result<Settings> {
        let get = |k: &str| self.setting(k);
        let num = |k: &str, d: f64| -> Result<f64> {
            Ok(get(k)?.and_then(|v| v.parse().ok()).unwrap_or(d))
        };
        let int = |k: &str, d: i64| -> Result<i64> {
            Ok(get(k)?.and_then(|v| v.parse().ok()).unwrap_or(d))
        };
        let text = |k: &str| -> Result<Option<String>> { Ok(get(k)?.filter(|v| !v.is_empty())) };
        Ok(Settings {
            swap_threshold_5h: num("swap_threshold_5h", 98.0)?,
            swap_threshold_7d: num("swap_threshold_7d", 98.0)?,
            swap_cooldown_s: int("swap_cooldown_s", 120)?,
            lock_timeout_ms: int("lock_timeout_ms", 30_000)? as u64,
            sample_retention_days: int("sample_retention_days", 90)?,
            statusline_downstream: text("statusline_downstream")?,
            profile_dir: text("profile_dir")?,
            claude_path: text("claude_path")?,
        })
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn meta_i64(&self, key: &str) -> Result<Option<i64>> {
        Ok(self.meta(key)?.and_then(|v| v.parse().ok()))
    }

    pub fn set_meta(&self, key: &str, value: impl ToString) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value.to_string()],
        )?;
        Ok(())
    }

    // ---- accounts ----------------------------------------------------------------------

    pub fn accounts(&self) -> Result<Vec<Account>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ACCOUNT_COLS} FROM accounts ORDER BY priority, label"
        ))?;
        let rows = stmt.query_map([], account_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn account(&self, id: i64) -> Result<Option<Account>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {ACCOUNT_COLS} FROM accounts WHERE id = ?1"),
                [id],
                account_from_row,
            )
            .optional()?)
    }

    pub fn account_by_label(&self, label: &str) -> Result<Option<Account>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {ACCOUNT_COLS} FROM accounts WHERE label = ?1 COLLATE NOCASE"),
                [label],
                account_from_row,
            )
            .optional()?)
    }

    pub fn account_by_uuid(&self, uuid: &str) -> Result<Option<Account>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {ACCOUNT_COLS} FROM accounts WHERE account_uuid = ?1"),
                [uuid],
                account_from_row,
            )
            .optional()?)
    }

    pub fn account_by_email(&self, email: &str) -> Result<Option<Account>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {ACCOUNT_COLS} FROM accounts WHERE email = ?1 COLLATE NOCASE"),
                [email],
                account_from_row,
            )
            .optional()?)
    }

    /// Resolve a user-supplied reference: a label, an email, or a numeric id.
    pub fn find_account(&self, reference: &str) -> Result<Account> {
        if let Some(a) = self.account_by_label(reference)? {
            return Ok(a);
        }
        if let Some(a) = self.account_by_email(reference)? {
            return Ok(a);
        }
        if let Ok(id) = reference.parse::<i64>() {
            if let Some(a) = self.account(id)? {
                return Ok(a);
            }
        }
        Err(anyhow!("no account matches {reference:?}"))
    }

    pub fn active_account(&self) -> Result<Option<Account>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {ACCOUNT_COLS} FROM accounts WHERE state = 'active'"),
                [],
                account_from_row,
            )
            .optional()?)
    }

    pub fn insert_account(
        &self,
        label: &str,
        ident: &AccountIdentity,
        state: AccountState,
    ) -> Result<i64> {
        let now = time::now_ms();
        self.conn.execute(
            "INSERT INTO accounts (label, email, account_uuid, org_uuid, org_name, subscription_type,
                                   rate_limit_tier, profile_account, state, added_at, last_login_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
            params![
                label,
                ident.email,
                ident.account_uuid,
                ident.org_uuid,
                ident.org_name,
                ident.subscription_type,
                ident.rate_limit_tier,
                ident.profile_account.as_ref().map(Value::to_string),
                state.as_str(),
                now,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Refresh identity fields after a login; `None` fields keep their stored value.
    pub fn update_identity(&self, id: i64, ident: &AccountIdentity) -> Result<()> {
        self.conn.execute(
            "UPDATE accounts SET
                email             = COALESCE(?2, email),
                account_uuid      = COALESCE(?3, account_uuid),
                org_uuid          = COALESCE(?4, org_uuid),
                org_name          = COALESCE(?5, org_name),
                subscription_type = COALESCE(?6, subscription_type),
                rate_limit_tier   = COALESCE(?7, rate_limit_tier),
                profile_account   = COALESCE(?8, profile_account),
                last_login_at     = ?9
             WHERE id = ?1",
            params![
                id,
                ident.email,
                ident.account_uuid,
                ident.org_uuid,
                ident.org_name,
                ident.subscription_type,
                ident.rate_limit_tier,
                ident.profile_account.as_ref().map(Value::to_string),
                time::now_ms(),
            ],
        )?;
        Ok(())
    }

    pub fn profile_account_json(&self, id: i64) -> Result<Option<Value>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT profile_account FROM accounts WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(raw.and_then(|s| serde_json::from_str(&s).ok()))
    }

    pub fn set_state(&self, id: i64, state: AccountState) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE accounts SET state = ?2 WHERE id = ?1",
            params![id, state.as_str()],
        )?;
        if n == 0 {
            bail!("account {id} does not exist");
        }
        Ok(())
    }

    pub fn set_priority(&self, id: i64, priority: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE accounts SET priority = ?2 WHERE id = ?1",
            params![id, priority],
        )?;
        Ok(())
    }

    pub fn rename_account(&self, id: i64, label: &str) -> Result<()> {
        validate_label(label)?;
        self.conn.execute(
            "UPDATE accounts SET label = ?2 WHERE id = ?1",
            params![id, label],
        )?;
        Ok(())
    }

    pub fn remove_account(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM accounts WHERE id = ?1", [id])?;
        Ok(())
    }

    /// A label derived from `base` that no account uses yet.
    pub fn unique_label(&self, base: &str) -> Result<String> {
        let base = sanitize_label(base);
        if self.account_by_label(&base)?.is_none() {
            return Ok(base);
        }
        for n in 2.. {
            let candidate = format!("{base}-{n}");
            if self.account_by_label(&candidate)?.is_none() {
                return Ok(candidate);
            }
        }
        unreachable!()
    }

    // ---- credentials -------------------------------------------------------------------

    pub fn save_credential(&self, account_id: i64, oauth: &Value, source: &str) -> Result<()> {
        let fp = credfile::oauth_fingerprint(oauth)
            .ok_or_else(|| anyhow!("refusing to store a login without a refresh token"))?;
        let sealed = self.key()?.seal(
            serde_json::to_vec(oauth)?.as_slice(),
            &secret::credential_aad(account_id),
        )?;
        let expires = oauth.get("expiresAt").and_then(Value::as_i64);
        let refresh_expires = oauth.get("refreshTokenExpiresAt").and_then(Value::as_i64);
        self.conn.execute(
            "INSERT INTO credentials (account_id, oauth_blob, refresh_fp, expires_at, refresh_expires_at, updated_at, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (account_id) DO UPDATE SET
                oauth_blob = excluded.oauth_blob, refresh_fp = excluded.refresh_fp,
                expires_at = excluded.expires_at, refresh_expires_at = excluded.refresh_expires_at,
                updated_at = excluded.updated_at, source = excluded.source",
            params![account_id, sealed, fp, expires, refresh_expires, time::now_ms(), source],
        )?;
        Ok(())
    }

    pub fn credential(&self, account_id: i64) -> Result<Option<StoredCredential>> {
        let row = self
            .conn
            .query_row(
                "SELECT oauth_blob, refresh_fp, expires_at, refresh_expires_at, updated_at, source
                 FROM credentials WHERE account_id = ?1",
                [account_id],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((blob, refresh_fp, expires_at, refresh_expires_at, updated_at, source)) = row
        else {
            return Ok(None);
        };
        if !secret::is_sealed(&blob) {
            bail!(
                "the stored login for account {account_id} was saved by an older cc-rotate and could not be converted; sign it in again with `cc-rotate-ctl login --relogin`"
            );
        }
        let plain = self
            .key()?
            .open(&blob, &secret::credential_aad(account_id))
            .with_context(|| format!("decrypting the stored login for account {account_id}"))?;
        Ok(Some(StoredCredential {
            oauth: serde_json::from_slice(&plain)?,
            refresh_fp,
            expires_at,
            refresh_expires_at,
            updated_at,
            source,
        }))
    }

    /// Fingerprint and expiry metadata without decrypting.
    pub fn credential_meta(&self, account_id: i64) -> Result<Option<CredentialMeta>> {
        Ok(self
            .conn
            .query_row(
                "SELECT refresh_fp, expires_at, refresh_expires_at, updated_at FROM credentials WHERE account_id = ?1",
                [account_id],
                |r| {
                    Ok(CredentialMeta {
                        refresh_fp: r.get(0)?,
                        expires_at: r.get(1)?,
                        refresh_expires_at: r.get(2)?,
                        updated_at: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    // ---- samples -----------------------------------------------------------------------

    /// Insert a sample unless it repeats the account's last one within a minute.
    /// Returns whether a row was written.
    pub fn record_sample(&self, s: &Sample) -> Result<bool> {
        if let Some(last) = self.last_sample(s.account_id)? {
            let close = |a: Option<f64>, b: Option<f64>| match (a, b) {
                (Some(a), Some(b)) => (a - b).abs() < 0.5,
                (None, None) => true,
                _ => false,
            };
            let same = close(last.five_pct, s.five_pct)
                && close(last.week_pct, s.week_pct)
                && last.five_resets_at == s.five_resets_at
                && last.week_resets_at == s.week_resets_at;
            if same && s.ts - last.ts < 60_000 {
                return Ok(false);
            }
        }
        self.conn.execute(
            &format!("INSERT INTO samples ({SAMPLE_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"),
            params![
                s.ts,
                s.account_id,
                s.profile,
                s.session_id,
                s.five_pct,
                s.five_resets_at,
                s.week_pct,
                s.week_resets_at
            ],
        )?;
        Ok(true)
    }

    pub fn last_sample(&self, account_id: i64) -> Result<Option<Sample>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {SAMPLE_COLS} FROM samples WHERE account_id = ?1 ORDER BY ts DESC LIMIT 1"
                ),
                [account_id],
                sample_from_row,
            )
            .optional()?)
    }

    pub fn samples_since(&self, since_ms: i64) -> Result<Vec<Sample>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SAMPLE_COLS} FROM samples WHERE ts >= ?1 ORDER BY ts"
        ))?;
        let rows = stmt.query_map([since_ms], sample_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Drop samples and events older than the retention window, at most once an hour.
    pub fn prune(&self, now_ms: i64) -> Result<()> {
        let last = self.meta_i64("last_prune_at")?.unwrap_or(0);
        if now_ms - last < 3_600_000 {
            return Ok(());
        }
        let days = self.settings()?.sample_retention_days.max(1);
        let cutoff = now_ms - days * 86_400_000;
        self.conn
            .execute("DELETE FROM samples WHERE ts < ?1", [cutoff])?;
        self.conn
            .execute("DELETE FROM events WHERE ts < ?1", [cutoff])?;
        self.set_meta("last_prune_at", now_ms)
    }

    // ---- events ------------------------------------------------------------------------

    pub fn log_event(
        &self,
        kind: &str,
        from: Option<i64>,
        to: Option<i64>,
        reason: Option<&str>,
        detail: Option<Value>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO events (ts, kind, account_from, account_to, reason, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                time::now_ms(),
                kind,
                from,
                to,
                reason,
                detail.map(|d| d.to_string())
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn events(&self, limit: usize) -> Result<Vec<Event>> {
        self.events_query(
            "SELECT id, ts, kind, account_from, account_to, reason, detail FROM events
                           ORDER BY id DESC LIMIT ?1",
            params![limit as i64],
        )
    }

    pub fn events_after(&self, after_id: i64) -> Result<Vec<Event>> {
        self.events_query(
            "SELECT id, ts, kind, account_from, account_to, reason, detail FROM events
                           WHERE id > ?1 ORDER BY id",
            params![after_id],
        )
    }

    pub fn last_event_of(&self, kind: &str) -> Result<Option<Event>> {
        Ok(self
            .events_query(
                "SELECT id, ts, kind, account_from, account_to, reason, detail FROM events
                           WHERE kind = ?1 ORDER BY id DESC LIMIT 1",
                params![kind],
            )?
            .pop())
    }

    fn events_query(&self, sql: &str, p: impl rusqlite::Params) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(p, |r| {
            let detail: Option<String> = r.get(6)?;
            Ok(Event {
                id: r.get(0)?,
                ts: r.get(1)?,
                kind: r.get(2)?,
                account_from: r.get(3)?,
                account_to: r.get(4)?,
                reason: r.get(5)?,
                detail: detail.and_then(|d| serde_json::from_str(&d).ok()),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// Open a login sealed by cc-rotate before schema 2.
#[cfg(windows)]
fn legacy_unseal(blob: &[u8]) -> Result<Vec<u8>> {
    secret::dpapi::unprotect(blob)
}

#[cfg(not(windows))]
fn legacy_unseal(_blob: &[u8]) -> Result<Vec<u8>> {
    bail!("it was sealed with Windows DPAPI, which only Windows can open")
}

fn validate_setting(key: &str, value: &str) -> Result<()> {
    match key {
        "swap_threshold_5h" | "swap_threshold_7d" => {
            let v: f64 = value.parse().context("expected a number")?;
            if !(1.0..=100.0).contains(&v) {
                bail!("{key} must be between 1 and 100");
            }
        }
        "swap_cooldown_s" | "lock_timeout_ms" | "sample_retention_days" => {
            let v: i64 = value.parse().context("expected an integer")?;
            if v < 0 {
                bail!("{key} must not be negative");
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn validate_label(label: &str) -> Result<()> {
    if label.is_empty() || label.len() > 40 {
        bail!("labels must be 1 to 40 characters");
    }
    if !label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("labels may only contain letters, digits, '-', '_' and '.'");
    }
    if label.parse::<i64>().is_ok() {
        bail!("labels cannot be plain numbers");
    }
    Ok(())
}

pub fn sanitize_label(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    let mut label: String = trimmed.chars().take(40).collect();
    if label.is_empty() || label.parse::<i64>().is_ok() {
        label = format!("account-{label}").trim_end_matches('-').to_string();
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ident(uuid: &str) -> AccountIdentity {
        AccountIdentity {
            account_uuid: Some(uuid.into()),
            email: Some(format!("{uuid}@example.com")),
            ..Default::default()
        }
    }

    #[test]
    fn only_one_active() {
        let s = Store::open_in_memory().unwrap();
        let a = s
            .insert_account("a", &ident("ua"), AccountState::Active)
            .unwrap();
        let b = s
            .insert_account("b", &ident("ub"), AccountState::Parked)
            .unwrap();
        assert!(s.set_state(b, AccountState::Active).is_err());
        s.immediate(|s| {
            s.set_state(a, AccountState::Parked)?;
            s.set_state(b, AccountState::Active)
        })
        .unwrap();
        assert_eq!(s.active_account().unwrap().unwrap().id, b);
    }

    #[test]
    fn failed_transaction_rolls_back() {
        let s = Store::open_in_memory().unwrap();
        let a = s
            .insert_account("a", &ident("ua"), AccountState::Active)
            .unwrap();
        let result: Result<()> = s.immediate(|s| {
            s.set_state(a, AccountState::Parked)?;
            bail!("boom")
        });
        assert!(result.is_err());
        assert_eq!(s.account(a).unwrap().unwrap().state, AccountState::Active);
    }

    #[test]
    fn credential_round_trip_is_encrypted() {
        let s = Store::open_in_memory().unwrap();
        let a = s
            .insert_account("a", &ident("ua"), AccountState::Parked)
            .unwrap();
        let oauth = json!({"accessToken": "acc-123", "refreshToken": "ref-456", "expiresAt": 10});
        s.save_credential(a, &oauth, "login").unwrap();
        let blob: Vec<u8> = s
            .conn()
            .query_row(
                "SELECT oauth_blob FROM credentials WHERE account_id = ?1",
                [a],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!String::from_utf8_lossy(&blob).contains("ref-456"));
        let back = s.credential(a).unwrap().unwrap();
        assert_eq!(back.oauth, oauth);
        assert_eq!(back.refresh_fp, credfile::fingerprint("ref-456"));
        assert_eq!(back.expires_at, Some(10));
        assert!(blob.starts_with(secret::MAGIC));
    }

    fn blob_of(s: &Store, account_id: i64) -> Vec<u8> {
        s.conn()
            .query_row(
                "SELECT oauth_blob FROM credentials WHERE account_id = ?1",
                [account_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn a_login_moved_to_another_account_does_not_open() {
        let s = Store::open_in_memory().unwrap();
        let a = s
            .insert_account("a", &ident("ua"), AccountState::Parked)
            .unwrap();
        let b = s
            .insert_account("b", &ident("ub"), AccountState::Parked)
            .unwrap();
        s.save_credential(a, &json!({"refreshToken": "ref-a"}), "login")
            .unwrap();
        s.save_credential(b, &json!({"refreshToken": "ref-b"}), "login")
            .unwrap();
        s.conn()
            .execute(
                "UPDATE credentials SET oauth_blob = ?2 WHERE account_id = ?1",
                params![b, blob_of(&s, a)],
            )
            .unwrap();
        assert!(s.credential(b).is_err());
        assert_eq!(
            s.credential(a).unwrap().unwrap().oauth["refreshToken"],
            "ref-a"
        );
    }

    #[test]
    fn key_file_is_shared_by_every_connection() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("data").join("store.db");
        let first = Store::open(&db).unwrap();
        let a = first
            .insert_account("a", &ident("ua"), AccountState::Parked)
            .unwrap();
        assert!(
            !dir.path().join("data").join(secret::KEY_FILE).exists(),
            "the key is only created when a login is stored"
        );
        first
            .save_credential(a, &json!({"refreshToken": "ref-a"}), "login")
            .unwrap();
        let second = Store::open(&db).unwrap();
        assert_eq!(
            second.credential(a).unwrap().unwrap().oauth["refreshToken"],
            "ref-a"
        );
    }

    #[test]
    fn missing_key_file_is_reported_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("store.db");
        let s = Store::open(&db).unwrap();
        let a = s
            .insert_account("a", &ident("ua"), AccountState::Parked)
            .unwrap();
        s.save_credential(a, &json!({"refreshToken": "ref-a"}), "login")
            .unwrap();
        drop(s);
        let key = dir.path().join(secret::KEY_FILE);
        std::fs::remove_file(&key).unwrap();

        let s = Store::open(&db).unwrap();
        let err = s.credential(a).unwrap_err();
        assert!(format!("{err:#}").contains("is missing"), "{err:#}");
        assert!(
            !key.exists(),
            "no new key that could never open the old logins"
        );
    }

    /// A store written by cc-rotate before schema 2, with logins sealed by Windows DPAPI.
    #[cfg(windows)]
    #[test]
    fn dpapi_store_is_converted_on_first_open() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("store.db");
        let oauth = json!({"accessToken": "acc-1", "refreshToken": "ref-1", "expiresAt": 10});
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute(
                "INSERT INTO accounts (id, label, account_uuid, state, added_at) VALUES (1, 'a', 'ua', 'active', 0), (2, 'b', 'ub', 'parked', 0)",
                [],
            )
            .unwrap();
            let insert = |id: i64, blob: Vec<u8>| {
                conn.execute(
                    "INSERT INTO credentials (account_id, oauth_blob, refresh_fp, updated_at, source) VALUES (?1, ?2, 'fp', 0, 'login')",
                    params![id, blob],
                )
                .unwrap();
            };
            insert(
                1,
                secret::dpapi::protect(&serde_json::to_vec(&oauth).unwrap()).unwrap(),
            );
            // Unreadable: sealed by some other user or machine.
            insert(2, b"not a dpapi blob".to_vec());
        }
        let backups = dir.path().join("backups");
        std::fs::create_dir_all(&backups).unwrap();
        std::fs::write(
            backups.join("credentials-20000.dpapi"),
            secret::dpapi::protect(b"{\"claudeAiOauth\":{}}").unwrap(),
        )
        .unwrap();

        let s = Store::open(&db).unwrap();
        let version: i64 = s
            .conn()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(blob_of(&s, 1).starts_with(secret::MAGIC));
        assert_eq!(s.credential(1).unwrap().unwrap().oauth, oauth);
        let err = s.credential(2).unwrap_err();
        assert!(format!("{err:#}").contains("sign it in again"), "{err:#}");
        let logged = s.last_event_of(kind::ERROR).unwrap().unwrap();
        assert_eq!(logged.account_from, Some(2));

        assert!(!backups.join("credentials-20000.dpapi").exists());
        let converted = backups.join("credentials-20000.ccr");
        let plain = s
            .key()
            .unwrap()
            .open(&std::fs::read(converted).unwrap(), secret::BACKUP_AAD)
            .unwrap();
        assert_eq!(plain, b"{\"claudeAiOauth\":{}}");

        // Opening again changes nothing.
        let before = blob_of(&s, 1);
        drop(s);
        let s = Store::open(&db).unwrap();
        assert_eq!(blob_of(&s, 1), before);
    }

    #[test]
    fn refuses_dead_login() {
        let s = Store::open_in_memory().unwrap();
        let a = s
            .insert_account("a", &ident("ua"), AccountState::Parked)
            .unwrap();
        assert!(
            s.save_credential(a, &json!({"refreshToken": ""}), "login")
                .is_err()
        );
    }

    #[test]
    fn samples_dedupe_within_a_minute() {
        let s = Store::open_in_memory().unwrap();
        let a = s
            .insert_account("a", &ident("ua"), AccountState::Active)
            .unwrap();
        let mk = |ts, pct| Sample {
            ts,
            account_id: a,
            profile: "rotate".into(),
            session_id: None,
            five_pct: Some(pct),
            five_resets_at: Some(100),
            week_pct: Some(10.0),
            week_resets_at: Some(200),
        };
        assert!(s.record_sample(&mk(1_000, 20.0)).unwrap());
        assert!(!s.record_sample(&mk(6_000, 20.2)).unwrap());
        assert!(s.record_sample(&mk(7_000, 21.0)).unwrap());
        assert!(s.record_sample(&mk(70_000, 21.0)).unwrap());
        assert_eq!(s.samples_since(0).unwrap().len(), 3);
    }

    #[test]
    fn settings_defaults_and_validation() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.settings().unwrap().swap_threshold_5h, 98.0);
        s.set_setting("swap_threshold_5h", Some("90")).unwrap();
        assert_eq!(s.settings().unwrap().swap_threshold_5h, 90.0);
        assert!(s.set_setting("swap_threshold_5h", Some("150")).is_err());
        assert!(s.set_setting("nope", Some("1")).is_err());
    }

    #[test]
    fn labels() {
        assert_eq!(sanitize_label("Jane.Doe+work"), "jane.doe-work");
        assert_eq!(sanitize_label("123"), "account-123");
        assert!(validate_label("team-1").is_ok());
        assert!(validate_label("bad label").is_err());
        let s = Store::open_in_memory().unwrap();
        s.insert_account("jane", &ident("u1"), AccountState::Parked)
            .unwrap();
        assert_eq!(s.unique_label("jane").unwrap(), "jane-2");
    }

    #[test]
    fn find_by_label_email_or_id() {
        let s = Store::open_in_memory().unwrap();
        let a = s
            .insert_account("work", &ident("u1"), AccountState::Parked)
            .unwrap();
        assert_eq!(s.find_account("WORK").unwrap().id, a);
        assert_eq!(s.find_account("u1@example.com").unwrap().id, a);
        assert_eq!(s.find_account(&a.to_string()).unwrap().id, a);
        assert!(s.find_account("missing").is_err());
    }
}
