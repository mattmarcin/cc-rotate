//! The swap protocol (spec section 6).
//!
//! Ordering is what makes this safe against Claude Code's token refresh:
//! 1. cc-rotate's own swap lock serializes swaps across processes.
//! 2. Claude Code's two refresh locks are held while the outgoing login is captured and the
//!    target login is written, so no refresh can be in flight. A refresh that finished before
//!    we took the locks is in the file and gets captured; one that starts after we release them
//!    re-reads the file under the lock and refreshes the target.
//! 3. Claude Code's credentials write lock (`.storage-write.lock`) is held for the read and
//!    write as well, so no other read-modify-write of the credentials (an MCP login, say) can
//!    put the outgoing login back.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::json;

use crate::lock::{DirLock, LockError, LockOptions};
use crate::paths::Paths;
use crate::scheduler::{self, Candidate, Policy, Reason, WeeklyEvidence, WeeklyLimit, WindowKind};
use crate::store::{Account, AccountState, Sample, SampleWindow, Store, kind};
use crate::{credfile, secret, time, usage_check};

pub const META_LAST_SWAP_AT: &str = "last_swap_at";
/// The reset times (comma-separated epoch seconds) of every window in the outgoing account's
/// last reading, for the stale-reading guard after a swap. Swaps within `STALE_SAMPLE_GUARD_MS`
/// of each other add to it, so a session that has not made a request since two swaps ago is
/// recognized too.
pub const META_LAST_SWAP_FROM_RESETS: &str = "last_swap_from_resets";
/// The fingerprint of the login a swap is writing into the profile, until it commits.
pub const META_SWAP_PENDING_FP: &str = "swap_pending_fp";
/// Prefix of the meta keys holding, per account, the reset times known to be its own windows'
/// (a JSON list of epoch seconds; only those still ahead count). See `attribute`.
pub const META_OWN_RESETS_PREFIX: &str = "own_resets:";
/// Window after a swap during which readings carrying the reset times of an account just left
/// are treated as stale readings of that account (sessions that have not made a request yet).
pub const STALE_SAMPLE_GUARD_MS: i64 = 120_000;
/// When the active account became active: a swap, or a login adopted from the profile.
pub const META_ACTIVE_SINCE: &str = "active_since";
/// The model id the rotation profile's sessions last reported, which decides whether a window
/// scoped to one model family applies.
pub const META_PROFILE_MODEL: &str = "profile_model_seen";
/// The credentials change marker (`Credentials::change_marker`) last seen. Named for the file
/// mtime it held before the marker could also be a Keychain fingerprint.
pub const META_CRED_SEEN: &str = "cred_mtime_seen";
/// Set when the profile is signed in to an account outside the pool; a swap clears it.
pub const META_PROFILE_FOREIGN: &str = "profile_foreign";

const POOL_EXHAUSTED_LOG_INTERVAL_MS: i64 = 10 * 60_000;
/// A swap checks at most this many picks with `/usage` before it swaps...
const MAX_PRE_SWAP_CHECKS: usize = 3;
/// ...and starts no check after this long.
const PRE_SWAP_CHECK_BUDGET: Duration = Duration::from_secs(25);
/// Bounds re-picking when the pick keeps changing under the account locks.
const MAX_PICK_ATTEMPTS: usize = 8;
const BACKUPS_KEPT: usize = 7;
const BACKUP_EXT: &str = "ccr";
const LEGACY_BACKUP_EXT: &str = "dpapi";

#[derive(Debug, Clone)]
pub struct SwapRequest {
    /// Only swap if this account is still the active one (idempotency across sessions).
    pub from: Option<i64>,
    /// Swap to this account instead of letting the scheduler choose.
    pub to: Option<i64>,
    pub reason: Reason,
    /// Wait for a concurrent swap to finish instead of skipping.
    pub wait: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SwapOutcome {
    Swapped { from: Option<i64>, to: i64 },
    Skipped { why: String },
    PoolExhausted,
}

/// Every account as the scheduler sees it at `now_s`, in the user's order, with the policy
/// from the settings.
pub fn load_pool(store: &Store, now_s: i64) -> Result<(Vec<Candidate>, Policy)> {
    let policy = Policy::from(&store.settings()?);
    let model = store.meta(META_PROFILE_MODEL)?;
    let accounts = store.accounts()?;
    // Places in the order; accounts at the same position (stores upgraded from `priority`,
    // which was usually 0 for all) share a place.
    let mut positions: Vec<i64> = accounts.iter().map(|a| a.position).collect();
    positions.dedup();
    let mut pool = Vec::new();
    for account in accounts {
        let order = positions
            .iter()
            .position(|&p| p == account.position)
            .unwrap_or_default();
        let sample = store.last_sample(account.id)?;
        let windows =
            scheduler::window_usage(sample.as_ref(), now_s, policy.thresholds, model.as_deref());
        let detected = detect_weekly_limit(store, account.id, now_s)?;
        pool.push(Candidate {
            weekly_limit: scheduler::weekly_limit(account.weekly_limit, detected),
            weekly_detected: detected,
            windows,
            order,
            held_back: usage_check::state(store, account.id)?.held_back,
            account,
        });
    }
    Ok((pool, policy))
}

/// What the account's recorded usage says about its weekly limit (`scheduler::WeeklyEvidence`).
pub fn detect_weekly_limit(store: &Store, account_id: i64, now_s: i64) -> Result<WeeklyLimit> {
    let names = store.window_names(account_id)?;
    let mut session_resets = Vec::new();
    for name in &names {
        let spec = scheduler::classify(name);
        if spec.kind == WindowKind::Session && spec.all_models() {
            session_resets.extend(store.window_resets_since(
                account_id,
                name,
                now_s - scheduler::NO_LIMIT_LOOKBACK_S,
            )?);
        }
    }
    Ok(scheduler::detect_weekly_limit(
        WeeklyEvidence::from_history(&names, &session_resets),
    ))
}

/// The account the scheduler would pick right now, excluding `exclude`.
pub fn next_target(store: &Store, exclude: Option<i64>) -> Result<Option<Account>> {
    let (pool, policy) = load_pool(store, time::now_s())?;
    Ok(scheduler::pick(&pool, policy, exclude).map(|c| c.account.clone()))
}

/// Where a status line reading of the active account belongs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attribution {
    /// Clearly the active account's: it shares no reset time with another account's latest
    /// reading, or it has one of the active account's own.
    Own,
    /// It shares a reset time with another account and the active account's own reset times
    /// are not known: it may be either. Counted, since hiding a genuine reading of a used-up
    /// account would leave its sessions waiting at the limit; a swap it triggers confirms the
    /// account's usage first where it can (section 20).
    Ambiguous,
    /// Another account's: it has another account's reset times, and the active account's are
    /// known and different. A session only notices a new login when it next starts a request
    /// (F1), so one idle since before a swap, or one whose request started before it, still
    /// shows the account it used then.
    Elsewhere,
}

/// Reset times within this many seconds are the same: the status line and `/usage` may round
/// a reset time differently.
const SAME_RESET_S: i64 = 60;

pub fn same_reset(a: i64, b: i64) -> bool {
    (a - b).abs() <= SAME_RESET_S
}

fn own_resets_key(account_id: i64) -> String {
    format!("{META_OWN_RESETS_PREFIX}{account_id}")
}

/// An account's own reset times that have not passed yet (`META_OWN_RESETS_PREFIX`).
pub fn known_resets(store: &Store, account_id: i64, now_s: i64) -> Result<Vec<i64>> {
    let resets: Vec<i64> = store
        .meta(&own_resets_key(account_id))?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default();
    Ok(resets.into_iter().filter(|&r| r > now_s).collect())
}

/// Add reset times known to be the account's own: from a reading clearly its own, a usage check,
/// another profile signed in to it, or observe mode. Never from an ambiguous reading.
pub fn note_known_resets(
    store: &Store,
    account_id: i64,
    windows: &[SampleWindow],
    now_s: i64,
) -> Result<()> {
    let fresh: Vec<i64> = windows
        .iter()
        .filter_map(|w| w.resets_at)
        .filter(|&r| r > now_s)
        .collect();
    let known = known_resets(store, account_id, now_s)?;
    if fresh
        .iter()
        .all(|&r| known.iter().any(|&k| same_reset(k, r)))
    {
        return Ok(());
    }
    store.immediate(|s| {
        let mut resets = known_resets(s, account_id, now_s)?;
        let before = resets.clone();
        for r in fresh {
            if !resets.iter().any(|&k| same_reset(k, r)) {
                resets.push(r);
            }
        }
        if resets == before {
            return Ok(());
        }
        resets.sort_unstable();
        s.set_meta(&own_resets_key(account_id), serde_json::to_string(&resets)?)
    })
}

/// Where a status line reading of the active account belongs (`Attribution`).
pub fn attribute(
    store: &Store,
    sample: &Sample,
    active_id: i64,
    now_s: i64,
) -> Result<Attribution> {
    let others = others_latest(store, active_id)?;
    let other_resets: Vec<i64> = others
        .iter()
        .flat_map(|s| &s.windows)
        .filter_map(|w| w.resets_at)
        .filter(|&r| r > now_s)
        .collect();
    let resets: Vec<i64> = sample.windows.iter().filter_map(|w| w.resets_at).collect();
    let among = |set: &[i64]| {
        resets
            .iter()
            .any(|&r| set.iter().any(|&k| same_reset(k, r)))
    };
    if !among(&other_resets) {
        return Ok(Attribution::Own);
    }
    let own = known_resets(store, active_id, now_s)?;
    if !among(&own) {
        return Ok(if own.is_empty() {
            Attribution::Ambiguous
        } else {
            Attribution::Elsewhere
        });
    }
    // Reset times are aligned, so another account's can be the active one's too; then the
    // values decide. A session still showing another account repeats that account's latest
    // reading exactly; a genuine reading of the active account is that only by coincidence.
    if copies_another(sample, &others) {
        let active_latest = store.last_sample(active_id)?;
        let is_active_latest =
            active_latest.is_some_and(|l| crate::store::same_windows(&l.windows, &sample.windows));
        if !is_active_latest {
            return Ok(Attribution::Elsewhere);
        }
    }
    Ok(Attribution::Own)
}

/// The latest reading of every account other than `account_id`.
pub fn others_latest(store: &Store, account_id: i64) -> Result<Vec<Sample>> {
    let mut out = Vec::new();
    for account in store.accounts()? {
        if account.id != account_id {
            if let Some(last) = store.last_sample(account.id)? {
                out.push(last);
            }
        }
    }
    Ok(out)
}

/// Whether a reading repeats one of these readings exactly (same windows, reset times and
/// percentages): what a session still showing that account reports.
pub fn copies_another(sample: &Sample, others: &[Sample]) -> bool {
    others
        .iter()
        .any(|o| crate::store::same_windows(&o.windows, &sample.windows))
}

/// Whether the threshold the active account's latest reading reached may not be its own: the
/// reading is from the status line and a window over its threshold has a reset time not known
/// to be the account's (section 8). Then a threshold swap confirms first.
fn needs_confirmation(store: &Store, active_id: i64, now_ms: i64) -> Result<bool> {
    let Some(last) = store.last_sample(active_id)? else {
        return Ok(false);
    };
    if last.profile != "rotate" {
        return Ok(false);
    }
    let (pool, _) = load_pool(store, now_ms / 1000)?;
    let Some(active) = pool.iter().find(|c| c.account.id == active_id) else {
        return Ok(false);
    };
    let own = known_resets(store, active_id, now_ms / 1000)?;
    Ok(active.windows.iter().filter(|w| w.blocking()).any(|w| {
        w.resets_at
            .is_none_or(|r| !own.iter().any(|&k| same_reset(k, r)))
    }))
}

/// The reset times the stale-reading guard currently knows (`META_LAST_SWAP_FROM_RESETS`).
pub fn left_resets(store: &Store) -> Result<Vec<i64>> {
    Ok(store
        .meta(META_LAST_SWAP_FROM_RESETS)?
        .unwrap_or_default()
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .filter(|&v: &i64| v != 0)
        .collect())
}

/// Whether the active account was already over a threshold when it became active: it has
/// readings since then, and every one of them has a window that applies at or above its
/// threshold. Its last reading before the swap was out of date, for example because another
/// profile used it meanwhile. `scheduler::Timing::leaves_at_once` lets such an account go
/// without waiting out the swap cooldown.
///
/// Only readings that are clearly the account's own count: one sharing a reset time with
/// another account's latest reading may be a session that has not made a request since an
/// earlier swap, still reporting the account it used then. Leaving out a genuine reading only
/// means the cooldown applies as usual.
pub fn arrived_over_limit(store: &Store, account_id: i64, now_ms: i64) -> Result<bool> {
    let policy = Policy::from(&store.settings()?);
    let model = store.meta(META_PROFILE_MODEL)?;
    let mut others: Vec<i64> = Vec::new();
    for account in store.accounts()? {
        if account.id != account_id {
            if let Some(sample) = store.last_sample(account.id)? {
                others.extend(sample.windows.iter().filter_map(|w| w.resets_at));
            }
        }
    }
    let own: Vec<_> = store
        .account_samples_since(account_id, active_since(store)?)?
        .into_iter()
        .filter(|s| {
            !s.windows.iter().any(|w| {
                w.resets_at
                    .is_some_and(|r| others.iter().any(|&o| same_reset(o, r)))
            })
        })
        .collect();
    Ok(!own.is_empty()
        && own.iter().all(|s| {
            scheduler::window_usage(Some(s), now_ms / 1000, policy.thresholds, model.as_deref())
                .iter()
                .any(scheduler::WindowUsage::blocking)
        }))
}

/// When the active account became active (epoch ms), falling back to the last swap for stores
/// written before this was recorded.
pub fn active_since(store: &Store) -> Result<i64> {
    Ok(store
        .meta_i64(META_ACTIVE_SINCE)?
        .or(store.meta_i64(META_LAST_SWAP_AT)?)
        .unwrap_or(0))
}

pub fn run(store: &Store, paths: &Paths, req: &SwapRequest) -> Result<SwapOutcome> {
    let settings = store.settings()?;
    let lock_timeout = Duration::from_millis(settings.lock_timeout_ms);

    let swap_lock_opts = LockOptions::claude(if req.wait {
        lock_timeout
    } else {
        Duration::ZERO
    });
    let _swap_lock = match DirLock::acquire(&paths.swap_lock(), swap_lock_opts) {
        Ok(lock) => lock,
        Err(LockError::Busy(_)) => return skip(store, req, "another swap is in progress"),
        Err(e) => return Err(e.into()),
    };

    let now = time::now_ms();
    let active = store.active_account()?;
    let active_id = active.as_ref().map(|a| a.id);

    if let Some(from) = req.from {
        if active_id != Some(from) {
            return skip(store, req, "the active account already changed");
        }
    }
    let mut arrived_full = false;
    if req.reason.respects_cooldown() {
        if let Some(last) = store.meta_i64(META_LAST_SWAP_AT)? {
            let cooldown_ms = settings.swap_cooldown_s * 1000;
            if now - last < cooldown_ms {
                arrived_full = req.reason.is_threshold()
                    && match active_id {
                        Some(id) => arrived_over_limit(store, id, now)?,
                        None => false,
                    };
                if !arrived_full {
                    return skip(store, req, "within the swap cooldown");
                }
            }
        }
    }
    if req.reason == Reason::PreferredAvailable
        && now - active_since(store)? < settings.switch_back_dwell_s * 1000
    {
        return skip(
            store,
            req,
            "the active account has not been in use long enough to switch back",
        );
    }

    // What the user's other profiles read since, so an account an agent used up there is not
    // picked. Best effort: an unreadable profile must not stop the swap.
    let _ = crate::profile_usage::import(store, paths, now);

    // A threshold swap that rests on a reading the status line could not attribute for sure
    // (section 8) first reads the account in use's own usage (section 20.4). When that is not
    // possible, the swap goes ahead as the reading says.
    let mut confirmed = None;
    if req.reason.is_threshold() {
        if let Some(account) = &active {
            if needs_confirmation(store, account.id, now)? {
                let outcome = usage_check::confirm_active(store, paths).unwrap_or_else(|e| {
                    usage_check::CheckOutcome::Failed {
                        why: format!("{e:#}"),
                    }
                });
                confirmed = Some(outcome.kind());
                if matches!(outcome, usage_check::CheckOutcome::Read { .. }) {
                    let (pool, _) = load_pool(store, time::now_s())?;
                    let fine = pool
                        .iter()
                        .find(|c| c.account.id == account.id)
                        .is_some_and(|c| c.blocking().is_none());
                    if fine {
                        return skip(
                            store,
                            req,
                            "the account in use is below its limits by its own usage reading",
                        );
                    }
                }
            }
        }
    }

    // Pick the target. A pick whose latest reading is old is checked first with Claude Code's
    // `/usage` (section 20), and the pick is made again with what that found, so a swap does
    // not land on an account that was used up elsewhere. Then the target's account lock is
    // taken, which waits for a background check of it to finish, and the target is settled
    // again under it. An account whose lock or leftovers keep it busy is passed over.
    let check_deadline = Instant::now() + PRE_SWAP_CHECK_BUDGET;
    let mut checked: Vec<serde_json::Value> = Vec::new();
    let mut checked_ids: Vec<i64> = Vec::new();
    let mut passed_over: Vec<i64> = Vec::new();
    let mut attempts = 0;
    let (target, _target_lock, pool, policy) = loop {
        attempts += 1;
        let last_try = req.to.is_some() || attempts >= MAX_PICK_ATTEMPTS;
        let (mut pool, policy) = load_pool(store, time::now_s())?;
        pool.retain(|c| !passed_over.contains(&c.account.id));
        let target = match choose(store, req, &pool, policy, active.as_ref())? {
            Choice::Target(account) => account,
            Choice::Done(outcome) => return Ok(outcome),
        };
        if req.to.is_none()
            && checked_ids.len() < MAX_PRE_SWAP_CHECKS
            && !checked_ids.contains(&target.id)
            && Instant::now() < check_deadline
            && usage_check::due_before_swap(store, &target, time::now_ms())?
        {
            let outcome =
                usage_check::check(store, paths, target.id, usage_check::Trigger::BeforeSwap)
                    .unwrap_or_else(|e| usage_check::CheckOutcome::Failed {
                        why: format!("{e:#}"),
                    });
            checked.push(usage_check::summary(store, target.id, &outcome));
            checked_ids.push(target.id);
            continue;
        }
        let lock = match DirLock::acquire(
            &paths.account_lock(target.id),
            LockOptions::claude(lock_timeout),
        ) {
            Ok(lock) => lock,
            Err(_) if !last_try => {
                passed_over.push(target.id);
                continue;
            }
            Err(e) => {
                return Err(log_error(
                    store,
                    req,
                    &target,
                    format!("could not take the lock of {}'s login: {e}", target.label),
                ));
            }
        };
        if let usage_check::Recovery::Busy(why) = usage_check::recover(store, paths, target.id)? {
            if !last_try {
                passed_over.push(target.id);
                continue;
            }
            return Err(log_error(
                store,
                req,
                &target,
                format!("{} cannot be used now: {why}", target.label),
            ));
        }
        // Under the lock: a check that finished while it was awaited may have found the target
        // used up, or its login rejected, or one in doubt.
        let (mut pool, policy) = load_pool(store, time::now_s())?;
        pool.retain(|c| !passed_over.contains(&c.account.id));
        match choose(store, req, &pool, policy, active.as_ref())? {
            Choice::Target(settled) if settled.id == target.id => {
                if req.to.is_some() && usage_check::state(store, target.id)?.held_back {
                    return Err(anyhow!(
                        "{} is waiting for a usage check to confirm its login; check it first",
                        target.label
                    ));
                }
                break (settled, lock, pool, policy);
            }
            Choice::Target(_) if !last_try => continue,
            Choice::Target(_) => {
                return skip(
                    store,
                    req,
                    "the accounts kept changing while the swap waited for them",
                );
            }
            Choice::Done(outcome) => return Ok(outcome),
        }
    };
    let now = time::now_ms();
    let now_s = now / 1000;
    let ranking = scheduler::ranking(&pool, policy, now_s);
    let reason_for = |id: i64| {
        ranking
            .iter()
            .find(|r| r.account_id == id)
            .map(|r| r.reason.clone())
    };
    let outgoing = pool.iter().find(|c| Some(c.account.id) == active_id);
    let why = match (req.reason, outgoing) {
        (Reason::PreferredAvailable, Some(from)) => Some(format!(
            "{} ranks above {}: {}",
            target.label,
            from.account.label,
            reason_for(target.id).unwrap_or_default()
        )),
        _ => None,
    };
    let trigger_window = outgoing
        .and_then(Candidate::blocking)
        .map(|w| w.name.clone());

    let target_login = store
        .credential(target.id)?
        .ok_or_else(|| anyhow!("{} has no stored login", target.label))?;

    let claude_lock_opts = LockOptions::claude(lock_timeout);
    let refresh_lock = DirLock::acquire(&paths.refresh_lock(), claude_lock_opts).map_err(|e| {
        log_error(
            store,
            req,
            &target,
            format!("could not take Claude Code's refresh lock: {e}"),
        )
    })?;
    let legacy_lock =
        DirLock::acquire(&paths.legacy_refresh_lock(), claude_lock_opts).map_err(|e| {
            log_error(
                store,
                req,
                &target,
                format!("could not take Claude Code's legacy refresh lock: {e}"),
            )
        })?;
    let storage_lock = DirLock::acquire(
        &paths.storage_lock(),
        LockOptions::claude_storage(lock_timeout),
    )
    .map_err(|e| {
        log_error(
            store,
            req,
            &target,
            format!("could not take Claude Code's credentials write lock: {e}"),
        )
    })?;

    let creds = paths.credentials();
    let previous = creds.read_oauth()?;
    let mut outgoing_state = AccountState::Parked;
    if let Some(account) = &active {
        match &previous {
            Some(oauth) if !credfile::is_dead(oauth) => {
                store.save_credential(account.id, oauth, "swap_out")?
            }
            Some(_) => outgoing_state = AccountState::NeedsLogin,
            None => {}
        }
        if req.reason == Reason::DeadLogin {
            outgoing_state = AccountState::NeedsLogin;
        }
    }

    backup_daily(store, paths, now).context("backing up the profile's credentials")?;
    // Until the swap commits, the store still names the outgoing account as active; a status
    // line write-back meanwhile must not store the target's login as the outgoing account's.
    store.set_meta(META_SWAP_PENDING_FP, &target_login.refresh_fp)?;
    if let Err(e) = creds.write_oauth(&target_login.oauth) {
        let _ = store.delete_meta(META_SWAP_PENDING_FP);
        return Err(e);
    }
    let marker = creds.change_marker()?;

    let from_sample = match active_id {
        Some(id) => store.last_sample(id)?,
        None => None,
    };
    let previous_swap = store.meta_i64(META_LAST_SWAP_AT)?;
    let committed = store.immediate(|s| {
        if let Some(account) = &active {
            s.set_state(account.id, outgoing_state)?;
        }
        s.set_state(target.id, AccountState::Active)?;
        s.set_meta(META_LAST_SWAP_AT, now)?;
        s.set_meta(META_PROFILE_FOREIGN, "0")?;
        s.set_meta(META_ACTIVE_SINCE, now)?;
        let mut resets: Vec<i64> = match previous_swap {
            Some(last) if now - last <= STALE_SAMPLE_GUARD_MS => left_resets(s)?,
            _ => Vec::new(),
        };
        resets.extend(
            from_sample
                .iter()
                .flat_map(|x| &x.windows)
                .filter_map(|w| w.resets_at),
        );
        resets.sort_unstable();
        resets.dedup();
        let resets: Vec<String> = resets.iter().map(i64::to_string).collect();
        s.set_meta(META_LAST_SWAP_FROM_RESETS, resets.join(","))?;
        if let Some(marker) = &marker {
            s.set_meta(META_CRED_SEEN, marker)?;
        }
        let from_windows: serde_json::Map<String, serde_json::Value> = from_sample
            .iter()
            .flat_map(|x| &x.windows)
            .map(|w| (w.name.clone(), json!(w.pct)))
            .collect();
        let mut detail = json!({
            "from_label": active.as_ref().map(|a| a.label.clone()),
            "to_label": target.label,
            "from_state": outgoing_state.as_str(),
            "from_windows": from_windows,
            "pick": reason_for(target.id),
        });
        if let Some(window) = &trigger_window {
            detail["window"] = json!(window);
        }
        if let Some(why) = &why {
            detail["why"] = json!(why);
        }
        if arrived_full {
            detail["arrived_over_limit"] = json!(true);
        }
        if !checked.is_empty() {
            detail["checked"] = json!(checked);
        }
        if let Some(outcome) = confirmed {
            detail["confirmed"] = json!(outcome);
        }
        s.delete_meta(META_SWAP_PENDING_FP)?;
        s.log_event(
            kind::SWAP,
            active_id,
            Some(target.id),
            Some(req.reason.as_str()),
            Some(detail),
        )?;
        Ok(())
    });

    if let Err(e) = committed {
        // Put the outgoing login back so the profile and the store agree again.
        if let Some(oauth) = &previous {
            let _ = creds.write_oauth(oauth);
        }
        let _ = store.delete_meta(META_SWAP_PENDING_FP);
        drop(storage_lock);
        drop(legacy_lock);
        drop(refresh_lock);
        return Err(e.context("recording the swap failed; the previous login was restored"));
    }

    drop(storage_lock);
    drop(legacy_lock);
    drop(refresh_lock);
    Ok(SwapOutcome::Swapped {
        from: active_id,
        to: target.id,
    })
}

/// Write a fresh login for the account that is already active (a re-login of the active
/// account), under Claude Code's refresh locks like any swap.
pub fn install_active_login(store: &Store, paths: &Paths, oauth: &serde_json::Value) -> Result<()> {
    let timeout = Duration::from_millis(store.settings()?.lock_timeout_ms);
    let opts = LockOptions::claude(timeout);
    let _swap_lock = DirLock::acquire(&paths.swap_lock(), opts)?;
    let _refresh = DirLock::acquire(&paths.refresh_lock(), opts)?;
    let _legacy = DirLock::acquire(&paths.legacy_refresh_lock(), opts)?;
    let _storage = DirLock::acquire(&paths.storage_lock(), LockOptions::claude_storage(timeout))?;
    let creds = paths.credentials();
    creds.write_oauth(oauth)?;
    if let Some(marker) = creds.change_marker()? {
        store.set_meta(META_CRED_SEEN, marker)?;
    }
    Ok(())
}

enum Choice {
    Target(Account),
    /// No swap: skipped or no account left, already logged.
    Done(SwapOutcome),
}

/// The account a request swaps to: the one it names, the switch-back target, or the pick.
fn choose(
    store: &Store,
    req: &SwapRequest,
    pool: &[Candidate],
    policy: Policy,
    active: Option<&Account>,
) -> Result<Choice> {
    let target = match req.to {
        Some(id) => {
            let account = store
                .account(id)?
                .ok_or_else(|| anyhow!("account {id} does not exist"))?;
            match account.state {
                AccountState::Active => {
                    return Ok(Choice::Done(skip(
                        store,
                        req,
                        "that account is already active",
                    )?));
                }
                AccountState::NeedsLogin => {
                    return Err(anyhow!(
                        "{} needs a new login before it can be used",
                        account.label
                    ));
                }
                AccountState::Parked | AccountState::Paused => account,
            }
        }
        None if req.reason == Reason::PreferredAvailable => {
            match scheduler::switch_back_target(pool, policy) {
                Some(c) => c.account.clone(),
                None => {
                    return Ok(Choice::Done(skip(
                        store,
                        req,
                        "no better ranked account is ready",
                    )?));
                }
            }
        }
        None => match scheduler::pick(pool, policy, active.map(|a| a.id)) {
            Some(c) => c.account.clone(),
            None => {
                return Ok(Choice::Done(pool_exhausted(
                    store,
                    req,
                    active,
                    time::now_ms(),
                )?));
            }
        },
    };
    Ok(Choice::Target(target))
}

fn skip(store: &Store, req: &SwapRequest, why: &str) -> Result<SwapOutcome> {
    store.log_event(
        kind::SWAP_SKIPPED,
        req.from,
        req.to,
        Some(req.reason.as_str()),
        Some(json!({ "why": why })),
    )?;
    Ok(SwapOutcome::Skipped {
        why: why.to_string(),
    })
}

fn pool_exhausted(
    store: &Store,
    req: &SwapRequest,
    active: Option<&Account>,
    now: i64,
) -> Result<SwapOutcome> {
    if req.reason == Reason::DeadLogin {
        if let Some(account) = active {
            store.set_state(account.id, AccountState::NeedsLogin)?;
        }
    }
    let recent = store
        .last_event_of(kind::POOL_EXHAUSTED)?
        .is_some_and(|e| now - e.ts < POOL_EXHAUSTED_LOG_INTERVAL_MS);
    if !recent || !req.reason.respects_cooldown() {
        store.log_event(
            kind::POOL_EXHAUSTED,
            active.map(|a| a.id),
            None,
            Some(req.reason.as_str()),
            None,
        )?;
    }
    Ok(SwapOutcome::PoolExhausted)
}

fn log_error(store: &Store, req: &SwapRequest, target: &Account, message: String) -> anyhow::Error {
    let _ = store.log_event(
        kind::ERROR,
        req.from,
        Some(target.id),
        Some(req.reason.as_str()),
        Some(json!({ "message": message })),
    );
    anyhow!(message)
}

/// Keep one encrypted copy of the profile's credentials document per day, for the last
/// `BACKUPS_KEPT` days. `cc-rotate-ctl backup <file>` prints one.
fn backup_daily(store: &Store, paths: &Paths, now_ms: i64) -> Result<()> {
    let Some((_, bytes)) = paths.credentials().read_raw()? else {
        return Ok(());
    };
    let dir = paths.backups_dir();
    std::fs::create_dir_all(&dir)?;
    let day = now_ms / 86_400_000;
    let target = dir.join(format!("credentials-{day}.{BACKUP_EXT}"));
    if target.exists() {
        return Ok(());
    }
    credfile::atomic_write(&target, &store.key()?.seal(&bytes, secret::BACKUP_AAD)?)?;
    let mut backups: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|x| x == BACKUP_EXT || x == LEGACY_BACKUP_EXT)
        })
        .collect();
    // Names sort by day; a legacy file of the same day sorts first and goes first.
    backups.sort();
    while backups.len() > BACKUPS_KEPT {
        let _ = std::fs::remove_file(backups.remove(0));
    }
    Ok(())
}

/// Decrypt a backup written by `backup_daily`.
pub fn open_backup(store: &Store, path: &std::path::Path) -> Result<Vec<u8>> {
    let sealed = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    store.key()?.open(&sealed, secret::BACKUP_AAD)
}

/// Backups an older cc-rotate sealed with Windows DPAPI.
#[cfg(windows)]
pub(crate) fn legacy_backups(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == LEGACY_BACKUP_EXT))
                .collect()
        })
        .unwrap_or_default()
}

/// Re-seal DPAPI backups in the current format. A backup that cannot be converted is kept and
/// ages out like any other.
#[cfg(windows)]
pub(crate) fn convert_legacy_backups(files: &[std::path::PathBuf], key: &secret::Key) {
    for file in files {
        let converted = std::fs::read(file)
            .map_err(anyhow::Error::from)
            .and_then(|sealed| secret::dpapi::unprotect(&sealed))
            .and_then(|plain| key.seal(&plain, secret::BACKUP_AAD))
            .and_then(|sealed| credfile::atomic_write(&file.with_extension(BACKUP_EXT), &sealed));
        if converted.is_ok() {
            let _ = std::fs::remove_file(file);
        }
    }
}
