//! The scheduler's metrics (spec §20), recorded through the `metrics`
//! facade. Nothing exports them until the exporter is wired (M8, M12).
//!
//! | Metric | Labels | What |
//! |---|---|---|
//! | `kage_requests_total` | `domain`, `render`, `outcome`, `vendor` | fetches that ended |
//! | `kage_attempt_duration_seconds` | `render`, `outcome` | how long an attempt took |
//! | `kage_attempts_total` | `domain`, `render`, `outcome`, `vendor` | attempts that ended |
//! | `kage_scheduler_wait_seconds` | `kind` | how long work waited for a persona and its slot |
//! | `kage_queue_depth` | `kind` | attempts waiting, by `sync` and `job` |
//! | `kage_scheduler_in_flight` | `render` | attempts in flight |
//! | `kage_scheduler_refusals_total` | `reason` | requests answered without an attempt |
//! | `kage_breaker_open` | `domain` | 1 while a domain is paused |
//! | `kage_bandwidth_bytes_total` | `direction` | bytes through the forwarder |
//! | `kage_bandwidth_budget_ratio` | | the share of the bandwidth cap that is used |
//!
//! A domain is a label of unbounded cardinality in principle; in practice
//! it is the set of sites the owner's projects fetch.

use std::time::Duration;

use kage_core::domain::Render;
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
pub fn request(domain: &str, render: Render, outcome: Outcome, vendor: &str) {
    metrics::counter!(
        "kage_requests_total",
        "domain" => domain.to_owned(),
        "render" => render.as_str(),
        "outcome" => outcome.as_str(),
        "vendor" => vendor_label(vendor),
    )
    .increment(1);
}

/// An attempt ended.
pub fn attempt(domain: &str, render: Render, outcome: Outcome, vendor: &str, took: Duration) {
    metrics::counter!(
        "kage_attempts_total",
        "domain" => domain.to_owned(),
        "render" => render.as_str(),
        "outcome" => outcome.as_str(),
        "vendor" => vendor_label(vendor),
    )
    .increment(1);
    metrics::histogram!(
        "kage_attempt_duration_seconds",
        "render" => render.as_str(),
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

/// A domain's breaker opened or closed.
pub fn breaker(domain: &str, open: bool) {
    metrics::gauge!("kage_breaker_open", "domain" => domain.to_owned()).set(if open {
        1.0
    } else {
        0.0
    });
}

/// Bytes passed through the forwarder since the last report.
pub fn bandwidth(up: u64, down: u64) {
    metrics::counter!("kage_bandwidth_bytes_total", "direction" => "up").increment(up);
    metrics::counter!("kage_bandwidth_bytes_total", "direction" => "down").increment(down);
}

/// The share of the bandwidth cap that is used.
pub fn budget(share: f64) {
    metrics::gauge!("kage_bandwidth_budget_ratio").set(share);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_core::domain::Render;
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
        super::request("example.org", Render::Browser, Outcome::Ok, "");
        super::attempt(
            "example.org",
            Render::Http,
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
        super::bandwidth(1200, 98_000);
        super::budget(0.41);
    }
}
