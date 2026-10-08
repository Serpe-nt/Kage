//! One health probe of one proxy (spec §7.4): read an echo through the
//! proxy for reachability, latency and the exit address, and now and then
//! look the exit address up for its country.
//!
//! # Who sees the probes
//!
//! The echo that every probe reads is run by the proxy provider itself, who
//! sees every connection anyway, so the probes add no observer. Only when
//! that echo fails is a second one, run by someone else, tried once before
//! the failure counts. The geo lookup goes to a third party, but only when
//! its answer can change: on the first check of a proxy, when the exit
//! address changes, and once a day.
//!
//! # The client-factory seam
//!
//! A probe gets its HTTP client from a [`ProbeClientFactory`]. Until
//! personas exist the factory is [`EmulatedClientFactory`]: a fresh wreq
//! client per request, built from one wreq-util emulation profile named in
//! the configuration, so the probe presents the TLS, HTTP/2 and header
//! fingerprint of a current browser. A fresh client means a fresh tunnel
//! every time, so the measured latency includes the `CONNECT`.
//!
//! Milestones M3 and M6 replace the factory with one that returns the
//! client of the persona bound to the proxy, so that a probe is
//! indistinguishable from that persona's other traffic. A factory must
//! return a client whose every request goes through the given proxy and
//! nowhere else: no environment proxy, no direct fallback.

use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use kage_core::config::ProxyHealthConfig;
use kage_core::domain::{CountryCode, HealthFailure};
use tokio::time::Instant;
use wreq_util::{Emulation, Platform, Profile};

use crate::machine::ExitObservation;
use crate::pool::ProxyEndpoint;

/// Longest body an echo or geo endpoint may send. An address is a few
/// dozen bytes and a geo record a few hundred.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// What a geo lookup said about an exit address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeoLookup {
    /// Country of the exit address.
    pub country: CountryCode,
    /// City, when the lookup names one.
    pub city: Option<String>,
    /// Region or state, when the lookup names one.
    pub region: Option<String>,
    /// Autonomous system number, for example `AS3320`.
    pub asn: Option<String>,
    /// Name of the network operator.
    pub org: Option<String>,
    /// IANA time zone of the exit address.
    pub timezone: Option<String>,
}

/// When a probe should also look the exit address up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeoPolicy {
    /// Never: the lookup is switched off, or it failed a moment ago.
    Never,
    /// Whenever an exit address was read.
    Always,
    /// Only when the exit address differs from the one last looked up.
    OnChange {
        /// The address the stored geo record is about.
        known: IpAddr,
    },
}

impl GeoPolicy {
    /// Whether to look `exit` up.
    pub fn wants(self, exit: IpAddr) -> bool {
        match self {
            Self::Never => false,
            Self::Always => true,
            Self::OnChange { known } => known != exit,
        }
    }
}

/// What one probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOutcome {
    /// What the echo saw, or why none could be read.
    pub exit: Result<ExitObservation, HealthFailure>,
    /// Whether a geo lookup was sent.
    pub geo_attempted: bool,
    /// What the geo lookup said, when it answered.
    pub geo: Option<GeoLookup>,
}

/// Probes proxies.
#[async_trait]
pub trait Prober: Send + Sync + fmt::Debug {
    /// Probes one proxy once.
    async fn probe(&self, target: &ProxyEndpoint, geo: GeoPolicy) -> ProbeOutcome;
}

/// Why a probe client could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProbeClientError {
    /// The configured emulation profile is not one wreq-util ships.
    #[error("unknown emulation profile {name:?}; this build knows: {known}")]
    UnknownProfile {
        /// The configured name.
        name: String,
        /// The names this build knows.
        known: String,
    },
    /// The configured platform is not one wreq-util knows.
    #[error("unknown platform {0:?}; use windows, macos or linux")]
    UnknownPlatform(String),
    /// The HTTP client could not be built.
    #[error("cannot build the probe client: {0}")]
    Build(String),
}

/// Builds the HTTP client a probe uses for one proxy. See the module
/// documentation for the contract.
pub trait ProbeClientFactory: Send + Sync + fmt::Debug {
    /// A client whose every request goes through `target` and nowhere else.
    ///
    /// # Errors
    ///
    /// Fails when the client cannot be built.
    fn client(&self, target: &ProxyEndpoint) -> Result<wreq::Client, ProbeClientError>;
}

/// The probe client until personas exist: one wreq-util emulation profile
/// for every proxy, a fresh client per request.
#[derive(Clone)]
pub struct EmulatedClientFactory {
    profile: Profile,
    platform: Platform,
    timeout: Duration,
}

impl fmt::Debug for EmulatedClientFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmulatedClientFactory")
            .field("profile", &self.profile)
            .field("platform", &self.platform)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl EmulatedClientFactory {
    /// A factory for a profile and platform.
    pub const fn new(profile: Profile, platform: Platform, timeout: Duration) -> Self {
        Self {
            profile,
            platform,
            timeout,
        }
    }

    /// The factory the configuration names.
    ///
    /// # Errors
    ///
    /// Fails when the profile or the platform is not one wreq-util ships.
    pub fn from_config(config: &ProxyHealthConfig) -> Result<Self, ProbeClientError> {
        Ok(Self::new(
            profile_named(&config.profile)?,
            platform_named(&config.platform)?,
            config.timeout.get(),
        ))
    }

    /// The emulation profile.
    pub const fn profile(&self) -> Profile {
        self.profile
    }
}

/// The wreq-util profile with this name, for example `chrome_149`.
///
/// # Errors
///
/// Fails when this build of wreq-util has no such profile; the error lists
/// the Chrome and Firefox profiles it has.
pub fn profile_named(name: &str) -> Result<Profile, ProbeClientError> {
    serde_json::from_value(serde_json::Value::String(name.to_owned())).map_err(|_| {
        ProbeClientError::UnknownProfile {
            name: name.to_owned(),
            known: profile_names()
                .into_iter()
                .filter(|known| known.starts_with("chrome_") || known.starts_with("firefox_"))
                .collect::<Vec<_>>()
                .join(", "),
        }
    })
}

/// The name of every emulation profile this build of wreq-util ships.
pub fn profile_names() -> Vec<String> {
    Profile::VARIANTS
        .iter()
        .filter_map(|profile| match serde_json::to_value(profile) {
            Ok(serde_json::Value::String(name)) => Some(name),
            _ => None,
        })
        .collect()
}

fn platform_named(name: &str) -> Result<Platform, ProbeClientError> {
    match name {
        "windows" => Ok(Platform::Windows),
        "macos" => Ok(Platform::MacOS),
        "linux" => Ok(Platform::Linux),
        other => Err(ProbeClientError::UnknownPlatform(other.to_owned())),
    }
}

impl ProbeClientFactory for EmulatedClientFactory {
    fn client(&self, target: &ProxyEndpoint) -> Result<wreq::Client, ProbeClientError> {
        // The error of a proxy URL can quote the URL; it holds no
        // credentials here, they are added as a header below.
        let proxy = wreq::Proxy::all(target.url())
            .map_err(|error| ProbeClientError::Build(error.to_string()))?
            .basic_auth(target.username.expose(), target.password.expose());
        wreq::Client::builder()
            .emulation(
                Emulation::builder()
                    .profile(self.profile)
                    .platform(self.platform)
                    .build(),
            )
            // Setting a proxy also switches off every proxy named in the
            // environment; there is no direct fallback.
            .proxy(proxy)
            // An echo has no reason to redirect, and a probe must never be
            // sent on to another site.
            .redirect(wreq::redirect::Policy::none())
            .timeout(self.timeout)
            .connect_timeout(self.timeout)
            .build()
            .map_err(|_| ProbeClientError::Build("the HTTP client rejected its options".to_owned()))
    }
}

/// Probes through wreq.
#[derive(Debug)]
pub struct HttpProber<F = EmulatedClientFactory> {
    factory: F,
    echo_url: String,
    confirm_url: Option<String>,
    geo_url: Option<String>,
}

impl HttpProber<EmulatedClientFactory> {
    /// The prober the configuration describes.
    ///
    /// # Errors
    ///
    /// Fails when the emulation profile or platform is unknown.
    pub fn from_config(config: &ProxyHealthConfig) -> Result<Self, ProbeClientError> {
        Ok(Self::new(
            EmulatedClientFactory::from_config(config)?,
            config,
        ))
    }
}

impl<F: ProbeClientFactory> HttpProber<F> {
    /// A prober with its own client factory and the configured endpoints.
    pub fn new(factory: F, config: &ProxyHealthConfig) -> Self {
        Self {
            factory,
            echo_url: config.echo_url.clone(),
            confirm_url: config.confirm_url.clone(),
            geo_url: config.geo_url.clone(),
        }
    }

    /// Reads one endpoint through the proxy with a fresh client.
    async fn read(
        &self,
        target: &ProxyEndpoint,
        url: &str,
    ) -> Result<(String, u32), HealthFailure> {
        let client = self.factory.client(target).map_err(|error| {
            tracing::error!(proxy = %target.id, %error, "cannot build the probe client");
            HealthFailure::Unreachable
        })?;
        let started = Instant::now();
        let response = client.get(url).send().await.map_err(|error| {
            let failure = if is_proxy_auth(&error) {
                HealthFailure::ProxyAuth
            } else {
                HealthFailure::Unreachable
            };
            // The error text can hold the proxy URL, so only its kind is
            // logged.
            tracing::debug!(
                proxy = %target.id,
                timeout = error.is_timeout(),
                connect = error.is_connect(),
                proxy_connect = error.is_proxy_connect(),
                failure = failure.as_str(),
                "probe request failed"
            );
            failure
        })?;
        let status = response.status();
        if status == wreq::StatusCode::PROXY_AUTHENTICATION_REQUIRED {
            return Err(HealthFailure::ProxyAuth);
        }
        if !status.is_success() {
            tracing::debug!(proxy = %target.id, status = status.as_u16(), "probe answered with an error status");
            return Err(HealthFailure::Unreachable);
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
        {
            return Err(HealthFailure::Unreachable);
        }
        let body = response
            .bytes()
            .await
            .map_err(|_| HealthFailure::Unreachable)?;
        let latency = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        if body.len() > MAX_BODY_BYTES {
            return Err(HealthFailure::Unreachable);
        }
        Ok((String::from_utf8_lossy(&body).into_owned(), latency))
    }

    async fn read_exit(
        &self,
        target: &ProxyEndpoint,
        url: &str,
    ) -> Result<ExitObservation, HealthFailure> {
        let (body, latency_ms) = self.read(target, url).await?;
        let ip = parse_ip(&body).ok_or(HealthFailure::Unreachable)?;
        Ok(ExitObservation { ip, latency_ms })
    }
}

#[async_trait]
impl<F: ProbeClientFactory> Prober for HttpProber<F> {
    async fn probe(&self, target: &ProxyEndpoint, geo: GeoPolicy) -> ProbeOutcome {
        let exit = match self.read_exit(target, &self.echo_url).await {
            Ok(exit) => Ok(exit),
            // Before a failure counts, one echo run by someone else is
            // tried: an echo that is down must not look like a dead proxy.
            Err(first) => match &self.confirm_url {
                Some(url) => self.read_exit(target, url).await.map_err(|second| {
                    // A refused login explains more than a timeout.
                    if first == HealthFailure::ProxyAuth || second == HealthFailure::ProxyAuth {
                        HealthFailure::ProxyAuth
                    } else {
                        second
                    }
                }),
                None => Err(first),
            },
        };

        let mut geo_attempted = false;
        let mut lookup = None;
        if let (Ok(exit), Some(url)) = (&exit, &self.geo_url)
            && geo.wants(exit.ip)
        {
            geo_attempted = true;
            lookup = match self.read(target, url).await {
                Ok((body, _)) => parse_geo(&body),
                Err(_) => None,
            };
            if lookup.is_none() {
                tracing::debug!(proxy = %target.id, "the geo lookup did not answer with a country");
            }
        }
        ProbeOutcome {
            exit,
            geo_attempted,
            geo: lookup,
        }
    }
}

/// Whether the proxy refused the credentials while the tunnel was set up.
fn is_proxy_auth(error: &wreq::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    while let Some(cause) = source {
        if cause.to_string().contains("proxy authorization required") {
            return true;
        }
        source = cause.source();
    }
    false
}

/// Reads the caller's address from an echo's answer: a bare address, a JSON
/// object with the address under a usual name, or `key=value` lines.
pub fn parse_ip(body: &str) -> Option<IpAddr> {
    let body = body.trim();
    if let Ok(ip) = body.parse() {
        return Some(ip);
    }
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(body) {
        return ["ip", "ip_address", "ipAddress", "query", "origin"]
            .iter()
            .find_map(|key| map.get(*key)?.as_str()?.trim().parse().ok());
    }
    body.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "ip")
            .then(|| value.trim().parse().ok())
            .flatten()
    })
}

/// Reads a geo record from a lookup's answer: a JSON object in the shape of
/// ipinfo.io (`country`, `city`, `region`, `org`, `timezone`) or of its
/// common relatives, or `key=value` lines with `loc=` as the country.
pub fn parse_geo(body: &str) -> Option<GeoLookup> {
    let body = body.trim();
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str(body) else {
        // `key=value` lines, as Cloudflare's trace answers.
        let country = body.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "loc")
                .then(|| CountryCode::new(value.trim()).ok())
                .flatten()
        })?;
        return Some(GeoLookup {
            country,
            city: None,
            region: None,
            asn: None,
            org: None,
            timezone: None,
        });
    };
    let text = |keys: &[&str]| -> Option<String> {
        keys.iter().find_map(|key| {
            let value = map.get(*key)?.as_str()?.trim();
            (!value.is_empty()).then(|| value.to_owned())
        })
    };
    // Some services put the name under `country` and the code elsewhere.
    let country = [
        "country",
        "country_code",
        "countryCode",
        "country_iso",
        "cc",
    ]
    .iter()
    .find_map(|key| CountryCode::new(map.get(*key)?.as_str()?.trim()).ok())?;
    let timezone = text(&["timezone", "time_zone"]).or_else(|| {
        // Or an object with the name inside.
        let zone = map.get("timezone").or_else(|| map.get("time_zone"))?;
        ["id", "name"]
            .iter()
            .find_map(|key| Some(zone.get(*key)?.as_str()?.trim().to_owned()))
            .filter(|name| !name.is_empty())
    });
    let asn = text(&["asn"]).or_else(|| map.get("asn")?.as_u64().map(|number| number.to_string()));
    let (asn, org) = split_org(asn.as_deref(), text(&["org", "as", "isp", "organization"]));
    Some(GeoLookup {
        country,
        city: text(&["city"]),
        region: text(&["region", "regionName", "region_name"]),
        asn,
        org,
        timezone,
    })
}

/// Separates the AS number from the operator's name. ipinfo.io writes both
/// in one field: `AS3320 Deutsche Telekom AG`.
fn split_org(asn: Option<&str>, org: Option<String>) -> (Option<String>, Option<String>) {
    let number = |text: &str| -> Option<String> {
        let digits = text
            .strip_prefix("AS")
            .or_else(|| text.strip_prefix("as"))
            .unwrap_or(text);
        (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| format!("AS{digits}"))
    };
    let asn = asn.and_then(number);
    match org {
        Some(org) => match org.split_once(' ') {
            Some((first, rest)) if first.len() > 2 && number(first).is_some() => (
                asn.or_else(|| number(first)),
                Some(rest.trim().to_owned()).filter(|rest| !rest.is_empty()),
            ),
            _ if asn.is_none() && org.len() > 2 && number(&org).is_some() => (number(&org), None),
            _ => (asn, Some(org)),
        },
        None => (asn, None),
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::time::Duration;

    use kage_core::config::ProxyHealthConfig;
    use kage_core::ids::ProxyId;
    use kage_core::secret::Secret;

    use super::{
        EmulatedClientFactory, GeoPolicy, ProbeClientError, ProbeClientFactory, parse_geo,
        parse_ip, profile_named, profile_names, split_org,
    };
    use crate::pool::ProxyEndpoint;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn an_echo_answer_is_read_in_every_usual_shape() {
        for body in [
            "203.0.113.20",
            "203.0.113.20\n",
            "  203.0.113.20  \r\n",
            r#"{"ip":"203.0.113.20"}"#,
            r#"{"ip_address": "203.0.113.20"}"#,
            r#"{ "query": "203.0.113.20", "status": "success" }"#,
            r#"{"origin": " 203.0.113.20 "}"#,
            "fl=107f251\nh=www.cloudflare.com\nip=203.0.113.20\nts=1791497681.000\nloc=PT\n",
        ] {
            assert_eq!(parse_ip(body), Some(ip("203.0.113.20")), "{body}");
        }
        assert_eq!(parse_ip("2001:db8::7"), Some(ip("2001:db8::7")));
        assert_eq!(parse_ip(r#"{"ip":"2001:db8::7"}"#), Some(ip("2001:db8::7")));

        for body in [
            "",
            "not an address",
            "<html>blocked</html>",
            r#"{"ip": 3405803796}"#,
            r#"{"address": "203.0.113.20"}"#,
            r#"["203.0.113.20"]"#,
            "ipv4=203.0.113.20",
            "ip=unknown",
            "999.0.0.1",
        ] {
            assert_eq!(parse_ip(body), None, "{body}");
        }
    }

    #[test]
    fn an_ipinfo_answer_gives_the_whole_geo_record() {
        let geo = parse_geo(
            r#"{
              "ip": "203.0.113.20",
              "hostname": "p5.dip0.t-ipconnect.de",
              "city": "Berlin",
              "region": "State of Berlin",
              "country": "DE",
              "loc": "52.5200,13.4050",
              "org": "AS3320 Deutsche Telekom AG",
              "postal": "10115",
              "timezone": "Europe/Berlin",
              "readme": "https://ipinfo.io/missingauth"
            }"#,
        )
        .unwrap();

        assert_eq!(geo.country.as_str(), "DE");
        assert_eq!(geo.city.as_deref(), Some("Berlin"));
        assert_eq!(geo.region.as_deref(), Some("State of Berlin"));
        assert_eq!(geo.asn.as_deref(), Some("AS3320"));
        assert_eq!(geo.org.as_deref(), Some("Deutsche Telekom AG"));
        assert_eq!(geo.timezone.as_deref(), Some("Europe/Berlin"));
    }

    #[test]
    fn other_lookup_shapes_are_read_too() {
        // The minimum: a country.
        let minimal = parse_geo(r#"{"ip":"203.0.113.20","country":"pt"}"#).unwrap();
        assert_eq!(minimal.country.as_str(), "PT");
        assert_eq!(minimal.city, None);
        assert_eq!(minimal.asn, None);
        assert_eq!(minimal.timezone, None);

        // The name under `country`, the code under another key; the zone
        // as an object; the AS number as a number.
        let verbose = parse_geo(
            r#"{"country": "Austria", "country_code": "AT", "regionName": "Vienna",
                "city": " Vienna ", "timezone": {"id": "Europe/Vienna", "utc": "+02:00"},
                "asn": 8447, "isp": "A1 Telekom Austria AG"}"#,
        )
        .unwrap();
        assert_eq!(verbose.country.as_str(), "AT");
        assert_eq!(verbose.city.as_deref(), Some("Vienna"));
        assert_eq!(verbose.region.as_deref(), Some("Vienna"));
        assert_eq!(verbose.timezone.as_deref(), Some("Europe/Vienna"));
        assert_eq!(verbose.asn.as_deref(), Some("AS8447"));
        assert_eq!(verbose.org.as_deref(), Some("A1 Telekom Austria AG"));

        // Cloudflare's trace.
        let trace = parse_geo("ip=203.0.113.20\ncolo=LIS\nloc=PT\ntls=TLSv1.3\n").unwrap();
        assert_eq!(trace.country.as_str(), "PT");
        assert_eq!(trace.city, None);
    }

    #[test]
    fn an_answer_without_a_country_is_no_geo_record() {
        for body in [
            "",
            "203.0.113.20",
            r#"{"ip":"203.0.113.20"}"#,
            r#"{"country":"Germany"}"#,
            r#"{"country":""}"#,
            r#"{"country":12}"#,
            r#"{"error":{"title":"Rate limit exceeded"}}"#,
            "<html>429</html>",
            "loc=Portugal",
            // ipinfo's `loc` is coordinates, not a country.
            r#"{"loc":"52.5200,13.4050"}"#,
        ] {
            assert_eq!(parse_geo(body), None, "{body}");
        }
    }

    #[test]
    fn the_as_number_is_separated_from_the_operators_name() {
        let split = |asn: Option<&str>, org: Option<&str>| split_org(asn, org.map(str::to_owned));
        let some = |text: &str| Some(text.to_owned());

        assert_eq!(
            split(None, Some("AS3320 Deutsche Telekom AG")),
            (some("AS3320"), some("Deutsche Telekom AG"))
        );
        assert_eq!(
            split(Some("8447"), Some("A1 Telekom Austria AG")),
            (some("AS8447"), some("A1 Telekom Austria AG"))
        );
        assert_eq!(
            split(Some("AS8447"), Some("AS8447 A1 Telekom Austria AG")),
            (some("AS8447"), some("A1 Telekom Austria AG"))
        );
        assert_eq!(split(None, Some("AS3320")), (some("AS3320"), None));
        // A name that merely starts like one stays a name.
        assert_eq!(
            split(None, Some("AS Roma Networks")),
            (None, some("AS Roma Networks"))
        );
        assert_eq!(split(None, Some("ASTRA")), (None, some("ASTRA")));
        assert_eq!(split(Some("not-a-number"), None), (None, None));
        assert_eq!(split(None, None), (None, None));
    }

    #[test]
    fn the_geo_policy_decides_per_exit_address() {
        let known = ip("203.0.113.20");

        assert!(!GeoPolicy::Never.wants(known));
        assert!(GeoPolicy::Always.wants(known));
        assert!(!GeoPolicy::OnChange { known }.wants(known));
        assert!(GeoPolicy::OnChange { known }.wants(ip("203.0.113.21")));
    }

    #[test]
    fn the_default_profile_exists_in_this_build() {
        let config = ProxyHealthConfig::default();

        let factory = EmulatedClientFactory::from_config(&config).unwrap();

        assert_eq!(factory.profile(), profile_named("chrome_149").unwrap());
        assert!(format!("{factory:?}").contains("Chrome149"));
        // The default is the newest Chrome this build of wreq-util ships:
        // when the pin moves, the default must move with it.
        let newest_chrome = profile_names()
            .into_iter()
            .filter_map(|name| name.strip_prefix("chrome_")?.parse::<u32>().ok())
            .max()
            .unwrap();
        assert_eq!(config.profile, format!("chrome_{newest_chrome}"));
    }

    #[test]
    fn unknown_profiles_and_platforms_are_refused_with_what_is_known() {
        let config = ProxyHealthConfig {
            profile: "chrome_9000".to_owned(),
            ..ProxyHealthConfig::default()
        };
        let error = EmulatedClientFactory::from_config(&config).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("chrome_9000"), "{message}");
        assert!(message.contains("chrome_149"), "{message}");
        assert!(message.contains("firefox_"), "{message}");
        assert!(!message.contains("okhttp"), "{message}");

        let mut config = ProxyHealthConfig {
            platform: "android".to_owned(),
            ..ProxyHealthConfig::default()
        };
        assert_eq!(
            EmulatedClientFactory::from_config(&config).unwrap_err(),
            ProbeClientError::UnknownPlatform("android".to_owned())
        );
        for platform in ["windows", "macos", "linux"] {
            config.platform = platform.to_owned();
            assert!(EmulatedClientFactory::from_config(&config).is_ok());
        }
        assert!(profile_names().len() > 50);
    }

    #[test]
    fn a_client_is_built_for_ipv4_ipv6_and_named_proxies() {
        let factory = EmulatedClientFactory::new(
            profile_named("chrome_149").unwrap(),
            wreq_util::Platform::Windows,
            Duration::from_secs(5),
        );
        for host in ["198.51.100.10", "2001:db8::10", "proxy.example.net"] {
            let target = ProxyEndpoint {
                id: ProxyId::new(),
                host: host.to_owned(),
                port: 8080,
                username: Secret::new("user with spaces"),
                password: Secret::new("p@ss:word/#?"),
                scheme: kage_core::domain::ProxyScheme::Http,
            };

            assert!(factory.client(&target).is_ok(), "{host}");
        }
        // A host that cannot be part of a URL is refused, not connected to.
        let broken = ProxyEndpoint {
            id: ProxyId::new(),
            host: "not a host".to_owned(),
            port: 8080,
            username: Secret::new("user"),
            password: Secret::new("hunter2"),
            scheme: kage_core::domain::ProxyScheme::Http,
        };
        let Err(error) = factory.client(&broken) else {
            panic!("a client was built for a host that is no host");
        };
        assert!(matches!(error, ProbeClientError::Build(_)));
        assert!(!error.to_string().contains("hunter2"));
    }
}
