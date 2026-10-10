//! Why a persona operation failed.

use kage_browser::BrowserError;
use kage_core::domain::{ClaimedOs, Engine, ProxyState};
use kage_core::ids::{PersonaId, ProxyId};
use kage_http::HttpError;
use kage_http::selftest::SelftestError;
use kage_proxy::{CryptoError, PoolError};
use kage_store::StoreError;

use crate::identity::HttpPathOff;

/// Why a persona operation failed.
#[derive(Debug, thiserror::Error)]
pub enum PersonaError {
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The proxy pool failed.
    #[error(transparent)]
    Pool(#[from] PoolError),
    /// A proxy credential could not be opened.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    /// A sidecar could not be reached, or refused.
    #[error(transparent)]
    Browser(#[from] BrowserError),
    /// The http path failed.
    #[error(transparent)]
    Http(#[from] HttpError),
    /// There is no such persona.
    #[error("there is no persona {0}")]
    NotFound(PersonaId),
    /// The persona is retired.
    #[error("persona {0} is retired")]
    Retired(PersonaId),
    /// There is no such proxy.
    #[error("there is no proxy {0}")]
    ProxyNotFound(ProxyId),
    /// The proxy cannot carry traffic now, so nothing can be generated or
    /// tested behind it.
    #[error("proxy {proxy} is {state}; a persona needs a healthy or degraded proxy")]
    ProxyNotUsable {
        /// The proxy.
        proxy: ProxyId,
        /// The state it is in.
        state: ProxyState,
    },
    /// A proxy leaked the host address: nothing leaves through the pool
    /// until that is cleared (spec §7.3).
    #[error(
        "egress is unsafe: a proxy leaked the host address; nothing is sent until it is cleared"
    )]
    EgressUnsafe,
    /// The configured mix yields no engine and system.
    #[error("personas.mix yields no engine: {0}")]
    Mix(String),
    /// A chromium persona may only claim the system its Chrome runs on.
    #[error(
        "a {engine} persona may not claim {os}: engines.chromium.allow_cross_os_claims is false"
    )]
    CrossOsClaim {
        /// The engine.
        engine: Engine,
        /// The system that was to be claimed.
        os: ClaimedOs,
    },
    /// The sidecar generated something else than it was asked for.
    #[error("the {engine} sidecar generated another persona than was asked for: {reason}")]
    Generation {
        /// The engine.
        engine: Engine,
        /// What differs.
        reason: String,
    },
    /// The persona's stored fingerprint is not a document a sidecar can
    /// take.
    #[error("the stored fingerprint of persona {0} is not a JSON object")]
    Fingerprint(PersonaId),
    /// The persona cannot use the http path.
    #[error("persona {persona} cannot use the http path: {reason}")]
    HttpPathOff {
        /// The persona.
        persona: PersonaId,
        /// Why.
        reason: HttpPathOff,
    },
    /// The http self-test could not be judged.
    #[error(transparent)]
    Selftest(#[from] SelftestError),
    /// There is nothing to run a self-test against.
    #[error("{0}")]
    NothingToTest(String),
    /// An origin that is not `http(s)://host[:port]`.
    #[error("{0:?} is not an origin: http or https, a host, and nothing after it")]
    Origin(String),
    /// A profile directory could not be moved or removed.
    #[error("profile directory {path}: {source}")]
    Profile {
        /// The directory.
        path: String,
        /// The I/O error.
        #[source]
        source: std::io::Error,
    },
}

impl PersonaError {
    /// Whether trying again later can help without anyone changing
    /// anything: a sidecar that could not be reached or was full, a store
    /// or pool hiccup.
    pub const fn is_transient(&self) -> bool {
        match self {
            Self::Browser(error) => {
                error.is_unreachable()
                    || error.is_at_capacity()
                    || matches!(
                        error,
                        BrowserError::Timeout { .. } | BrowserError::Transport { .. }
                    )
            }
            Self::Store(_) | Self::Pool(_) | Self::ProxyNotUsable { .. } | Self::EgressUnsafe => {
                true
            }
            _ => false,
        }
    }
}
