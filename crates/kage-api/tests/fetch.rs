//! `POST /v1/fetch`, `GET /v1/results/{id}` and `GET /v1/artifacts/{id}`
//! over both fetch paths (spec §16.3 to §16.5, §19): the router of `kage
//! serve` over the scheduler's lab.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::sched::lab::{LENIENT_FAST, article};
use common::{Api, ask, request};
use kage_core::domain::Scope;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

/// Every request goes to the browser, and a persona may leave a page
/// after half a second.
const BROWSER_FAST: &str = "defaults: { render: browser, pacing: fast, active_hours: ignore, min_dwell: 500ms }\ndomains: {}\n";

fn page(title: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(article(title), "text/html; charset=utf-8")
}

async fn serve(api: &Api, at: &str, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(response)
        .mount(&api.lab.site)
        .await;
}

fn header<'a>(result: &'a Value, name: &str) -> Option<&'a str> {
    result["headers"]
        .as_array()?
        .iter()
        .find(|header| header["name"].as_str() == Some(name))
        .and_then(|header| header["value"].as_str())
}

fn site_header(request: &wiremock::Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[tokio::test]
async fn a_page_is_fetched_on_the_http_path_and_found_again_by_its_id() {
    let mut api = Api::start(2, 149, LENIENT_FAST).await;
    serve(
        &api,
        "/page",
        page("The shop").insert_header("set-cookie", "sid=abc123; Path=/; HttpOnly"),
    )
    .await;
    let (key, token) = api.key_with(&[Scope::Fetch], common::ROOMY).await;
    let other = api.key(&[Scope::Fetch]).await;
    let admin = api.key(&[Scope::Fetch, Scope::Admin]).await;

    let reply = api
        .post(
            &token,
            "/v1/fetch",
            &json!({ "url": Api::url("/page"), "render": "http" }),
        )
        .await;

    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    let result = reply.json();
    assert_eq!(result["state"], "finished");
    assert_eq!(result["outcome"], "ok");
    assert_eq!(result["status"], 200);
    assert_eq!(result["final_url"], Api::url("/page"));
    assert_eq!(result["escalated"], false);
    assert_eq!(result["cached"], false);
    // The request's id is the result's, and the answer's header.
    let id = result["id"].as_str().unwrap().to_owned();
    assert_eq!(reply.header("x-request-id"), Some(id.as_str()));
    // An http render returns the body and the headers unless told
    // otherwise; it has no document.
    assert!(
        result["raw_body"]
            .as_str()
            .unwrap()
            .contains("<h1>The shop</h1>")
    );
    assert_eq!(result["dom_html"], Value::Null);
    assert_eq!(
        header(&result, "content-type"),
        Some("text/html; charset=utf-8")
    );
    // What the site's cookie holds is not the caller's to have.
    assert_eq!(
        header(&result, "set-cookie"),
        Some("sid=[redacted]; Path=/; HttpOnly")
    );
    assert!(!String::from_utf8_lossy(&reply.body).contains("abc123"));

    // The persona is a handle of this key's, with what a caller may know
    // of it; nothing names a proxy, an address or the persona's real id.
    let persona = &result["persona"];
    let handle = persona["id"].as_str().unwrap();
    assert!(handle.starts_with("p_") && handle.len() == 18, "{handle}");
    assert_eq!(persona["engine"], "chromium");
    assert_eq!(persona["claimed_os"], "linux");
    assert_eq!(persona["country"], "DE");
    let [attempt] = result["attempts"].as_array().unwrap().as_slice() else {
        panic!("one attempt, not {}", result["attempts"]);
    };
    assert_eq!(attempt["n"], 1);
    assert_eq!(attempt["render"], "http");
    assert_eq!(attempt["outcome"], "ok");
    assert_eq!(attempt["persona"], handle);
    assert!(attempt.get("seed").is_none());
    let text = String::from_utf8_lossy(&reply.body).into_owned();
    for persona in &api.lab.personas {
        assert!(!text.contains(&persona.id.to_string()), "{text}");
    }
    assert!(
        !text.contains("proxy") && !text.contains("198.51."),
        "{text}"
    );
    // The pacing that applied is said; the http path has no behaviour.
    assert_eq!(result["pacing"]["applied"], "fast");
    assert_eq!(result["pacing"]["request_ignored"], false);
    assert_eq!(result["behavior"], Value::Null);

    // It was fetched by a persona, through its proxy, once.
    let received = api.lab.received("/page").await;
    assert_eq!(received.len(), 1);
    assert!(
        site_header(&received[0], "user-agent")
            .unwrap()
            .contains("Chrome/149.")
    );

    // The body was kept, and is served as what the site sent, never as a
    // page of this origin.
    let artifact = result["artifacts"]["raw_body"].as_str().unwrap();
    assert!(artifact.starts_with("art_"), "{artifact}");
    let kept = api.get(&token, &format!("/v1/artifacts/{artifact}")).await;
    assert_eq!(kept.status, StatusCode::OK);
    assert_eq!(kept.body, article("The shop").as_bytes());
    assert_eq!(
        kept.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(kept.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(
        kept.header("content-security-policy"),
        Some("sandbox; default-src 'none'")
    );
    assert!(kept.header("etag").is_some());
    // A client that takes gzip gets the stored bytes as they are.
    let mut packed = request(
        Method::GET,
        &format!("/v1/artifacts/{artifact}"),
        Some(&token),
        None,
    );
    packed
        .headers_mut()
        .insert("accept-encoding", "gzip, br".parse().unwrap());
    let packed = api.send(packed).await;
    assert_eq!(packed.status, StatusCode::OK);
    assert_eq!(packed.header("content-encoding"), Some("gzip"));
    assert!(packed.body.len() < kept.body.len());
    assert_eq!(&packed.body[..2], [0x1f, 0x8b]);

    // The result is found again by the request's id, with what is stored.
    let again = api.get(&token, &format!("/v1/results/{id}")).await;
    assert_eq!(again.status, StatusCode::OK, "{:?}", again.json());
    let stored = again.json();
    for member in [
        "id",
        "state",
        "outcome",
        "status",
        "final_url",
        "headers",
        "raw_body",
        "artifacts",
        "persona",
        "attempts",
        "escalated",
    ] {
        assert_eq!(stored[member], result[member], "{member}");
    }
    // What only the synchronous answer has.
    assert_eq!(stored["pacing"], Value::Null);
    assert_eq!(stored["behavior"], Value::Null);

    // Another key sees neither, as if they were not there; one with the
    // admin scope sees both, and the persona as it is.
    api.get(&other, &format!("/v1/results/{id}"))
        .await
        .problem(StatusCode::NOT_FOUND, "result_not_found");
    api.get(&other, &format!("/v1/artifacts/{artifact}"))
        .await
        .problem(StatusCode::NOT_FOUND, "artifact_not_found");
    let seen = api.get(&admin, &format!("/v1/results/{id}")).await.json();
    let real = seen["persona"]["id"].as_str().unwrap();
    assert!(
        api.lab
            .personas
            .iter()
            .any(|persona| persona.id.to_string() == real),
        "{real}"
    );
    assert!(seen["attempts"][0]["seed"].is_string());
    assert_eq!(
        api.get(&admin, &format!("/v1/artifacts/{artifact}"))
            .await
            .status,
        StatusCode::OK
    );
    // What is not there.
    api.get(&token, "/v1/results/0190a0b0-0000-7000-8000-000000000001")
        .await
        .problem(StatusCode::NOT_FOUND, "result_not_found");
    api.get(&token, "/v1/results/nothing")
        .await
        .problem(StatusCode::NOT_FOUND, "result_not_found");
    api.get(&token, "/v1/artifacts/art_nothing")
        .await
        .problem(StatusCode::NOT_FOUND, "artifact_not_found");

    // The request is on record under the key, with what was asked.
    let record = api
        .lab
        .store()
        .requests()
        .get(id.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.api_key_id, Some(key));
    assert_eq!(record.spec["render"], "http");
    api.shut_down().await;
}

#[tokio::test]
async fn a_page_is_fetched_in_a_browser_with_what_was_asked_for() {
    let mut api = Api::start(2, 155, BROWSER_FAST).await;
    let token = api.key(&[Scope::Fetch]).await;
    let admin = api.key(&[Scope::Fetch, Scope::Admin]).await;
    // A screenshot the sidecar wrote below the artifacts directory.
    let shot = b"RIFF\x10\x00\x00\x00WEBPVP8 not really a picture".to_vec();
    let dir = api.lab.config.artifacts.dir.join("sidecar/ctx");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("shot.webp"), &shot).unwrap();
    let url = Api::url("/listing");
    api.lab.chromium.with(|sidecar| {
        sidecar.pages.insert(
            url.clone(),
            json!({
                "title": "Listing",
                "dom_html": article("Listing"),
                "headers": [
                    { "name": "content-type", "value": "text/html; charset=utf-8" },
                    { "name": "set-cookie", "value": "cart=xyz789; Secure" },
                ],
                "xhr": [{
                    "url": format!("{url}/api/items"),
                    "method": "GET",
                    "kind": "fetch",
                    "status": 200,
                    "content_type": "application/json",
                    "body": { "bytes": 17, "truncated": false, "encoding": "utf8", "data": "{\"items\":[1,2,3]}" },
                }],
                "xhr_dropped": 1,
                "console": [{ "level": "error", "source": "network", "text": "Failed to load resource", "url": null }],
                "screenshot": {
                    "path": "sidecar/ctx/shot.webp",
                    "bytes": shot.len(),
                    "sha256": "0".repeat(64),
                    "content_type": "image/webp",
                },
                "side_effects": ["resize"],
            }),
        );
    });

    let reply = api
        .post(
            &token,
            "/v1/fetch",
            &json!({
                "url": url,
                "include": ["dom", "headers", "xhr", "console", "screenshot"],
                "behavior": { "scroll_depth": 0, "dwell_ms": 3000, "referer_strategy": "none" },
                "wait": { "until": "network_idle", "selector": "main", "extra_ms": 250 },
                "screenshot": { "full_page": true, "format": "webp" },
                "xhr": { "content_types": ["application/json"], "max_bytes": 100_000 },
                "timeout_ms": 60000,
                "max_retries": 0,
            }),
        )
        .await;

    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    let result = reply.json();
    assert_eq!(result["outcome"], "ok");
    assert_eq!(result["attempts"][0]["render"], "browser");
    assert!(
        result["dom_html"]
            .as_str()
            .unwrap()
            .contains("<h1>Listing</h1>")
    );
    assert_eq!(result["raw_body"], Value::Null);
    assert_eq!(
        header(&result, "set-cookie"),
        Some("cart=[redacted]; Secure")
    );
    assert_eq!(
        result["xhr"],
        json!({
            "responses": [{
                "url": format!("{url}/api/items"),
                "method": "GET",
                "kind": "fetch",
                "status": 200,
                "content_type": "application/json",
                "bytes": 17,
                "truncated": false,
                "body": "{\"items\":[1,2,3]}",
            }],
            "dropped": 1,
        })
    );
    assert_eq!(result["console"][0]["level"], "error");
    assert_eq!(result["console"][0]["text"], "Failed to load resource");
    assert_eq!(result["side_effects"], json!(["resize"]));

    // What the persona did is what the request asked for, and is said.
    let behavior = &result["behavior"];
    assert_eq!(behavior["applied"], "request");
    assert_eq!(behavior["request_ignored"], false);
    assert_eq!(behavior["scroll_depth"], 0.0);
    assert_eq!(behavior["referer_strategy"], "none");
    assert!(behavior.get("seed").is_none());
    // The sidecar was told so: no scrolling, a dwell drawn around three
    // seconds and never below the policy's floor, and the wait as given.
    let navigated = api.lab.chromium.with(|sidecar| sidecar.navigated.clone());
    let [asked] = navigated.as_slice() else {
        panic!("one navigation, not {navigated:?}");
    };
    assert_eq!(asked["url"], url);
    // (The contract's way to say "no scrolling" is to leave the depth out.)
    assert_eq!(asked["behavior"]["scroll_depth"], Value::Null, "{asked}");
    assert_eq!(asked["behavior"]["dwell"], "fixed", "{asked}");
    assert_eq!(asked["behavior"]["wander"], true, "{asked}");
    assert_eq!(asked["behavior"]["entry"]["strategy"], "none", "{asked}");
    let dwell = asked["dwell_ms"].as_u64().unwrap();
    assert!((500..=60_000).contains(&dwell), "{dwell}");
    assert_eq!(asked["wait"]["until"], "network_idle");
    assert_eq!(asked["wait"]["selector"], "main");
    assert_eq!(asked["wait"]["extra_ms"], 250);
    assert_eq!(asked["capture"]["screenshot"]["full_page"], true);
    assert_eq!(asked["capture"]["console"], true);
    assert_eq!(asked["capture"]["xhr"]["max_bytes"], 100_000);
    assert_eq!(asked["timeout_ms"], 60_000);

    // The document and the screenshot were kept; the sidecar's file was
    // taken into the store.
    let document = result["artifacts"]["dom_html"].as_str().unwrap();
    let picture = result["artifacts"]["screenshot"].as_str().unwrap();
    let kept = api.get(&token, &format!("/v1/artifacts/{picture}")).await;
    assert_eq!(kept.status, StatusCode::OK);
    assert_eq!(kept.header("content-type"), Some("image/webp"));
    assert_eq!(kept.body, shot);
    assert!(!dir.join("shot.webp").exists());
    let kept = api.get(&token, &format!("/v1/artifacts/{document}")).await;
    assert_eq!(kept.body, article("Listing").as_bytes());

    // The stored result has the document and the responses again.
    let id = result["id"].as_str().unwrap();
    let stored = api.get(&token, &format!("/v1/results/{id}")).await.json();
    assert_eq!(stored["dom_html"], result["dom_html"]);
    assert_eq!(stored["xhr"], result["xhr"]);
    assert_eq!(stored["artifacts"], result["artifacts"]);
    assert_eq!(stored["console"], Value::Null);

    // Without `include`, a browser render returns the document and the
    // headers, keeps the document, and behaves as the policy says. A key
    // with the admin scope is told the seed of what the persona did.
    let plain = api
        .post(&admin, "/v1/fetch", &json!({ "url": Api::url("/other") }))
        .await
        .json();
    assert_eq!(plain["outcome"], "ok", "{plain}");
    assert!(plain["dom_html"].is_string());
    assert_eq!(plain["xhr"], Value::Null);
    assert_eq!(plain["console"], Value::Null);
    assert_eq!(
        plain["artifacts"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["dom_html"]
    );
    assert_eq!(plain["behavior"]["applied"], "policy");
    assert!(plain["behavior"]["seed"].is_string());
    assert!(plain["attempts"][0]["seed"].is_string());
    api.shut_down().await;
}

#[tokio::test]
async fn a_policy_that_locks_behaviour_ignores_the_requests_and_says_so() {
    let rules = "defaults: { render: browser, pacing: fast, active_hours: ignore }\n\
                 domains:\n  shop.kage.test: { behavior_locked: true, pacing_locked: true }\n";
    let mut api = Api::start(1, 155, rules).await;
    let token = api.key(&[Scope::Fetch]).await;

    let result = api
        .post(
            &token,
            "/v1/fetch",
            &json!({
                "url": Api::url("/page"),
                "behavior": { "scroll_depth": 0, "dwell_ms": 100 },
                "pacing": { "profile": "human" },
            }),
        )
        .await
        .json();

    assert_eq!(result["outcome"], "ok", "{result}");
    assert_eq!(result["behavior"]["applied"], "policy");
    assert_eq!(result["behavior"]["request_ignored"], true);
    assert_eq!(result["pacing"]["applied"], "fast");
    assert_eq!(result["pacing"]["request_ignored"], true);
    // The persona scrolled as its policy has it.
    let navigated = api.lab.chromium.with(|sidecar| sidecar.navigated.clone());
    assert!(
        navigated[0]["behavior"]["scroll_depth"].as_f64().unwrap() > 0.0,
        "{}",
        navigated[0]
    );
    api.shut_down().await;
}

#[tokio::test]
async fn a_request_that_cannot_be_made_as_asked_is_refused_and_nothing_is_fetched() {
    let mut api = Api::start(1, 149, LENIENT_FAST).await;
    let token = api.key(&[Scope::Fetch, Scope::Sessions]).await;
    let page = Api::url("/page");
    let listing = Api::url("/listing");

    let cases: Vec<(Value, StatusCode, &str)> = vec![
        // The URL, before anything is looked up.
        (
            json!({ "url": "not a url" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_url",
        ),
        (
            json!({ "url": "ftp://example.org/file" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_url",
        ),
        (
            json!({ "url": "https://user:secret@example.org/" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_url",
        ),
        (
            json!({ "url": "http://example.org:0/" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_url",
        ),
        // The SSRF guard (spec §19).
        (
            json!({ "url": "http://127.0.0.1/admin" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        (
            json!({ "url": "http://10.0.0.5/" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        (
            json!({ "url": "http://192.168.1.1/" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        (
            json!({ "url": "http://169.254.169.254/latest/meta-data/" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        (
            json!({ "url": "http://[::1]/" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        (
            json!({ "url": "http://[fd00::1]/" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        (
            json!({ "url": "http://localhost/" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        (
            json!({ "url": "http://metadata.google.internal/" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        (
            json!({ "url": "http://2130706433/" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        // A public address by number, while `security.allow_ip_targets` is off.
        (
            json!({ "url": "http://93.184.216.34/" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        // The header denylist.
        (
            json!({ "url": page, "render": "http", "headers": { "User-Agent": "curl/8" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "render": "http", "headers": { "cookie": "sid=1" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "render": "http", "headers": { "Host": "other.example" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "render": "http", "headers": { "Sec-Fetch-Site": "none" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "render": "http", "headers": { "Sec-CH-UA": "x" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "render": "http", "headers": { "Accept-Language": "en" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "render": "http", "headers": { "Proxy-Authorization": "Basic eA==" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "render": "http", "headers": { "Via": "1.1 squid" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "render": "http", "headers": { "bad name": "x" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_invalid",
        ),
        (
            json!({ "url": page, "render": "http", "headers": { "X-Note": "line\nbreak" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_invalid",
        ),
        (
            json!({ "url": page, "render": "http", "headers": { "Referer": "about:blank" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_invalid",
        ),
        // What only the http path sends, asked of a browser.
        (
            json!({ "url": page, "headers": { "X-Note": "1" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "render_needs_http",
        ),
        (
            json!({ "url": page, "method": "POST", "body": "a=1" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "render_needs_http",
        ),
        (
            json!({ "url": page, "render": "browser", "method": "HEAD" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "render_needs_http",
        ),
        (
            json!({ "url": page, "request_kind": "fetch", "headers": { "Referer": listing } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "render_kind_conflict",
        ),
        // A script's request.
        (
            json!({ "url": page, "render": "http", "request_kind": "xhr" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "request_kind_needs_referer",
        ),
        (
            json!({ "url": page, "render": "http", "request_kind": "fetch", "headers": { "Referer": listing, "X-Requested-With": "XMLHttpRequest" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_not_placeable",
        ),
        (
            json!({ "url": "https://api.example.org/items", "render": "http", "request_kind": "fetch", "method": "POST", "body": "{}", "headers": { "Referer": "https://www.example.org/", "Content-Type": "application/json" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "preflight_required",
        ),
        // What a browser cannot produce on the http path.
        (
            json!({ "url": page, "render": "http", "include": ["dom"] }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "render_include_conflict",
        ),
        (
            json!({ "url": page, "render": "http", "include": ["raw", "screenshot"] }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "render_include_conflict",
        ),
        // Time.
        (
            json!({ "url": page, "timeout_ms": 5000 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "timeout_too_short",
        ),
        (
            json!({ "url": page, "render": "http", "timeout_ms": 500 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "timeout_too_short",
        ),
        (
            json!({ "url": page, "timeout_ms": 300_001 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        // Pacing and behaviour.
        (
            json!({ "url": page, "pacing": { "profile": "warp" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_pacing",
        ),
        (
            json!({ "url": page, "behavior": { "scroll_depth": 1.5 } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "behavior": { "dwell_ms": 600_000 } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "max_retries": 11 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "persona": { "countries": ["Germany"] } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        // What arrives with a later milestone is refused, not ignored.
        (
            json!({ "url": page, "include": ["extract"] }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_available_yet",
        ),
        (
            json!({ "url": page, "cache_ttl_s": 60 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_available_yet",
        ),
        (
            json!({ "url": page, "robots": "enforce" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_available_yet",
        ),
        // What the API does not take at all.
        (
            json!({ "url": page, "locale": "de-DE" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "proxy": "http://127.0.0.1:8080" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "render": "curl" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "render": "http" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        // Sessions.
        (
            json!({ "url": page, "session_id": "has space" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_session_id",
        ),
        (
            json!({ "url": page, "session_id": "never-opened" }),
            StatusCode::NOT_FOUND,
            "session_not_found",
        ),
    ];
    for (body, status, code) in cases {
        let reply = api.post(&token, "/v1/fetch", &body).await;
        assert_eq!(reply.status, status, "{body}: {:?}", reply.json());
        assert_eq!(reply.code(), code, "{body}: {:?}", reply.json());
        reply.problem(status, code);
    }
    // A refusal says what to change.
    let reply = api
        .post(
            &token,
            "/v1/fetch",
            &json!({ "url": page, "timeout_ms": 5000 }),
        )
        .await;
    let problem = reply.problem(StatusCode::UNPROCESSABLE_ENTITY, "timeout_too_short");
    assert_eq!(problem["field"], "timeout_ms");
    assert_eq!(problem["minimum_ms"], 15_000);
    let reply = api
        .post(
            &token,
            "/v1/fetch",
            &json!({ "url": page, "render": "http", "headers": { "User-Agent": "curl/8" } }),
        )
        .await;
    let problem = reply.problem(StatusCode::UNPROCESSABLE_ENTITY, "header_denied");
    assert_eq!(problem["header"], "User-Agent");
    // A body that is not JSON, or not said to be.
    let mut raw = request(Method::POST, "/v1/fetch", Some(&token), None);
    *raw.body_mut() = axum::body::Body::from("{\"url\": ");
    raw.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    api.send(raw)
        .await
        .problem(StatusCode::BAD_REQUEST, "invalid_request");
    let mut untyped = request(Method::POST, "/v1/fetch", Some(&token), None);
    *untyped.body_mut() = axum::body::Body::from(json!({ "url": page }).to_string());
    api.send(untyped)
        .await
        .problem(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type");

    // Nothing of all that reached a site, a browser or the store.
    assert!(api.lab.site.received_requests().await.unwrap().is_empty());
    assert!(
        api.lab
            .chromium
            .with(|sidecar| sidecar.navigated.is_empty())
    );
    let since = chrono::Utc::now() - chrono::Duration::hours(1);
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM requests WHERE created_at >= $1")
        .bind(since)
        .fetch_one(api.lab.store().pool())
        .await
        .unwrap();
    assert_eq!(stored, 0);
    api.shut_down().await;
}

#[tokio::test]
async fn a_redirect_to_a_denied_address_ends_the_fetch_and_returns_nothing_of_it() {
    let mut api = Api::start(2, 149, LENIENT_FAST).await;
    let token = api.key(&[Scope::Fetch]).await;
    serve(
        &api,
        "/moved",
        ResponseTemplate::new(302)
            .insert_header("location", "http://169.254.169.254/latest/meta-data/")
            .insert_header("set-cookie", "trap=1")
            .set_body_string("moved"),
    )
    .await;
    serve(
        &api,
        "/inside",
        ResponseTemplate::new(301).insert_header("location", "http://10.0.0.5:80/admin"),
    )
    .await;

    // On the http path the hop is checked before it is requested.
    for (at, range) in [("/moved", "169.254.169.254"), ("/inside", "10.0.0.5")] {
        let reply = api
            .post(
                &token,
                "/v1/fetch",
                &json!({ "url": Api::url(at), "render": "http", "include": ["raw", "headers"] }),
            )
            .await;
        // A fetch was made and ended: the answer is a result.
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
        let result = reply.json();
        assert_eq!(result["outcome"], "ssrf_denied", "{result}");
        assert_eq!(result["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(result["raw_body"], Value::Null);
        assert_eq!(result["artifacts"], json!({}));
        assert!(
            result["reason"]
                .as_str()
                .is_some_and(|reason| !reason.contains(range)),
            "{result}"
        );
        // Every proxy saw the site and nothing else.
        for proxy in &api.lab.proxies {
            for seen in proxy.plain() {
                assert!(
                    seen.target.starts_with("http://shop.kage.test/"),
                    "{}",
                    seen.target
                );
            }
        }
    }

    // In a browser the chain is known afterwards: the page is thrown
    // away, with whatever it read.
    let url = Api::url("/chain");
    api.lab.chromium.with(|sidecar| {
        sidecar.pages.insert(
            url.clone(),
            json!({
                "final_url": "http://192.168.0.1/router",
                "redirects": [{ "url": url, "status": 302, "location": "http://192.168.0.1/router" }],
                "dom_html": "<html><body>the router's admin page</body></html>",
            }),
        );
    });
    let result = api
        .post(
            &token,
            "/v1/fetch",
            &json!({ "url": url, "render": "browser", "include": ["dom", "headers", "xhr"] }),
        )
        .await
        .json();
    assert_eq!(result["outcome"], "ssrf_denied", "{result}");
    assert_eq!(result["dom_html"], Value::Null);
    assert_eq!(result["artifacts"], json!({}));
    assert!(!result.to_string().contains("admin page"));
    api.shut_down().await;
}

#[tokio::test]
async fn the_host_looks_names_up_only_when_told_to() {
    // Off (the default): a name is the proxy's to resolve. Nothing here
    // asks a resolver, so a name nobody can resolve is fetched and fails
    // as a fetch.
    let mut api = Api::start(1, 149, LENIENT_FAST).await;
    let token = api.key(&[Scope::Fetch]).await;
    let body = json!({
        "url": "http://kage-no-such-host.invalid/",
        "render": "http",
        "max_retries": 0,
    });
    let reply = api.post(&token, "/v1/fetch", &body).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    assert_ne!(reply.json()["outcome"], "ok");
    assert_ne!(reply.json()["outcome"], "ssrf_denied");
    api.shut_down().await;

    // On: the name is looked up here first, and one that resolves to
    // nothing is refused before a persona is asked.
    let mut api = Api::start_with(1, 149, LENIENT_FAST, |config| {
        config.security.resolve_on_host = true;
    })
    .await;
    let token = api.key(&[Scope::Fetch]).await;
    let reply = api.post(&token, "/v1/fetch", &body).await;
    let problem = reply.problem(StatusCode::UNPROCESSABLE_ENTITY, "ssrf_denied");
    assert_eq!(problem["field"], "url");
    for proxy in &api.lab.proxies {
        assert!(proxy.seen().is_empty());
    }
    api.shut_down().await;
}

#[tokio::test]
async fn a_fetch_that_outlasts_the_wall_time_is_answered_202_and_its_result_comes_later() {
    let mut api = Api::start_with(1, 149, LENIENT_FAST, |config| {
        config.scheduler.sync_wall_time =
            kage_core::duration::HumanDuration::new(Duration::from_millis(300));
    })
    .await;
    let token = api.key(&[Scope::Fetch]).await;
    serve(
        &api,
        "/slow",
        page("Slow").set_delay(Duration::from_millis(1500)),
    )
    .await;

    let reply = api
        .post(
            &token,
            "/v1/fetch",
            &json!({ "url": Api::url("/slow"), "render": "http" }),
        )
        .await;

    assert_eq!(reply.status, StatusCode::ACCEPTED, "{:?}", reply.json());
    let accepted = reply.json();
    let id = accepted["id"].as_str().unwrap().to_owned();
    assert_eq!(accepted["state"], "running");
    // The job that would carry it on arrives with milestone M9.
    assert_eq!(accepted["job_id"], Value::Null);
    assert_eq!(accepted["result"], format!("/v1/results/{id}"));
    assert_eq!(
        reply.header("location"),
        Some(format!("/v1/results/{id}").as_str())
    );
    assert_eq!(reply.header("retry-after"), Some("5"));
    assert_eq!(reply.header("x-request-id"), Some(id.as_str()));

    // While it runs the result is not there yet.
    let waiting = api.get(&token, &format!("/v1/results/{id}")).await;
    assert_eq!(waiting.status, StatusCode::ACCEPTED);
    assert_eq!(waiting.json()["state"], "running");
    assert_eq!(waiting.header("retry-after"), Some("5"));

    // It ran on, and its result was stored.
    let mut result = Value::Null;
    for _ in 0..100 {
        let reply = api.get(&token, &format!("/v1/results/{id}")).await;
        if reply.status == StatusCode::OK {
            result = reply.json();
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(result["state"], "finished", "{result}");
    assert_eq!(result["outcome"], "ok");
    assert!(
        result["raw_body"]
            .as_str()
            .unwrap()
            .contains("<h1>Slow</h1>")
    );
    assert_eq!(api.lab.received("/slow").await.len(), 1);
    api.shut_down().await;
}

#[tokio::test]
async fn a_scripts_request_goes_out_as_one_and_a_navigation_as_a_navigation() {
    let mut api = Api::start(1, 149, LENIENT_FAST).await;
    let token = api.key(&[Scope::Fetch]).await;
    serve(
        &api,
        "/api/items",
        ResponseTemplate::new(200)
            .insert_header("content-type", "application/json")
            .set_body_string("{\"items\":[1,2,3]}"),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/api/search"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string("{\"hits\":0}"),
        )
        .mount(&api.lab.site)
        .await;
    let listing = Api::url("/listing");

    // The default is a navigation, whatever the URL looks like.
    let result = api
        .post(
            &token,
            "/v1/fetch",
            &json!({ "url": Api::url("/api/items"), "render": "http" }),
        )
        .await
        .json();
    assert_eq!(result["outcome"], "ok", "{result}");
    assert_eq!(result["raw_body"], "{\"items\":[1,2,3]}");

    // A fetch() of the page that the Referer names.
    let result = api
        .post(
            &token,
            "/v1/fetch",
            &json!({
                "url": Api::url("/api/items"),
                "render": "http",
                "request_kind": "fetch",
                "headers": { "Referer": listing, "Accept": "application/json" },
            }),
        )
        .await
        .json();
    assert_eq!(result["outcome"], "ok", "{result}");

    // An XMLHttpRequest that posts JSON to its own origin.
    let result = api
        .post(
            &token,
            "/v1/fetch",
            &json!({
                "url": Api::url("/api/search"),
                "render": "http",
                "request_kind": "xhr",
                "method": "POST",
                "body": "{\"q\":\"shoes\"}",
                "headers": { "Referer": listing, "Content-Type": "application/json" },
            }),
        )
        .await
        .json();
    assert_eq!(result["outcome"], "ok", "{result}");
    assert_eq!(result["raw_body"], "{\"hits\":0}");

    let received = api.lab.received("/api/items").await;
    let [navigation, scripted] = received.as_slice() else {
        panic!("two requests, not {}", received.len());
    };
    // A navigation asks for a document and may upgrade.
    assert!(
        site_header(navigation, "accept")
            .unwrap()
            .starts_with("text/html")
    );
    assert_eq!(
        site_header(navigation, "upgrade-insecure-requests").as_deref(),
        Some("1")
    );
    assert_eq!(site_header(navigation, "referer"), None);
    // A script's request names its page, takes what the script asked
    // for, and has nothing of a navigation.
    assert_eq!(site_header(scripted, "referer"), Some(listing.clone()));
    assert_eq!(
        site_header(scripted, "accept").as_deref(),
        Some("application/json")
    );
    assert_eq!(site_header(scripted, "upgrade-insecure-requests"), None);
    assert_eq!(
        site_header(scripted, "user-agent"),
        site_header(navigation, "user-agent")
    );
    // A same-origin GET names no origin; a POST does.
    assert_eq!(site_header(scripted, "origin"), None);
    let posted = api.lab.received("/api/search").await;
    assert_eq!(posted.len(), 1);
    assert_eq!(
        site_header(&posted[0], "origin").as_deref(),
        Some("http://shop.kage.test")
    );
    assert_eq!(
        site_header(&posted[0], "content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(posted[0].body, b"{\"q\":\"shoes\"}");
    api.shut_down().await;
}

#[tokio::test]
async fn a_block_is_a_result_and_what_the_api_cannot_do_is_a_problem() {
    let mut api = Api::start(2, 155, BROWSER_FAST).await;
    let token = api.key(&[Scope::Fetch]).await;
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

    // Every persona met the wall: the answer is 200, and says who blocked.
    let reply = api
        .post(&token, "/v1/fetch", &json!({ "url": Api::url("/guarded") }))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    let result = reply.json();
    assert_eq!(result["outcome"], "forbidden", "{result}");
    assert_eq!(result["status"], 403);
    assert_eq!(result["block"]["vendor"], "cloudflare");
    assert!(result["block"]["confidence"].as_f64().unwrap() > 0.5);
    assert_eq!(result["attempts"].as_array().unwrap().len(), 2);
    assert_ne!(
        result["attempts"][0]["persona"],
        result["attempts"][1]["persona"]
    );

    // Without a pool nothing can be fetched: Kage never connects itself.
    let bare = api.with_parts(|parts| parts.engine = None);
    let sent = request(
        Method::POST,
        "/v1/fetch",
        Some(&token),
        Some(&json!({ "url": Api::url("/page") })),
    );
    ask(&bare, sent)
        .await
        .problem(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    api.shut_down().await;
}
