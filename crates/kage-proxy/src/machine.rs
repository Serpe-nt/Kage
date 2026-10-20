//! The pool state machine (spec §7.3) as pure functions over a [`Proxy`].
//!
//! ```text
//! new ──ok──▶ healthy ──failure──▶ degraded ──N failures──▶ backoff ──expiry──▶ degraded
//!               ▲                      │                        │
//!               └───────── ok ─────────┘                        │
//! backoff ──M cycles and replace_after, or bad reputation──▶ replacing ──done──▶ retired
//! ```
//!
//! What the diagram leaves out, and this module adds:
//!
//! - A proxy that lets the host's own address through goes straight to
//!   `backoff` from `new`, `healthy` or `degraded`. Leak cycles never count
//!   toward a replacement: a leak is a fault on the path, not in the address.
//! - A `new` proxy that fails N checks goes to `backoff` without ever having
//!   been `healthy`.
//! - A proxy with a bad reputation, or one an operator picked, can be
//!   replaced from `healthy` or `degraded` (and an operator may also pick a
//!   `new` one).
//! - A replacement the provider fails sends the proxy back to `backoff` and
//!   frees its place in the monthly budget.
//! - A proxy that disappears from the provider's list is `retired` from any
//!   state.
//!
//! [`ALLOWED_TRANSITIONS`] is the complete list; property tests check that
//! no sequence of events ever leaves it, that a backoff never exceeds its
//! cap, and that the monthly budget is never exceeded.
//!
//! Nothing here reads a clock, draws a random number or touches the store:
//! the caller passes the time, the jitter and the number of replacements
//! already requested this month. The pool calls these functions inside a
//! database transaction that holds the row, so what they decide is what is
//! stored.

use std::net::IpAddr;

use chrono::{DateTime, Datelike, TimeZone, Utc};
use kage_core::config::Config;
use kage_core::domain::{
    CountryCode, GeoRecord, HealthFailure, Proxy, ProxyHealth, ProxyReplacement, ProxyState,
    ReplaceReason,
};
use kage_core::ids::ProxyId;

use crate::provider::AutoReplace;

/// Every state change the machine can make. Staying in a state is always
/// allowed and not listed.
pub const ALLOWED_TRANSITIONS: &[(ProxyState, ProxyState)] = &[
    // The diagram of spec §7.3.
    (ProxyState::New, ProxyState::Healthy),
    (ProxyState::Healthy, ProxyState::Degraded),
    (ProxyState::Degraded, ProxyState::Healthy),
    (ProxyState::Degraded, ProxyState::Backoff),
    (ProxyState::Backoff, ProxyState::Degraded),
    (ProxyState::Backoff, ProxyState::Replacing),
    (ProxyState::Replacing, ProxyState::Retired),
    // A leak, or N failures before the first success.
    (ProxyState::New, ProxyState::Backoff),
    (ProxyState::Healthy, ProxyState::Backoff),
    // Bad reputation or an operator's request.
    (ProxyState::New, ProxyState::Replacing),
    (ProxyState::Healthy, ProxyState::Replacing),
    (ProxyState::Degraded, ProxyState::Replacing),
    // The provider failed the replacement.
    (ProxyState::Replacing, ProxyState::Backoff),
    // Gone from the provider's list.
    (ProxyState::New, ProxyState::Retired),
    (ProxyState::Healthy, ProxyState::Retired),
    (ProxyState::Degraded, ProxyState::Retired),
    (ProxyState::Backoff, ProxyState::Retired),
];

/// Whether the machine may move a proxy from `from` to `to`.
pub fn is_allowed(from: ProxyState, to: ProxyState) -> bool {
    from == to || ALLOWED_TRANSITIONS.contains(&(from, to))
}

/// The numbers the state machine works with (spec §7.3, §7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolRules {
    /// Failed checks in a row that send a proxy into backoff (N).
    pub failure_threshold: u32,
    /// First backoff; it doubles with every cycle in a row.
    pub backoff_base: chrono::Duration,
    /// Longest backoff.
    pub backoff_cap: chrono::Duration,
    /// Backoff cycles in a row before a replacement is considered (M).
    pub backoff_cycles: u32,
    /// How long a proxy must have been failing without a break before it is
    /// replaced.
    pub replace_after: chrono::Duration,
    /// Replacements Kage may request per budget window: the provider's
    /// subscription term when the account reports one, else the UTC calendar
    /// month ([`budget_window_start`]).
    pub replacements_per_month: u32,
    /// The provider's automatic replacement settings the operator accepts
    /// (`webshare.accepted_auto_replace`): one of these being on is noted,
    /// not alerted, and a proxy the provider replaced for that reason is
    /// retired without the alert.
    pub accepted_auto_replace: AutoReplace,
    /// Geo lookups in a row that must disagree with the provider's country
    /// before the proxy is degraded.
    pub geo_mismatches: u32,
    /// Abuse score at or above which an address is bad.
    pub bad_score: u8,
    /// Reputation lookups in a row that must say bad.
    pub bad_lookups: u32,
}

impl PoolRules {
    /// The rules in the configuration.
    pub fn from_config(config: &Config) -> Self {
        let duration = |value: std::time::Duration| {
            chrono::Duration::from_std(value).unwrap_or(chrono::Duration::MAX)
        };
        Self {
            failure_threshold: config.proxies.failure_threshold,
            backoff_base: duration(config.proxies.backoff_base.get()),
            backoff_cap: duration(config.proxies.backoff_cap.get()),
            backoff_cycles: config.proxies.backoff_cycles,
            replace_after: duration(config.proxies.replace_after.get()),
            replacements_per_month: config.proxies.replacements_per_month,
            // A configuration that loaded names known settings only.
            accepted_auto_replace: AutoReplace::from_names(&config.webshare.accepted_auto_replace)
                .unwrap_or_default(),
            geo_mismatches: config.proxies.health.geo_mismatches,
            bad_score: config.reputation.bad_score,
            bad_lookups: config.reputation.bad_lookups,
        }
    }
}

impl Default for PoolRules {
    /// The defaults of the specification and of consult 2 of milestone M2.
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            backoff_base: chrono::Duration::minutes(1),
            backoff_cap: chrono::Duration::hours(1),
            backoff_cycles: 3,
            replace_after: chrono::Duration::hours(6),
            replacements_per_month: 5,
            // Decision D31: Webshare may replace a proxy its own checks
            // found dead.
            accepted_auto_replace: AutoReplace {
                invalid: true,
                low_country_confidence: false,
                out_of_rotation: false,
                failed_site_check: false,
            },
            geo_mismatches: 2,
            bad_score: 75,
            bad_lookups: 2,
        }
    }
}

/// What a probe saw at the far end of a proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitObservation {
    /// The address the echo service saw.
    pub ip: IpAddr,
    /// Round-trip time of the probe, including the new tunnel.
    pub latency_ms: u32,
    /// Of that, opening the tunnel through the proxy: TCP to the proxy and
    /// the `CONNECT`. `None` when the probe did not measure it.
    pub connect_ms: Option<u32>,
    /// Of that, the TLS handshake with the echo. `None` for a plain echo
    /// or when the probe did not measure it.
    pub tls_ms: Option<u32>,
}

/// The outcome of one health probe of one proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    /// When the probe ran.
    pub at: DateTime<Utc>,
    /// What the echo saw, or why none could be read.
    pub exit: Result<ExitObservation, HealthFailure>,
    /// Whether the exit address is the host's own. `None` when the host
    /// address is unknown, or nothing was read.
    pub leak: Option<bool>,
    /// The geo record, when this probe included a lookup that answered.
    pub geo: Option<GeoRecord>,
}

/// One reputation lookup of a proxy's exit address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReputationReport {
    /// When the lookup ran.
    pub at: DateTime<Utc>,
    /// Abuse score, 0 (clean) to 100.
    pub score: u8,
    /// How the reputation provider classifies the address.
    pub usage_type: Option<String>,
}

/// Something the machine did or noticed, for the pool to store and to alert
/// on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// The proxy changed state.
    StateChanged {
        /// The state before.
        from: ProxyState,
        /// The state after.
        to: ProxyState,
    },
    /// The proxy let the host's own address through.
    Leak {
        /// The address the echo saw: the host's.
        exit_ip: IpAddr,
    },
    /// A probe through a proxy that had leaked saw a distinct exit address
    /// again.
    LeakCleared,
    /// The first geo lookup of the proxy answered.
    GeoObserved {
        /// What it said.
        current: GeoRecord,
    },
    /// A geo lookup placed the proxy somewhere else than the one before.
    /// Personas derive their timezone and locale from this record.
    GeoChanged {
        /// The record before.
        previous: GeoRecord,
        /// The record now.
        current: GeoRecord,
    },
    /// Enough lookups in a row placed the proxy in another country than the
    /// provider states.
    GeoMismatch {
        /// The provider's country.
        provider: CountryCode,
        /// The observed country.
        observed: CountryCode,
    },
    /// A lookup agreed with the provider again.
    GeoMismatchCleared,
    /// The probe saw the exit address of another live proxy, older than
    /// this one: this one carries nothing while it lasts (decision D6).
    ExitShared {
        /// The older proxy that leaves by the same address.
        with: ProxyId,
    },
    /// The proxy leaves by an address of its own again.
    ExitSharedCleared,
    /// Enough lookups in a row scored the address as bad.
    ReputationBad {
        /// The latest score.
        score: u8,
    },
    /// A lookup scored the address below the bad score again.
    ReputationRecovered,
    /// The reputation provider classifies the address as data center or
    /// hosting space.
    UsageTypeSuspicious {
        /// The classification.
        usage_type: String,
    },
}

/// What became of a request to replace a proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacementDecision {
    /// The proxy is now `replacing` and holds a place in this month's
    /// budget.
    Started,
    /// This month's budget is used up; nothing changed.
    BudgetExhausted {
        /// Replacements requested this month.
        used: u32,
        /// The budget.
        budget: u32,
    },
    /// The provider's plan has no replacements left; nothing changed.
    QuotaExhausted,
    /// The proxy is already being replaced or is retired; nothing changed.
    NotEligible {
        /// The state it is in.
        state: ProxyState,
    },
}

/// The start of the window the replacement budget is counted over at `now`.
///
/// The provider counts replacements per subscription term, not per calendar
/// month (Webshare: thirty days from the day the plan was bought). When the
/// account reports a term, Kage's budget follows it: the window starts at
/// the term's start. An account read that is older than its own term (the
/// plan renewed since) is rolled forward by whole terms. Without a usable
/// term the window is the UTC calendar month ([`month_start`]).
pub fn budget_window_start(
    now: DateTime<Utc>,
    term: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> DateTime<Utc> {
    let Some((start, end)) = term.filter(|(start, end)| start < end && *start <= now) else {
        return month_start(now);
    };
    if now < end {
        return start;
    }
    let length = end.signed_duration_since(start).num_seconds().max(1);
    let terms = now.signed_duration_since(start).num_seconds() / length;
    start + chrono::Duration::seconds(terms.saturating_mul(length))
}

/// The start of the UTC calendar month `now` falls in: the window the
/// replacement budget is counted over when the provider reports no term.
pub fn month_start(now: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        // The first of a month at midnight exists in UTC.
        .unwrap_or(now)
}

/// The backoff of cycle `cycle` (0 for the first): the base doubled `cycle`
/// times, capped, then jittered into the upper half of that, so it is never
/// longer than the cap and never shorter than half of it. `jitter` is a
/// number in `0..1`.
pub fn backoff_delay(cycle: u32, rules: &PoolRules, jitter: f64) -> chrono::Duration {
    let base_ms = rules.backoff_base.num_milliseconds().max(0);
    let cap_ms = rules.backoff_cap.num_milliseconds().max(0);
    let doubled = 2_i64
        .checked_pow(cycle)
        .and_then(|factor| base_ms.checked_mul(factor))
        .unwrap_or(i64::MAX);
    let ceiling = doubled.min(cap_ms);
    let jitter = if jitter.is_finite() {
        jitter.clamp(0.0, 1.0)
    } else {
        1.0
    };
    let half = ceiling / 2;
    // `half` is at most half an i64 of milliseconds and the factor at most
    // one, so the product fits and the sum stays within `ceiling`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let jittered = half + (half as f64 * jitter) as i64;
    chrono::Duration::milliseconds(jittered.clamp(0, ceiling))
}

/// Applies one probe to a proxy.
///
/// `outage` says that more than half of the pool is failing right now: the
/// failure is recorded, but no counter moves and no state changes, because
/// the cause is on the path and not in this address.
///
/// A proxy that is `replacing` or `retired` only has the probe recorded; a
/// proxy in `backoff` must be expired first (see [`expire_backoff`]).
pub fn apply_probe(
    proxy: &mut Proxy,
    report: &ProbeReport,
    outage: bool,
    jitter: f64,
    rules: &PoolRules,
) -> Vec<Effect> {
    let mut effects = Vec::new();
    let from = proxy.state;
    let previous = proxy.last_health.take();
    let was_leaking = previous
        .as_ref()
        .is_some_and(|health| health.leak_detected == Some(true));
    let last_failure_was_leak = previous
        .as_ref()
        .is_some_and(|health| health.failure == Some(HealthFailure::Leak));
    let mut health = previous.unwrap_or_else(|| ProxyHealth::new(report.at, false));
    health.checked_at = report.at;

    match &report.exit {
        Ok(exit) => {
            health.reachable = true;
            health.latency_ms = Some(exit.latency_ms);
            health.connect_ms = exit.connect_ms;
            health.tls_ms = exit.tls_ms;
            health.exit_ip = Some(exit.ip);
            // A leak is cleared only by a probe that shows a distinct exit
            // address; one that could not compare leaves the flag standing.
            health.leak_detected = match report.leak {
                Some(leak) => Some(leak),
                None if was_leaking => Some(true),
                None => None,
            };
            health.failure = (health.leak_detected == Some(true)).then_some(HealthFailure::Leak);
        }
        Err(failure) => {
            health.reachable = false;
            health.latency_ms = None;
            health.connect_ms = None;
            health.tls_ms = None;
            health.failure = Some(*failure);
        }
    }
    let leaking = health.reachable && health.leak_detected == Some(true);
    if leaking
        && !was_leaking
        && let Some(exit_ip) = health.exit_ip
    {
        effects.push(Effect::Leak { exit_ip });
    } else if was_leaking && health.reachable && !leaking {
        effects.push(Effect::LeakCleared);
    }

    if health.reachable
        && let Some(geo) = &report.geo
    {
        record_geo(proxy, &mut health, geo, rules, &mut effects);
    }

    if matches!(
        from,
        ProxyState::New | ProxyState::Healthy | ProxyState::Degraded
    ) {
        // Leak cycles do not count toward a replacement: whatever follows a
        // leak starts from a clean slate.
        if last_failure_was_leak && !leaking {
            proxy.backoff_count = 0;
        }

        if leaking {
            proxy.consecutive_failures = 0;
            proxy.failing_since = None;
            enter_backoff(proxy, report.at, jitter, rules);
        } else if health.reachable {
            proxy.consecutive_failures = 0;
            proxy.failing_since = None;
            proxy.backoff_count = 0;
            proxy.backoff_until = None;
            // A new proxy is healthy first; a standing condition degrades
            // it on the next check.
            proxy.state = if from != ProxyState::New && is_degrading(&health, rules) {
                ProxyState::Degraded
            } else {
                ProxyState::Healthy
            };
        } else if !outage {
            proxy.consecutive_failures = proxy.consecutive_failures.saturating_add(1);
            proxy.failing_since.get_or_insert(report.at);
            // After a backoff the proxy is on probation: one failure sends
            // it back, with the next longer wait.
            let on_probation = proxy.backoff_count > 0;
            if on_probation || proxy.consecutive_failures >= rules.failure_threshold {
                enter_backoff(proxy, report.at, jitter, rules);
            } else if from == ProxyState::Healthy {
                proxy.state = ProxyState::Degraded;
            }
        }
    }

    proxy.last_health = Some(health);
    push_state_change(&mut effects, from, proxy.state);
    effects
}

/// Folds a geo lookup into the health record.
fn record_geo(
    proxy: &mut Proxy,
    health: &mut ProxyHealth,
    geo: &GeoRecord,
    rules: &PoolRules,
    effects: &mut Vec<Effect>,
) {
    let was_confirmed = health.geo_mismatches >= rules.geo_mismatches;
    let matches = proxy.country.map(|country| country == geo.country);
    health.exit_country = Some(geo.country);
    health.country_match = matches;
    health.geo_mismatches = if matches == Some(false) {
        health.geo_mismatches.saturating_add(1)
    } else {
        0
    };
    let confirmed = health.geo_mismatches >= rules.geo_mismatches;

    match health.geo.replace(geo.clone()) {
        None => effects.push(Effect::GeoObserved {
            current: geo.clone(),
        }),
        Some(previous) if !previous.same_location(geo) => effects.push(Effect::GeoChanged {
            previous,
            current: geo.clone(),
        }),
        Some(_) => {}
    }
    if let Some(asn) = geo.asn.as_ref().or(geo.org.as_ref()) {
        proxy.asn = Some(asn.clone());
    }
    if confirmed
        && !was_confirmed
        && let Some(provider) = proxy.country
    {
        effects.push(Effect::GeoMismatch {
            provider,
            observed: geo.country,
        });
    } else if was_confirmed && !confirmed {
        effects.push(Effect::GeoMismatchCleared);
    }
}

/// Whether a standing condition keeps a reachable proxy `degraded`: a
/// confirmed geo mismatch, a confirmed bad reputation, or an exit address
/// another live proxy leaves by too.
fn is_degrading(health: &ProxyHealth, rules: &PoolRules) -> bool {
    health.geo_mismatches >= rules.geo_mismatches
        || health.reputation_bad_lookups >= rules.bad_lookups
        || health.exit_shared_with.is_some()
}

/// Notes whether a probe saw the exit address of another live proxy that
/// is older than this one (`shared`, which the pool finds among its live
/// proxies): two proxies that leave by one address would be two personas
/// on one address (decision D6), so the later one is marked and carries
/// nothing while it lasts. Applied before [`apply_probe`], which then keeps
/// the proxy `degraded`. A probe that read no exit address, or saw a leak,
/// tells nothing about it and changes nothing.
pub fn mark_shared_exit(
    proxy: &mut Proxy,
    report: &ProbeReport,
    shared: Option<ProxyId>,
) -> Vec<Effect> {
    if report.exit.is_err() || report.leak == Some(true) {
        return Vec::new();
    }
    let health = proxy
        .last_health
        .get_or_insert_with(|| ProxyHealth::new(report.at, false));
    let before = std::mem::replace(&mut health.exit_shared_with, shared);
    match (before, shared) {
        (None, Some(with)) => vec![Effect::ExitShared { with }],
        (Some(_), None) => vec![Effect::ExitSharedCleared],
        _ => Vec::new(),
    }
}

fn enter_backoff(proxy: &mut Proxy, now: DateTime<Utc>, jitter: f64, rules: &PoolRules) {
    let delay = backoff_delay(proxy.backoff_count, rules, jitter);
    proxy.state = ProxyState::Backoff;
    proxy.backoff_until = Some(now + delay);
    proxy.backoff_count = proxy.backoff_count.saturating_add(1);
}

fn push_state_change(effects: &mut Vec<Effect>, from: ProxyState, to: ProxyState) {
    if from != to {
        debug_assert!(is_allowed(from, to), "{from} -> {to} is not in the diagram");
        effects.push(Effect::StateChanged { from, to });
    }
}

/// Ends a backoff that has run out: the proxy becomes `degraded` and is
/// probed once. With `force`, the backoff ends even if it has time left
/// (an operator asked for a check now).
pub fn expire_backoff(proxy: &mut Proxy, now: DateTime<Utc>, force: bool) -> Vec<Effect> {
    let mut effects = Vec::new();
    if proxy.state == ProxyState::Backoff
        && (force || proxy.backoff_until.is_none_or(|until| until <= now))
    {
        proxy.state = ProxyState::Degraded;
        proxy.backoff_until = None;
        push_state_change(&mut effects, ProxyState::Backoff, ProxyState::Degraded);
    }
    effects
}

/// Applies one reputation lookup to a live proxy.
pub fn apply_reputation(
    proxy: &mut Proxy,
    report: &ReputationReport,
    rules: &PoolRules,
) -> Vec<Effect> {
    let mut effects = Vec::new();
    if !proxy.state.is_live() {
        return effects;
    }
    let from = proxy.state;
    let health = proxy
        .last_health
        .get_or_insert_with(|| ProxyHealth::new(report.at, false));
    let was_bad = health.reputation_bad_lookups >= rules.bad_lookups;
    health.reputation_score = Some(report.score);
    health.reputation_checked_at = Some(report.at);
    health.reputation_bad_lookups = if report.score >= rules.bad_score {
        health.reputation_bad_lookups.saturating_add(1)
    } else {
        0
    };
    let is_bad = health.reputation_bad_lookups >= rules.bad_lookups;

    if health.usage_type != report.usage_type
        && let Some(usage_type) = &report.usage_type
        && is_hosting(usage_type)
    {
        effects.push(Effect::UsageTypeSuspicious {
            usage_type: usage_type.clone(),
        });
    }
    health.usage_type.clone_from(&report.usage_type);

    if is_bad && !was_bad {
        effects.push(Effect::ReputationBad {
            score: report.score,
        });
    } else if was_bad && !is_bad {
        effects.push(Effect::ReputationRecovered);
    }
    // A bad address is degraded at once; whether it is replaced depends on
    // the budget. Recovery to healthy is left to the next probe.
    if is_bad && from == ProxyState::Healthy {
        proxy.state = ProxyState::Degraded;
    }
    push_state_change(&mut effects, from, proxy.state);
    effects
}

/// Whether a usage type names data center or hosting space, which a
/// residential proxy must not be.
pub fn is_hosting(usage_type: &str) -> bool {
    let usage_type = usage_type.to_ascii_lowercase();
    ["data center", "datacenter", "hosting", "content delivery"]
        .iter()
        .any(|marker| usage_type.contains(marker))
}

/// Whether the proxy qualifies for a replacement right now, and why.
///
/// Unreachable: in `backoff` after a probe that read nothing, with at least
/// M cycles in a row, failing without a break for at least `replace_after`.
/// A leak never qualifies. Reputation: usable or in backoff, with enough
/// bad lookups in a row.
pub fn replacement_due(
    proxy: &Proxy,
    now: DateTime<Utc>,
    rules: &PoolRules,
) -> Option<ReplaceReason> {
    let health = proxy.last_health.as_ref();
    let last_probe_read_nothing = health
        .is_some_and(|health| !health.reachable && health.failure != Some(HealthFailure::Leak));
    let unreachable = proxy.state == ProxyState::Backoff
        && last_probe_read_nothing
        && proxy.backoff_count >= rules.backoff_cycles
        && proxy
            .failing_since
            .is_some_and(|since| now.signed_duration_since(since) >= rules.replace_after);
    if unreachable {
        return Some(ReplaceReason::Unreachable);
    }
    let bad_reputation = matches!(
        proxy.state,
        ProxyState::Healthy | ProxyState::Degraded | ProxyState::Backoff
    ) && health
        .is_some_and(|health| health.reputation_bad_lookups >= rules.bad_lookups);
    bad_reputation.then_some(ReplaceReason::Reputation)
}

/// Moves a proxy to `replacing` if the monthly budget and the provider's
/// own quota allow it.
///
/// `used_this_month` is the number of replacements requested since
/// [`month_start`]; the pool reads it inside the same transaction, under a
/// lock that serializes every caller. `provider_available` is the quota the
/// provider last reported, `None` when unknown.
pub fn start_replacement(
    proxy: &mut Proxy,
    reason: ReplaceReason,
    used_this_month: u32,
    provider_available: Option<u32>,
    now: DateTime<Utc>,
    rules: &PoolRules,
) -> ReplacementDecision {
    if !matches!(
        proxy.state,
        ProxyState::New | ProxyState::Healthy | ProxyState::Degraded | ProxyState::Backoff
    ) {
        return ReplacementDecision::NotEligible { state: proxy.state };
    }
    if used_this_month >= rules.replacements_per_month {
        return ReplacementDecision::BudgetExhausted {
            used: used_this_month,
            budget: rules.replacements_per_month,
        };
    }
    if provider_available == Some(0) {
        return ReplacementDecision::QuotaExhausted;
    }
    debug_assert!(is_allowed(proxy.state, ProxyState::Replacing));
    proxy.state = ProxyState::Replacing;
    proxy.backoff_until = None;
    proxy.replacement = Some(ProxyReplacement {
        requested_at: now,
        reason,
        ticket: None,
    });
    ReplacementDecision::Started
}

/// The provider replaced the proxy: it is retired. Its replacement record
/// stays and keeps counting against the month it was requested in. Returns
/// whether anything changed.
pub fn finish_replacement(proxy: &mut Proxy, now: DateTime<Utc>) -> bool {
    if proxy.state != ProxyState::Replacing {
        return false;
    }
    proxy.state = ProxyState::Retired;
    proxy.retired_at = Some(now);
    proxy.backoff_until = None;
    true
}

/// The replacement did not happen: the proxy goes back to `backoff` and its
/// place in the budget is freed. Returns whether anything changed.
pub fn fail_replacement(
    proxy: &mut Proxy,
    now: DateTime<Utc>,
    jitter: f64,
    rules: &PoolRules,
) -> bool {
    if proxy.state != ProxyState::Replacing {
        return false;
    }
    proxy.replacement = None;
    proxy.state = ProxyState::Backoff;
    // The wait of the cycle the proxy was in; the count is not raised,
    // because nothing new failed.
    proxy.backoff_until = Some(now + backoff_delay(proxy.backoff_count, rules, jitter));
    true
}

/// The proxy is gone from the provider's list: it is retired from any
/// state. Returns whether anything changed.
pub fn retire(proxy: &mut Proxy, now: DateTime<Utc>) -> bool {
    if !proxy.state.is_live() {
        return false;
    }
    proxy.state = ProxyState::Retired;
    proxy.retired_at = Some(now);
    proxy.backoff_until = None;
    true
}

/// An operator declares a recorded leak dealt with. The proxy stays where
/// it is; its next probe decides again. Returns whether a leak was
/// recorded.
pub fn clear_leak(proxy: &mut Proxy) -> bool {
    let Some(health) = proxy.last_health.as_mut() else {
        return false;
    };
    if health.leak_detected != Some(true) {
        return false;
    }
    health.leak_detected = None;
    if health.failure == Some(HealthFailure::Leak) {
        health.failure = None;
        // The cycles the leak caused do not count toward a replacement.
        proxy.backoff_count = 0;
    }
    true
}

#[cfg(test)]
mod tests;
