//! Shared helpers for the integration tests of the persona manager:
//!
//! - [`TestDb`]: a throwaway Postgres with the schema applied.
//! - [`ListProvider`]: a proxy provider whose list a test edits.
//! - [`MockSidecar`]: a sidecar that generates what it is asked for, opens
//!   a persona's context once and hands the same one back until it is
//!   closed, and answers navigations, state export and import and
//!   self-tests with what a test set, recording every call.
//! - [`Harness`]: a pool, two mock sidecars and a manager over them, on a
//!   clock the test moves.

// Each test binary uses a different subset of these helpers.
#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::too_many_lines
)]

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use kage_browser::contract::EXAMPLES_JSON;
use kage_core::alert::MemoryAlertSink;
use kage_core::config::Config;
use kage_core::domain::{
    ApiKey, Engine, GeoRecord, HealthEvent, HealthFailure, KeyLimits, Persona, Proxy,
    ProxyProviderKind, ProxyState, Scope,
};
use kage_core::ids::{ApiKeyId, ProxyId};
use kage_core::secret::Secret;
use kage_core::seed::Seed;
use kage_http::{ClientFactory, HttpSettings, StoreCookies};
use kage_persona::{PersonaManager, PersonaParts, Sidecars};
use kage_proxy::machine::{ExitObservation, ProbeReport};
use kage_proxy::provider::{Capabilities, ProviderError, ProviderProxy, ProxyProvider};
use kage_proxy::{CredentialCipher, ManualClock, Pool, PoolEvent, PoolParts, PoolRules};
use kage_store::Store;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// The Postgres release the compose file pins (docs/versions.md).
const POSTGRES_TAG: &str = "17.11-trixie";

/// A master key for tests.
pub const MASTER_KEY: &str = "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";

/// A Postgres server that lives as long as this value, with every
/// migration applied.
pub struct TestDb {
    pub store: Store,
    pub url: String,
    _container: ContainerAsync<Postgres>,
}

impl TestDb {
    pub async fn migrated() -> Self {
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
        let store = Store::from_pool(pool);
        store.migrate().await.unwrap();
        Self {
            store,
            url,
            _container: container,
        }
    }
}

// --- Provider ----------------------------------------------------------------

/// A provider whose list a test edits. By default it stands for a
/// provider whose country the observed geo record overrides (as
/// Webshare's); [`ListProvider::declaring`] stands for the operator's
/// declared list, whose country, locale and zone the persona follows
/// (ADR 0013).
#[derive(Debug)]
pub struct ListProvider {
    kind: ProxyProviderKind,
    proxies: Mutex<Vec<ProviderProxy>>,
}

impl Default for ListProvider {
    fn default() -> Self {
        Self {
            kind: ProxyProviderKind::Webshare,
            proxies: Mutex::new(Vec::new()),
        }
    }
}

impl ListProvider {
    /// A declared list.
    pub fn declaring() -> Self {
        Self {
            kind: ProxyProviderKind::Static,
            proxies: Mutex::new(Vec::new()),
        }
    }

    /// Adds an entry that declares `country` (none: a plain exit), and
    /// optionally a locale and a zone, and returns its id.
    pub fn add_declared(
        &self,
        country: Option<&str>,
        locale: Option<&str>,
        timezone: Option<&str>,
    ) -> String {
        let provider_ref = self.add(country.unwrap_or(""));
        self.change(&provider_ref, |entry| {
            entry.locale = locale.map(str::to_owned);
            entry.timezone = timezone.map(str::to_owned);
        });
        provider_ref
    }

    /// Changes an entry of the list, as an operator edits it.
    pub fn change(&self, provider_ref: &str, change: impl FnOnce(&mut ProviderProxy)) {
        let mut proxies = self.proxies.lock().unwrap();
        let entry = proxies
            .iter_mut()
            .find(|proxy| proxy.provider_ref == provider_ref)
            .expect("the entry is listed");
        change(entry);
    }

    /// Adds a proxy in `country` and returns the provider's id for it.
    pub fn add(&self, country: &str) -> String {
        let mut proxies = self.proxies.lock().unwrap();
        let number = proxies.len() + 1;
        let provider_ref = format!("p-{number}-{}", uuid::Uuid::now_v7().simple());
        proxies.push(ProviderProxy {
            provider_ref: provider_ref.clone(),
            // Nothing listens there: no test sends anything to a proxy.
            host: "127.0.0.1".to_owned(),
            port: 9,
            username: Secret::new(format!("user-{number}")),
            password: Secret::new("secret-pass-91aa"),
            country: country.parse().ok(),
            locale: None,
            timezone: None,
            city: None,
            asn: None,
            valid: Some(true),
        });
        provider_ref
    }

    /// Takes a proxy off the list.
    pub fn remove(&self, provider_ref: &str) {
        self.proxies
            .lock()
            .unwrap()
            .retain(|proxy| proxy.provider_ref != provider_ref);
    }
}

#[async_trait]
impl ProxyProvider for ListProvider {
    fn kind(&self) -> ProxyProviderKind {
        self.kind
    }

    // A declared list: it lists, and the rest is the trait's "unsupported".
    fn capabilities(&self) -> Capabilities {
        Capabilities::LIST_ONLY
    }

    async fn list(&self) -> Result<Vec<ProviderProxy>, ProviderError> {
        Ok(self.proxies.lock().unwrap().clone())
    }
}

// --- Sidecar -----------------------------------------------------------------

/// The contract's first example of a schema.
pub fn example(schema: &str) -> Value {
    let examples: Value = serde_json::from_str(EXAMPLES_JSON).unwrap();
    examples[schema][0].clone()
}

/// What a mock sidecar was asked, and what it answers.
#[derive(Debug)]
pub struct SidecarState {
    /// The browser major the engine bundles.
    pub major: u64,
    /// Answer `generate` with this status and body instead.
    pub refuse_generate: Option<(u16, Value)>,
    /// Report another seed than was asked for.
    pub wrong_seed: bool,
    /// The status of the page a navigation loads.
    pub page_status: i64,
    /// The cookies an export answers with.
    pub export_cookies: Value,
    /// The answer of a self-test.
    pub selftest: Value,
    /// Contexts the container holds at once.
    pub capacity: usize,
    /// The open contexts: by persona, the context and the request that
    /// opened it.
    pub contexts: BTreeMap<String, (String, Value)>,
    /// Requests, by operation, in order.
    pub generated: Vec<Value>,
    pub opened: Vec<Value>,
    pub closed: Vec<String>,
    pub navigated: Vec<Value>,
    pub exported: Vec<Value>,
    pub imported: Vec<Value>,
    pub selftested: Vec<Value>,
    pub upgraded: Vec<Value>,
}

/// A self-test answer: complete, with one finding.
pub fn selftest_answer(passed: bool) -> Value {
    let mut answer = example("SelftestResponse");
    answer["complete"] = json!(true);
    answer["passed"] = json!(passed);
    answer["score"] = json!(if passed { 100.0 } else { 40.0 });
    answer["findings"] = json!([{
        "page": "probe",
        "check": "navigator.webdriver",
        "status": if passed { "pass" } else { "fail" },
        "hard": true,
        "weight": 1,
        "observed": if passed { "false" } else { "true" },
        "expected": "false",
    }]);
    answer
}

struct SidecarResponder {
    engine: Engine,
    state: Arc<Mutex<SidecarState>>,
}

/// The browser major a document was made or moved for, as the mock writes
/// it; `None` for a document that names none.
fn major_of(document: &Value) -> Option<u64> {
    document["browser_version"]
        .as_str()?
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn user_agent(engine: Engine, os: &str, major: u64) -> String {
    match (engine, os) {
        (Engine::Chromium, _) => format!(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/{major}.0.0.0 Safari/537.36"
        ),
        (Engine::Firefox, "macos") => format!(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:{major}.0) Gecko/20100101 \
             Firefox/{major}.0"
        ),
        (Engine::Firefox, _) => format!(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:{major}.0) Gecko/20100101 \
             Firefox/{major}.0"
        ),
    }
}

impl SidecarResponder {
    fn generate(&self, request: &Value) -> ResponseTemplate {
        let mut state = self.state.lock().unwrap();
        state.generated.push(request.clone());
        if let Some((status, body)) = &state.refuse_generate {
            return ResponseTemplate::new(*status).set_body_json(body);
        }
        let seed = request["seed"].as_str().unwrap_or_default().to_owned();
        let os = request["claimed_os"].as_str().unwrap_or("linux").to_owned();
        // The generator's own choice of processors, from the seed.
        let cores =
            [4, 6, 8, 12, 16][usize::from(seed.as_bytes().first().copied().unwrap_or(0)) % 5];
        let screen = json!({
            "width": request["screen"]["min_width"].as_u64().unwrap_or(1920),
            "height": request["screen"]["min_height"].as_u64().unwrap_or(1080),
            "dpr": 1,
            "color_depth": 24,
        });
        let mut answer = example("GeneratePersonaResponse");
        answer["fingerprint"] = json!({
            "schema": 1,
            "seed": seed,
            "claimed_os": os,
            "browser_version": format!("{}.0.1", state.major),
            "native": { "screen": screen, "hardware_concurrency": cores },
        });
        let summary = &mut answer["summary"];
        summary["engine"] = json!(self.engine.as_str());
        summary["claimed_os"] = json!(os);
        summary["browser_major"] = json!(state.major);
        summary["browser_version"] = json!(format!("{}.0.1", state.major));
        summary["user_agent"] = json!(user_agent(self.engine, &os, state.major));
        summary["screen"] = screen;
        summary["hardware_concurrency"] = json!(cores);
        summary["languages"] = request["languages"].clone();
        summary["seed"] = if state.wrong_seed {
            json!("00".repeat(32))
        } else {
            json!(seed)
        };
        ResponseTemplate::new(200).set_body_json(answer)
    }

    /// Moves a document to the major the mock bundles, as an engine does:
    /// the version and the `User-Agent`, nothing of the device.
    fn upgrade(&self, request: &Value) -> ResponseTemplate {
        let mut state = self.state.lock().unwrap();
        state.upgraded.push(request.clone());
        let mut document = request["fingerprint"].clone();
        let from = major_of(&document).unwrap_or(state.major);
        if from > state.major {
            return ResponseTemplate::new(409).set_body_json(json!({
                "code": "browser_major_mismatch",
                "message": "the document is of a newer browser than this engine runs",
            }));
        }
        document["browser_version"] = json!(format!("{}.0.1", state.major));
        let os = document["claimed_os"]
            .as_str()
            .unwrap_or("linux")
            .to_owned();
        let mut answer = example("UpgradePersonaResponse");
        let summary = &mut answer["summary"];
        summary["engine"] = json!(self.engine.as_str());
        summary["claimed_os"] = json!(os);
        summary["browser_major"] = json!(state.major);
        summary["browser_version"] = json!(format!("{}.0.1", state.major));
        summary["user_agent"] = json!(user_agent(self.engine, &os, state.major));
        summary["screen"] = document["native"]["screen"].clone();
        summary["hardware_concurrency"] = document["native"]["hardware_concurrency"].clone();
        summary["seed"] = document["seed"].clone();
        answer["upgraded"] = json!(from != state.major);
        answer["fingerprint"] = document;
        ResponseTemplate::new(200).set_body_json(answer)
    }
}

impl Respond for SidecarResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path().to_owned();
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        let method = request.method.as_str();
        if method == "POST" && path == "/v1/personas/generate" {
            return self.generate(&body);
        }
        if method == "POST" && path == "/v1/personas/upgrade" {
            return self.upgrade(&body);
        }
        let mut state = self.state.lock().unwrap();
        if method == "POST" && path == "/v1/contexts" {
            let persona = body["persona_id"].as_str().unwrap_or_default().to_owned();
            let mut answer = example("OpenContextResponse");
            answer["persona_id"] = json!(persona);
            state.opened.push(body.clone());
            // A document of another major than the engine runs is refused.
            if major_of(&body["fingerprint"]).is_some_and(|major| major != state.major) {
                return ResponseTemplate::new(409).set_body_json(json!({
                    "code": "browser_major_mismatch",
                    "message": "the persona was made for another browser: move it first",
                }));
            }
            // Opening is idempotent: the open context is handed back as it
            // is, unless it was opened with other parameters.
            if let Some((context, opened_with)) = state.contexts.get(&persona) {
                if *opened_with != body {
                    return ResponseTemplate::new(409).set_body_json(json!({
                        "code": "context_conflict",
                        "message": "the persona's context is open with other parameters",
                    }));
                }
                answer["context_id"] = json!(context);
                answer["reused"] = json!(true);
                return ResponseTemplate::new(200).set_body_json(answer);
            }
            if state.contexts.len() >= state.capacity {
                return ResponseTemplate::new(429).set_body_json(json!({
                    "code": "context_limit",
                    "message": "this container is full",
                }));
            }
            let context = uuid::Uuid::now_v7().to_string();
            answer["context_id"] = json!(context);
            answer["reused"] = json!(false);
            state.contexts.insert(persona, (context, body));
            return ResponseTemplate::new(200).set_body_json(answer);
        }
        let Some(rest) = path.strip_prefix("/v1/contexts/") else {
            return ResponseTemplate::new(404);
        };
        let (context, operation) = rest.split_once('/').unwrap_or((rest, ""));
        match (method, operation) {
            ("DELETE", "") => {
                state.contexts.retain(|_, (open, _)| open != context);
                state.closed.push(context.to_owned());
                ResponseTemplate::new(204)
            }
            ("POST", "navigate") => {
                let mut page = example("PageResult");
                page["url"] = body["url"].clone();
                page["final_url"] = body["url"].clone();
                page["redirects"] = json!([]);
                page["status"] = json!(state.page_status);
                page["error"] = Value::Null;
                state.navigated.push(body);
                ResponseTemplate::new(200).set_body_json(page)
            }
            ("POST", "state/export") => {
                let mut answer = example("ExportStateResponse");
                answer["origin"] = body["origin"].clone();
                answer["cookies"] = state.export_cookies.clone();
                state.exported.push(body);
                ResponseTemplate::new(200).set_body_json(answer)
            }
            ("POST", "state/import") => {
                let imported = body["cookies"].as_array().map_or(0, Vec::len);
                state.imported.push(body);
                ResponseTemplate::new(200).set_body_json(json!({
                    "imported": imported,
                    "rejected": [],
                }))
            }
            ("POST", "selftest") => {
                state.selftested.push(body);
                ResponseTemplate::new(200).set_body_json(state.selftest.clone())
            }
            _ => ResponseTemplate::new(404),
        }
    }
}

/// A mock of one engine's sidecar.
pub struct MockSidecar {
    server: MockServer,
    state: Arc<Mutex<SidecarState>>,
}

impl MockSidecar {
    pub async fn start(engine: Engine, major: u64) -> Self {
        let state = Arc::new(Mutex::new(SidecarState {
            major,
            refuse_generate: None,
            wrong_seed: false,
            page_status: 200,
            export_cookies: json!([]),
            selftest: selftest_answer(true),
            capacity: 64,
            contexts: BTreeMap::new(),
            generated: Vec::new(),
            opened: Vec::new(),
            closed: Vec::new(),
            navigated: Vec::new(),
            exported: Vec::new(),
            imported: Vec::new(),
            selftested: Vec::new(),
            upgraded: Vec::new(),
        }));
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(SidecarResponder {
                engine,
                state: Arc::clone(&state),
            })
            .mount(&server)
            .await;
        Self { server, state }
    }

    pub fn url(&self) -> String {
        self.server.uri()
    }

    /// Reads or changes what the mock holds.
    pub fn with<T>(&self, change: impl FnOnce(&mut SidecarState) -> T) -> T {
        change(&mut self.state.lock().unwrap())
    }
}

// --- Harness -----------------------------------------------------------------

/// When the tests' clock starts: a Wednesday noon in UTC, inside the
/// active hours of every European persona.
pub fn start() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 0).unwrap()
}

/// What a geo lookup of a test proxy says.
pub fn geo(country: &str, city: &str, timezone: &str, loc: &str, at: DateTime<Utc>) -> GeoRecord {
    GeoRecord {
        country: country.parse().unwrap(),
        city: Some(city.to_owned()),
        region: None,
        asn: Some("AS64500".to_owned()),
        org: Some("Example Broadband".to_owned()),
        timezone: Some(timezone.to_owned()),
        loc: Some(loc.to_owned()),
        ip: IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10)),
        checked_at: at,
    }
}

/// The capital of a country the tests place proxies in.
pub fn capital(country: &str, at: DateTime<Utc>) -> GeoRecord {
    let (city, timezone, loc) = match country {
        "AT" => ("Vienna", "Europe/Vienna", "48.2082,16.3738"),
        "DE" => ("Berlin", "Europe/Berlin", "52.5200,13.4050"),
        "FR" => ("Paris", "Europe/Paris", "48.8566,2.3522"),
        "GB" => ("London", "Europe/London", "51.5074,-0.1278"),
        "IT" => ("Rome", "Europe/Rome", "41.9028,12.4964"),
        other => panic!("no capital for {other}"),
    };
    geo(country, city, timezone, loc, at)
}

/// The configuration of a test: no secrets but the test keys, warm-up off.
pub fn config() -> Config {
    let mut config = Config::load(
        None,
        [
            ("KAGE__DATABASE__URL", "postgres://kage@localhost/kage"),
            ("KAGE__SIDECAR__TOKEN", "token"),
            ("KAGE__SECRETS__MASTER_KEY", MASTER_KEY),
        ]
        .map(|(key, value)| (key.to_owned(), value.to_owned())),
    )
    .expect("the configuration loads");
    config.personas.warmup.enabled = false;
    config
}

/// A pool, two sidecars and a manager, on a clock the test moves.
pub struct Harness {
    pub db: TestDb,
    pub provider: Arc<ListProvider>,
    pub pool: Arc<Pool>,
    pub clock: Arc<ManualClock>,
    pub alerts: Arc<MemoryAlertSink>,
    pub chromium: MockSidecar,
    pub firefox: MockSidecar,
    pub http: Arc<ClientFactory>,
    pub manager: Arc<PersonaManager>,
    pub config: Config,
    /// The personas volume.
    pub volume: tempfile::TempDir,
}

impl Harness {
    /// A harness with the default test configuration and both engines at
    /// the majors of this release (Chrome 155, Firefox 156).
    pub async fn start() -> Self {
        Self::with(155, 156, |_| {}).await
    }

    /// A harness at the majors of this release, with `change` applied to
    /// the configuration.
    pub async fn start_with(change: impl FnOnce(&mut Config)) -> Self {
        Self::with(155, 156, change).await
    }

    /// A harness whose engines bundle the given majors, with `change`
    /// applied to the configuration.
    pub async fn with(chrome: u32, firefox: u32, change: impl FnOnce(&mut Config)) -> Self {
        Self::build(chrome, firefox, ListProvider::default(), change).await
    }

    /// A harness whose pool is the operator's declared list (ADR 0013).
    pub async fn declared(change: impl FnOnce(&mut Config)) -> Self {
        Self::build(155, 156, ListProvider::declaring(), change).await
    }

    async fn build(
        chrome: u32,
        firefox: u32,
        provider: ListProvider,
        change: impl FnOnce(&mut Config),
    ) -> Self {
        let db = TestDb::migrated().await;
        let clock = Arc::new(ManualClock::new(start()));
        let alerts = Arc::new(MemoryAlertSink::new());
        let provider = Arc::new(provider);
        let pool = Arc::new(Pool::new(PoolParts {
            store: db.store.clone(),
            provider: Some(provider.clone()),
            cipher: CredentialCipher::new(&[0x0f; 32]),
            rules: PoolRules::default(),
            missing_syncs: 1,
            alerts: alerts.clone(),
            clock: clock.clone(),
            seed: Seed::from_u64(7),
        }));
        let chromium = MockSidecar::start(Engine::Chromium, u64::from(chrome)).await;
        let firefox_sidecar = MockSidecar::start(Engine::Firefox, u64::from(firefox)).await;
        let volume = tempfile::tempdir().unwrap();

        let mut config = config();
        config.sidecar.chromium_url = chromium.url();
        config.sidecar.firefox_url = firefox_sidecar.url();
        config.engines.chromium.browser_major = Some(chrome);
        config.engines.firefox.browser_major = Some(firefox);
        config.personas.dir = volume.path().to_path_buf();
        change(&mut config);

        let http = Arc::new(ClientFactory::new(
            HttpSettings::from_config(&config.http),
            Arc::new(StoreCookies::new(db.store.clone())),
        ));
        let manager = Arc::new(
            PersonaManager::new(PersonaParts {
                store: db.store.clone(),
                pool: pool.clone(),
                sidecars: Sidecars::from_config(&config).unwrap(),
                http: http.clone(),
                alerts: alerts.clone(),
                config: config.clone(),
                echo_trust_anchor: None,
            })
            .unwrap(),
        );
        Self {
            db,
            provider,
            pool,
            clock,
            alerts,
            chromium,
            firefox: firefox_sidecar,
            http,
            manager,
            config,
            volume,
        }
    }

    pub fn store(&self) -> &Store {
        &self.db.store
    }

    pub fn sidecar(&self, engine: Engine) -> &MockSidecar {
        match engine {
            Engine::Chromium => &self.chromium,
            Engine::Firefox => &self.firefox,
        }
    }

    /// Lists a proxy in `country` and syncs the pool: the proxy is `new`.
    pub async fn add_proxy(&self, country: &str) -> Proxy {
        let provider_ref = self.provider.add(country);
        self.pool.reconcile().await.unwrap();
        self.pool
            .live()
            .await
            .unwrap()
            .into_iter()
            .find(|proxy| proxy.provider_ref == provider_ref)
            .expect("the pool adopted the proxy")
    }

    /// A probe of a proxy that answered, with this geo record, from an exit
    /// address of the proxy's own: two proxies that leave by one address
    /// carry no persona (decision D6).
    pub async fn probe_ok(&self, proxy: ProxyId, geo: Option<GeoRecord>) -> Proxy {
        let at = kage_proxy::Clock::now(&*self.clock);
        let own = IpAddr::V6(std::net::Ipv6Addr::from(proxy.as_uuid().as_u128()));
        self.pool
            .record_probe(
                proxy,
                &ProbeReport {
                    at,
                    exit: Ok(ExitObservation {
                        ip: own,
                        latency_ms: 120,
                        connect_ms: Some(40),
                        tls_ms: Some(60),
                    }),
                    leak: Some(false),
                    geo,
                },
            )
            .await
            .unwrap()
            .expect("the proxy exists")
    }

    /// A proxy in `country` whose first probe passed and placed it in the
    /// country's capital: it is `healthy`.
    pub async fn healthy_proxy(&self, country: &str) -> Proxy {
        let proxy = self.add_proxy(country).await;
        let at = kage_proxy::Clock::now(&*self.clock);
        self.probe_ok(proxy.id, Some(capital(country, at))).await
    }

    /// A healthy proxy and the persona generated for it.
    pub async fn persona(&self, country: &str) -> (Proxy, Persona) {
        let proxy = self.healthy_proxy(country).await;
        let persona = self
            .manager
            .ensure(proxy.id)
            .await
            .unwrap()
            .expect("a persona was generated");
        (proxy, persona)
    }

    /// The stored persona.
    pub async fn reload(&self, persona: &Persona) -> Persona {
        self.store()
            .personas()
            .get(persona.id)
            .await
            .unwrap()
            .unwrap()
    }

    /// The health events about a persona, oldest first, by kind.
    pub async fn events_of(&self, persona: &Persona) -> Vec<HealthEvent> {
        let mut events: Vec<HealthEvent> = self
            .store()
            .health_events()
            .recent(None, 1000)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.subject_id == Some(persona.id.as_uuid()))
            .collect();
        events.sort_by_key(|event| event.id);
        events
    }

    /// How many events of a kind are stored about a persona.
    pub async fn count_events(&self, persona: &Persona, kind: &str) -> usize {
        self.events_of(persona)
            .await
            .iter()
            .filter(|event| event.kind == kind)
            .count()
    }

    /// Applies every pool event that is waiting to the manager, as its
    /// run loop would.
    pub async fn pump(
        &self,
        events: &mut tokio::sync::broadcast::Receiver<PoolEvent>,
    ) -> Vec<PoolEvent> {
        let mut seen = Vec::new();
        while let Ok(event) = events.try_recv() {
            self.manager.handle(&event).await.unwrap();
            seen.push(event);
        }
        seen
    }

    /// Probes of a proxy that fail until it is in backoff.
    pub async fn fail_into_backoff(&self, proxy: ProxyId) -> Proxy {
        for _ in 0..20 {
            let at = kage_proxy::Clock::now(&*self.clock);
            let after = self
                .pool
                .record_probe(
                    proxy,
                    &ProbeReport {
                        at,
                        exit: Err(HealthFailure::Unreachable),
                        leak: None,
                        geo: None,
                    },
                )
                .await
                .unwrap()
                .expect("the proxy exists");
            if after.state == ProxyState::Backoff {
                return after;
            }
            self.clock.advance(chrono::Duration::minutes(5));
        }
        panic!("the proxy never went into backoff");
    }

    /// Ends a proxy's backoff as an operator would and probes it: it is
    /// usable again, where `geo` places it.
    pub async fn recover(&self, proxy: ProxyId, geo: Option<GeoRecord>) -> Proxy {
        self.pool.end_backoff(proxy).await.unwrap();
        self.probe_ok(proxy, geo).await
    }

    /// An API key that exists, for sessions to belong to.
    pub async fn api_key(&self, name: &str) -> ApiKeyId {
        let key = ApiKey {
            id: ApiKeyId::new(),
            name: name.to_owned(),
            secret_hash: [7; 32],
            scopes: [Scope::Fetch].into_iter().collect(),
            limits: KeyLimits {
                requests_per_minute: 60,
                max_concurrent: 4,
                daily_page_budget: None,
            },
            disabled_at: None,
            created_at: Utc::now(),
            last_used_at: None,
        };
        self.store().api_keys().insert(&key).await.unwrap();
        key.id
    }

    /// A directory below the personas volume.
    pub fn in_volume(&self, relative: &str) -> PathBuf {
        self.volume.path().join(relative)
    }
}

/// Counts by key.
pub fn tally<K: Ord, I: IntoIterator<Item = K>>(items: I) -> BTreeMap<K, usize> {
    let mut counts = BTreeMap::new();
    for item in items {
        *counts.entry(item).or_insert(0) += 1;
    }
    counts
}
