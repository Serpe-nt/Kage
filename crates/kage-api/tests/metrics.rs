//! `/metrics` (spec §20): the exporter installed as `kage serve` installs
//! it, the series of the specification after real work, and who may read
//! them.
//!
//! One test: a process has one recorder.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Api, ask, request};
use kage_core::config::{SelftestPageConfig, SelftestPageKind};
use kage_core::domain::Scope;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use common::sched::lab::article;

/// Whether the exposition has a sample of `series` with every one of
/// `labels` (`name="value"`).
fn has(text: &str, series: &str, labels: &[&str]) -> bool {
    text.lines().any(|line| {
        !line.starts_with('#')
            && (line.starts_with(&format!("{series}{{")) || line.starts_with(&format!("{series} ")))
            && labels.iter().all(|label| line.contains(label))
    })
}

#[track_caller]
fn expect(text: &str, series: &str, labels: &[&str]) {
    assert!(
        has(text, series, labels),
        "no sample of {series} with {labels:?} in:\n{text}"
    );
}

#[tokio::test]
async fn metrics_expose_the_series_of_the_specification() {
    // The recorder is installed before anything records, as in `serve`.
    let handle = kage_api::metrics::install().unwrap();
    assert!(kage_api::metrics::install().is_err());

    // The http path first, a browser for what needs one, and a domain
    // that is paused after two blocks.
    let rules = "defaults: { render: http, pacing: fast, active_hours: ignore }\n\
                 domains:\n  walled.kage.test: { render: browser, breaker_threshold: 2, breaker_pause: 30m, retries: 1 }\n";
    let mut api = Api::start_with(2, 149, rules, |config| {
        config.jobs.poll_interval =
            kage_core::duration::HumanDuration::new(std::time::Duration::from_millis(50));
        config.selftest.browser.pages = vec![SelftestPageConfig {
            id: "probe".to_owned(),
            kind: SelftestPageKind::Probe,
            url: "http://shop.kage.test/probe/".to_owned(),
        }];
    })
    .await;
    let router = api.with_parts(|parts| parts.metrics = Some(handle.clone()));
    let fetch = api.key(&[Scope::Fetch]).await;
    let admin = api.key(&[Scope::Admin]).await;
    let send = |verb: Method, at: &str, token: Option<&str>, body: Option<serde_json::Value>| {
        let router = router.clone();
        let sent = request(verb, at, token, body.as_ref());
        async move { ask(&router, sent).await }
    };
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(article("A page")),
        )
        .mount(&api.lab.site)
        .await;

    // Work: a page over http, a page in a browser, a wall met twice (the
    // domain is paused), a request the scheduler refuses, a self-test, a
    // replacement asked of the provider, and requests the API refuses.
    let page = send(
        Method::POST,
        "/v1/fetch",
        Some(&fetch),
        Some(json!({ "url": Api::url("/page"), "render": "http" })),
    )
    .await;
    assert_eq!(page.json()["outcome"], "ok", "{:?}", page.json());
    // A job of two pages, worked to its end by a server's worker.
    let jobs = api.key(&[Scope::Jobs]).await;
    let worker = kage_api::ApiState::new({
        let mut parts = api.parts.clone();
        parts.metrics = Some(handle.clone());
        parts
    })
    .unwrap();
    let work = common::Background::worker_only(&worker);
    let accepted = send(
        Method::POST,
        "/v1/jobs",
        Some(&jobs),
        Some(json!({
            "type": "batch",
            "urls": [Api::url("/page?item=1"), Api::url("/page?item=2")],
            "defaults": { "render": "http" },
        })),
    )
    .await;
    assert_eq!(
        accepted.status,
        StatusCode::CREATED,
        "{:?}",
        accepted.json()
    );
    let job = accepted.json()["id"].as_str().unwrap().to_owned();
    let mut ended = serde_json::Value::Null;
    for _ in 0..400 {
        ended = send(Method::GET, &format!("/v1/jobs/{job}"), Some(&jobs), None)
            .await
            .json();
        if ended["state"] == "succeeded" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(ended["state"], "succeeded", "{ended}");
    // A crawl of two pages that say the same: the seed typed, the second
    // reached from it, a link nobody sees left alone.
    let words = (0..60)
        .map(|n| format!("word{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    for (at, links) in [
        (
            "/crawl/",
            "<a href=\"/crawl/copy\">on</a><a href=\"/crawl/trap\" hidden>no</a>",
        ),
        ("/crawl/copy", "<a href=\"/crawl/\">on</a>"),
    ] {
        Mock::given(method("GET"))
            .and(path(at))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html; charset=utf-8")
                    .set_body_string(format!(
                        "<html><body><main><p>{words}</p></main><nav>{links}</nav></body></html>"
                    )),
            )
            .mount(&api.lab.site)
            .await;
    }
    let crawler = api.key(&[Scope::Jobs, Scope::Crawl]).await;
    let accepted = send(
        Method::POST,
        "/v1/jobs",
        Some(&crawler),
        Some(json!({
            "type": "crawl",
            "seeds": [Api::url("/crawl/")],
            "defaults": { "render": "http" },
        })),
    )
    .await;
    assert_eq!(
        accepted.status,
        StatusCode::CREATED,
        "{:?}",
        accepted.json()
    );
    let crawl = accepted.json()["id"].as_str().unwrap().to_owned();
    let mut read = serde_json::Value::Null;
    for _ in 0..400 {
        read = send(
            Method::GET,
            &format!("/v1/jobs/{crawl}"),
            Some(&crawler),
            None,
        )
        .await
        .json();
        if read["state"] == "succeeded" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(read["state"], "succeeded", "{read}");
    assert_eq!(read["crawl"]["duplicates"], 1, "{read}");
    work.stop().await;
    let rendered = send(
        Method::POST,
        "/v1/fetch",
        Some(&fetch),
        Some(json!({ "url": Api::url("/app"), "render": "browser" })),
    )
    .await;
    assert_eq!(rendered.json()["outcome"], "ok", "{:?}", rendered.json());
    let wall = json!({
        "status": 403,
        "title": "Attention Required! | Cloudflare",
        "dom_html": "<html><head><title>Attention Required! | Cloudflare</title></head>\
                     <body><h1>Sorry, you have been blocked</h1>\
                     <p>You are unable to access this site. Cloudflare Ray ID: 8f1</p></body></html>",
    });
    api.lab.chromium.with(|sidecar| {
        sidecar.next_pages.push_back(wall.clone());
        sidecar.next_pages.push_back(wall);
    });
    let blocked = send(
        Method::POST,
        "/v1/fetch",
        Some(&fetch),
        Some(json!({ "url": "http://walled.kage.test/" })),
    )
    .await;
    assert_eq!(
        blocked.json()["outcome"],
        "forbidden",
        "{:?}",
        blocked.json()
    );
    let paused = send(
        Method::POST,
        "/v1/fetch",
        Some(&fetch),
        Some(json!({ "url": "http://walled.kage.test/again" })),
    )
    .await;
    assert_eq!(
        paused.json()["reason"],
        "domain_paused",
        "{:?}",
        paused.json()
    );
    let persona = api.lab.personas[0].id;
    let tested = send(
        Method::POST,
        &format!("/v1/admin/personas/{persona}/selftest"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(
        tested.json()["browser"]["verdict"],
        "passed",
        "{:?}",
        tested.json()
    );
    send(Method::GET, "/v1/admin/pool", Some(&admin), None).await;
    send(Method::GET, "/v1/admin/keys", None, None)
        .await
        .problem(StatusCode::UNAUTHORIZED, "missing_key");
    send(Method::GET, "/v1/nothing", None, None)
        .await
        .problem(StatusCode::NOT_FOUND, "not_found");
    // What the server's loops would have done by now.
    let engine = api.parts.engine.clone().unwrap();
    engine.bandwidth.flush().await.unwrap();
    engine.bandwidth.assess().await.unwrap();
    api.lab.pool.tick().await.unwrap();
    let proxy = api.lab.personas[1].proxy_id;
    send(
        Method::POST,
        &format!("/v1/admin/proxies/{proxy}/replace"),
        Some(&admin),
        None,
    )
    .await;

    // Who may read them: a key with the admin scope.
    send(Method::GET, "/metrics", None, None)
        .await
        .problem(StatusCode::UNAUTHORIZED, "missing_key");
    send(Method::GET, "/metrics", Some(&fetch), None)
        .await
        .problem(StatusCode::FORBIDDEN, "scope_missing");
    let scraped = send(Method::GET, "/metrics", Some(&admin), None).await;
    assert_eq!(scraped.status, StatusCode::OK);
    assert_eq!(
        scraped.header("content-type"),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );
    let text = String::from_utf8(scraped.body.to_vec()).unwrap();

    // The series of spec §20, with their labels.
    let site = "domain=\"shop.kage.test\"";
    expect(
        &text,
        "kage_requests_total",
        &[site, "render=\"http\"", "outcome=\"ok\"", "vendor=\"none\""],
    );
    expect(
        &text,
        "kage_requests_total",
        &[site, "render=\"browser\"", "outcome=\"ok\""],
    );
    expect(
        &text,
        "kage_requests_total",
        &[
            "domain=\"walled.kage.test\"",
            "outcome=\"forbidden\"",
            "vendor=\"cloudflare\"",
        ],
    );
    expect(
        &text,
        "kage_attempts_total",
        &[site, "render=\"http\"", "outcome=\"ok\""],
    );
    expect(
        &text,
        "kage_attempts_total",
        &[
            "domain=\"walled.kage.test\"",
            "render=\"browser\"",
            "outcome=\"forbidden\"",
            "vendor=\"cloudflare\"",
        ],
    );
    // A histogram with buckets, so that it can be aggregated.
    expect(
        &text,
        "kage_attempt_duration_seconds_bucket",
        &["render=\"http\"", "outcome=\"ok\"", "le=\"+Inf\""],
    );
    expect(
        &text,
        "kage_attempt_duration_seconds_count",
        &["render=\"browser\""],
    );
    expect(&text, "kage_persona_state", &["state=\"active\""]);
    expect(&text, "kage_persona_state", &["state=\"cooling\""]);
    expect(&text, "kage_persona_state", &["state=\"retired\""]);
    expect(&text, "kage_proxy_state", &["state=\"healthy\""]);
    expect(
        &text,
        "kage_scheduler_wait_seconds_bucket",
        &["kind=\"sync\""],
    );
    expect(&text, "kage_queue_depth", &["kind=\"sync\""]);
    expect(&text, "kage_queue_depth", &["kind=\"job\""]);
    expect(&text, "kage_scheduler_in_flight", &["render=\"browser\""]);
    expect(&text, "kage_scheduler_in_flight", &["render=\"http\""]);
    expect(
        &text,
        "kage_scheduler_refusals_total",
        &["reason=\"domain_paused\""],
    );
    expect(
        &text,
        "kage_breaker_open",
        &["domain=\"walled.kage.test\"", " 1"],
    );
    expect(&text, "kage_bandwidth_bytes_total", &["direction=\"up\""]);
    expect(&text, "kage_bandwidth_bytes_total", &["direction=\"down\""]);
    expect(&text, "kage_bandwidth_budget_ratio", &[]);
    expect(
        &text,
        "kage_selftest_score",
        &[
            &format!("persona=\"{persona}\""),
            "engine=\"chromium\"",
            " 100",
        ],
    );
    // `kage_replacements_total` counts replacements asked of the provider,
    // and the lab's provider cannot be asked for one: nothing moved it.
    // (crates/kage-proxy/tests covers it against the mock Webshare API.)
    // The sidecars' own series, merged in from both containers.
    expect(&text, "sidecar_contexts_open", &["engine=\"chromium\""]);
    expect(&text, "sidecar_contexts_open", &["engine=\"firefox\""]);
    expect(
        &text,
        "sidecar_navigation_duration_seconds_count",
        &["engine=\"chromium\""],
    );

    // Jobs: what the store holds at the scrape, and what the worker
    // counted.
    expect(
        &text,
        "kage_jobs",
        &["job_type=\"batch\"", "state=\"succeeded\"", " 1"],
    );
    expect(&text, "kage_job_items", &["state=\"queued\"", " 0"]);
    expect(&text, "kage_job_items", &["state=\"running\"", " 0"]);
    expect(&text, "kage_webhook_deliveries_pending", &[" 0"]);
    expect(&text, "kage_jobs_accepted_total", &["job_type=\"batch\""]);
    expect(
        &text,
        "kage_jobs_finished_total",
        &["job_type=\"batch\"", "state=\"succeeded\""],
    );
    expect(
        &text,
        "kage_job_items_total",
        &["job_type=\"batch\"", "state=\"succeeded\"", " 2"],
    );
    // A crawl is a job, and says how its pages were reached, what it
    // left alone and what repeated.
    expect(
        &text,
        "kage_jobs_finished_total",
        &["job_type=\"crawl\"", "state=\"succeeded\""],
    );
    expect(
        &text,
        "kage_job_items_total",
        &["job_type=\"crawl\"", "state=\"succeeded\"", " 2"],
    );
    expect(
        &text,
        "kage_crawl_pages_total",
        &["reached=\"typed\"", " 1"],
    );
    expect(
        &text,
        "kage_crawl_pages_total",
        &["reached=\"referer\"", " 1"],
    );
    expect(
        &text,
        "kage_crawl_skipped_total",
        &["reason=\"hidden\"", " 1"],
    );
    expect(
        &text,
        "kage_crawl_duplicates_total",
        &["kind=\"exact\"", " 1"],
    );
    // The job's pages were the scheduler's work like any other, under
    // their own kind.
    expect(
        &text,
        "kage_scheduler_wait_seconds_bucket",
        &["kind=\"job\""],
    );

    // The API's own.
    expect(
        &text,
        "kage_api_requests_total",
        &["route=\"/v1/fetch\"", "method=\"POST\"", "status=\"200\""],
    );
    expect(
        &text,
        "kage_api_requests_total",
        &[
            "route=\"/v1/admin/personas/{id}/selftest\"",
            "status=\"200\"",
        ],
    );
    // A path nothing is served under is one label, whatever was asked.
    expect(
        &text,
        "kage_api_requests_total",
        &["route=\"unmatched\"", "status=\"404\""],
    );
    assert!(!text.contains("/v1/nothing"), "{text}");
    expect(
        &text,
        "kage_api_request_duration_seconds_bucket",
        &["route=\"/v1/fetch\"", "le=\"+Inf\""],
    );
    expect(&text, "kage_api_requests_in_flight", &[]);
    expect(&text, "kage_api_refusals_total", &["code=\"missing_key\""]);
    // Every family is described once, and no line is there twice.
    for family in [
        "kage_requests_total",
        "kage_persona_state",
        "kage_api_requests_total",
        "kage_jobs",
        "kage_job_items",
        "sidecar_contexts_open",
    ] {
        let described = text
            .lines()
            .filter(|line| line.starts_with(&format!("# TYPE {family} ")))
            .count();
        assert_eq!(described, 1, "{family}");
    }
    // Nothing secret is a label: no token, no proxy login, no address of
    // a proxy.
    for secret in [
        fetch.as_str(),
        admin.as_str(),
        common::sched::proxy::PROXY_PASS,
        "198.51.",
    ] {
        assert!(!text.contains(secret), "{secret} in the metrics");
    }

    // The listener of `server.metrics_bind` serves the same without a
    // key, and nothing else of the API.
    let private = kage_api::metrics_router(
        kage_api::ApiState::new({
            let mut parts = api.parts.clone();
            parts.metrics = Some(handle.clone());
            parts
        })
        .unwrap(),
    );
    let open = ask(&private, request(Method::GET, "/metrics", None, None)).await;
    assert_eq!(open.status, StatusCode::OK);
    let open_text = String::from_utf8(open.body.to_vec()).unwrap();
    expect(&open_text, "kage_requests_total", &[site]);
    expect(
        &open_text,
        "sidecar_contexts_open",
        &["engine=\"chromium\""],
    );
    let alive = ask(&private, request(Method::GET, "/healthz", None, None)).await;
    assert_eq!(alive.status, StatusCode::OK);
    for at in ["/v1/admin/keys", "/v1/fetch", "/openapi.json", "/readyz"] {
        let reply = ask(&private, request(Method::GET, at, Some(&admin), None)).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{at}");
    }
    api.shut_down().await;
}
