//! A flow as one browser attempt (spec §10.5), one layer below the compose
//! stack: the scheduler over Postgres, the real executor, and a mock
//! sidecar that walks the steps and answers the contract's NDJSON stream.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod common;

use common::lab::{Lab, SITE};
use kage_browser::contract::FlowAction;
use kage_core::domain::{Level, Render};
use kage_core::outcome::Outcome;
use kage_core::schedule::queue::Priority;
use kage_scheduler::pipeline::attempt_seed;
use kage_scheduler::{Backend, Capture, FetchOrder, FlowPlan, Keep};
use serde_json::{Value, json};

/// Every request goes to the browser.
const BROWSER_FAST: &str = "defaults: { pacing: fast, active_hours: ignore }\ndomains: {}\n";

fn site(path: &str) -> String {
    format!("http://{SITE}{path}")
}

fn actions(steps: &Value) -> Vec<FlowAction> {
    serde_json::from_value(steps.clone()).expect("steps the contract takes")
}

/// What an `extract` step captures: the document.
fn document() -> Value {
    json!({
        "dom": true, "raw": false, "xhr": null, "console": false,
        "screenshot": null, "pdf": null,
    })
}

/// The names of the steps a sidecar was asked for, in order.
fn asked_steps(asked: &Value) -> Vec<String> {
    asked["actions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|action| action["action"].as_str().unwrap_or_default().to_owned())
        .collect()
}

const SEARCH: [&str; 7] = [
    "goto", "type", "press", "extract", "goto", "extract", "back",
];

/// A search of the shop: its start page, a query typed and sent, the list
/// captured, one product opened and left again.
fn search() -> Value {
    json!([
        { "action": "goto", "url": site("/start") },
        { "action": "type", "selector": "input[name=q]", "text": "lamp" },
        { "action": "press", "key": "Enter" },
        { "action": "extract", "capture": document() },
        { "action": "goto", "url": site("/product/7") },
        { "action": "extract", "capture": document() },
        { "action": "back" },
    ])
}

fn flow(steps: &Value) -> FetchOrder {
    FetchOrder {
        level: Level::Human,
        flow: Some(FlowPlan {
            actions: actions(steps),
            typos: false,
        }),
        ..FetchOrder::get(Lab::url("/start"), Priority::Job)
    }
}

/// A page a Cloudflare WAF answers with.
fn waf_block() -> Value {
    json!({
        "status": 403,
        "title": "Attention Required! | Cloudflare",
        "dom_html": "<html><head><title>Attention Required! | Cloudflare</title></head>\
                     <body><h1>Sorry, you have been blocked</h1>\
                     <p>You are unable to access this site. Cloudflare Ray ID: 8f1</p></body></html>",
    })
}

#[tokio::test]
async fn a_flow_is_one_browser_attempt_paced_like_a_person_with_a_capture_per_step() {
    let mut lab = Lab::start(2, 149, BROWSER_FAST).await;
    lab.chromium.with(|sidecar| {
        sidecar
            .pages
            .insert(site("/product/7"), json!({ "title": "A lamp" }));
    });

    let report = lab.pipeline.fetch(flow(&search())).await.unwrap();

    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    let [attempt] = report.attempts.as_slice() else {
        panic!("one attempt, not {:?}", report.attempts);
    };
    assert_eq!(attempt.render, Render::Browser);
    let persona = lab
        .personas
        .iter()
        .find(|persona| Some(persona.id) == attempt.persona_id)
        .unwrap();

    // The sidecar was asked for the whole script once, in the persona's
    // context, and for no navigation beside it.
    let (flows, navigated) = lab
        .chromium
        .with(|sidecar| (sidecar.flows.clone(), sidecar.navigated.clone()));
    assert_eq!(navigated.len(), 0);
    let [asked] = flows.as_slice() else {
        panic!("one flow, not {flows:?}");
    };
    assert_eq!(asked_steps(asked), SEARCH);
    assert_eq!(asked["actions"][0]["url"], site("/start"));
    assert_eq!(asked["actions"][1]["text"], "lamp");
    assert_eq!(asked["actions"][3]["capture"]["dom"], true);
    // Always paced: the persona's own dwell on every page it arrives at,
    // never below what the policy allows; every time from the attempt's
    // seed, so the same flow replays.
    let pace = &asked["behavior"]["pace"];
    assert!(pace.is_object(), "{asked}");
    assert_eq!(pace["dwell_ms"], Value::Null);
    let policy = lab.policies.current().domains.resolve(SITE);
    assert_eq!(
        pace["min_dwell_ms"].as_u64(),
        u64::try_from(policy.min_dwell.get().as_millis()).ok()
    );
    let seed = attempt_seed(&persona.seed, report.request, 1);
    assert_eq!(attempt.seed, Some(seed));
    assert_eq!(asked["behavior"]["seed"], seed.to_string());
    assert_eq!(asked["behavior"]["typos"], false);
    // The page it ends on is always captured: the classifier reads it.
    assert_eq!(asked["capture"]["dom"], true);
    assert_eq!(
        asked["timeout_ms"].as_u64(),
        u64::try_from(lab.config.browser.timeout.get().as_millis()).ok()
    );

    // Every step as it ran, and what the capturing ones brought back.
    let last = report.last.as_ref().unwrap();
    let run = last.flow.as_ref().expect("the attempt ran a flow");
    assert!(run.ok);
    assert_eq!(run.failed(), None);
    let names: Vec<&str> = run.steps.iter().map(|step| step.action.as_str()).collect();
    assert_eq!(names, SEARCH);
    for (step, index) in run.steps.iter().zip(0_u32..) {
        assert_eq!(step.index, index);
        assert!(step.ok);
        assert_eq!(step.error, None);
        assert_eq!(step.paced_ms, Some(7));
        assert_eq!(step.capture.is_some(), matches!(index, 3 | 5), "{index}");
    }
    let listing = run.steps[3].capture.as_ref().unwrap();
    assert_eq!(listing.final_url, site("/start"));
    let product = run.steps[5].capture.as_ref().unwrap();
    assert_eq!(product.final_url, site("/product/7"));
    assert_eq!(product.title.as_deref(), Some("A lamp"));
    // The flow went back: the page it ended on is the list's.
    assert_eq!(last.final_url.as_deref(), Some(site("/start").as_str()));

    // The browser stays the persona's, open for what comes next.
    assert_eq!(lab.chromium.with(|sidecar| sidecar.closed.len()), 0);
    assert_eq!(lab.manager.contexts().open(), [persona.id]);

    // A capture is kept as a request and a result of its own; the flow's
    // attempt stays with the flow's request.
    let kept = lab
        .pipeline
        .keep_capture(Capture {
            page: product,
            keep: Keep {
                dom: true,
                ..Keep::default()
            },
            api_key: None,
            job: None,
            session: None,
            persona: report.persona.clone(),
            extract: Some(kage_extract::Options::default()),
            spec: json!({ "flow": report.request, "step": 5, "action": "extract" }),
            level: Level::Human,
        })
        .await
        .unwrap();
    assert_ne!(kept.request, report.request);
    assert_eq!((kept.outcome, kept.status), (Outcome::Ok, Some(200)));
    let store = lab.store();
    let stored = store.requests().get(kept.request).await.unwrap().unwrap();
    assert_eq!(stored.url, site("/product/7"));
    assert_eq!(stored.domain, SITE);
    assert_eq!(stored.outcome, Some(Outcome::Ok));
    assert_eq!(stored.spec["step"], 5);
    let result = store
        .results()
        .get_by_request(kept.request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        result.final_url.as_deref(),
        Some(site("/product/7").as_str())
    );
    assert_eq!(
        result.persona.as_ref().map(|persona| persona.id),
        Some(persona.id)
    );
    assert_eq!(
        store
            .attempts()
            .list_for_request(kept.request)
            .await
            .unwrap()
            .len(),
        0
    );
    lab.shut_down().await;
}

#[tokio::test]
async fn a_request_may_slow_a_flow_down_but_never_below_the_policys_dwell() {
    let rules = "defaults: { pacing: fast, active_hours: ignore, min_dwell: 3s }\n\
                 domains: {}\n";
    let mut lab = Lab::start(1, 149, rules).await;
    let with_dwell = |millis: u64| FetchOrder {
        behavior: Some(kage_scheduler::BehaviorWish {
            dwell: Some(std::time::Duration::from_millis(millis)),
            ..kage_scheduler::BehaviorWish::default()
        }),
        ..flow(&json!([{ "action": "goto", "url": site("/start") }]))
    };

    let slow = lab.pipeline.fetch(with_dwell(9_000)).await.unwrap();
    let hasty = lab.pipeline.fetch(with_dwell(200)).await.unwrap();

    assert_eq!(slow.outcome, Outcome::Ok, "{:?}", slow.reason);
    assert_eq!(hasty.outcome, Outcome::Ok, "{:?}", hasty.reason);
    let flows = lab.chromium.with(|sidecar| sidecar.flows.clone());
    let paces: Vec<(Option<u64>, Option<u64>)> = flows
        .iter()
        .map(|asked| {
            let pace = &asked["behavior"]["pace"];
            (pace["dwell_ms"].as_u64(), pace["min_dwell_ms"].as_u64())
        })
        .collect();
    // What was asked for where it is allowed; the policy's floor where it
    // is not.
    assert_eq!(
        paces,
        [(Some(9_000), Some(3_000)), (Some(3_000), Some(3_000))]
    );
    let applied = slow.last.as_ref().unwrap().behavior.as_ref().unwrap();
    assert!(applied.from_request);
    assert_eq!(applied.dwell, Some(std::time::Duration::from_secs(9)));
    lab.shut_down().await;
}

#[tokio::test]
async fn a_flow_at_level_two_rests_its_hand_and_dwells_the_policys_own_time() {
    let rules = "defaults: { pacing: fast, active_hours: ignore, min_dwell: 3s }\n\
                 domains: {}\n";
    let mut lab = Lab::start(1, 149, rules).await;
    let at = |level| FetchOrder {
        level,
        ..flow(&json!([{ "action": "goto", "url": site("/start") }]))
    };

    let still = lab.pipeline.fetch(at(Level::Fast)).await.unwrap();
    let person = lab.pipeline.fetch(at(Level::Human)).await.unwrap();

    assert_eq!(still.outcome, Outcome::Ok, "{:?}", still.reason);
    assert_eq!(person.outcome, Outcome::Ok, "{:?}", person.reason);
    let flows = lab.chromium.with(|sidecar| sidecar.flows.clone());
    let paces: Vec<&Value> = flows
        .iter()
        .map(|asked| &asked["behavior"]["pace"])
        .collect();
    // Level 2: the floor drawn upward, the hand at rest.
    assert_eq!(paces[0]["dwell_ms"], 4_500);
    assert_eq!(paces[0]["min_dwell_ms"], 3_000);
    assert_eq!(paces[0]["wander"], false);
    // Level 3: the persona's own dwell, the hand as a person's.
    assert_eq!(paces[1]["dwell_ms"], Value::Null);
    assert_eq!(paces[1]["wander"], Value::Null);
    let applied = still.last.as_ref().unwrap().behavior.as_ref().unwrap();
    assert!(!applied.from_request);
    assert_eq!(applied.dwell, Some(std::time::Duration::from_millis(4_500)));
    lab.shut_down().await;
}

#[tokio::test]
async fn a_step_that_fails_ends_the_flow_and_no_other_persona_tries_the_script() {
    let mut lab = Lab::start(3, 149, BROWSER_FAST).await;
    lab.chromium.with(|sidecar| {
        sidecar.next_flows.push_back(json!({
            "fail": { "index": 1, "error": "selector_not_found: input[name=q]" },
        }));
    });

    let report = lab.pipeline.fetch(flow(&search())).await.unwrap();

    // The script is what failed: the page was served, nobody was turned
    // away, and another household would meet the same page.
    assert_eq!(report.outcome, Outcome::ProviderError);
    assert_eq!(report.reason.as_deref(), Some("flow_step_failed"));
    assert_eq!(report.attempts.len(), 1);
    assert_eq!(report.block, None);
    let last = report.last.as_ref().unwrap();
    let run = last.flow.as_ref().unwrap();
    assert!(!run.ok);
    assert_eq!(run.steps.len(), 2);
    let failed = run.failed().unwrap();
    assert_eq!(failed.index, 1);
    assert_eq!(
        failed.error.as_deref(),
        Some("selector_not_found: input[name=q]")
    );
    assert!(
        last.classification
            .evidence
            .iter()
            .any(|evidence| evidence == "step:1:selector_not_found: input[name=q]"),
        "{:?}",
        last.classification.evidence
    );
    assert_eq!(lab.chromium.with(|sidecar| sidecar.flows.len()), 1);
    // Nothing to answer for.
    let facts = lab.backend.facts(SITE).await.unwrap();
    assert_eq!(
        facts.states[&report.attempts[0].persona_id.unwrap()].cooldown_until,
        None
    );
    assert_eq!(facts.breaker, None);
    lab.shut_down().await;
}

#[tokio::test]
async fn a_page_that_did_not_arrive_is_tried_again_from_the_first_step() {
    let mut lab = Lab::start(2, 149, BROWSER_FAST).await;
    lab.chromium.with(|sidecar| {
        sidecar.next_flows.push_back(json!({
            "fail": { "index": 0, "error": "timeout: the page did not load in 30000 ms" },
        }));
    });

    let report = lab.pipeline.fetch(flow(&search())).await.unwrap();

    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    assert_eq!(report.attempts.len(), 2);
    assert_eq!(report.attempts[0].outcome, Outcome::Timeout);
    // Both times the whole script, from its first step.
    let flows = lab.chromium.with(|sidecar| sidecar.flows.clone());
    assert_eq!(flows.len(), 2);
    assert_eq!(flows[0]["actions"], flows[1]["actions"]);
    assert_ne!(flows[0]["behavior"]["seed"], flows[1]["behavior"]["seed"]);
    assert!(report.last.as_ref().unwrap().flow.as_ref().unwrap().ok);
    lab.shut_down().await;
}

#[tokio::test]
async fn a_wall_inside_a_flow_sends_another_persona_from_the_first_step() {
    let mut lab = Lab::start(3, 149, BROWSER_FAST).await;
    lab.chromium.with(|sidecar| {
        sidecar.next_flows.push_back(json!({
            // The click led to the wall; the steps after it found nothing.
            "fail": { "index": 2, "error": "timeout: no navigation after the key" },
            "final": waf_block(),
        }));
    });

    let report = lab.pipeline.fetch(flow(&search())).await.unwrap();

    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    assert_eq!(report.attempts.len(), 2);
    let (blocked, served) = (&report.attempts[0], &report.attempts[1]);
    // The wall is what the attempt is judged by, not the step that met it.
    assert_eq!(blocked.outcome, Outcome::Forbidden);
    assert_eq!(
        blocked.block.as_ref().unwrap().vendor.as_str(),
        "cloudflare"
    );
    assert_ne!(blocked.persona_id, served.persona_id);
    // The blocked persona left, its window closed, and stays away; the
    // other one ran the whole script.
    let (flows, closed) = lab
        .chromium
        .with(|sidecar| (sidecar.flows.clone(), sidecar.closed.clone()));
    assert_eq!(flows.len(), 2);
    assert_eq!(closed, [flows[0]["context_id"].as_str().unwrap()]);
    assert_eq!(asked_steps(&flows[1]), SEARCH);
    let facts = lab.backend.facts(SITE).await.unwrap();
    assert!(
        facts.states[&blocked.persona_id.unwrap()]
            .cooldown_until
            .is_some()
    );
    assert_eq!(
        facts.states[&served.persona_id.unwrap()].cooldown_until,
        None
    );
    let run = report.last.as_ref().unwrap().flow.as_ref().unwrap();
    assert!(run.ok);
    assert_eq!(run.steps.len(), 7);
    lab.shut_down().await;
}

#[tokio::test]
async fn everything_a_flow_visited_passes_the_guard_or_nothing_of_it_is_kept() {
    let mut lab = Lab::start(2, 149, BROWSER_FAST).await;
    lab.chromium.with(|sidecar| {
        // A link of the list led to an address of the host's own network,
        // and a step captured it.
        sidecar.next_flows.push_back(json!({
            "captures": { "5": {
                "url": "http://10.0.0.5/admin",
                "final_url": "http://10.0.0.5/admin",
                "dom_html": "<html><body>router admin</body></html>",
            } },
        }));
    });

    let report = lab.pipeline.fetch(flow(&search())).await.unwrap();

    assert_eq!(report.outcome, Outcome::SsrfDenied);
    assert_eq!(report.reason.as_deref(), Some("ssrf_denied"));
    // No other persona would be led anywhere else.
    assert_eq!(report.attempts.len(), 1);
    let last = report.last.as_ref().unwrap();
    assert_eq!(last.final_url.as_deref(), Some("http://10.0.0.5/admin"));
    // Not one step's capture is handed on, and the window is closed.
    assert_eq!(last.flow, None);
    assert_eq!(last.page.as_ref().unwrap().dom_html, None);
    assert_eq!(lab.chromium.with(|sidecar| sidecar.closed.len()), 1);

    // The same for the page a flow ends on, through a redirect.
    lab.chromium.with(|sidecar| {
        sidecar.next_flows.push_back(json!({
            "final": {
                "final_url": "http://169.254.169.254/latest/meta-data",
                "redirects": [{
                    "url": site("/start"),
                    "status": 302,
                    "location": "http://169.254.169.254/latest/meta-data",
                }],
            },
        }));
    });
    let again = lab.pipeline.fetch(flow(&search())).await.unwrap();
    assert_eq!(again.outcome, Outcome::SsrfDenied);
    assert_eq!(again.attempts.len(), 1);
    lab.shut_down().await;
}

#[tokio::test]
async fn a_flow_whose_answer_broke_off_is_not_taken_for_a_page() {
    let mut lab = Lab::start(2, 149, BROWSER_FAST).await;
    lab.chromium.with(|sidecar| {
        sidecar.next_flows.push_back(json!({ "interrupted": true }));
    });

    let report = lab.pipeline.fetch(flow(&search())).await.unwrap();

    // The sidecar's failure is not the site's: the script runs again.
    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    assert_eq!(report.attempts.len(), 2);
    assert_eq!(report.attempts[0].outcome, Outcome::ProviderError);
    assert_eq!(lab.chromium.with(|sidecar| sidecar.flows.len()), 2);
    // The browser whose state nobody knows was closed.
    assert_eq!(lab.chromium.with(|sidecar| sidecar.closed.len()), 1);
    lab.shut_down().await;
}
