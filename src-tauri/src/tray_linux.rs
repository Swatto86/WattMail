//! Linux system tray via ksni (StatusNotifierItem).
//!
//! Tauri's built-in tray click events are unsupported on Linux (the
//! libappindicator backend only ever shows a context menu — `on_tray_icon_event`
//! never fires), so a left/double click can't open the window. On the Wayland
//! status bars used by Omarchy/Hyprland (waybar) a primary click instead sends
//! the SNI `Activate` request, so here we register our own StatusNotifierItem and
//! route `Activate` to showing the window — giving real click-to-open — while a
//! right click still opens the Show / Settings / Quit menu.
//!
//! macOS and Windows keep Tauri's native tray (see `build_tray` in `lib.rs`),
//! where click routing works correctly.

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::StandardItem;
use ksni::{Icon, MenuItem, ToolTip, Tray};
use std::sync::OnceLock;

use tauri::{AppHandle, Emitter};

use crate::{quit_with_flush, show_main};

/// Live handle to the running tray, used by `update` to refresh the icon and
/// tooltip as the unread count changes.
static HANDLE: OnceLock<Handle<WattmailTray>> = OnceLock::new();

/// Decode a bundled 8-bit RGBA PNG into a single ARGB32 ksni icon. Returns
/// `None` for any unexpected format so a bad asset degrades to "no pixmap"
/// rather than a panic.
fn decode_icon(bytes: &[u8]) -> Option<Icon> {
    let decoder = png::Decoder::new(bytes);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let mut data = buf[..info.buffer_size()].to_vec();
    // SNI wants ARGB32; PNG gives RGBA8. Rotate each pixel's bytes right by one.
    for px in data.chunks_exact_mut(4) {
        px.rotate_right(1);
    }
    Some(Icon {
        width: info.width as i32,
        height: info.height as i32,
        data,
    })
}

fn idle_icon() -> Vec<Icon> {
    decode_icon(include_bytes!("../icons/32x32.png"))
        .into_iter()
        .collect()
}

fn unread_icon() -> Vec<Icon> {
    decode_icon(include_bytes!("../icons/tray-unread.png"))
        .into_iter()
        .collect()
}

struct WattmailTray {
    app: AppHandle,
    unread: u32,
    tooltip: String,
}

impl Tray for WattmailTray {
    fn id(&self) -> String {
        "co.swatto.wattmail".into()
    }

    fn title(&self) -> String {
        "WattMail".into()
    }

    /// Primary activation (waybar/most bars map a left click to this) — bring the
    /// window to the foreground. This is the behaviour Tauri's Linux tray can't
    /// provide and the reason this module exists.
    fn activate(&mut self, _x: i32, _y: i32) {
        show_main(&self.app);
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        if self.unread > 0 {
            unread_icon()
        } else {
            idle_icon()
        }
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self.tooltip.clone(),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Show WattMail".into(),
                activate: Box::new(|t: &mut Self| show_main(&t.app)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Settings…".into(),
                activate: Box::new(|t: &mut Self| {
                    show_main(&t.app);
                    let _ = t.app.emit("open-settings", ());
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut Self| quit_with_flush(&t.app)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Register the StatusNotifierItem. `assume_sni_available(true)` tolerates the
/// watcher not being up yet (e.g. autostarted into the tray before waybar has
/// started): the tray appears once the watcher comes online instead of failing.
pub fn spawn(app: AppHandle) {
    let tray = WattmailTray {
        app,
        unread: 0,
        tooltip: "WattMail".into(),
    };
    match tray.assume_sni_available(true).spawn() {
        Ok(handle) => {
            let _ = HANDLE.set(handle);
        }
        Err(e) => eprintln!("WattMail: failed to register Linux tray: {e}"),
    }
}

/// Refresh the tray icon (idle vs unread) and tooltip.
pub fn update(unread: u32, tooltip: String) {
    if let Some(handle) = HANDLE.get() {
        handle.update(move |t: &mut WattmailTray| {
            t.unread = unread;
            t.tooltip = tooltip;
        });
    }
}
