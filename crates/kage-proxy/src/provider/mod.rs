//! The proxy provider: where the list of proxies comes from and who
//! replaces a burned one (spec §7.1, ADR 0013).
//!
//! [`ProxyProvider`] is the plugin surface. Listing is the one thing every
//! provider does; replacement, the log of replaced proxies, the account
//! and the traffic stats are optional, and a provider says which it offers
//! in [`ProxyProvider::capabilities`]. An operation it does not offer
//! answers [`ProviderError::Unsupported`] (the trait's default bodies do),
//! and the pool, the CLI and the admin API show it as unsupported instead
//! of failing.
//!
//! Two implementations: [`WebshareProvider`] talks to the Webshare API and
//! offers everything; [`StaticListProvider`] serves the operator's declared
//! list and only lists. A new provider is one module here, its
//! configuration section, its pins in `docs/versions.md`, and a pass of the
//! conformance suite (`tests/provider_conformance.rs`) against a mock of
//! its API; `CONTRIBUTING.md` walks through it.

mod static_list;
mod webshare;

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kage_core::config::ResolvedProvider;
use kage_core::domain::{CountryCode, Proxy, ProxyProviderKind};
use kage_core::secret::Secret;

pub use static_list::StaticListProvider;
pub use webshare::{
    CAPABILITIES as WEBSHARE_CAPABILITIES, ECHO_URL as WEBSHARE_ECHO_URL, WebshareProvider,
};

/// The echo the health probes read through the proxies of a provider that
/// runs none of its own, unless `proxies.health.echo_url` names another
/// (decision D51): the public echo 1.0 already asked as its confirmation.
/// Its operator sees every proxy address on the probe's schedule; the
/// operator's own echo is the private alternative.
pub const PUBLIC_ECHO_URL: &str = "https://checkip.amazonaws.com/";

/// What the provider the configuration resolves to can do, without
/// building it (for `kage config check` and the defaults that follow from
/// it); `None` without a provider. A new provider adds its arm here.
pub const fn capabilities_of(provider: ResolvedProvider) -> Option<Capabilities> {
    match provider {
        ResolvedProvider::Webshare => Some(WEBSHARE_CAPABILITIES),
        ResolvedProvider::Static => Some(Capabilities::LIST_ONLY),
        ResolvedProvider::None => None,
    }
}

/// The echo every reachability probe reads through a proxy when
/// `proxies.health.echo_url` is unset: the provider's own, else
/// [`PUBLIC_ECHO_URL`].
pub fn default_echo(provider: ResolvedProvider) -> &'static str {
    capabilities_of(provider)
        .and_then(|capabilities| capabilities.echo)
        .unwrap_or(PUBLIC_ECHO_URL)
}

/// Result type of every provider call.
pub type Result<T> = std::result::Result<T, ProviderError>;

/// Why a provider call failed. No variant carries credentials.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// The provider client could not be built from its configuration.
    #[error("the provider client cannot be set up: {0}")]
    Setup(String),
    /// The provider cannot do this at all (a static list cannot replace):
    /// its [`ProxyProvider::capabilities`] say so.
    #[error("the {provider} provider does not support {operation}")]
    Unsupported {
        /// The provider.
        provider: ProxyProviderKind,
        /// What was asked of it: one of [`Operation`]'s names.
        operation: &'static str,
    },
    /// The request did not complete: connection, TLS or timeout.
    #[error("the provider API could not be reached: {0}")]
    Transport(String),
    /// The provider refused the API key.
    #[error("the provider API refused the API key (HTTP {status})")]
    Unauthorized {
        /// The HTTP status, 401 or 403.
        status: u16,
    },
    /// The provider answered 429 and the retries are used up.
    #[error("the provider API is rate limiting; retry after {retry_after:?}")]
    RateLimited {
        /// How long the provider asked to stay away.
        retry_after: Duration,
    },
    /// The provider answered with an error status.
    #[error("the provider API answered HTTP {status}: {message}")]
    Status {
        /// The HTTP status.
        status: u16,
        /// The provider's error code, when the body names one.
        code: Option<String>,
        /// The provider's message, or a generic one.
        message: String,
    },
    /// The provider's answer is not what its documentation describes.
    #[error("the provider API answer could not be read: {0}")]
    Decode(String),
    /// The operator's declared list cannot be read or does not parse. The
    /// message names the entry and the rule, never the list's content; the
    /// pool keeps what it has.
    #[error("the declared proxy list is not in force: {0}; the last good list stays")]
    InvalidList(String),
    /// The subscription has no replacements left.
    #[error("the subscription has no proxy replacements left")]
    QuotaExhausted,
    /// Another replacement is still running; the provider takes one at a
    /// time.
    #[error("another proxy replacement is still running at the provider")]
    Busy,
    /// A request that changes something was sent but its outcome is
    /// unknown (the connection broke or the server failed). It is not sent
    /// again: it may have been applied.
    #[error("the outcome of the request is unknown: {0}")]
    OutcomeUnknown(String),
}

impl ProviderError {
    /// The answer of a provider to an operation it does not offer.
    pub const fn unsupported(provider: ProxyProviderKind, operation: Operation) -> Self {
        Self::Unsupported {
            provider,
            operation: operation.as_str(),
        }
    }

    /// Whether trying again later can help without changing anything.
    pub const fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Transport(_)
                | Self::RateLimited { .. }
                | Self::Busy
                | Self::Status {
                    status: 500..=599,
                    ..
                }
        )
    }
}

/// One proxy as the provider lists it.
#[derive(Debug, Clone)]
pub struct ProviderProxy {
    /// The provider's own id for the proxy.
    pub provider_ref: String,
    /// Proxy host name or address.
    pub host: String,
    /// Proxy port.
    pub port: u16,
    /// Proxy user name.
    pub username: Secret,
    /// Proxy password.
    pub password: Secret,
    /// Country of the exit address, as the provider states it: a
    /// declaration for a provider that declares
    /// ([`ProxyProviderKind::declares_identity`]), else a fallback the
    /// observed geo record overrides.
    pub country: Option<CountryCode>,
    /// The locale its persona speaks, as declared (BCP 47); `None` for the
    /// country's primary language. Only a declaring provider sets it.
    pub locale: Option<String>,
    /// The IANA time zone of its persona, as declared. Only a declaring
    /// provider sets it.
    pub timezone: Option<String>,
    /// City of the exit address, as the provider states it; informational.
    pub city: Option<String>,
    /// Autonomous system of the exit address, when the provider names it.
    pub asn: Option<String>,
    /// Whether the provider's own checks consider the proxy working.
    pub valid: Option<bool>,
}

/// What to replace a proxy with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReplaceSpec {
    /// The country the new proxy must be in. `None` lets the provider pick.
    pub country: Option<CountryCode>,
}

/// The provider's handle for a replacement it accepted.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReplaceTicket(pub String);

/// How far a replacement has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceState {
    /// The provider is still working on it.
    Pending,
    /// The provider replaced the proxy.
    Done {
        /// Proxies removed from the list.
        removed: u32,
        /// Proxies added to the list.
        added: u32,
    },
    /// The provider gave up; nothing was replaced.
    Failed {
        /// The provider's error code, when it names one.
        code: Option<String>,
        /// The provider's message.
        message: String,
    },
}

/// One entry of the provider's log of replaced proxies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacedProxy {
    /// Why it was replaced, in the provider's words: `proxy_replaced` for a
    /// request through the API, `auto_…` when the provider did it itself.
    pub reason: String,
    /// Address of the proxy that was removed.
    pub old_host: String,
    /// Its port.
    pub old_port: Option<u16>,
    /// Address of the proxy that took its place.
    pub new_host: Option<String>,
    /// Its port.
    pub new_port: Option<u16>,
    /// Its country.
    pub new_country: Option<CountryCode>,
    /// When the replacement happened.
    pub at: DateTime<Utc>,
}

/// The provider's settings that replace proxies without being asked.
// Four independent switches of the provider account, not a state.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct AutoReplace {
    /// Replaces proxies its own checks found invalid for 15 minutes.
    pub invalid: bool,
    /// Replaces proxies whose country it is no longer sure of.
    pub low_country_confidence: bool,
    /// Replaces proxies it took out of rotation.
    pub out_of_rotation: bool,
    /// Replaces proxies that no longer pass its site checks.
    pub failed_site_check: bool,
}

impl AutoReplace {
    /// The provider's names of the four settings, in the order of the
    /// fields.
    pub const SETTINGS: [&'static str; 4] = [
        "auto_replace_invalid_proxies",
        "auto_replace_low_country_confidence_proxies",
        "auto_replace_out_of_rotation_proxies",
        "auto_replace_failed_site_check_proxies",
    ];

    /// The settings named in `names`, as flags. A name that is none of
    /// [`Self::SETTINGS`] is returned as the error.
    ///
    /// # Errors
    ///
    /// The first unknown name.
    pub fn from_names<N: AsRef<str>>(names: &[N]) -> std::result::Result<Self, String> {
        let mut flags = Self::default();
        for name in names {
            match name.as_ref() {
                "auto_replace_invalid_proxies" => flags.invalid = true,
                "auto_replace_low_country_confidence_proxies" => {
                    flags.low_country_confidence = true;
                }
                "auto_replace_out_of_rotation_proxies" => flags.out_of_rotation = true,
                "auto_replace_failed_site_check_proxies" => flags.failed_site_check = true,
                other => return Err(other.to_owned()),
            }
        }
        Ok(flags)
    }

    /// The names of the settings that are switched on.
    pub fn enabled(&self) -> Vec<&'static str> {
        [
            self.invalid,
            self.low_country_confidence,
            self.out_of_rotation,
            self.failed_site_check,
        ]
        .into_iter()
        .zip(Self::SETTINGS)
        .filter_map(|(on, name)| on.then_some(name))
        .collect()
    }

    /// The settings that are on here and off in `other`.
    #[must_use]
    pub const fn without(&self, other: &Self) -> Self {
        Self {
            invalid: self.invalid && !other.invalid,
            low_country_confidence: self.low_country_confidence && !other.low_country_confidence,
            out_of_rotation: self.out_of_rotation && !other.out_of_rotation,
            failed_site_check: self.failed_site_check && !other.failed_site_check,
        }
    }

    /// The settings that are on in both.
    #[must_use]
    pub const fn both(&self, other: &Self) -> Self {
        Self {
            invalid: self.invalid && other.invalid,
            low_country_confidence: self.low_country_confidence && other.low_country_confidence,
            out_of_rotation: self.out_of_rotation && other.out_of_rotation,
            failed_site_check: self.failed_site_check && other.failed_site_check,
        }
    }

    /// Whether the setting behind a reason of the provider's log of
    /// replaced proxies is switched on here: `auto_invalidated` is what
    /// `auto_replace_invalid_proxies` does, and so on. A reason that is not
    /// one of the provider's automatic ones (`proxy_replaced`,
    /// `list_updated`, `auto_deleted`) belongs to no setting.
    pub fn covers_reason(&self, reason: &str) -> bool {
        match reason {
            "auto_invalidated" => self.invalid,
            "auto_low_country_confidence" => self.low_country_confidence,
            "auto_out_of_rotation" => self.out_of_rotation,
            "auto_site_check" => self.failed_site_check,
            _ => false,
        }
    }
}

/// The provider account: plan, proxy count and replacement quota. Every
/// field the provider did not report is `None`.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct AccountInfo {
    /// The plan's id at the provider.
    pub plan_id: Option<String>,
    /// The plan's status, for example `active`.
    pub plan_status: Option<String>,
    /// The kind of proxies, for example `dedicated` / `isp`.
    pub proxy_type: Option<String>,
    /// How many proxies the plan includes.
    pub proxy_count: Option<u32>,
    /// Proxies per country in the list.
    pub countries: BTreeMap<String, u32>,
    /// Proxies per country the provider could hand out as replacements.
    pub available_countries: Option<BTreeMap<String, u32>>,
    /// Bandwidth limit in gigabytes; `0` means unlimited.
    pub bandwidth_limit_gb: Option<f64>,
    /// Replacements the plan includes.
    pub replacements_total: Option<u32>,
    /// Replacements already used.
    pub replacements_used: Option<u32>,
    /// Replacements left.
    pub replacements_available: Option<u32>,
    /// Start of the paid term. The provider counts replacements per term,
    /// so Kage's own budget follows the term when both ends are known.
    pub term_started_at: Option<DateTime<Utc>>,
    /// End of the paid term.
    pub term_ends_at: Option<DateTime<Utc>>,
    /// Whether the subscription renews by itself.
    pub renewals_enabled: Option<bool>,
    /// Whether the subscription is paused.
    pub paused: Option<bool>,
    /// Whether the provider is throttling the account.
    pub throttled: Option<bool>,
    /// The provider's automatic replacement settings.
    pub auto_replace: Option<AutoReplace>,
}

/// One hour of traffic through the proxies, as the provider counted it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ProxyActivity {
    /// Start of the hour.
    pub hour: DateTime<Utc>,
    /// Requests in the hour.
    pub requests_total: u64,
    /// Requests the provider counted as successful.
    pub requests_successful: u64,
    /// Requests the provider counted as failed.
    pub requests_failed: u64,
    /// Bytes transferred.
    pub bytes: u64,
    /// Distinct proxies used.
    pub proxies_used: u32,
    /// Failures by the provider's reason code.
    pub errors: BTreeMap<String, u64>,
}

/// An operation a provider may or may not offer. Listing is not one of
/// them: every provider lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    /// [`ProxyProvider::replace`] and [`ProxyProvider::replace_status`].
    Replacement,
    /// [`ProxyProvider::replaced`]: who replaced which proxy, and why.
    ReplacedLog,
    /// [`ProxyProvider::account`]: plan, quota and the provider's settings.
    Account,
    /// [`ProxyProvider::stats`]: the provider's own count of the traffic.
    Stats,
}

impl Operation {
    /// Every optional operation, in a fixed order.
    pub const ALL: [Self; 4] = [
        Self::Replacement,
        Self::ReplacedLog,
        Self::Account,
        Self::Stats,
    ];

    /// The stable name, as errors, logs and the CLI print it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Replacement => "replacement",
            Self::ReplacedLog => "replaced_log",
            Self::Account => "account",
            Self::Stats => "stats",
        }
    }
}

impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a provider can do besides listing its proxies, and the echo it
/// runs. A provider states this once, at build; it never changes while
/// the provider lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
// Four independent operations a provider offers or not, not a state.
#[allow(clippy::struct_excessive_bools)]
pub struct Capabilities {
    /// It replaces a proxy on request.
    pub replacement: bool,
    /// It keeps a log of the proxies it replaced, and why.
    pub replaced_log: bool,
    /// It reports its account: plan, quota, settings, term.
    pub account: bool,
    /// It counts the traffic through its proxies.
    pub stats: bool,
    /// The address echo the provider runs itself, which then sees nothing
    /// it does not see anyway: the default of `proxies.health.echo_url`.
    /// `None` for a provider without one; the public echo is used then.
    pub echo: Option<&'static str>,
}

impl Capabilities {
    /// A provider that lists and does nothing else, and runs no echo.
    pub const LIST_ONLY: Self = Self {
        replacement: false,
        replaced_log: false,
        account: false,
        stats: false,
        echo: None,
    };

    /// Whether the provider offers `operation`.
    pub const fn offers(&self, operation: Operation) -> bool {
        match operation {
            Operation::Replacement => self.replacement,
            Operation::ReplacedLog => self.replaced_log,
            Operation::Account => self.account,
            Operation::Stats => self.stats,
        }
    }

    /// The operations the provider does not offer, in [`Operation::ALL`]'s
    /// order.
    pub fn unsupported(&self) -> Vec<Operation> {
        Operation::ALL
            .into_iter()
            .filter(|operation| !self.offers(*operation))
            .collect()
    }
}

/// Where the proxy list comes from and who replaces a burned proxy.
///
/// Implementors write [`Self::kind`], [`Self::capabilities`] and
/// [`Self::list`], and each optional operation their capabilities name;
/// the rest keep the default bodies, which answer
/// [`ProviderError::Unsupported`]. No method may put a credential in an
/// error, a log line or a `Debug` output.
#[async_trait]
pub trait ProxyProvider: Send + Sync + std::fmt::Debug {
    /// Which provider this is; proxies it lists are stored under this kind.
    fn kind(&self) -> ProxyProviderKind;

    /// What the provider can do besides listing.
    fn capabilities(&self) -> Capabilities;

    /// Every proxy the provider currently lists.
    async fn list(&self) -> Result<Vec<ProviderProxy>>;

    /// Asks the provider to replace `proxy`. Optional
    /// ([`Operation::Replacement`]).
    async fn replace(&self, _proxy: &Proxy, _want: ReplaceSpec) -> Result<ReplaceTicket> {
        Err(ProviderError::unsupported(
            self.kind(),
            Operation::Replacement,
        ))
    }

    /// How far a replacement has got. Optional
    /// ([`Operation::Replacement`]).
    async fn replace_status(&self, _ticket: &ReplaceTicket) -> Result<ReplaceState> {
        Err(ProviderError::unsupported(
            self.kind(),
            Operation::Replacement,
        ))
    }

    /// The proxies the provider replaced at or after `since`, whoever asked
    /// for it. Optional ([`Operation::ReplacedLog`]).
    async fn replaced(&self, _since: DateTime<Utc>) -> Result<Vec<ReplacedProxy>> {
        Err(ProviderError::unsupported(
            self.kind(),
            Operation::ReplacedLog,
        ))
    }

    /// Plan, proxy count and replacement quota. Optional
    /// ([`Operation::Account`]).
    async fn account(&self) -> Result<AccountInfo> {
        Err(ProviderError::unsupported(self.kind(), Operation::Account))
    }

    /// Hourly traffic at or after `since`. Optional ([`Operation::Stats`]).
    async fn stats(&self, _since: DateTime<Utc>) -> Result<Vec<ProxyActivity>> {
        Err(ProviderError::unsupported(self.kind(), Operation::Stats))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_core::domain::ProxyProviderKind;

    use kage_core::config::ResolvedProvider;

    use super::{
        AutoReplace, Capabilities, Operation, PUBLIC_ECHO_URL, ProviderError,
        WEBSHARE_CAPABILITIES, capabilities_of, default_echo,
    };

    /// A provider with an echo of its own keeps its probes on it; one
    /// without has them read the public echo (decision D51).
    #[test]
    fn the_default_echo_is_the_providers_own_else_the_public_one() {
        assert_eq!(
            default_echo(ResolvedProvider::Webshare),
            "https://ipv4.webshare.io/"
        );
        assert_eq!(default_echo(ResolvedProvider::Static), PUBLIC_ECHO_URL);
        assert_eq!(default_echo(ResolvedProvider::None), PUBLIC_ECHO_URL);
        assert_eq!(PUBLIC_ECHO_URL, "https://checkip.amazonaws.com/");
        assert_eq!(
            capabilities_of(ResolvedProvider::Webshare),
            Some(WEBSHARE_CAPABILITIES)
        );
        assert_eq!(WEBSHARE_CAPABILITIES.unsupported(), []);
        assert_eq!(
            capabilities_of(ResolvedProvider::Static),
            Some(Capabilities::LIST_ONLY)
        );
        assert_eq!(capabilities_of(ResolvedProvider::None), None);
    }

    #[test]
    fn capabilities_name_what_a_provider_lacks() {
        assert_eq!(Capabilities::LIST_ONLY.unsupported(), Operation::ALL);
        for operation in Operation::ALL {
            assert!(!Capabilities::LIST_ONLY.offers(operation), "{operation}");
        }
        let replaces_only = Capabilities {
            replacement: true,
            ..Capabilities::LIST_ONLY
        };
        assert!(replaces_only.offers(Operation::Replacement));
        assert_eq!(
            replaces_only.unsupported(),
            [Operation::ReplacedLog, Operation::Account, Operation::Stats]
        );
        for operation in [Operation::ReplacedLog, Operation::Account, Operation::Stats] {
            let mut capabilities = Capabilities::LIST_ONLY;
            match operation {
                Operation::ReplacedLog => capabilities.replaced_log = true,
                Operation::Account => capabilities.account = true,
                Operation::Stats => capabilities.stats = true,
                Operation::Replacement => capabilities.replacement = true,
            }
            assert!(capabilities.offers(operation), "{operation}");
            assert!(!capabilities.unsupported().contains(&operation));
            assert_eq!(capabilities.unsupported().len(), 3);
        }
        assert_eq!(
            Operation::ALL.map(Operation::as_str),
            ["replacement", "replaced_log", "account", "stats"]
        );
        assert_eq!(Operation::ReplacedLog.to_string(), "replaced_log");
        assert_eq!(
            serde_json::to_value(Capabilities::LIST_ONLY).unwrap(),
            serde_json::json!({
                "replacement": false, "replaced_log": false, "account": false,
                "stats": false, "echo": null,
            })
        );
        assert_eq!(
            ProviderError::unsupported(ProxyProviderKind::Static, Operation::Stats),
            ProviderError::Unsupported {
                provider: ProxyProviderKind::Static,
                operation: "stats",
            }
        );
    }

    #[test]
    fn transient_errors_are_the_ones_worth_retrying_later() {
        let status = |status| ProviderError::Status {
            status,
            code: None,
            message: String::new(),
        };
        for error in [
            ProviderError::Transport("connection reset".to_owned()),
            ProviderError::RateLimited {
                retry_after: Duration::from_mins(1),
            },
            ProviderError::Busy,
            status(500),
            status(503),
        ] {
            assert!(error.is_transient(), "{error}");
        }
        for error in [
            ProviderError::Setup("no key".to_owned()),
            ProviderError::Unsupported {
                provider: ProxyProviderKind::Static,
                operation: "replacement",
            },
            ProviderError::Unauthorized { status: 401 },
            status(400),
            status(404),
            ProviderError::Decode("missing field".to_owned()),
            ProviderError::QuotaExhausted,
            ProviderError::OutcomeUnknown("timeout".to_owned()),
        ] {
            assert!(!error.is_transient(), "{error}");
        }
    }

    #[test]
    fn errors_read_as_sentences() {
        assert_eq!(
            ProviderError::Unsupported {
                provider: ProxyProviderKind::Static,
                operation: "replacement",
            }
            .to_string(),
            "the static provider does not support replacement"
        );
        assert_eq!(
            ProviderError::Status {
                status: 400,
                code: Some("invalid".to_owned()),
                message: "Your proxy list is not editable".to_owned(),
            }
            .to_string(),
            "the provider API answered HTTP 400: Your proxy list is not editable"
        );
    }

    #[test]
    fn auto_replace_names_what_is_switched_on() {
        assert_eq!(AutoReplace::default().enabled(), Vec::<&str>::new());
        assert_eq!(
            AutoReplace {
                invalid: true,
                low_country_confidence: false,
                out_of_rotation: true,
                failed_site_check: false,
            }
            .enabled(),
            [
                "auto_replace_invalid_proxies",
                "auto_replace_out_of_rotation_proxies"
            ]
        );
        let all = AutoReplace {
            invalid: true,
            low_country_confidence: true,
            out_of_rotation: true,
            failed_site_check: true,
        };
        assert_eq!(all.enabled(), AutoReplace::SETTINGS);
    }

    #[test]
    fn accepted_settings_are_read_by_name_and_taken_out_of_what_is_on() {
        let accepted = AutoReplace::from_names(&["auto_replace_invalid_proxies"]).unwrap();
        assert_eq!(accepted.enabled(), ["auto_replace_invalid_proxies"]);
        assert_eq!(
            AutoReplace::from_names(&AutoReplace::SETTINGS)
                .unwrap()
                .enabled(),
            AutoReplace::SETTINGS
        );
        assert_eq!(
            AutoReplace::from_names(&Vec::<String>::new()).unwrap(),
            AutoReplace::default()
        );
        assert_eq!(
            AutoReplace::from_names(&["auto_replace_everything"]),
            Err("auto_replace_everything".to_owned())
        );

        let on = AutoReplace {
            invalid: true,
            low_country_confidence: true,
            out_of_rotation: false,
            failed_site_check: false,
        };
        assert_eq!(
            on.without(&accepted).enabled(),
            ["auto_replace_low_country_confidence_proxies"]
        );
        assert_eq!(
            on.both(&accepted).enabled(),
            ["auto_replace_invalid_proxies"]
        );
        assert_eq!(
            AutoReplace::default().without(&accepted),
            AutoReplace::default()
        );
    }

    #[test]
    fn a_reason_of_the_replaced_log_belongs_to_one_setting() {
        let all = AutoReplace::from_names(&AutoReplace::SETTINGS).unwrap();
        for reason in [
            "auto_invalidated",
            "auto_low_country_confidence",
            "auto_out_of_rotation",
            "auto_site_check",
        ] {
            assert!(all.covers_reason(reason), "{reason}");
            assert!(!AutoReplace::default().covers_reason(reason), "{reason}");
        }
        // What Kage asked for, and what no setting does, is never covered.
        for reason in ["proxy_replaced", "list_updated", "auto_deleted", ""] {
            assert!(!all.covers_reason(reason), "{reason}");
        }
        let invalid_only = AutoReplace::from_names(&["auto_replace_invalid_proxies"]).unwrap();
        assert!(invalid_only.covers_reason("auto_invalidated"));
        assert!(!invalid_only.covers_reason("auto_site_check"));
    }
}
