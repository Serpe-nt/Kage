//! The planner: which waiting request leaves now, on which persona, and
//! when to look again (spec §3.2, §8.2, §11).
//!
//! The planner holds what only lives while the process does: the queue,
//! what is in flight, and how every persona is pacing itself on every
//! domain. What is stored (personas, their state per domain, the breakers)
//! it is shown whenever it plans, so the store stays the one truth. It
//! reads no clock: every call is told the time.
//!
//! # Visits
//!
//! A person does not fetch one page from a site every two minutes all day;
//! a person visits. So the work of a domain is not dealt out request by
//! request. A persona that fetched a page from a domain is *visiting* it:
//! it keeps taking that domain's requests, each after its own gap from the
//! start of the page before (a floor: on the browser path the time on a
//! page is the behavior engine's) and never before its rest after the end of
//! that page is over (the profile's `rest_ms`: a reader who is done with a
//! page still takes a moment before the next), burst after burst with a
//! break between
//! them, until its visit ends: after the number of bursts its profile
//! drew, after a block, or when no work came for a while. Then it is away
//! from the domain for the long break.
//!
//! A new visitor is chosen, by the score of spec §8.2, only when work
//! waits and fewer personas than the profile's `domain_concurrency` are
//! visiting (D37). A synchronous request does not wait for a visitor's
//! break: when every visitor that could take it is on a break that does
//! not end within `visitor_wait`, another persona starts a visit of its
//! own, past `domain_concurrency`. A visitor in a gap, or in the middle of
//! a page, is waited for however many synchronous requests wait for it:
//! the site sees one address reading at its gap, not every persona of the
//! pool taking turns (F106).
//!
//! # Nobody ready, and a proxy that failed
//!
//! Work that no persona could take because none that may take it is ready
//! now (their proxies are in backoff or being replaced, they cool down
//! for their proxy, they are not warmed up yet) is refused as
//! `no_persona` with a moment to ask again: an outage of the pool says
//! something about now, not about the request, and a job waits it out
//! (F107). Work that no persona could ever take is refused without one.
//!
//! An attempt that failed at the persona's proxy (the site had no say)
//! ends the persona's visit, and for [`PROXY_SUSPICION`] the persona is
//! set aside for another that could start before that time ends, until an
//! attempt of it is answered again (F108). One cooling for hours is no
//! such other: the persona is taken. Work that waits for another looks
//! again when the time ends.
//!
//! # Never in lockstep
//!
//! Every persona's waits come from a sampler seeded from its own seed, the
//! domain and the moment its pacing there began, at its own pace. Nothing
//! here runs on a shared timer: a persona's next page is timed from its
//! own previous page. And a start that is not timed by the persona's own
//! slot (the first page of a visit, or a page whose slot stood open when
//! the request or a free session arrived) is kept apart from the last such
//! start of another persona by a drawn pause, so that requests that arrive
//! in the same instant do not leave in the same instant.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};

use super::breaker::{Admission, BreakerRules};
use super::day::{DayBook, DayRules};
use super::politeness::{Courtesy, ServerErrorRules, ServerErrors};
use super::queue::{Priority, Ticket, WorkQueue};
use super::retry::retry_pause;
use super::selection::{self, Candidate, Network};
use crate::domain::{CountryCode, DomainBreaker, Engine, PersonaDomainState, Render};
use crate::ids::PersonaId;
use crate::macros::string_enum;
use crate::pacing::{Lognormal, PacingProfile, PacingSampler};
use crate::persona::{pacing_factor, place};
use crate::policy::ActiveHoursMode;
use crate::seed::{Seed, SeededRng};

/// How far back a persona's dispatches count as its load.
const LOAD_WINDOW: Duration = Duration::minutes(10);

/// The window of a domain's requests per minute.
const RPM_WINDOW: Duration = Duration::minutes(1);

/// How far back a visit to a domain counts for the score (spec §8.2).
const FRESH_WINDOW: Duration = Duration::hours(1);

/// A page that starts within this long of its persona's slot opening is
/// timed by that slot, not by whatever arrived.
const PACED_TOLERANCE: Duration = Duration::milliseconds(250);

/// When work that nobody ready could take is worth asking about again:
/// the first backoff of a proxy (spec §7.3).
pub const UNREADY_RECHECK: Duration = Duration::minutes(1);

/// How long a persona whose attempt failed at its proxy is set aside for
/// another persona that could start before then: the interval of the pool's
/// health probes (spec §7.4), by which time a probe has said whether the
/// proxy answers.
pub const PROXY_SUSPICION: Duration = Duration::minutes(5);

string_enum! {
    /// Why a request is answered without an attempt. The names are the
    /// `reason` of the `provider_error` the caller gets.
    pub enum Refusal {
        /// A proxy leaked the host's address: nothing may be fetched.
        EgressUnsafe => "egress_unsafe",
        /// The bandwidth budget is used up and the hard stop is on.
        BandwidthExhausted => "bandwidth_exhausted",
        /// The domain's circuit breaker is open.
        DomainPaused => "domain_paused",
        /// No persona can serve the request, now or before it gives up.
        /// With a moment to ask again when one may then: nobody that may
        /// serve it is ready now, or the one it waits for comes too late.
        NoPersona => "no_persona",
        /// The scheduler is shutting down.
        ShuttingDown => "shutting_down",
        /// The site's `robots.txt` is enforced and what it says cannot be
        /// had now (spec §14). Never the planner's own answer: the pipeline
        /// gives it, with `robots_denied` to a caller that waits for its
        /// page and as the reason a job's fetch is put off.
        RobotsUnavailable => "robots_unavailable",
        /// A plain client of level 0 leaves through the site's one plain exit
        /// of the pool (its scout's proxy, spec §25), and that exit cannot
        /// carry traffic now or the site has none. Never the planner's own
        /// answer: the pipeline gives it, and never sends another address.
        PlainExitUnavailable => "plain_exit_unavailable",
    }
}

/// The limits and pauses the planner works with.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannerRules {
    /// Browser attempts in flight at once.
    pub max_browser_sessions: u32,
    /// Attempts on the http path in flight at once.
    pub max_http_inflight: u32,
    /// Browser attempts of one engine in flight at once.
    pub max_per_engine: u32,
    /// How long a synchronous request waits for a visitor before another
    /// persona starts a visit.
    pub visitor_wait: StdDuration,
    /// A visit in which no page was asked for this long is over.
    pub visit_idle: StdDuration,
    /// The pause between two starts of different personas that their own
    /// pacing did not time.
    pub visit_start_spacing: Lognormal,
    /// When personas are awake.
    pub day: DayRules,
    /// What server errors in a row do to a domain.
    pub server_errors: ServerErrorRules,
    /// The longest pause of a breaker.
    pub breaker_max_pause: StdDuration,
    /// How long a trial request holds a breaker without reporting back.
    pub breaker_trial_timeout: StdDuration,
}

/// A persona as the planner needs it: what is stored about it, and whether
/// it can work at all.
#[derive(Debug, Clone, PartialEq)]
pub struct PersonaView {
    /// The persona.
    pub id: PersonaId,
    /// Its seed.
    pub seed: Seed,
    /// Its engine.
    pub engine: Engine,
    /// The country of its address.
    pub country: Option<CountryCode>,
    /// Its time zone.
    pub timezone: String,
    /// Whether it is active, warmed up, and its proxy can carry traffic.
    pub ready: bool,
    /// Whether it has an http path.
    pub http: bool,
    /// Where its address sits.
    pub network: Network,
}

/// What is stored about one domain, as of the moment of planning.
#[derive(Debug, Clone, Default)]
pub struct DomainFacts {
    /// What each persona has done on the domain.
    pub states: HashMap<PersonaId, PersonaDomainState>,
    /// The domain's breaker, when it ever counted a block.
    pub breaker: Option<DomainBreaker>,
}

/// The pause before a retry on another persona: drawn from `pause` at the
/// pace of whichever persona takes the retry, counted from the moment the
/// retry was asked for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPause {
    /// The distribution of the pause, in milliseconds.
    pub pause: Lognormal,
    /// The seed of the draw.
    pub seed: Seed,
}

/// The personas that read a domain for one job, and how many it may have
/// there (spec §16.6): a batch looks like one visitor per site unless its
/// caller asked for more.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Visitors {
    /// The personas that have been reading the domain for the job.
    pub known: Vec<PersonaId>,
    /// How many personas may read the domain for the job; at least one.
    pub max: u32,
}

/// One attempt that waits for a persona.
#[derive(Debug, Clone, PartialEq)]
pub struct Work {
    /// The host the attempt goes to: the key of pacing and policy.
    pub domain: String,
    /// Whose work it is.
    pub priority: Priority,
    /// The path the attempt takes.
    pub render: Render,
    /// The only persona that may make it: a session's, or the one that is
    /// escalating to its browser.
    pub pin: Option<PersonaId>,
    /// Personas that may not: they already tried.
    pub exclude: Vec<PersonaId>,
    /// Networks of the personas this request was blocked on.
    pub avoid: Vec<Network>,
    /// Engines that may serve: the policy's, narrowed by the request.
    pub engines: Vec<Engine>,
    /// Countries that may serve; `None` for any.
    pub countries: Option<Vec<CountryCode>>,
    /// Countries the request would rather have.
    pub prefer_countries: Vec<CountryCode>,
    /// The visitors of the job the work belongs to. While the job has as
    /// many as it may have and they can go on, only they take its work,
    /// and the work waits for them: through their gaps, their breaks and
    /// their nights. A persona that is cooling for the domain, at its
    /// daily cap or no longer ready does not count, and another takes its
    /// place.
    pub visitors: Option<Visitors>,
    /// The pacing profile in force, before a persona's own pace.
    pub pacing: PacingProfile,
    /// What the personas' days mean for the domain.
    pub active_hours: ActiveHoursMode,
    /// The domain's breaker threshold and first pause, from its policy.
    pub breaker_threshold: u32,
    /// See `breaker_threshold`.
    pub breaker_pause: StdDuration,
    /// The pause before this attempt when it is a retry on another
    /// persona.
    pub retry_pause: Option<RetryPause>,
    /// Not before this moment, whoever takes it.
    pub not_before: Option<DateTime<Utc>>,
    /// Refuse instead of waiting past this moment.
    pub give_up_at: Option<DateTime<Utc>>,
    /// The least time from the start of this page to the start of the
    /// persona's next one on the domain: the `Crawl-delay` of a site whose
    /// `robots.txt` is enforced. A gap the persona's pacing draws shorter
    /// is this long and up to a fifth longer, never a constant.
    pub gap_floor: Option<StdDuration>,
    /// A plain client's attempt (level 0, spec §25): no persona makes it,
    /// so nothing of a persona's applies (no gap, no visit, no night, no
    /// daily cap). The site's own gates hold it as they hold everybody: a
    /// paused domain, the pause after server errors, the pacing's
    /// `domain_concurrency` and `domain_rpm`, the global limit of the
    /// http path. It is never a half-open breaker's trial: a site that
    /// answers a plain client says nothing about the personas.
    pub plain: bool,
    /// The other sites the attempt reaches: the later sites of a flow
    /// (spec §10.5, F101). Each holds the attempt as a request of its own
    /// would be held, for the same persona: its breaker, its pause after
    /// server errors, its `domain_concurrency` and `domain_rpm`, and the
    /// persona's cooldown, daily cap, nights and gap there. Empty for
    /// every attempt that reads one site.
    pub legs: Vec<Leg>,
}

/// A later site an attempt reaches besides its own domain: one of a flow's
/// (spec §10.5, F101). Its gates are its own policy's.
#[derive(Debug, Clone, PartialEq)]
pub struct Leg {
    /// The site's host: the key of its pacing and its breaker.
    pub domain: String,
    /// The pacing in force there, before a persona's own pace.
    pub pacing: PacingProfile,
    /// What the personas' days mean there.
    pub active_hours: ActiveHoursMode,
    /// Its breaker threshold, from its policy.
    pub breaker_threshold: u32,
    /// Its breaker's first pause, from its policy.
    pub breaker_pause: StdDuration,
    /// The least gap between two pages of a persona there, as
    /// [`Work::gap_floor`] for the attempt's own domain.
    pub gap_floor: Option<StdDuration>,
}

/// What the planner booked on a later site of an attempt ([`Leg`]).
#[derive(Debug, Clone, PartialEq)]
pub struct LegDispatch {
    /// The site.
    pub domain: String,
    /// Whether the attempt is the one trial of the site's half-open
    /// breaker.
    pub trial: bool,
    /// The rules of the site's breaker, for whoever reports the attempt's
    /// end to it.
    pub breaker: BreakerRules,
    /// Whether the attempt starts a visit of the persona to the site.
    pub starts_visit: bool,
}

/// A gap with a floor under it: the gap as drawn when it is at least the
/// floor, else the floor and up to a fifth of it more. What the site asked
/// for is kept to the millisecond, and no two gaps at the floor are alike.
pub fn floored(
    drawn: StdDuration,
    floor: Option<StdDuration>,
    seed: &Seed,
    page: u32,
) -> StdDuration {
    let Some(floor) = floor.filter(|floor| drawn < *floor) else {
        return drawn;
    };
    let share = Seed::derive("gap-floor", &[seed.as_bytes(), &page.to_le_bytes()])
        .rng()
        .unit();
    floor + floor.mul_f64(share * 0.2)
}

/// One attempt that may leave now.
// Each flag is a fact of its own about the attempt; none excludes another.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq)]
pub struct Dispatch {
    /// The work.
    pub work: Ticket,
    /// Names the attempt until it is reported back.
    pub grant: GrantId,
    /// The persona that makes it.
    pub persona: PersonaId,
    /// The domain.
    pub domain: String,
    /// The path.
    pub render: Render,
    /// How long the work waited in the queue.
    pub waited: Duration,
    /// Whether this page starts a visit of the persona to the domain.
    pub starts_visit: bool,
    /// Which page of the visit this is, from 1.
    pub page_of_visit: u32,
    /// Whether the burst is over with this page: the wait that follows
    /// includes a break.
    pub ends_burst: bool,
    /// Whether the visit is over with this page: the persona is away from
    /// the domain afterwards.
    pub ends_visit: bool,
    /// When the persona's next page on the domain may start.
    pub next_slot: DateTime<Utc>,
    /// Whether this is the one trial request of a half-open breaker.
    pub trial: bool,
    /// Whether the persona was asleep and woken for this one page.
    pub woken: bool,
    /// The seed of the persona's pacing on the domain.
    pub pacing_seed: Seed,
    /// The rules of the domain's breaker, for whoever reports the
    /// attempt's end to it.
    pub breaker: BreakerRules,
    /// What was booked on each of the work's [`Leg`]s, in their order.
    pub legs: Vec<LegDispatch>,
}

/// A plain client's attempt (level 0) that may leave now: no persona, no
/// visit, no slot.
#[derive(Debug, Clone, PartialEq)]
pub struct PlainDispatch {
    /// The work.
    pub work: Ticket,
    /// Names the attempt until it is reported back.
    pub grant: GrantId,
    /// The domain.
    pub domain: String,
    /// How long the work waited in the queue.
    pub waited: Duration,
}

/// What the planner decided about one waiting request.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// It leaves now.
    Dispatch(Dispatch),
    /// A plain client's attempt leaves now.
    Plain(PlainDispatch),
    /// It is answered without an attempt.
    Refuse {
        /// The work.
        work: Ticket,
        /// Why.
        reason: Refusal,
        /// When it would be worth asking again, when that is known.
        retry_at: Option<DateTime<Utc>>,
    },
}

/// Names one attempt in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GrantId(u64);

/// How an attempt ended, as far as the planner needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completion {
    /// What the end says about the site's health.
    pub courtesy: Courtesy,
    /// Whether the persona was blocked: its visit is over.
    pub blocked: bool,
    /// Whether the attempt failed at the persona's proxy, before the site
    /// had a say: its visit is over, and for [`PROXY_SUSPICION`] it is set
    /// aside for another persona that could start before then (F108).
    pub proxy_failed: bool,
}

impl Completion {
    /// An attempt that ended as `courtesy` says, nobody blocked, no proxy
    /// at fault.
    pub const fn of(courtesy: Courtesy) -> Self {
        Self {
            courtesy,
            blocked: false,
            proxy_failed: false,
        }
    }
}

/// Why the manager's own work cannot have a persona now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    /// Nothing may be fetched.
    Stopped(Refusal),
    /// The persona is not known, or not live.
    UnknownPersona,
    /// The persona is making an attempt.
    PersonaBusy,
    /// Every browser session is taken, or the last one is wanted by work
    /// that waits.
    NoSlot,
}

#[derive(Debug, Default)]
struct PersonaRun {
    busy: Option<GrantId>,
    /// Dispatch times within [`LOAD_WINDOW`].
    recent: VecDeque<DateTime<Utc>>,
    /// When an attempt of the persona last failed at its proxy, until an
    /// attempt of it is answered again.
    proxy_failed_at: Option<DateTime<Utc>>,
}

/// How one persona paces itself on one domain.
#[derive(Debug)]
struct Cadence {
    sampler: PacingSampler,
    seed: Seed,
    /// The profile the sampler was made for, before the persona's pace.
    profile: PacingProfile,
    /// When the next page may start.
    next_at: DateTime<Utc>,
    /// Whether the wait that runs is a break.
    on_break: bool,
    /// Whether the persona is visiting.
    visiting: bool,
    last_start: DateTime<Utc>,
    pages: u32,
}

#[derive(Debug, Default)]
struct DomainRun {
    in_flight: u32,
    /// Dispatch times within [`RPM_WINDOW`].
    starts: VecDeque<DateTime<Utc>>,
    errors: ServerErrors,
    /// When the plain client may start its next page on the domain, where
    /// the site's `Crawl-delay` is kept (`Work::gap_floor`).
    plain_next: Option<DateTime<Utc>>,
}

#[derive(Debug)]
struct Grant {
    /// `None` for a plain client's attempt.
    persona: Option<PersonaId>,
    engine: Engine,
    render: Render,
    domain: Option<String>,
    /// The other sites the attempt counts on ([`Leg`]).
    legs: Vec<String>,
}

#[derive(Debug)]
struct Queued {
    work: Work,
    since: DateTime<Utc>,
}

/// What looking at one waiting request came to.
enum Looked {
    Go {
        persona: PersonaId,
        trial: bool,
        woken: bool,
        /// Whether the persona's own slot timed the start.
        paced: bool,
        /// Which of the work's legs the attempt is the breaker's trial of.
        leg_trials: Vec<bool>,
    },
    /// A plain client's attempt leaves.
    GoPlain,
    /// Wait; until the moment given, or for something in flight to end.
    Wait(Option<DateTime<Utc>>),
    Refuse(Refusal, Option<DateTime<Utc>>),
}

/// What is known about an attempt at the moment it leaves.
#[derive(Clone)]
struct Flags {
    trial: bool,
    woken: bool,
    paced: bool,
    leg_trials: Vec<bool>,
}

/// What holds a site's work by the site's own limits.
enum SiteHold {
    /// The pause after server errors in a row, until then.
    ErrorPause(DateTime<Utc>),
    /// As many attempts in flight as `domain_concurrency` allows.
    Full,
    /// As many starts in the last minute as `domain_rpm` allows: until the
    /// oldest of them is a minute old.
    Rpm(Option<DateTime<Utc>>),
}

/// What booking a page on a site's pacing came to.
struct Booked {
    starts_visit: bool,
    page_of_visit: u32,
    next_slot: DateTime<Utc>,
    pacing_seed: Seed,
    ends_burst: bool,
    ends_visit: bool,
}

/// A persona that could serve a request, and when.
#[allow(clippy::struct_excessive_bools)] // Five independent facts.
#[derive(Clone, Copy)]
struct Possible<'a> {
    view: &'a PersonaView,
    /// When it could start; `None` while that hangs on something in flight.
    at: Option<DateTime<Utc>>,
    /// When it could start once what is in flight lets it: `at` when
    /// nothing does. For one in the middle of a page that is the end of
    /// the page and the rest after it, which nobody knows before the page
    /// ends, and never earlier than this.
    after: DateTime<Utc>,
    awake: bool,
    /// Whether it is visiting the domain and nothing but its own pacing
    /// stands between it and its next page.
    visiting: bool,
    on_break: bool,
    /// Whether its slot on the domain opens just now: a start now is timed
    /// by its own pacing.
    paced: bool,
    /// Whether it is kept from the domain for now: it cools after a block,
    /// or reached its daily cap. A job's visitor that is barred is no
    /// longer one the job waits for.
    barred: bool,
    /// While an attempt of it failed at its proxy a moment ago, when that
    /// suspicion ends: until then it is set aside for another persona that
    /// could start before (`set_suspects_aside`).
    suspect_until: Option<DateTime<Utc>>,
}

/// See the module documentation.
#[derive(Debug)]
pub struct Planner {
    rules: PlannerRules,
    seed: Seed,
    rng: SeededRng,
    queue: WorkQueue<Queued>,
    personas: BTreeMap<PersonaId, PersonaView>,
    runs: HashMap<PersonaId, PersonaRun>,
    cadences: HashMap<(PersonaId, String), Cadence>,
    domains: HashMap<String, DomainRun>,
    grants: HashMap<GrantId, Grant>,
    browser_in_flight: u32,
    http_in_flight: u32,
    per_engine: HashMap<Engine, u32>,
    /// No start of another persona that its own pacing did not time
    /// leaves before this.
    next_free_start: Option<DateTime<Utc>>,
    last_spaced: Option<PersonaId>,
    stop: Option<Refusal>,
    wake: Option<DateTime<Utc>>,
    next_grant: u64,
    /// Who is awake at the moment being planned, and when the others get
    /// up; filled once per plan, and only when some waiting work asks.
    awake: HashMap<PersonaId, (bool, DateTime<Utc>)>,
    days: DayBook,
}

fn chrono_of(duration: StdDuration) -> Duration {
    Duration::from_std(duration).unwrap_or(Duration::MAX)
}

fn earlier(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Sets aside every persona whose proxy failed a moment ago (F108) when
/// another could start before the first suspicion ends: one cooling after
/// a block or waiting for its hours is no such other, and the suspect
/// stays. Returns who remains, and when the work should look again: the
/// end of the suspicion of the ones set aside.
fn set_suspects_aside(possible: Vec<Possible<'_>>) -> (Vec<Possible<'_>>, Option<DateTime<Utc>>) {
    let Some(cleared) = possible.iter().filter_map(|one| one.suspect_until).min() else {
        return (possible, None);
    };
    let other_first = possible
        .iter()
        .any(|one| one.suspect_until.is_none() && one.after <= cleared);
    if !other_first {
        return (possible, None);
    }
    let rest = possible
        .into_iter()
        .filter(|one| one.suspect_until.is_none())
        .collect();
    (rest, Some(cleared))
}

/// What is stored about every later site of `work`, in the order of its
/// legs (F101). `None` while one of them has its own work held, which holds
/// this work too, or nothing of it is known yet: it waits for the next
/// plan.
fn leg_facts<'f>(
    work: &Work,
    facts: &'f HashMap<String, DomainFacts>,
    held: &HashSet<String>,
) -> Option<Vec<&'f DomainFacts>> {
    work.legs
        .iter()
        .map(|leg| {
            (!held.contains(&leg.domain))
                .then(|| facts.get(&leg.domain))
                .flatten()
        })
        .collect()
}

fn prune(times: &mut VecDeque<DateTime<Utc>>, now: DateTime<Utc>, window: Duration) {
    while times
        .front()
        .is_some_and(|at| now.signed_duration_since(*at) >= window)
    {
        times.pop_front();
    }
}

impl Planner {
    /// A planner with nothing waiting. `seed` draws its ties and the pauses
    /// between visit starts; it is worth logging.
    pub fn new(rules: PlannerRules, seed: Seed) -> Self {
        Self {
            days: DayBook::new(rules.day),
            rules,
            rng: seed.rng(),
            seed,
            queue: WorkQueue::new(),
            personas: BTreeMap::new(),
            runs: HashMap::new(),
            cadences: HashMap::new(),
            domains: HashMap::new(),
            grants: HashMap::new(),
            browser_in_flight: 0,
            http_in_flight: 0,
            per_engine: HashMap::new(),
            next_free_start: None,
            last_spaced: None,
            stop: None,
            wake: None,
            next_grant: 0,
            awake: HashMap::new(),
        }
    }

    /// The seed of the planner's own draws.
    pub const fn seed(&self) -> &Seed {
        &self.seed
    }

    /// The rules in force.
    pub const fn rules(&self) -> &PlannerRules {
        &self.rules
    }

    /// Shows the planner the personas as they are stored now. A persona
    /// that is no longer among them is forgotten, with its pacing.
    pub fn set_personas(&mut self, personas: impl IntoIterator<Item = PersonaView>) {
        self.personas = personas
            .into_iter()
            .map(|persona| (persona.id, persona))
            .collect();
        let known = &self.personas;
        self.cadences
            .retain(|(persona, _), _| known.contains_key(persona));
        self.runs
            .retain(|persona, run| known.contains_key(persona) || run.busy.is_some());
    }

    /// Stops every dispatch for a reason (a leak, the bandwidth cap), or
    /// lets them go again with `None`.
    pub fn set_stop(&mut self, stop: Option<Refusal>) {
        self.stop = stop;
    }

    /// Why nothing may be fetched, when that is so.
    pub const fn stop(&self) -> Option<Refusal> {
        self.stop
    }

    /// Queues one attempt at `now`.
    pub fn submit(&mut self, work: Work, now: DateTime<Utc>) -> Ticket {
        let priority = work.priority;
        self.queue.push(priority, Queued { work, since: now })
    }

    /// Takes waiting work out of the queue: its caller gave up. `false`
    /// when it already left.
    pub fn cancel(&mut self, ticket: Ticket) -> bool {
        self.queue.remove(ticket).is_some()
    }

    /// The domains that have work waiting.
    pub fn domains_waiting(&self) -> Vec<String> {
        let mut domains: Vec<String> = self
            .queue
            .iter()
            .flat_map(|(_, queued)| {
                std::iter::once(queued.work.domain.clone())
                    .chain(queued.work.legs.iter().map(|leg| leg.domain.clone()))
            })
            .collect();
        domains.sort_unstable();
        domains.dedup();
        domains
    }

    /// How many attempts of a priority wait.
    pub fn depth(&self, priority: Priority) -> usize {
        self.queue.len_of(priority)
    }

    /// Attempts in flight on a path.
    pub const fn in_flight(&self, render: Render) -> u32 {
        match render {
            Render::Browser => self.browser_in_flight,
            Render::Http => self.http_in_flight,
        }
    }

    /// How many personas are visiting a domain at `now`.
    pub fn visitors(&self, domain: &str, now: DateTime<Utc>) -> usize {
        self.cadences
            .iter()
            .filter(|((persona, on), cadence)| {
                on == domain && self.is_visiting(*persona, cadence, now)
            })
            .count()
    }

    /// When to plan again if nothing else happens; `None` when only an
    /// attempt's end or new work can change anything.
    pub const fn wake_at(&self) -> Option<DateTime<Utc>> {
        self.wake
    }

    fn busy(&self, persona: PersonaId) -> bool {
        self.runs
            .get(&persona)
            .is_some_and(|run| run.busy.is_some())
    }

    fn engine_load(&self, engine: Engine) -> u32 {
        self.per_engine.get(&engine).copied().unwrap_or(0)
    }

    /// A visit lasts while its persona is making a page of it, for
    /// `visit_idle` after its last page started, and through its own wait
    /// however long that is: a break that outlasts the idle limit is still
    /// the visit's break, and the persona is its visitor when it returns.
    fn is_visiting(&self, persona: PersonaId, cadence: &Cadence, now: DateTime<Utc>) -> bool {
        cadence.visiting
            && (self.busy(persona)
                || now.signed_duration_since(cadence.last_start) < chrono_of(self.rules.visit_idle)
                || now <= cadence.next_at + chrono_of(self.rules.visitor_wait))
    }

    fn breaker_rules(&self, work: &Work) -> BreakerRules {
        BreakerRules {
            threshold: work.breaker_threshold,
            pause: work.breaker_pause,
            max_pause: self.rules.breaker_max_pause,
            trial_timeout: self.rules.breaker_trial_timeout,
        }
    }

    fn leg_breaker_rules(&self, leg: &Leg) -> BreakerRules {
        BreakerRules {
            threshold: leg.breaker_threshold,
            pause: leg.breaker_pause,
            max_pause: self.rules.breaker_max_pause,
            trial_timeout: self.rules.breaker_trial_timeout,
        }
    }

    /// What holds work of a site by the site's own limits now: the pause
    /// after server errors, its concurrency and its requests per minute.
    fn site_hold(
        &self,
        now: DateTime<Utc>,
        domain: &str,
        pacing: &PacingProfile,
    ) -> Option<SiteHold> {
        let run = self.domains.get(domain);
        if let Some(until) = run.and_then(|run| run.errors.paused_until(now)) {
            return Some(SiteHold::ErrorPause(until));
        }
        if run.is_some_and(|run| run.in_flight >= pacing.domain_concurrency) {
            return Some(SiteHold::Full);
        }
        let recent: Vec<DateTime<Utc>> = run.map_or_else(Vec::new, |run| {
            run.starts
                .iter()
                .filter(|at| now.signed_duration_since(**at) < RPM_WINDOW)
                .copied()
                .collect()
        });
        if recent.len() >= usize::try_from(pacing.domain_rpm).unwrap_or(usize::MAX) {
            return Some(SiteHold::Rpm(recent.first().map(|at| *at + RPM_WINDOW)));
        }
        None
    }

    /// Looks at everything that waits, in order, and says what leaves and
    /// what is refused. `facts` holds what is stored about every domain
    /// with waiting work; work of a domain that is missing waits for the
    /// next call. Dispatched and refused work is out of the queue.
    pub fn plan(
        &mut self,
        now: DateTime<Utc>,
        facts: &HashMap<String, DomainFacts>,
    ) -> Vec<Decision> {
        let mut decisions = Vec::new();
        let mut wake: Option<DateTime<Utc>> = None;
        // Domains whose own limits hold their work: later work of the same
        // domain does not overtake.
        let mut held: HashSet<String> = HashSet::new();
        // Visitors that work earlier in the queue is waiting for.
        let mut claimed: HashSet<PersonaId> = HashSet::new();
        // Domains whose half-open breaker let its trial go in this pass.
        let mut trials: HashSet<String> = HashSet::new();
        self.awake.clear();
        let hours_matter = self.queue.iter().any(|(_, queued)| {
            queued.work.active_hours != ActiveHoursMode::Ignore
                || queued
                    .work
                    .legs
                    .iter()
                    .any(|leg| leg.active_hours != ActiveHoursMode::Ignore)
        });
        if hours_matter {
            for view in self.personas.values() {
                let awake = self.days.awake(&view.seed, now, &view.timezone);
                let up = self.days.next_up(&view.seed, now, &view.timezone);
                self.awake.insert(view.id, (awake, up));
            }
        }

        for ticket in self.queue.tickets() {
            let Some(queued) = self.queue.get(ticket) else {
                continue;
            };
            let domain = queued.work.domain.clone();
            if held.contains(&domain) {
                continue;
            }
            let Some(known) = facts.get(&domain) else {
                continue;
            };
            let Some(legs) = leg_facts(&queued.work, facts, &held) else {
                continue;
            };
            let looked = self.look(
                now,
                queued,
                (known, &legs),
                &mut claimed,
                &mut held,
                &trials,
            );
            match looked {
                Looked::Go {
                    persona,
                    trial,
                    woken,
                    paced,
                    leg_trials,
                } => {
                    let Some(queued) = self.queue.remove(ticket) else {
                        continue;
                    };
                    if trial {
                        trials.insert(domain);
                    }
                    for (leg, tried) in queued.work.legs.iter().zip(&leg_trials) {
                        if *tried {
                            trials.insert(leg.domain.clone());
                        }
                    }
                    decisions.push(Decision::Dispatch(self.dispatch(
                        now,
                        ticket,
                        &queued,
                        persona,
                        Flags {
                            trial,
                            woken,
                            paced,
                            leg_trials,
                        },
                    )));
                }
                Looked::GoPlain => {
                    let Some(queued) = self.queue.remove(ticket) else {
                        continue;
                    };
                    decisions.push(Decision::Plain(self.dispatch_plain(now, ticket, &queued)));
                }
                Looked::Wait(until) => wake = earlier(wake, until),
                Looked::Refuse(reason, retry_at) => {
                    self.queue.remove(ticket);
                    decisions.push(Decision::Refuse {
                        work: ticket,
                        reason,
                        retry_at,
                    });
                }
            }
        }
        self.wake = wake;
        decisions
    }

    /// Decides one waiting request.
    #[allow(clippy::too_many_lines)] // One request against every rule, in order.
    fn look(
        &self,
        now: DateTime<Utc>,
        queued: &Queued,
        (facts, legs): (&DomainFacts, &[&DomainFacts]),
        claimed: &mut HashSet<PersonaId>,
        held: &mut HashSet<String>,
        trials: &HashSet<String>,
    ) -> Looked {
        let work = &queued.work;
        if let Some(reason) = self.stop {
            return Looked::Refuse(reason, None);
        }
        // Every later site of the attempt admits it as it would admit a
        // request of its own (F101): a paused site refuses it, and a
        // half-open one lets it go as its one trial.
        let mut leg_trials = Vec::with_capacity(work.legs.len());
        for (leg, known) in work.legs.iter().zip(legs) {
            let mut tried = false;
            if let Some(breaker) = &known.breaker {
                match breaker.admission(now, &self.leg_breaker_rules(leg)) {
                    Admission::Flow => {}
                    Admission::Trial if !trials.contains(&leg.domain) => tried = true,
                    Admission::Trial => return Looked::Refuse(Refusal::DomainPaused, None),
                    Admission::Paused { until } => {
                        return Looked::Refuse(Refusal::DomainPaused, until);
                    }
                }
            }
            leg_trials.push(tried);
        }
        let mut trial = false;
        if let Some(breaker) = &facts.breaker {
            match breaker.admission(now, &self.breaker_rules(work)) {
                Admission::Flow => {}
                // A plain client is never the trial of a breaker.
                Admission::Trial if work.plain => {
                    return Looked::Refuse(Refusal::DomainPaused, None);
                }
                Admission::Trial if !trials.contains(&work.domain) => trial = true,
                Admission::Trial => return Looked::Refuse(Refusal::DomainPaused, None),
                Admission::Paused { until } => {
                    return Looked::Refuse(Refusal::DomainPaused, until);
                }
            }
        }
        if let Some(at) = work.not_before.filter(|at| *at > now) {
            return Looked::Wait(Some(at));
        }

        // The domain's own limits.
        let run = self.domains.get(&work.domain);
        if let Some(until) = run.and_then(|run| run.errors.paused_until(now)) {
            held.insert(work.domain.clone());
            return self.wait_or_give_up(work, Some(until));
        }
        if run.is_some_and(|run| run.in_flight >= work.pacing.domain_concurrency) {
            held.insert(work.domain.clone());
            return Looked::Wait(None);
        }
        let in_minute = run.map_or(0, |run| {
            run.starts
                .iter()
                .filter(|at| now.signed_duration_since(**at) < RPM_WINDOW)
                .count()
        });
        if in_minute >= usize::try_from(work.pacing.domain_rpm).unwrap_or(usize::MAX) {
            held.insert(work.domain.clone());
            let oldest = run.and_then(|run| {
                run.starts
                    .iter()
                    .find(|at| now.signed_duration_since(**at) < RPM_WINDOW)
            });
            return Looked::Wait(oldest.map(|at| *at + RPM_WINDOW));
        }
        // The later sites' own limits hold the attempt too; they hold no
        // other work of this domain.
        for leg in &work.legs {
            match self.site_hold(now, &leg.domain, &leg.pacing) {
                None => {}
                Some(SiteHold::ErrorPause(until)) => {
                    return self.wait_or_give_up(work, Some(until));
                }
                Some(SiteHold::Full) => return Looked::Wait(None),
                Some(SiteHold::Rpm(until)) => return Looked::Wait(until),
            }
        }

        // The global limits.
        let full = match work.render {
            Render::Browser => self.browser_in_flight >= self.rules.max_browser_sessions,
            Render::Http => self.http_in_flight >= self.rules.max_http_inflight,
        };
        if full {
            return Looked::Wait(None);
        }
        // A plain client needs nothing more: no persona makes its attempt.
        // Where the site's file is enforced it keeps to its `Crawl-delay`
        // as a persona does, from the start of one of its pages on the
        // domain to the start of the next (F99: level 0 kept none).
        if work.plain {
            if work.gap_floor.is_some()
                && let Some(at) = self
                    .domains
                    .get(&work.domain)
                    .and_then(|domain| domain.plain_next)
                    .filter(|at| *at > now)
            {
                // A request that would wait past its patience is refused
                // with the moment, as a persona's is: a job's page is put
                // off, never held in the queue for an hour.
                return self.wait_or_give_up(work, Some(at));
            }
            return Looked::GoPlain;
        }

        let possible = self.possible(now, queued, (facts, legs));
        if possible.is_empty() {
            // Nobody ready now: an outage of the pool says nothing about
            // the request, and it is worth asking again (F107). Nobody at
            // all: it never will be.
            let again = self
                .could_serve_once_ready(work)
                .then(|| now + UNREADY_RECHECK);
            return Looked::Refuse(Refusal::NoPersona, again);
        }
        let ready = |one: &Possible<'_>| one.at.is_some_and(|at| at <= now);

        // A pinned request has one persona and waits for it.
        if work.pin.is_some() {
            let Some(only) = possible.first() else {
                return Looked::Refuse(Refusal::NoPersona, None);
            };
            if ready(only) {
                if let Some(at) = self.spaced(now, only) {
                    return Looked::Wait(Some(at));
                }
                return self.go(now, only, (trial, &leg_trials), work);
            }
            return self.wait_or_give_up(work, only.at);
        }

        // A persona whose proxy failed a moment ago is set aside for one
        // that could start before the suspicion ends (F108); the work looks
        // again when it ends, when the one set aside may be the first.
        let (possible, recheck) = set_suspects_aside(possible);
        let looked = self.choose(now, work, &possible, claimed, facts, (trial, &leg_trials));
        match (looked, recheck) {
            (Looked::Wait(at), Some(recheck)) => Looked::Wait(earlier(at, Some(recheck))),
            (Looked::Refuse(Refusal::NoPersona, Some(at)), Some(recheck)) if at > recheck => {
                self.wait_or_give_up(work, Some(recheck))
            }
            (looked, _) => looked,
        }
    }

    /// Decides one waiting request among the personas that may take it:
    /// a visitor, a new visitor when there is room, or the first who can.
    /// `trials` says whether the attempt is the half-open trial of its
    /// domain and of each later site of a flow (F101).
    #[allow(clippy::too_many_lines)] // Every way a persona may take it, in order.
    fn choose(
        &self,
        now: DateTime<Utc>,
        work: &Work,
        possible: &[Possible<'_>],
        claimed: &mut HashSet<PersonaId>,
        facts: &DomainFacts,
        trials: (bool, &[bool]),
    ) -> Looked {
        let ready = |one: &Possible<'_>| one.at.is_some_and(|at| at <= now);

        // A job that has the visitors it may have on the domain stays with
        // them; another persona comes only for one that cannot go on.
        let possible = match &work.visitors {
            Some(visitors) => {
                let staying: Vec<Possible<'_>> = possible
                    .iter()
                    .filter(|one| visitors.known.contains(&one.view.id) && !one.barred)
                    .copied()
                    .collect();
                let full = staying.len() >= usize::try_from(visitors.max.max(1)).unwrap_or(1);
                if full { staying } else { possible.to_vec() }
            }
            _ => possible.to_vec(),
        };

        // A persona that is visiting takes the work when its slot is open.
        let visitor = possible
            .iter()
            .filter(|one| one.visiting && ready(one) && !claimed.contains(&one.view.id))
            .min_by_key(|one| (one.at, one.view.id));
        if let Some(visitor) = visitor {
            if let Some(at) = self.spaced(now, visitor) {
                return Looked::Wait(Some(at));
            }
            return self.go(now, visitor, trials, work);
        }
        // One whose slot opens soon is waited for: the work is its.
        let patience = chrono_of(self.rules.visitor_wait);
        let soon = possible
            .iter()
            .filter(|one| one.visiting && !claimed.contains(&one.view.id))
            .filter_map(|one| one.at.map(|at| (at, one)))
            .filter(|(at, one)| match work.priority {
                Priority::Sync => *at <= now + patience,
                // A job waits out a gap of any length, and a break that is
                // nearly over.
                Priority::Job => !one.on_break || *at <= now + patience,
            })
            .min_by_key(|(at, one)| (*at, one.view.id));
        if let Some((at, one)) = soon {
            claimed.insert(one.view.id);
            return self.wait_or_give_up(work, Some(at));
        }

        // Otherwise a new visitor, when there is room for one (D37). A
        // synchronous request goes past the room for a visitor's break
        // only: when every visitor that could take it is on a break that
        // does not end within `visitor_wait`. A visitor in a gap or in
        // the middle of a page is waited for, however many synchronous
        // requests wait for it (F106). One that no visitor could take (it
        // excludes them, or asks for an engine or a country they lack) has
        // nobody to wait for, and starts a visit as for a break.
        let visiting = self.visitors(&work.domain, now);
        let room = visiting < usize::try_from(work.pacing.domain_concurrency).unwrap_or(usize::MAX);
        let past_room = !room
            && work.priority == Priority::Sync
            && possible
                .iter()
                .filter(|one| one.visiting)
                .all(|one| one.on_break && one.after > now + patience);
        if room || past_room {
            let fresh: Vec<&Possible<'_>> = possible
                .iter()
                .filter(|one| !one.visiting && ready(one))
                .collect();
            if !fresh.is_empty() {
                let candidates: Vec<Candidate> = fresh
                    .iter()
                    .map(|one| self.candidate(now, one, work, facts))
                    .collect();
                let prefer_awake = work.active_hours != ActiveHoursMode::Ignore;
                // The draw of a tie is the planner's, but looking must not
                // move it: a request that then waits would change what the
                // next one draws.
                let mut rng = self
                    .seed
                    .child("tie")
                    .child(&format!("{}:{}", work.domain, now.timestamp_millis()))
                    .rng();
                if let Some(index) = selection::choose(&candidates, prefer_awake, &mut rng)
                    && let Some(chosen) = fresh.get(index)
                {
                    if let Some(at) = self.spaced(now, chosen) {
                        return Looked::Wait(Some(at));
                    }
                    return self.go(now, chosen, trials, work);
                }
            }
        }

        // Nobody can start now: wait for the first who can. Work is given
        // up only for a wait that is known to be too long: one who is
        // making an attempt counts from what it waits for besides, and one
        // whose slot is open waits for nothing but the end of its page,
        // which nobody knows before it comes; one that earlier work waits
        // for counts from its slot, after which the work behind may have
        // it. A job used to give up on its visitors for the long break of
        // one of them while the others were reading, and its site stood
        // still for that break (follow-up F54).
        let waiting_for = possible
            .iter()
            .filter(|one| room || one.visiting || past_room);
        let first = waiting_for
            .clone()
            .filter(|one| !claimed.contains(&one.view.id))
            .filter_map(|one| one.at)
            .min();
        let back = waiting_for.map(|one| one.after).min();
        match back {
            Some(back) if back > now => match self.wait_or_give_up(work, Some(back)) {
                // The end of an attempt plans again by itself.
                Looked::Wait(_) => Looked::Wait(first),
                refused => refused,
            },
            // Everyone who could is making an attempt, or is spoken for.
            _ => Looked::Wait(first),
        }
    }

    /// When a persona has to wait because another persona's start left a
    /// moment ago; `None` when it may start: its own slot times it, or the
    /// last such start was its own.
    fn spaced(&self, now: DateTime<Utc>, one: &Possible<'_>) -> Option<DateTime<Utc>> {
        if one.paced || self.last_spaced == Some(one.view.id) {
            return None;
        }
        self.next_free_start.filter(|at| *at > now)
    }

    /// Whether a persona that may take the work exists, though none is
    /// ready now: what keeps it is a state of the moment (its proxy in
    /// backoff, a cooling, a warm-up), not the request.
    fn could_serve_once_ready(&self, work: &Work) -> bool {
        self.personas.values().any(|view| {
            !view.ready
                && work.pin.is_none_or(|pin| pin == view.id)
                && !work.exclude.contains(&view.id)
                && work.engines.contains(&view.engine)
                && (work.render == Render::Browser || view.http)
                && work.countries.as_ref().is_none_or(|countries| {
                    view.country
                        .is_some_and(|country| countries.contains(&country))
                })
        })
    }

    /// Waits until `until`, unless the request would rather be refused
    /// than wait that long.
    #[allow(clippy::unused_self)]
    fn wait_or_give_up(&self, work: &Work, until: Option<DateTime<Utc>>) -> Looked {
        match (until, work.give_up_at) {
            (Some(until), Some(limit)) if until > limit => {
                Looked::Refuse(Refusal::NoPersona, Some(until))
            }
            _ => Looked::Wait(until),
        }
    }

    #[allow(clippy::unused_self)]
    fn go(
        &self,
        _now: DateTime<Utc>,
        one: &Possible<'_>,
        (trial, leg_trials): (bool, &[bool]),
        work: &Work,
    ) -> Looked {
        Looked::Go {
            persona: one.view.id,
            trial,
            // Asleep, and the hour matters: it reads the one page.
            woken: !one.awake && work.active_hours != ActiveHoursMode::Ignore,
            paced: one.paced,
            leg_trials: leg_trials.to_vec(),
        }
    }

    /// Every persona that could serve the work, and when. A persona that
    /// may not read one of the work's later sites now (it cools there, has
    /// reached its daily cap there, sleeps by that site's hours, or its gap
    /// there has not run out) is held as it would be on the domain itself.
    #[allow(clippy::too_many_lines)] // One persona against every rule of every site.
    fn possible<'a>(
        &'a self,
        now: DateTime<Utc>,
        queued: &Queued,
        (facts, legs): (&DomainFacts, &[&DomainFacts]),
    ) -> Vec<Possible<'a>> {
        let work = &queued.work;
        let mut possible = Vec::new();
        for view in self.personas.values() {
            if !view.ready
                || work.pin.is_some_and(|pin| pin != view.id)
                || work.exclude.contains(&view.id)
                || !work.engines.contains(&view.engine)
                || (work.render == Render::Http && !view.http)
            {
                continue;
            }
            if let Some(countries) = &work.countries
                && !view
                    .country
                    .is_some_and(|country| countries.contains(&country))
            {
                continue;
            }
            let mut at = now;
            let mut barred = false;
            if let Some(state) = facts.states.get(&view.id) {
                if let Some(until) = state.cooldown_until.filter(|until| *until > now) {
                    at = at.max(until);
                    barred = true;
                }
                let today = place::local_day(now, &view.timezone);
                if state.cap_reached(today, work.pacing.daily_cap_per_persona_per_domain) {
                    at = at.max(place::next_local_midnight(now, &view.timezone));
                    barred = true;
                }
            }
            for (leg, known) in work.legs.iter().zip(legs) {
                let Some(state) = known.states.get(&view.id) else {
                    continue;
                };
                if let Some(until) = state.cooldown_until.filter(|until| *until > now) {
                    at = at.max(until);
                    barred = true;
                }
                let today = place::local_day(now, &view.timezone);
                if state.cap_reached(today, leg.pacing.daily_cap_per_persona_per_domain) {
                    at = at.max(place::next_local_midnight(now, &view.timezone));
                    barred = true;
                }
            }
            // Where the hour is not looked at, everyone counts as awake.
            let hours_matter = work.active_hours != ActiveHoursMode::Ignore
                || work
                    .legs
                    .iter()
                    .any(|leg| leg.active_hours != ActiveHoursMode::Ignore);
            let (awake, up) = if hours_matter {
                self.awake.get(&view.id).copied().unwrap_or((true, now))
            } else {
                (true, now)
            };
            let sleeps = |mode: ActiveHoursMode| match mode {
                ActiveHoursMode::Ignore => false,
                ActiveHoursMode::Enforce => !awake,
                // A job never wakes a sleeper; a synchronous request may.
                ActiveHoursMode::Prefer => !awake && work.priority != Priority::Sync,
            };
            let sleeps_through =
                sleeps(work.active_hours) || work.legs.iter().any(|leg| sleeps(leg.active_hours));
            if sleeps_through {
                at = at.max(up);
            }
            // Its gap on a later site is no gap of its own on the domain.
            for leg in &work.legs {
                if let Some(cadence) = self.cadences.get(&(view.id, leg.domain.clone())) {
                    at = at.max(cadence.next_at);
                }
            }
            if let Some(retry) = &work.retry_pause {
                // A retry leaves a pause after the block, at the pace of
                // the persona that makes it.
                let pause = retry_pause(&retry.pause, pacing_factor(&view.seed), &retry.seed);
                at = at.max(queued.since + chrono_of(pause));
            }
            // Whether anything but the persona's own pacing delays it.
            let held_back = at > now;
            let cadence = self.cadences.get(&(view.id, work.domain.clone()));
            if let Some(cadence) = cadence {
                at = at.max(cadence.next_at);
            }
            let in_use = self.busy(view.id)
                || (work.render == Render::Browser
                    && self.engine_load(view.engine) >= self.rules.max_per_engine);
            possible.push(Possible {
                view,
                at: (!in_use).then_some(at),
                after: at,
                awake,
                visiting: !held_back
                    && cadence.is_some_and(|cadence| self.is_visiting(view.id, cadence, now)),
                on_break: cadence.is_some_and(|cadence| cadence.on_break),
                paced: cadence.is_some_and(|cadence| {
                    let open_for = now.signed_duration_since(cadence.next_at);
                    open_for >= Duration::zero() && open_for < PACED_TOLERANCE
                }),
                barred,
                suspect_until: self
                    .runs
                    .get(&view.id)
                    .and_then(|run| run.proxy_failed_at)
                    .map(|at| at + PROXY_SUSPICION)
                    .filter(|until| *until > now),
            });
        }
        possible
    }

    fn candidate(
        &self,
        now: DateTime<Utc>,
        one: &Possible<'_>,
        work: &Work,
        facts: &DomainFacts,
    ) -> Candidate {
        let state = facts.states.get(&one.view.id);
        let recent_load = self.runs.get(&one.view.id).map_or(0, |run| {
            run.recent
                .iter()
                .filter(|at| now.signed_duration_since(**at) < LOAD_WINDOW)
                .count()
        });
        Candidate {
            persona: one.view.id,
            country_preferred: one
                .view
                .country
                .is_some_and(|country| work.prefer_countries.contains(&country)),
            visited_last_hour: state
                .and_then(|state| state.last_request_at)
                .is_some_and(|at| now.signed_duration_since(at) < FRESH_WINDOW),
            recent_load: u32::try_from(recent_load).unwrap_or(u32::MAX),
            previous_ok: state
                .and_then(|state| state.last_outcome)
                .is_some_and(crate::outcome::Outcome::is_success),
            awake: one.awake,
            same_subnet_as_blocked: work
                .avoid
                .iter()
                .any(|network| network.shares_subnet(&one.view.network)),
            same_asn_as_blocked: work
                .avoid
                .iter()
                .any(|network| network.shares_asn(&one.view.network)),
        }
    }

    /// Books an attempt that leaves now.
    #[allow(clippy::too_many_lines)] // Every counter an attempt moves, in one place.
    fn dispatch(
        &mut self,
        now: DateTime<Utc>,
        ticket: Ticket,
        queued: &Queued,
        persona: PersonaId,
        flags: Flags,
    ) -> Dispatch {
        let Flags {
            trial,
            woken,
            paced,
            leg_trials,
        } = flags;
        let work = &queued.work;
        let (engine, persona_seed) = self
            .personas
            .get(&persona)
            .map_or((Engine::Chromium, self.seed), |view| {
                (view.engine, view.seed)
            });
        let factor = pacing_factor(&persona_seed);

        let Booked {
            starts_visit,
            page_of_visit,
            next_slot,
            pacing_seed,
            ends_burst,
            ends_visit,
        } = self.book(
            now,
            (persona, &persona_seed, factor),
            (&work.domain, work.pacing, work.gap_floor),
            woken,
        );
        // The later sites are visited by the same persona in the same
        // attempt: each is a page of its own on that site's pacing, and
        // counts toward that site's limits until the attempt ends.
        let mut legs = Vec::with_capacity(work.legs.len());
        for (leg, tried) in work
            .legs
            .iter()
            .zip(leg_trials.iter().copied().chain(std::iter::repeat(false)))
        {
            let booked = self.book(
                now,
                (persona, &persona_seed, factor),
                (&leg.domain, leg.pacing, leg.gap_floor),
                woken,
            );
            let site = self.domains.entry(leg.domain.clone()).or_default();
            prune(&mut site.starts, now, RPM_WINDOW);
            site.starts.push_back(now);
            site.in_flight += 1;
            legs.push(LegDispatch {
                domain: leg.domain.clone(),
                trial: tried,
                breaker: self.leg_breaker_rules(leg),
                starts_visit: booked.starts_visit,
            });
        }

        if !paced {
            let spacing = self
                .rules
                .visit_start_spacing
                .scaled(factor)
                .sample(&mut self.rng);
            // Whole milliseconds, like every other wait.
            #[allow(clippy::cast_possible_truncation)]
            let spacing = Duration::milliseconds(spacing.max(0.0).round() as i64);
            self.next_free_start = Some(now + spacing);
            self.last_spaced = Some(persona);
        }

        let run = self.runs.entry(persona).or_default();
        prune(&mut run.recent, now, LOAD_WINDOW);
        run.recent.push_back(now);
        let grant = GrantId(self.next_grant);
        self.next_grant += 1;
        run.busy = Some(grant);
        match work.render {
            Render::Browser => {
                self.browser_in_flight += 1;
                *self.per_engine.entry(engine).or_insert(0) += 1;
            }
            Render::Http => self.http_in_flight += 1,
        }
        let domain = self.domains.entry(work.domain.clone()).or_default();
        prune(&mut domain.starts, now, RPM_WINDOW);
        domain.starts.push_back(now);
        domain.in_flight += 1;
        self.grants.insert(
            grant,
            Grant {
                persona: Some(persona),
                engine,
                render: work.render,
                domain: Some(work.domain.clone()),
                legs: work.legs.iter().map(|leg| leg.domain.clone()).collect(),
            },
        );

        Dispatch {
            work: ticket,
            grant,
            persona,
            domain: work.domain.clone(),
            render: work.render,
            waited: now.signed_duration_since(queued.since),
            starts_visit,
            page_of_visit,
            ends_burst,
            ends_visit,
            next_slot,
            trial,
            woken,
            pacing_seed,
            breaker: self.breaker_rules(work),
            legs,
        }
    }

    /// Books a page of a persona on a site's pacing: the gap to its next
    /// page there is drawn, and whether the page starts, or ends, its visit.
    fn book(
        &mut self,
        now: DateTime<Utc>,
        (persona, persona_seed, factor): (PersonaId, &Seed, f64),
        (domain, pacing, gap_floor): (&str, PacingProfile, Option<StdDuration>),
        woken: bool,
    ) -> Booked {
        let key = (persona, domain.to_owned());
        let fresh = || {
            let seed = Seed::derive(
                "cadence",
                &[
                    persona_seed.as_bytes(),
                    domain.as_bytes(),
                    &now.timestamp_millis().to_le_bytes(),
                ],
            );
            Cadence {
                sampler: PacingSampler::new(pacing.for_persona(factor), seed),
                seed,
                profile: pacing,
                next_at: now,
                on_break: false,
                visiting: false,
                last_start: now,
                pages: 0,
            }
        };
        let visiting = self
            .cadences
            .get(&key)
            .is_some_and(|cadence| self.is_visiting(persona, cadence, now));
        let cadence = self.cadences.entry(key).or_insert_with(fresh);
        if cadence.profile != pacing {
            // The pacing in force changed: the persona paces anew.
            *cadence = fresh();
        }
        let starts_visit = !visiting;
        if starts_visit {
            cadence.pages = 0;
        }
        let gap = cadence.sampler.next_gap();
        let wait = floored(gap.wait, gap_floor, &cadence.seed, cadence.pages);
        cadence.next_at = now + chrono_of(wait);
        cadence.on_break = gap.after_burst;
        cadence.visiting = !(gap.after_visit || woken);
        cadence.pages = cadence.pages.saturating_add(1);
        cadence.last_start = now;
        Booked {
            starts_visit,
            page_of_visit: cadence.pages,
            next_slot: cadence.next_at,
            pacing_seed: cadence.seed,
            ends_burst: cadence.on_break,
            ends_visit: !cadence.visiting,
        }
    }

    /// Books a plain client's attempt that leaves now: the domain's counts
    /// and the http path's, nothing of a persona's.
    fn dispatch_plain(
        &mut self,
        now: DateTime<Utc>,
        ticket: Ticket,
        queued: &Queued,
    ) -> PlainDispatch {
        let work = &queued.work;
        let grant = GrantId(self.next_grant);
        self.next_grant += 1;
        self.http_in_flight += 1;
        let seed = self.seed;
        let domain = self.domains.entry(work.domain.clone()).or_default();
        prune(&mut domain.starts, now, RPM_WINDOW);
        domain.starts.push_back(now);
        domain.in_flight += 1;
        if let Some(floor) = work.gap_floor {
            // Drawn from what is recorded: the planner's seed, the domain and
            // the moment of this start, which the attempt stores. A restart
            // draws the same share for the same start.
            let seed = Seed::derive(
                "plain-gap",
                &[
                    seed.as_bytes(),
                    work.domain.as_bytes(),
                    &now.timestamp_millis().to_le_bytes(),
                ],
            );
            let wait = floored(StdDuration::ZERO, Some(floor), &seed, 0);
            domain.plain_next = Some(now + chrono_of(wait));
        }
        self.grants.insert(
            grant,
            Grant {
                persona: None,
                // No browser is held; the engine is never read for it.
                engine: Engine::Chromium,
                render: Render::Http,
                domain: Some(work.domain.clone()),
                legs: Vec::new(),
            },
        );
        PlainDispatch {
            work: ticket,
            grant,
            domain: work.domain.clone(),
            waited: now.signed_duration_since(queued.since),
        }
    }

    /// Takes a persona and a browser session for the manager's own work: a
    /// warm-up visit or a scheduled self-test. It never waits and comes
    /// last, after synchronous requests and jobs: it is refused when the
    /// persona is in use, when no session is free, and when it would take
    /// the last free session while work waits for one.
    ///
    /// # Errors
    ///
    /// [`Held`] says why not.
    pub fn reserve(&mut self, persona: PersonaId, honour_stop: bool) -> Result<GrantId, Held> {
        self.take(persona, honour_stop, true)
    }

    /// Takes a persona and a browser session for a self-test an operator
    /// asked for. It counts as a synchronous request: it leaves no session
    /// to work that waits, and no stop holds it. It never waits either.
    ///
    /// # Errors
    ///
    /// [`Held`] when the persona is in use, unknown, or no session is free.
    pub fn reserve_for_operator(&mut self, persona: PersonaId) -> Result<GrantId, Held> {
        self.take(persona, false, false)
    }

    /// A reservation outside the queue. With `yields` the last free session
    /// is left to work that waits for one.
    fn take(
        &mut self,
        persona: PersonaId,
        honour_stop: bool,
        yields: bool,
    ) -> Result<GrantId, Held> {
        if honour_stop && let Some(reason) = self.stop {
            return Err(Held::Stopped(reason));
        }
        let Some(view) = self.personas.get(&persona) else {
            return Err(Held::UnknownPersona);
        };
        let engine = view.engine;
        if self.busy(persona) {
            return Err(Held::PersonaBusy);
        }
        let waiting = yields
            && self
                .queue
                .iter()
                .any(|(_, queued)| queued.work.render == Render::Browser);
        let reserve = u32::from(waiting);
        if self.browser_in_flight + reserve >= self.rules.max_browser_sessions
            || self.engine_load(engine) + reserve >= self.rules.max_per_engine
        {
            return Err(Held::NoSlot);
        }
        let grant = GrantId(self.next_grant);
        self.next_grant += 1;
        self.runs.entry(persona).or_default().busy = Some(grant);
        self.browser_in_flight += 1;
        *self.per_engine.entry(engine).or_insert(0) += 1;
        self.grants.insert(
            grant,
            Grant {
                persona: Some(persona),
                engine,
                render: Render::Browser,
                domain: None,
                legs: Vec::new(),
            },
        );
        Ok(grant)
    }

    /// Reports that an attempt ended at `now`. `false` for a grant the
    /// planner does not hold: it is reported once.
    pub fn complete(&mut self, grant: GrantId, now: DateTime<Utc>, completion: Completion) -> bool {
        let Some(grant) = self.grants.remove(&grant) else {
            return false;
        };
        if let Some(run) = grant
            .persona
            .and_then(|persona| self.runs.get_mut(&persona))
        {
            run.busy = None;
            if completion.proxy_failed {
                run.proxy_failed_at = Some(now);
            } else if completion.courtesy == Courtesy::Answered {
                // Its proxy carried an answer: it is trusted again.
                run.proxy_failed_at = None;
            }
        }
        match grant.render {
            Render::Browser => {
                self.browser_in_flight = self.browser_in_flight.saturating_sub(1);
                if let Some(load) = self.per_engine.get_mut(&grant.engine) {
                    *load = load.saturating_sub(1);
                }
            }
            Render::Http => self.http_in_flight = self.http_in_flight.saturating_sub(1),
        }
        // The later sites of the attempt are free of it again. What the
        // attempt said about a site's health and whether it was turned away
        // there is the domain's: a block on a later site is stored as that
        // site's cooldown of the persona, which holds the persona there.
        for leg in &grant.legs {
            if let Some(site) = self.domains.get_mut(leg) {
                site.in_flight = site.in_flight.saturating_sub(1);
            }
            if let Some(cadence) = grant
                .persona
                .and_then(|persona| self.cadences.get_mut(&(persona, leg.clone())))
            {
                let rest = cadence.sampler.next_rest();
                cadence.next_at = cadence.next_at.max(now + chrono_of(rest));
            }
        }
        let Some(name) = grant.domain else {
            return true;
        };
        if let Some(domain) = self.domains.get_mut(&name) {
            domain.in_flight = domain.in_flight.saturating_sub(1);
            domain
                .errors
                .record(completion.courtesy, now, &self.rules.server_errors);
        }
        let Some(persona) = grant.persona else {
            return true;
        };
        if completion.proxy_failed {
            // A proxy that failed reaches no site: every visit of the
            // persona is over, so that it holds no seat anywhere (F108).
            for ((visitor, _), cadence) in &mut self.cadences {
                if *visitor == persona {
                    cadence.visiting = false;
                }
            }
        }
        if let Some(cadence) = self.cadences.get_mut(&(persona, name)) {
            if completion.blocked || completion.proxy_failed {
                // It leaves: no more pages of this visit. One whose proxy
                // failed cannot reach the site.
                cadence.visiting = false;
            } else {
                // The page is over; the next one is not asked for in the
                // same instant. The gap counted from the page's start and
                // may be long past: the rest counts from its end.
                let rest = cadence.sampler.next_rest();
                cadence.next_at = cadence.next_at.max(now + chrono_of(rest));
            }
        }
        true
    }

    /// When a domain may be asked again after server errors in a row;
    /// `None` when it may be now.
    pub fn server_error_pause(&self, domain: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.domains
            .get(domain)
            .and_then(|run| run.errors.paused_until(now))
    }
}

#[cfg(test)]
mod tests;
