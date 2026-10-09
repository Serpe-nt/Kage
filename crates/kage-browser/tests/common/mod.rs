//! Shared by the integration tests of this crate.

#![allow(dead_code, unreachable_pub, clippy::unwrap_used)]

use serde_json::Value;
use wiremock::{Match, Request};

/// Whether two JSON values say the same thing. Numbers are compared by
/// value: Rust writes a `f64` that holds a whole number as `1.0` where the
/// contract's examples write `1`, and both are the same number in JSON.
pub fn same_json(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => {
            a == b
                || a.as_f64()
                    .zip(b.as_f64())
                    .is_some_and(|(a, b)| (a - b).abs() < f64::EPSILON)
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_json(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| same_json(value, other)))
        }
        _ => a == b,
    }
}

/// Matches a request whose JSON body says the same as `expected`.
pub struct BodySays(pub Value);

impl Match for BodySays {
    fn matches(&self, request: &Request) -> bool {
        serde_json::from_slice::<Value>(&request.body).is_ok_and(|body| same_json(&body, &self.0))
    }
}
