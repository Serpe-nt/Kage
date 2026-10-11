//! Jobs and their webhooks (spec §16.6, §8.4): `POST /v1/jobs` and what
//! follows from it, over the router of `kage serve` and the scheduler's
//! lab, with the job worker and the webhook sender running as `kage serve`
//! runs them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod common;

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use chrono::{DateTime, Utc};
use common::sched::lab::{LENIENT_FAST, article};
use common::{Api, Background, ask, request};
use hmac::{Hmac, KeyInit, Mac};
use kage_api::jobs::webhook::retry_wait;
use kage_api::{DirectTransport, WebhookSender};
use kage_core::config::Config;
use kage_core::domain::{KeyLimits, Scope};
use kage_core::duration::HumanDuration;
use kage_core::ids::{RequestId, WebhookDeliveryId};
use kage_proxy::{Clock, CredentialCipher, ManualClock};
use serde_json::{Value, json};
use sha2::Sha256;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Every request goes to the browser.
const BROWSER_FAST: &str = "defaults: { render: browser, pacing: fast, active_hours: ignore, min_dwell: 500ms }\ndomains: {}\n";

/// A worker and a sender that look often: a test does not wait seconds for
/// either.
fn quick(config: &mut Config) {
    config.jobs.poll_interval = HumanDuration::new(Duration::from_millis(50));
    config.jobs.lease = HumanDuration::new(Duration::from_secs(3));
    config.webhooks.poll_interval = HumanDuration::new(Duration::from_millis(50));
}

fn page(title: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(article(title), "text/html; charset=utf-8")
}

/// Serves every `<prefix>/<number>` of the site.
async fn pages(api: &Api, prefix: &str, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path_regex(format!("^{prefix}/\\d+$")))
        .respond_with(response)
        .mount(&api.lab.site)
        .await;
}

fn urls(prefix: &str, count: usize) -> Vec<String> {
    (1..=count)
        .map(|number| Api::url(&format!("{prefix}/{number}")))
        .collect()
}

/// The paths under `prefix` the site was asked for, in order.
async fn asked(api: &Api, prefix: &str) -> Vec<String> {
    api.lab
        .site
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|request| request.url.path().to_owned())
        .filter(|path| path.starts_with(prefix))
        .collect()
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

/// The personas that made the attempts of an item's request.
async fn personas_of(api: &Api, item: &Value) -> Vec<String> {
    let request: RequestId = item["request_id"].as_str().unwrap().parse().unwrap();
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

#[tokio::test]
async fn a_batch_is_read_in_order_by_one_visitor_and_every_item_has_its_result() {
    let mut api = Api::start_with(3, 149, LENIENT_FAST, quick).await;
    pages(&api, "/p", page("A page")).await;
    pages(&api, "/other", page("Another page")).await;
    let token = api.key(&[Scope::Jobs, Scope::Fetch]).await;
    let other = api.key(&[Scope::Jobs]).await;
    let listed = urls("/p", 6);
    let body = json!({
        "type": "batch",
        "urls": listed,
        "items": [{ "url": Api::url("/p/7"), "include": ["raw", "headers"] }],
        "defaults": { "render": "http" },
    });
    let keyed = |body: &Value, key: &str| {
        let mut request = request(Method::POST, "/v1/jobs", Some(&token), Some(body));
        request
            .headers_mut()
            .insert("idempotency-key", key.parse().unwrap());
        request
    };

    let reply = api.send(keyed(&body, "nightly-1")).await;

    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
    let job = reply.json();
    let id = job["id"].as_str().unwrap().to_owned();
    assert_eq!(
        reply.header("location"),
        Some(format!("/v1/jobs/{id}").as_str())
    );
    assert_eq!(reply.header("idempotency-replayed"), None);
    assert_eq!(job["type"], "batch");
    assert_eq!(job["state"], "queued");
    assert_eq!(job["origin"], "api");
    assert_eq!(job["visitors"], 1);
    assert_eq!(
        job["items"],
        json!({ "total": 7, "succeeded": 0, "failed": 0, "cancelled": 0, "pending": 7 })
    );
    assert_eq!(job["webhook"], Value::Null);
    assert_eq!(job["items_url"], format!("/v1/jobs/{id}/items"));
    assert!(job["deadline"].is_string());

    // The same request under the same key is the same job, whatever the
    // order its members are written in; nothing is stored twice.
    let again = api.send(keyed(&body, "nightly-1")).await;
    assert_eq!(again.status, StatusCode::OK, "{:?}", again.json());
    assert_eq!(again.header("idempotency-replayed"), Some("true"));
    assert_eq!(again.json()["id"], id);
    let reordered = json!({
        "defaults": { "render": "http" },
        "items": [{ "include": ["raw", "headers"], "url": Api::url("/p/7") }],
        "urls": listed,
        "type": "batch",
    });
    let again = api.send(keyed(&reordered, "nightly-1")).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.json()["id"], id);
    // Another request under that key is a mistake, and says which job the
    // key names.
    let different = json!({ "type": "batch", "urls": [Api::url("/other/1")] });
    let refused = api.send(keyed(&different, "nightly-1")).await;
    let problem = refused.problem(StatusCode::CONFLICT, "idempotency_key_reused");
    assert_eq!(problem["job_id"], id);
    // A key belongs to the API key that used it.
    let elsewhere = {
        let mut request = request(Method::POST, "/v1/jobs", Some(&other), Some(&different));
        request
            .headers_mut()
            .insert("idempotency-key", "nightly-1".parse().unwrap());
        api.send(request).await
    };
    assert_eq!(elsewhere.status, StatusCode::CREATED);
    let theirs = elsewhere.json()["id"].as_str().unwrap().to_owned();
    assert_ne!(theirs, id);
    // What cannot be a key is refused before anything else.
    api.send(keyed(&body, "two words"))
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed");
    assert_eq!(count(&api, "jobs").await, 2);
    assert_eq!(count(&api, "job_items").await, 8);

    let work = api.work();
    let done = api.job_until(&token, &id, ended).await;

    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(
        done["items"],
        json!({ "total": 7, "succeeded": 7, "failed": 0, "cancelled": 0, "pending": 0 })
    );
    assert!(done["started_at"].is_string() && done["finished_at"].is_string());
    assert_eq!(done["paused"], Value::Null);

    // The items, page by page, in the order they were given.
    let first = api
        .get(&token, &format!("/v1/jobs/{id}/items?limit=4"))
        .await
        .json();
    assert_eq!(first["items"].as_array().unwrap().len(), 4);
    assert_eq!(first["next_cursor"], "3");
    let rest = api
        .get(&token, &format!("/v1/jobs/{id}/items?limit=4&cursor=3"))
        .await
        .json();
    assert_eq!(rest["items"].as_array().unwrap().len(), 3);
    assert_eq!(rest["next_cursor"], Value::Null);
    let items = api.job_items(&token, &id).await;
    assert_eq!(items.len(), 7);
    let mut visitors = Vec::new();
    for (item, number) in items.iter().zip(1_u64..) {
        assert_eq!(item["seq"], number - 1);
        assert_eq!(item["state"], "succeeded", "{item}");
        assert_eq!(item["url"], Api::url(&format!("/p/{number}")));
        assert_eq!(item["outcome"], "ok");
        assert_eq!(item["error"], Value::Null);
        assert_eq!(item["step"], Value::Null);
        assert!(item["started_at"].is_string() && item["finished_at"].is_string());
        // Its result, as `GET /v1/results/{id}` shows it, without the
        // bodies.
        let result = &item["result"];
        assert_eq!(result["state"], "finished", "{item}");
        assert_eq!(result["outcome"], "ok");
        assert_eq!(result["id"], item["request_id"]);
        assert_eq!(result["raw_body"], Value::Null);
        let by = personas_of(&api, item).await;
        assert_eq!(by.len(), 1, "{item}");
        visitors.extend(by);
    }
    // One household read the site, though three could have.
    visitors.dedup();
    assert_eq!(visitors.len(), 1, "{visitors:?}");
    assert_eq!(
        asked(&api, "/p/").await,
        ["/p/1", "/p/2", "/p/3", "/p/4", "/p/5", "/p/6", "/p/7"]
    );
    // The body of an item is read like any result's.
    let seventh = items[6]["request_id"].as_str().unwrap();
    let result = api.get(&token, &format!("/v1/results/{seventh}")).await;
    assert_eq!(result.status, StatusCode::OK);
    assert!(
        result.json()["raw_body"]
            .as_str()
            .unwrap()
            .contains("<h1>A page</h1>")
    );
    // Without results: the states and outcomes alone.
    let bare = api
        .get(&token, &format!("/v1/jobs/{id}/items?results=false"))
        .await
        .json();
    for item in bare["items"].as_array().unwrap() {
        assert_eq!(item["result"], Value::Null);
        assert_eq!(item["outcome"], "ok");
    }

    // What is no page of items is refused.
    for bad in ["limit=0", "limit=201", "cursor=abc", "page=2"] {
        let reply = api.get(&token, &format!("/v1/jobs/{id}/items?{bad}")).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    // Another key's job is not there, and neither is one that never was.
    for path in [
        format!("/v1/jobs/{id}"),
        format!("/v1/jobs/{id}/items"),
        "/v1/jobs/not-an-id".to_owned(),
        format!("/v1/jobs/{}", uuid::Uuid::now_v7()),
    ] {
        let key = if path.contains(&id) { &other } else { &token };
        api.get(key, &path)
            .await
            .problem(StatusCode::NOT_FOUND, "job_not_found");
    }
    api.post(&other, &format!("/v1/jobs/{id}/cancel"), &json!({}))
        .await
        .problem(StatusCode::NOT_FOUND, "job_not_found");
    // A job that has ended is answered as it ended.
    let late = api
        .call(
            Method::POST,
            &format!("/v1/jobs/{id}/cancel"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(late.status, StatusCode::OK);
    assert_eq!(late.json()["state"], "succeeded");
    assert_eq!(late.json()["cancel_requested"], false);

    // The other key's job ran too, beside it.
    let theirs = api.job_until(&other, &theirs, ended).await;
    assert_eq!(theirs["state"], "succeeded");
    work.stop().await;
    api.shut_down().await;
}

/// A request that is refused: its body, the code, and what the problem
/// names beside it.
type Case = (Value, &'static str, Option<(&'static str, Value)>);

#[tokio::test]
async fn a_job_is_accepted_whole_or_not_at_all() {
    let rules = "defaults: { render: browser, pacing: fast, active_hours: ignore }\n\
                 domains:\n  driven.kage.test: { driver_input: true }\n";
    let mut api = Api::start_with(1, 149, rules, quick).await;
    let token = api.key(&[Scope::Jobs]).await;
    let fetch_only = api.key(&[Scope::Fetch]).await;
    let batch = |change: Value| {
        let mut body = json!({ "type": "batch", "urls": [Api::url("/a")] });
        for (name, value) in change.as_object().unwrap() {
            body[name.as_str()] = value.clone();
        }
        body
    };
    let flow = |actions: Value| json!({ "type": "flow", "actions": actions });
    let hook = json!({ "url": "https://hooks.example.org/kage", "secret": "a secret of sixteen bytes or more" });

    // Who may: the `jobs` scope, and `sessions` for a session.
    api.post(&fetch_only, "/v1/jobs", &batch(json!({})))
        .await
        .problem(StatusCode::FORBIDDEN, "scope_missing");
    api.post(
        &token,
        "/v1/jobs",
        &batch(json!({ "session_id": "proj-1" })),
    )
    .await
    .problem(StatusCode::FORBIDDEN, "scope_missing");
    api.call(Method::POST, "/v1/jobs", None, Some(&batch(json!({}))))
        .await
        .problem(StatusCode::UNAUTHORIZED, "missing_key");

    let cases: Vec<Case> = vec![
        (json!({ "type": "batch" }), "validation_failed", None),
        (
            json!({ "type": "sitemap", "urls": [] }),
            "validation_failed",
            None,
        ),
        (batch(json!({ "surprise": 1 })), "validation_failed", None),
        // Every item passes the checks of a fetch, and says which did not.
        (
            batch(json!({ "urls": [Api::url("/a"), "http://10.0.0.5/admin"] })),
            "ssrf_denied",
            Some(("item", json!(1))),
        ),
        (
            batch(json!({ "items": [{ "url": Api::url("/b"), "render": "paper" }] })),
            "validation_failed",
            Some(("item", json!(1))),
        ),
        (
            batch(json!({ "defaults": { "url": Api::url("/c") } })),
            "validation_failed",
            None,
        ),
        // More visitors than the site's pacing has places for.
        (batch(json!({ "visitors": 11 })), "validation_failed", None),
        (batch(json!({ "visitors": 0 })), "validation_failed", None),
        // A receiver nobody listed, whatever else is right about it.
        (
            batch(json!({ "webhook": hook })),
            "webhook_origin_not_allowed",
            Some(("field", json!("webhook.url"))),
        ),
        // A flow begins on a page, goes nowhere the guard refuses, and is
        // done by the persona's own hands unless the operator said the
        // site does not look.
        (
            flow(json!([{ "action": "click", "selector": "#next" }])),
            "validation_failed",
            None,
        ),
        (
            flow(json!([
                { "action": "goto", "url": Api::url("/start") },
                { "action": "goto", "url": "http://169.254.169.254/latest/meta-data" },
            ])),
            "ssrf_denied",
            Some(("step", json!(1))),
        ),
        (
            flow(json!([
                { "action": "goto", "url": Api::url("/start") },
                { "action": "click", "selector": "#next", "human": false },
            ])),
            "driver_input_denied",
            Some(("step", json!(1))),
        ),
        (flow(json!([])), "validation_failed", None),
    ];
    for (body, code, extension) in cases {
        let reply = api.post(&token, "/v1/jobs", &body).await;
        let problem = reply.problem(StatusCode::UNPROCESSABLE_ENTITY, code);
        if let Some((name, value)) = extension {
            assert_eq!(problem[name], value, "{body}: {problem}");
        }
    }
    // A crawl is M10's.
    let crawler = api.key(&[Scope::Jobs, Scope::Crawl]).await;
    api.post(
        &crawler,
        "/v1/jobs",
        &json!({ "type": "crawl", "seeds": [] }),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY, "not_available_yet");
    // Nothing of any of them was stored.
    assert_eq!(count(&api, "jobs").await, 0);
    assert_eq!(count(&api, "job_items").await, 0);

    // Where the operator said so, the driver may act.
    let driven = flow(json!([
        { "action": "goto", "url": "http://driven.kage.test/start" },
        { "action": "click", "selector": "#next", "human": false },
    ]));
    let reply = api.post(&token, "/v1/jobs", &driven).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
    assert_eq!(reply.json()["type"], "flow");
    assert_eq!(reply.json()["items"]["total"], 3);
    assert_eq!(reply.json()["visitors"], Value::Null);

    // A server that is stopping accepts nothing new.
    api.state.stop();
    api.post(&token, "/v1/jobs", &batch(json!({})))
        .await
        .problem(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    api.shut_down().await;
}

#[tokio::test]
async fn a_flow_runs_its_steps_in_one_browser_and_each_capture_is_a_result() {
    let mut api = Api::start_with(2, 149, BROWSER_FAST, quick).await;
    let token = api.key(&[Scope::Jobs, Scope::Fetch]).await;
    api.lab.chromium.with(|sidecar| {
        sidecar.pages.insert(
            Api::url("/product/7"),
            json!({ "title": "A lamp", "dom_html": article("A lamp") }),
        );
    });
    let script = json!({
        "type": "flow",
        "actions": [
            { "action": "goto", "url": Api::url("/start") },
            { "action": "type", "selector": "input[name=q]", "text": "lamp" },
            { "action": "press", "key": "Enter" },
            { "action": "wait_for", "selector": "#results" },
            { "action": "extract", "include": ["dom"] },
            { "action": "goto", "url": Api::url("/product/7") },
            { "action": "extract", "include": ["dom", "headers"] },
            { "action": "back" },
        ],
        "include": ["dom", "headers"],
        "behavior": { "dwell_ms": 4000 },
    });

    let reply = api.post(&token, "/v1/jobs", &script).await;

    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
    let id = reply.json()["id"].as_str().unwrap().to_owned();
    // One item per step, and one for the page the flow ends on.
    assert_eq!(reply.json()["items"]["total"], 9);
    let work = api.work();
    let done = api.job_until(&token, &id, ended).await;
    assert_eq!(done["state"], "succeeded", "{done}");

    // One flow in one browser: the whole script at once, paced.
    let (flows, navigated) = api
        .lab
        .chromium
        .with(|sidecar| (sidecar.flows.clone(), sidecar.navigated.clone()));
    assert_eq!((flows.len(), navigated.len()), (1, 0));
    assert_eq!(flows[0]["actions"].as_array().unwrap().len(), 8);
    assert_eq!(flows[0]["behavior"]["pace"]["dwell_ms"], 4000);
    assert_eq!(flows[0]["behavior"]["pace"]["min_dwell_ms"], 500);

    let items = api.job_items(&token, &id).await;
    assert_eq!(items.len(), 9);
    let actions: Vec<&str> = items
        .iter()
        .map(|item| item["step"]["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        actions,
        [
            "goto", "type", "press", "wait_for", "extract", "goto", "extract", "back", "final"
        ]
    );
    for (item, index) in items.iter().zip(0_u64..).take(8) {
        assert_eq!(item["state"], "succeeded", "{item}");
        assert_eq!(item["url"], Value::Null);
        let step = &item["step"];
        assert_eq!(step["ok"], true);
        assert_eq!(step["duration_ms"], 12 + index);
        assert_eq!(step["paced_ms"], 7);
        assert_eq!(step["error"], Value::Null);
        // Only a step that captured the page names a result.
        let captured = matches!(index, 4 | 6);
        assert_eq!(item["request_id"].is_string(), captured, "{item}");
        assert_eq!(item["result"].is_object(), captured, "{item}");
    }
    // What each `extract` captured is a result of its own.
    let listing = &items[4]["result"];
    assert_eq!(listing["final_url"], Api::url("/start"));
    assert_eq!(listing["outcome"], "ok");
    let product = items[6]["request_id"].as_str().unwrap();
    let stored = api.get(&token, &format!("/v1/results/{product}")).await;
    assert_eq!(stored.status, StatusCode::OK, "{:?}", stored.json());
    let stored = stored.json();
    assert_eq!(stored["final_url"], Api::url("/product/7"));
    assert!(
        stored["dom_html"]
            .as_str()
            .unwrap()
            .contains("<h1>A lamp</h1>")
    );
    // The last item is the page the flow ended on: the flow's own request,
    // with its one attempt.
    let last = &items[8];
    assert_eq!(last["state"], "succeeded");
    assert_eq!(last["outcome"], "ok");
    assert_eq!(last["result"]["final_url"], Api::url("/start"));
    assert_eq!(personas_of(&api, last).await.len(), 1);
    assert_ne!(last["request_id"], items[4]["request_id"]);

    // A step that fails ends the flow there: what ran before it stands,
    // what comes after it did not run, and nobody else tries the script.
    api.lab.chromium.with(|sidecar| {
        sidecar.next_flows.push_back(json!({
            "fail": { "index": 3, "error": "selector_not_found: #results" },
        }));
    });
    let reply = api.post(&token, "/v1/jobs", &script).await;
    let broken = reply.json()["id"].as_str().unwrap().to_owned();
    let done = api.job_until(&token, &broken, ended).await;
    assert_eq!(done["state"], "partial", "{done}");
    assert_eq!(done["items"]["succeeded"], 3);
    assert_eq!(done["items"]["failed"], 6);
    let items = api.job_items(&token, &broken).await;
    assert_eq!(items[3]["state"], "failed");
    assert_eq!(items[3]["error"], "selector_not_found: #results");
    assert_eq!(items[3]["step"]["ok"], false);
    for item in &items[4..8] {
        assert_eq!(item["state"], "failed");
        assert_eq!(item["error"], "not_run");
        assert_eq!(item["step"]["ok"], Value::Null);
    }
    assert_eq!(items[8]["state"], "failed");
    assert_eq!(items[8]["error"], "flow_step_failed");
    assert_eq!(api.lab.chromium.with(|sidecar| sidecar.flows.len()), 2);
    work.stop().await;
    api.shut_down().await;
}

#[tokio::test]
async fn a_cancel_stops_a_job_within_one_item() {
    let mut api = Api::start_with(2, 149, LENIENT_FAST, quick).await;
    pages(
        &api,
        "/slow",
        page("Slow").set_delay(Duration::from_millis(500)),
    )
    .await;
    let token = api.key(&[Scope::Jobs]).await;
    let batch =
        json!({ "type": "batch", "urls": urls("/slow", 12), "defaults": { "render": "http" } });

    // Before anybody worked it: nothing of it is ever fetched.
    let reply = api.post(&token, "/v1/jobs", &batch).await;
    let early = reply.json()["id"].as_str().unwrap().to_owned();
    let cancelled = api
        .call(
            Method::POST,
            &format!("/v1/jobs/{early}/cancel"),
            Some(&token),
            None,
        )
        .await;
    assert_eq!(cancelled.status, StatusCode::OK);
    assert_eq!(cancelled.json()["cancel_requested"], true);
    assert_eq!(cancelled.json()["state"], "queued");
    let work = api.work();
    let done = api.job_until(&token, &early, ended).await;
    assert_eq!(done["state"], "cancelled");
    assert_eq!(done["items"]["cancelled"], 12);
    assert_eq!(asked(&api, "/slow/").await.len(), 0);

    // While it is read: the page that is being fetched is finished, and
    // not one more is begun.
    let reply = api.post(&token, "/v1/jobs", &batch).await;
    let id = reply.json()["id"].as_str().unwrap().to_owned();
    api.job_until(&token, &id, |job| {
        job["items"]["succeeded"].as_u64() >= Some(2)
    })
    .await;
    let at_cancel = api
        .call(
            Method::POST,
            &format!("/v1/jobs/{id}/cancel"),
            Some(&token),
            None,
        )
        .await
        .json();
    assert_eq!(at_cancel["cancel_requested"], true);
    let before = at_cancel["items"]["succeeded"].as_u64().unwrap();
    let done = api.job_until(&token, &id, ended).await;
    assert_eq!(done["state"], "cancelled", "{done}");
    let succeeded = done["items"]["succeeded"].as_u64().unwrap();
    assert!(
        succeeded <= before + 1,
        "{succeeded} items ended, {before} had when the cancel came"
    );
    assert_eq!(done["items"]["cancelled"], 12 - succeeded);
    assert_eq!(done["items"]["pending"], 0);
    // The site saw the pages that ended, and none after them.
    let seen = asked(&api, "/slow/").await;
    assert_eq!(seen.len() as u64, succeeded, "{seen:?}");
    let items = api.job_items(&token, &id).await;
    for (item, number) in items.iter().zip(1_u64..) {
        if number <= succeeded {
            assert_eq!(item["state"], "succeeded", "{item}");
        } else {
            assert_eq!(item["state"], "cancelled", "{item}");
            assert_eq!(item["error"], "cancelled");
            // Nothing was fetched for it. The one that waited for its
            // persona when the cancel came had a request already: whoever
            // asks for that is told why there is no page.
            let result = &item["result"];
            if !result.is_null() {
                assert_eq!(result["reason"], "cancelled", "{item}");
                assert_eq!(result["attempts"], json!([]));
            }
        }
    }
    work.stop().await;
    api.shut_down().await;
}

fn hex_hmac(secret: &str, message: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(message.as_bytes());
    let mut hex = String::new();
    for byte in mac.finalize().into_bytes() {
        write!(hex, "{byte:02x}").unwrap();
    }
    hex
}

/// The deliveries a receiver got at a path, in order.
async fn delivered(receiver: &MockServer, at: &str) -> Vec<wiremock::Request> {
    receiver
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path() == at)
        .collect()
}

async fn until_delivered(receiver: &MockServer, at: &str, count: usize) -> Vec<wiremock::Request> {
    for _ in 0..200 {
        let got = delivered(receiver, at).await;
        if got.len() >= count {
            return got;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{at} did not get {count} deliveries");
}

fn hook_header(request: &wiremock::Request, name: &str) -> String {
    request
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_else(|| panic!("no {name} header"))
        .to_owned()
}

#[tokio::test]
async fn a_webhook_is_signed_and_retried_on_its_schedule() {
    const SECRET: &str = "a-secret-of-the-receivers-0001";
    let receiver = MockServer::start().await;
    let origin = receiver.uri();
    for (at, status) in [("/hooks", 200), ("/down", 500), ("/gone", 410)] {
        Mock::given(method("POST"))
            .and(path(at))
            .respond_with(ResponseTemplate::new(status))
            .mount(&receiver)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/moved"))
        .respond_with(
            ResponseTemplate::new(307).insert_header("location", format!("{origin}/elsewhere")),
        )
        .mount(&receiver)
        .await;
    let listed = origin.clone();
    let mut api = Api::start_with(1, 149, LENIENT_FAST, move |config| {
        quick(config);
        // The receiver is the operator's own, on this machine.
        config.webhooks.allowed_origins = vec![listed.clone()];
        config.webhooks.private_origins = vec![listed];
    })
    .await;
    pages(&api, "/w", page("A page")).await;
    let token = api.key(&[Scope::Jobs]).await;
    let job_to = |at: &str, progress: Option<u32>| {
        let mut webhook = json!({ "url": format!("{origin}{at}"), "secret": SECRET });
        if let Some(every) = progress {
            webhook["progress_every"] = json!(every);
        }
        json!({
            "type": "batch",
            "urls": urls("/w", 3),
            "defaults": { "render": "http" },
            "webhook": webhook,
        })
    };

    // The sender's clock is the test's: the schedule is observed by moving
    // it. The worker's is the machine's.
    let clock = Arc::new(ManualClock::new(Utc::now() + chrono::Duration::minutes(1)));
    let key = api.lab.config.secrets.master_key_bytes().unwrap();
    let sender = WebhookSender::new(
        api.lab.store().clone(),
        api.lab.config.webhooks.clone(),
        CredentialCipher::new(&key),
        clock.clone(),
        Arc::new(DirectTransport::system()),
    );
    let (stop_sender, mut stopped) = tokio::sync::watch::channel(false);
    let sending = tokio::spawn(Arc::clone(&sender).run(async move {
        let _ = stopped.wait_for(|stop| *stop).await;
    }));
    let work = Background::worker_only(&api.state);

    // --- a receiver that answers ------------------------------------------
    let reply = api
        .post(&token, "/v1/jobs", &job_to("/hooks", Some(1)))
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
    let accepted = reply.json();
    let id = accepted["id"].as_str().unwrap().to_owned();
    // Of the webhook the job shows its receiver's host, never the secret.
    assert_eq!(accepted["webhook"]["host"], "127.0.0.1");
    assert_eq!(accepted["webhook"]["progress_every"], 1);
    assert!(!reply.json().to_string().contains(SECRET));
    api.job_until(&token, &id, ended).await;
    let done = api
        .job_until(&token, &id, |job| {
            job["webhook"]["deliveries"]
                .as_array()
                .is_some_and(|deliveries| {
                    deliveries.iter().any(|delivery| {
                        delivery["type"] == "job.completed" && delivery["state"] == "delivered"
                    })
                })
        })
        .await;
    let got = delivered(&receiver, "/hooks").await;
    let (last, earlier) = got.split_last().unwrap();
    assert_eq!(hook_header(last, "x-kage-event"), "job.completed");
    for request in earlier {
        assert_eq!(hook_header(request, "x-kage-event"), "job.progress");
    }
    let mut ids = Vec::new();
    for request in &got {
        let body = String::from_utf8(request.body.clone()).unwrap();
        let timestamp = hook_header(request, "x-kage-timestamp");
        // The signature is what the documentation says it is.
        assert_eq!(
            hook_header(request, "x-kage-signature"),
            format!(
                "sha256={}",
                hex_hmac(SECRET, &format!("{timestamp}.{body}"))
            )
        );
        assert_eq!(timestamp.parse::<i64>().unwrap(), clock.now().timestamp());
        assert!(kage_api::jobs::webhook::verify(
            SECRET.as_bytes(),
            &timestamp,
            &body,
            &hook_header(request, "x-kage-signature"),
            clock.now(),
            Duration::from_mins(5),
        ));
        assert_eq!(hook_header(request, "content-type"), "application/json");
        assert!(hook_header(request, "user-agent").starts_with("kage/"));
        // The event is thin: the job as its key sees it, and nothing of
        // what was fetched or by whom.
        let event: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(event["type"], hook_header(request, "x-kage-event"));
        assert_eq!(event["id"], hook_header(request, "x-kage-delivery"));
        assert_eq!(event["job"]["id"], id);
        assert!(event["created_at"].is_string());
        assert!(!body.contains("/w/1") && !body.contains("persona") && !body.contains(SECRET));
        ids.push(hook_header(request, "x-kage-delivery"));
    }
    let event: Value = serde_json::from_slice(&last.body).unwrap();
    assert_eq!(event["job"]["state"], "succeeded");
    assert_eq!(event["job"]["items"]["succeeded"], 3);
    ids.dedup();
    assert_eq!(ids.len(), got.len(), "every event has an id of its own");
    // The delivery log, on the job.
    let log = done["webhook"]["deliveries"].as_array().unwrap();
    let completed = log
        .iter()
        .find(|delivery| delivery["type"] == "job.completed")
        .unwrap();
    assert_eq!(completed["attempts"], 1);
    assert_eq!(completed["last_status"], 200);
    assert_eq!(completed["next_attempt_at"], Value::Null);

    // --- a receiver that is down ------------------------------------------
    let reply = api.post(&token, "/v1/jobs", &job_to("/down", None)).await;
    let down = reply.json()["id"].as_str().unwrap().to_owned();
    api.job_until(&token, &down, ended).await;
    until_delivered(&receiver, "/down", 1).await;
    let delivery_of = |job: &Value| job["webhook"]["deliveries"][0].clone();
    let steps = [30_u64, 120, 480, 1920, 7680];
    let mut signatures = Vec::new();
    for (retry, base) in (1_u32..).zip(steps) {
        let made = usize::try_from(retry).unwrap();
        let job = api
            .job_until(&token, &down, |job| delivery_of(job)["attempts"] == retry)
            .await;
        let delivery = delivery_of(&job);
        assert_eq!(delivery["type"], "job.completed");
        assert_eq!(delivery["state"], "pending", "{delivery}");
        assert_eq!(delivery["last_status"], 500);
        let next: DateTime<Utc> = delivery["next_attempt_at"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        let wait = (next - clock.now()).to_std().unwrap();
        // The event's own wait: the schedule moved by up to a fifth.
        let delivery_id: WebhookDeliveryId = delivery["id"].as_str().unwrap().parse().unwrap();
        let own = retry_wait(&api.lab.config.webhooks, delivery_id, retry, None);
        assert!(
            wait.abs_diff(own) < Duration::from_millis(1),
            "retry {retry}: {wait:?} against {own:?}"
        );
        let base = Duration::from_secs(base);
        assert!(
            wait >= base.mul_f64(0.8) && wait <= base.mul_f64(1.2),
            "retry {retry}: {wait:?} around {base:?}"
        );
        // A second before it is due nothing is sent, however often the
        // sender looks.
        clock.advance(
            chrono::Duration::from_std(wait.checked_sub(Duration::from_secs(1)).unwrap()).unwrap(),
        );
        sender.wake();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(delivered(&receiver, "/down").await.len(), made);
        // When it is due, it is.
        clock.advance(chrono::Duration::milliseconds(1001));
        sender.wake();
        let got = until_delivered(&receiver, "/down", made + 1).await;
        // The same event, signed anew at the time of the attempt.
        let latest = got.last().unwrap();
        assert_eq!(
            hook_header(latest, "x-kage-delivery"),
            delivery["id"].as_str().unwrap()
        );
        assert_eq!(
            hook_header(latest, "x-kage-timestamp")
                .parse::<i64>()
                .unwrap(),
            clock.now().timestamp()
        );
        signatures.push(hook_header(latest, "x-kage-signature"));
    }
    signatures.dedup();
    assert_eq!(signatures.len(), 5);
    // Five retries and no more: the delivery failed, and the log says so.
    let job = api
        .job_until(&token, &down, |job| delivery_of(job)["state"] == "failed")
        .await;
    let delivery = delivery_of(&job);
    assert_eq!(delivery["attempts"], 6);
    assert_eq!(delivery["next_attempt_at"], Value::Null);
    assert_eq!(delivery["last_status"], 500);
    clock.advance(chrono::Duration::hours(12));
    sender.wake();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(delivered(&receiver, "/down").await.len(), 6);
    // The job itself is none the worse for it.
    assert_eq!(job["state"], "succeeded");

    // --- a receiver that is gone, and one that points elsewhere -----------
    let reply = api.post(&token, "/v1/jobs", &job_to("/gone", None)).await;
    let gone = reply.json()["id"].as_str().unwrap().to_owned();
    let job = api
        .job_until(&token, &gone, |job| delivery_of(job)["state"] == "failed")
        .await;
    assert_eq!(delivery_of(&job)["attempts"], 1);
    assert_eq!(delivery_of(&job)["last_status"], 410);
    assert_eq!(delivered(&receiver, "/gone").await.len(), 1);

    // After a failure the receiver is left alone for a while: the next
    // event waits behind it without spending an attempt.
    let reply = api.post(&token, "/v1/jobs", &job_to("/moved", None)).await;
    let moved = reply.json()["id"].as_str().unwrap().to_owned();
    api.job_until(&token, &moved, ended).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(delivered(&receiver, "/moved").await.len(), 0);
    let job = api.get(&token, &format!("/v1/jobs/{moved}")).await.json();
    assert_eq!(delivery_of(&job)["attempts"], 0);
    clock.advance(chrono::Duration::hours(3));
    sender.wake();
    until_delivered(&receiver, "/moved", 1).await;
    // A redirect is a failure: nobody checked where it points.
    let job = api
        .job_until(&token, &moved, |job| delivery_of(job)["attempts"] == 1)
        .await;
    assert_eq!(delivery_of(&job)["last_status"], 307);
    assert_eq!(delivery_of(&job)["state"], "pending");
    assert_eq!(delivered(&receiver, "/elsewhere").await.len(), 0);

    let _ = stop_sender.send(true);
    sending.await.unwrap();
    work.stop().await;
    api.shut_down().await;
}

#[tokio::test]
async fn a_fetch_that_overflowed_is_a_job_and_its_result_is_there_after_a_restart() {
    // One block pauses the site for a few seconds.
    let rules = "defaults: { render: http, pacing: fast, active_hours: ignore, \
                 breaker_threshold: 1, breaker_pause: 3s, retries: 2 }\ndomains: {}\n";
    let mut api = Api::start_with(2, 149, rules, |config| {
        quick(config);
        config.scheduler.sync_wall_time = HumanDuration::new(Duration::from_millis(300));
    })
    .await;
    let token = api.key(&[Scope::Fetch, Scope::Jobs]).await;
    // The first visitor is turned away, slowly; whoever comes later is
    // served.
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_delay(Duration::from_millis(900))
                .set_body_raw(
                    "<html><head><title>Attention Required! | Cloudflare</title></head>\
                     <body><h1>Sorry, you have been blocked</h1>\
                     <p>You are unable to access this site. Cloudflare Ray ID: 8f1</p></body></html>",
                    "text/html; charset=utf-8",
                ),
        )
        .up_to_n_times(1)
        .mount(&api.lab.site)
        .await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(page("Slow"))
        .mount(&api.lab.site)
        .await;

    let reply = api
        .post(
            &token,
            "/v1/fetch",
            &json!({ "url": Api::url("/slow"), "render": "http" }),
        )
        .await;

    // It outlasted the wall time: a job carries it on from here.
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{:?}", reply.json());
    let accepted = reply.json();
    let request = accepted["id"].as_str().unwrap().to_owned();
    let job = accepted["job_id"].as_str().unwrap().to_owned();
    let seen = api.get(&token, &format!("/v1/jobs/{job}")).await.json();
    assert_eq!(seen["type"], "batch");
    assert_eq!(seen["origin"], "sync_overflow");
    assert_eq!(seen["items"]["total"], 1);
    assert_eq!(seen["visitors"], Value::Null);
    let waiting = api.get(&token, &format!("/v1/results/{request}")).await;
    assert_eq!(waiting.status, StatusCode::ACCEPTED);
    assert_eq!(waiting.json()["job_id"], job);

    // Its attempt was blocked and the site is paused: the fetch is not
    // answered with an error, it waits as a job waits, in the store.
    let paused = api
        .job_until(&token, &job, |job| job["state"] == "paused")
        .await;
    assert_eq!(paused["paused"]["reason"], "domain_paused");
    assert!(paused["paused"]["until"].is_string());
    let items = api.job_items(&token, &job).await;
    assert_eq!(items[0]["state"], "queued");
    assert_eq!(items[0]["request_id"], request);
    assert!(items[0]["not_before"].is_string());
    assert_eq!(asked(&api, "/slow").await.len(), 1);
    let waiting = api.get(&token, &format!("/v1/results/{request}")).await;
    assert_eq!(waiting.status, StatusCode::ACCEPTED);

    // The server is gone and another one starts over the same store, with
    // nothing in memory: its worker takes the job up when the pause is
    // over, under the same request.
    let restarted = api.restarted_state();
    let router = kage_api::router(restarted.clone());
    let work = Background::start(&restarted, Arc::new(DirectTransport::system()));
    let mut done = Value::Null;
    for _ in 0..400 {
        done = ask(
            &router,
            common::request(Method::GET, &format!("/v1/jobs/{job}"), Some(&token), None),
        )
        .await
        .json();
        if ended(&done) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(done["state"], "succeeded", "{done}");
    let result = ask(
        &router,
        common::request(
            Method::GET,
            &format!("/v1/results/{request}"),
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(result.status, StatusCode::OK, "{:?}", result.json());
    let result = result.json();
    assert_eq!(result["state"], "finished");
    assert_eq!(result["outcome"], "ok");
    assert_eq!(result["id"], request);
    assert!(
        result["raw_body"]
            .as_str()
            .unwrap()
            .contains("<h1>Slow</h1>")
    );
    // Both attempts are the request's: the one that was blocked before the
    // restart and the one that was served after it, by another household.
    let attempts = api
        .lab
        .store()
        .attempts()
        .list_for_request(request.parse().unwrap())
        .await
        .unwrap();
    assert_eq!(attempts.len(), 2, "{attempts:?}");
    assert!(attempts[0].outcome.is_block());
    assert!(attempts[1].outcome.is_success());
    assert_ne!(attempts[0].persona_id, attempts[1].persona_id);
    assert_eq!(asked(&api, "/slow").await.len(), 2);
    let items = api.job_items(&token, &job).await;
    assert_eq!(items[0]["state"], "succeeded");
    assert_eq!(items[0]["request_id"], request);
    work.stop().await;
    api.shut_down().await;
}

#[tokio::test]
async fn a_job_waits_where_it_must_and_ends_when_it_may_wait_no_longer() {
    let mut api = Api::start_with(2, 149, LENIENT_FAST, quick).await;
    pages(&api, "/d", page("A page")).await;
    // Two pages a day.
    let (key, token) = api
        .key_with(
            &[Scope::Jobs, Scope::Sessions],
            KeyLimits {
                daily_page_budget: Some(2),
                ..common::ROOMY
            },
        )
        .await;
    let batch = json!({ "type": "batch", "urls": urls("/d", 3), "defaults": { "render": "http" } });
    let reply = api.post(&token, "/v1/jobs", &batch).await;
    let id = reply.json()["id"].as_str().unwrap().to_owned();

    let work = api.work();
    // The third page is not failed for the budget: it waits for the day
    // to turn, and the job with it, in nobody's hands.
    let paused = api
        .job_until(&token, &id, |job| job["state"] == "paused")
        .await;
    assert_eq!(paused["paused"]["reason"], "budget_exhausted");
    let until: DateTime<Utc> = paused["paused"]["until"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        until,
        (Utc::now().date_naive() + chrono::Days::new(1))
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
    );
    assert_eq!(
        paused["items"],
        json!({ "total": 3, "succeeded": 2, "failed": 0, "cancelled": 0, "pending": 1 })
    );
    let items = api.job_items(&token, &id).await;
    assert_eq!(items[2]["state"], "queued");
    assert_eq!(items[2]["request_id"], Value::Null);
    assert_eq!(asked(&api, "/d/").await, ["/d/1", "/d/2"]);

    // A job may wait only so long (`jobs.max_age`): past its deadline it
    // ends with what it has, and what is left says what it waited for.
    sqlx::query("UPDATE jobs SET deadline = now() - interval '1 second' WHERE id = $1::uuid")
        .bind(&id)
        .execute(api.lab.store().pool())
        .await
        .unwrap();
    let done = api.job_until(&token, &id, ended).await;
    assert_eq!(done["state"], "partial", "{done}");
    assert_eq!(done["items"]["failed"], 1);
    let items = api.job_items(&token, &id).await;
    assert_eq!(items[2]["state"], "failed");
    assert_eq!(items[2]["error"], "expired: budget_exhausted");
    assert_eq!(asked(&api, "/d/").await.len(), 2);

    // A job whose key was disabled meanwhile fetches nothing.
    let reply = api
        .post(
            &token,
            "/v1/jobs",
            &json!({ "type": "batch", "urls": urls("/d", 2) }),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let orphan = reply.json()["id"].as_str().unwrap().to_owned();
    work.stop().await;
    sqlx::query("UPDATE api_keys SET disabled_at = now() WHERE id = $1")
        .bind(key.as_uuid())
        .execute(api.lab.store().pool())
        .await
        .unwrap();
    let work = api.work();
    let mut state = String::new();
    for _ in 0..200 {
        state = sqlx::query_scalar("SELECT state FROM jobs WHERE id = $1::uuid")
            .bind(&orphan)
            .fetch_one(api.lab.store().pool())
            .await
            .unwrap();
        if state == "failed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(state, "failed");
    let errors: Vec<Option<String>> =
        sqlx::query_scalar("SELECT error FROM job_items WHERE job_id = $1::uuid")
            .bind(&orphan)
            .fetch_all(api.lab.store().pool())
            .await
            .unwrap();
    assert_eq!(
        errors,
        [
            Some("key_disabled".to_owned()),
            Some("key_disabled".to_owned())
        ]
    );
    assert_eq!(asked(&api, "/d/").await.len(), 2);
    work.stop().await;
    api.shut_down().await;
}

#[tokio::test]
async fn a_job_in_a_session_is_read_by_the_sessions_persona() {
    let mut api = Api::start_with(3, 149, LENIENT_FAST, quick).await;
    pages(&api, "/s", page("A page")).await;
    let token = api.key(&[Scope::Jobs, Scope::Sessions, Scope::Fetch]).await;
    let batch = |session: &str| {
        json!({
            "type": "batch",
            "urls": urls("/s", 4),
            "defaults": { "render": "http" },
            "session_id": session,
        })
    };

    // A session that is not open is no session.
    api.post(&token, "/v1/jobs", &batch("proj-9"))
        .await
        .problem(StatusCode::NOT_FOUND, "session_not_found");
    // One whose persona an item cannot use is a conflict, and names the
    // item.
    let opened = api
        .post(&token, "/v1/sessions", &json!({ "id": "proj-9" }))
        .await;
    assert_eq!(opened.status, StatusCode::CREATED, "{:?}", opened.json());
    let mut picky = batch("proj-9");
    picky["items"] = json!([{ "url": Api::url("/s/9"), "persona": { "engines": ["firefox"] } }]);
    let conflict = api.post(&token, "/v1/jobs", &picky).await;
    let problem = conflict.problem(StatusCode::CONFLICT, "session_persona_conflict");
    assert_eq!(problem["item"], 4);
    assert_eq!(count(&api, "jobs").await, 0);

    let reply = api.post(&token, "/v1/jobs", &batch("proj-9")).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
    let id = reply.json()["id"].as_str().unwrap().to_owned();
    assert_eq!(reply.json()["session_id"], "proj-9");
    let work = api.work();
    let done = api.job_until(&token, &id, ended).await;
    assert_eq!(done["state"], "succeeded", "{done}");

    // Every page by the session's persona, in the session.
    let fetched = api
        .post(
            &token,
            "/v1/fetch",
            &json!({ "url": Api::url("/s/5"), "render": "http", "session_id": "proj-9" }),
        )
        .await
        .json();
    let own = personas_of(&api, &json!({ "request_id": fetched["id"] })).await;
    let items = api.job_items(&token, &id).await;
    for item in &items {
        assert_eq!(item["result"]["session_id"], "proj-9", "{item}");
        assert_eq!(personas_of(&api, item).await, own);
    }
    work.stop().await;
    api.shut_down().await;
}

#[tokio::test]
async fn an_admin_purges_a_keys_data_and_retention_deletes_what_is_old() {
    let mut api = Api::start_with(2, 149, LENIENT_FAST, quick).await;
    pages(&api, "/x", page("A page")).await;
    let admin = api.key(&[Scope::Admin]).await;
    let (mine, token) = api
        .key_with(&[Scope::Jobs, Scope::Fetch], common::ROOMY)
        .await;
    let (_, theirs) = api
        .key_with(&[Scope::Jobs, Scope::Fetch], common::ROOMY)
        .await;
    let fetch = |path: &str| json!({ "url": Api::url(path), "render": "http" });
    let batch = json!({ "type": "batch", "urls": urls("/x", 2), "defaults": { "render": "http" } });

    let work = api.work();
    let single = api.post(&token, "/v1/fetch", &fetch("/x/10")).await.json();
    let kept = api.post(&theirs, "/v1/fetch", &fetch("/x/11")).await.json();
    let job = api.post(&token, "/v1/jobs", &batch).await.json();
    let job = job["id"].as_str().unwrap().to_owned();
    api.job_until(&token, &job, ended).await;
    let other_job = api.post(&theirs, "/v1/jobs", &batch).await.json();
    let other_job = other_job["id"].as_str().unwrap().to_owned();
    api.job_until(&theirs, &other_job, ended).await;
    let item = api.job_items(&token, &job).await[0]["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(count(&api, "requests").await, 6);
    let artifacts = count(&api, "artifacts").await;
    assert!(artifacts >= 6, "{artifacts}");

    // Only an admin, only for a key there is, and never for the future.
    api.post(
        &token,
        "/v1/admin/purge",
        &json!({ "key_id": mine.to_string() }),
    )
    .await
    .problem(StatusCode::FORBIDDEN, "scope_missing");
    api.post(
        &admin,
        "/v1/admin/purge",
        &json!({ "key_id": uuid::Uuid::now_v7().to_string() }),
    )
    .await
    .problem(StatusCode::NOT_FOUND, "key_not_found");
    api.post(
        &admin,
        "/v1/admin/purge",
        &json!({ "key_id": mine.to_string(), "before": Utc::now() + chrono::Duration::hours(1) }),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed");
    // Before anything of the key existed: nothing goes.
    let nothing = api
        .post(
            &admin,
            "/v1/admin/purge",
            &json!({ "key_id": mine.to_string(), "before": Utc::now() - chrono::Duration::hours(1) }),
        )
        .await;
    assert_eq!(nothing.status, StatusCode::OK, "{:?}", nothing.json());
    assert_eq!(
        nothing.json(),
        json!({ "requests": 0, "jobs": 0, "artifacts": 0 })
    );

    let purged = api
        .post(
            &admin,
            "/v1/admin/purge",
            &json!({ "key_id": mine.to_string() }),
        )
        .await;

    assert_eq!(purged.status, StatusCode::OK, "{:?}", purged.json());
    let purged = purged.json();
    assert_eq!(purged["requests"], 3);
    assert_eq!(purged["jobs"], 1);
    assert!(purged["artifacts"].as_u64().unwrap() >= 3, "{purged}");
    // The key's results and its job are gone; the other key's are not.
    for gone in [single["id"].as_str().unwrap(), item.as_str()] {
        api.get(&token, &format!("/v1/results/{gone}"))
            .await
            .problem(StatusCode::NOT_FOUND, "result_not_found");
    }
    api.get(&token, &format!("/v1/jobs/{job}"))
        .await
        .problem(StatusCode::NOT_FOUND, "job_not_found");
    let theirs_result = api
        .get(
            &theirs,
            &format!("/v1/results/{}", kept["id"].as_str().unwrap()),
        )
        .await;
    assert_eq!(theirs_result.status, StatusCode::OK);
    assert_eq!(
        api.get(&theirs, &format!("/v1/jobs/{other_job}"))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(count(&api, "requests").await, 3);
    assert_eq!(count(&api, "jobs").await, 1);
    // It is on the audit log, with what went.
    let audit = api.get(&admin, "/v1/admin/audit?limit=100").await;
    assert_eq!(audit.status, StatusCode::OK, "{:?}", audit.json());
    let events = audit.json();
    let events = events["events"].as_array().unwrap();
    let logged: Vec<&Value> = events
        .iter()
        .filter(|event| event["action"] == "admin.data.purge")
        .collect();
    // Every call is on the log, the refused ones too.
    assert!(logged.len() >= 4, "{events:?}");
    let done: Vec<&&Value> = logged
        .iter()
        .filter(|event| event["details"]["result"]["requests"] == 3)
        .collect();
    assert_eq!(done.len(), 1, "{logged:?}");
    assert_eq!(done[0]["target"], mine.to_string());
    assert_eq!(done[0]["outcome"], "success");
    assert_eq!(done[0]["details"]["result"]["jobs"], 1);

    // The retention sweep: finished jobs older than `jobs.retention_days`
    // go with everything of theirs, and results that are no job's when
    // they are as old as the artifacts may get.
    let soon = Utc::now() + chrono::Duration::days(1);
    assert_eq!(
        kage_api::jobs::sweep(&api.state, soon).await.unwrap(),
        (0, 0)
    );
    let later = Utc::now() + chrono::Duration::days(15);
    let (jobs, requests) = kage_api::jobs::sweep(&api.state, later).await.unwrap();
    assert_eq!((jobs, requests), (1, 3));
    assert_eq!(count(&api, "jobs").await, 0);
    assert_eq!(count(&api, "job_items").await, 0);
    assert_eq!(count(&api, "requests").await, 0);
    assert_eq!(count(&api, "artifacts").await, 0);
    work.stop().await;
    api.shut_down().await;
}
