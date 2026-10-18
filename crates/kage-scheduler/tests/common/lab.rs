//! Everything between a fetch order and the wire, for real, on one
//! machine:
//!
//! - [`TestDb`]: a throwaway Postgres with the schema applied.
//! - [`ListProvider`]: a provider whose proxies are [`TestProxy`]s.
//! - [`MockSidecar`]: a sidecar that answers the contract: it generates
//!   personas, opens a persona's context once and hands the same one back,
//!   answers navigations with what a test set for a URL, walks a flow
//!   step by step over the same pages, keeps the cookies a context holds
//!   per origin, so that what is imported can be exported again, and
//!   answers the health, metrics and self-test questions of the API.
//! - [`Lab`]: the pool, the persona manager with the forwarder as its
//!   egress, the scheduler over the store, the real executor and the
//!   pipeline, and a site behind the provider's proxies.
//!
//! The clock is the system's: real sockets do not wait on a paused timer.
//! The tests therefore run with the `fast` pacing profile and short
//! pauses.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kage_browser::contract::EXAMPLES_JSON;
use kage_core::alert::{AlertSink, MemoryAlertSink};
use kage_core::config::Config;
use kage_core::domain::{Engine, GeoRecord, Persona, Proxy, ProxyProviderKind};
use kage_core::duration::HumanDuration;
use kage_core::ids::ProxyId;
use kage_core::policy::PolicyStore;
use kage_core::secret::Secret;
use kage_core::seed::Seed;
use kage_http::{ClientFactory, HttpSettings, StoreCookies};
use kage_persona::{BrowserGate, Egress, PersonaManager, PersonaParts, Sidecars};
use kage_proxy::machine::{ExitObservation, ProbeReport};
use kage_proxy::provider::{
    AccountInfo, ProviderError, ProviderProxy, ProxyActivity, ProxyProvider, ReplaceSpec,
    ReplaceState, ReplaceTicket, ReplacedProxy,
};
use kage_proxy::{Clock, CredentialCipher, Pool, PoolParts, PoolRules, SystemClock};
use kage_scheduler::{
    AttemptExecutor, Backend, Forwarder, ForwarderSettings, PathExecutor, Pipeline,
    PipelineSettings, PoolRoutes, Routes, Scheduler, SchedulerGate, StoreBackend, Tokens, Verdicts,
    cooldown_rules, planner_rules,
};
use kage_store::Store;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use tempfile::TempDir;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use tokio::sync::oneshot;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::policies;
use super::proxy::{PROXY_PASS, PROXY_USER, TestProxy};

/// The Postgres release the compose file pins (docs/versions.md).
const POSTGRES_TAG: &str = "17.11-trixie";

/// A master key for tests.
pub const MASTER_KEY: &str = "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";

/// The name the site is reached by.
pub const SITE: &str = "shop.kage.test";

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

/// A provider whose list a test edits.
#[derive(Debug, Default)]
pub struct ListProvider {
    proxies: Mutex<Vec<ProviderProxy>>,
    /// Bytes the provider says each proxy carried, in all.
    traffic: Mutex<u64>,
}

impl ListProvider {
    /// Lists a proxy in `country` that is `proxy`, and returns the
    /// provider's id for it.
    pub fn add(&self, country: &str, proxy: &TestProxy) -> String {
        let mut proxies = self.proxies.lock().unwrap();
        let number = proxies.len() + 1;
        let provider_ref = format!("p-{number}-{}", uuid::Uuid::now_v7().simple());
        proxies.push(ProviderProxy {
            provider_ref: provider_ref.clone(),
            host: proxy.addr.ip().to_string(),
            port: proxy.addr.port(),
            username: Secret::new(PROXY_USER),
            password: Secret::new(PROXY_PASS),
            country: country.parse().ok(),
            city: None,
            asn: None,
            valid: Some(true),
        });
        provider_ref
    }

    /// What the provider reports as carried from now on.
    pub fn set_traffic(&self, bytes: u64) {
        *self.traffic.lock().unwrap() = bytes;
    }
}

#[async_trait]
impl ProxyProvider for ListProvider {
    fn kind(&self) -> ProxyProviderKind {
        ProxyProviderKind::Static
    }

    async fn list(&self) -> Result<Vec<ProviderProxy>, ProviderError> {
        Ok(self.proxies.lock().unwrap().clone())
    }

    async fn replace(&self, _: &Proxy, _: ReplaceSpec) -> Result<ReplaceTicket, ProviderError> {
        Err(ProviderError::Unsupported {
            provider: ProxyProviderKind::Static,
            operation: "replacement",
        })
    }

    async fn replace_status(&self, _: &ReplaceTicket) -> Result<ReplaceState, ProviderError> {
        Err(ProviderError::Unsupported {
            provider: ProxyProviderKind::Static,
            operation: "replacement",
        })
    }

    async fn replaced(&self, _: DateTime<Utc>) -> Result<Vec<ReplacedProxy>, ProviderError> {
        Ok(Vec::new())
    }

    async fn account(&self) -> Result<AccountInfo, ProviderError> {
        Ok(AccountInfo::default())
    }

    async fn stats(&self, _: DateTime<Utc>) -> Result<Vec<ProxyActivity>, ProviderError> {
        Ok(Vec::new())
    }
}

// --- Sidecar -----------------------------------------------------------------

/// The contract's first example of a schema.
pub fn example(schema: &str) -> Value {
    let examples: Value = serde_json::from_str(EXAMPLES_JSON).unwrap();
    examples[schema][0].clone()
}

/// A document long enough to be a page and not a wall.
pub fn article(title: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><title>{title}</title></head><body><h1>{title}</h1>{}</body></html>",
        "<p>The council met on Tuesday and agreed on the budget for the coming year.</p>"
            .repeat(60)
    )
}

/// What a mock sidecar was asked, and what it answers.
#[derive(Debug)]
pub struct SidecarState {
    /// The browser major the engine bundles.
    pub major: u64,
    /// Contexts the container holds at once.
    pub capacity: usize,
    /// The open contexts: by persona, the context and the request that
    /// opened it.
    pub contexts: BTreeMap<String, (String, Value)>,
    /// What the next navigations answer, before anything set per URL:
    /// each is laid over a served page.
    pub next_pages: VecDeque<Value>,
    /// What a navigation to a URL answers, laid over a served page.
    pub pages: HashMap<String, Value>,
    /// The cookies a context holds, by origin.
    pub cookies: HashMap<String, Vec<Value>>,
    /// The tab of each open context: the pages it went to, the open one
    /// last. A click on a page it went back to drops what came after it.
    pub tabs: HashMap<String, Vec<String>>,
    /// Pages whose scrolling ends elsewhere than at the depth asked for:
    /// by URL, how far down it came and why it ended there.
    pub scrolls: HashMap<String, (f64, String)>,
    /// Requests, by operation, in order.
    pub generated: Vec<Value>,
    pub opened: Vec<Value>,
    pub closed: Vec<String>,
    pub navigated: Vec<Value>,
    /// The flows that were asked for, each with its context.
    pub flows: Vec<Value>,
    /// What the next flows do other than succeed, one entry per flow:
    /// `fail: { index, error }` ends it at a step, `captures: { "<index>":
    /// page }` is laid over a step's capture, `final: page` over the page
    /// it ends on, and `interrupted: true` ends the answer without its
    /// final event.
    pub next_flows: VecDeque<Value>,
    /// How long the next flows take to answer.
    pub flow_delay: Option<Duration>,
    pub exported: Vec<Value>,
    pub imported: Vec<Value>,
    /// Self-tests that were asked for.
    pub selftested: Vec<Value>,
    /// What a self-test answers, laid over a passing report.
    pub selftest: Option<Value>,
    /// Whether the container answers its health question.
    pub healthy: bool,
}

struct SidecarResponder {
    engine: Engine,
    state: Arc<Mutex<SidecarState>>,
}

fn refusal(status: u16, code: &str, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({ "code": code, "message": message }))
}

impl SidecarResponder {
    fn generate(&self, request: &Value) -> ResponseTemplate {
        let mut state = self.state.lock().unwrap();
        state.generated.push(request.clone());
        let seed = request["seed"].as_str().unwrap_or_default().to_owned();
        let os = request["claimed_os"].as_str().unwrap_or("linux").to_owned();
        let screen = json!({
            "width": request["screen"]["min_width"].as_u64().unwrap_or(1920),
            "height": request["screen"]["min_height"].as_u64().unwrap_or(1080),
            "dpr": 1,
            "color_depth": 24,
        });
        let user_agent = match self.engine {
            Engine::Chromium => format!(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/{}.0.0.0 Safari/537.36",
                state.major
            ),
            Engine::Firefox => format!(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:{0}.0) Gecko/20100101 Firefox/{0}.0",
                state.major
            ),
        };
        let mut answer = example("GeneratePersonaResponse");
        answer["fingerprint"] = json!({
            "schema": 1,
            "seed": seed,
            "claimed_os": os,
            "native": { "screen": screen, "hardware_concurrency": 8 },
        });
        let summary = &mut answer["summary"];
        summary["engine"] = json!(self.engine.as_str());
        summary["claimed_os"] = json!(os);
        summary["browser_major"] = json!(state.major);
        summary["browser_version"] = json!(format!("{}.0.1", state.major));
        summary["user_agent"] = json!(user_agent);
        summary["screen"] = screen;
        summary["hardware_concurrency"] = json!(8);
        summary["languages"] = request["languages"].clone();
        summary["seed"] = json!(seed);
        ResponseTemplate::new(200).set_body_json(answer)
    }

    fn open(state: &mut SidecarState, body: Value) -> ResponseTemplate {
        let persona = body["persona_id"].as_str().unwrap_or_default().to_owned();
        state.opened.push(body.clone());
        let mut answer = example("OpenContextResponse");
        answer["persona_id"] = json!(persona);
        if let Some((context, opened_with)) = state.contexts.get(&persona) {
            if *opened_with != body {
                return refusal(
                    409,
                    "context_conflict",
                    "the persona's context is open with other parameters",
                );
            }
            answer["context_id"] = json!(context);
            answer["reused"] = json!(true);
            return ResponseTemplate::new(200).set_body_json(answer);
        }
        if state.contexts.len() >= state.capacity {
            return refusal(429, "context_limit", "this container is full");
        }
        let context = uuid::Uuid::now_v7().to_string();
        answer["context_id"] = json!(context);
        answer["reused"] = json!(false);
        state.contexts.insert(persona, (context, body));
        ResponseTemplate::new(200).set_body_json(answer)
    }

    /// A page of `url` as a browser brings it back, before anything a
    /// test laid over it.
    fn served(url: &str) -> Value {
        let mut page = example("PageResult");
        page["url"] = json!(url);
        page["final_url"] = json!(url);
        page["redirects"] = json!([]);
        page["status"] = json!(200);
        page["error"] = Value::Null;
        page["headers"] = json!([{ "name": "content-type", "value": "text/html; charset=utf-8" }]);
        page["title"] = json!("A page");
        page["dom_html"] = json!(article("A page"));
        page["raw_body"] = Value::Null;
        page["xhr"] = json!([]);
        page["console"] = json!([]);
        page["challenge"] = json!({
            "vendors": [], "script_hosts": [], "iframe_hosts": [],
            "interstitial": false, "marker": null, "resolved": null, "waited_ms": 0,
        });
        page["screenshot"] = Value::Null;
        page["pdf"] = Value::Null;
        page
    }

    fn lay(page: &mut Value, over: Option<Value>) {
        if let Some(Value::Object(over)) = over {
            for (key, value) in over {
                page[key.as_str()] = value;
            }
        }
    }

    /// What a browser with one tab says of a navigation's behavior (the
    /// contract since 1.3.0): how it arrived, the patience it was given
    /// and where its scrolling ended. A link on a page the persona has been
    /// reading is clicked when the tab holds that page, the open one or at
    /// most `max_backs` before it; else the address is typed.
    fn behave(state: &mut SidecarState, context: &str, asked: &Value, page: &mut Value) {
        let behavior = &asked["behavior"];
        let url = asked["url"].as_str().unwrap_or_default().to_owned();
        let tab = state.tabs.entry(context.to_owned()).or_default();
        if !behavior.is_object() {
            tab.push(url);
            return;
        }
        let entry = &behavior["entry"];
        let strategy = entry["strategy"].as_str().unwrap_or("direct");
        let from = entry["from_url"].as_str();
        let in_the_tab = strategy == "link" && entry["from_open_page"] == json!(true);
        let max_backs = usize::try_from(entry["max_backs"].as_u64().unwrap_or(0)).unwrap();
        let backs = from.filter(|_| in_the_tab).and_then(|from| {
            tab.iter()
                .rev()
                .take(max_backs + 1)
                .position(|held| held == from)
        });
        page["behavior"]["entry"] = match (strategy, backs) {
            ("link", Some(backs)) => {
                tab.truncate(tab.len() - backs);
                json!({
                    "strategy": "link", "outcome": "arrived", "from_url": from,
                    "referer": from, "backs": backs,
                })
            }
            ("link", None) if in_the_tab => json!({
                "strategy": "link", "outcome": "fell_back", "from_url": from,
                "referer": null, "backs": null,
            }),
            ("link", None) => json!({
                "strategy": "link", "outcome": "arrived", "from_url": from,
                "referer": from, "backs": null,
            }),
            _ => json!({
                "strategy": strategy, "outcome": "arrived", "from_url": null,
                "referer": null, "backs": null,
            }),
        };
        tab.push(url.clone());
        page["behavior"]["patience_ms"] = behavior["patience_ms"].clone();
        let depth = behavior["scroll_depth"].as_f64();
        page["behavior"]["scroll_reached"] = json!(depth);
        page["behavior"]["scroll_ended_by"] = json!(depth.map(|_| "depth"));
        if let (Some(_), Some((reached, why))) = (depth, state.scrolls.get(&url)) {
            page["behavior"]["scroll_reached"] = json!(reached);
            page["behavior"]["scroll_ended_by"] = json!(why);
        }
        if let Some(seed) = behavior["seed"].as_str() {
            page["behavior"]["seed"] = json!(seed);
        }
    }

    fn navigate(state: &mut SidecarState, context: &str, body: Value) -> ResponseTemplate {
        let url = body["url"].as_str().unwrap_or_default().to_owned();
        let mut page = Self::served(&url);
        // A page has the links it was asked for and no others.
        if body["capture"]["links"] != json!(true) {
            page["links"] = Value::Null;
            page["links_truncated"] = json!(false);
        }
        Self::behave(state, context, &body, &mut page);
        let over = state
            .next_pages
            .pop_front()
            .or_else(|| state.pages.get(&url).cloned());
        Self::lay(&mut page, over);
        let mut asked = body;
        asked["context_id"] = json!(context);
        state.navigated.push(asked);
        ResponseTemplate::new(200).set_body_json(page)
    }

    /// Walks a flow: a `goto` moves to its URL, a `back` to the page
    /// before, every other step stays; `extract`, `screenshot` and `pdf`
    /// capture the page the flow is on. One line per step, then the final
    /// one.
    fn flow(state: &mut SidecarState, context: &str, body: Value) -> ResponseTemplate {
        let script = state.next_flows.pop_front().unwrap_or(Value::Null);
        let fail_at = script["fail"]["index"].as_u64();
        let paced = !body["behavior"]["pace"].is_null();
        let mut url = String::from("about:blank");
        let mut history: Vec<String> = Vec::new();
        let mut lines: Vec<String> = Vec::new();
        let mut ok = true;
        let actions = body["actions"].as_array().cloned().unwrap_or_default();
        for (index, action) in actions
            .iter()
            .zip(0_u64..)
            .map(|(action, index)| (index, action))
        {
            let name = action["action"].as_str().unwrap_or_default();
            let failed = fail_at == Some(index);
            if !failed {
                match name {
                    "goto" => {
                        history.push(std::mem::take(&mut url));
                        action["url"]
                            .as_str()
                            .unwrap_or_default()
                            .clone_into(&mut url);
                    }
                    "back" => {
                        if let Some(before) = history.pop() {
                            url = before;
                        }
                    }
                    _ => {}
                }
            }
            let capture =
                (!failed && matches!(name, "extract" | "screenshot" | "pdf")).then(|| {
                    let mut page = Self::served(&url);
                    Self::lay(&mut page, state.pages.get(&url).cloned());
                    Self::lay(
                        &mut page,
                        script["captures"].get(index.to_string()).cloned(),
                    );
                    page
                });
            lines.push(
                json!({
                    "type": "step",
                    "index": index,
                    "action": name,
                    "ok": !failed,
                    "duration_ms": 12 + index,
                    "paced_ms": paced.then_some(7),
                    "error": failed.then(|| script["fail"]["error"].clone()),
                    "capture": capture,
                })
                .to_string(),
            );
            if failed {
                ok = false;
                break;
            }
        }
        if script["interrupted"].as_bool() != Some(true) {
            let mut result = Self::served(&url);
            Self::lay(&mut result, state.pages.get(&url).cloned());
            Self::lay(&mut result, script.get("final").cloned());
            lines.push(
                json!({ "type": "final", "ok": ok, "result": result, "behavior": null })
                    .to_string(),
            );
        }
        let mut asked = body;
        asked["context_id"] = json!(context);
        state.flows.push(asked);
        let mut answer = ResponseTemplate::new(200)
            .set_body_raw(lines.join("\n") + "\n", "application/x-ndjson");
        if let Some(delay) = state.flow_delay {
            answer = answer.set_delay(delay);
        }
        answer
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
        let mut state = self.state.lock().unwrap();
        if method == "GET" && path == "/v1/health" {
            if !state.healthy {
                return ResponseTemplate::new(503);
            }
            let mut health = example("HealthResponse");
            health["engine"] = json!(self.engine.as_str());
            return ResponseTemplate::new(200).set_body_json(health);
        }
        if method == "GET" && path == "/v1/metrics" {
            let engine = self.engine.as_str();
            return ResponseTemplate::new(200).set_body_string(format!(
                "# HELP sidecar_contexts_open Open persona contexts\n\
                 # TYPE sidecar_contexts_open gauge\n\
                 sidecar_contexts_open{{engine=\"{engine}\"}} {}\n\
                 # HELP sidecar_navigation_duration_seconds Time a navigation took\n\
                 # TYPE sidecar_navigation_duration_seconds histogram\n\
                 sidecar_navigation_duration_seconds_bucket{{engine=\"{engine}\",le=\"+Inf\"}} {navigated}\n\
                 sidecar_navigation_duration_seconds_sum{{engine=\"{engine}\"}} 1.5\n\
                 sidecar_navigation_duration_seconds_count{{engine=\"{engine}\"}} {navigated}\n",
                state.contexts.len(),
                navigated = state.navigated.len(),
            ));
        }
        if method == "POST" && path == "/v1/contexts" {
            return Self::open(&mut state, body);
        }
        let Some(rest) = path.strip_prefix("/v1/contexts/") else {
            return ResponseTemplate::new(404);
        };
        let (context, operation) = rest.split_once('/').unwrap_or((rest, ""));
        let known = state.contexts.values().any(|(open, _)| open == context);
        if !known {
            return refusal(404, "context_not_found", "no such context");
        }
        match (method, operation) {
            ("DELETE", "") => {
                state.contexts.retain(|_, (open, _)| open != context);
                state.tabs.remove(context);
                state.closed.push(context.to_owned());
                ResponseTemplate::new(204)
            }
            ("POST", "navigate") => Self::navigate(&mut state, context, body),
            ("POST", "flow") => Self::flow(&mut state, context, body),
            ("POST", "selftest") => {
                let mut report = example("SelftestResponse");
                report["passed"] = json!(true);
                report["complete"] = json!(true);
                report["score"] = json!(100);
                for finding in report["findings"].as_array_mut().into_iter().flatten() {
                    finding["status"] = json!("pass");
                }
                if let Some(Value::Object(over)) = state.selftest.clone() {
                    for (key, value) in over {
                        report[key.as_str()] = value;
                    }
                }
                state.selftested.push(body);
                ResponseTemplate::new(200).set_body_json(report)
            }
            ("POST", "state/export") => {
                let origin = body["origin"].as_str().unwrap_or_default().to_owned();
                let mut answer = example("ExportStateResponse");
                answer["origin"] = json!(origin);
                answer["cookies"] = json!(state.cookies.get(&origin).cloned().unwrap_or_default());
                answer["local_storage"] = json!([]);
                state.exported.push(body);
                ResponseTemplate::new(200).set_body_json(answer)
            }
            ("POST", "state/import") => {
                let origin = body["origin"].as_str().unwrap_or_default().to_owned();
                let cookies = body["cookies"].as_array().cloned().unwrap_or_default();
                let imported = cookies.len();
                state.cookies.entry(origin).or_default().extend(cookies);
                state.imported.push(body);
                ResponseTemplate::new(200).set_body_json(json!({
                    "imported": imported,
                    "rejected": [],
                }))
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
            capacity: 3,
            contexts: BTreeMap::new(),
            next_pages: VecDeque::new(),
            pages: HashMap::new(),
            cookies: HashMap::new(),
            tabs: HashMap::new(),
            scrolls: HashMap::new(),
            generated: Vec::new(),
            opened: Vec::new(),
            closed: Vec::new(),
            navigated: Vec::new(),
            flows: Vec::new(),
            next_flows: VecDeque::new(),
            flow_delay: None,
            exported: Vec::new(),
            imported: Vec::new(),
            selftested: Vec::new(),
            selftest: None,
            healthy: true,
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

// --- Lab ---------------------------------------------------------------------

/// A cookie as the contract writes it.
pub fn cookie(name: &str, value: &str) -> Value {
    json!({
        "domain": SITE,
        "host_only": true,
        "path": "/",
        "name": name,
        "value": value,
        "secure": false,
        "http_only": true,
        "same_site": "lax",
        "expires_at": "2031-01-01T00:00:00Z",
    })
}

/// The domain rules the lab's tests share: the http path first, at the
/// `fast` pace, whatever the hour of the machine's clock.
pub const LENIENT_FAST: &str =
    "defaults: { render: http, pacing: fast, active_hours: ignore }\ndomains: {}\n";

/// See the module documentation.
pub struct Lab {
    pub db: TestDb,
    pub provider: Arc<ListProvider>,
    pub pool: Arc<Pool>,
    pub alerts: Arc<MemoryAlertSink>,
    pub chromium: MockSidecar,
    pub firefox: MockSidecar,
    /// The provider's proxies, in the order their personas were made.
    pub proxies: Vec<TestProxy>,
    pub personas: Vec<Persona>,
    pub forwarder: Arc<Forwarder>,
    pub tokens: Tokens,
    pub manager: Arc<PersonaManager>,
    pub backend: Arc<StoreBackend>,
    pub scheduler: Arc<Scheduler>,
    pub verdicts: Arc<Verdicts>,
    pub gate: Arc<SchedulerGate>,
    pub pipeline: Pipeline,
    pub policies: Arc<PolicyStore>,
    /// The site every provider proxy reaches as [`SITE`], port 80.
    pub site: MockServer,
    pub config: Config,
    stops: Vec<oneshot::Sender<()>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    policy_dir: TempDir,
    _volume: TempDir,
}

fn millis(count: u64) -> HumanDuration {
    HumanDuration::new(Duration::from_millis(count))
}

impl Lab {
    /// A lab with `personas` personas of chromium at `major` (149 has an
    /// http path, 155 has none) under the domain rules `domains`.
    pub async fn start(personas: usize, major: u32, domains: &str) -> Self {
        Self::start_with(personas, major, domains, |_| {}).await
    }

    /// The same, with `change` applied to the configuration.
    pub async fn start_with(
        personas: usize,
        major: u32,
        domains: &str,
        change: impl FnOnce(&mut Config),
    ) -> Self {
        let db = TestDb::migrated().await;
        let store = db.store.clone();
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let alerts = Arc::new(MemoryAlertSink::new());
        let sink: Arc<dyn AlertSink> = alerts.clone();
        let provider = Arc::new(ListProvider::default());
        let pool = Arc::new(Pool::new(PoolParts {
            store: store.clone(),
            provider: Some(provider.clone()),
            cipher: CredentialCipher::new(&[0x0f; 32]),
            rules: PoolRules::default(),
            missing_syncs: 1,
            alerts: sink.clone(),
            clock: Arc::clone(&clock),
            seed: Seed::from_u64(7),
        }));
        let chromium = MockSidecar::start(Engine::Chromium, u64::from(major)).await;
        let firefox = MockSidecar::start(Engine::Firefox, 156).await;
        let volume = tempfile::tempdir().unwrap();
        let policy_dir = tempfile::tempdir().unwrap();
        let policies = policies(policy_dir.path(), domains, Arc::clone(&alerts));

        let mut config = super::config(&[]);
        config.personas.warmup.enabled = false;
        // Every persona is chromium on Linux: the engine a test scripts.
        config.personas.mix = vec![kage_core::persona::mix::MixEntry {
            engine: Engine::Chromium,
            os: kage_core::domain::ClaimedOs::Linux,
            weight: 1,
        }];
        config.sidecar.chromium_url = chromium.url();
        config.sidecar.firefox_url = firefox.url();
        config.engines.chromium.browser_major = Some(major);
        config.engines.firefox.browser_major = Some(156);
        config.personas.dir = volume.path().to_path_buf();
        config.scheduler.retry_pause = kage_core::config::PauseConfig {
            min: millis(50),
            median: millis(100),
            p95: millis(200),
        };
        config.scheduler.visit_start_spacing = kage_core::config::PauseConfig {
            min: millis(10),
            median: millis(20),
            p95: millis(40),
        };
        change(&mut config);

        // The forwarder, in front of whatever proxy the pool holds for a
        // persona.
        let key = config.secrets.master_key_bytes().unwrap();
        let routes: Arc<dyn Routes> = Arc::new(PoolRoutes::new(store.clone(), Arc::clone(&pool)));
        let (forwarder, listener) = Forwarder::bind(
            "127.0.0.1:0".parse().unwrap(),
            ForwarderSettings::from_config(&config),
            Tokens::new(&key),
            routes,
        )
        .await
        .unwrap();
        let mut stops = Vec::new();
        let mut tasks = Vec::new();
        let (stop, stopped) = oneshot::channel::<()>();
        stops.push(stop);
        tasks.push(tokio::spawn(Arc::clone(&forwarder).serve(
            listener,
            async move {
                let _ = stopped.await;
            },
        )));

        let http = Arc::new(ClientFactory::new(
            HttpSettings::from_config(&config.http),
            Arc::new(StoreCookies::new(store.clone())),
        ));
        let egress: Arc<dyn Egress> = Arc::new(forwarder.egress());
        let manager = Arc::new(
            PersonaManager::new(PersonaParts {
                store: store.clone(),
                pool: Arc::clone(&pool),
                sidecars: Sidecars::from_config(&config).unwrap(),
                http,
                alerts: sink.clone(),
                config: config.clone(),
                echo_trust_anchor: None,
            })
            .unwrap()
            .with_egress(egress),
        );

        let backend = Arc::new(StoreBackend::new(store.clone(), Arc::clone(&pool)));
        let stored: Arc<dyn Backend> = backend.clone();
        let scheduler = Scheduler::new(
            planner_rules(&config),
            Seed::from_u64(11),
            Arc::clone(&stored),
            Arc::clone(&clock),
        );
        let verdicts = Arc::new(Verdicts::new(
            stored,
            Arc::clone(&policies),
            cooldown_rules(&config),
            config.scheduler.breaker_max_pause.get(),
            config.scheduler.breaker_trial_timeout.get(),
            sink,
            Arc::clone(&clock),
        ));
        let gate = Arc::new(SchedulerGate::new(
            Arc::clone(&scheduler),
            Arc::clone(&verdicts),
            Some(Arc::clone(forwarder.meter())),
        ));
        let as_gate: Arc<dyn BrowserGate> = gate.clone();
        assert!(manager.set_gate(as_gate));
        let executor: Arc<dyn AttemptExecutor> = Arc::new(PathExecutor::new(
            Arc::clone(&manager),
            Arc::clone(&verdicts),
            &config,
            Some(Arc::clone(forwarder.meter())),
        ));
        let pipeline = Pipeline::new(
            Arc::clone(&scheduler),
            executor,
            Arc::clone(&verdicts),
            PipelineSettings::from_config(&config),
        );
        let (stop, stopped) = oneshot::channel::<()>();
        stops.push(stop);
        tasks.push(tokio::spawn(Arc::clone(&scheduler).run(async move {
            let _ = stopped.await;
        })));

        let mut lab = Self {
            db,
            provider,
            pool,
            alerts,
            chromium,
            firefox,
            proxies: Vec::new(),
            personas: Vec::new(),
            tokens: Tokens::new(&key),
            forwarder,
            manager,
            backend,
            scheduler,
            verdicts,
            gate,
            pipeline,
            policies,
            site: MockServer::start().await,
            config,
            stops,
            tasks,
            policy_dir,
            _volume: volume,
        };
        for _ in 0..personas {
            lab.add_persona().await;
        }
        lab
    }

    pub fn store(&self) -> &Store {
        &self.db.store
    }

    /// The directory of the policies in force.
    pub fn policy_dir(&self) -> &std::path::Path {
        self.policy_dir.path()
    }

    pub fn now(&self) -> DateTime<Utc> {
        self.pool.clock().now()
    }

    /// A probe of a proxy that answered from an address of its own.
    pub async fn probe_ok(&self, proxy: ProxyId, number: u8) -> Proxy {
        let at = self.now();
        let ip = IpAddr::V4(Ipv4Addr::new(198, 51, number, 10));
        self.pool
            .record_probe(
                proxy,
                &ProbeReport {
                    at,
                    exit: Ok(ExitObservation {
                        ip,
                        latency_ms: 120,
                        connect_ms: Some(40),
                        tls_ms: Some(60),
                    }),
                    leak: Some(false),
                    geo: Some(GeoRecord {
                        country: "DE".parse().unwrap(),
                        city: Some("Berlin".to_owned()),
                        region: None,
                        asn: Some(format!("AS{}", 64_500 + u32::from(number))),
                        org: Some("Example Broadband".to_owned()),
                        timezone: Some("Europe/Berlin".to_owned()),
                        loc: Some("52.5200,13.4050".to_owned()),
                        ip,
                        checked_at: at,
                    }),
                },
            )
            .await
            .unwrap()
            .expect("the proxy exists")
    }

    /// One more provider proxy that reaches the site, healthy, and the
    /// persona generated for it.
    pub async fn add_persona(&mut self) -> Persona {
        let proxy = TestProxy::start().await;
        proxy.route(&format!("{SITE}:80"), *self.site.address());
        let provider_ref = self.provider.add("DE", &proxy);
        self.pool.reconcile().await.unwrap();
        let listed = self
            .pool
            .live()
            .await
            .unwrap()
            .into_iter()
            .find(|proxy| proxy.provider_ref == provider_ref)
            .expect("the pool adopted the proxy");
        let number = u8::try_from(self.proxies.len()).unwrap();
        let healthy = self.probe_ok(listed.id, number).await;
        let persona = self
            .manager
            .ensure(healthy.id)
            .await
            .unwrap()
            .expect("a persona was generated");
        self.proxies.push(proxy);
        self.personas.push(persona.clone());
        persona
    }

    /// The provider proxy of a persona.
    pub fn proxy_of(&self, persona: &Persona) -> &TestProxy {
        let index = self
            .personas
            .iter()
            .position(|known| known.id == persona.id)
            .expect("a persona of this lab");
        &self.proxies[index]
    }

    /// A URL of the site.
    pub fn url(path: &str) -> url::Url {
        url::Url::parse(&format!("http://{SITE}{path}")).unwrap()
    }

    /// The requests the site received for a path, in order.
    pub async fn received(&self, path: &str) -> Vec<Request> {
        self.site
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|request| request.url.path() == path)
            .collect()
    }

    /// Stops the scheduler and the forwarder.
    pub async fn shut_down(&mut self) {
        for stop in self.stops.drain(..) {
            let _ = stop.send(());
        }
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
    }
}
