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
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import { initCalendar, loadCalendar, resetCalendar } from "./calendar";
import "./styles.css";

// ---- Backend DTOs (mirror src-tauri/src/commands.rs) ----
interface Account {
  displayName: string;
  email: string;
}
interface AccountSummary {
  id: string;
  provider: string; // slug: office365 | outlook | gmail
  providerLabel: string;
  email: string;
  displayName: string;
  active: boolean;
  supportsRules: boolean;
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
  hasAttachments: boolean;
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
  designed: boolean; // email sets its own (non-white) background -> render on a light card
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
interface MessageRule {
  id: string;
  displayName: string;
  sequence: number;
  isEnabled: boolean;
  conditions: {
    senderContains: string[];
    subjectContains: string[];
    recipientContains: string[];
  };
  actions: {
    moveToFolderId: string | null;
    markAsRead: boolean;
  };
}
interface NewMailBatch {
  count: number;
  newestId: string;
  newestSubject: string;
}

// The delta sync caches only a bounded recent window of each folder. The list
// reads a growing window of the cache; "Load more" grows it by PAGE_SIZE and,
// once the cache is exhausted, backfills older history from the server. Switching
// folders resets both the window and the "reached the folder's start" flag.
const PAGE_SIZE = 50;
let loadedCount = PAGE_SIZE;
let currentTotal = 0;
// Set once a server backfill returns nothing older — the folder's start is cached
// and "Load more" should stop offering. Reset on every folder switch.
let reachedOldest = false;
// Guards against overlapping backfills from rapid "Load more" clicks.
let backfilling = false;

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
  if (loadThemePref() === "system") {
    applyThemePref("system");
    reRenderOpenMessage();
  }
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
// Well-known system folders. Rename/Delete are withheld for these: Graph rejects
// deleting them anyway, and — critically — the app identifies Inbox/Drafts/Sent by
// displayName (tray unread count, outgoing-column rendering, draft resume), so a
// rename would silently break that detection. Matched by name to stay consistent
// with the app's existing English-mailbox special-casing.
const PROTECTED_FOLDER_NAMES = new Set([
  "inbox",
  "drafts",
  "sent items",
  "sent",
  "outbox",
  "deleted items",
  "junk email",
  "archive",
  "conversation history",
]);
function isProtectedFolder(name: string): boolean {
  return PROTECTED_FOLDER_NAMES.has(name.toLowerCase());
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

// ---- Quick filters (client-side, over the loaded window) ----
type FilterMode = "all" | "unread" | "flagged" | "attachments";
const FILTER_KEY = "wattmail.filter";
function loadFilterMode(): FilterMode {
  const v = localStorage.getItem(FILTER_KEY);
  return v === "unread" || v === "flagged" || v === "attachments" ? v : "all";
}
let filterMode: FilterMode = loadFilterMode();

function applyFilter(messages: Message[]): Message[] {
  switch (filterMode) {
    case "unread":
      return messages.filter((m) => !m.isRead);
    case "flagged":
      return messages.filter((m) => m.isFlagged);
    case "attachments":
      return messages.filter((m) => m.hasAttachments);
    default:
      return messages;
  }
}

// ---- Date grouping (Outlook-style sections) ----
const GROUP_KEY = "wattmail.group";
// Grouping is on by default; only meaningful for date-ordered sorts.
let groupByDate = localStorage.getItem(GROUP_KEY) !== "0";
function groupingActive(): boolean {
  return groupByDate && (sortMode === "dateDesc" || sortMode === "dateAsc");
}

// The Outlook-style section a message falls in, relative to now: Today,
// Yesterday, This Week, Last Week, This Month, Last Month, then "Month YYYY".
function dateSectionLabel(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "Unknown date";
  const now = new Date();
  const dayMs = 86_400_000;
  const startOfDay = (x: Date): number =>
    new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  const today = startOfDay(now);
  const msgDay = startOfDay(d);
  const diffDays = Math.round((today - msgDay) / dayMs);
  if (diffDays <= 0) return "Today"; // today (future-dated mail lumps in here)
  if (diffDays === 1) return "Yesterday";
  // Week starts Monday (dow: 0 = Mon … 6 = Sun).
  const dow = (now.getDay() + 6) % 7;
  const startOfWeek = today - dow * dayMs;
  if (msgDay >= startOfWeek) return "This Week";
  if (msgDay >= startOfWeek - 7 * dayMs) return "Last Week";
  if (d.getFullYear() === now.getFullYear() && d.getMonth() === now.getMonth()) return "This Month";
  const lastMonth = new Date(now.getFullYear(), now.getMonth() - 1, 1);
  if (d.getFullYear() === lastMonth.getFullYear() && d.getMonth() === lastMonth.getMonth()) {
    return "Last Month";
  }
  return `${d.toLocaleString(undefined, { month: "long" })} ${d.getFullYear()}`;
}

// Build the list body: grouped into date sections when grouping is active,
// otherwise a flat list of rows. `sorted` must already be in display order.
function renderListBody(sorted: Message[], showRecipient: boolean): string {
  if (!groupingActive()) {
    return sorted.map((m) => messageRowHtml(m, showRecipient)).join("");
  }
  let html = "";
  let section = "";
  for (const m of sorted) {
    const label = dateSectionLabel(m.received);
    if (label !== section) {
      section = label;
      html += `<div class="msg-section">${esc(label)}</div>`;
    }
    html += messageRowHtml(m, showRecipient);
  }
  return html;
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
      <div id="view-nav" class="view-nav hidden" role="group" aria-label="Switch view">
        <button type="button" data-view="mail" class="active">Mail</button>
        <button type="button" data-view="calendar" id="view-calendar-btn">Calendar</button>
      </div>
      <button id="account" class="account-switch text-xs opacity-70 flex-1 hidden" title="Switch account" type="button"></button>
      <div class="toolbar-search mail-only">
        <input id="search" class="input input-bordered input-xs" type="search" placeholder="Search mail…" autocomplete="off" />
        <button id="search-clear" class="search-clear hidden" type="button" title="Clear search">&times;</button>
      </div>
      <div id="filter-seg" class="filter-seg mail-only" role="group" aria-label="Filter messages">
        <button type="button" data-filter="all" title="Show all">All</button>
        <button type="button" data-filter="unread" title="Unread only">Unread</button>
        <button type="button" data-filter="flagged" title="Flagged only">Flagged</button>
        <button type="button" data-filter="attachments" title="With attachments only" aria-label="With attachments only">
          <svg class="seg-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48"/></svg>
        </button>
      </div>
      <select id="sort" class="select select-bordered select-xs mail-only" title="Sort by">
        <option value="dateDesc">Newest</option>
        <option value="dateAsc">Oldest</option>
        <option value="sender">Sender</option>
        <option value="subject">Subject</option>
        <option value="unread">Unread first</option>
      </select>
      <button id="group-toggle" class="btn btn-xs mail-only" type="button" title="Group by date">&#9776;</button>
      <button id="compose" class="btn btn-xs btn-primary mail-only" title="Compose">&#9993; Compose</button>
      <button id="refresh" class="btn btn-xs mail-only" title="Refresh">&#8635; Refresh</button>
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

    <div id="calendar" class="calendar-view flex flex-col flex-1 min-h-0 hidden"></div>

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
      <label class="settings-row">
        <span>Show notifications for new mail<br /><span class="hint">A desktop alert when unread messages arrive in the Inbox</span></span>
        <input type="checkbox" id="set-notifications" class="toggle toggle-sm toggle-primary" />
      </label>
      <div class="settings-row" id="rules-row">
        <span>Inbox rules<br /><span class="hint">Server-side rules that move or mark messages on arrival</span></span>
        <button id="rules-btn" class="btn btn-sm">Rules&hellip;</button>
      </div>
      <div class="settings-row settings-accounts">
        <div class="settings-accounts-head">
          <span>Accounts<br /><span class="hint">Switch, add, or remove mailboxes</span></span>
          <button id="add-account-btn" class="btn btn-sm btn-primary">Add account</button>
        </div>
        <div id="accounts-list" class="accounts-list"></div>
      </div>
      <div id="settings-msg" class="settings-msg"></div>
      <div class="settings-actions"><button id="settings-close" class="btn btn-sm">Close</button></div>
    </div>
  </div>

  <div id="provider-overlay" class="overlay hidden">
    <div class="settings-panel provider-panel">
      <div class="settings-title">Add an account</div>
      <div class="hint" style="margin-bottom: 8px">Choose your email provider</div>
      <div id="provider-list" class="provider-list"></div>
      <div id="provider-msg" class="settings-msg"></div>
      <div class="settings-actions"><button id="provider-cancel" class="btn btn-sm">Cancel</button></div>
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

  <div id="rules-overlay" class="overlay hidden">
    <div class="settings-panel rules-panel">
      <div class="rules-head">
        <div class="settings-title">Inbox rules</div>
        <div class="rules-tools">
          <button id="rules-new" class="btn btn-xs btn-primary">New rule</button>
          <button id="rules-close" class="btn btn-xs">Close</button>
        </div>
      </div>
      <div id="rules-msg" class="settings-msg"></div>
      <div id="rules-list" class="rules-list"></div>
      <div id="rules-editor" class="rules-editor hidden">
        <div class="settings-title" id="rules-editor-title">New rule</div>
        <label class="settings-row"><span>Name</span><input id="rule-name" class="input input-bordered input-sm" placeholder="Rule name" autocomplete="off" /></label>
        <label class="settings-row"><span>Sequence (order)</span><input id="rule-sequence" class="input input-bordered input-sm" type="number" value="1" min="1" /></label>
        <label class="settings-row"><span>Enabled</span><input type="checkbox" id="rule-enabled" class="toggle toggle-sm toggle-primary" /></label>
        <label class="settings-row"><span>Sender contains<br /><span class="hint">Comma-separated</span></span><input id="rule-sender" class="input input-bordered input-sm" placeholder="e.g. newsletter@example.com" autocomplete="off" /></label>
        <label class="settings-row"><span>Subject contains<br /><span class="hint">Comma-separated</span></span><input id="rule-subject" class="input input-bordered input-sm" placeholder="e.g. invoice, receipt" autocomplete="off" /></label>
        <label class="settings-row"><span>Recipient contains<br /><span class="hint">Comma-separated</span></span><input id="rule-recipient" class="input input-bordered input-sm" placeholder="e.g. support@example.com" autocomplete="off" /></label>
        <label class="settings-row"><span>Move to folder</span><select id="rule-folder" class="select select-bordered select-sm"><option value="">(none)</option></select></label>
        <label class="settings-row"><span>Mark as read</span><input type="checkbox" id="rule-markread" class="toggle toggle-sm toggle-primary" /></label>
        <div class="settings-actions" style="gap: 8px">
          <button id="rule-delete" class="btn btn-sm btn-error hidden">Delete</button>
          <span style="flex:1"></span>
          <button id="rule-cancel" class="btn btn-sm">Cancel</button>
          <button id="rule-save" class="btn btn-sm btn-primary">Save</button>
        </div>
      </div>
    </div>
  </div>

  <div id="shortcuts-overlay" class="overlay hidden">
    <div class="settings-panel shortcuts-panel">
      <div class="settings-title">Keyboard shortcuts</div>
      <table class="shortcuts-table">
        <tbody>
          <tr><td><kbd>j</kbd> / <kbd>&darr;</kbd></td><td>Move cursor down</td></tr>
          <tr><td><kbd>k</kbd> / <kbd>&uarr;</kbd></td><td>Move cursor up</td></tr>
          <tr><td><kbd>Enter</kbd></td><td>Open message under cursor</td></tr>
          <tr><td><kbd>r</kbd></td><td>Reply</td></tr>
          <tr><td><kbd>a</kbd></td><td>Reply all</td></tr>
          <tr><td><kbd>f</kbd></td><td>Forward</td></tr>
          <tr><td><kbd>u</kbd></td><td>Toggle read / unread</td></tr>
          <tr><td><kbd>g</kbd></td><td>Toggle follow-up flag</td></tr>
          <tr><td><kbd>#</kbd></td><td>Delete message</td></tr>
          <tr><td><kbd>c</kbd></td><td>Compose new message</td></tr>
          <tr><td><kbd>/</kbd></td><td>Focus search</td></tr>
          <tr><td><kbd>?</kbd></td><td>Show this cheat-sheet</td></tr>
          <tr><td><kbd>Esc</kbd></td><td>Close modal / overlay</td></tr>
        </tbody>
      </table>
      <div class="settings-actions"><button id="shortcuts-close" class="btn btn-sm">Close</button></div>
    </div>
  </div>
`;

const accountEl = document.querySelector<HTMLButtonElement>("#account")!;
const brandVersion = document.querySelector<HTMLSpanElement>("#brand-version")!;
const refreshBtn = document.querySelector<HTMLButtonElement>("#refresh")!;
const gear = document.querySelector<HTMLButtonElement>("#gear")!;
const sortSelect = document.querySelector<HTMLSelectElement>("#sort")!;
const filterSeg = document.querySelector<HTMLDivElement>("#filter-seg")!;
const groupToggle = document.querySelector<HTMLButtonElement>("#group-toggle")!;
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
const calendarHost = document.querySelector<HTMLDivElement>("#calendar")!;
const viewNav = document.querySelector<HTMLDivElement>("#view-nav")!;
const viewCalendarBtn = document.querySelector<HTMLButtonElement>("#view-calendar-btn")!;
const foldersEl = document.querySelector<HTMLDivElement>("#folders")!;
const listEl = document.querySelector<HTMLDivElement>("#list")!;
const splitter = document.querySelector<HTMLDivElement>("#splitter")!;
const readerEl = document.querySelector<HTMLDivElement>("#reader")!;
const statusEl = document.querySelector<HTMLDivElement>("#status")!;
const settingsOverlay = document.querySelector<HTMLDivElement>("#settings-overlay")!;
const setTheme = document.querySelector<HTMLSelectElement>("#set-theme")!;
const setTray = document.querySelector<HTMLInputElement>("#set-tray")!;
const setAutostart = document.querySelector<HTMLInputElement>("#set-autostart")!;
const addAccountBtn = document.querySelector<HTMLButtonElement>("#add-account-btn")!;
const accountsListEl = document.querySelector<HTMLDivElement>("#accounts-list")!;
const rulesRow = document.querySelector<HTMLDivElement>("#rules-row")!;
const providerOverlay = document.querySelector<HTMLDivElement>("#provider-overlay")!;
const providerListEl = document.querySelector<HTMLDivElement>("#provider-list")!;
const providerCancelBtn = document.querySelector<HTMLButtonElement>("#provider-cancel")!;
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

// ---- Notification / rules / shortcuts element refs ----
const setNotifications = document.querySelector<HTMLInputElement>("#set-notifications")!;
const rulesBtn = document.querySelector<HTMLButtonElement>("#rules-btn")!;
const rulesOverlay = document.querySelector<HTMLDivElement>("#rules-overlay")!;
const rulesMsg = document.querySelector<HTMLDivElement>("#rules-msg")!;
const rulesList = document.querySelector<HTMLDivElement>("#rules-list")!;
const rulesEditor = document.querySelector<HTMLDivElement>("#rules-editor")!;
const rulesEditorTitle = document.querySelector<HTMLDivElement>("#rules-editor-title")!;
const rulesNewBtn = document.querySelector<HTMLButtonElement>("#rules-new")!;
const rulesCloseBtn = document.querySelector<HTMLButtonElement>("#rules-close")!;
const ruleName = document.querySelector<HTMLInputElement>("#rule-name")!;
const ruleSequence = document.querySelector<HTMLInputElement>("#rule-sequence")!;
const ruleEnabled = document.querySelector<HTMLInputElement>("#rule-enabled")!;
const ruleSender = document.querySelector<HTMLInputElement>("#rule-sender")!;
const ruleSubject = document.querySelector<HTMLInputElement>("#rule-subject")!;
const ruleRecipient = document.querySelector<HTMLInputElement>("#rule-recipient")!;
const ruleFolder = document.querySelector<HTMLSelectElement>("#rule-folder")!;
const ruleMarkRead = document.querySelector<HTMLInputElement>("#rule-markread")!;
const ruleDeleteBtn = document.querySelector<HTMLButtonElement>("#rule-delete")!;
const ruleCancelBtn = document.querySelector<HTMLButtonElement>("#rule-cancel")!;
const ruleSaveBtn = document.querySelector<HTMLButtonElement>("#rule-save")!;
const shortcutsOverlay = document.querySelector<HTMLDivElement>("#shortcuts-overlay")!;
const shortcutsCloseBtn = document.querySelector<HTMLButtonElement>("#shortcuts-close")!;

// ---- View state ----
let selectedId: string | null = null;
let lastMessage: MessageView | null = null;
let currentIds = new Set<string>();
let currentFolderId: string | null = null;
let folders: FolderInfo[] = [];
let accountEmail = "";
let accountList: AccountSummary[] = [];
// Keyboard-navigation cursor: the id of the row the cursor is on, kept distinct
// from `selectedId` (the opened/read message) so j/k can move without opening.
// Reconciled against the rendered rows on every list re-render (see syncCursor).
let cursorId: string | null = null;

// Primary view (Mail vs Calendar) + sign-in / capability state. reflectView()
// derives all toolbar/view visibility from these, so showSignedIn (called on
// every cache refresh) stays a cheap, idempotent class toggle.
type AppMode = "mail" | "calendar";
let appMode: AppMode = "mail";
let signedIn = false;
let calendarSupported = false;
let calendarInited = false;

// Build the calendar module's DOM on first use only.
function ensureCalendarInit(): void {
  if (calendarInited) return;
  initCalendar(calendarHost);
  calendarInited = true;
}

// Enforce the current mode's visibility. Idempotent and network-free.
function reflectView(): void {
  const showMail = signedIn && appMode === "mail";
  const showCal = signedIn && appMode === "calendar";
  mainView.classList.toggle("hidden", !showMail);
  calendarHost.classList.toggle("hidden", !showCal);
  document
    .querySelectorAll<HTMLElement>(".mail-only")
    .forEach((el) => el.classList.toggle("hidden", !showMail));
  viewNav
    .querySelectorAll<HTMLButtonElement>("button[data-view]")
    .forEach((b) => b.classList.toggle("active", b.dataset.view === appMode));
  // Hide the Calendar tab for mail-only accounts (e.g. Gmail).
  viewCalendarBtn.classList.toggle("hidden", !calendarSupported);
}

function switchView(mode: AppMode): void {
  if (mode === appMode) return;
  if (mode === "calendar" && !calendarSupported) return;
  appMode = mode;
  reflectView();
  if (mode === "calendar") {
    ensureCalendarInit();
    void loadCalendar();
  }
}

viewNav.addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest<HTMLButtonElement>("button[data-view]");
  if (btn) switchView(btn.dataset.view as AppMode);
});

function showSignedOut(): void {
  signedIn = false;
  appMode = "mail";
  calendarSupported = false;
  if (calendarInited) resetCalendar();
  signinView.classList.remove("hidden");
  viewNav.classList.add("hidden");
  accountEl.classList.add("hidden");
  accountEl.textContent = "";
  reflectView();
  accountEmail = "";
  accountList = [];
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
  signedIn = true;
  signinView.classList.add("hidden");
  accountEl.classList.remove("hidden");
  viewNav.classList.remove("hidden");
  reflectView();
}

// Refresh the active account's calendar capability (whether its provider has a
// calendar backend), updating the tab. If the calendar tab is open but the new
// account is mail-only, fall back to mail.
async function refreshCalendarCapability(): Promise<void> {
  try {
    calendarSupported = await invoke<boolean>("account_supports_calendar");
  } catch {
    calendarSupported = false;
  }
  if (!calendarSupported && appMode === "calendar") {
    appMode = "mail";
  }
  reflectView();
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
  const attach = m.hasAttachments
    ? `<span class="msg-attach" title="Has attachments" aria-label="Has attachments"><svg class="msg-attach-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48"/></svg></span>`
    : "";
  const who = showRecipient ? `To: ${esc(m.to)}` : esc(senderName(m.from));
  const whoTitle = showRecipient ? esc(m.to) : esc(m.from);
  return `
        <div class="msg ${unread}${flagged}" data-id="${esc(m.id)}">
          <div class="msg-dot">${dot}</div>
          <div class="msg-main">
            <div class="msg-top">
              <span class="msg-from" title="${whoTitle}">${who}</span>
              <span class="msg-date">${attach}${flag}${esc(fmtDate(m.received))}</span>
            </div>
            <div class="msg-subject" title="${esc(m.subject)}">${esc(m.subject)}</div>
            <div class="msg-preview">${esc(m.preview)}</div>
          </div>
        </div>`;
}

function renderInbox(inbox: Inbox): void {
  if (inbox.account) {
    accountEmail = inbox.account.email;
    accountEl.textContent = inbox.account.displayName
      ? `${inbox.account.displayName} · ${inbox.account.email}`
      : inbox.account.email;
  }
  currentIds = new Set(inbox.messages.map((m) => m.id));
  currentTotal = inbox.total;

  const folder = folders.find((f) => f.id === currentFolderId);
  const showRecipient = !!folder && isOutgoingFolder(folder.name);

  // Quick-filter then sort the loaded window; grouping happens in renderListBody.
  const visible = sortMessages(applyFilter(inbox.messages));

  // Rows cached but not yet in the window can be shown instantly; once those run
  // out, "Load more" backfills older history from the server until its start is
  // reached (`reachedOldest`).
  const cachedRemaining = inbox.total - inbox.messages.length;
  const label =
    cachedRemaining > 0
      ? `Load ${Math.min(cachedRemaining, PAGE_SIZE)} more (${cachedRemaining} older)`
      : "Load older messages";
  const more =
    cachedRemaining > 0 || !reachedOldest
      ? `<button class="load-more" data-role="load-more">${esc(label)}</button>`
      : "";

  if (visible.length === 0) {
    const empty =
      inbox.messages.length === 0 ? "No messages." : "No messages match this filter.";
    listEl.innerHTML = `<div class="p-6 text-center opacity-60">${empty}</div>` + more;
  } else {
    listEl.innerHTML = renderListBody(visible, showRecipient) + more;
  }

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
  // The quick filter also applies to search results; grouping does not (results
  // keep their relevance/date order under the search header).
  const visible = sortMessages(applyFilter(results));
  const count =
    visible.length === results.length ? `${results.length}` : `${visible.length} of ${results.length}`;
  const header = `<div class="search-header">Search results for: ${esc(query)} (${count})</div>`;
  if (visible.length === 0) {
    listEl.innerHTML = header + `<div class="p-6 text-center opacity-60">No matching messages.</div>`;
    if (selectedId && !currentIds.has(selectedId)) resetReader();
    syncCursor();
    return;
  }
  const rows = visible.map((m) => messageRowHtml(m, false)).join("");
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
  // Designed mail (and everything in light mode) renders on the light paper
  // card; plain mail in dark mode adapts to the theme surface instead.
  const dark = resolveTheme(loadThemePref()) === "business";
  const adapt = !msg.designed && dark;
  const theme = readThemeColors();
  frame.classList.toggle("is-paper-card", !adapt);
  frame.addEventListener("load", () => {
    wireFrameLinks(frame);
    if (adapt) adaptPlainEmail(frame, theme);
  });
  frame.srcdoc = wrapEmailHtml(msg.html, { adapt, bg: theme.bg, fg: theme.fg });
}

// Re-render the open message after a theme change so the body re-applies the
// light-card vs. adapt-to-theme decision. No network fetch — it reuses the
// already-loaded message, and the cached html is never mutated (the adapt pass
// only edits the freshly rebuilt iframe DOM). Attachments are reloaded because
// renderReader rebuilds the reader DOM.
function reRenderOpenMessage(): void {
  if (!lastMessage) return;
  renderReader(lastMessage);
  void loadAttachments(lastMessage.id);
}

// Intercept clicks inside the (script-disabled, same-origin) email frame so links
// open in the system browser — where the user can see the real destination —
// instead of navigating the frame. Also intercept right-click on links to offer
// a "Copy link address" context menu.
function wireFrameLinks(frame: HTMLIFrameElement): void {
  const doc = frame.contentDocument;
  if (!doc) return;
  doc.addEventListener("click", (ev) => {
    ev.preventDefault();
    const anchor = (ev.target as HTMLElement | null)?.closest?.("a");
    const href = anchor?.getAttribute("href") ?? "";
    if (/^https?:\/\//i.test(href)) void openUrl(href);
  });
  doc.addEventListener("contextmenu", (ev) => {
    const anchor = (ev.target as HTMLElement | null)?.closest?.("a");
    if (!anchor) return; // off a link: leave the default menu
    const href = anchor.getAttribute("href") ?? "";
    if (!href) return;
    ev.preventDefault();
    showLinkContextMenu(ev.clientX, ev.clientY, href, frame);
  });
}

// ---- Link context menu (reading pane) ----
const linkCtxMenu = document.createElement("div");
linkCtxMenu.className = "ctx-menu link-ctx-menu hidden";
linkCtxMenu.setAttribute("role", "menu");
linkCtxMenu.innerHTML = `<button class="ctx-item" data-act="copy">Copy link address</button>`;
document.body.appendChild(linkCtxMenu);

function showLinkContextMenu(x: number, y: number, href: string, frame: HTMLIFrameElement): void {
  linkCtxMenu.style.left = "0";
  linkCtxMenu.style.top = "0";
  linkCtxMenu.classList.remove("hidden");
  // Position relative to the viewport (the iframe coords are already viewport-relative
  // because contextmenu event clientX/Y are from the iframe's viewport, which maps
  // to the parent viewport via the iframe's bounding rect).
  const frameRect = frame.getBoundingClientRect();
  const absX = frameRect.left + x;
  const absY = frameRect.top + y;
  const left = Math.max(4, Math.min(absX, window.innerWidth - linkCtxMenu.offsetWidth - 4));
  const top = Math.max(4, Math.min(absY, window.innerHeight - linkCtxMenu.offsetHeight - 4));
  linkCtxMenu.style.left = `${left}px`;
  linkCtxMenu.style.top = `${top}px`;
  linkCtxMenu.dataset.href = href;
}

linkCtxMenu.addEventListener("click", (e) => {
  e.stopPropagation();
  const item = (e.target as HTMLElement).closest<HTMLElement>(".ctx-item");
  if (!item) return;
  const href = linkCtxMenu.dataset.href ?? "";
  if (item.dataset.act === "copy") {
    void copyText(href).then((ok) => {
      item.textContent = ok ? "Copied!" : "Copy failed";
      setTimeout(() => {
        item.textContent = "Copy link address";
        linkCtxMenu.classList.add("hidden");
      }, 1000);
    });
  }
});

document.addEventListener("click", (e) => {
  if (!linkCtxMenu.contains(e.target as Node)) linkCtxMenu.classList.add("hidden");
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") linkCtxMenu.classList.add("hidden");
});
window.addEventListener("blur", () => linkCtxMenu.classList.add("hidden"));

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

// ---- Adaptive dark-mode email rendering ----
//
// DESIGNED emails (those that set their own non-white background) always render
// on the light "paper" card so the author's colours read as intended. PLAIN
// emails (no background of their own — most personal/business mail) follow the
// app theme: in dark mode they render on the theme surface, and a load-time
// pass repairs any author text colour that would otherwise be dark-on-dark,
// while leaving colours that are already readable (and therefore intentional).
// WCAG-grounded: relative luminance + contrast ratio, lightness-only lift.

type RGB = { r: number; g: number; b: number }; // 0..255

const NAMED_COLORS: Record<string, string> = {
  black: "#000000", white: "#ffffff", red: "#ff0000", green: "#008000",
  blue: "#0000ff", gray: "#808080", grey: "#808080", silver: "#c0c0c0",
  navy: "#000080", maroon: "#800000", purple: "#800080", teal: "#008080",
  olive: "#808000", lime: "#00ff00", aqua: "#00ffff", cyan: "#00ffff",
  fuchsia: "#ff00ff", magenta: "#ff00ff", yellow: "#ffff00", orange: "#ffa500",
  transparent: "", inherit: "", currentcolor: "", initial: "", unset: "",
};

const RGB_RE = /^rgba?\(\s*([\d.]+)[ ,]+([\d.]+)[ ,]+([\d.]+)(?:[ ,/]+([\d.]+%?))?\s*\)$/;

function parseColor(raw: string): RGB | null {
  if (!raw) return null;
  let v = raw.trim().toLowerCase();
  if (v in NAMED_COLORS) {
    const m = NAMED_COLORS[v];
    if (!m) return null;
    v = m;
  }
  if (v[0] === "#") {
    if (v.length === 4) v = "#" + v[1] + v[1] + v[2] + v[2] + v[3] + v[3];
    if (v.length === 7) {
      const n = parseInt(v.slice(1), 16);
      if (Number.isNaN(n)) return null;
      return { r: (n >> 16) & 255, g: (n >> 8) & 255, b: n & 255 };
    }
    return null;
  }
  const m = v.match(RGB_RE);
  if (m) {
    const a = m[4] == null ? 1 : m[4].endsWith("%") ? parseFloat(m[4]) / 100 : parseFloat(m[4]);
    if (a === 0) return null; // fully transparent == no colour
    return { r: +m[1], g: +m[2], b: +m[3] };
  }
  return null; // hsl()/oklch()/keywords: skip safely
}

function relLuminance(c: RGB): number {
  const lin = (x: number) => {
    x /= 255;
    return x <= 0.04045 ? x / 12.92 : Math.pow((x + 0.055) / 1.055, 2.4);
  };
  return 0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b);
}

function contrast(a: RGB, b: RGB): number {
  const la = relLuminance(a), lb = relLuminance(b);
  return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
}

function rgbToHsl(c: RGB): { h: number; s: number; l: number } {
  const r = c.r / 255, g = c.g / 255, b = c.b / 255;
  const mx = Math.max(r, g, b), mn = Math.min(r, g, b);
  let h = 0, s = 0;
  const l = (mx + mn) / 2;
  if (mx !== mn) {
    const d = mx - mn;
    s = l > 0.5 ? d / (2 - mx - mn) : d / (mx + mn);
    h = mx === r ? (g - b) / d + (g < b ? 6 : 0) : mx === g ? (b - r) / d + 2 : (r - g) / d + 4;
    h /= 6;
  }
  return { h, s, l };
}

function hslToRgb(h: number, s: number, l: number): RGB {
  const f = (n: number) => {
    const k = (n + h * 12) % 12;
    const a = s * Math.min(l, 1 - l);
    return l - a * Math.max(-1, Math.min(k - 3, 9 - k, 1));
  };
  return { r: Math.round(f(0) * 255), g: Math.round(f(8) * 255), b: Math.round(f(4) * 255) };
}

function rgbToCss(c: RGB): string {
  return `rgb(${c.r}, ${c.g}, ${c.b})`;
}

function isNearNeutral(c: RGB): boolean {
  return rgbToHsl(c).s < 0.15; // chroma proxy: plain black/grey
}

// Contrast vs a dark background is monotonic in HSL lightness, so binary-search
// the *minimum* lightness (same hue/saturation) that clears the target — keeps
// the author's hue while making the colour readable.
function liftToContrast(fg: RGB, bg: RGB, target: number): RGB {
  const { h, s, l } = rgbToHsl(fg);
  let lo = l, hi = 1;
  let best = hslToRgb(h, s, 1);
  for (let i = 0; i < 12; i++) {
    const mid = (lo + hi) / 2;
    const cand = hslToRgb(h, s, mid);
    if (contrast(cand, bg) >= target) {
      best = cand;
      hi = mid;
    } else {
      lo = mid;
    }
  }
  return best;
}

// Resolve the active DaisyUI theme tokens to concrete rgb in the PARENT (the
// iframe has no DaisyUI tokens). A throwaway probe span lets the browser resolve
// oklch(var(--bc))/oklch(var(--b1)) for us.
function readThemeColors(): { bg: RGB; fg: RGB } {
  const p = document.createElement("span");
  p.style.cssText =
    "color:oklch(var(--bc));background:oklch(var(--b1));position:absolute;left:-9999px";
  document.body.appendChild(p);
  const cs = getComputedStyle(p);
  const fg = parseColor(cs.color) ?? { r: 230, g: 230, b: 230 };
  const bg = parseColor(cs.backgroundColor) ?? { r: 24, g: 24, b: 27 };
  p.remove();
  return { bg, fg };
}

const BODY_CONTRAST = 4.5; // WCAG AA, normal text
const ACCENT_CONTRAST = 3.0; // WCAG AA, large text / UI: leave a brand colour that already clears this
const NODE_CAP = 4000;
const SKIP_TAGS = new Set(["IMG", "PICTURE", "SVG", "CANVAS", "VIDEO", "OBJECT", "EMBED", "IFRAME"]);

// Repair author text colours inside a same-origin frame. Run on the frame
// 'load' event, only for (plain email AND dark theme).
function adaptPlainEmail(frame: HTMLIFrameElement, theme: { bg: RGB; fg: RGB }): void {
  const doc = frame.contentDocument;
  if (!doc) return;

  // First explicit, non-transparent ancestor background; else the dark card.
  const localBg = (el: Element): RGB => {
    let n: Element | null = el;
    while (n && n !== doc.body) {
      const h = n as HTMLElement;
      const c = parseColor(h.style?.backgroundColor) ?? parseColor(h.style?.background ?? "");
      if (c) return c;
      n = n.parentElement;
    }
    return theme.bg;
  };

  const styled = doc.querySelectorAll<HTMLElement>("[style], font[color]");
  const limit = Math.min(styled.length, NODE_CAP);
  for (let i = 0; i < limit; i++) {
    const el = styled[i];
    if (SKIP_TAGS.has(el.tagName)) continue;

    // Light local cell (e.g. a highlight): keep the author's dark text intact.
    if (relLuminance(localBg(el)) > 0.5) continue;

    const raw = el.style.color || (el.tagName === "FONT" ? el.getAttribute("color") ?? "" : "");
    const authored = parseColor(raw);
    if (!authored) continue;

    const isLink = el.closest("a") != null;
    const gate = isLink ? ACCENT_CONTRAST : BODY_CONTRAST;
    if (contrast(authored, theme.bg) >= gate) continue; // already readable + intentional: keep

    if (!isLink && isNearNeutral(authored)) {
      // Plain black/grey body text: inherit the theme foreground.
      el.style.removeProperty("color");
      if (el.tagName === "FONT") el.removeAttribute("color");
    } else {
      // Chromatic accent or link: preserve hue, lift lightness until it clears.
      el.style.color = rgbToCss(liftToContrast(authored, theme.bg, gate));
      if (el.tagName === "FONT") el.removeAttribute("color");
    }
  }
  // Dark local author backgrounds are left to blend into the dark card.
}

type WrapOpts = { adapt: boolean; bg: RGB; fg: RGB };

// Wrap a sanitized body for the reading-pane iframe. Plain text in light mode
// (and every designed email) renders on the light "paper" card — the
// conservative default that matches the author's light-background assumption.
// Plain mail in dark mode renders on the theme surface (adaptPlainEmail then
// repairs author text colours).
function wrapEmailHtml(inner: string, opts: WrapOpts): string {
  const bg = opts.adapt ? rgbToCss(opts.bg) : "#ffffff";
  const fg = opts.adapt ? rgbToCss(opts.fg) : "#1a1a1a";
  const link = opts.adapt ? rgbToCss(liftToContrast({ r: 37, g: 99, b: 235 }, opts.bg, ACCENT_CONTRAST)) : "#2563eb";
  const quote = opts.adapt
    ? `border-left: 3px solid ${rgbToCss(opts.fg)}; opacity: 0.85;`
    : "border-left: 3px solid #cbd5e1; color: #475569;";
  return `<!doctype html><html><head><meta charset="utf-8" />
<meta name="referrer" content="no-referrer" />
<style>
  html, body { margin: 0; }
  body {
    padding: 16px;
    font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
    font-size: 14px; line-height: 1.55; color: ${fg}; background: ${bg};
    word-wrap: break-word; overflow-wrap: anywhere;
  }
  a { color: ${link}; }
  img { max-width: 100%; height: auto; }
  table { max-width: 100%; }
  pre { white-space: pre-wrap; }
  blockquote { margin: 0 0 0 12px; padding-left: 12px; ${quote} }
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

// ---- Message rules manager ----
// Server-side inbox rules via Graph messageRule. The UI lists existing rules,
// and an editor form creates/updates/deletes them. Conditions are simplified to
// sender/subject/recipient contains; actions are move-to-folder or mark-as-read.

let editingRuleId: string | null = null;

async function openRules(): Promise<void> {
  rulesMsg.textContent = "";
  rulesEditor.classList.add("hidden");
  rulesOverlay.classList.remove("hidden");
  await loadRules();
}

function closeRules(): void {
  rulesOverlay.classList.add("hidden");
}

async function loadRules(): Promise<void> {
  rulesList.innerHTML = `<div class="rules-loading">Loading rules…</div>`;
  try {
    const rules = await invoke<MessageRule[]>("list_message_rules");
    renderRulesList(rules);
  } catch (e) {
    rulesList.innerHTML = `<div class="rules-loading">Could not load rules: ${esc(String(e))}</div>`;
  }
}

function ruleSummary(rule: MessageRule): string {
  const parts: string[] = [];
  if (rule.conditions.senderContains.length) parts.push(`from: ${rule.conditions.senderContains.join(", ")}`);
  if (rule.conditions.subjectContains.length) parts.push(`subject: ${rule.conditions.subjectContains.join(", ")}`);
  if (rule.conditions.recipientContains.length) parts.push(`to: ${rule.conditions.recipientContains.join(", ")}`);
  const cond = parts.length ? parts.join(" · ") : "(no conditions)";
  const actions: string[] = [];
  if (rule.actions.moveToFolderId) {
    const folder = folders.find((f) => f.id === rule.actions.moveToFolderId);
    actions.push(`move to ${folder ? folder.name : "folder"}`);
  }
  if (rule.actions.markAsRead) actions.push("mark as read");
  const act = actions.length ? actions.join(" · ") : "(no actions)";
  return `${cond} &rarr; ${act}`;
}

function renderRulesList(rules: MessageRule[]): void {
  if (rules.length === 0) {
    rulesList.innerHTML = `<div class="rules-loading">No rules yet. Click "New rule" to create one.</div>`;
    return;
  }
  rulesList.innerHTML = rules
    .map(
      (r) => `
      <div class="rule-item" data-rid="${esc(r.id)}">
        <div class="rule-info">
          <div class="rule-name">${esc(r.displayName)}${r.isEnabled ? "" : " <span class=\"rule-disabled\">(disabled)</span>"}</div>
          <div class="rule-summary">${ruleSummary(r)}</div>
        </div>
        <button class="btn btn-xs rule-edit" data-rid="${esc(r.id)}">Edit</button>
      </div>`,
    )
    .join("");
  rulesList.querySelectorAll<HTMLButtonElement>(".rule-edit").forEach((btn) => {
    btn.addEventListener("click", () => void editRule(btn.dataset.rid!));
  });
}

function populateFolderDropdown(): void {
  ruleFolder.innerHTML = `<option value="">(none)</option>` +
    folders.map((f) => `<option value="${esc(f.id)}">${esc(f.name)}</option>`).join("");
}

function showRuleEditor(rule?: MessageRule): void {
  editingRuleId = rule?.id ?? null;
  rulesEditorTitle.textContent = rule ? "Edit rule" : "New rule";
  ruleName.value = rule?.displayName ?? "";
  ruleSequence.value = String(rule?.sequence ?? 1);
  ruleEnabled.checked = rule?.isEnabled ?? true;
  ruleSender.value = rule?.conditions.senderContains.join(", ") ?? "";
  ruleSubject.value = rule?.conditions.subjectContains.join(", ") ?? "";
  ruleRecipient.value = rule?.conditions.recipientContains.join(", ") ?? "";
  populateFolderDropdown();
  if (rule?.actions.moveToFolderId) ruleFolder.value = rule.actions.moveToFolderId;
  ruleMarkRead.checked = rule?.actions.markAsRead ?? false;
  ruleDeleteBtn.classList.toggle("hidden", !rule);
  rulesEditor.classList.remove("hidden");
}

async function editRule(id: string): Promise<void> {
  try {
    const rules = await invoke<MessageRule[]>("list_message_rules");
    const rule = rules.find((r) => r.id === id);
    if (rule) showRuleEditor(rule);
  } catch (e) {
    rulesMsg.textContent = `Could not load rule: ${e}`;
  }
}

function parseCommaList(value: string): string[] {
  return value.split(",").map((s) => s.trim()).filter(Boolean);
}

function buildRuleFromForm(): MessageRule {
  return {
    id: editingRuleId ?? "",
    displayName: ruleName.value.trim() || "Untitled rule",
    sequence: Number.parseInt(ruleSequence.value, 10) || 1,
    isEnabled: ruleEnabled.checked,
    conditions: {
      senderContains: parseCommaList(ruleSender.value),
      subjectContains: parseCommaList(ruleSubject.value),
      recipientContains: parseCommaList(ruleRecipient.value),
    },
    actions: {
      moveToFolderId: ruleFolder.value || null,
      markAsRead: ruleMarkRead.checked,
    },
  };
}

async function saveRule(): Promise<void> {
  const rule = buildRuleFromForm();
  ruleSaveBtn.disabled = true;
  rulesMsg.textContent = "Saving…";
  try {
    if (editingRuleId) {
      await invoke("update_message_rule", { id: editingRuleId, rule });
    } else {
      await invoke("create_message_rule", { rule });
    }
    rulesMsg.textContent = "";
    rulesEditor.classList.add("hidden");
    await loadRules();
  } catch (e) {
    rulesMsg.textContent = `Could not save rule: ${e}`;
  } finally {
    ruleSaveBtn.disabled = false;
  }
}

async function deleteRule(): Promise<void> {
  if (!editingRuleId) return;
  ruleDeleteBtn.disabled = true;
  rulesMsg.textContent = "Deleting…";
  try {
    await invoke("delete_message_rule", { id: editingRuleId });
    rulesMsg.textContent = "";
    rulesEditor.classList.add("hidden");
    await loadRules();
  } catch (e) {
    rulesMsg.textContent = `Could not delete rule: ${e}`;
  } finally {
    ruleDeleteBtn.disabled = false;
  }
}

rulesNewBtn.addEventListener("click", () => showRuleEditor());
rulesCloseBtn.addEventListener("click", closeRules);
ruleCancelBtn.addEventListener("click", () => rulesEditor.classList.add("hidden"));
ruleSaveBtn.addEventListener("click", () => void saveRule());
ruleDeleteBtn.addEventListener("click", () => void deleteRule());
rulesOverlay.addEventListener("click", (e) => {
  if (e.target === rulesOverlay) closeRules();
});

// ---- Keyboard shortcut cheat-sheet overlay ----
function toggleShortcuts(): void {
  shortcutsOverlay.classList.toggle("hidden");
}
function closeShortcuts(): void {
  shortcutsOverlay.classList.add("hidden");
}
shortcutsCloseBtn.addEventListener("click", closeShortcuts);
shortcutsOverlay.addEventListener("click", (e) => {
  if (e.target === shortcutsOverlay) closeShortcuts();
});

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
  reachedOldest = false; // re-enable server backfill for the new folder
  renderFolders();
  await refreshFromCache().catch(() => {});
  await syncFolder();
}

// ---- Desktop notifications for new mail ----
// After an Inbox sync, ask the backend to check for messages newer than the
// last-notified timestamp. If there are new unread messages, show a native OS
// notification; clicking it focuses the window and opens the message.
async function checkNewMail(): Promise<void> {
  try {
    const inbox = await invoke<Inbox>("folder_from_cache", {
      folderId: currentFolderId!,
      top: loadedCount,
    });
    const batch = await invoke<NewMailBatch | null>("check_new_mail", {
      messages: inbox.messages.map((m) => ({
        id: m.id,
        subject: m.subject,
        received: m.received,
        isRead: m.isRead,
      })),
    });
    if (!batch) return;
    const body = batch.count === 1 ? batch.newestSubject : `${batch.count} new messages`;
    await sendNotification({ title: "WattMail", body });
  } catch {
    /* notifications are best-effort — don't disrupt the sync */
  }
}

// ---- Actions ----
let syncing = false;
// Set when syncFolder is called while one is already in flight, so the request
// isn't silently dropped: the in-flight sync re-runs once it finishes, picking
// up whatever folder is current by then (e.g. after a quick folder switch).
let pendingSync = false;

// Render the current folder from the local SQLite cache — instant, offline-capable.
// `preserveScroll` keeps the list position during a background refresh.
async function refreshFromCache(preserveScroll = false): Promise<void> {
  const fid = currentFolderId;
  if (!fid) return;
  const scroll = listEl.scrollTop;
  const inbox = await invoke<Inbox>("folder_from_cache", {
    folderId: fid,
    top: loadedCount,
  });
  // Bail if the folder changed (or we signed out) while the cache read was in
  // flight — otherwise we'd paint this folder's rows under another folder's
  // selection. Mirrors the openMessage / search epoch guards.
  if (fid !== currentFolderId) return;
  showSignedIn();
  renderInbox(inbox);
  if (preserveScroll) listEl.scrollTop = scroll;
}

// Pull a page of older messages from the server into the cache, then re-render
// the grown window. Used by "Load more" once the cached window is exhausted, so
// the user can page back through the whole folder rather than only the recent
// slice the delta sync keeps. Scroll is preserved so new rows append below.
async function backfillOlder(): Promise<void> {
  const fid = currentFolderId;
  if (!fid || backfilling) return;
  backfilling = true;
  const scroll = listEl.scrollTop;
  const before = currentTotal;
  try {
    const inbox = await invoke<Inbox>("load_older", { folderId: fid, top: loadedCount });
    if (fid !== currentFolderId) return; // folder switched mid-flight
    if (inbox.total <= before) reachedOldest = true; // server had nothing older
    renderInbox(inbox);
    listEl.scrollTop = scroll;
  } catch {
    // Offline or fetch failed — undo the optimistic window growth so the button
    // stays and the user can retry.
    loadedCount = Math.max(PAGE_SIZE, loadedCount - PAGE_SIZE);
  } finally {
    backfilling = false;
  }
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
  if (!currentFolderId) return;
  if (syncing) {
    // A sync is already running; remember the request so the (possibly
    // newly-selected) folder is still pulled when it completes.
    pendingSync = true;
    return;
  }
  syncing = true;
  if (!quiet) {
    refreshBtn.disabled = true;
    statusEl.textContent = "Syncing…";
  }
  try {
    await invoke("sync_folder", { folderId: currentFolderId });
    await refreshFromCache(quiet);
    await loadFolders(); // refresh unread counts
    // After an Inbox sync, check for new mail to show a desktop notification.
    // Only when not searching (search results aren't the inbox) and the current
    // folder is the Inbox.
    if (!searchActive) {
      const inboxFolder = folders.find((f) => f.name.toLowerCase() === "inbox");
      if (inboxFolder && currentFolderId === inboxFolder.id) {
        void checkNewMail();
      }
    }
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
    if (pendingSync) {
      pendingSync = false;
      void syncFolder(quiet);
    }
  }
}

// ---- Accounts ----
// Pull the account list from the backend and re-render the toolbar switcher and
// the settings list. Cheap; called after any account change (add/switch/remove)
// and on boot.
async function refreshAccounts(): Promise<void> {
  try {
    accountList = await invoke<AccountSummary[]>("list_accounts");
  } catch {
    accountList = [];
  }
  renderAccountButton();
  renderAccountsSettings();
}

function renderAccountButton(): void {
  const active = accountList.find((a) => a.active);
  if (active) {
    const label =
      active.displayName && active.email
        ? `${active.displayName} · ${active.email}`
        : active.email || active.displayName || "Account";
    accountEl.textContent = label;
    accountEl.classList.toggle("has-many", accountList.length > 1);
    accountEl.title = accountList.length > 1 ? "Switch account" : label;
  }
}

function renderAccountsSettings(): void {
  // Hide the inbox-rules row unless the active account's provider supports them
  // (Exchange work/school only).
  const active = accountList.find((a) => a.active);
  rulesRow.style.display = active && active.supportsRules ? "" : "none";

  if (accountList.length === 0) {
    accountsListEl.innerHTML = `<div class="accounts-empty">No accounts signed in.</div>`;
    return;
  }
  accountsListEl.innerHTML = accountList
    .map(
      (a) => `
        <div class="account-row${a.active ? " account-row-active" : ""}">
          <div class="account-row-info">
            <div class="account-row-name">${esc(a.displayName || a.email || "Account")}${a.active ? ` <span class="account-badge">Active</span>` : ""}</div>
            <div class="account-row-email">${esc(a.email)}<span class="provider-tag">${esc(a.providerLabel)}</span></div>
          </div>
          <div class="account-row-actions">
            ${a.active ? "" : `<button class="btn btn-xs" data-switch="${esc(a.id)}">Switch</button>`}
            <button class="btn btn-xs btn-error" data-remove="${esc(a.id)}">Remove</button>
          </div>
        </div>`,
    )
    .join("");
}

// Reset per-account view state and load the (now) active account's mailbox, so
// one mailbox's folders, cursor, or search never bleed into another's.
async function loadActiveAccount(): Promise<void> {
  currentFolderId = null;
  folders = [];
  loadedCount = PAGE_SIZE;
  reachedOldest = false;
  searchActive = false;
  searchSeq++;
  searchInput.value = "";
  setSearchClearVisible(false);
  cursorId = null;
  // Drop the previous mailbox's agenda window/selection so it can't carry over.
  if (calendarInited) resetCalendar();
  resetReader();
  showSignedIn();
  await refreshCalendarCapability();
  // If the calendar tab is showing (e.g. after switching to a calendar-capable
  // account), refresh its contents for the new mailbox.
  if (appMode === "calendar" && calendarSupported) {
    ensureCalendarInit();
    void loadCalendar();
  }
  await loadFolders();
  await refreshFromCache().catch(() => {});
  await syncFolder();
}

// The providers a user can add. Tags match the backend's ProviderKind serde
// names (see ProviderKind::from_tag).
const PROVIDERS: { tag: string; label: string; hint: string }[] = [
  { tag: "office365", label: "Office 365", hint: "Work or school (Microsoft)" },
  { tag: "outlook_consumer", label: "Outlook.com / Hotmail", hint: "Personal Microsoft account" },
  { tag: "gmail", label: "Gmail", hint: "Google account" },
];

let providerResolve: ((tag: string | null) => void) | null = null;

// Show the provider chooser and resolve with the chosen tag (or null if cancelled).
// Only providers the backend reports as configured (real OAuth credentials, not
// placeholders) are offered; if exactly one is available, it's auto-selected.
async function pickProvider(): Promise<string | null> {
  let providers = PROVIDERS;
  try {
    const configured = new Set(await invoke<string[]>("configured_providers"));
    const filtered = PROVIDERS.filter((p) => configured.has(p.tag));
    if (filtered.length) providers = filtered;
  } catch {
    // Backend unreachable: fall back to the full list rather than block sign-in.
  }
  if (providers.length === 1) return providers[0].tag;
  providerListEl.innerHTML = providers
    .map(
      (p) => `
      <button class="provider-option" data-provider="${esc(p.tag)}">
        <span class="provider-option-label">${esc(p.label)}</span>
        <span class="provider-option-hint">${esc(p.hint)}</span>
      </button>`,
    )
    .join("");
  providerOverlay.classList.remove("hidden");
  return new Promise((resolve) => {
    providerResolve = resolve;
  });
}
function closeProvider(tag: string | null): void {
  providerOverlay.classList.add("hidden");
  const resolve = providerResolve;
  providerResolve = null;
  resolve?.(tag);
}
providerListEl.addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest("button");
  if (btn?.dataset.provider) closeProvider(btn.dataset.provider);
});
providerCancelBtn.addEventListener("click", () => closeProvider(null));
providerOverlay.addEventListener("click", (e) => {
  if (e.target === providerOverlay) closeProvider(null);
});

// Interactive add (browser sign-in). Used by the welcome screen, the toolbar
// switcher, and the settings "Add account" button. Prompts for a provider, then
// runs that provider's OAuth flow; on success the new account becomes active.
async function addAccount(): Promise<void> {
  const provider = await pickProvider();
  if (!provider) return;
  signinBtn.disabled = true;
  addAccountBtn.disabled = true;
  signinMsg.textContent = "Opening your browser to sign in…";
  settingsMsg.textContent = "Opening your browser to sign in…";
  try {
    await invoke<AccountSummary>("add_account", { provider });
    signinMsg.textContent = "";
    settingsMsg.textContent = "";
    await refreshAccounts();
    await loadActiveAccount();
  } catch (e) {
    signinMsg.textContent = `Sign-in failed: ${e}`;
    settingsMsg.textContent = `Add account failed: ${e}`;
  } finally {
    signinBtn.disabled = false;
    addAccountBtn.disabled = false;
  }
}

async function switchAccount(id: string): Promise<void> {
  const active = accountList.find((a) => a.active);
  if (active && active.id === id) return;
  try {
    await invoke("switch_account", { id });
  } catch (e) {
    statusEl.textContent = `Could not switch account: ${e}`;
    return;
  }
  await refreshAccounts();
  await loadActiveAccount();
}

async function removeAccount(id: string): Promise<void> {
  try {
    await invoke("remove_account", { id });
  } catch (e) {
    settingsMsg.textContent = `Could not remove account: ${e}`;
    return;
  }
  await refreshAccounts();
  if (accountList.length > 0) {
    await loadActiveAccount();
  } else {
    closeSettings();
    showSignedOut();
  }
}

// ---- Toolbar account switcher dropdown ----
const accountMenu = document.createElement("div");
accountMenu.className = "ctx-menu account-menu hidden";
accountMenu.setAttribute("role", "menu");
document.body.appendChild(accountMenu);

function hideAccountMenu(): void {
  accountMenu.classList.add("hidden");
}

function showAccountMenu(): void {
  const rows = accountList
    .map(
      (a) => `
        <button class="ctx-item account-item${a.active ? " account-active" : ""}" data-acc="${esc(a.id)}" title="${esc(a.email)}">
          <span class="account-item-name">${esc(a.displayName || a.email || "Account")}</span>
          <span class="account-item-email">${esc(a.email)}<span class="provider-tag">${esc(a.providerLabel)}</span></span>
        </button>`,
    )
    .join("");
  accountMenu.innerHTML =
    rows +
    `<div class="ctx-sep"></div>` +
    `<button class="ctx-item" data-acc-add="1">+ Add account…</button>`;
  accountMenu.classList.remove("hidden");
  // Position under the account button, clamped to the viewport.
  const r = accountEl.getBoundingClientRect();
  accountMenu.style.left = "0";
  accountMenu.style.top = "0";
  const left = Math.max(4, Math.min(r.left, window.innerWidth - accountMenu.offsetWidth - 4));
  const top = Math.min(r.bottom + 4, window.innerHeight - accountMenu.offsetHeight - 4);
  accountMenu.style.left = `${left}px`;
  accountMenu.style.top = `${top}px`;
}

accountEl.addEventListener("click", (e) => {
  e.stopPropagation();
  if (accountMenu.classList.contains("hidden")) showAccountMenu();
  else hideAccountMenu();
});
accountMenu.addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest("button");
  if (!btn) return;
  hideAccountMenu();
  if (btn.dataset.accAdd) void addAccount();
  else if (btn.dataset.acc) void switchAccount(btn.dataset.acc);
});
document.addEventListener("click", (e) => {
  if (!accountMenu.contains(e.target as Node) && e.target !== accountEl) hideAccountMenu();
});
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  hideAccountMenu();
  if (!providerOverlay.classList.contains("hidden")) closeProvider(null);
});
window.addEventListener("blur", hideAccountMenu);

// ---- Settings ----
function openSettings(): void {
  setTheme.value = loadThemePref();
  settingsMsg.textContent = "";
  void refreshAccounts();
  invoke<boolean>("get_close_to_tray")
    .then((v) => (setTray.checked = v))
    .catch(() => {});
  isAutostartEnabled()
    .then((v) => (setAutostart.checked = v))
    .catch(() => {});
  invoke<boolean>("get_notification_setting")
    .then((v) => (setNotifications.checked = v))
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
    loadedCount += PAGE_SIZE;
    // Widen from cache when it still holds unshown rows; otherwise pull older
    // history from the server.
    if (loadedCount <= currentTotal) void refreshFromCache(true);
    else void backfillOlder();
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
  hideFolderMenu(); // never leave the folder menu open alongside this one
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

// ---- Folder right-click context menu (create / rename / delete) ----
const folderMenu = document.createElement("div");
folderMenu.className = "ctx-menu hidden";
folderMenu.setAttribute("role", "menu");
document.body.appendChild(folderMenu);
// The right-clicked folder's id, or null when the click was on empty sidebar
// space (only "New folder…" applies then).
let folderMenuTargetId: string | null = null;

function hideFolderMenu(): void {
  folderMenu.classList.add("hidden");
  folderMenuTargetId = null;
}

function showFolderMenu(x: number, y: number, fid: string | null): void {
  hideCtxMenu(); // never leave the message menu open alongside this one
  folderMenuTargetId = fid;
  const items: Array<{ act: string; label: string; danger?: boolean } | "sep"> = [
    { act: "newFolder", label: "New folder…" },
  ];
  if (fid) {
    items.push({ act: "newSubfolder", label: "New subfolder…" });
    // Rename/Delete only for non-system folders — renaming Inbox/Drafts/Sent
    // would silently break the app's name-based folder detection.
    const target = folders.find((f) => f.id === fid);
    if (target && !isProtectedFolder(target.name)) {
      items.push(
        "sep",
        { act: "renameFolder", label: "Rename…" },
        { act: "deleteFolder", label: "Delete folder", danger: true },
      );
    }
  }
  folderMenu.innerHTML = items
    .map((it) =>
      it === "sep"
        ? `<div class="ctx-sep"></div>`
        : `<button class="ctx-item${it.danger ? " ctx-danger" : ""}" data-act="${it.act}">${it.label}</button>`,
    )
    .join("");
  folderMenu.classList.remove("hidden");
  // Position at the click point, clamped inside the viewport.
  folderMenu.style.left = "0";
  folderMenu.style.top = "0";
  const left = Math.max(4, Math.min(x, window.innerWidth - folderMenu.offsetWidth - 4));
  const top = Math.max(4, Math.min(y, window.innerHeight - folderMenu.offsetHeight - 4));
  folderMenu.style.left = `${left}px`;
  folderMenu.style.top = `${top}px`;
}

foldersEl.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  const btn = (e.target as HTMLElement).closest<HTMLElement>(".folder");
  showFolderMenu(e.clientX, e.clientY, btn?.dataset.fid ?? null);
});

folderMenu.addEventListener("click", (e) => {
  e.stopPropagation();
  const item = (e.target as HTMLElement).closest<HTMLElement>(".ctx-item");
  if (!item) return;
  const fid = folderMenuTargetId;
  hideFolderMenu();
  switch (item.dataset.act) {
    case "newFolder":
      void createFolder(null);
      break;
    case "newSubfolder":
      if (fid) void createFolder(fid);
      break;
    case "renameFolder":
      if (fid) void renameFolder(fid);
      break;
    case "deleteFolder":
      if (fid) void deleteFolder(fid);
      break;
  }
});

document.addEventListener("click", (e) => {
  if (!folderMenu.contains(e.target as Node)) hideFolderMenu();
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") hideFolderMenu();
});
window.addEventListener("blur", hideFolderMenu);

// Turn a folder-operation error into a readable status line. Graph rejects a
// duplicate name with a 409 whose raw body would otherwise dump JSON at the user.
function folderErrorText(action: string, name: string, e: unknown): string {
  const raw = String(e);
  if (/already exist/i.test(raw)) return `A folder named "${name}" already exists here.`;
  return `Could not ${action} folder: ${raw}`;
}

async function createFolder(parentId: string | null): Promise<void> {
  const name = window.prompt(parentId ? "New subfolder name:" : "New folder name:");
  if (name === null) return;
  const trimmed = name.trim();
  if (!trimmed) return;
  try {
    await invoke("create_folder", { name: trimmed, parentId });
    await loadFolders();
  } catch (e) {
    statusEl.textContent = folderErrorText("create", trimmed, e);
  }
}

async function renameFolder(id: string): Promise<void> {
  const current = folders.find((f) => f.id === id);
  const name = window.prompt("Rename folder:", current?.name ?? "");
  if (name === null) return;
  const trimmed = name.trim();
  if (!trimmed || trimmed === current?.name) return;
  try {
    await invoke("rename_folder", { id, name: trimmed });
    await loadFolders();
  } catch (e) {
    statusEl.textContent = folderErrorText("rename", trimmed, e);
  }
}

async function deleteFolder(id: string): Promise<void> {
  const target = folders.find((f) => f.id === id);
  const ok = window.confirm(
    `Delete folder "${target?.name ?? ""}" and everything in it? This cannot be undone.`,
  );
  if (!ok) return;
  try {
    await invoke("delete_folder", { id });
    if (currentFolderId === id) {
      // The open folder is gone — fall back to Inbox (or the first folder) and
      // load it, since loadFolders() only repaints the sidebar.
      currentFolderId = null;
      await loadFolders();
      if (currentFolderId) {
        loadedCount = PAGE_SIZE;
        reachedOldest = false;
        await refreshFromCache().catch(() => {});
        await syncFolder();
      }
    } else {
      await loadFolders();
    }
  } catch (e) {
    statusEl.textContent = `Could not delete folder: ${e}`;
  }
}
refreshBtn.addEventListener("click", () => {
  if (searchActive) exitSearch(); // Refresh leaves search and reloads the folder
  void syncFolder();
});
// Re-render the current view (search results or cached folder) without a fetch —
// used when sort / filter / grouping changes.
function rerenderList(): void {
  if (searchActive) void runSearch(searchInput.value);
  else void refreshFromCache(true);
}
function updateFilterUi(): void {
  for (const b of filterSeg.querySelectorAll<HTMLButtonElement>("button")) {
    b.classList.toggle("active", b.dataset.filter === filterMode);
  }
}
function updateGroupUi(): void {
  // Grouping only applies to date sorts: pressed when active, disabled otherwise.
  const dateSort = sortMode === "dateDesc" || sortMode === "dateAsc";
  groupToggle.classList.toggle("active", groupByDate && dateSort);
  groupToggle.disabled = !dateSort;
  groupToggle.title = !dateSort
    ? "Date grouping (sort by date to use)"
    : groupByDate
      ? "Grouped by date — click to ungroup"
      : "Group by date";
}
sortSelect.addEventListener("change", () => {
  sortMode = sortSelect.value as SortMode;
  localStorage.setItem(SORT_KEY, sortMode);
  updateGroupUi(); // group availability depends on the sort
  rerenderList();
});
filterSeg.addEventListener("click", (e) => {
  const f = (e.target as HTMLElement).closest("button")?.dataset.filter as FilterMode | undefined;
  if (!f || f === filterMode) return;
  filterMode = f;
  localStorage.setItem(FILTER_KEY, filterMode);
  updateFilterUi();
  rerenderList();
});
groupToggle.addEventListener("click", () => {
  groupByDate = !groupByDate;
  localStorage.setItem(GROUP_KEY, groupByDate ? "1" : "0");
  updateGroupUi();
  rerenderList();
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
signinBtn.addEventListener("click", () => void addAccount());
settingsClose.addEventListener("click", closeSettings);
addAccountBtn.addEventListener("click", () => void addAccount());
accountsListEl.addEventListener("click", (e) => {
  const btn = (e.target as HTMLElement).closest("button");
  if (!btn) return;
  if (btn.dataset.switch) void switchAccount(btn.dataset.switch);
  else if (btn.dataset.remove) void removeAccount(btn.dataset.remove);
});
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
setTheme.addEventListener("change", () => {
  setThemePref(setTheme.value as ThemePref);
  reRenderOpenMessage();
});
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
setNotifications.addEventListener("change", async () => {
  const want = setNotifications.checked;
  try {
    if (want) {
      // Request OS permission on first enable; if denied, revert the toggle.
      let granted = await isPermissionGranted();
      if (!granted) {
        const permission = await requestPermission();
        granted = permission === "granted";
      }
      if (!granted) {
        setNotifications.checked = false;
        settingsMsg.textContent = "Notification permission was denied. You can re-enable it from your system settings.";
        return;
      }
    }
    await invoke("set_notification_setting", { value: want });
  } catch (e) {
    settingsMsg.textContent = `Could not change notification setting: ${e}`;
    setNotifications.checked = !want;
  }
});
rulesBtn.addEventListener("click", () => void openRules());
settingsOverlay.addEventListener("click", (e) => {
  if (e.target === settingsOverlay) closeSettings();
});
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  if (!shortcutsOverlay.classList.contains("hidden")) closeShortcuts();
  else if (!rulesOverlay.classList.contains("hidden")) {
    if (!rulesEditor.classList.contains("hidden")) rulesEditor.classList.add("hidden");
    else closeRules();
  }
  else if (!headersOverlay.classList.contains("hidden")) closeHeaders();
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
    !headersOverlay.classList.contains("hidden") ||
    !rulesOverlay.classList.contains("hidden") ||
    !shortcutsOverlay.classList.contains("hidden")
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
  // A context menu owns the keyboard while open (its own Escape closes it);
  // don't fire list actions behind it. Covers both the message and folder menus.
  if (!ctxMenu.classList.contains("hidden") || !folderMenu.classList.contains("hidden")) return;

  // "/" focuses search regardless of the cursor; it's the one binding that
  // deliberately moves focus into a text field.
  if (e.key === "/") {
    e.preventDefault();
    searchInput.focus();
    searchInput.select();
    return;
  }

  // "?" opens the keyboard shortcut cheat-sheet (when not typing / in a modal).
  if (e.key === "?") {
    e.preventDefault();
    toggleShortcuts();
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

// Notification click: focus the window and open the message.
void listen<string>("open-message", (event) => {
  void getCurrentWindow().show();
  void getCurrentWindow().setFocus();
  const id = event.payload;
  if (id) void openMessage(id);
});

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
  updateFilterUi();
  updateGroupUi();
  try {
    brandVersion.textContent = `v${await getVersion()}`;
  } catch {
    /* version unavailable in dev is fine */
  }
  try {
    const signedIn = await invoke<boolean>("is_signed_in");
    if (signedIn) {
      await refreshAccounts();
      await loadActiveAccount(); // cached view first, then sync from the server
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
  // Only while the mail view is active: no point spending a Graph sync_folder
  // call + mail-DOM rebuild on a hidden list while the calendar tab is open.
  // Don't clobber displayed search results with a background folder refresh.
  if (appMode === "mail" && currentFolderId && !searchActive) void syncFolder(true);
}, AUTO_SYNC_MS);
