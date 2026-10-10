//! Why the scheduler could not do something.

use kage_core::pacing::PacingError;
use kage_persona::PersonaError;
use kage_proxy::PoolError;
use kage_store::StoreError;

/// Why the scheduler could not do something. None of these is an outcome
/// of a fetch: a fetch that fails still has an outcome.
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The proxy pool failed.
    #[error(transparent)]
    Pool(#[from] PoolError),
    /// The persona manager failed.
    #[error(transparent)]
    Persona(#[from] PersonaError),
    /// The request names pacing that does not exist or is not valid.
    #[error(transparent)]
    Pacing(#[from] PacingError),
    /// The URL has no host a policy could be looked up for.
    #[error("{0:?} has no host")]
    NoHost(String),
    /// The scheduler is shutting down.
    #[error("the scheduler is shutting down")]
    ShuttingDown,
}
