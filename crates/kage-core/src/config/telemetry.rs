//! Configuration of traces (spec §20): where the spans of the process go.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::duration::HumanDuration;

/// Traces. Without an endpoint the spans and their ids are still made, so
/// that every log line of a request carries its trace id and a sidecar's
/// lines share it, and nothing leaves the process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Where traces are sent, as OTLP over HTTP with protobuf: the address
    /// of a collector with its path, `http://otel-collector:4318/v1/traces`
    /// in the `observability` profile. Asked directly, never through a
    /// proxy. `None` sends nothing.
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    /// The `service.name` the traces of this process carry.
    #[serde(default = "default_service_name")]
    pub service_name: String,
    /// The longest one export to the collector may take.
    #[serde(default = "default_export_timeout")]
    pub export_timeout: HumanDuration,
}

fn default_service_name() -> String {
    "kage".to_owned()
}

const fn default_export_timeout() -> HumanDuration {
    HumanDuration::from_secs(10)
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            otlp_endpoint: None,
            service_name: default_service_name(),
            export_timeout: default_export_timeout(),
        }
    }
}

/// Checks the section.
///
/// # Errors
///
/// A sentence that names the first key that is out of range.
pub(super) fn validate(telemetry: &TelemetryConfig) -> Result<(), String> {
    if let Some(endpoint) = &telemetry.otlp_endpoint {
        let parsed = Url::parse(endpoint).ok().filter(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
        });
        let Some(parsed) = parsed else {
            return Err(
                "telemetry.otlp_endpoint must be an http or https address without credentials \
                 or a query, as http://otel-collector:4318/v1/traces"
                    .to_owned(),
            );
        };
        if !parsed.path().ends_with("/v1/traces") {
            return Err(
                "telemetry.otlp_endpoint is taken as written and must end in /v1/traces, as \
                 http://otel-collector:4318/v1/traces"
                    .to_owned(),
            );
        }
    }
    let name = &telemetry.service_name;
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(
            "telemetry.service_name is 1 to 64 letters, digits, dots, hyphens and underscores"
                .to_owned(),
        );
    }
    let timeout = telemetry.export_timeout.get();
    if timeout < Duration::from_millis(100) || timeout > Duration::from_mins(2) {
        return Err("telemetry.export_timeout must be between 100ms and 2m".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{TelemetryConfig, validate};
    use crate::duration::HumanDuration;

    fn with(change: impl FnOnce(&mut TelemetryConfig)) -> Result<(), String> {
        let mut telemetry = TelemetryConfig::default();
        change(&mut telemetry);
        validate(&telemetry)
    }

    #[test]
    fn nothing_is_sent_unless_an_endpoint_is_named() {
        let telemetry = TelemetryConfig::default();
        assert_eq!(telemetry.otlp_endpoint, None);
        assert_eq!(telemetry.service_name, "kage");
        assert_eq!(telemetry.export_timeout.get(), Duration::from_secs(10));
        assert_eq!(validate(&telemetry), Ok(()));
        assert_eq!(
            serde_json::from_str::<TelemetryConfig>("{}").unwrap(),
            telemetry
        );
        assert!(serde_json::from_str::<TelemetryConfig>(r#"{"endpoint":"x"}"#).is_err());
    }

    #[test]
    fn an_endpoint_is_a_collectors_address_with_its_path() {
        for good in [
            "http://otel-collector:4318/v1/traces",
            "https://collector.example:4318/v1/traces",
        ] {
            assert_eq!(
                with(|telemetry| telemetry.otlp_endpoint = Some(good.to_owned())),
                Ok(()),
                "{good}"
            );
        }
        for bad in [
            "otel-collector:4318",
            "http://otel-collector:4318",
            "http://otel-collector:4318/v1/metrics",
            "ftp://otel-collector/v1/traces",
            "http://user:secret@otel-collector:4318/v1/traces",
            "http://otel-collector:4318/v1/traces?token=x",
            "",
        ] {
            let said =
                with(|telemetry| telemetry.otlp_endpoint = Some(bad.to_owned())).unwrap_err();
            assert!(said.starts_with("telemetry.otlp_endpoint"), "{bad}: {said}");
            // What was written is not repeated: it may hold a credential.
            assert!(!said.contains("secret"), "{said}");
        }
    }

    #[test]
    fn the_name_and_the_timeout_are_bounded() {
        type Change = fn(&mut TelemetryConfig);
        let cases: [(Change, &str); 5] = [
            (
                |telemetry| telemetry.service_name = String::new(),
                "telemetry.service_name",
            ),
            (
                |telemetry| telemetry.service_name = "a".repeat(65),
                "telemetry.service_name",
            ),
            (
                |telemetry| telemetry.service_name = "kage prod".to_owned(),
                "telemetry.service_name",
            ),
            (
                |telemetry| {
                    telemetry.export_timeout = HumanDuration::new(Duration::from_millis(99));
                },
                "telemetry.export_timeout",
            ),
            (
                |telemetry| telemetry.export_timeout = HumanDuration::from_secs(121),
                "telemetry.export_timeout",
            ),
        ];
        for (change, key) in cases {
            assert!(with(change).unwrap_err().starts_with(key));
        }
        let edges: [Change; 4] = [
            |telemetry| telemetry.service_name = "a".repeat(64),
            |telemetry| telemetry.service_name = "kage-prod_2.eu".to_owned(),
            |telemetry| {
                telemetry.export_timeout = HumanDuration::new(Duration::from_millis(100));
            },
            |telemetry| telemetry.export_timeout = HumanDuration::from_mins(2),
        ];
        for edge in edges {
            assert_eq!(with(edge), Ok(()));
        }
    }
}
