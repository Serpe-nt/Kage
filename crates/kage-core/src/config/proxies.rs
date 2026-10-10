//! Configuration of the proxy pool (spec §7): the provider, the Webshare
//! API client, the pool state machine, the health checks and the reputation
//! lookups.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::domain::CountryCode;
use crate::duration::HumanDuration;
use crate::macros::string_enum;
use crate::secret::Secret;

/// Webshare's documented limit for its general API, requests per minute.
pub const WEBSHARE_GENERAL_LIMIT_PER_MINUTE: u32 = 240;

/// Webshare's documented limit for the proxy list endpoints, requests per
/// minute.
pub const WEBSHARE_LIST_LIMIT_PER_MINUTE: u32 = 60;

/// The Webshare account and its API client (spec §7.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebshareConfig {
    /// API token. Secret: `KAGE__WEBSHARE__API_KEY`.
    #[serde(default)]
    pub api_key: Option<Secret>,
    /// Root of the API. The versioned paths (`v2/…`, `v3/…`) are appended.
    #[serde(default = "default_webshare_base_url")]
    pub base_url: String,
    /// The plan to manage. `null` uses the plan of the active subscription.
    #[serde(default)]
    pub plan_id: Option<u64>,
    /// Proxies per page of the list endpoints.
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    /// Kage's own ceiling for the general API. Webshare allows 240.
    #[serde(default = "default_requests_per_minute")]
    pub requests_per_minute: u32,
    /// Kage's own ceiling for the proxy list endpoints. Webshare allows 60.
    #[serde(default = "default_list_requests_per_minute")]
    pub list_requests_per_minute: u32,
    /// Timeout of one API request.
    #[serde(default = "default_api_timeout")]
    pub timeout: HumanDuration,
    /// How long to stay away after a 429 that names no `Retry-After`.
    #[serde(default = "default_rate_limit_backoff")]
    pub rate_limit_backoff: HumanDuration,
    /// Extra tries after a 429 or a failed read. A request that changes
    /// something is never sent twice.
    #[serde(default = "default_api_retries")]
    pub max_retries: u32,
    /// The account's automatic replacement settings the operator leaves on
    /// on purpose (decision D31). One of them being on is recorded as an
    /// informational event instead of the alert
    /// `provider_auto_replace_enabled`, and a proxy Webshare replaced for
    /// that reason is retired without the alert `provider_replaced_proxy`:
    /// its persona is retired and a new one generated either way. Names
    /// are Webshare's own, see [`WEBSHARE_AUTO_REPLACE_SETTINGS`].
    #[serde(default = "default_accepted_auto_replace")]
    pub accepted_auto_replace: Vec<String>,
}

/// The four settings of a Webshare account that replace proxies without
/// being asked.
pub const WEBSHARE_AUTO_REPLACE_SETTINGS: [&str; 4] = [
    "auto_replace_invalid_proxies",
    "auto_replace_low_country_confidence_proxies",
    "auto_replace_out_of_rotation_proxies",
    "auto_replace_failed_site_check_proxies",
];

fn default_accepted_auto_replace() -> Vec<String> {
    // Webshare replaces only a proxy its own checks find dead, which Kage
    // would have replaced anyway after its backoff.
    vec!["auto_replace_invalid_proxies".to_owned()]
}

fn default_webshare_base_url() -> String {
    "https://proxy.webshare.io/api/".to_owned()
}

const fn default_page_size() -> u32 {
    100
}

const fn default_requests_per_minute() -> u32 {
    120
}

const fn default_list_requests_per_minute() -> u32 {
    30
}

const fn default_api_timeout() -> HumanDuration {
    HumanDuration::from_secs(30)
}

const fn default_rate_limit_backoff() -> HumanDuration {
    HumanDuration::from_secs(60)
}

const fn default_api_retries() -> u32 {
    2
}

impl Default for WebshareConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: default_webshare_base_url(),
            plan_id: None,
            page_size: default_page_size(),
            requests_per_minute: default_requests_per_minute(),
            list_requests_per_minute: default_list_requests_per_minute(),
            timeout: default_api_timeout(),
            rate_limit_backoff: default_rate_limit_backoff(),
            max_retries: default_api_retries(),
            accepted_auto_replace: default_accepted_auto_replace(),
        }
    }
}

/// IP reputation lookups (spec §7.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReputationConfig {
    /// AbuseIPDB key; lookups are disabled without it. Secret:
    /// `KAGE__REPUTATION__ABUSEIPDB_KEY`.
    #[serde(default)]
    pub abuseipdb_key: Option<Secret>,
    /// Root of the AbuseIPDB API.
    #[serde(default = "default_abuseipdb_base_url")]
    pub base_url: String,
    /// How often each proxy's address is looked up.
    #[serde(default = "default_reputation_interval")]
    pub interval: HumanDuration,
    /// Abuse confidence score, 0 to 100, at or above which an address is bad.
    #[serde(default = "default_bad_score")]
    pub bad_score: u8,
    /// Lookups in a row that must say bad before the proxy is replaced.
    #[serde(default = "default_bad_lookups")]
    pub bad_lookups: u32,
    /// How far back reports count, in days.
    #[serde(default = "default_max_age_days")]
    pub max_age_days: u16,
}

fn default_abuseipdb_base_url() -> String {
    "https://api.abuseipdb.com/api/v2/".to_owned()
}

const fn default_reputation_interval() -> HumanDuration {
    HumanDuration::from_hours(24)
}

const fn default_bad_score() -> u8 {
    75
}

const fn default_bad_lookups() -> u32 {
    2
}

const fn default_max_age_days() -> u16 {
    90
}

impl Default for ReputationConfig {
    fn default() -> Self {
        Self {
            abuseipdb_key: None,
            base_url: default_abuseipdb_base_url(),
            interval: default_reputation_interval(),
            bad_score: default_bad_score(),
            bad_lookups: default_bad_lookups(),
            max_age_days: default_max_age_days(),
        }
    }
}

string_enum! {
    /// Where the proxy list comes from.
    pub enum ProviderChoice {
        /// Webshare when its API key is set, else the static list when one
        /// is set, else no provider.
        Auto => "auto",
        /// The Webshare API. Needs `KAGE__WEBSHARE__API_KEY`.
        Webshare => "webshare",
        /// The static list. Needs `KAGE__PROXIES__STATIC_LIST`.
        Static => "static",
        /// No provider: the pool stays empty and every fetch fails, because
        /// Kage never connects to a target directly.
        None => "none",
    }
}

string_enum! {
    /// The provider the configuration resolves to.
    pub enum ResolvedProvider {
        /// The Webshare API.
        Webshare => "webshare",
        /// The static list.
        Static => "static",
        /// No provider.
        None => "none",
    }
}

/// The health checks of the pool (spec §7.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyHealthConfig {
    /// Time between two probes of one proxy, before jitter.
    #[serde(default = "default_health_interval")]
    pub interval: HumanDuration,
    /// Each wait is the interval times a factor drawn from
    /// `1 - jitter ..= 1 + jitter`, per proxy, so probes never run in step.
    #[serde(default = "default_health_jitter")]
    pub jitter: f64,
    /// Timeout of one probe request.
    #[serde(default = "default_health_timeout")]
    pub timeout: HumanDuration,
    /// Most probes in flight at once.
    #[serde(default = "default_health_concurrency")]
    pub max_concurrent: u32,
    /// The first probes after start are spread over this long.
    #[serde(default = "default_startup_spread")]
    pub startup_spread: HumanDuration,
    /// Echo read through every proxy for reachability, latency, the exit
    /// address and the leak check. The default belongs to the proxy
    /// provider, who sees every connection anyway.
    #[serde(default = "default_echo_url")]
    pub echo_url: String,
    /// Second echo, run by someone else, tried once when the first fails
    /// before the failure counts. `null` disables the confirmation.
    #[serde(default = "default_confirm_url")]
    pub confirm_url: Option<String>,
    /// Geo lookup of the exit address: on the first check of a new proxy,
    /// when the exit address changes, and once per `geo_interval`. `null`
    /// disables the country check.
    #[serde(default = "default_geo_url")]
    pub geo_url: Option<String>,
    /// Time between two geo lookups of one proxy, before jitter.
    #[serde(default = "default_geo_interval")]
    pub geo_interval: HumanDuration,
    /// Lookups in a row that must disagree with the provider's country
    /// before the proxy is marked degraded.
    #[serde(default = "default_geo_mismatches")]
    pub geo_mismatches: u32,
    /// Echo read without a proxy to learn the host's own public address,
    /// which the leak check compares against.
    #[serde(default = "default_echo_url")]
    pub host_ip_url: String,
    /// How often the host address is read again.
    #[serde(default = "default_host_ip_refresh")]
    pub host_ip_refresh: HumanDuration,
    /// wreq-util emulation profile of the probe client until personas
    /// supply their own (milestone M6).
    #[serde(default = "default_probe_profile")]
    pub profile: String,
    /// Platform the probe client claims: windows, macos or linux.
    #[serde(default = "default_probe_platform")]
    pub platform: String,
}

const fn default_health_interval() -> HumanDuration {
    HumanDuration::from_mins(5)
}

const fn default_health_jitter() -> f64 {
    0.25
}

const fn default_health_timeout() -> HumanDuration {
    HumanDuration::from_secs(20)
}

const fn default_health_concurrency() -> u32 {
    3
}

const fn default_startup_spread() -> HumanDuration {
    HumanDuration::from_mins(2)
}

fn default_echo_url() -> String {
    "https://ipv4.webshare.io/".to_owned()
}

#[allow(clippy::unnecessary_wraps)]
fn default_confirm_url() -> Option<String> {
    Some("https://checkip.amazonaws.com/".to_owned())
}

#[allow(clippy::unnecessary_wraps)]
fn default_geo_url() -> Option<String> {
    Some("https://ipinfo.io/json".to_owned())
}

const fn default_geo_interval() -> HumanDuration {
    HumanDuration::from_hours(24)
}

const fn default_geo_mismatches() -> u32 {
    2
}

const fn default_host_ip_refresh() -> HumanDuration {
    HumanDuration::from_hours(1)
}

fn default_probe_profile() -> String {
    "chrome_149".to_owned()
}

fn default_probe_platform() -> String {
    "windows".to_owned()
}

impl Default for ProxyHealthConfig {
    fn default() -> Self {
        Self {
            interval: default_health_interval(),
            jitter: default_health_jitter(),
            timeout: default_health_timeout(),
            max_concurrent: default_health_concurrency(),
            startup_spread: default_startup_spread(),
            echo_url: default_echo_url(),
            confirm_url: default_confirm_url(),
            geo_url: default_geo_url(),
            geo_interval: default_geo_interval(),
            geo_mismatches: default_geo_mismatches(),
            host_ip_url: default_echo_url(),
            host_ip_refresh: default_host_ip_refresh(),
            profile: default_probe_profile(),
            platform: default_probe_platform(),
        }
    }
}

/// The proxy pool (spec §7.2, §7.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxiesConfig {
    /// Where the proxy list comes from.
    #[serde(default = "default_provider")]
    pub provider: ProviderChoice,
    /// A fixed proxy list, for tests and for vendors without an API. Secret:
    /// `KAGE__PROXIES__STATIC_LIST`. See [`StaticProxy::parse_list`] for the
    /// two formats.
    #[serde(default)]
    pub static_list: Option<Secret>,
    /// How often the pool is reconciled with the provider's list and the
    /// account is read.
    #[serde(default = "default_sync_interval")]
    pub sync_interval: HumanDuration,
    /// Syncs in a row a proxy must be missing from the provider's list
    /// before it is retired, unless the provider itself reports it replaced.
    #[serde(default = "default_missing_syncs")]
    pub missing_syncs: u32,
    /// Replacements Kage may request per budget window: the provider's
    /// subscription term when the account reports one, else the UTC
    /// calendar month.
    #[serde(default = "default_replacements_per_month")]
    pub replacements_per_month: u32,
    /// Failed checks in a row that send a proxy into backoff (N).
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,
    /// First backoff; it doubles with every cycle in a row.
    #[serde(default = "default_backoff_base")]
    pub backoff_base: HumanDuration,
    /// Longest backoff.
    #[serde(default = "default_backoff_cap")]
    pub backoff_cap: HumanDuration,
    /// Backoff cycles in a row before a replacement is considered (M).
    #[serde(default = "default_backoff_cycles")]
    pub backoff_cycles: u32,
    /// How long a proxy must have been failing without a break before it is
    /// replaced. Both this and `backoff_cycles` must be reached.
    #[serde(default = "default_replace_after")]
    pub replace_after: HumanDuration,
    /// The health checks.
    #[serde(default)]
    pub health: ProxyHealthConfig,
}

const fn default_provider() -> ProviderChoice {
    ProviderChoice::Auto
}

const fn default_sync_interval() -> HumanDuration {
    HumanDuration::from_mins(10)
}

const fn default_missing_syncs() -> u32 {
    3
}

const fn default_replacements_per_month() -> u32 {
    5
}

const fn default_failure_threshold() -> u32 {
    3
}

const fn default_backoff_base() -> HumanDuration {
    HumanDuration::from_mins(1)
}

const fn default_backoff_cap() -> HumanDuration {
    HumanDuration::from_hours(1)
}

const fn default_backoff_cycles() -> u32 {
    3
}

const fn default_replace_after() -> HumanDuration {
    HumanDuration::from_hours(6)
}

impl Default for ProxiesConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            static_list: None,
            sync_interval: default_sync_interval(),
            missing_syncs: default_missing_syncs(),
            replacements_per_month: default_replacements_per_month(),
            failure_threshold: default_failure_threshold(),
            backoff_base: default_backoff_base(),
            backoff_cap: default_backoff_cap(),
            backoff_cycles: default_backoff_cycles(),
            replace_after: default_replace_after(),
            health: ProxyHealthConfig::default(),
        }
    }
}

/// One proxy of a static list.
#[derive(Debug, Clone)]
pub struct StaticProxy {
    /// A stable name for the proxy; `host:port` when the list gives none.
    pub id: String,
    /// Proxy host name or address.
    pub host: String,
    /// Proxy port.
    pub port: u16,
    /// Proxy user name.
    pub username: Secret,
    /// Proxy password.
    pub password: Secret,
    /// Country of the exit address, when known.
    pub country: Option<CountryCode>,
    /// City of the exit address, when known.
    pub city: Option<String>,
    /// Autonomous system of the exit address, when known.
    pub asn: Option<String>,
}

/// The YAML form of a static list.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StaticListDocument {
    proxies: Vec<StaticProxyEntry>,
}

/// The YAML form of one static proxy.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StaticProxyEntry {
    #[serde(default)]
    id: Option<String>,
    host: String,
    port: u16,
    username: String,
    password: String,
    #[serde(default)]
    country: Option<CountryCode>,
    #[serde(default)]
    city: Option<String>,
    #[serde(default)]
    asn: Option<String>,
}

/// Why a static proxy list was rejected. The message names the entry, never
/// its content: the list holds credentials.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("static proxy list: {0}")]
pub struct StaticListError(String);

impl StaticProxy {
    /// Parses a static proxy list in one of two formats.
    ///
    /// One proxy per line, the format of Webshare's own list download.
    /// Blank lines and lines starting with `#` are skipped:
    ///
    /// ```text
    /// 198.51.100.10:8080:username:password
    /// 198.51.100.11:8080:username:password:DE
    /// ```
    ///
    /// Or a YAML document with a `proxies` list, which also covers passwords
    /// and hosts that contain a colon:
    ///
    /// ```yaml
    /// proxies:
    ///   - host: 198.51.100.10
    ///     port: 8080
    ///     username: user
    ///     password: "pa:ss"
    ///     country: DE
    ///     city: Berlin
    /// ```
    ///
    /// # Errors
    ///
    /// Fails on a malformed entry, a port of 0, an empty host or user name,
    /// or two entries with the same id.
    pub fn parse_list(text: &str) -> Result<Vec<Self>, StaticListError> {
        let proxies = if text.trim_start().starts_with("proxies:") {
            Self::parse_yaml(text)?
        } else {
            Self::parse_lines(text)?
        };
        for (index, proxy) in proxies.iter().enumerate() {
            if proxies[..index].iter().any(|other| other.id == proxy.id) {
                return Err(StaticListError(format!(
                    "entry {} repeats the id of an earlier entry",
                    index + 1
                )));
            }
        }
        Ok(proxies)
    }

    fn parse_yaml(text: &str) -> Result<Vec<Self>, StaticListError> {
        // The parser's message can quote the document, which holds
        // passwords, so it is not passed on.
        let document: StaticListDocument = crate::yaml::from_str(text).map_err(|_| {
            StaticListError(
                "the YAML form must be `proxies:` with a list of entries that have host, \
                 port, username and password, optionally id, country, city and asn"
                    .to_owned(),
            )
        })?;
        document
            .proxies
            .into_iter()
            .enumerate()
            .map(|(index, entry)| {
                Self::checked(
                    index + 1,
                    entry.id,
                    &entry.host,
                    entry.port,
                    entry.username,
                    entry.password,
                    entry.country,
                    entry.city,
                    entry.asn,
                )
            })
            .collect()
    }

    fn parse_lines(text: &str) -> Result<Vec<Self>, StaticListError> {
        let mut proxies = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split(':').collect();
            let (host, port, username, password, country) = match fields.as_slice() {
                [host, port, username, password] => (*host, *port, *username, *password, None),
                [host, port, username, password, country] => {
                    (*host, *port, *username, *password, Some(*country))
                }
                _ => {
                    return Err(StaticListError(format!(
                        "line {number}: expected host:port:username:password[:country]"
                    )));
                }
            };
            let port: u16 = port
                .parse()
                .map_err(|_| StaticListError(format!("line {number}: the port is not a number")))?;
            let country = country
                .map(|code| {
                    CountryCode::new(code).map_err(|_| {
                        StaticListError(format!(
                            "line {number}: the country is not a two-letter code"
                        ))
                    })
                })
                .transpose()?;
            proxies.push(Self::checked(
                number,
                None,
                host,
                port,
                username.to_owned(),
                password.to_owned(),
                country,
                None,
                None,
            )?);
        }
        Ok(proxies)
    }

    #[allow(clippy::too_many_arguments)]
    fn checked(
        position: usize,
        id: Option<String>,
        host: &str,
        port: u16,
        username: String,
        password: String,
        country: Option<CountryCode>,
        city: Option<String>,
        asn: Option<String>,
    ) -> Result<Self, StaticListError> {
        let problem = |what: &str| StaticListError(format!("entry {position}: {what}"));
        let host = host.trim().to_owned();
        if host.is_empty() || host.contains(char::is_whitespace) || host.contains('/') {
            return Err(problem("the host is empty or not a host name"));
        }
        if port == 0 {
            return Err(problem("the port must not be 0"));
        }
        if username.is_empty() {
            return Err(problem("the user name is empty"));
        }
        let id = match id {
            Some(id) if id.trim().is_empty() => return Err(problem("the id is empty")),
            Some(id) => id.trim().to_owned(),
            None => format!("{host}:{port}"),
        };
        Ok(Self {
            id,
            host,
            port,
            username: Secret::new(username),
            password: Secret::new(password),
            country,
            city: city.filter(|city| !city.trim().is_empty()),
            asn: asn.filter(|asn| !asn.trim().is_empty()),
        })
    }
}

/// Checks the pool sections; returns the first problem as a message.
pub(super) fn validate(
    webshare: &WebshareConfig,
    reputation: &ReputationConfig,
    proxies: &ProxiesConfig,
) -> Result<(), String> {
    let fail = |message: &str| Err(message.to_owned());
    let second = Duration::from_secs(1);

    http_url("webshare.base_url", &webshare.base_url, true)?;
    if !(1..=100).contains(&webshare.page_size) {
        return fail("webshare.page_size must be between 1 and 100");
    }
    if !(1..WEBSHARE_GENERAL_LIMIT_PER_MINUTE).contains(&webshare.requests_per_minute) {
        return fail(
            "webshare.requests_per_minute must be between 1 and 239, below Webshare's limit of 240",
        );
    }
    if !(1..WEBSHARE_LIST_LIMIT_PER_MINUTE).contains(&webshare.list_requests_per_minute) {
        return fail(
            "webshare.list_requests_per_minute must be between 1 and 59, below Webshare's limit of 60",
        );
    }
    if webshare.timeout.get() < second {
        return fail("webshare.timeout must be at least 1s");
    }
    if webshare.max_retries > 10 {
        return fail("webshare.max_retries must be at most 10");
    }
    for (index, name) in webshare.accepted_auto_replace.iter().enumerate() {
        if !WEBSHARE_AUTO_REPLACE_SETTINGS.contains(&name.as_str()) {
            return Err(format!(
                "webshare.accepted_auto_replace: {name:?} is not one of {}",
                WEBSHARE_AUTO_REPLACE_SETTINGS.join(", ")
            ));
        }
        if webshare.accepted_auto_replace[..index].contains(name) {
            return Err(format!(
                "webshare.accepted_auto_replace names {name} more than once"
            ));
        }
    }

    http_url("reputation.base_url", &reputation.base_url, true)?;
    if reputation.interval.get() < Duration::from_hours(1) {
        return fail("reputation.interval must be at least 1h");
    }
    if !(1..=100).contains(&reputation.bad_score) {
        return fail("reputation.bad_score must be between 1 and 100");
    }
    if reputation.bad_lookups == 0 {
        return fail("reputation.bad_lookups must be at least 1");
    }
    if !(1..=365).contains(&reputation.max_age_days) {
        return fail("reputation.max_age_days must be between 1 and 365");
    }

    if proxies.sync_interval.get() < second {
        return fail("proxies.sync_interval must be at least 1s");
    }
    if proxies.missing_syncs == 0 {
        return fail("proxies.missing_syncs must be at least 1");
    }
    if proxies.failure_threshold == 0 {
        return fail("proxies.failure_threshold must be at least 1");
    }
    if proxies.backoff_base.get() < second {
        return fail("proxies.backoff_base must be at least 1s");
    }
    if proxies.backoff_cap.get() < proxies.backoff_base.get() {
        return fail("proxies.backoff_cap must not be shorter than proxies.backoff_base");
    }
    if proxies.backoff_cycles == 0 {
        return fail("proxies.backoff_cycles must be at least 1");
    }

    let health = &proxies.health;
    if health.interval.get() < second {
        return fail("proxies.health.interval must be at least 1s");
    }
    if !(0.0..=0.9).contains(&health.jitter) {
        return fail("proxies.health.jitter must be between 0 and 0.9");
    }
    if health.timeout.get() < second {
        return fail("proxies.health.timeout must be at least 1s");
    }
    if health.max_concurrent == 0 {
        return fail("proxies.health.max_concurrent must be at least 1");
    }
    http_url("proxies.health.echo_url", &health.echo_url, false)?;
    if let Some(url) = &health.confirm_url {
        http_url("proxies.health.confirm_url", url, false)?;
    }
    if let Some(url) = &health.geo_url {
        http_url("proxies.health.geo_url", url, false)?;
    }
    if health.geo_interval.get() < Duration::from_mins(1) {
        return fail("proxies.health.geo_interval must be at least 1m");
    }
    if health.geo_mismatches == 0 {
        return fail("proxies.health.geo_mismatches must be at least 1");
    }
    http_url("proxies.health.host_ip_url", &health.host_ip_url, false)?;
    if health.host_ip_refresh.get() < second {
        return fail("proxies.health.host_ip_refresh must be at least 1s");
    }
    if health.profile.trim().is_empty() {
        return fail("proxies.health.profile must not be empty");
    }
    if !["windows", "macos", "linux"].contains(&health.platform.as_str()) {
        return fail("proxies.health.platform must be windows, macos or linux");
    }

    if let Some(list) = &proxies.static_list {
        StaticProxy::parse_list(list.expose()).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// An absolute http(s) URL with a host; API roots must end with a slash so
/// that paths can be appended.
fn http_url(key: &str, value: &str, root: bool) -> Result<(), String> {
    let parsed =
        url::Url::parse(value).map_err(|_| format!("{key} must be an absolute http(s) URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(format!("{key} must be an absolute http(s) URL"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(format!("{key} must not carry credentials"));
    }
    if root && !value.ends_with('/') {
        return Err(format!("{key} must end with a slash"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ProviderChoice, ProxiesConfig, ReputationConfig, ResolvedProvider, StaticProxy,
        WebshareConfig, validate,
    };
    use crate::duration::HumanDuration;
    use crate::macros::test_support::assert_string_enum;
    use crate::secret::Secret;

    #[test]
    fn enum_names_are_stable() {
        assert_string_enum(
            ProviderChoice::ALL,
            ProviderChoice::as_str,
            &["auto", "webshare", "static", "none"],
        );
        assert_string_enum(
            ResolvedProvider::ALL,
            ResolvedProvider::as_str,
            &["webshare", "static", "none"],
        );
    }

    #[test]
    fn the_defaults_are_the_spec_defaults_and_validate() {
        let webshare = WebshareConfig::default();
        let reputation = ReputationConfig::default();
        let proxies = ProxiesConfig::default();

        assert_eq!(validate(&webshare, &reputation, &proxies), Ok(()));
        // §7.2: below 240 and 60 per minute.
        assert!(webshare.requests_per_minute < 240);
        assert!(webshare.list_requests_per_minute < 60);
        assert_eq!(webshare.base_url, "https://proxy.webshare.io/api/");
        // §7.3: N = 3, 1 min doubling to 1 h, M = 3, five a month.
        assert_eq!(proxies.failure_threshold, 3);
        assert_eq!(proxies.backoff_base, HumanDuration::from_mins(1));
        assert_eq!(proxies.backoff_cap, HumanDuration::from_hours(1));
        assert_eq!(proxies.backoff_cycles, 3);
        assert_eq!(proxies.replacements_per_month, 5);
        assert_eq!(proxies.replace_after, HumanDuration::from_hours(6));
        assert_eq!(proxies.sync_interval, HumanDuration::from_mins(10));
        // §7.4: five minutes jittered, reputation daily.
        assert_eq!(proxies.health.interval, HumanDuration::from_mins(5));
        assert_eq!(proxies.health.max_concurrent, 3);
        assert_eq!(proxies.health.profile, "chrome_149");
        assert_eq!(proxies.health.platform, "windows");
        assert_eq!(reputation.interval, HumanDuration::from_hours(24));
        assert_eq!(reputation.bad_score, 75);
        assert_eq!(reputation.bad_lookups, 2);
    }

    // One table of every bound: long, and better read in one piece.
    #[allow(clippy::too_many_lines)]
    #[test]
    fn out_of_range_values_are_rejected_by_name() {
        type Change = fn(&mut WebshareConfig, &mut ReputationConfig, &mut ProxiesConfig);
        let cases: &[(&str, Change)] = &[
            ("webshare.base_url", |w, _, _| {
                w.base_url = "ftp://x/".to_owned();
            }),
            ("webshare.base_url must end", |w, _, _| {
                w.base_url = "https://proxy.webshare.io/api".to_owned();
            }),
            ("webshare.base_url must not carry", |w, _, _| {
                w.base_url = "https://user:pw@proxy.webshare.io/api/".to_owned();
            }),
            ("webshare.page_size", |w, _, _| w.page_size = 0),
            ("webshare.page_size", |w, _, _| w.page_size = 101),
            ("webshare.requests_per_minute", |w, _, _| {
                w.requests_per_minute = 240;
            }),
            ("webshare.requests_per_minute", |w, _, _| {
                w.requests_per_minute = 0;
            }),
            ("webshare.list_requests_per_minute", |w, _, _| {
                w.list_requests_per_minute = 60;
            }),
            ("webshare.timeout", |w, _, _| {
                w.timeout = HumanDuration::from_secs(0);
            }),
            ("webshare.max_retries", |w, _, _| w.max_retries = 11),
            (
                "webshare.accepted_auto_replace: \"auto_replace_everything\"",
                |w, _, _| {
                    w.accepted_auto_replace = vec!["auto_replace_everything".to_owned()];
                },
            ),
            ("webshare.accepted_auto_replace names", |w, _, _| {
                w.accepted_auto_replace = vec![
                    "auto_replace_invalid_proxies".to_owned(),
                    "auto_replace_invalid_proxies".to_owned(),
                ];
            }),
            ("reputation.base_url", |_, r, _| {
                r.base_url = "not a url".to_owned();
            }),
            ("reputation.interval", |_, r, _| {
                r.interval = HumanDuration::from_mins(5);
            }),
            ("reputation.bad_score", |_, r, _| r.bad_score = 0),
            ("reputation.bad_score", |_, r, _| r.bad_score = 101),
            ("reputation.bad_lookups", |_, r, _| r.bad_lookups = 0),
            ("reputation.max_age_days", |_, r, _| r.max_age_days = 0),
            ("reputation.max_age_days", |_, r, _| r.max_age_days = 366),
            ("proxies.sync_interval", |_, _, p| {
                p.sync_interval = HumanDuration::from_secs(0);
            }),
            ("proxies.missing_syncs", |_, _, p| p.missing_syncs = 0),
            ("proxies.failure_threshold", |_, _, p| {
                p.failure_threshold = 0;
            }),
            ("proxies.backoff_base", |_, _, p| {
                p.backoff_base = HumanDuration::from_secs(0);
            }),
            ("proxies.backoff_cap", |_, _, p| {
                p.backoff_cap = HumanDuration::from_secs(30);
            }),
            ("proxies.backoff_cycles", |_, _, p| p.backoff_cycles = 0),
            ("proxies.health.interval", |_, _, p| {
                p.health.interval = HumanDuration::from_secs(0);
            }),
            ("proxies.health.jitter", |_, _, p| p.health.jitter = 1.0),
            ("proxies.health.jitter", |_, _, p| p.health.jitter = -0.1),
            ("proxies.health.jitter", |_, _, p| {
                p.health.jitter = f64::NAN;
            }),
            ("proxies.health.timeout", |_, _, p| {
                p.health.timeout = HumanDuration::from_secs(0);
            }),
            ("proxies.health.max_concurrent", |_, _, p| {
                p.health.max_concurrent = 0;
            }),
            ("proxies.health.echo_url", |_, _, p| {
                p.health.echo_url = "ipv4.webshare.io".to_owned();
            }),
            ("proxies.health.confirm_url", |_, _, p| {
                p.health.confirm_url = Some("file:///etc/passwd".to_owned());
            }),
            ("proxies.health.geo_url", |_, _, p| {
                p.health.geo_url = Some(String::new());
            }),
            ("proxies.health.geo_interval", |_, _, p| {
                p.health.geo_interval = HumanDuration::from_secs(5);
            }),
            ("proxies.health.geo_mismatches", |_, _, p| {
                p.health.geo_mismatches = 0;
            }),
            ("proxies.health.host_ip_url", |_, _, p| {
                p.health.host_ip_url = "http://".to_owned();
            }),
            ("proxies.health.host_ip_refresh", |_, _, p| {
                p.health.host_ip_refresh = HumanDuration::from_secs(0);
            }),
            ("proxies.health.profile", |_, _, p| {
                p.health.profile = " ".to_owned();
            }),
            ("proxies.health.platform", |_, _, p| {
                p.health.platform = "amiga".to_owned();
            }),
            ("static proxy list", |_, _, p| {
                p.static_list = Some(Secret::new("not-a-proxy-line"));
            }),
        ];
        for (needle, change) in cases {
            let mut webshare = WebshareConfig::default();
            let mut reputation = ReputationConfig::default();
            let mut proxies = ProxiesConfig::default();
            change(&mut webshare, &mut reputation, &mut proxies);

            let message = validate(&webshare, &reputation, &proxies).unwrap_err();

            assert!(message.contains(needle), "{needle}: {message}");
        }
    }

    #[test]
    fn the_optional_endpoints_can_be_switched_off() {
        let mut proxies = ProxiesConfig::default();
        proxies.health.confirm_url = None;
        proxies.health.geo_url = None;

        assert_eq!(
            validate(
                &WebshareConfig::default(),
                &ReputationConfig::default(),
                &proxies
            ),
            Ok(())
        );
    }

    #[test]
    fn the_line_format_is_the_one_webshare_downloads() {
        let proxies = StaticProxy::parse_list(
            "# exported 2026-10-08\n\
             198.51.100.10:8080:user-a:pass-a\n\
             \n\
             proxy.example.net:3128:user-b:pass-b:de\n",
        )
        .unwrap();

        assert_eq!(proxies.len(), 2);
        assert_eq!(proxies[0].id, "198.51.100.10:8080");
        assert_eq!(proxies[0].host, "198.51.100.10");
        assert_eq!(proxies[0].port, 8080);
        assert_eq!(proxies[0].username.expose(), "user-a");
        assert_eq!(proxies[0].password.expose(), "pass-a");
        assert_eq!(proxies[0].country, None);
        assert_eq!(proxies[1].id, "proxy.example.net:3128");
        assert_eq!(proxies[1].country.unwrap().as_str(), "DE");
        assert_eq!(StaticProxy::parse_list("").unwrap().len(), 0);
        assert_eq!(StaticProxy::parse_list("# nothing\n\n").unwrap().len(), 0);
    }

    #[test]
    fn the_yaml_format_carries_everything() {
        let proxies = StaticProxy::parse_list(
            "proxies:
  - id: berlin-1
    host: 198.51.100.10
    port: 8080
    username: user
    password: \"pa:ss\"
    country: DE
    city: Berlin
    asn: AS64500
  - host: \"2001:db8::10\"
    port: 8081
    username: user
    password: other
",
        )
        .unwrap();

        assert_eq!(proxies.len(), 2);
        assert_eq!(proxies[0].id, "berlin-1");
        assert_eq!(proxies[0].password.expose(), "pa:ss");
        assert_eq!(proxies[0].country.unwrap().as_str(), "DE");
        assert_eq!(proxies[0].city.as_deref(), Some("Berlin"));
        assert_eq!(proxies[0].asn.as_deref(), Some("AS64500"));
        assert_eq!(proxies[1].id, "2001:db8::10:8081");
        assert_eq!(proxies[1].city, None);
        assert_eq!(StaticProxy::parse_list("proxies: []").unwrap().len(), 0);
    }

    #[test]
    fn malformed_lists_are_rejected_without_quoting_them() {
        let cases = [
            ("198.51.100.10:8080:user", "line 1: expected"),
            (
                "198.51.100.10:8080:user:hunter2:DE:extra",
                "line 1: expected",
            ),
            (
                "ok.example:1:u:hunter2\n198.51.100.10:http:user:hunter2",
                "line 2: the port",
            ),
            (
                "198.51.100.10:8080:user:hunter2:Germany",
                "line 1: the country",
            ),
            ("198.51.100.10:0:user:hunter2", "entry 1: the port"),
            (":8080:user:hunter2", "entry 1: the host"),
            ("198.51.100.10:8080::hunter2", "entry 1: the user name"),
            (
                "198.51.100.10:8080:user:hunter2\n198.51.100.10:8080:other:hunter2",
                "entry 2 repeats",
            ),
            (
                "proxies:\n  - host: a.example\n    port: 1\n    username: u\n",
                "YAML form",
            ),
            (
                "proxies:\n  - host: a.example\n    port: 1\n    username: u\n    password: hunter2\n    extra: 1\n",
                "YAML form",
            ),
            ("proxies: hunter2\n", "YAML form"),
            (
                "proxies:\n  - id: \" \"\n    host: a.example\n    port: 1\n    username: u\n    password: hunter2\n",
                "entry 1: the id",
            ),
            (
                "proxies:\n  - host: \"http://a.example\"\n    port: 1\n    username: u\n    password: hunter2\n",
                "entry 1: the host",
            ),
        ];
        for (text, needle) in cases {
            let message = StaticProxy::parse_list(text).unwrap_err().to_string();

            assert!(message.contains(needle), "{needle}: {message}");
            assert!(!message.contains("hunter2"), "{message}");
        }
    }

    #[test]
    fn parsed_proxies_never_print_their_credentials() {
        let proxies = StaticProxy::parse_list("198.51.100.10:8080:user-a:hunter2").unwrap();

        let printed = format!("{proxies:?}");

        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(!printed.contains("user-a"), "{printed}");
    }
}
