//! iCloud implementation of the domain [`CalendarProvider`] contract, over
//! CalDAV (RFC 4791) with HTTP Basic auth.
//!
//! **Auth:** Apple offers no OAuth for CalDAV. The credential is the Apple ID
//! plus an app-specific password generated at appleid.apple.com; the real
//! account password does not work once two-factor auth is on, which it is for
//! every modern Apple Account.
//!
//! **Discovery:** iCloud partitions each account onto its own `pNN-caldav`
//! host, reached by a redirect from the generic well-known URL. Redirects are
//! followed *manually* here — see [`IcloudClient::dav`] for why an automatic
//! follow silently breaks authentication.
//!
//! **Time zones:** nothing is converted. Each event keeps the wall clock and
//! zone iCalendar stated, and the frontend resolves it against the browser's
//! IANA database. See [`super::civil`].
//!
//! **Milestone 1 is read-only.** Creating, editing, deleting and RSVP return
//! [`MailError::Unsupported`], and events are reported with `is_organizer:
//! false` so the UI offers no edit affordance that cannot yet work.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use reqwest::Method;
use url::Url;
use wattmail_domain::{
    Attendee, CalendarEvent, CalendarInfo, CalendarProvider, EventDateTime, InviteResponse,
    MailError, NewEvent, ResponseStatus,
};

use super::{civil, dav, ical, rrule};

/// Discovery always starts here, never at a partition host: a `pNN` host that
/// has been reassigned answers the well-known URL with a 401 that looks like a
/// credential failure (vdirsyncer #855).
const WELL_KNOWN: &str = "https://caldav.icloud.com/.well-known/caldav";

/// Redirect hops followed before giving up. Discovery needs one; the cap only
/// exists so a misconfigured server cannot loop us.
const MAX_REDIRECTS: u8 = 5;

/// Days added at each edge of the server-side time-range filter.
///
/// Occurrences are compared in their own wall clock, which can sit up to 14
/// hours either side of the UTC instant it represents, so a one-day margin
/// guarantees nothing visible is filtered out server-side. The frontend, which
/// has the timezone database, does the exact filtering.
const WINDOW_SLOP_DAYS: i64 = 1;

/// Ceiling on events returned from one view, mirroring the Graph backend's page
/// cap. Far beyond any real agenda window.
const MAX_EVENTS: usize = 2_000;

/// A CalDAV calendar backend for one iCloud account.
pub struct IcloudClient {
    http: reqwest::Client,
    apple_id: String,
    app_password: String,
    /// Absolute URL of the calendar collection to read. `None` falls back to the
    /// account's first calendar, which needs a discovery round trip first.
    calendar_id: Option<String>,
}

impl IcloudClient {
    pub fn new(apple_id: impl Into<String>, app_password: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                // Redirects are handled by hand; see `dav`.
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(std::time::Duration::from_secs(15))
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("reqwest client"),
            apple_id: apple_id.into(),
            app_password: app_password.into(),
            calendar_id: None,
        }
    }

    /// Scope this client to one calendar collection (an absolute CalDAV URL).
    pub fn with_calendar(mut self, calendar_id: Option<String>) -> Self {
        self.calendar_id = calendar_id.filter(|id| !id.is_empty());
        self
    }

    /// Issue a WebDAV request, following redirects manually.
    ///
    /// reqwest's default redirect policy strips `Authorization` whenever the
    /// target host changes — and iCloud's discovery *always* moves to a
    /// per-account partition host. Letting reqwest follow it therefore produces
    /// an unauthenticated request and a 401 indistinguishable from a wrong
    /// password. Each hop is reissued here with the credentials re-attached.
    ///
    /// Returns the URL that actually answered, so relative hrefs in the body can
    /// be resolved against the right host.
    async fn dav(
        &self,
        method: Method,
        url: Url,
        depth: &str,
        body: &'static str,
    ) -> Result<(Url, String), MailError> {
        self.dav_body(method, url, depth, body.to_string()).await
    }

    async fn dav_body(
        &self,
        method: Method,
        url: Url,
        depth: &str,
        body: String,
    ) -> Result<(Url, String), MailError> {
        let mut url = url;
        for _ in 0..MAX_REDIRECTS {
            let response = self
                .http
                .request(method.clone(), url.clone())
                .basic_auth(&self.apple_id, Some(&self.app_password))
                .header("Depth", depth)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/xml; charset=utf-8",
                )
                .body(body.clone())
                .send()
                .await
                .map_err(|e| MailError::Network(self.scrub(&e.to_string())))?;

            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| {
                        MailError::Decode("iCloud redirected without a Location header".into())
                    })?
                    .to_string();
                url = url
                    .join(&location)
                    .map_err(|e| MailError::Decode(format!("bad redirect target: {e}")))?;
                // Following a redirect re-attaches the app-specific password, so
                // the target has to stay inside Apple's own domain over TLS.
                // reqwest's default policy drops credentials across origins; that
                // policy is disabled here (it is what breaks iCloud discovery),
                // so this check is what stands in for it.
                if !is_icloud_host(&url) {
                    return Err(MailError::Decode(
                        "iCloud redirected outside icloud.com - refusing to send credentials there"
                            .into(),
                    ));
                }
                continue;
            }

            if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                return Err(MailError::NotAuthenticated);
            }
            if !response.status().is_success() {
                let status = response.status().as_u16();
                let message = response.text().await.unwrap_or_default();
                return Err(MailError::Api {
                    status,
                    message: self.scrub(&message),
                });
            }

            let text = response
                .text()
                .await
                .map_err(|e| MailError::Network(self.scrub(&e.to_string())))?;
            return Ok((url, text));
        }
        Err(MailError::Network(
            "iCloud redirected too many times during discovery".into(),
        ))
    }

    /// Remove the app-specific password from anything that might be surfaced or
    /// logged. Nothing puts it in a URL, but an error string is not a place to
    /// discover otherwise.
    fn scrub(&self, message: &str) -> String {
        if self.app_password.is_empty() {
            return message.to_string();
        }
        message.replace(&self.app_password, "***")
    }

    /// Walk well-known → principal → calendar home.
    async fn calendar_home(&self) -> Result<Url, MailError> {
        let start = Url::parse(WELL_KNOWN).expect("valid well-known URL");
        let (base, body) = self
            .dav(propfind(), start, "0", dav::PROPFIND_CURRENT_USER_PRINCIPAL)
            .await?;
        let principal = dav::parse_multistatus(&body)?
            .into_iter()
            .find_map(|r| r.principal_href)
            .ok_or_else(|| MailError::Decode("iCloud reported no principal URL".into()))?;
        let principal = dav::resolve_href(&base, &principal)?;

        let (base, body) = self
            .dav(propfind(), principal, "0", dav::PROPFIND_CALENDAR_HOME)
            .await?;
        let home = dav::parse_multistatus(&body)?
            .into_iter()
            .find_map(|r| r.home_href)
            .ok_or_else(|| MailError::Decode("iCloud reported no calendar home".into()))?;
        dav::resolve_href(&base, &home)
    }

    /// The collection this view targets: the selected calendar, or the first one
    /// discovered when nothing has been picked yet.
    async fn target_calendar(&self) -> Result<Url, MailError> {
        if let Some(id) = &self.calendar_id {
            return Url::parse(id)
                .map_err(|e| MailError::Decode(format!("bad calendar id {id:?}: {e}")));
        }
        let first = self
            .list_calendars()
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| MailError::Decode("this iCloud account has no calendars".into()))?;
        Url::parse(&first.id).map_err(|e| MailError::Decode(format!("bad calendar URL: {e}")))
    }
}

fn propfind() -> Method {
    Method::from_bytes(b"PROPFIND").expect("valid HTTP method")
}

fn report() -> Method {
    Method::from_bytes(b"REPORT").expect("valid HTTP method")
}

#[async_trait]
impl CalendarProvider for IcloudClient {
    async fn list_calendars(&self) -> Result<Vec<CalendarInfo>, MailError> {
        let home = self.calendar_home().await?;
        let (base, body) = self
            .dav(propfind(), home, "1", dav::PROPFIND_CALENDARS)
            .await?;

        let mut calendars: Vec<CalendarInfo> = Vec::new();
        for response in dav::parse_multistatus(&body)? {
            // The calendar home also contains the scheduling inbox/outbox and
            // reminder (VTODO) lists; both tests are needed to exclude them.
            if !response.is_calendar() || !response.supports_events() {
                continue;
            }
            let url = dav::resolve_href(&base, &response.href)?;
            let name = response
                .display_name
                .clone()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| fallback_name(&url));
            calendars.push(CalendarInfo {
                id: url.to_string(),
                name,
                color: response.color,
                is_default: false,
                // Milestone 1 never writes, so no iCloud calendar is editable
                // yet. This is what keeps the new-event button from offering a
                // create that could only fail.
                can_edit: false,
            });
        }

        // CalDAV has no "default calendar" marker, so the picker needs a stable
        // choice: sort by name and take the first, which keeps the selection
        // from reshuffling between refreshes.
        calendars.sort_by_key(|c| c.name.to_lowercase());
        if let Some(first) = calendars.first_mut() {
            first.is_default = true;
        }
        Ok(calendars)
    }

    async fn calendar_view(
        &self,
        start: &str,
        end: &str,
        time_zone: &str,
    ) -> Result<Vec<CalendarEvent>, MailError> {
        let bound = |iso: &str, slop: i64| {
            dav::to_utc_wall(iso, slop)
                .ok_or_else(|| MailError::Decode(format!("bad calendar window bound {iso:?}")))
        };
        let window_start = bound(start, -WINDOW_SLOP_DAYS)?;
        let window_end = bound(end, WINDOW_SLOP_DAYS)?;
        let filter_start = dav::to_caldav_instant(start, -WINDOW_SLOP_DAYS)
            .ok_or_else(|| MailError::Decode(format!("bad calendar window start {start:?}")))?;
        let filter_end = dav::to_caldav_instant(end, WINDOW_SLOP_DAYS)
            .ok_or_else(|| MailError::Decode(format!("bad calendar window end {end:?}")))?;

        let target = self.target_calendar().await?;
        let (base, body) = self
            .dav_body(
                report(),
                target,
                "1",
                dav::calendar_query(&filter_start, &filter_end),
            )
            .await?;

        let mut events = Vec::new();
        for response in dav::parse_multistatus(&body)? {
            let Some(data) = response.calendar_data.as_deref() else {
                continue;
            };
            let resource = dav::resolve_href(&base, &response.href)?;
            events.extend(events_from_resource(
                resource.as_str(),
                data,
                &window_start,
                &window_end,
                time_zone,
                &self.apple_id,
            ));
            if events.len() >= MAX_EVENTS {
                break;
            }
        }

        events.sort_by(|a, b| a.start.date_time.cmp(&b.start.date_time));
        Ok(events)
    }

    async fn create_event(
        &self,
        _event: &NewEvent,
        _time_zone: &str,
    ) -> Result<CalendarEvent, MailError> {
        Err(MailError::Unsupported)
    }

    async fn respond_to_event(
        &self,
        _id: &str,
        _response: InviteResponse,
        _comment: Option<&str>,
        _send_response: bool,
    ) -> Result<(), MailError> {
        Err(MailError::Unsupported)
    }

    async fn delete_event(&self, _id: &str) -> Result<(), MailError> {
        Err(MailError::Unsupported)
    }
}

/// Whether a redirect target is still Apple's CalDAV service, over TLS.
fn is_icloud_host(url: &Url) -> bool {
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|h| h == "icloud.com" || h.ends_with(".icloud.com"))
}

/// An override's own start, falling back to the slot it replaces when it does
/// not declare one.
fn override_start(component: &ical::Component, recurrence_id: &str) -> String {
    component
        .get("DTSTART")
        .and_then(|p| p.date_time())
        .map(|dt| dt.wall)
        .unwrap_or_else(|| recurrence_id.to_string())
}

/// The date part of a wall clock, used to match exclusions across zone forms.
fn day_key(wall: &str) -> String {
    wall.get(..10).unwrap_or(wall).to_string()
}

/// A readable name for a calendar whose `displayname` the server omitted: the
/// last path segment of its URL.
fn fallback_name(url: &Url) -> String {
    url.path_segments()
        .and_then(|mut segments| segments.rfind(|s| !s.is_empty()))
        .unwrap_or("Calendar")
        .to_string()
}

/// Turn one calendar resource's iCalendar body into the events it contributes to
/// `[window_start, window_end)`.
///
/// Pure: no HTTP, no credentials — the whole mapping is testable against a
/// captured `.ics` body.
fn events_from_resource(
    resource: &str,
    data: &str,
    window_start: &str,
    window_end: &str,
    fallback_zone: &str,
    self_email: &str,
) -> Vec<CalendarEvent> {
    let mut out = Vec::new();
    for calendar in ical::parse(data) {
        // A recurring series is one resource holding a master VEVENT plus one
        // extra VEVENT per modified occurrence, all sharing a UID. BTreeMap, not
        // HashMap, so repeated views return events in the same order.
        let mut groups: BTreeMap<String, (Option<ical::Component>, Vec<ical::Component>)> =
            BTreeMap::new();
        for event in calendar.children_named("VEVENT") {
            let entry = groups.entry(event.text("UID")).or_default();
            if event.get("RECURRENCE-ID").is_some() {
                entry.1.push(event.clone());
            } else if entry.0.is_none() {
                entry.0 = Some(event.clone());
            }
        }
        for (_, (master, overrides)) in groups {
            if out.len() >= MAX_EVENTS {
                return out;
            }
            out.extend(expand_group(
                resource,
                master.as_ref(),
                &overrides,
                window_start,
                window_end,
                fallback_zone,
                self_email,
            ));
        }
    }
    out
}

/// Expand one UID's master + overrides into concrete occurrences.
fn expand_group(
    resource: &str,
    master: Option<&ical::Component>,
    overrides: &[ical::Component],
    window_start: &str,
    window_end: &str,
    fallback_zone: &str,
    self_email: &str,
) -> Vec<CalendarEvent> {
    let mut out = Vec::new();

    // Overrides keyed by the occurrence they replace.
    let by_key: BTreeMap<String, &ical::Component> = overrides
        .iter()
        .filter_map(|o| {
            let key = o.get("RECURRENCE-ID")?.date_time()?.wall;
            Some((key, o))
        })
        .collect();

    let Some(master) = master else {
        // The master lies outside the queried window but a modified occurrence
        // falls inside it. Showing the override alone beats hiding a real event.
        for (key, component) in by_key {
            // A moved occurrence belongs at its OWN start; the slot it replaces is
            // precisely the time it was moved away from.
            let start = override_start(component, &key);
            if let Some(event) = build_event(
                resource,
                component,
                &start,
                true,
                fallback_zone,
                self_email,
                window_start,
                window_end,
            ) {
                out.push(event);
            }
        }
        return out;
    };

    let Some(start) = master.get("DTSTART").and_then(|p| p.date_time()) else {
        return out;
    };
    let duration = duration_seconds(master, &start);

    // Deleted occurrences. EXDATE is repeatable *and* comma-separated.
    // Keyed by day rather than by exact wall clock: a server may write EXDATE in
    // UTC while DTSTART carries a TZID, and comparing those two wall clocks
    // directly would silently resurrect a deleted occurrence.
    // ponytail: day granularity cannot separate two occurrences of one series on
    // the same day, which sub-daily frequencies would need - and those are
    // unsupported anyway. Exact once something here can convert between zones.
    let excluded: BTreeSet<String> = master
        .all("EXDATE")
        .flat_map(|p| {
            p.value
                .split(',')
                .filter_map(|v| ical::IcalDateTime::parse(v, p.param("TZID")))
                .map(|dt| day_key(&dt.wall))
                .collect::<Vec<_>>()
        })
        .collect();

    let rule = master
        .get("RRULE")
        .and_then(|p| rrule::RRule::parse(&p.value));
    let is_recurring = rule.is_some() || !by_key.is_empty();

    // Widen the expansion window by the event's own length so a long occurrence
    // that began before the window still surfaces.
    let expand_from =
        civil::add_seconds(window_start, -duration).unwrap_or_else(|| window_start.to_string());

    let occurrences = match &rule {
        Some(rule) => rrule::expand(&start.wall, rule, &expand_from, window_end),
        None => vec![start.wall.clone()],
    };

    let mut seen = BTreeSet::new();
    for occurrence in occurrences {
        if excluded.contains(&day_key(&occurrence)) || !seen.insert(occurrence.clone()) {
            continue;
        }
        // A moved occurrence carries its own times; a generated one inherits the
        // series' start time and length.
        let (component, occurrence_start) = match by_key.get(&occurrence) {
            Some(component) => {
                let moved = component
                    .get("DTSTART")
                    .and_then(|p| p.date_time())
                    .map(|dt| dt.wall)
                    .unwrap_or_else(|| occurrence.clone());
                (*component, moved)
            }
            None => (master, occurrence.clone()),
        };
        if let Some(event) = build_event(
            resource,
            component,
            &occurrence_start,
            is_recurring,
            fallback_zone,
            self_email,
            window_start,
            window_end,
        ) {
            out.push(event);
        }
    }

    // An override moved outside the generated set (its rule no longer produces
    // that slot) is still a real event the user scheduled.
    for (key, component) in &by_key {
        if excluded.contains(&day_key(key)) || seen.contains(key) {
            continue;
        }
        let start = override_start(component, key);
        if let Some(event) = build_event(
            resource,
            component,
            &start,
            true,
            fallback_zone,
            self_email,
            window_start,
            window_end,
        ) {
            out.push(event);
        }
    }

    out
}

/// Build one domain event, or `None` when it does not overlap the window.
#[allow(clippy::too_many_arguments)]
fn build_event(
    resource: &str,
    source: &ical::Component,
    start_wall: &str,
    is_recurring: bool,
    fallback_zone: &str,
    self_email: &str,
    window_start: &str,
    window_end: &str,
) -> Option<CalendarEvent> {
    let declared_start = source.get("DTSTART").and_then(|p| p.date_time())?;
    let duration = duration_seconds(source, &declared_start);
    let is_all_day = declared_start.is_date;

    // All-day values are advanced whole days; shifting them by seconds is what
    // lands them on the wrong date.
    let end_wall = if is_all_day {
        civil::add_days(start_wall, duration / 86_400)
    } else {
        civil::add_seconds(start_wall, duration)
    }
    .unwrap_or_else(|| start_wall.to_string());

    // Overlap, not containment: a multi-day event that began before the window
    // is still in view. Dropping it here is the same bug the agenda's
    // clamp-to-first-day logic exists to avoid.
    if start_wall >= window_end || end_wall.as_str() <= window_start {
        return None;
    }

    let zone = declared_start
        .zone
        .clone()
        .unwrap_or_else(|| fallback_zone.to_string());

    let attendees: Vec<Attendee> = source.all("ATTENDEE").filter_map(attendee_from).collect();
    let organizer = source.get("ORGANIZER");
    let organizer_email = organizer
        .map(|p| strip_mailto(&p.value))
        .unwrap_or_default();
    let organizer_name = organizer
        .and_then(|p| p.param("CN").map(str::to_string))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| organizer_email.clone());

    let response_status =
        if !organizer_email.is_empty() && organizer_email.eq_ignore_ascii_case(self_email) {
            ResponseStatus::Organizer
        } else {
            attendees
                .iter()
                .find(|a| a.email.eq_ignore_ascii_case(self_email))
                .map(|a| a.status)
                .unwrap_or(ResponseStatus::None)
        };

    // The `RECURRENCE-ID` segment is what milestone 2 needs to address a single
    // occurrence of a series; it is empty for a one-off event.
    let recurrence_key = if is_recurring { start_wall } else { "" };

    Some(CalendarEvent {
        id: format!("icloud:{resource}|{recurrence_key}"),
        subject: source.text("SUMMARY"),
        start: EventDateTime {
            date_time: start_wall.to_string(),
            time_zone: zone.clone(),
        },
        end: EventDateTime {
            date_time: end_wall,
            time_zone: zone,
        },
        is_all_day,
        location: source.text("LOCATION"),
        organizer_name,
        organizer_email,
        attendees,
        // DESCRIPTION is plain text; the shared sanitizer escapes it and turns
        // line breaks into markup, exactly as it does for a plain-text mail body.
        body_html: crate::html::sanitize_email(&source.text("DESCRIPTION"), false, false).html,
        is_cancelled: source.text("STATUS").eq_ignore_ascii_case("CANCELLED"),
        is_recurring,
        online_meeting_url: meeting_url(source),
        response_status,
        web_link: None,
        // ponytail: milestone 1 is read-only, so no event advertises an edit
        // affordance that would only fail. Milestone 2 sets this from whether
        // ORGANIZER matches the signed-in Apple ID.
        is_organizer: false,
        // There is no reply path either, so the UI must not offer RSVP buttons
        // that could only fail. Milestone 2 turns both of these on together.
        can_respond: false,
        reminder_minutes_before_start: reminder_minutes(source),
    })
}

/// An event's length in seconds, from `DTEND` or `DURATION`.
///
/// RFC 5545 §3.6.1: a date-only `DTSTART` with neither lasts one day; a
/// date-time `DTSTART` with neither is instantaneous.
fn duration_seconds(component: &ical::Component, start: &ical::IcalDateTime) -> i64 {
    if let Some(end) = component.get("DTEND").and_then(|p| p.date_time()) {
        if let Some(seconds) = civil::diff_seconds(&start.wall, &end.wall) {
            return seconds.max(0);
        }
    }
    if let Some(seconds) = component
        .get("DURATION")
        .and_then(|p| ical::parse_duration(&p.value))
    {
        return seconds.max(0);
    }
    if start.is_date {
        86_400
    } else {
        0
    }
}

fn attendee_from(property: &ical::Property) -> Option<Attendee> {
    let email = strip_mailto(&property.value);
    if email.is_empty() {
        return None;
    }
    Some(Attendee {
        name: property
            .param("CN")
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| email.clone()),
        email,
        status: partstat(property.param("PARTSTAT")),
        is_required: !matches!(
            property.param("ROLE").unwrap_or("REQ-PARTICIPANT"),
            "OPT-PARTICIPANT" | "NON-PARTICIPANT"
        ),
    })
}

/// Map an iCalendar `PARTSTAT` onto the domain's response vocabulary.
fn partstat(raw: Option<&str>) -> ResponseStatus {
    match raw.unwrap_or("").to_ascii_uppercase().as_str() {
        "ACCEPTED" => ResponseStatus::Accepted,
        "DECLINED" => ResponseStatus::Declined,
        "TENTATIVE" => ResponseStatus::TentativelyAccepted,
        "NEEDS-ACTION" => ResponseStatus::NotResponded,
        _ => ResponseStatus::None,
    }
}

fn strip_mailto(value: &str) -> String {
    let trimmed = value.trim();
    trimmed
        .get(..7)
        .filter(|p| p.eq_ignore_ascii_case("mailto:"))
        .map(|_| trimmed[7..].to_string())
        .unwrap_or_else(|| trimmed.to_string())
}

/// A join link, if the event carries one.
fn meeting_url(component: &ical::Component) -> Option<String> {
    ["CONFERENCE", "X-APPLE-MEETING-URL", "URL"]
        .iter()
        .filter_map(|name| component.get(name))
        .map(|property| property.text())
        .find(|value| value.starts_with("https://") || value.starts_with("http://"))
}

/// Minutes before the start at which the user's alarm fires.
fn reminder_minutes(component: &ical::Component) -> Option<u32> {
    component.children_named("VALARM").find_map(|alarm| {
        let trigger = alarm.get("TRIGGER")?;
        // An absolute trigger cannot be expressed as "minutes before start".
        if trigger
            .param("VALUE")
            .is_some_and(|v| v.eq_ignore_ascii_case("DATE-TIME"))
        {
            return None;
        }
        let seconds = ical::parse_duration(&trigger.value)?;
        (seconds <= 0).then_some((-seconds / 60) as u32)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESOURCE: &str = "https://p42-caldav.icloud.com/123/calendars/home/series.ics";

    /// A weekly series with one moved occurrence and one deleted occurrence —
    /// the shape iCloud actually returns for an edited recurring meeting.
    const SERIES: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:series-1\r\n\
SUMMARY:Standup\r\n\
DTSTART;TZID=Europe/London:20260722T090000\r\n\
DTEND;TZID=Europe/London:20260722T091500\r\n\
RRULE:FREQ=WEEKLY;COUNT=4\r\n\
EXDATE;TZID=Europe/London:20260805T090000\r\n\
ORGANIZER;CN=Jane Doe:mailto:jane@example.com\r\n\
ATTENDEE;CN=Sam;PARTSTAT=ACCEPTED;ROLE=REQ-PARTICIPANT:mailto:sam@example.com\r\n\
ATTENDEE;PARTSTAT=NEEDS-ACTION;ROLE=OPT-PARTICIPANT:mailto:me@icloud.com\r\n\
BEGIN:VALARM\r\n\
TRIGGER:-PT15M\r\n\
ACTION:DISPLAY\r\n\
END:VALARM\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:series-1\r\n\
SUMMARY:Standup (moved)\r\n\
RECURRENCE-ID;TZID=Europe/London:20260729T090000\r\n\
DTSTART;TZID=Europe/London:20260729T110000\r\n\
DTEND;TZID=Europe/London:20260729T113000\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    fn view(data: &str, from: &str, to: &str) -> Vec<CalendarEvent> {
        events_from_resource(RESOURCE, data, from, to, "Europe/London", "me@icloud.com")
    }

    #[test]
    fn expands_a_series_applying_the_override_and_the_exception() {
        let mut events = view(SERIES, "2026-07-01T00:00:00", "2026-09-01T00:00:00");
        events.sort_by(|a, b| a.start.date_time.cmp(&b.start.date_time));

        let slots: Vec<_> = events
            .iter()
            .map(|e| (e.start.date_time.as_str(), e.subject.as_str()))
            .collect();
        assert_eq!(
            slots,
            [
                ("2026-07-22T09:00:00", "Standup"),
                // The 29th was moved to 11:00 and renamed.
                ("2026-07-29T11:00:00", "Standup (moved)"),
                // 5 August is an EXDATE — deleted, so it must not appear.
                ("2026-08-12T09:00:00", "Standup"),
            ],
            "COUNT=4 minus one EXDATE, with the override replacing its slot"
        );
    }

    #[test]
    fn occurrences_carry_the_events_own_zone_never_a_converted_one() {
        let event = &view(SERIES, "2026-07-01T00:00:00", "2026-07-23T00:00:00")[0];
        assert_eq!(event.start.time_zone, "Europe/London");
        assert_eq!(event.start.date_time, "2026-07-22T09:00:00");
        // 15-minute meeting: the generated end keeps the series' own length.
        assert_eq!(event.end.date_time, "2026-07-22T09:15:00");
        assert!(!event.is_all_day);
        assert!(event.is_recurring);
    }

    #[test]
    fn maps_organizer_attendees_reminder_and_the_users_own_response() {
        let event = &view(SERIES, "2026-07-01T00:00:00", "2026-07-23T00:00:00")[0];
        assert_eq!(event.organizer_name, "Jane Doe");
        assert_eq!(event.organizer_email, "jane@example.com");
        assert_eq!(event.attendees.len(), 2);
        assert!(event.attendees[0].is_required);
        assert_eq!(event.attendees[0].status, ResponseStatus::Accepted);
        // An attendee with no CN falls back to the address.
        assert_eq!(event.attendees[1].name, "me@icloud.com");
        assert!(!event.attendees[1].is_required, "ROLE=OPT-PARTICIPANT");
        // The signed-in user's own PARTSTAT becomes the event's response.
        assert_eq!(event.response_status, ResponseStatus::NotResponded);
        assert_eq!(event.reminder_minutes_before_start, Some(15));
        // Milestone 1 is read-only, so no edit affordance is advertised.
        assert!(!event.is_organizer);
    }

    #[test]
    fn event_ids_round_trip_to_a_resource_and_an_occurrence() {
        let events = view(SERIES, "2026-07-01T00:00:00", "2026-08-01T00:00:00");
        let (resource, occurrence) = events[0]
            .id
            .strip_prefix("icloud:")
            .unwrap()
            .split_once('|')
            .unwrap();
        assert_eq!(resource, RESOURCE);
        assert_eq!(occurrence, "2026-07-22T09:00:00");
    }

    #[test]
    fn all_day_events_keep_their_dates_and_span_whole_days() {
        // iCalendar's all-day DTEND is exclusive, exactly like Graph's.
        let data = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:trip\r\n\
SUMMARY:Trip\r\n\
DTSTART;VALUE=DATE:20260722\r\n\
DTEND;VALUE=DATE:20260725\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let event = &view(data, "2026-07-01T00:00:00", "2026-08-01T00:00:00")[0];
        assert!(event.is_all_day);
        assert_eq!(event.start.date_time, "2026-07-22");
        assert_eq!(event.end.date_time, "2026-07-25");
    }

    #[test]
    fn a_multi_day_event_already_under_way_is_not_dropped() {
        // Starts before the window and ends inside it. A containment test would
        // lose it; the agenda clamps it onto the first visible day instead.
        let data = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:conf\r\n\
SUMMARY:Conference\r\n\
DTSTART:20260720T090000Z\r\n\
DTEND:20260724T170000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = view(data, "2026-07-22T00:00:00", "2026-07-23T00:00:00");
        assert_eq!(events.len(), 1, "ongoing event must survive the filter");
        assert_eq!(events[0].start.time_zone, "UTC");

        // …but one that ended before the window opened is genuinely gone.
        assert!(view(data, "2026-07-25T00:00:00", "2026-07-26T00:00:00").is_empty());
    }

    #[test]
    fn a_duration_stands_in_for_a_missing_dtend() {
        let data = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:call\r\n\
SUMMARY:Call\r\n\
DTSTART:20260722T090000Z\r\n\
DURATION:PT90M\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let event = &view(data, "2026-07-01T00:00:00", "2026-08-01T00:00:00")[0];
        assert_eq!(event.end.date_time, "2026-07-22T10:30:00");
    }

    #[test]
    fn an_orphan_override_still_shows() {
        // The master's own slot is outside the queried window, so iCloud returns
        // only the modified occurrence. Hiding it would lose a real event.
        let data = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:series-2\r\n\
SUMMARY:Moved one\r\n\
RECURRENCE-ID:20260722T090000Z\r\n\
DTSTART:20260722T140000Z\r\n\
DTEND:20260722T150000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let events = view(data, "2026-07-01T00:00:00", "2026-08-01T00:00:00");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].subject, "Moved one");
        assert!(events[0].is_recurring);
        // At its OWN start — the RECURRENCE-ID is the slot it was moved AWAY
        // from, so showing it there puts the meeting at the wrong time.
        assert_eq!(events[0].start.date_time, "2026-07-22T14:00:00");
        assert_eq!(events[0].end.date_time, "2026-07-22T15:00:00");
    }

    #[test]
    fn an_exdate_written_in_another_zone_form_still_deletes_its_occurrence() {
        // The master is zoned; the EXDATE is UTC. Comparing the two wall clocks
        // literally would miss, silently resurrecting a deleted occurrence.
        let data = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:series-3\r\n\
SUMMARY:Standup\r\n\
DTSTART;TZID=Europe/London:20260722T090000\r\n\
DTEND;TZID=Europe/London:20260722T091500\r\n\
RRULE:FREQ=WEEKLY;COUNT=3\r\n\
EXDATE:20260729T080000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let starts: Vec<_> = view(data, "2026-07-01T00:00:00", "2026-09-01T00:00:00")
            .into_iter()
            .map(|e| e.start.date_time)
            .collect();
        assert_eq!(starts, ["2026-07-22T09:00:00", "2026-08-05T09:00:00"]);
    }

    #[test]
    fn a_redirect_out_of_apples_domain_is_refused() {
        // Credentials are re-attached on every hop, so an off-domain redirect
        // would hand the app-specific password to whoever asked for it.
        for allowed in [
            "https://caldav.icloud.com/",
            "https://p42-caldav.icloud.com/1/calendars/",
            "https://icloud.com/",
        ] {
            assert!(is_icloud_host(&Url::parse(allowed).unwrap()), "{allowed}");
        }
        for refused in [
            "https://attacker.example/",
            "http://p42-caldav.icloud.com/", // plaintext downgrade
            "https://icloud.com.attacker.example/", // suffix lookalike
            "https://noticloud.com/",
        ] {
            assert!(!is_icloud_host(&Url::parse(refused).unwrap()), "{refused}");
        }
    }

    #[test]
    fn cancelled_status_and_description_sanitizing_survive() {
        let data = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:x\r\n\
SUMMARY:Gone\r\n\
STATUS:CANCELLED\r\n\
DESCRIPTION:Line one\\nwith <script>alert(1)</script>\r\n\
DTSTART:20260722T090000Z\r\n\
DTEND:20260722T100000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let event = &view(data, "2026-07-01T00:00:00", "2026-08-01T00:00:00")[0];
        assert!(event.is_cancelled);
        assert!(
            !event.body_html.contains("<script>"),
            "description is escaped: {}",
            event.body_html
        );
        assert!(event.body_html.contains("alert(1)"));
    }

    #[test]
    fn the_app_specific_password_never_reaches_an_error_string() {
        let client = IcloudClient::new("me@icloud.com", "abcd-efgh-ijkl-mnop");
        let leaked = client.scrub("connect failed for user with abcd-efgh-ijkl-mnop supplied");
        assert!(!leaked.contains("abcd-efgh-ijkl-mnop"), "{leaked}");
        assert!(leaked.contains("***"));
    }
}
