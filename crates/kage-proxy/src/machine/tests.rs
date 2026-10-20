//! The state machine: one test per rule, then property tests over random
//! event sequences (no transition outside the diagram, no backoff beyond its
//! cap, the monthly budget never exceeded).

use std::net::IpAddr;

use chrono::{DateTime, Duration, TimeZone, Utc};
use kage_core::config::Config;
use kage_core::domain::{
    GeoRecord, HealthFailure, Proxy, ProxyReplacement, ProxyState, ReplaceReason,
};
use proptest::prelude::*;

use super::{
    ALLOWED_TRANSITIONS, Effect, ExitObservation, PoolRules, ProbeReport, ReplacementDecision,
    ReputationReport, apply_probe, apply_reputation, backoff_delay, budget_window_start,
    clear_leak, expire_backoff, fail_replacement, finish_replacement, is_allowed, is_hosting,
    mark_shared_exit, month_start, replacement_due, retire, start_replacement,
};
use crate::test_support::{noon, proxy};

const EXIT: &str = "203.0.113.20";
const HOST: &str = "192.0.2.77";
/// A jitter in the middle of the range.
const MID: f64 = 0.5;

fn ip(text: &str) -> IpAddr {
    text.parse().unwrap()
}

fn rules() -> PoolRules {
    PoolRules::default()
}

fn ok(at: DateTime<Utc>) -> ProbeReport {
    ProbeReport {
        at,
        exit: Ok(ExitObservation {
            ip: ip(EXIT),
            latency_ms: 80,
            connect_ms: None,
            tls_ms: None,
        }),
        leak: Some(false),
        geo: None,
    }
}

fn failed(at: DateTime<Utc>) -> ProbeReport {
    ProbeReport {
        at,
        exit: Err(HealthFailure::Unreachable),
        leak: None,
        geo: None,
    }
}

fn leaked(at: DateTime<Utc>) -> ProbeReport {
    ProbeReport {
        at,
        exit: Ok(ExitObservation {
            ip: ip(HOST),
            latency_ms: 5,
            connect_ms: None,
            tls_ms: None,
        }),
        leak: Some(true),
        geo: None,
    }
}

fn geo(country: &str, at: DateTime<Utc>) -> GeoRecord {
    GeoRecord {
        country: country.parse().unwrap(),
        city: Some("Vienna".to_owned()),
        region: None,
        asn: Some("AS8447".to_owned()),
        org: Some("A1 Telekom Austria AG".to_owned()),
        timezone: Some("Europe/Vienna".to_owned()),
        loc: Some("48.2085,16.3721".to_owned()),
        ip: ip(EXIT),
        checked_at: at,
    }
}

fn with_geo(country: &str, at: DateTime<Utc>) -> ProbeReport {
    ProbeReport {
        geo: Some(geo(country, at)),
        ..ok(at)
    }
}

fn healthy(at: DateTime<Utc>) -> Proxy {
    let mut proxy = proxy("d-1");
    apply_probe(&mut proxy, &ok(at), false, MID, &rules());
    assert_eq!(proxy.state, ProxyState::Healthy);
    proxy
}

fn changed(from: ProxyState, to: ProxyState) -> Effect {
    Effect::StateChanged { from, to }
}

/// Fails the proxy at five-minute steps until it is in backoff, then lets
/// each backoff run out and fails the probe that follows, until `until`.
/// Returns the time of the last event.
fn fail_until(proxy: &mut Proxy, start: DateTime<Utc>, until: DateTime<Utc>) -> DateTime<Utc> {
    let rules = rules();
    let mut now = start;
    while now < until {
        if proxy.state == ProxyState::Backoff {
            now = proxy.backoff_until.unwrap();
            expire_backoff(proxy, now, false);
        } else {
            now += Duration::minutes(5);
        }
        apply_probe(proxy, &failed(now), false, MID, &rules);
    }
    now
}

#[test]
fn a_new_proxy_is_healthy_after_its_first_passing_check() {
    let mut proxy = proxy("d-1");

    let effects = apply_probe(&mut proxy, &ok(noon()), false, MID, &rules());

    assert_eq!(effects, [changed(ProxyState::New, ProxyState::Healthy)]);
    let health = proxy.last_health.as_ref().unwrap();
    assert!(health.reachable);
    assert_eq!(health.failure, None);
    assert_eq!(health.latency_ms, Some(80));
    assert_eq!(health.exit_ip, Some(ip(EXIT)));
    assert_eq!(health.leak_detected, Some(false));
    assert_eq!(health.checked_at, noon());
    assert_eq!(proxy.consecutive_failures, 0);
}

#[test]
fn failures_degrade_a_healthy_proxy_and_the_third_sends_it_into_backoff() {
    let mut proxy = healthy(noon());
    let rules = rules();
    let at = |minutes| noon() + Duration::minutes(minutes);

    let first = apply_probe(&mut proxy, &failed(at(5)), false, MID, &rules);
    assert_eq!(first, [changed(ProxyState::Healthy, ProxyState::Degraded)]);
    assert_eq!(proxy.consecutive_failures, 1);
    assert_eq!(proxy.failing_since, Some(at(5)));
    let health = proxy.last_health.as_ref().unwrap();
    assert!(!health.reachable);
    assert_eq!(health.failure, Some(HealthFailure::Unreachable));
    assert_eq!(health.latency_ms, None);
    // The last known exit address is kept for the record.
    assert_eq!(health.exit_ip, Some(ip(EXIT)));

    let second = apply_probe(&mut proxy, &failed(at(10)), false, MID, &rules);
    assert_eq!(second, []);
    assert_eq!(proxy.state, ProxyState::Degraded);
    assert_eq!(proxy.consecutive_failures, 2);

    let third = apply_probe(&mut proxy, &failed(at(15)), false, MID, &rules);
    assert_eq!(third, [changed(ProxyState::Degraded, ProxyState::Backoff)]);
    assert_eq!(proxy.consecutive_failures, 3);
    assert_eq!(proxy.backoff_count, 1);
    // The first backoff: one minute, jittered into its upper half.
    assert_eq!(proxy.backoff_until, Some(at(15) + Duration::seconds(45)));
    // The run of failures began with the first one.
    assert_eq!(proxy.failing_since, Some(at(5)));
}

#[test]
fn a_passing_check_brings_a_degraded_proxy_back_and_resets_the_counters() {
    let mut proxy = healthy(noon());
    let rules = rules();
    apply_probe(
        &mut proxy,
        &failed(noon() + Duration::minutes(5)),
        false,
        MID,
        &rules,
    );
    apply_probe(
        &mut proxy,
        &failed(noon() + Duration::minutes(10)),
        false,
        MID,
        &rules,
    );

    let effects = apply_probe(
        &mut proxy,
        &ok(noon() + Duration::minutes(15)),
        false,
        MID,
        &rules,
    );

    assert_eq!(
        effects,
        [changed(ProxyState::Degraded, ProxyState::Healthy)]
    );
    assert_eq!(proxy.consecutive_failures, 0);
    assert_eq!(proxy.failing_since, None);
    assert_eq!(proxy.backoff_count, 0);
}

#[test]
fn a_proxy_that_never_worked_goes_from_new_to_backoff() {
    let mut proxy = proxy("d-1");
    let rules = rules();

    for step in 1..=2 {
        let effects = apply_probe(
            &mut proxy,
            &failed(noon() + Duration::minutes(5 * step)),
            false,
            MID,
            &rules,
        );
        assert_eq!(effects, []);
        assert_eq!(proxy.state, ProxyState::New);
    }
    let effects = apply_probe(
        &mut proxy,
        &failed(noon() + Duration::minutes(15)),
        false,
        MID,
        &rules,
    );

    assert_eq!(effects, [changed(ProxyState::New, ProxyState::Backoff)]);
    assert!(proxy.backoff_until.is_some());
}

#[test]
fn a_backoff_that_ran_out_gives_one_probe_and_one_failure_doubles_the_wait() {
    let mut proxy = healthy(noon());
    let rules = rules();
    let mut now = noon();
    for _ in 0..3 {
        now += Duration::minutes(5);
        apply_probe(&mut proxy, &failed(now), false, MID, &rules);
    }
    assert_eq!(proxy.state, ProxyState::Backoff);
    let until = proxy.backoff_until.unwrap();

    // Not yet.
    assert_eq!(
        expire_backoff(&mut proxy, until - Duration::seconds(1), false),
        []
    );
    assert_eq!(proxy.state, ProxyState::Backoff);

    assert_eq!(
        expire_backoff(&mut proxy, until, false),
        [changed(ProxyState::Backoff, ProxyState::Degraded)]
    );
    assert_eq!(proxy.backoff_until, None);
    // The cycle count survives the expiry: the proxy is on probation.
    assert_eq!(proxy.backoff_count, 1);

    // One failure, not three, sends it back, for twice as long.
    let effects = apply_probe(&mut proxy, &failed(until), false, MID, &rules);
    assert_eq!(
        effects,
        [changed(ProxyState::Degraded, ProxyState::Backoff)]
    );
    assert_eq!(proxy.backoff_count, 2);
    assert_eq!(proxy.backoff_until, Some(until + Duration::seconds(90)));

    // A passing probe after the next expiry clears everything.
    let until = proxy.backoff_until.unwrap();
    expire_backoff(&mut proxy, until, false);
    let effects = apply_probe(&mut proxy, &ok(until), false, MID, &rules);
    assert_eq!(
        effects,
        [changed(ProxyState::Degraded, ProxyState::Healthy)]
    );
    assert_eq!(proxy.backoff_count, 0);
    assert_eq!(proxy.failing_since, None);
}

#[test]
fn an_operator_can_end_a_backoff_early() {
    let mut proxy = healthy(noon());
    fail_until(&mut proxy, noon(), noon() + Duration::minutes(15));
    assert_eq!(proxy.state, ProxyState::Backoff);

    assert_eq!(
        expire_backoff(&mut proxy, noon(), true),
        [changed(ProxyState::Backoff, ProxyState::Degraded)]
    );
    // Nothing to expire in any other state.
    assert_eq!(expire_backoff(&mut proxy, noon(), true), []);
}

#[test]
fn backoff_doubles_from_the_base_to_the_cap_and_jitters_into_the_upper_half() {
    let rules = rules();
    let minutes = |cycle| backoff_delay(cycle, &rules, 1.0).num_minutes();

    // 1 min x 2^k, capped at 1 h.
    assert_eq!(
        (0..9).map(minutes).collect::<Vec<_>>(),
        [1, 2, 4, 8, 16, 32, 60, 60, 60]
    );
    for cycle in [0, 3, 6, 40, u32::MAX] {
        let ceiling = backoff_delay(cycle, &rules, 1.0);
        assert_eq!(backoff_delay(cycle, &rules, 0.0), ceiling / 2);
        assert_eq!(
            backoff_delay(cycle, &rules, 0.5).num_milliseconds(),
            ceiling.num_milliseconds() * 3 / 4
        );
        // Out-of-range jitter is clamped, never trusted.
        assert_eq!(backoff_delay(cycle, &rules, 7.0), ceiling);
        assert_eq!(backoff_delay(cycle, &rules, -1.0), ceiling / 2);
        assert_eq!(backoff_delay(cycle, &rules, f64::NAN), ceiling);
        assert!(ceiling <= rules.backoff_cap);
    }
}

#[test]
fn replacement_needs_both_enough_cycles_and_enough_time() {
    let rules = rules();
    let mut proxy = healthy(noon());
    let start = noon();

    // Three backoff cycles are over after about half an hour: far too early.
    let now = fail_until(&mut proxy, start, start + Duration::minutes(40));
    assert_eq!(proxy.state, ProxyState::Backoff);
    assert!(proxy.backoff_count >= rules.backoff_cycles);
    assert_eq!(replacement_due(&proxy, now, &rules), None);

    // Still failing five hours in: not yet.
    let now = fail_until(&mut proxy, now, start + Duration::hours(5));
    assert_eq!(replacement_due(&proxy, now, &rules), None);

    // Six hours of failing without a break, counted from the first failure.
    let failing_since = proxy.failing_since.unwrap();
    assert_eq!(failing_since, start + Duration::minutes(5));
    let just_before = failing_since + rules.replace_after - Duration::seconds(1);
    assert_eq!(replacement_due(&proxy, just_before, &rules), None);
    assert_eq!(
        replacement_due(&proxy, failing_since + rules.replace_after, &rules),
        Some(ReplaceReason::Unreachable)
    );

    // Time alone is not enough either: a proxy that has been failing for a
    // day but is only in its second cycle does not qualify.
    let mut slow = proxy.clone();
    slow.backoff_count = rules.backoff_cycles - 1;
    assert_eq!(
        replacement_due(&slow, failing_since + Duration::days(1), &rules),
        None
    );
    // Nor does one that is no longer in backoff.
    let mut probing = proxy.clone();
    expire_backoff(&mut probing, failing_since + Duration::days(1), true);
    assert_eq!(
        replacement_due(&probing, failing_since + Duration::days(1), &rules),
        None
    );
}

#[test]
fn one_passing_check_restarts_the_six_hours() {
    let rules = rules();
    let mut proxy = healthy(noon());
    let now = fail_until(&mut proxy, noon(), noon() + Duration::hours(5));

    // The address answers once, then fails again.
    let until = proxy.backoff_until.unwrap().max(now);
    expire_backoff(&mut proxy, until, false);
    apply_probe(&mut proxy, &ok(until), false, MID, &rules);
    let now = fail_until(&mut proxy, until, until + Duration::hours(2));

    // Seven hours after the first failure, but only two since the break.
    assert!(now >= noon() + Duration::hours(7));
    assert_eq!(replacement_due(&proxy, now, &rules), None);
}

/// Decision D6 and consult 5g of M15: a proxy that leaves by an older
/// proxy's address is marked, kept `degraded` while it lasts, and clear
/// again when it leaves by an address of its own.
#[test]
fn a_proxy_that_leaves_by_an_older_ones_address_is_degraded_while_it_lasts() {
    let older = kage_core::ids::ProxyId::new();
    let mut proxy = healthy(noon());

    // A probe that read nothing, or saw a leak, says nothing of it.
    assert_eq!(
        mark_shared_exit(&mut proxy, &failed(noon()), Some(older)),
        []
    );
    assert_eq!(
        mark_shared_exit(&mut proxy, &leaked(noon()), Some(older)),
        []
    );
    assert!(!proxy.exit_shared());

    let at = noon() + Duration::minutes(5);
    let effects = mark_shared_exit(&mut proxy, &ok(at), Some(older));
    assert_eq!(effects, [Effect::ExitShared { with: older }]);
    let effects = apply_probe(&mut proxy, &ok(at), false, MID, &rules());
    assert_eq!(proxy.state, ProxyState::Degraded);
    assert_eq!(
        effects,
        [changed(ProxyState::Healthy, ProxyState::Degraded)]
    );
    assert!(proxy.exit_shared());
    assert!(!proxy.carries_persona());
    // Still shared: nothing new is said.
    let later = at + Duration::minutes(5);
    assert_eq!(mark_shared_exit(&mut proxy, &ok(later), Some(older)), []);
    apply_probe(&mut proxy, &ok(later), false, MID, &rules());
    assert_eq!(proxy.state, ProxyState::Degraded);

    // An address of its own again: cleared, and healthy at once.
    let apart = later + Duration::minutes(5);
    assert_eq!(
        mark_shared_exit(&mut proxy, &ok(apart), None),
        [Effect::ExitSharedCleared]
    );
    apply_probe(&mut proxy, &ok(apart), false, MID, &rules());
    assert_eq!(proxy.state, ProxyState::Healthy);
    assert!(proxy.carries_persona());

    // A proxy never probed gets a record of its own.
    let mut fresh = proxy_fixture();
    assert_eq!(
        mark_shared_exit(&mut fresh, &ok(noon()), Some(older)),
        [Effect::ExitShared { with: older }]
    );
    assert_eq!(
        fresh.last_health.as_ref().unwrap().exit_shared_with,
        Some(older)
    );
}

fn proxy_fixture() -> Proxy {
    proxy("d-2")
}

#[test]
fn a_pool_wide_outage_freezes_the_counters() {
    let rules = rules();
    let mut proxy = healthy(noon());

    for step in 1..=20 {
        let effects = apply_probe(
            &mut proxy,
            &failed(noon() + Duration::minutes(5 * step)),
            true,
            MID,
            &rules,
        );
        assert_eq!(effects, []);
    }

    assert_eq!(proxy.state, ProxyState::Healthy);
    assert_eq!(proxy.consecutive_failures, 0);
    assert_eq!(proxy.failing_since, None);
    assert_eq!(proxy.backoff_count, 0);
    // The failure itself is on record.
    let health = proxy.last_health.as_ref().unwrap();
    assert!(!health.reachable);
    assert_eq!(health.checked_at, noon() + Duration::minutes(100));

    // A passing probe is never frozen: it always counts.
    let mut degraded = healthy(noon());
    apply_probe(&mut degraded, &failed(noon()), false, MID, &rules);
    assert_eq!(degraded.state, ProxyState::Degraded);
    apply_probe(&mut degraded, &ok(noon()), true, MID, &rules);
    assert_eq!(degraded.state, ProxyState::Healthy);
}

#[test]
fn a_leak_sends_the_proxy_straight_into_backoff_from_every_checked_state() {
    let rules = rules();
    let mut never_checked = proxy("d-1");
    let mut was_healthy = healthy(noon());
    let mut was_degraded = healthy(noon());
    apply_probe(&mut was_degraded, &failed(noon()), false, MID, &rules);

    for (proxy, from) in [
        (&mut never_checked, ProxyState::New),
        (&mut was_healthy, ProxyState::Healthy),
        (&mut was_degraded, ProxyState::Degraded),
    ] {
        let effects = apply_probe(proxy, &leaked(noon()), false, MID, &rules);

        assert_eq!(
            effects,
            [
                Effect::Leak { exit_ip: ip(HOST) },
                changed(from, ProxyState::Backoff)
            ],
            "{from}"
        );
        assert!(proxy.leak_detected());
        let health = proxy.last_health.as_ref().unwrap();
        assert_eq!(health.failure, Some(HealthFailure::Leak));
        assert!(health.reachable);
        assert!(proxy.backoff_until.is_some());
        // A leak is not a run of unreachable checks.
        assert_eq!(proxy.consecutive_failures, 0);
        assert_eq!(proxy.failing_since, None);
    }

    // An outage flag does not excuse a leak.
    let mut during_outage = healthy(noon());
    apply_probe(&mut during_outage, &leaked(noon()), true, MID, &rules);
    assert_eq!(during_outage.state, ProxyState::Backoff);
}

#[test]
fn a_leak_never_leads_to_a_replacement() {
    let rules = rules();
    let mut proxy = healthy(noon());
    let mut now = noon();

    // The proxy leaks at every probe for two days.
    for _ in 0..80 {
        apply_probe(&mut proxy, &leaked(now), false, MID, &rules);
        assert_eq!(proxy.state, ProxyState::Backoff);
        assert_eq!(
            replacement_due(&proxy, now + Duration::days(30), &rules),
            None
        );
        now = proxy.backoff_until.unwrap();
        expire_backoff(&mut proxy, now, false);
    }
    assert!(now > noon() + Duration::days(2));
    assert!(proxy.backoff_count >= rules.backoff_cycles);

    // Then it stops leaking and stops answering. The leak cycles do not
    // count: the run of failures starts now, from zero.
    apply_probe(&mut proxy, &failed(now), false, MID, &rules);
    assert_eq!(proxy.state, ProxyState::Degraded);
    assert_eq!(proxy.backoff_count, 0);
    assert_eq!(proxy.failing_since, Some(now));
    assert_eq!(proxy.consecutive_failures, 1);
    assert_eq!(replacement_due(&proxy, now, &rules), None);
    // The leak stays on record until a probe shows a distinct exit address.
    assert!(proxy.leak_detected());
}

#[test]
fn only_a_probe_with_a_distinct_exit_address_clears_a_leak() {
    let rules = rules();
    let mut proxy = healthy(noon());
    apply_probe(&mut proxy, &leaked(noon()), false, MID, &rules);
    let later = proxy.backoff_until.unwrap();
    expire_backoff(&mut proxy, later, false);

    // Unreachable: nothing was seen, the flag stays.
    let effects = apply_probe(&mut proxy, &failed(later), false, MID, &rules);
    assert!(!effects.contains(&Effect::LeakCleared));
    assert!(proxy.leak_detected());

    // Reachable, but the host address is unknown so nothing can be
    // compared: the flag stays and the proxy is parked again.
    let unknown = ProbeReport {
        leak: None,
        ..ok(later)
    };
    let effects = apply_probe(&mut proxy, &unknown, false, MID, &rules);
    assert!(!effects.contains(&Effect::LeakCleared));
    assert!(proxy.leak_detected());
    assert_eq!(proxy.state, ProxyState::Backoff);

    // A distinct exit address: cleared, and healthy again.
    let later = proxy.backoff_until.unwrap();
    expire_backoff(&mut proxy, later, false);
    let effects = apply_probe(&mut proxy, &ok(later), false, MID, &rules);
    assert_eq!(
        effects,
        [
            Effect::LeakCleared,
            changed(ProxyState::Degraded, ProxyState::Healthy)
        ]
    );
    assert!(!proxy.leak_detected());
    assert_eq!(proxy.backoff_count, 0);

    // Leaking again is reported again.
    let effects = apply_probe(&mut proxy, &leaked(later), false, MID, &rules);
    assert_eq!(effects[0], Effect::Leak { exit_ip: ip(HOST) });
}

#[test]
fn an_unknown_host_address_is_not_a_leak() {
    let mut proxy = proxy("d-1");
    let report = ProbeReport {
        leak: None,
        ..ok(noon())
    };

    apply_probe(&mut proxy, &report, false, MID, &rules());

    assert_eq!(proxy.state, ProxyState::Healthy);
    assert_eq!(proxy.last_health.as_ref().unwrap().leak_detected, None);
    assert!(!proxy.leak_detected());
}

#[test]
fn an_operator_can_clear_a_leak() {
    let rules = rules();
    let mut proxy = healthy(noon());
    assert!(!clear_leak(&mut proxy));
    assert!(!clear_leak(&mut crate::test_support::proxy(
        "never-checked"
    )));

    apply_probe(&mut proxy, &leaked(noon()), false, MID, &rules);
    assert!(clear_leak(&mut proxy));

    assert!(!proxy.leak_detected());
    let health = proxy.last_health.as_ref().unwrap();
    assert_eq!(health.leak_detected, None);
    assert_eq!(health.failure, None);
    // The proxy stays parked until its next probe, and the cycles the leak
    // caused are forgotten.
    assert_eq!(proxy.state, ProxyState::Backoff);
    assert_eq!(proxy.backoff_count, 0);
    assert!(!clear_leak(&mut proxy));
}

#[test]
fn the_first_geo_lookup_is_recorded_and_fills_the_asn() {
    let mut proxy = proxy("d-1");

    let effects = apply_probe(&mut proxy, &with_geo("DE", noon()), false, MID, &rules());

    assert_eq!(
        effects,
        [
            Effect::GeoObserved {
                current: geo("DE", noon())
            },
            changed(ProxyState::New, ProxyState::Healthy)
        ]
    );
    let health = proxy.last_health.as_ref().unwrap();
    assert_eq!(health.exit_country.unwrap().as_str(), "DE");
    assert_eq!(health.country_match, Some(true));
    assert_eq!(health.geo_mismatches, 0);
    assert_eq!(health.geo, Some(geo("DE", noon())));
    assert_eq!(proxy.asn.as_deref(), Some("AS8447"));
    assert_eq!(proxy.effective_country().unwrap().as_str(), "DE");
}

#[test]
fn two_mismatching_lookups_in_a_row_degrade_the_proxy_and_a_match_clears_it() {
    let rules = rules();
    let mut proxy = healthy(noon());
    let day = |days| noon() + Duration::days(days);

    // One lookup that disagrees is not enough: geo databases lag.
    let first = apply_probe(&mut proxy, &with_geo("AT", day(1)), false, MID, &rules);
    assert_eq!(
        first,
        [Effect::GeoObserved {
            current: geo("AT", day(1))
        }]
    );
    assert_eq!(proxy.state, ProxyState::Healthy);
    assert_eq!(
        proxy.last_health.as_ref().unwrap().country_match,
        Some(false)
    );

    // A probe without a lookup in between changes nothing about it.
    apply_probe(&mut proxy, &ok(day(1)), false, MID, &rules);
    assert_eq!(proxy.last_health.as_ref().unwrap().geo_mismatches, 1);
    assert_eq!(proxy.state, ProxyState::Healthy);

    let second = apply_probe(&mut proxy, &with_geo("AT", day(2)), false, MID, &rules);
    assert_eq!(
        second,
        [
            Effect::GeoMismatch {
                provider: "DE".parse().unwrap(),
                observed: "AT".parse().unwrap()
            },
            changed(ProxyState::Healthy, ProxyState::Degraded)
        ]
    );
    // Degraded is still usable, and it is not on its way to a replacement.
    assert!(proxy.state.is_usable());
    assert_eq!(replacement_due(&proxy, day(30), &rules), None);
    // The observed country is the one personas and policies go by.
    assert_eq!(proxy.effective_country().unwrap().as_str(), "AT");

    // It stays degraded while the mismatch stands, without a new alert.
    let third = apply_probe(&mut proxy, &with_geo("AT", day(3)), false, MID, &rules);
    assert_eq!(third, []);
    let between = apply_probe(&mut proxy, &ok(day(3)), false, MID, &rules);
    assert_eq!(between, []);
    assert_eq!(proxy.state, ProxyState::Degraded);

    let cleared = apply_probe(&mut proxy, &with_geo("DE", day(4)), false, MID, &rules);
    assert_eq!(
        cleared,
        [
            Effect::GeoChanged {
                previous: geo("AT", day(3)),
                current: geo("DE", day(4))
            },
            Effect::GeoMismatchCleared,
            changed(ProxyState::Degraded, ProxyState::Healthy)
        ]
    );
}

#[test]
fn a_changed_geo_record_is_an_event_and_an_unchanged_one_is_not() {
    let rules = rules();
    let mut proxy = healthy(noon());
    apply_probe(&mut proxy, &with_geo("DE", noon()), false, MID, &rules);

    // The same place a day later: nothing to report.
    let same = apply_probe(
        &mut proxy,
        &with_geo("DE", noon() + Duration::days(1)),
        false,
        MID,
        &rules,
    );
    assert_eq!(same, []);

    // Another timezone in the same country: the persona's clock is at stake.
    let mut moved = geo("DE", noon() + Duration::days(2));
    moved.timezone = Some("Europe/Berlin".to_owned());
    moved.asn = None;
    moved.org = Some("Deutsche Telekom AG".to_owned());
    let report = ProbeReport {
        geo: Some(moved.clone()),
        ..ok(noon() + Duration::days(2))
    };
    let effects = apply_probe(&mut proxy, &report, false, MID, &rules);
    assert_eq!(
        effects,
        [Effect::GeoChanged {
            previous: geo("DE", noon() + Duration::days(1)),
            current: moved
        }]
    );
    // Without a number the operator's name stands in.
    assert_eq!(proxy.asn.as_deref(), Some("Deutsche Telekom AG"));

    // A proxy whose provider names no country cannot mismatch.
    let mut unknown = healthy(noon());
    unknown.country = None;
    for day in 1..=3 {
        apply_probe(
            &mut unknown,
            &with_geo("AT", noon() + Duration::days(day)),
            false,
            MID,
            &rules,
        );
    }
    assert_eq!(unknown.state, ProxyState::Healthy);
    assert_eq!(unknown.last_health.as_ref().unwrap().country_match, None);
    assert_eq!(unknown.effective_country().unwrap().as_str(), "AT");
}

#[test]
fn a_geo_record_from_an_unreachable_probe_is_ignored() {
    let mut proxy = healthy(noon());
    let report = ProbeReport {
        geo: Some(geo("AT", noon())),
        ..failed(noon())
    };

    apply_probe(&mut proxy, &report, false, MID, &rules());

    assert_eq!(proxy.last_health.as_ref().unwrap().geo, None);
}

#[test]
fn a_bad_reputation_twice_in_a_row_degrades_and_qualifies_for_replacement() {
    let rules = rules();
    let mut proxy = healthy(noon());
    let lookup = |days, score| ReputationReport {
        at: noon() + Duration::days(days),
        score,
        usage_type: Some("Fixed Line ISP".to_owned()),
    };

    assert_eq!(apply_reputation(&mut proxy, &lookup(0, 80), &rules), []);
    assert_eq!(proxy.state, ProxyState::Healthy);
    assert_eq!(replacement_due(&proxy, noon(), &rules), None);

    let second = apply_reputation(&mut proxy, &lookup(1, 75), &rules);
    assert_eq!(
        second,
        [
            Effect::ReputationBad { score: 75 },
            changed(ProxyState::Healthy, ProxyState::Degraded)
        ]
    );
    let health = proxy.last_health.as_ref().unwrap();
    assert_eq!(health.reputation_score, Some(75));
    assert_eq!(
        health.reputation_checked_at,
        Some(noon() + Duration::days(1))
    );
    assert_eq!(health.usage_type.as_deref(), Some("Fixed Line ISP"));
    assert_eq!(
        replacement_due(&proxy, noon(), &rules),
        Some(ReplaceReason::Reputation)
    );
    // While it is bad a passing probe keeps it degraded, not healthy.
    apply_probe(
        &mut proxy,
        &ok(noon() + Duration::days(1)),
        false,
        MID,
        &rules,
    );
    assert_eq!(proxy.state, ProxyState::Degraded);
    // A third bad lookup does not alert again.
    assert_eq!(apply_reputation(&mut proxy, &lookup(2, 99), &rules), []);

    // One score below the line and the streak is over.
    let recovered = apply_reputation(&mut proxy, &lookup(3, 74), &rules);
    assert_eq!(recovered, [Effect::ReputationRecovered]);
    assert_eq!(replacement_due(&proxy, noon(), &rules), None);
    apply_probe(
        &mut proxy,
        &ok(noon() + Duration::days(3)),
        false,
        MID,
        &rules,
    );
    assert_eq!(proxy.state, ProxyState::Healthy);

    // A single bad lookup between good ones never counts.
    apply_reputation(&mut proxy, &lookup(4, 100), &rules);
    apply_reputation(&mut proxy, &lookup(5, 0), &rules);
    assert_eq!(apply_reputation(&mut proxy, &lookup(6, 100), &rules), []);
}

#[test]
fn a_hosting_usage_type_is_reported_once_per_change() {
    let rules = rules();
    let mut proxy = healthy(noon());
    let lookup = |usage: Option<&str>| ReputationReport {
        at: noon(),
        score: 0,
        usage_type: usage.map(str::to_owned),
    };
    let hosting = "Data Center/Web Hosting/Transit";

    assert_eq!(
        apply_reputation(&mut proxy, &lookup(Some(hosting)), &rules),
        [Effect::UsageTypeSuspicious {
            usage_type: hosting.to_owned()
        }]
    );
    // The same answer again is not news, and it never changes the state.
    assert_eq!(
        apply_reputation(&mut proxy, &lookup(Some(hosting)), &rules),
        []
    );
    assert_eq!(proxy.state, ProxyState::Healthy);
    assert_eq!(replacement_due(&proxy, noon(), &rules), None);

    assert_eq!(
        apply_reputation(&mut proxy, &lookup(Some("Fixed Line ISP")), &rules),
        []
    );
    assert_eq!(apply_reputation(&mut proxy, &lookup(None), &rules), []);
    assert_eq!(proxy.last_health.as_ref().unwrap().usage_type, None);
    assert_eq!(
        apply_reputation(
            &mut proxy,
            &lookup(Some("Content Delivery Network")),
            &rules
        )
        .len(),
        1
    );

    for (usage, expected) in [
        ("Data Center/Web Hosting/Transit", true),
        ("datacenter", true),
        ("Web Hosting", true),
        ("Content Delivery Network", true),
        ("Fixed Line ISP", false),
        ("Mobile ISP", false),
        ("Commercial", false),
        ("", false),
    ] {
        assert_eq!(is_hosting(usage), expected, "{usage}");
    }
}

#[test]
fn a_reputation_lookup_on_a_retired_proxy_changes_nothing() {
    let mut proxy = healthy(noon());
    retire(&mut proxy, noon());
    let before = proxy.clone();

    let effects = apply_reputation(
        &mut proxy,
        &ReputationReport {
            at: noon(),
            score: 100,
            usage_type: None,
        },
        &rules(),
    );

    assert_eq!(effects, []);
    assert_eq!(proxy, before);

    // A proxy that was never probed still takes the lookup.
    let mut unprobed = crate::test_support::proxy("d-2");
    apply_reputation(
        &mut unprobed,
        &ReputationReport {
            at: noon(),
            score: 10,
            usage_type: None,
        },
        &rules(),
    );
    assert_eq!(
        unprobed.last_health.as_ref().unwrap().reputation_score,
        Some(10)
    );
    assert_eq!(unprobed.state, ProxyState::New);
}

#[test]
fn a_replacement_starts_only_within_the_budget_and_the_providers_quota() {
    let rules = rules();
    let start = |proxy: &mut Proxy, used, available| {
        start_replacement(
            proxy,
            ReplaceReason::Unreachable,
            used,
            available,
            noon(),
            &rules,
        )
    };

    // Budget used up: nothing changes.
    let mut proxy = healthy(noon());
    let before = proxy.clone();
    assert_eq!(
        start(&mut proxy, 5, Some(10)),
        ReplacementDecision::BudgetExhausted { used: 5, budget: 5 }
    );
    assert_eq!(
        start(&mut proxy, 9, None),
        ReplacementDecision::BudgetExhausted { used: 9, budget: 5 }
    );
    assert_eq!(proxy, before);

    // The provider's own quota is at zero: nothing changes.
    assert_eq!(
        start(&mut proxy, 0, Some(0)),
        ReplacementDecision::QuotaExhausted
    );
    assert_eq!(proxy, before);

    // The last slot, with the provider's quota unknown or positive.
    assert_eq!(start(&mut proxy, 4, None), ReplacementDecision::Started);
    assert_eq!(proxy.state, ProxyState::Replacing);
    assert_eq!(
        proxy.replacement,
        Some(ProxyReplacement {
            requested_at: noon(),
            reason: ReplaceReason::Unreachable,
            ticket: None,
        })
    );
    assert_eq!(proxy.backoff_until, None);

    // Already being replaced, or gone.
    assert_eq!(
        start(&mut proxy, 0, None),
        ReplacementDecision::NotEligible {
            state: ProxyState::Replacing
        }
    );
    finish_replacement(&mut proxy, noon());
    assert_eq!(
        start(&mut proxy, 0, None),
        ReplacementDecision::NotEligible {
            state: ProxyState::Retired
        }
    );

    // From every other live state.
    for state in [
        ProxyState::New,
        ProxyState::Healthy,
        ProxyState::Degraded,
        ProxyState::Backoff,
    ] {
        let mut proxy = crate::test_support::proxy("d-9");
        proxy.state = state;
        assert_eq!(
            start(&mut proxy, 0, Some(1)),
            ReplacementDecision::Started,
            "{state}"
        );
    }
}

#[test]
fn a_finished_replacement_retires_and_a_failed_one_frees_its_slot() {
    let rules = rules();
    let mut done = healthy(noon());
    assert!(!finish_replacement(&mut done, noon()));
    assert!(!fail_replacement(&mut done, noon(), MID, &rules));
    start_replacement(&mut done, ReplaceReason::Operator, 0, None, noon(), &rules);
    let mut failed_one = done.clone();

    assert!(finish_replacement(&mut done, noon() + Duration::minutes(2)));
    assert_eq!(done.state, ProxyState::Retired);
    assert_eq!(done.retired_at, Some(noon() + Duration::minutes(2)));
    // The record stays: it is what the monthly budget counts.
    assert!(done.replacement.is_some());

    assert!(fail_replacement(
        &mut failed_one,
        noon() + Duration::minutes(2),
        MID,
        &rules
    ));
    assert_eq!(failed_one.state, ProxyState::Backoff);
    assert_eq!(failed_one.replacement, None);
    assert_eq!(
        failed_one.backoff_until,
        Some(noon() + Duration::minutes(2) + Duration::seconds(45))
    );
    assert_eq!(failed_one.backoff_count, 0);
}

#[test]
fn a_proxy_gone_from_the_list_is_retired_from_any_state_and_stays_retired() {
    let rules = rules();
    for state in [
        ProxyState::New,
        ProxyState::Healthy,
        ProxyState::Degraded,
        ProxyState::Backoff,
        ProxyState::Replacing,
    ] {
        let mut proxy = proxy("d-1");
        proxy.state = state;
        proxy.backoff_until = Some(noon());

        assert!(retire(&mut proxy, noon()), "{state}");

        assert_eq!(proxy.state, ProxyState::Retired);
        assert_eq!(proxy.retired_at, Some(noon()));
        assert_eq!(proxy.backoff_until, None);
    }

    let mut retired = healthy(noon());
    retire(&mut retired, noon());
    let before = retired.clone();
    assert!(!retire(&mut retired, noon() + Duration::days(1)));
    assert_eq!(expire_backoff(&mut retired, noon(), true), []);
    assert!(!finish_replacement(&mut retired, noon()));
    assert!(!fail_replacement(&mut retired, noon(), MID, &rules));
    assert_eq!(retired, before);

    // A probe on a retired or replacing proxy is recorded and nothing more.
    let effects = apply_probe(&mut retired, &failed(noon()), false, MID, &rules);
    assert_eq!(effects, []);
    assert_eq!(retired.state, ProxyState::Retired);
    assert_eq!(retired.consecutive_failures, 0);
    let mut replacing = healthy(noon());
    start_replacement(
        &mut replacing,
        ReplaceReason::Operator,
        0,
        None,
        noon(),
        &rules,
    );
    assert_eq!(
        apply_probe(&mut replacing, &ok(noon()), false, MID, &rules),
        []
    );
    assert_eq!(replacing.state, ProxyState::Replacing);
    // A leak is reported whatever the state.
    assert_eq!(
        apply_probe(&mut replacing, &leaked(noon()), false, MID, &rules),
        [Effect::Leak { exit_ip: ip(HOST) }]
    );
    assert_eq!(replacing.state, ProxyState::Replacing);
    assert_eq!(
        apply_probe(&mut replacing, &ok(noon()), false, MID, &rules),
        [Effect::LeakCleared]
    );
}

#[test]
fn a_refused_login_counts_like_any_other_failure() {
    let mut proxy = healthy(noon());
    let report = ProbeReport {
        exit: Err(HealthFailure::ProxyAuth),
        ..failed(noon())
    };

    apply_probe(&mut proxy, &report, false, MID, &rules());

    assert_eq!(proxy.state, ProxyState::Degraded);
    assert_eq!(
        proxy.last_health.as_ref().unwrap().failure,
        Some(HealthFailure::ProxyAuth)
    );
}

#[test]
fn the_budget_window_is_the_utc_calendar_month() {
    let at = |day, hour| Utc.with_ymd_and_hms(2026, 10, day, hour, 30, 15).unwrap();
    let first = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap();

    assert_eq!(month_start(at(1, 0)), first);
    assert_eq!(month_start(at(15, 12)), first);
    assert_eq!(month_start(at(31, 23)), first);
    assert_eq!(month_start(first), first);
    assert_eq!(
        month_start(first - Duration::seconds(1)),
        Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap()
    );
    assert_eq!(
        month_start(Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap()),
        Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap()
    );
    // Without a term, or with one that says nothing, it is the month.
    let now = at(15, 12);
    assert_eq!(budget_window_start(now, None), first);
    assert_eq!(budget_window_start(now, Some((now, now))), first);
    assert_eq!(
        budget_window_start(
            now,
            Some((now + Duration::days(1), now + Duration::days(31)))
        ),
        first
    );
}

#[test]
fn the_budget_window_follows_the_providers_subscription_term() {
    // The owner's plan: bought on the 9th at 12:20, thirty days long.
    let start = Utc.with_ymd_and_hms(2026, 10, 9, 12, 20, 31).unwrap();
    let end = start + Duration::days(30);
    let term = Some((start, end));

    // Inside the term the window starts where the term does, across the
    // turn of the calendar month.
    for now in [
        start,
        start + Duration::seconds(1),
        Utc.with_ymd_and_hms(2026, 10, 31, 23, 59, 59).unwrap(),
        Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap(),
        end - Duration::seconds(1),
    ] {
        assert_eq!(budget_window_start(now, term), start, "{now}");
    }
    // An account read that is older than its term: the plan renewed, and
    // the window is the term the plan is in now.
    assert_eq!(budget_window_start(end, term), end);
    assert_eq!(budget_window_start(end + Duration::days(29), term), end);
    assert_eq!(
        budget_window_start(end + Duration::days(30), term),
        end + Duration::days(30)
    );
    assert_eq!(
        budget_window_start(start + Duration::days(95), term),
        start + Duration::days(90)
    );
    // Before the term began the account says nothing about now.
    assert_eq!(
        budget_window_start(start - Duration::seconds(1), term),
        Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()
    );
}

#[test]
fn the_transition_table_is_the_diagram_plus_the_documented_additions() {
    // No duplicates and no self-loops.
    for (index, edge) in ALLOWED_TRANSITIONS.iter().enumerate() {
        assert_ne!(edge.0, edge.1, "{edge:?}");
        assert!(!ALLOWED_TRANSITIONS[..index].contains(edge), "{edge:?}");
    }
    // Retired is final and new is never entered.
    assert!(
        ALLOWED_TRANSITIONS
            .iter()
            .all(|(from, to)| *from != ProxyState::Retired && *to != ProxyState::New)
    );
    // The edges of spec §7.3.
    for (from, to) in [
        (ProxyState::New, ProxyState::Healthy),
        (ProxyState::Healthy, ProxyState::Degraded),
        (ProxyState::Degraded, ProxyState::Backoff),
        (ProxyState::Backoff, ProxyState::Degraded),
        (ProxyState::Degraded, ProxyState::Healthy),
        (ProxyState::Backoff, ProxyState::Replacing),
        (ProxyState::Replacing, ProxyState::Retired),
    ] {
        assert!(is_allowed(from, to), "{from} -> {to}");
    }
    // Things that must never happen.
    for (from, to) in [
        (ProxyState::Backoff, ProxyState::Healthy),
        (ProxyState::New, ProxyState::Degraded),
        (ProxyState::Replacing, ProxyState::Healthy),
        (ProxyState::Replacing, ProxyState::Degraded),
        (ProxyState::Retired, ProxyState::Healthy),
        (ProxyState::Healthy, ProxyState::New),
    ] {
        assert!(!is_allowed(from, to), "{from} -> {to}");
    }
    for state in ProxyState::ALL {
        assert!(is_allowed(*state, *state));
    }
    assert_eq!(ALLOWED_TRANSITIONS.len(), 17);
}

#[test]
fn the_default_rules_are_the_configured_defaults() {
    let env = [
        ("KAGE__DATABASE__URL", "postgres://kage@localhost/kage"),
        ("KAGE__SIDECAR__TOKEN", "token"),
        (
            "KAGE__SECRETS__MASTER_KEY",
            "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
        ),
    ]
    .map(|(name, value)| (name.to_owned(), value.to_owned()));
    let config = Config::load(None, env).unwrap();

    assert_eq!(PoolRules::from_config(&config), PoolRules::default());
}

// --- Property tests ---------------------------------------------------------

/// One thing that can happen to a proxy.
#[derive(Debug, Clone)]
enum Op {
    /// A probe that read a distinct exit address, maybe with a geo lookup.
    Pass { geo: Option<bool> },
    /// A probe that read nothing.
    Fail { auth: bool, outage: bool },
    /// A probe that read the host's own address.
    Leak,
    /// A probe that read an address but could not compare it.
    PassUnknownHost,
    /// The pool tick: ends a backoff that ran out.
    Expire { force: bool },
    /// A reputation lookup.
    Reputation { score: u8, hosting: bool },
    /// A request to replace the proxy.
    Replace {
        reason: ReplaceReason,
        available: Option<u32>,
    },
    /// The provider finished the replacement.
    Finish,
    /// The provider failed the replacement.
    FailReplacement,
    /// The proxy vanished from the provider's list.
    Retire,
    /// An operator cleared the leak flag.
    ClearLeak,
    /// Time passes.
    Wait { minutes: u32 },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => proptest::option::of(any::<bool>()).prop_map(|geo| Op::Pass { geo }),
        10 => (any::<bool>(), proptest::bool::weighted(0.1))
            .prop_map(|(auth, outage)| Op::Fail { auth, outage }),
        2 => Just(Op::Leak),
        1 => Just(Op::PassUnknownHost),
        8 => proptest::bool::weighted(0.15).prop_map(|force| Op::Expire { force }),
        3 => (any::<u8>(), any::<bool>())
            .prop_map(|(score, hosting)| Op::Reputation { score: score % 101, hosting }),
        5 => (
            prop_oneof![
                Just(ReplaceReason::Unreachable),
                Just(ReplaceReason::Reputation),
                Just(ReplaceReason::Operator)
            ],
            proptest::option::of(0_u32..3)
        )
            .prop_map(|(reason, available)| Op::Replace { reason, available }),
        2 => Just(Op::Finish),
        2 => Just(Op::FailReplacement),
        1 => Just(Op::Retire),
        1 => Just(Op::ClearLeak),
        10 => prop_oneof![1_u32..30, 30_u32..600, 600_u32..20_000]
            .prop_map(|minutes| Op::Wait { minutes }),
    ]
}

fn any_rules() -> impl Strategy<Value = PoolRules> {
    (
        1_u32..6,
        1_i64..600,
        0_i64..7200,
        1_u32..6,
        0_i64..720,
        0_u32..7,
        1_u32..4,
        (1_u8..=100, 1_u32..4),
    )
        .prop_map(
            |(n, base_s, extra_s, m, replace_after_min, budget, geo, (bad_score, bad_lookups))| {
                PoolRules {
                    failure_threshold: n,
                    backoff_base: Duration::seconds(base_s),
                    backoff_cap: Duration::seconds(base_s + extra_s),
                    backoff_cycles: m,
                    replace_after: Duration::minutes(replace_after_min),
                    replacements_per_month: budget,
                    geo_mismatches: geo,
                    bad_score,
                    bad_lookups,
                    ..PoolRules::default()
                }
            },
        )
}

/// The number the store counts inside the replacement transaction:
/// replacements requested in the calendar month of `now`, retired proxies
/// included.
fn used_this_month(pool: &[Proxy], now: DateTime<Utc>) -> u32 {
    let start = month_start(now);
    let used = pool
        .iter()
        .filter_map(|proxy| proxy.replacement.as_ref())
        .filter(|replacement| replacement.requested_at >= start)
        .count();
    u32::try_from(used).unwrap()
}

/// Applies one operation to one proxy of the pool, the way the pool does.
fn step(
    pool: &mut [Proxy],
    index: usize,
    op: &Op,
    jitter: f64,
    now: DateTime<Utc>,
    rules: &PoolRules,
) {
    let used = used_this_month(pool, now);
    let proxy = &mut pool[index];
    match op {
        Op::Pass { geo: lookup } => {
            let report = ProbeReport {
                geo: lookup.map(|matches| geo(if matches { "DE" } else { "AT" }, now)),
                ..ok(now)
            };
            apply_probe(proxy, &report, false, jitter, rules);
        }
        Op::Fail { auth, outage } => {
            let report = ProbeReport {
                exit: Err(if *auth {
                    HealthFailure::ProxyAuth
                } else {
                    HealthFailure::Unreachable
                }),
                ..failed(now)
            };
            apply_probe(proxy, &report, *outage, jitter, rules);
        }
        Op::Leak => {
            apply_probe(proxy, &leaked(now), false, jitter, rules);
        }
        Op::PassUnknownHost => {
            let report = ProbeReport {
                leak: None,
                ..ok(now)
            };
            apply_probe(proxy, &report, false, jitter, rules);
        }
        Op::Expire { force } => {
            expire_backoff(proxy, now, *force);
        }
        Op::Reputation { score, hosting } => {
            let report = ReputationReport {
                at: now,
                score: *score,
                usage_type: hosting.then(|| "Data Center/Web Hosting/Transit".to_owned()),
            };
            apply_reputation(proxy, &report, rules);
        }
        Op::Replace { reason, available } => {
            start_replacement(proxy, *reason, used, *available, now, rules);
        }
        Op::Finish => {
            finish_replacement(proxy, now);
        }
        Op::FailReplacement => {
            fail_replacement(proxy, now, jitter, rules);
        }
        Op::Retire => {
            retire(proxy, now);
        }
        Op::ClearLeak => {
            clear_leak(proxy);
        }
        Op::Wait { .. } => {}
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    /// Whatever happens to a pool, in whatever order: every state change is
    /// an edge of the diagram, every backoff ends within its cap, the
    /// monthly budget is never exceeded, and the stored fields stay
    /// consistent with the state.
    #[test]
    fn no_sequence_of_events_breaks_the_state_machine(
        rules in any_rules(),
        size in 1_usize..6,
        ops in proptest::collection::vec((any::<proptest::sample::Index>(), op(), 0.0_f64..1.0), 1..250),
    ) {
        let mut pool: Vec<Proxy> = (0..size).map(|index| proxy(&format!("d-{index}"))).collect();
        let mut now = noon();

        for (pick, op, jitter) in &ops {
            if let Op::Wait { minutes } = op {
                now += Duration::minutes(i64::from(*minutes));
                continue;
            }
            let index = pick.index(size);
            let before = pool[index].clone();

            step(&mut pool, index, op, *jitter, now, &rules);

            let after = &pool[index];
            // No transition outside the diagram.
            prop_assert!(
                is_allowed(before.state, after.state),
                "{} -> {} after {:?}", before.state, after.state, op
            );
            // Retired is final: nothing about a retired proxy changes its state.
            if before.state == ProxyState::Retired {
                prop_assert_eq!(after.state, ProxyState::Retired);
            }
            // A backoff always has an end, within the cap from when it began.
            if after.state == ProxyState::Backoff {
                let until = after.backoff_until;
                prop_assert!(until.is_some(), "backoff without an end after {:?}", op);
                if before.state != ProxyState::Backoff {
                    let wait = until.unwrap() - now;
                    prop_assert!(wait >= Duration::zero(), "{wait}");
                    prop_assert!(wait <= rules.backoff_cap, "{wait} > {}", rules.backoff_cap);
                }
            } else {
                prop_assert_eq!(after.backoff_until, None, "{:?} left a backoff end behind", op);
            }
            // The fields that belong to a state are there.
            if after.state == ProxyState::Replacing {
                prop_assert!(after.replacement.is_some());
            }
            prop_assert_eq!(after.state == ProxyState::Retired, after.retired_at.is_some());
            if after.replacement.is_some() {
                prop_assert!(
                    matches!(after.state, ProxyState::Replacing | ProxyState::Retired),
                    "a replacement record on a {} proxy", after.state
                );
            }
            // The budget is never exceeded, in any month the run passes through.
            prop_assert!(
                used_this_month(&pool, now) <= rules.replacements_per_month,
                "{} replacements in one month with a budget of {}",
                used_this_month(&pool, now), rules.replacements_per_month
            );
            // Only a probe that could compare addresses clears a leak, and
            // only an operator otherwise.
            if before.leak_detected() && !after.leak_detected() {
                prop_assert!(matches!(op, Op::Pass { .. } | Op::ClearLeak), "{:?} cleared a leak", op);
            }
        }
    }

    /// A proxy only becomes due for replacement because it is unreachable
    /// after at least M backoff cycles and at least `replace_after` of
    /// failing without a break, and never while its failure is a leak.
    #[test]
    fn replacement_for_unreachability_always_meets_both_conditions(
        rules in any_rules(),
        ops in proptest::collection::vec((op(), 0.0_f64..1.0), 1..250),
    ) {
        let mut pool = vec![proxy("d-0")];
        let mut now = noon();

        for (op, jitter) in &ops {
            if let Op::Wait { minutes } = op {
                now += Duration::minutes(i64::from(*minutes));
            } else {
                step(&mut pool, 0, op, *jitter, now, &rules);
            }
            let proxy = &pool[0];
            if replacement_due(proxy, now, &rules) == Some(ReplaceReason::Unreachable) {
                prop_assert_eq!(proxy.state, ProxyState::Backoff);
                prop_assert!(proxy.backoff_count >= rules.backoff_cycles);
                let since = proxy.failing_since;
                prop_assert!(since.is_some());
                prop_assert!(now - since.unwrap() >= rules.replace_after);
                let health = proxy.last_health.as_ref().unwrap();
                prop_assert!(!health.reachable);
                prop_assert_ne!(health.failure, Some(HealthFailure::Leak));
            }
        }
    }

    /// The backoff never exceeds its cap and never falls below half of the
    /// uncapped, unjittered value, for any cycle, jitter and rules.
    #[test]
    fn backoff_never_exceeds_its_cap(
        cycle in any::<u32>(),
        jitter in prop_oneof![0.0_f64..1.0, any::<f64>()],
        base_s in 1_i64..100_000,
        extra_s in 0_i64..1_000_000,
    ) {
        let rules = PoolRules {
            backoff_base: Duration::seconds(base_s),
            backoff_cap: Duration::seconds(base_s + extra_s),
            ..PoolRules::default()
        };

        let delay = backoff_delay(cycle, &rules, jitter);

        prop_assert!(delay <= rules.backoff_cap, "{delay} > {}", rules.backoff_cap);
        let ceiling = backoff_delay(cycle, &rules, 1.0);
        prop_assert!(delay <= ceiling);
        prop_assert!(delay >= ceiling / 2 - Duration::milliseconds(1));
        prop_assert!(delay >= rules.backoff_base / 2 - Duration::milliseconds(1));
        // Later cycles never wait less than earlier ones at the same jitter.
        if cycle > 0 {
            prop_assert!(delay >= backoff_delay(cycle - 1, &rules, jitter));
        }
    }
}
