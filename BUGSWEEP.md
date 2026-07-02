# WattMail bug-fix implementation prompt — 2026-07-02 sweep

This is a work order produced by a full bug sweep of WattMail's user-facing
actions and features. Every finding below was traced to exact code and
verified against both sides of the contract (frontend call site AND Rust
handler / Graph wire shape). Your job is to implement the fixes.

---

## Context you need before touching anything

- **App:** WattMail, a Tauri v2 desktop email client for Office 365 mailboxes
  via Microsoft Graph. Rust workspace + vanilla TypeScript frontend (no
  framework). Windows is the only shipped target.
- **Layout:** `src/main.ts` (~4200 lines, the whole mail UI), `src/calendar.ts`,
  `src/dialog.ts` (in-app alert/confirm/prompt), `src-tauri/src/` (Tauri
  commands = `commands.rs`, composition root = `accounts.rs`, tray/setup =
  `lib.rs`), `crates/domain` (traits/types, no I/O), `crates/application`
  (use-cases), `crates/infrastructure` (Graph client `graph/mod.rs`,
  calendar `graph/calendar.rs`, OAuth `auth/mod.rs`, SQLite cache
  `store/mod.rs`, sanitizer `html.rs`).
- **Read `CONTEXT.md` at the repo root first.** It documents the architecture,
  the release policy, and known deferrals.
- **The Gmail backend (`crates/infrastructure/src/gmail/`) is gated off**
  (placeholder OAuth creds; unreachable from the shipped UI). Do not fix Gmail
  behavior, but if you change a `MailProvider` trait signature you must update
  the Gmail impl so it still compiles.

### Hard rules

1. **Tauri v2 argument casing:** Rust command params are `snake_case`; the JS
   `invoke()` side must pass them as `camelCase`. All DTOs use
   `#[serde(rename_all = "camelCase")]`. Any new field/arg must follow this or
   it will silently arrive as null/undefined.
2. **Verify before you claim done:**
   ```
   npm run build                              # tsc --noEmit + vite build
   cargo fmt --all
   cargo clippy --all-targets -- -D warnings
   cargo test --workspace
   ```
   All four must be clean. Add/extend unit tests for pure Rust logic you
   change (decode/URL/classifier functions have existing test modules to
   extend — see `graph::tests`, `html.rs` tests).
3. **One commit per numbered bug** (or per bundle where the bug says
   "bundle with #N"), with a message like `fix: <symptom> (#N from BUGSWEEP)`.
4. **No new crates** unless a fix is impossible without one. No refactors
   beyond what a fix needs. `src/main.ts` is a monolith by design — extend it
   in place, matching the surrounding style.
5. **Never weaken sanitization.** Email HTML is only ever rendered inside the
   sandboxed reader iframe or run through a sanitizer first. Bug #10 and #13
   touch this area — follow their instructions exactly.
6. When done, add a progress entry at the TOP of `CONTEXT.md`'s progress log
   summarizing what was fixed (match the existing entry style). Do not bump
   the version or touch release files.

### Non-goals — explicitly out of scope (known, accepted deferrals)

- Graph upload sessions for large attachments (#4 only adds a size guard).
- Reply threading headers (`In-Reply-To`/`References`), `.msg` export,
  signatures, undo-send, conversation view.
- Anything in the parked IMAP branch or the gated Gmail/Outlook.com providers.
- SSRF hardening of the image proxy; cache-encryption changes.

---

## P1 — data loss or a core action that does not do what it says

### 1. Delete permanently deletes instead of moving to Deleted Items

- **Where:** `crates/infrastructure/src/graph/mod.rs:453-464`
  (`delete_message`), `crates/application/src/lib.rs` (`delete_message`
  use-case), frontend delete paths in `src/main.ts` (context-menu Delete and
  the `#` shortcut).
- **Symptom:** Deleting a message issues `DELETE /me/messages/{id}`. Contrary
  to the code comment, Graph does NOT move it to Deleted Items — it goes to
  Recoverable Items (invisible in every folder). User deletes a message,
  looks in Deleted Items to recover it, and it is not there. Every UI string
  in the chain promises Deleted Items. There is also no delete confirmation
  anywhere, so this is one keypress (`#`) from silent near-permanent loss.
- **Fix:**
  - Change the `MailProvider::delete_message` signature to
    `delete_message(&self, id: &str, permanent: bool)`.
  - Graph impl: when `permanent == false`, do
    `POST /me/messages/{id}/move` with body
    `{"destinationId": "deleteditems"}` (well-known name is accepted; reuse
    the existing move code path — see `move_message` just below it). When
    `permanent == true`, keep the current `DELETE`.
  - Gmail impl already trashes on delete; just accept and ignore the new
    param (permanent can also trash — it's unreachable anyway).
  - Application use-case and the `delete_message` Tauri command gain the
    `permanent` flag; cache write-through (`store.remove_message`) is
    unchanged (a move out of the folder should still remove the row).
  - Frontend: pass `permanent: true` only when the current folder's role is
    `"deleteditems"` (folder roles are lowercase well-known names — see
    `f.role === "inbox"` at `main.ts:2139` for the pattern). Before a
    permanent delete, show `showConfirm` from `src/dialog.ts` with
    `danger: true` ("Permanently delete this message? This cannot be
    undone."). No confirmation for the normal move-to-Deleted-Items delete.
- **Accept:** deleting from Inbox moves the message to Deleted Items
  (verifiable via another client); deleting from Deleted Items asks, then
  hard-deletes. `cargo test --workspace` still green.

### 2. Compose discards an in-progress message with no warning

- **Where:** `src/main.ts` — Esc handler `:3983` (`closeCompose()`
  unconditionally), backdrop click `:3921-3923`, Cancel button `:3831`,
  `closeCompose` `:3014-3016` (just hides), `openCompose` `:2982-2998`
  (overwrites all fields on next open).
- **Symptom:** One Esc press (even while focused in the To/Subject/body —
  the Esc chain never checks), one click on the dimmed backdrop (including
  the common "drag a text selection and release outside the panel" case), or
  a misclicked Cancel silently destroys a fully written email. No confirm, no
  auto-draft, unrecoverable.
- **Fix:**
  - Track dirty state: on `openCompose`, snapshot to/cc/subject and
    `cBodyInput.innerHTML` plus attachment counts; a helper
    `composeIsDirty()` compares current values against the snapshot.
  - Route ALL three close paths through a single
    `async requestCloseCompose()`: if dirty, `await showConfirm("Discard this
    message?", { danger: true, okLabel: "Discard" })` and only close on
    confirm.
  - Fix the backdrop path properly: only treat it as a close when the
    `mousedown` ALSO started on the overlay (track the mousedown target),
    so selecting text in the editor and releasing on the backdrop never
    closes.
  - `dialog.ts` dialogs are wired into `aModalIsOpen`/Esc capture already —
    the confirm's own Esc/Enter will not leak (verified).
- **Accept:** Esc on a dirty compose asks before discarding; a clean compose
  (nothing typed beyond the prefill) still closes instantly; drag-select
  release on backdrop does not close.

### 3. Failed token refresh silently opens browser sign-ins and wedges the app; cancelled OAuth hangs forever

- **Where:** `crates/infrastructure/src/auth/mod.rs:171-185` (`access_token`
  falls through refresh failure to `interactive_login`), `:356-390`
  (`wait_for_code` blocks on `server.incoming_requests()` forever),
  `src-tauri/src/accounts.rs:261-320` (`add_account`), `src/main.ts:4184-4189`
  (60s auto-sync), `:2287-2321` (`syncing` flag), `:2461-2481`
  (sign-in buttons).
- **Symptom (a):** When the refresh token is revoked/expired — or refresh
  merely fails because the machine is OFFLINE — the very next background
  sync or click (open message, mark read) launches the OS browser with no
  user gesture. If the user closes the tab, the invoke never resolves:
  `syncing` stays true forever, manual Refresh is dead, and every further
  action opens yet another browser tab. **Symptom (b):** during Add account,
  closing the browser tab leaves the loopback listener waiting forever and
  the sign-in buttons permanently disabled.
- **Fix:**
  - `access_token()` must NEVER start an interactive login. On refresh
    failure, return a new distinguishable error (add
    `AuthError::ReauthRequired` — keep a separate `Network` case for
    transport failures so offline does not demand re-auth: inspect the
    refresh error; a reqwest transport error → `Network`, an OAuth
    `invalid_grant`-style response → `ReauthRequired`).
  - Interactive login remains only in the explicit flows: `add_account`, and
    a NEW Tauri command `reauthenticate_account` that runs
    `interactive_login()` + `remember()` on the ACTIVE account's
    `AuthService`.
  - Give `wait_for_code` a deadline: loop on `server.recv_timeout(...)`
    (tiny_http supports it) with a total budget of ~5 minutes, returning a
    "sign-in timed out / cancelled" `AuthError` variant. This bounds BOTH
    add-account and re-auth.
  - Map `ReauthRequired` to a recognizable error string in the command layer
    (e.g. prefix `"auth-required:"` — commands already stringify errors).
  - Frontend: on any invoke rejection matching that prefix, stop the
    auto-sync loop (a `reauthRequired` flag), and show a persistent banner or
    dialog: "Your session expired — sign in again" with a button that calls
    `reauthenticate_account`, clears the flag on success, and resumes sync.
    Also make the sign-in/add-account buttons re-enable in a `finally` so a
    timeout lets the user retry.
- **Accept:** with a dead refresh token, the app shows the re-auth banner
  (no surprise browser tabs); clicking it completes sign-in and sync resumes.
  Cancelling an add-account browser flow returns an error within the timeout
  and the button is usable again. Offline sync failures surface as network
  errors, not re-auth demands.

### 4. Attachments over Graph's size cap always fail at send, with a raw JSON error

- **Where:** `src-tauri/src/commands.rs:518-570` (`send_message` buffers all
  bytes), `crates/infrastructure/src/graph/mod.rs:638-668` (single
  `POST /me/sendMail`), `:1144-1169` (`attachments_json` base64-inlines
  everything). No size check exists in `pickAttachments`
  (`src/main.ts:2960-2965`) or the forwarded-chip flow (`:3052-3065`); the
  existing 3 MB caps at `:2800-2801` cover inline images only.
- **Symptom:** Attach (or forward) a 5 MB PDF → Send always fails: base64
  inflates ~33% and Graph's simple `sendMail` path caps the request at
  ~4 MB, so Graph rejects it and the raw error JSON is dumped into the
  compose status line. The UI happily accepts and even displays the size of
  attachments it can never send.
- **Fix (guard only — upload sessions are OUT of scope):**
  - In the `send_message` command, before building the payload, sum the raw
    byte sizes of: local attachment files (`std::fs::metadata`), forwarded
    attachment refs (the DTO already carries `size` — if it doesn't, add it;
    frontend has it), and decoded inline images. If the total exceeds
    **2.5 MB**, return a readable error:
    `"Attachments total X.X MB — messages with more than ~2.5 MB of attachments can't be sent yet."`
    Do the same check in `save_draft`'s future path only if trivially
    applicable (drafts don't carry attachments today — skip otherwise).
  - Frontend: surface that message verbatim in `composeMsg` (it already
    displays command errors); additionally, when adding a forwarded-
    attachment chip whose `size` alone exceeds the budget, show the warning
    immediately in `composeMsg` so the user learns before send.
- **Accept:** oversized send fails fast with the readable message; a normal
  small send is unaffected.

---

## P2 — wrong behavior a user will hit in normal use

### 5. Delta changes applied out of order — deleted messages resurrect in the cache

- **Where:** `crates/application/src/lib.rs:200-224` (`sync_folder`).
- **Symptom:** All `Upserted` changes are buffered and flushed AFTER the
  loop, while `Removed`/`FlagsChanged` are applied inline — so a
  remove-then-flush reorders the feed. If one delta round carries both an
  upsert (early page) and a tombstone/flag change (later page) for the same
  id, the delete is undone by the stale upsert: a ghost row that never goes
  away (the token advances past the tombstone). Same shape leaves read/flag
  state stale.
- **Fix:** Preserve feed order. Keep the batching, but flush the pending
  `upserts` buffer to the store immediately BEFORE applying each `Removed`
  or `FlagsChanged` (then continue buffering). Add a unit test in the
  application crate: feed `[Upserted(a), Removed(a)]` through `sync_folder`
  with a mock store and assert `a` is absent afterwards (there are existing
  mock-provider tests to crib from).
- **Accept:** new test green; existing tests green.

### 6. "N new messages" notification spam on every launch; state leaks across accounts

- **Where:** `src-tauri/src/commands.rs:800-849` (`check_new_mail`),
  `src-tauri/src/lib.rs:32-38` (`NotificationState`), `src/main.ts:2183-2203`.
- **Symptom:** `last_notified_at` starts `None` each process start, and the
  filter `last.is_none_or(...)` passes EVERY unread message — so a user with
  12 old unread messages gets a "12 new messages" toast on every launch.
  The state is also process-global: after switching accounts, account B's
  mail is compared against account A's timestamp (spurious or suppressed
  notifications).
- **Fix:** Key the state per account: `NotificationState` becomes a
  `RwLock<HashMap<String, String>>` (account id → last-notified timestamp);
  `check_new_mail` resolves the active account id via `State<AccountManager>`
  (other commands show the pattern). On the FIRST check for an account
  (no entry), seed the entry with the newest `received` among the passed
  messages and return `Ok(None)` — never notify on the seeding pass.
- **Accept:** fresh launch with old unread mail produces no toast; a message
  arriving after launch does; switching accounts doesn't cross-talk.

### 7. New-mail notifications never fire unless the Inbox is the open folder

- **Where:** `src/main.ts:4184-4189` (auto-sync ticks only
  `currentFolderId`), `:2299-2304` (`checkNewMail` gated to the inbox
  folder).
- **Symptom:** The setting promises "a desktop alert when unread messages
  arrive in the Inbox", but the 60s timer only syncs the folder being
  viewed. Work in any other folder (or leave the app in the tray on another
  folder) and no inbox sync ever runs → no notification, ever.
- **Fix:** In the auto-sync tick, after syncing the current folder, also
  sync the Inbox when it isn't the current folder (find it by
  `f.role === "inbox"`, same as `main.ts:2139`), then run the existing
  `checkNewMail` path against the Inbox cache regardless of which folder is
  open. Keep the existing "suspend while searching / modal open" guards.
- **Accept:** with the app sitting on another folder, new inbox mail
  produces a toast within ~60s.

### 8. All-day calendar events render on the wrong day for cross-timezone organizers

- **Where:** `crates/infrastructure/src/graph/calendar.rs` (`to_domain_event`
  around `:234-319`); bucketing consumers `src/calendar.ts:269-274`,
  `:356-377`.
- **Symptom:** The pipeline assumes all-day events arrive as date-only
  midnight, but `Prefer: outlook.timezone` converts ALL returned dateTimes
  including all-day ones. A London-created all-day "July 10" event viewed
  from New York comes back as `2026-07-09T19:00:00` → bucketed and displayed
  under July 9, and the multi-day range math collapses wrongly.
- **Fix:** In `to_domain_event`, when `is_all_day == Some(true)`, snap both
  start and end to the NEAREST midnight before building `EventDateTime`:
  parse the `HH` component of `YYYY-MM-DDTHH:MM:SS`; if `HH >= 12` roll
  forward to the next day's `00:00:00`, else truncate to `00:00:00`. (The
  conversion shift is a zone-offset delta, always < 12h.) Add unit tests:
  `2026-07-09T19:00:00` → `2026-07-10T00:00:00`; `2026-07-10T08:00:00` →
  `2026-07-10T00:00:00`; an already-midnight value unchanged.
- **Accept:** tests green; timed events untouched.

### 9. Ongoing multi-day events vanish from the agenda once their start scrolls out of the window

- **Where:** `src/calendar.ts:265-294` (`renderAgenda` buckets by start-day
  key only; keys before the 7-day window match no bucket).
- **Symptom:** A 2-week vacation or a conference that started last Friday is
  returned by `calendarView` (overlap semantics) but silently discarded —
  "Today" shows "No events" during an ongoing event.
- **Fix:** Clamp the bucket key: `bucketDay = max(eventStartDay,
  windowStartDay)` so an ongoing event lists under the first visible day.
  Keep the existing "(continues)"-style display from `fmtWhen` as-is (it
  already renders ranges).
- **Accept:** an event spanning the window start appears under the first
  window day.

### 10. Inline (cid:) images in received mail never render — and the banner claims clicking will load them

- **Where:** `crates/infrastructure/src/graph/mod.rs:323-349` (`message()` —
  no cid resolution), `:1236-1261` (`inline_remote_images` handles http(s)
  only), `:760-766` (`attachments()` filters OUT `isInline`, so the bytes
  are unreachable), `crates/infrastructure/src/html.rs:131-134` (blocked
  mode strips ALL `<img>`), `:194-200` (`has_remote_content` = "contains
  `<img`", so cid-only mail shows the banner), banner UI
  `src/main.ts:1096-1118`.
- **Symptom:** Any message with an inline image (signature logo, pasted
  screenshot) renders without it; the "Images blocked — click to load"
  banner appears, and clicking does nothing for cid images (ammonia drops
  the `cid:` scheme; the proxy only fetches http). The image is also absent
  from the attachment chips. There is no way to see it at all.
- **Fix (do all three):**
  1. In `GraphClient::message`, when the raw body contains `cid:`, fetch the
     message's inline attachments (`GET .../attachments` filtered to
     `isInline eq true`, or fetch all and filter — reuse the existing
     attachment-fetch plumbing) and rewrite each `src="cid:<contentId>"` to
     a `data:<contentType>;base64,<bytes>` URL BEFORE sanitization, matching
     ids case-insensitively and tolerating a `<`/`>`-wrapped contentId.
     Mirror the string-replace approach of `inline_remote_images`.
  2. Make cid-inlined `data:` images survive BOTH sanitizer modes: in
     `html.rs`, blocked mode must stop blanket-stripping `<img>` — instead
     keep `<img>` whose src is `data:` and strip only http(s)/other-scheme
     imgs (ammonia attribute-filter or a pre-pass; follow the existing
     allowlist style). `data:` srcs must already survive the allow-images
     mode (the http proxy inlines to `data:` today — reuse whatever makes
     that pass).
  3. Fix the banner heuristic: `has_remote_content` returns true only when
     an `<img>` src (or the existing url() checks) points at http(s) —
     a message whose only images are cid/data must not show the banner.
  - Add `html.rs` unit tests for: cid-only body → no banner flag; data: img
    survives blocked mode; http img still stripped in blocked mode.
- **Accept:** a message with an inline screenshot shows it immediately with
  no banner; remote-image blocking/consent behavior for http images is
  unchanged. **Stretch (optional, same area):** reply/forward quote
  currently strips these images too (`commands.rs` calls `read_message(...,
  false)`); once cid→data resolution exists, the quoted original can keep
  inline images by using the blocked-mode sanitizer output (which now keeps
  data: images). Do NOT auto-fetch remote http images for quotes.

### 11. Compose has no Bcc, and sending a resumed draft delivers hidden Bcc recipients/attachments

- **Where:** `crates/infrastructure/src/graph/mod.rs:728` (`load_draft`
  `$select` omits `bccRecipients` and attachments), `:1132-1139`
  (`draft_body_json` PATCHes to/cc only), `src/main.ts:3176-3181` (draft
  send = PATCH + `POST /send` of the whole server object).
- **Symptom:** A draft started in Outlook/OWA with a Bcc (or with
  attachments) opened in WattMail shows neither; Send delivers them anyway —
  invisible and unremovable. Separately, WattMail can't Bcc at all.
- **Fix:**
  - Add a Bcc field to compose, mirroring Cc exactly (input, parse via the
    same `parseAddresses`, include in `send_message` and
    `save_draft`/`draft_body_json` payloads as `bccRecipients`, and in
    `compose_reply`'s DTO plumbing where trivial — a new empty field is fine
    for reply/forward prefills).
  - `load_draft`: add `bccRecipients` and `hasAttachments` to the `$select`;
    populate the Bcc field on resume. When `hasAttachments` is true, show a
    persistent note in `composeMsg`: "This draft has attachments saved on
    the server — they will be sent with it."
- **Accept:** Bcc round-trips draft-save → resume → send; a foreign draft's
  Bcc is visible and editable before send; the attachment note appears.

### 12. "Save draft" silently discards attachment chips (send-time guard then never fires)

- **Where:** `src/main.ts:3121-3140` (`saveDraft` — no warning), `:3159-3164`
  (warning exists only in `sendCompose`).
- **Symptom:** Forward with chips → Save draft → resume later → chips gone,
  and since the resumed compose has no chips the send-time guard is silent —
  the forward goes out WITHOUT its attachments. The exact silent-drop class
  v0.2.6 fixed for direct sends, resurrected via the draft path.
- **Fix:** In `saveDraft`, when `composeAttachPaths.length ||
  composeForwardedAtts.length` (or inline images present — reuse the send
  path's detection), show the same warning via `showConfirm` ("Attachments
  aren't saved with drafts yet — save anyway without them?") before saving.
- **Accept:** saving a draft with chips asks first; chipless saves are
  unchanged.

### 13. Resumed draft body is injected into the app DOM verbatim — foreign `<style>` restyles the whole app

- **Where:** `src/main.ts:2990-2991` (`cBodyInput.innerHTML = opts.bodyHtml`),
  `crates/infrastructure/src/graph/mod.rs:724-743` (`load_draft` returns the
  raw body, deliberately unsanitized).
- **Symptom:** A draft authored in Outlook desktop carries Word/Outlook CSS
  in a `<style>` block; `innerHTML` keeps it and `<style>` applies
  document-wide → the entire WattMail UI is restyled while the draft is
  open. Inline `on*` handler attributes in the stored body also land in the
  privileged main window (unlike reader HTML, which is sanitized and
  sandboxed).
- **Fix:** Run resumed-draft bodies through the existing paste sanitizer
  `sanitizeHtml` (`src/main.ts:2789`) before assignment — it already strips
  scripts/styles/handlers and keeps the compose-supported subset including
  `data:` inline images. Only the `opts.bodyHtml` path changes; the
  `quotedHtml` path (server-sanitized) stays as-is. If `sanitizeHtml` drops
  something drafts legitimately need (check its allowlist against a plain
  Outlook draft: p/br/div/span/a/b/i/u/ul/ol/li/img are the essentials),
  extend its allowlist rather than writing a second sanitizer.
- **Accept:** opening a style-carrying draft no longer restyles the app;
  a WattMail-authored draft round-trips visually unchanged.

### 14. In-app dialogs: Enter while Cancel is focused confirms instead of cancelling

- **Where:** `src/dialog.ts:126-137` (capture-phase keydown treats any Enter
  outside the prompt input as OK, suppressing the focused button's native
  activation).
- **Symptom:** "Delete folder … cannot be undone" → Tab to Cancel → Enter →
  the folder is deleted.
- **Fix:** In the key handler, when `document.activeElement === cancelBtn`,
  settle with `cancelResult()` instead. (Equivalently: don't intercept Enter
  when focus is on either dialog button and let native activation run —
  pick one, keep it simple.)
- **Accept:** Enter on focused Cancel cancels; Enter elsewhere still
  confirms; prompts still submit on Enter from the input.

### 15. Remove account never actually deletes the mailbox cache on Windows

- **Where:** `src-tauri/src/accounts.rs:324-345` (`remove_account`),
  `crates/infrastructure/src/store/mod.rs` (store owns an open rusqlite
  `Connection`).
- **Symptom:** `std::fs::remove_file` runs while `removed:
  Arc<ManagedAccount>` still holds the open connection — on Windows (the
  only shipped target) deletion fails with a sharing violation, silently
  (`let _ =`). The mailbox's `cache-*.db` stays on disk with the user's
  mail; the `-wal`/`-shm` siblings are never targeted at all.
- **Fix:** Compute the db path first, `drop(removed)` (closing the
  connection — verify nothing else clones the Arc; the store field may need
  extracting) BEFORE deleting, then remove the `.db`, `.db-wal`, and
  `.db-shm` files. Log (eprintln, matching existing style) instead of
  discarding errors.
- **Accept:** after Remove account, the cache files are gone (test on the
  dev box or via a temp-dir unit test around the path logic).

### 16. Search is clobbered by in-flight folder refreshes, and a stale debounce resurrects dead queries

- **Where:** `src/main.ts:2214-2229` (`refreshFromCache` — folder-id guard
  only), `:2235-2254` (`backfillOlder` — same), `:3795-3804` (search input
  arms `searchTimer`), `:2159-2177` (`selectFolder` reset lacks
  `clearTimeout`), `:2380-2390` (`loadActiveAccount` reset lacks it),
  `:753-775` (`showSignedOut` lacks it); contrast `exitSearch` `:1064-1075`
  which clears it.
- **Symptom (a):** Refresh/auto-sync/Load-more in flight, then search: when
  the network call lands, the folder rows repaint OVER the search results
  with `searchActive` still true — live query in the box, no results header,
  and auto-sync suspended until the user re-clicks a folder. **Symptom (b):**
  type a query and switch folder/account within the 300ms debounce — the
  timer fires the OLD query later, hijacking the new view (after an account
  switch, with an empty search box and `searchActive=true`).
- **Fix:** (a) In `refreshFromCache` and `backfillOlder`, bail before
  rendering when `searchActive` is true (mirror `reconcileAfterAction`).
  (b) Extract the `clearTimeout(searchTimer); searchTimer = null` step and
  call it unconditionally in `selectFolder`, `loadActiveAccount`, and
  `showSignedOut`.
- **Accept:** searching during a slow refresh keeps the results painted;
  folder-switch inside the debounce window never triggers the old search.

### 17. Modal/menu keyboard leaks: provider picker, floating menus, Esc double-fire

- **Where:** `src/main.ts:4004-4014` (`aModalIsOpen` omits
  `providerOverlay`), `:4030` (menu guard checks only
  `ctxMenu`/`folderMenu`), `:2562-2566` (provider picker has its own Esc
  listener that runs IN ADDITION to the central chain at `:3973-3984`).
- **Symptom:** With the provider picker open (toolbar → "+ Add account…"),
  `#` deletes the cursored message behind it, `c` opens compose on top,
  j/k/Enter navigate. Same leak while the edit menu, link menu, or account
  dropdown is open. And Esc on the picker also closes the Settings panel
  beneath it (both document-level listeners fire).
- **Fix:** Add `providerOverlay` to `aModalIsOpen`; extend the `:4030` guard
  with `editMenu`, `linkCtxMenu`, and `accountMenu` hidden-checks; fold the
  provider picker into the central Esc chain (insert it at the correct
  topmost position, i.e. before the settings branch) and delete its private
  Esc listener.
- **Accept:** no list shortcut fires under any open overlay/menu; one Esc
  closes only the picker, a second closes Settings.

---

## P3 — smaller but real; fix after P1/P2

### 18. Older-mail backfill skips messages sharing the boundary second
`crates/infrastructure/src/graph/mod.rs:296` uses
`receivedDateTime lt {before}` against a second-granularity anchor — bulk
deliveries sharing the oldest cached second are never fetched. **Fix:** use
`le`; overlap rows re-upsert harmlessly by id, and the existing frontend
`total <= before` check already treats an overlap-only page as end-of-folder.
Update the URL unit test.

### 19. Opening a message leaves the unread badge/tray stale for up to 60s
`src/main.ts:925-932` — `markRead` never refreshes folders, unlike the
context-menu path (`:935-949`). **Fix:** after a successful `set_read`, call
`loadFolders()` (bundle with #20's chime suppression so this doesn't beep).

### 20. False "new mail" chime on account switch and on mark-as-unread
`src-tauri/src/lib.rs:189-216` — the global `LAST_UNREAD` chimes whenever the
reported count rises: mark-as-unread → chime; switching to a busier account →
chime. **Fix:** add a `silent: bool` param to the `set_unread`/tray-update
path (or reset `LAST_UNREAD` to −1 on account add/switch/remove and pass a
user-initiated flag from the frontend for read-state changes) so only the
new-mail sync path can chime.

### 21. Clicking the new-mail notification does nothing (dead listener)
`src/main.ts:4107-4113` listens for an `open-message` event nothing emits;
the code comment promises click-to-open. `tauri-plugin-notification` gives no
click events, and `setFocus` isn't in `capabilities/default.json`. **Fix
(minimal):** delete the dead listener and the comment's promise. Do NOT
build a native notification click path in this pass.

### 22. Calendar RSVP/Delete buttons allow double-fire
`src/calendar.ts:543-574` — unlike `saveEvent`, `respond`/`deleteEvent` never
disable their buttons; a double-click sends duplicate Graph POSTs (organizer
can get contradictory response mails). **Fix:** disable all
`[data-rsvp]`/`[data-cal-delete]` buttons on click; re-enable on error
(success re-renders anyway).

### 23. Recipient/attendee parsing: no validation, commas in display names split into garbage
`src/main.ts:3097-3102` (`parseAddresses` splits on `[,;]`, keeps anything),
`src/calendar.ts:703-706` (attendees split on comma only). Pasting
`Doe, John <john@x.com>` or `alice@x.com; bob@y.com` yields invalid tokens →
raw Graph 400 in the status line. **Fix:** in both places: split on `[,;]`,
extract the `<addr>` from `Name <addr>` tokens, validate each with a minimal
`/^\S+@\S+\.\S+$/`, and report the first bad token readably
("'Doe' is not a valid address") before invoking. (Full RFC display-name
splitting is NOT required — extracting angle-bracket addresses fixes the
practical cases.)

### 24. Removing an account: no confirmation, and removing a NON-active account resets the whole view
`src/main.ts:3824-3829` (no confirm; contrast folder delete) and `:2496-2510`
(`removeAccount` unconditionally reloads the active account, dropping open
message/folder/search). **Fix:** wrap in `showConfirm(..., { danger: true })`;
only call `loadActiveAccount()` when the removed id WAS the active one
(check against the pre-removal account list), else just refresh the accounts
UI.

### 25. A transient cache-open failure at boot permanently drops an account
`src-tauri/src/accounts.rs:157-190` — an unloadable account is skipped AND
then `persist()` rewrites `accounts.json` without it; a one-off locked DB
file silently deletes the account (tokens orphaned in the keyring). **Fix:**
keep unloadable records in the persisted list (retry next launch) — e.g.
persist the original records for entries that failed to open, or only persist
when adoption actually changed the list.

### 26. Folder sidebar silently truncates at 100 folders per level
`crates/infrastructure/src/graph/mod.rs:65-92` — `$top=100`, no
`@odata.nextLink` following in `fetch_child_folders`. Mail in folder 101+ is
unreachable in-app. **Fix:** page via `@odata.nextLink` (mirror the message
list's paging loop); extend the decode test.

### 27. Deleting a parent folder strands the UI inside a deleted descendant
`src/main.ts:3729` (exact-id check only) — deleting "Projects" while viewing
"Projects/2026" leaves the UI in the dead subfolder with every sync failing
404. **Fix:** after the post-delete `loadFolders()`, if `currentFolderId` is
no longer in the refreshed list, fall back to Inbox (the delete-open-folder
path already does this — reuse it). Optionally purge vanished folders' cached
rows via the existing `forget_folder`.

### 28. Offline "Load more" hides cached rows it promised
`src/main.ts:3229-3236` + `:2235-2254` — when the click overshoots the cache
and the server backfill fails, `loadedCount` rolls back to the OLD value, so
the cached rows the button labeled ("Load 20 more") never render. **Fix:** on
backfill failure, roll back to `min(attempted, cachedTotal)` so cached rows
still widen.

### 29. Insert-link accepts protocol-less URLs
`src/main.ts:3842-3862` — `www.example.com` becomes a relative href, dead in
the recipient's client. **Fix:** prepend `https://` when the value has no
scheme; show a status message instead of silently no-oping when the
selection is collapsed.

### 30. Save-draft / Send race can duplicate a message
`src/main.ts:3121-3153` — Send clicked while a draft save is in flight goes
out via `sendMail`, then the save completes and leaves a resumable duplicate
draft. **Fix:** disable Send while a save is in flight and vice versa (a
shared in-flight flag/promise both handlers await).

### 31. Link context menu can't be dismissed from inside the reading pane
`src/main.ts:1164-1189` — dismiss listeners live on the parent document;
clicks/Escape inside the sandboxed iframe never reach them. **Fix:** in
`wireFrameLinks`, hide `linkCtxMenu` (and `editMenu`) at the top of the
iframe click handler, and add an iframe-doc Escape keydown that hides them.

### 32. Reply puts original To recipients in Cc and ignores Reply-To
`crates/application/src/lib.rs:379-406` + `graph/mod.rs:326` (`$select` lacks
`replyTo`). Reply-all demotes To→Cc (breaks recipients' filters vs
Outlook/Gmail convention); list/ticket mail with Reply-To goes to the raw
From. **Fix:** select `replyTo`; prefer it (when non-empty) as the reply
target; keep original To recipients (minus self) in `to` for reply-all.

### 33. Forwarding silently drops non-file attachments
`crates/infrastructure/src/graph/mod.rs:760-766` keeps only
`fileAttachment`s, so an attached .eml (`itemAttachment`) or OneDrive link
(`referenceAttachment`) vanishes from a forward with no notice. **Fix
(surface, don't implement):** have `attachments()` (or a sibling count)
report whether non-file attachments exist; the forward prefetch shows a
non-removable notice chip "1 attached item can't be forwarded by WattMail".

---

## Suggested order

Work P1 top-down (1 → 4), then P2 (5 → 17), then P3. Within P2: #5, #6+#7,
#14, #16, #17 are small and independent — do them early; #10 and #11 are the
two larger ones — do them last, each in its own commit, and re-run the full
verify suite after each.

Areas the sweep verified CLEAN — do not "improve" them while you're in
there: invoke/DTO casing across all existing commands; Graph id
path-segment encoding and the `$value`/`$batch`/delta URL shapes; the
XSS-escaping discipline in list rows/reader/calendar (`esc()` everywhere);
the optimistic-action revert paths; dialog.ts's Esc capture; the sync
`pendingSync` re-run; the folder-role `$batch` mapping; theme handling; the
updater flow; tray close-to-quit behavior.
