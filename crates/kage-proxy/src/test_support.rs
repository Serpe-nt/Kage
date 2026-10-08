//! Builders shared by this crate's unit tests.

use chrono::{DateTime, TimeZone, Utc};
use kage_core::domain::{Proxy, ProxyProviderKind, ProxyScheme, ProxyState};
use kage_core::ids::ProxyId;
use kage_core::secret::SealedSecret;

/// A fixed instant, mid-month, so tests that move a few days stay inside
/// one calendar month unless they mean to leave it.
pub(crate) fn noon() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0)
        .single()
        .unwrap_or_default()
}

/// A new Webshare proxy in Germany that was never checked.
pub(crate) fn proxy(provider_ref: &str) -> Proxy {
    Proxy {
        id: ProxyId::new(),
        provider: ProxyProviderKind::Webshare,
        provider_ref: provider_ref.to_owned(),
        host: "198.51.100.10".to_owned(),
        port: 8080,
        username: SealedSecret::new(vec![1]),
        password: SealedSecret::new(vec![2]),
        scheme: ProxyScheme::Http,
        country: "DE".parse().ok(),
        city: Some("Berlin".to_owned()),
        asn: None,
        state: ProxyState::New,
        consecutive_failures: 0,
        backoff_count: 0,
        backoff_until: None,
        failing_since: None,
        replacement: None,
        last_health: None,
        created_at: noon(),
        retired_at: None,
    }
}
