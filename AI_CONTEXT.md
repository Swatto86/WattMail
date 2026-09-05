# WattMail — AI Context

## System Overview

Personal desktop email client for Office 365 (Microsoft Graph) plus iCloud
calendar. Tauri v2 shell; secrets live in one encrypted vault file whose key
is the single item WattMail keeps in the OS keychain (read once per process).

## Tech Stack & Architecture

- Rust workspace (`domain` → `application` ← `infrastructure`), Tauri presentation
  in `src-tauri/`
- Frontend: Vite + TypeScript + Tailwind/DaisyUI (vanilla TS)
- Linux tray: ksni StatusNotifierItem (`src-tauri/src/tray_linux.rs`) on a
  dedicated thread so ksni’s blocking `block_on` never runs on Tokio workers
- Linux WebKit/NVIDIA: `src-tauri/src/linux_webkit.rs` applies
  `webkit2gtk-nvidia-quirk` before GTK init (Hyprland → DMABUF off)
- Packaging: Windows NSIS + portable exe; Linux signed AppImage (tag-driven CI)
- Compile speed: workspace `[profile.dev]` (line-tables, dep opt-level 1,
  build-override 3); `scripts/fastcheck.sh`; CI uses sccache + debug
  `tauri build --debug --no-bundle` (release LTO unchanged). Linux `mold`
  is host-local (`~/.cargo/config.toml`), not committed.

## Component Map

- `src-tauri/src/lib.rs` — composition, tray branch (Linux ksni vs native)
- `src-tauri/src/linux_webkit.rs` — NVIDIA/Hyprland WebKit env quirks at boot
- `src-tauri/src/window_ops.rs` — show / toggle main window (tray Activate)
- `src-tauri/src/notify.rs` — OS notifications + new-mail sound off Tokio
  workers (`zbus::block_on` / `canberra` / `MessageBeep`)
- `src-tauri/src/commands.rs` — IPC; `set_unread` → `update_tray`
- `crates/infrastructure/` — Graph, OAuth, secrets vault (`vault.rs`,
  `secrets.rs`, `crypto.rs` master key) + token store, SQLite cache
- `src-tauri/src/migrate_keyring.rs` — one-off paced move of the old chunked
  keychain entries into the vault
- `src-tauri/src/autostart.rs` — refuses/repairs a login entry that would
  point at a non-installed binary
- `scripts/verify.sh` — full gate (fmt/clippy/test + reader-frame webview check)
- `scripts/fastcheck.sh` — iteration gate (`-p <crate>` = check only)

## Data Flow

Frontend sync → `set_unread` → `update_tray` → (Linux) channel → ksni update.
Boot → `checkForUpdates` → if newer signed release: banner + `downloadAndInstall`
→ `relaunch`. About check still shows Install/Later without forcing restart.
New mail / reminders → `show_desktop_notification` → dedicated thread →
Linux `notify-rust` directly (never `NotificationExt::show`, which spawns
back onto Tokio). Other OSes still use the plugin builder.
Auth: `AuthService::access_token` refreshes via the vault-backed refresh token.
Keychain: exactly one read per process (`crypto::master_key`), one write on
first run; never two Secret Service calls back to back (gnome-keyring 50.0
aborts on that).

## Recent Context & Decisions

- 2026-09-05: v0.15.0 — Secrets vault: every refresh token / app-password in
  `<data dir>/secrets.bin` (AES-256-GCM, 0600), keyed off the existing
  `cache-key` keychain item via SHA-256 derivation; one keychain read per
  process; paced migration from the chunked entries; autostart toggle refuses
  non-installed binaries and Linux start-up repairs a stale `.desktop` Exec.
- 2026-09-04: v0.14.8 — launch auto-downloads and installs updates (relaunch);
  About "Check for updates" still uses the banner + Install for a mid-session opt-in.
- 2026-09-04: Compile-speed defaults — proper `dev`/`debugging` profiles,
  `fastcheck.sh`, CI sccache, PR CI debug compile (no packaging). Release
  LTO left alone; Linux mold is host-local.
- 2026-09-04: v0.14.7 — Linux NVIDIA/Hyprland: apply `webkit2gtk-nvidia-quirk`
  before GTK init (`WEBKIT_DISABLE_DMABUF_RENDERER`) so tray Activate / show
  after `--hidden` does not die with Wayland protocol error 71.
- 2026-09-03: v0.14.6 — tray primary-click toggles the window (hide if shown).
- 2026-09-03: v0.14.4 — tray updates must not call ksni blocking API on Tokio
  workers (Omarchy SIGABRT). Dedicated `wattmail-tray` thread.
- 2026-09-03: Add in-window new-mail popout and Linux/macOS notification
  sound (best-effort via freedesktop/macOS system sounds).
- 2026-09-02: v0.14.3 — strip AppImage env when spawning browser/attachments.
- Living progress log: `CONTEXT.md`.
