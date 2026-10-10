//! The first visits of a new persona (spec §6.1 step 5).
//!
//! The plan is `kage_core::persona::warmup`'s: which pages, in which
//! order, when. This module makes the visits. Each one is the persona's
//! own browser through its own proxy: a context is opened for the visit,
//! the page is loaded by typing its address with the behavior engine on
//! (a human dwell, a scroll, a wandering pointer, no click), and the
//! context is closed again, so that cookies, storage and history land in
//! the profile. The http path never warms up.
//!
//! - One visit of the manager's own per engine at a time, one per persona
//!   per sweep, and none outside the persona's active hours.
//! - Nothing is sent while a leak is on record or the proxy cannot carry
//!   traffic; the visit waits.
//! - A visit counts toward the persona's state on that site like any
//!   other request. A blocked one is recorded and cools that site for
//!   that persona only; it is never repeated and never escalated, and a
//!   later visit to the same site is skipped.
//! - A visit that was made or skipped is done, whatever it found. Only a
//!   sidecar that could not be reached leaves a visit to be tried again.
//!
//! A persona is ready for real work (`warmed_at`) after
//! `personas.warmup.min_visits` visits or `personas.warmup.max_wait`,
//! whichever is first, and makes the rest of its plan afterwards.

use kage_browser::Navigation;
use kage_browser::contract::{
    BehaviorOptions, CaptureOptions, DwellMode, Entry, EntryStrategy, NavigateRequest,
    NavigationErrorCode, Wait, WaitUntil,
};
use kage_core::alert::AlertSeverity;
use kage_core::domain::{Persona, PersonaState};
use kage_core::outcome::Outcome;
use kage_core::persona::warmup::{self, PlannedVisit};
use kage_core::policy::Cooldowns;
use kage_core::seed::Seed;
use serde_json::json;

use crate::manager::PersonaManager;
use crate::sidecar::open_request;
use crate::{PersonaError, PersonaEvent};

/// How far down a warm-up visit scrolls, as a share of the page.
const SCROLL_DEPTH: (f64, f64) = (0.6, 0.9);

/// What a visit found, and the status it was answered with.
fn classify(navigation: &Navigation) -> (Outcome, Option<i64>) {
    let page = match navigation {
        Navigation::SsrfDenied { .. } => return (Outcome::SsrfDenied, None),
        Navigation::Page(page) => page,
    };
    let outcome = if let Some(error) = &page.error {
        match error.code {
            NavigationErrorCode::Timeout => Outcome::Timeout,
            NavigationErrorCode::NavigationFailed
            | NavigationErrorCode::ProxyFailed
            | NavigationErrorCode::Aborted => Outcome::ProviderError,
        }
    } else if page.challenge.interstitial {
        if page.challenge.resolved == Some(true) {
            Outcome::ChallengePassed
        } else {
            Outcome::ChallengeFailed
        }
    } else {
        match page.status {
            Some(429) => Outcome::RateLimited,
            Some(403) => Outcome::Forbidden,
            Some(status) if status >= 500 => Outcome::ProviderError,
            _ => Outcome::Ok,
        }
    };
    (outcome, page.status)
}

/// The navigation of one warm-up visit: typed into the address bar, read
/// like a person reads, nothing captured.
fn visit_request(
    persona: &Persona,
    visit: &PlannedVisit,
    timeout_ms: Option<u64>,
) -> NavigateRequest {
    let seed = Seed::derive(
        "warmup-visit",
        &[persona.seed.as_bytes(), &visit.number.to_le_bytes()],
    );
    let (shallow, deep) = SCROLL_DEPTH;
    NavigateRequest {
        artifact_prefix: None,
        behavior: Some(BehaviorOptions {
            dwell: Some(DwellMode::Human),
            entry: Some(Entry {
                fallback: None,
                from_url: None,
                strategy: EntryStrategy::Direct,
            }),
            scroll_depth: Some(seed.child("scroll").rng().uniform(shallow, deep)),
            seed: Some(seed.to_string()),
            wander: Some(true),
        }),
        capture: CaptureOptions {
            console: false,
            dom: false,
            pdf: None,
            raw: false,
            screenshot: None,
            xhr: None,
        },
        challenge_wait_ms: None,
        dwell_ms: None,
        max_body_bytes: None,
        timeout_ms,
        url: visit.url.to_string(),
        wait: Wait {
            extra_ms: None,
            selector: None,
            until: WaitUntil::Load,
        },
    }
}

impl PersonaManager {
    /// Marks personas whose warm-up is over as ready, and makes the
    /// warm-up visits that are due. Returns how many visits were made or
    /// skipped.
    ///
    /// # Errors
    ///
    /// Fails when the store fails. A visit that fails is logged and the
    /// others still run.
    pub async fn warm_up_due(&self) -> Result<u32, PersonaError> {
        let config = &self.config.personas.warmup;
        let hours = config.active_hours.hours();
        let mut handled = 0;
        for mut persona in self.store.personas().list_live().await? {
            let plan = self.warmup_plan(&persona);
            let now = self.now();
            if persona.warmed_at.is_none()
                && warmup::ready(
                    plan.len(),
                    persona.warmup_visits,
                    persona.created_at,
                    now,
                    config,
                )
            {
                persona = self.mark_warmed(&persona).await?;
            }
            let next = usize::try_from(persona.warmup_visits)
                .ok()
                .and_then(|done| plan.get(done));
            let Some(visit) = next else {
                continue;
            };
            if persona.state != PersonaState::Active
                || visit.due_at > now
                || !hours.contains(now, &persona.timezone)
            {
                continue;
            }
            match self.warmup_visit(&persona, visit).await {
                Ok(true) => {
                    handled += 1;
                    // The visit that makes the persona ready says so at
                    // once, not a sweep later.
                    if persona.warmed_at.is_none()
                        && warmup::ready(
                            plan.len(),
                            persona.warmup_visits + 1,
                            persona.created_at,
                            self.now(),
                            config,
                        )
                    {
                        self.mark_warmed(&persona).await?;
                    }
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(persona = %persona.id, %error, "a warm-up visit could not be made");
                }
            }
        }
        Ok(handled)
    }

    async fn mark_warmed(&self, persona: &Persona) -> Result<Persona, PersonaError> {
        let now = self.now();
        let Some((first, updated)) = self
            .store
            .personas()
            .modify(persona.id, |persona| {
                let first = persona.warmed_at.is_none();
                if first {
                    persona.warmed_at = Some(now);
                }
                (first, persona.clone())
            })
            .await?
        else {
            return Err(PersonaError::NotFound(persona.id));
        };
        if first {
            self.event(
                persona.id,
                "persona_warmed",
                AlertSeverity::Info,
                json!({ "visits": updated.warmup_visits }),
            )
            .await;
            self.publish(PersonaEvent::Warmed {
                persona: persona.id,
            });
        }
        Ok(updated)
    }

    /// Makes one warm-up visit. `Ok(true)` when the visit is done (made,
    /// or skipped because the site cools for this persona), `Ok(false)`
    /// when it has to wait.
    async fn warmup_visit(
        &self,
        persona: &Persona,
        visit: &PlannedVisit,
    ) -> Result<bool, PersonaError> {
        let (proxy, endpoint) = match self.route(persona).await {
            Ok(route) => route,
            Err(PersonaError::ProxyNotUsable { .. } | PersonaError::EgressUnsafe) => {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        let domain = visit.domain().to_owned();
        let domains = self.domains();
        if !domains
            .eligibility(persona, &domain, None)
            .await?
            .is_eligible()
        {
            self.visit_done(persona, visit, "skipped", None).await?;
            return Ok(true);
        }
        let Ok(_one_at_a_time) = self.browser_work(persona.engine).try_lock() else {
            return Ok(false);
        };
        let sidecar = self.sidecars.of(persona.engine);
        let context = sidecar
            .open_context(&open_request(persona, &endpoint)?)
            .await?
            .context_id;
        domains.record_request(persona, &domain).await?;
        let timeout_ms =
            u64::try_from(self.config.selftest.browser.page_timeout.get().as_millis()).ok();
        let navigation = sidecar
            .navigate_checked(
                context,
                &visit_request(persona, visit, timeout_ms),
                self.config.security,
                usize::try_from(self.config.http.max_redirects).unwrap_or(usize::MAX),
            )
            .await;
        if let Err(error) = sidecar.close_context(context).await {
            tracing::warn!(persona = %persona.id, %error, "a warm-up context could not be closed");
        }
        // The request went out, or may have: the visit is done either way.
        let (outcome, status) = match &navigation {
            Ok(navigation) => classify(navigation),
            Err(error) => {
                tracing::warn!(persona = %persona.id, %error, "a warm-up navigation failed");
                (Outcome::ProviderError, None)
            }
        };
        domains
            .record_outcome(
                persona,
                &domain,
                outcome,
                Cooldowns::default().after(outcome),
            )
            .await?;
        if matches!(outcome, Outcome::Ok | Outcome::ChallengePassed) {
            self.pool.report_traffic_ok(proxy.id);
        }
        self.visit_done(persona, visit, outcome.as_str(), status)
            .await?;
        Ok(true)
    }

    async fn visit_done(
        &self,
        persona: &Persona,
        visit: &PlannedVisit,
        outcome: &str,
        status: Option<i64>,
    ) -> Result<(), PersonaError> {
        self.store
            .personas()
            .modify(persona.id, |persona| {
                persona.warmup_visits = persona.warmup_visits.saturating_add(1);
            })
            .await?;
        self.event(
            persona.id,
            "persona_warmup_visit",
            if outcome == Outcome::Ok.as_str() || outcome == Outcome::ChallengePassed.as_str() {
                AlertSeverity::Info
            } else {
                AlertSeverity::Warning
            },
            json!({
                "number": visit.number,
                "domain": visit.domain(),
                "outcome": outcome,
                "status": status,
            }),
        )
        .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use kage_browser::Navigation;
    use kage_browser::contract::{
        DwellMode, EXAMPLES_JSON, EntryStrategy, NavigationError, NavigationErrorCode, PageResult,
    };
    use kage_core::domain::Engine;
    use kage_core::outcome::Outcome;
    use kage_core::persona::warmup::PlannedVisit;

    use super::{classify, visit_request};
    use crate::test_support::persona;

    fn page() -> PageResult {
        let examples: serde_json::Value = serde_json::from_str(EXAMPLES_JSON).unwrap();
        let mut page: PageResult =
            serde_json::from_value(examples["PageResult"][0].clone()).unwrap();
        page.error = None;
        page.status = Some(200);
        page.challenge.interstitial = false;
        page.challenge.resolved = None;
        page
    }

    fn outcome_of(change: impl FnOnce(&mut PageResult)) -> Outcome {
        let mut page = page();
        change(&mut page);
        classify(&Navigation::Page(Box::new(page))).0
    }

    #[test]
    fn a_visit_is_judged_by_what_the_browser_reports() {
        assert_eq!(
            classify(&Navigation::Page(Box::new(page()))),
            (Outcome::Ok, Some(200))
        );
        assert_eq!(outcome_of(|page| page.status = Some(404)), Outcome::Ok);
        assert_eq!(
            outcome_of(|page| page.status = Some(429)),
            Outcome::RateLimited
        );
        assert_eq!(
            outcome_of(|page| page.status = Some(403)),
            Outcome::Forbidden
        );
        assert_eq!(
            outcome_of(|page| page.status = Some(503)),
            Outcome::ProviderError
        );
        // An interstitial outranks the status it was served with.
        assert_eq!(
            outcome_of(|page| {
                page.status = Some(403);
                page.challenge.interstitial = true;
                page.challenge.resolved = Some(false);
            }),
            Outcome::ChallengeFailed
        );
        assert_eq!(
            outcome_of(|page| {
                page.challenge.interstitial = true;
                page.challenge.resolved = Some(true);
            }),
            Outcome::ChallengePassed
        );
        // A navigation that did not arrive.
        for (code, outcome) in [
            (NavigationErrorCode::Timeout, Outcome::Timeout),
            (NavigationErrorCode::ProxyFailed, Outcome::ProviderError),
            (
                NavigationErrorCode::NavigationFailed,
                Outcome::ProviderError,
            ),
            (NavigationErrorCode::Aborted, Outcome::ProviderError),
        ] {
            assert_eq!(
                outcome_of(|page| {
                    page.status = None;
                    page.error = Some(NavigationError {
                        code,
                        message: "did not arrive".to_owned(),
                        net_error: None,
                    });
                }),
                outcome
            );
        }
    }

    #[test]
    fn a_visit_is_typed_read_and_scrolled_and_captures_nothing() {
        let persona = persona(Engine::Firefox, 152);
        let visit = |number| PlannedVisit {
            number,
            url: "https://de.wikipedia.org/wiki/Special:Random"
                .parse()
                .unwrap(),
            due_at: Utc::now(),
        };

        let request = visit_request(&persona, &visit(2), Some(45_000));

        assert_eq!(request.url, "https://de.wikipedia.org/wiki/Special:Random");
        assert_eq!(request.timeout_ms, Some(45_000));
        let behavior = request.behavior.clone().unwrap();
        assert_eq!(behavior.dwell, Some(DwellMode::Human));
        assert_eq!(behavior.entry.unwrap().strategy, EntryStrategy::Direct);
        assert_eq!(behavior.wander, Some(true));
        let depth = behavior.scroll_depth.unwrap();
        assert!((0.6..=0.9).contains(&depth), "{depth}");
        assert_eq!(behavior.seed.as_deref().map(str::len), Some(64));
        let capture = &request.capture;
        assert!(!capture.dom && !capture.raw && !capture.console);
        assert!(capture.screenshot.is_none() && capture.pdf.is_none() && capture.xhr.is_none());

        // The same visit is the same every time; another visit, and
        // another persona, behave differently.
        assert_eq!(request, visit_request(&persona, &visit(2), Some(45_000)));
        let other_visit = visit_request(&persona, &visit(3), Some(45_000));
        assert_ne!(other_visit.behavior.unwrap().seed, behavior.seed);
        let other = self::persona(Engine::Firefox, 152);
        let other_persona = visit_request(&other, &visit(2), Some(45_000));
        assert_ne!(other_persona.behavior.unwrap().seed, behavior.seed);
    }
}
