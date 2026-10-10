//! Choosing a persona for a request (spec §8.2).
//!
//! Who may serve at all (ready, the right engine and country, not cooling,
//! under its cap) is decided by the planner. Among those the score decides:
//!
//! | Why | Points |
//! |---|---|
//! | the persona's country is one the request prefers | +3 |
//! | it has not visited this domain in the last hour | +2 |
//! | it is among the least loaded | +1 |
//! | its last attempt on this domain was a success | +1 |
//! | it shares the autonomous system of a persona this request was just blocked on | −2 |
//!
//! Ties are drawn. Two things rank before the score: a persona that is
//! awake is taken before one that sleeps, and a persona in the same /24 as
//! one the request was just blocked on is taken only when there is no
//! other, because a block followed by the same request from the next
//! address is what a rotating proxy looks like.

use std::net::IpAddr;

use crate::ids::PersonaId;
use crate::seed::SeededRng;

/// Points for a country the request prefers.
pub const COUNTRY_POINTS: i32 = 3;
/// Points for no visit to the domain in the last hour.
pub const FRESH_POINTS: i32 = 2;
/// Points for being among the least loaded.
pub const LOAD_POINTS: i32 = 1;
/// Points for a success as the last outcome on the domain.
pub const SUCCESS_POINTS: i32 = 1;
/// Points taken for the autonomous system of a persona just blocked.
pub const SAME_ASN_PENALTY: i32 = 2;

/// Where a persona's address sits: what two personas can have in common
/// that a site sees.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Network {
    /// The /24 of an IPv4 exit address, or the /48 of an IPv6 one.
    pub subnet: Option<String>,
    /// The autonomous system, for example `AS3320`.
    pub asn: Option<String>,
}

impl Network {
    /// The network of an exit address and its autonomous system.
    pub fn of(address: Option<IpAddr>, asn: Option<&str>) -> Self {
        let subnet = address.map(|address| match address {
            IpAddr::V4(v4) => {
                let [a, b, c, _] = v4.octets();
                format!("{a}.{b}.{c}.0/24")
            }
            IpAddr::V6(v6) => {
                let [a, b, c, ..] = v6.segments();
                format!("{a:x}:{b:x}:{c:x}::/48")
            }
        });
        Self {
            subnet,
            asn: asn
                .map(str::trim)
                .filter(|asn| !asn.is_empty())
                .map(str::to_ascii_uppercase),
        }
    }

    /// Whether both addresses are known to be in one subnet.
    pub fn shares_subnet(&self, other: &Self) -> bool {
        self.subnet.is_some() && self.subnet == other.subnet
    }

    /// Whether both addresses are known to be in one autonomous system.
    pub fn shares_asn(&self, other: &Self) -> bool {
        self.asn.is_some() && self.asn == other.asn
    }
}

/// One persona that may serve a request, as the score sees it.
// One flag per line of the score table.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The persona.
    pub persona: PersonaId,
    /// Its country is one the request prefers.
    pub country_preferred: bool,
    /// It visited the domain in the last hour.
    pub visited_last_hour: bool,
    /// What it did lately: dispatches in the last ten minutes.
    pub recent_load: u32,
    /// Its last attempt on the domain was a success.
    pub previous_ok: bool,
    /// It is awake.
    pub awake: bool,
    /// It shares the /24 of a persona this request was blocked on.
    pub same_subnet_as_blocked: bool,
    /// It shares the autonomous system of such a persona.
    pub same_asn_as_blocked: bool,
}

/// The score of a candidate. `least_load` is the smallest load among the
/// candidates it competes with.
pub fn score(candidate: &Candidate, least_load: u32) -> i32 {
    let mut points = 0;
    if candidate.country_preferred {
        points += COUNTRY_POINTS;
    }
    if !candidate.visited_last_hour {
        points += FRESH_POINTS;
    }
    if candidate.recent_load <= least_load {
        points += LOAD_POINTS;
    }
    if candidate.previous_ok {
        points += SUCCESS_POINTS;
    }
    if candidate.same_asn_as_blocked {
        points -= SAME_ASN_PENALTY;
    }
    points
}

/// The index of the candidate to take, or `None` when there is none.
///
/// With `prefer_awake` a sleeping persona is only taken when no candidate
/// is awake. A candidate in the subnet of a blocked persona is only taken
/// when every candidate is. Among the rest the highest score wins and ties
/// are drawn from `rng`.
pub fn choose(candidates: &[Candidate], prefer_awake: bool, rng: &mut SeededRng) -> Option<usize> {
    let mut pool: Vec<usize> = (0..candidates.len()).collect();
    if prefer_awake && pool.iter().any(|&index| candidates[index].awake) {
        pool.retain(|&index| candidates[index].awake);
    }
    if pool
        .iter()
        .any(|&index| !candidates[index].same_subnet_as_blocked)
    {
        pool.retain(|&index| !candidates[index].same_subnet_as_blocked);
    }
    let least_load = pool
        .iter()
        .map(|&index| candidates[index].recent_load)
        .min()?;
    let best = pool
        .iter()
        .map(|&index| score(&candidates[index], least_load))
        .max()?;
    pool.retain(|&index| score(&candidates[index], least_load) == best);
    let last = u64::try_from(pool.len().checked_sub(1)?).ok()?;
    let drawn = usize::try_from(rng.int_inclusive(0, last)).ok()?;
    pool.get(drawn).copied()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::IpAddr;

    use super::{Candidate, Network, choose, score};
    use crate::ids::PersonaId;
    use crate::seed::Seed;

    fn plain() -> Candidate {
        Candidate {
            persona: PersonaId::new(),
            country_preferred: false,
            visited_last_hour: true,
            recent_load: 5,
            previous_ok: false,
            awake: true,
            same_subnet_as_blocked: false,
            same_asn_as_blocked: false,
        }
    }

    fn with(change: impl FnOnce(&mut Candidate)) -> Candidate {
        let mut candidate = plain();
        change(&mut candidate);
        candidate
    }

    #[test]
    fn the_score_is_the_specs() {
        // Nothing speaks for it: visited lately, more loaded than the least.
        assert_eq!(score(&plain(), 0), 0);
        assert_eq!(score(&with(|c| c.country_preferred = true), 0), 3);
        assert_eq!(score(&with(|c| c.visited_last_hour = false), 0), 2);
        assert_eq!(score(&with(|c| c.previous_ok = true), 0), 1);
        // Among the least loaded: at the least load, or below it.
        assert_eq!(score(&plain(), 5), 1);
        assert_eq!(score(&plain(), 6), 1);
        assert_eq!(score(&plain(), 4), 0);
        assert_eq!(score(&with(|c| c.same_asn_as_blocked = true), 0), -2);
        // Awake and the subnet rank before the score and are not in it.
        assert_eq!(score(&with(|c| c.awake = false), 0), 0);
        assert_eq!(score(&with(|c| c.same_subnet_as_blocked = true), 0), 0);

        let everything = with(|c| {
            c.country_preferred = true;
            c.visited_last_hour = false;
            c.previous_ok = true;
            c.recent_load = 0;
        });
        assert_eq!(score(&everything, 0), 7);
    }

    #[test]
    fn the_highest_score_wins() {
        let mut rng = Seed::from_u64(1).rng();
        let candidates = [
            plain(),
            with(|c| c.visited_last_hour = false),
            with(|c| c.country_preferred = true),
            with(|c| c.previous_ok = true),
        ];

        for _ in 0..50 {
            assert_eq!(choose(&candidates, true, &mut rng), Some(2));
        }
        assert_eq!(choose(&[], true, &mut rng), None);
        assert_eq!(choose(&candidates[..1], true, &mut rng), Some(0));
    }

    #[test]
    fn the_least_loaded_gets_its_point_among_those_that_compete() {
        let mut rng = Seed::from_u64(2).rng();
        let candidates = [
            with(|c| c.recent_load = 9),
            with(|c| c.recent_load = 3),
            with(|c| c.recent_load = 4),
        ];

        for _ in 0..50 {
            assert_eq!(choose(&candidates, true, &mut rng), Some(1));
        }
    }

    #[test]
    fn ties_are_drawn_and_every_equal_candidate_gets_its_turn() {
        let mut rng = Seed::from_u64(3).rng();
        let candidates = [plain(), plain(), plain(), with(|c| c.recent_load = 6)];
        let mut drawn: BTreeMap<usize, u32> = BTreeMap::new();

        for _ in 0..3_000 {
            *drawn
                .entry(choose(&candidates, true, &mut rng).unwrap())
                .or_default() += 1;
        }

        // The three equal ones share the draws; the busier one never wins.
        assert_eq!(drawn.keys().copied().collect::<Vec<_>>(), [0, 1, 2]);
        assert!(
            drawn.values().all(|count| (850..1150).contains(count)),
            "{drawn:?}"
        );
        // The same seed draws the same.
        let replay = |seed| {
            let mut rng = Seed::from_u64(seed).rng();
            (0..20)
                .map(|_| choose(&candidates, true, &mut rng).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(replay(8), replay(8));
        assert_ne!(replay(8), replay(9));
    }

    #[test]
    fn an_awake_persona_is_taken_before_a_better_sleeping_one() {
        let mut rng = Seed::from_u64(4).rng();
        let sleeper = with(|c| {
            c.awake = false;
            c.country_preferred = true;
            c.visited_last_hour = false;
        });
        let candidates = [sleeper.clone(), plain()];

        assert_eq!(choose(&candidates, true, &mut rng), Some(1));
        // When the hour does not matter the score decides.
        assert_eq!(choose(&candidates, false, &mut rng), Some(0));
        // No one is awake: the sleeper serves.
        assert_eq!(
            choose(std::slice::from_ref(&sleeper), true, &mut rng),
            Some(0)
        );
        // Load is compared among those that compete: the sleeper's low load
        // does not take the point from the one that is awake.
        let rested = with(|c| {
            c.awake = false;
            c.recent_load = 0;
        });
        assert_eq!(
            choose(
                &[rested, plain(), with(|c| c.recent_load = 7)],
                true,
                &mut rng
            ),
            Some(1)
        );
    }

    #[test]
    fn the_subnet_of_a_blocked_persona_is_the_last_resort() {
        let mut rng = Seed::from_u64(5).rng();
        let neighbour = with(|c| {
            c.same_subnet_as_blocked = true;
            c.country_preferred = true;
            c.visited_last_hour = false;
        });
        let same_asn = with(|c| c.same_asn_as_blocked = true);
        let candidates = [neighbour.clone(), same_asn.clone(), plain()];

        // The neighbour is out although it scores highest; of the other
        // two the one outside the autonomous system wins.
        for _ in 0..50 {
            assert_eq!(choose(&candidates, true, &mut rng), Some(2));
        }
        // Without anyone else the autonomous system is only a penalty.
        assert_eq!(
            choose(&[neighbour.clone(), same_asn], true, &mut rng),
            Some(1)
        );
        // And the neighbour serves when it is the only one.
        assert_eq!(choose(&[neighbour], true, &mut rng), Some(0));
    }

    #[test]
    fn a_network_is_a_subnet_and_an_autonomous_system() {
        let address = |text: &str| Some(text.parse::<IpAddr>().unwrap());
        let a = Network::of(address("203.0.113.7"), Some("AS64500"));
        let b = Network::of(address("203.0.113.200"), Some(" as64500 "));
        let c = Network::of(address("203.0.114.7"), Some("AS64500"));
        let d = Network::of(address("198.51.100.1"), Some("AS64501"));

        assert_eq!(a.subnet.as_deref(), Some("203.0.113.0/24"));
        assert_eq!(b.asn.as_deref(), Some("AS64500"));
        assert!(a.shares_subnet(&b) && a.shares_asn(&b));
        assert!(!a.shares_subnet(&c) && a.shares_asn(&c));
        assert!(!a.shares_subnet(&d) && !a.shares_asn(&d));

        let six = Network::of(address("2001:db8:12:34::1"), None);
        assert_eq!(six.subnet.as_deref(), Some("2001:db8:12::/48"));
        assert!(six.shares_subnet(&Network::of(address("2001:db8:12:ffff::9"), None)));
        assert!(!six.shares_subnet(&Network::of(address("2001:db8:13::1"), None)));

        // Nothing known is nothing shared.
        let unknown = Network::of(None, Some("  "));
        assert_eq!(unknown, Network::default());
        assert!(!unknown.shares_subnet(&Network::default()));
        assert!(!unknown.shares_asn(&Network::default()));
        assert!(!unknown.shares_subnet(&a) && !a.shares_asn(&unknown));
    }
}
