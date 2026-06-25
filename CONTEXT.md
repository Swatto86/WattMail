# WattMail — Living Context

> Progress log, architecture decisions, and open questions for WattMail.
> **Maintenance:** update this at the end of any session with meaningful changes —
> new milestone state, a decision made/reversed, or an open question resolved.
> Keep newest progress entries at the top of the log.
>
> **Last updated:** 2026-06-25

---

## Overview

A personal email client. Initial target: **Office 365 business mailboxes via the
Microsoft Graph API** with OAuth 2.0. The transport sits behind a provider-agnostic
`MailProvider` contract; a generic **IMAP/SMTP** backend is in fact already built on
branch `feature/imap-accounts` (parked off main — see the NOTE in the progress log).

**Platform reality:** only **Windows** is shipped/proven (NSIS installer + signed
auto-update). The code is cross-platform-capable (per-OS path + keychain abstraction)
but macOS/Linux are compile-gated in CI only — never built into a bundle or run live.

**Provider status (v0.1.20):** Office 365 = live/configured (real client+tenant ids in
`accounts.rs`). Outlook.com + Gmail = code-complete but **gated off** by
`REPLACE_WITH_…` placeholder creds (`is_provider_configured()` = false → filtered from
the picker, rejected by `add_account`); a default build offers **only Office 365**.
Generic IMAP/SMTP = **built but parked** on `feature/imap-accounts`.

**Release policy (2026-06-25):** personal, single-user tool — **release once CI is
green** (frontend build + `cargo fmt --check` + `clippy --all-targets -D warnings` +
`cargo test --workspace` + full `tauri build`); **no manual install or live-run test
gate before tagging**. Any live exercise happens *after* release (release-then-test).
Earlier log entries that say "live test pending before release" / "the usual
test-then-release" describe the *prior* policy, now retired.

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

### 2026-06-25 — Distinguished-folder detection by Graph well-known name, not display name (v0.2.2)
Fixes two folder bugs that shared one root cause — folder *role* was guessed from
the **English display name** (`PROTECTED_FOLDER_NAMES` / `isOutgoingFolder` in
main.ts), which both over- and under-blocked:
- A custom folder literally named **"Sent"** (distinct from the real "Sent Items")
  was treated as protected, so its right-click **Delete** was withheld.
- Exchange system folders **not** in the hardcoded English set — the **Sync Issues**
  family (Sync Issues / Conflicts / Local Failures / Server Failures), RSS Feeds —
  were offered a Delete that Graph always rejects with a raw
  `ErrorDeleteDistinguishedFolder` 400, dumped verbatim to the status line. This is
  the footgun the v0.1.23 notes flagged as "no reliable well-known flag in the
  default `$select`" — now resolved.
- **Fix (server truth).** New `FolderRole` enum + `Folder.role` in `domain`. The
  Graph backend resolves the well-known distinguished-folder names to their concrete
  ids in **one `$batch` round-trip** (`well_known_roles()` → pure, unit-tested
  `roles_from_batch()`), tagging each folder by **id**, not name. Best-effort: a
  `$batch` failure degrades to a role-less list (sidebar still renders). Gmail maps
  its system labels to roles too. Role persisted in the SQLite cache (**schema v8**,
  drop-and-rebuild). `FolderDto.role` carries it to the frontend, where protection /
  outgoing-column / draft-resume now key off the role; the English-name set remains
  only as an **offline fallback floor**, with **"sent" removed**. Delete errors map
  to a readable "system folder" line.
- **Verification level:** compile-verified — fmt, `clippy --all-targets -D warnings`,
  `cargo test --workspace` (34 tests, incl. new `FolderRole` round-trip + `$batch`
  decode tests), `tsc` + `vite build`. **Not live-run verified** against a real mailbox
  (can't auth to Graph here) — the `$batch` wire shape and whether the user's "Sent" is
  genuinely a custom (deletable) folder are unconfirmed. **Released as v0.2.2
  (release-then-test)** under the new no-test-gate-before-release policy. Live pass to
  run from the installer: right-click "Sent" → Delete appears & works; Sync Issues no
  longer raw-errors. Both outcomes degrade gracefully (a secretly-distinguished "Sent"
  now shows the friendly message instead of raw JSON); if the `$batch` shape is wrong,
  folders simply carry no role (sidebar still renders) — cut a fix release if so.

### 2026-06-25 — About dialog (v0.2.1)
Frontend-only. Adds the **About dialog** (desktop-capability checklist item): app
name, **version read from the build** (`getVersion()`, not hard-coded), description,
**Developer = Swatto**, **Licence = MIT**, a **github.com/Swatto86/WattMail** source
link (opens in the system browser via `openUrl` on a hard-coded `REPO_URL`), and a
**Check for updates** button reusing the existing updater (`check()` + the banner/
install flow; reports up-to-date / available / offline). Two entry points: an
"About…" row in Settings and clicking the toolbar **WattMail** brand. Wired into the
Esc stack + `aModalIsOpen` shortcut guard; backdrop-click closes; `oklch()` DaisyUI
tokens. `openAbout()` closes Settings first so About never stacks on it (review-found:
otherwise the update banner stays hidden behind Settings). Reviewed (2-lens adversarial
+ per-finding verify): integration clean, 1 medium UX finding fixed. Rolled into the
v0.2.1 release alongside the calendar so there's one build to test.

### 2026-06-25 — Calendar tab over Microsoft Graph (v0.2.0 — released for live testing)
**The app's first multi-view feature.** Adds a Mail/Calendar view switch and a
calendar tab with a rolling 7-day agenda, event detail pane, RSVP, and create-event.

- **Architecture (mirrors `MailProvider`):** new `CalendarProvider` trait in `domain`
  (`CalendarEvent`, `EventDateTime`, `Attendee`, `ResponseStatus`, `NewEvent`,
  `InviteResponse`; reuses `MailError` as the shared provider error). Graph impl in a
  new `crates/infrastructure/src/graph/calendar.rs` submodule (shares `GraphClient`'s
  http/token/`check_status`/recipient helpers, exposed `pub(super)`). Thin application
  use-cases; 5 Tauri commands (`account_supports_calendar`, `calendar_view`,
  `create_event`, `respond_to_event`, `delete_event`) + DTOs. New `src/calendar.ts`
  owns all calendar DOM so `main.ts` is barely touched (view-switch + capability gate).
- **Reads:** `/me/calendarView` (NOT `/me/events` — only calendarView expands
  recurrence), paginated via `@odata.nextLink` (cap 20 pages), `Prefer:
  outlook.timezone="<IANA>"`. Create = `POST /me/events`; RSVP = `POST
  /me/events/{id}/{accept|tentativelyAccept|decline}`; delete = `DELETE`.
- **Scope:** added **`Calendars.ReadWrite`** to both Microsoft OAuth configs
  (`auth/mod.rs`) — **existing users must sign out / in once** to re-consent. Request
  ReadWrite from the start so create/RSVP never triggers a 2nd consent.
- **Time zones (the trap):** events come back as local wall-clock with no offset (via
  the Prefer header) and the frontend parses them as local. **calendarView *window
  bounds* are different** — Graph interprets an offset-less bound as **UTC** and ignores
  the Prefer header for it, so the frontend sends true instants (`Date.toISOString()`),
  DST-safe. All-day events use exclusive next-midnight end and are never zone-shifted.
- **Security:** event bodies are server-sanitized (`sanitize_email`) then rendered in a
  sandboxed `allow-same-origin` (no-scripts) iframe; join/web/body links gated to
  `http(s)` both client (`openExternal`) and server (`http_url`). All networking stays
  in Rust; CSP unchanged. Gmail (mail-only) hides the Calendar tab.
- **Process:** built compile-first, then a **6-lens adversarial review** (19 raised →
  11 confirmed / 8 refuted; 4 lenses independently caught the UTC-bounds bug with Graph
  docs cited — the project's classic "compile-green ≠ correct Graph wire" class), fixed
  all 11, then a **fix-verification pass** (16/16 resolved, 0 regressions). Removed the
  dead `calendars()`/`list_calendars` path (registered IPC nobody called).
- **Verification level:** compile-verified — `clippy --all-targets -D warnings`,
  `cargo test --workspace` (30 tests, incl. new calendar decode/url/tz tests), `tsc` +
  `vite build`, and the **full `tauri build` (CI, windows-latest)** + cross-check matrix.
  Deliberately **released as v0.2.0 for the user to live-test via the published
  installer** (release-then-test, by request) rather than the usual test-then-release —
  the Graph calendar wire code is **not yet live-run verified**. First live pass to run:
  sign out/in (grant `Calendars.ReadWrite`), load agenda, open an event, RSVP, create a
  timed + all-day event, eyeball a recurring series and a non-UTC/BST moment. If a wire
  bug surfaces (the v0.1.13/14 class), cut a fix release.

### 2026-06-24 — Post-review hardening of folder management (v0.1.24)
Ran a verified 4-lens adversarial review of the v0.1.23 diff (Graph contract,
frontend, wiring/layering, edge cases; every finding independently refuted-or-
confirmed). **Graph contract came back clean** — endpoints/payloads correct and
`Mail.ReadWrite` does authorize folder create/rename/delete (no scope gap). 6 of 8
findings confirmed; the two real footguns + two cheap polish items fixed here:

- **Keyboard shortcuts no longer fire behind the folder menu (was: medium).** The
  global shortcut handler suspended itself only for the message menu
  (`!ctxMenu…hidden`); the folder menu was missing from the guard, so with it open
  `#` would delete the cursored message, `c` open compose, etc. Extended the guard to
  cover `folderMenu` too (main.ts).
- **Well-known folders can't be renamed/deleted from the menu (was: medium).** Graph
  *allows* renaming Inbox/Drafts/Sent via `displayName`, which would silently break
  the app's name-based detection (tray unread count, outgoing-column rendering, draft
  resume). `showFolderMenu` now withholds Rename/Delete for a `PROTECTED_FOLDER_NAMES`
  set (inbox/drafts/sent items/sent/outbox/deleted items/junk email/archive/
  conversation history); New folder / New subfolder still offered (main.ts).
- **Deleted folder's cached messages are purged (was: low leak).** New
  `MailStore::forget_folder(folder_id)` (`DELETE FROM messages WHERE folder_id`),
  called from the `delete_folder` use-case after the provider delete succeeds —
  mirrors the provider-then-store write-through of `delete_message`/`move_message`.
- **Duplicate-name error is now readable (was: low UX).** A Graph 409 maps to
  `A folder named "X" already exists here.` instead of dumping raw JSON (main.ts).
- **Accepted (low, self-healing), documented not fixed:** a deleted/renamed folder can
  momentarily reappear from cache *only if* the immediate post-mutation `list_folders`
  refresh also fails (rare; replace-all write-through heals it on the next refresh);
  the deleted folder's `delta:{id}` token row is left in `sync_state` (inert — folder
  ids are immutable, never re-read); descendant folders of a deleted parent aren't
  cache-purged. The Gmail "offers unsupported folder ops" finding was **refuted** —
  unreachable (Gmail gated off main).
- **Verified:** clippy `--all-targets -D warnings`, fmt, `cargo test --workspace`
  (19 infra + 2 desktop-lib), `npm run build` all clean. Still not exercised live.

### 2026-06-24 — Right-click folder management: add / rename / delete (v0.1.23)
The folder sidebar now has a custom right-click context menu for managing folders,
mirroring the message-row menu pattern exactly (same `.ctx-*` classes — no new CSS).

- **Menu:** right-click a folder → **New folder… / New subfolder… / Rename… /
  Delete folder**; right-click empty sidebar space → just **New folder…**. Reuses the
  message menu's viewport-clamped placement and the click-away / Escape / blur dismiss;
  the folder and message menus cross-hide so only one is ever open.
- **Backend (defaulted trait methods, Graph-only impl).** `MailProvider::create_folder
  (name, parent_id) / rename_folder(id, name) / delete_folder(id)`, all defaulting to
  `Unsupported` so Gmail/IMAP (parked/gated) compile unchanged. Graph impl:
  `POST /me/mailFolders` (top level) or `…/{parent}/childFolders` (subfolder) with
  `{displayName}`; `PATCH /me/mailFolders/{id} {displayName}`; `DELETE /me/mailFolders/
  {id}`. New `folder_endpoint(id)` URL helper mirrors `message_endpoint` (opaque id as a
  path segment). Application use-cases `create_folder`/`rename_folder`/`delete_folder`
  are thin provider passthroughs; commands of the same names wired in `lib.rs`.
- **Frontend flow.** `window.prompt` for the name (same idiom as compose's link prompt),
  `window.confirm` before delete. After any mutation the sidebar refreshes via the
  existing `loadFolders()` (live list → write-through cache → re-render), which also
  recomputes folder depths. Deleting the open folder falls back to Inbox/first and loads
  it. Errors surface in the status line.
- **Notes / deferrals (deliberate):** Graph rejects deleting well-known folders (Inbox,
  Sent Items, …) with an error that surfaces to the status line — no client-side guard
  (no reliable well-known flag in the default `$select`). A deleted folder's orphaned
  cached message rows + delta token remain in SQLite but are unreachable (no sidebar
  button) and clear on the next disposable-cache rebuild. Gmail returns `Unsupported`.
- **Verified:** clippy `--all-targets -D warnings`, fmt, full `cargo test --workspace`
  (19 infra + 2 desktop-lib), and `npm run build` (tsc + vite) all clean. Folder
  create/rename/delete are **write** Graph calls, so not exercised live (can't safely
  test deletion against the real mailbox); the URL-building mirrors tested helpers.
  End-to-end click-through in a running build pending.

### 2026-06-24 — Attachment indicator + "has attachments" filter (v0.1.22)
A new quick filter and a per-row paperclip indicator so attachments are visible at
a glance. Plumbs a `has_attachments` boolean through every layer, mirroring
`is_flagged` exactly.

- **Signal = Graph `hasAttachments`, verified clean.** Checked live: of the 40
  newest Inbox messages only 2 reported `hasAttachments=true` — exactly the two with
  real attachments (a PDF receipt, a CV `.docx`), both non-inline; every newsletter
  with inline images reported `false`. So Graph's `hasAttachments` excludes inline
  images (matching the reader's non-inline attachment list), making the indicator
  accurate, not noisy. No per-message attachment fetch needed.
- **Plumbing (mirrors `is_flagged`):** `MessageSummary.has_attachments` (domain);
  `hasAttachments` added to all four Graph `$select` strings (list/search/delta/
  fetch_older) + `GraphMessage`/`DeltaItem` decode + the delta upsert and
  `From<GraphMessage>`; cache **schema 6 → 7** (new `has_attachments` column, plaintext
  for filtering, disposable-rebuild repopulates it); `MessageDto.has_attachments`;
  frontend `Message.hasAttachments`. A flags-only delta (read/flag toggle) still goes
  through `FlagsChanged` (column-only update), so it never resets `has_attachments`.
- **UI:** a paperclip (inline SVG, `currentColor`) sits in the row's date/flag cluster
  when `hasAttachments`; a 4th segmented-filter button ("with attachments", paperclip
  icon, `data-filter="attachments"`) joins All/Unread/Flagged — client-side over the
  loaded window like the others, persisted in `localStorage`, and applies to search
  results too. Gmail (parked/gated) sets `has_attachments=false` for now (MIME-part
  detection deferred until it's un-parked).
- **Verified:** clippy `--all-targets -D warnings`, 19 infra tests, fmt, `npm run build`
  all clean; Graph `hasAttachments` semantics confirmed read-only against the live
  mailbox; adversarial multi-lens review of the diff. End-to-end click-through in a
  running build not yet done.

### 2026-06-24 — On-demand backfill of older mail (v0.1.21)
Fixed "Outlook shows far more mail than WattMail." Verified against the live mailbox:
the Inbox has **10,262** messages server-side but WattMail cached only **64**.

- **Root cause (a long-standing wrong assumption):** WattMail populates each folder
  *only* from Graph's `/messages/delta`, and that query's initial sync returns just
  **`$top` of the most-recent messages, then issues a `deltaLink` and stops** — it does
  **not** enumerate the whole folder. The delta URL hard-codes `$top=50`, so each folder
  only ever cached ~50 recent messages (Inbox had 64). **Proven empirically:** `$top=50`
  delta → exactly 50 returned; `$top=500` → 500; both reach `deltaLink`. The regular
  `/messages` endpoint *does* page the full folder (paged it past 750, back to January).
  This **supersedes the v0.1.5 note** ("the cache was never capped … delta pages the
  whole folder; 50 is just a read-side window") — that was wrong; 50 was a real sync cap.
- **Fix — on-demand server backfill (no schema change).** New
  `MailProvider::fetch_older(folder_id, before, limit)` (default returns none; Graph
  impl GETs `/me/mailFolders/{id}/messages?$filter=receivedDateTime lt {before}
  &$orderby=receivedDateTime desc&$top=…` — the regular endpoint, which reaches history
  the delta window can't). New `MailStore::oldest_received` anchors it; application
  `load_older` = oldest → fetch_older → upsert; command `load_older` backfills a page
  then returns the grown cache window. Frontend "Load more" now widens from cache while
  rows remain, then backfills older history from the server (preserving scroll), until a
  backfill returns nothing (`reachedOldest`) — so you can page back through the entire
  folder. `lt {oldest}` paging verified live: 0 overlap with the cached window, no gap.
- **Verified:** new `graph::tests` for path-encoding (19 infra tests green), clippy
  `--all-targets -D warnings`, fmt, `npm run build` all clean, and the exact `fetch_older`
  Graph query exercised read-only against the live mailbox. End-to-end click-through in a
  running build not yet done (backend wire query is the verified-risky part).

### 2026-06-24 — Fix delta sync clobbering cached mail with placeholders (v0.1.20)
Diagnosed and fixed the **"(unknown) / (no subject) / UNKNOWN DATE"** rows that
appeared at the bottom of folder lists. They were **not** non-mail or stale items —
each was a real, intact message on the server (confirmed by a read-only Graph `GET`:
e.g. a phantom row decoded to *"8 new Technical Services Consultant jobs"* from
`info@jobs.totaljobsmail.com`, received 18 Jun). The live cache held 24 such rows
across 4 folders, all with identical encrypted field lengths — i.e. all overwritten
with the same `(no subject)` / `(unknown)` / empty-date fallbacks.

- **Root cause — Graph delta *partial-property* clobber.** Microsoft Graph's
  `/messages/delta` feed reports a flag change (e.g. a message marked read) by
  returning **only the id plus the changed scalar** — no `subject`/`from`/
  `receivedDateTime`/`bodyPreview`, even though `$select` requests them. `DeltaItem`
  has all-`Option` fields, so such a notification deserialized cleanly and
  `GraphClient::sync` pushed it as a full `Upserted`, whose `unwrap_or` fallbacks
  then **overwrote the cached row's real content** with placeholders (empty
  `received` → JS `new Date("")` = NaN → the "Unknown date" section). The non-delta
  list/search paths can't hit this — `GraphMessage::is_read` is a required `bool`,
  so they only parse full message objects.
- **Fix — discriminate the notification from a real message.** New
  `DeltaItem::is_flags_only_change()` (no subject/from/date/preview) routes these to
  a new `MessageChange::FlagsChanged { id, is_read, is_flagged }` variant;
  `application::sync_folder` applies only the present flags via the existing
  column-only `store.set_read`/`set_flag` (a no-op if the row isn't cached), so
  cached content is **preserved, never overwritten**. Real messages (any content
  field present) still take the `Upserted` path unchanged. Removed-tombstone
  handling (`@removed`) is untouched.
- **Heal — schema bump 5 → 6** (no schema change): forces the disposable cache to
  drop-and-rebuild once on next launch, discarding the already-corrupted rows; the
  full re-enumeration re-fetches their real content. Gmail is unaffected (its History
  sync re-fetches each changed message in full) and is gated off on `main` anyway.
- **Verified:** 4 new `graph::tests` (partial vs full vs date-only vs tombstone) +
  full suite 18/18 green; `cargo clippy --all-targets -D warnings` clean. Diagnosis
  grounded in the live cache (`%LOCALAPPDATA%\WattMail\cache.db`) + a read-only Graph
  round-trip, not inference.

### 2026-06-23 — Outlook-style date sections + quick filters (v0.1.19)
Frontend-only message-list upgrade (no backend/wire changes): the inbox now groups
into **Outlook-style date sections** and gains **quick filters**, on top of the
existing client-side sort.

- **Date sections** (`src/main.ts`): when sorting by date (the default), the list is
  grouped under sticky headers — **Today, Yesterday, This Week, Last Week, This Month,
  Last Month**, then **"Month YYYY"** for older mail. `dateSectionLabel()` buckets a
  message relative to now (week starts Monday); buckets are mutually exclusive and
  consecutive in date order, so `renderListBody()` emits one header per run.
  Verified deterministically (12-case bucketing + consecutive-no-repeats harness).
  A **group-by-date toggle** (☰ in the toolbar) turns sections off; disabled for the
  non-date sorts (Sender/Subject/Unread). Persisted in `localStorage` (`wattmail.group`).
- **Quick filters** (`#filter-seg`): **All / Unread / Flagged**, client-side over the
  loaded window, in both the folder view and search results (search header shows
  "X of Y" when filtered). Persisted (`wattmail.filter`). The keyboard cursor (j/k)
  skips section headers (not `.msg`); rows get `scroll-margin-top` so the sticky
  header never hides the cursored row.
- **Sort** unchanged (Newest/Oldest/Sender/Subject/Unread); sort/filter/group all
  re-render from cache via a shared `rerenderList()` — no refetch.
- Verified: `tsc` + `vite build` + `cargo check` + fmt clean; bucketing logic
  unit-verified. Pure client-side rendering of already-cached data — none of the
  v0.1.13/14 (Graph wire-type) live-run risk.

> **NOTE — IMAP/OAuth work is parked.** The generic IMAP/SMTP backend +
> Mailspring-style account setup + Gmail-sync fixes + OAuth credential injection
> (the "IMAP / SMTP — design" section below, now *built*) live on branch
> **`feature/imap-accounts`** (CI-green), deliberately kept off `main`/releases until
> live-tested with a real app-password account. `main` is intentionally back at the
> v0.1.18 base for this release. To resume: merge that branch, live-test, then ship.

### 2026-06-23 — Post-review fixes: Graph delta-expiry recovery + sync races (v0.1.18)
Ran a verified multi-agent codebase review (status + a 6-dimension review with
adversarial verification of every finding + feature-gap analysis + IMAP/SMTP
design). Fixed the findings that survived verification **on the live Office 365
path**, then cut v0.1.18. Sequencing chosen: fix shipping bugs first → release →
then begin IMAP. The full review + IMAP design is captured under "Review findings
(2026-06-23)" and "IMAP / SMTP — design" below.

- **Graph `deltaLink` expiry (HTTP 410) now recovers** instead of breaking a
  folder's sync *permanently*. Previously `GraphClient::sync` propagated the 410;
  `application::sync_folder` bailed before persisting, leaving the dead cursor in
  `sync_state`, so every later sync of that folder failed forever (silent under
  the 60s auto-sync). Now: on a 410 while resuming from a stored cursor, discard
  the dead cursor + partial accumulation and re-run a fresh full enumeration once
  (a `recovered` flag prevents a loop; only fires when a stored token was used).
  Converges via upsert-by-id. Mirrors Gmail's 404→`full_sync` fallback. (graph/mod.rs)
- **Folder-switch render race** — `refreshFromCache` now snapshots the folder id
  and bails before `renderInbox` if it changed during the in-flight cache read,
  so folder A's rows can't paint under folder B's selection. Mirrors the existing
  `openMessage`/search epoch guards. (main.ts)
- **Dropped-sync race** — `syncFolder` no longer silently no-ops when a sync is
  in flight; it sets `pendingSync` and re-runs once the current sync finishes, so
  a freshly-selected folder still pulls server changes rather than waiting up to
  60s for the next auto-sync tick. (main.ts)
- **Production CSP tightened** — the Vite-HMR `ws://localhost:* http://localhost:*`
  allowances moved to Tauri v2 `devCsp`; the shipped `connect-src` is now
  `'self' ipc: http://ipc.localhost`. (tauri.conf.json)
- **Verified:** `cargo fmt --check`, `clippy --all-targets -D warnings`,
  `cargo test` (14/14), `tsc --noEmit`, `vite build`, and a full **release**
  build all clean. Live click-through still pending (no signed-in window run this
  session). **Toolchain is now present locally** (Rust 1.96.0 + Node 24) — the
  historical IMAP blocker (no toolchain to regenerate `Cargo.lock`) is gone.

### 2026-06-22 — Compile/verify + provider gating + release (v0.1.17)
First build of the v0.1.16/v0.1.17 work on a machine with a Rust toolchain (the
prior session had none), then cut the release.

- **Fixed two clippy `-D warnings` blockers** the no-toolchain session couldn't
  catch: an `unnecessary_cast` (`as i64` on an already-`i64`) in
  `gmail/mod.rs::civil_from_days`, and an unused `wattmail_domain::MailProvider`
  import in `accounts.rs`. Truncation/corruption from the prior session left no
  damage — the whole workspace compiles, `clippy --all-targets -D warnings` is
  clean, `cargo test --workspace` passes, `tsc`/`vite build` clean.
- **Provider gating (the release decision).** Outlook.com + Gmail still ship with
  `REPLACE_WITH_…` OAuth placeholders, so they can't sign in. Rather than ship a
  picker with two dead options, the **picker now only offers configured
  providers**: `ProviderKind::ALL`/`tag()` (infra), `is_real_credential` +
  `configured_provider_tags()` (accounts.rs), a `configured_providers` Tauri
  command, and a frontend filter in `pickProvider` (auto-selects when exactly one
  provider is configured → Office 365 alone shows no chooser). `add_account` also
  rejects an unconfigured provider defensively. Two unit tests lock it in. **When
  the Outlook.com/Gmail credentials are filled, they reappear automatically** —
  no further UI change needed.
- **Released v0.1.17** (Office 365 only, functionally). v0.1.16 (multi-account)
  ships under the same tag — it was never separately released (rolling-release =
  latest only).

### 2026-06-20 — Multiple providers: Outlook.com + Gmail (v0.1.17)
Generalized the single-provider (Office 365) stack into a pluggable
provider abstraction and added two backends, building on the `AccountManager`.

- **Provider abstraction.** New `ProviderKind` (infra) = `Office365` /
  `OutlookConsumer` / `Gmail` (serde `snake_case`, default `Office365` so legacy
  records load unchanged). `build_mail_provider(kind, token) -> Box<dyn MailProvider>`
  is the runtime factory. `OAuthConfig` is now provider-neutral (explicit
  authorize/token endpoints, optional `client_secret`, `extra_authorize_params`)
  with `office365` / `outlook_consumer` / `google` constructors; `AuthService`
  drives the same PKCE loopback flow for all, appending the client secret at the
  token endpoint only when present (Google installed-app), and the extra authorize
  params (Google `access_type=offline` + `prompt=consent`).
- **Server-side rules moved onto the trait.** The four Graph `messageRule`
  methods + `supports_message_rules()` are now `MailProvider` methods with
  defaults (empty list / `MailError::Unsupported` / `false`); only `GraphClient`
  overrides them (returns `true`). Non-Exchange providers degrade cleanly and the
  UI hides the Rules row for them.
- **Outlook.com / Hotmail (consumer).** Reuses `GraphClient` verbatim — `/me/*`
  is identical for personal MSAs — with a `consumers`-tenant OAuth config and
  reduced scopes (no `MailboxSettings.ReadWrite`; personal accounts have no rules).
- **Gmail (`crates/infrastructure/src/gmail/mod.rs`, ~1200 lines).** New
  `GmailClient: MailProvider` over the Gmail REST API: labels↔folders, base64url
  MIME bodies (reuses `crate::html::sanitize_email`), History-API incremental
  sync with full-list fallback, label-mutation read/flag/move, trash-as-delete,
  hand-built RFC 5322 MIME for send/draft. **No new crates** (reqwest/serde/
  base64/url only — deliberately hand-rolls date math + MIME to avoid chrono /
  mail-builder and a `Cargo.lock` change).
- **Account model.** `AccountRecord` gained `provider`; keyring/cache namespaces
  are `"{slug}:{id}:refresh-token"` / `cache-{slug}-{id}.db` (the adopted legacy
  Office 365 mailbox still uses `office365:refresh-token` + `cache.db`).
  `add_account(provider)` runs that provider's OAuth + identity discovery via the
  factory. Same-mailbox re-sign-in is still a silent in-place refresh (matched by
  id or case-insensitive email) — confirmed, no dialog. `AccountSummary` carries
  `provider`/`provider_label`/`supports_rules`. Commands resolve a
  `Box<dyn MailProvider>` per active account.
- **Frontend.** "Add account" (welcome / toolbar / settings) opens a provider
  picker (Office 365 / Outlook.com / Gmail) before launching OAuth; account rows
  and the switcher show a provider chip; the Rules row is hidden unless the active
  account supports rules.
- **OAuth app registrations required (placeholders shipped).** `accounts.rs`
  holds the public client ids: Office 365 works as-is; **Outlook.com needs an
  Entra app that allows personal accounts**, and **Gmail needs a Google Cloud
  "Desktop app" client id + secret** — both are `REPLACE_WITH_…` placeholders and
  must be filled before those providers work in a release.
- **Yahoo — deliberately staged (next pass).** Yahoo means IMAP + SMTP (XOAUTH2),
  a distinct transport with the largest surface, and it requires new crates
  (`imap`, `lettre`, `mail-parser`) → a `Cargo.lock` update that can't be
  generated without a Rust toolchain in this environment, plus Yahoo OAuth-for-IMAP
  app approval. `ProviderKind` leaves the seam so it's purely additive.
- **Verification.** Frontend `tsc --noEmit` is clean (0 errors). Rust reviewed by
  subagent against the trait/signatures (no compiler here) — no blocking issues
  after restoring a tail-truncation in `commands.rs` (see note below). **Run
  `cargo fmt && cargo clippy --all-targets -- -D warnings` before release.**
- **⚠️ Environment note.** Mid-session the editor/sync layer truncated several
  source files' tails (and corrupted `.git/index`). All were restored from the
  real on-disk files; if anything looks off after pulling, re-check `commands.rs`,
  `src/main.ts`, and `crates/infrastructure/src/lib.rs`, and rebuild the git index
  (`del .git\index.lock & del .git\index & git reset`).

### 2026-06-20 — Multiple accounts / mailboxes (v0.1.16)
WattMail was single-account: one `AuthService` and one `SqliteStore` were created
in the composition root and `.manage()`d directly, and every Tauri command pulled
`State<AuthService>` / `State<SqliteStore>`. You can now add and switch between
several Office 365 mailboxes, each fully isolated.

- **`AccountManager` (new `src-tauri/src/accounts.rs`) is the composition root.**
  It owns a `Vec<Arc<ManagedAccount>>` (each = `AuthService` + `SqliteStore` +
  durable `AccountRecord`) plus an `active_id`, behind a single `RwLock`. The list
  and active selection persist to `accounts.json` (atomic temp-file + rename, like
  `settings.json`). Commands now take `State<AccountManager>` and resolve the active
  account via a shared `active_provider()` helper. Guards are never held across an
  `.await` — `add_account` does its network work (login, `/me`) with no lock held,
  then takes a brief write lock to register.
- **Per-account credential + cache isolation.** `TokenStore` is now namespaced by a
  keyring `prefix` (meta = `prefix`, chunks = `prefix:i`), and `AuthService::new`
  takes that prefix. New accounts use `office365:<oid>:refresh-token` + a
  `cache-<oid>.db` file; the shared AES-256-GCM cache key (`cache-key`) is unchanged,
  so all per-account DBs stay readable.
- **Account identity.** `UserProfile`/`GraphUser` gained the Entra object id
  (`oid`), selected from `/me`. `add_account` runs an interactive login on a
  throwaway service, reads `/me` to learn the stable id + email, then persists the
  tokens under that account's own namespace. Re-signing into an existing mailbox
  (matched by id **or** case-insensitive email) refreshes it in place instead of
  duplicating it.
- **Legacy adoption — zero migration.** A pre-multi-account install is adopted on
  first launch under id `default`, reusing the original keyring entry
  (`office365:refresh-token`) and `cache.db` verbatim, so the existing mailbox keeps
  working with no re-sign-in. Identity is backfilled from the legacy cache.
- **Frontend.** Toolbar account label is now a switcher dropdown (list accounts,
  switch, "Add account…", caret only when >1). Settings replaces the single
  "Sign out" with an accounts manager (per-account Switch / Remove + Add account).
  Switching resets all per-account view state (folders, cursor, search, reader)
  before loading the new mailbox. Commands `sign_in`/`sign_out` were replaced by
  `list_accounts` / `add_account` / `switch_account` / `remove_account`;
  `is_signed_in` now means "≥1 account".
- **Follow-up (out of scope here):** shared/delegated mailboxes (same credentials,
  Graph `/users/{id}` base + `Mail.Read.Shared`) — the `/me`-relative `GraphClient`
  and the `AccountRecord` model leave room for this as a later, additive change.
- **Verification.** Frontend `tsc --noEmit` shows no new errors vs. the committed
  baseline (the 14 reported are pre-existing, environment-only "unused import"
  noise from Tauri-API type resolution in the sandbox). The Rust workspace was not
  compiled locally (no toolchain in this environment); changes were reviewed against
  the application/domain signatures and rely on CI's `cargo fmt` + `clippy -D warnings`
  compile-gate. **Run `cargo clippy --all-targets -- -D warnings` before release.**

### 2026-06-19 — Adaptive dark-mode email-body rendering (v0.1.15)
Email bodies used to render on a **forced white background regardless of theme**
(`wrapEmailHtml` hardcoded `#1a1a1a` on `#ffffff`), so in the default dark theme
every message was a jarring full-bleed white slab. Replaced with an **adaptive
("smart") strategy** — accuracy preserved for designed mail, readability for
plain mail in both themes:

- **Rust classifier (theme-independent, cache-safe).** `sanitize_email` now also
  returns `Sanitized.is_designed` (→ `MessageBody.is_designed` → `MessageViewDto.designed`,
  serde camelCase `designed`). `has_own_background` scans the *raw source* for a
  `bgcolor=` attribute or an inline `background[-color]:` whose value is real and
  **not pure white** (`#fff`/`#ffffff`/`white`/`rgb(255,255,255)`/transparent/
  inherit/none → ignored). Rationale: designed/marketing mail almost always sets at
  least one non-white background (header bar, card, coloured cell); plain mail sets
  none or only restates the white canvas. **Pure-white is treated as *not* designed**
  — mirrors Apple Mail treating a white background like no background — so ordinary
  Outlook white-`bgcolor` mail still adapts in dark mode. Conservative: biased to
  `true` (the safe failure is the light card, never unreadable). 8 unit tests in `html.rs`.
- **Frontend render (`src/main.ts`).** `wrapEmailHtml(inner, {adapt, bg, fg})` now
  builds three cases: **(a) designed (any theme)** and **(b) plain + light** keep the
  existing light "paper card" (`#1a1a1a` on `#ffffff`); **(c) plain + dark** renders
  on the resolved DaisyUI surface (`oklch(var(--b1))`/`--bc`, read once via a probe
  span in `readThemeColors`), with the default link colour lifted to ≥3:1.
- **Per-element colour repair (`adaptPlainEmail`, plain+dark only).** On the iframe
  `load` event, walks `[style], font[color]` (cap 4000, skips media tags) and, using
  **WCAG relative-luminance contrast**: KEEPs author colours already ≥4.5:1 (links
  ≥3:1) — they're readable and intentional; STRIPs near-neutral dark text (HSL S<0.15,
  e.g. `#000`) so it inherits the theme foreground; LIFTs chromatic colours by HSL
  lightness (12-step binary search, hue preserved) until they clear the gate. Skips
  any element on a light *local* background so dark text on a highlight cell isn't
  stranded. Reads/rewrites already-sanitised inline values only — no new tags, no
  remote loads, no sanitiser change.
- **Paper-card framing.** `.reader-frame` base background is now `oklch(var(--b1))`;
  designed/light mail gets `.is-paper-card` (set in `renderReader`) which, **only in
  the dark theme**, frames the white surface with `margin:8px`, `border-radius:8px`
  and a soft shadow so it reads as an intentional sheet, not a slab.
- **Theme-change re-render.** Toggling the theme select or the OS theme (system mode)
  now calls `reRenderOpenMessage()` → `renderReader(lastMessage)` + reload attachments,
  so an open message re-applies the light-card/adapt decision. No network fetch; the
  cached `msg.html` is never mutated (the pass only edits the freshly rebuilt iframe DOM).
- **Verified.** `cargo fmt --check`, `clippy --all-targets -D warnings`, `cargo test`
  (8/8), `tsc --noEmit`, `vite build` all clean. Rendering confirmed **visually in a
  throwaway harness** running the real adaptation logic over representative emails
  (plain reply, chromatic heading, `<font color=#000>`, Outlook white-bg, designed
  newsletter) in **both themes** — plain mail adapts to readable light-on-dark, the
  navy heading lifts to a readable blue, the newsletter stays a framed white card.
- **Out of scope (deliberate):** the compose quoted-original is *not* adapted — it is
  editable, send-bound content; recolouring it would corrupt outgoing mail.

### 2026-06-19 — Fix inbox-rules decode + botched-version release (v0.1.14)
Two bugs from the v0.1.13 batch (which was compile-verified but never run live):

- **Inbox rules "invalid response body"** — the Graph layer modelled `recipientContains` (and `senderContains`, via the wrong `fromAddresses` mapping) as recipient *objects*, but Graph's `messageRulePredicates` defines `senderContains`/`recipientContains`/`subjectContains` all as `Collection(String)` substring predicates. So any existing rule with a `recipientContains` condition failed to deserialize, crashing the whole `list_message_rules` decode (surfaced as reqwest's "error decoding response body"). Fixed `GraphRuleConditions`, its `From` impl, and `message_rule_json` to use string collections for all three "… contains" predicates — matching the UI's substring semantics and round-tripping rules created in Outlook. Compiled-but-wrong is why fmt/clippy/`cargo check` stayed green.
- **Setup showed 0.1.12** — the v0.1.13 batch never bumped the version, so `tauri-action` built `WattMail_0.1.12_x64-setup.exe` and a `latest.json` advertising 0.1.12 under the v0.1.13 tag (auto-update would never fire). Bumped `package.json` / `tauri.conf.json` / `src-tauri/Cargo.toml` to **0.1.14**, synced `Cargo.lock`. Cutting a clean 0.1.14 rather than reusing the 0.1.13 tag (monotonic, supersedes the broken installer). Old v0.1.12 + v0.1.13 releases/tags deleted per the rolling-release model.

fmt + `clippy --all-targets -D warnings` + `npm run build` clean.

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
> **⚠️ Correction (2026-06-24, v0.1.21):** the "never capped" claim below is WRONG.
> Graph's `/messages/delta` stops after ~`$top` of the most-recent messages, so `$top=50`
> *was* a real sync cap (~50 cached per folder). v0.1.21 adds on-demand server backfill.
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
| — | Message list: Outlook-style date sections + quick filters (All/Unread/Flagged) + group-by-date toggle | ✅ built (v0.1.19); frontend-only |
| — | Calendar tab (7-day agenda + create/RSVP/delete; `Calendars.ReadWrite`) | ✅ BUILT on main (CalendarProvider→Graph calendarView, multi-view nav); compile-verified + 2 adversarial-review passes; **live test pending before version bump/release** |
| — | Contacts / recipient autocomplete (`People.Read`/`Contacts.Read`) | ⬜ backlog (roadmap v0.2.0) |
| — | Generic IMAP/SMTP backend + Mailspring-style setup | 🟡 BUILT on branch `feature/imap-accounts` (CI-green); parked off main/releases pending a live app-password test |

---

## Architecture decisions

| Date | Decision | Rationale | Status |
| --- | --- | --- | --- |
| 06-15 | **Graph API first**, behind a provider-agnostic `MailProvider` trait | Fastest path to a working O365 client (conversations, delta sync, calendar/contacts free). Contract keeps IMAP/portability open. | Active |
| 06-15 | **OAuth public client + PKCE + loopback** (`http://localhost`), single-tenant | Recommended desktop pattern; a distributed binary can't protect a secret. Single-tenant = simplest for own mailbox. | Active |
| 06-15 | **Token exchange via raw form-posts**, isolated in `infrastructure/auth`, not the `oauth2` crate | Transparent, fewer moving parts, mirrors Mailspring's handshake; swappable later without touching callers. | Active |
| 06-15 | **Sync = delta-query polling** (`/messages/delta` + persisted deltaLink) | Graph change-notification webhooks need a public HTTPS callback — impractical for a desktop app. | Active (implemented, M3) |
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
- **Multi-account model** — ✓ resolved (v0.1.16/0.1.17): full per-account isolation —
  each account has its own keyring namespace + `cache-{slug}-{id}.db`, behind the
  `AccountManager` composition root, with legacy single-account adoption on first launch.
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

---

## Review findings (2026-06-23, multi-agent + adversarially verified)

Every finding below was verified against the code by an independent skeptic; several
were **downgraded on inspection** (noted). The codebase is structurally sound
(layering holds, no locks across `.await`, OAuth/PKCE textbook, both sanitizers
solid). What's left, by priority:

**Fixed in v0.1.18 (live Office 365 path):** Graph 410 recovery; the two
folder-switch frontend races; prod CSP. See the v0.1.18 progress entry.

**Deferred — latent until Gmail/Outlook OAuth creds are filled (fix BEFORE un-gating Gmail):**
- **Gmail history sync advances the cursor past dropped pages → silent permanent
  data loss.** `GmailHistoryResponse` doesn't deserialize/follow `nextPageToken`,
  but `historyId` jumps to the latest, so any history beyond page 1 (a burst, or a
  long offline gap) is lost forever. Same defect skips-but-counts-as-processed any
  added message whose hydration fetch fails. Fix: page through `nextPageToken`
  before adopting the final `historyId`; don't advance past changes not applied.
  (gmail/mod.rs:452-482, 1079-1084) — **verified HIGH**.
- **Gmail incremental sync ignores `labelsAdded`/`labelsRemoved`.** Read state
  (UNREAD), flag (STARRED), and moves changed on another device never reach the
  cache (`is_read`/`is_flagged` derive purely from labels at hydration). Stale
  until a token expires (~days), corrupting unread badges/tray/chime. Fix:
  deserialize + translate label history records into re-hydrated upserts /
  folder-scoped removals. (gmail/mod.rs:457-472, 1086-1098) — **verified HIGH/MED**.

**Pre-IMAP groundwork (do before the third backend):**
- **Extract shared infra helpers** before IMAP triples the boilerplate: `check_status`
  is byte-identical across graph/gmail, plus the bearer-verb wrappers and the
  opaque-id `path_segment` URL builder → a `crates/infrastructure/src/http.rs`.
  (NB: the address-extraction "duplication" is a false pair — Graph builds from
  structured JSON, Gmail parses raw RFC5322; not shared.) — verified, downgraded to LOW.
- **Add tests on the critical pure functions** (all offline): `sanitize_style`
  allowlist (pass + url()/expression()/@import/javascript:/`/*` rejection),
  `FieldCipher` round-trip + tamper/short-blob, `SqliteStore`
  open→upsert→recent→count→migrate(version-bump drop), `build_raw_message` MIME
  structure + inline-image Content-ID. Highest-value tests in the repo. — verified MED.

**Lower priority / accepted for a single-user Windows app (verifiers downgraded HIGH→LOW):**
- Image-proxy **SSRF** (no private-IP/loopback/redirect filter on `inline_remote_images`):
  real but desktop-only, opt-in click, reaches only the user's own LAN. graph/mod.rs:907-960.
- **Single shared AES cache key** across all per-account DBs (crypto.rs:14-16): the
  "one key decrypts all" framing is an incoherent threat model (key + DBs sit behind
  one OS user/keychain). Defense-in-depth only.
- **No macOS/Linux bundle** anywhere (tauri.conf.json `targets:["nsis"]`,
  Windows-only CI) + **Linux Secret Service hard-dep** + appindicator tray + autostart
  `--hidden`/"Start with Windows" label: all real but **known deferrals / never run
  live**. Honest fix = build the bundles OR soften the "cross-platform" claim in
  README/CONTEXT (currently overstated). README also still advertises Outlook.com/Gmail
  as available though both are gated off by placeholder creds.
- Minor: Gmail `add_account` duplicate-account check-then-insert race (accounts.rs);
  `save_folders` non-transactional DELETE+INSERT (store/mod.rs:299-318);
  `check_new_mail` TOCTOU on `last_notified_at` (commands.rs:662-694);
  `inline_remote_images` naive `src="…"` string-replace (graph/mod.rs:920-932);
  `is_flagged` stored plaintext but undocumented; locale-fragile English folder-name
  matching (main.ts + graph `inbox` literal); `checkNewMail` `currentFolderId!`
  non-null assertion can run post-sign-out (main.ts:1870-1876); main.ts is a
  3300-line monolith with the security-sensitive paste sanitizer buried in it.

**Feature gaps (prioritized, none done):** *Now* — per-account signatures + templates
(M); reply threading headers In-Reply-To/References (M, still broken — cheapest via
Graph `createReply`); junk/block-sender (S, reuses rules + move). *Next* — unified
inbox across accounts (L, registry already holds all accounts); recipient autocomplete
(L, needs `People.Read`); bulk multi-select (M); undo-send (S, client-side). *Later* —
conversation/threaded view (L, blocked on whole-folder server-side date sort),
large-attachment upload sessions (L), snooze (L).

---

## IMAP / SMTP — built, parked on `feature/imap-accounts`

The generic IMAP/SMTP backend (Mailspring-style account setup, autodiscovery,
`ProviderKind::Imap`, the credential-seam refactor, SMTP via `lettre`, MIME via
`mail-parser`) is **fully built** but lives on branch **`feature/imap-accounts`**,
deliberately kept off `main` until live-tested against a real app-password mailbox.

The full design + implementation notes — crate choices, the credential seam, the
`UIDVALIDITY`/sync model, the SMTP path, and the adversarial-review fixes — live in
**that branch's `CONTEXT.md`**, not duplicated here. To resume generic-IMAP support:
merge the branch, live-test, then release.
