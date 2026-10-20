//! The classified result of an attempt (spec §2, §8, §12).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::macros::string_enum;

/// Classified result of one attempt.
///
/// The `snake_case` names are part of the public API and of the stored data;
/// they never change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The page was fetched. Also used for 4xx statuses other than 403 and
    /// 429, which are returned as they are (spec §8.3).
    Ok,
    /// A challenge interstitial appeared and resolved by itself in the
    /// browser; the page behind it was fetched.
    ChallengePassed,
    /// A challenge interstitial did not resolve.
    ChallengeFailed,
    /// A CAPTCHA stands in front of the content. Kage never solves these.
    Captcha,
    /// The site answered with a rate limit.
    RateLimited,
    /// The site refused the request.
    Forbidden,
    /// A successful status with no real content: empty or boilerplate body,
    /// login wall, consent wall, or a script-only shell on the http path.
    SoftBlock,
    /// The request failed outside the site's bot defences: proxy or upstream
    /// error, a paused domain, or a 5xx with no vendor marker.
    ///
    /// A server error is retried on another persona, cools no persona down
    /// and does not count towards the domain's circuit breaker. Note for the
    /// scheduler (milestone M7): after consecutive server errors from one
    /// domain across personas it must still insert a short politeness pause
    /// for that domain, so that a site which is simply down is not hammered
    /// by retries.
    ProviderError,
    /// The attempt ran out of time.
    Timeout,
    /// The SSRF guard refused the URL or a redirect hop.
    SsrfDenied,
    /// robots.txt disallows the URL and the domain policy enforces it.
    RobotsDenied,
}

impl Outcome {
    /// Every outcome, in declaration order.
    pub const ALL: [Self; 11] = [
        Self::Ok,
        Self::ChallengePassed,
        Self::ChallengeFailed,
        Self::Captcha,
        Self::RateLimited,
        Self::Forbidden,
        Self::SoftBlock,
        Self::ProviderError,
        Self::Timeout,
        Self::SsrfDenied,
        Self::RobotsDenied,
    ];

    /// The stable `snake_case` name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::ChallengePassed => "challenge_passed",
            Self::ChallengeFailed => "challenge_failed",
            Self::Captcha => "captcha",
            Self::RateLimited => "rate_limited",
            Self::Forbidden => "forbidden",
            Self::SoftBlock => "soft_block",
            Self::ProviderError => "provider_error",
            Self::Timeout => "timeout",
            Self::SsrfDenied => "ssrf_denied",
            Self::RobotsDenied => "robots_denied",
        }
    }

    /// Whether the caller got the page.
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Ok | Self::ChallengePassed)
    }

    /// Whether the site itself turned the persona away. A block cools the
    /// persona down for that domain (spec §6.3) and counts towards the
    /// domain's circuit breaker (spec §8.3).
    pub const fn is_block(self) -> bool {
        matches!(
            self,
            Self::ChallengeFailed
                | Self::Captcha
                | Self::RateLimited
                | Self::Forbidden
                | Self::SoftBlock
        )
    }

    /// Whether the scheduler may try again with another persona (spec §8.3).
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::RateLimited
                | Self::ChallengeFailed
                | Self::Captcha
                | Self::Forbidden
                | Self::SoftBlock
                | Self::ProviderError
                | Self::Timeout
        )
    }

    /// How long a persona stays away from the domain after this outcome,
    /// unless the signature that matched says otherwise: blocks take the
    /// domain policy's cooldown for the outcome (spec §6.3), everything else
    /// none.
    pub const fn default_cooldown(self) -> CooldownClass {
        if self.is_block() {
            CooldownClass::Long
        } else {
            CooldownClass::None
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

string_enum! {
    /// How long a blocked persona stays away from a domain, as a class that
    /// the domain policy turns into a duration. A signature carries one so
    /// that the weight of the evidence, not only the outcome, decides how
    /// much of a persona's time a block costs.
    pub enum CooldownClass {
        /// No cooldown: another address would get the same answer.
        None => "none",
        /// The policy's short cooldown: the block is real but weakly
        /// attributed, for example a bare 403 with no vendor marker.
        Short => "short",
        /// The policy's cooldown for the outcome (spec §6.3).
        Long => "long",
    }
}

/// An outcome name that is not one of [`Outcome::ALL`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown outcome {0:?}")]
pub struct UnknownOutcome(pub String);

impl FromStr for Outcome {
    type Err = UnknownOutcome;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|outcome| outcome.as_str() == text)
            .ok_or_else(|| UnknownOutcome(text.to_owned()))
    }
}

/// A lowercase slug: ASCII letters, digits and underscores, starting with a
/// letter. Vendors and block types are slugs so that signature files can
/// introduce new ones without a code change.
fn is_slug(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && text.len() <= 64
}

/// A name that is not a valid slug.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a lowercase slug (a-z, 0-9, _; starts with a letter; at most 64 characters)")]
pub struct InvalidSlug(pub String);

macro_rules! define_slug {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Validates and wraps a slug.
            ///
            /// # Errors
            ///
            /// Fails when the text is not a lowercase slug.
            pub fn new(text: impl Into<String>) -> Result<Self, InvalidSlug> {
                let text = text.into();
                if is_slug(&text) {
                    Ok(Self(text))
                } else {
                    Err(InvalidSlug(text))
                }
            }

            /// The slug.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = InvalidSlug;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Self::new(text)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                Self::new(text).map_err(serde::de::Error::custom)
            }
        }
    };
}

define_slug!(
    /// The bot-defence vendor a signature attributes a response to, for
    /// example `cloudflare`. `generic` means no vendor was identified.
    Vendor
);

define_slug!(
    /// The kind of block within a vendor, for example `managed_challenge`.
    BlockType
);

impl Vendor {
    /// Vendor slug for rules that match on status alone.
    pub const GENERIC: &'static str = "generic";
}

/// The block report attached to a blocked result (spec §12, §16.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    /// Who blocked.
    pub vendor: Vendor,
    /// How.
    #[serde(rename = "type")]
    pub block_type: BlockType,
    /// How sure the matching signature is, from 0 to 1.
    pub confidence: f32,
    /// What matched, for example `header:cf-mitigated=challenge`.
    pub evidence: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::{Block, BlockType, Outcome, UnknownOutcome, Vendor};

    #[test]
    fn names_round_trip_and_match_serde() {
        for outcome in Outcome::ALL {
            let name = outcome.as_str();
            assert_eq!(name.parse::<Outcome>().unwrap(), outcome);
            assert_eq!(outcome.to_string(), name);
            assert_eq!(
                serde_json::to_string(&outcome).unwrap(),
                format!("\"{name}\"")
            );
            assert_eq!(
                serde_json::from_str::<Outcome>(&format!("\"{name}\"")).unwrap(),
                outcome
            );
        }
        assert_eq!(
            "blocked".parse::<Outcome>(),
            Err(UnknownOutcome("blocked".to_owned()))
        );
    }

    #[test]
    fn spec_names_are_all_present() {
        let names: Vec<&str> = Outcome::ALL.iter().map(|o| o.as_str()).collect();
        assert_eq!(
            names,
            [
                "ok",
                "challenge_passed",
                "challenge_failed",
                "captcha",
                "rate_limited",
                "forbidden",
                "soft_block",
                "provider_error",
                "timeout",
                "ssrf_denied",
                "robots_denied",
            ]
        );
    }

    #[test]
    fn retryable_outcomes_follow_spec_8_3() {
        let retryable: Vec<Outcome> = Outcome::ALL
            .into_iter()
            .filter(|o| o.is_retryable())
            .collect();
        assert_eq!(
            retryable,
            [
                Outcome::ChallengeFailed,
                Outcome::Captcha,
                Outcome::RateLimited,
                Outcome::Forbidden,
                Outcome::SoftBlock,
                Outcome::ProviderError,
                Outcome::Timeout,
            ]
        );
        assert!(!Outcome::SsrfDenied.is_retryable());
        assert!(!Outcome::RobotsDenied.is_retryable());
        assert!(!Outcome::Ok.is_retryable());
    }

    #[test]
    fn blocks_are_the_outcomes_with_a_cooldown() {
        let blocks: Vec<Outcome> = Outcome::ALL.into_iter().filter(|o| o.is_block()).collect();
        assert_eq!(
            blocks,
            [
                Outcome::ChallengeFailed,
                Outcome::Captcha,
                Outcome::RateLimited,
                Outcome::Forbidden,
                Outcome::SoftBlock,
            ]
        );
        assert!(blocks.iter().all(|o| o.is_retryable() && !o.is_success()));
    }

    #[test]
    fn only_blocks_cool_a_persona_down_by_default() {
        use super::CooldownClass;

        for outcome in Outcome::ALL {
            let expected = if outcome.is_block() {
                CooldownClass::Long
            } else {
                CooldownClass::None
            };
            assert_eq!(outcome.default_cooldown(), expected, "{outcome}");
        }
        // A server error is retried, but costs no persona its access.
        assert!(Outcome::ProviderError.is_retryable());
        assert_eq!(
            Outcome::ProviderError.default_cooldown(),
            CooldownClass::None
        );
        crate::macros::test_support::assert_string_enum(
            CooldownClass::ALL,
            CooldownClass::as_str,
            &["none", "short", "long"],
        );
    }

    #[test]
    fn success() {
        assert!(Outcome::Ok.is_success());
        assert!(Outcome::ChallengePassed.is_success());
        assert!(!Outcome::SoftBlock.is_success());
        assert!(!Outcome::ChallengeFailed.is_success());
    }

    #[test]
    fn slugs_are_validated() {
        assert_eq!(Vendor::new("cloudflare").unwrap().as_str(), "cloudflare");
        assert_eq!("aws_waf".parse::<Vendor>().unwrap().to_string(), "aws_waf");
        assert_eq!(Vendor::new(Vendor::GENERIC).unwrap().as_str(), "generic");
        assert_eq!(BlockType::new("http_403").unwrap().as_str(), "http_403");

        for bad in [
            "",
            "Cloudflare",
            "aws-waf",
            "9lives",
            "has space",
            "_x",
            &"a".repeat(65),
        ] {
            assert!(Vendor::new(bad).is_err(), "{bad:?}");
        }
        assert!(serde_json::from_str::<Vendor>("\"Not A Slug\"").is_err());
        assert!(serde_json::from_str::<BlockType>("\"ok_type\"").is_ok());
    }

    #[test]
    fn block_serializes_like_the_spec_example() {
        let block = Block {
            vendor: Vendor::new("cloudflare").unwrap(),
            block_type: BlockType::new("managed_challenge").unwrap(),
            confidence: 0.5,
            evidence: vec!["header:cf-mitigated=challenge".to_owned()],
        };

        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "vendor": "cloudflare",
                "type": "managed_challenge",
                "confidence": 0.5,
                "evidence": ["header:cf-mitigated=challenge"],
            })
        );
        assert_eq!(serde_json::from_value::<Block>(json).unwrap(), block);
    }
}
