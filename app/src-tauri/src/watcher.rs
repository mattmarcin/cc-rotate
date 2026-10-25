//! Polls the store once a second. When another process commits (a statusline tick, a swap,
//! a CLI command) it tells the UI to refresh and turns new attention-worthy events into native
//! notifications. Every 5 seconds it runs the background sync (write-back, dead logins,
//! switching back to a better ranked account), and every 30 seconds it refreshes the tray
//! tooltip and checks whether an exhausted pool has an account ready again, since both move
//! with the clock even when nothing is written. Every minute it starts a usage check of the
//! account that is most overdue for one (`usage_check::next_due`), one at a time, so the
//! dashboard knows how much every account has left.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ccr_core::snapshot::Snapshot;
use ccr_core::store::{Event, Store, kind};
use ccr_core::usage_check::{self, Trigger};
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
        if tick % SYNC_EVERY_S == 0
            && !watch.swapping.load(Ordering::Acquire)
            && statusline::needs_settling(store, paths, time::now_ms()).unwrap_or(false)
        {
            // The profile's login has to be identified: that runs Claude Code, so off this thread.
            watch.swapping.store(true, Ordering::Release);
            let swapping = watch.swapping.clone();
            std::thread::spawn(move || {
                if let Ok((store, paths)) = ccr_core::open(None) {
                    if let Ok(_lock) = ccr_core::lock::DirLock::acquire(
                        &paths.swap_lock(),
                        ccr_core::lock::LockOptions::claude(Duration::ZERO),
                    ) {
                        let _ = swap::settle_profile(&store, &paths);
                    }
                }
                swapping.store(false, Ordering::Release);
            });
        }
        if tick % SYNC_EVERY_S == 0 && !watch.swapping.load(Ordering::Acquire) {
            if let Ok(Some(req)) = statusline::sync(store, paths) {
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
        }
        if tick % CHECK_EVERY_S == 0 {
            start_due_check(store, paths, &watch.checking);
        }
        let Ok(version) = store.data_version() else {
            return;
        };
        if watch.last_version != Some(version) {
            watch.last_version = Some(version);
            refresh(app, store, paths, watch);
        } else if tick % STATUS_EVERY_S == 0 {
            status(app, store, paths, watch);
        }
        tick += 1;
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Check the most overdue account in the background, unless a check is still running. Its
/// results arrive like any other write, through `data_version`.
fn start_due_check(store: &Store, paths: &Paths, checking: &Arc<AtomicBool>) {
    if checking.swap(true, Ordering::AcqRel) {
        return;
    }
    let Ok(Some(id)) = usage_check::next_due(store, paths, time::now_ms()) else {
        checking.store(false, Ordering::Release);
        return;
    };
    let checking = checking.clone();
    std::thread::spawn(move || {
        if let Ok((store, paths)) = ccr_core::open(None) {
            let _ = usage_check::check(&store, &paths, id, Trigger::Background);
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
                &format!("Every account was near a limit. cc-rotate can use {label} now."),
            );
        }
    }
    watch.exhausted = Some(exhausted && !snap.accounts.is_empty());
}

fn tooltip(snap: &Snapshot) -> String {
    let Some(a) = snap
        .accounts
        .iter()
        .find(|a| Some(a.account.id) == snap.active_id)
    else {
        return if snap.accounts.is_empty() {
            "cc-rotate: no accounts yet".to_string()
        } else {
            "cc-rotate: no active account".to_string()
        };
    };
    let five = a
        .windows
        .iter()
        .find(|w| w.name == ccr_core::scheduler::FIVE_HOUR)
        .map_or("no reading".to_string(), |w| format!("{:.0}%", w.pct));
    let week = match (
        a.windows
            .iter()
            .find(|w| w.name == ccr_core::scheduler::SEVEN_DAY),
        a.weekly.effective,
    ) {
        (Some(w), _) => format!("week {:.0}%", w.pct),
        (None, ccr_core::scheduler::WeeklyLimit::None) => "no weekly limit".to_string(),
        (None, _) => "week no reading".to_string(),
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
        kind::DEAD_LOGIN => Some((
            format!("{} needs a new login", label(event.account_from)),
            "Its login stopped working. Open cc-rotate and choose Log in again.".into(),
        )),
        kind::FOREIGN_LOGIN => Some((
            "cc-rotate profile signed in to another account".into(),
            "A /login in the cc-rotate profile signed in to an account that isn't in cc-rotate. Add that account, or use Swap now to go back to one of yours.".into(),
        )),
        kind::ERROR => Some((
            "cc-rotate hit an error".into(),
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
