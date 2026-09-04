// Hide the extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Linux: NVIDIA + Hyprland WebKit DMA-BUF protocol error 71 — apply before
    // anything that can load GTK/WebKit (including lib::run → Builder).
    #[cfg(target_os = "linux")]
    wattmail_desktop_lib::linux_webkit::apply_session_quirks();

    wattmail_desktop_lib::run();
}
