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
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::secret::{self, Key};
use crate::{credfile, time};

/// 1: initial schema, logins sealed with Windows DPAPI.
/// 2: logins sealed with the platform-independent format in `secret`.
/// 3: usage windows as rows (`sample_windows`), the account order (`position`, formerly
///    `priority`) and the per-account weekly limit override.
/// 4: the per-account weekly cap (`weekly_cap`).
/// 5: whether an account's weekly budget is used before it resets (`use_expiring`).
/// 6: sessions stopped at a usage limit (`waiting_sessions`).
/// 7: what each account's use would cost at API prices (`session_costs`, `account_costs`) and
///    its extra usage (`extra_usage`).
/// 8: whether cc-rotate keeps an account's 5-hour window running (`keep_running`).
const SCHEMA_VERSION: i64 = 8;

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

/// Schema 3. Samples used to hold exactly two windows in fixed columns; now every window a data
/// source reports is a row in `sample_windows`, so a new limit system needs no schema change.
/// The two fixed columns become `five_hour` and `seven_day` rows. `priority` keeps its values
/// as the account order (`position`, lower is used first).
const SCHEMA_V3: &str = r#"
ALTER TABLE accounts RENAME COLUMN priority TO position;
ALTER TABLE accounts ADD COLUMN weekly_limit TEXT NOT NULL DEFAULT 'auto'
    CHECK (weekly_limit IN ('auto', 'yes', 'no'));

DROP INDEX samples_account_ts;
DROP INDEX samples_ts;
ALTER TABLE samples RENAME TO samples_v2;
CREATE TABLE samples (
    id         INTEGER PRIMARY KEY,
    ts         INTEGER NOT NULL,
    account_id INTEGER NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    profile    TEXT NOT NULL,
    session_id TEXT,
    model      TEXT
);
CREATE INDEX samples_account_ts ON samples (account_id, ts);
CREATE INDEX samples_ts ON samples (ts);
CREATE TABLE sample_windows (
    sample_id  INTEGER NOT NULL REFERENCES samples (id) ON DELETE CASCADE,
    account_id INTEGER NOT NULL,
    name       TEXT NOT NULL,
    pct        REAL NOT NULL,
    resets_at  INTEGER,
    PRIMARY KEY (sample_id, name)
) WITHOUT ROWID;
CREATE INDEX sample_windows_account ON sample_windows (account_id, name, resets_at);

INSERT INTO samples (id, ts, account_id, profile, session_id)
    SELECT id, ts, account_id, profile, session_id FROM samples_v2;
INSERT INTO sample_windows (sample_id, account_id, name, pct, resets_at)
    SELECT id, account_id, 'five_hour', five_pct, five_resets_at FROM samples_v2
    WHERE five_pct IS NOT NULL;
INSERT INTO sample_windows (sample_id, account_id, name, pct, resets_at)
    SELECT id, account_id, 'seven_day', week_pct, week_resets_at FROM samples_v2
    WHERE week_pct IS NOT NULL;
DROP TABLE samples_v2;
"#;

/// Schema 4: rotation stops using an account once its weekly usage reaches `weekly_cap`, when
/// that is below `swap_threshold_7d`; NULL uses the setting.
const SCHEMA_V4: &str = r#"
ALTER TABLE accounts ADD COLUMN weekly_cap REAL
    CHECK (weekly_cap IS NULL OR (weekly_cap >= 1 AND weekly_cap <= 100));
"#;

/// Schema 5: whether the account moves ahead when its weekly budget would reset unused
/// (`budget::expiring`); on unless the user turned it off for the account.
const SCHEMA_V5: &str = r#"
ALTER TABLE accounts ADD COLUMN use_expiring INTEGER NOT NULL DEFAULT 1
    CHECK (use_expiring IN (0, 1));
"#;

/// Schema 6: one row per session whose turn stopped at a usage limit, kept while the
/// `StopFailure` hook waits to wake it and until it is seen working again (`waiting`).
const SCHEMA_V6: &str = r#"
CREATE TABLE waiting_sessions (
    session_id       TEXT PRIMARY KEY,
    since_ms         INTEGER NOT NULL,
    account_id       INTEGER REFERENCES accounts (id) ON DELETE SET NULL,
    hook_pid         INTEGER,
    hook_started     TEXT,
    state            TEXT NOT NULL
        CHECK (state IN ('waiting', 'woken', 'resumed', 'stuck', 'ended')),
    woken_at         INTEGER,
    woken_account_id INTEGER REFERENCES accounts (id) ON DELETE SET NULL,
    woken_accounts   TEXT NOT NULL DEFAULT '[]',
    updated_at       INTEGER NOT NULL
);
"#;

/// Schema 7. `session_costs` holds the last `cost.total_cost_usd` from the status line of each
/// session's Claude Code process, so only what it adds is counted; `account_costs` sums that per account and hour (epoch
/// hours); `extra_usage` is each account's latest extra-usage state from a usage report.
const SCHEMA_V7: &str = r#"
CREATE TABLE session_costs (
    session_id TEXT NOT NULL,
    process    TEXT NOT NULL,
    total_usd  REAL NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (session_id, process)
) WITHOUT ROWID;
CREATE TABLE account_costs (
    account_id INTEGER NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    hour       INTEGER NOT NULL,
    usd        REAL NOT NULL,
    PRIMARY KEY (account_id, hour)
) WITHOUT ROWID;
CREATE TABLE extra_usage (
    account_id      INTEGER PRIMARY KEY REFERENCES accounts (id) ON DELETE CASCADE,
    enabled         INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    monthly_limit   INTEGER,
    used            INTEGER,
    utilization     REAL,
    currency        TEXT,
    disabled_reason TEXT,
    read_at         INTEGER NOT NULL
);
"#;

/// Schema 8: whether cc-rotate starts the account's next 5-hour window with a small request as
/// soon as one ends while it is not in use (`usage_check::warm_up`); off unless the user turns
/// it on for the account.
const SCHEMA_V8: &str = r#"
ALTER TABLE accounts ADD COLUMN keep_running INTEGER NOT NULL DEFAULT 0
    CHECK (keep_running IN (0, 1));
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

/// Whether an account has a weekly limit on all models: detected from its usage (`auto`), or
/// set by the user when detection does not know yet or gets it wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WeeklyLimitSetting {
    Auto,
    Yes,
    No,
}

impl WeeklyLimitSetting {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Yes => "yes",
            Self::No => "no",
        }
    }
}

impl FromStr for WeeklyLimitSetting {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "auto" | "detect" => Self::Auto,
            "yes" | "has" | "true" | "on" => Self::Yes,
            "no" | "none" | "false" | "off" => Self::No,
            other => bail!("expected auto, yes or no, not {other:?}"),
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
    /// Place in the user's order: lower is used first. Ties are possible in stores upgraded
    /// from the old `priority`; reordering numbers every account from 1.
    pub position: i64,
    pub weekly_limit: WeeklyLimitSetting,
    /// Rotation stops using the account once its weekly usage reaches this (percent), when it
    /// is below `swap_threshold_7d`: the rest is kept for the user's own use.
    pub weekly_cap: Option<f64>,
    /// The account moves ahead when its weekly budget would reset unused (`budget::expiring`),
    /// unless the user turned that off for it.
    pub use_expiring: bool,
    /// cc-rotate starts the account's next 5-hour window with a small request as soon as one
    /// ends while it is not in use (`usage_check::warm_up`), because the user turned that on.
    pub keep_running: bool,
    pub added_at: i64,
    pub last_login_at: Option<i64>,
}

const ACCOUNT_COLS: &str = "id, label, email, account_uuid, org_uuid, org_name, subscription_type, \
    rate_limit_tier, state, position, added_at, last_login_at, weekly_limit, weekly_cap, use_expiring, \
    keep_running";

fn parse_col<T: FromStr<Err = anyhow::Error>>(row: &Row, i: usize) -> rusqlite::Result<T> {
    let text: String = row.get(i)?;
    text.parse().map_err(|e: anyhow::Error| {
        rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, e.into())
    })
}

fn account_from_row(row: &Row) -> rusqlite::Result<Account> {
    Ok(Account {
        id: row.get(0)?,
        label: row.get(1)?,
        email: row.get(2)?,
        account_uuid: row.get(3)?,
        org_uuid: row.get(4)?,
        org_name: row.get(5)?,
        subscription_type: row.get(6)?,
        rate_limit_tier: row.get(7)?,
        state: parse_col(row, 8)?,
        position: row.get(9)?,
        added_at: row.get(10)?,
        last_login_at: row.get(11)?,
        weekly_limit: parse_col(row, 12)?,
        weekly_cap: row.get(13)?,
        use_expiring: row.get(14)?,
        keep_running: row.get(15)?,
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

/// One reading of an account's usage: every window the data source reported at that moment.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sample {
    pub ts: i64,
    pub account_id: i64,
    pub profile: String,
    pub session_id: Option<String>,
    /// The model the session that sent this reading was using, when it said.
    pub model: Option<String>,
    pub windows: Vec<SampleWindow>,
}

/// One usage window in a sample, named as Claude Code names it (`five_hour`, `seven_day`, ...).
/// `scheduler::classify` says what kind of limit a name is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SampleWindow {
    pub name: String,
    pub pct: f64,
    /// Epoch seconds.
    pub resets_at: Option<i64>,
}

impl Sample {
    pub fn window(&self, name: &str) -> Option<&SampleWindow> {
        self.windows.iter().find(|w| w.name == name)
    }

    /// Same windows and reset times, and percentages within half a point.
    fn same_reading(&self, other: &Sample) -> bool {
        same_windows(&self.windows, &other.windows)
    }
}

/// Two readings with the same windows and reset times, and percentages within half a point.
pub fn same_windows(a: &[SampleWindow], b: &[SampleWindow]) -> bool {
    a.len() == b.len()
        && a.iter().all(|w| {
            b.iter().any(|o| {
                o.name == w.name && o.resets_at == w.resets_at && (o.pct - w.pct).abs() < 0.5
            })
        })
}

/// The highest reading recorded for one usage window (one reset time) of an account.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowPeak {
    /// Epoch seconds.
    pub resets_at: i64,
    pub peak_pct: f64,
    /// Readings that reported this window.
    pub samples: usize,
    /// Epoch ms.
    pub first_sample_at: i64,
    /// Epoch ms.
    pub last_sample_at: i64,
}

const SAMPLE_COLS: &str = "id, ts, account_id, profile, session_id, model";

fn sample_from_row(row: &Row) -> rusqlite::Result<(i64, Sample)> {
    Ok((
        row.get(0)?,
        Sample {
            ts: row.get(1)?,
            account_id: row.get(2)?,
            profile: row.get(3)?,
            session_id: row.get(4)?,
            model: row.get(5)?,
            windows: Vec::new(),
        },
    ))
}

/// An account's extra usage (overage) for the billing period, as a usage report gives it.
/// Amounts are in minor units of `currency` (cents for USD).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtraUsage {
    /// False while extra usage cannot cover requests: turned off, or its limit reached.
    pub enabled: bool,
    pub monthly_limit: Option<i64>,
    pub used: Option<i64>,
    /// Percent of the monthly limit used.
    pub utilization: Option<f64>,
    pub currency: Option<String>,
    pub disabled_reason: Option<String>,
}

/// Where a session stopped at a usage limit stands (`waiting`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingState {
    /// Its `StopFailure` hook waits for an account with room to wake it.
    Waiting,
    /// The hook woke it on `woken_account_id`; not seen working since.
    Woken,
    /// Its status line reported the account it was woken on.
    Resumed,
    /// Woken, but not seen working since (`waiting::STUCK_AFTER_MS`); the user was told.
    Stuck,
    /// The hook stopped without waking it: it gave up, was replaced, or its process is gone.
    Ended,
}

impl WaitingState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Woken => "woken",
            Self::Resumed => "resumed",
            Self::Stuck => "stuck",
            Self::Ended => "ended",
        }
    }
}

impl FromStr for WaitingState {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "waiting" => Self::Waiting,
            "woken" => Self::Woken,
            "resumed" => Self::Resumed,
            "stuck" => Self::Stuck,
            "ended" => Self::Ended,
            other => bail!("unknown waiting state {other:?}"),
        })
    }
}

/// A session whose turn stopped at a usage limit (`waiting_sessions`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WaitingSession {
    pub session_id: String,
    /// When the turn stopped (epoch ms). A later stop of the same session replaces the row,
    /// and the hook that recorded the earlier one gives up.
    pub since_ms: i64,
    /// The account in the profile when it stopped.
    pub account_id: Option<i64>,
    /// The hook process waiting to wake it, with its start time (`process::start_time`), so a
    /// reused process id does not count as the hook.
    pub hook_pid: Option<u32>,
    pub hook_started: Option<String>,
    pub state: WaitingState,
    /// The last time the session was woken, and on which account.
    pub woken_at: Option<i64>,
    pub woken_account_id: Option<i64>,
    /// Every account the session was woken on since it first stopped. Kept across a new stop
    /// within `WAKE_MEMORY_MS` of the last wake, unless the session was seen working since, so
    /// a session that keeps stopping is never woken twice on one account, even while readings
    /// wrongly show room on several.
    pub woken_accounts: Vec<i64>,
    pub updated_at: i64,
}

/// A new stop of a session woken this recently, and not seen working since, remembers the
/// accounts it was woken on.
pub const WAKE_MEMORY_MS: i64 = 30 * 60_000;

/// Waiting-session rows are dropped this long after their last change.
const WAITING_RETENTION_MS: i64 = 7 * 86_400_000;

const WAITING_COLS: &str = "session_id, since_ms, account_id, hook_pid, hook_started, state, \
    woken_at, woken_account_id, woken_accounts, updated_at";

fn waiting_from_row(row: &Row) -> rusqlite::Result<WaitingSession> {
    Ok(WaitingSession {
        session_id: row.get(0)?,
        since_ms: row.get(1)?,
        account_id: row.get(2)?,
        hook_pid: row.get(3)?,
        hook_started: row.get(4)?,
        state: parse_col(row, 5)?,
        woken_at: row.get(6)?,
        woken_account_id: row.get(7)?,
        woken_accounts: serde_json::from_str(&row.get::<_, String>(8)?).unwrap_or_default(),
        updated_at: row.get(9)?,
    })
}

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
    /// A session stopped at a usage limit (`waiting`), was woken on another account
    /// (`woken`), or was seen working again (`resumed`); `detail.session` names it.
    pub const SESSION: &str = "session";
    /// A session cc-rotate woke did not continue (`waiting::scan`).
    pub const SESSION_STUCK: &str = "session_stuck";
    /// An account's sign-in ends soon (`health::scan`); `detail.expires_at` says when.
    pub const LOGIN_EXPIRING: &str = "login_expiring";
    /// The rotation profile's sessions run a Claude Code version not seen before;
    /// `detail` has `version`, `previous` and `tested`.
    pub const CLAUDE_VERSION: &str = "claude_version";
    /// Every account is at a limit and the one in use runs on paid extra usage
    /// (`extra_usage::scan`); `detail` has its spend so far.
    pub const PAID_USAGE: &str = "paid_usage";
    /// A warm-up started an account's 5-hour window (`usage_check::warm_up`);
    /// `detail.cost_usd` is what Claude Code reported it cost at API prices.
    pub const WARM_UP: &str = "warm_up";
}

/// Typed view of the `settings` table with defaults applied.
#[derive(Debug, Clone, Serialize)]
pub struct Settings {
    /// Applies to 5-hour (session) windows.
    pub swap_threshold_5h: f64,
    /// Applies to every weekly window, on all models or on one.
    pub swap_threshold_7d: f64,
    pub swap_cooldown_s: i64,
    /// Rank accounts without a weekly limit before the ones with one.
    pub prefer_no_weekly_limit: bool,
    /// How long the active account stays in use before cc-rotate switches back to a better
    /// ranked account that became usable again.
    pub switch_back_dwell_s: i64,
    pub lock_timeout_ms: u64,
    pub sample_retention_days: i64,
    pub statusline_downstream: Option<String>,
    pub profile_dir: Option<String>,
    pub claude_path: Option<String>,
    /// Read the usage of accounts that are not in use with Claude Code's own `/usage`.
    pub usage_checks: bool,
    /// Background checks read an account at most this often.
    pub usage_check_interval_s: i64,
    /// A swap first checks the account it picked when its latest reading is older than this.
    pub usage_check_max_age_s: i64,
    /// Move an account with a weekly limit ahead while its weekly budget would otherwise reset
    /// unused (`budget`).
    pub use_expiring_weekly: bool,
    /// Warn this many days before an account's sign-in ends; 0 turns the warning off.
    pub login_warning_days: i64,
    /// Go by the user's order alone (`scheduler::Policy::strict_order`).
    pub strict_order: bool,
    /// When every account is at a limit and the one in use has extra usage on, move to one
    /// without, so sessions wait for a reset instead of paying (`extra_usage`).
    pub avoid_extra_usage: bool,
}

pub const SETTING_KEYS: &[&str] = &[
    "swap_threshold_5h",
    "swap_threshold_7d",
    "swap_cooldown_s",
    "prefer_no_weekly_limit",
    "switch_back_dwell_s",
    "lock_timeout_ms",
    "sample_retention_days",
    "statusline_downstream",
    "profile_dir",
    "claude_path",
    "usage_checks",
    "usage_check_interval_s",
    "usage_check_max_age_s",
    "use_expiring_weekly",
    "login_warning_days",
    "strict_order",
    "avoid_extra_usage",
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

    /// Open an existing store without changing it: read only, never migrated, and refused when
    /// its schema is not this build's. For tools that only read the real store, such as
    /// `examples/e2e_stage.rs`, so running one never upgrades the store under the installed
    /// cc-rotate.
    pub fn open_read_only(path: &Path) -> Result<Store> {
        let dir = path
            .parent()
            .context("the store path has no parent directory")?;
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening store {} read-only", path.display()))?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            bail!(
                "{} is at schema {version} and this build uses schema {SCHEMA_VERSION}. Use a build that matches the installed cc-rotate.",
                path.display()
            );
        }
        Ok(Store {
            conn,
            key_path: Some(secret::key_path(dir)),
            key: OnceCell::new(),
        })
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
                "{} is missing, so saved logins can't be decrypted. Restore the file, or remove the accounts and sign in to them again.",
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
            if version < 3 {
                s.conn.execute_batch(SCHEMA_V3)?;
                s.conn.pragma_update(None, "user_version", 3)?;
            }
            if version < 4 {
                s.conn.execute_batch(SCHEMA_V4)?;
                s.conn.pragma_update(None, "user_version", 4)?;
            }
            if version < 5 {
                s.conn.execute_batch(SCHEMA_V5)?;
                s.conn.pragma_update(None, "user_version", 5)?;
            }
            if version < 6 {
                s.conn.execute_batch(SCHEMA_V6)?;
                s.conn.pragma_update(None, "user_version", 6)?;
            }
            if version < 7 {
                s.conn.execute_batch(SCHEMA_V7)?;
                s.conn.pragma_update(None, "user_version", 7)?;
            }
            if version < 8 {
                s.conn.execute_batch(SCHEMA_V8)?;
                s.conn.pragma_update(None, "user_version", 8)?;
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
                            "message": format!("Couldn't upgrade this account's saved login ({e:#}). Sign in to it again."),
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
        if key == "usage_checks" {
            // Turning checks on (or off) again is how the user lifts a stop (usage_check).
            self.delete_meta(crate::usage_check::META_BLOCKED)?;
        }
        match value {
            Some(v) => {
                let normalized = validate_setting(key, v)?;
                let v = normalized.as_deref().unwrap_or(v);
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
            prefer_no_weekly_limit: get("prefer_no_weekly_limit")?
                .and_then(|v| parse_bool(&v))
                .unwrap_or(true),
            switch_back_dwell_s: int("switch_back_dwell_s", 600)?,
            lock_timeout_ms: int("lock_timeout_ms", 30_000)? as u64,
            sample_retention_days: int("sample_retention_days", 90)?,
            statusline_downstream: text("statusline_downstream")?,
            profile_dir: text("profile_dir")?,
            claude_path: text("claude_path")?,
            usage_checks: get("usage_checks")?
                .and_then(|v| parse_bool(&v))
                .unwrap_or(true),
            usage_check_interval_s: int("usage_check_interval_s", 3600)?,
            usage_check_max_age_s: int("usage_check_max_age_s", 900)?,
            use_expiring_weekly: get("use_expiring_weekly")?
                .and_then(|v| parse_bool(&v))
                .unwrap_or(true),
            login_warning_days: int("login_warning_days", 5)?,
            strict_order: get("strict_order")?
                .and_then(|v| parse_bool(&v))
                .unwrap_or(false),
            avoid_extra_usage: get("avoid_extra_usage")?
                .and_then(|v| parse_bool(&v))
                .unwrap_or(false),
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

    pub fn delete_meta(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM meta WHERE key = ?1", [key])?;
        Ok(())
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
            "SELECT {ACCOUNT_COLS} FROM accounts ORDER BY position, label COLLATE NOCASE, id"
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
        // A new account goes to the end of the order.
        self.conn.execute(
            "INSERT INTO accounts (label, email, account_uuid, org_uuid, org_name, subscription_type,
                                   rate_limit_tier, profile_account, state, added_at, last_login_at,
                                   position)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10,
                     (SELECT COALESCE(MAX(position), 0) + 1 FROM accounts))",
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

    /// Put `ids` first, in that order, and every other account after them in its current
    /// order; then number the whole order from 1.
    pub fn set_order(&self, ids: &[i64]) -> Result<()> {
        let current: Vec<i64> = self.accounts()?.iter().map(|a| a.id).collect();
        if let Some(id) = ids.iter().find(|id| !current.contains(id)) {
            bail!("account {id} does not exist");
        }
        let mut order: Vec<i64> = Vec::with_capacity(current.len());
        for id in ids.iter().chain(&current) {
            if !order.contains(id) {
                order.push(*id);
            }
        }
        self.write_order(&order)
    }

    /// Move one account to place `to` in the order (1 is used first), shifting the others.
    pub fn move_account(&self, id: i64, to: usize) -> Result<()> {
        let mut order: Vec<i64> = self.accounts()?.iter().map(|a| a.id).collect();
        let from = order
            .iter()
            .position(|&x| x == id)
            .ok_or_else(|| anyhow!("account {id} does not exist"))?;
        order.remove(from);
        order.insert(to.saturating_sub(1).min(order.len()), id);
        self.write_order(&order)
    }

    fn write_order(&self, order: &[i64]) -> Result<()> {
        self.conn.execute_batch("SAVEPOINT write_order")?;
        let written = order.iter().enumerate().try_for_each(|(i, id)| {
            self.conn
                .execute(
                    "UPDATE accounts SET position = ?2 WHERE id = ?1",
                    params![id, i as i64 + 1],
                )
                .map(drop)
                .map_err(anyhow::Error::from)
        });
        finish_savepoint(&self.conn, "write_order", written)
    }

    pub fn set_weekly_limit(&self, id: i64, setting: WeeklyLimitSetting) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE accounts SET weekly_limit = ?2 WHERE id = ?1",
            params![id, setting.as_str()],
        )?;
        if n == 0 {
            bail!("account {id} does not exist");
        }
        Ok(())
    }

    /// Set or clear (`None`) the account's weekly cap, in percent from 1 to 100.
    pub fn set_weekly_cap(&self, id: i64, cap: Option<f64>) -> Result<()> {
        if let Some(c) = cap {
            if !(1.0..=100.0).contains(&c) {
                bail!("Use a number from 1 to 100.");
            }
        }
        let n = self.conn.execute(
            "UPDATE accounts SET weekly_cap = ?2 WHERE id = ?1",
            params![id, cap],
        )?;
        if n == 0 {
            bail!("account {id} does not exist");
        }
        Ok(())
    }

    /// Whether cc-rotate keeps the account's 5-hour window running. Turning it on for any
    /// account lets warm-ups run again after they were stopped (`usage_check::META_WARM_BLOCKED`).
    pub fn set_keep_running(&self, id: i64, on: bool) -> Result<()> {
        self.immediate(|s| {
            let n = s.conn.execute(
                "UPDATE accounts SET keep_running = ?2 WHERE id = ?1",
                params![id, on],
            )?;
            if n == 0 {
                bail!("account {id} does not exist");
            }
            if on {
                s.delete_meta(crate::usage_check::META_WARM_BLOCKED)?;
            }
            Ok(())
        })
    }

    /// Whether the account moves ahead when its weekly budget would reset unused.
    pub fn set_use_expiring(&self, id: i64, on: bool) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE accounts SET use_expiring = ?2 WHERE id = ?1",
            params![id, on],
        )?;
        if n == 0 {
            bail!("account {id} does not exist");
        }
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
        // A login names the account's plan, and every renewal repeats it, so storing a login is
        // also what keeps the plan shown for the account current.
        let text = |k: &str| {
            oauth
                .get(k)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
        };
        self.conn.execute(
            "UPDATE accounts SET
                subscription_type = COALESCE(?2, subscription_type),
                rate_limit_tier   = COALESCE(?3, rate_limit_tier)
             WHERE id = ?1",
            params![account_id, text("subscriptionType"), text("rateLimitTier")],
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
                "The saved login for account {account_id} is from an older cc-rotate and couldn't be upgraded. Sign in to it again."
            );
        }
        let plain = self
            .key()?
            .open(&blob, &secret::credential_aad(account_id))
            .with_context(|| format!("decrypting the saved login for account {account_id}"))?;
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
            if s.ts - last.ts < 60_000 && last.same_reading(s) {
                return Ok(false);
            }
        }
        self.conn.execute_batch("SAVEPOINT record_sample")?;
        let written = (|| -> Result<()> {
            self.conn.execute(
                "INSERT INTO samples (ts, account_id, profile, session_id, model)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![s.ts, s.account_id, s.profile, s.session_id, s.model],
            )?;
            let id = self.conn.last_insert_rowid();
            for w in &s.windows {
                self.conn.execute(
                    "INSERT OR REPLACE INTO sample_windows (sample_id, account_id, name, pct, resets_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, s.account_id, w.name, w.pct, w.resets_at],
                )?;
            }
            Ok(())
        })();
        finish_savepoint(&self.conn, "record_sample", written)?;
        Ok(true)
    }

    pub fn last_sample(&self, account_id: i64) -> Result<Option<Sample>> {
        let row = self
            .conn
            .query_row(
                &format!(
                    "SELECT {SAMPLE_COLS} FROM samples WHERE account_id = ?1
                     ORDER BY ts DESC, id DESC LIMIT 1"
                ),
                [account_id],
                sample_from_row,
            )
            .optional()?;
        let Some((id, mut sample)) = row else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT name, pct, resets_at FROM sample_windows WHERE sample_id = ?1 ORDER BY name",
        )?;
        sample.windows = stmt
            .query_map([id], window_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Some(sample))
    }

    pub fn samples_since(&self, since_ms: i64) -> Result<Vec<Sample>> {
        self.samples_matching(None, since_ms, i64::MAX)
    }

    /// One account's samples taken at or after `since_ms`, oldest first.
    pub fn account_samples_since(&self, account_id: i64, since_ms: i64) -> Result<Vec<Sample>> {
        self.samples_matching(Some(account_id), since_ms, i64::MAX)
    }

    /// One account's samples taken from `from_ms` up to `to_ms`, both included, oldest first.
    pub fn account_samples_between(
        &self,
        account_id: i64,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<Sample>> {
        self.samples_matching(Some(account_id), from_ms, to_ms)
    }

    fn samples_matching(
        &self,
        account_id: Option<i64>,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vec<Sample>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {SAMPLE_COLS} FROM samples
             WHERE ts >= ?1 AND ts <= ?3 AND (?2 IS NULL OR account_id = ?2) ORDER BY ts, id"
        ))?;
        let mut samples: Vec<(i64, Sample)> = stmt
            .query_map(params![since_ms, account_id, until_ms], sample_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        let index: std::collections::HashMap<i64, usize> = samples
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (*id, i))
            .collect();
        let mut stmt = self.conn.prepare_cached(
            "SELECT w.sample_id, w.name, w.pct, w.resets_at FROM sample_windows w
             JOIN samples s ON s.id = w.sample_id
             WHERE s.ts >= ?1 AND s.ts <= ?3 AND (?2 IS NULL OR s.account_id = ?2)
             ORDER BY w.name",
        )?;
        let rows = stmt.query_map(params![since_ms, account_id, until_ms], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                SampleWindow {
                    name: r.get(1)?,
                    pct: r.get(2)?,
                    resets_at: r.get(3)?,
                },
            ))
        })?;
        for row in rows {
            let (id, window) = row?;
            if let Some(&i) = index.get(&id) {
                samples[i].1.windows.push(window);
            }
        }
        Ok(samples.into_iter().map(|(_, s)| s).collect())
    }

    /// Every window name recorded for an account within the retention period.
    pub fn window_names(&self, account_id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT name FROM sample_windows WHERE account_id = ?1 ORDER BY name",
        )?;
        let rows = stmt.query_map([account_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The distinct reset times recorded for one of an account's windows that are at or after
    /// `since_s`, in ascending order.
    pub fn window_resets_since(
        &self,
        account_id: i64,
        name: &str,
        since_s: i64,
    ) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT resets_at FROM sample_windows
             WHERE account_id = ?1 AND name = ?2 AND resets_at >= ?3 ORDER BY resets_at",
        )?;
        let rows = stmt.query_map(params![account_id, name, since_s], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The highest reading of each of an account's windows named `name` whose reset time is in
    /// `resets_from_s..=resets_to_s`, from samples taken at or after `since_ms`: one row per
    /// distinct reset time, in ascending order. Read-only.
    pub fn window_peaks(
        &self,
        account_id: i64,
        name: &str,
        resets_from_s: i64,
        resets_to_s: i64,
        since_ms: i64,
    ) -> Result<Vec<WindowPeak>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT w.resets_at, MAX(w.pct), COUNT(*), MIN(s.ts), MAX(s.ts)
             FROM sample_windows w JOIN samples s ON s.id = w.sample_id
             WHERE w.account_id = ?1 AND w.name = ?2 AND w.resets_at BETWEEN ?3 AND ?4
               AND s.ts >= ?5
             GROUP BY w.resets_at ORDER BY w.resets_at",
        )?;
        let rows = stmt.query_map(
            params![account_id, name, resets_from_s, resets_to_s, since_ms],
            |r| {
                Ok(WindowPeak {
                    resets_at: r.get(0)?,
                    peak_pct: r.get(1)?,
                    samples: r.get::<_, i64>(2)? as usize,
                    first_sample_at: r.get(3)?,
                    last_sample_at: r.get(4)?,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// How many readings of an account were taken in `from_ms..to_ms`. Read-only.
    pub fn sample_count(&self, account_id: i64, from_ms: i64, to_ms: i64) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM samples WHERE account_id = ?1 AND ts >= ?2 AND ts < ?3",
            params![account_id, from_ms, to_ms],
            |r| r.get(0),
        )?;
        Ok(n as usize)
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
        self.conn.execute(
            "DELETE FROM waiting_sessions WHERE updated_at < ?1",
            [now_ms - WAITING_RETENTION_MS],
        )?;
        self.conn
            .execute("DELETE FROM session_costs WHERE updated_at < ?1", [cutoff])?;
        self.conn.execute(
            "DELETE FROM account_costs WHERE hour < ?1",
            [cutoff.div_euclid(3_600_000)],
        )?;
        self.set_meta("last_prune_at", now_ms)
    }

    // ---- what use would cost at API prices ---------------------------------------------------

    /// A session's `cost.total_cost_usd` from the status line of its Claude Code `process`
    /// (`process::Ancestors::launcher`; two processes can run one session, resumed twice, each
    /// restoring the same total). What it added since that process's last total counts for
    /// `account_id`, in the hour of `now_ms`; returns that.
    ///
    /// A process's first total counts whole only when it is a `new_session`'s (it ran under a
    /// minute) that was never seen before; otherwise it carries what the session spent before (a
    /// resumed or forked one, given the total and run time it had, or one running before cc-rotate
    /// counted) and is only where counting starts. A total below the last one (a `/login` resets
    /// it) is also only where counting goes on from.
    pub fn note_session_cost(
        &self,
        session_id: &str,
        process: &str,
        total_usd: f64,
        new_session: bool,
        account_id: i64,
        now_ms: i64,
    ) -> Result<f64> {
        if !total_usd.is_finite() || total_usd < 0.0 {
            return Ok(0.0);
        }
        self.immediate(|s| {
            let last: Option<f64> = s
                .conn
                .query_row(
                    "SELECT total_usd FROM session_costs WHERE session_id = ?1 AND process = ?2",
                    params![session_id, process],
                    |r| r.get(0),
                )
                .optional()?;
            let seen: bool = s.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM session_costs WHERE session_id = ?1)",
                [session_id],
                |r| r.get(0),
            )?;
            let added = match last {
                Some(last) if total_usd >= last => total_usd - last,
                Some(_) => 0.0,
                None if new_session && !seen => total_usd,
                None => 0.0,
            };
            s.conn.execute(
                "INSERT INTO session_costs (session_id, process, total_usd, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (session_id, process) DO UPDATE SET
                    total_usd = excluded.total_usd, updated_at = excluded.updated_at",
                params![session_id, process, total_usd, now_ms],
            )?;
            if added > 0.0 {
                s.conn.execute(
                    "INSERT INTO account_costs (account_id, hour, usd) VALUES (?1, ?2, ?3)
                     ON CONFLICT (account_id, hour) DO UPDATE SET usd = usd + excluded.usd",
                    params![account_id, now_ms.div_euclid(3_600_000), added],
                )?;
            }
            Ok(added)
        })
    }

    /// What an account's use from `from_s` to `to_s` (epoch seconds, by whole hours) would have
    /// cost at API prices, or `None` when nothing was counted then.
    pub fn account_cost_between(
        &self,
        account_id: i64,
        from_s: i64,
        to_s: i64,
    ) -> Result<Option<f64>> {
        Ok(self.conn.query_row(
            "SELECT SUM(usd) FROM account_costs WHERE account_id = ?1 AND hour >= ?2 AND hour < ?3",
            params![account_id, from_s.div_euclid(3600), to_s.div_euclid(3600)],
            |r| r.get(0),
        )?)
    }

    // ---- extra usage ---------------------------------------------------------------------------

    /// Record an account's extra usage as a report read at `read_at` (epoch ms) gave it, unless a
    /// newer one is already recorded.
    pub fn set_extra_usage(&self, account_id: i64, extra: &ExtraUsage, read_at: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO extra_usage (account_id, enabled, monthly_limit, used, utilization,
                                      currency, disabled_reason, read_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (account_id) DO UPDATE SET
                enabled = excluded.enabled, monthly_limit = excluded.monthly_limit,
                used = excluded.used, utilization = excluded.utilization,
                currency = excluded.currency, disabled_reason = excluded.disabled_reason,
                read_at = excluded.read_at
             WHERE excluded.read_at >= extra_usage.read_at",
            params![
                account_id,
                extra.enabled,
                extra.monthly_limit,
                extra.used,
                extra.utilization,
                extra.currency,
                extra.disabled_reason,
                read_at
            ],
        )?;
        Ok(())
    }

    /// An account's latest extra usage, with when it was read (epoch ms).
    pub fn extra_usage(&self, account_id: i64) -> Result<Option<(ExtraUsage, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT enabled, monthly_limit, used, utilization, currency, disabled_reason, read_at
                 FROM extra_usage WHERE account_id = ?1",
                [account_id],
                |r| {
                    Ok((
                        ExtraUsage {
                            enabled: r.get(0)?,
                            monthly_limit: r.get(1)?,
                            used: r.get(2)?,
                            utilization: r.get(3)?,
                            currency: r.get(4)?,
                            disabled_reason: r.get(5)?,
                        },
                        r.get(6)?,
                    ))
                },
            )
            .optional()?)
    }

    // ---- sessions stopped at a usage limit ------------------------------------------------

    /// Record that `session_id` stopped at a usage limit at `since_ms`, replacing an earlier
    /// stop of the same session. Its wakes are kept when the last was recent (`WAKE_MEMORY_MS`)
    /// and the session was not seen working since. The expressions read the row as it was.
    pub fn start_waiting(
        &self,
        session_id: &str,
        since_ms: i64,
        account_id: Option<i64>,
        hook: Option<(u32, Option<String>)>,
    ) -> Result<WaitingSession> {
        let (pid, started) = hook.unzip();
        self.conn.execute(
            "INSERT INTO waiting_sessions (session_id, since_ms, account_id, hook_pid, hook_started,
                                           state, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'waiting', ?2)
             ON CONFLICT (session_id) DO UPDATE SET
                since_ms = excluded.since_ms,
                account_id = excluded.account_id,
                hook_pid = excluded.hook_pid,
                hook_started = excluded.hook_started,
                state = 'waiting',
                woken_at = CASE WHEN woken_at >= ?6 AND state <> 'resumed' THEN woken_at END,
                woken_account_id = CASE WHEN woken_at >= ?6 AND state <> 'resumed'
                                        THEN woken_account_id END,
                woken_accounts = CASE WHEN woken_at >= ?6 AND state <> 'resumed'
                                      THEN woken_accounts ELSE '[]' END,
                updated_at = excluded.updated_at",
            params![
                session_id,
                since_ms,
                account_id,
                pid,
                started.flatten(),
                since_ms - WAKE_MEMORY_MS
            ],
        )?;
        self.waiting_session(session_id)?
            .context("the waiting session was not recorded")
    }

    pub fn waiting_session(&self, session_id: &str) -> Result<Option<WaitingSession>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {WAITING_COLS} FROM waiting_sessions WHERE session_id = ?1"),
                [session_id],
                waiting_from_row,
            )
            .optional()?)
    }

    /// Sessions still waiting to be woken, or woken and not seen working since, oldest first.
    pub fn open_waiting_sessions(&self) -> Result<Vec<WaitingSession>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {WAITING_COLS} FROM waiting_sessions
             WHERE state IN ('waiting', 'woken') ORDER BY since_ms"
        ))?;
        let rows = stmt.query_map([], waiting_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Move the stop recorded at `since_ms` from one of the `from` states to `state` (and record
    /// a wake), unless a later stop replaced it or another process moved it first. Returns
    /// whether it did.
    pub fn set_waiting_state(
        &self,
        session_id: &str,
        since_ms: i64,
        from: &[WaitingState],
        state: WaitingState,
        woken: Option<(i64, i64)>,
        now_ms: i64,
    ) -> Result<bool> {
        let (woken_at, woken_account) = woken.unzip();
        let from: Vec<String> = from.iter().map(|s| format!("'{}'", s.as_str())).collect();
        let n = self.conn.execute(
            &format!(
                "UPDATE waiting_sessions SET state = ?3,
                woken_at = COALESCE(?4, woken_at),
                woken_account_id = COALESCE(?5, woken_account_id),
                woken_accounts = CASE WHEN ?5 IS NULL THEN woken_accounts
                                      ELSE json_insert(woken_accounts, '$[#]', ?5) END,
                updated_at = ?6
             WHERE session_id = ?1 AND since_ms = ?2 AND state IN ({})",
                from.join(", ")
            ),
            params![
                session_id,
                since_ms,
                state.as_str(),
                woken_at,
                woken_account,
                now_ms
            ],
        )?;
        Ok(n > 0)
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

fn window_from_row(r: &Row) -> rusqlite::Result<SampleWindow> {
    Ok(SampleWindow {
        name: r.get(0)?,
        pct: r.get(1)?,
        resets_at: r.get(2)?,
    })
}

/// Release a savepoint after `result`, first rolling its changes back if it failed.
fn finish_savepoint(conn: &Connection, name: &str, result: Result<()>) -> Result<()> {
    match result {
        Ok(()) => {
            conn.execute_batch(&format!("RELEASE {name}"))?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}"));
            Err(e)
        }
    }
}

pub fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// The shortest `usage_check_interval_s`.
pub const MIN_CHECK_INTERVAL_S: i64 = 300;

/// Check a setting's value. Returns the value to store instead, when it is spelled differently.
fn validate_setting(key: &str, value: &str) -> Result<Option<String>> {
    match key {
        "swap_threshold_5h" | "swap_threshold_7d" => {
            let v: f64 = value
                .parse()
                .map_err(|_| anyhow!("Use a number from 1 to 100."))?;
            if !(1.0..=100.0).contains(&v) {
                bail!("Use a number from 1 to 100.");
            }
        }
        "swap_cooldown_s" | "switch_back_dwell_s" | "lock_timeout_ms" | "sample_retention_days" => {
            let v: i64 = value.parse().map_err(|_| anyhow!("Use a whole number."))?;
            if v < 0 {
                bail!("Use 0 or more.");
            }
        }
        "usage_check_interval_s" => {
            let v: i64 = value.parse().map_err(|_| anyhow!("Use a whole number."))?;
            if v < MIN_CHECK_INTERVAL_S {
                bail!(
                    "Use {MIN_CHECK_INTERVAL_S} or more. Anthropic refuses more frequent usage checks."
                );
            }
        }
        "usage_check_max_age_s" => {
            let v: i64 = value.parse().map_err(|_| anyhow!("Use a whole number."))?;
            if v < 0 {
                bail!("Use 0 or more.");
            }
        }
        "login_warning_days" => {
            let v: i64 = value
                .parse()
                .map_err(|_| anyhow!("Use a whole number of days from 0 to 30."))?;
            if !(0..=30).contains(&v) {
                bail!("Use a whole number of days from 0 to 30.");
            }
        }
        "prefer_no_weekly_limit"
        | "usage_checks"
        | "use_expiring_weekly"
        | "strict_order"
        | "avoid_extra_usage" => {
            let v = parse_bool(value).context("expected true or false")?;
            return Ok(Some(v.to_string()));
        }
        _ => {}
    }
    Ok(None)
}

pub fn validate_label(label: &str) -> Result<()> {
    if label.is_empty() || label.len() > 40 {
        bail!("Labels can be 1 to 40 characters.");
    }
    if !label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("Labels can only use letters, digits, - _ and .");
    }
    if label.parse::<i64>().is_ok() {
        bail!("A label can't be just a number.");
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

    #[test]
    fn a_read_only_open_never_changes_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        Store::open(&path).unwrap();

        let store = Store::open_read_only(&path).unwrap();
        assert!(store.accounts().unwrap().is_empty());
        assert!(store.set_setting("usage_checks", Some("false")).is_err());
        drop(store);

        // An older store is refused, not migrated.
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION - 1)
            .unwrap();
        drop(conn);
        let err = Store::open_read_only(&path).err().unwrap().to_string();
        assert!(err.contains("this build uses schema"), "{err}");
        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION - 1);
    }
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
        assert!(
            format!("{err:#}").contains("Sign in to it again"),
            "{err:#}"
        );
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
            model: Some("claude-opus-4-1".into()),
            windows: vec![
                SampleWindow {
                    name: "five_hour".into(),
                    pct,
                    resets_at: Some(100),
                },
                SampleWindow {
                    name: "seven_day".into(),
                    pct: 10.0,
                    resets_at: Some(200),
                },
            ],
        };
        assert!(s.record_sample(&mk(1_000, 20.0)).unwrap());
        assert!(!s.record_sample(&mk(6_000, 20.2)).unwrap());
        assert!(s.record_sample(&mk(7_000, 21.0)).unwrap());
        assert!(s.record_sample(&mk(70_000, 21.0)).unwrap());
        let mut fewer = mk(71_000, 21.0);
        fewer.windows.pop();
        assert!(
            s.record_sample(&fewer).unwrap(),
            "a window that stopped being reported is a new reading"
        );
        let all = s.samples_since(0).unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0], mk(1_000, 20.0), "windows round-trip");
        assert_eq!(s.last_sample(a).unwrap().unwrap(), fewer);
        assert_eq!(
            s.window_names(a).unwrap(),
            vec!["five_hour".to_string(), "seven_day".to_string()]
        );
        assert_eq!(s.window_resets_since(a, "five_hour", 0).unwrap(), vec![100]);
        assert!(
            s.window_resets_since(a, "five_hour", 101)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn order_and_weekly_limit_setting() {
        let s = Store::open_in_memory().unwrap();
        let a = s
            .insert_account("a", &ident("ua"), AccountState::Active)
            .unwrap();
        let b = s
            .insert_account("b", &ident("ub"), AccountState::Parked)
            .unwrap();
        let c = s
            .insert_account("c", &ident("uc"), AccountState::Parked)
            .unwrap();
        let order = |s: &Store| {
            s.accounts()
                .unwrap()
                .iter()
                .map(|x| x.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&s), vec![a, b, c], "new accounts go to the end");

        s.set_order(&[c]).unwrap();
        assert_eq!(order(&s), vec![c, a, b]);
        let positions: Vec<i64> = s.accounts().unwrap().iter().map(|x| x.position).collect();
        assert_eq!(positions, vec![1, 2, 3]);
        s.move_account(c, 3).unwrap();
        assert_eq!(order(&s), vec![a, b, c]);
        s.move_account(b, 1).unwrap();
        assert_eq!(order(&s), vec![b, a, c]);
        s.move_account(a, 99).unwrap();
        assert_eq!(order(&s), vec![b, c, a], "past the end means last");
        assert!(s.set_order(&[42]).is_err());

        assert_eq!(
            s.account(a).unwrap().unwrap().weekly_limit,
            WeeklyLimitSetting::Auto
        );
        s.set_weekly_limit(a, "none".parse().unwrap()).unwrap();
        assert_eq!(
            s.account(a).unwrap().unwrap().weekly_limit,
            WeeklyLimitSetting::No
        );
        assert!("maybe".parse::<WeeklyLimitSetting>().is_err());
    }

    #[test]
    fn a_stored_login_refreshes_the_plan() {
        let s = Store::open_in_memory().unwrap();
        let a = s
            .insert_account("a", &ident("ua"), AccountState::Parked)
            .unwrap();
        s.save_credential(
            a,
            &json!({"refreshToken": "r1", "subscriptionType": "max", "rateLimitTier": "default_claude_max_5x"}),
            "login",
        )
        .unwrap();
        let acct = s.account(a).unwrap().unwrap();
        assert_eq!(acct.subscription_type.as_deref(), Some("max"));
        s.save_credential(
            a,
            &json!({"refreshToken": "r2", "rateLimitTier": "default_claude_max_20x"}),
            "sync",
        )
        .unwrap();
        let acct = s.account(a).unwrap().unwrap();
        assert_eq!(
            acct.subscription_type.as_deref(),
            Some("max"),
            "kept when absent"
        );
        assert_eq!(
            acct.rate_limit_tier.as_deref(),
            Some("default_claude_max_20x")
        );
    }

    /// A store written by cc-rotate at schema 2: two fixed usage columns and `priority`.
    #[test]
    fn schema_2_store_is_upgraded_keeping_samples_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("store.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
            conn.execute_batch(
                "INSERT INTO accounts (id, label, account_uuid, state, priority, added_at) VALUES
                    (1, 'max', 'u1', 'active', 5, 0),
                    (2, 'team', 'u2', 'parked', -1, 0),
                    (3, 'tie', 'u3', 'parked', 5, 0);
                 INSERT INTO samples (id, ts, account_id, profile, session_id, five_pct, five_resets_at, week_pct, week_resets_at) VALUES
                    (10, 1000, 1, 'rotate', 's', 40.0, 5000, 70.0, 90000),
                    (11, 2000, 2, 'rotate', NULL, 12.5, 6000, NULL, NULL),
                    (12, 3000, 2, 'work', NULL, NULL, NULL, NULL, NULL);
                 INSERT INTO settings (key, value) VALUES ('swap_threshold_5h', '90');",
            )
            .unwrap();
        }

        let s = Store::open(&db).unwrap();
        let version: i64 = s
            .conn()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        let accounts = s.accounts().unwrap();
        let labels: Vec<_> = accounts.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["team", "max", "tie"],
            "priorities kept as the order"
        );
        assert_eq!(accounts[1].position, 5);
        assert!(
            accounts
                .iter()
                .all(|a| a.weekly_limit == WeeklyLimitSetting::Auto)
        );

        let samples = s.samples_since(0).unwrap();
        assert_eq!(
            samples.len(),
            3,
            "every sample kept, even one without windows"
        );
        assert_eq!(
            samples[0].windows,
            vec![
                SampleWindow {
                    name: "five_hour".into(),
                    pct: 40.0,
                    resets_at: Some(5000)
                },
                SampleWindow {
                    name: "seven_day".into(),
                    pct: 70.0,
                    resets_at: Some(90000)
                },
            ]
        );
        assert_eq!(samples[0].session_id.as_deref(), Some("s"));
        assert_eq!(
            samples[1].windows,
            vec![SampleWindow {
                name: "five_hour".into(),
                pct: 12.5,
                resets_at: Some(6000)
            }],
            "a missing weekly column is no window, not a 0% one"
        );
        assert!(samples[2].windows.is_empty());
        assert_eq!(s.settings().unwrap().swap_threshold_5h, 90.0);

        // New samples, accounts and removals work on the upgraded schema.
        let id = s
            .insert_account("new", &ident("u4"), AccountState::Parked)
            .unwrap();
        assert_eq!(s.account(id).unwrap().unwrap().position, 6);
        s.remove_account(2).unwrap();
        let left: i64 = s
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sample_windows WHERE account_id = 2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "windows go with their samples");

        drop(s);
        let s = Store::open(&db).unwrap();
        assert_eq!(
            s.samples_since(0).unwrap().len(),
            1,
            "opening again changes nothing"
        );
    }

    #[test]
    fn settings_defaults_and_validation() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.settings().unwrap().swap_threshold_5h, 98.0);
        s.set_setting("swap_threshold_5h", Some("90")).unwrap();
        assert_eq!(s.settings().unwrap().swap_threshold_5h, 90.0);
        assert!(s.set_setting("swap_threshold_5h", Some("150")).is_err());
        assert!(s.set_setting("nope", Some("1")).is_err());

        let d = s.settings().unwrap();
        assert!(d.usage_checks);
        assert_eq!(
            (d.usage_check_interval_s, d.usage_check_max_age_s),
            (3600, 900)
        );
        s.set_setting("usage_checks", Some("off")).unwrap();
        assert_eq!(s.setting("usage_checks").unwrap().as_deref(), Some("false"));
        assert!(!s.settings().unwrap().usage_checks);
        assert!(s.set_setting("usage_check_interval_s", Some("60")).is_err());
        s.set_setting("usage_check_interval_s", Some("1800"))
            .unwrap();
        assert_eq!(s.settings().unwrap().usage_check_interval_s, 1800);
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
