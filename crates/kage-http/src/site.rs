//! Origins and sites, the way a browser tells them apart.
//!
//! Three questions the http path must answer exactly like a browser, because
//! a server can see the answer in the request:
//!
//! - How does this request relate to the page that caused it? That is the
//!   `Sec-Fetch-Site` header and the `SameSite` rule of every cookie.
//! - Is the target a *potentially trustworthy* origin? Only those get client
//!   hints, fetch metadata and the modern content codings.
//! - What `Referer` does a page send to this target?
//!
//! A *site* is a scheme plus a registrable domain (the public suffix and one
//! label), so `www.example.co.uk` and `api.example.co.uk` are one site and
//! `a.github.io` and `b.github.io` are two. The public suffixes come from the
//! Public Suffix List compiled into the `psl` crate.

use std::net::{Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

/// How a request's target relates to the origin that initiated it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Relation {
    /// Same scheme, host and port.
    SameOrigin,
    /// Same scheme and registrable domain, another host or port.
    SameSite,
    /// Anything else.
    CrossSite,
}

/// The lowercase host of a URL without the brackets of an IPv6 literal and
/// without a trailing dot.
pub fn host_of(url: &Url) -> Option<String> {
    match url.host()? {
        Host::Domain(name) => {
            let name = name.strip_suffix('.').unwrap_or(name);
            (!name.is_empty()).then(|| name.to_ascii_lowercase())
        }
        Host::Ipv4(address) => Some(address.to_string()),
        Host::Ipv6(address) => Some(address.to_string()),
    }
}

/// Whether a host is a literal IP address.
pub fn is_ip_literal(host: &str) -> bool {
    host.parse::<Ipv4Addr>().is_ok() || host.parse::<Ipv6Addr>().is_ok()
}

/// The registrable domain of a host: its public suffix plus one label. An
/// IP literal is its own site. `None` when the host is itself a public
/// suffix (`co.uk`, `github.io`), which belongs to no one site.
pub fn registrable_domain(host: &str) -> Option<String> {
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    if is_ip_literal(host) {
        return Some(host.to_owned());
    }
    let host = host.to_ascii_lowercase();
    psl::domain_str(&host).map(str::to_owned)
}

/// Whether a domain is a public suffix, under which anyone can register:
/// `com`, `co.uk`, `github.io`. A cookie may never be set for one.
pub fn is_public_suffix(domain: &str) -> bool {
    let domain = domain
        .strip_suffix('.')
        .unwrap_or(domain)
        .to_ascii_lowercase();
    if domain.is_empty() || is_ip_literal(&domain) {
        return false;
    }
    psl::suffix_str(&domain).is_some_and(|suffix| suffix == domain)
}

/// Whether two URLs have the same origin: scheme, host and port.
pub fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && host_of(a).is_some()
        && host_of(a) == host_of(b)
        && a.port_or_known_default() == b.port_or_known_default()
}

/// Whether two URLs belong to the same site: same scheme and same
/// registrable domain. A host that is a public suffix is a site by itself.
pub fn same_site(a: &Url, b: &Url) -> bool {
    if a.scheme() != b.scheme() {
        return false;
    }
    let (Some(host_a), Some(host_b)) = (host_of(a), host_of(b)) else {
        return false;
    };
    if host_a == host_b {
        return true;
    }
    match (registrable_domain(&host_a), registrable_domain(&host_b)) {
        (Some(site_a), Some(site_b)) => site_a == site_b,
        _ => false,
    }
}

/// How `target` relates to `initiator`.
pub fn relation(initiator: &Url, target: &Url) -> Relation {
    if same_origin(initiator, target) {
        Relation::SameOrigin
    } else if same_site(initiator, target) {
        Relation::SameSite
    } else {
        Relation::CrossSite
    }
}

/// Whether a URL's origin is *potentially trustworthy*: `https`, or plain
/// `http` to this machine (`localhost`, `*.localhost`, `127.0.0.0/8`,
/// `::1`). Browsers send client hints, fetch metadata and the `br` and
/// `zstd` codings only to such origins.
pub fn is_trustworthy(url: &Url) -> bool {
    if url.scheme() == "https" {
        return true;
    }
    match url.host() {
        Some(Host::Domain(name)) => {
            let name = name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost")
        }
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

/// The origin of a URL as it is written into `Origin` and into a trimmed
/// `Referer`: `https://example.org`, with the port only when it is not the
/// scheme's default.
pub fn serialize_origin(url: &Url) -> String {
    let host = match url.host() {
        Some(Host::Ipv6(address)) => format!("[{address}]"),
        Some(Host::Ipv4(address)) => address.to_string(),
        Some(Host::Domain(name)) => name.to_ascii_lowercase(),
        None => String::new(),
    };
    match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    }
}

/// The `Referer` a page at `referrer` sends to `target` under the default
/// policy of every current browser, `strict-origin-when-cross-origin`:
///
/// - same origin: the page's URL without fragment and credentials;
/// - another origin: the page's origin only, with a trailing slash;
/// - from `https` down to plain `http`: nothing.
pub fn referer_for(referrer: &Url, target: &Url) -> Option<String> {
    if !matches!(referrer.scheme(), "http" | "https") {
        return None;
    }
    if referrer.scheme() == "https" && target.scheme() == "http" && !is_trustworthy(target) {
        return None;
    }
    if same_origin(referrer, target) {
        let mut full = referrer.clone();
        full.set_fragment(None);
        // A URL with credentials never reaches this point (the SSRF guard
        // refuses it), but a Referer must not carry them either way.
        let _ = full.set_username("");
        let _ = full.set_password(None);
        Some(full.to_string())
    } else {
        Some(format!("{}/", serialize_origin(referrer)))
    }
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{
        Relation, host_of, is_ip_literal, is_public_suffix, is_trustworthy, referer_for,
        registrable_domain, relation, same_origin, same_site, serialize_origin,
    };

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn registrable_domains_follow_the_public_suffix_list() {
        for (host, expected) in [
            ("example.org", Some("example.org")),
            ("www.example.org", Some("example.org")),
            ("a.b.c.example.org", Some("example.org")),
            ("WWW.Example.ORG", Some("example.org")),
            ("example.org.", Some("example.org")),
            ("www.example.co.uk", Some("example.co.uk")),
            ("example.co.uk", Some("example.co.uk")),
            // Private suffixes: every customer is a site of their own.
            ("alice.github.io", Some("alice.github.io")),
            ("pages.alice.github.io", Some("alice.github.io")),
            // A public suffix by itself belongs to no site.
            ("co.uk", None),
            ("com", None),
            ("github.io", None),
            // An unlisted top-level domain follows the default rule.
            ("site.kage.test", Some("kage.test")),
            ("203.0.113.9", Some("203.0.113.9")),
            ("2001:db8::7", Some("2001:db8::7")),
            ("", None),
        ] {
            assert_eq!(registrable_domain(host).as_deref(), expected, "{host}");
        }
    }

    #[test]
    fn public_suffixes_are_recognised() {
        for suffix in ["com", "co.uk", "github.io", "CO.UK", "co.uk."] {
            assert!(is_public_suffix(suffix), "{suffix}");
        }
        for domain in [
            "example.com",
            "example.co.uk",
            "alice.github.io",
            "203.0.113.9",
            "",
        ] {
            assert!(!is_public_suffix(domain), "{domain}");
        }
    }

    #[test]
    fn origins_compare_scheme_host_and_port() {
        assert!(same_origin(
            &url("https://example.org/a"),
            &url("https://EXAMPLE.org:443/b?x=1")
        ));
        assert!(!same_origin(
            &url("https://example.org/"),
            &url("http://example.org/")
        ));
        assert!(!same_origin(
            &url("https://example.org/"),
            &url("https://example.org:8443/")
        ));
        assert!(!same_origin(
            &url("https://example.org/"),
            &url("https://www.example.org/")
        ));
        assert!(same_origin(
            &url("http://[2001:db8::7]:8080/"),
            &url("http://[2001:db8::7]:8080/x")
        ));
    }

    #[test]
    fn sites_are_schemeful_registrable_domains() {
        let cases = [
            (
                "https://example.org/",
                "https://example.org/x",
                Relation::SameOrigin,
            ),
            (
                "https://www.example.org/",
                "https://api.example.org/",
                Relation::SameSite,
            ),
            (
                "https://example.org/",
                "https://example.org:8443/",
                Relation::SameSite,
            ),
            (
                "https://www.example.co.uk/",
                "https://shop.example.co.uk/",
                Relation::SameSite,
            ),
            (
                "https://example.org/",
                "https://example.com/",
                Relation::CrossSite,
            ),
            // Another scheme is another site.
            (
                "http://example.org/",
                "https://example.org/",
                Relation::CrossSite,
            ),
            // Two customers of one private suffix are two sites.
            (
                "https://alice.github.io/",
                "https://bob.github.io/",
                Relation::CrossSite,
            ),
            (
                "https://a.alice.github.io/",
                "https://b.alice.github.io/",
                Relation::SameSite,
            ),
            (
                "https://www.example.co.uk/",
                "https://www.other.co.uk/",
                Relation::CrossSite,
            ),
            (
                "https://203.0.113.9/",
                "https://203.0.113.10/",
                Relation::CrossSite,
            ),
            (
                "https://203.0.113.9/",
                "https://203.0.113.9:8443/",
                Relation::SameSite,
            ),
        ];
        for (initiator, target, expected) in cases {
            assert_eq!(
                relation(&url(initiator), &url(target)),
                expected,
                "{initiator} -> {target}"
            );
        }
        // A public suffix used as a host is only ever the same site as itself.
        assert!(same_site(
            &url("https://github.io/"),
            &url("https://github.io/x")
        ));
        assert!(!same_site(
            &url("https://github.io/"),
            &url("https://alice.github.io/")
        ));
        assert!(
            Relation::SameOrigin < Relation::SameSite && Relation::SameSite < Relation::CrossSite
        );
    }

    #[test]
    fn only_https_and_this_machine_are_trustworthy() {
        for text in [
            "https://example.org/",
            "http://localhost/",
            "http://localhost:8080/",
            "http://app.localhost/",
            "http://127.0.0.1:3000/",
            "http://127.8.9.10/",
            "http://[::1]:3000/",
        ] {
            assert!(is_trustworthy(&url(text)), "{text}");
        }
        for text in [
            "http://example.org/",
            "http://site.kage.test/",
            "http://notlocalhost/",
            "http://localhost.example.org/",
            "http://192.168.1.10/",
            "http://[2001:db8::7]/",
        ] {
            assert!(!is_trustworthy(&url(text)), "{text}");
        }
    }

    #[test]
    fn the_referer_follows_strict_origin_when_cross_origin() {
        let page = url("https://www.example.org/list?page=2#row-7");

        assert_eq!(
            referer_for(&page, &url("https://www.example.org/item/9")).as_deref(),
            Some("https://www.example.org/list?page=2")
        );
        // Same site but another origin: the origin only.
        assert_eq!(
            referer_for(&page, &url("https://api.example.org/v1")).as_deref(),
            Some("https://www.example.org/")
        );
        assert_eq!(
            referer_for(&page, &url("https://other.example.com/")).as_deref(),
            Some("https://www.example.org/")
        );
        // A downgrade sends nothing.
        assert_eq!(
            referer_for(&page, &url("http://www.example.org/item/9")),
            None
        );
        // Plain http pages send as usual, to http and to https.
        let plain = url("http://shop.example.org:8080/cart");
        assert_eq!(
            referer_for(&plain, &url("http://shop.example.org:8080/pay")).as_deref(),
            Some("http://shop.example.org:8080/cart")
        );
        assert_eq!(
            referer_for(&plain, &url("https://pay.example.net/")).as_deref(),
            Some("http://shop.example.org:8080/")
        );
        assert_eq!(referer_for(&url("about:blank"), &page), None);
    }

    #[test]
    fn origins_serialize_without_default_ports() {
        assert_eq!(
            serialize_origin(&url("https://Example.org:443/x")),
            "https://example.org"
        );
        assert_eq!(
            serialize_origin(&url("http://example.org:8080/x")),
            "http://example.org:8080"
        );
        assert_eq!(
            serialize_origin(&url("https://[2001:db8::7]/")),
            "https://[2001:db8::7]"
        );
        assert_eq!(
            serialize_origin(&url("http://203.0.113.9/")),
            "http://203.0.113.9"
        );
    }

    #[test]
    fn hosts_are_normalised() {
        assert_eq!(
            host_of(&url("https://EXAMPLE.org./")).as_deref(),
            Some("example.org")
        );
        assert_eq!(
            host_of(&url("https://[2001:db8::7]/")).as_deref(),
            Some("2001:db8::7")
        );
        assert_eq!(host_of(&url("data:text/plain,hi")), None);
        assert!(is_ip_literal("203.0.113.9"));
        assert!(is_ip_literal("2001:db8::7"));
        assert!(!is_ip_literal("example.org"));
    }
}
