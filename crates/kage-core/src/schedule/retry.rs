//! What follows an attempt (spec §3.2, §8.1, §8.3): the request is done,
//! the same persona tries again in its browser, or another persona tries;
//! and how long a blocked persona stays away from the domain.
//!
//! # Escalation
//!
//! Under `render: auto` an attempt on the http path that was blocked makes
//! the next attempt use the browser, once per request. Who makes it depends
//! on what the block was:
//!
//! - A block a browser cures (a challenge the http path cannot answer, a
//!   page that needs scripts): **the same persona** comes back in its
//!   browser, as a person reloading would. The verdict on the persona is
//!   then the browser's. It passes: nothing cools, the breaker counts
//!   nothing. It is blocked: the persona cools once, for the browser's
//!   block. It neither passes nor is blocked (a timeout, a broken
//!   connection): the http block stands.
//! - A refusal of the visitor (`forbidden`, `captcha`, `rate_limited`):
//!   the persona cools and **another persona** makes the browser attempt.
//!
//! # Retries
//!
//! Every retry after a refusal, after a browser block, and after a failure
//! that was not the site's verdict, is made by another persona.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::classifier::Classification;
use crate::domain::{Render, RenderMode};
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
    /// Whether another persona may try.
    pub retryable: bool,
    /// Whether a block on the http path sends the request to the browser.
    pub escalate: bool,
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
            escalate: outcome.escalates_to_browser(),
            cooldown: outcome.default_cooldown(),
            confidence: 1.0,
        }
    }

    /// The verdict the classifier reached.
    pub fn of(classification: &Classification) -> Self {
        Self {
            outcome: classification.outcome,
            retryable: classification.retryable,
            escalate: classification.escalate,
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
    /// The same persona tries again in its browser.
    EscalateSamePersona,
    /// Another persona tries on this path.
    RetryOtherPersona {
        /// The path of the next attempt.
        render: Render,
    },
}

/// What an attempt means for the request and for the persona that made it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ruling {
    /// What happens to the request.
    pub step: Step,
    /// The block the persona answers for: it cools as this verdict says and
    /// the domain's breaker counts one. `None` when the persona has nothing
    /// to answer for.
    pub penalty: Option<AttemptVerdict>,
}

/// How far a request has come.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestProgress {
    mode: RenderMode,
    render: Render,
    max_attempts: u32,
    attempts: u32,
    escalated: bool,
    /// The http block of the persona that is now trying in its browser.
    deferred: Option<AttemptVerdict>,
    used: Vec<PersonaId>,
    blocked: Vec<PersonaId>,
}

impl RequestProgress {
    /// A request that asked for `mode`, starts on `first` and may be retried
    /// `retries` times.
    pub fn new(mode: RenderMode, first: Render, retries: u32) -> Self {
        Self {
            mode,
            render: first,
            max_attempts: retries.saturating_add(1),
            attempts: 0,
            escalated: false,
            deferred: None,
            used: Vec::new(),
            blocked: Vec::new(),
        }
    }

    /// The same request taken up again after its attempts so far were
    /// made by a process that stopped: every earlier attempt counts toward
    /// the retry budget, the personas that made them are not asked again,
    /// the ones that were blocked stay blocked, and a request that had
    /// moved to the browser stays there.
    #[must_use]
    pub fn resumed(
        mut self,
        earlier: impl IntoIterator<Item = (PersonaId, Render, Outcome)>,
    ) -> Self {
        for (persona, render, outcome) in earlier {
            self.attempts = self.attempts.saturating_add(1);
            if !self.used.contains(&persona) {
                self.used.push(persona);
            }
            if outcome.is_block() && !self.blocked.contains(&persona) {
                self.blocked.push(persona);
            }
            if render == Render::Browser && self.render == Render::Http {
                self.escalated = true;
                self.render = Render::Browser;
            }
        }
        self
    }

    /// Whether another attempt may be made.
    pub const fn attempts_left(&self) -> bool {
        self.attempts < self.max_attempts
    }

    /// The path of the next attempt.
    pub const fn render(&self) -> Render {
        self.render
    }

    /// Attempts made so far.
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Whether the request moved from the http path to the browser.
    pub const fn escalated(&self) -> bool {
        self.escalated
    }

    /// Every persona that made an attempt, in order.
    pub fn used(&self) -> &[PersonaId] {
        &self.used
    }

    /// Every persona that was blocked on this request, in order.
    pub fn blocked(&self) -> &[PersonaId] {
        &self.blocked
    }

    /// Gives up the persona's own second try: its browser could not be
    /// had. Returns the http block it was to cure, which now stands, and
    /// the persona that answers for it; the next attempt is another
    /// persona's.
    pub fn forfeit(&mut self) -> Option<(PersonaId, AttemptVerdict)> {
        let verdict = self.deferred.take()?;
        let persona = *self.used.last()?;
        self.blocked.push(persona);
        Some((persona, verdict))
    }

    /// Whether a block on the http path may still send the request to the
    /// browser: it asked for `auto`, is on the http path and has not moved
    /// yet.
    const fn may_escalate(&self) -> bool {
        matches!(self.mode, RenderMode::Auto)
            && matches!(self.render, Render::Http)
            && !self.escalated
    }

    /// Records the attempt `persona` just made and says what follows.
    pub fn record(&mut self, persona: PersonaId, verdict: AttemptVerdict) -> Ruling {
        self.attempts = self.attempts.saturating_add(1);
        if !self.used.contains(&persona) {
            self.used.push(persona);
        }
        // Set when this attempt was the persona's own escalation.
        let unanswered = self.deferred.take();
        if verdict.outcome.is_success() {
            return Ruling {
                step: Step::Finish,
                penalty: None,
            };
        }
        let left = self.attempts < self.max_attempts;
        let blocked = verdict.outcome.is_block();

        if blocked && verdict.escalate && self.may_escalate() {
            if !verdict.is_refusal() {
                if left {
                    self.escalated = true;
                    self.render = Render::Browser;
                    self.deferred = Some(verdict);
                    return Ruling {
                        step: Step::EscalateSamePersona,
                        penalty: None,
                    };
                }
                // No attempt is left to cure it: the block stands.
                self.blocked.push(persona);
                return Ruling {
                    step: Step::Finish,
                    penalty: Some(verdict),
                };
            }
            self.blocked.push(persona);
            if left && verdict.retryable {
                self.escalated = true;
                self.render = Render::Browser;
                return Ruling {
                    step: Step::RetryOtherPersona {
                        render: Render::Browser,
                    },
                    penalty: Some(verdict),
                };
            }
            return Ruling {
                step: Step::Finish,
                penalty: Some(verdict),
            };
        }

        let penalty = if blocked { Some(verdict) } else { unanswered };
        if penalty.is_some() {
            self.blocked.push(persona);
        }
        let step = if verdict.retryable && left {
            Step::RetryOtherPersona {
                render: self.render,
            }
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
    use crate::domain::{Render, RenderMode};
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
        assert!(forbidden.retryable && forbidden.escalate && forbidden.is_refusal());
        assert_eq!(forbidden.cooldown, CooldownClass::Long);
        assert!((forbidden.confidence - 1.0).abs() < f32::EPSILON);

        let limited = verdict(Outcome::RateLimited);
        assert!(limited.retryable && !limited.escalate && limited.is_refusal());
        assert!(verdict(Outcome::Captcha).is_refusal());
        assert!(!verdict(Outcome::ChallengeFailed).is_refusal());
        assert!(!verdict(Outcome::SoftBlock).is_refusal());
        assert!(!verdict(Outcome::Timeout).is_refusal());

        let fine = verdict(Outcome::Ok);
        assert!(!fine.retryable && !fine.escalate);
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
            escalate: true,
            cooldown: CooldownClass::None,
            evidence: Vec::new(),
        };

        let verdict = AttemptVerdict::of(&classification);

        assert_eq!(verdict.outcome, Outcome::SoftBlock);
        assert!(!verdict.retryable && verdict.escalate);
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
        let mut request = RequestProgress::new(RenderMode::Browser, Render::Browser, 2);
        assert_eq!(request.render(), Render::Browser);
        assert_eq!(request.attempts(), 0);
        let block = verdict(Outcome::Forbidden);
        let again = Step::RetryOtherPersona {
            render: Render::Browser,
        };

        assert_eq!(
            request.record(a, block),
            Ruling {
                step: again,
                penalty: Some(block)
            }
        );
        assert_eq!(
            request.record(b, block),
            Ruling {
                step: again,
                penalty: Some(block)
            }
        );
        // The third attempt is the last: its block is the request's result.
        assert_eq!(
            request.record(c, block),
            Ruling {
                step: Step::Finish,
                penalty: Some(block)
            }
        );
        assert_eq!(request.attempts(), 3);
        assert_eq!(request.used(), [a, b, c]);
        assert_eq!(request.blocked(), [a, b, c]);
        assert!(!request.escalated());
    }

    #[test]
    fn a_request_that_is_taken_up_again_keeps_what_its_earlier_attempts_cost() {
        let (first, second, third) = (PersonaId::new(), PersonaId::new(), PersonaId::new());
        // Three attempts in all; two were made before the server stopped,
        // the first of them blocked.
        let progress = RequestProgress::new(RenderMode::Browser, Render::Browser, 2).resumed([
            (first, Render::Browser, Outcome::Forbidden),
            (second, Render::Browser, Outcome::Timeout),
        ]);

        assert_eq!(progress.attempts(), 2);
        assert_eq!(progress.used(), [first, second]);
        assert_eq!(progress.blocked(), [first]);
        assert!(progress.attempts_left());
        assert!(!progress.escalated());

        // The one attempt that is left is the last.
        let mut progress = progress;
        let ruling = progress.record(third, verdict(Outcome::Forbidden));
        assert_eq!(ruling.step, Step::Finish);
        assert!(ruling.penalty.is_some());
        assert!(!progress.attempts_left());

        // A request that had moved to the browser stays there.
        let moved = RequestProgress::new(RenderMode::Auto, Render::Http, 2).resumed([
            (first, Render::Http, Outcome::ChallengeFailed),
            (first, Render::Browser, Outcome::Timeout),
        ]);
        assert_eq!(moved.render(), Render::Browser);
        assert!(moved.escalated());
        assert_eq!(moved.used(), [first]);
        assert_eq!(moved.blocked(), [first]);
        // Nothing before: nothing changes.
        let fresh = RequestProgress::new(RenderMode::Auto, Render::Http, 2);
        assert_eq!(fresh.clone().resumed([]), fresh);
        // A budget that is used up is used up.
        let spent = RequestProgress::new(RenderMode::Browser, Render::Browser, 0).resumed([(
            first,
            Render::Browser,
            Outcome::Timeout,
        )]);
        assert!(!spent.attempts_left());
    }

    #[test]
    fn a_success_ends_the_request_and_costs_nothing() {
        let mut request = RequestProgress::new(RenderMode::Auto, Render::Browser, 2);
        let persona = PersonaId::new();

        assert_eq!(request.record(persona, verdict(Outcome::Ok)), FINISHED);
        assert_eq!(request.attempts(), 1);
        assert_eq!(request.used(), [persona]);
        assert_eq!(request.blocked(), []);

        let mut passed = RequestProgress::new(RenderMode::Auto, Render::Browser, 2);
        assert_eq!(
            passed.record(persona, verdict(Outcome::ChallengePassed)),
            FINISHED
        );
    }

    #[test]
    fn a_curable_http_block_sends_the_same_persona_to_its_browser_once() {
        let [a, b] = [PersonaId::new(), PersonaId::new()];
        for curable in [Outcome::ChallengeFailed, Outcome::SoftBlock] {
            let mut request = RequestProgress::new(RenderMode::Auto, Render::Http, 2);

            // The http block is not held against the persona yet.
            assert_eq!(
                request.record(a, verdict(curable)),
                Ruling {
                    step: Step::EscalateSamePersona,
                    penalty: None
                }
            );
            assert!(request.escalated());
            assert_eq!(request.render(), Render::Browser);
            assert_eq!(request.blocked(), []);

            // Its browser is blocked too: one penalty, the browser's, and
            // the retry is another persona's, on the browser.
            let browser_block = verdict(Outcome::Captcha);
            assert_eq!(
                request.record(a, browser_block),
                Ruling {
                    step: Step::RetryOtherPersona {
                        render: Render::Browser
                    },
                    penalty: Some(browser_block)
                }
            );
            assert_eq!(request.used(), [a]);
            assert_eq!(request.blocked(), [a]);

            // The request never goes back to the http path and never
            // escalates again.
            assert_eq!(
                request.record(b, verdict(curable)),
                Ruling {
                    step: Step::Finish,
                    penalty: Some(verdict(curable))
                }
            );
            assert_eq!(request.attempts(), 3);
            assert_eq!(request.used(), [a, b]);
        }
    }

    #[test]
    fn a_browser_that_passes_after_an_http_block_leaves_the_persona_clean() {
        let a = PersonaId::new();
        let mut request = RequestProgress::new(RenderMode::Auto, Render::Http, 2);
        request.record(a, verdict(Outcome::ChallengeFailed));

        assert_eq!(
            request.record(a, verdict(Outcome::ChallengePassed)),
            FINISHED
        );
        assert_eq!(request.blocked(), []);
        assert_eq!(request.attempts(), 2);
    }

    #[test]
    fn an_http_block_stands_when_the_browser_gives_no_verdict() {
        let [a, b] = [PersonaId::new(), PersonaId::new()];
        let http_block = verdict(Outcome::ChallengeFailed);
        let mut request = RequestProgress::new(RenderMode::Auto, Render::Http, 2);
        request.record(a, http_block);

        // The browser attempt timed out: the unanswered challenge is what
        // the persona answers for, and another persona tries.
        assert_eq!(
            request.record(a, verdict(Outcome::Timeout)),
            Ruling {
                step: Step::RetryOtherPersona {
                    render: Render::Browser
                },
                penalty: Some(http_block)
            }
        );
        assert_eq!(request.blocked(), [a]);
        // The debt is paid once: a later timeout of another persona costs
        // nothing.
        assert_eq!(
            request.record(b, verdict(Outcome::Timeout)),
            Ruling {
                step: Step::Finish,
                penalty: None
            }
        );
        assert_eq!(request.blocked(), [a]);
    }

    #[test]
    fn an_escalation_that_cannot_be_made_leaves_the_http_block_standing() {
        let [a, b] = [PersonaId::new(), PersonaId::new()];
        let http_block = verdict(Outcome::SoftBlock);
        let mut request = RequestProgress::new(RenderMode::Auto, Render::Http, 2);
        // Nothing to give up before anything was deferred.
        assert_eq!(request.forfeit(), None);
        request.record(a, http_block);

        assert_eq!(request.forfeit(), Some((a, http_block)));
        assert_eq!(request.blocked(), [a]);
        // It is given up once, and the next persona does not inherit it.
        assert_eq!(request.forfeit(), None);
        assert_eq!(
            request.record(b, verdict(Outcome::Timeout)),
            Ruling {
                step: Step::RetryOtherPersona {
                    render: Render::Browser
                },
                penalty: None
            }
        );
        assert_eq!(request.blocked(), [a]);
    }

    #[test]
    fn a_curable_block_on_the_last_attempt_stands() {
        let a = PersonaId::new();
        let block = verdict(Outcome::SoftBlock);
        let mut request = RequestProgress::new(RenderMode::Auto, Render::Http, 0);

        assert_eq!(
            request.record(a, block),
            Ruling {
                step: Step::Finish,
                penalty: Some(block)
            }
        );
        assert!(!request.escalated());
        assert_eq!(request.render(), Render::Http);
        assert_eq!(request.blocked(), [a]);
    }

    #[test]
    fn a_refusal_on_the_http_path_cools_the_persona_and_another_takes_the_browser() {
        let a = PersonaId::new();
        for refusal in [Outcome::Forbidden, Outcome::Captcha] {
            let block = verdict(refusal);
            let mut request = RequestProgress::new(RenderMode::Auto, Render::Http, 2);

            assert_eq!(
                request.record(a, block),
                Ruling {
                    step: Step::RetryOtherPersona {
                        render: Render::Browser
                    },
                    penalty: Some(block)
                }
            );
            assert!(request.escalated());
            assert_eq!(request.render(), Render::Browser);
            assert_eq!(request.blocked(), [a]);
        }
        // On the last attempt, or when the rule says nobody else would do
        // better, the refusal is the result.
        let block = verdict(Outcome::Forbidden);
        let mut last = RequestProgress::new(RenderMode::Auto, Render::Http, 0);
        assert_eq!(
            last.record(a, block),
            Ruling {
                step: Step::Finish,
                penalty: Some(block)
            }
        );
        assert!(!last.escalated());
        let final_word = AttemptVerdict {
            retryable: false,
            ..block
        };
        let mut hopeless = RequestProgress::new(RenderMode::Auto, Render::Http, 2);
        assert_eq!(
            hopeless.record(a, final_word),
            Ruling {
                step: Step::Finish,
                penalty: Some(final_word)
            }
        );
        assert!(!hopeless.escalated());
        assert_eq!(hopeless.render(), Render::Http);
    }

    #[test]
    fn a_rate_limit_and_an_explicit_http_request_stay_on_the_http_path() {
        let a = PersonaId::new();
        // `rate_limited` does not escalate: another address, same path.
        let limited = verdict(Outcome::RateLimited);
        let mut request = RequestProgress::new(RenderMode::Auto, Render::Http, 2);
        assert_eq!(
            request.record(a, limited),
            Ruling {
                step: Step::RetryOtherPersona {
                    render: Render::Http
                },
                penalty: Some(limited)
            }
        );
        assert!(!request.escalated());

        // The caller asked for http: no escalation whatever the block.
        let challenge = verdict(Outcome::ChallengeFailed);
        let mut fixed = RequestProgress::new(RenderMode::Http, Render::Http, 2);
        assert_eq!(
            fixed.record(a, challenge),
            Ruling {
                step: Step::RetryOtherPersona {
                    render: Render::Http
                },
                penalty: Some(challenge)
            }
        );
        assert!(!fixed.escalated());
        // A rule that says "do not escalate" is obeyed under auto too.
        let stay = AttemptVerdict {
            escalate: false,
            ..challenge
        };
        let mut obedient = RequestProgress::new(RenderMode::Auto, Render::Http, 2);
        assert_eq!(
            obedient.record(a, stay).step,
            Step::RetryOtherPersona {
                render: Render::Http
            }
        );
    }

    #[test]
    fn failures_that_are_not_the_sites_verdict_retry_without_a_penalty() {
        let [a, b, c] = [PersonaId::new(), PersonaId::new(), PersonaId::new()];
        let mut request = RequestProgress::new(RenderMode::Browser, Render::Browser, 2);
        let again = Ruling {
            step: Step::RetryOtherPersona {
                render: Render::Browser,
            },
            penalty: None,
        };

        assert_eq!(request.record(a, verdict(Outcome::Timeout)), again);
        assert_eq!(request.record(b, verdict(Outcome::ProviderError)), again);
        assert_eq!(request.record(c, verdict(Outcome::Timeout)), FINISHED);
        assert_eq!(request.blocked(), []);

        // What no other persona would change ends the request at once.
        for final_outcome in [Outcome::SsrfDenied, Outcome::RobotsDenied] {
            let mut request = RequestProgress::new(RenderMode::Auto, Render::Http, 2);
            assert_eq!(request.record(a, verdict(final_outcome)), FINISHED);
        }
        // Zero retries is one attempt.
        let mut single = RequestProgress::new(RenderMode::Browser, Render::Browser, 0);
        assert_eq!(single.record(a, verdict(Outcome::Timeout)), FINISHED);
        // A persona that made two attempts is listed once.
        let mut request = RequestProgress::new(RenderMode::Browser, Render::Browser, u32::MAX);
        request.record(a, verdict(Outcome::Timeout));
        request.record(a, verdict(Outcome::Timeout));
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
                    escalate: true,
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
