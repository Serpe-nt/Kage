//! How a persona is made, as pure functions (spec §6.1).
//!
//! A persona is a synthetic household behind one proxy. Everything about it
//! that is drawn is drawn from its seed, and every draw is a function of
//! values that are recorded with the persona, so a persona can be explained
//! afterwards and a test can replay it:
//!
//! - [`mix`]: which engine and operating system it runs, kept close to the
//!   configured shares across the pool and inside each country;
//! - [`locale`]: the locale and the language list its browser ships with in
//!   the country its address is in;
//! - [`screen`]: the class of screen it has, from tables of sizes that are
//!   real panels and that the engines' generators hold a real device for;
//! - [`place`]: its time zone and the position it would report;
//! - [`warmup`]: the visits of its first hours;
//! - [`pacing_factor`]: how much slower or faster than the profile it paces
//!   itself.
//!
//! Nothing here talks to a sidecar, a proxy or the store: the persona
//! manager (`kage-persona`) does, with what these functions return.

pub mod locale;
pub mod mix;
pub mod place;
pub mod screen;
pub mod warmup;

use std::time::Duration;

use crate::domain::Engine;
use crate::ids::PersonaId;
use crate::seed::Seed;

/// Version of the tables a persona was drawn from ([`screen`], [`locale`]).
/// It is stored with every persona. Tables only ever change under a new
/// version, and a persona is never drawn again: what it was given at
/// generation it keeps for life (spec §6.2).
pub const TABLES_VERSION: &str = "v1";

/// Slowest and fastest a persona paces itself against its profile.
const PACING_FACTOR_RANGE: (f64, f64) = (0.8, 1.25);

/// The seed of a persona. It is derived from the persona's id (a UUIDv7 with
/// 74 random bits), stored with the persona, and never changes: the
/// fingerprint, the motor profile of the behavior engine and every draw in
/// this module hang on it.
pub fn seed_for(persona: PersonaId) -> Seed {
    Seed::derive("persona", &[persona.as_uuid().as_bytes()])
}

/// The factor a persona scales the median and the 95th percentile of its
/// pacing gaps by: drawn once from its seed, log-uniform in `[0.8, 1.25]`,
/// so that as many personas are faster than the profile as are slower and
/// no two personas share one gap distribution (spec §11).
pub fn pacing_factor(seed: &Seed) -> f64 {
    let (low, high) = PACING_FACTOR_RANGE;
    let unit = seed.child("pacing-factor").rng().unit();
    (low.ln() + (high.ln() - low.ln()) * unit).exp()
}

/// The browser profile directory of a persona, relative to the personas
/// volume. Each engine has its own tree: the two sidecars share the volume
/// and one browser cannot open the other's profile.
pub fn profile_dir(engine: Engine, persona: PersonaId) -> String {
    format!("{}/{persona}", engine.as_str())
}

/// Where a retired persona's profile directory is moved to, relative to the
/// personas volume.
pub fn archive_dir(persona: PersonaId) -> String {
    format!(".archive/{persona}")
}

/// An interval with a persona's own jitter: `interval` times a factor in
/// `[0.75, 1.25)` drawn from the seed, the label and the run number, so
/// that twenty personas never do the same thing on the same schedule.
pub fn jittered(interval: Duration, seed: &Seed, label: &str, run: u64) -> Duration {
    let factor = Seed::derive(label, &[seed.as_bytes(), &run.to_le_bytes()])
        .rng()
        .uniform(0.75, 1.25);
    interval.mul_f64(factor)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{archive_dir, jittered, pacing_factor, profile_dir, seed_for};
    use crate::domain::Engine;
    use crate::ids::PersonaId;
    use crate::seed::Seed;

    #[test]
    fn a_personas_seed_is_its_own_and_always_the_same() {
        let first = PersonaId::new();
        let second = PersonaId::new();

        assert_eq!(seed_for(first), seed_for(first));
        assert_ne!(seed_for(first), seed_for(second));
        assert_eq!(seed_for(first).to_string().len(), 64);
    }

    #[test]
    fn pacing_factors_spread_over_the_range_around_one() {
        let factors: Vec<f64> = (0..2_000)
            .map(|index| pacing_factor(&Seed::from_u64(index)))
            .collect();

        assert!(factors.iter().all(|factor| (0.8..=1.25).contains(factor)));
        // Log-uniform: as many below one as above, and both ends reached.
        let below = factors.iter().filter(|factor| **factor < 1.0).count();
        assert!((900..1_100).contains(&below), "{below}");
        assert!(factors.iter().any(|factor| *factor < 0.82));
        assert!(factors.iter().any(|factor| *factor > 1.22));
        // One persona, one factor.
        let seed = Seed::from_u64(7);
        assert!((pacing_factor(&seed) - pacing_factor(&seed)).abs() < f64::EPSILON);
        // Twenty personas, twenty factors.
        let mut twenty: Vec<u64> = (0..20)
            .map(|index| pacing_factor(&Seed::from_u64(index)).to_bits())
            .collect();
        twenty.sort_unstable();
        twenty.dedup();
        assert_eq!(twenty.len(), 20);
    }

    #[test]
    fn directories_are_named_after_the_engine_and_the_persona() {
        let id: PersonaId = "0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e".parse().unwrap();

        assert_eq!(
            profile_dir(Engine::Chromium, id),
            "chromium/0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e"
        );
        assert_eq!(
            profile_dir(Engine::Firefox, id),
            "firefox/0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e"
        );
        assert_eq!(
            archive_dir(id),
            ".archive/0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e"
        );
    }

    #[test]
    fn a_jittered_interval_stays_within_a_quarter_and_differs_per_run() {
        let day = Duration::from_hours(24);
        let seed = Seed::from_u64(3);

        let runs: Vec<Duration> = (0..200)
            .map(|run| jittered(day, &seed, "selftest", run))
            .collect();

        for wait in &runs {
            assert!(*wait >= day.mul_f64(0.75) && *wait < day.mul_f64(1.25));
        }
        assert_ne!(runs[0], runs[1]);
        assert_eq!(runs[5], jittered(day, &seed, "selftest", 5));
        assert_ne!(runs[5], jittered(day, &seed, "warmup", 5));
        assert_ne!(runs[5], jittered(day, &Seed::from_u64(4), "selftest", 5));
    }
}
