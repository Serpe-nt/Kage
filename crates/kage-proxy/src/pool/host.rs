//! The rows that stand for the host's own connection (spec §25, ADR 0012).
//!
//! Under `egress.mode: direct` every host persona has a row of its own,
//! provider `host`: the persona manager generates a persona for it like
//! for any usable proxy, and the forwarder relays that persona's traffic
//! with no upstream. A host row is never synced with a provider, never
//! probed through, never part of the leak check, the outage share or the
//! capacity, and never replaced. Its place is what the host's own geo
//! lookup saw ([`Pool::record_host_place`]), or what the operator declared
//! (`egress.direct.country`), which the lookup then only verifies.

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use kage_core::alert::AlertSeverity;
use kage_core::domain::{
    CountryCode, GeoRecord, Proxy, ProxyProviderKind, ProxyScheme, ProxyState,
};
use kage_core::ids::ProxyId;
use serde_json::json;

use super::{Pool, PoolError, PoolEvent, ReconcileReport};
use crate::crypto::CredentialField;
use crate::health::GeoLookup;
use crate::machine::{ExitObservation, ProbeReport};

/// What a host row names as its address: nothing is ever connected to it.
pub const HOST_ADDRESS: &str = "host.invalid";

/// The provider reference of the `n`th host row, from 1.
pub fn host_ref(n: u32) -> String {
    format!("host-{n}")
}

/// The position of a host row by its provider reference.
fn position(provider_ref: &str) -> Option<u32> {
    provider_ref.strip_prefix("host-")?.parse().ok()
}

impl Pool {
    /// The live host rows, in order.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn host_rows(&self) -> Result<Vec<Proxy>, PoolError> {
        let mut rows = self
            .store
            .proxies()
            .list_live(Some(ProxyProviderKind::Host))
            .await?;
        rows.sort_by_key(|row| position(&row.provider_ref).unwrap_or(u32::MAX));
        Ok(rows)
    }

    /// Makes the host's rows what the configuration says: `count` of them,
    /// live and usable, the missing ones made with `country` as their
    /// declared place, and those beyond `count` retired (their personas
    /// with them). Returns the live host rows, in order. `count` 0 retires
    /// every one: under `egress.mode: proxies` none may exist.
    ///
    /// # Errors
    ///
    /// Fails when the store fails or a credential cannot be sealed.
    pub async fn keep_host_rows(
        &self,
        count: u32,
        country: Option<CountryCode>,
    ) -> Result<Vec<Proxy>, PoolError> {
        let now = self.clock.now();
        let mut report = ReconcileReport::default();
        for row in self.host_rows().await? {
            if position(&row.provider_ref).is_none_or(|place| place > count) {
                self.retire(&row, now, "egress_mode", &mut report).await?;
            }
        }
        let known: Vec<String> = self
            .host_rows()
            .await?
            .into_iter()
            .map(|row| row.provider_ref)
            .collect();
        for place in 1..=count {
            let provider_ref = host_ref(place);
            if known.contains(&provider_ref) {
                continue;
            }
            self.add_host_row(provider_ref, country, now).await?;
        }
        self.host_rows().await
    }

    async fn add_host_row(
        &self,
        provider_ref: String,
        country: Option<CountryCode>,
        now: DateTime<Utc>,
    ) -> Result<(), PoolError> {
        let id = ProxyId::new();
        let proxy = Proxy {
            id,
            provider: ProxyProviderKind::Host,
            provider_ref,
            host: HOST_ADDRESS.to_owned(),
            port: 1,
            // Nothing opens these: the forwarder has no upstream for the
            // row.
            username: self.cipher.seal(id, CredentialField::Username, "")?,
            password: self.cipher.seal(id, CredentialField::Password, "")?,
            scheme: ProxyScheme::Http,
            country,
            city: None,
            asn: None,
            // Usable from the start: it is never probed into health.
            state: ProxyState::Healthy,
            consecutive_failures: 0,
            backoff_count: 0,
            backoff_until: None,
            failing_since: None,
            replacement: None,
            last_health: None,
            created_at: now,
            retired_at: None,
        };
        let added = self.health_event(
            Some(id),
            "proxy_added",
            AlertSeverity::Info,
            json!({
                "provider": ProxyProviderKind::Host, "provider_ref": proxy.provider_ref,
                "country": proxy.country,
            }),
        );
        match self.store.proxies().insert_noting(&proxy, &[added]).await {
            Ok(()) => {}
            // Another process made it a moment ago.
            Err(error) if error.is_conflict() => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        tracing::info!(
            proxy = %id,
            provider_ref = proxy.provider_ref,
            country = proxy.country.map(|country| country.to_string()),
            "a row for the host's own connection was added"
        );
        self.publish(PoolEvent::Added { proxy: id });
        Ok(())
    }

    /// Records on a host row what the host's own geo lookup saw: its
    /// address and its place. A row with a declared country counts a
    /// lookup that disagrees as a probe counts one (`geo_mismatch` after
    /// two); one without takes the place it is told. Returns the row as
    /// it is afterwards, or `None` when it does not exist or is no host
    /// row.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn record_host_place(
        &self,
        id: ProxyId,
        ip: IpAddr,
        lookup: GeoLookup,
        at: DateTime<Utc>,
    ) -> Result<Option<Proxy>, PoolError> {
        let is_host = self
            .store
            .proxies()
            .get(id)
            .await?
            .is_some_and(|row| row.provider == ProxyProviderKind::Host);
        if !is_host {
            return Ok(None);
        }
        let report = ProbeReport {
            at,
            exit: Ok(ExitObservation {
                ip,
                latency_ms: 0,
                connect_ms: None,
                tls_ms: None,
            }),
            // The host's own address is no leak: it is the exit.
            leak: None,
            geo: Some(GeoRecord {
                country: lookup.country,
                city: lookup.city,
                region: lookup.region,
                asn: lookup.asn,
                org: lookup.org,
                timezone: lookup.timezone,
                loc: lookup.loc,
                ip,
                checked_at: at,
            }),
        };
        self.record_probe(id, &report).await
    }
}

#[cfg(test)]
mod tests {
    use super::{host_ref, position};

    #[test]
    fn a_host_row_is_named_by_its_place() {
        assert_eq!(host_ref(1), "host-1");
        assert_eq!(position(&host_ref(4)), Some(4));
        assert_eq!(position("host-"), None);
        assert_eq!(position("webshare-1"), None);
    }
}
