//! `POST /v1/fetch`, `GET /v1/results/{id}` and `GET /v1/artifacts/{id}`
//! at every level (spec §16.3 to §16.5, §19, §25): the router of `kage
//! serve` over the scheduler's lab.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::sched::lab::{FAST, article};
use common::{Api, ask, request};
use kage_core::domain::{ContentHash, Level, Scope};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

/// A persona may leave a page after half a second.
const BROWSER_FAST: &str =
    "defaults: { pacing: fast, active_hours: ignore, min_dwell: 500ms }\ndomains: {}\n";

/// A page of Cloudflare's that turns the visitor away.
const CLOUDFLARE_WALL: &str = "<html><head><title>Attention Required! | Cloudflare</title></head>\
                               <body><h1>Sorry, you have been blocked</h1>\
                               <p>You are unable to access this site. Cloudflare Ray ID: 8f1</p></body></html>";

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

/// How many requests the store holds.
async fn count_requests(api: &Api) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM requests")
        .fetch_one(api.lab.store().pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_page_is_fetched_on_the_http_path_and_found_again_by_its_id() {
    let mut api = Api::start(2, 149, FAST).await;
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
            &json!({ "url": Api::url("/page"), "level": 1 }),
        )
        .await;

    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    let result = reply.json();
    assert_eq!(result["state"], "finished");
    assert_eq!(result["outcome"], "ok");
    assert_eq!(result["status"], 200);
    assert_eq!(result["final_url"], Api::url("/page"));
    // The result names the level it was made at; nothing moves a request
    // from one level to another, so nothing says it moved.
    assert_eq!(result["level"], 1);
    assert!(result.get("escalated").is_none(), "{result}");
    assert_eq!(result["cached"], false);
    // The request's id is the result's, and the answer's header.
    let id = result["id"].as_str().unwrap().to_owned();
    assert_eq!(reply.header("x-request-id"), Some(id.as_str()));
    // The http path returns the body and the headers unless told
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
    assert_eq!(attempt["engine"], "chromium");
    // A persona's attempt leaves through its own proxy: no exit to name.
    assert_eq!(attempt["exit"], Value::Null);
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
        "level",
    ] {
        assert_eq!(stored[member], result[member], "{member}");
    }
    // What the answer said of the visit besides the page is stored with
    // the result (follow-up F36): the pacing that applied and how long
    // the request waited.
    assert_eq!(stored["pacing"], result["pacing"]);
    assert_eq!(stored["pacing"]["applied"], "fast");
    assert_eq!(stored["behavior"], result["behavior"]);
    assert_eq!(stored["side_effects"], result["side_effects"]);

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
    assert_eq!(record.level, Some(Level::Emulated));
    assert_eq!(record.spec["level"], 1);
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
    let digest = sha256_of(&shot);
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
                    "sha256": digest,
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
                "level": 3,
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
    assert_eq!(result["level"], 3);
    assert_eq!(result["attempts"][0]["render"], "browser");
    assert_eq!(result["attempts"][0]["engine"], "chromium");
    assert_eq!(result["attempts"][0]["exit"], Value::Null);
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
    // A page that is not scrolled has no end of its scrolling; it has a
    // patience all the same, which no request sets.
    assert_eq!(behavior["scroll_reached"], Value::Null);
    assert_eq!(behavior["scroll_ended_by"], Value::Null);
    let patience = behavior["patience_ms"].as_u64().unwrap();
    assert!((6_000..=240_000).contains(&patience), "{patience}");
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
    assert_eq!(stored["level"], 3);
    assert_eq!(stored["dom_html"], result["dom_html"]);
    assert_eq!(stored["xhr"], result["xhr"]);
    assert_eq!(stored["artifacts"], result["artifacts"]);
    // And what the answer said of the visit: the browser's log, what the
    // screenshot fired in the page, what the persona did there and the
    // pacing that applied (follow-up F36).
    assert_eq!(stored["console"], result["console"]);
    assert_eq!(stored["console"][0]["text"], "Failed to load resource");
    assert_eq!(stored["side_effects"], json!(["resize"]));
    assert_eq!(stored["behavior"], result["behavior"]);
    assert!(stored["behavior"]["dwell_ms"].is_u64(), "{stored}");
    assert_eq!(stored["pacing"], result["pacing"]);
    // The seed of what the persona did is for a key with the admin scope,
    // in the stored result as in the answer.
    assert_eq!(stored["behavior"]["seed"], Value::Null);
    let seen_by_admin = api.get(&admin, &format!("/v1/results/{id}")).await.json();
    assert_eq!(
        seen_by_admin["behavior"]["seed"],
        seen_by_admin["attempts"][0]["seed"]
    );
    assert!(seen_by_admin["behavior"]["seed"].is_string());

    // Without `include`, a browser visit returns the document and the
    // headers, keeps the document, and behaves as the policy says. A key
    // with the admin scope is told the seed of what the persona did.
    let plain = api
        .post(
            &admin,
            "/v1/fetch",
            &json!({ "url": Api::url("/other"), "level": 3 }),
        )
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
    // The policy's page is scrolled: the result says how far the persona
    // meant to go, how far it came and what ended it, and the patience it
    // had, which is the one the browser was given.
    let scrolled = &plain["behavior"];
    assert_eq!(scrolled["scroll_ended_by"], "depth");
    assert_eq!(scrolled["scroll_reached"], scrolled["scroll_depth"]);
    let given = api
        .lab
        .chromium
        .with(|sidecar| sidecar.navigated.last().unwrap()["behavior"]["patience_ms"].clone());
    assert_eq!(scrolled["patience_ms"], given);
    assert_ne!(scrolled["patience_ms"], behavior["patience_ms"]);
    assert!(plain["attempts"][0]["seed"].is_string());
    api.shut_down().await;
}

/// Security review S1. A sidecar that was taken over names as its
/// screenshot a link to a file of the host outside the artifacts
/// directory, and describes that file truly. Kage does not take it: the
/// answer has no screenshot, and the file stays where it was.
#[cfg(unix)]
#[tokio::test]
async fn a_sidecar_cannot_hand_over_a_file_outside_its_part_of_the_artifacts_directory() {
    let mut api = Api::start(2, 155, BROWSER_FAST).await;
    let token = api.key(&[Scope::Fetch]).await;
    let host = tempfile::tempdir().unwrap();
    let secret = b"KAGE__SERVER__MASTER_KEY=not-a-real-key".to_vec();
    std::fs::write(host.path().join("environ"), &secret).unwrap();
    let dir = api.lab.config.artifacts.dir.join("sidecar/ctx");
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(host.path().join("environ"), dir.join("shot.webp")).unwrap();
    let digest = sha256_of(&secret);
    let url = Api::url("/leak");
    api.lab.chromium.with(|sidecar| {
        sidecar.pages.insert(
            url.clone(),
            json!({
                "title": "Leak",
                "dom_html": article("Leak"),
                "headers": [{ "name": "content-type", "value": "text/html; charset=utf-8" }],
                "screenshot": {
                    "path": "sidecar/ctx/shot.webp",
                    "bytes": secret.len(),
                    "sha256": digest,
                    "content_type": "image/webp",
                },
            }),
        );
    });

    let reply = api
        .post(
            &token,
            "/v1/fetch",
            &json!({
                "url": url,
                "level": 3,
                "include": ["screenshot"],
                "screenshot": { "full_page": true, "format": "webp" },
                "max_retries": 0,
            }),
        )
        .await;

    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    let result = reply.json();
    assert_eq!(result["outcome"], "ok");
    assert_eq!(result["artifacts"]["screenshot"], Value::Null, "{result}");
    assert_eq!(count_artifacts(&api).await, 0);
    assert_eq!(std::fs::read(host.path().join("environ")).unwrap(), secret);
    api.shut_down().await;
}

/// The SHA-256 of `bytes`, as a sidecar names it.
fn sha256_of(bytes: &[u8]) -> String {
    ContentHash::from_digest(Sha256::digest(bytes).into()).to_string()
}

/// How many artifacts the store holds.
async fn count_artifacts(api: &Api) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM artifacts")
        .fetch_one(api.lab.store().pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_policy_that_locks_behaviour_ignores_the_requests_and_says_so() {
    let rules = "defaults: { pacing: fast, active_hours: ignore }\n\
                 domains:\n  shop.kage.test: { behavior_locked: true, pacing_locked: true }\n";
    let mut api = Api::start(1, 155, rules).await;
    let token = api.key(&[Scope::Fetch]).await;

    let result = api
        .post(
            &token,
            "/v1/fetch",
            &json!({
                "url": Api::url("/page"),
                "level": 3,
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
    let mut api = Api::start(1, 149, FAST).await;
    let token = api.key(&[Scope::Fetch, Scope::Sessions]).await;
    let page = Api::url("/page");
    let listing = Api::url("/listing");

    let cases: Vec<(Value, StatusCode, &str)> = vec![
        // The level, before anything else: nothing is decided for the
        // caller, and a level is one of four.
        (
            json!({ "url": page }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "level_required",
        ),
        (
            json!({ "url": page, "level": 4 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "level": "1" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        // The URL, before anything is looked up.
        (
            json!({ "url": "not a url", "level": 3 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_url",
        ),
        (
            json!({ "url": "ftp://example.org/file", "level": 3 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_url",
        ),
        (
            json!({ "url": "https://user:secret@example.org/", "level": 3 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_url",
        ),
        (
            json!({ "url": "http://example.org:0/", "level": 3 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_url",
        ),
        // The SSRF guard (spec §19), at every level.
        (
            json!({ "url": "http://127.0.0.1/admin", "level": 0 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        (
            json!({ "url": "http://10.0.0.5/", "level": 1 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        (
            json!({ "url": "http://192.168.1.1/", "level": 2 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        (
            json!({ "url": "http://169.254.169.254/latest/meta-data/", "level": 0 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        (
            json!({ "url": "http://[::1]/", "level": 3 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        (
            json!({ "url": "http://[fd00::1]/", "level": 3 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        (
            json!({ "url": "http://localhost/", "level": 3 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        (
            json!({ "url": "http://metadata.google.internal/", "level": 0 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        (
            json!({ "url": "http://2130706433/", "level": 3 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        // A public address by number, while `security.allow_ip_targets` is off.
        (
            json!({ "url": "http://93.184.216.34/", "level": 3 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "ssrf_denied",
        ),
        // The header denylist of a persona's http client: what is the
        // persona's identity or the transport's.
        (
            json!({ "url": page, "level": 1, "headers": { "User-Agent": "curl/8" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "level": 1, "headers": { "cookie": "sid=1" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "level": 1, "headers": { "Host": "other.example" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "level": 1, "headers": { "Sec-Fetch-Site": "none" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "level": 1, "headers": { "Sec-CH-UA": "x" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "level": 1, "headers": { "Accept-Language": "en" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "level": 1, "headers": { "Proxy-Authorization": "Basic eA==" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "level": 1, "headers": { "Via": "1.1 squid" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "level": 1, "headers": { "bad name": "x" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_invalid",
        ),
        (
            json!({ "url": page, "level": 1, "headers": { "X-Note": "line\nbreak" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_invalid",
        ),
        (
            json!({ "url": page, "level": 1, "headers": { "Referer": "about:blank" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_invalid",
        ),
        // The denylist of a plain client: its caller's request is its own,
        // but the transport's, the proxy's and the forwarding family are
        // not.
        (
            json!({ "url": page, "level": 0, "headers": { "Proxy-Authorization": "Basic eA==" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "level": 0, "headers": { "X-Forwarded-For": "198.51.100.7" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "level": 0, "headers": { "Forwarded": "for=198.51.100.7" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "level": 0, "headers": { "Accept-Encoding": "identity" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        (
            json!({ "url": page, "level": 0, "headers": { "Host": "other.example" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_denied",
        ),
        // What only the http path sends, asked of a browser.
        (
            json!({ "url": page, "level": 3, "headers": { "X-Note": "1" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "level_needs_http",
        ),
        (
            json!({ "url": page, "level": 3, "method": "POST", "body": "a=1" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "level_needs_http",
        ),
        (
            json!({ "url": page, "level": 2, "method": "HEAD" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "level_needs_http",
        ),
        // A script's request is a browser's emulated: level 1's alone.
        (
            json!({ "url": page, "level": 3, "request_kind": "fetch", "headers": { "Referer": listing } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "level_kind_conflict",
        ),
        (
            json!({ "url": page, "level": 0, "request_kind": "xhr", "headers": { "Referer": listing } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "level_kind_conflict",
        ),
        (
            json!({ "url": page, "level": 1, "request_kind": "xhr" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "request_kind_needs_referer",
        ),
        (
            json!({ "url": page, "level": 1, "request_kind": "fetch", "headers": { "Referer": listing, "X-Requested-With": "XMLHttpRequest" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "header_not_placeable",
        ),
        (
            json!({ "url": "https://api.example.org/items", "level": 1, "request_kind": "fetch", "method": "POST", "body": "{}", "headers": { "Referer": "https://www.example.org/", "Content-Type": "application/json" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "preflight_required",
        ),
        // What only a browser produces, asked of the http path.
        (
            json!({ "url": page, "level": 1, "include": ["dom"] }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "level_include_conflict",
        ),
        (
            json!({ "url": page, "level": 0, "include": ["raw", "screenshot"] }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "level_include_conflict",
        ),
        // What a level has no use for is refused, not ignored: a plain
        // client has no persona and is paced by the site, the http path
        // has no page to wait on, and a still visit does nothing on its
        // page. A session is a persona's.
        (
            json!({ "url": page, "level": 0, "persona": { "countries": ["DE"] } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "level": 0, "pacing": { "profile": "fast" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "level": 1, "wait": { "until": "load" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "level": 1, "behavior": { "dwell_ms": 3000 } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "level": 2, "behavior": { "dwell_ms": 3000 } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "level": 0, "session_id": "s1" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "session_needs_persona",
        ),
        // Time.
        (
            json!({ "url": page, "level": 3, "timeout_ms": 5000 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "timeout_too_short",
        ),
        (
            json!({ "url": page, "level": 2, "timeout_ms": 14_999 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "timeout_too_short",
        ),
        (
            json!({ "url": page, "level": 1, "timeout_ms": 500 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "timeout_too_short",
        ),
        (
            json!({ "url": page, "level": 0, "timeout_ms": 999 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "timeout_too_short",
        ),
        (
            json!({ "url": page, "level": 3, "timeout_ms": 300_001 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        // Pacing and behaviour.
        (
            json!({ "url": page, "level": 3, "pacing": { "profile": "warp" } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_pacing",
        ),
        (
            json!({ "url": page, "level": 3, "behavior": { "scroll_depth": 1.5 } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "level": 3, "behavior": { "dwell_ms": 600_000 } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "level": 3, "max_retries": 11 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "level": 3, "persona": { "countries": ["Germany"] } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        // What the site's policy does not let a request ask for is
        // refused, not ignored.
        (
            json!({ "url": page, "level": 3, "robots": "enforce" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "robots_ignored",
        ),
        (
            json!({ "url": page, "level": 3, "cache_ttl_s": 86_401 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "cache_ttl_too_long",
        ),
        (
            json!({ "url": page, "level": 3, "extract": { "links": false } }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        // What the API does not take at all, the render mode of 1.x among
        // it: the level names the path.
        (
            json!({ "url": page, "level": 3, "locale": "de-DE" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "level": 3, "proxy": "http://127.0.0.1:8080" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "level": 1, "render": "http" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "url": page, "render": "browser" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        (
            json!({ "level": 1 }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_failed",
        ),
        // Sessions.
        (
            json!({ "url": page, "level": 1, "session_id": "has space" }),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_session_id",
        ),
        (
            json!({ "url": page, "level": 1, "session_id": "never-opened" }),
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
            &json!({ "url": page, "level": 3, "timeout_ms": 5000 }),
        )
        .await;
    let problem = reply.problem(StatusCode::UNPROCESSABLE_ENTITY, "timeout_too_short");
    assert_eq!(problem["field"], "timeout_ms");
    assert_eq!(problem["minimum_ms"], 15_000);
    let reply = api
        .post(
            &token,
            "/v1/fetch",
            &json!({ "url": page, "level": 0, "timeout_ms": 999 }),
        )
        .await;
    let problem = reply.problem(StatusCode::UNPROCESSABLE_ENTITY, "timeout_too_short");
    assert_eq!(problem["minimum_ms"], 1_000);
    let reply = api
        .post(
            &token,
            "/v1/fetch",
            &json!({ "url": page, "level": 1, "headers": { "User-Agent": "curl/8" } }),
        )
        .await;
    let problem = reply.problem(StatusCode::UNPROCESSABLE_ENTITY, "header_denied");
    assert_eq!(problem["header"], "User-Agent");
    let reply = api
        .post(
            &token,
            "/v1/fetch",
            &json!({ "url": page, "level": 2, "method": "HEAD" }),
        )
        .await;
    let problem = reply.problem(StatusCode::UNPROCESSABLE_ENTITY, "level_needs_http");
    assert_eq!(problem["field"], "method");
    let reply = api
        .post(
            &token,
            "/v1/fetch",
            &json!({ "url": page, "level": 0, "include": ["raw", "screenshot"] }),
        )
        .await;
    let problem = reply.problem(StatusCode::UNPROCESSABLE_ENTITY, "level_include_conflict");
    assert_eq!(problem["include"], "screenshot");
    for (body, field) in [
        (json!({ "url": page, "level": 4 }), "level"),
        (
            json!({ "url": page, "level": 0, "pacing": { "profile": "fast" } }),
            "pacing",
        ),
        (
            json!({ "url": page, "level": 2, "behavior": { "dwell_ms": 3000 } }),
            "behavior",
        ),
        (
            json!({ "url": page, "level": 3, "request_kind": "fetch", "headers": { "Referer": listing } }),
            "request_kind",
        ),
    ] {
        let reply = api.post(&token, "/v1/fetch", &body).await;
        assert_eq!(reply.json()["field"], field, "{body}");
    }
    // A body that is not JSON, or not said to be.
    let mut raw = request(Method::POST, "/v1/fetch", Some(&token), None);
    *raw.body_mut() = axum::body::Body::from("{\"url\": ");
    raw.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    api.send(raw)
        .await
        .problem(StatusCode::BAD_REQUEST, "invalid_request");
    let mut untyped = request(Method::POST, "/v1/fetch", Some(&token), None);
    *untyped.body_mut() = axum::body::Body::from(json!({ "url": page, "level": 1 }).to_string());
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
    assert_eq!(count_requests(&api).await, 0);
    api.shut_down().await;
}

#[tokio::test]
async fn a_redirect_to_a_denied_address_ends_the_fetch_and_returns_nothing_of_it() {
    let mut api = Api::start(2, 149, FAST).await;
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
                &json!({ "url": Api::url(at), "level": 1, "include": ["raw", "headers"] }),
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
            &json!({ "url": url, "level": 3, "include": ["dom", "headers", "xhr"] }),
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
    let mut api = Api::start(1, 149, FAST).await;
    let token = api.key(&[Scope::Fetch]).await;
    let body = json!({
        "url": "http://kage-no-such-host.invalid/",
        "level": 1,
        "max_retries": 0,
    });
    let reply = api.post(&token, "/v1/fetch", &body).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    assert_ne!(reply.json()["outcome"], "ok");
    assert_ne!(reply.json()["outcome"], "ssrf_denied");
    api.shut_down().await;

    // On: the name is looked up here first, and one that resolves to
    // nothing is refused before a persona is asked.
    let mut api = Api::start_with(1, 149, FAST, |config| {
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
    let mut api = Api::start_with(1, 149, FAST, |config| {
        config.scheduler.sync_wall_time =
            kage_core::duration::HumanDuration::new(Duration::from_millis(300));
    })
    .await;
    let token = api.key(&[Scope::Fetch]).await;
    let reader = api.key(&[Scope::Fetch, Scope::Jobs]).await;
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
            &json!({ "url": Api::url("/slow"), "level": 1 }),
        )
        .await;

    assert_eq!(reply.status, StatusCode::ACCEPTED, "{:?}", reply.json());
    let accepted = reply.json();
    let id = accepted["id"].as_str().unwrap().to_owned();
    assert_eq!(accepted["state"], "running");
    // A job carries it on from here (spec §8.4): were the server to stop,
    // the fetch would be taken up again under this request.
    let job = accepted["job_id"].as_str().unwrap().to_owned();
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
    assert_eq!(waiting.json()["job_id"], job);
    assert_eq!(waiting.header("retry-after"), Some("5"));
    // The job is read with the `jobs` scope like any other, and by its
    // own key only; the result needs no more than the fetch did.
    api.get(&token, &format!("/v1/jobs/{job}"))
        .await
        .problem(StatusCode::FORBIDDEN, "scope_missing");
    api.get(&reader, &format!("/v1/jobs/{job}"))
        .await
        .problem(StatusCode::NOT_FOUND, "job_not_found");

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
    // The result a job carried on names the level of its request too.
    assert_eq!(result["level"], 1);
    assert!(
        result["raw_body"]
            .as_str()
            .unwrap()
            .contains("<h1>Slow</h1>")
    );
    assert_eq!(api.lab.received("/slow").await.len(), 1);
    // The job ends with its one item, without a job worker having run:
    // the server that accepted the fetch finishes it, a moment after the
    // result is there to be read.
    let mut jobs: Vec<(String, String, i32)> = Vec::new();
    for _ in 0..100 {
        jobs = sqlx::query_as("SELECT state, origin, items_succeeded FROM jobs")
            .fetch_all(api.lab.store().pool())
            .await
            .unwrap();
        if jobs.iter().all(|(state, _, _)| state == "succeeded") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        jobs,
        [("succeeded".to_owned(), "sync_overflow".to_owned(), 1)]
    );
    api.shut_down().await;
}

#[tokio::test]
async fn a_scripts_request_goes_out_as_one_and_a_navigation_as_a_navigation() {
    let mut api = Api::start(1, 149, FAST).await;
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
            &json!({ "url": Api::url("/api/items"), "level": 1 }),
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
                "level": 1,
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
                "level": 1,
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
        "dom_html": CLOUDFLARE_WALL,
    });
    api.lab.chromium.with(|sidecar| {
        sidecar.next_pages.push_back(wall.clone());
        sidecar.next_pages.push_back(wall);
    });

    // Every persona met the wall: the answer is 200, and says who blocked.
    let reply = api
        .post(
            &token,
            "/v1/fetch",
            &json!({ "url": Api::url("/guarded"), "level": 3 }),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    let result = reply.json();
    assert_eq!(result["outcome"], "forbidden", "{result}");
    assert_eq!(result["level"], 3);
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
    for level in 0..=3 {
        let sent = request(
            Method::POST,
            "/v1/fetch",
            Some(&token),
            Some(&json!({ "url": Api::url("/page"), "level": level })),
        );
        ask(&bare, sent)
            .await
            .problem(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    }
    api.shut_down().await;
}

/// 1.x moved a request that was blocked on the http path to the same
/// persona's browser. 2.0 does not: the level is the request's ceiling. A
/// block at level 1 is the next persona's turn on the http path, after a
/// pause, and the result is the block when every turn met it.
#[tokio::test]
async fn a_block_at_level_one_is_another_personas_turn_and_never_a_browsers() {
    let mut api = Api::start(2, 149, FAST).await;
    let token = api.key(&[Scope::Fetch]).await;
    serve(
        &api,
        "/guarded",
        ResponseTemplate::new(403)
            .insert_header("server", "cloudflare")
            .insert_header("cf-ray", "8f1-FRA")
            .set_body_raw(CLOUDFLARE_WALL, "text/html; charset=utf-8"),
    )
    .await;

    let reply = api
        .post(
            &token,
            "/v1/fetch",
            &json!({ "url": Api::url("/guarded"), "level": 1, "max_retries": 1 }),
        )
        .await;

    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    let result = reply.json();
    assert_eq!(result["outcome"], "forbidden", "{result}");
    assert_eq!(result["level"], 1);
    assert_eq!(result["block"]["vendor"], "cloudflare");
    assert!(result.get("escalated").is_none(), "{result}");
    let attempts = result["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2, "{result}");
    for attempt in attempts {
        assert_eq!(attempt["render"], "http", "{attempt}");
        assert_eq!(attempt["outcome"], "forbidden", "{attempt}");
    }
    assert_ne!(attempts[0]["persona"], attempts[1]["persona"]);
    // The site saw two visitors on the wire, and no browser was opened.
    assert_eq!(api.lab.received("/guarded").await.len(), 2);
    let (navigated, opened) = api
        .lab
        .chromium
        .with(|sidecar| (sidecar.navigated.len(), sidecar.opened.len()));
    assert_eq!((navigated, opened), (0, 0));
    api.shut_down().await;
}

#[tokio::test]
async fn a_request_names_its_level_and_is_never_raised_to_the_floor_of_its_site() {
    // The shop takes a persona's browser and nothing below it.
    let rules = "defaults: { pacing: fast, active_hours: ignore, min_dwell: 500ms }\n\
                 domains:\n  shop.kage.test: { min_level: 2 }\n";
    let mut api = Api::start(1, 149, rules).await;
    let token = api.key(&[Scope::Fetch]).await;
    let page = Api::url("/page");

    // Nothing is decided for a request that names no level.
    let problem = api
        .post(&token, "/v1/fetch", &json!({ "url": page }))
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "level_required");
    assert_eq!(problem["field"], "level");

    // Below the site's floor a request is refused, not raised, and the
    // refusal names the floor.
    for level in [0, 1] {
        let problem = api
            .post(&token, "/v1/fetch", &json!({ "url": page, "level": level }))
            .await
            .problem(StatusCode::UNPROCESSABLE_ENTITY, "level_below_policy");
        assert_eq!(problem["field"], "level", "{level}");
        assert_eq!(problem["min_level"], 2, "{level}");
    }
    // The preview of the site's policy says the same; another site has
    // the default floor.
    let preview = api
        .get(&token, "/v1/policies/domains/shop.kage.test")
        .await
        .json();
    assert_eq!(preview["min_level"], 2, "{preview}");
    let elsewhere = api
        .get(&token, "/v1/policies/domains/example.org")
        .await
        .json();
    assert_eq!(elsewhere["min_level"], 0, "{elsewhere}");
    assert!(api.lab.site.received_requests().await.unwrap().is_empty());
    assert!(
        api.lab
            .chromium
            .with(|sidecar| sidecar.navigated.is_empty())
    );
    assert_eq!(count_requests(&api).await, 0);

    // At the floor and above it the page is fetched in the persona's
    // browser, at the level the request named: a still visit at 2, a
    // person's at 3.
    for level in [2, 3] {
        let reply = api
            .post(&token, "/v1/fetch", &json!({ "url": page, "level": level }))
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
        let result = reply.json();
        assert_eq!(result["outcome"], "ok", "{result}");
        assert_eq!(result["level"], level);
        assert_eq!(result["attempts"][0]["render"], "browser");
    }
    let navigated = api.lab.chromium.with(|sidecar| sidecar.navigated.clone());
    let [still, person] = navigated.as_slice() else {
        panic!("two navigations, not {navigated:?}");
    };
    assert_eq!(still["behavior"]["wander"], false, "{still}");
    assert_eq!(still["behavior"]["scroll_depth"], Value::Null, "{still}");
    assert!(
        person["behavior"]["scroll_depth"].as_f64().unwrap() > 0.0,
        "{person}"
    );
    api.shut_down().await;
}

#[tokio::test]
async fn level_one_is_refused_where_no_browser_of_the_installation_has_an_http_profile() {
    // Chromium 155 has no verified wire fingerprint of its own.
    let mut api = Api::start(1, 155, BROWSER_FAST).await;
    let token = api.key(&[Scope::Fetch]).await;
    let page = Api::url("/page");

    let problem = api
        .post(&token, "/v1/fetch", &json!({ "url": page, "level": 1 }))
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "level_unavailable");
    assert_eq!(problem["field"], "level");
    assert!(api.lab.site.received_requests().await.unwrap().is_empty());
    assert_eq!(count_requests(&api).await, 0);

    // The persona's browser is there all the same.
    let result = api
        .post(&token, "/v1/fetch", &json!({ "url": page, "level": 3 }))
        .await
        .json();
    assert_eq!(result["outcome"], "ok", "{result}");
    assert_eq!(result["level"], 3);
    api.shut_down().await;
}

#[tokio::test]
async fn a_session_is_a_personas_and_a_plain_client_has_none() {
    let mut api = Api::start(1, 149, FAST).await;
    let token = api.key(&[Scope::Fetch, Scope::Sessions]).await;
    serve(&api, "/page", page("The shop")).await;
    let opened = api
        .post(&token, "/v1/sessions", &json!({ "id": "visit" }))
        .await;
    assert_eq!(opened.status, StatusCode::CREATED, "{:?}", opened.json());

    // An open session does not give level 0 a persona.
    let problem = api
        .post(
            &token,
            "/v1/fetch",
            &json!({ "url": Api::url("/page"), "level": 0, "session_id": "visit" }),
        )
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "session_needs_persona");
    assert_eq!(problem["field"], "session_id");
    assert!(api.lab.received("/page").await.is_empty());

    // At a persona's level the session's persona makes it.
    let result = api
        .post(
            &token,
            "/v1/fetch",
            &json!({ "url": Api::url("/page"), "level": 1, "session_id": "visit" }),
        )
        .await
        .json();
    assert_eq!(result["outcome"], "ok", "{result}");
    assert_eq!(result["session_id"], "visit");
    assert_eq!(result["persona"]["id"], opened.json()["persona"]["id"]);
    api.shut_down().await;
}

#[tokio::test]
async fn a_plain_client_says_it_is_kage_sends_the_callers_headers_and_keeps_no_credential() {
    let mut api = Api::start(2, 149, FAST).await;
    let token = api.key(&[Scope::Fetch]).await;
    serve(
        &api,
        "/plain",
        page("The shop").insert_header("set-cookie", "sid=fresh; Path=/"),
    )
    .await;

    let reply = api
        .post(
            &token,
            "/v1/fetch",
            &json!({
                "url": Api::url("/plain"),
                "level": 0,
                "headers": {
                    "Cookie": "sid=abc123",
                    "Authorization": "Bearer t0k3n",
                    "X-Api": "1",
                },
            }),
        )
        .await;

    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    let result = reply.json();
    assert_eq!(result["outcome"], "ok", "{result}");
    assert_eq!(result["level"], 0);
    // No persona made it: none is named, and nothing a persona does on a
    // page is said.
    assert_eq!(result["persona"], Value::Null);
    assert_eq!(result["behavior"], Value::Null);
    let [attempt] = result["attempts"].as_array().unwrap().as_slice() else {
        panic!("one attempt, not {}", result["attempts"]);
    };
    assert_eq!(attempt["render"], "http");
    assert_eq!(attempt["persona"], Value::Null);
    assert_eq!(attempt["engine"], Value::Null);
    // It left through the pool, the only exit there is under
    // `egress.mode: proxies`.
    assert_eq!(attempt["exit"], "pool");
    assert!(
        result["raw_body"]
            .as_str()
            .unwrap()
            .contains("<h1>The shop</h1>")
    );

    // The site saw a client that says it is Kage, with the caller's own
    // headers and nothing of a person.
    let received = api.lab.received("/plain").await;
    assert_eq!(received.len(), 1);
    let agent = site_header(&received[0], "user-agent").unwrap();
    assert!(agent.starts_with("kage/"), "{agent}");
    assert_eq!(
        site_header(&received[0], "cookie").as_deref(),
        Some("sid=abc123")
    );
    assert_eq!(
        site_header(&received[0], "authorization").as_deref(),
        Some("Bearer t0k3n")
    );
    assert_eq!(site_header(&received[0], "x-api").as_deref(), Some("1"));
    assert_eq!(site_header(&received[0], "accept-language"), None);
    // One proxy of the pool carried it.
    let carried: Vec<usize> = api
        .lab
        .proxies
        .iter()
        .map(|proxy| proxy.plain().len())
        .collect();
    assert_eq!(carried.iter().sum::<usize>(), 1, "{carried:?}");
    // The caller's credentials are not in the answer.
    let text = String::from_utf8_lossy(&reply.body).into_owned();
    assert!(
        !text.contains("abc123") && !text.contains("t0k3n"),
        "{text}"
    );

    // The request is on record with the level and the headers it named,
    // and without the values of the caller's credentials.
    let id = result["id"].as_str().unwrap().to_owned();
    let record = api
        .lab
        .store()
        .requests()
        .get(id.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.level, Some(Level::Plain));
    assert_eq!(record.spec["level"], 0);
    assert_eq!(record.spec["headers"]["Cookie"], "[redacted]");
    assert_eq!(record.spec["headers"]["Authorization"], "[redacted]");
    assert_eq!(record.spec["headers"]["X-Api"], "1");
    let kept = record.spec.to_string();
    assert!(
        !kept.contains("abc123") && !kept.contains("t0k3n"),
        "{kept}"
    );

    // Found again by its id, as it was answered.
    let stored = api.get(&token, &format!("/v1/results/{id}")).await;
    assert_eq!(stored.status, StatusCode::OK, "{:?}", stored.json());
    let stored = stored.json();
    for member in ["level", "outcome", "persona", "attempts", "raw_body"] {
        assert_eq!(stored[member], result[member], "{member}");
    }
    api.shut_down().await;
}
