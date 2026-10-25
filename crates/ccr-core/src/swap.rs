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

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::json;

use crate::lock::{DirLock, LockError, LockOptions};
use crate::paths::Paths;
use crate::scheduler::{self, Candidate, Policy, Reason, WeeklyEvidence, WeeklyLimit, WindowKind};
use crate::store::{Account, AccountState, Store, kind};
use crate::{credfile, secret, time};

pub const META_LAST_SWAP_AT: &str = "last_swap_at";
/// The reset times (comma-separated epoch seconds) of every window in the outgoing account's
/// last reading, for the stale-reading guard after a swap.
pub const META_LAST_SWAP_FROM_RESETS: &str = "last_swap_from_resets";
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

/// Whether the active account was already over a threshold when it became active: it has
/// readings since then, and every one of them has a window that applies at or above its
/// threshold. Its last reading before the swap was out of date, for example because another
/// profile used it meanwhile. `scheduler::Timing::leaves_at_once` lets such an account go
/// without waiting out the swap cooldown.
pub fn arrived_over_limit(store: &Store, account_id: i64, now_ms: i64) -> Result<bool> {
    let policy = Policy::from(&store.settings()?);
    let model = store.meta(META_PROFILE_MODEL)?;
    let samples = store.account_samples_since(account_id, active_since(store)?)?;
    Ok(!samples.is_empty()
        && samples.iter().all(|s| {
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

    let now_s = now / 1000;
    let (pool, policy) = load_pool(store, now_s)?;
    let target = match req.to {
        Some(id) => {
            let account = store
                .account(id)?
                .ok_or_else(|| anyhow!("account {id} does not exist"))?;
            match account.state {
                AccountState::Active => return skip(store, req, "that account is already active"),
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
            match scheduler::switch_back_target(&pool, policy) {
                Some(c) => c.account.clone(),
                None => return skip(store, req, "no better ranked account is ready"),
            }
        }
        None => match scheduler::pick(&pool, policy, active_id) {
            Some(c) => c.account.clone(),
            None => return pool_exhausted(store, req, active.as_ref(), now),
        },
    };
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
    creds.write_oauth(&target_login.oauth)?;
    let marker = creds.change_marker()?;

    let from_sample = match active_id {
        Some(id) => store.last_sample(id)?,
        None => None,
    };
    let committed = store.immediate(|s| {
        if let Some(account) = &active {
            s.set_state(account.id, outgoing_state)?;
        }
        s.set_state(target.id, AccountState::Active)?;
        s.set_meta(META_LAST_SWAP_AT, now)?;
        s.set_meta(META_PROFILE_FOREIGN, "0")?;
        s.set_meta(META_ACTIVE_SINCE, now)?;
        let resets: Vec<String> = from_sample
            .iter()
            .flat_map(|x| &x.windows)
            .filter_map(|w| w.resets_at)
            .map(|r| r.to_string())
            .collect();
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
