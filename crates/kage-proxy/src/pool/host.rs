//! The rows that stand for the host's own connection (spec §25, ADR 0012).
//!
//! Under `egress.mode: direct` every host persona has a row of its own,
//! provider `host`: the persona manager generates a persona for it like
//! for any usable proxy, and the forwarder relays that persona's traffic
//! with no upstream. A host row is never synced with a provider, never
//! probed through, never part of the leak check, the outage share or the
//! capacity, and never replaced. Its place is what the host's own geo
//! lookup saw ([`Pool::record_host_place`]), or what the operator declared
//! (`egress.direct.country`, `locale`, `timezone`), which is written on the
//! row like a declared list's entry and which the lookup then only
//! verifies (ADR 0013: one rule for every declaration).

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use kage_core::alert::AlertSeverity;
use kage_core::domain::{
    Declaration, GeoRecord, Proxy, ProxyProviderKind, ProxyScheme, ProxyState,
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
    /// live and usable, each with `declared` as its declared place (the
    /// missing ones made with it, the others given it when it changed,
    /// which [`PoolEvent::DeclarationChanged`] announces), and those beyond
    /// `count` retired (their personas with them). Returns the live host
    /// rows, in order. `count` 0 retires every one: under
    /// `egress.mode: proxies` none may exist.
    ///
    /// # Errors
    ///
    /// Fails when the store fails or a credential cannot be sealed.
    pub async fn keep_host_rows(
        &self,
        count: u32,
        declared: Option<&Declaration>,
    ) -> Result<Vec<Proxy>, PoolError> {
        let now = self.clock.now();
        let mut report = ReconcileReport::default();
        for row in self.host_rows().await? {
            if position(&row.provider_ref).is_none_or(|place| place > count) {
                self.retire(&row, now, "egress_mode", &mut report).await?;
            }
        }
        let live = self.host_rows().await?;
        for row in &live {
            self.declare_host_row(row, declared).await?;
        }
        let known: Vec<&str> = live.iter().map(|row| row.provider_ref.as_str()).collect();
        for place in 1..=count {
            let provider_ref = host_ref(place);
            if known.contains(&provider_ref.as_str()) {
                continue;
            }
            self.add_host_row(provider_ref, declared, now).await?;
        }
        self.host_rows().await
    }

    /// Writes the declaration in force on a host row whose own differs,
    /// and announces it: its persona is made again when the identity
    /// changed, and follows when only the zone did (ADR 0013).
    async fn declare_host_row(
        &self,
        row: &Proxy,
        declared: Option<&Declaration>,
    ) -> Result<(), PoolError> {
        let country = declared.map(|declared| declared.country);
        let locale = declared.and_then(|declared| declared.locale.clone());
        let timezone = declared.and_then(|declared| declared.timezone.clone());
        if row.country == country && row.locale == locale && row.timezone == timezone {
            return Ok(());
        }
        let event = self.health_event(
            Some(row.id),
            "proxy_declaration_changed",
            AlertSeverity::Info,
            json!({
                "from": { "country": row.country, "locale": row.locale, "timezone": row.timezone },
                "to": { "country": country, "locale": locale, "timezone": timezone },
            }),
        );
        let changed = self
            .store
            .proxies()
            .modify_noting(row.id, |stored| {
                stored.country = country;
                stored.locale.clone_from(&locale);
                stored.timezone.clone_from(&timezone);
                ((), vec![event])
            })
            .await?;
        if changed.is_some() {
            tracing::info!(proxy = %row.id, "the host's declared place changed");
            self.publish(PoolEvent::DeclarationChanged { proxy: row.id });
        }
        Ok(())
    }

    async fn add_host_row(
        &self,
        provider_ref: String,
        declared: Option<&Declaration>,
        now: DateTime<Utc>,
    ) -> Result<(), PoolError> {
        let country = declared.map(|declared| declared.country);
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
            locale: declared.and_then(|declared| declared.locale.clone()),
            timezone: declared.and_then(|declared| declared.timezone.clone()),
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
