//! Tray icon: shows the active account in its tooltip and offers Open, Swap now and Quit.

use ccr_core::scheduler::Reason;
use ccr_core::swap::{self, SwapRequest};
use tauri::AppHandle;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

pub const TRAY_ID: &str = "main";

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open cc-rotate", true, None::<&str>)?;
    let swap_now = MenuItem::with_id(app, "swap", "Swap now", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit cc-rotate", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &swap_now,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("cc-rotate")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => crate::show_main(app),
            "swap" => {
                std::thread::spawn(|| {
                    let _ = crate::commands::with_store(|s, p| {
                        swap::run(
                            s,
                            p,
                            &SwapRequest {
                                from: None,
                                to: None,
                                reason: Reason::Manual,
                                wait: true,
                            },
                        )
                    });
                });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                crate::show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

pub fn set_tooltip(app: &AppHandle, text: &str) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(text));
    }
}
