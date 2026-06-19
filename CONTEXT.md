# WattMail — Living Context

> Progress log, architecture decisions, and open questions for WattMail.
> **Maintenance:** update this at the end of any session with meaningful changes —
> new milestone state, a decision made/reversed, or an open question resolved.
> Keep newest progress entries at the top of the log.
>
> **Last updated:** 2026-06-18

---

## Overview

A personal, cross-platform (Windows / macOS / Linux) email client. Initial target:
**Office 365 business mailboxes via the Microsoft Graph API** with OAuth 2.0. The
transport sits behind a provider-agnostic `MailProvider` contract so IMAP/SMTP or
other backends can be added later without touching the application or presentation
layers.

## Tech stack

| Area | Choice |
| --- | --- |
| Language (core) | Rust (workspace, edition 2021, toolchain pinned 1.96.0 at repo root) |
| Desktop shell | Tauri v2 (`tray-icon`, single-instance) |
| Frontend | Vite 6 + TypeScript + Tailwind 3 + **DaisyUI 4** (vanilla TS, no framework — fast startup) |
| Themes | DaisyUI `business` = dark, `corporate` = light, `system` = follow OS |
| Mail API | Microsoft Graph (REST) via `reqwest` |
| Auth | `oauth2`-style public-client + PKCE done with raw form-posts; tokens in OS keychain (`keyring`) |
| MIME (future) | `mail-parser` / `mail-builder` (Stalwart) |
| Local cache (future) | SQLite (`rusqlite`/`sqlx`) |

Stack and patterns deliberately mirror Swatto's **AllTheThings**
(github.com/Swatto86/AllTheThings) for a proven fast-startup Tauri setup.

## Repo layout

```
WattMail/
├─ Cargo.toml                 # workspace + release profile (LTO/strip)
├─ rust-toolchain.toml        # pinned 1.96.0 (repo root, per Tauri rules)
├─ package.json, index.html, vite/tailwind/postcss/tsconfig   # frontend (root)
├─ src/                       # frontend TypeScript (main.ts, styles.css)
├─ src-tauri/                 # Tauri crate = presentation layer + composition root
│  ├─ src/{main,lib,commands,settings}.rs
│  ├─ tauri.conf.json, build.rs, capabilities/, icons/, appicon.png
├─ crates/
│  ├─ domain/                 # EmailAddress, MessageSummary, MailProvider trait — no I/O
│  ├─ application/            # inbox_preview() use-case over the trait
│  └─ infrastructure/         # Graph client, OAuth/PKCE flow, chunked keyring token store
└─ apps/auth-spike/           # console proof of the OAuth + Graph round-trip
```

Dependencies point inward: `presentation/composition root → application → domain`,
with `infrastructure` implementing the domain contracts. `src-tauri` is the only
module that spans presentation + composition (wiring infra into the app).

## Build / run / verify

```powershell
# Run the desktop app (dev)
npm run tauri dev

# Console auth proof
cargo run -p auth-spike

# Verify (run before declaring done)
npm run build                              # tsc --noEmit + vite build
cargo fmt --all
cargo clippy --all-targets -- -D warnings  # never --lib
```

Entra app registration (public, not secret):
`client_id = 60d6101b-3d8a-4a09-8718-ad90c0d88f13`,
`tenant_id = 652459b1-612f-4586-b424-a0069d51cc32` (single-tenant, SWATTO.CO.UK).

---

## Progress log

### 2026-06-18 — Notifications, tray tooltip, inbox rules, link context menu, shortcuts cheat-sheet (v0.1.13)
Built as a multi-agent batch (plan → implement → build verify). `cargo check`, `cargo clippy --all-targets -D warnings`, `cargo fmt --check`, and `npm run build` all clean. Live run pending.

- **Desktop notifications for new mail** — after each Inbox background sync, the frontend calls a new `check_new_mail` command that compares the cached messages' `received` timestamps against an in-memory `last_notified_at` (managed `NotificationState` in `lib.rs`). If newer unread messages exist and the `notifications_enabled` setting is on, a native OS notification is shown via `@tauri-apps/plugin-notification` (JS API: `sendNotification`). The setting persists in `settings.json` (`Settings.notifications_enabled`); on first enable, OS permission is requested and the toggle reverts if denied. `tauri-plugin-notification` added to `Cargo.toml` + capabilities. Notification click is handled by a Tauri `open-message` event listener that focuses the window and opens the message.
- **Tray tooltip with account email** — `update_tray` now reads the cached account email from the SQLite store (new `SqliteStore::cached_account_email()` sync helper) and includes it in the tooltip: `WattMail — user@example.com — N unread emails`. The icon still switches to `tray-unread.png` when unread > 0.
- **Message rules manager** — new domain types `MessageRule` / `MessageRuleConditions` / `MessageRuleActions` (serde, camelCase). Graph client methods `list_message_rules` / `create_message_rule` / `update_message_rule` / `delete_message_rule` over `GET/POST/PATCH/DELETE /me/mailFolders/inbox/messageRules`, mapping Graph `fromAddresses`/`subjectContains`/`recipientContains`/`moveToFolder`/`markAsRead`. Tauri commands wired in `commands.rs` + `lib.rs` `generate_handler!`. Frontend rules overlay (list + editor form) inside the settings panel; conditions are sender/subject/recipient contains (comma lists), actions are move-to-folder dropdown + mark-as-read checkbox. **`MailboxSettings.ReadWrite`** scope added to `auth/mod.rs` — existing signed-in users need to sign out and back in for the new scope to take effect.
- **"Copy link address" in reading pane** — `wireFrameLinks` now also attaches a `contextmenu` listener inside the sandboxed iframe. Right-clicking an `<a>` shows a custom context menu with "Copy link address"; clicking it copies the href via the existing `copyText` helper. The menu is dismissed on outside click / Escape / window blur.
- **Keyboard shortcut cheat-sheet** — pressing `?` (when not typing / no modal open) toggles a `#shortcuts-overlay` listing all shortcuts in a two-column table. Closeable with Escape or clicking the backdrop. Added to `aModalIsOpen()` and the Escape handler.

### 2026-06-17 — Compose: resizable/maximizable + rich input (v0.1.12)
Built as a multi-agent batch (sequential backend→frontend implement → full build verify →
3 adversarial reviewers (security/correctness/integrity) → remediation). `cargo fmt`,
`clippy --all-targets -D warnings`, and `npm run build` all clean. Live run pending.

- **Resizable + maximizable compose** — the compose/reply/forward panel has a bottom-right
  drag grip (same pointer-capture idiom as the list splitter) that resizes width+height
  (clamped to min 420×360 and ≤96vw/92vh) and persists to localStorage (`wattmail.composeW`/
  `composeH`/`composeMax`); a Maximize/Restore toggle in the header. Default size 720×560; the
  panel is now a flex column so the body grows to fill and scrolls (header/inputs/toolbar/
  actions pinned).
- **Rich paste** — replaced the plain-text-only paste handler. Pasted (and dropped) `text/html`
  is now **sanitized** and inserted with formatting preserved; plain-text fallback otherwise.
  The sanitizer (TS, security-critical — pasted content can come from a hostile page) parses via
  `DOMParser` into a detached doc then rebuilds an **allow-listed** tree with `createElement`/
  `setAttribute` (never dirty `innerHTML`): drops script/style/iframe/object/embed/link/meta/
  base/form/input/button/svg subtrees, strips `on*` + `javascript:`/`vbscript:`, allow-lists a
  style attribute (rejecting `url()`/`expression()`/`@import`/`position:fixed`/behavio(u)r),
  limits `a@href` to http/https/mailto and `img@src` to https or base64-raster `data:`.
- **Inline images** — paste/drop an image into the body → embedded as a `data:` URL (CSP
  `img-src 'self' data:` allows it). On **send**, each `data:image/…;base64,` `<img>` is rewritten
  to `cid:<id>` and shipped as an **inline `fileAttachment`** (Graph `isInline`+`contentId`).
  `OutgoingAttachment` gained `content_id: Option<String>` + `is_inline: bool`; the `send_message`
  command gained an `inlineImages` param (base64 → bytes). Per-image + cumulative ~3 MB caps are
  enforced at send time (not just on insert). svg+xml and non-base64 `data:` images are excluded
  (one shared `INLINE_IMAGE_SRC` regex across sanitizer/insert/extract). **Drafts:** sending a
  draft that contains inline `data:` images is blocked with a warning (draft attachments are still
  deferred), so no unrenderable `data:` URI is shipped.
- **`dragDropEnabled: false`** added to the main window in `tauri.conf.json` so the webview
  receives DOM drag/drop events (Tauri's native OS drag-drop would otherwise swallow them); paired
  with `dragenter`/`dragover` listeners on the editor.

### 2026-06-17 — Quick-wins batch: search, drafts, flags, folder cache, shortcuts (v0.1.11)
Built as one coordinated batch (multi-agent: sequential implement → full build verify →
adversarial review → remediation). All four features landed across the proper layers;
`cargo fmt`, `clippy --all-targets -D warnings`, and `npm run build` all clean. Live run
pending (needs a signed-in window).

- **Search** — cross-folder mail search via Graph server-side `$search`. New
  `MailProvider::search(query, top)`; Graph `GET /me/messages?$search="…"` with the
  `ConsistencyLevel: eventual` header, **no** `$orderby` (illegal with `$search`) — results
  sorted newest-first in Rust; query percent-encoded via `reqwest .query()` and embedded
  double-quotes stripped to keep the KQL phrase well-formed. No new scope (covered by
  `Mail.ReadWrite`), no cache change (local FTS is impossible — content columns use a
  per-value random nonce). Command `search_messages`; debounced toolbar input rendering into
  the existing `#list`; a `searchActive`/`searchSeq` guard stops the 60s auto-sync and stale
  responses from clobbering results.
- **Drafts** — save / resume / send drafts. New `MailProvider::create_draft`/`update_draft`/
  `send_draft`/`load_draft` (+ `DraftPrefill` domain type) over Graph `POST/PATCH /me/messages`
  and `POST /me/messages/{id}/send`. Resume loads the **raw** body (not the display-sanitized
  HTML); sending a resumed draft goes `save_draft → send_draft` (never `sendMail`), so it can't
  double-send or orphan. Clicking a row in the Drafts folder opens compose in edit mode.
  Commands `save_draft`/`send_draft`/`load_draft`. **Deferred:** attachments on drafts (compose
  warns rather than silently dropping them).
- **Follow-up flags** — `is_flagged` added to `MessageSummary`/`MessageDto` (+ all construction
  sites). `MailProvider::set_flag` (PATCH `/me/messages/{id}` `{flag:{flagStatus}}`) and
  `MailStore::set_flag`; command `set_flag`. Flag glyph on flagged rows + a Flag/Clear-flag
  context-menu toggle (optimistic). `flag,flagStatus` added to the list/delta `$select`.
- **Cached folder sidebar** — new `folders` cache table (name **encrypted**; id/unread/depth/
  position plaintext for ordering); `MailStore::save_folders`/`cached_folders`. `list_folders`
  is now write-through (live → persist → return; falls back to cache on network failure), so a
  cold offline start shows folders.
- **Schema bump 4 → 5** (single migration covering both flags + folders): added `is_flagged`
  column + `folders` table to `SCHEMA`, and `DROP TABLE IF EXISTS folders` to `migrate()`. The
  disposable cache drops-and-rebuilds once on first launch after upgrade (documented pattern).
- **Keyboard shortcuts (MVP, frontend-only)** — `j`/`k` (+ arrows) cursor, Enter to open,
  `r`/`a`/`f` reply/reply-all/forward, `u` toggle read, `#` delete, flag toggle, `/` focus
  search, `c` compose, Escape integrated with the modal stack. A typing/modal/context-menu guard
  keeps keys inert while composing or searching. Highlight CSS uses `oklch(var(--…))` (DaisyUI
  4.12) — not `hsl()`. Command palette + signatures/templates deferred to a later phase.

### 2026-06-16 — Headers viewer: flag unverifiable To: (v0.1.10)
- The `To:` header is sender-supplied and forgeable. The Overview now corroborates it against a
  delivery header (`Received … for` / `Delivered-To`): shows a caution when nothing backs it up
  (confirm via server-side message trace), and flags a disagreement when a delivery address
  exists but differs from `To:`. Caution callout uses a warning tint with `--bc` text for
  light-theme legibility. (main.ts + styles.css only.)

### 2026-06-16 — View & trace email headers (v0.1.9)
- New **Headers** action in the reading pane: fetches a message's internet headers (Graph
  `internetMessageHeaders`) and opens a trace view — parsed Overview, SPF/DKIM/DMARC verdict
  badges, Microsoft spam-filter summary (SCL/BCL/CAT/CIP/CTRY), the `Received` delivery path
  (origin → mailbox), and the full raw header list with key headers highlighted, plus a filter
  box and copy-all. New command `message_headers` wired domain → application → infrastructure →
  presentation; `MessageHeader` domain type; badge text uses `--bc` for both themes.

### 2026-06-16 — Fix transparent context menu (v0.1.8)
- The right-click menu (and `.load-more`) used `hsl(var(--b1))`, but this project's DaisyUI (4.12)
  exposes theme tokens as **oklch** components — the rest of the app uses `oklch(var(--b1))`
  (styles.css). `hsl(var(--bX))` is an invalid color → silent fallback to `transparent` (bg) /
  inherited (text), so the menu rendered see-through with visible text. Swapped all custom
  `hsl(var(...))` → `oklch(var(...))` in styles.css.
- Recurring class of bug across our Tauri+DaisyUI apps; added a preventive bullet to the global
  CLAUDE.md Tauri section ("match the project's color-function wrapper — grep `(var(--b1)`").
- Verified: `npm run build` clean (no `hsl(var(` left).

### 2026-06-16 — Context menu: Move to folder (v0.1.7)
- Adds **Move to folder…** to the email context menu. Clicking it drills the menu
  into a scrollable, depth-indented folder list (current folder excluded) with a
  **← Back** item; picking a folder moves the message. The drill-in swaps the menu's
  contents in place (no nested flyout); `e.stopPropagation()` on menu clicks stops
  the in-place content swap from tripping the outside-click dismiss.
- **Backend:** `MailProvider::move_message(id, destination_folder_id)` → Graph
  `POST /me/messages/{id}/move` `{destinationId}` (returns the moved copy; ignored —
  the destination folder's next delta sync picks it up). Application `move_message`
  use-case = provider.move + `store.remove_message` (drops it from the source cache).
  Command `move_message` registered in lib.rs. `Mail.ReadWrite` covers it.
- Frontend `moveMessage()` removes the row optimistically, then refreshes the
  loaded/total count and unread badges; restores from cache on failure.
- Verified: `npm run build` (incl. `tsc`), `cargo fmt --check`, `clippy --all-targets
  -D warnings` clean. Live run pending; not yet released.

### 2026-06-16 — Right-click context menu on emails (v0.1.6)
- **Custom webview context menu** on message-list rows (matches the all-custom UI; themed via
  DaisyUI CSS vars; acts on the right-clicked row, not just the open message). Actions: Open, Reply,
  Reply all, Forward, Mark as read/unread (toggles on the row's state), Delete (→ Deleted Items).
  Built in `main.ts` (element appended to body, viewport-clamped, dismissed on click-away / Esc /
  scroll / window blur); off-row right-clicks keep the default menu.
- **New backend actions:** `MailProvider::mark_read(id)` generalized to `set_read(id, read)` (PATCH
  `isRead`); added `delete_message(id)` (Graph `DELETE /me/messages/{id}` → soft-delete to Deleted
  Items). Threaded through application (`set_read`/`delete_message` use-cases: provider + cache) and
  commands (`set_read`, `delete_message`; old `mark_read` command replaced). Added a `check_status`
  helper in the Graph client to de-dup response validation. `Mail.ReadWrite` already covers
  delete/move — no new scope.
- Frontend: `replyTo`/`forwardMsg` now take an optional `id` (default = open message) so the menu
  and the reader toolbar share one path; delete/read changes update optimistically then refresh
  unread badges + loaded/total count.
- Verified: `npm run build` (incl. `tsc`), `cargo fmt --check`, `clippy --all-targets -D warnings`
  clean. Live run pending; not yet released. Deferred: Move-to-folder (needs a folder picker);
  reader-pane right-click.

### 2026-06-16 — Message list paging ("Load more") (v0.1.5)
- **Clarified:** the cache was never capped — delta sync already pages the **whole** folder into
  SQLite (follows `@odata.nextLink` to the `deltaLink`; `$top=50` is just the Graph page size). The
  50-message limit was purely a **read/display** cap (`store.recent(… LIMIT top)` with a fixed
  `top=50`).
- **Fix (read side):** the list now reads a **growing window** of the already-cached folder. New
  `MailStore::count(folder_id)` (COUNT(*), covered by `idx_messages_folder_received`) returns the
  folder total; `CachedFolder`/`InboxDto` carry it. Frontend: `loadedCount` starts at `PAGE_SIZE`
  (50), a **"Load more"** control grows it by `PAGE_SIZE`, switching folders resets it, and the
  status line shows "N of M messages". Window persists across the 60s auto-sync (scroll preserved).
- Paging beats rendering the whole folder at once: a large Inbox would otherwise rebuild tens of
  thousands of DOM rows every auto-sync.
- Verified: `npm run build` (incl. `tsc`), `cargo fmt --check`, `clippy --all-targets -D warnings`
  clean. Live run pending; not yet released.

### 2026-06-16 — Cross-platform config dir + CI compile-gate (v0.1.4)
- **Config-dir abstraction:** new `src-tauri/src/paths.rs` `data_dir()` resolves the per-user
  data dir via `dirs::data_local_dir()` (`%LOCALAPPDATA%\WattMail` / `~/Library/Application
  Support/WattMail` / `~/.local/share/WattMail`). On Windows this equals the old hand-rolled
  `%LOCALAPPDATA%\WattMail` path **exactly**, so existing caches/settings are found in place — no
  migration. Both `cache_db_path()` (lib.rs) and `settings_path()` (settings.rs) now build on it.
  Settings persistence previously **silently failed off-Windows** (`LOCALAPPDATA` unset → every
  save errored, every load reverted to defaults); now infallible everywhere.
- **CI cross-check job:** `ci.yml` gains a `cross-check` matrix (`macos-latest`, `ubuntu-22.04`)
  running fmt + `clippy --all-targets -D warnings`, with the Tauri Linux system deps. This is the
  first time the `cfg(not(windows))` branch (e.g. the no-op `play_notify_sound`) and the
  `apple-native` / `sync-secret-service` keyring backends are actually **compiled** — previously
  gated by inspection only, since the existing `verify` job is Windows-only.
- Verified locally (Windows): `npm run build`, `cargo fmt --check`, `clippy --all-targets -D
  warnings` all clean. macOS/Linux compile is exercised by the new CI job (not run locally).

### 2026-06-16 — Start with Windows (hidden in tray) (v0.1.3)
- **Autostart:** `tauri-plugin-autostart` registers WattMail at login with a `--hidden` arg
  (Windows `HKCU\…\Run`). A login-launched instance detects the flag (`StartHidden` managed state),
  **skips revealing the window** (frontend `started_hidden` command + the Rust 3s safety-net is
  gated on it) and sits in the tray, still syncing in the background. A **manual** launch has no
  flag and shows the window normally; the tray icon reveals it any time.
- **Settings toggle** "Start with Windows" drives the plugin's `enable()`/`disable()` directly from
  the frontend; the toggle reverts if the OS call fails. `autostart:default` capability added.
- Verified: `npm run build` clean, `cargo build` clean, `clippy --all-targets -D warnings` clean.

### 2026-06-16 — Auto-update + repo made public (v0.1.2)
- **Auto-update:** Tauri updater wired in (`tauri-plugin-updater` + `tauri-plugin-process`). On launch
  the frontend calls `check()`; if a newer signed release exists, a top-of-shell banner offers
  "Install & restart" (`downloadAndInstall()` → `relaunch()`). Manifest served from the rolling
  release: `…/releases/latest/download/latest.json`. `createUpdaterArtifacts: true` makes
  `tauri build` / `tauri-action` emit the signed `latest.json` + `.sig`.
- **Signing:** minisign keypair at `~/.tauri/wattmail-updater.key` (empty password). Public key
  embedded in `tauri.conf.json` (`plugins.updater.pubkey`); private key + empty password set as repo
  secrets `TAURI_SIGNING_PRIVATE_KEY` / `…_PASSWORD`, consumed by `release.yml`.
- **Repo made public:** updater can't fetch assets from a private repo unauthenticated, so
  `Swatto86/WattMail` is now **public**. Pre-flight: gitleaks history scan of WattMail (clean) **and
  all 21 public repos** (clean; one confirmed false positive). Entra client/tenant IDs are public
  values, safe to ship.
- **Updater only activates from v0.1.2 onward** — v0.1.1 has no updater, so this release must be
  installed manually once; subsequent releases auto-update.
- Verified: `npm run build` clean, `cargo build` clean, `clippy --all-targets -D warnings` clean.

### 2026-06-16 — Release model: single rolling release (v0.1.1)
- Releases are now a **single rolling release**: only the latest version's release + tag exist; cutting
  a new one deletes the previous. `release.yml` now **publishes** (no longer draft). Cut v0.1.1 (tray
  badge/tooltip + new-mail sound), removing v0.1.0. Process documented in memory `wattmail-release-workflow`.

### 2026-06-16 — Tray unread indicator + new-mail sound
- A red-badged tray icon variant (`src-tauri/icons/tray-unread.png`, embedded via
  `include_image!`) is shown when the **Inbox** has unread mail; the tray tooltip reads
  "WattMail — N unread email(s)". `update_tray(app, count)` + a `set_unread` command, driven
  from the frontend after each folder sync (updates within the 60s auto-sync).
- **New-mail sound:** `update_tray` tracks the previous count (`static AtomicI64`) and plays the
  Windows notification sound (`user32!MessageBeep(MB_ICONASTERISK)` via inline FFI — no dep) when
  the count **increases**, so it only chimes on genuinely new mail, not on every sync.
- Verified: builds, `npm run build` clean, `clippy --all-targets -D warnings` clean.

### 2026-06-16 — Rich-text compose + GitHub repo & CI/release
- **Rich-text compose:** the compose body is a `contenteditable` editor with a formatting toolbar
  (bold/italic/underline/lists/link/clear) via `execCommand`; paste is coerced to plain text; the
  quoted reply now lives **in** the editor (visible & editable). Sends the editor's `innerHTML`.
- **Repo + CI:** committed to private `Swatto86/WattMail`. `README.md`; `ci.yml` (pinned
  `dtolnay/rust-toolchain@1.96.0`, `npm ci` → `npm run build` → `fmt --check` →
  `clippy --all-targets -D warnings` → full `npm run tauri build`) and a tag-driven `release.yml`
  (`tauri-action`, draft release with the NSIS installer) — honouring the standing Tauri CI rules.

### 2026-06-16 — Image proxy, cache encryption, sort
- **Image proxy:** "load images" now fetches each remote `<img>` **server-side in Rust** (clean
  headers — no cookies/referer/UA fingerprint) and inlines them as `data:` URLs; failed/non-image
  sources are blanked. The webview makes **zero remote requests**, so CSP `img-src` tightened to
  `'self' data:`. (A local fetch still leaves the user's machine — not IP-hiding; that needs a
  remote relay. Sequential fetch; images > 5 MB skipped.)
- **Cache encryption:** content columns (subject/sender/recipients/preview) and all `sync_state`
  values are AES-256-GCM encrypted at rest (`infrastructure/crypto.rs`); 256-bit key in the OS
  keychain. `id`/`folder_id`/`received`/`is_read` stay plaintext for sort/filter. Schema v4 (rebuild).
- **Sort:** toolbar dropdown — Newest / Oldest / Sender / Subject / Unread first (client-side over the
  loaded window; `INBOX_TOP` 25 → 50). Persisted in localStorage.
- Verified: builds, `npm run build` clean, `clippy --all-targets -D warnings` clean. Live run pending.

### 2026-06-16 — Attachments
- **Receive:** `attachments(messageId)` lists non-inline file attachments; chips in the reader →
  click → save dialog → `save_attachment` fetches the attachment `/$value` bytes and writes to disk.
- **Send:** compose **Attach** (multi-file picker) → `send_message` reads the files, base64-encodes
  them as Graph `fileAttachment`s in the `sendMail` payload.
- `MailProvider` gains `attachments` + `attachment_bytes`; `OutgoingMessage` gains `attachments`.
  Added `tauri-plugin-dialog` (file open/save) + `dialog:default` capability. Verified: builds,
  `npm run build` clean, `clippy --all-targets -D warnings` clean. Live run pending.

### 2026-06-16 — Auto-sync + email readability
- **Auto-sync:** quiet background sync of the current folder every **60s** (list scroll preserved,
  no status churn), on top of launch / folder-switch / manual Refresh. New mail now appears without
  pressing Refresh.
- **Readability:** email bodies now render on a **white background** (app chrome stays dark) — email
  HTML assumes a light theme, so authors' dark/grey text was invisible on the old dark body. Fixes
  the dark-on-dark bodies and the grey-on-dark SwatBox report.

### 2026-06-16 — Compose / reply / forward / send
- `MailProvider::send_message(OutgoingMessage)` → Graph `POST /me/sendMail` (saved to Sent Items).
- Reply / Reply-all / Forward prefills built in Rust (`compose_reply` / `compose_forward`):
  recipients (reply-all CCs the original to+cc minus self), `Re:` / `Fwd:` subject, quoted original
  HTML. `MessageBody` gains sender/to/cc addresses; `message()` now fetches `ccRecipients`.
- Compose modal (To/Cc/Subject/Body); Reply/Reply-all/Forward buttons in the reader header,
  Compose in the toolbar. Body = typed text (→ HTML) + quoted original; send is always
  user-initiated via the Send button.
- Commands `prepare_reply`, `prepare_forward`, `send_message`. Verified: builds, `npm run build`
  clean, `clippy --all-targets -D warnings` clean. Live run pending.

### 2026-06-16 — Nested folders, sent recipient display, rich body rendering
- **Nested folders:** `folders()` walks the tree (DFS over `childFolders`), each annotated with
  `depth`; the sidebar indents children under their parent.
- **Sent/Drafts recipient display:** `MessageSummary` gains a `to` summary (first recipient `+N`)
  via `toRecipients`; the list shows "To: …" for outgoing folders (Sent/Drafts/Outbox). Cache
  schema → v3 (rebuild on first run).
- **Rich body rendering:** the sanitizer keeps inline `style` (plus `bgcolor`/`align`/`width`/…
  and `<font color>`) through a **CSS-property allowlist** that rejects `url(...)`, `expression`,
  `@import`, `javascript:` — so styled mail (the SwatBox table, coloured ticks) renders with
  fidelity, still with no remote-content vector.
- Verified: builds, `npm run build` clean, `clippy --all-targets -D warnings` clean. Live run pending.

### 2026-06-16 — Folder navigation
- Folder sidebar (live `GET /me/mailFolders`, with unread badges); click a folder to view + sync it.
- Sync is now **per-folder**: `MailProvider::sync(folder_id, since)`, each folder keeps its own
  deltaLink (`delta:{folderId}` in `sync_state`); the cache `messages` table gains a `folder_id`
  column (**schema v2** — the old `cache.db` is dropped & rebuilt on first run via `PRAGMA user_version`).
- Commands `list_folders`, `folder_from_cache(folderId)`, `sync_folder(folderId)` replace the
  inbox-only versions. UI defaults to Inbox (matched by displayName).
- Verified: builds, `npm run build` clean, `clippy --all-targets -D warnings` clean. Live run pending.

### 2026-06-16 — Milestone 3: sync engine + SQLite cache — built
- Provider-agnostic `MailProvider::sync(since) -> SyncBatch{changes, token}` with an opaque
  `SyncToken` (Graph deltaLink now; IMAP UID/modseq later). The Graph impl pages the delta
  query (`/me/mailFolders/inbox/messages/delta`) internally, accumulating add/update/remove.
- `MailStore` port (domain) implemented by `SqliteStore` (rusqlite, **bundled** SQLite) at
  `%LOCALAPPDATA%\WattMail\cache.db` — `messages` + `sync_state` tables; ops run on
  `spawn_blocking` so the async runtime never stalls.
- Application: `sync_inbox` (provider→store, persists deltaLink + cached account) and
  `cached_inbox` (read from store). Commands `inbox_from_cache` + `sync_inbox` replace the
  old live `load_inbox`; `mark_read` now updates remote **and** cache.
- UI reads the cache instantly on boot/refresh, then syncs in the background and re-renders —
  offline-capable, no live Graph round-trip just to repaint the list.
- Verified: builds (incl. bundled SQLite), `npm run build` clean, `clippy --all-targets
  -D warnings` clean. **Live `tauri dev` sync round still pending.**

### 2026-06-16 — Reader UX: resize, mark-read, load-images, external links
- Draggable splitter between list/reader panes (width persisted, 260–640px).
- Clicking a message marks it read (Graph `PATCH /me/messages/{id}` `{isRead:true}`,
  optimistic UI) — `MailProvider::mark_read`, command `mark_read`.
- "Images blocked" banner is now a button → re-fetches with `allow_images=true`
  (sanitizer keeps `<img>`; CSP `img-src` allows `https:`/`http:` so the sandboxed
  frame loads them). Direct load reveals IP to senders — backend image proxy still future.
- Links in the email open in the **system browser** (opener plugin): the frame is
  `sandbox="allow-same-origin"` (still no scripts), and the parent intercepts anchor
  clicks → `openUrl`, so the user sees the real destination rather than navigating in-app.
- Verified: builds, `npm run build` clean, `clippy --all-targets -D warnings` clean.

### 2026-06-16 — Reading pane + HTML sanitization — built
- Split layout: message list (left) + reading pane (right); click a message to read it.
- New Graph fetch `GET /me/messages/{id}` (id encoded as a path segment) → full body.
- **Sanitization in Rust** (`infrastructure/html.rs`, ammonia): strips scripts, event
  handlers, `style`/`<style>`, and **all images** (closes the tracking-pixel vector);
  links kept but inert. Body rendered in a **sandboxed `<iframe sandbox="">`** — no
  script execution, opaque origin, no network egress. "Remote content blocked" banner
  shown when the original had images.
- New command `load_message`; domain `MessageBody` + `MailProvider::message`.
- Verified: workspace builds, `npm run build` clean, `clippy --all-targets -D warnings`
  clean. Live `npm run tauri dev` click-through still pending.

### 2026-06-16 — Milestone 2: Tauri shell (UI-first) — built
- Vite + TS + Tailwind + DaisyUI frontend; inbox list, sign-in view, settings modal.
- Light/dark/system theme (pre-paint script avoids flash), tray icon (Show/Settings/Quit),
  close-to-tray, window hidden until painted.
- `src-tauri` commands: `is_signed_in`, `sign_in`, `sign_out`, `load_inbox`,
  `get/set_close_to_tray`. Reuses `AuthService` + `GraphClient` from infrastructure.
- Verified: `npm run build` clean, `cargo build -p wattmail-desktop` compiles,
  `clippy --all-targets -D warnings` clean. **Live `npm run tauri dev` run still pending (needs user).**

### 2026-06-15 — Milestone 1: Auth spike — done (verified live)
- End-to-end OAuth (public client + PKCE + loopback) → token exchange → Graph read.
- Verified live: first run browser consent + prints profile and 10 inbox messages;
  second run refreshes silently with no browser.
- Fixed: Windows Credential Manager 2560-char limit by storing only the (chunked)
  refresh token; access tokens stay in memory.

### Milestones

| # | Milestone | State |
| --- | --- | --- |
| 1 | Auth spike (OAuth + Graph round-trip) | ✅ done, verified live |
| 2 | Tauri shell — inbox list, themes, tray (UI-first) | ✅ done, verified |
| 3 | Sync engine + SQLite cache behind `MailProvider` | ✅ built, live run pending |
| — | Message reading pane + **HTML sanitization** (ammonia, sandboxed iframe, images stripped) | ✅ done, verified |
| — | Reader UX: resizable split, mark-as-read, load-images, external links | ✅ done, verified |
| — | **Rich body rendering** — safe inline-CSS allowlist (tables, colours; e.g. SwatBox report ticks) | ✅ done |
| — | Nested folders + Sent/Drafts recipient display | ✅ done |
| — | Compose / reply / forward / send (`sendMail`) | ✅ done |
| — | Attachments — view/download received, attach on compose | ✅ done |
| — | Image proxy (server-side inline), cache encryption (AES-256-GCM), sort | ✅ done |
| — | Rich-text compose; GitHub repo + CI/release pipeline | ✅ done |
| — | Auto-update (Tauri updater, signed rolling-release `latest.json`); repo public | ✅ done |
| — | Cross-platform pass — config dir abstraction + CI compile-gate (macOS/Linux) | 🟡 config dir + CI done; live macOS/Linux run pending |
| — | Headers viewer — view & trace internet headers, auth (SPF/DKIM/DMARC) badges, forged-`To:` caution | ✅ done (v0.1.9–0.1.10) |
| — | Quick wins — search, drafts, follow-up flags, cached folder sidebar, keyboard shortcuts | ✅ built (v0.1.11); live run pending |
| — | Compose polish — resizable/maximizable window, sanitized rich-HTML paste, inline images (cid) | ✅ built (v0.1.12); live run pending |
| — | Calendar tab (read agenda + accept/decline; `Calendars.ReadWrite`) | ⬜ backlog (roadmap big-bet, v0.3.0) |
| — | Contacts / recipient autocomplete (`People.Read`/`Contacts.Read`) | ⬜ backlog (roadmap v0.2.0) |
| — | Second provider (IMAP/SMTP) behind the contract | ⬜ backlog |

---

## Architecture decisions

| Date | Decision | Rationale | Status |
| --- | --- | --- | --- |
| 06-15 | **Graph API first**, behind a provider-agnostic `MailProvider` trait | Fastest path to a working O365 client (conversations, delta sync, calendar/contacts free). Contract keeps IMAP/portability open. | Active |
| 06-15 | **OAuth public client + PKCE + loopback** (`http://localhost`), single-tenant | Recommended desktop pattern; a distributed binary can't protect a secret. Single-tenant = simplest for own mailbox. | Active |
| 06-15 | **Token exchange via raw form-posts**, isolated in `infrastructure/auth`, not the `oauth2` crate | Transparent, fewer moving parts, mirrors Mailspring's handshake; swappable later without touching callers. | Active |
| 06-15 | **Sync = delta-query polling** (`/messages/delta` + persisted deltaLink) | Graph change-notification webhooks need a public HTTPS callback — impractical for a desktop app. | Active (to implement in M3) |
| 06-15 | **Persist only the refresh token, chunked across keyring entries** | Entra refresh tokens (~2.5–3.5 KB) exceed the Windows Credential Manager 2560-char limit; access tokens are short-lived → keep in memory. | Active |
| 06-16 | **UI-first sequencing** (Tauri shell before sync engine) | Visible payoff + exercises the Tauri build pipeline early; the `MailProvider` seam makes the later cache swap invisible to the UI. | Active |
| 06-16 | **Stack mirrors AllTheThings**: Vite/TS/Tailwind/DaisyUI, vanilla TS, window-hidden-until-painted | Proven fast-startup Tauri setup the user already likes. | Active |
| 06-16 | **All networking stays in Rust; webview does IPC only; CSP stays locked** | OAuth runs in the system browser, Graph calls in Rust — the frontend never needs a Graph origin, shrinking the webview attack surface. | Active |
| 06-16 | **Email sanitization: ammonia (strip scripts/styles/images) + sandboxed `<iframe sandbox="">`** | Email HTML is hostile; stripping images closes tracking pixels without per-URL CSS filtering; `sandbox=""` blocks scripts and gives an opaque origin. Per-image opt-in + CSS sanitization are future work. | Active |
| 06-16 | **Sync = provider-agnostic `sync(token)` returning an opaque cursor; UI reads cache-first** | Keeps the contract portable (Graph delta now, IMAP UID/modseq later); the SQLite cache makes refresh instant/offline and decouples the UI from the network. | Active |
| 06-16 | **Send via `/me/sendMail` with a client-composed reply** (not Graph `/reply`) | One send path + full edit control over recipients/subject/body. Trade-off: no `In-Reply-To`/`References` headers, so replies don't thread server-side — deferred. | Active |
| 06-16 | **Email body always rendered on white** (app chrome stays themed); **auto-sync every 60s** | Email HTML assumes a light background — authors' dark/grey text is unreadable on a dark body, and per-email inversion is unreliable. Auto-sync keeps the list current without manual Refresh. | Active |
| 06-16 | **Auto-update via Tauri updater against the rolling release; repo made public** | An unauthenticated updater can't pull assets from a private repo; minisign-signed `latest.json` keeps trust without an auth token. Verified no secrets in WattMail or any public repo first. | Active |
| 06-17 | **Search = Graph server-side `$search` (live), not local FTS** | The encrypted cache uses a per-value random nonce, so content columns can't be queried/sorted in SQL — local FTS is impossible without weakening encryption. Graph `$search` needs no new scope and reuses the existing message decoder. Trade-off: search is online-only and returns by relevance (sorted client-side). | Active |
| 06-17 | **Drafts via the dedicated `/me/messages` draft flow, not `sendMail`** | A resumed draft must update + `POST /{id}/send` so it isn't duplicated/orphaned, and must load the **raw** (unsanitized) body for editing — the display-sanitization path is read-only. | Active |
| 06-17 | **Compose paste is sanitized client-side (TS), not via the Rust display sanitizer** | Paste needs an *editing*-oriented allow-list (keep formatting) and must run synchronously at the caret — a Rust round-trip per paste is wrong-shaped. Pasted content is untrusted (hostile-page origin), so it's rebuilt via DOM APIs, never dirty `innerHTML`. | Active |
| 06-17 | **Inline images = `data:` while editing, rewritten to `cid:` inline attachments at send** | Keeps the editor simple (data URLs render under `img-src data:`); only the send path converts to Graph `isInline`/`contentId` attachments. Drafts keep `data:` in-body but are blocked from sending (draft attachments deferred). `dragDropEnabled:false` lets the webview get DOM drop events. | Active |

---

## Open questions / deferred

- **HTML email rendering — image privacy & fidelity.** "Load images" now **proxies** images
  server-side and inlines them as `data:` (no remote loads, tight CSP). Residual: a local fetch
  doesn't hide the IP (needs a remote relay); CSS `url()` backgrounds and `<style>`-block CSS are
  still stripped (only inline `style` honoured); image fetch is sequential (slow for big newsletters).
  - **✓ Resolved (2026-06-16):** inline CSS is now kept via a property allowlist (`infrastructure/
    html.rs`), so the SwatBox table/ticks render. Residual: CSS `url(...)` backgrounds are always
    stripped (even with "load images"), and `<style>`-block CSS is still dropped — only inline
    `style` attributes are honoured.
- **Cross-platform secrets/config** — config/cache paths now go through `paths::data_dir()`
  (`dirs`-backed, cross-platform) and CI compiles the macOS/Linux gates, but **nothing has been
  run live on macOS/Linux yet** — CI is compile-only (clippy, not a packaged bundle). Residual:
  token chunking stays uniform across platforms (harmless but unnecessary on macOS Keychain /
  Linux Secret Service, which have no 2560-char limit); decide later whether to store the full
  blob off-Windows.
- **Attachment limits** — outgoing attachments ride inline in `sendMail` (~3 MB total Graph limit);
  larger files need an upload session (deferred). Inline images (v0.1.12) also ship as inline
  `fileAttachment`s (`cid:`/`isInline`) under the same ~3 MB cap (enforced client-side at send).
  Only `fileAttachment`s are listed — `itemAttachment` (embedded messages) and `referenceAttachment`
  (links) aren't shown. Outgoing MIME type is guessed from the file extension.
- **Reply threading** — replies go via `sendMail`, so they don't set `In-Reply-To`/`References`
  and won't thread into the conversation in the recipient's client (the `Re:` subject groups
  loosely). Still deferred. (Rich-text compose, attachments, an editable quoted original,
  **drafts**, a **resizable/maximizable** window, **sanitized rich-HTML paste** and **inline
  images** are all done — v0.1.11–0.1.12. Drafts deferral: file *and* inline-image attachments
  aren't saved on a draft yet; compose warns/blocks rather than dropping them silently.)
- **Multi-account model** — domain currently assumes a single account; the SQLite cache is
  single-account (one `cache.db`, no account column).
- **Cache at rest** — content columns + sync-state values are AES-256-GCM encrypted (key in the OS
  keychain). Residual: `id`, `folder_id`, `received`, and `is_read` are plaintext (needed for
  sort/filter); whole-DB encryption would need SQLCipher (heavier Windows/OpenSSL build).
- **Sort scope** — sort is client-side over the loaded window. The window now grows via "Load more"
  (`PAGE_SIZE`=50 per page) instead of a fixed 50, so the user can extend it across the whole cached
  folder; but a given sort still only orders the *loaded* rows (e.g. "Oldest" = oldest of what's
  loaded). Encryption blocks SQL-side content sorting; a server-side date sort would make sort
  whole-folder without loading everything.
- **First sync pulls the whole folder** — delta enumerates the entire folder (paged), not just
  recent N. Fine for now; may want a bound/older-mail strategy for very large mailboxes.
- **Folder polish** — (a) default-Inbox (and the Drafts folder, for draft-resume) is matched by
  English `displayName` (locale-fragile; use the well-known folder id later); (b) ✓ resolved
  (v0.1.11): the folder sidebar is now cached (encrypted `folders` table) and `list_folders` is
  write-through with an offline fallback, so a cold offline start shows folders. (Nested folders
  and Sent/Drafts recipient display were already done.)
- **`Mail-Advanced.ReadWrite` (effective Dec 31 2026)** — editing subject/body/
  recipients of *already-delivered* messages will need elevated scope. Normal
  compose/read/send/move/flag is unaffected — **don't build a feature that rewrites
  received mail in place.**
- **npm audit** — `npm install` flags 2 high-severity advisories in dev-only deps
  (transitive under vite/tauri-cli). Not shipped in the binary; review before release.
- **CI / release** — done. `ci.yml` `verify` (windows-latest) runs fmt + `clippy --all-targets`
  + full `npm run tauri build`; `cross-check` (macos-latest, ubuntu-22.04) runs fmt + clippy to
  compile the non-Windows gates. `release.yml` tags → signed NSIS rolling release. Toolchain
  pinned (`1.96.0`) to match `rust-toolchain.toml`. Residual: no macOS/Linux *bundle* in CI yet
  (compile-only) and no live run on those platforms.

## Constraints / gotchas

- **Windows Credential Manager:** 2560 UTF-16 chars per entry → refresh token is
  chunked (1024 chars/entry) with a count entry. See `infrastructure/auth/token_store.rs`.
- **CSP includes `ws://localhost:* http://localhost:*`** for Vite HMR in dev (matches
  AllTheThings). Harmless in production (nothing at localhost there).
- **CSP `img-src` allows `https:`/`http:`** so opted-in remote email images load in the
  sandboxed frame. Email content is otherwise isolated: `sandbox="allow-same-origin"`
  (no `allow-scripts`) disables email JS while letting the parent intercept link clicks
  and open them externally via the opener plugin.
- **Local cache** is `cache.db` in `paths::data_dir()` (cross-platform; `%LOCALAPPDATA%\WattMail`
  on Windows, `dirs`-backed elsewhere), alongside `settings.json`. `rusqlite` uses the **bundled**
  SQLite (compiled from source via the MSVC toolchain), so there's no system SQLite dependency to
  ship.
- **Release profile** lives at the workspace root (`[profile.release]`); member-crate
  profiles are ignored.
- **`panic = "abort"`** in release — no test relies on unwinding.
