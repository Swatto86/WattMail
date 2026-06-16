//! WattMail desktop — Tauri presentation layer and composition root.
//!
//! Wires the infrastructure (`AuthService`, Graph) into Tauri commands and owns
//! the window, tray, and settings. No domain logic lives here.

mod commands;
mod paths;
mod settings;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::RwLock;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WindowEvent};

use settings::SettingsState;
use wattmail_infrastructure::{AuthService, OAuthConfig, SqliteStore};

// Public client identifiers — NOT secrets. Safe to ship in the binary.
const CLIENT_ID: &str = "60d6101b-3d8a-4a09-8718-ad90c0d88f13";
const TENANT_ID: &str = "652459b1-612f-4586-b424-a0069d51cc32";

/// CLI flag the autostart entry passes so a login-launched instance stays in the
/// tray instead of showing its window. Manual launches omit it and show normally.
const HIDDEN_FLAG: &str = "--hidden";

/// Whether this process was launched into the tray (autostart). Read by the
/// `started_hidden` command so the frontend skips revealing the window.
pub(crate) struct StartHidden(pub bool);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let auth = AuthService::new(OAuthConfig::office365(TENANT_ID, CLIENT_ID))
        .expect("initialise auth service");
    let store = SqliteStore::open(cache_db_path()).expect("open mail cache");
    let loaded = settings::load();
    let start_hidden = std::env::args().any(|arg| arg == HIDDEN_FLAG);

    tauri::Builder::default()
        // single-instance must be registered first: a second launch focuses the
        // running window instead of opening a duplicate.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main(app);
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        // Autostart registers the app at login with `--hidden`, so it boots into
        // the tray; a manual launch (no flag) shows the window as usual.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![HIDDEN_FLAG]),
        ))
        .manage(auth)
        .manage(store)
        .manage(SettingsState(RwLock::new(loaded)))
        .manage(StartHidden(start_hidden))
        .setup(move |app| {
            build_tray(app.handle())?;
            // Safety net: if the frontend never reveals the window (e.g. a script
            // error), show it anyway — unless we were autostarted into the tray.
            if !start_hidden {
                if let Some(window) = app.get_webview_window("main") {
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(3000));
                        let _ = window.show();
                    });
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let close_to_tray = window
                    .app_handle()
                    .state::<SettingsState>()
                    .0
                    .read()
                    .map(|s| s.close_to_tray)
                    .unwrap_or(true);
                if close_to_tray {
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::is_signed_in,
            commands::sign_in,
            commands::sign_out,
            commands::list_folders,
            commands::folder_from_cache,
            commands::sync_folder,
            commands::load_message,
            commands::prepare_reply,
            commands::prepare_forward,
            commands::send_message,
            commands::attachments,
            commands::save_attachment,
            commands::set_read,
            commands::delete_message,
            commands::move_message,
            commands::get_close_to_tray,
            commands::set_close_to_tray,
            commands::set_unread,
            commands::started_hidden,
        ])
        .run(tauri::generate_context!())
        .expect("error while running WattMail");
}

/// Build the system-tray icon with a Show / Settings / Quit menu.
fn build_tray(app: &AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let show = MenuItem::with_id(app, "show", "Show WattMail", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&show, &settings, &separator, &quit])?;

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or("missing default window icon")?;

    TrayIconBuilder::with_id("main")
        .icon(icon)
        .tooltip("WattMail")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main(app),
            "settings" => {
                show_main(app);
                let _ = app.emit("open-settings", ());
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
                show_main(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

/// Path to the local message cache, in WattMail's per-user data dir
/// (`%LOCALAPPDATA%\WattMail` / `~/Library/Application Support/WattMail` /
/// `~/.local/share/WattMail`). `SqliteStore::open` creates the parent on demand.
fn cache_db_path() -> std::path::PathBuf {
    paths::data_dir().join("cache.db")
}

/// Last reported inbox unread count; `-1` until the first report. Used to play a
/// sound only when the count *increases* (new mail), not on every sync.
static LAST_UNREAD: AtomicI64 = AtomicI64::new(-1);

/// Play the system notification sound (respects the user's sound scheme).
#[cfg(windows)]
fn play_notify_sound() {
    // user32!MessageBeep(MB_ICONASTERISK) — plays the "Asterisk" scheme sound,
    // asynchronously. Declared inline to avoid a windows-sys dependency.
    #[link(name = "user32")]
    extern "system" {
        fn MessageBeep(utype: u32) -> i32;
    }
    const MB_ICONASTERISK: u32 = 0x0000_0040;
    unsafe {
        MessageBeep(MB_ICONASTERISK);
    }
}

#[cfg(not(windows))]
fn play_notify_sound() {}

/// Update the tray icon + tooltip to reflect the inbox unread count, and chime
/// when the count increases.
pub(crate) fn update_tray(app: &AppHandle, unread: u32) {
    let previous = LAST_UNREAD.swap(i64::from(unread), Ordering::Relaxed);
    if previous >= 0 && i64::from(unread) > previous {
        play_notify_sound();
    }

    let Some(tray) = app.tray_by_id("main") else {
        return;
    };
    let tooltip = match unread {
        0 => "WattMail".to_string(),
        1 => "WattMail — 1 unread email".to_string(),
        n => format!("WattMail — {n} unread emails"),
    };
    let _ = tray.set_tooltip(Some(tooltip));
    if unread > 0 {
        let _ = tray.set_icon(Some(tauri::include_image!("icons/tray-unread.png")));
    } else if let Some(icon) = app.default_window_icon().cloned() {
        let _ = tray.set_icon(Some(icon));
    }
}

/// Bring the main window to the foreground.
fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}
