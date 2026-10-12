//! Crawls (spec §13, §16.6): `POST /v1/jobs` with `type: crawl` and what
//! follows from it, over the router of `kage serve` and the scheduler's
//! lab, with the job worker running as `kage serve` runs it. The site is
//! served to the http path by the lab's site and to the browser path by
//! the mock sidecar, which keeps a tab per context and says how it got to
//! every page.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::sched::lab::LENIENT_FAST;
use common::{Api, request};
use kage_core::config::Config;
use kage_core::domain::Scope;
use kage_core::duration::HumanDuration;
use kage_core::ids::RequestId;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

/// Every request goes to the browser.
const BROWSER_FAST: &str = "defaults: { render: browser, pacing: fast, active_hours: ignore, min_dwell: 500ms }\ndomains: {}\n";

/// A worker that looks often: a test does not wait seconds for it.
fn quick(config: &mut Config) {
    config.jobs.poll_interval = HumanDuration::new(Duration::from_millis(50));
    config.jobs.lease = HumanDuration::new(Duration::from_secs(3));
}

/// One page of the test site.
#[derive(Debug, Clone)]
struct Leaf {
    path: &'static str,
    /// What the page says: 400 words nobody else says, unless told.
    text: String,
    /// Links in the text, in order.
    links: Vec<&'static str>,
    /// Links in the navigation.
    menu: Vec<&'static str>,
    /// Links no reader can see.
    hidden: Vec<&'static str>,
}

/// Words that are the page's own: two pages made of them are far apart.
fn words(name: &str) -> String {
    (0..400)
        .map(|n| format!("{name}{n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn leaf(path: &'static str, links: &[&'static str]) -> Leaf {
    Leaf {
        path,
        text: words(&path.replace('/', "x")),
        links: links.to_vec(),
        menu: Vec::new(),
        hidden: Vec::new(),
    }
}

/// The address a link of the site leads to.
fn address(href: &str) -> String {
    if href.starts_with("http") {
        href.to_owned()
    } else {
        Api::url(href)
    }
}

impl Leaf {
    fn html(&self) -> String {
        let anchors = |links: &[&str], style: &str| {
            links
                .iter()
                .map(|href| format!("<p><a href=\"{href}\"{style}>to {href}</a></p>"))
                .collect::<Vec<_>>()
                .concat()
        };
        format!(
            "<!DOCTYPE html><html><head><title>{path}</title></head><body>\
             <nav>{menu}</nav><main><p>{text}</p>{links}{hidden}</main>\
             </body></html>",
            path = self.path,
            menu = anchors(&self.menu, ""),
            text = self.text,
            links = anchors(&self.links, ""),
            hidden = anchors(&self.hidden, " style=\"display:none\""),
        )
    }

    /// The links as a browser brings them back: the menu comes first in
    /// the document.
    fn seen(&self) -> Value {
        let link = |href: &&str, visible: bool, furniture: bool| {
            json!({
                "url": address(href), "text": format!("to {href}"), "rel": [],
                "visible": visible, "furniture": furniture,
            })
        };
        let mut links: Vec<Value> = self
            .menu
            .iter()
            .map(|href| link(href, true, true))
            .collect();
        links.extend(self.links.iter().map(|href| link(href, true, false)));
        links.extend(self.hidden.iter().map(|href| link(href, false, false)));
        json!(links)
    }
}

/// A small site: a home page, a chain four links deep, a page that says
/// what another says, one that nearly does, a page only the menu leads to,
/// a link nobody sees, a file and a link that leaves.
fn small_site() -> Vec<Leaf> {
    let mut home = leaf(
        "/",
        &[
            "/a",
            "/b",
            "/files/report.pdf",
            "http://elsewhere.test/x",
            "/a#top",
        ],
    );
    home.menu = vec!["/about"];
    home.hidden = vec!["/trap"];
    let page_a = leaf("/a", &["/a/1", "/b"]);
    let page_b = leaf("/b", &["/dup", "/near"]);
    // The same page under another address: what it says and what it
    // links to in its text are the same; only its menu has one link more.
    let mut dup = leaf("/dup", &["/a/1", "/b"]);
    dup.text.clone_from(&page_a.text);
    dup.menu = vec!["/only-from-dup"];
    // The same page with one word of 400 changed.
    let mut near = leaf("/near", &["/dup", "/near"]);
    near.text = page_b.text.replacen("xb75 ", "changed ", 1);
    near.menu = vec!["/c"];
    vec![
        home,
        page_a,
        leaf("/a/1", &["/a/2"]),
        leaf("/a/2", &["/a/3"]),
        leaf("/a/3", &["/a/4"]),
        leaf("/a/4", &[]),
        page_b,
        dup,
        near,
        leaf("/c", &[]),
        leaf("/about", &[]),
        leaf("/trap", &[]),
        leaf("/only-from-dup", &[]),
    ]
}

/// Serves a site to the http path.
async fn serve(api: &Api, site: &[Leaf], delay: Duration) {
    for page in site {
        Mock::given(method("GET"))
            .and(path(page.path))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(page.html(), "text/html; charset=utf-8")
                    .set_delay(delay),
            )
            .mount(&api.lab.site)
            .await;
    }
}

/// Serves a site to the browsers of the lab.
fn show(api: &Api, site: &[Leaf]) {
    api.lab.chromium.with(|sidecar| {
        for page in site {
            sidecar.pages.insert(
                Api::url(page.path),
                json!({ "title": page.path, "dom_html": page.html(), "links": page.seen() }),
            );
        }
    });
}

/// The paths the site was asked for, in order.
async fn asked(api: &Api) -> Vec<String> {
    api.lab
        .site
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|request| request.url.path().to_owned())
        .collect()
}

fn ended(job: &Value) -> bool {
    matches!(
        job["state"].as_str(),
        Some("succeeded" | "partial" | "failed" | "cancelled")
    )
}

/// The path of an item's page.
fn path_of(item: &Value) -> String {
    item["url"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches(&Api::url(""))
        .to_owned()
}

/// A member of each item's `crawl` block, by the path of its page.
fn by_path(items: &[Value], member: &str) -> BTreeMap<String, Value> {
    items
        .iter()
        .map(|item| (path_of(item), item["crawl"][member].clone()))
        .collect()
}

/// The paths of the pages of a job's items, in the job's order.
fn paths(items: &[Value]) -> Vec<String> {
    items.iter().map(path_of).collect()
}

/// The personas that made the attempts of a request.
async fn personas_of(api: &Api, request: &str) -> Vec<String> {
    let request: RequestId = request.parse().unwrap();
    api.lab
        .store()
        .attempts()
        .list_for_request(request)
        .await
        .unwrap()
        .into_iter()
        .map(|attempt| attempt.persona_id.to_string())
        .collect()
}

async fn count(api: &Api, table: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
        .fetch_one(api.lab.store().pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_crawl_reads_a_site_along_its_links_as_far_as_it_may() {
    let mut api = Api::start_with(2, 149, LENIENT_FAST, quick).await;
    serve(&api, &small_site(), Duration::ZERO).await;
    let token = api.key(&[Scope::Jobs, Scope::Crawl]).await;
    let body = json!({
        "type": "crawl",
        "seeds": [Api::url("/")],
        "max_depth": 4,
        "defaults": { "render": "http", "include": ["headers"] },
    });

    let reply = api.post(&token, "/v1/jobs", &body).await;

    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
    let job = reply.json();
    let id = job["id"].as_str().unwrap().to_owned();
    assert_eq!(job["type"], "crawl");
    assert_eq!(job["state"], "queued");
    assert_eq!(job["visitors"], 1);
    // A crawl has no items yet: it grows by the pages it asks for.
    assert_eq!(job["items"]["total"], 0);
    assert_eq!(job["crawl"]["max_pages"], 100);
    assert_eq!(job["crawl"]["max_depth"], 4);
    assert_eq!(job["crawl"]["ended"], Value::Null);
    assert_eq!(job["crawl"]["sitemaps"], json!([]));

    let work = api.work();
    let done = api.job_until(&token, &id, ended).await;
    assert_eq!(done["state"], "succeeded", "{done}");

    // The pages in the order a reader goes: the links of the open page,
    // those of the text first; a page read before when the open one has
    // none left; an address typed when the tab holds nothing more.
    let items = api.job_items(&token, &id).await;
    let read = paths(&items);
    assert_eq!(
        read,
        [
            "/", "/a", "/a/1", "/a/2", "/a/3", "/b", "/dup", "/near", "/c", "/about"
        ]
    );
    assert_eq!(asked(&api).await, read);
    for (item, seq) in items.iter().zip(0_u64..) {
        assert_eq!(item["seq"], seq, "{item}");
        assert_eq!(item["state"], "succeeded", "{item}");
        assert_eq!(item["outcome"], "ok", "{item}");
        assert_eq!(item["result"]["status"], 200, "{item}");
        assert_eq!(item["step"], Value::Null);
    }
    // The depth is the least number of links from the seed.
    let depths = by_path(&items, "depth");
    for (page, depth) in [
        ("/", 0),
        ("/a", 1),
        ("/b", 1),
        ("/about", 1),
        ("/a/1", 2),
        ("/dup", 2),
        ("/near", 2),
        ("/a/2", 3),
        ("/c", 3),
        ("/a/3", 4),
    ] {
        assert_eq!(depths[page], depth, "{page}");
    }
    let found_on = by_path(&items, "found_on");
    assert_eq!(found_on["/"], Value::Null);
    assert_eq!(found_on["/a"], Api::url("/"));
    assert_eq!(found_on["/a/2"], Api::url("/a/1"));
    assert_eq!(found_on["/about"], Api::url("/"));
    let found_by = by_path(&items, "found_by");
    assert_eq!(found_by["/"], "seed");
    assert_eq!(found_by["/c"], "link");
    // The http path has no tab: a page that a link led to names the page
    // that holds the link, and a page nothing in reach leads to names none.
    let reached = by_path(&items, "reached");
    assert_eq!(reached["/"], "typed");
    assert_eq!(reached["/a"], "referer");
    assert_eq!(reached["/b"], "referer");
    assert_eq!(reached["/about"], "typed");
    let sent = api.lab.site.received_requests().await.unwrap();
    let referer = |page: &str| {
        sent.iter()
            .find(|request| request.url.path() == page)
            .and_then(|request| request.headers.get("referer"))
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    assert_eq!(referer("/"), None);
    assert_eq!(referer("/a/1"), Some(Api::url("/a")));
    // Back to the page that holds the link, not the one read last.
    assert_eq!(referer("/b"), Some(Api::url("/a")));
    assert_eq!(referer("/near"), Some(Api::url("/b")));
    assert_eq!(referer("/about"), None);

    // What says the same is flagged and its own links are not followed;
    // what nearly says the same is flagged and read on from.
    let duplicate_of = by_path(&items, "duplicate_of");
    assert_eq!(duplicate_of["/dup"], Api::url("/a"));
    assert_eq!(duplicate_of["/a"], Value::Null);
    assert!(!read.contains(&"/only-from-dup".to_owned()));
    let near = by_path(&items, "near_duplicate_of");
    assert_eq!(near["/near"]["url"], Api::url("/b"));
    assert!(near["/near"]["bits"].as_u64().unwrap() <= 6, "{near:?}");
    assert_eq!(near["/c"], Value::Null);
    let hashes = by_path(&items, "content_hash");
    assert_eq!(hashes["/dup"], hashes["/a"]);
    assert_ne!(hashes["/near"], hashes["/b"]);
    assert_eq!(hashes["/a"].as_str().unwrap().len(), 64);
    let links = by_path(&items, "links");
    // Of the home page: /a, /b and /about are followed (the second link
    // to /a is the same address); nobody sees /trap; the file and the
    // other site are known and not read.
    assert_eq!(links["/"], json!({ "found": 5, "new": 5 }));
    assert_eq!(links["/a"], json!({ "found": 2, "new": 1 }));
    assert_eq!(links["/dup"], Value::Null);

    let crawl = &done["crawl"];
    assert_eq!(crawl["discovered"], 14, "{crawl}");
    assert_eq!(crawl["fetched"], 10);
    assert_eq!(
        (&crawl["waiting"], &crawl["fetching"], &crawl["failed"]),
        (&json!(0), &json!(0), &json!(0))
    );
    assert_eq!(
        crawl["skipped"],
        json!({ "hidden": 1, "not_a_page": 1, "off_site": 1, "too_deep": 1 })
    );
    assert_eq!(crawl["duplicates"], 1);
    assert_eq!(crawl["near_duplicates"], 1);
    assert_eq!(crawl["depth_reached"], 4);
    assert_eq!(crawl["pages"], 10);
    assert_eq!(
        crawl["reached"],
        json!({ "typed": 2, "click": 0, "back_click": 0, "referer": 8 })
    );
    assert_eq!(crawl["ended"], "completed");
    assert_eq!(
        done["items"],
        json!({ "total": 10, "succeeded": 10, "failed": 0, "cancelled": 0, "pending": 0 })
    );
    // One visitor read the site, and nothing of who it was is shown.
    let mut visitors = BTreeSet::new();
    for item in &items {
        visitors.extend(personas_of(&api, item["request_id"].as_str().unwrap()).await);
    }
    assert_eq!(visitors.len(), 1, "{visitors:?}");
    assert_eq!(crawl.get("slots"), None);
    assert_eq!(count(&api, "crawl_frontier").await, 14);

    // A page the site has no more is asked for like any other and is no
    // page of the crawl: nothing is taken from what an error page says.
    Mock::given(method("GET"))
        .and(path("/gone"))
        .respond_with(ResponseTemplate::new(404).set_body_raw(
            leaf("/gone", &["/behind-the-error"]).html(),
            "text/html; charset=utf-8",
        ))
        .mount(&api.lab.site)
        .await;
    serve(&api, &[leaf("/start", &["/gone", "/c"])], Duration::ZERO).await;
    let reply = api
        .post(
            &token,
            "/v1/jobs",
            &json!({
                "type": "crawl", "seeds": [Api::url("/start")], "defaults": { "render": "http" },
            }),
        )
        .await;
    let id = reply.json()["id"].as_str().unwrap().to_owned();
    let done = api.job_until(&token, &id, ended).await;
    let items = api.job_items(&token, &id).await;
    assert_eq!(paths(&items), ["/start", "/gone", "/c"]);
    assert_eq!(items[1]["state"], "succeeded", "{}", items[1]);
    assert_eq!(items[1]["result"]["status"], 404);
    assert_eq!(items[1]["crawl"]["reached"], "referer");
    assert_eq!(items[1]["crawl"]["links"], Value::Null);
    assert_eq!(done["crawl"]["failed"], 1, "{done}");
    assert_eq!(done["crawl"]["fetched"], 2);
    assert_eq!(done["crawl"]["discovered"], 3);
    assert_eq!(done["state"], "succeeded");

    // A crawl that may go less deep leaves what is deeper: counted, and
    // never asked for.
    let before = asked(&api).await.len();
    let shallow = json!({
        "type": "crawl", "seeds": [Api::url("/")], "max_depth": 1, "max_pages": 3,
        "defaults": { "render": "http" },
    });
    let reply = api.post(&token, "/v1/jobs", &shallow).await;
    let id = reply.json()["id"].as_str().unwrap().to_owned();
    let done = api.job_until(&token, &id, ended).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    let read = asked(&api).await[before..].to_vec();
    // No more pages than it may ask for: the fourth waits for ever.
    assert_eq!(read, ["/", "/a", "/b"]);
    let crawl = &done["crawl"];
    assert_eq!(crawl["ended"], "max_pages");
    assert_eq!(crawl["pages"], 3);
    assert_eq!(crawl["waiting"], 1);
    assert_eq!(crawl["skipped"]["too_deep"], 3, "{crawl}");
    work.stop().await;
    api.shut_down().await;
}

#[tokio::test]
async fn in_a_browser_a_crawl_clicks_its_way_and_never_follows_what_nobody_sees() {
    let mut api = Api::start_with(2, 149, BROWSER_FAST, quick).await;
    let site = small_site();
    show(&api, &site);
    // The home page is longer than the persona's patience with it.
    api.lab.chromium.with(|sidecar| {
        sidecar
            .scrolls
            .insert(Api::url("/"), (0.23, "patience".to_owned()));
    });
    let token = api.key(&[Scope::Jobs, Scope::Crawl]).await;
    let body = json!({ "type": "crawl", "seeds": [Api::url("/")], "max_depth": 4 });

    let reply = api.post(&token, "/v1/jobs", &body).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
    let id = reply.json()["id"].as_str().unwrap().to_owned();
    let work = api.work();
    let done = api.job_until(&token, &id, ended).await;
    assert_eq!(done["state"], "succeeded", "{done}");

    let items = api.job_items(&token, &id).await;
    assert_eq!(
        paths(&items),
        [
            "/", "/a", "/a/1", "/a/2", "/a/3", "/b", "/dup", "/near", "/c", "/about"
        ]
    );
    // How every page was reached, as the browser said it did it: the
    // seed typed, a link of the open page clicked, a link of a page read
    // before clicked after going back to it, and what the tab no longer
    // leads to typed.
    let reached = by_path(&items, "reached");
    let backs = by_path(&items, "backs");
    for (page, way, back) in [
        ("/", "typed", Value::Null),
        ("/a", "click", json!(0)),
        ("/a/1", "click", json!(0)),
        ("/a/3", "click", json!(0)),
        ("/b", "back_click", json!(3)),
        ("/dup", "click", json!(0)),
        ("/near", "back_click", json!(1)),
        ("/c", "click", json!(0)),
        ("/about", "typed", Value::Null),
    ] {
        assert_eq!(reached[page], way, "{page}");
        assert_eq!(backs[page], back, "{page}");
    }
    assert_eq!(
        done["crawl"]["reached"],
        json!({ "typed": 2, "click": 6, "back_click": 2, "referer": 0 })
    );
    assert_eq!(done["crawl"]["skipped"]["hidden"], 1);

    // What the sidecar was asked: the links of every page, a click from
    // the page the persona has been reading, and a patience of its own
    // for every page.
    let navigated = api.lab.chromium.with(|sidecar| sidecar.navigated.clone());
    assert_eq!(navigated.len(), 10);
    assert!(
        navigated
            .iter()
            .all(|asked| asked["capture"]["links"] == true)
    );
    assert_eq!(
        navigated[0]["behavior"]["entry"],
        json!({ "strategy": "direct" })
    );
    assert_eq!(
        navigated[1]["behavior"]["entry"],
        json!({
            "strategy": "link", "from_url": Api::url("/"), "from_open_page": true,
            "max_backs": 3, "fallback": "none",
        })
    );
    assert_eq!(navigated[5]["url"], Api::url("/b"));
    assert_eq!(
        navigated[5]["behavior"]["entry"]["from_url"],
        Api::url("/a")
    );
    let patiences: BTreeSet<u64> = navigated
        .iter()
        .map(|asked| asked["behavior"]["patience_ms"].as_u64().unwrap())
        .collect();
    assert_eq!(patiences.len(), 10, "{patiences:?}");
    assert!(
        patiences
            .iter()
            .all(|patience| (4_800..=300_000).contains(patience))
    );
    // One persona in one window read all of it.
    let contexts: BTreeSet<&str> = navigated
        .iter()
        .map(|asked| asked["context_id"].as_str().unwrap())
        .collect();
    assert_eq!(contexts.len(), 1);

    // A page that was longer than the persona's patience says how far it
    // came and what ended it; the patience is kept with the attempt.
    let scroll = by_path(&items, "scroll");
    assert_eq!(scroll["/"]["ended_by"], "patience");
    assert_eq!(scroll["/"]["reached"], 0.23);
    assert_eq!(
        scroll["/"]["patience_ms"],
        navigated[0]["behavior"]["patience_ms"]
    );
    assert_eq!(scroll["/a"]["ended_by"], "depth");
    let request: RequestId = items[0]["request_id"].as_str().unwrap().parse().unwrap();
    let attempts = api
        .lab
        .store()
        .attempts()
        .list_for_request(request)
        .await
        .unwrap();
    assert_eq!(
        attempts[0].patience_ms,
        navigated[0]["behavior"]["patience_ms"].as_u64()
    );
    work.stop().await;
    api.shut_down().await;
}

/// A gzipped sitemap that names `/packed/only` of the lab's site, as
/// base64: what Firefox hands over of a file it saved.
const PACKED: &str = "H4sIAAAAAAACCjWOTQ6CMBSEr0K6NfQ9MBokpew8AR6gKQ0Q+xdeFby9AXEzi8n3TUa0q7PZ28w0Bd+wgiPLjNehn/zQsEd3zyvWSvGaLZmUrc56atiYUqwBlmXhNCXjVCQe5gFIj8YpgqME5De2u1LYoOWh0Rgif6rB8GQoQVT6aXoI3n4EbJiwipILvSyxvOYF5njpsKrPWCOecEsBf0TAvg6/f/ILsWnS0s4AAAA=";

/// A sitemap as a browser brings it back: the file as the server sent it.
fn raw(body: &str) -> Value {
    json!({
        "raw_body": { "data": body, "encoding": "utf8", "bytes": body.len(), "truncated": false },
        "dom_html": "<html><body><pre>a sitemap</pre></body></html>",
        "title": "",
    })
}

#[tokio::test]
async fn sitemaps_are_read_by_a_scout_and_never_by_the_sites_reader() {
    let mut api = Api::start_with(3, 149, BROWSER_FAST, quick).await;
    let site = vec![
        leaf("/", &["/a"]),
        leaf("/a", &[]),
        leaf("/only", &[]),
        leaf("/deep/only", &[]),
        leaf("/packed/only", &[]),
    ];
    show(&api, &site);
    api.lab.chromium.with(|sidecar| {
        sidecar.pages.insert(
            Api::url("/sitemap.xml"),
            raw(&format!(
                "<?xml version=\"1.0\"?><sitemapindex xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\
                 <sitemap><loc>{}</loc></sitemap><sitemap><loc>{}</loc></sitemap>\
                 <sitemap><loc>{}</loc></sitemap>\
                 <sitemap><loc>http://elsewhere.test/sitemap.xml</loc></sitemap></sitemapindex>",
                Api::url("/sitemaps/pages.xml"),
                Api::url("/sitemaps/more.xml.gz"),
                Api::url("/sitemaps/kept.xml.gz"),
            )),
        );
        // A browser saves a gzipped file and says the navigation was
        // aborted. Chrome hands over nothing of it; Firefox hands over the
        // bytes (measured in both), and they are a sitemap.
        let saved = json!({ "code": "aborted", "message": "page.goto: Download is starting", "net_error": null });
        sidecar.pages.insert(
            Api::url("/sitemaps/more.xml.gz"),
            json!({ "error": saved, "raw_body": null, "dom_html": null }),
        );
        sidecar.pages.insert(
            Api::url("/sitemaps/kept.xml.gz"),
            json!({
                "error": saved,
                "dom_html": null,
                "raw_body": { "data": PACKED, "encoding": "base64", "bytes": 176, "truncated": false },
            }),
        );
        sidecar.pages.insert(
            Api::url("/sitemaps/pages.xml"),
            raw(&format!(
                "<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\
                 <url><loc>{}</loc><lastmod>2026-09-30</lastmod></url>\
                 <url><loc>{}</loc></url><url><loc>{}</loc></url>\
                 <url><loc>http://elsewhere.test/x</loc></url></urlset>",
                Api::url("/only"),
                Api::url("/deep/only"),
                Api::url("/a"),
            )),
        );
    });
    let token = api.key(&[Scope::Jobs, Scope::Crawl]).await;
    let body = json!({
        "type": "crawl", "seeds": [Api::url("/")], "use_sitemaps": true, "max_depth": 1,
    });

    let reply = api.post(&token, "/v1/jobs", &body).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
    let id = reply.json()["id"].as_str().unwrap().to_owned();
    let work = api.work();
    let done = api.job_until(&token, &id, ended).await;
    assert_eq!(done["state"], "succeeded", "{done}");

    // The sitemaps were read first, the index and what it names of the
    // same site; the pages only a sitemap names are places to start from.
    let navigated = api.lab.chromium.with(|sidecar| sidecar.navigated.clone());
    let order: Vec<&str> = navigated
        .iter()
        .map(|asked| {
            asked["url"]
                .as_str()
                .unwrap()
                .trim_start_matches(&Api::url(""))
        })
        .collect();
    // Each file was asked for once: a file that was saved and not shown
    // is not asked for again from another address.
    assert_eq!(
        order[..4],
        [
            "/sitemap.xml",
            "/sitemaps/pages.xml",
            "/sitemaps/more.xml.gz",
            "/sitemaps/kept.xml.gz"
        ]
    );
    let mut pages: Vec<&str> = order[4..].to_vec();
    pages.sort_unstable();
    assert_eq!(pages, ["/", "/a", "/deep/only", "/only", "/packed/only"]);
    assert_eq!(
        done["crawl"]["sitemaps"],
        json!([
            { "site": "kage.test", "url": Api::url("/sitemap.xml"), "state": "index", "urls": 3 },
            { "site": "kage.test", "url": Api::url("/sitemaps/pages.xml"), "state": "read", "urls": 4 },
            { "site": "kage.test", "url": Api::url("/sitemaps/more.xml.gz"), "state": "sitemap_unreadable", "urls": 0 },
            { "site": "kage.test", "url": Api::url("/sitemaps/kept.xml.gz"), "state": "read", "urls": 1 },
        ])
    );
    let items = api.job_items(&token, &id).await;
    let found_by = by_path(&items, "found_by");
    assert_eq!(found_by["/only"], "sitemap");
    assert_eq!(found_by["/deep/only"], "sitemap");
    assert_eq!(found_by["/"], "seed");
    // The sitemap named it before the home page was read.
    assert_eq!(found_by["/a"], "sitemap");
    let depths = by_path(&items, "depth");
    assert_eq!(
        (&depths["/only"], &depths["/deep/only"]),
        (&json!(0), &json!(0))
    );
    assert_eq!(by_path(&items, "lastmod")["/only"], "2026-09-30T00:00:00Z");
    assert_eq!(found_by["/packed/only"], "sitemap");
    assert_eq!(
        by_path(&items, "lastmod")["/packed/only"],
        "2026-10-05T08:30:00Z"
    );
    assert_eq!(done["crawl"]["skipped"]["off_site"], 1);
    assert_eq!(done["items"]["total"], 5);

    // The scout asked for the sitemaps, typed and in its own window; the
    // reader asked for the pages, and neither ever did the other's part.
    let window = |asked: &Value| asked["context_id"].as_str().unwrap().to_owned();
    let scouts: BTreeSet<String> = navigated[..4].iter().map(window).collect();
    let readers: BTreeSet<String> = navigated[4..].iter().map(window).collect();
    assert!(scouts.is_disjoint(&readers), "{scouts:?} {readers:?}");
    assert_eq!(readers.len(), 1);
    for asked in &navigated[..4] {
        assert_eq!(asked["behavior"]["entry"]["strategy"], "direct");
        assert_eq!(asked["capture"]["raw"], true);
    }

    // Where nobody but the reader could ask, nobody asks: a reader does
    // not open /sitemap.xml.
    work.stop().await;
    api.shut_down().await;
    let mut api = Api::start_with(1, 149, BROWSER_FAST, quick).await;
    show(&api, &site);
    let token = api.key(&[Scope::Jobs, Scope::Crawl]).await;
    let reply = api.post(&token, "/v1/jobs", &body).await;
    let id = reply.json()["id"].as_str().unwrap().to_owned();
    let work = api.work();
    let done = api.job_until(&token, &id, ended).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(
        done["crawl"]["sitemaps"],
        json!([{ "site": "kage.test", "url": Api::url("/sitemap.xml"), "state": "no_scout", "urls": 0 }])
    );
    let navigated = api.lab.chromium.with(|sidecar| sidecar.navigated.clone());
    assert!(
        navigated
            .iter()
            .all(|asked| !asked["url"].as_str().unwrap().contains("sitemap")),
        "{navigated:?}"
    );
    assert_eq!(done["items"]["total"], 2);
    work.stop().await;
    api.shut_down().await;
}

#[tokio::test]
async fn a_crawl_is_accepted_whole_or_not_at_all_and_needs_its_scope() {
    let mut api = Api::start_with(1, 149, LENIENT_FAST, quick).await;
    let jobs_only = api.key(&[Scope::Jobs]).await;
    let token = api.key(&[Scope::Jobs, Scope::Crawl]).await;
    let body = json!({ "type": "crawl", "seeds": [Api::url("/")] });

    // A crawl asks a site for pages nobody named: a scope of its own.
    api.post(&jobs_only, "/v1/jobs", &body)
        .await
        .problem(StatusCode::FORBIDDEN, "scope_missing");
    for (refused, code, field) in [
        (
            json!({ "type": "crawl", "seeds": [] }),
            "validation_failed",
            "seeds",
        ),
        (
            json!({ "type": "crawl", "seeds": [Api::url("/")], "max_depth": 99 }),
            "validation_failed",
            "max_depth",
        ),
        (
            json!({ "type": "crawl", "seeds": [Api::url("/")], "allow": ["("] }),
            "validation_failed",
            "allow",
        ),
        (
            json!({ "type": "crawl", "seeds": [Api::url("/")], "visitors": 11 }),
            "validation_failed",
            "visitors",
        ),
        (
            json!({ "type": "crawl", "seeds": [Api::url("/"), Api::url("/login")], "deny": ["/login"] }),
            "validation_failed",
            "seeds",
        ),
    ] {
        let problem = api
            .post(&token, "/v1/jobs", &refused)
            .await
            .problem(StatusCode::UNPROCESSABLE_ENTITY, code);
        assert_eq!(problem["field"], field, "{problem}");
    }
    let denied = api
        .post(
            &token,
            "/v1/jobs",
            &json!({ "type": "crawl", "seeds": [Api::url("/"), "http://169.254.169.254/latest"] }),
        )
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "ssrf_denied");
    assert_eq!(denied["item"], 1);
    api.post(
        &token,
        "/v1/jobs",
        &json!({ "type": "crawl", "seeds": [Api::url("/")], "personas": 2 }),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed");
    assert_eq!(count(&api, "jobs").await, 0);
    assert_eq!(count(&api, "crawl_frontier").await, 0);

    // The same crawl under the same key is the same job.
    let keyed = |body: &Value| {
        let mut request = request(Method::POST, "/v1/jobs", Some(&token), Some(body));
        request
            .headers_mut()
            .insert("idempotency-key", "crawl-1".parse().unwrap());
        request
    };
    let first = api.send(keyed(&body)).await;
    assert_eq!(first.status, StatusCode::CREATED, "{:?}", first.json());
    let again = api.send(keyed(&body)).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.header("idempotency-replayed"), Some("true"));
    assert_eq!(again.json()["id"], first.json()["id"]);
    api.send(keyed(
        &json!({ "type": "crawl", "seeds": [Api::url("/other")] }),
    ))
    .await
    .problem(StatusCode::CONFLICT, "idempotency_key_reused");
    assert_eq!(count(&api, "jobs").await, 1);
    api.shut_down().await;
}

#[tokio::test]
async fn a_cancel_stops_a_crawl_within_one_page_and_a_crawl_goes_on_after_a_restart() {
    let mut api = Api::start_with(2, 149, LENIENT_FAST, quick).await;
    // A chain of pages that each take a while.
    let chain: Vec<Leaf> = vec![
        leaf("/", &["/s/1"]),
        leaf("/s/1", &["/s/2"]),
        leaf("/s/2", &["/s/3"]),
        leaf("/s/3", &["/s/4"]),
        leaf("/s/4", &["/s/5"]),
        leaf("/s/5", &["/s/6"]),
        leaf("/s/6", &["/s/7"]),
        leaf("/s/7", &["/s/8"]),
        leaf("/s/8", &[]),
    ];
    serve(&api, &chain, Duration::from_millis(400)).await;
    let token = api.key(&[Scope::Jobs, Scope::Crawl]).await;
    let body = json!({
        "type": "crawl", "seeds": [Api::url("/")], "max_depth": 20,
        "defaults": { "render": "http" },
    });
    let cancel = |id: &str| format!("/v1/jobs/{id}/cancel");

    // Before anybody worked it: nothing is ever asked of the site.
    let reply = api.post(&token, "/v1/jobs", &body).await;
    let early = reply.json()["id"].as_str().unwrap().to_owned();
    let cancelled = api
        .call(Method::POST, &cancel(&early), Some(&token), None)
        .await;
    assert_eq!(cancelled.json()["cancel_requested"], true);
    let work = api.work();
    let done = api.job_until(&token, &early, ended).await;
    assert_eq!(done["state"], "cancelled", "{done}");
    assert_eq!(done["crawl"]["ended"], "cancelled");
    assert_eq!(done["items"]["total"], 0);
    assert_eq!(asked(&api).await.len(), 0);

    // While it reads: the page that is being read is finished, and not
    // one more is asked for.
    let reply = api.post(&token, "/v1/jobs", &body).await;
    let id = reply.json()["id"].as_str().unwrap().to_owned();
    api.job_until(&token, &id, |job| {
        job["items"]["succeeded"].as_u64() >= Some(2)
    })
    .await;
    let at_cancel = api
        .call(Method::POST, &cancel(&id), Some(&token), None)
        .await
        .json();
    let before = at_cancel["items"]["succeeded"].as_u64().unwrap();
    let done = api.job_until(&token, &id, ended).await;
    assert_eq!(done["state"], "cancelled", "{done}");
    let succeeded = done["items"]["succeeded"].as_u64().unwrap();
    assert!(
        succeeded <= before + 1,
        "{succeeded} pages ended, {before} had when the cancel came"
    );
    assert_eq!(done["items"]["pending"], 0);
    assert_eq!(done["crawl"]["ended"], "cancelled");
    assert_eq!(done["crawl"]["fetched"], succeeded);
    assert_eq!(done["crawl"]["fetching"], 0);
    assert!(done["crawl"]["waiting"].as_u64() >= Some(1), "{done}");
    // The site saw the pages that ended and none after them.
    assert_eq!(asked(&api).await.len() as u64, succeeded);
    work.stop().await;

    // A crawl whose worker stops goes on where it stands: no page is
    // asked for twice, none is left out.
    let before = asked(&api).await.len();
    let reply = api.post(&token, "/v1/jobs", &body).await;
    let id = reply.json()["id"].as_str().unwrap().to_owned();
    let work = api.work();
    api.job_until(&token, &id, |job| {
        job["items"]["succeeded"].as_u64() >= Some(3)
    })
    .await;
    work.stop().await;
    let stopped = api.get(&token, &format!("/v1/jobs/{id}")).await.json();
    assert!(!ended(&stopped), "{stopped}");
    let work = api.work();
    let done = api.job_until(&token, &id, ended).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(done["crawl"]["ended"], "completed");
    assert_eq!(done["items"]["total"], 9);
    let seen = asked(&api).await[before..].to_vec();
    assert_eq!(
        seen,
        [
            "/", "/s/1", "/s/2", "/s/3", "/s/4", "/s/5", "/s/6", "/s/7", "/s/8"
        ]
    );
    let items = api.job_items(&token, &id).await;
    assert_eq!(items.len(), 9);
    assert!(items.iter().all(|item| item["state"] == "succeeded"));
    work.stop().await;
    api.shut_down().await;
}

#[tokio::test]
async fn a_crawl_waits_out_what_holds_its_site_and_ends_at_its_deadline() {
    // A site that turns its reader away after the first page: the
    // breaker opens after two blocks and the crawl waits for it.
    let rules = "defaults: { render: http, pacing: fast, active_hours: ignore, retries: 1, \
                 breaker_threshold: 2, breaker_pause: 30m }\ndomains: {}\n";
    let mut api = Api::start_with(3, 149, rules, quick).await;
    serve(&api, &[leaf("/", &["/wall", "/after"])], Duration::ZERO).await;
    Mock::given(method("GET"))
        .and(path("/wall"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("server", "cloudflare")
                .insert_header("cf-ray", "8f1-FRA")
                .set_body_raw(
                    "<html><head><title>Attention Required! | Cloudflare</title></head><body>\
                     <h1>Sorry, you have been blocked</h1><p>Cloudflare Ray ID: 8f1</p></body></html>",
                    "text/html; charset=utf-8",
                ),
        )
        .mount(&api.lab.site)
        .await;
    let token = api.key(&[Scope::Jobs, Scope::Crawl]).await;
    let body = json!({
        "type": "crawl", "seeds": [Api::url("/")], "defaults": { "render": "http" },
    });
    let reply = api.post(&token, "/v1/jobs", &body).await;
    let id = reply.json()["id"].as_str().unwrap().to_owned();

    let work = api.work();
    // The page behind the wall failed on two personas; what is left is
    // not failed for it: the crawl waits for the site, in nobody's hands.
    let paused = api
        .job_until(&token, &id, |job| job["state"] == "paused")
        .await;
    assert_eq!(paused["paused"]["reason"], "domain_paused", "{paused}");
    assert!(paused["paused"]["until"].is_string());
    assert_eq!(paused["items"]["succeeded"], 1);
    assert_eq!(paused["items"]["failed"], 1);
    assert_eq!(paused["crawl"]["failed"], 1);
    assert_eq!(paused["crawl"]["ended"], Value::Null);
    let seen = asked(&api).await;
    assert_eq!(seen, ["/", "/wall", "/wall"]);
    // The page after the wall was asked of nobody.
    let items = api.job_items(&token, &id).await;
    assert_eq!(items[1]["state"], "failed");
    assert_eq!(items[1]["outcome"], "forbidden");
    assert_eq!(items[1]["crawl"]["reached"], Value::Null);

    // Past its deadline it ends with what it has, and says why it stopped.
    sqlx::query("UPDATE jobs SET deadline = now() - interval '1 second' WHERE id = $1::uuid")
        .bind(&id)
        .execute(api.lab.store().pool())
        .await
        .unwrap();
    let done = api.job_until(&token, &id, ended).await;
    assert_eq!(done["state"], "partial", "{done}");
    assert_eq!(done["crawl"]["ended"], "expired");
    assert_eq!(asked(&api).await.len(), 3);
    let items = api.job_items(&token, &id).await;
    let last = items.last().unwrap();
    if last["url"] == Api::url("/after") {
        assert_eq!(last["state"], "failed", "{last}");
        assert_eq!(last["error"], "expired: domain_paused");
    }
    work.stop().await;
    api.shut_down().await;
}
