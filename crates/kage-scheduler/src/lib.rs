//! The scheduler (spec §3.2, §8, §11, §12): which request leaves when, on
//! which persona, what its attempt came to, and what follows.
//!
//! The rules themselves are pure and live in `kage_core::schedule`; this
//! crate gives them a clock, a store, the two fetch paths and the wire.
//!
//! - [`scheduler`]: the [`Scheduler`], which queues attempts and grants
//!   them a persona and its slot, and the gate the persona manager's own
//!   browser work asks ([`SchedulerGate`], follow-up F27).
//! - [`pipeline`]: the [`Pipeline`], one fetch from the request to its
//!   stored result: the render decision, retries with a persona switch,
//!   escalation from the http path to the browser, the wall-time hook.
//! - [`executor`]: one attempt on the http path or in the persona's
//!   browser, with the behavior the domain policy asks for and the cookie
//!   handoff around it.
//! - [`verdicts`]: the classifier's verdict and what follows from it:
//!   cooldowns scaled by confidence, the breaker.
//! - [`archive`]: what a request keeps of its page, stored as artifacts
//!   with its result.
//! - [`robots`]: the rules of `robots.txt` (spec §14): whether a site's
//!   file is asked for, who asks, what is kept of the answer.
//! - [`forwarder`]: the one proxy every persona's traffic leaves through
//!   (follow-up F10): the provider's credentials stay in this process and
//!   every byte is counted.
//! - [`bandwidth`]: the bandwidth budget of the proxy plan (decision D5).
//! - [`backend`]: what the scheduler reads and writes outside itself, over
//!   Postgres ([`StoreBackend`]) or in memory ([`MemoryBackend`]).
//! - [`metrics`]: the scheduler's metrics (spec §20).

#![forbid(unsafe_code)]

pub mod archive;
pub mod backend;
pub mod bandwidth;
mod error;
pub mod executor;
pub mod forwarder;
mod memory;
pub mod metrics;
pub mod pipeline;
pub mod robots;
pub mod scheduler;
pub mod verdicts;

use kage_core::config::Config;
use kage_core::schedule::planner::PlannerRules;
use kage_core::schedule::politeness::ServerErrorRules;
use kage_core::schedule::retry::CooldownRules;

pub use archive::{Archive, Archived, Keep, StoreArchive};
pub use backend::{Backend, CacheHit, PoolRoutes, StoreBackend};
pub use bandwidth::{Bandwidth, Ledger, StoreLedger, Term};
pub use error::SchedulerError;
pub use executor::{
    Arrival, AttemptExecutor, AttemptJob, AttemptOutput, BehaviorApplied, BehaviorWish, FlowPlan,
    FlowRun, FlowStep, PathExecutor, Reached,
};
pub use forwarder::{Forwarder, ForwarderEgress, ForwarderSettings, Meter, Routes, Tokens};
pub use memory::MemoryBackend;
pub use pipeline::{
    CachePlan, Capture, CrawlReading, Deferred, FetchOrder, FetchReport, INTERRUPTED, JobFetch,
    Kept, PacingReport, Peek, Pipeline, PipelineSettings, ROBOTS_NO_SCOUT, ROBOTS_UNAVAILABLE,
    ReadLink, RobotsRead, Running, ScoutNeed, Stop, Synchronous, WoundDown,
};
pub use robots::{Conflict, RobotsSettings, RobotsWish};
pub use scheduler::{Grant, Refused, Scheduler, SchedulerGate};
pub use verdicts::Verdicts;

/// Name of this crate as recorded in its manifest.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// The planner's rules as the configuration sets them.
pub fn planner_rules(config: &Config) -> PlannerRules {
    let scheduler = &config.scheduler;
    PlannerRules {
        max_browser_sessions: scheduler.max_browser_sessions,
        max_http_inflight: scheduler.max_http_inflight,
        max_per_engine: scheduler.max_contexts_per_engine,
        visitor_wait: scheduler.visitor_wait.get(),
        visit_idle: scheduler.visit_idle.get(),
        visit_start_spacing: scheduler.visit_start_spacing.distribution(),
        day: scheduler.active_hours.rules(),
        server_errors: ServerErrorRules {
            after: scheduler.server_errors.after,
            pause: scheduler.server_errors.pause.get(),
            max_pause: scheduler.server_errors.max_pause.get(),
        },
        breaker_max_pause: scheduler.breaker_max_pause.get(),
        breaker_trial_timeout: scheduler.breaker_trial_timeout.get(),
    }
}

/// How a cooldown is made from the policy's, as the configuration says.
pub fn cooldown_rules(config: &Config) -> CooldownRules {
    CooldownRules {
        floor: config.scheduler.cooldown_floor.get(),
        jitter: config.scheduler.cooldown_jitter,
    }
}

/// What the scheduler will do with this configuration, for `kage config
/// check`.
pub fn describe(config: &Config) -> serde_json::Value {
    let scheduler = &config.scheduler;
    serde_json::json!({
        "max_browser_sessions": scheduler.max_browser_sessions,
        "max_http_inflight": scheduler.max_http_inflight,
        "max_contexts_per_engine": scheduler.max_contexts_per_engine,
        "sync_wall_time_s": scheduler.sync_wall_time.get().as_secs(),
        "bandwidth": {
            "cap_bytes": config.bandwidth.cap_bytes(),
            "warn_at": config.bandwidth.warn_at,
            "hard_stop": config.bandwidth.hard_stop,
        },
        "forwarder": {
            "bind": config.forwarder.bind.to_string(),
            "advertise": config.forwarder.advertise,
        },
        // Whether a site's robots.txt is asked for at all is its policy's.
        "robots": {
            "ttl_s": config.robots.ttl.get().as_secs(),
            "error_ttl_s": config.robots.error_ttl.get().as_secs(),
            "max_bytes": config.robots.max_bytes,
            "agents": config.robots.agents,
        },
        "cache": {
            "max_ttl_s": config.cache.max_ttl.get().as_secs(),
        },
        "extract": {
            "thin_words": config.extract.thin_words,
        },
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_core::config::Config;

    use super::{CRATE_NAME, cooldown_rules, describe, planner_rules};

    fn config() -> Config {
        Config::load(
            None,
            [
                ("KAGE__DATABASE__URL", "postgres://kage@localhost/kage"),
                ("KAGE__SIDECAR__TOKEN", "token"),
                (
                    "KAGE__SECRETS__MASTER_KEY",
                    "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
                ),
                ("KAGE__SCHEDULER__MAX_BROWSER_SESSIONS", "4"),
                ("KAGE__SCHEDULER__COOLDOWN_JITTER", "0.2"),
            ]
            .map(|(key, value)| (key.to_owned(), value.to_owned())),
        )
        .unwrap()
    }

    #[test]
    fn smoke() {
        assert_eq!(CRATE_NAME, "kage-scheduler");
    }

    #[test]
    fn the_rules_are_the_configurations() {
        let config = config();

        let rules = planner_rules(&config);
        assert_eq!(rules.max_browser_sessions, 4);
        assert_eq!(rules.max_http_inflight, 20);
        assert_eq!(rules.max_per_engine, 3);
        assert_eq!(rules.visitor_wait, Duration::from_secs(5));
        assert_eq!(rules.visit_idle, Duration::from_mins(10));
        assert_eq!(rules.visit_start_spacing.min, Some(150.0));
        assert_eq!(rules.server_errors.after, 3);
        assert_eq!(rules.server_errors.pause, Duration::from_secs(30));
        assert_eq!(rules.server_errors.max_pause, Duration::from_mins(10));
        assert_eq!(rules.breaker_max_pause, Duration::from_hours(6));
        assert_eq!(rules.breaker_trial_timeout, Duration::from_mins(10));
        assert_eq!(rules.day, config.scheduler.active_hours.rules());

        let cooldowns = cooldown_rules(&config);
        assert_eq!(cooldowns.floor, Duration::from_mins(5));
        assert!((cooldowns.jitter - 0.2).abs() < 1e-12);

        let described = describe(&config);
        assert_eq!(described["max_browser_sessions"], 4);
        assert_eq!(described["sync_wall_time_s"], 180);
        assert_eq!(described["bandwidth"]["cap_bytes"], 250_000_000_000_u64);
        assert_eq!(described["bandwidth"]["hard_stop"], true);
        assert_eq!(described["forwarder"]["advertise"], "http://kage:8899");
        assert_eq!(described["robots"]["ttl_s"], 86_400);
        assert_eq!(described["robots"]["error_ttl_s"], 3600);
        assert_eq!(described["robots"]["max_bytes"], 524_288);
        assert_eq!(described["robots"]["agents"], serde_json::json!(["kage"]));
        assert_eq!(described["cache"]["max_ttl_s"], 86_400);
        assert_eq!(described["extract"]["thin_words"], 80);
    }
}
