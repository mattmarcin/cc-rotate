//! `hook stop-failure` (spec section 21): a turn stopped at a usage limit.
//!
//! Claude Code runs the hook in the background (`asyncRewake`). It asks for a swap, then waits
//! until the profile holds an account with room and wakes the session: exit code 2, with a
//! message for it on stderr. The session's next turn reads the new login.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use ccr_core::scheduler::Reason;
use ccr_core::store::{Account, Store, WaitingSession, WaitingState, kind};
use ccr_core::swap::SwapRequest;
use ccr_core::time;
use ccr_core::waiting::{self, Decision};
use serde_json::{Value, json};

use crate::process;

/// Exit code that makes Claude Code wake the session with the hook's stderr.
const WAKE: u8 = 2;

pub fn run(profile: Option<PathBuf>, input: &str) -> Result<ExitCode> {
    let payload: Value = serde_json::from_str(input).unwrap_or_default();
    if payload.get("error").and_then(Value::as_str) != Some("rate_limit") {
        return Ok(ExitCode::SUCCESS);
    }
    let (store, paths) = ccr_core::open(profile.clone())?;
    let active = store.active_account()?;
    let request_swap = |active: &Option<Account>| -> Result<()> {
        match active {
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
        request_swap(&active)?;
        return Ok(ExitCode::SUCCESS);
    };

    // The shell Claude Code runs the hook in, and Claude Code: once the session is closed, the
    // hook stops waiting (the shell waits for it, so nothing else would stop it).
    let session_alive = ccr_core::process::Ancestors::of_this_process(2);
    let now = time::now_ms();
    let mut ws = store.immediate(|s| {
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
    })?;

    // A swap that landed just before the stop already moved the profile: no new swap, and the
    // session wakes at once. Otherwise the swap waits for the reading of the stop first.
    let mut decision = waiting::decide(&store, &paths, &ws, now)?;
    if !matches!(decision, Decision::Wake(_)) {
        if let Some(a) = &active {
            let deadline =
                std::time::Instant::now() + Duration::from_millis(waiting::READING_WAIT_MS);
            while std::time::Instant::now() < deadline
                && store.last_sample(a.id)?.is_none_or(|s| s.ts < now)
            {
                std::thread::sleep(Duration::from_millis(250));
            }
        }
        request_swap(&active)?;
    }
    loop {
        match decision {
            Decision::Wake(account) => return wake(&store, &ws, &account),
            Decision::GiveUp(_) => {
                store.set_waiting_state(
                    &ws.session_id,
                    ws.since_ms,
                    WaitingState::Ended,
                    None,
                    time::now_ms(),
                )?;
                return Ok(ExitCode::SUCCESS);
            }
            Decision::Wait => {}
        }
        std::thread::sleep(Duration::from_millis(waiting::POLL_MS));
        if !session_alive.all_alive() {
            store.set_waiting_state(
                &ws.session_id,
                ws.since_ms,
                WaitingState::Ended,
                None,
                time::now_ms(),
            )?;
            return Ok(ExitCode::SUCCESS);
        }
        match still_ours(&store, &ws) {
            Ok(Some(current)) => ws = current,
            // A later stop of the session took over, or another process settled this one.
            Ok(None) => return Ok(ExitCode::SUCCESS),
            Err(e) => {
                // A busy store is no reason to stop waiting: look again next time.
                process::log_error("hook stop-failure", &e);
                continue;
            }
        }
        decision = match waiting::decide(&store, &paths, &ws, time::now_ms()) {
            Ok(d) => d,
            Err(e) => {
                process::log_error("hook stop-failure", &e);
                Decision::Wait
            }
        };
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

fn wake(store: &Store, ws: &WaitingSession, account: &Account) -> Result<ExitCode> {
    if !waiting::note_woken(store, ws, account, time::now_ms())? {
        return Ok(ExitCode::SUCCESS);
    }
    eprintln!("{}", waiting::wake_message(account));
    Ok(ExitCode::from(WAKE))
}
