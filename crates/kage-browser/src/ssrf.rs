//! The SSRF guard on the browser path (spec §19): a check of the redirect
//! chain after the navigation.
//!
//! On the http path Kage follows redirects itself and checks every hop
//! before it is requested. A browser follows redirects by itself, and
//! intercepting its requests to check them first would add an automation
//! fingerprint (and, with this driver, switch the HTTP cache off). So the
//! browser path is guarded in two other ways:
//!
//! - the engines launch with a bypass list that sends loopback through the
//!   proxy too, so nothing a page or a redirect names can reach the
//!   sidecar's own network: every request leaves at the proxy's exit;
//! - when the navigation is over, [`check_chain`] runs every URL the
//!   browser asked for through the same functions that guard the http path
//!   ([`kage_core::ssrf`]). A refused hop makes the attempt `ssrf_denied`,
//!   and [`drop_captures`] removes what the navigation captured, so nothing
//!   read from a refused address travels any further.
//!
//! The check is by URL: scheme, credentials, port, host name, literal
//! address. The proxy resolves host names, as on the http path, so no
//! resolved address is known here.

use kage_core::ssrf::{RedirectChain, SsrfDenial, SsrfPolicy};
use url::Url;

use crate::contract::PageResult;

/// Schemes a browser shows without asking the network: a document that
/// navigated itself there has not been anywhere.
const LOCAL_SCHEMES: [&str; 5] = ["about", "data", "blob", "javascript", "chrome-error"];

/// A hop of a browser navigation that the guard refuses.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("hop {hop} of the navigation, {url:?}, is refused: {reason}")]
pub struct DeniedHop {
    /// Which URL of the chain it was: 0 for the URL that was asked for.
    pub hop: usize,
    /// The refused URL.
    pub url: String,
    /// Why it is refused.
    pub reason: SsrfDenial,
}

impl DeniedHop {
    /// The evidence string for the block report of the `ssrf_denied` result,
    /// in the form the http path writes.
    pub fn evidence(&self) -> String {
        format!("redirect:{}:{} ({})", self.hop, self.url, self.reason)
    }
}

/// Whether two URLs name the same document: equal but for the fragment.
fn same_document(a: &Url, b: &Url) -> bool {
    a.as_str().split('#').next() == b.as_str().split('#').next()
}

/// Checks every URL of a browser navigation against the SSRF guard: the URL
/// that was asked for, the target of every redirect, and the URL of the
/// captured document when a script moved the page on afterwards.
///
/// # Errors
///
/// The first refused hop.
pub fn check_chain(
    result: &PageResult,
    policy: SsrfPolicy,
    max_redirects: usize,
) -> Result<(), DeniedHop> {
    let first = Url::parse(&result.url).map_err(|_| DeniedHop {
        hop: 0,
        url: result.url.clone(),
        reason: SsrfDenial::InvalidUrl(result.url.clone()),
    })?;
    let mut chain =
        RedirectChain::start(&first, policy, max_redirects).map_err(|reason| DeniedHop {
            hop: 0,
            url: result.url.clone(),
            reason,
        })?;
    let mut hop = 0;
    let mut step = |target: &str| -> Result<(), DeniedHop> {
        let Ok(next) = chain.current().join(target.trim()) else {
            return Err(DeniedHop {
                hop: hop + 1,
                url: target.to_owned(),
                reason: SsrfDenial::InvalidUrl(target.to_owned()),
            });
        };
        // A local document is not a network address, and the URL the chain
        // already points at is not a new hop.
        if LOCAL_SCHEMES.contains(&next.scheme()) || same_document(chain.current(), &next) {
            return Ok(());
        }
        hop += 1;
        chain
            .follow(next.as_str())
            .map(|_| ())
            .map_err(|refused| DeniedHop {
                hop,
                url: next.to_string(),
                reason: refused.reason,
            })
    };
    for redirect in &result.redirects {
        step(&redirect.url)?;
        if let Some(location) = &redirect.location {
            step(location)?;
        }
    }
    step(&result.final_url)
}

/// Removes everything a navigation captured, and keeps what describes it:
/// the URLs, the status, the redirect chain, the timing and the error. Used
/// when a hop was refused: the result is kept for the record, its content is
/// not.
pub fn drop_captures(result: &mut PageResult) {
    result.headers.clear();
    result.title = None;
    result.dom_html = None;
    result.dom_truncated = false;
    result.raw_body = None;
    result.xhr.clear();
    result.xhr_dropped = 0;
    result.console.clear();
    result.screenshot = None;
    result.pdf = None;
}

#[cfg(test)]
mod tests {
    use kage_core::ssrf::{DEFAULT_MAX_REDIRECTS, DeniedRange, SsrfDenial, SsrfPolicy};

    use super::{check_chain, drop_captures};
    use crate::contract::{PageResult, RedirectHop};

    fn result(url: &str, redirects: &[(&str, &str)], final_url: &str) -> PageResult {
        let examples: serde_json::Value =
            serde_json::from_str(crate::contract::EXAMPLES_JSON).unwrap();
        let mut result: PageResult =
            serde_json::from_value(examples["PageResult"][0].clone()).unwrap();
        url.clone_into(&mut result.url);
        final_url.clone_into(&mut result.final_url);
        result.redirects = redirects
            .iter()
            .map(|(from, location)| RedirectHop {
                url: (*from).to_owned(),
                status: 302,
                location: Some((*location).to_owned()),
            })
            .collect();
        result
    }

    fn check(
        url: &str,
        redirects: &[(&str, &str)],
        final_url: &str,
    ) -> Result<(), super::DeniedHop> {
        check_chain(
            &result(url, redirects, final_url),
            SsrfPolicy::default(),
            DEFAULT_MAX_REDIRECTS,
        )
    }

    #[test]
    fn a_public_chain_passes() {
        assert_eq!(
            check("https://example.org/", &[], "https://example.org/"),
            Ok(())
        );
        assert_eq!(
            check(
                "https://example.org/a",
                &[
                    ("https://example.org/a", "/b"),
                    ("https://example.org/b", "https://www.example.org/c"),
                ],
                "https://www.example.org/c",
            ),
            Ok(())
        );
        // A script moved the page on after the redirects.
        assert_eq!(
            check(
                "https://example.org/a",
                &[("https://example.org/a", "/b")],
                "https://shop.example.org/landing",
            ),
            Ok(())
        );
    }

    #[test]
    fn a_hop_to_a_denied_address_is_named_with_its_place_in_the_chain() {
        let denied = check(
            "https://example.org/a",
            &[
                (
                    "https://example.org/a",
                    "http://169.254.169.254/latest/meta-data/",
                ),
                ("http://169.254.169.254/latest/meta-data/", "/x"),
            ],
            "http://169.254.169.254/x",
        )
        .unwrap_err();
        assert_eq!(denied.hop, 1);
        assert_eq!(denied.url, "http://169.254.169.254/latest/meta-data/");
        assert!(
            matches!(
                denied.reason,
                SsrfDenial::IpLiteral(_)
                    | SsrfDenial::Address {
                        range: DeniedRange::CloudMetadata,
                        ..
                    }
            ),
            "{:?}",
            denied.reason
        );
        assert!(
            denied
                .evidence()
                .starts_with("redirect:1:http://169.254.169.254/")
        );
        assert!(denied.to_string().contains("hop 1"));
    }

    #[test]
    fn loopback_private_and_local_names_are_refused_wherever_they_appear() {
        for target in [
            "http://127.0.0.1:8090/v1/health",
            "http://localhost/",
            "http://10.0.0.5/",
            "http://[::1]/",
            "http://192.168.1.1/admin",
            "http://browser-chromium:8090/",
        ] {
            // As the final URL only (a script went there) ...
            let last = check("https://example.org/", &[], target);
            // ... and as a redirect target.
            let middle = check(
                "https://example.org/",
                &[("https://example.org/", target)],
                "https://example.org/after",
            );
            if target.contains("browser-chromium") {
                // A bare host name is a name the proxy resolves on its own
                // side; the guard has no address to judge it by.
                continue;
            }
            assert_eq!(last.unwrap_err().hop, 1, "{target}");
            assert_eq!(middle.unwrap_err().hop, 1, "{target}");
        }
    }

    #[test]
    fn the_first_url_is_checked_too() {
        let denied = check("http://127.0.0.1/", &[], "http://127.0.0.1/").unwrap_err();
        assert_eq!(denied.hop, 0);
        let denied = check("ftp://example.org/", &[], "ftp://example.org/").unwrap_err();
        assert!(matches!(denied.reason, SsrfDenial::Scheme(_)));
        let denied = check("not a url", &[], "not a url").unwrap_err();
        assert!(matches!(denied.reason, SsrfDenial::InvalidUrl(_)));
        let denied = check(
            "https://user:secret@example.org/",
            &[],
            "https://example.org/",
        )
        .unwrap_err();
        assert_eq!(denied.reason, SsrfDenial::Credentials);
    }

    #[test]
    fn a_page_that_ended_on_a_local_document_has_not_been_anywhere() {
        for local in [
            "about:blank",
            "chrome-error://chromewebdata/",
            "data:text/html,hello",
            "blob:https://example.org/3f1c",
        ] {
            assert_eq!(check("https://example.org/", &[], local), Ok(()), "{local}");
        }
        // But a navigation cannot start at one.
        assert!(check("about:blank", &[], "about:blank").is_err());
        // And what follows a local document is still checked.
        let mut odd = result("https://example.org/", &[], "http://127.0.0.1/");
        odd.redirects = vec![RedirectHop {
            url: "about:blank".to_owned(),
            status: 302,
            location: None,
        }];
        assert_eq!(
            check_chain(&odd, SsrfPolicy::default(), DEFAULT_MAX_REDIRECTS)
                .unwrap_err()
                .hop,
            1
        );
    }

    #[test]
    fn a_literal_address_follows_the_policy_and_the_ranges_hold_either_way() {
        let page = result("https://93.184.216.34/", &[], "https://93.184.216.34/");
        assert!(check_chain(&page, SsrfPolicy::default(), 10).is_err());
        let allow = SsrfPolicy {
            allow_ip_targets: true,
        };
        assert_eq!(check_chain(&page, allow, 10), Ok(()));
        let private = result("https://10.1.2.3/", &[], "https://10.1.2.3/");
        assert!(check_chain(&private, allow, 10).is_err());
    }

    #[test]
    fn more_hops_than_the_limit_are_refused() {
        let hops: Vec<(String, String)> = (0..4)
            .map(|index| {
                (
                    format!("https://example.org/{index}"),
                    format!("/{}", index + 1),
                )
            })
            .collect();
        let hops: Vec<(&str, &str)> = hops
            .iter()
            .map(|(from, to)| (from.as_str(), to.as_str()))
            .collect();
        let page = result("https://example.org/0", &hops, "https://example.org/4");
        assert_eq!(check_chain(&page, SsrfPolicy::default(), 4), Ok(()));
        let denied = check_chain(&page, SsrfPolicy::default(), 3).unwrap_err();
        assert_eq!(denied.reason, SsrfDenial::TooManyRedirects(3));
        assert_eq!(denied.hop, 4);
    }

    #[test]
    fn dropping_the_captures_keeps_the_record_and_none_of_the_content() {
        let mut page = result(
            "https://example.org/",
            &[("https://example.org/", "http://127.0.0.1/")],
            "http://127.0.0.1/",
        );
        assert!(page.dom_html.is_some() && !page.xhr.is_empty() && page.screenshot.is_some());
        drop_captures(&mut page);
        assert_eq!(page.dom_html, None);
        assert_eq!(page.raw_body, None);
        assert_eq!(page.title, None);
        assert!(page.headers.is_empty() && page.xhr.is_empty() && page.console.is_empty());
        assert_eq!((page.screenshot, page.pdf), (None, None));
        // What happened is still told.
        assert_eq!(page.final_url, "http://127.0.0.1/");
        assert_eq!(page.redirects.len(), 1);
        assert_eq!(page.status, Some(200));
    }
}
