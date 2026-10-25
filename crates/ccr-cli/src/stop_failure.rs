//! `hook stop-failure` (spec section 21): a turn stopped at a usage limit.
//!
//! Claude Code runs the hook in the background (`asyncRewake`). It asks for a swap, then waits
//! until the profile holds an account with room and wakes the session: exit code 2, with a
//! message for it on stderr. The session's next turn reads the new login.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::Result;
use ccr_core::process::Ancestors;
use ccr_core::scheduler::Reason;
use ccr_core::store::{Account, Store, WaitingSession, WaitingState, kind};
use ccr_core::swap::SwapRequest;
use ccr_core::waiting::{self, Decision, WakeReason};
use ccr_core::{Paths, time};
use serde_json::{Value, json};

use crate::process;

/// Exit code that makes Claude Code wake the session with the hook's stderr.
const WAKE: u8 = 2;

pub fn run(profile: Option<PathBuf>, input: &str) -> Result<ExitCode> {
    let payload: Value = serde_json::from_str(input).unwrap_or_default();
    if payload.get("error").and_then(Value::as_str) != Some("rate_limit") {
        return Ok(ExitCode::SUCCESS);
    }
    // The shell Claude Code runs the hook in, if any, and Claude Code: once the session is
    // closed, the hook stops waiting (a shell waits for it, so nothing else would stop it).
    let session_alive = Ancestors::of_hook();
    let (store, paths) = ccr_core::open(profile.clone())?;
    let active = store.active_account()?;
    let request_swap = || -> Result<()> {
        match &active {
            // In the background, like the status line's: a swap may check accounts first.
            Some(a) => process::spawn_swap(
                &SwapRequest {
                    from: Some(a.id),
                    to: None,
                    reason: Reason::StopFailure,
                    wait: false,
                },
                profile.as_deref(),
            ),
            None => Ok(()),
        }
    };
    let Some(session) = payload
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 128)
    else {
        // No session to wake: only the swap.
        request_swap()?;
        return Ok(ExitCode::SUCCESS);
    };

    let now = time::now_ms();
    let recorded = store.immediate(|s| {
        let ws = s.start_waiting(
            session,
            now,
            active.as_ref().map(|a| a.id),
            Some(waiting::this_hook()),
        )?;
        s.log_event(
            kind::SESSION,
            active.as_ref().map(|a| a.id),
            None,
            Some("waiting"),
            Some(json!({ "session": session, "label": active.as_ref().map(|a| &a.label) })),
        )?;
        Ok(ws)
    });
    let mut ws = match recorded {
        Ok(ws) => ws,
        Err(e) => {
            // Without a record there is nothing to wait on, but the swap still helps the other
            // sessions.
            process::log_error("hook stop-failure", &e);
            request_swap()?;
            return Ok(ExitCode::SUCCESS);
        }
    };

    // A swap that landed just before the stop already moved the profile: the session wakes at
    // once, with no new swap. Otherwise the swap waits for the reading of the stop first, then
    // goes ahead unless the session can be woken by then.
    let mut decision = decide(&store, &paths, &ws, now);
    if matches!(decision, Decision::Wait) {
        if let Some(a) = &active {
            wait_for_reading(&store, a.id, now);
        }
        decision = decide(&store, &paths, &ws, time::now_ms());
        if !matches!(decision, Decision::Wake(..)) {
            request_swap()?;
        }
    }
    loop {
        match decision {
            Decision::Wake(account, reason) => return wake(&store, &ws, &account, reason),
            Decision::TimedOut => return stop_waiting(&store, &ws),
            Decision::Wait => {}
        }
        std::thread::sleep(Duration::from_millis(waiting::POLL_MS));
        if !session_alive.all_alive() {
            return stop_waiting(&store, &ws);
        }
        match still_ours(&store, &ws) {
            Ok(Some(current)) => ws = current,
            // A later stop of the session took over, the session is working again, or another
            // process settled this stop.
            Ok(None) => return Ok(ExitCode::SUCCESS),
            Err(e) => {
                // A busy store is no reason to stop waiting: look again next time.
                process::log_error("hook stop-failure", &e);
                continue;
            }
        }
        decision = decide(&store, &paths, &ws, time::now_ms());
    }
}

/// `waiting::decide`, with a store error as a reason to look again later.
fn decide(store: &Store, paths: &Paths, ws: &WaitingSession, now_ms: i64) -> Decision {
    waiting::decide(store, paths, ws, now_ms).unwrap_or_else(|e| {
        process::log_error("hook stop-failure", &e);
        Decision::Wait
    })
}

/// Wait, at most `waiting::READING_WAIT_MS`, for the status line to record a reading of the
/// account taken since the stop.
fn wait_for_reading(store: &Store, account_id: i64, since_ms: i64) {
    let deadline = Instant::now() + Duration::from_millis(waiting::READING_WAIT_MS);
    while Instant::now() < deadline {
        match store.last_sample(account_id) {
            Ok(Some(s)) if s.ts >= since_ms => return,
            _ => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// The stop this hook recorded, while it is still the session's latest and still waiting.
fn still_ours(store: &Store, ws: &WaitingSession) -> Result<Option<WaitingSession>> {
    Ok(store.waiting_session(&ws.session_id)?.filter(|current| {
        current.since_ms == ws.since_ms
            && current.hook_pid == ws.hook_pid
            && current.state == WaitingState::Waiting
    }))
}

fn stop_waiting(store: &Store, ws: &WaitingSession) -> Result<ExitCode> {
    store.set_waiting_state(
        &ws.session_id,
        ws.since_ms,
        &[WaitingState::Waiting],
        WaitingState::Ended,
        None,
        time::now_ms(),
    )?;
    Ok(ExitCode::SUCCESS)
}

fn wake(
    store: &Store,
    ws: &WaitingSession,
    account: &Account,
    reason: WakeReason,
) -> Result<ExitCode> {
    if !waiting::note_woken(store, ws, account, time::now_ms())? {
        return Ok(ExitCode::SUCCESS);
    }
    eprintln!("{}", waiting::wake_message(account, reason));
    Ok(ExitCode::from(WAKE))
}
