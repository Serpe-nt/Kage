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
    /// The task that ran the fetch panicked: a fault of Kage's own, not a
    /// stop (QA round three api-08).
    #[error("the fetch's task failed: {0}")]
    Panicked(String),
}

impl SchedulerError {
    /// What a fetch's task that did not return is: a panic is
    /// [`SchedulerError::Panicked`], a task cancelled by the runtime as it
    /// stops is [`SchedulerError::ShuttingDown`].
    pub(crate) fn of_join(error: &tokio::task::JoinError) -> Self {
        if error.is_panic() {
            Self::Panicked(error.to_string())
        } else {
            Self::ShuttingDown
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SchedulerError;

    #[tokio::test]
    async fn a_panicked_task_is_no_stop_and_a_cancelled_one_is() {
        // api-08: a panic was answered "the server is stopping".
        let panicked = tokio::spawn(async { panic!("DateTime + TimeDelta overflowed") })
            .await
            .unwrap_err();
        assert!(matches!(
            SchedulerError::of_join(&panicked),
            SchedulerError::Panicked(_)
        ));
        let task = tokio::spawn(std::future::pending::<()>());
        task.abort();
        let cancelled = task.await.unwrap_err();
        assert!(matches!(
            SchedulerError::of_join(&cancelled),
            SchedulerError::ShuttingDown
        ));
    }
}
