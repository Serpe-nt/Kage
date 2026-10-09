//! Proxies: one static residential IP with credentials (spec §5, §7).

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::CountryCode;
use crate::ids::ProxyId;
use crate::macros::string_enum;
use crate::secret::{SealedSecret, Secret};

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

    /// Whether the proxy still belongs to the pool: every state but
    /// `retired`.
    pub const fn is_live(self) -> bool {
        !matches!(self, Self::Retired)
    }
}

/// Where a proxy is and how to log in, with the credentials opened.
///
/// It is what the pool hands to anything that must send traffic through a
/// proxy: the health probes and the http path. `Debug` prints the
/// credentials redacted.
#[derive(Debug, Clone)]
pub struct ProxyEndpoint {
    /// The proxy.
    pub id: ProxyId,
    /// Proxy host name or address.
    pub host: String,
    /// Proxy port.
    pub port: u16,
    /// Proxy user name.
    pub username: Secret,
    /// Proxy password.
    pub password: Secret,
    /// Proxy protocol.
    pub scheme: ProxyScheme,
}

impl ProxyEndpoint {
    /// The proxy as a URL without credentials, for example
    /// `http://198.51.100.10:8080`. An IPv6 address is bracketed.
    pub fn url(&self) -> String {
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("{}://[{}]:{}", self.scheme, self.host, self.port)
        } else {
            format!("{}://{}:{}", self.scheme, self.host, self.port)
        }
    }
}

string_enum! {
    /// Why a health check counted as failed.
    pub enum HealthFailure {
        /// No echo endpoint could be read through the proxy.
        Unreachable => "unreachable",
        /// The proxy refused the credentials.
        ProxyAuth => "proxy_auth",
        /// The exit address was the host's own address.
        Leak => "leak",
    }
}

string_enum! {
    /// Why a replacement was requested from the provider.
    pub enum ReplaceReason {
        /// Sustained failure: backoff cycles and time both ran out.
        Unreachable => "unreachable",
        /// The reputation provider scored the address as bad.
        Reputation => "reputation",
        /// An operator asked for it.
        Operator => "operator",
    }
}

/// What a geo lookup said about the exit address (spec §7.4). This observed
/// record, not the provider's, is what a persona's country, timezone and
/// locale are derived from: it is what a target's own geo database will most
/// likely agree with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeoRecord {
    /// Country of the exit address.
    pub country: CountryCode,
    /// City, when the lookup names one.
    #[serde(default)]
    pub city: Option<String>,
    /// Region or state, when the lookup names one.
    #[serde(default)]
    pub region: Option<String>,
    /// Autonomous system number, for example `AS3320`.
    #[serde(default)]
    pub asn: Option<String>,
    /// Name of the network operator.
    #[serde(default)]
    pub org: Option<String>,
    /// IANA time zone of the exit address.
    #[serde(default)]
    pub timezone: Option<String>,
    /// The address that was looked up.
    pub ip: IpAddr,
    /// When the lookup ran.
    pub checked_at: DateTime<Utc>,
}

impl GeoRecord {
    /// Whether two records describe the same place and network. The lookup
    /// time is not part of the comparison.
    pub fn same_location(&self, other: &Self) -> bool {
        self.country == other.country
            && self.city == other.city
            && self.region == other.region
            && self.asn == other.asn
            && self.org == other.org
            && self.timezone == other.timezone
            && self.ip == other.ip
    }
}

/// What the health checks last found, stored as `last_health`. Reachability
/// is refreshed by every probe; the geo and reputation parts keep the result
/// of their own, slower lookups.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyHealth {
    /// When the last probe ran.
    pub checked_at: DateTime<Utc>,
    /// Whether an echo endpoint answered through the proxy.
    pub reachable: bool,
    /// Why the last probe failed.
    #[serde(default)]
    pub failure: Option<HealthFailure>,
    /// Round-trip time of the probe, including the new tunnel.
    #[serde(default)]
    pub latency_ms: Option<u32>,
    /// Of the round trip, opening the tunnel through the proxy: TCP to the
    /// proxy and the `CONNECT`. A proxy that is slow here is slow on every
    /// new connection, whatever the target.
    #[serde(default)]
    pub connect_ms: Option<u32>,
    /// Of the round trip, the TLS handshake with the echo.
    #[serde(default)]
    pub tls_ms: Option<u32>,
    /// The address the echo service saw.
    #[serde(default)]
    pub exit_ip: Option<IpAddr>,
    /// The country the geo lookup located the exit address in.
    #[serde(default)]
    pub exit_country: Option<CountryCode>,
    /// Whether `exit_country` equals the provider's country.
    #[serde(default)]
    pub country_match: Option<bool>,
    /// Whether the exit address was the host's own address. `None` when the
    /// host address is unknown, or after an operator cleared a leak.
    #[serde(default)]
    pub leak_detected: Option<bool>,
    /// The observed geo record; authoritative for persona generation.
    #[serde(default)]
    pub geo: Option<GeoRecord>,
    /// Geo lookups in a row whose country differed from the provider's.
    #[serde(default)]
    pub geo_mismatches: u32,
    /// Abuse score from the reputation provider, 0 (clean) to 100.
    #[serde(default)]
    pub reputation_score: Option<u8>,
    /// How the reputation provider classifies the address, for example
    /// `Fixed Line ISP` or `Data Center/Web Hosting/Transit`.
    #[serde(default)]
    pub usage_type: Option<String>,
    /// When the reputation was last looked up.
    #[serde(default)]
    pub reputation_checked_at: Option<DateTime<Utc>>,
    /// Reputation lookups in a row at or above the bad score.
    #[serde(default)]
    pub reputation_bad_lookups: u32,
}

impl ProxyHealth {
    /// A record with nothing known but the time and reachability.
    pub fn new(checked_at: DateTime<Utc>, reachable: bool) -> Self {
        Self {
            checked_at,
            reachable,
            failure: None,
            latency_ms: None,
            connect_ms: None,
            tls_ms: None,
            exit_ip: None,
            exit_country: None,
            country_match: None,
            leak_detected: None,
            geo: None,
            geo_mismatches: 0,
            reputation_score: None,
            usage_type: None,
            reputation_checked_at: None,
            reputation_bad_lookups: 0,
        }
    }
}

/// A replacement requested from the provider for one proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyReplacement {
    /// When Kage asked. Replacements are counted against the monthly budget
    /// by this time.
    pub requested_at: DateTime<Utc>,
    /// Why.
    pub reason: ReplaceReason,
    /// The provider's handle for the request, once it accepted it.
    pub ticket: Option<String>,
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
    /// Proxy user name, encrypted at rest (spec §19).
    pub username: SealedSecret,
    /// Proxy password, encrypted at rest (spec §19).
    pub password: SealedSecret,
    /// Proxy protocol.
    pub scheme: ProxyScheme,
    /// Country of the exit address as the provider states it.
    pub country: Option<CountryCode>,
    /// City of the exit address as the provider states it.
    pub city: Option<String>,
    /// Autonomous system of the exit address, from the geo lookup when one
    /// succeeded, else from the provider.
    pub asn: Option<String>,
    /// Pool state.
    pub state: ProxyState,
    /// Failed health checks in a row.
    pub consecutive_failures: u32,
    /// Backoff cycles in a row without a passing check in between.
    pub backoff_count: u32,
    /// End of the current backoff, when in [`ProxyState::Backoff`].
    pub backoff_until: Option<DateTime<Utc>>,
    /// When the current run of failed checks began.
    pub failing_since: Option<DateTime<Utc>>,
    /// The replacement requested for this proxy, if any.
    pub replacement: Option<ProxyReplacement>,
    /// What the health checks last found.
    pub last_health: Option<ProxyHealth>,
    /// When the record was created.
    pub created_at: DateTime<Utc>,
    /// When the proxy was retired.
    pub retired_at: Option<DateTime<Utc>>,
}

impl Proxy {
    /// The observed geo record, when a lookup has succeeded.
    pub fn observed_geo(&self) -> Option<&GeoRecord> {
        self.last_health.as_ref()?.geo.as_ref()
    }

    /// The country to use for persona generation and policy `countries`
    /// filters: the observed one, falling back to the provider's only when
    /// no geo lookup has succeeded yet.
    pub fn effective_country(&self) -> Option<CountryCode> {
        self.observed_geo().map(|geo| geo.country).or(self.country)
    }

    /// Whether the last probe saw the host's own address leave through this
    /// proxy. One such proxy makes the whole pool unsafe to use (see the
    /// pool's `egress_unsafe` flag).
    pub fn leak_detected(&self) -> bool {
        self.last_health
            .as_ref()
            .is_some_and(|health| health.leak_detected == Some(true))
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::{
        GeoRecord, HealthFailure, Proxy, ProxyHealth, ProxyProviderKind, ProxyScheme, ProxyState,
        ReplaceReason,
    };
    use crate::ids::ProxyId;
    use crate::macros::test_support::assert_string_enum;
    use crate::secret::SealedSecret;

    fn geo(country: &str) -> GeoRecord {
        GeoRecord {
            country: country.parse().unwrap(),
            city: Some("Vienna".to_owned()),
            region: None,
            asn: Some("AS8447".to_owned()),
            org: Some("A1 Telekom Austria AG".to_owned()),
            timezone: Some("Europe/Vienna".to_owned()),
            ip: "203.0.113.9".parse().unwrap(),
            checked_at: Utc::now(),
        }
    }

    fn proxy() -> Proxy {
        Proxy {
            id: ProxyId::new(),
            provider: ProxyProviderKind::Webshare,
            provider_ref: "d-1".to_owned(),
            host: "198.51.100.10".to_owned(),
            port: 8080,
            username: SealedSecret::new(vec![1]),
            password: SealedSecret::new(vec![2]),
            scheme: ProxyScheme::Http,
            country: Some("DE".parse().unwrap()),
            city: None,
            asn: None,
            state: ProxyState::New,
            consecutive_failures: 0,
            backoff_count: 0,
            backoff_until: None,
            failing_since: None,
            replacement: None,
            last_health: None,
            created_at: Utc::now(),
            retired_at: None,
        }
    }

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
        assert_string_enum(
            HealthFailure::ALL,
            HealthFailure::as_str,
            &["unreachable", "proxy_auth", "leak"],
        );
        assert_string_enum(
            ReplaceReason::ALL,
            ReplaceReason::as_str,
            &["unreachable", "reputation", "operator"],
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
    fn every_state_but_retired_is_live() {
        let gone: Vec<ProxyState> = ProxyState::ALL
            .iter()
            .copied()
            .filter(|state| !state.is_live())
            .collect();

        assert_eq!(gone, [ProxyState::Retired]);
    }

    #[test]
    fn health_json_tolerates_missing_optional_fields() {
        let health: ProxyHealth =
            serde_json::from_str(r#"{"checked_at":"2026-10-08T12:00:00Z","reachable":true}"#)
                .unwrap();

        assert_eq!(health, ProxyHealth::new(health.checked_at, true));

        let full = ProxyHealth {
            failure: Some(HealthFailure::Leak),
            latency_ms: Some(120),
            exit_ip: Some("203.0.113.9".parse().unwrap()),
            exit_country: Some("AT".parse().unwrap()),
            country_match: Some(false),
            leak_detected: Some(false),
            geo: Some(geo("AT")),
            geo_mismatches: 2,
            reputation_score: Some(3),
            usage_type: Some("Fixed Line ISP".to_owned()),
            reputation_checked_at: Some(Utc::now()),
            reputation_bad_lookups: 1,
            ..ProxyHealth::new(Utc::now(), true)
        };
        let json = serde_json::to_string(&full).unwrap();
        assert_eq!(serde_json::from_str::<ProxyHealth>(&json).unwrap(), full);
    }

    #[test]
    fn the_observed_country_wins_over_the_providers() {
        let mut proxy = proxy();
        assert_eq!(proxy.effective_country().unwrap().as_str(), "DE");
        assert!(proxy.observed_geo().is_none());

        // A probe without a geo lookup changes nothing.
        proxy.last_health = Some(ProxyHealth::new(Utc::now(), true));
        assert_eq!(proxy.effective_country().unwrap().as_str(), "DE");

        proxy.last_health = Some(ProxyHealth {
            geo: Some(geo("AT")),
            ..ProxyHealth::new(Utc::now(), true)
        });
        assert_eq!(proxy.effective_country().unwrap().as_str(), "AT");
        assert_eq!(
            proxy.observed_geo().unwrap().timezone.as_deref(),
            Some("Europe/Vienna")
        );

        proxy.country = None;
        proxy.last_health = None;
        assert_eq!(proxy.effective_country(), None);
    }

    #[test]
    fn a_leak_is_read_from_the_last_probe() {
        let mut proxy = proxy();
        assert!(!proxy.leak_detected());

        for (flag, expected) in [(None, false), (Some(false), false), (Some(true), true)] {
            proxy.last_health = Some(ProxyHealth {
                leak_detected: flag,
                ..ProxyHealth::new(Utc::now(), true)
            });
            assert_eq!(proxy.leak_detected(), expected, "{flag:?}");
        }
    }

    #[test]
    fn geo_records_compare_by_place_not_by_time() {
        let first = geo("AT");
        let mut later = first.clone();
        later.checked_at += chrono::Duration::hours(24);
        assert!(first.same_location(&later));

        let mut moved = first.clone();
        moved.timezone = Some("Europe/Berlin".to_owned());
        assert!(!first.same_location(&moved));
        assert!(!first.same_location(&geo("DE")));
    }
}
