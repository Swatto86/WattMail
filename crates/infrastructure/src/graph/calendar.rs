//! Microsoft Graph implementation of the domain [`CalendarProvider`] contract.
//!
//! Lives beside the mail [`GraphClient`](super::GraphClient) impl and shares its
//! HTTP client, bearer token, and helpers. Reads come from `/me/calendarView`,
//! which (unlike `/me/events`) expands recurring series into individual
//! occurrences in the requested window — the only correct source for an agenda.
//!
//! **Time zones:** every request carries `Prefer: outlook.timezone="<tz>"`, so
//! Graph returns each event's `start`/`end` as a local wall-clock string in that
//! zone with no offset. The caller passes the user's own zone, so those strings
//! parse directly as local time. All-day events are reported at date-only
//! midnight and are never zone-shifted.

use async_trait::async_trait;
use serde::Deserialize;

use super::{check_status, GraphClient, GraphEmailAddress, GraphRecipient, GRAPH_BASE};
use wattmail_domain::{
    Attendee, CalendarEvent, CalendarInfo, CalendarProvider, EventDateTime, InviteResponse,
    MailError, MeetingInvite, NewEvent, ResponseStatus,
};

/// The `/me/…` prefix that scopes a calendar request. With no calendar selected
/// this is the mailbox root, so Graph applies its own default calendar.
fn calendar_scope(client: &GraphClient) -> String {
    let Some(id) = client.calendar_id() else {
        return format!("{GRAPH_BASE}/me");
    };
    // Graph calendar ids are long base64-ish strings containing `/` and `+`, so
    // the id is pushed as a path segment and percent-encoded rather than
    // interpolated raw.
    let mut url = url::Url::parse(&format!("{GRAPH_BASE}/me/calendars")).expect("valid base");
    url.path_segments_mut()
        .expect("base URL has a path")
        .push(id);
    url.to_string()
}

/// The calendar list, as Graph reports it.
#[derive(Deserialize)]
struct GraphCalendars {
    value: Vec<GraphCalendar>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphCalendar {
    id: String,
    /// Graph names this `name` — there is no `displayName` on a calendar.
    #[serde(default)]
    name: String,
    color: Option<String>,
    hex_color: Option<String>,
    is_default_calendar: Option<bool>,
    can_edit: Option<bool>,
}

/// Event fields the agenda needs. `body` is fetched so the detail pane can render
/// the description; `onlineMeeting`/`onlineMeetingUrl` give a join link.
const EVENT_SELECT: &str = "id,subject,start,end,isAllDay,isCancelled,isOrganizer,type,location,\
organizer,attendees,body,onlineMeeting,onlineMeetingUrl,webLink,responseStatus,\
isReminderOn,reminderMinutesBeforeStart";

/// Hard cap on `calendarView` pages followed, so a pathological window can never
/// loop unbounded. 20 pages × 100 events = 2000 occurrences, far beyond any sane
/// agenda range.
const MAX_PAGES: u32 = 20;

#[async_trait]
impl CalendarProvider for GraphClient {
    async fn list_calendars(&self) -> Result<Vec<CalendarInfo>, MailError> {
        let response = self
            .http()
            .get(format!("{GRAPH_BASE}/me/calendars"))
            .query(&[
                (
                    "$select",
                    "id,name,color,hexColor,isDefaultCalendar,canEdit",
                ),
                ("$top", "100"),
            ])
            .bearer_auth(self.token())
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;

        let body: GraphCalendars = check_status(response)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        Ok(body
            .value
            .into_iter()
            .map(|c| CalendarInfo {
                id: c.id,
                name: c.name,
                // Prefer the real hex; `color` is a preset name like "lightBlue".
                color: c.hex_color.filter(|h| !h.is_empty()).or(c.color),
                is_default: c.is_default_calendar.unwrap_or(false),
                can_edit: c.can_edit.unwrap_or(true),
            })
            .collect())
    }

    async fn calendar_view(
        &self,
        start: &str,
        end: &str,
        time_zone: &str,
    ) -> Result<Vec<CalendarEvent>, MailError> {
        let tz = sanitize_timezone(time_zone);
        let prefer = format!("outlook.timezone=\"{tz}\", outlook.body-content-type=\"html\"");

        let mut events = Vec::new();
        let mut next: Option<String> = None;
        let mut page = 0u32;

        loop {
            // `start`/`end` must be absolute instants (ISO-8601 with offset/`Z`):
            // Graph interprets an offset-less calendarView bound as UTC and does
            // NOT apply the Prefer header to it, so the caller sends true instants.
            // First page: build from the base URL + query params. Later pages: GET
            // the opaque @odata.nextLink Graph returned (already fully encoded).
            let request = match &next {
                None => self
                    .http()
                    .get(format!("{}/calendarView", calendar_scope(self)))
                    .query(&[
                        ("startDateTime", start),
                        ("endDateTime", end),
                        ("$select", EVENT_SELECT),
                        ("$orderby", "start/dateTime"),
                        ("$top", "100"),
                    ]),
                Some(link) => self.http().get(link),
            };

            let response = request
                .bearer_auth(self.token())
                .header("Prefer", &prefer)
                .send()
                .await
                .map_err(|e| MailError::Network(e.to_string()))?;

            let body: GraphEvents = check_status(response)
                .await?
                .json()
                .await
                .map_err(|e| MailError::Decode(e.to_string()))?;

            for event in body.value {
                events.push(to_domain_event(event, &tz));
            }

            page += 1;
            match body.next_link {
                Some(link) if page < MAX_PAGES => next = Some(link),
                _ => break,
            }
        }

        Ok(events)
    }

    async fn create_event(
        &self,
        event: &NewEvent,
        time_zone: &str,
    ) -> Result<CalendarEvent, MailError> {
        let tz = sanitize_timezone(time_zone);
        let prefer = format!("outlook.timezone=\"{tz}\", outlook.body-content-type=\"html\"");

        let response = self
            .http()
            .post(format!("{}/events", calendar_scope(self)))
            .bearer_auth(self.token())
            .header("Prefer", &prefer)
            .json(&event_payload(event, &tz))
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        let created: GraphEvent = check_status(response)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        Ok(to_domain_event(created, &tz))
    }

    async fn update_event(
        &self,
        id: &str,
        event: &NewEvent,
        time_zone: &str,
    ) -> Result<CalendarEvent, MailError> {
        let tz = sanitize_timezone(time_zone);
        let prefer = format!("outlook.timezone=\"{tz}\", outlook.body-content-type=\"html\"");

        // PATCH /me/events/{id} replaces the supplied fields in place (the
        // attendee list is replaced wholesale); Graph returns the updated
        // event and, for a meeting, sends attendees an update.
        let mut payload = event_payload(event, &tz);
        if let Some(list) = &event.attendees {
            // The edited list carries bare addresses only. Rebuilding entries
            // from scratch would mark everyone "required" with no `status`,
            // which Graph treats as a fresh invite: it re-notifies the whole
            // list and wipes recorded accept/decline responses. Merge with the
            // event's current collection so surviving attendees keep their
            // name, optional/required type, and RSVP status.
            let mut url = event_endpoint(id);
            url.set_query(Some("$select=attendees"));
            let current: serde_json::Value = self
                .get(url.as_str())
                .await?
                .json()
                .await
                .map_err(|e| MailError::Decode(e.to_string()))?;
            payload["attendees"] =
                serde_json::Value::Array(merged_attendees_json(list, &current["attendees"]));
        }
        let response = self
            .http()
            .patch(event_endpoint(id).as_str())
            .bearer_auth(self.token())
            .header("Prefer", &prefer)
            .json(&payload)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        let updated: GraphEvent = check_status(response)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        Ok(to_domain_event(updated, &tz))
    }

    async fn respond_to_event(
        &self,
        id: &str,
        response: InviteResponse,
        comment: Option<&str>,
        send_response: bool,
    ) -> Result<(), MailError> {
        let action = match response {
            InviteResponse::Accept => "accept",
            InviteResponse::TentativelyAccept => "tentativelyAccept",
            InviteResponse::Decline => "decline",
        };
        let mut url = event_endpoint(id);
        url.path_segments_mut()
            .expect("base URL is a proper path")
            .push(action);

        let payload = serde_json::json!({
            "comment": comment.unwrap_or(""),
            "sendResponse": send_response,
        });
        let response = self
            .http()
            .post(url.as_str())
            .bearer_auth(self.token())
            .json(&payload)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        check_status(response).await?;
        Ok(())
    }

    async fn delete_event(&self, id: &str) -> Result<(), MailError> {
        let response = self
            .http()
            .delete(event_endpoint(id).as_str())
            .bearer_auth(self.token())
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        check_status(response).await?;
        Ok(())
    }
}

impl GraphClient {
    /// The meeting invitation carried by a message: `Some` only when the
    /// message is a meeting *request* whose linked calendar event is still
    /// reachable. Backs [`wattmail_domain::MailProvider::meeting_invite`];
    /// lives here beside the event wire types it reuses.
    pub(super) async fn fetch_meeting_invite(
        &self,
        message_id: &str,
        time_zone: &str,
    ) -> Result<Option<MeetingInvite>, MailError> {
        // 1. Classify the message. Graph annotates derived-type instances with
        //    `@odata.type`, so a `$select=id` GET is enough to tell a meeting
        //    request (`eventMessageRequest`) from ordinary mail and from invite
        //    responses/cancellations — without ever `$select`ing a derived-only
        //    property on a base-typed URL (which would 400).
        let mut url = super::message_endpoint(message_id);
        url.set_query(Some("$select=id"));
        let probe: GraphTypedObject = self
            .get(url.as_str())
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        if probe.odata_type.as_deref() != Some("#microsoft.graph.eventMessageRequest") {
            return Ok(None);
        }

        // 2. Follow the eventMessage cast's `event` navigation to the linked
        //    calendar event, rendered in the user's zone. A 404 means the event
        //    is gone (cancelled / declined-and-removed) — not an error, just no
        //    longer respondable.
        let tz = sanitize_timezone(time_zone);
        let prefer = format!("outlook.timezone=\"{tz}\"");
        let mut url = super::message_endpoint(message_id);
        {
            let mut segments = url.path_segments_mut().expect("base URL is a proper path");
            segments.push("microsoft.graph.eventMessage");
            segments.push("event");
        }
        url.set_query(Some(
            "$select=id,start,end,isAllDay,responseStatus,isCancelled",
        ));
        let response = self
            .http()
            .get(url.as_str())
            .bearer_auth(self.token())
            .header("Prefer", &prefer)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let event: GraphEvent = check_status(response)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        // Reuse the full event mapping (all-day midnight snap included) and
        // keep just the fields the RSVP bar needs.
        let domain = to_domain_event(event, &tz);
        // A cancelled meeting isn't respondable — and a cancellation NOTICE
        // may (depending on Graph's typing) pass the eventMessageRequest probe
        // above, so this guard covers both readings of the wire contract.
        if domain.is_cancelled {
            return Ok(None);
        }
        Ok(Some(MeetingInvite {
            event_id: domain.id,
            start: domain.start,
            end: domain.end,
            is_all_day: domain.is_all_day,
            response_status: domain.response_status,
        }))
    }
}

/// A minimal decode of any Graph object, keeping only its OData type
/// annotation (present whenever the instance is of a derived type).
#[derive(Deserialize)]
struct GraphTypedObject {
    #[serde(rename = "@odata.type")]
    odata_type: Option<String>,
}

/// The Graph JSON for an event's editable fields, shared by create (POST) and
/// update (PATCH — Graph replaces exactly the fields present, so both send the
/// same set). `attendees` is omitted entirely when `None`: a PATCH then leaves
/// the existing attendee collection untouched, preserving each attendee's
/// optional/required type and display name (Graph replaces the collection
/// wholesale whenever the key is present).
fn event_payload(event: &NewEvent, tz: &str) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "subject": event.subject,
        "body": { "contentType": "HTML", "content": event.body_html },
        "start": { "dateTime": event.start.date_time, "timeZone": tz },
        "end": { "dateTime": event.end.date_time, "timeZone": tz },
        "isAllDay": event.is_all_day,
        "location": { "displayName": event.location },
    });
    if let Some(list) = &event.attendees {
        let attendees: Vec<serde_json::Value> = list
            .iter()
            .map(|a| a.trim())
            .filter(|a| !a.is_empty())
            .map(|address| {
                serde_json::json!({
                    "emailAddress": { "address": address },
                    "type": "required",
                })
            })
            .collect();
        payload
            .as_object_mut()
            .expect("json! built an object above")
            .insert("attendees".to_string(), serde_json::Value::Array(attendees));
    }
    payload
}

/// Merge an edited attendee address list with the event's current server-side
/// attendee collection (`current` = the GET's `attendees` array). Addresses
/// already attending keep their full existing entry; new addresses join as
/// bare `required` entries. Pure, so the merge is unit-tested.
fn merged_attendees_json(
    new_addresses: &[String],
    current: &serde_json::Value,
) -> Vec<serde_json::Value> {
    let existing = current.as_array().cloned().unwrap_or_default();
    new_addresses
        .iter()
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .map(|address| {
            existing
                .iter()
                .find(|entry| {
                    entry["emailAddress"]["address"]
                        .as_str()
                        .is_some_and(|e| e.eq_ignore_ascii_case(address))
                })
                .cloned()
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "emailAddress": { "address": address },
                        "type": "required",
                    })
                })
        })
        .collect()
}

/// Build the `/me/events/{id}` endpoint with the opaque id encoded as a single
/// path segment (event ids are base64-ish and may contain `/`, `+`, `=`).
fn event_endpoint(id: &str) -> url::Url {
    let mut url = url::Url::parse(&format!("{GRAPH_BASE}/me/events")).expect("valid base");
    url.path_segments_mut()
        .expect("base URL is a proper path")
        .push(id);
    url
}

/// Reduce a caller-supplied IANA/Windows time-zone name to a safe header value,
/// falling back to `UTC` for anything with unexpected characters. Prevents a
/// malformed/hostile zone string from corrupting the `Prefer` header (or failing
/// the request outright). Real zone names — `Europe/London`, `America/New_York`,
/// `GMT Standard Time` — pass through unchanged.
fn sanitize_timezone(tz: &str) -> String {
    let trimmed = tz.trim();
    let ok = !trimmed.is_empty()
        && trimmed.len() <= 64
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+' | ' ' | ':'));
    if ok {
        trimmed.to_string()
    } else {
        "UTC".to_string()
    }
}

/// Truncate a Graph local-datetime to seconds and strip any stray zone marker, so
/// the frontend gets a clean `YYYY-MM-DDTHH:MM:SS` it can parse as local time.
/// Graph returns e.g. `2026-06-24T09:00:00.0000000` (7 fractional digits) which
/// is not standard ISO and parses inconsistently across engines.
fn normalize_local_datetime(s: &str) -> String {
    s.split('.')
        .next()
        .unwrap_or(s)
        .trim_end_matches('Z')
        .to_string()
}

/// Snap an all-day event's (zone-converted) local datetime back to the nearest
/// midnight. Graph applies the `Prefer` zone conversion to all-day events too, so
/// their `T00:00:00` slides by the zone offset (< 12 h). If the hour is < 12 the
/// conversion nudged it forward within the same day → truncate to that day's
/// midnight; if ≥ 12 it slid back across midnight → roll forward to the next day.
fn snap_all_day_to_midnight(dt: &str) -> String {
    let Some((date, time)) = dt.split_once('T') else {
        return dt.to_string();
    };
    let hour: u32 = time.get(0..2).and_then(|h| h.parse().ok()).unwrap_or(0);
    if hour >= 12 {
        format!("{}T00:00:00", next_calendar_day(date))
    } else {
        format!("{date}T00:00:00")
    }
}

/// Increment a `YYYY-MM-DD` date by one day (Gregorian, leap-aware). Returns the
/// input unchanged if it doesn't parse as a date.
fn next_calendar_day(date: &str) -> String {
    let parts: Vec<&str> = date.split('-').collect();
    let (Some(Ok(y)), Some(Ok(m)), Some(Ok(d))) = (
        parts.first().map(|s| s.parse::<i32>()),
        parts.get(1).map(|s| s.parse::<u32>()),
        parts.get(2).map(|s| s.parse::<u32>()),
    ) else {
        return date.to_string();
    };
    let (mut y, mut m, mut d) = (y, m, d);
    d += 1;
    if d > days_in_month(y, m) {
        d = 1;
        m += 1;
        if m > 12 {
            m = 1;
            y += 1;
        }
    }
    format!("{y:04}-{m:02}-{d:02}")
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 31,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Map a Graph event to the domain [`CalendarEvent`], sanitizing its body and
/// resolving the user's own response. `tz` is the requested zone, used only as a
/// fallback when Graph omits a per-field `timeZone`.
fn to_domain_event(event: GraphEvent, tz: &str) -> CalendarEvent {
    let body_raw = event
        .body
        .as_ref()
        .map(|b| b.content.clone().unwrap_or_default());
    let is_html = event
        .body
        .as_ref()
        .and_then(|b| b.content_type.as_deref())
        .map(|t| t.eq_ignore_ascii_case("html"))
        .unwrap_or(false);
    let body_html = body_raw
        .map(|raw| crate::html::sanitize_email(&raw, is_html, false).html)
        .unwrap_or_default();

    let (organizer_name, organizer_email) = match event.organizer {
        Some(GraphRecipient { email_address }) => (
            email_address.name.unwrap_or_default(),
            email_address.address.unwrap_or_default(),
        ),
        None => (String::new(), String::new()),
    };

    let attendees = event
        .attendees
        .into_iter()
        .filter_map(|a| {
            let email = a.email_address.as_ref().and_then(|e| e.address.clone());
            let email = email?;
            let name = a
                .email_address
                .and_then(|e| e.name)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| email.clone());
            // Anything not explicitly optional/resource is treated as required.
            let is_required = !matches!(
                a.attendee_type.as_deref(),
                Some("optional") | Some("resource")
            );
            Some(Attendee {
                name,
                email,
                status: ResponseStatus::parse(a.status.and_then(|s| s.response).as_deref()),
                is_required,
            })
        })
        .collect();

    // calendarView yields singleInstance / occurrence / exception (never a
    // seriesMaster); the latter two are slices of a recurring series.
    let is_recurring = matches!(
        event.event_type.as_deref(),
        Some("occurrence") | Some("exception")
    );

    let online_meeting_url = http_url(
        event
            .online_meeting
            .and_then(|m| m.join_url)
            .or(event.online_meeting_url),
    );

    let is_all_day = event.is_all_day.unwrap_or(false);
    let mut start = to_domain_datetime(event.start, tz);
    let mut end = to_domain_datetime(event.end, tz);
    if is_all_day {
        // `Prefer: outlook.timezone` converts EVERY returned dateTime, including
        // all-day events, so an all-day event created in another zone comes back
        // shifted off midnight (e.g. `...T19:00:00`). Snap back to the nearest
        // midnight so day-bucketing and range math land on the right day.
        start.date_time = snap_all_day_to_midnight(&start.date_time);
        end.date_time = snap_all_day_to_midnight(&end.date_time);
    }

    CalendarEvent {
        id: event.id,
        subject: event.subject.unwrap_or_else(|| "(no subject)".to_string()),
        start,
        end,
        is_all_day,
        location: event
            .location
            .and_then(|l| l.display_name)
            .unwrap_or_default(),
        organizer_name,
        organizer_email,
        attendees,
        body_html,
        is_cancelled: event.is_cancelled.unwrap_or(false),
        is_recurring,
        online_meeting_url,
        response_status: ResponseStatus::parse(
            event.response_status.and_then(|s| s.response).as_deref(),
        ),
        web_link: http_url(event.web_link),
        is_organizer: event.is_organizer.unwrap_or(false),
        // Graph implements the invitation reply path for every event.
        can_respond: true,
        // A reminder exists only when explicitly on; a missing lead defaults to
        // Outlook's usual 15 minutes, and negative/absurd values are clamped.
        reminder_minutes_before_start: match event.is_reminder_on {
            Some(true) => Some(
                event
                    .reminder_minutes_before_start
                    .unwrap_or(15)
                    .clamp(0, 40_320) as u32, // ≤ 4 weeks
            ),
            _ => None,
        },
    }
}

/// Keep a URL only if it is a non-empty `http`/`https` link. Event join URLs and
/// web links come from the meeting organizer, so this rejects any other scheme
/// (e.g. `file:`, `ms-*:`) server-side before it can reach the system opener —
/// defense in depth alongside the frontend's `openExternal` gate.
fn http_url(url: Option<String>) -> Option<String> {
    url.filter(|u| {
        let u = u.trim();
        u.len() > "https://".len()
            && (u.to_ascii_lowercase().starts_with("https://")
                || u.to_ascii_lowercase().starts_with("http://"))
    })
}

fn to_domain_datetime(dt: Option<GraphDateTime>, tz: &str) -> EventDateTime {
    match dt {
        Some(dt) => EventDateTime {
            date_time: normalize_local_datetime(&dt.date_time.unwrap_or_default()),
            time_zone: dt
                .time_zone
                .filter(|z| !z.is_empty())
                .unwrap_or_else(|| tz.to_string()),
        },
        None => EventDateTime {
            date_time: String::new(),
            time_zone: tz.to_string(),
        },
    }
}

// ---- Graph wire types ----

#[derive(Deserialize)]
struct GraphEvents {
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
    #[serde(default)]
    value: Vec<GraphEvent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphEvent {
    id: String,
    subject: Option<String>,
    start: Option<GraphDateTime>,
    end: Option<GraphDateTime>,
    is_all_day: Option<bool>,
    is_cancelled: Option<bool>,
    is_organizer: Option<bool>,
    #[serde(rename = "type")]
    event_type: Option<String>,
    location: Option<GraphLocation>,
    organizer: Option<GraphRecipient>,
    #[serde(default)]
    attendees: Vec<GraphAttendee>,
    body: Option<GraphEventBody>,
    online_meeting: Option<GraphOnlineMeeting>,
    online_meeting_url: Option<String>,
    web_link: Option<String>,
    response_status: Option<GraphResponse>,
    is_reminder_on: Option<bool>,
    /// Graph models this as a signed int; negatives are clamped on mapping.
    reminder_minutes_before_start: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphDateTime {
    date_time: Option<String>,
    time_zone: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphLocation {
    display_name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphAttendee {
    #[serde(rename = "type")]
    attendee_type: Option<String>,
    status: Option<GraphResponse>,
    email_address: Option<GraphEmailAddress>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphResponse {
    response: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphOnlineMeeting {
    join_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphEventBody {
    content_type: Option<String>,
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attendee_merge_keeps_existing_entries_and_adds_new_as_required() {
        // Existing collection: an optional attendee with a name and an
        // accepted required one — both must survive an edit untouched.
        let current = serde_json::json!([
            {
                "emailAddress": { "address": "fyi@x.com", "name": "Manager" },
                "type": "optional",
                "status": { "response": "none", "time": "0001-01-01T00:00:00Z" }
            },
            {
                "emailAddress": { "address": "dev@x.com", "name": "Dev" },
                "type": "required",
                "status": { "response": "accepted", "time": "2026-07-01T09:00:00Z" }
            }
        ]);
        // Edit keeps both (one with different case), drops nobody, adds one.
        let merged = merged_attendees_json(
            &["FYI@x.com".into(), "dev@x.com".into(), " new@x.com ".into()],
            &current,
        );

        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0]["type"], "optional");
        assert_eq!(merged[0]["emailAddress"]["name"], "Manager");
        assert_eq!(merged[1]["status"]["response"], "accepted");
        assert_eq!(merged[2]["emailAddress"]["address"], "new@x.com");
        assert_eq!(merged[2]["type"], "required");
        assert!(merged[2].get("status").is_none());
    }

    #[test]
    fn normalize_strips_fractional_seconds_and_zone_marker() {
        assert_eq!(
            normalize_local_datetime("2026-06-24T09:00:00.0000000"),
            "2026-06-24T09:00:00"
        );
        assert_eq!(
            normalize_local_datetime("2026-06-24T09:00:00"),
            "2026-06-24T09:00:00"
        );
        assert_eq!(
            normalize_local_datetime("2026-06-24T09:00:00Z"),
            "2026-06-24T09:00:00"
        );
    }

    #[test]
    fn all_day_snap_rolls_forward_when_shifted_back_across_midnight() {
        // London all-day "July 10" viewed from New York → 2026-07-09T19:00:00.
        assert_eq!(
            snap_all_day_to_midnight("2026-07-09T19:00:00"),
            "2026-07-10T00:00:00"
        );
    }

    #[test]
    fn all_day_snap_truncates_when_shifted_forward_within_day() {
        // LA all-day event viewed from London → 2026-07-10T08:00:00.
        assert_eq!(
            snap_all_day_to_midnight("2026-07-10T08:00:00"),
            "2026-07-10T00:00:00"
        );
    }

    #[test]
    fn all_day_snap_leaves_midnight_unchanged() {
        assert_eq!(
            snap_all_day_to_midnight("2026-07-10T00:00:00"),
            "2026-07-10T00:00:00"
        );
    }

    #[test]
    fn next_calendar_day_handles_month_and_year_boundaries() {
        assert_eq!(next_calendar_day("2026-07-10"), "2026-07-11");
        assert_eq!(next_calendar_day("2026-07-31"), "2026-08-01");
        assert_eq!(next_calendar_day("2026-12-31"), "2027-01-01");
        // Leap year: Feb has 29 days in 2028.
        assert_eq!(next_calendar_day("2028-02-28"), "2028-02-29");
        assert_eq!(next_calendar_day("2028-02-29"), "2028-03-01");
        assert_eq!(next_calendar_day("2027-02-28"), "2027-03-01");
    }

    #[test]
    fn sanitize_timezone_passes_real_zones_and_rejects_garbage() {
        assert_eq!(sanitize_timezone("Europe/London"), "Europe/London");
        assert_eq!(sanitize_timezone("America/New_York"), "America/New_York");
        assert_eq!(sanitize_timezone("GMT Standard Time"), "GMT Standard Time");
        // A quote would break the Prefer header — rejected to UTC.
        assert_eq!(sanitize_timezone("Foo\"; rm -rf"), "UTC");
        assert_eq!(sanitize_timezone(""), "UTC");
    }

    #[test]
    fn http_url_keeps_only_web_links() {
        assert_eq!(
            http_url(Some("https://teams.example/join".to_string())).as_deref(),
            Some("https://teams.example/join")
        );
        assert_eq!(
            http_url(Some("http://x.example/y".to_string())).as_deref(),
            Some("http://x.example/y")
        );
        // Non-web schemes that would otherwise reach the OS opener are rejected.
        assert_eq!(http_url(Some("file:///etc/passwd".to_string())), None);
        assert_eq!(http_url(Some("ms-msdt:/id".to_string())), None);
        assert_eq!(http_url(Some(String::new())), None);
        assert_eq!(http_url(None), None);
    }

    #[test]
    fn typed_object_probe_decodes_the_odata_type_annotation() {
        // A meeting request probed with $select=id: the derived type shows up
        // as an annotation; a plain message carries no annotation at all.
        let invite: GraphTypedObject = serde_json::from_str(
            r##"{"@odata.type":"#microsoft.graph.eventMessageRequest","@odata.etag":"W/\"x\"","id":"AAA"}"##,
        )
        .expect("parses");
        assert_eq!(
            invite.odata_type.as_deref(),
            Some("#microsoft.graph.eventMessageRequest")
        );

        let plain: GraphTypedObject = serde_json::from_str(r#"{"id":"BBB"}"#).expect("parses");
        assert!(plain.odata_type.is_none());
    }

    #[test]
    fn event_payload_carries_zoned_times_and_required_attendees() {
        let event = NewEvent {
            subject: "Standup".into(),
            start: EventDateTime {
                date_time: "2026-07-13T09:00:00".into(),
                time_zone: "Europe/London".into(),
            },
            end: EventDateTime {
                date_time: "2026-07-13T09:15:00".into(),
                time_zone: "Europe/London".into(),
            },
            is_all_day: false,
            location: "Room 2".into(),
            body_html: "<p>Notes</p>".into(),
            attendees: Some(vec!["a@x.io".into(), " ".into(), "b@y.io".into()]),
        };
        let json = event_payload(&event, "Europe/London");
        assert_eq!(json["subject"], "Standup");
        assert_eq!(json["start"]["dateTime"], "2026-07-13T09:00:00");
        assert_eq!(json["start"]["timeZone"], "Europe/London");
        assert_eq!(json["isAllDay"], false);
        assert_eq!(json["location"]["displayName"], "Room 2");
        // Blank attendee tokens are dropped; the rest are required invitees.
        let attendees = json["attendees"].as_array().expect("array");
        assert_eq!(attendees.len(), 2);
        assert_eq!(attendees[0]["emailAddress"]["address"], "a@x.io");
        assert_eq!(attendees[0]["type"], "required");

        // No attendee list at all → the key is OMITTED, so a PATCH leaves the
        // server's attendee collection (types, display names) untouched.
        let untouched = NewEvent {
            attendees: None,
            ..event
        };
        let json = event_payload(&untouched, "Europe/London");
        assert!(json.get("attendees").is_none());
    }

    #[test]
    fn event_endpoint_encodes_opaque_id_as_one_segment() {
        let url = event_endpoint("AB/cd+ef=");
        assert!(url
            .as_str()
            .starts_with(&format!("{GRAPH_BASE}/me/events/")));
        assert!(url.as_str().contains("%2F")); // '/' encoded, not a path split
    }

    #[test]
    fn event_decode_maps_recurrence_online_meeting_and_response() {
        let json = r#"{
            "id": "AAA",
            "subject": "Sprint review",
            "type": "occurrence",
            "isOrganizer": false,
            "isAllDay": false,
            "start": { "dateTime": "2026-06-24T09:00:00.0000000", "timeZone": "Europe/London" },
            "end": { "dateTime": "2026-06-24T09:30:00.0000000", "timeZone": "Europe/London" },
            "location": { "displayName": "Room 1" },
            "organizer": { "emailAddress": { "name": "Boss", "address": "boss@x.io" } },
            "attendees": [
                { "type": "required", "status": { "response": "accepted" },
                  "emailAddress": { "name": "Me", "address": "me@x.io" } },
                { "type": "optional", "status": { "response": "none" },
                  "emailAddress": { "address": "opt@x.io" } }
            ],
            "onlineMeeting": { "joinUrl": "https://teams.example/join" },
            "responseStatus": { "response": "tentativelyAccepted" },
            "body": { "contentType": "html", "content": "<p>Hello</p><script>x()</script>" }
        }"#;
        let event: GraphEvent = serde_json::from_str(json).expect("parses");
        let domain = to_domain_event(event, "Europe/London");

        assert_eq!(domain.subject, "Sprint review");
        assert!(domain.is_recurring);
        assert!(!domain.is_organizer);
        assert_eq!(domain.start.date_time, "2026-06-24T09:00:00");
        assert_eq!(domain.location, "Room 1");
        assert_eq!(domain.organizer_email, "boss@x.io");
        assert_eq!(domain.attendees.len(), 2);
        assert_eq!(domain.attendees[0].status, ResponseStatus::Accepted);
        assert!(domain.attendees[0].is_required);
        assert!(!domain.attendees[1].is_required); // optional
        assert_eq!(domain.response_status, ResponseStatus::TentativelyAccepted);
        assert_eq!(
            domain.online_meeting_url.as_deref(),
            Some("https://teams.example/join")
        );
        // Body sanitized: the <script> must be gone.
        assert!(!domain.body_html.contains("<script"));
        assert!(domain.body_html.contains("Hello"));
    }

    #[test]
    fn all_day_event_keeps_date_only_midnight() {
        let json = r#"{
            "id": "B",
            "subject": "Holiday",
            "type": "singleInstance",
            "isAllDay": true,
            "start": { "dateTime": "2026-06-24T00:00:00.0000000", "timeZone": "Europe/London" },
            "end": { "dateTime": "2026-06-25T00:00:00.0000000", "timeZone": "Europe/London" }
        }"#;
        let event: GraphEvent = serde_json::from_str(json).expect("parses");
        let domain = to_domain_event(event, "Europe/London");
        assert!(domain.is_all_day);
        assert!(!domain.is_recurring);
        assert_eq!(domain.start.date_time, "2026-06-24T00:00:00");
    }
}
