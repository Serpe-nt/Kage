//! `robots.txt` as the pipeline keeps to it (spec §14).
//!
//! A reader never asks for `/robots.txt`, so the request marks the address
//! that makes it. Three things follow, and this module holds the rules of
//! all three; the pipeline does the asking.
//!
//! - **Whether a site's file is asked for at all** is its policy's
//!   (`robots: ignore | report | enforce`, `ignore` unless said). A
//!   request may ask for more care than the policy takes and never for
//!   less, and never causes a request the policy would not make
//!   ([`mode`]).
//! - **Who asks** is the site's scout: one persona per site (a
//!   registrable domain), which asks for the `robots.txt` of each of the
//!   site's origins and for its sitemaps and never reads a page of it, so
//!   that one address is marked and no other ([`pick_scout`]).
//! - **What is kept** is one answer per origin until it is due again
//!   ([`entry_of`], [`Known`]): the file, or that there is none (a `4xx`:
//!   everything is allowed), or that it could not be had (a server error,
//!   a timeout, a block: nothing is known, and where the policy enforces
//!   nothing of the origin is fetched until it is asked for again).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use kage_core::config::{Config, RobotsConfig};
use kage_core::domain::{Engine, RobotsVerdict};
use kage_core::ids::PersonaId;
use kage_core::outcome::Outcome;
use kage_core::policy::{EffectivePolicy, RobotsMode};
use kage_core::schedule::planner::PersonaView;
use kage_core::seed::Seed;
use kage_extract::robots::Robots;
use kage_store::repo::RobotsCacheEntry;
use tokio::time::Instant;
use url::Url;

/// How long the pipeline trusts what it holds in memory (a site's scout,
/// an origin's file) before it looks at the store again: a row an operator
/// removed is gone from memory within this time.
const RECHECK: Duration = Duration::from_secs(30);

/// The most origins whose parsed file is held in memory; beyond it what
/// has expired goes, and then everything: the store has them all.
const MAX_KNOWN: usize = 4096;

/// What a request says about `robots.txt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RobotsWish {
    /// What the site's policy says.
    #[default]
    Policy,
    /// Report the verdict and fetch whatever it says.
    Report,
    /// Do not fetch what the file disallows.
    Enforce,
    /// Do not look at the file.
    Ignore,
}

/// Why a request's wish about `robots.txt` cannot be had. The names are
/// the error codes of the API and the `reason` of a `robots_denied`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    /// The site's policy enforces its `robots.txt`; a request cannot ask
    /// for less.
    Enforced,
    /// The site's policy never asks for its `robots.txt`, so there is
    /// nothing a request could enforce.
    Ignored,
}

impl Conflict {
    /// The stable name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enforced => "robots_enforced",
            Self::Ignored => "robots_ignored",
        }
    }
}

/// What applies to a fetch: the policy's mode, as far as the request's
/// wish may move it. A request never loosens `enforce` and never makes
/// Kage ask a site for a file its policy leaves alone.
///
/// # Errors
///
/// The [`Conflict`] between the wish and the policy.
pub const fn mode(policy: RobotsMode, wish: RobotsWish) -> Result<RobotsMode, Conflict> {
    match (policy, wish) {
        (_, RobotsWish::Policy) => Ok(policy),
        // Where the policy reports, a request may have the file enforced:
        // the policy has it asked for either way.
        (RobotsMode::Enforce | RobotsMode::Report, RobotsWish::Enforce) => Ok(RobotsMode::Enforce),
        (RobotsMode::Enforce, RobotsWish::Report | RobotsWish::Ignore) => Err(Conflict::Enforced),
        (RobotsMode::Report, RobotsWish::Report) => Ok(RobotsMode::Report),
        // No verdict is wanted, and none is looked up; the file is still
        // the policy's to ask for when another request wants it.
        (RobotsMode::Report | RobotsMode::Ignore, RobotsWish::Ignore)
        // Nothing will ever be known of the file: the answer says so.
        | (RobotsMode::Ignore, RobotsWish::Report) => Ok(RobotsMode::Ignore),
        (RobotsMode::Ignore, RobotsWish::Enforce) => Err(Conflict::Ignored),
    }
}

/// How the pipeline keeps `robots.txt` (the configuration's `robots`
/// section).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobotsSettings {
    /// How long an answer is kept, before it is moved by up to a tenth.
    pub ttl: Duration,
    /// How long an origin is left alone after its file could not be had.
    pub error_ttl: Duration,
    /// How much of a file is read, in bytes.
    pub max_bytes: u64,
    /// The product tokens Kage answers to besides `*`.
    pub agents: Vec<String>,
}

impl RobotsSettings {
    /// The settings of the configuration.
    pub fn from_config(config: &Config) -> Self {
        Self::of(&config.robots)
    }

    fn of(robots: &RobotsConfig) -> Self {
        Self {
            ttl: robots.ttl.get(),
            error_ttl: robots.error_ttl.get(),
            max_bytes: robots.max_bytes,
            agents: robots.agents.clone(),
        }
    }
}

impl Default for RobotsSettings {
    /// What the configuration says when it says nothing.
    fn default() -> Self {
        Self::of(&RobotsConfig::default())
    }
}

/// The origin of a URL as `robots.txt` has one: `scheme://host[:port]` in
/// lower case, the port left out when it is the scheme's own. `None` for a
/// URL without a host.
pub fn origin_of(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    Some(match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

/// Where the `robots.txt` of a URL's origin is.
pub fn file_of(url: &Url) -> Option<Url> {
    Url::parse(&format!("{}/robots.txt", origin_of(url)?)).ok()
}

/// What a scout brought back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// The file.
    File(String),
    /// There is none: the site answered with a `4xx` that is no block.
    Missing,
    /// It could not be had: a server error, a timeout, a block.
    Unreachable,
}

impl Answer {
    /// What an attempt for a `robots.txt` came to. `body` is the body of
    /// the main response as the site sent it; what a file has beyond
    /// `max_bytes` is left out.
    pub fn of(outcome: Outcome, status: Option<u16>, body: Option<&[u8]>, max_bytes: u64) -> Self {
        if outcome.is_block() {
            return Self::Unreachable;
        }
        match status {
            Some(200..=299) if outcome.is_success() => {
                let body = body.unwrap_or_default();
                let most = usize::try_from(max_bytes).unwrap_or(usize::MAX);
                // Cut at the byte: a character that is cut in two is
                // replaced, and the line it stands in is the last one.
                let kept = &body[..body.len().min(most)];
                Self::File(String::from_utf8_lossy(kept).into_owned())
            }
            Some(400..=499) => Self::Missing,
            _ => Self::Unreachable,
        }
    }
}

/// The cache row for what a scout brought back at `now`. An answer is
/// kept for `ttl` moved by up to a tenth either way, drawn from the origin
/// and the moment, so that the files of many sites are not asked for on a
/// grid; "could not be had" is kept for `error_ttl` as it is.
pub fn entry_of(
    origin: &str,
    site: &str,
    answer: &Answer,
    status: Option<u16>,
    now: DateTime<Utc>,
    settings: &RobotsSettings,
) -> RobotsCacheEntry {
    let keep = match answer {
        Answer::Unreachable => settings.error_ttl,
        Answer::File(_) | Answer::Missing => {
            let share = Seed::derive(
                "robots-ttl",
                &[origin.as_bytes(), &now.timestamp_millis().to_le_bytes()],
            )
            .rng()
            .uniform(0.9, 1.1);
            settings.ttl.mul_f64(share)
        }
    };
    let expires_at = chrono::Duration::from_std(keep)
        .ok()
        .and_then(|keep| now.checked_add_signed(keep))
        .unwrap_or(now);
    RobotsCacheEntry {
        origin: origin.to_owned(),
        site: site.to_owned(),
        fetched_at: now,
        expires_at,
        status,
        body: match answer {
            Answer::File(body) => Some(body.clone()),
            Answer::Missing | Answer::Unreachable => None,
        },
        unreachable: *answer == Answer::Unreachable,
    }
}

/// What a kept answer says.
#[derive(Debug, Clone, PartialEq)]
pub enum Standing {
    /// The rules of the file.
    Rules(Robots),
    /// The origin has no `robots.txt`: everything is allowed.
    Missing,
    /// The file could not be had: nothing is known.
    Unreachable,
}

/// What is known of the `robots.txt` of one origin, read once.
#[derive(Debug, Clone, PartialEq)]
pub struct Known {
    /// When the file was asked for.
    pub fetched_at: DateTime<Utc>,
    /// When it is due again.
    pub expires_at: DateTime<Utc>,
    /// What it says.
    pub standing: Standing,
}

impl Known {
    /// A cache row, read.
    pub fn of(entry: &RobotsCacheEntry) -> Self {
        let standing = match (&entry.body, entry.unreachable) {
            (_, true) => Standing::Unreachable,
            (Some(body), false) => Standing::Rules(Robots::parse(body)),
            (None, false) => Standing::Missing,
        };
        Self {
            fetched_at: entry.fetched_at,
            expires_at: entry.expires_at,
            standing,
        }
    }

    /// Whether the answer still stands at `now`.
    pub fn fresh(&self, now: DateTime<Utc>) -> bool {
        self.expires_at > now
    }

    /// What the file says of an address, for whoever answers to `agents`.
    /// `None` when the file could not be had: nothing is known.
    pub fn verdict(&self, agents: &[String], url: &Url) -> Option<RobotsVerdict> {
        match &self.standing {
            Standing::Rules(robots) => {
                let decision = robots.decide(agents, url);
                Some(RobotsVerdict {
                    fetched: true,
                    allowed: decision.allowed,
                    matched_rule: decision.matched_rule,
                    crawl_delay: decision.crawl_delay,
                })
            }
            Standing::Missing => Some(RobotsVerdict {
                fetched: true,
                allowed: true,
                matched_rule: None,
                crawl_delay: None,
            }),
            Standing::Unreachable => None,
        }
    }

    /// The sitemaps the file names, as written.
    pub fn sitemaps(&self) -> &[String] {
        match &self.standing {
            Standing::Rules(robots) => robots.sitemaps(),
            Standing::Missing | Standing::Unreachable => &[],
        }
    }
}

/// The verdict of a fetch that knows nothing of the file: it was not
/// asked for, or not yet.
pub const fn unasked() -> RobotsVerdict {
    RobotsVerdict {
        fetched: false,
        allowed: true,
        matched_rule: None,
        crawl_delay: None,
    }
}

/// The least gap a `Crawl-delay` asks for between two pages, when it asks
/// for one.
pub fn gap_floor(verdict: &RobotsVerdict) -> Option<Duration> {
    verdict
        .crawl_delay
        .filter(|delay| *delay > 0.0)
        .and_then(|delay| Duration::try_from_secs_f64(delay).ok())
}

/// Who asks a site for what no reader asks for, in this order:
///
/// 1. the persona that asked before (`remembered`), ready or not: a
///    second address is never marked while the first one lives;
/// 2. a ready persona the site's policy keeps from reading it anyway
///    (another country, another engine), which costs the site no reader;
/// 3. a ready persona that never read a page of the site, as long as
///    `readers_needed` others that may read it stay.
///
/// Never a persona that read the site (`readers`). Among equals a Firefox
/// goes first (it shows a gzipped sitemap where Chrome saves it as a
/// file), and then the choice is drawn from the site's name, so that the
/// scouts of many sites are not one persona. `None` when nobody can ask.
pub fn pick_scout(
    site: &str,
    remembered: Option<PersonaId>,
    personas: &[PersonaView],
    readers: &[PersonaId],
    policy: &EffectivePolicy,
    readers_needed: usize,
) -> Option<PersonaId> {
    if let Some(scout) = remembered.filter(|scout| personas.iter().any(|view| view.id == *scout)) {
        return Some(scout);
    }
    let admitted = |view: &PersonaView| {
        policy.engines.contains(&view.engine)
            && policy.countries.as_ref().is_none_or(|countries| {
                view.country
                    .is_some_and(|country| countries.contains(&country))
            })
    };
    let free: Vec<&PersonaView> = personas
        .iter()
        .filter(|view| view.ready && !readers.contains(&view.id))
        .collect();
    let rank = |view: &PersonaView| {
        let drawn = Seed::derive("scout", &[site.as_bytes(), view.id.as_uuid().as_bytes()])
            .rng()
            .next_u64();
        (view.engine != Engine::Firefox, drawn, view.id)
    };
    let outside = free
        .iter()
        .filter(|view| !admitted(view))
        .min_by_key(|view| rank(view));
    if let Some(scout) = outside {
        return Some(scout.id);
    }
    // One of the site's own: whoever is left has to be enough to read it.
    let able = personas
        .iter()
        .filter(|view| view.ready && admitted(view))
        .count();
    if able <= readers_needed {
        return None;
    }
    free.iter()
        .min_by_key(|view| rank(view))
        .map(|view| view.id)
}

/// The scout of a site as the desk last looked it up: `None` inside for a
/// site that has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Scouted(pub(crate) Option<PersonaId>);

/// What the pipeline remembers between two fetches: the files it read,
/// who scouts which site, and what is being asked for right now.
#[derive(Debug, Default)]
pub struct Desk {
    known: Mutex<HashMap<String, (Instant, Arc<Known>)>>,
    scouts: Mutex<HashMap<String, (Instant, Option<PersonaId>)>>,
    /// One request per origin at a time: whoever comes second waits for
    /// the first one's answer.
    asking: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// The origins a scout is under way for though no fetch waits for it.
    following: Mutex<HashSet<String>>,
    /// One choice of a scout at a time, so that two origins of a site that
    /// are first read at the same moment get the same one.
    pub(crate) choosing: tokio::sync::Mutex<()>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Desk {
    /// What is held of an origin's file, while the store was looked at
    /// recently enough.
    pub(crate) fn known(&self, origin: &str) -> Option<Arc<Known>> {
        locked(&self.known)
            .get(origin)
            .filter(|(at, _)| at.elapsed() < RECHECK)
            .map(|(_, known)| Arc::clone(known))
    }

    /// What is held of an origin's file, however long ago the store was
    /// looked at: the file as it was read, for a row that has not changed.
    pub(crate) fn held(&self, origin: &str) -> Option<Arc<Known>> {
        locked(&self.known)
            .get(origin)
            .map(|(_, known)| Arc::clone(known))
    }

    /// Holds what the store has of an origin's file, as of now.
    pub(crate) fn keep(&self, origin: &str, known: Arc<Known>, now: DateTime<Utc>) -> Arc<Known> {
        let mut held = locked(&self.known);
        if held.len() >= MAX_KNOWN && !held.contains_key(origin) {
            held.retain(|_, (_, known)| known.fresh(now));
            if held.len() >= MAX_KNOWN {
                held.clear();
            }
        }
        held.insert(origin.to_owned(), (Instant::now(), Arc::clone(&known)));
        drop(held);
        known
    }

    /// The store has nothing of an origin's file any more.
    pub(crate) fn forget(&self, origin: &str) {
        locked(&self.known).remove(origin);
    }

    /// The scout of a site as it was last looked up, while that is recent
    /// enough; `None` when it has to be looked up again.
    pub(crate) fn scout(&self, site: &str) -> Option<Scouted> {
        locked(&self.scouts)
            .get(site)
            .filter(|(at, _)| at.elapsed() < RECHECK)
            .map(|(_, scout)| Scouted(*scout))
    }

    /// Notes who scouts a site.
    pub(crate) fn note_scout(&self, site: &str, scout: Option<PersonaId>) {
        let mut scouts = locked(&self.scouts);
        scouts.retain(|_, (at, _)| at.elapsed() < RECHECK);
        scouts.insert(site.to_owned(), (Instant::now(), scout));
    }

    /// The lock of one origin's request.
    pub(crate) fn asking(&self, origin: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut asking = locked(&self.asking);
        // A lock nobody holds or waits for is only its entry's.
        asking.retain(|_, lock| Arc::strong_count(lock) > 1);
        Arc::clone(asking.entry(origin.to_owned()).or_default())
    }

    /// Marks an origin as followed up. Returns whether it was not yet.
    pub(crate) fn follow(&self, origin: &str) -> bool {
        locked(&self.following).insert(origin.to_owned())
    }

    /// The follow-up of an origin is over.
    pub(crate) fn followed(&self, origin: &str) {
        locked(&self.following).remove(origin);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, TimeZone, Utc};
    use kage_core::domain::{CountryCode, Engine, RobotsVerdict};
    use kage_core::ids::PersonaId;
    use kage_core::outcome::Outcome;
    use kage_core::policy::{DomainPolicies, RobotsMode};
    use kage_core::schedule::planner::PersonaView;
    use kage_core::schedule::selection::Network;
    use kage_core::seed::Seed;
    use url::Url;
    use uuid::Uuid;

    use super::{
        Answer, Conflict, Desk, Known, MAX_KNOWN, RobotsSettings, RobotsWish, Scouted, Standing,
        entry_of, file_of, gap_floor, mode, origin_of, pick_scout, unasked,
    };

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 13, 9, 0, 0).unwrap()
    }

    fn settings() -> RobotsSettings {
        RobotsSettings {
            ttl: Duration::from_hours(24),
            error_ttl: Duration::from_hours(1),
            max_bytes: 512 * 1024,
            agents: vec!["kage".to_owned()],
        }
    }

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn a_request_may_ask_for_more_care_than_the_policy_and_never_for_less() {
        use RobotsMode::{Enforce, Ignore, Report};
        let cases = [
            (Ignore, RobotsWish::Policy, Ok(Ignore)),
            (Ignore, RobotsWish::Ignore, Ok(Ignore)),
            // Nothing will be known: the answer says `fetched: false`.
            (Ignore, RobotsWish::Report, Ok(Ignore)),
            (Ignore, RobotsWish::Enforce, Err(Conflict::Ignored)),
            (Report, RobotsWish::Policy, Ok(Report)),
            (Report, RobotsWish::Report, Ok(Report)),
            (Report, RobotsWish::Enforce, Ok(Enforce)),
            (Report, RobotsWish::Ignore, Ok(Ignore)),
            (Enforce, RobotsWish::Policy, Ok(Enforce)),
            (Enforce, RobotsWish::Enforce, Ok(Enforce)),
            (Enforce, RobotsWish::Report, Err(Conflict::Enforced)),
            (Enforce, RobotsWish::Ignore, Err(Conflict::Enforced)),
        ];
        for (policy, wish, expected) in cases {
            assert_eq!(mode(policy, wish), expected, "{policy:?} {wish:?}");
        }
        assert_eq!(RobotsWish::default(), RobotsWish::Policy);
        assert_eq!(Conflict::Enforced.as_str(), "robots_enforced");
        assert_eq!(Conflict::Ignored.as_str(), "robots_ignored");
    }

    #[test]
    fn an_origin_is_a_scheme_a_host_and_a_port_that_is_not_the_schemes_own() {
        for (address, origin, file) in [
            (
                "https://WWW.Example.org/a/b?c=d#e",
                "https://www.example.org",
                "https://www.example.org/robots.txt",
            ),
            (
                "https://example.org:443/",
                "https://example.org",
                "https://example.org/robots.txt",
            ),
            (
                "http://example.org:8080/x",
                "http://example.org:8080",
                "http://example.org:8080/robots.txt",
            ),
            (
                "http://[2001:db8::1]:81/x",
                "http://[2001:db8::1]:81",
                "http://[2001:db8::1]:81/robots.txt",
            ),
        ] {
            let address = url(address);
            assert_eq!(origin_of(&address).as_deref(), Some(origin));
            assert_eq!(file_of(&address), Some(url(file)));
        }
        assert_eq!(origin_of(&url("data:text/plain,hello")), None);
        assert_eq!(file_of(&url("mailto:a@example.org")), None);
    }

    #[test]
    fn what_a_scout_brought_back_is_a_file_or_none_or_nothing_known() {
        let file = b"User-agent: *\nDisallow: /private\n";
        let of = |outcome, status, body: Option<&[u8]>| Answer::of(outcome, status, body, 1024);
        assert_eq!(
            of(Outcome::Ok, Some(200), Some(file)),
            Answer::File(String::from_utf8_lossy(file).into_owned())
        );
        assert_eq!(
            of(Outcome::ChallengePassed, Some(299), Some(b"")),
            Answer::File(String::new())
        );
        // A page without a body is an empty file: everything is allowed.
        assert_eq!(
            of(Outcome::Ok, Some(204), None),
            Answer::File(String::new())
        );
        // No file: every client error that is no block.
        for status in [400, 401, 404, 410, 451, 499] {
            assert_eq!(of(Outcome::Ok, Some(status), Some(file)), Answer::Missing);
        }
        // A block, whatever its status, and everything else: nothing known.
        for (outcome, status) in [
            (Outcome::Forbidden, Some(403)),
            (Outcome::RateLimited, Some(429)),
            (Outcome::SoftBlock, Some(200)),
            (Outcome::Captcha, Some(200)),
            (Outcome::ChallengeFailed, Some(403)),
            (Outcome::ProviderError, Some(500)),
            (Outcome::ProviderError, Some(503)),
            (Outcome::ProviderError, Some(200)),
            (Outcome::Timeout, None),
            (Outcome::Ok, None),
            (Outcome::Ok, Some(199)),
            (Outcome::Ok, Some(300)),
            (Outcome::Ok, Some(399)),
            (Outcome::Ok, Some(500)),
        ] {
            assert_eq!(
                of(outcome, status, Some(file)),
                Answer::Unreachable,
                "{outcome:?} {status:?}"
            );
        }
        // What a file has beyond the limit is left out, at the byte.
        assert_eq!(
            Answer::of(Outcome::Ok, Some(200), Some(file), 13),
            Answer::File("User-agent: *".to_owned())
        );
        assert_eq!(
            Answer::of(Outcome::Ok, Some(200), Some(file), file.len() as u64),
            Answer::File(String::from_utf8_lossy(file).into_owned())
        );
        // A character cut in two is replaced, never a failure.
        assert_eq!(
            Answer::of(Outcome::Ok, Some(200), Some("é".as_bytes()), 1),
            Answer::File("\u{fffd}".to_owned())
        );
    }

    #[test]
    fn an_answer_is_kept_for_about_the_ttl_and_a_failure_for_the_error_ttl() {
        let settings = settings();
        let day = chrono::Duration::hours(24);
        let file = Answer::File("User-agent: *\nDisallow: /x\n".to_owned());
        let mut kept = Vec::new();
        for number in 0..200 {
            let origin = format!("https://site{number}.example");
            let entry = entry_of(&origin, "example", &file, Some(200), at(), &settings);
            assert_eq!(
                (entry.origin.as_str(), entry.site.as_str()),
                (origin.as_str(), "example")
            );
            assert_eq!(entry.fetched_at, at());
            assert_eq!(entry.status, Some(200));
            assert_eq!(entry.body.as_deref(), Some("User-agent: *\nDisallow: /x\n"));
            assert!(!entry.unreachable);
            let keep = entry.expires_at - at();
            assert!(keep >= day * 9 / 10 && keep < day * 11 / 10, "{keep}");
            // The same origin at the same moment: the same answer.
            assert_eq!(
                entry_of(&origin, "example", &file, Some(200), at(), &settings),
                entry
            );
            kept.push(keep);
        }
        // No grid: the sites differ, on both sides of the day.
        kept.sort_unstable();
        kept.dedup();
        assert!(kept.len() > 190);
        assert!(kept[0] < day * 92 / 100 && kept[kept.len() - 1] > day * 108 / 100);
        // Another moment, another draw for the same origin.
        let again = entry_of(
            "https://site0.example",
            "example",
            &file,
            Some(200),
            at() + chrono::Duration::milliseconds(1),
            &settings,
        );
        assert_ne!(again.expires_at - again.fetched_at, kept[0]);

        let missing = entry_of(
            "https://a.example",
            "a.example",
            &Answer::Missing,
            Some(404),
            at(),
            &settings,
        );
        assert_eq!((missing.body, missing.unreachable), (None, false));
        let keep = missing.expires_at - at();
        assert!(keep >= day * 9 / 10 && keep < day * 11 / 10);

        let down = entry_of(
            "https://a.example",
            "a.example",
            &Answer::Unreachable,
            None,
            at(),
            &settings,
        );
        assert_eq!(
            (down.body, down.unreachable, down.status),
            (None, true, None)
        );
        assert_eq!(down.expires_at - at(), chrono::Duration::hours(1));
    }

    #[test]
    fn a_kept_answer_says_what_the_file_says_of_an_address() {
        let settings = settings();
        let body = "User-agent: *\nDisallow: /private\nCrawl-delay: 2.5\nSitemap: https://example.org/map.xml\n";
        let entry = entry_of(
            "https://example.org",
            "example.org",
            &Answer::File(body.to_owned()),
            Some(200),
            at(),
            &settings,
        );
        let known = Known::of(&entry);
        assert!(matches!(known.standing, Standing::Rules(_)));
        assert_eq!(
            (known.fetched_at, known.expires_at),
            (at(), entry.expires_at)
        );
        assert!(known.fresh(at()));
        assert!(known.fresh(entry.expires_at - chrono::Duration::milliseconds(1)));
        assert!(!known.fresh(entry.expires_at));
        assert_eq!(known.sitemaps(), ["https://example.org/map.xml"]);
        assert_eq!(
            known.verdict(&settings.agents, &url("https://example.org/private/x")),
            Some(RobotsVerdict {
                fetched: true,
                allowed: false,
                matched_rule: Some("Disallow: /private".to_owned()),
                crawl_delay: Some(2.5),
            })
        );
        let open = known
            .verdict(&settings.agents, &url("https://example.org/public"))
            .unwrap();
        assert!(open.fetched && open.allowed);
        assert_eq!((open.matched_rule, open.crawl_delay), (None, Some(2.5)));

        // No file: asked, and everything is allowed.
        let missing = Known::of(&entry_of(
            "https://example.org",
            "example.org",
            &Answer::Missing,
            Some(404),
            at(),
            &settings,
        ));
        assert_eq!(missing.standing, Standing::Missing);
        assert_eq!(
            missing.verdict(&settings.agents, &url("https://example.org/private")),
            Some(RobotsVerdict {
                fetched: true,
                allowed: true,
                matched_rule: None,
                crawl_delay: None,
            })
        );
        assert_eq!(missing.sitemaps().len(), 0);

        // Could not be had: nothing is known, whatever body the row has.
        let mut row = entry;
        row.unreachable = true;
        let down = Known::of(&row);
        assert_eq!(down.standing, Standing::Unreachable);
        assert_eq!(
            down.verdict(&settings.agents, &url("https://example.org/")),
            None
        );
        assert_eq!(down.sitemaps().len(), 0);

        assert_eq!(
            unasked(),
            RobotsVerdict {
                fetched: false,
                allowed: true,
                matched_rule: None,
                crawl_delay: None,
            }
        );
    }

    #[test]
    fn a_crawl_delay_is_a_floor_when_it_asks_for_time() {
        let with = |crawl_delay| RobotsVerdict {
            crawl_delay,
            ..unasked()
        };
        assert_eq!(gap_floor(&with(None)), None);
        assert_eq!(gap_floor(&with(Some(0.0))), None);
        assert_eq!(gap_floor(&with(Some(-1.0))), None);
        assert_eq!(gap_floor(&with(Some(f64::NAN))), None);
        assert_eq!(gap_floor(&with(Some(f64::INFINITY))), None);
        assert_eq!(
            gap_floor(&with(Some(0.5))),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            gap_floor(&with(Some(3600.0))),
            Some(Duration::from_hours(1))
        );
    }

    fn id(number: u8) -> PersonaId {
        PersonaId::from_uuid(Uuid::from_u128(u128::from(number) + 1))
    }

    fn view(number: u8, engine: Engine, country: &str, ready: bool) -> PersonaView {
        PersonaView {
            id: id(number),
            seed: Seed::from_u64(u64::from(number)),
            engine,
            country: country.parse::<CountryCode>().ok(),
            timezone: "Europe/Berlin".to_owned(),
            ready,
            http: false,
            network: Network::of(None, None),
        }
    }

    // One pool looked at under every rule in turn.
    #[allow(clippy::too_many_lines)]
    #[test]
    fn the_scout_of_a_site_is_whoever_asked_before_then_an_outsider_then_a_stranger() {
        let open = DomainPolicies::default().resolve("example.org");
        let germans = DomainPolicies::from_yaml(
            "domains:\n  example.org: { countries: [DE], engines: [chromium] }\n",
        )
        .unwrap()
        .resolve("example.org");
        let pool = [
            view(0, Engine::Chromium, "DE", true),
            view(1, Engine::Chromium, "DE", true),
            view(2, Engine::Chromium, "FR", true),
            view(3, Engine::Firefox, "DE", true),
            view(4, Engine::Firefox, "FR", false),
        ];

        // Whoever asked before, ready or not, reader or not.
        for remembered in 0..5 {
            assert_eq!(
                pick_scout(
                    "example.org",
                    Some(id(remembered)),
                    &pool,
                    &[id(remembered)],
                    &germans,
                    9
                ),
                Some(id(remembered))
            );
        }
        // One that is gone is nobody's scout: the next rule applies.
        assert_eq!(
            pick_scout("example.org", Some(id(77)), &pool, &[], &germans, 1),
            Some(id(3))
        );

        // An outsider of the policy costs the site no reader: the Firefox
        // before the French Chrome, and never one that is not ready.
        assert_eq!(
            pick_scout("example.org", None, &pool, &[], &germans, 1),
            Some(id(3))
        );
        assert_eq!(
            pick_scout("example.org", None, &pool, &[id(3)], &germans, 1),
            Some(id(2))
        );
        // However many readers the site needs: an outsider is none of them.
        assert_eq!(
            pick_scout("example.org", None, &pool, &[id(3)], &germans, 50),
            Some(id(2))
        );
        // No outsider left: one of the site's own that never read it, as
        // long as enough of them stay to read it.
        assert_eq!(
            pick_scout(
                "example.org",
                None,
                &pool,
                &[id(2), id(3), id(0)],
                &germans,
                1
            ),
            Some(id(1))
        );
        assert_eq!(
            pick_scout(
                "example.org",
                None,
                &pool,
                &[id(2), id(3), id(0)],
                &germans,
                2
            ),
            None
        );
        // Everybody read it: nobody asks.
        assert_eq!(
            pick_scout(
                "example.org",
                None,
                &pool,
                &[id(0), id(1), id(2), id(3)],
                &germans,
                1
            ),
            None
        );

        // A policy that admits everybody has no outsider: a stranger, the
        // Firefox first, while the readers it needs stay.
        assert_eq!(
            pick_scout("example.org", None, &pool, &[], &open, 1),
            Some(id(3))
        );
        assert_eq!(
            pick_scout("example.org", None, &pool, &[], &open, 3),
            Some(id(3))
        );
        assert_eq!(pick_scout("example.org", None, &pool, &[], &open, 4), None);
        // A pool of one keeps its reader.
        let alone = [view(0, Engine::Chromium, "DE", true)];
        assert_eq!(pick_scout("example.org", None, &alone, &[], &open, 1), None);
        assert_eq!(pick_scout("example.org", None, &[], &[], &open, 0), None);

        // Among equals the site's name draws: many sites, more than one
        // scout, and the same site always the same one.
        let chromes: Vec<_> = (0..8)
            .map(|n| view(n, Engine::Chromium, "DE", true))
            .collect();
        let mut chosen = std::collections::BTreeSet::new();
        for number in 0..64 {
            let site = format!("site{number}.example");
            let scout = pick_scout(&site, None, &chromes, &[], &open, 1).unwrap();
            assert_eq!(
                pick_scout(&site, None, &chromes, &[], &open, 1),
                Some(scout)
            );
            chosen.insert(scout);
        }
        assert!(chosen.len() >= 6, "{chosen:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_desk_remembers_files_and_scouts_for_a_while() {
        let desk = Desk::default();
        let settings = settings();
        let known = |origin: &str, now| {
            std::sync::Arc::new(Known::of(&entry_of(
                origin,
                "example.org",
                &Answer::Missing,
                Some(404),
                now,
                &settings,
            )))
        };

        assert!(desk.known("https://example.org").is_none());
        assert!(desk.held("https://example.org").is_none());
        let kept = desk.keep(
            "https://example.org",
            known("https://example.org", at()),
            at(),
        );
        assert_eq!(desk.known("https://example.org"), Some(kept.clone()));
        assert!(desk.known("http://example.org").is_none());
        // After half a minute the store is looked at again; what was read
        // is still held for a row that has not changed.
        tokio::time::advance(Duration::from_secs(29)).await;
        assert!(desk.known("https://example.org").is_some());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(desk.known("https://example.org").is_none());
        assert_eq!(desk.held("https://example.org"), Some(kept.clone()));
        desk.keep("https://example.org", kept, at());
        assert!(desk.known("https://example.org").is_some());
        desk.forget("https://example.org");
        assert!(desk.held("https://example.org").is_none());
        desk.keep(
            "https://example.org",
            known("https://example.org", at()),
            at(),
        );

        // Full: what has expired goes first, and what is fresh stays.
        let late = at() + chrono::Duration::days(2);
        for number in 0..MAX_KNOWN - 1 {
            let origin = format!("https://old{number}.example");
            desk.keep(&origin, known(&origin, at()), at());
        }
        desk.keep(
            "https://new.example",
            known("https://new.example", late),
            late,
        );
        assert!(desk.known("https://new.example").is_some());
        assert!(desk.known("https://old7.example").is_none());
        assert!(desk.known("https://example.org").is_none());
        // Full of what is fresh: everything goes, the store has it all.
        for number in 0..MAX_KNOWN {
            let origin = format!("https://fresh{number}.example");
            desk.keep(&origin, known(&origin, late), late);
        }
        assert!(desk.known("https://new.example").is_none());
        assert!(
            desk.known(&format!("https://fresh{}.example", MAX_KNOWN - 1))
                .is_some()
        );
        // What is held already is kept again without anything going.
        let last = format!("https://fresh{}.example", MAX_KNOWN - 1);
        for number in 0..MAX_KNOWN - 1 {
            let origin = format!("https://more{number}.example");
            desk.keep(&origin, known(&origin, late), late);
        }
        desk.keep(&last, known(&last, late), late);
        assert!(desk.known("https://more0.example").is_some());

        // A scout is trusted for half a minute, then looked up again.
        assert_eq!(desk.scout("example.org"), None);
        desk.note_scout("example.org", Some(id(3)));
        desk.note_scout("example.net", None);
        assert_eq!(desk.scout("example.org"), Some(Scouted(Some(id(3)))));
        assert_eq!(desk.scout("example.net"), Some(Scouted(None)));
        tokio::time::advance(Duration::from_secs(29)).await;
        assert_eq!(desk.scout("example.org"), Some(Scouted(Some(id(3)))));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(desk.scout("example.org"), None);

        // One request per origin: the same lock while somebody holds it.
        let first = desk.asking("https://example.org");
        let second = desk.asking("https://example.org");
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        let other = desk.asking("https://example.net");
        assert!(!std::sync::Arc::ptr_eq(&first, &other));
        let held = first.lock().await;
        assert!(second.try_lock().is_err());
        drop(held);

        // An origin is followed up once at a time.
        assert!(desk.follow("https://example.org"));
        assert!(!desk.follow("https://example.org"));
        desk.followed("https://example.org");
        assert!(desk.follow("https://example.org"));
    }
}
