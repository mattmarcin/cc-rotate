//! Sessions stopped at a usage limit (spec section 21).
//!
//! Claude Code reads the login when it builds a request, so a turn that stopped at a limit is
//! not helped by a swap: the session sits until its own account resets. The `StopFailure` hook
//! runs in the background (`asyncRewake`), records the session here, and waits until the
//! profile holds an account with room; exiting with code 2 then wakes the session, whose new
//! turn reads the new login. The status line marks a woken session resumed once it reports
//! usage again, and `scan` tells the user about one that does not.

use anyhow::Result;
use serde_json::{Value, json};

use crate::paths::Paths;
use crate::store::{Account, Store, WaitingSession, WaitingState, kind};
use crate::{process, swap};

/// A swap that landed this soon before a turn stopped may not have reached that turn's request,
/// so the account it landed on still counts as new for the session.
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
    Wake(Box<Account>),
    /// Stop waiting without waking the session.
    GiveUp(GiveUp),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GiveUp {
    /// The account that stopped the session has room again and is still in the profile:
    /// Claude Code continues the session itself (`autoContinueAtUsageLimit`).
    LimitReset,
    /// Waited `MAX_WAIT_MS`.
    TimedOut,
}

impl GiveUp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LimitReset => "limit_reset",
            Self::TimedOut => "timed_out",
        }
    }
}

/// Whether to wake `ws` now. A session is woken on an account with room: one the profile moved
/// to since its turn stopped, or the one it stopped on once a reading taken since shows room.
/// It is never woken twice on the same account in one episode of stops, so a session that stops
/// again cannot be woken in a loop, even while readings wrongly show room on several accounts.
pub fn decide(store: &Store, paths: &Paths, ws: &WaitingSession, now_ms: i64) -> Result<Decision> {
    if now_ms - ws.since_ms >= MAX_WAIT_MS {
        return Ok(Decision::GiveUp(GiveUp::TimedOut));
    }
    let Some(active) = store.active_account()? else {
        return Ok(Decision::Wait);
    };
    if ws.woken_accounts.contains(&active.id) {
        return Ok(Decision::Wait);
    }
    let moved = ws.account_id != Some(active.id);
    // Still the account in the profile when the turn stopped: its reading from before the stop
    // may predate the jump to its limit, which the status line reports moments later, so only
    // one taken since tells whether it has room.
    if !moved
        && store
            .last_sample(active.id)?
            .is_none_or(|s| s.ts < ws.since_ms)
    {
        return Ok(Decision::Wait);
    }
    let (pool, _) = swap::load_pool(store, now_ms / 1000)?;
    let has_room = pool
        .iter()
        .any(|c| c.account.id == active.id && c.blocking().is_none());
    if !has_room {
        return Ok(Decision::Wait);
    }
    if moved {
        return Ok(Decision::Wake(Box::new(active)));
    }
    // The account the turn stopped on has room by a reading since. Either a swap landed just
    // before the stop, and the turn's request still carried the previous account's login, or
    // its limit has reset since, and Claude Code continues the session itself unless the user
    // turned that off.
    if swap::active_since(store)? >= ws.since_ms - LANDED_GRACE_MS || auto_continue_off(paths) {
        return Ok(Decision::Wake(Box::new(active)));
    }
    Ok(Decision::GiveUp(GiveUp::LimitReset))
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
pub fn wake_message(account: &Account) -> String {
    format!(
        "cc-rotate switched this Claude Code profile to the account {}, which has usage left. \
         Continue the task you were working on when the limit was reached; do not repeat work \
         that is already complete.",
        account.label
    )
}

/// Record the wake of the stop recorded at `ws.since_ms`. Returns false when a later stop
/// replaced it, and the hook must not wake the session.
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

/// A status line reading of `session_id` on the account in use: a session woken earlier is
/// working again.
pub fn note_reading(store: &Store, session_id: &str, account_id: i64, now_ms: i64) -> Result<()> {
    let Some(ws) = store.waiting_session(session_id)? else {
        return Ok(());
    };
    if !matches!(ws.state, WaitingState::Woken | WaitingState::Stuck)
        || ws.woken_at.is_none_or(|t| now_ms <= t)
    {
        return Ok(());
    }
    store.immediate(|s| {
        if s.set_waiting_state(
            &ws.session_id,
            ws.since_ms,
            WaitingState::Resumed,
            None,
            now_ms,
        )? {
            let label = s.account(account_id)?.map(|a| a.label);
            s.log_event(
                kind::SESSION,
                None,
                Some(account_id),
                Some("resumed"),
                Some(json!({ "session": ws.session_id, "label": label })),
            )?;
        }
        Ok(())
    })
}

/// Settle the sessions the hooks left: one woken that has not reported usage since
/// (`STUCK_AFTER_MS`) is stuck, and the user is told once; one still waiting whose hook is gone
/// (the session ended, or Claude Code stopped the hook) is over.
pub fn scan(store: &Store, now_ms: i64) -> Result<()> {
    for ws in store.open_waiting_sessions()? {
        match ws.state {
            WaitingState::Waiting if !hook_alive(&ws) => {
                store.set_waiting_state(
                    &ws.session_id,
                    ws.since_ms,
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
