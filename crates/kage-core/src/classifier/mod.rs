//! Block detection and classification (spec §12).
//!
//! Signatures are data: `policies/block-signatures.yaml` holds rules, each
//! with a vendor, a block type, an outcome, a confidence and a condition
//! over what one attempt observed. [`SignatureSet::classify`] evaluates
//! every rule and reports the best match.
//!
//! # File format
//!
//! ```yaml
//! version: 1
//! settings:
//!   max_body_bytes: 1048576        # how much of a body the rules look at
//! rules:
//!   - id: cloudflare-managed-challenge
//!     vendor: cloudflare
//!     type: managed_challenge
//!     outcome: challenge_failed
//!     confidence: 0.97
//!     priority: 80                 # optional, default 50; highest wins
//!     retryable: true              # optional: try another persona
//!     cooldown: long               # optional: none | short | long
//!     when:
//!       all:
//!         - status: [403, 503]
//!         - header: { name: cf-mitigated, regex: "(?i)^challenge$" }
//! hints:
//!   - id: cloudflare-cookies
//!     vendor: cloudflare
//!     when: { cookie: "^(__cf_bm|cf_clearance)$" }
//! ```
//!
//! Conditions are `all`, `any`, `not`, and the leaves `status`, `header`,
//! `cookie`, `body`, `url`, `script_host`, `iframe_host`,
//! `interstitial_text`, `visible_text`, `body_bytes`, `render` and
//! `interstitial`. Patterns are regular expressions matched anywhere in the
//! value; they are case-sensitive unless they start with `(?i)`.
//!
//! `retryable` and `cooldown` tell the scheduler what a match means. Each
//! defaults to what the outcome implies ([`Outcome::is_retryable`],
//! [`Outcome::default_cooldown`]); a rule overrides them when the evidence
//! says otherwise, for example a script-only shell that another persona
//! would get just the same. The `escalate` of 1.x is gone: nothing moves a
//! request from the http path to a browser since 2.0 (spec §25), and a
//! signature file that still names it is refused.
//!
//! A *hint* never blocks. It records which vendor fronts a site, because
//! protected sites set their vendor's cookies on good responses too. What
//! the hints matched is part of every verdict's evidence.

#[cfg(test)]
mod fixture_tests;
mod text;

use std::cell::OnceCell;
use std::ops::RangeInclusive;

use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};

use crate::domain::{Header, Render};
use crate::outcome::{Block, BlockType, CooldownClass, Outcome, Vendor};
use crate::policy::Retired;
use text::visible_text;

/// The only signature file version this build reads.
pub const SIGNATURE_FILE_VERSION: u32 = 1;

/// Default for `settings.max_body_bytes`.
const DEFAULT_MAX_BODY_BYTES: usize = 1 << 20;

/// Upper bound on the compiled size of one pattern.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// Longest header value quoted in evidence.
const EVIDENCE_VALUE_CHARS: usize = 80;

/// Priority of a rule that does not set one.
const DEFAULT_PRIORITY: i32 = 50;

/// A cookie set during the attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedCookie {
    /// Cookie name.
    pub name: String,
    /// Cookie value. Rules may match on it; it is never copied into evidence.
    #[serde(default)]
    pub value: String,
}

/// What the browser saw besides the response itself (spec §10.2 "challenge
/// signals").
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BrowserSignals {
    /// Hosts that scripts were loaded from.
    #[serde(default)]
    pub script_hosts: Vec<String>,
    /// Hosts that iframes were loaded from.
    #[serde(default)]
    pub iframe_hosts: Vec<String>,
    /// Text of a detected interstitial.
    #[serde(default)]
    pub interstitial_text: Option<String>,
    /// Whether an interstitial was detected at any point.
    #[serde(default)]
    pub interstitial_detected: bool,
    /// Whether that interstitial went away by itself.
    #[serde(default)]
    pub interstitial_resolved: bool,
}

/// Everything the classifier looks at for one attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    /// The path the attempt took.
    pub render: Render,
    /// Status of the final response.
    pub status: u16,
    /// URL after redirects.
    pub url: String,
    /// Headers of the final response.
    #[serde(default)]
    pub headers: Vec<Header>,
    /// Cookies set during the attempt.
    #[serde(default)]
    pub cookies: Vec<ObservedCookie>,
    /// The body: the rendered DOM on the browser path, the raw body on the
    /// http path.
    #[serde(default)]
    pub body: String,
    /// Browser signals; `None` on the http path.
    #[serde(default)]
    pub browser: Option<BrowserSignals>,
}

/// The classifier's verdict on one observation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Classification {
    /// The outcome.
    pub outcome: Outcome,
    /// The block report, when a rule with a non-`ok` outcome decided.
    pub block: Option<Block>,
    /// Vendors seen on the response, the blocking one first. Filled for good
    /// responses too, so policy can learn which vendor fronts a domain.
    pub vendors: Vec<Vendor>,
    /// Id of the rule that decided, if one did.
    pub rule: Option<String>,
    /// Whether the scheduler may try again with another persona. From the
    /// rule that decided; `false` when the page was fetched.
    pub retryable: bool,
    /// How long the persona stays away from the domain, as a class the
    /// domain policy turns into a duration.
    pub cooldown: CooldownClass,
    /// Every marker that fired: what the deciding rule matched, then what
    /// the vendor hints matched. Filled for good responses too, so a report
    /// can show why a page was or was not treated as a block.
    pub evidence: Vec<String>,
}

/// A problem in `policies/block-signatures.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    /// The YAML document could not be parsed.
    #[error("block signatures: {0}")]
    Parse(String),
    /// The file declares a version this build does not read.
    #[error("block signatures: version {0} is not supported, expected 1")]
    UnsupportedVersion(u32),
    /// The file has no rules.
    #[error("block signatures: the file defines no rules")]
    NoRules,
    /// Two rules or hints share an id.
    #[error("block signatures: id {0:?} is used more than once")]
    DuplicateId(String),
    /// A rule or hint is invalid.
    #[error("block signatures: {id}: {reason}")]
    Invalid {
        /// The rule or hint.
        id: String,
        /// What is wrong with it.
        reason: String,
    },
}

/// State of the interstitial that an `interstitial` condition asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum InterstitialState {
    /// One was detected, resolved or not.
    Detected,
    /// One was detected and went away by itself.
    Resolved,
    /// One was detected and is still there.
    Unresolved,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignatureFile {
    version: u32,
    #[serde(default)]
    settings: Settings,
    rules: Vec<RuleSpec>,
    #[serde(default)]
    hints: Vec<HintSpec>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    #[serde(default = "default_max_body_bytes")]
    max_body_bytes: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
        }
    }
}

const fn default_max_body_bytes() -> usize {
    DEFAULT_MAX_BODY_BYTES
}

const fn default_priority() -> i32 {
    DEFAULT_PRIORITY
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleSpec {
    id: String,
    vendor: Vendor,
    #[serde(rename = "type")]
    block_type: BlockType,
    outcome: Outcome,
    confidence: f32,
    #[serde(default = "default_priority")]
    priority: i32,
    /// Overrides of what the outcome implies by default.
    #[serde(default)]
    retryable: Option<bool>,
    /// The hint of 1.x, read only to be refused (see the module's
    /// documentation).
    #[serde(default)]
    escalate: Option<Retired>,
    #[serde(default)]
    cooldown: Option<CooldownClass>,
    /// Free text for maintainers; not used by the engine.
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    when: ConditionSpec,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HintSpec {
    id: String,
    vendor: Vendor,
    when: ConditionSpec,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ConditionSpec {
    All(Vec<ConditionSpec>),
    Any(Vec<ConditionSpec>),
    Not(Box<ConditionSpec>),
    Status(OneOrMany<StatusItem>),
    Header(NameOrFull<HeaderMatch>),
    Cookie(NameOrFull<CookieMatch>),
    Body(String),
    Url(String),
    ScriptHost(String),
    IframeHost(String),
    InterstitialText(String),
    VisibleText(CharBounds),
    BodyBytes(ByteBounds),
    Render(Render),
    Interstitial(InterstitialState),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StatusItem {
    Code(u16),
    Range(String),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NameOrFull<T> {
    Name(String),
    Full(T),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderMatch {
    name: String,
    #[serde(default)]
    regex: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CookieMatch {
    name: String,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct CharBounds {
    #[serde(default)]
    min_chars: Option<usize>,
    #[serde(default)]
    max_chars: Option<usize>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct ByteBounds {
    #[serde(default)]
    min: Option<usize>,
    #[serde(default)]
    max: Option<usize>,
}

/// A compiled regular expression with the source it was written as.
#[derive(Debug, Clone)]
struct Pattern {
    source: String,
    regex: Regex,
}

impl Pattern {
    fn compile(source: &str) -> Result<Self, String> {
        if source.is_empty() {
            return Err("a pattern must not be empty".to_owned());
        }
        let regex = RegexBuilder::new(source)
            .size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map_err(|error| format!("pattern {source:?}: {error}"))?;
        Ok(Self {
            source: source.to_owned(),
            regex,
        })
    }

    fn is_match(&self, text: &str) -> bool {
        self.regex.is_match(text)
    }
}

#[derive(Debug, Clone)]
enum Condition {
    All(Vec<Condition>),
    Any(Vec<Condition>),
    Not(Box<Condition>),
    Status(Vec<RangeInclusive<u16>>),
    Header {
        name: String,
        value: Option<Pattern>,
    },
    Cookie {
        name: Pattern,
        value: Option<Pattern>,
    },
    Body(Pattern),
    Url(Pattern),
    ScriptHost(Pattern),
    IframeHost(Pattern),
    InterstitialText(Pattern),
    VisibleText(CharBounds),
    BodyBytes(ByteBounds),
    Render(Render),
    Interstitial(InterstitialState),
}

impl Condition {
    fn compile(spec: ConditionSpec) -> Result<Self, String> {
        let group = |specs: Vec<ConditionSpec>, name: &str| {
            if specs.is_empty() {
                return Err(format!("`{name}` needs at least one condition"));
            }
            specs
                .into_iter()
                .map(Self::compile)
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(match spec {
            ConditionSpec::All(specs) => Self::All(group(specs, "all")?),
            ConditionSpec::Any(specs) => Self::Any(group(specs, "any")?),
            ConditionSpec::Not(spec) => Self::Not(Box::new(Self::compile(*spec)?)),
            ConditionSpec::Status(items) => {
                let items = match items {
                    OneOrMany::One(item) => vec![item],
                    OneOrMany::Many(items) => items,
                };
                if items.is_empty() {
                    return Err("`status` needs at least one code or range".to_owned());
                }
                Self::Status(items.iter().map(status_range).collect::<Result<_, _>>()?)
            }
            ConditionSpec::Header(header) => {
                let (name, regex) = match header {
                    NameOrFull::Name(name) => (name, None),
                    NameOrFull::Full(full) => (full.name, full.regex),
                };
                if name.trim().is_empty() {
                    return Err("`header` needs a name".to_owned());
                }
                Self::Header {
                    name: name.trim().to_ascii_lowercase(),
                    value: regex.as_deref().map(Pattern::compile).transpose()?,
                }
            }
            ConditionSpec::Cookie(cookie) => {
                let (name, value) = match cookie {
                    NameOrFull::Name(name) => (name, None),
                    NameOrFull::Full(full) => (full.name, full.value),
                };
                Self::Cookie {
                    name: Pattern::compile(&name)?,
                    value: value.as_deref().map(Pattern::compile).transpose()?,
                }
            }
            ConditionSpec::Body(pattern) => Self::Body(Pattern::compile(&pattern)?),
            ConditionSpec::Url(pattern) => Self::Url(Pattern::compile(&pattern)?),
            ConditionSpec::ScriptHost(pattern) => Self::ScriptHost(Pattern::compile(&pattern)?),
            ConditionSpec::IframeHost(pattern) => Self::IframeHost(Pattern::compile(&pattern)?),
            ConditionSpec::InterstitialText(pattern) => {
                Self::InterstitialText(Pattern::compile(&pattern)?)
            }
            ConditionSpec::VisibleText(bounds) => {
                check_bounds("visible_text", bounds.min_chars, bounds.max_chars)?;
                Self::VisibleText(bounds)
            }
            ConditionSpec::BodyBytes(bounds) => {
                check_bounds("body_bytes", bounds.min, bounds.max)?;
                Self::BodyBytes(bounds)
            }
            ConditionSpec::Render(render) => Self::Render(render),
            ConditionSpec::Interstitial(state) => Self::Interstitial(state),
        })
    }

    /// Evaluates the condition. Evidence is appended only for the leaves
    /// that made the whole condition true.
    // One arm per kind of condition: the match is the grammar, kept in one place.
    #[allow(clippy::too_many_lines)]
    fn matches(&self, context: &Context<'_>, evidence: &mut Vec<String>) -> bool {
        match self {
            Self::All(conditions) => {
                let mut found = Vec::new();
                if conditions
                    .iter()
                    .all(|condition| condition.matches(context, &mut found))
                {
                    evidence.append(&mut found);
                    true
                } else {
                    false
                }
            }
            Self::Any(conditions) => {
                let mut matched = false;
                for condition in conditions {
                    matched |= condition.matches(context, evidence);
                }
                matched
            }
            // A negation holds because something is absent: no evidence.
            Self::Not(condition) => !condition.matches(context, &mut Vec::new()),
            Self::Status(ranges) => {
                let status = context.observation.status;
                let matched = ranges.iter().any(|range| range.contains(&status));
                if matched {
                    evidence.push(format!("status:{status}"));
                }
                matched
            }
            Self::Header { name, value } => {
                let found = context.observation.headers.iter().find(|header| {
                    header.name.eq_ignore_ascii_case(name)
                        && value
                            .as_ref()
                            .is_none_or(|pattern| pattern.is_match(&header.value))
                });
                if let Some(header) = found {
                    let shown: String = header.value.chars().take(EVIDENCE_VALUE_CHARS).collect();
                    evidence.push(format!("header:{name}={shown}"));
                }
                found.is_some()
            }
            Self::Cookie { name, value } => {
                let found = context.observation.cookies.iter().find(|cookie| {
                    name.is_match(&cookie.name)
                        && value
                            .as_ref()
                            .is_none_or(|pattern| pattern.is_match(&cookie.value))
                });
                if let Some(cookie) = found {
                    evidence.push(format!("cookie:{}", cookie.name));
                }
                found.is_some()
            }
            Self::Body(pattern) => record(pattern.is_match(context.body), evidence, || {
                format!("body:/{}/", pattern.source)
            }),
            Self::Url(pattern) => {
                record(pattern.is_match(&context.observation.url), evidence, || {
                    format!("url:/{}/", pattern.source)
                })
            }
            Self::ScriptHost(pattern) => {
                host_match(context.script_hosts(), pattern, "script_host", evidence)
            }
            Self::IframeHost(pattern) => {
                host_match(context.iframe_hosts(), pattern, "iframe_host", evidence)
            }
            Self::InterstitialText(pattern) => {
                let text = context
                    .observation
                    .browser
                    .as_ref()
                    .and_then(|browser| browser.interstitial_text.as_deref());
                record(
                    text.is_some_and(|text| pattern.is_match(text)),
                    evidence,
                    || format!("interstitial_text:/{}/", pattern.source),
                )
            }
            Self::VisibleText(bounds) => {
                let chars = context.visible_chars();
                record(
                    within(chars, bounds.min_chars, bounds.max_chars),
                    evidence,
                    || format!("visible_text_chars:{chars}"),
                )
            }
            Self::BodyBytes(bounds) => {
                let bytes = context.observation.body.len();
                record(within(bytes, bounds.min, bounds.max), evidence, || {
                    format!("body_bytes:{bytes}")
                })
            }
            Self::Render(render) => record(context.observation.render == *render, evidence, || {
                format!("render:{render}")
            }),
            Self::Interstitial(state) => {
                let browser = context.observation.browser.as_ref();
                let detected = browser.is_some_and(|browser| browser.interstitial_detected);
                let resolved = browser.is_some_and(|browser| browser.interstitial_resolved);
                let (matched, name) = match state {
                    InterstitialState::Detected => (detected, "detected"),
                    InterstitialState::Resolved => (detected && resolved, "resolved"),
                    InterstitialState::Unresolved => (detected && !resolved, "unresolved"),
                };
                record(matched, evidence, || format!("interstitial:{name}"))
            }
        }
    }
}

fn record(matched: bool, evidence: &mut Vec<String>, describe: impl FnOnce() -> String) -> bool {
    if matched {
        evidence.push(describe());
    }
    matched
}

fn host_match(hosts: &[String], pattern: &Pattern, kind: &str, evidence: &mut Vec<String>) -> bool {
    let found = hosts.iter().find(|host| pattern.is_match(host));
    if let Some(host) = found {
        evidence.push(format!("{kind}:{host}"));
    }
    found.is_some()
}

fn within(value: usize, min: Option<usize>, max: Option<usize>) -> bool {
    min.is_none_or(|min| value >= min) && max.is_none_or(|max| value <= max)
}

fn check_bounds(name: &str, min: Option<usize>, max: Option<usize>) -> Result<(), String> {
    match (min, max) {
        (None, None) => Err(format!("`{name}` needs a minimum or a maximum")),
        (Some(min), Some(max)) if min > max => Err(format!(
            "`{name}`: minimum {min} is greater than maximum {max}"
        )),
        _ => Ok(()),
    }
}

fn status_range(item: &StatusItem) -> Result<RangeInclusive<u16>, String> {
    let valid = |code: u16| (100..=599).contains(&code);
    match item {
        StatusItem::Code(code) if valid(*code) => Ok(*code..=*code),
        StatusItem::Code(code) => Err(format!("status {code} is not between 100 and 599")),
        StatusItem::Range(text) => {
            let parsed = text.split_once('-').and_then(|(low, high)| {
                Some((
                    low.trim().parse::<u16>().ok()?,
                    high.trim().parse::<u16>().ok()?,
                ))
            });
            match parsed {
                Some((low, high)) if valid(low) && valid(high) && low <= high => Ok(low..=high),
                _ => Err(format!("status range {text:?} must look like \"500-599\"")),
            }
        }
    }
}

/// One observation plus what is derived from it at most once.
struct Context<'a> {
    observation: &'a Observation,
    /// The body, cut to `settings.max_body_bytes`.
    body: &'a str,
    visible_chars: OnceCell<usize>,
}

impl Context<'_> {
    fn visible_chars(&self) -> usize {
        *self
            .visible_chars
            .get_or_init(|| visible_text(self.body).chars().count())
    }

    fn script_hosts(&self) -> &[String] {
        self.observation
            .browser
            .as_ref()
            .map_or(&[], |browser| browser.script_hosts.as_slice())
    }

    fn iframe_hosts(&self) -> &[String] {
        self.observation
            .browser
            .as_ref()
            .map_or(&[], |browser| browser.iframe_hosts.as_slice())
    }
}

fn truncate(body: &str, max_bytes: usize) -> &str {
    if body.len() <= max_bytes {
        return body;
    }
    let mut end = max_bytes;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    &body[..end]
}

#[derive(Debug, Clone)]
struct Rule {
    id: String,
    vendor: Vendor,
    block_type: BlockType,
    outcome: Outcome,
    confidence: f32,
    priority: i32,
    retryable: bool,
    cooldown: CooldownClass,
    when: Condition,
}

#[derive(Debug, Clone)]
struct Hint {
    vendor: Vendor,
    when: Condition,
}

/// What a signature file says about one rule, for listings and tests.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuleInfo {
    /// Rule id.
    pub id: String,
    /// Vendor it attributes the block to.
    pub vendor: Vendor,
    /// Block type it reports.
    #[serde(rename = "type")]
    pub block_type: BlockType,
    /// Outcome it gives.
    pub outcome: Outcome,
    /// Its confidence.
    pub confidence: f32,
    /// Its priority.
    pub priority: i32,
    /// Whether a match may be retried with another persona.
    pub retryable: bool,
    /// The cooldown class a match carries.
    pub cooldown: CooldownClass,
}

/// The compiled contents of `policies/block-signatures.yaml`.
#[derive(Debug, Clone)]
pub struct SignatureSet {
    max_body_bytes: usize,
    rules: Vec<Rule>,
    hints: Vec<Hint>,
}

impl SignatureSet {
    /// Parses, validates and compiles the YAML document.
    ///
    /// # Errors
    ///
    /// Fails when the document does not parse, declares another version,
    /// has no rules, repeats an id, or contains an invalid rule or pattern.
    pub fn from_yaml(text: &str) -> Result<Self, SignatureError> {
        let file: SignatureFile = crate::yaml::from_str(text).map_err(SignatureError::Parse)?;
        if file.version != SIGNATURE_FILE_VERSION {
            return Err(SignatureError::UnsupportedVersion(file.version));
        }
        if file.rules.is_empty() {
            return Err(SignatureError::NoRules);
        }
        if file.settings.max_body_bytes == 0 {
            return Err(SignatureError::Invalid {
                id: "settings".to_owned(),
                reason: "max_body_bytes must be at least 1".to_owned(),
            });
        }

        let mut ids = std::collections::BTreeSet::new();
        let mut claim = |id: &str| {
            if id.trim().is_empty() {
                return Err(SignatureError::Invalid {
                    id: "(no id)".to_owned(),
                    reason: "every rule and hint needs an id".to_owned(),
                });
            }
            if ids.insert(id.to_owned()) {
                Ok(())
            } else {
                Err(SignatureError::DuplicateId(id.to_owned()))
            }
        };
        let invalid = |id: &str, reason: String| SignatureError::Invalid {
            id: id.to_owned(),
            reason,
        };

        let mut rules = Vec::with_capacity(file.rules.len());
        for spec in file.rules {
            claim(&spec.id)?;
            if !(spec.confidence > 0.0 && spec.confidence <= 1.0) {
                return Err(invalid(
                    &spec.id,
                    "confidence must be above 0 and at most 1".to_owned(),
                ));
            }
            if !rule_outcome_allowed(spec.outcome) {
                return Err(invalid(
                    &spec.id,
                    format!("a rule cannot give the outcome {}", spec.outcome),
                ));
            }
            if spec.escalate.is_some() {
                return Err(invalid(
                    &spec.id,
                    "escalate is gone in 2.0: the level a request names decides its path and \
                     nothing moves it to a browser (spec §25); remove the key"
                        .to_owned(),
                ));
            }
            let overrides = spec.retryable.is_some() || spec.cooldown.is_some();
            if spec.outcome == Outcome::Ok && overrides {
                return Err(invalid(
                    &spec.id,
                    "a rule with the outcome ok cannot set retryable or cooldown".to_owned(),
                ));
            }
            let cooldown = spec.cooldown.unwrap_or(spec.outcome.default_cooldown());
            if cooldown == CooldownClass::Long && !spec.outcome.is_block() {
                return Err(invalid(
                    &spec.id,
                    format!(
                        "cooldown long needs an outcome the policy has a cooldown for; {} has none, use short or none",
                        spec.outcome
                    ),
                ));
            }
            let when = Condition::compile(spec.when).map_err(|reason| invalid(&spec.id, reason))?;
            rules.push(Rule {
                id: spec.id,
                vendor: spec.vendor,
                block_type: spec.block_type,
                outcome: spec.outcome,
                confidence: spec.confidence,
                priority: spec.priority,
                retryable: spec.retryable.unwrap_or(spec.outcome.is_retryable()),
                cooldown,
                when,
            });
        }

        let mut hints = Vec::with_capacity(file.hints.len());
        for spec in file.hints {
            claim(&spec.id)?;
            let when = Condition::compile(spec.when).map_err(|reason| invalid(&spec.id, reason))?;
            hints.push(Hint {
                vendor: spec.vendor,
                when,
            });
        }

        Ok(Self {
            max_body_bytes: file.settings.max_body_bytes,
            rules,
            hints,
        })
    }

    /// Every rule, in file order.
    pub fn rules(&self) -> Vec<RuleInfo> {
        self.rules
            .iter()
            .map(|rule| RuleInfo {
                id: rule.id.clone(),
                vendor: rule.vendor.clone(),
                block_type: rule.block_type.clone(),
                outcome: rule.outcome,
                confidence: rule.confidence,
                priority: rule.priority,
                retryable: rule.retryable,
                cooldown: rule.cooldown,
            })
            .collect()
    }

    /// Number of hints.
    pub fn hint_count(&self) -> usize {
        self.hints.len()
    }

    /// Classifies one observation.
    ///
    /// Every rule is evaluated. Among the rules that match, the one with the
    /// highest priority decides; ties go to the higher confidence, then to
    /// the rule listed first. When no rule matches the outcome is `ok`, or
    /// `challenge_passed` if the browser reports an interstitial that
    /// resolved by itself.
    pub fn classify(&self, observation: &Observation) -> Classification {
        let context = Context {
            observation,
            body: truncate(&observation.body, self.max_body_bytes),
            visible_chars: OnceCell::new(),
        };

        let mut winner: Option<(&Rule, Vec<String>)> = None;
        for rule in &self.rules {
            let mut evidence = Vec::new();
            if !rule.when.matches(&context, &mut evidence) {
                continue;
            }
            let better = winner.as_ref().is_none_or(|(best, _)| {
                (rule.priority, rule.confidence) > (best.priority, best.confidence)
            });
            if better {
                winner = Some((rule, evidence));
            }
        }

        let mut vendors: Vec<Vendor> = Vec::new();
        let mut see = |vendor: &Vendor| {
            if vendor.as_str() != Vendor::GENERIC && !vendors.contains(vendor) {
                vendors.push(vendor.clone());
            }
        };
        if let Some((rule, _)) = &winner
            && rule.outcome != Outcome::Ok
        {
            see(&rule.vendor);
        }
        let mut hint_evidence = Vec::new();
        for hint in &self.hints {
            if hint.when.matches(&context, &mut hint_evidence) {
                see(&hint.vendor);
            }
        }
        // What the deciding rule matched first, then what the hints matched.
        let all_evidence = |rule_evidence: &[String]| {
            unique(
                rule_evidence
                    .iter()
                    .chain(&hint_evidence)
                    .cloned()
                    .collect(),
            )
        };

        let passed_challenge = observation
            .browser
            .as_ref()
            .is_some_and(|browser| browser.interstitial_detected && browser.interstitial_resolved);

        match winner {
            Some((rule, evidence)) if rule.outcome != Outcome::Ok => {
                let evidence = unique(evidence);
                Classification {
                    outcome: rule.outcome,
                    evidence: all_evidence(&evidence),
                    block: Some(Block {
                        vendor: rule.vendor.clone(),
                        block_type: rule.block_type.clone(),
                        confidence: rule.confidence,
                        evidence,
                    }),
                    vendors,
                    rule: Some(rule.id.clone()),
                    retryable: rule.retryable,
                    cooldown: rule.cooldown,
                }
            }
            // The page was fetched: nothing to retry or cool down.
            decided => Classification {
                outcome: if passed_challenge {
                    Outcome::ChallengePassed
                } else {
                    Outcome::Ok
                },
                block: None,
                vendors,
                evidence: all_evidence(decided.as_ref().map_or(&[], |(_, evidence)| evidence)),
                rule: decided.map(|(rule, _)| rule.id.clone()),
                retryable: false,
                cooldown: CooldownClass::None,
            },
        }
    }
}

/// Drops repeated items, keeping the first of each in place.
fn unique(items: Vec<String>) -> Vec<String> {
    let mut kept: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        if !kept.contains(&item) {
            kept.push(item);
        }
    }
    kept
}

/// Outcomes a rule may give. The others are produced elsewhere: `timeout`
/// by the executor, `ssrf_denied` and `robots_denied` before any request,
/// and `challenge_passed` from the browser's interstitial signals.
const fn rule_outcome_allowed(outcome: Outcome) -> bool {
    matches!(
        outcome,
        Outcome::Ok
            | Outcome::ChallengeFailed
            | Outcome::Captcha
            | Outcome::RateLimited
            | Outcome::Forbidden
            | Outcome::SoftBlock
            | Outcome::ProviderError
    )
}

#[cfg(test)]
mod tests {
    use super::{
        BrowserSignals, Classification, Observation, ObservedCookie, SignatureError, SignatureSet,
    };
    use crate::domain::{Header, Render};
    use crate::outcome::{Outcome, Vendor};

    const RULES: &str = r#"
version: 1
rules:
  - id: cf-challenge
    vendor: cloudflare
    type: managed_challenge
    outcome: challenge_failed
    confidence: 0.97
    priority: 80
    when:
      all:
        - status: [403, 503]
        - any:
            - header: { name: CF-Mitigated, regex: "(?i)^challenge$" }
            - body: "Just a moment"
  - id: captcha-page
    vendor: recaptcha
    type: recaptcha
    outcome: captcha
    confidence: 0.9
    priority: 70
    when:
      all:
        - any:
            - iframe_host: "(^|\\.)google\\.com$"
            - body: "google\\.com/recaptcha"
        - visible_text: { max_chars: 40 }
  - id: api-403-is-fine
    vendor: generic
    type: api_error
    outcome: ok
    confidence: 0.6
    priority: 60
    when:
      all:
        - status: 403
        - header: { name: content-type, regex: "application/json" }
  - id: http-403
    vendor: generic
    type: http_403
    outcome: forbidden
    confidence: 0.5
    priority: 10
    when: { status: 403 }
  - id: http-429
    vendor: generic
    type: http_429
    outcome: rate_limited
    confidence: 0.6
    priority: 10
    when: { status: 429 }
  - id: http-5xx
    vendor: generic
    type: server_error
    outcome: provider_error
    confidence: 0.4
    priority: 10
    when: { status: "500-599" }
  - id: empty-body
    vendor: generic
    type: empty_body
    outcome: soft_block
    confidence: 0.7
    when:
      all:
        - status: 200
        - body_bytes: { max: 0 }
        - not: { render: browser }
hints:
  - id: cf-cookies
    vendor: cloudflare
    when: { cookie: "^(__cf_bm|cf_clearance)$" }
  - id: akamai-validated-cookie
    vendor: akamai
    when: { cookie: { name: "^_abck$", value: "~0~" } }
  - id: px-script
    vendor: human
    when: { script_host: "px-cdn\\.net$" }
"#;

    fn set() -> SignatureSet {
        SignatureSet::from_yaml(RULES).unwrap()
    }

    fn observation(status: u16, body: &str) -> Observation {
        Observation {
            render: Render::Http,
            status,
            url: "https://shop.example/".to_owned(),
            headers: Vec::new(),
            cookies: Vec::new(),
            body: body.to_owned(),
            browser: None,
        }
    }

    fn header(name: &str, value: &str) -> Header {
        Header {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }

    fn cookie(name: &str, value: &str) -> ObservedCookie {
        ObservedCookie {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }

    fn classify(observation: &Observation) -> Classification {
        set().classify(observation)
    }

    const PAGE: &str = "<html><body><h1>Shop</h1><p>Plenty of ordinary visible text on this page, well over forty characters.</p></body></html>";

    #[test]
    fn a_plain_page_is_ok_with_no_block_and_no_vendor() {
        let verdict = classify(&observation(200, PAGE));

        assert_eq!(verdict.outcome, Outcome::Ok);
        assert_eq!(verdict.block, None);
        assert_eq!(verdict.vendors, []);
        assert_eq!(verdict.rule, None);
    }

    #[test]
    fn a_vendor_rule_reports_vendor_type_confidence_and_evidence() {
        let mut blocked = observation(403, "<title>Just a moment...</title>");
        blocked.headers.push(header("cf-mitigated", "challenge"));

        let verdict = classify(&blocked);
        let block = verdict.block.unwrap();

        assert_eq!(verdict.outcome, Outcome::ChallengeFailed);
        assert_eq!(verdict.rule.as_deref(), Some("cf-challenge"));
        assert_eq!(block.vendor.as_str(), "cloudflare");
        assert_eq!(block.block_type.as_str(), "managed_challenge");
        assert!((block.confidence - 0.97).abs() < f32::EPSILON);
        assert_eq!(
            block.evidence,
            [
                "status:403",
                "header:cf-mitigated=challenge",
                "body:/Just a moment/"
            ]
        );
        assert_eq!(verdict.vendors.len(), 1);
        assert_eq!(verdict.vendors[0].as_str(), "cloudflare");
    }

    #[test]
    fn vendor_rules_outrank_generic_status_rules() {
        // Both cf-challenge and http-403 match; priority decides.
        let verdict = classify(&observation(403, "Just a moment"));
        assert_eq!(verdict.rule.as_deref(), Some("cf-challenge"));

        // Without the marker only the generic rule is left.
        let verdict = classify(&observation(403, PAGE));
        assert_eq!(verdict.outcome, Outcome::Forbidden);
        assert_eq!(verdict.rule.as_deref(), Some("http-403"));
        let block = verdict.block.unwrap();
        assert_eq!(block.vendor.as_str(), "generic");
        assert_eq!(block.evidence, ["status:403"]);
        // `generic` is not a vendor sighting.
        assert_eq!(verdict.vendors, []);
    }

    #[test]
    fn generic_statuses_map_to_their_outcomes() {
        assert_eq!(
            classify(&observation(429, PAGE)).outcome,
            Outcome::RateLimited
        );
        for status in [500, 502, 503, 599] {
            let verdict = classify(&observation(status, PAGE));
            assert_eq!(verdict.outcome, Outcome::ProviderError, "{status}");
            assert_eq!(verdict.block.unwrap().block_type.as_str(), "server_error");
        }
        // Other 4xx are returned as they are (spec §8.3).
        for status in [400, 404, 410, 451] {
            assert_eq!(
                classify(&observation(status, PAGE)).outcome,
                Outcome::Ok,
                "{status}"
            );
        }
    }

    #[test]
    fn an_ok_rule_can_shield_a_response_from_a_weaker_block_rule() {
        let mut api = observation(403, r#"{"error":"forbidden"}"#);
        api.headers
            .push(header("Content-Type", "application/json; charset=utf-8"));

        let verdict = classify(&api);

        assert_eq!(verdict.outcome, Outcome::Ok);
        assert_eq!(verdict.block, None);
        assert_eq!(verdict.rule.as_deref(), Some("api-403-is-fine"));
    }

    #[test]
    fn vendor_cookies_alone_are_a_hint_not_a_block() {
        let mut good = observation(200, PAGE);
        good.cookies.push(cookie("__cf_bm", "opaque"));
        good.cookies.push(cookie("_abck", "ABC~0~DEF"));

        let verdict = classify(&good);

        assert_eq!(verdict.outcome, Outcome::Ok);
        assert_eq!(verdict.block, None);
        let vendors: Vec<&str> = verdict.vendors.iter().map(Vendor::as_str).collect();
        assert_eq!(vendors, ["cloudflare", "akamai"]);
    }

    #[test]
    fn cookie_value_patterns_are_honoured_and_values_stay_out_of_evidence() {
        let mut unvalidated = observation(200, PAGE);
        unvalidated.cookies.push(cookie("_abck", "ABC~-1~DEF"));
        assert_eq!(classify(&unvalidated).vendors, []);

        let rules = RULES.replace(
            "when: { status: 429 }",
            "when: { all: [ { status: 429 }, { cookie: { name: \"^sess$\", value: \"^secret-\" } } ] }",
        );
        let mut limited = observation(429, PAGE);
        limited.cookies.push(cookie("sess", "secret-token-value"));
        let block = SignatureSet::from_yaml(&rules)
            .unwrap()
            .classify(&limited)
            .block
            .unwrap();
        assert_eq!(block.evidence, ["status:429", "cookie:sess"]);
    }

    #[test]
    fn the_blocking_vendor_is_listed_before_hinted_ones() {
        let mut blocked = observation(503, "Just a moment");
        blocked.cookies.push(cookie("_abck", "x~0~y"));
        blocked.cookies.push(cookie("cf_clearance", "z"));

        let vendors: Vec<String> = classify(&blocked)
            .vendors
            .iter()
            .map(ToString::to_string)
            .collect();

        assert_eq!(vendors, ["cloudflare", "akamai"]);
    }

    #[test]
    fn a_captcha_widget_on_a_full_page_is_not_a_captcha_page() {
        let widget =
            format!("{PAGE}<script src=\"https://www.google.com/recaptcha/api.js\"></script>");
        assert_eq!(classify(&observation(200, &widget)).outcome, Outcome::Ok);

        let wall = "<html><body><p>Verify</p><script src=\"https://www.google.com/recaptcha/api.js\"></script></body></html>";
        let verdict = classify(&observation(200, wall));
        assert_eq!(verdict.outcome, Outcome::Captcha);
        assert_eq!(
            verdict.block.unwrap().evidence,
            ["body:/google\\.com/recaptcha/", "visible_text_chars:6"]
        );
    }

    #[test]
    fn browser_signals_feed_host_and_interstitial_conditions() {
        let mut page = observation(200, "<p>Verify</p>");
        page.render = Render::Browser;
        page.browser = Some(BrowserSignals {
            script_hosts: vec!["client.px-cdn.net".to_owned()],
            iframe_hosts: vec!["www.google.com".to_owned()],
            ..BrowserSignals::default()
        });

        let verdict = classify(&page);

        assert_eq!(verdict.outcome, Outcome::Captcha);
        let block = verdict.block.unwrap();
        assert_eq!(
            block.evidence,
            ["iframe_host:www.google.com", "visible_text_chars:6"]
        );
        let vendors: Vec<&str> = verdict.vendors.iter().map(Vendor::as_str).collect();
        assert_eq!(vendors, ["recaptcha", "human"]);

        // The same conditions are simply false without browser signals.
        assert_eq!(classify(&observation(200, PAGE)).vendors.len(), 0);
    }

    #[test]
    fn a_resolved_interstitial_is_a_passed_challenge() {
        let mut page = observation(200, PAGE);
        page.render = Render::Browser;
        page.browser = Some(BrowserSignals {
            interstitial_detected: true,
            interstitial_resolved: true,
            ..BrowserSignals::default()
        });
        page.cookies.push(cookie("cf_clearance", "x"));

        let verdict = classify(&page);
        assert_eq!(verdict.outcome, Outcome::ChallengePassed);
        assert_eq!(verdict.block, None);
        assert_eq!(verdict.vendors[0].as_str(), "cloudflare");

        // Still showing the interstitial: the block rule decides.
        let mut stuck = observation(403, "Just a moment");
        stuck.browser = Some(BrowserSignals {
            interstitial_detected: true,
            interstitial_resolved: false,
            ..BrowserSignals::default()
        });
        assert_eq!(classify(&stuck).outcome, Outcome::ChallengeFailed);

        // Resolved, but the page behind it is a block: the block wins.
        let mut blocked_after = observation(403, PAGE);
        blocked_after.browser = Some(BrowserSignals {
            interstitial_detected: true,
            interstitial_resolved: true,
            ..BrowserSignals::default()
        });
        assert_eq!(classify(&blocked_after).outcome, Outcome::Forbidden);
    }

    #[test]
    fn not_and_render_and_body_size_conditions() {
        let empty = observation(200, "");
        let verdict = classify(&empty);
        assert_eq!(verdict.outcome, Outcome::SoftBlock);
        // The negated condition leaves no evidence.
        assert_eq!(
            verdict.block.unwrap().evidence,
            ["status:200", "body_bytes:0"]
        );

        let mut in_browser = observation(200, "");
        in_browser.render = Render::Browser;
        assert_eq!(classify(&in_browser).outcome, Outcome::Ok);
    }

    fn rule_with(when: &str) -> Result<SignatureSet, SignatureError> {
        SignatureSet::from_yaml(&format!(
            "version: 1\nrules:\n  - id: r\n    vendor: generic\n    type: t\n    outcome: forbidden\n    confidence: 0.5\n    when: {when}\n"
        ))
    }

    #[test]
    fn every_leaf_condition_evaluates() {
        let mut page = observation(403, "<p>Access Denied</p>");
        page.url = "https://shop.example/blocked?ref=18.abc".to_owned();
        page.headers.push(header("Server", "AkamaiGHost"));
        page.cookies.push(cookie("bm_sz", "v"));
        page.browser = Some(BrowserSignals {
            script_hosts: vec!["js.example-cdn.net".to_owned()],
            iframe_hosts: vec!["geo.captcha-delivery.com".to_owned()],
            interstitial_text: Some("Press & Hold".to_owned()),
            interstitial_detected: true,
            interstitial_resolved: false,
        });

        let cases = [
            ("{ status: 403 }", "status:403"),
            ("{ status: [200, \"400-499\"] }", "status:403"),
            ("{ header: server }", "header:server=AkamaiGHost"),
            (
                "{ header: { name: SERVER, regex: \"^Akamai\" } }",
                "header:server=AkamaiGHost",
            ),
            ("{ cookie: \"^bm_\" }", "cookie:bm_sz"),
            ("{ body: \"Access Denied\" }", "body:/Access Denied/"),
            ("{ url: \"ref=18\\\\.\" }", "url:/ref=18\\./"),
            (
                "{ script_host: \"example-cdn\" }",
                "script_host:js.example-cdn.net",
            ),
            (
                "{ iframe_host: \"captcha-delivery\\\\.com$\" }",
                "iframe_host:geo.captcha-delivery.com",
            ),
            (
                "{ interstitial_text: \"(?i)press & hold\" }",
                "interstitial_text:/(?i)press & hold/",
            ),
            (
                "{ visible_text: { min_chars: 5, max_chars: 20 } }",
                "visible_text_chars:13",
            ),
            ("{ body_bytes: { min: 1 } }", "body_bytes:20"),
            ("{ render: http }", "render:http"),
            ("{ interstitial: detected }", "interstitial:detected"),
            ("{ interstitial: unresolved }", "interstitial:unresolved"),
        ];
        for (when, expected) in cases {
            let verdict = rule_with(when).unwrap().classify(&page);
            assert_eq!(verdict.outcome, Outcome::Forbidden, "{when}");
            assert_eq!(verdict.block.unwrap().evidence, [expected], "{when}");
        }

        for when in [
            "{ status: 404 }",
            "{ header: x-missing }",
            "{ header: { name: server, regex: \"^nginx\" } }",
            "{ cookie: \"^datadome$\" }",
            "{ cookie: { name: \"^bm_sz$\", value: \"^other$\" } }",
            "{ body: \"access denied\" }",
            "{ url: \"/checkout\" }",
            "{ script_host: \"px-cdn\" }",
            "{ iframe_host: \"hcaptcha\" }",
            "{ interstitial_text: \"Just a moment\" }",
            "{ visible_text: { max_chars: 5 } }",
            "{ body_bytes: { max: 3 } }",
            "{ render: browser }",
            "{ interstitial: resolved }",
            "{ not: { status: 403 } }",
            "{ all: [ { status: 403 }, { body: \"nope\" } ] }",
            "{ any: [ { status: 500 }, { body: \"nope\" } ] }",
        ] {
            assert_eq!(
                rule_with(when).unwrap().classify(&page).outcome,
                Outcome::Ok,
                "{when}"
            );
        }
    }

    #[test]
    fn header_evidence_is_truncated() {
        let mut page = observation(403, "");
        page.headers.push(header("x-long", &"v".repeat(300)));

        let block = rule_with("{ header: x-long }")
            .unwrap()
            .classify(&page)
            .block
            .unwrap();

        assert_eq!(block.evidence[0].len(), "header:x-long=".len() + 80);
    }

    #[test]
    fn only_the_first_megabyte_of_a_body_is_scanned() {
        let filler = "é".repeat(600_000);
        let late = format!("{filler}MARKER");
        let early = format!("MARKER{filler}");
        let set = rule_with("{ body: \"MARKER\" }").unwrap();

        assert_eq!(
            set.classify(&observation(200, &early)).outcome,
            Outcome::Forbidden
        );
        assert_eq!(set.classify(&observation(200, &late)).outcome, Outcome::Ok);

        let small = SignatureSet::from_yaml(
            "version: 1\nsettings: { max_body_bytes: 3 }\nrules:\n  - { id: r, vendor: generic, type: t, outcome: forbidden, confidence: 0.5, when: { body: \"é\" } }\n",
        )
        .unwrap();
        // The cut lands inside the second "é" and backs off to a boundary.
        assert_eq!(
            small.classify(&observation(200, "éé")).outcome,
            Outcome::Forbidden
        );
        assert_eq!(super::truncate("éé", 3), "é");
        assert_eq!(super::truncate("abc", 10), "abc");
    }

    #[test]
    fn ties_go_to_confidence_and_then_to_file_order() {
        let tie = "
version: 1
rules:
  - { id: first, vendor: generic, type: a, outcome: forbidden, confidence: 0.5, when: { status: 403 } }
  - { id: surer, vendor: generic, type: b, outcome: rate_limited, confidence: 0.8, when: { status: 403 } }
  - { id: same, vendor: generic, type: c, outcome: captcha, confidence: 0.8, when: { status: 403 } }
";
        let verdict = SignatureSet::from_yaml(tie)
            .unwrap()
            .classify(&observation(403, ""));

        assert_eq!(verdict.rule.as_deref(), Some("surer"));
    }

    fn hinted(outcome: &str, overrides: &str) -> Result<SignatureSet, SignatureError> {
        SignatureSet::from_yaml(&format!(
            "version: 1\nrules:\n  - id: r\n    vendor: generic\n    type: t\n    outcome: {outcome}\n    confidence: 0.5\n{overrides}    when: {{ status: 418 }}\n"
        ))
    }

    #[test]
    fn scheduler_hints_default_to_what_the_outcome_implies() {
        use crate::outcome::CooldownClass;

        let teapot = observation(418, PAGE);
        for (outcome, retryable, cooldown) in [
            ("challenge_failed", true, CooldownClass::Long),
            ("captcha", true, CooldownClass::Long),
            ("forbidden", true, CooldownClass::Long),
            ("soft_block", true, CooldownClass::Long),
            ("rate_limited", true, CooldownClass::Long),
            ("provider_error", true, CooldownClass::None),
        ] {
            let verdict = hinted(outcome, "").unwrap().classify(&teapot);
            assert_eq!(
                (verdict.retryable, verdict.cooldown),
                (retryable, cooldown),
                "{outcome}"
            );
        }

        // No rule matched: nothing to retry or cool down.
        let fetched = classify(&observation(200, PAGE));
        assert!(!fetched.retryable);
        assert_eq!(fetched.cooldown, CooldownClass::None);
        assert_eq!(fetched.evidence, Vec::<String>::new());
    }

    #[test]
    fn a_rule_can_override_each_scheduler_hint() {
        use crate::outcome::CooldownClass;

        let teapot = observation(418, PAGE);
        let set = hinted("soft_block", "    retryable: false\n    cooldown: none\n").unwrap();
        let verdict = set.classify(&teapot);
        assert_eq!(
            (verdict.retryable, verdict.cooldown),
            (false, CooldownClass::None)
        );
        let listed = &set.rules()[0];
        assert_eq!(
            (listed.retryable, listed.cooldown),
            (false, CooldownClass::None)
        );

        let short = hinted("forbidden", "    cooldown: short\n")
            .unwrap()
            .classify(&teapot);
        assert_eq!(short.cooldown, CooldownClass::Short);
        assert!(short.retryable);

        // A short cooldown is allowed on an outcome with no cooldown of its own.
        let pause = hinted("provider_error", "    cooldown: short\n")
            .unwrap()
            .classify(&teapot);
        assert_eq!(pause.cooldown, CooldownClass::Short);
    }

    #[test]
    fn hints_that_contradict_the_outcome_are_rejected() {
        for (outcome, overrides, needle) in [
            ("ok", "    retryable: true\n", "outcome ok cannot set"),
            ("forbidden", "    escalate: true\n", "escalate is gone"),
            ("ok", "    escalate: false\n", "escalate is gone"),
            ("ok", "    cooldown: none\n", "outcome ok cannot set"),
            (
                "provider_error",
                "    cooldown: long\n",
                "cooldown long needs",
            ),
        ] {
            let error = hinted(outcome, overrides).unwrap_err();
            assert!(
                matches!(&error, SignatureError::Invalid { id, reason } if id == "r" && reason.contains(needle)),
                "{outcome} {overrides:?}: {error}"
            );
        }
        assert!(matches!(
            hinted("forbidden", "    cooldown: forever\n"),
            Err(SignatureError::Parse(_))
        ));
    }

    #[test]
    fn evidence_lists_the_rule_markers_then_the_hint_markers_without_repeats() {
        // Blocked: the rule's evidence, then what the hints saw.
        let mut blocked = observation(503, "Just a moment");
        blocked.cookies.push(cookie("cf_clearance", "z"));
        blocked.cookies.push(cookie("_abck", "x~0~y"));
        let verdict = classify(&blocked);
        assert_eq!(
            verdict.evidence,
            [
                "status:503",
                "body:/Just a moment/",
                "cookie:cf_clearance",
                "cookie:_abck"
            ]
        );
        assert_eq!(
            verdict.block.unwrap().evidence,
            ["status:503", "body:/Just a moment/"]
        );

        // Fetched: only the hints, and the page is still ok.
        let mut good = observation(200, PAGE);
        good.cookies.push(cookie("__cf_bm", "opaque"));
        let verdict = classify(&good);
        assert_eq!(verdict.outcome, Outcome::Ok);
        assert_eq!(verdict.evidence, ["cookie:__cf_bm"]);

        // An ok rule that shields a response is evidence too.
        let mut api = observation(403, "{}");
        api.headers.push(header("content-type", "application/json"));
        let verdict = classify(&api);
        assert_eq!(verdict.outcome, Outcome::Ok);
        assert_eq!(
            verdict.evidence,
            ["status:403", "header:content-type=application/json"]
        );

        // The same marker matched twice is listed once.
        let twice = rule_with(
            "{ all: [ { status: 403 }, { any: [ { status: 403 }, { status: [403, 404] } ] } ] }",
        )
        .unwrap()
        .classify(&observation(403, ""));
        assert_eq!(twice.evidence, ["status:403"]);
        assert_eq!(twice.block.unwrap().evidence, ["status:403"]);
    }

    #[test]
    fn listing_reports_every_rule() {
        let set = set();
        let rules = set.rules();

        assert_eq!(rules.len(), 7);
        assert_eq!(set.hint_count(), 3);
        assert_eq!(rules[0].id, "cf-challenge");
        assert_eq!(rules[0].priority, 80);
        assert_eq!(rules[6].id, "empty-body");
        assert_eq!(rules[6].priority, 50);
        let json = serde_json::to_value(&rules[0]).unwrap();
        assert_eq!(json["type"], "managed_challenge");
        assert_eq!(json["outcome"], "challenge_failed");
    }

    #[test]
    fn structural_problems_are_rejected() {
        assert!(matches!(
            SignatureSet::from_yaml("version: ["),
            Err(SignatureError::Parse(_))
        ));
        assert!(matches!(
            SignatureSet::from_yaml("rules: []"),
            Err(SignatureError::Parse(_))
        ));
        assert_eq!(
            SignatureSet::from_yaml("version: 2\nrules: []\n").unwrap_err(),
            SignatureError::UnsupportedVersion(2)
        );
        assert_eq!(
            SignatureSet::from_yaml("version: 1\nrules: []\n").unwrap_err(),
            SignatureError::NoRules
        );
        assert!(matches!(
            SignatureSet::from_yaml(&RULES.replace("version: 1", "version: 1\nsettings: { max_body_bytes: 0 }")),
            Err(SignatureError::Invalid { id, .. }) if id == "settings"
        ));
        assert_eq!(
            SignatureSet::from_yaml(&RULES.replace("id: http-429", "id: http-403")).unwrap_err(),
            SignatureError::DuplicateId("http-403".to_owned())
        );
        assert_eq!(
            SignatureSet::from_yaml(&RULES.replace("id: px-script", "id: http-403")).unwrap_err(),
            SignatureError::DuplicateId("http-403".to_owned())
        );
        assert!(matches!(
            SignatureSet::from_yaml(&RULES.replace("id: http-429", "id: \" \"")),
            Err(SignatureError::Invalid { .. })
        ));
        // Unknown keys and unknown conditions are parse errors.
        assert!(matches!(
            SignatureSet::from_yaml(&RULES.replace("priority: 80", "prio: 80")),
            Err(SignatureError::Parse(_))
        ));
        assert!(matches!(
            rule_with("{ smell: bad }"),
            Err(SignatureError::Parse(_))
        ));
        assert!(matches!(
            SignatureSet::from_yaml(&RULES.replace("vendor: recaptcha", "vendor: \"Re Captcha\"")),
            Err(SignatureError::Parse(_))
        ));
    }

    fn invalid_reason(yaml: &str) -> String {
        match SignatureSet::from_yaml(yaml) {
            Err(SignatureError::Invalid { reason, .. }) => reason,
            other => panic!("expected an invalid rule, got {other:?}"),
        }
    }

    #[test]
    fn invalid_rules_are_rejected_with_a_reason() {
        for (when, needle) in [
            ("{ body: \"(unclosed\" }", "pattern"),
            ("{ body: \"\" }", "must not be empty"),
            ("{ all: [] }", "at least one condition"),
            ("{ any: [] }", "at least one condition"),
            ("{ status: [] }", "at least one code"),
            ("{ status: 99 }", "between 100 and 599"),
            ("{ status: \"599-500\" }", "must look like"),
            ("{ status: \"abc\" }", "must look like"),
            ("{ status: \"100-700\" }", "must look like"),
            ("{ header: \" \" }", "needs a name"),
            ("{ visible_text: {} }", "needs a minimum or a maximum"),
            ("{ body_bytes: { min: 9, max: 1 } }", "greater than maximum"),
            ("{ not: { cookie: { name: \"[\" } } }", "pattern"),
            ("{ header: { name: x, regex: \"[\" } }", "pattern"),
        ] {
            let error = rule_with(when).unwrap_err();
            assert!(
                matches!(&error, SignatureError::Invalid { id, reason } if id == "r" && reason.contains(needle)),
                "{when}: {error}"
            );
        }

        assert!(
            invalid_reason(&RULES.replace("confidence: 0.97", "confidence: 0"))
                .contains("confidence")
        );
        assert!(
            invalid_reason(&RULES.replace("confidence: 0.97", "confidence: 1.5"))
                .contains("confidence")
        );
        assert!(
            invalid_reason(&RULES.replace("outcome: challenge_failed", "outcome: timeout"))
                .contains("cannot give the outcome timeout")
        );
        assert!(
            invalid_reason(&RULES.replace(
                "when: { script_host: \"px-cdn\\\\.net$\" }",
                "when: { script_host: \"(\" }"
            ))
            .contains("pattern")
        );
    }

    #[test]
    fn observation_round_trips_as_json_with_defaults() {
        let minimal: Observation =
            serde_json::from_str(r#"{"render":"http","status":200,"url":"https://a.example/"}"#)
                .unwrap();

        assert_eq!(minimal.headers, []);
        assert_eq!(minimal.cookies, []);
        assert_eq!(minimal.body, "");
        assert_eq!(minimal.browser, None);

        let json = serde_json::to_string(&minimal).unwrap();
        assert_eq!(serde_json::from_str::<Observation>(&json).unwrap(), minimal);

        let verdict = serde_json::to_value(classify(&minimal)).unwrap();
        assert_eq!(verdict["outcome"], "soft_block");
    }
}
