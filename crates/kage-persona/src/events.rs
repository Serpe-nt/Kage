//! What happens to personas, for subscribers in other crates.

use kage_core::domain::PersonaState;
use kage_core::ids::{PersonaId, ProxyId};

use crate::lifecycle::GeoChange;

/// Something that happened to a persona. The stored health events are the
/// complete record; a subscriber that falls behind loses the oldest ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersonaEvent {
    /// A persona was generated for a proxy.
    Generated {
        /// The new persona.
        persona: PersonaId,
        /// Its proxy.
        proxy: ProxyId,
    },
    /// A persona changed state.
    StateChanged {
        /// The persona.
        persona: PersonaId,
        /// The state before.
        from: PersonaState,
        /// The state after.
        to: PersonaState,
    },
    /// The geo record of a persona's address changed and the persona
    /// followed it.
    GeoUpdated {
        /// The persona.
        persona: PersonaId,
        /// What changed.
        change: GeoChange,
    },
    /// A persona's warm-up is over: the scheduler may give it real work.
    Warmed {
        /// The persona.
        persona: PersonaId,
    },
}
