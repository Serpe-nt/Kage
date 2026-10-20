//! The admin endpoints (spec §16.3, §19): personas, proxies, the pool, the
//! policies, API keys, and the audit trail every one of them leaves.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod common;

use axum::http::{Method, StatusCode};
use common::sched::lab::FAST;
use common::{Api, ECHO_PATH};
use kage_core::config::{SelftestPageConfig, SelftestPageKind};
use kage_core::domain::Scope;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

/// The audit events of the newest page, oldest first.
async fn trail(api: &Api, admin: &str) -> Vec<Value> {
    let reply = api.get(admin, "/v1/admin/audit?limit=200").await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    let mut events = reply.json()["events"].as_array().unwrap().clone();
    events.reverse();
    events
}

/// The newest event of an action.
fn last<'a>(events: &'a [Value], action: &str) -> &'a Value {
    events
        .iter()
        .rev()
        .find(|event| event["action"] == action)
        .unwrap_or_else(|| panic!("no {action} in {events:?}"))
}

/// The secret of a token `kage_<key id>_<secret>`: everything after the
/// second underscore. The secret is base64url, so it may hold an underscore
/// itself and may end in one; its last piece is not the secret.
fn secret_of(token: &str) -> &str {
    let secret = token
        .splitn(3, '_')
        .nth(2)
        .expect("a token has three parts");
    assert_eq!(secret.len(), 43, "32 bytes in base64url without padding");
    secret
}

#[tokio::test]
async fn personas_are_listed_shown_retired_and_made_again() {
    let mut api = Api::start(2, 149, FAST).await;
    let (admin_id, admin) = api.key_with(&[Scope::Admin], common::ROOMY).await;
    let first = api.lab.personas[0].clone();
    let second = api.lab.personas[1].clone();

    // The list: every live persona, as it is.
    let listed = api.get(&admin, "/v1/admin/personas").await;
    assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.json());
    let personas = listed.json()["personas"].as_array().unwrap().clone();
    assert_eq!(personas.len(), 2);
    let shown = personas
        .iter()
        .find(|persona| persona["id"] == json!(first.id))
        .unwrap();
    assert_eq!(shown["proxy_id"], json!(first.proxy_id));
    assert_eq!(shown["engine"], "chromium");
    assert_eq!(shown["browser_major"], 149);
    assert_eq!(shown["user_agent"], first.user_agent);
    assert_eq!(shown["http_profile"], "chrome_149");
    assert_eq!(shown["country"], "DE");
    assert_eq!(shown["state"], "active");
    assert_eq!(shown["ready"], true);
    assert_eq!(shown["browser_open"], false);
    // Never the fingerprint unless it is asked for, never a credential.
    assert!(shown.get("fingerprint").is_none());
    let text = String::from_utf8_lossy(&listed.body).into_owned();
    assert!(
        !text.contains("password") && !text.contains("username"),
        "{text}"
    );

    // One persona, with its fingerprint when asked.
    let one = api
        .get(&admin, &format!("/v1/admin/personas/{}", first.id))
        .await
        .json();
    assert_eq!(one["id"], json!(first.id));
    assert!(one.get("fingerprint").is_none());
    let whole = api
        .get(
            &admin,
            &format!("/v1/admin/personas/{}?fingerprint=true", first.id),
        )
        .await
        .json();
    assert_eq!(whole["fingerprint"]["seed"], first.fingerprint["seed"]);

    // Retired: it is gone from the list of the living, and says why.
    let retired = api
        .post(
            &admin,
            &format!("/v1/admin/personas/{}/retire", first.id),
            &json!({}),
        )
        .await;
    assert_eq!(retired.status, StatusCode::OK, "{:?}", retired.json());
    assert_eq!(retired.json()["state"], "retired");
    assert!(retired.json()["retired_at"].is_string());
    let living = api.get(&admin, "/v1/admin/personas").await.json();
    assert_eq!(living["personas"].as_array().unwrap().len(), 1);
    let all = api.get(&admin, "/v1/admin/personas?all=true").await.json();
    assert_eq!(all["personas"].as_array().unwrap().len(), 2);
    let only = api
        .get(&admin, "/v1/admin/personas?state=retired")
        .await
        .json();
    assert_eq!(only["personas"][0]["id"], json!(first.id));
    // Retiring what is retired changes nothing: it is retired, as asked.
    let again = api
        .post(
            &admin,
            &format!("/v1/admin/personas/{}/retire", first.id),
            &json!({}),
        )
        .await;
    assert_eq!(again.status, StatusCode::OK, "{:?}", again.json());
    assert_eq!(again.json()["state"], "retired");
    assert_eq!(again.json()["retired_at"], retired.json()["retired_at"]);

    // Made again: the old one is retired, a new one lives behind the
    // same proxy, with another identity.
    let regenerated = api
        .post(
            &admin,
            &format!("/v1/admin/personas/{}/regenerate", second.id),
            &json!({}),
        )
        .await;
    assert_eq!(
        regenerated.status,
        StatusCode::OK,
        "{:?}",
        regenerated.json()
    );
    let fresh = regenerated.json();
    assert_ne!(fresh["id"], json!(second.id));
    assert_eq!(fresh["proxy_id"], json!(second.proxy_id));
    assert_eq!(fresh["state"], "active");
    let old = api
        .get(&admin, &format!("/v1/admin/personas/{}", second.id))
        .await
        .json();
    assert_eq!(old["state"], "retired");

    // What is not there, and what is not a question.
    for at in [
        "/v1/admin/personas/0190a0b0-0000-7000-8000-000000000001".to_owned(),
        "/v1/admin/personas/nobody".to_owned(),
    ] {
        api.get(&admin, &at)
            .await
            .problem(StatusCode::NOT_FOUND, "persona_not_found");
    }
    api.post(
        &admin,
        "/v1/admin/personas/0190a0b0-0000-7000-8000-000000000001/retire",
        &json!({}),
    )
    .await
    .problem(StatusCode::NOT_FOUND, "persona_not_found");
    api.get(&admin, "/v1/admin/personas?state=sleeping")
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed");
    api.get(&admin, "/v1/admin/personas?everything=yes")
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed");

    // Every one of these calls is in the audit trail, with who, what, to
    // what, and how it ended.
    let events = trail(&api, &admin).await;
    for event in &events {
        assert_eq!(event["actor"], "api_key");
        assert_eq!(event["api_key_id"], json!(admin_id));
        assert!(event["details"]["request_id"].is_string(), "{event}");
    }
    let retire = events
        .iter()
        .find(|event| event["action"] == "admin.persona.retire" && event["outcome"] == "success")
        .unwrap();
    assert_eq!(retire["target"], json!(first.id));
    assert_eq!(retire["details"]["status"], 200);
    let refused = events
        .iter()
        .find(|event| event["action"] == "admin.persona.retire" && event["outcome"] == "failure")
        .unwrap();
    assert_eq!(refused["details"]["code"], "persona_not_found");
    assert_eq!(refused["details"]["status"], 404);
    let regenerate = last(&events, "admin.persona.regenerate");
    assert_eq!(regenerate["target"], json!(second.id));
    assert_eq!(regenerate["details"]["result"]["new_persona"], fresh["id"]);
    assert_eq!(last(&events, "admin.persona.show")["outcome"], "failure");
    let listed: Vec<&Value> = events
        .iter()
        .filter(|event| event["action"] == "admin.persona.list")
        .collect();
    assert!(listed.len() >= 5, "{listed:?}");
    api.shut_down().await;
}

#[tokio::test]
async fn a_self_test_runs_in_the_server_in_the_personas_own_browser() {
    let mut api = Api::start_with(2, 155, FAST, |config| {
        config.selftest.browser.pages = vec![SelftestPageConfig {
            id: "probe".to_owned(),
            kind: SelftestPageKind::Probe,
            url: "http://shop.kage.test/probe/".to_owned(),
        }];
    })
    .await;
    let admin = api.key(&[Scope::Admin]).await;
    let fetch = api.key(&[Scope::Fetch]).await;
    let persona = api.lab.personas[0].clone();
    let at = format!("/v1/admin/personas/{}/selftest", persona.id);

    // Real work has the persona's browser open.
    let pinned = api
        .post(
            &fetch,
            "/v1/fetch",
            &json!({ "url": Api::url("/page"), "level": 3, "max_retries": 0 }),
        )
        .await
        .json();
    assert_eq!(pinned["outcome"], "ok", "{pinned}");
    let open = api.lab.manager.contexts().open();
    assert_eq!(open.len(), 1);
    let tested = open[0];
    let at_open = format!("/v1/admin/personas/{tested}/selftest");

    // The self-test is made in that very context: no second browser for
    // the persona, no `context_conflict`, and the browser stays open for
    // the work that had it. No body at all asks for the default.
    let reply = api.call(Method::POST, &at_open, Some(&admin), None).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    let result = reply.json();
    assert_eq!(result["passed"], true, "{result}");
    assert_eq!(result["browser"]["verdict"], "passed");
    assert_eq!(result["browser"]["score"], 100.0);
    assert!(result["browser"]["run"].is_string());
    // This persona's browser has no http path, and no echo is configured.
    assert_eq!(result["http"]["verdict"], "skipped");
    assert!(result["http"]["skipped"].is_string());
    assert_eq!(result["persona"]["id"], json!(tested));
    assert_eq!(result["persona"]["state"], "active");
    assert_eq!(result["persona"]["browser_open"], true);
    let (contexts, opened, tests, closed) = api.lab.chromium.with(|sidecar| {
        (
            sidecar.contexts.len(),
            sidecar.opened.clone(),
            sidecar.selftested.clone(),
            sidecar.closed.len(),
        )
    });
    assert_eq!(contexts, 1);
    assert_eq!(closed, 0);
    // Whoever asked the sidecar for the persona's context asked for the
    // same one: the sidecar never saw two sets of parameters.
    assert!(opened.iter().all(|asked| *asked == opened[0]), "{opened:?}");
    let [asked] = tests.as_slice() else {
        panic!("one self-test, not {tests:?}");
    };
    assert_eq!(asked["pages"][0]["id"], "probe");
    assert_eq!(asked["expected"]["browser_major"], 155);
    // The run is on record.
    let runs = api
        .lab
        .store()
        .selftest_runs()
        .recent(tested, 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].passed);

    // A persona whose browser is closed gets one for the test, and it is
    // closed again. A test that fails cools the persona.
    let other = api
        .lab
        .personas
        .iter()
        .find(|persona| persona.id != tested)
        .unwrap()
        .id;
    api.lab.chromium.with(|sidecar| {
        sidecar.selftest = Some(json!({ "passed": false, "score": 41 }));
    });
    let reply = api
        .post(
            &admin,
            &format!("/v1/admin/personas/{other}/selftest"),
            &json!({ "third_party": false }),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    let failed = reply.json();
    assert_eq!(failed["passed"], false);
    assert_eq!(failed["browser"]["verdict"], "failed");
    assert_eq!(failed["browser"]["score"], 41.0);
    assert_eq!(failed["persona"]["state"], "cooling");
    assert_eq!(failed["persona"]["cooling_reason"], "selftest_failed");
    assert_eq!(failed["persona"]["browser_open"], false);
    assert_eq!(api.lab.chromium.with(|sidecar| sidecar.closed.len()), 1);
    // One that passes ends the cooling.
    api.lab.chromium.with(|sidecar| sidecar.selftest = None);
    let again = api
        .post(
            &admin,
            &format!("/v1/admin/personas/{other}/selftest"),
            &json!({}),
        )
        .await
        .json();
    assert_eq!(again["passed"], true, "{again}");
    assert_eq!(again["persona"]["state"], "active");

    // What cannot be tested, and what is not a request.
    api.post(
        &admin,
        "/v1/admin/personas/0190a0b0-0000-7000-8000-000000000001/selftest",
        &json!({}),
    )
    .await
    .problem(StatusCode::NOT_FOUND, "persona_not_found");
    api.post(&admin, &at, &json!({ "pages": ["https://example.org/"] }))
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed");

    let events = trail(&api, &admin).await;
    let passed = events
        .iter()
        .find(|event| {
            event["action"] == "admin.persona.selftest" && event["target"] == json!(tested)
        })
        .unwrap();
    assert_eq!(passed["outcome"], "success");
    assert_eq!(passed["details"]["result"]["browser"], "passed");
    assert_eq!(passed["details"]["result"]["passed"], true);
    let cooled = events
        .iter()
        .find(|event| {
            event["action"] == "admin.persona.selftest"
                && event["details"]["result"]["passed"] == false
        })
        .unwrap();
    assert_eq!(cooled["target"], json!(other));
    // A test that found a failing persona was itself carried out.
    assert_eq!(cooled["outcome"], "success");
    api.shut_down().await;
}

#[tokio::test]
async fn proxies_the_pool_and_the_policies_are_an_admins_to_see_and_to_act_on() {
    let mut api = Api::start(2, 149, FAST).await;
    // The policy preview is the `fetch` scope's.
    let admin = api.key(&[Scope::Admin, Scope::Fetch]).await;
    let proxy = api.lab.personas[0].proxy_id;

    // The proxies, without what they are logged in with.
    let listed = api.get(&admin, "/v1/admin/proxies").await;
    assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.json());
    let proxies = listed.json()["proxies"].as_array().unwrap().clone();
    assert_eq!(proxies.len(), 2);
    let shown = proxies
        .iter()
        .find(|shown| shown["id"] == json!(proxy))
        .unwrap();
    assert_eq!(shown["host"], "127.0.0.1");
    assert_eq!(shown["country"], "DE");
    assert_eq!(shown["state"], "healthy");
    let text = String::from_utf8_lossy(&listed.body).into_owned();
    for secret in [
        common::sched::proxy::PROXY_USER,
        common::sched::proxy::PROXY_PASS,
        "password",
        "username",
    ] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }

    // A probe now: it goes through the proxy to the echo, and the proxy
    // is shown as it is afterwards.
    Mock::given(method("GET"))
        .and(path(ECHO_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string("198.51.0.10"))
        .mount(&api.lab.site)
        .await;
    let probed = api
        .post(
            &admin,
            &format!("/v1/admin/proxies/{proxy}/health"),
            &json!({}),
        )
        .await;
    assert_eq!(probed.status, StatusCode::OK, "{:?}", probed.json());
    assert_eq!(probed.json()["id"], json!(proxy));
    assert_eq!(probed.json()["state"], "healthy");
    assert_eq!(probed.json()["consecutive_failures"], 0);
    assert!(probed.json()["last_health"].is_object());
    assert_eq!(api.lab.received(ECHO_PATH).await.len(), 1);
    // The host's own address was asked of the harness, of nobody else.
    assert_eq!(api.host_echo.received_requests().await.unwrap().len(), 1);

    // The pool at a glance.
    let pool = api.get(&admin, "/v1/admin/pool").await;
    assert_eq!(pool.status, StatusCode::OK, "{:?}", pool.json());
    let pool = pool.json();
    assert_eq!(pool["personas"]["live"], 2);
    assert_eq!(pool["personas"]["ready"], 2);
    assert_eq!(pool["personas"]["by_engine"]["chromium"], 2);
    assert_eq!(pool["personas"]["browsers_open"], 0);
    assert!(pool["proxies"].is_object());
    assert!(pool["bandwidth"].is_object(), "{pool}");

    // A replacement the provider cannot make is said, not thrown.
    let replaced = api
        .post(
            &admin,
            &format!("/v1/admin/proxies/{proxy}/replace"),
            &json!({}),
        )
        .await;
    assert_eq!(replaced.status, StatusCode::OK, "{:?}", replaced.json());
    let outcome = replaced.json()["outcome"].as_str().unwrap().to_owned();
    assert!(
        ["unsupported", "failed", "not_eligible"].contains(&outcome.as_str()),
        "{outcome}"
    );
    assert!(replaced.json()["detail"].is_string());
    for at in ["health", "replace"] {
        api.post(
            &admin,
            &format!("/v1/admin/proxies/0190a0b0-0000-7000-8000-000000000001/{at}"),
            &json!({}),
        )
        .await
        .problem(StatusCode::NOT_FOUND, "proxy_not_found");
    }

    // The policies are read again on request; a file that is invalid
    // changes nothing and is said.
    let rules = api.lab.policy_dir().join("domains.yaml");
    std::fs::write(
        &rules,
        "defaults: { pacing: fast, active_hours: ignore }\n\
         domains:\n  news.example: { min_level: 2 }\n  shop.example: { retries: 1 }\n",
    )
    .unwrap();
    let reloaded = api
        .post(&admin, "/v1/admin/policies/reload", &json!({}))
        .await;
    assert_eq!(reloaded.status, StatusCode::OK, "{:?}", reloaded.json());
    assert_eq!(reloaded.json()["domains"], 2);
    assert!(reloaded.json()["pacing_profiles"].as_u64().unwrap() >= 2);
    assert!(reloaded.json()["signature_rules"].as_u64().unwrap() > 0);
    let preview = api
        .get(&admin, "/v1/policies/domains/www.news.example")
        .await
        .json();
    assert_eq!(preview["min_level"], 2, "{preview}");
    assert!(preview.get("first_render").is_none(), "{preview}");
    // A file of 1.x that still names a render mode is refused, as is a
    // floor that is no level: the level of each request decides the path.
    for invalid in [
        "defaults: { render: http, pacing: fast }\n",
        "domains:\n  news.example: { min_level: 4 }\n",
    ] {
        std::fs::write(&rules, invalid).unwrap();
        let refused = api
            .post(&admin, "/v1/admin/policies/reload", &json!({}))
            .await;
        let problem = refused.problem(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed");
        assert!(problem["kept"].is_string(), "{invalid}: {problem}");
    }
    let preview = api
        .get(&admin, "/v1/policies/domains/www.news.example")
        .await
        .json();
    assert_eq!(preview["min_level"], 2);

    let events = trail(&api, &admin).await;
    let probe = events
        .iter()
        .find(|event| event["action"] == "admin.proxy.health" && event["outcome"] == "success")
        .unwrap();
    assert_eq!(probe["target"], json!(proxy));
    // A call that failed still says what it was aimed at.
    let missed = last(&events, "admin.proxy.health");
    assert_eq!(missed["outcome"], "failure");
    assert_eq!(missed["target"], "0190a0b0-0000-7000-8000-000000000001");
    assert_eq!(missed["details"]["code"], "proxy_not_found");
    let replace = events
        .iter()
        .find(|event| event["action"] == "admin.proxy.replace" && event["outcome"] == "success")
        .unwrap();
    assert_eq!(replace["details"]["result"]["outcome"], outcome);
    assert_eq!(last(&events, "admin.proxy.list")["outcome"], "success");
    assert_eq!(last(&events, "admin.pool.show")["outcome"], "success");
    let reloads: Vec<&str> = events
        .iter()
        .filter(|event| event["action"] == "admin.policy.reload")
        .map(|event| event["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(reloads, ["success", "failure", "failure"]);
    api.shut_down().await;
}

#[tokio::test]
async fn keys_are_made_rotated_and_revoked_and_their_tokens_shown_once() {
    let mut api = Api::start(0, 149, FAST).await;
    let (admin_id, admin) = api.key_with(&[Scope::Admin], common::ROOMY).await;
    let probe = "/v1/policies/domains/example.org";

    // Made: the token is in the answer, which nothing may keep.
    let made = api
        .post(
            &admin,
            "/v1/admin/keys",
            &json!({
                "name": "crawler of project A",
                "scopes": ["fetch", "sessions"],
                "requests_per_minute": 120,
                "daily_page_budget": 5000,
            }),
        )
        .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.json());
    assert_eq!(made.header("cache-control"), Some("no-store"));
    let created = made.json();
    let id = created["key"]["id"].as_str().unwrap().to_owned();
    let token = created["token"].as_str().unwrap().to_owned();
    assert!(
        token.starts_with(&format!("kage_{}_", id.replace('-', ""))),
        "{token}"
    );
    assert_eq!(created["key"]["name"], "crawler of project A");
    assert_eq!(created["key"]["scopes"], json!(["fetch", "sessions"]));
    assert_eq!(created["key"]["requests_per_minute"], 120);
    // What was left out is the default.
    assert_eq!(created["key"]["max_concurrent"], 4);
    assert_eq!(created["key"]["daily_page_budget"], 5000);
    assert_eq!(created["key"]["active"], true);
    // The token works, with the scopes it was given.
    assert_eq!(api.get(&token, probe).await.status, StatusCode::OK);
    api.get(&token, "/v1/admin/keys")
        .await
        .problem(StatusCode::FORBIDDEN, "scope_missing");

    // Listed without anything of its secret: Kage has only a hash, and
    // does not show that either.
    let listed = api.get(&admin, "/v1/admin/keys").await;
    assert_eq!(listed.status, StatusCode::OK);
    let keys = listed.json()["keys"].as_array().unwrap().clone();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().any(|key| key["id"] == json!(admin_id)));
    let shown = keys.iter().find(|key| key["id"] == id.as_str()).unwrap();
    assert!(shown.get("token").is_none() && shown.get("secret_hash").is_none());
    assert!(shown["last_used_at"].is_string());
    // The secret is everything after the second underscore: it is base64url
    // and may itself contain or end in one (F42).
    let secret = secret_of(&token);
    assert!(!String::from_utf8_lossy(&listed.body).contains(secret));

    // Rotated: a new token, the old one is dead at once, the key is the
    // same.
    let rotated = api
        .post(&admin, &format!("/v1/admin/keys/{id}/rotate"), &json!({}))
        .await;
    assert_eq!(rotated.status, StatusCode::OK, "{:?}", rotated.json());
    assert_eq!(rotated.header("cache-control"), Some("no-store"));
    let fresh = rotated.json()["token"].as_str().unwrap().to_owned();
    assert_ne!(fresh, token);
    assert_eq!(rotated.json()["key"]["id"], id.as_str());
    api.get(&token, probe)
        .await
        .problem(StatusCode::UNAUTHORIZED, "invalid_key");
    assert_eq!(api.get(&fresh, probe).await.status, StatusCode::OK);

    // Revoked: it stops authenticating at once; its row stays.
    let revoked = api.delete(&admin, &format!("/v1/admin/keys/{id}")).await;
    assert_eq!(revoked.status, StatusCode::OK, "{:?}", revoked.json());
    assert_eq!(revoked.json()["active"], false);
    assert!(revoked.json()["disabled_at"].is_string());
    api.get(&fresh, probe)
        .await
        .problem(StatusCode::UNAUTHORIZED, "key_disabled");
    let keys = api.get(&admin, "/v1/admin/keys").await.json();
    assert_eq!(keys["keys"].as_array().unwrap().len(), 2);
    // A revoked key is not rotated back to life.
    api.post(&admin, &format!("/v1/admin/keys/{id}/rotate"), &json!({}))
        .await
        .problem(StatusCode::CONFLICT, "conflict");

    // What is not a key.
    for body in [
        json!({ "name": "", "scopes": ["fetch"] }),
        json!({ "name": "x".repeat(101), "scopes": ["fetch"] }),
        json!({ "name": "no scopes", "scopes": [] }),
        json!({ "name": "root", "scopes": ["everything"] }),
        json!({ "name": "zero", "scopes": ["fetch"], "requests_per_minute": 0 }),
        json!({ "name": "zero", "scopes": ["fetch"], "max_concurrent": 0 }),
        json!({ "name": "secret", "scopes": ["fetch"], "token": "kage_mine" }),
    ] {
        api.post(&admin, "/v1/admin/keys", &body)
            .await
            .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed");
    }
    for (verb, at) in [
        (
            Method::DELETE,
            "/v1/admin/keys/0190a0b0-0000-7000-8000-000000000001".to_owned(),
        ),
        (
            Method::POST,
            "/v1/admin/keys/0190a0b0-0000-7000-8000-000000000001/rotate".to_owned(),
        ),
        (Method::DELETE, "/v1/admin/keys/nothing".to_owned()),
    ] {
        api.call(verb, &at, Some(&admin), None)
            .await
            .problem(StatusCode::NOT_FOUND, "key_not_found");
    }

    // The trail: who made, rotated and revoked which key, and never a
    // token or a hash.
    let events = trail(&api, &admin).await;
    let create = events
        .iter()
        .find(|event| event["action"] == "admin.key.create" && event["outcome"] == "success")
        .unwrap();
    assert_eq!(create["target"], id.as_str());
    assert_eq!(create["api_key_id"], json!(admin_id));
    assert_eq!(create["details"]["status"], 201);
    assert_eq!(
        create["details"]["result"]["scopes"],
        json!(["fetch", "sessions"])
    );
    let rotate = events
        .iter()
        .find(|event| event["action"] == "admin.key.rotate" && event["outcome"] == "success")
        .unwrap();
    assert_eq!(rotate["target"], id.as_str());
    let revoke = events
        .iter()
        .find(|event| event["action"] == "admin.key.revoke" && event["outcome"] == "success")
        .unwrap();
    assert_eq!(revoke["target"], id.as_str());
    let text = serde_json::to_string(&events).unwrap();
    for token in [&token, &fresh] {
        let secret = secret_of(token);
        assert!(!text.contains(secret), "a secret is in the audit trail");
    }
    assert!(
        events
            .iter()
            .any(|event| event["action"] == "admin.key.create" && event["outcome"] == "failure")
    );

    // The trail is read a page at a time, newest first.
    let page = api.get(&admin, "/v1/admin/audit?limit=3").await.json();
    let first = page["events"].as_array().unwrap().clone();
    assert_eq!(first.len(), 3);
    let cursor = page["next_cursor"].as_str().unwrap();
    assert_eq!(cursor, first[2]["id"].as_str().unwrap());
    let next = api
        .get(&admin, &format!("/v1/admin/audit?limit=3&cursor={cursor}"))
        .await
        .json();
    let second = next["events"].as_array().unwrap().clone();
    assert_eq!(second.len(), 3);
    assert!(second[0]["id"].as_str() < first[2]["id"].as_str());
    assert!(first[0]["at"].as_str() >= first[2]["at"].as_str());
    api.get(&admin, "/v1/admin/audit?cursor=yesterday")
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed");
    api.shut_down().await;
}

#[tokio::test]
async fn without_a_pool_the_admin_endpoints_that_need_one_say_so() {
    let mut api = Api::start(0, 149, FAST).await;
    let admin = api.key(&[Scope::Admin]).await;
    let bare = api.with_parts(|parts| parts.engine = None);

    for (verb, at) in [
        (Method::GET, "/v1/admin/pool"),
        (
            Method::POST,
            "/v1/admin/personas/0190a0b0-0000-7000-8000-000000000001/retire",
        ),
        (
            Method::POST,
            "/v1/admin/proxies/0190a0b0-0000-7000-8000-000000000001/health",
        ),
    ] {
        let sent = common::request(verb, at, Some(&admin), None);
        common::ask(&bare, sent)
            .await
            .problem(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    }
    // What needs only the store still answers.
    for at in ["/v1/admin/keys", "/v1/admin/audit", "/v1/admin/personas"] {
        let sent = common::request(Method::GET, at, Some(&admin), None);
        let reply = common::ask(&bare, sent).await;
        assert_eq!(reply.status, StatusCode::OK, "{at}: {:?}", reply.json());
    }
    api.shut_down().await;
}
