//! `kage serve`: checks the database, loads the policies, starts the
//! background jobs and the proxy pool, and runs the API router until
//! shutdown.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use kage_core::alert::AlertSink;
use kage_core::config::Config;
use kage_core::policy::PolicyStore;
use kage_proxy::{HealthChecker, Pool};
use kage_store::{ArtifactStore, Store};
use tokio::net::TcpListener;
use tokio::time::MissedTickBehavior;

use crate::{alerts, migrate};

/// Blobs younger than this are never treated as orphans: a write in
/// progress has a blob on disk before its row is committed.
const ORPHAN_GRACE: Duration = Duration::from_hours(1);

/// How often the proxy pool ends backoffs that ran out and follows
/// replacements in flight.
const POOL_TICK: Duration = Duration::from_secs(10);

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

    let artifacts = ArtifactStore::new(store.pool().clone(), &config.artifacts);
    let retention = tokio::spawn(run_retention(
        artifacts,
        config.artifacts.sweep_interval.get(),
    ));

    let proxy_pool = match start_proxy_pool(&config, &store, alerts).await {
        Ok(tasks) => tasks,
        Err(error) => {
            retention.abort();
            recorder.abort();
            store.close().await;
            return Err(error);
        }
    };

    let served = match listen(bind.unwrap_or(config.server.bind)).await {
        Ok(listener) => serve(listener, shutdown_signal()).await,
        Err(error) => Err(error),
    };

    for task in proxy_pool {
        task.abort();
    }
    retention.abort();
    recorder.abort();
    store.close().await;
    served
}

/// Binds the listening socket.
async fn listen(bind: SocketAddr) -> anyhow::Result<TcpListener> {
    TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))
}

/// Serves the API on `listener` until `shutdown` resolves, then drains
/// in-flight requests.
pub(crate) async fn serve(
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let local_addr = listener
        .local_addr()
        .context("failed to read the listener address")?;
    tracing::info!(%local_addr, "kage listening");

    axum::serve(listener, kage_api::router())
        .with_graceful_shutdown(shutdown)
        .await
        .context("server terminated with an error")?;

    tracing::info!("kage stopped");
    Ok(())
}

/// Starts the proxy pool's two loops (spec §7): upkeep (sync with the
/// provider, backoffs, replacements) and health checks. Without a provider
/// nothing is started, and every fetch will fail: Kage never connects to a
/// target directly.
async fn start_proxy_pool(
    config: &Config,
    store: &Store,
    alerts: Arc<dyn AlertSink>,
) -> anyhow::Result<Vec<tokio::task::JoinHandle<()>>> {
    let pool = Arc::new(
        Pool::from_config(config, store.clone(), alerts).context("cannot set up the proxy pool")?,
    );
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
            return Ok(Vec::new());
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
    let checker = Arc::new(
        HealthChecker::from_config(config, Arc::clone(&pool))
            .context("cannot set up the proxy health checks")?,
    );
    Ok(vec![
        tokio::spawn(pool.run(
            config.proxies.sync_interval.get(),
            POOL_TICK.min(config.proxies.sync_interval.get()),
            std::future::pending(),
        )),
        tokio::spawn(checker.run(std::future::pending())),
    ])
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

    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;

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
            let task = tokio::spawn(super::serve(listener, async {
                // A dropped sender also ends the server, so a failed test cannot leak it.
                let _ = stopped.await;
            }));
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
    use tokio::net::TcpListener;

    use super::test_support::RunningServer;
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

        serve(listener, async {}).await.unwrap();
    }
}
