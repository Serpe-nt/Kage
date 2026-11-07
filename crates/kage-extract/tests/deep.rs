//! Extraction of a page nested deeper than a worker's stack (F97, QA
//! finding extract-01): a page of five thousand nested elements read with
//! `include: ["extract"]` killed the server, and everything in flight with
//! it. Every test here runs on a thread with the 2 MiB a Tokio worker has,
//! so that a parse, a walk or a rendering that called itself once per level
//! would overflow here as it did there. The result is whole: what the page
//! says, at any depth.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use kage_extract::{Document, Options, extract, main_content};
use url::Url;

/// The stack of a Tokio worker thread, and of its blocking pool.
const WORKER_STACK: usize = 2 * 1024 * 1024;

/// Runs `work` on a thread with a worker's stack, not the test's.
fn on_a_workers_stack<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("worker-sized".to_owned())
            .stack_size(WORKER_STACK)
            .spawn_scoped(scope, work)
            .unwrap()
            .join()
            .unwrap()
    })
}

fn words(count: usize) -> String {
    (0..count)
        .map(|index| format!("word{index}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn loaded_from() -> Url {
    Url::parse("http://deep.kage.test/page").unwrap()
}

/// The finding's page: `depth` `<div>` opened and never closed, then a
/// paragraph of a hundred words.
fn findings_page(depth: usize) -> String {
    format!(
        "<!doctype html><title>Deep</title>{}<p>{}</p>",
        "<div>".repeat(depth),
        words(100)
    )
}

#[test]
fn the_findings_page_is_extracted_on_a_workers_stack() {
    let extracted = on_a_workers_stack(|| {
        let document = Document::parse(&findings_page(5_000), &loaded_from());
        extract(&document, &Options::default())
    });
    assert_eq!(extracted.title.as_deref(), Some("Deep"));
    assert_eq!(extracted.text, words(100));
    assert_eq!(extracted.markdown.as_deref(), Some(words(100).as_str()));
    assert_eq!(extracted.word_count, 100);
    assert!(!extracted.thin_content);
    assert_eq!(extracted.links, Some(Vec::new()));
    assert!(!extracted.links_truncated);
    assert_eq!(extracted.headings, Vec::new());
}

#[test]
fn a_page_of_twenty_thousand_nested_blocks_is_extracted_whole() {
    let depth = 20_000;
    let html = format!(
        "<html lang=\"en\"><head><title>Deeper</title>\
         <meta name=\"description\" content=\"Far down.\"></head>\
         <body><nav><a href=\"/\">Home</a></nav>{}<h1 id=\"top\">A heading</h1>\
         <p>{} <a href=\"next\">the next page</a>.</p>{}</body></html>",
        "<div>".repeat(depth),
        words(100),
        "</div>".repeat(depth)
    );
    let (extracted, main) = on_a_workers_stack(|| {
        let document = Document::parse(&html, &loaded_from());
        (
            extract(&document, &Options::default()),
            main_content(&document),
        )
    });
    assert_eq!(extracted.title.as_deref(), Some("Deeper"));
    assert_eq!(extracted.description.as_deref(), Some("Far down."));
    assert_eq!(extracted.lang.as_deref(), Some("en"));
    assert_eq!(extracted.headings.len(), 1);
    assert_eq!(extracted.headings[0].text, "A heading");
    assert_eq!(extracted.headings[0].id.as_deref(), Some("top"));
    assert_eq!(
        extracted.text,
        format!("A heading\n\n{} the next page.", words(100))
    );
    let markdown = extracted.markdown.unwrap();
    assert!(
        markdown.starts_with("# A heading\n\nword0 word1 "),
        "{markdown}"
    );
    assert!(
        markdown.ends_with("[the next page](http://deep.kage.test/next)."),
        "{markdown}"
    );
    let links = extracted.links.unwrap();
    let seen: Vec<&str> = links.iter().map(|link| link.url.as_str()).collect();
    assert_eq!(
        seen,
        ["http://deep.kage.test/", "http://deep.kage.test/next"]
    );
    assert_eq!(extracted.word_count, 105);
    assert!(!extracted.thin_content);
    assert_eq!(main.text, extracted.text);
    assert_eq!(main.headings, extracted.headings);
}

#[test]
fn a_page_of_twenty_thousand_nested_inline_elements_is_extracted_whole() {
    let depth = 20_000;
    let html = format!(
        "<html><head><title>Inline</title></head><body>{}<p>{} <em>the end</em>.</p>{}</body></html>",
        "<span>".repeat(depth),
        words(100),
        "</span>".repeat(depth)
    );
    let extracted = on_a_workers_stack(|| {
        let document = Document::parse(&html, &loaded_from());
        extract(&document, &Options::default())
    });
    assert_eq!(extracted.title.as_deref(), Some("Inline"));
    assert_eq!(extracted.text, format!("{} the end.", words(100)));
    assert_eq!(
        extracted.markdown.as_deref(),
        Some(format!("{} *the end*.", words(100)).as_str())
    );
    assert_eq!(extracted.word_count, 102);
    assert!(!extracted.thin_content);
}

/// F147: a page nested two hundred thousand elements deep, whose parse
/// would take minutes, is parsed until its time and what was parsed is
/// extracted in a time of the same order: no walk of the extraction reads a
/// subtree once per level (the cleaning read the text under every short
/// block, which over a chain of empty blocks was the square of the depth).
#[test]
fn a_page_whose_parse_was_cut_is_extracted_within_its_time() {
    let html = format!(
        "<!doctype html><title>Deep</title><p>{}</p>{}<p>the end</p>",
        words(100),
        "<div>".repeat(200_000)
    );
    let budget = std::time::Duration::from_secs(2);
    let started = std::time::Instant::now();
    let (extracted, cut, parsed) = on_a_workers_stack(|| {
        let document = Document::parse_within(&html, &loaded_from(), encoding_rs::UTF_8, budget);
        let parsed = started.elapsed();
        let page = document.page();
        assert_eq!(page.title, "Deep");
        (
            extract(&document, &Options::default()),
            document.cut(),
            parsed,
        )
    });
    let took = started.elapsed();
    eprintln!("parsed in {parsed:?}, read and extracted by {took:?}");
    assert!(cut);
    assert_eq!(
        extracted.truncated,
        Some(kage_extract::Truncated::ParseTime)
    );
    assert_eq!(extracted.title.as_deref(), Some("Deep"));
    assert!(
        extracted.text.starts_with("word0 word1 "),
        "{}",
        &extracted.text[..80]
    );
    assert!(!extracted.text.contains("the end"));
    assert!(
        took < budget * 10,
        "the parse stopped at {parsed:?} and all took {took:?}"
    );
}
