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
- `src-tauri/src/tray_linux.rs` — SNI tray; updates via `mpsc` → tray thread
- `src-tauri/src/commands.rs` — IPC; `set_unread` → `update_tray`
- `crates/infrastructure/` — Graph, OAuth, keyring token store, SQLite cache
- `scripts/verify.sh` — full gate (fmt/clippy/test + reader-frame webview check)

## Data Flow

Frontend sync → `set_unread` → `update_tray` → (Linux) channel → ksni update.
Auth: `AuthService::access_token` refreshes via keyring-backed refresh token.

## Recent Context & Decisions

- 2026-09-03: v0.14.4 — tray updates must not call ksni blocking API on Tokio
  workers (Omarchy SIGABRT). Dedicated `wattmail-tray` thread.
- 2026-09-02: v0.14.3 — strip AppImage env when spawning browser/attachments.
- Living progress log: `CONTEXT.md`.
