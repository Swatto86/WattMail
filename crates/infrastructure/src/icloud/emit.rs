//! Serialize an iCalendar [`Component`] back to the wire format, for the write
//! path (create / update / delete-occurrence / RSVP).
//!
//! The counterpart to [`super::ical::parse`]: a component built here, or parsed,
//! edited and re-emitted, round-trips through a CalDAV `PUT`.
//!
//! **Emoji and any non-ASCII text** ride through untouched — the one thing that
//! must be right is line folding, which RFC 5545 §3.1 defines in **octets**, not
//! characters. Folding mid-character would corrupt a multi-byte sequence, so
//! [`fold`] only ever breaks on a character boundary.

use super::ical::{Component, Property};

/// The octet limit for one physical line, per RFC 5545 §3.1. The trailing CRLF
/// is not counted against it.
const LINE_OCTETS: usize = 75;

/// Serialize a component (with its children) to CRLF-terminated iCalendar text.
pub fn serialize(component: &Component) -> String {
    let mut out = String::new();
    write_component(component, &mut out);
    out
}

fn write_component(component: &Component, out: &mut String) {
    push_line(&format!("BEGIN:{}", component.name), out);
    for property in &component.properties {
        push_line(&content_line(property), out);
    }
    for child in &component.children {
        write_component(child, out);
    }
    push_line(&format!("END:{}", component.name), out);
}

/// Assemble one unfolded content line: `NAME;PARAM=value:VALUE`.
fn content_line(property: &Property) -> String {
    let mut line = property.name.clone();
    for (key, value) in property.params() {
        line.push(';');
        line.push_str(key);
        line.push('=');
        line.push_str(&encode_param(value));
    }
    line.push(':');
    // `value` is already in wire form (escaped by `Property::plain`, or verbatim
    // from `Property::raw` / a parse), so it is emitted as-is.
    line.push_str(&property.value);
    line
}

/// Quote and RFC 6868-encode a parameter value when it needs it.
///
/// A value containing `:`, `;` or `,` must be double-quoted; a quote or newline
/// inside it cannot be represented literally even when quoted, so those use the
/// caret escapes RFC 6868 defines.
fn encode_param(value: &str) -> String {
    let needs_quotes = value.contains([':', ';', ',']);
    let mut inner = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '^' => inner.push_str("^^"),
            '\n' => inner.push_str("^n"),
            '"' => inner.push_str("^'"),
            other => inner.push(other),
        }
    }
    if needs_quotes || inner.contains('^') {
        format!("\"{inner}\"")
    } else {
        inner
    }
}

/// Fold a content line to ≤75 octets per physical line and append it with its
/// CRLF terminators. A continuation line begins with one space, which counts
/// toward its own octet budget.
fn push_line(line: &str, out: &mut String) {
    let mut col = 0usize;
    for c in line.chars() {
        let width = c.len_utf8();
        // Break before a character that would overflow the line — never in the
        // middle of one, so a multi-byte character (an emoji) is never split.
        if col + width > LINE_OCTETS {
            out.push_str("\r\n ");
            col = 1; // the leading space already occupies one octet
        }
        out.push(c);
        col += width;
    }
    out.push_str("\r\n");
}

#[cfg(test)]
mod tests {
    use super::super::ical::{self, Component, Property};
    use super::*;

    #[test]
    fn round_trips_a_built_event_through_the_parser() {
        let mut event = Component::new("vevent");
        event
            .set(Property::raw("UID", "abc-123@wattmail"))
            .set(Property::plain("SUMMARY", "Lunch with Amélie; bring caké"))
            .set(Property::raw("DTSTART", "20260722T120000Z"))
            .set(Property::raw("DTEND", "20260722T130000Z"));
        let mut calendar = Component::new("vcalendar");
        calendar.set(Property::raw("VERSION", "2.0"));
        calendar.push_child(event);

        let text = serialize(&calendar);
        assert!(text.ends_with("END:VCALENDAR\r\n"));
        // Parse it back and confirm the escaped value decodes to the original.
        let parsed = ical::parse(&text);
        let vevent = parsed[0].children_named("VEVENT").next().unwrap();
        assert_eq!(vevent.text("SUMMARY"), "Lunch with Amélie; bring caké");
        assert_eq!(vevent.get("DTSTART").unwrap().value, "20260722T120000Z");
    }

    #[test]
    fn text_values_are_escaped_on_emit_and_survive_the_round_trip() {
        let prop = Property::plain("DESCRIPTION", "line one\nsemi; comma, back\\slash");
        // The stored wire value is escaped…
        assert_eq!(prop.value, "line one\\nsemi\\; comma\\, back\\\\slash");
        // …and a parse restores the plain text exactly.
        let line = content_line(&prop);
        let reparsed = ical::parse(&format!("BEGIN:VEVENT\r\n{line}\r\nEND:VEVENT\r\n"));
        assert_eq!(
            reparsed[0].text("DESCRIPTION"),
            "line one\nsemi; comma, back\\slash"
        );
    }

    #[test]
    fn folding_counts_octets_and_never_splits_an_emoji() {
        // A run of 4-octet emoji whose total blows well past 75 octets. If any
        // physical line split one, the bytes would no longer be valid UTF-8 and
        // re-parsing would not recover the original.
        let summary = "🎉".repeat(40); // 160 octets
        let prop = Property::plain("SUMMARY", &summary);
        let mut out = String::new();
        push_line(&content_line(&prop), &mut out);

        for physical in out.trim_end().split("\r\n") {
            assert!(
                physical.len() <= LINE_OCTETS,
                "physical line is {} octets: {physical:?}",
                physical.len()
            );
        }
        // Every continuation begins with a space, and unfolding restores the run.
        let reparsed = ical::parse(&format!("BEGIN:VEVENT\r\n{out}END:VEVENT\r\n"));
        assert_eq!(reparsed[0].text("SUMMARY"), summary);
    }

    #[test]
    fn a_parameter_with_delimiters_is_quoted_and_caret_encoded() {
        assert_eq!(encode_param("Europe/London"), "Europe/London");
        assert_eq!(encode_param("Doe, Jane"), "\"Doe, Jane\"");
        assert_eq!(encode_param("a:b;c"), "\"a:b;c\"");
        // A quote inside can't be represented literally even when quoted.
        assert_eq!(encode_param("say \"hi\""), "\"say ^'hi^'\"");

        // And it round-trips: the parser decodes the caret escapes back.
        let attendee = Property::raw("ATTENDEE", "mailto:a@x.io").with_param("CN", "Doe; \"Jane\"");
        let line = content_line(&attendee);
        let reparsed = ical::parse(&format!("BEGIN:VEVENT\r\n{line}\r\nEND:VEVENT\r\n"));
        assert_eq!(
            reparsed[0].get("ATTENDEE").unwrap().param("CN"),
            Some("Doe; \"Jane\"")
        );
    }
}
