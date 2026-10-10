//! Cookies between a persona's browser and its http jar (spec §6.4).
//!
//! After a browser attempt the cookies of the origin are exported from the
//! persona's context and put into its http jar; before the next browser
//! attempt for an origin, the cookies the http path holds for it are put
//! into the context. Both directions are one rule away from not happening:
//! the persona must have an http path, and the profile it presents there
//! must be the one its engine's major resolves to
//! ([`crate::identity::http_spec`]). A persona whose browser is another
//! major than its profile keeps its cookies in the browser.

use kage_browser::contract::{ExportStateRequest, ImportStateRequest};
use kage_browser::cookies::{from_row, to_row};
use kage_core::domain::{HttpCookie, Persona};
use url::Url;
use uuid::Uuid;

use crate::PersonaError;
use crate::identity::http_spec;
use crate::manager::PersonaManager;

/// What one handoff moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    /// The origin, as `scheme://host[:port]`.
    pub origin: String,
    /// Cookies that arrived.
    pub moved: usize,
    /// Cookies the receiving side refused, as `name (domain): reason`.
    pub rejected: Vec<String>,
}

/// An origin as the sidecar takes it: `http(s)://host[:port]`, and nothing
/// after it.
///
/// # Errors
///
/// [`PersonaError::Origin`] for anything else.
pub fn origin_of(text: &str) -> Result<Url, PersonaError> {
    let refused = || PersonaError::Origin(text.to_owned());
    let url = Url::parse(text).map_err(|_| refused())?;
    let plain = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.path(), "" | "/")
        && url.query().is_none()
        && url.fragment().is_none();
    if plain { Ok(url) } else { Err(refused()) }
}

/// Whether a stored cookie belongs to requests to `origin`: its domain is
/// the host or, for a domain cookie, a parent of it, and a `Secure` cookie
/// only goes to https.
fn sent_to(cookie: &HttpCookie, origin: &Url) -> bool {
    let Some(host) = origin.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let domain = cookie.domain.trim_start_matches('.').to_ascii_lowercase();
    let matches = host == domain
        || (!cookie.host_only && host.ends_with(&format!(".{domain}")) && !domain.is_empty());
    matches && (!cookie.secure || origin.scheme() == "https")
}

impl PersonaManager {
    /// Browser to http: exports the cookies of `origin` from the persona's
    /// open context and puts them into its http jar.
    ///
    /// # Errors
    ///
    /// [`PersonaError::HttpPathOff`] when the persona has no http path or
    /// its profile is not its browser's major: nothing is exported then.
    /// [`PersonaError::Origin`] for an origin that is none; a sidecar or
    /// http error when the export or the jar fails.
    pub async fn browser_to_http(
        &self,
        persona: &Persona,
        context: Uuid,
        origin: &str,
    ) -> Result<Handoff, PersonaError> {
        let url = origin_of(origin)?;
        let spec = http_spec(persona).map_err(|reason| PersonaError::HttpPathOff {
            persona: persona.id,
            reason,
        })?;
        let (_, endpoint) = self.route(persona).await?;
        let client = self.http.client(&spec, &endpoint).await?;
        let exported = self
            .sidecars
            .of(persona.engine)
            .export_state(
                context,
                &ExportStateRequest {
                    include_storage: Some(false),
                    origin: origin.to_owned(),
                },
            )
            .await?;
        let now = self.now();
        let mut moved = 0;
        for cookie in exported.cookies {
            let row = to_row(cookie, now);
            // The browser answers with every cookie it would send to the
            // origin; anything else is not this handoff's.
            if sent_to(&row, &url) {
                client.jar().import(row);
                moved += 1;
            }
        }
        client.persist().await;
        Ok(Handoff {
            origin: origin.to_owned(),
            moved,
            rejected: Vec::new(),
        })
    }

    /// Http to browser: puts the cookies the persona's http jar holds for
    /// `origin` into its open context.
    ///
    /// # Errors
    ///
    /// As [`Self::browser_to_http`]: nothing is imported when the persona
    /// has no http path or its profile is not its browser's major.
    pub async fn http_to_browser(
        &self,
        persona: &Persona,
        context: Uuid,
        origin: &str,
    ) -> Result<Handoff, PersonaError> {
        let url = origin_of(origin)?;
        let spec = http_spec(persona).map_err(|reason| PersonaError::HttpPathOff {
            persona: persona.id,
            reason,
        })?;
        let (_, endpoint) = self.route(persona).await?;
        let client = self.http.client(&spec, &endpoint).await?;
        let cookies: Vec<_> = client
            .jar()
            .snapshot(self.now())
            .iter()
            .filter(|cookie| sent_to(cookie, &url))
            .map(from_row)
            .collect();
        if cookies.is_empty() {
            return Ok(Handoff {
                origin: origin.to_owned(),
                moved: 0,
                rejected: Vec::new(),
            });
        }
        let imported = self
            .sidecars
            .of(persona.engine)
            .import_state(
                context,
                &ImportStateRequest {
                    cookies,
                    origin: origin.to_owned(),
                },
            )
            .await?;
        Ok(Handoff {
            origin: origin.to_owned(),
            moved: usize::try_from(imported.imported).unwrap_or(usize::MAX),
            rejected: imported
                .rejected
                .into_iter()
                .map(|cookie| {
                    format!(
                        "{} ({}): {}",
                        cookie.name,
                        cookie.domain,
                        serde_json::to_value(cookie.reason)
                            .ok()
                            .and_then(|reason| reason.as_str().map(str::to_owned))
                            .unwrap_or_default()
                    )
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use kage_core::domain::HttpCookie;

    use super::{origin_of, sent_to};
    use crate::PersonaError;

    fn cookie(domain: &str, host_only: bool, secure: bool) -> HttpCookie {
        HttpCookie {
            domain: domain.to_owned(),
            host_only,
            path: "/".to_owned(),
            name: "sid".to_owned(),
            value: "1".to_owned(),
            secure,
            http_only: true,
            same_site: None,
            expires_at: None,
            created_at: Utc::now(),
            last_used_at: Utc::now(),
        }
    }

    #[test]
    fn an_origin_is_a_scheme_a_host_and_nothing_else() {
        for fine in [
            "https://shop.example",
            "https://shop.example/",
            "http://shop.example:8080",
        ] {
            assert_eq!(origin_of(fine).unwrap().host_str(), Some("shop.example"));
        }
        for refused in [
            "shop.example",
            "ftp://shop.example",
            "https://shop.example/cart",
            "https://shop.example/?a=1",
            "https://shop.example/#top",
            "https://user:pw@shop.example",
            "https://",
            "",
        ] {
            assert!(
                matches!(origin_of(refused), Err(PersonaError::Origin(text)) if text == refused),
                "{refused}"
            );
        }
    }

    #[test]
    fn only_the_cookies_of_the_origin_are_handed_over() {
        let https = origin_of("https://www.shop.example").unwrap();
        let http = origin_of("http://www.shop.example").unwrap();

        // The host's own cookie, and a domain cookie of a parent.
        assert!(sent_to(&cookie("www.shop.example", true, false), &https));
        assert!(sent_to(&cookie("shop.example", false, true), &https));
        assert!(sent_to(&cookie(".Shop.Example", false, false), &https));
        // A host-only cookie of the parent stays with the parent.
        assert!(!sent_to(&cookie("shop.example", true, false), &https));
        // Another site's, and one that only ends the same way.
        assert!(!sent_to(&cookie("other.example", false, false), &https));
        assert!(!sent_to(&cookie("op.example", false, false), &https));
        // A sibling's host cookie.
        assert!(!sent_to(&cookie("api.shop.example", false, false), &https));
        // A secure cookie never goes to plain http.
        assert!(!sent_to(&cookie("www.shop.example", true, true), &http));
        assert!(sent_to(&cookie("www.shop.example", true, false), &http));
        // A cookie without a domain matches nothing.
        assert!(!sent_to(&cookie("", false, false), &https));
    }
}
