//! The response cache on the way of a fetch (spec §15).
//!
//! A fetch that names a lifetime is answered from the cache when its key
//! holds a result that is young enough: no persona, no visit, no pacing,
//! nothing asked of the site, and so it is answered while the site is
//! paused or the pool sleeps. The answer is a request and a result of its
//! own, pointing at the bodies that are stored already. A fetch that is
//! not answered from the cache fills it, when its result is one worth
//! keeping.

use std::time::Duration;

use chrono::{DateTime, Utc};
use kage_core::domain::{ArtifactKind, ArtifactRef, FetchRecord};
use kage_core::ids::{ApiKeyId, ArtifactId, ResultId};
use kage_store::repo::ResponseCacheEntry;
use serde_json::Value;

use super::{Course, FetchOrder, FetchReport, PacingReport, Pipeline, visit};
use crate::SchedulerError;
use crate::backend::CacheHit;
use crate::metrics;

/// What a fetch may do with the response cache (spec §15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePlan {
    /// The key: everything that changes what the result holds
    /// ([`kage_core::cache::CacheKey`]).
    pub key: String,
    /// The normalized URL, for whoever looks at the table.
    pub url_key: String,
    /// The key an entry serves; `None` where the site's policy shares its
    /// pages between keys.
    pub scope: Option<ApiKeyId>,
    /// How long the result may be served: an entry written for the fetch
    /// lives this long, and one that is read for it is no older.
    pub ttl: Duration,
    /// Whether the fetch may be answered from the cache. A crawl's page
    /// only fills it: a page that is not visited cannot be clicked from.
    pub read: bool,
}

/// Whether an entry is young enough for a fetch that accepts results up
/// to `ttl` old: nobody gets a page older than they asked for.
fn young_enough(cached_at: DateTime<Utc>, ttl: Duration, now: DateTime<Utc>) -> bool {
    (now - cached_at).to_std().is_ok_and(|age| age <= ttl)
}

/// Whether a result is one the cache keeps: a page that was fetched
/// (never a block, a failure, a refused redirect or an error status) and
/// whose extraction, when there was one, found more than next to nothing.
fn worth_keeping(record: &FetchRecord, thin: bool) -> bool {
    record.outcome.is_success()
        && record
            .status
            .is_some_and(|status| (200..300).contains(&status))
        && !thin
        && !record.cached
}

/// Whether an order has anything to do with the cache at all: a flow is a
/// script in a browser, what a session sees depends on its cookies, and a
/// scout's files are kept elsewhere.
pub(super) fn plan_of(order: &FetchOrder) -> Option<&CachePlan> {
    order
        .cache
        .as_ref()
        .filter(|_| order.session.is_none() && order.flow.is_none() && !order.scout)
}

impl Pipeline {
    /// The entry an order may be answered with, when the cache has one.
    async fn hit(&self, order: &FetchOrder) -> Result<Option<CacheHit>, SchedulerError> {
        let Some(plan) = plan_of(order).filter(|plan| plan.read) else {
            return Ok(None);
        };
        let now = self.now();
        Ok(self
            .backend()
            .cached(&plan.key, now)
            .await?
            .filter(|hit| young_enough(hit.entry.cached_at, plan.ttl, now)))
    }

    /// Whether the cache holds what the order would be answered with: for
    /// a caller that counts pages before it asks, and counts none for a
    /// fetch the cache answers.
    ///
    /// # Errors
    ///
    /// Fails when the backend fails.
    pub async fn has_cached(&self, order: &FetchOrder) -> Result<bool, SchedulerError> {
        Ok(self.hit(order).await?.is_some())
    }

    /// A reference of the request to a stored body, or `None` when the
    /// body is gone or nothing keeps bodies.
    async fn link(&self, course: &Course, artifact: ArtifactRef) -> Option<ArtifactRef> {
        let archive = self.archive.as_ref()?;
        archive.link(course.request.id, artifact, self.now()).await
    }

    /// The captured XHR responses of a stored result with their bodies
    /// pointed at for this request. `None` when a body is gone.
    async fn link_xhr(&self, course: &Course, xhr: Option<&Value>) -> Option<Option<Value>> {
        let Some(xhr) = xhr else {
            return Some(None);
        };
        let mut linked = xhr.clone();
        if let Some(responses) = linked.as_array_mut() {
            for response in responses {
                let Some(id) = response
                    .get("body_artifact")
                    .and_then(|id| serde_json::from_value::<ArtifactId>(id.clone()).ok())
                else {
                    continue;
                };
                let body = ArtifactRef {
                    kind: ArtifactKind::XhrBody,
                    id,
                };
                response["body_artifact"] = serde_json::json!(self.link(course, body).await?.id);
            }
        }
        Some(Some(linked))
    }

    /// Answers a request from the cache when it may be and the cache has
    /// its page: stores the result under the request and reports it. A
    /// miss is counted by whoever then fetches the page
    /// ([`Pipeline::board`]).
    pub(super) async fn serve_cached(
        &self,
        course: &Course,
    ) -> Result<Option<FetchReport>, SchedulerError> {
        let order = &course.order;
        if plan_of(order).is_none_or(|plan| !plan.read) {
            return Ok(None);
        }
        let Some(hit) = self.hit(order).await? else {
            return Ok(None);
        };
        // The bodies: rows of this request that point at what is stored,
        // so that neither a purge nor retention of the one that filled the
        // entry takes anything from this one.
        let mut artifacts = Vec::with_capacity(hit.record.artifacts.len());
        for artifact in &hit.record.artifacts {
            let Some(linked) = self.link(course, *artifact).await else {
                return Ok(None);
            };
            artifacts.push(linked);
        }
        let Some(xhr) = self.link_xhr(course, hit.record.xhr.as_ref()).await else {
            return Ok(None);
        };
        let now = self.now();
        let stored = hit.record;
        // The request's own pacing: it waited for nobody.
        let pacing = PacingReport {
            applied: course.pacing.applied.clone(),
            request_ignored: course.pacing.request_ignored,
            waited: Duration::ZERO,
        };
        let record = FetchRecord {
            id: ResultId::new(),
            request_id: course.request.id,
            outcome: stored.outcome,
            status: stored.status,
            headers: stored.headers,
            final_url: stored.final_url,
            redirects: stored.redirects,
            extract: stored.extract,
            xhr,
            artifacts,
            // Who fetched the page is the business of the key that had it
            // fetched.
            persona: stored.persona.filter(|_| hit.filled_by == order.api_key),
            block: None,
            robots: stored.robots,
            cached: true,
            cached_at: Some(hit.entry.cached_at),
            reason: None,
            visit: visit::of_cached(stored.visit, visit::pacing(&pacing)),
            reading: None,
            created_at: now,
        };
        self.backend().finish(&record).await?;
        metrics::cache("hit");
        tracing::info!(
            request = %record.request_id,
            domain = course.domain,
            cached_at = %hit.entry.cached_at,
            "answered from the cache"
        );
        Ok(Some(FetchReport {
            request: record.request_id,
            result: record.id,
            outcome: record.outcome,
            reason: None,
            retry_at: None,
            attempts: Vec::new(),
            last: None,
            headers: record.headers.clone(),
            artifacts: record.artifacts.clone(),
            xhr: record.xhr.clone(),
            finished_at: now,
            persona: record.persona.clone(),
            block: None,
            pacing,
            escalated: false,
            extract: record.extract.clone(),
            robots: record.robots.clone(),
            page: None,
            reading: None,
            visit: record.visit.clone(),
            cached: Some(Box::new(record)),
        }))
    }

    /// Puts the result of a fetch into the cache when the fetch named a
    /// lifetime and the result is worth keeping. Never fails the fetch:
    /// what could not be written is a warning.
    pub(super) async fn fill_cache(&self, order: &FetchOrder, record: &FetchRecord, thin: bool) {
        let Some(plan) = plan_of(order) else {
            return;
        };
        if !worth_keeping(record, thin) {
            metrics::cache("bypass");
            return;
        }
        let Some(expires_at) = chrono::Duration::from_std(plan.ttl)
            .ok()
            .and_then(|ttl| record.created_at.checked_add_signed(ttl))
        else {
            return;
        };
        let entry = ResponseCacheEntry {
            cache_key: plan.key.clone(),
            url_key: plan.url_key.clone(),
            result_id: record.id,
            api_key_id: plan.scope,
            cached_at: record.created_at,
            expires_at,
        };
        match self.backend().cache(&entry).await {
            Ok(()) => metrics::cache("stored"),
            Err(error) => {
                tracing::warn!(request = %record.request_id, %error, "a result could not be put into the cache");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, TimeZone, Utc};
    use kage_core::domain::{FetchRecord, Visit};
    use kage_core::ids::{RequestId, ResultId};
    use kage_core::outcome::Outcome;

    use super::{worth_keeping, young_enough};

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 13, 9, 0, 0).unwrap()
    }

    #[test]
    fn nobody_gets_a_page_older_than_they_asked_for() {
        let ttl = Duration::from_mins(10);
        let later = |seconds| at() + chrono::Duration::seconds(seconds);
        assert!(young_enough(at(), ttl, at()));
        assert!(young_enough(at(), ttl, later(599)));
        assert!(young_enough(at(), ttl, later(600)));
        assert!(!young_enough(at(), ttl, later(601)));
        // A clock that went back never makes an entry younger than new.
        assert!(!young_enough(at(), ttl, later(-1)));
        assert!(!young_enough(at(), Duration::ZERO, later(1)));
    }

    #[test]
    fn the_cache_keeps_a_page_that_was_fetched_and_says_something() {
        let record = |outcome, status| FetchRecord {
            id: ResultId::new(),
            request_id: RequestId::new(),
            outcome,
            status,
            headers: Vec::new(),
            final_url: None,
            redirects: Vec::new(),
            extract: None,
            xhr: None,
            artifacts: Vec::new(),
            persona: None,
            block: None,
            robots: None,
            cached: false,
            cached_at: None,
            reason: None,
            visit: Visit::default(),
            reading: None,
            created_at: at(),
        };
        for (outcome, status) in [
            (Outcome::Ok, Some(200)),
            (Outcome::Ok, Some(299)),
            (Outcome::ChallengePassed, Some(200)),
        ] {
            assert!(worth_keeping(&record(outcome, status), false));
            // What extraction called thin is not kept.
            assert!(!worth_keeping(&record(outcome, status), true));
        }
        for (outcome, status) in [
            (Outcome::Ok, Some(199)),
            (Outcome::Ok, Some(300)),
            (Outcome::Ok, Some(404)),
            (Outcome::Ok, None),
            (Outcome::Forbidden, Some(403)),
            (Outcome::SoftBlock, Some(200)),
            (Outcome::Captcha, Some(200)),
            (Outcome::ProviderError, Some(200)),
            (Outcome::Timeout, None),
            (Outcome::SsrfDenied, Some(200)),
            (Outcome::RobotsDenied, None),
        ] {
            assert!(
                !worth_keeping(&record(outcome, status), false),
                "{outcome:?} {status:?}"
            );
        }
        // What came from the cache is not put into it again.
        let served = FetchRecord {
            cached: true,
            cached_at: Some(at()),
            ..record(Outcome::Ok, Some(200))
        };
        assert!(!worth_keeping(&served, false));
    }
}
