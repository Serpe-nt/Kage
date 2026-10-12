//! What a site is, for a crawl that stays on one (spec §13).
//!
//! A site is a registrable domain as the Public Suffix List defines it:
//! `www.example.org` and `blog.example.org` are one site, `example.co.uk`
//! and `other.co.uk` are two, and so are two customers of a host that lets
//! anyone register a name under it (`alice.github.io`, `bob.github.io`).
//! The list is compiled in at a pinned version (`docs/versions.md`); no
//! lookup is made.
//!
//! Unlike the `Sec-Fetch-Site` of a request, the site of a crawl does not
//! depend on the scheme: a site that answers on `http` and on `https` is one
//! site to its reader.

use std::net::IpAddr;

use url::Url;

/// The site a host belongs to: its registrable domain in lower case. A
/// host that has none is its own site: an address, a name of one label
/// (`localhost`), a public suffix by itself.
pub fn of_host(host: &str) -> String {
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    let bare = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(&host);
    if bare.parse::<IpAddr>().is_ok() {
        return bare.to_owned();
    }
    psl::domain_str(&host).map_or_else(|| host.clone(), str::to_owned)
}

/// The site of a URL; `None` for a URL without a host.
pub fn of(url: &Url) -> Option<String> {
    url.host_str().map(of_host)
}

/// Whether two URLs are pages of one site.
pub fn same(a: &Url, b: &Url) -> bool {
    match (of(a), of(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{of, of_host, same};

    #[test]
    fn a_site_is_a_registrable_domain() {
        for (host, site) in [
            ("example.org", "example.org"),
            ("www.example.org", "example.org"),
            ("a.b.c.example.org", "example.org"),
            ("WWW.Example.ORG", "example.org"),
            ("example.org.", "example.org"),
            ("shop.example.co.uk", "example.co.uk"),
            // Under a suffix anyone can register below, every name is a
            // site of its own.
            ("alice.github.io", "alice.github.io"),
            ("pages.alice.github.io", "alice.github.io"),
            // A top-level domain the list does not know follows its default
            // rule: the last two labels.
            ("fixture.kage.test", "kage.test"),
            ("lenient.kage.test", "kage.test"),
            ("offsite.example.test", "example.test"),
        ] {
            assert_eq!(of_host(host), site, "{host}");
        }
    }

    #[test]
    fn a_host_without_a_registrable_domain_is_its_own_site() {
        for (host, site) in [
            ("localhost", "localhost"),
            ("co.uk", "co.uk"),
            ("github.io", "github.io"),
            ("203.0.113.9", "203.0.113.9"),
            ("[2001:db8::7]", "2001:db8::7"),
            ("2001:db8::7", "2001:db8::7"),
            ("", ""),
        ] {
            assert_eq!(of_host(host), site, "{host}");
        }
    }

    #[test]
    fn the_scheme_and_the_port_are_no_part_of_a_site() {
        let url = |text: &str| Url::parse(text).unwrap();
        assert!(same(
            &url("http://www.example.org:8080/a"),
            &url("https://blog.example.org/b")
        ));
        assert!(!same(
            &url("https://example.org/"),
            &url("https://example.net/")
        ));
        assert!(!same(
            &url("https://alice.github.io/"),
            &url("https://bob.github.io/")
        ));
        assert!(same(
            &url("http://203.0.113.9/a"),
            &url("http://203.0.113.9:81/b")
        ));
        assert!(!same(
            &url("http://203.0.113.9/"),
            &url("http://203.0.113.10/")
        ));
        assert_eq!(
            of(&url("https://[2001:db8::7]:8443/")).as_deref(),
            Some("2001:db8::7")
        );
        // A URL without a host belongs to no site.
        assert_eq!(of(&url("mailto:someone@example.org")), None);
        assert!(!same(
            &url("mailto:someone@example.org"),
            &url("mailto:someone@example.org")
        ));
    }
}
