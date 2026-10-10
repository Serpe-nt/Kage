//! What an attempt means once it is over: the classifier's verdict, and
//! what follows from it for the persona on the domain and for the domain's
//! breaker (spec §6.3, §8.3, §12).
//!
//! One place does this for every attempt, whoever made it: the fetch
//! pipeline for real work, and the gate for the persona manager's warm-up
//! visits. A block cools the persona for the domain, for the policy's
//! cooldown scaled by the confidence of the signature that matched, and
//! counts once on the domain's breaker.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use kage_browser::contract::{NavigationErrorCode, PageResult};
use kage_browser::{BrowserError, Navigation};
use kage_core::alert::{Alert, AlertKind, AlertSeverity, AlertSink};
use kage_core::classifier::{BrowserSignals, Classification, Observation, ObservedCookie};
use kage_core::domain::{Header, Persona, Render};
use kage_core::ids::PersonaId;
use kage_core::outcome::{CooldownClass, Outcome};
use kage_core::policy::PolicyStore;
use kage_core::schedule::breaker::{BreakerChange, BreakerRules};
use kage_core::schedule::politeness::Courtesy;
use kage_core::schedule::retry::{AttemptVerdict, CooldownRules, cooldown_for, cooldown_seed};
use kage_core::seed::Seed;
use kage_persona::Judged;
use kage_proxy::Clock;

use crate::SchedulerError;
use crate::backend::Backend;
use crate::metrics;

/// What the classifier and the rules around it need to judge and settle
/// an attempt.
pub struct Verdicts {
    backend: Arc<dyn Backend>,
    policies: Arc<PolicyStore>,
    cooldowns: CooldownRules,
    breaker_max_pause: Duration,
    breaker_trial_timeout: Duration,
    alerts: Arc<dyn AlertSink>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for Verdicts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verdicts")
            .field("cooldowns", &self.cooldowns)
            .field("breaker_max_pause", &self.breaker_max_pause)
            .finish_non_exhaustive()
    }
}

/// What was done about an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settled {
    /// How long the persona stays away from the domain, when it does.
    pub cooldown: Option<Duration>,
    /// What the domain's breaker did.
    pub breaker: BreakerChange,
}

/// An attempt that ended without the site's answer: an outcome with the
/// hints the outcome implies, and the reason as its evidence.
pub fn unanswered(outcome: Outcome, reason: &str) -> Classification {
    Classification {
        outcome,
        block: None,
        vendors: Vec::new(),
        rule: None,
        retryable: outcome.is_retryable(),
        escalate: false,
        cooldown: CooldownClass::None,
        evidence: vec![format!("failure:{reason}")],
    }
}

/// The cookies a response set, by its `Set-Cookie` headers.
pub fn cookies_set(headers: &[Header]) -> Vec<ObservedCookie> {
    headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("set-cookie"))
        .filter_map(|header| {
            let pair = header.value.split(';').next()?;
            let (name, value) = pair.split_once('=')?;
            let name = name.trim();
            (!name.is_empty()).then(|| ObservedCookie {
                name: name.to_owned(),
                value: value.trim().to_owned(),
            })
        })
        .collect()
}

/// What the classifier looks at for a page a browser loaded; `None` when
/// no response arrived.
pub fn observe_page(page: &PageResult) -> Option<Observation> {
    let status = u16::try_from(page.status?).ok()?;
    let headers: Vec<Header> = page
        .headers
        .iter()
        .map(|header| Header {
            name: header.name.clone(),
            value: header.value.clone(),
        })
        .collect();
    let body = page
        .dom_html
        .clone()
        .or_else(|| page.raw_body.as_ref().map(|body| body.data.clone()))
        .unwrap_or_default();
    Some(Observation {
        render: Render::Browser,
        status,
        url: page.final_url.clone(),
        cookies: cookies_set(&headers),
        headers,
        body,
        browser: Some(BrowserSignals {
            script_hosts: page.challenge.script_hosts.clone(),
            iframe_hosts: page.challenge.iframe_hosts.clone(),
            interstitial_text: page.challenge.marker.clone(),
            interstitial_detected: page.challenge.interstitial,
            interstitial_resolved: page.challenge.resolved == Some(true),
        }),
    })
}

/// What a navigation came to, before the classifier: the reason it failed
/// when it did, and what that says about the site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The outcome.
    pub outcome: Outcome,
    /// A stable name for why.
    pub reason: &'static str,
    /// What it says about the site's health.
    pub courtesy: Courtesy,
}

/// Why a page did not arrive, when it did not.
pub fn page_failure(page: &PageResult) -> Option<Failure> {
    let error = page.error.as_ref()?;
    Some(match error.code {
        NavigationErrorCode::Timeout => Failure {
            outcome: Outcome::Timeout,
            reason: "timeout",
            courtesy: Courtesy::ServerError,
        },
        // The proxy, not the site.
        NavigationErrorCode::ProxyFailed => Failure {
            outcome: Outcome::ProviderError,
            reason: "proxy_failed",
            courtesy: Courtesy::NotTheSite,
        },
        // The site did not answer: it counts as a server error.
        NavigationErrorCode::NavigationFailed => Failure {
            outcome: Outcome::ProviderError,
            reason: "navigation_failed",
            courtesy: Courtesy::ServerError,
        },
        // A download, a closed page, an entry that found no link.
        NavigationErrorCode::Aborted => Failure {
            outcome: Outcome::ProviderError,
            reason: "aborted",
            courtesy: Courtesy::NotTheSite,
        },
    })
}

/// What an answer says about the site's health.
pub const fn courtesy_of(outcome: Outcome, status: Option<u16>) -> Courtesy {
    match (outcome, status) {
        (Outcome::Timeout, _) | (Outcome::ProviderError, Some(500..=599)) => Courtesy::ServerError,
        (Outcome::ProviderError | Outcome::SsrfDenied | Outcome::RobotsDenied, _) => {
            Courtesy::NotTheSite
        }
        _ => Courtesy::Answered,
    }
}

impl Verdicts {
    /// Verdicts over the policies in force.
    pub fn new(
        backend: Arc<dyn Backend>,
        policies: Arc<PolicyStore>,
        cooldowns: CooldownRules,
        breaker_max_pause: Duration,
        breaker_trial_timeout: Duration,
        alerts: Arc<dyn AlertSink>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            backend,
            policies,
            cooldowns,
            breaker_max_pause,
            breaker_trial_timeout,
            alerts,
            clock,
        }
    }

    /// The policies in force.
    pub const fn policies(&self) -> &Arc<PolicyStore> {
        &self.policies
    }

    /// The classifier's verdict with the signatures in force.
    pub fn classify(&self, observation: &Observation) -> Classification {
        self.policies.current().signatures.classify(observation)
    }

    /// The rules of a domain's breaker: its policy's threshold and first
    /// pause, the scheduler's longest pause.
    pub fn breaker_rules(&self, domain: &str) -> BreakerRules {
        let policy = self.policies.current().domains.resolve(domain);
        BreakerRules {
            threshold: policy.breaker_threshold,
            pause: policy.breaker_pause.get(),
            max_pause: self.breaker_max_pause,
            trial_timeout: self.breaker_trial_timeout,
        }
    }

    /// How long a persona stays away from a domain for a block.
    pub fn cooldown(
        &self,
        persona: &Seed,
        domain: &str,
        verdict: &AttemptVerdict,
        at: DateTime<Utc>,
    ) -> Option<Duration> {
        let policy = self.policies.current().domains.resolve(domain);
        cooldown_for(
            &policy.cooldowns,
            verdict,
            &self.cooldowns,
            &cooldown_seed(persona, domain, at),
        )
    }

    /// Applies what an attempt means. `outcome` is how it ended; `penalty`
    /// is the block the persona answers for, when it does (spec §8.3; an
    /// http block that its own escalation cured is none). `trial` says
    /// that the attempt was the one trial of a half-open breaker.
    ///
    /// # Errors
    ///
    /// Fails when the backend fails.
    pub async fn settle(
        &self,
        persona: (PersonaId, &Seed),
        domain: &str,
        outcome: Outcome,
        penalty: Option<&AttemptVerdict>,
        trial: bool,
        breaker: &BreakerRules,
    ) -> Result<Settled, SchedulerError> {
        let at = self.clock.now();
        let (id, seed) = persona;
        let cooldown = penalty.and_then(|verdict| self.cooldown(seed, domain, verdict, at));
        // What the persona answers for is what its state remembers.
        let recorded = penalty.map_or(outcome, |verdict| verdict.outcome);
        self.backend
            .record_outcome(id, domain, recorded, cooldown, at)
            .await?;
        let change = if penalty.is_some() {
            let change = self.backend.breaker_block(domain, breaker, at).await?;
            if let BreakerChange::Opened { until } = change {
                self.alerts.raise(
                    Alert::new(
                        AlertKind::DomainPaused,
                        AlertSeverity::Warning,
                        format!(
                            "{domain} turned {} attempts away within an hour: it is paused \
                             until {until} and every request for it is answered at once",
                            breaker.threshold
                        ),
                        serde_json::json!({
                            "domain": domain,
                            "paused_until": until,
                            "threshold": breaker.threshold,
                        }),
                    )
                    .at(at),
                );
            }
            change
        } else if trial && outcome.is_success() {
            let change = self.backend.breaker_success(domain, at).await?;
            if change == BreakerChange::Closed {
                tracing::info!(%domain, "the domain answers again: its breaker closed");
            }
            change
        } else {
            BreakerChange::Unchanged
        };
        match change {
            BreakerChange::Opened { .. } => metrics::breaker(domain, true),
            BreakerChange::Closed => metrics::breaker(domain, false),
            BreakerChange::Unchanged => {}
        }
        Ok(Settled {
            cooldown,
            breaker: change,
        })
    }

    /// Judges what a warm-up visit found and applies it like any attempt.
    ///
    /// # Errors
    ///
    /// Fails when the backend fails.
    pub async fn judge_warmup(
        &self,
        persona: &Persona,
        domain: &str,
        navigation: Result<&Navigation, &BrowserError>,
    ) -> Result<Judged, SchedulerError> {
        let (classification, status) = match navigation {
            Err(_) => (unanswered(Outcome::ProviderError, "sidecar"), None),
            Ok(Navigation::SsrfDenied { .. }) => (unanswered(Outcome::SsrfDenied, "ssrf"), None),
            Ok(Navigation::Page(page)) => {
                let status = page.status.and_then(|status| u16::try_from(status).ok());
                (self.judge_page(page), status)
            }
        };
        let verdict = AttemptVerdict::of(&classification);
        let penalty = classification.outcome.is_block().then_some(&verdict);
        self.settle(
            (persona.id, &persona.seed),
            domain,
            classification.outcome,
            penalty,
            false,
            &self.breaker_rules(domain),
        )
        .await?;
        Ok(Judged {
            outcome: classification.outcome,
            status,
        })
    }

    /// The verdict on a page a browser loaded, or failed to load.
    pub fn judge_page(&self, page: &PageResult) -> Classification {
        let seen = observe_page(page).map(|observation| self.classify(&observation));
        match (page_failure(page), seen) {
            // A page that hung after it answered may be a wall that never
            // finishes: what was seen decides when it is a block.
            (Some(_), Some(seen)) if seen.outcome.is_block() => seen,
            (Some(failure), _) => unanswered(failure.outcome, failure.reason),
            (None, Some(seen)) => seen,
            (None, None) => unanswered(Outcome::ProviderError, "no_response"),
        }
    }
}

#[cfg(test)]
mod tests {
    use kage_browser::contract::{EXAMPLES_JSON, NavigationError, NavigationErrorCode, PageResult};
    use kage_core::domain::{Header, Render};
    use kage_core::outcome::Outcome;
    use kage_core::schedule::politeness::Courtesy;

    use super::{cookies_set, courtesy_of, observe_page, page_failure, unanswered};

    fn page() -> PageResult {
        let examples: serde_json::Value = serde_json::from_str(EXAMPLES_JSON).unwrap();
        let mut page: PageResult =
            serde_json::from_value(examples["PageResult"][0].clone()).unwrap();
        page.error = None;
        page.status = Some(200);
        page
    }

    fn header(name: &str, value: &str) -> Header {
        Header {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }

    #[test]
    fn cookies_are_read_from_the_set_cookie_headers() {
        let cookies = cookies_set(&[
            header("content-type", "text/html"),
            header("Set-Cookie", "__cf_bm=abc.def; Path=/; Secure; HttpOnly"),
            header("set-cookie", " sid = 7 "),
            header("set-cookie", "novalue"),
            header("set-cookie", "=orphan; Path=/"),
        ]);

        let pairs: Vec<(&str, &str)> = cookies
            .iter()
            .map(|cookie| (cookie.name.as_str(), cookie.value.as_str()))
            .collect();
        assert_eq!(pairs, [("__cf_bm", "abc.def"), ("sid", "7")]);
    }

    #[test]
    fn a_page_becomes_what_the_classifier_looks_at() {
        let mut loaded = page();
        loaded.final_url = "https://example.org/after".to_owned();
        loaded.dom_html = Some("<html>the page</html>".to_owned());
        loaded.headers = vec![kage_browser::contract::Header {
            name: "set-cookie".to_owned(),
            value: "datadome=x; Path=/".to_owned(),
        }];
        loaded.challenge.interstitial = true;
        loaded.challenge.resolved = Some(true);
        loaded.challenge.marker = Some("Just a moment".to_owned());
        loaded.challenge.script_hosts = vec!["challenges.cloudflare.com".to_owned()];

        let observation = observe_page(&loaded).unwrap();

        assert_eq!(observation.render, Render::Browser);
        assert_eq!(observation.status, 200);
        assert_eq!(observation.url, "https://example.org/after");
        assert_eq!(observation.body, "<html>the page</html>");
        assert_eq!(observation.cookies[0].name, "datadome");
        let browser = observation.browser.unwrap();
        assert!(browser.interstitial_detected && browser.interstitial_resolved);
        assert_eq!(browser.interstitial_text.as_deref(), Some("Just a moment"));
        assert_eq!(browser.script_hosts, ["challenges.cloudflare.com"]);

        // Without a DOM the raw body is what there is; without either,
        // nothing.
        loaded.dom_html = None;
        let raw = observe_page(&loaded).unwrap().body;
        assert_eq!(
            raw,
            loaded
                .raw_body
                .as_ref()
                .map_or("", |body| body.data.as_str())
        );
        loaded.challenge.resolved = Some(false);
        assert!(
            !observe_page(&loaded)
                .unwrap()
                .browser
                .unwrap()
                .interstitial_resolved
        );
        // No response, or a status that is none: nothing to classify.
        loaded.status = None;
        assert_eq!(observe_page(&loaded), None);
        loaded.status = Some(70_000);
        assert_eq!(observe_page(&loaded), None);
    }

    #[test]
    fn a_navigation_that_did_not_arrive_says_whose_fault_it_was() {
        let failed = |code| {
            let mut page = page();
            page.error = Some(NavigationError {
                code,
                message: "did not arrive".to_owned(),
                net_error: None,
            });
            let failure = page_failure(&page).unwrap();
            (failure.outcome, failure.reason, failure.courtesy)
        };

        assert_eq!(page_failure(&page()), None);
        assert_eq!(
            failed(NavigationErrorCode::Timeout),
            (Outcome::Timeout, "timeout", Courtesy::ServerError)
        );
        assert_eq!(
            failed(NavigationErrorCode::ProxyFailed),
            (Outcome::ProviderError, "proxy_failed", Courtesy::NotTheSite)
        );
        assert_eq!(
            failed(NavigationErrorCode::NavigationFailed),
            (
                Outcome::ProviderError,
                "navigation_failed",
                Courtesy::ServerError
            )
        );
        assert_eq!(
            failed(NavigationErrorCode::Aborted),
            (Outcome::ProviderError, "aborted", Courtesy::NotTheSite)
        );
    }

    #[test]
    fn an_unanswered_attempt_keeps_the_hints_of_its_outcome() {
        let timeout = unanswered(Outcome::Timeout, "timeout");
        assert!(timeout.retryable && !timeout.escalate);
        assert_eq!(timeout.block, None);
        assert_eq!(timeout.evidence, ["failure:timeout"]);
        assert!(!unanswered(Outcome::SsrfDenied, "ssrf").retryable);
    }

    #[test]
    fn only_server_errors_and_timeouts_speak_against_the_sites_health() {
        assert_eq!(courtesy_of(Outcome::Timeout, None), Courtesy::ServerError);
        assert_eq!(
            courtesy_of(Outcome::ProviderError, Some(503)),
            Courtesy::ServerError
        );
        assert_eq!(
            courtesy_of(Outcome::ProviderError, Some(500)),
            Courtesy::ServerError
        );
        assert_eq!(
            courtesy_of(Outcome::ProviderError, Some(599)),
            Courtesy::ServerError
        );
        // A provider error without a status is the proxy's, or Kage's.
        assert_eq!(
            courtesy_of(Outcome::ProviderError, None),
            Courtesy::NotTheSite
        );
        assert_eq!(
            courtesy_of(Outcome::ProviderError, Some(499)),
            Courtesy::NotTheSite
        );
        assert_eq!(courtesy_of(Outcome::SsrfDenied, None), Courtesy::NotTheSite);
        assert_eq!(
            courtesy_of(Outcome::RobotsDenied, None),
            Courtesy::NotTheSite
        );
        for answered in [
            Outcome::Ok,
            Outcome::ChallengePassed,
            Outcome::Forbidden,
            Outcome::RateLimited,
            Outcome::SoftBlock,
        ] {
            assert_eq!(courtesy_of(answered, Some(200)), Courtesy::Answered);
        }
    }
}
