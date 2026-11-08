//! How the http path reads a body as text (spec §9, F100): by the
//! character encoding a browser would take for it, as the Encoding Standard
//! and the browsers decide it, so that levels 0 and 1 read what levels 2
//! and 3 read in a browser (F160, F172).
//!
//! In order, the first that answers:
//!
//! 1. a byte order mark (UTF-8, UTF-16LE, UTF-16BE), which wins over any
//!    declaration;
//! 2. the `charset` parameter of `Content-Type`, read as an Encoding
//!    Standard label (`latin1`, `iso-8859-1` and `us-ascii` are all
//!    windows-1252, `shift_jis` and `sjis` are `Shift_JIS`);
//! 3. for an HTML document (or a body that does not say what it is), the
//!    `<meta charset>` or `<meta http-equiv="content-type">` that Chrome's
//!    scan for it finds ([`prescan`]): the markup is read as its tokenizer
//!    reads it, so that a `<meta>` written inside a script, a style, a
//!    title or a comment is none (F172), and the scan goes on past the
//!    first [`PRESCAN_BYTES`] for as long as the document's head lasts, as
//!    a browser honours a late `<meta>` there (F160); a `<meta>` inside
//!    `<noscript>` counts, as in Chrome (Firefox's scan runs with scripting
//!    on and skips it); for an XML document, the `encoding` of its XML
//!    declaration;
//! 4. what a browser's detector guesses for the bytes ([`detect`]), never
//!    UTF-8: neither Chrome nor Firefox takes an undeclared page from the
//!    web for UTF-8, so that no author comes to rely on it, and both read
//!    one as windows-1252 or as the legacy encoding its bytes look like
//!    (`Shift_JIS`, GBK, EUC-KR, windows-1251 and the others, F160). An
//!    undeclared UTF-8 page therefore reads as mojibake, as it does in both
//!    browsers (F172). An XML document that declares nothing is UTF-8.
//!
//! JSON is UTF-8 unless a byte order mark says otherwise (RFC 8259). Bytes
//! that are not valid in the chosen encoding are replaced, as a browser
//! replaces them.

use std::borrow::Cow;

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252, X_USER_DEFINED};
use url::Url;

/// How many bytes of a document the `<meta>` scan always reads, as the HTML
/// standard's prescan and Chrome's scan have it. Past them the scan goes on
/// only while the document's head lasts.
pub const PRESCAN_BYTES: usize = 1024;

/// How many bytes of an undeclared body the detector looks at.
pub const DETECTOR_BYTES: usize = 1024 * 1024;

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
    /// The body is JSON, or XML that declares nothing: UTF-8.
    Utf8,
    /// Nothing was declared: what a browser's detector guesses for the
    /// bytes, never UTF-8.
    Detected,
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
/// `Content-Type`, when it had one. [`decode_from`] with no address: the
/// detector guesses as for a `.com` site.
pub fn decode<'a>(content_type: Option<&str>, body: &'a [u8]) -> Decoded<'a> {
    decode_from(None, content_type, body)
}

/// Reads a body that was loaded from `url` as text by the encoding a
/// browser would take for it (see the module's documentation). The address
/// matters only to an undeclared body: its top-level domain is the
/// detector's hint, as it is a browser's.
pub fn decode_from<'a>(
    url: Option<&Url>,
    content_type: Option<&str>,
    body: &'a [u8],
) -> Decoded<'a> {
    decode_cut_from(url, content_type, body, false)
}

/// Reads a body as [`decode_from`] does; when it was `cut` at a byte limit
/// (spec §9, F122), without the character the cut fell inside: a sequence
/// that the end of the bytes leaves incomplete in the chosen encoding is
/// left out, not replaced, as a browser's capture steps back to a whole
/// character, so that what is said of the page is said of its first part
/// and the text is no longer than the bytes allow (QA round three's
/// extract-08). A sequence that is invalid before the end is replaced as
/// always.
pub fn decode_cut_from<'a>(
    url: Option<&Url>,
    content_type: Option<&str>,
    body: &'a [u8],
    cut: bool,
) -> Decoded<'a> {
    let read = |bytes: &'a [u8], encoding: &'static Encoding, by: DecidedBy| {
        if cut {
            read_cut(bytes, encoding, by)
        } else {
            read(bytes, encoding, by)
        }
    };
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
    if kind == Kind::Xml {
        return read(body, UTF_8, DecidedBy::Utf8);
    }
    read(body, detect(url, body), DecidedBy::Detected)
}

/// What a browser's detector guesses an undeclared body loaded from `url`
/// is written in: Firefox's own detector (chardetng, which Chrome's agrees
/// with on the legacy encodings of the web), told the site's top-level
/// domain and never allowed to answer UTF-8 or ISO-2022-JP, as a browser
/// never is for a page from the web. A body of ASCII alone is
/// windows-1252, which reads it as it is.
pub fn detect(url: Option<&Url>, body: &[u8]) -> &'static Encoding {
    let mut detector = EncodingDetector::new(Iso2022JpDetection::Deny);
    detector.feed(&body[..body.len().min(DETECTOR_BYTES)], true);
    let tld = top_level_domain(url);
    detector.guess(tld.as_deref().map(str::as_bytes), Utf8Detection::Deny)
}

/// The last label of a URL's host name, in the form the detector takes:
/// lower-case ASCII (an internationalized one in its Punycode form), no
/// dot. `None` for an address, and for a label the detector would refuse.
fn top_level_domain(url: Option<&Url>) -> Option<String> {
    let host = url?.domain()?.trim_end_matches('.');
    let label = host.rsplit('.').next()?.to_ascii_lowercase();
    let usable = !label.is_empty()
        && label
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    usable.then_some(label)
}

/// A body as text where it is text: decoded when its `Content-Type` says it
/// is text, or says nothing and the body starts as markup does or with a
/// byte order mark (what a browser's MIME sniffing takes for a page); as it
/// is when it is UTF-8 whatever it says; `None` otherwise, so that bytes of
/// no declared type are not shown as windows-1252. [`text_of_from`] with no
/// address.
pub fn text_of(content_type: Option<&str>, body: &[u8]) -> Option<String> {
    text_of_from(None, content_type, body)
}

/// [`text_of`] for a body loaded from `url`, whose top-level domain is the
/// detector's hint for an undeclared body ([`decode_from`]).
pub fn text_of_from(url: Option<&Url>, content_type: Option<&str>, body: &[u8]) -> Option<String> {
    let typed = content_type.is_some_and(|value| !essence(Some(value)).is_empty());
    let text = if typed {
        is_text(content_type)
    } else {
        Encoding::for_bom(body).is_some() || sniffs_as_markup(body)
    };
    if text {
        return Some(decode_from(url, content_type, body).text.into_owned());
    }
    std::str::from_utf8(body).ok().map(str::to_owned)
}

/// Whether a body that declares no type is text, as [`text_of`] reads it:
/// it has a byte order mark, begins as markup does, or is UTF-8 (MIME
/// sniffing). Other bytes are no document, and nothing is extracted from
/// them (F180).
pub fn untyped_is_text(body: &[u8]) -> bool {
    Encoding::for_bom(body).is_some() || sniffs_as_markup(body) || std::str::from_utf8(body).is_ok()
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

/// Whether a body of no declared type is HTML to a browser's MIME
/// sniffing: it begins as one of the standard's HTML patterns. The XML
/// declaration makes it XML, and a body that begins otherwise (a JSON
/// object, words) is text. A crawl takes links from such a body only
/// (crawl-01).
pub fn sniffs_as_html(body: &[u8]) -> bool {
    sniffs_as_markup(body)
        && !body
            .trim_ascii_start()
            .get(..5)
            .is_some_and(|start| start.eq_ignore_ascii_case(b"<?xml"))
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

/// [`read`] of bytes that were cut: a sequence the end leaves incomplete is
/// held back by the decoder (`last: false`) and so left out.
fn read_cut<'a>(bytes: &'a [u8], encoding: &'static Encoding, by: DecidedBy) -> Decoded<'a> {
    let mut decoder = encoding.new_decoder_without_bom_handling();
    let Some(capacity) = decoder.max_utf8_buffer_length(bytes.len()) else {
        return read(bytes, encoding, by);
    };
    let mut text = String::with_capacity(capacity);
    let (_, _, malformed) = decoder.decode_to_string(bytes, &mut text, false);
    Decoded {
        text: Cow::Owned(text),
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

/// The elements a document's head may hold before Chrome's scan takes the
/// head for ended: any other start tag, and any end tag but theirs, ends
/// it (`<html>` and `<head>` start tags do not).
const HEAD_ELEMENTS: [&[u8]; 8] = [
    b"script",
    b"noscript",
    b"style",
    b"link",
    b"meta",
    b"object",
    b"title",
    b"base",
];

/// The elements whose content the tokenizer reads as text, not markup, as
/// Chrome's scan sets it after their start tag: RCDATA, RAWTEXT, script
/// data and plain text (`<noscript>` and `<noembed>` are markup there: the
/// scan runs with scripting and plugins off).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextOf {
    /// `<title>`, `<textarea>`: text up to their end tag.
    RcData,
    /// `<style>`, `<xmp>`, `<iframe>`, `<noframes>`: text up to their end
    /// tag.
    RawText,
    /// `<script>`: script data, with its escaped states.
    Script,
    /// `<plaintext>`: text to the end of the document.
    PlainText,
}

fn text_of_element(name: &[u8]) -> Option<TextOf> {
    match name {
        b"title" | b"textarea" => Some(TextOf::RcData),
        b"style" | b"xmp" | b"iframe" | b"noframes" => Some(TextOf::RawText),
        b"script" => Some(TextOf::Script),
        b"plaintext" => Some(TextOf::PlainText),
        _ => None,
    }
}

/// Whether `bytes` at `at` hold the end tag `</name` the tokenizer takes
/// for one that closes a text element: the name, then white space, `/` or
/// `>`.
fn is_end_tag(bytes: &[u8], at: usize, name: &[u8]) -> bool {
    bytes.get(at) == Some(&b'<')
        && bytes.get(at + 1) == Some(&b'/')
        && starts_with_at(bytes, at + 2, name)
        && bytes
            .get(at + 2 + name.len())
            .is_some_and(|&next| is_space(next) || next == b'/' || next == b'>')
}

/// Where the text of an RCDATA or RAWTEXT element that starts at `at` ends:
/// the `<` of its end tag; `None` when the document ends first.
fn raw_text_end(bytes: &[u8], mut at: usize, name: &[u8]) -> Option<usize> {
    while at < bytes.len() {
        if is_end_tag(bytes, at, name) {
            return Some(at);
        }
        at += 1;
    }
    None
}

/// Where the text of a `<script>` that starts at `at` ends: the `<` of the
/// `</script>` that closes it, read through the tokenizer's escaped states
/// (`<!--` inside a script, and a `<script>` inside that, whose
/// `</script>` does not end the outer one); `None` when the document ends
/// first.
fn script_end(bytes: &[u8], mut at: usize) -> Option<usize> {
    #[derive(PartialEq, Eq)]
    enum State {
        Data,
        Escaped,
        DoubleEscaped,
    }
    let mut state = State::Data;
    while at < bytes.len() {
        let here = &bytes[at..];
        match state {
            State::Data if here.starts_with(b"<!--") => {
                state = State::Escaped;
                at += 4;
                continue;
            }
            State::Escaped | State::DoubleEscaped if here.starts_with(b"-->") => {
                state = State::Data;
                at += 3;
                continue;
            }
            State::Data | State::Escaped if is_end_tag(bytes, at, b"script") => return Some(at),
            State::DoubleEscaped if is_end_tag(bytes, at, b"script") => {
                state = State::Escaped;
                at += 8;
                continue;
            }
            State::Escaped
                if here.first() == Some(&b'<')
                    && starts_with_at(bytes, at + 1, b"script")
                    && bytes
                        .get(at + 7)
                        .is_some_and(|&next| is_space(next) || next == b'/' || next == b'>') =>
            {
                state = State::DoubleEscaped;
                at += 7;
                continue;
            }
            _ => {}
        }
        at += 1;
    }
    None
}

/// Where a comment that starts at `at` (its `<!--`) ends, past its `-->`:
/// `<!-->` and `<!--->` end where they start, and `--!>` ends one too, as
/// the tokenizer reads them; `None` when the document ends first.
fn comment_end(bytes: &[u8], at: usize) -> Option<usize> {
    let body = at + 4;
    if bytes.get(body) == Some(&b'>') {
        return Some(body + 1);
    }
    if bytes.get(body..body + 2) == Some(b"->") {
        return Some(body + 2);
    }
    let close = find(bytes, body, b"-->").map(|end| end + 3);
    let bang = find(bytes, body, b"--!>").map(|end| end + 4);
    match (close, bang) {
        (Some(close), Some(bang)) => Some(close.min(bang)),
        (close, bang) => close.or(bang),
    }
}

/// The name of a tag that starts at `at` (its first letter), lower case,
/// and the position after it.
fn tag_name(bytes: &[u8], mut at: usize) -> (Vec<u8>, usize) {
    let mut name = Vec::new();
    while let Some(&byte) = bytes.get(at) {
        if is_space(byte) || byte == b'/' || byte == b'>' {
            break;
        }
        name.push(byte.to_ascii_lowercase());
        at += 1;
    }
    (name, at)
}

/// The encoding a document declares in its markup, found as Chrome finds
/// it (its `HTMLMetaCharsetParser`): the markup is read token by token as
/// the HTML tokenizer reads it, so that what stands in a comment, in a
/// script, a style, a title or a text area, or inside another tag's
/// attribute is no `<meta>` (F172); the first `<meta>` that declares an
/// encoding the Encoding Standard knows counts; and the scan stops once it
/// has read [`PRESCAN_BYTES`] bytes and the document's head has ended (a
/// start tag, or an end tag, of an element a head does not hold), so that
/// a `<meta>` past the first 1024 bytes still counts while the head lasts
/// (F160). A tag the bytes end inside declares nothing.
pub fn prescan(body: &[u8]) -> Option<&'static Encoding> {
    let bytes = body;
    let mut at = 0;
    let mut in_head = true;
    while at < bytes.len() {
        // Between two tokens: the scan's end, as Chrome checks it.
        if !in_head && at >= PRESCAN_BYTES {
            return None;
        }
        if bytes[at] != b'<' {
            // Text, up to the next tag.
            at = find(bytes, at, b"<").unwrap_or(bytes.len());
            continue;
        }
        let next = bytes.get(at + 1).copied();
        match next {
            Some(b'!') if bytes[at..].starts_with(b"<!--") => {
                at = comment_end(bytes, at)?;
            }
            Some(b'!' | b'?') => {
                // A doctype or a bogus comment: to its `>`.
                at = find(bytes, at, b">")? + 1;
            }
            Some(b'/') => match bytes.get(at + 2) {
                Some(letter) if letter.is_ascii_alphabetic() => {
                    let (name, after) = tag_name(bytes, at + 2);
                    at = after;
                    while attribute(bytes, &mut at).is_some() {}
                    if at >= bytes.len() {
                        return None;
                    }
                    at += 1;
                    if !HEAD_ELEMENTS.contains(&name.as_slice()) {
                        in_head = false;
                    }
                }
                Some(b'>') => at += 3,
                Some(_) => at = find(bytes, at, b">")? + 1,
                None => return None,
            },
            Some(letter) if letter.is_ascii_alphabetic() => {
                let (name, after) = tag_name(bytes, at + 1);
                at = after;
                if name == b"meta" {
                    if let Some(encoding) = meta(bytes, &mut at) {
                        return Some(encoding);
                    }
                } else {
                    while attribute(bytes, &mut at).is_some() {}
                }
                if at >= bytes.len() {
                    return None;
                }
                at += 1;
                let allowed =
                    HEAD_ELEMENTS.contains(&name.as_slice()) || name == b"html" || name == b"head";
                if !allowed {
                    in_head = false;
                }
                at = match text_of_element(&name) {
                    None => at,
                    Some(TextOf::RcData | TextOf::RawText) => raw_text_end(bytes, at, &name)?,
                    Some(TextOf::Script) => script_end(bytes, at)?,
                    Some(TextOf::PlainText) => return None,
                };
            }
            // A `<` that opens nothing is text.
            _ => at += 1,
        }
    }
    None
}

/// The attributes of one `<meta>`, `at` just after its name, read as
/// Chrome reads them: a repeated attribute is dropped, and the first
/// `charset` or `content` that names an encoding decides, `content` only
/// with `http-equiv="content-type"`. The encoding it declares, when it
/// declares one the Encoding Standard knows.
fn meta(bytes: &[u8], at: &mut usize) -> Option<&'static Encoding> {
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut got_pragma = false;
    // The label and whether it came from `content` (and needs the pragma).
    let mut declared: Option<(Vec<u8>, bool)> = None;
    while let Some((name, value)) = attribute(bytes, at) {
        if seen.contains(&name) {
            continue;
        }
        match name.as_slice() {
            b"http-equiv" => got_pragma |= value.trim_ascii().eq_ignore_ascii_case(b"content-type"),
            b"charset" if declared.is_none() => {
                let label = value.trim_ascii();
                if !label.is_empty() {
                    declared = Some((label.to_vec(), false));
                }
            }
            b"content" if declared.is_none() => {
                if let Some(label) = charset_in_content(&value) {
                    declared = Some((label.to_vec(), true));
                }
            }
            _ => {}
        }
        seen.push(name);
    }
    // A tag the bytes end inside declares nothing: a browser ignores an
    // unfinished tag.
    if *at >= bytes.len() {
        return None;
    }
    let (label, needs_pragma) = declared?;
    if needs_pragma && !got_pragma {
        return None;
    }
    Encoding::for_label(&label).map(as_declared_in_markup)
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
                value.push(byte);
            }
        }
        Some(_) => {
            while let Some(&byte) = bytes.get(*at) {
                if is_space(byte) || byte == b'>' {
                    break;
                }
                value.push(byte);
                *at += 1;
            }
        }
    }
    // Chrome reads a `<meta>` with its tokenizer, which decodes character
    // references in a value (F212): `charset=&quot;shift_jis&quot;` in a
    // `content` is a quoted label.
    let mut value = unescape(&value);
    value.make_ascii_lowercase();
    Some((name, value))
}

/// The named character references whose character is ASCII, the only
/// ones that can change how a label or a `content` value reads: any other
/// character makes a label no label, decoded or not. The first eight are
/// the legacy ones, decoded without their `;` too.
const ASCII_REFERENCES: [(&[u8], u8); 45] = [
    (b"amp", b'&'),
    (b"AMP", b'&'),
    (b"lt", b'<'),
    (b"LT", b'<'),
    (b"gt", b'>'),
    (b"GT", b'>'),
    (b"quot", b'"'),
    (b"QUOT", b'"'),
    (b"Tab", b'\t'),
    (b"NewLine", b'\n'),
    (b"excl", b'!'),
    (b"num", b'#'),
    (b"dollar", b'$'),
    (b"percnt", b'%'),
    (b"apos", b'\''),
    (b"lpar", b'('),
    (b"rpar", b')'),
    (b"ast", b'*'),
    (b"midast", b'*'),
    (b"plus", b'+'),
    (b"comma", b','),
    (b"period", b'.'),
    (b"sol", b'/'),
    (b"colon", b':'),
    (b"semi", b';'),
    (b"equals", b'='),
    (b"quest", b'?'),
    (b"commat", b'@'),
    (b"lsqb", b'['),
    (b"lbrack", b'['),
    (b"bsol", b'\\'),
    (b"rsqb", b']'),
    (b"rbrack", b']'),
    (b"Hat", b'^'),
    (b"lowbar", b'_'),
    (b"UnderBar", b'_'),
    (b"grave", b'`'),
    (b"DiacriticalGrave", b'`'),
    (b"lcub", b'{'),
    (b"lbrace", b'{'),
    (b"verbar", b'|'),
    (b"vert", b'|'),
    (b"VerticalLine", b'|'),
    (b"rcub", b'}'),
    (b"rbrace", b'}'),
];

/// How many of [`ASCII_REFERENCES`] are legacy references, which need no
/// `;`.
const LEGACY_REFERENCES: usize = 8;

/// An attribute value with its character references decoded as the HTML
/// tokenizer decodes them in an attribute: numeric ones always, named ones
/// of [`ASCII_REFERENCES`] when they end with `;`, and a legacy one
/// without it unless an `=` or a letter or digit follows. A reference to a
/// character that is not ASCII is written out as UTF-8; a named one this
/// table does not hold stays as it is, which reads the same (no label).
fn unescape(raw: &[u8]) -> Vec<u8> {
    if !raw.contains(&b'&') {
        return raw.to_vec();
    }
    let mut out = Vec::with_capacity(raw.len());
    let mut at = 0;
    while at < raw.len() {
        if raw[at] != b'&' {
            out.push(raw[at]);
            at += 1;
            continue;
        }
        if let Some((character, used)) = numeric_reference(&raw[at + 1..]) {
            let mut buffer = [0; 4];
            out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            at += 1 + used;
            continue;
        }
        if let Some((byte, used)) = named_reference(&raw[at + 1..]) {
            out.push(byte);
            at += 1 + used;
            continue;
        }
        out.push(b'&');
        at += 1;
    }
    out
}

/// A numeric reference after its `&` (`#65;`, `#x41`): the character
/// and how many bytes it took, `;` included.
fn numeric_reference(rest: &[u8]) -> Option<(char, usize)> {
    let rest = rest.strip_prefix(b"#")?;
    let (radix, start) = match rest.first() {
        Some(b'x' | b'X') => (16, 1),
        _ => (10, 0),
    };
    let digits = rest[start..]
        .iter()
        .take_while(|byte| char::from(**byte).is_digit(radix))
        .count();
    if digits == 0 {
        return None;
    }
    let mut value: u32 = 0;
    for byte in &rest[start..start + digits] {
        let digit = char::from(*byte).to_digit(radix).unwrap_or_default();
        value = value.saturating_mul(radix).saturating_add(digit);
    }
    let mut used = 1 + start + digits;
    if rest.get(start + digits) == Some(&b';') {
        used += 1;
    }
    let character = match value {
        0 => char::REPLACEMENT_CHARACTER,
        // The C1 range reads as windows-1252 would read the byte.
        0x80..=0x9f => u8::try_from(value)
            .ok()
            .map_or(char::REPLACEMENT_CHARACTER, |byte| {
                WINDOWS_1252
                    .decode_without_bom_handling(&[byte])
                    .0
                    .chars()
                    .next()
                    .unwrap_or(char::REPLACEMENT_CHARACTER)
            }),
        _ => char::from_u32(value).unwrap_or(char::REPLACEMENT_CHARACTER),
    };
    Some((character, used))
}

/// A named reference of [`ASCII_REFERENCES`] after its `&`: its
/// character and how many bytes it took.
fn named_reference(rest: &[u8]) -> Option<(u8, usize)> {
    for (index, (name, byte)) in ASCII_REFERENCES.iter().enumerate() {
        let Some(after) = rest.strip_prefix(*name) else {
            continue;
        };
        if after.first() == Some(&b';') {
            return Some((*byte, name.len() + 1));
        }
        let legacy = index < LEGACY_REFERENCES;
        let continues = after
            .first()
            .is_some_and(|next| *next == b'=' || next.is_ascii_alphanumeric());
        if legacy && !continues {
            return Some((*byte, name.len()));
        }
    }
    None
}

/// The standard's "extract a character encoding from a meta element": the
/// label a `content` value such as `text/html; charset=shift_jis` names,
/// when it names one.
fn charset_in_content(content: &[u8]) -> Option<&[u8]> {
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
    (!value.is_empty()).then_some(value)
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
    use encoding_rs::{
        EUC_JP, EUC_KR, Encoding, GBK, KOI8_R, SHIFT_JIS, UTF_8, UTF_16LE, WINDOWS_1251,
        WINDOWS_1252,
    };
    use url::Url;

    use super::{
        DecidedBy, charset_of, decode, decode_cut_from, decode_from, detect, is_text, prescan,
        sniffs_as_html,
        text_of, text_of_from, top_level_domain, untyped_is_text,
    };

    /// QA round three's extract-08: a body cut inside a character reads
    /// up to the last whole one, in every encoding; a whole body, and a
    /// sequence invalid before the end, read as always.
    #[test]
    fn a_body_cut_inside_a_character_ends_at_the_last_whole_one() {
        let utf8 = Some("text/html; charset=utf-8");
        for character in ["é", "✓", "🦀"] {
            let page = character.repeat(10);
            let bytes = page.as_bytes();
            for cut in 1..bytes.len() {
                let read = decode_cut_from(None, utf8, &bytes[..cut], true);
                let whole = cut / character.len() * character.len();
                assert_eq!(read.text, page[..whole], "{character} cut at {cut}");
                assert!(!read.text.contains('\u{fffd}'));
                assert!(read.text.len() <= cut);
                assert!(!read.malformed);
                // Uncut, the same bytes end in a replacement.
                let uncut = decode_from(None, utf8, &bytes[..cut]);
                assert_eq!(uncut.text.ends_with('\u{fffd}'), cut != whole);
            }
        }
        // Shift_JIS and UTF-16 too.
        let (sjis, _, _) = SHIFT_JIS.encode("日本語");
        let read = decode_cut_from(None, Some("text/html; charset=shift_jis"), &sjis[..3], true);
        assert_eq!(read.text, "日");
        let mut utf16 = vec![0xff, 0xfe];
        utf16.extend("ab".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode_cut_from(None, None, &utf16[..5], true).text, "a");
        // A broken sequence before the end is the page's, replaced.
        let broken = b"a\xffb\xe2\x9c";
        let read = decode_cut_from(None, utf8, broken, true);
        assert_eq!(read.text, "a\u{fffd}b");
        assert!(read.malformed);
        assert_eq!(decode_cut_from(None, utf8, b"whole", false).text, "whole");
    }

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
    fn without_a_declaration_the_detector_guesses_and_never_utf8() {
        // An undeclared UTF-8 page reads as both browsers read it: as
        // windows-1252, mojibake and all (F172, http-05).
        let body = page("", "Café Müller".as_bytes());
        let read = decode(Some("text/html"), &body);
        assert_eq!(
            (read.encoding, read.by),
            (WINDOWS_1252, DecidedBy::Detected)
        );
        assert!(read.text.contains("<title>CafÃ© MÃ¼ller</title>"));
        let body = page("", "日本語のテキスト".as_bytes());
        assert_ne!(decode(Some("text/html"), &body).encoding, UTF_8);
        let body = page("", LATIN1);
        let read = decode(None, &body);
        assert_eq!(
            (read.encoding, read.by),
            (WINDOWS_1252, DecidedBy::Detected)
        );
        assert!(read.text.contains("<p>Café Müller, Straße</p>"));
        // windows-1252's own letters (an em dash, curly quotes).
        let read = decode(Some("text/plain"), b"\x93quoted\x94 \x97 dash");
        assert_eq!(read.text, "\u{201c}quoted\u{201d} \u{2014} dash");
    }

    #[test]
    fn the_prescan_keeps_to_the_standards_rules() {
        // A meta in a comment says nothing; one after the first 1024 bytes
        // counts while the head lasts (F160), not after it.
        assert_eq!(prescan(b"<!-- <meta charset=shift_jis> --><p>x"), None);
        let late = format!("{}<meta charset=shift_jis>", " ".repeat(1024));
        assert_eq!(prescan(late.as_bytes()), Some(SHIFT_JIS));
        let late = format!("<p>{}<meta charset=shift_jis>", " ".repeat(1024));
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
        // Past 1024 bytes and past the head, nothing counts: the scan ends
        // at the first token that starts after both, as Chrome checks
        // between tokens; a tag that starts before byte 1024 is read whole.
        let mut long = b"<body>".to_vec();
        long.resize(1023, b' ');
        long.extend_from_slice(b"<meta charset=shift_jis>");
        assert_eq!(prescan(&long), Some(SHIFT_JIS));
        long.insert(6, b' ');
        assert_eq!(prescan(&long), None);
        assert_eq!(decode(Some("text/html"), &long).by, DecidedBy::Detected);
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
    /// F212: Chrome reads a `<meta>` with its tokenizer, so a character
    /// reference in a value is the character it names.
    #[test]
    fn a_character_reference_in_a_meta_is_read_as_its_character() {
        for head in [
            "<meta http-equiv=content-type content=\"text/html; charset=&quot;shift_jis&quot;\">",
            "<meta http-equiv=content-type content='text/html; charset=&#34;shift_jis&#34;'>",
            "<meta http-equiv=content-type content=\"text/html&semi; charset&equals;shift_jis\">",
            "<meta charset=\"&#x73;hift&#95;jis\">",
            "<meta charset=\"shift&lowbar;jis\">",
            "<meta charset=shift_jis&Tab;>",
        ] {
            assert_eq!(prescan(head.as_bytes()), Some(SHIFT_JIS), "{head}");
        }
        // A legacy reference needs no `;`, unless a letter, a digit or `=`
        // follows, as in any attribute.
        assert_eq!(
            prescan(
                b"<meta http-equiv=content-type content=\"text/html; charset=&quotkoi8-r&quot\">"
            ),
            None
        );
        assert_eq!(
            prescan(b"<meta http-equiv=content-type content=\"text/html; charset=&quot koi8-r\">"),
            None
        );
        assert_eq!(
            prescan(
                b"<meta http-equiv=content-type content=\"text/html; charset=&quot;koi8-r&quot\">"
            ),
            Some(KOI8_R)
        );
        // A reference to what no label holds leaves no label, decoded or not.
        assert_eq!(prescan(b"<meta charset=\"utf&#x2014;8\">"), None);
        assert_eq!(prescan(b"<meta charset=\"utf&mdash;8\">"), None);
        assert_eq!(prescan(b"<meta charset=\"&#0;utf-8\">"), None);
        // A bare `&` and an unfinished reference are text.
        assert_eq!(prescan(b"<meta charset=\"windows-1251&\">"), None);
        assert_eq!(prescan(b"<meta charset=\"windows&#-1251\">"), None);
        assert_eq!(
            prescan(b"<meta charset=\"windows&#x2d1251\">"),
            None,
            "the hex digits run on"
        );
        assert_eq!(
            prescan(b"<meta charset=\"windows&#45;1251\">"),
            Some(WINDOWS_1251)
        );
    }

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

    /// Chrome's scan reads the markup as the tokenizer does: a `<meta>` in
    /// a script's, a style's or a title's text is none (F172, http-06), and
    /// `<noscript>` is markup, the scan running with scripting off.
    #[test]
    fn a_meta_in_the_text_of_a_script_a_style_or_a_title_is_none() {
        assert_eq!(
            prescan(
                b"<script>var t = \"<meta charset=utf-8>\";</script><meta charset=windows-1252>"
            ),
            Some(WINDOWS_1252)
        );
        assert_eq!(
            prescan(
                b"<head><script>document.write('<meta charset=koi8-r>')</script><title>x</title>"
            ),
            None
        );
        for (open, close, name) in [
            ("<style>", "</style>", "style"),
            ("<title>", "</title>", "title"),
            ("<textarea>", "</textarea>", "textarea"),
            ("<xmp>", "</xmp>", "xmp"),
            ("<iframe>", "</iframe >", "iframe"),
            ("<noframes>", "</NOFRAMES>", "noframes"),
            ("<script type=text/template>", "</script/>", "script"),
        ] {
            let page = format!("{open}<meta charset=koi8-r>{close}<meta charset=shift_jis>");
            assert_eq!(prescan(page.as_bytes()), Some(SHIFT_JIS), "{open}");
            // An end tag that only begins like the element's is text.
            let page = format!("{open}</{name}x><meta charset=koi8-r>");
            assert_eq!(prescan(page.as_bytes()), None, "{open}");
        }
        assert_eq!(
            prescan(b"<noscript><meta charset=koi8-r></noscript>"),
            Some(KOI8_R)
        );
        // Everything after <plaintext> is text.
        assert_eq!(prescan(b"<plaintext><meta charset=koi8-r>"), None);
        // A script's escaped states: inside <!--, a <script> nests, and its
        // </script> does not end the outer one.
        assert_eq!(
            prescan(
                b"<script><!--<script></script><meta charset=koi8-r></script><meta charset=shift_jis>"
            ),
            Some(SHIFT_JIS)
        );
        assert_eq!(
            prescan(b"<script><!-- x --><meta charset=koi8-r></script><meta charset=euc-jp>"),
            Some(EUC_JP)
        );
        assert_eq!(
            prescan(b"<script><!--</script><meta charset=euc-jp>"),
            Some(EUC_JP)
        );
        // Comments end as the tokenizer ends them.
        assert_eq!(prescan(b"<!---><meta charset=euc-jp>"), Some(EUC_JP));
        assert_eq!(prescan(b"<!-- x --!><meta charset=euc-jp>"), Some(EUC_JP));
        assert_eq!(
            prescan(b"<!-- x --!> --><meta charset=euc-jp>"),
            Some(EUC_JP)
        );
        // A text element the document ends inside declares nothing after it.
        for open in ["<script>", "<style>", "<title>", "<!-- "] {
            assert_eq!(
                prescan(format!("{open}<meta charset=euc-jp>").as_bytes()),
                None
            );
        }
        // A start tag's name ends at white space, `/` or `>`.
        assert_eq!(prescan(b"<meta/charset=euc-jp>"), Some(EUC_JP));
        assert_eq!(prescan(b"</>x<meta charset=euc-jp>"), Some(EUC_JP));
        assert_eq!(prescan(b"</"), None);
        assert_eq!(prescan(b"< meta charset=euc-jp>"), None);
    }

    /// The scan goes on past 1024 bytes while the document's head lasts, as
    /// Chrome's does and as Firefox honours a late `<meta>` (F160): what
    /// ends the head is a start tag of an element a head does not hold, or
    /// an end tag but of the head's own elements.
    #[test]
    fn a_late_meta_counts_while_the_head_lasts() {
        let script = format!("<script>{}</script>", "var x = 1;\n".repeat(200));
        let page = format!(
            "<!doctype html><html><head><title>t</title>{script}<link rel=stylesheet href=a.css><meta charset=shift_jis>"
        );
        assert!(page.len() > 2000);
        assert_eq!(prescan(page.as_bytes()), Some(SHIFT_JIS));
        for ends in ["<body>", "<div>", "</head>", "</html>", "<p>"] {
            let page = format!("<head>{script}{ends}<meta charset=shift_jis>");
            assert_eq!(prescan(page.as_bytes()), None, "{ends}");
        }
        for stays in [
            "</title>",
            "</script>",
            "<base href=/>",
            "<object>",
            "<noscript>",
            "<html>",
            "<head>",
        ] {
            let page = format!("<head>{script}{stays}<meta charset=shift_jis>");
            assert_eq!(prescan(page.as_bytes()), Some(SHIFT_JIS), "{stays}");
        }
        // Inside the first 1024 bytes the head's end changes nothing.
        assert_eq!(
            prescan(b"<html><body><p>x</p><meta charset=shift_jis>"),
            Some(SHIFT_JIS)
        );
    }

    fn written(encoding: &'static Encoding, text: &str) -> Vec<u8> {
        let (bytes, _, unmappable) = encoding.encode(text);
        assert!(!unmappable, "{}", encoding.name());
        page("", &bytes)
    }

    /// An undeclared page in a legacy encoding reads as a browser's
    /// detector names it (F160), and the site's top-level domain is its
    /// hint.
    #[test]
    fn an_undeclared_legacy_page_is_named_as_a_browser_names_it() {
        for (encoding, text) in [
            (
                SHIFT_JIS,
                "日本語のテキストです。これは文字コードを判定するための文章で、ひらがなとカタカナと漢字が入っています。",
            ),
            (
                EUC_KR,
                "한국어 텍스트입니다. 이것은 문자 인코딩을 판별하기 위한 문장이며 한글이 많이 들어 있습니다.",
            ),
            (
                GBK,
                "这是一段中文文本。我们用它来检测字符编码，其中包含许多常用的汉字和标点符号。",
            ),
            (
                WINDOWS_1251,
                "Съешь же ещё этих мягких французских булок, да выпей чаю. Это русский текст для проверки.",
            ),
        ] {
            let body = written(encoding, &text.repeat(3));
            let read = decode(Some("text/html"), &body);
            assert_eq!(
                (read.encoding, read.by),
                (encoding, DecidedBy::Detected),
                "{}",
                encoding.name()
            );
            assert!(read.text.contains(text), "{}", encoding.name());
            assert!(!read.malformed, "{}", encoding.name());
        }
        // The fixture site's undeclared page (tests/e2e/fetch.sh): 日本語 again
        // and again among ASCII, from a top-level domain the detector does
        // not know.
        let mut fixture = b"<!doctype html><html lang=\"ja\"><head><title>".to_vec();
        fixture.extend_from_slice(SJIS);
        fixture.extend_from_slice(b"</title></head><body><main><article><h1>");
        fixture.extend_from_slice(SJIS);
        fixture.extend_from_slice(b"</h1><p>");
        fixture.extend_from_slice(&SJIS.repeat(40));
        fixture.extend_from_slice(" wort".repeat(120).as_bytes());
        fixture.extend_from_slice(b"</p></article></main></body></html>");
        let url = Url::parse("http://fixture.kage.test:8088/legacy/undeclared-sjis").unwrap();
        let read = decode_from(Some(&url), Some("text/html"), &fixture);
        assert_eq!((read.encoding, read.by), (SHIFT_JIS, DecidedBy::Detected));
        assert!(read.text.contains("<title>日本語</title>"));
        // ASCII alone is windows-1252, which reads it as it is.
        assert_eq!(detect(None, b"<p>plain words</p>"), WINDOWS_1252);
        let url = Url::parse("http://shop.example.jp/").unwrap();
        let read = decode_from(Some(&url), Some("text/plain"), b"plain");
        assert_eq!(
            (read.text.as_ref(), read.by),
            ("plain", DecidedBy::Detected)
        );
        // The hint is the site's: the same bytes are Shift_JIS from a
        // Japanese site, windows-1252 from a German one, as in Firefox.
        assert_eq!(
            text_of_from(Some(&url), Some("text/html"), "Café".as_bytes()).as_deref(),
            Some("Cafﾃｩ")
        );
        let url = Url::parse("http://shop.example.de/").unwrap();
        assert_eq!(
            text_of_from(Some(&url), Some("text/html"), "Café".as_bytes()).as_deref(),
            Some("CafÃ©")
        );
    }

    #[test]
    fn the_detectors_hint_is_the_hosts_last_label_or_none() {
        let tld = |url: &str| top_level_domain(Some(&Url::parse(url).unwrap()));
        assert_eq!(tld("https://shop.example.co.jp/x").as_deref(), Some("jp"));
        assert_eq!(tld("https://SHOP.Example.DE./").as_deref(), Some("de"));
        assert_eq!(tld("https://пример.рф/").as_deref(), Some("xn--p1ai"));
        assert_eq!(tld("http://192.0.2.1/"), None);
        assert_eq!(tld("http://[2001:db8::1]/"), None);
        assert_eq!(tld("http://localhost/").as_deref(), Some("localhost"));
        assert_eq!(top_level_domain(None), None);
    }
    /// F180: a body of no type is text, and so a document extraction may
    /// read, exactly when [`text_of`] gives it as text.
    /// crawl-01: of a body of no type, only one that begins as HTML is
    /// HTML; the XML declaration, a JSON object and words are not.
    #[test]
    fn an_untyped_body_is_html_only_when_it_begins_as_html() {
        for body in [
            b"<!DOCTYPE html><p>x".as_slice(),
            b"  \n<html lang=en>".as_slice(),
            b"<HEAD>".as_slice(),
            b"<!-- a comment --><p>".as_slice(),
            b"<p>words</p>".as_slice(),
        ] {
            assert!(sniffs_as_html(body), "{body:?}");
        }
        for body in [
            br#"{"html":"<a href=\"/x\">x</a>"}"#.as_slice(),
            br#"<?xml version="1.0"?><doc/>"#.as_slice(),
            br#"<?XML version="1.0"?>"#.as_slice(),
            b"words and <a href=x>a link</a>".as_slice(),
            b"<htmlx>".as_slice(),
            b"".as_slice(),
        ] {
            assert!(!sniffs_as_html(body), "{body:?}");
        }
    }

    #[test]
    fn a_body_of_no_type_is_text_when_raw_body_would_show_it() {
        for body in [
            b"<!doctype html><p>a page".as_slice(),
            b"plain words".as_slice(),
            "UTF-8 \u{2713}".as_bytes(),
            b"\xef\xbb\xbfa byte order mark".as_slice(),
            b"\xff\xfeU\x00T\x00F\x00".as_slice(),
            b"".as_slice(),
        ] {
            assert!(untyped_is_text(body), "{body:?}");
            assert!(text_of(None, body).is_some(), "{body:?}");
        }
        for body in [
            b"\x00\x01\xfe\xe9\x10\x80".as_slice(),
            b"\x89PNG\r\n\x1a\n\x00".as_slice(),
        ] {
            assert!(!untyped_is_text(body), "{body:?}");
            assert!(text_of(None, body).is_none(), "{body:?}");
        }
    }
}
