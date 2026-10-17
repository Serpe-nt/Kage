//! What is read from the document of a page a fetch got (spec §13, §14):
//! what a crawl takes from it (its links, its text) and what a request
//! asked to have extracted. The document is the one the visit captured,
//! never a second request, and it is parsed once for both.

use kage_core::domain::Header;
use kage_core::outcome::Outcome;
use kage_crawl::html::Page;
use kage_extract::{Document, Options};
use serde::{Deserialize, Serialize};
use url::Url;

use super::{FetchOrder, Pipeline};
use crate::executor::AttemptOutput;
use crate::metrics;

/// What was read from a page's document.
#[derive(Debug, Default)]
pub(super) struct Reading {
    /// What a crawl reads.
    pub page: Option<Page>,
    /// What a crawl takes from the page, as it is stored with the result.
    pub crawl: Option<CrawlReading>,
    /// The extraction (spec §14), as it is stored.
    pub extract: Option<serde_json::Value>,
    /// Whether the extraction found next to nothing.
    pub thin: bool,
}

impl Reading {
    /// What a crawl took from the page, as it is stored with the result.
    pub(super) fn stored_crawl(&self) -> Option<serde_json::Value> {
        self.crawl
            .as_ref()
            .and_then(|crawl| serde_json::to_value(crawl).ok())
    }
}

/// What a crawl takes from a page it got: where it ended, what it links
/// to and what it says (spec §13). It is stored with the page's result
/// until the crawl has recorded the page, so that a crawl that changes
/// hands between the two still knows the page's links (follow-up F49).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawlReading {
    /// The address the page ended on, after redirects; `None` when the
    /// attempt did not say.
    #[serde(default)]
    pub final_url: Option<String>,
    /// Its links, in document order: the browser's own when it brought
    /// them (with whether a reader can see each), else those of the
    /// markup.
    #[serde(default)]
    pub links: Vec<ReadLink>,
    /// Whether the page held more links than were read.
    #[serde(default)]
    pub links_truncated: bool,
    /// The text two pages are compared by: the main text, without what
    /// the site repeats on every page.
    #[serde(default)]
    pub text: String,
}

/// One link of a page as a crawl takes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadLink {
    /// Where it leads, as written, fragment included.
    pub url: String,
    /// Whether a reader cannot see it.
    #[serde(default)]
    pub hidden: bool,
    /// Whether it stands in what the site repeats on every page.
    #[serde(default)]
    pub furniture: bool,
}

/// What a crawl takes from the last attempt of a fetch that got its page
/// and from the tree its document was parsed into.
fn crawl_reading(last: &AttemptOutput, page: Option<&Page>) -> CrawlReading {
    let seen = last.page.as_ref().and_then(|seen| {
        seen.links
            .as_ref()
            .map(|links| (links, seen.links_truncated))
    });
    let (links, links_truncated) = match (seen, page) {
        (Some((links, truncated)), _) => (
            links
                .iter()
                .map(|link| ReadLink {
                    url: link.url.clone(),
                    hidden: !link.visible,
                    furniture: link.furniture,
                })
                .collect(),
            truncated,
        ),
        (None, Some(page)) => (
            page.links
                .iter()
                .map(|link| ReadLink {
                    url: link.url.to_string(),
                    hidden: link.hidden,
                    furniture: link.furniture,
                })
                .collect(),
            page.links_truncated,
        ),
        (None, None) => (Vec::new(), false),
    };
    CrawlReading {
        final_url: last.final_url.clone(),
        links,
        links_truncated,
        text: page.map(|page| page.main_text.clone()).unwrap_or_default(),
    }
}

/// Whether a response says it is an HTML document. One that does not say
/// what it is is taken for one: a browser showed it as a page.
pub(super) fn is_html(headers: &[Header]) -> bool {
    let Some(content_type) = headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("content-type"))
    else {
        return true;
    };
    let mime = content_type
        .value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    mime.is_empty() || mime == "text/html" || mime == "application/xhtml+xml"
}

/// The document an attempt captured: the tree as the browser had it after
/// the page settled, else the body the http path fetched.
fn document_of(output: &AttemptOutput) -> Option<String> {
    output
        .page
        .as_ref()
        .and_then(|page| page.dom_html.clone())
        .or_else(|| output.fetched.as_ref().map(|fetched| fetched.text()))
}

/// Parses a document once and reads from it what is asked for. Off the
/// async threads: a large page takes milliseconds.
pub(super) async fn read_document(
    document: String,
    loaded_from: Url,
    page: bool,
    options: Option<Options>,
) -> Reading {
    let started = std::time::Instant::now();
    let read = tokio::task::spawn_blocking(move || {
        let document = Document::parse(&document, &loaded_from);
        let page = page.then(|| document.page());
        let extract = options.map(|options| kage_extract::extract(&document, &options));
        (page, extract)
    })
    .await;
    metrics::extracted(started.elapsed());
    match read {
        Ok((page, extract)) => Reading {
            page,
            crawl: None,
            thin: extract.as_ref().is_some_and(|extract| extract.thin_content),
            extract: extract.and_then(|extract| serde_json::to_value(extract).ok()),
        },
        Err(error) => {
            tracing::warn!(%error, "a page's document could not be read");
            Reading::default()
        }
    }
}

impl Pipeline {
    /// Reads what an order asks for from the document of its last attempt.
    /// Nothing is extracted from an attempt that did not get its page, or
    /// from a response that is no HTML document.
    pub(super) async fn read(
        &self,
        order: &FetchOrder,
        outcome: Outcome,
        last: Option<&AttemptOutput>,
    ) -> Reading {
        let Some(last) = last else {
            return Reading::default();
        };
        let options = order
            .extract
            .filter(|_| outcome.is_success() && is_html(&last.headers));
        if !order.read_page && options.is_none() {
            return Reading::default();
        }
        let Some(document) = document_of(last) else {
            return Reading::default();
        };
        let loaded_from = last
            .final_url
            .as_deref()
            .and_then(|url| Url::parse(url).ok())
            .unwrap_or_else(|| order.url.clone());
        let mut reading = read_document(document, loaded_from, order.read_page, options).await;
        // What a crawl takes from a page it got, to be stored with the
        // page's result.
        reading.crawl = (order.read_page && outcome.is_success())
            .then(|| crawl_reading(last, reading.page.as_ref()));
        reading
    }
}

#[cfg(test)]
mod tests {
    use kage_core::domain::Header;
    use kage_extract::Options;
    use url::Url;

    use super::{is_html, read_document};

    fn typed(value: &str) -> Vec<Header> {
        vec![
            Header {
                name: "Server".to_owned(),
                value: "x".to_owned(),
            },
            Header {
                name: "content-TYPE".to_owned(),
                value: value.to_owned(),
            },
        ]
    }

    #[test]
    fn a_response_is_a_document_when_it_says_so_or_says_nothing() {
        for value in [
            "text/html",
            "text/html; charset=utf-8",
            " TEXT/HTML ;charset=iso-8859-1",
            "application/xhtml+xml",
            "",
        ] {
            assert!(is_html(&typed(value)), "{value:?}");
        }
        for value in [
            "text/plain",
            "application/json",
            "application/xml",
            "text/htmlx",
            "application/pdf",
            "image/svg+xml",
        ] {
            assert!(!is_html(&typed(value)), "{value:?}");
        }
        assert!(is_html(&[]));
    }

    #[tokio::test]
    async fn a_document_is_parsed_once_for_what_a_crawl_and_a_request_read() {
        let words = "word ".repeat(120);
        let html = format!(
            "<html><head><title>A page</title></head><body><nav><a href=\"/menu\">Menu</a></nav>\
             <article><h1>Heading</h1><p>{words}</p><p>See <a href=\"next\">the next page</a>.</p>\
             </article></body></html>"
        );
        let base = Url::parse("https://example.org/a/page").unwrap();

        let nothing = read_document(html.clone(), base.clone(), false, None).await;
        assert!(nothing.page.is_none() && nothing.extract.is_none() && !nothing.thin);

        let both = read_document(html.clone(), base.clone(), true, Some(Options::default())).await;
        let page = both.page.unwrap();
        assert_eq!(page.title, "A page");
        let links: Vec<&str> = page.links.iter().map(|link| link.url.as_str()).collect();
        assert_eq!(
            links,
            ["https://example.org/menu", "https://example.org/a/next"]
        );
        let extract = both.extract.unwrap();
        assert_eq!(extract["title"], "A page");
        assert_eq!(extract["thin_content"], false);
        assert_eq!(extract["headings"][0]["text"], "Heading");
        assert!(extract["text"].as_str().unwrap().contains("the next page"));
        assert!(!extract["text"].as_str().unwrap().contains("Menu"));
        assert!(
            extract["markdown"]
                .as_str()
                .unwrap()
                .starts_with("# Heading")
        );
        assert_eq!(extract["links"].as_array().unwrap().len(), 2);
        assert!(!both.thin);

        // What is not asked for is not read, and a page that says next to
        // nothing is thin.
        let lean = Options {
            markdown: false,
            links: false,
            ..Options::default()
        };
        let shell = "<html><body><div id=\"root\">Loading</div></body></html>".to_owned();
        let thin = read_document(shell, base, false, Some(lean)).await;
        assert!(thin.page.is_none() && thin.thin);
        let extract = thin.extract.unwrap();
        assert_eq!(extract["thin_content"], true);
        assert!(extract["markdown"].is_null() && extract["links"].is_null());
    }
}
