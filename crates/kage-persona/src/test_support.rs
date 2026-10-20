//! Fixtures for this crate's unit tests.

use chrono::Utc;
use kage_core::domain::{
    ClaimedOs, Engine, Geolocation, Persona, PersonaState, Proxy, ProxyEndpoint, ProxyProviderKind,
    ProxyScheme, ProxyState, Screen,
};
use kage_core::ids::{PersonaId, ProxyId};
use kage_core::persona;
use kage_core::secret::{SealedSecret, Secret};

use crate::identity::profile_for;

/// A Berlin persona of `engine` at `major`: chromium on Linux, firefox on
/// Windows, with the wreq profile its major resolves to.
pub(crate) fn persona(engine: Engine, major: u32) -> Persona {
    let id = PersonaId::new();
    let (claimed_os, user_agent, languages) = match engine {
        Engine::Chromium => (
            ClaimedOs::Linux,
            format!(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/{major}.0.0.0 Safari/537.36"
            ),
            vec!["de-DE", "de", "en-US", "en"],
        ),
        Engine::Firefox => (
            ClaimedOs::Windows,
            format!(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:{major}.0) Gecko/20100101 \
                 Firefox/{major}.0"
            ),
            vec!["de", "en-US", "en"],
        ),
    };
    Persona {
        id,
        proxy_id: ProxyId::new(),
        engine,
        claimed_os,
        browser_major: major,
        seed: persona::seed_for(id),
        fingerprint: serde_json::json!({ "schema": 1, "engine": engine.as_str() }),
        user_agent,
        hardware_concurrency: 8,
        wreq_profile: profile_for(engine, major, claimed_os),
        country: "DE".parse().ok(),
        locale: "de-DE".to_owned(),
        languages: languages.into_iter().map(str::to_owned).collect(),
        timezone: "Europe/Berlin".to_owned(),
        geolocation: Some(Geolocation {
            latitude: 52.52,
            longitude: 13.405,
            accuracy_m: 35.0,
        }),
        screen: Screen {
            width: 1920,
            height: 1080,
            dpr: 1.0,
            color_depth: 24,
        },
        tables_version: persona::TABLES_VERSION.to_owned(),
        profile_dir: persona::profile_dir(engine, id),
        state: PersonaState::Active,
        cooling_reason: None,
        warmup_visits: 0,
        warmed_at: None,
        created_at: Utc::now(),
        retired_at: None,
        retire_reason: None,
        profile_archived_at: None,
        profile_archive: None,
        profile_deleted_at: None,
    }
}

/// The opened address of a proxy on loopback.
pub(crate) fn endpoint(proxy: ProxyId) -> ProxyEndpoint {
    ProxyEndpoint {
        id: proxy,
        host: "127.0.0.1".to_owned(),
        port: 8080,
        username: Secret::new("user"),
        password: Secret::new("pass"),
        scheme: ProxyScheme::Http,
    }
}

/// A healthy proxy the provider places in `country`, before any lookup.
pub(crate) fn proxy(country: &str) -> Proxy {
    Proxy {
        id: ProxyId::new(),
        // A provider whose country the observed record overrides; a
        // test of a declaration says so (ADR 0013).
        provider: ProxyProviderKind::Webshare,
        provider_ref: "fixture".to_owned(),
        host: "127.0.0.1".to_owned(),
        port: 8080,
        username: SealedSecret::new(Vec::new()),
        password: SealedSecret::new(Vec::new()),
        scheme: ProxyScheme::Http,
        country: country.parse().ok(),
        locale: None,
        timezone: None,
        city: None,
        asn: None,
        state: ProxyState::Healthy,
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
