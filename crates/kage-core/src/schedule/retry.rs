//! What follows an attempt (spec §3.2, §8.3, §25): the request is done or
//! another attempt is made; and how long a blocked persona stays away from
//! the domain.
//!
//! # The level is the ceiling
//!
//! Nothing moves a request from one path to another. The level a request
//! names decides its path once (spec §25): the http path at 0 and 1, the
//! browser at 2 and 3. A block at level 0 or 1 is reported, never tried
//! again in a browser.
//!
//! # Retries
//!
//! At levels 1 to 3 every retry, after a block or after a failure that was
//! not the site's verdict, is made by another persona. At level 0 there is
//! no persona: a failure that was not the site's verdict (a broken
//! connection, a timeout, a server error) is tried again from the same
//! exit, and a block is the request's answer at once, never asked again
//! from another address.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::classifier::Classification;
use crate::domain::{Level, Render};
use crate::ids::PersonaId;
use crate::outcome::{CooldownClass, Outcome};
use crate::pacing::Lognormal;
use crate::policy::Cooldowns;
use crate::seed::Seed;

/// What one attempt came to, as far as the scheduler needs it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AttemptVerdict {
    /// The outcome.
    pub outcome: Outcome,
    /// Whether another attempt may be made.
    pub retryable: bool,
    /// How long the persona stays away, as a class.
    pub cooldown: CooldownClass,
    /// How sure the verdict is, from 0 to 1.
    pub confidence: f32,
}

impl AttemptVerdict {
    /// The verdict of an outcome nothing more is known about: every hint is
    /// what the outcome implies, and it is certain.
    pub const fn plain(outcome: Outcome) -> Self {
        Self {
            outcome,
            retryable: outcome.is_retryable(),
            cooldown: outcome.default_cooldown(),
            confidence: 1.0,
        }
    }

    /// The verdict the classifier reached.
    pub fn of(classification: &Classification) -> Self {
        Self {
            outcome: classification.outcome,
            retryable: classification.retryable,
            cooldown: classification.cooldown,
            confidence: classification
                .block
                .as_ref()
                .map_or(1.0, |block| block.confidence),
        }
    }

    /// Whether the site turned the visitor itself away, as opposed to
    /// asking for something only a browser can give.
    pub const fn is_refusal(&self) -> bool {
        matches!(
            self.outcome,
            Outcome::Forbidden | Outcome::Captcha | Outcome::RateLimited
        )
    }
}

/// What happens to the request next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Nothing: the attempt's result is the request's.
    Finish,
    /// Another attempt on the request's path: by another persona, or at
    /// level 0 from the same exit.
    Retry,
}

/// What an attempt means for the request and for the persona that made it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ruling {
    /// What happens to the request.
    pub step: Step,
    /// The block the persona answers for: it cools as this verdict says and
    /// the domain's breaker counts one. `None` when the persona has nothing
    /// to answer for, and always at level 0, which has no persona: a site
    /// that refuses a plain client says nothing about the personas.
    pub penalty: Option<AttemptVerdict>,
}

/// How far a request has come.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestProgress {
    level: Level,
    max_attempts: u32,
    attempts: u32,
    used: Vec<PersonaId>,
    blocked: Vec<PersonaId>,
}

impl RequestProgress {
    /// A request of `level` that may be retried `retries` times.
    pub fn new(level: Level, retries: u32) -> Self {
        Self {
            level,
            max_attempts: retries.saturating_add(1),
            attempts: 0,
            used: Vec::new(),
            blocked: Vec::new(),
        }
    }

    /// The same request taken up again after its attempts so far were
    /// made by a process that stopped: every earlier attempt counts toward
    /// the retry budget, the personas that made them are not asked again,
    /// and the ones that were blocked stay blocked.
    #[must_use]
    pub fn resumed(
        mut self,
        earlier: impl IntoIterator<Item = (Option<PersonaId>, Outcome)>,
    ) -> Self {
        for (persona, outcome) in earlier {
            self.attempts = self.attempts.saturating_add(1);
            let Some(persona) = persona else { continue };
            if !self.used.contains(&persona) {
                self.used.push(persona);
            }
            if outcome.is_block() && !self.blocked.contains(&persona) {
                self.blocked.push(persona);
            }
        }
        self
    }

    /// Whether another attempt may be made.
    pub const fn attempts_left(&self) -> bool {
        self.attempts < self.max_attempts
    }

    /// The level of the request.
    pub const fn level(&self) -> Level {
        self.level
    }

    /// The path of every attempt: the level's.
    pub const fn render(&self) -> Render {
        self.level.render()
    }

    /// Attempts made so far.
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Every persona that made an attempt, in order.
    pub fn used(&self) -> &[PersonaId] {
        &self.used
    }

    /// Every persona that was blocked on this request, in order.
    pub fn blocked(&self) -> &[PersonaId] {
        &self.blocked
    }

    /// Records the attempt just made, by `persona` (`None` at level 0),
    /// and says what follows.
    pub fn record(&mut self, persona: Option<PersonaId>, verdict: AttemptVerdict) -> Ruling {
        self.attempts = self.attempts.saturating_add(1);
        if let Some(persona) = persona
            && !self.used.contains(&persona)
        {
            self.used.push(persona);
        }
        if verdict.outcome.is_success() {
            return Ruling {
                step: Step::Finish,
                penalty: None,
            };
        }
        let left = self.attempts < self.max_attempts;
        let blocked = verdict.outcome.is_block();
        let Some(persona) = persona else {
            // A plain client: the site's refusal is its answer.
            let step = if verdict.retryable && !blocked && left {
                Step::Retry
            } else {
                Step::Finish
            };
            return Ruling {
                step,
                penalty: None,
            };
        };
        let penalty = blocked.then_some(verdict);
        if blocked {
            self.blocked.push(persona);
        }
        let step = if verdict.retryable && left {
            Step::Retry
        } else {
            Step::Finish
        };
        Ruling { step, penalty }
    }
}

/// How a cooldown is made from the policy's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CooldownRules {
    /// The shortest cooldown any block gives.
    pub floor: Duration,
    /// How far a cooldown is moved per persona, as a share of it.
    pub jitter: f64,
}

/// The seed of the jitter of one cooldown: the persona, the domain and the
/// moment of the block, so the cooldown can be explained afterwards.
pub fn cooldown_seed(persona: &Seed, domain: &str, at: DateTime<Utc>) -> Seed {
    Seed::derive(
        "cooldown",
        &[
            persona.as_bytes(),
            domain.as_bytes(),
            &at.timestamp_millis().to_le_bytes(),
        ],
    )
}

/// How long a persona stays away from a domain after a block (spec §6.3):
/// the policy's cooldown for the outcome and the class of the signature,
/// scaled by how sure the signature is, never below the floor, and then
/// moved by up to the jitter, drawn from `seed`, so that personas blocked
/// in the same minute do not come back in the same minute. `None` when the
/// verdict asks for no cooldown.
pub fn cooldown_for(
    cooldowns: &Cooldowns,
    verdict: &AttemptVerdict,
    rules: &CooldownRules,
    seed: &Seed,
) -> Option<Duration> {
    let base = cooldowns.resolve(verdict.outcome, verdict.cooldown)?;
    let confidence = if verdict.confidence.is_finite() {
        f64::from(verdict.confidence).clamp(0.0, 1.0)
    } else {
        1.0
    };
    // Whole seconds: a confidence is a short decimal that a float holds
    // only nearly, and 22.8 hours should not come out a millisecond short.
    let scaled =
        Duration::from_secs_f64((base.as_secs_f64() * confidence).round()).max(rules.floor);
    let jitter = if rules.jitter.is_finite() {
        rules.jitter.clamp(0.0, 1.0)
    } else {
        0.0
    };
    Some(scaled.mul_f64(1.0 + seed.rng().uniform(-jitter, jitter)))
}

/// The pause before a retry on another persona: drawn from `pause` at that
/// persona's own pace.
pub fn retry_pause(pause: &Lognormal, factor: f64, seed: &Seed) -> Duration {
    let millis = pause.scaled(factor).sample(&mut seed.rng());
    Duration::from_secs_f64(millis.max(0.0) / 1000.0)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{TimeZone, Utc};

    use super::{
        AttemptVerdict, CooldownRules, RequestProgress, Ruling, Step, cooldown_for, cooldown_seed,
        retry_pause,
    };
    use crate::classifier::Classification;
    use crate::domain::{Level, Render};
    use crate::ids::PersonaId;
    use crate::outcome::{Block, BlockType, CooldownClass, Outcome, Vendor};
    use crate::pacing::Lognormal;
    use crate::policy::Cooldowns;
    use crate::seed::Seed;

    fn verdict(outcome: Outcome) -> AttemptVerdict {
        AttemptVerdict::plain(outcome)
    }

    const FINISHED: Ruling = Ruling {
        step: Step::Finish,
        penalty: None,
    };

    #[test]
    fn a_plain_verdict_takes_its_hints_from_the_outcome() {
        let forbidden = verdict(Outcome::Forbidden);
        assert!(forbidden.retryable && forbidden.is_refusal());
        assert_eq!(forbidden.cooldown, CooldownClass::Long);
        assert!((forbidden.confidence - 1.0).abs() < f32::EPSILON);

        let limited = verdict(Outcome::RateLimited);
        assert!(limited.retryable && limited.is_refusal());
        assert!(verdict(Outcome::Captcha).is_refusal());
        assert!(!verdict(Outcome::ChallengeFailed).is_refusal());
        assert!(!verdict(Outcome::SoftBlock).is_refusal());
        assert!(!verdict(Outcome::Timeout).is_refusal());

        let fine = verdict(Outcome::Ok);
        assert!(!fine.retryable);
        assert_eq!(fine.cooldown, CooldownClass::None);
    }

    #[test]
    fn a_classification_becomes_a_verdict_with_its_confidence() {
        let classification = Classification {
            outcome: Outcome::SoftBlock,
            block: Some(Block {
                vendor: Vendor::new("generic").unwrap(),
                block_type: BlockType::new("script_shell").unwrap(),
                confidence: 0.6,
                evidence: Vec::new(),
            }),
            vendors: Vec::new(),
            rule: Some("script-shell".to_owned()),
            retryable: false,
            cooldown: CooldownClass::None,
            evidence: Vec::new(),
        };

        let verdict = AttemptVerdict::of(&classification);

        assert_eq!(verdict.outcome, Outcome::SoftBlock);
        assert!(!verdict.retryable);
        assert_eq!(verdict.cooldown, CooldownClass::None);
        assert!((verdict.confidence - 0.6).abs() < f32::EPSILON);
        // Without a block report the verdict is certain.
        let good = Classification {
            outcome: Outcome::Ok,
            block: None,
            ..classification
        };
        assert!((AttemptVerdict::of(&good).confidence - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn three_blocked_attempts_use_three_personas() {
        let [a, b, c] = [PersonaId::new(), PersonaId::new(), PersonaId::new()];
        let mut request = RequestProgress::new(Level::Human, 2);
        assert_eq!(request.render(), Render::Browser);
        assert_eq!(request.level(), Level::Human);
        assert_eq!(request.attempts(), 0);
        let block = verdict(Outcome::Forbidden);
        let again = Ruling {
            step: Step::Retry,
            penalty: Some(block),
        };

        assert_eq!(request.record(Some(a), block), again);
        assert_eq!(request.record(Some(b), block), again);
        // The third attempt is the last: its block is the request's result.
        assert_eq!(
            request.record(Some(c), block),
            Ruling {
                step: Step::Finish,
                penalty: Some(block)
            }
        );
        assert_eq!(request.attempts(), 3);
        assert_eq!(request.used(), [a, b, c]);
        assert_eq!(request.blocked(), [a, b, c]);
    }

    #[test]
    fn the_level_is_the_path_of_every_attempt_and_nothing_moves_it() {
        assert_eq!(
            Level::ALL.map(|level| RequestProgress::new(level, 2).render()),
            [Render::Http, Render::Http, Render::Browser, Render::Browser]
        );
        // A block a browser would cure, on the http path of a persona: the
        // request stays where its level put it, and another persona tries.
        let [a, b] = [PersonaId::new(), PersonaId::new()];
        for curable in [
            Outcome::ChallengeFailed,
            Outcome::SoftBlock,
            Outcome::Forbidden,
        ] {
            let mut request = RequestProgress::new(Level::Emulated, 2);
            let blocked = verdict(curable);
            let ruling = request.record(Some(a), blocked);
            assert_eq!(ruling.penalty, Some(blocked), "{curable}");
            assert_eq!(
                ruling.step,
                if blocked.retryable {
                    Step::Retry
                } else {
                    Step::Finish
                },
                "{curable}"
            );
            assert_eq!(request.render(), Render::Http, "{curable}");
            request.record(Some(b), verdict(Outcome::Ok));
            assert_eq!(request.render(), Render::Http, "{curable}");
        }
    }

    #[test]
    fn a_plain_client_is_never_asked_again_after_a_block_and_answers_for_nothing() {
        for blocked in [
            Outcome::Forbidden,
            Outcome::RateLimited,
            Outcome::Captcha,
            Outcome::ChallengeFailed,
            Outcome::SoftBlock,
        ] {
            let mut request = RequestProgress::new(Level::Plain, 5);
            assert_eq!(
                request.record(None, verdict(blocked)),
                FINISHED,
                "{blocked}"
            );
            assert_eq!(request.attempts(), 1);
            assert_eq!(request.used(), []);
            assert_eq!(request.blocked(), []);
        }
        // What was not the site's verdict is tried again, from the same exit,
        // as long as the budget lasts, and never costs anybody anything.
        let mut request = RequestProgress::new(Level::Plain, 2);
        let again = Ruling {
            step: Step::Retry,
            penalty: None,
        };
        assert_eq!(request.record(None, verdict(Outcome::Timeout)), again);
        assert_eq!(request.record(None, verdict(Outcome::ProviderError)), again);
        assert_eq!(request.record(None, verdict(Outcome::Timeout)), FINISHED);
        assert!(!request.attempts_left());
        // A page is a page.
        let mut fine = RequestProgress::new(Level::Plain, 2);
        assert_eq!(fine.record(None, verdict(Outcome::Ok)), FINISHED);
        // What nothing would change ends it at once.
        for final_outcome in [Outcome::SsrfDenied, Outcome::RobotsDenied] {
            let mut request = RequestProgress::new(Level::Plain, 2);
            assert_eq!(request.record(None, verdict(final_outcome)), FINISHED);
        }
    }

    #[test]
    fn a_request_that_is_taken_up_again_keeps_what_its_earlier_attempts_cost() {
        let (first, second, third) = (PersonaId::new(), PersonaId::new(), PersonaId::new());
        // Three attempts in all; two were made before the server stopped,
        // the first of them blocked.
        let progress = RequestProgress::new(Level::Human, 2).resumed([
            (Some(first), Outcome::Forbidden),
            (Some(second), Outcome::Timeout),
        ]);

        assert_eq!(progress.attempts(), 2);
        assert_eq!(progress.used(), [first, second]);
        assert_eq!(progress.blocked(), [first]);
        assert!(progress.attempts_left());

        // The one attempt that is left is the last.
        let mut progress = progress;
        let ruling = progress.record(Some(third), verdict(Outcome::Forbidden));
        assert_eq!(ruling.step, Step::Finish);
        assert!(ruling.penalty.is_some());
        assert!(!progress.attempts_left());

        // A persona that was blocked twice is listed once.
        let twice = RequestProgress::new(Level::Fast, 3).resumed([
            (Some(first), Outcome::ChallengeFailed),
            (Some(first), Outcome::Forbidden),
        ]);
        assert_eq!(twice.used(), [first]);
        assert_eq!(twice.blocked(), [first]);
        // A plain client's attempts count and name nobody.
        let plain = RequestProgress::new(Level::Plain, 2)
            .resumed([(None, Outcome::Timeout), (None, Outcome::Forbidden)]);
        assert_eq!(plain.attempts(), 2);
        assert_eq!(plain.used(), []);
        assert_eq!(plain.blocked(), []);
        // Nothing before: nothing changes.
        let fresh = RequestProgress::new(Level::Emulated, 2);
        assert_eq!(fresh.clone().resumed([]), fresh);
        // A budget that is used up is used up.
        let spent =
            RequestProgress::new(Level::Human, 0).resumed([(Some(first), Outcome::Timeout)]);
        assert!(!spent.attempts_left());
    }

    #[test]
    fn a_success_ends_the_request_and_costs_nothing() {
        let mut request = RequestProgress::new(Level::Human, 2);
        let persona = PersonaId::new();

        assert_eq!(
            request.record(Some(persona), verdict(Outcome::Ok)),
            FINISHED
        );
        assert_eq!(request.attempts(), 1);
        assert_eq!(request.used(), [persona]);
        assert_eq!(request.blocked(), []);

        let mut passed = RequestProgress::new(Level::Fast, 2);
        assert_eq!(
            passed.record(Some(persona), verdict(Outcome::ChallengePassed)),
            FINISHED
        );
    }

    #[test]
    fn a_block_that_may_not_be_retried_ends_the_request_and_the_persona_answers_for_it() {
        let persona = PersonaId::new();
        let mut request = RequestProgress::new(Level::Human, 2);
        let final_block = AttemptVerdict {
            retryable: false,
            ..verdict(Outcome::Forbidden)
        };
        assert_eq!(
            request.record(Some(persona), final_block),
            Ruling {
                step: Step::Finish,
                penalty: Some(final_block)
            }
        );
        assert_eq!(request.blocked(), [persona]);
    }

    #[test]
    fn failures_that_are_not_the_sites_verdict_retry_without_a_penalty() {
        let [a, b, c] = [PersonaId::new(), PersonaId::new(), PersonaId::new()];
        let mut request = RequestProgress::new(Level::Human, 2);
        let again = Ruling {
            step: Step::Retry,
            penalty: None,
        };

        assert_eq!(request.record(Some(a), verdict(Outcome::Timeout)), again);
        assert_eq!(
            request.record(Some(b), verdict(Outcome::ProviderError)),
            again
        );
        assert_eq!(request.record(Some(c), verdict(Outcome::Timeout)), FINISHED);
        assert_eq!(request.blocked(), []);

        // What no other persona would change ends the request at once.
        for final_outcome in [Outcome::SsrfDenied, Outcome::RobotsDenied] {
            let mut request = RequestProgress::new(Level::Emulated, 2);
            assert_eq!(request.record(Some(a), verdict(final_outcome)), FINISHED);
        }
        // Zero retries is one attempt.
        let mut single = RequestProgress::new(Level::Human, 0);
        assert_eq!(single.record(Some(a), verdict(Outcome::Timeout)), FINISHED);
        // A persona that made two attempts is listed once.
        let mut request = RequestProgress::new(Level::Human, u32::MAX);
        request.record(Some(a), verdict(Outcome::Timeout));
        request.record(Some(a), verdict(Outcome::Timeout));
        assert_eq!(request.used(), [a]);
        assert_eq!(request.attempts(), 2);
    }

    fn rules() -> CooldownRules {
        CooldownRules {
            floor: Duration::from_mins(5),
            jitter: 0.0,
        }
    }

    #[test]
    fn a_cooldown_is_the_policys_scaled_by_the_confidence() {
        let cooldowns = Cooldowns::default();
        let seed = Seed::from_u64(1);
        let at = |outcome, cooldown, confidence| {
            cooldown_for(
                &cooldowns,
                &AttemptVerdict {
                    outcome,
                    retryable: true,
                    cooldown,
                    confidence,
                },
                &rules(),
                &seed,
            )
        };

        // A vendor block at 0.95 keeps 22.8 of 24 hours.
        assert_eq!(
            at(Outcome::Forbidden, CooldownClass::Long, 0.95).map(|d| d.as_secs()),
            Some(82_080)
        );
        // A bare 403 at 0.6 with class `short`: 18 of 30 minutes.
        assert_eq!(
            at(Outcome::Forbidden, CooldownClass::Short, 0.6).map(|d| d.as_secs()),
            Some(18 * 60)
        );
        assert_eq!(
            at(Outcome::RateLimited, CooldownClass::Long, 1.0),
            Some(Duration::from_hours(2))
        );
        // Never below the floor, however unsure.
        assert_eq!(
            at(Outcome::SoftBlock, CooldownClass::Short, 0.1),
            Some(Duration::from_mins(5))
        );
        assert_eq!(
            at(Outcome::SoftBlock, CooldownClass::Short, 0.0),
            Some(Duration::from_mins(5))
        );
        // No cooldown asked for is none, floor or not.
        assert_eq!(at(Outcome::SoftBlock, CooldownClass::None, 1.0), None);
        assert_eq!(at(Outcome::Ok, CooldownClass::None, 1.0), None);
        // A confidence that is no number, or out of range, is certainty.
        assert_eq!(
            at(Outcome::RateLimited, CooldownClass::Long, f32::NAN),
            Some(Duration::from_hours(2))
        );
        assert_eq!(
            at(Outcome::RateLimited, CooldownClass::Long, 7.0),
            Some(Duration::from_hours(2))
        );
        assert_eq!(
            at(Outcome::RateLimited, CooldownClass::Long, -1.0),
            Some(Duration::from_mins(5))
        );
    }

    #[test]
    fn personas_blocked_together_do_not_come_back_together() {
        let cooldowns = Cooldowns::default();
        let rules = CooldownRules {
            floor: Duration::from_mins(5),
            jitter: 0.1,
        };
        let block = AttemptVerdict::plain(Outcome::RateLimited);
        let moment = Utc.with_ymd_and_hms(2026, 10, 10, 12, 0, 0).unwrap();

        let mut seconds: Vec<u64> = (0..200_u64)
            .map(|persona| {
                let seed = cooldown_seed(&Seed::from_u64(persona), "example.org", moment);
                cooldown_for(&cooldowns, &block, &rules, &seed)
                    .unwrap()
                    .as_secs()
            })
            .collect();
        seconds.sort_unstable();

        // Two hours, give or take ten percent, spread over the whole range.
        assert!(seconds[0] >= 6480 && seconds[199] <= 7920, "{seconds:?}");
        assert!(seconds[0] < 6600 && seconds[199] > 7800);
        seconds.dedup();
        assert!(seconds.len() > 150, "{}", seconds.len());

        // The seed is the persona, the domain and the moment.
        let seed = |persona, domain, at| cooldown_seed(&Seed::from_u64(persona), domain, at);
        let same = seed(1, "example.org", moment);
        assert_eq!(same, seed(1, "example.org", moment));
        assert_ne!(same, seed(2, "example.org", moment));
        assert_ne!(same, seed(1, "example.net", moment));
        assert_ne!(
            same,
            seed(1, "example.org", moment + chrono::Duration::milliseconds(1))
        );
        // A jitter that is no number moves nothing; one above 1 is 1.
        let broken = CooldownRules {
            jitter: f64::NAN,
            ..rules
        };
        assert_eq!(
            cooldown_for(&cooldowns, &block, &broken, &same),
            Some(Duration::from_hours(2))
        );
        let wild = CooldownRules {
            jitter: 5.0,
            ..rules
        };
        assert!(cooldown_for(&cooldowns, &block, &wild, &same).unwrap() < Duration::from_hours(4));
    }

    #[test]
    fn the_pause_before_a_retry_is_at_least_two_seconds_and_follows_the_persona() {
        let pause = Lognormal {
            median: 6000.0,
            p95: 20_000.0,
            min: Some(2000.0),
            max: None,
        };

        let mut drawn: Vec<Duration> = (0..2_000_u64)
            .map(|n| retry_pause(&pause, 1.0, &Seed::from_u64(n)))
            .collect();
        drawn.sort();

        assert!(drawn[0] > Duration::from_secs(2));
        let median = drawn[1000];
        assert!(
            (Duration::from_millis(5400)..Duration::from_millis(6600)).contains(&median),
            "{median:?}"
        );
        // A slower persona waits longer, the same seed draws the same.
        let seed = Seed::from_u64(77);
        let plain = retry_pause(&pause, 1.0, &seed);
        assert_eq!(plain, retry_pause(&pause, 1.0, &seed));
        assert!(retry_pause(&pause, 1.25, &seed) > plain);
        assert!(retry_pause(&pause, 0.8, &seed) < plain);
    }
}
