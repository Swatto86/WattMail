// WattMail calendar view — a self-contained module that owns all calendar DOM,
// so the mail UI in main.ts stays untouched beyond a view switch. Renders a
// rolling multi-day agenda from the backend `calendar_view` command (recurrence
// already expanded server-side), an event detail pane with RSVP, and a
// create-event modal.
//
// Time zones: Graph converts timezones server-side and hands back each event's
// start/end as a wall-clock string already in the viewer's own IANA zone, so
// parsing it is a no-op conversion. CalDAV (iCloud) can't do that server-side —
// the Rust tree carries no timezone database — so it passes each event's own
// wall clock through together with whatever zone iCalendar stated for it
// (startZone/endZone), which may differ from the viewer's zone; the frontend
// resolves that using the browser's own IANA data. All-day events are
// date-only and are never run through zone maths.

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
  // Whether this provider can actually send an RSVP for this event.
  canRespond: boolean;
  // Minutes before start the user's reminder should fire; null = reminder off.
  reminderMinutes: number | null;
}
interface CalendarInfo {
  id: string;
  name: string;
  color: string | null;
  isDefault: boolean;
  canEdit: boolean;
}

const RANGE_DAYS = 7;
// Month view renders a fixed 6-week grid (stable layout across months).
const MONTH_GRID_DAYS = 42;
const MONTH_MAX_PILLS = 3;
type CalView = "agenda" | "month";
const VIEW_KEY = "wattmail.calView";
export const IANA_ZONE = (() => {
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
let calendarsSeq = 0; // ditto for the calendar list across account switches
let calPicker: HTMLSelectElement;
let calendars: CalendarInfo[] = [];
let selectedCalendarId: string | null = null;
// Set whenever `loadCalendars` runs (account activation), so the refresh
// button and the picker's own change handler know which account's selection
// to persist without main.ts having to thread it through every call.
let activeAccountId: string | null = null;
// The active account's provider slug. iCloud (CalDAV) has no server-side zone
// conversion, so its writes are converted to UTC here before they are sent.
let activeProviderSlug = "";

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
let evReminder: HTMLSelectElement;
let evMsg: HTMLDivElement;
let evSave: HTMLButtonElement;
// The event being edited (null = the modal is creating a new event). While
// editing, the description textarea holds the body as plain text; if the user
// leaves it untouched we send back the original HTML so a rich body written in
// Outlook isn't flattened by a time-only edit.
let editingId: string | null = null;
let editingBodyText = "";
let editingBodyHtml = "";
// The attendee field as prefilled: if the user leaves it untouched, the save
// sends attendees=null so the server keeps the existing collection (with each
// attendee's optional/required type and display name intact).
let editingAttendeesBaseline = "";
// The reminder as prefilled: the exact original minutes and the snapped select
// value at open. If the user never touches Alert, the exact original is sent
// back unchanged, so editing a title never silently rounds a 10-minute reminder
// down to the nearest preset.
let editingReminderMinutes: number | null = null;
let editingReminderBaseline = "";
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
// Windows zone IDs an Outlook-authored event can carry into iCloud's iCalendar
// (TZID is free text, and Windows names are what Outlook writes) — mapped to
// their IANA equivalent so Intl can resolve them. Not exhaustive, just the
// common ones; anything else falls back to floating time below.
const WINDOWS_ZONE_MAP: Record<string, string> = {
  "GMT Standard Time": "Europe/London",
  GMT: "Etc/GMT",
  "W. Europe Standard Time": "Europe/Berlin",
  "Central Europe Standard Time": "Europe/Budapest",
  "Romance Standard Time": "Europe/Paris",
  "Eastern Standard Time": "America/New_York",
  "Central Standard Time": "America/Chicago",
  "Mountain Standard Time": "America/Denver",
  "Pacific Standard Time": "America/Los_Angeles",
  UTC: "UTC",
  "AUS Eastern Standard Time": "Australia/Sydney",
  "Tokyo Standard Time": "Asia/Tokyo",
  "India Standard Time": "Asia/Kolkata",
  "China Standard Time": "Asia/Shanghai",
  "Singapore Standard Time": "Asia/Singapore",
};

function buildZoneFormatter(timeZone: string): Intl.DateTimeFormat {
  // en-US + h23 so formatToParts hands back plain ASCII digits in 24h form.
  return new Intl.DateTimeFormat("en-US", {
    timeZone,
    hourCycle: "h23",
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

// Memoized per zone — render loops call this twice per event — and the
// failure case is cached too, so an unresolvable zone (e.g. a Windows name
// with no table entry) isn't retried on every event that carries it.
const zoneFormatters = new Map<string, Intl.DateTimeFormat | null>();
function zoneFormatter(timeZone: string): Intl.DateTimeFormat | null {
  const cached = zoneFormatters.get(timeZone);
  if (cached !== undefined) return cached;
  let dtf: Intl.DateTimeFormat | null = null;
  try {
    dtf = buildZoneFormatter(timeZone);
  } catch {
    // Intl throws RangeError for anything that isn't a valid IANA name —
    // try the Windows-name mapping before giving up on the zone entirely.
    const mapped = WINDOWS_ZONE_MAP[timeZone];
    if (mapped) {
      try {
        dtf = buildZoneFormatter(mapped);
      } catch {
        dtf = null;
      }
    }
  }
  zoneFormatters.set(timeZone, dtf);
  return dtf;
}

function getOffsetMs(ms: number, timeZone: string): number {
  // Offset (ms, east-positive) of `timeZone` at instant `ms`: read back the
  // wall clock that instant shows in the zone, reinterpret those digits as UTC,
  // and diff. The browser's own IANA data does the work.
  const dtf = zoneFormatter(timeZone);
  if (!dtf) return 0;
  const p: Record<string, string> = {};
  for (const part of dtf.formatToParts(new Date(ms))) p[part.type] = part.value;
  const asUtc = Date.UTC(+p.year, +p.month - 1, +p.day, +p.hour, +p.minute, +p.second);
  return asUtc - ms;
}

function zonedWallClockToInstant(wallClock: string, timeZone: string): Date {
  // A wall clock already expressed in the viewer's own zone is what `new Date`
  // parses natively, so take that path verbatim rather than round-tripping it
  // through the offset maths below. Graph always sends this case, and the
  // round trip is not quite an identity: inside the repeated hour of an autumn
  // DST changeover the two disagree by an hour, which would silently move a
  // once-a-year meeting — and its reminder — for existing users.
  if (!timeZone || timeZone === IANA_ZONE) return new Date(wallClock);
  const naiveUtcMs = Date.parse(wallClock + "Z");
  if (Number.isNaN(naiveUtcMs)) return new Date(NaN);
  // Two passes: the offset at the naive guess is wrong right across a DST
  // boundary, so it is re-read at the candidate instant and applied again.
  const first = getOffsetMs(naiveUtcMs, timeZone);
  let instant = naiveUtcMs - first;
  const second = getOffsetMs(instant, timeZone);
  if (second !== first) instant = naiveUtcMs - second;
  return new Date(instant);
}

// Parse an event's wall-clock string into a Date. Graph events already carry
// the viewer's own zone, so this is a straight conversion for them; iCloud
// events resolve against whatever zone they came tagged with. All-day dates
// have no zone to resolve — running one through zone maths is exactly what
// slides it onto the wrong day. Invalid/empty strings yield an Invalid Date,
// which callers guard against.
function parseLocal(s: string, zone: string, allDay: boolean): Date {
  if (!allDay) return zonedWallClockToInstant(s, zone);
  // The date part is built explicitly rather than handed to `new Date(s)`:
  // ECMA-262 parses a bare "YYYY-MM-DD" as UTC midnight but "YYYY-MM-DDTHH:MM:SS"
  // as *local* midnight, and every reader below uses local getters. Graph happens
  // to send the second shape, CalDAV sends the first, and left to that asymmetry
  // an all-day event slides a day earlier everywhere west of Greenwich.
  const parts = s.slice(0, 10).split("-").map(Number);
  if (parts.length !== 3 || parts.some((n) => !Number.isFinite(n))) {
    return new Date(NaN);
  }
  return new Date(parts[0], parts[1] - 1, parts[2]);
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
      <select id="cal-picker" class="select select-bordered select-xs" title="Calendar" aria-label="Calendar" hidden></select>
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
  calPicker = host.querySelector<HTMLSelectElement>("#cal-picker")!;
  // Delegated so row clicks survive every agenda re-render.
  agendaEl.addEventListener("click", onAgendaClick);
  calPicker.addEventListener("change", () => {
    selectedCalendarId = calPicker.value || null;
    if (activeAccountId) {
      try {
        localStorage.setItem(calendarStorageKey(activeAccountId), selectedCalendarId ?? "");
      } catch {
        /* persistence is best-effort */
      }
    }
    void loadCalendar();
  });
  // `loadCalendars` may already have run (account activation happens on sign-in,
  // before the calendar tab is ever opened), so reflect whatever it fetched
  // instead of starting the picker from a blank state.
  renderCalPicker();

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
    void refreshCalendars();
  });
  host.querySelector<HTMLButtonElement>("#cal-new")!.addEventListener("click", () => {
    // A read-only calendar (a subscribed feed, or any iCloud calendar while the
    // backend is read-only) would only fail on save — don't open the form.
    if (!selectedCalendarCanEdit()) return;
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

// ---- Calendar picker ----
function calendarStorageKey(accountId: string): string {
  return `wattmail.selectedCalendar.${accountId}`;
}

function renderCalPicker(): void {
  if (!calPicker) return; // initCalendar hasn't built the toolbar yet
  calPicker.innerHTML = calendars
    .map(
      (c) =>
        `<option value="${esc(c.id)}"${c.id === selectedCalendarId ? " selected" : ""}>${esc(
          c.name,
        )}</option>`,
    )
    .join("");
  calPicker.value = selectedCalendarId ?? "";
  // A Graph user with exactly one calendar must see no new UI at all.
  calPicker.hidden = calendars.length <= 1;
  const newBtn = host?.querySelector<HTMLButtonElement>("#cal-new");
  if (newBtn) newBtn.disabled = !selectedCalendarCanEdit();
}

// Whether the calendar in view accepts new events. A subscribed feed never
// does, and neither does any iCloud calendar while that backend is read-only.
// Unknown (no list loaded) counts as editable, which is how it behaved before
// there was a picker.
function selectedCalendarCanEdit(): boolean {
  const selected = calendars.find((c) => c.id === selectedCalendarId);
  return selected ? selected.canEdit : calendars.length === 0;
}

// Fetch the account's calendar list and reconcile the persisted choice against
// it. Called once per account activation (main.ts) and from the refresh
// button below — NOT on prev/next/today, which would otherwise hit the
// backend for a list that essentially never changes mid-session.
export async function loadCalendars(accountId: string, providerSlug = ""): Promise<void> {
  activeProviderSlug = providerSlug;
  // The backend resolves `list_calendars` against whatever account is active
  // when it runs, so a slow reply for the account we just left would otherwise
  // overwrite the new one's list — and send its calendar id to the wrong
  // mailbox on the next view.
  const seq = ++calendarsSeq;
  activeAccountId = accountId;
  let fetched: CalendarInfo[];
  try {
    fetched = await invoke<CalendarInfo[]>("list_calendars");
  } catch {
    // No multi-calendar support (or a transient failure): picker stays
    // hidden and calendar_view runs with calendarId: null, i.e. the
    // backend's own default calendar — the agenda still loads.
    fetched = [];
  }
  if (seq !== calendarsSeq) return; // a newer account switch superseded this
  calendars = fetched;
  let stored: string | null = null;
  try {
    stored = localStorage.getItem(calendarStorageKey(accountId));
  } catch {
    stored = null;
  }
  // A stored id the server no longer has (calendar deleted, or a stale value
  // from before a reconciliation existed) must not be sent as-is — the
  // backend fails the whole view rather than silently falling back.
  const chosen =
    calendars.find((c) => c.id === stored) ?? calendars.find((c) => c.isDefault) ?? calendars[0];
  selectedCalendarId = chosen?.id ?? null;
  renderCalPicker();
}

async function refreshCalendars(): Promise<void> {
  if (activeAccountId) await loadCalendars(activeAccountId, activeProviderSlug);
  await loadCalendar();
}

// True when `ev` really overlaps [windowStart, windowEnd) — an overlap test,
// not "does it start inside the window": a genuinely-ongoing multi-day event
// (started before the window, still running) must survive this, since the
// render-side clamp exists precisely to show those on day one. All-day dates
// are exempt; they have no zone to run through instant maths in the first place.
function overlapsWindow(ev: CalendarEvent, windowStart: Date, windowEnd: Date): boolean {
  if (ev.isAllDay) return true;
  const s = parseLocal(ev.start, ev.startZone, ev.isAllDay).getTime();
  const e = parseLocal(ev.end, ev.endZone, ev.isAllDay).getTime();
  return s < windowEnd.getTime() && e > windowStart.getTime();
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
      calendarId: selectedCalendarId,
    });
    if (seq !== loadSeq) return; // a newer load superseded this one
    // iCloud over-fetches by up to a day at each edge (it can't compare a
    // zoned wall clock against a UTC instant without a tz database); drop
    // what doesn't really overlap so it doesn't pile onto day one's clamp.
    // No-op against Graph, which never over-fetches.
    events = result.filter((ev) => overlapsWindow(ev, start, end));
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
// The local day keys an event covers, clamped to [windowStartKey, windowEndKey].
//
// A multi-day event must appear on EVERY day it spans, not just its start — so a
// three-day trip fills all three cells. All-day DTEND is the exclusive next
// midnight, so the last day it touches is one before it; a timed event ending
// exactly at midnight likewise doesn't reach into the next day.
function coveredDayKeys(
  ev: CalendarEvent,
  windowStartKey: string,
  windowEndKey: string,
): string[] {
  const start = parseLocal(ev.start, ev.startZone, ev.isAllDay);
  if (isNaN(start.getTime())) return [];
  let end = parseLocal(ev.end, ev.endZone, ev.isAllDay);
  if (isNaN(end.getTime())) end = start;
  let last = end;
  if (ev.isAllDay) {
    last = addDays(end, -1);
  } else if (
    end.getTime() > start.getTime() &&
    end.getHours() === 0 &&
    end.getMinutes() === 0 &&
    end.getSeconds() === 0
  ) {
    last = addDays(end, -1);
  }

  const lastKey = dayKey(last);
  const keys: string[] = [];
  // Start the walk at the window, not the event's true start: a long event (a
  // multi-year assignment) that began well before the window would otherwise
  // spend the whole iteration budget crossing the pre-window gap and reach no
  // visible day at all. The window is at most MONTH_GRID_DAYS wide, so the guard
  // now only has to cover the window itself.
  let day = startOfDay(start);
  if (dayKey(day) < windowStartKey) {
    const [y, m, d] = windowStartKey.split("-").map(Number);
    day = new Date(y, m - 1, d);
  }
  for (let guard = 0; guard < 400; guard++) {
    const key = dayKey(day);
    if (key > windowEndKey) break;
    if (key >= windowStartKey && key <= lastKey) keys.push(key);
    if (key >= lastKey) break;
    day = addDays(day, 1);
  }
  return keys;
}

function renderAgenda(start: Date): void {
  // Each event is bucketed onto every visible day it covers (see coveredDayKeys)
  // — so an ongoing multi-day event shows on each of its days rather than only
  // its start, and one that began before the window still appears on the days it
  // reaches into.
  const windowStartKey = dayKey(start);
  const windowEndKey = dayKey(addDays(start, RANGE_DAYS - 1));
  const byDay = new Map<string, CalendarEvent[]>();
  for (const ev of events) {
    for (const key of coveredDayKeys(ev, windowStartKey, windowEndKey)) {
      const list = byDay.get(key) ?? [];
      list.push(ev);
      byDay.set(key, list);
    }
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
  // All-day first, then by start time. Wall-clock strings can carry different
  // zones now (iCloud), so they're compared as parsed instants, not strings.
  if (a.isAllDay !== b.isAllDay) return a.isAllDay ? -1 : 1;
  return (
    parseLocal(a.start, a.startZone, a.isAllDay).getTime() -
    parseLocal(b.start, b.startZone, b.isAllDay).getTime()
  );
}

function eventRowHtml(ev: CalendarEvent): string {
  const selected = ev.id === selectedId ? " selected" : "";
  const cancelled = ev.isCancelled ? " cancelled" : "";
  const time = ev.isAllDay ? "All day" : fmtTime(parseLocal(ev.start, ev.startZone, ev.isAllDay));
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
  // Each event fills every grid cell it covers (see coveredDayKeys), so a
  // multi-day event spans its days instead of showing only on the first.
  const gridStartKey = dayKey(gridStart);
  const gridEndKey = dayKey(addDays(gridStart, MONTH_GRID_DAYS - 1));
  const byDay = new Map<string, CalendarEvent[]>();
  for (const ev of events) {
    for (const key of coveredDayKeys(ev, gridStartKey, gridEndKey)) {
      const list = byDay.get(key) ?? [];
      list.push(ev);
      byDay.set(key, list);
    }
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
    : `<span class="cal-pill-time">${esc(fmtTime(parseLocal(ev.start, ev.startZone, ev.isAllDay)))}</span> `;
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
  // Update selection highlight without a full agenda re-render. A multi-day
  // event renders one node per covered day, so highlight ALL of them (a full
  // render already does, via the per-node `selected` ternary) — not just the
  // first, which would light up the wrong day.
  agendaEl.querySelectorAll(".cal-event.selected").forEach((el) => el.classList.remove("selected"));
  agendaEl
    .querySelectorAll<HTMLElement>(`.cal-event[data-id="${cssEscape(id)}"]`)
    .forEach((el) => el.classList.add("selected"));
  renderDetail(ev);
}

function cssEscape(s: string): string {
  // Minimal attribute-selector escaping for opaque ids (may contain quotes, etc.).
  if (typeof CSS !== "undefined" && CSS.escape) return CSS.escape(s);
  return s.replace(/["\\\]]/g, "\\$&");
}

function fmtWhen(ev: CalendarEvent): string {
  if (ev.isAllDay) {
    const start = parseLocal(ev.start, ev.startZone, ev.isAllDay);
    // All-day end is exclusive next-midnight; show the inclusive last day.
    const endExclusive = parseLocal(ev.end, ev.endZone, ev.isAllDay);
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
  const start = parseLocal(ev.start, ev.startZone, ev.isAllDay);
  const end = parseLocal(ev.end, ev.endZone, ev.isAllDay);
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
    ev.canRespond &&
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

// Resolve the detail pane's surface + text colours to concrete rgb, so the
// body iframe (which has no DaisyUI tokens of its own) can match them. A
// throwaway probe lets the browser evaluate the oklch(var(--…)) expressions.
function paneColors(): { bg: string; fg: string; link: string } {
  const probe = document.createElement("span");
  probe.style.cssText =
    "color:oklch(var(--bc));background:oklch(var(--b1));position:absolute;left:-9999px";
  document.body.appendChild(probe);
  const cs = getComputedStyle(probe);
  const dark = document.documentElement.dataset.theme !== "corporate";
  const colors = {
    fg: cs.color || (dark ? "#d6d6d6" : "#1f2937"),
    bg: cs.backgroundColor || (dark ? "#1d232a" : "#ffffff"),
    link: dark ? "#7cb3ff" : "#1d4ed8",
  };
  probe.remove();
  return colors;
}

function renderBodyIframe(container: HTMLElement, html: string): void {
  const iframe = document.createElement("iframe");
  iframe.className = "cal-body-frame";
  // allow-same-origin (so we can auto-size + intercept links) but NOT
  // allow-scripts, so no JS in the body can ever run.
  iframe.setAttribute("sandbox", "allow-same-origin");
  // Set the background EXPLICITLY, not `transparent`: a sandboxed srcdoc iframe
  // is painted on an opaque white layer by WebView2, so a transparent body left
  // the light-on-dark description text unreadable (grey on white). The pane's
  // own DaisyUI tokens (resolved to concrete rgb here — the iframe has no access
  // to them) make it sit seamlessly on the detail surface in either theme.
  const pane = paneColors();
  iframe.srcdoc = `<!doctype html><html><head><meta charset="utf-8"><base target="_blank"><style>
    html,body{margin:0;padding:0;background:${pane.bg};color:${pane.fg};font:13px/1.5 -apple-system,'Segoe UI',system-ui,sans-serif;word-wrap:break-word;overflow-wrap:anywhere}
    a{color:${pane.link}} img{max-width:100%;height:auto} table{max-width:100%}
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

// A CalDAV write conflict (the event changed on another device since it was
// loaded) comes back as a 409/412 provider error — say something the user can
// act on rather than dumping the raw status line.
function conflictMessage(e: unknown, prefix: string): string {
  const text = String(e);
  if (/\((?:409|412)\)/.test(text)) {
    return "This event changed on another device. Reload the calendar and try again.";
  }
  return `${prefix}: ${text}`;
}

async function deleteEvent(ev: CalendarEvent): Promise<void> {
  // A recurring event's id addresses one occurrence, so deleting it removes
  // just that occurrence (the backend writes an EXDATE) — say so plainly.
  const prompt = ev.isRecurring
    ? `Delete this occurrence of "${ev.subject}"? The rest of the series stays.`
    : `Delete "${ev.subject}"? This cancels the event for all attendees.`;
  const ok = await showConfirm(prompt, {
    title: "Delete event",
    okLabel: "Delete",
    danger: true,
  });
  if (!ok) return;
  const msg = detailEl.querySelector<HTMLDivElement>("#cal-detail-msg");
  if (msg) msg.textContent = "Deleting…";
  setDetailActionsDisabled(true);
  try {
    await invoke("delete_event", { id: ev.id });
    selectedId = null;
    await loadCalendar();
  } catch (e) {
    if (msg) msg.textContent = conflictMessage(e, "Could not delete event");
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
      <label class="settings-row"><span>Alert</span><select id="ev-reminder" class="select select-bordered select-sm">
        <option value="">None</option>
        <option value="0">At time of event</option>
        <option value="5">5 minutes before</option>
        <option value="15">15 minutes before</option>
        <option value="30">30 minutes before</option>
        <option value="60">1 hour before</option>
        <option value="120">2 hours before</option>
        <option value="1440">1 day before</option>
        <option value="2880">2 days before</option>
        <option value="10080">1 week before</option>
      </select></label>
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
  evReminder = eventOverlay.querySelector<HTMLSelectElement>("#ev-reminder")!;
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
// Block boundaries get a newline first so lists/rows/paragraphs don't run
// together as one long line in the textarea.
function htmlToPlainText(html: string): string {
  const withBreaks = html.replace(/<(?:\/(?:p|div|li|ul|ol|tr|table|h[1-6])|br\s*\/?)>/gi, "$&\n");
  const doc = new DOMParser().parseFromString(withBreaks, "text/html");
  return (doc.body.textContent ?? "")
    .replace(/[ \t]+\n/g, "\n")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
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
  editingAttendeesBaseline = evAttendees.value;
  editingBodyHtml = ev?.bodyHtml ?? "";
  editingBodyText = ev ? htmlToPlainText(ev.bodyHtml) : "";
  evBody.value = editingBodyText;
  // Reminders are one of the exact iPhone presets, or the closest below it.
  editingReminderMinutes = ev?.reminderMinutes ?? null;
  evReminder.value = ev && ev.reminderMinutes !== null ? nearestReminderPreset(ev.reminderMinutes) : "";
  editingReminderBaseline = evReminder.value;
  evAllDay.checked = ev?.isAllDay ?? false;
  reflectAllDay();

  if (ev) {
    const start = parseLocal(ev.start, ev.startZone, ev.isAllDay);
    const end = parseLocal(ev.end, ev.endZone, ev.isAllDay);
    evStartDate.value = dayKey(start);
    if (ev.isAllDay) {
      // The stored all-day end is the exclusive next midnight; the form shows
      // the inclusive last day (save adds the day back). The hidden time
      // fields get sensible defaults — an all-day event's own times are both
      // midnight, which would turn into a 00:00 start if "All day" is
      // unchecked during the edit.
      evStartTime.value = "09:00";
      evEndTime.value = "10:00";
      evEndDate.value = dayKey(addDays(end, -1));
    } else {
      evStartTime.value = `${pad(start.getHours())}:${pad(start.getMinutes())}`;
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

// A local wall-clock string -> the same instant written as a UTC wall clock
// ("YYYY-MM-DDTHH:MM:SS"), for a provider that stores times in UTC.
function toUtcWallClock(localWall: string): string {
  const d = zonedWallClockToInstant(localWall, IANA_ZONE);
  return (
    `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())}` +
    `T${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}:${pad(d.getUTCSeconds())}`
  );
}

// The reminder <select> only offers a fixed set of leads; an event authored
// elsewhere may use any minute count, so snap it to the closest option at or
// below it (never inventing an earlier alert than the user set).
const REMINDER_PRESETS = [0, 5, 15, 30, 60, 120, 1440, 2880, 10080];
function nearestReminderPreset(minutes: number): string {
  let best = 0;
  for (const p of REMINDER_PRESETS) {
    if (p <= minutes) best = p;
  }
  return String(best);
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
    // These are the modal's own date/time inputs, always implicitly in the
    // viewer's zone — never an event's own startZone/endZone.
    if (
      parseLocal(end, IANA_ZONE, allDay).getTime() <= parseLocal(start, IANA_ZONE, allDay).getTime()
    ) {
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
  // An untouched attendee field sends null: the server then keeps its existing
  // attendee collection — optional/required types and display names included —
  // instead of a wholesale replace that would flatten everyone to "required".
  const attendeesTouched = !editingId || evAttendees.value !== editingAttendeesBaseline;
  const reminderRaw = evReminder.value;
  // Untouched on an edit → send the exact original (unsnapped) value.
  const reminderMinutes =
    editingId && reminderRaw === editingReminderBaseline
      ? editingReminderMinutes
      : reminderRaw === ""
        ? null
        : Number(reminderRaw);

  // iCloud (CalDAV) has no server-side zone conversion and the Rust side has no
  // timezone database, so timed events are converted to a UTC instant here — the
  // browser's IANA data does it. All-day dates carry no zone and are left alone.
  let sendZone = IANA_ZONE;
  if (activeProviderSlug === "icloud" && !allDay) {
    start = toUtcWallClock(start);
    end = toUtcWallClock(end);
    sendZone = "UTC";
  }
  const payload = {
    event: {
      subject,
      start,
      end,
      isAllDay: allDay,
      location: evLocation.value.trim(),
      bodyHtml,
      attendees: attendeesTouched ? attendees : null,
      reminderMinutes,
    },
    timeZone: sendZone,
  };

  evSave.disabled = true;
  evMsg.textContent = editingId ? "Saving…" : "Creating…";
  try {
    const saved = editingId
      ? await invoke<CalendarEvent>("update_event", { id: editingId, ...payload })
      : await invoke<CalendarEvent>("create_event", { ...payload, calendarId: selectedCalendarId });
    closeEventModal();
    // Jump the view to the event's week so it's visible, then select it.
    const startDate = parseLocal(saved.start, saved.startZone, saved.isAllDay);
    if (!isNaN(startDate.getTime())) {
      rangeStart = startOfDay(startDate);
    }
    selectedId = saved.id;
    await loadCalendar();
  } catch (e) {
    evMsg.textContent = conflictMessage(e, `Could not ${editingId ? "save" : "create"} event`);
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
// Discards out-of-order refresh completions: an in-flight fetch for the
// previous account must not overwrite the new account's list (mirrors loadSeq).
let reminderSeq = 0;

export function startEventReminders(): void {
  // Always refresh right away: a sign-in or account SWITCH must not keep
  // serving the previous mailbox's reminders until the next 10-min cycle.
  void refreshReminderEvents();
  if (remindersStarted) return; // the intervals themselves are app-lifetime
  remindersStarted = true;
  setInterval(() => void refreshReminderEvents(), REMINDER_REFRESH_MS);
  setInterval(checkReminders, REMINDER_TICK_MS);
}

async function refreshReminderEvents(): Promise<void> {
  const seq = ++reminderSeq;
  try {
    const supports = await invoke<boolean>("account_supports_calendar");
    const enabled = supports && (await invoke<boolean>("get_notification_setting"));
    if (!enabled) {
      if (seq === reminderSeq) reminderEvents = [];
      return;
    }
    const now = new Date();
    // 26h ahead: catches everything for today+tomorrow-morning. An event whose
    // reminder lead exceeds the horizon fires when its start enters the window
    // (later than configured, never missed).
    const end = new Date(now.getTime() + 26 * 3_600_000);
    const fetched = await invoke<CalendarEvent[]>("calendar_view", {
      start: now.toISOString(),
      end: end.toISOString(),
      timeZone: IANA_ZONE,
      calendarId: selectedCalendarId,
    });
    // Commit only if no newer refresh (or account switch) superseded this one.
    if (seq === reminderSeq) reminderEvents = fetched;
  } catch {
    /* offline / signed out — keep the previous list and retry next cycle */
  }
}

// Called by the settings toggle: switching notifications off must silence
// already-fetched reminders immediately (not on the next 10-min cycle), and
// switching them on picks upcoming events straight up.
export function notificationSettingChanged(): void {
  void refreshReminderEvents();
}

function checkReminders(): void {
  if (reminderEvents.length === 0) return;
  const now = Date.now();
  const reminded = loadReminded();
  let dirty = false;
  for (const ev of reminderEvents) {
    if (ev.reminderMinutes === null || ev.isCancelled) continue;
    const start = parseLocal(ev.start, ev.startZone, ev.isAllDay).getTime();
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
  // The picker is per-account too — the old account's calendar list and
  // choice must never leak into the new one's toolbar.
  calendars = [];
  selectedCalendarId = null;
  activeAccountId = null;
  renderCalPicker();
  // The old account's reminders must stop immediately too (the switch path
  // also calls startEventReminders, which refetches for the new account), and
  // any of its fetches still in flight must land dead.
  reminderEvents = [];
  reminderSeq++;
  loadSeq++; // cancel any in-flight load
  if (agendaEl) agendaEl.innerHTML = "";
  if (detailEl) resetDetail();
}
