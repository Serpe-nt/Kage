//! The contract test (spec §10.2, §21): the committed OpenAPI document of
//! the sidecar and the Rust side must describe the same thing.
//!
//! The types are generated from the document at build time, so they cannot
//! differ from it by construction. What can still drift is checked here:
//!
//! - every example in the document must survive the generated types
//!   unchanged, which catches a type the generator maps too loosely or too
//!   strictly;
//! - every component schema must have a type, and every operation must be
//!   one the client calls;
//! - the constants the client is written with (paths, the token header)
//!   must be the document's.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use kage_browser::IMPLEMENTED_OPERATIONS;
use kage_browser::contract::{
    self, BehaviorOptions, CONTRACT_VERSION, CaptureOptions, ClaimedOs, DwellMode, Entry,
    EntryStrategy, ErrorCode, FlowAction, FlowEvent, NavigateRequest, OPERATIONS, PageResult,
    SCHEMA_NAMES, TOKEN_HEADER, Wait, WaitUntil,
};
use serde_json::{Value, json};

fn document() -> Value {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sidecar/packages/contract/openapi.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn every_example_of_the_document_survives_the_rust_types_unchanged() {
    let examples: Value = serde_json::from_str(contract::EXAMPLES_JSON).unwrap();
    let examples = examples.as_object().unwrap();
    assert!(
        examples.len() >= 15,
        "the document carries {} schemas with examples",
        examples.len()
    );
    let mut checked = 0;
    for (schema, values) in examples {
        for (index, example) in values.as_array().unwrap().iter().enumerate() {
            let written = contract::roundtrip(schema, example.clone())
                .unwrap_or_else(|| panic!("{schema} has examples but no generated type"))
                .unwrap_or_else(|error| {
                    panic!("example {index} of {schema} does not fit its type: {error}")
                });
            assert!(
                common::same_json(&written, example),
                "example {index} of {schema} changed on its way through the Rust type:
{written:#}
was:
{example:#}"
            );
            checked += 1;
        }
    }
    assert!(checked >= 25, "only {checked} examples were checked");
}

#[test]
fn every_schema_has_a_type_and_the_client_calls_every_operation() {
    let document = document();
    let schemas = document["components"]["schemas"].as_object().unwrap();
    // A schema that is only an option of a union is a variant of that
    // union's enum; every other one is a type of its own.
    let variants: BTreeSet<&str> = schemas
        .values()
        .filter_map(|schema| schema["oneOf"].as_array())
        .flatten()
        .filter_map(|option| option["$ref"].as_str())
        .filter_map(|target| target.rsplit('/').next())
        .collect();
    let expected: BTreeSet<&str> = schemas
        .keys()
        .map(String::as_str)
        .filter(|name| !variants.contains(name))
        .collect();
    assert_eq!(
        SCHEMA_NAMES.iter().copied().collect::<BTreeSet<_>>(),
        expected
    );
    assert!(expected.contains("PageResult") && expected.contains("FlowAction"));
    assert!(variants.contains("GotoAction") && !expected.contains("GotoAction"));

    // Since M5 the client calls every operation: none is left for later.
    let operations: BTreeSet<&str> = OPERATIONS.iter().map(|(id, _, _)| *id).collect();
    let implemented: BTreeSet<&str> = IMPLEMENTED_OPERATIONS.iter().copied().collect();
    assert_eq!(
        operations, implemented,
        "the contract and the client disagree about the operations: add the client method and \
         its name to IMPLEMENTED_OPERATIONS"
    );
    assert_eq!(
        implemented.len(),
        IMPLEMENTED_OPERATIONS.len(),
        "an operation is listed twice"
    );
    assert!(implemented.contains("flow"));
}

#[test]
fn the_constants_are_the_documents() {
    let document = document();
    assert_eq!(document["info"]["version"], CONTRACT_VERSION);
    assert_eq!(TOKEN_HEADER, "x-kage-sidecar-token");
    let by_id = |wanted: &str| {
        OPERATIONS
            .iter()
            .find(|(id, _, _)| *id == wanted)
            .map(|(_, method, path)| (*method, *path))
    };
    assert_eq!(by_id("health"), Some(("GET", "/v1/health")));
    assert_eq!(by_id("open_context"), Some(("POST", "/v1/contexts")));
    assert_eq!(
        by_id("close_context"),
        Some(("DELETE", "/v1/contexts/{id}"))
    );
    assert_eq!(
        by_id("navigate"),
        Some(("POST", "/v1/contexts/{id}/navigate"))
    );
    assert_eq!(by_id("flow"), Some(("POST", "/v1/contexts/{id}/flow")));
    assert_eq!(OPERATIONS.len(), 10);
}

#[test]
fn enums_are_written_as_the_contract_writes_them() {
    assert_eq!(
        serde_json::to_value(WaitUntil::NetworkIdle).unwrap(),
        "network_idle"
    );
    assert_eq!(WaitUntil::NetworkIdle.as_str(), "network_idle");
    assert_eq!(WaitUntil::NetworkIdle.to_string(), "network_idle");
    assert_eq!(
        ClaimedOs::ALL
            .iter()
            .map(|os| os.as_str())
            .collect::<Vec<_>>(),
        ["windows", "macos", "linux"]
    );
    assert_eq!(
        serde_json::from_value::<ErrorCode>(json!("context_limit")).unwrap(),
        ErrorCode::ContextLimit
    );
    // The values are the ones kage-core uses for the same things.
    for os in kage_core::domain::ClaimedOs::ALL {
        assert!(
            ClaimedOs::ALL
                .iter()
                .any(|ours| ours.as_str() == os.as_str())
        );
    }
    assert!(serde_json::from_value::<WaitUntil>(json!("whenever")).is_err());
}

#[test]
fn an_optional_field_is_left_out_and_a_nullable_one_is_written_as_null() {
    let mut request = NavigateRequest {
        url: "https://example.org/".to_owned(),
        wait: Wait {
            until: WaitUntil::Load,
            selector: None,
            extra_ms: None,
        },
        capture: CaptureOptions {
            dom: true,
            raw: false,
            xhr: None,
            console: false,
            screenshot: None,
            pdf: None,
        },
        timeout_ms: Some(60_000),
        challenge_wait_ms: None,
        dwell_ms: None,
        max_body_bytes: None,
        artifact_prefix: None,
        behavior: None,
    };
    let bare = json!({
        "url": "https://example.org/",
        "wait": { "until": "load" },
        // Required and nullable: written, as null.
        "capture": { "dom": true, "raw": false, "xhr": null, "console": false, "screenshot": null, "pdf": null },
        "timeout_ms": 60_000,
    });
    // No `behavior` at all is a bare page load, so the key is left out.
    assert_eq!(serde_json::to_value(&request).unwrap(), bare);

    // The behavior options of contract 1.1.0 follow the same rule one level
    // down: what is not set is not written, and the sidecar's defaults apply.
    request.behavior = Some(BehaviorOptions {
        seed: None,
        entry: Some(Entry {
            strategy: EntryStrategy::Link,
            from_url: Some("https://www.example.org/".to_owned()),
            fallback: None,
        }),
        scroll_depth: Some(0.5),
        dwell: Some(DwellMode::Human),
        wander: None,
    });
    let mut with_behavior = bare;
    with_behavior["behavior"] = json!({
        "entry": { "strategy": "link", "from_url": "https://www.example.org/" },
        "scroll_depth": 0.5,
        "dwell": "human",
    });
    let written = serde_json::to_value(&request).unwrap();
    assert!(common::same_json(&written, &with_behavior), "{written:#}");
    assert_eq!(
        serde_json::from_value::<NavigateRequest>(written).unwrap(),
        request
    );
    assert_eq!(
        EntryStrategy::ALL
            .iter()
            .map(|strategy| strategy.as_str())
            .collect::<Vec<_>>(),
        ["none", "direct", "link", "search_engine"]
    );
}

#[test]
fn a_union_is_chosen_by_its_tag_and_a_newer_sidecar_may_say_more() {
    let action: FlowAction =
        serde_json::from_value(json!({ "action": "goto", "url": "https://example.org/" })).unwrap();
    assert!(
        matches!(&action, FlowAction::Goto { url, human: None, .. } if url == "https://example.org/")
    );
    assert_eq!(
        serde_json::to_value(&action).unwrap(),
        json!({ "action": "goto", "url": "https://example.org/" })
    );
    assert!(serde_json::from_value::<FlowAction>(json!({ "action": "teleport" })).is_err());

    let event: FlowEvent = serde_json::from_value(json!({
        "type": "step", "index": 0, "action": "goto", "ok": true, "duration_ms": 12,
        "error": null, "capture": null
    }))
    .unwrap();
    assert!(matches!(
        event,
        FlowEvent::Step {
            index: 0,
            ok: true,
            ..
        }
    ));

    // A response field this build does not know yet is ignored, so a newer
    // sidecar that reports more does not break an older Kage.
    let examples: Value = serde_json::from_str(contract::EXAMPLES_JSON).unwrap();
    let mut page = examples["PageResult"][0].clone();
    page["added_in_a_later_version"] = json!({ "anything": 1 });
    let parsed: PageResult = serde_json::from_value(page).unwrap();
    assert_eq!(parsed.status, Some(200));
    // A field the contract requires is still required.
    let mut incomplete = examples["PageResult"][0].clone();
    incomplete.as_object_mut().unwrap().remove("final_url");
    assert!(serde_json::from_value::<PageResult>(incomplete).is_err());
}
