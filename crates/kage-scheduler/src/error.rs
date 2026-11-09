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
    /// Whether the scheduler failed because the database could not be
    /// reached: the store, or the store under the persona manager or the
    /// proxy pool, lost its connection or had none to give (a cut, a fenced
    /// store; [`StoreError::is_connection`]). Such a failure says nothing
    /// about the request, and asking again later may succeed (QA round four
    /// lock-01, review of #98).
    pub fn is_store_unreachable(&self) -> bool {
        match self {
            Self::Store(store)
            | Self::Pool(PoolError::Store(store))
            | Self::Persona(
                PersonaError::Store(store) | PersonaError::Pool(PoolError::Store(store)),
            ) => store.is_connection(),
            _ => false,
        }
    }

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

    /// Review of #98: a database out of reach, under whatever part of the
    /// scheduler met it, and nothing else, is a store that cannot be
    /// reached.
    #[test]
    fn only_a_database_out_of_reach_is_a_store_that_cannot_be_reached() {
        use kage_persona::PersonaError;
        use kage_proxy::PoolError;
        use kage_store::StoreError;
        let cut = || StoreError::Unreachable(std::time::Duration::from_secs(3));
        for unreachable in [
            SchedulerError::Store(cut()),
            SchedulerError::Pool(PoolError::Store(cut())),
            SchedulerError::Persona(PersonaError::Store(cut())),
            SchedulerError::Persona(PersonaError::Pool(PoolError::Store(cut()))),
        ] {
            assert!(unreachable.is_store_unreachable(), "{unreachable}");
        }
        for other in [
            SchedulerError::Store(StoreError::Unnamed),
            SchedulerError::ShuttingDown,
            SchedulerError::NoHost("x".to_owned()),
        ] {
            assert!(!other.is_store_unreachable(), "{other}");
        }
    }
}
