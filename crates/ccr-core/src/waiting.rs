//! Sessions stopped at a usage limit (spec section 21).
//!
//! Claude Code reads the login when it builds a request, so a turn that stopped at a limit is
//! not helped by a swap: the session sits until its own account resets. The `StopFailure` hook
//! runs in the background (`asyncRewake`), records the session here, and waits until the
//! profile holds an account with room; exiting with code 2 then wakes the session, whose new
//! turn reads the new login. The status line marks a session resumed once it reports usage with
//! room again, whether it was woken or continued by itself, and `scan` tells the user about a
//! woken one that does not.

use anyhow::Result;
use serde_json::{Value, json};

use crate::paths::Paths;
use crate::process;
use crate::scheduler::{self, Thresholds};
use crate::store::{Account, Sample, Store, WaitingSession, WaitingState, kind};
use crate::swap::{self, META_PROFILE_MODEL};

/// A swap that landed this soon before a turn stopped may not have reached that turn's request:
/// the stop may be the previous account's.
pub const LANDED_GRACE_MS: i64 = 2 * 60_000;

/// A woken session that has not reported usage for this long did not continue.
pub const STUCK_AFTER_MS: i64 = 3 * 60_000;

/// How often the hook looks at the store while it waits.
pub const POLL_MS: u64 = 3_000;

/// How long the hook waits for the status line to record the reading of the stop before it asks
/// for a swap. Recorded after the swap, that reading would be taken for a stale one of the account
/// left and dropped, and the account would look as if it still had room.
pub const READING_WAIT_MS: u64 = 5_000;

/// The hook's `timeout` in the profile's settings, in seconds. Claude Code stops an
/// `asyncRewake` hook at its timeout, so the hook gives up a minute before (`MAX_WAIT_MS`).
pub const HOOK_TIMEOUT_S: u64 = 24 * 3600;
pub const MAX_WAIT_MS: i64 = (HOOK_TIMEOUT_S as i64 - 60) * 1000;

/// What the hook does next.
#[derive(Debug, Clone)]
pub enum Decision {
    Wait,
    /// Wake the session: it continues on this account.
    Wake(Box<Account>, WakeReason),
    /// Waited `MAX_WAIT_MS`: stop without waking the session.
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeReason {
    /// The profile holds another account than the one whose limit stopped the turn.
    Switched,
    /// The limit that stopped the turn has reset, and the user turned off Claude Code's own
    /// continue at that point (`autoContinueAtUsageLimit`).
    LimitReset,
}

/// Whether to wake `ws` now. A session is woken on an account with room other than the one whose
/// limit stopped its turn (`stopped_on`). It is never woken twice on one account in an episode
/// of stops, so it cannot be woken in a loop, even while readings wrongly show room on several
/// accounts. Once the limit that stopped it resets, Claude Code continues the session itself,
/// unless the user turned that off: then cc-rotate wakes it.
pub fn decide(store: &Store, paths: &Paths, ws: &WaitingSession, now_ms: i64) -> Result<Decision> {
    if now_ms - ws.since_ms >= MAX_WAIT_MS {
        return Ok(Decision::TimedOut);
    }
    let Some(active) = store.active_account()? else {
        return Ok(Decision::Wait);
    };
    if ws.woken_accounts.contains(&active.id) {
        return Ok(Decision::Wait);
    }
    // Moved on: another account is in the profile, or a swap since the stop put this one back,
    // which it only does while the account has room.
    if ws.account_id != Some(active.id) || swap::active_since(store)? > ws.since_ms {
        let (pool, _) = swap::load_pool(store, now_ms / 1000)?;
        let has_room = pool
            .iter()
            .any(|c| c.account.id == active.id && c.blocking().is_none());
        return Ok(if has_room {
            Decision::Wake(Box::new(active), WakeReason::Switched)
        } else {
            Decision::Wait
        });
    }
    // Still on the account that stopped the turn. Only the stop's own reading says which limit
    // it was and when it resets: other sessions' readings may predate the limit.
    if auto_continue_off(paths) && limit_reset(store, ws, &active, now_ms)? {
        return Ok(Decision::Wake(Box::new(active), WakeReason::LimitReset));
    }
    Ok(Decision::Wait)
}

/// The account whose limit stopped a turn when `active_id` was in the profile: the account a swap
/// left at most `LANDED_GRACE_MS` before, when one landed on `active_id` then (the turn's request
/// still carried that account's login), else `active_id`. A first activation, from no account,
/// does not count.
pub fn stopped_on(store: &Store, active_id: i64, since_ms: i64) -> Result<i64> {
    let left = store
        .last_event_of(kind::SWAP)?
        .filter(|e| {
            e.account_to == Some(active_id)
                && e.ts >= since_ms - LANDED_GRACE_MS
                && e.ts <= since_ms
        })
        .and_then(|e| e.account_from);
    // Events outlive accounts: one removed since cannot be the stop's.
    Ok(match left {
        Some(id) if store.account(id)?.is_some() => id,
        _ => active_id,
    })
}

/// How far around a stop its reading is looked for: the status line may record it just before
/// the hook starts, and moments after.
const STOP_READING_BEFORE_MS: i64 = 30_000;
const STOP_READING_AFTER_MS: i64 = 120_000;

/// The reading of a stop: the first reading of `account` at a limit taken around `since_ms`,
/// the session's own when it has one there (another session's may be at another limit). Idle
/// sessions repeat earlier numbers with a fresh time, so a reading with room says nothing about
/// the stop.
pub fn stop_reading(
    store: &Store,
    account: &Account,
    session_id: &str,
    since_ms: i64,
) -> Result<Option<Sample>> {
    let limit = Limit::of(store, account)?;
    let at_limit: Vec<Sample> = store
        .account_samples_between(
            account.id,
            since_ms - STOP_READING_BEFORE_MS,
            since_ms + STOP_READING_AFTER_MS,
        )?
        .into_iter()
        .filter(|s| limit.blocking(s, s.ts / 1000))
        .collect();
    let own = at_limit
        .iter()
        .position(|s| s.session_id.as_deref() == Some(session_id));
    Ok(match own {
        Some(i) => at_limit.into_iter().nth(i),
        None => at_limit.into_iter().next(),
    })
}

/// The limit that stopped the session has reset: the stop's reading was at a limit, and that
/// limit's reset time has passed.
fn limit_reset(store: &Store, ws: &WaitingSession, account: &Account, now_ms: i64) -> Result<bool> {
    let Some(sample) = stop_reading(store, account, &ws.session_id, ws.since_ms)? else {
        return Ok(false);
    };
    Ok(!Limit::of(store, account)?.blocking(&sample, now_ms / 1000))
}

/// What counts as at a limit for an account: its thresholds, for the model in use.
struct Limit {
    thresholds: Thresholds,
    model: Option<String>,
}

impl Limit {
    fn of(store: &Store, account: &Account) -> Result<Limit> {
        Ok(Limit {
            thresholds: Thresholds::from(&store.settings()?).for_account(account),
            model: store.meta(META_PROFILE_MODEL)?,
        })
    }

    /// Whether `sample` has a window at its threshold at `at_s` (a window counts as 0 once its
    /// reset time has passed).
    fn blocking(&self, sample: &Sample, at_s: i64) -> bool {
        scheduler::window_usage(Some(sample), at_s, self.thresholds, self.model.as_deref())
            .iter()
            .any(scheduler::WindowUsage::blocking)
    }
}

/// Whether the user turned off Claude Code's own continue at a limit's reset for the profile
/// (`autoContinueAtUsageLimit`, read from user settings), so cc-rotate wakes the session then.
fn auto_continue_off(paths: &Paths) -> bool {
    std::fs::read_to_string(paths.settings_file())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|s| s.get("autoContinueAtUsageLimit").and_then(Value::as_bool))
        == Some(false)
}

/// What the woken session reads, as a system reminder. It mirrors Claude Code's own message at a
/// reset, which still arrives later and must not make the session redo its work.
pub fn wake_message(account: &Account, reason: WakeReason) -> String {
    let what = match reason {
        WakeReason::Switched => format!(
            "cc-rotate switched this Claude Code profile to the account {}, which has usage left.",
            account.label
        ),
        WakeReason::LimitReset => format!(
            "The usage limit of the account {} has reset, so it has usage again.",
            account.label
        ),
    };
    format!(
        "{what} Continue the task you were working on when the limit was reached; do not repeat \
         work that is already complete."
    )
}

/// Record the wake of the stop recorded at `ws.since_ms`. Returns false when a later stop
/// replaced it, or another process settled it, and the hook must not wake the session.
pub fn note_woken(
    store: &Store,
    ws: &WaitingSession,
    account: &Account,
    now_ms: i64,
) -> Result<bool> {
    store.immediate(|s| {
        if !s.set_waiting_state(
            &ws.session_id,
            ws.since_ms,
            &[WaitingState::Waiting],
            WaitingState::Woken,
            Some((now_ms, account.id)),
            now_ms,
        )? {
            return Ok(false);
        }
        s.log_event(
            kind::SESSION,
            ws.account_id,
            Some(account.id),
            Some("woken"),
            Some(json!({ "session": ws.session_id, "label": account.label })),
        )?;
        Ok(true)
    })
}

/// A status line reading of `session_id`, attributed to the account in use: when it shows room,
/// a session stopped at a limit is working again, whether it was woken or continued by itself
/// (Claude Code's own continue, or the user). A refused request's reading is at the limit, so it
/// never counts.
pub fn note_reading(
    store: &Store,
    session_id: &str,
    account_id: i64,
    sample: &Sample,
    now_ms: i64,
) -> Result<()> {
    let Some(ws) = store.waiting_session(session_id)? else {
        return Ok(());
    };
    let after = match ws.state {
        WaitingState::Waiting => ws.since_ms,
        WaitingState::Woken | WaitingState::Stuck => match ws.woken_at {
            Some(t) => t,
            None => return Ok(()),
        },
        WaitingState::Resumed | WaitingState::Ended => return Ok(()),
    };
    if now_ms <= after {
        return Ok(());
    }
    let Some(account) = store.account(account_id)? else {
        return Ok(());
    };
    // Room now, and already at the stop (or the wake): a session idling at a limit repeats its
    // reading, which shows room once the limit's reset time passes, but not before it. A reading
    // with a window that had already reset by then is such a repeat too: a fresh response only
    // reports windows still running.
    let limit = Limit::of(store, &account)?;
    let repeat = sample
        .windows
        .iter()
        .any(|w| w.resets_at.is_some_and(|r| r <= after / 1000));
    if repeat || limit.blocking(sample, after / 1000) || limit.blocking(sample, now_ms / 1000) {
        return Ok(());
    }
    store.immediate(|s| {
        if s.set_waiting_state(
            &ws.session_id,
            ws.since_ms,
            &[ws.state],
            WaitingState::Resumed,
            None,
            now_ms,
        )? {
            s.log_event(
                kind::SESSION,
                None,
                Some(account_id),
                Some("resumed"),
                Some(json!({
                    "session": ws.session_id,
                    "label": account.label,
                    "woken": ws.state != WaitingState::Waiting,
                })),
            )?;
        }
        Ok(())
    })
}

/// Settle the sessions the hooks left: one woken that has not reported usage since
/// (`STUCK_AFTER_MS`) is stuck, and the user is told once; one still waiting whose hook is gone
/// (the session ended, or Claude Code stopped the hook) is over. Each change applies only from
/// the state read, so concurrent scans, hooks and status lines never undo each other.
pub fn scan(store: &Store, now_ms: i64) -> Result<()> {
    for ws in store.open_waiting_sessions()? {
        match ws.state {
            WaitingState::Waiting if !hook_alive(&ws) => {
                store.set_waiting_state(
                    &ws.session_id,
                    ws.since_ms,
                    &[WaitingState::Waiting],
                    WaitingState::Ended,
                    None,
                    now_ms,
                )?;
            }
            WaitingState::Woken if ws.woken_at.is_some_and(|t| now_ms - t >= STUCK_AFTER_MS) => {
                store.immediate(|s| {
                    if s.set_waiting_state(
                        &ws.session_id,
                        ws.since_ms,
                        &[WaitingState::Woken],
                        WaitingState::Stuck,
                        None,
                        now_ms,
                    )? {
                        let label = match ws.woken_account_id {
                            Some(id) => s.account(id)?.map(|a| a.label),
                            None => None,
                        };
                        s.log_event(
                            kind::SESSION_STUCK,
                            ws.account_id,
                            ws.woken_account_id,
                            None,
                            Some(json!({
                                "session": ws.session_id,
                                "label": label,
                                "woken_at": ws.woken_at,
                                "claude_version": s.meta(crate::health::META_CLAUDE_VERSION)?,
                            })),
                        )?;
                    }
                    Ok(())
                })?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// The hook recorded with the stop is still running (the same process, not a reused id).
fn hook_alive(ws: &WaitingSession) -> bool {
    let Some(pid) = ws.hook_pid else {
        return false;
    };
    process::alive(pid)
        && match &ws.hook_started {
            Some(started) => process::start_time(pid).as_ref() == Some(started),
            None => true,
        }
}

/// The hook's own process, as `WaitingSession::hook_pid` and `hook_started` record it.
pub fn this_hook() -> (u32, Option<String>) {
    let pid = std::process::id();
    (pid, process::start_time(pid))
}
