// WattMail calendar view — a self-contained module that owns all calendar DOM,
// so the mail UI in main.ts stays untouched beyond a view switch. Renders a
// rolling multi-day agenda from the backend `calendar_view` command (recurrence
// already expanded server-side), an event detail pane with RSVP, and a
// create-event modal.
//
// Time zones: the backend returns each event's start/end as a local wall-clock
// string in the user's own IANA zone (sent as `time_zone`), so `new Date(start)`
// parses correctly as local time. All-day events are date-only midnight and are
// never time-shifted.

import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { showConfirm } from "./dialog";

interface Attendee {
  name: string;
  email: string;
  status: string;
  isRequired: boolean;
}
interface CalendarEvent {
  id: string;
  subject: string;
  start: string; // "YYYY-MM-DDTHH:MM:SS" local wall-clock
  end: string;
  startZone: string;
  endZone: string;
  isAllDay: boolean;
  location: string;
  organizerName: string;
  organizerEmail: string;
  attendees: Attendee[];
  bodyHtml: string;
  isCancelled: boolean;
  isRecurring: boolean;
  onlineMeetingUrl: string | null;
  responseStatus: string;
  webLink: string | null;
  isOrganizer: boolean;
}

const RANGE_DAYS = 7;
const IANA_ZONE = (() => {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
  } catch {
    return "UTC";
  }
})();

// ---- Module state ----
let host: HTMLDivElement;
let agendaEl: HTMLDivElement;
let detailEl: HTMLDivElement;
let rangeLabel: HTMLSpanElement;
// First day of the visible window, at local midnight.
let rangeStart = startOfToday();
let events: CalendarEvent[] = [];
let selectedId: string | null = null;
let loadSeq = 0; // guards against out-of-order responses on rapid nav

// Create-event modal refs (built once, appended to <body>).
let eventOverlay: HTMLDivElement;
let evSubject: HTMLInputElement;
let evAllDay: HTMLInputElement;
let evStartDate: HTMLInputElement;
let evStartTime: HTMLInputElement;
let evEndDate: HTMLInputElement;
let evEndTime: HTMLInputElement;
let evLocation: HTMLInputElement;
let evAttendees: HTMLInputElement;
let evBody: HTMLTextAreaElement;
let evMsg: HTMLDivElement;
let evSave: HTMLButtonElement;
// The two time <input>s only — hidden in all-day mode. NOT their .settings-row,
// which also wraps the date pickers (those must stay visible for all-day events).
let evTimeInputs: HTMLInputElement[] = [];

// ---- Helpers ----
function esc(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}
function pad(n: number): string {
  return String(n).padStart(2, "0");
}
function startOfToday(): Date {
  const d = new Date();
  d.setHours(0, 0, 0, 0);
  return d;
}
function addDays(d: Date, n: number): Date {
  const c = new Date(d);
  c.setDate(c.getDate() + n);
  return c;
}
// Add days to a "YYYY-MM-DD" string, returning the same format (local calendar
// math, DST-safe because we never cross a wall-clock hour).
function addDaysToDateStr(dateStr: string, n: number): string {
  const [y, m, day] = dateStr.split("-").map(Number);
  const dt = new Date(y, m - 1, day + n);
  return `${dt.getFullYear()}-${pad(dt.getMonth() + 1)}-${pad(dt.getDate())}`;
}
// Parse an event's local wall-clock string into a Date in local time. Invalid /
// empty strings yield an Invalid Date, which callers guard against.
function parseLocal(s: string): Date {
  return new Date(s);
}
function dayKey(d: Date): string {
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}
function fmtTime(d: Date): string {
  return d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
}
function fmtDayHeading(d: Date): string {
  return d.toLocaleDateString(undefined, {
    weekday: "long",
    day: "numeric",
    month: "long",
  });
}
function fmtRangeLabel(start: Date, endInclusive: Date): string {
  const sameMonth =
    start.getMonth() === endInclusive.getMonth() &&
    start.getFullYear() === endInclusive.getFullYear();
  const s = start.toLocaleDateString(undefined, {
    day: "numeric",
    month: "short",
  });
  const e = endInclusive.toLocaleDateString(undefined, {
    day: "numeric",
    month: "short",
    year: "numeric",
  });
  return sameMonth
    ? `${start.getDate()} – ${e}`
    : `${s} – ${e}`;
}
function statusLabel(s: string): string {
  switch (s) {
    case "accepted":
      return "Accepted";
    case "tentativelyAccepted":
      return "Tentative";
    case "declined":
      return "Declined";
    case "notResponded":
      return "Not responded";
    case "organizer":
      return "Organizer";
    default:
      return "";
  }
}
// Open an external URL in the system browser, but only well-formed http(s) — the
// same gate the message reader applies. Event join/web links and body links come
// from the meeting organizer (untrusted), so a crafted file:/ms-*: scheme must
// never reach the OS shell opener.
function openExternal(url: string | null | undefined): void {
  if (url && /^https?:\/\//i.test(url)) void openUrl(url);
}

function statusClass(s: string): string {
  switch (s) {
    case "accepted":
      return "ev-status-accepted";
    case "tentativelyAccepted":
      return "ev-status-tentative";
    case "declined":
      return "ev-status-declined";
    default:
      return "ev-status-none";
  }
}

// ---- Public API ----
export function initCalendar(hostEl: HTMLDivElement): void {
  host = hostEl;
  host.innerHTML = /* html */ `
    <div class="cal-toolbar">
      <button id="cal-prev" class="btn btn-xs" title="Previous week" aria-label="Previous week">&#8249;</button>
      <button id="cal-today" class="btn btn-xs" title="Jump to today">Today</button>
      <button id="cal-next" class="btn btn-xs" title="Next week" aria-label="Next week">&#8250;</button>
      <span id="cal-range" class="cal-range-label"></span>
      <span class="cal-spacer"></span>
      <button id="cal-refresh" class="btn btn-xs" title="Refresh">&#8635;</button>
      <button id="cal-new" class="btn btn-xs btn-primary" title="Create event">&#43; New event</button>
    </div>
    <div class="cal-main">
      <div id="cal-agenda" class="cal-agenda scroll-thin"></div>
      <div id="cal-detail" class="cal-detail scroll-thin"></div>
    </div>`;

  agendaEl = host.querySelector<HTMLDivElement>("#cal-agenda")!;
  detailEl = host.querySelector<HTMLDivElement>("#cal-detail")!;
  rangeLabel = host.querySelector<HTMLSpanElement>("#cal-range")!;
  // Delegated so row clicks survive every agenda re-render.
  agendaEl.addEventListener("click", onAgendaClick);

  host.querySelector<HTMLButtonElement>("#cal-prev")!.addEventListener("click", () => {
    rangeStart = addDays(rangeStart, -RANGE_DAYS);
    void loadCalendar();
  });
  host.querySelector<HTMLButtonElement>("#cal-next")!.addEventListener("click", () => {
    rangeStart = addDays(rangeStart, RANGE_DAYS);
    void loadCalendar();
  });
  host.querySelector<HTMLButtonElement>("#cal-today")!.addEventListener("click", () => {
    rangeStart = startOfToday();
    void loadCalendar();
  });
  host.querySelector<HTMLButtonElement>("#cal-refresh")!.addEventListener("click", () => {
    void loadCalendar();
  });
  host.querySelector<HTMLButtonElement>("#cal-new")!.addEventListener("click", () => {
    openEventModal();
  });

  resetDetail();
  buildEventModal();
}

// Load (or reload) the current range from the backend and render it.
export async function loadCalendar(): Promise<void> {
  const seq = ++loadSeq;
  const start = new Date(rangeStart);
  const end = addDays(start, RANGE_DAYS); // exclusive
  rangeLabel.textContent = fmtRangeLabel(start, addDays(end, -1));
  agendaEl.innerHTML = `<div class="cal-empty">Loading…</div>`;
  try {
    const result = await invoke<CalendarEvent[]>("calendar_view", {
      // Absolute instants bounding the window. Graph's calendarView interprets
      // an offset-LESS value as UTC and does NOT apply the Prefer:outlook.timezone
      // header to the bounds, so we must send true instants (Z). `start`/`end` are
      // real local-midnight Dates, so toISOString() yields the correct UTC instants
      // (DST-safe — the Date carries the right offset). The Prefer header still
      // renders the *returned* events in the user's zone.
      start: start.toISOString(),
      end: end.toISOString(),
      timeZone: IANA_ZONE,
    });
    if (seq !== loadSeq) return; // a newer load superseded this one
    events = result;
    renderAgenda(start);
    // Keep the open event selected if it's still present; else clear detail.
    if (selectedId && events.some((e) => e.id === selectedId)) {
      selectEvent(selectedId);
    } else {
      selectedId = null;
      resetDetail();
    }
  } catch (e) {
    if (seq !== loadSeq) return;
    agendaEl.innerHTML = `<div class="cal-empty cal-error">Could not load calendar: ${esc(
      String(e),
    )}</div>`;
  }
}

// ---- Agenda rendering ----
function renderAgenda(start: Date): void {
  // Bucket events by their start day (local). Multi-day events appear under their
  // start day only — adequate for an agenda MVP.
  const byDay = new Map<string, CalendarEvent[]>();
  for (const ev of events) {
    const d = parseLocal(ev.start);
    if (isNaN(d.getTime())) continue;
    const key = dayKey(d);
    const list = byDay.get(key) ?? [];
    list.push(ev);
    byDay.set(key, list);
  }

  let html = "";
  for (let i = 0; i < RANGE_DAYS; i++) {
    const day = addDays(start, i);
    const key = dayKey(day);
    const dayEvents = (byDay.get(key) ?? []).slice().sort(sortEvents);
    const isToday = key === dayKey(startOfToday());
    html += `<div class="cal-day-head${isToday ? " is-today" : ""}">${esc(
      fmtDayHeading(day),
    )}${isToday ? `<span class="cal-today-badge">Today</span>` : ""}</div>`;
    if (dayEvents.length === 0) {
      html += `<div class="cal-noevents">No events</div>`;
    } else {
      html += dayEvents.map(eventRowHtml).join("");
    }
  }
  agendaEl.innerHTML = html;
}

function sortEvents(a: CalendarEvent, b: CalendarEvent): number {
  // All-day first, then by start time.
  if (a.isAllDay !== b.isAllDay) return a.isAllDay ? -1 : 1;
  return a.start.localeCompare(b.start);
}

function eventRowHtml(ev: CalendarEvent): string {
  const selected = ev.id === selectedId ? " selected" : "";
  const cancelled = ev.isCancelled ? " cancelled" : "";
  const time = ev.isAllDay ? "All day" : fmtTime(parseLocal(ev.start));
  const recur = ev.isRecurring
    ? `<span class="cal-ev-recur" title="Part of a recurring series">&#8635;</span>`
    : "";
  const online = ev.onlineMeetingUrl
    ? `<span class="cal-ev-online" title="Online meeting">&#128247;</span>`
    : "";
  const loc = ev.location
    ? `<span class="cal-ev-loc">${esc(ev.location)}</span>`
    : "";
  return /* html */ `
    <div class="cal-event${selected}${cancelled}" data-id="${esc(ev.id)}" role="button" tabindex="0">
      <span class="cal-ev-time">${esc(time)}</span>
      <span class="cal-ev-body">
        <span class="cal-ev-subject">${recur}${online}${esc(ev.subject)}</span>
        ${loc}
      </span>
    </div>`;
}

// Event row clicks (delegated, so re-renders don't drop listeners).
function onAgendaClick(e: Event): void {
  const row = (e.target as HTMLElement).closest<HTMLElement>(".cal-event");
  if (!row) return;
  const id = row.dataset.id;
  if (id) selectEvent(id);
}

// ---- Detail pane ----
function resetDetail(): void {
  detailEl.innerHTML = `<div class="cal-detail-empty">Select an event to see its details</div>`;
}

function selectEvent(id: string): void {
  const ev = events.find((e) => e.id === id);
  if (!ev) return;
  selectedId = id;
  // Update selection highlight without a full agenda re-render.
  agendaEl.querySelectorAll(".cal-event.selected").forEach((el) => el.classList.remove("selected"));
  agendaEl
    .querySelector<HTMLElement>(`.cal-event[data-id="${cssEscape(id)}"]`)
    ?.classList.add("selected");
  renderDetail(ev);
}

function cssEscape(s: string): string {
  // Minimal attribute-selector escaping for opaque ids (may contain quotes, etc.).
  if (typeof CSS !== "undefined" && CSS.escape) return CSS.escape(s);
  return s.replace(/["\\\]]/g, "\\$&");
}

function fmtWhen(ev: CalendarEvent): string {
  if (ev.isAllDay) {
    const start = parseLocal(ev.start);
    // All-day end is exclusive next-midnight; show the inclusive last day.
    const endExclusive = parseLocal(ev.end);
    const lastDay = addDays(endExclusive, -1);
    const startStr = start.toLocaleDateString(undefined, {
      weekday: "short",
      day: "numeric",
      month: "short",
      year: "numeric",
    });
    if (dayKey(lastDay) !== dayKey(start) && !isNaN(lastDay.getTime())) {
      const endStr = lastDay.toLocaleDateString(undefined, {
        weekday: "short",
        day: "numeric",
        month: "short",
        year: "numeric",
      });
      return `All day · ${startStr} – ${endStr}`;
    }
    return `All day · ${startStr}`;
  }
  const start = parseLocal(ev.start);
  const end = parseLocal(ev.end);
  const dateStr = start.toLocaleDateString(undefined, {
    weekday: "short",
    day: "numeric",
    month: "short",
    year: "numeric",
  });
  const sameDay = dayKey(start) === dayKey(end);
  if (sameDay) return `${dateStr} · ${fmtTime(start)} – ${fmtTime(end)}`;
  const endStr = end.toLocaleDateString(undefined, {
    weekday: "short",
    day: "numeric",
    month: "short",
  });
  return `${dateStr} ${fmtTime(start)} – ${endStr} ${fmtTime(end)}`;
}

function renderDetail(ev: CalendarEvent): void {
  const recur = ev.isRecurring
    ? `<span class="cal-detail-recur" title="Recurring">&#8635; Recurring</span>`
    : "";
  const cancelled = ev.isCancelled
    ? `<span class="cal-detail-cancelled">Cancelled</span>`
    : "";
  const ownStatus = statusLabel(ev.responseStatus);
  const ownBadge =
    ownStatus && !ev.isOrganizer
      ? `<span class="ev-badge ${statusClass(ev.responseStatus)}">${esc(ownStatus)}</span>`
      : "";

  const rows: string[] = [];
  rows.push(`<div class="cal-detail-when">${esc(fmtWhen(ev))}</div>`);
  if (ev.location) {
    rows.push(
      `<div class="cal-detail-row"><span class="cal-detail-key">Location</span><span>${esc(
        ev.location,
      )}</span></div>`,
    );
  }
  if (ev.organizerName || ev.organizerEmail) {
    const org = ev.organizerName
      ? `${esc(ev.organizerName)}${ev.organizerEmail ? ` &lt;${esc(ev.organizerEmail)}&gt;` : ""}`
      : esc(ev.organizerEmail);
    rows.push(
      `<div class="cal-detail-row"><span class="cal-detail-key">Organizer</span><span>${org}</span></div>`,
    );
  }

  // Action buttons: RSVP for invitees, Delete for organizers. "Can I respond" is
  // the user's own responseStatus being a respondable value — not merely "the
  // event has attendees" (which would offer RSVP on shared/delegate calendars
  // where the user isn't actually an invitee).
  const canRespond =
    !ev.isOrganizer &&
    ["accepted", "tentativelyAccepted", "declined", "notResponded"].includes(
      ev.responseStatus,
    );
  let actions = "";
  if (canRespond) {
    actions = /* html */ `
      <div class="cal-rsvp">
        <button class="btn btn-xs" data-rsvp="accept">Accept</button>
        <button class="btn btn-xs" data-rsvp="tentative">Tentative</button>
        <button class="btn btn-xs" data-rsvp="decline">Decline</button>
      </div>`;
  } else if (ev.isOrganizer) {
    actions = `<div class="cal-rsvp"><button class="btn btn-xs btn-error" data-cal-delete="1">Delete event</button></div>`;
  }

  const join = ev.onlineMeetingUrl
    ? `<button class="btn btn-xs btn-primary cal-join" data-join="1">&#128247; Join online</button>`
    : "";
  const openWeb = ev.webLink
    ? `<button class="btn btn-xs cal-openweb" data-openweb="1">Open in Outlook</button>`
    : "";

  const attendees =
    ev.attendees.length > 0
      ? `<div class="cal-detail-section"><div class="cal-detail-key">Attendees (${ev.attendees.length})</div>
          <div class="cal-attendees">${ev.attendees
            .map((a) => {
              const lbl = statusLabel(a.status);
              const badge = lbl
                ? `<span class="ev-badge ${statusClass(a.status)}">${esc(lbl)}</span>`
                : "";
              const name = esc(a.name || a.email);
              const opt = a.isRequired ? "" : ` <span class="cal-att-opt">(optional)</span>`;
              return `<div class="cal-attendee" title="${esc(a.email)}"><span class="cal-att-name">${name}${opt}</span>${badge}</div>`;
            })
            .join("")}</div></div>`
      : "";

  detailEl.innerHTML = /* html */ `
    <div class="cal-detail-head">
      <div class="cal-detail-title">${esc(ev.subject)}</div>
      <div class="cal-detail-tags">${recur}${cancelled}${ownBadge}</div>
    </div>
    ${rows.join("")}
    <div class="cal-detail-actions">${join}${openWeb}</div>
    ${actions}
    ${attendees}
    <div class="cal-detail-body" id="cal-detail-body"></div>
    <div id="cal-detail-msg" class="settings-msg"></div>`;

  // Body in a sandboxed, script-free iframe (server-sanitized, but isolated for
  // defense in depth — same posture as the mail reader).
  const bodyHost = detailEl.querySelector<HTMLDivElement>("#cal-detail-body")!;
  if (ev.bodyHtml.trim()) renderBodyIframe(bodyHost, ev.bodyHtml);

  // Wire the detail buttons.
  detailEl.querySelector<HTMLButtonElement>("[data-join]")?.addEventListener("click", () => {
    openExternal(ev.onlineMeetingUrl);
  });
  detailEl.querySelector<HTMLButtonElement>("[data-openweb]")?.addEventListener("click", () => {
    openExternal(ev.webLink);
  });
  detailEl.querySelectorAll<HTMLButtonElement>("[data-rsvp]").forEach((btn) => {
    btn.addEventListener("click", () => void respond(ev.id, btn.dataset.rsvp!));
  });
  detailEl.querySelector<HTMLButtonElement>("[data-cal-delete]")?.addEventListener("click", () => {
    void deleteEvent(ev);
  });
}

function renderBodyIframe(container: HTMLElement, html: string): void {
  const iframe = document.createElement("iframe");
  iframe.className = "cal-body-frame";
  // allow-same-origin (so we can auto-size + intercept links) but NOT
  // allow-scripts, so no JS in the body can ever run.
  iframe.setAttribute("sandbox", "allow-same-origin");
  const dark = document.documentElement.dataset.theme !== "corporate";
  const color = dark ? "#d6d6d6" : "#1f2937";
  const linkColor = dark ? "#7cb3ff" : "#1d4ed8";
  iframe.srcdoc = `<!doctype html><html><head><meta charset="utf-8"><base target="_blank"><style>
    html,body{margin:0;padding:0;background:transparent;color:${color};font:13px/1.5 -apple-system,'Segoe UI',system-ui,sans-serif;word-wrap:break-word;overflow-wrap:anywhere}
    a{color:${linkColor}} img{max-width:100%;height:auto} table{max-width:100%}
  </style></head><body>${html}</body></html>`;
  iframe.addEventListener("load", () => {
    try {
      const doc = iframe.contentDocument;
      if (!doc) return;
      const resize = () => {
        iframe.style.height = `${Math.min(doc.body.scrollHeight + 4, 1200)}px`;
      };
      resize();
      // Re-measure as late-loading images / web fonts reflow the body, so the
      // body isn't clipped at the initially-measured (too-short) height.
      if (typeof ResizeObserver !== "undefined") {
        new ResizeObserver(resize).observe(doc.documentElement);
      }
      doc.querySelectorAll<HTMLAnchorElement>("a[href]").forEach((a) => {
        a.addEventListener("click", (e) => {
          e.preventDefault();
          openExternal(a.getAttribute("href"));
        });
      });
    } catch {
      /* cross-origin guard — ignore */
    }
  });
  container.appendChild(iframe);
}

async function respond(id: string, response: string): Promise<void> {
  const msg = detailEl.querySelector<HTMLDivElement>("#cal-detail-msg");
  if (msg) msg.textContent = "Sending response…";
  try {
    await invoke("respond_to_event", {
      id,
      response,
      comment: null,
      sendResponse: true,
    });
    await loadCalendar();
  } catch (e) {
    if (msg) msg.textContent = `Could not send response: ${String(e)}`;
  }
}

async function deleteEvent(ev: CalendarEvent): Promise<void> {
  const ok = await showConfirm(
    `Delete "${ev.subject}"? This cancels the event for all attendees.`,
    { title: "Delete event", okLabel: "Delete", danger: true },
  );
  if (!ok) return;
  const msg = detailEl.querySelector<HTMLDivElement>("#cal-detail-msg");
  if (msg) msg.textContent = "Deleting…";
  try {
    await invoke("delete_event", { id: ev.id });
    selectedId = null;
    await loadCalendar();
  } catch (e) {
    if (msg) msg.textContent = `Could not delete event: ${String(e)}`;
  }
}

// ---- Create-event modal ----
function buildEventModal(): void {
  eventOverlay = document.createElement("div");
  eventOverlay.id = "event-overlay";
  eventOverlay.className = "overlay hidden";
  eventOverlay.innerHTML = /* html */ `
    <div class="settings-panel event-panel">
      <div class="settings-title">New event</div>
      <input id="ev-subject" class="input input-bordered input-sm compose-input" placeholder="Title" autocomplete="off" />
      <label class="settings-row"><span>All day</span><input type="checkbox" id="ev-allday" class="toggle toggle-sm toggle-primary" /></label>
      <label class="settings-row"><span>Start</span><span class="ev-when"><input type="date" id="ev-start-date" class="input input-bordered input-sm" /><input type="time" id="ev-start-time" class="input input-bordered input-sm ev-time" /></span></label>
      <label class="settings-row"><span>End</span><span class="ev-when"><input type="date" id="ev-end-date" class="input input-bordered input-sm" /><input type="time" id="ev-end-time" class="input input-bordered input-sm ev-time" /></span></label>
      <input id="ev-location" class="input input-bordered input-sm compose-input" placeholder="Location" autocomplete="off" />
      <input id="ev-attendees" class="input input-bordered input-sm compose-input" placeholder="Attendees (comma-separated emails)" autocomplete="off" />
      <textarea id="ev-body" class="textarea textarea-bordered ev-body" placeholder="Description"></textarea>
      <div id="ev-msg" class="settings-msg"></div>
      <div class="settings-actions" style="gap: 8px">
        <button id="ev-cancel" class="btn btn-sm">Cancel</button>
        <button id="ev-save" class="btn btn-sm btn-primary">Create event</button>
      </div>
    </div>`;
  document.body.appendChild(eventOverlay);

  evSubject = eventOverlay.querySelector<HTMLInputElement>("#ev-subject")!;
  evAllDay = eventOverlay.querySelector<HTMLInputElement>("#ev-allday")!;
  evStartDate = eventOverlay.querySelector<HTMLInputElement>("#ev-start-date")!;
  evStartTime = eventOverlay.querySelector<HTMLInputElement>("#ev-start-time")!;
  evEndDate = eventOverlay.querySelector<HTMLInputElement>("#ev-end-date")!;
  evEndTime = eventOverlay.querySelector<HTMLInputElement>("#ev-end-time")!;
  evLocation = eventOverlay.querySelector<HTMLInputElement>("#ev-location")!;
  evAttendees = eventOverlay.querySelector<HTMLInputElement>("#ev-attendees")!;
  evBody = eventOverlay.querySelector<HTMLTextAreaElement>("#ev-body")!;
  evMsg = eventOverlay.querySelector<HTMLDivElement>("#ev-msg")!;
  evSave = eventOverlay.querySelector<HTMLButtonElement>("#ev-save")!;
  evTimeInputs = [evStartTime, evEndTime];

  evAllDay.addEventListener("change", reflectAllDay);
  eventOverlay.querySelector<HTMLButtonElement>("#ev-cancel")!.addEventListener("click", closeEventModal);
  evSave.addEventListener("click", () => void saveEvent());
  // Click on the backdrop (not the panel) closes.
  eventOverlay.addEventListener("click", (e) => {
    if (e.target === eventOverlay) closeEventModal();
  });
  // Esc closes (only while the modal is open).
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !eventOverlay.classList.contains("hidden")) closeEventModal();
  });
}

function reflectAllDay(): void {
  const allDay = evAllDay.checked;
  evTimeInputs.forEach((el) => el.classList.toggle("hidden", allDay));
}

function openEventModal(): void {
  evMsg.textContent = "";
  evSubject.value = "";
  evLocation.value = "";
  evAttendees.value = "";
  evBody.value = "";
  evAllDay.checked = false;
  reflectAllDay();

  // Default to the first visible day, next round hour, 1-hour duration.
  const base = new Date(rangeStart);
  const now = new Date();
  if (dayKey(base) === dayKey(startOfToday())) {
    base.setHours(now.getHours() + 1, 0, 0, 0);
  } else {
    base.setHours(9, 0, 0, 0);
  }
  const end = new Date(base);
  end.setHours(base.getHours() + 1);
  evStartDate.value = dayKey(base);
  evStartTime.value = `${pad(base.getHours())}:${pad(base.getMinutes())}`;
  evEndDate.value = dayKey(end);
  evEndTime.value = `${pad(end.getHours())}:${pad(end.getMinutes())}`;

  eventOverlay.classList.remove("hidden");
  evSubject.focus();
}

function closeEventModal(): void {
  eventOverlay.classList.add("hidden");
}

function plainToHtml(text: string): string {
  const trimmed = text.trim();
  if (!trimmed) return "";
  return `<p>${esc(trimmed).replace(/\r?\n/g, "<br>")}</p>`;
}

async function saveEvent(): Promise<void> {
  const subject = evSubject.value.trim();
  if (!subject) {
    evMsg.textContent = "A title is required.";
    return;
  }
  const allDay = evAllDay.checked;
  const sd = evStartDate.value;
  const ed = evEndDate.value;
  if (!sd || !ed) {
    evMsg.textContent = "Start and end dates are required.";
    return;
  }

  let start: string;
  let end: string;
  if (allDay) {
    if (ed < sd) {
      evMsg.textContent = "The end date is before the start date.";
      return;
    }
    start = `${sd}T00:00:00`;
    // Graph all-day end is exclusive — the midnight after the last day.
    end = `${addDaysToDateStr(ed, 1)}T00:00:00`;
  } else {
    const st = evStartTime.value || "09:00";
    const et = evEndTime.value || "10:00";
    start = `${sd}T${st}:00`;
    end = `${ed}T${et}:00`;
    if (parseLocal(end).getTime() <= parseLocal(start).getTime()) {
      evMsg.textContent = "The end must be after the start.";
      return;
    }
  }

  const attendees = evAttendees.value
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s.length > 0);

  evSave.disabled = true;
  evMsg.textContent = "Creating…";
  try {
    const created = await invoke<CalendarEvent>("create_event", {
      event: {
        subject,
        start,
        end,
        isAllDay: allDay,
        location: evLocation.value.trim(),
        bodyHtml: plainToHtml(evBody.value),
        attendees,
      },
      timeZone: IANA_ZONE,
    });
    closeEventModal();
    // Jump the view to the new event's week so it's visible, then select it.
    const startDate = parseLocal(created.start);
    if (!isNaN(startDate.getTime())) {
      rangeStart = startOfDay(startDate);
    }
    selectedId = created.id;
    await loadCalendar();
  } catch (e) {
    evMsg.textContent = `Could not create event: ${String(e)}`;
  } finally {
    evSave.disabled = false;
  }
}

function startOfDay(d: Date): Date {
  const c = new Date(d);
  c.setHours(0, 0, 0, 0);
  return c;
}

// Clear all calendar view state (range window, selection, rendered panes), so one
// mailbox's agenda never persists into another's after sign-out / account switch.
// Safe to call before the module is initialized (the panes are guarded).
export function resetCalendar(): void {
  rangeStart = startOfToday();
  selectedId = null;
  events = [];
  loadSeq++; // cancel any in-flight load
  if (agendaEl) agendaEl.innerHTML = "";
  if (detailEl) resetDetail();
}
