# WattMail

[![CI](https://github.com/Swatto86/WattMail/actions/workflows/ci.yml/badge.svg)](https://github.com/Swatto86/WattMail/actions/workflows/ci.yml)

A fast, private, cross-platform desktop email client for **Office 365**, built with
**Rust + Tauri**. Privacy and security are first-class: email is sanitized and
rendered in a sandboxed frame, remote images are proxied, and the local cache is
encrypted at rest.

> Status: early but functional — see [`CONTEXT.md`](CONTEXT.md) for the live
> progress log, architecture decisions, and roadmap.

## Features

- **Office 365 over Microsoft Graph** with OAuth 2.0 (public client + PKCE, no secret).
- **Folder navigation** (nested folders, unread counts) with per-folder delta sync;
  the folder sidebar is cached, so it still renders on a cold offline start.
- **Local SQLite cache** — instant, offline-capable; auto-syncs every 60s.
- **Search** — cross-folder mail search via Microsoft Graph server-side `$search`.
- **Reading pane** with hostile-HTML sanitization in a sandboxed iframe; links open
  in your real browser.
- **Compose / reply / reply-all / forward** with a rich-text editor and attachments.
- **Drafts** — save, resume and send drafts (stored server-side in your mailbox).
- **Follow-up flags** — flag / clear messages from the list or the context menu.
- **Keyboard shortcuts** — `j`/`k` navigation and single-key actions (reply, flag,
  delete, search, compose) with a typing-safe guard.
- **Privacy**: "load images" proxies images server-side (no remote requests from the
  webview); the cache is AES-256-GCM encrypted at rest.
- **Auto-update** — checks the latest GitHub release on launch and installs signed
  (minisign) updates in place.
- **Start with Windows** — optionally launches hidden into the system tray at login,
  syncing in the background.
- Light / dark / system themes, system-tray, sort, resizable panes.

## Security & privacy model

- **OAuth** runs in the system browser; tokens live in the OS keychain (only the
  refresh token, chunked to fit Windows Credential Manager), never in the webview.
- **All networking happens in Rust** — the webview only does IPC, so the Content
  Security Policy stays locked (`img-src 'self' data:`, no remote origins).
- **Email bodies** are sanitized with [`ammonia`](https://crates.io/crates/ammonia)
  (scripts / event handlers / remote content removed; inline CSS allow-listed) and
  rendered in a `sandbox`-ed iframe with scripts disabled.
- **Remote images** (opt-in) are fetched server-side with clean headers and inlined
  as `data:` URLs — the webview never contacts a remote server. (A local fetch does
  not hide your IP; true IP-hiding would need a remote relay.)
- **Cache at rest**: subjects, senders, recipients and previews are AES-256-GCM
  encrypted; the key is stored in the OS keychain.

## Architecture

A Cargo workspace with strict, inward-pointing layers:

| Crate | Layer | Responsibility |
| --- | --- | --- |
| `crates/domain` | domain | Core types + the `MailProvider` / `MailStore` contracts. No I/O. |
| `crates/application` | application | Use-cases orchestrating the contracts. |
| `crates/infrastructure` | infrastructure | Graph client, OAuth/PKCE, SQLite cache, sanitization, crypto. |
| `src-tauri` | presentation | Tauri commands, window, tray, settings (composition root). |
| `apps/auth-spike` | — | Console tool that proves the OAuth + Graph round-trip. |

The transport sits behind a provider-agnostic `MailProvider` trait, so IMAP/SMTP or
other backends can be added without touching the application or presentation layers.

Frontend: **Vite + TypeScript + Tailwind + DaisyUI**, vanilla TS (no framework) for
fast startup.

## Getting started

### Prerequisites

- **Rust 1.96.0** (pinned via `rust-toolchain.toml`)
- **Node.js 20+**
- A **Microsoft Entra app registration** (public client / native, redirect
  `http://localhost`, delegated scopes `offline_access User.Read Mail.ReadWrite Mail.Send`).
  Put your `client_id` / `tenant_id` in `src-tauri/src/lib.rs` (the committed IDs are
  the author's — these are public client identifiers, not secrets).

### Run

```sh
npm install
npm run tauri dev      # desktop app
cargo run -p auth-spike  # console OAuth + Graph proof
```

### Verify (run before committing)

```sh
npm run build                              # tsc --noEmit + vite build
cargo fmt --all
cargo clippy --all-targets -- -D warnings  # never --lib
```

### Build an installer

```sh
npm run tauri build    # NSIS installer under target/release/bundle/
```

## License

MIT © Swatto
