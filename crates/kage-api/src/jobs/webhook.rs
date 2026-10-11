//! Webhooks: a job's events, posted to the receiver the job names (spec
//! §16.6).
//!
//! # What is sent
//!
//! `job.completed` or `job.failed` when a job ends, and `job.progress`
//! whenever so many more items are over, when the job asked for that. The
//! body is thin: the event's id, its type, when it happened, and the job
//! as `GET /v1/jobs/{id}` shows it to the key that made it. No item, no
//! URL that was fetched, no persona and no proxy is in it: whoever
//! receives it reads the items with the key.
//!
//! # How it is signed
//!
//! `X-Kage-Signature: sha256=<hex>` is the HMAC-SHA-256, under the job's
//! secret, of `<timestamp>.<body>`, with the timestamp (Unix seconds) in
//! `X-Kage-Timestamp` and the body exactly as sent. `X-Kage-Event` names
//! the event and `X-Kage-Delivery` its id, which is the same on every
//! attempt, so a receiver drops a repeat. Every attempt is signed anew. A
//! receiver recomputes the signature, compares in constant time, and
//! refuses a timestamp older than a few minutes ([`verify`] is that check).
//!
//! # Where it goes, and from where
//!
//! A webhook is the one request Kage makes from its own address, so it
//! goes only to an origin the operator listed (`webhooks.allowed_origins`).
//! The sender resolves the receiver's name itself, refuses a name any of
//! whose addresses is not public (unless the operator listed the origin as
//! its own, and then still link-local, metadata and multicast addresses),
//! connects to the addresses it checked and to no other, never through a
//! proxy, and follows no redirect. All of that again on every attempt.
//!
//! # Failures
//!
//! Anything but a `2xx` is a failure and is tried again, up to
//! `webhooks.max_retries` times, after 30 s, 2 min, 8 min, 32 min and
//! 2 h 8 min, each moved by up to a fifth either way by the event's own
//! seed; a `Retry-After` is honoured up to an hour; `410` ends the
//! retries. One delivery is in flight per receiver, and while a receiver's
//! last attempt failed its other events wait behind the next probe without
//! spending their own attempts: a receiver that is down gets one request
//! per step, not one per job. Every attempt is a row of the delivery log,
//! and what waits survives a restart.

use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use kage_core::config::WebhooksConfig;
use kage_core::domain::{DeliveryState, Job, WebhookAttempt, WebhookDelivery, WebhookEvent};
use kage_core::ids::{JobId, WebhookDeliveryId};
use kage_core::seed::Seed;
use kage_core::ssrf::{DeniedRange, classify_ip};
use kage_http::HopResolver;
use kage_proxy::{Clock, CredentialCipher};
use kage_store::Store;
use kage_store::repo::AttemptEnd;
use serde::Serialize;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use url::{Host, Url};

use super::view::JobView;
use crate::resolve::HostResolver;

/// The header that carries the signature.
pub const SIGNATURE_HEADER: &str = "X-Kage-Signature";

/// The header that carries the time the attempt was signed at.
pub const TIMESTAMP_HEADER: &str = "X-Kage-Timestamp";

/// The header that names the event.
pub const EVENT_HEADER: &str = "X-Kage-Event";

/// The header that carries the event's id.
pub const DELIVERY_HEADER: &str = "X-Kage-Delivery";

/// What Kage calls itself to a receiver: it is no browser and does not
/// pretend to be one.
const USER_AGENT: &str = concat!("kage/", env!("CARGO_PKG_VERSION"));

/// How long the sender's claim on a delivery holds beyond the attempt's
/// own time: a delivery whose attempt died with the process is due again
/// after it.
const LEASE_SLACK: Duration = Duration::from_secs(30);

/// What the associated data of a sealed webhook secret starts with.
const SECRET_BINDING: &[u8] = b"kage:webhook-secret:v1:";

/// What a job's webhook secret is bound to when it is sealed: the job.
pub(crate) fn secret_binding(job: JobId) -> Vec<u8> {
    let mut binding = SECRET_BINDING.to_vec();
    binding.extend_from_slice(job.as_uuid().as_bytes());
    binding
}

/// The signature of one attempt: `sha256=` and the hexadecimal
/// HMAC-SHA-256 of `<timestamp>.<body>` under the secret.
pub fn signature(secret: &[u8], timestamp: i64, body: &str) -> String {
    let mut text = String::from("sha256=");
    // HMAC takes a key of any length.
    if let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) {
        mac.update(timestamp.to_string().as_bytes());
        mac.update(b".");
        mac.update(body.as_bytes());
        for byte in mac.finalize().into_bytes() {
            // Writing to a String cannot fail.
            let _ = write!(text, "{byte:02x}");
        }
    }
    text
}

/// What a receiver does with a delivery: recomputes the signature over
/// the timestamp it was sent and the body it received, compares in
/// constant time, and refuses a timestamp further than `tolerance` from
/// its own clock.
pub fn verify(
    secret: &[u8],
    timestamp: &str,
    body: &str,
    signed: &str,
    now: DateTime<Utc>,
    tolerance: Duration,
) -> bool {
    let Ok(at) = timestamp.parse::<i64>() else {
        return false;
    };
    let age = now.timestamp().abs_diff(at);
    if age > tolerance.as_secs() {
        return false;
    }
    let expected = signature(secret, at, body);
    expected.as_bytes().ct_eq(signed.as_bytes()).into()
}

/// The body of an event.
#[derive(Debug, Clone, Serialize)]
struct Payload<'a> {
    id: WebhookDeliveryId,
    #[serde(rename = "type")]
    kind: &'static str,
    created_at: DateTime<Utc>,
    job: &'a JobView,
}

/// Which event the end of a job is.
pub(crate) const fn end_event(job: &Job) -> WebhookEvent {
    match job.state {
        kage_core::domain::JobState::Failed => WebhookEvent::JobFailed,
        _ => WebhookEvent::JobCompleted,
    }
}

/// Queues an event of a job for delivery, when the job has a webhook.
/// Returns whether one was queued.
///
/// # Errors
///
/// Fails when the store fails.
pub(crate) async fn enqueue(
    store: &Store,
    job: &Job,
    event: WebhookEvent,
    at: DateTime<Utc>,
) -> Result<bool, kage_store::StoreError> {
    let Some(url) = job
        .webhook_url
        .as_deref()
        .and_then(|url| Url::parse(url).ok())
    else {
        return Ok(false);
    };
    let Some(origin) = kage_core::config::origin_of(&url) else {
        return Ok(false);
    };
    let id = WebhookDeliveryId::new();
    let view = JobView::of(job, &[]);
    let at = crate::stored(at);
    let body = serde_json::to_string(&Payload {
        id,
        kind: event.as_str(),
        created_at: at,
        job: &view,
    })
    .unwrap_or_default();
    store
        .webhooks()
        .enqueue(&WebhookDelivery {
            id,
            job_id: job.id,
            event,
            origin,
            body,
            state: DeliveryState::Pending,
            attempts: 0,
            next_attempt_at: Some(at),
            created_at: at,
            finished_at: None,
        })
        .await?;
    Ok(true)
}

/// One request to a receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    /// The receiver.
    pub url: Url,
    /// Whether the operator listed the receiver as one on its own network.
    pub private: bool,
    /// The headers, the signature among them.
    pub headers: Vec<(&'static str, String)>,
    /// The body, exactly as it was signed.
    pub body: String,
    /// How long the attempt may take.
    pub timeout: Duration,
}

/// What a receiver answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    /// The status.
    pub status: u16,
    /// What its `Retry-After` asked for, in seconds.
    pub retry_after: Option<Duration>,
}

/// Why a receiver did not answer. The names are what the delivery log
/// says; none carries an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// The receiver's name resolves to nothing.
    #[error("unresolved")]
    Unresolved,
    /// The receiver's name resolves to an address Kage does not post to.
    #[error("address_denied")]
    AddressDenied,
    /// No connection could be made.
    #[error("connect")]
    Connect,
    /// The receiver took too long.
    #[error("timeout")]
    Timeout,
    /// The request failed after it was sent.
    #[error("transport")]
    Transport,
}

/// Posts one event to one receiver.
#[async_trait]
pub trait Transport: Send + Sync + fmt::Debug {
    /// Makes the request and says what the receiver answered.
    async fn post(&self, outgoing: &Outgoing) -> Result<Answer, SendError>;
}

/// Whether Kage posts to an address: a public one always; a private or
/// loopback one only for a receiver the operator listed as its own; never
/// a link-local, metadata, multicast or otherwise special one.
pub fn postable(address: IpAddr, private: bool) -> bool {
    match classify_ip(address) {
        None => true,
        Some(DeniedRange::Loopback | DeniedRange::Private | DeniedRange::UniqueLocal) => private,
        Some(_) => false,
    }
}

/// The real transport: plain wreq without emulation, never through a
/// proxy, to addresses it resolved and checked itself.
#[derive(Debug, Clone)]
pub struct DirectTransport {
    resolver: HostResolver,
}

impl DirectTransport {
    /// A transport that looks names up with the system's resolver.
    pub const fn system() -> Self {
        Self {
            resolver: HostResolver::system(),
        }
    }

    /// A transport that asks `resolver` instead: for tests.
    pub const fn over(resolver: HostResolver) -> Self {
        Self { resolver }
    }

    /// The addresses of a receiver, every one of them checked.
    async fn addresses(&self, url: &Url, private: bool) -> Result<Vec<SocketAddr>, SendError> {
        let port = url.port_or_known_default().ok_or(SendError::Unresolved)?;
        let addresses: Vec<IpAddr> = match url.host() {
            Some(Host::Ipv4(address)) => vec![IpAddr::V4(address)],
            Some(Host::Ipv6(address)) => vec![IpAddr::V6(address)],
            Some(Host::Domain(name)) => self
                .resolver
                .resolve(name)
                .await
                .map_err(|_| SendError::Unresolved)?,
            None => return Err(SendError::Unresolved),
        };
        if addresses.is_empty() {
            return Err(SendError::Unresolved);
        }
        // One address that is not postable refuses the name: the
        // connection may go to any of them.
        if !addresses.iter().all(|address| postable(*address, private)) {
            return Err(SendError::AddressDenied);
        }
        Ok(addresses
            .into_iter()
            .map(|address| SocketAddr::new(address, port))
            .collect())
    }
}

#[async_trait]
impl Transport for DirectTransport {
    async fn post(&self, outgoing: &Outgoing) -> Result<Answer, SendError> {
        let addresses = self.addresses(&outgoing.url, outgoing.private).await?;
        let mut builder = wreq::Client::builder()
            // From this host's own address, never through a proxy, and
            // never through one named in the environment.
            .no_proxy()
            // A redirect is a failure: its target was checked by nobody.
            .redirect(wreq::redirect::Policy::none())
            .user_agent(USER_AGENT)
            .connect_timeout(outgoing.timeout)
            .timeout(outgoing.timeout);
        // The connection goes to an address that was checked a moment
        // ago, and to no other: nothing is looked up a second time.
        if let Some(Host::Domain(name)) = outgoing.url.host() {
            builder = builder.resolve_to_addrs(name.to_owned(), addresses);
        }
        let client = builder.build().map_err(|_| SendError::Transport)?;
        let mut request = client
            .post(outgoing.url.as_str())
            .header("content-type", "application/json")
            .body(outgoing.body.clone());
        for (name, value) in &outgoing.headers {
            request = request.header(*name, value.as_str());
        }
        let response = request.send().await.map_err(|error| {
            if error.is_timeout() {
                SendError::Timeout
            } else if error.is_connect() {
                SendError::Connect
            } else {
                SendError::Transport
            }
        })?;
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        Ok(Answer {
            status: response.status().as_u16(),
            retry_after,
        })
    }
}

/// How a receiver has been answering.
#[derive(Debug, Clone, Copy, Default)]
struct Receiver {
    in_flight: bool,
    /// Failures in a row.
    failures: u32,
    /// Left alone until then, after a failure.
    blocked_until: Option<DateTime<Utc>>,
}

/// What an attempt came to, for the sender's own bookkeeping.
struct Sent {
    origin: String,
    delivered: bool,
    next: Option<DateTime<Utc>>,
}

/// Sends what is due. One per server.
pub struct WebhookSender {
    store: Store,
    config: WebhooksConfig,
    cipher: CredentialCipher,
    clock: Arc<dyn Clock>,
    transport: Arc<dyn Transport>,
    receivers: Mutex<HashMap<String, Receiver>>,
    wake: Notify,
}

impl fmt::Debug for WebhookSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebhookSender")
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}

/// The wait before retry `retry` of one event: the configured backoff,
/// moved by up to `jitter` either way by a draw that is the event's own,
/// so that events that failed together do not come back together, and
/// never shorter than what the receiver asked for (up to
/// `max_retry_after`).
pub fn retry_wait(
    config: &WebhooksConfig,
    delivery: WebhookDeliveryId,
    retry: u32,
    retry_after: Option<Duration>,
) -> Duration {
    let seed = Seed::derive(
        "webhook-retry",
        &[delivery.as_uuid().as_bytes(), &retry.to_le_bytes()],
    );
    let moved = seed.rng().uniform(1.0 - config.jitter, 1.0 + config.jitter);
    let backoff = config.backoff(retry).mul_f64(moved);
    let asked = retry_after.map_or(Duration::ZERO, |asked| {
        asked.min(config.max_retry_after.get())
    });
    backoff.max(asked)
}

impl WebhookSender {
    /// A sender over the store.
    pub fn new(
        store: Store,
        config: WebhooksConfig,
        cipher: CredentialCipher,
        clock: Arc<dyn Clock>,
        transport: Arc<dyn Transport>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            config,
            cipher,
            clock,
            transport,
            receivers: Mutex::new(HashMap::new()),
            wake: Notify::new(),
        })
    }

    /// Tells the sender that an event was queued.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    fn receivers(&self) -> std::sync::MutexGuard<'_, HashMap<String, Receiver>> {
        self.receivers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The receivers nothing is sent to right now: one delivery is in
    /// flight to them, or their last attempt failed and they are left
    /// alone until their next probe.
    fn busy(&self, now: DateTime<Utc>) -> Vec<String> {
        self.receivers()
            .iter()
            .filter(|(_, receiver)| {
                receiver.in_flight || receiver.blocked_until.is_some_and(|until| until > now)
            })
            .map(|(origin, _)| origin.clone())
            .collect()
    }

    /// Runs until `shutdown` resolves: sends what is due, one delivery per
    /// receiver at a time. An attempt in flight at the stop is finished.
    pub async fn run(self: Arc<Self>, shutdown: impl Future<Output = ()> + Send) {
        tokio::pin!(shutdown);
        let mut attempts: JoinSet<Sent> = JoinSet::new();
        let mut stopping = false;
        loop {
            if !stopping {
                self.start_due(&mut attempts).await;
            }
            if stopping && attempts.is_empty() {
                return;
            }
            tokio::select! {
                () = &mut shutdown, if !stopping => stopping = true,
                Some(sent) = attempts.join_next(), if !attempts.is_empty() => {
                    if let Ok(sent) = sent {
                        self.settle(&sent);
                    }
                }
                () = self.wake.notified(), if !stopping => {}
                () = tokio::time::sleep(self.config.poll_interval.get()), if !stopping => {}
            }
        }
    }

    /// Starts an attempt for every delivery that is due and whose
    /// receiver is free.
    async fn start_due(self: &Arc<Self>, attempts: &mut JoinSet<Sent>) {
        loop {
            let now = self.clock.now();
            let lease = self.config.timeout.get() + LEASE_SLACK;
            let lease_until = chrono::Duration::from_std(lease)
                .ok()
                .and_then(|lease| now.checked_add_signed(lease))
                .unwrap_or(now);
            let busy = self.busy(now);
            let claimed = match self
                .store
                .webhooks()
                .claim_due(now, lease_until, &busy)
                .await
            {
                Ok(Some(delivery)) => delivery,
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(%error, "the webhook deliveries that are due could not be read");
                    return;
                }
            };
            self.receivers()
                .entry(claimed.origin.clone())
                .or_default()
                .in_flight = true;
            let sender = Arc::clone(self);
            attempts.spawn(async move { sender.attempt(claimed).await });
        }
    }

    /// What an attempt means for its receiver.
    fn settle(&self, sent: &Sent) {
        let mut receivers = self.receivers();
        let receiver = receivers.entry(sent.origin.clone()).or_default();
        receiver.in_flight = false;
        if sent.delivered {
            receiver.failures = 0;
            receiver.blocked_until = None;
        } else {
            receiver.failures = receiver.failures.saturating_add(1);
            // Left alone until the event that probed it is due again; when
            // that event gave up, as long as its failures in a row say.
            let step = receiver.failures.min(self.config.max_retries.max(1));
            let own = chrono::Duration::from_std(self.config.backoff(step))
                .ok()
                .and_then(|wait| self.clock.now().checked_add_signed(wait));
            receiver.blocked_until = sent.next.or(own);
        }
        // A receiver that answers well and has nothing in flight needs no
        // entry.
        receivers.retain(|_, receiver| {
            receiver.in_flight || receiver.failures > 0 || receiver.blocked_until.is_some()
        });
    }

    /// What is sent for a delivery, or why nothing can be.
    async fn outgoing(&self, delivery: &WebhookDelivery) -> Result<Outgoing, &'static str> {
        let job = self
            .store
            .jobs()
            .get(delivery.job_id)
            .await
            .map_err(|_| "store")?
            .ok_or("job_gone")?;
        let url = job
            .webhook_url
            .as_deref()
            .and_then(|url| Url::parse(url).ok())
            .ok_or("no_receiver")?;
        // The list may have changed since the job was accepted.
        if !self.config.allows(&url) {
            return Err("origin_not_allowed");
        }
        let secret = job
            .webhook_secret
            .as_ref()
            .and_then(|sealed| self.cipher.open_bound(&secret_binding(job.id), sealed).ok())
            .ok_or("secret_unreadable")?;
        let timestamp = self.clock.now().timestamp();
        Ok(Outgoing {
            private: self.config.is_private(&url),
            url,
            headers: vec![
                (EVENT_HEADER, delivery.event.as_str().to_owned()),
                (DELIVERY_HEADER, delivery.id.to_string()),
                (TIMESTAMP_HEADER, timestamp.to_string()),
                (
                    SIGNATURE_HEADER,
                    signature(secret.expose().as_bytes(), timestamp, &delivery.body),
                ),
            ],
            body: delivery.body.clone(),
            timeout: self.config.timeout.get(),
        })
    }

    /// One attempt at one delivery, written into the log.
    async fn attempt(&self, delivery: WebhookDelivery) -> Sent {
        let started_at = self.clock.now();
        let n = delivery.attempts.saturating_add(1);
        let (status, error, retry_after, final_failure) = match self.outgoing(&delivery).await {
            // Nothing a later attempt would change.
            Err(reason) => (None, Some(reason.to_owned()), None, reason != "store"),
            Ok(outgoing) => match self.transport.post(&outgoing).await {
                Ok(answer) => (
                    Some(answer.status),
                    None,
                    answer.retry_after,
                    // The receiver says it is gone for good.
                    answer.status == 410,
                ),
                Err(error) => (None, Some(error.to_string()), None, false),
            },
        };
        let delivered = status.is_some_and(|status| (200..300).contains(&status));
        let ended_at = self.clock.now();
        let end = if delivered {
            AttemptEnd::Delivered
        } else if final_failure || n > self.config.max_retries {
            AttemptEnd::Failed
        } else {
            let wait = retry_wait(&self.config, delivery.id, n, retry_after);
            let next = chrono::Duration::from_std(wait)
                .ok()
                .and_then(|wait| ended_at.checked_add_signed(wait))
                .unwrap_or(ended_at);
            AttemptEnd::Retry(next)
        };
        let result = match end {
            AttemptEnd::Delivered => "delivered",
            AttemptEnd::Retry(_) => "retry",
            AttemptEnd::Failed => "failed",
        };
        metrics::counter!(
            "kage_webhook_deliveries_total",
            "event" => delivery.event.as_str(),
            "result" => result
        )
        .increment(1);
        tracing::info!(
            delivery = %delivery.id,
            job = %delivery.job_id,
            event = delivery.event.as_str(),
            attempt = n,
            status,
            error = error.as_deref(),
            result,
            "webhook attempt"
        );
        let attempt = WebhookAttempt {
            delivery_id: delivery.id,
            n,
            started_at,
            duration_ms: u32::try_from((ended_at - started_at).num_milliseconds().max(0))
                .unwrap_or(u32::MAX),
            status,
            error,
        };
        if let Err(error) = self.store.webhooks().record(&attempt, end).await {
            // The claim's lease runs out and the delivery is due again.
            tracing::warn!(delivery = %delivery.id, %error, "a webhook attempt could not be recorded");
        }
        Sent {
            origin: delivery.origin,
            delivered,
            next: match end {
                AttemptEnd::Retry(next) => Some(next),
                AttemptEnd::Delivered | AttemptEnd::Failed => None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::time::Duration;

    use chrono::{TimeZone, Utc};
    use kage_core::config::WebhooksConfig;
    use kage_core::ids::WebhookDeliveryId;

    use super::{postable, retry_wait, signature, verify};

    #[test]
    fn a_signature_is_the_hmac_of_the_timestamp_a_dot_and_the_body() {
        // RFC 4231-style check against a value computed independently:
        // HMAC-SHA-256("secret-secret-16", "1760180400.{\"a\":1}").
        let signed = signature(b"secret-secret-16", 1_760_180_400, "{\"a\":1}");

        assert!(signed.starts_with("sha256="));
        assert_eq!(signed.len(), "sha256=".len() + 64);
        assert!(
            signed["sha256=".len()..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
        );
        // The same input signs the same; any part that differs does not.
        assert_eq!(
            signed,
            signature(b"secret-secret-16", 1_760_180_400, "{\"a\":1}")
        );
        assert_ne!(
            signed,
            signature(b"secret-secret-17", 1_760_180_400, "{\"a\":1}")
        );
        assert_ne!(
            signed,
            signature(b"secret-secret-16", 1_760_180_401, "{\"a\":1}")
        );
        assert_ne!(
            signed,
            signature(b"secret-secret-16", 1_760_180_400, "{\"a\":2}")
        );
        // The timestamp and the body cannot be shifted into each other.
        assert_ne!(signature(b"k", 12, "3.x"), signature(b"k", 123, ".x"));
    }

    #[test]
    fn a_receiver_accepts_a_fresh_signature_and_nothing_else() {
        let secret = b"0123456789abcdef";
        let body = "{\"type\":\"job.completed\"}";
        let now = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
        let at = now.timestamp();
        let signed = signature(secret, at, body);
        let five = Duration::from_mins(5);
        let accepts = |timestamp: &str, body: &str, signed: &str, now| {
            verify(secret, timestamp, body, signed, now, five)
        };

        assert!(accepts(&at.to_string(), body, &signed, now));
        // A clock a little off, either way.
        assert!(accepts(
            &at.to_string(),
            body,
            &signed,
            now + chrono::Duration::minutes(4)
        ));
        assert!(accepts(
            &at.to_string(),
            body,
            &signed,
            now - chrono::Duration::minutes(4)
        ));
        // A replay, a changed body, a changed timestamp, another secret,
        // a header that is no number.
        assert!(!accepts(
            &at.to_string(),
            body,
            &signed,
            now + chrono::Duration::minutes(6)
        ));
        assert!(!accepts(
            &at.to_string(),
            "{\"type\":\"job.failed\"}",
            &signed,
            now
        ));
        assert!(!accepts(&(at + 1).to_string(), body, &signed, now));
        assert!(!verify(
            b"another-secret-16",
            &at.to_string(),
            body,
            &signed,
            now,
            five
        ));
        assert!(!accepts("yesterday", body, &signed, now));
        assert!(!accepts(&at.to_string(), body, "sha256=00", now));
    }

    #[test]
    fn the_waits_follow_the_schedule_and_each_event_has_its_own() {
        let config = WebhooksConfig::default();
        let delivery = WebhookDeliveryId::new();
        let steps = [30_u64, 120, 480, 1920, 7680];

        for (retry, base) in (1_u32..).zip(steps) {
            let wait = retry_wait(&config, delivery, retry, None).as_secs_f64();
            #[allow(clippy::cast_precision_loss)]
            let base = base as f64;
            assert!(
                (base * 0.8..=base * 1.2).contains(&wait),
                "retry {retry}: {wait} around {base}"
            );
            // The same event waits the same; it replays.
            assert_eq!(
                retry_wait(&config, delivery, retry, None),
                retry_wait(&config, delivery, retry, None)
            );
        }
        // Two events that failed together do not come back together.
        let other = WebhookDeliveryId::new();
        assert_ne!(
            retry_wait(&config, delivery, 1, None),
            retry_wait(&config, other, 1, None)
        );
        // What the receiver asks for is honoured when it is longer, up to
        // an hour.
        assert_eq!(
            retry_wait(&config, delivery, 1, Some(Duration::from_mins(10))),
            Duration::from_mins(10)
        );
        assert_eq!(
            retry_wait(&config, delivery, 1, Some(Duration::from_hours(9))),
            Duration::from_hours(1)
        );
        assert!(
            retry_wait(&config, delivery, 3, Some(Duration::from_secs(1))) > Duration::from_mins(6)
        );
        // No jitter: the schedule itself.
        let exact = WebhooksConfig {
            jitter: 0.0,
            ..WebhooksConfig::default()
        };
        assert_eq!(
            retry_wait(&exact, delivery, 2, None),
            Duration::from_mins(2)
        );
    }

    #[test]
    fn only_a_public_address_is_posted_to_unless_the_receiver_is_the_operators_own() {
        let address = |text: &str| text.parse::<IpAddr>().unwrap();

        for public in ["93.184.216.34", "2606:2800:220:1::1"] {
            assert!(postable(address(public), false), "{public}");
            assert!(postable(address(public), true), "{public}");
        }
        for own in [
            "127.0.0.1",
            "10.0.0.5",
            "172.18.0.4",
            "192.168.1.10",
            "::1",
            "fd00::7",
        ] {
            assert!(!postable(address(own), false), "{own}");
            assert!(postable(address(own), true), "{own}");
        }
        // Never, whoever listed what.
        for never in [
            "169.254.169.254",
            "169.254.10.1",
            "fe80::1",
            "224.0.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "100.64.0.1",
            "192.0.2.1",
        ] {
            assert!(!postable(address(never), false), "{never}");
            assert!(!postable(address(never), true), "{never}");
        }
    }
}
