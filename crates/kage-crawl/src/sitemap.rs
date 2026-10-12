//! Sitemaps (spec §13): the list of its pages a site publishes for whoever
//! asks, as the protocol at sitemaps.org writes it.
//!
//! [`parse`] reads one file as it was fetched: a `urlset` of pages, a
//! `sitemapindex` of further sitemaps, or the plain-text form with one
//! address a line; gzipped or not, which the first two bytes say and not
//! the name. Every address must be an absolute `http` or `https` URL; what
//! is not is left out and counted. A `lastmod` is read in every form the
//! protocol allows (a year, a month, a day, a moment) and left out when it
//! is none of them.
//!
//! A sitemap is somebody else's text. Nothing in it is expanded or
//! followed (the parser knows no document type definition), the size of
//! what a gzipped file unpacks to is capped before it is unpacked, and no
//! more entries are read than the limit says.

use std::io::Read as _;

use chrono::{DateTime, NaiveDate, Utc};
use flate2::read::GzDecoder;
use quick_xml::Reader;
use quick_xml::events::Event;
use url::Url;

/// How much of a sitemap is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The largest file, after unpacking, in bytes. The protocol's own
    /// limit is 50 MiB.
    pub max_bytes: usize,
    /// The most entries read from one file. The protocol's own limit is
    /// 50 000.
    pub max_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 50 * 1024 * 1024,
            max_entries: 50_000,
        }
    }
}

/// One address a sitemap names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The address.
    pub url: Url,
    /// When the site says it last changed.
    pub lastmod: Option<DateTime<Utc>>,
}

/// What kind of file a sitemap is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Its entries are pages.
    Pages,
    /// Its entries are further sitemaps.
    Index,
}

/// A sitemap as it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sitemap {
    /// Pages, or further sitemaps.
    pub kind: Kind,
    /// The entries, in the order of the file.
    pub entries: Vec<Entry>,
    /// Entries left out because their address was no absolute `http` or
    /// `https` URL.
    pub invalid: usize,
    /// Whether the file held more entries than the limit.
    pub truncated: bool,
    /// Whether the file was gzipped.
    pub gzipped: bool,
}

/// Why a file is no sitemap.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SitemapError {
    /// It is larger than the limit, or unpacks to more.
    #[error("the sitemap is larger than {limit} bytes")]
    TooLarge {
        /// The limit in bytes.
        limit: usize,
    },
    /// The gzip stream is damaged.
    #[error("the sitemap is gzipped and cannot be unpacked: {0}")]
    Gzip(String),
    /// It is no well-formed XML.
    #[error("the sitemap is not well-formed XML: {0}")]
    Xml(String),
    /// It is XML, but neither a `urlset` nor a `sitemapindex`.
    #[error("the document is a <{0}>, not a sitemap")]
    NotASitemap(String),
    /// It is empty.
    #[error("the sitemap is empty")]
    Empty,
}

/// The gzip magic number.
const GZIP: [u8; 2] = [0x1f, 0x8b];

fn unpack(body: &[u8], limit: usize) -> Result<Vec<u8>, SitemapError> {
    let mut out = Vec::new();
    // One byte more than the limit is enough to know that it is exceeded.
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    GzDecoder::new(body)
        .take(cap)
        .read_to_end(&mut out)
        .map_err(|error| SitemapError::Gzip(error.to_string()))?;
    if out.len() > limit {
        return Err(SitemapError::TooLarge { limit });
    }
    Ok(out)
}

/// A `lastmod` in the W3C datetime forms the protocol allows: a year, a
/// year and month, a date, or a moment with its offset. A date without a
/// time is its first moment in UTC.
pub fn parse_lastmod(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
    if let Ok(moment) = DateTime::parse_from_rfc3339(text) {
        return Some(moment.with_timezone(&Utc));
    }
    // A moment written without seconds: `2026-10-05T08:30+00:00`.
    if let Ok(moment) = DateTime::parse_from_str(text, "%Y-%m-%dT%H:%M%:z") {
        return Some(moment.with_timezone(&Utc));
    }
    let date = match text.len() {
        4 => NaiveDate::parse_from_str(&format!("{text}-01-01"), "%Y-%m-%d").ok(),
        7 => NaiveDate::parse_from_str(&format!("{text}-01"), "%Y-%m-%d").ok(),
        10 => NaiveDate::parse_from_str(text, "%Y-%m-%d").ok(),
        _ => None,
    }?;
    Some(date.and_hms_opt(0, 0, 0)?.and_utc())
}

/// An absolute `http` or `https` address with a host.
fn address(text: &str) -> Option<Url> {
    Url::parse(text.trim())
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

/// The name of an element without its namespace prefix, in lower case.
fn local(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_ascii_lowercase()
}

struct Collected {
    entries: Vec<Entry>,
    invalid: usize,
    truncated: bool,
}

impl Collected {
    fn push(&mut self, loc: Option<&str>, lastmod: Option<&str>, limits: Limits) {
        let Some(url) = loc.and_then(address) else {
            self.invalid += 1;
            return;
        };
        if self.entries.len() >= limits.max_entries {
            self.truncated = true;
            return;
        }
        self.entries.push(Entry {
            url,
            lastmod: lastmod.and_then(parse_lastmod),
        });
    }
}

/// What the root element makes of the document.
fn kind_of(root: &str) -> Result<Kind, SitemapError> {
    match root {
        "urlset" => Ok(Kind::Pages),
        "sitemapindex" => Ok(Kind::Index),
        _ => Err(SitemapError::NotASitemap(root.to_owned())),
    }
}

/// The field of an entry a child element is, when it is one of the two a
/// crawl reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Loc,
    Lastmod,
}

/// One entry while it is read.
#[derive(Default)]
struct Draft {
    loc: Option<String>,
    lastmod: Option<String>,
}

impl Draft {
    fn slot(&mut self, field: Field) -> &mut String {
        match field {
            Field::Loc => self.loc.get_or_insert_with(String::new),
            Field::Lastmod => self.lastmod.get_or_insert_with(String::new),
        }
    }
}

fn parse_xml(text: &str, limits: Limits) -> Result<(Kind, Collected), SitemapError> {
    let mut reader = Reader::from_str(text);
    let mut kind = None;
    let mut collected = Collected {
        entries: Vec::new(),
        invalid: 0,
        truncated: false,
    };
    // The root is at depth 1, an entry at 2, its fields at 3. What sits
    // deeper belongs to an extension (an image, a video, a translation) and
    // is no field of the entry, whatever it is called.
    let mut depth = 0_usize;
    let mut entry: Option<Draft> = None;
    let mut field: Option<Field> = None;
    loop {
        let event = reader
            .read_event()
            .map_err(|error| SitemapError::Xml(error.to_string()))?;
        match event {
            Event::Start(start) => {
                depth += 1;
                let name = local(start.name().as_ref());
                match (depth, kind) {
                    (1, _) => kind = Some(kind_of(&name)?),
                    (2, Some(Kind::Pages)) if name == "url" => entry = Some(Draft::default()),
                    (2, Some(Kind::Index)) if name == "sitemap" => entry = Some(Draft::default()),
                    (3, _) if entry.is_some() => {
                        field = match name.as_str() {
                            "loc" => Some(Field::Loc),
                            "lastmod" => Some(Field::Lastmod),
                            _ => None,
                        };
                    }
                    _ => {}
                }
            }
            Event::Empty(empty) if depth == 0 => {
                kind = Some(kind_of(&local(empty.name().as_ref()))?);
            }
            Event::Text(piece) if depth == 3 => {
                if let (Some(field), Some(draft)) = (field, entry.as_mut()) {
                    draft.slot(field).push_str(&piece.xml10_content());
                }
            }
            Event::CData(piece) if depth == 3 => {
                if let (Some(field), Some(draft)) = (field, entry.as_mut()) {
                    draft.slot(field).push_str(piece.as_ref());
                }
            }
            Event::GeneralRef(reference) if depth == 3 => {
                // `&amp;` in an address, the other four the language itself
                // knows, and characters by number. A reference to anything
                // else is not expanded: it leaves a space, and the address
                // it stands in is no address.
                if let (Some(field), Some(draft)) = (field, entry.as_mut()) {
                    let name: &str = reference.as_ref();
                    let resolved = match name {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" => Some('\''),
                        _ => reference
                            .resolve_char_ref()
                            .map_err(|error| SitemapError::Xml(error.to_string()))?,
                    };
                    draft.slot(field).push(resolved.unwrap_or(' '));
                }
            }
            Event::End(_) => {
                if depth == 3 {
                    field = None;
                } else if depth == 2
                    && let Some(draft) = entry.take()
                {
                    collected.push(draft.loc.as_deref(), draft.lastmod.as_deref(), limits);
                    if collected.truncated {
                        break;
                    }
                }
                depth = depth.saturating_sub(1);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let Some(kind) = kind else {
        return Err(SitemapError::Empty);
    };
    Ok((kind, collected))
}

fn parse_text(text: &str, limits: Limits) -> Collected {
    let mut collected = Collected {
        entries: Vec::new(),
        invalid: 0,
        truncated: false,
    };
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        collected.push(Some(line), None, limits);
        if collected.truncated {
            break;
        }
    }
    collected
}

/// Reads a sitemap from the body it was fetched as.
///
/// # Errors
///
/// [`SitemapError`] when the body is too large, cannot be unpacked, or is
/// no sitemap. A sitemap whose entries are all unusable is no error: it is
/// a sitemap without entries.
pub fn parse(body: &[u8], limits: Limits) -> Result<Sitemap, SitemapError> {
    let gzipped = body.starts_with(&GZIP);
    let unpacked;
    let bytes = if gzipped {
        unpacked = unpack(body, limits.max_bytes)?;
        &unpacked
    } else {
        if body.len() > limits.max_bytes {
            return Err(SitemapError::TooLarge {
                limit: limits.max_bytes,
            });
        }
        body
    };
    let text = String::from_utf8_lossy(bytes);
    // A byte order mark is no part of the text.
    let text = text.trim_start_matches('\u{feff}').trim_start();
    if text.is_empty() {
        return Err(SitemapError::Empty);
    }
    let (kind, collected) = if text.starts_with('<') {
        parse_xml(text, limits)?
    } else {
        (Kind::Pages, parse_text(text, limits))
    };
    Ok(Sitemap {
        kind,
        entries: collected.entries,
        invalid: collected.invalid,
        truncated: collected.truncated,
        gzipped,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use chrono::{TimeZone, Utc};
    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::{Kind, Limits, SitemapError, parse, parse_lastmod};

    fn gzip(text: &str) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(text.as_bytes()).unwrap();
        encoder.finish().unwrap()
    }

    const PAGES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"
        xmlns:image="http://www.google.com/schemas/sitemap-image/1.1">
  <url>
    <loc>https://example.org/</loc>
    <lastmod>2026-10-01</lastmod>
    <changefreq>weekly</changefreq>
    <priority>0.8</priority>
  </url>
  <url>
    <loc>
      https://example.org/list?page=2&amp;sort=new
    </loc>
    <lastmod>2026-10-05T08:30:00+02:00</lastmod>
    <image:image><image:loc>https://example.org/picture.png</image:loc></image:image>
  </url>
  <url><loc><![CDATA[https://example.org/a?b=1&c=2]]></loc></url>
  <url><loc>/relative</loc></url>
  <url><loc>ftp://example.org/file</loc></url>
  <url><lastmod>2026-01-01</lastmod></url>
  <url><loc>https://example.org/last</loc><lastmod>not a date</lastmod></url>
</urlset>
"#;

    #[test]
    fn a_urlset_is_read_with_its_dates_and_without_what_is_no_address() {
        let sitemap = parse(PAGES.as_bytes(), Limits::default()).unwrap();
        assert_eq!(sitemap.kind, Kind::Pages);
        assert!(!sitemap.gzipped && !sitemap.truncated);
        let read: Vec<(&str, Option<String>)> = sitemap
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.url.as_str(),
                    entry.lastmod.map(|moment| moment.to_rfc3339()),
                )
            })
            .collect();
        assert_eq!(
            read,
            [
                (
                    "https://example.org/",
                    Some("2026-10-01T00:00:00+00:00".to_owned())
                ),
                // The entity is the ampersand it stands for, and the moment
                // is told in UTC. The image inside the entry is no page.
                (
                    "https://example.org/list?page=2&sort=new",
                    Some("2026-10-05T06:30:00+00:00".to_owned())
                ),
                ("https://example.org/a?b=1&c=2", None),
                ("https://example.org/last", None),
            ]
        );
        // A relative address, another scheme, an entry without an address.
        assert_eq!(sitemap.invalid, 3);
    }

    #[test]
    fn an_index_names_further_sitemaps() {
        let index = r"<sitemapindex xmlns='http://www.sitemaps.org/schemas/sitemap/0.9'>
            <sitemap><loc>https://example.org/sitemaps/pages.xml</loc><lastmod>2026-10</lastmod></sitemap>
            <sitemap><loc>https://example.org/sitemaps/more.xml.gz</loc></sitemap>
            <url><loc>https://example.org/not-an-entry-of-an-index</loc></url>
        </sitemapindex>";
        let sitemap = parse(index.as_bytes(), Limits::default()).unwrap();
        assert_eq!(sitemap.kind, Kind::Index);
        assert_eq!(
            sitemap
                .entries
                .iter()
                .map(|entry| entry.url.path())
                .collect::<Vec<_>>(),
            ["/sitemaps/pages.xml", "/sitemaps/more.xml.gz"]
        );
        assert_eq!(
            sitemap.entries[0].lastmod,
            Some(Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap())
        );
        assert_eq!(sitemap.invalid, 0);
    }

    #[test]
    fn a_gzipped_file_is_known_by_its_first_bytes_and_unpacked() {
        let sitemap = parse(&gzip(PAGES), Limits::default()).unwrap();
        assert!(sitemap.gzipped);
        assert_eq!(sitemap.entries.len(), 4);
        // Damaged: the magic number and then nothing of a gzip stream.
        let damaged = parse(&[0x1f, 0x8b, 0x08, 0x00, 0xff, 0xff], Limits::default());
        assert!(matches!(damaged, Err(SitemapError::Gzip(_))), "{damaged:?}");
    }

    #[test]
    fn nothing_larger_than_the_limit_is_read_or_unpacked() {
        let limits = Limits {
            max_bytes: 200,
            max_entries: 10,
        };
        assert_eq!(
            parse(PAGES.as_bytes(), limits),
            Err(SitemapError::TooLarge { limit: 200 })
        );
        // A small file that unpacks to a large one is stopped at the limit.
        let bomb = gzip(&format!(
            "<urlset>{}</urlset>",
            "<url><loc>https://example.org/a</loc></url>".repeat(10_000)
        ));
        assert!(bomb.len() < 4000, "{} bytes", bomb.len());
        let limits = Limits {
            max_bytes: 100_000,
            max_entries: 10,
        };
        assert_eq!(
            parse(&bomb, limits),
            Err(SitemapError::TooLarge { limit: 100_000 })
        );
        // Exactly at the limit is inside it.
        let exact = "<urlset><url><loc>https://example.org/a</loc></url></urlset>";
        let limits = Limits {
            max_bytes: exact.len(),
            max_entries: 10,
        };
        assert_eq!(parse(exact.as_bytes(), limits).unwrap().entries.len(), 1);
        assert_eq!(
            parse(&gzip(exact), limits).unwrap().entries.len(),
            1,
            "the limit is on what it unpacks to"
        );
    }

    #[test]
    fn no_more_entries_are_read_than_the_limit_says() {
        let many = format!(
            "<urlset>{}</urlset>",
            (0..7)
                .map(|index| format!("<url><loc>https://example.org/p{index}</loc></url>"))
                .collect::<Vec<_>>()
                .concat()
        );
        let limits = Limits {
            max_bytes: 10_000,
            max_entries: 5,
        };
        let sitemap = parse(many.as_bytes(), limits).unwrap();
        assert_eq!(sitemap.entries.len(), 5);
        assert!(sitemap.truncated);
        assert_eq!(sitemap.entries[4].url.path(), "/p4");
        let limits = Limits {
            max_bytes: 10_000,
            max_entries: 7,
        };
        let all = parse(many.as_bytes(), limits).unwrap();
        assert_eq!(all.entries.len(), 7);
        assert!(!all.truncated);
    }

    #[test]
    fn the_plain_text_form_is_one_address_a_line() {
        let text = "\u{feff}https://example.org/a\r\n\r\n  https://example.org/b  \nnot an address\nhttps://example.org/c";
        let sitemap = parse(text.as_bytes(), Limits::default()).unwrap();
        assert_eq!(sitemap.kind, Kind::Pages);
        assert_eq!(
            sitemap
                .entries
                .iter()
                .map(|entry| entry.url.path())
                .collect::<Vec<_>>(),
            ["/a", "/b", "/c"]
        );
        assert_eq!(sitemap.invalid, 1);
        assert!(sitemap.entries.iter().all(|entry| entry.lastmod.is_none()));
        let limits = Limits {
            max_bytes: 10_000,
            max_entries: 2,
        };
        assert!(parse(text.as_bytes(), limits).unwrap().truncated);
    }

    #[test]
    fn what_is_no_sitemap_says_what_it_is() {
        let limits = Limits::default();
        assert_eq!(parse(b"", limits), Err(SitemapError::Empty));
        assert_eq!(parse(b" \n\t", limits), Err(SitemapError::Empty));
        assert_eq!(
            parse(b"<html><body>Not found</body></html>", limits),
            Err(SitemapError::NotASitemap("html".to_owned()))
        );
        assert_eq!(
            parse(b"<rss version='2.0'/>", limits),
            Err(SitemapError::NotASitemap("rss".to_owned()))
        );
        assert_eq!(
            parse(b"<?xml version='1.0'?><!-- nothing -->", limits),
            Err(SitemapError::Empty)
        );
        let broken = parse(
            b"<urlset><url><loc>https://example.org/a</url></urlset>",
            limits,
        );
        assert!(matches!(broken, Err(SitemapError::Xml(_))), "{broken:?}");
        // An empty sitemap is a sitemap.
        let empty = parse(b"<urlset/>", limits).unwrap();
        assert_eq!((empty.kind, empty.entries.len()), (Kind::Pages, 0));
        let empty = parse(b"<sitemapindex></sitemapindex>", limits).unwrap();
        assert_eq!((empty.kind, empty.entries.len()), (Kind::Index, 0));
    }

    #[test]
    fn nothing_of_a_document_type_is_expanded() {
        // An entity of the document's own is not expanded: the address it
        // stands in is no address, and nothing is read from a file.
        let hostile = r#"<?xml version="1.0"?>
<!DOCTYPE urlset [ <!ENTITY secret SYSTEM "file:///etc/passwd"> <!ENTITY big "aaaaaaaaaa"> ]>
<urlset>
  <url><loc>https://example.org/&secret;</loc></url>
  <url><loc>&big;</loc></url>
  <url><loc>https://example.org/ok&#x3f;a=1&#38;b=2</loc></url>
</urlset>"#;
        let sitemap = parse(hostile.as_bytes(), Limits::default()).unwrap();
        let read: Vec<&str> = sitemap
            .entries
            .iter()
            .map(|entry| entry.url.as_str())
            .collect();
        assert!(read.contains(&"https://example.org/ok?a=1&b=2"), "{read:?}");
        assert!(
            read.iter()
                .all(|url| !url.contains("passwd") && !url.contains("aaaa")),
            "{read:?}"
        );
    }

    #[test]
    fn lastmod_is_read_in_every_form_of_the_protocol() {
        let at = |y, mo, d, h, mi, s| Some(Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap());
        assert_eq!(parse_lastmod("2026"), at(2026, 1, 1, 0, 0, 0));
        assert_eq!(parse_lastmod("2026-10"), at(2026, 10, 1, 0, 0, 0));
        assert_eq!(parse_lastmod(" 2026-10-05 "), at(2026, 10, 5, 0, 0, 0));
        assert_eq!(
            parse_lastmod("2026-10-05T08:30+00:00"),
            at(2026, 10, 5, 8, 30, 0)
        );
        assert_eq!(
            parse_lastmod("2026-10-05T08:30:45Z"),
            at(2026, 10, 5, 8, 30, 45)
        );
        assert_eq!(
            parse_lastmod("2026-10-05T08:30:45.25-05:00"),
            Some(
                Utc.with_ymd_and_hms(2026, 10, 5, 13, 30, 45).unwrap()
                    + chrono::Duration::milliseconds(250)
            )
        );
        for odd in [
            "",
            "yesterday",
            "2026-13",
            "2026-02-30",
            "05.10.2026",
            "20261005",
            "2026-1",
        ] {
            assert_eq!(parse_lastmod(odd), None, "{odd}");
        }
    }
}
