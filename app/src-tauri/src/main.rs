#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod login_flow;
mod tray;
mod watcher;

use tauri::{AppHandle, Manager, RunEvent, WindowEvent};
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
            commands::costs,
            commands::swaps,
            commands::events,
            commands::usage_history,
            commands::swap_now,
            commands::check_usage,
            commands::set_paused,
            commands::remove_account,
            commands::rename_account,
            commands::set_order,
            commands::set_weekly_limit,
            commands::set_weekly_cap,
            commands::set_use_expiring,
            commands::set_keep_running,
            commands::doctor,
            commands::other_profiles,
            commands::set_setting,
            commands::app_info,
            commands::autostart_enabled,
            commands::set_autostart,
            login_flow::open_login_page,
            login_flow::start_login,
            login_flow::submit_login_code,
            login_flow::cancel_login,
        ])
        .setup(|app| {
            tray::create(app.handle())?;
            watcher::start(app.handle().clone());
            sync_autostart(app.handle());
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
        .build(tauri::generate_context!())
        .expect("error while building cc-rotate")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                login_flow::stop_all(app);
            }
        });
}

/// The app is the only source of notifications, so it starts at login unless the user turned
/// that off. The first launch of an installed copy enables it; later launches respect the user's
/// toggle, and while it is on, point it at the copy running now, so it follows the copy an
/// install puts in place. A copy run from a Cargo build folder leaves it alone: login would start
/// a build the next build replaces, and an install could not update it.
fn sync_autostart(app: &AppHandle) {
    let built_here = std::env::current_exe().is_ok_and(|exe| in_build_folder(&exe));
    if cfg!(debug_assertions) || built_here {
        return;
    }
    let Ok((store, _)) = ccr_core::open(None) else {
        return;
    };
    let launcher = app.autolaunch();
    let initialized = store
        .meta(META_AUTOSTART_INITIALIZED)
        .ok()
        .flatten()
        .is_some();
    if !initialized {
        let _ = launcher.enable();
        let _ = store.set_meta(META_AUTOSTART_INITIALIZED, "1");
    } else if launcher.is_enabled().unwrap_or(false) {
        // Written again with this copy's path.
        let _ = launcher.enable();
    }
}

/// Whether `exe` is a Cargo build's output (`target/<profile>/` or `target/<triple>/<profile>/`),
/// which Cargo marks with a `CACHEDIR.TAG` in `target`, rather than an installed copy.
fn in_build_folder(exe: &std::path::Path) -> bool {
    exe.ancestors()
        .skip(2)
        .take(2)
        .any(|dir| dir.join("CACHEDIR.TAG").is_file())
}

#[cfg(test)]
mod tests {
    use super::in_build_folder;

    #[test]
    fn a_build_folder_is_told_from_an_installed_copy() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        for dir in ["release", "x86_64-pc-windows-msvc/release"] {
            std::fs::create_dir_all(target.join(dir)).unwrap();
        }
        std::fs::write(
            target.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55",
        )
        .unwrap();
        let bin = root.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        assert!(in_build_folder(&target.join("release/cc-rotate-app")));
        assert!(in_build_folder(
            &target.join("x86_64-pc-windows-msvc/release/cc-rotate-app")
        ));
        assert!(!in_build_folder(&bin.join("cc-rotate-app")));
        // A copy in the target folder itself is not a build's output.
        assert!(!in_build_folder(&target.join("cc-rotate-app")));
    }
}
