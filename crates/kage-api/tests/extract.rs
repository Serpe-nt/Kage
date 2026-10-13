//! Extraction, `robots.txt` and the response cache through the API (spec
//! §14, §15): the router of `kage serve` over the scheduler's lab, with
//! the job worker running as `kage serve` runs it. The site is shown to
//! the browsers by the mock sidecar, which records every navigation it
//! was asked for and by which persona's window.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use axum::http::StatusCode;
use common::{Api, ROOMY};
use kage_core::config::Config;
use kage_core::domain::{KeyLimits, Scope};
use kage_core::duration::HumanDuration;
use serde_json::{Value, json};

/// Every request goes to the browser, and nobody asks for a `robots.txt`.
const BROWSER_FAST: &str = "defaults: { render: browser, pacing: fast, active_hours: ignore, min_dwell: 500ms }\ndomains: {}\n";

/// The same, with the site's `robots.txt` enforced.
const ENFORCED: &str = "defaults: { render: browser, pacing: fast, active_hours: ignore, min_dwell: 500ms }\ndomains:\n  kage.test: { robots: enforce }\n";

/// A worker that looks often: a test does not wait seconds for it.
fn quick(config: &mut Config) {
    config.jobs.poll_interval = HumanDuration::new(Duration::from_millis(50));
    config.jobs.lease = HumanDuration::new(Duration::from_secs(3));
}

/// An article of the site: a menu, a heading, 240 words and two links.
fn article(title: &str, links: &[&str]) -> String {
    let words = (0..240)
        .map(|n| format!("{}{n}", title.to_lowercase().replace(' ', "")))
        .collect::<Vec<_>>()
        .join(" ");
    let links = links
        .iter()
        .map(|href| format!("<p>See <a href=\"{href}\">the page at {href}</a>.</p>"))
        .collect::<Vec<_>>()
        .concat();
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><title>{title}</title>\
         <meta name=\"description\" content=\"About {title}.\">\
         <meta property=\"og:title\" content=\"{title}, shared\">\
         <script type=\"application/ld+json\">{{\"@type\":\"Article\",\
         \"datePublished\":\"2026-10-01T08:00:00+02:00\"}}</script></head>\
         <body><nav><a href=\"/\">Home</a></nav>\
         <article><h1>{title}</h1><p>{words}</p>{links}</article>\
         <footer>Imprint</footer></body></html>"
    )
}

/// Shows a page of the site to the lab's browsers.
fn show(api: &Api, path: &str, html: &str, links: &[&str]) {
    let seen: Vec<Value> = links
        .iter()
        .map(|href| {
            json!({
                "url": Api::url(href), "text": format!("the page at {href}"), "rel": [],
                "visible": true, "furniture": false,
            })
        })
        .collect();
    api.lab.chromium.with(|sidecar| {
        sidecar.pages.insert(
            Api::url(path),
            json!({ "title": path, "dom_html": html, "links": seen }),
        );
    });
}

/// Shows a file of the site as a browser brings one back: the body as the
/// server sent it.
fn show_file(api: &Api, path: &str, body: &str) {
    api.lab.chromium.with(|sidecar| {
        sidecar.pages.insert(
            Api::url(path),
            json!({
                "title": "",
                "headers": [{ "name": "content-type", "value": "text/plain" }],
                "dom_html": format!("<html><head></head><body><pre>{body}</pre></body></html>"),
                "raw_body": {
                    "data": body, "encoding": "utf8", "bytes": body.len(), "truncated": false,
                },
            }),
        );
    });
}

/// Every navigation the browsers were asked for so far: the path and the
/// window (one per persona) that made it.
fn navigated(api: &Api) -> Vec<(String, String)> {
    api.lab.chromium.with(|sidecar| {
        sidecar
            .navigated
            .iter()
            .map(|asked| {
                (
                    asked["url"]
                        .as_str()
                        .unwrap()
                        .trim_start_matches(&Api::url(""))
                        .to_owned(),
                    asked["context_id"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    })
}

fn paths(api: &Api) -> Vec<String> {
    navigated(api).into_iter().map(|(path, _)| path).collect()
}

async fn count(api: &Api, table: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
        .fetch_one(api.lab.store().pool())
        .await
        .unwrap()
}

fn ended(job: &Value) -> bool {
    matches!(
        job["state"].as_str(),
        Some("succeeded" | "partial" | "failed" | "cancelled")
    )
}

#[tokio::test]
async fn a_fetch_returns_what_the_page_says_and_the_cache_answers_the_second_time() {
    let mut api = Api::start(2, 155, BROWSER_FAST).await;
    // One page a day: what the cache answers is no page of the budget.
    let limits = KeyLimits {
        daily_page_budget: Some(1),
        ..ROOMY
    };
    let (_, token) = api.key_with(&[Scope::Fetch], limits).await;
    let other = api.key(&[Scope::Fetch]).await;
    show(
        &api,
        "/budget",
        &article("The budget", &["/plan", "http://elsewhere.test/x"]),
        &[],
    );
    let asking = json!({
        "url": Api::url("/budget"),
        "include": ["dom", "headers", "extract"],
        "behavior": { "scroll_depth": 0, "dwell_ms": 1000 },
        "cache_ttl_s": 600,
    });

    let reply = api.post(&token, "/v1/fetch", &asking).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    let first = reply.json();
    assert_eq!(first["outcome"], "ok");
    assert_eq!(
        (&first["cached"], &first["cached_at"]),
        (&json!(false), &Value::Null)
    );
    // Nobody asked for the site's robots.txt, and the result says so.
    assert_eq!(
        first["robots"],
        json!({ "fetched": false, "allowed": true, "matched_rule": null, "crawl_delay": null })
    );
    let extract = &first["extract"];
    assert_eq!(extract["title"], "The budget");
    assert_eq!(extract["description"], "About The budget.");
    assert_eq!(extract["lang"], "en");
    assert_eq!(extract["published_at"], "2026-10-01T06:00:00Z");
    assert_eq!(
        extract["opengraph"]["og:title"],
        json!(["The budget, shared"])
    );
    assert_eq!(extract["jsonld"][0]["@type"], "Article");
    assert_eq!(
        extract["headings"],
        json!([{ "level": 1, "text": "The budget", "id": null }])
    );
    assert_eq!(extract["thin_content"], false);
    assert!(extract["word_count"].as_u64().unwrap() > 240);
    let text = extract["text"].as_str().unwrap();
    assert!(text.contains("thebudget239") && !text.contains("Imprint"));
    assert!(
        extract["markdown"]
            .as_str()
            .unwrap()
            .contains(&format!("[the page at /plan]({})", Api::url("/plan")))
    );
    let links = extract["links"].as_array().unwrap();
    let leaving = links
        .iter()
        .find(|link| link["url"] == "http://elsewhere.test/x")
        .unwrap();
    assert_eq!(leaving["same_site"], false);
    assert!(
        links
            .iter()
            .any(|link| link["url"] == Api::url("/plan") && link["same_site"] == true)
    );
    assert_eq!(paths(&api), ["/budget"]);

    // The same request again: nothing is asked of the site, though the
    // key's day is used up. The answer is a result of its own that says
    // where its page came from, with what the first one returned.
    let reply = api.post(&token, "/v1/fetch", &asking).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    let second = reply.json();
    assert_eq!(paths(&api), ["/budget"]);
    assert_ne!(second["id"], first["id"]);
    assert_eq!(second["cached"], true);
    assert_eq!(second["cached_at"], first["created_at"]);
    assert_eq!(second["attempts"], json!([]));
    assert_eq!(second["outcome"], "ok");
    assert_eq!(second["status"], 200);
    for same in [
        "extract",
        "dom_html",
        "headers",
        "final_url",
        "persona",
        "robots",
    ] {
        assert_eq!(second[same], first[same], "{same}");
    }
    assert!(
        second["dom_html"]
            .as_str()
            .unwrap()
            .contains("<h1>The budget</h1>")
    );
    // Its document is a reference of its own to what is stored once.
    assert_ne!(
        second["artifacts"]["dom_html"],
        first["artifacts"]["dom_html"]
    );
    let found = api
        .get(
            &token,
            &format!("/v1/results/{}", second["id"].as_str().unwrap()),
        )
        .await;
    assert_eq!(found.status, StatusCode::OK);
    assert_eq!(found.json()["cached"], true);
    assert_eq!(found.json()["extract"], first["extract"]);
    let artifact = api
        .get(
            &token,
            &format!(
                "/v1/artifacts/{}",
                second["artifacts"]["dom_html"].as_str().unwrap()
            ),
        )
        .await;
    assert_eq!(artifact.status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&artifact.body).contains("<h1>The budget</h1>"));
    assert_eq!(count(&api, "response_cache").await, 1);

    // Another include set is another request: a visit, and this key has
    // no page left today.
    let mut fewer = asking.clone();
    fewer["include"] = json!(["dom", "headers"]);
    let refused = api.post(&token, "/v1/fetch", &fewer).await;
    refused.problem(StatusCode::TOO_MANY_REQUESTS, "budget_exhausted");
    // And another key has entries of its own: its request is a visit, and
    // fills them.
    let theirs = api.post(&other, "/v1/fetch", &asking).await.json();
    assert_eq!(theirs["cached"], false);
    assert_eq!(paths(&api), ["/budget", "/budget"]);
    let again = api.post(&other, "/v1/fetch", &asking).await.json();
    assert_eq!(again["cached"], true);
    assert_eq!(paths(&api).len(), 2);
    assert_eq!(count(&api, "response_cache").await, 2);
    // A different include set misses for it too, and is a visit.
    let visit = api.post(&other, "/v1/fetch", &fewer).await.json();
    assert_eq!(
        (&visit["cached"], &visit["extract"]),
        (&json!(false), &Value::Null)
    );
    assert_eq!(paths(&api).len(), 3);
    // A request that accepts nothing older than no time at all is one too.
    let mut never = asking.clone();
    never["cache_ttl_s"] = json!(0);
    assert_eq!(
        api.post(&other, "/v1/fetch", &never).await.json()["cached"],
        false
    );
    assert_eq!(paths(&api).len(), 4);

    // What the site's results may not be served for is refused, not cut.
    let mut long = asking.clone();
    long["cache_ttl_s"] = json!(86_401);
    let problem = api
        .post(&other, "/v1/fetch", &long)
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "cache_ttl_too_long");
    assert_eq!(problem["maximum_s"], 86_400);
    // And a site whose policy leaves its robots.txt alone has nothing a
    // request could have enforced.
    let mut enforcing = asking.clone();
    enforcing["robots"] = json!("enforce");
    let problem = api
        .post(&other, "/v1/fetch", &enforcing)
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "robots_ignored");
    assert_eq!(problem["field"], "robots");
    assert_eq!(paths(&api).len(), 4);
    assert_eq!(count(&api, "robots_cache").await, 0);
    api.shut_down().await;
}

#[tokio::test]
async fn where_a_robots_txt_is_enforced_a_scout_asks_and_what_it_disallows_is_not_fetched() {
    let mut api = Api::start(3, 155, ENFORCED).await;
    let token = api.key(&[Scope::Fetch]).await;
    show_file(
        &api,
        "/robots.txt",
        "User-agent: *\nDisallow: /private\nCrawl-delay: 1\n",
    );
    show(&api, "/open", &article("Open", &[]), &[]);
    show(&api, "/private/ledger", &article("Ledger", &[]), &[]);
    let quickly = |path: &str| {
        json!({
            "url": Api::url(path),
            "behavior": { "scroll_depth": 0, "dwell_ms": 1000 },
            "max_retries": 0,
        })
    };

    // The first fetch of the origin waits for the file: the scout asks for
    // it, typed, in its own window, and the reader then reads the page.
    let reply = api.post(&token, "/v1/fetch", &quickly("/open")).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    let open = reply.json();
    assert_eq!(open["outcome"], "ok");
    assert_eq!(
        open["robots"],
        json!({ "fetched": true, "allowed": true, "matched_rule": null, "crawl_delay": 1.0 })
    );
    let asked = navigated(&api);
    assert_eq!(
        asked
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>(),
        ["/robots.txt", "/open"]
    );
    let (scout, reader) = (asked[0].1.clone(), asked[1].1.clone());
    assert_ne!(scout, reader);
    let file = api
        .lab
        .chromium
        .with(|sidecar| sidecar.navigated[0].clone());
    assert_eq!(file["behavior"]["entry"]["strategy"], "direct");
    assert_eq!(file["capture"]["raw"], true);
    assert_eq!(count(&api, "robots_cache").await, 1);
    assert_eq!(count(&api, "site_scouts").await, 1);

    // What the file disallows is not asked for: the answer is a result
    // that names the rule, and nothing reached the site.
    let reply = api
        .post(&token, "/v1/fetch", &quickly("/private/ledger"))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    let denied = reply.json();
    assert_eq!(denied["outcome"], "robots_denied");
    assert_eq!(denied["attempts"], json!([]));
    assert_eq!(
        (&denied["status"], &denied["persona"]),
        (&Value::Null, &Value::Null)
    );
    assert_eq!(
        denied["robots"],
        json!({
            "fetched": true, "allowed": false, "matched_rule": "Disallow: /private",
            "crawl_delay": 1.0,
        })
    );
    assert_eq!(navigated(&api).len(), 2);
    let stored = api
        .get(
            &token,
            &format!("/v1/results/{}", denied["id"].as_str().unwrap()),
        )
        .await
        .json();
    assert_eq!(stored["outcome"], "robots_denied");
    assert_eq!(stored["robots"], denied["robots"]);

    // More pages: one request for the file, and the scout reads none.
    for _ in 0..3 {
        let again = api
            .post(&token, "/v1/fetch", &quickly("/open"))
            .await
            .json();
        assert_eq!(again["outcome"], "ok");
    }
    let asked = navigated(&api);
    assert_eq!(
        asked
            .iter()
            .filter(|(path, _)| path == "/robots.txt")
            .count(),
        1
    );
    let readers: BTreeSet<&str> = asked
        .iter()
        .filter(|(path, _)| path != "/robots.txt")
        .map(|(_, window)| window.as_str())
        .collect();
    assert!(!readers.contains(scout.as_str()), "{asked:?}");

    // A request cannot ask for less than the policy enforces.
    for wish in ["ignore", "report"] {
        let mut asking = quickly("/private/ledger");
        asking["robots"] = json!(wish);
        let problem = api
            .post(&token, "/v1/fetch", &asking)
            .await
            .problem(StatusCode::UNPROCESSABLE_ENTITY, "robots_enforced");
        assert_eq!(problem["field"], "robots");
    }
    assert_eq!(navigated(&api).len(), asked.len());
    api.shut_down().await;
}

#[tokio::test]
async fn a_crawl_keeps_to_an_enforced_robots_txt_and_reads_the_sitemaps_it_names() {
    let mut api = Api::start_with(3, 155, ENFORCED, quick).await;
    let token = api.key(&[Scope::Jobs, Scope::Crawl, Scope::Fetch]).await;
    show_file(
        &api,
        "/robots.txt",
        &format!(
            "User-agent: *\nDisallow: /private\nSitemap: {}\nSitemap: http://elsewhere.test/map.xml\n",
            Api::url("/maps/pages.xml")
        ),
    );
    show_file(
        &api,
        "/maps/pages.xml",
        &format!(
            "<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\
             <url><loc>{}</loc></url><url><loc>{}</loc></url></urlset>",
            Api::url("/only"),
            Api::url("/private/listed"),
        ),
    );
    show(
        &api,
        "/",
        &article("Home", &["/about", "/private/ledger"]),
        &["/about", "/private/ledger"],
    );
    show(&api, "/about", &article("About", &[]), &[]);
    show(&api, "/only", &article("Only", &[]), &[]);
    show(&api, "/private/ledger", &article("Ledger", &[]), &[]);
    show(&api, "/private/listed", &article("Listed", &[]), &[]);

    let body = json!({
        "type": "crawl",
        "seeds": [Api::url("/")],
        "use_sitemaps": true,
        "max_depth": 2,
        "defaults": {
            "include": ["dom", "headers", "extract"],
            "extract": { "markdown": false },
            "behavior": { "scroll_depth": 0, "dwell_ms": 1000 },
            "cache_ttl_s": 600,
        },
    });
    let reply = api.post(&token, "/v1/jobs", &body).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
    let id = reply.json()["id"].as_str().unwrap().to_owned();
    let work = api.work();
    let done = api.job_until(&token, &id, ended).await;
    assert_eq!(done["state"], "succeeded", "{done}");

    // The file was read first, then the sitemap it names (and not the
    // guess, and not another site's), then the pages it allows.
    let asked = navigated(&api);
    let order: Vec<&str> = asked.iter().map(|(path, _)| path.as_str()).collect();
    assert_eq!(order[..2], ["/robots.txt", "/maps/pages.xml"]);
    let mut pages: Vec<&str> = order[2..].to_vec();
    pages.sort_unstable();
    assert_eq!(pages, ["/", "/about", "/only"]);
    assert_eq!(
        done["crawl"]["sitemaps"],
        json!([
            { "site": "kage.test", "url": Api::url("/robots.txt"), "state": "read", "urls": 1 },
            { "site": "kage.test", "url": Api::url("/maps/pages.xml"), "state": "read", "urls": 2 },
        ])
    );
    // What the file disallows was never asked for and is no item: it is
    // counted, with its reason.
    assert_eq!(done["crawl"]["skipped"]["robots_denied"], 2);
    assert_eq!(done["items"]["total"], 3);
    // One scout for the file and the sitemap, and it read no page.
    let scouts: BTreeSet<&str> = asked[..2]
        .iter()
        .map(|(_, window)| window.as_str())
        .collect();
    let readers: BTreeSet<&str> = asked[2..]
        .iter()
        .map(|(_, window)| window.as_str())
        .collect();
    assert_eq!(scouts.len(), 1);
    assert!(scouts.is_disjoint(&readers), "{asked:?}");
    assert_eq!(count(&api, "site_scouts").await, 1);

    // Every page's result has what was extracted from it and the verdict.
    let items = api.job_items(&token, &id).await;
    assert_eq!(items.len(), 3);
    for item in &items {
        let result = api
            .get(
                &token,
                &format!("/v1/results/{}", item["request_id"].as_str().unwrap()),
            )
            .await
            .json();
        assert_eq!(result["cached"], false);
        assert_eq!(result["robots"]["fetched"], true);
        assert_eq!(result["robots"]["allowed"], true);
        assert_eq!(result["extract"]["thin_content"], false);
        assert_eq!(result["extract"]["markdown"], Value::Null);
        assert!(result["extract"]["links"].is_array());
    }

    // What the crawl read is there for a fetch that asks for the same;
    // a second crawl visits every page again.
    let before = navigated(&api).len();
    let fetched = api
        .post(
            &token,
            "/v1/fetch",
            &json!({
                "url": Api::url("/about"),
                "include": ["dom", "headers", "extract"],
                "extract": { "markdown": false },
                "behavior": { "scroll_depth": 0, "dwell_ms": 1000 },
                "cache_ttl_s": 600,
            }),
        )
        .await
        .json();
    assert_eq!(fetched["cached"], true, "{fetched}");
    assert_eq!(fetched["extract"]["title"], "About");
    assert_eq!(navigated(&api).len(), before);
    let reply = api.post(&token, "/v1/jobs", &body).await;
    let again = reply.json()["id"].as_str().unwrap().to_owned();
    let done = api.job_until(&token, &again, ended).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    let asked = navigated(&api);
    let mut later: Vec<&str> = asked[before..]
        .iter()
        .map(|(path, _)| path.as_str())
        .collect();
    later.sort_unstable();
    // The file is known, and the sitemap is asked for by the same scout.
    assert_eq!(later, ["/", "/about", "/maps/pages.xml", "/only"]);
    assert_eq!(
        asked
            .iter()
            .filter(|(path, _)| path == "/robots.txt")
            .count(),
        1
    );
    let sitemap_scouts: BTreeSet<&str> = asked
        .iter()
        .filter(|(path, _)| path == "/maps/pages.xml")
        .map(|(_, window)| window.as_str())
        .collect();
    assert_eq!(sitemap_scouts, scouts);
    work.stop().await;
    api.shut_down().await;
}

#[tokio::test]
async fn the_items_of_a_batch_are_answered_from_the_cache_and_a_flow_extracts_what_it_captures() {
    let mut api = Api::start_with(2, 155, BROWSER_FAST, quick).await;
    let token = api.key(&[Scope::Jobs, Scope::Fetch]).await;
    show(&api, "/a", &article("Page A", &[]), &[]);
    show(&api, "/b", &article("Page B", &[]), &[]);
    let batch = json!({
        "type": "batch",
        "urls": [Api::url("/a"), Api::url("/b")],
        "defaults": {
            "include": ["extract"],
            "behavior": { "scroll_depth": 0, "dwell_ms": 1000 },
            "cache_ttl_s": 600,
        },
    });
    let work = api.work();
    let result_of = |item: &Value| format!("/v1/results/{}", item["request_id"].as_str().unwrap());

    let reply = api.post(&token, "/v1/jobs", &batch).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
    let first = reply.json()["id"].as_str().unwrap().to_owned();
    let done = api.job_until(&token, &first, ended).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(paths(&api).len(), 2);
    for item in api.job_items(&token, &first).await {
        let result = api.get(&token, &result_of(&item)).await.json();
        assert_eq!(result["cached"], false);
        assert!(
            result["extract"]["title"]
                .as_str()
                .unwrap()
                .starts_with("Page ")
        );
        // Only the extraction was asked for: no document is returned.
        assert_eq!(result["dom_html"], Value::Null);
    }

    // The same batch again: both items are answered from the cache, and
    // nothing is asked of the site.
    let reply = api.post(&token, "/v1/jobs", &batch).await;
    let second = reply.json()["id"].as_str().unwrap().to_owned();
    let done = api.job_until(&token, &second, ended).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(paths(&api).len(), 2);
    for item in api.job_items(&token, &second).await {
        assert_eq!(item["state"], "succeeded");
        let result = api.get(&token, &result_of(&item)).await.json();
        assert_eq!(result["cached"], true);
        assert_eq!(result["attempts"], json!([]));
        assert!(
            result["extract"]["title"]
                .as_str()
                .unwrap()
                .starts_with("Page ")
        );
    }

    // A flow's capture is extracted where the step asks for it, from the
    // page as it was at that point.
    api.lab.chromium.with(|sidecar| {
        sidecar.pages.insert(
            Api::url("/a"),
            json!({ "title": "Page A", "dom_html": article("Page A", &[]) }),
        );
    });
    let flow = json!({
        "type": "flow",
        "actions": [
            { "action": "goto", "url": Api::url("/a") },
            { "action": "extract", "include": ["extract"], "extract": { "links": false } },
        ],
        "include": ["extract"],
    });
    let reply = api.post(&token, "/v1/jobs", &flow).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
    let id = reply.json()["id"].as_str().unwrap().to_owned();
    let done = api.job_until(&token, &id, ended).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    let items = api.job_items(&token, &id).await;
    let captured = api.get(&token, &result_of(&items[1])).await.json();
    assert_eq!(captured["extract"]["title"], "Page A");
    assert_eq!(captured["extract"]["links"], Value::Null);
    assert!(captured["extract"]["markdown"].is_string());
    assert_eq!(captured["cached"], false);
    work.stop().await;
    api.shut_down().await;
}
