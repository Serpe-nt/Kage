//! Sessions through the API (spec §5, §16.3): opened by `POST
//! /v1/sessions` and by nothing else, pinned to one persona, a key's own.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use axum::http::StatusCode;
use common::Api;
use common::sched::lab::{FAST, article};
use kage_core::domain::Scope;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

async fn serve(api: &Api, at: &str) {
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(article("A page")),
        )
        .mount(&api.lab.site)
        .await;
}

#[tokio::test]
async fn a_session_is_opened_once_looked_at_and_ended() {
    let mut api = Api::start(2, 149, FAST).await;
    let token = api.key(&[Scope::Sessions]).await;
    let other = api.key(&[Scope::Sessions]).await;
    let admin = api.key(&[Scope::Sessions, Scope::Admin]).await;

    // New: 201, and the persona behind the id is said.
    let reply = api
        .post(&token, "/v1/sessions", &json!({ "id": "proj-a-42" }))
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
    let session = reply.json();
    assert_eq!(session["id"], "proj-a-42");
    let handle = session["persona"]["id"].as_str().unwrap().to_owned();
    assert!(handle.starts_with("p_"), "{handle}");
    assert_eq!(session["persona"]["engine"], "chromium");
    assert_eq!(session["persona"]["country"], "DE");
    assert!(session["created_at"].is_string() && session["expires_at"].is_string());

    // Open already: 200, the same session and the same persona.
    let reply = api
        .post(&token, "/v1/sessions", &json!({ "id": "proj-a-42" }))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["persona"]["id"], handle);
    assert_eq!(reply.json()["created_at"], session["created_at"]);

    // Looking at it is not using it.
    let seen = api.get(&token, "/v1/sessions/proj-a-42").await;
    assert_eq!(seen.status, StatusCode::OK);
    assert_eq!(seen.json()["persona"]["id"], handle);
    assert_eq!(seen.json()["last_used_at"], reply.json()["last_used_at"]);

    // Without an id the server makes one, and the second session goes to
    // the persona with fewer of them.
    let made = api.post(&token, "/v1/sessions", &json!({})).await;
    assert_eq!(made.status, StatusCode::CREATED);
    let made = made.json();
    let made_id = made["id"].as_str().unwrap();
    assert!(!made_id.is_empty() && made_id != "proj-a-42");
    assert_ne!(made["persona"]["id"], handle);

    // A session is its key's: another key has none under the id, may open
    // its own under the same one, and sees the persona under another
    // handle. An admin sees the persona as it is.
    api.get(&other, "/v1/sessions/proj-a-42")
        .await
        .problem(StatusCode::NOT_FOUND, "session_not_found");
    api.delete(&other, "/v1/sessions/proj-a-42")
        .await
        .problem(StatusCode::NOT_FOUND, "session_not_found");
    let theirs = api
        .post(&other, "/v1/sessions", &json!({ "id": "proj-a-42" }))
        .await;
    assert_eq!(theirs.status, StatusCode::CREATED);
    assert!(
        api.lab
            .personas
            .iter()
            .all(|persona| persona.id.to_string() != handle)
    );
    let own = api
        .post(&admin, "/v1/sessions", &json!({ "id": "ops" }))
        .await
        .json();
    let real = own["persona"]["id"].as_str().unwrap();
    assert!(
        api.lab
            .personas
            .iter()
            .any(|persona| persona.id.to_string() == real),
        "{real}"
    );

    // Ended: 204, and the id is free; the next one under it is new.
    assert_eq!(
        api.delete(&token, "/v1/sessions/proj-a-42").await.status,
        StatusCode::NO_CONTENT
    );
    api.get(&token, "/v1/sessions/proj-a-42")
        .await
        .problem(StatusCode::NOT_FOUND, "session_not_found");
    api.delete(&token, "/v1/sessions/proj-a-42")
        .await
        .problem(StatusCode::NOT_FOUND, "session_not_found");
    let reopened = api
        .post(&token, "/v1/sessions", &json!({ "id": "proj-a-42" }))
        .await;
    assert_eq!(reopened.status, StatusCode::CREATED);
    // The other key's session under that id was never touched.
    assert_eq!(
        api.get(&other, "/v1/sessions/proj-a-42").await.status,
        StatusCode::OK
    );

    // What is not an id, and what is not a request.
    for id in ["has space", "", "ü", &"x".repeat(65)] {
        let reply = api.post(&token, "/v1/sessions", &json!({ "id": id })).await;
        reply.problem(StatusCode::UNPROCESSABLE_ENTITY, "invalid_session_id");
    }
    api.get(&token, "/v1/sessions/has%20space")
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "invalid_session_id");
    api.post(&token, "/v1/sessions", &json!({ "ttl": 60 }))
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed");
    api.post(
        &token,
        "/v1/sessions",
        &json!({ "persona": { "countries": ["Germany"] } }),
    )
    .await
    .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed");
    api.post(&token, "/v1/sessions", &json!({ "domain": "https://x/y" }))
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed");
    api.shut_down().await;
}

#[tokio::test]
async fn hints_and_a_domain_choose_a_new_sessions_persona_and_bind_an_open_one() {
    let rules = "defaults: { pacing: fast, active_hours: ignore }\n\
                 domains:\n  firefox-only.example: { engines: [firefox] }\n";
    let mut api = Api::start(2, 149, rules).await;
    let token = api.key(&[Scope::Sessions]).await;

    // Hints the pool satisfies.
    let reply = api
        .post(
            &token,
            "/v1/sessions",
            &json!({
                "id": "de",
                "persona": { "countries": ["AT", "DE"], "engines": ["chromium"] },
                "domain": "shop.kage.test",
            }),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());

    // Hints nobody satisfies: nothing is opened.
    for wanted in [
        json!({ "id": "fr", "persona": { "countries": ["FR"] } }),
        json!({ "id": "fr", "persona": { "engines": ["firefox"] } }),
        json!({ "id": "fr", "domain": "firefox-only.example" }),
    ] {
        api.post(&token, "/v1/sessions", &wanted)
            .await
            .problem(StatusCode::CONFLICT, "no_matching_persona");
    }
    api.get(&token, "/v1/sessions/fr")
        .await
        .problem(StatusCode::NOT_FOUND, "session_not_found");

    // Against a session that is open, a hint is a condition: the session
    // is not moved to a persona the request would rather have.
    let reply = api
        .post(
            &token,
            "/v1/sessions",
            &json!({ "id": "de", "persona": { "engines": ["firefox"] } }),
        )
        .await;
    let problem = reply.problem(StatusCode::CONFLICT, "session_persona_conflict");
    assert_eq!(problem["session_id"], "de");
    assert_eq!(problem["constraint"], "persona.engines");
    let reply = api
        .post(
            &token,
            "/v1/sessions",
            &json!({ "id": "de", "domain": "firefox-only.example" }),
        )
        .await;
    assert_eq!(
        reply.problem(StatusCode::CONFLICT, "session_persona_conflict")["constraint"],
        "policy.engines"
    );
    // It is still there, as it was.
    assert_eq!(
        api.get(&token, "/v1/sessions/de").await.status,
        StatusCode::OK
    );
    api.shut_down().await;
}

#[tokio::test]
async fn a_fetch_in_a_session_is_made_by_its_persona_and_never_opens_or_moves_one() {
    let mut api = Api::start(3, 149, FAST).await;
    let token = api.key(&[Scope::Fetch, Scope::Sessions]).await;
    let other = api.key(&[Scope::Fetch, Scope::Sessions]).await;
    serve(&api, "/a").await;
    serve(&api, "/b").await;
    serve(&api, "/c").await;

    // A fetch opens nothing.
    let fetch = |at: &str, session: &str, extra: Value| {
        let mut body = json!({ "url": Api::url(at), "level": 1, "session_id": session });
        for (name, value) in extra.as_object().cloned().unwrap_or_default() {
            body[name.as_str()] = value;
        }
        body
    };
    let reply = api
        .post(&token, "/v1/fetch", &fetch("/a", "visit", json!({})))
        .await;
    let problem = reply.problem(StatusCode::NOT_FOUND, "session_not_found");
    assert_eq!(problem["session_id"], "visit");
    assert!(api.lab.received("/a").await.is_empty());

    let opened = api
        .post(&token, "/v1/sessions", &json!({ "id": "visit" }))
        .await
        .json();
    let handle = opened["persona"]["id"].clone();

    // Every fetch of the session is the same persona's, with the same
    // identity on the wire.
    for at in ["/a", "/b", "/c"] {
        let reply = api
            .post(&token, "/v1/fetch", &fetch(at, "visit", json!({})))
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
        let result = reply.json();
        assert_eq!(result["outcome"], "ok", "{result}");
        assert_eq!(result["session_id"], "visit");
        assert_eq!(result["persona"]["id"], handle);
        assert_eq!(result["attempts"].as_array().unwrap().len(), 1);
    }
    let through: Vec<usize> = api
        .lab
        .proxies
        .iter()
        .map(|proxy| proxy.plain().len())
        .collect();
    assert_eq!(through.iter().sum::<usize>(), 3);
    assert_eq!(through.iter().filter(|count| **count > 0).count(), 1);
    // The stored result names the session too.
    let used = api.get(&token, "/v1/sessions/visit").await.json();
    assert!(used["last_used_at"].as_str() >= opened["last_used_at"].as_str());

    // A constraint the session's persona fails is a conflict: nothing is
    // fetched, and the session stays where it is.
    let before = api.lab.received("/a").await.len();
    for (extra, constraint) in [
        (
            json!({ "persona": { "countries": ["FR"] } }),
            "request.persona.countries",
        ),
        (
            json!({ "persona": { "engines": ["firefox"] } }),
            "request.persona.engines",
        ),
    ] {
        let reply = api
            .post(&token, "/v1/fetch", &fetch("/a", "visit", extra))
            .await;
        let problem = reply.problem(StatusCode::CONFLICT, "session_persona_conflict");
        assert_eq!(problem["constraint"], constraint);
        assert_eq!(problem["session_id"], "visit");
        assert_eq!(problem["persona"], handle);
        assert_eq!(problem["engine"], "chromium");
        assert_eq!(problem["country"], "DE");
    }
    assert_eq!(api.lab.received("/a").await.len(), before);
    assert_eq!(
        api.get(&token, "/v1/sessions/visit").await.json()["persona"]["id"],
        handle
    );

    // Another key has no session under that id.
    api.post(&other, "/v1/fetch", &fetch("/a", "visit", json!({})))
        .await
        .problem(StatusCode::NOT_FOUND, "session_not_found");

    // A session that ended is not reopened by a fetch that still names it.
    assert_eq!(
        api.delete(&token, "/v1/sessions/visit").await.status,
        StatusCode::NO_CONTENT
    );
    api.post(&token, "/v1/fetch", &fetch("/a", "visit", json!({})))
        .await
        .problem(StatusCode::NOT_FOUND, "session_not_found");
    assert_eq!(api.lab.received("/a").await.len(), before);
    api.shut_down().await;
}
