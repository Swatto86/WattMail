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
import { sendNotification } from "@tauri-apps/plugin-notification";
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
  // Minutes before start the user's reminder should fire; null = reminder off.
  reminderMinutes: number | null;
}

const RANGE_DAYS = 7;
// Month view renders a fixed 6-week grid (stable layout across months).
const MONTH_GRID_DAYS = 42;
const MONTH_MAX_PILLS = 3;
type CalView = "agenda" | "month";
const VIEW_KEY = "wattmail.calView";
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
// First day of the visible window, at local midnight. In month view only its
// year/month matter (the grid always starts on the Monday on/before the 1st).
let rangeStart = startOfToday();
let viewMode: CalView = localStorage.getItem(VIEW_KEY) === "month" ? "month" : "agenda";
let viewAgendaBtn: HTMLButtonElement;
let viewMonthBtn: HTMLButtonElement;
let prevBtn: HTMLButtonElement;
let nextBtn: HTMLButtonElement;
let events: CalendarEvent[] = [];
let selectedId: string | null = null;
let loadSeq = 0; // guards against out-of-order responses on rapid nav

// Create-event modal refs (built once, appended to <body>).
let eventOverlay: HTMLDivElement;
let evTitle: HTMLDivElement;
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
// The event being edited (null = the modal is creating a new event). While
// editing, the description textarea holds the body as plain text; if the user
// leaves it untouched we send back the original HTML so a rich body written in
// Outlook isn't flattened by a time-only edit.
let editingId: string | null = null;
let editingBodyText = "";
let editingBodyHtml = "";
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
function monthFirst(d: Date): Date {
  return new Date(d.getFullYear(), d.getMonth(), 1);
}
function addMonths(d: Date, n: number): Date {
  return new Date(d.getFullYear(), d.getMonth() + n, 1);
}
// The Monday on or before `d`, at local midnight (grids are Monday-first).
function mondayOnOrBefore(d: Date): Date {
  const c = new Date(d);
  c.setDate(c.getDate() - ((c.getDay() + 6) % 7));
  c.setHours(0, 0, 0, 0);
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
      <button id="cal-prev" class="btn btn-xs" title="Previous week" aria-label="Previous">&#8249;</button>
      <button id="cal-today" class="btn btn-xs" title="Jump to today">Today</button>
      <button id="cal-next" class="btn btn-xs" title="Next week" aria-label="Next">&#8250;</button>
      <span id="cal-range" class="cal-range-label"></span>
      <span class="cal-spacer"></span>
      <span class="cal-viewswitch">
        <button id="cal-view-agenda" class="btn btn-xs">Agenda</button>
        <button id="cal-view-month" class="btn btn-xs">Month</button>
      </span>
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

  prevBtn = host.querySelector<HTMLButtonElement>("#cal-prev")!;
  nextBtn = host.querySelector<HTMLButtonElement>("#cal-next")!;
  viewAgendaBtn = host.querySelector<HTMLButtonElement>("#cal-view-agenda")!;
  viewMonthBtn = host.querySelector<HTMLButtonElement>("#cal-view-month")!;
  prevBtn.addEventListener("click", () => {
    rangeStart =
      viewMode === "month" ? addMonths(rangeStart, -1) : addDays(rangeStart, -RANGE_DAYS);
    void loadCalendar();
  });
  nextBtn.addEventListener("click", () => {
    rangeStart = viewMode === "month" ? addMonths(rangeStart, 1) : addDays(rangeStart, RANGE_DAYS);
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
  viewAgendaBtn.addEventListener("click", () => setViewMode("agenda"));
  viewMonthBtn.addEventListener("click", () => setViewMode("month"));

  reflectViewMode();
  resetDetail();
  buildEventModal();
}

function setViewMode(mode: CalView): void {
  viewMode = mode;
  try {
    localStorage.setItem(VIEW_KEY, mode);
  } catch {
    /* persistence is best-effort */
  }
  reflectViewMode();
  void loadCalendar();
}

function reflectViewMode(): void {
  const month = viewMode === "month";
  viewAgendaBtn.classList.toggle("btn-active", !month);
  viewMonthBtn.classList.toggle("btn-active", month);
  prevBtn.title = month ? "Previous month" : "Previous week";
  nextBtn.title = month ? "Next month" : "Next week";
  agendaEl.classList.toggle("cal-month-mode", month);
}

// Load (or reload) the current range from the backend and render it.
export async function loadCalendar(): Promise<void> {
  const seq = ++loadSeq;
  let start: Date;
  let end: Date; // exclusive
  if (viewMode === "month") {
    // Fetch the whole 6-week grid so leading/trailing out-of-month cells show
    // their events too; the label names the anchor month.
    start = mondayOnOrBefore(monthFirst(rangeStart));
    end = addDays(start, MONTH_GRID_DAYS);
    rangeLabel.textContent = monthFirst(rangeStart).toLocaleDateString(undefined, {
      month: "long",
      year: "numeric",
    });
  } else {
    start = new Date(rangeStart);
    end = addDays(start, RANGE_DAYS);
    rangeLabel.textContent = fmtRangeLabel(start, addDays(end, -1));
  }
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
    if (viewMode === "month") renderMonth(start);
    else renderAgenda(start);
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
  // Bucket events by their start day (local). An event that started before the
  // visible window (an ongoing multi-day vacation/conference) is clamped to the
  // first window day so it still shows — otherwise its off-window start-day key
  // matches no rendered day and the event silently disappears.
  const windowStartKey = dayKey(start);
  const byDay = new Map<string, CalendarEvent[]>();
  for (const ev of events) {
    const d = parseLocal(ev.start);
    if (isNaN(d.getTime())) continue;
    let key = dayKey(d);
    if (key < windowStartKey) key = windowStartKey;
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

// ---- Month grid rendering ----
function renderMonth(gridStart: Date): void {
  const month = monthFirst(rangeStart).getMonth();
  const byDay = new Map<string, CalendarEvent[]>();
  for (const ev of events) {
    const d = parseLocal(ev.start);
    if (isNaN(d.getTime())) continue;
    const key = dayKey(d);
    const list = byDay.get(key) ?? [];
    list.push(ev);
    byDay.set(key, list);
  }
  const todayKey = dayKey(startOfToday());

  let head = "";
  for (let i = 0; i < 7; i++) {
    head += `<span>${esc(
      addDays(gridStart, i).toLocaleDateString(undefined, { weekday: "short" }),
    )}</span>`;
  }

  let cells = "";
  for (let i = 0; i < MONTH_GRID_DAYS; i++) {
    const day = addDays(gridStart, i);
    const key = dayKey(day);
    const dayEvents = (byDay.get(key) ?? []).slice().sort(sortEvents);
    const pills = dayEvents.slice(0, MONTH_MAX_PILLS).map(monthPillHtml).join("");
    const more =
      dayEvents.length > MONTH_MAX_PILLS
        ? `<button class="cal-more" data-goto-day="${key}">+${dayEvents.length - MONTH_MAX_PILLS} more</button>`
        : "";
    const classes = ["cal-cell"];
    if (day.getMonth() !== month) classes.push("cal-cell-out");
    if (key === todayKey) classes.push("is-today");
    cells += `<div class="${classes.join(" ")}"><button class="cal-cell-day" data-goto-day="${key}" title="Open this day's agenda">${day.getDate()}</button>${pills}${more}</div>`;
  }

  agendaEl.innerHTML = `<div class="cal-month-week">${head}</div><div class="cal-month-grid">${cells}</div>`;
}

// A compact event pill for a month cell. Shares the .cal-event class so the
// delegated click handler and selection highlight work unchanged.
function monthPillHtml(ev: CalendarEvent): string {
  const selected = ev.id === selectedId ? " selected" : "";
  const cancelled = ev.isCancelled ? " cancelled" : "";
  const allday = ev.isAllDay ? " cal-pill-allday" : "";
  const time = ev.isAllDay
    ? ""
    : `<span class="cal-pill-time">${esc(fmtTime(parseLocal(ev.start)))}</span> `;
  return `<div class="cal-event cal-pill${selected}${cancelled}${allday}" data-id="${esc(
    ev.id,
  )}" role="button" tabindex="0" title="${esc(ev.subject)}">${time}${esc(ev.subject)}</div>`;
}

// Event row / pill / day-number clicks (delegated, so re-renders don't drop
// listeners). A day number or "+N more" jumps to that day's agenda.
function onAgendaClick(e: Event): void {
  const goto = (e.target as HTMLElement).closest<HTMLElement>("[data-goto-day]");
  if (goto?.dataset.gotoDay) {
    const [y, m, d] = goto.dataset.gotoDay.split("-").map(Number);
    rangeStart = new Date(y, m - 1, d);
    setViewMode("agenda");
    return;
  }
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
    // Recurrence editing is out of scope: occurrences/exceptions get no Edit
    // (a PATCH on an occurrence id would fork it in surprising ways).
    const edit =
      !ev.isRecurring && !ev.isCancelled
        ? `<button class="btn btn-xs" data-cal-edit="1">Edit</button>`
        : "";
    actions = `<div class="cal-rsvp">${edit}<button class="btn btn-xs btn-error" data-cal-delete="1">Delete event</button></div>`;
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
  detailEl.querySelector<HTMLButtonElement>("[data-cal-edit]")?.addEventListener("click", () => {
    openEventModal(ev);
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

// Disable/enable all RSVP + delete buttons in the detail pane, so a double-click
// (or clicking Accept then Decline) can't fire duplicate Graph requests — each
// RSVP emails the organizer, and a stray delete surfaces a spurious 404.
function setDetailActionsDisabled(disabled: boolean): void {
  detailEl
    .querySelectorAll<HTMLButtonElement>("[data-rsvp],[data-cal-delete]")
    .forEach((b) => (b.disabled = disabled));
}

async function respond(id: string, response: string): Promise<void> {
  const msg = detailEl.querySelector<HTMLDivElement>("#cal-detail-msg");
  if (msg) msg.textContent = "Sending response…";
  setDetailActionsDisabled(true);
  try {
    await invoke("respond_to_event", {
      id,
      response,
      comment: null,
      sendResponse: true,
    });
    await loadCalendar(); // re-renders the detail pane (fresh buttons)
  } catch (e) {
    if (msg) msg.textContent = `Could not send response: ${String(e)}`;
    setDetailActionsDisabled(false); // let the user retry
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
  setDetailActionsDisabled(true);
  try {
    await invoke("delete_event", { id: ev.id });
    selectedId = null;
    await loadCalendar();
  } catch (e) {
    if (msg) msg.textContent = `Could not delete event: ${String(e)}`;
    setDetailActionsDisabled(false);
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

  evTitle = eventOverlay.querySelector<HTMLDivElement>(".settings-title")!;
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

// Extract readable plain text from an HTML body, for the edit textarea. The
// input is the server-sanitized body (no scripts), parsed inert via DOMParser.
function htmlToPlainText(html: string): string {
  const doc = new DOMParser().parseFromString(html, "text/html");
  return (doc.body.textContent ?? "").trim();
}

// Open the modal to create a new event, or — given `ev` — to edit it in place.
function openEventModal(ev?: CalendarEvent): void {
  editingId = ev?.id ?? null;
  evTitle.textContent = ev ? "Edit event" : "New event";
  evSave.textContent = ev ? "Save changes" : "Create event";
  evMsg.textContent = "";
  evSubject.value = ev?.subject ?? "";
  evLocation.value = ev?.location ?? "";
  evAttendees.value = ev ? ev.attendees.map((a) => a.email).join(", ") : "";
  editingBodyHtml = ev?.bodyHtml ?? "";
  editingBodyText = ev ? htmlToPlainText(ev.bodyHtml) : "";
  evBody.value = editingBodyText;
  evAllDay.checked = ev?.isAllDay ?? false;
  reflectAllDay();

  if (ev) {
    const start = parseLocal(ev.start);
    const end = parseLocal(ev.end);
    evStartDate.value = dayKey(start);
    evStartTime.value = `${pad(start.getHours())}:${pad(start.getMinutes())}`;
    if (ev.isAllDay) {
      // The stored all-day end is the exclusive next midnight; the form shows
      // the inclusive last day (save adds the day back).
      evEndDate.value = dayKey(addDays(end, -1));
      evEndTime.value = "10:00";
    } else {
      evEndDate.value = dayKey(end);
      evEndTime.value = `${pad(end.getHours())}:${pad(end.getMinutes())}`;
    }
  } else {
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
  }

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

  // Split on comma OR semicolon, extract the address from "Name <addr>" tokens,
  // and validate — so an Outlook-style paste ("a@x.com; b@y.com" or
  // "Alice <a@x.com>") doesn't silently invite garbage or fail with a raw 400.
  const attendees: string[] = [];
  for (const raw of evAttendees.value.split(/[,;]/)) {
    const token = raw.trim();
    if (!token) continue;
    const m = /<([^>]+)>/.exec(token);
    const addr = (m ? m[1] : token).trim();
    if (!/^\S+@\S+\.\S+$/.test(addr)) {
      evMsg.textContent = `"${token}" is not a valid attendee address.`;
      return;
    }
    attendees.push(addr);
  }

  // An untouched description round-trips the original (sanitized) HTML, so a
  // time-only edit never flattens a rich body to plain text.
  const bodyHtml =
    editingId && evBody.value.trim() === editingBodyText
      ? editingBodyHtml
      : plainToHtml(evBody.value);
  const payload = {
    event: {
      subject,
      start,
      end,
      isAllDay: allDay,
      location: evLocation.value.trim(),
      bodyHtml,
      attendees,
    },
    timeZone: IANA_ZONE,
  };

  evSave.disabled = true;
  evMsg.textContent = editingId ? "Saving…" : "Creating…";
  try {
    const saved = editingId
      ? await invoke<CalendarEvent>("update_event", { id: editingId, ...payload })
      : await invoke<CalendarEvent>("create_event", payload);
    closeEventModal();
    // Jump the view to the event's week so it's visible, then select it.
    const startDate = parseLocal(saved.start);
    if (!isNaN(startDate.getTime())) {
      rangeStart = startOfDay(startDate);
    }
    selectedId = saved.id;
    await loadCalendar();
  } catch (e) {
    evMsg.textContent = `Could not ${editingId ? "save" : "create"} event: ${String(e)}`;
  } finally {
    evSave.disabled = false;
  }
}

function startOfDay(d: Date): Date {
  const c = new Date(d);
  c.setHours(0, 0, 0, 0);
  return c;
}

// ---- Event reminders ----
// A lightweight loop, independent of the calendar tab: every REFRESH interval
// it pulls the next ~26h of events; every TICK it fires a desktop notification
// for any event whose per-event reminder lead (Graph's reminderMinutesBeforeStart,
// as set in Outlook) has arrived. Fired reminders are remembered in localStorage
// so a reload doesn't re-notify. Runs only while the app is alive — WattMail is
// tray-resident by design, so that covers the workday.
const REMINDER_REFRESH_MS = 10 * 60_000;
const REMINDER_TICK_MS = 30_000;
// Don't announce a reminder more than this far past the event start (e.g. the
// app was closed all morning) — a stale "meeting at 09:00" toast at 14:00 is noise.
const REMINDER_STALE_MS = 5 * 60_000;
const REMINDED_KEY = "wattmail.remindedEvents";

let reminderEvents: CalendarEvent[] = [];
let remindersStarted = false;

export function startEventReminders(): void {
  if (remindersStarted) return; // boot can run more than once (sign-out/in)
  remindersStarted = true;
  void refreshReminderEvents();
  setInterval(() => void refreshReminderEvents(), REMINDER_REFRESH_MS);
  setInterval(checkReminders, REMINDER_TICK_MS);
}

async function refreshReminderEvents(): Promise<void> {
  try {
    const supports = await invoke<boolean>("account_supports_calendar");
    const enabled = supports && (await invoke<boolean>("get_notification_setting"));
    if (!enabled) {
      reminderEvents = [];
      return;
    }
    const now = new Date();
    // 26h ahead: catches everything for today+tomorrow-morning. An event whose
    // reminder lead exceeds the horizon fires when its start enters the window
    // (later than configured, never missed).
    const end = new Date(now.getTime() + 26 * 3_600_000);
    reminderEvents = await invoke<CalendarEvent[]>("calendar_view", {
      start: now.toISOString(),
      end: end.toISOString(),
      timeZone: IANA_ZONE,
    });
  } catch {
    /* offline / signed out — keep the previous list and retry next cycle */
  }
}

function checkReminders(): void {
  if (reminderEvents.length === 0) return;
  const now = Date.now();
  const reminded = loadReminded();
  let dirty = false;
  for (const ev of reminderEvents) {
    if (ev.reminderMinutes === null || ev.isCancelled) continue;
    const start = parseLocal(ev.start).getTime();
    if (isNaN(start)) continue;
    const remindAt = start - ev.reminderMinutes * 60_000;
    // Occurrence ids are unique per instance, but key on start too so an event
    // moved to a new time re-reminds at the new time.
    const key = `${ev.id}@${start}`;
    if (now < remindAt || key in reminded) continue;
    reminded[key] = start;
    dirty = true;
    if (now > start + REMINDER_STALE_MS) continue; // mark silently, too late to help
    const when = ev.isAllDay
      ? "All day"
      : new Date(start).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
    const where = ev.location ? ` · ${ev.location}` : "";
    try {
      void sendNotification({ title: "Upcoming event", body: `${ev.subject} · ${when}${where}` });
    } catch {
      /* best-effort */
    }
  }
  if (dirty) saveReminded(reminded);
}

function loadReminded(): Record<string, number> {
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(REMINDED_KEY) ?? "{}");
    return parsed && typeof parsed === "object" ? (parsed as Record<string, number>) : {};
  } catch {
    return {};
  }
}

function saveReminded(map: Record<string, number>): void {
  // Prune entries whose event started over 2 days ago so the store stays small.
  const cutoff = Date.now() - 2 * 86_400_000;
  for (const k of Object.keys(map)) {
    if (typeof map[k] !== "number" || map[k] < cutoff) delete map[k];
  }
  try {
    localStorage.setItem(REMINDED_KEY, JSON.stringify(map));
  } catch {
    /* storage full — reminders degrade to per-session dedup */
  }
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
