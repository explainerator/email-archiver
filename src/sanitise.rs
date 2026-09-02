//! Making HTML mail safe to render.
//!
//! Every message in this archive is untrusted input that arrived from the
//! internet, and the reading pane renders it inside an authenticated session.
//! This module is the first of four defences described in WEBAPP-PLAN.md 6; the
//! others — an opaque-origin sandboxed iframe, a hash-pinned CSP, and remote
//! images off by default — live in the client and the response headers, and
//! none of them is trusted alone.
//!
//! **The allowlist is deliberately brutal: structural HTML only, and no styling
//! at all.** Some mail renders badly. That is an accepted, stated cost — a
//! newsletter that looks wrong is a complaint, a message that runs code is an
//! incident. Loosening this later with a list of real broken messages is easy;
//! tightening it after a leak is not.
//!
//! Sanitisation is parse-then-reserialise (`ammonia`, on `html5ever`), never
//! regex. Regex over HTML loses to nesting and encoding tricks every time.

use base64ct::Encoding as _;
use std::collections::{HashMap, HashSet};

/// What came back from sanitising a message body.
pub struct Sanitised {
    pub html: String,
    /// How many remote images were blocked, so the client can offer to load
    /// them rather than silently hiding that anything was removed.
    pub blocked_images: usize,
}

/// Elements allowed through. Everything else is unwrapped or dropped.
///
/// Tables stay despite being a layout hack in mail: they are structurally
/// inert, and dropping them turns most newsletters into an unreadable run of
/// concatenated text rather than something merely ugly. More to the point, the
/// highest-value mail in an archive — receipts, invoices, itineraries — is
/// tabular, and its structure is the part worth keeping.
const ALLOWED_TAGS: &[&str] = &[
    // Block
    "p",
    "div",
    "br",
    "hr",
    "blockquote",
    "pre", // Headings
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6", // Inline
    "span",
    "strong",
    "b",
    "em",
    "i",
    "u",
    "s",
    "sub",
    "sup",
    "code",
    "small",
    // Lists
    "ul",
    "ol",
    "li",
    "dl",
    "dt",
    "dd", // Tables
    "table",
    "thead",
    "tbody",
    "tfoot",
    "tr",
    "td",
    "th",
    "caption", // Links and images
    "a",
    "img",
];

/// Elements removed **with their contents**, rather than unwrapped.
///
/// A `<style>` block whose tags are stripped but whose text is kept would
/// render the CSS as visible garbage in the middle of the message.
const DROP_WITH_CONTENTS: &[&str] = &[
    "script", "style", "link", "meta", "base", "iframe", "object", "embed", "applet", "form",
    "input", "button", "select", "textarea", "option", "svg", "math", "canvas", "audio", "video",
    "frame", "frameset", "noscript", "template", "marquee",
];

/// Placeholder a blocked remote image points at.
///
/// A `data:` URI for a 1x1 transparent GIF, so nothing is requested from the
/// network and the `img` element survives for the client to swap back if the
/// reader asks for remote content.
const BLOCKED_PIXEL: &str =
    "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";

/// Sanitise one message body.
///
/// `inline_base` is the URL prefix for this message's own parts; `cid:`
/// references are rewritten onto it so the CSP can be `img-src 'self'` with no
/// `data:` allowance for author content at all.
pub fn clean(html: &str, inline_base: &str, allow_remote: bool) -> Sanitised {
    // Shared with the attribute filter closure, which ammonia takes by value.
    let blocked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&blocked);

    let tags: HashSet<&str> = ALLOWED_TAGS.iter().copied().collect();
    let clean_content: HashSet<&str> = DROP_WITH_CONTENTS.iter().copied().collect();

    // Attributes are allowed PER TAG and nowhere generically. This is where
    // strictness actually bites: `style` is gone everywhere, no exceptions,
    // which removes overlay and click-jacking tricks, CSS-based exfiltration
    // through attribute selectors and background URLs, and text hidden from the
    // reader but visible to a parser. It also means the frame CSP needs no
    // `style-src 'unsafe-inline'`, which is the single biggest strength gain
    // available here and the reason the rendering loss is worth accepting.
    let mut per_tag: HashMap<&str, HashSet<&str>> = HashMap::new();
    per_tag.insert("a", ["href"].into_iter().collect());
    per_tag.insert("img", ["src", "alt"].into_iter().collect());
    per_tag.insert("td", ["colspan", "rowspan"].into_iter().collect());
    per_tag.insert("th", ["colspan", "rowspan"].into_iter().collect());

    let inline_base = inline_base.to_string();

    let mut builder = ammonia::Builder::default();
    builder
        .tags(tags)
        .clean_content_tags(clean_content)
        .tag_attributes(per_tag)
        // No attribute is permitted on the strength of being harmless-looking:
        // class, id, width, height, align, bgcolor, border, target, srcset,
        // background and every data-* go, along with every on* handler.
        .generic_attributes(HashSet::new())
        // Added by us, never taken from the message.
        .link_rel(Some("noopener noreferrer nofollow"))
        // First of two URL checks. Schemes that can execute never reach the
        // filter below at all. `cid` is here because inline images arrive that
        // way and are rewritten; `data:` is absent, because the only data: URL
        // in the output is our own placeholder, inserted after this runs.
        .url_schemes(["http", "https", "mailto", "cid"].into_iter().collect())
        // Relative URLs are PASSED to the filter rather than refused here,
        // because the filter's own output -- the same-origin inline image URL --
        // is relative, and Deny would throw that away too. The filter is
        // therefore the authority on what a relative URL means: ours are kept,
        // the message's are dropped.
        .url_relative(ammonia::UrlRelative::PassThrough)
        .strip_comments(true)
        .attribute_filter(move |element, attribute, value| {
            match (element, attribute) {
                ("a", "href") => {
                    // Second URL check, and the one that rejects relative links.
                    // A relative URL in mail has no base of its own, so a
                    // browser would resolve it against OUR origin -- turning a
                    // link in a message into a request to the archive.
                    let lower = value.trim().to_ascii_lowercase();
                    let absolute = lower.starts_with("http://")
                        || lower.starts_with("https://")
                        || lower.starts_with("mailto:");
                    absolute.then(|| value.into())
                }
                ("img", "src") => {
                    // Inline parts referenced by Content-ID become same-origin
                    // URLs into this message's own parts.
                    if let Some(cid) = value.strip_prefix("cid:") {
                        return Some(format!("{inline_base}/{}", encode(cid)).into());
                    }
                    // Anything else is remote: a tracking pixel until proven
                    // otherwise. It tells the sender the message was read, when,
                    // and from which IP -- a privacy control as much as a
                    // security one, and for old archived mail the default should
                    // be off.
                    // Only absolute http(s) counts as a remote image. Anything
                    // else -- a relative path, an unrecognised scheme -- is
                    // dropped rather than guessed at.
                    let lower = value.trim().to_ascii_lowercase();
                    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
                        return None;
                    }
                    if allow_remote {
                        Some(value.into())
                    } else {
                        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        Some(BLOCKED_PIXEL.into())
                    }
                }
                _ => Some(value.into()),
            }
        });

    let html = builder.clean(html).to_string();

    Sanitised {
        html,
        blocked_images: blocked.load(std::sync::atomic::Ordering::Relaxed),
    }
}

/// Percent-encode a Content-ID for use in a path segment.
fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean_str(html: &str) -> String {
        clean(html, "/api/messages/abc/inline", false).html
    }

    #[test]
    fn scripts_do_not_survive() {
        let out = clean_str("<p>hi</p><script>alert(1)</script>");
        assert!(!out.contains("script"), "{out}");
        assert!(!out.contains("alert"), "script CONTENTS survived: {out}");
    }

    #[test]
    fn style_blocks_are_removed_with_their_contents() {
        // Unwrapping rather than dropping would render the CSS as visible text.
        let out = clean_str("<style>body{color:red}</style><p>hi</p>");
        assert!(!out.contains("color:red"), "{out}");
        assert_eq!(out, "<p>hi</p>");
    }

    #[test]
    fn inline_styles_are_stripped_everywhere() {
        // The single most important rule here: it is what lets the frame CSP
        // avoid style-src 'unsafe-inline'.
        let out = clean_str(r#"<p style="position:absolute;top:0">hi</p>"#);
        assert!(!out.contains("style"), "{out}");
        assert!(out.contains("hi"));
    }

    #[test]
    fn event_handlers_are_stripped() {
        let out = clean_str(r#"<p onclick="steal()" onmouseover="x()">hi</p>"#);
        assert!(!out.contains("onclick"), "{out}");
        assert!(!out.contains("onmouseover"), "{out}");
        assert!(!out.contains("steal"), "{out}");
    }

    #[test]
    fn javascript_urls_are_rejected() {
        for bad in [
            r#"<a href="javascript:alert(1)">x</a>"#,
            r#"<a href="JaVaScRiPt:alert(1)">x</a>"#,
            r#"<a href="vbscript:x">x</a>"#,
            r#"<a href="data:text/html,<script>x</script>">x</a>"#,
        ] {
            let out = clean_str(bad);
            assert!(!out.contains("javascript"), "{bad} -> {out}");
            assert!(!out.contains("vbscript"), "{bad} -> {out}");
            assert!(!out.contains("data:text/html"), "{bad} -> {out}");
        }
    }

    #[test]
    fn iframes_and_objects_are_removed_entirely() {
        for bad in [
            "<iframe src=https://evil.test></iframe>",
            "<object data=x></object>",
            "<embed src=x>",
            "<form action=https://evil.test><input name=p></form>",
        ] {
            let out = clean_str(bad);
            assert!(!out.contains("evil.test"), "{bad} -> {out}");
            assert!(
                out.trim().is_empty() || !out.contains('<'),
                "{bad} -> {out}"
            );
        }
    }

    #[test]
    fn remote_images_are_blocked_and_counted() {
        let result = clean(
            r#"<img src="https://tracker.test/pixel.gif"><img src="http://x.test/a.png">"#,
            "/api/messages/abc/inline",
            false,
        );
        assert_eq!(result.blocked_images, 2);
        assert!(!result.html.contains("tracker.test"), "{}", result.html);
        assert!(result.html.contains("data:image/gif"), "{}", result.html);
    }

    #[test]
    fn remote_images_pass_when_the_reader_asks() {
        let result = clean(
            r#"<img src="https://example.test/a.png">"#,
            "/api/messages/abc/inline",
            true,
        );
        assert_eq!(result.blocked_images, 0);
        assert!(result.html.contains("example.test"), "{}", result.html);
    }

    #[test]
    fn cid_images_become_same_origin_urls() {
        // This is what lets the frame CSP be `img-src 'self'` with no data:
        // allowance for author content.
        let out = clean_str(r#"<img src="cid:logo@example.com">"#);
        assert!(
            out.contains("/api/messages/abc/inline/logo%40example.com"),
            "{out}"
        );
        assert!(!out.contains("cid:"), "{out}");
    }

    #[test]
    fn links_get_our_rel_not_the_message_s() {
        let out = clean_str(r#"<a href="https://ok.test" target="_blank" rel="opener">x</a>"#);
        assert!(out.contains("noopener"), "{out}");
        assert!(out.contains("noreferrer"), "{out}");
        // target is not in the allowlist; the client opens links itself.
        assert!(!out.contains("target"), "{out}");
    }

    #[test]
    fn structure_and_tables_survive() {
        // The reason mail stays readable: receipts and itineraries are tables.
        let html = "<table><tr><th>Item</th><td>Widget</td></tr></table>\
                    <ul><li>one</li></ul><h2>Title</h2><blockquote>q</blockquote>";
        let out = clean_str(html);
        for expected in ["<table", "<th", "<td", "<ul", "<li", "<h2", "<blockquote"] {
            assert!(out.contains(expected), "lost {expected}: {out}");
        }
        assert!(out.contains("Widget"));
    }

    #[test]
    fn colspan_survives_but_presentation_does_not() {
        // Wrapped in a table: html5ever drops a bare <td>, since it is not
        // valid outside one. That is the parser being correct, not the
        // allowlist rejecting it.
        let out =
            clean_str(r#"<table><tr><td colspan="2" bgcolor="red" width="50">x</td></tr></table>"#);
        assert!(out.contains("colspan"), "{out}");
        assert!(!out.contains("bgcolor"), "{out}");
        assert!(!out.contains("width"), "{out}");
    }

    #[test]
    fn comments_are_stripped() {
        // Outlook conditional comments can hide markup from a naive parser
        // while remaining live to a browser.
        let out = clean_str("<p>a</p><!--[if mso]><script>x</script><![endif]--><p>b</p>");
        assert!(!out.contains("script"), "{out}");
        assert!(!out.contains("mso"), "{out}");
    }

    #[test]
    fn class_and_id_are_dropped() {
        // Nothing generic is permitted, so message markup cannot collide with
        // or target the shell's own DOM.
        let out = clean_str(r#"<div class="x" id="main" data-track="1">hi</div>"#);
        assert!(!out.contains("class"), "{out}");
        assert!(!out.contains("id="), "{out}");
        assert!(!out.contains("data-track"), "{out}");
    }

    #[test]
    fn relative_urls_are_refused() {
        // A relative URL in mail has no base; letting one through would resolve
        // it against OUR origin.
        let out = clean_str(r#"<a href="/api/session">x</a>"#);
        assert!(!out.contains("/api/session"), "{out}");
    }

    #[test]
    fn nesting_and_encoding_tricks_do_not_get_through() {
        // The reason this is parse-then-reserialise rather than regex.
        for bad in [
            "<scr<script>ipt>alert(1)</script>",
            "<img src=x onerror=alert(1)>",
            "<a href=\"jav\tascript:alert(1)\">x</a>",
            "<<SCRIPT>alert(1);//<</SCRIPT>",
        ] {
            let out = clean_str(bad);
            let lower = out.to_ascii_lowercase();
            // What matters is that no EXECUTABLE markup survives, not that the
            // string "alert(1)" is absent. A broken tag can leave fragments
            // behind as escaped text, and inert text reading "alert(1)" is
            // harmless -- asserting on the literal string fails on safe output
            // while proving nothing useful.
            assert!(!lower.contains("<script"), "{bad} -> {out}");
            assert!(!lower.contains("<scr"), "{bad} -> {out}");
            assert!(!lower.contains("onerror"), "{bad} -> {out}");
            assert!(!lower.contains("javascript:"), "{bad} -> {out}");
        }
    }
}

// ---------------------------------------------------------------------------
// The frame document
// ---------------------------------------------------------------------------

/// Our own stylesheet for the reading frame.
///
/// Author styles are stripped entirely (§6.2), so this is the only CSS in the
/// document. It exists for legibility, not appearance: stop wide images and
/// long tables from overflowing, and wrap text that has no line breaks.
const FRAME_CSS: &str = "\
html{color-scheme:light dark}\
body{margin:0;font:14px/1.55 system-ui,-apple-system,'Segoe UI',sans-serif;overflow-wrap:anywhere}\
img{max-width:100%;height:auto}\
table{max-width:100%;border-collapse:collapse}\
td,th{padding:.15rem .4rem;vertical-align:top}\
blockquote{margin:.5em 0 .5em .8em;padding-left:.8em;border-left:2px solid currentColor;opacity:.75}\
pre{white-space:pre-wrap}\
a{color:inherit}";

/// Wrap sanitised body HTML in the document the reading frame renders.
///
/// Built here rather than in the client so the policy and the stylesheet it
/// pins can never drift apart: the hash below is computed from the very bytes
/// that go into the document.
///
/// The policy is only this restrictive because author styles are gone. With
/// them, `style-src` would need `'unsafe-inline'`, which permits any style
/// block at all; pinning one hash permits exactly ours and refuses anything
/// that somehow survived sanitisation.
///
/// * `default-src 'none'` — no scripts, fonts, frames, connections, media.
/// * `img-src 'self'` — inline parts only. Remote images were already rewritten
///   to a `data:` placeholder, so `data:` is listed for that alone.
/// * `form-action 'none'`, `base-uri 'none'` — nothing to submit to or rebase.
///
/// The frame itself is sandboxed WITHOUT `allow-scripts` or
/// `allow-same-origin`, so this is the third layer, not the only one.
pub fn frame_document(body_html: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(FRAME_CSS.as_bytes());
    let hash = base64ct::Base64::encode_string(&digest);

    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"\
         default-src 'none'; img-src 'self' data:; style-src 'sha256-{hash}'; \
         form-action 'none'; base-uri 'none'\">\
         <style>{FRAME_CSS}</style></head><body>{body_html}</body></html>"
    )
}

#[cfg(test)]
mod frame_tests {
    use super::*;

    #[test]
    fn the_policy_pins_our_own_stylesheet() {
        let doc = frame_document("<p>hi</p>");
        assert!(doc.contains("style-src 'sha256-"), "{doc}");
        assert!(doc.contains("default-src 'none'"), "{doc}");
        // The weaker alternative must not appear: with unsafe-inline any style
        // block is permitted, which is exactly what dropping author styles buys
        // us the ability to avoid.
        assert!(!doc.contains("unsafe-inline"), "{doc}");
        assert!(!doc.contains("unsafe-eval"), "{doc}");
    }

    #[test]
    fn the_hash_actually_matches_the_stylesheet() {
        // A hash that does not match its own stylesheet fails closed -- the
        // frame renders unstyled -- which is easy to miss by eye. Recomputing
        // it here means changing FRAME_CSS without updating the digest cannot
        // pass silently.
        use sha2::{Digest, Sha256};
        let expected = base64ct::Base64::encode_string(&Sha256::digest(FRAME_CSS.as_bytes()));
        let doc = frame_document("");
        assert!(doc.contains(&format!("'sha256-{expected}'")), "{doc}");
    }

    #[test]
    fn scripts_never_reach_the_frame() {
        let cleaned = clean("<script>x</script><p>ok</p>", "/inline", false);
        let doc = frame_document(&cleaned.html);
        assert!(!doc.to_ascii_lowercase().contains("<script"), "{doc}");
        assert!(doc.contains("<p>ok</p>"), "{doc}");
    }
}
