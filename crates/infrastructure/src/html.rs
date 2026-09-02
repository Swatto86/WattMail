//! Email-body sanitization.
//!
//! Email HTML is hostile by default. Everything produced here is safe to drop
//! into a sandboxed frame: scripts, event handlers, `<style>`, and all
//! remote-loading elements (remote images, media, stylesheets, CSS `url(...)`)
//! are removed. Links keep their href for display but cannot navigate the
//! frame; the parent opens http(s) targets in the system browser.
//!
//! Inline `style` attributes are **kept but sanitized** to an allowlist of safe
//! properties (colours, borders, padding, alignment, …) with any `url(...)`,
//! `expression`, `@import`, or `javascript:` rejected — so styled mail (tables,
//! coloured ticks) renders with fidelity without reopening a remote-content
//! vector. Images are stripped by default; `allow_images` keeps them.
//!
//! A pre-pass rewrites invalid `<p><a href>…<table>/<div>…</a></p>` button
//! markup (HTML5 would otherwise close the paragraph — and the link — before
//! the block, leaving a visible unclickable button). Matching `</p>` is
//! depth-aware so a label paragraph inside the table does not truncate the
//! rewrite.

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
        // `data:` URLs are self-contained (no remote load): allow them so inline
        // `cid:`-resolved and embedded images survive sanitization. The
        // attribute filter below still strips remote (`http(s)`) image sources
        // in blocked mode, and a `data:` href on a link is dropped there too.
        .add_url_schemes(["data"])
        .attribute_filter(move |element, attribute, value| {
            if attribute == "style" {
                let cleaned = sanitize_style(value);
                return if cleaned.is_empty() {
                    None
                } else {
                    Some(Cow::Owned(cleaned))
                };
            }
            if element == "img" && attribute == "src" {
                let is_data = value.trim_start().to_ascii_lowercase().starts_with("data:");
                // Keep inline (`data:`) images always; keep remote images only
                // when images are allowed (they're proxied to `data:` after
                // sanitization). In blocked mode a remote src is dropped so
                // nothing loads remotely.
                return if is_data || allow_images {
                    Some(Cow::Borrowed(value))
                } else {
                    None
                };
            }
            // Never let a `data:` URL ride on a link (defense in depth — links
            // are inert in the sandbox and externally gated, but keep them out).
            if attribute == "href" && value.trim_start().to_ascii_lowercase().starts_with("data:") {
                return None;
            }
            Some(Cow::Borrowed(value))
        });
    // Classify against the raw source (before cleaning) so author intent is
    // visible even where the sanitizer would later drop a value. This only
    // reads; it can never reintroduce blocked content.
    let is_designed = has_own_background(content);

    // HTML5 closes `<p>` before a nested `<table>`/`<div>`/heading, which
    // splits `<p><a href><table>…button…</table></a></p>` into an empty link
    // plus an unlinked visual. Rename those invalid paragraphs first so the
    // `<a>` still wraps the button after parsing.
    let rewritten = rewrite_paragraphs_that_wrap_block_links(content);
    let html = builder.clean(&rewritten).to_string();

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
/// Backslashes are rejected outright: CSS escape sequences (`\75rl(…)` decodes
/// to `url(…)`) would otherwise reconstruct any of the banned tokens past
/// these literal substring checks. No legitimate mail style needs one.
fn is_safe_css_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    !lower.contains('\\')
        && !lower.contains("url(")
        && !lower.contains("expression")
        && !lower.contains("javascript:")
        && !lower.contains("@import")
        && !lower.contains("/*")
}

/// Tags that close an open `<p>` in HTML5 and are used as "bulletproof" email
/// button chrome. Detected as real start tags (so `<div` matches `<div>` /
/// `<div class=…>` but not `<divine>`).
const BLOCK_BUTTON_TAGS: &[&str] = &[
    "<table",
    "<div",
    "<h1",
    "<h2",
    "<h3",
    "<h4",
    "<h5",
    "<h6",
    "<ul",
    "<ol",
    "<pre",
    "<blockquote",
    "<center",
    "<section",
    "<article",
    "<header",
    "<footer",
];

/// True when `lower` (already ASCII-lowercased) contains a real start tag
/// whose name is `tag` (`tag` includes the leading `<`, e.g. `"<table"`).
fn contains_tag_open(lower: &str, tag: &str) -> bool {
    let mut rest = lower;
    while let Some(i) = rest.find(tag) {
        let after = rest.as_bytes().get(i + tag.len()).copied();
        if after.is_none_or(|b| matches!(b, b'>' | b'/' | b' ' | b'\t' | b'\n' | b'\r')) {
            return true;
        }
        rest = &rest[i + tag.len()..];
    }
    false
}

fn contains_block_button_tag(lower: &str) -> bool {
    BLOCK_BUTTON_TAGS
        .iter()
        .any(|tag| contains_tag_open(lower, tag))
}

fn is_real_p_open(lower: &str, at: usize) -> bool {
    lower[at..].starts_with("<p")
        && lower
            .as_bytes()
            .get(at + 2)
            .copied()
            .is_none_or(|b| matches!(b, b'>' | b'/' | b' ' | b'\t' | b'\n' | b'\r'))
}

/// Index of the `</p>` that matches the paragraph whose inner HTML starts at
/// `open_end`. Tracks nesting so a label `<p>` inside a table button does not
/// steal the wrapper's close (the naive first-`</p>` match would rewrite a
/// truncated slice and HTML5 would still eject the button from the `<a>`).
fn find_matching_p_close(lower: &str, open_end: usize) -> Option<usize> {
    let mut depth = 1;
    let mut i = open_end;
    while i < lower.len() {
        let rel = lower[i..].find('<')?;
        i += rel;
        if lower[i..].starts_with("</p>") {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
            i += 4;
            continue;
        }
        if is_real_p_open(lower, i) {
            let gt = lower[i..].find('>')?;
            let tag_end = i + gt + 1;
            let self_close = lower.as_bytes().get(tag_end.saturating_sub(2)) == Some(&b'/');
            if !self_close {
                depth += 1;
            }
            i = tag_end;
            continue;
        }
        i += 1;
    }
    None
}

/// Rename `<p>` wrappers that contain both an `<a>` and a block-level tag to
/// `<div>`, keeping attributes. Those paragraphs are already invalid HTML;
/// leaving them as `<p>` makes the parser eject the button from the link.
fn rewrite_paragraphs_that_wrap_block_links(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    if !contains_tag_open(&lower, "<p")
        || !contains_tag_open(&lower, "<a")
        || !contains_block_button_tag(&lower)
    {
        return html.to_string();
    }

    let mut out = String::with_capacity(html.len() + 8);
    let mut pos = 0;
    while pos < html.len() {
        let Some(rel) = lower[pos..].find("<p") else {
            out.push_str(&html[pos..]);
            break;
        };
        let p_start = pos + rel;
        if !is_real_p_open(&lower, p_start) {
            out.push_str(&html[pos..p_start + 2]);
            pos = p_start + 2;
            continue;
        }
        let Some(gt) = html[p_start..].find('>') else {
            out.push_str(&html[pos..]);
            break;
        };
        let open_end = p_start + gt + 1;
        if html.as_bytes().get(open_end.saturating_sub(2)) == Some(&b'/') {
            out.push_str(&html[pos..open_end]);
            pos = open_end;
            continue;
        }
        let Some(inner_end) = find_matching_p_close(&lower, open_end) else {
            out.push_str(&html[pos..]);
            break;
        };
        let close_end = inner_end + 4;
        let inner_lower = &lower[open_end..inner_end];
        if contains_tag_open(inner_lower, "<a") && contains_block_button_tag(inner_lower) {
            out.push_str(&html[pos..p_start]);
            out.push_str("<div");
            out.push_str(&html[p_start + 2..open_end]);
            out.push_str(&html[open_end..inner_end]);
            out.push_str("</div>");
        } else {
            out.push_str(&html[pos..close_end]);
        }
        pos = close_end;
    }
    out
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

/// Heuristic: did the original body reference *remote* resources we strip?
/// Drives the "remote content blocked" indicator, not the sanitization itself.
/// Inline `cid:`/`data:` images are self-contained (no remote load), so an image
/// whose only sources are inline must NOT trip the banner — otherwise clicking
/// "load images" does nothing.
fn has_remote_content(html: &str) -> bool {
    // Match against a whitespace-stripped, lowercased copy so non-canonical
    // spacing (`src = "http"`, `url( 'http' )`, `background: url`) still trips
    // the banner. The real (DOM-based) sanitizer strips these regardless of
    // spacing, so a remote image that doesn't trip here would show as a
    // silently-missing image the user has no way to load.
    let lower: String = html
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    has_remote_img_src(&lower)
        || lower.contains("url(http")
        || lower.contains("url('http")
        || lower.contains("url(\"http")
        || lower.contains("background:url")
}

/// True when a source attribute points at an `http(s)` URL — a remote-loading
/// image. Only `<img>` carries a fetching `src` in rendered mail (scripts and
/// link `href`s don't load in the sandboxed frame), so this reads as "a remote
/// image would have loaded". `cid:`/`data:` sources are inline and excluded.
fn has_remote_img_src(lower: &str) -> bool {
    lower.contains("src=\"http")
        || lower.contains("src='http")
        || lower.contains("src=http")
        // Scheme-relative sources (`//host/x`) load remotely too and are stripped
        // by the attribute filter just like `http(s)`, so the banner must fire for
        // them or the image goes silently missing with no way to load it.
        || lower.contains("src=\"//")
        || lower.contains("src='//")
        || lower.contains("src=//")
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
    fn css_backslash_escape_cannot_smuggle_url_past_the_filter() {
        // `\75` decodes to `u` in CSS, reconstituting `url(...)` — the whole
        // declaration must be dropped, in both blocked and allowed modes.
        let html = r#"<div style="background:\75rl(http://tracker.example/p.gif)">x</div>"#;
        for allow_images in [false, true] {
            let out = sanitize_email(html, true, allow_images).html;
            assert!(!out.contains('\\'), "escape survived: {out}");
            assert!(!out.contains("tracker.example"), "url survived: {out}");
        }
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

    #[test]
    fn cid_only_body_does_not_trip_the_remote_banner() {
        // A message whose only image is a cid: reference has no remote content —
        // the banner must not appear (clicking "load images" can't help it).
        let s = sanitize_email(r#"<p>hi</p><img src="cid:logo@01d">"#, true, false);
        assert!(!s.remote_content_blocked);
    }

    #[test]
    fn data_image_survives_blocked_mode() {
        // Inline data: images are self-contained and must render even when remote
        // images are blocked.
        let s = sanitize_email(r#"<img src="data:image/png;base64,AAAA">"#, true, false);
        assert!(s.html.contains("data:image/png;base64,AAAA"));
        assert!(!s.remote_content_blocked);
    }

    #[test]
    fn remote_image_is_stripped_and_flagged_in_blocked_mode() {
        let s = sanitize_email(r#"<img src="http://tracker.example/x.png">"#, true, false);
        assert!(s.remote_content_blocked);
        assert!(!s.html.contains("http://tracker.example"));
    }

    #[test]
    fn remote_image_with_spaced_attribute_still_trips_banner() {
        // Non-canonical spacing around `=` must not fool the banner heuristic —
        // the sanitizer strips the image either way.
        let s = sanitize_email(r#"<img src = "http://tracker.example/x.png">"#, true, false);
        assert!(s.remote_content_blocked);
        assert!(!s.html.contains("http://tracker.example"));
    }

    #[test]
    fn scheme_relative_remote_image_is_stripped_and_flagged() {
        // `//host/x` is a remote load; it must trip the banner AND be stripped,
        // exactly like an http(s) source.
        let s = sanitize_email(r#"<img src="//cdn.example/logo.png">"#, true, false);
        assert!(s.remote_content_blocked);
        assert!(!s.html.contains("//cdn.example"));
    }

    #[test]
    fn remote_css_background_with_spaces_trips_banner() {
        let s = sanitize_email(
            r#"<div style="background: url( 'http://tracker.example/x.gif' )">hi</div>"#,
            true,
            false,
        );
        assert!(s.remote_content_blocked);
    }

    #[test]
    fn remote_image_survives_when_images_allowed() {
        // In allow mode the remote src is kept (it's proxied to data: afterward).
        let s = sanitize_email(r#"<img src="http://cdn.example/x.png">"#, true, true);
        assert!(s.html.contains("http://cdn.example/x.png"));
    }

    #[test]
    fn data_href_on_a_link_is_dropped() {
        let s = sanitize_email(
            r#"<a href="data:text/html,<script>">click</a>"#,
            true,
            false,
        );
        assert!(!s.html.contains("data:text/html"));
    }

    fn assert_link_wraps(html: &str, url: &str, label: &str) {
        let out = sanitize_email(html, true, false).html;
        let href_at = out
            .find(&format!("href=\"{url}\""))
            .unwrap_or_else(|| panic!("href lost: {out}"));
        let label_at = out
            .find(label)
            .unwrap_or_else(|| panic!("label lost: {out}"));
        let a_close = out[href_at..]
            .find("</a>")
            .map(|i| href_at + i)
            .unwrap_or_else(|| panic!("no closing </a>: {out}"));
        assert!(
            label_at < a_close,
            "button label is not inside the <a> (HTML5 split the link): {out}"
        );
    }

    #[test]
    fn styled_anchor_button_keeps_href_and_padding() {
        let s = sanitize_email(
            r##"<a href="https://example.com/confirm" style="display:inline-block;background-color:#f97316;color:#ffffff;padding:12px 24px;border-radius:6px;text-decoration:none;">Confirm Email</a>"##,
            true,
            false,
        );
        assert!(s.html.contains("href=\"https://example.com/confirm\""));
        assert!(s.html.contains("display:inline-block"));
        assert!(s.html.contains("padding:12px 24px"));
        assert!(s.html.contains("Confirm Email"));
    }

    #[test]
    fn paragraph_wrapped_table_button_stays_inside_the_anchor() {
        // Outlook-style bulletproof button nested in a <p> — without the
        // pre-pass, HTML5 closes the <p> (and the <a>) before <table>.
        assert_link_wraps(
            r##"<p align="center"><a href="https://example.com/confirm"><table><tr><td bgcolor="#f97316" style="padding:12px 24px;color:#ffffff;">Confirm Email</td></tr></table></a></p>"##,
            "https://example.com/confirm",
            "Confirm Email",
        );
    }

    #[test]
    fn paragraph_wrapped_div_button_stays_inside_the_anchor() {
        assert_link_wraps(
            r##"<p><a href="https://example.com/x"><div style="background-color:#f97316;padding:12px 24px;color:#ffffff;">Confirm Email</div></a></p>"##,
            "https://example.com/x",
            "Confirm Email",
        );
    }

    #[test]
    fn react_email_button_keeps_href_and_label() {
        assert_link_wraps(
            r##"<a href="https://claude.ai/magic-link/abc" target="_blank" style="line-height:100%;text-decoration:none;display:inline-block;max-width:100%;mso-padding-alt:0px;background:#191919;border-radius:8px;color:#ffffff;font-size:14px;font-weight:600;text-align:center;padding:12px 24px 12px 24px"><span><!--[if mso]><i style="letter-spacing: 24px;mso-font-width:-100%;mso-text-raise:18" hidden>&nbsp;</i><![endif]--></span><span style="max-width:100%;display:inline-block;line-height:120%;text-decoration:none;text-transform:none;mso-padding-alt:0px;mso-text-raise:9px">Sign in</span><span><!--[if mso]><i style="letter-spacing: 24px;mso-font-width:-100%" hidden>&nbsp;</i><![endif]--></span></a>"##,
            "https://claude.ai/magic-link/abc",
            "Sign in",
        );
    }

    #[test]
    fn nested_paragraph_inside_table_button_stays_inside_the_anchor() {
        // Labels wrapped in their own <p> used to make the rewrite close at the
        // inner </p>, leaving the table outside the <a>.
        assert_link_wraps(
            r##"<p align="center"><a href="https://example.com/confirm"><table><tr><td bgcolor="#f97316"><p style="color:#ffffff;">Confirm Email</p></td></tr></table></a></p>"##,
            "https://example.com/confirm",
            "Confirm Email",
        );
    }

    #[test]
    fn ordinary_paragraph_with_a_text_link_is_not_rewritten() {
        let s = sanitize_email(
            r#"<p>Click <a href="https://example.com/x">here</a> please.</p>"#,
            true,
            false,
        );
        assert!(s.html.contains("<p>"), "plain paragraph became: {}", s.html);
        assert!(
            !s.html.contains("<div"),
            "plain paragraph was rewritten: {}",
            s.html
        );
    }

    #[test]
    fn pre_tag_is_not_mistaken_for_a_paragraph() {
        let s = sanitize_email(
            r#"<pre>p &lt; a <a href="https://example.com/x">link</a></pre>"#,
            true,
            false,
        );
        assert!(s.html.contains("<pre>"), "pre lost: {}", s.html);
        assert!(s.html.contains("href=\"https://example.com/x\""));
    }
}
