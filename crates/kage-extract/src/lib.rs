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
}

fn found(content: &content::Content<'_>, document: &Document, markdown: bool) -> MainContent {
    let text = render::render(content, document.base(), Mode::Text);
    MainContent {
        markdown: if markdown {
            render::render(content, document.base(), Mode::Markdown).text
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
    }
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{CRATE_NAME, Document, Options, extract, main_content};

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
}
