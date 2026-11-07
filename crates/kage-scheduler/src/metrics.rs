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
//! | `kage_policy_breaker_threshold` | `domain` | the blocks per hour that pause a domain under its policy, set at each attempt there |
//! | `kage_bandwidth_bytes_total` | `direction`, `egress` | bytes through the forwarder, by the exit they left through (`pool`, `host`) |
//! | `kage_bandwidth_budget_ratio` | | the share of the bandwidth cap that is used |
//! | `kage_cache_requests_total` | `result` | what the response cache did for a request: `hit`, `miss`, `stored`, `bypass`, `joined` |
//! | `kage_robots_requests_total` | `result` | what came of asking for a `robots.txt`: `file`, `missing`, `unreachable`, `not_asked`, `no_scout` |
//! | `kage_robots_denied_total` | `reason` | fetches that ended `robots_denied`: `rule`, `robots_unavailable`, `robots_no_scout`, `robots_enforced`, `robots_ignored` |
//! | `kage_extract_duration_seconds` | | how long reading a page's document took |
//!
//! A domain is a label of unbounded cardinality in principle; in practice
//! it is the set of sites the owner's projects fetch. Its values are
//! bounded all the same (the security review's L3): the first
//! `server.metrics_max_domains` domains (500) since the start get a label of
//! their own, and every later one is counted under [`OTHER_DOMAIN`], so that
//! a key that asks for many hosts cannot grow the series without end
//! ([`DomainLabels`]). A level is its number, `0` to `3` (spec §25).
//!
//! `kage_breaker_open{domain}` is not recorded here: the API reads it from
//! the stored breakers at every scrape (F126), so that it survives a restart
//! and comes down when a pause ends.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;

use kage_core::domain::{Exit, Level, Render};
use kage_core::outcome::Outcome;
use kage_core::schedule::planner::Refusal;
use kage_core::schedule::queue::Priority;

/// The `domain` label every domain past the bound shares (L3). A domain has
/// a dot, so no domain is labelled so by itself.
pub const OTHER_DOMAIN: &str = "other";

/// How many domains get a `domain` label of their own when nothing else is
/// set (`server.metrics_max_domains`).
pub const DEFAULT_MAX_DOMAINS: usize = 500;

/// The `domain` label values in use: the first `max` distinct domains get a
/// label of their own, for the life of the process, and every later one
/// [`OTHER_DOMAIN`] (the security review's L3). A domain keeps the label it
/// got first, so a series never changes its name; lowering the bound keeps
/// the labels already given.
#[derive(Debug)]
pub struct DomainLabels {
    max: usize,
    labelled: HashSet<String>,
}

impl DomainLabels {
    /// Labels for at most `max` domains.
    pub fn new(max: usize) -> Self {
        Self {
            max,
            labelled: HashSet::new(),
        }
    }

    /// Sets the bound for the domains seen from now on.
    pub const fn set_max(&mut self, max: usize) {
        self.max = max;
    }

    /// The label of `domain`: its own, while there is room or when it has
    /// one already, else [`OTHER_DOMAIN`].
    pub fn label<'a>(&mut self, domain: &'a str) -> Cow<'a, str> {
        if self.labelled.contains(domain) {
            return Cow::Borrowed(domain);
        }
        if self.labelled.len() < self.max {
            self.labelled.insert(domain.to_owned());
            return Cow::Borrowed(domain);
        }
        Cow::Borrowed(OTHER_DOMAIN)
    }

    /// Whether `domain` has a label of its own.
    pub fn has_own(&self, domain: &str) -> bool {
        self.labelled.contains(domain)
    }

    /// How many domains have a label of their own.
    pub fn len(&self) -> usize {
        self.labelled.len()
    }

    /// Whether no domain has a label yet.
    pub fn is_empty(&self) -> bool {
        self.labelled.is_empty()
    }
}

/// The process's labels, behind every metric with a `domain` label.
static DOMAINS: LazyLock<Mutex<DomainLabels>> =
    LazyLock::new(|| Mutex::new(DomainLabels::new(DEFAULT_MAX_DOMAINS)));

/// Bounds the `domain` label values (`server.metrics_max_domains`), once at
/// the start of the server.
pub fn bound_domains(max: usize) {
    DOMAINS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .set_max(max);
}

/// The `domain` label a metric carries for `domain`: its own, or
/// [`OTHER_DOMAIN`] past the bound ([`DomainLabels`]).
pub fn domain_label(domain: &str) -> String {
    DOMAINS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .label(domain)
        .into_owned()
}

/// The label of an attempt or a request without a vendor.
const NO_VENDOR: &str = "none";

/// The values of `kage_cache_requests_total{result}` (see [`cache`]).
pub const CACHE_RESULTS: [&str; 5] = ["hit", "miss", "stored", "bypass", "joined"];

/// The values of `kage_robots_requests_total{result}` (see [`robots`]).
pub const ROBOTS_RESULTS: [&str; 5] = ["file", "missing", "unreachable", "not_asked", "no_scout"];

/// The values of `kage_robots_denied_total{reason}` (see [`robots_denied`]).
pub const ROBOTS_DENIED_REASONS: [&str; 5] = [
    "rule",
    "robots_unavailable",
    "robots_no_scout",
    "robots_enforced",
    "robots_ignored",
];

/// Registers at zero every counter of this module whose label values are
/// known before any event: the refusals by reason, the bytes by direction
/// and exit, the cache, the `robots.txt` asked for and the robots denials
/// by result. A Prometheus series is born with its first sample, and
/// `increase()` never counts that one; a counter that exists from zero
/// shows its first event as a change (F110). `kage_requests_total` and
/// `kage_attempts_total` carry a domain, which arrives with the event, so
/// they cannot be registered here: the alert and the panels that count
/// them read the counter against its value an hour earlier instead.
///
/// Once per process, after the exporter is installed; without a recorder
/// it does nothing.
pub fn register() {
    for reason in Refusal::ALL.iter().copied() {
        metrics::counter!("kage_scheduler_refusals_total", "reason" => reason.as_str())
            .increment(0);
    }
    for egress in Exit::ALL.iter().copied() {
        for direction in ["up", "down"] {
            metrics::counter!(
                "kage_bandwidth_bytes_total",
                "direction" => direction,
                "egress" => egress.as_str(),
            )
            .increment(0);
        }
    }
    for result in CACHE_RESULTS {
        metrics::counter!("kage_cache_requests_total", "result" => result).increment(0);
    }
    for result in ROBOTS_RESULTS {
        metrics::counter!("kage_robots_requests_total", "result" => result).increment(0);
    }
    for reason in ROBOTS_DENIED_REASONS {
        metrics::counter!("kage_robots_denied_total", "reason" => reason).increment(0);
    }
}

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
        "domain" => domain_label(domain),
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
        "domain" => domain_label(domain),
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
/// for the alert that compares a domain's blocks with it (spec §20). Only
/// for a domain with a label of its own: the threshold of one domain is
/// none of the others' that share [`OTHER_DOMAIN`], so no series is made
/// for that label (L3).
pub fn breaker_threshold(domain: &str, threshold: u32) {
    let label = domain_label(domain);
    if label == OTHER_DOMAIN {
        return;
    }
    metrics::gauge!("kage_policy_breaker_threshold", "domain" => label).set(f64::from(threshold));
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

    /// L3: the first domains get a label of their own and keep it; every
    /// later one shares `other`; a bound changed later keeps what was given.
    #[test]
    fn the_domain_label_values_are_bounded() {
        let mut labels = super::DomainLabels::new(2);
        assert!(labels.is_empty());
        assert_eq!(labels.label("a.example"), "a.example");
        assert_eq!(labels.label("b.example"), "b.example");
        assert_eq!(labels.label("c.example"), super::OTHER_DOMAIN);
        assert_eq!(labels.label("a.example"), "a.example");
        assert_eq!(labels.label("d.example"), "other");
        assert_eq!(labels.len(), 2);
        assert!(labels.has_own("b.example"));
        assert!(!labels.has_own("c.example"));
        labels.set_max(1);
        assert_eq!(labels.label("b.example"), "b.example");
        labels.set_max(3);
        assert_eq!(labels.label("c.example"), "c.example");
        assert_eq!(labels.label("e.example"), "other");
        // A bound of 0 gives no domain a label of its own.
        let mut none = super::DomainLabels::new(0);
        assert_eq!(none.label("a.example"), "other");
        // Ten thousand hosts make three series at most here.
        let mut many = super::DomainLabels::new(3);
        let distinct: std::collections::HashSet<String> = (0..10_000)
            .map(|index| many.label(&format!("host-{index}.example")).into_owned())
            .collect();
        assert_eq!(distinct.len(), 4);
        // The process's labels.
        super::bound_domains(super::DEFAULT_MAX_DOMAINS);
        assert_eq!(super::domain_label("shop.example"), "shop.example");
    }

    #[test]
    fn an_attempt_without_a_vendor_is_labelled_none() {
        assert_eq!(vendor_label(""), "none");
        assert_eq!(vendor_label("cloudflare"), "cloudflare");
    }

    #[test]
    fn recording_without_a_recorder_is_harmless() {
        // No exporter is installed in a test: every call must still be
        // safe to make.
        super::register();
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
        super::bandwidth(Exit::Pool, 1200, 98_000);
        super::bandwidth(Exit::Host, 0, 0);
        super::budget(0.41);
        super::cache("hit");
        super::robots("file");
        super::robots_denied("rule");
        super::extracted(Duration::from_millis(12));
    }
}
