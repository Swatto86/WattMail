//! Desktop alerts and the new-mail sound.
//!
//! Linux `notify-rust` (via `tauri-plugin-notification`) calls `zbus::block_on`
//! to talk to the session bus. Tauri command handlers run on Tokio workers, so
//! showing a notification there panics: "Cannot start a runtime from within a
//! runtime" — SIGABRT with `panic = "abort"`. Same class of bug as the ksni
//! tray (`tray_linux`). All blocking notify/sound work runs on a plain
//! `std::thread`.

use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

/// Show an OS notification off the Tokio worker that handled the IPC call.
pub fn show_desktop_notification(app: AppHandle, title: String, body: String) {
    std::thread::spawn(move || {
        let _ = app.notification().builder().title(title).body(body).show();
    });
}

/// Play the system notification sound (respects the user's sound scheme).
#[cfg(windows)]
pub fn play_notify_sound() {
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
pub fn play_notify_sound() {
    #[cfg(target_os = "linux")]
    {
        std::thread::spawn(|| {
            fn try_cmd(prog: &str, args: &[&str]) -> bool {
                std::process::Command::new(prog)
                    .args(args)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            }

            if try_cmd("canberra-gtk-play", &["-i", "message-new-email"]) {
                return;
            }
            if try_cmd("canberra-gtk-play", &["-i", "mail-notification"]) {
                return;
            }
            if try_cmd("canberra-gtk-play", &["-i", "bell"]) {
                return;
            }

            let fallback = "/usr/share/sounds/freedesktop/stereo/message-new-email.oga";
            if std::path::Path::new(fallback).exists() && try_cmd("paplay", &[fallback]) {
                return;
            }
            let fallback2 = "/usr/share/sounds/freedesktop/stereo/bell.oga";
            if std::path::Path::new(fallback2).exists() {
                let _ = try_cmd("paplay", &[fallback2]);
            }
        });
    }

    #[cfg(target_os = "macos")]
    {
        std::thread::spawn(|| {
            let path = "/System/Library/Sounds/Ping.aiff";
            if !std::path::Path::new(path).exists() {
                return;
            }
            let _ = std::process::Command::new("afplay").arg(path).status();
        });
    }
}

#[cfg(test)]
mod tests {
    /// The failure observed on Omarchy when new mail fired
    /// `plugin:notification|notify`: notify-rust's `zbus::block_on` on a Tokio
    /// worker panics (and abort-on-panic kills WattMail).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nested_block_on_panics_inside_tokio_worker() {
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

    /// The fix: the same `block_on` is safe on a plain `std::thread`, even when
    /// the request originated on a Tokio worker (as `sendNotification` does).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_on_on_dedicated_thread_from_tokio_worker_is_safe() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
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
            tx.send(()).expect("notify thread alive");
            done_rx.recv().expect("block_on completed");
        })
        .await
        .expect("join");
    }

    #[test]
    fn frontend_does_not_call_plugin_notify_directly() {
        let helper = include_str!("../../src/desktop-notify.ts");
        let main = include_str!("../../src/main.ts");
        let calendar = include_str!("../../src/calendar.ts");
        assert!(
            helper.contains(r#"invoke("show_desktop_notification""#),
            "desktop-notify.ts must invoke the off-thread command"
        );
        assert!(
            main.contains("showDesktopNotification"),
            "new-mail path must use showDesktopNotification"
        );
        assert!(
            !main.contains("sendNotification"),
            "main.ts still imports or calls sendNotification — that hits zbus::block_on on a Tokio worker"
        );
        assert!(
            calendar.contains("showDesktopNotification"),
            "calendar reminders must use showDesktopNotification"
        );
        assert!(
            !calendar.contains("sendNotification"),
            "calendar.ts still imports or calls sendNotification"
        );
        let lib = include_str!("lib.rs");
        assert!(
            lib.contains("commands::show_desktop_notification"),
            "show_desktop_notification is not registered"
        );
    }
}
