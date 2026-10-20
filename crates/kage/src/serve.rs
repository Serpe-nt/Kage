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
use kage_store::{ArtifactStore, Store};
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

    let (alerts, recorder) = alerts::recorder(store.clone());
    let policies = Arc::new(
        PolicyStore::open(&config.policies.dir, Arc::clone(&alerts))
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

    // Before anything records: what is counted from here on is scraped.
    let metrics = match kage_api::metrics::install() {
        Ok(handle) => Some(handle),
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
        metrics,
    )
    .await;

    for task in started.tasks {
        task.abort();
    }
    // After the manager's own work stopped: what waits is refused, the
    // browsers are closed and the last byte counts are written.
    if let Some((engine, _)) = started.engine {
        engine.stop().await;
    }
    retention.abort();
    recorder.abort();
    store.close().await;
    served
}

/// Builds the API over what was started and serves it until a signal.
async fn serve_api(
    config: &Config,
    bind: Option<SocketAddr>,
    store: &Store,
    artifacts: ArtifactStore,
    policies: Arc<PolicyStore>,
    engine: Option<&(Engine, Arc<HealthChecker>)>,
    metrics: Option<MetricsHandle>,
) -> anyhow::Result<()> {
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

    // One signal stops both listeners, the upkeep and the work on jobs.
    let (stop, stopped) = watch::channel(false);
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
        tokio::spawn(serve(listener, router, settings, wait_for(stopped.clone())))
    });
    let mut api = tokio::spawn(serve(
        listener,
        kage_api::router(state.clone()),
        settings,
        wait_for(stopped),
    ));

    tokio::select! {
        () = shutdown_signal() => {}
        // The listener gave up by itself: nothing is left to wait for.
        ended = &mut api => {
            let _ = stop.send(true);
            upkeep.abort();
            for task in background {
                let _ = task.await;
            }
            return ended.context("the API server task failed")?;
        }
    }
    // Readiness says so at once; the requests in flight finish. The
    // scheduler grants nothing more from here on: what waited for a
    // persona is told `shutting_down`, a job's page is given back.
    state.stop();
    if let Some((engine, _)) = engine {
        engine.quiesce();
    }
    let _ = stop.send(true);
    let served = api.await.context("the API server task failed")?;
    if let Some(server) = metrics_server {
        let _ = server.await;
    }
    // Before the engine stops: the worker waits for the pages that are
    // being read (`jobs.wind_down`), writes down what it has to cut, and
    // gives its jobs back; the sender its deliveries.
    for task in background {
        let _ = task.await;
    }
    upkeep.abort();
    served
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
    let probe_clients =
        PersonaProbeClients::from_config(personas.directory(), &config.proxies.health)
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
    let probe_clients =
        PersonaProbeClients::from_config(personas.directory(), &config.proxies.health)
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
