//! Domain types, outcomes, policy model, pacing math and the block classifier.
//!
//! This crate is the shared vocabulary of Kage (spec §5) and the pure logic
//! that every other crate builds on:
//!
//! - [`domain`]: proxies, personas, sessions, attempts, results, jobs, keys
//!   and events, with typed ids ([`ids`]).
//! - [`outcome`]: the classified result of an attempt and the block report.
//! - [`policy`]: per-domain policies, their resolution order, the three
//!   policy files and the hot-reloading [`policy::PolicyStore`].
//! - [`pacing`]: pacing profiles and their seeded samplers.
//! - [`classifier`]: block signatures as data and the rule engine.
//! - [`ssrf`]: the SSRF guard as pure functions over resolved addresses.
//! - [`urlnorm`]: URL normalization for dedupe and cache keys (spec §13).
//! - [`config`]: `kage.yaml` with `KAGE__SECTION__KEY` layering and secrets.
//!
//! Nothing here performs network I/O. The only file access is reading the
//! configuration and policy files.

#![forbid(unsafe_code)]

mod macros;

pub mod alert;
pub mod classifier;
pub mod config;
pub mod domain;
pub mod duration;
pub mod ids;
pub mod outcome;
pub mod pacing;
pub mod policy;
pub mod secret;
pub mod seed;
pub mod ssrf;
pub mod urlnorm;

mod yaml;

pub use macros::UnknownVariant;

/// Name of this crate as recorded in its manifest.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn smoke() {
        assert_eq!(CRATE_NAME, "kage-core");
    }
}
