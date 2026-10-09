//! Proxy provider, pool state machine, health checks and replacement (spec
//! §7), and the encryption of proxy credentials at rest (spec §19).
//!
//! - [`provider`]: the [`ProxyProvider`] trait with the Webshare API client
//!   and a static list.
//! - [`pool`]: the [`Pool`], which keeps the stored proxies in step with the
//!   provider, applies health results and replaces burned addresses within
//!   the monthly budget.
//! - [`machine`]: the pool state machine as pure functions.
//! - [`health`]: the [`HealthChecker`] with its probes, the host address
//!   lookup and the reputation provider.
//! - [`crypto`]: sealing and opening of proxy user names and passwords.
//! - [`rate`]: the token bucket the Webshare client limits itself with.
//! - [`clock`]: the pool's replaceable source of time.
//!
//! Outbound HTTP goes through wreq only. The provider API, the host address
//! lookup and the reputation lookup are reached directly; probes go through
//! the proxy they check. Nothing in this crate ever connects to a target
//! site.

#![forbid(unsafe_code)]

pub mod clock;
pub mod crypto;
pub mod health;
pub mod machine;
pub mod pool;
pub mod provider;
pub mod rate;

#[cfg(test)]
mod test_support;

use kage_core::config::Config;

pub use clock::{Clock, ManualClock, SystemClock, TokioClock};
pub use crypto::{CredentialCipher, CredentialField, CryptoError};
pub use health::{CheckResult, HealthChecker, HealthSettings, HealthSetupError};
pub use machine::PoolRules;
pub use pool::{
    Pool, PoolError, PoolEvent, PoolParts, PoolSummary, ProxyEndpoint, ReconcileReport,
    ReplacementOutcome, TickReport,
};
pub use provider::{
    AccountInfo, AutoReplace, ProviderError, ProviderProxy, ProxyActivity, ProxyProvider,
    ReplaceSpec, ReplaceState, ReplaceTicket, ReplacedProxy, StaticListProvider, WebshareProvider,
};

/// Name of this crate as recorded in its manifest.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// Checks the parts of the configuration only this crate can judge: that
/// the master key builds a cipher, that the static list parses, and that
/// the probe's emulation profile is one this build of wreq-util ships.
/// `kage config check` calls it, so a wrong profile name is found before
/// the server starts.
///
/// # Errors
///
/// The first problem, as a message that names the configuration key.
pub fn check_config(config: &Config) -> Result<(), String> {
    CredentialCipher::from_config(&config.secrets)
        .map_err(|error| format!("secrets.master_key: {error}"))?;
    StaticListProvider::from_config(&config.proxies).map_err(|error| error.to_string())?;
    health::EmulatedClientFactory::from_config(&config.proxies.health)
        .map_err(|error| format!("proxies.health.profile: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use kage_core::config::Config;

    use super::{CRATE_NAME, check_config};

    fn config(extra: &[(&str, &str)]) -> Config {
        let mut pairs = vec![
            ("KAGE__DATABASE__URL", "postgres://kage@localhost/kage"),
            ("KAGE__SIDECAR__TOKEN", "token"),
            (
                "KAGE__SECRETS__MASTER_KEY",
                "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
            ),
        ];
        pairs.extend_from_slice(extra);
        Config::load(
            None,
            pairs
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value.to_owned())),
        )
        .unwrap()
    }

    #[test]
    fn smoke() {
        assert_eq!(CRATE_NAME, "kage-proxy");
    }

    #[test]
    fn the_default_configuration_passes() {
        assert_eq!(check_config(&config(&[])), Ok(()));
        assert_eq!(
            check_config(&config(&[
                ("KAGE__PROXIES__HEALTH__PROFILE", "firefox_151"),
                ("KAGE__PROXIES__HEALTH__PLATFORM", "macos"),
                ("KAGE__PROXIES__STATIC_LIST", "198.51.100.10:8080:user:pass"),
            ])),
            Ok(())
        );
    }

    #[test]
    fn an_unknown_emulation_profile_is_named_with_its_key() {
        let error =
            check_config(&config(&[("KAGE__PROXIES__HEALTH__PROFILE", "chrome_1")])).unwrap_err();

        assert!(error.starts_with("proxies.health.profile:"), "{error}");
        assert!(error.contains("chrome_1"), "{error}");
        assert!(error.contains("chrome_149"), "{error}");
    }
}
