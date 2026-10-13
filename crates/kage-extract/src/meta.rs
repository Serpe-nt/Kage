//! What a page says about itself (spec §14): its title, description,
//! canonical address and language, its `<meta>` elements, its OpenGraph
//! and Twitter card properties, its JSON-LD, and when it was published and
//! changed.
//!
//! Everything here is read as the page wrote it. Nothing is guessed: a
//! page without a `<title>` has no title, a time without a zone is given
//! without one, a JSON-LD block that is no JSON is counted and left out.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use kage_crawl::html::{Document, text_of};
use scraper::ElementRef;
use serde_json::Value;

/// The most `<meta>` elements read from one page.
pub const MAX_META: usize = 1000;

/// The longest value kept of one `<meta>`, in bytes; a longer one is cut
/// at a character.
pub const MAX_META_VALUE: usize = 8 * 1024;

/// The most JSON-LD values kept from one page.
pub const MAX_JSONLD: usize = 64;

/// The longest JSON-LD block that is parsed, in bytes.
pub const MAX_JSONLD_BYTES: usize = 1024 * 1024;

/// The namespaces of the OpenGraph protocol and the one shops add to it.
const OPENGRAPH: [&str; 7] = [
    "og:", "article:", "book:", "profile:", "video:", "music:", "product:",
];

/// `<meta>` names that say when a page was published, in the order they
/// are trusted.
const PUBLISHED: [&str; 12] = [
    "article:published_time",
    "og:article:published_time",
    "datepublished",
    "date",
    "dc.date.issued",
    "dcterms.issued",
    "dc.date",
    "dcterms.created",
    "citation_publication_date",
    "parsely-pub-date",
    "sailthru.date",
    "pubdate",
];

/// `<meta>` names that say when a page was last changed.
const MODIFIED: [&str; 7] = [
    "article:modified_time",
    "og:article:modified_time",
    "og:updated_time",
    "datemodified",
    "dc.date.modified",
    "dcterms.modified",
    "last-modified",
];

/// A map from a name to every value it has on the page, in document order.
pub type Named = BTreeMap<String, Vec<String>>;

/// What a page says about itself.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Meta {
    /// The `<title>`, white space collapsed.
    pub title: Option<String>,
    /// `<meta name="description">`.
    pub description: Option<String>,
    /// `<link rel="canonical">`, as an absolute address.
    pub canonical: Option<String>,
    /// The `lang` of the document, or its `content-language`.
    pub lang: Option<String>,
    /// When the page was published.
    pub published_at: Option<String>,
    /// When the page was last changed.
    pub modified_at: Option<String>,
    /// Every `<meta>` that is neither OpenGraph nor a Twitter card, by its
    /// name in lower case.
    pub meta: Named,
    /// OpenGraph properties (`og:`, `article:`, `product:` and the other
    /// namespaces of the protocol), by their full name.
    pub opengraph: Named,
    /// Twitter card properties, by their full name.
    pub twitter: Named,
    /// Every JSON-LD value of the page; an array at the top of a block is
    /// its members.
    pub jsonld: Vec<Value>,
    /// JSON-LD blocks that were left out: no JSON, too long, or beyond
    /// [`MAX_JSONLD`].
    pub jsonld_skipped: u32,
}

fn squeeze(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A value of at most [`MAX_META_VALUE`] bytes, cut at a character.
fn bounded(value: &str) -> String {
    let value = value.trim();
    if value.len() <= MAX_META_VALUE {
        return value.to_owned();
    }
    let mut end = MAX_META_VALUE;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// A time as a page wrote it, in one spelling: with a zone, in UTC as
/// RFC 3339; without one, as written (`2026-03-12T06:30:00`); a date
/// alone, as a date. `None` for what is no time.
pub fn normalize_time(written: &str) -> Option<String> {
    let written = written.trim();
    if written.is_empty() || written.len() > 64 {
        return None;
    }
    let spaced = written.replacen(' ', "T", 1);
    let zoned = DateTime::parse_from_rfc3339(&spaced)
        .or_else(|_| DateTime::parse_from_str(&spaced, "%Y-%m-%dT%H:%M:%S%.f%z"))
        .or_else(|_| DateTime::parse_from_str(&spaced, "%Y-%m-%dT%H:%M%z"))
        .or_else(|_| DateTime::parse_from_str(&spaced, "%Y-%m-%dT%H:%M%:z"));
    if let Ok(time) = zoned {
        return Some(
            time.with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::AutoSi, true),
        );
    }
    let naive = NaiveDateTime::parse_from_str(&spaced, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(&spaced, "%Y-%m-%dT%H:%M"));
    if let Ok(time) = naive {
        return Some(time.format("%Y-%m-%dT%H:%M:%S%.f").to_string());
    }
    NaiveDate::parse_from_str(written, "%Y-%m-%d")
        .ok()
        .map(|date| date.format("%Y-%m-%d").to_string())
}

/// The first time under `key` that a JSON-LD value of the page says of
/// itself: the values in document order, and after each the members of its
/// `@graph`. What a value holds deeper is not the page's: the date of a
/// review on a product's page, of a comment under an article, of the site
/// an article is part of.
fn jsonld_time(values: &[Value], key: &str) -> Option<String> {
    let own = |value: &Value| {
        value
            .get(key)
            .and_then(Value::as_str)
            .and_then(normalize_time)
    };
    values.iter().find_map(|value| {
        own(value).or_else(|| {
            value
                .get("@graph")
                .and_then(Value::as_array)
                .and_then(|members| members.iter().find_map(own))
        })
    })
}

/// The time of the first `<time>` element that says it is the page's
/// date of this kind, by its `itemprop` or its class.
fn time_element(document: &Document, itemprop: &str, classes: &[&str]) -> Option<String> {
    document
        .tree()
        .root_element()
        .descendent_elements()
        .filter(|element| element.value().name() == "time")
        .filter(|element| {
            let value = element.value();
            value
                .attr("itemprop")
                .is_some_and(|property| property.eq_ignore_ascii_case(itemprop))
                || value.attr("class").is_some_and(|class| {
                    class.split_ascii_whitespace().any(|name| {
                        classes
                            .iter()
                            .any(|wanted| name.eq_ignore_ascii_case(wanted))
                    })
                })
                || (itemprop == "datePublished" && value.attr("pubdate").is_some())
        })
        .find_map(|element| element.value().attr("datetime").and_then(normalize_time))
}

fn first_time(maps: &[&Named], names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        maps.iter().find_map(|map| {
            map.get(*name)
                .and_then(|values| values.iter().find_map(|value| normalize_time(value)))
        })
    })
}

fn rel_has(element: ElementRef<'_>, wanted: &str) -> bool {
    element.value().attr("rel").is_some_and(|rel| {
        rel.split_ascii_whitespace()
            .any(|token| token.eq_ignore_ascii_case(wanted))
    })
}

/// Reads what a page says about itself.
#[allow(clippy::too_many_lines)] // One walk through the document, one arm an element.
pub fn read(document: &Document) -> Meta {
    let mut meta = Meta::default();
    let mut seen = 0_usize;
    for element in document.tree().root_element().descendent_elements() {
        let value = element.value();
        match value.name() {
            "title" if meta.title.is_none() => {
                // The document's title: not the `<title>` of a drawing.
                let in_svg = element
                    .ancestors()
                    .filter_map(ElementRef::wrap)
                    .any(|ancestor| ancestor.value().name() == "svg");
                if !in_svg {
                    meta.title = Some(text_of(element)).filter(|title| !title.is_empty());
                }
            }
            "html" if meta.lang.is_none() => {
                meta.lang = value
                    .attr("lang")
                    .map(str::trim)
                    .filter(|lang| !lang.is_empty())
                    .map(str::to_owned);
            }
            "link" if meta.canonical.is_none() && rel_has(element, "canonical") => {
                meta.canonical = value
                    .attr("href")
                    .and_then(|href| document.base().join(href.trim()).ok())
                    .filter(|url| matches!(url.scheme(), "http" | "https"))
                    .map(|url| url.to_string());
            }
            "meta" => {
                let Some(content) = value.attr("content") else {
                    continue;
                };
                let name = ["property", "name", "itemprop", "http-equiv"]
                    .into_iter()
                    .find_map(|attribute| value.attr(attribute))
                    .map(|name| name.trim().to_ascii_lowercase())
                    .filter(|name| !name.is_empty() && name.len() <= 256);
                let Some(name) = name else {
                    continue;
                };
                seen += 1;
                if seen > MAX_META {
                    continue;
                }
                let into = if OPENGRAPH.iter().any(|prefix| name.starts_with(prefix)) {
                    &mut meta.opengraph
                } else if name.starts_with("twitter:") {
                    &mut meta.twitter
                } else {
                    &mut meta.meta
                };
                into.entry(name).or_default().push(bounded(content));
            }
            "script"
                if value.attr("type").is_some_and(|kind| {
                    kind.split(';')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .eq_ignore_ascii_case("application/ld+json")
                }) =>
            {
                let block: String = element.text().collect();
                let parsed = (block.len() <= MAX_JSONLD_BYTES)
                    .then(|| serde_json::from_str::<Value>(block.trim()).ok())
                    .flatten();
                let values = match parsed {
                    Some(Value::Array(items)) => items,
                    Some(value) => vec![value],
                    None => {
                        meta.jsonld_skipped = meta.jsonld_skipped.saturating_add(1);
                        continue;
                    }
                };
                for value in values {
                    if meta.jsonld.len() < MAX_JSONLD {
                        meta.jsonld.push(value);
                    } else {
                        meta.jsonld_skipped = meta.jsonld_skipped.saturating_add(1);
                    }
                }
            }
            _ => {}
        }
    }
    let first = |map: &Named, name: &str| {
        map.get(name)
            .and_then(|values| values.iter().find(|value| !value.is_empty()))
            .map(|value| squeeze(value))
    };
    meta.description = first(&meta.meta, "description");
    if meta.lang.is_none() {
        meta.lang = first(&meta.meta, "content-language");
    }
    let named = [&meta.opengraph, &meta.meta];
    meta.published_at = jsonld_time(&meta.jsonld, "datePublished")
        .or_else(|| first_time(&named, &PUBLISHED))
        .or_else(|| time_element(document, "datePublished", &["published", "entry-date"]));
    meta.modified_at = jsonld_time(&meta.jsonld, "dateModified")
        .or_else(|| first_time(&named, &MODIFIED))
        .or_else(|| time_element(document, "dateModified", &["updated", "modified"]));
    meta
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use kage_crawl::html::Document;
    use serde_json::json;
    use url::Url;

    use super::{MAX_JSONLD, MAX_META, MAX_META_VALUE, Meta, normalize_time, read};

    fn meta(html: &str) -> Meta {
        read(&Document::parse(
            html,
            &Url::parse("https://news.example/2026/story?ref=home").unwrap(),
        ))
    }

    #[test]
    fn a_page_says_its_title_description_language_and_canonical_address() {
        let read = meta(
            r#"<!doctype html><html lang=" en-GB "><head>
            <title>  A   headline
               – Site </title>
            <meta name="Description" content="  What the page   is about. ">
            <link rel="alternate canonical" href="/2026/story">
            <link rel="canonical" href="https://other.example/second">
            </head><body><svg><title>a drawing</title></svg><p>x</p></body></html>"#,
        );
        assert_eq!(read.title.as_deref(), Some("A headline – Site"));
        assert_eq!(read.description.as_deref(), Some("What the page is about."));
        assert_eq!(read.lang.as_deref(), Some("en-GB"));
        // The first canonical link, resolved against the page.
        assert_eq!(
            read.canonical.as_deref(),
            Some("https://news.example/2026/story")
        );

        // A page that says none of it has none of it.
        let bare = meta("<p>just text</p>");
        assert_eq!(bare, Meta::default());
        // The title of a drawing is not the page's, an empty title is none,
        // and a canonical that is no web address is none.
        let odd = meta(
            r#"<html lang=""><body><svg><title>drawing</title></svg><title> </title>
            <link rel="canonical" href="javascript:void(0)"></body></html>"#,
        );
        assert_eq!((odd.title, odd.lang, odd.canonical), (None, None, None));
        // The language of a page that says it only in a header-like meta.
        let german = meta(r#"<meta http-equiv="Content-Language" content="de">"#);
        assert_eq!(german.lang.as_deref(), Some("de"));
        // A base element moves what the canonical link resolves against.
        let based =
            meta(r#"<base href="https://cdn.example/dir/"><link rel="canonical" href="page">"#);
        assert_eq!(
            based.canonical.as_deref(),
            Some("https://cdn.example/dir/page")
        );
    }

    #[test]
    fn meta_elements_are_sorted_into_plain_opengraph_and_twitter() {
        let read = meta(
            r#"<head>
            <meta charset="utf-8">
            <meta name="keywords" content="a, b">
            <meta name="KEYWORDS" content="c">
            <meta property="og:title" content="A headline">
            <meta property="og:image" content="https://img.example/1.jpg">
            <meta property="og:image" content="https://img.example/2.jpg">
            <meta property="article:tag" content="rail">
            <meta property="product:price:amount" content="129.00">
            <meta name="twitter:card" content="summary">
            <meta property="twitter:site" content="@site">
            <meta property="fb:app_id" content="1">
            <meta itemprop="datePublished" content="2026-03-12">
            <meta http-equiv="refresh" content="30">
            <meta name="" content="nameless">
            <meta name="empty">
            </head>"#,
        );
        assert_eq!(read.meta["keywords"], ["a, b", "c"]);
        assert_eq!(read.meta["fb:app_id"], ["1"]);
        assert_eq!(read.meta["datepublished"], ["2026-03-12"]);
        assert_eq!(read.meta["refresh"], ["30"]);
        assert_eq!(read.meta.len(), 4);
        assert_eq!(read.opengraph["og:title"], ["A headline"]);
        assert_eq!(
            read.opengraph["og:image"],
            ["https://img.example/1.jpg", "https://img.example/2.jpg"]
        );
        assert_eq!(read.opengraph["article:tag"], ["rail"]);
        assert_eq!(read.opengraph["product:price:amount"], ["129.00"]);
        assert_eq!(read.opengraph.len(), 4);
        assert_eq!(read.twitter["twitter:card"], ["summary"]);
        assert_eq!(read.twitter["twitter:site"], ["@site"]);
        assert_eq!(read.twitter.len(), 2);
    }

    #[test]
    fn a_page_with_too_much_meta_is_cut() {
        let mut many = String::new();
        for n in 0..MAX_META + 5 {
            write!(many, r#"<meta name="m{n}" content="v">"#).unwrap();
        }
        assert_eq!(meta(&many).meta.len(), MAX_META);
        // A value is cut at a character, never inside one.
        let long = format!(
            r#"<meta name="long" content="{}é">"#,
            "x".repeat(MAX_META_VALUE - 1)
        );
        let read = meta(&long);
        assert_eq!(read.meta["long"][0].len(), MAX_META_VALUE - 1);
        let exact = format!(
            r#"<meta name="long" content="{}">"#,
            "x".repeat(MAX_META_VALUE)
        );
        assert_eq!(meta(&exact).meta["long"][0].len(), MAX_META_VALUE);
    }

    #[test]
    fn jsonld_is_every_block_that_parses() {
        let read = meta(
            r#"<head>
            <script type="application/ld+json">{"@type":"NewsArticle","headline":"A"}</script>
            <script type="Application/LD+JSON; charset=utf-8">
              [{"@type":"Recipe"},{"@type":"Article"}]
            </script>
            <script type="application/ld+json">{'@type': 'Broken',}</script>
            <script type="application/ld+json"></script>
            <script type="application/json">{"not":"linked data"}</script>
            <script>var x = {"@type":"Nothing"};</script>
            </head><body><script type="application/ld+json">{"@graph":[{"@type":"Product"}]}</script></body>"#,
        );
        assert_eq!(
            read.jsonld,
            [
                json!({"@type":"NewsArticle","headline":"A"}),
                json!({"@type":"Recipe"}),
                json!({"@type":"Article"}),
                json!({"@graph":[{"@type":"Product"}]}),
            ]
        );
        assert_eq!(read.jsonld_skipped, 2);

        let many = format!(
            r#"<script type="application/ld+json">[{}]</script>"#,
            vec!["{}"; MAX_JSONLD + 3].join(",")
        );
        let read = meta(&many);
        assert_eq!(read.jsonld.len(), MAX_JSONLD);
        assert_eq!(read.jsonld_skipped, 3);
    }

    #[test]
    fn times_are_given_in_one_spelling_and_no_zone_is_invented() {
        for (written, normalized) in [
            ("2026-03-12T06:30:00+00:00", "2026-03-12T06:30:00Z"),
            ("2026-03-12T07:30:00+01:00", "2026-03-12T06:30:00Z"),
            ("2026-03-12T06:30:00Z", "2026-03-12T06:30:00Z"),
            ("2026-03-12T06:30:00.250Z", "2026-03-12T06:30:00.250Z"),
            ("2026-03-12T08:30:00+0200", "2026-03-12T06:30:00Z"),
            ("2026-03-12 06:30:00+00:00", "2026-03-12T06:30:00Z"),
            ("2026-03-12T06:30+00:00", "2026-03-12T06:30:00Z"),
            ("2026-03-12T06:30:00", "2026-03-12T06:30:00"),
            ("2026-03-12T06:30", "2026-03-12T06:30:00"),
            ("2026-03-12 06:30:00", "2026-03-12T06:30:00"),
            ("  2026-03-12 ", "2026-03-12"),
        ] {
            assert_eq!(
                normalize_time(written).as_deref(),
                Some(normalized),
                "{written}"
            );
        }
        for nothing in [
            "",
            "12 March 2026",
            "2026-13-40",
            "yesterday",
            "2026-03-12T25:00:00Z",
            &"2".repeat(65),
        ] {
            assert_eq!(normalize_time(nothing), None, "{nothing}");
        }
    }

    #[test]
    fn when_a_page_was_published_comes_from_where_it_says_so_best() {
        // JSON-LD before meta before a time element.
        let all = meta(
            r#"<head>
            <meta property="article:published_time" content="2026-03-02T00:00:00Z">
            <meta property="article:modified_time" content="2026-03-03T00:00:00Z">
            <script type="application/ld+json">{"@graph":[{"@type":"WebSite"},
              {"@type":"Article","datePublished":"2026-03-01T10:00:00+01:00","dateModified":"not a time"}]}</script>
            </head><body><time class="published" datetime="2026-03-04">4 March</time></body>"#,
        );
        assert_eq!(all.published_at.as_deref(), Some("2026-03-01T09:00:00Z"));
        // What JSON-LD says badly is taken from the next source.
        assert_eq!(all.modified_at.as_deref(), Some("2026-03-03T00:00:00Z"));

        let by_meta = meta(
            r#"<meta name="DC.date.issued" content="2026-02-01">
            <meta property="og:updated_time" content="2026-02-02T12:00:00+00:00">"#,
        );
        assert_eq!(by_meta.published_at.as_deref(), Some("2026-02-01"));
        assert_eq!(by_meta.modified_at.as_deref(), Some("2026-02-02T12:00:00Z"));

        let by_time = meta(
            r#"<p>Posted on <time class="entry-date Published" datetime="2016-02-14T09:12:00+00:00">14 February</time>
            <time class="updated" datetime="2016-02-20T18:40:00+00:00">20 February</time>
            <time datetime="2016-01-01">a date in the text</time></p>"#,
        );
        assert_eq!(
            by_time.published_at.as_deref(),
            Some("2016-02-14T09:12:00Z")
        );
        assert_eq!(by_time.modified_at.as_deref(), Some("2016-02-20T18:40:00Z"));

        let by_property = meta(
            r#"<time itemprop="datePublished" datetime="2020-05-05">x</time>
            <time itemprop="dateModified" datetime="2020-05-06">y</time>"#,
        );
        assert_eq!(by_property.published_at.as_deref(), Some("2020-05-05"));
        assert_eq!(by_property.modified_at.as_deref(), Some("2020-05-06"));
        // What a block holds deeper is somebody else's date: the review on
        // a product's page says when the review was written.
        let product = meta(
            r#"<script type="application/ld+json">{"@graph":[{"@type":"Product","name":"A jacket",
              "review":[{"@type":"Review","datePublished":"2026-09-14T06:41:07Z"}],
              "isPartOf":{"@type":"WebSite","dateModified":"2020-01-01"}}]}</script>"#,
        );
        assert_eq!((product.published_at, product.modified_at), (None, None));
        let by_attribute = meta(r#"<time pubdate datetime="2011-11-11">x</time>"#);
        assert_eq!(by_attribute.published_at.as_deref(), Some("2011-11-11"));
        assert_eq!(by_attribute.modified_at, None);

        // A time that says nothing about what it is, is not the page's.
        let unmarked = meta(r#"<p>Last updated on <time datetime="2026-09-03">Sep 3</time></p>"#);
        assert_eq!((unmarked.published_at, unmarked.modified_at), (None, None));
    }
}
