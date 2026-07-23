//! WebDAV/CalDAV wire format: the request bodies WattMail sends and a parser for
//! the `multistatus` (207) documents it gets back.
//!
//! Pure string/XML work — no HTTP, no credentials — so every shape here is unit
//! testable against captured responses.
//!
//! Element matching is by **namespace URI plus local name**, never by prefix:
//! the prefix is the server's choice (`D:`/`d:`/`a:`) and Apple, Google and
//! Fastmail all pick different ones, so prefix matching silently stops working
//! the moment the server changes its mind.

use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::NsReader;
use url::Url;
use wattmail_domain::MailError;

use super::civil;

pub const NS_DAV: &str = "DAV:";
pub const NS_CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
pub const NS_APPLE: &str = "http://apple.com/ns/ical/";

/// `PROPFIND` body asking who the authenticated user is. Sent with `Depth: 0`.
pub const PROPFIND_CURRENT_USER_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:"><D:prop><D:current-user-principal/></D:prop></D:propfind>"#;

/// `PROPFIND` body asking a principal where its calendars live. `Depth: 0`.
pub const PROPFIND_CALENDAR_HOME: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><C:calendar-home-set/><D:displayname/></D:prop></D:propfind>"#;

/// `PROPFIND` body listing the calendar home's children. Sent with `Depth: 1`.
///
/// `calendar-color` and `getctag` are Apple/CalendarServer extensions rather
/// than RFC 4791 properties; iCloud is their origin implementation, and a server
/// that does not know a requested property simply reports it as 404 inside its
/// own `propstat` block, so asking costs nothing.
pub const PROPFIND_CALENDARS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:CS="http://calendarserver.org/ns/" xmlns:A="http://apple.com/ns/ical/"><D:prop><D:resourcetype/><D:displayname/><A:calendar-color/><C:supported-calendar-component-set/><CS:getctag/><D:current-user-privilege-set/></D:prop></D:propfind>"#;

/// `REPORT calendar-query` body selecting every VEVENT overlapping a window.
///
/// Deliberately **no** `<C:expand>`: iCloud's expansion is documented as
/// returning inconsistent results for identical requests (Apple Developer Forums
/// thread 94363), so WattMail fetches the master `VEVENT` with its `RRULE` and
/// expands client-side, which is also what DAVx5 does.
///
/// `start` is inclusive and `end` exclusive (RFC 4791 §9.9), both compact UTC.
pub fn calendar_query(start: &str, end: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><D:getetag/><C:calendar-data/></D:prop><C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT"><C:time-range start="{start}" end="{end}"/></C:comp-filter></C:comp-filter></C:filter></C:calendar-query>"#
    )
}

/// One `<D:response>` element, flattened to the properties WattMail asks for.
///
/// A single struct rather than a generic DOM: four request shapes share it, each
/// reading the two or three fields it cares about, and the fields a given
/// request never asks for simply stay `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DavResponse {
    /// The resource this response describes, exactly as the server wrote it
    /// (may be relative — resolve with [`resolve_href`]).
    pub href: String,
    pub display_name: Option<String>,
    pub color: Option<String>,
    pub etag: Option<String>,
    pub ctag: Option<String>,
    /// The raw iCalendar body from `<C:calendar-data>`.
    pub calendar_data: Option<String>,
    /// `(namespace, local name)` of every child of `<D:resourcetype>`.
    pub resource_types: Vec<(String, String)>,
    /// `name=` of every `<C:comp>` in `supported-calendar-component-set`.
    pub components: Vec<String>,
    /// The href inside `<D:current-user-principal>`.
    pub principal_href: Option<String>,
    /// The href inside `<C:calendar-home-set>`.
    pub home_href: Option<String>,
    /// Whether the server returned a `current-user-privilege-set` at all.
    pub privileges_reported: bool,
    /// Whether that set granted a write privilege.
    pub can_write: bool,
}

impl DavResponse {
    /// True when this collection is a CalDAV calendar. Namespace-checked, so a
    /// scheduling inbox/outbox or a plain collection never matches.
    pub fn is_calendar(&self) -> bool {
        self.resource_types
            .iter()
            .any(|(ns, name)| ns == NS_CALDAV && name == "calendar")
    }

    /// Whether new events can be created here. A subscribed feed (a holiday
    /// calendar) reports a privilege set without write; a calendar that omits
    /// the property is assumed writable, so an owned calendar is never hidden
    /// just because the server was terse.
    pub fn writable(&self) -> bool {
        !self.privileges_reported || self.can_write
    }

    /// True when the collection accepts `VEVENT`.
    ///
    /// An absent `supported-calendar-component-set` means "all components"
    /// (RFC 4791 §5.2.3), so silence counts as support — refusing it would hide
    /// real calendars from servers that omit the property.
    pub fn supports_events(&self) -> bool {
        self.components.is_empty()
            || self
                .components
                .iter()
                .any(|c| c.eq_ignore_ascii_case("VEVENT"))
    }
}

/// Parse a `multistatus` document into one entry per `<D:response>`.
///
/// `propstat` status codes are ignored: a property the server reports as 404
/// arrives empty anyway, and dropping a whole response because one optional
/// extension property was missing would lose real calendars.
pub fn parse_multistatus(xml: &str) -> Result<Vec<DavResponse>, MailError> {
    let mut reader = NsReader::from_str(xml);
    let mut responses = Vec::new();
    let mut current = DavResponse::default();
    let mut in_response = false;
    // (namespace, local name) of every open element, outermost first.
    let mut path: Vec<(String, String)> = Vec::new();
    let mut text = String::new();

    loop {
        let (ns, event) = reader
            .read_resolved_event()
            .map_err(|e| MailError::Decode(format!("malformed CalDAV XML: {e}")))?;
        let namespace = match ns {
            ResolveResult::Bound(n) => String::from_utf8_lossy(n.as_ref()).into_owned(),
            _ => String::new(),
        };

        match event {
            Event::Start(e) => {
                let local = local_name(e.local_name().as_ref());
                if namespace == NS_DAV && local == "response" {
                    in_response = true;
                    current = DavResponse::default();
                }
                collect_marker(&namespace, &local, &path, &e, &mut current);
                path.push((namespace, local));
                text.clear();
            }
            Event::Empty(e) => {
                // Self-closing elements never produce an End, so anything read
                // from the tag itself must be collected here too.
                let local = local_name(e.local_name().as_ref());
                collect_marker(&namespace, &local, &path, &e, &mut current);
            }
            Event::Text(e) => {
                text.push_str(&decode(e.xml10_content())?);
            }
            Event::CData(e) => {
                // calendar-data is sometimes wrapped in CDATA, which by
                // definition carries no entity references.
                text.push_str(&decode(e.xml10_content())?);
            }
            Event::GeneralRef(e) => {
                // quick-xml reports `&amp;` and friends as their own events
                // rather than inlining them, so text has to be reassembled here
                // — dropping these would silently mangle any name with an `&`.
                if let Some(c) = e
                    .resolve_char_ref()
                    .map_err(|err| MailError::Decode(format!("bad XML reference: {err}")))?
                {
                    text.push(c);
                } else if let Some(c) = predefined_entity(&decode(e.decode())?) {
                    text.push(c);
                }
            }
            Event::End(_) => {
                let Some((ns, local)) = path.pop() else {
                    continue;
                };
                if in_response {
                    let parent = path.last().map(|(_, l)| l.as_str()).unwrap_or("");
                    store_text(&ns, &local, parent, &text, &mut current);
                    if ns == NS_DAV && local == "response" {
                        responses.push(std::mem::take(&mut current));
                        in_response = false;
                    }
                }
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(responses)
}

/// Record the elements whose meaning is carried by the tag rather than by text:
/// `resourcetype` children and `supported-calendar-component-set` entries.
fn collect_marker(
    namespace: &str,
    local: &str,
    path: &[(String, String)],
    tag: &quick_xml::events::BytesStart<'_>,
    current: &mut DavResponse,
) {
    let parent = path.last().map(|(_, l)| l.as_str()).unwrap_or("");
    if parent == "resourcetype" {
        current
            .resource_types
            .push((namespace.to_string(), local.to_string()));
        return;
    }
    if namespace == NS_CALDAV && local == "comp" {
        if let Ok(Some(attr)) = tag.try_get_attribute("name") {
            if let Ok(value) = attr.normalized_value(quick_xml::XmlVersion::Explicit1_0) {
                current.components.push(value.into_owned());
            }
        }
    }
    // Writability, from `current-user-privilege-set`. Its presence alone marks
    // the server as having reported privileges; a `write` (or blanket `all`)
    // privilege inside it grants creation.
    if namespace == NS_DAV && local == "current-user-privilege-set" {
        current.privileges_reported = true;
    }
    let in_privilege = path.iter().any(|(ns, l)| ns == NS_DAV && l == "privilege");
    if in_privilege
        && namespace == NS_DAV
        && matches!(local, "write" | "write-content" | "all" | "bind")
    {
        current.can_write = true;
    }
}

/// Assign an element's accumulated text to the field it belongs to.
fn store_text(namespace: &str, local: &str, parent: &str, text: &str, current: &mut DavResponse) {
    let value = text.trim();
    if value.is_empty() {
        return;
    }
    match (namespace, local) {
        // `href` means different things depending on what encloses it: the
        // resource itself directly under `response`, or a property value.
        (NS_DAV, "href") => match parent {
            "response" => {
                if current.href.is_empty() {
                    current.href = value.to_string();
                }
            }
            "current-user-principal" => current.principal_href = Some(value.to_string()),
            "calendar-home-set" => current.home_href = Some(value.to_string()),
            _ => {}
        },
        (NS_DAV, "displayname") => current.display_name = Some(value.to_string()),
        (NS_DAV, "getetag") => current.etag = Some(value.to_string()),
        (NS_APPLE, "calendar-color") => current.color = Some(value.to_string()),
        (NS_CALDAV, "calendar-data") => current.calendar_data = Some(text.to_string()),
        (_, "getctag") => current.ctag = Some(value.to_string()),
        _ => {}
    }
}

/// Lift a quick-xml decode result into [`MailError`].
fn decode(
    result: Result<std::borrow::Cow<'_, str>, quick_xml::encoding::EncodingError>,
) -> Result<std::borrow::Cow<'_, str>, MailError> {
    result.map_err(|e| MailError::Decode(format!("bad XML encoding: {e}")))
}

/// The five entities XML predefines; anything else is a DTD entity we neither
/// declare nor expect from a CalDAV server.
fn predefined_entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => None,
    }
}

fn local_name(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).to_ascii_lowercase()
}

/// Resolve a possibly-relative href from a `multistatus` against the request URL.
pub fn resolve_href(base: &Url, href: &str) -> Result<Url, MailError> {
    base.join(href.trim())
        .map_err(|e| MailError::Decode(format!("bad href {href:?}: {e}")))
}

/// Convert an absolute ISO-8601 instant into the compact UTC form a CalDAV
/// `time-range` filter requires (`YYYYMMDDTHHMMSSZ`), shifted by `slop_days`.
///
/// The slop widens the server-side window. Occurrences are compared in their own
/// wall-clock afterwards, and a zoned wall clock can sit up to 14 hours either
/// side of the UTC instant it represents, so a one-day margin guarantees nothing
/// in view is ever filtered out server-side. Over-fetching is free: the frontend
/// buckets strictly by day and simply never renders the extras.
pub fn to_caldav_instant(iso: &str, slop_days: i64) -> Option<String> {
    let (date, time) = to_utc_wall(iso, slop_days)?
        .split_once('T')
        .map(|(d, t)| (d.to_string(), t.to_string()))?;
    Some(format!(
        "{}T{}Z",
        date.replace('-', ""),
        time.replace(':', "")
    ))
}

/// The same normalisation as [`to_caldav_instant`] but left as a wall-clock
/// string, which is the space the recurrence expander works in.
pub fn to_utc_wall(iso: &str, slop_days: i64) -> Option<String> {
    let raw = iso.trim();
    // Split the instant from its zone designator.
    let (wall, offset_seconds) = if let Some(body) = raw.strip_suffix('Z').or(raw.strip_suffix('z'))
    {
        (body, 0i64)
    } else {
        let split = raw.rfind(['+', '-']).filter(|&i| i > 10)?;
        let (body, offset) = raw.split_at(split);
        (body, parse_offset(offset)?)
    };

    // Drop any fractional seconds, then normalise to UTC. Subtracting a fixed
    // offset is plain arithmetic — no timezone database involved.
    let wall = wall.split('.').next()?;
    let utc = civil::add_seconds(wall, -offset_seconds)?;
    civil::add_days(&utc, slop_days)
}

/// Parse a `+HH:MM` / `-HHMM` UTC offset into seconds east of UTC.
fn parse_offset(raw: &str) -> Option<i64> {
    let sign = match raw.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits: String = raw[1..].chars().filter(char::is_ascii_digit).collect();
    if digits.len() != 4 {
        return None;
    }
    let hours: i64 = digits.get(..2)?.parse().ok()?;
    let minutes: i64 = digits.get(2..)?.parse().ok()?;
    Some(sign * (hours * 3_600 + minutes * 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A calendar-home listing shaped like iCloud's: server-chosen prefixes that
    /// are NOT the ones in our request bodies, a scheduling inbox that must be
    /// filtered out, and a VTODO-only list that must not appear as a calendar.
    const CALENDAR_HOME: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:ic="http://apple.com/ns/ical/" xmlns:cs="http://calendarserver.org/ns/">
  <response>
    <href>/1234567/calendars/</href>
    <propstat><prop><resourcetype><collection/></resourcetype></prop><status>HTTP/1.1 200 OK</status></propstat>
  </response>
  <response>
    <href>/1234567/calendars/home/</href>
    <propstat><prop>
      <resourcetype><collection/><cal:calendar/></resourcetype>
      <displayname>Home &amp; Family</displayname>
      <ic:calendar-color>#34AADC</ic:calendar-color>
      <cal:supported-calendar-component-set><cal:comp name="VEVENT"/></cal:supported-calendar-component-set>
      <cs:getctag>HwoQEgwAAAh4</cs:getctag>
    </prop><status>HTTP/1.1 200 OK</status></propstat>
  </response>
  <response>
    <href>/1234567/calendars/tasks/</href>
    <propstat><prop>
      <resourcetype><collection/><cal:calendar/></resourcetype>
      <displayname>Reminders</displayname>
      <cal:supported-calendar-component-set><cal:comp name="VTODO"/></cal:supported-calendar-component-set>
    </prop><status>HTTP/1.1 200 OK</status></propstat>
  </response>
  <response>
    <href>/1234567/calendars/inbox/</href>
    <propstat><prop><resourcetype><collection/><cal:schedule-inbox/></resourcetype></prop><status>HTTP/1.1 200 OK</status></propstat>
  </response>
</multistatus>"#;

    #[test]
    fn calendar_home_listing_keeps_only_real_event_calendars() {
        let responses = parse_multistatus(CALENDAR_HOME).unwrap();
        assert_eq!(responses.len(), 4);

        let calendars: Vec<_> = responses
            .iter()
            .filter(|r| r.is_calendar() && r.supports_events())
            .collect();
        assert_eq!(
            calendars.len(),
            1,
            "inbox, plain collection and VTODO list excluded"
        );

        let home = calendars[0];
        assert_eq!(home.href, "/1234567/calendars/home/");
        // Entity decoded, and read despite the server using a default namespace
        // plus prefixes that differ from the ones we sent.
        assert_eq!(home.display_name.as_deref(), Some("Home & Family"));
        assert_eq!(home.color.as_deref(), Some("#34AADC"));
        assert_eq!(home.ctag.as_deref(), Some("HwoQEgwAAAh4"));
        assert_eq!(home.components, ["VEVENT"]);
    }

    #[test]
    fn writability_comes_from_the_privilege_set() {
        // An owned calendar: privilege set present, with write.
        let owned = r#"<multistatus xmlns="DAV:">
  <response><href>/c/own/</href><propstat><prop>
    <current-user-privilege-set>
      <privilege><read/></privilege>
      <privilege><write/></privilege>
    </current-user-privilege-set>
  </prop></propstat></response></multistatus>"#;
        assert!(parse_multistatus(owned).unwrap()[0].writable());

        // A subscribed feed: privileges reported, but read-only.
        let subscribed = r#"<multistatus xmlns="DAV:">
  <response><href>/c/holidays/</href><propstat><prop>
    <current-user-privilege-set>
      <privilege><read/></privilege>
    </current-user-privilege-set>
  </prop></propstat></response></multistatus>"#;
        assert!(!parse_multistatus(subscribed).unwrap()[0].writable());

        // A server that omits the property is assumed writable.
        let terse = r#"<multistatus xmlns="DAV:">
  <response><href>/c/x/</href><propstat><prop><displayname>X</displayname></prop></propstat></response></multistatus>"#;
        assert!(parse_multistatus(terse).unwrap()[0].writable());
    }

    #[test]
    fn resourcetype_matching_is_namespace_qualified() {
        // A collection whose resourcetype has `calendar` in the WRONG namespace
        // must not be treated as a CalDAV calendar.
        let xml = r#"<multistatus xmlns="DAV:" xmlns:x="http://example.com/ns/">
  <response><href>/fake/</href><propstat><prop>
    <resourcetype><collection/><x:calendar/></resourcetype>
  </prop></propstat></response></multistatus>"#;
        let responses = parse_multistatus(xml).unwrap();
        assert!(!responses[0].is_calendar());
    }

    #[test]
    fn nested_hrefs_are_distinguished_from_the_resource_href() {
        let xml = r#"<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:response><D:href>/1234567/principal/</D:href><D:propstat><D:prop>
    <D:current-user-principal><D:href>/1234567/principal/</D:href></D:current-user-principal>
    <C:calendar-home-set><D:href>https://p42-caldav.icloud.com/1234567/calendars/</D:href></C:calendar-home-set>
  </D:prop></D:propstat></D:response></D:multistatus>"#;
        let r = &parse_multistatus(xml).unwrap()[0];
        assert_eq!(r.href, "/1234567/principal/");
        assert_eq!(r.principal_href.as_deref(), Some("/1234567/principal/"));
        assert_eq!(
            r.home_href.as_deref(),
            Some("https://p42-caldav.icloud.com/1234567/calendars/")
        );
    }

    #[test]
    fn calendar_data_survives_entities_and_cdata() {
        let xml = r#"<multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <response><href>/c/e1.ics</href><propstat><prop>
    <getetag>"abc123"</getetag>
    <C:calendar-data>BEGIN:VCALENDAR
SUMMARY:Tea &amp; biscuits &lt;important&gt;
END:VCALENDAR
</C:calendar-data>
  </prop></propstat></response></multistatus>"#;
        let r = &parse_multistatus(xml).unwrap()[0];
        assert_eq!(r.etag.as_deref(), Some("\"abc123\""));
        let data = r.calendar_data.as_deref().unwrap();
        assert!(
            data.contains("SUMMARY:Tea & biscuits <important>"),
            "{data}"
        );
    }

    #[test]
    fn time_range_bounds_normalise_to_compact_utc_with_slop() {
        // The exact shape JS `new Date(...).toISOString()` produces.
        assert_eq!(
            to_caldav_instant("2026-07-22T13:00:00.000Z", 0).unwrap(),
            "20260722T130000Z"
        );
        // Slop widens the window and rolls the month boundary correctly.
        assert_eq!(
            to_caldav_instant("2026-08-01T00:00:00.000Z", -1).unwrap(),
            "20260731T000000Z"
        );
        assert_eq!(
            to_caldav_instant("2026-07-31T23:00:00Z", 1).unwrap(),
            "20260801T230000Z"
        );
        // A bound carrying an offset is normalised to UTC, not truncated.
        assert_eq!(
            to_caldav_instant("2026-07-22T14:00:00+01:00", 0).unwrap(),
            "20260722T130000Z"
        );
        assert_eq!(
            to_caldav_instant("2026-07-22T09:00:00-0500", 0).unwrap(),
            "20260722T140000Z"
        );
        for bad in ["", "not-a-date", "2026-07-22"] {
            assert!(to_caldav_instant(bad, 0).is_none(), "should reject {bad:?}");
        }
    }

    #[test]
    fn hrefs_resolve_against_the_request_url() {
        let base = Url::parse("https://p42-caldav.icloud.com/1234567/calendars/").unwrap();
        assert_eq!(
            resolve_href(&base, "/1234567/calendars/home/")
                .unwrap()
                .as_str(),
            "https://p42-caldav.icloud.com/1234567/calendars/home/"
        );
        assert_eq!(
            resolve_href(&base, "home/e1.ics").unwrap().as_str(),
            "https://p42-caldav.icloud.com/1234567/calendars/home/e1.ics"
        );
    }

    #[test]
    fn query_body_embeds_the_window_and_never_asks_for_expansion() {
        let body = calendar_query("20260701T000000Z", "20260801T000000Z");
        assert!(body.contains(r#"<C:time-range start="20260701T000000Z" end="20260801T000000Z"/>"#));
        assert!(!body.contains("expand"), "iCloud's expand is unreliable");
    }
}
