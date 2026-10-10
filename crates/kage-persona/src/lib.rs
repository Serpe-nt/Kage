//! The persona manager (spec §6): one long-lived synthetic household user
//! per proxy, from its generation to the deletion of its archived profile.
//!
//! - [`manager`]: the [`PersonaManager`]. It generates a persona when a
//!   proxy is first usable, follows the pool's events for the rest of its
//!   life, and retires it with its proxy.
//! - [`lifecycle`]: what those events do to a persona, as pure functions.
//! - [`identity`]: the persona on the http path: its wreq profile, the
//!   profile-major rule, and the probe client that presents as the persona
//!   behind a proxy.
//! - [`sidecar`]: the two sidecars and the conversions to their contract.
//! - [`sessions`]: handles that pin a caller's requests to one persona.
//! - [`domain_state`]: cooldowns, daily caps and block counters of one
//!   persona on one domain.
//! - [`handoff`]: cookies between a persona's browser and its http jar
//!   (spec §6.4).
//! - [`warmup`]: the first visits of a new persona.
//! - [`selftest`]: the self-tests of one persona, on demand and scheduled.
//! - [`archive`]: the profile directory of a retired persona.
//!
//! Nothing here chooses a persona for a request: that is the scheduler's
//! (M7). What a persona is drawn from (the mix, the locale and screen
//! tables, the warm-up plan) is pure logic in `kage_core::persona`.

#![forbid(unsafe_code)]

pub mod archive;
pub mod domain_state;
mod error;
mod events;
pub mod handoff;
pub mod identity;
pub mod lifecycle;
pub mod manager;
pub mod selftest;
pub mod sessions;
pub mod sidecar;
pub mod warmup;

#[cfg(test)]
mod test_support;

pub use domain_state::{DomainStates, Eligibility};
pub use error::PersonaError;
pub use events::PersonaEvent;
pub use identity::{Directory, HttpPathOff, PersonaProbeClients};
pub use manager::{PersonaManager, PersonaParts, ReconcileReport, SweepReport};
pub use sessions::{Pinned, SessionError, Sessions};
pub use sidecar::Sidecars;

/// The crate's name, for the binary's module listing.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// What the persona manager will do with this configuration, for `kage
/// config check`: the shares of the mix, whether new personas warm up, and
/// which self-tests the server runs by itself. A scheduled self-test only
/// ever uses the operator's own echo and pages, so without them none is
/// scheduled.
pub fn describe(config: &kage_core::config::Config) -> serde_json::Value {
    let personas = &config.personas;
    let weights: u32 = personas.mix.iter().map(|entry| entry.weight).sum();
    let mix: Vec<serde_json::Value> = personas
        .mix
        .iter()
        .map(|entry| {
            serde_json::json!({
                "engine": entry.engine.as_str(),
                "os": entry.os.as_str(),
                "share": f64::from(entry.weight) / f64::from(weights.max(1)),
            })
        })
        .collect();
    serde_json::json!({
        "mix": mix,
        "tables_version": kage_core::persona::TABLES_VERSION,
        "countries_with_a_locale": kage_core::persona::locale::known_countries(),
        "warmup": {
            "enabled": personas.warmup.enabled,
            "sites": personas.warmup.sites.len(),
        },
        "scheduled_selftests": {
            "http": config.selftest.http.echo_url.is_some(),
            "browser": !config.selftest.browser.pages.is_empty(),
        },
        "profile_retention_days": personas.retention_days,
    })
}

#[cfg(test)]
mod tests {
    use kage_core::config::{Config, SelftestPageConfig, SelftestPageKind};

    use super::describe;

    #[test]
    fn the_description_says_what_the_server_will_do_by_itself() {
        let mut config = Config::load(
            None,
            [
                ("KAGE__DATABASE__URL", "postgres://kage@localhost/kage"),
                ("KAGE__SIDECAR__TOKEN", "token"),
                (
                    "KAGE__SECRETS__MASTER_KEY",
                    "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
                ),
            ]
            .map(|(key, value)| (key.to_owned(), value.to_owned())),
        )
        .unwrap();

        let shipped = describe(&config);
        let shares: Vec<(String, f64)> = shipped["mix"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    format!(
                        "{}/{}",
                        entry["engine"].as_str().unwrap(),
                        entry["os"].as_str().unwrap()
                    ),
                    entry["share"].as_f64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            shares,
            [
                ("chromium/linux".to_owned(), 0.40),
                ("firefox/windows".to_owned(), 0.45),
                ("firefox/macos".to_owned(), 0.15),
            ]
        );
        assert_eq!(shipped["tables_version"], "v1");
        assert_eq!(shipped["warmup"]["enabled"], true);
        assert!(shipped["warmup"]["sites"].as_u64().unwrap() > 0);
        assert_eq!(shipped["profile_retention_days"], 7);
        assert!(
            shipped["countries_with_a_locale"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("AT"))
        );
        // Nothing of the operator's own is configured: the server tests
        // nothing by itself, and never asks a third party.
        assert_eq!(
            shipped["scheduled_selftests"],
            serde_json::json!({ "http": false, "browser": false })
        );

        config.selftest.http.echo_url = Some("https://echo.kage.test/".to_owned());
        config.selftest.browser.pages = vec![SelftestPageConfig {
            id: "probe".to_owned(),
            kind: SelftestPageKind::Probe,
            url: "https://fixture.kage.test/probe/".to_owned(),
        }];
        assert_eq!(
            describe(&config)["scheduled_selftests"],
            serde_json::json!({ "http": true, "browser": true })
        );
    }
}
