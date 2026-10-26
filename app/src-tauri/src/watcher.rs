//! Polls the store once a second. When another process commits (a statusline tick, a swap,
//! a CLI command) it tells the UI to refresh and turns new attention-worthy events into native
//! notifications. Every 5 seconds it runs the background sync (write-back, dead logins,
//! switching back to an account without a weekly limit), and every 30 seconds it refreshes the tray
//! tooltip and checks whether an exhausted pool has an account ready again, since both move
//! with the clock even when nothing is written. Every minute it starts the next background job
//! (`usage_check::next_job`), one at a time: a usage check of the account most overdue for one,
//! so the dashboard knows how much every account has left, or a warm-up that starts the 5-hour
//! window of an account the user keeps running.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ccr_core::snapshot::Snapshot;
use ccr_core::store::{Event, Store, kind};
use ccr_core::usage_check::{self, Due, Trigger};
use ccr_core::{Paths, snapshot, statusline, swap, time};
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;

pub const CHANGED: &str = "store-changed";

const SYNC_EVERY_S: u64 = 5;
const STATUS_EVERY_S: u64 = 30;
const CHECK_EVERY_S: u64 = 60;

#[derive(Default)]
struct Watch {
    last_version: Option<i64>,
    last_event_id: Option<i64>,
    /// Whether the pool was exhausted at the last status check.
    exhausted: Option<bool>,
    /// The "running out" notification went out, and the forecast has not shown room since.
    warned_running_out: bool,
    /// A background usage check is running.
    checking: Arc<AtomicBool>,
    /// A swap the background sync asked for is running.
    swapping: Arc<AtomicBool>,
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        let mut watch = Watch::default();
        loop {
            if let Ok((store, paths)) = ccr_core::open(None) {
                run(&app, &store, &paths, &mut watch);
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

fn run(app: &AppHandle, store: &Store, paths: &Paths, watch: &mut Watch) {
    // Settle what checks that died before the app started left behind.
    let _ = usage_check::sweep(store, paths);
    // `data_version` only reflects other connections' commits, so keep this connection open.
    let mut tick = 0u64;
    loop {
        // Keep the store's copy of the active login current even when no interactive session
        // is running a statusline (for example only `claude -p` jobs).
        if tick.is_multiple_of(SYNC_EVERY_S)
            && !watch.swapping.load(Ordering::Acquire)
            && statusline::needs_settling(store, paths, time::now_ms()).unwrap_or(false)
        {
            // The profile's login has to be identified: that runs Claude Code, so off this thread.
            watch.swapping.store(true, Ordering::Release);
            let swapping = watch.swapping.clone();
            std::thread::spawn(move || {
                if let Ok((store, paths)) = ccr_core::open(None)
                    && let Ok(_lock) = ccr_core::lock::DirLock::acquire(
                        &paths.swap_lock(),
                        ccr_core::lock::LockOptions::claude(Duration::ZERO),
                    )
                {
                    let _ = swap::settle_profile(&store, &paths);
                }
                swapping.store(false, Ordering::Release);
            });
        }
        if tick.is_multiple_of(SYNC_EVERY_S)
            && !watch.swapping.load(Ordering::Acquire)
            && let Ok(Some(req)) = statusline::sync(store, paths)
        {
            // On its own thread: it may check accounts first, which takes seconds.
            watch.swapping.store(true, Ordering::Release);
            let swapping = watch.swapping.clone();
            std::thread::spawn(move || {
                if let Ok((store, paths)) = ccr_core::open(None) {
                    let _ = swap::run(&store, &paths, &req);
                }
                swapping.store(false, Ordering::Release);
            });
            // Our own writes do not move `data_version` for this connection.
            watch.last_version = None;
        }
        // Relearn the rates that decide which weekly budgets would expire (cheap while the
        // learned ones are fresh), so the status line never has to.
        if tick.is_multiple_of(CHECK_EVERY_S) {
            let _ = ccr_core::budget::refresh_rates(store, time::now_ms());
        }
        if tick.is_multiple_of(CHECK_EVERY_S) {
            start_due_check(store, paths, &watch.checking);
        }
        // Sign-ins about to end and sessions that did not continue after a usage limit: each
        // becomes an event once, which the refresh below turns into a notification.
        if tick.is_multiple_of(STATUS_EVERY_S) {
            let _ = ccr_core::health::scan(store, time::now_ms());
        }
        let Ok(version) = store.data_version() else {
            return;
        };
        if watch.last_version != Some(version) {
            watch.last_version = Some(version);
            refresh(app, store, paths, watch);
        } else if tick.is_multiple_of(STATUS_EVERY_S) {
            status(app, store, paths, watch);
        }
        tick += 1;
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Run the next background job (a check or a warm-up), unless one is still running. Its
/// results arrive like any other write, through `data_version`.
fn start_due_check(store: &Store, paths: &Paths, checking: &Arc<AtomicBool>) {
    if checking.swap(true, Ordering::AcqRel) {
        return;
    }
    let Ok(Some(due)) = usage_check::next_job(store, paths, time::now_ms()) else {
        checking.store(false, Ordering::Release);
        return;
    };
    let checking = checking.clone();
    std::thread::spawn(move || {
        if let Ok((store, paths)) = ccr_core::open(None) {
            match due {
                Due::Check(id) => {
                    let _ = usage_check::check(&store, &paths, id, Trigger::Background);
                }
                Due::WarmUp(id) => {
                    let _ = usage_check::warm_up(&store, &paths, id, Trigger::WarmUp);
                }
            }
        }
        checking.store(false, Ordering::Release);
    });
}

fn refresh(app: &AppHandle, store: &Store, paths: &Paths, watch: &mut Watch) {
    let _ = app.emit(CHANGED, ());
    status(app, store, paths, watch);

    match watch.last_event_id {
        None => {
            watch.last_event_id = Some(
                store
                    .events(1)
                    .ok()
                    .and_then(|e| e.first().map(|e| e.id))
                    .unwrap_or(0),
            )
        }
        Some(after) => {
            let events = store.events_after(after).unwrap_or_default();
            if let Some(last) = events.last() {
                watch.last_event_id = Some(last.id);
            }
            for event in events {
                if let Some((title, body)) = describe(store, &event) {
                    notify(app, &title, &body);
                }
            }
        }
    }
}

/// The tray tooltip, and the "ready again" notification when an exhausted pool has an account
/// it can use again.
fn status(app: &AppHandle, store: &Store, paths: &Paths, watch: &mut Watch) {
    let Ok(snap) = snapshot::snapshot(store, paths) else {
        return;
    };
    crate::tray::set_tooltip(app, &tooltip(&snap));

    let exhausted = snap.pool.exhausted;
    if watch.exhausted == Some(true) && !exhausted {
        let ready = snap.next.as_ref().map(|n| n.label.clone()).or_else(|| {
            snap.accounts
                .iter()
                .find(|a| Some(a.account.id) == snap.active_id)
                .map(|a| a.account.label.clone())
        });
        if let Some(label) = ready {
            notify(
                app,
                &format!("{label} is ready again"),
                &format!("Every account had been near a limit. cc-rotate can use {label} now."),
            );
        }
    }
    watch.exhausted = Some(exhausted && !snap.accounts.is_empty());

    // Once, when at the current pace every account would be at a limit within the hour.
    let now_s = snap.now_ms / 1000;
    let soon = snap.forecast.as_ref().and_then(|f| {
        f.runs_out_at
            .filter(|&t| t - now_s <= RUNNING_OUT_WARNING_S)
            .map(|t| (t, f.free_again_at))
    });
    match soon {
        Some((out_at, free)) if !watch.warned_running_out && !exhausted => {
            let mut body = format!(
                "At this pace, every account is at a limit in {}.",
                ccr_core::time::span(out_at - now_s)
            );
            if let Some(free) = free {
                body.push_str(&format!(
                    " The first frees up {} after that.",
                    ccr_core::time::span(free - out_at)
                ));
            }
            notify(app, "Running out of accounts", &body);
            watch.warned_running_out = true;
        }
        // Room again only when the pool would last well past the warning, so a forecast that
        // wavers around the hour does not warn again.
        // No forecast while the pool is out is no sign of room; no forecast otherwise is a break
        // from work, after which a new run-out is worth a new warning.
        None if match &snap.forecast {
            Some(f) => f
                .runs_out_at
                .is_none_or(|t| t - now_s > 2 * RUNNING_OUT_WARNING_S),
            None => !exhausted,
        } =>
        {
            watch.warned_running_out = false
        }
        _ => {}
    }
}

/// How far ahead the "running out" notification warns.
const RUNNING_OUT_WARNING_S: i64 = 3600;

fn tooltip(snap: &Snapshot) -> String {
    let Some(a) = snap
        .accounts
        .iter()
        .find(|a| Some(a.account.id) == snap.active_id)
    else {
        return if snap.accounts.is_empty() {
            "cc-rotate: no accounts yet".to_string()
        } else {
            "cc-rotate: no account in use".to_string()
        };
    };
    let five = a
        .windows
        .iter()
        .find(|w| w.name == ccr_core::scheduler::FIVE_HOUR)
        .map_or("no data".to_string(), |w| format!("{:.0}%", w.pct));
    let week = match (
        a.windows
            .iter()
            .find(|w| w.name == ccr_core::scheduler::SEVEN_DAY),
        a.weekly.effective,
    ) {
        (Some(w), _) => format!("weekly {:.0}%", w.pct),
        (None, ccr_core::scheduler::WeeklyLimit::None) => "no weekly limit".to_string(),
        (None, _) => "weekly no data".to_string(),
    };
    let mut text = format!("cc-rotate: {}\n5-hour {five}, {week}", a.account.label);
    if let Some(next) = &snap.next {
        text.push_str(&format!("\nNext: {}", next.label));
    }
    text
}

fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}

fn describe(store: &Store, event: &Event) -> Option<(String, String)> {
    let label = |id: Option<i64>| {
        id.and_then(|i| store.account(i).ok().flatten())
            .map(|a| a.label)
            .unwrap_or_else(|| "An account".into())
    };
    match event.kind.as_str() {
        kind::POOL_EXHAUSTED => Some((
            "No account left to swap to".into(),
            "Every other account is near a limit, paused or signed out.".into(),
        )),
        kind::DEAD_LOGIN if event.reason.as_deref() == Some("sign_in_ended") => Some((
            format!("{} is signed out", label(event.account_from)),
            "Its sign-in ended. Open cc-rotate and choose Sign in again.".into(),
        )),
        kind::DEAD_LOGIN => Some((
            format!("{} is signed out", label(event.account_from)),
            "Its login stopped working. Open cc-rotate and choose Sign in again.".into(),
        )),
        kind::LOGIN_EXPIRING => {
            let name = label(event.account_from);
            let left = event
                .detail
                .as_ref()
                .and_then(|d| d.get("expires_at"))
                .and_then(|v| v.as_i64())
                .map(|t| format!("in {}", time::span(t / 1000 - time::now_s())))
                .unwrap_or_else(|| "soon".into());
            Some((
                format!("{name}'s sign-in ends {left}"),
                format!("Sign in to {name} again before then to keep it in rotation."),
            ))
        }
        kind::PAID_USAGE => {
            let detail = event.detail.as_ref();
            let minor = |k: &str| detail.and_then(|d| d.get(k)).and_then(|v| v.as_i64());
            let currency = detail
                .and_then(|d| d.get("currency"))
                .and_then(|v| v.as_str());
            // The account in use is rarely read, so the figures say when they are from.
            let as_of = minor("read_at")
                .map(|t| format!("As of {}, it", time::local(t / 1000, "%b %-d, %-I:%M %p")))
                .unwrap_or_else(|| "It".into());
            let spend = match (minor("used"), minor("monthly_limit")) {
                (Some(used), Some(limit)) => format!(
                    " {as_of} had used {} of its {} this month.",
                    ccr_core::extra_usage::money(used, currency),
                    ccr_core::extra_usage::money(limit, currency)
                ),
                (Some(used), None) => format!(
                    " {as_of} had used {} this month.",
                    ccr_core::extra_usage::money(used, currency)
                ),
                _ => String::new(),
            };
            let avoiding = detail
                .and_then(|d| d.get("avoiding"))
                .and_then(|v| v.as_bool())
                == Some(true);
            let could_move = detail
                .and_then(|d| d.get("could_move"))
                .and_then(|v| v.as_bool())
                == Some(true);
            let next = if avoiding && could_move {
                " cc-rotate couldn't move to an account without extra usage. Activity says why."
            } else if avoiding {
                " No account without extra usage is ready to move to."
            } else {
                " To wait for a reset instead, turn that on in Settings."
            };
            Some((
                "Sessions are using paid extra usage".into(),
                format!(
                    "Every account is at a limit, and {} has extra usage on, so its sessions now cost money.{spend}{next}",
                    label(event.account_from)
                ),
            ))
        }
        kind::SESSION_STUCK => {
            let detail = event.detail.as_ref();
            let version = detail
                .and_then(|d| d.get("claude_version"))
                .and_then(|v| v.as_str())
                .filter(|v| !ccr_core::health::is_tested(v));
            let mut body = format!(
                "cc-rotate woke a session that stopped at a usage limit, now on {}, but it hasn't continued. Check it, and type continue if it's still waiting.",
                label(event.account_to)
            );
            if let Some(v) = version {
                body.push_str(&format!(" Claude Code {v} isn't tested with cc-rotate yet."));
            }
            Some(("A session didn't continue".into(), body))
        }
        kind::FOREIGN_LOGIN => Some((
            "cc-rotate profile signed in to another account".into(),
            "A /login in the cc-rotate profile signed in to an account that isn't in cc-rotate. Add that account, or use Swap now to go back to one of yours.".into(),
        )),
        kind::ERROR => Some((
            "cc-rotate ran into a problem".into(),
            event
                .detail
                .as_ref()
                .and_then(|d| d.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("Open cc-rotate to see the details.")
                .to_string(),
        )),
        _ => None,
    }
}
