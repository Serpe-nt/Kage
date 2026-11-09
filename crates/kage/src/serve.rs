//! `kage serve`: checks the database, loads the policies, starts the
//! background jobs, the proxy pool, the personas and the fetch engine
//! (forwarder, scheduler, bandwidth budget), and serves the API until
//! shutdown.
//!
//! Beside the API run the job worker (spec §16.6), which claims jobs and
//! works them through the scheduler, and the webhook sender, which posts
//! their events from this host's own address to the receivers the operator
//! listed. Both stop with the server: the worker gives its jobs back, so
//! that they are taken up again after a restart.
//!
//! The API listens on `server.bind`, which is loopback unless configured
//! otherwise, with the limits of spec §19 on every connection
//! (`kage_api::serve`). With `server.metrics_bind` a second listener
//! serves `/metrics` without a key for a Prometheus on the same private
//! network. The metrics exporter is installed here, once per process:
//! everything the pool, the scheduler and the forwarder recorded through
//! the `metrics` facade since M2 is scraped from now on.
//!
//! Kage is single-instance (ADR 0005, F174): a server holds the database's
//! server lock (`kage_store::ServerLock`) for its life, and a second server
//! on the same database refuses to start and says so. A server whose lock
//! was taken by another while its own connection was lost stops.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use chrono::Utc;
use kage_api::metrics::{MetricsHandle, UPKEEP_EVERY};
use kage_api::serve::ServeSettings;
use kage_api::{ApiParts, ApiState, DirectTransport, EngineParts};
use kage_core::alert::AlertSink;
use kage_core::config::{Config, DirectEgressConfig, EgressMode};
use kage_core::domain::Declaration;
use kage_core::policy::PolicyStore;
use kage_persona::PersonaProbeClients;
use kage_proxy::health::{Endpoints, HostIp};
use kage_proxy::{HealthChecker, Pool, SystemClock};
use kage_store::{ArtifactStore, ServerLock, Store};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use crate::engine::Engine;
use crate::{alerts, migrate};

/// Blobs younger than this are never treated as orphans: a write in
/// progress has a blob on disk before its row is committed.
const ORPHAN_GRACE: Duration = Duration::from_hours(1);

/// How often the proxy pool ends backoffs that ran out and follows
/// replacements in flight.
const POOL_TICK: Duration = Duration::from_secs(10);

/// How often a server looks whether it still holds its database's lock.
const LOCK_LOOK_EVERY: Duration = Duration::from_secs(15);

/// How long a look at the lock may take before it confirms nothing: the
/// look on the lock's session, and taking it again on a new connection
/// after a look that failed at once, share it.
const LOCK_LOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a server serves without having confirmed its lock. Below the
/// minute after which Postgres gives up a silent session and its lock
/// (`kage_store::KEEPALIVES`: 30 s of silence, then three
/// questions 10 s apart), counted from the last answer: the server stops
/// writing to the database before another one could take the lock (QA
/// round three's lock-02, QA round four lock-01).
const LOCK_UNCONFIRMED_LIMIT: Duration = Duration::from_secs(40);

/// How long what runs may take to end once a server stopped for its lock:
/// the pages are cut at once and every write fails at once, so it is
/// quick; what is left after it is cut (QA round four lock-01).
const FENCED_STOP_WITHIN: Duration = Duration::from_secs(10);

/// How long the browsers may take to close after that.
const FENCED_ENGINE_STOP_WITHIN: Duration = Duration::from_secs(5);

/// How long a server that stopped for its lock tries to end its own
/// sessions in the database on its way out (QA round four lock-02).
const OWN_SESSIONS_WITHIN: Duration = Duration::from_secs(3);

/// What a server that finds another one on its database is told.
const ANOTHER_SERVER: &str = "another kage server is running on this database: it holds the \
     lock every server holds for its life. Kage is single-instance (ADR 0005): stop the other \
     server first. A job the other server held is taken up by this one once it starts, when \
     the job's lease has run out";

/// What the fetch engine is made of, once it runs.
struct Started {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    engine: Option<(Engine, Arc<HealthChecker>)>,
}

/// Starts everything `serve` needs and serves until SIGINT or SIGTERM.
///
/// Startup fails, instead of serving in a broken state, when the database
/// is unreachable or not migrated, or when a policy file is invalid.
pub(crate) async fn run(config: Config, bind: Option<SocketAddr>) -> anyhow::Result<()> {
    let store = migrate::connect(&config).await?;
    let status = store
        .migration_status()
        .await
        .context("cannot read the migration state")?;
    migrate::ensure_clean(&status)?;
    // One server per database, before anything is started (F174).
    let lock = ServerLock::take(store.pool())
        .await
        .context("cannot take the database's server lock")?
        .ok_or_else(|| anyhow::anyhow!(ANOTHER_SERVER))?;
    let (lost, lock_lost) = watch::channel(None);
    let keeper = tokio::spawn(keep_the_lock(
        StoreLock {
            store: store.clone(),
            lock,
        },
        lost,
    ));

    let (alerts, recorder) = alerts::recorder(store.clone());
    let policies = Arc::new(
        PolicyStore::open(&config.policies.dir, Arc::clone(&alerts))
            .and_then(|store| store.with_cache_bound(config.cache.max_ttl.get()))
            // A reload's domains are named for the metrics as the start's
            // are, while room is left (obs-01).
            .map(|store| store.with_reload_hook(Arc::new(kage_api::metrics::name_reloaded)))
            .context("policies are invalid")?,
    );
    let current = policies.current();
    tracing::info!(
        dir = %config.policies.dir.display(),
        domains = current.domains.domains().len(),
        pacing_profiles = current.pacing.as_map().len(),
        signature_rules = current.signatures.rules().len(),
        hot_reload = config.policies.hot_reload,
        "policies loaded"
    );
    // Held until shutdown: dropping it stops the watch.
    let _watcher = if config.policies.hot_reload {
        Some(
            policies
                .watch()
                .context("cannot watch the policy directory")?,
        )
    } else {
        None
    };

    // Before anything records: what is counted from here on is scraped, with
    // the `domain` label's values bounded, the policies' domains first (L3).
    kage_api::metrics::bound_domains(&config.server, &current);
    let metrics = match kage_api::metrics::install() {
        Ok(handle) => {
            // Taken above; the keeper says so from here on (lock-02).
            kage_api::metrics::server_lock(true, Utc::now());
            Some(handle)
        }
        Err(error) => {
            tracing::warn!(%error, "the metrics exporter could not be installed; /metrics is empty");
            None
        }
    };

    let artifacts = ArtifactStore::new(store.pool().clone(), &config.artifacts);
    let retention = tokio::spawn(run_retention(
        artifacts.clone(),
        config.artifacts.sweep_interval.get(),
    ));

    let started = match start_proxy_pool(&config, &store, alerts, Arc::clone(&policies)).await {
        Ok(started) => started,
        Err(error) => {
            retention.abort();
            recorder.abort();
            keeper.abort();
            store.close().await;
            return Err(error);
        }
    };

    let served = serve_api(
        &config,
        bind,
        &store,
        artifacts,
        policies,
        started.engine.as_ref(),
        (metrics, lock_lost),
    )
    .await;

    for task in started.tasks {
        task.abort();
    }
    // After the manager's own work stopped: what waits is refused, the
    // browsers are closed and the last byte counts are written. A fenced
    // store writes none of them, and the browsers get a bound.
    if let Some((engine, _)) = started.engine {
        if store.fenced() {
            if tokio::time::timeout(FENCED_ENGINE_STOP_WITHIN, engine.stop())
                .await
                .is_err()
            {
                tracing::warn!("the browsers had not closed in time; the server exits");
            }
        } else {
            engine.stop().await;
        }
    }
    retention.abort();
    recorder.abort();
    // Last: the lock goes with its connection.
    keeper.abort();
    ended(&store, served).await
}

/// What `serve` comes to once everything it started stopped: the store is
/// closed, or, for a server that stopped for its database's lock, its
/// sessions in the database are ended and the error says why it stopped.
async fn ended(store: &Store, served: anyhow::Result<Option<LockLoss>>) -> anyhow::Result<()> {
    let lost = match served {
        Ok(lost) => lost,
        Err(error) => {
            if !store.fenced() {
                store.close().await;
            }
            return Err(error);
        }
    };
    let Some(lost) = lost else {
        store.close().await;
        return Ok(());
    };
    // The store is fenced: its pool would wait for connections a cut
    // network holds. What this server still has in the database (the
    // lock's session, a statement the network held up) is ended over a
    // connection of its own when the database can be reached, so that the
    // lock is free at once and nothing of this server's runs after it
    // (QA round four lock-02).
    let sessions = match store.end_own_sessions(OWN_SESSIONS_WITHIN).await {
        Ok(ended) => {
            tracing::info!(
                sessions = ended,
                "this server's sessions in the database were ended"
            );
            OwnSessions::Ended
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "this server's sessions in the database could not be ended: Postgres ends \
                 them, and lets the lock go, about a minute after they last answered"
            );
            OwnSessions::Unreachable
        }
    };
    match lost {
        LockLoss::Taken => anyhow::bail!(ANOTHER_SERVER),
        LockLoss::Unconfirmed { after } => Err(LockUnconfirmed { after, sessions }.into()),
    }
}

/// The database's server lock as [`keep_the_lock`] looks at it.
trait HeldLock {
    /// Whether the lock is still this server's: its session answers and
    /// holds it. An error when the session does not answer.
    fn held(&mut self) -> impl Future<Output = Result<bool, kage_store::StoreError>> + Send;
    /// Takes the lock again on a new connection, after the one that held
    /// it stopped answering or was lost: a session of this server's own
    /// that still holds it is ended first. False when another server
    /// holds it now.
    fn take_again(&mut self) -> impl Future<Output = Result<bool, kage_store::StoreError>> + Send;
    /// Fences the server's store: nothing it writes reaches the database
    /// from now on (QA round four lock-01).
    fn fence(&self);
}

/// The lock a server holds, and the store it takes it again from.
struct StoreLock {
    store: Store,
    lock: ServerLock,
}

impl HeldLock for StoreLock {
    async fn held(&mut self) -> Result<bool, kage_store::StoreError> {
        self.lock.held().await
    }

    async fn take_again(&mut self) -> Result<bool, kage_store::StoreError> {
        Ok(
            match ServerLock::take_back(self.store.pool(), self.lock.pid()).await? {
                Some(again) => {
                    self.lock = again;
                    true
                }
                None => false,
            },
        )
    }

    fn fence(&self) {
        self.store.fence();
    }
}

/// Why a server stops for its database's lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockLoss {
    /// Another server took the lock while this one's connection was lost.
    Taken,
    /// No look confirmed the lock for [`LOCK_UNCONFIRMED_LIMIT`]: the
    /// database could not be reached (a partition), and Postgres may give
    /// the lock to another server soon.
    Unconfirmed {
        /// How long the lock had not been confirmed when the server
        /// decided to stop.
        after: Duration,
    },
}

/// The exit code of a server that stopped because it could not confirm its
/// database's lock (`EX_TEMPFAIL`): nothing is wrong with its configuration
/// and no other server was seen, and starting it again is right once the
/// database can be reached. Every other failure of `serve` exits 1.
pub(crate) const EXIT_LOCK_UNCONFIRMED: u8 = 75;

/// What became of a stopped server's own sessions in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnSessions {
    /// They were ended on the way out: the lock is free.
    Ended,
    /// The database could not be reached on the way out.
    Unreachable,
}

/// What a server that could not confirm its lock says as it exits.
#[derive(Debug)]
pub(crate) struct LockUnconfirmed {
    /// Since the last confirmation, when the server decided to stop.
    pub(crate) after: Duration,
    /// What became of its sessions.
    pub(crate) sessions: OwnSessions,
}

impl std::fmt::Display for LockUnconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.after.is_zero() {
            f.write_str("this server could not confirm that it holds the database's server lock")?;
        } else {
            write!(
                f,
                "this server could not confirm for {} s that it holds the database's server \
                 lock (the database could not be reached)",
                self.after.as_secs()
            )?;
        }
        f.write_str(
            ". It stopped writing to the database at that moment, before Postgres lets the \
             lock go (about a minute after the lock's session last answered), and the jobs it \
             held are claimed again once their leases run out (ADR 0005). ",
        )?;
        match self.sessions {
            OwnSessions::Ended => f.write_str(
                "Its sessions in the database were ended on the way out: the lock is free now",
            )?,
            OwnSessions::Unreachable => f.write_str(
                "The database could not be reached on the way out: until Postgres lets the \
                 lock go, a server started is refused as if another one ran",
            )?,
        }
        write!(
            f,
            ". Start it again once the database can be reached (exit code \
             {EXIT_LOCK_UNCONFIRMED})"
        )
    }
}

impl std::error::Error for LockUnconfirmed {}

/// Looks every [`LOCK_LOOK_EVERY`] whether the server still holds its
/// database's lock. A lock whose session stopped answering or was lost
/// (Postgres restarted, the network broke) is taken again on a new
/// connection, after ending a session of this server's own that still
/// holds it; when another server took it in the meantime, the store is
/// fenced and `lost` says so (`Taken`): this server stops. Aborting the
/// task drops the lock, and Postgres lets it go with its connection.
///
/// Each look has its own deadline, [`LOCK_LOOK_TIMEOUT`], and confirms
/// nothing when it does not answer within it. A look that confirmed
/// nothing leaves the lock's connection behind what the network held up:
/// the next look takes the lock again on a new connection instead of
/// queueing behind it, so that the first look after a cut heals confirms
/// the lock (QA round four lock-04). A look whose session answers at once
/// that it does not hold the lock, or fails, takes it again within the
/// same deadline.
///
/// The server stops (`Unconfirmed`) only when a look made at or past
/// [`LOCK_UNCONFIRMED_LIMIT`] since the last confirmation fails to confirm
/// too. A server cut off for good decides at the first look at or past the
/// limit, which ends 45 to 50 s after its last confirmation (a look every
/// 15 s, 5 s for each); Postgres gives a silent session's lock up about a
/// minute after its last answer. The decision fences the store before
/// anything else hears of it: from that instant no claim, lease, item,
/// persona or session write of this server's leaves for the database (QA
/// round four lock-01).
async fn keep_the_lock(mut lock: impl HeldLock, lost: watch::Sender<Option<LockLoss>>) {
    let mut ticker = tokio::time::interval(LOCK_LOOK_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker.tick().await;
    let mut confirmed = tokio::time::Instant::now();
    // Whether the lock's connection is behind a look that confirmed nothing.
    let mut stuck = false;
    loop {
        ticker.tick().await;
        let deadline = tokio::time::Instant::now() + LOCK_LOOK_TIMEOUT;
        let mut take = stuck;
        if !stuck {
            match tokio::time::timeout_at(deadline, lock.held()).await {
                Ok(Ok(true)) => {
                    confirmed = tokio::time::Instant::now();
                    kage_api::metrics::server_lock(true, Utc::now());
                    continue;
                }
                Ok(Ok(false)) => {
                    tracing::warn!("the database's server lock is not held by its session");
                    take = true;
                }
                Ok(Err(error)) => {
                    tracing::warn!(%error, "the database's server lock's session is gone");
                    take = true;
                }
                Err(_) => {
                    tracing::warn!("the database's server lock did not answer");
                }
            }
            kage_api::metrics::server_lock(false, Utc::now());
        }
        if take {
            match tokio::time::timeout_at(deadline, lock.take_again()).await {
                Ok(Ok(true)) => {
                    tracing::warn!("the database's server lock is held again, on a new connection");
                    stuck = false;
                    confirmed = tokio::time::Instant::now();
                    kage_api::metrics::server_lock(true, Utc::now());
                    continue;
                }
                Ok(Ok(false)) => {
                    lock.fence();
                    tracing::error!(
                        "another kage server took this database's lock while this server's \
                         connection was lost: this server stopped writing to the database and \
                         stops (ADR 0005)"
                    );
                    kage_api::metrics::server_lock(false, Utc::now());
                    let _ = lost.send(Some(LockLoss::Taken));
                    return;
                }
                Ok(Err(error)) => {
                    tracing::warn!(%error, "the database's server lock cannot be taken again now");
                }
                Err(_) => {
                    tracing::warn!("the database's server lock cannot be taken again now");
                }
            }
        }
        // This look confirmed nothing: the next one goes by a new connection.
        stuck = true;
        let unconfirmed = confirmed.elapsed();
        if unconfirmed >= LOCK_UNCONFIRMED_LIMIT {
            lock.fence();
            tracing::error!(
                unconfirmed_s = unconfirmed.as_secs(),
                "this server could not confirm that it holds the database's server lock: \
                 Postgres may give the lock to another server soon, so this server stopped \
                 writing to the database, stops at once and cuts the pages under way \
                 (ADR 0005)"
            );
            kage_api::metrics::server_lock(false, Utc::now());
            let _ = lost.send(Some(LockLoss::Unconfirmed { after: unconfirmed }));
            return;
        }
        tracing::warn!(
            unconfirmed_s = unconfirmed.as_secs(),
            "the database's server lock is looked at again shortly"
        );
    }
}

/// Builds the API over what was started and serves it until a signal, or
/// until the server stops for its database's lock: then `Some` says why.
#[allow(clippy::too_many_lines)] // One shutdown, in its order, with the bound of a fenced stop.
async fn serve_api(
    config: &Config,
    bind: Option<SocketAddr>,
    store: &Store,
    artifacts: ArtifactStore,
    policies: Arc<PolicyStore>,
    engine: Option<&(Engine, Arc<HealthChecker>)>,
    (metrics, lock_lost): (Option<MetricsHandle>, watch::Receiver<Option<LockLoss>>),
) -> anyhow::Result<Option<LockLoss>> {
    let state = ApiState::new(ApiParts {
        config: Arc::new(config.clone()),
        store: store.clone(),
        artifacts,
        policies,
        clock: Arc::new(SystemClock),
        engine: engine.map(|(engine, health)| EngineParts {
            pipeline: engine.pipeline.clone(),
            personas: Arc::clone(&engine.personas),
            pool: Arc::clone(&engine.pool),
            health: Arc::clone(health),
            bandwidth: Arc::clone(&engine.bandwidth),
        }),
        metrics,
    })
    .context("cannot set up the API")?;

    let bind = bind.unwrap_or(config.server.bind);
    let listener = listen(bind).await?;
    if !bind.ip().is_loopback() {
        tracing::warn!(
            %bind,
            "the API listens beyond loopback: every request needs a key, but the connection \
             is plain HTTP. Put a TLS-terminating proxy in front of it"
        );
    }
    let metrics_listener = match config.server.metrics_bind {
        Some(bind) => Some(listen(bind).await?),
        None => None,
    };

    // One signal stops the upkeep and the work on jobs at once; the
    // listeners close once that work and the requests in flight are done,
    // so that `/readyz` answers `503 stopping` all the while (F121).
    let (stop, stopped) = watch::channel(false);
    let (close, closed) = watch::channel(false);
    let upkeep = tokio::spawn(upkeep(state.clone(), stopped.clone()));
    let sender = state
        .webhook_sender(Arc::new(DirectTransport::system()))
        .context("cannot set up the webhook sender")?;
    let background = [
        tokio::spawn(kage_api::run_worker(
            state.clone(),
            wait_for(stopped.clone()),
        )),
        tokio::spawn(sender.run(wait_for(stopped.clone()))),
        tokio::spawn(run_job_retention(
            state.clone(),
            config.artifacts.sweep_interval.get(),
            stopped.clone(),
        )),
    ];
    let settings = ServeSettings::from_config(&config.server);
    let metrics_server = metrics_listener.map(|listener| {
        let router = kage_api::metrics_router(state.clone());
        tokio::spawn(serve(listener, router, settings, wait_for(closed.clone())))
    });
    let mut api = tokio::spawn(serve(
        listener,
        kage_api::router(state.clone()),
        settings,
        wait_for(closed),
    ));

    let mut stopped_for = None;
    tokio::select! {
        () = shutdown_signal() => {}
        // Another server holds the database now, or may soon: this one
        // stops as at a signal, and says why (F174, lock-02).
        why = lost_lock(lock_lost) => stopped_for = Some(why),
        // The listener gave up by itself: nothing is left to wait for.
        ended = &mut api => {
            let _ = stop.send(true);
            let _ = close.send(true);
            upkeep.abort();
            for task in background {
                let _ = task.await;
            }
            ended.context("the API server task failed")??;
            return Ok(None);
        }
    }
    // Readiness says so at once, and the listener stays to say it: a load
    // balancer is told `503 stopping`, never a reset connection. The
    // scheduler grants nothing more from here on: what waited for a
    // persona is told `shutting_down`, a job's page is given back.
    let signalled = tokio::time::Instant::now();
    // A server that may no longer hold the lock wrote its last to the
    // database when its keeper decided so (the keeper fenced the store; a
    // keeper that ended without a word is taken at its word here). It cuts
    // the pages under way at once: what it would finish in
    // `jobs.wind_down` could still run when Postgres gives the lock to
    // another server, which claims the same items once their leases run
    // out (lock-02, QA round four lock-01).
    if stopped_for.is_some() {
        store.fence();
        state.stop_at_once();
    }
    // Whatever runs on when a fenced stop runs out of time is cut.
    let mut cut = vec![api.abort_handle(), upkeep.abort_handle()];
    cut.extend(background.iter().map(tokio::task::JoinHandle::abort_handle));
    cut.extend(
        metrics_server
            .iter()
            .map(tokio::task::JoinHandle::abort_handle),
    );
    let stopping = async {
        state.stop();
        if let Some((engine, _)) = engine {
            engine.quiesce();
        }
        let _ = stop.send(true);
        tracing::info!(
            "the server stops: /readyz answers 503 until the pages that are being read and \
             the requests in flight have ended"
        );
        // Before the engine stops: the worker waits for the pages that are
        // being read (`jobs.wind_down`), writes down what it has to cut,
        // and gives its jobs back; the sender its deliveries.
        for task in background {
            let _ = task.await;
        }
        // The requests with a key that are being answered, for at most
        // `server.shutdown_grace` from the signal; then the listeners
        // close, and what is still open gets that grace once more.
        state
            .settle(signalled + config.server.shutdown_grace.get())
            .await;
        let _ = close.send(true);
        let served = api.await.context("the API server task failed")?;
        if let Some(server) = metrics_server {
            let _ = server.await;
        }
        upkeep.abort();
        served
    };
    if stopped_for.is_none() {
        stopping.await?;
        return Ok(None);
    }
    // A fenced server's work fails at its first write: what is left of it
    // ends at once, and the server exits within a bound (lock-01).
    if tokio::time::timeout(FENCED_STOP_WITHIN, stopping)
        .await
        .is_err()
    {
        tracing::warn!(
            within_s = FENCED_STOP_WITHIN.as_secs(),
            "what ran when the server stopped for its lock had not ended in time: it is cut"
        );
        for handle in cut {
            handle.abort();
        }
    }
    Ok(stopped_for)
}

/// Resolves with why the lock was lost, once the keeper says so; a keeper
/// that ended without saying confirms nothing either.
async fn lost_lock(mut told: watch::Receiver<Option<LockLoss>>) -> LockLoss {
    let unknown = LockLoss::Unconfirmed {
        after: Duration::ZERO,
    };
    match told.wait_for(Option::is_some).await {
        Ok(why) => why.unwrap_or(unknown),
        Err(_) => unknown,
    }
}

/// Resolves when `stopped` says so, or when its sender is gone.
async fn wait_for(mut stopped: watch::Receiver<bool>) {
    let _ = stopped.wait_for(|stop| *stop).await;
}

/// The API's housekeeping while the server runs.
async fn upkeep(state: ApiState, stopped: watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(UPKEEP_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let stop = wait_for(stopped);
    tokio::pin!(stop);
    loop {
        tokio::select! {
            () = &mut stop => break,
            _ = ticker.tick() => state.upkeep(),
        }
    }
}

/// Binds the listening socket.
async fn listen(bind: SocketAddr) -> anyhow::Result<TcpListener> {
    TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))
}

/// Serves `router` on `listener` until `shutdown` resolves, then drains
/// in-flight requests.
pub(crate) async fn serve(
    listener: TcpListener,
    router: Router,
    settings: ServeSettings,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let local_addr = listener
        .local_addr()
        .context("failed to read the listener address")?;
    tracing::info!(%local_addr, "kage listening");

    kage_api::serve::serve(listener, router, settings, shutdown)
        .await
        .context("server terminated with an error")?;

    tracing::info!(%local_addr, "kage stopped");
    Ok(())
}

/// Starts the proxy pool's two loops (spec §7): upkeep (sync with the
/// provider, backoffs, replacements) and health checks, the fetch engine
/// (spec §3.2, §11: the forwarder every persona's traffic leaves through,
/// the scheduler, the bandwidth budget), and the persona manager that
/// follows the pool (spec §6): it generates a persona for every usable
/// proxy, retires it with its proxy, and runs warm-up visits and scheduled
/// self-tests, each of which asks the scheduler first. A health probe
/// presents as the persona behind the proxy it checks. Without a provider
/// nothing is started, and every fetch will fail: under `egress.mode:
/// proxies` Kage never connects to a target from the host's own address.
/// Under `direct` see [`start_direct`].
async fn start_proxy_pool(
    config: &Config,
    store: &Store,
    alerts: Arc<dyn AlertSink>,
    policies: Arc<PolicyStore>,
) -> anyhow::Result<Started> {
    let pool = Arc::new(
        Pool::from_config(config, store.clone(), Arc::clone(&alerts))
            .context("cannot set up the proxy pool")?,
    );
    if config.egress.mode == EgressMode::Direct {
        return start_direct(config, store, alerts, policies, pool).await;
    }
    // Under the pool none of the host's own rows may live: their personas
    // retire with them (spec §25, never both).
    let retired = pool
        .host_rows()
        .await
        .context("cannot read the host's rows")?;
    if !retired.is_empty() {
        pool.keep_host_rows(0, None)
            .await
            .context("cannot retire the host's rows")?;
        tracing::warn!(
            rows = retired.len(),
            "egress.mode is proxies: the host's own rows and their personas were retired"
        );
    }
    warn_of_plain_exits(config);
    if pool.provider_kind().is_none() {
        let stored = pool
            .live()
            .await
            .context("cannot read the proxy pool")?
            .len();
        if stored == 0 {
            tracing::warn!(
                "no proxy provider is configured (KAGE__WEBSHARE__API_KEY or \
                 KAGE__PROXIES__STATIC_LIST): the proxy pool is empty and nothing can be fetched"
            );
            return Ok(Started {
                tasks: Vec::new(),
                engine: None,
            });
        }
        // Proxies of an earlier provider are still in the pool and may be
        // used, so they are still checked: a leak must not go unseen because
        // a key went missing.
        tracing::warn!(
            stored,
            "no proxy provider is configured: the stored proxies are still health-checked, but \
             the pool is not synced with a provider and nothing is replaced"
        );
    }
    if pool
        .egress_unsafe()
        .await
        .context("cannot read the egress flag")?
    {
        tracing::error!(
            "egress is unsafe: a proxy leaked the host address. Nothing may be fetched until a \
             probe shows a distinct exit address or `kage proxy egress --clear` is run"
        );
    }
    let mut engine = Engine::start(
        config,
        store,
        Arc::clone(&pool),
        alerts,
        policies,
        config.forwarder.bind,
    )
    .await?;
    engine.serve();
    let personas = Arc::clone(&engine.personas);
    // Before anything is probed: which persona lives behind which proxy.
    let stored = personas
        .load()
        .await
        .context("cannot read the stored personas")?;
    tracing::info!(
        personas = stored,
        warmup = config.personas.warmup.enabled,
        scheduled_http_selftest = config.selftest.http.echo_url.is_some(),
        scheduled_browser_selftest = !config.selftest.browser.pages.is_empty(),
        "persona manager starting"
    );
    let probe_clients = PersonaProbeClients::from_config(personas.directory(), config)
        .context("cannot set up the probe clients")?;
    let checker = Arc::new(
        HealthChecker::with_clients(config, Arc::clone(&pool), probe_clients)
            .context("cannot set up the proxy health checks")?,
    );
    let tasks = vec![
        tokio::spawn(personas.run(std::future::pending())),
        tokio::spawn(pool.run(
            config.proxies.sync_interval.get(),
            POOL_TICK.min(config.proxies.sync_interval.get()),
            std::future::pending(),
        )),
        tokio::spawn(Arc::clone(&checker).run(std::future::pending())),
    ];
    Ok(Started {
        tasks,
        engine: Some((engine, checker)),
    })
}

/// Warns at start about the declared list's entries without a country
/// (ADR 0013): they get no persona and serve as plain exits only.
fn warn_of_plain_exits(config: &Config) {
    let plain = kage_proxy::plain_exit_entries(config);
    if !plain.is_empty() {
        tracing::warn!(
            entries = ?plain,
            "entries of the declared proxy list name no country: they get no persona and serve as \
             plain exits (level 0) only"
        );
    }
}

/// Starts what a host that leaves from its own connection runs
/// (`egress.mode: direct`, spec §25, ADR 0012): its rows
/// (`egress.direct.personas`), their place from the host's own geo lookup
/// or the operator's declaration, the engine, and the persona manager,
/// which generates, warms up and self-tests their personas like any. The
/// pool is neither synced, probed nor used: nothing leaves through it.
/// Without a declared country and without a lookup that answers there is
/// no host persona yet, and level 0 works all the same; the lookup is
/// tried again once per `proxies.health.geo_interval`.
async fn start_direct(
    config: &Config,
    store: &Store,
    alerts: Arc<dyn AlertSink>,
    policies: Arc<PolicyStore>,
    pool: Arc<Pool>,
) -> anyhow::Result<Started> {
    if pool.provider_kind().is_some() {
        tracing::warn!(
            "egress.mode is direct and a proxy provider is configured: the pool is idle, \
             neither synced, probed nor used"
        );
    }
    let health = &config.proxies.health;
    // Never the provider's echo: under direct the pool is idle (ADR 0013).
    let host = Arc::new(
        HostIp::new(Endpoints::from_config(config).host_ip, health.timeout.get())
            .context("cannot set up the host's own lookup")?,
    );
    let direct = config.egress.direct.clone();
    let geo_url = health.geo_url.clone();
    place_host(&pool, &host, &direct, geo_url.as_deref()).await?;

    let mut engine = Engine::start(
        config,
        store,
        Arc::clone(&pool),
        alerts,
        policies,
        config.forwarder.bind,
    )
    .await?;
    engine.serve();
    let personas = Arc::clone(&engine.personas);
    let stored = personas
        .load()
        .await
        .context("cannot read the stored personas")?;
    tracing::info!(
        personas = stored,
        host_personas = direct.personas,
        declared_country = direct.country.map(|country| country.to_string()),
        "egress.mode is direct: the host's own connection, no pool"
    );
    let probe_clients = PersonaProbeClients::from_config(personas.directory(), config)
        .context("cannot set up the probe clients")?;
    // Built for the admin API; never run, since nothing of the pool is
    // probed under direct.
    let checker = Arc::new(
        HealthChecker::with_clients(config, Arc::clone(&pool), probe_clients)
            .context("cannot set up the proxy health checks")?,
    );
    let every = health.geo_interval.get();
    let tasks = vec![
        tokio::spawn(personas.run(std::future::pending())),
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(every);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            // The first tick is now: the place was looked up at start.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if let Err(error) = place_host(&pool, &host, &direct, geo_url.as_deref()).await {
                    tracing::warn!(%error, "the host's place could not be recorded");
                }
            }
        }),
    ];
    Ok(Started {
        tasks,
        engine: Some((engine, checker)),
    })
}

/// Looks the host's own address and place up, directly, keeps its rows,
/// and records the place on each (spec §25). A declared country makes the
/// rows without a lookup and is then only verified by it.
///
/// # Errors
///
/// Fails when the store fails. A lookup that does not answer is logged.
async fn place_host(
    pool: &Pool,
    host: &HostIp,
    direct: &DirectEgressConfig,
    geo_url: Option<&str>,
) -> anyhow::Result<()> {
    let now = pool.clock().now();
    let looked = match geo_url {
        Some(url) => match host.refresh(now).await {
            Ok(ip) => host.lookup(url).await.map(|lookup| (ip, lookup)),
            Err(error) => Err(error),
        }
        .map_err(|error| error.to_string()),
        None => Err("proxies.health.geo_url is not set".to_owned()),
    };
    if direct.country.is_none()
        && let Err(error) = &looked
    {
        tracing::error!(
            %error,
            "egress.mode is direct and the host's place could not be looked up: no host persona \
             until it can, or until egress.direct.country declares it; level 0 works all the same"
        );
        return Ok(());
    }
    // The declaration is written on the rows, as a declared list's entry
    // is: one rule for every declaration (ADR 0013).
    let declared = direct.country.map(|country| Declaration {
        country,
        locale: direct.locale.clone(),
        timezone: direct.timezone.clone(),
    });
    let rows = pool
        .keep_host_rows(direct.personas, declared.as_ref())
        .await
        .context("cannot keep the host's rows")?;
    match looked {
        Ok((ip, lookup)) => {
            for row in &rows {
                pool.record_host_place(row.id, ip, lookup.clone(), now)
                    .await
                    .context("cannot record the host's place")?;
            }
        }
        Err(error) => tracing::warn!(
            %error,
            "the host's place could not be looked up: the declared egress.direct.country stands \
             unverified"
        ),
    }
    Ok(())
}

/// The artifact retention job: one sweep at startup, then one per interval.
async fn run_retention(artifacts: ArtifactStore, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let now = Utc::now();
        if let Err(error) = artifacts.sweep(now).await {
            tracing::error!(%error, "artifact retention sweep failed");
        }
        if let Err(error) = artifacts.sweep_orphans(now - ORPHAN_GRACE).await {
            tracing::error!(%error, "artifact orphan sweep failed");
        }
    }
}

/// The retention of jobs and results (spec §16.6, §18): one sweep at
/// startup, then one per interval, until the server stops. What it deletes
/// leaves its artifacts' blobs to the orphan sweep.
async fn run_job_retention(state: ApiState, interval: Duration, stopped: watch::Receiver<bool>) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let stop = wait_for(stopped);
    tokio::pin!(stop);
    loop {
        tokio::select! {
            () = &mut stop => break,
            _ = ticker.tick() => {}
        }
        match kage_api::jobs::sweep(&state, Utc::now()).await {
            Ok((0, 0)) => {}
            Ok((jobs, requests)) => {
                tracing::info!(
                    jobs,
                    requests,
                    "jobs and results past their retention are deleted"
                );
            }
            Err(error) => tracing::error!(%error, "the retention sweep of jobs failed"),
        }
    }
}

/// Resolves on the first SIGINT (Ctrl-C) or, on Unix, SIGTERM. A handler that
/// cannot be installed is logged and then ignored, so it never triggers a
/// shutdown by itself.
pub(crate) async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "cannot listen for SIGINT");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => {
                tracing::warn!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => tracing::info!("received SIGINT, shutting down"),
        () = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::net::SocketAddr;

    use axum::routing::get;
    use axum::{Json, Router};
    use kage_api::serve::ServeSettings;
    use kage_core::config::ServerConfig;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

    /// A router that answers the liveness probe and nothing else: what a
    /// test of the listener needs, without a database behind it.
    pub(crate) fn liveness_only() -> Router {
        Router::new().route(
            kage_api::HEALTHZ_PATH,
            get(|| async { Json(serde_json::json!({ "status": "ok" })) }),
        )
    }

    /// A server on an ephemeral loopback port, stopped with [`RunningServer::stop`].
    pub(crate) struct RunningServer {
        pub(crate) addr: SocketAddr,
        shutdown: oneshot::Sender<()>,
        task: JoinHandle<anyhow::Result<()>>,
    }

    impl RunningServer {
        pub(crate) async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (shutdown, stopped) = oneshot::channel::<()>();
            let task = tokio::spawn(super::serve(
                listener,
                liveness_only(),
                ServeSettings::from_config(&ServerConfig::default()),
                async {
                    // A dropped sender also ends the server, so a failed test cannot leak it.
                    let _ = stopped.await;
                },
            ));
            Self {
                addr,
                shutdown,
                task,
            }
        }

        /// Triggers graceful shutdown and returns what `serve` returned.
        pub(crate) async fn stop(self) -> anyhow::Result<()> {
            let _ = self.shutdown.send(());
            self.task.await.unwrap()
        }
    }
}

#[cfg(test)]
mod tests {
    use kage_api::serve::ServeSettings;
    use kage_core::config::ServerConfig;
    use tokio::net::TcpListener;

    use super::test_support::{RunningServer, liveness_only};
    use super::{listen, serve};

    #[tokio::test]
    async fn bind_failure_names_the_address() {
        let server = RunningServer::start().await;
        let taken = server.addr;

        let error = listen(taken).await.unwrap_err();

        assert!(
            format!("{error:#}").contains(&taken.to_string()),
            "{error:#}"
        );
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn serves_until_shutdown_then_returns_ok() {
        let server = RunningServer::start().await;

        crate::healthcheck::run(server.addr).await.unwrap();

        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn a_shutdown_that_already_fired_stops_the_server_at_once() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();

        serve(
            listener,
            liveness_only(),
            ServeSettings::from_config(&ServerConfig::default()),
            async {},
        )
        .await
        .unwrap();
    }
}

/// The server lock's keeper (QA round three's lock-02, QA round four
/// lock-01 and lock-04), with locks that answer as a test says, on a clock
/// the test moves.
#[cfg(test)]
mod lock_tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use kage_store::StoreError;
    use tokio::sync::watch;

    use super::{
        HeldLock, LOCK_UNCONFIRMED_LIMIT, LockLoss, LockUnconfirmed, OwnSessions, keep_the_lock,
        lost_lock,
    };

    /// How a fake lock answers one ask.
    #[derive(Clone, Copy, Debug)]
    enum Answer {
        /// It holds the lock (a look), or took it again (a take).
        Held,
        /// It never answers: a network that drops every packet.
        Silent,
        /// The session is gone (a look), or another server holds the lock
        /// (a take).
        Taken,
        /// Its session answers that it does not hold the lock (a look).
        NotHeld,
    }

    /// What the keeper asked of the lock, in order.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Ask {
        Look,
        Take,
        Fence,
    }

    /// A lock that answers each ask as its script says, the last answer
    /// for every ask after the script.
    struct Fake {
        script: Vec<Answer>,
        asks: Arc<Mutex<Vec<Ask>>>,
    }

    impl Fake {
        fn ask(&self, ask: Ask) -> Answer {
            let mut asks = self.asks.lock().unwrap();
            let at = asks.iter().filter(|done| **done != Ask::Fence).count();
            asks.push(ask);
            self.script[at.min(self.script.len() - 1)]
        }
    }

    impl HeldLock for Fake {
        async fn held(&mut self) -> Result<bool, StoreError> {
            match self.ask(Ask::Look) {
                Answer::Held => Ok(true),
                Answer::NotHeld => Ok(false),
                Answer::Silent => std::future::pending().await,
                Answer::Taken => Err(StoreError::Database(sqlx::Error::PoolClosed)),
            }
        }

        async fn take_again(&mut self) -> Result<bool, StoreError> {
            match self.ask(Ask::Take) {
                Answer::Held => Ok(true),
                Answer::Silent => std::future::pending().await,
                Answer::Taken | Answer::NotHeld => Ok(false),
            }
        }

        fn fence(&self) {
            self.asks.lock().unwrap().push(Ask::Fence);
        }
    }

    /// What a keeper did in a run.
    struct Kept {
        /// When it said the lock was lost and why, and what it had asked
        /// by then.
        lost: Option<(Duration, LockLoss, Vec<Ask>)>,
        asks: Vec<Ask>,
    }

    /// Runs a keeper for `for_`.
    async fn kept(script: &[Answer], for_: Duration) -> Kept {
        let asks = Arc::new(Mutex::new(Vec::new()));
        let (lost, told) = watch::channel(None);
        let started = tokio::time::Instant::now();
        let keeper = tokio::spawn(keep_the_lock(
            Fake {
                script: script.to_vec(),
                asks: Arc::clone(&asks),
            },
            lost,
        ));
        let lost = tokio::time::timeout(for_, lost_lock(told))
            .await
            .ok()
            .map(|loss| (started.elapsed(), loss, asks.lock().unwrap().clone()));
        keeper.abort();
        let asks = asks.lock().unwrap().clone();
        Kept { lost, asks }
    }

    #[tokio::test(start_paused = true)]
    async fn a_lock_that_answers_is_kept() {
        let kept = kept(&[Answer::Held], Duration::from_mins(10)).await;
        assert!(kept.lost.is_none());
        assert!(kept.asks.len() >= 39, "{:?}", kept.asks);
        assert!(kept.asks.iter().all(|ask| *ask == Ask::Look));
    }

    /// A session cut off by the network never answers: the server decides
    /// to stop, saying it could not confirm the lock, at the first look at
    /// or past the limit that fails too, before Postgres gives the lock up
    /// (a minute after the last answer). The store is fenced before the
    /// server hears of it (QA round four lock-01).
    #[tokio::test(start_paused = true)]
    async fn a_lock_that_cannot_be_confirmed_fences_the_store_and_stops_the_server_in_time() {
        let kept = kept(&[Answer::Silent], Duration::from_mins(10)).await;
        let (after, why, asks) = kept.lost.unwrap();
        assert_eq!(
            why,
            LockLoss::Unconfirmed {
                after: Duration::from_secs(50)
            }
        );
        assert!(after >= LOCK_UNCONFIRMED_LIMIT, "{after:?}");
        assert!(after <= Duration::from_secs(50), "{after:?}");
        // A look at 15 s that timed out, then the lock taken again on a
        // new connection at 30 and 45 s, never queued behind the look; the
        // fence came before the word.
        assert_eq!(asks, [Ask::Look, Ask::Take, Ask::Take, Ask::Fence]);
    }

    /// QA round four lock-04: a 28 s cut over one look. The look into the
    /// cut and the next take time out; once the cut heals, the next look
    /// takes the lock again on a new connection (its own session, held up
    /// behind the look the network kept, is ended there) and confirms it.
    /// Before, the looks after the heal queued behind the one sent into
    /// the cut, and the server stopped 7.7 s after the heal.
    #[tokio::test(start_paused = true)]
    async fn a_short_cut_over_one_look_keeps_the_lock_once_it_heals() {
        let kept = kept(
            &[Answer::Silent, Answer::Silent, Answer::Held, Answer::Held],
            Duration::from_mins(10),
        )
        .await;
        assert!(kept.lost.is_none(), "{:?}", kept.lost.map(|lost| lost.1));
        assert_eq!(
            &kept.asks[..4],
            [Ask::Look, Ask::Take, Ask::Take, Ask::Look]
        );
        assert!(!kept.asks.contains(&Ask::Fence));
    }

    /// A session that answers it does not hold the lock, or is gone, is
    /// replaced within the same look.
    #[tokio::test(start_paused = true)]
    async fn a_lock_whose_session_lost_it_is_taken_again_within_the_look() {
        for first in [Answer::NotHeld, Answer::Taken] {
            let kept = kept(&[first, Answer::Held], Duration::from_mins(2)).await;
            assert!(kept.lost.is_none(), "{first:?}");
            assert_eq!(
                &kept.asks[..3],
                [Ask::Look, Ask::Take, Ask::Look],
                "{first:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_lock_another_server_took_fences_the_store_and_stops_the_server_at_the_next_look() {
        let kept = kept(&[Answer::Taken], Duration::from_mins(10)).await;
        let (after, why, asks) = kept.lost.unwrap();
        assert_eq!((after, why), (Duration::from_secs(15), LockLoss::Taken));
        assert_eq!(asks, [Ask::Look, Ask::Take, Ask::Fence]);
    }

    /// A keeper that ended without a word confirms nothing either.
    #[tokio::test]
    async fn a_keeper_that_ended_without_a_word_is_a_lock_not_confirmed() {
        let (lost, told) = watch::channel(None);
        drop(lost);
        assert_eq!(
            lost_lock(told).await,
            LockLoss::Unconfirmed {
                after: Duration::ZERO
            }
        );
    }

    /// The exit message says what happened, and only that (QA round four
    /// lock-01): when the server stopped writing, and what became of its
    /// sessions.
    #[test]
    fn the_exit_message_says_when_the_server_stopped_writing_and_whether_the_lock_is_free() {
        let ended = LockUnconfirmed {
            after: Duration::from_secs(50),
            sessions: OwnSessions::Ended,
        }
        .to_string();
        assert!(
            ended.starts_with("this server could not confirm for 50 s"),
            "{ended}"
        );
        assert!(
            ended.contains("stopped writing to the database at that moment"),
            "{ended}"
        );
        assert!(ended.contains("the lock is free now"), "{ended}");
        assert!(!ended.contains("before Postgres could give"), "{ended}");
        let unreachable = LockUnconfirmed {
            after: Duration::ZERO,
            sessions: OwnSessions::Unreachable,
        }
        .to_string();
        assert!(
            unreachable.starts_with("this server could not confirm that it holds"),
            "{unreachable}"
        );
        assert!(
            unreachable.contains("refused as if another one ran"),
            "{unreachable}"
        );
        assert!(unreachable.ends_with("(exit code 75)"), "{unreachable}");
    }
}
