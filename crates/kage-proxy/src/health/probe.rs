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
//! personas exist the factory is [`EmulatedClientFactory`]: a fresh client
//! of the http path (`kage-http`) per request, with one emulation profile
//! named in the configuration, so the probe presents the TLS, HTTP/2 and
//! header fingerprint of a current browser. A fresh client means a fresh
//! tunnel every time, without cookies, so the measured latency includes the
//! `CONNECT` and the handshake.
//!
//! Milestone M6 replaces the factory with one that returns the client of
//! the persona bound to the proxy, so that a probe is indistinguishable
//! from that persona's other traffic. A factory must return a client whose
//! every request goes through the given proxy and nowhere else: no
//! environment proxy, no direct fallback. Every client of `kage-http` is
//! built that way.
//!
//! # Where the time goes
//!
//! A probe reports its whole round trip and, separately, what opening the
//! connection cost: the tunnel through the proxy (TCP to the proxy and the
//! `CONNECT`, which includes the proxy dialling the echo) and the TLS
//! handshake. A proxy that is slow to open tunnels is slow on every new
//! connection a persona makes, whatever the target, so the two are kept
//! apart (follow-up F2 of the M2 live check).

use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use kage_core::config::{Config, EgressMode, ProxyHealthConfig, ResolvedProvider};
use kage_core::domain::{CountryCode, HealthFailure, ProxyEndpoint};
use kage_http::headers::{DEFAULT_LANGUAGES, Identity, IdentityOverride};
use kage_http::profile::{self, ProfileError};
use kage_http::{
    ClientFactory, ClientSpec, FetchSpec, HttpClient, HttpError, HttpSettings, ProfileChoice,
};
use url::Url;

use crate::machine::ExitObservation;
use crate::provider::{PUBLIC_ECHO_URL, capabilities_of};

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
    /// Where the lookup places the address: `latitude,longitude` in decimal
    /// degrees.
    pub loc: Option<String>,
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
    /// The configured profile is not a desktop Chrome or Firefox profile.
    #[error(
        "emulation profile {0:?} is not a desktop Chrome or Firefox profile; the http path supports only those"
    )]
    UnsupportedProfile(String),
    /// The configured platform is not one a persona can claim.
    #[error("unknown platform {0:?}; use windows, macos or linux")]
    UnknownPlatform(String),
    /// The HTTP client could not be built.
    #[error("cannot build the probe client: {0}")]
    Build(String),
}

impl From<ProfileError> for ProbeClientError {
    fn from(error: ProfileError) -> Self {
        match error {
            ProfileError::Unknown { name, known } => Self::UnknownProfile { name, known },
            ProfileError::Unsupported(name) => Self::UnsupportedProfile(name),
            ProfileError::UnknownPlatform(name) => Self::UnknownPlatform(name),
            other @ ProfileError::MissingHeader { .. } => Self::Build(other.to_string()),
        }
    }
}

/// Builds the HTTP client a probe uses for one proxy. See the module
/// documentation for the contract.
pub trait ProbeClientFactory: Send + Sync + fmt::Debug {
    /// A client of the http path whose every request goes through `target`
    /// and nowhere else.
    ///
    /// # Errors
    ///
    /// Fails when the client cannot be built.
    fn client(&self, target: &ProxyEndpoint) -> Result<HttpClient, ProbeClientError>;
}

/// The probe client until personas exist: one emulation profile for every
/// proxy, a fresh client of the http path per request.
#[derive(Debug)]
pub struct EmulatedClientFactory {
    factory: ClientFactory,
    spec: ClientSpec,
}

impl EmulatedClientFactory {
    /// A factory for a profile, with `timeout` as the limit of one request
    /// and of its connection setup.
    ///
    /// # Errors
    ///
    /// Fails when the profile's own identity must not be used (wreq-util's
    /// Firefox on Linux claims an Ubuntu build; only a persona can supply
    /// the real one).
    pub fn new(profile: ProfileChoice, timeout: Duration) -> Result<Self, ProbeClientError> {
        Identity::new(&profile, &DEFAULT_LANGUAGES, &IdentityOverride::default())
            .map_err(|error| ProbeClientError::Build(error.to_string()))?;
        Ok(Self {
            factory: ClientFactory::in_memory(HttpSettings {
                timeout,
                connect_timeout: timeout,
                max_body_bytes: MAX_BODY_BYTES,
                ..HttpSettings::default()
            }),
            spec: ClientSpec::new(profile),
        })
    }

    /// The factory the configuration names.
    ///
    /// # Errors
    ///
    /// Fails when the profile or the platform is not one the http path
    /// supports.
    pub fn from_config(config: &ProxyHealthConfig) -> Result<Self, ProbeClientError> {
        let os = profile::os_named(&config.platform)?;
        Self::new(
            ProfileChoice::named(&config.profile, os)?,
            config.timeout.get(),
        )
    }

    /// The emulation profile.
    pub const fn profile(&self) -> &ProfileChoice {
        &self.spec.profile
    }

    /// The limits of a probe: its timeout and how much of an answer it
    /// reads. Another probe client factory takes them from here, so that
    /// every probe has the same.
    pub const fn settings(&self) -> &HttpSettings {
        self.factory.settings()
    }
}

/// The name of every emulation profile this build of wreq-util ships.
pub fn profile_names() -> Vec<String> {
    profile::profile_names()
}

impl ProbeClientFactory for EmulatedClientFactory {
    fn client(&self, target: &ProxyEndpoint) -> Result<HttpClient, ProbeClientError> {
        // No error of the http path quotes the proxy's credentials.
        self.factory
            .ephemeral(&self.spec, target)
            .map_err(|error| ProbeClientError::Build(error.to_string()))
    }
}

/// The endpoints the health checks read, as the configuration resolves
/// them (spec §7.4, ADR 0013).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Endpoints {
    /// The echo every probe reads through the proxy: `proxies.health.echo_url`,
    /// else the provider's own echo, else [`PUBLIC_ECHO_URL`].
    pub echo: String,
    /// The second echo, tried once before a failure counts; when it is
    /// [`Self::echo`], a second read on a fresh tunnel.
    pub confirm: Option<String>,
    /// The geo lookup of an exit address; `None` switches it off.
    pub geo: Option<String>,
    /// The echo read without a proxy for the host's own address:
    /// `proxies.health.host_ip_url`, else the provider's own echo under
    /// `egress.mode: proxies`, else [`HOST_IP_ECHO_URL`].
    pub host_ip: String,
}

/// The echo the host's own address is read from when nothing else is
/// configured and the provider runs no echo, and always under
/// `egress.mode: direct`: a public echo of another operator than
/// [`PUBLIC_ECHO_URL`], so that nobody outside sees both the host and the
/// pool (ADR 0013).
pub const HOST_IP_ECHO_URL: &str = "https://api.ipify.org/";

impl Endpoints {
    /// The endpoints of `health` for `provider` under `mode`.
    pub fn resolve(
        health: &ProxyHealthConfig,
        provider: ResolvedProvider,
        mode: EgressMode,
    ) -> Self {
        let own = capabilities_of(provider).and_then(|capabilities| capabilities.echo);
        Self {
            echo: health
                .echo_url
                .clone()
                .unwrap_or_else(|| own.unwrap_or(PUBLIC_ECHO_URL).to_owned()),
            confirm: health.confirm_url.clone(),
            geo: health.geo_url.clone(),
            host_ip: health.host_ip_url.clone().unwrap_or_else(|| {
                match (mode, own) {
                    (EgressMode::Proxies, Some(own)) => own,
                    _ => HOST_IP_ECHO_URL,
                }
                .to_owned()
            }),
        }
    }

    /// The endpoints the configuration resolves to.
    pub fn from_config(config: &Config) -> Self {
        Self::resolve(
            &config.proxies.health,
            config.proxy_provider(),
            config.egress.mode,
        )
    }
}

/// Probes through the http path.
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
    pub fn from_config(config: &Config) -> Result<Self, ProbeClientError> {
        Ok(Self::new(
            EmulatedClientFactory::from_config(&config.proxies.health)?,
            &Endpoints::from_config(config),
        ))
    }
}

/// What reading one endpoint through a proxy gave.
struct Read {
    body: String,
    latency_ms: u32,
    connect_ms: Option<u32>,
    tls_ms: Option<u32>,
}

fn millis(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

impl<F: ProbeClientFactory> HttpProber<F> {
    /// A prober with its own client factory and the given endpoints.
    pub fn new(factory: F, endpoints: &Endpoints) -> Self {
        Self {
            factory,
            echo_url: endpoints.echo.clone(),
            confirm_url: endpoints.confirm.clone(),
            geo_url: endpoints.geo.clone(),
        }
    }

    /// Reads one endpoint through the proxy with a fresh client.
    async fn read(&self, target: &ProxyEndpoint, url: &str) -> Result<Read, HealthFailure> {
        let client = self.factory.client(target).map_err(|error| {
            tracing::error!(proxy = %target.id, %error, "cannot build the probe client");
            HealthFailure::Unreachable
        })?;
        let url = Url::parse(url).map_err(|error| {
            tracing::error!(proxy = %target.id, %error, "a probe endpoint is not a URL");
            HealthFailure::Unreachable
        })?;
        // An endpoint the operator configured: it is not followed anywhere
        // else, and it may be on this machine.
        let mut spec = FetchSpec::operator_get(url);
        spec.max_body_bytes = Some(MAX_BODY_BYTES);
        let fetched = client.fetch(&spec).await.map_err(|error| {
            let failure = if matches!(error, HttpError::ProxyAuth) {
                HealthFailure::ProxyAuth
            } else {
                HealthFailure::Unreachable
            };
            tracing::debug!(
                proxy = %target.id,
                timeout = error.is_timeout(),
                proxy_side = error.is_proxy_failure(),
                failure = failure.as_str(),
                "probe request failed"
            );
            failure
        })?;
        if fetched.status == 407 {
            return Err(HealthFailure::ProxyAuth);
        }
        if !(200..300).contains(&fetched.status) {
            tracing::debug!(proxy = %target.id, status = fetched.status, "probe answered with an error status");
            return Err(HealthFailure::Unreachable);
        }
        if fetched.truncated {
            return Err(HealthFailure::Unreachable);
        }
        Ok(Read {
            body: fetched.text(),
            latency_ms: millis(fetched.timing.total_ms),
            connect_ms: fetched.timing.connect_ms.map(millis),
            tls_ms: fetched.timing.tls_ms.map(millis),
        })
    }

    async fn read_exit(
        &self,
        target: &ProxyEndpoint,
        url: &str,
    ) -> Result<ExitObservation, HealthFailure> {
        let read = self.read(target, url).await?;
        let ip = parse_ip(&read.body).ok_or(HealthFailure::Unreachable)?;
        Ok(ExitObservation {
            ip,
            latency_ms: read.latency_ms,
            connect_ms: read.connect_ms,
            tls_ms: read.tls_ms,
        })
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
                Ok(read) => parse_geo(&read.body),
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

/// Reads the caller's address from an echo's answer: a bare address, a JSON
/// object with the address under a usual name (`peer` is Kage's own echo,
/// `kage echo serve`), or `key=value` lines. An address may carry the port
/// the echo saw (`203.0.113.20:51234`, `[2001:db8::7]:443`).
pub fn parse_ip(body: &str) -> Option<IpAddr> {
    let body = body.trim();
    if let Some(ip) = address(body) {
        return Some(ip);
    }
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(body) {
        return ["ip", "ip_address", "ipAddress", "query", "origin", "peer"]
            .iter()
            .find_map(|key| address(map.get(*key)?.as_str()?.trim()));
    }
    body.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == "ip")
            .then(|| address(value.trim()))
            .flatten()
    })
}

/// An address, alone or with a port.
fn address(text: &str) -> Option<IpAddr> {
    text.parse::<IpAddr>().ok().or_else(|| {
        text.parse::<std::net::SocketAddr>()
            .ok()
            .map(|socket| socket.ip())
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
            loc: None,
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
        loc: coordinates(&map),
    })
}

/// The coordinates a lookup names, as `latitude,longitude`: ipinfo.io's
/// `loc`, or a pair of numbers under the usual names. Anything that is not
/// a point on the globe is left out.
fn coordinates(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let number = |keys: &[&str]| -> Option<f64> {
        keys.iter().find_map(|key| {
            let value = map.get(*key)?;
            value
                .as_f64()
                .or_else(|| value.as_str()?.trim().parse().ok())
        })
    };
    let pair = map
        .get("loc")
        .and_then(serde_json::Value::as_str)
        .and_then(|loc| {
            let (latitude, longitude) = loc.split_once(',')?;
            Some((
                latitude.trim().parse().ok()?,
                longitude.trim().parse().ok()?,
            ))
        })
        .or_else(|| number(&["latitude", "lat"]).zip(number(&["longitude", "lon", "lng"])));
    let (latitude, longitude): (f64, f64) = pair?;
    ((-90.0..=90.0).contains(&latitude) && (-180.0..=180.0).contains(&longitude))
        .then(|| format!("{latitude},{longitude}"))
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
    use kage_core::domain::{ClaimedOs, ProxyEndpoint};
    use kage_core::ids::ProxyId;
    use kage_core::secret::Secret;
    use kage_http::ProfileChoice;

    use super::{
        EmulatedClientFactory, GeoPolicy, ProbeClientError, ProbeClientFactory, parse_geo,
        parse_ip, profile_names, split_org,
    };

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
        // With the port the echo saw, as some echoes write it.
        for body in [
            "203.0.113.20:51234",
            r#"{"ip": "203.0.113.20:51234", "http_version": "h2"}"#,
            "ip=203.0.113.20:443",
        ] {
            assert_eq!(parse_ip(body), Some(ip("203.0.113.20")), "{body}");
        }
        assert_eq!(
            parse_ip(r#"{"ip": "[2001:db8::7]:443"}"#),
            Some(ip("2001:db8::7"))
        );

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

    /// ADR 0013, consult 4b: the provider's own echo, else the public one;
    /// the host's address from the provider's echo under the pool, else
    /// from an echo of another operator, so nobody outside sees both.
    #[test]
    fn the_endpoints_follow_the_provider_and_the_mode() {
        use kage_core::config::{EgressMode, ResolvedProvider};

        use super::{Endpoints, HOST_IP_ECHO_URL};

        let health = ProxyHealthConfig::default();
        let resolve = |provider, mode| Endpoints::resolve(&health, provider, mode);

        let webshare = resolve(ResolvedProvider::Webshare, EgressMode::Proxies);
        assert_eq!(webshare.echo, "https://ipv4.webshare.io/");
        assert_eq!(webshare.host_ip, "https://ipv4.webshare.io/");
        assert_eq!(
            webshare.confirm.as_deref(),
            Some("https://checkip.amazonaws.com/")
        );
        assert_eq!(webshare.geo.as_deref(), Some("https://ipinfo.io/json"));
        for provider in [ResolvedProvider::Static, ResolvedProvider::None] {
            let listed = resolve(provider, EgressMode::Proxies);
            assert_eq!(listed.echo, "https://checkip.amazonaws.com/");
            // The confirmation is then a second read of the same echo.
            assert_eq!(listed.confirm.as_deref(), Some(listed.echo.as_str()));
            assert_eq!(listed.host_ip, HOST_IP_ECHO_URL);
        }
        assert_eq!(HOST_IP_ECHO_URL, "https://api.ipify.org/");
        // Under direct the host's address never comes from the provider.
        let direct = resolve(ResolvedProvider::Webshare, EgressMode::Direct);
        assert_eq!(direct.host_ip, HOST_IP_ECHO_URL);

        // What the operator names wins, whatever the provider.
        let own = ProxyHealthConfig {
            echo_url: Some("https://echo.example.org/".to_owned()),
            host_ip_url: Some("https://echo.example.org/host".to_owned()),
            confirm_url: None,
            geo_url: None,
            ..ProxyHealthConfig::default()
        };
        for (provider, mode) in [
            (ResolvedProvider::Webshare, EgressMode::Proxies),
            (ResolvedProvider::Static, EgressMode::Direct),
        ] {
            let endpoints = Endpoints::resolve(&own, provider, mode);
            assert_eq!(endpoints.echo, "https://echo.example.org/");
            assert_eq!(endpoints.host_ip, "https://echo.example.org/host");
            assert_eq!(endpoints.confirm, None);
            assert_eq!(endpoints.geo, None);
        }
    }

    /// The operator's own echo (`kage echo serve`, O5) is the private
    /// choice of `proxies.health.echo_url` (ADR 0013): the probe reads the
    /// address out of the echo's own answer, an observation of the
    /// request.
    #[test]
    fn kages_own_echo_answers_an_address_the_probe_reads() {
        use kage_http::echo::{ECHO_NAME, ECHO_VERSION, Observation, TlsObservation};

        for peer in ["203.0.113.20", "2001:db8::7"] {
            let observation = Observation {
                echo: ECHO_NAME.to_owned(),
                version: ECHO_VERSION,
                peer: peer.to_owned(),
                method: "GET".to_owned(),
                path: "/".to_owned(),
                http_version: "h2".to_owned(),
                tls: TlsObservation {
                    ja4: "t13d1516h2_8daaf6152771_02713d6af862".to_owned(),
                    ja4_r: String::new(),
                    version: "13".to_owned(),
                    sni: Some("echo.example.org".to_owned()),
                    alpn: vec!["h2".to_owned()],
                    ciphers: Vec::new(),
                    extensions: Vec::new(),
                    groups: Vec::new(),
                    signature_algorithms: Vec::new(),
                },
                http2: None,
                http1: None,
            };
            // As the echo writes it: pretty JSON and a newline.
            let body = format!("{}\n", serde_json::to_string_pretty(&observation).unwrap());

            assert_eq!(parse_ip(&body), Some(ip(peer)), "{body}");
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
        assert_eq!(geo.loc.as_deref(), Some("52.52,13.405"));
    }

    #[test]
    fn other_lookup_shapes_are_read_too() {
        // The minimum: a country.
        let minimal = parse_geo(r#"{"ip":"203.0.113.20","country":"pt"}"#).unwrap();
        assert_eq!(minimal.country.as_str(), "PT");
        assert_eq!(minimal.city, None);
        assert_eq!(minimal.asn, None);
        assert_eq!(minimal.timezone, None);
        assert_eq!(minimal.loc, None);

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
        assert_eq!(verbose.loc, None);

        // Coordinates as two numbers, or as text; a point off the globe is
        // left out and the record still stands.
        for (body, expected) in [
            (
                r#"{"countryCode":"IT","lat":45.4643,"lon":9.1895}"#,
                Some("45.4643,9.1895"),
            ),
            (
                r#"{"country":"FR","latitude":"48.8534","longitude":"2.3488"}"#,
                Some("48.8534,2.3488"),
            ),
            (
                r#"{"country":"FR","loc":"48.8534, 2.3488"}"#,
                Some("48.8534,2.3488"),
            ),
            (r#"{"country":"FR","loc":"948.8,2.3"}"#, None),
            (r#"{"country":"FR","loc":"Paris"}"#, None),
            (r#"{"country":"FR","lat":48.8}"#, None),
        ] {
            assert_eq!(parse_geo(body).unwrap().loc.as_deref(), expected, "{body}");
        }

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

        assert_eq!(factory.profile().name(), "chrome_149");
        assert_eq!(factory.profile().os(), ClaimedOs::Windows);
        assert!(format!("{factory:?}").contains("chrome_149"));
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
        assert!(matches!(error, ProbeClientError::UnknownProfile { .. }));
        assert!(message.contains("chrome_9000"), "{message}");
        assert!(message.contains("chrome_149"), "{message}");
        assert!(message.contains("firefox_"), "{message}");
        assert!(!message.contains("okhttp"), "{message}");

        // A profile wreq-util ships but the http path has no headers for.
        let config = ProxyHealthConfig {
            profile: "edge_148".to_owned(),
            ..ProxyHealthConfig::default()
        };
        assert_eq!(
            EmulatedClientFactory::from_config(&config).unwrap_err(),
            ProbeClientError::UnsupportedProfile("edge_148".to_owned())
        );

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

    /// wreq-util's Firefox on Linux claims an Ubuntu build, which only a
    /// persona's own User-Agent can replace; a probe has none.
    #[test]
    fn firefox_on_linux_is_refused_as_a_probe_profile() {
        let mut config = ProxyHealthConfig {
            profile: "firefox_151".to_owned(),
            platform: "linux".to_owned(),
            ..ProxyHealthConfig::default()
        };
        let error = EmulatedClientFactory::from_config(&config).unwrap_err();
        assert!(matches!(error, ProbeClientError::Build(_)), "{error:?}");
        assert!(error.to_string().contains("User-Agent"), "{error}");

        for platform in ["windows", "macos"] {
            config.platform = platform.to_owned();
            assert!(EmulatedClientFactory::from_config(&config).is_ok());
        }
    }

    #[test]
    fn a_client_is_built_for_ipv4_ipv6_and_named_proxies() {
        let factory = EmulatedClientFactory::new(
            ProfileChoice::named("chrome_149", ClaimedOs::Windows).unwrap(),
            Duration::from_secs(5),
        )
        .unwrap();
        for host in ["198.51.100.10", "2001:db8::10", "proxy.example.net"] {
            let target = ProxyEndpoint {
                id: ProxyId::new(),
                host: host.to_owned(),
                port: 8080,
                username: Secret::new("user with spaces"),
                password: Secret::new("p@ss:word/#?"),
                scheme: kage_core::domain::ProxyScheme::Http,
            };

            let client = factory.client(&target).unwrap();
            assert_eq!(client.proxy(), target.id, "{host}");
            // A probe starts without cookies and keeps none.
            assert!(client.jar().is_empty());
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
