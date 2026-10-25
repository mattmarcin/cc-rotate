//! A read-only view of the pool for `status` and the desktop app.

use anyhow::Result;
use serde::Serialize;

use crate::paths::Paths;
use crate::scheduler::{self, Ranked, WeeklyLimit, WindowUsage};
use crate::store::{
    Account, AccountState, Event, Sample, Settings, Store, WeeklyLimitSetting, kind,
};
use crate::swap::{self, META_PROFILE_MODEL};
use crate::usage_check::{self, CheckState};
use crate::{plan, time};

#[derive(Debug, Clone, Serialize)]
pub struct AccountView {
    #[serde(flatten)]
    pub account: Account,
    /// "Max 5x", "Max 20x", "Team 5x", "Pro", or the raw subscription type.
    pub plan: Option<String>,
    /// Every window of the latest reading, as it stands now (0 once a window has reset).
    pub windows: Vec<WindowUsage>,
    pub weekly: WeeklyLimitView,
    /// When the latest reading was taken (epoch ms).
    pub last_sample_at: Option<i64>,
    /// Where it came from: `rotate` (the rotation profile's status line), `check` (a usage
    /// check), or the folder name of another profile (`.claude-work`) whose status line or
    /// cached usage reported it.
    pub last_sample_source: Option<String>,
    /// The account's usage checks.
    pub usage_check: CheckState,
    pub login_expires_at: Option<i64>,
    pub login_updated_at: Option<i64>,
    /// When the account's sign-in ends: after it, the account needs a new sign-in.
    pub refresh_expires_at: Option<i64>,
    /// The sign-in ends within `login_warning_days`.
    pub sign_in_ends_soon: bool,
    /// The account's extra usage, as its latest usage report gave it.
    pub extra_usage: Option<ExtraUsageView>,
    /// Parked and below every threshold that applies: the scheduler may pick it.
    pub eligible: bool,
    /// When this account is usable again, if a window keeps it from being used now.
    pub available_at: Option<i64>,
    /// Place in the pick order (from 1) among the accounts usable now.
    pub rank: Option<usize>,
    /// Why the account is where it is in the pick order.
    pub reason: String,
    /// Its weekly budget would reset unused unless it is used now (`budget::expiring`).
    pub expiring: Option<crate::budget::Expiring>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtraUsageView {
    #[serde(flatten)]
    pub usage: crate::store::ExtraUsage,
    /// When the report was read (epoch ms).
    pub read_at: i64,
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
    /// Accounts whose sign-in ends within `login_warning_days`.
    pub sign_ins_ending: Vec<String>,
    /// Sessions stopped at a usage limit, waiting for an account with room (`waiting`).
    pub sessions_waiting: usize,
    /// A swap stopped half way and is not settled yet (`swap::finish_dead_swap`).
    pub swap_unsettled: bool,
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
    /// How long the pool lasts at the account in use's current pace, while it is being used.
    pub forecast: Option<crate::forecast::Forecast>,
    /// Why warm-ups stopped until the user turns keeping a window running on again (spec
    /// section 22), if they did.
    pub warm_ups_blocked: Option<String>,
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
    let next = scheduler::pick(&pool, policy, active_id, now_s).map(|c| NextPick {
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
        let last = store.last_sample(c.account.id)?;
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
            last_sample_at: last.as_ref().map(|s| s.ts),
            last_sample_source: last.map(|s| s.profile),
            usage_check: usage_check::state(store, c.account.id)?,
            login_expires_at: meta.as_ref().and_then(|m| m.expires_at),
            refresh_expires_at: meta.as_ref().and_then(|m| m.refresh_expires_at),
            sign_in_ends_soon: c.account.state != AccountState::NeedsLogin
                && meta
                    .as_ref()
                    .and_then(|m| m.refresh_expires_at)
                    .is_some_and(|t| {
                        crate::health::sign_in_ends_soon(t, settings.login_warning_days, now_ms)
                    }),
            login_updated_at: meta.as_ref().map(|m| m.updated_at),
            extra_usage: store
                .extra_usage(c.account.id)?
                .map(|(usage, read_at)| ExtraUsageView { usage, read_at }),
            eligible: c.eligible(),
            available_at: c.blocked_until(),
            rank: ranked.and_then(|r| r.rank),
            reason: ranked.map(|r| r.reason.clone()).unwrap_or_default(),
            expiring: policy.expiring(c).copied(),
            account: c.account.clone(),
        });
    }

    let usable: Vec<&scheduler::Candidate> = pool.iter().filter(|c| c.usable()).collect();
    let limited: Vec<&&scheduler::Candidate> = usable
        .iter()
        .filter(|c| c.weekly_limit != WeeklyLimit::None)
        .collect();
    // Up to each account's cap: the rest of a capped week is the user's, not the pool's.
    let weekly_accounts_left = limited
        .iter()
        .map(|c| {
            let limit = c.account.weekly_cap.map_or(100.0, |cap| cap.min(100.0));
            (limit - c.weekly().map_or(0.0, |w| w.pct)).max(0.0) / 100.0
        })
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

    let forecast = match active_id {
        Some(_) => {
            let rates = crate::budget::cached_rates(store)?;
            // Windows are kept running only while warm-ups run.
            let mut pool = pool.clone();
            if crate::usage_check::warm_ups_paused(store, paths)?.is_some() {
                for c in &mut pool {
                    c.account.keep_running = false;
                }
            }
            crate::forecast::pace(store, &rates, now_ms)?
                .map(|pace| crate::forecast::forecast(&pool, policy, &rates, pace, now_s))
                // Already out: the pool summary says so, with when one frees up.
                .filter(|f| f.runs_out_at.is_none_or(|t| t > now_s))
        }
        None => None,
    };
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
            sign_ins_ending: accounts
                .iter()
                .filter(|a| a.sign_in_ends_soon)
                .map(|a| a.account.label.clone())
                .collect(),
            sessions_waiting: store
                .open_waiting_sessions()?
                .iter()
                .filter(|w| w.state == crate::store::WaitingState::Waiting)
                .count(),
            swap_unsettled: swap::pending_swap(store)?.is_some()
                && !crate::lock::is_held(&paths.swap_lock(), crate::lock::CLAUDE_STALE),
        },
        accounts,
        ranking,
        next,
        model: store.meta(META_PROFILE_MODEL)?,
        settings,
        profile_dir: paths.profile_dir.display().to_string(),
        data_dir: paths.data_dir.display().to_string(),
        last_event_id: store.events(1)?.first().map(|e| e.id).unwrap_or(0),
        forecast,
        warm_ups_blocked: crate::usage_check::warm_ups_blocked(store)?,
    })
}

pub fn recent_events(store: &Store, limit: usize) -> Result<Vec<Event>> {
    store.events(limit)
}

pub fn series(store: &Store, since_ms: i64) -> Result<Vec<Sample>> {
    store.samples_since(since_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AccountIdentity, SampleWindow};

    /// `cc-rotate-ctl watch` redraws from snapshots every few seconds, so building one must never
    /// write to the store or touch the profile.
    #[test]
    fn a_snapshot_never_writes() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::new(root.path().join("data"), root.path().join("profile"));
        let store = Store::open(&paths.data_dir.join("store.db")).unwrap();
        let now = time::now_ms();
        for (i, state) in [
            AccountState::Active,
            AccountState::Parked,
            AccountState::Paused,
        ]
        .into_iter()
        .enumerate()
        {
            let id = store
                .insert_account(
                    &format!("a{i}"),
                    &AccountIdentity {
                        account_uuid: Some(format!("u{i}")),
                        ..Default::default()
                    },
                    state,
                )
                .unwrap();
            store
                .record_sample(&Sample {
                    ts: now - 60_000,
                    account_id: id,
                    profile: "rotate".into(),
                    session_id: None,
                    model: None,
                    windows: vec![SampleWindow {
                        name: "five_hour".into(),
                        pct: 99.0,
                        resets_at: Some(now / 1000 + 3600),
                    }],
                })
                .unwrap();
            store
                .log_event(kind::SWAP, None, Some(id), Some("manual"), None)
                .unwrap();
        }
        let before = store.conn().total_changes();
        let snap = snapshot(&store, &paths).unwrap();
        recent_events(&store, 10).unwrap();
        assert_eq!(snap.accounts.len(), 3);
        assert_eq!(store.conn().total_changes(), before, "no row written");
        assert!(!paths.profile_dir.exists(), "the profile is left alone");
    }
}
