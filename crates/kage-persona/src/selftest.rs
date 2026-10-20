//! The self-tests of one persona (spec §7.4): the http path against a
//! fingerprint echo, the browser path against detection pages.
//!
//! A self-test runs as the persona, through its own proxy: the http one
//! with the persona's profile, `User-Agent`, brand list and languages on a
//! fresh connection without cookies; the browser one in the persona's own
//! context, opened with its stored fingerprint and closed again. Every run
//! is written to `selftest_runs`.
//!
//! **Verdicts.** A run passes, fails or is inconclusive. A failed run
//! cools the persona (reason `selftest_failed`) and raises
//! `http_selftest_failed` or `browser_selftest_failed` once; a passing
//! one ends a cooling that waits for a self-test, unless the latest run of
//! the other path failed. A run is inconclusive when the test site could
//! not be read (the echo did not answer, a page was unreachable and
//! nothing that was read failed hard): it changes nothing, so that an
//! outage of a test site never cools a persona down.
//!
//! **Schedule.** The manager's sweep runs each path for each persona
//! every `selftest.<path>.interval` (24 h) times a factor in
//! `[0.75, 1.25)` from the persona's seed, the first time a few minutes
//! after the persona was generated. A persona that waits for a self-test,
//! or whose last run was inconclusive, is tested again after a twelfth of
//! the interval. A scheduled run only ever asks the operator's own echo
//! (`selftest.http.echo_url`) and loads the operator's own pages
//! (`selftest.browser.pages`); without them nothing is scheduled. The
//! echo and the pages run by others see the exit address of whoever asks
//! and are only ever used on demand, by `kage persona selftest
//! --third-party` and the gated live suite.

use std::time::Duration;

use chrono::{DateTime, Utc};
use kage_browser::contract::{
    EchoExpectation, ExpectedIdentity, FindingStatus, SelftestPage,
    SelftestPageKind as ContractPageKind, SelftestRequest, SelftestResponse,
};
use kage_core::alert::{Alert, AlertKind, AlertSeverity};
use kage_core::config::{BrowserSelftestConfig, SelftestPageKind};
use kage_core::domain::{CoolingReason, Engine, Persona, PersonaState, Render, SelftestRun};
use kage_core::ids::{PersonaId, SelftestRunId};
use kage_core::persona;
use kage_core::seed::Seed;
use kage_http::selftest::{SelftestError, SelftestReport};
use serde_json::json;
use url::Url;

use crate::contexts::{Purpose, Via};
use crate::identity::http_spec;
use crate::lifecycle;
use crate::manager::PersonaManager;
use crate::sidecar::contract_os;
use crate::{PersonaError, PersonaEvent};

/// The earliest and the latest the first scheduled self-test of a new
/// persona happens after its generation.
const FIRST_RUN_AFTER: (Duration, Duration) = (Duration::from_mins(2), Duration::from_mins(30));

/// A persona that waits for a self-test is tested again after this share
/// of the interval.
const RETRY_SHARE: u32 = 12;

/// Most scheduled http self-tests in one sweep.
const HTTP_RUNS_PER_SWEEP: u32 = 4;

/// How a self-test ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Everything held.
    Passed,
    /// Something differs from what the persona claims.
    Failed,
    /// The test site could not be read; nothing is known.
    Inconclusive,
}

impl Verdict {
    /// The stable name stored with a run.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Inconclusive => "inconclusive",
        }
    }

    /// The verdict stored with a run.
    fn of(run: &SelftestRun) -> Self {
        match run
            .findings
            .get("verdict")
            .and_then(|verdict| verdict.as_str())
        {
            Some("inconclusive") => Self::Inconclusive,
            _ if run.passed => Self::Passed,
            _ => Self::Failed,
        }
    }
}

/// One self-test of the http path.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpRun {
    /// The stored run.
    pub run: SelftestRun,
    /// How it ended.
    pub verdict: Verdict,
    /// What the echo saw and how it compares; `None` when the echo could
    /// not be read.
    pub report: Option<SelftestReport>,
    /// The persona after the run.
    pub persona: Persona,
}

/// One self-test of the browser path.
#[derive(Debug, Clone, PartialEq)]
pub struct BrowserRun {
    /// The stored run.
    pub run: SelftestRun,
    /// How it ended.
    pub verdict: Verdict,
    /// The sidecar's findings.
    pub report: SelftestResponse,
    /// The persona after the run.
    pub persona: Persona,
}

/// The pages of a browser self-test as the contract writes them: the
/// operator's own, and with `third_party` the detection pages run by
/// others. A scheduled run never passes `third_party`.
pub fn pages(config: &BrowserSelftestConfig, third_party: bool) -> Vec<SelftestPage> {
    let others: &[_] = if third_party {
        &config.third_party_pages
    } else {
        &[]
    };
    config
        .pages
        .iter()
        .chain(others)
        .map(|page| SelftestPage {
            id: page.id.clone(),
            kind: match page.kind {
                SelftestPageKind::Probe => ContractPageKind::Probe,
                SelftestPageKind::Echo => ContractPageKind::Echo,
                SelftestPageKind::Sannysoft => ContractPageKind::Sannysoft,
                SelftestPageKind::Creepjs => ContractPageKind::Creepjs,
            },
            rules: None,
            settle_ms: None,
            url: page.url.clone(),
            weight: None,
        })
        .collect()
}

/// What an echo page must report for this browser: the recorded capture.
pub fn echo_expectation(engine: Engine, major: u32) -> Option<EchoExpectation> {
    let capture = kage_http::selftest::browser_capture(engine.as_str(), major)?;
    Some(EchoExpectation {
        akamai: capture.akamai,
        headers: capture.headers,
        ja4: capture.ja4,
    })
}

/// How a browser self-test ended. A hard failure on a page that was read
/// is a failure whatever else could not be read.
fn browser_verdict(report: &SelftestResponse) -> Verdict {
    let hard = report
        .findings
        .iter()
        .any(|finding| finding.hard && finding.status == FindingStatus::Fail);
    if hard {
        Verdict::Failed
    } else if !report.complete {
        Verdict::Inconclusive
    } else if report.passed {
        Verdict::Passed
    } else {
        Verdict::Failed
    }
}

const fn label(render: Render) -> &'static str {
    match render {
        Render::Http => "selftest-http",
        Render::Browser => "selftest-browser",
    }
}

/// When the next scheduled self-test of a persona on a path is due, given
/// its latest run and how many it had.
pub fn next_due(
    persona: &Persona,
    render: Render,
    latest: Option<&SelftestRun>,
    runs: u64,
    interval: Duration,
) -> DateTime<Utc> {
    let waits = persona.state == PersonaState::Cooling
        && persona
            .cooling_reason
            .is_some_and(CoolingReason::needs_selftest);
    let span = |span: Duration| chrono::Duration::from_std(span).unwrap_or(chrono::Duration::MAX);
    match latest {
        // A persona that waits for its first self-test gets it now.
        None if waits => persona.created_at,
        None => {
            let (earliest, latest) = FIRST_RUN_AFTER;
            let first = Seed::derive(label(render), &[persona.seed.as_bytes()])
                .rng()
                .uniform(earliest.as_secs_f64(), latest.as_secs_f64());
            persona.created_at + span(Duration::from_secs_f64(first).min(interval))
        }
        Some(run) => {
            let base = if waits || Verdict::of(run) == Verdict::Inconclusive {
                interval / RETRY_SHARE
            } else {
                interval
            };
            run.finished_at + span(persona::jittered(base, &persona.seed, label(render), runs))
        }
    }
}

/// The key of a persona's standing self-test alert on one path.
pub(crate) fn alert_key(persona: PersonaId, render: Render) -> String {
    format!("selftest_failed:{persona}:{render}")
}

impl PersonaManager {
    /// Runs the self-test of the http path for a persona against `echo`.
    ///
    /// # Errors
    ///
    /// [`PersonaError::HttpPathOff`] for a persona without an http path,
    /// [`PersonaError::Selftest`] when no fingerprint is recorded for its
    /// profile, and whatever [`Self::route`] refuses with. An echo that
    /// could not be read is not an error: the run is inconclusive.
    pub async fn selftest_http(&self, id: PersonaId, echo: &Url) -> Result<HttpRun, PersonaError> {
        let persona = self.get(id).await?;
        let spec = http_spec(&persona).map_err(|reason| PersonaError::HttpPathOff {
            persona: id,
            reason,
        })?;
        if kage_http::selftest::expected(&spec.profile).is_none() {
            return Err(SelftestError::NoExpectation {
                profile: spec.profile.to_string(),
            }
            .into());
        }
        let (_, endpoint) = self.egress(&persona, Via::Http).await?;
        let client = self.selftest_http.ephemeral(&spec, &endpoint)?;
        let started_at = self.now();
        let outcome =
            kage_http::selftest::run(&client, echo, self.config.selftest.http.timeout.get()).await;
        let finished_at = self.now();

        let mut asked = echo.clone();
        asked.set_query(None);
        asked.set_fragment(None);
        let (verdict, findings, report) = match outcome {
            Ok(report) => {
                let verdict = if report.passed {
                    Verdict::Passed
                } else {
                    Verdict::Failed
                };
                let findings = json!({
                    "verdict": verdict.as_str(),
                    "profile": report.profile,
                    "os": report.os,
                    "echo": report.echo,
                    "findings": report.findings,
                });
                (verdict, findings, Some(report))
            }
            Err(error @ SelftestError::NoExpectation { .. }) => return Err(error.into()),
            Err(error) => (
                Verdict::Inconclusive,
                json!({
                    "verdict": Verdict::Inconclusive.as_str(),
                    "profile": spec.profile.name(),
                    "echo": asked.as_str(),
                    "error": error.to_string(),
                }),
                None,
            ),
        };
        let run = SelftestRun {
            id: SelftestRunId::new(),
            persona_id: id,
            engine: persona.engine,
            render: Render::Http,
            started_at,
            finished_at,
            passed: verdict == Verdict::Passed,
            score: None,
            findings,
        };
        self.store.selftest_runs().insert(&run).await?;
        let failed: Vec<&str> = report
            .iter()
            .flat_map(|report| &report.findings)
            .filter(|finding| !finding.passed)
            .map(|finding| finding.check)
            .collect();
        let persona = self
            .judged(&persona, &run, verdict, json!({ "differing": failed }))
            .await?;
        Ok(HttpRun {
            run,
            verdict,
            report,
            persona,
        })
    }

    /// Runs the self-test of the browser path for a persona: its context
    /// is opened, the sidecar loads `pages` and judges them, and the
    /// context is closed again whatever it found.
    ///
    /// # Errors
    ///
    /// [`PersonaError::NothingToTest`] without a page, whatever
    /// [`Self::route`] refuses with, and a sidecar error when the test
    /// could not be run at all; no run is stored then.
    pub async fn selftest_browser(
        &self,
        id: PersonaId,
        pages: Vec<SelftestPage>,
    ) -> Result<BrowserRun, PersonaError> {
        self.selftest_browser_for(id, pages, true).await
    }

    /// [`Self::selftest_browser`], for an operator or for the schedule. A
    /// scheduled run waits ([`PersonaError::Held`]) where an operator's
    /// goes ahead.
    async fn selftest_browser_for(
        &self,
        id: PersonaId,
        pages: Vec<SelftestPage>,
        operator: bool,
    ) -> Result<BrowserRun, PersonaError> {
        if pages.is_empty() {
            return Err(PersonaError::NothingToTest(
                "there is no page to load: name the operator's own pages in \
                 selftest.browser.pages, or ask for the third-party pages"
                    .to_owned(),
            ));
        }
        let mut persona = self.get(id).await?;
        let (_, endpoint) = self.egress(&persona, Via::Browser).await?;
        let config = &self.config.selftest.browser;
        let sidecar = self.sidecars.of(persona.engine);

        // With a scheduler its gate decides whether the persona and a
        // session can be had; without one, one context of the manager's
        // own per engine at a time.
        let (_permit, _one_at_a_time) = match self.gate() {
            Some(gate) => {
                let permit = gate
                    .reserve(&persona, Purpose::Selftest { operator })
                    .await
                    .map_err(|held| PersonaError::Held { persona: id, held })?;
                (Some(permit), None)
            }
            None => (None, Some(self.browser_work(persona.engine).lock().await)),
        };
        let started_at = self.now();
        let outcome = {
            let (lease, moved) = self.open_browser(&persona, &endpoint).await?;
            // A persona whose engine runs a newer browser is tested as it
            // was moved and stored.
            if let Some(moved) = moved {
                persona = moved;
            }
            let request = SelftestRequest {
                artifact_prefix: None,
                echo: echo_expectation(persona.engine, persona.browser_major),
                // What the persona is stored as, not what a sidecar says
                // it is: a context that shows anything else must fail the
                // test.
                expected: ExpectedIdentity {
                    browser_major: u64::from(persona.browser_major),
                    claimed_os: contract_os(persona.claimed_os),
                    languages: persona.languages.clone(),
                    timezone: persona.timezone.clone(),
                },
                min_score: Some(f64::from(config.min_score)),
                pages: Some(pages),
                timeout_ms: u64::try_from(config.page_timeout.get().as_millis()).ok(),
            };
            let outcome = sidecar.selftest(lease.context(), &request).await;
            // The test's own browser is closed again; one that real work
            // had open stays open.
            if lease.reused() {
                lease.keep(false);
            } else {
                lease.close().await;
            }
            outcome
        };
        let report = outcome?;
        let finished_at = self.now();

        let verdict = browser_verdict(&report);
        // A score is a number between 0 and 100.
        #[allow(clippy::cast_possible_truncation)]
        let score = report.score as f32;
        let run = SelftestRun {
            id: SelftestRunId::new(),
            persona_id: id,
            engine: persona.engine,
            render: Render::Browser,
            started_at,
            finished_at,
            passed: verdict == Verdict::Passed,
            score: Some(score),
            findings: json!({
                "verdict": verdict.as_str(),
                "complete": report.complete,
                "min_score": report.min_score,
                "findings": report.findings,
                "pages": report.pages,
            }),
        };
        self.store.selftest_runs().insert(&run).await?;
        let failed: Vec<String> = report
            .findings
            .iter()
            .filter(|finding| finding.status == FindingStatus::Fail)
            .map(|finding| format!("{}/{}", finding.page, finding.check))
            .collect();
        let persona = self
            .judged(
                &persona,
                &run,
                verdict,
                json!({ "failed": failed, "score": report.score }),
            )
            .await?;
        Ok(BrowserRun {
            run,
            verdict,
            report,
            persona,
        })
    }

    /// Applies a verdict to the persona: a failure cools it and raises the
    /// alert once, a pass ends a cooling that waited for it.
    async fn judged(
        &self,
        persona: &Persona,
        run: &SelftestRun,
        verdict: Verdict,
        mut details: serde_json::Value,
    ) -> Result<Persona, PersonaError> {
        if let Some(details) = details.as_object_mut() {
            details.insert("render".to_owned(), json!(run.render.as_str()));
            details.insert("run".to_owned(), json!(run.id));
        }
        match verdict {
            Verdict::Inconclusive => {
                self.event(
                    persona.id,
                    "persona_selftest_inconclusive",
                    AlertSeverity::Warning,
                    details,
                )
                .await;
                Ok(persona.clone())
            }
            Verdict::Failed => self.selftest_failed(persona, run.render, details).await,
            Verdict::Passed => self.selftest_passed(persona, run.render, details).await,
        }
    }

    async fn selftest_failed(
        &self,
        persona: &Persona,
        render: Render,
        details: serde_json::Value,
    ) -> Result<Persona, PersonaError> {
        let Some((transition, updated)) = self
            .store
            .personas()
            .modify(persona.id, |persona| {
                let transition =
                    lifecycle::cool_until_selftest(persona, CoolingReason::SelftestFailed);
                (transition, persona.clone())
            })
            .await?
        else {
            return Err(PersonaError::NotFound(persona.id));
        };
        self.event(
            persona.id,
            "persona_selftest_failed",
            AlertSeverity::Warning,
            details.clone(),
        )
        .await;
        self.raise_once(
            alert_key(persona.id, render),
            Alert::new(
                match render {
                    Render::Http => AlertKind::HttpSelftestFailed,
                    Render::Browser => AlertKind::BrowserSelftestFailed,
                },
                AlertSeverity::Critical,
                format!(
                    "the {render} self-test of persona {} failed; it is cooling until a \
                     self-test passes",
                    persona.id
                ),
                details,
            )
            .about_persona(persona.id),
        );
        if let Some((from, to)) = transition {
            self.publish(PersonaEvent::StateChanged {
                persona: persona.id,
                from,
                to,
            });
        }
        Ok(updated)
    }

    async fn selftest_passed(
        &self,
        persona: &Persona,
        render: Render,
        details: serde_json::Value,
    ) -> Result<Persona, PersonaError> {
        let other = match render {
            Render::Http => Render::Browser,
            Render::Browser => Render::Http,
        };
        let other_failed = self
            .store
            .selftest_runs()
            .latest(persona.id, other)
            .await?
            .is_some_and(|run| Verdict::of(&run) == Verdict::Failed);
        let usable = self
            .pool
            .get(persona.proxy_id)
            .await?
            .is_some_and(|proxy| proxy.state.is_usable());
        let mut updated = persona.clone();
        if !other_failed {
            let Some((transition, after)) = self
                .store
                .personas()
                .modify(persona.id, |persona| {
                    (lifecycle::selftest_passed(persona, usable), persona.clone())
                })
                .await?
            else {
                return Err(PersonaError::NotFound(persona.id));
            };
            updated = after;
            if let Some((from, to)) = transition {
                self.publish(PersonaEvent::StateChanged {
                    persona: persona.id,
                    from,
                    to,
                });
            }
        }
        self.settle(&alert_key(persona.id, render));
        self.event(
            persona.id,
            "persona_selftest_passed",
            AlertSeverity::Info,
            details,
        )
        .await;
        Ok(updated)
    }

    async fn selftest_is_due(
        &self,
        persona: &Persona,
        render: Render,
        interval: Duration,
    ) -> Result<bool, PersonaError> {
        let runs = self.store.selftest_runs();
        let latest = runs.latest(persona.id, render).await?;
        let count = runs.count(persona.id, render).await?;
        Ok(next_due(persona, render, latest.as_ref(), count, interval) <= self.now())
    }

    /// Runs the scheduled self-tests that are due: only against the
    /// operator's own echo and pages, never a third party. Returns how
    /// many ran.
    ///
    /// # Errors
    ///
    /// Fails when the store fails. A run that cannot be made is logged and
    /// tried again at the next sweep.
    pub async fn selftests_due(&self) -> Result<u32, PersonaError> {
        let selftest = &self.config.selftest;
        let echo = selftest
            .http
            .echo_url
            .as_deref()
            .and_then(|echo| Url::parse(echo).ok());
        let own_pages = pages(&selftest.browser, false);
        if (echo.is_none() && own_pages.is_empty()) || self.egress_unsafe().await? {
            return Ok(0);
        }
        let waits = |error: &PersonaError| {
            matches!(
                error,
                PersonaError::ProxyNotUsable { .. }
                    | PersonaError::EgressUnsafe
                    | PersonaError::Held { .. }
            )
        };
        let mut runs = 0;
        let mut http_runs = 0;
        let mut browser_ran = [false; 2];
        for persona in self.store.personas().list_live().await? {
            let testable = http_spec(&persona)
                .is_ok_and(|spec| kage_http::selftest::expected(&spec.profile).is_some());
            if let Some(echo) = &echo
                && testable
                && http_runs < HTTP_RUNS_PER_SWEEP
                && self
                    .selftest_is_due(&persona, Render::Http, selftest.http.interval.get())
                    .await?
            {
                match self.selftest_http(persona.id, echo).await {
                    Ok(_) => {
                        runs += 1;
                        http_runs += 1;
                    }
                    Err(error) if waits(&error) => {}
                    Err(error) => {
                        tracing::warn!(persona = %persona.id, %error, "the http self-test could not be run");
                    }
                }
            }
            let engine = usize::from(persona.engine == Engine::Firefox);
            if !own_pages.is_empty()
                && !browser_ran[engine]
                && self
                    .selftest_is_due(&persona, Render::Browser, selftest.browser.interval.get())
                    .await?
            {
                match self
                    .selftest_browser_for(persona.id, own_pages.clone(), false)
                    .await
                {
                    Ok(_) => {
                        runs += 1;
                        browser_ran[engine] = true;
                    }
                    Err(error) if waits(&error) => {}
                    Err(error) => {
                        tracing::warn!(persona = %persona.id, %error, "the browser self-test could not be run");
                    }
                }
            }
        }
        Ok(runs)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::Utc;
    use kage_browser::contract::{
        EXAMPLES_JSON, Finding, FindingStatus, SelftestPageKind as ContractPageKind,
        SelftestResponse,
    };
    use kage_core::config::{BrowserSelftestConfig, SelftestPageConfig, SelftestPageKind};
    use kage_core::domain::{CoolingReason, Engine, PersonaState, Render, SelftestRun};
    use kage_core::ids::SelftestRunId;
    use serde_json::json;

    use super::{Verdict, alert_key, browser_verdict, echo_expectation, next_due, pages};
    use crate::test_support::persona;

    const DAY: Duration = Duration::from_hours(24);

    fn report() -> SelftestResponse {
        let examples: serde_json::Value = serde_json::from_str(EXAMPLES_JSON).unwrap();
        let mut report: SelftestResponse =
            serde_json::from_value(examples["SelftestResponse"][0].clone()).unwrap();
        report.complete = true;
        report.passed = true;
        report.findings.clear();
        report
    }

    fn finding(status: FindingStatus, hard: bool) -> Finding {
        Finding {
            check: "navigator.webdriver".to_owned(),
            expected: None,
            hard,
            observed: None,
            page: "probe".to_owned(),
            status,
            weight: 1.0,
        }
    }

    #[test]
    fn a_scheduled_run_never_loads_a_page_run_by_others() {
        let config = BrowserSelftestConfig {
            pages: vec![
                SelftestPageConfig {
                    id: "probe".to_owned(),
                    kind: SelftestPageKind::Probe,
                    url: "https://fixture.kage.test/probe".to_owned(),
                },
                SelftestPageConfig {
                    id: "echo".to_owned(),
                    kind: SelftestPageKind::Echo,
                    url: "https://echo.kage.test/".to_owned(),
                },
            ],
            ..BrowserSelftestConfig::default()
        };
        let own = pages(&config, false);
        assert_eq!(
            own.iter().map(|page| page.id.as_str()).collect::<Vec<_>>(),
            ["probe", "echo"]
        );
        assert_eq!(own[0].kind, ContractPageKind::Probe);
        assert_eq!(own[1].kind, ContractPageKind::Echo);
        let all = pages(&config, true);
        assert_eq!(
            all.iter().map(|page| page.id.as_str()).collect::<Vec<_>>(),
            ["probe", "echo", "sannysoft", "creepjs"]
        );
        assert_eq!(all[2].kind, ContractPageKind::Sannysoft);
        assert_eq!(all[3].kind, ContractPageKind::Creepjs);
        // The default has no own page: nothing is scheduled.
        assert_eq!(pages(&BrowserSelftestConfig::default(), false).len(), 0);
    }

    #[test]
    fn an_echo_page_is_judged_against_the_recorded_capture_of_the_browser() {
        let chrome = echo_expectation(Engine::Chromium, 155).unwrap();
        assert!(chrome.ja4.starts_with("t13d"), "{}", chrome.ja4);
        assert_ne!(chrome.headers.len(), 0);
        assert!(echo_expectation(Engine::Firefox, 156).is_some());
        // The capture of the previous image is gone with it.
        assert!(echo_expectation(Engine::Firefox, 152).is_none());
        assert!(echo_expectation(Engine::Chromium, 1).is_none());
    }

    #[test]
    fn an_outage_of_a_test_site_is_no_verdict() {
        assert_eq!(browser_verdict(&report()), Verdict::Passed);

        let mut low = report();
        low.passed = false;
        assert_eq!(browser_verdict(&low), Verdict::Failed);

        // A page could not be read and nothing that was read failed hard.
        let mut outage = report();
        outage.complete = false;
        outage.passed = false;
        outage.findings.push(finding(FindingStatus::Unknown, true));
        outage.findings.push(finding(FindingStatus::Fail, false));
        assert_eq!(browser_verdict(&outage), Verdict::Inconclusive);

        // A hard failure counts whatever else could not be read.
        outage.findings.push(finding(FindingStatus::Fail, true));
        assert_eq!(browser_verdict(&outage), Verdict::Failed);

        assert_eq!(Verdict::Passed.as_str(), "passed");
        assert_eq!(Verdict::Failed.as_str(), "failed");
        assert_eq!(Verdict::Inconclusive.as_str(), "inconclusive");
    }

    fn run(persona: &kage_core::domain::Persona, verdict: Verdict) -> SelftestRun {
        SelftestRun {
            id: SelftestRunId::new(),
            persona_id: persona.id,
            engine: persona.engine,
            render: Render::Browser,
            started_at: persona.created_at,
            finished_at: persona.created_at + chrono::Duration::hours(1),
            passed: verdict == Verdict::Passed,
            score: None,
            findings: json!({ "verdict": verdict.as_str() }),
        }
    }

    #[test]
    fn self_tests_are_a_day_apart_and_sooner_for_a_persona_that_waits() {
        let mut persona = persona(Engine::Firefox, 156);
        persona.created_at = Utc::now();
        let minutes = |at: chrono::DateTime<Utc>, from: chrono::DateTime<Utc>| {
            at.signed_duration_since(from).num_minutes()
        };

        // The first run: a few minutes after generation, the same every
        // time it is asked, and not the same minute on both paths.
        let first = next_due(&persona, Render::Browser, None, 0, DAY);
        assert!((2..=30).contains(&minutes(first, persona.created_at)));
        assert_eq!(first, next_due(&persona, Render::Browser, None, 0, DAY));
        assert_ne!(first, next_due(&persona, Render::Http, None, 0, DAY));
        // With a short interval the first run is not later than one.
        let soon = next_due(&persona, Render::Browser, None, 0, Duration::from_secs(60));
        assert!(minutes(soon, persona.created_at) <= 1);

        // After a pass: a day, give or take a quarter, and not the same
        // gap every day.
        let passed = run(&persona, Verdict::Passed);
        let next = next_due(&persona, Render::Browser, Some(&passed), 1, DAY);
        let gap = minutes(next, passed.finished_at);
        assert!((18 * 60..30 * 60).contains(&gap), "{gap} min");
        let gaps: std::collections::BTreeSet<i64> = (1..6)
            .map(|runs| {
                minutes(
                    next_due(&persona, Render::Browser, Some(&passed), runs, DAY),
                    passed.finished_at,
                )
            })
            .collect();
        assert!(gaps.len() >= 4, "{gaps:?}");

        // After an outage of the test site: two hours, give or take.
        let outage = run(&persona, Verdict::Inconclusive);
        let retry = minutes(
            next_due(&persona, Render::Browser, Some(&outage), 1, DAY),
            outage.finished_at,
        );
        assert!((90..150).contains(&retry), "{retry} min");

        // A persona that waits for a self-test: at once when it never had
        // one, else two hours after the last.
        persona.state = PersonaState::Cooling;
        persona.cooling_reason = Some(CoolingReason::GeoChanged);
        assert_eq!(
            next_due(&persona, Render::Browser, None, 0, DAY),
            persona.created_at
        );
        let waiting = minutes(
            next_due(&persona, Render::Browser, Some(&passed), 1, DAY),
            passed.finished_at,
        );
        assert!((90..150).contains(&waiting), "{waiting} min");
        // One that cools with its proxy is on the ordinary schedule.
        persona.cooling_reason = Some(CoolingReason::ProxyBackoff);
        let ordinary = minutes(
            next_due(&persona, Render::Browser, Some(&passed), 1, DAY),
            passed.finished_at,
        );
        assert_eq!(ordinary, gap);
    }

    #[test]
    fn a_stored_run_says_how_it_ended() {
        let persona = persona(Engine::Chromium, 155);
        for verdict in [Verdict::Passed, Verdict::Failed, Verdict::Inconclusive] {
            assert_eq!(Verdict::of(&run(&persona, verdict)), verdict);
        }
        // A run from before verdicts were stored is judged by its flag.
        let mut old = run(&persona, Verdict::Failed);
        old.findings = json!([]);
        assert_eq!(Verdict::of(&old), Verdict::Failed);
        old.passed = true;
        assert_eq!(Verdict::of(&old), Verdict::Passed);

        assert_ne!(
            alert_key(persona.id, Render::Http),
            alert_key(persona.id, Render::Browser)
        );
    }
}
