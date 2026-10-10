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
//! - The pipeline runs one fetch through both paths.
//! - The bandwidth service keeps the budget of the proxy plan, and its
//!   hard stop reaches the scheduler, the forwarder and the open browsers.
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
use kage_core::config::Config;
use kage_core::policy::PolicyStore;
use kage_core::schedule::budget::{Budget, BudgetRules};
use kage_core::schedule::planner::Refusal;
use kage_core::seed::Seed;
use kage_persona::{BrowserGate, Egress, PersonaManager};
use kage_proxy::Pool;
use kage_scheduler::{
    AttemptExecutor, Backend, Bandwidth, Forwarder, ForwarderSettings, Ledger, PathExecutor,
    Pipeline, PipelineSettings, PoolRoutes, Routes, Scheduler, SchedulerGate, StoreBackend,
    StoreLedger, Tokens, Verdicts, cooldown_rules, planner_rules,
};
use kage_store::Store;
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
    pool: Arc<Pool>,
    context_idle: Duration,
    flush_interval: Duration,
    stops: Arc<Stops>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
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

impl Engine {
    /// Composes the engine and starts the forwarder and the scheduler.
    /// The forwarder listens on `bind`; a port of 0 takes a free one,
    /// which is then the one the sidecars are told.
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
        let routes: Arc<dyn Routes> = Arc::new(PoolRoutes::new(store.clone(), Arc::clone(&pool)));
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
        let backend: Arc<dyn Backend> =
            Arc::new(StoreBackend::new(store.clone(), Arc::clone(&pool)));
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
        let executor: Arc<dyn AttemptExecutor> = Arc::new(PathExecutor::new(
            Arc::clone(&personas),
            Arc::clone(&verdicts),
            config,
            Some(Arc::clone(&meter)),
        ));
        let pipeline = Pipeline::new(
            Arc::clone(&scheduler),
            executor,
            verdicts,
            PipelineSettings::from_config(config),
        );
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

        let (shutdown, _) = watch::channel(false);
        let tasks = vec![
            tokio::spawn(Arc::clone(&forwarder).serve(listener, stopped(shutdown.subscribe()))),
            tokio::spawn(Arc::clone(&scheduler).run(stopped(shutdown.subscribe()))),
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
            stops: Arc::new(Stops::default()),
            shutdown,
            tasks,
        })
    }

    /// Where the budget stands goes to the scheduler and the forwarder.
    fn on_budget(&self) -> impl FnMut(Budget) + Send + 'static {
        let scheduler = Arc::clone(&self.scheduler);
        let forwarder = Arc::clone(&self.forwarder);
        let stops = Arc::clone(&self.stops);
        move |budget: Budget| {
            scheduler.set_bandwidth_stop(budget.stop);
            stops.budget.store(budget.stop, Ordering::Relaxed);
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
            stopped(self.shutdown.subscribe()),
        ));
        let tend = tokio::spawn(tend(
            Arc::clone(&self.personas),
            Arc::clone(&self.pool),
            Arc::clone(&self.forwarder),
            Arc::clone(&self.stops),
            self.context_idle,
            stopped(self.shutdown.subscribe()),
        ));
        self.tasks.extend([bandwidth, tend]);
    }

    /// Stops everything: what waits is refused, the browsers this process
    /// opened are closed, and what the forwarder counted is written.
    pub(crate) async fn stop(mut self) {
        let _ = self.shutdown.send(true);
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

/// See [`Engine::serve`].
async fn tend(
    personas: Arc<PersonaManager>,
    pool: Arc<Pool>,
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
        match pool.egress_unsafe().await {
            Ok(leak) => stops.leak.store(leak, Ordering::Relaxed),
            Err(error) => tracing::warn!(%error, "cannot read the egress flag"),
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
    use kage_core::domain::ProxyEndpoint;
    use kage_core::ids::PersonaId;
    use kage_scheduler::forwarder::RouteRefusal;
    use kage_scheduler::{Forwarder, ForwarderSettings, Routes, Tokens};

    use super::Stops;

    #[derive(Debug)]
    struct NoRoutes;

    #[async_trait]
    impl Routes for NoRoutes {
        async fn upstream(&self, _: PersonaId) -> Result<ProxyEndpoint, RouteRefusal> {
            Err(RouteRefusal::UnknownPersona)
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
