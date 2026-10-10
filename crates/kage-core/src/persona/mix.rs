//! Which engine and operating system a new persona gets (spec §6.1 step 2).
//!
//! The configuration names shares (`personas.mix`): chromium on Linux 40 %,
//! firefox on Windows 45 %, firefox on macOS 15 % by default. A pool is
//! small, twenty proxies and four per country, so independent draws would
//! often miss those shares by a wide margin: a pool with twelve chromium
//! personas, or a country whose four personas all run one engine, while a
//! domain policy may ask for a country and an engine together.
//!
//! The draw is therefore seeded and weighted by what is missing. An entry's
//! deficit is the count it should have once the new persona exists, its
//! share times one more than the personas there are, minus the count it
//! has. Deficits are taken inside the persona's country first, then over
//! the whole pool:
//!
//! 1. only entries with a deficit inside the country can be drawn (there is
//!    always at least one), which keeps every country close to the shares;
//! 2. among those, the draw is weighted by what the pool as a whole is
//!    short of. In an empty pool these weights are the configured shares;
//! 3. when the pool is short of none of them, the one the pool has the
//!    least too many of is taken.
//!
//! Every country of four then has both engines, and a persona generated
//! for a replaced proxy fills the place its predecessor left. Twenty
//! proxies with the default shares yield eight, nine and three personas,
//! exactly in three pools of four and give or take one in nearly all the
//! others: the draw stays a draw, and about one pool in a thousand ends two
//! away for one entry, when two countries were each left with a choice
//! between entries the pool already had enough of.

use serde::{Deserialize, Serialize};

use crate::domain::{ClaimedOs, Engine};
use crate::seed::Seed;

/// One entry of `personas.mix`: an engine, the system its personas claim,
/// and its share of the pool as a weight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MixEntry {
    /// The engine.
    pub engine: Engine,
    /// The operating system the persona claims.
    pub os: ClaimedOs,
    /// The entry's share, relative to the sum of all weights.
    pub weight: u32,
}

impl MixEntry {
    /// Whether a persona is one of this entry's.
    pub fn is(&self, engine: Engine, os: ClaimedOs) -> bool {
        self.engine == engine && self.os == os
    }
}

/// The mix of spec §6.1: chromium on Linux 40 %, firefox on Windows 45 %,
/// firefox on macOS 15 %.
pub fn default_mix() -> Vec<MixEntry> {
    vec![
        MixEntry {
            engine: Engine::Chromium,
            os: ClaimedOs::Linux,
            weight: 40,
        },
        MixEntry {
            engine: Engine::Firefox,
            os: ClaimedOs::Windows,
            weight: 45,
        },
        MixEntry {
            engine: Engine::Firefox,
            os: ClaimedOs::Macos,
            weight: 15,
        },
    ]
}

/// Why a mix cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MixError {
    /// The list is empty.
    #[error("personas.mix must name at least one engine and system")]
    Empty,
    /// No entry has a weight.
    #[error("personas.mix: at least one entry needs a weight above zero")]
    NoWeight,
    /// An engine and system are named twice.
    #[error("personas.mix names {engine} on {os} more than once")]
    Duplicate {
        /// The engine.
        engine: Engine,
        /// The system.
        os: ClaimedOs,
    },
    /// A chromium persona may only claim the system its Chrome runs on.
    #[error(
        "personas.mix names chromium on {os}, but a chromium persona claims the system its \
         Chrome runs on, linux, unless engines.chromium.allow_cross_os_claims is true \
         (docs/adr/0009)"
    )]
    CrossOsClaim {
        /// The system the entry claims.
        os: ClaimedOs,
    },
}

/// Checks a mix: not empty, a weight somewhere, no entry twice, and no
/// chromium entry for another system than Linux unless cross-OS claims are
/// allowed (follow-up F15).
///
/// # Errors
///
/// The first problem.
pub fn validate(mix: &[MixEntry], allow_cross_os_claims: bool) -> Result<(), MixError> {
    if mix.is_empty() {
        return Err(MixError::Empty);
    }
    if mix.iter().all(|entry| entry.weight == 0) {
        return Err(MixError::NoWeight);
    }
    for (index, entry) in mix.iter().enumerate() {
        if mix[..index]
            .iter()
            .any(|earlier| earlier.is(entry.engine, entry.os))
        {
            return Err(MixError::Duplicate {
                engine: entry.engine,
                os: entry.os,
            });
        }
        if entry.engine == Engine::Chromium
            && entry.os != ClaimedOs::Linux
            && entry.weight > 0
            && !allow_cross_os_claims
        {
            return Err(MixError::CrossOsClaim { os: entry.os });
        }
    }
    Ok(())
}

/// How many personas each entry of a mix already has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixCounts {
    /// Per entry, in the order of the mix: personas in the new persona's
    /// country.
    pub in_country: Vec<u32>,
    /// Per entry: personas in the whole pool, that country included.
    pub in_pool: Vec<u32>,
}

impl MixCounts {
    /// Counts for an empty pool.
    pub fn empty(mix: &[MixEntry]) -> Self {
        Self {
            in_country: vec![0; mix.len()],
            in_pool: vec![0; mix.len()],
        }
    }

    /// Counts the personas of a pool, given as (engine, system, in the new
    /// persona's country). A persona of no entry (the mix changed since it
    /// was generated) counts toward the size of the pool but toward no
    /// entry.
    pub fn of<I>(mix: &[MixEntry], personas: I) -> (Self, Totals)
    where
        I: IntoIterator<Item = (Engine, ClaimedOs, bool)>,
    {
        let mut counts = Self::empty(mix);
        let mut totals = Totals::default();
        for (engine, os, same_country) in personas {
            totals.pool += 1;
            if same_country {
                totals.country += 1;
            }
            if let Some(index) = mix.iter().position(|entry| entry.is(engine, os)) {
                counts.in_pool[index] += 1;
                if same_country {
                    counts.in_country[index] += 1;
                }
            }
        }
        (counts, totals)
    }
}

/// How many personas there are, whatever entry they belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Totals {
    /// In the new persona's country.
    pub country: u32,
    /// In the whole pool.
    pub pool: u32,
}

/// Differences below this are rounding, not deficits.
const EPSILON: f64 = 1e-9;

/// What each entry is short of once one more persona exists: its share of
/// `total + 1` minus its count. Negative for an entry that has more than
/// its share.
fn deficits(mix: &[MixEntry], counts: &[u32], total: u32) -> Vec<f64> {
    let weights: f64 = mix.iter().map(|entry| f64::from(entry.weight)).sum();
    mix.iter()
        .zip(counts)
        .map(|(entry, count)| {
            let target = f64::from(entry.weight) / weights * f64::from(total + 1);
            target - f64::from(*count)
        })
        .collect()
}

/// Draws the entry of a new persona. See the module documentation for the
/// rule. Returns the index into `mix`, or `None` for a mix that
/// [`validate`] refuses.
pub fn draw(mix: &[MixEntry], counts: &MixCounts, totals: Totals, seed: &Seed) -> Option<usize> {
    if mix.is_empty()
        || mix.iter().all(|entry| entry.weight == 0)
        || counts.in_country.len() != mix.len()
        || counts.in_pool.len() != mix.len()
    {
        return None;
    }
    // The entries the country is short of. The deficits of a country sum
    // to one, so there is always such an entry; an entry without a weight
    // is never short.
    let mut allowed: Vec<bool> = deficits(mix, &counts.in_country, totals.country)
        .iter()
        .zip(mix)
        .map(|(deficit, entry)| entry.weight > 0 && *deficit > EPSILON)
        .collect();
    if !allowed.contains(&true) {
        allowed = mix.iter().map(|entry| entry.weight > 0).collect();
    }
    let pool = deficits(mix, &counts.in_pool, totals.pool);
    // Among them, weighted by what the pool is short of.
    let mut weights: Vec<f64> = pool
        .iter()
        .zip(&allowed)
        .map(|(deficit, allowed)| if *allowed { deficit.max(0.0) } else { 0.0 })
        .collect();
    if weights.iter().sum::<f64>() <= EPSILON {
        // The pool is short of none of them: the one it has the least too
        // many of, and the seed decides between equals.
        let least = pool
            .iter()
            .zip(&allowed)
            .filter(|(_, allowed)| **allowed)
            .map(|(deficit, _)| *deficit)
            .fold(f64::NEG_INFINITY, f64::max);
        weights = pool
            .iter()
            .zip(&allowed)
            .map(|(deficit, allowed)| {
                if *allowed && *deficit >= least - EPSILON {
                    1.0
                } else {
                    0.0
                }
            })
            .collect();
    }
    let total: f64 = weights.iter().sum();
    let mut threshold = seed.child("persona-mix").rng().unit() * total;
    let mut chosen = None;
    for (index, weight) in weights.iter().enumerate() {
        if *weight <= 0.0 {
            continue;
        }
        chosen = Some(index);
        threshold -= weight;
        if threshold < 0.0 {
            break;
        }
    }
    chosen
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{MixCounts, MixEntry, MixError, Totals, default_mix, draw, validate};
    use crate::domain::{ClaimedOs, Engine};
    use crate::seed::Seed;

    const COUNTRIES: [&str; 5] = ["AT", "DE", "FR", "GB", "IT"];

    /// Generates a pool of `per_country` personas in each of five
    /// countries, in an order drawn from the seed, and returns per country
    /// the entry each persona got.
    fn pool(mix: &[MixEntry], per_country: usize, seed: u64) -> BTreeMap<&'static str, Vec<usize>> {
        // The proxies become healthy in some order.
        let mut order: Vec<&str> = COUNTRIES
            .iter()
            .flat_map(|country| std::iter::repeat_n(*country, per_country))
            .collect();
        let mut shuffle = Seed::from_u64(seed).child("order").rng();
        for index in (1..order.len()).rev() {
            let bound = u64::try_from(index).unwrap();
            let other = usize::try_from(shuffle.int_inclusive(0, bound)).unwrap();
            order.swap(index, other);
        }

        let mut personas: Vec<(&str, usize)> = Vec::new();
        for (number, country) in order.into_iter().enumerate() {
            let (counts, totals) = MixCounts::of(
                mix,
                personas
                    .iter()
                    .map(|(home, entry)| (mix[*entry].engine, mix[*entry].os, *home == country)),
            );
            let persona_seed = Seed::derive("test", &[&seed.to_le_bytes(), &number.to_le_bytes()]);
            let entry = draw(mix, &counts, totals, &persona_seed).unwrap();
            personas.push((country, entry));
        }
        let mut by_country: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for (country, entry) in personas {
            by_country.entry(country).or_default().push(entry);
        }
        by_country
    }

    /// The acceptance criterion of milestone M6: a pool of twenty yields
    /// the configured mix within tolerance, over repeated runs. The
    /// tolerance: within one persona of 8, 9 and 3 in at least 99 % of the
    /// pools and never more than two away; both engines in every country;
    /// the configured shares over all runs.
    #[test]
    fn twenty_proxies_yield_the_configured_mix_within_one_persona() {
        let mix = default_mix();
        let mut totals = [0_u32; 3];
        let runs = 2_000;
        let mut spread: BTreeMap<i64, u32> = BTreeMap::new();
        let mut shapes: BTreeMap<[u32; 3], u32> = BTreeMap::new();

        for seed in 0..runs {
            let by_country = pool(&mix, 4, seed);
            let mut counts = [0_u32; 3];
            for entries in by_country.values() {
                for entry in entries {
                    counts[*entry] += 1;
                }
                // Every country of four has both engines.
                let chromium = entries
                    .iter()
                    .filter(|entry| mix[**entry].engine == Engine::Chromium)
                    .count();
                assert!(
                    (1..=3).contains(&chromium),
                    "seed {seed}: a country has {chromium} chromium personas of four"
                );
            }
            // 40 %, 45 % and 15 % of twenty are 8, 9 and 3.
            let off = counts
                .iter()
                .zip([8_i64, 9, 3])
                .map(|(count, target)| (i64::from(*count) - target).abs())
                .max()
                .unwrap_or(0);
            *spread.entry(off).or_insert(0_u32) += 1;
            *shapes.entry(counts).or_insert(0_u32) += 1;
            for (total, count) in totals.iter_mut().zip(counts) {
                *total += count;
            }
        }

        assert!(
            spread.keys().all(|off| *off <= 2),
            "pools more than two away from [8, 9, 3]: {spread:?}; {shapes:?}"
        );
        let within_one: u32 = spread
            .iter()
            .filter(|(off, _)| **off <= 1)
            .map(|(_, pools)| *pools)
            .sum();
        assert!(
            f64::from(within_one) >= 0.99 * f64::from(u32::try_from(runs).unwrap()),
            "only {within_one} of {runs} pools within one of [8, 9, 3]: {spread:?}; {shapes:?}"
        );
        // The exact mix is by far the most common outcome.
        assert!(
            shapes[&[8, 9, 3]] > u32::try_from(runs).unwrap() / 2,
            "{shapes:?}"
        );

        // And over all runs the shares are the configured ones.
        let all = f64::from(u32::try_from(runs).unwrap() * 20);
        for (total, share) in totals.iter().zip([0.40, 0.45, 0.15]) {
            let observed = f64::from(*total) / all;
            assert!(
                (observed - share).abs() < 0.01,
                "share {observed:.3} instead of {share}"
            );
        }
    }

    #[test]
    fn the_draw_is_a_function_of_the_seed_and_the_pool() {
        let mix = default_mix();
        let counts = MixCounts::empty(&mix);
        let totals = Totals::default();
        let seed = Seed::from_u64(11);

        assert_eq!(
            draw(&mix, &counts, totals, &seed),
            draw(&mix, &counts, totals, &seed)
        );
        // An empty pool draws by the plain shares.
        let mut drawn = [0_u32; 3];
        for index in 0..10_000 {
            let entry = draw(&mix, &counts, totals, &Seed::from_u64(index)).unwrap();
            drawn[entry] += 1;
        }
        assert!((3_800..4_200).contains(&drawn[0]), "{drawn:?}");
        assert!((4_300..4_700).contains(&drawn[1]), "{drawn:?}");
        assert!((1_350..1_650).contains(&drawn[2]), "{drawn:?}");
    }

    #[test]
    fn a_replacement_fills_the_place_its_predecessor_left() {
        let mix = default_mix();
        // A full pool of twenty with the firefox-on-macos persona of one
        // country retired: three countries have one, the pool has two.
        let counts = MixCounts {
            in_country: vec![2, 1, 0],
            in_pool: vec![8, 9, 2],
        };
        let totals = Totals {
            country: 3,
            pool: 19,
        };
        for index in 0..500 {
            assert_eq!(
                draw(&mix, &counts, totals, &Seed::from_u64(index)),
                Some(2),
                "seed {index}"
            );
        }
    }

    #[test]
    fn a_pool_of_one_engine_is_possible_by_configuration() {
        let only_chromium = vec![MixEntry {
            engine: Engine::Chromium,
            os: ClaimedOs::Linux,
            weight: 1,
        }];
        assert_eq!(validate(&only_chromium, false), Ok(()));
        for seed in 0..50 {
            let by_country = pool(&only_chromium, 4, seed);
            assert!(by_country.values().flatten().all(|entry| *entry == 0));
        }

        // An entry with weight zero is never drawn.
        let mut mix = default_mix();
        mix[1].weight = 0;
        for seed in 0..200 {
            let by_country = pool(&mix, 4, seed);
            assert!(by_country.values().flatten().all(|entry| *entry != 1));
        }
    }

    #[test]
    fn personas_of_no_entry_count_toward_the_pool_only() {
        let mix = default_mix();
        let (counts, totals) = MixCounts::of(
            &mix,
            [
                (Engine::Chromium, ClaimedOs::Linux, true),
                (Engine::Firefox, ClaimedOs::Windows, false),
                // Generated under an earlier mix.
                (Engine::Firefox, ClaimedOs::Linux, true),
            ],
        );

        assert_eq!(counts.in_country, [1, 0, 0]);
        assert_eq!(counts.in_pool, [1, 1, 0]);
        assert_eq!(
            totals,
            Totals {
                country: 2,
                pool: 3
            }
        );
        assert!(draw(&mix, &counts, totals, &Seed::from_u64(1)).is_some());
    }

    #[test]
    fn a_mix_that_cannot_be_used_is_refused_and_never_drawn_from() {
        let chromium_on = |os| MixEntry {
            engine: Engine::Chromium,
            os,
            weight: 1,
        };
        assert_eq!(validate(&[], false), Err(MixError::Empty));
        assert_eq!(
            validate(
                &[MixEntry {
                    weight: 0,
                    ..chromium_on(ClaimedOs::Linux)
                }],
                false
            ),
            Err(MixError::NoWeight)
        );
        assert_eq!(
            validate(
                &[chromium_on(ClaimedOs::Linux), chromium_on(ClaimedOs::Linux)],
                false
            ),
            Err(MixError::Duplicate {
                engine: Engine::Chromium,
                os: ClaimedOs::Linux
            })
        );
        // Follow-up F15: the Rust side reads the same switch as the sidecar.
        let cross = [
            chromium_on(ClaimedOs::Linux),
            chromium_on(ClaimedOs::Windows),
        ];
        let refused = validate(&cross, false).unwrap_err();
        assert_eq!(
            refused,
            MixError::CrossOsClaim {
                os: ClaimedOs::Windows
            }
        );
        assert!(refused.to_string().contains("allow_cross_os_claims"));
        assert_eq!(validate(&cross, true), Ok(()));
        // A firefox persona may claim any system, linux included.
        assert_eq!(
            validate(
                &[MixEntry {
                    engine: Engine::Firefox,
                    os: ClaimedOs::Linux,
                    weight: 5
                }],
                false
            ),
            Ok(())
        );
        assert_eq!(validate(&default_mix(), false), Ok(()));

        let seed = Seed::from_u64(1);
        assert_eq!(
            draw(&[], &MixCounts::empty(&[]), Totals::default(), &seed),
            None
        );
        let zero = [MixEntry {
            weight: 0,
            ..chromium_on(ClaimedOs::Linux)
        }];
        assert_eq!(
            draw(&zero, &MixCounts::empty(&zero), Totals::default(), &seed),
            None
        );
        // Counts for another mix are not counts for this one.
        let mix = default_mix();
        assert_eq!(
            draw(&mix, &MixCounts::empty(&zero), Totals::default(), &seed),
            None
        );
    }
}
