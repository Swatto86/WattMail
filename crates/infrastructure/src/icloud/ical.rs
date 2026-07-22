//! Minimal RFC 5545 (iCalendar) reader for the CalDAV backend.
//!
//! Hand-rolled rather than taken from a crate. The two candidates both needed
//! this much wrapping anyway: `icalendar` drags in chrono + uuid + iso8601 as
//! hard dependencies (our domain speaks ISO strings and the tree has no chrono),
//! and `ical` has been archived since 2024-08-17, folds by `char` rather than by
//! octet, leaves property *values* escaped, and implements no RFC 6868.
//!
//! Scope is deliberately only what the calendar read path needs: line unfolding,
//! property tokenizing, text unescaping, and date-time typing. **Nothing here
//! converts a time zone** — the Rust tree has no tz database, so each value is
//! carried through with the zone iCalendar stated (`Z`, a `TZID`, or floating)
//! and the frontend resolves it against the browser's IANA data.
//!
//! Emitting iCalendar is milestone 2's problem and is not built here.

/// A parsed content line: `NAME;PARAM=value:VALUE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Property {
    /// Upper-cased property name (`SUMMARY`, `DTSTART`, …).
    pub name: String,
    /// Upper-cased parameter names with their unquoted, RFC 6868-decoded values.
    params: Vec<(String, String)>,
    /// The value exactly as it arrived — still escaped. Use [`Property::text`].
    pub value: String,
}

impl Property {
    /// The value of parameter `name` (case-insensitive), if present.
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The value with RFC 5545 TEXT escapes resolved.
    pub fn text(&self) -> String {
        unescape_text(&self.value)
    }

    /// The value read as a DATE or DATE-TIME, honouring a `TZID` parameter.
    pub fn date_time(&self) -> Option<IcalDateTime> {
        IcalDateTime::parse(&self.value, self.param("TZID"))
    }
}

/// One iCalendar component (`VCALENDAR`, `VEVENT`, `VTIMEZONE`, …).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Component {
    /// Upper-cased component name.
    pub name: String,
    pub properties: Vec<Property>,
    pub children: Vec<Component>,
}

impl Component {
    /// The first property named `name`.
    pub fn get(&self, name: &str) -> Option<&Property> {
        self.properties.iter().find(|p| p.name == name)
    }

    /// Every property named `name`, in document order (`EXDATE` repeats).
    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Property> + 'a {
        self.properties.iter().filter(move |p| p.name == name)
    }

    /// Every direct child component named `name`.
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Component> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// The unescaped text of property `name`, or an empty string when absent.
    pub fn text(&self, name: &str) -> String {
        self.get(name).map(Property::text).unwrap_or_default()
    }
}

/// A date-time exactly as iCalendar expressed it — deliberately **not**
/// converted, because the Rust side has no timezone database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcalDateTime {
    /// `YYYY-MM-DDTHH:MM:SS`, or `YYYY-MM-DD` when [`is_date`](Self::is_date).
    pub wall: String,
    /// True for a date-only (all-day) value.
    pub is_date: bool,
    /// `Some("UTC")` for a trailing `Z`, `Some(tzid)` for a `TZID=` parameter,
    /// `None` for a floating time (no zone stated).
    pub zone: Option<String>,
}

impl IcalDateTime {
    /// Parse an iCalendar DATE (`20260722`) or DATE-TIME (`20260722T140000`,
    /// optionally `Z`-suffixed) value. Returns `None` for anything malformed, so
    /// one bad property drops one event rather than the whole agenda.
    pub fn parse(value: &str, tzid: Option<&str>) -> Option<Self> {
        let raw = value.trim();
        let (body, is_utc) = match raw.strip_suffix('Z') {
            Some(rest) => (rest, true),
            None => (raw, false),
        };

        let date = digits(body.get(..8)?)?;
        let wall_date = format!("{}-{}-{}", &date[0..4], &date[4..6], &date[6..8]);

        match body.get(8..) {
            None | Some("") => Some(Self {
                wall: wall_date,
                is_date: true,
                // A date has no zone by definition; all-day values must never be
                // shifted, and carrying a zone here is what invites that.
                zone: None,
            }),
            Some(rest) => {
                let time = digits(rest.strip_prefix('T')?.get(..6)?)?;
                Some(Self {
                    wall: format!(
                        "{wall_date}T{}:{}:{}",
                        &time[0..2],
                        &time[2..4],
                        &time[4..6]
                    ),
                    is_date: false,
                    zone: if is_utc {
                        Some("UTC".to_string())
                    } else {
                        tzid.map(str::to_string)
                    },
                })
            }
        }
    }
}

/// Parse an iCalendar DURATION (`P1DT2H30M`, `-PT15M`, `P2W`) into seconds.
///
/// Only the units RFC 5545 §3.3.6 allows appear here (weeks/days/hours/minutes/
/// seconds — never months or years), so a plain second count is exact.
pub fn parse_duration(raw: &str) -> Option<i64> {
    let (sign, rest) = match raw.trim() {
        r if r.starts_with('-') => (-1i64, &r[1..]),
        r => (1i64, r.strip_prefix('+').unwrap_or(r)),
    };
    let mut rest = rest.strip_prefix('P')?;

    let mut seconds = 0i64;
    let mut in_time = false;
    let mut number = String::new();
    while let Some(c) = rest.chars().next() {
        rest = &rest[c.len_utf8()..];
        match c {
            'T' => in_time = true,
            '0'..='9' => number.push(c),
            unit => {
                let n: i64 = number.parse().ok()?;
                number.clear();
                let scale = match (unit, in_time) {
                    ('W', false) => 7 * 86_400,
                    ('D', false) => 86_400,
                    ('H', true) => 3_600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return None,
                };
                seconds = seconds.checked_add(n.checked_mul(scale)?)?;
            }
        }
    }
    // A trailing number with no unit is malformed.
    number.is_empty().then_some(sign * seconds)
}

/// Parse an iCalendar stream into its top-level components.
///
/// Deliberately lenient: a malformed line is skipped and an unterminated
/// component still yields what it contained. Remote calendar data is display
/// input, and losing one broken event beats blanking the user's whole agenda.
/// Transport-level failures are already caught by the HTTP status check.
pub fn parse(input: &str) -> Vec<Component> {
    let mut stack: Vec<Component> = Vec::new();
    let mut out: Vec<Component> = Vec::new();

    for line in unfold(input) {
        let Some(property) = parse_property(&line) else {
            continue;
        };
        match property.name.as_str() {
            "BEGIN" => stack.push(Component {
                name: property.value.trim().to_ascii_uppercase(),
                ..Default::default()
            }),
            "END" => close(&mut stack, &mut out),
            _ => {
                if let Some(current) = stack.last_mut() {
                    current.properties.push(property);
                }
            }
        }
    }

    // Close anything the stream left open (a truncated response still shows the
    // events it did carry).
    while !stack.is_empty() {
        close(&mut stack, &mut out);
    }
    out
}

/// Pop the innermost open component into its parent, or into the output.
fn close(stack: &mut Vec<Component>, out: &mut Vec<Component>) {
    let Some(done) = stack.pop() else {
        return;
    };
    match stack.last_mut() {
        Some(parent) => parent.children.push(done),
        None => out.push(done),
    }
}

/// Undo RFC 5545 line folding: a line beginning with a space or tab continues
/// the previous one. Tolerates bare `LF` as well as the spec's `CRLF`.
fn unfold(input: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in input.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(continuation) = line.strip_prefix([' ', '\t']) {
            if let Some(last) = lines.last_mut() {
                last.push_str(continuation);
            }
            continue;
        }
        if !line.is_empty() {
            lines.push(line.to_string());
        }
    }
    lines
}

/// Split one unfolded content line into name, parameters and value.
fn parse_property(line: &str) -> Option<Property> {
    // The value begins at the first ':' outside a quoted parameter value — a
    // quoted TZID or ALTREP may legitimately contain ':' and ';'.
    let colon = split_unquoted(line, ':').next()?.len();
    let (head, value) = line.split_at(colon);
    let value = value.strip_prefix(':')?;

    let mut parts = split_unquoted(head, ';');
    let name = parts.next()?.trim().to_ascii_uppercase();
    if name.is_empty() {
        return None;
    }
    let params = parts
        .filter_map(|part| {
            let (key, raw) = part.split_once('=')?;
            Some((key.trim().to_ascii_uppercase(), unquote_param(raw)))
        })
        .collect();

    Some(Property {
        name,
        params,
        value: value.to_string(),
    })
}

/// Split `s` on `sep`, ignoring separators inside a double-quoted section.
fn split_unquoted(s: &str, sep: char) -> impl Iterator<Item = &str> {
    let mut in_quotes = false;
    let mut start = 0usize;
    let mut done = false;
    let mut chars = s.char_indices();
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        for (i, c) in chars.by_ref() {
            if c == '"' {
                in_quotes = !in_quotes;
            } else if c == sep && !in_quotes {
                let piece = &s[start..i];
                start = i + c.len_utf8();
                return Some(piece);
            }
        }
        done = true;
        Some(&s[start..])
    })
}

/// Strip the quotes from a parameter value and decode RFC 6868 caret escapes
/// (a quoted parameter value cannot itself contain a quote).
fn unquote_param(raw: &str) -> String {
    let inner = raw
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .unwrap_or(raw);

    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '^' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('\'') => out.push('"'),
            Some('^') => out.push('^'),
            // An unknown escape is kept verbatim rather than swallowed.
            Some(other) => {
                out.push('^');
                out.push(other);
            }
            None => out.push('^'),
        }
    }
    out
}

/// Resolve RFC 5545 TEXT escapes (`\n`, `\N`, `\\`, `\,`, `\;`).
fn unescape_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(',') => out.push(','),
            Some(';') => out.push(';'),
            // Showing a stray backslash beats silently eating the next char.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// `Some(s)` when every byte of `s` is an ASCII digit.
fn digits(s: &str) -> Option<&str> {
    s.bytes().all(|b| b.is_ascii_digit()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream shaped like a real iCloud `calendar-data` blob: nested
    /// VTIMEZONE, a folded DESCRIPTION, a quoted parameter containing both
    /// delimiters, and an override VEVENT sharing the master's UID.
    const SAMPLE: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VTIMEZONE\r\n\
TZID:Europe/London\r\n\
BEGIN:DAYLIGHT\r\n\
TZOFFSETTO:+0100\r\n\
END:DAYLIGHT\r\n\
END:VTIMEZONE\r\n\
BEGIN:VEVENT\r\n\
UID:abc-123\r\n\
SUMMARY:Team sync\\, weekly\r\n\
DESCRIPTION:First line\\nsecond line and a very long tail that the\r\n\
\x20\x20server folded right here\r\n\
DTSTART;TZID=Europe/London:20260722T140000\r\n\
DTEND;TZID=Europe/London:20260722T150000\r\n\
ATTENDEE;CN=\"Doe; Jane: lead\";PARTSTAT=ACCEPTED:mailto:jane@example.com\r\n\
RRULE:FREQ=WEEKLY;BYDAY=WE\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:abc-123\r\n\
RECURRENCE-ID;TZID=Europe/London:20260729T140000\r\n\
SUMMARY:Team sync (moved)\r\n\
DTSTART;TZID=Europe/London:20260729T160000\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    fn sample_events() -> Vec<Component> {
        let parsed = parse(SAMPLE);
        assert_eq!(parsed.len(), 1, "one top-level VCALENDAR");
        assert_eq!(parsed[0].name, "VCALENDAR");
        parsed[0].children_named("VEVENT").cloned().collect()
    }

    #[test]
    fn parses_nested_components_and_keeps_document_order() {
        let calendar = &parse(SAMPLE)[0];
        // VTIMEZONE nests a DAYLIGHT child; VEVENTs are siblings of VTIMEZONE.
        let tz = calendar.children_named("VTIMEZONE").next().unwrap();
        assert_eq!(tz.text("TZID"), "Europe/London");
        assert_eq!(tz.children_named("DAYLIGHT").count(), 1);
        assert_eq!(calendar.children_named("VEVENT").count(), 2);
    }

    #[test]
    fn unfolds_continuation_lines_and_unescapes_text() {
        let event = &sample_events()[0];
        assert_eq!(event.text("SUMMARY"), "Team sync, weekly");
        assert_eq!(
            event.text("DESCRIPTION"),
            "First line\nsecond line and a very long tail that the server folded right here"
        );
    }

    #[test]
    fn quoted_parameter_may_contain_the_delimiters() {
        // Splitting naively on ';' or ':' would cut this CN in half and leave
        // the mailto: value truncated.
        let attendee = sample_events()[0].get("ATTENDEE").cloned().unwrap();
        assert_eq!(attendee.param("CN"), Some("Doe; Jane: lead"));
        assert_eq!(attendee.param("PARTSTAT"), Some("ACCEPTED"));
        assert_eq!(attendee.value, "mailto:jane@example.com");
        // Parameter lookup is case-insensitive.
        assert_eq!(attendee.param("cn"), Some("Doe; Jane: lead"));
    }

    #[test]
    fn date_times_carry_their_own_zone_and_are_never_converted() {
        let event = &sample_events()[0];
        let start = event.get("DTSTART").unwrap().date_time().unwrap();
        assert_eq!(start.wall, "2026-07-22T14:00:00");
        assert_eq!(start.zone.as_deref(), Some("Europe/London"));
        assert!(!start.is_date);

        // UTC, floating and all-day forms.
        let utc = IcalDateTime::parse("20260722T140000Z", None).unwrap();
        assert_eq!(utc.zone.as_deref(), Some("UTC"));
        assert_eq!(utc.wall, "2026-07-22T14:00:00");

        let floating = IcalDateTime::parse("20260722T140000", None).unwrap();
        assert_eq!(floating.zone, None);

        // An all-day value keeps no zone at all — that is what stops anything
        // downstream from shifting it onto the wrong day.
        let all_day = IcalDateTime::parse("20260722", Some("Europe/London")).unwrap();
        assert!(all_day.is_date);
        assert_eq!(all_day.wall, "2026-07-22");
        assert_eq!(all_day.zone, None);
    }

    #[test]
    fn malformed_date_times_are_rejected_not_guessed() {
        for bad in ["", "2026072", "abcdefgh", "20260722X140000", "20260722T14"] {
            assert!(
                IcalDateTime::parse(bad, None).is_none(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn durations_cover_every_allowed_unit() {
        assert_eq!(parse_duration("PT1H"), Some(3_600));
        assert_eq!(parse_duration("P1DT2H30M"), Some(86_400 + 9_000));
        assert_eq!(parse_duration("P2W"), Some(2 * 7 * 86_400));
        assert_eq!(parse_duration("-PT15M"), Some(-900));
        assert_eq!(parse_duration("PT45S"), Some(45));
        // 'M' means months outside a time part, which DURATION does not allow.
        assert_eq!(parse_duration("P1M"), None);
        assert_eq!(parse_duration("P1D2H"), None);
        assert_eq!(parse_duration("PT5"), None);
        assert_eq!(parse_duration("1H"), None);
    }

    #[test]
    fn tolerates_truncated_and_malformed_input() {
        // No END:VEVENT / END:VCALENDAR — the event still comes through.
        let truncated = "BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Kept\n";
        let events: Vec<_> = parse(truncated)[0]
            .children_named("VEVENT")
            .cloned()
            .collect();
        assert_eq!(events[0].text("SUMMARY"), "Kept");

        // A line with no ':' is skipped, not fatal.
        let junk = "BEGIN:VCALENDAR\nBEGIN:VEVENT\ngarbage line\nSUMMARY:Still here\nEND:VEVENT\nEND:VCALENDAR\n";
        let events: Vec<_> = parse(junk)[0].children_named("VEVENT").cloned().collect();
        assert_eq!(events[0].text("SUMMARY"), "Still here");

        // Empty input yields nothing rather than panicking.
        assert!(parse("").is_empty());
    }

    #[test]
    fn rfc6868_caret_escapes_decode_in_parameters() {
        let line = "ATTENDEE;CN=\"Alice ^'the boss^'\":mailto:a@example.com";
        let property = parse_property(line).unwrap();
        assert_eq!(property.param("CN"), Some("Alice \"the boss\""));
    }
}
