//! Identical fetches under way at once (spec §15, F52). Of two fetches
//! with one cache key, the second waits for the first and is answered
//! from the cache with what the first one stored: a page asked for twice
//! at the same moment is one visit. Only inside one process: two
//! processes each fetch.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::watch;

use super::{Course, FetchReport, Pipeline, Stop, cache, stopped};
use crate::SchedulerError;
use crate::metrics;

/// The fetches under way that put their page into the cache, by key.
#[derive(Debug, Default)]
pub(super) struct Flights {
    under_way: Mutex<HashMap<String, watch::Receiver<()>>>,
}

/// A fetch's hold on its key, from the moment it missed the cache until
/// its course ends: after its result was put into the cache, or when
/// nothing came of it. Whoever waits for it is woken then.
#[derive(Debug)]
pub(super) struct Flight {
    flights: Arc<Flights>,
    key: String,
    // Nothing is ever sent: the flight has landed when this is dropped.
    _landed: watch::Sender<()>,
}

/// What a fetch that missed the cache finds of an identical one.
enum Boarding {
    /// Nobody fetches the page: this one does, holding the key.
    Lead(Flight),
    /// Another one is under way: whoever holds this waits for it to land.
    Wait(watch::Receiver<()>),
}

/// How the wait for an identical fetch ended.
enum Landing {
    Landed,
    Stopped,
    GaveUp,
}

/// What stands before a fetch's visit, as far as the cache is concerned.
pub(super) enum Boarded {
    /// The cache answered it.
    Served(Box<FetchReport>),
    /// It was stopped while it waited for an identical one.
    Stopped,
    /// It is to be fetched.
    Go,
}

impl Flights {
    fn locked(&self) -> MutexGuard<'_, HashMap<String, watch::Receiver<()>>> {
        self.under_way
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn board(self: &Arc<Self>, key: &str) -> Boarding {
        let mut under_way = self.locked();
        if let Some(landing) = under_way.get(key) {
            return Boarding::Wait(landing.clone());
        }
        let (landed, landing) = watch::channel(());
        under_way.insert(key.to_owned(), landing);
        Boarding::Lead(Flight {
            flights: Arc::clone(self),
            key: key.to_owned(),
            _landed: landed,
        })
    }

    /// How many keys are being fetched now.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.locked().len()
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        // The key is this flight's: nobody boards a key that is held.
        self.flights.locked().remove(&self.key);
    }
}

/// Resolves when a flight has landed.
async fn landed(mut landing: watch::Receiver<()>) {
    while landing.changed().await.is_ok() {}
}

impl Pipeline {
    /// Answers a request from the cache, or sends it on to be fetched
    /// holding its key. A request whose identical twin is under way waits
    /// for it to land and is then answered with what it stored; when the
    /// twin stored nothing (a block, a failure, thin content), the request
    /// is fetched itself. One that waited past its patience, or may not be
    /// answered from the cache (a crawl's page), is fetched without
    /// waiting; the second holds the key when nobody does.
    pub(super) async fn board(
        &self,
        course: &mut Course,
        stop: Option<&Stop>,
    ) -> Result<Boarded, SchedulerError> {
        loop {
            let Some(plan) = cache::plan_of(&course.order) else {
                return Ok(Boarded::Go);
            };
            if !plan.read {
                if let Boarding::Lead(flight) = self.flights.board(&plan.key) {
                    course.flight = Some(flight);
                }
                return Ok(Boarded::Go);
            }
            if let Some(report) = self.serve_cached(course).await? {
                return Ok(Boarded::Served(Box::new(report)));
            }
            let landing = match self.flights.board(&plan.key) {
                Boarding::Lead(flight) => {
                    metrics::cache("miss");
                    course.flight = Some(flight);
                    return Ok(Boarded::Go);
                }
                Boarding::Wait(landing) => landing,
            };
            metrics::cache("joined");
            tracing::info!(
                request = %course.request.id,
                domain = course.domain,
                "waits for an identical fetch under way"
            );
            match self.wait_for_landing(landing, course, stop).await {
                Landing::Landed => {}
                Landing::Stopped => return Ok(Boarded::Stopped),
                Landing::GaveUp => {
                    metrics::cache("miss");
                    return Ok(Boarded::Go);
                }
            }
        }
    }

    /// Waits for an identical fetch to land, for at most as long as the
    /// request waits for anything, and until its job is stopped.
    async fn wait_for_landing(
        &self,
        landing: watch::Receiver<()>,
        course: &Course,
        stop: Option<&Stop>,
    ) -> Landing {
        let patience = course
            .give_up_at
            .map(|at| (at - self.now()).to_std().unwrap_or(Duration::ZERO));
        let mut stop = stop.cloned();
        tokio::select! {
            () = landed(landing) => Landing::Landed,
            () = async {
                match stop.as_mut() {
                    Some(stop) => stopped(stop).await,
                    None => std::future::pending().await,
                }
            } => Landing::Stopped,
            () = async {
                match patience {
                    Some(patience) => tokio::time::sleep(patience).await,
                    None => std::future::pending().await,
                }
            } => Landing::GaveUp,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Boarding, Flights, landed};

    #[tokio::test]
    async fn one_fetch_holds_a_key_and_whoever_waits_is_woken_when_it_lets_go() {
        let flights = Arc::new(Flights::default());
        let Boarding::Lead(first) = flights.board("k") else {
            panic!("nobody held the key");
        };
        let Boarding::Wait(waiting) = flights.board("k") else {
            panic!("the key is held");
        };
        // Another key is another flight.
        assert!(matches!(flights.board("other"), Boarding::Lead(_)));
        assert_eq!(flights.len(), 1);
        let woken = tokio::spawn(landed(waiting));
        tokio::task::yield_now().await;
        assert!(!woken.is_finished());
        drop(first);
        woken.await.unwrap();
        // Let go, the key is free for the next one.
        assert_eq!(flights.len(), 0);
        assert!(matches!(flights.board("k"), Boarding::Lead(_)));
    }
}
