//! The proxy provider: where the list of proxies comes from and who
//! replaces a burned one (spec §7.1).
//!
//! Two implementations: [`WebshareProvider`] talks to the Webshare API,
//! [`StaticListProvider`] serves a fixed list for tests and for vendors
//! without an API.

mod static_list;
mod webshare;

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kage_core::domain::{CountryCode, Proxy, ProxyProviderKind};
use kage_core::secret::Secret;

pub use static_list::StaticListProvider;
pub use webshare::WebshareProvider;

/// Result type of every provider call.
pub type Result<T> = std::result::Result<T, ProviderError>;

/// Why a provider call failed. No variant carries credentials.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    /// The provider client could not be built from its configuration.
    #[error("the provider client cannot be set up: {0}")]
    Setup(String),
    /// The provider cannot do this at all (a static list cannot replace).
    #[error("the {provider} provider does not support {operation}")]
    Unsupported {
        /// The provider.
        provider: ProxyProviderKind,
        /// What was asked of it.
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
    /// Country of the exit address, as the provider states it.
    pub country: Option<CountryCode>,
    /// City of the exit address, as the provider states it.
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
    /// The names of the settings that are switched on.
    pub fn enabled(&self) -> Vec<&'static str> {
        [
            (self.invalid, "auto_replace_invalid_proxies"),
            (
                self.low_country_confidence,
                "auto_replace_low_country_confidence_proxies",
            ),
            (self.out_of_rotation, "auto_replace_out_of_rotation_proxies"),
            (
                self.failed_site_check,
                "auto_replace_failed_site_check_proxies",
            ),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect()
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

/// Where the proxy list comes from and who replaces a burned proxy.
#[async_trait]
pub trait ProxyProvider: Send + Sync + std::fmt::Debug {
    /// Which provider this is; proxies it lists are stored under this kind.
    fn kind(&self) -> ProxyProviderKind;

    /// Every proxy the provider currently lists.
    async fn list(&self) -> Result<Vec<ProviderProxy>>;

    /// Asks the provider to replace `proxy`.
    async fn replace(&self, proxy: &Proxy, want: ReplaceSpec) -> Result<ReplaceTicket>;

    /// How far a replacement has got.
    async fn replace_status(&self, ticket: &ReplaceTicket) -> Result<ReplaceState>;

    /// The proxies the provider replaced at or after `since`, whoever asked
    /// for it.
    async fn replaced(&self, since: DateTime<Utc>) -> Result<Vec<ReplacedProxy>>;

    /// Plan, proxy count and replacement quota.
    async fn account(&self) -> Result<AccountInfo>;

    /// Hourly traffic at or after `since`.
    async fn stats(&self, since: DateTime<Utc>) -> Result<Vec<ProxyActivity>>;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_core::domain::ProxyProviderKind;

    use super::{AutoReplace, ProviderError};

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
        assert_eq!(
            AutoReplace {
                invalid: true,
                low_country_confidence: true,
                out_of_rotation: true,
                failed_site_check: true,
            }
            .enabled()
            .len(),
            4
        );
    }
}
