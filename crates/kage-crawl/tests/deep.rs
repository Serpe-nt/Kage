//! A page nested deeper than a worker's stack is read (F97, QA finding
//! extract-01): five thousand nested elements killed the server, because
//! the walk that read a page's links and its text called itself once per
//! level. Every test here runs on a thread with the 2 MiB a Tokio worker
//! has, so that a walk that recursed would overflow here as it did there.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use kage_crawl::html::Document;
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

/// A parse budget no test's page outruns, however slow the runner: what is
/// tested here is the stack, not F147's ten seconds, which a runner slowed
/// by coverage instrumentation outran once with a page of 20 000 levels.
const AMPLE: std::time::Duration = std::time::Duration::from_secs(3600);

/// The page `html` parsed with all the time it takes, whole.
fn parsed(html: &str) -> Document {
    let document = Document::parse_within(html, &loaded_from(), encoding_rs::UTF_8, AMPLE);
    assert!(!document.cut());
    document
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
fn the_findings_page_is_read_on_a_workers_stack() {
    let page = on_a_workers_stack(|| parsed(&findings_page(5_000)).page());
    assert_eq!(page.title, "Deep");
    assert_eq!(page.text, words(100));
    assert_eq!(page.main_text, page.text);
    assert_eq!(page.links, Vec::new());
    assert!(!page.links_truncated);
}

#[test]
fn a_page_of_twenty_thousand_nested_elements_gives_its_links_and_text() {
    let depth = 20_000;
    let html = format!(
        "<html><head><title>Deeper</title></head><body><nav><a href=\"/\">Home</a></nav>\
         {}<p>{} <a href=\"next\" rel=\"Next\">the next page</a></p>{}</body></html>",
        "<div>".repeat(depth),
        words(100),
        "</div>".repeat(depth)
    );
    let page = on_a_workers_stack(|| parsed(&html).page());
    assert_eq!(page.title, "Deeper");
    let hrefs: Vec<&str> = page.links.iter().map(|link| link.url.as_str()).collect();
    assert_eq!(
        hrefs,
        ["http://deep.kage.test/", "http://deep.kage.test/next"]
    );
    assert!(page.links[0].furniture && !page.links[0].hidden);
    assert_eq!(page.links[1].text, "the next page");
    assert_eq!(page.links[1].rel, ["next"]);
    assert!(!page.links[1].hidden && !page.links[1].furniture);
    assert!(page.text.starts_with("Home word0 word1 "));
    assert!(page.text.ends_with(" word99 the next page"));
    assert_eq!(page.main_text, format!("{} the next page", words(100)));
    assert!(!page.links_truncated);
}

#[test]
fn what_an_element_hands_down_holds_at_any_depth() {
    let depth = 5_000;
    let html = format!(
        "<body><div style=\"display:none\">{}<a href=\"/hidden\">hidden</a>{}</div>\
         <aside>{}<a href=\"/aside\">in the margin</a>{}</aside>\
         <div>{}<script>var x = 1;</script><p>seen</p>{}</div></body>",
        "<span>".repeat(depth),
        "</span>".repeat(depth),
        "<div>".repeat(depth),
        "</div>".repeat(depth),
        "<section>".repeat(depth),
        "</section>".repeat(depth)
    );
    let page = on_a_workers_stack(|| parsed(&html).page());
    let seen: Vec<(&str, bool, bool)> = page
        .links
        .iter()
        .map(|link| (link.url.path(), link.hidden, link.furniture))
        .collect();
    assert_eq!(seen, [("/hidden", true, false), ("/aside", false, true)]);
    // What is hidden and what a script says is no text; the margin's text
    // stands in for a main text of one word.
    assert_eq!(page.text, "in the margin seen");
    assert_eq!(page.main_text, "in the margin seen");
}
