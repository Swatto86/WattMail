# WattMail — AI Context

## System Overview

Personal desktop email client for Office 365 (Microsoft Graph) plus iCloud
calendar. Tauri v2 shell; tokens and cache keys live in the OS keychain.

## Tech Stack & Architecture

- Rust workspace (`domain` → `application` ← `infrastructure`), Tauri presentation
  in `src-tauri/`
- Frontend: Vite + TypeScript + Tailwind/DaisyUI (vanilla TS)
- Linux tray: ksni StatusNotifierItem (`src-tauri/src/tray_linux.rs`) on a
  dedicated thread so ksni’s blocking `block_on` never runs on Tokio workers
- Packaging: Windows NSIS + portable exe; Linux signed AppImage (tag-driven CI)

## Component Map

- `src-tauri/src/lib.rs` — composition, tray branch (Linux ksni vs native)
- `src-tauri/src/notify.rs` — OS notifications + new-mail sound off Tokio
  workers (`zbus::block_on` / `canberra` / `MessageBeep`)
- `src-tauri/src/commands.rs` — IPC; `set_unread` → `update_tray`
- `crates/infrastructure/` — Graph, OAuth, keyring token store, SQLite cache
- `scripts/verify.sh` — full gate (fmt/clippy/test + reader-frame webview check)

## Data Flow

Frontend sync → `set_unread` → `update_tray` → (Linux) channel → ksni update.
New mail / reminders → `show_desktop_notification` → dedicated thread →
notify-rust (never on a Tokio worker).
Auth: `AuthService::access_token` refreshes via keyring-backed refresh token.

## Recent Context & Decisions

- 2026-09-03: New-mail OS toast used `plugin:notification|notify` →
  notify-rust `zbus::block_on` on a Tokio worker → SIGABRT (same nested-runtime
  panic as the tray). `show_desktop_notification` offloads to a std thread;
  calendar reminders use the same path.
- 2026-09-03: v0.14.4 — tray updates must not call ksni blocking API on Tokio
  workers (Omarchy SIGABRT). Dedicated `wattmail-tray` thread.
- 2026-09-03: Add in-window new-mail popout and Linux/macOS notification
  sound (best-effort via freedesktop/macOS system sounds).
- 2026-09-02: v0.14.3 — strip AppImage env when spawning browser/attachments.
- Living progress log: `CONTEXT.md`.
