#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod login_flow;
mod tray;
mod watcher;

use tauri::{AppHandle, Manager, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

const MINIMIZED_ARG: &str = "--minimized";
const META_AUTOSTART_INITIALIZED: &str = "app_autostart_initialized";

pub fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main(app)
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec![MINIMIZED_ARG]),
        ))
        .manage(login_flow::Logins::default())
        .invoke_handler(tauri::generate_handler![
            commands::snapshot,
            commands::series,
            commands::events,
            commands::swap_now,
            commands::set_paused,
            commands::remove_account,
            commands::rename_account,
            commands::set_order,
            commands::set_weekly_limit,
            commands::doctor,
            commands::other_profiles,
            commands::set_setting,
            commands::app_info,
            commands::autostart_enabled,
            commands::set_autostart,
            commands::open_url,
            login_flow::start_login,
            login_flow::submit_login_code,
            login_flow::cancel_login,
        ])
        .setup(|app| {
            tray::create(app.handle())?;
            watcher::start(app.handle().clone());
            enable_autostart_once(app.handle());
            if !std::env::args().any(|a| a == MINIMIZED_ARG) {
                show_main(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps the app in the tray so notifications keep working.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running cc-rotate");
}

/// The app is the only source of notifications, so it starts at login unless the user turned
/// that off. Only the very first launch enables it; later launches respect the user's toggle.
fn enable_autostart_once(app: &AppHandle) {
    let Ok((store, _)) = ccr_core::open(None) else {
        return;
    };
    if store
        .meta(META_AUTOSTART_INITIALIZED)
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }
    if !cfg!(debug_assertions) {
        let _ = app.autolaunch().enable();
    }
    let _ = store.set_meta(META_AUTOSTART_INITIALIZED, "1");
}
