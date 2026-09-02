// Pop-out message window — opened by double-click / Enter / context menu in the
// main window. Renders a single message in its own OS window, faithfully
// (header, sanitized body, attachments, meeting-invite RSVP, dark-mode
// adaptation) and delegates Reply / Forward / Delete / Headers to the main
// window (where the compose modal and headers overlay live). Print and Save-as-
// EML run locally. Shared email-rendering helpers come from ./email-render.ts.

import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { emit, listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import { save } from "@tauri-apps/plugin-dialog";
import {
  adaptPlainEmail,
  EMAIL_FRAME_SANDBOX,
  readThemeColors,
  safeExternalHref,
  wireEmailFrame,
  wrapEmailHtml,
} from "./email-render";
import "./styles.css";

// ---- Backend DTOs (mirror src-tauri/src/commands.rs) ----
interface MessageView {
  id: string;
  subject: string;
  from: string;
  to: string[];
  cc: string[];
  bcc: string[];
  received: string;
  html: string;
  importance: "low" | "normal" | "high";
  remoteBlocked: boolean;
  designed: boolean;
}
interface AttachmentInfo {
  id: string;
  name: string;
  contentType: string;
  size: number;
}
interface MeetingInviteInfo {
  eventId: string;
  start: string;
  end: string;
  isAllDay: boolean;
  responseStatus: string;
}
interface MessageTarget {
  messageId: string;
  subject: string;
}

// ---- Formatting helpers ----
function esc(s: string): string {
  return s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]!);
}
function fmtDateFull(iso: string): string {
  if (!iso) return "";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const p = (x: number): string => x.toString().padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
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
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}
function safeFilename(subject: string): string {
  const cleaned = subject
    .replace(/[<>:"/\\|?*\x00-\x1f]/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .replace(/[. ]+$/g, "");
  return cleaned.slice(0, 120) || "message";
}

// ---- Theme ----
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

const INVITE_STATUS_LABEL: Record<string, string> = {
  accepted: "You accepted this invitation.",
  tentativelyAccepted: "You tentatively accepted this invitation.",
  declined: "You declined this invitation.",
};

const appEl = document.querySelector<HTMLDivElement>("#app")!;
const win = getCurrentWindow();
let messageId = "";
let currentMsg: MessageView | null = null;

// Outcome line for local actions — the pop-out otherwise has nowhere to
// surface a failed save/preview.
const statusBar = document.createElement("div");
statusBar.className = "settings-msg";
statusBar.setAttribute("role", "status");
statusBar.setAttribute("aria-live", "polite");
statusBar.style.cssText =
  "position:fixed;left:8px;right:8px;bottom:4px;pointer-events:none;z-index:50";
document.body.appendChild(statusBar);
function showStatus(text: string): void {
  statusBar.textContent = text;
}

async function main(): Promise<void> {
  applyThemePref(loadThemePref());

  // Re-resolve on OS theme change while following the system.
  matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
    if (loadThemePref() === "system") {
      applyThemePref("system");
      if (currentMsg) renderMessage(currentMsg);
    }
  });
  // Stay in sync if the user changes the theme in the main window (it writes the
  // same localStorage key; the `storage` event fires in this other window).
  window.addEventListener("storage", (e) => {
    if (e.key === THEME_KEY) {
      applyThemePref(loadThemePref());
      if (currentMsg) renderMessage(currentMsg);
    }
  });

  // Never show the webview's native (web-page) context menu anywhere.
  document.addEventListener("contextmenu", (e) => e.preventDefault());

  let target: MessageTarget;
  try {
    target = await invoke<MessageTarget>("message_window_target");
  } catch (e) {
    appEl.innerHTML = `<div class="reader-empty">This message is no longer available.</div>`;
    return;
  }
  messageId = target.messageId;
  document.title = target.subject ? `${target.subject} — WattMail` : "Message — WattMail";

  // Close automatically when the message is deleted or moved elsewhere (the main
  // window emits this on successful delete/move of this message id).
  void listen("message-removed", (e) => {
    if (e.payload && (e.payload as { messageId?: string }).messageId === messageId) {
      void win.close();
    }
  });

  appEl.innerHTML = `<div class="reader-empty">Loading…</div>`;
  await loadAndRender(false);

  // Mark the message read (mirrors the reading pane) and tell the main window so
  // its list row + unread badges update immediately rather than at next sync.
  void markRead();
}

async function loadAndRender(allowImages: boolean): Promise<void> {
  try {
    const msg = await invoke<MessageView>("load_message", { id: messageId, allowImages });
    currentMsg = msg;
    renderMessage(msg);
    void loadAttachments(messageId);
    void probeMeetingInvite(messageId);
  } catch (e) {
    appEl.innerHTML = `<div class="reader-empty">Failed to load message: ${esc(String(e))}</div>`;
  }
}

function renderMessage(msg: MessageView): void {
  currentMsg = msg;
  const to = msg.to.length ? ` · to ${esc(msg.to.join(", "))}` : "";
  const cc = msg.cc.length ? `<div class="reader-meta">cc ${esc(msg.cc.join(", "))}</div>` : "";
  const bcc = msg.bcc.length ? `<div class="reader-meta">bcc ${esc(msg.bcc.join(", "))}</div>` : "";
  const importance =
    msg.importance === "high"
      ? `<div class="reader-importance high">! This message was sent with high importance.</div>`
      : msg.importance === "low"
        ? `<div class="reader-importance">&darr; This message was sent with low importance.</div>`
        : "";
  const banner = msg.remoteBlocked
    ? `<button id="load-images" class="reader-banner" type="button">Images blocked &mdash; click to load images for this message</button>`
    : "";
  appEl.innerHTML = `
    <div class="reader-head">
      <div class="reader-subject">${esc(msg.subject)}</div>
      ${importance}
      <div class="reader-meta">${esc(msg.from)}</div>
      <div class="reader-meta">${esc(fmtDateFull(msg.received))}${to}</div>
      ${cc}${bcc}
      <div class="reader-actions">
        <button id="reply-btn" class="btn btn-xs">Reply</button>
        <button id="reply-all-btn" class="btn btn-xs">Reply all</button>
        <button id="forward-btn" class="btn btn-xs">Forward</button>
        <button id="delete-btn" class="btn btn-xs btn-ghost" title="Delete this message">Delete</button>
        <button id="print-btn" class="btn btn-xs btn-ghost" title="Print this message (Ctrl+P)">Print</button>
        <button id="save-eml-btn" class="btn btn-xs btn-ghost" title="Save this message to disk as an .eml file">Save as EML</button>
        <button id="headers-btn" class="btn btn-xs btn-ghost" title="View raw message headers and trace its origin">Headers</button>
      </div>
    </div>
    <div id="reader-invite" class="reader-invite hidden"></div>
    <div id="reader-attachments" class="reader-attachments"></div>
    ${banner}
    <iframe class="reader-frame reader-frame--win" sandbox="${EMAIL_FRAME_SANDBOX}" referrerpolicy="no-referrer"></iframe>
  `;
  appEl.querySelector<HTMLButtonElement>("#load-images")?.addEventListener("click", () => {
    void loadAndRender(true);
  });
  appEl.querySelector<HTMLButtonElement>("#reply-btn")?.addEventListener("click", () => delegate("reply"));
  appEl.querySelector<HTMLButtonElement>("#reply-all-btn")?.addEventListener("click", () => delegate("replyAll"));
  appEl.querySelector<HTMLButtonElement>("#forward-btn")?.addEventListener("click", () => delegate("forward"));
  appEl.querySelector<HTMLButtonElement>("#delete-btn")?.addEventListener("click", () => delegate("delete"));
  appEl.querySelector<HTMLButtonElement>("#print-btn")?.addEventListener("click", printMessage);
  appEl.querySelector<HTMLButtonElement>("#save-eml-btn")?.addEventListener("click", () => saveAsEml());
  appEl.querySelector<HTMLButtonElement>("#headers-btn")?.addEventListener("click", () => delegate("headers"));

  const frame = appEl.querySelector<HTMLIFrameElement>(".reader-frame")!;
  const dark = resolveTheme(loadThemePref()) === "business";
  const adapt = !msg.designed && dark;
  const theme = readThemeColors();
  frame.classList.toggle("is-paper-card", !adapt);
  wireFrameLinks(frame);
  frame.addEventListener("load", () => {
    wireFrameLinks(frame);
    if (adapt) adaptPlainEmail(frame, theme);
  });
  frame.srcdoc = wrapEmailHtml(msg.html, { adapt, bg: theme.bg, fg: theme.fg });
}

function printMessage(): void {
  appEl.querySelector<HTMLIFrameElement>(".reader-frame")?.contentWindow?.print();
}

// Delegate an action to the main window: it owns the compose modal, the headers
// overlay, and the list (delete/move). The main listener also focuses itself.
function delegate(action: "reply" | "replyAll" | "forward" | "delete" | "headers"): void {
  void emit("message-window-action", {
    messageId,
    action,
    subject: currentMsg?.subject ?? "",
  });
}

// ---- Attachments ----
async function loadAttachments(id: string): Promise<void> {
  let list: AttachmentInfo[];
  try {
    list = await invoke<AttachmentInfo[]>("attachments", { messageId: id });
  } catch {
    return;
  }
  const container = appEl.querySelector<HTMLDivElement>("#reader-attachments");
  if (!container) return;
  const chips = list
    .map((a, i) => {
      const ct = a.contentType.toLowerCase();
      const hint = ct.startsWith("image/") ? "Preview" : ct === "application/pdf" ? "Open" : "Download";
      return (
        `<span class="attach-chip-group">` +
        `<button class="attach-chip" data-i="${i}" title="${hint} ${esc(a.name)}">&#128206; ${esc(a.name)} <span class="attach-size">${fmtBytes(a.size)}</span></button>` +
        `<button class="attach-save" data-i="${i}" title="Save ${esc(a.name)}">&#8595;</button>` +
        `</span>`
      );
    })
    .join("");
  container.innerHTML = chips;
  container.querySelectorAll<HTMLButtonElement>(".attach-chip").forEach((b) => {
    b.addEventListener("click", () => {
      const a = list[Number(b.dataset.i)];
      if (!a) return;
      const ct = a.contentType.toLowerCase();
      if (ct.startsWith("image/")) void previewImage(id, a);
      else if (ct === "application/pdf") void openAttachmentExternal(id, a);
      else void downloadAttachment(id, a.id, a.name);
    });
  });
  container.querySelectorAll<HTMLButtonElement>(".attach-save").forEach((b) => {
    b.addEventListener("click", () => {
      const a = list[Number(b.dataset.i)];
      if (a) void downloadAttachment(id, a.id, a.name);
    });
  });
}

async function downloadAttachment(id: string, attachmentId: string, name: string): Promise<void> {
  try {
    const path = await save({ defaultPath: name });
    if (!path) return;
    showStatus(`Saving ${name}…`);
    await invoke("save_attachment", { messageId: id, attachmentId, destPath: path });
    showStatus(`Saved ${name}`);
  } catch (e) {
    showStatus(`Download failed: ${e}`);
  }
}

async function previewImage(id: string, a: AttachmentInfo): Promise<void> {
  try {
    const url = await invoke<string>("attachment_data_url", { messageId: id, attachmentId: a.id });
    openLightbox(url, a.name);
  } catch (e) {
    showStatus(`Preview failed: ${e}`);
  }
}

async function openAttachmentExternal(id: string, a: AttachmentInfo): Promise<void> {
  try {
    await invoke("open_attachment", { messageId: id, attachmentId: a.id });
  } catch (e) {
    showStatus(`Open failed: ${e}`);
  }
}

// Minimal lightbox for image attachments (data: URL served by the backend).
const lightbox = document.createElement("div");
lightbox.className = "overlay hidden";
lightbox.innerHTML = `<figure class="lightbox-figure"><img class="lightbox-img" alt="" /><figcaption class="lightbox-caption"></figcaption></figure>`;
document.body.appendChild(lightbox);
function openLightbox(dataUrl: string, name: string): void {
  lightbox.querySelector<HTMLImageElement>(".lightbox-img")!.src = dataUrl;
  lightbox.querySelector<HTMLElement>(".lightbox-caption")!.textContent = name;
  lightbox.classList.remove("hidden");
}
lightbox.addEventListener("click", () => lightbox.classList.add("hidden"));
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !lightbox.classList.contains("hidden")) lightbox.classList.add("hidden");
});

// ---- Save as EML ----
async function saveAsEml(): Promise<void> {
  const subject = currentMsg?.subject ?? "";
  try {
    const path = await save({
      defaultPath: `${safeFilename(subject)}.eml`,
      filters: [{ name: "Email message", extensions: ["eml"] }],
    });
    if (!path) return;
    await invoke("export_message", { id: messageId, destPath: path });
    showStatus("Saved message as .eml");
  } catch (e) {
    showStatus(`Save failed: ${e}`);
  }
}

// ---- Meeting-invite RSVP ----
async function probeMeetingInvite(id: string): Promise<void> {
  let invite: MeetingInviteInfo | null;
  try {
    invite = await invoke<MeetingInviteInfo | null>("meeting_invite", {
      id,
      timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
    });
  } catch {
    return;
  }
  if (!invite) return;
  const bar = appEl.querySelector<HTMLDivElement>("#reader-invite");
  if (!bar) return;
  renderInviteBar(bar, invite);
}

function fmtInviteWhen(invite: MeetingInviteInfo): string {
  const s = new Date(invite.start);
  if (isNaN(s.getTime())) return "";
  const day = s.toLocaleDateString(undefined, { weekday: "short", day: "numeric", month: "short" });
  if (invite.isAllDay) return `${day} · All day`;
  const t = (d: Date) => d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
  const e = new Date(invite.end);
  return isNaN(e.getTime()) ? `${day} · ${t(s)}` : `${day} · ${t(s)} – ${t(e)}`;
}

function renderInviteBar(bar: HTMLDivElement, invite: MeetingInviteInfo): void {
  const when = fmtInviteWhen(invite);
  const status = INVITE_STATUS_LABEL[invite.responseStatus] ?? "";
  bar.title = "This message contains a meeting invitation.";
  bar.innerHTML = `
    <span class="reader-invite-label">&#128197; Meeting invitation${when ? ` · ${esc(when)}` : ""}</span>
    ${status ? `<span class="reader-invite-status">${esc(status)}</span>` : ""}
    <span class="reader-invite-actions">
      <button class="btn btn-xs" data-invite-rsvp="accept">Accept</button>
      <button class="btn btn-xs" data-invite-rsvp="tentative">Tentative</button>
      <button class="btn btn-xs" data-invite-rsvp="decline">Decline</button>
    </span>`;
  bar.classList.remove("hidden");
  bar.querySelectorAll<HTMLButtonElement>("[data-invite-rsvp]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const response = btn.dataset.inviteRsvp as "accept" | "tentative" | "decline";
      bar.querySelectorAll<HTMLButtonElement>("button").forEach((b) => (b.disabled = true));
      invoke("respond_to_event", {
        id: invite.eventId,
        response,
        comment: null,
        sendResponse: true,
      })
        .then(() => {
          const done =
            response === "accept"
              ? "Accepted"
              : response === "tentative"
                ? "Tentatively accepted"
                : "Declined";
          bar.innerHTML = `<span class="reader-invite-label">&#128197; ${done} — your response was sent to the organizer.</span>`;
        })
        .catch((e) => {
          bar.querySelectorAll<HTMLButtonElement>("button").forEach((b) => (b.disabled = false));
          const label = bar.querySelector<HTMLSpanElement>(".reader-invite-label");
          if (label) label.textContent = `Could not send response: ${e}`;
        });
    });
  });
}

// ---- Links inside the email body ----
function wireFrameLinks(frame: HTMLIFrameElement): void {
  wireEmailFrame(frame, {
    onClick: ({ href }) => {
      const url = safeExternalHref(href);
      if (url) void openUrl(url);
    },
  });
}

async function markRead(): Promise<void> {
  try {
    await invoke("set_read", { id: messageId, read: true });
    void emit("message-read-changed", { messageId, read: true });
  } catch {
    // best-effort
  }
}

// Ctrl+P prints the open message.
document.addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && !e.altKey && e.key.toLowerCase() === "p") {
    e.preventDefault();
    printMessage();
  }
});

void main();
