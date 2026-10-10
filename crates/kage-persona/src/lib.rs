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
