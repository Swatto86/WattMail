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
interface AttachmentInfo {
  id: string;
  name: string;
  contentType: string;
  size: number;
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
    <div class="settings-panel compose-panel">
      <div class="settings-title" id="compose-title">New message</div>
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
        <button id="compose-send" class="btn btn-sm btn-primary">Send</button>
      </div>
    </div>
  </div>
`;

const accountEl = document.querySelector<HTMLDivElement>("#account")!;
const brandVersion = document.querySelector<HTMLSpanElement>("#brand-version")!;
const refreshBtn = document.querySelector<HTMLButtonElement>("#refresh")!;
const gear = document.querySelector<HTMLButtonElement>("#gear")!;
const sortSelect = document.querySelector<HTMLSelectElement>("#sort")!;
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
const composeTitle = document.querySelector<HTMLDivElement>("#compose-title")!;
const cToInput = document.querySelector<HTMLInputElement>("#c-to")!;
const cCcInput = document.querySelector<HTMLInputElement>("#c-cc")!;
const cSubjectInput = document.querySelector<HTMLInputElement>("#c-subject")!;
const cBodyInput = document.querySelector<HTMLDivElement>("#c-body")!;
const composeToolbar = document.querySelector<HTMLDivElement>("#c-toolbar")!;
const composeMsg = document.querySelector<HTMLDivElement>("#compose-msg")!;
const composeCancel = document.querySelector<HTMLButtonElement>("#compose-cancel")!;
const composeSendBtn = document.querySelector<HTMLButtonElement>("#compose-send")!;
const cAttachBtn = document.querySelector<HTMLButtonElement>("#c-attach")!;
const cAttachments = document.querySelector<HTMLDivElement>("#c-attachments")!;

// ---- View state ----
let selectedId: string | null = null;
let lastMessage: MessageView | null = null;
let currentIds = new Set<string>();
let currentFolderId: string | null = null;
let folders: FolderInfo[] = [];
let accountEmail = "";

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
    return;
  }

  const folder = folders.find((f) => f.id === currentFolderId);
  const showRecipient = !!folder && isOutgoingFolder(folder.name);

  const rows = sortMessages(inbox.messages)
    .map((m) => {
      const unread = m.isRead ? "" : "unread";
      const dot = m.isRead ? "" : `<span class="dot"></span>`;
      const who = showRecipient ? `To: ${esc(m.to)}` : esc(senderName(m.from));
      const whoTitle = showRecipient ? esc(m.to) : esc(m.from);
      return `
        <div class="msg ${unread}" data-id="${esc(m.id)}">
          <div class="msg-dot">${dot}</div>
          <div class="msg-main">
            <div class="msg-top">
              <span class="msg-from" title="${whoTitle}">${who}</span>
              <span class="msg-date">${esc(fmtDate(m.received))}</span>
            </div>
            <div class="msg-subject" title="${esc(m.subject)}">${esc(m.subject)}</div>
            <div class="msg-preview">${esc(m.preview)}</div>
          </div>
        </div>`;
    })
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
    await refreshFromCache(true);
  }
}

// Delete a message (moves it to Deleted Items), updating the list optimistically.
async function deleteMessage(id: string): Promise<void> {
  rowFor(id)?.remove();
  if (selectedId === id) resetReader();
  try {
    await invoke("delete_message", { id });
    await refreshFromCache(true); // update loaded/total count
    await loadFolders(); // refresh unread badges
  } catch (e) {
    statusEl.textContent = `Delete failed: ${e}`;
    await refreshFromCache(true); // restore the row from cache
  }
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
  if (id === currentFolderId) return;
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

// ---- Compose ----
let composeAttachPaths: string[] = [];

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
  quotedHtml: string;
}): void {
  composeTitle.textContent = opts.title;
  cToInput.value = opts.to.join(", ");
  cCcInput.value = opts.cc.join(", ");
  cSubjectInput.value = opts.subject;
  // The quoted original (if any) goes into the editor so it's visible and editable.
  cBodyInput.innerHTML = opts.quotedHtml ? `<p><br></p>${opts.quotedHtml}` : "";
  composeAttachPaths = [];
  renderComposeAttachments();
  composeMsg.textContent = "";
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

function parseAddresses(value: string): string[] {
  return value
    .split(/[,;]/)
    .map((a) => a.trim())
    .filter(Boolean);
}

async function sendCompose(): Promise<void> {
  const to = parseAddresses(cToInput.value);
  if (to.length === 0) {
    composeMsg.textContent = "Add at least one recipient.";
    return;
  }
  composeSendBtn.disabled = true;
  composeMsg.textContent = "Sending…";
  const bodyHtml = cBodyInput.innerHTML;
  try {
    await invoke("send_message", {
      to,
      cc: parseAddresses(cCcInput.value),
      subject: cSubjectInput.value,
      bodyHtml,
      attachmentPaths: composeAttachPaths,
    });
    closeCompose();
    statusEl.textContent = "Message sent.";
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
  if (row?.dataset.id) void openMessage(row.dataset.id);
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

function showCtxMenu(x: number, y: number, id: string, unread: boolean): void {
  ctxTargetId = id;
  const items: Array<{ act: string; label: string; danger?: boolean } | "sep"> = [
    { act: "open", label: "Open" },
    { act: "reply", label: "Reply" },
    { act: "replyAll", label: "Reply all" },
    { act: "forward", label: "Forward" },
    "sep",
    { act: "toggleRead", label: unread ? "Mark as read" : "Mark as unread" },
    { act: "delete", label: "Delete", danger: true },
  ];
  ctxMenu.innerHTML = items
    .map((it) =>
      it === "sep"
        ? `<div class="ctx-sep"></div>`
        : `<button class="ctx-item${it.danger ? " ctx-danger" : ""}" data-act="${it.act}">${it.label}</button>`,
    )
    .join("");
  // Reveal at the origin to measure, then clamp inside the viewport.
  ctxMenu.style.left = "0";
  ctxMenu.style.top = "0";
  ctxMenu.classList.remove("hidden");
  const left = Math.max(4, Math.min(x, window.innerWidth - ctxMenu.offsetWidth - 4));
  const top = Math.max(4, Math.min(y, window.innerHeight - ctxMenu.offsetHeight - 4));
  ctxMenu.style.left = `${left}px`;
  ctxMenu.style.top = `${top}px`;
  rowFor(id)?.classList.add("ctx-target");
}

listEl.addEventListener("contextmenu", (e) => {
  const row = (e.target as HTMLElement).closest<HTMLElement>(".msg");
  if (!row?.dataset.id) return; // off a row: leave the default menu
  e.preventDefault();
  showCtxMenu(e.clientX, e.clientY, row.dataset.id, row.classList.contains("unread"));
});

ctxMenu.addEventListener("click", (e) => {
  const act = (e.target as HTMLElement).closest<HTMLElement>(".ctx-item")?.dataset.act;
  const id = ctxTargetId;
  const unread = id ? (rowFor(id)?.classList.contains("unread") ?? false) : false;
  hideCtxMenu();
  if (!act || !id) return;
  switch (act) {
    case "open":
      void openMessage(id);
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
refreshBtn.addEventListener("click", () => void syncFolder());
sortSelect.addEventListener("change", () => {
  sortMode = sortSelect.value as SortMode;
  localStorage.setItem(SORT_KEY, sortMode);
  void refreshFromCache(true);
});
gear.addEventListener("click", openSettings);
signinBtn.addEventListener("click", () => void signIn());
settingsClose.addEventListener("click", closeSettings);
signoutBtn.addEventListener("click", () => void signOut());
composeBtn.addEventListener("click", composeNew);
composeCancel.addEventListener("click", closeCompose);
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
// Paste as plain text so pasted markup can't carry hostile HTML into the message.
cBodyInput.addEventListener("paste", (e) => {
  e.preventDefault();
  const text = e.clipboardData?.getData("text/plain") ?? "";
  document.execCommand("insertText", false, text);
});
composeOverlay.addEventListener("click", (e) => {
  if (e.target === composeOverlay) closeCompose();
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
  if (!settingsOverlay.classList.contains("hidden")) closeSettings();
  else if (!composeOverlay.classList.contains("hidden")) closeCompose();
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
  if (currentFolderId) void syncFolder(true);
}, AUTO_SYNC_MS);
