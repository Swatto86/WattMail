//! Linux WebKitGTK + NVIDIA session quirks.
//!
//! On Omarchy/Hyprland with the proprietary NVIDIA driver, showing a WattMail
//! window (including tray Activate after `--hidden` autostart) triggers WebKit's
//! DMA-BUF renderer. That path omits a Wayland acquire point; Hyprland answers
//! with protocol error 71 and kills the client — matching the 2026-09-04 tray
//! crash (`Error 71 … Wayland` + `WebKitWebProcess` SIGSEGV in nvidia-egl).
//! Apply before any GTK/WebKit init.

use webkit2gtk_nvidia_quirk::{apply_workaround_with_options, ApplyWorkaroundOptions};

/// Set the env vars WebKit/NVIDIA need for this session, if any.
pub fn apply_session_quirks() {
    apply_workaround_with_options(ApplyWorkaroundOptions::default());
}

#[cfg(test)]
mod tests {
    use super::*;
    use webkit2gtk_nvidia_quirk::{needs_workaround, WorkaroundKind};

    #[test]
    fn quirk_is_wired_before_tauri_builder() {
        let main = include_str!("main.rs");
        assert!(
            main.contains("apply_session_quirks"),
            "main must apply WebKit NVIDIA quirks before run()"
        );
        let lib = include_str!("lib.rs");
        let quirk_at = lib
            .find("linux_webkit::apply_session_quirks()")
            .expect("lib must call apply_session_quirks");
        let builder_at = lib
            .find("tauri::Builder::default()")
            .expect("scan must see Builder before asserting order");
        assert!(
            quirk_at < builder_at,
            "quirks must run before tauri::Builder (GTK/WebKit init)"
        );
    }

    #[test]
    fn apply_sets_dmabuf_disable_when_that_is_the_needed_kind() {
        // On this Omarchy host that is NVIDIA + Hyprland; elsewhere the kind
        // may differ — only assert the env effect for the Hyprland/X11 path.
        if needs_workaround() != WorkaroundKind::DisableWebkitDmabufRenderer {
            return;
        }
        // Clear so a prior test or shell export cannot make the assert a no-op.
        // SAFETY: single-threaded test process; no other threads read this yet.
        unsafe {
            std::env::remove_var("WEBKIT_DISABLE_DMABUF_RENDERER");
        }
        apply_session_quirks();
        assert_eq!(
            std::env::var("WEBKIT_DISABLE_DMABUF_RENDERER")
                .ok()
                .as_deref(),
            Some("1"),
            "Hyprland/NVIDIA needs WEBKIT_DISABLE_DMABUF_RENDERER=1 before show"
        );
    }
}
