//! The planner under a clock that is only a number: its rules one by one,
//! then whole days of twenty personas (the simulations of the M7
//! acceptance: pacing as the profiles say, bursts, breaks and visits, daily
//! caps in each persona's own zone, the breaker opening and closing, and
//! no two personas in lockstep).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use uuid::Uuid;

use super::{
    Completion, Decision, Dispatch, DomainFacts, Held, PersonaView, Planner, PlannerRules, Refusal,
    RetryPause, Work,
};
use crate::domain::{BreakerState, CountryCode, DomainBreaker, Engine, PersonaDomainState, Render};
use crate::ids::PersonaId;
use crate::outcome::Outcome;
use crate::pacing::{Distribution, IntRange, Lognormal, PacingProfile, PacingProfiles, Uniform};
use crate::persona::{pacing_factor, place};
use crate::policy::{ActiveHoursMode, Cooldowns};
use crate::schedule::breaker::BreakerRules;
use crate::schedule::day::{self, DayRules};
use crate::schedule::politeness::{Courtesy, ServerErrorRules};
use crate::schedule::queue::{Priority, Ticket};
use crate::schedule::retry::{AttemptVerdict, CooldownRules, cooldown_for, cooldown_seed};
use crate::schedule::selection::Network;
use crate::seed::Seed;

const SHIPPED: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../policies/pacing-profiles.yaml"
));

fn human() -> PacingProfile {
    *PacingProfiles::from_yaml(SHIPPED)
        .unwrap()
        .require("human")
        .unwrap()
}

/// `human` without visits, caps and limits: one persona's bare cadence.
fn bare() -> PacingProfile {
    PacingProfile {
        visit_bursts: None,
        visit_break_ms: None,
        daily_cap_per_persona_per_domain: 1_000_000,
        domain_concurrency: 1000,
        domain_rpm: 100_000,
        ..human()
    }
}

/// Pages a few seconds apart with no breaks, for tests about other things.
fn brisk() -> PacingProfile {
    PacingProfile {
        gap_ms: Distribution::Uniform(Uniform {
            min: 2000.0,
            max: 4000.0,
        }),
        burst_pages: IntRange { min: 50, max: 50 },
        burst_break_ms: Distribution::Uniform(Uniform { min: 0.0, max: 0.0 }),
        ..bare()
    }
}

fn day_rules() -> DayRules {
    let time = |hour, minute| NaiveTime::from_hms_opt(hour, minute, 0).unwrap();
    DayRules::new(
        (time(6, 30), time(8, 30)),
        (time(22, 30), time(0, 30)),
        StdDuration::from_mins(20),
    )
}

fn rules() -> PlannerRules {
    PlannerRules {
        max_browser_sessions: 6,
        max_http_inflight: 20,
        max_per_engine: 3,
        visitor_wait: StdDuration::from_secs(5),
        visit_idle: StdDuration::from_mins(10),
        visit_start_spacing: Lognormal {
            median: 700.0,
            p95: 2500.0,
            min: Some(150.0),
            max: None,
        },
        day: day_rules(),
        server_errors: ServerErrorRules {
            after: 3,
            pause: StdDuration::from_secs(30),
            max_pause: StdDuration::from_mins(10),
        },
        breaker_max_pause: StdDuration::from_hours(6),
        breaker_trial_timeout: StdDuration::from_mins(10),
    }
}

/// Noon in Berlin on a summer day: everyone there is awake.
fn noon() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 14, 10, 0, 0).unwrap()
}

fn secs(count: i64) -> Duration {
    Duration::seconds(count)
}

fn millis(count: i64) -> Duration {
    Duration::milliseconds(count)
}

fn id(number: u128) -> PersonaId {
    PersonaId::from_uuid(Uuid::from_u128(number + 1))
}

/// Persona `number`: chromium for even numbers and firefox for odd ones,
/// in Berlin, each in a /24 and an autonomous system of its own.
fn persona(number: u8) -> PersonaView {
    PersonaView {
        id: id(u128::from(number)),
        seed: Seed::from_u64(u64::from(number)),
        engine: if number.is_multiple_of(2) {
            Engine::Chromium
        } else {
            Engine::Firefox
        },
        country: "DE".parse().ok(),
        timezone: "Europe/Berlin".to_owned(),
        ready: true,
        http: true,
        network: Network::of(
            Some(IpAddr::V4(Ipv4Addr::new(198, 51, number, 10))),
            Some(&format!("AS{}", 64_500 + u32::from(number))),
        ),
    }
}

fn personas(count: u8) -> Vec<PersonaView> {
    (0..count).map(persona).collect()
}

fn work(domain: &str) -> Work {
    Work {
        domain: domain.to_owned(),
        priority: Priority::Job,
        render: Render::Browser,
        pin: None,
        exclude: Vec::new(),
        avoid: Vec::new(),
        engines: Engine::ALL.to_vec(),
        countries: None,
        prefer_countries: Vec::new(),
        pacing: brisk(),
        active_hours: ActiveHoursMode::Ignore,
        breaker_threshold: 5,
        breaker_pause: StdDuration::from_mins(30),
        follow_up: false,
        retry_pause: None,
        not_before: None,
        give_up_at: None,
    }
}

fn sync(domain: &str) -> Work {
    Work {
        priority: Priority::Sync,
        ..work(domain)
    }
}

type Refused = (DateTime<Utc>, Ticket, Refusal, Option<DateTime<Utc>>);

/// How an attempt ends, given when.
type Ending = Box<dyn FnMut(&Dispatch, DateTime<Utc>) -> Outcome>;

/// A world for the planner: a clock that is a number, the stored facts as
/// a map, and attempts that take as long and end as the test says.
struct Sim {
    planner: Planner,
    now: DateTime<Utc>,
    facts: HashMap<String, DomainFacts>,
    views: HashMap<PersonaId, PersonaView>,
    flying: Vec<(DateTime<Utc>, Dispatch)>,
    /// Every attempt that left, with the moment it did.
    log: Vec<(DateTime<Utc>, Dispatch)>,
    refused: Vec<Refused>,
    /// Every attempt that ended, with the moment and how.
    ended: Vec<(DateTime<Utc>, Dispatch, Outcome)>,
    takes: Box<dyn FnMut(&Dispatch) -> Duration>,
    ends: Ending,
    breaker: BreakerRules,
}

impl Sim {
    fn new(personas: Vec<PersonaView>) -> Self {
        let mut planner = Planner::new(rules(), Seed::from_u64(99));
        planner.set_personas(personas.clone());
        Self {
            planner,
            now: noon(),
            facts: HashMap::new(),
            views: personas.into_iter().map(|view| (view.id, view)).collect(),
            flying: Vec::new(),
            log: Vec::new(),
            refused: Vec::new(),
            ended: Vec::new(),
            takes: Box::new(|_| millis(300)),
            ends: Box::new(|_, _| Outcome::Ok),
            breaker: BreakerRules {
                threshold: 5,
                pause: StdDuration::from_mins(30),
                max_pause: StdDuration::from_hours(6),
                trial_timeout: StdDuration::from_mins(10),
            },
        }
    }

    fn submit(&mut self, work: Work) -> Ticket {
        self.facts.entry(work.domain.clone()).or_default();
        self.planner.submit(work, self.now)
    }

    fn state(&mut self, persona: PersonaId, domain: &str) -> &mut PersonaDomainState {
        self.facts
            .entry(domain.to_owned())
            .or_default()
            .states
            .entry(persona)
            .or_insert_with(|| PersonaDomainState::new(persona, domain))
    }

    fn land(&mut self, dispatch: &Dispatch) {
        let now = self.now;
        let outcome = (self.ends)(dispatch, now);
        let seed = self.views[&dispatch.persona].seed;
        let cooldown = cooldown_for(
            &Cooldowns::default(),
            &AttemptVerdict::plain(outcome),
            &CooldownRules {
                floor: StdDuration::from_mins(5),
                jitter: 0.1,
            },
            &cooldown_seed(&seed, &dispatch.domain, now),
        )
        .and_then(|cooldown| Duration::from_std(cooldown).ok());
        self.state(dispatch.persona, &dispatch.domain)
            .record_outcome(outcome, now, cooldown);
        let rules = self.breaker;
        let facts = self.facts.entry(dispatch.domain.clone()).or_default();
        if outcome.is_block() {
            facts
                .breaker
                .get_or_insert_with(|| DomainBreaker::closed(dispatch.domain.clone(), now))
                .record_block(now, &rules);
        } else if outcome.is_success()
            && let Some(breaker) = &mut facts.breaker
        {
            breaker.record_success(now);
        }
        let courtesy = match outcome {
            Outcome::ProviderError | Outcome::Timeout => Courtesy::ServerError,
            _ => Courtesy::Answered,
        };
        assert!(self.planner.complete(
            dispatch.grant,
            now,
            Completion {
                courtesy,
                blocked: outcome.is_block(),
            },
        ));
        self.ended.push((now, dispatch.clone(), outcome));
    }

    /// Everything that happens at the current moment: attempts that are
    /// over come back, and the planner plans until it has nothing to say.
    fn settle(&mut self) {
        loop {
            let now = self.now;
            let (due, flying): (Vec<_>, Vec<_>) = std::mem::take(&mut self.flying)
                .into_iter()
                .partition(|(ends, _)| *ends <= now);
            self.flying = flying;
            let landed = !due.is_empty();
            for (_, dispatch) in due {
                self.land(&dispatch);
            }
            let decisions = self.planner.plan(now, &self.facts);
            if decisions.is_empty() && !landed {
                return;
            }
            for decision in decisions {
                match decision {
                    Decision::Dispatch(dispatch) => {
                        let zone = self.views[&dispatch.persona].timezone.clone();
                        let today = place::local_day(now, &zone);
                        self.state(dispatch.persona, &dispatch.domain)
                            .record_request(now, today);
                        if dispatch.trial
                            && let Some(breaker) = self
                                .facts
                                .get_mut(&dispatch.domain)
                                .and_then(|facts| facts.breaker.as_mut())
                        {
                            breaker.begin_trial(now);
                        }
                        let ends = now + (self.takes)(&dispatch);
                        self.flying.push((ends, dispatch.clone()));
                        self.log.push((now, dispatch));
                    }
                    Decision::Refuse {
                        work,
                        reason,
                        retry_at,
                    } => self.refused.push((now, work, reason, retry_at)),
                }
            }
        }
    }

    /// The next moment anything can happen by itself.
    fn next_event(&self) -> Option<DateTime<Utc>> {
        let landing = self.flying.iter().map(|(ends, _)| *ends).min();
        match (self.planner.wake_at(), landing) {
            (Some(wake), Some(landing)) => Some(wake.min(landing)),
            (wake, landing) => wake.or(landing),
        }
    }

    /// Runs until `end`: everything before `end` happens, nothing at it.
    /// `feed` is called at the start and then every `tick`, to add work.
    fn run(&mut self, end: DateTime<Utc>, tick: Duration, mut feed: impl FnMut(&mut Self)) {
        let mut next_tick = self.now;
        loop {
            if self.now >= next_tick {
                feed(self);
                next_tick += tick;
            }
            self.settle();
            let next = self
                .next_event()
                .map_or(next_tick, |event| event.min(next_tick));
            if next >= end {
                self.now = end;
                return;
            }
            // The planner never asks to be woken in the past.
            assert!(next > self.now, "{next} at {}", self.now);
            self.now = next;
        }
    }

    /// Runs until `end` with no new work.
    fn run_until(&mut self, end: DateTime<Utc>) {
        self.run(end, Duration::days(1), |_| {});
    }

    /// Runs until `end`, calling `feed` whenever anything happened and at
    /// least every `tick`: for work that is topped up as it leaves.
    fn run_fed(&mut self, end: DateTime<Utc>, tick: Duration, mut feed: impl FnMut(&mut Self)) {
        let mut next_tick = self.now;
        loop {
            if self.now >= next_tick {
                next_tick += tick;
            }
            feed(self);
            self.settle();
            feed(self);
            self.settle();
            let next = self
                .next_event()
                .map_or(next_tick, |event| event.min(next_tick));
            if next >= end {
                self.now = end;
                return;
            }
            assert!(next > self.now, "{next} at {}", self.now);
            self.now = next;
        }
    }

    fn starts_of(&self, persona: PersonaId) -> Vec<&(DateTime<Utc>, Dispatch)> {
        self.log
            .iter()
            .filter(|(_, dispatch)| dispatch.persona == persona)
            .collect()
    }
}

fn dispatched(decisions: &[Decision]) -> Vec<&Dispatch> {
    decisions
        .iter()
        .filter_map(|decision| match decision {
            Decision::Dispatch(dispatch) => Some(dispatch),
            Decision::Refuse { .. } => None,
        })
        .collect()
}

fn facts_for(domains: &[&str]) -> HashMap<String, DomainFacts> {
    domains
        .iter()
        .map(|domain| ((*domain).to_owned(), DomainFacts::default()))
        .collect()
}

fn percentile(sorted: &[i64], p: f64) -> i64 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index]
}

// --- The rules, one by one ---------------------------------------------

#[test]
fn a_request_leaves_at_once_on_a_persona_that_is_free() {
    let mut planner = Planner::new(rules(), Seed::from_u64(1));
    planner.set_personas(personas(1));
    assert_eq!(planner.seed(), &Seed::from_u64(1));
    assert_eq!(planner.rules(), &rules());
    let ticket = planner.submit(work("example.org"), noon() - secs(2));
    assert_eq!(planner.depth(Priority::Job), 1);
    assert_eq!(planner.depth(Priority::Sync), 0);
    assert_eq!(planner.domains_waiting(), ["example.org"]);

    // What is stored about the domain is not known yet: nothing leaves.
    assert_eq!(planner.plan(noon(), &HashMap::new()), []);
    assert_eq!(planner.depth(Priority::Job), 1);

    let decisions = planner.plan(noon(), &facts_for(&["example.org"]));

    let [Decision::Dispatch(dispatch)] = decisions.as_slice() else {
        panic!("{decisions:?}");
    };
    assert_eq!(dispatch.work, ticket);
    assert_eq!(dispatch.persona, id(0));
    assert_eq!(dispatch.domain, "example.org");
    assert_eq!(dispatch.render, Render::Browser);
    assert_eq!(dispatch.waited, secs(2));
    assert!(dispatch.starts_visit);
    assert_eq!(dispatch.page_of_visit, 1);
    assert!(!dispatch.ends_visit && !dispatch.ends_burst);
    assert!(!dispatch.trial && !dispatch.woken);
    // The next page of the persona: a gap of the profile, at its pace.
    let wait = dispatch.next_slot - noon();
    assert!(wait >= secs(2) && wait < secs(4), "{wait}");
    assert_eq!(planner.depth(Priority::Job), 0);
    assert_eq!(planner.domains_waiting(), Vec::<String>::new());
    assert_eq!(planner.in_flight(Render::Browser), 1);
    assert_eq!(planner.in_flight(Render::Http), 0);
    assert_eq!(planner.visitors("example.org", noon()), 1);
    assert_eq!(planner.visitors("example.net", noon()), 0);
    // Nothing waits: nothing to wake up for.
    assert_eq!(planner.wake_at(), None);
    assert_eq!(planner.plan(noon(), &facts_for(&["example.org"])), []);

    assert!(planner.complete(
        dispatch.grant,
        noon() + secs(1),
        Completion {
            courtesy: Courtesy::Answered,
            blocked: false
        }
    ));
    assert_eq!(planner.in_flight(Render::Browser), 0);
    // A grant is reported once.
    assert!(!planner.complete(
        dispatch.grant,
        noon() + secs(1),
        Completion {
            courtesy: Courtesy::Answered,
            blocked: false
        }
    ));
}

#[test]
fn sync_work_leaves_before_jobs_that_arrived_earlier() {
    let mut sim = Sim::new(personas(1));
    sim.takes = Box::new(|_| secs(1));
    let job = sim.submit(work("example.org"));
    let urgent = sim.submit(sync("example.org"));

    sim.run_until(noon() + secs(30));

    let order: Vec<Ticket> = sim.log.iter().map(|(_, dispatch)| dispatch.work).collect();
    assert_eq!(order, [urgent, job]);
    // The second page is the same persona's, a gap after the first began.
    let (first, second) = (&sim.log[0], &sim.log[1]);
    assert_eq!(second.0, first.1.next_slot);
    assert!(!second.1.starts_visit);
    assert_eq!(second.1.page_of_visit, 2);
    assert_eq!(second.1.waited, second.0 - noon());
}

#[test]
fn a_stop_refuses_everything_until_it_is_lifted() {
    let mut planner = Planner::new(rules(), Seed::from_u64(1));
    planner.set_personas(personas(2));
    assert_eq!(planner.stop(), None);
    let facts = facts_for(&["example.org"]);
    for stop in [Refusal::EgressUnsafe, Refusal::BandwidthExhausted] {
        planner.set_stop(Some(stop));
        assert_eq!(planner.stop(), Some(stop));
        let ticket = planner.submit(sync("example.org"), noon());

        assert_eq!(
            planner.plan(noon(), &facts),
            [Decision::Refuse {
                work: ticket,
                reason: stop,
                retry_at: None
            }]
        );
        assert_eq!(planner.depth(Priority::Sync), 0);
        assert_eq!(planner.reserve(id(0), true), Err(Held::Stopped(stop)));
    }
    // An operator's own self-test is let through the stop.
    let grant = planner.reserve(id(0), false).unwrap();
    planner.complete(
        grant,
        noon(),
        Completion {
            courtesy: Courtesy::NotTheSite,
            blocked: false,
        },
    );

    planner.set_stop(None);
    planner.submit(sync("example.org"), noon());
    assert_eq!(dispatched(&planner.plan(noon(), &facts)).len(), 1);
}

#[test]
fn only_a_persona_that_fits_the_request_serves_it() {
    let mut not_ready = persona(0);
    not_ready.ready = false;
    let mut browser_only = persona(1);
    browser_only.http = false;
    let mut french = persona(2);
    french.country = "FR".parse().ok();
    let mut nowhere = persona(4);
    nowhere.country = None;
    nowhere.http = false;
    let facts = facts_for(&["example.org"]);
    let served_by = |change: fn(&mut Work)| {
        let mut planner = Planner::new(rules(), Seed::from_u64(1));
        planner.set_personas([
            not_ready.clone(),
            browser_only.clone(),
            french.clone(),
            nowhere.clone(),
        ]);
        let mut work = work("example.org");
        change(&mut work);
        planner.submit(work, noon());
        match planner.plan(noon(), &facts).as_slice() {
            [Decision::Dispatch(dispatch)] => Ok(dispatch.persona),
            [
                Decision::Refuse {
                    reason, retry_at, ..
                },
            ] => Err((*reason, *retry_at)),
            other => panic!("{other:?}"),
        }
    };
    let nobody = Err((Refusal::NoPersona, None));

    // The only firefox, the only one in France, the only one with an http
    // path that is ready.
    assert_eq!(
        served_by(|work| work.engines = vec![Engine::Firefox]),
        Ok(id(1))
    );
    assert_eq!(
        served_by(|work| work.countries = Some(vec!["FR".parse().unwrap()])),
        Ok(id(2))
    );
    assert_eq!(served_by(|work| work.render = Render::Http), Ok(id(2)));
    // A persona of unknown country serves only where any country may.
    assert_eq!(
        served_by(|work| {
            work.countries = Some(vec!["DE".parse().unwrap()]);
            work.engines = vec![Engine::Chromium];
        }),
        nobody
    );
    assert_eq!(
        served_by(|work| work.exclude = vec![id(1), id(2)]),
        Ok(id(4))
    );
    // Pinned to one persona: that one or nobody.
    assert_eq!(served_by(|work| work.pin = Some(id(4))), Ok(id(4)));
    assert_eq!(served_by(|work| work.pin = Some(id(0))), nobody);
    assert_eq!(served_by(|work| work.pin = Some(id(77))), nobody);
    assert_eq!(
        served_by(|work| {
            work.pin = Some(id(1));
            work.render = Render::Http;
        }),
        nobody
    );
    assert_eq!(
        served_by(|work| {
            work.render = Render::Http;
            work.exclude = vec![id(2)];
        }),
        nobody
    );
    assert_eq!(served_by(|work| work.engines = Vec::new()), nobody);
}

#[test]
fn a_cooling_persona_is_passed_over_and_waited_for_when_it_is_the_only_one() {
    let mut sim = Sim::new(personas(2));
    let until = noon() + Duration::hours(1);
    sim.state(id(0), "example.org").cooldown_until = Some(until);
    sim.takes = Box::new(|_| Duration::hours(3));
    sim.submit(work("example.org"));
    sim.settle();
    assert_eq!(sim.log[0].1.persona, id(1));

    // The other persona is busy for hours: the next request waits for the
    // cooldown of the first, and is refused when it would rather not wait
    // that long.
    sim.submit(work("example.org"));
    let impatient = sim.submit(Work {
        give_up_at: Some(noon() + Duration::minutes(10)),
        ..work("example.org")
    });
    sim.settle();
    assert_eq!(sim.planner.wake_at(), Some(until));
    assert_eq!(
        sim.refused,
        [(noon(), impatient, Refusal::NoPersona, Some(until))]
    );

    sim.run_until(until + secs(1));
    assert_eq!(sim.log.len(), 2);
    assert_eq!(sim.log[1].0, until);
    assert_eq!(sim.log[1].1.persona, id(0));
    // A cooldown that is over at this very moment does not hold anyone.
    let mut sim = Sim::new(personas(1));
    sim.state(id(0), "example.org").cooldown_until = Some(noon());
    sim.submit(work("example.org"));
    sim.settle();
    assert_eq!(sim.log.len(), 1);
}

#[test]
fn the_daily_cap_counts_the_personas_own_day() {
    let mut tokyo = persona(0);
    tokyo.timezone = "Asia/Tokyo".to_owned();
    let mut sim = Sim::new(vec![tokyo, persona(1)]);
    let capped = PacingProfile {
        daily_cap_per_persona_per_domain: 2,
        ..brisk()
    };
    // Both made two requests at 08:00 UTC today: 17:00 in Tokyo, 10:00 in
    // Berlin. Then it is noon in Berlin and 19:00 in Tokyo: both are at
    // their cap, Tokyo's day ends five hours from now, Berlin's in twelve.
    let earlier = noon() - Duration::hours(2);
    for number in 0..2 {
        let zone = sim.views[&id(number)].timezone.clone();
        let state = sim.state(id(number), "example.org");
        state.record_request(earlier, place::local_day(earlier, &zone));
        state.record_request(earlier, place::local_day(earlier, &zone));
    }
    sim.submit(Work {
        pacing: capped,
        ..work("example.org")
    });
    sim.settle();
    assert_eq!(sim.log, []);
    let tokyo_midnight = place::next_local_midnight(noon(), "Asia/Tokyo");
    assert_eq!(tokyo_midnight, noon() + Duration::hours(5));
    assert_eq!(sim.planner.wake_at(), Some(tokyo_midnight));

    sim.run_until(tokyo_midnight + secs(1));
    assert_eq!(sim.log.len(), 1);
    assert_eq!(sim.log[0].0, tokyo_midnight);
    assert_eq!(sim.log[0].1.persona, id(0));

    // One below the cap is not at it.
    let mut sim = Sim::new(personas(1));
    let state = sim.state(id(0), "example.org");
    state.record_request(earlier, place::local_day(earlier, "Europe/Berlin"));
    sim.submit(Work {
        pacing: capped,
        ..work("example.org")
    });
    sim.settle();
    assert_eq!(sim.log.len(), 1);
}

#[test]
fn an_open_breaker_refuses_at_once_and_lets_one_trial_through_afterwards() {
    let mut sim = Sim::new(personas(3));
    let until = noon() + Duration::minutes(30);
    let mut breaker = DomainBreaker::closed("example.org", noon());
    breaker.state = BreakerState::Open;
    breaker.paused_until = Some(until);
    breaker.trips = 1;
    sim.facts
        .entry("example.org".to_owned())
        .or_default()
        .breaker = Some(breaker);

    let paused = sim.submit(sync("example.org"));
    let elsewhere = sim.submit(sync("example.net"));
    sim.settle();
    assert_eq!(
        sim.refused,
        [(noon(), paused, Refusal::DomainPaused, Some(until))]
    );
    // Another domain is not paused.
    assert_eq!(sim.log.len(), 1);
    assert_eq!(sim.log[0].1.work, elsewhere);
    assert!(!sim.log[0].1.trial);

    // The pause is over: the first request is the trial, the second is
    // still refused, and so is a third while the trial is under way.
    sim.now = until;
    sim.takes = Box::new(|_| secs(20));
    let trial = sim.submit(sync("example.org"));
    let second = sim.submit(sync("example.org"));
    sim.settle();
    let third = sim.submit(sync("example.org"));
    sim.settle();
    assert_eq!(sim.log.len(), 2);
    assert_eq!(sim.log[1].1.work, trial);
    assert!(sim.log[1].1.trial);
    assert_eq!(
        sim.refused[1..],
        [
            (until, second, Refusal::DomainPaused, None),
            (
                until,
                third,
                Refusal::DomainPaused,
                Some(until + Duration::minutes(10))
            ),
        ]
    );

    // The trial comes back fine: the breaker is closed and work flows.
    sim.run_until(until + secs(21));
    let breaker = sim.facts["example.org"].breaker.clone().unwrap();
    assert_eq!(breaker.state, BreakerState::Closed);
    assert_eq!(breaker.trips, 0);
    sim.submit(sync("example.org"));
    sim.settle();
    assert_eq!(sim.log.len(), 3);
    assert!(!sim.log[2].1.trial);
}

#[test]
fn a_domain_never_has_more_in_flight_than_its_profile_allows() {
    let mut sim = Sim::new(personas(4));
    sim.takes = Box::new(|_| secs(30));
    let narrow = PacingProfile {
        domain_concurrency: 1,
        ..brisk()
    };
    let first = sim.submit(Work {
        pacing: narrow,
        ..sync("example.org")
    });
    let second = sim.submit(Work {
        pacing: narrow,
        ..sync("example.org")
    });
    let other = sim.submit(Work {
        pacing: narrow,
        ..sync("example.net")
    });
    sim.settle();

    // The second request of the domain waits for the first to end; the
    // other domain is not held up by it.
    let left: Vec<Ticket> = sim.log.iter().map(|(_, dispatch)| dispatch.work).collect();
    assert_eq!(left, [first]);
    sim.run_until(noon() + secs(29));
    let left: Vec<Ticket> = sim.log.iter().map(|(_, dispatch)| dispatch.work).collect();
    assert_eq!(left, [first, other]);

    sim.run_until(noon() + secs(31));
    assert_eq!(sim.log.len(), 3);
    assert_eq!(sim.log[2].1.work, second);
    assert_eq!(sim.log[2].0, noon() + secs(30));
    // It is the visitor's: the persona of the first page, whose slot has
    // long been open.
    assert_eq!(sim.log[2].1.persona, sim.log[0].1.persona);
}

#[test]
fn a_domain_never_gets_more_requests_a_minute_than_its_profile_allows() {
    let mut sim = Sim::new(personas(6));
    sim.takes = Box::new(|_| millis(200));
    let slow = PacingProfile {
        domain_rpm: 4,
        ..brisk()
    };
    for _ in 0..11 {
        sim.submit(Work {
            pacing: slow,
            ..work("example.org")
        });
    }

    sim.run_until(noon() + secs(200));

    assert_eq!(sim.log.len(), 11);
    let starts: Vec<DateTime<Utc>> = sim.log.iter().map(|(at, _)| *at).collect();
    for (index, start) in starts.iter().enumerate() {
        let in_window = starts[index..]
            .iter()
            .filter(|at| **at - *start < Duration::minutes(1))
            .count();
        assert!(
            in_window <= 4,
            "{in_window} requests in the minute from {start}"
        );
    }
    // The fifth request leaves the moment the first is a minute old.
    assert_eq!(starts[4], starts[0] + Duration::minutes(1));
    assert_eq!(starts[8], starts[4] + Duration::minutes(1));
}

#[test]
fn the_global_caps_hold_work_until_an_attempt_ends() {
    // Six browser sessions in all, three per engine; twenty personas, ten
    // of each engine.
    let mut sim = Sim::new(personas(20));
    sim.takes = Box::new(|_| secs(100));
    for number in 0..9 {
        sim.submit(sync(&format!("site{number}.test")));
    }
    sim.run_until(noon() + secs(60));

    assert_eq!(sim.log.len(), 6);
    assert_eq!(sim.planner.in_flight(Render::Browser), 6);
    let by_engine = |engine| {
        sim.log
            .iter()
            .filter(|(_, dispatch)| sim.views[&dispatch.persona].engine == engine)
            .count()
    };
    assert_eq!(by_engine(Engine::Chromium), 3);
    assert_eq!(by_engine(Engine::Firefox), 3);
    assert_eq!(sim.planner.depth(Priority::Sync), 3);
    // The http path has its own cap and is not held by the browsers.
    for number in 0..25 {
        sim.submit(Work {
            render: Render::Http,
            ..sync(&format!("api{number}.test"))
        });
    }
    sim.run_until(noon() + secs(90));
    assert_eq!(sim.planner.in_flight(Render::Http), 14);
    assert_eq!(sim.planner.in_flight(Render::Browser), 6);

    // When the first browser attempts end, the waiting ones leave.
    sim.run_until(noon() + secs(160));
    assert_eq!(
        sim.log
            .iter()
            .filter(|(_, dispatch)| dispatch.render == Render::Browser)
            .count(),
        9
    );
}

#[test]
fn only_chromium_personas_means_only_three_browser_sessions() {
    let chromium: Vec<PersonaView> = (0..10).map(|number| persona(number * 2)).collect();
    let mut sim = Sim::new(chromium);
    sim.takes = Box::new(|_| secs(100));
    for number in 0..5 {
        sim.submit(sync(&format!("site{number}.test")));
    }

    sim.run_until(noon() + secs(60));

    assert_eq!(sim.log.len(), 3);
    assert_eq!(sim.planner.in_flight(Render::Browser), 3);
}

#[test]
fn a_persona_makes_one_attempt_at_a_time() {
    let mut sim = Sim::new(personas(1));
    sim.takes = Box::new(|_| secs(40));
    sim.submit(sync("one.test"));
    sim.submit(sync("two.test"));
    sim.settle();

    assert_eq!(sim.log.len(), 1);
    // Nothing to wake up for: only the attempt's end changes anything.
    assert_eq!(sim.planner.wake_at(), None);
    sim.run_until(noon() + secs(41));
    assert_eq!(sim.log.len(), 2);
    assert_eq!(sim.log[1].0, noon() + secs(40));
}

#[test]
fn the_work_of_a_domain_stays_with_its_visitor() {
    let mut sim = Sim::new(personas(8));
    sim.takes = Box::new(|_| millis(400));
    // Pages are asked for one at a time, twenty seconds apart.
    sim.run(noon() + secs(400), secs(20), |sim| {
        sim.submit(work("example.org"));
    });

    assert_eq!(sim.log.len(), 20);
    let visitor = sim.log[0].1.persona;
    assert!(
        sim.log
            .iter()
            .all(|(_, dispatch)| dispatch.persona == visitor)
    );
    assert!(sim.log[0].1.starts_visit);
    assert!(
        sim.log[1..]
            .iter()
            .all(|(_, dispatch)| !dispatch.starts_visit)
    );
    let pages: Vec<u32> = sim
        .log
        .iter()
        .map(|(_, dispatch)| dispatch.page_of_visit)
        .collect();
    assert_eq!(pages, (1..=20).collect::<Vec<u32>>());
    assert_eq!(sim.planner.visitors("example.org", sim.now), 1);
    // Ten minutes without a page: the visit is over, the seat is free.
    assert_eq!(
        sim.planner.visitors(
            "example.org",
            sim.log[19].0 + Duration::minutes(10) - secs(1)
        ),
        1
    );
    assert_eq!(
        sim.planner
            .visitors("example.org", sim.log[19].0 + Duration::minutes(10)),
        0
    );
    sim.now += Duration::minutes(11);
    sim.submit(work("example.org"));
    sim.settle();
    assert!(sim.log[20].1.starts_visit);
    assert_eq!(sim.log[20].1.page_of_visit, 1);
}

#[test]
fn a_job_waits_for_the_visitors_gap_and_a_deep_queue_brings_more_visitors() {
    let mut sim = Sim::new(personas(8));
    sim.takes = Box::new(|_| millis(400));
    let three = PacingProfile {
        domain_concurrency: 3,
        ..brisk()
    };
    let job = || Work {
        pacing: three,
        ..work("example.org")
    };
    sim.submit(job());
    sim.settle();
    let first = sim.log[0].clone();

    // One more page: the visitor's, when its gap is over.
    sim.now += millis(500);
    sim.submit(job());
    sim.settle();
    assert_eq!(sim.log.len(), 1);
    assert_eq!(sim.planner.wake_at(), Some(first.1.next_slot));
    sim.run_until(first.1.next_slot + millis(1));
    assert_eq!(sim.log.len(), 2);
    assert_eq!(sim.log[1].1.persona, first.1.persona);

    // Ten pages at once: the visitor takes the first when its gap is
    // over, two more personas join, and nobody else.
    for _ in 0..10 {
        sim.submit(job());
    }
    sim.run_until(sim.now + secs(60));
    let visitors: BTreeSet<PersonaId> = sim.log.iter().map(|(_, d)| d.persona).collect();
    assert_eq!(visitors.len(), 3);
    assert_eq!(sim.log.len(), 12);
    assert_eq!(sim.planner.visitors("example.org", sim.now), 3);
    // Each of them paces itself: no page before the slot of the one before.
    for persona in visitors {
        let own = sim.starts_of(persona);
        for pair in own.windows(2) {
            assert!(pair[1].0 >= pair[0].1.next_slot);
        }
    }
}

#[test]
fn a_sync_request_does_not_wait_for_a_visitors_break() {
    // Every page ends a burst: after each the visitor is on a break of a
    // minute.
    let breaks = PacingProfile {
        burst_pages: IntRange { min: 1, max: 1 },
        burst_break_ms: Distribution::Uniform(Uniform {
            min: 60_000.0,
            max: 60_000.0,
        }),
        domain_concurrency: 1,
        ..brisk()
    };
    let mut sim = Sim::new(personas(4));
    sim.takes = Box::new(|_| millis(400));
    sim.submit(Work {
        pacing: breaks,
        ..work("example.org")
    });
    sim.settle();
    let visitor = sim.log[0].1.persona;
    assert!(sim.log[0].1.ends_burst);
    let back = sim.log[0].1.next_slot;
    assert!(back - noon() > secs(60));

    // A job waits for the visitor although others are free: the domain has
    // one seat.
    sim.now += secs(10);
    sim.submit(Work {
        pacing: breaks,
        ..work("example.org")
    });
    sim.settle();
    assert_eq!(sim.log.len(), 1);
    assert_eq!(sim.planner.wake_at(), Some(back));

    // A synchronous request does not: another persona starts a visit.
    sim.submit(Work {
        pacing: breaks,
        ..sync("example.org")
    });
    sim.settle();
    assert_eq!(sim.log.len(), 2);
    assert_ne!(sim.log[1].1.persona, visitor);
    assert!(sim.log[1].1.starts_visit);

    // A break that is nearly over is waited for, by a sync request too.
    let mut sim = Sim::new(personas(4));
    sim.takes = Box::new(|_| millis(400));
    sim.submit(Work {
        pacing: breaks,
        ..work("example.org")
    });
    sim.settle();
    let back = sim.log[0].1.next_slot;
    sim.now = back - secs(4);
    sim.submit(Work {
        pacing: breaks,
        ..sync("example.org")
    });
    sim.settle();
    assert_eq!(sim.log.len(), 1);
    sim.run_until(back + millis(1));
    assert_eq!(sim.log.len(), 2);
    assert_eq!(sim.log[1].1.persona, sim.log[0].1.persona);
    assert_eq!(sim.log[1].0, back);
}

#[test]
fn a_new_visitor_is_the_one_with_the_best_score() {
    let facts = |sim: &mut Sim| {
        // Persona 0 visited the domain twenty minutes ago; persona 1 an
        // hour and a minute ago, and it went well; persona 2 never did.
        let recent = noon() - Duration::minutes(20);
        sim.state(id(0), "example.org").last_request_at = Some(recent);
        let long_ago = noon() - Duration::minutes(61);
        let state = sim.state(id(1), "example.org");
        state.last_request_at = Some(long_ago);
        state.last_outcome = Some(Outcome::Ok);
    };
    // No visit in the last hour and a success before: persona 1.
    let mut sim = Sim::new(personas(3));
    facts(&mut sim);
    sim.submit(work("example.org"));
    sim.settle();
    assert_eq!(sim.log[0].1.persona, id(1));

    // A country the request prefers outweighs a recent visit: three
    // points against two.
    let mut french = persona(0);
    french.country = "FR".parse().ok();
    let mut sim = Sim::new(vec![french, persona(2)]);
    sim.state(id(0), "example.org").last_request_at = Some(noon() - Duration::minutes(20));
    sim.submit(Work {
        prefer_countries: vec!["FR".parse::<CountryCode>().unwrap()],
        ..work("example.org")
    });
    sim.settle();
    assert_eq!(sim.log[0].1.persona, id(0));

    // A visit exactly an hour ago is no longer in the last hour.
    let mut sim = Sim::new(personas(2));
    sim.state(id(0), "example.org").last_request_at = Some(noon() - Duration::hours(1));
    sim.state(id(0), "example.org").last_outcome = Some(Outcome::Ok);
    sim.state(id(1), "example.org").last_request_at = Some(noon() - Duration::hours(1) + secs(1));
    sim.state(id(1), "example.org").last_outcome = Some(Outcome::Ok);
    sim.submit(work("example.org"));
    sim.settle();
    assert_eq!(sim.log[0].1.persona, id(0));
}

#[test]
fn the_least_loaded_persona_is_preferred_and_equals_take_turns() {
    // Persona 0 worked on other domains in the last minutes; 1 and 2 did
    // not.
    let mut sim = Sim::new(personas(3));
    sim.takes = Box::new(|_| millis(100));
    for number in 0..3 {
        sim.submit(Work {
            pin: Some(id(0)),
            ..work(&format!("warm{number}.test"))
        });
    }
    sim.run_until(noon() + secs(30));
    assert_eq!(sim.log.len(), 3);
    let mut chosen: BTreeMap<PersonaId, u32> = BTreeMap::new();
    for number in 0..40 {
        sim.submit(work(&format!("fresh{number}.test")));
        sim.run_until(sim.now + secs(5));
        *chosen.entry(sim.log.last().unwrap().1.persona).or_default() += 1;
    }
    // The first requests go to the two idle ones; later the loads are
    // alike and everyone takes a turn.
    assert_ne!(sim.log[3].1.persona, id(0));
    assert_ne!(sim.log[4].1.persona, id(0));
    assert_eq!(chosen.len(), 3);
    assert!(chosen.values().all(|count| *count >= 8), "{chosen:?}");

    // Ten minutes later the load is forgotten.
    sim.now += Duration::minutes(11);
    let mut rested: BTreeSet<PersonaId> = BTreeSet::new();
    for number in 0..12 {
        sim.submit(work(&format!("later{number}.test")));
        sim.settle();
        rested.insert(sim.log.last().unwrap().1.persona);
        sim.now += Duration::minutes(11);
    }
    assert_eq!(rested.len(), 3);
}

#[test]
fn a_retry_avoids_the_network_of_the_persona_that_was_blocked() {
    // Personas 0 and 1 share a /24, 2 shares only the autonomous system
    // with them, 3 shares nothing.
    let mut views = personas(4);
    let address = |last| Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, last)));
    views[0].network = Network::of(address(10), Some("AS64500"));
    views[1].network = Network::of(address(20), Some("AS64500"));
    views[2].network = Network::of(
        Some(IpAddr::V4(Ipv4Addr::new(203, 0, 114, 1))),
        Some("AS64500"),
    );
    let blocked_on = views[0].network.clone();
    let retry = |views: &[PersonaView]| {
        let mut sim = Sim::new(views.to_vec());
        sim.submit(Work {
            exclude: vec![id(0)],
            avoid: vec![blocked_on.clone()],
            ..work("example.org")
        });
        sim.settle();
        sim.log[0].1.persona
    };

    assert_eq!(retry(&views), id(3));
    // Without the stranger: the one outside the /24.
    assert_eq!(retry(&views[..3]), id(2));
    // Only the neighbour is left: it serves.
    assert_eq!(retry(&views[..2]), id(1));
}

#[test]
fn starts_that_no_pacing_timed_are_kept_apart() {
    let mut sim = Sim::new(personas(20));
    sim.takes = Box::new(|_| secs(2));
    // Twenty requests for twenty domains in the same instant, on the http
    // path, where nothing else limits them.
    for number in 0..20 {
        sim.submit(Work {
            render: Render::Http,
            ..sync(&format!("site{number}.test"))
        });
    }
    sim.run_until(noon() + secs(120));

    assert_eq!(sim.log.len(), 20);
    let starts: Vec<DateTime<Utc>> = sim.log.iter().map(|(at, _)| *at).collect();
    assert_eq!(starts[0], noon());
    let gaps: Vec<i64> = starts
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).num_milliseconds())
        .collect();
    // Never closer than the shortest pause at the fastest persona's pace,
    // and not all the same.
    assert!(gaps.iter().all(|gap| *gap >= 120), "{gaps:?}");
    assert!(gaps.iter().collect::<BTreeSet<_>>().len() >= 18, "{gaps:?}");
    let mut sorted = gaps.clone();
    sorted.sort_unstable();
    let median = percentile(&sorted, 0.5);
    assert!((450..1000).contains(&median), "{median}");
    // Everyone had left within half a minute.
    assert!(starts[19] - starts[0] < secs(40));
    let distinct: BTreeSet<PersonaId> = sim.log.iter().map(|(_, d)| d.persona).collect();
    assert_eq!(distinct.len(), 20);
}

#[test]
fn a_page_timed_by_the_personas_own_slot_is_not_delayed_by_others() {
    let mut sim = Sim::new(personas(2));
    sim.takes = Box::new(|_| millis(200));
    // Persona 0 visits one domain, page after page.
    for _ in 0..3 {
        sim.submit(Work {
            pin: Some(id(0)),
            ..work("paced.test")
        });
    }
    sim.settle();
    let slot = sim.log[0].1.next_slot;
    // A moment before its second page is due, persona 1 starts somewhere
    // else.
    sim.run_until(slot - millis(50));
    sim.submit(Work {
        pin: Some(id(1)),
        ..work("other.test")
    });
    sim.run_until(slot + secs(20));

    let second = sim.starts_of(id(0))[1];
    // The second page leaves on its slot, 50 ms after the other persona's
    // start, although that is closer than any drawn pause.
    assert_eq!(second.0, slot);
    assert_eq!(sim.starts_of(id(1))[0].0, slot - millis(50));
}

#[test]
fn at_night_a_job_waits_for_the_morning_and_a_sync_request_wakes_one_persona_for_one_page() {
    // 03:00 in Berlin.
    let night = Utc.with_ymd_and_hms(2026, 7, 14, 1, 0, 0).unwrap();
    let rules = day_rules();
    let prefer = |priority| Work {
        priority,
        active_hours: ActiveHoursMode::Prefer,
        ..work("example.org")
    };
    let mut sim = Sim::new(personas(3));
    sim.now = night;
    assert!(
        sim.views
            .values()
            .all(|view| !day::awake(&view.seed, night, &view.timezone, &rules))
    );

    // The job waits until the first of them gets up.
    sim.submit(prefer(Priority::Job));
    sim.settle();
    assert_eq!(sim.log, []);
    let first_up = sim
        .views
        .values()
        .map(|view| day::next_up(&view.seed, night, &view.timezone, &rules))
        .min()
        .unwrap();
    assert_eq!(sim.planner.wake_at(), Some(first_up));

    // A synchronous request is answered: one persona is woken, reads the
    // one page, and is not visiting afterwards.
    sim.submit(prefer(Priority::Sync));
    sim.settle();
    assert_eq!(sim.log.len(), 1);
    assert!(sim.log[0].1.woken);
    assert!(sim.log[0].1.ends_visit);
    assert_eq!(sim.planner.visitors("example.org", night + secs(1)), 0);

    // In the morning the job leaves, on the persona that is up.
    sim.run_until(first_up + secs(1));
    assert_eq!(sim.log.len(), 2);
    assert_eq!(sim.log[1].0, first_up);
    assert!(!sim.log[1].1.woken);
    let early_bird = &sim.views[&sim.log[1].1.persona];
    assert!(day::awake(
        &early_bird.seed,
        first_up,
        &early_bird.timezone,
        &rules
    ));
}

#[test]
fn enforced_hours_hold_sync_requests_too_and_ignored_hours_hold_nothing() {
    let night = Utc.with_ymd_and_hms(2026, 7, 14, 1, 0, 0).unwrap();
    let rules = day_rules();
    let at_night = |mode, give_up_at| {
        let mut sim = Sim::new(personas(3));
        sim.now = night;
        sim.submit(Work {
            active_hours: mode,
            give_up_at,
            ..sync("example.org")
        });
        sim.settle();
        sim
    };
    let first_up = personas(3)
        .iter()
        .map(|view| day::next_up(&view.seed, night, &view.timezone, &rules))
        .min()
        .unwrap();

    let enforced = at_night(ActiveHoursMode::Enforce, None);
    assert_eq!(enforced.log, []);
    assert_eq!(enforced.planner.wake_at(), Some(first_up));
    // With three minutes of patience it is refused, with the time to come
    // back at.
    let refused = at_night(ActiveHoursMode::Enforce, Some(night + Duration::minutes(3)));
    assert_eq!(refused.refused.len(), 1);
    assert_eq!(
        (refused.refused[0].2, refused.refused[0].3),
        (Refusal::NoPersona, Some(first_up))
    );

    let ignored = at_night(ActiveHoursMode::Ignore, None);
    assert_eq!(ignored.log.len(), 1);
    assert!(!ignored.log[0].1.woken);
    assert!(!ignored.log[0].1.ends_visit);

    // A persona that is awake is taken before a sleeper, in Tokyo it is
    // ten in the morning.
    let mut tokyo = persona(2);
    tokyo.timezone = "Asia/Tokyo".to_owned();
    let mut sim = Sim::new(vec![persona(0), persona(1), tokyo]);
    sim.now = night;
    for _ in 0..10 {
        sim.submit(Work {
            active_hours: ActiveHoursMode::Prefer,
            ..sync(&format!("site{}.test", sim.log.len()))
        });
        sim.run_until(sim.now + secs(5));
    }
    assert_eq!(sim.log.len(), 10);
    assert!(
        sim.log
            .iter()
            .all(|(_, dispatch)| dispatch.persona == id(2))
    );
    assert!(sim.log.iter().all(|(_, dispatch)| !dispatch.woken));
}

#[test]
fn a_personas_own_second_try_does_not_wait_for_its_slot_or_its_cooldown() {
    let mut sim = Sim::new(personas(2));
    sim.takes = Box::new(|_| millis(300));
    sim.ends = Box::new(|dispatch, _| {
        if dispatch.render == Render::Http {
            Outcome::ChallengeFailed
        } else {
            Outcome::Ok
        }
    });
    sim.submit(Work {
        render: Render::Http,
        ..sync("example.org")
    });
    sim.run_until(noon() + millis(301));
    let http = sim.log[0].clone();
    // The simulated store cooled the persona for the challenge; its own
    // escalation is not held by that, nor by the gap that runs.
    assert!(sim.facts["example.org"].states[&http.1.persona].is_cooling(sim.now));
    assert!(http.1.next_slot > sim.now);

    sim.submit(Work {
        pin: Some(http.1.persona),
        follow_up: true,
        ..sync("example.org")
    });
    sim.settle();

    assert_eq!(sim.log.len(), 2);
    let browser = &sim.log[1].1;
    assert_eq!(browser.persona, http.1.persona);
    assert_eq!(browser.render, Render::Browser);
    // Still the same page of the same visit, and the slot did not move.
    assert!(!browser.starts_visit);
    assert_eq!(browser.page_of_visit, 1);
    assert_eq!(browser.next_slot, http.1.next_slot);
    // Without the mark the same request waits for the cooldown.
    let mut sim = Sim::new(personas(1));
    sim.state(id(0), "example.org").cooldown_until = Some(noon() + Duration::hours(1));
    sim.submit(Work {
        pin: Some(id(0)),
        ..sync("example.org")
    });
    sim.settle();
    assert_eq!(sim.log, []);
}

#[test]
fn a_retry_is_not_made_before_its_pause_is_over() {
    let mut sim = Sim::new(personas(2));
    let after = noon() + secs(6);
    sim.submit(Work {
        not_before: Some(after),
        ..sync("example.org")
    });
    sim.settle();
    assert_eq!(sim.log, []);
    assert_eq!(sim.planner.wake_at(), Some(after));

    sim.run_until(after + secs(1));
    assert_eq!(sim.log.len(), 1);
    assert_eq!(sim.log[0].0, after);
    assert_eq!(sim.log[0].1.waited, secs(6));
}

#[test]
fn a_retry_waits_a_drawn_pause_at_the_pace_of_the_persona_that_makes_it() {
    let pause = Lognormal {
        median: 6000.0,
        p95: 20_000.0,
        min: Some(2000.0),
        max: None,
    };
    let seed = Seed::from_u64(41);
    let retried_by = |number: u8| {
        let mut sim = Sim::new(vec![persona(number)]);
        sim.submit(Work {
            retry_pause: Some(RetryPause { pause, seed }),
            ..sync("example.org")
        });
        sim.settle();
        assert_eq!(sim.log, []);
        let wake = sim.planner.wake_at().unwrap();
        sim.run_until(wake + secs(1));
        assert_eq!(sim.log.len(), 1);
        assert_eq!(sim.log[0].0, wake);
        // The breaker's rules travel with the attempt.
        assert_eq!(sim.log[0].1.breaker.threshold, 5);
        assert_eq!(sim.log[0].1.breaker.pause, StdDuration::from_mins(30));
        assert_eq!(sim.log[0].1.breaker.max_pause, StdDuration::from_hours(6));
        assert_eq!(
            sim.log[0].1.breaker.trial_timeout,
            StdDuration::from_mins(10)
        );
        (wake - noon(), pacing_factor(&persona(number).seed))
    };

    let (first, first_factor) = retried_by(0);
    let (second, second_factor) = retried_by(1);
    // Never under two seconds, and the same draw is longer for the slower
    // of two personas.
    assert!(first > secs(2) && second > secs(2), "{first} {second}");
    assert_ne!(first, second);
    assert_eq!(first > second, first_factor > second_factor);
    assert_eq!(
        first,
        Duration::from_std(crate::schedule::retry::retry_pause(
            &pause,
            first_factor,
            &seed
        ))
        .unwrap()
    );
}

#[test]
fn three_server_errors_in_a_row_pause_the_domain_and_a_good_answer_ends_the_pause() {
    let mut sim = Sim::new(personas(8));
    sim.takes = Box::new(|_| secs(1));
    sim.ends = Box::new(|_, at| {
        if at < noon() + secs(60) {
            Outcome::ProviderError
        } else {
            Outcome::Ok
        }
    });
    // A request every five seconds.
    sim.run(noon() + secs(200), secs(5), |sim| {
        sim.submit(sync("example.org"));
    });

    let starts: Vec<i64> = sim
        .log
        .iter()
        .map(|(at, _)| (*at - noon()).num_seconds())
        .collect();
    // Three errors (at 1, 6 and 11 s) pause the domain for 30 s; what
    // queued up meanwhile leaves when the pause is over; the fourth error
    // pauses it for 60 s more; then the site answers and nothing holds
    // any more.
    assert_eq!(&starts[..3], [0, 5, 10]);
    assert_eq!(starts[3], 41);
    assert!(starts[4..].iter().all(|at| *at >= 41));
    assert_eq!(sim.planner.server_error_pause("never.test", noon()), None);
    // No persona was cooled and no breaker counted anything.
    assert!(sim.facts["example.org"].breaker.is_none());
    assert!(
        sim.facts["example.org"]
            .states
            .values()
            .all(|state| state.cooldown_until.is_none())
    );
    let last = sim.log.last().unwrap().0;
    assert!(last > noon() + secs(180));
    assert_eq!(sim.planner.server_error_pause("example.org", last), None);
}

#[test]
fn work_for_a_domain_that_is_down_waits_for_the_end_of_its_pause() {
    let mut sim = Sim::new(personas(1));
    sim.takes = Box::new(|_| secs(1));
    sim.ends = Box::new(|_, _| Outcome::Timeout);
    for _ in 0..4 {
        sim.submit(sync("example.org"));
    }
    // Three timeouts in a row, each a few seconds after the one before.
    sim.run_until(noon() + secs(20));
    assert_eq!(sim.log.len(), 3);
    let third = sim.ended[2].0;

    assert_eq!(
        sim.planner.server_error_pause("example.org", sim.now),
        Some(third + secs(30))
    );
    assert_eq!(sim.planner.wake_at(), Some(third + secs(30)));
    assert_eq!(
        sim.planner
            .server_error_pause("example.org", third + secs(30)),
        None
    );
    sim.run_until(third + secs(32));
    assert_eq!(sim.log.len(), 4);
    assert_eq!(sim.log[3].0, third + secs(30));

    // A request that would rather not wait that long is refused with the
    // time to come back at.
    let fourth = sim.ended[3].0;
    let impatient = sim.submit(Work {
        give_up_at: Some(sim.now + secs(10)),
        ..sync("example.org")
    });
    sim.settle();
    assert_eq!(
        sim.refused,
        [(
            sim.now,
            impatient,
            Refusal::NoPersona,
            Some(fourth + secs(60))
        )]
    );
}

#[test]
fn a_block_ends_the_visit_of_the_persona_that_was_blocked() {
    let mut sim = Sim::new(personas(3));
    sim.takes = Box::new(|_| secs(1));
    sim.ends = Box::new(|dispatch, _| {
        if dispatch.page_of_visit == 2 && dispatch.persona == id(0) {
            Outcome::Forbidden
        } else {
            Outcome::Ok
        }
    });
    for _ in 0..2 {
        sim.submit(Work {
            pin: Some(id(0)),
            ..work("example.org")
        });
    }
    sim.run_until(noon() + secs(20));
    assert_eq!(sim.log.len(), 2);
    assert_eq!(sim.ended[1].2, Outcome::Forbidden);

    // It is not visiting any more, and it cools: the next page is another
    // persona's, at once.
    assert_eq!(sim.planner.visitors("example.org", sim.now), 0);
    sim.submit(work("example.org"));
    sim.settle();
    assert_eq!(sim.log.len(), 3);
    assert_ne!(sim.log[2].1.persona, id(0));
    assert!(sim.log[2].1.starts_visit);
}

#[test]
fn the_managers_own_work_takes_a_free_session_and_comes_last() {
    let mut planner = Planner::new(rules(), Seed::from_u64(1));
    planner.set_personas(personas(8));
    let facts = facts_for(&["a.test", "b.test", "c.test", "d.test", "e.test", "f.test"]);
    let done = Completion {
        courtesy: Courtesy::NotTheSite,
        blocked: false,
    };

    assert_eq!(planner.reserve(id(99), true), Err(Held::UnknownPersona));
    let own = planner.reserve(id(0), true).unwrap();
    assert_eq!(planner.in_flight(Render::Browser), 1);
    // The persona is taken: for the manager and for real work alike.
    assert_eq!(planner.reserve(id(0), true), Err(Held::PersonaBusy));
    planner.submit(
        Work {
            pin: Some(id(0)),
            ..sync("a.test")
        },
        noon(),
    );
    assert_eq!(planner.plan(noon(), &facts), []);
    assert!(planner.complete(own, noon(), done));
    assert_eq!(planner.in_flight(Render::Browser), 0);
    assert_eq!(dispatched(&planner.plan(noon(), &facts)).len(), 1);

    // Chromium has three sessions: persona 0 holds one, 2 and 4 take the
    // others, 6 finds none.
    let second = planner.reserve(id(2), true).unwrap();
    planner.reserve(id(4), true).unwrap();
    assert_eq!(planner.reserve(id(6), true), Err(Held::NoSlot));
    // Firefox has room: but once work waits for a browser, the manager
    // leaves the last session of an engine to it.
    planner.reserve(id(1), true).unwrap();
    planner.submit(sync("b.test"), noon() + secs(30));
    assert_eq!(planner.depth(Priority::Sync), 1);
    planner.reserve(id(3), true).unwrap();
    assert_eq!(planner.reserve(id(5), true), Err(Held::NoSlot));
    assert_eq!(planner.in_flight(Render::Browser), 5);
    // The waiting request takes the session that was left.
    let decisions = planner.plan(noon() + secs(30), &facts);
    assert_eq!(dispatched(&decisions).len(), 1);
    assert_eq!(planner.in_flight(Render::Browser), 6);
    assert_eq!(planner.reserve(id(6), true), Err(Held::NoSlot));
    assert!(planner.complete(second, noon(), done));
    assert_eq!(planner.in_flight(Render::Browser), 5);
}

#[test]
fn work_that_is_cancelled_never_leaves() {
    let mut planner = Planner::new(rules(), Seed::from_u64(1));
    planner.set_personas(personas(1));
    let ticket = planner.submit(sync("example.org"), noon());

    assert!(planner.cancel(ticket));
    assert!(!planner.cancel(ticket));
    assert_eq!(planner.depth(Priority::Sync), 0);
    assert_eq!(planner.plan(noon(), &facts_for(&["example.org"])), []);
}

#[test]
fn a_persona_that_is_gone_is_forgotten_with_its_pacing() {
    let mut sim = Sim::new(personas(2));
    sim.takes = Box::new(|_| secs(1));
    sim.submit(Work {
        pin: Some(id(0)),
        ..work("example.org")
    });
    sim.run_until(noon() + secs(2));
    assert_eq!(sim.planner.visitors("example.org", sim.now), 1);

    sim.planner.set_personas([persona(1)]);

    assert_eq!(sim.planner.visitors("example.org", sim.now), 0);
    sim.submit(Work {
        pin: Some(id(0)),
        ..sync("example.org")
    });
    sim.settle();
    assert_eq!(sim.refused.len(), 1);
    assert_eq!(sim.refused[0].2, Refusal::NoPersona);
    // The other persona still serves.
    sim.submit(sync("example.org"));
    sim.settle();
    assert_eq!(sim.log.last().unwrap().1.persona, id(1));
}

#[test]
fn a_change_of_pacing_starts_the_personas_cadence_anew() {
    let mut sim = Sim::new(personas(1));
    sim.takes = Box::new(|_| millis(100));
    sim.submit(work("example.org"));
    sim.settle();
    let first = sim.log[0].1.clone();

    sim.run_until(first.next_slot);
    let slow = PacingProfile {
        gap_ms: Distribution::Uniform(Uniform {
            min: 50_000.0,
            max: 50_000.0,
        }),
        ..brisk()
    };
    sim.submit(Work {
        pacing: slow,
        ..work("example.org")
    });
    sim.settle();

    let second = &sim.log[1].1;
    assert_ne!(second.pacing_seed, first.pacing_seed);
    assert_eq!(second.next_slot - sim.log[1].0, secs(50));
}

#[test]
fn refusal_names_are_the_reasons_callers_get() {
    crate::macros::test_support::assert_string_enum(
        Refusal::ALL,
        Refusal::as_str,
        &[
            "egress_unsafe",
            "bandwidth_exhausted",
            "domain_paused",
            "no_persona",
            "shutting_down",
        ],
    );
}

// --- Whole days ----------------------------------------------------------

/// How long a browser attempt takes: between 12 and 55 seconds, drawn from
/// the attempt itself so that a run replays.
fn browsing(dispatch: &Dispatch) -> Duration {
    let seed = Seed::derive(
        "sim-duration",
        &[
            dispatch.pacing_seed.as_bytes(),
            &dispatch.next_slot.timestamp_millis().to_le_bytes(),
        ],
    );
    #[allow(clippy::cast_possible_truncation)]
    let millis_taken = seed.rng().uniform(12_000.0, 55_000.0) as i64;
    millis(millis_taken)
}

#[test]
fn on_the_http_path_the_pace_is_the_profiles_at_the_personas_own_factor() {
    // One persona, always work waiting, fetches that take 300 ms: the
    // time from one request to the next is exactly the wait that was
    // drawn, and the waits are the profile's.
    let view = persona(0);
    let factor = pacing_factor(&view.seed);
    let mut sim = Sim::new(vec![view]);
    let job = || Work {
        render: Render::Http,
        pacing: bare(),
        ..work("example.org")
    };
    sim.run_fed(noon() + Duration::hours(48), Duration::hours(1), |sim| {
        if sim.planner.depth(Priority::Job) == 0 {
            sim.submit(job());
        }
    });

    assert!(sim.log.len() > 8000, "{}", sim.log.len());
    let mut inside = Vec::new();
    let mut breaks = Vec::new();
    let mut burst = 0_u32;
    let mut bursts = BTreeSet::new();
    for pair in sim.log.windows(2) {
        let drawn = pair[0].1.next_slot - pair[0].0;
        // Start to start is the wait that was drawn, to the millisecond.
        assert_eq!(pair[1].0 - pair[0].0, drawn);
        burst += 1;
        if pair[0].1.ends_burst {
            breaks.push(drawn.num_milliseconds());
            bursts.insert(burst);
            burst = 0;
        } else {
            inside.push(drawn.num_milliseconds());
        }
    }
    inside.sort_unstable();
    breaks.sort_unstable();

    // Inside a burst: the shifted log-normal of `human`, median 4.5 s and
    // p95 15 s at this persona's pace, never at or below the floor.
    assert!(inside[0] > 1500, "{}", inside[0]);
    let within = |value: i64, target: f64, tolerance: f64| {
        #[allow(clippy::cast_precision_loss)]
        let value = value as f64;
        (value - target).abs() < target * tolerance
    };
    let median = percentile(&inside, 0.5);
    let p95 = percentile(&inside, 0.95);
    assert!(
        within(median, 4500.0 * factor, 0.04),
        "{median} at {factor}"
    );
    assert!(within(p95, 15_000.0 * factor, 0.07), "{p95} at {factor}");
    // Bursts of 4 to 14 pages, every length of them, each ended by a
    // break of about a minute on top of the gap.
    assert_eq!(bursts, (4..=14).collect::<BTreeSet<u32>>());
    let break_median = percentile(&breaks, 0.5);
    assert!(
        within(break_median, 66_500.0 * factor, 0.12),
        "{break_median} at {factor}"
    );
    assert!(breaks[0] > 1500);
    // No visits in this profile: the persona never leaves.
    assert!(sim.log.iter().all(|(_, dispatch)| !dispatch.ends_visit));
    assert_eq!(
        sim.log
            .iter()
            .filter(|(_, dispatch)| dispatch.starts_visit)
            .count(),
        1
    );
}

#[test]
fn on_the_browser_path_the_gap_is_a_floor_under_the_time_on_the_page() {
    let mut sim = Sim::new(vec![persona(0)]);
    sim.takes = Box::new(|_| secs(35));
    sim.run_fed(noon() + Duration::hours(12), Duration::hours(1), |sim| {
        if sim.planner.depth(Priority::Job) == 0 {
            sim.submit(Work {
                pacing: bare(),
                ..work("example.org")
            });
        }
    });

    assert!(sim.log.len() > 800, "{}", sim.log.len());
    let mut floor_held = 0;
    for pair in sim.log.windows(2) {
        let drawn = pair[0].1.next_slot - pair[0].0;
        let between = pair[1].0 - pair[0].0;
        // The next page starts when the page is done and the gap is over,
        // whichever is later; the page is dwelt on once.
        assert_eq!(between, drawn.max(secs(35)));
        if drawn > secs(35) {
            floor_held += 1;
        }
    }
    // The gap is usually over long before the page is: it only holds
    // after a burst, and now and then inside one.
    #[allow(clippy::cast_precision_loss)]
    let share = f64::from(floor_held) / sim.log.len() as f64;
    assert!((0.05..0.2).contains(&share), "{share}");
}

/// Twenty personas, one domain with work always waiting, the shipped
/// `human` profile with its visits, from nine in the morning for thirteen
/// hours.
fn a_day_on_one_domain() -> Sim {
    let mut sim = Sim::new(personas(20));
    sim.now = Utc.with_ymd_and_hms(2026, 7, 14, 7, 0, 0).unwrap();
    sim.takes = Box::new(browsing);
    let end = sim.now + Duration::hours(13);
    sim.run_fed(end, Duration::hours(1), |sim| {
        while sim.planner.depth(Priority::Job) < 6 {
            sim.submit(Work {
                pacing: human(),
                active_hours: ActiveHoursMode::Prefer,
                ..work("example.org")
            });
        }
    });
    sim
}

#[test]
fn a_day_on_one_domain_is_visits_of_a_few_personas_at_a_time() {
    let sim = a_day_on_one_domain();
    let start = Utc.with_ymd_and_hms(2026, 7, 14, 7, 0, 0).unwrap();
    assert!(sim.log.len() > 1500, "{}", sim.log.len());

    // The domain's limits: three in flight, twenty a minute.
    let mut events: Vec<(DateTime<Utc>, i32)> = Vec::new();
    for (at, dispatch) in &sim.log {
        events.push((*at, 1));
        events.push((*at + browsing(dispatch), -1));
    }
    events.sort();
    let mut in_flight = 0;
    for (_, change) in events {
        in_flight += change;
        assert!(in_flight <= 3);
    }
    for (index, (at, _)) in sim.log.iter().enumerate() {
        let in_minute = sim.log[index..]
            .iter()
            .take_while(|(later, _)| *later - *at < Duration::minutes(1))
            .count();
        assert!(in_minute <= 20);
    }

    // Few addresses at a time, and over the day most of the pool.
    let mut windows: BTreeMap<i64, BTreeSet<PersonaId>> = BTreeMap::new();
    for (at, dispatch) in &sim.log {
        windows
            .entry((*at - start).num_minutes() / 10)
            .or_default()
            .insert(dispatch.persona);
    }
    #[allow(clippy::cast_precision_loss)]
    let active = windows.values().map(BTreeSet::len).sum::<usize>() as f64 / windows.len() as f64;
    assert!(active < 5.5, "{active} addresses per ten minutes");
    assert!(windows.values().all(|personas| personas.len() <= 8));
    let everyone: BTreeSet<PersonaId> = sim.log.iter().map(|(_, d)| d.persona).collect();
    assert!(everyone.len() >= 15, "{}", everyone.len());

    // Visits: bursts with breaks, ended by the long break.
    let mut visit_pages = Vec::new();
    let mut visit_bursts = Vec::new();
    for persona in &everyone {
        let own = sim.starts_of(*persona);
        // Every page waits for the slot the one before left it, and the
        // persona stays under its cap.
        assert!(own.len() <= 300);
        let (mut pages, mut bursts) = (0_u32, 0_u32);
        for (index, (at, dispatch)) in own.iter().enumerate() {
            if dispatch.starts_visit {
                assert_eq!(dispatch.page_of_visit, 1);
                (pages, bursts) = (0, 0);
            }
            pages += 1;
            assert_eq!(dispatch.page_of_visit, pages);
            if dispatch.ends_burst {
                bursts += 1;
            }
            if let Some((next_at, next)) = own.get(index + 1) {
                assert!(*next_at >= dispatch.next_slot);
                if dispatch.ends_visit {
                    // Away for the long break: more than five minutes even
                    // at the short end, and the next page starts a visit.
                    assert!(*next_at - *at > Duration::minutes(5));
                    assert!(next.starts_visit);
                } else if dispatch.ends_burst {
                    assert!(*next_at - *at > secs(8));
                }
            }
            if dispatch.ends_visit {
                assert!(dispatch.ends_burst);
                visit_pages.push(i64::from(pages));
                visit_bursts.push(bursts);
            }
        }
    }
    assert!(visit_pages.len() > 40, "{} visits", visit_pages.len());
    visit_pages.sort_unstable();
    let median = percentile(&visit_pages, 0.5);
    assert!((14..=42).contains(&median), "{median} pages per visit");
    assert!(*visit_pages.last().unwrap() <= 70);
    // A visit that ran its course has two to five bursts; one that sat
    // idle for ten minutes may have ended early, never late.
    assert!(visit_bursts.iter().all(|bursts| (1..=5).contains(bursts)));
    assert!(visit_bursts.iter().filter(|bursts| **bursts >= 2).count() > visit_bursts.len() / 2);
}

#[test]
fn daily_caps_are_kept_in_each_personas_own_zone() {
    let mut tokyo = persona(0);
    tokyo.timezone = "Asia/Tokyo".to_owned();
    let mut honolulu = persona(2);
    honolulu.timezone = "Pacific/Honolulu".to_owned();
    let views = vec![tokyo, persona(1), honolulu];
    let mut sim = Sim::new(views.clone());
    sim.takes = Box::new(|_| millis(500));
    let capped = PacingProfile {
        daily_cap_per_persona_per_domain: 25,
        domain_concurrency: 3,
        ..brisk()
    };
    // From 12:00 UTC for two days: 21:00 in Tokyo, 14:00 in Berlin, 02:00
    // in Honolulu.
    sim.now = Utc.with_ymd_and_hms(2026, 7, 14, 12, 0, 0).unwrap();
    let begin = sim.now;
    sim.run_fed(begin + Duration::hours(48), Duration::hours(1), |sim| {
        while sim.planner.depth(Priority::Job) < 4 {
            sim.submit(Work {
                pacing: capped,
                ..work("example.org")
            });
        }
    });

    for view in &views {
        let mut per_day: BTreeMap<chrono::NaiveDate, Vec<DateTime<Utc>>> = BTreeMap::new();
        for (at, _) in sim.starts_of(view.id) {
            per_day
                .entry(place::local_day(*at, &view.timezone))
                .or_default()
                .push(*at);
        }
        // Three local days are touched by the two days, and on each the
        // persona fetched exactly its cap: never more, and with work
        // always waiting never less.
        assert_eq!(per_day.len(), 3, "{}", view.timezone);
        for (day, starts) in &per_day {
            assert_eq!(starts.len(), 25, "{} on {day}", view.timezone);
        }
        // A new day's first page leaves at the persona's own midnight.
        for (day, starts) in per_day.iter().skip(1) {
            let midnight = place::instant_of(day.and_time(NaiveTime::MIN), &view.timezone);
            assert!(
                starts[0] >= midnight && starts[0] - midnight < secs(40),
                "{} on {day}: {}",
                view.timezone,
                starts[0] - midnight
            );
        }
    }
    // The three midnights are hours apart: 15:00, 22:00 and 10:00 UTC.
    let midnights: BTreeSet<u32> = views
        .iter()
        .map(|view| {
            use chrono::Timelike;
            place::next_local_midnight(begin, &view.timezone).hour()
        })
        .collect();
    assert_eq!(midnights, BTreeSet::from([15, 22, 10]));
}

#[test]
fn the_breaker_opens_on_blocks_and_closes_on_a_trial_that_succeeds() {
    let mut sim = Sim::new(personas(12));
    sim.takes = Box::new(|_| secs(5));
    // The site blocks everyone for fifty minutes, then answers again.
    let friendly_from = noon() + Duration::minutes(50);
    sim.ends = Box::new(move |_, at| {
        if at < friendly_from {
            Outcome::Forbidden
        } else {
            Outcome::Ok
        }
    });
    // A request every twenty seconds for three hours.
    sim.run(noon() + Duration::hours(3), secs(20), |sim| {
        sim.submit(sync("example.org"));
    });

    // Five blocks, each on another persona, opened it.
    let blocks: Vec<&(DateTime<Utc>, Dispatch, Outcome)> = sim
        .ended
        .iter()
        .filter(|(_, _, outcome)| *outcome == Outcome::Forbidden)
        .collect();
    assert_eq!(blocks.len(), 6);
    let opened = blocks[4].0;
    assert!(opened < noon() + Duration::minutes(3));
    let burned: BTreeSet<PersonaId> = blocks[..5].iter().map(|(_, d, _)| d.persona).collect();
    assert_eq!(burned.len(), 5);
    assert!(blocks[..5].iter().all(|(_, dispatch, _)| !dispatch.trial));

    // While it was open nothing left: every request was answered at once
    // with the time the pause ends.
    let first_pause = opened + Duration::minutes(30);
    let during: Vec<&Refused> = sim
        .refused
        .iter()
        .filter(|(at, ..)| *at > opened && *at < first_pause)
        .collect();
    assert!(during.len() > 80, "{}", during.len());
    assert!(during.iter().all(
        |(_, _, reason, retry)| *reason == Refusal::DomainPaused && *retry == Some(first_pause)
    ));
    assert!(
        !sim.log
            .iter()
            .any(|(at, _)| *at > opened && *at < first_pause)
    );

    // After the pause one trial went, was blocked, and the pause doubled.
    let trial = blocks[5];
    assert!(trial.1.trial);
    assert!(!burned.contains(&trial.1.persona));
    let trial_left = sim
        .log
        .iter()
        .find(|(_, dispatch)| dispatch.grant == trial.1.grant)
        .unwrap()
        .0;
    assert!(trial_left >= first_pause && trial_left - first_pause < secs(20));
    let second_pause = trial.0 + Duration::hours(1);
    assert!(
        !sim.log
            .iter()
            .any(|(at, _)| *at > trial_left && *at < second_pause)
    );

    // The second trial found the site friendly: closed, and work flows.
    let after: Vec<&(DateTime<Utc>, Dispatch)> = sim
        .log
        .iter()
        .filter(|(at, _)| *at >= second_pause)
        .collect();
    assert!(after[0].1.trial);
    assert!(after[0].0 - second_pause < secs(20));
    assert!(after.len() > 100, "{}", after.len());
    assert!(after[1..].iter().all(|(_, dispatch)| !dispatch.trial));
    let breaker = sim.facts["example.org"].breaker.clone().unwrap();
    assert_eq!(breaker.state, BreakerState::Closed);
    assert_eq!((breaker.trips, breaker.block_count_1h), (0, 0));
    assert!(
        !sim.refused
            .iter()
            .any(|(at, ..)| *at > after[0].0 + secs(5))
    );
    // Only six personas ever met the block.
    assert_eq!(
        sim.facts["example.org"]
            .states
            .values()
            .filter(|state| state.cooldown_until.is_some())
            .count(),
        6
    );
}

/// Twenty personas on a mixed load for three hours: jobs always waiting on
/// five domains, and every ten minutes twenty synchronous requests for
/// twenty other domains in the same instant.
fn a_mixed_afternoon() -> Sim {
    let mut sim = Sim::new(personas(20));
    sim.takes = Box::new(browsing);
    let begin = sim.now;
    let mut volleys = 0_i64;
    sim.run_fed(begin + Duration::hours(3), Duration::minutes(10), |sim| {
        for site in 0..5 {
            let domain = format!("shop{site}.test");
            if !sim.planner.domains_waiting().contains(&domain) {
                sim.submit(Work {
                    pacing: human(),
                    ..work(&domain)
                });
            }
        }
        if sim.now - begin >= Duration::minutes(volleys * 10) {
            volleys += 1;
            for site in 0..20 {
                sim.submit(Work {
                    pacing: human(),
                    ..sync(&format!("news{site}.test"))
                });
            }
        }
    });
    sim
}

#[test]
fn twenty_personas_never_dispatch_in_lockstep() {
    let sim = a_mixed_afternoon();
    assert!(sim.log.len() > 1200, "{}", sim.log.len());
    let everyone: BTreeSet<PersonaId> = sim.log.iter().map(|(_, d)| d.persona).collect();
    assert_eq!(everyone.len(), 20);

    // 1. No two personas share a sequence of waits: each paces itself from
    //    its own seed at its own factor.
    let mut sequences: BTreeSet<Vec<i64>> = BTreeSet::new();
    let mut seeds = BTreeSet::new();
    for persona in &everyone {
        let waits: Vec<i64> = sim
            .starts_of(*persona)
            .iter()
            .take(6)
            .map(|(at, dispatch)| (dispatch.next_slot - *at).num_milliseconds())
            .collect();
        assert_eq!(waits.len(), 6);
        assert!(sequences.insert(waits));
        for (_, dispatch) in sim.starts_of(*persona) {
            seeds.insert((dispatch.domain.clone(), dispatch.pacing_seed.to_string()));
        }
    }
    let streams = seeds.len();
    let by_seed: BTreeSet<&String> = seeds.iter().map(|(_, seed)| seed).collect();
    assert_eq!(by_seed.len(), streams);
    let factors: BTreeSet<u64> = sim
        .views
        .values()
        .map(|view| pacing_factor(&view.seed).to_bits())
        .collect();
    assert_eq!(factors.len(), 20);

    // 2. No shared timer: two personas never start in the same
    //    millisecond, and starts are not gathered on any tick of the
    //    simulation (the work arrived on whole seconds).
    let mut starts: Vec<(DateTime<Utc>, PersonaId)> =
        sim.log.iter().map(|(at, d)| (*at, d.persona)).collect();
    starts.sort();
    assert!(
        starts
            .windows(2)
            .all(|pair| pair[0].1 == pair[1].1 || pair[0].0 != pair[1].0)
    );
    let on_the_second = starts
        .iter()
        .filter(|(at, _)| at.timestamp_subsec_millis() == 0)
        .count();
    assert!(on_the_second * 10 < starts.len(), "{on_the_second}");

    // 3. Starts of different personas within a quarter of a second of each
    //    other are no more frequent than for independent arrivals at the
    //    same rate, and never closer than the shortest drawn pause unless
    //    one of them was timed by its persona's own slot.
    let close = starts
        .windows(2)
        .filter(|pair| pair[0].1 != pair[1].1 && pair[1].0 - pair[0].0 < millis(250))
        .count();
    #[allow(clippy::cast_precision_loss)]
    let (count, span) = (
        starts.len() as f64,
        (starts[starts.len() - 1].0 - starts[0].0).num_milliseconds() as f64 / 1000.0,
    );
    let independent = count * (1.0 - (-(count / span) * 0.25).exp());
    #[allow(clippy::cast_precision_loss)]
    let close = close as f64;
    assert!(close <= independent, "{close} against {independent}");

    // The volleys: twenty requests in one instant leave over many seconds.
    let begin = noon();
    for volley in 0..18 {
        let arrived = begin + Duration::minutes(volley * 10);
        let mut left: Vec<DateTime<Utc>> = sim
            .log
            .iter()
            .filter(|(at, dispatch)| {
                dispatch.domain.starts_with("news") && *at - dispatch.waited == arrived
            })
            .map(|(at, _)| *at)
            .collect();
        left.sort();
        assert_eq!(left.len(), 20, "volley {volley}");
        let in_first_100ms = left
            .iter()
            .filter(|at| **at - arrived < millis(100))
            .count();
        assert!(in_first_100ms <= 1, "volley {volley}: {in_first_100ms}");
        assert!(left[19] - left[0] > secs(5), "volley {volley}");
        for pair in left.windows(2) {
            assert!(pair[1] - pair[0] >= millis(120), "volley {volley}");
        }
    }
}

#[test]
fn the_same_seed_replays_the_same_afternoon() {
    let (first, second) = (a_mixed_afternoon(), a_mixed_afternoon());

    assert_eq!(first.log.len(), second.log.len());
    assert!(
        first
            .log
            .iter()
            .zip(&second.log)
            .all(|(a, b)| a.0 == b.0 && a.1.persona == b.1.persona && a.1.domain == b.1.domain)
    );
}

#[test]
fn the_managers_own_work_leaves_the_last_session_to_work_that_waits() {
    let done = Completion {
        courtesy: Courtesy::NotTheSite,
        blocked: false,
    };
    let waiting_for = |persona: u128| Work {
        pin: Some(id(persona)),
        ..sync("shop.test")
    };

    // Two sessions in all, three per engine: only the first limit binds.
    let mut limits = rules();
    limits.max_browser_sessions = 2;
    limits.max_per_engine = 3;
    let mut planner = Planner::new(limits, Seed::from_u64(3));
    planner.set_personas(personas(6));
    // Nothing waits: both sessions can be had, each under its own name.
    let first = planner.reserve(id(0), true).unwrap();
    let second = planner.reserve(id(1), true).unwrap();
    assert_ne!(first, second);
    assert_eq!(planner.in_flight(Render::Browser), 2);
    assert_eq!(planner.reserve(id(2), true), Err(Held::NoSlot));
    assert!(planner.complete(second, noon(), done));
    assert_eq!(planner.in_flight(Render::Browser), 1);
    // A request for a browser waits (its persona is taken): the one free
    // session is kept for it, whatever the engine of who asks.
    let ticket = planner.submit(waiting_for(0), noon());
    assert_eq!(planner.plan(noon(), &HashMap::new()), []);
    assert_eq!(planner.reserve(id(1), true), Err(Held::NoSlot));
    assert_eq!(planner.reserve(id(2), true), Err(Held::NoSlot));
    // An operator's self-test counts as a synchronous request: it takes
    // the free session, and then there is none for anybody.
    let asked = planner.reserve_for_operator(id(1)).unwrap();
    assert_eq!(planner.in_flight(Render::Browser), 2);
    assert_eq!(planner.reserve_for_operator(id(2)), Err(Held::NoSlot));
    assert_eq!(planner.reserve_for_operator(id(1)), Err(Held::PersonaBusy));
    assert_eq!(
        planner.reserve_for_operator(id(99)),
        Err(Held::UnknownPersona)
    );
    assert!(planner.complete(asked, noon(), done));
    // Nobody waits any more: it can be had again.
    planner.cancel(ticket);
    let again = planner.reserve(id(1), true).unwrap();
    assert_ne!(again, first);
    assert!(planner.complete(again, noon(), done));
    // Work for the http path wants no browser session.
    let mut plain = waiting_for(0);
    plain.render = Render::Http;
    planner.submit(plain, noon());
    planner.reserve(id(1), true).unwrap();
    assert_eq!(planner.in_flight(Render::Browser), 2);

    // Six sessions in all, two per engine: only the second limit binds.
    let mut limits = rules();
    limits.max_browser_sessions = 6;
    limits.max_per_engine = 2;
    let mut planner = Planner::new(limits, Seed::from_u64(3));
    planner.set_personas(personas(6));
    planner.reserve(id(0), true).unwrap();
    planner.submit(waiting_for(0), noon());
    assert_eq!(planner.plan(noon(), &HashMap::new()), []);
    // The engine of the taken persona keeps its last session; the other
    // engine keeps one of its two.
    assert_eq!(planner.reserve(id(2), true), Err(Held::NoSlot));
    planner.reserve(id(1), true).unwrap();
    assert_eq!(planner.reserve(id(3), true), Err(Held::NoSlot));
    assert_eq!(planner.in_flight(Render::Browser), 2);
    // An operator's self-test takes the engine's last session, and no
    // stop holds it.
    planner.set_stop(Some(Refusal::BandwidthExhausted));
    assert_eq!(
        planner.reserve(id(4), true),
        Err(Held::Stopped(Refusal::BandwidthExhausted))
    );
    planner.reserve_for_operator(id(2)).unwrap();
    assert_eq!(planner.reserve_for_operator(id(4)), Err(Held::NoSlot));
    assert_eq!(planner.in_flight(Render::Browser), 3);
}

#[test]
fn work_that_would_rather_be_refused_is_told_when_to_come_back() {
    let mut planner = Planner::new(rules(), Seed::from_u64(3));
    planner.set_personas(personas(1));
    let back_at = noon() + Duration::hours(2);
    let mut state = PersonaDomainState::new(id(0), "shop.test");
    state.cooldown_until = Some(back_at);
    let facts = HashMap::from([(
        "shop.test".to_owned(),
        DomainFacts {
            states: HashMap::from([(id(0), state)]),
            breaker: None,
        },
    )]);
    let with_patience = |until: Option<DateTime<Utc>>| Work {
        give_up_at: until,
        ..sync("shop.test")
    };

    // The only persona is back in two hours. Without a limit the work
    // waits for it, and so it does with a limit at or after that moment.
    planner.submit(with_patience(None), noon());
    planner.submit(with_patience(Some(back_at)), noon());
    planner.submit(with_patience(Some(back_at + secs(1))), noon());
    assert_eq!(planner.plan(noon(), &facts), []);
    assert_eq!(planner.wake_at(), Some(back_at));
    assert_eq!(planner.depth(Priority::Sync), 3);

    // With a limit before it, the work is refused at once and told when.
    let hurried = planner.submit(with_patience(Some(back_at - secs(1))), noon());
    assert_eq!(
        planner.plan(noon(), &facts),
        [Decision::Refuse {
            work: hurried,
            reason: Refusal::NoPersona,
            retry_at: Some(back_at),
        }]
    );
    assert_eq!(planner.depth(Priority::Sync), 3);
}
