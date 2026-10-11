//! The listener (spec §19 "Input limits"): what `axum::serve` does, with
//! the limits it has no switch for.
//!
//! - **The head of a request** is at most `server.max_header_bytes` long
//!   and has at most `server.max_headers` headers; a longer one is
//!   answered `431` by the HTTP layer and the connection is closed.
//! - **A client that trickles its headers** (slowloris) is cut off after
//!   `server.header_read_timeout`: the deadline covers the whole head of
//!   every request, and the wait for the next one on a kept connection.
//! - **Connections** are counted: past `server.max_connections` a new one
//!   waits in the accept queue until another ends.
//! - **Stopping** is graceful: nothing new is accepted, requests in flight
//!   get `server.shutdown_grace` to finish, and what is still open then is
//!   dropped.
//!
//! The body of a request has its own limits, which the router sets:
//! `server.max_body_bytes` and `server.body_read_timeout`.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use kage_core::config::ServerConfig;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

/// How long the listener waits before it accepts again after `accept`
/// failed: out of file descriptors, most likely, which passes.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// The limits of a listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeSettings {
    /// Largest head of a request, in bytes.
    pub max_header_bytes: usize,
    /// Most headers of a request.
    pub max_headers: usize,
    /// How long the head of a request may take to arrive.
    pub header_read_timeout: Duration,
    /// Most connections served at once.
    pub max_connections: usize,
    /// How long requests in flight are waited for at a stop.
    pub shutdown_grace: Duration,
}

impl ServeSettings {
    /// The limits the `server` section of the configuration sets.
    pub fn from_config(server: &ServerConfig) -> Self {
        Self {
            max_header_bytes: usize::try_from(server.max_header_bytes).unwrap_or(usize::MAX),
            max_headers: usize::try_from(server.max_headers).unwrap_or(usize::MAX),
            header_read_timeout: server.header_read_timeout.get(),
            max_connections: usize::try_from(server.max_connections).unwrap_or(usize::MAX),
            shutdown_grace: server.shutdown_grace.get(),
        }
    }
}

/// The HTTP layer of one connection, with the limits applied.
fn connections(settings: ServeSettings) -> Builder<TokioExecutor> {
    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(settings.header_read_timeout)
        .max_headers(settings.max_headers)
        .max_buf_size(settings.max_header_bytes);
    builder
        .http2()
        .timer(TokioTimer::new())
        .max_header_list_size(u32::try_from(settings.max_header_bytes).unwrap_or(u32::MAX))
        .keep_alive_interval(Some(settings.header_read_timeout))
        .keep_alive_timeout(settings.header_read_timeout);
    builder
}

/// Serves `router` on `listener` until `shutdown` resolves, then lets the
/// requests in flight finish.
///
/// # Errors
///
/// Fails when the listener's own address cannot be read. A connection
/// that fails is that connection's end, not the server's.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    settings: ServeSettings,
    shutdown: impl Future<Output = ()> + Send,
) -> io::Result<()> {
    let local = listener.local_addr()?;
    let builder = connections(settings);
    let graceful = GracefulShutdown::new();
    let slots = Arc::new(Semaphore::new(settings.max_connections.max(1)));
    tokio::pin!(shutdown);

    loop {
        // A connection is only accepted when there is room to serve it.
        let slot = tokio::select! {
            () = &mut shutdown => break,
            slot = Arc::clone(&slots).acquire_owned() => match slot {
                Ok(slot) => slot,
                Err(_) => break,
            },
        };
        let (stream, peer) = tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!(%local, %error, "a connection could not be accepted");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    continue;
                }
            },
        };
        if let Err(error) = stream.set_nodelay(true) {
            tracing::debug!(%peer, %error, "TCP_NODELAY could not be set");
        }
        let connection = builder
            .serve_connection(
                TokioIo::new(stream),
                TowerToHyperService::new(router.clone()),
            )
            .into_owned();
        let connection = graceful.watch(connection);
        tokio::spawn(async move {
            // A client that goes away, sends garbage or runs into a limit
            // ends its own connection.
            if let Err(error) = connection.await {
                tracing::debug!(%peer, %error, "a connection ended with an error");
            }
            drop(slot);
        });
    }

    drop(listener);
    let open = graceful.count();
    if open > 0 {
        tracing::info!(%local, open, "waiting for the requests in flight");
    }
    if tokio::time::timeout(settings.shutdown_grace, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!(
            %local,
            grace = ?settings.shutdown_grace,
            "connections were still open when the grace period ended"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_core::config::ServerConfig;

    use super::ServeSettings;

    #[test]
    fn the_limits_are_the_configurations() {
        let settings = ServeSettings::from_config(&ServerConfig::default());

        assert_eq!(settings.max_header_bytes, 16 * 1024);
        assert_eq!(settings.max_headers, 64);
        assert_eq!(settings.header_read_timeout, Duration::from_secs(10));
        assert_eq!(settings.max_connections, 1024);
        assert_eq!(settings.shutdown_grace, Duration::from_secs(30));
    }
}
