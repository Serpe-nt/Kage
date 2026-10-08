//! Seeded, reproducible randomness.
//!
//! Randomness that affects behavior or fingerprints is seeded and the seed is
//! recorded, so a test or an incident analysis can replay it (house rule in
//! `CLAUDE.md`). Two things make a replay exact:
//!
//! - the generator is `ChaCha8`, a named algorithm whose output for a seed
//!   does not change between releases (unlike `StdRng`);
//! - every derived quantity (unit floats, bounded integers, normal deviates)
//!   is computed here from raw 64-bit words, not by a library distribution
//!   whose algorithm may change on a dependency bump.

use std::fmt;
use std::str::FromStr;

use rand::rngs::ChaCha8Rng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

/// Prefix hashed into every derived seed; bump it to change every stream.
const DERIVATION_TAG: &[u8] = b"kage-seed-v1";

/// 2^-53: scales a 53-bit integer onto the unit interval.
const UNIT_SCALE: f64 = 1.0 / 9_007_199_254_740_992.0;

/// A 256-bit seed. Printed and stored as 64 hexadecimal characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Seed([u8; 32]);

impl Seed {
    /// Uses the given bytes as the seed.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The seed bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Derives a seed from a label and any number of byte strings, for
    /// example a persona id and a request id. The same inputs always give
    /// the same seed; each part is length-prefixed, so `["ab", "c"]` and
    /// `["a", "bc"]` give different seeds.
    pub fn derive(label: &str, parts: &[&[u8]]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(DERIVATION_TAG);
        absorb(&mut hasher, label.as_bytes());
        for part in parts {
            absorb(&mut hasher, part);
        }
        Self(hasher.finalize().into())
    }

    /// Derives a seed from a number. Meant for tests and examples.
    pub fn from_u64(value: u64) -> Self {
        Self::derive("u64", &[&value.to_le_bytes()])
    }

    /// Derives an independent seed for a named purpose, so one recorded seed
    /// can drive several unrelated streams.
    #[must_use]
    pub fn child(&self, label: &str) -> Self {
        Self::derive(label, &[&self.0])
    }

    /// Starts the random stream for this seed.
    pub fn rng(&self) -> SeededRng {
        SeededRng(ChaCha8Rng::from_seed(self.0))
    }
}

fn absorb(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

impl fmt::Display for Seed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Seed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Seed({self})")
    }
}

/// Why a seed string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a seed is 64 hexadecimal characters")]
pub struct SeedParseError;

impl FromStr for Seed {
    type Err = SeedParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.as_bytes();
        if text.len() != 64 {
            return Err(SeedParseError);
        }
        let mut bytes = [0_u8; 32];
        let (pairs, _) = text.as_chunks::<2>();
        for (byte, pair) in bytes.iter_mut().zip(pairs) {
            let pair = std::str::from_utf8(pair).map_err(|_| SeedParseError)?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| SeedParseError)?;
        }
        Ok(Self(bytes))
    }
}

impl Serialize for Seed {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Seed {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// A deterministic random stream started from a [`Seed`].
pub struct SeededRng(ChaCha8Rng);

impl fmt::Debug for SeededRng {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SeededRng(..)")
    }
}

impl SeededRng {
    /// The next raw 64-bit word.
    pub fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    /// A float in `[0, 1)` with 53 random bits.
    pub fn unit(&mut self) -> f64 {
        // The top 53 bits fit an f64 mantissa exactly, so the cast is lossless.
        #[allow(clippy::cast_precision_loss)]
        let mantissa = (self.next_u64() >> 11) as f64;
        mantissa * UNIT_SCALE
    }

    /// A float in `[min, max)`; returns `min` when the range is empty.
    pub fn uniform(&mut self, min: f64, max: f64) -> f64 {
        if max > min {
            min + (max - min) * self.unit()
        } else {
            min
        }
    }

    /// An integer in `[min, max]` without modulo bias; returns `min` when
    /// `max < min`.
    pub fn int_inclusive(&mut self, min: u64, max: u64) -> u64 {
        if max <= min {
            return min;
        }
        let Some(span) = (max - min).checked_add(1) else {
            // The range is all of u64.
            return self.next_u64();
        };
        // Largest multiple of `span` that fits; values at or above it are
        // rejected so every residue is equally likely.
        let limit = u64::MAX - (u64::MAX % span + 1) % span;
        loop {
            let value = self.next_u64();
            if value <= limit {
                return min + value % span;
            }
        }
    }

    /// `true` with probability `p` (clamped to `[0, 1]`).
    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }

    /// A standard normal deviate (mean 0, variance 1) by the Box–Muller
    /// transform.
    pub fn standard_normal(&mut self) -> f64 {
        // `1 - unit()` is in (0, 1], so the logarithm is finite.
        let radius = (-2.0 * (1.0 - self.unit()).ln()).sqrt();
        let angle = std::f64::consts::TAU * self.unit();
        radius * angle.cos()
    }
}

#[cfg(test)]
mod tests {
    use super::{Seed, SeedParseError};

    #[test]
    fn derivation_is_deterministic_and_separates_inputs() {
        let a = Seed::derive("gap", &[b"persona", b"request"]);

        assert_eq!(a, Seed::derive("gap", &[b"persona", b"request"]));
        assert_ne!(a, Seed::derive("dwell", &[b"persona", b"request"]));
        assert_ne!(a, Seed::derive("gap", &[b"persona", b"other"]));
        // Length prefixes keep part boundaries significant.
        assert_ne!(
            Seed::derive("x", &[b"ab", b"c"]),
            Seed::derive("x", &[b"a", b"bc"])
        );
        assert_ne!(
            Seed::derive("x", &[b"ab"]),
            Seed::derive("x", &[b"a", b"b"])
        );
    }

    #[test]
    fn child_seeds_are_independent_of_each_other() {
        let root = Seed::from_u64(7);

        assert_eq!(root.child("gap"), root.child("gap"));
        assert_ne!(root.child("gap"), root.child("break"));
        assert_ne!(root.child("gap"), root);
    }

    #[test]
    fn hex_round_trips() {
        let seed = Seed::from_u64(42);
        let text = seed.to_string();

        assert_eq!(text.len(), 64);
        assert_eq!(text.parse::<Seed>().unwrap(), seed);
        assert_eq!(format!("{seed:?}"), format!("Seed({text})"));

        let json = serde_json::to_string(&seed).unwrap();
        assert_eq!(serde_json::from_str::<Seed>(&json).unwrap(), seed);
    }

    #[test]
    fn malformed_hex_is_rejected() {
        assert_eq!("abc".parse::<Seed>(), Err(SeedParseError));
        assert_eq!("zz".repeat(32).parse::<Seed>(), Err(SeedParseError));
        assert_eq!("é".repeat(32).parse::<Seed>(), Err(SeedParseError));
        assert!(serde_json::from_str::<Seed>("\"nope\"").is_err());
    }

    /// Pins the stream for a known seed. If this fails after a dependency
    /// bump, recorded seeds no longer replay: do not update the numbers
    /// without an ADR.
    #[test]
    fn stream_is_value_stable() {
        let seed = Seed::from_bytes([7; 32]);
        assert_eq!(seed.as_bytes(), &[7; 32]);
        let mut rng = seed.rng();

        let words: Vec<u64> = (0..4).map(|_| rng.next_u64()).collect();

        assert_eq!(
            words, GOLDEN_WORDS,
            "ChaCha8 output changed for a fixed seed"
        );
        assert_eq!(Seed::from_u64(1).to_string(), GOLDEN_SEED_FROM_ONE);
    }

    const GOLDEN_WORDS: [u64; 4] = [
        4_115_934_089_738_703_076,
        15_345_232_379_140_719_590,
        5_365_187_766_710_272_727,
        6_528_134_485_496_232_873,
    ];
    const GOLDEN_SEED_FROM_ONE: &str =
        "93dcf9bbd265ca86c30d911edfef9d988cc37c9d267ddeced8370b04bb35f750";

    #[test]
    fn the_same_seed_replays_the_same_stream() {
        let seed = Seed::from_u64(99);
        let mut first = seed.rng();
        let mut second = seed.rng();

        for _ in 0..100 {
            let value = first.next_u64();
            assert_eq!(value, second.next_u64());
        }
        assert_eq!(format!("{first:?}"), "SeededRng(..)");
    }

    #[test]
    fn unit_floats_stay_in_the_half_open_interval() {
        let mut rng = Seed::from_u64(1).rng();

        let mut sum = 0.0;
        for _ in 0..20_000 {
            let value = rng.unit();
            assert!((0.0..1.0).contains(&value));
            sum += value;
        }
        let mean = sum / 20_000.0;
        assert!((mean - 0.5).abs() < 0.01, "mean {mean}");
    }

    #[test]
    fn uniform_respects_bounds_and_empty_ranges() {
        let mut rng = Seed::from_u64(2).rng();

        for _ in 0..5_000 {
            let value = rng.uniform(300.0, 1200.0);
            assert!((300.0..1200.0).contains(&value));
        }
        assert!((rng.uniform(5.0, 5.0) - 5.0).abs() < f64::EPSILON);
        assert!((rng.uniform(9.0, 1.0) - 9.0).abs() < f64::EPSILON);
    }

    #[test]
    fn bounded_integers_cover_the_range_evenly() {
        let mut rng = Seed::from_u64(3).rng();
        let mut counts = [0_u32; 11];

        for _ in 0..22_000 {
            let value = rng.int_inclusive(4, 14);
            assert!((4..=14).contains(&value));
            counts[usize::try_from(value - 4).unwrap()] += 1;
        }

        for (offset, count) in counts.iter().enumerate() {
            assert!(
                (1_700..2_300).contains(count),
                "value {} drawn {count} times",
                offset + 4
            );
        }
    }

    #[test]
    fn bounded_integers_handle_degenerate_ranges() {
        let mut rng = Seed::from_u64(4).rng();

        assert_eq!(rng.int_inclusive(50, 50), 50);
        assert_eq!(rng.int_inclusive(9, 3), 9);
        // The full range must not overflow while computing the span.
        let _ = rng.int_inclusive(0, u64::MAX);
        for _ in 0..1_000 {
            assert!(rng.int_inclusive(u64::MAX - 1, u64::MAX) >= u64::MAX - 1);
        }
    }

    #[test]
    fn chance_follows_its_probability() {
        let mut rng = Seed::from_u64(5).rng();

        let hits = (0..20_000).filter(|_| rng.chance(0.25)).count();
        assert!((4_600..5_400).contains(&hits), "{hits}");
        assert!(!rng.chance(0.0));
        assert!(rng.chance(1.0));
    }

    #[test]
    fn standard_normal_has_zero_mean_and_unit_variance() {
        let mut rng = Seed::from_u64(6).rng();
        let samples: Vec<f64> = (0..50_000).map(|_| rng.standard_normal()).collect();

        let count = 50_000.0;
        let mean = samples.iter().sum::<f64>() / count;
        let variance = samples.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / count;

        assert!(samples.iter().all(|x| x.is_finite()));
        assert!(mean.abs() < 0.02, "mean {mean}");
        assert!((variance - 1.0).abs() < 0.03, "variance {variance}");
    }
}
