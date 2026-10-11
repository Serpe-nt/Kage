//! Configuration of the API server (spec §16.1, §19) and of what the
//! `security` section decides beyond the SSRF guard's own switch.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::duration::HumanDuration;
use crate::ssrf::SsrfPolicy;

/// The smallest buffer the server may be given for a request's head: the
/// HTTP/1.1 parser needs this much.
pub const MIN_HEADER_BYTES: u32 = 8 * 1024;

/// The largest buffer for a request's head.
pub const MAX_HEADER_BYTES: u32 = 1024 * 1024;

/// The largest request body the API can be configured to accept.
pub const MAX_API_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// The HTTP server (spec §16.1, §19 "Input limits" and "Rate limiting").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Address `kage serve` listens on. Loopback by default: the API is
    /// reached from the same host, or through whatever the operator puts
    /// in front of it. The compose file listens on every address inside
    /// the container and publishes the port on the host's loopback only.
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    /// Address `kage healthcheck` probes.
    #[serde(default = "default_healthcheck_addr")]
    pub healthcheck_addr: SocketAddr,
    /// A second address that serves `/metrics` and `/healthz` without a
    /// key, for a Prometheus on the same private network. Unset: metrics
    /// are served by the API only, to keys with the `admin` scope. Never
    /// publish this port.
    #[serde(default)]
    pub metrics_bind: Option<SocketAddr>,
    /// Largest request body, in bytes.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: u64,
    /// Largest head of a request (request line and headers), in bytes.
    #[serde(default = "default_max_header_bytes")]
    pub max_header_bytes: u32,
    /// Most headers a request may carry.
    #[serde(default = "default_max_headers")]
    pub max_headers: u32,
    /// Longest request target (path and query), in bytes.
    #[serde(default = "default_max_uri_bytes")]
    pub max_uri_bytes: u32,
    /// How long a client may take to send the head of a request, and how
    /// long a connection may sit idle between two requests. A connection
    /// that trickles its headers is closed when it runs out.
    #[serde(default = "default_header_read_timeout")]
    pub header_read_timeout: HumanDuration,
    /// How long a client may take to send the body of a request.
    #[serde(default = "default_body_read_timeout")]
    pub body_read_timeout: HumanDuration,
    /// How long any request other than a fetch may take. A fetch has its
    /// own limit, `scheduler.sync_wall_time`.
    #[serde(default = "default_request_timeout")]
    pub request_timeout: HumanDuration,
    /// How long the server waits for requests in flight when it stops.
    #[serde(default = "default_shutdown_grace")]
    pub shutdown_grace: HumanDuration,
    /// Most connections served at once; further ones wait to be accepted.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    /// Requests per minute over all keys together; 0 for no such limit.
    /// Each key has its own limit besides.
    #[serde(default = "default_requests_per_minute")]
    pub requests_per_minute: u32,
    /// Origins a browser may call the API from (`Access-Control-Allow-Origin`).
    /// Empty, the default: no CORS header is ever sent.
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,
    /// Serve Swagger UI at `/docs` to keys with the `admin` scope.
    #[serde(default = "default_true")]
    pub docs: bool,
}

const fn default_bind() -> SocketAddr {
    SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)
}

const fn default_healthcheck_addr() -> SocketAddr {
    SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 8080)
}

const fn default_max_body_bytes() -> u64 {
    1024 * 1024
}

const fn default_max_header_bytes() -> u32 {
    16 * 1024
}

const fn default_max_headers() -> u32 {
    64
}

const fn default_max_uri_bytes() -> u32 {
    8 * 1024
}

const fn default_header_read_timeout() -> HumanDuration {
    HumanDuration::from_secs(10)
}

const fn default_body_read_timeout() -> HumanDuration {
    HumanDuration::from_secs(30)
}

const fn default_request_timeout() -> HumanDuration {
    HumanDuration::from_secs(30)
}

const fn default_shutdown_grace() -> HumanDuration {
    HumanDuration::from_secs(30)
}

const fn default_max_connections() -> u32 {
    1024
}

const fn default_requests_per_minute() -> u32 {
    600
}

const fn default_true() -> bool {
    true
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            healthcheck_addr: default_healthcheck_addr(),
            metrics_bind: None,
            max_body_bytes: default_max_body_bytes(),
            max_header_bytes: default_max_header_bytes(),
            max_headers: default_max_headers(),
            max_uri_bytes: default_max_uri_bytes(),
            header_read_timeout: default_header_read_timeout(),
            body_read_timeout: default_body_read_timeout(),
            request_timeout: default_request_timeout(),
            shutdown_grace: default_shutdown_grace(),
            max_connections: default_max_connections(),
            requests_per_minute: default_requests_per_minute(),
            cors_allowed_origins: Vec::new(),
            docs: true,
        }
    }
}

/// The `security` section (spec §19).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityConfig {
    /// Allow URLs whose host is a literal IP address. The denied ranges
    /// apply either way.
    #[serde(default)]
    pub allow_ip_targets: bool,
    /// Look up every target, redirect hop and webhook host with this
    /// host's resolver as well, and refuse a name that resolves into a
    /// denied range. Off by default: the proxy resolves, and no name a
    /// consumer asks for is ever looked up from here. Turn it on only when
    /// the proxies could reach the operator's own networks (a static list
    /// of self-hosted proxies, for example).
    #[serde(default)]
    pub resolve_on_host: bool,
    /// Return the values of a target's `Set-Cookie` headers. Off by
    /// default: they are the persona's standing on that site, bound to its
    /// address and its fingerprint, and a result shows their names and
    /// attributes only.
    #[serde(default)]
    pub expose_set_cookie: bool,
}

impl SecurityConfig {
    /// The settings of the SSRF guard's own checks.
    pub const fn ssrf(&self) -> SsrfPolicy {
        SsrfPolicy {
            allow_ip_targets: self.allow_ip_targets,
        }
    }
}

/// What can be judged about the `server` section alone.
pub(super) fn validate(server: &ServerConfig) -> Result<(), String> {
    if server.max_body_bytes == 0 || server.max_body_bytes > MAX_API_BODY_BYTES {
        return Err(format!(
            "server.max_body_bytes must be between 1 and {MAX_API_BODY_BYTES}"
        ));
    }
    if !(MIN_HEADER_BYTES..=MAX_HEADER_BYTES).contains(&server.max_header_bytes) {
        return Err(format!(
            "server.max_header_bytes must be between {MIN_HEADER_BYTES} and {MAX_HEADER_BYTES}"
        ));
    }
    if !(8..=1024).contains(&server.max_headers) {
        return Err("server.max_headers must be between 8 and 1024".to_owned());
    }
    if server.max_uri_bytes < 256 || server.max_uri_bytes > server.max_header_bytes {
        return Err(
            "server.max_uri_bytes must be at least 256 and at most server.max_header_bytes"
                .to_owned(),
        );
    }
    for (key, value, least) in [
        (
            "server.header_read_timeout",
            server.header_read_timeout,
            Duration::from_millis(100),
        ),
        (
            "server.body_read_timeout",
            server.body_read_timeout,
            Duration::from_millis(100),
        ),
        (
            "server.request_timeout",
            server.request_timeout,
            Duration::from_secs(1),
        ),
    ] {
        if value.get() < least {
            return Err(format!("{key} must be at least {least:?}"));
        }
    }
    if server.max_connections == 0 {
        return Err("server.max_connections must be at least 1".to_owned());
    }
    if server.metrics_bind == Some(server.bind) {
        return Err(
            "server.metrics_bind must not be the address of the API: it serves metrics without \
             a key"
                .to_owned(),
        );
    }
    for origin in &server.cors_allowed_origins {
        let valid = url::Url::parse(origin).is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none()
                && !origin.ends_with('/')
        });
        if !valid {
            return Err(format!(
                "server.cors_allowed_origins: {origin:?} is not an origin such as \
                 https://console.example.org (a scheme, a host, an optional port, nothing else)"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{SecurityConfig, ServerConfig, validate};
    use crate::duration::HumanDuration;

    #[test]
    fn the_api_listens_on_loopback_unless_told_otherwise() {
        let server = ServerConfig::default();

        assert_eq!(server.bind.to_string(), "127.0.0.1:8080");
        assert!(server.bind.ip().is_loopback());
        assert_eq!(server.metrics_bind, None);
        assert_eq!(server.max_body_bytes, 1024 * 1024);
        assert_eq!(server.max_header_bytes, 16 * 1024);
        assert_eq!(server.max_headers, 64);
        assert_eq!(server.max_uri_bytes, 8 * 1024);
        assert_eq!(server.header_read_timeout.get(), Duration::from_secs(10));
        assert_eq!(server.request_timeout.get(), Duration::from_secs(30));
        assert_eq!(server.requests_per_minute, 600);
        assert_eq!(server.cors_allowed_origins, Vec::<String>::new());
        assert!(server.docs);
        assert_eq!(validate(&server), Ok(()));
    }

    #[test]
    fn limits_that_cannot_work_are_refused() {
        let broken = |change: fn(&mut ServerConfig)| {
            let mut server = ServerConfig::default();
            change(&mut server);
            validate(&server).unwrap_err()
        };

        assert!(broken(|server| server.max_body_bytes = 0).starts_with("server.max_body_bytes"));
        assert!(
            broken(|server| server.max_body_bytes = u64::MAX).starts_with("server.max_body_bytes")
        );
        assert!(
            broken(|server| server.max_header_bytes = 512).starts_with("server.max_header_bytes")
        );
        assert!(broken(|server| server.max_headers = 2).starts_with("server.max_headers"));
        assert!(broken(|server| server.max_uri_bytes = 100).starts_with("server.max_uri_bytes"));
        assert!(
            broken(|server| server.max_uri_bytes = server.max_header_bytes + 1)
                .starts_with("server.max_uri_bytes")
        );
        assert!(
            broken(|server| {
                server.header_read_timeout = HumanDuration::new(Duration::from_millis(1));
            })
            .starts_with("server.header_read_timeout")
        );
        assert!(
            broken(|server| server.request_timeout = HumanDuration::new(Duration::ZERO))
                .starts_with("server.request_timeout")
        );
        assert!(broken(|server| server.max_connections = 0).starts_with("server.max_connections"));
        assert!(
            broken(|server| server.metrics_bind = Some(server.bind))
                .starts_with("server.metrics_bind")
        );
    }

    #[test]
    fn a_cors_origin_is_a_scheme_a_host_and_a_port() {
        let with = |origin: &str| {
            let server = ServerConfig {
                cors_allowed_origins: vec![origin.to_owned()],
                ..ServerConfig::default()
            };
            validate(&server)
        };

        assert_eq!(with("https://console.example.org"), Ok(()));
        assert_eq!(with("http://localhost:5173"), Ok(()));
        for bad in [
            "*",
            "console.example.org",
            "https://console.example.org/",
            "https://console.example.org/app",
            "https://console.example.org?x=1",
            "ftp://console.example.org",
        ] {
            let error = with(bad).unwrap_err();
            assert!(error.starts_with("server.cors_allowed_origins"), "{error}");
        }
    }

    #[test]
    fn the_guard_takes_its_switch_from_the_security_section() {
        let security = SecurityConfig::default();
        assert!(!security.allow_ip_targets);
        assert!(!security.resolve_on_host);
        assert!(!security.expose_set_cookie);
        assert!(!security.ssrf().allow_ip_targets);

        let open = SecurityConfig {
            allow_ip_targets: true,
            ..SecurityConfig::default()
        };
        assert!(open.ssrf().allow_ip_targets);
    }
}
