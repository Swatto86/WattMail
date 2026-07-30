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
//! the browser's IANA database. One-off writes are normalised to UTC; recurring
//! writes keep an IANA `TZID` so their local time survives daylight-saving
//! changes. See [`super::civil`].
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
    Attendee, CalendarEvent, CalendarInfo, CalendarProvider, EventDateTime, EventRecurrence,
    InviteResponse, MailError, NewEvent, ResponseStatus,
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
        let partstat = match response {
            InviteResponse::Accept => "ACCEPTED",
            InviteResponse::TentativelyAccept => "TENTATIVE",
            InviteResponse::Decline => "DECLINED",
        };
        // The VCALENDAR that changed — the one we PUT back. Deciding and mutating is
        // pure (`apply_response`); the GET above and PUT below are the only I/O.
        let ci = apply_response(
            &mut calendars,
            &self.apple_id,
            &recurrence,
            partstat,
            &now_utc_stamp(),
        )?;
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

/// The largest real UTC offset (Kiribati, UTC+14), the most a zone can move a
/// wall clock away from the instant it represents.
const MAX_ZONE_OFFSET_SECONDS: i64 = 14 * 3_600;

/// Whether an `EXDATE` wall clock hides the occurrence at wall clock `occurrence`,
/// tolerant of the zone-form mismatch a foreign CalDAV client can introduce —
/// writing an `EXDATE` in UTC while `DTSTART` carries a `TZID`. The same calendar
/// day matches (the common case, same-form values, and all-day dates); otherwise
/// a cross-midnight pair whose naive wall clocks lie within one zone offset of
/// each other matches too, since that offset is exactly what a form change shifts.
///
/// **Display-filter only.** This deliberately over-matches, so it must never route
/// a *write*: for a DAILY (or adjacent-`BYDAY`) series in a >10h-offset zone a
/// cross-form exclusion can also fall within this window of a genuinely adjacent
/// occurrence — the two are indistinguishable without a tz database. Hiding the
/// wrong occurrence from the agenda is recoverable; deleting or RSVP-ing the wrong
/// one is not, so `find_override`/`remove_override` match exactly instead.
/// Same-form data (the overwhelming majority, and everything WattMail writes)
/// matches by the day-key branch and is untouched by the tolerance.
fn excludes_occurrence(exdate: &str, occurrence: &str) -> bool {
    day_key(exdate) == day_key(occurrence)
        || matches!(civil::diff_seconds(exdate, occurrence), Some(d) if d.abs() <= MAX_ZONE_OFFSET_SECONDS)
}

/// Every `EXDATE` wall clock on a master, one per comma-separated value, each in
/// the zone form its own `EXDATE` property carries. `EXDATE` is repeatable *and*
/// comma-separated, so both are flattened.
fn exdate_walls(master: &ical::Component) -> Vec<String> {
    master
        .all("EXDATE")
        .flat_map(|p| {
            p.value
                .split(',')
                .filter_map(|v| ical::IcalDateTime::parse(v, p.param("TZID")))
                .map(|dt| dt.wall)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Whether the master's own `EXDATE` list hides the occurrence at wall clock
/// `recurrence`. Tolerant (via [`excludes_occurrence`]) on purpose: refusing to
/// write an override onto an already-deleted occurrence is the safe direction, so
/// over-matching here can only decline a genuine RSVP (recoverable) — never
/// resurrect a cancelled instance by writing an override the master contradicts.
fn master_excludes(master: &ical::Component, recurrence: &str) -> bool {
    exdate_walls(master)
        .iter()
        .any(|ex| excludes_occurrence(ex, recurrence))
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
    if let Some(recurrence) = event.recurrence {
        vevent.set(ical::Property::raw("RRULE", recurrence_rule(recurrence)));
    }

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
/// (`…Z`) or IANA-zone wall clocks for recurring events.
fn format_dt(name: &str, dt: &EventDateTime, all_day: bool) -> ical::Property {
    let digits = compact(&dt.date_time);
    if all_day {
        let date = digits.get(..8).unwrap_or(&digits);
        ical::Property::raw(name, date).with_param("VALUE", "DATE")
    } else if dt.time_zone.eq_ignore_ascii_case("UTC") {
        ical::Property::raw(name, &format!("{digits}Z"))
    } else if safe_tzid(&dt.time_zone) {
        // ponytail: iCloud understands IANA TZIDs; embed VTIMEZONE only if a
        // CalDAV server that does not is added.
        ical::Property::raw(name, &digits).with_param("TZID", &dt.time_zone)
    } else {
        ical::Property::raw(name, &digits)
    }
}

fn safe_tzid(zone: &str) -> bool {
    !zone.is_empty()
        && zone.len() <= 64
        && zone
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'/' | b'_' | b'-' | b'+'))
}

fn recurrence_rule(recurrence: EventRecurrence) -> &'static str {
    match recurrence {
        EventRecurrence::Daily => "FREQ=DAILY",
        EventRecurrence::Weekly => "FREQ=WEEKLY",
        EventRecurrence::Biweekly => "FREQ=WEEKLY;INTERVAL=2",
        EventRecurrence::Monthly => "FREQ=MONTHLY",
        EventRecurrence::Yearly => "FREQ=YEARLY",
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

/// Where an RSVP's PARTSTAT should be written.
#[derive(Debug)]
enum ResponseTarget {
    /// An existing VEVENT `(calendar index, vevent index)` — a one-off event, a
    /// whole-series response, or an occurrence that already has an override.
    Existing(usize, usize),
    /// A series occurrence with no override yet: synthesise one from the master
    /// at `(calendar index, master vevent index)` so only that instance changes.
    Synthesize(usize, usize),
}

/// Resolve the target for an RSVP. A one-off id (empty recurrence) responds on
/// the master/only VEVENT; a series occurrence prefers its existing override, and
/// otherwise asks for a synthesised one rather than editing the master — editing
/// the master's own PARTSTAT would move the WHOLE series' status and relay that
/// to the organiser.
fn resolve_response_target(
    calendars: &[ical::Component],
    recurrence: &str,
) -> Option<ResponseTarget> {
    if recurrence.is_empty() {
        return find_master(calendars).map(|(c, v)| ResponseTarget::Existing(c, v));
    }
    if let Some((c, v)) = find_override(calendars, recurrence) {
        return Some(ResponseTarget::Existing(c, v));
    }
    find_master(calendars).map(|(c, v)| ResponseTarget::Synthesize(c, v))
}

/// Apply an RSVP to the calendars just fetched from the resource, returning the
/// index of the VCALENDAR that changed (the one to PUT back). Pure — the decision
/// (edit an existing VEVENT, synthesise a per-occurrence override, or refuse) is
/// unit-testable without the surrounding network I/O.
fn apply_response(
    calendars: &mut [ical::Component],
    apple_id: &str,
    recurrence: &str,
    partstat: &str,
    stamp: &str,
) -> Result<usize, MailError> {
    match resolve_response_target(calendars, recurrence)
        .ok_or_else(|| MailError::Decode("event not found in its resource".into()))?
    {
        ResponseTarget::Existing(ci, vi) => {
            set_partstat(&mut calendars[ci].children[vi], apple_id, partstat)?;
            touch(&mut calendars[ci].children[vi], stamp);
            Ok(ci)
        }
        // A single occurrence of a series with no override yet: set the PARTSTAT on
        // a freshly synthesised override so only this instance responds, not the
        // whole series.
        ResponseTarget::Synthesize(ci, master_vi) => {
            // Refuse if the master's own freshly-fetched EXDATE already hides this
            // occurrence — it was deleted elsewhere and our view is stale. Writing an
            // override for it would PUT a resource that both excludes and defines the
            // slot, resurrecting the cancelled occurrence on other clients.
            if master_excludes(&calendars[ci].children[master_vi], recurrence) {
                return Err(MailError::Decode(
                    "occurrence was deleted from the series; refresh and try again".into(),
                ));
            }
            let mut over =
                build_occurrence_override(&calendars[ci].children[master_vi], recurrence)
                    .ok_or_else(|| MailError::Decode("cannot build occurrence override".into()))?;
            // Fail before the PUT if the user is not an attendee (no reply to give).
            set_partstat(&mut over, apple_id, partstat)?;
            touch(&mut over, stamp);
            calendars[ci].children.push(over);
            Ok(ci)
        }
    }
}

/// The override VEVENT whose `RECURRENCE-ID` is exactly `recurrence`, if one
/// exists. Matched by exact wall clock, deliberately NOT the tolerant
/// [`excludes_occurrence`]: a write must never bind to a *different* occurrence,
/// and the tolerant match can collide two adjacent occurrences of a high-offset
/// series across a zone-form boundary. A foreign-form override this misses simply
/// routes the RSVP to a synthesised override instead — a duplicate at worst, never
/// a wrong-occurrence write. `recurrence` and a well-formed `RECURRENCE-ID` are
/// both the occurrence's original slot in the master's own form, so they match.
fn find_override(calendars: &[ical::Component], recurrence: &str) -> Option<(usize, usize)> {
    for (ci, calendar) in calendars.iter().enumerate() {
        for (vi, child) in calendar.children.iter().enumerate() {
            let matches = child.name == "VEVENT"
                && child
                    .get("RECURRENCE-ID")
                    .and_then(|p| p.date_time())
                    .is_some_and(|dt| dt.wall == recurrence);
            if matches {
                return Some((ci, vi));
            }
        }
    }
    None
}

/// Synthesise a per-occurrence override VEVENT from the series master, so a
/// single-occurrence RSVP changes only that instance. The override is the master
/// pinned to one slot: `RRULE`/`RDATE`/`EXDATE` dropped, `RECURRENCE-ID` plus this
/// occurrence's `DTSTART`/`DTEND` set in the master's own zone form, and
/// everything else (`ATTENDEE`, `ORGANIZER`, `VALARM`, `SUMMARY`, …) inherited so
/// the reply the server relays matches the series.
fn build_occurrence_override(
    master: &ical::Component,
    recurrence: &str,
) -> Option<ical::Component> {
    let start = master.get("DTSTART").and_then(|p| p.date_time())?;
    let duration = duration_seconds(master, &start);
    let mut over = master.clone();
    over.remove("RRULE");
    over.remove("RDATE");
    over.remove("EXDATE");
    over.set(datetime_like(
        "RECURRENCE-ID",
        master.get("DTSTART"),
        recurrence,
    ));
    over.set(datetime_like("DTSTART", master.get("DTSTART"), recurrence));
    // Recompute the end only when the master pins one with DTEND; a DURATION-based
    // length is occurrence-independent and rides along on the clone untouched. The
    // end is emitted in DTSTART's zone form, not DTEND's: `end` is `recurrence` (a
    // DTSTART-form wall clock) plus the series' naive length, so DTSTART's form is
    // the frame it is actually in. Mirroring DTEND's own form instead would mislabel
    // or malform the value whenever a foreign master carries DTSTART and DTEND in
    // different forms (e.g. TZID DTSTART with a UTC-Z or all-day DTEND).
    if master.get("DTEND").is_some() {
        let end = if start.is_date {
            civil::add_days(recurrence, duration / 86_400)
        } else {
            civil::add_seconds(recurrence, duration)
        }
        .unwrap_or_else(|| recurrence.to_string());
        over.set(datetime_like("DTEND", master.get("DTSTART"), &end));
    }
    Some(over)
}

/// Build a date-time property named `name` carrying `wall`, mirroring the zone
/// form — `VALUE=DATE` / trailing `Z` / `TZID` / floating — of `template` (always
/// the master's `DTSTART`). A synthesised `RECURRENCE-ID`/`DTSTART`/`DTEND` or an
/// `EXDATE` must match the form the server's `RRULE` generates, which is what makes
/// the server recognise it as the same occurrence.
fn datetime_like(name: &str, template: Option<&ical::Property>, wall: &str) -> ical::Property {
    let digits = compact(wall);
    let is_date = template
        .and_then(|p| p.param("VALUE"))
        .is_some_and(|v| v.eq_ignore_ascii_case("DATE"));
    let is_utc = template.is_some_and(|p| p.value.trim_end().ends_with(['Z', 'z']));
    let tzid = template.and_then(|p| p.param("TZID"));

    if is_date {
        ical::Property::raw(name, digits.get(..8).unwrap_or(&digits)).with_param("VALUE", "DATE")
    } else if is_utc {
        ical::Property::raw(name, &format!("{digits}Z"))
    } else if let Some(zone) = tzid {
        ical::Property::raw(name, &digits).with_param("TZID", zone)
    } else {
        ical::Property::raw(name, &digits)
    }
}

/// Add an `EXDATE` for `recurrence` to the master, mirroring its `DTSTART`'s
/// zone form so the server matches it against the generated occurrence.
fn add_exdate(master: &mut ical::Component, recurrence: &str) {
    let exdate = datetime_like("EXDATE", master.get("DTSTART"), recurrence);
    master.push(exdate);
}

/// Drop any override VEVENT for `recurrence`, so a deleted occurrence does not
/// linger as a modified one.
fn remove_override(calendar: &mut ical::Component, recurrence: &str) {
    // Exact match, like `find_override` and for the same reason: dropping the
    // wrong occurrence's override is destructive. The master's own EXDATE (written
    // by `add_exdate`) is what actually hides the deleted occurrence; a foreign-form
    // override this leaves behind is caught by the display-side exclusion filter.
    calendar.children.retain(|child| {
        !(child.name == "VEVENT"
            && child
                .get("RECURRENCE-ID")
                .and_then(|p| p.date_time())
                .is_some_and(|dt| dt.wall == recurrence))
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
        is_recurring: event.recurrence.is_some() || !recurrence.is_empty(),
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

    // Deleted occurrences. Kept as full wall clocks and matched with
    // `excludes_occurrence`, so an EXDATE a foreign client wrote in UTC while
    // DTSTART carries a TZID still excludes its occurrence even when the zone
    // offset pushes it onto an adjacent day — otherwise a deleted occurrence
    // silently resurrects.
    let excluded = exdate_walls(master);
    let is_excluded = |wall: &str| excluded.iter().any(|ex| excludes_occurrence(ex, wall));

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
        if is_excluded(&occurrence) || !seen.insert(occurrence.clone()) {
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
        if is_excluded(key) || seen.contains(key) {
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

    /// A fixed DTSTAMP for the write-path tests, which do not care about wall time.
    const STAMP: &str = "20260730T000000Z";

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
            recurrence: None,
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
    fn a_recurring_event_keeps_its_wall_clock_zone_and_writes_an_rrule() {
        let mut event = new_event(
            "Fortnightly review",
            "2026-07-29T09:00:00",
            "2026-07-29T10:00:00",
        );
        event.start.time_zone = "Europe/London".into();
        event.end.time_zone = "Europe/London".into();
        event.recurrence = Some(wattmail_domain::EventRecurrence::Biweekly);

        let calendar = build_vcalendar(build_vevent(&event, "uid-2@wattmail", "20260729T080000Z"));
        let parsed = ical::parse(&emit::serialize(&calendar));
        let vevent = parsed[0].children_named("VEVENT").next().unwrap();

        assert_eq!(vevent.get("RRULE").unwrap().value, "FREQ=WEEKLY;INTERVAL=2");
        assert_eq!(
            vevent.get("DTSTART").unwrap().param("TZID"),
            Some("Europe/London")
        );
        assert_eq!(vevent.get("DTSTART").unwrap().value, "20260729T090000");
        for (recurrence, expected) in [
            (EventRecurrence::Daily, "FREQ=DAILY"),
            (EventRecurrence::Weekly, "FREQ=WEEKLY"),
            (EventRecurrence::Monthly, "FREQ=MONTHLY"),
            (EventRecurrence::Yearly, "FREQ=YEARLY"),
        ] {
            assert_eq!(recurrence_rule(recurrence), expected);
        }
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

    /// The `(calendar, vevent)` indices of an RSVP target that must already
    /// exist (a one-off, a whole-series response, or an existing override).
    fn existing_target(calendars: &[ical::Component], recurrence: &str) -> (usize, usize) {
        match resolve_response_target(calendars, recurrence) {
            Some(ResponseTarget::Existing(ci, vi)) => (ci, vi),
            other => panic!("expected an existing target, got {other:?}"),
        }
    }

    #[test]
    fn rsvp_sets_the_users_own_partstat_and_leaves_others_alone() {
        let mut calendars = ical::parse(SERIES);
        let (ci, vi) = existing_target(&calendars, "");
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
        let (ci, vi) = existing_target(&fresh, "");
        assert!(set_partstat(&mut fresh[ci].children[vi], "stranger@x.io", "DECLINED").is_err());
    }

    #[test]
    fn rsvp_to_a_bare_occurrence_synthesises_an_override_and_leaves_the_series() {
        // Decline the 12 Aug occurrence, which has no override of its own. It must
        // NOT touch the master (that would change the whole series' status and
        // relay a whole-series reply to the organiser).
        let mut calendars = ical::parse(SERIES);
        let recurrence = "2026-08-12T09:00:00";
        let (ci, master_vi) = match resolve_response_target(&calendars, recurrence) {
            Some(ResponseTarget::Synthesize(ci, vi)) => (ci, vi),
            other => panic!("expected Synthesize, got {other:?}"),
        };
        let mut over =
            build_occurrence_override(&calendars[ci].children[master_vi], recurrence).unwrap();
        set_partstat(&mut over, "me@icloud.com", "DECLINED").unwrap();
        calendars[ci].children.push(over);

        let parsed = ical::parse(&emit::serialize(&calendars[ci]))[0].clone();
        // Master + the pre-existing 29 Jul override + the new 12 Aug override.
        assert_eq!(parsed.children_named("VEVENT").count(), 3);

        // The MASTER's own PARTSTAT is untouched, and its rule is intact.
        let master = parsed
            .children_named("VEVENT")
            .find(|v| v.get("RECURRENCE-ID").is_none())
            .unwrap();
        let master_me = master
            .all("ATTENDEE")
            .find(|a| strip_mailto(&a.value) == "me@icloud.com")
            .unwrap();
        assert_eq!(master_me.param("PARTSTAT"), Some("NEEDS-ACTION"));
        assert!(master.get("RRULE").is_some(), "series rule preserved");

        // The new override carries the decline for just this instance, pinned to
        // the slot in DTSTART's zone form, with no recurrence rule of its own.
        let over = parsed
            .children_named("VEVENT")
            .find(|v| {
                v.get("RECURRENCE-ID")
                    .is_some_and(|p| p.value.starts_with("20260812"))
            })
            .expect("an override for 12 Aug");
        assert!(over.get("RRULE").is_none());
        assert_eq!(
            over.get("RECURRENCE-ID").unwrap().param("TZID"),
            Some("Europe/London")
        );
        assert_eq!(over.get("DTSTART").unwrap().value, "20260812T090000");
        assert_eq!(over.get("DTEND").unwrap().value, "20260812T091500");
        let over_me = over
            .all("ATTENDEE")
            .find(|a| strip_mailto(&a.value) == "me@icloud.com")
            .unwrap();
        assert_eq!(over_me.param("PARTSTAT"), Some("DECLINED"));
        // Sam's status rides along on the clone untouched.
        let over_sam = over
            .all("ATTENDEE")
            .find(|a| strip_mailto(&a.value) == "sam@example.com")
            .unwrap();
        assert_eq!(over_sam.param("PARTSTAT"), Some("ACCEPTED"));
    }

    #[test]
    fn rsvp_to_an_occurrence_that_already_has_an_override_edits_it_in_place() {
        // The 29 Jul occurrence is the moved override (children[1]); an RSVP for it
        // must land there, not synthesise a duplicate.
        let calendars = ical::parse(SERIES);
        let (ci, vi) = existing_target(&calendars, "2026-07-29T09:00:00");
        assert_eq!(ci, 0);
        assert!(calendars[ci].children[vi].get("RECURRENCE-ID").is_some());
    }

    /// A series whose 29 Jul occurrence already has an override that carries the
    /// user as an attendee — the shape needed to RSVP an existing override in place.
    const SERIES_WITH_ATTENDEE_OVERRIDE: &str = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:s\r\n\
SUMMARY:Weekly\r\n\
DTSTART;TZID=Europe/London:20260722T090000\r\n\
DTEND;TZID=Europe/London:20260722T093000\r\n\
RRULE:FREQ=WEEKLY;COUNT=4\r\n\
ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@icloud.com\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:s\r\n\
RECURRENCE-ID;TZID=Europe/London:20260729T090000\r\n\
DTSTART;TZID=Europe/London:20260729T100000\r\n\
DTEND;TZID=Europe/London:20260729T103000\r\n\
ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@icloud.com\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn rsvp_to_an_existing_override_writes_partstat_there_and_leaves_the_master() {
        // Exercises apply_response's Existing arm end to end: the PARTSTAT must land
        // on the existing override (no duplicate synthesised) and the master's own
        // status must not move.
        let mut calendars = ical::parse(SERIES_WITH_ATTENDEE_OVERRIDE);
        let ci = apply_response(
            &mut calendars,
            "me@icloud.com",
            "2026-07-29T09:00:00",
            "ACCEPTED",
            STAMP,
        )
        .unwrap();
        assert_eq!(ci, 0);
        // Edited in place — exactly one override, no duplicate.
        assert_eq!(
            calendars[0]
                .children_named("VEVENT")
                .filter(|v| v.get("RECURRENCE-ID").is_some())
                .count(),
            1
        );
        let over = calendars[0]
            .children
            .iter()
            .find(|v| v.get("RECURRENCE-ID").is_some())
            .unwrap();
        assert_eq!(
            over.all("ATTENDEE")
                .find(|a| strip_mailto(&a.value) == "me@icloud.com")
                .unwrap()
                .param("PARTSTAT"),
            Some("ACCEPTED")
        );
        // The master's own PARTSTAT is untouched.
        let master = calendars[0]
            .children
            .iter()
            .find(|v| v.name == "VEVENT" && v.get("RECURRENCE-ID").is_none())
            .unwrap();
        assert_eq!(
            master
                .all("ATTENDEE")
                .find(|a| strip_mailto(&a.value) == "me@icloud.com")
                .unwrap()
                .param("PARTSTAT"),
            Some("NEEDS-ACTION")
        );
    }

    #[test]
    fn rsvp_to_an_occurrence_the_master_already_excludes_is_refused_not_resurrected() {
        // 5 Aug is EXDATE'd on the SERIES master (deleted elsewhere). A stale client
        // that still shows the card and RSVPs it must be refused — synthesising an
        // override would PUT a resource that both excludes and defines 5 Aug,
        // resurrecting the cancelled occurrence on other clients.
        let mut calendars = ical::parse(SERIES);
        let err = apply_response(
            &mut calendars,
            "me@icloud.com",
            "2026-08-05T09:00:00",
            "DECLINED",
            STAMP,
        )
        .unwrap_err();
        assert!(matches!(err, MailError::Decode(_)));
        // No override was written for the excluded slot: master + the pre-existing
        // 29 Jul override only.
        assert_eq!(calendars[0].children_named("VEVENT").count(), 2);
        assert!(find_override(&calendars, "2026-08-05T09:00:00").is_none());
    }

    #[test]
    fn build_occurrence_override_pins_an_all_day_series_occurrence() {
        // The all-day (VALUE=DATE) branch: DTSTART and DTEND stay bare dates (via
        // add_days), the two-day span is preserved, and no time or TZID appears.
        let data = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:allday\r\n\
SUMMARY:Sprint\r\n\
DTSTART;VALUE=DATE:20260706\r\n\
DTEND;VALUE=DATE:20260708\r\n\
RRULE:FREQ=WEEKLY;COUNT=4\r\n\
ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@icloud.com\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let calendars = ical::parse(data);
        let over = build_occurrence_override(&calendars[0].children[0], "2026-07-20").unwrap();
        assert!(over.get("RRULE").is_none());
        let dtstart = over.get("DTSTART").unwrap();
        assert_eq!(dtstart.value, "20260720");
        assert_eq!(dtstart.param("VALUE"), Some("DATE"));
        let dtend = over.get("DTEND").unwrap();
        // 6→8 Jul is a two-day span, so 20→22 Jul, still a bare DATE with no TZID.
        assert_eq!(dtend.value, "20260722");
        assert_eq!(dtend.param("VALUE"), Some("DATE"));
        assert!(dtend.param("TZID").is_none());
        let rid = over.get("RECURRENCE-ID").unwrap();
        assert_eq!(rid.value, "20260720");
        assert_eq!(rid.param("VALUE"), Some("DATE"));
    }

    #[test]
    fn build_occurrence_override_keeps_a_duration_based_length_untouched() {
        // A DURATION-based master (no DTEND): the override inherits the DURATION on
        // the clone and must not grow a DTEND of its own.
        let data = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:dur\r\n\
SUMMARY:Call\r\n\
DTSTART;TZID=Europe/London:20260706T090000\r\n\
DURATION:PT45M\r\n\
RRULE:FREQ=WEEKLY;COUNT=4\r\n\
ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@icloud.com\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let calendars = ical::parse(data);
        let over =
            build_occurrence_override(&calendars[0].children[0], "2026-07-20T09:00:00").unwrap();
        assert!(over.get("DTEND").is_none(), "no DTEND synthesised");
        assert_eq!(over.get("DURATION").unwrap().value, "PT45M");
        assert_eq!(over.get("DTSTART").unwrap().value, "20260720T090000");
        assert!(over.get("RRULE").is_none());
    }

    #[test]
    fn build_occurrence_override_emits_dtend_in_dtstarts_frame_for_a_mixed_zone_master() {
        // A foreign producer wrote DTSTART in a TZID but DTEND in UTC-Z form. With no
        // tz database the naive duration is only meaningful in DTSTART's frame, so the
        // occurrence's DTEND must be emitted in that same frame — a valid, consistent
        // property — not relabelled with DTEND's own Z form (which mis-shifts the end).
        let data = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:mixed\r\n\
SUMMARY:Sync\r\n\
DTSTART;TZID=America/New_York:20260706T090000\r\n\
DTEND:20260706T130000Z\r\n\
RRULE:FREQ=WEEKLY;COUNT=4\r\n\
ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@icloud.com\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let calendars = ical::parse(data);
        let over =
            build_occurrence_override(&calendars[0].children[0], "2026-07-20T09:00:00").unwrap();
        let dtend = over.get("DTEND").unwrap();
        // 09:00 + naive 4h = 13:00, emitted in DTSTART's TZID frame, not a bare Z.
        assert_eq!(dtend.value, "20260720T130000");
        assert_eq!(dtend.param("TZID"), Some("America/New_York"));
        assert!(!dtend.value.ends_with('Z'));
        assert_eq!(
            over.get("DTSTART").unwrap().param("TZID"),
            Some("America/New_York")
        );
    }

    #[test]
    fn excludes_occurrence_matches_a_cross_form_pair_across_midnight_but_not_adjacent_days() {
        // The same instant either side of local midnight (a 00:20 TZID occurrence
        // vs its UTC form on the previous day) — must match.
        assert!(excludes_occurrence(
            "2026-07-12T23:20:00",
            "2026-07-13T00:20:00"
        ));
        // The same calendar day always matches.
        assert!(excludes_occurrence(
            "2026-07-13T00:20:00",
            "2026-07-13T09:00:00"
        ));
        // Two genuinely different days of a daily series (24h apart) must NOT
        // collapse into one, or a single exclusion would over-hide.
        assert!(!excludes_occurrence(
            "2026-07-13T09:00:00",
            "2026-07-14T09:00:00"
        ));
        // All-day dates on different days never match.
        assert!(!excludes_occurrence("2026-07-13", "2026-07-14"));
    }

    #[test]
    fn a_foreign_form_override_never_hijacks_a_write_for_an_adjacent_occurrence() {
        // Brisbane is UTC+10 (no DST). A daily series; a foreign client moved the
        // 11 Jul occurrence, writing its RECURRENCE-ID in UTC form
        // (20260710T230000Z = 11 Jul 09:00 Brisbane). Its wall lands on 10 Jul — the
        // SAME day_key as the 10 Jul occurrence, so a tolerant match would collide
        // the two. A write must not.
        let data = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:series-bne\r\n\
SUMMARY:Daily\r\n\
DTSTART;TZID=Australia/Brisbane:20260710T090000\r\n\
DTEND;TZID=Australia/Brisbane:20260710T093000\r\n\
RRULE:FREQ=DAILY;COUNT=5\r\n\
ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@icloud.com\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:series-bne\r\n\
RECURRENCE-ID:20260710T230000Z\r\n\
SUMMARY:Daily (moved)\r\n\
DTSTART;TZID=Australia/Brisbane:20260711T110000\r\n\
DTEND;TZID=Australia/Brisbane:20260711T113000\r\n\
ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@icloud.com\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let calendars = ical::parse(data);

        // RSVP to the plain 10 Jul occurrence must SYNTHESISE — never bind the
        // 11 Jul override just because their day_keys coincide.
        match resolve_response_target(&calendars, "2026-07-10T09:00:00") {
            Some(ResponseTarget::Synthesize(..)) => {}
            other => panic!("10 Jul must synthesise, not hijack the 11 Jul override: {other:?}"),
        }

        // Deleting the 10 Jul occurrence must not drop the 11 Jul override.
        let mut cal = calendars[0].clone();
        remove_override(&mut cal, "2026-07-10T09:00:00");
        assert_eq!(
            cal.children_named("VEVENT")
                .filter(|v| v.get("RECURRENCE-ID").is_some())
                .count(),
            1,
            "the 11 Jul override must survive a 10 Jul delete"
        );

        // The override's own (foreign-form) id still binds to it exactly.
        match resolve_response_target(&calendars, "2026-07-10T23:00:00") {
            Some(ResponseTarget::Existing(_, vi)) => {
                assert!(calendars[0].children[vi].get("RECURRENCE-ID").is_some());
            }
            other => panic!("the override's own id must bind to it: {other:?}"),
        }
    }

    #[test]
    fn an_exdate_crossing_local_midnight_still_deletes_its_occurrence() {
        // The series starts at 00:20 London (BST = UTC+1), so 13 Jul 00:20 local is
        // 12 Jul 23:20 UTC — a day earlier. A foreign client's UTC EXDATE lands on
        // the 12th; day-only matching would miss it and resurrect the occurrence.
        let data = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\n\
UID:series-late\r\n\
SUMMARY:Late standup\r\n\
DTSTART;TZID=Europe/London:20260706T002000\r\n\
DTEND;TZID=Europe/London:20260706T003000\r\n\
RRULE:FREQ=WEEKLY;COUNT=3\r\n\
EXDATE:20260712T232000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let starts: Vec<_> = view(data, "2026-07-01T00:00:00", "2026-08-01T00:00:00")
            .into_iter()
            .map(|e| e.start.date_time)
            .collect();
        // Occurrences 6/13/20 Jul at 00:20; the 13th is the excluded one.
        assert_eq!(starts, ["2026-07-06T00:20:00", "2026-07-20T00:20:00"]);
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
