//! Shared helpers for the store's integration tests: a throwaway Postgres
//! per test and builders for domain values.

// Each test binary uses a different subset of these helpers.
#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::missing_panics_doc
)]

use std::collections::BTreeSet;

use chrono::{DateTime, SubsecRound, Utc};
use kage_core::domain::{
    ApiKey, ClaimedOs, Engine, Geolocation, KeyLimits, Level, Persona, PersonaState, Proxy,
    ProxyProviderKind, ProxyScheme, ProxyState, RenderMode, RequestRecord, Scope, Screen,
};
use kage_core::ids::{ApiKeyId, PersonaId, ProxyId, RequestId};
use kage_core::secret::SealedSecret;
use kage_store::Store;
use sqlx::postgres::PgPoolOptions;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;

/// The Postgres release the compose file pins (docs/versions.md).
const POSTGRES_TAG: &str = "17.11-trixie";

/// A Postgres server that lives as long as this value. Each test gets its
/// own, so tests never see each other's rows and can run in parallel.
pub struct TestDb {
    pub store: Store,
    pub url: String,
    _container: ContainerAsync<Postgres>,
}

impl TestDb {
    /// Starts a server with an empty database: no migration applied.
    pub async fn empty() -> Self {
        let container = Postgres::default()
            .with_tag(POSTGRES_TAG)
            .start()
            .await
            .expect("cannot start the Postgres test container; is Docker running?");
        let host = container.get_host().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(&url)
            .await
            .expect("cannot connect to the Postgres test container");
        Self {
            store: Store::from_pool(pool),
            url,
            _container: container,
        }
    }

    /// Starts a server and applies every migration.
    pub async fn migrated() -> Self {
        let db = Self::empty().await;
        db.store.migrate().await.unwrap();
        db
    }
}

/// The current time at microsecond precision, which is what `timestamptz`
/// stores: values read back compare equal to values written.
pub fn now() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(6)
}

pub fn proxy(provider_ref: &str) -> Proxy {
    Proxy {
        id: ProxyId::new(),
        provider: ProxyProviderKind::Webshare,
        provider_ref: provider_ref.to_owned(),
        host: "198.51.100.10".to_owned(),
        port: 8080,
        username: SealedSecret::new(vec![0xfe, 0xed]),
        password: SealedSecret::new(vec![0xde, 0xad, 0xbe, 0xef]),
        scheme: ProxyScheme::Http,
        country: Some("DE".parse().unwrap()),
        locale: None,
        timezone: None,
        city: Some("Berlin".to_owned()),
        asn: Some("AS64500".to_owned()),
        state: ProxyState::New,
        consecutive_failures: 0,
        backoff_count: 0,
        backoff_until: None,
        failing_since: None,
        replacement: None,
        last_health: None,
        created_at: now(),
        retired_at: None,
    }
}

pub fn persona(proxy_id: ProxyId) -> Persona {
    let id = PersonaId::new();
    Persona {
        id,
        proxy_id,
        engine: Engine::Chromium,
        claimed_os: ClaimedOs::Linux,
        browser_major: 149,
        seed: kage_core::persona::seed_for(id),
        fingerprint: serde_json::json!({ "schema": 1, "native": { "hardware_concurrency": 8 } }),
        user_agent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                     Chrome/149.0.0.0 Safari/537.36"
            .to_owned(),
        hardware_concurrency: 8,
        wreq_profile: Some("chrome_149".to_owned()),
        country: Some("DE".parse().unwrap()),
        locale: "de-DE".to_owned(),
        languages: vec![
            "de-DE".to_owned(),
            "de".to_owned(),
            "en-US".to_owned(),
            "en".to_owned(),
        ],
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
        tables_version: kage_core::persona::TABLES_VERSION.to_owned(),
        profile_dir: kage_core::persona::profile_dir(Engine::Chromium, id),
        state: PersonaState::Active,
        cooling_reason: None,
        warmup_visits: 0,
        warmed_at: None,
        created_at: now(),
        retired_at: None,
        retire_reason: None,
        profile_archived_at: None,
        profile_archive: None,
        profile_deleted_at: None,
    }
}

pub fn api_key(name: &str, scopes: &[Scope]) -> ApiKey {
    ApiKey {
        id: ApiKeyId::new(),
        name: name.to_owned(),
        secret_hash: [7; 32],
        scopes: scopes.iter().copied().collect::<BTreeSet<_>>(),
        limits: KeyLimits {
            requests_per_minute: 60,
            max_concurrent: 4,
            daily_page_budget: Some(5000),
        },
        disabled_at: None,
        created_at: now(),
        last_used_at: None,
    }
}

pub fn request(api_key_id: Option<ApiKeyId>) -> RequestRecord {
    RequestRecord {
        id: RequestId::new(),
        api_key_id,
        job_id: None,
        session_id: None,
        url: "https://example.org/listing?page=2".to_owned(),
        method: "GET".to_owned(),
        domain: "example.org".to_owned(),
        render: RenderMode::Browser,
        level: Some(Level::Human),
        spec: serde_json::json!({ "url": "https://example.org/listing?page=2", "level": 3 }),
        created_at: now(),
        finished_at: None,
        outcome: None,
    }
}

/// Inserts a proxy and its persona, the parents most other rows need.
pub async fn seed_persona(store: &Store) -> Persona {
    let proxy = proxy(&format!("ref-{}", ProxyId::new()));
    store.proxies().insert(&proxy).await.unwrap();
    let persona = persona(proxy.id);
    store.personas().insert(&persona).await.unwrap();
    persona
}

/// Inserts an API key.
pub async fn seed_key(store: &Store) -> ApiKey {
    let key = api_key("test-key", &[Scope::Fetch, Scope::Jobs]);
    store.api_keys().insert(&key).await.unwrap();
    key
}
