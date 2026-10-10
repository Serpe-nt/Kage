//! Configuration of the scheduler (spec §8, §11), of a browser attempt
//! (spec §8.4, §12), of the bandwidth budget (decision D5) and of the
//! forwarder every persona's traffic leaves through.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::personas::ClockTime;
use crate::duration::HumanDuration;
use crate::pacing::Lognormal;
use crate::schedule::day::DayRules;

/// The longest one attempt may ever take (spec §8.4).
pub const MAX_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(300);

/// Most attempts of either kind the scheduler may be told to run at once.
pub const MAX_INFLIGHT: u32 = 1000;

/// A pause drawn from a shifted log-normal: never shorter than `min`, half
/// of the draws below `median`, nineteen in twenty below `p95`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PauseConfig {
    /// No pause is this short or shorter.
    pub min: HumanDuration,
    /// Half of the pauses are shorter.
    pub median: HumanDuration,
    /// Nineteen pauses in twenty are shorter.
    pub p95: HumanDuration,
}

impl PauseConfig {
    const fn of_millis(min: u64, median: u64, p95: u64) -> Self {
        Self {
            min: HumanDuration::new(Duration::from_millis(min)),
            median: HumanDuration::new(Duration::from_millis(median)),
            p95: HumanDuration::new(Duration::from_millis(p95)),
        }
    }

    /// The pause as the distribution the samplers draw from, in
    /// milliseconds.
    pub fn distribution(&self) -> Lognormal {
        Lognormal {
            median: self.median.get().as_secs_f64() * 1000.0,
            p95: self.p95.get().as_secs_f64() * 1000.0,
            min: Some(self.min.get().as_secs_f64() * 1000.0),
            max: None,
        }
    }

    fn validate(&self, key: &str) -> Result<(), String> {
        self.distribution()
            .validate()
            .map_err(|reason| format!("{key}: {reason}"))
    }
}

/// A stretch of the clock, from `from` to `until`. `until` at or before
/// `from` is on the next day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockRange {
    /// The earliest.
    pub from: ClockTime,
    /// The latest.
    pub until: ClockTime,
}

/// When personas are awake (spec §11): each persona gets up and goes to
/// bed at its own times, drawn once from its seed inside these ranges, and
/// a little earlier or later from day to day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonaDayConfig {
    /// When a persona gets up, in its own time zone.
    #[serde(default = "default_up")]
    pub up: ClockRange,
    /// When it goes to bed.
    #[serde(default = "default_to_bed")]
    pub to_bed: ClockRange,
    /// How far both move from one day to the next, at most.
    #[serde(default = "default_drift")]
    pub drift: HumanDuration,
}

const fn clock(hour: u32, minute: u32) -> ClockTime {
    ClockTime::new_or_midnight(hour, minute)
}

const fn default_up() -> ClockRange {
    ClockRange {
        from: clock(6, 30),
        until: clock(8, 30),
    }
}

const fn default_to_bed() -> ClockRange {
    ClockRange {
        from: clock(22, 30),
        until: clock(0, 30),
    }
}

const fn default_drift() -> HumanDuration {
    HumanDuration::from_mins(20)
}

impl Default for PersonaDayConfig {
    fn default() -> Self {
        Self {
            up: default_up(),
            to_bed: default_to_bed(),
            drift: default_drift(),
        }
    }
}

impl PersonaDayConfig {
    /// The ranges as the day of a persona is drawn from them.
    pub fn rules(&self) -> DayRules {
        DayRules::new(
            (self.up.from.get(), self.up.until.get()),
            (self.to_bed.from.get(), self.to_bed.until.get()),
            self.drift.get(),
        )
    }

    fn validate(&self) -> Result<(), String> {
        if self.up.until < self.up.from {
            return Err("scheduler.active_hours.up: until is before from".to_owned());
        }
        if self.drift.get() > Duration::from_hours(2) {
            return Err("scheduler.active_hours.drift must be at most 2h".to_owned());
        }
        let rules = self.rules();
        if rules.shortest_day() < chrono::Duration::hours(4) {
            return Err(
                "scheduler.active_hours: a persona must be awake for at least four hours; \
                 to_bed has to come after up"
                    .to_owned(),
            );
        }
        if rules.longest_day() > chrono::Duration::hours(23) {
            return Err(
                "scheduler.active_hours: a persona must sleep for at least an hour".to_owned(),
            );
        }
        Ok(())
    }
}

/// What server errors in a row do to a domain (spec §8.3): a site that is
/// simply down is not hammered by retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerErrorsConfig {
    /// Server errors or timeouts in a row, over all personas, that pause
    /// the domain.
    #[serde(default = "default_server_errors_after")]
    pub after: u32,
    /// The first pause. Every further error in the row doubles it.
    #[serde(default = "default_server_errors_pause")]
    pub pause: HumanDuration,
    /// The longest pause.
    #[serde(default = "default_server_errors_max_pause")]
    pub max_pause: HumanDuration,
}

const fn default_server_errors_after() -> u32 {
    3
}

const fn default_server_errors_pause() -> HumanDuration {
    HumanDuration::from_secs(30)
}

const fn default_server_errors_max_pause() -> HumanDuration {
    HumanDuration::from_mins(10)
}

impl Default for ServerErrorsConfig {
    fn default() -> Self {
        Self {
            after: default_server_errors_after(),
            pause: default_server_errors_pause(),
            max_pause: default_server_errors_max_pause(),
        }
    }
}

/// The scheduler (spec §8, §11).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerConfig {
    /// Browser attempts in flight at once, over both engines.
    #[serde(default = "default_max_browser_sessions")]
    pub max_browser_sessions: u32,
    /// Attempts on the http path in flight at once.
    #[serde(default = "default_max_http_inflight")]
    pub max_http_inflight: u32,
    /// Contexts one engine's sidecar holds open at once: its
    /// `SIDECAR_MAX_CONTEXTS`.
    #[serde(default = "default_max_contexts_per_engine")]
    pub max_contexts_per_engine: u32,
    /// How long a persona's browser stays open after its last page. Kage
    /// closes it; the sidecar's own idle limit must be longer.
    #[serde(default = "default_context_idle")]
    pub context_idle: HumanDuration,
    /// Total time a synchronous fetch may take before it is handed over as
    /// a job (spec §8.4).
    #[serde(default = "default_sync_wall_time")]
    pub sync_wall_time: HumanDuration,
    /// How long a synchronous request waits for a persona that is already
    /// visiting the domain before another persona starts a visit.
    #[serde(default = "default_visitor_wait")]
    pub visitor_wait: HumanDuration,
    /// A visit in which no page was asked for this long is over: the
    /// persona's seat on the domain is free.
    #[serde(default = "default_visit_idle")]
    pub visit_idle: HumanDuration,
    /// The pause between the starts of two visits of different personas,
    /// so that requests arriving together do not leave together.
    #[serde(default = "default_visit_start_spacing")]
    pub visit_start_spacing: PauseConfig,
    /// The pause before a retry on another persona.
    #[serde(default = "default_retry_pause")]
    pub retry_pause: PauseConfig,
    /// The shortest cooldown any block gives.
    #[serde(default = "default_cooldown_floor")]
    pub cooldown_floor: HumanDuration,
    /// How far a cooldown is moved per persona, as a share of it: personas
    /// blocked in the same minute do not come back in the same minute.
    #[serde(default = "default_cooldown_jitter")]
    pub cooldown_jitter: f64,
    /// What server errors in a row do to a domain.
    #[serde(default)]
    pub server_errors: ServerErrorsConfig,
    /// The longest pause of a domain's circuit breaker. It doubles from
    /// the policy's `breaker_pause` every time a trial request is blocked.
    #[serde(default = "default_breaker_max_pause")]
    pub breaker_max_pause: HumanDuration,
    /// After this long a trial request that never reported back no longer
    /// holds the breaker.
    #[serde(default = "default_breaker_trial_timeout")]
    pub breaker_trial_timeout: HumanDuration,
    /// When personas are awake.
    #[serde(default)]
    pub active_hours: PersonaDayConfig,
}

const fn default_max_browser_sessions() -> u32 {
    6
}

const fn default_max_http_inflight() -> u32 {
    20
}

const fn default_max_contexts_per_engine() -> u32 {
    3
}

const fn default_context_idle() -> HumanDuration {
    HumanDuration::from_mins(5)
}

const fn default_sync_wall_time() -> HumanDuration {
    HumanDuration::from_secs(180)
}

const fn default_visitor_wait() -> HumanDuration {
    HumanDuration::from_secs(5)
}

const fn default_visit_idle() -> HumanDuration {
    HumanDuration::from_mins(10)
}

const fn default_visit_start_spacing() -> PauseConfig {
    PauseConfig::of_millis(150, 700, 2500)
}

const fn default_retry_pause() -> PauseConfig {
    PauseConfig::of_millis(2000, 6000, 20_000)
}

const fn default_cooldown_floor() -> HumanDuration {
    HumanDuration::from_mins(5)
}

const fn default_cooldown_jitter() -> f64 {
    0.1
}

const fn default_breaker_max_pause() -> HumanDuration {
    HumanDuration::from_hours(6)
}

const fn default_breaker_trial_timeout() -> HumanDuration {
    HumanDuration::from_mins(10)
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_browser_sessions: default_max_browser_sessions(),
            max_http_inflight: default_max_http_inflight(),
            max_contexts_per_engine: default_max_contexts_per_engine(),
            context_idle: default_context_idle(),
            sync_wall_time: default_sync_wall_time(),
            visitor_wait: default_visitor_wait(),
            visit_idle: default_visit_idle(),
            visit_start_spacing: default_visit_start_spacing(),
            retry_pause: default_retry_pause(),
            cooldown_floor: default_cooldown_floor(),
            cooldown_jitter: default_cooldown_jitter(),
            server_errors: ServerErrorsConfig::default(),
            breaker_max_pause: default_breaker_max_pause(),
            breaker_trial_timeout: default_breaker_trial_timeout(),
            active_hours: PersonaDayConfig::default(),
        }
    }
}

/// How deep a page is scrolled, as a share of the document.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepthRange {
    /// The shallowest.
    pub min: f64,
    /// The deepest.
    pub max: f64,
}

/// A browser attempt (spec §8.4, §10.4, §12).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserConfig {
    /// Longest one browser attempt may take when the request does not say:
    /// the load, the behavior and the captures together.
    #[serde(default = "default_browser_timeout")]
    pub timeout: HumanDuration,
    /// How long a detected interstitial is given to resolve by itself.
    #[serde(default = "default_challenge_wait_ms")]
    pub challenge_wait_ms: u64,
    /// How deep a page is scrolled when the request does not say; the
    /// depth of each page is drawn from the request's seed.
    #[serde(default = "default_scroll_depth")]
    pub scroll_depth: DepthRange,
}

const fn default_browser_timeout() -> HumanDuration {
    HumanDuration::from_secs(60)
}

const fn default_challenge_wait_ms() -> u64 {
    15_000
}

const fn default_scroll_depth() -> DepthRange {
    DepthRange { min: 0.6, max: 0.9 }
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            timeout: default_browser_timeout(),
            challenge_wait_ms: default_challenge_wait_ms(),
            scroll_depth: default_scroll_depth(),
        }
    }
}

/// The bandwidth budget of the proxy plan (decision D5).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BandwidthConfig {
    /// What the plan includes per billing term, in gigabytes (10^9 bytes).
    #[serde(default = "default_monthly_cap_gb")]
    pub monthly_cap_gb: u32,
    /// The share of the cap at which `bandwidth_budget_warning` is raised.
    #[serde(default = "default_warn_at")]
    pub warn_at: f64,
    /// Whether everything stops at the cap: no dispatch, open browsers
    /// closed, the forwarder refuses new connections. Off: alerts only.
    #[serde(default = "default_true")]
    pub hard_stop: bool,
    /// How often the forwarder's counters are written to the store.
    #[serde(default = "default_flush_interval")]
    pub flush_interval: HumanDuration,
}

const fn default_monthly_cap_gb() -> u32 {
    250
}

const fn default_warn_at() -> f64 {
    0.8
}

const fn default_true() -> bool {
    true
}

const fn default_flush_interval() -> HumanDuration {
    HumanDuration::from_secs(30)
}

impl Default for BandwidthConfig {
    fn default() -> Self {
        Self {
            monthly_cap_gb: default_monthly_cap_gb(),
            warn_at: default_warn_at(),
            hard_stop: true,
            flush_interval: default_flush_interval(),
        }
    }
}

impl BandwidthConfig {
    /// The cap in bytes.
    pub const fn cap_bytes(&self) -> u64 {
        self.monthly_cap_gb as u64 * 1_000_000_000
    }
}

/// The forwarder (follow-up F10): the one proxy every persona's browser
/// and http client is given. It adds the provider's credentials, relays,
/// and counts what passes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForwarderConfig {
    /// Address the forwarder listens on. The sidecars must reach it;
    /// nothing outside the compose network may.
    #[serde(default = "default_forwarder_bind")]
    pub bind: SocketAddr,
    /// The forwarder as the sidecars reach it, as `http://host:port`.
    #[serde(default = "default_forwarder_advertise")]
    pub advertise: String,
    /// Longest the connection to a persona's proxy, and its answer to a
    /// `CONNECT`, may take.
    #[serde(default = "default_forwarder_connect_timeout")]
    pub connect_timeout: HumanDuration,
    /// A relayed connection on which nothing moved for this long is closed.
    #[serde(default = "default_forwarder_idle_timeout")]
    pub idle_timeout: HumanDuration,
    /// Connections relayed at once.
    #[serde(default = "default_forwarder_max_connections")]
    pub max_connections: u32,
}

const fn default_forwarder_bind() -> SocketAddr {
    SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8899)
}

fn default_forwarder_advertise() -> String {
    "http://kage:8899".to_owned()
}

const fn default_forwarder_connect_timeout() -> HumanDuration {
    HumanDuration::from_secs(20)
}

const fn default_forwarder_idle_timeout() -> HumanDuration {
    HumanDuration::from_mins(10)
}

const fn default_forwarder_max_connections() -> u32 {
    4096
}

impl Default for ForwarderConfig {
    fn default() -> Self {
        Self {
            bind: default_forwarder_bind(),
            advertise: default_forwarder_advertise(),
            connect_timeout: default_forwarder_connect_timeout(),
            idle_timeout: default_forwarder_idle_timeout(),
            max_connections: default_forwarder_max_connections(),
        }
    }
}

fn at_least_one(key: &str, value: u32, most: u32) -> Result<(), String> {
    if value == 0 || value > most {
        return Err(format!("{key} must be between 1 and {most}"));
    }
    Ok(())
}

fn share(key: &str, value: f64) -> Result<(), String> {
    if value.is_finite() && (0.0..=1.0).contains(&value) {
        Ok(())
    } else {
        Err(format!("{key} must be a number between 0 and 1"))
    }
}

/// Checks the four sections.
///
/// # Errors
///
/// The first value that is out of range, as a message naming its key.
pub(super) fn validate(
    scheduler: &SchedulerConfig,
    browser: &BrowserConfig,
    bandwidth: &BandwidthConfig,
    forwarder: &ForwarderConfig,
    sidecar_timeout: Duration,
) -> Result<(), String> {
    at_least_one(
        "scheduler.max_browser_sessions",
        scheduler.max_browser_sessions,
        MAX_INFLIGHT,
    )?;
    at_least_one(
        "scheduler.max_http_inflight",
        scheduler.max_http_inflight,
        MAX_INFLIGHT,
    )?;
    at_least_one(
        "scheduler.max_contexts_per_engine",
        scheduler.max_contexts_per_engine,
        MAX_INFLIGHT,
    )?;
    for (key, value) in [
        ("scheduler.context_idle", scheduler.context_idle),
        ("scheduler.sync_wall_time", scheduler.sync_wall_time),
        ("scheduler.visit_idle", scheduler.visit_idle),
        ("scheduler.cooldown_floor", scheduler.cooldown_floor),
        (
            "scheduler.server_errors.pause",
            scheduler.server_errors.pause,
        ),
        (
            "scheduler.breaker_trial_timeout",
            scheduler.breaker_trial_timeout,
        ),
        ("browser.timeout", browser.timeout),
        ("bandwidth.flush_interval", bandwidth.flush_interval),
        ("forwarder.connect_timeout", forwarder.connect_timeout),
        ("forwarder.idle_timeout", forwarder.idle_timeout),
    ] {
        if value.get() < Duration::from_secs(1) {
            return Err(format!("{key} must be at least 1s"));
        }
    }
    scheduler
        .visit_start_spacing
        .validate("scheduler.visit_start_spacing")?;
    scheduler.retry_pause.validate("scheduler.retry_pause")?;
    if !(scheduler.cooldown_jitter.is_finite() && (0.0..=0.5).contains(&scheduler.cooldown_jitter))
    {
        return Err("scheduler.cooldown_jitter must be a number between 0 and 0.5".to_owned());
    }
    if scheduler.server_errors.after == 0 {
        return Err("scheduler.server_errors.after must be at least 1".to_owned());
    }
    if scheduler.server_errors.max_pause < scheduler.server_errors.pause {
        return Err("scheduler.server_errors.max_pause is shorter than pause".to_owned());
    }
    if scheduler.breaker_max_pause.get() < Duration::from_mins(1) {
        return Err("scheduler.breaker_max_pause must be at least 1m".to_owned());
    }
    scheduler.active_hours.validate()?;

    if browser.timeout.get() > MAX_ATTEMPT_TIMEOUT {
        return Err("browser.timeout must be at most 300s".to_owned());
    }
    // A sidecar call that Kage gives up on before the navigation's own
    // budget ran out leaves a browser loading a page nobody waits for.
    if sidecar_timeout <= browser.timeout.get() {
        return Err("sidecar.timeout must be longer than browser.timeout".to_owned());
    }
    if browser.challenge_wait_ms > 120_000 {
        return Err("browser.challenge_wait_ms must be at most 120000".to_owned());
    }
    share("browser.scroll_depth.min", browser.scroll_depth.min)?;
    share("browser.scroll_depth.max", browser.scroll_depth.max)?;
    if browser.scroll_depth.min > browser.scroll_depth.max {
        return Err("browser.scroll_depth: min is greater than max".to_owned());
    }

    if bandwidth.monthly_cap_gb == 0 {
        return Err("bandwidth.monthly_cap_gb must be at least 1".to_owned());
    }
    share("bandwidth.warn_at", bandwidth.warn_at)?;

    if forwarder.bind.port() == 0 {
        return Err("forwarder.bind needs a fixed port: the sidecars are told it".to_owned());
    }
    let advertised = url::Url::parse(&forwarder.advertise).is_ok_and(|url| {
        url.scheme() == "http"
            && url.host_str().is_some()
            && url.port_or_known_default().is_some_and(|port| port != 0)
            && url.path() == "/"
            && url.username().is_empty()
            && url.password().is_none()
    });
    if !advertised {
        return Err(
            "forwarder.advertise must be http://host:port, the forwarder as the sidecars reach it"
                .to_owned(),
        );
    }
    at_least_one(
        "forwarder.max_connections",
        forwarder.max_connections,
        1_000_000,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        BandwidthConfig, BrowserConfig, ClockRange, ForwarderConfig, PauseConfig, SchedulerConfig,
        clock, validate,
    };
    use crate::duration::HumanDuration;

    const SIDECAR: Duration = Duration::from_secs(330);

    fn check(
        change: impl FnOnce(
            &mut SchedulerConfig,
            &mut BrowserConfig,
            &mut BandwidthConfig,
            &mut ForwarderConfig,
        ),
    ) -> Result<(), String> {
        let mut scheduler = SchedulerConfig::default();
        let mut browser = BrowserConfig::default();
        let mut bandwidth = BandwidthConfig::default();
        let mut forwarder = ForwarderConfig::default();
        change(&mut scheduler, &mut browser, &mut bandwidth, &mut forwarder);
        validate(&scheduler, &browser, &bandwidth, &forwarder, SIDECAR)
    }

    fn refused(
        change: impl FnOnce(
            &mut SchedulerConfig,
            &mut BrowserConfig,
            &mut BandwidthConfig,
            &mut ForwarderConfig,
        ),
    ) -> String {
        check(change).unwrap_err()
    }

    #[test]
    fn the_defaults_are_the_specs_and_valid() {
        assert_eq!(check(|_, _, _, _| {}), Ok(()));

        let scheduler = SchedulerConfig::default();
        assert_eq!(scheduler.max_browser_sessions, 6);
        assert_eq!(scheduler.max_http_inflight, 20);
        assert_eq!(scheduler.max_contexts_per_engine, 3);
        assert_eq!(scheduler.sync_wall_time.get(), Duration::from_secs(180));
        assert_eq!(scheduler.context_idle.get(), Duration::from_mins(5));
        assert_eq!(scheduler.cooldown_floor.get(), Duration::from_mins(5));
        assert_eq!(scheduler.server_errors.after, 3);
        assert_eq!(scheduler.breaker_max_pause.get(), Duration::from_hours(6));
        let browser = BrowserConfig::default();
        assert_eq!(browser.timeout.get(), Duration::from_secs(60));
        assert_eq!(browser.challenge_wait_ms, 15_000);
        let bandwidth = BandwidthConfig::default();
        assert_eq!(bandwidth.cap_bytes(), 250_000_000_000);
        assert!(bandwidth.hard_stop);
        assert_eq!(ForwarderConfig::default().bind.port(), 8899);
    }

    #[test]
    fn a_pause_is_a_shifted_log_normal_in_milliseconds() {
        let pause = SchedulerConfig::default().retry_pause.distribution();

        assert_eq!(pause.min, Some(2000.0));
        assert_eq!((pause.median, pause.p95), (6000.0, 20_000.0));
        assert_eq!(pause.max, None);
    }

    #[test]
    fn counts_and_durations_have_floors() {
        type Change = fn(
            &mut SchedulerConfig,
            &mut BrowserConfig,
            &mut BandwidthConfig,
            &mut ForwarderConfig,
        );
        let cases: [(&str, Change); 16] = [
            ("scheduler.max_browser_sessions", |s, _, _, _| {
                s.max_browser_sessions = 0;
            }),
            ("scheduler.max_http_inflight", |s, _, _, _| {
                s.max_http_inflight = 0;
            }),
            ("scheduler.max_http_inflight", |s, _, _, _| {
                s.max_http_inflight = 1001;
            }),
            ("scheduler.max_contexts_per_engine", |s, _, _, _| {
                s.max_contexts_per_engine = 0;
            }),
            ("scheduler.context_idle", |s, _, _, _| {
                s.context_idle = HumanDuration::default();
            }),
            ("scheduler.sync_wall_time", |s, _, _, _| {
                s.sync_wall_time = HumanDuration::default();
            }),
            ("scheduler.visit_idle", |s, _, _, _| {
                s.visit_idle = HumanDuration::default();
            }),
            ("scheduler.cooldown_floor", |s, _, _, _| {
                s.cooldown_floor = HumanDuration::default();
            }),
            ("scheduler.server_errors.pause", |s, _, _, _| {
                s.server_errors.pause = HumanDuration::default();
            }),
            ("scheduler.breaker_trial_timeout", |s, _, _, _| {
                s.breaker_trial_timeout = HumanDuration::default();
            }),
            ("scheduler.server_errors.after", |s, _, _, _| {
                s.server_errors.after = 0;
            }),
            ("scheduler.server_errors.max_pause", |s, _, _, _| {
                s.server_errors.max_pause = HumanDuration::from_secs(29);
            }),
            ("scheduler.breaker_max_pause", |s, _, _, _| {
                s.breaker_max_pause = HumanDuration::from_secs(59);
            }),
            ("scheduler.cooldown_jitter", |s, _, _, _| {
                s.cooldown_jitter = 0.51;
            }),
            ("scheduler.cooldown_jitter", |s, _, _, _| {
                s.cooldown_jitter = -0.01;
            }),
            ("scheduler.cooldown_jitter", |s, _, _, _| {
                s.cooldown_jitter = f64::NAN;
            }),
        ];
        for (name, change) in cases {
            let error = refused(change);
            assert!(error.starts_with(name), "{name}: {error}");
        }
        // The edges that are allowed.
        assert_eq!(
            check(|s, _, _, _| {
                s.max_http_inflight = 1000;
                s.cooldown_jitter = 0.5;
                s.server_errors.max_pause = s.server_errors.pause;
                s.breaker_max_pause = HumanDuration::from_mins(1);
                s.context_idle = HumanDuration::from_secs(1);
            }),
            Ok(())
        );
        assert_eq!(check(|s, _, _, _| s.cooldown_jitter = 0.0), Ok(()));
    }

    #[test]
    fn a_pause_must_be_a_distribution() {
        let error = refused(|s, _, _, _| {
            s.retry_pause = PauseConfig::of_millis(2000, 2000, 20_000);
        });
        assert!(
            error.starts_with("scheduler.retry_pause: median"),
            "{error}"
        );
        let error = refused(|s, _, _, _| {
            s.visit_start_spacing = PauseConfig::of_millis(100, 700, 600);
        });
        assert!(
            error.starts_with("scheduler.visit_start_spacing: p95"),
            "{error}"
        );
    }

    #[test]
    fn a_persona_must_have_a_day_and_a_night() {
        let error = refused(|s, _, _, _| {
            s.active_hours.up = ClockRange {
                from: clock(9, 0),
                until: clock(8, 0),
            };
        });
        assert_eq!(error, "scheduler.active_hours.up: until is before from");

        // To bed half an hour after the latest getting up.
        let error = refused(|s, _, _, _| {
            s.active_hours.to_bed = ClockRange {
                from: clock(9, 0),
                until: clock(10, 0),
            };
        });
        assert!(error.contains("awake for at least four hours"), "{error}");

        // Never to bed: up at six, to bed at six the next morning.
        let error = refused(|s, _, _, _| {
            s.active_hours.up = ClockRange {
                from: clock(6, 0),
                until: clock(6, 0),
            };
            s.active_hours.to_bed = ClockRange {
                from: clock(5, 30),
                until: clock(5, 59),
            };
        });
        assert!(error.contains("sleep for at least an hour"), "{error}");

        let error = refused(|s, _, _, _| {
            s.active_hours.drift = HumanDuration::from_mins(121);
        });
        assert_eq!(error, "scheduler.active_hours.drift must be at most 2h");
        assert_eq!(
            check(|s, _, _, _| s.active_hours.drift = HumanDuration::from_hours(2)),
            // Two hours of drift on the shipped ranges still leave a night.
            Ok(())
        );
    }

    #[test]
    fn a_browser_attempt_fits_the_cap_and_the_sidecars_timeout() {
        assert_eq!(
            refused(|_, b, _, _| b.timeout = HumanDuration::from_secs(301)),
            "browser.timeout must be at most 300s"
        );
        assert_eq!(
            check(|_, b, _, _| b.timeout = HumanDuration::from_secs(300)),
            Ok(())
        );
        let too_short = validate(
            &SchedulerConfig::default(),
            &BrowserConfig::default(),
            &BandwidthConfig::default(),
            &ForwarderConfig::default(),
            Duration::from_secs(60),
        );
        assert_eq!(
            too_short,
            Err("sidecar.timeout must be longer than browser.timeout".to_owned())
        );
        assert!(
            refused(|_, b, _, _| b.timeout = HumanDuration::default())
                .starts_with("browser.timeout")
        );
        assert_eq!(
            refused(|_, b, _, _| b.challenge_wait_ms = 120_001),
            "browser.challenge_wait_ms must be at most 120000"
        );
        assert_eq!(check(|_, b, _, _| b.challenge_wait_ms = 120_000), Ok(()));
        assert!(
            refused(|_, b, _, _| b.scroll_depth.min = -0.1).starts_with("browser.scroll_depth.min")
        );
        assert!(
            refused(|_, b, _, _| b.scroll_depth.max = 1.1).starts_with("browser.scroll_depth.max")
        );
        assert_eq!(
            refused(|_, b, _, _| {
                b.scroll_depth.min = 0.9;
                b.scroll_depth.max = 0.6;
            }),
            "browser.scroll_depth: min is greater than max"
        );
        assert_eq!(
            check(|_, b, _, _| {
                b.scroll_depth.min = 0.0;
                b.scroll_depth.max = 1.0;
            }),
            Ok(())
        );
    }

    #[test]
    fn the_budget_and_the_forwarder_are_checked() {
        assert_eq!(
            refused(|_, _, b, _| b.monthly_cap_gb = 0),
            "bandwidth.monthly_cap_gb must be at least 1"
        );
        assert!(refused(|_, _, b, _| b.warn_at = 1.01).starts_with("bandwidth.warn_at"));
        assert_eq!(check(|_, _, b, _| b.warn_at = 1.0), Ok(()));
        assert_eq!(check(|_, _, b, _| b.warn_at = 0.0), Ok(()));
        assert!(
            refused(|_, _, b, _| b.flush_interval = HumanDuration::default())
                .starts_with("bandwidth.flush_interval")
        );

        assert!(refused(|_, _, _, f| f.bind.set_port(0)).starts_with("forwarder.bind"));
        for advertise in [
            "kage:8899",
            "https://kage:8899",
            "http://kage:8899/path",
            "http://user:pw@kage:8899",
            "http://kage:0",
            "",
        ] {
            let error = refused(|_, _, _, f| f.advertise = advertise.to_owned());
            assert!(
                error.starts_with("forwarder.advertise"),
                "{advertise}: {error}"
            );
        }
        assert_eq!(
            check(|_, _, _, f| f.advertise = "http://host.docker.internal:8899".to_owned()),
            Ok(())
        );
        assert!(
            refused(|_, _, _, f| f.max_connections = 0).starts_with("forwarder.max_connections")
        );
        assert!(
            refused(|_, _, _, f| f.connect_timeout = HumanDuration::default())
                .starts_with("forwarder.connect_timeout")
        );
        assert!(
            refused(|_, _, _, f| f.idle_timeout = HumanDuration::default())
                .starts_with("forwarder.idle_timeout")
        );
    }
}
