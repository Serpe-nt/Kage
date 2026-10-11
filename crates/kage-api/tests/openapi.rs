//! The committed OpenAPI document (`crates/kage-api/openapi.json`) is what
//! the server serves, and is the contract a generated client is built
//! from (spec §16, §21). `just openapi` writes it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use serde_json::Value;

/// The document as `kage openapi` prints it.
fn generated() -> String {
    let mut text = serde_json::to_string_pretty(&kage_api::openapi()).unwrap();
    text.push('\n');
    text
}

fn committed() -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/openapi.json");
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{path}: {error}; run `just openapi`"))
        .replace("\r\n", "\n")
}

#[test]
fn the_committed_document_is_the_one_the_server_serves() {
    assert!(
        committed() == generated(),
        "crates/kage-api/openapi.json is out of date: run `just openapi` and commit the result"
    );
}

/// Every `$ref` of the document, wherever it is.
fn refs(value: &Value, found: &mut BTreeSet<String>) {
    match value {
        Value::Object(members) => {
            for (name, member) in members {
                if name == "$ref"
                    && let Some(target) = member.as_str()
                {
                    found.insert(target.to_owned());
                }
                refs(member, found);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| refs(item, found)),
        _ => {}
    }
}

#[test]
fn the_document_holds_together() {
    let document: Value = serde_json::from_str(&committed()).unwrap();
    assert_eq!(document["openapi"], "3.1.0");

    // Every reference points at something that is there.
    let mut found = BTreeSet::new();
    refs(&document, &mut found);
    assert!(!found.is_empty());
    for target in &found {
        let pointer = target
            .strip_prefix('#')
            .unwrap_or_else(|| panic!("{target} leaves the document"));
        assert!(
            document.pointer(pointer).is_some(),
            "{target} points at nothing"
        );
    }

    // Every operation has an id of its own, a summary, a tag the document
    // describes, and says what it answers; every operation but the three
    // public ones needs a key.
    let tags: BTreeSet<&str> = document["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tag| tag["name"].as_str().unwrap())
        .collect();
    let mut ids = BTreeSet::new();
    for (path, item) in document["paths"].as_object().unwrap() {
        for (verb, operation) in item.as_object().unwrap() {
            let name = format!("{verb} {path}");
            let id = operation["operationId"]
                .as_str()
                .unwrap_or_else(|| panic!("{name} has no operationId"));
            assert!(ids.insert(id.to_owned()), "{id} is used twice");
            assert!(operation["summary"].is_string(), "{name} has no summary");
            for tag in operation["tags"].as_array().unwrap() {
                assert!(tags.contains(tag.as_str().unwrap()), "{name}: {tag}");
            }
            let responses = operation["responses"].as_object().unwrap();
            assert!(
                responses.keys().any(|status| status.starts_with('2')),
                "{name} documents no success"
            );
            let public = matches!(path.as_str(), "/healthz" | "/readyz");
            assert_eq!(
                operation["security"].is_array(),
                !public,
                "{name}: security"
            );
            // A problem is a problem document, never plain JSON. (A
            // readiness that fails is a report of what is missing.)
            for (status, response) in responses {
                let failure = status.starts_with('4') || status.starts_with('5');
                if failure && path != "/readyz" {
                    let content = response["content"].as_object();
                    if let Some(content) = content {
                        assert!(
                            content.contains_key("application/problem+json"),
                            "{name} {status}: {content:?}"
                        );
                    }
                }
            }
        }
    }

    // The request of a fetch is closed: a field the API does not know is
    // an error, in the document as in the server.
    let request = &document["components"]["schemas"]["FetchRequest"];
    assert_eq!(request["additionalProperties"], false, "{request}");
    assert_eq!(request["required"], serde_json::json!(["url"]));
    // What the specification dropped is not in it.
    assert!(request["properties"].get("locale").is_none());
    assert!(request["properties"].get("proxy").is_none());
    // The kind of request is one of three, navigation unless said.
    let kind = &document["components"]["schemas"]["RequestKindField"];
    assert_eq!(
        kind["enum"],
        serde_json::json!(["navigation", "fetch", "xhr"])
    );
}
