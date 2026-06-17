import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import { openUrl } from "@tauri-apps/plugin-opener";
import { open, save } from "@tauri-apps/plugin-dialog";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import {
  enable as enableAutostart,
  disable as disableAutostart,
  isEnabled as isAutostartEnabled,
} from "@tauri-apps/plugin-autostart";
import "./styles.css";

// ---- Backend DTOs (mirror src-tauri/src/commands.rs) ----
interface Account {
  displayName: string;
  email: string;
}
interface Message {
  id: string;
  subject: string;
  from: string;
  to: string;
  received: string; // ISO-8601
  preview: string;
  isRead: boolean;
  isFlagged: boolean;
}
interface Inbox {
  account: Account | null;
  messages: Message[];
  total: number; // total cached in the folder; messages is a window of this
}
interface MessageView {
  id: string;
  subject: string;
  from: string;
  to: string[];
  received: string;
  html: string; // already sanitized in Rust
  remoteBlocked: boolean;
}
interface FolderInfo {
  id: string;
  name: string;
  unreadCount: number;
  depth: number;
}
interface ComposeData {
  to: string[];
  cc: string[];
  subject: string;
  quotedHtml: string;
}
interface DraftPrefill {
  to: string[];
  cc: string[];
  subject: string;
  bodyHtml: string; // raw, unsanitized body for the editor
}
interface AttachmentInfo {
  id: string;
  name: string;
  contentType: string;
  size: number;
}
interface HeaderItem {
  name: string;
  value: string;
}

// The whole folder is cached locally; the list reads a growing window of it.
// "Load more" grows the window by PAGE_SIZE; switching folders resets it.
const PAGE_SIZE = 50;
let loadedCount = PAGE_SIZE;
let currentTotal = 0;

// ---- Theme (also applied pre-paint in index.html; this keeps it in sync) ----
type ThemePref = "business" | "corporate" | "system";
const THEME_KEY = "wattmail.theme";

function resolveTheme(pref: ThemePref): "business" | "corporate" {
  if (pref === "system") {
    return matchMedia("(prefers-color-scheme: dark)").matches ? "business" : "corporate";
  }
  return pref;
}
function loadThemePref(): ThemePref {
  const v = localStorage.getItem(THEME_KEY);
  return v === "corporate" || v === "system" ? v : "business";
}
function applyThemePref(pref: ThemePref): void {
  document.documentElement.dataset.theme = resolveTheme(pref);
}
function setThemePref(pref: ThemePref): void {
  localStorage.setItem(THEME_KEY, pref);
  applyThemePref(pref);
}
// Re-resolve on OS theme change while following the system.
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
  if (loadThemePref() === "system") applyThemePref("system");
});

// ---- Formatting ----
function fmtDate(iso: string): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const p = (x: number): string => x.toString().padStart(2, "0");
  const sameDay = d.toDateString() === new Date().toDateString();
  return sameDay
    ? `${p(d.getHours())}:${p(d.getMinutes())}`
    : `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}
function fmtDateFull(iso: string): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const p = (x: number): string => x.toString().padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}
function esc(s: string): string {
  return s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]!);
}
function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}
// "Name <addr>" -> "Name"; a bare address stays as-is.
function senderName(from: string): string {
  const lt = from.indexOf("<");
  const name = lt > 0 ? from.slice(0, lt).trim() : from.trim();
  return name || from;
}
// Outgoing folders show the recipient ("To: …") rather than the sender.
function isOutgoingFolder(name: string): boolean {
  const n = name.toLowerCase();
  return n === "sent items" || n === "sent" || n === "drafts" || n === "outbox";
}
// The Drafts folder: clicking a row resumes editing in compose, not the reader.
// (Matched by displayName, consistent with the app's other folder special-casing.)
function isDraftsFolder(name: string): boolean {
  return name.toLowerCase() === "drafts";
}
function currentFolderIsDrafts(): boolean {
  const folder = folders.find((f) => f.id === currentFolderId);
  return !!folder && isDraftsFolder(folder.name);
}

// ---- Sort (client-side, over the loaded window) ----
type SortMode = "dateDesc" | "dateAsc" | "sender" | "subject" | "unread";
const SORT_KEY = "wattmail.sort";
function loadSortMode(): SortMode {
  const v = localStorage.getItem(SORT_KEY);
  return v === "dateAsc" || v === "sender" || v === "subject" || v === "unread" ? v : "dateDesc";
}
let sortMode: SortMode = loadSortMode();

function sortMessages(messages: Message[]): Message[] {
  const arr = [...messages];
  switch (sortMode) {
    case "dateAsc":
      return arr.sort((a, b) => a.received.localeCompare(b.received));
    case "sender":
      return arr.sort((a, b) => senderName(a.from).localeCompare(senderName(b.from)));
    case "subject":
      return arr.sort((a, b) => a.subject.localeCompare(b.subject));
    case "unread":
      return arr.sort(
        (a, b) => Number(a.isRead) - Number(b.isRead) || b.received.localeCompare(a.received),
      );
    default:
      return arr.sort((a, b) => b.received.localeCompare(a.received));
  }
}

// ---- App shell ----
const appRoot = document.querySelector<HTMLDivElement>("#app")!;
appRoot.innerHTML = /* html */ `
  <div class="flex flex-col h-screen bg-base-100 text-base-content">
    <div id="update-banner" class="update-banner hidden">
      <span id="update-text"></span>
      <div class="update-actions">
        <button id="update-install" class="btn btn-xs btn-primary">Install &amp; restart</button>
        <button id="update-later" class="btn btn-xs btn-ghost">Later</button>
      </div>
    </div>
    <div class="flex items-center gap-2 p-2 border-b border-base-300">
      <div class="toolbar-brand">
        <span class="brand-name">WattMail</span>
        <span id="brand-version" class="brand-version"></span>
      </div>
      <div id="account" class="text-xs opacity-70 truncate flex-1"></div>
      <div class="toolbar-search">
        <input id="search" class="input input-bordered input-xs" type="search" placeholder="Search mail…" autocomplete="off" />
        <button id="search-clear" class="search-clear hidden" type="button" title="Clear search">&times;</button>
      </div>
      <select id="sort" class="select select-bordered select-xs" title="Sort by">
        <option value="dateDesc">Newest</option>
        <option value="dateAsc">Oldest</option>
        <option value="sender">Sender</option>
        <option value="subject">Subject</option>
        <option value="unread">Unread first</option>
      </select>
      <button id="compose" class="btn btn-xs btn-primary" title="Compose">&#9993; Compose</button>
      <button id="refresh" class="btn btn-xs" title="Refresh">&#8635; Refresh</button>
      <button id="gear" class="btn btn-xs" title="Settings">&#9881;</button>
    </div>

    <div id="signin" class="flex-1 flex flex-col items-center justify-center gap-4 hidden">
      <div class="text-lg opacity-80">Welcome to WattMail</div>
      <button id="signin-btn" class="btn btn-primary">Sign in to Office 365</button>
      <div id="signin-msg" class="text-xs opacity-60"></div>
    </div>

    <div id="main" class="flex flex-1 min-h-0">
      <div id="folders" class="w-[200px] shrink-0 overflow-y-auto scroll-thin border-r border-base-300 py-1"></div>
      <div id="list" class="shrink-0 overflow-y-auto scroll-thin border-r border-base-300"></div>
      <div id="splitter" class="splitter" title="Drag to resize"></div>
      <div id="reader" class="flex-1 flex flex-col min-w-0"></div>
    </div>

    <div id="status" class="text-xs px-3 py-1 border-t border-base-300 bg-base-200 opacity-80"></div>
  </div>

  <div id="settings-overlay" class="overlay hidden">
    <div class="settings-panel">
      <div class="settings-title">Settings</div>
      <label class="settings-row">
        <span>Theme<br /><span class="hint">Light, dark, or follow Windows</span></span>
        <select id="set-theme" class="select select-bordered select-sm">
          <option value="business">Dark</option>
          <option value="corporate">Light</option>
          <option value="system">System</option>
        </select>
      </label>
      <label class="settings-row">
        <span>Close button minimises to tray<br /><span class="hint">Otherwise closing the window quits WattMail</span></span>
        <input type="checkbox" id="set-tray" class="toggle toggle-sm toggle-primary" />
      </label>
      <label class="settings-row">
        <span>Start with Windows<br /><span class="hint">Launch hidden in the system tray at sign-in</span></span>
        <input type="checkbox" id="set-autostart" class="toggle toggle-sm toggle-primary" />
      </label>
      <div class="settings-row">
        <span>Account<br /><span class="hint" id="set-account">&mdash;</span></span>
        <button id="signout-btn" class="btn btn-sm btn-error">Sign out</button>
      </div>
      <div id="settings-msg" class="settings-msg"></div>
      <div class="settings-actions"><button id="settings-close" class="btn btn-sm">Close</button></div>
    </div>
  </div>

  <div id="compose-overlay" class="overlay hidden">
    <div class="settings-panel compose-panel" id="compose-panel">
      <div class="compose-head">
        <div class="settings-title" id="compose-title">New message</div>
        <button id="compose-maximize" class="compose-maximize" type="button" title="Maximize" aria-label="Maximize">&#9974;</button>
      </div>
      <input id="c-to" class="input input-bordered input-sm compose-input" placeholder="To (comma-separated)" autocomplete="off" />
      <input id="c-cc" class="input input-bordered input-sm compose-input" placeholder="Cc" autocomplete="off" />
      <input id="c-subject" class="input input-bordered input-sm compose-input" placeholder="Subject" autocomplete="off" />
      <div id="c-toolbar" class="compose-toolbar">
        <button type="button" data-cmd="bold" title="Bold"><b>B</b></button>
        <button type="button" data-cmd="italic" title="Italic"><i>I</i></button>
        <button type="button" data-cmd="underline" title="Underline"><u>U</u></button>
        <button type="button" data-cmd="insertUnorderedList" title="Bulleted list">&bull;</button>
        <button type="button" data-cmd="insertOrderedList" title="Numbered list">1.</button>
        <button type="button" data-cmd="createLink" title="Insert link">&#128279;</button>
        <button type="button" data-cmd="removeFormat" title="Clear formatting">&#10005;</button>
      </div>
      <div id="c-body" class="compose-body" contenteditable="true" role="textbox" aria-label="Message body"></div>
      <div class="compose-attach-row">
        <button id="c-attach" class="btn btn-xs" type="button">&#128206; Attach</button>
        <div id="c-attachments" class="compose-attachments"></div>
      </div>
      <div id="compose-msg" class="settings-msg"></div>
      <div class="settings-actions" style="gap: 8px">
        <button id="compose-cancel" class="btn btn-sm">Cancel</button>
        <button id="compose-savedraft" class="btn btn-sm">Save draft</button>
        <button id="compose-send" class="btn btn-sm btn-primary">Send</button>
      </div>
      <div id="compose-resize" class="compose-resize" title="Drag to resize" aria-hidden="true"></div>
    </div>
  </div>

  <div id="headers-overlay" class="overlay hidden">
    <div class="settings-panel headers-panel">
      <div class="headers-head">
        <div class="settings-title" id="headers-title">Message headers</div>
        <div class="headers-tools">
          <input id="headers-filter" class="input input-bordered input-xs" placeholder="Filter raw headers…" autocomplete="off" />
          <button id="headers-copy" class="btn btn-xs" title="Copy all headers to the clipboard">Copy all</button>
          <button id="headers-close" class="btn btn-xs">Close</button>
        </div>
      </div>
      <div id="headers-body" class="headers-body"></div>
    </div>
  </div>
`;

const accountEl = document.querySelector<HTMLDivElement>("#account")!;
const brandVersion = document.querySelector<HTMLSpanElement>("#brand-version")!;
const refreshBtn = document.querySelector<HTMLButtonElement>("#refresh")!;
const gear = document.querySelector<HTMLButtonElement>("#gear")!;
const sortSelect = document.querySelector<HTMLSelectElement>("#sort")!;
const searchInput = document.querySelector<HTMLInputElement>("#search")!;
const searchClearBtn = document.querySelector<HTMLButtonElement>("#search-clear")!;
const updateBanner = document.querySelector<HTMLDivElement>("#update-banner")!;
const updateText = document.querySelector<HTMLSpanElement>("#update-text")!;
const updateInstall = document.querySelector<HTMLButtonElement>("#update-install")!;
const updateLater = document.querySelector<HTMLButtonElement>("#update-later")!;
const signinView = document.querySelector<HTMLDivElement>("#signin")!;
const signinBtn = document.querySelector<HTMLButtonElement>("#signin-btn")!;
const signinMsg = document.querySelector<HTMLDivElement>("#signin-msg")!;
const mainView = document.querySelector<HTMLDivElement>("#main")!;
const foldersEl = document.querySelector<HTMLDivElement>("#folders")!;
const listEl = document.querySelector<HTMLDivElement>("#list")!;
const splitter = document.querySelector<HTMLDivElement>("#splitter")!;
const readerEl = document.querySelector<HTMLDivElement>("#reader")!;
const statusEl = document.querySelector<HTMLDivElement>("#status")!;
const settingsOverlay = document.querySelector<HTMLDivElement>("#settings-overlay")!;
const setTheme = document.querySelector<HTMLSelectElement>("#set-theme")!;
const setTray = document.querySelector<HTMLInputElement>("#set-tray")!;
const setAutostart = document.querySelector<HTMLInputElement>("#set-autostart")!;
const setAccount = document.querySelector<HTMLSpanElement>("#set-account")!;
const signoutBtn = document.querySelector<HTMLButtonElement>("#signout-btn")!;
const settingsMsg = document.querySelector<HTMLDivElement>("#settings-msg")!;
const settingsClose = document.querySelector<HTMLButtonElement>("#settings-close")!;
const composeBtn = document.querySelector<HTMLButtonElement>("#compose")!;
const composeOverlay = document.querySelector<HTMLDivElement>("#compose-overlay")!;
const composePanel = document.querySelector<HTMLDivElement>("#compose-panel")!;
const composeMaximizeBtn = document.querySelector<HTMLButtonElement>("#compose-maximize")!;
const composeResizeGrip = document.querySelector<HTMLDivElement>("#compose-resize")!;
const composeTitle = document.querySelector<HTMLDivElement>("#compose-title")!;
const cToInput = document.querySelector<HTMLInputElement>("#c-to")!;
const cCcInput = document.querySelector<HTMLInputElement>("#c-cc")!;
const cSubjectInput = document.querySelector<HTMLInputElement>("#c-subject")!;
const cBodyInput = document.querySelector<HTMLDivElement>("#c-body")!;
const composeToolbar = document.querySelector<HTMLDivElement>("#c-toolbar")!;
const composeMsg = document.querySelector<HTMLDivElement>("#compose-msg")!;
const composeCancel = document.querySelector<HTMLButtonElement>("#compose-cancel")!;
const composeSaveDraftBtn = document.querySelector<HTMLButtonElement>("#compose-savedraft")!;
const composeSendBtn = document.querySelector<HTMLButtonElement>("#compose-send")!;
const cAttachBtn = document.querySelector<HTMLButtonElement>("#c-attach")!;
const cAttachments = document.querySelector<HTMLDivElement>("#c-attachments")!;
const headersOverlay = document.querySelector<HTMLDivElement>("#headers-overlay")!;
const headersTitle = document.querySelector<HTMLDivElement>("#headers-title")!;
const headersBody = document.querySelector<HTMLDivElement>("#headers-body")!;
const headersFilter = document.querySelector<HTMLInputElement>("#headers-filter")!;
const headersCopyBtn = document.querySelector<HTMLButtonElement>("#headers-copy")!;
const headersCloseBtn = document.querySelector<HTMLButtonElement>("#headers-close")!;

// ---- View state ----
let selectedId: string | null = null;
let lastMessage: MessageView | null = null;
let currentIds = new Set<string>();
let currentFolderId: string | null = null;
let folders: FolderInfo[] = [];
let accountEmail = "";
// Keyboard-navigation cursor: the id of the row the cursor is on, kept distinct
// from `selectedId` (the opened/read message) so j/k can move without opening.
// Reconciled against the rendered rows on every list re-render (see syncCursor).
let cursorId: string | null = null;

function showSignedOut(): void {
  signinView.classList.remove("hidden");
  mainView.classList.add("hidden");
  refreshBtn.classList.add("hidden");
  composeBtn.classList.add("hidden");
  accountEl.textContent = "";
  accountEmail = "";
  currentFolderId = null;
  folders = [];
  foldersEl.innerHTML = "";
  searchActive = false;
  searchSeq++;
  searchInput.value = "";
  setSearchClearVisible(false);
  void invoke("set_unread", { count: 0 }).catch(() => {});
  resetReader();
  statusEl.textContent = "Not signed in";
}
function showSignedIn(): void {
  signinView.classList.add("hidden");
  mainView.classList.remove("hidden");
  refreshBtn.classList.remove("hidden");
  composeBtn.classList.remove("hidden");
}

function resetReader(): void {
  selectedId = null;
  lastMessage = null;
  readerEl.innerHTML = `<div class="reader-empty">Select a message to read</div>`;
}

// ---- Message list ----
// Render one list row. `showRecipient` shows "To: …" instead of the sender (used
// by outgoing folders). Search results use the sender form.
function messageRowHtml(m: Message, showRecipient: boolean): string {
  const unread = m.isRead ? "" : "unread";
  const flagged = m.isFlagged ? " flagged" : "";
  const dot = m.isRead ? "" : `<span class="dot"></span>`;
  const flag = m.isFlagged ? `<span class="msg-flag" title="Flagged for follow-up">&#9873;</span>` : "";
  const who = showRecipient ? `To: ${esc(m.to)}` : esc(senderName(m.from));
  const whoTitle = showRecipient ? esc(m.to) : esc(m.from);
  return `
        <div class="msg ${unread}${flagged}" data-id="${esc(m.id)}">
          <div class="msg-dot">${dot}</div>
          <div class="msg-main">
            <div class="msg-top">
              <span class="msg-from" title="${whoTitle}">${who}</span>
              <span class="msg-date">${flag}${esc(fmtDate(m.received))}</span>
            </div>
            <div class="msg-subject" title="${esc(m.subject)}">${esc(m.subject)}</div>
            <div class="msg-preview">${esc(m.preview)}</div>
          </div>
        </div>`;
}

function renderInbox(inbox: Inbox): void {
  if (inbox.account) {
    accountEmail = inbox.account.email;
    accountEl.textContent = `${inbox.account.displayName} · ${inbox.account.email}`;
    setAccount.textContent = inbox.account.email;
  } else {
    accountEl.textContent = "";
  }
  currentIds = new Set(inbox.messages.map((m) => m.id));
  currentTotal = inbox.total;

  if (inbox.messages.length === 0) {
    listEl.innerHTML = `<div class="p-6 text-center opacity-60">No messages.</div>`;
    resetReader();
    syncCursor();
    return;
  }

  const folder = folders.find((f) => f.id === currentFolderId);
  const showRecipient = !!folder && isOutgoingFolder(folder.name);

  const rows = sortMessages(inbox.messages)
    .map((m) => messageRowHtml(m, showRecipient))
    .join("");

  const remaining = inbox.total - inbox.messages.length;
  const more =
    remaining > 0
      ? `<button class="load-more" data-role="load-more">Load ${Math.min(remaining, PAGE_SIZE)} more (${remaining} older)</button>`
      : "";
  listEl.innerHTML = rows + more;

  // A refresh may have dropped the open message; clear the reader if so.
  if (selectedId && !currentIds.has(selectedId)) resetReader();
  highlightSelected();
  syncCursor();
}

function rowFor(id: string): HTMLElement | null {
  for (const el of listEl.querySelectorAll<HTMLElement>(".msg")) {
    if (el.dataset.id === id) return el;
  }
  return null;
}
function highlightSelected(): void {
  listEl.querySelectorAll<HTMLElement>(".msg").forEach((el) => {
    el.classList.toggle("selected", el.dataset.id === selectedId);
  });
}

// ---- Keyboard-navigation cursor ----
// The rendered rows in display order — the basis for cursor movement, so j/k
// follow the same order the user sees (sort/search aware).
function rowEls(): HTMLElement[] {
  return Array.from(listEl.querySelectorAll<HTMLElement>(".msg"));
}
function highlightCursor(): void {
  rowEls().forEach((el) => el.classList.toggle("cursor", el.dataset.id === cursorId));
}
// Re-anchor the cursor after a list re-render: keep it on the same message if it
// survived, else fall back to the first row (or clear it when the list is empty).
function syncCursor(): void {
  const rows = rowEls();
  if (rows.length === 0) {
    cursorId = null;
  } else if (!cursorId || !rows.some((el) => el.dataset.id === cursorId)) {
    cursorId = rows[0].dataset.id ?? null;
  }
  highlightCursor();
}
// Move the cursor by a signed step, clamped to the list bounds, scrolling the
// new row into view. No-op when the list is empty.
function moveCursor(step: number): void {
  const rows = rowEls();
  if (rows.length === 0) return;
  const current = rows.findIndex((el) => el.dataset.id === cursorId);
  const next = current < 0 ? 0 : Math.min(rows.length - 1, Math.max(0, current + step));
  cursorId = rows[next].dataset.id ?? null;
  highlightCursor();
  rows[next].scrollIntoView({ block: "nearest" });
}
// Open the row under the cursor, mirroring a click: drafts resume editing.
function activateCursor(): void {
  if (!cursorId) return;
  if (!searchActive && currentFolderIsDrafts()) void resumeDraft(cursorId);
  else void openMessage(cursorId);
}
// Optimistically mark a row read in the UI, then tell the server.
function markRead(id: string): void {
  const row = rowFor(id);
  if (!row?.classList.contains("unread")) return;
  row.classList.remove("unread");
  const dot = row.querySelector(".msg-dot");
  if (dot) dot.innerHTML = "";
  void invoke("set_read", { id, read: true }).catch(() => {});
}

// Set a message's read state (context-menu action), reflecting it optimistically.
async function setRead(id: string, read: boolean): Promise<void> {
  const row = rowFor(id);
  if (row) {
    row.classList.toggle("unread", !read);
    const dot = row.querySelector(".msg-dot");
    if (dot) dot.innerHTML = read ? "" : `<span class="dot"></span>`;
  }
  try {
    await invoke("set_read", { id, read });
    await loadFolders(); // refresh unread badges
  } catch (e) {
    statusEl.textContent = `Could not update message: ${e}`;
    await revertOptimisticAction();
  }
}

// Toggle a message's follow-up flag (context-menu action), reflecting it
// optimistically on the row and reconciling from the cache on failure.
async function setFlag(id: string, flagged: boolean): Promise<void> {
  const row = rowFor(id);
  if (row) {
    row.classList.toggle("flagged", flagged);
    const dateEl = row.querySelector(".msg-date");
    if (dateEl) {
      dateEl.querySelector(".msg-flag")?.remove();
      if (flagged) {
        dateEl.insertAdjacentHTML(
          "afterbegin",
          `<span class="msg-flag" title="Flagged for follow-up">&#9873;</span>`,
        );
      }
    }
  }
  try {
    await invoke("set_flag", { id, flagged });
  } catch (e) {
    statusEl.textContent = `Could not update flag: ${e}`;
    await revertOptimisticAction();
  }
}

// Delete a message (moves it to Deleted Items), updating the list optimistically.
async function deleteMessage(id: string): Promise<void> {
  rowFor(id)?.remove();
  if (selectedId === id) resetReader();
  try {
    await invoke("delete_message", { id });
    await reconcileAfterAction(); // update loaded/total count and unread badges
  } catch (e) {
    statusEl.textContent = `Delete failed: ${e}`;
    await revertOptimisticAction(); // restore the removed row
  }
}

// Move a message to another folder (it leaves the current folder's list).
async function moveMessage(id: string, destinationFolderId: string): Promise<void> {
  rowFor(id)?.remove();
  if (selectedId === id) resetReader();
  try {
    await invoke("move_message", { id, destinationFolderId });
    await reconcileAfterAction(); // update loaded/total count and unread badges
  } catch (e) {
    statusEl.textContent = `Move failed: ${e}`;
    await revertOptimisticAction(); // restore the removed row
  }
}

// ---- Search ----
// Cross-folder search via live Graph $search. Results bypass the local cache and
// render into the list area; while a search is active the folder auto-sync must
// not clobber them. Clicking a result opens it by id (works across folders).
const SEARCH_TOP = 50;
const SEARCH_DEBOUNCE_MS = 300;
let searchActive = false;
let searchTimer: ReturnType<typeof setTimeout> | null = null;
let searchSeq = 0; // discards stale responses when the query changes mid-flight

function setSearchClearVisible(visible: boolean): void {
  searchClearBtn.classList.toggle("hidden", !visible);
}

// Render search results into the list area, reusing the folder list-row markup.
function renderSearchResults(query: string, results: Message[]): void {
  currentIds = new Set(results.map((m) => m.id));
  const header = `<div class="search-header">Search results for: ${esc(query)} (${results.length})</div>`;
  if (results.length === 0) {
    listEl.innerHTML = header + `<div class="p-6 text-center opacity-60">No matching messages.</div>`;
    if (selectedId && !currentIds.has(selectedId)) resetReader();
    syncCursor();
    return;
  }
  const rows = sortMessages(results)
    .map((m) => messageRowHtml(m, false))
    .join("");
  listEl.innerHTML = header + rows;
  if (selectedId && !currentIds.has(selectedId)) resetReader();
  highlightSelected();
  syncCursor();
}

async function runSearch(query: string): Promise<void> {
  const trimmed = query.trim();
  if (!trimmed) {
    exitSearch();
    return;
  }
  searchActive = true;
  const seq = ++searchSeq;
  listEl.innerHTML =
    `<div class="search-header">Search results for: ${esc(trimmed)}</div>` +
    `<div class="p-6 text-center opacity-60">Searching…</div>`;
  try {
    const results = await invoke<Message[]>("search_messages", { query: trimmed, top: SEARCH_TOP });
    if (seq !== searchSeq || !searchActive) return; // superseded or cleared
    renderSearchResults(trimmed, results);
    statusEl.textContent = `${results.length} search result(s) for "${trimmed}"`;
  } catch (e) {
    if (seq !== searchSeq || !searchActive) return;
    listEl.innerHTML =
      `<div class="search-header">Search results for: ${esc(trimmed)}</div>` +
      `<div class="p-6 text-center opacity-60">Search failed: ${esc(String(e))}</div>`;
    statusEl.textContent = `Search failed: ${e}`;
  }
}

// Leave search mode and restore the current folder's cached view.
function exitSearch(): void {
  if (searchTimer) {
    clearTimeout(searchTimer);
    searchTimer = null;
  }
  searchSeq++; // invalidate any in-flight response
  const wasActive = searchActive;
  searchActive = false;
  searchInput.value = "";
  setSearchClearVisible(false);
  if (wasActive) void refreshFromCache().catch(() => {});
}

// ---- Reading pane ----
async function openMessage(id: string, allowImages = false): Promise<void> {
  selectedId = id;
  highlightSelected();
  readerEl.innerHTML = `<div class="reader-empty">Loading…</div>`;
  try {
    const msg = await invoke<MessageView>("load_message", { id, allowImages });
    if (selectedId !== id) return; // superseded by another click
    renderReader(msg);
    markRead(id);
    void loadAttachments(id);
  } catch (e) {
    readerEl.innerHTML = `<div class="reader-empty">Failed to load message: ${esc(String(e))}</div>`;
  }
}

function renderReader(msg: MessageView): void {
  lastMessage = msg;
  const to = msg.to.length ? ` · to ${esc(msg.to.join(", "))}` : "";
  const banner = msg.remoteBlocked
    ? `<button id="load-images" class="reader-banner" type="button">Images blocked &mdash; click to load images for this message</button>`
    : "";
  readerEl.innerHTML = `
    <div class="reader-head">
      <div class="reader-subject">${esc(msg.subject)}</div>
      <div class="reader-meta">${esc(msg.from)}</div>
      <div class="reader-meta">${esc(fmtDateFull(msg.received))}${to}</div>
      <div class="reader-actions">
        <button id="reply-btn" class="btn btn-xs">Reply</button>
        <button id="reply-all-btn" class="btn btn-xs">Reply all</button>
        <button id="forward-btn" class="btn btn-xs">Forward</button>
        <button id="headers-btn" class="btn btn-xs btn-ghost" title="View raw message headers and trace its origin">Headers</button>
      </div>
    </div>
    <div id="reader-attachments" class="reader-attachments"></div>
    ${banner}
    <iframe class="reader-frame" sandbox="allow-same-origin" referrerpolicy="no-referrer"></iframe>
  `;
  readerEl
    .querySelector<HTMLButtonElement>("#load-images")
    ?.addEventListener("click", () => void openMessage(msg.id, true));
  readerEl
    .querySelector<HTMLButtonElement>("#reply-btn")
    ?.addEventListener("click", () => void replyTo(false));
  readerEl
    .querySelector<HTMLButtonElement>("#reply-all-btn")
    ?.addEventListener("click", () => void replyTo(true));
  readerEl
    .querySelector<HTMLButtonElement>("#forward-btn")
    ?.addEventListener("click", () => void forwardMsg());
  readerEl
    .querySelector<HTMLButtonElement>("#headers-btn")
    ?.addEventListener("click", () => void openHeaders(msg.id, msg.subject));

  const frame = readerEl.querySelector<HTMLIFrameElement>(".reader-frame")!;
  frame.addEventListener("load", () => wireFrameLinks(frame));
  frame.srcdoc = wrapEmailHtml(msg.html);
}

// Intercept clicks inside the (script-disabled, same-origin) email frame so links
// open in the system browser — where the user can see the real destination —
// instead of navigating the frame.
function wireFrameLinks(frame: HTMLIFrameElement): void {
  const doc = frame.contentDocument;
  if (!doc) return;
  doc.addEventListener("click", (ev) => {
    ev.preventDefault();
    const anchor = (ev.target as HTMLElement | null)?.closest?.("a");
    const href = anchor?.getAttribute("href") ?? "";
    if (/^https?:\/\//i.test(href)) void openUrl(href);
  });
}

async function loadAttachments(messageId: string): Promise<void> {
  let list: AttachmentInfo[];
  try {
    list = await invoke<AttachmentInfo[]>("attachments", { messageId });
  } catch {
    return; // attachments are best-effort; don't disrupt the reader
  }
  if (selectedId !== messageId) return;
  const container = document.querySelector<HTMLDivElement>("#reader-attachments");
  if (!container) return;
  container.innerHTML = list
    .map(
      (a) =>
        `<button class="attach-chip" data-aid="${esc(a.id)}" data-name="${esc(a.name)}" title="Download ${esc(a.name)}">&#128206; ${esc(a.name)} <span class="attach-size">${fmtBytes(a.size)}</span></button>`,
    )
    .join("");
  container.querySelectorAll<HTMLButtonElement>(".attach-chip").forEach((b) => {
    b.addEventListener("click", () => void downloadAttachment(messageId, b.dataset.aid!, b.dataset.name!));
  });
}

async function downloadAttachment(messageId: string, attachmentId: string, name: string): Promise<void> {
  try {
    const path = await save({ defaultPath: name });
    if (!path) return; // cancelled
    await invoke("save_attachment", { messageId, attachmentId, destPath: path });
    statusEl.textContent = `Saved ${name}`;
  } catch (e) {
    statusEl.textContent = `Download failed: ${e}`;
  }
}

// Email bodies render on a light background regardless of app theme: email HTML
// assumes a light theme, so authors' own colours (often dark text with no
// background) only read correctly on light.
function wrapEmailHtml(inner: string): string {
  return `<!doctype html><html><head><meta charset="utf-8" />
<meta name="referrer" content="no-referrer" />
<style>
  html, body { margin: 0; }
  body {
    padding: 16px;
    font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
    font-size: 14px; line-height: 1.55; color: #1a1a1a; background: #ffffff;
    word-wrap: break-word; overflow-wrap: anywhere;
  }
  a { color: #2563eb; }
  img { max-width: 100%; height: auto; }
  table { max-width: 100%; }
  pre { white-space: pre-wrap; }
  blockquote { margin: 0 0 0 12px; padding-left: 12px; border-left: 3px solid #cbd5e1; color: #475569; }
</style></head><body>${inner}</body></html>`;
}

// ---- Message headers viewer ----
// Fetch a message's raw internet headers and present a trace-oriented view: a
// parsed summary of the fields that reveal where a message came from and why it
// was filtered, plus the full raw header list with the key fields highlighted.

// Header names worth highlighting in the raw list when tracing a message.
const KEY_HEADERS = new Set([
  "received",
  "authentication-results",
  "received-spf",
  "return-path",
  "from",
  "sender",
  "reply-to",
  "to",
  "delivered-to",
  "dkim-signature",
  "message-id",
  "date",
  "x-forefront-antispam-report",
  "x-microsoft-antispam",
]);

// Microsoft spam-filter verdict categories (the CAT field), expanded to plain
// English. Unknown codes are shown as-is.
const CAT_MEANINGS: Record<string, string> = {
  SPM: "spam",
  HSPM: "high-confidence spam",
  PHSH: "phishing",
  MALW: "malware",
  SPOOF: "spoofing",
  BULK: "bulk mail",
  DIMP: "domain impersonation",
  UIMP: "user impersonation",
  GIMP: "intra-org impersonation",
  AMP: "anti-malware policy",
};

let currentHeaders: HeaderItem[] = [];

async function openHeaders(id: string, subject: string): Promise<void> {
  // textContent (not innerHTML) — the subject is shown verbatim, never parsed.
  headersTitle.textContent = subject ? `Message headers — ${subject}` : "Message headers";
  headersFilter.value = "";
  headersBody.innerHTML = `<div class="headers-loading">Loading headers…</div>`;
  headersOverlay.classList.remove("hidden");
  try {
    currentHeaders = await invoke<HeaderItem[]>("message_headers", { id });
  } catch (e) {
    headersBody.innerHTML = `<div class="headers-loading">Could not load headers: ${esc(String(e))}</div>`;
    return;
  }
  if (currentHeaders.length === 0) {
    headersBody.innerHTML = `<div class="headers-loading">No internet headers are available for this message.</div>`;
    return;
  }
  renderHeaders(subject);
}

function closeHeaders(): void {
  headersOverlay.classList.add("hidden");
}

// First / all values for a header name (case-insensitive). Repeated headers
// (notably Received) keep their provider order.
function headerValue(name: string): string | null {
  const lower = name.toLowerCase();
  return currentHeaders.find((h) => h.name.toLowerCase() === lower)?.value ?? null;
}
function headerValues(name: string): string[] {
  const lower = name.toLowerCase();
  return currentHeaders.filter((h) => h.name.toLowerCase() === lower).map((h) => h.value);
}

// The address the message was actually delivered to — an explicit envelope
// header if present, else the `for <addr>` clause of the Received chain. This
// is what explains a message arriving at an address that isn't your own.
function deliveredTo(): string | null {
  const explicit =
    headerValue("Delivered-To") ?? headerValue("X-Envelope-To") ?? headerValue("X-Original-To");
  if (explicit) return explicit;
  for (const received of headerValues("Received")) {
    const m = received.match(/for\s+<?([^\s;>]+@[^\s;>]+)>?/i);
    if (m) return m[1];
  }
  return null;
}

// Pull the spf / dkim / dmarc / compauth verdicts out of Authentication-Results.
interface Verdict {
  method: string;
  result: string;
}
// Collect spf/dkim/dmarc/compauth verdicts across *every* Authentication-Results
// header (Exchange Online can stamp several — ARC, connectors, forwarding) and
// every token within each (a header may carry one `dkim=` per signature). Each
// distinct method+result is surfaced once, so a passing and a failing signature
// both show instead of the first silently winning.
function parseAuthResults(): Verdict[] {
  const raw = headerValues("Authentication-Results").join("; ");
  if (!raw) return [];
  const out: Verdict[] = [];
  const seen = new Set<string>();
  for (const method of ["spf", "dkim", "dmarc", "compauth"]) {
    for (const m of raw.matchAll(new RegExp(`\\b${method}=([a-z]+)`, "gi"))) {
      const result = m[1].toLowerCase();
      const key = `${method}:${result}`;
      if (!seen.has(key)) {
        seen.add(key);
        out.push({ method: method.toUpperCase(), result });
      }
    }
  }
  return out;
}
function verdictClass(result: string): string {
  if (result === "pass") return "pass";
  if (result === "fail" || result === "hardfail" || result === "permerror") return "fail";
  if (result === "none" || result === "neutral") return "neutral";
  return "warn"; // softfail, temperror, bestguesspass, …
}

// Split a `key:value;key:value` header (e.g. X-Forefront-Antispam-Report) into a
// keyed map. Keys are upper-cased; values keep their case.
function parseSegments(raw: string): Map<string, string> {
  const map = new Map<string, string>();
  for (const part of raw.split(";")) {
    const idx = part.indexOf(":");
    if (idx > 0) map.set(part.slice(0, idx).trim().toUpperCase(), part.slice(idx + 1).trim());
  }
  return map;
}
function sclMeaning(scl: string): string {
  const n = Number.parseInt(scl, 10);
  if (!Number.isFinite(n)) return "";
  if (n < 0) return "trusted / safe-listed";
  if (n <= 1) return "not spam";
  if (n >= 9) return "high-confidence spam";
  if (n >= 5) return "spam";
  return "borderline";
}
function bclMeaning(bcl: string): string {
  const n = Number.parseInt(bcl, 10);
  if (!Number.isFinite(n)) return "";
  if (n === 0) return "not from a bulk sender";
  if (n <= 3) return "low-volume bulk";
  if (n <= 7) return "moderate bulk";
  return "high-volume bulk";
}

interface Hop {
  from?: string;
  by?: string;
  forAddr?: string;
  date?: string;
}
function parseReceived(value: string): Hop {
  const semi = value.lastIndexOf(";");
  // Stop the host token at whitespace, ';', ',' or '(' and trim a trailing
  // FQDN root dot / stray punctuation so the displayed host is clean.
  const host = (re: RegExp): string | undefined => value.match(re)?.[1]?.replace(/[.,;]+$/, "");
  return {
    from: host(/\bfrom\s+([^\s;,()]+)/i),
    by: host(/\bby\s+([^\s;,()]+)/i),
    forAddr: value.match(/\bfor\s+<?([^\s;>]+@[^\s;>]+)>?/i)?.[1],
    date: semi >= 0 ? value.slice(semi + 1).trim() : undefined,
  };
}

function renderHeaders(subject: string): void {
  const sections: string[] = [];

  // The To: header is sender-supplied. Corroborate it against a delivery header
  // (Received … for / Delivered-To); flag it when nothing backs it up, or when
  // the delivery address disagrees — so To: is never read as proof of the real
  // recipient. (The true envelope recipient often isn't recorded in headers at
  // all, in which case only a server-side message trace can confirm it.)
  const toHeader = headerValue("To");
  const deliveredAddr = deliveredTo();
  let toNote: string | undefined;
  if (toHeader) {
    if (!deliveredAddr) {
      toNote =
        "To: is taken from the message header and is sender-supplied — no delivery header (Received … for / Delivered-To) corroborates it here, so it is not proof of the real recipient. Confirm the actual recipient with a server-side message trace.";
    } else if (!toHeader.toLowerCase().includes(deliveredAddr.toLowerCase())) {
      toNote = `Delivered to ${deliveredAddr} — this differs from the To: header, so the To: address is not where the message was actually delivered.`;
    }
  }
  sections.push(
    summaryCard(
      "Overview",
      [
        ["Subject", headerValue("Subject") ?? subject],
        ["Date", headerValue("Date")],
        ["From", headerValue("From")],
        ["Return-Path", headerValue("Return-Path")],
        ["Reply-To", headerValue("Reply-To")],
        ["To", toHeader],
        ["Delivered to", deliveredAddr],
        ["Message-ID", headerValue("Message-ID")],
      ],
      toNote,
    ),
  );

  const verdicts = parseAuthResults();
  if (verdicts.length) {
    const badges = verdicts
      .map((v) => `<span class="hdr-badge ${verdictClass(v.result)}">${esc(v.method)}: ${esc(v.result)}</span>`)
      .join("");
    const rawLines = headerValues("Authentication-Results")
      .map((v) => `<div class="hdr-rawline">${esc(v)}</div>`)
      .join("");
    sections.push(
      `<div class="hdr-card"><div class="hdr-card-title">Authentication</div>` +
        `<div class="hdr-badges">${badges}</div>` +
        rawLines +
        `</div>`,
    );
  }

  const spam = renderSpamCard();
  if (spam) sections.push(spam);

  const path = renderReceivedCard();
  if (path) sections.push(path);

  sections.push(renderRawCard());

  headersBody.innerHTML = sections.join("");
  applyHeaderFilter();
}

function summaryCard(title: string, rows: Array<[string, string | null]>, note?: string): string {
  const body = rows
    .filter(([, v]) => v && v.trim())
    .map(([k, v]) => `<div class="hdr-row"><span class="hdr-key">${esc(k)}</span><span class="hdr-val">${esc(v!)}</span></div>`)
    .join("");
  const footnote = note ? `<div class="hdr-note">&#9888; ${esc(note)}</div>` : "";
  return `<div class="hdr-card"><div class="hdr-card-title">${esc(title)}</div>${body}${footnote}</div>`;
}

function renderSpamCard(): string | null {
  const seg = new Map<string, string>();
  for (const raw of [headerValue("X-Forefront-Antispam-Report"), headerValue("X-Microsoft-Antispam")]) {
    if (raw) for (const [k, v] of parseSegments(raw)) seg.set(k, v);
  }
  const rows: Array<[string, string | null]> = [];
  const scl = seg.get("SCL");
  if (scl != null) rows.push(["Spam level (SCL)", `${scl}${sclMeaning(scl) ? ` — ${sclMeaning(scl)}` : ""}`]);
  const cat = seg.get("CAT");
  if (cat) rows.push(["Filter verdict (CAT)", `${cat}${CAT_MEANINGS[cat] ? ` — ${CAT_MEANINGS[cat]}` : ""}`]);
  const bcl = seg.get("BCL");
  if (bcl != null) rows.push(["Bulk level (BCL)", `${bcl}${bclMeaning(bcl) ? ` — ${bclMeaning(bcl)}` : ""}`]);
  const cip = seg.get("CIP");
  if (cip) rows.push(["Connecting IP (CIP)", cip]);
  const ctry = seg.get("CTRY");
  if (ctry) rows.push(["Origin country (CTRY)", ctry]);
  const helo = seg.get("H");
  if (helo) rows.push(["Sending host (HELO)", helo]);
  return rows.length ? summaryCard("Spam filtering (Microsoft)", rows) : null;
}

function renderReceivedCard(): string | null {
  const chain = headerValues("Received");
  if (chain.length === 0) return null;
  // Graph returns the newest hop (closest to the mailbox) first; reverse so the
  // trace reads origin → mailbox, top to bottom.
  const hops = chain.slice().reverse();
  const items = hops
    .map((value, i) => {
      const p = parseReceived(value);
      const label = i === 0 ? "Origin" : i === hops.length - 1 ? "Mailbox" : `Hop ${i}`;
      const parts: string[] = [];
      if (p.from) parts.push(`<span class="hdr-key">from</span> ${esc(p.from)}`);
      if (p.by) parts.push(`<span class="hdr-key">by</span> ${esc(p.by)}`);
      if (p.forAddr) parts.push(`<span class="hdr-key">for</span> ${esc(p.forAddr)}`);
      const detail = parts.length ? parts.join(" ") : esc(value);
      return `<div class="hdr-hop"><div class="hdr-hop-label">${label}${p.date ? ` · ${esc(p.date)}` : ""}</div><div class="hdr-hop-body">${detail}</div></div>`;
    })
    .join("");
  return `<div class="hdr-card"><div class="hdr-card-title">Delivery path (${hops.length} hop${hops.length === 1 ? "" : "s"})</div>${items}</div>`;
}

function renderRawCard(): string {
  const rows = currentHeaders
    .map((h) => {
      const important = KEY_HEADERS.has(h.name.toLowerCase());
      const key = esc(`${h.name} ${h.value}`.toLowerCase());
      return `<div class="hdr-raw${important ? " important" : ""}" data-h="${key}"><span class="hdr-raw-name">${esc(h.name)}:</span> <span class="hdr-raw-value">${esc(h.value)}</span></div>`;
    })
    .join("");
  return `<div class="hdr-card"><div class="hdr-card-title">All headers (${currentHeaders.length})</div><div class="hdr-raw-list">${rows}</div></div>`;
}

// Filter the raw list as the user types; the parsed summary cards stay put.
function applyHeaderFilter(): void {
  const q = headersFilter.value.trim().toLowerCase();
  headersBody.querySelectorAll<HTMLElement>(".hdr-raw").forEach((el) => {
    el.style.display = !q || (el.dataset.h ?? "").includes(q) ? "" : "none";
  });
}

async function copyHeaders(): Promise<void> {
  const text = currentHeaders.map((h) => `${h.name}: ${h.value}`).join("\n");
  const ok = await copyText(text);
  headersCopyBtn.textContent = ok ? "Copied" : "Copy failed";
  setTimeout(() => (headersCopyBtn.textContent = "Copy all"), 1500);
}

// Clipboard via the async API, falling back to a hidden textarea for webviews
// that don't grant clipboard-write.
async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    try {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand("copy");
      ta.remove();
      return ok;
    } catch {
      return false;
    }
  }
}

// ---- Resizable splitter ----
const LIST_W_KEY = "wattmail.listWidth";
const clampWidth = (w: number): number => Math.max(260, Math.min(640, w));
function applyListWidth(w: number): void {
  mainView.style.setProperty("--list-w", `${w}px`);
}
function loadListWidth(): number {
  const v = Number.parseInt(localStorage.getItem(LIST_W_KEY) ?? "", 10);
  return Number.isFinite(v) ? clampWidth(v) : 380;
}
splitter.addEventListener("pointerdown", (e) => {
  e.preventDefault();
  const startX = e.clientX;
  const startW = listEl.getBoundingClientRect().width;
  splitter.setPointerCapture(e.pointerId);
  const move = (ev: PointerEvent): void => applyListWidth(clampWidth(startW + (ev.clientX - startX)));
  const up = (ev: PointerEvent): void => {
    splitter.releasePointerCapture(ev.pointerId);
    splitter.removeEventListener("pointermove", move);
    splitter.removeEventListener("pointerup", up);
    localStorage.setItem(LIST_W_KEY, String(Math.round(listEl.getBoundingClientRect().width)));
  };
  splitter.addEventListener("pointermove", move);
  splitter.addEventListener("pointerup", up);
});

// ---- Resizable / maximizable compose panel ----
// The panel size persists across opens; Maximize is a separate sticky state that
// expands to fill the viewport and snaps back to the persisted size on Restore.
const COMPOSE_W_KEY = "wattmail.composeW";
const COMPOSE_H_KEY = "wattmail.composeH";
const COMPOSE_MAX_KEY = "wattmail.composeMax";
const COMPOSE_MIN_W = 420;
const COMPOSE_MIN_H = 360;
const COMPOSE_DEFAULT_W = 720;
const COMPOSE_DEFAULT_H = 560;
// Maximized footprint, and the ceiling a free resize is clamped to.
const composeMaxW = (): number => Math.round(window.innerWidth * 0.96);
const composeMaxH = (): number => Math.round(window.innerHeight * 0.92);
const clampComposeW = (w: number): number => Math.max(COMPOSE_MIN_W, Math.min(composeMaxW(), w));
const clampComposeH = (h: number): number => Math.max(COMPOSE_MIN_H, Math.min(composeMaxH(), h));

let composeMaximized = localStorage.getItem(COMPOSE_MAX_KEY) === "1";

function loadComposeSize(): { w: number; h: number } {
  const w = Number.parseInt(localStorage.getItem(COMPOSE_W_KEY) ?? "", 10);
  const h = Number.parseInt(localStorage.getItem(COMPOSE_H_KEY) ?? "", 10);
  return {
    w: clampComposeW(Number.isFinite(w) ? w : COMPOSE_DEFAULT_W),
    h: clampComposeH(Number.isFinite(h) ? h : COMPOSE_DEFAULT_H),
  };
}
// Apply either the maximized footprint or the persisted free size to the panel.
function applyComposeSize(): void {
  if (composeMaximized) {
    composePanel.style.width = `${composeMaxW()}px`;
    composePanel.style.height = `${composeMaxH()}px`;
  } else {
    const { w, h } = loadComposeSize();
    composePanel.style.width = `${w}px`;
    composePanel.style.height = `${h}px`;
  }
  composeMaximizeBtn.classList.toggle("is-max", composeMaximized);
  composeMaximizeBtn.title = composeMaximized ? "Restore" : "Maximize";
  composeMaximizeBtn.setAttribute("aria-label", composeMaximized ? "Restore" : "Maximize");
}
function toggleComposeMaximize(): void {
  composeMaximized = !composeMaximized;
  localStorage.setItem(COMPOSE_MAX_KEY, composeMaximized ? "1" : "0");
  applyComposeSize();
}
composeMaximizeBtn.addEventListener("click", toggleComposeMaximize);

// Bottom-right corner grip: same pointer-capture idiom as the list splitter.
// Dragging exits the maximized state so the new size is the one that persists.
composeResizeGrip.addEventListener("pointerdown", (e) => {
  e.preventDefault();
  if (composeMaximized) {
    composeMaximized = false;
    localStorage.setItem(COMPOSE_MAX_KEY, "0");
    composeMaximizeBtn.classList.remove("is-max");
    composeMaximizeBtn.title = "Maximize";
    composeMaximizeBtn.setAttribute("aria-label", "Maximize");
  }
  const startX = e.clientX;
  const startY = e.clientY;
  const rect = composePanel.getBoundingClientRect();
  const startW = rect.width;
  const startH = rect.height;
  composeResizeGrip.setPointerCapture(e.pointerId);
  const move = (ev: PointerEvent): void => {
    composePanel.style.width = `${clampComposeW(startW + (ev.clientX - startX))}px`;
    composePanel.style.height = `${clampComposeH(startH + (ev.clientY - startY))}px`;
  };
  const up = (ev: PointerEvent): void => {
    composeResizeGrip.releasePointerCapture(ev.pointerId);
    composeResizeGrip.removeEventListener("pointermove", move);
    composeResizeGrip.removeEventListener("pointerup", up);
    const final = composePanel.getBoundingClientRect();
    localStorage.setItem(COMPOSE_W_KEY, String(Math.round(final.width)));
    localStorage.setItem(COMPOSE_H_KEY, String(Math.round(final.height)));
  };
  composeResizeGrip.addEventListener("pointermove", move);
  composeResizeGrip.addEventListener("pointerup", up);
});

// ---- Folders ----
async function loadFolders(): Promise<void> {
  try {
    folders = await invoke<FolderInfo[]>("list_folders");
  } catch (e) {
    statusEl.textContent = `Could not list folders: ${e}`;
    return;
  }
  if (!currentFolderId) {
    const inbox = folders.find((f) => f.name.toLowerCase() === "inbox");
    currentFolderId = inbox?.id ?? folders[0]?.id ?? null;
  }
  renderFolders();
  // Reflect the inbox unread count in the system tray (icon + tooltip).
  const inboxFolder = folders.find((f) => f.name.toLowerCase() === "inbox");
  void invoke("set_unread", { count: inboxFolder?.unreadCount ?? 0 }).catch(() => {});
}

function renderFolders(): void {
  foldersEl.innerHTML = folders
    .map((f) => {
      const active = f.id === currentFolderId ? "active" : "";
      const badge = f.unreadCount > 0 ? `<span class="folder-badge">${f.unreadCount}</span>` : "";
      const pad = 10 + f.depth * 14;
      return `<button class="folder ${active}" data-fid="${esc(f.id)}" title="${esc(f.name)}" style="padding-left:${pad}px"><span class="folder-name">${esc(f.name)}</span>${badge}</button>`;
    })
    .join("");
}

async function selectFolder(id: string): Promise<void> {
  // Clicking a folder always leaves search mode, even the current folder.
  if (id === currentFolderId) {
    if (searchActive) exitSearch();
    return;
  }
  if (searchActive) {
    searchSeq++;
    searchActive = false;
    searchInput.value = "";
    setSearchClearVisible(false);
  }
  currentFolderId = id;
  loadedCount = PAGE_SIZE; // start each folder at the first page
  renderFolders();
  await refreshFromCache().catch(() => {});
  await syncFolder();
}

// ---- Actions ----
let syncing = false;

// Render the current folder from the local SQLite cache — instant, offline-capable.
// `preserveScroll` keeps the list position during a background refresh.
async function refreshFromCache(preserveScroll = false): Promise<void> {
  if (!currentFolderId) return;
  const scroll = listEl.scrollTop;
  const inbox = await invoke<Inbox>("folder_from_cache", {
    folderId: currentFolderId,
    top: loadedCount,
  });
  showSignedIn();
  renderInbox(inbox);
  if (preserveScroll) listEl.scrollTop = scroll;
}

// Reconcile the visible list after a row action (read/flag/delete/move). While a
// search is active the optimistic row update already mutated the displayed search
// results, so re-rendering the cached folder would clobber them; only the folder
// unread badges need refreshing. Outside search, re-read the folder from cache.
async function reconcileAfterAction(): Promise<void> {
  if (searchActive) {
    await loadFolders();
  } else {
    await refreshFromCache(true);
    await loadFolders();
  }
}

// Undo an optimistic row change after the server call failed. While searching,
// re-run the query so the list reflects true server state without clobbering it
// with the cached folder; otherwise restore the row from the cache.
async function revertOptimisticAction(): Promise<void> {
  if (searchActive) await runSearch(searchInput.value);
  else await refreshFromCache(true);
}

// Pull changes for the current folder into the cache, then re-render from it.
// `quiet` (used by the auto-sync timer) skips the status churn and keeps scroll.
async function syncFolder(quiet = false): Promise<void> {
  if (syncing || !currentFolderId) return;
  syncing = true;
  if (!quiet) {
    refreshBtn.disabled = true;
    statusEl.textContent = "Syncing…";
  }
  try {
    await invoke("sync_folder", { folderId: currentFolderId });
    await refreshFromCache(quiet);
    await loadFolders(); // refresh unread counts
    if (!quiet) {
      const shown =
        currentTotal > currentIds.size
          ? `${currentIds.size} of ${currentTotal} messages`
          : `${currentIds.size} message(s)`;
      statusEl.textContent = `${shown} · synced ${fmtDate(new Date().toISOString())}`;
    }
  } catch (e) {
    if (!quiet) statusEl.textContent = `Sync failed: ${e}`;
  } finally {
    syncing = false;
    if (!quiet) refreshBtn.disabled = false;
  }
}

async function signIn(): Promise<void> {
  signinBtn.disabled = true;
  signinMsg.textContent = "Opening your browser to sign in…";
  try {
    await invoke("sign_in");
    signinMsg.textContent = "";
    await loadFolders();
    await refreshFromCache().catch(() => {});
    await syncFolder();
  } catch (e) {
    signinMsg.textContent = `Sign-in failed: ${e}`;
  } finally {
    signinBtn.disabled = false;
  }
}

async function signOut(): Promise<void> {
  try {
    await invoke("sign_out");
  } catch (e) {
    settingsMsg.textContent = `Sign-out failed: ${e}`;
    return;
  }
  closeSettings();
  showSignedOut();
}

// ---- Settings ----
function openSettings(): void {
  setTheme.value = loadThemePref();
  settingsMsg.textContent = "";
  invoke<boolean>("get_close_to_tray")
    .then((v) => (setTray.checked = v))
    .catch(() => {});
  isAutostartEnabled()
    .then((v) => (setAutostart.checked = v))
    .catch(() => {});
  settingsOverlay.classList.remove("hidden");
}
function closeSettings(): void {
  settingsOverlay.classList.add("hidden");
}

// ---- Paste sanitizer (SECURITY-CRITICAL) ----
// Pasted/dropped content can originate from a hostile web page, so it is parsed
// into a detached document and rebuilt against an allow-list — never inserted
// raw. Trusted editor content (the quoted reply/forward inserted by openCompose)
// is NOT routed through here; only the clipboard/drag payload is.
const ALLOWED_TAGS = new Set([
  "p", "br", "div", "span", "b", "strong", "i", "em", "u", "s", "strike", "sub", "sup",
  "a", "ul", "ol", "li", "blockquote", "pre", "code", "h1", "h2", "h3", "h4", "h5", "h6",
  "table", "thead", "tbody", "tr", "td", "th", "hr", "img", "font",
]);
// Tags dropped wholesale — their subtree never reaches the editor.
const DROP_SUBTREE_TAGS = new Set([
  "script", "style", "iframe", "object", "embed", "link", "meta", "base", "form",
  "input", "button", "svg",
]);
const TABLE_ATTRS = new Set(["colspan", "rowspan", "align", "valign"]);
// The only data: img src shape accepted into the editor and converted to a cid:
// inline attachment on send: base64-encoded raster image data. svg+xml and
// non-base64 (e.g. URL-encoded) forms are excluded so the sanitizer, the
// byte-counter, and extractInlineImages all agree on exactly what is "inline".
const INLINE_IMAGE_SRC = /^data:image\/(png|jpeg|jpg|gif|webp|bmp);base64,/i;
// CSS properties permitted inside a sanitized `style` attribute.
const ALLOWED_STYLE_PROPS = new Set([
  "color", "background-color", "font-weight", "font-style", "font-size", "font-family",
  "text-align", "text-decoration", "margin", "margin-top", "margin-right",
  "margin-bottom", "margin-left", "padding", "padding-top", "padding-right",
  "padding-bottom", "padding-left", "border", "width", "height",
]);

// Reject any style value carrying an active/escape vector (url(), expression(),
// @import, javascript:, position:fixed, behavio[u]r). Case-insensitive.
function styleValueIsSafe(value: string): boolean {
  const v = value.toLowerCase();
  return !(
    v.includes("url(") ||
    v.includes("expression(") ||
    v.includes("@import") ||
    v.includes("javascript:") ||
    v.includes("position:fixed") ||
    v.includes("position: fixed") ||
    v.includes("behavior") ||
    v.includes("behaviour")
  );
}

// Rebuild a `style` attribute keeping only allow-listed, vector-free properties.
function sanitizeStyle(style: string): string {
  return style
    .split(";")
    .map((decl) => decl.trim())
    .filter(Boolean)
    .map((decl) => {
      const idx = decl.indexOf(":");
      if (idx < 0) return "";
      const prop = decl.slice(0, idx).trim().toLowerCase();
      const value = decl.slice(idx + 1).trim();
      if (!ALLOWED_STYLE_PROPS.has(prop) || !styleValueIsSafe(value)) return "";
      return `${prop}: ${value}`;
    })
    .filter(Boolean)
    .join("; ");
}

const URL_PROTOCOL = /^([a-z][a-z0-9+.-]*):/i;
function protocolOf(url: string): string | null {
  const m = URL_PROTOCOL.exec(url.trim());
  return m ? m[1].toLowerCase() : null;
}

// Copy only the allow-listed attributes for `tag` from `src` onto `dst`,
// applying per-attribute value rules. Anything not explicitly allowed is dropped.
function copyAllowedAttributes(tag: string, src: Element, dst: Element): void {
  for (const attr of Array.from(src.attributes)) {
    const name = attr.name.toLowerCase();
    const value = attr.value;
    // Defence in depth: never carry event handlers or javascript:/vbscript: values.
    if (name.startsWith("on")) continue;
    const lowered = value.toLowerCase();
    if (lowered.includes("javascript:") || lowered.includes("vbscript:")) continue;

    if (name === "style") {
      const safe = sanitizeStyle(value);
      if (safe) dst.setAttribute("style", safe);
      continue;
    }
    if (tag === "a" && name === "href") {
      const proto = protocolOf(value);
      if (proto === "http" || proto === "https" || proto === "mailto") dst.setAttribute("href", value);
      continue;
    }
    if (tag === "img" && name === "src") {
      const proto = protocolOf(value);
      // Inline images must be base64 raster data: URLs. We exclude svg+xml (which
      // mail clients render inconsistently and enlarges the recipient surface) and
      // non-base64 forms (e.g. URL-encoded SVG) — the latter would survive here but
      // not be picked up by extractInlineImages, shipping an unrenderable data: URI.
      // Keeping this in lockstep with extractInlineImages avoids that divergence.
      const isInlineImage = proto === "data" && INLINE_IMAGE_SRC.test(value.trim());
      if (proto === "https" || isInlineImage) dst.setAttribute("src", value);
      continue;
    }
    if (tag === "img" && (name === "alt" || name === "width" || name === "height")) {
      dst.setAttribute(name, value);
      continue;
    }
    if ((tag === "td" || tag === "th") && TABLE_ATTRS.has(name)) {
      dst.setAttribute(name, value);
      continue;
    }
    if (tag === "font" && name === "color") {
      dst.setAttribute("color", value);
      continue;
    }
  }
}

// Recursively rebuild `node`'s children into `out` (a node in the live document),
// keeping only allow-listed elements and text. Disallowed elements are unwrapped
// (children kept) unless they are in DROP_SUBTREE_TAGS, where the subtree is cut.
function rebuildInto(node: Node, out: Node, doc: Document): void {
  for (const child of Array.from(node.childNodes)) {
    if (child.nodeType === Node.TEXT_NODE) {
      out.appendChild(doc.createTextNode(child.textContent ?? ""));
      continue;
    }
    if (child.nodeType !== Node.ELEMENT_NODE) continue; // drop comments, etc.
    const el = child as Element;
    const tag = el.tagName.toLowerCase();
    if (DROP_SUBTREE_TAGS.has(tag)) continue;
    if (!ALLOWED_TAGS.has(tag)) {
      // Unknown but otherwise-harmless wrapper: keep its contents, drop the tag.
      rebuildInto(el, out, doc);
      continue;
    }
    const clean = doc.createElement(tag);
    copyAllowedAttributes(tag, el, clean);
    rebuildInto(el, clean, doc);
    out.appendChild(clean);
  }
}

// Sanitize an untrusted HTML string into an allow-listed fragment string safe to
// hand to execCommand("insertHTML").
function sanitizeHtml(dirty: string): string {
  const parsed = new DOMParser().parseFromString(dirty, "text/html");
  const container = document.createElement("div");
  rebuildInto(parsed.body, container, document);
  return container.innerHTML;
}

// ---- Inline images ----
// Single-image and total-payload caps. Graph's sendMail inline limit is ~3 MB
// per image; we cap raw bytes so a base64-inflated body still fits, and warn
// once the cumulative inline payload approaches the same ceiling.
const INLINE_IMAGE_MAX_BYTES = 3 * 1024 * 1024;
const INLINE_TOTAL_WARN_BYTES = 3 * 1024 * 1024;
const oneMb = (bytes: number): string => (bytes / (1024 * 1024)).toFixed(1);
// data: image URLs are base64; raw bytes ≈ payload length * 3 / 4.
function dataUrlByteLength(dataUrl: string): number {
  const comma = dataUrl.indexOf(",");
  if (comma < 0) return 0;
  const b64 = dataUrl.slice(comma + 1);
  return Math.floor((b64.length * 3) / 4);
}
// Sum of raw bytes across every data:image/ embedded in the editor right now.
function inlineImagesTotalBytes(): number {
  let total = 0;
  for (const img of Array.from(cBodyInput.querySelectorAll("img"))) {
    const src = img.getAttribute("src") ?? "";
    if (/^data:image\//i.test(src)) total += dataUrlByteLength(src);
  }
  return total;
}
function readFileAsDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result));
    reader.onerror = () => reject(reader.error ?? new Error("read failed"));
    reader.readAsDataURL(file);
  });
}
// Insert one image file at the caret as a data: URL, enforcing the per-image cap
// and surfacing a running total. Returns false (with a message) when skipped.
async function insertImageFile(file: File): Promise<boolean> {
  if (file.size > INLINE_IMAGE_MAX_BYTES) {
    composeMsg.textContent = `Image "${file.name || "pasted image"}" skipped — ${oneMb(
      file.size,
    )} MB exceeds the ${Math.round(
      INLINE_IMAGE_MAX_BYTES / (1024 * 1024),
    )} MB per-image inline limit. Attach it as a file instead.`;
    return false;
  }
  const dataUrl = await readFileAsDataUrl(file);
  // Embed only the supported base64-raster types (matches INLINE_IMAGE_SRC and the
  // send-time extractor); reject anything else with guidance instead of inserting
  // an image that would be silently dropped on send.
  if (!INLINE_IMAGE_SRC.test(dataUrl)) {
    composeMsg.textContent = `Image "${file.name || "pasted image"}" skipped — only PNG, JPEG, GIF, WebP and BMP can be embedded. Attach it as a file instead.`;
    return false;
  }
  document.execCommand("insertImage", false, dataUrl);
  const total = inlineImagesTotalBytes();
  composeMsg.textContent =
    total > INLINE_TOTAL_WARN_BYTES
      ? `Inline images now total ${oneMb(total)} MB — large messages may be rejected; consider attaching files instead.`
      : "";
  return true;
}
// Extract image files from a clipboard/drag payload (handles both files and the
// synthetic image item the OS produces for a copied screenshot).
function imageFilesFrom(data: DataTransfer | null): File[] {
  if (!data) return [];
  return Array.from(data.files).filter((f) => f.type.startsWith("image/"));
}

// The inline-image payload the backend expects (camelCase keys; serde renames to
// snake_case). Body must reference each image as src="cid:<contentId>".
interface InlineImage {
  contentId: string;
  contentType: string;
  dataBase64: string;
}

// On SEND only: rewrite every embedded data:image/ <img> in the body to a
// cid:-referenced inline attachment. Returns the rewritten HTML plus the list of
// inline images to send alongside. Pure aside from generating ids — never mutates
// the live editor (it operates on a detached parse of the body).
function extractInlineImages(bodyHtml: string): { html: string; images: InlineImage[] } {
  const doc = new DOMParser().parseFromString(bodyHtml, "text/html");
  const images: InlineImage[] = [];
  let counter = 0;
  for (const img of Array.from(doc.querySelectorAll("img"))) {
    const src = img.getAttribute("src") ?? "";
    // Defence in depth: only ever build an inline attachment from the exact
    // base64-raster shape the sanitizer permits, regardless of upstream paths.
    if (!INLINE_IMAGE_SRC.test(src.trim())) continue;
    const m = /^data:([^;,]+);base64,(.*)$/is.exec(src.trim());
    if (!m) continue;
    const contentType = m[1].trim();
    const dataBase64 = m[2].trim();
    const contentId = `img${++counter}-${crypto.randomUUID()}`;
    img.setAttribute("src", `cid:${contentId}`);
    images.push({ contentId, contentType, dataBase64 });
  }
  return { html: doc.body.innerHTML, images };
}

// Authoritative send-time cap on inline images. The per-image insert path warns
// early, but data:image/ images also enter the body via HTML paste/drop and via
// resumed drafts, which never hit insertImageFile — so this is the guard that
// actually enforces the cap before we hand the payload to Graph. Returns a
// user-facing message when over the limit, or null when the payload is fine.
function inlineImagesSizeProblem(images: InlineImage[]): string | null {
  let total = 0;
  for (const img of images) {
    const bytes = Math.floor((img.dataBase64.length * 3) / 4);
    if (bytes > INLINE_IMAGE_MAX_BYTES) {
      return `An inline image is ${oneMb(bytes)} MB, over the ${Math.round(
        INLINE_IMAGE_MAX_BYTES / (1024 * 1024),
      )} MB per-image limit. Remove it or attach it as a file instead.`;
    }
    total += bytes;
  }
  if (total > INLINE_IMAGE_MAX_BYTES) {
    return `Inline images total ${oneMb(total)} MB, over the ${Math.round(
      INLINE_IMAGE_MAX_BYTES / (1024 * 1024),
    )} MB limit. Remove some or attach them as files instead.`;
  }
  return null;
}

// ---- Compose ----
let composeAttachPaths: string[] = [];
// The draft currently open in the compose modal, if any. Set when resuming a
// draft or after the first "Save draft"; null for a fresh compose/reply/forward.
let currentDraftId: string | null = null;

function renderComposeAttachments(): void {
  cAttachments.innerHTML = composeAttachPaths
    .map((p, i) => {
      const name = p.split(/[\\/]/).pop() ?? p;
      return `<span class="attach-chip-c">${esc(name)} <button class="attach-remove" data-i="${i}" type="button" title="Remove">&times;</button></span>`;
    })
    .join("");
  cAttachments.querySelectorAll<HTMLButtonElement>(".attach-remove").forEach((b) => {
    b.addEventListener("click", () => {
      composeAttachPaths.splice(Number(b.dataset.i), 1);
      renderComposeAttachments();
    });
  });
}

async function pickAttachments(): Promise<void> {
  const result = (await open({ multiple: true, title: "Attach files" })) as string[] | string | null;
  if (!result) return;
  composeAttachPaths.push(...(Array.isArray(result) ? result : [result]));
  renderComposeAttachments();
}

function openCompose(opts: {
  title: string;
  to: string[];
  cc: string[];
  subject: string;
  // The quoted original (reply/forward) — prefixed with a blank line in the editor.
  quotedHtml?: string;
  // A resumed draft's raw body — placed in the editor verbatim.
  bodyHtml?: string;
  // The draft being edited, if any; null/omitted for a fresh compose.
  draftId?: string | null;
}): void {
  currentDraftId = opts.draftId ?? null;
  composeTitle.textContent = opts.title;
  cToInput.value = opts.to.join(", ");
  cCcInput.value = opts.cc.join(", ");
  cSubjectInput.value = opts.subject;
  // A resumed draft's body is restored verbatim; a quoted reply/forward goes in
  // below a blank line so it's visible and editable. Never repopulate the editor
  // from display-sanitized HTML — drafts carry their own raw body.
  if (opts.bodyHtml !== undefined) {
    cBodyInput.innerHTML = opts.bodyHtml;
  } else {
    cBodyInput.innerHTML = opts.quotedHtml ? `<p><br></p>${opts.quotedHtml}` : "";
  }
  composeAttachPaths = [];
  renderComposeAttachments();
  composeMsg.textContent = "";
  applyComposeSize();
  composeOverlay.classList.remove("hidden");
  focusComposeBody();
}

// Focus the editor with the caret at the very start (above any quoted reply).
function focusComposeBody(): void {
  cBodyInput.focus();
  const range = document.createRange();
  range.setStart(cBodyInput, 0);
  range.collapse(true);
  const sel = window.getSelection();
  sel?.removeAllRanges();
  sel?.addRange(range);
}
function closeCompose(): void {
  composeOverlay.classList.add("hidden");
}

function composeNew(): void {
  openCompose({ title: "New message", to: [], cc: [], subject: "", quotedHtml: "" });
}

async function replyTo(replyAll: boolean, id?: string): Promise<void> {
  const targetId = id ?? lastMessage?.id;
  if (!targetId) return;
  try {
    const p = await invoke<ComposeData>("prepare_reply", {
      id: targetId,
      replyAll,
      selfEmail: accountEmail,
    });
    openCompose({
      title: replyAll ? "Reply all" : "Reply",
      to: p.to,
      cc: p.cc,
      subject: p.subject,
      quotedHtml: p.quotedHtml,
    });
  } catch (e) {
    statusEl.textContent = `Could not prepare reply: ${e}`;
  }
}

async function forwardMsg(id?: string): Promise<void> {
  const targetId = id ?? lastMessage?.id;
  if (!targetId) return;
  try {
    const p = await invoke<ComposeData>("prepare_forward", { id: targetId });
    openCompose({
      title: "Forward",
      to: p.to,
      cc: p.cc,
      subject: p.subject,
      quotedHtml: p.quotedHtml,
    });
  } catch (e) {
    statusEl.textContent = `Could not prepare forward: ${e}`;
  }
}

// Resume editing a saved draft: load its RAW body (never the display-sanitized
// reader HTML) and open compose in edit mode, tracking the draft id.
async function resumeDraft(id: string): Promise<void> {
  try {
    const d = await invoke<DraftPrefill>("load_draft", { id });
    openCompose({
      title: "Edit draft",
      to: d.to,
      cc: d.cc,
      subject: d.subject,
      bodyHtml: d.bodyHtml,
      draftId: id,
    });
  } catch (e) {
    statusEl.textContent = `Could not open draft: ${e}`;
  }
}

function parseAddresses(value: string): string[] {
  return value
    .split(/[,;]/)
    .map((a) => a.trim())
    .filter(Boolean);
}

// Pull the Drafts folder's cache up to date so the list reflects a just
// saved/sent draft. No-op if Drafts isn't loaded; never throws.
async function syncDraftsFolder(): Promise<void> {
  const drafts = folders.find((f) => isDraftsFolder(f.name));
  if (!drafts) return;
  try {
    await invoke("sync_folder", { folderId: drafts.id });
  } catch {
    /* best-effort — the next auto-sync will reconcile */
  }
  // If Drafts is the folder on screen, re-render it; always refresh badges.
  if (currentFolderId === drafts.id && !searchActive) await refreshFromCache(true).catch(() => {});
  await loadFolders();
}

// Save the compose modal as a draft: create on first save, update thereafter.
// Tracks the new draft id so subsequent saves (and a later Send) reuse it.
async function saveDraft(): Promise<void> {
  composeSaveDraftBtn.disabled = true;
  composeMsg.textContent = "Saving draft…";
  try {
    const id = await invoke<string>("save_draft", {
      id: currentDraftId,
      to: parseAddresses(cToInput.value),
      cc: parseAddresses(cCcInput.value),
      subject: cSubjectInput.value,
      bodyHtml: cBodyInput.innerHTML,
    });
    currentDraftId = id;
    composeMsg.textContent = "Draft saved.";
    await syncDraftsFolder();
  } catch (e) {
    composeMsg.textContent = `Could not save draft: ${e}`;
  } finally {
    composeSaveDraftBtn.disabled = false;
  }
}

async function sendCompose(): Promise<void> {
  const to = parseAddresses(cToInput.value);
  if (to.length === 0) {
    composeMsg.textContent = "Add at least one recipient.";
    return;
  }
  composeSendBtn.disabled = true;
  composeMsg.textContent = "Sending…";
  const cc = parseAddresses(cCcInput.value);
  const subject = cSubjectInput.value;
  const bodyHtml = cBodyInput.innerHTML;
  const draftId = currentDraftId;
  // Drafts are sent via the Graph /send path, which carries only the saved draft
  // body — attachments added in the compose modal aren't persisted to the draft,
  // so warn rather than silently drop them.
  if (draftId && composeAttachPaths.length > 0) {
    composeMsg.textContent =
      "Attachments aren't supported on drafts yet — remove them or start a fresh message to attach files.";
    composeSendBtn.disabled = false;
    return;
  }
  // The draft /send path carries the saved body verbatim (draft_body_json omits
  // attachments), so inline data:image/ images aren't converted to cid: and most
  // mail clients drop data: URIs — the recipient sees broken images. Warn rather
  // than silently ship them, mirroring the file-attachment deferral above.
  if (draftId && cBodyInput.querySelector('img[src^="data:image/"]')) {
    composeMsg.textContent =
      "Inline images aren't supported on drafts yet — remove them or start a fresh message to embed images.";
    composeSendBtn.disabled = false;
    return;
  }
  try {
    if (draftId) {
      // Resuming a draft: flush edits, then send via /send so the draft is
      // consumed (moved to Sent) rather than duplicated by a separate sendMail.
      await invoke("save_draft", { id: draftId, to, cc, subject, bodyHtml });
      await invoke("send_draft", { id: draftId });
      currentDraftId = null;
    } else {
      // Embedded data:image/ images become cid:-referenced inline attachments at
      // send time; file attachments continue via attachmentPaths.
      const { html, images } = extractInlineImages(bodyHtml);
      // Hard cap regardless of how the image entered the body (insert, HTML
      // paste/drop, or a resumed draft) — Graph rejects oversized sends, so
      // catch it here with actionable guidance instead of a raw API error.
      const sizeProblem = inlineImagesSizeProblem(images);
      if (sizeProblem) {
        composeMsg.textContent = sizeProblem;
        composeSendBtn.disabled = false;
        return;
      }
      await invoke("send_message", {
        to,
        cc,
        subject,
        bodyHtml: html,
        attachmentPaths: composeAttachPaths,
        inlineImages: images,
      });
    }
    closeCompose();
    statusEl.textContent = "Message sent.";
    if (draftId) await syncDraftsFolder(); // the sent draft has left Drafts
  } catch (e) {
    composeMsg.textContent = `Send failed: ${e}`;
  } finally {
    composeSendBtn.disabled = false;
  }
}

// ---- Wiring ----
foldersEl.addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest<HTMLElement>(".folder");
  if (btn?.dataset.fid) void selectFolder(btn.dataset.fid);
});
listEl.addEventListener("click", (e) => {
  const target = e.target as HTMLElement;
  if (target.closest(".load-more")) {
    loadedCount += PAGE_SIZE; // grow the window, then re-read from cache
    void refreshFromCache(true);
    return;
  }
  const row = target.closest<HTMLElement>(".msg");
  if (!row?.dataset.id) return;
  cursorId = row.dataset.id; // keep the keyboard cursor on the clicked row
  highlightCursor();
  // In Drafts, a click resumes editing in compose rather than opening the reader.
  // (Search results are never drafts here — they render with the sender form.)
  if (!searchActive && currentFolderIsDrafts()) void resumeDraft(row.dataset.id);
  else void openMessage(row.dataset.id);
});

// ---- Email right-click context menu ----
const ctxMenu = document.createElement("div");
ctxMenu.className = "ctx-menu hidden";
ctxMenu.setAttribute("role", "menu");
document.body.appendChild(ctxMenu);
let ctxTargetId: string | null = null;

function hideCtxMenu(): void {
  if (ctxMenu.classList.contains("hidden")) return;
  ctxMenu.classList.add("hidden");
  if (ctxTargetId) rowFor(ctxTargetId)?.classList.remove("ctx-target");
  ctxTargetId = null;
}

let ctxX = 0;
let ctxY = 0;

// Position the menu at the opening point, clamped inside the viewport. Re-run
// after any content swap (e.g. drilling into the folder list) so it still fits.
function placeMenu(): void {
  ctxMenu.style.left = "0";
  ctxMenu.style.top = "0";
  const left = Math.max(4, Math.min(ctxX, window.innerWidth - ctxMenu.offsetWidth - 4));
  const top = Math.max(4, Math.min(ctxY, window.innerHeight - ctxMenu.offsetHeight - 4));
  ctxMenu.style.left = `${left}px`;
  ctxMenu.style.top = `${top}px`;
}

function renderMainMenu(unread: boolean, flagged: boolean): void {
  const items: Array<{ act: string; label: string; danger?: boolean } | "sep"> = [
    { act: "open", label: "Open" },
    { act: "reply", label: "Reply" },
    { act: "replyAll", label: "Reply all" },
    { act: "forward", label: "Forward" },
    "sep",
    { act: "toggleRead", label: unread ? "Mark as read" : "Mark as unread" },
    { act: "toggleFlag", label: flagged ? "Clear flag" : "Flag" },
    { act: "moveMenu", label: "Move to folder…" },
    { act: "delete", label: "Delete", danger: true },
  ];
  ctxMenu.innerHTML = items
    .map((it) =>
      it === "sep"
        ? `<div class="ctx-sep"></div>`
        : `<button class="ctx-item${it.danger ? " ctx-danger" : ""}" data-act="${it.act}">${it.label}</button>`,
    )
    .join("");
}

// Second "page" of the menu: pick a destination folder (current folder excluded).
function renderFolderMenu(): void {
  const others = folders.filter((f) => f.id !== currentFolderId);
  const list = others.length
    ? others
        .map(
          (f) =>
            `<button class="ctx-item" data-fid="${esc(f.id)}" title="${esc(f.name)}" style="padding-left:${10 + f.depth * 12}px">${esc(f.name)}</button>`,
        )
        .join("")
    : `<div class="ctx-empty">No other folders</div>`;
  ctxMenu.innerHTML =
    `<button class="ctx-item ctx-back" data-act="back">← Back</button>` +
    `<div class="ctx-sep"></div>` +
    `<div class="ctx-folders">${list}</div>`;
}

function showCtxMenu(x: number, y: number, id: string, unread: boolean, flagged: boolean): void {
  ctxTargetId = id;
  ctxX = x;
  ctxY = y;
  renderMainMenu(unread, flagged);
  ctxMenu.classList.remove("hidden");
  placeMenu();
  rowFor(id)?.classList.add("ctx-target");
}

listEl.addEventListener("contextmenu", (e) => {
  const row = (e.target as HTMLElement).closest<HTMLElement>(".msg");
  if (!row?.dataset.id) return; // off a row: leave the default menu
  e.preventDefault();
  showCtxMenu(
    e.clientX,
    e.clientY,
    row.dataset.id,
    row.classList.contains("unread"),
    row.classList.contains("flagged"),
  );
});

ctxMenu.addEventListener("click", (e) => {
  // The menu handles its own clicks; don't let them reach the outside-click
  // dismiss handler (the clicked node may be detached by a content swap).
  e.stopPropagation();
  const item = (e.target as HTMLElement).closest<HTMLElement>(".ctx-item");
  const id = ctxTargetId;
  if (!item || !id) return;

  // Drill in / out without closing the menu.
  if (item.dataset.act === "moveMenu") {
    renderFolderMenu();
    placeMenu();
    return;
  }
  if (item.dataset.act === "back") {
    renderMainMenu(
      rowFor(id)?.classList.contains("unread") ?? false,
      rowFor(id)?.classList.contains("flagged") ?? false,
    );
    placeMenu();
    return;
  }

  const unread = rowFor(id)?.classList.contains("unread") ?? false;
  const flagged = rowFor(id)?.classList.contains("flagged") ?? false;
  const destFolderId = item.dataset.fid;
  hideCtxMenu();
  if (destFolderId) {
    void moveMessage(id, destFolderId);
    return;
  }
  switch (item.dataset.act) {
    case "open":
      if (!searchActive && currentFolderIsDrafts()) void resumeDraft(id);
      else void openMessage(id);
      break;
    case "reply":
      void replyTo(false, id);
      break;
    case "replyAll":
      void replyTo(true, id);
      break;
    case "forward":
      void forwardMsg(id);
      break;
    case "toggleRead":
      void setRead(id, unread); // unread → mark read; read → mark unread
      break;
    case "toggleFlag":
      void setFlag(id, !flagged); // flagged → clear; unflagged → flag
      break;
    case "delete":
      void deleteMessage(id);
      break;
  }
});

document.addEventListener("click", (e) => {
  if (!ctxMenu.contains(e.target as Node)) hideCtxMenu();
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") hideCtxMenu();
});
window.addEventListener("blur", hideCtxMenu);
listEl.addEventListener("scroll", hideCtxMenu);
refreshBtn.addEventListener("click", () => {
  if (searchActive) exitSearch(); // Refresh leaves search and reloads the folder
  void syncFolder();
});
sortSelect.addEventListener("change", () => {
  sortMode = sortSelect.value as SortMode;
  localStorage.setItem(SORT_KEY, sortMode);
  // The sort applies to whatever is shown: search results or the cached folder.
  if (searchActive) void runSearch(searchInput.value);
  else void refreshFromCache(true);
});

// Search: debounce typing, submit immediately on Enter, clear on Escape.
searchInput.addEventListener("input", () => {
  const value = searchInput.value;
  setSearchClearVisible(value.trim().length > 0);
  if (searchTimer) clearTimeout(searchTimer);
  if (!value.trim()) {
    exitSearch();
    return;
  }
  searchTimer = setTimeout(() => void runSearch(value), SEARCH_DEBOUNCE_MS);
});
searchInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    if (searchTimer) clearTimeout(searchTimer);
    void runSearch(searchInput.value);
  } else if (e.key === "Escape" && searchInput.value) {
    e.preventDefault();
    e.stopPropagation();
    exitSearch();
  }
});
searchClearBtn.addEventListener("click", () => {
  exitSearch();
  searchInput.focus();
});
gear.addEventListener("click", openSettings);
signinBtn.addEventListener("click", () => void signIn());
settingsClose.addEventListener("click", closeSettings);
signoutBtn.addEventListener("click", () => void signOut());
composeBtn.addEventListener("click", composeNew);
composeCancel.addEventListener("click", closeCompose);
composeSaveDraftBtn.addEventListener("click", () => void saveDraft());
composeSendBtn.addEventListener("click", () => void sendCompose());
cAttachBtn.addEventListener("click", () => void pickAttachments());
// Rich-text editing via execCommand (deprecated but universally supported in the
// webview). mousedown + preventDefault keeps the editor's selection.
composeToolbar.addEventListener("mousedown", (e) => {
  const btn = (e.target as HTMLElement).closest<HTMLElement>("[data-cmd]");
  if (!btn) return;
  e.preventDefault();
  const cmd = btn.dataset.cmd ?? "";
  if (cmd === "createLink") {
    const url = window.prompt("Link URL:");
    if (url) document.execCommand("createLink", false, url);
  } else if (cmd) {
    document.execCommand(cmd, false);
  }
});
// Paste: prefer pasted image(s); else sanitized rich HTML; else plain text.
// All HTML is allow-list sanitized before insertion (the clipboard can carry
// hostile markup from any web page).
cBodyInput.addEventListener("paste", (e) => {
  const data = e.clipboardData;
  const images = imageFilesFrom(data);
  if (images.length > 0) {
    e.preventDefault();
    void (async () => {
      for (const file of images) await insertImageFile(file);
    })();
    return;
  }
  const html = data?.getData("text/html") ?? "";
  if (html) {
    e.preventDefault();
    document.execCommand("insertHTML", false, sanitizeHtml(html));
    return;
  }
  e.preventDefault();
  const text = data?.getData("text/plain") ?? "";
  document.execCommand("insertText", false, text);
});
// Make the editor a live, deterministic drop target: the HTML5 DnD contract
// requires preventDefault on dragenter/dragover, and without it the native
// contenteditable drop (which inserts UNSANITIZED markup) would run instead.
// Requires app.windows[].dragDropEnabled=false in tauri.conf.json so the
// webview delivers DOM drag/drop rather than swallowing it as a Tauri event.
const allowDrop = (e: DragEvent): void => {
  e.preventDefault();
  if (e.dataTransfer) e.dataTransfer.dropEffect = "copy";
};
cBodyInput.addEventListener("dragenter", allowDrop);
cBodyInput.addEventListener("dragover", allowDrop);
// Drop: mirror the paste handler exactly — always preventDefault (so the native
// unsanitized contenteditable drop never fires), then prefer dropped image
// file(s); else sanitized rich HTML; else plain text.
cBodyInput.addEventListener("drop", (e) => {
  e.preventDefault();
  const data = e.dataTransfer;
  const images = imageFilesFrom(data);
  if (images.length > 0) {
    void (async () => {
      for (const file of images) await insertImageFile(file);
    })();
    return;
  }
  const html = data?.getData("text/html") ?? "";
  if (html) {
    document.execCommand("insertHTML", false, sanitizeHtml(html));
    return;
  }
  document.execCommand("insertText", false, data?.getData("text/plain") ?? "");
});
composeOverlay.addEventListener("click", (e) => {
  if (e.target === composeOverlay) closeCompose();
});
headersCloseBtn.addEventListener("click", closeHeaders);
headersFilter.addEventListener("input", applyHeaderFilter);
headersCopyBtn.addEventListener("click", () => void copyHeaders());
headersOverlay.addEventListener("click", (e) => {
  if (e.target === headersOverlay) closeHeaders();
});
setTheme.addEventListener("change", () => setThemePref(setTheme.value as ThemePref));
setTray.addEventListener("change", () => {
  invoke("set_close_to_tray", { value: setTray.checked }).catch(
    (e) => (settingsMsg.textContent = `Could not save: ${e}`),
  );
});
setAutostart.addEventListener("change", () => {
  const want = setAutostart.checked;
  const action = want ? enableAutostart() : disableAutostart();
  action.catch((e) => {
    settingsMsg.textContent = `Could not change autostart: ${e}`;
    setAutostart.checked = !want; // revert the toggle to the real state
  });
});
settingsOverlay.addEventListener("click", (e) => {
  if (e.target === settingsOverlay) closeSettings();
});
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  if (!headersOverlay.classList.contains("hidden")) closeHeaders();
  else if (!settingsOverlay.classList.contains("hidden")) closeSettings();
  else if (!composeOverlay.classList.contains("hidden")) closeCompose();
});

// ---- Global keyboard shortcuts (message-list navigation + actions) ----
// A single keydown layer over the message list. It MUST stay inert while the
// user is typing or a modal is open — otherwise plain letters (r, f, c, …) would
// fire actions mid-compose or mid-search. Escape is intentionally left to the
// existing handlers (search input, context menu, modal stack) so this layer
// never competes with them.

// True when focus is in a text-entry surface (search box, compose fields/body,
// any input/textarea/select or contenteditable), so letter keys must pass
// through as text rather than trigger shortcuts.
function isTypingTarget(): boolean {
  const el = document.activeElement as HTMLElement | null;
  if (!el) return false;
  if (el.isContentEditable) return true;
  const tag = el.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT";
}
// Any modal open means shortcuts are suspended (the modal owns the keyboard).
function aModalIsOpen(): boolean {
  return (
    !composeOverlay.classList.contains("hidden") ||
    !settingsOverlay.classList.contains("hidden") ||
    !headersOverlay.classList.contains("hidden")
  );
}

function cursorRowState(): { unread: boolean; flagged: boolean } | null {
  if (!cursorId) return null;
  const row = rowFor(cursorId);
  if (!row) return null;
  return { unread: row.classList.contains("unread"), flagged: row.classList.contains("flagged") };
}

document.addEventListener("keydown", (e) => {
  // Never intercept while signed out, typing, holding a modifier, or in a modal.
  if (mainView.classList.contains("hidden")) return;
  if (e.ctrlKey || e.metaKey || e.altKey) return;
  if (isTypingTarget() || aModalIsOpen()) return;
  // The context menu owns the keyboard while open (its own Escape closes it);
  // don't fire list actions behind it.
  if (!ctxMenu.classList.contains("hidden")) return;

  // "/" focuses search regardless of the cursor; it's the one binding that
  // deliberately moves focus into a text field.
  if (e.key === "/") {
    e.preventDefault();
    searchInput.focus();
    searchInput.select();
    return;
  }

  switch (e.key) {
    case "j":
    case "ArrowDown":
      e.preventDefault();
      moveCursor(1);
      return;
    case "k":
    case "ArrowUp":
      e.preventDefault();
      moveCursor(-1);
      return;
    case "Enter":
      e.preventDefault();
      activateCursor();
      return;
    case "c":
      e.preventDefault();
      composeNew();
      return;
  }

  // The remaining bindings all act on the row under the cursor.
  if (!cursorId) return;
  const id = cursorId;
  const state = cursorRowState();
  switch (e.key) {
    case "r":
      e.preventDefault();
      void replyTo(false, id);
      break;
    case "a":
      e.preventDefault();
      void replyTo(true, id);
      break;
    case "f":
      e.preventDefault();
      void forwardMsg(id);
      break;
    case "u":
      e.preventDefault();
      if (state) void setRead(id, state.unread); // unread → read; read → unread
      break;
    case "g":
      e.preventDefault();
      if (state) void setFlag(id, !state.flagged); // toggle follow-up flag
      break;
    case "#":
      e.preventDefault();
      void deleteMessage(id);
      break;
  }
});

updateInstall.addEventListener("click", () => void installUpdate());
updateLater.addEventListener("click", () => updateBanner.classList.add("hidden"));

// Tray "Settings…" menu item asks the frontend to open the modal.
void listen("open-settings", () => openSettings());

// ---- Updates ----
let pendingUpdate: Update | null = null;

async function checkForUpdates(): Promise<void> {
  try {
    const update = await check();
    if (!update) return;
    pendingUpdate = update;
    updateText.textContent = `WattMail ${update.version} is available.`;
    updateBanner.classList.remove("hidden");
  } catch {
    /* offline, or no published update manifest yet — ignore */
  }
}

async function installUpdate(): Promise<void> {
  if (!pendingUpdate) return;
  updateInstall.disabled = true;
  updateText.textContent = "Downloading update…";
  try {
    await pendingUpdate.downloadAndInstall();
    await relaunch();
  } catch (e) {
    updateText.textContent = `Update failed: ${e}`;
    updateInstall.disabled = false;
  }
}

// ---- Boot ----
applyListWidth(loadListWidth());
resetReader();

async function boot(): Promise<void> {
  applyThemePref(loadThemePref());
  sortSelect.value = sortMode;
  try {
    brandVersion.textContent = `v${await getVersion()}`;
  } catch {
    /* version unavailable in dev is fine */
  }
  try {
    const signedIn = await invoke<boolean>("is_signed_in");
    if (signedIn) {
      await loadFolders();
      await refreshFromCache().catch(() => {}); // instant cached view (may be empty)
      await syncFolder(); // then update from the server
    } else {
      showSignedOut();
    }
  } catch (e) {
    statusEl.textContent = `Startup error: ${e}`;
    showSignedOut();
  }
  void checkForUpdates();
}

// Reveal the window once the shell is built — fast perceived startup, no flash
// of an empty/unstyled window. Skip it when autostarted into the tray; the user
// reveals the window from the tray icon when they want it.
invoke<boolean>("started_hidden")
  .then((hidden) => {
    if (!hidden) void getCurrentWindow().show();
  })
  .catch(() => void getCurrentWindow().show());
void boot();

// Pick up new mail in the current folder automatically (quietly, every 60s).
const AUTO_SYNC_MS = 60_000;
setInterval(() => {
  // Don't clobber displayed search results with a background folder refresh.
  if (currentFolderId && !searchActive) void syncFolder(true);
}, AUTO_SYNC_MS);
