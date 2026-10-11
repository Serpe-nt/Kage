//! The host-side lookup of the SSRF guard (spec §19), behind
//! `security.resolve_on_host`.
//!
//! By default Kage looks no name up: the proxy resolves every target, and a
//! lookup from the Kage host would show the operator's resolver to the name
//! servers of whatever a consumer asks for. The guard then judges the URL
//! and every redirect hop by what can be read from them (scheme, port,
//! literal addresses, names that never name a public site).
//!
//! A deployment whose proxies could reach the operator's own networks (a
//! static list of self-hosted proxies, say) turns the lookup on. Then the
//! target of every fetch, every redirect hop of the http path and, when
//! jobs carry them, every webhook host is resolved here with the system's
//! resolver, and a name with any address in a denied range is refused: the
//! proxy resolves the name again on its side and may pick any of them.
//!
//! The lookup is the operating system's (`getaddrinfo`), so it follows the
//! host's own configuration and adds no resolver of Kage's to the build.
//! It runs on the blocking pool under a deadline.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kage_core::ssrf::{SsrfDenial, SsrfPolicy, Target, check_resolved, check_url};
use kage_http::HopResolver;
use url::Url;

/// How long one lookup may take.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Looks host names up for the address check of the guard.
#[derive(Debug, Clone)]
pub struct HostResolver {
    timeout: Duration,
    fixed: Option<Arc<dyn HopResolver>>,
}

impl HostResolver {
    /// The system's resolver.
    pub const fn system() -> Self {
        Self {
            timeout: LOOKUP_TIMEOUT,
            fixed: None,
        }
    }

    /// A resolver that asks `inner` instead of the system: for tests, which
    /// must not depend on what a name resolves to today.
    pub fn over(inner: Arc<dyn HopResolver>) -> Self {
        Self {
            timeout: LOOKUP_TIMEOUT,
            fixed: Some(inner),
        }
    }

    async fn lookup(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        if let Some(fixed) = &self.fixed {
            return fixed.resolve(host).await;
        }
        // The port is not looked at; `lookup_host` wants one.
        let looking = tokio::net::lookup_host((host, 0));
        match tokio::time::timeout(self.timeout, looking).await {
            Err(_) => Err(format!("no answer within {:?}", self.timeout)),
            Ok(Err(error)) => Err(error.to_string()),
            Ok(Ok(addresses)) => {
                let mut addresses: Vec<IpAddr> = addresses.map(|address| address.ip()).collect();
                addresses.sort_unstable();
                addresses.dedup();
                Ok(addresses)
            }
        }
    }

    /// Checks the addresses a checked target's name resolves to. A target
    /// that is a literal address was judged by [`check_url`] already.
    ///
    /// # Errors
    ///
    /// The guard's refusal: an address in a denied range, or a name that
    /// resolves to nothing (which is also what a lookup that failed is
    /// reported as: a name nobody can look up is not fetched).
    pub async fn check(&self, target: &Target) -> Result<(), SsrfDenial> {
        let Some(name) = target.name() else {
            return Ok(());
        };
        let addresses = match self.lookup(name).await {
            Ok(addresses) => addresses,
            Err(reason) => {
                tracing::debug!(host = name, reason, "a host name could not be looked up");
                Vec::new()
            }
        };
        check_resolved(target, &addresses)
    }
}

#[async_trait]
impl HopResolver for HostResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        self.lookup(host).await
    }
}

/// The whole guard for a URL a caller named: what can be read from the URL
/// and, with a resolver, where its name points.
///
/// # Errors
///
/// The guard's refusal.
pub async fn guard(
    url: &Url,
    policy: SsrfPolicy,
    resolver: Option<&HostResolver>,
) -> Result<(), SsrfDenial> {
    let target = check_url(url, policy)?;
    if let Some(resolver) = resolver {
        resolver.check(&target).await?;
    }
    Ok(())
}

/// The guard for a webhook URL (spec §16.6, §19): the address Kage itself
/// would post a job's events to. It passes the same checks as a target,
/// and here they matter most: a webhook is the one request Kage makes
/// that does not leave through a proxy's network.
///
/// # Errors
///
/// The guard's refusal; [`SsrfDenial::InvalidUrl`] for text that is not a
/// URL.
pub async fn guard_webhook(
    url: &str,
    policy: SsrfPolicy,
    resolver: Option<&HostResolver>,
) -> Result<Url, SsrfDenial> {
    let parsed = Url::parse(url).map_err(|_| SsrfDenial::InvalidUrl(url.to_owned()))?;
    guard(&parsed, policy, resolver).await?;
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::IpAddr;
    use std::sync::Arc;

    use async_trait::async_trait;
    use kage_core::ssrf::{DeniedRange, SsrfDenial, SsrfPolicy};
    use kage_http::HopResolver;
    use url::Url;

    use super::{HostResolver, guard, guard_webhook};

    #[derive(Debug)]
    struct Fixed(HashMap<&'static str, Vec<&'static str>>);

    #[async_trait]
    impl HopResolver for Fixed {
        async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, String> {
            self.0
                .get(host)
                .map(|addresses| {
                    addresses
                        .iter()
                        .map(|address| address.parse().unwrap())
                        .collect()
                })
                .ok_or_else(|| "no such host".to_owned())
        }
    }

    fn resolver() -> HostResolver {
        HostResolver::over(Arc::new(Fixed(HashMap::from([
            (
                "public.example",
                vec!["93.184.216.34", "2606:2800:220:1::1"],
            ),
            ("intranet.example", vec!["10.0.0.5"]),
            ("mixed.example", vec!["93.184.216.34", "169.254.169.254"]),
            ("empty.example", vec![]),
        ]))))
    }

    const STRICT: SsrfPolicy = SsrfPolicy {
        allow_ip_targets: false,
    };

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[tokio::test]
    async fn a_name_that_points_into_a_denied_range_is_refused() {
        let resolver = resolver();
        let check = |text: &'static str| {
            let resolver = resolver.clone();
            async move { guard(&url(text), STRICT, Some(&resolver)).await }
        };

        assert_eq!(check("https://public.example/page").await, Ok(()));
        assert!(matches!(
            check("https://intranet.example/").await,
            Err(SsrfDenial::Address {
                range: DeniedRange::Private,
                ..
            })
        ));
        // One denied address denies the name: the proxy may pick any.
        assert!(matches!(
            check("https://mixed.example/").await,
            Err(SsrfDenial::Address {
                range: DeniedRange::CloudMetadata | DeniedRange::LinkLocal,
                ..
            })
        ));
        // A name that resolves to nothing, or cannot be looked up, is not
        // fetched.
        assert_eq!(
            check("https://empty.example/").await,
            Err(SsrfDenial::Unresolved("empty.example".to_owned()))
        );
        assert_eq!(
            check("https://unknown.example/").await,
            Err(SsrfDenial::Unresolved("unknown.example".to_owned()))
        );
    }

    #[tokio::test]
    async fn what_the_url_alone_shows_is_refused_before_anything_is_looked_up() {
        // The fixed resolver knows none of these: reaching it would fail
        // with `Unresolved`, not with the refusals below.
        let resolver = resolver();

        assert!(matches!(
            guard(&url("http://localhost/"), STRICT, Some(&resolver)).await,
            Err(SsrfDenial::Name { .. })
        ));
        assert!(matches!(
            guard(&url("http://127.0.0.1/"), STRICT, Some(&resolver)).await,
            Err(SsrfDenial::Address {
                range: DeniedRange::Loopback,
                ..
            })
        ));
        assert!(matches!(
            guard(&url("ftp://public.example/"), STRICT, Some(&resolver)).await,
            Err(SsrfDenial::Scheme(_))
        ));
    }

    #[tokio::test]
    async fn without_a_resolver_nothing_is_looked_up() {
        // The default: the proxy resolves. A name that would resolve into
        // a private range passes here and is the proxy's to refuse.
        assert_eq!(
            guard(&url("https://intranet.example/"), STRICT, None).await,
            Ok(())
        );
        // A literal address needs no lookup, with or without a resolver.
        let open = SsrfPolicy {
            allow_ip_targets: true,
        };
        assert_eq!(
            guard(&url("https://93.184.216.34/"), open, Some(&resolver())).await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn a_webhook_url_in_a_denied_range_is_refused() {
        let resolver = resolver();
        let open = SsrfPolicy {
            allow_ip_targets: true,
        };

        // By number: denied ranges stay denied whatever the switch says.
        for (url, range) in [
            ("http://127.0.0.1:9000/hook", DeniedRange::Loopback),
            ("http://10.1.2.3/hook", DeniedRange::Private),
            ("http://192.168.1.10:8080/hook", DeniedRange::Private),
            ("http://172.16.0.9/hook", DeniedRange::Private),
            ("http://100.64.0.1/hook", DeniedRange::Cgnat),
            ("http://[::1]/hook", DeniedRange::Loopback),
            ("http://[fd00::1]/hook", DeniedRange::UniqueLocal),
        ] {
            for policy in [STRICT, open] {
                let refused = guard_webhook(url, policy, None).await.unwrap_err();
                assert!(
                    matches!(refused, SsrfDenial::Address { range: found, .. } if found == range),
                    "{url}: {refused}"
                );
            }
        }
        // The metadata address of a cloud, by number.
        assert!(matches!(
            guard_webhook("http://169.254.169.254/latest/meta-data/", open, None).await,
            Err(SsrfDenial::Address { .. })
        ));
        // By a name that never names a public host.
        assert!(matches!(
            guard_webhook("https://hooks.internal/kage", STRICT, None).await,
            Err(SsrfDenial::Name { .. })
        ));
        // By a name that resolves into a private range, when the host
        // looks names up.
        assert!(matches!(
            guard_webhook("https://intranet.example/hook", STRICT, Some(&resolver)).await,
            Err(SsrfDenial::Address {
                range: DeniedRange::Private,
                ..
            })
        ));
        // Not a URL, not http.
        assert!(matches!(
            guard_webhook("hooks.example.org/kage", STRICT, None).await,
            Err(SsrfDenial::InvalidUrl(_))
        ));
        assert!(matches!(
            guard_webhook("file:///etc/passwd", STRICT, None).await,
            Err(SsrfDenial::Scheme(_))
        ));
        // A public receiver passes, and comes back parsed.
        let url = guard_webhook("https://public.example/hooks/kage", STRICT, Some(&resolver))
            .await
            .unwrap();
        assert_eq!(url.host_str(), Some("public.example"));
    }

    #[tokio::test]
    async fn the_systems_resolver_answers_for_a_literal_and_fails_for_an_impossible_name() {
        let system = HostResolver::system();

        // getaddrinfo answers a literal without asking anyone.
        assert_eq!(
            system.resolve("192.0.2.7").await.unwrap(),
            ["192.0.2.7".parse::<IpAddr>().unwrap()]
        );
        // `.invalid` never resolves (RFC 6761), whatever the network is.
        assert!(system.resolve("kage-no-such-host.invalid").await.is_err());
    }
}
