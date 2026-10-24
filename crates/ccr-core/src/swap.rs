//! The swap protocol (spec section 6).
//!
//! Ordering is what makes this safe against Claude Code's token refresh:
//! 1. cc-rotate's own swap lock serializes swaps across processes.
//! 2. Claude Code's two refresh locks are held while the outgoing login is captured and the
//!    target login is written, so no refresh can be in flight. A refresh that finished before
//!    we took the locks is in the file and gets captured; one that starts after we release them
//!    re-reads the file under the lock and refreshes the target.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::json;

use crate::lock::{DirLock, LockError, LockOptions};
use crate::paths::Paths;
use crate::scheduler::{self, Candidate, Reason, Thresholds, Usage};
use crate::store::{Account, AccountState, Store, kind};
use crate::{credfile, secret, time};

pub const META_LAST_SWAP_AT: &str = "last_swap_at";
pub const META_LAST_SWAP_FROM_FIVE_RESETS: &str = "last_swap_from_five_resets";
pub const META_LAST_SWAP_FROM_WEEK_RESETS: &str = "last_swap_from_week_resets";
pub const META_CRED_MTIME_SEEN: &str = "cred_mtime_seen";
/// Set when the profile is signed in to an account outside the pool; a swap clears it.
pub const META_PROFILE_FOREIGN: &str = "profile_foreign";

const POOL_EXHAUSTED_LOG_INTERVAL_MS: i64 = 10 * 60_000;
const BACKUPS_KEPT: usize = 7;

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

/// Every account with its effective usage at `now_s`.
pub fn usage_by_account(store: &Store, now_s: i64) -> Result<Vec<(Account, Usage)>> {
    store
        .accounts()?
        .into_iter()
        .map(|a| {
            let usage = Usage::from_sample(store.last_sample(a.id)?.as_ref(), now_s);
            Ok((a, usage))
        })
        .collect()
}

/// The account the scheduler would pick right now, excluding `exclude`.
pub fn next_target(store: &Store, exclude: Option<i64>) -> Result<Option<Account>> {
    let thresholds = Thresholds::from(&store.settings()?);
    let pool = usage_by_account(store, time::now_s())?;
    let cands: Vec<Candidate> = pool
        .iter()
        .map(|(a, u)| Candidate {
            account: a,
            usage: *u,
        })
        .collect();
    Ok(scheduler::pick(&cands, thresholds, exclude).cloned())
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
    if req.reason.respects_cooldown() {
        if let Some(last) = store.meta_i64(META_LAST_SWAP_AT)? {
            let cooldown_ms = settings.swap_cooldown_s * 1000;
            if now - last < cooldown_ms {
                return skip(store, req, "within the swap cooldown");
            }
        }
    }

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
        None => match next_target(store, active_id)? {
            Some(account) => account,
            None => return pool_exhausted(store, req, active.as_ref(), now),
        },
    };

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

    let cred_path = paths.credentials_file();
    let previous = credfile::read_oauth(&cred_path)?;
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

    backup_daily(paths, now).context("backing up .credentials.json")?;
    credfile::write_oauth(&cred_path, &target_login.oauth)?;

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
        let five = from_sample
            .as_ref()
            .and_then(|x| x.five_resets_at)
            .unwrap_or(0);
        let week = from_sample
            .as_ref()
            .and_then(|x| x.week_resets_at)
            .unwrap_or(0);
        s.set_meta(META_LAST_SWAP_FROM_FIVE_RESETS, five)?;
        s.set_meta(META_LAST_SWAP_FROM_WEEK_RESETS, week)?;
        if let Some(mtime) = credfile::mtime_ms(&cred_path) {
            s.set_meta(META_CRED_MTIME_SEEN, mtime)?;
        }
        s.log_event(
            kind::SWAP,
            active_id,
            Some(target.id),
            Some(req.reason.as_str()),
            Some(json!({
                "from_label": active.as_ref().map(|a| a.label.clone()),
                "to_label": target.label,
                "from_state": outgoing_state.as_str(),
                "from_five_pct": from_sample.as_ref().and_then(|x| x.five_pct),
                "from_week_pct": from_sample.as_ref().and_then(|x| x.week_pct),
            })),
        )?;
        Ok(())
    });

    if let Err(e) = committed {
        // Put the outgoing login back so the file and the store agree again.
        if let Some(oauth) = &previous {
            let _ = credfile::write_oauth(&cred_path, oauth);
        }
        drop(legacy_lock);
        drop(refresh_lock);
        return Err(e.context("recording the swap failed; the previous login was restored"));
    }

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
    let opts = LockOptions::claude(Duration::from_millis(store.settings()?.lock_timeout_ms));
    let _swap_lock = DirLock::acquire(&paths.swap_lock(), opts)?;
    let _refresh = DirLock::acquire(&paths.refresh_lock(), opts)?;
    let _legacy = DirLock::acquire(&paths.legacy_refresh_lock(), opts)?;
    let cred_path = paths.credentials_file();
    credfile::write_oauth(&cred_path, oauth)?;
    if let Some(mtime) = credfile::mtime_ms(&cred_path) {
        store.set_meta(META_CRED_MTIME_SEEN, mtime)?;
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

/// Keep one encrypted copy of `.credentials.json` per day, for the last `BACKUPS_KEPT` days.
fn backup_daily(paths: &Paths, now_ms: i64) -> Result<()> {
    let source = paths.credentials_file();
    let Ok(bytes) = std::fs::read(&source) else {
        return Ok(());
    };
    let dir = paths.backups_dir();
    std::fs::create_dir_all(&dir)?;
    let day = now_ms / 86_400_000;
    let target = dir.join(format!("credentials-{day}.dpapi"));
    if target.exists() {
        return Ok(());
    }
    credfile::atomic_write(&target, &secret::protect(&bytes)?)?;
    let mut backups: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "dpapi"))
        .collect();
    backups.sort();
    while backups.len() > BACKUPS_KEPT {
        let _ = std::fs::remove_file(backups.remove(0));
    }
    Ok(())
}
