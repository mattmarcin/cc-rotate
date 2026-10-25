//! A read-only view of the pool for `status` and the desktop app.

use anyhow::Result;
use serde::Serialize;

use crate::paths::Paths;
use crate::scheduler::{self, Ranked, WeeklyLimit, WindowUsage};
use crate::store::{
    Account, AccountState, Event, Sample, Settings, Store, WeeklyLimitSetting, kind,
};
use crate::swap::{self, META_PROFILE_MODEL};
use crate::{plan, time};

#[derive(Debug, Clone, Serialize)]
pub struct AccountView {
    #[serde(flatten)]
    pub account: Account,
    /// "Max 5x", "Team Premium", "Pro", or the raw subscription type.
    pub plan: Option<String>,
    /// Every window of the latest reading, as it stands now (0 once a window has reset).
    pub windows: Vec<WindowUsage>,
    pub weekly: WeeklyLimitView,
    /// When the latest reading was taken (epoch ms).
    pub last_sample_at: Option<i64>,
    pub login_expires_at: Option<i64>,
    pub login_updated_at: Option<i64>,
    pub refresh_expires_at: Option<i64>,
    /// Parked and below every threshold that applies: the scheduler may pick it.
    pub eligible: bool,
    /// When this account is usable again, if a window keeps it from being used now.
    pub available_at: Option<i64>,
    /// Place in the pick order (from 1) among the accounts usable now.
    pub rank: Option<usize>,
    /// Why the account is where it is in the pick order.
    pub reason: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct WeeklyLimitView {
    /// The user's setting: detect it, or say it has one or not.
    pub setting: WeeklyLimitSetting,
    pub detected: WeeklyLimit,
    /// What the scheduler uses: the setting, or what was detected.
    pub effective: WeeklyLimit,
}

#[derive(Debug, Clone, Serialize)]
pub struct NextPick {
    pub account_id: i64,
    pub label: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PoolSummary {
    /// Weekly budget left across the usable accounts that have a weekly limit, in units of one
    /// account.
    pub weekly_accounts_left: f64,
    /// Usable accounts with a weekly limit (or one not known yet).
    pub weekly_limited: usize,
    /// Usable accounts without a weekly limit.
    pub no_weekly_limit: usize,
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
    /// In the user's order.
    pub accounts: Vec<AccountView>,
    /// Every account in the order the scheduler would use them, with a reason for each.
    pub ranking: Vec<Ranked>,
    /// The account the next swap would pick.
    pub next: Option<NextPick>,
    /// The model the rotation profile's sessions last used.
    pub model: Option<String>,
    pub pool: PoolSummary,
    pub settings: Settings,
    pub profile_dir: String,
    pub data_dir: String,
    pub last_event_id: i64,
}

pub fn snapshot(store: &Store, paths: &Paths) -> Result<Snapshot> {
    snapshot_at(store, paths, time::now_ms())
}

pub fn snapshot_at(store: &Store, paths: &Paths, now_ms: i64) -> Result<Snapshot> {
    let now_s = now_ms / 1000;
    let settings = store.settings()?;
    let (pool, policy) = swap::load_pool(store, now_s)?;
    let ranking = scheduler::ranking(&pool, policy, now_s);
    let active_id = pool
        .iter()
        .find(|c| c.account.state == AccountState::Active)
        .map(|c| c.account.id);
    let next = scheduler::pick(&pool, policy, active_id).map(|c| NextPick {
        account_id: c.account.id,
        label: c.account.label.clone(),
        reason: ranking
            .iter()
            .find(|r| r.account_id == c.account.id)
            .map(|r| r.reason.clone())
            .unwrap_or_default(),
    });

    let mut accounts = Vec::new();
    for c in &pool {
        let meta = store.credential_meta(c.account.id)?;
        let ranked = ranking.iter().find(|r| r.account_id == c.account.id);
        accounts.push(AccountView {
            plan: plan::label(
                c.account.subscription_type.as_deref(),
                c.account.rate_limit_tier.as_deref(),
            ),
            windows: c.windows.clone(),
            weekly: WeeklyLimitView {
                setting: c.account.weekly_limit,
                detected: c.weekly_detected,
                effective: c.weekly_limit,
            },
            last_sample_at: store.last_sample(c.account.id)?.map(|s| s.ts),
            login_expires_at: meta.as_ref().and_then(|m| m.expires_at),
            refresh_expires_at: meta.as_ref().and_then(|m| m.refresh_expires_at),
            login_updated_at: meta.as_ref().map(|m| m.updated_at),
            eligible: c.eligible(),
            available_at: c.blocked_until(),
            rank: ranked.and_then(|r| r.rank),
            reason: ranked.map(|r| r.reason.clone()).unwrap_or_default(),
            account: c.account.clone(),
        });
    }

    let usable: Vec<&scheduler::Candidate> = pool.iter().filter(|c| c.usable()).collect();
    let limited: Vec<&&scheduler::Candidate> = usable
        .iter()
        .filter(|c| c.weekly_limit != WeeklyLimit::None)
        .collect();
    let weekly_accounts_left = limited
        .iter()
        .map(|c| (100.0 - c.weekly().map_or(0.0, |w| w.pct)).max(0.0) / 100.0)
        .sum::<f64>();
    let eligible = pool.iter().filter(|c| c.eligible()).count();
    let active = pool
        .iter()
        .find(|c| c.account.state == AccountState::Active);
    let active_blocked = active.is_some_and(|c| c.blocking().is_some());
    let next_available_at = usable.iter().filter_map(|c| c.blocked_until()).min();
    let day_ago = now_ms - 86_400_000;
    let swaps_24h = store
        .events(500)?
        .iter()
        // A swap with no outgoing account is the first activation, not a rotation.
        .filter(|e| e.kind == kind::SWAP && e.account_from.is_some() && e.ts >= day_ago)
        .count();

    Ok(Snapshot {
        now_ms,
        active_id,
        pool: PoolSummary {
            weekly_accounts_left,
            weekly_limited: limited.len(),
            no_weekly_limit: usable.len() - limited.len(),
            usable: usable.len(),
            eligible,
            exhausted: eligible == 0 && (active_blocked || active.is_none()),
            next_available_at,
            swaps_24h,
            needs_login: pool
                .iter()
                .filter(|c| c.account.state == AccountState::NeedsLogin)
                .map(|c| c.account.label.clone())
                .collect(),
        },
        accounts,
        ranking,
        next,
        model: store.meta(META_PROFILE_MODEL)?,
        settings,
        profile_dir: paths.profile_dir.display().to_string(),
        data_dir: paths.data_dir.display().to_string(),
        last_event_id: store.events(1)?.first().map(|e| e.id).unwrap_or(0),
    })
}

pub fn recent_events(store: &Store, limit: usize) -> Result<Vec<Event>> {
    store.events(limit)
}

pub fn series(store: &Store, since_ms: i64) -> Result<Vec<Sample>> {
    store.samples_since(since_ms)
}
