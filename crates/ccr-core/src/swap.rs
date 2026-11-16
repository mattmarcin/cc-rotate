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
use crate::{credfile, relay, secret, time, usage_check};

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
    /// A change of relay mode (`move_profile_login`), not a swap: committing it sets the mode
    /// instead of which account is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayChange>,
}

/// A change of relay mode that moves the profile's login from one account to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum RelayChange {
    /// Relay mode on, with `home` as the home account.
    On {
        home: i64,
    },
    Off,
}

impl RelayChange {
    /// The home account once the change is made: `None` for off.
    pub fn home(self) -> Option<i64> {
        match self {
            Self::On { home } => Some(home),
            Self::Off => None,
        }
    }

    /// The word events record.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On { .. } => "on",
            Self::Off => "off",
        }
    }
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
///   login was stored before the write); for a change of relay mode (`PendingSwap::relay`), the
///   mode is set and the states are left alone;
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
            Verdict::Written if pending.relay.is_some() => {
                // A change of relay mode: which account is active never changed.
                if let Some(change) = pending.relay {
                    relay::set_mode(s, change.home())?;
                    s.log_event(
                        kind::RELAY_MODE,
                        pending.from,
                        Some(pending.to),
                        Some(change.as_str()),
                        Some(json!({
                            "home": change.home(),
                            "why": "finished a change of the app account that was interrupted after writing the login",
                        })),
                    )?;
                }
            }
            Verdict::Written => {
                let now = time::now_ms();
                if let Some(from) = pending.from
                    && s.account(from)?
                        .is_some_and(|a| a.state == AccountState::Active)
                {
                    s.set_state(from, AccountState::Parked)?;
                }
                if s.account(pending.to)?.is_some() {
                    // Only the states: the login in the profile is stored by the write-back that
                    // follows, like any renewal of the active account's.
                    s.set_state(pending.to, AccountState::Active)?;
                    s.set_meta(META_LAST_SWAP_AT, now)?;
                    // In use since the swap wrote its login, not since this settled it: sessions
                    // may have been working on it meanwhile (`statusline::idle_since_swap`).
                    let written = if pending.at > 0 { pending.at.min(now) } else { now };
                    set_active_since(s, written)?;
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
                if !restored
                    && let Some(from) = pending.from
                    && s.account(from)?.is_some()
                {
                    crate::usage_check::hold_back(s, from)?;
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
/// When the write-back first saw a changed login waiting to be identified (epoch ms), kept while
/// it waits (Claude Code may renew it meanwhile): if it turns out to be another pool account's,
/// that account has been in use since then.
pub(crate) const META_LOGIN_CHANGED_AT: &str = "login_changed_at";
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
    /// The account the profile's login belongs to (`relay::profile_account`): the active one,
    /// or the home account in relay mode.
    account: Account,
    oauth: serde_json::Value,
    fp: String,
    marker: String,
}

/// The login in the profile when it is neither one stored for the profile's account (the active
/// one, or the home account in relay mode) nor identified yet: a renewal of that account's
/// login, or a `/login` in the profile, which
/// `.claude.json` cannot tell apart after a swap (F14). `None` when there is nothing to identify,
/// or logins are not identified (`usage_check::identifies_logins`).
pub(crate) fn changed_login(store: &Store, paths: &Paths) -> Result<Option<ChangedLogin>> {
    if pending_swap(store)?.is_some() || store.meta(META_PROFILE_FOREIGN)?.as_deref() == Some("1") {
        return Ok(None);
    }
    let Some(account) = relay::profile_account(store)? else {
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
    if store.credential_meta(account.id)?.map(|m| m.refresh_fp) == Some(fp.clone()) {
        return Ok(None);
    }
    Ok(Some(ChangedLogin {
        account,
        oauth,
        fp,
        marker,
    }))
}

/// Identify a login that changed in the profile without cc-rotate writing it (`changed_login`),
/// with the swap lock held, by asking Claude Code whose it is with a copy it cannot renew:
/// - the profile's account's (a renewal) is stored as its login: the active account's, or the
///   home account's in relay mode;
/// - another pool account's (a `/login` in the profile) makes that account active, or the home
///   account in relay mode (`adopt_login`);
/// - any other account's (a `/login` with an account outside the pool) marks the profile
///   foreign, so its login is never stored.
///
/// `Unsure` while Claude Code cannot tell: the login is not stored and no swap runs, for at most
/// `UNIDENTIFIED_FOR_AT_MOST_MS`. After that it is stored as the profile's account's (the active
/// one, or the home account in relay mode), as it was before logins were identified, so a Claude
/// Code that cannot identify logins never stops the rotation.
pub fn identify_changed_login(store: &Store, paths: &Paths) -> Result<Settle> {
    let Some(changed) = changed_login(store, paths)? else {
        store.delete_meta(META_LOGIN_UNIDENTIFIED)?;
        store.delete_meta(META_LOGIN_TO_IDENTIFY)?;
        store.delete_meta(META_LOGIN_CHANGED_AT)?;
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
        Some(who) => match who.is(&changed.account) {
            Some(true) => Verdict::Own,
            active_is => match store
                .accounts()?
                .into_iter()
                .find(|a| a.id != changed.account.id && who.is(a) == Some(true))
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
    let active = &changed.account;
    store.immediate(|s| {
        // Only the login identified: the write-back, a swap or a `/login` may have dealt with
        // it meanwhile.
        if s.meta(META_CRED_SEEN)?.as_deref() == Some(changed.marker.as_str())
            || relay::profile_account(s)?.map(|a| a.id) != Some(active.id)
        {
            s.delete_meta(META_LOGIN_TO_IDENTIFY)?;
            return s.delete_meta(META_LOGIN_CHANGED_AT);
        }
        match &verdict {
            Verdict::Own => store_profile_login(s, active.id, &changed.oauth)?,
            Verdict::Adopt(account) => {
                let changed_at = s.meta_i64(META_LOGIN_CHANGED_AT)?.unwrap_or(time::now_ms());
                adopt_login(s, Some(active), account, Some(&changed.oauth), changed_at)?;
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
        s.delete_meta(META_LOGIN_CHANGED_AT)?;
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
        Some(changed.account.id),
        None,
        Some("login"),
        Some(json!({ "message": format!("The profile's login changed and Claude Code couldn't confirm its account for 10 minutes, so cc-rotate saved it as {}'s. Confirming needs the `claude` command and a network connection.", changed.account.label) })),
    )?;
    Ok(true)
}

/// Store a renewal or a fresh login of the profile's account (`relay::profile_account`) as its
/// login, and put it back in the pool when it was signed out: in relay mode, the home account's
/// login died in the profile and the user signed in to it again.
pub(crate) fn store_profile_login(
    s: &Store,
    account_id: i64,
    oauth: &serde_json::Value,
) -> Result<()> {
    s.save_credential(account_id, oauth, "sync")?;
    s.log_event(kind::SYNC, Some(account_id), None, Some("refreshed"), None)?;
    signed_in_again(s, account_id, oauth)
}

/// A live login of an account marked signed out puts it back in the pool, parked. An account in
/// use is never marked signed out, so it stays active.
fn signed_in_again(s: &Store, account_id: i64, oauth: &serde_json::Value) -> Result<()> {
    if !credfile::is_dead(oauth)
        && s.account(account_id)?.map(|a| a.state) == Some(AccountState::NeedsLogin)
    {
        s.set_state(account_id, AccountState::Parked)?;
    }
    Ok(())
}

/// Record that the active account has been in use since `at` (epoch ms): never before it was
/// last recorded, which was for the account before it.
fn set_active_since(s: &Store, at: i64) -> Result<()> {
    let at = at.max(s.meta_i64(META_ACTIVE_SINCE)?.unwrap_or(0));
    s.set_meta(META_ACTIVE_SINCE, at)
}

/// Make `account` active because the profile holds its login (a `/login` in the profile), and
/// store that login as its own. `changed_at` is when the profile's login changed (epoch ms), as
/// near as is known: the account has been in use since. Call inside a transaction.
///
/// In relay mode the profile's login is the home account's, so `account` becomes the home
/// account instead, and which account is active does not change: its login stays where Claude
/// Code renews it, and nothing else keeps a copy that renewal would retire.
pub(crate) fn adopt_login(
    s: &Store,
    active: Option<&Account>,
    account: &Account,
    oauth: Option<&serde_json::Value>,
    changed_at: i64,
) -> Result<()> {
    let relay_mode = relay::home(s)?.is_some();
    if relay_mode {
        relay::set_mode(s, Some(account.id))?;
    } else {
        if let Some(a) = active {
            s.set_state(a.id, AccountState::Parked)?;
        }
        s.set_state(account.id, AccountState::Active)?;
        set_active_since(s, changed_at)?;
    }
    if let Some(o) = oauth.filter(|o| !credfile::is_dead(o)) {
        s.save_credential(account.id, o, "sync")?;
        if relay_mode {
            signed_in_again(s, account.id, o)?;
        }
    }
    s.log_event(
        kind::ADOPTED_LOGIN,
        active.map(|a| a.id),
        Some(account.id),
        Some("login_in_profile"),
        relay_mode.then(|| json!({ "home": true })),
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
/// Prefix of the meta keys holding, per account, when a swap left it because a turn stopped at a
/// usage limit (epoch ms). See `stop_hold_until`.
pub const META_LEFT_AT_STOP_PREFIX: &str = "left_at_stop:";
/// How long an account left at a stop is not switched back to without a newer reading of it:
/// every 5-hour window has reset by then.
pub const LEFT_AT_STOP_HOLD_MS: i64 = 5 * 3600 * 1000;
/// When the active account became active: a swap, or a login adopted from the profile.
pub const META_ACTIVE_SINCE: &str = "active_since";

/// When the last swap's write of the login returned (after its lock waits, which `active_since`
/// precedes). The desktop wake waits for Claude Code's Keychain cache from here (spec 24.3).
pub const META_LOGIN_WRITTEN_AT: &str = "login_written_at";

/// `META_LOGIN_WRITTEN_AT`, or `active_since` for a swap made before it was recorded.
pub fn login_written_at(store: &Store) -> Result<i64> {
    Ok(store
        .meta_i64(META_LOGIN_WRITTEN_AT)?
        .unwrap_or(0)
        .max(active_since(store)?))
}
/// The model id the rotation profile's sessions last reported in their status line. The models
/// in use (`models_in_use`) fall back to it.
pub const META_PROFILE_MODEL: &str = "profile_model_seen";
/// A model counts as in use while a request for it came within this long of the latest request.
pub const MODELS_IN_USE_MS: i64 = 3_600_000;

/// The models sessions are using, which decide whether a window scoped to one model family
/// applies (`scheduler::WindowSpec::applies_to_any`): every model a reading came back for
/// within `MODELS_IN_USE_MS` of the latest one. The proxy records the model of each request it
/// relays and the status line its session's, so desktop sessions, which run no status line,
/// count too, and so does each model one session mixes (a Fable main loop with Haiku helpers).
/// Measured from the latest reading rather than now, so a pause does not forget them. Before
/// any reading names a model, the one the status line saw last.
pub fn models_in_use(store: &Store) -> Result<Vec<String>> {
    let models = store.recent_models(MODELS_IN_USE_MS)?;
    if !models.is_empty() {
        return Ok(models);
    }
    Ok(store.meta(META_PROFILE_MODEL)?.into_iter().collect())
}
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
    let models = models_in_use(store)?;
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
        let sample = store.current_usage(account.id)?;
        let thresholds = policy.thresholds.for_account(&account);
        let windows = scheduler::window_usage(sample.as_ref(), now_s, thresholds, &models);
        let stop_hold_until = stop_hold_until(
            store,
            account.id,
            sample.as_ref(),
            thresholds,
            &models,
            now_s * 1000,
        )?;
        let detected = detect_weekly_limit(store, account.id, now_s)?;
        pool.push(Candidate {
            weekly_limit: scheduler::weekly_limit(account.weekly_limit, detected),
            weekly_detected: detected,
            windows,
            order,
            held_back: usage_check::state(store, account.id)?.held_back,
            expiring: None,
            stop_hold_until,
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

/// Until when (epoch seconds) the account's room is not trusted, if a swap left it within
/// `LEFT_AT_STOP_HOLD_MS` because a turn stopped at a usage limit (`Reason::StopFailure`).
///
/// Usually the reading the account was left on shows the limit, and its thresholds and reset
/// time keep it out by themselves: no hold. When it does not (a limit the status line does not
/// report, or a stop reading that never came), its room is out of date, and switching back to
/// it would only stop the next turn again. A later reading that differs from that one (a usage
/// check after a reset, say) is news and ends the hold; one with the same numbers is not.
/// `latest` is the account's usage (`Store::current_usage`), `thresholds` and `model` what
/// applies to it.
pub fn stop_hold_until(
    store: &Store,
    account_id: i64,
    latest: Option<&Sample>,
    thresholds: scheduler::Thresholds,
    models: &[String],
    now_ms: i64,
) -> Result<Option<i64>> {
    let Some(left) = store.meta_i64(&left_at_stop_key(account_id))? else {
        return Ok(None);
    };
    let until_ms = left + LEFT_AT_STOP_HOLD_MS;
    if now_ms >= until_ms {
        return Ok(None);
    }
    let at_stop = match latest {
        Some(s) if s.ts <= left => Some(s.clone()),
        _ => store.current_usage_until(account_id, left)?,
    };
    if let Some(s) = &at_stop {
        // As the session that sent it saw it: a window scoped to its model applied to it.
        let own = s.model.clone().map(|m| vec![m]);
        let models = own.as_deref().unwrap_or(models);
        let shown = scheduler::window_usage(Some(s), s.ts / 1000, thresholds, models)
            .iter()
            .any(|w| w.blocking());
        if shown {
            return Ok(None);
        }
    }
    let news = match (latest, &at_stop) {
        (Some(l), _) if l.ts <= left => false,
        (Some(l), Some(s)) => {
            let reset_since = s
                .windows
                .iter()
                .any(|w| w.resets_at.is_some_and(|r| r * 1000 <= l.ts));
            reset_since || !same_numbers(l, s)
        }
        (Some(_), None) => true,
        (None, _) => false,
    };
    Ok((!news).then_some(until_ms / 1000))
}

/// Two readings say the same of the windows both have: the same usage within a point and the
/// same reset time. The status line and a usage check may round usage and reset times
/// differently, and one may report windows the other does not.
fn same_numbers(a: &Sample, b: &Sample) -> bool {
    let mut shared = 0;
    for w in &a.windows {
        let Some(o) = b.window(&w.name) else {
            continue;
        };
        shared += 1;
        let resets = match (w.resets_at, o.resets_at) {
            (Some(x), Some(y)) => same_reset(x, y),
            (x, y) => x == y,
        };
        if !resets || (w.pct - o.pct).abs() >= 1.0 {
            return false;
        }
    }
    shared > 0
}

pub fn left_at_stop_key(account_id: i64) -> String {
    format!("{META_LEFT_AT_STOP_PREFIX}{account_id}")
}

/// The account the scheduler would pick right now, excluding `exclude`.
pub fn next_target(store: &Store, exclude: Option<i64>) -> Result<Option<Account>> {
    let now_s = time::now_s();
    let (pool, policy) = load_pool(store, now_s)?;
    Ok(scheduler::pick(&pool, policy, exclude, now_s).map(|c| c.account.clone()))
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
pub(crate) const SAME_RESET_S: i64 = 60;

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
        if account.id != account_id
            && let Some(last) = store.last_sample(account.id)?
        {
            out.push(last);
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

/// Whether a threshold the active account's usage reached may not be its own: a window over its
/// threshold got there only by readings of the rotation profile's status line (`Store::own_high`)
/// and has a reset time not known to be the account's (section 8). Then a threshold swap
/// confirms first.
fn needs_confirmation(store: &Store, active_id: i64, now_ms: i64) -> Result<bool> {
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
    let latest_elsewhere = store
        .last_sample(active_id)?
        .is_some_and(|s| s.profile != crate::store::ROTATION_PROFILE);
    for w in active.windows.iter().filter(|w| w.blocking()) {
        // Without a reset time, a window is as the latest reading has it.
        let Some(r) = w.resets_at else {
            if latest_elsewhere {
                continue;
            }
            return Ok(true);
        };
        if store
            .own_high(active_id, &w.name, r)?
            .is_some_and(|high| high > w.pct - 0.5)
        {
            continue;
        }
        // Unsure when the reset time is not known to be the account's own, or is another
        // account's too (aligned reset times).
        if !own.iter().any(|&k| same_reset(k, r)) || others.iter().any(|&o| same_reset(o, r)) {
            return Ok(true);
        }
    }
    Ok(false)
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
    let models = models_in_use(store)?;
    let mut others: Vec<i64> = Vec::new();
    for account in store.accounts()? {
        if account.id != account_id
            && let Some(sample) = store.last_sample(account.id)?
        {
            others.extend(sample.windows.iter().filter_map(|w| w.resets_at));
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
            scheduler::window_usage(Some(s), now_ms / 1000, thresholds, &models)
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
    run_with(store, paths, req, &usage_check::renew_before_swap)
}

/// Why a session the macOS Keychain is locked to (started over SSH) may not write the profile's
/// login.
pub const KEYCHAIN_LOCKED: &str = "this session can't use the macOS Keychain, which holds the profile's login (it was started over SSH). Swap from the Mac's desktop session.";

/// Whether the profile's credentials can't be used in full (`Credentials::complete`): a Keychain
/// locked to this session, where the file may hold a stale login.
fn keychain_locked(complete: Result<bool>) -> bool {
    !complete.unwrap_or(true)
}

/// Whether a stored login whose access token expires at `expires_at` (epoch ms) must be renewed
/// before the relay uses it: it expires within the margin, or when is unknown.
fn needs_renewal(expires_at: Option<i64>, now_ms: i64, margin_s: i64) -> bool {
    expires_at.is_none_or(|at| crate::relay_proto::renew_due(at, now_ms, margin_s))
}

/// `run`, with what renews a login in relay mode (`usage_check::renew_before_swap`) given.
///
/// In relay mode (spec 6) a swap only changes which account is active: the profile keeps the
/// home account's login, so it is not read, identified or written, and Claude Code's locks are
/// not taken. A target whose stored access token is not valid for `app_account_renew_margin_s` is
/// renewed first, within the checks' time budget, and passed over like a busy one when that
/// leaves it no valid login.
fn run_with(
    store: &Store,
    paths: &Paths,
    req: &SwapRequest,
    renew: &dyn Fn(&Store, &Paths, i64) -> Result<usage_check::CheckOutcome>,
) -> Result<SwapOutcome> {
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
    // A session the macOS Keychain is locked to (started over SSH) cannot write the profile's
    // login where Claude Code reads it: the file instead would leave the desktop session's
    // sessions on the old account. A relay-mode swap writes no login.
    let mut keychain_checked = false;
    if relay::home(store)?.is_none() {
        if keychain_locked(paths.credentials().complete()) {
            return skip(store, req, KEYCHAIN_LOCKED);
        }
        keychain_checked = true;
    }
    // An earlier swap that stopped half way. While the profile's login is unidentified, no swap
    // runs: it would overwrite the only copy of that login, or file it under the wrong account.
    // A change of relay mode that stopped half way is settled here too, before the mode is read.
    if finish_dead_swap(store, paths, Settler::Locked)? == Settle::Unsure {
        return skip(
            store,
            req,
            "an earlier swap was interrupted and the profile's login isn't identified yet",
        );
    }
    let relay_home = relay::home(store)?;
    if relay_home.is_none() {
        if !keychain_checked && keychain_locked(paths.credentials().complete()) {
            return skip(store, req, KEYCHAIN_LOCKED);
        }
        // A login that changed in the profile is identified first, so the swap below never
        // stores another account's login as the outgoing account's.
        if identify_changed_login(store, paths)? == Settle::Unsure {
            return skip(
                store,
                req,
                "the profile's login changed and isn't identified yet",
            );
        }
    }

    let now = time::now_ms();
    let active = store.active_account()?;
    let active_id = active.as_ref().map(|a| a.id);

    if let Some(from) = req.from
        && active_id != Some(from)
    {
        return skip(store, req, "the account in use already changed");
    }
    let mut arrived_full = false;
    if req.reason.respects_cooldown()
        && let Some(last) = store.meta_i64(META_LAST_SWAP_AT)?
    {
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
    // What the user's other profiles read since, so an account an agent used up there is not
    // picked. Best effort: an unreadable profile must not stop the swap.
    let _ = crate::profile_usage::import(store, paths, now);

    // A threshold swap that rests on a reading the status line could not attribute for sure
    // (section 8) first reads the account in use's own usage (section 20.4). When that is not
    // possible, the swap goes ahead as the reading says.
    // Checks and the confirmation share one time budget: sessions may be waiting at a limit.
    let check_deadline = Instant::now() + PRE_SWAP_CHECK_BUDGET;
    let mut confirmed = None;
    // In relay mode readings are the relay's own, read from each response for the account it
    // used (spec 7): nothing to confirm.
    if relay_home.is_none()
        && req.reason.is_threshold()
        && let Some(account) = &active
        && needs_confirmation(store, account.id, now)?
    {
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
    let mut renewed_ids: Vec<i64> = Vec::new();
    let mut unrenewable = false;
    let mut attempts = 0;
    let (target, _target_lock, pool, policy) = loop {
        attempts += 1;
        let last_try = req.to.is_some() || attempts >= MAX_PICK_ATTEMPTS;
        let now_s = time::now_s();
        let (mut pool, policy) = load_pool(store, now_s)?;
        let some_ready = scheduler::pick(&pool, policy, active_id, now_s).is_some();
        pool.retain(|c| !passed_over.contains(&c.account.id));
        if req.to.is_none()
            && some_ready
            && scheduler::pick(&pool, policy, active_id, now_s).is_none()
        {
            // The accounts that could take over are only busy (a check of them running): not an
            // exhausted pool, so no such event or notification.
            return skip(
                store,
                req,
                if unrenewable {
                    "the accounts that could take over are being checked, or their logins couldn't be renewed"
                } else {
                    "the accounts that could take over are being checked"
                },
            );
        }
        let target = match choose(store, req, &pool, policy, active.as_ref(), now_s)? {
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
        let now_s = time::now_s();
        let (mut pool, policy) = load_pool(store, now_s)?;
        pool.retain(|c| !passed_over.contains(&c.account.id));
        match choose(store, req, &pool, policy, active.as_ref(), now_s)? {
            Choice::Target(settled) if settled.id == target.id => {
                if req.to.is_some() && usage_check::state(store, target.id)?.held_back {
                    return Err(anyhow!(
                        "{} is on hold until a usage check confirms its login. Check its usage first.",
                        target.label
                    ));
                }
                // In relay mode the relay sends requests with the target's stored access token
                // (the home account's is the profile's, which Claude Code renews), so it must
                // stay valid a while. Renewal runs a check, which takes the account lock.
                if relay_home.is_some_and(|home| home != target.id)
                    && needs_renewal(
                        store.credential_meta(target.id)?.and_then(|m| m.expires_at),
                        time::now_ms(),
                        settings.relay_renew_margin_s,
                    )
                {
                    if !renewed_ids.contains(&target.id) && Instant::now() < check_deadline {
                        drop(lock);
                        let outcome = renew(store, paths, target.id).unwrap_or_else(|e| {
                            usage_check::CheckOutcome::Failed {
                                why: format!("{e:#}"),
                            }
                        });
                        checked.push(usage_check::summary(store, target.id, &outcome));
                        renewed_ids.push(target.id);
                        continue;
                    }
                    if !last_try {
                        passed_over.push(target.id);
                        unrenewable = true;
                        continue;
                    }
                    return Err(log_error(
                        store,
                        req,
                        &target,
                        format!(
                            "{}'s login expires soon and couldn't be renewed. Check its usage to renew it.",
                            target.label
                        ),
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
            "{} has no weekly limit, and {} {}",
            target.label,
            from.account.label,
            if from.weekly_limit == WeeklyLimit::Has {
                "has one"
            } else {
                "may have one"
            }
        )),
        _ => None,
    };
    let trigger_window = outgoing
        .and_then(Candidate::blocking)
        .map(|w| w.name.clone());

    if relay_home.is_some() {
        // Spec 6, steps 5 and 6: the relay sends every request after this commit with the
        // target's login. The profile and Claude Code's locks are left alone.
        let outgoing_state = match (&active, req.reason) {
            (Some(_), Reason::DeadLogin) => AccountState::NeedsLogin,
            _ => AccountState::Parked,
        };
        let from_sample = match active_id {
            Some(id) => store.current_usage(id)?,
            None => None,
        };
        let commit = SwapCommit {
            active: active.as_ref(),
            outgoing_state,
            target: &target,
            reason: req.reason,
            now,
            promoted_until: promoted_until(&pool, policy, target.id),
            previous_swap: store.meta_i64(META_LAST_SWAP_AT)?,
            from_sample,
            pick: json!(reason_for(target.id)),
            trigger_window,
            why,
            arrived_full,
            checked,
            confirmed,
        };
        store.immediate(|s| record_swap(s, &commit))?;
        return Ok(SwapOutcome::Swapped {
            from: active_id,
            to: target.id,
        });
    }

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
        Some(id) => store.current_usage(id)?,
        None => None,
    };
    let previous_swap = store.meta_i64(META_LAST_SWAP_AT)?;
    let pending = PendingSwap {
        fp: target_login.refresh_fp.clone(),
        from: active_id,
        to: target.id,
        at: now,
        relay: None,
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
    // When the new login is in place: Claude Code may have read the old one until now.
    let written_at = time::now_ms();
    let marker = creds.change_marker().ok().flatten();

    let commit = SwapCommit {
        active: active.as_ref(),
        outgoing_state,
        target: &target,
        reason: req.reason,
        now,
        promoted_until: promoted_until(&pool, policy, target.id),
        previous_swap,
        from_sample,
        pick: json!(reason_for(target.id)),
        trigger_window,
        why,
        arrived_full,
        checked,
        confirmed,
    };
    let committed = store.immediate(|s| {
        s.set_meta(META_PROFILE_FOREIGN, "0")?;
        s.set_meta(META_LOGIN_WRITTEN_AT, written_at)?;
        if let Some(marker) = &marker {
            s.set_meta(META_CRED_SEEN, marker)?;
        }
        s.delete_meta(META_SWAP_PENDING)?;
        record_swap(s, &commit)
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

/// What a swap records when it commits (`record_swap`).
struct SwapCommit<'a> {
    /// The account that was active.
    active: Option<&'a Account>,
    /// What the outgoing account becomes: parked, or signed out.
    outgoing_state: AccountState,
    target: &'a Account,
    reason: Reason,
    /// When the swap started (epoch ms).
    now: i64,
    /// The reset of the expiring weekly budget the target was taken for.
    promoted_until: Option<i64>,
    /// When the swap before this one committed (epoch ms).
    previous_swap: Option<i64>,
    /// The outgoing account's latest reading.
    from_sample: Option<Sample>,
    /// Why the scheduler ranks the target where it does (section 7.3).
    pick: serde_json::Value,
    /// The outgoing account's window that triggered the swap.
    trigger_window: Option<String>,
    why: Option<String>,
    arrived_full: bool,
    /// The checks run before the swap.
    checked: Vec<serde_json::Value>,
    /// What the confirmation of the account in use found.
    confirmed: Option<&'static str>,
}

/// The reset of the expiring weekly budget `target` is taken for, if it is: it stays ahead after
/// its 5-hour limit this week.
fn promoted_until(pool: &[Candidate], policy: Policy, target: i64) -> Option<i64> {
    pool.iter()
        .find(|c| c.account.id == target)
        .and_then(|c| policy.expiring(c))
        .map(|e| e.resets_at)
}

/// Record a swap, inside its commit's transaction: the states, the outgoing account left at a
/// limit, the target's promotion, when it happened, the stale-reading guard's reset times, and
/// the `swap` event. Shared by swaps that write the profile's login and relay-mode swaps.
fn record_swap(s: &Store, c: &SwapCommit) -> Result<()> {
    if let Some(account) = c.active {
        s.set_state(account.id, c.outgoing_state)?;
        if matches!(c.reason, Reason::StopFailure | Reason::LimitRefused) {
            // At the commit: a reading of the account taken while it was still in use,
            // during the swap's lock waits, is older and so not news (`stop_hold_until`).
            s.set_meta(&left_at_stop_key(account.id), time::now_ms())?;
        }
    }
    s.set_state(c.target.id, AccountState::Active)?;
    if let Some(reset) = c.promoted_until {
        s.set_meta(&crate::budget::promoted_key(c.target.id), reset)?;
    }
    s.set_meta(META_LAST_SWAP_AT, c.now)?;
    s.set_meta(META_ACTIVE_SINCE, c.now)?;
    let mut resets: Vec<i64> = match c.previous_swap {
        Some(last) if c.now - last <= STALE_SAMPLE_GUARD_MS => left_resets(s)?,
        _ => Vec::new(),
    };
    resets.extend(
        c.from_sample
            .iter()
            .flat_map(|x| &x.windows)
            .filter_map(|w| w.resets_at),
    );
    resets.sort_unstable();
    resets.dedup();
    let resets: Vec<String> = resets.iter().map(i64::to_string).collect();
    s.set_meta(META_LAST_SWAP_FROM_RESETS, resets.join(","))?;
    let from_windows: serde_json::Map<String, serde_json::Value> = c
        .from_sample
        .iter()
        .flat_map(|x| &x.windows)
        .map(|w| (w.name.clone(), json!(w.pct)))
        .collect();
    let mut detail = json!({
        "from_label": c.active.map(|a| a.label.clone()),
        "to_label": c.target.label,
        "from_state": c.outgoing_state.as_str(),
        "from_windows": from_windows,
        "pick": c.pick,
    });
    if let Some(window) = &c.trigger_window {
        detail["window"] = json!(window);
    }
    if let Some(why) = &c.why {
        detail["why"] = json!(why);
    }
    if c.arrived_full {
        detail["arrived_over_limit"] = json!(true);
    }
    if !c.checked.is_empty() {
        detail["checked"] = json!(c.checked);
    }
    if let Some(outcome) = c.confirmed {
        detail["confirmed"] = json!(outcome);
    }
    s.log_event(
        kind::SWAP,
        c.active.map(|a| a.id),
        Some(c.target.id),
        Some(c.reason.as_str()),
        Some(detail),
    )?;
    Ok(())
}

/// Move the profile's login from `outgoing`'s to `incoming`'s for a change of relay mode (spec 4),
/// with the swap lock held: main 6 steps 4 to 6 and 8. Under Claude Code's locks the profile's
/// login is saved to `outgoing`'s row, then `incoming`'s stored login is written in its place,
/// with a pending mark that carries `change` so `finish_dead_swap` settles a crash in between as
/// a change of mode. The commit sets the mode; which account is active does not change.
pub fn move_profile_login(
    store: &Store,
    paths: &Paths,
    outgoing: i64,
    incoming: i64,
    change: RelayChange,
) -> Result<()> {
    if outgoing == incoming {
        return Err(anyhow!("the profile already has that account's login"));
    }
    if keychain_locked(paths.credentials().complete()) {
        return Err(anyhow!(KEYCHAIN_LOCKED));
    }
    let incoming_account = store
        .account(incoming)?
        .ok_or_else(|| anyhow!("account {incoming} does not exist"))?;
    // A removed account: the profile's login is nobody's in the pool, so it is not kept.
    let outgoing_account = store.account(outgoing)?;
    let now = time::now_ms();

    let timeout = Duration::from_millis(store.settings()?.lock_timeout_ms);
    // Like a swap's target: a check of the incoming account (or the relay's renewal of it) may
    // be renewing its login in a scratch profile, which would leave the copy written into the
    // profile dead. Its lock waits for that, and is held through the commit.
    let account_lock =
        DirLock::acquire(&paths.account_lock(incoming), LockOptions::claude(timeout))
            .map_err(|e| anyhow!("couldn't lock {}'s login: {e}", incoming_account.label))?;
    if let usage_check::Recovery::Busy(why) = usage_check::recover(store, paths, incoming)? {
        return Err(anyhow!(
            "{} can't be used now: {why}",
            incoming_account.label
        ));
    }
    if usage_check::state(store, incoming)?.held_back {
        return Err(anyhow!(
            "{} is on hold until a usage check confirms its login. Check its usage first.",
            incoming_account.label
        ));
    }
    // Read under the lock: a check that finished while it was awaited may have renewed it.
    let incoming_login = store
        .credential(incoming)?
        .ok_or_else(|| anyhow!("{} has no saved login", incoming_account.label))?;

    let opts = LockOptions::claude(timeout);
    let refresh_lock = DirLock::acquire(&paths.refresh_lock(), opts)
        .context("taking Claude Code's refresh lock")?;
    let legacy_lock = DirLock::acquire(&paths.legacy_refresh_lock(), opts)
        .context("taking Claude Code's legacy refresh lock")?;
    let storage_lock =
        DirLock::acquire(&paths.storage_lock(), LockOptions::claude_storage(timeout))
            .context("taking Claude Code's credentials write lock")?;

    let creds = paths.credentials();
    let previous = creds.read_oauth()?;
    let foreign = store.meta(META_PROFILE_FOREIGN)?.as_deref() == Some("1");
    // Like a swap: a login that changed unseen since it was identified is identified first.
    if let (Some(account), Some(oauth)) = (&outgoing_account, &previous) {
        let fp = credfile::oauth_fingerprint(oauth);
        if !foreign
            && !credfile::is_dead(oauth)
            && fp.is_some()
            && fp != store.credential_meta(account.id)?.map(|m| m.refresh_fp)
            && creds.change_marker()? != store.meta(META_CRED_SEEN)?
            && usage_check::identifies_logins(store, paths)?
        {
            return Err(anyhow!(
                "the profile's login changed and needs identifying first. Try again in a minute."
            ));
        }
    }
    // A dead login is not stored over the one the account has.
    if let Some(account) = outgoing_account.as_ref().filter(|_| !foreign)
        && let Some(oauth) = previous.as_ref().filter(|o| !credfile::is_dead(o))
    {
        store.save_credential(account.id, oauth, "swap_out")?;
    }

    backup_daily(store, paths, now).context("backing up the profile's credentials")?;
    let pending = PendingSwap {
        fp: incoming_login.refresh_fp.clone(),
        from: Some(outgoing),
        to: incoming,
        at: now,
        relay: Some(change),
    };
    store.set_meta(META_SWAP_PENDING, serde_json::to_string(&pending)?)?;
    if let Err(e) = creds.write_oauth(&incoming_login.oauth) {
        if restore_outgoing(&creds, previous.as_ref(), &incoming_login.refresh_fp) {
            let _ = store.delete_meta(META_SWAP_PENDING);
        }
        return Err(e);
    }
    let written_at = time::now_ms();
    let marker = creds.change_marker().ok().flatten();

    let committed = store.immediate(|s| {
        relay::set_mode(s, change.home())?;
        s.set_meta(META_PROFILE_FOREIGN, "0")?;
        s.set_meta(META_LOGIN_WRITTEN_AT, written_at)?;
        if let Some(marker) = &marker {
            s.set_meta(META_CRED_SEEN, marker)?;
        }
        s.delete_meta(META_SWAP_PENDING)?;
        s.log_event(
            kind::RELAY_MODE,
            Some(outgoing),
            Some(incoming),
            Some(change.as_str()),
            Some(json!({ "home": change.home() })),
        )?;
        Ok(())
    });
    if let Err(e) = committed {
        let restored = restore_outgoing(&creds, previous.as_ref(), &incoming_login.refresh_fp);
        if restored {
            let _ = store.delete_meta(META_SWAP_PENDING);
        }
        drop(storage_lock);
        drop(legacy_lock);
        drop(refresh_lock);
        drop(account_lock);
        return Err(e.context(if restored {
            "couldn't record the change of the app account, so the previous login was put back"
        } else {
            "couldn't record the change of the app account"
        }));
    }
    drop(storage_lock);
    drop(legacy_lock);
    drop(refresh_lock);
    drop(account_lock);
    Ok(())
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
    /// Not written: relay mode is on and the account is not the home account, whose login the
    /// profile keeps. The relay uses the stored login.
    NotHome,
}

/// Write a fresh login for the account that is already active (a re-login of the active
/// account), under Claude Code's refresh locks like any swap. The fresh login is stored for the
/// account before this is called, so when it is not written, nothing is lost.
///
/// In relay mode the profile's login is the home account's: only the home account's fresh login
/// is written, whether or not it is active.
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
    match relay::home(store)? {
        // Not stored again either: a check may have renewed it since the caller stored it.
        Some(home) if home != account_id => return Ok(ActiveLogin::NotHome),
        Some(_) => {}
        None => {
            if store.active_account()?.map(|a| a.id) != Some(account_id) {
                return Ok(ActiveLogin::NoLongerActive);
            }
        }
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

/// The account a request swaps to: the one it names, a proactive target, or the pick.
fn choose(
    store: &Store,
    req: &SwapRequest,
    pool: &[Candidate],
    policy: Policy,
    active: Option<&Account>,
    now_s: i64,
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
        None if req.reason == Reason::WeeklyExpiring => {
            match scheduler::expiring_target(pool, policy, now_s) {
                Some(c) => c.account.clone(),
                None => {
                    return Ok(Choice::Done(skip(
                        store,
                        req,
                        "no earlier expiring weekly budget needs to take over",
                    )?));
                }
            }
        }
        None if req.reason == Reason::PreferredAvailable => {
            match scheduler::switch_back_target(pool, policy, now_s) {
                Some(c) => c.account.clone(),
                None => {
                    let in_use = pool
                        .iter()
                        .find(|c| Some(c.account.id) == active.map(|a| a.id));
                    let held = pool.iter().any(|c| {
                        c.eligible()
                            && c.weekly_limit == WeeklyLimit::None
                            && c.has_margin()
                            && c.on_stop_hold(now_s)
                    });
                    let why = if !policy.prefer_no_weekly_limit {
                        "accounts without a weekly limit no longer go first"
                    } else if in_use.is_some_and(|c| c.weekly_limit == WeeklyLimit::None) {
                        "the account in use has no weekly limit"
                    } else if in_use.is_some_and(|c| c.blocking().is_some()) {
                        "the account in use is at a limit"
                    } else if held {
                        "every ready account without a weekly limit had a session stop at a limit its usage didn't show"
                    } else {
                        "no account without a weekly limit is ready to swap back to"
                    };
                    return Ok(Choice::Done(skip(store, req, why)?));
                }
            }
        }
        None => match scheduler::pick(pool, policy, active.map(|a| a.id), now_s) {
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
    if req.reason == Reason::DeadLogin
        && let Some(account) = active
    {
        store.set_state(account.id, AccountState::NeedsLogin)?;
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

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::relay;
    use crate::store::AccountIdentity;

    /// A pool of `a`, `b` and `c` in an in-memory store, with a scratch profile and data folder.
    /// `a` is active and its login is in the profile, next to an MCP login.
    pub(crate) struct Env {
        _root: tempfile::TempDir,
        pub paths: Paths,
        pub store: Store,
        pub a: i64,
        pub b: i64,
        pub c: i64,
    }

    /// A login whose access token expires `expires_in_ms` from now.
    pub(crate) fn oauth_expiring(tag: &str, expires_in_ms: i64) -> serde_json::Value {
        json!({
            "accessToken": format!("access-{tag}"),
            "refreshToken": format!("refresh-{tag}"),
            "expiresAt": time::now_ms() + expires_in_ms,
            "scopes": ["user:inference"],
            "subscriptionType": "max",
        })
    }

    pub(crate) fn oauth(tag: &str) -> serde_json::Value {
        oauth_expiring(tag, 3_600_000)
    }

    impl Env {
        pub(crate) fn new() -> Env {
            let root = tempfile::tempdir().unwrap();
            let data = root.path().join("data");
            let profile = root.path().join("profile");
            let home = root.path().join("home");
            for dir in [&data, &profile, &home] {
                std::fs::create_dir_all(dir).unwrap();
            }
            let paths = Paths::new(data.clone(), profile).with_home(home);
            let store = Store::open_in_memory().unwrap();
            // No check may run a real Claude Code.
            store.set_setting("usage_checks", Some("false")).unwrap();
            store
                .set_setting("use_expiring_weekly", Some("false"))
                .unwrap();
            let nowhere = data.join("no-claude-here");
            store
                .set_setting("claude_path", Some(&nowhere.display().to_string()))
                .unwrap();
            let ident = |u: &str| AccountIdentity {
                account_uuid: Some(u.into()),
                email: Some(format!("{u}@x.test")),
                org_uuid: Some("org-x".into()),
                ..Default::default()
            };
            let a = store
                .insert_account("a", &ident("ua"), AccountState::Active)
                .unwrap();
            let b = store
                .insert_account("b", &ident("ub"), AccountState::Parked)
                .unwrap();
            let c = store
                .insert_account("c", &ident("uc"), AccountState::Parked)
                .unwrap();
            store.save_credential(a, &oauth("a1"), "login").unwrap();
            store.save_credential(b, &oauth("b1"), "login").unwrap();
            store.save_credential(c, &oauth("c1"), "login").unwrap();
            let env = Env {
                _root: root,
                paths,
                store,
                a,
                b,
                c,
            };
            env.write_profile(&oauth("a1"));
            env
        }

        /// Put `oauth` in the profile, next to an MCP login.
        pub(crate) fn write_profile(&self, oauth: &serde_json::Value) {
            std::fs::write(
                self.paths.credentials_file(),
                serde_json::to_string(
                    &json!({"mcpOAuth": {"srv|1": {"accessToken": "mcp"}}, "claudeAiOauth": oauth}),
                )
                .unwrap(),
            )
            .unwrap();
        }

        pub(crate) fn profile_bytes(&self) -> Vec<u8> {
            std::fs::read(self.paths.credentials_file()).unwrap()
        }

        pub(crate) fn profile_refresh(&self) -> String {
            let doc: serde_json::Value = serde_json::from_slice(&self.profile_bytes()).unwrap();
            assert_eq!(doc["mcpOAuth"], json!({"srv|1": {"accessToken": "mcp"}}));
            doc["claudeAiOauth"]["refreshToken"]
                .as_str()
                .unwrap()
                .to_string()
        }

        pub(crate) fn stored_refresh(&self, id: i64) -> String {
            self.store.credential(id).unwrap().unwrap().oauth["refreshToken"]
                .as_str()
                .unwrap()
                .to_string()
        }

        pub(crate) fn state(&self, id: i64) -> AccountState {
            self.store.account(id).unwrap().unwrap().state
        }

        /// Make `active` the active account and the others parked.
        pub(crate) fn activate(&self, active: i64) {
            for id in [self.a, self.b, self.c] {
                let state = if id == active {
                    AccountState::Active
                } else {
                    AccountState::Parked
                };
                self.store.set_state(id, state).unwrap();
            }
        }

        /// Relay mode on with `a` as home, `b` active, `a`'s login in the profile.
        fn relay_on_b_active(&self) {
            relay::set_mode(&self.store, Some(self.a)).unwrap();
            self.activate(self.b);
        }
    }

    fn manual(to: Option<i64>) -> SwapRequest {
        SwapRequest {
            from: None,
            to,
            reason: Reason::Manual,
            wait: true,
        }
    }

    fn no_renewal(_: &Store, _: &Paths, _: i64) -> Result<usage_check::CheckOutcome> {
        panic!("no renewal expected")
    }

    #[test]
    fn relay_swap_leaves_profile() {
        let env = Env::new();
        env.relay_on_b_active();
        let before = env.profile_bytes();
        let out = run_with(&env.store, &env.paths, &manual(Some(env.c)), &no_renewal).unwrap();
        assert_eq!(
            out,
            SwapOutcome::Swapped {
                from: Some(env.b),
                to: env.c
            }
        );
        assert_eq!(env.state(env.c), AccountState::Active);
        assert_eq!(env.state(env.b), AccountState::Parked);
        assert_eq!(env.state(env.a), AccountState::Parked);
        assert_eq!(env.profile_bytes(), before, "the profile is not written");
        assert_eq!(
            env.stored_refresh(env.b),
            "refresh-b1",
            "nothing saved as b's"
        );
        let event = env.store.last_event_of(kind::SWAP).unwrap().unwrap();
        assert_eq!(
            (event.account_from, event.account_to),
            (Some(env.b), Some(env.c))
        );
        assert!(pending_swap(&env.store).unwrap().is_none());
        assert!(env.store.meta(META_SWAP_PENDING).unwrap().is_none());
        assert!(env.store.meta(META_LOGIN_WRITTEN_AT).unwrap().is_none());
        assert_eq!(relay::home(&env.store).unwrap(), Some(env.a));
        assert!(!env.paths.refresh_lock().exists());
    }

    #[test]
    fn relay_swap_renews_target() {
        let env = Env::new();
        env.relay_on_b_active();
        // Within the 900 s margin.
        env.store
            .save_credential(env.c, &oauth_expiring("c1", 300_000), "login")
            .unwrap();
        let before = env.profile_bytes();
        let calls = Cell::new(0);
        let renew = |s: &Store, _: &Paths, id: i64| -> Result<usage_check::CheckOutcome> {
            calls.set(calls.get() + 1);
            assert_eq!(id, env.c);
            assert_eq!(s.active_account()?.unwrap().id, env.b, "before the commit");
            s.save_credential(id, &oauth("c2"), "renew")?;
            Ok(usage_check::CheckOutcome::Read {
                windows: Vec::new(),
            })
        };
        let out = run_with(&env.store, &env.paths, &manual(Some(env.c)), &renew).unwrap();
        assert_eq!(
            out,
            SwapOutcome::Swapped {
                from: Some(env.b),
                to: env.c
            }
        );
        assert_eq!(calls.get(), 1);
        assert_eq!(env.stored_refresh(env.c), "refresh-c2");
        assert_eq!(env.profile_bytes(), before);
    }

    #[test]
    fn relay_swap_refused_when_renewal_fails() {
        let env = Env::new();
        env.relay_on_b_active();
        env.store
            .save_credential(env.c, &oauth_expiring("c1", 300_000), "login")
            .unwrap();
        let calls = Cell::new(0);
        let renew = |_: &Store, _: &Paths, _: i64| -> Result<usage_check::CheckOutcome> {
            calls.set(calls.get() + 1);
            Ok(usage_check::CheckOutcome::Failed {
                why: "no network".into(),
            })
        };
        assert!(run_with(&env.store, &env.paths, &manual(Some(env.c)), &renew).is_err());
        assert_eq!(calls.get(), 1);
        assert_eq!(env.state(env.b), AccountState::Active);
        assert_eq!(env.state(env.c), AccountState::Parked);
    }

    #[test]
    fn relay_swap_passes_over_an_account_it_cannot_renew() {
        let env = Env::new();
        env.relay_on_b_active();
        // The home account is paused, so only c could take over, and its login can't be renewed.
        env.store.set_state(env.a, AccountState::Paused).unwrap();
        env.store
            .save_credential(env.c, &oauth_expiring("c1", 300_000), "login")
            .unwrap();
        let renew = |_: &Store, _: &Paths, _: i64| -> Result<usage_check::CheckOutcome> {
            Ok(usage_check::CheckOutcome::Failed {
                why: "no network".into(),
            })
        };
        let out = run_with(&env.store, &env.paths, &manual(None), &renew).unwrap();
        let SwapOutcome::Skipped { why } = out else {
            panic!("{out:?}");
        };
        assert!(why.contains("couldn't be renewed"), "{why}");
        assert_eq!(env.state(env.b), AccountState::Active);
        assert_eq!(env.state(env.c), AccountState::Parked);
    }

    #[test]
    fn relay_swap_to_home() {
        let env = Env::new();
        env.relay_on_b_active();
        // The home account's stored login is not what the relay uses: never renewed for a swap.
        env.store
            .save_credential(env.a, &oauth_expiring("a1", 60_000), "login")
            .unwrap();
        let before = env.profile_bytes();
        let out = run_with(&env.store, &env.paths, &manual(Some(env.a)), &no_renewal).unwrap();
        assert_eq!(
            out,
            SwapOutcome::Swapped {
                from: Some(env.b),
                to: env.a
            }
        );
        assert_eq!(env.state(env.a), AccountState::Active);
        assert_eq!(env.state(env.b), AccountState::Parked);
        assert_eq!(env.profile_bytes(), before);
    }

    #[test]
    fn relay_swap_at_a_refused_limit_holds_the_account_back() {
        let env = Env::new();
        env.relay_on_b_active();
        let req = SwapRequest {
            from: Some(env.b),
            to: Some(env.c),
            reason: Reason::LimitRefused,
            wait: true,
        };
        run_with(&env.store, &env.paths, &req, &no_renewal).unwrap();
        assert!(
            env.store
                .meta_i64(&left_at_stop_key(env.b))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn relay_swap_at_a_refused_login_parks_the_account() {
        let env = Env::new();
        env.relay_on_b_active();
        let req = SwapRequest {
            from: Some(env.b),
            to: Some(env.c),
            reason: Reason::Unauthorized,
            wait: true,
        };
        run_with(&env.store, &env.paths, &req, &no_renewal).unwrap();
        // A check settles the login; only one that sees it rejected marks it `needs_login`.
        assert_eq!(env.state(env.b), AccountState::Parked);
        assert_eq!(env.state(env.c), AccountState::Active);
        assert!(
            env.store
                .meta_i64(&left_at_stop_key(env.b))
                .unwrap()
                .is_none()
        );
        let ev = env.store.last_event_of(kind::SWAP).unwrap().unwrap();
        assert_eq!(ev.reason.as_deref(), Some("unauthorized"));
    }

    #[test]
    fn relay_refused_login_with_nowhere_to_go_stays_active() {
        let env = Env::new();
        env.relay_on_b_active();
        for id in [env.a, env.c] {
            env.store.set_state(id, AccountState::Paused).unwrap();
        }
        let req = SwapRequest {
            from: Some(env.b),
            to: None,
            reason: Reason::Unauthorized,
            wait: true,
        };
        run_with(&env.store, &env.paths, &req, &no_renewal).unwrap();
        assert_eq!(env.state(env.b), AccountState::Active);
    }

    #[test]
    fn relay_changed_login_compares_with_home() {
        let env = Env::new();
        env.relay_on_b_active();
        env.store.set_setting("usage_checks", Some("true")).unwrap();
        if !usage_check::identifies_logins(&env.store, &env.paths).unwrap() {
            eprintln!("skipped: a Keychain this test can't use");
            return;
        }
        // The home account's stored login is the profile's: nothing changed.
        assert!(changed_login(&env.store, &env.paths).unwrap().is_none());
        // Claude Code renewed it in the profile: a change of the home account's login.
        env.write_profile(&oauth("a2"));
        let changed = changed_login(&env.store, &env.paths).unwrap().unwrap();
        assert_eq!(changed.account.id, env.a);
    }

    fn pending_relay(env: &Env, from: i64, to: i64, change: RelayChange) {
        let pending = PendingSwap {
            fp: env.store.credential_meta(to).unwrap().unwrap().refresh_fp,
            from: Some(from),
            to,
            at: time::now_ms(),
            relay: Some(change),
        };
        env.store
            .set_meta(META_SWAP_PENDING, serde_json::to_string(&pending).unwrap())
            .unwrap();
    }

    #[test]
    fn dead_relay_on_settles_mode() {
        // `relay on` with home a, b active, stopped after writing a's login into the profile.
        let env = Env::new();
        env.activate(env.b);
        pending_relay(&env, env.b, env.a, RelayChange::On { home: env.a });
        env.write_profile(&oauth("a1"));
        let settled = finish_dead_swap(&env.store, &env.paths, Settler::Locked).unwrap();
        assert_eq!(settled, Settle::Settled);
        assert_eq!(relay::home(&env.store).unwrap(), Some(env.a));
        assert_eq!(env.state(env.b), AccountState::Active, "states left alone");
        assert_eq!(env.state(env.a), AccountState::Parked);
        assert!(pending_swap(&env.store).unwrap().is_none());
        assert!(env.store.last_event_of(kind::SWAP).unwrap().is_none());

        // Stopped before the write: the profile still has b's login.
        let env = Env::new();
        env.activate(env.b);
        pending_relay(&env, env.b, env.a, RelayChange::On { home: env.a });
        env.write_profile(&oauth("b1"));
        let settled = finish_dead_swap(&env.store, &env.paths, Settler::Locked).unwrap();
        assert_eq!(settled, Settle::Settled);
        assert!(pending_swap(&env.store).unwrap().is_none());
        assert_eq!(relay::home(&env.store).unwrap(), None);
        assert_eq!(env.state(env.b), AccountState::Active);
    }

    #[test]
    fn dead_relay_off_settles_mode() {
        // `relay off` with home a, c active, stopped after writing c's login into the profile.
        let env = Env::new();
        relay::set_mode(&env.store, Some(env.a)).unwrap();
        env.activate(env.c);
        pending_relay(&env, env.a, env.c, RelayChange::Off);
        env.write_profile(&oauth("c1"));
        let settled = finish_dead_swap(&env.store, &env.paths, Settler::Tick).unwrap();
        assert_eq!(settled, Settle::Settled);
        assert_eq!(relay::home(&env.store).unwrap(), None);
        assert_eq!(env.state(env.c), AccountState::Active);
        assert!(pending_swap(&env.store).unwrap().is_none());
    }

    #[test]
    fn a_locked_keychain_is_refused() {
        assert!(keychain_locked(Ok(false)));
        assert!(!keychain_locked(Ok(true)));
        // As before: a Keychain that can't be asked is not taken for a locked one.
        assert!(!keychain_locked(Err(anyhow!("security failed"))));
    }

    #[test]
    fn relay_install_active_login_writes_only_home() {
        let env = Env::new();
        env.relay_on_b_active();
        let before = env.profile_bytes();
        env.store
            .save_credential(env.b, &oauth("b-fresh"), "login")
            .unwrap();
        let out = install_active_login(&env.store, &env.paths, env.b, &oauth("b-fresh")).unwrap();
        assert_eq!(out, ActiveLogin::NotHome);
        assert_eq!(
            env.profile_bytes(),
            before,
            "the home account's login stays"
        );
        assert_eq!(env.stored_refresh(env.b), "refresh-b-fresh");

        // The home account's fresh login goes into the profile, though it isn't active.
        env.store
            .save_credential(env.a, &oauth("a-fresh"), "login")
            .unwrap();
        let out = install_active_login(&env.store, &env.paths, env.a, &oauth("a-fresh")).unwrap();
        assert_eq!(out, ActiveLogin::Written);
        assert_eq!(env.profile_refresh(), "refresh-a-fresh");
        assert_eq!(env.state(env.b), AccountState::Active);
    }

    #[test]
    fn an_old_pending_mark_still_reads() {
        let raw = r#"{"fp":"x","from":1,"to":2,"at":5}"#;
        let pending: PendingSwap = serde_json::from_str(raw).unwrap();
        assert!(pending.relay.is_none());
    }

    #[test]
    fn a_swap_with_relay_mode_off_still_writes_the_profile() {
        let env = Env::new();
        let out = run_with(&env.store, &env.paths, &manual(Some(env.b)), &no_renewal).unwrap();
        assert_eq!(
            out,
            SwapOutcome::Swapped {
                from: Some(env.a),
                to: env.b
            }
        );
        assert_eq!(env.profile_refresh(), "refresh-b1");
        assert!(env.store.meta(META_LOGIN_WRITTEN_AT).unwrap().is_some());
    }
}
