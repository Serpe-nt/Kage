//! What a page says, reduced to two numbers (spec §13).
//!
//! [`content_hash`] tells whether two pages say exactly the same thing: the
//! SHA-256 of the page's text with its white space collapsed. The text is
//! used and not the body, because the body of one page differs from fetch to
//! fetch (a nonce in a script, a token in a form, a time in a comment) while
//! what a reader sees stays the same.
//!
//! [`simhash`] tells whether two pages say nearly the same thing: a 64-bit
//! fingerprint in which similar texts differ in few bits (Charikar's
//! SimHash). The features are runs of three words, each counted as often as
//! it occurs. Two fingerprints are compared with [`distance`]; a crawl flags
//! a page as a near-duplicate of an earlier one when they are at most
//! [`NEAR_DUPLICATE_BITS`] bits apart.
//!
//! Both are pure functions of the text and never change for a given text:
//! the hash of a feature is spelled out here and does not depend on the
//! standard library's hasher, whose values are not promised from one release
//! to the next.

use sha2::{Digest, Sha256};

/// Words in one feature of the fingerprint.
const SHINGLE: usize = 3;

/// Two pages at most this many bits apart are near-duplicates, unless a
/// crawl is told another number.
///
/// Measured (M10, consult 5): of texts of 400 words with one word in a
/// hundred changed, 95 in 100 are within 6 bits and 57 within 3; with one
/// word in 25 changed, 61 and 11; with one in 12, 23 and 2. Unrelated texts
/// are 32 bits apart at the median and were never closer than 18 in 3000
/// pairs; two pages that share half of their main text never closer than
/// 10 in 1000, two that share seven tenths within 6 in three pairs of 1000.
/// On the fixture site the two printings of one article, with another date
/// line and one sentence more, are 5 bits apart, and no two other pages
/// fewer than 22.
pub const NEAR_DUPLICATE_BITS: u32 = 6;

/// A text shorter than this many words has no fingerprint: a few words in
/// common are no likeness, and every short page of a site would be flagged
/// as a copy of every other.
pub const MIN_WORDS: usize = 24;

/// The SHA-256 of a text with every run of white space written as one
/// space and none at its ends, in lower-case hexadecimal.
pub fn content_hash(text: &str) -> String {
    use std::fmt::Write as _;

    let mut hasher = Sha256::new();
    let mut first = true;
    for word in text.split_whitespace() {
        if !first {
            hasher.update(b" ");
        }
        first = false;
        hasher.update(word.as_bytes());
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // Writing to a String cannot fail.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The words of a text as the fingerprint counts them: runs of letters and
/// digits, in lower case. Punctuation and white space only separate.
pub fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// FNV-1a over the bytes of the words of one feature, with a unit separator
/// between them, finished with the avalanche of `MurmurHash3` so that every
/// bit of the result depends on every byte.
fn feature_hash(feature: &[String]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET;
    for (index, word) in feature.iter().enumerate() {
        if index > 0 {
            hash = (hash ^ 0x1f).wrapping_mul(PRIME);
        }
        for byte in word.bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(PRIME);
        }
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^ (hash >> 33)
}

/// The 64-bit SimHash of a text, or `None` for a text of fewer than
/// [`MIN_WORDS`] words.
pub fn simhash(text: &str) -> Option<u64> {
    let words = words(text);
    if words.len() < MIN_WORDS {
        return None;
    }
    let mut votes = [0_i64; 64];
    for feature in words.windows(SHINGLE) {
        let hash = feature_hash(feature);
        for (bit, vote) in votes.iter_mut().enumerate() {
            if hash >> bit & 1 == 1 {
                *vote += 1;
            } else {
                *vote -= 1;
            }
        }
    }
    let mut fingerprint = 0_u64;
    for (bit, vote) in votes.iter().enumerate() {
        if *vote > 0 {
            fingerprint |= 1 << bit;
        }
    }
    Some(fingerprint)
}

/// How many bits two fingerprints differ in: 0 for the same, 64 at most.
pub const fn distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Whether two fingerprints are those of nearly the same text.
pub const fn near_duplicate(a: u64, b: u64) -> bool {
    distance(a, b) <= NEAR_DUPLICATE_BITS
}

#[cfg(test)]
mod tests {
    use super::{
        MIN_WORDS, NEAR_DUPLICATE_BITS, content_hash, distance, feature_hash, near_duplicate,
        simhash, words,
    };

    /// Generated words: the same seed, the same words. A few are common and
    /// most are rare, as in a language; whole numbers only, so that every
    /// machine draws the same.
    fn generated(seed: u64, count: usize) -> Vec<String> {
        let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut draw = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % 2000
        };
        (0..count)
            .map(|_| format!("w{}", draw() * draw() / 2000))
            .collect()
    }

    /// A text of generated words.
    fn prose(seed: u64, count: usize) -> String {
        generated(seed, count).join(" ")
    }

    /// The fingerprints of a text and of the same text with a run of
    /// `changed` words replaced by others: what an edit looks like.
    fn edited(seed: u64, length: usize, changed: usize) -> (u64, u64) {
        let original = generated(seed, length);
        let mut revised = original.clone();
        let at = usize::try_from(seed * 37).unwrap() % (length - changed);
        revised.splice(at..at + changed, generated(seed + 9000, changed));
        (
            simhash(&original.join(" ")).unwrap(),
            simhash(&revised.join(" ")).unwrap(),
        )
    }

    #[test]
    fn the_content_hash_ignores_how_the_white_space_is_written() {
        let plain = content_hash("A walk along the harbour.");
        assert_eq!(plain.len(), 64);
        assert!(plain.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(content_hash("  A walk\n\talong   the harbour.\r\n"), plain);
        // Everything else counts: a letter, its case, a full stop.
        assert_ne!(content_hash("A walk along the harbour"), plain);
        assert_ne!(content_hash("a walk along the harbour."), plain);
        assert_ne!(content_hash("Awalk along the harbour."), plain);
        // The value itself is fixed: SHA-256 of the words joined by spaces.
        assert_eq!(
            content_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            content_hash(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(content_hash(" \n "), content_hash(""));
    }

    #[test]
    fn words_are_runs_of_letters_and_digits_in_lower_case() {
        assert_eq!(
            words("Über-Straße 12, 3rd floor: don't!"),
            ["über", "straße", "12", "3rd", "floor", "don", "t"]
        );
        assert_eq!(words(" \t\n"), Vec::<String>::new());
        assert_eq!(words("…—"), Vec::<String>::new());
    }

    #[test]
    fn a_feature_hashes_to_the_same_value_on_every_machine() {
        let feature = |text: &str| -> Vec<String> { words(text) };
        // Literal values: the fingerprints in the store are compared across
        // releases, so this function may never change by accident.
        assert_eq!(
            feature_hash(&feature("a walk along")),
            0x2c92_82ae_5048_faca
        );
        assert_eq!(
            feature_hash(&feature("walk along the")),
            0x89dc_c5bf_2dd2_4426
        );
        // Where one word ends and the next begins is part of the feature.
        assert_ne!(
            feature_hash(&feature("ab c d")),
            feature_hash(&feature("a bc d"))
        );
        assert_ne!(
            feature_hash(&feature("a b c")),
            feature_hash(&feature("c b a"))
        );
    }

    #[test]
    fn a_short_text_has_no_fingerprint() {
        assert_eq!(simhash(""), None);
        assert_eq!(simhash(&prose(1, MIN_WORDS - 1)), None);
        assert!(simhash(&prose(1, MIN_WORDS)).is_some());
    }

    #[test]
    fn the_same_text_has_the_same_fingerprint_however_it_is_set() {
        let text = prose(7, 300);
        let fingerprint = simhash(&text).unwrap();
        assert_eq!(simhash(&text), Some(fingerprint));
        // Case, punctuation and white space are not what a text says.
        let shouted = text.to_uppercase().replace(' ', ",\n  ");
        assert_eq!(simhash(&shouted), Some(fingerprint));
    }

    #[test]
    fn a_text_with_a_few_words_changed_is_a_few_bits_away() {
        const TEXTS: u64 = 300;
        // How many of the edited texts are still near-duplicates.
        let flagged = |changed: usize| {
            (1..=TEXTS)
                .filter(|seed| {
                    let (original, revised) = edited(*seed, 400, changed);
                    near_duplicate(original, revised)
                })
                .count()
        };
        // Two words in 400, as a date or a counter that changed: nearly
        // always. One sentence in 25: more often than not. A sixth of the
        // text rewritten: hardly ever. (Measured: 99, 61 and 4 in 100.)
        let (slight, sentence, rewritten) = (flagged(2), flagged(16), flagged(60));
        assert!(slight >= 285, "{slight} of {TEXTS} with two words changed");
        assert!(
            (120..=240).contains(&sentence),
            "{sentence} of {TEXTS} with a sentence changed"
        );
        assert!(
            rewritten <= 30,
            "{rewritten} of {TEXTS} with a sixth rewritten"
        );
        // The fewer words changed, the fewer bits.
        let apart = |changed: usize| -> u32 {
            (1..=TEXTS)
                .map(|seed| {
                    let (original, revised) = edited(seed, 400, changed);
                    distance(original, revised)
                })
                .sum()
        };
        assert!(apart(2) < apart(16) && apart(16) < apart(60));

        // Unrelated texts are about half of the 64 bits apart, never a
        // handful.
        let far: Vec<u32> = (1..=1000_u64)
            .map(|seed| {
                distance(
                    simhash(&prose(seed, 400)).unwrap(),
                    simhash(&prose(seed + 50_000, 400)).unwrap(),
                )
            })
            .collect();
        let nearest = far.iter().min().copied().unwrap();
        assert!(
            nearest > 2 * NEAR_DUPLICATE_BITS,
            "an unrelated text only {nearest} bits away"
        );
        let mean = f64::from(far.iter().sum::<u32>()) / 1000.0;
        assert!((30.0..34.0).contains(&mean), "{mean} bits on average");
    }

    #[test]
    fn the_distance_counts_differing_bits() {
        assert_eq!(distance(0, 0), 0);
        assert_eq!(distance(u64::MAX, 0), 64);
        assert_eq!(distance(0b1011, 0b0010), 2);
        assert_eq!(distance(1 << 63, 0), 1);
        assert!(near_duplicate(0b11_1111, 0));
        assert!(!near_duplicate(0b111_1111, 0));
    }
}
