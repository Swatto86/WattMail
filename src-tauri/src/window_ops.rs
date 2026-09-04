//! Main-window show / hide / toggle used by the tray and close-to-tray path.

use std::sync::atomic::Ordering;

use tauri::{AppHandle, Manager};

use crate::USER_HID_WINDOW;

/// Bring the main window to the foreground.
pub(crate) fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Primary tray click: hide when the window is shown, show when it is not.
/// Minimized counts as not shown so a tray click restores rather than hiding.
fn tray_click_hides(visible: bool, minimized: bool) -> bool {
    visible && !minimized
}

/// Toggle the main window from a tray primary-click (Linux SNI Activate or
/// Windows/macOS left-click). The tray menu's "Show WattMail" still always shows.
pub(crate) fn toggle_main(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let visible = window.is_visible().unwrap_or(false);
    let minimized = window.is_minimized().unwrap_or(false);
    if tray_click_hides(visible, minimized) {
        USER_HID_WINDOW.store(true, Ordering::SeqCst);
        let _ = window.hide();
    } else {
        show_main(app);
    }
}

#[cfg(test)]
mod tests {
    use super::tray_click_hides;

    #[test]
    fn tray_click_hides_when_the_window_is_shown() {
        assert!(
            tray_click_hides(true, false),
            "shown window must hide on tray click"
        );
        assert!(
            !tray_click_hides(false, false),
            "hidden window must show on tray click"
        );
        assert!(
            !tray_click_hides(true, true),
            "minimized window must restore, not hide"
        );
        assert!(!tray_click_hides(false, true), "hidden+minimized must show");
    }

    #[test]
    fn tray_primary_click_is_wired_to_toggle() {
        let lib = include_str!("lib.rs");
        assert!(
            lib.contains("toggle_main(tray.app_handle())"),
            "Windows/macOS left-click must toggle, not only show"
        );
        let linux = include_str!("tray_linux.rs");
        assert!(
            linux.contains("fn activate("),
            "scan must see Linux Activate before asserting the toggle"
        );
        assert!(
            linux.contains("toggle_main(&self.app)"),
            "Linux SNI Activate must toggle, not only show"
        );
    }
}
