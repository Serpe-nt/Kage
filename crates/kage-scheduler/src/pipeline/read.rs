//! What is read from the document of a page a fetch got (spec §13, §14):
//! what a crawl takes from it (its links, its text) and what a request
//! asked to have extracted. The document is the one the visit captured,
//! never a second request, and it is parsed once for both.

use encoding_rs::Encoding;
use kage_core::domain::Header;
use kage_core::outcome::Outcome;
use kage_crawl::html::Page;
use kage_extract::{Document, Options, Truncated};
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
    /// Whether the document read was not the whole page: its body was cut
    /// at `max_body_bytes` (F122), or its parse stopped at its time
    /// (F147). Such a page is not put into the cache.
    pub cut: bool,
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
/// and from the tree its document was parsed into. A response that is no
/// HTML document gives no links and no text, whatever a browser made of
/// it (F118).
fn crawl_reading(last: &AttemptOutput, page: Option<&Page>, document: bool) -> CrawlReading {
    // What extraction read of a body that is no HTML document is no page
    // of the crawl's either.
    let page = page.filter(|_| document);
    let seen = last.page.as_ref().filter(|_| document).and_then(|seen| {
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

/// Whether a response is a document extraction reads: it says it is HTML,
/// or it says nothing and its body is text by the sniffing that decides
/// whether a body is shown as `raw_body` (F180). A body a browser showed
/// without bringing its bytes is taken for the page it showed.
pub(super) fn is_document(output: &AttemptOutput) -> bool {
    if !is_html(&output.headers) {
        return false;
    }
    let typed = output.headers.iter().any(|header| {
        header.name.eq_ignore_ascii_case("content-type")
            && !header
                .value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .is_empty()
    });
    typed
        || output
            .sent_body()
            .is_none_or(|body| kage_http::charset::untyped_is_text(&body))
}

/// Whether a crawl reads a response's links and text: an HTML document
/// ([`is_document`]) that says it is HTML, or says nothing and begins as
/// HTML does, as a browser's sniffing takes it. An untyped JSON body is
/// text to a browser, and its escaped quotes are no links (crawl-01, F118).
/// A body a browser showed without bringing its bytes is the browser's
/// page.
pub(super) fn is_markup(output: &AttemptOutput) -> bool {
    if !is_document(output) {
        return false;
    }
    let typed = output.headers.iter().any(|header| {
        header.name.eq_ignore_ascii_case("content-type")
            && !header
                .value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .is_empty()
    });
    typed
        || output
            .sent_body()
            .is_none_or(|body| kage_http::charset::sniffs_as_html(&body))
}

/// A document an attempt captured.
#[derive(Debug)]
pub(super) struct Captured {
    /// Its text.
    pub text: String,
    /// The encoding it was written in.
    pub encoding: &'static Encoding,
    /// Whether it was cut at `max_body_bytes` (F122).
    pub cut: bool,
}

/// The document an attempt captured and the encoding it was written in:
/// the tree as the browser had it after the page settled (UTF-8, the
/// browser resolved what it wrote), else the body the http path fetched,
/// decoded as a browser decodes it, in the encoding a link's query is
/// written in (F161); and whether it was cut at `max_body_bytes`.
pub(super) fn document_of(output: &AttemptOutput) -> Option<Captured> {
    output
        .page
        .as_ref()
        .and_then(|page| {
            page.dom_html.clone().map(|dom| Captured {
                text: dom,
                encoding: encoding_rs::UTF_8,
                cut: page.dom_truncated,
            })
        })
        .or_else(|| {
            output.fetched.as_ref().map(|fetched| {
                let (text, encoding) = fetched.decoded();
                Captured {
                    text,
                    encoding,
                    cut: fetched.truncated,
                }
            })
        })
}

/// The links of the browser's document with whether a reader sees each;
/// `None` when the attempt brought none (F123).
pub(super) fn seen_links(output: &AttemptOutput) -> Option<Vec<(String, bool)>> {
    output
        .page
        .as_ref()
        .and_then(|page| page.links.as_ref())
        .map(|links| {
            links
                .iter()
                .map(|link| (link.url.clone(), link.visible))
                .collect()
        })
}

/// Parses a document once and reads from it what is asked for. Off the
/// async threads: a large page takes milliseconds, and its parse is bounded
/// in time (F147). The links of an extraction leave out what the browser
/// reports a reader cannot see, when it was asked (`seen`, F123).
pub(super) async fn read_document(
    captured: Captured,
    loaded_from: Url,
    page: bool,
    options: Option<Options>,
    seen: Option<Vec<(String, bool)>>,
) -> Reading {
    let started = std::time::Instant::now();
    let read = tokio::task::spawn_blocking(move || {
        let document = Document::parse_in(&captured.text, &loaded_from, captured.encoding);
        let page = page.then(|| document.page());
        let extract = options.map(|options| {
            let mut extract = kage_extract::extract(&document, &options);
            if let Some(seen) = &seen {
                extract
                    .leave_out_unseen(seen.iter().map(|(url, visible)| (url.as_str(), *visible)));
            }
            if captured.cut {
                extract.truncated = Some(Truncated::MaxBodyBytes);
            }
            extract
        });
        (page, extract, captured.cut || document.cut())
    })
    .await;
    metrics::extracted(started.elapsed());
    match read {
        Ok((page, extract, cut)) => Reading {
            page,
            crawl: None,
            thin: extract.as_ref().is_some_and(|extract| extract.thin_content),
            extract: extract.and_then(|extract| serde_json::to_value(extract).ok()),
            cut,
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
        let document = is_document(last);
        let options = order.extract.filter(|_| outcome.is_success() && document);
        // A crawl reads a page's links and text from its markup only when
        // it is an HTML document: the escaped quotes of a JSON body or the
        // text of a plain file are no links, typed or not (F118, crawl-01).
        let read_page = order.read_page && is_markup(last);
        if !read_page && options.is_none() {
            return Reading {
                crawl: (order.read_page && outcome.is_success())
                    .then(|| crawl_reading(last, None, false)),
                ..Reading::default()
            };
        }
        let Some(captured) = document_of(last) else {
            return Reading::default();
        };
        let loaded_from = last
            .final_url
            .as_deref()
            .and_then(|url| Url::parse(url).ok())
            .unwrap_or_else(|| order.url.clone());
        let seen = options
            .filter(|options| options.links)
            .and_then(|_| seen_links(last));
        let mut reading = read_document(captured, loaded_from, read_page, options, seen).await;
        // What a crawl takes from a page it got, to be stored with the
        // page's result.
        reading.crawl = (order.read_page && outcome.is_success())
            .then(|| crawl_reading(last, reading.page.as_ref(), read_page));
        reading
    }
}

#[cfg(test)]
mod tests {
    use encoding_rs::UTF_8;
    use kage_core::domain::Header;
    use kage_extract::Options;
    use url::Url;

    use kage_core::domain::Timing;
    use kage_core::outcome::Outcome;
    use kage_core::schedule::politeness::Courtesy;
    use kage_http::Fetched;

    use super::{Captured, document_of, is_document, is_html, read_document};
    use crate::executor::AttemptOutput;

    /// A document of the browser's, whole.
    fn whole(text: String) -> Captured {
        Captured {
            text,
            encoding: UTF_8,
            cut: false,
        }
    }

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

        let nothing = read_document(whole(html.clone()), base.clone(), false, None, None).await;
        assert!(nothing.page.is_none() && nothing.extract.is_none() && !nothing.thin);

        let both = read_document(
            whole(html.clone()),
            base.clone(),
            true,
            Some(Options::default()),
            None,
        )
        .await;
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
        let thin = read_document(whole(shell), base, false, Some(lean), None).await;
        assert!(thin.page.is_none() && thin.thin);
        let extract = thin.extract.unwrap();
        assert_eq!(extract["thin_content"], true);
        assert!(extract["markdown"].is_null() && extract["links"].is_null());
    }

    /// The finding's page (F97): five thousand nested `div` and a
    /// paragraph, and a page four times as deep, read for a crawl and for a
    /// request on the blocking pool of a runtime whose threads have the
    /// 2 MiB stack the server's have. A walk that called itself once per
    /// level overflowed it, and the process with it; the result is whole.
    #[test]
    fn a_page_nested_deeper_than_a_workers_stack_is_read_whole() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_stack_size(2 * 1024 * 1024)
            .enable_all()
            .build()
            .unwrap();
        let words = (0..100)
            .map(|index| format!("word{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let base = Url::parse("http://deep.kage.test/page").unwrap();
        for depth in [5_000_usize, 20_000] {
            let html = format!(
                "<!doctype html><title>Deep</title>{}<p>{words} <a href=\"next\">on</a></p>",
                "<div>".repeat(depth)
            );
            let read = runtime.block_on(read_document(
                whole(html),
                base.clone(),
                true,
                Some(Options::default()),
                None,
            ));
            let page = read.page.unwrap();
            assert_eq!(page.title, "Deep", "{depth}");
            let links: Vec<&str> = page.links.iter().map(|link| link.url.as_str()).collect();
            assert_eq!(links, ["http://deep.kage.test/next"], "{depth}");
            assert!(page.main_text.starts_with("word0 word1 "), "{depth}");
            let extract = read.extract.unwrap();
            assert_eq!(extract["title"], "Deep", "{depth}");
            assert_eq!(extract["word_count"], 101, "{depth}");
            assert_eq!(extract["links"].as_array().unwrap().len(), 1, "{depth}");
            assert!(!read.thin, "{depth}");
        }
    }

    /// What the http path fetched, for a test: `body` with `content_type`.
    fn fetched(content_type: &str, body: &[u8]) -> AttemptOutput {
        let mut output = AttemptOutput::failed(Outcome::Ok, "test", Courtesy::Answered);
        output.status = Some(200);
        output.fetched = Some(Box::new(Fetched {
            status: 200,
            final_url: Url::parse("http://latin.kage.test/page").unwrap(),
            http_version: "http/1.1",
            headers: vec![Header {
                name: "Content-Type".to_owned(),
                value: content_type.to_owned(),
            }],
            body: body.to_vec().into(),
            truncated: false,
            hops: Vec::new(),
            downgraded: false,
            timing: Timing::default(),
            subresources: None,
            text_cache: kage_http::TextCache::default(),
        }));
        output
    }

    /// QA round one's extract-03 and crawl-04 (F100): the document of a page
    /// the http path fetched is read in the encoding it declares, so the
    /// title, the text and the links of a Latin-1 page are what it says,
    /// and a link whose name is not ASCII is the address a browser asks.
    #[tokio::test]
    async fn the_document_of_the_http_path_is_read_in_its_declared_encoding() {
        let words = "wort ".repeat(100);
        let mut body =
            b"<!doctype html><html><head><meta charset=\"iso-8859-1\"><title>Caf\xe9</title></head>\
              <body><article><p>Stra\xdfe M\xfcnchen "
                .to_vec();
        body.extend_from_slice(words.as_bytes());
        body.extend_from_slice(b"<a href=\"/f\xfcr\">f\xfcr</a> <a href=\"/s?q=f\xfcr\">s</a></p></article></body></html>");
        let output = fetched("text/html", &body);
        let document = document_of(&output).unwrap();
        assert!(
            document.text.contains("<title>Café</title>"),
            "{}",
            document.text
        );
        assert_eq!(document.encoding, encoding_rs::WINDOWS_1252);
        assert!(!document.cut);
        let base = Url::parse("http://latin.kage.test/page").unwrap();
        let read = read_document(document, base, true, Some(Options::default()), None).await;
        let page = read.page.unwrap();
        assert_eq!(page.title, "Café");
        // A path is UTF-8; a query is written in the page's encoding, as a
        // browser writes it (F161).
        assert_eq!(
            page.links[0].url.as_str(),
            "http://latin.kage.test/f%C3%BCr"
        );
        assert_eq!(
            page.links[1].url.as_str(),
            "http://latin.kage.test/s?q=f%FCr"
        );
        let extract = read.extract.unwrap();
        assert_eq!(extract["title"], "Café");
        assert!(
            extract["text"]
                .as_str()
                .unwrap()
                .starts_with("Straße München")
        );
        // What the site sent is what `sent_body` gives, byte for byte.
        assert_eq!(output.sent_body().unwrap(), body);
    }

    /// QA round one's api-01 and crawl-03 (F99): the body of a file a level
    /// 0 attempt fetched is read like a browser's page's raw body.
    #[test]
    fn the_body_the_site_sent_is_the_pages_or_the_http_paths() {
        let file = fetched("text/plain", b"User-agent: *\nDisallow: /private\n");
        assert_eq!(
            file.sent_body().as_deref(),
            Some(b"User-agent: *\nDisallow: /private\n".as_slice())
        );
        let nothing = AttemptOutput::failed(Outcome::Timeout, "timeout", Courtesy::NotTheSite);
        assert_eq!(nothing.sent_body(), None);
    }
    /// QA round one's extract-04 (F122): a body cut at `max_body_bytes`
    /// says so in what is read from it, and a reading of it is cut, which
    /// keeps it out of the cache.
    #[tokio::test]
    async fn a_document_cut_at_its_limit_says_so() {
        let words = "word ".repeat(300);
        let body = format!("<!doctype html><title>Big</title><article><p>{words}</p>");
        let mut output = fetched("text/html", body.as_bytes());
        if let Some(fetched) = output.fetched.as_mut() {
            fetched.truncated = true;
        }
        let document = document_of(&output).unwrap();
        assert!(document.cut);
        let base = Url::parse("http://latin.kage.test/page").unwrap();
        let read =
            read_document(document, base.clone(), true, Some(Options::default()), None).await;
        assert!(read.cut);
        let extract = read.extract.unwrap();
        assert_eq!(extract["truncated"], "max_body_bytes");
        assert_eq!(extract["thin_content"], false);
        // A whole one says nothing of the kind.
        let whole_read =
            read_document(whole(body), base, false, Some(Options::default()), None).await;
        assert!(!whole_read.cut);
        assert!(whole_read.extract.unwrap()["truncated"].is_null());
    }

    /// QA round two's http-08 (F180): a body of no type is a document when
    /// it is text by the sniffing that shows it as `raw_body`, and bytes
    /// that are no text get no extraction, as they get no `raw_body`.
    #[test]
    fn a_body_of_no_type_is_a_document_only_when_it_is_text() {
        let untyped = |body: &[u8]| {
            let mut output = fetched("text/html", body);
            if let Some(fetched) = output.fetched.as_mut() {
                fetched.headers.clear();
            }
            output.headers = Vec::new();
            output
        };
        assert!(!is_document(&untyped(&[
            0x00, 0x01, 0xfe, 0xe9, 0x10, 0x80
        ])));
        assert!(is_document(&untyped(b"<p>a page</p>")));
        assert!(is_document(&untyped("plain text, UTF-8 ✓".as_bytes())));
        assert!(is_document(&untyped(b"\xef\xbb\xbfwith a byte order mark")));
        // An empty type is no type.
        let mut empty = untyped(&[0x00, 0x80]);
        empty.headers = typed(" ; charset=utf-8");
        assert!(!is_document(&empty));
        // A declared type decides, whatever the bytes.
        let mut declared = untyped(&[0x00, 0x80]);
        declared.headers = typed("text/html");
        assert!(is_document(&declared));
        declared.headers = typed("image/png");
        assert!(!is_document(&declared));
        // A page a browser showed without its bytes is the page it showed.
        let browser = AttemptOutput::failed(Outcome::Ok, "test", Courtesy::Answered);
        assert!(is_document(&browser));
    }

    /// QA round one's extract-06 (F123): the links of an extraction leave
    /// out what the browser reports a reader cannot see.
    #[tokio::test]
    async fn the_links_of_an_extraction_are_those_the_browser_shows() {
        let html = "<html><body><article><p>Read <a href=\"/shown\">shown</a> and \
                    <a class=\"x\" href=\"/classhid\">this</a>.</p></article></body></html>"
            .to_owned();
        let base = Url::parse("http://other.qa.test/page").unwrap();
        let seen = vec![
            ("http://other.qa.test/shown".to_owned(), true),
            ("http://other.qa.test/classhid".to_owned(), false),
        ];
        let read = read_document(
            whole(html.clone()),
            base.clone(),
            false,
            Some(Options::default()),
            Some(seen),
        )
        .await;
        let extract = read.extract.unwrap();
        let links: Vec<&str> = extract["links"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|link| link["url"].as_str())
            .collect();
        assert_eq!(links, ["http://other.qa.test/shown"]);
        // Without the browser's word, the markup's stands.
        let markup = read_document(whole(html), base, false, Some(Options::default()), None).await;
        assert_eq!(
            markup.extract.unwrap()["links"].as_array().unwrap().len(),
            2
        );
    }
}
