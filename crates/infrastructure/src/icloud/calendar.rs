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
//! **Time zones:** on the read side nothing is converted — each event keeps the
//! wall clock and zone iCalendar stated, and the frontend resolves it against
//! the browser's IANA database. On the write side the frontend does the reverse,
//! sending times already normalised to UTC, so [`emit`] never needs the tz
//! database the Rust tree does not carry. See [`super::civil`].
//!
//! **Writes** (create / update / delete / RSVP) build or round-trip an
//! iCalendar resource and `PUT`/`DELETE` it. An update re-emits the parsed
//! resource with only the edited fields changed, so unmodelled properties
//! (`ORGANIZER`, an `RRULE`, a `VTIMEZONE`, `X-` extensions) survive untouched.
//! Deleting one occurrence of a series writes an `EXDATE` rather than removing
//! the whole resource.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use reqwest::Method;
use url::Url;
use wattmail_domain::{
    Attendee, CalendarEvent, CalendarInfo, CalendarProvider, EventDateTime, InviteResponse,
    MailError, NewEvent, ResponseStatus,
};

use super::{civil, dav, emit, ical, rrule};

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
        let (url, response) = self
            .send(
                method,
                url,
                &[
                    ("Depth", depth.to_string()),
                    ("Content-Type", "application/xml; charset=utf-8".to_string()),
                ],
                Some(body),
            )
            .await?;
        let response = self.expect_ok(response).await?;
        let text = response
            .text()
            .await
            .map_err(|e| MailError::Network(self.scrub(&e.to_string())))?;
        Ok((url, text))
    }

    /// The redirect-following, credential-re-attaching request core shared by
    /// every read and write call. Returns the URL that answered and the final
    /// non-redirect response, unread — the caller owns status and body handling.
    ///
    /// The Apple-domain guard here is load-bearing: the app-specific password is
    /// re-sent on every hop, so a redirect out of `*.icloud.com` (or off TLS)
    /// would hand it to whoever asked. reqwest's own cross-origin credential
    /// stripping is disabled because it is what breaks iCloud discovery.
    async fn send(
        &self,
        method: Method,
        url: Url,
        headers: &[(&str, String)],
        body: Option<String>,
    ) -> Result<(Url, reqwest::Response), MailError> {
        let mut url = url;
        for _ in 0..MAX_REDIRECTS {
            let mut request = self
                .http
                .request(method.clone(), url.clone())
                .basic_auth(&self.apple_id, Some(&self.app_password));
            for (name, value) in headers {
                request = request.header(*name, value);
            }
            if let Some(body) = &body {
                request = request.body(body.clone());
            }
            let response = request
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
                if !is_icloud_host(&url) {
                    return Err(MailError::Decode(
                        "iCloud redirected outside icloud.com - refusing to send credentials there"
                            .into(),
                    ));
                }
                continue;
            }
            return Ok((url, response));
        }
        Err(MailError::Network(
            "iCloud redirected too many times".into(),
        ))
    }

    /// Map a non-redirect response's status to an error, or hand it back on 2xx.
    async fn expect_ok(&self, response: reqwest::Response) -> Result<reqwest::Response, MailError> {
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
        Ok(response)
    }

    /// GET a calendar resource, returning its body and current `ETag` (for a
    /// later conditional write).
    async fn get_resource(&self, url: Url) -> Result<(String, Option<String>), MailError> {
        let (_, response) = self.send(Method::GET, url, &[], None).await?;
        let response = self.expect_ok(response).await?;
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = response
            .text()
            .await
            .map_err(|e| MailError::Network(self.scrub(&e.to_string())))?;
        Ok((body, etag))
    }

    /// PUT an iCalendar resource. `if_match` guards an update against a
    /// concurrent edit; `create_only` sends `If-None-Match: *` so a create can
    /// never clobber an existing resource.
    async fn put_resource(
        &self,
        url: Url,
        body: String,
        if_match: Option<&str>,
        create_only: bool,
    ) -> Result<(), MailError> {
        let mut headers = vec![("Content-Type", "text/calendar; charset=utf-8".to_string())];
        if create_only {
            headers.push(("If-None-Match", "*".to_string()));
        }
        if let Some(tag) = if_match {
            headers.push(("If-Match", tag.to_string()));
        }
        let (_, response) = self.send(Method::PUT, url, &headers, Some(body)).await?;
        self.expect_ok(response).await.map(|_| ())
    }

    /// DELETE a calendar resource.
    async fn delete_resource(&self, url: Url, if_match: Option<&str>) -> Result<(), MailError> {
        let headers: Vec<(&str, String)> = if_match
            .map(|tag| vec![("If-Match", tag.to_string())])
            .unwrap_or_default();
        let (_, response) = self.send(Method::DELETE, url, &headers, None).await?;
        self.expect_ok(response).await.map(|_| ())
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
        // The selected id comes from localStorage, so it is guarded exactly like
        // a stored event resource: a create would otherwise PUT the app-specific
        // password to whatever host a tampered selection named.
        if let Some(id) = &self.calendar_id {
            return self.resource_url(id);
        }
        let first = self
            .list_calendars()
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| MailError::Decode("this iCloud account has no calendars".into()))?;
        self.resource_url(&first.id)
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
            // A subscribed feed (a holiday calendar) reports no write privilege,
            // so the UI won't offer "new event" on it; an owned calendar is
            // writable. Read before the struct moves `color` out of `response`.
            let can_edit = response.writable();
            calendars.push(CalendarInfo {
                id: url.to_string(),
                name,
                color: response.color,
                is_default: false,
                can_edit,
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
        event: &NewEvent,
        _time_zone: &str,
    ) -> Result<CalendarEvent, MailError> {
        // The frontend sends iCloud times already normalised to UTC (the Rust
        // side has no tz database to convert a zoned wall clock), so `time_zone`
        // is unused: DTSTART/DTEND are emitted as UTC instants or date-only.
        let collection = self.target_calendar().await?;
        let (uid, filename) = new_uid();
        let resource = collection
            .join(&filename)
            .map_err(|e| MailError::Decode(format!("bad calendar collection URL: {e}")))?;

        let calendar = build_vcalendar(build_vevent(event, &uid, &now_utc_stamp()));
        self.put_resource(resource.clone(), emit::serialize(&calendar), None, true)
            .await?;
        Ok(echo_event(event, &resource, ""))
    }

    async fn update_event(
        &self,
        id: &str,
        event: &NewEvent,
        _time_zone: &str,
    ) -> Result<CalendarEvent, MailError> {
        let (resource, recurrence) = parse_event_id(id)?;
        let url = self.resource_url(&resource)?;
        // Round-trip the existing resource so unmodelled properties (ORGANIZER,
        // any X- extensions, a VTIMEZONE) survive an edit untouched.
        let (body, etag) = self.get_resource(url.clone()).await?;
        let mut calendars = ical::parse(&body);
        let (ci, vi) = find_master(&calendars)
            .ok_or_else(|| MailError::Decode("event not found in its resource".into()))?;
        apply_edits(&mut calendars[ci].children[vi], event, &now_utc_stamp());
        self.put_resource(
            url.clone(),
            emit::serialize(&calendars[ci]),
            etag.as_deref(),
            false,
        )
        .await?;
        Ok(echo_event(event, &url, &recurrence))
    }

    async fn respond_to_event(
        &self,
        id: &str,
        response: InviteResponse,
        _comment: Option<&str>,
        _send_response: bool,
    ) -> Result<(), MailError> {
        // iCloud uses RFC 6638 implicit scheduling: writing the attendee's own
        // PARTSTAT back is what dispatches the iTIP reply to the organizer.
        let (resource, recurrence) = parse_event_id(id)?;
        let url = self.resource_url(&resource)?;
        let (body, etag) = self.get_resource(url.clone()).await?;
        let mut calendars = ical::parse(&body);
        let (ci, vi) = find_response_target(&calendars, &recurrence)
            .ok_or_else(|| MailError::Decode("event not found in its resource".into()))?;
        let partstat = match response {
            InviteResponse::Accept => "ACCEPTED",
            InviteResponse::TentativelyAccept => "TENTATIVE",
            InviteResponse::Decline => "DECLINED",
        };
        set_partstat(&mut calendars[ci].children[vi], &self.apple_id, partstat)?;
        touch(&mut calendars[ci].children[vi], &now_utc_stamp());
        self.put_resource(url, emit::serialize(&calendars[ci]), etag.as_deref(), false)
            .await
    }

    async fn delete_event(&self, id: &str) -> Result<(), MailError> {
        let (resource, recurrence) = parse_event_id(id)?;
        let url = self.resource_url(&resource)?;
        if recurrence.is_empty() {
            // A one-off event, or an explicit whole-series delete: drop the
            // whole resource.
            return self.delete_resource(url, None).await;
        }
        // A single occurrence of a series: exclude it on the master and drop any
        // override for it, leaving the rest of the series in place.
        let (body, etag) = self.get_resource(url.clone()).await?;
        let mut calendars = ical::parse(&body);
        let (ci, vi) = find_master(&calendars)
            .ok_or_else(|| MailError::Decode("series master not found in its resource".into()))?;
        add_exdate(&mut calendars[ci].children[vi], &recurrence);
        remove_override(&mut calendars[ci], &recurrence);
        touch(&mut calendars[ci].children[vi], &now_utc_stamp());
        self.put_resource(url, emit::serialize(&calendars[ci]), etag.as_deref(), false)
            .await
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

// ---- Write path (create / update / delete / RSVP) ----

impl IcloudClient {
    /// Parse a stored resource href and confirm it is still an Apple URL over
    /// TLS before any credential-bearing request is sent to it — the id could
    /// have been tampered with in `accounts.json` or localStorage.
    fn resource_url(&self, resource: &str) -> Result<Url, MailError> {
        let url = Url::parse(resource)
            .map_err(|e| MailError::Decode(format!("bad calendar resource id: {e}")))?;
        if !is_icloud_host(&url) {
            return Err(MailError::Decode(
                "calendar resource is not an iCloud URL".into(),
            ));
        }
        Ok(url)
    }
}

/// Split an event id (`icloud:<resource-href>|<recurrence-id>`) into its parts.
/// The recurrence segment is empty for a one-off event.
fn parse_event_id(id: &str) -> Result<(String, String), MailError> {
    let rest = id
        .strip_prefix("icloud:")
        .ok_or_else(|| MailError::Decode(format!("not an iCloud event id: {id}")))?;
    let (resource, recurrence) = rest.split_once('|').unwrap_or((rest, ""));
    if resource.is_empty() {
        return Err(MailError::Decode("event id has no resource".into()));
    }
    Ok((resource.to_string(), recurrence.to_string()))
}

/// A fresh (UID, resource filename). The filename is a random hex so it never
/// needs URL-escaping; the UID reuses it so the two stay associated.
fn new_uid() -> (String, String) {
    let n: u128 = rand::random();
    let hex = format!("{n:032x}");
    (format!("{hex}@wattmail"), format!("{hex}.ics"))
}

/// The current instant as an iCalendar UTC stamp (`YYYYMMDDTHHMMSSZ`), for
/// `DTSTAMP` / `LAST-MODIFIED`. Uses the civil-date helpers, not a tz database.
fn now_utc_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (y, m, d) = civil::civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        sod / 3600,
        (sod / 60) % 60,
        sod % 60
    )
}

/// Wrap a VEVENT in a minimal VCALENDAR ready to PUT.
fn build_vcalendar(vevent: ical::Component) -> ical::Component {
    let mut calendar = ical::Component::new("VCALENDAR");
    calendar
        .set(ical::Property::raw("VERSION", "2.0"))
        .set(ical::Property::raw("PRODID", "-//WattMail//Calendar//EN"))
        .push_child(vevent);
    calendar
}

/// Build a fresh VEVENT from a [`NewEvent`].
fn build_vevent(event: &NewEvent, uid: &str, dtstamp: &str) -> ical::Component {
    let mut vevent = ical::Component::new("VEVENT");
    vevent
        .set(ical::Property::raw("UID", uid))
        .set(ical::Property::raw("DTSTAMP", dtstamp));
    apply_edits(&mut vevent, event, dtstamp);
    vevent
}

/// Apply a [`NewEvent`]'s editable fields onto a VEVENT — used both to build a
/// new event and to overwrite an existing one in place, so an edit preserves
/// every property (UID, ORGANIZER, RRULE, X- extensions) it does not touch.
fn apply_edits(vevent: &mut ical::Component, event: &NewEvent, stamp: &str) {
    vevent.set(ical::Property::plain("SUMMARY", &event.subject));
    vevent.set(format_dt("DTSTART", &event.start, event.is_all_day));
    vevent.set(format_dt("DTEND", &event.end, event.is_all_day));

    if event.location.trim().is_empty() {
        vevent.remove("LOCATION");
    } else {
        vevent.set(ical::Property::plain("LOCATION", event.location.trim()));
    }

    let description = html_to_plain(&event.body_html);
    if description.is_empty() {
        vevent.remove("DESCRIPTION");
    } else {
        vevent.set(ical::Property::plain("DESCRIPTION", &description));
    }

    if let Some(list) = &event.attendees {
        // Preserve the existing entry for any address that stays on the list, so
        // an edit does not reset everyone's PARTSTAT (which would re-fire the
        // invitations via implicit scheduling) or drop their display names.
        let existing: Vec<ical::Property> = vevent.all("ATTENDEE").cloned().collect();
        vevent.remove("ATTENDEE");
        for address in list
            .iter()
            .map(|a| a.trim())
            // A control character here would inject a second content line into
            // the iCalendar — the frontend already validates, but this is the
            // trust boundary.
            .filter(|a| !a.is_empty() && !a.chars().any(char::is_control))
        {
            match existing
                .iter()
                .find(|p| strip_mailto(&p.value).eq_ignore_ascii_case(address))
            {
                Some(kept) => vevent.push(kept.clone()),
                None => vevent.push(
                    ical::Property::raw("ATTENDEE", &format!("mailto:{address}"))
                        .with_param("ROLE", "REQ-PARTICIPANT")
                        .with_param("PARTSTAT", "NEEDS-ACTION")
                        .with_param("RSVP", "TRUE"),
                ),
            };
        }
    }

    // WattMail's single alert picker owns exactly one relative display alarm.
    // Replace that, but leave anything structurally different (an audio/email
    // alarm, an absolute-time trigger) that the user set elsewhere in place.
    vevent.retain_children(|child| child.name != "VALARM" || !is_wattmail_alarm(child));
    if let Some(minutes) = event.reminder_minutes_before_start {
        vevent.push_child(build_valarm(minutes));
    }

    touch(vevent, stamp);
}

/// Whether a VALARM is the kind WattMail's single alert picker manages: a
/// display alarm with a relative (`-PT…`/`PT…`) trigger, not an absolute one.
fn is_wattmail_alarm(alarm: &ical::Component) -> bool {
    let display = alarm
        .get("ACTION")
        .map(|p| p.value.eq_ignore_ascii_case("DISPLAY"))
        .unwrap_or(true);
    let relative = alarm.get("TRIGGER").is_some_and(|t| {
        !t.param("VALUE")
            .is_some_and(|v| v.eq_ignore_ascii_case("DATE-TIME"))
    });
    display && relative
}

/// A display reminder firing `minutes` before the start (`0` = at the start).
fn build_valarm(minutes: u32) -> ical::Component {
    let mut alarm = ical::Component::new("VALARM");
    // A zero lead is a non-negative "at start" trigger; `-PT0M` trips some
    // parsers, so it is written as `PT0S`.
    let trigger = if minutes == 0 {
        "PT0S".to_string()
    } else {
        format!("-PT{minutes}M")
    };
    alarm
        .set(ical::Property::raw("ACTION", "DISPLAY"))
        .set(ical::Property::plain("DESCRIPTION", "Reminder"))
        .set(ical::Property::raw("TRIGGER", &trigger));
    alarm
}

/// Bump `SEQUENCE` and refresh the modification stamps, so the server and any
/// attendees see the change as newer.
fn touch(vevent: &mut ical::Component, stamp: &str) {
    vevent.set(ical::Property::raw("DTSTAMP", stamp));
    vevent.set(ical::Property::raw("LAST-MODIFIED", stamp));
    let sequence = vevent
        .get("SEQUENCE")
        .and_then(|p| p.value.trim().parse::<u64>().ok())
        .unwrap_or(0);
    vevent.set(ical::Property::raw("SEQUENCE", &(sequence + 1).to_string()));
}

/// Format an [`EventDateTime`] as a DTSTART/DTEND-style property.
///
/// All-day values are date-only (`VALUE=DATE`); timed values are UTC instants
/// (`…Z`) because the frontend converts to UTC before sending — the Rust side
/// has no tz database and never emits a bare `TZID` without a `VTIMEZONE`.
fn format_dt(name: &str, dt: &EventDateTime, all_day: bool) -> ical::Property {
    let digits = compact(&dt.date_time);
    if all_day {
        let date = digits.get(..8).unwrap_or(&digits);
        ical::Property::raw(name, date).with_param("VALUE", "DATE")
    } else if dt.time_zone.eq_ignore_ascii_case("UTC") {
        ical::Property::raw(name, &format!("{digits}Z"))
    } else {
        // Floating fallback (should not occur for iCloud writes): emit the wall
        // clock with no zone rather than an unbacked TZID.
        ical::Property::raw(name, &digits)
    }
}

/// Strip the separators from an ISO wall clock: `2026-07-22T13:00:00` →
/// `20260722T130000`.
fn compact(wall: &str) -> String {
    wall.chars()
        .filter(|c| c.is_ascii_digit() || *c == 'T')
        .collect()
}

/// Reduce our own (or an edited) HTML body to the plain text iCalendar stores in
/// `DESCRIPTION`. Block boundaries become newlines; the handful of entities we
/// emit are decoded. Rich formatting is intentionally flattened — iCalendar
/// `DESCRIPTION` is plain text.
fn html_to_plain(html: &str) -> String {
    if html.trim().is_empty() {
        return String::new();
    }
    let mut with_breaks = html.to_string();
    for tag in [
        "<br>", "<br/>", "<br />", "</p>", "</div>", "</li>", "</tr>",
    ] {
        with_breaks = with_breaks.replace(tag, "\n");
    }
    let mut text = String::with_capacity(with_breaks.len());
    let mut in_tag = false;
    for c in with_breaks.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    let decoded = text
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    decoded
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// The VCALENDAR + master-VEVENT indices for the series master (the VEVENT with
/// no RECURRENCE-ID). A non-recurring resource has exactly one VEVENT.
fn find_master(calendars: &[ical::Component]) -> Option<(usize, usize)> {
    for (ci, calendar) in calendars.iter().enumerate() {
        for (vi, child) in calendar.children.iter().enumerate() {
            if child.name == "VEVENT" && child.get("RECURRENCE-ID").is_none() {
                return Some((ci, vi));
            }
        }
    }
    None
}

/// The VEVENT to respond on: the override for `recurrence` when it is a series
/// occurrence, otherwise the master.
fn find_response_target(calendars: &[ical::Component], recurrence: &str) -> Option<(usize, usize)> {
    if !recurrence.is_empty() {
        let key = day_key(recurrence);
        for (ci, calendar) in calendars.iter().enumerate() {
            for (vi, child) in calendar.children.iter().enumerate() {
                let matches = child.name == "VEVENT"
                    && child
                        .get("RECURRENCE-ID")
                        .and_then(|p| p.date_time())
                        .map(|dt| day_key(&dt.wall))
                        == Some(key.clone());
                if matches {
                    return Some((ci, vi));
                }
            }
        }
    }
    find_master(calendars)
}

/// Add an `EXDATE` for `recurrence` to the master, mirroring its `DTSTART`'s
/// zone form so the server matches it against the generated occurrence.
fn add_exdate(master: &mut ical::Component, recurrence: &str) {
    let digits = compact(recurrence);
    let start = master.get("DTSTART");
    let is_date = start
        .and_then(|p| p.param("VALUE"))
        .is_some_and(|v| v.eq_ignore_ascii_case("DATE"));
    let is_utc = start.is_some_and(|p| p.value.trim_end().ends_with(['Z', 'z']));
    let tzid = start.and_then(|p| p.param("TZID"));

    let exdate = if is_date {
        ical::Property::raw("EXDATE", digits.get(..8).unwrap_or(&digits))
            .with_param("VALUE", "DATE")
    } else if is_utc {
        ical::Property::raw("EXDATE", &format!("{digits}Z"))
    } else if let Some(zone) = tzid {
        ical::Property::raw("EXDATE", &digits).with_param("TZID", zone)
    } else {
        ical::Property::raw("EXDATE", &digits)
    };
    master.push(exdate);
}

/// Drop any override VEVENT for `recurrence`, so a deleted occurrence does not
/// linger as a modified one.
fn remove_override(calendar: &mut ical::Component, recurrence: &str) {
    let key = day_key(recurrence);
    calendar.children.retain(|child| {
        !(child.name == "VEVENT"
            && child
                .get("RECURRENCE-ID")
                .and_then(|p| p.date_time())
                .map(|dt| day_key(&dt.wall))
                == Some(key.clone()))
    });
}

/// Set the signed-in user's own `ATTENDEE` `PARTSTAT`. Errors when the user is
/// not an attendee — there is then no reply to send.
fn set_partstat(
    vevent: &mut ical::Component,
    self_email: &str,
    partstat: &str,
) -> Result<(), MailError> {
    for property in &mut vevent.properties {
        if property.name == "ATTENDEE"
            && strip_mailto(&property.value).eq_ignore_ascii_case(self_email)
        {
            property.set_param("PARTSTAT", partstat);
            return Ok(());
        }
    }
    Err(MailError::Unsupported)
}

/// The event echoed back to the caller after a write. The frontend re-fetches
/// the agenda immediately, so this only needs a correct id and start for the
/// jump-and-select; the times are those the caller just supplied.
fn echo_event(event: &NewEvent, resource: &Url, recurrence: &str) -> CalendarEvent {
    CalendarEvent {
        id: format!("icloud:{resource}|{recurrence}"),
        subject: event.subject.clone(),
        start: event.start.clone(),
        end: event.end.clone(),
        is_all_day: event.is_all_day,
        location: event.location.clone(),
        organizer_name: String::new(),
        organizer_email: String::new(),
        attendees: Vec::new(),
        body_html: crate::html::sanitize_email(&html_to_plain(&event.body_html), false, false).html,
        is_cancelled: false,
        is_recurring: !recurrence.is_empty(),
        online_meeting_url: None,
        response_status: ResponseStatus::Organizer,
        web_link: None,
        is_organizer: true,
        can_respond: false,
        reminder_minutes_before_start: event.reminder_minutes_before_start,
    }
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
            // A moved occurrence displays at its OWN start; the slot it replaces
            // (`key`, its RECURRENCE-ID) is what the id must carry.
            let start = override_start(component, &key);
            if let Some(event) = build_event(
                resource,
                component,
                &start,
                &key,
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
            &occurrence,
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
            key,
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
    // The occurrence's original slot (its RECURRENCE-ID), which the event id
    // must carry so a later delete/RSVP addresses the right instant. For a moved
    // occurrence this differs from `start_wall` (the time it was moved *to*); for
    // a generated occurrence and a one-off they are the same.
    recurrence_id: &str,
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

    // You organize an event you created (no ORGANIZER, the personal-event case)
    // or one whose ORGANIZER is your own address — either way you may edit and
    // delete it. Anything organized by someone else is an invitation.
    let is_organizer =
        organizer_email.is_empty() || organizer_email.eq_ignore_ascii_case(self_email);

    let response_status = if is_organizer {
        ResponseStatus::Organizer
    } else {
        attendees
            .iter()
            .find(|a| a.email.eq_ignore_ascii_case(self_email))
            .map(|a| a.status)
            .unwrap_or(ResponseStatus::None)
    };

    // RSVP is offered only when you are a genuine invitee with a reply to give.
    let can_respond = !is_organizer
        && matches!(
            response_status,
            ResponseStatus::Accepted
                | ResponseStatus::Declined
                | ResponseStatus::TentativelyAccepted
                | ResponseStatus::NotResponded
        );

    // The id's recurrence segment addresses one occurrence of a series for a
    // later delete/RSVP; it is the occurrence's original slot (RECURRENCE-ID),
    // never its moved display start, and empty for a one-off event.
    let recurrence_key = if is_recurring { recurrence_id } else { "" };

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
        is_organizer,
        can_respond,
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
        // Jane organizes this, so the user is an invitee who may RSVP but not edit.
        assert!(!event.is_organizer);
        assert!(event.can_respond);
    }

    #[test]
    fn a_personal_event_with_no_organizer_is_yours_to_edit() {
        // The common case: an event you created on iPhone has no ORGANIZER, so
        // you organize it — Edit/Delete are offered, RSVP is not.
        let data = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:mine\r\n\
SUMMARY:Dentist\r\n\
DTSTART:20260722T090000Z\r\n\
DTEND:20260722T093000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let event = &view(data, "2026-07-01T00:00:00", "2026-08-01T00:00:00")[0];
        assert!(event.is_organizer);
        assert!(!event.can_respond);
        assert_eq!(event.response_status, ResponseStatus::Organizer);
    }

    #[test]
    fn a_moved_occurrence_id_carries_its_recurrence_id_not_its_new_time() {
        // The SERIES override moved the 29th from 09:00 to 11:00. The event
        // displays at 11:00 but its id must address the 09:00 slot, so a later
        // delete writes an EXDATE the server's RRULE actually generates.
        let moved = view(SERIES, "2026-07-01T00:00:00", "2026-09-01T00:00:00")
            .into_iter()
            .find(|e| e.subject == "Standup (moved)")
            .unwrap();
        assert_eq!(moved.start.date_time, "2026-07-29T11:00:00"); // display
        let (_, recurrence) = parse_event_id(&moved.id).unwrap();
        assert_eq!(recurrence, "2026-07-29T09:00:00"); // RECURRENCE-ID, not 11:00

        // Feeding that id's recurrence back through delete's EXDATE step excludes
        // the real 09:00 slot — so the occurrence genuinely disappears.
        let mut calendars = ical::parse(SERIES);
        let (ci, vi) = find_master(&calendars).unwrap();
        add_exdate(&mut calendars[ci].children[vi], &recurrence);
        remove_override(&mut calendars[ci], &recurrence);
        let starts: Vec<_> = view(
            &emit::serialize(&calendars[ci]),
            "2026-07-01T00:00:00",
            "2026-09-01T00:00:00",
        )
        .into_iter()
        .map(|e| e.start.date_time)
        .collect();
        assert!(
            !starts.iter().any(|s| s.starts_with("2026-07-29")),
            "the 29th is gone, not resurrected at 09:00: {starts:?}"
        );
    }

    #[test]
    fn editing_attendees_preserves_the_partstat_of_those_who_stay() {
        let mut calendars = ical::parse(SERIES);
        let (ci, vi) = find_master(&calendars).unwrap();
        // Keep Sam (ACCEPTED) and me (NEEDS-ACTION); add a new invitee.
        let event = NewEvent {
            attendees: Some(vec![
                "sam@example.com".into(),
                "me@icloud.com".into(),
                "new@example.com".into(),
            ]),
            ..new_event("Standup", "2026-07-22T09:00:00", "2026-07-22T09:15:00")
        };
        apply_edits(&mut calendars[ci].children[vi], &event, "20260723T000000Z");

        let out = emit::serialize(&calendars[ci]);
        let vevent = ical::parse(&out)[0]
            .children_named("VEVENT")
            .next()
            .unwrap()
            .clone();
        let find = |addr: &str| {
            vevent
                .all("ATTENDEE")
                .find(|a| strip_mailto(&a.value) == addr)
                .cloned()
        };
        // Sam's ACCEPTED and CN survive — not reset to NEEDS-ACTION (which would
        // re-fire the invite).
        assert_eq!(
            find("sam@example.com").unwrap().param("PARTSTAT"),
            Some("ACCEPTED")
        );
        assert_eq!(find("sam@example.com").unwrap().param("CN"), Some("Sam"));
        // The new invitee is fresh.
        assert_eq!(
            find("new@example.com").unwrap().param("PARTSTAT"),
            Some("NEEDS-ACTION")
        );
    }

    #[test]
    fn an_edit_keeps_a_structurally_different_alarm_but_replaces_our_own() {
        // An event with a display reminder AND an audio alarm the user set
        // elsewhere. Editing (with our single picker) must keep the audio one.
        let data = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:two-alarms\r\n\
SUMMARY:Thing\r\n\
DTSTART:20260722T090000Z\r\n\
DTEND:20260722T093000Z\r\n\
BEGIN:VALARM\r\n\
ACTION:DISPLAY\r\n\
TRIGGER:-PT10M\r\n\
END:VALARM\r\n\
BEGIN:VALARM\r\n\
ACTION:AUDIO\r\n\
TRIGGER:-PT1H\r\n\
END:VALARM\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let mut calendars = ical::parse(data);
        let (ci, vi) = find_master(&calendars).unwrap();
        let event = NewEvent {
            reminder_minutes_before_start: Some(30),
            ..new_event("Thing", "2026-07-22T09:00:00", "2026-07-22T09:30:00")
        };
        apply_edits(&mut calendars[ci].children[vi], &event, "20260723T000000Z");

        let vevent = ical::parse(&emit::serialize(&calendars[ci]))[0]
            .children_named("VEVENT")
            .next()
            .unwrap()
            .clone();
        let alarms: Vec<_> = vevent.children_named("VALARM").collect();
        assert_eq!(alarms.len(), 2, "audio alarm kept, display alarm replaced");
        assert!(alarms
            .iter()
            .any(|a| a.get("ACTION").unwrap().value == "AUDIO"));
        let display = alarms
            .iter()
            .find(|a| a.get("ACTION").unwrap().value == "DISPLAY")
            .unwrap();
        assert_eq!(display.get("TRIGGER").unwrap().value, "-PT30M");
    }

    #[test]
    fn a_control_character_in_an_attendee_address_is_dropped_not_injected() {
        let mut vevent = ical::Component::new("VEVENT");
        vevent.set(ical::Property::raw("DTSTART", "20260722T090000Z"));
        let event = NewEvent {
            attendees: Some(vec!["ok@x.io".into(), "evil@x.io\r\nSUMMARY:HACKED".into()]),
            ..new_event("T", "2026-07-22T09:00:00", "2026-07-22T10:00:00")
        };
        apply_edits(&mut vevent, &event, "20260723T000000Z");
        let out = emit::serialize(&build_vcalendar(vevent));
        // The injected line never becomes its own property, and the good address
        // is the only attendee (checked through the parser — the good line may be
        // folded, so a raw substring search would be unreliable).
        assert!(!out.contains("HACKED"));
        let parsed = ical::parse(&out)[0]
            .children_named("VEVENT")
            .next()
            .unwrap()
            .clone();
        assert_eq!(parsed.text("SUMMARY"), "T");
        let attendees: Vec<_> = parsed
            .all("ATTENDEE")
            .map(|a| strip_mailto(&a.value))
            .collect();
        assert_eq!(attendees, ["ok@x.io"]);
    }

    #[test]
    fn a_create_target_outside_apple_is_refused() {
        let client = IcloudClient::new("me@icloud.com", "pw")
            .with_calendar(Some("https://attacker.example/steal/".into()));
        assert!(client
            .resource_url("https://attacker.example/steal/")
            .is_err());
        assert!(client
            .resource_url("https://p1-caldav.icloud.com/1/calendars/home/")
            .is_ok());
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

    fn new_event(subject: &str, start: &str, end: &str) -> NewEvent {
        NewEvent {
            subject: subject.into(),
            start: EventDateTime {
                date_time: start.into(),
                time_zone: "UTC".into(),
            },
            end: EventDateTime {
                date_time: end.into(),
                time_zone: "UTC".into(),
            },
            is_all_day: false,
            location: "Room 1".into(),
            body_html: "<p>Bring cake</p>".into(),
            attendees: None,
            reminder_minutes_before_start: Some(15),
        }
    }

    #[test]
    fn a_built_event_carries_emoji_reminders_and_utc_times() {
        // The subject is pure emoji — the whole point of the octet-safe folding.
        let event = new_event(
            "🎂🎉 Party 🥳",
            "2026-07-22T18:00:00",
            "2026-07-22T21:00:00",
        );
        let calendar = build_vcalendar(build_vevent(&event, "uid-1@wattmail", "20260722T120000Z"));
        let text = emit::serialize(&calendar);

        // Round-trips through the reader with the emoji intact.
        let parsed = &ical::parse(&text)[0];
        let vevent = parsed.children_named("VEVENT").next().unwrap();
        assert_eq!(vevent.text("SUMMARY"), "🎂🎉 Party 🥳");
        // Timed values are emitted as UTC instants (no TZID that would need a
        // VTIMEZONE the Rust side cannot build).
        assert_eq!(vevent.get("DTSTART").unwrap().value, "20260722T180000Z");
        assert_eq!(vevent.get("DTEND").unwrap().value, "20260722T210000Z");
        assert_eq!(vevent.text("LOCATION"), "Room 1");
        assert_eq!(vevent.text("DESCRIPTION"), "Bring cake");
        // A 15-minute reminder becomes a relative VALARM.
        let alarm = vevent.children_named("VALARM").next().unwrap();
        assert_eq!(alarm.get("TRIGGER").unwrap().value, "-PT15M");
    }

    #[test]
    fn an_all_day_built_event_is_date_only() {
        let mut event = new_event("Trip", "2026-07-22T00:00:00", "2026-07-25T00:00:00");
        event.is_all_day = true;
        let vevent = build_vevent(&event, "u@wattmail", "20260722T120000Z");
        let start = vevent.get("DTSTART").unwrap();
        assert_eq!(start.value, "20260722");
        assert_eq!(start.param("VALUE"), Some("DATE"));
        assert!(vevent.get("DTEND").unwrap().value.ends_with("20260725"));
    }

    #[test]
    fn an_edit_preserves_properties_it_does_not_touch() {
        // A resource that carries an ORGANIZER, an X- extension and a VTIMEZONE
        // the editor knows nothing about — all must survive the round trip.
        let existing = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VTIMEZONE\r\n\
TZID:Europe/London\r\n\
END:VTIMEZONE\r\n\
BEGIN:VEVENT\r\n\
UID:keep-me\r\n\
SEQUENCE:2\r\n\
ORGANIZER:mailto:boss@example.com\r\n\
X-APPLE-TRAVEL-ADVISORY-BEHAVIOR:AUTOMATIC\r\n\
SUMMARY:Old title\r\n\
DTSTART:20260722T090000Z\r\n\
DTEND:20260722T100000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let mut calendars = ical::parse(existing);
        let (ci, vi) = find_master(&calendars).unwrap();
        let event = new_event("New title", "2026-07-22T11:00:00", "2026-07-22T12:00:00");
        apply_edits(&mut calendars[ci].children[vi], &event, "20260723T000000Z");

        let out = emit::serialize(&calendars[ci]);
        let parsed = &ical::parse(&out)[0];
        let vevent = parsed.children_named("VEVENT").next().unwrap();
        // Edited fields changed…
        assert_eq!(vevent.text("SUMMARY"), "New title");
        assert_eq!(vevent.get("DTSTART").unwrap().value, "20260722T110000Z");
        // …the UID and unmodelled properties are untouched…
        assert_eq!(vevent.text("UID"), "keep-me");
        assert_eq!(vevent.text("ORGANIZER"), "mailto:boss@example.com");
        assert_eq!(vevent.text("X-APPLE-TRAVEL-ADVISORY-BEHAVIOR"), "AUTOMATIC");
        assert!(parsed.children_named("VTIMEZONE").next().is_some());
        // …and the sequence advanced so the change is seen as newer.
        assert_eq!(vevent.text("SEQUENCE"), "3");
    }

    #[test]
    fn deleting_one_occurrence_adds_an_exdate_and_drops_its_override() {
        let mut calendars = ical::parse(SERIES);
        let (ci, vi) = find_master(&calendars).unwrap();
        // Master DTSTART is TZID Europe/London, so the EXDATE must mirror that.
        add_exdate(&mut calendars[ci].children[vi], "2026-07-29T09:00:00");
        remove_override(&mut calendars[ci], "2026-07-29T09:00:00");

        let out = emit::serialize(&calendars[ci]);
        let parsed = &ical::parse(&out)[0];
        // The moved 29th override is gone…
        assert_eq!(parsed.children_named("VEVENT").count(), 1);
        let master = parsed.children_named("VEVENT").next().unwrap();
        // …and the master now also excludes the 29th, in DTSTART's zone form
        // (EXDATE is repeatable — the fixture already excluded the 5th).
        let exdate = master
            .all("EXDATE")
            .find(|p| p.value == "20260729T090000")
            .expect("an EXDATE for the deleted occurrence");
        assert_eq!(exdate.param("TZID"), Some("Europe/London"));

        // The whole series is unaffected apart from the exclusion.
        let starts: Vec<_> = view(&out, "2026-07-01T00:00:00", "2026-09-01T00:00:00")
            .into_iter()
            .map(|e| e.start.date_time)
            .collect();
        assert!(!starts.iter().any(|s| s.starts_with("2026-07-29")));
    }

    #[test]
    fn rsvp_sets_the_users_own_partstat_and_leaves_others_alone() {
        let mut calendars = ical::parse(SERIES);
        let (ci, vi) = find_response_target(&calendars, "").unwrap();
        set_partstat(&mut calendars[ci].children[vi], "me@icloud.com", "ACCEPTED").unwrap();

        let out = emit::serialize(&calendars[ci]);
        let vevent = ical::parse(&out)[0]
            .children_named("VEVENT")
            .next()
            .unwrap()
            .clone();
        let mine = vevent
            .all("ATTENDEE")
            .find(|a| strip_mailto(&a.value) == "me@icloud.com")
            .unwrap();
        assert_eq!(mine.param("PARTSTAT"), Some("ACCEPTED"));
        // Sam's ACCEPTED is untouched.
        let sam = vevent
            .all("ATTENDEE")
            .find(|a| strip_mailto(&a.value) == "sam@example.com")
            .unwrap();
        assert_eq!(sam.param("PARTSTAT"), Some("ACCEPTED"));

        // Responding as someone who is not an attendee is an error, not a no-op.
        let mut fresh = ical::parse(SERIES);
        let (ci, vi) = find_response_target(&fresh, "").unwrap();
        assert!(set_partstat(&mut fresh[ci].children[vi], "stranger@x.io", "DECLINED").is_err());
    }

    #[test]
    fn html_bodies_flatten_to_plain_description_text() {
        assert_eq!(
            html_to_plain("<p>Line one<br>line two</p>"),
            "Line one\nline two"
        );
        assert_eq!(html_to_plain("Tea &amp; <b>biscuits</b>"), "Tea & biscuits");
        assert_eq!(html_to_plain("   "), "");
    }

    #[test]
    fn event_ids_parse_back_into_a_resource_and_occurrence() {
        let (resource, recurrence) =
            parse_event_id("icloud:https://p1-caldav.icloud.com/1/c/e.ics|2026-07-22T09:00:00")
                .unwrap();
        assert_eq!(resource, "https://p1-caldav.icloud.com/1/c/e.ics");
        assert_eq!(recurrence, "2026-07-22T09:00:00");

        let (resource, recurrence) =
            parse_event_id("icloud:https://p1-caldav.icloud.com/1/c/e.ics|").unwrap();
        assert!(recurrence.is_empty());
        assert!(!resource.is_empty());

        assert!(parse_event_id("graph:whatever").is_err());
    }

    #[test]
    fn the_app_specific_password_never_reaches_an_error_string() {
        let client = IcloudClient::new("me@icloud.com", "abcd-efgh-ijkl-mnop");
        let leaked = client.scrub("connect failed for user with abcd-efgh-ijkl-mnop supplied");
        assert!(!leaked.contains("abcd-efgh-ijkl-mnop"), "{leaked}");
        assert!(leaked.contains("***"));
    }
}
