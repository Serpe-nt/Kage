//! Proxies: one static residential IP with credentials (spec §5, §7).

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::CountryCode;
use crate::ids::ProxyId;
use crate::macros::string_enum;
use crate::secret::SealedSecret;

string_enum! {
    /// Where a proxy record comes from.
    pub enum ProxyProviderKind {
        /// The Webshare list and replacement API.
        Webshare => "webshare",
        /// A fixed list from configuration.
        Static => "static",
    }
}

string_enum! {
    /// How Kage talks to the proxy. SOCKS5 is unsupported in v1.
    pub enum ProxyScheme {
        /// HTTP proxy with `CONNECT`.
        Http => "http",
    }
}

string_enum! {
    /// Position of a proxy in the pool state machine (spec §7.3).
    pub enum ProxyState {
        /// Known to the provider, not yet health-checked.
        New => "new",
        /// Passing health checks.
        Healthy => "healthy",
        /// Failing some checks; still usable.
        Degraded => "degraded",
        /// Too many failures; unused until `backoff_until`.
        Backoff => "backoff",
        /// A replacement was requested from the provider.
        Replacing => "replacing",
        /// Gone for good.
        Retired => "retired",
    }
}

impl ProxyState {
    /// Whether personas on this proxy may serve requests (spec §8.2).
    pub const fn is_usable(self) -> bool {
        matches!(self, Self::Healthy | Self::Degraded)
    }
}

/// Result of the most recent health check, stored as `last_health`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyHealth {
    /// When the check ran.
    pub checked_at: DateTime<Utc>,
    /// Whether the neutral endpoint answered through the proxy.
    pub reachable: bool,
    /// Round-trip time of the check.
    #[serde(default)]
    pub latency_ms: Option<u32>,
    /// The address the echo service saw.
    #[serde(default)]
    pub exit_ip: Option<IpAddr>,
    /// The country the echo service located the exit address in.
    #[serde(default)]
    pub exit_country: Option<CountryCode>,
    /// Whether `exit_country` equals the provider's country.
    #[serde(default)]
    pub country_match: Option<bool>,
    /// Whether the exit address was the host's own address.
    #[serde(default)]
    pub leak_detected: Option<bool>,
    /// Abuse score from the reputation provider, 0 (clean) to 100.
    #[serde(default)]
    pub reputation_score: Option<u8>,
}

/// One static residential IP with credentials.
#[derive(Debug, Clone, PartialEq)]
pub struct Proxy {
    /// Identity.
    pub id: ProxyId,
    /// Which provider owns the record.
    pub provider: ProxyProviderKind,
    /// The provider's own id for the proxy.
    pub provider_ref: String,
    /// Proxy host name or address.
    pub host: String,
    /// Proxy port.
    pub port: u16,
    /// Proxy user name.
    pub username: String,
    /// Proxy password, encrypted at rest (spec §19).
    pub password: SealedSecret,
    /// Proxy protocol.
    pub scheme: ProxyScheme,
    /// Country of the exit address, from the provider and verified by health.
    pub country: Option<CountryCode>,
    /// City of the exit address.
    pub city: Option<String>,
    /// Autonomous system of the exit address.
    pub asn: Option<String>,
    /// Pool state.
    pub state: ProxyState,
    /// Failed health checks in a row.
    pub consecutive_failures: u32,
    /// End of the current backoff, when in [`ProxyState::Backoff`].
    pub backoff_until: Option<DateTime<Utc>>,
    /// The most recent health check.
    pub last_health: Option<ProxyHealth>,
    /// When the record was created.
    pub created_at: DateTime<Utc>,
    /// When the proxy was retired.
    pub retired_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::{ProxyHealth, ProxyProviderKind, ProxyScheme, ProxyState};
    use crate::macros::test_support::assert_string_enum;

    #[test]
    fn enum_names_are_the_spec_names() {
        assert_string_enum(
            ProxyProviderKind::ALL,
            ProxyProviderKind::as_str,
            &["webshare", "static"],
        );
        assert_string_enum(ProxyScheme::ALL, ProxyScheme::as_str, &["http"]);
        assert_string_enum(
            ProxyState::ALL,
            ProxyState::as_str,
            &[
                "new",
                "healthy",
                "degraded",
                "backoff",
                "replacing",
                "retired",
            ],
        );
    }

    #[test]
    fn only_healthy_and_degraded_proxies_are_usable() {
        let usable: Vec<ProxyState> = ProxyState::ALL
            .iter()
            .copied()
            .filter(|state| state.is_usable())
            .collect();

        assert_eq!(usable, [ProxyState::Healthy, ProxyState::Degraded]);
    }

    #[test]
    fn health_json_tolerates_missing_optional_fields() {
        let health: ProxyHealth =
            serde_json::from_str(r#"{"checked_at":"2026-10-08T12:00:00Z","reachable":true}"#)
                .unwrap();

        assert!(health.reachable);
        assert_eq!(health.latency_ms, None);
        assert_eq!(health.exit_ip, None);

        let full = ProxyHealth {
            checked_at: Utc::now(),
            reachable: true,
            latency_ms: Some(120),
            exit_ip: Some("203.0.113.9".parse().unwrap()),
            exit_country: Some("DE".parse().unwrap()),
            country_match: Some(true),
            leak_detected: Some(false),
            reputation_score: Some(3),
        };
        let json = serde_json::to_string(&full).unwrap();
        assert_eq!(serde_json::from_str::<ProxyHealth>(&json).unwrap(), full);
    }
}
