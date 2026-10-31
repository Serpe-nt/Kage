//! Every fetch path for real, one layer below the compose stack (spec §8,
//! §9, §10, §25): the scheduler over Postgres, the real executor, kage-http
//! through the forwarder and a stand-in provider proxy to a site, mock
//! sidecars that answer the contract, personas whose browser major has an
//! http profile (chromium 149 with `chrome_149`), and the plain client.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::lab::{FAST, Lab, SITE, article, cookie};
use common::proxy::{PROXY_PASS, PROXY_USER, basic};
use kage_core::alert::{Alert, AlertKind, AlertSeverity, AlertSink};
use kage_core::config::DEFAULT_PLAIN_USER_AGENT;
use kage_core::domain::{
    BreakerState, Exit, HealthFailure, IDLE_DOMAIN, Level, Persona, ProxyState, Render, SubjectKind,
};
use kage_core::ids::PersonaId;
use kage_core::outcome::Outcome;
use kage_core::persona::pacing_factor;
use kage_core::schedule::budget::{BudgetLevel, BudgetRules};
use kage_core::schedule::queue::Priority;
use kage_proxy::machine::{ExitObservation, ProbeReport};
use kage_scheduler::pipeline::attempt_seed;
use kage_scheduler::{Arrival, Backend, Bandwidth, FetchOrder, Ledger, Reached, StoreLedger};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

/// A page in a persona's browser, as a person (level 3).
fn get(path: &str) -> FetchOrder {
    FetchOrder::get(Lab::url(path), Priority::Sync)
}

/// A page at `level`.
fn at(level: Level, path: &str) -> FetchOrder {
    FetchOrder { level, ..get(path) }
}

/// A page on a persona's http path (level 1).
fn http(path: &str) -> FetchOrder {
    at(Level::Emulated, path)
}

/// The tab the reader's pages share, as a crawl names it (F96).
const CRAWL_TAB: &str = "crawl-0199c1f4";

/// A page the persona reaches from `holder`, a page it has been reading,
/// as a page of a crawl: in the crawl's tab.
fn get_from(path: &str, holder: &str) -> FetchOrder {
    FetchOrder {
        arrival: Some(Arrival::Link {
            from: Lab::url(holder),
            max_backs: 3,
        }),
        tab: Some(CRAWL_TAB.to_owned()),
        ..get(path)
    }
}

/// The same on the http path.
fn http_from(path: &str, holder: &str) -> FetchOrder {
    FetchOrder {
        level: Level::Emulated,
        ..get_from(path, holder)
    }
}

fn page(title: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/html; charset=utf-8")
        .set_body_string(article(title))
}

async fn serve(lab: &Lab, at: &str, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(response)
        .mount(&lab.site)
        .await;
}

fn persona_of(lab: &Lab, id: PersonaId) -> Persona {
    lab.personas
        .iter()
        .find(|persona| persona.id == id)
        .cloned()
        .expect("a persona of this lab")
}

fn header(request: &wiremock::Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The `Referer` the site received with the first request for `at`.
async fn referer_at(lab: &Lab, at: &str) -> Option<String> {
    header(&lab.received(at).await[0], "referer")
}

/// A page a Cloudflare WAF answers with.
fn waf_block() -> serde_json::Value {
    json!({
        "status": 403,
        "title": "Attention Required! | Cloudflare",
        "dom_html": "<html><head><title>Attention Required! | Cloudflare</title></head>\
                     <body><h1>Sorry, you have been blocked</h1>\
                     <p>You are unable to access this site. Cloudflare Ray ID: 8f1</p></body></html>",
    })
}

#[tokio::test]
async fn the_http_path_leaves_through_the_forwarder_and_the_personas_own_proxy() {
    let mut lab = Lab::start(2, 149, FAST).await;
    serve(
        &lab,
        "/page",
        page("The shop").insert_header("set-cookie", "sid=abc; Path=/"),
    )
    .await;
    serve(&lab, "/next", page("The next page")).await;

    let report = lab.pipeline.fetch(http("/page")).await.unwrap();

    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    let [attempt] = report.attempts.as_slice() else {
        panic!("one attempt, not {:?}", report.attempts);
    };
    assert_eq!(attempt.render, Render::Http);
    assert_eq!(attempt.status, Some(200));
    assert_eq!(attempt.exit, None);
    let persona = persona_of(&lab, attempt.persona_id.unwrap());
    assert_eq!(attempt.engine, Some(persona.engine));
    assert_eq!(
        attempt.seed,
        Some(attempt_seed(&persona.seed, report.request, 1))
    );
    let fetched = report.last.as_ref().unwrap().fetched.as_ref().unwrap();
    assert!(fetched.text().contains("<h1>The shop</h1>"));

    // The site saw the persona: its User-Agent and its languages.
    let received = lab.received("/page").await;
    assert_eq!(received.len(), 1);
    assert_eq!(
        header(&received[0], "user-agent"),
        Some(persona.user_agent.clone())
    );
    assert!(persona.user_agent.contains("Chrome/149."));
    assert!(
        header(&received[0], "accept-language")
            .unwrap()
            .starts_with("de-DE")
    );
    assert_eq!(header(&received[0], "referer"), None);

    // It left through that persona's own proxy, with the provider's
    // credentials added by the forwarder; the other proxy saw nothing.
    let own = lab.proxy_of(&persona);
    let plain = own.plain();
    assert_eq!(plain.len(), 1);
    assert_eq!(plain[0].target, format!("http://{SITE}/page"));
    assert_eq!(
        plain[0].header("proxy-authorization"),
        Some(basic(PROXY_USER, PROXY_PASS))
    );
    for other in lab.proxies.iter().filter(|proxy| proxy.addr != own.addr) {
        assert_eq!(other.seen().len(), 0);
    }
    assert_eq!(own.refused(), 0);

    // Every byte was counted, toward the site, and it is what the
    // provider's proxy received and sent.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let counted = lab.forwarder.meter().drain();
    assert_eq!(counted.len(), 1, "{counted:?}");
    assert_eq!(counted[0].persona, Some(persona.id));
    assert_eq!(counted[0].egress, Exit::Pool);
    assert_eq!(counted[0].domain, SITE);
    assert_eq!((counted[0].bytes_up, counted[0].bytes_down), own.bytes());
    assert!(counted[0].bytes_down > fetched.body.len() as u64);

    // Everything about it is in the store.
    let store = lab.store();
    let request = store.requests().get(report.request).await.unwrap().unwrap();
    assert_eq!(request.outcome, Some(Outcome::Ok));
    assert_eq!(request.domain, SITE);
    assert_eq!(request.level, Some(Level::Emulated));
    let attempts = store
        .attempts()
        .list_for_request(report.request)
        .await
        .unwrap();
    // (The store keeps microseconds; everything else is as reported.)
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].id, attempt.id);
    assert_eq!(attempts[0].persona_id, Some(persona.id));
    assert_eq!(attempts[0].outcome, Outcome::Ok);
    assert_eq!(attempts[0].seed, attempt.seed);
    assert_eq!(attempts[0].timing, attempt.timing);
    let result = store
        .results()
        .get_by_request(report.request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.id, report.result);
    assert_eq!(result.status, Some(200));
    let facts = lab.backend.facts(SITE).await.unwrap();
    let state = &facts.states[&persona.id];
    assert_eq!(state.requests_today, 1);
    assert_eq!(state.last_outcome, Some(Outcome::Ok));
    assert_eq!(facts.breaker, None);
    // The proxy just carried a page: its next health probe can wait.
    assert!(lab.pool.last_traffic(persona.proxy_id).is_some());

    // The next page of the visit is the same persona's, arrives from the
    // page before it, and brings the cookie the site set.
    let next = lab.pipeline.fetch(http("/next")).await.unwrap();
    assert_eq!(next.outcome, Outcome::Ok);
    assert_eq!(next.attempts[0].persona_id, Some(persona.id));
    let received = lab.received("/next").await;
    assert_eq!(
        header(&received[0], "referer"),
        Some(format!("http://{SITE}/page"))
    );
    assert_eq!(header(&received[0], "cookie"), Some("sid=abc".to_owned()));
    // Its own pace: not before the shortest gap of the profile at the
    // pace of the fastest persona.
    let gap = next.attempts[0].started_at - attempt.started_at;
    assert!(gap >= chrono::Duration::milliseconds(240), "{gap}");
    lab.shut_down().await;
}

#[tokio::test]
async fn a_personas_cookies_flow_between_its_http_path_and_its_browser() {
    let mut lab = Lab::start(1, 149, FAST).await;
    serve(
        &lab,
        "/start",
        page("The shop").insert_header("set-cookie", "pre=1; Path=/"),
    )
    .await;
    serve(&lab, "/after", page("Behind the wall")).await;
    // What the browser earns when it loads the page: the clearance.
    lab.chromium.with(|sidecar| {
        sidecar.cookies.insert(
            format!("http://{SITE}"),
            vec![cookie("cf_clearance", "xyz")],
        );
    });

    // Level 1: the persona's http path, whose jar keeps what the site set.
    let start = lab.pipeline.fetch(http("/start")).await.unwrap();
    assert_eq!(start.outcome, Outcome::Ok, "{:?}", start.reason);
    let persona = persona_of(&lab, start.attempts[0].persona_id.unwrap());

    // Level 3: the same persona's browser, the lab's only one.
    let report = lab.pipeline.fetch(get("/wall")).await.unwrap();
    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    let [attempt] = report.attempts.as_slice() else {
        panic!("one attempt, not {:?}", report.attempts);
    };
    assert_eq!(attempt.render, Render::Browser);
    assert_eq!(attempt.persona_id, Some(persona.id));

    let (opened, navigated, imported, exported) = lab.chromium.with(|sidecar| {
        (
            sidecar.opened.clone(),
            sidecar.navigated.clone(),
            sidecar.imported.clone(),
            sidecar.exported.clone(),
        )
    });
    // The browser was given the forwarder as its proxy, under the
    // persona's own credential: nothing of the provider's enters a
    // sidecar.
    assert_eq!(opened.len(), 1);
    let proxy = &opened[0]["proxy"];
    assert_eq!(
        proxy["server"],
        format!("http://kage:{}", lab.forwarder.local_addr().port())
    );
    assert_eq!(proxy["username"], persona.id.to_string());
    assert_eq!(proxy["password"], lab.tokens.token(persona.id));
    assert!(!opened[0].to_string().contains(PROXY_PASS));

    // Before the page: what the http jar held for the origin went into
    // the browser.
    let last = report.last.as_ref().unwrap();
    assert_eq!(last.cookies_to_browser, Some(1));
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0]["origin"], format!("http://{SITE}"));
    assert_eq!(imported[0]["cookies"][0]["name"], "pre");
    assert_eq!(imported[0]["cookies"][0]["value"], "1");

    // The page was asked for with the behavior of a person: a human
    // dwell, a scroll depth from the attempt's seed, a wandering pointer,
    // a typed navigation; and with time for a challenge to resolve.
    assert_eq!(navigated.len(), 1);
    let asked = &navigated[0];
    assert_eq!(asked["url"], format!("http://{SITE}/wall"));
    let seed = attempt_seed(&persona.seed, report.request, 1);
    assert_eq!(attempt.seed, Some(seed));
    let behavior = &asked["behavior"];
    assert_eq!(behavior["dwell"], "human");
    assert_eq!(behavior["wander"], true);
    assert_eq!(behavior["seed"], seed.to_string());
    assert_eq!(behavior["entry"]["strategy"], "direct");
    let depth = behavior["scroll_depth"].as_f64().unwrap();
    assert!((0.6..=0.9).contains(&depth), "{depth}");
    // How long it goes on scrolling is a patience drawn for this attempt
    // from its seed at the persona's pace, and recorded with it: the time
    // to reach the page bounds nothing else (follow-up F44).
    let patience = lab
        .config
        .browser
        .patience
        .draw(pacing_factor(&persona.seed), &seed);
    assert_eq!(
        behavior["patience_ms"].as_u64(),
        u64::try_from(patience.as_millis()).ok()
    );
    let applied = last.behavior.as_ref().unwrap();
    assert_eq!(applied.patience, Some(patience));
    assert_eq!(last.reached, Some(Reached::Typed));
    assert_eq!(asked["challenge_wait_ms"], 15_000);
    assert_eq!(asked["capture"]["dom"], true);
    assert_eq!(
        asked["timeout_ms"].as_u64(),
        u64::try_from(lab.config.browser.timeout.get().as_millis()).ok()
    );

    // After the page: what the browser holds for the origin went to the
    // http jar, and the browser stays open for the visit.
    assert_eq!(exported.len(), 1);
    assert_eq!(last.cookies_to_http, Some(2));
    assert_eq!(lab.chromium.with(|sidecar| sidecar.closed.len()), 0);
    assert_eq!(lab.manager.contexts().open(), [persona.id]);

    // The persona's next page goes over the http path again, and carries
    // what its browser earned.
    let after = lab.pipeline.fetch(http("/after")).await.unwrap();
    assert_eq!(after.outcome, Outcome::Ok);
    assert_eq!(after.attempts.len(), 1);
    assert_eq!(after.attempts[0].render, Render::Http);
    assert_eq!(after.attempts[0].persona_id, Some(persona.id));
    let received = lab.received("/after").await;
    let sent = header(&received[0], "cookie").unwrap();
    assert!(sent.contains("cf_clearance=xyz"), "{sent}");
    assert!(sent.contains("pre=1"), "{sent}");
    lab.shut_down().await;
}

#[tokio::test]
async fn the_next_page_of_a_reader_is_a_click_on_the_page_that_holds_its_link() {
    let mut lab = Lab::start(1, 149, FAST).await;
    let url = |path: &str| format!("http://{SITE}{path}");
    let asked = |lab: &Lab, index: usize| {
        lab.chromium
            .with(|sidecar| sidecar.navigated[index]["behavior"]["entry"].clone())
    };
    let reached = |report: &kage_scheduler::FetchReport| report.last.as_ref().unwrap().reached;

    // The first page: the tab holds nothing, whatever the caller believes
    // the persona has been reading, and the address is typed.
    let first = lab
        .pipeline
        .fetch(get_from("/list", "/home"))
        .await
        .unwrap();
    assert_eq!(first.outcome, Outcome::Ok, "{:?}", first.reason);
    assert_eq!(
        asked(&lab, 0),
        json!({
            "strategy": "link", "from_url": url("/home"), "from_open_page": true,
            "max_backs": 3, "fallback": "none",
        })
    );
    assert_eq!(reached(&first), Some(Reached::Typed));

    // A link of the open page is clicked there.
    let a = lab.pipeline.fetch(get_from("/a", "/list")).await.unwrap();
    assert_eq!(reached(&a), Some(Reached::Link { backs: Some(0) }));
    let deeper = lab
        .pipeline
        .fetch(get_from("/a/notes", "/a"))
        .await
        .unwrap();
    assert_eq!(reached(&deeper), Some(Reached::Link { backs: Some(0) }));
    // A link of a page it read before: it goes back to it, as far as it
    // may, and clicks there.
    let b = lab.pipeline.fetch(get_from("/b", "/list")).await.unwrap();
    assert_eq!(asked(&lab, 3)["from_url"], url("/list"));
    assert_eq!(reached(&b), Some(Reached::Link { backs: Some(2) }));
    // A page the tab no longer holds (the click on /list dropped what
    // came after it): the address is typed, and nothing is said to be a
    // click that was none.
    let gone = lab
        .pipeline
        .fetch(get_from("/c", "/a/notes"))
        .await
        .unwrap();
    assert_eq!(reached(&gone), Some(Reached::Typed));
    // Without a page to arrive from the address is typed, as ever, and
    // so it is when the caller says that nothing leads there.
    let plain = lab.pipeline.fetch(get("/d")).await.unwrap();
    assert_eq!(asked(&lab, 5)["strategy"], "direct");
    assert_eq!(asked(&lab, 5).get("from_open_page"), None);
    assert_eq!(reached(&plain), Some(Reached::Typed));
    let typed = FetchOrder {
        arrival: Some(Arrival::Typed),
        ..get("/e")
    };
    let typed = lab.pipeline.fetch(typed).await.unwrap();
    assert_eq!(asked(&lab, 6), json!({ "strategy": "direct" }));
    assert_eq!(reached(&typed), Some(Reached::Typed));
    // All of it was one persona in one window.
    assert_eq!(lab.chromium.with(|sidecar| sidecar.contexts.len()), 1);
    assert_eq!(lab.chromium.with(|sidecar| sidecar.closed.len()), 0);
    lab.shut_down().await;

    // A policy without an entry strategy types every address.
    let mut lab = Lab::start(
        1,
        149,
        "defaults: { pacing: fast, active_hours: ignore, referer_strategy: none }\ndomains: {}\n",
    )
    .await;
    lab.pipeline.fetch(get("/list")).await.unwrap();
    let typed = lab.pipeline.fetch(get_from("/a", "/list")).await.unwrap();
    assert_eq!(asked(&lab, 1), json!({ "strategy": "none" }));
    assert_eq!(reached(&typed), Some(Reached::Typed));
    lab.shut_down().await;
}

#[tokio::test]
async fn on_the_http_path_the_page_that_holds_the_link_is_the_referer() {
    let mut lab = Lab::start(1, 149, FAST).await;
    for at in ["/list", "/a", "/b", "/c", "/d"] {
        serve(&lab, at, page(at)).await;
    }
    let reached = |report: &kage_scheduler::FetchReport| report.last.as_ref().unwrap().reached;

    // The first page of a visit is typed: a reader that comes to a site
    // comes from nowhere, whatever the caller names.
    let first = lab
        .pipeline
        .fetch(http_from("/list", "/home"))
        .await
        .unwrap();
    assert_eq!(first.outcome, Outcome::Ok, "{:?}", first.reason);
    assert_eq!(referer_at(&lab, "/list").await, None);
    assert_eq!(reached(&first), Some(Reached::Typed));

    // A later page arrives from the page that holds its link: here the
    // one before it, and then one read earlier, which is not the last.
    let a = lab.pipeline.fetch(http_from("/a", "/list")).await.unwrap();
    assert_eq!(
        referer_at(&lab, "/a").await,
        Some(format!("http://{SITE}/list"))
    );
    assert_eq!(reached(&a), Some(Reached::Link { backs: None }));
    let b = lab.pipeline.fetch(http_from("/b", "/list")).await.unwrap();
    assert_eq!(
        referer_at(&lab, "/b").await,
        Some(format!("http://{SITE}/list"))
    );
    assert_eq!(reached(&b), Some(Reached::Link { backs: None }));
    // Without one it is the page before it, as it was.
    let c = lab.pipeline.fetch(http("/c")).await.unwrap();
    assert_eq!(
        referer_at(&lab, "/c").await,
        Some(format!("http://{SITE}/b"))
    );
    assert_eq!(reached(&c), Some(Reached::Link { backs: None }));
    // A page nothing it read leads to comes from nowhere: no page is
    // named that does not hold its link.
    let typed = FetchOrder {
        arrival: Some(Arrival::Typed),
        ..http("/d")
    };
    let d = lab.pipeline.fetch(typed).await.unwrap();
    assert_eq!(referer_at(&lab, "/d").await, None);
    assert_eq!(reached(&d), Some(Reached::Typed));
    lab.shut_down().await;
}

#[tokio::test]
async fn a_block_in_the_browser_closes_it_and_another_persona_comes() {
    let mut lab = Lab::start(3, 149, FAST).await;
    lab.chromium
        .with(|sidecar| sidecar.next_pages.push_back(waf_block()));

    let report = lab.pipeline.fetch(get("/page")).await.unwrap();

    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    assert_eq!(report.attempts.len(), 2);
    let (blocked, served) = (&report.attempts[0], &report.attempts[1]);
    assert_eq!(blocked.outcome, Outcome::Forbidden);
    assert_eq!(blocked.status, Some(403));
    let block = blocked.block.as_ref().unwrap();
    assert_eq!(block.vendor.as_str(), "cloudflare");
    assert_ne!(blocked.persona_id, served.persona_id);

    // The blocked persona left: its window is closed, nothing of the wall
    // was taken along, and it stays away for the policy's cooldown scaled
    // by the signature's confidence.
    let (navigated, closed, exported) = lab.chromium.with(|sidecar| {
        (
            sidecar.navigated.clone(),
            sidecar.closed.clone(),
            sidecar.exported.clone(),
        )
    });
    assert_eq!(navigated.len(), 2);
    assert_eq!(closed, [navigated[0]["context_id"].as_str().unwrap()]);
    assert_eq!(exported.len(), 1);
    assert_eq!(lab.manager.contexts().open(), [served.persona_id.unwrap()]);
    let facts = lab.backend.facts(SITE).await.unwrap();
    let cooldown = (facts.states[&blocked.persona_id.unwrap()]
        .cooldown_until
        .unwrap()
        - blocked.finished_at)
        .to_std()
        .unwrap();
    let expected = Duration::from_secs_f64(24.0 * 3600.0 * f64::from(block.confidence));
    assert!(
        cooldown >= expected.mul_f64(0.89) && cooldown <= expected.mul_f64(1.11),
        "{cooldown:?} against {expected:?}"
    );
    assert_eq!(
        facts.states[&served.persona_id.unwrap()].cooldown_until,
        None
    );
    // The breaker counted it, in the store.
    let breaker = lab
        .store()
        .domain_breakers()
        .get(SITE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(breaker.block_count_1h, 1);
    assert_eq!(breaker.state, BreakerState::Closed);
    // Each attempt asked for its own behavior.
    assert_ne!(
        navigated[0]["behavior"]["seed"],
        navigated[1]["behavior"]["seed"]
    );
    lab.shut_down().await;
}

#[tokio::test]
async fn a_challenge_that_resolves_by_itself_is_the_page_and_one_that_does_not_is_a_block() {
    let mut lab = Lab::start(3, 149, FAST).await;
    let challenge = |resolved: bool| {
        json!({
            "vendors": ["cloudflare"],
            "script_hosts": ["challenges.cloudflare.com"],
            "iframe_hosts": [],
            "interstitial": true,
            "marker": "Just a moment...",
            "resolved": resolved,
            "waited_ms": 4200,
        })
    };
    lab.chromium.with(|sidecar| {
        sidecar
            .next_pages
            .push_back(json!({ "challenge": challenge(true) }));
    });

    let passed = lab.pipeline.fetch(get("/guarded")).await.unwrap();

    // The interstitial went away within the wait: the page behind it.
    assert_eq!(
        passed.outcome,
        Outcome::ChallengePassed,
        "{:?}",
        passed.reason
    );
    assert_eq!(passed.attempts.len(), 1);
    assert_eq!(passed.block, None);
    let waited = lab
        .chromium
        .with(|sidecar| sidecar.navigated[0]["challenge_wait_ms"].clone());
    assert_eq!(waited, lab.config.browser.challenge_wait_ms);

    // One that stays is a block: the persona leaves and another one tries.
    lab.chromium.with(|sidecar| {
        sidecar.next_pages.push_back(json!({
            "status": 403,
            "title": "Just a moment...",
            "dom_html": "<html><head><title>Just a moment...</title></head><body></body></html>",
            "challenge": challenge(false),
        }));
    });
    let failed = lab.pipeline.fetch(get("/guarded")).await.unwrap();
    assert_eq!(failed.outcome, Outcome::Ok, "{:?}", failed.reason);
    assert_eq!(failed.attempts.len(), 2);
    assert_eq!(failed.attempts[0].outcome, Outcome::ChallengeFailed);
    assert_ne!(failed.attempts[0].persona_id, failed.attempts[1].persona_id);
    lab.shut_down().await;
}

#[tokio::test]
async fn a_redirect_chain_that_leaves_for_a_private_address_ends_the_request() {
    let mut lab = Lab::start(2, 149, FAST).await;
    let url = Lab::url("/redirect").to_string();
    lab.chromium.with(|sidecar| {
        sidecar.pages.insert(
            url.clone(),
            json!({
                "final_url": "http://10.0.0.5/admin",
                "redirects": [{ "url": url, "status": 302, "location": "http://10.0.0.5/admin" }],
                "dom_html": "<html><body>router admin</body></html>",
            }),
        );
    });

    let report = lab.pipeline.fetch(get("/redirect")).await.unwrap();

    // The chain is checked after the navigation; no other persona would
    // be led anywhere else.
    assert_eq!(report.outcome, Outcome::SsrfDenied);
    assert_eq!(report.reason.as_deref(), Some("ssrf_denied"));
    assert_eq!(report.attempts.len(), 1);
    let last = report.last.as_ref().unwrap();
    assert_eq!(last.final_url.as_deref(), Some("http://10.0.0.5/admin"));
    // Nothing the page showed is kept, and the window is closed.
    assert_eq!(last.page.as_ref().unwrap().dom_html, None);
    assert_eq!(lab.chromium.with(|sidecar| sidecar.closed.len()), 1);
    let facts = lab.backend.facts(SITE).await.unwrap();
    assert_eq!(
        facts.states[&report.attempts[0].persona_id.unwrap()].cooldown_until,
        None
    );
    lab.shut_down().await;
}

#[tokio::test]
async fn without_an_http_path_level_one_is_refused_and_the_browser_is_not() {
    // Chromium 155: no profile exists for it, as in this release.
    let mut lab = Lab::start(2, 155, FAST).await;
    serve(&lab, "/page", page("The shop")).await;

    let report = lab.pipeline.fetch(get("/page")).await.unwrap();

    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    assert_eq!(report.attempts.len(), 1);
    assert_eq!(report.attempts[0].render, Render::Browser);
    // No handoff: there is no jar to hand to.
    let last = report.last.as_ref().unwrap();
    assert_eq!(
        (last.cookies_to_browser, last.cookies_to_http),
        (None, None)
    );
    assert_eq!(lab.chromium.with(|sidecar| sidecar.exported.len()), 0);
    assert_eq!(lab.received("/page").await.len(), 0);

    let refused = lab.pipeline.fetch(http("/page")).await.unwrap();
    assert_eq!(refused.outcome, Outcome::ProviderError);
    assert_eq!(refused.reason.as_deref(), Some("no_persona"));
    assert_eq!(refused.attempts, []);
    lab.shut_down().await;
}

#[tokio::test]
async fn a_plain_client_names_itself_and_leaves_through_the_sites_scouts_proxy() {
    let mut lab = Lab::start(2, 149, FAST).await;
    serve(
        &lab,
        "/plain",
        page("The shop").insert_header("set-cookie", "sid=abc; Path=/"),
    )
    .await;

    let report = lab
        .pipeline
        .fetch(at(Level::Plain, "/plain"))
        .await
        .unwrap();

    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    assert_eq!(report.persona, None);
    let [attempt] = report.attempts.as_slice() else {
        panic!("one attempt, not {:?}", report.attempts);
    };
    assert_eq!(
        (attempt.persona_id, attempt.engine, attempt.exit),
        (None, None, Some(Exit::Pool))
    );
    assert_eq!(attempt.render, Render::Http);
    assert_eq!(attempt.seed, None);
    let fetched = report.last.as_ref().unwrap().fetched.as_ref().unwrap();
    assert!(fetched.text().contains("<h1>The shop</h1>"));

    // The site saw a client that names itself, and nothing of a person.
    let received = lab.received("/plain").await;
    assert_eq!(received.len(), 1);
    assert_eq!(
        header(&received[0], "user-agent"),
        Some(DEFAULT_PLAIN_USER_AGENT.to_owned())
    );
    assert_eq!(header(&received[0], "accept"), Some("*/*".to_owned()));
    assert_eq!(header(&received[0], "accept-language"), None);
    assert_eq!(header(&received[0], "referer"), None);

    // It left through the proxy of the site's scout, with the provider's
    // credentials added by the forwarder; the other proxy saw nothing.
    let site = kage_crawl::site::of_host(SITE);
    let scout = lab.backend.scout(&site).await.unwrap().unwrap();
    let scout = persona_of(&lab, scout);
    let own = lab.proxy_of(&scout);
    let plain = own.plain();
    assert_eq!(plain.len(), 1);
    assert_eq!(plain[0].target, format!("http://{SITE}/plain"));
    assert_eq!(
        plain[0].header("proxy-authorization"),
        Some(basic(PROXY_USER, PROXY_PASS))
    );
    for other in lab.proxies.iter().filter(|proxy| proxy.addr != own.addr) {
        assert_eq!(other.seen().len(), 0);
    }
    assert!(lab.pool.last_traffic(scout.proxy_id).is_some());

    // Its bytes are nobody's: counted toward the site, through the pool.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let counted = lab.forwarder.meter().drain();
    assert_eq!(counted.len(), 1, "{counted:?}");
    assert_eq!((counted[0].persona, counted[0].egress), (None, Exit::Pool));
    assert_eq!(counted[0].domain, SITE);
    assert_eq!((counted[0].bytes_up, counted[0].bytes_down), own.bytes());

    // It keeps nothing: the next one sends no cookie, from the same exit.
    let again = lab
        .pipeline
        .fetch(at(Level::Plain, "/plain"))
        .await
        .unwrap();
    assert_eq!(again.outcome, Outcome::Ok, "{:?}", again.reason);
    let received = lab.received("/plain").await;
    assert_eq!(received.len(), 2);
    assert_eq!(header(&received[1], "cookie"), None);
    assert_eq!(own.plain().len(), 2);

    // Nobody's calendar counted a page; the store has them with their
    // exit and their level.
    let facts = lab.backend.facts(SITE).await.unwrap();
    assert!(facts.states.is_empty(), "{:?}", facts.states);
    let store = lab.store();
    let attempts = store
        .attempts()
        .list_for_request(report.request)
        .await
        .unwrap();
    assert_eq!(attempts[0].exit, Some(Exit::Pool));
    assert_eq!(attempts[0].persona_id, None);
    let request = store.requests().get(report.request).await.unwrap().unwrap();
    assert_eq!(request.level, Some(Level::Plain));
    lab.shut_down().await;
}

#[tokio::test]
async fn a_domain_that_blocks_is_paused_in_the_store() {
    let rules = "defaults: { pacing: fast, active_hours: ignore, \
                 breaker_threshold: 2, breaker_pause: 30m, retries: 1 }\ndomains: {}\n";
    let mut lab = Lab::start(3, 149, rules).await;
    lab.chromium.with(|sidecar| {
        sidecar.next_pages.push_back(waf_block());
        sidecar.next_pages.push_back(waf_block());
    });

    let report = lab.pipeline.fetch(get("/page")).await.unwrap();

    assert_eq!(report.outcome, Outcome::Forbidden);
    assert_eq!(report.attempts.len(), 2);
    let breaker = lab
        .store()
        .domain_breakers()
        .get(SITE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(breaker.state, BreakerState::Open);
    assert_eq!(breaker.trips, 1);
    let pause = breaker.paused_until.unwrap() - report.attempts[1].finished_at;
    assert_eq!(pause.num_minutes(), 30);
    assert!(lab.alerts.kinds().contains(&AlertKind::DomainPaused));

    // What a restart would read: the next request is answered at once.
    let refused = lab.pipeline.fetch(get("/other")).await.unwrap();
    assert_eq!(refused.outcome, Outcome::ProviderError);
    assert_eq!(refused.reason.as_deref(), Some("domain_paused"));
    assert_eq!(refused.retry_at, breaker.paused_until);
    assert_eq!(lab.chromium.with(|sidecar| sidecar.navigated.len()), 2);
    lab.shut_down().await;
}

#[tokio::test]
async fn nothing_is_fetched_while_a_leak_is_on_record() {
    let mut lab = Lab::start(2, 149, FAST).await;
    serve(&lab, "/page", page("The shop")).await;
    let leaking = lab.personas[0].proxy_id;
    let at = lab.now();
    // A probe through the proxy showed the host's own address.
    let report = ProbeReport {
        at,
        exit: Ok(ExitObservation {
            ip: "192.0.2.77".parse().unwrap(),
            latency_ms: 3,
            connect_ms: None,
            tls_ms: None,
        }),
        leak: Some(true),
        geo: None,
    };
    lab.pool.record_probe(leaking, &report).await.unwrap();
    assert!(lab.pool.egress_unsafe().await.unwrap());

    let refused = lab.pipeline.fetch(http("/page")).await.unwrap();

    assert_eq!(refused.outcome, Outcome::ProviderError);
    assert_eq!(refused.reason.as_deref(), Some("egress_unsafe"));
    assert_eq!(refused.attempts, []);
    assert_eq!(lab.received("/page").await.len(), 0);

    // The operator looked into it and cleared the flag.
    lab.pool.clear_egress_unsafe().await.unwrap();
    let served = lab.pipeline.fetch(http("/page")).await.unwrap();
    assert_eq!(served.outcome, Outcome::Ok, "{:?}", served.reason);
    lab.shut_down().await;
}

/// Asks the forwarder for a tunnel as a persona; the head of the answer.
async fn tunnel_as(lab: &Lab, persona: PersonaId) -> String {
    let mut stream = TcpStream::connect(lab.forwarder.local_addr())
        .await
        .unwrap();
    let credential = basic(&persona.to_string(), &lab.tokens.token(persona));
    stream
        .write_all(
            format!(
                "CONNECT {SITE}:80 HTTP/1.1\r\nHost: {SITE}:80\r\nProxy-Authorization: {credential}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut answer = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !answer.windows(4).any(|window| window == b"\r\n\r\n") {
        match tokio::time::timeout(Duration::from_secs(3), stream.read(&mut chunk)).await {
            Ok(Ok(read)) if read > 0 => answer.extend_from_slice(&chunk[..read]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&answer).into_owned()
}

#[tokio::test]
async fn the_forwarder_relays_only_for_personas_that_may_send() {
    let mut lab = Lab::start(3, 149, FAST).await;
    let (active, failing) = (lab.personas[0].clone(), lab.personas[1].clone());

    // A persona whose proxy carries traffic.
    let head = tunnel_as(&lab, active.id).await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    assert_eq!(lab.proxy_of(&active).tunnels().len(), 1);

    // One whose proxy went into backoff: nothing of it leaves, not even a
    // browser's background traffic.
    let mut state = ProxyState::Healthy;
    for _ in 0..30 {
        let report = ProbeReport {
            at: lab.now(),
            exit: Err(HealthFailure::Unreachable),
            leak: None,
            geo: None,
        };
        let proxy = lab
            .pool
            .record_probe(failing.proxy_id, &report)
            .await
            .unwrap()
            .unwrap();
        state = proxy.state;
        if state == ProxyState::Backoff {
            break;
        }
    }
    assert_eq!(state, ProxyState::Backoff);
    let head = tunnel_as(&lab, failing.id).await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    assert_eq!(lab.proxy_of(&failing).seen().len(), 0);

    // A credential that is well formed for a persona that does not exist.
    let nobody = PersonaId::from_uuid(Uuid::now_v7());
    let head = tunnel_as(&lab, nobody).await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");

    // And the scheduler does not send work to the persona in backoff.
    serve(&lab, "/page", page("The shop")).await;
    for _ in 0..3 {
        let report = lab.pipeline.fetch(http("/page")).await.unwrap();
        assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
        assert_ne!(report.attempts[0].persona_id, Some(failing.id));
    }
    lab.shut_down().await;
}

#[tokio::test]
async fn what_the_forwarder_counted_is_kept_by_the_hour_and_judged_against_the_cap() {
    let mut lab = Lab::start(1, 149, FAST).await;
    serve(&lab, "/page", page("The shop")).await;
    let report = lab.pipeline.fetch(http("/page")).await.unwrap();
    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    let persona = lab.personas[0].clone();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (sent, received) = lab.proxies[0].bytes();

    let ledger = Arc::new(StoreLedger::new(lab.store().clone(), Arc::clone(&lab.pool)));
    let budget_of = |cap_bytes: u64, hard_stop: bool| {
        let ledger: Arc<dyn Ledger> = ledger.clone();
        let alerts: Arc<dyn AlertSink> = lab.alerts.clone();
        Bandwidth::new(
            Arc::clone(lab.forwarder.meter()),
            ledger,
            BudgetRules {
                cap_bytes,
                warn_at: 0.8,
                hard_stop,
            },
            alerts,
            Arc::clone(lab.pool.clock()),
        )
    };
    let roomy = budget_of(250_000_000_000, true);

    roomy.flush().await.unwrap();

    // In the store: by the hour, per persona and per domain.
    let term = ledger.term(lab.now());
    assert!(term.start <= lab.now() && lab.now() < term.end);
    let bandwidth = lab.store().bandwidth();
    let total = bandwidth.total_since(term.start, Exit::Pool).await.unwrap();
    assert_eq!((total.bytes_up, total.bytes_down), (sent, received));
    let by_persona = bandwidth.by_persona_since(term.start).await.unwrap();
    assert_eq!(by_persona.len(), 1);
    assert_eq!(by_persona[0].0, persona.id);
    let by_domain = bandwidth.by_domain_since(term.start, 10).await.unwrap();
    assert_eq!(by_domain.len(), 1);
    assert_eq!(by_domain[0].0, SITE);
    assert_ne!(by_domain[0].0, IDLE_DOMAIN);

    let budget = roomy.assess().await.unwrap();
    assert_eq!(budget.counted_bytes, sent + received);
    assert_eq!(budget.level, BudgetLevel::Ok);
    assert!(!budget.stop);

    // With a cap this page already passed, everything stops: the
    // scheduler refuses, the forwarder relays nothing new, the browsers
    // are closed.
    let tight = budget_of(1_000, true);
    let budget = tight.assess().await.unwrap();
    assert_eq!(budget.level, BudgetLevel::Exhausted);
    assert!(budget.stop);
    assert!(
        lab.alerts
            .kinds()
            .contains(&AlertKind::BandwidthBudgetExhausted)
    );
    lab.scheduler.set_bandwidth_stop(budget.stop);
    lab.forwarder.set_stop(Some("bandwidth_exhausted"));
    lab.manager.contexts().close_all().await;
    let refused = lab.pipeline.fetch(http("/page")).await.unwrap();
    assert_eq!(refused.reason.as_deref(), Some("bandwidth_exhausted"));
    let head = tunnel_as(&lab, persona.id).await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");

    // An alert that is on record for the term is not raised again by a
    // process that starts later.
    for kind in [
        AlertKind::BandwidthBudgetWarning,
        AlertKind::BandwidthBudgetExhausted,
    ] {
        let alert = Alert::new(kind, AlertSeverity::Warning, "on record", json!({})).at(lab.now());
        lab.store()
            .health_events()
            .record_alert(SubjectKind::System, &alert)
            .await
            .unwrap();
    }
    let before = lab.alerts.alerts().len();
    let restarted = budget_of(1_000, true);
    assert!(restarted.assess().await.unwrap().stop);
    assert_eq!(lab.alerts.alerts().len(), before);
    lab.shut_down().await;
}
