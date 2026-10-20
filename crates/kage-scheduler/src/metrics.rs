//! The scheduler's metrics (spec §20), recorded through the `metrics`
//! facade. Nothing exports them until the exporter is wired (M8, M12).
//!
//! | Metric | Labels | What |
//! |---|---|---|
//! | `kage_requests_total` | `domain`, `level`, `outcome`, `vendor` | fetches that ended |
//! | `kage_attempt_duration_seconds` | `level`, `outcome` | how long an attempt took |
//! | `kage_attempts_total` | `domain`, `level`, `outcome`, `vendor` | attempts that ended |
//! | `kage_scheduler_wait_seconds` | `kind` | how long work waited for a persona and its slot |
//! | `kage_queue_depth` | `kind` | attempts waiting, by `sync` and `job` |
//! | `kage_scheduler_in_flight` | `render` | attempts in flight |
//! | `kage_scheduler_refusals_total` | `reason` | requests answered without an attempt |
//! | `kage_breaker_open` | `domain` | 1 while a domain is paused |
//! | `kage_policy_breaker_threshold` | `domain` | the blocks per hour that pause a domain under its policy, set at each attempt there |
//! | `kage_bandwidth_bytes_total` | `direction`, `egress` | bytes through the forwarder, by the exit they left through (`pool`, `host`) |
//! | `kage_bandwidth_budget_ratio` | | the share of the bandwidth cap that is used |
//! | `kage_cache_requests_total` | `result` | what the response cache did for a request: `hit`, `miss`, `stored`, `bypass`, `joined` |
//! | `kage_robots_requests_total` | `result` | what came of asking for a `robots.txt`: `file`, `missing`, `unreachable`, `not_asked`, `no_scout` |
//! | `kage_robots_denied_total` | `reason` | fetches that ended `robots_denied`: `rule`, `robots_unavailable`, `robots_no_scout`, `robots_enforced`, `robots_ignored` |
//! | `kage_extract_duration_seconds` | | how long reading a page's document took |
//!
//! A domain is a label of unbounded cardinality in principle; in practice
//! it is the set of sites the owner's projects fetch. A level is its
//! number, `0` to `3` (spec §25).

use std::time::Duration;

use kage_core::domain::{Exit, Level, Render};
use kage_core::outcome::Outcome;
use kage_core::schedule::planner::Refusal;
use kage_core::schedule::queue::Priority;

/// The label of an attempt or a request without a vendor.
const NO_VENDOR: &str = "none";

fn vendor_label(vendor: &str) -> String {
    if vendor.is_empty() {
        NO_VENDOR.to_owned()
    } else {
        vendor.to_owned()
    }
}

/// A fetch ended.
pub fn request(domain: &str, level: Level, outcome: Outcome, vendor: &str) {
    metrics::counter!(
        "kage_requests_total",
        "domain" => domain.to_owned(),
        "level" => level.label(),
        "outcome" => outcome.as_str(),
        "vendor" => vendor_label(vendor),
    )
    .increment(1);
}

/// An attempt ended.
pub fn attempt(domain: &str, level: Level, outcome: Outcome, vendor: &str, took: Duration) {
    metrics::counter!(
        "kage_attempts_total",
        "domain" => domain.to_owned(),
        "level" => level.label(),
        "outcome" => outcome.as_str(),
        "vendor" => vendor_label(vendor),
    )
    .increment(1);
    metrics::histogram!(
        "kage_attempt_duration_seconds",
        "level" => level.label(),
        "outcome" => outcome.as_str(),
    )
    .record(took.as_secs_f64());
}

/// Work waited this long for a persona and its slot.
pub fn waited(priority: Priority, waited: Duration) {
    metrics::histogram!("kage_scheduler_wait_seconds", "kind" => priority.as_str())
        .record(waited.as_secs_f64());
}

/// How much waits.
pub fn queue(sync: usize, job: usize) {
    // Queue depths are far below 2^52.
    #[allow(clippy::cast_precision_loss)]
    for (kind, depth) in [(Priority::Sync, sync), (Priority::Job, job)] {
        metrics::gauge!("kage_queue_depth", "kind" => kind.as_str()).set(depth as f64);
    }
}

/// How much is in flight.
pub fn in_flight(browser: u32, http: u32) {
    for (render, count) in [(Render::Browser, browser), (Render::Http, http)] {
        metrics::gauge!("kage_scheduler_in_flight", "render" => render.as_str())
            .set(f64::from(count));
    }
}

/// A request was answered without an attempt.
pub fn refused(reason: Refusal) {
    metrics::counter!("kage_scheduler_refusals_total", "reason" => reason.as_str()).increment(1);
}

/// The blocks per hour that pause a domain under its policy (spec §11),
/// for the alert that compares a domain's blocks with it (spec §20).
pub fn breaker_threshold(domain: &str, threshold: u32) {
    metrics::gauge!("kage_policy_breaker_threshold", "domain" => domain.to_owned())
        .set(f64::from(threshold));
}

/// A domain's breaker opened or closed.
pub fn breaker(domain: &str, open: bool) {
    metrics::gauge!("kage_breaker_open", "domain" => domain.to_owned()).set(if open {
        1.0
    } else {
        0.0
    });
}

/// Bytes passed through the forwarder since the last report, through
/// one exit.
pub fn bandwidth(egress: Exit, up: u64, down: u64) {
    metrics::counter!(
        "kage_bandwidth_bytes_total",
        "direction" => "up",
        "egress" => egress.as_str(),
    )
    .increment(up);
    metrics::counter!(
        "kage_bandwidth_bytes_total",
        "direction" => "down",
        "egress" => egress.as_str(),
    )
    .increment(down);
}

/// The share of the bandwidth cap that is used.
pub fn budget(share: f64) {
    metrics::gauge!("kage_bandwidth_budget_ratio").set(share);
}

/// What the response cache did for a request: `hit` (served from it),
/// `miss` (asked, nothing young enough was there, and it is fetched),
/// `stored` (its result was put in), `bypass` (it named a lifetime and its
/// result is none the cache keeps), `joined` (it waits for an identical
/// request under way).
pub fn cache(result: &'static str) {
    metrics::counter!("kage_cache_requests_total", "result" => result).increment(1);
}

/// What came of asking for a `robots.txt`.
pub fn robots(result: &'static str) {
    metrics::counter!("kage_robots_requests_total", "result" => result).increment(1);
}

/// A fetch ended `robots_denied`.
pub fn robots_denied(reason: &'static str) {
    metrics::counter!("kage_robots_denied_total", "reason" => reason).increment(1);
}

/// Reading a page's document (its links, its text, its extraction) took
/// this long.
pub fn extracted(took: Duration) {
    metrics::histogram!("kage_extract_duration_seconds").record(took.as_secs_f64());
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_core::domain::{Exit, Level};
    use kage_core::outcome::Outcome;
    use kage_core::schedule::planner::Refusal;
    use kage_core::schedule::queue::Priority;

    use super::vendor_label;

    #[test]
    fn an_attempt_without_a_vendor_is_labelled_none() {
        assert_eq!(vendor_label(""), "none");
        assert_eq!(vendor_label("cloudflare"), "cloudflare");
    }

    #[test]
    fn recording_without_a_recorder_is_harmless() {
        // No exporter is installed in a test: every call must still be
        // safe to make.
        super::request("example.org", Level::Human, Outcome::Ok, "");
        super::attempt(
            "example.org",
            Level::Plain,
            Outcome::Forbidden,
            "cloudflare",
            Duration::from_millis(840),
        );
        super::waited(Priority::Sync, Duration::from_secs(3));
        super::queue(2, 40);
        super::in_flight(6, 20);
        super::refused(Refusal::DomainPaused);
        super::breaker("example.org", true);
        super::breaker("example.org", false);
        super::bandwidth(Exit::Pool, 1200, 98_000);
        super::bandwidth(Exit::Host, 0, 0);
        super::budget(0.41);
        super::cache("hit");
        super::robots("file");
        super::robots_denied("rule");
        super::extracted(Duration::from_millis(12));
    }
}
