//! Polls the store once a second. When another process commits (a statusline tick, a swap,
//! a CLI command) it tells the UI to refresh, updates the tray tooltip, and turns new
//! attention-worthy events into native notifications.

use std::time::Duration;

use ccr_core::store::{Event, Store, kind};
use ccr_core::{Paths, snapshot, statusline, swap};
use tauri::{AppHandle, Emitter};
use tauri_plugin_notification::NotificationExt;

pub const CHANGED: &str = "store-changed";

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last_version = None;
        let mut last_event_id = None;
        loop {
            if let Ok((store, paths)) = ccr_core::open(None) {
                run(&app, &store, &paths, &mut last_version, &mut last_event_id);
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
}

fn run(
    app: &AppHandle,
    store: &Store,
    paths: &Paths,
    last_version: &mut Option<i64>,
    last_event_id: &mut Option<i64>,
) {
    // `data_version` only reflects other connections' commits, so keep this connection open.
    let mut tick = 0u64;
    loop {
        // Keep the store's copy of the active login current even when no interactive session
        // is running a statusline (for example only `claude -p` jobs).
        if tick % 5 == 0 {
            if let Ok(Some(req)) = statusline::sync(store, paths) {
                let _ = swap::run(store, paths, &req);
                // Our own writes do not move `data_version` for this connection.
                *last_version = None;
            }
        }
        tick += 1;
        let Ok(version) = store.data_version() else {
            return;
        };
        if *last_version != Some(version) {
            *last_version = Some(version);
            refresh(app, store, paths, last_event_id);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn refresh(app: &AppHandle, store: &Store, paths: &Paths, last_event_id: &mut Option<i64>) {
    let _ = app.emit(CHANGED, ());

    if let Ok(snap) = snapshot::snapshot(store, paths) {
        let tooltip = match snap
            .accounts
            .iter()
            .find(|a| Some(a.account.id) == snap.active_id)
        {
            Some(a) => format!(
                "cc-rotate: {}\n5-hour {:.0}%, week {:.0}%",
                a.account.label, a.usage.five_hour, a.usage.weekly
            ),
            None if snap.accounts.is_empty() => "cc-rotate: no accounts yet".to_string(),
            None => "cc-rotate: no active account".to_string(),
        };
        crate::tray::set_tooltip(app, &tooltip);
    }

    match *last_event_id {
        None => {
            *last_event_id = Some(
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
                *last_event_id = Some(last.id);
            }
            for event in events {
                if let Some((title, body)) = describe(store, &event) {
                    let _ = app.notification().builder().title(title).body(body).show();
                }
            }
        }
    }
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
            format!("{} is near its limit and every other account is paused, needs a login, or is used up.", label(event.account_from)),
        )),
        kind::DEAD_LOGIN => Some((
            format!("{} needs a new login", label(event.account_from)),
            "Its login stopped working. Open cc-rotate and choose Log in again.".into(),
        )),
        kind::FOREIGN_LOGIN => Some((
            "The rotation profile is signed in to an unknown account".into(),
            "Someone ran /login in the cc-rotate profile with an account outside the pool. Add it in cc-rotate or swap back.".into(),
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
