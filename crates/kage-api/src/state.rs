//! What the API serves from: the services `kage serve` composed, and what
//! the API keeps of its own between requests.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use kage_core::config::Config;
use kage_core::policy::PolicyStore;
use kage_persona::PersonaManager;
use kage_proxy::{Clock, CredentialCipher, HealthChecker, Pool};
use kage_scheduler::{Bandwidth, Pipeline};
use kage_store::{ArtifactStore, Store};
use tokio::sync::Mutex;

use crate::handle::Handles;
use crate::jobs::worker::JobsHub;
use crate::limits::Limits;
use crate::metrics::MetricsHandle;
use crate::ops::Readiness;
use crate::resolve::HostResolver;

/// The fetch engine as the API uses it. `kage serve` starts it when a
/// proxy pool exists; without one nothing can be fetched.
#[derive(Clone)]
pub struct EngineParts {
    /// One fetch from the request to its stored result.
    pub pipeline: Pipeline,
    /// The personas: sessions, self-tests, retiring.
    pub personas: Arc<PersonaManager>,
    /// The proxy pool.
    pub pool: Arc<Pool>,
    /// The health checks of the pool.
    pub health: Arc<HealthChecker>,
    /// The bandwidth budget of the proxy plan.
    pub bandwidth: Arc<Bandwidth>,
}

impl fmt::Debug for EngineParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineParts").finish_non_exhaustive()
    }
}

/// Everything the API is built from.
#[derive(Clone)]
pub struct ApiParts {
    /// The configuration in force.
    pub config: Arc<Config>,
    /// The store.
    pub store: Store,
    /// The artifact store.
    pub artifacts: ArtifactStore,
    /// The policies in force.
    pub policies: Arc<PolicyStore>,
    /// The clock.
    pub clock: Arc<dyn Clock>,
    /// The fetch engine; `None` when no proxy pool is configured.
    pub engine: Option<EngineParts>,
    /// Where `/metrics` is rendered from; `None` when no exporter is
    /// installed, as in most tests.
    pub metrics: Option<MetricsHandle>,
}

impl fmt::Debug for ApiParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiParts")
            .field("engine", &self.engine.is_some())
            .field("metrics", &self.metrics.is_some())
            .finish_non_exhaustive()
    }
}

/// The requests with a key that are being answered: what a stopping server
/// waits for before it closes its listener (F121).
#[derive(Debug, Default)]
pub(crate) struct InFlight {
    count: AtomicUsize,
}

/// One request counted in [`InFlight`] until it is dropped.
#[derive(Debug)]
pub(crate) struct Counted<'a>(&'a InFlight);

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.count.fetch_sub(1, Ordering::AcqRel);
    }
}

/// How often a stopping server looks whether its requests have ended.
const SETTLE_EVERY: Duration = Duration::from_millis(50);

impl InFlight {
    /// Counts a request until the guard is dropped.
    pub(crate) fn enter(&self) -> Counted<'_> {
        self.count.fetch_add(1, Ordering::AcqRel);
        Counted(self)
    }

    /// How many are being answered.
    pub(crate) fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Waits until none is being answered, or until `until`.
    pub(crate) async fn settle(&self, until: tokio::time::Instant) {
        while self.count() > 0 && tokio::time::Instant::now() < until {
            tokio::time::sleep(SETTLE_EVERY).await;
        }
    }
}

pub(crate) struct Inner {
    pub(crate) parts: ApiParts,
    pub(crate) limits: Limits,
    pub(crate) handles: Handles,
    pub(crate) jobs: JobsHub,
    pub(crate) resolver: Option<HostResolver>,
    pub(crate) readiness: Mutex<Option<Readiness>>,
    pub(crate) stopping: AtomicBool,
    pub(crate) in_flight: InFlight,
}

/// The state every handler shares.
#[derive(Clone)]
pub struct ApiState(pub(crate) Arc<Inner>);

impl fmt::Debug for ApiState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiState")
            .field("parts", &self.0.parts)
            .finish_non_exhaustive()
    }
}

/// Why the API's state cannot be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    /// The master key of the configuration is not 32 bytes of hexadecimal.
    #[error("secrets.master_key is not 32 bytes of hexadecimal")]
    MasterKey,
}

impl ApiState {
    /// The state over the services `kage serve` composed.
    ///
    /// # Errors
    ///
    /// Fails when the master key of the configuration cannot be read; a
    /// loaded configuration always has a valid one.
    pub fn new(parts: ApiParts) -> Result<Self, StateError> {
        let key = parts
            .config
            .secrets
            .master_key_bytes()
            .ok_or(StateError::MasterKey)?;
        let resolver = parts
            .config
            .security
            .resolve_on_host
            .then(|| HostResolver::for_config(&parts.config));
        Ok(Self(Arc::new(Inner {
            limits: Limits::new(parts.config.server.requests_per_minute),
            handles: Handles::new(&key),
            jobs: JobsHub::new(
                CredentialCipher::new(&key),
                &key,
                parts.config.jobs.max_items_in_flight,
            ),
            resolver,
            readiness: Mutex::new(None),
            stopping: AtomicBool::new(false),
            in_flight: InFlight::default(),
            parts,
        })))
    }

    pub(crate) fn config(&self) -> &Config {
        &self.0.parts.config
    }

    pub(crate) fn store(&self) -> &Store {
        &self.0.parts.store
    }

    pub(crate) fn artifacts(&self) -> &ArtifactStore {
        &self.0.parts.artifacts
    }

    pub(crate) fn policies(&self) -> &Arc<PolicyStore> {
        &self.0.parts.policies
    }

    pub(crate) fn clock(&self) -> &Arc<dyn Clock> {
        &self.0.parts.clock
    }

    pub(crate) fn engine(&self) -> Option<&EngineParts> {
        self.0.parts.engine.as_ref()
    }

    pub(crate) fn metrics(&self) -> Option<&MetricsHandle> {
        self.0.parts.metrics.as_ref()
    }

    pub(crate) fn limits(&self) -> &Limits {
        &self.0.limits
    }

    pub(crate) fn handles(&self) -> &Handles {
        &self.0.handles
    }

    pub(crate) fn resolver(&self) -> Option<&HostResolver> {
        self.0.resolver.as_ref()
    }

    pub(crate) fn jobs(&self) -> &JobsHub {
        &self.0.jobs
    }

    /// A sender for the events of jobs over `transport`: `kage serve` runs
    /// it beside the job worker. The worker wakes it when it queued an
    /// event.
    ///
    /// # Errors
    ///
    /// Fails when the master key of the configuration cannot be read; a
    /// loaded configuration always has a valid one.
    pub fn webhook_sender(
        &self,
        transport: Arc<dyn crate::jobs::webhook::Transport>,
    ) -> Result<Arc<crate::jobs::webhook::WebhookSender>, StateError> {
        let key = self
            .config()
            .secrets
            .master_key_bytes()
            .ok_or(StateError::MasterKey)?;
        let sender = crate::jobs::webhook::WebhookSender::new(
            self.store().clone(),
            self.config().webhooks.clone(),
            CredentialCipher::new(&key),
            Arc::clone(self.clock()),
            transport,
        );
        self.0.jobs.set_sender(Arc::clone(&sender));
        Ok(sender)
    }

    /// Tells the API that the server is stopping: `/readyz` says so at
    /// once, so that whatever routes requests here stops sending new ones
    /// while those in flight finish.
    pub fn stop(&self) {
        self.0.stopping.store(true, Ordering::Relaxed);
    }

    /// What a running server does every
    /// [`UPKEEP_EVERY`](crate::metrics::UPKEEP_EVERY): the exporter's
    /// histograms are drained, and the limit states of keys that have
    /// nothing in flight are let go.
    pub fn upkeep(&self) {
        if let Some(metrics) = self.metrics() {
            metrics.upkeep();
        }
        self.0.limits.forget_idle();
    }

    pub(crate) fn is_stopping(&self) -> bool {
        self.0.stopping.load(Ordering::Relaxed)
    }

    /// Counts a request with a key until the guard is dropped.
    pub(crate) fn enter(&self) -> Counted<'_> {
        self.0.in_flight.enter()
    }

    /// Waits until no request with a key is being answered, or until
    /// `until`: what a stopping server waits for, after its jobs wound down,
    /// before it closes its listener. `/readyz` answers `503 stopping` all
    /// the while (spec §16.1).
    pub async fn settle(&self, until: tokio::time::Instant) {
        self.0.in_flight.settle(until).await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::InFlight;

    #[tokio::test(start_paused = true)]
    async fn a_stopping_server_waits_for_its_requests_and_no_longer_than_it_may() {
        let requests = InFlight::default();
        // Nothing in flight: no wait at all.
        let start = tokio::time::Instant::now();
        requests.settle(start + Duration::from_secs(30)).await;
        assert_eq!(start.elapsed(), Duration::ZERO);

        // One in flight: the wait ends when it does.
        let counted = requests.enter();
        assert_eq!(requests.count(), 1);
        let settled = requests.settle(start + Duration::from_secs(30));
        let ended = async {
            tokio::time::sleep(Duration::from_secs(2)).await;
            drop(counted);
        };
        tokio::join!(settled, ended);
        assert_eq!(requests.count(), 0);
        let took = start.elapsed();
        assert!(
            took >= Duration::from_secs(2) && took < Duration::from_secs(3),
            "{took:?}"
        );

        // One that never ends: the wait ends at the bound.
        let _held = requests.enter();
        let start = tokio::time::Instant::now();
        requests.settle(start + Duration::from_secs(30)).await;
        let took = start.elapsed();
        assert!(
            took >= Duration::from_secs(30) && took < Duration::from_secs(31),
            "{took:?}"
        );
    }
}
