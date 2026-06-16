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
    let html = builder.clean(content).to_string();

    Sanitized {
        html,
        remote_content_blocked,
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
