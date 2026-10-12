//! Metrics (spec §20): the exporter behind `/metrics`, and the API's own
//! series.
//!
//! Everything in Kage records through the `metrics` facade: the pool since
//! M2, the scheduler and the forwarder since M7. Nothing was scraped until
//! a recorder was installed, which [`install`] does. `/metrics` renders
//!
//! - what the facade recorded: `kage_requests_total`,
//!   `kage_attempts_total`, `kage_attempt_duration_seconds`,
//!   `kage_proxy_state`, `kage_scheduler_wait_seconds`, `kage_queue_depth`,
//!   `kage_scheduler_in_flight`, `kage_scheduler_refusals_total`,
//!   `kage_breaker_open`, `kage_bandwidth_bytes_total`,
//!   `kage_bandwidth_budget_ratio`, `kage_replacements_total`;
//! - what is read from the store when a scrape arrives:
//!   `kage_persona_state{state}` and `kage_selftest_score{persona,engine}`;
//! - the API's own: `kage_api_requests_total{route,method,status}`,
//!   `kage_api_request_duration_seconds{route,method}`,
//!   `kage_api_requests_in_flight`, `kage_api_refusals_total{code}`;
//! - the sidecars' series (`sidecar_contexts_open`,
//!   `sidecar_navigation_duration_seconds` and the rest), fetched from
//!   each sidecar that answers and merged in, so that Prometheus needs one
//!   target and never holds the sidecar token.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use metrics_exporter_prometheus::{
    BuildError, Matcher, PrometheusBuilder, PrometheusHandle, PrometheusRecorder,
};

/// The media type of the Prometheus text format.
pub const PROMETHEUS_TEXT: &str = "text/plain; version=0.0.4; charset=utf-8";

/// How often a recorder's upkeep runs: histograms are drained and idle
/// series are let go.
pub const UPKEEP_EVERY: Duration = Duration::from_secs(5);

/// The upper bounds of every histogram of seconds: from a quick API call
/// to an attempt at its hard cap of five minutes.
const SECONDS_BUCKETS: [f64; 14] = [
    0.005, 0.025, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 20.0, 45.0, 90.0, 180.0, 300.0,
];

/// Renders what a recorder holds.
#[derive(Clone)]
pub struct MetricsHandle(PrometheusHandle);

impl fmt::Debug for MetricsHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MetricsHandle")
    }
}

impl MetricsHandle {
    /// The recorder's series in the Prometheus text format.
    pub fn render(&self) -> String {
        self.0.render()
    }

    /// Drains histograms and lets idle series go; call it every
    /// [`UPKEEP_EVERY`].
    pub fn upkeep(&self) {
        self.0.run_upkeep();
    }
}

fn builder() -> Result<PrometheusBuilder, BuildError> {
    // With buckets a histogram is a Prometheus histogram and can be
    // aggregated over instances; without, it would be a summary.
    PrometheusBuilder::new()
        .set_buckets_for_metric(Matcher::Suffix("_seconds".to_owned()), &SECONDS_BUCKETS)
}

/// Installs the exporter as the recorder of the `metrics` facade for the
/// whole process. Once per process: a second call fails.
///
/// # Errors
///
/// Fails when a recorder is installed already.
pub fn install() -> Result<MetricsHandle, BuildError> {
    let handle = builder()?.install_recorder()?;
    describe();
    Ok(MetricsHandle(handle))
}

/// A recorder that is not installed, and its handle: for a test, which
/// records into it with `metrics::with_local_recorder`.
///
/// # Errors
///
/// Fails when the exporter cannot be built.
pub fn local() -> Result<(PrometheusRecorder, MetricsHandle), BuildError> {
    let recorder = builder()?.build_recorder();
    let handle = MetricsHandle(recorder.handle());
    Ok((recorder, handle))
}

/// Says what the API's series mean (`# HELP`).
fn describe() {
    metrics::describe_counter!(
        "kage_api_requests_total",
        "Requests the API answered, by route, method and status"
    );
    metrics::describe_histogram!(
        "kage_api_request_duration_seconds",
        metrics::Unit::Seconds,
        "Time from a request's head to its answer's head"
    );
    metrics::describe_gauge!(
        "kage_api_requests_in_flight",
        "Requests the API is working on"
    );
    metrics::describe_counter!(
        "kage_api_refusals_total",
        "Requests refused before a handler ran, by error code"
    );
    metrics::describe_counter!(
        "kage_crawl_pages_total",
        "Pages crawls read, by how they were reached: typed, click, back_click, referer"
    );
    metrics::describe_counter!(
        "kage_crawl_skipped_total",
        "Addresses crawls came to know and did not read, by reason"
    );
    metrics::describe_counter!(
        "kage_crawl_duplicates_total",
        "Pages crawls read that repeat a page read before, by kind: exact, near"
    );
    metrics::describe_gauge!("kage_persona_state", "Personas by state");
    metrics::describe_gauge!(
        "kage_selftest_score",
        "Score of the latest browser self-test of each live persona"
    );
}

/// Counts a request the API answered.
pub(crate) fn request(route: &str, method: &str, status: u16, took: Duration) {
    metrics::counter!(
        "kage_api_requests_total",
        "route" => route.to_owned(),
        "method" => method.to_owned(),
        "status" => status.to_string(),
    )
    .increment(1);
    metrics::histogram!(
        "kage_api_request_duration_seconds",
        "route" => route.to_owned(),
        "method" => method.to_owned(),
    )
    .record(took.as_secs_f64());
}

/// Moves the number of requests in flight.
pub(crate) fn in_flight(change: f64) {
    metrics::gauge!("kage_api_requests_in_flight").increment(change);
}

/// Counts a request that was refused before its handler ran.
pub(crate) fn refusal(code: &'static str) {
    metrics::counter!("kage_api_refusals_total", "code" => code).increment(1);
}

/// One gauge line of the text format. Label values are escaped as the
/// format asks.
fn gauge_line(out: &mut String, name: &str, labels: &[(&str, &str)], value: f64) {
    use fmt::Write as _;

    out.push_str(name);
    if !labels.is_empty() {
        out.push('{');
        for (index, (label, text)) in labels.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            let escaped = text
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n");
            // Writing to a String cannot fail.
            let _ = write!(out, "{label}=\"{escaped}\"");
        }
        out.push('}');
    }
    let _ = writeln!(out, " {value}");
}

/// A gauge family read from the store at scrape time, as text.
pub(crate) fn gauge_family(name: &str, help: &str, samples: &[(Vec<(&str, &str)>, f64)]) -> String {
    let mut out = format!("# HELP {name} {help}\n# TYPE {name} gauge\n");
    for (labels, value) in samples {
        gauge_line(&mut out, name, labels, *value);
    }
    out
}

/// The metric family a line of the text format belongs to: the name after
/// `# HELP` or `# TYPE`, or a sample's name without the suffix a histogram
/// or a summary gives it.
fn family_of<'a>(line: &'a str, known: &BTreeMap<String, Vec<String>>) -> Option<&'a str> {
    let line = line.trim_start();
    if let Some(rest) = line
        .strip_prefix("# HELP ")
        .or_else(|| line.strip_prefix("# TYPE "))
    {
        return rest.split_whitespace().next();
    }
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let name = line
        .split(|character: char| character == '{' || character.is_whitespace())
        .next()?;
    if known.contains_key(name) {
        return Some(name);
    }
    for suffix in ["_bucket", "_sum", "_count", "_total", "_created"] {
        if let Some(base) = name.strip_suffix(suffix)
            && known.contains_key(base)
        {
            return Some(base);
        }
    }
    Some(name)
}

/// Merges expositions in the text format into one: the lines of a family
/// stay together, and a family that several of them have is described
/// once. The format allows one `TYPE` line per name, so the sidecars' two
/// expositions cannot simply be written one after the other.
pub(crate) fn merge(expositions: &[String]) -> String {
    let mut families: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for exposition in expositions {
        for line in exposition.lines() {
            let Some(family) = family_of(line, &families).map(str::to_owned) else {
                continue;
            };
            let lines = families.entry(family.clone()).or_insert_with(|| {
                order.push(family);
                Vec::new()
            });
            let described = line.trim_start().starts_with('#');
            // The first description of a family stands.
            if described
                && lines.iter().any(|known| {
                    known
                        .split_whitespace()
                        .take(2)
                        .eq(line.split_whitespace().take(2))
                })
            {
                continue;
            }
            lines.push(line.to_owned());
        }
    }
    let mut merged = String::new();
    for family in order {
        let Some(lines) = families.get(&family) else {
            continue;
        };
        // Descriptions first, then the samples in the order they came.
        for line in lines
            .iter()
            .filter(|line| line.trim_start().starts_with('#'))
        {
            merged.push_str(line);
            merged.push('\n');
        }
        for line in lines
            .iter()
            .filter(|line| !line.trim_start().starts_with('#'))
        {
            merged.push_str(line);
            merged.push('\n');
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{gauge_family, local, merge, refusal, request};

    #[test]
    fn what_the_facade_records_is_rendered_as_prometheus_text() {
        let (recorder, handle) = local().unwrap();

        metrics::with_local_recorder(&recorder, || {
            request("/v1/fetch", "POST", 200, Duration::from_millis(1800));
            request("/v1/fetch", "POST", 200, Duration::from_millis(40));
            request("/v1/fetch", "POST", 429, Duration::from_millis(1));
            refusal("rate_limited");
            metrics::counter!("kage_requests_total", "domain" => "example.org", "render" => "browser", "outcome" => "ok", "vendor" => "").increment(1);
        });
        let text = handle.render();

        assert!(
            text.contains(
                "kage_api_requests_total{route=\"/v1/fetch\",method=\"POST\",status=\"200\"} 2"
            ),
            "{text}"
        );
        assert!(text.contains("status=\"429\"} 1"), "{text}");
        assert!(text.contains("kage_api_refusals_total{code=\"rate_limited\"} 1"));
        // A duration is a histogram with buckets, not a summary.
        assert!(text.contains("# TYPE kage_api_request_duration_seconds histogram"));
        assert!(
            text.contains(
                "kage_api_request_duration_seconds_bucket{route=\"/v1/fetch\",method=\"POST\",le=\"2.5\"} 3"
            ),
            "{text}"
        );
        assert!(text.contains("kage_requests_total{"));
        // Upkeep keeps what was recorded.
        handle.upkeep();
        assert!(handle.render().contains("kage_api_refusals_total"));
        assert_eq!(format!("{handle:?}"), "MetricsHandle");
    }

    #[test]
    fn a_family_read_from_the_store_is_described_and_escaped() {
        let text = gauge_family(
            "kage_persona_state",
            "Personas by state",
            &[
                (vec![("state", "active")], 18.0),
                (vec![("state", "cooling")], 2.0),
            ],
        );
        assert_eq!(
            text,
            "# HELP kage_persona_state Personas by state\n# TYPE kage_persona_state gauge\n\
             kage_persona_state{state=\"active\"} 18\nkage_persona_state{state=\"cooling\"} 2\n"
        );

        let odd = gauge_family("x", "h", &[(vec![("a", "q\"uo\\te\nd")], 1.5)]);
        assert!(odd.ends_with("x{a=\"q\\\"uo\\\\te\\nd\"} 1.5\n"), "{odd}");
        assert_eq!(
            gauge_family("n", "h", &[(Vec::new(), 0.25)]),
            "# HELP n h\n# TYPE n gauge\nn 0.25\n"
        );
    }

    #[test]
    fn the_sidecars_expositions_merge_into_one_family_each() {
        let chromium = "# HELP sidecar_contexts_open Open contexts\n\
                        # TYPE sidecar_contexts_open gauge\n\
                        sidecar_contexts_open{engine=\"chromium\"} 2\n\
                        # HELP sidecar_navigation_duration_seconds Navigation time\n\
                        # TYPE sidecar_navigation_duration_seconds histogram\n\
                        sidecar_navigation_duration_seconds_bucket{engine=\"chromium\",le=\"1\"} 4\n\
                        sidecar_navigation_duration_seconds_sum{engine=\"chromium\"} 9.5\n\
                        sidecar_navigation_duration_seconds_count{engine=\"chromium\"} 4\n"
            .to_owned();
        let firefox = "# HELP sidecar_contexts_open Open contexts\n\
                       # TYPE sidecar_contexts_open gauge\n\
                       sidecar_contexts_open{engine=\"firefox\"} 1\n\
                       # HELP sidecar_navigation_duration_seconds Navigation time\n\
                       # TYPE sidecar_navigation_duration_seconds histogram\n\
                       sidecar_navigation_duration_seconds_bucket{engine=\"firefox\",le=\"1\"} 1\n\
                       sidecar_navigation_duration_seconds_sum{engine=\"firefox\"} 0.5\n\
                       sidecar_navigation_duration_seconds_count{engine=\"firefox\"} 1\n"
            .to_owned();
        let own = "# TYPE kage_requests_total counter\nkage_requests_total{outcome=\"ok\"} 3\n\n"
            .to_owned();

        let merged = merge(&[own, chromium, firefox]);

        // One TYPE line per family, and a family's lines are together.
        for family in [
            "kage_requests_total",
            "sidecar_contexts_open",
            "sidecar_navigation_duration_seconds",
        ] {
            assert_eq!(
                merged.matches(&format!("# TYPE {family} ")).count(),
                1,
                "{family}\n{merged}"
            );
        }
        assert_eq!(merged.matches("# HELP sidecar_contexts_open ").count(), 1);
        let lines: Vec<&str> = merged.lines().collect();
        let at = |needle: &str| lines.iter().position(|line| line.contains(needle)).unwrap();
        assert_eq!(
            at("sidecar_contexts_open{engine=\"firefox\"}"),
            at("sidecar_contexts_open{engine=\"chromium\"}") + 1
        );
        assert!(
            at("sidecar_navigation_duration_seconds_count{engine=\"firefox\"}")
                > at("# TYPE sidecar_navigation_duration_seconds histogram")
        );
        assert!(
            at("# TYPE sidecar_navigation_duration_seconds histogram")
                > at("sidecar_contexts_open{engine=\"firefox\"}")
        );
        assert_eq!(lines.len(), 14);
        assert_eq!(merge(&[]), "");
    }
}
