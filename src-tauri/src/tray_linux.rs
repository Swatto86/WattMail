//! Linux system tray via ksni (StatusNotifierItem).
//!
//! Tauri's built-in tray click events are unsupported on Linux (the
//! libappindicator backend only ever shows a context menu — `on_tray_icon_event`
//! never fires), so a left/double click can't open the window. On the Wayland
//! status bars used by Omarchy/Hyprland (waybar) a primary click instead sends
//! the SNI `Activate` request, so here we register our own StatusNotifierItem and
//! route `Activate` to toggling the window (hide if shown, show if hidden) —
//! while a right click still opens the Show / Settings / Quit menu.
//!
//! All ksni **blocking** API calls run on a dedicated `std::thread`. ksni's
//! `Handle::update` / `spawn` use an internal `Runtime::block_on`; calling that
//! from a Tokio worker (Tauri command handlers) panics with "Cannot start a
//! runtime from within a runtime" and aborts the process.
//!
//! macOS and Windows keep Tauri's native tray (see `build_tray` in `lib.rs`),
//! where click routing works correctly.

use ksni::blocking::TrayMethods;
use ksni::menu::StandardItem;
use ksni::{Icon, MenuItem, ToolTip, Tray};
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;
use std::thread;

use tauri::{AppHandle, Emitter};

use crate::{quit_with_flush, show_main, toggle_main};

/// Commands for the dedicated tray thread. Only that thread may call ksni's
/// blocking `spawn` / `Handle::update`.
enum TrayCmd {
    Update { unread: u32, tooltip: String },
}

static CMD_TX: OnceLock<Sender<TrayCmd>> = OnceLock::new();

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

    /// Primary activation (waybar/most bars map a left click to this) — toggle
    /// the window. Show/Settings in the menu still always bring it forward.
    fn activate(&mut self, _x: i32, _y: i32) {
        toggle_main(&self.app);
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

/// Register the StatusNotifierItem on a dedicated thread. `assume_sni_available(true)`
/// tolerates the watcher not being up yet (e.g. autostarted into the tray before
/// waybar has started): the tray appears once the watcher comes online instead of
/// failing.
pub fn spawn(app: AppHandle) {
    let (tx, rx) = mpsc::channel();
    if CMD_TX.set(tx).is_err() {
        return;
    }
    if let Err(e) = thread::Builder::new()
        .name("wattmail-tray".into())
        .spawn(move || run_tray_thread(app, rx))
    {
        eprintln!("WattMail: failed to start Linux tray thread: {e}");
    }
}

fn run_tray_thread(app: AppHandle, rx: mpsc::Receiver<TrayCmd>) {
    let tray = WattmailTray {
        app,
        unread: 0,
        tooltip: "WattMail".into(),
    };
    let handle = match tray.assume_sni_available(true).spawn() {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("WattMail: failed to register Linux tray: {e}");
            return;
        }
    };
    while let Ok(cmd) = rx.recv() {
        match cmd {
            TrayCmd::Update { unread, tooltip } => {
                handle.update(move |t: &mut WattmailTray| {
                    t.unread = unread;
                    t.tooltip = tooltip;
                });
            }
        }
    }
}

/// Refresh the tray icon (idle vs unread) and tooltip. Safe to call from any
/// Tauri command thread — work is forwarded to the tray thread.
pub fn update(unread: u32, tooltip: String) {
    if let Some(tx) = CMD_TX.get() {
        let _ = tx.send(TrayCmd::Update { unread, tooltip });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    /// The failure mode observed on Omarchy: ksni's blocking API calls
    /// `Runtime::block_on` on a private current-thread runtime. Doing that on a
    /// Tokio worker panics (and with abort-on-panic, kills WattMail).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ksni_style_block_on_panics_inside_tokio_worker() {
        let panicked = tokio::spawn(async {
            std::panic::catch_unwind(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("current-thread runtime");
                rt.block_on(async {});
            })
            .is_err()
        })
        .await
        .expect("join");
        assert!(
            panicked,
            "nested Runtime::block_on must panic inside a Tokio worker"
        );
    }

    /// The fix pattern: the same `block_on` is safe when confined to a plain
    /// `std::thread` that is not driving Tokio, even when the request originates
    /// on a Tokio worker (as `set_unread` / tray updates do).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_on_on_dedicated_thread_from_tokio_worker_is_safe() {
        let (tx, rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread runtime");
            while rx.recv().is_ok() {
                rt.block_on(async {});
                let _ = done_tx.send(());
            }
        });

        tokio::spawn(async move {
            tx.send(()).expect("tray thread alive");
            done_rx.recv().expect("block_on completed");
        })
        .await
        .expect("join");
    }

    /// `update` before `spawn` is a no-op and must not touch ksni / panic.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn update_without_spawn_is_noop() {
        tokio::spawn(async {
            update(3, "WattMail — 3 unread emails".into());
        })
        .await
        .expect("join");
    }
}
