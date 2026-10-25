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

/// How long the hook waits when it cannot tell whether its session is still open (it found
/// none of its parent processes).
const UNWATCHED_MAX_WAIT_MS: i64 = 6 * 3600 * 1000;

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
        // The account whose limit stopped the turn: the one a swap just left, when the turn's
        // request still carried its login.
        let stopped_on = match &active {
            Some(a) => Some(waiting::stopped_on(s, a.id, now)?),
            None => None,
        };
        let ws = s.start_waiting(session, now, stopped_on, Some(waiting::this_hook()))?;
        let label = match stopped_on {
            Some(id) => s.account(id)?.map(|a| a.label),
            None => None,
        };
        s.log_event(
            kind::SESSION,
            stopped_on,
            None,
            Some("waiting"),
            Some(json!({ "session": session, "label": label })),
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
    let mut swap_requested = false;
    if matches!(decision, Decision::Wait) {
        if let Some(a) = &active {
            wait_for_stop_reading(&store, a, session, now);
        }
        decision = decide(&store, &paths, &ws, time::now_ms());
        if !matches!(decision, Decision::Wake(..)) {
            request_swap()?;
            swap_requested = true;
        }
    }
    loop {
        match &decision {
            Decision::Wake(account, reason) => match wake(&store, &ws, account, *reason) {
                Ok(code) => return Ok(code),
                // Not recorded, so not woken: try again next time.
                Err(e) => process::log_error("hook stop-failure", &e),
            },
            Decision::TimedOut => return stop_waiting(&store, &ws),
            // Waiting after all (a first wake could not be recorded): the swap is still due.
            Decision::Wait if !swap_requested => {
                request_swap()?;
                swap_requested = true;
            }
            Decision::Wait => {}
        }
        std::thread::sleep(Duration::from_millis(waiting::POLL_MS));
        // Without the processes to watch, a closed session cannot be told apart: wait less.
        let unwatched_too_long =
            session_alive.is_empty() && time::now_ms() - ws.since_ms >= UNWATCHED_MAX_WAIT_MS;
        if !session_alive.all_alive() || unwatched_too_long {
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

/// Wait, at most `waiting::READING_WAIT_MS`, for the status line to record the reading of the
/// stop (`waiting::stop_reading`). Other sessions' idle readings do not count.
fn wait_for_stop_reading(store: &Store, account: &Account, session: &str, since_ms: i64) {
    let deadline = Instant::now() + Duration::from_millis(waiting::READING_WAIT_MS);
    while Instant::now() < deadline {
        match waiting::stop_reading(store, account, session, since_ms) {
            Ok(Some(_)) => return,
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

/// Wake the session, unless another process settled the stop first (the session is working
/// again, or a later stop replaced it).
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
