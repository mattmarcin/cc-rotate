//! A read-only view of the pool for `status` and the desktop app.

use anyhow::Result;
use serde::Serialize;

use crate::paths::Paths;
use crate::scheduler::{Thresholds, Usage};
use crate::store::{Account, AccountState, Event, Sample, Settings, Store, kind};
use crate::time;

#[derive(Debug, Clone, Serialize)]
pub struct AccountView {
    #[serde(flatten)]
    pub account: Account,
    /// Effective usage now (0 once a window has reset).
    pub usage: Usage,
    pub last_sample: Option<Sample>,
    pub login_expires_at: Option<i64>,
    pub login_updated_at: Option<i64>,
    pub refresh_expires_at: Option<i64>,
    /// Parked and below both thresholds: the scheduler may pick it.
    pub eligible: bool,
    /// When this account next drops below the thresholds, if it is above one now.
    pub available_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PoolSummary {
    /// Weekly budget left across active and parked accounts, in units of one account.
    pub weekly_accounts_left: f64,
    pub usable: usize,
    pub eligible: usize,
    pub exhausted: bool,
    /// Earliest time an account that is over a threshold becomes usable again (epoch seconds).
    pub next_available_at: Option<i64>,
    pub swaps_24h: usize,
    pub needs_login: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub now_ms: i64,
    pub active_id: Option<i64>,
    pub accounts: Vec<AccountView>,
    pub pool: PoolSummary,
    pub settings: Settings,
    pub profile_dir: String,
    pub data_dir: String,
    pub last_event_id: i64,
}

pub fn snapshot(store: &Store, paths: &Paths) -> Result<Snapshot> {
    let now_ms = time::now_ms();
    let now_s = now_ms / 1000;
    let settings = store.settings()?;
    let thresholds = Thresholds::from(&settings);

    let mut accounts = Vec::new();
    for account in store.accounts()? {
        let last_sample = store.last_sample(account.id)?;
        let usage = Usage::from_sample(last_sample.as_ref(), now_s);
        let meta = store.credential_meta(account.id)?;
        let available_at = last_sample
            .as_ref()
            .and_then(|s| blocked_until(s, usage, thresholds));
        accounts.push(AccountView {
            eligible: account.state == AccountState::Parked && usage.below(thresholds),
            usage,
            login_expires_at: meta.as_ref().and_then(|m| m.expires_at),
            refresh_expires_at: meta.as_ref().and_then(|m| m.refresh_expires_at),
            login_updated_at: meta.as_ref().map(|m| m.updated_at),
            last_sample,
            available_at,
            account,
        });
    }

    let usable: Vec<&AccountView> = accounts
        .iter()
        .filter(|a| matches!(a.account.state, AccountState::Active | AccountState::Parked))
        .collect();
    let weekly_accounts_left = usable
        .iter()
        .map(|a| (100.0 - a.usage.weekly).max(0.0) / 100.0)
        .sum::<f64>();
    let eligible = accounts.iter().filter(|a| a.eligible).count();
    let active_blocked = accounts
        .iter()
        .any(|a| a.account.state == AccountState::Active && !a.usage.below(thresholds));
    let next_available_at = usable.iter().filter_map(|a| a.available_at).min();
    let day_ago = now_ms - 86_400_000;
    let swaps_24h = store
        .events(500)?
        .iter()
        .filter(|e| e.kind == kind::SWAP && e.ts >= day_ago)
        .count();

    Ok(Snapshot {
        now_ms,
        active_id: accounts
            .iter()
            .find(|a| a.account.state == AccountState::Active)
            .map(|a| a.account.id),
        pool: PoolSummary {
            weekly_accounts_left,
            usable: usable.len(),
            eligible,
            exhausted: eligible == 0
                && (active_blocked
                    || !accounts
                        .iter()
                        .any(|a| a.account.state == AccountState::Active)),
            next_available_at,
            swaps_24h,
            needs_login: accounts
                .iter()
                .filter(|a| a.account.state == AccountState::NeedsLogin)
                .map(|a| a.account.label.clone())
                .collect(),
        },
        accounts,
        settings,
        profile_dir: paths.profile_dir.display().to_string(),
        data_dir: paths.data_dir.display().to_string(),
        last_event_id: store.events(1)?.first().map(|e| e.id).unwrap_or(0),
    })
}

fn blocked_until(sample: &Sample, usage: Usage, t: Thresholds) -> Option<i64> {
    let mut until = None;
    if usage.weekly >= t.weekly {
        until = sample.week_resets_at;
    }
    if usage.five_hour >= t.five_hour {
        until = until.max(sample.five_resets_at);
    }
    until
}

pub fn recent_events(store: &Store, limit: usize) -> Result<Vec<Event>> {
    store.events(limit)
}

pub fn series(store: &Store, since_ms: i64) -> Result<Vec<Sample>> {
    store.samples_since(since_ms)
}
