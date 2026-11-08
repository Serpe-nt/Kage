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
    // A page captured without its document (a flow's screenshot) has
    // nothing to judge, nor has an answer of no content (F86, F116). A 304
    // is no such answer here: the tab shows the page the browser kept
    // (F178), a document like any other.
    let captured = page
        .dom_html
        .clone()
        .or_else(|| page.raw_body.as_ref().map(|body| body.data.clone()));
    let document = captured.is_some() && !matches!(status, 204 | 205);
    Some(Observation {
        render: Render::Browser,
        status,
        url: page.final_url.clone(),
        cookies: cookies_set(&headers),
        headers,
        body: captured.unwrap_or_default(),
        document,
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

/// What a navigation the site answered without a page to show came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoDocument {
    /// No content (204, 205), or a `304` the browser kept no page for: the
    /// browser stays on the page it showed. The site answered, as it
    /// answers a level 0 fetch (F116, QA round three class-02).
    Empty,
    /// A file to save: the browser saved it and showed nothing, from a
    /// typed address (`Content-Disposition: attachment`) or from a click
    /// that led to no page (spec §10; F120).
    File,
}

/// Whether a navigation the browser broke off was the site's answer that
/// holds no page: no content, or a file to save. Such an answer is no
/// failure of the persona or of its path, and another persona would get
/// the same.
pub fn no_document(page: &PageResult) -> Option<NoDocument> {
    let aborted = page
        .error
        .as_ref()
        .is_some_and(|error| error.code == NavigationErrorCode::Aborted);
    if !aborted {
        return None;
    }
    // Chrome breaks a load off at a `304` it kept no page for as it does
    // at a `204`, and its engine reports the wire's `304`: the site's
    // answer with no page, as Camoufox reports it and as at level 0 (QA
    // round three, class-02).
    if matches!(page.status, Some(204 | 205 | 304)) {
        return Some(NoDocument::Empty);
    }
    let attachment = page.headers.iter().any(|header| {
        header.name.eq_ignore_ascii_case("content-disposition")
            && header
                .value
                .trim_start()
                .get(..10)
                .is_some_and(|start| start.eq_ignore_ascii_case("attachment"))
    });
    // A load the browser turned into a download says so, in the driver's
    // words (a typed address, F120).
    let said = page.error.as_ref().is_some_and(|error| {
        error
            .message
            .to_ascii_lowercase()
            .contains("download is starting")
    });
    // The sidecar's signature of a click the browser answered by saving a
    // file: no status, no entry report, the click made (`NoPage`).
    let clicked_to_nothing = page.status.is_none()
        && page
            .behavior
            .as_ref()
            .is_some_and(|behavior| behavior.clicks >= 1 && behavior.entry.is_none());
    // A typed address answered with a file says so by its answer, whatever
    // document the tab still shows from before (and Firefox may hand over
    // the file's bytes: they are the file's). A click says so only by the
    // sidecar's signature, with nothing read.
    (attachment || said || (clicked_to_nothing && read_nothing(page))).then_some(NoDocument::File)
}

/// Whether a failed navigation ended at the site's own error answer: an
/// empty `4xx` the browser replaced with its own page, which came inside
/// a tunnel (`https`), where only the site answers (spec §8.3, F189). The
/// status the browser saw is the site's verdict, as the same answer is at
/// level 0 and with a body in either engine: a `403` or `429` is the
/// classifier's block, any other `4xx` is `ok` with its status, retried by
/// no other persona and no server error of the site (QA round three,
/// browser-02). Over plain `http` the proxy may have answered in the
/// site's place: such a page stays a failed navigation.
pub fn site_error_answer(page: &PageResult) -> bool {
    page.error
        .as_ref()
        .is_some_and(|error| error.code == NavigationErrorCode::NavigationFailed)
        && read_nothing(page)
        && matches!(page.status, Some(400..=499))
        && url::Url::parse(&page.final_url).is_ok_and(|url| url.scheme() == "https")
}

/// Why a page did not arrive, when it did not. An answer that holds no
/// page ([`no_document`]) is none, nor is the site's own error answer the
/// browser replaced with its page ([`site_error_answer`]): the site
/// answered.
pub fn page_failure(page: &PageResult) -> Option<Failure> {
    if no_document(page).is_some() || site_error_answer(page) {
        return None;
    }
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
        // A closed page, an entry that found no link (a download and no
        // content are `no_document`'s).
        NavigationErrorCode::Aborted => Failure {
            outcome: Outcome::ProviderError,
            reason: "aborted",
            courtesy: Courtesy::NotTheSite,
        },
    })
}

/// Whether nothing of a page whose navigation failed was read: no document
/// and no body. The browser saved what came instead of showing it (a
/// download), or the page stopped answering the engine's reads or its
/// renderer crashed (F103), or nothing came. A soft block is judged by what
/// a page says, and such a page said nothing: an empty body read from it is
/// no empty page, and its failure is what the attempt came to.
pub fn read_nothing(page: &PageResult) -> bool {
    page.error.is_some() && page.dom_html.is_none() && page.raw_body.is_none()
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
        Ok(Settled {
            cooldown,
            breaker: change,
        })
    }

    /// Ends the trial a flow took of a later site's half-open breaker when
    /// the attempt was not settled on that site (F101): the flow came to
    /// what it asked for, so the site answered every page it was asked
    /// for and its breaker closes; otherwise nothing is known of the site
    /// and the trial is given back, so that the next request is the trial
    /// at once rather than after the trial's timeout.
    ///
    /// # Errors
    ///
    /// When the backend cannot be written.
    pub async fn end_trial(&self, domain: &str, answered: bool) -> Result<(), SchedulerError> {
        let at = self.clock.now();
        if !answered {
            return self.backend.breaker_release(domain, at).await;
        }
        if self.backend.breaker_success(domain, at).await? == BreakerChange::Closed {
            tracing::info!(%domain, "the domain answers again: its breaker closed");
        }
        Ok(())
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
        judged(page, seen)
    }
}

/// What a page that holds no document came to: the site's answer, which
/// no other persona would change. `ok`, never a block.
fn no_page(kind: NoDocument) -> Classification {
    let mut classification = unanswered(
        Outcome::Ok,
        match kind {
            NoDocument::Empty => "no_content",
            NoDocument::File => "download",
        },
    );
    classification.retryable = false;
    classification
}

/// What a browser's page came to, given what the classifier saw of it.
fn judged(page: &PageResult, seen: Option<Classification>) -> Classification {
    if let Some(kind) = no_document(page) {
        return no_page(kind);
    }
    match (page_failure(page), seen) {
        // A page that hung after it answered may be a wall that never
        // finishes: what was seen decides when it is a block, unless the
        // block is nobody's.
        (Some(failure), Some(seen))
            if seen.outcome.is_block() && !nobodys_block(page, &failure, &seen) =>
        {
            seen
        }
        (Some(failure), _) => unanswered(failure.outcome, failure.reason),
        (None, Some(seen)) => seen,
        (None, None) => unanswered(Outcome::ProviderError, "no_response"),
    }
}

/// Whether a block the classifier saw on a page that failed is nobody's.
///
/// A soft block is judged by what a page says, so a page of which nothing
/// was read (a file the browser saved, a page that stopped answering, F103)
/// is none: the persona that asked for it was not turned away. Nor is an
/// error answer the browser replaced with its own page (a failed
/// navigation of which nothing was read) where that answer came over plain
/// `http://` (F189): there a proxy that will not carry a target answers in
/// the site's place, and an empty error answer may be the proxy's as well
/// as the site's. Inside a tunnel only the site answers, so over `https`
/// the status the browser saw stays the site's verdict (an empty `403` of a
/// WAF is the site's `forbidden`).
fn nobodys_block(page: &PageResult, failure: &Failure, seen: &Classification) -> bool {
    read_nothing(page)
        && (seen.outcome == Outcome::SoftBlock
            || (failure.reason == "navigation_failed" && answered_over_plain_http(page)))
}

/// Whether the address the page's status belongs to is plain `http:`. That
/// is the page's `final_url`: where the tab was sent, which for an answer
/// the browser replaced with its own page is the very document whose
/// status the result reports (the engine keeps a status only then).
fn answered_over_plain_http(page: &PageResult) -> bool {
    url::Url::parse(&page.final_url).is_ok_and(|url| url.scheme() == "http")
}

#[cfg(test)]
mod tests {
    use kage_browser::contract::{EXAMPLES_JSON, NavigationError, NavigationErrorCode, PageResult};
    use kage_core::domain::{Header, Render};
    use kage_core::outcome::{CooldownClass, Outcome};
    use kage_core::schedule::politeness::Courtesy;

    use super::{
        NoDocument, cookies_set, courtesy_of, judged, no_document, observe_page, page_failure,
        read_nothing, unanswered,
    };

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
    fn a_file_a_browser_saved_and_showed_nothing_of_is_no_empty_page() {
        let aborted = |code| {
            let mut page = page();
            page.error = Some(NavigationError {
                code,
                message: "page.goto: Download is starting".to_owned(),
                net_error: None,
            });
            page.dom_html = None;
            page.raw_body = None;
            page
        };
        // Saved, nothing kept: no page (a download; F163: the check of its
        // own that only this test used is `read_nothing`'s case).
        let saved = aborted(NavigationErrorCode::Aborted);
        // The bytes were handed over all the same (Firefox): there is
        // something to look at.
        let mut handed = saved.clone();
        handed.raw_body = page().raw_body.or_else(|| {
            serde_json::from_value(serde_json::json!({
                "data": "H4sI", "encoding": "base64", "bytes": 3, "truncated": false,
            }))
            .ok()
        });
        let mut shown = saved.clone();
        shown.dom_html = Some("<html></html>".to_owned());
        let mut loaded = saved;
        loaded.error = None;

        // Read nothing: every failure of which no document and no body came
        // (F103: a page that stopped answering keeps its status 200), and
        // never a page that loaded or one with something to look at.
        for code in [
            NavigationErrorCode::Aborted,
            NavigationErrorCode::Timeout,
            NavigationErrorCode::NavigationFailed,
            NavigationErrorCode::ProxyFailed,
        ] {
            assert!(read_nothing(&aborted(code)), "{code:?}");
        }
        assert!(!read_nothing(&loaded));
        assert!(!read_nothing(&handed));
        assert!(!read_nothing(&shown));
    }

    /// QA round one's policy-08, crawl-06, jobs-02 and extract-07 (F116,
    /// F120, F86): an answer that holds no page (no content, a file the
    /// browser saved) is the site's and no failure, which no other persona
    /// would change; a capture without its document and an answer that has
    /// none are judged on nothing.
    #[test]
    fn an_answer_that_holds_no_page_is_the_sites_and_no_failure() {
        let aborted = |status: Option<i64>| {
            let mut page = page();
            page.error = Some(NavigationError {
                code: NavigationErrorCode::Aborted,
                message: "net::ERR_ABORTED".to_owned(),
                net_error: None,
            });
            page.status = status;
            page
        };
        // No content: whatever the tab still shows. A `304` Chrome kept no
        // page for is broken off the same way (QA round three, class-02).
        for status in [204, 205, 304] {
            let empty = aborted(Some(status));
            assert_eq!(no_document(&empty), Some(NoDocument::Empty), "{status}");
            assert_eq!(page_failure(&empty), None, "{status}");
            let verdict = judged(&empty, None);
            assert_eq!(verdict.outcome, Outcome::Ok, "{status}");
            assert!(!verdict.retryable, "{status}");
            assert_eq!(verdict.evidence, ["failure:no_content"]);
        }
        // A file to save from a typed address: the answer said so.
        let mut typed = aborted(Some(200));
        typed.dom_html = None;
        typed.raw_body = None;
        typed.headers = vec![kage_browser::contract::Header {
            name: "Content-Disposition".to_owned(),
            value: "Attachment; filename=\"x.bin\"".to_owned(),
        }];
        assert_eq!(no_document(&typed), Some(NoDocument::File));
        // A typed address the browser turned into a download, which says so
        // in the driver's words and nothing else: no status, no headers.
        let mut starting = aborted(None);
        starting.dom_html = None;
        starting.raw_body = None;
        if let Some(error) = starting.error.as_mut() {
            error.message = "page.goto: Download is starting".to_owned();
        }
        if let Some(behavior) = starting.behavior.as_mut() {
            behavior.clicks = 0;
        }
        assert_eq!(no_document(&starting), Some(NoDocument::File));
        // A click the browser answered by saving a file: the sidecar's
        // signature, no status and no entry report after a click.
        let mut clicked = aborted(None);
        clicked.dom_html = None;
        clicked.raw_body = None;
        if let Some(behavior) = clicked.behavior.as_mut() {
            behavior.entry = None;
            behavior.clicks = 1;
        }
        assert_eq!(no_document(&clicked), Some(NoDocument::File));
        assert_eq!(page_failure(&clicked), None);
        let verdict = judged(&clicked, None);
        assert_eq!((verdict.outcome, verdict.retryable), (Outcome::Ok, false));
        assert_eq!(verdict.evidence, ["failure:download"]);
        // A typed file whose tab still shows the page before, and one whose
        // bytes the browser handed over (Firefox): files all the same.
        let mut stale = typed.clone();
        stale.dom_html = Some("<html><body>the page before</body></html>".to_owned());
        assert_eq!(no_document(&stale), Some(NoDocument::File));
        stale.raw_body = page().raw_body.or_else(|| {
            serde_json::from_value(serde_json::json!({
                "data": "H4sI", "encoding": "base64", "bytes": 3, "truncated": false,
            }))
            .ok()
        });
        assert_eq!(no_document(&stale), Some(NoDocument::File));
        // Anything else broken off stays a failure: an entry that found no
        // link (its report says so), a click of which a page was read, a
        // page that closed with a status.
        let mut entered = clicked.clone();
        entered.behavior = page().behavior;
        assert_eq!(no_document(&entered), None);
        let mut read = clicked.clone();
        read.dom_html = Some("<html></html>".to_owned());
        assert_eq!(no_document(&read), None);
        assert_eq!(no_document(&aborted(Some(200))), None);
        assert_eq!(
            page_failure(&aborted(Some(200))).map(|failure| failure.reason),
            Some("aborted")
        );
        let mut loaded = page();
        loaded.status = Some(204);
        assert_eq!(no_document(&loaded), None);

        // What the classifier looks at: a capture without the document has
        // none, nor has an answer of no content, and a page has one.
        let mut capture = page();
        capture.dom_html = None;
        capture.raw_body = None;
        assert!(!observe_page(&capture).unwrap().document);
        let mut nothing_new = page();
        nothing_new.dom_html = Some("<html><body>the page before</body></html>".to_owned());
        nothing_new.status = Some(205);
        assert!(!observe_page(&nothing_new).unwrap().document);
        // A 304 shows the page the browser kept: a document (F178).
        nothing_new.status = Some(304);
        assert!(observe_page(&nothing_new).unwrap().document);
        nothing_new.status = Some(200);
        assert!(observe_page(&nothing_new).unwrap().document);
    }

    #[test]
    fn an_unanswered_attempt_keeps_the_hints_of_its_outcome() {
        let timeout = unanswered(Outcome::Timeout, "timeout");
        assert!(timeout.retryable);
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

    #[test]
    fn an_error_answer_the_browser_replaced_is_the_sites_over_https_and_nobodys_over_plain_http() {
        let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../policies");
        let policies = kage_core::policy::PolicySet::load(&shipped).unwrap();
        // The page Firefox gives for an empty `403` it replaced with its
        // own: the status it saw, nothing read (F189).
        let replaced = |at: &str| {
            let mut page = page();
            page.final_url = at.to_owned();
            page.status = Some(403);
            page.title = None;
            page.dom_html = None;
            page.raw_body = None;
            page.headers = Vec::new();
            page.error = Some(NavigationError {
                code: NavigationErrorCode::NavigationFailed,
                message: format!(
                    "the browser could not load {at}: it answered with 403 and shows its own error page"
                ),
                net_error: None,
            });
            let seen = observe_page(&page).map(|seen| policies.signatures.classify(&seen));
            assert_eq!(
                seen.as_ref().map(|seen| seen.outcome),
                Some(Outcome::Forbidden),
                "{at}: the classifier sees the status"
            );
            judged(&page, seen)
        };

        // Inside a tunnel only the site answers: a WAF's empty `403` is the
        // site's `forbidden`, as it was before F189.
        let walled = replaced("https://shop.kage.test/a");
        assert_eq!(walled.outcome, Outcome::Forbidden);
        // Over plain http a proxy that will not carry the target answers in
        // the site's place: the failed navigation, nobody's block.
        let refused = replaced("http://shop.kage.test/a");
        assert_eq!(refused.outcome, Outcome::ProviderError);
        assert_eq!(
            refused.evidence,
            vec!["failure:navigation_failed".to_owned()]
        );
        assert_eq!(refused.cooldown, CooldownClass::None);
        // Where nobody heard the status (Chrome's page before F204, or an
        // answer its debugger missed), nothing is seen to judge.
        let mut chrome = page();
        chrome.final_url = "https://shop.kage.test/a".to_owned();
        chrome.status = None;
        chrome.title = None;
        chrome.dom_html = None;
        chrome.raw_body = None;
        chrome.headers = Vec::new();
        chrome.error = Some(NavigationError {
            code: NavigationErrorCode::NavigationFailed,
            message: "net::ERR_HTTP_RESPONSE_CODE_FAILURE".to_owned(),
            net_error: Some("ERR_HTTP_RESPONSE_CODE_FAILURE".to_owned()),
        });
        let seen = observe_page(&chrome).map(|seen| policies.signatures.classify(&seen));
        assert_eq!(judged(&chrome, seen).outcome, Outcome::ProviderError);
    }

    #[test]
    fn the_sites_empty_4xx_over_https_is_ok_with_its_status_and_retried_nowhere() {
        let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../policies");
        let policies = kage_core::policy::PolicySet::load(&shipped).unwrap();
        // The page either engine gives for an empty error answer it
        // replaced with its own (F189, F204): the status, nothing read.
        let replaced = |at: &str, status: i64| {
            let mut page = page();
            page.final_url = at.to_owned();
            page.status = Some(status);
            page.title = None;
            page.dom_html = None;
            page.raw_body = None;
            page.headers = Vec::new();
            page.error = Some(NavigationError {
                code: NavigationErrorCode::NavigationFailed,
                message: format!("the browser could not load {at}: it answered with {status}"),
                net_error: None,
            });
            page
        };
        let judge = |page: &PageResult| {
            let seen = observe_page(page).map(|seen| policies.signatures.classify(&seen));
            judged(page, seen)
        };
        // Inside a tunnel only the site answers: a missing page is the
        // site's `404`, as at level 0 and with a body (QA round three,
        // browser-02).
        for status in [400, 404, 410, 451] {
            let missing = replaced("https://shop.kage.test/gone", status);
            assert!(super::site_error_answer(&missing), "{status}");
            assert!(page_failure(&missing).is_none(), "{status}");
            let verdict = judge(&missing);
            assert_eq!(verdict.outcome, Outcome::Ok, "{status}");
            assert!(!verdict.retryable, "{status}");
            assert_eq!(verdict.cooldown, CooldownClass::None, "{status}");
            assert_eq!(
                courtesy_of(verdict.outcome, u16::try_from(status).ok()),
                Courtesy::Answered,
                "{status}"
            );
        }
        // A wall's empty 403 and 429 stay the site's blocks.
        assert_eq!(
            judge(&replaced("https://shop.kage.test/a", 403)).outcome,
            Outcome::Forbidden
        );
        assert_eq!(
            judge(&replaced("https://shop.kage.test/a", 429)).outcome,
            Outcome::RateLimited
        );
        // A server error stays one, retried; over plain http the proxy may
        // have answered: a failed navigation, as before.
        for (at, status) in [
            ("https://shop.kage.test/a", 503),
            ("http://shop.kage.test/a", 404),
            ("http://shop.kage.test/a", 451),
        ] {
            let page = replaced(at, status);
            assert!(!super::site_error_answer(&page), "{at} {status}");
            let verdict = judge(&page);
            assert_eq!(verdict.outcome, Outcome::ProviderError, "{at} {status}");
            assert_eq!(
                verdict.evidence,
                vec!["failure:navigation_failed".to_owned()],
                "{at} {status}"
            );
        }
        // A page that was read, or a failure of another kind, is no such
        // answer.
        let mut read = replaced("https://shop.kage.test/gone", 404);
        read.dom_html = Some("<html></html>".to_owned());
        assert!(!super::site_error_answer(&read));
        let mut timed_out = replaced("https://shop.kage.test/gone", 404);
        if let Some(error) = timed_out.error.as_mut() {
            error.code = NavigationErrorCode::Timeout;
        }
        assert!(!super::site_error_answer(&timed_out));
    }
}
