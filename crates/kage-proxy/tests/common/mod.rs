//! Shared helpers for the integration tests of the proxy pool:
//!
//! - [`TestDb`]: a throwaway Postgres with the schema applied.
//! - [`MockWebshare`]: a stateful mock of the Webshare API (wiremock) with
//!   list paging, the replacement lifecycle, the replaced list, the account
//!   endpoints, hourly stats and injectable 429s and errors.
//! - [`Echo`]: a local echo service that reports the address it was called
//!   from, and a geo record for it.
//! - [`TestProxy`]: a local HTTP proxy with Basic authentication that
//!   leaves through a loopback address of its own, so every proxy has a
//!   distinct "exit address" the echo can see.
//!
//! The `kage` binary's CLI tests include this file too.

// Each test binary uses a different subset of these helpers.
#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::assigning_clones,
    clippy::too_many_lines
)]

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use chrono::{DateTime, Utc};
use kage_store::Store;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// The Postgres release the compose file pins (docs/versions.md).
const POSTGRES_TAG: &str = "17.11-trixie";

/// A master key for tests: 32 bytes as 64 hexadecimal characters.
pub const MASTER_KEY: &str = "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";

/// The same key as bytes.
pub const MASTER_KEY_BYTES: [u8; 32] = [0x0f; 32];

/// A Postgres server that lives as long as this value, with every migration
/// applied.
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

// --- Mock Webshare -----------------------------------------------------------

/// The API token the mock accepts.
pub const WEBSHARE_TOKEN: &str = "ws-token-51aa";

/// The proxy user name and password of the mock account.
pub const PROXY_USER: &str = "wsuser";
pub const PROXY_PASS: &str = "wspass-77aa";

/// One proxy in the mock account's list.
#[derive(Debug, Clone)]
pub struct MockProxy {
    pub id: String,
    pub ip: String,
    pub port: u16,
    pub country: String,
    pub city: String,
    pub valid: bool,
    pub username: String,
    pub password: String,
}

impl MockProxy {
    fn json(&self) -> Value {
        json!({
            "id": self.id,
            "username": self.username,
            "password": self.password,
            "proxy_address": self.ip,
            "port": self.port,
            "valid": self.valid,
            "last_verification": "2026-10-08T10:00:00.000000-07:00",
            "country_code": self.country,
            "city_name": self.city,
            "created_at": "2026-09-01T10:00:00.000000-07:00",
        })
    }
}

/// A replacement the mock is working on.
#[derive(Debug, Clone)]
pub struct MockReplacement {
    pub id: u64,
    pub ip: String,
    pub country: Option<String>,
    pub state: String,
    /// Status reads left before the replacement completes or fails.
    pub polls_left: u32,
    pub request: Value,
}

/// A response the mock gives instead of its normal one.
#[derive(Debug, Clone)]
pub struct Injected {
    /// The request path must start with this.
    pub path_prefix: String,
    /// The request method must be this, if set.
    pub method: Option<String>,
    pub status: u16,
    pub body: Value,
    pub retry_after: Option<u64>,
    /// How many requests get this response.
    pub times: u32,
}

/// Everything the mock account holds. Tests change it through
/// [`MockWebshare::with`].
#[derive(Debug)]
pub struct WebshareState {
    pub proxies: Vec<MockProxy>,
    pub replaced: Vec<Value>,
    pub replacements: Vec<MockReplacement>,
    pub next_replacement_id: u64,
    pub next_proxy: u32,
    pub plan_id: u64,
    pub replacements_total: u32,
    pub replacements_used: u32,
    pub auto_replace: [bool; 4],
    /// Start and end of the subscription term, as the API writes them.
    pub term: (String, String),
    pub paused: bool,
    pub throttled: bool,
    pub available_countries: BTreeMap<String, u32>,
    /// Status reads a new replacement needs before it completes.
    pub polls_to_complete: u32,
    /// The next replacement fails with this code and message.
    pub fail_replacement: Option<(String, String)>,
    pub stats: Vec<Value>,
    /// The time the mock stamps its log of replaced proxies with. Tests
    /// that run on a manual clock keep it in step.
    pub now: DateTime<Utc>,
    pub injected: VecDeque<Injected>,
    /// Every request seen: method, path, query.
    pub seen: Vec<(String, String, String)>,
}

impl WebshareState {
    fn new() -> Self {
        Self {
            proxies: Vec::new(),
            replaced: Vec::new(),
            replacements: Vec::new(),
            next_replacement_id: 98_315,
            next_proxy: 1,
            plan_id: 4242,
            replacements_total: 10,
            replacements_used: 0,
            auto_replace: [false; 4],
            term: (
                "2026-10-01T11:19:14.489458-07:00".to_owned(),
                "2026-10-31T11:19:14.489461-07:00".to_owned(),
            ),
            paused: false,
            throttled: false,
            available_countries: BTreeMap::from([("DE".to_owned(), 40), ("AT".to_owned(), 5)]),
            polls_to_complete: 2,
            fail_replacement: None,
            stats: Vec::new(),
            now: Utc::now(),
            injected: VecDeque::new(),
            seen: Vec::new(),
        }
    }

    /// Adds `count` proxies in `country` with addresses in 198.51.100.0/24
    /// and returns their addresses.
    pub fn add_proxies(&mut self, count: u32, country: &str) -> Vec<String> {
        (0..count).map(|_| self.add_proxy(country)).collect()
    }

    pub fn add_proxy(&mut self, country: &str) -> String {
        let number = self.next_proxy;
        self.next_proxy += 1;
        let ip = format!("198.51.100.{number}");
        self.proxies.push(MockProxy {
            id: format!("d-{}", 10_000 + number),
            ip: ip.clone(),
            port: 8000 + u16::try_from(number).unwrap(),
            country: country.to_owned(),
            city: "Testville".to_owned(),
            valid: true,
            username: PROXY_USER.to_owned(),
            password: PROXY_PASS.to_owned(),
        });
        ip
    }

    /// Adds a proxy that points at a local [`TestProxy`].
    pub fn add_local_proxy(&mut self, proxy: &TestProxy, country: &str) {
        let number = self.next_proxy;
        self.next_proxy += 1;
        self.proxies.push(MockProxy {
            id: format!("d-{}", 10_000 + number),
            ip: proxy.addr.ip().to_string(),
            port: proxy.addr.port(),
            country: country.to_owned(),
            city: "Testville".to_owned(),
            valid: true,
            username: proxy.username.clone(),
            password: proxy.password.clone(),
        });
    }

    /// Requests seen for a path prefix, optionally of one method.
    pub fn count(&self, method: Option<&str>, path_prefix: &str) -> usize {
        self.seen
            .iter()
            .filter(|(seen_method, path, _)| {
                path.starts_with(path_prefix) && method.is_none_or(|method| method == seen_method)
            })
            .count()
    }

    fn replacement_json(&self, replacement: &MockReplacement) -> Value {
        let done = replacement.state == "completed";
        let failed = replacement.state == "failed";
        let (code, message) = self
            .fail_replacement
            .clone()
            .filter(|_| failed)
            .map_or((Value::Null, Value::Null), |(code, message)| {
                (json!(code), json!(message))
            });
        json!({
            "id": replacement.id,
            "to_replace": replacement.request["to_replace"],
            "replace_with": replacement.request["replace_with"],
            "dry_run": false,
            "state": replacement.state,
            "proxies_removed": if done { json!(1) } else { Value::Null },
            "proxies_added": if done { json!(1) } else { Value::Null },
            "reason": if done { "proxy_replaced" } else { "" },
            "error": message,
            "error_code": code,
            "created_at": "2026-10-08T21:25:13.966946-07:00",
            "dry_run_completed_at": null,
            "completed_at": if done { json!("2026-10-08T21:26:13.966946-07:00") } else { Value::Null },
        })
    }

    /// The provider replaces a proxy by itself, as its automatic settings
    /// do: the address leaves the list, a new one arrives, and the log of
    /// replaced proxies says why.
    pub fn auto_replace(&mut self, ip: &str, reason: &str, at: DateTime<Utc>) -> String {
        let index = self
            .proxies
            .iter()
            .position(|proxy| proxy.ip == ip)
            .unwrap();
        let old = self.proxies.remove(index);
        let new_ip = self.add_proxy(&old.country);
        let new = self.proxies.last().unwrap().clone();
        self.replaced.insert(
            0,
            json!({
                "id": 90_000 + self.replaced.len(),
                "reason": reason,
                "proxy": old.ip,
                "proxy_port": old.port,
                "proxy_country_code": old.country,
                "replaced_with": new.ip,
                "replaced_with_port": new.port,
                "replaced_with_country_code": new.country,
                "created_at": at.to_rfc3339(),
            }),
        );
        new_ip
    }
}

fn error(status: u16, body: Value) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(body)
}

fn not_found() -> ResponseTemplate {
    error(404, json!({ "detail": "Not found." }))
}

struct WebshareResponder {
    state: Arc<Mutex<WebshareState>>,
    base: String,
}

impl WebshareResponder {
    fn list(
        &self,
        state: &WebshareState,
        items: &[Value],
        request: &Request,
        path: &str,
    ) -> ResponseTemplate {
        let query: HashMap<String, String> = request.url.query_pairs().into_owned().collect();
        let page: usize = query
            .get("page")
            .and_then(|page| page.parse().ok())
            .unwrap_or(1);
        let page_size: usize = query
            .get("page_size")
            .and_then(|size| size.parse().ok())
            .unwrap_or(25);
        if page == 0 || page_size == 0 {
            return error(404, json!({ "detail": "Invalid page." }));
        }
        let start = (page - 1) * page_size;
        if start > items.len() || (start == items.len() && page > 1) {
            return error(404, json!({ "detail": "Invalid page." }));
        }
        let end = (start + page_size).min(items.len());
        let link = |number: usize| {
            format!(
                "{}{path}?mode=direct&page={number}&page_size={page_size}&plan_id={}",
                self.base, state.plan_id
            )
        };
        ResponseTemplate::new(200).set_body_json(json!({
            "count": items.len(),
            "next": if end < items.len() { json!(link(page + 1)) } else { Value::Null },
            "previous": if page > 1 { json!(link(page - 1)) } else { Value::Null },
            "results": items[start..end],
        }))
    }

    fn create_replacement(state: &mut WebshareState, request: &Request) -> ResponseTemplate {
        let Ok(body) = request.body_json::<Value>() else {
            return error(400, json!({ "detail": "JSON parse error" }));
        };
        let ip = body["to_replace"]["ip_addresses"][0].as_str();
        let (Some("ip_address"), Some(ip)) = (body["to_replace"]["type"].as_str(), ip) else {
            return error(
                400,
                json!({ "to_replace": [{ "message": "Invalid definition", "code": "invalid" }] }),
            );
        };
        if body["dry_run"] != json!(false) || !body["replace_with"].is_array() {
            return error(
                400,
                json!({ "replace_with": [{ "message": "Invalid definition", "code": "invalid" }] }),
            );
        }
        if !state.proxies.iter().any(|proxy| proxy.ip == ip) {
            return error(
                400,
                json!({ "to_replace": [{ "message": "No proxies match", "code": "invalid" }] }),
            );
        }
        if state
            .replacements
            .iter()
            .any(|replacement| !matches!(replacement.state.as_str(), "completed" | "failed"))
        {
            return error(
                400,
                json!({ "non_field_errors": [{
                    "message": "You are already replacing a proxy. Please wait a moment and try again.",
                    "code": "active_replacement",
                }] }),
            );
        }
        if state.replacements_used >= state.replacements_total {
            return error(
                400,
                json!({ "non_field_errors": [{
                    "message": "You have 0 proxy replacements remaining. Upgrade subscription to receive more replacements.",
                    "code": "not_enough_replacements_in_subscription",
                }] }),
            );
        }
        let replacement = MockReplacement {
            id: state.next_replacement_id,
            ip: ip.to_owned(),
            country: body["replace_with"][0]["country_code"]
                .as_str()
                .map(str::to_owned),
            state: "validating".to_owned(),
            polls_left: state.polls_to_complete,
            request: body,
        };
        state.next_replacement_id += 1;
        let answer = state.replacement_json(&replacement);
        state.replacements.push(replacement);
        ResponseTemplate::new(201).set_body_json(answer)
    }

    fn read_replacement(state: &mut WebshareState, id: &str) -> ResponseTemplate {
        let Some(index) = state
            .replacements
            .iter()
            .position(|replacement| replacement.id.to_string() == id)
        else {
            return not_found();
        };
        let mut replacement = state.replacements[index].clone();
        if !matches!(replacement.state.as_str(), "completed" | "failed") {
            if replacement.polls_left > 0 {
                replacement.polls_left -= 1;
                replacement.state = "processing".to_owned();
            } else if state.fail_replacement.is_some() {
                replacement.state = "failed".to_owned();
            } else {
                replacement.state = "completed".to_owned();
                let country = replacement
                    .country
                    .clone()
                    .unwrap_or_else(|| "DE".to_owned());
                // The new address arrives, the old one leaves.
                let new_ip = state.add_proxy(&country);
                let new = state.proxies.last().unwrap().clone();
                let old_index = state
                    .proxies
                    .iter()
                    .position(|proxy| proxy.ip == replacement.ip)
                    .unwrap();
                let old = state.proxies.remove(old_index);
                state.replaced.insert(
                    0,
                    json!({
                        "id": 93_000 + state.replaced.len(),
                        "reason": "proxy_replaced",
                        "proxy": old.ip,
                        "proxy_port": old.port,
                        "proxy_country_code": old.country,
                        "replaced_with": new_ip,
                        "replaced_with_port": new.port,
                        "replaced_with_country_code": new.country,
                        "created_at": state.now.to_rfc3339(),
                    }),
                );
                state.replacements_used += 1;
                if let Some(stock) = state.available_countries.get_mut(&country) {
                    *stock = stock.saturating_sub(1);
                }
            }
            state.replacements[index] = replacement.clone();
        }
        ResponseTemplate::new(200).set_body_json(state.replacement_json(&replacement))
    }

    fn plan(state: &WebshareState) -> Value {
        let mut countries: BTreeMap<String, u32> = BTreeMap::new();
        for proxy in &state.proxies {
            *countries.entry(proxy.country.clone()).or_insert(0) += 1;
        }
        json!({
            "id": state.plan_id,
            "status": "active",
            "bandwidth_limit": 0.0,
            "monthly_price": 20.0,
            "yearly_price": 200.0,
            "proxy_type": "dedicated",
            "proxy_subtype": "isp",
            "proxy_count": state.proxies.len(),
            "proxy_countries": countries,
            "required_site_checks": [],
            "on_demand_refreshes_total": 0,
            "on_demand_refreshes_used": 0,
            "on_demand_refreshes_available": 0,
            "automatic_refresh_frequency": 0,
            "automatic_refresh_last_at": null,
            "automatic_refresh_next_at": null,
            "proxy_replacements_total": state.replacements_total,
            "proxy_replacements_used": state.replacements_used,
            "proxy_replacements_available": state.replacements_total.saturating_sub(state.replacements_used),
            "subusers_total": 3,
            "subusers_used": 0,
            "subusers_available": 3,
            "is_unlimited_ip_authorizations": true,
            "is_high_concurrency": true,
            "is_high_priority_network": false,
            "created_at": "2026-09-01T10:00:00.000000-07:00",
            "updated_at": "2026-09-01T10:00:00.000000-07:00",
        })
    }
}

impl Respond for WebshareResponder {
    #[allow(clippy::too_many_lines)]
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let mut state = self.state.lock().unwrap();
        let method = request.method.as_str().to_owned();
        let path = request.url.path().to_owned();
        state.seen.push((
            method.clone(),
            path.clone(),
            request.url.query().unwrap_or("").to_owned(),
        ));

        let authorized = request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some(&format!("Token {WEBSHARE_TOKEN}"));
        if !authorized {
            return error(401, json!({ "detail": "Invalid token." }));
        }

        if let Some(index) = state.injected.iter().position(|injected| {
            path.starts_with(&injected.path_prefix)
                && injected
                    .method
                    .as_ref()
                    .is_none_or(|wanted| *wanted == method)
        }) {
            let injected = state.injected[index].clone();
            if injected.times <= 1 {
                state.injected.remove(index);
            } else {
                state.injected[index].times -= 1;
            }
            let mut response = error(injected.status, injected.body);
            if let Some(seconds) = injected.retry_after {
                response = response.insert_header("Retry-After", seconds.to_string().as_str());
            }
            return response;
        }

        let query: HashMap<String, String> = request.url.query_pairs().into_owned().collect();
        let plan_matches = query
            .get("plan_id")
            .is_none_or(|plan| *plan == state.plan_id.to_string());
        if !plan_matches {
            return error(
                400,
                json!({ "plan_id": [{ "message": "Unknown plan", "code": "invalid" }] }),
            );
        }
        let needs_plan = || {
            error(
                400,
                json!({ "plan_id": [{ "message": "This field is required.", "code": "required" }] }),
            )
        };

        match (method.as_str(), path.as_str()) {
            ("GET", "/api/v2/proxy/list/") => {
                if query.get("mode").map(String::as_str) != Some("direct") {
                    return error(
                        400,
                        json!({ "mode": [{ "message": "This field is required.", "code": "required" }] }),
                    );
                }
                let items: Vec<Value> = state.proxies.iter().map(MockProxy::json).collect();
                self.list(&state, &items, request, &path)
            }
            ("GET", "/api/v2/proxy/list/replaced/") => {
                let items = state.replaced.clone();
                self.list(&state, &items, request, &path)
            }
            ("POST", "/api/v3/proxy/replace/") => Self::create_replacement(&mut state, request),
            ("GET", "/api/v2/subscription/") => ResponseTemplate::new(200).set_body_json(json!({
                "id": 1,
                "plan": state.plan_id,
                "payment_method": 1,
                "free_credits": 0.0,
                "term": "monthly",
                "start_date": state.term.0,
                "end_date": state.term.1,
                "renewals_paid": 3,
                "renewals_enabled": true,
                "failed_payment_times": 0,
                "paused": state.paused,
                "throttled": state.throttled,
                "created_at": "2026-07-01T11:58:10.246406-07:00",
                "updated_at": "2026-10-01T11:58:10.246406-07:00",
            })),
            ("GET", "/api/v3/proxy/config") => {
                if !query.contains_key("plan_id") {
                    return needs_plan();
                }
                ResponseTemplate::new(200).set_body_json(json!({
                    "request_timeout": 86400,
                    "request_idle_timeout": 900,
                    "ip_authorization_country_codes": null,
                    "auto_replace_invalid_proxies": state.auto_replace[0],
                    "auto_replace_low_country_confidence_proxies": state.auto_replace[1],
                    "auto_replace_out_of_rotation_proxies": state.auto_replace[2],
                    "auto_replace_failed_site_check_proxies": state.auto_replace[3],
                    "proxy_list_download_token": "download-token",
                }))
            }
            ("GET", "/api/v3/proxy/list/stats") => {
                if !query.contains_key("plan_id") {
                    return needs_plan();
                }
                ResponseTemplate::new(200).set_body_json(json!({
                    "available_countries": state.available_countries,
                    "ip_ranges_24": {},
                    "asns": {},
                }))
            }
            ("GET", "/api/v2/stats/") => {
                if !query.contains_key("timestamp__gte") || !query.contains_key("timestamp__lte") {
                    return error(
                        400,
                        json!({ "detail": "timestamps are required in this mock" }),
                    );
                }
                ResponseTemplate::new(200).set_body_json(json!(state.stats))
            }
            ("GET", _) if path.starts_with("/api/v2/subscription/plan/") => {
                let id = path
                    .trim_start_matches("/api/v2/subscription/plan/")
                    .trim_end_matches('/');
                if id == state.plan_id.to_string() && path.ends_with('/') {
                    ResponseTemplate::new(200).set_body_json(Self::plan(&state))
                } else {
                    not_found()
                }
            }
            ("GET", _) if path.starts_with("/api/v3/proxy/replace/") && path.ends_with('/') => {
                let id = path
                    .trim_start_matches("/api/v3/proxy/replace/")
                    .trim_end_matches('/')
                    .to_owned();
                Self::read_replacement(&mut state, &id)
            }
            _ => not_found(),
        }
    }
}

/// A mock of the Webshare API. See the module documentation.
pub struct MockWebshare {
    pub server: MockServer,
    state: Arc<Mutex<WebshareState>>,
}

impl MockWebshare {
    pub async fn start() -> Self {
        let server = MockServer::start().await;
        let state = Arc::new(Mutex::new(WebshareState::new()));
        Mock::given(any())
            .respond_with(WebshareResponder {
                state: Arc::clone(&state),
                base: server.uri(),
            })
            .mount(&server)
            .await;
        Self { server, state }
    }

    /// The API root to configure as `webshare.base_url`.
    pub fn base_url(&self) -> String {
        format!("{}/api/", self.server.uri())
    }

    /// Reads or changes the mock account.
    pub fn with<T>(&self, change: impl FnOnce(&mut WebshareState) -> T) -> T {
        change(&mut self.state.lock().unwrap())
    }

    /// Makes the next `times` requests to a path answer with `status`.
    pub fn inject(
        &self,
        method: Option<&str>,
        path_prefix: &str,
        status: u16,
        body: Value,
        times: u32,
    ) {
        self.with(|state| {
            state.injected.push_back(Injected {
                path_prefix: path_prefix.to_owned(),
                method: method.map(str::to_owned),
                status,
                body,
                retry_after: None,
                times,
            });
        });
    }

    /// Makes the next `times` requests to a path answer 429.
    pub fn rate_limit(&self, path_prefix: &str, retry_after: Option<u64>, times: u32) {
        self.with(|state| {
            state.injected.push_back(Injected {
                path_prefix: path_prefix.to_owned(),
                method: None,
                status: 429,
                body: json!({ "detail": "Request was throttled. Expected available in 60 seconds." }),
                retry_after,
                times,
            });
        });
    }

    /// Every request the mock received, with headers.
    pub async fn requests(&self) -> Vec<Request> {
        self.server.received_requests().await.unwrap_or_default()
    }
}

// --- Echo ----------------------------------------------------------------------

/// One request the echo received.
#[derive(Debug, Clone)]
pub struct EchoHit {
    pub path: String,
    pub peer: IpAddr,
    pub headers: HeaderMap,
}

/// What the echo's geo lookup says about an address.
#[derive(Debug, Clone)]
pub struct GeoFixture {
    pub country: String,
    pub city: String,
    pub org: String,
    pub timezone: String,
}

impl GeoFixture {
    pub fn new(country: &str, city: &str, org: &str, timezone: &str) -> Self {
        Self {
            country: country.to_owned(),
            city: city.to_owned(),
            org: org.to_owned(),
            timezone: timezone.to_owned(),
        }
    }

    pub fn berlin() -> Self {
        Self::new(
            "DE",
            "Berlin",
            "AS3320 Deutsche Telekom AG",
            "Europe/Berlin",
        )
    }

    pub fn vienna() -> Self {
        Self::new(
            "AT",
            "Vienna",
            "AS8447 A1 Telekom Austria AG",
            "Europe/Vienna",
        )
    }
}

#[derive(Default)]
struct EchoState {
    hits: Mutex<Vec<EchoHit>>,
    geo: Mutex<HashMap<IpAddr, GeoFixture>>,
    /// Paths that answer 503.
    down: Mutex<HashSet<String>>,
}

/// A local echo: `/ip` and `/confirm` answer with the caller's address as
/// text, `/json` with a geo record for it in the shape of ipinfo.io.
pub struct Echo {
    pub addr: SocketAddr,
    state: Arc<EchoState>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Echo {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn echo_ip(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<EchoState>>,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> axum::response::Response {
    let path = uri.path().to_owned();
    state.hits.lock().unwrap().push(EchoHit {
        path: path.clone(),
        peer: peer.ip(),
        headers,
    });
    if state.down.lock().unwrap().contains(&path) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if path == "/json" {
        let geo = state.geo.lock().unwrap().get(&peer.ip()).cloned();
        return match geo {
            Some(geo) => axum::Json(json!({
                "ip": peer.ip().to_string(),
                "city": geo.city,
                "region": format!("State of {}", geo.city),
                "country": geo.country,
                "loc": "52.5200,13.4050",
                "org": geo.org,
                "postal": "10115",
                "timezone": geo.timezone,
                "readme": "https://ipinfo.io/missingauth",
            }))
            .into_response(),
            // An address the lookup knows nothing about.
            None => {
                axum::Json(json!({ "ip": peer.ip().to_string(), "bogon": true })).into_response()
            }
        };
    }
    format!("{}\n", peer.ip()).into_response()
}

impl Echo {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(EchoState::default());
        let app = axum::Router::new()
            .route("/ip", get(echo_ip))
            .route("/confirm", get(echo_ip))
            .route("/host", get(echo_ip))
            .route("/json", get(echo_ip))
            .with_state(Arc::clone(&state));
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Self { addr, state, task }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// What the geo lookup says about an exit address.
    pub fn set_geo(&self, exit: IpAddr, geo: GeoFixture) {
        self.state.geo.lock().unwrap().insert(exit, geo);
    }

    /// Makes a path answer 503, or work again.
    pub fn set_down(&self, path: &str, down: bool) {
        let mut paths = self.state.down.lock().unwrap();
        if down {
            paths.insert(path.to_owned());
        } else {
            paths.remove(path);
        }
    }

    pub fn hits(&self) -> Vec<EchoHit> {
        self.state.hits.lock().unwrap().clone()
    }

    /// How many requests a path received.
    pub fn count(&self, path: &str) -> usize {
        self.hits().iter().filter(|hit| hit.path == path).count()
    }
}

// --- Test proxy ------------------------------------------------------------------

/// A local HTTP proxy with Basic authentication. It forwards plain HTTP
/// requests and `CONNECT` tunnels, and leaves through `exit`, a loopback
/// address of its own, so the echo sees a distinct "exit address" per
/// proxy. Loopback addresses other than 127.0.0.1 need no setup on Linux.
pub struct TestProxy {
    pub addr: SocketAddr,
    pub username: String,
    pub password: String,
    shared: Arc<ProxyShared>,
    task: tokio::task::JoinHandle<()>,
}

struct ProxyShared {
    exit: Mutex<IpAddr>,
    /// The `Proxy-Authorization` value the proxy accepts.
    expected: Mutex<String>,
    up: AtomicBool,
    requests: AtomicUsize,
    refused: AtomicUsize,
}

impl Drop for TestProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Standard base64, for the `Proxy-Authorization` header.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let bytes = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let word = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if index <= chunk.len() {
                out.push(char::from(ALPHABET[((word >> shift) & 0x3f) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn basic(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64(format!("{username}:{password}").as_bytes())
    )
}

impl TestProxy {
    /// Starts a proxy that leaves through `exit` and accepts the given
    /// credentials.
    pub async fn start(exit: IpAddr, username: &str, password: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(ProxyShared {
            exit: Mutex::new(exit),
            expected: Mutex::new(basic(username, password)),
            up: AtomicBool::new(true),
            requests: AtomicUsize::new(0),
            refused: AtomicUsize::new(0),
        });
        let task = tokio::spawn({
            let shared = Arc::clone(&shared);
            async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let shared = Arc::clone(&shared);
                    tokio::spawn(async move {
                        let _ = serve_proxy(stream, &shared).await;
                    });
                }
            }
        });
        Self {
            addr,
            username: username.to_owned(),
            password: password.to_owned(),
            shared,
            task,
        }
    }

    /// A proxy that leaves through `127.0.0.<last>`.
    pub async fn with_exit(last: u8) -> Self {
        Self::start(
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, last)),
            PROXY_USER,
            PROXY_PASS,
        )
        .await
    }

    pub fn exit(&self) -> IpAddr {
        *self.shared.exit.lock().unwrap()
    }

    /// Changes the address the proxy leaves through.
    pub fn set_exit(&self, exit: IpAddr) {
        *self.shared.exit.lock().unwrap() = exit;
    }

    /// Takes the proxy down (connections are closed at once) or up again.
    pub fn set_up(&self, up: bool) {
        self.shared.up.store(up, Ordering::SeqCst);
    }

    /// Changes the credentials the proxy accepts.
    pub fn set_credentials(&self, username: &str, password: &str) {
        *self.shared.expected.lock().unwrap() = basic(username, password);
    }

    /// Requests that were forwarded.
    pub fn requests(&self) -> usize {
        self.shared.requests.load(Ordering::SeqCst)
    }

    /// Requests that were refused for their credentials.
    pub fn refused(&self) -> usize {
        self.shared.refused.load(Ordering::SeqCst)
    }

    /// The proxy as a line of a static proxy list.
    pub fn line(&self, country: &str) -> String {
        format!(
            "{}:{}:{}:{}:{country}",
            self.addr.ip(),
            self.addr.port(),
            self.username,
            self.password
        )
    }
}

async fn serve_proxy(mut client: TcpStream, shared: &ProxyShared) -> std::io::Result<()> {
    if !shared.up.load(Ordering::SeqCst) {
        // Closed without a word, as a dead proxy does.
        return Ok(());
    }
    // Read the request head.
    let mut buffer = Vec::new();
    let head_end = loop {
        let mut chunk = [0_u8; 2048];
        let read = client.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 64 * 1024 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let rest = buffer[head_end..].to_vec();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_owned();
    let headers: Vec<&str> = lines.filter(|line| !line.is_empty()).collect();
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(());
    };

    let authorization = headers.iter().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("proxy-authorization")
            .then(|| value.trim().to_owned())
    });
    if authorization.as_deref() != Some(shared.expected.lock().unwrap().as_str()) {
        shared.refused.fetch_add(1, Ordering::SeqCst);
        client
            .write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\n\
                  Proxy-Authenticate: Basic realm=\"test\"\r\n\
                  Content-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await?;
        return Ok(());
    }

    let exit = *shared.exit.lock().unwrap();
    let connect = |authority: String| async move {
        let target: SocketAddr = tokio::net::lookup_host(authority)
            .await?
            .next()
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "the target does not resolve")
            })?;
        let socket = TcpSocket::new_v4()?;
        socket.bind(SocketAddr::new(exit, 0))?;
        socket.connect(target).await
    };

    if method.eq_ignore_ascii_case("CONNECT") {
        let Ok(mut upstream) = connect(target.to_owned()).await else {
            client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await?;
            return Ok(());
        };
        shared.requests.fetch_add(1, Ordering::SeqCst);
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        upstream.write_all(&rest).await?;
        tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
        return Ok(());
    }

    // A plain request in absolute form: http://host:port/path.
    let Some(without_scheme) = target.strip_prefix("http://") else {
        client
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    };
    let (authority, path) = match without_scheme.find('/') {
        Some(slash) => (&without_scheme[..slash], &without_scheme[slash..]),
        None => (without_scheme, "/"),
    };
    let authority = if authority.contains(':') {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    };
    let Ok(mut upstream) = connect(authority).await else {
        client
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    };
    shared.requests.fetch_add(1, Ordering::SeqCst);
    let mut forwarded = format!("{method} {path} HTTP/1.1\r\n");
    for line in headers {
        let name = line.split(':').next().unwrap_or_default();
        if name.eq_ignore_ascii_case("proxy-authorization")
            || name.eq_ignore_ascii_case("proxy-connection")
        {
            continue;
        }
        forwarded.push_str(line);
        forwarded.push_str("\r\n");
    }
    forwarded.push_str("\r\n");
    upstream.write_all(forwarded.as_bytes()).await?;
    upstream.write_all(&rest).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// 127.0.0.`last`.
pub fn loopback(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(127, 0, 0, last))
}
