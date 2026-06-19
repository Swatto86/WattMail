//! Email-body sanitization.
//!
//! Email HTML is hostile by default. Everything produced here is safe to drop
//! into a sandboxed frame: scripts, event handlers, `<style>`, and all
//! remote-loading elements (remote images, media, stylesheets, CSS `url(...)`)
//! are removed. Links keep their href for display but are inert inside the
//! frame's `sandbox`.
//!
//! Inline `style` attributes are **kept but sanitized** to an allowlist of safe
//! properties (colours, borders, padding, alignment, …) with any `url(...)`,
//! `expression`, `@import`, or `javascript:` rejected — so styled mail (tables,
//! coloured ticks) renders with fidelity without reopening a remote-content
//! vector. Images are stripped by default; `allow_images` keeps them.

use std::borrow::Cow;

/// The result of sanitizing an email body.
pub struct Sanitized {
    /// HTML that is always safe to render in a sandboxed frame.
    pub html: String,
    /// True if the original carried remote content (e.g. images) that was removed.
    pub remote_content_blocked: bool,
    /// True when the email sets its own (non-white) large-area background —
    /// the signature of designed/marketing mail. Theme-independent, so it is
    /// safe to compute once and cache. Conservative: biased to `true`, because
    /// the safe failure is rendering on the light "paper" card. Drives the
    /// frontend's light-card vs. adapt-to-theme decision; a pure-white or absent
    /// background is treated as *not* designed, so ordinary mail can follow the
    /// app theme in dark mode.
    pub is_designed: bool,
}

/// Inline CSS properties we allow through (everything else is dropped). None of
/// these can load a remote resource once `url(...)` values are rejected.
const ALLOWED_CSS_PROPERTIES: &[&str] = &[
    "color",
    "background-color",
    "background",
    "font",
    "font-weight",
    "font-style",
    "font-size",
    "font-family",
    "text-align",
    "text-decoration",
    "text-transform",
    "line-height",
    "letter-spacing",
    "vertical-align",
    "white-space",
    "padding",
    "padding-top",
    "padding-bottom",
    "padding-left",
    "padding-right",
    "margin",
    "margin-top",
    "margin-bottom",
    "margin-left",
    "margin-right",
    "border",
    "border-top",
    "border-bottom",
    "border-left",
    "border-right",
    "border-color",
    "border-width",
    "border-style",
    "border-radius",
    "border-collapse",
    "border-spacing",
    "width",
    "max-width",
    "min-width",
    "height",
    "max-height",
    "min-height",
    "display",
    "table-layout",
];

/// Sanitize an email body. `is_html` selects HTML cleaning vs. plain-text
/// escaping; `allow_images` keeps remote images instead of stripping them.
pub fn sanitize_email(content: &str, is_html: bool, allow_images: bool) -> Sanitized {
    if !is_html {
        return Sanitized {
            html: text_to_html(content),
            remote_content_blocked: false,
            is_designed: false,
        };
    }

    let remote_content_blocked = if allow_images {
        false
    } else {
        has_remote_content(content)
    };

    let mut builder = ammonia::Builder::default();
    builder
        // `<font color=…>` is common in mail and carries no remote risk.
        .add_tags(["font"])
        .add_tag_attributes("font", ["color", "face", "size"])
        // Presentational attributes (safe — no remote loads) plus `style`, which
        // the attribute filter below sanitizes.
        .add_generic_attributes([
            "style",
            "align",
            "valign",
            "bgcolor",
            "width",
            "height",
            "colspan",
            "rowspan",
            "cellpadding",
            "cellspacing",
            "border",
        ])
        .attribute_filter(|_element, attribute, value| {
            if attribute == "style" {
                let cleaned = sanitize_style(value);
                if cleaned.is_empty() {
                    None
                } else {
                    Some(Cow::Owned(cleaned))
                }
            } else {
                Some(Cow::Borrowed(value))
            }
        });
    if !allow_images {
        // Removing `img` closes the remote-image / tracking-pixel vector.
        builder.rm_tags(["img"]);
    }
    // Classify against the raw source (before cleaning) so author intent is
    // visible even where the sanitizer would later drop a value. This only
    // reads; it can never reintroduce blocked content.
    let is_designed = has_own_background(content);

    let html = builder.clean(content).to_string();

    Sanitized {
        html,
        remote_content_blocked,
        is_designed,
    }
}

/// Keep only allowlisted CSS declarations with safe values.
fn sanitize_style(style: &str) -> String {
    let mut out = String::new();
    for declaration in style.split(';') {
        let Some((property, value)) = declaration.split_once(':') else {
            continue;
        };
        let property = property.trim().to_ascii_lowercase();
        let value = value.trim();
        if value.is_empty()
            || !ALLOWED_CSS_PROPERTIES.contains(&property.as_str())
            || !is_safe_css_value(value)
        {
            continue;
        }
        out.push_str(&property);
        out.push(':');
        out.push_str(value);
        out.push(';');
    }
    out
}

/// Reject CSS values that could load remote content or escape into script.
fn is_safe_css_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    !lower.contains("url(")
        && !lower.contains("expression")
        && !lower.contains("javascript:")
        && !lower.contains("@import")
        && !lower.contains("/*")
}

/// Escape plain text into HTML, preserving line breaks.
fn text_to_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "<br>")
}

/// Heuristic: did the original body reference remote resources we strip? Drives
/// the "remote content blocked" indicator, not the sanitization itself.
fn has_remote_content(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    lower.contains("<img")
        || lower.contains("url(http")
        || lower.contains("background:url")
        || lower.contains("background: url")
}

/// Conservative, theme-independent test: does the source declare its own
/// large-area background that is *not* the default white canvas? "Designed"
/// (marketing/HTML) mail almost always sets at least one non-white background
/// (a header bar, a card, a coloured cell); plain mail sets none, or only a
/// pure-white one it inherits by convention. Pure-white / transparent / absent
/// backgrounds count as *not* designed so ordinary mail can follow the app
/// theme in dark mode — this mirrors how Apple Mail treats a white background
/// the same as no background. Biased to `true` on anything else.
fn has_own_background(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    has_designed_bgcolor_attr(&lower)
        || has_designed_css_background(&lower, "background-color:")
        || has_designed_css_background(&lower, "background:")
}

/// Any `bgcolor="…"` presentational attribute whose value is a real, non-white
/// colour. Scans every occurrence — one non-white hit is enough.
fn has_designed_bgcolor_attr(lower: &str) -> bool {
    let mut rest = lower;
    while let Some(i) = rest.find("bgcolor=") {
        let after = &rest[i + "bgcolor=".len()..];
        if !is_ignorable_background(read_attr_value(after)) {
            return true;
        }
        rest = after;
    }
    false
}

/// Any inline `background[-color]:` declaration with a real, non-white value.
fn has_designed_css_background(lower: &str, key: &str) -> bool {
    let mut rest = lower;
    while let Some(i) = rest.find(key) {
        let after = &rest[i + key.len()..];
        let end = after.find([';', '"', '\'', '}']).unwrap_or(after.len());
        if !is_ignorable_background(after[..end].trim()) {
            return true;
        }
        rest = after;
    }
    false
}

/// Read an HTML attribute value, quoted (`"…"` / `'…'`) or bare (up to
/// whitespace or `>`). Input is the slice immediately following `name=`.
fn read_attr_value(after: &str) -> &str {
    let mut chars = after.chars();
    match chars.next() {
        Some(q @ ('"' | '\'')) => {
            let body = &after[1..];
            let end = body.find(q).unwrap_or(body.len());
            body[..end].trim()
        }
        _ => {
            let end = after
                .find(|c: char| c.is_whitespace() || c == '>')
                .unwrap_or(after.len());
            after[..end].trim()
        }
    }
}

/// A background value that should *not* mark an email as designed: empty, a
/// CSS default keyword, or pure white in any common notation.
fn is_ignorable_background(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return true;
    }
    if [
        "transparent",
        "inherit",
        "none",
        "initial",
        "unset",
        "currentcolor",
    ]
    .iter()
    .any(|kw| v.starts_with(kw))
    {
        return true;
    }
    is_white(v)
}

/// Pure white in the notations mail actually uses (whitespace-insensitive).
fn is_white(value: &str) -> bool {
    let compact: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    matches!(
        compact.as_str(),
        "#fff" | "#ffffff" | "#ffffffff" | "white" | "rgb(255,255,255)" | "rgba(255,255,255,1)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn designed(html: &str) -> bool {
        sanitize_email(html, true, false).is_designed
    }

    #[test]
    fn plain_text_is_never_designed() {
        assert!(!sanitize_email("hello", false, false).is_designed);
    }

    #[test]
    fn unstyled_html_is_not_designed() {
        assert!(!designed("<p>hi there</p>"));
    }

    #[test]
    fn plain_coloured_text_is_not_designed() {
        // Dark text on no background is the *plain* case we adapt per-element,
        // not a designed layout.
        assert!(!designed(r##"<p style="color:#000000">hi</p>"##));
    }

    #[test]
    fn non_white_bgcolor_attr_is_designed() {
        assert!(designed(
            r##"<table bgcolor="#0a66c2"><tr><td>x</td></tr></table>"##
        ));
    }

    #[test]
    fn non_white_inline_background_is_designed() {
        assert!(designed(
            r##"<div style="background-color:#102030">x</div>"##
        ));
        assert!(designed(
            r##"<div style="background:#102030 none repeat">x</div>"##
        ));
    }

    #[test]
    fn white_background_is_not_designed() {
        // Ordinary mail that merely restates the white canvas must still adapt.
        assert!(!designed(r##"<body bgcolor="#FFFFFF"><p>hi</p></body>"##));
        assert!(!designed(r##"<div style="background-color:#fff">x</div>"##));
        assert!(!designed(
            r#"<table bgcolor="white"><tr><td>x</td></tr></table>"#
        ));
        assert!(!designed(
            r#"<div style="background:rgb(255, 255, 255)">x</div>"#
        ));
    }

    #[test]
    fn transparent_background_is_not_designed() {
        assert!(!designed(r#"<div style="background:transparent">x</div>"#));
    }

    #[test]
    fn white_then_coloured_background_is_designed() {
        // A white page wrapper plus any coloured cell is a designed layout.
        assert!(designed(
            r##"<body bgcolor="#ffffff"><td bgcolor="#0a66c2">x</td></body>"##
        ));
    }
}
