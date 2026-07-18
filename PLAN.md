# WattMail work order — 2026-07-18 sweep + rich-paste fidelity

Work order for the next implementation session. Two parts:

- **Part A — Feature: preserve pasted formatting** (style-inlining pre-pass for
  paste/drop/quote HTML).
- **Part B — Bug fixes** from the 2026-07-18 full sweep (three independent
  review lenses, findings verified/triaged below).

---

## Context you need before touching anything

- **App:** WattMail, a Tauri v2 desktop email client for Office 365 mailboxes
  via Microsoft Graph. Rust workspace + vanilla TypeScript frontend (no
  framework). Windows is the only shipped target.
- **Layout:** `src/main.ts` (~5000 lines, the whole mail UI), `src/calendar.ts`,
  `src/dialog.ts`, `src-tauri/src/` (commands = `commands.rs`, composition root
  = `accounts.rs`, tray/setup = `lib.rs`), `crates/domain`, `crates/application`,
  `crates/infrastructure` (Graph client `graph/mod.rs`, calendar
  `graph/calendar.rs`, OAuth `auth/mod.rs`, SQLite cache `store/mod.rs`,
  reader-iframe sanitizer `html.rs`).
- **Read `CONTEXT.md` at the repo root first.** Architecture, release policy,
  known deferrals.
- The Gmail backend (`crates/infrastructure/src/gmail/`) is gated off and
  unreachable. Never fix Gmail behavior; only keep it compiling if a shared
  trait changes.

### Hard rules

1. **Tauri v2 argument casing:** Rust command params are `snake_case`; the JS
   `invoke()` side passes `camelCase`. All DTOs use
   `#[serde(rename_all = "camelCase")]`.
2. **Verify before you claim done** (this is `scripts/verify.sh`; the stop-gate
   hook runs it too):
   ```
   npm run build                              # tsc --noEmit + vite build
   cargo fmt --all -- --check
   cargo clippy --all-targets -- -D warnings
   cargo test --workspace
   ```
   PATH gotcha on this machine: prepend `$HOME/.cargo/bin` so the pinned MSVC
   toolchain is used — the bare `cargo` on PATH is a GNU install that cannot
   link the cdylib.
3. **One commit per numbered item** (feature = its own commit; bugs may be
   bundled only where a bug entry says so), message style
   `fix: <symptom> (#N from PLAN)` / `feat: ...`.
4. **Never weaken sanitization.** Part A deliberately runs BEFORE the existing
   sanitizer and only ever ADDS inline `style` attributes for it to filter; the
   sanitizer's allow-list changes in A3 are enumerated exactly — nothing else.
5. When done: progress entry at the top of `CONTEXT.md`'s log,
   `ARCHITECTURE.md` untouched unless a boundary moved (Part A does not move
   one). Version bump + `[release]` per repo convention once everything lands
   green.

---

## Part A — Preserve pasted text formatting (rich-paste fidelity)

### What already exists (do not rebuild)

- Compose body is `contenteditable` (`#c-body`), toolbar drives
  `execCommand`, and the `paste` handler (`src/main.ts` ~4843) already prefers
  clipboard `text/html`, runs it through `sanitizeHtml()` (~3167) and inserts
  via `execCommand("insertHTML")`. The drop handler (~4880s) does the same.
  Reply/forward quoting inserts the original body through the same
  `sanitizeHtml()` (~3514). Send transmits `cBodyInput.innerHTML` as Graph
  `body: { contentType: "HTML" }` — formatting that survives the sanitizer
  already reaches recipients.

### Why formatting is still lost (the actual gaps)

1. `rebuildInto()` drops **`class` attributes** (never copied) and cuts
   **`<style>` blocks** wholesale (`DROP_SUBTREE_TAGS`). Word, Excel, Outlook,
   Teams, OneNote and many web pages carry most of their formatting as
   class-based rules in a `<style>` block — all of it evaporates. This is the
   dominant cause.
2. `ALLOWED_STYLE_PROPS` (~3046) is missing properties these sources rely on:
   `line-height`, `list-style-type`, `vertical-align`, `text-indent`,
   `white-space`, `border-collapse`, and the `border-*` longhands
   (`border-top/right/bottom/left`, `border-width/style/color`) — Word emits
   table borders as longhands, so pasted tables lose their rules.
3. Table presentation attributes: `<table>` keeps NO attributes;
   `td`/`th` keep only `colspan/rowspan/align/valign`. `bgcolor` and `width`
   on `table/td/th`, and `border/cellpadding/cellspacing` on `table`, are
   dropped — Excel grids lose shading and column widths.
4. `<font>` keeps only `color` — `face` and `size` are dropped.

### A1 — Computed-style inlining pre-pass (the core fix)

New function in `src/main.ts` (near the sanitizer, matching its comment style):

```ts
// Resolve class/<style>-based formatting in untrusted clipboard HTML into
// inline style attributes, so the sanitizer (which drops classes and <style>
// blocks) can keep it. Renders in a network-blocked, script-blocked iframe.
async function inlineComputedStyles(dirtyHtml: string): Promise<string>
```

Algorithm:

1. **Guard:** if `dirtyHtml.length > 2_000_000`, skip inlining entirely and
   return `dirtyHtml` unchanged (giant pastes fall back to today's behavior).
2. Create a hidden `<iframe>` with `sandbox="allow-same-origin"` (NO
   `allow-scripts` — scripts cannot run) appended to `document.body`,
   `style="position:fixed;left:-9999px;width:800px;height:10px;visibility:hidden"`.
   Set its content via `srcdoc` to:
   `<!doctype html><html><head><meta http-equiv="Content-Security-Policy"
   content="default-src 'none'; style-src 'unsafe-inline'"></head><body>` +
   dirtyHtml + `</body></html>` — the CSP blocks every network fetch
   (images, fonts, `@import`) so hostile CSS cannot exfiltrate or ping, while
   inline/`<style>` CSS still applies for computation. Await the iframe `load`
   event with a **500 ms timeout**; on timeout, remove the iframe and return
   `dirtyHtml` unchanged (fallback, never lose the paste).
3. In the iframe document, walk all elements top-down. For each element whose
   tag is in `ALLOWED_TAGS`, compute `getComputedStyle(el)` and build a style
   patch over exactly the props in `ALLOWED_STYLE_PROPS` (post-A3 list):
   - **Inherited props** (`color`, `font-family`, `font-size`, `font-weight`,
     `font-style`, `text-align`, `line-height`, `list-style-type`,
     `white-space`, `text-indent`): inline only when the value differs from
     the element's **parent's** computed value (root elements compare against
     the iframe `body`). This stops every `<span>` repeating the document
     font.
   - **Non-inherited props** (`background-color`, `text-decoration-line` —
     map it to `text-decoration` when writing, `vertical-align`, the
     `border-*` longhands, `border-collapse`, margins, paddings): inline only
     when the value differs from a **per-tag baseline** — a pristine element
     of the same tag appended once to the iframe body (cache baselines in a
     `Map<string, CSSStyleDeclaration>`; snapshot the needed prop values into
     a plain object because the declaration goes live-dead when the probe is
     removed). Skip transparent/`rgba(0, 0, 0, 0)` background-color and
     `none`-ish borders.
   - Merge the patch into the element's existing `style` attribute
     (existing inline declarations win — they are the author's most specific
     intent and already survive today's pipeline).
   - Skip `width`/`height` here (computed px widths on every div would
     freeze fluid layouts); explicit width/height inline styles and attrs
     already pass through.
4. Serialize `iframe.contentDocument.body.innerHTML`, remove the iframe
   (`finally`), return the serialized HTML.
5. Wrap everything in try/catch → on any error return `dirtyHtml` unchanged.

Then feed the result through the EXISTING `sanitizeHtml()` — the inliner never
bypasses it. Net effect: class/`<style>`-based formatting becomes inline
`style` attributes, which `sanitizeStyle()` then filters prop-by-prop exactly
as it does today (`styleValueIsSafe` still rejects `url(`, `expression(`,
`@import`, `javascript:`, `position:fixed`, `behavior`).

**Shared entry point** so every rich-HTML sink gets the same treatment:

```ts
async function richHtmlToSafeFragment(dirty: string): Promise<string> {
  return sanitizeHtml(await inlineComputedStyles(dirty));
}
```

Wire it into all three sinks:

- **Paste handler** (~4843): the `text/html` branch becomes async — call
  `e.preventDefault()` synchronously, snapshot the current selection range
  (clone it, mirroring the `createLink` pattern at ~4805), await
  `richHtmlToSafeFragment(html)`, restore the range if the selection moved,
  then `execCommand("insertHTML", ...)`. The image branch already uses this
  async-void pattern.
- **Drop handler** (~4889): same change (it already computed a caret position
  from the drop point — keep that logic, just await the fragment first).
- **Reply/forward quote** (~3514, `cBodyInput.innerHTML =
  sanitizeHtml(opts.bodyHtml)`): make the compose-open path await
  `richHtmlToSafeFragment(opts.bodyHtml)` — forwarded newsletters/styled mail
  keep their look in the quote. This call site is inside compose-open setup;
  make the enclosing function async or `.then()` the assignment before the
  autosave baseline snapshot is taken (~3565/3576 uses `cBodyInput.innerHTML`
  as the baseline — the baseline MUST be captured after the async assignment,
  or every untouched reply shows the "unsaved changes" state).

### A2 — Selection-restore correctness (small but load-bearing)

`execCommand("insertHTML")` inserts at the CURRENT selection. Between
`preventDefault()` and the awaited fragment, focus/selection can move (user
clicks elsewhere; autosave doesn't move it, but the settings overlay could).
Rule: snapshot `getSelection().getRangeAt(0)` before the await; after the
await, if the active element is no longer inside `#c-body`, re-focus
`cBodyInput` and restore the snapshot via `removeAllRanges()/addRange()`. If
the snapshot's container was removed from the DOM meanwhile (draft reloaded),
fall back to collapsing the caret to the end of `#c-body`. Keep this in one
helper used by paste + drop.

### A3 — Broaden the allow-lists (exactly this, nothing more)

In `src/main.ts`:

- `ALLOWED_STYLE_PROPS` — add: `line-height`, `list-style-type`,
  `vertical-align`, `text-indent`, `white-space`, `border-collapse`,
  `border-top`, `border-right`, `border-bottom`, `border-left`,
  `border-width`, `border-style`, `border-color`, `text-decoration-line`.
  (`styleValueIsSafe` already blocks `url(` etc. for all of them; `background`
  shorthand stays banned — only `background-color` is allowed.)
- `copyAllowedAttributes()`:
  - `table`: allow `border`, `cellpadding`, `cellspacing`, `width`, `bgcolor`.
  - `td`/`th`: additionally allow `width`, `bgcolor` (extend `TABLE_ATTRS`
    usage or add alongside — keep the existing structure).
  - `font`: additionally allow `face` and `size` (size values `1`–`7` only —
    validate with `/^[1-7]$/`).
  - `bgcolor`/`color` attribute values: accept only `#hex` (3/4/6/8) or
    `[a-zA-Z]+` color names — reject anything else (defence in depth; these
    land in attributes, not style, so `styleValueIsSafe` never sees them).
- `ALLOWED_TAGS` — add `tfoot`, `caption`, `col`, `colgroup` (with `span` and
  `width` attrs for `col`/`colgroup`), `dl`, `dt`, `dd`, `small`, `mark`,
  `ins`, `del`. All structural/benign; `rebuildInto` already unwraps unknowns
  so this only upgrades them from "unwrapped" to "kept".

### A4 — Verification (feature)

There is no TS test harness in this repo (deliberate; do not add one for
this). Verification is live, against the running app (`npm run tauri dev`),
with this checklist — record the outcome in the `CONTEXT.md` progress entry:

1. Paste from **Word** (styled doc: colored text, two fonts, bullet list,
   bordered table) → colors/fonts/list/table borders survive in the editor.
2. Paste from **Excel** (range with cell shading + borders) → grid keeps
   shading/borders.
3. Paste from a **web page** (e.g. a docs page with code blocks) → headings,
   links, code blocks keep their look; NO remote images appear (https images
   are allowed by the sanitizer, data: non-image blocked — unchanged
   behavior).
4. Paste plain text from Notepad → unchanged plain-text behavior.
5. Paste a screenshot image → existing image path still wins (image branch
   runs before the html branch — preserve that order).
6. Reply to a styled HTML email → quoted body keeps its formatting, and the
   compose window does NOT immediately show unsaved-changes/autosave fire
   (baseline race in A1 step 3).
7. Send one of the above to yourself; open in Outlook — formatting present.
8. Hostile-clipboard spot check (paste from a local HTML file opened in a
   browser containing `<style>body{background:url(https://example.com/x)}
   </style><p onclick=alert(1) style="color:red;position:fixed">x</p>
   <script>alert(2)</script>`): no network request fires from the iframe
   (devtools), inserted fragment has the red color, no `onclick`, no
   `position:fixed`, no script.
9. `document.execCommand` still handles undo (Ctrl+Z) after an inlined paste
   as one step — known-good with insertHTML; just confirm no regression.

---

## Part B — Bug fixes (2026-07-18 sweep)

Three independent review lenses (frontend UI, Rust crates, Tauri glue +
cross-boundary contracts) over the whole codebase. Every finding below was
re-verified against the cited source before inclusion. The contract lens
cross-matched all 50 registered commands, every `invoke()` arg casing, every
DTO shape, both emit/listen event pairs, and the capabilities file against the
plugin APIs actually called — all clean, no contract bugs this sweep.

One commit per numbered bug, `fix: <symptom> (#N from PLAN)`.

### #1 — HIGH: rotated refresh token destroyed by non-atomic keyring write

- **Where**: `crates/infrastructure/src/auth/token_store.rs:79-86`
  (`save_refresh_token`), called from `remember` in
  `crates/infrastructure/src/auth/mod.rs` after every successful refresh.
- **Symptom**: a transient Windows Credential Manager error mid-write
  permanently destroys the session — the refresh succeeded at Microsoft, but
  the new token is gone locally → forced full re-sign-in.
- **Defect**: `save_refresh_token` calls `self.clear()?` FIRST (deletes meta +
  all chunk entries), then writes new chunks, meta last. Any `Entry::new` /
  `set_password` failure after the clear leaves the store empty; `remember`
  returns `Err` before the in-memory cache is updated, so both copies of the
  token are lost.
- **Fix**: don't clear first. `set_password` overwrites in place, so: read the
  OLD chunk count from the meta entry (if any) → write the new chunks over
  indices `0..new_len` → write the meta entry (new count) LAST → only then
  delete stale chunk indices `new_len..old_len` (via the existing
  `delete_ignoring_missing`; ignore errors here — stale chunks beyond the meta
  count are unreachable garbage, not corruption). A failure at any step now
  leaves either the old token fully intact (meta not yet rewritten) or the new
  token fully written.
- **Test**: extract the "which indices to write / which to prune" ordering into
  a small pure helper (old_count, new_count) → (write_range, prune_range) and
  unit-test it in `token_store.rs` (keyring itself can't run in CI). Keep
  `clear()` as-is for sign-out.

### #2 — MEDIUM: stale autosave from a closed compose hijacks the next session's draft id

- **Where**: `src/main.ts:3680-3715` (`runAutosave`; the unguarded
  `currentDraftId = id` at :3700) vs `openCompose` (resets `currentDraftId`
  ~:3501) and `closeCompose` (~:3550, cancels only the TIMER, not the
  in-flight `invoke`).
- **Symptom**: edits in a new compose window silently PATCH the previous
  (discarded) session's draft; a resumed draft sits stale in Drafts while the
  user's edits land under an unrelated draft id.
- **Defect**: `runAutosave` checks `composeOverlay` hidden / dirty only BEFORE
  the `await invoke("save_draft", ...)`. Close compose A while its save is in
  flight, open compose B — the overlay is visible again, so when A's save
  resolves it unconditionally writes A's draft id into the global
  `currentDraftId`, and B's subsequent saves/send target A's draft.
- **Fix**: module-level `let composeSession = 0`, incremented in
  `openCompose` AND in `closeCompose`. Snapshot `const session =
  composeSession` at the top of `runAutosave`; after the `await`, bail before
  `currentDraftId = id` (and before the "Draft saved" message +
  `syncDraftsFolder`) unless `session === composeSession`. Same stale-response
  pattern the codebase already uses (`resumeDraft`'s `currentDraftId !== id`
  guard, invite-bar's `selectedId !== id`). Apply the identical snapshot/bail
  to `saveDraft`/`sendCompose` only if tracing shows they can also resolve
  across a session boundary (send closes the modal itself — likely already
  safe; verify, don't assume).
- **Confidence**: mechanism confirmed by trace (single-threaded ordering);
  needs a slow network to trigger live.

### #3 — MEDIUM: Escape to dismiss recipient autocomplete closes the whole compose window

- **Where**: `src/main.ts:4789-4791` (To/Cc/Bcc keydown handler) +
  `src/main.ts:4964-4979` (document-level Escape modal-stack handler,
  compose branch at :4978).
- **Symptom**: focus in To/Cc/Bcc with the correspondent-suggestion dropdown
  open → Escape dismisses the dropdown AND closes compose (instantly, no
  confirm, if nothing is dirty yet — the just-opened reply vanishes) or throws
  an unwanted "Discard this message?" dialog.
- **Defect**: the field handler calls `hideCorrespondentSuggestions()` but
  never stops propagation, so the same keystroke reaches the document handler,
  which sees `composeOverlay` visible and calls `requestCloseCompose()`.
- **Fix**: in the field's Escape branch, when the suggestion list is actually
  visible, call `event.preventDefault(); event.stopPropagation();` then hide
  it. When the list is NOT visible, let the event bubble — Esc-with-no-dropdown
  should still close compose (current modal-stack semantics). `dialog.ts`
  already models this stop-the-leak pattern.

### #4 — LOW: `hasAttachments`-only delta change silently dropped (stale paperclip)

- **Where**: `crates/infrastructure/src/graph/mod.rs:1295-1306`
  (`DeltaItem::is_flags_only_change`) + `crates/domain/src/lib.rs:533-537`
  (`MessageChange::FlagsChanged` carries only `is_read`/`is_flagged`).
- **Symptom**: attach a file to a draft (or receive an attachment change) from
  another client; Graph's delta feed emits `{id, hasAttachments}` only — the
  classifier calls it flags-only, `FlagsChanged` has no field for it, nothing
  updates; the cached list row shows a stale attachment indicator until a full
  re-sync.
- **Fix (laziest correct)**: widen the classifier — treat the item as
  flags-only only when `has_attachments.is_none()` too; a
  `hasAttachments`-bearing delta then falls through to the existing full-upsert
  path. No domain enum change, no new store method.
- **Test**: add a `graph::tests` case: delta item with only `id` +
  `hasAttachments` → NOT classified flags-only.

### #5 — LOW: "load images" banner misses non-canonical remote-image markup

- **Where**: `crates/infrastructure/src/html.rs:218-234` (`has_remote_content`
  / `has_remote_img_src` literal-substring heuristic).
- **Symptom**: a message whose remote image is written `src = "http…"` (spaces)
  or `url( 'http…' )` has the image correctly STRIPPED by ammonia, but the
  heuristic returns false → no "click to load images" banner → the user sees a
  silently missing image with no way to load it (the banner is the only gate).
- **Fix**: normalize before matching — strip whitespace around `=` and inside
  `url(…)` (a small regex or a char-filtered copy of the lowered string), or
  detect during ammonia's own pass. Do NOT touch the sanitizer itself.
- **Test**: extend the existing `html.rs` tests with the spaced variants (both
  should report remote content present).

### #6 — LOW: modal dialogs don't trap Tab focus

- **Where**: `src/dialog.ts` (`build()`/`open()`; `activeKeyHandler` handles
  only Escape/Enter).
- **Symptom**: Tab from the dialog's last button walks focus into the
  background UI while "Discard this message?" / "Permanently delete?" is still
  up; Enter/Space can then activate a background control.
- **Fix**: add a Tab/Shift+Tab branch to the existing capture-phase
  `activeKeyHandler` cycling focus among the dialog's own focusable elements
  (input, Cancel, OK). Skip `inert` — the keydown branch is smaller and stays
  in one file.
- **Confidence**: probable (no trap mechanism exists in code; exact wrap order
  not verified live — verify while testing).

### #7 — LOW (optional): blocking keyring I/O inside async auth paths

- **Where**: `crates/infrastructure/src/auth/mod.rs` `remember`/`sign_out`
  call `TokenStore::save_refresh_token`/`clear` (multiple synchronous
  Credential Manager syscalls) directly on the tokio worker thread; the
  project wraps SQLite in `spawn_blocking` but not keyring.
- **Impact**: normally sub-millisecond; only bites if the credential vault
  stalls (AV/EDR interception). Fix = wrap the store calls in
  `tokio::task::spawn_blocking`, moving the `store_lock` acquisition INSIDE
  the blocking closure (never hold a std mutex across `.await`).
- **Skip-permission**: lowest value of the sweep — implement only if trivial
  after #1's token_store changes land; otherwise leave and note in
  `CONTEXT.md` as an accepted deferral.

### Verified-clean areas (don't re-sweep)

Sanitizer/innerHTML sinks, undo-send timers, folder/account-switch races,
calendar.ts seq guards + timezone handling, all invoke/DTO/event/capability
contracts, tray quit-flush ordering, single-instance + reveal flow, settings
persistence atomicity.

---

## Delivery order

1. Part B bugs, highest severity first (each its own commit).
2. Part A feature (one commit, A1+A2+A3 together — they only make sense
   together).
3. Full verify gate, `CONTEXT.md` progress entry, version bump `0.5.0`
   (feature release), `[release]` commit, tag, publish per
   `wattmail-release-workflow` (single rolling release).
