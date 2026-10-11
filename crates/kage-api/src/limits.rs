//! Rate limits (spec §16.2, §19): requests per minute over all keys
//! together, and per key requests per minute, requests in flight and pages
//! a day.
//!
//! - **Per minute.** A token bucket per key and one for the whole API
//!   (`governor`): a key with 60 a minute may send 60 at once and then one
//!   a second. A refusal says how long to wait (`Retry-After`).
//! - **In flight.** A request holds a permit of its key from the moment it
//!   is admitted until its answer's head is written. A fetch that outlasts
//!   its wall time gives the permit back with its `202`.
//! - **Pages a day.** Every fetch a key starts is a page, whatever came of
//!   it. The count is the key's requests since midnight UTC in the store,
//!   so a restart forgets nothing, read once a day per key and counted on
//!   in memory.
//!
//! The limits are a key's own (`api_keys`); a key that was made again with
//! other limits gets a new state the next time it is seen.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDate, Utc};
use governor::clock::{Clock as _, DefaultClock};
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use kage_core::domain::{ApiKey, KeyLimits};
use kage_core::ids::ApiKeyId;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::{ApiError, ErrorCode};

/// How long a key that is at its concurrency limit is told to wait.
const BUSY_RETRY: Duration = Duration::from_secs(1);

/// How long a key must have been quiet before what is kept for it is let
/// go: twice the minute after which its bucket is full again, so that
/// forgetting a key forgives it nothing.
const FORGET_AFTER: Duration = Duration::from_secs(120);

/// The pages a key has fetched on one day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DayCount {
    day: NaiveDate,
    pages: u64,
}

/// What is kept per key.
#[derive(Debug)]
struct KeyState {
    limits: KeyLimits,
    per_minute: DefaultDirectRateLimiter,
    in_flight: Arc<Semaphore>,
    day: Mutex<Option<DayCount>>,
    seen: Mutex<Instant>,
}

impl KeyState {
    fn new(limits: KeyLimits) -> Self {
        Self {
            limits,
            per_minute: RateLimiter::direct(quota(limits.requests_per_minute)),
            in_flight: Arc::new(Semaphore::new(
                usize::try_from(limits.max_concurrent).unwrap_or(usize::MAX),
            )),
            day: Mutex::new(None),
            seen: Mutex::new(Instant::now()),
        }
    }
}

/// `per_minute` requests a minute, as many at once.
fn quota(per_minute: u32) -> Quota {
    Quota::per_minute(NonZeroU32::new(per_minute).unwrap_or(NonZeroU32::MIN))
}

/// A request that was admitted: it counts toward its key's requests in
/// flight until this is dropped.
#[derive(Debug)]
pub(crate) struct Admitted {
    _permit: OwnedSemaphorePermit,
}

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct Limits {
    global: Option<DefaultDirectRateLimiter>,
    keys: Mutex<HashMap<ApiKeyId, Arc<KeyState>>>,
    clock: DefaultClock,
}

impl Limits {
    /// Limits with `requests_per_minute` over all keys together; 0 for no
    /// such limit.
    pub(crate) fn new(requests_per_minute: u32) -> Self {
        Self {
            global: (requests_per_minute > 0)
                .then(|| RateLimiter::direct(quota(requests_per_minute))),
            keys: Mutex::new(HashMap::new()),
            clock: DefaultClock::default(),
        }
    }

    fn state(&self, key: &ApiKey) -> Arc<KeyState> {
        let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        match keys.get(&key.id) {
            Some(state) if state.limits == key.limits => {
                *state.seen.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
                Arc::clone(state)
            }
            _ => {
                let state = Arc::new(KeyState::new(key.limits));
                keys.insert(key.id, Arc::clone(&state));
                state
            }
        }
    }

    /// Counts a request against the limit of the whole API, before anyone
    /// is asked who sent it.
    ///
    /// # Errors
    ///
    /// `rate_limited` with the time to wait.
    pub(crate) fn admit_any(&self) -> Result<(), ApiError> {
        let Some(global) = &self.global else {
            return Ok(());
        };
        global.check().map_err(|not_until| {
            ApiError::new(
                ErrorCode::RateLimited,
                "the API as a whole is at its limit of requests per minute",
            )
            .with("limit", "server.requests_per_minute")
            .retry_after(not_until.wait_time_from(self.clock.now()))
        })
    }

    /// Admits a request of a key: one of its requests per minute and one
    /// of its requests in flight.
    ///
    /// # Errors
    ///
    /// `rate_limited` or `concurrency_limited`, with the time to wait.
    pub(crate) fn admit(&self, key: &ApiKey) -> Result<Admitted, ApiError> {
        let state = self.state(key);
        state.per_minute.check().map_err(|not_until| {
            ApiError::new(
                ErrorCode::RateLimited,
                format!(
                    "the key is at its limit of {} requests per minute",
                    key.limits.requests_per_minute
                ),
            )
            .with("limit", "requests_per_minute")
            .retry_after(not_until.wait_time_from(self.clock.now()))
        })?;
        let permit = Arc::clone(&state.in_flight)
            .try_acquire_owned()
            .map_err(|_| {
                ApiError::new(
                    ErrorCode::ConcurrencyLimited,
                    format!(
                        "the key already has {} requests in flight",
                        key.limits.max_concurrent
                    ),
                )
                .with("limit", "max_concurrent")
                .retry_after(BUSY_RETRY)
            })?;
        Ok(Admitted { _permit: permit })
    }

    /// Counts one page against the key's daily budget. `stored` is asked
    /// for the pages the key has fetched since midnight when today's count
    /// is not known yet; it counts the page that is being asked for when
    /// that page's request is stored already, so call this first.
    ///
    /// # Errors
    ///
    /// `budget_exhausted` with the time until midnight UTC; whatever
    /// `stored` fails with.
    pub(crate) async fn spend_page<F>(
        &self,
        key: &ApiKey,
        now: DateTime<Utc>,
        stored: F,
    ) -> Result<(), ApiError>
    where
        F: AsyncFnOnce(DateTime<Utc>) -> Result<u64, ApiError>,
    {
        let Some(budget) = key.limits.daily_page_budget else {
            return Ok(());
        };
        let state = self.state(key);
        let today = now.date_naive();
        let known = state
            .day
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .filter(|count| count.day == today);
        let before = if let Some(count) = known {
            count.pages
        } else {
            let midnight = today
                .and_hms_opt(0, 0, 0)
                .map_or(now, |midnight| midnight.and_utc());
            stored(midnight).await?
        };
        let mut day = state.day.lock().unwrap_or_else(PoisonError::into_inner);
        // Another request of the key may have counted meanwhile.
        let pages = day
            .filter(|count| count.day == today)
            .map_or(before, |count| count.pages.max(before));
        if pages >= u64::from(budget) {
            *day = Some(DayCount { day: today, pages });
            drop(day);
            let tomorrow = today
                .succ_opt()
                .and_then(|day| day.and_hms_opt(0, 0, 0))
                .map_or(now, |midnight| midnight.and_utc());
            let wait = (tomorrow - now).to_std().unwrap_or(Duration::from_secs(1));
            return Err(ApiError::new(
                ErrorCode::BudgetExhausted,
                format!("the key has fetched its {budget} pages of today (UTC)"),
            )
            .with("limit", "daily_page_budget")
            .retry_after(wait));
        }
        *day = Some(DayCount {
            day: today,
            pages: pages + 1,
        });
        Ok(())
    }

    /// Forgets the keys that have nothing in flight and were not seen for
    /// two minutes, so that the map does not keep every key that was ever
    /// seen. By then a key's bucket is full again, and its day count is
    /// read from the store when it comes back.
    pub(crate) fn forget_idle(&self) {
        self.forget_idle_at(Instant::now());
    }

    fn forget_idle_at(&self, now: Instant) {
        self.keys
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|_, state| {
                let busy = state.in_flight.available_permits()
                    < usize::try_from(state.limits.max_concurrent).unwrap_or(usize::MAX);
                let seen = *state.seen.lock().unwrap_or_else(PoisonError::into_inner);
                busy || now.saturating_duration_since(seen) < FORGET_AFTER
            });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    use chrono::{TimeZone, Utc};
    use kage_core::domain::{ApiKey, KeyLimits, Scope};
    use kage_core::ids::ApiKeyId;

    use super::{FORGET_AFTER, Limits};
    use crate::error::{ApiError, ErrorCode};

    fn key(limits: KeyLimits) -> ApiKey {
        ApiKey {
            id: ApiKeyId::new(),
            name: "test".to_owned(),
            secret_hash: [0; 32],
            scopes: BTreeSet::from([Scope::Fetch]),
            limits,
            disabled_at: None,
            created_at: Utc::now(),
            last_used_at: None,
        }
    }

    const fn limits(per_minute: u32, concurrent: u32, budget: Option<u32>) -> KeyLimits {
        KeyLimits {
            requests_per_minute: per_minute,
            max_concurrent: concurrent,
            daily_page_budget: budget,
        }
    }

    #[test]
    fn a_key_gets_its_requests_per_minute_and_then_a_time_to_wait() {
        let all = Limits::new(0);
        let key = key(limits(3, 10, None));

        for _ in 0..3 {
            drop(all.admit(&key).unwrap());
        }
        let refused = all.admit(&key).unwrap_err();

        assert_eq!(refused.code(), ErrorCode::RateLimited);
        let problem = refused.problem();
        assert_eq!(problem.extensions["limit"], "requests_per_minute");
        // Three a minute is one every twenty seconds.
        let wait = problem.extensions["retry_after_s"].as_u64().unwrap();
        assert!((15..=20).contains(&wait), "{wait}");
        // Another key is not held by it.
        drop(all.admit(&self::key(limits(3, 10, None))).unwrap());
    }

    #[test]
    fn a_key_has_only_so_many_requests_in_flight() {
        let all = Limits::new(0);
        let key = key(limits(1000, 2, None));

        let first = all.admit(&key).unwrap();
        let _second = all.admit(&key).unwrap();
        let refused = all.admit(&key).unwrap_err();
        assert_eq!(refused.code(), ErrorCode::ConcurrencyLimited);
        assert_eq!(refused.problem().extensions["limit"], "max_concurrent");

        // A finished request makes room.
        drop(first);
        drop(all.admit(&key).unwrap());
    }

    #[test]
    fn the_whole_api_has_a_limit_of_its_own() {
        let all = Limits::new(2);

        all.admit_any().unwrap();
        all.admit_any().unwrap();
        let refused = all.admit_any().unwrap_err();
        assert_eq!(refused.code(), ErrorCode::RateLimited);
        assert_eq!(
            refused.problem().extensions["limit"],
            "server.requests_per_minute"
        );

        // Zero switches it off.
        let open = Limits::new(0);
        for _ in 0..1000 {
            open.admit_any().unwrap();
        }
    }

    #[tokio::test]
    async fn a_days_pages_are_counted_from_the_store_once_and_end_at_the_budget() {
        let all = Limits::new(0);
        let key = key(limits(1000, 10, Some(5)));
        let noon = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
        let asked = AtomicU64::new(0);
        // Three pages were fetched before this process started.
        let stored = async |since: chrono::DateTime<Utc>| -> Result<u64, ApiError> {
            assert_eq!(since, Utc.with_ymd_and_hms(2026, 10, 11, 0, 0, 0).unwrap());
            asked.fetch_add(1, Ordering::Relaxed);
            Ok(3)
        };

        all.spend_page(&key, noon, stored).await.unwrap();
        all.spend_page(&key, noon, stored).await.unwrap();
        let refused = all.spend_page(&key, noon, stored).await.unwrap_err();

        assert_eq!(refused.code(), ErrorCode::BudgetExhausted);
        // The store was asked once; the rest was counted here.
        assert_eq!(asked.load(Ordering::Relaxed), 1);
        // Twelve hours until midnight UTC.
        assert_eq!(refused.problem().extensions["retry_after_s"], 12 * 3600);

        // The next day starts at what the store says then.
        let next_day = Utc.with_ymd_and_hms(2026, 10, 12, 0, 0, 1).unwrap();
        let fresh = async |_: chrono::DateTime<Utc>| -> Result<u64, ApiError> { Ok(0) };
        all.spend_page(&key, next_day, fresh).await.unwrap();

        // A key without a budget is never asked about.
        let unlimited = self::key(limits(1000, 10, None));
        let never = async |_: chrono::DateTime<Utc>| -> Result<u64, ApiError> {
            Err(ApiError::new(ErrorCode::Internal, "must not be asked"))
        };
        all.spend_page(&unlimited, noon, never).await.unwrap();
        // A store that fails is the answer.
        let failing = self::key(limits(1000, 10, Some(1)));
        assert_eq!(
            all.spend_page(&failing, noon, never)
                .await
                .unwrap_err()
                .code(),
            ErrorCode::Internal
        );
    }

    #[test]
    fn a_key_made_again_with_other_limits_starts_anew_and_quiet_keys_are_forgotten() {
        let all = Limits::new(0);
        let mut key = key(limits(1, 1, None));
        drop(all.admit(&key).unwrap());
        assert_eq!(all.admit(&key).unwrap_err().code(), ErrorCode::RateLimited);
        // Forgetting forgives nothing: a key that was just seen keeps its
        // count, in flight or not.
        all.forget_idle();
        assert_eq!(all.admit(&key).unwrap_err().code(), ErrorCode::RateLimited);

        key.limits = limits(5, 1, None);
        let held = all.admit(&key).unwrap();

        // Long after, a key with a request in flight is still known.
        let later = Instant::now() + FORGET_AFTER;
        all.forget_idle_at(later);
        assert_eq!(all.keys.lock().unwrap().len(), 1);
        drop(held);
        all.forget_idle_at(later);
        assert_eq!(all.keys.lock().unwrap().len(), 0);
    }
}
