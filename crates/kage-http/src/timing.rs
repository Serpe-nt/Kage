//! Where the time of a request goes: opening the tunnel, the TLS handshake,
//! waiting for the first byte, reading the body (spec §9, follow-up F2 of
//! the M2 live check).
//!
//! wreq reports none of this, and its connector does the whole connection
//! setup in one step. Two hooks it does offer are enough to split it:
//!
//! - a **connector layer** wraps every connection attempt, so its start and
//!   its end are known;
//! - the **TLS session cache** is asked for a session to resume immediately
//!   before every TLS handshake, which marks the moment the tunnel is up and
//!   the handshake begins.
//!
//! Both hooks have to find the request they are working for. A request runs
//! inside [`observe`], which puts its [`Phases`] into a task-local; the
//! connector layer picks it up when wreq asks it to connect, and keeps it
//! in a second task-local around the connection attempt, where the session
//! cache finds it. The session cache is wreq's own LRU cache behind the
//! wrapper, so session resumption behaves exactly as it does without the
//! hook.
//!
//! What the phases mean on this path:
//!
//! - **connect**: TCP to the proxy plus the `CONNECT` round trip, which
//!   includes the proxy resolving the target and dialling it. For a plain
//!   `http://` URL it is TCP to the proxy only.
//! - **tls**: the handshake with the target through the tunnel. Absent for
//!   plain HTTP.
//! - There is no **dns** phase: the proxy resolves the target. A request on
//!   a connection that is already open has neither connect nor tls.
//!
//! If a pin moves and one of the hooks is no longer called, the phases come
//! back empty and the integration tests that assert them fail.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::time::Instant;
use tower::{Layer, Service};
use wreq::tls::session::{Key, LruTlsSessionCache, TlsSession, TlsSessionCache};

/// Sessions kept per host, the default of wreq's own cache.
const SESSIONS_PER_HOST: usize = 8;

tokio::task_local! {
    /// The phases of the request this task is sending.
    static REQUEST: Arc<Phases>;
    /// The phases of the request a connection attempt belongs to.
    static CONNECTING: Arc<Phases>;
}

#[derive(Debug, Default, Clone, Copy)]
struct Marks {
    connect_started: Option<Instant>,
    tls_started: Option<Instant>,
    connected: Option<Instant>,
}

/// The instants of one request's connection setup.
#[derive(Debug, Default)]
pub struct Phases {
    marks: Mutex<Marks>,
}

/// How long the connection setup of one request took.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConnectionSetup {
    /// TCP to the proxy and the tunnel; `None` when the request used a
    /// connection that was already open.
    pub connect: Option<Duration>,
    /// The TLS handshake through the tunnel; `None` for plain HTTP and for
    /// a connection that was already open.
    pub tls: Option<Duration>,
}

impl ConnectionSetup {
    /// Whether the request opened a connection.
    pub const fn opened_connection(&self) -> bool {
        self.connect.is_some()
    }
}

impl Phases {
    /// A recorder with nothing marked.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn update(&self, change: impl FnOnce(&mut Marks)) {
        change(&mut self.marks.lock().unwrap_or_else(PoisonError::into_inner));
    }

    fn connect_started(&self) {
        // A request that is retried on a fresh connection keeps the first
        // start: the caller waited from then on.
        self.update(|marks| {
            marks.connect_started.get_or_insert_with(Instant::now);
        });
    }

    fn tls_started(&self) {
        self.update(|marks| marks.tls_started = Some(Instant::now()));
    }

    fn connected(&self) {
        self.update(|marks| marks.connected = Some(Instant::now()));
    }

    /// Whether a connection attempt has begun and not finished.
    pub fn is_connecting(&self) -> bool {
        let marks = *self.marks.lock().unwrap_or_else(PoisonError::into_inner);
        marks.connect_started.is_some() && marks.connected.is_none()
    }

    /// What was measured. Empty when the connection attempt did not finish.
    pub fn setup(&self) -> ConnectionSetup {
        let marks = *self.marks.lock().unwrap_or_else(PoisonError::into_inner);
        let (Some(started), Some(connected)) = (marks.connect_started, marks.connected) else {
            return ConnectionSetup::default();
        };
        match marks.tls_started {
            Some(tls) if tls >= started && tls <= connected => ConnectionSetup {
                connect: Some(tls - started),
                tls: Some(connected - tls),
            },
            _ => ConnectionSetup {
                connect: Some(connected.saturating_duration_since(started)),
                tls: None,
            },
        }
    }
}

/// Runs the sending of one request so that the hooks can attribute a
/// connection attempt to `phases`.
pub async fn observe<F: Future>(phases: Arc<Phases>, send: F) -> F::Output {
    REQUEST.scope(phases, send).await
}

/// The connector layer: times every connection attempt made for a request
/// that runs inside [`observe`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ConnectTimingLayer;

impl<S> Layer<S> for ConnectTimingLayer {
    type Service = ConnectTiming<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ConnectTiming { inner }
    }
}

/// The service [`ConnectTimingLayer`] wraps a connector in.
#[derive(Debug, Clone)]
pub struct ConnectTiming<S> {
    inner: S,
}

impl<S, Request> Service<Request> for ConnectTiming<S>
where
    S: Service<Request>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        // wreq asks for a connection while it is polled by the request's
        // own future, so the request's recorder is in reach here.
        let phases = REQUEST.try_with(Arc::clone).ok();
        let connecting = self.inner.call(request);
        match phases {
            None => Box::pin(connecting),
            Some(phases) => Box::pin(async move {
                phases.connect_started();
                let connection = CONNECTING.scope(Arc::clone(&phases), connecting).await;
                if connection.is_ok() {
                    phases.connected();
                }
                connection
            }),
        }
    }
}

/// wreq's own session cache, with a mark set whenever a handshake is about
/// to start.
pub struct TimedSessionCache {
    inner: LruTlsSessionCache,
}

impl std::fmt::Debug for TimedSessionCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimedSessionCache").finish_non_exhaustive()
    }
}

impl Default for TimedSessionCache {
    fn default() -> Self {
        Self {
            inner: LruTlsSessionCache::new(SESSIONS_PER_HOST),
        }
    }
}

impl TlsSessionCache for TimedSessionCache {
    fn put(&self, key: Key, session: TlsSession) {
        self.inner.put(key, session);
    }

    fn pop(&self, key: &Key) -> Option<TlsSession> {
        // wreq looks for a session to resume right before it starts the
        // handshake, inside the connection attempt.
        let _ = CONNECTING.try_with(|phases| phases.tls_started());
        self.inner.pop(key)
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::time::Duration;

    use tower::{Layer, Service, service_fn};

    use super::{CONNECTING, ConnectTimingLayer, ConnectionSetup, Phases, observe};

    /// A connector that takes `tunnel` to connect and, when `tls` is given,
    /// marks the handshake start the way the session cache does and then
    /// takes `tls` more.
    async fn connect(tunnel: Duration, tls: Option<Duration>) -> Result<&'static str, Infallible> {
        tokio::time::sleep(tunnel).await;
        if let Some(tls) = tls {
            let _ = CONNECTING.try_with(|phases| phases.tls_started());
            tokio::time::sleep(tls).await;
        }
        Ok("connection")
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_tls_connection_is_split_into_tunnel_and_handshake() {
        let phases = Phases::new();
        let mut connector = ConnectTimingLayer.layer(service_fn(|()| {
            connect(Duration::from_millis(300), Some(Duration::from_millis(120)))
        }));

        // wreq asks for the connection while the request is being sent.
        let connection = observe(Arc::clone(&phases), async { connector.call(()).await }).await;

        assert_eq!(connection, Ok("connection"));
        let setup = phases.setup();
        assert_eq!(setup.connect, Some(Duration::from_millis(300)));
        assert_eq!(setup.tls, Some(Duration::from_millis(120)));
        assert!(setup.opened_connection());
    }

    #[tokio::test(start_paused = true)]
    async fn a_plain_connection_has_no_handshake() {
        let phases = Phases::new();
        let mut connector =
            ConnectTimingLayer.layer(service_fn(|()| connect(Duration::from_millis(80), None)));

        observe(Arc::clone(&phases), async { connector.call(()).await })
            .await
            .unwrap();

        assert_eq!(
            phases.setup(),
            ConnectionSetup {
                connect: Some(Duration::from_millis(80)),
                tls: None
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_on_an_open_connection_has_no_setup_at_all() {
        let phases = Phases::new();

        // The connector is never asked.
        observe(Arc::clone(&phases), async {}).await;

        assert_eq!(phases.setup(), ConnectionSetup::default());
        assert!(!phases.setup().opened_connection());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_connection_attempt_reports_nothing() {
        let phases = Phases::new();
        let mut connector = ConnectTimingLayer.layer(service_fn(|()| async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Err::<(), &str>("refused")
        }));

        let outcome = observe(Arc::clone(&phases), async {
            let attempt = connector.call(());
            assert!(!phases.is_connecting());
            attempt.await
        })
        .await;

        assert_eq!(outcome, Err("refused"));
        assert_eq!(phases.setup(), ConnectionSetup::default());
        // It began and never finished.
        assert!(phases.is_connecting());
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_made_outside_a_request_is_left_alone() {
        let mut connector = ConnectTimingLayer.layer(service_fn(|()| {
            connect(Duration::from_millis(10), Some(Duration::from_millis(10)))
        }));

        // No recorder in reach: the connection is made all the same.
        assert_eq!(connector.call(()).await, Ok("connection"));
        assert!(futures_ready(&mut connector));
    }

    fn futures_ready<S: Service<(), Error = Infallible>>(service: &mut S) -> bool {
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        service.poll_ready(&mut context).is_ready()
    }

    #[tokio::test(start_paused = true)]
    async fn two_requests_at_once_keep_their_own_measurements() {
        let (slow, fast) = (Phases::new(), Phases::new());
        let mut connector = ConnectTimingLayer.layer(service_fn(|tunnel: Duration| {
            connect(tunnel, Some(Duration::from_millis(40)))
        }));

        // The slow connection is asked for inside the slow request, but is
        // driven to its end elsewhere, as wreq does when another connection
        // becomes free first.
        let asked = observe(Arc::clone(&slow), async {
            Some(connector.call(Duration::from_millis(900)))
        })
        .await
        .unwrap();
        let background = tokio::spawn(asked);
        observe(Arc::clone(&fast), async {
            connector.call(Duration::from_millis(100)).await
        })
        .await
        .unwrap();
        background.await.unwrap().unwrap();

        assert_eq!(fast.setup().connect, Some(Duration::from_millis(100)));
        assert_eq!(fast.setup().tls, Some(Duration::from_millis(40)));
        assert_eq!(slow.setup().connect, Some(Duration::from_millis(900)));
        assert_eq!(slow.setup().tls, Some(Duration::from_millis(40)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_attempt_keeps_the_first_start() {
        let phases = Phases::new();
        let mut connector = ConnectTimingLayer.layer(service_fn(|fail: bool| async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if fail { Err("reset") } else { Ok("connection") }
        }));

        observe(Arc::clone(&phases), async {
            assert!(connector.call(true).await.is_err());
            assert!(connector.call(false).await.is_ok());
        })
        .await;

        // The caller waited for both attempts.
        assert_eq!(phases.setup().connect, Some(Duration::from_millis(400)));
    }
}
