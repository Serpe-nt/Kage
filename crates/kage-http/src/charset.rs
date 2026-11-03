//! How the http path reads a body as text (spec §9, F100): by the
//! character encoding a browser would take for it, as the Encoding Standard
//! and the HTML standard decide it, so that levels 0 and 1 read what levels
//! 2 and 3 read in a browser, for a page that says what it is written in.
//! Two cases still read otherwise (spec §9): a `<meta>` past the first
//! [`PRESCAN_BYTES`], which a browser's tree builder may still honour, and a
//! page in another legacy encoding that declares nothing, which a browser's
//! detector may name and this module reads as windows-1252.
//!
//! In order, the first that answers:
//!
//! 1. a byte order mark (UTF-8, UTF-16LE, UTF-16BE), which wins over any
//!    declaration;
//! 2. the `charset` parameter of `Content-Type`, read as an Encoding
//!    Standard label (`latin1`, `iso-8859-1` and `us-ascii` are all
//!    windows-1252, `shift_jis` and `sjis` are `Shift_JIS`);
//! 3. for an HTML document (or a body that does not say what it is), the
//!    `<meta charset>` or `<meta http-equiv="content-type">` of its first
//!    1024 bytes, found by the standard's prescan; for an XML document, the
//!    `encoding` of its XML declaration;
//! 4. the fallback: UTF-8 when the bytes are UTF-8 (as Chrome's detector
//!    finds), else windows-1252, the standard's fallback for the locales of
//!    every country the personas live in (and for most others).
//!
//! JSON is UTF-8 unless a byte order mark says otherwise (RFC 8259). Bytes
//! that are not valid in the chosen encoding are replaced, as a browser
//! replaces them.

use std::borrow::Cow;

use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252, X_USER_DEFINED};

/// How many bytes of a document the `<meta>` prescan reads, as the HTML
/// standard has it.
pub const PRESCAN_BYTES: usize = 1024;

/// What decided the encoding of a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecidedBy {
    /// A byte order mark.
    Bom,
    /// The `charset` of `Content-Type`.
    Header,
    /// A `<meta>` of the document.
    Meta,
    /// The XML declaration.
    XmlDeclaration,
    /// Nothing was declared: the bytes are UTF-8, or the body is JSON.
    Utf8,
    /// Nothing was declared and the bytes are not UTF-8: windows-1252.
    Fallback,
}

/// A body read as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded<'a> {
    /// The text.
    pub text: Cow<'a, str>,
    /// The encoding it was read in.
    pub encoding: &'static Encoding,
    /// What decided the encoding.
    pub by: DecidedBy,
    /// Whether some bytes were not valid in it and were replaced.
    pub malformed: bool,
}

/// The kinds of body that are read by different rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// HTML, or a body that does not say what it is: a browser shows it as
    /// a page.
    Html,
    /// XML, XHTML among it.
    Xml,
    /// JSON.
    Json,
    /// Any other text, and whatever is no text.
    Other,
}

/// The essence of a `Content-Type` (`text/html`), lower case, without its
/// parameters; empty when there is none.
fn essence(content_type: Option<&str>) -> String {
    content_type
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn kind_of(content_type: Option<&str>) -> Kind {
    let mime = essence(content_type);
    if mime.is_empty() || mime == "text/html" {
        Kind::Html
    } else if mime == "application/xhtml+xml"
        || mime == "text/xml"
        || mime == "application/xml"
        || mime.ends_with("+xml")
    {
        Kind::Xml
    } else if mime == "application/json" || mime.ends_with("+json") {
        Kind::Json
    } else {
        Kind::Other
    }
}

/// Whether a `Content-Type` says the body is text: `text/*`, HTML, XML,
/// JSON, a script. A body that does not say is taken for HTML, as a browser
/// takes it.
pub fn is_text(content_type: Option<&str>) -> bool {
    let mime = essence(content_type);
    mime.is_empty()
        || mime.starts_with("text/")
        || kind_of(content_type) != Kind::Other
        || matches!(
            mime.as_str(),
            "application/javascript" | "application/ecmascript" | "application/x-javascript"
        )
}

/// The encoding a `charset` parameter of a `Content-Type` names, when it
/// names one the Encoding Standard knows.
pub fn charset_of(content_type: &str) -> Option<&'static Encoding> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("charset") {
            return None;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|rest| rest.split('"').next())
            .unwrap_or(value);
        Encoding::for_label(value.as_bytes())
    })
}

/// Reads a body as text by the encoding a browser would take for it (see
/// the module's documentation). `content_type` is the response's
/// `Content-Type`, when it had one.
pub fn decode<'a>(content_type: Option<&str>, body: &'a [u8]) -> Decoded<'a> {
    if let Some((encoding, length)) = Encoding::for_bom(body) {
        return read(&body[length..], encoding, DecidedBy::Bom);
    }
    let kind = kind_of(content_type);
    if kind == Kind::Json {
        return read(body, UTF_8, DecidedBy::Utf8);
    }
    if let Some(encoding) = content_type.and_then(charset_of) {
        return read(body, encoding, DecidedBy::Header);
    }
    let declared = match kind {
        Kind::Html => prescan(body).map(|encoding| (encoding, DecidedBy::Meta)),
        Kind::Xml => xml_declaration(body).map(|encoding| (encoding, DecidedBy::XmlDeclaration)),
        Kind::Json | Kind::Other => None,
    };
    if let Some((encoding, by)) = declared {
        return read(body, encoding, by);
    }
    if kind == Kind::Xml || std::str::from_utf8(body).is_ok() {
        return read(body, UTF_8, DecidedBy::Utf8);
    }
    read(body, WINDOWS_1252, DecidedBy::Fallback)
}

/// A body as text where it is text: decoded when its `Content-Type` says it
/// is text, or says nothing and the body starts as markup does or with a
/// byte order mark (what a browser's MIME sniffing takes for a page); as it
/// is when it is UTF-8 whatever it says; `None` otherwise, so that bytes of
/// no declared type are not shown as windows-1252.
pub fn text_of(content_type: Option<&str>, body: &[u8]) -> Option<String> {
    let typed = content_type.is_some_and(|value| !essence(Some(value)).is_empty());
    let text = if typed {
        is_text(content_type)
    } else {
        Encoding::for_bom(body).is_some() || sniffs_as_markup(body)
    };
    if text {
        return Some(decode(content_type, body).text.into_owned());
    }
    std::str::from_utf8(body).ok().map(str::to_owned)
}

/// The tags whose opening, after white space, makes a browser's MIME
/// sniffing take a body of no declared type for HTML (the MIME Sniffing
/// standard's HTML patterns).
const MARKUP_STARTS: [&[u8]; 18] = [
    b"<!doctype html",
    b"<html",
    b"<head",
    b"<script",
    b"<iframe",
    b"<h1",
    b"<div",
    b"<font",
    b"<table",
    b"<a",
    b"<style",
    b"<title",
    b"<b",
    b"<body",
    b"<br",
    b"<p",
    b"<!--",
    b"<?xml",
];

/// Whether a body of no declared type starts as an HTML or XML document:
/// after white space, one of [`MARKUP_STARTS`] followed by a space or `>`
/// (or, for a comment and the XML declaration, anything).
fn sniffs_as_markup(body: &[u8]) -> bool {
    let start = body.trim_ascii_start();
    MARKUP_STARTS.iter().any(|pattern| {
        starts_with_at(start, 0, pattern)
            && (pattern.starts_with(b"<!--")
                || pattern.starts_with(b"<?")
                || start
                    .get(pattern.len())
                    .is_some_and(|&next| next == b' ' || next == b'>'))
    })
}

fn read<'a>(bytes: &'a [u8], encoding: &'static Encoding, by: DecidedBy) -> Decoded<'a> {
    let (text, malformed) = encoding.decode_without_bom_handling(bytes);
    Decoded {
        text,
        encoding,
        by,
        malformed,
    }
}

/// What a `<meta>` declares counts as the standard says: a UTF-16 label in
/// an ASCII-compatible document means UTF-8, and `x-user-defined` means
/// windows-1252.
fn as_declared_in_markup(encoding: &'static Encoding) -> &'static Encoding {
    if encoding == UTF_16BE || encoding == UTF_16LE {
        UTF_8
    } else if encoding == X_USER_DEFINED {
        WINDOWS_1252
    } else {
        encoding
    }
}

const fn is_space(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

/// Whether `bytes` at `at` start with `prefix`, ignoring ASCII case.
fn starts_with_at(bytes: &[u8], at: usize, prefix: &[u8]) -> bool {
    bytes
        .get(at..at + prefix.len())
        .is_some_and(|here| here.eq_ignore_ascii_case(prefix))
}

/// The first position at or after `from` where `needle` starts.
fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|found| from + found)
}

/// The HTML standard's prescan of a byte stream for its encoding: the
/// first `<meta>` among the first [`PRESCAN_BYTES`] bytes that declares
/// one, outside comments.
pub fn prescan(body: &[u8]) -> Option<&'static Encoding> {
    let bytes = &body[..body.len().min(PRESCAN_BYTES)];
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at..].starts_with(b"<!--") {
            // To the `>` of the first `-->` after `<!`.
            at = find(bytes, at + 2, b"-->").map_or(bytes.len(), |end| end + 3);
        } else if starts_with_at(bytes, at, b"<meta")
            && bytes
                .get(at + 5)
                .is_some_and(|&next| is_space(next) || next == b'/')
        {
            at += 5;
            if let Some(encoding) = meta(bytes, &mut at) {
                return Some(as_declared_in_markup(encoding));
            }
        } else if bytes[at] == b'<'
            && (bytes.get(at + 1).is_some_and(u8::is_ascii_alphabetic)
                || (bytes.get(at + 1) == Some(&b'/')
                    && bytes.get(at + 2).is_some_and(u8::is_ascii_alphabetic)))
        {
            // Another tag: past its name and its attributes.
            while at < bytes.len() && !is_space(bytes[at]) && bytes[at] != b'>' {
                at += 1;
            }
            while attribute(bytes, &mut at).is_some() {}
        } else if starts_with_at(bytes, at, b"<!")
            || starts_with_at(bytes, at, b"</")
            || starts_with_at(bytes, at, b"<?")
        {
            at = find(bytes, at, b">").map_or(bytes.len(), |end| end + 1);
        } else {
            at += 1;
        }
    }
    None
}

/// The attributes of one `<meta>`, `at` just after its name: the encoding
/// it declares, when it declares one.
fn meta(bytes: &[u8], at: &mut usize) -> Option<&'static Encoding> {
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut got_pragma = false;
    let mut need_pragma: Option<bool> = None;
    let mut charset: Option<&'static Encoding> = None;
    while let Some((name, value)) = attribute(bytes, at) {
        if seen.contains(&name) {
            continue;
        }
        match name.as_slice() {
            b"http-equiv" => got_pragma |= value.eq_ignore_ascii_case(b"content-type"),
            b"content" if charset.is_none() => {
                if let Some(encoding) = charset_in_content(&value) {
                    charset = Some(encoding);
                    need_pragma = Some(true);
                }
            }
            b"charset" => {
                charset = Encoding::for_label(&value);
                need_pragma = Some(false);
            }
            _ => {}
        }
        seen.push(name);
    }
    // A tag the bytes end inside declares nothing: the standard aborts
    // there, and a browser ignores an unfinished tag.
    if *at >= bytes.len() {
        return None;
    }
    match need_pragma {
        Some(true) if !got_pragma => None,
        Some(_) => charset,
        None => None,
    }
}

/// The standard's "get an attribute": the next attribute of a tag, its
/// name in lower case; `None` at the end of the tag.
fn attribute(bytes: &[u8], at: &mut usize) -> Option<(Vec<u8>, Vec<u8>)> {
    while *at < bytes.len() && (is_space(bytes[*at]) || bytes[*at] == b'/') {
        *at += 1;
    }
    if *at >= bytes.len() || bytes[*at] == b'>' {
        return None;
    }
    let mut name = Vec::new();
    loop {
        let Some(&byte) = bytes.get(*at) else {
            return Some((name, Vec::new()));
        };
        if byte == b'=' && !name.is_empty() {
            *at += 1;
            break;
        }
        if is_space(byte) {
            while *at < bytes.len() && is_space(bytes[*at]) {
                *at += 1;
            }
            if bytes.get(*at) != Some(&b'=') {
                return Some((name, Vec::new()));
            }
            *at += 1;
            break;
        }
        if byte == b'/' || byte == b'>' {
            return Some((name, Vec::new()));
        }
        name.push(byte.to_ascii_lowercase());
        *at += 1;
    }
    while *at < bytes.len() && is_space(bytes[*at]) {
        *at += 1;
    }
    let mut value = Vec::new();
    match bytes.get(*at) {
        // The end of the bytes, or of the tag: an empty value.
        None | Some(b'>') => {}
        Some(&quote @ (b'"' | b'\'')) => {
            *at += 1;
            while let Some(&byte) = bytes.get(*at) {
                *at += 1;
                if byte == quote {
                    break;
                }
                value.push(byte.to_ascii_lowercase());
            }
        }
        Some(_) => {
            while let Some(&byte) = bytes.get(*at) {
                if is_space(byte) || byte == b'>' {
                    break;
                }
                value.push(byte.to_ascii_lowercase());
                *at += 1;
            }
        }
    }
    Some((name, value))
}

/// The standard's "extract a character encoding from a meta element": the
/// encoding a `content` value such as `text/html; charset=shift_jis` names.
fn charset_in_content(content: &[u8]) -> Option<&'static Encoding> {
    let mut at = 0;
    loop {
        let found = content
            .get(at..)?
            .windows(7)
            .position(|window| window.eq_ignore_ascii_case(b"charset"))?;
        at += found + 7;
        while content.get(at).copied().is_some_and(is_space) {
            at += 1;
        }
        if content.get(at) == Some(&b'=') {
            at += 1;
            break;
        }
    }
    while content.get(at).copied().is_some_and(is_space) {
        at += 1;
    }
    let rest = content.get(at..)?;
    let value = match rest.first() {
        Some(&quote @ (b'"' | b'\'')) => {
            let end = rest[1..].iter().position(|&byte| byte == quote)?;
            &rest[1..=end]
        }
        Some(_) => {
            let end = rest
                .iter()
                .position(|&byte| is_space(byte) || byte == b';')
                .unwrap_or(rest.len());
            &rest[..end]
        }
        None => return None,
    };
    Encoding::for_label(value)
}

/// The `encoding` of an XML declaration at the start of a document.
fn xml_declaration(body: &[u8]) -> Option<&'static Encoding> {
    if !body.starts_with(b"<?xml") {
        return None;
    }
    let end = find(body, 0, b"?>").filter(|&end| end < PRESCAN_BYTES)?;
    let declaration = &body[..end];
    let at = find(declaration, 0, b"encoding")? + 8;
    let rest = declaration[at..].trim_ascii_start().strip_prefix(b"=")?;
    let rest = rest.trim_ascii_start();
    let quote = *rest
        .first()
        .filter(|&&byte| byte == b'"' || byte == b'\'')?;
    let close = rest[1..].iter().position(|&byte| byte == quote)?;
    Encoding::for_label(&rest[1..=close]).map(as_declared_in_markup)
}

#[cfg(test)]
mod tests {
    use encoding_rs::{EUC_JP, SHIFT_JIS, UTF_8, UTF_16LE, WINDOWS_1252};

    use super::{DecidedBy, charset_of, decode, is_text, prescan, text_of};

    /// "Café Müller, Straße" in ISO-8859-1.
    const LATIN1: &[u8] = b"Caf\xe9 M\xfcller, Stra\xdfe";
    /// 日本語 in `Shift_JIS`.
    const SJIS: &[u8] = &[0x93, 0xfa, 0x96, 0x7b, 0x8c, 0xea];

    fn page(head: &str, text: &[u8]) -> Vec<u8> {
        let mut body = format!("<!doctype html><html><head>{head}<title>").into_bytes();
        body.extend_from_slice(text);
        body.extend_from_slice(b"</title></head><body><p>");
        body.extend_from_slice(text);
        body.extend_from_slice(b"</p></body></html>");
        body
    }

    #[test]
    fn a_latin1_page_is_read_by_its_header() {
        let body = page("", LATIN1);
        for header in [
            "text/html; charset=iso-8859-1",
            "text/html;charset=ISO-8859-1",
            "text/html; charset=\"latin1\"",
            "text/html; charset=us-ascii",
            "TEXT/HTML ; Charset = windows-1252",
        ] {
            let read = decode(Some(header), &body);
            assert_eq!(read.encoding, WINDOWS_1252, "{header}");
            assert_eq!(read.by, DecidedBy::Header, "{header}");
            assert!(
                read.text.contains("<title>Café Müller, Straße</title>"),
                "{header}"
            );
            assert!(!read.malformed, "{header}");
        }
    }

    #[test]
    fn a_latin1_page_is_read_by_its_meta() {
        for head in [
            "<meta charset=\"iso-8859-1\">",
            "<meta charset=iso-8859-1>",
            "<META CHARSET='ISO-8859-1'/>",
            "<meta http-equiv=\"Content-Type\" content=\"text/html; charset=iso-8859-1\">",
            "<meta content='text/html;charset=latin1' http-equiv='content-type'>",
        ] {
            let body = page(head, LATIN1);
            for header in [None, Some("text/html")] {
                let read = decode(header, &body);
                assert_eq!(read.encoding, WINDOWS_1252, "{head}");
                assert_eq!(read.by, DecidedBy::Meta, "{head}");
                assert!(read.text.contains("<p>Café Müller, Straße</p>"), "{head}");
            }
        }
    }

    #[test]
    fn a_shift_jis_page_is_read_by_its_header_or_its_meta() {
        let body = page("", SJIS);
        let by_header = decode(Some("text/html; charset=shift_jis"), &body);
        assert_eq!(by_header.encoding, SHIFT_JIS);
        assert_eq!(by_header.by, DecidedBy::Header);
        assert!(by_header.text.contains("<title>日本語</title>"));
        for head in [
            "<meta charset=\"shift_jis\">",
            "<meta charset=sjis>",
            "<meta http-equiv=content-type content=\"text/html; charset=Shift_JIS\">",
        ] {
            let body = page(head, SJIS);
            let read = decode(Some("text/html"), &body);
            assert_eq!(read.encoding, SHIFT_JIS, "{head}");
            assert_eq!(read.by, DecidedBy::Meta, "{head}");
            assert!(read.text.contains("<p>日本語</p>"), "{head}");
        }
    }

    #[test]
    fn a_byte_order_mark_wins_over_every_declaration() {
        // UTF-16LE with its mark, declared as UTF-16 by the header.
        let text = "<!doctype html><title>Ünïcode</title><p>日本語</p>";
        let mut body = vec![0xff, 0xfe];
        for unit in text.encode_utf16() {
            body.extend_from_slice(&unit.to_le_bytes());
        }
        for header in [
            Some("text/html; charset=utf-16"),
            Some("text/html; charset=iso-8859-1"),
            None,
        ] {
            let read = decode(header, &body);
            assert_eq!(read.encoding, UTF_16LE, "{header:?}");
            assert_eq!(read.by, DecidedBy::Bom);
            assert_eq!(read.text, text);
        }
        // A UTF-8 mark over a page whose meta and header say Latin-1.
        let mut body = b"\xef\xbb\xbf".to_vec();
        body.extend_from_slice(&page("<meta charset=iso-8859-1>", "Café".as_bytes()));
        let read = decode(Some("text/html; charset=iso-8859-1"), &body);
        assert_eq!((read.encoding, read.by), (UTF_8, DecidedBy::Bom));
        assert!(read.text.starts_with("<!doctype") && read.text.contains("<p>Café</p>"));
    }

    /// A header that says otherwise than the bytes is believed, as a
    /// browser believes it, and wins over the document's own `<meta>`.
    #[test]
    fn a_lying_header_is_believed_as_a_browser_believes_it() {
        let utf8 = page("", "Café".as_bytes());
        let read = decode(Some("text/html; charset=iso-8859-1"), &utf8);
        assert_eq!((read.encoding, read.by), (WINDOWS_1252, DecidedBy::Header));
        assert!(read.text.contains("<p>CafÃ©</p>"));
        let latin = page("<meta charset=utf-8>", LATIN1);
        let read = decode(Some("text/html; charset=windows-1252"), &latin);
        assert_eq!((read.encoding, read.by), (WINDOWS_1252, DecidedBy::Header));
        // The meta lies, nothing else speaks: it is believed, and what is
        // not UTF-8 is replaced.
        let read = decode(Some("text/html"), &latin);
        assert_eq!((read.encoding, read.by), (UTF_8, DecidedBy::Meta));
        assert!(read.malformed && read.text.contains('\u{fffd}'));
        // A label the standard does not know is no declaration.
        let read = decode(Some("text/html; charset=no-such-thing"), &latin);
        assert_eq!((read.encoding, read.by), (UTF_8, DecidedBy::Meta));
    }

    #[test]
    fn without_a_declaration_utf8_is_sniffed_and_windows_1252_is_the_fallback() {
        let body = page("", "Café 日本語".as_bytes());
        let read = decode(Some("text/html"), &body);
        assert_eq!((read.encoding, read.by), (UTF_8, DecidedBy::Utf8));
        assert!(read.text.contains("<p>Café 日本語</p>"));
        let body = page("", LATIN1);
        let read = decode(None, &body);
        assert_eq!(
            (read.encoding, read.by),
            (WINDOWS_1252, DecidedBy::Fallback)
        );
        assert!(read.text.contains("<p>Café Müller, Straße</p>"));
        // windows-1252's own letters (an em dash, curly quotes).
        let read = decode(Some("text/plain"), b"\x93quoted\x94 \x97 dash");
        assert_eq!(read.text, "\u{201c}quoted\u{201d} \u{2014} dash");
    }

    #[test]
    fn the_prescan_keeps_to_the_standards_rules() {
        // A meta in a comment, or after the first 1024 bytes, says nothing.
        assert_eq!(prescan(b"<!-- <meta charset=shift_jis> --><p>x"), None);
        let late = format!("{}<meta charset=shift_jis>", " ".repeat(1024));
        assert_eq!(prescan(late.as_bytes()), None);
        let early = format!("{}<meta charset=shift_jis>", " ".repeat(990));
        assert_eq!(prescan(early.as_bytes()), Some(SHIFT_JIS));
        // `content` needs the pragma; the first declaration counts.
        assert_eq!(
            prescan(b"<meta content=\"text/html; charset=euc-jp\">"),
            None
        );
        assert_eq!(
            prescan(b"<meta charset=euc-jp><meta charset=shift_jis>"),
            Some(EUC_JP)
        );
        // An attribute that only looks like one, inside another tag.
        assert_eq!(
            prescan(b"<div title=\"<meta charset=shift_jis>\"><meta charset=euc-jp>"),
            Some(EUC_JP)
        );
        // A UTF-16 label in markup means UTF-8; x-user-defined, windows-1252.
        assert_eq!(prescan(b"<meta charset=utf-16le>"), Some(UTF_8));
        assert_eq!(
            prescan(b"<meta charset=x-user-defined>"),
            Some(WINDOWS_1252)
        );
        // `<metadata>` is no meta, an unknown label is no declaration.
        assert_eq!(prescan(b"<metadata charset=shift_jis>"), None);
        assert_eq!(prescan(b"<meta charset=klingon>"), None);
        // A document cut inside a tag.
        assert_eq!(prescan(b"<meta charset=\"shift"), None);
        assert_eq!(prescan(b"<meta http-equiv"), None);
        assert_eq!(prescan(b""), None);
    }

    /// What a careless reading of the markup would take for a declaration
    /// is none: a comment that ends where it starts, a declaration inside
    /// another tag, in `<!…>`, `<?…>` and a `</` with no name; white space
    /// around an `=` is allowed; the first of `charset` and `content` counts;
    /// and a document cut anywhere declares nothing.
    #[test]
    fn the_prescan_reads_past_what_only_looks_like_a_declaration() {
        // A `<meta>` the bytes end inside declares nothing: at the end of
        // the body, and at the end of the 1024 bytes the prescan reads.
        for cut in [
            "<meta charset=shift_jis",
            "<meta charset=\"shift_jis\"",
            "<meta charset=\"shift_jis",
            "<meta http-equiv=content-type content=\"text/html; charset=shift_jis\"",
        ] {
            assert_eq!(prescan(cut.as_bytes()), None, "{cut}");
        }
        let mut long = vec![b' '; 1000];
        long.extend_from_slice(b"<meta charset=shift_jis>");
        assert_eq!(long.len(), 1024);
        assert_eq!(prescan(&long), Some(SHIFT_JIS));
        long.insert(0, b' ');
        assert_eq!(prescan(&long), None);
        assert_eq!(decode(Some("text/html"), &long).by, DecidedBy::Utf8);
        assert_eq!(
            prescan(b"<p>x</p><!--><meta charset=euc-jp><!-- --><meta charset=shift_jis>"),
            Some(EUC_JP)
        );
        assert_eq!(
            prescan(b"<p>hello <x charset=euc-jp><meta charset=shift_jis>"),
            Some(SHIFT_JIS)
        );
        assert_eq!(
            prescan(b"<p>x</p title=\">x <meta charset=euc-jp>\"><meta charset=shift_jis>"),
            Some(SHIFT_JIS)
        );
        for skipped in [
            "<!x <meta charset=euc-jp>",
            "<?x <meta charset=euc-jp>",
            "</ <meta charset=euc-jp>",
        ] {
            let page = format!("<p>x</p>{skipped}<meta charset=shift_jis>");
            assert_eq!(prescan(page.as_bytes()), Some(SHIFT_JIS), "{skipped}");
        }
        for meta in [
            "<meta charset = shift_jis>",
            "<meta charset= shift_jis>",
            "<meta charset =shift_jis>",
            "<meta http-equiv=content-type content=\"text/html; charset = shift_jis\">",
            "<meta http-equiv=content-type content='text/html; charset=\"shift_jis\"'>",
        ] {
            assert_eq!(prescan(meta.as_bytes()), Some(SHIFT_JIS), "{meta}");
        }
        assert_eq!(
            prescan(
                b"<meta charset=euc-jp content=\"text/html; charset=shift_jis\" http-equiv=content-type>"
            ),
            Some(EUC_JP)
        );
        // A `/` ends a name: `charset/x` is two attributes, and the first,
        // an empty `charset`, is the one that counts.
        assert_eq!(prescan(b"<meta charset/x=euc-jp charset=shift_jis>"), None);
        for cut in [
            "<meta ",
            "<meta charset=",
            "<meta charset",
            "<meta charset= ",
            "<meta charset = ",
            "<p ",
            "<p",
            "<!--",
            "<meta http-equiv=content-type content=\"charset=",
            "<meta http-equiv=content-type content=\"charset =",
        ] {
            assert_eq!(prescan(cut.as_bytes()), None, "{cut}");
        }
    }

    /// The edges of a `content` value and of the XML declaration: a value
    /// ends at `;` or white space, the declaration counts only within the
    /// first 1024 bytes, and its `encoding` is quoted or not there at all.
    #[test]
    fn a_declarations_value_ends_where_the_standard_ends_it() {
        for content in [
            "text/html; charset=shift_jis; level=1",
            "text/html; charset=shift_jis level=1",
        ] {
            let meta = format!("<meta http-equiv=content-type content=\"{content}\">");
            assert_eq!(prescan(meta.as_bytes()), Some(SHIFT_JIS), "{content}");
        }
        // A name of a `<meta>` attribute cut by the end of the bytes.
        assert_eq!(prescan(b"<meta charset "), None);
        // The declaration's end at byte 1024 is past the prescan's bytes.
        let mut late = b"<?xml version=\"1.0\" encoding=\"iso-8859-1\"".to_vec();
        late.resize(1024, b' ');
        late.extend_from_slice(b"?><a>\xfc</a>");
        let read = decode(Some("application/xml"), &late);
        assert_eq!((read.encoding, read.by), (UTF_8, DecidedBy::Utf8));
        let mut early = late.clone();
        early.remove(1000);
        let read = decode(Some("application/xml"), &early);
        assert_eq!(
            (read.encoding, read.by),
            (WINDOWS_1252, DecidedBy::XmlDeclaration)
        );
        // An unquoted `encoding` is no declaration.
        let read = decode(
            Some("text/xml"),
            b"<?xml version=\"1.0\" encoding=Xlatin1X?><a>\xfc</a>",
        );
        assert_eq!((read.encoding, read.by), (UTF_8, DecidedBy::Utf8));
    }

    #[test]
    fn xml_reads_its_declaration_and_json_is_utf8() {
        let mut xml =
            b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><urlset><loc>http://x.example/f"
                .to_vec();
        xml.extend_from_slice(b"\xfcr</loc></urlset>");
        let read = decode(Some("application/xml"), &xml);
        assert_eq!(
            (read.encoding, read.by),
            (WINDOWS_1252, DecidedBy::XmlDeclaration)
        );
        assert!(read.text.contains("/für<"));
        // An XML document without a declaration is UTF-8.
        let read = decode(Some("text/xml"), b"<a>\xfc</a>");
        assert_eq!((read.encoding, read.by), (UTF_8, DecidedBy::Utf8));
        assert!(read.malformed);
        // JSON is UTF-8 whatever its header says.
        let read = decode(
            Some("application/json; charset=iso-8859-1"),
            "{\"a\":\"é\"}".as_bytes(),
        );
        assert_eq!(
            (read.encoding, read.text.as_ref()),
            (UTF_8, "{\"a\":\"é\"}")
        );
    }

    #[test]
    fn what_is_text_is_decoded_and_what_is_not_stays_bytes() {
        for header in [
            None,
            Some("text/html"),
            Some("text/plain; charset=iso-8859-1"),
            Some("text/css"),
            Some("application/xhtml+xml"),
            Some("application/rss+xml"),
            Some("application/ld+json"),
            Some("application/javascript"),
        ] {
            assert!(is_text(header), "{header:?}");
        }
        for header in [
            "application/octet-stream",
            "image/png",
            "application/pdf",
            "application/gzip",
        ] {
            assert!(!is_text(Some(header)), "{header}");
        }
        assert_eq!(
            text_of(Some("text/html; charset=iso-8859-1"), LATIN1).as_deref(),
            Some("Café Müller, Straße")
        );
        assert_eq!(text_of(Some("application/octet-stream"), LATIN1), None);
        assert_eq!(
            text_of(Some("application/octet-stream"), b"plain").as_deref(),
            Some("plain")
        );
        // A body of no declared type is text when it starts as markup does
        // or with a byte order mark, or when it is UTF-8; bytes that are
        // neither stay null, as a browser's sniffing shows no page of them.
        let mut page = b"  <!DOCTYPE html><title>".to_vec();
        page.extend_from_slice(LATIN1);
        assert_eq!(
            text_of(None, &page).as_deref(),
            Some("  <!DOCTYPE html><title>Café Müller, Straße")
        );
        let mut tagged = b"<p>".to_vec();
        tagged.extend_from_slice(LATIN1);
        assert_eq!(
            text_of(Some(""), &tagged).as_deref(),
            Some("<p>Café Müller, Straße")
        );
        assert_eq!(
            text_of(None, b"\xef\xbb\xbfplain").as_deref(),
            Some("plain")
        );
        assert_eq!(text_of(None, b"plain \xc3\xa9").as_deref(), Some("plain é"));
        assert_eq!(
            text_of(None, &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0xff]),
            None
        );
        assert_eq!(text_of(None, LATIN1), None);
        // A tag that only begins like one of the patterns is no page.
        let mut pre = b"<pre>".to_vec();
        pre.extend_from_slice(LATIN1);
        assert_eq!(text_of(None, &pre), None);
        assert_eq!(charset_of("text/html"), None);
        assert_eq!(charset_of("text/html; charset="), None);
        assert_eq!(charset_of("text/html; q=1; charset=sjis"), Some(SHIFT_JIS));
    }
}
