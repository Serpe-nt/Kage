//! The response cache and extraction on the way of a fetch (spec §14,
//! §15), with a scripted executor on a paused clock: a page the cache
//! holds is no visit, nobody gets a page older than they asked for or
//! another request's page, and what a request asks to have extracted is
//! read from the document its visit captured.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod common;

use std::time::Duration;

use chrono::Duration as Span;
use common::{DEFAULT_DOMAINS, World, blocked, document, file};
use kage_core::domain::{DomainBreaker, RenderMode, SessionId};
use kage_core::ids::{ApiKeyId, RequestId};
use kage_core::outcome::Outcome;
use kage_core::schedule::queue::Priority;
use kage_extract::Options;
use kage_scheduler::{CachePlan, FetchOrder, JobFetch};
use tokio::sync::watch;
use url::Url;

const BROWSER: &str =
    "defaults: { render: browser, pacing: fast, active_hours: ignore }\ndomains: {}\n";

fn get(url: &str) -> FetchOrder {
    FetchOrder {
        render: RenderMode::Browser,
        ..FetchOrder::get(Url::parse(url).unwrap(), Priority::Sync)
    }
}

/// A request that may be answered from the cache with a result up to
/// `minutes` old, under `key`.
fn cached(url: &str, key: &str, minutes: u64) -> FetchOrder {
    FetchOrder {
        cache: Some(CachePlan {
            key: key.to_owned(),
            url_key: url.to_owned(),
            scope: None,
            ttl: Duration::from_mins(minutes),
            read: true,
        }),
        ..get(url)
    }
}

/// An article of `words` words.
fn article(words: usize) -> String {
    format!(
        "<html lang=\"en\"><head><title>The budget</title>\
         <meta name=\"description\" content=\"What the council agreed on.\"></head>\
         <body><nav><a href=\"/\">Home</a> <a href=\"/news\">News</a></nav>\
         <article><h1>The budget</h1><p>{}</p>\
         <p>Read <a href=\"/budget/2027\">the plan for next year</a>.</p></article>\
         <footer>Imprint</footer></body></html>",
        "council ".repeat(words)
    )
}

#[tokio::test(start_paused = true)]
async fn a_page_the_cache_holds_is_no_visit() {
    let mut world = World::new(3, BROWSER);
    let url = "https://shop.example/a";

    // Nothing is there: the page is fetched, and kept for ten minutes.
    assert!(
        !world
            .pipeline
            .has_cached(&cached(url, "k1", 10))
            .await
            .unwrap()
    );
    let first = world.pipeline.fetch(cached(url, "k1", 10)).await.unwrap();
    assert_eq!(first.outcome, Outcome::Ok);
    assert!(first.cached.is_none());
    assert_eq!(world.script.seen().len(), 1);
    let entry = world.backend.cache_entry("k1").unwrap();
    assert_eq!(entry.result_id, first.result);
    assert_eq!(entry.url_key, url);
    assert_eq!(entry.cached_at, first.finished_at);
    assert_eq!(entry.expires_at - entry.cached_at, Span::minutes(10));

    // The same request again: no persona, no attempt, nothing asked of the
    // site. It is a request and a result of its own that says where its
    // page came from.
    tokio::time::sleep(Duration::from_mins(4)).await;
    assert!(
        world
            .pipeline
            .has_cached(&cached(url, "k1", 10))
            .await
            .unwrap()
    );
    let second = world.pipeline.fetch(cached(url, "k1", 10)).await.unwrap();
    assert_eq!(world.script.seen().len(), 1);
    assert_eq!(second.outcome, Outcome::Ok);
    assert!(second.attempts.is_empty() && second.last.is_none());
    assert_ne!(second.request, first.request);
    assert_ne!(second.result, first.result);
    assert_eq!(second.pacing.waited, Duration::ZERO);
    let served = second.cached.as_deref().unwrap();
    assert!(served.cached);
    assert_eq!(served.cached_at, Some(first.finished_at));
    assert_eq!(served.request_id, second.request);
    assert_eq!(served.status, Some(200));
    assert_eq!(served.created_at, world.now());
    assert_eq!(served.persona, first.persona);
    let records = world.backend.records();
    assert_eq!(records.last(), Some(served));
    let requests = world.backend.requests();
    let stored = requests.last().unwrap();
    assert_eq!(stored.id, second.request);
    assert_eq!(stored.outcome, Some(Outcome::Ok));
    // What was served is not put into the cache again.
    assert_eq!(
        world.backend.cache_entry("k1").unwrap().result_id,
        first.result
    );

    // Another request (another include set, another body) is another key.
    let other = world.pipeline.fetch(cached(url, "k2", 10)).await.unwrap();
    assert!(other.cached.is_none());
    assert_eq!(world.script.seen().len(), 2);

    // Nobody gets a page older than they asked for: the entry is five
    // minutes old, and a request that accepts one minute is a visit. Its
    // page takes the entry's place.
    tokio::time::sleep(Duration::from_mins(1)).await;
    assert!(
        !world
            .pipeline
            .has_cached(&cached(url, "k1", 1))
            .await
            .unwrap()
    );
    let fresh = world.pipeline.fetch(cached(url, "k1", 1)).await.unwrap();
    assert!(fresh.cached.is_none());
    assert_eq!(world.script.seen().len(), 3);
    let entry = world.backend.cache_entry("k1").unwrap();
    assert_eq!(entry.result_id, fresh.result);
    assert_eq!(entry.expires_at - entry.cached_at, Span::minutes(1));
    // And an entry is not served past its own lifetime, whatever a later
    // request would accept.
    tokio::time::sleep(Duration::from_mins(2)).await;
    assert!(
        !world
            .pipeline
            .has_cached(&cached(url, "k1", 60))
            .await
            .unwrap()
    );
    let late = world.pipeline.fetch(cached(url, "k1", 60)).await.unwrap();
    assert!(late.cached.is_none());
    assert_eq!(world.script.seen().len(), 4);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn the_cache_answers_while_the_site_is_paused_and_for_a_job() {
    let mut world = World::new(3, BROWSER);
    let url = "https://shop.example/a";
    world.pipeline.fetch(cached(url, "k1", 30)).await.unwrap();

    // The site's breaker is open: a fetch is told so, a page the cache
    // holds is served.
    let mut breaker = DomainBreaker::closed("shop.example", world.now());
    for _ in 0..50 {
        breaker.record_block(world.now(), &world.verdicts.breaker_rules("shop.example"));
    }
    world.backend.put_breaker(breaker);
    tokio::time::sleep(Duration::from_secs(6)).await;
    let paused = world.pipeline.fetch(get(url)).await.unwrap();
    assert_eq!(paused.outcome, Outcome::ProviderError);
    assert_eq!(paused.reason.as_deref(), Some("domain_paused"));
    let served = world.pipeline.fetch(cached(url, "k1", 30)).await.unwrap();
    assert_eq!(served.outcome, Outcome::Ok);
    assert!(served.cached.is_some());

    // An item of a batch is answered the same way.
    let (_keep, stop) = watch::channel(false);
    let item = world
        .pipeline
        .fetch_for_job(RequestId::new(), cached(url, "k1", 30), None, stop)
        .await
        .unwrap();
    let JobFetch::Finished(report) = item else {
        panic!("the item did not finish: {item:?}");
    };
    assert!(report.cached.is_some());
    assert_eq!(world.script.seen().len(), 1);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn who_fetched_a_page_is_shown_to_the_key_that_had_it_fetched() {
    let mut world = World::new(3, BROWSER);
    let url = "https://shop.example/a";
    let (filler, other) = (ApiKeyId::new(), ApiKeyId::new());
    let by = |key: ApiKeyId| FetchOrder {
        api_key: Some(key),
        ..cached(url, "shared", 30)
    };

    let first = world.pipeline.fetch(by(filler)).await.unwrap();
    assert!(first.persona.is_some());

    // A site whose policy shares its pages: another key is served the page
    // and nothing of who fetched it.
    let theirs = world.pipeline.fetch(by(other)).await.unwrap();
    let served = theirs.cached.as_deref().unwrap();
    assert_eq!(served.persona, None);
    assert_eq!(theirs.persona, None);
    // The key that had it fetched sees the persona of that visit.
    let own = world.pipeline.fetch(by(filler)).await.unwrap();
    assert_eq!(own.cached.as_deref().unwrap().persona, first.persona);
    assert_eq!(world.script.seen().len(), 1);
    // Each answer is a request of the key that asked.
    let requests = world.backend.requests();
    let keys: Vec<_> = requests.iter().map(|request| request.api_key_id).collect();
    assert_eq!(keys, [Some(filler), Some(other), Some(filler)]);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn only_a_page_that_was_fetched_and_says_something_is_kept() {
    let mut world = World::new(6, BROWSER);
    let extracting = |url: &str, key: &str| FetchOrder {
        extract: Some(Options::default()),
        max_retries: Some(0),
        ..cached(url, key, 30)
    };

    // A block, an error status, a page extraction calls thin: none is
    // kept, and the next request is a visit.
    world.script.then(blocked(Outcome::Forbidden, 1.0));
    let wall = world
        .pipeline
        .fetch(extracting("https://wall.example/a", "wall"))
        .await
        .unwrap();
    assert_eq!(wall.outcome, Outcome::Forbidden);
    assert!(world.backend.cache_entry("wall").is_none());

    world
        .script
        .then(file("https://shop.example/gone", 404, "Not found"));
    let gone = world
        .pipeline
        .fetch(extracting("https://shop.example/gone", "gone"))
        .await
        .unwrap();
    assert_eq!(
        (gone.outcome, gone.attempts[0].status),
        (Outcome::Ok, Some(404))
    );
    assert!(world.backend.cache_entry("gone").is_none());

    world.script.then(document(
        "https://shop.example/shell",
        "<html><body><div id=\"root\">Loading</div></body></html>",
    ));
    let shell = world
        .pipeline
        .fetch(extracting("https://shop.example/shell", "shell"))
        .await
        .unwrap();
    assert_eq!(shell.outcome, Outcome::Ok);
    assert_eq!(shell.extract.as_ref().unwrap()["thin_content"], true);
    assert!(world.backend.cache_entry("shell").is_none());

    // A page that says something is kept, with what was extracted from it.
    world
        .script
        .then(document("https://shop.example/budget", &article(200)));
    let budget = world
        .pipeline
        .fetch(extracting("https://shop.example/budget", "budget"))
        .await
        .unwrap();
    assert_eq!(budget.extract.as_ref().unwrap()["thin_content"], false);
    assert!(world.backend.cache_entry("budget").is_some());
    let before = world.script.seen().len();
    let again = world
        .pipeline
        .fetch(extracting("https://shop.example/budget", "budget"))
        .await
        .unwrap();
    assert_eq!(world.script.seen().len(), before);
    assert_eq!(again.extract, budget.extract);
    assert_eq!(again.cached.as_deref().unwrap().extract, budget.extract);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_crawl_fills_the_cache_and_a_session_stays_out_of_it() {
    let mut world = World::new(3, BROWSER);
    let url = "https://shop.example/a";
    let filling = || {
        let mut order = cached(url, "k1", 30);
        if let Some(plan) = order.cache.as_mut() {
            plan.read = false;
        }
        order
    };

    // A page of a crawl is a visit every time: a page that is not open in
    // the tab cannot be clicked from. What it read is there for others.
    for visit in 1..=2 {
        assert!(!world.pipeline.has_cached(&filling()).await.unwrap());
        let report = world.pipeline.fetch(filling()).await.unwrap();
        assert!(report.cached.is_none());
        assert_eq!(world.script.seen().len(), visit);
        assert_eq!(
            world.backend.cache_entry("k1").unwrap().result_id,
            report.result
        );
    }
    let served = world.pipeline.fetch(cached(url, "k1", 30)).await.unwrap();
    assert!(served.cached.is_some());
    assert_eq!(world.script.seen().len(), 2);

    // What a session sees depends on its cookies: it neither reads the
    // cache nor fills it.
    let session = SessionId::new("a-session").unwrap();
    let in_session = |key: &str| FetchOrder {
        session: Some(session.clone()),
        ..cached(url, key, 30)
    };
    assert!(!world.pipeline.has_cached(&in_session("k1")).await.unwrap());
    let own = world.pipeline.fetch(in_session("k1")).await.unwrap();
    assert!(own.cached.is_none());
    assert_eq!(world.script.seen().len(), 3);
    world.pipeline.fetch(in_session("k9")).await.unwrap();
    assert!(world.backend.cache_entry("k9").is_none());
    assert_ne!(
        world.backend.cache_entry("k1").unwrap().result_id,
        own.result
    );
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn what_a_request_asks_for_is_read_from_the_document_its_visit_captured() {
    let mut world = World::new(3, DEFAULT_DOMAINS);
    let page = "https://news.example/budget";
    let extracting = |options| FetchOrder {
        extract: options,
        ..get(page)
    };

    // Asked for nothing, nothing is read.
    world.script.then(document(page, &article(200)));
    let plain = world.pipeline.fetch(extracting(None)).await.unwrap();
    assert!(plain.extract.is_none() && plain.page.is_none());

    // The extraction is of the document the browser had, relative to where
    // it was loaded from; it is stored with the result, and one visit
    // serves it.
    world.script.then(document(page, &article(200)));
    let report = world
        .pipeline
        .fetch(extracting(Some(Options::default())))
        .await
        .unwrap();
    assert_eq!(report.attempts.len(), 1);
    let extract = report.extract.as_ref().unwrap();
    assert_eq!(extract["title"], "The budget");
    assert_eq!(extract["description"], "What the council agreed on.");
    assert_eq!(extract["lang"], "en");
    assert_eq!(extract["word_count"], 208);
    assert_eq!(extract["thin_content"], false);
    assert_eq!(extract["headings"][0]["text"], "The budget");
    let text = extract["text"].as_str().unwrap();
    assert!(text.contains("the plan for next year") && !text.contains("Imprint"));
    assert!(
        extract["markdown"]
            .as_str()
            .unwrap()
            .contains("[the plan for next year](https://news.example/budget/2027)")
    );
    let links: Vec<&str> = extract["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|link| link["url"].as_str().unwrap())
        .collect();
    assert_eq!(
        links,
        [
            "https://news.example/",
            "https://news.example/news",
            "https://news.example/budget/2027"
        ]
    );
    let stored = world.backend.records().last().cloned().unwrap();
    assert_eq!(stored.extract.as_ref(), Some(extract));
    assert_eq!(world.script.seen().len(), 2);

    // The options are the request's: without markdown and links neither
    // is written.
    world.script.then(document(page, &article(200)));
    let lean = world
        .pipeline
        .fetch(extracting(Some(Options {
            markdown: false,
            links: false,
            thin_words: 500,
        })))
        .await
        .unwrap();
    let extract = lean.extract.unwrap();
    assert!(extract["markdown"].is_null() && extract["links"].is_null());
    // And so is what counts as thin.
    assert_eq!(extract["thin_content"], true);

    // Nothing is extracted from a page that was not got, or from a
    // response that is no HTML document.
    world.script.then(blocked(Outcome::Forbidden, 1.0));
    let wall = world
        .pipeline
        .fetch(FetchOrder {
            max_retries: Some(0),
            ..extracting(Some(Options::default()))
        })
        .await
        .unwrap();
    assert_eq!(wall.outcome, Outcome::Forbidden);
    assert!(wall.extract.is_none());
    world.script.then(file(
        "https://news.example/data.txt",
        200,
        &"plain words ".repeat(200),
    ));
    let text = world
        .pipeline
        .fetch(FetchOrder {
            extract: Some(Options::default()),
            ..get("https://news.example/data.txt")
        })
        .await
        .unwrap();
    assert_eq!(text.outcome, Outcome::Ok);
    assert!(text.extract.is_none());

    // A crawl reads its links and its text from the same tree, and gets
    // them for a page that asked for no extraction too.
    world.script.then(document(page, &article(200)));
    let crawled = world
        .pipeline
        .fetch(FetchOrder {
            read_page: true,
            ..extracting(Some(Options::default()))
        })
        .await
        .unwrap();
    let read = crawled.page.unwrap();
    assert_eq!(read.title, "The budget");
    assert_eq!(read.links.len(), 3);
    assert!(read.main_text.contains("the plan for next year"));
    assert!(crawled.extract.is_some());
    world.script.then(document(page, &article(200)));
    let only_read = world
        .pipeline
        .fetch(FetchOrder {
            read_page: true,
            ..get(page)
        })
        .await
        .unwrap();
    assert!(only_read.page.is_some() && only_read.extract.is_none());
    world.shut_down().await;
}
