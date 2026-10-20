//! The Webshare API client (spec §7.2).
//!
//! Paths and parameters were verified against <https://apidocs.webshare.io>
//! on 2026-10-08. The API root is `https://proxy.webshare.io/api/`; some
//! endpoints live under `v2/`, some under `v3/`:
//!
//! | Purpose | Method and path |
//! |---|---|
//! | Proxy list | `GET v2/proxy/list/?mode=direct&page=&page_size=` |
//! | Request a replacement | `POST v3/proxy/replace/` |
//! | Replacement status | `GET v3/proxy/replace/{id}/` |
//! | Replaced proxies | `GET v2/proxy/list/replaced/?page=&page_size=` |
//! | Subscription | `GET v2/subscription/` |
//! | Plan with replacement quota | `GET v2/subscription/plan/{id}/` |
//! | Proxy config (auto-replace flags) | `GET v3/proxy/config?plan_id=` |
//! | List stats (countries available) | `GET v3/proxy/list/stats?plan_id=` |
//! | Hourly traffic | `GET v2/stats/?timestamp__gte=&timestamp__lte=` |
//!
//! Every request carries `Authorization: Token <key>`. Webshare allows 240
//! requests a minute in general and 60 a minute on the proxy list
//! endpoints, and asks to wait 60 seconds after a 429. The client keeps its
//! own token buckets below both limits and stays away for as long as a 429
//! says.
//!
//! The client is plain wreq: no browser emulation and an honest
//! `User-Agent`. Webshare is a vendor, not a target. It never goes through
//! a proxy.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use kage_core::config::WebshareConfig;
use kage_core::domain::{CountryCode, Proxy, ProxyProviderKind};
use kage_core::secret::Secret;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use url::Url;
use wreq::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use wreq::{Method, StatusCode};

use super::{
    AccountInfo, AutoReplace, Capabilities, ProviderError, ProviderProxy, ProxyActivity,
    ProxyProvider, ReplaceSpec, ReplaceState, ReplaceTicket, ReplacedProxy, Result,
};
use crate::rate::TokenBucket;

/// Webshare's own address echo: what the health probes read through its
/// proxies unless `proxies.health.echo_url` names another. Webshare sees
/// every connection through its proxies anyway, so it adds no observer.
pub const ECHO_URL: &str = "https://ipv4.webshare.io/";

/// Everything the trait offers, and Webshare's echo.
pub const CAPABILITIES: Capabilities = Capabilities {
    replacement: true,
    replaced_log: true,
    account: true,
    stats: true,
    echo: Some(ECHO_URL),
};

/// What the client calls itself.
const USER_AGENT: &str = concat!("kage/", env!("CARGO_PKG_VERSION"));

/// Most pages read from one paged endpoint. At 100 proxies a page this is
/// far beyond any plan; it only bounds a server that never ends its list.
const MAX_PAGES: u32 = 200;

/// Webshare keeps statistics for this long.
const STATS_HISTORY: chrono::Duration = chrono::Duration::days(90);

/// First wait before a read is retried; it doubles with every try.
const RETRY_BASE: Duration = Duration::from_millis(500);

/// Longest `Retry-After` the client honors in one go.
const MAX_RETRY_AFTER: Duration = Duration::from_mins(15);

/// Which of Webshare's rate limits an endpoint falls under.
#[derive(Debug, Clone, Copy)]
enum Limit {
    General,
    List,
}

/// One API call.
struct Call<'a> {
    method: Method,
    path: String,
    query: Vec<(&'a str, String)>,
    body: Option<serde_json::Value>,
    limit: Limit,
}

impl<'a> Call<'a> {
    fn get(path: impl Into<String>, limit: Limit) -> Self {
        Self {
            method: Method::GET,
            path: path.into(),
            query: Vec::new(),
            body: None,
            limit,
        }
    }

    fn query(mut self, name: &'a str, value: impl Into<String>) -> Self {
        self.query.push((name, value.into()));
        self
    }
}

/// The Webshare list, replacement and account API.
pub struct WebshareProvider {
    client: wreq::Client,
    base: Url,
    authorization: HeaderValue,
    plan_id: Option<u64>,
    page_size: u32,
    general: TokenBucket,
    list: TokenBucket,
    rate_limit_backoff: Duration,
    max_retries: u32,
}

impl std::fmt::Debug for WebshareProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebshareProvider")
            .field("base", &self.base.as_str())
            .field("plan_id", &self.plan_id)
            .finish_non_exhaustive()
    }
}

impl WebshareProvider {
    /// A client for the configured account.
    ///
    /// # Errors
    ///
    /// Fails when the configuration has no API key, the key cannot be sent
    /// in a header, or the HTTP client cannot be built.
    pub fn new(config: &WebshareConfig) -> Result<Self> {
        let key = config
            .api_key
            .as_ref()
            .ok_or_else(|| ProviderError::Setup("KAGE__WEBSHARE__API_KEY is not set".to_owned()))?;
        Self::with_key(config, key)
    }

    fn with_key(config: &WebshareConfig, key: &Secret) -> Result<Self> {
        let mut authorization =
            HeaderValue::from_str(&format!("Token {}", key.expose())).map_err(|_| {
                ProviderError::Setup(
                    "the Webshare API key contains characters that cannot be sent in a header"
                        .to_owned(),
                )
            })?;
        authorization.set_sensitive(true);
        let base = Url::parse(&config.base_url)
            .map_err(|_| ProviderError::Setup("webshare.base_url is not a URL".to_owned()))?;
        let client = wreq::Client::builder()
            // The provider API is reached directly, never through a proxy,
            // and never through one named in the environment.
            .no_proxy()
            .user_agent(USER_AGENT)
            .timeout(config.timeout.get())
            .connect_timeout(config.timeout.get())
            .redirect(wreq::redirect::Policy::limited(3))
            .build()
            .map_err(|error| ProviderError::Setup(format!("cannot build the HTTP client: {error}")))?;
        Ok(Self {
            client,
            base,
            authorization,
            plan_id: config.plan_id,
            page_size: config.page_size,
            general: TokenBucket::for_rate(config.requests_per_minute),
            list: TokenBucket::for_rate(config.list_requests_per_minute),
            rate_limit_backoff: config.rate_limit_backoff.get(),
            max_retries: config.max_retries,
        })
    }

    /// Adds `plan_id` to a call when a plan is configured.
    fn for_configured_plan<'a>(&self, call: Call<'a>) -> Call<'a> {
        match self.plan_id {
            Some(id) => call.query("plan_id", id.to_string()),
            None => call,
        }
    }

    fn url(&self, call: &Call<'_>) -> Result<Url> {
        let mut url = self
            .base
            .join(&call.path)
            .map_err(|_| ProviderError::Setup(format!("cannot build the URL of {}", call.path)))?;
        if !call.query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in &call.query {
                pairs.append_pair(name, value);
            }
        }
        Ok(url)
    }

    /// Sends one call and returns the decoded JSON body.
    ///
    /// A 429 is always retried after the wait it names, because the request
    /// was refused, not applied. A failed read is retried too. A request
    /// that changes something is never sent twice after a failure that may
    /// have applied it.
    async fn send<T: DeserializeOwned>(&self, call: Call<'_>) -> Result<T> {
        let url = self.url(&call)?;
        let reads = call.method == Method::GET;
        let bucket = match call.limit {
            Limit::General => &self.general,
            Limit::List => &self.list,
        };
        let body = call
            .body
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|error| ProviderError::Decode(error.to_string()))?;

        let mut attempt = 0;
        loop {
            bucket.acquire().await;
            let error = match self
                .exchange(&call.method, &url, body.as_deref(), reads)
                .await
            {
                Ok(value) => {
                    return serde_json::from_value(value)
                        .map_err(|error| ProviderError::Decode(format!("{}: {error}", call.path)));
                }
                Err(error) => error,
            };

            let retry = match &error {
                ProviderError::RateLimited { retry_after } => {
                    // Everyone sharing the bucket stays away, not only
                    // this call; the next `acquire` waits the block out.
                    bucket.block_for(*retry_after);
                    tracing::warn!(
                        path = %call.path,
                        retry_after_ms = retry_after.as_millis(),
                        "the Webshare API is rate limiting"
                    );
                    true
                }
                ProviderError::Transport(_) => reads,
                ProviderError::Status { status, .. } => reads && *status >= 500,
                _ => false,
            };
            if !retry || attempt >= self.max_retries {
                return Err(error);
            }
            if !matches!(error, ProviderError::RateLimited { .. }) {
                tokio::time::sleep(RETRY_BASE.saturating_mul(2_u32.saturating_pow(attempt))).await;
            }
            attempt += 1;
        }
    }

    /// One request and its answer, as the JSON body or the error it stands
    /// for.
    async fn exchange(
        &self,
        method: &Method,
        url: &Url,
        body: Option<&[u8]>,
        reads: bool,
    ) -> Result<serde_json::Value> {
        let mut request = self
            .client
            .request(method.clone(), url.as_str())
            .header(AUTHORIZATION, self.authorization.clone())
            .header(ACCEPT, "application/json");
        if let Some(body) = body {
            request = request
                .header(CONTENT_TYPE, "application/json")
                .body(body.to_vec());
        }

        let response = request.send().await.map_err(|error| {
            // A connection that was never made sent nothing.
            if reads || error.is_connect() {
                ProviderError::Transport(describe(&error))
            } else {
                ProviderError::OutcomeUnknown(describe(&error))
            }
        })?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(ProviderError::RateLimited {
                retry_after: retry_after(response.headers())
                    .unwrap_or(self.rate_limit_backoff)
                    .min(MAX_RETRY_AFTER),
            });
        }
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes.to_vec(),
            // The server said yes but the answer was lost on the way.
            Err(error) if status.is_success() => {
                return Err(if reads {
                    ProviderError::Transport(describe(&error))
                } else {
                    ProviderError::OutcomeUnknown(describe(&error))
                });
            }
            // An error status says enough without its body.
            Err(_) => Vec::new(),
        };
        match classify(status, &bytes) {
            // A server error after a request that changes something leaves
            // its outcome open.
            Err(ProviderError::Status {
                status, message, ..
            }) if !reads && status >= 500 => Err(ProviderError::OutcomeUnknown(format!(
                "HTTP {status}: {message}"
            ))),
            other => other,
        }
    }

    /// Reads every page of a paged endpoint, stopping early when `more`
    /// says a page was the last one of interest.
    async fn pages<T: DeserializeOwned>(
        &self,
        build: impl Fn(u32) -> Call<'static>,
        mut more: impl FnMut(&[T]) -> bool,
    ) -> Result<Vec<T>> {
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let batch: Page<T> = self.send(build(page)).await?;
            let last = batch.next.is_none() || batch.results.is_empty();
            let wanted = more(&batch.results);
            all.extend(batch.results);
            if last || !wanted {
                return Ok(all);
            }
        }
        Err(ProviderError::Decode(format!(
            "the list did not end after {MAX_PAGES} pages"
        )))
    }
}

#[async_trait]
impl ProxyProvider for WebshareProvider {
    fn kind(&self) -> ProxyProviderKind {
        ProxyProviderKind::Webshare
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    async fn list(&self) -> Result<Vec<ProviderProxy>> {
        let page_size = self.page_size.to_string();
        let listed: Vec<ListedProxy> = self
            .pages(
                |page| {
                    self.for_configured_plan(
                        Call::get("v2/proxy/list/", Limit::List)
                            .query("mode", "direct")
                            .query("page", page.to_string())
                            .query("page_size", page_size.clone()),
                    )
                },
                |_| true,
            )
            .await?;
        listed.into_iter().map(ProviderProxy::try_from).collect()
    }

    async fn replace(&self, proxy: &Proxy, want: ReplaceSpec) -> Result<ReplaceTicket> {
        let replace_with = match want.country {
            Some(country) => serde_json::json!({
                "type": "country",
                "country_code": country.as_str(),
                "count": 1,
            }),
            None => serde_json::json!({ "type": "any", "count": 1 }),
        };
        let call = self.for_configured_plan(Call {
            method: Method::POST,
            path: "v3/proxy/replace/".to_owned(),
            query: Vec::new(),
            body: Some(serde_json::json!({
                "to_replace": { "type": "ip_address", "ip_addresses": [proxy.host] },
                "replace_with": [replace_with],
                "dry_run": false,
            })),
            limit: Limit::General,
        });
        let created: Replacement = self.send(call).await.map_err(|error| match error {
            ProviderError::Status {
                code: Some(code), ..
            } if code == "not_enough_replacements_in_subscription" => ProviderError::QuotaExhausted,
            ProviderError::Status {
                code: Some(code), ..
            } if code == "active_replacement" => ProviderError::Busy,
            other => other,
        })?;
        Ok(ReplaceTicket(created.id.into_string()))
    }

    async fn replace_status(&self, ticket: &ReplaceTicket) -> Result<ReplaceState> {
        // The ticket is an id this client received from the API; anything
        // else must not become part of a path.
        if ticket.0.is_empty() || !ticket.0.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(ProviderError::Decode(
                "the replacement ticket is not an id".to_owned(),
            ));
        }
        let call = self.for_configured_plan(Call::get(
            format!("v3/proxy/replace/{}/", ticket.0),
            Limit::General,
        ));
        let replacement: Replacement = self.send(call).await?;
        Ok(replacement.state())
    }

    async fn replaced(&self, since: DateTime<Utc>) -> Result<Vec<ReplacedProxy>> {
        let page_size = self.page_size.to_string();
        let entries: Vec<ReplacedEntry> = self
            .pages(
                |page| {
                    self.for_configured_plan(
                        Call::get("v2/proxy/list/replaced/", Limit::List)
                            // Newest first, so reading can stop at `since`.
                            .query("ordering", "-created_at")
                            .query("page", page.to_string())
                            .query("page_size", page_size.clone()),
                    )
                },
                // Stop once a page holds nothing new enough; if the server
                // ignored the ordering this still only costs completeness
                // beyond the first stale page.
                |page: &[ReplacedEntry]| page.iter().any(|entry| entry.created_at >= since),
            )
            .await?;
        Ok(entries
            .into_iter()
            .filter(|entry| entry.created_at >= since)
            .map(ReplacedProxy::from)
            .collect())
    }

    async fn account(&self) -> Result<AccountInfo> {
        let subscription: Subscription = self
            .send(Call::get("v2/subscription/", Limit::General))
            .await?;
        let plan_id = self
            .plan_id
            .map(|id| id.to_string())
            .or_else(|| subscription.plan.map(Id::into_string));
        let mut account = AccountInfo {
            plan_id: plan_id.clone(),
            term_started_at: subscription.start_date,
            term_ends_at: subscription.end_date,
            renewals_enabled: subscription.renewals_enabled,
            paused: subscription.paused,
            throttled: subscription.throttled,
            ..AccountInfo::default()
        };
        let Some(plan_id) = plan_id.filter(|id| id.bytes().all(|byte| byte.is_ascii_digit()))
        else {
            return Ok(account);
        };

        let plan: Plan = self
            .send(Call::get(
                format!("v2/subscription/plan/{plan_id}/"),
                Limit::General,
            ))
            .await?;
        account.plan_status = plan.status;
        account.proxy_type = match (plan.proxy_type, plan.proxy_subtype) {
            (Some(kind), Some(subtype)) => Some(format!("{kind}/{subtype}")),
            (kind, subtype) => kind.or(subtype),
        };
        account.proxy_count = plan.proxy_count;
        account.countries = plan.proxy_countries.unwrap_or_default();
        account.bandwidth_limit_gb = plan.bandwidth_limit;
        account.replacements_total = plan.proxy_replacements_total;
        account.replacements_used = plan.proxy_replacements_used;
        account.replacements_available = plan.proxy_replacements_available;

        // The two v3 reads add detail; the account is still useful without
        // them, so their failure is logged and not passed on.
        match self
            .send::<ProxyConfig>(
                Call::get("v3/proxy/config", Limit::General).query("plan_id", plan_id.clone()),
            )
            .await
        {
            Ok(config) => account.auto_replace = config.auto_replace(),
            Err(error) => tracing::warn!(%error, "cannot read the Webshare proxy config"),
        }
        match self
            .send::<ListStats>(
                Call::get("v3/proxy/list/stats", Limit::List).query("plan_id", plan_id),
            )
            .await
        {
            Ok(stats) => account.available_countries = stats.available_countries,
            Err(error) => tracing::warn!(%error, "cannot read the Webshare proxy list stats"),
        }
        Ok(account)
    }

    async fn stats(&self, since: DateTime<Utc>) -> Result<Vec<ProxyActivity>> {
        let now = Utc::now();
        // Webshare refuses a start more than 90 days back.
        let since = since.max(now - STATS_HISTORY + chrono::Duration::minutes(5));
        let call = self.for_configured_plan(
            Call::get("v2/stats/", Limit::General)
                .query(
                    "timestamp__gte",
                    since.to_rfc3339_opts(SecondsFormat::Secs, true),
                )
                .query(
                    "timestamp__lte",
                    now.to_rfc3339_opts(SecondsFormat::Secs, true),
                ),
        );
        let hours: Vec<HourlyStats> = self.send(call).await?;
        Ok(hours
            .into_iter()
            .filter(|hour| !hour.is_projected)
            .map(ProxyActivity::from)
            .collect())
    }
}

/// Turns a response into its JSON body or the error it stands for.
fn classify(status: StatusCode, body: &[u8]) -> Result<serde_json::Value> {
    if status.is_success() {
        return serde_json::from_slice(body)
            .map_err(|error| ProviderError::Decode(format!("the body is not JSON: {error}")));
    }
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(ProviderError::Unauthorized {
            status: status.as_u16(),
        });
    }
    let (code, message) = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .map(|value| error_details(&value))
        .unwrap_or_default();
    Err(ProviderError::Status {
        status: status.as_u16(),
        code,
        message: message
            .unwrap_or_else(|| status.canonical_reason().unwrap_or("no message").to_owned()),
    })
}

/// Finds the error code and message in an error body. Webshare answers in
/// the shapes of Django REST Framework: `{"detail": "…"}`, a list of
/// `{"message": "…", "code": "…"}` under a field name or
/// `non_field_errors`, or such an object at the top.
fn error_details(value: &serde_json::Value) -> (Option<String>, Option<String>) {
    fn text(value: Option<&serde_json::Value>) -> Option<String> {
        value?
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .map(|text| truncate(text.trim(), 300))
    }
    match value {
        serde_json::Value::Object(map) => {
            let code = text(map.get("code"));
            let message = text(map.get("message")).or_else(|| text(map.get("detail")));
            if code.is_some() || message.is_some() {
                return (code, message);
            }
            map.values()
                .map(error_details)
                .find(|(code, message)| code.is_some() || message.is_some())
                .unwrap_or_default()
        }
        serde_json::Value::Array(items) => items
            .iter()
            .map(error_details)
            .find(|(code, message)| code.is_some() || message.is_some())
            .unwrap_or_default(),
        serde_json::Value::String(message) if !message.trim().is_empty() => {
            (None, Some(truncate(message.trim(), 300)))
        }
        _ => (None, None),
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// `Retry-After` in seconds. The HTTP-date form is not used by Webshare.
fn retry_after(headers: &wreq::header::HeaderMap) -> Option<Duration> {
    let seconds: u64 = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(seconds))
}

/// A transport error as text. The request's URL never holds a secret: the
/// key travels in a header.
fn describe(error: &wreq::Error) -> String {
    let kind = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connection failed"
    } else {
        "request failed"
    };
    format!("{kind}: {error}")
}

/// An id the API writes as a number in some places and as text in others.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Id {
    Number(i64),
    Text(String),
}

impl Id {
    fn into_string(self) -> String {
        match self {
            Self::Number(number) => number.to_string(),
            Self::Text(text) => text,
        }
    }
}

/// A count the API may write as an integer or as a float.
fn count<'de, D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<u64, D::Error> {
    let value = Option::<f64>::deserialize(deserializer)?.unwrap_or(0.0);
    if value.is_finite() && value >= 0.0 {
        // Counts are whole and far below 2^53.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(value.round() as u64)
    } else {
        Err(serde::de::Error::custom("a count must not be negative"))
    }
}

/// A country code, or `None` for anything that is not one. Webshare writes
/// `ZZ` for "any country", which is no country either.
fn country(code: Option<&str>) -> Option<CountryCode> {
    CountryCode::new(code?.trim())
        .ok()
        .filter(|country| country.as_str() != "ZZ")
}

#[derive(Deserialize)]
struct Page<T> {
    #[serde(default)]
    next: Option<String>,
    #[serde(default = "Vec::new")]
    results: Vec<T>,
}

#[derive(Deserialize)]
struct ListedProxy {
    id: Id,
    username: String,
    password: String,
    #[serde(default)]
    proxy_address: Option<String>,
    port: u16,
    #[serde(default)]
    valid: Option<bool>,
    #[serde(default)]
    country_code: Option<String>,
    #[serde(default)]
    city_name: Option<String>,
    #[serde(default)]
    asn_number: Option<Id>,
}

impl TryFrom<ListedProxy> for ProviderProxy {
    type Error = ProviderError;

    fn try_from(listed: ListedProxy) -> Result<Self> {
        let host = listed
            .proxy_address
            .filter(|address| !address.trim().is_empty())
            .ok_or_else(|| {
                ProviderError::Decode(
                    "a listed proxy has no address: Kage needs static proxies in direct mode, \
                     not a rotating residential plan"
                        .to_owned(),
                )
            })?;
        Ok(Self {
            provider_ref: listed.id.into_string(),
            host,
            port: listed.port,
            username: Secret::new(listed.username),
            password: Secret::new(listed.password),
            country: country(listed.country_code.as_deref()),
            // Webshare states a country and declares nothing (decision D5).
            locale: None,
            timezone: None,
            city: listed.city_name.filter(|city| !city.trim().is_empty()),
            asn: listed
                .asn_number
                .map(|asn| format!("AS{}", asn.into_string())),
            valid: listed.valid,
        })
    }
}

#[derive(Deserialize)]
struct Replacement {
    id: Id,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    proxies_removed: Option<u32>,
    #[serde(default)]
    proxies_added: Option<u32>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_code: Option<String>,
}

impl Replacement {
    fn state(self) -> ReplaceState {
        match self.state.as_deref() {
            Some("completed") => ReplaceState::Done {
                removed: self.proxies_removed.unwrap_or(0),
                added: self.proxies_added.unwrap_or(0),
            },
            Some("failed") => ReplaceState::Failed {
                code: self.error_code.filter(|code| !code.is_empty()),
                message: self
                    .error
                    .filter(|message| !message.is_empty())
                    .map_or_else(
                        || "the provider gave no reason".to_owned(),
                        |message| truncate(&message, 300),
                    ),
            },
            // validating, validated, processing, and anything added later.
            _ => ReplaceState::Pending,
        }
    }
}

#[derive(Deserialize)]
struct ReplacedEntry {
    #[serde(default)]
    reason: Option<String>,
    proxy: String,
    #[serde(default)]
    proxy_port: Option<u16>,
    #[serde(default)]
    replaced_with: Option<String>,
    #[serde(default)]
    replaced_with_port: Option<u16>,
    #[serde(default)]
    replaced_with_country_code: Option<String>,
    created_at: DateTime<Utc>,
}

impl From<ReplacedEntry> for ReplacedProxy {
    fn from(entry: ReplacedEntry) -> Self {
        Self {
            reason: entry.reason.unwrap_or_else(|| "unknown".to_owned()),
            old_host: entry.proxy,
            old_port: entry.proxy_port,
            new_host: entry.replaced_with.filter(|host| !host.is_empty()),
            new_port: entry.replaced_with_port,
            new_country: country(entry.replaced_with_country_code.as_deref()),
            at: entry.created_at,
        }
    }
}

#[derive(Deserialize)]
struct Subscription {
    #[serde(default)]
    plan: Option<Id>,
    #[serde(default)]
    start_date: Option<DateTime<Utc>>,
    #[serde(default)]
    end_date: Option<DateTime<Utc>>,
    #[serde(default)]
    renewals_enabled: Option<bool>,
    #[serde(default)]
    paused: Option<bool>,
    #[serde(default)]
    throttled: Option<bool>,
}

#[derive(Deserialize)]
struct Plan {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    proxy_type: Option<String>,
    #[serde(default)]
    proxy_subtype: Option<String>,
    #[serde(default)]
    proxy_count: Option<u32>,
    #[serde(default)]
    proxy_countries: Option<BTreeMap<String, u32>>,
    #[serde(default)]
    bandwidth_limit: Option<f64>,
    #[serde(default)]
    proxy_replacements_total: Option<u32>,
    #[serde(default)]
    proxy_replacements_used: Option<u32>,
    #[serde(default)]
    proxy_replacements_available: Option<u32>,
}

// The field names are the API's.
#[allow(clippy::struct_field_names)]
#[derive(Deserialize)]
struct ProxyConfig {
    #[serde(default)]
    auto_replace_invalid_proxies: Option<bool>,
    #[serde(default)]
    auto_replace_low_country_confidence_proxies: Option<bool>,
    #[serde(default)]
    auto_replace_out_of_rotation_proxies: Option<bool>,
    #[serde(default)]
    auto_replace_failed_site_check_proxies: Option<bool>,
}

impl ProxyConfig {
    /// The flags, or `None` when the answer names none of them: an answer
    /// without the fields is not evidence that they are off.
    fn auto_replace(&self) -> Option<AutoReplace> {
        let flags = [
            self.auto_replace_invalid_proxies,
            self.auto_replace_low_country_confidence_proxies,
            self.auto_replace_out_of_rotation_proxies,
            self.auto_replace_failed_site_check_proxies,
        ];
        flags.iter().any(Option::is_some).then(|| AutoReplace {
            invalid: flags[0].unwrap_or(false),
            low_country_confidence: flags[1].unwrap_or(false),
            out_of_rotation: flags[2].unwrap_or(false),
            failed_site_check: flags[3].unwrap_or(false),
        })
    }
}

#[derive(Deserialize)]
struct ListStats {
    #[serde(default)]
    available_countries: Option<BTreeMap<String, u32>>,
}

#[derive(Deserialize)]
struct HourlyStats {
    timestamp: DateTime<Utc>,
    #[serde(default)]
    is_projected: bool,
    #[serde(default, deserialize_with = "count")]
    bandwidth_total: u64,
    #[serde(default, deserialize_with = "count")]
    requests_total: u64,
    #[serde(default, deserialize_with = "count")]
    requests_successful: u64,
    #[serde(default, deserialize_with = "count")]
    requests_failed: u64,
    #[serde(default, deserialize_with = "count")]
    number_of_proxies_used: u64,
    #[serde(default)]
    error_reasons: Vec<ErrorReason>,
}

#[derive(Deserialize)]
struct ErrorReason {
    #[serde(default)]
    reason: Option<String>,
    #[serde(default, deserialize_with = "count")]
    count: u64,
}

impl From<HourlyStats> for ProxyActivity {
    fn from(hour: HourlyStats) -> Self {
        let mut errors = BTreeMap::new();
        for error in hour.error_reasons {
            let reason = error.reason.unwrap_or_else(|| "unknown".to_owned());
            *errors.entry(reason).or_insert(0) += error.count;
        }
        Self {
            hour: hour.timestamp,
            requests_total: hour.requests_total,
            requests_successful: hour.requests_successful,
            requests_failed: hour.requests_failed,
            bytes: hour.bandwidth_total,
            proxies_used: u32::try_from(hour.number_of_proxies_used).unwrap_or(u32::MAX),
            errors,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wreq::StatusCode;
    use wreq::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    use super::{
        HourlyStats, Id, ListedProxy, ProxyConfig, Replacement, classify, error_details,
        retry_after,
    };
    use crate::provider::{ProviderError, ProviderProxy, ProxyActivity, ReplaceState};

    #[test]
    fn error_bodies_are_read_in_every_shape_webshare_uses() {
        let cases = [
            (
                json!({ "detail": "Not found." }),
                (None, Some("Not found.")),
            ),
            (
                json!({ "non_field_errors": [
                    { "message": "You are already replacing a proxy.", "code": "active_replacement" }
                ] }),
                (
                    Some("active_replacement"),
                    Some("You are already replacing a proxy."),
                ),
            ),
            (
                json!({ "to_replace": [{ "message": "Your proxy list is not editable", "code": "invalid" }] }),
                (Some("invalid"), Some("Your proxy list is not editable")),
            ),
            (
                json!({ "code": "throttled", "message": "Slow down" }),
                (Some("throttled"), Some("Slow down")),
            ),
            (json!(["Plain message"]), (None, Some("Plain message"))),
            (
                json!({ "a": { "b": [1, null, { "detail": "deep" }] } }),
                (None, Some("deep")),
            ),
            (json!({}), (None, None)),
            (json!({ "detail": "  " }), (None, None)),
            (json!(42), (None, None)),
        ];
        for (body, (code, message)) in cases {
            assert_eq!(
                error_details(&body),
                (code.map(str::to_owned), message.map(str::to_owned)),
                "{body}"
            );
        }
        // A long message is cut, never passed on whole.
        let (_, long) = error_details(&json!({ "detail": "x".repeat(5000) }));
        assert_eq!(long.unwrap().len(), 300);
    }

    #[test]
    fn statuses_become_the_errors_they_stand_for() {
        assert_eq!(
            classify(StatusCode::OK, br#"{"count": 1}"#).unwrap(),
            json!({ "count": 1 })
        );
        assert!(matches!(
            classify(StatusCode::OK, b"<html>"),
            Err(ProviderError::Decode(_))
        ));
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            assert_eq!(
                classify(status, br#"{"detail": "Invalid token."}"#).unwrap_err(),
                ProviderError::Unauthorized {
                    status: status.as_u16()
                }
            );
        }
        assert_eq!(
            classify(StatusCode::BAD_REQUEST, br#"{"detail": "Bad plan"}"#).unwrap_err(),
            ProviderError::Status {
                status: 400,
                code: None,
                message: "Bad plan".to_owned(),
            }
        );
        // No usable body: the status text stands in.
        assert_eq!(
            classify(StatusCode::BAD_GATEWAY, b"<html>upstream</html>").unwrap_err(),
            ProviderError::Status {
                status: 502,
                code: None,
                message: "Bad Gateway".to_owned(),
            }
        );
    }

    #[test]
    fn retry_after_is_read_in_seconds() {
        let with = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(RETRY_AFTER, HeaderValue::from_static(value));
            retry_after(&headers)
        };

        assert_eq!(with("60"), Some(std::time::Duration::from_mins(1)));
        assert_eq!(with(" 5 "), Some(std::time::Duration::from_secs(5)));
        assert_eq!(with("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(with("-1"), None);
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn a_listed_proxy_becomes_a_provider_proxy() {
        let listed: ListedProxy = serde_json::from_value(json!({
            "id": "d-10513",
            "username": "user",
            "password": "pass",
            "proxy_address": "198.51.100.10",
            "port": 8168,
            "valid": true,
            "last_verification": "2019-06-09T23:34:00.095501-07:00",
            "country_code": "de",
            "city_name": "Berlin",
            "asn_number": 3320,
            "created_at": "2022-06-14T11:58:10.246406-07:00"
        }))
        .unwrap();

        let proxy = ProviderProxy::try_from(listed).unwrap();

        assert_eq!(proxy.provider_ref, "d-10513");
        assert_eq!(proxy.host, "198.51.100.10");
        assert_eq!(proxy.port, 8168);
        assert_eq!(proxy.username.expose(), "user");
        assert_eq!(proxy.password.expose(), "pass");
        assert_eq!(proxy.country.unwrap().as_str(), "DE");
        assert_eq!(proxy.city.as_deref(), Some("Berlin"));
        assert_eq!(proxy.asn.as_deref(), Some("AS3320"));
        assert_eq!(proxy.valid, Some(true));
    }

    #[test]
    fn a_proxy_without_an_address_is_refused_with_the_reason() {
        for address in [json!(null), json!(""), json!("  ")] {
            let listed: ListedProxy = serde_json::from_value(json!({
                "id": 7, "username": "user", "password": "pass",
                "proxy_address": address, "port": 80, "country_code": "ZZZ", "city_name": ""
            }))
            .unwrap();

            let error = ProviderProxy::try_from(listed).unwrap_err();

            assert!(error.to_string().contains("direct mode"), "{error}");
        }
        // The minimum the API can send: ids as numbers, nothing optional.
        let minimal: ListedProxy = serde_json::from_value(json!({
            "id": 7, "username": "u", "password": "p", "proxy_address": "198.51.100.1", "port": 80
        }))
        .unwrap();
        let proxy = ProviderProxy::try_from(minimal).unwrap();
        assert_eq!(proxy.provider_ref, "7");
        assert_eq!(proxy.country, None);
        assert_eq!(proxy.city, None);
        assert_eq!(proxy.asn, None);
        assert_eq!(proxy.valid, None);
    }

    #[test]
    fn replacement_states_map_onto_pending_done_and_failed() {
        let state =
            |body: serde_json::Value| serde_json::from_value::<Replacement>(body).unwrap().state();

        for pending in ["validating", "validated", "processing", "something_new"] {
            assert_eq!(
                state(json!({ "id": 98315, "state": pending })),
                ReplaceState::Pending
            );
        }
        assert_eq!(state(json!({ "id": 1 })), ReplaceState::Pending);
        assert_eq!(
            state(
                json!({ "id": 98315, "state": "completed", "proxies_removed": 1, "proxies_added": 1 })
            ),
            ReplaceState::Done {
                removed: 1,
                added: 1
            }
        );
        assert_eq!(
            state(
                json!({ "id": 98315, "state": "failed", "error": "No proxies available", "error_code": "no_proxies" })
            ),
            ReplaceState::Failed {
                code: Some("no_proxies".to_owned()),
                message: "No proxies available".to_owned(),
            }
        );
        assert_eq!(
            state(json!({ "id": 98315, "state": "failed", "error": "", "error_code": "" })),
            ReplaceState::Failed {
                code: None,
                message: "the provider gave no reason".to_owned(),
            }
        );
        assert_eq!(Id::Number(98315).into_string(), "98315");
        assert_eq!(Id::Text("d-1".to_owned()).into_string(), "d-1");
    }

    #[test]
    fn auto_replace_flags_are_unknown_when_the_answer_names_none() {
        let none: ProxyConfig =
            serde_json::from_value(json!({ "request_timeout": 86400 })).unwrap();
        assert_eq!(none.auto_replace(), None);

        let some: ProxyConfig = serde_json::from_value(json!({
            "auto_replace_invalid_proxies": true,
            "auto_replace_out_of_rotation_proxies": false
        }))
        .unwrap();
        let flags = some.auto_replace().unwrap();
        assert!(flags.invalid);
        assert!(!flags.out_of_rotation);
        assert!(!flags.low_country_confidence);
        assert!(!flags.failed_site_check);
    }

    #[test]
    fn hourly_stats_tolerate_floats_nulls_and_missing_fields() {
        let hour: HourlyStats = serde_json::from_value(json!({
            "timestamp": "2022-08-11T17:00:00-07:00",
            "is_projected": false,
            "bandwidth_total": 5000.0,
            "bandwidth_average": 1000,
            "requests_total": 5,
            "requests_successful": 4,
            "requests_failed": null,
            "error_reasons": [
                { "reason": "client_connect_forbidden_host", "type": "configuration", "count": 1 },
                { "reason": "client_connect_forbidden_host", "count": 2 },
                { "count": 3 }
            ],
            "number_of_proxies_used": 2
        }))
        .unwrap();

        let activity = ProxyActivity::from(hour);

        assert_eq!(activity.hour.to_rfc3339(), "2022-08-12T00:00:00+00:00");
        assert_eq!(activity.bytes, 5000);
        assert_eq!(activity.requests_total, 5);
        assert_eq!(activity.requests_successful, 4);
        assert_eq!(activity.requests_failed, 0);
        assert_eq!(activity.proxies_used, 2);
        assert_eq!(activity.errors["client_connect_forbidden_host"], 3);
        assert_eq!(activity.errors["unknown"], 3);

        assert!(
            serde_json::from_value::<HourlyStats>(
                json!({ "timestamp": "2022-08-11T17:00:00Z", "requests_total": -1 })
            )
            .is_err()
        );
    }
}
