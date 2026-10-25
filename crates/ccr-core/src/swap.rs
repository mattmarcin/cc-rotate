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
/// The swap that is writing a login into the profile, until it commits (JSON `PendingSwap`).
pub const META_SWAP_PENDING: &str = "swap_pending";

/// A swap between writing the target's login into the profile and committing.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct PendingSwap {
    /// Fingerprint of the login being written.
    pub fp: String,
    pub from: Option<i64>,
    pub to: i64,
    /// When (epoch ms).
    #[serde(default)]
    pub at: i64,
}

pub fn pending_swap(store: &Store) -> Result<Option<PendingSwap>> {
    Ok(store
        .meta(META_SWAP_PENDING)?
        .and_then(|v| serde_json::from_str(&v).ok()))
}

/// Who settles the profile's login, which decides what it may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settler {
    /// A status line tick: does not hold the swap lock, must stay fast, so it only acts when the
    /// profile's login says for certain what happened.
    Tick,
    /// Holds the swap lock (a swap, `cc-rotate-ctl settle-login`, the app): may ask Claude Code
    /// which account the profile's login belongs to.
    Locked,
}

/// What became of settling the profile's login: a swap that died half way, or a login that
/// changed without cc-rotate writing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settle {
    /// Nothing to settle (or a swap is still running).
    Nothing,
    Settled,
    /// The profile's login could not be identified yet: until it is, nothing may be stored from
    /// the profile and no swap may run.
    Unsure,
}

/// Settle a swap that died between writing the target's login into the profile and committing
/// (a crash, a power cut, a failed write that could not be undone). It only acts on certainty:
/// - the profile has exactly the outgoing account's login: the write never happened, and the
///   mark is cleared;
/// - it has exactly the target's: the swap is completed (states only; the outgoing account's
///   login was stored before the write);
/// - it has no usable login (Anthropic rejected whichever it was, or a `/logout`): with the swap
///   lock held, the outgoing account's stored login, which the swap saved before writing, is put
///   back, and the target is held back until a check settles whether its login still works;
/// - anything else (a renewal since, a `/login`): with the swap lock held, Claude Code is asked
///   whose login it is (`usage_check::identify_profile_login`, with a copy it cannot renew).
///
/// `Unsure` otherwise: then nothing is stored from the profile and no swap runs, so no login
/// that cannot be identified is ever overwritten or filed under an account; the sessions go on
/// with whatever login the profile has.
pub fn finish_dead_swap(store: &Store, paths: &Paths, settler: Settler) -> Result<Settle> {
    let Some(raw) = store.meta(META_SWAP_PENDING)? else {
        return Ok(Settle::Nothing);
    };
    let Ok(pending) = serde_json::from_str::<PendingSwap>(&raw) else {
        store.delete_meta(META_SWAP_PENDING)?;
        return Ok(Settle::Settled);
    };
    if settler == Settler::Tick
        && crate::lock::is_held(&paths.swap_lock(), crate::lock::CLAUDE_STALE)
    {
        return Ok(Settle::Nothing);
    }
    let creds = paths.credentials();
    // A Keychain that cannot be read now may hold the live login while the file is stale.
    if !creds.complete().unwrap_or(false) {
        return Ok(Settle::Unsure);
    }
    let Ok(oauth) = creds.read_oauth() else {
        return Ok(Settle::Unsure);
    };
    let file_fp = oauth.as_ref().and_then(credfile::oauth_fingerprint);
    let outgoing_fp = match pending.from {
        Some(id) => store.credential_meta(id)?.map(|m| m.refresh_fp),
        None => None,
    };
    enum Verdict {
        NeverWritten,
        Written,
        Foreign,
        NoLogin,
    }
    let verdict = if file_fp.is_some() && file_fp == outgoing_fp {
        Verdict::NeverWritten
    } else if file_fp.as_deref() == Some(pending.fp.as_str()) {
        Verdict::Written
    } else if settler == Settler::Tick {
        return Ok(Settle::Unsure);
    } else if oauth.as_ref().is_none_or(credfile::is_dead) {
        Verdict::NoLogin
    } else {
        let account = |id: Option<i64>| -> Result<Option<Account>> {
            Ok(match id {
                Some(id) => store.account(id)?,
                None => None,
            })
        };
        let (to, from) = (account(Some(pending.to))?, account(pending.from)?);
        let is = |who: &crate::usage_check::Who, a: &Option<Account>| -> Option<bool> {
            who.is(a.as_ref()?)
        };
        match crate::usage_check::identify_profile_login(store, paths)? {
            Some(who) => match (is(&who, &to), is(&who, &from)) {
                (Some(true), _) => Verdict::Written,
                (_, Some(true)) => Verdict::NeverWritten,
                // Only when neither account could be one whose identity is unknown.
                (Some(false), Some(false)) | (Some(false), None) if pending.from.is_none() => {
                    Verdict::Foreign
                }
                (Some(false), Some(false)) => Verdict::Foreign,
                _ => return unsure(store, &pending, &raw),
            },
            None => return unsure(store, &pending, &raw),
        }
    };
    // Putting the outgoing login back writes the profile, under Claude Code's locks.
    let restored = match (&verdict, pending.from) {
        (Verdict::NoLogin, Some(from)) => match store.credential(from)? {
            Some(login) => {
                write_profile_login(store, paths, &login.oauth)?;
                true
            }
            None => false,
        },
        _ => false,
    };
    store.immediate(|s| {
        // Only the swap read above: a newer one may have replaced the mark meanwhile.
        if s.meta(META_SWAP_PENDING)?.as_deref() != Some(raw.as_str()) {
            return Ok(());
        }
        match verdict {
            Verdict::NeverWritten => {}
            Verdict::Written => {
                let now = time::now_ms();
                if let Some(from) = pending.from {
                    if s.account(from)?
                        .is_some_and(|a| a.state == AccountState::Active)
                    {
                        s.set_state(from, AccountState::Parked)?;
                    }
                }
                if s.account(pending.to)?.is_some() {
                    // Only the states: the login in the profile is stored by the write-back that
                    // follows, like any renewal of the active account's.
                    s.set_state(pending.to, AccountState::Active)?;
                    s.set_meta(META_LAST_SWAP_AT, now)?;
                    s.set_meta(META_ACTIVE_SINCE, now)?;
                    s.log_event(
                        kind::SWAP,
                        pending.from,
                        Some(pending.to),
                        Some("completed"),
                        Some(json!({ "why": "finished a swap that was interrupted after writing the login" })),
                    )?;
                }
            }
            Verdict::NoLogin => {
                // Whichever login was rejected: the target's, if the write happened, is checked
                // with a renewal before it is used again; the outgoing one, if it is the dead
                // one, fails in the profile and is handled as any dead login.
                if s.account(pending.to)?.is_some() {
                    crate::usage_check::hold_back(s, pending.to)?;
                }
                if !restored {
                    if let Some(from) = pending.from {
                        if s.account(from)?.is_some() {
                            crate::usage_check::hold_back(s, from)?;
                        }
                    }
                }
                let label = |id: i64| -> Result<Option<String>> {
                    Ok(s.account(id)?.map(|a| a.label))
                };
                let to = label(pending.to)?.unwrap_or_else(|| "The other account".into());
                let from = match pending.from {
                    Some(id) => label(id)?,
                    None => None,
                };
                let what = "A swap was interrupted and the profile's login stopped working.";
                let message = match (restored, from) {
                    (true, Some(from)) => format!(
                        "{what} cc-rotate put {from}'s login back. {to} is on hold until a usage check confirms its login."
                    ),
                    (false, Some(from)) => format!(
                        "{what} {from} and {to} are on hold until usage checks confirm their logins."
                    ),
                    (_, None) => format!(
                        "{what} {to} is on hold until a usage check confirms its login."
                    ),
                };
                s.log_event(
                    kind::ERROR,
                    pending.from,
                    Some(pending.to),
                    Some("swap"),
                    Some(json!({ "message": message })),
                )?;
            }
            Verdict::Foreign => {
                s.set_meta(META_PROFILE_FOREIGN, "1")?;
                s.log_event(
                    kind::FOREIGN_LOGIN,
                    pending.from,
                    None,
                    Some("login_in_profile"),
                    Some(json!({ "why": "after an interrupted swap, the profile holds the login of an account that isn't in cc-rotate" })),
                )?;
            }
        }
        s.delete_meta(META_SWAP_PENDING)
    })?;
    Ok(Settle::Settled)
}

/// Still unsure: tell the user once it has lasted a while, with what to do.
fn unsure(store: &Store, pending: &PendingSwap, raw: &str) -> Result<Settle> {
    const REPORT_AFTER_MS: i64 = 10 * 60_000;
    const META_REPORTED: &str = "dead_swap_reported";
    if time::now_ms() - pending.at > REPORT_AFTER_MS
        && store.meta(META_REPORTED)?.as_deref() != Some(raw)
    {
        store.set_meta(META_REPORTED, raw)?;
        store.log_event(
            kind::ERROR,
            pending.from,
            Some(pending.to),
            Some("swap"),
            Some(json!({ "message": "A swap was interrupted and cc-rotate can't tell yet which account the profile is signed in to. Until it can, swaps are on hold and nothing is saved from the profile. It retries every minute, which needs the `claude` command and a network connection. Your sessions keep working." })),
        )?;
    }
    Ok(Settle::Unsure)
}

/// A login in the profile that could not be identified yet (JSON `Unidentified`).
pub const META_LOGIN_UNIDENTIFIED: &str = "login_unidentified";
/// Set by the write-back while a changed login waits to be identified (its change marker), so
/// the app's frequent `statusline::needs_settling` reads the store, not the credentials.
pub(crate) const META_LOGIN_TO_IDENTIFY: &str = "login_to_identify";
/// How long a changed login may stay unidentified (no swap runs meanwhile) before it is stored
/// as the active account's without Claude Code's word, as before logins were identified.
pub const UNIDENTIFIED_FOR_AT_MOST_MS: i64 = 10 * 60_000;

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct Unidentified {
    /// The credentials' change marker of the login.
    pub marker: String,
    /// When it first could not be identified (epoch ms).
    pub at: i64,
}

/// A login in the profile that cc-rotate did not write and has not identified.
pub(crate) struct ChangedLogin {
    active: Account,
    oauth: serde_json::Value,
    fp: String,
    marker: String,
}

/// The login in the profile when it is neither one stored for the active account nor
/// identified yet: a renewal of the active account's login, or a `/login` in the profile, which
/// `.claude.json` cannot tell apart after a swap (F14). `None` when there is nothing to identify,
/// or logins are not identified (`usage_check::identifies_logins`).
pub(crate) fn changed_login(store: &Store, paths: &Paths) -> Result<Option<ChangedLogin>> {
    if pending_swap(store)?.is_some() || store.meta(META_PROFILE_FOREIGN)?.as_deref() == Some("1") {
        return Ok(None);
    }
    let Some(active) = store.active_account()? else {
        return Ok(None);
    };
    let creds = paths.credentials();
    let Some(marker) = creds.change_marker()? else {
        return Ok(None);
    };
    if store.meta(META_CRED_SEEN)?.as_deref() == Some(marker.as_str())
        || !usage_check::identifies_logins(store, paths)?
    {
        return Ok(None);
    }
    let Some(oauth) = creds
        .read_oauth()
        .ok()
        .flatten()
        .filter(|o| !credfile::is_dead(o))
    else {
        return Ok(None);
    };
    let Some(fp) = credfile::oauth_fingerprint(&oauth) else {
        return Ok(None);
    };
    if store.credential_meta(active.id)?.map(|m| m.refresh_fp) == Some(fp.clone()) {
        return Ok(None);
    }
    Ok(Some(ChangedLogin {
        active,
        oauth,
        fp,
        marker,
    }))
}

/// Identify a login that changed in the profile without cc-rotate writing it (`changed_login`),
/// with the swap lock held, by asking Claude Code whose it is with a copy it cannot renew:
/// - the active account's (a renewal) is stored as its login;
/// - another pool account's (a `/login` in the profile) makes that account active;
/// - any other account's (a `/login` with an account outside the pool) marks the profile
///   foreign, so its login is never stored.
///
/// `Unsure` while Claude Code cannot tell: the login is not stored and no swap runs, for at most
/// `UNIDENTIFIED_FOR_AT_MOST_MS`. After that it is stored as the active account's, as it was
/// before logins were identified, so a Claude Code that cannot identify logins never stops the
/// rotation.
pub fn identify_changed_login(store: &Store, paths: &Paths) -> Result<Settle> {
    let Some(changed) = changed_login(store, paths)? else {
        store.delete_meta(META_LOGIN_UNIDENTIFIED)?;
        store.delete_meta(META_LOGIN_TO_IDENTIFY)?;
        return Ok(Settle::Nothing);
    };
    let who = usage_check::identify_profile_login(store, paths)?;
    // Claude Code may have renewed the login meanwhile: act only on the one identified.
    let now_fp = paths
        .credentials()
        .read_oauth()
        .ok()
        .flatten()
        .as_ref()
        .and_then(credfile::oauth_fingerprint);
    if now_fp.as_deref() != Some(changed.fp.as_str()) {
        return Ok(Settle::Unsure);
    }
    enum Verdict {
        Own,
        Adopt(Box<Account>),
        Foreign(Option<String>),
    }
    let verdict = match who {
        Some(who) => match who.is(&changed.active) {
            Some(true) => Verdict::Own,
            active_is => match store
                .accounts()?
                .into_iter()
                .find(|a| a.id != changed.active.id && who.is(a) == Some(true))
            {
                Some(account) => Verdict::Adopt(Box::new(account)),
                // Nothing to compare the active account with: stored as before.
                None if active_is.is_none() => Verdict::Own,
                None => Verdict::Foreign(who.uuid),
            },
        },
        None => {
            if !unidentified_for_long(store, &changed)? {
                return Ok(Settle::Unsure);
            }
            Verdict::Own
        }
    };
    let active = &changed.active;
    store.immediate(|s| {
        // Only the login identified: the write-back, a swap or a `/login` may have dealt with
        // it meanwhile.
        if s.meta(META_CRED_SEEN)?.as_deref() == Some(changed.marker.as_str())
            || s.active_account()?.map(|a| a.id) != Some(active.id)
        {
            return Ok(());
        }
        match &verdict {
            Verdict::Own => {
                s.save_credential(active.id, &changed.oauth, "sync")?;
                s.log_event(kind::SYNC, Some(active.id), None, Some("refreshed"), None)?;
            }
            Verdict::Adopt(account) => {
                adopt_login(s, Some(active), account, Some(&changed.oauth))?;
            }
            Verdict::Foreign(uuid) => {
                s.set_meta(META_PROFILE_FOREIGN, "1")?;
                s.log_event(
                    kind::FOREIGN_LOGIN,
                    Some(active.id),
                    None,
                    Some("login_in_profile"),
                    Some(json!({ "account_uuid": uuid })),
                )?;
            }
        }
        s.set_meta(META_CRED_SEEN, &changed.marker)?;
        s.delete_meta(META_LOGIN_TO_IDENTIFY)?;
        s.delete_meta(META_LOGIN_UNIDENTIFIED)
    })?;
    Ok(Settle::Settled)
}

/// Whether the changed login has gone unidentified for `UNIDENTIFIED_FOR_AT_MOST_MS`; if so, the
/// user is told it is stored without Claude Code's word.
fn unidentified_for_long(store: &Store, changed: &ChangedLogin) -> Result<bool> {
    let now = time::now_ms();
    let since = match store
        .meta(META_LOGIN_UNIDENTIFIED)?
        .and_then(|v| serde_json::from_str::<Unidentified>(&v).ok())
    {
        Some(u) if u.marker == changed.marker => u.at,
        _ => {
            let first = Unidentified {
                marker: changed.marker.clone(),
                at: now,
            };
            store.set_meta(META_LOGIN_UNIDENTIFIED, serde_json::to_string(&first)?)?;
            now
        }
    };
    if now - since < UNIDENTIFIED_FOR_AT_MOST_MS {
        return Ok(false);
    }
    store.log_event(
        kind::ERROR,
        Some(changed.active.id),
        None,
        Some("login"),
        Some(json!({ "message": format!("The profile's login changed and Claude Code couldn't confirm its account for 10 minutes, so cc-rotate saved it as {}'s. Confirming needs the `claude` command and a network connection.", changed.active.label) })),
    )?;
    Ok(true)
}

/// Make `account` active because the profile holds its login (a `/login` in the profile), and
/// store that login as its own. Call inside a transaction.
pub(crate) fn adopt_login(
    s: &Store,
    active: Option<&Account>,
    account: &Account,
    oauth: Option<&serde_json::Value>,
) -> Result<()> {
    if let Some(a) = active {
        s.set_state(a.id, AccountState::Parked)?;
    }
    s.set_state(account.id, AccountState::Active)?;
    s.set_meta(META_ACTIVE_SINCE, time::now_ms())?;
    if let Some(o) = oauth.filter(|o| !credfile::is_dead(o)) {
        s.save_credential(account.id, o, "sync")?;
    }
    s.log_event(
        kind::ADOPTED_LOGIN,
        active.map(|a| a.id),
        Some(account.id),
        Some("login_in_profile"),
        None,
    )?;
    Ok(())
}

/// Settle what the profile's login is, with the swap lock held: a swap that stopped half way
/// (`finish_dead_swap`), then a login that changed without cc-rotate writing it
/// (`identify_changed_login`).
pub fn settle_profile(store: &Store, paths: &Paths) -> Result<Settle> {
    let dead = finish_dead_swap(store, paths, Settler::Locked)?;
    if dead == Settle::Unsure {
        return Ok(Settle::Unsure);
    }
    Ok(match (dead, identify_changed_login(store, paths)?) {
        (_, Settle::Unsure) => Settle::Unsure,
        (Settle::Settled, _) | (_, Settle::Settled) => Settle::Settled,
        _ => Settle::Nothing,
    })
}

/// Write a pool account's login into the profile under Claude Code's refresh and credentials
/// locks (the caller holds the swap lock). The profile is no longer signed in to an account
/// outside the pool, so Claude Code's renewals of this login are stored again, as after a swap.
fn write_profile_login(store: &Store, paths: &Paths, oauth: &serde_json::Value) -> Result<()> {
    let timeout = Duration::from_millis(store.settings()?.lock_timeout_ms);
    let opts = LockOptions::claude(timeout);
    let _refresh = DirLock::acquire(&paths.refresh_lock(), opts)?;
    let _legacy = DirLock::acquire(&paths.legacy_refresh_lock(), opts)?;
    let _storage = DirLock::acquire(&paths.storage_lock(), LockOptions::claude_storage(timeout))?;
    let creds = paths.credentials();
    creds.write_oauth(oauth)?;
    store.set_meta(META_PROFILE_FOREIGN, "0")?;
    if let Some(marker) = creds.change_marker()? {
        store.set_meta(META_CRED_SEEN, marker)?;
    }
    Ok(())
}

/// Put the outgoing login back after a failed write or commit, and say whether the profile is
/// verifiably back to it. Only when the profile still has this swap's target (or nothing it
/// could have changed): another swap may have taken over a lock this one held too long.
fn restore_outgoing(
    creds: &crate::credstore::Credentials,
    previous: Option<&serde_json::Value>,
    target_fp: &str,
) -> bool {
    let now = creds.read_oauth().ok().flatten();
    let now_fp = now.as_ref().and_then(credfile::oauth_fingerprint);
    let previous_fp = previous.and_then(credfile::oauth_fingerprint);
    if now_fp == previous_fp {
        return true;
    }
    if now_fp.as_deref() != Some(target_fp) {
        return false;
    }
    let Some(previous) = previous else {
        return false;
    };
    creds.write_oauth(previous).is_ok()
        && creds
            .read_oauth()
            .ok()
            .flatten()
            .as_ref()
            .and_then(credfile::oauth_fingerprint)
            == previous_fp
}

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
/// modification time the marker once was.
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
        let windows = scheduler::window_usage(
            sample.as_ref(),
            now_s,
            policy.thresholds.for_account(&account),
            model.as_deref(),
        );
        let detected = detect_weekly_limit(store, account.id, now_s)?;
        pool.push(Candidate {
            weekly_limit: scheduler::weekly_limit(account.weekly_limit, detected),
            weekly_detected: detected,
            windows,
            order,
            held_back: usage_check::state(store, account.id)?.held_back,
            expiring: None,
            account,
        });
    }
    if policy.use_expiring_weekly {
        // The cached rates only: the status line must stay fast (`budget::refresh_rates`).
        let rates = crate::budget::cached_rates(store)?;
        let promoted = crate::budget::promoted(store, &pool)?;
        let expiring =
            crate::budget::expiring(&pool, &rates, &promoted, policy.thresholds.session, now_s);
        for c in &mut pool {
            c.expiring = expiring.get(&c.account.id).copied();
        }
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
    let others: Vec<i64> = others_latest(store, active_id)?
        .iter()
        .flat_map(|s| &s.windows)
        .filter_map(|w| w.resets_at)
        .collect();
    // Unsure when the reset time is not known to be the account's own, or is another account's
    // too (aligned reset times).
    Ok(active.windows.iter().filter(|w| w.blocking()).any(|w| {
        w.resets_at.is_none_or(|r| {
            !own.iter().any(|&k| same_reset(k, r)) || others.iter().any(|&o| same_reset(o, r))
        })
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
    let thresholds = match store.account(account_id)? {
        Some(account) => policy.thresholds.for_account(&account),
        None => policy.thresholds,
    };
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
            scheduler::window_usage(Some(s), now_ms / 1000, thresholds, model.as_deref())
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
        Err(LockError::Busy(_)) => return skip(store, req, "another swap is running"),
        Err(e) => return Err(e.into()),
    };
    // An earlier swap that stopped half way. While the profile's login is unidentified, no swap
    // runs: it would overwrite the only copy of that login, or file it under the wrong account.
    if finish_dead_swap(store, paths, Settler::Locked)? == Settle::Unsure {
        return skip(
            store,
            req,
            "an earlier swap was interrupted and the profile's login isn't identified yet",
        );
    }
    // A login that changed in the profile is identified first, so the swap below never stores
    // another account's login as the outgoing account's.
    if identify_changed_login(store, paths)? == Settle::Unsure {
        return skip(
            store,
            req,
            "the profile's login changed and isn't identified yet",
        );
    }

    let now = time::now_ms();
    let active = store.active_account()?;
    let active_id = active.as_ref().map(|a| a.id);

    if let Some(from) = req.from {
        if active_id != Some(from) {
            return skip(store, req, "the account in use already changed");
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
                    return skip(store, req, "too soon after the last swap");
                }
            }
        }
    }
    if req.reason == Reason::PreferredAvailable
        && now - active_since(store)? < settings.switch_back_dwell_s * 1000
    {
        return skip(store, req, "too soon to swap back");
    }

    // What the user's other profiles read since, so an account an agent used up there is not
    // picked. Best effort: an unreadable profile must not stop the swap.
    let _ = crate::profile_usage::import(store, paths, now);

    // A threshold swap that rests on a reading the status line could not attribute for sure
    // (section 8) first reads the account in use's own usage (section 20.4). When that is not
    // possible, the swap goes ahead as the reading says.
    // Checks and the confirmation share one time budget: sessions may be waiting at a limit.
    let check_deadline = Instant::now() + PRE_SWAP_CHECK_BUDGET;
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
                            "a fresh check shows the account in use is below its limits",
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
    // Relearn the rates that decide which budgets would expire, off the status line (best effort:
    // the cached ones serve otherwise).
    let _ = crate::budget::refresh_rates(store, time::now_ms());
    let mut checked: Vec<serde_json::Value> = Vec::new();
    let mut checked_ids: Vec<i64> = Vec::new();
    let mut passed_over: Vec<i64> = Vec::new();
    let mut attempts = 0;
    let (target, _target_lock, pool, policy) = loop {
        attempts += 1;
        let last_try = req.to.is_some() || attempts >= MAX_PICK_ATTEMPTS;
        let (mut pool, policy) = load_pool(store, time::now_s())?;
        let some_ready = scheduler::pick(&pool, policy, active_id).is_some();
        pool.retain(|c| !passed_over.contains(&c.account.id));
        if req.to.is_none() && some_ready && scheduler::pick(&pool, policy, active_id).is_none() {
            // The accounts that could take over are only busy (a check of them running): not an
            // exhausted pool, so no such event or notification.
            return skip(
                store,
                req,
                "the accounts that could take over are being checked",
            );
        }
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
                    format!("couldn't lock {}'s login: {e}", target.label),
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
                format!("{} can't be used now: {why}", target.label),
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
                        "{} is on hold until a usage check confirms its login. Check its usage first.",
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
        .ok_or_else(|| anyhow!("{} has no saved login", target.label))?;

    let claude_lock_opts = LockOptions::claude(lock_timeout);
    let refresh_lock = DirLock::acquire(&paths.refresh_lock(), claude_lock_opts).map_err(|e| {
        log_error(
            store,
            req,
            &target,
            format!("couldn't take Claude Code's refresh lock: {e}"),
        )
    })?;
    let legacy_lock =
        DirLock::acquire(&paths.legacy_refresh_lock(), claude_lock_opts).map_err(|e| {
            log_error(
                store,
                req,
                &target,
                format!("couldn't take Claude Code's legacy refresh lock: {e}"),
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
            format!("couldn't take Claude Code's credentials write lock: {e}"),
        )
    })?;

    let creds = paths.credentials();
    let previous = creds.read_oauth()?;
    let mut outgoing_state = AccountState::Parked;
    // A login of an account outside the pool (a `/login` in the profile) is never stored as the
    // active account's: it keeps the login it has stored.
    let foreign = store.meta(META_PROFILE_FOREIGN)?.as_deref() == Some("1");
    // The login identified above may have changed since, while this swap checked accounts (a
    // renewal, or a `/login`): identified first, by the next swap request. Only a change nobody
    // has seen: one already dealt with must never hold every swap back.
    if let (Some(account), Some(oauth)) = (&active, &previous) {
        let fp = credfile::oauth_fingerprint(oauth);
        if !foreign
            && !credfile::is_dead(oauth)
            && fp.is_some()
            && fp != store.credential_meta(account.id)?.map(|m| m.refresh_fp)
            && creds.change_marker()? != store.meta(META_CRED_SEEN)?
            && usage_check::identifies_logins(store, paths)?
        {
            return skip(
                store,
                req,
                "the profile's login changed during the swap and needs identifying first",
            );
        }
    }
    if let Some(account) = active.as_ref().filter(|_| !foreign) {
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
    // Everything that can fail is read before the write; after it, only the commit can, and its
    // failure restores the outgoing login.
    let from_sample = match active_id {
        Some(id) => store.last_sample(id)?,
        None => None,
    };
    let previous_swap = store.meta_i64(META_LAST_SWAP_AT)?;
    let pending = PendingSwap {
        fp: target_login.refresh_fp.clone(),
        from: active_id,
        to: target.id,
        at: now,
    };
    store.set_meta(META_SWAP_PENDING, serde_json::to_string(&pending)?)?;
    if let Err(e) = creds.write_oauth(&target_login.oauth) {
        // The write may have landed anyway (a Keychain read-back that failed): only a verified
        // restore clears the mark; otherwise `finish_dead_swap` settles it.
        if restore_outgoing(&creds, previous.as_ref(), &target_login.refresh_fp) {
            let _ = store.delete_meta(META_SWAP_PENDING);
        }
        return Err(e);
    }
    let marker = creds.change_marker().ok().flatten();

    // Taken for its expiring budget: it stays ahead after its 5-hour limit this week.
    let promoted_until = pool
        .iter()
        .find(|c| c.account.id == target.id)
        .and_then(|c| policy.expiring(c))
        .map(|e| e.resets_at);
    let committed = store.immediate(|s| {
        if let Some(account) = &active {
            s.set_state(account.id, outgoing_state)?;
        }
        s.set_state(target.id, AccountState::Active)?;
        if let Some(reset) = promoted_until {
            s.set_meta(&crate::budget::promoted_key(target.id), reset)?;
        }
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
        s.delete_meta(META_SWAP_PENDING)?;
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
        // Put the outgoing login back so the profile and the store agree again; keep the mark
        // unless that is verified, so `finish_dead_swap` settles it.
        let restored = restore_outgoing(&creds, previous.as_ref(), &target_login.refresh_fp);
        if restored {
            let _ = store.delete_meta(META_SWAP_PENDING);
        }
        drop(storage_lock);
        drop(legacy_lock);
        drop(refresh_lock);
        return Err(e.context(if restored {
            "couldn't record the swap, so the previous login was put back"
        } else {
            "couldn't record the swap"
        }));
    }

    drop(storage_lock);
    drop(legacy_lock);
    drop(refresh_lock);
    Ok(SwapOutcome::Swapped {
        from: active_id,
        to: target.id,
    })
}

/// What became of a fresh login of the active account (`install_active_login`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveLogin {
    /// Written into the profile.
    Written,
    /// Not written: a swap that stopped half way leaves the profile's login unidentified, and
    /// the profile may hold the only copy of another account's login.
    Unidentified,
    /// Not written: settling a swap that stopped half way made another account active, so the
    /// profile's login is that account's.
    NoLongerActive,
}

/// Write a fresh login for the account that is already active (a re-login of the active
/// account), under Claude Code's refresh locks like any swap. The fresh login is stored for the
/// account before this is called, so when it is not written, nothing is lost.
pub fn install_active_login(
    store: &Store,
    paths: &Paths,
    account_id: i64,
    oauth: &serde_json::Value,
) -> Result<ActiveLogin> {
    let timeout = Duration::from_millis(store.settings()?.lock_timeout_ms);
    let _swap_lock = DirLock::acquire(&paths.swap_lock(), LockOptions::claude(timeout))?;
    if finish_dead_swap(store, paths, Settler::Locked)? == Settle::Unsure {
        return Ok(ActiveLogin::Unidentified);
    }
    if store.active_account()?.map(|a| a.id) != Some(account_id) {
        return Ok(ActiveLogin::NoLongerActive);
    }
    // Stored again under the swap lock: an identification of the profile's previous login may
    // have stored that one over it meanwhile, and the profile and the store must agree.
    store.save_credential(account_id, oauth, "login")?;
    write_profile_login(store, paths, oauth)?;
    Ok(ActiveLogin::Written)
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
                        "that account is already in use",
                    )?));
                }
                AccountState::NeedsLogin => {
                    return Err(anyhow!(
                        "{} is signed out. Sign in to it again first.",
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
                        "no account higher in your order is ready",
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
