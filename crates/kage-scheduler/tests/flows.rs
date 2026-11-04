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

use common::lab::{HIDDEN, Lab, OnFlow, SITE};
use kage_browser::contract::FlowAction;
use kage_core::domain::{BreakerState, Level, Render};
use kage_core::outcome::Outcome;
use kage_core::schedule::breaker::BreakerRules;
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
async fn a_tab_a_flows_page_opened_is_reported_with_the_step_that_opened_it() {
    let mut lab = Lab::start(2, 149, BROWSER_FAST).await;
    // The round's flow (flows-02): the step after the first opens a tab of
    // another site, which the sidecar keeps behind the flow's page and
    // closes with the flow (F184).
    lab.chromium.with(|sidecar| {
        sidecar.next_flows.push_back(json!({
            "opened": { "1": ["https://other.example/terms"] },
        }));
    });

    let report = lab.pipeline.fetch(flow(&search())).await.unwrap();

    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    let last = report.last.as_ref().unwrap();
    let run = last.flow.as_ref().unwrap();
    let opened: Vec<(&str, &[String])> = run
        .steps
        .iter()
        .map(|step| (step.action.as_str(), step.opened_tabs.as_slice()))
        .collect();
    assert_eq!(
        opened[1],
        ("type", &["https://other.example/terms".to_owned()][..])
    );
    assert!(
        opened
            .iter()
            .enumerate()
            .all(|(index, (_, tabs))| index == 1 || tabs.is_empty()),
        "{opened:?}"
    );
    // The page the flow ended on is its own, not the tab's.
    assert_eq!(last.final_url.as_deref(), Some(site("/start").as_str()));
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

/// Another site the lab's world serves, which a flow goes on to.
const PAY: &str = "pay.kage.test";

fn pay(path: &str) -> String {
    format!("http://{PAY}{path}")
}

/// A flow that starts on the shop and goes on to the other site.
fn across() -> Value {
    json!([
        { "action": "goto", "url": site("/start") },
        { "action": "extract", "capture": document() },
        { "action": "goto", "url": pay("/checkout") },
    ])
}

#[tokio::test]
async fn a_wall_at_a_flows_later_site_is_booked_to_the_site_that_showed_it() {
    let mut lab = Lab::start(2, 149, BROWSER_FAST).await;
    lab.chromium.with(|sidecar| {
        // The shop's page came; the other site answered with its wall.
        sidecar
            .next_flows
            .push_back(json!({ "final": waf_block() }));
    });

    let report = lab.pipeline.fetch(flow(&across())).await.unwrap();

    // Another persona ran the whole flow again and got through.
    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    let [blocked, served] = report.attempts.as_slice() else {
        panic!("two attempts, not {:?}", report.attempts);
    };
    assert_eq!(blocked.outcome, Outcome::Forbidden);
    assert_eq!(
        blocked.final_url.as_deref(),
        Some(pay("/checkout").as_str())
    );
    let (blocked, served) = (blocked.persona_id.unwrap(), served.persona_id.unwrap());
    assert_ne!(blocked, served);

    // The wall is the other site's: the blocked persona cools there, and
    // its breaker counts the block (jobs-06).
    let there = lab.backend.facts(PAY).await.unwrap();
    let cooled = &there.states[&blocked];
    assert!(cooled.cooldown_until.is_some());
    assert_eq!(cooled.last_outcome, Some(Outcome::Forbidden));
    assert_eq!(cooled.block_count_24h, 1);
    assert_eq!(
        there.breaker.as_ref().map(|breaker| breaker.block_count_1h),
        Some(1)
    );
    // The shop showed no wall: nobody cools there, its breaker counted
    // nothing, and the blocked persona's last word from it is a page.
    let shop = lab.backend.facts(SITE).await.unwrap();
    assert_eq!(shop.breaker, None);
    assert_eq!(shop.states[&blocked].cooldown_until, None);
    assert_eq!(shop.states[&blocked].block_count_24h, 0);
    assert_eq!(shop.states[&blocked].last_outcome, Some(Outcome::Ok));
    // The persona that got through read both sites.
    assert_eq!(there.states[&served].last_outcome, Some(Outcome::Ok));
    assert_eq!(shop.states[&served].last_outcome, Some(Outcome::Ok));
    lab.shut_down().await;
}

/// Opens the other site's breaker an hour ago with a pause of half an
/// hour: it is half open now, and the next request for it is its trial.
async fn half_open_pay(lab: &Lab) {
    let rules = BreakerRules {
        threshold: 1,
        pause: std::time::Duration::from_mins(30),
        max_pause: std::time::Duration::from_hours(6),
        trial_timeout: std::time::Duration::from_mins(10),
    };
    let opened = chrono::Utc::now() - chrono::Duration::hours(1);
    lab.backend
        .breaker_block(PAY, &rules, opened)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_flow_that_passed_a_half_open_site_and_came_back_closes_its_breaker() {
    let mut lab = Lab::start(2, 149, BROWSER_FAST).await;
    half_open_pay(&lab).await;
    // To the other site and back: the flow ends on the shop.
    let there_and_back = json!([
        { "action": "goto", "url": site("/start") },
        { "action": "goto", "url": pay("/checkout") },
        { "action": "goto", "url": site("/done") },
    ]);

    let report = lab.pipeline.fetch(flow(&there_and_back)).await.unwrap();

    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    assert_eq!(report.attempts.len(), 1);
    // The flow was the other site's trial, and that site answered: its
    // breaker closed, rather than holding every request for it paused
    // until the trial's timeout.
    let breaker = lab.backend.facts(PAY).await.unwrap().breaker.unwrap();
    assert_eq!(breaker.state, BreakerState::Closed);
    assert_eq!(breaker.trial_started_at, None);
    lab.shut_down().await;
}

#[tokio::test]
async fn a_later_site_named_with_its_root_dot_is_the_same_site() {
    let mut lab = Lab::start(2, 149, BROWSER_FAST).await;
    half_open_pay(&lab).await;
    // F104 with F101: `pay.kage.test.` is the site `pay.kage.test`, and
    // `shop.kage.test.` is the flow's own site, no later one.
    let pay_dotted = |path: &str| format!("http://{PAY}.{path}");
    let dotted = json!([
        { "action": "goto", "url": site("/start") },
        { "action": "goto", "url": pay_dotted("/checkout") },
        { "action": "goto", "url": format!("http://{SITE}./done") },
    ]);

    let report = lab.pipeline.fetch(flow(&dotted)).await.unwrap();

    // The flow was the other site's trial, and closed its breaker.
    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    let breaker = lab.backend.facts(PAY).await.unwrap().breaker.unwrap();
    assert_eq!(breaker.state, BreakerState::Closed);

    // A wall met there, on the page of the dotted name, is that site's.
    lab.chromium.with(|sidecar| {
        sidecar
            .next_flows
            .push_back(json!({ "final": waf_block() }));
    });
    let walled = json!([
        { "action": "goto", "url": site("/start") },
        { "action": "goto", "url": pay_dotted("/checkout") },
    ]);
    let report = lab.pipeline.fetch(flow(&walled)).await.unwrap();
    let blocked = report.attempts[0].persona_id.unwrap();
    assert_eq!(report.attempts[0].outcome, Outcome::Forbidden);
    let there = lab.backend.facts(PAY).await.unwrap();
    assert!(there.states[&blocked].cooldown_until.is_some());
    assert_eq!(lab.backend.facts(SITE).await.unwrap().breaker, None);

    // Nothing is kept under a dotted name.
    for name in [format!("{PAY}."), format!("{SITE}.")] {
        let facts = lab.backend.facts(&name).await.unwrap();
        assert_eq!(facts.breaker, None, "{name}");
        assert!(facts.states.is_empty(), "{name}: {:?}", facts.states);
    }
    lab.shut_down().await;
}

#[tokio::test]
async fn a_flow_that_ended_before_a_half_open_site_answered_gives_its_trial_back() {
    let mut lab = Lab::start(2, 149, BROWSER_FAST).await;
    half_open_pay(&lab).await;
    let steps = json!([
        { "action": "goto", "url": site("/start") },
        { "action": "click", "selector": "a.checkout" },
        { "action": "goto", "url": pay("/checkout") },
    ]);
    lab.chromium.with(|sidecar| {
        sidecar.next_flows.push_back(json!({
            "fail": { "index": 1, "error": "selector_not_found: a.checkout" },
        }));
    });

    let report = lab.pipeline.fetch(flow(&steps)).await.unwrap();

    assert_eq!(report.reason.as_deref(), Some("flow_step_failed"));
    // The other site was never asked: its pause and counts stay, and the
    // trial is free again at once.
    let breaker = lab.backend.facts(PAY).await.unwrap().breaker.unwrap();
    assert_eq!(breaker.state, BreakerState::Open);
    assert_eq!(breaker.trial_started_at, None);
    assert_eq!(breaker.trips, 1);
    let plain = lab.pipeline.fetch(flow(&across())).await.unwrap();
    assert_eq!(plain.outcome, Outcome::Ok, "{:?}", plain.reason);
    let breaker = lab.backend.facts(PAY).await.unwrap().breaker.unwrap();
    assert_eq!(breaker.state, BreakerState::Closed);
    lab.shut_down().await;
}

#[tokio::test]
async fn a_flows_later_site_with_one_seat_waits_for_another_personas_visit_to_end() {
    // F188 (left-01): the other site has one seat (`fast` with
    // `domain_concurrency: 1`). One persona reads a page of it; a flow that
    // only the other persona may take goes there next. It waits for the
    // reader's visit to end, as a page of the site would, instead of a
    // second address reading the site at once.
    let mut lab = Box::pin(Lab::start_with(2, 149, BROWSER_FAST, |config| {
        config.scheduler.visit_idle =
            kage_core::duration::HumanDuration::new(std::time::Duration::from_secs(3));
        config.scheduler.visitor_wait =
            kage_core::duration::HumanDuration::new(std::time::Duration::from_secs(1));
    }))
    .await;
    let profiles = lab.policy_dir().join("pacing-profiles.yaml");
    let mut text = std::fs::read_to_string(&profiles).unwrap();
    text.push_str(
        "\nsingle:\n  gap_ms: { dist: uniform, min: 300, max: 1200 }\n  burst_pages: { min: 50, max: 50 }\n  burst_break_ms: { dist: uniform, min: 0, max: 0 }\n  daily_cap_per_persona_per_domain: 5000\n  domain_concurrency: 1\n  domain_rpm: 300\n",
    );
    std::fs::write(&profiles, text).unwrap();
    std::fs::write(
        lab.policy_dir().join("domains.yaml"),
        format!(
            "defaults: {{ pacing: fast, active_hours: ignore }}\ndomains:\n  {PAY}:\n    pacing: single\n"
        ),
    )
    .unwrap();
    lab.policies.reload().unwrap();
    let (reader, other) = (lab.personas[0].id, lab.personas[1].id);

    let read = lab
        .pipeline
        .fetch(FetchOrder {
            level: Level::Human,
            exclude: vec![other],
            ..FetchOrder::get(url::Url::parse(&pay("/page")).unwrap(), Priority::Sync)
        })
        .await
        .unwrap();
    assert_eq!(read.outcome, Outcome::Ok, "{:?}", read.reason);
    assert_eq!(read.attempts[0].persona_id, Some(reader));
    let read_at = read.attempts[0].started_at;

    let report = lab
        .pipeline
        .fetch(FetchOrder {
            exclude: vec![reader],
            priority: Priority::Sync,
            ..flow(&across())
        })
        .await
        .unwrap();

    assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
    assert_eq!(report.attempts[0].persona_id, Some(other));
    // Not before the reader's visit was over: three seconds after its page
    // began, with nothing more asked of it.
    let began = report.attempts[0].started_at;
    assert!(
        began - read_at >= chrono::Duration::seconds(3),
        "the flow began {} after the reader's page",
        began - read_at
    );
    lab.shut_down().await;
}

#[tokio::test]
async fn a_persona_that_cools_at_a_flows_later_site_does_not_run_the_flow() {
    let mut lab = Lab::start(2, 149, BROWSER_FAST).await;
    let (cooling, other) = (lab.personas[0].id, lab.personas[1].id);
    let now = chrono::Utc::now();
    lab.backend
        .record_outcome(
            cooling,
            PAY,
            Outcome::Forbidden,
            Some(std::time::Duration::from_hours(20)),
            now,
        )
        .await
        .unwrap();

    for _ in 0..3 {
        let report = lab.pipeline.fetch(flow(&across())).await.unwrap();
        assert_eq!(report.outcome, Outcome::Ok, "{:?}", report.reason);
        assert_eq!(report.attempts[0].persona_id, Some(other));
    }
    // The flow's first site alone may still go to the cooling persona.
    let flows = lab.chromium.with(|sidecar| sidecar.flows.len());
    assert_eq!(flows, 3);
    lab.shut_down().await;
}

/// The forwarder's refusal of `hidden.kage.test` for every persona of the
/// lab, as the sidecar's hook of the next flow: it is recorded once the
/// attempt is under way, after the executor took its mark of the
/// forwarder's denials, and before the flow is answered (F102).
fn refuse_hidden(lab: &Lab) -> OnFlow {
    let denials = std::sync::Arc::clone(lab.forwarder.denials());
    let personas: Vec<_> = lab.personas.iter().map(|persona| persona.id).collect();
    OnFlow(Box::new(move || {
        for persona in personas {
            denials.record(
                kage_scheduler::Caller::Persona(persona),
                "hidden.kage.test",
                "hidden.kage.test resolves to 10.0.0.9, in Private",
            );
        }
    }))
}

#[tokio::test]
async fn a_flow_whose_address_the_forwarder_refused_is_ssrf_denied_at_once() {
    let mut lab = Lab::start(2, 149, BROWSER_FAST).await;
    // Under `egress.mode: direct` the forwarder refuses a name for its
    // private address; the browser sees a connection that failed, and so
    // does the flow's goto (F102).
    let hidden = "http://hidden.kage.test/admin";
    let refused_flow = || {
        flow(&json!([
            { "action": "goto", "url": site("/start") },
            { "action": "goto", "url": hidden },
        ]))
    };
    let failed_goto = json!({
        "fail": { "index": 1, "error": "proxy_failed: net::ERR_TUNNEL_CONNECTION_FAILED" },
        "final": {
            "url": hidden,
            "final_url": hidden,
            "status": null,
            "dom_html": "<html><body>the browser's own error page</body></html>",
            "error": {
                "code": "proxy_failed",
                "message": "net::ERR_TUNNEL_CONNECTION_FAILED",
                "net_error": "ERR_TUNNEL_CONNECTION_FAILED",
            },
        },
    });
    // The forwarder refuses the name while the flow runs, for whichever
    // persona runs it: when the sidecar is asked for the flow.
    let refusing = refuse_hidden(&lab);
    lab.chromium.with(|sidecar| {
        sidecar.next_flows.push_back(failed_goto.clone());
        sidecar.on_flow = Some(refusing);
    });

    let report = lab.pipeline.fetch(refused_flow()).await.unwrap();

    // The guard's outcome, at the first attempt, nothing of it kept.
    assert_eq!(report.outcome, Outcome::SsrfDenied, "{:?}", report.reason);
    assert_eq!(report.reason.as_deref(), Some("ssrf_denied"));
    assert_eq!(report.attempts.len(), 1);
    let last = report.last.as_ref().unwrap();
    assert_eq!(last.flow, None);
    assert_eq!(last.page.as_ref().unwrap().dom_html, None);
    assert!(
        last.classification
            .evidence
            .iter()
            .any(|evidence| evidence.starts_with("refused:hidden.kage.test (")),
        "{:?}",
        last.classification.evidence
    );

    // The same failure without a refusal is a failed connection, tried
    // again by another persona.
    lab.chromium
        .with(|sidecar| sidecar.next_flows.push_back(failed_goto.clone()));
    let failed = lab.pipeline.fetch(refused_flow()).await.unwrap();
    assert_eq!(failed.attempts[0].outcome, Outcome::ProviderError);
    assert_eq!(failed.attempts.len(), 2);
    lab.shut_down().await;
}

#[tokio::test]
async fn a_flow_that_a_click_sent_to_a_refused_address_is_ssrf_denied_at_once() {
    let mut lab = Lab::start(2, 149, BROWSER_FAST).await;
    // No `goto` names the address: a link of the page does. The sidecar
    // ends the flow at the click and reports the page where the tab was
    // sent (F102).
    let hidden = "http://hidden.kage.test/admin";
    let clicked = flow(&json!([
        { "action": "goto", "url": site("/start") },
        { "action": "click", "selector": "a.admin" },
    ]));
    let message = format!(
        "navigation_failed: the browser could not load {hidden}: it shows its own error page"
    );
    lab.chromium.with(|sidecar| {
        sidecar.next_flows.push_back(json!({
            "fail": { "index": 1, "error": message },
            "final": {
                "url": site("/start"),
                "final_url": hidden,
                "status": null,
                "dom_html": null,
                "error": {
                    "code": "navigation_failed",
                    "message": message,
                    "net_error": null,
                },
            },
        }));
        sidecar.on_flow = Some(refuse_hidden(&lab));
    });

    let report = lab.pipeline.fetch(clicked).await.unwrap();

    assert_eq!(report.outcome, Outcome::SsrfDenied, "{:?}", report.reason);
    assert_eq!(report.attempts.len(), 1);
    assert_eq!(report.last.as_ref().unwrap().flow, None);
    lab.shut_down().await;
}

/// F57 (QA round two, ssrf-01): under `security.resolve_on_host` every page
/// a flow reached is judged by its addresses after the fact, as a
/// navigation's chain is: a click that took the tab to a name of the
/// operator's network is `ssrf_denied`, nothing of the flow kept, even when
/// the page came back.
#[tokio::test]
async fn with_resolve_on_host_a_flows_pages_are_judged_by_their_addresses() {
    let mut lab = Box::pin(Lab::start_with(2, 149, BROWSER_FAST, |config| {
        config.security.resolve_on_host = true;
    }))
    .await;
    let hidden = format!("http://{HIDDEN}/admin");
    let clicked = flow(&json!([
        { "action": "goto", "url": site("/start") },
        { "action": "click", "selector": "a.admin" },
    ]));
    lab.chromium.with(|sidecar| {
        sidecar.next_flows.push_back(json!({
            "final": {
                "final_url": hidden,
                "status": 200,
                "dom_html": "<html><body>INTERNAL</body></html>",
            },
        }));
    });

    let report = lab.pipeline.fetch(clicked).await.unwrap();

    assert_eq!(report.outcome, Outcome::SsrfDenied, "{:?}", report.reason);
    assert_eq!(report.attempts.len(), 1);
    let last = report.last.as_ref().unwrap();
    assert_eq!(last.flow, None);
    assert_eq!(last.page.as_ref().unwrap().dom_html, None);
    assert_eq!(last.final_url.as_deref(), Some(hidden.as_str()));
    assert!(
        last.classification
            .evidence
            .iter()
            .any(|seen| seen.contains(&hidden) && seen.contains("10.0.0.9")),
        "{:?}",
        last.classification.evidence
    );
    lab.shut_down().await;
}
