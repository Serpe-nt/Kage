//! What a page says, read from the document a visit captured (spec §14):
//! its metadata, its structured data, its links, its main content as text
//! and as markdown, its headings, and whether it gave a reader anything at
//! all; and what a site's `robots.txt` says about an address.
//!
//! Nothing here fetches anything. [`extract`] works on the one tree the
//! page reader of `kage-crawl` parsed ([`Document`]), with the same rules
//! for what a reader sees, so a crawl and an extraction never disagree
//! about a page, and a page is parsed once.
//!
//! - [`extract`]: everything `include: ["extract"]` returns.
//! - [`main_content`]: the main content alone.
//! - [`meta`]: what a page says about itself.
//! - [`robots`]: `robots.txt`, parsed and asked about an address.

#![forbid(unsafe_code)]

mod content;
mod dom;
pub mod meta;
mod render;
pub mod robots;

pub use kage_crawl::html::Document;
use kage_crawl::site;
use serde::{Deserialize, Serialize};

use crate::meta::Named;
use crate::render::Mode;

/// Name of this crate as recorded in its manifest.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// A page whose main content has fewer words than this gave its reader
/// next to nothing, unless the configuration says another number: about
/// the 500 characters under which Readability distrusts its own result.
pub const DEFAULT_THIN_WORDS: usize = 80;

/// A heading of the main content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heading {
    /// 1 for `h1` to 6 for `h6`.
    pub level: u8,
    /// Its text.
    pub text: String,
    /// Its `id`, when it has one: the fragment that leads to it.
    pub id: Option<String>,
}

/// Why what was extracted is not the whole document (F122, F147).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Truncated {
    /// The body was cut at `max_body_bytes` (spec §9): the document is
    /// its first part.
    MaxBodyBytes,
    /// The parse stopped at its time (F147): the document is what was
    /// parsed until then.
    ParseTime,
}

/// A link of a page that a reader can see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// Where it leads, as an absolute address, fragment included.
    pub url: String,
    /// Its text, white space collapsed.
    pub text: String,
    /// The tokens of its `rel`, in lower case.
    pub rel: Vec<String>,
    /// Whether it leads to a page of the site the page belongs to: the
    /// same registrable domain.
    pub same_site: bool,
}

/// The main content of a page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MainContent {
    /// As text: one block after another with an empty line between them,
    /// the items of a list and the rows of a table one to a line.
    pub text: String,
    /// As markdown.
    pub markdown: String,
    /// Its headings, in order.
    pub headings: Vec<Heading>,
}

/// What of a page is extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Whether the main content is also written as markdown.
    pub markdown: bool,
    /// Whether the links of the page are listed.
    pub links: bool,
    /// A page whose main content has fewer words is thin.
    pub thin_words: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            markdown: true,
            links: true,
            thin_words: DEFAULT_THIN_WORDS,
        }
    }
}

/// Everything extracted from a page (spec §14).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Extract {
    /// The `<title>` of the document.
    pub title: Option<String>,
    /// `<meta name="description">`.
    pub description: Option<String>,
    /// `<link rel="canonical">`, as an absolute address.
    pub canonical: Option<String>,
    /// The language the document says it is in.
    pub lang: Option<String>,
    /// When the page says it was published: in UTC (RFC 3339) when it
    /// names a zone, as written when it names none.
    pub published_at: Option<String>,
    /// When the page says it was last changed, in the same spelling.
    pub modified_at: Option<String>,
    /// `<meta>` elements that are neither OpenGraph nor a Twitter card: a
    /// name in lower case to every value it has, in document order.
    pub meta: Named,
    /// OpenGraph properties by their full name (`og:title`,
    /// `article:tag`).
    pub opengraph: Named,
    /// Twitter card properties by their full name (`twitter:card`).
    pub twitter: Named,
    /// Every JSON-LD value of the page, as the page wrote it.
    pub jsonld: Vec<serde_json::Value>,
    /// JSON-LD blocks that were left out: no JSON, or too many.
    pub jsonld_skipped: u32,
    /// The links a reader can see, in document order; `None` when they
    /// were not asked for.
    pub links: Option<Vec<Link>>,
    /// Whether the page held more links than are listed.
    pub links_truncated: bool,
    /// The headings of the main content, in order.
    pub headings: Vec<Heading>,
    /// The main content as text.
    pub text: String,
    /// The main content as markdown; `None` when it was not asked for.
    pub markdown: Option<String>,
    /// The words of `text`.
    pub word_count: u64,
    /// Whether the page gave a reader next to nothing: its main content
    /// has fewer words than the configured number. On the http path that
    /// is a page scripts would have filled, or a wall.
    pub thin_content: bool,
    /// Why this is not the whole document, when it is not: its body was
    /// cut, or its parse stopped at its time. `None` for a whole one.
    #[serde(default)]
    pub truncated: Option<Truncated>,
}

impl Extract {
    /// Leaves out of the links what the browser that rendered the page
    /// reports a reader cannot see (F123, D48): what a style sheet hides
    /// only a browser knows. `seen` is every link of the browser's
    /// document, where it leads and whether a reader sees it. A link is
    /// kept as often as the browser saw its address shown; an address the
    /// browser did not list is kept, for the markup did not hide it.
    pub fn leave_out_unseen<'a>(&mut self, seen: impl IntoIterator<Item = (&'a str, bool)>) {
        let Some(links) = self.links.as_mut() else {
            return;
        };
        let mut shown: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for (url, visible) in seen {
            let Ok(url) = url::Url::parse(url) else {
                continue;
            };
            let count = shown.entry(same_address(url.as_str())).or_insert(0);
            if visible {
                *count += 1;
            }
        }
        links.retain(|link| match shown.get_mut(&same_address(&link.url)) {
            None => true,
            Some(0) => false,
            Some(left) => {
                *left -= 1;
                true
            }
        });
    }
}

/// The characters that browsers and Kage's parser spell either as they
/// are or percent-encoded in a serialized address, and that delimit
/// nothing in any part of it: Firefox and Chrome encode `^` in a path,
/// Chrome `|` too, where Kage's parser keeps them (QA round three,
/// extract-06). Either spelling names the same address.
const SPELLED_EITHER_WAY: &[u8] = b"^|{}`<>\"' ";

/// An address in one spelling for comparing links: every percent-encoded
/// character of [`SPELLED_EITHER_WAY`] as itself, every other escape in
/// upper case. Only for telling two serialized addresses apart; never an
/// address to use.
fn same_address(url: &str) -> String {
    let bytes = url.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = (bytes[index] == b'%')
            .then(|| bytes.get(index + 1..index + 3))
            .flatten()
            .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) if SPELLED_EITHER_WAY.contains(&byte) => {
                out.push(byte);
                index += 3;
            }
            Some(_) => {
                out.push(b'%');
                out.extend(
                    bytes[index + 1..index + 3]
                        .iter()
                        .map(u8::to_ascii_uppercase),
                );
                index += 3;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn found(content: &content::Content<'_>, document: &Document, markdown: bool) -> MainContent {
    let resolve = |href: &str| document.resolve(href);
    let text = render::render_with(content, &resolve, Mode::Text);
    MainContent {
        markdown: if markdown {
            render::render_with(content, &resolve, Mode::Markdown).text
        } else {
            String::new()
        },
        text: text.text,
        headings: text
            .headings
            .into_iter()
            .map(|heading| Heading {
                level: heading.level,
                text: heading.text,
                id: heading.id,
            })
            .collect(),
    }
}

/// Finds the main content of a page and writes it as text and as markdown.
pub fn main_content(document: &Document) -> MainContent {
    let content = content::find(document.tree().tree.root());
    found(&content, document, true)
}

/// Extracts everything `include: ["extract"]` returns from a page.
pub fn extract(document: &Document, options: &Options) -> Extract {
    let meta = meta::read(document);
    let content = content::find(document.tree().tree.root());
    let main = found(&content, document, options.markdown);
    let (links, links_truncated) = if options.links {
        let page = document.page();
        let links = page
            .links
            .into_iter()
            // A link the markup hides is one nobody can follow.
            .filter(|link| !link.hidden)
            .map(|link| Link {
                same_site: site::same(&link.url, document.loaded_from()),
                url: link.url.into(),
                text: link.text,
                rel: link.rel,
            })
            .collect();
        (Some(links), page.links_truncated)
    } else {
        (None, false)
    };
    let word_count = main.text.split_whitespace().count();
    Extract {
        title: meta.title,
        description: meta.description,
        canonical: meta.canonical,
        lang: meta.lang,
        published_at: meta.published_at,
        modified_at: meta.modified_at,
        meta: meta.meta,
        opengraph: meta.opengraph,
        twitter: meta.twitter,
        jsonld: meta.jsonld,
        jsonld_skipped: meta.jsonld_skipped,
        links,
        links_truncated,
        headings: main.headings,
        text: main.text,
        markdown: options.markdown.then_some(main.markdown),
        word_count: u64::try_from(word_count).unwrap_or(u64::MAX),
        thin_content: word_count < options.thin_words,
        truncated: document.cut().then_some(Truncated::ParseTime),
    }
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{CRATE_NAME, Document, Options, Truncated, extract, main_content};

    fn document(html: &str) -> Document {
        Document::parse(
            html,
            &Url::parse("https://www.shop.example/catalogue/item?id=7").unwrap(),
        )
    }

    fn article(paragraphs: usize) -> String {
        let paragraph = "<p>The harbour was quiet that morning, and the boats, which had come in late, lay \
                         side by side along the quay, their nets drying in the wind.</p>";
        format!(
            r#"<html lang="en"><head><title>The harbour</title>
            <meta name="description" content="A morning at the harbour.">
            <meta property="og:type" content="article"></head><body>
            <nav><a href="/">Home</a> <a href="https://other.example/x" rel="nofollow NoOpener">Elsewhere</a></nav>
            <div class="entry-content"><h1 id="top">The harbour</h1>{}
            <p><a href="/next#part">The next chapter</a> follows, and <a href="/hidden" style="display:none">this</a> does not.</p></div>
            <footer><a href="//blog.shop.example/about">About</a></footer></body></html>"#,
            paragraph.repeat(paragraphs)
        )
    }

    #[test]
    fn smoke() {
        assert_eq!(CRATE_NAME, "kage-extract");
    }

    /// What is extracted is read from text the http path decoded by its
    /// declaration (F100): the title, the text, the markdown and the links
    /// of a page written in another encoding say what it says, and the
    /// `<meta charset>` it carries changes nothing of it.
    #[test]
    fn a_decoded_document_is_extracted_as_it_says() {
        let words = (0..100)
            .map(|n| format!("wort{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let html = format!(
            "<html><head><meta charset=\"shift_jis\"><title>日本語 Café</title></head><body>\
             <article><h1>Straße München</h1><p>{words} <a href=\"/für\">für</a></p></article></body></html>"
        );
        let extracted = extract(&document(&html), &Options::default());
        assert_eq!(extracted.title.as_deref(), Some("日本語 Café"));
        assert!(
            extracted.text.starts_with("Straße München"),
            "{}",
            extracted.text
        );
        assert!(
            extracted
                .markdown
                .as_deref()
                .unwrap()
                .starts_with("# Straße München"),
        );
        assert_eq!(
            extracted.links.as_ref().unwrap()[0].url,
            "https://www.shop.example/f%C3%BCr"
        );
        assert!(!extracted.text.contains('\u{fffd}'));
    }

    #[test]
    fn a_page_is_extracted_whole() {
        let extracted = extract(&document(&article(6)), &Options::default());
        assert_eq!(extracted.title.as_deref(), Some("The harbour"));
        assert_eq!(
            extracted.description.as_deref(),
            Some("A morning at the harbour.")
        );
        assert_eq!(extracted.lang.as_deref(), Some("en"));
        assert_eq!(extracted.opengraph["og:type"], ["article"]);
        assert_eq!((extracted.canonical, extracted.published_at), (None, None));
        assert!(
            extracted
                .text
                .starts_with("The harbour\n\nThe harbour was quiet")
        );
        assert!(
            extracted
                .text
                .ends_with("The next chapter follows, and does not.")
        );
        let markdown = extracted.markdown.unwrap();
        assert!(markdown.starts_with("# The harbour\n\nThe harbour was quiet"));
        assert!(
            markdown.contains("[The next chapter](https://www.shop.example/next#part) follows")
        );
        assert_eq!(extracted.headings.len(), 1);
        assert_eq!(
            (
                extracted.headings[0].level,
                extracted.headings[0].id.as_deref()
            ),
            (1, Some("top"))
        );
        // Six paragraphs of 27 words, a heading of two and a line of seven.
        assert_eq!(extracted.word_count, 6 * 27 + 2 + 7);
        assert!(!extracted.thin_content);
        // The links a reader can see, where they lead and whose they are.
        let links = extracted.links.unwrap();
        let seen: Vec<(&str, bool)> = links
            .iter()
            .map(|link| (link.url.as_str(), link.same_site))
            .collect();
        assert_eq!(
            seen,
            [
                ("https://www.shop.example/", true),
                ("https://other.example/x", false),
                ("https://www.shop.example/next#part", true),
                ("https://blog.shop.example/about", true),
            ]
        );
        assert_eq!(links[1].rel, ["nofollow", "noopener"]);
        assert_eq!(links[2].text, "The next chapter");
        assert!(!extracted.links_truncated);
    }

    #[test]
    fn what_was_not_asked_for_is_not_there() {
        let options = Options {
            markdown: false,
            links: false,
            ..Options::default()
        };
        let extracted = extract(&document(&article(6)), &options);
        assert_eq!((extracted.markdown, extracted.links), (None, None));
        assert!(extracted.text.starts_with("The harbour"));
    }

    #[test]
    fn a_page_with_little_to_read_is_thin() {
        // Two paragraphs of 27 words, the heading's two and the line's seven.
        let short = extract(&document(&article(2)), &Options::default());
        assert_eq!(short.word_count, 63);
        assert!(short.thin_content);
        // The number is the configuration's: at exactly as many words a
        // page is not thin, one word under it is.
        let at = |thin_words| {
            extract(
                &document(&article(2)),
                &Options {
                    thin_words,
                    ..Options::default()
                },
            )
            .thin_content
        };
        assert!(!at(63));
        assert!(at(64));
        // A shell that scripts would fill says nothing at all.
        let shell = extract(
            &document(
                r#"<html><head><title>App</title></head><body><noscript>You need to enable JavaScript.</noscript>
                <div id="root"></div><div aria-hidden="true">Loading…</div><script src="/app.js"></script></body></html>"#,
            ),
            &Options::default(),
        );
        assert_eq!((shell.text.as_str(), shell.word_count), ("", 0));
        assert!(shell.thin_content);
        assert_eq!(shell.title.as_deref(), Some("App"));
        assert_eq!(main_content(&document("")).text, "");
    }
    /// QA round one's extract-06 (F123): a link the browser reports hidden
    /// (a style sheet's `display: none`) is left out of `links`, as the
    /// crawl leaves it out; the same address shown elsewhere stays as often
    /// as it is shown, and one the browser did not list stays.
    #[test]
    fn what_the_browser_reports_hidden_is_no_link() {
        let html = r#"<html><body><article><p>Read <a href="/shown">shown</a> and <a class="x" href="/classhid">this</a>,
            <a href="/twice">once</a> <a class="x" href="/twice">twice</a>, <a href="/unlisted">unlisted</a>.</p></article></body></html>"#;
        let mut extracted = extract(&document(html), &Options::default());
        extracted.leave_out_unseen([
            ("https://www.shop.example/shown", true),
            ("https://www.shop.example/classhid", false),
            ("https://www.shop.example/twice", true),
            ("https://www.shop.example/twice", false),
            ("not a url", false),
        ]);
        let left: Vec<&str> = extracted
            .links
            .as_ref()
            .unwrap()
            .iter()
            .map(|link| link.url.as_str())
            .collect();
        assert_eq!(
            left,
            [
                "https://www.shop.example/shown",
                "https://www.shop.example/twice",
                "https://www.shop.example/unlisted"
            ]
        );
        // Without links asked for there is nothing to leave out.
        let mut lean = extract(
            &document(html),
            &Options {
                links: false,
                ..Options::default()
            },
        );
        lean.leave_out_unseen([("https://www.shop.example/shown", false)]);
        assert_eq!(lean.links, None);
    }

    /// QA round three's extract-06: the browser spells some characters of
    /// an address percent-encoded where Kage's parser keeps them (Firefox
    /// and Chrome `^` in a path, Chrome `|` too); the link it reports
    /// hidden is left out whatever the spelling, and an escape of a
    /// character that delimits is no other spelling of it.
    #[test]
    fn a_hidden_link_is_left_out_however_the_browser_spells_its_address() {
        let html = r#"<html><body><article><p>Read <a class="x" href="/h/pipe|x">a</a> <a class="x" href="/h/caret^x">b</a>
            <a class="x" href="/h/brace{x}">c</a> <a class="x" href="/h/q?a=`b`">d</a> <a href="/s/shown">shown</a>
            <a class="x" href="/h/slash%2Fx">e</a> <a href="/h/case%7cx">f</a></p></article></body></html>"#;
        let mut extracted = extract(&document(html), &Options::default());
        let kept: Vec<String> = extracted
            .links
            .as_ref()
            .unwrap()
            .iter()
            .map(|link| link.url.clone())
            .collect();
        assert!(
            kept.contains(&"https://www.shop.example/h/caret^x".to_owned()),
            "{kept:?}"
        );
        extracted.leave_out_unseen([
            // As Chrome 155 lists them.
            ("https://www.shop.example/h/pipe%7Cx", false),
            ("https://www.shop.example/h/caret%5Ex", false),
            ("https://www.shop.example/h/brace%7Bx%7D", false),
            ("https://www.shop.example/h/q?a=%60b%60", false),
            ("https://www.shop.example/s/shown", true),
            // `/h/slash/x` is another address than `/h/slash%2Fx`.
            ("https://www.shop.example/h/slash/x", false),
            ("https://www.shop.example/h/case%7Cx", true),
        ]);
        let left: Vec<&str> = extracted
            .links
            .as_ref()
            .unwrap()
            .iter()
            .map(|link| link.url.as_str())
            .collect();
        assert_eq!(
            left,
            [
                "https://www.shop.example/s/shown",
                "https://www.shop.example/h/slash%2Fx",
                "https://www.shop.example/h/case%7cx"
            ]
        );
        assert_eq!(
            super::same_address("https://a.example/%5e%7C%2f%zz%"),
            "https://a.example/^|%2F%zz%"
        );
    }

    /// F147: a document whose parse stopped at its time says so; a whole
    /// one says nothing, and a stored extraction from before the member
    /// reads as whole.
    #[test]
    fn an_extraction_of_a_cut_parse_says_so() {
        let base = Url::parse("https://www.shop.example/").unwrap();
        let whole = extract(&document(&article(6)), &Options::default());
        assert_eq!(whole.truncated, None);
        let cut = Document::parse_within(
            &article(6),
            &base,
            encoding_rs::UTF_8,
            std::time::Duration::ZERO,
        );
        let extracted = extract(&cut, &Options::default());
        assert_eq!(extracted.truncated, Some(Truncated::ParseTime));
        let json = serde_json::to_value(&extracted).unwrap();
        assert_eq!(json["truncated"], "parse_time");
        let mut old = serde_json::to_value(&whole).unwrap();
        old.as_object_mut().unwrap().remove("truncated");
        let read: super::Extract = serde_json::from_value(old).unwrap();
        assert_eq!(read.truncated, None);
    }
}
