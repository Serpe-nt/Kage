//! The fetch engine: everything between a fetch order and the wire,
//! composed (spec §3.2).
//!
//! - The forwarder is bound first and becomes the egress of every
//!   persona: browsers and the http path are handed it under the
//!   persona's own credential, and the provider's credentials stay in
//!   this process (follow-up F10).
//! - The scheduler runs over the store and the pool; the persona
//!   manager's own browser work (warm-up visits, scheduled self-tests)
//!   asks it first (follow-up F27).
//! - The pipeline runs one fetch through both paths, and keeps what a
//!   request asks to keep of its page in the artifact store.
//! - The bandwidth service keeps the budget of the proxy plan, and its
//!   hard stop reaches the scheduler, the forwarder and the open browsers.
//! - A plain client (level 0, spec §25) leaves through the forwarder too,
//!   from its exit (`egress.plain`). Under `egress.mode: direct` nothing
//!   uses the pool: its stops (a leak, the provider's budget) stop nothing,
//!   and what leaves from the host's connection is counted but costs
//!   nothing of the budget.
//!
//! `kage serve` runs all of it until shutdown; `kage fetch` runs it for
//! one request.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Context;
use kage_core::alert::AlertSink;
use kage_core::config::{Config, EgressMode};
use kage_core::policy::PolicyStore;
use kage_core::schedule::budget::{Budget, BudgetRules};
use kage_core::schedule::planner::Refusal;
use kage_core::seed::Seed;
use kage_persona::{BrowserGate, Egress, PersonaManager};
use kage_proxy::Pool;
use kage_scheduler::{
    Archive, AttemptExecutor, Backend, Bandwidth, Forwarder, ForwarderSettings, Ledger,
    PathExecutor, Pipeline, PipelineSettings, PoolRoutes, RobotsSettings, Routes, Scheduler,
    SchedulerGate, StoreArchive, StoreBackend, StoreLedger, Tokens, Verdicts, cooldown_rules,
    planner_rules,
};
use kage_store::{ArtifactStore, Store};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::persona_cmd;

/// How often the engine looks after itself: the leak flag reaches the
/// forwarder and idle browsers are closed.
const TEND_EVERY: Duration = Duration::from_secs(5);

/// Why nothing may leave, as the forwarder is told.
#[derive(Debug, Default)]
struct Stops {
    /// A leak is on record.
    leak: AtomicBool,
    /// The bandwidth budget is used up and the hard stop is on.
    budget: AtomicBool,
    /// What the forwarder was told last.
    applied: Mutex<Option<&'static str>>,
    /// A stop began since the browsers were last closed.
    close_due: AtomicBool,
}

impl Stops {
    fn reason(&self) -> Option<&'static str> {
        if self.budget.load(Ordering::Relaxed) {
            Some(Refusal::BandwidthExhausted.as_str())
        } else if self.leak.load(Ordering::Relaxed) {
            Some(Refusal::EgressUnsafe.as_str())
        } else {
            None
        }
    }

    /// Tells the forwarder where things stand. Open browsers are closed
    /// when a stop begins: their own background traffic ends with them.
    fn apply(&self, forwarder: &Forwarder) {
        let reason = self.reason();
        let mut applied = self.applied.lock().unwrap_or_else(PoisonError::into_inner);
        if *applied == reason {
            return;
        }
        if let Some(reason) = reason {
            tracing::warn!(
                reason,
                "nothing leaves: the forwarder relays no new connection"
            );
            if applied.is_none() {
                self.close_due.store(true, Ordering::Relaxed);
            }
        } else {
            tracing::info!("the forwarder relays again");
        }
        forwarder.set_stop(reason);
        *applied = reason;
    }
}

/// See the module documentation.
pub(crate) struct Engine {
    pub(crate) personas: Arc<PersonaManager>,
    pub(crate) scheduler: Arc<Scheduler>,
    pub(crate) pipeline: Pipeline,
    pub(crate) forwarder: Arc<Forwarder>,
    pub(crate) bandwidth: Arc<Bandwidth>,
    pub(crate) pool: Arc<Pool>,
    context_idle: Duration,
    flush_interval: Duration,
    /// Whether anything leaves through the pool (`egress.mode: proxies`):
    /// only then do its stops apply.
    pooled: bool,
    stops: Arc<Stops>,
    signals: Signals,
    tasks: Vec<JoinHandle<()>>,
}

/// What tells the engine's tasks to stop, in two steps.
#[derive(Debug)]
struct Signals {
    /// Tells the scheduler to grant nothing more; the forwarder goes on
    /// relaying for the pages that are being read (follow-up F39).
    quiesce: watch::Sender<bool>,
    /// Tells everything else to stop.
    shutdown: watch::Sender<bool>,
}

impl Default for Signals {
    fn default() -> Self {
        Self {
            quiesce: watch::Sender::new(false),
            shutdown: watch::Sender::new(false),
        }
    }
}

impl Signals {
    /// Resolves when the scheduler is told to grant nothing more.
    fn quiesced(&self) -> impl Future<Output = ()> + Send + use<> {
        stopped(self.quiesce.subscribe())
    }

    /// Resolves when everything is told to stop.
    fn stopped(&self) -> impl Future<Output = ()> + Send + use<> {
        stopped(self.shutdown.subscribe())
    }
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("forwarder", &self.forwarder.local_addr())
            .finish_non_exhaustive()
    }
}

/// Resolves when `shutdown` says so, or when its sender is gone.
async fn stopped(mut shutdown: watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}

/// The forwarder's routes of this installation: through the pool, or from
/// the host's own connection under `egress.mode: direct`, with level 0
/// leaving by the exit `egress.plain` names (spec §25). The server's
/// forwarder and a command's own both take theirs from here, so that a
/// command routes a host persona as the server does.
pub(crate) fn routes(config: &Config, store: &Store, pool: &Arc<Pool>) -> PoolRoutes {
    let routes = PoolRoutes::new(store.clone(), Arc::clone(pool))
        .with_plain_exit(config.egress.plain_exit());
    match config.egress.mode {
        EgressMode::Direct => routes.direct(),
        EgressMode::Proxies => routes,
    }
}

impl Engine {
    /// Composes the engine and starts the forwarder and the scheduler.
    /// The forwarder listens on `bind`; a port of 0 takes a free one,
    /// which is then the one the sidecars are told.
    // Composes the engine in the order of the module's list.
    #[allow(clippy::too_many_lines)]
    pub(crate) async fn start(
        config: &Config,
        store: &Store,
        pool: Arc<Pool>,
        alerts: Arc<dyn AlertSink>,
        policies: Arc<PolicyStore>,
        bind: SocketAddr,
    ) -> anyhow::Result<Self> {
        let key = config
            .secrets
            .master_key_bytes()
            .context("secrets.master_key is not 32 bytes of hexadecimal")?;
        let pooled = config.egress.mode == EgressMode::Proxies;
        let routes: Arc<dyn Routes> = Arc::new(routes(config, store, &pool));
        let (forwarder, listener) = Forwarder::bind(
            bind,
            ForwarderSettings::from_config(config),
            Tokens::new(&key),
            routes,
        )
        .await
        .with_context(|| format!("cannot bind the forwarder to {bind}"))?;
        let egress = forwarder.egress();
        tracing::info!(
            local = %forwarder.local_addr(),
            advertised = %config.forwarder.advertise,
            "the forwarder listens: every persona's traffic leaves through it"
        );

        let egress: Arc<dyn Egress> = Arc::new(egress);
        let personas = Arc::new(
            persona_cmd::build(config, store, Arc::clone(&pool), Arc::clone(&alerts))?
                .with_egress(egress),
        );

        let clock = Arc::clone(pool.clock());
        let mut stored = StoreBackend::new(store.clone(), Arc::clone(&pool));
        if !pooled {
            stored = stored.direct();
        }
        let backend: Arc<dyn Backend> = Arc::new(stored);
        // A UUIDv7 carries 74 random bits; the seed draws ties and pauses,
        // it guards nothing.
        let seed = Seed::derive("scheduler", &[uuid::Uuid::now_v7().as_bytes()]);
        let scheduler = Scheduler::new(
            planner_rules(config),
            seed,
            Arc::clone(&backend),
            Arc::clone(&clock),
        );
        let verdicts = Arc::new(Verdicts::new(
            backend,
            policies,
            cooldown_rules(config),
            config.scheduler.breaker_max_pause.get(),
            config.scheduler.breaker_trial_timeout.get(),
            Arc::clone(&alerts),
            Arc::clone(&clock),
        ));
        let meter = Arc::clone(forwarder.meter());
        let gate: Arc<dyn BrowserGate> = Arc::new(SchedulerGate::new(
            Arc::clone(&scheduler),
            Arc::clone(&verdicts),
            Some(Arc::clone(&meter)),
        ));
        personas.set_gate(gate);
        let executor: Arc<dyn AttemptExecutor> = Arc::new(
            PathExecutor::new(
                Arc::clone(&personas),
                Arc::clone(&verdicts),
                config,
                Some(Arc::clone(&meter)),
            )
            .with_plain(forwarder.egress()),
        );
        // Bodies, screenshots and PDFs a request asks to keep go into the
        // artifact store, next to where the sidecars write their files.
        let archive: Arc<dyn Archive> = Arc::new(StoreArchive::new(ArtifactStore::new(
            store.pool().clone(),
            &config.artifacts,
        )));
        let pipeline = Pipeline::new(
            Arc::clone(&scheduler),
            executor,
            verdicts,
            PipelineSettings::from_config(config),
        )
        .with_archive(archive)
        .with_robots(RobotsSettings::from_config(config));
        let ledger: Arc<dyn Ledger> = Arc::new(StoreLedger::new(store.clone(), Arc::clone(&pool)));
        let bandwidth = Arc::new(Bandwidth::new(
            meter,
            ledger,
            BudgetRules {
                cap_bytes: config.bandwidth.cap_bytes(),
                warn_at: config.bandwidth.warn_at,
                hard_stop: config.bandwidth.hard_stop,
            },
            alerts,
            clock,
        ));

        // The scheduler has a signal of its own: it stops first.
        let signals = Signals::default();
        let tasks = vec![
            tokio::spawn(Arc::clone(&forwarder).serve(listener, signals.stopped())),
            tokio::spawn(Arc::clone(&scheduler).run(signals.quiesced())),
        ];
        Ok(Self {
            personas,
            scheduler,
            pipeline,
            forwarder,
            bandwidth,
            pool,
            context_idle: config.scheduler.context_idle.get(),
            flush_interval: config.bandwidth.flush_interval.get(),
            pooled,
            stops: Arc::new(Stops::default()),
            signals,
            tasks,
        })
    }

    /// Where the budget stands goes to the scheduler and the forwarder.
    /// Without the pool there is no provider's budget to stop for.
    fn on_budget(&self) -> impl FnMut(Budget) + Send + 'static {
        let scheduler = Arc::clone(&self.scheduler);
        let forwarder = Arc::clone(&self.forwarder);
        let stops = Arc::clone(&self.stops);
        let pooled = self.pooled;
        move |budget: Budget| {
            let stop = pooled && budget.stop;
            scheduler.set_bandwidth_stop(stop);
            stops.budget.store(stop, Ordering::Relaxed);
            stops.apply(&forwarder);
        }
    }

    /// Judges the budget once, and applies its hard stop: for a command
    /// that makes one fetch.
    pub(crate) async fn check_budget(&self) -> anyhow::Result<Budget> {
        let budget = self
            .bandwidth
            .assess()
            .await
            .context("cannot judge the bandwidth budget")?;
        (self.on_budget())(budget);
        Ok(budget)
    }

    /// Starts what a server runs besides the forwarder and the scheduler:
    /// the bandwidth service, and the loop that carries the leak flag to
    /// the forwarder and closes browsers nobody uses.
    pub(crate) fn serve(&mut self) {
        let bandwidth = tokio::spawn(Arc::clone(&self.bandwidth).run(
            self.flush_interval,
            self.on_budget(),
            self.signals.stopped(),
        ));
        let tend = tokio::spawn(tend(
            Arc::clone(&self.personas),
            Arc::clone(&self.pool),
            self.pooled,
            Arc::clone(&self.forwarder),
            Arc::clone(&self.stops),
            self.context_idle,
            self.signals.stopped(),
        ));
        self.tasks.extend([bandwidth, tend]);
    }

    /// Tells the scheduler to stop: what waits for a persona is refused
    /// with `shutting_down` and nothing new is granted. The attempts that
    /// are under way go on, through the forwarder, until they end or
    /// [`Self::stop`] closes their browsers.
    pub(crate) fn quiesce(&self) {
        let _ = self.signals.quiesce.send(true);
    }

    /// Stops everything: what waits is refused, the browsers this process
    /// opened are closed, and what the forwarder counted is written.
    pub(crate) async fn stop(mut self) {
        self.quiesce();
        let _ = self.signals.shutdown.send(true);
        for task in self.tasks.drain(..) {
            if let Err(error) = task.await {
                tracing::warn!(%error, "a task of the engine ended badly");
            }
        }
        let closed = self.personas.contexts().close_all().await;
        if closed > 0 {
            tracing::info!(closed, "browser contexts closed");
        }
        if let Err(error) = self.bandwidth.flush().await {
            tracing::warn!(%error, "the last bandwidth counts could not be written");
        }
    }
}

/// See [`Engine::serve`]. The pool's leak flag stops everything when
/// anything leaves through the pool (`pooled`).
async fn tend(
    personas: Arc<PersonaManager>,
    pool: Arc<Pool>,
    pooled: bool,
    forwarder: Arc<Forwarder>,
    stops: Arc<Stops>,
    context_idle: Duration,
    shutdown: impl Future<Output = ()> + Send,
) {
    let mut ticker = tokio::time::interval(TEND_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            _ = ticker.tick() => {}
        }
        if pooled {
            match pool.egress_unsafe().await {
                Ok(leak) => stops.leak.store(leak, Ordering::Relaxed),
                Err(error) => tracing::warn!(%error, "cannot read the egress flag"),
            }
        }
        stops.apply(&forwarder);
        let contexts = personas.contexts();
        if stops.close_due.swap(false, Ordering::Relaxed) {
            let closed = contexts.close_all().await;
            tracing::warn!(closed, "browser contexts closed: nothing may be fetched");
        }
        let idle_since = chrono::Duration::from_std(context_idle)
            .ok()
            .and_then(|idle| pool.clock().now().checked_sub_signed(idle));
        if let Some(before) = idle_since {
            let closed = contexts.close_idle(before).await;
            if closed > 0 {
                tracing::debug!(closed, "idle browser contexts closed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use async_trait::async_trait;
    use kage_core::ids::PersonaId;
    use kage_scheduler::forwarder::{PlainRoute, RouteRefusal, SystemResolver, Upstream};
    use kage_scheduler::{Forwarder, ForwarderSettings, Routes, Tokens};

    use super::Stops;

    #[derive(Debug)]
    struct NoRoutes;

    #[async_trait]
    impl Routes for NoRoutes {
        async fn upstream(&self, _: PersonaId) -> Result<Upstream, RouteRefusal> {
            Err(RouteRefusal::UnknownPersona)
        }

        async fn plain(&self, _: PlainRoute) -> Result<Upstream, RouteRefusal> {
            Err(RouteRefusal::NotConfigured)
        }
    }

    #[tokio::test]
    async fn the_budget_outranks_the_leak_and_a_stop_that_begins_closes_the_browsers() {
        let (forwarder, _listener) = Forwarder::bind(
            "127.0.0.1:0".parse().unwrap(),
            ForwarderSettings {
                advertised: ("kage".to_owned(), 0),
                connect_timeout: Duration::from_secs(1),
                idle_timeout: Duration::from_secs(1),
                max_connections: 4,
                operator_hosts: HashSet::new(),
                private_names: HashSet::new(),
                resolver: Arc::new(SystemResolver),
            },
            Tokens::new(&[1; 32]),
            Arc::new(NoRoutes),
        )
        .await
        .unwrap();
        let stops = Stops::default();

        assert_eq!(stops.reason(), None);
        stops.apply(&forwarder);
        assert!(!stops.close_due.load(Ordering::Relaxed));

        stops.leak.store(true, Ordering::Relaxed);
        assert_eq!(stops.reason(), Some("egress_unsafe"));
        stops.apply(&forwarder);
        assert!(stops.close_due.swap(false, Ordering::Relaxed));

        // A second reason on top of the first closes nothing again.
        stops.budget.store(true, Ordering::Relaxed);
        assert_eq!(stops.reason(), Some("bandwidth_exhausted"));
        stops.apply(&forwarder);
        assert!(!stops.close_due.load(Ordering::Relaxed));

        stops.budget.store(false, Ordering::Relaxed);
        stops.leak.store(false, Ordering::Relaxed);
        stops.apply(&forwarder);
        assert_eq!(stops.reason(), None);
        assert!(!stops.close_due.load(Ordering::Relaxed));

        // And a stop that begins again closes again.
        stops.budget.store(true, Ordering::Relaxed);
        stops.apply(&forwarder);
        assert!(stops.close_due.load(Ordering::Relaxed));
    }
}
