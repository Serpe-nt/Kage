//! A small HTML-to-visible-text reduction for the classifier.
//!
//! Soft-block rules ask "is there almost no text on this page?". Answering
//! that needs only a rough reduction: drop comments, tags and the contents
//! of elements that never render text, then collapse whitespace. A real HTML
//! parser lives in `kage-extract`; this crate stays free of one.

/// Elements whose contents are never rendered as page text.
const NON_TEXT_ELEMENTS: [&str; 5] = ["script", "style", "template", "head", "title"];

/// Reduces HTML to the text a reader would see, with whitespace collapsed to
/// single spaces. `<noscript>` content is kept: it is what a client without
/// JavaScript is shown.
pub(crate) fn visible_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 4);
    let mut rest = html;

    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let tag = &rest[open..];

        if let Some(after) = tag.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |end| &after[end + 3..]);
        } else if let Some(name) = non_text_element(tag) {
            match after_closing_tag(tag, name) {
                Some(after) => rest = after,
                // A script or style that never closes swallows the rest of
                // the document in a browser too. An unclosed <head> does
                // not, so only its own tag is dropped.
                None if name == "head" || name == "title" => rest = after_tag(tag),
                None => rest = "",
            }
        } else if looks_like_tag(tag) {
            rest = after_tag(tag);
        } else {
            // A lone '<' is text.
            out.push('<');
            rest = &tag[1..];
            continue;
        }
        // A tag separates words even when the source has no whitespace.
        out.push(' ');
    }
    out.push_str(rest);

    let out = out.replace("&nbsp;", " ");
    let mut collapsed = String::with_capacity(out.len());
    for word in out.split_whitespace() {
        if !collapsed.is_empty() {
            collapsed.push(' ');
        }
        collapsed.push_str(word);
    }
    collapsed
}

/// The non-text element that starts at `tag` (which begins with `<`), if any.
fn non_text_element(tag: &str) -> Option<&'static str> {
    let body = &tag[1..];
    NON_TEXT_ELEMENTS.into_iter().find(|name| {
        starts_with_ignore_case(body, name)
            && body[name.len()..]
                .chars()
                .next()
                .is_none_or(|c| c == '>' || c == '/' || c.is_ascii_whitespace())
    })
}

/// The text after `</name ...>`, searching from the start of the open tag.
fn after_closing_tag<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut search = tag;
    while let Some(position) = search.find("</") {
        let candidate = &search[position + 2..];
        if starts_with_ignore_case(candidate, name) {
            return Some(after_tag(candidate));
        }
        search = candidate;
    }
    None
}

/// The text after the next `>`, or nothing when the tag never closes.
fn after_tag(tag: &str) -> &str {
    tag.find('>').map_or("", |end| &tag[end + 1..])
}

/// Whether `tag` (which begins with `<`) starts a tag, a closing tag or a
/// declaration, as opposed to a literal less-than sign in text.
fn looks_like_tag(tag: &str) -> bool {
    tag[1..]
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!' || c == '?')
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::visible_text;

    #[test]
    fn tags_are_dropped_and_whitespace_collapsed() {
        let html = "<html><body>\n  <h1>Hello</h1>\n\n  <p>big   <b>world</b></p>\n</body></html>";

        assert_eq!(visible_text(html), "Hello big world");
    }

    #[test]
    fn script_style_template_head_and_comments_are_not_text() {
        let html = r#"<!doctype html>
<html><head><title>Just a moment...</title><meta charset="utf-8">
<style>body { color: red; }</style></head>
<body>
<!-- a comment with <b>markup</b> -->
<script type="text/javascript">var text = "<p>not text</p>"; if (a < b) { run(); }</script>
<template><p>template text</p></template>
<p>Only this.</p>
<SCRIPT>upper()</SCRIPT>
</body></html>"#;

        assert_eq!(visible_text(html), "Only this.");
    }

    #[test]
    fn noscript_content_counts_as_text() {
        let html =
            "<body><noscript>Please enable JavaScript.</noscript><div id=\"app\"></div></body>";

        assert_eq!(visible_text(html), "Please enable JavaScript.");
    }

    #[test]
    fn unclosed_script_swallows_the_rest_but_unclosed_head_does_not() {
        assert_eq!(
            visible_text("before <script>var a = 1; <p>after</p>"),
            "before"
        );
        assert_eq!(visible_text("<style>p { }"), "");
        assert_eq!(visible_text("<head><p>still text</p>"), "still text");
        assert_eq!(
            visible_text("<title>never closed <p>text</p>"),
            "never closed text"
        );
        assert_eq!(visible_text("text <!-- never closed"), "text");
    }

    #[test]
    fn element_names_need_a_boundary() {
        // <scripted> and <header> are not <script> and <head>.
        assert_eq!(
            visible_text("<scripted>a</scripted> <header>b</header>"),
            "a b"
        );
        assert_eq!(visible_text("<script/>x</script>y"), "y");
    }

    #[test]
    fn a_lone_less_than_sign_is_text() {
        assert_eq!(visible_text("1 < 2 and 3 <4"), "1 < 2 and 3 <4");
        assert_eq!(visible_text("ends with <"), "ends with <");
        assert_eq!(visible_text("<p unclosed"), "");
    }

    #[test]
    fn plain_text_and_json_pass_through() {
        assert_eq!(visible_text("  plain   text  "), "plain text");
        assert_eq!(visible_text(r#"{"items":[1,2,3]}"#), r#"{"items":[1,2,3]}"#);
        assert_eq!(visible_text(""), "");
        assert_eq!(visible_text("a&nbsp;&nbsp;b"), "a b");
    }

    #[test]
    fn non_ascii_text_is_kept_intact() {
        assert_eq!(
            visible_text("<p>Zugriff verweigert — bitte später</p>"),
            "Zugriff verweigert — bitte später"
        );
        assert_eq!(
            visible_text("<p>アクセス</p><script>‹›</script>拒否"),
            "アクセス 拒否"
        );
    }
}
