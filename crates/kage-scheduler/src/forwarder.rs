//! The forwarder (follow-up F10): the one proxy every persona's browser
//! and http client is given.
//!
//! A client connects with a credential that only names its persona. The
//! forwarder looks up that persona's own proxy, connects to it and nowhere
//! else, adds the provider's credentials, relays, and counts every byte in
//! both directions. So the provider's credentials never enter a browser or
//! a sidecar container, the bytes are counted where the budget lives, and
//! what must not be sent is stopped where it would pass:
//!
//! - **Whose proxy.** The credential is the persona's id and a token made
//!   from the master key, so it is the same after a restart and names
//!   exactly one persona. A persona that is unknown, retired or cooling,
//!   or whose proxy cannot carry traffic, is refused. A context that
//!   outlived its persona can never leave through another one's address.
//! - **Where to.** `CONNECT host:port` and absolute-form `http://`
//!   requests only, and never to a target the SSRF guard refuses:
//!   loopback, private, link-local and metadata addresses, local names,
//!   port 0, credentials in the URL. The browsers send loopback through
//!   their proxy, so this is the last line between a page's redirect and
//!   the compose network. The operator's own self-test hosts are the one
//!   exception, by name and port.
//! - **Whether at all.** While a leak is on record, or the bandwidth
//!   budget is used up, no new connection is relayed: a browser's own
//!   background traffic stops too.
//!
//! A refusal looks like a network that does not answer, not like a proxy
//! with a name: a refused tunnel gets a bare `403`, which a browser turns
//! into a failed connection, and a refused plain request is closed without
//! an answer, so that no page ever reads a response that Kage wrote.
//!
//! Targets see none of this: TLS is end to end, and the connection they
//! see is the exit's.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use hmac::{Hmac, KeyInit, Mac};
use kage_core::domain::{IDLE_DOMAIN, Persona, ProxyEndpoint, ProxyScheme};
use kage_core::ids::PersonaId;
use kage_core::secret::Secret;
use kage_core::ssrf::{SsrfPolicy, check_url};
use kage_persona::{Egress, Via};
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::Instant;
use url::Url;
use uuid::Uuid;

/// The longest request or response head the forwarder reads.
const HEAD_LIMIT: usize = 32 * 1024;

/// How long a client may take to send its request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Bytes read at once while relaying.
const CHUNK: usize = 16 * 1024;

/// What the token of a persona is made from, besides the key.
const TOKEN_LABEL: &[u8] = b"kage-forwarder-token-v1";

/// Hexadecimal characters of a token: 128 bits.
const TOKEN_CHARS: usize = 32;

// --- Credentials -----------------------------------------------------------

/// Makes and checks the credential a persona's clients present to the
/// forwarder: the persona's id and a token only this installation can
/// make.
#[derive(Clone)]
pub struct Tokens {
    key: [u8; 32],
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Tokens(..)")
    }
}

fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;

    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a String cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

impl Tokens {
    /// Tokens under a key derived from the master key, so that the master
    /// key itself signs nothing.
    pub fn new(master_key: &[u8; 32]) -> Self {
        let mut key = [0_u8; 32];
        if let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(master_key) {
            mac.update(b"kage-forwarder-key-v1");
            key.copy_from_slice(&mac.finalize().into_bytes());
        }
        Self { key }
    }

    /// The token of a persona.
    pub fn token(&self, persona: PersonaId) -> String {
        let mut text = Hmac::<Sha256>::new_from_slice(&self.key).map_or_else(
            |_| String::new(),
            |mut mac| {
                mac.update(TOKEN_LABEL);
                mac.update(persona.as_uuid().as_bytes());
                hex(&mac.finalize().into_bytes())
            },
        );
        text.truncate(TOKEN_CHARS);
        text
    }

    /// The persona a credential names, when the token is that persona's.
    pub fn verify(&self, username: &str, password: &str) -> Option<PersonaId> {
        let persona = PersonaId::from_uuid(Uuid::parse_str(username).ok()?);
        let expected = self.token(persona);
        // Compared without stopping at the first difference.
        let equal = expected.len() == password.len()
            && expected
                .bytes()
                .zip(password.bytes())
                .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
                == 0;
        (equal && !expected.is_empty()).then_some(persona)
    }
}

/// Hands every persona the forwarder as its proxy, under the persona's own
/// credential.
#[derive(Debug, Clone)]
pub struct ForwarderEgress {
    tokens: Arc<Tokens>,
    /// The forwarder as the sidecars reach it.
    advertised: (String, u16),
    /// The forwarder as this process reaches it.
    local: SocketAddr,
}

impl Egress for ForwarderEgress {
    fn endpoint(&self, persona: &Persona, upstream: &ProxyEndpoint, via: Via) -> ProxyEndpoint {
        let (host, port) = match via {
            Via::Browser => self.advertised.clone(),
            Via::Http => (self.local.ip().to_string(), self.local.port()),
        };
        ProxyEndpoint {
            id: upstream.id,
            host,
            port,
            username: Secret::new(persona.id.to_string()),
            password: Secret::new(self.tokens.token(persona.id)),
            scheme: ProxyScheme::Http,
        }
    }
}

// --- Routes ----------------------------------------------------------------

/// Why a persona's traffic is not relayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteRefusal {
    /// There is no such persona.
    UnknownPersona,
    /// The persona is cooling or retired.
    PersonaNotActive,
    /// Its proxy is in backoff, being replaced, or gone.
    ProxyNotUsable,
    /// The answer could not be found out: the store or the pool failed.
    Unavailable,
}

impl RouteRefusal {
    /// A stable name for logs and metrics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownPersona => "unknown_persona",
            Self::PersonaNotActive => "persona_not_active",
            Self::ProxyNotUsable => "proxy_not_usable",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Finds the one proxy a persona's traffic may leave through.
#[async_trait]
pub trait Routes: Send + Sync + fmt::Debug {
    /// The persona's own proxy with the provider's credentials.
    async fn upstream(&self, persona: PersonaId) -> Result<ProxyEndpoint, RouteRefusal>;
}

// --- The meter -------------------------------------------------------------

/// Bytes one persona exchanged with its proxy while one domain was being
/// fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counted {
    /// The persona.
    pub persona: PersonaId,
    /// The domain being fetched, or `(idle)`.
    pub domain: String,
    /// Bytes sent towards the proxy.
    pub bytes_up: u64,
    /// Bytes received from the proxy.
    pub bytes_down: u64,
}

#[derive(Debug, Default)]
struct MeterState {
    labels: HashMap<PersonaId, String>,
    bytes: HashMap<(PersonaId, String), (u64, u64)>,
}

/// Counts what passes, per persona and per domain being fetched.
#[derive(Debug, Default)]
pub struct Meter {
    state: Mutex<MeterState>,
    total_up: AtomicU64,
    total_down: AtomicU64,
}

/// While it lives, a persona's bytes count toward a domain.
#[derive(Debug)]
pub struct Label {
    meter: Arc<Meter>,
    persona: PersonaId,
}

impl Drop for Label {
    fn drop(&mut self) {
        self.meter.state().labels.remove(&self.persona);
    }
}

impl Meter {
    fn state(&self) -> MutexGuard<'_, MeterState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts a persona's bytes toward `domain` until the label is
    /// dropped; outside any label they count as `(idle)`.
    pub fn label(self: &Arc<Self>, persona: PersonaId, domain: &str) -> Label {
        self.state().labels.insert(persona, domain.to_owned());
        Label {
            meter: Arc::clone(self),
            persona,
        }
    }

    /// Adds bytes of a persona.
    pub fn add(&self, persona: PersonaId, up: u64, down: u64) {
        if up == 0 && down == 0 {
            return;
        }
        let mut state = self.state();
        let domain = state
            .labels
            .get(&persona)
            .cloned()
            .unwrap_or_else(|| IDLE_DOMAIN.to_owned());
        let entry = state.bytes.entry((persona, domain)).or_default();
        entry.0 = entry.0.saturating_add(up);
        entry.1 = entry.1.saturating_add(down);
        drop(state);
        self.total_up.fetch_add(up, Ordering::Relaxed);
        self.total_down.fetch_add(down, Ordering::Relaxed);
    }

    /// Everything counted since the last call, and forgets it.
    pub fn drain(&self) -> Vec<Counted> {
        let mut counted: Vec<Counted> = std::mem::take(&mut self.state().bytes)
            .into_iter()
            .map(|((persona, domain), (bytes_up, bytes_down))| Counted {
                persona,
                domain,
                bytes_up,
                bytes_down,
            })
            .collect();
        counted.sort_by(|a, b| (a.persona, &a.domain).cmp(&(b.persona, &b.domain)));
        counted
    }

    /// Everything counted since the forwarder started: up, down.
    pub fn totals(&self) -> (u64, u64) {
        (
            self.total_up.load(Ordering::Relaxed),
            self.total_down.load(Ordering::Relaxed),
        )
    }
}

// --- Requests --------------------------------------------------------------

/// A request head as a client sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Head {
    method: String,
    target: String,
    version: String,
    /// The header lines, without their line ends, in order.
    lines: Vec<String>,
}

fn header_name(line: &str) -> &str {
    line.split_once(':').map_or("", |(name, _)| name.trim())
}

impl Head {
    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.split("\r\n");
        let mut parts = lines.next()?.split(' ');
        let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some()
            || method.is_empty()
            || target.is_empty()
            || !version.starts_with("HTTP/1.")
        {
            return None;
        }
        Some(Self {
            method: method.to_owned(),
            target: target.to_owned(),
            version: version.to_owned(),
            lines: lines
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.lines
            .iter()
            .find(|line| header_name(line).eq_ignore_ascii_case(name))
            .and_then(|line| line.split_once(':'))
            .map(|(_, value)| value.trim())
    }

    /// The user name and password of `Proxy-Authorization: Basic`.
    fn credentials(&self) -> Option<(String, String)> {
        let value = self.header("proxy-authorization")?;
        let (scheme, encoded) = value.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("basic") {
            return None;
        }
        let decoded = String::from_utf8(base64_decode(encoded.trim())?).ok()?;
        let (username, password) = decoded.split_once(':')?;
        Some((username.to_owned(), password.to_owned()))
    }

    fn is_connect(&self) -> bool {
        self.method.eq_ignore_ascii_case("CONNECT")
    }

    /// The head as it goes upstream: as the client wrote it, with the
    /// provider's credentials in place of the persona's.
    fn rewritten(&self, upstream: &ProxyEndpoint) -> Vec<u8> {
        let mut text = format!("{} {} {}\r\n", self.method, self.target, self.version);
        for line in &self.lines {
            if !header_name(line).eq_ignore_ascii_case("proxy-authorization") {
                text.push_str(line);
                text.push_str("\r\n");
            }
        }
        let login = format!(
            "{}:{}",
            upstream.username.expose(),
            upstream.password.expose()
        );
        text.push_str("Proxy-Authorization: Basic ");
        text.push_str(&base64_encode(login.as_bytes()));
        text.push_str("\r\n\r\n");
        text.into_bytes()
    }

    /// How the body of a plain request is framed.
    fn body(&self) -> Body {
        if self
            .header("transfer-encoding")
            .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
        {
            return Body::Chunked;
        }
        match self
            .header("content-length")
            .and_then(|value| value.parse::<u64>().ok())
        {
            Some(length) if length > 0 => Body::Length(length),
            _ => Body::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Body {
    None,
    Length(u64),
    Chunked,
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let sextets = [
            group[0] >> 2,
            ((group[0] & 0x03) << 4) | (group[1] >> 4),
            ((group[1] & 0x0f) << 2) | (group[2] >> 6),
            group[2] & 0x3f,
        ];
        for (index, sextet) in sextets.iter().enumerate() {
            if index <= chunk.len() {
                text.push(char::from(BASE64[usize::from(*sextet)]));
            } else {
                text.push('=');
            }
        }
    }
    text
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    let mut bytes = Vec::with_capacity(text.len() * 3 / 4);
    let (mut buffer, mut bits) = (0_u32, 0_u32);
    for character in text.bytes() {
        let value = BASE64.iter().position(|known| *known == character)?;
        buffer = (buffer << 6) | u32::try_from(value).ok()?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push(u8::try_from((buffer >> bits) & 0xff).ok()?);
        }
    }
    Some(bytes)
}

/// The URL a request asks the proxy for, and its host and port.
fn target_of(head: &Head) -> Option<(Url, String, u16)> {
    let url = if head.is_connect() {
        // An authority: `host:port`. The scheme is not known and does not
        // matter to the guard.
        let url = Url::parse(&format!("http://{}/", head.target)).ok()?;
        // A tunnel names its port; a path or a query is no authority.
        if url.path() != "/" || url.query().is_some() || !head.target.contains(':') {
            return None;
        }
        url
    } else {
        let url = Url::parse(&head.target).ok()?;
        if url.scheme() != "http" {
            return None;
        }
        url
    };
    let host = url.host_str()?.to_ascii_lowercase();
    let port = url.port_or_known_default()?;
    Some((url, host, port))
}

// --- The forwarder ---------------------------------------------------------

/// How the forwarder listens and relays.
#[derive(Debug, Clone)]
pub struct ForwarderSettings {
    /// The forwarder as the sidecars reach it: host and port.
    pub advertised: (String, u16),
    /// Longest the connection to a persona's proxy and its answer to a
    /// `CONNECT` may take.
    pub connect_timeout: Duration,
    /// A relayed connection on which nothing moved for this long is closed.
    pub idle_timeout: Duration,
    /// Connections relayed at once.
    pub max_connections: usize,
    /// The operator's own hosts, by lower-case name and port, that are
    /// relayed although the guard would refuse them: the self-test pages.
    pub operator_hosts: HashSet<(String, u16)>,
}

impl ForwarderSettings {
    /// The settings of the configuration, with the hosts of the operator's
    /// own self-test pages and echo as the exceptions of the guard.
    pub fn from_config(config: &kage_core::config::Config) -> Self {
        let advertised = Url::parse(&config.forwarder.advertise).ok();
        let host_of = |text: &str| {
            let url = Url::parse(text).ok()?;
            Some((
                url.host_str()?.to_ascii_lowercase(),
                url.port_or_known_default()?,
            ))
        };
        let selftest = &config.selftest;
        let operator_hosts = selftest
            .browser
            .pages
            .iter()
            .map(|page| page.url.as_str())
            .chain(selftest.http.echo_url.as_deref())
            .filter_map(host_of)
            .collect();
        Self {
            advertised: advertised
                .as_ref()
                .and_then(|url| Some((url.host_str()?.to_owned(), url.port_or_known_default()?)))
                .unwrap_or_else(|| ("kage".to_owned(), config.forwarder.bind.port())),
            connect_timeout: config.forwarder.connect_timeout.get(),
            idle_timeout: config.forwarder.idle_timeout.get(),
            max_connections: usize::try_from(config.forwarder.max_connections)
                .unwrap_or(usize::MAX),
            operator_hosts,
        }
    }
}

/// Why a connection ended without being relayed, for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Refused {
    /// The request head was not one.
    BadRequest,
    /// No credential, or not a persona's.
    Credential,
    /// Nothing may be fetched.
    Stopped(&'static str),
    /// The target is one the guard refuses.
    Target(String),
    /// The persona's traffic is not relayed.
    Route(RouteRefusal),
    /// The persona's proxy could not be reached or did not answer.
    Upstream(String),
}

/// See the module documentation.
#[derive(Debug)]
pub struct Forwarder {
    settings: ForwarderSettings,
    tokens: Arc<Tokens>,
    routes: Arc<dyn Routes>,
    meter: Arc<Meter>,
    stop: Mutex<Option<&'static str>>,
    connections: Arc<Semaphore>,
    local: SocketAddr,
    refused: AtomicU64,
    relayed: AtomicU64,
}

impl Forwarder {
    /// Binds the forwarder. A port of 0 takes a free one.
    ///
    /// # Errors
    ///
    /// Fails when the address cannot be bound.
    pub async fn bind(
        bind: SocketAddr,
        settings: ForwarderSettings,
        tokens: Tokens,
        routes: Arc<dyn Routes>,
    ) -> std::io::Result<(Arc<Self>, TcpListener)> {
        let listener = TcpListener::bind(bind).await?;
        let bound = listener.local_addr()?;
        // What this process connects to: the bound port on loopback, or on
        // the one address the forwarder listens on.
        let local = if bound.ip().is_unspecified() {
            SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), bound.port())
        } else {
            bound
        };
        let mut settings = settings;
        if settings.advertised.1 == 0 || bind.port() == 0 {
            // A port that was not fixed is advertised as it was bound.
            settings.advertised.1 = bound.port();
        }
        let forwarder = Arc::new(Self {
            connections: Arc::new(Semaphore::new(settings.max_connections.max(1))),
            settings,
            tokens: Arc::new(tokens),
            routes,
            meter: Arc::new(Meter::default()),
            stop: Mutex::new(None),
            local,
            refused: AtomicU64::new(0),
            relayed: AtomicU64::new(0),
        });
        Ok((forwarder, listener))
    }

    /// The address this process reaches the forwarder at.
    pub const fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// The counters.
    pub const fn meter(&self) -> &Arc<Meter> {
        &self.meter
    }

    /// The egress that hands every persona this forwarder.
    pub fn egress(&self) -> ForwarderEgress {
        ForwarderEgress {
            tokens: Arc::clone(&self.tokens),
            advertised: self.settings.advertised.clone(),
            local: self.local,
        }
    }

    /// Refuses every new connection for a reason (a leak, the bandwidth
    /// cap), or relays again with `None`.
    pub fn set_stop(&self, stop: Option<&'static str>) {
        *self.stop.lock().unwrap_or_else(PoisonError::into_inner) = stop;
    }

    /// Connections relayed and connections refused since the start. A
    /// connection counts as relayed from the moment its first request went
    /// to the persona's proxy and, for a tunnel, the proxy opened it.
    pub fn counts(&self) -> (u64, u64) {
        (
            self.relayed.load(Ordering::Relaxed),
            self.refused.load(Ordering::Relaxed),
        )
    }

    fn stopped(&self) -> Option<&'static str> {
        *self.stop.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Accepts and relays until `shutdown` resolves. Connections under way
    /// are dropped then.
    pub async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send,
    ) {
        let mut tasks = tokio::task::JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                // Finished connections are reaped as they end.
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                accepted = listener.accept() => {
                    let Ok((client, _)) = accepted else {
                        // Out of descriptors, or the like: let it pass.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    };
                    let Ok(permit) = Arc::clone(&self.connections).try_acquire_owned() else {
                        // Full: the client sees a connection that closes.
                        self.refused.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };
                    let forwarder = Arc::clone(&self);
                    tasks.spawn(async move {
                        forwarder.connection(client).await;
                        drop(permit);
                    });
                }
            }
        }
        tasks.shutdown().await;
    }

    async fn connection(&self, mut client: TcpStream) {
        let _ = client.set_nodelay(true);
        let mut buffer = Vec::new();
        // Nothing sent, too much sent, or too slowly: no answer.
        let Ok(Ok(Some(head))) =
            tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut client, &mut buffer)).await
        else {
            return;
        };
        if let Err(refused) = self.relay(&mut client, head, buffer).await {
            self.refused.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(reason = ?refused, "the forwarder refused a connection");
        }
        let _ = client.shutdown().await;
    }

    /// Checks one request and relays its connection.
    async fn relay(
        &self,
        client: &mut TcpStream,
        head: Head,
        buffer: Vec<u8>,
    ) -> Result<(), Refused> {
        let Some((url, host, port)) = target_of(&head) else {
            // Not a request to a proxy: no answer.
            return Err(Refused::BadRequest);
        };
        let Some(persona) = head
            .credentials()
            .and_then(|(username, password)| self.tokens.verify(&username, &password))
        else {
            // Asking for a credential is what any proxy does.
            let _ = client
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\n\
                      Proxy-Authenticate: Basic realm=\"proxy\"\r\n\
                      Content-Length: 0\r\nConnection: close\r\nProxy-Connection: close\r\n\r\n",
                )
                .await;
            return Err(Refused::Credential);
        };
        let tunnel = head.is_connect();
        if let Some(reason) = self.stopped() {
            return refuse(client, tunnel, Refused::Stopped(reason)).await;
        }
        if let Err(reason) = self.check_target(&url, &host, port) {
            return refuse(client, tunnel, Refused::Target(reason)).await;
        }
        let upstream = match self.routes.upstream(persona).await {
            Ok(upstream) => upstream,
            Err(refusal) => return refuse(client, tunnel, Refused::Route(refusal)).await,
        };
        let connecting = TcpStream::connect((upstream.host.as_str(), upstream.port));
        let mut server = match tokio::time::timeout(self.settings.connect_timeout, connecting).await
        {
            Ok(Ok(server)) => server,
            Ok(Err(error)) => {
                return refuse(client, tunnel, Refused::Upstream(error.to_string())).await;
            }
            Err(_) => {
                let reason = Refused::Upstream("the proxy did not answer".to_owned());
                return refuse(client, tunnel, reason).await;
            }
        };
        let _ = server.set_nodelay(true);
        tracing::trace!(%persona, %host, port, tunnel = head.is_connect(), "relaying");

        if head.is_connect() {
            self.tunnel(client, &mut server, &head, buffer, persona, &upstream)
                .await
        } else {
            self.plain(client, server, head, buffer, persona, &upstream)
                .await;
            Ok(())
        }
    }

    /// The guard: whether the forwarder may ask the proxy for this target.
    fn check_target(&self, url: &Url, host: &str, port: u16) -> Result<(), String> {
        if self
            .settings
            .operator_hosts
            .contains(&(host.to_owned(), port))
        {
            return Ok(());
        }
        // A page may load from a public address by number; what the guard
        // refuses by range it refuses here too. Whether a request may
        // *name* an address is decided where requests are accepted.
        let policy = SsrfPolicy {
            allow_ip_targets: true,
        };
        check_url(url, policy)
            .map(|_| ())
            .map_err(|denial| denial.to_string())
    }

    /// A `CONNECT`: asks the proxy for the tunnel and relays both ways.
    async fn tunnel(
        &self,
        client: &mut TcpStream,
        server: &mut TcpStream,
        head: &Head,
        early: Vec<u8>,
        persona: PersonaId,
        upstream: &ProxyEndpoint,
    ) -> Result<(), Refused> {
        let request = head.rewritten(upstream);
        let failed = |error: std::io::Error| Refused::Upstream(error.to_string());
        server.write_all(&request).await.map_err(failed)?;
        self.meter.add(persona, request.len() as u64, 0);

        let mut answer = Vec::new();
        let status = tokio::time::timeout(
            self.settings.connect_timeout,
            read_status(server, &mut answer),
        )
        .await
        .map_err(|_| Refused::Upstream("the proxy did not answer the tunnel".to_owned()))?
        .map_err(failed)?;
        self.meter.add(persona, 0, answer.len() as u64);
        // The proxy's own answer goes to the client as it came: a refusal
        // of the proxy is a failed tunnel to the browser.
        client.write_all(&answer).await.map_err(failed)?;
        if !(200..300).contains(&status) {
            return Err(Refused::Upstream(format!(
                "the proxy answered the tunnel with {status}"
            )));
        }
        self.relayed.fetch_add(1, Ordering::Relaxed);
        if !early.is_empty() {
            server.write_all(&early).await.map_err(failed)?;
            self.meter.add(persona, early.len() as u64, 0);
        }

        let active = Activity::new();
        let (mut client_read, mut client_write) = client.split();
        let (mut server_read, mut server_write) = server.split();
        let idle = self.settings.idle_timeout;
        let up = pump(
            &mut client_read,
            &mut server_write,
            idle,
            &active,
            |bytes| {
                self.meter.add(persona, bytes, 0);
            },
        );
        let down = pump(
            &mut server_read,
            &mut client_write,
            idle,
            &active,
            |bytes| {
                self.meter.add(persona, 0, bytes);
            },
        );
        tokio::join!(up, down);
        Ok(())
    }

    /// Plain `http://` requests: every request head of the connection gets
    /// the provider's credentials and passes the guard; answers are
    /// relayed as they come.
    async fn plain(
        &self,
        client: &mut TcpStream,
        mut server: TcpStream,
        first: Head,
        mut buffer: Vec<u8>,
        persona: PersonaId,
        upstream: &ProxyEndpoint,
    ) {
        self.relayed.fetch_add(1, Ordering::Relaxed);
        let active = Activity::new();
        let idle = self.settings.idle_timeout;
        let (mut client_read, mut client_write) = client.split();
        let (mut server_read, mut server_write) = server.split();
        let down = pump(
            &mut server_read,
            &mut client_write,
            idle,
            &active,
            |bytes| {
                self.meter.add(persona, 0, bytes);
            },
        );
        let up = async {
            let mut head = first;
            loop {
                let request = head.rewritten(upstream);
                if server_write.write_all(&request).await.is_err() {
                    break;
                }
                self.meter.add(persona, request.len() as u64, 0);
                active.touch();
                let sent = send_body(
                    head.body(),
                    &mut client_read,
                    &mut server_write,
                    &mut buffer,
                    idle,
                    &active,
                )
                .await;
                match sent {
                    Ok(bytes) => self.meter.add(persona, bytes, 0),
                    Err(_) => break,
                }
                // The next request of the connection, when there is one.
                let next = loop {
                    match tokio::time::timeout(idle, read_head(&mut client_read, &mut buffer)).await
                    {
                        Ok(Ok(next)) => break next,
                        // Nothing came: fine while answers still flow.
                        Err(_) if active.within(idle) => {}
                        Ok(Err(_)) | Err(_) => break None,
                    }
                };
                let Some(next) = next else { break };
                let same_persona = next
                    .credentials()
                    .and_then(|(username, password)| self.tokens.verify(&username, &password))
                    == Some(persona);
                let allowed = !next.is_connect()
                    && self.stopped().is_none()
                    && target_of(&next).is_some_and(|(url, host, port)| {
                        self.check_target(&url, &host, port).is_ok()
                    });
                if !(same_persona && allowed) {
                    // The connection ends: no answer of the forwarder's own
                    // may be mixed into the proxy's.
                    self.refused.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                head = next;
            }
            let _ = server_write.shutdown().await;
        };
        tokio::join!(up, down);
    }
}

/// Ends a connection that is not relayed. A tunnel that is refused is a
/// failed connection to the browser; a plain request gets no answer at
/// all, so that no page reads a response the forwarder wrote.
async fn refuse(client: &mut TcpStream, tunnel: bool, reason: Refused) -> Result<(), Refused> {
    if tunnel {
        let _ = client
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
            .await;
    }
    Err(reason)
}

/// When anything last moved on a relayed connection.
#[derive(Debug)]
struct Activity {
    started: Instant,
    last: AtomicU64,
}

impl Activity {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            last: AtomicU64::new(0),
        }
    }

    fn touch(&self) {
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last.store(now, Ordering::Relaxed);
    }

    /// Whether anything moved within the last `idle`.
    fn within(&self, idle: Duration) -> bool {
        let now = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let idle = u64::try_from(idle.as_millis()).unwrap_or(u64::MAX);
        now.saturating_sub(self.last.load(Ordering::Relaxed)) < idle
    }
}

/// Copies one direction until its end, counting. A direction that is quiet
/// stays open while the other one moves.
async fn pump<R, W>(
    from: &mut R,
    to: &mut W,
    idle: Duration,
    active: &Activity,
    count: impl Fn(u64),
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut chunk = vec![0_u8; CHUNK];
    loop {
        let read = match tokio::time::timeout(idle, from.read(&mut chunk)).await {
            Ok(Ok(0) | Err(_)) => break,
            Ok(Ok(read)) => read,
            Err(_) if active.within(idle) => continue,
            Err(_) => break,
        };
        active.touch();
        if to.write_all(&chunk[..read]).await.is_err() {
            break;
        }
        count(read as u64);
    }
    let _ = to.shutdown().await;
}

/// Reads until the end of a head (`\r\n\r\n`). Returns the offset just
/// after it; what follows stays in `buffer`. `None` when the connection
/// ended before any byte.
async fn fill_head<R: AsyncRead + Unpin>(
    from: &mut R,
    buffer: &mut Vec<u8>,
) -> std::io::Result<Option<usize>> {
    let mut chunk = [0_u8; 4096];
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            return Ok(Some(end + 4));
        }
        if buffer.len() > HEAD_LIMIT {
            return Err(std::io::Error::other("the head is too long"));
        }
        let read = from.read(&mut chunk).await?;
        if read == 0 {
            return if buffer.is_empty() {
                Ok(None)
            } else {
                Err(std::io::ErrorKind::UnexpectedEof.into())
            };
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// Reads one request head. What follows it stays in `buffer`.
async fn read_head<R: AsyncRead + Unpin>(
    from: &mut R,
    buffer: &mut Vec<u8>,
) -> std::io::Result<Option<Head>> {
    let Some(end) = fill_head(from, buffer).await? else {
        return Ok(None);
    };
    let head = std::str::from_utf8(&buffer[..end - 4])
        .ok()
        .and_then(Head::parse)
        .ok_or_else(|| std::io::Error::other("not a request head"))?;
    buffer.drain(..end);
    Ok(Some(head))
}

/// Reads a response head into `buffer` and returns its status. Everything
/// read stays in `buffer`: it is the proxy's answer, relayed as it came.
async fn read_status<R: AsyncRead + Unpin>(
    from: &mut R,
    buffer: &mut Vec<u8>,
) -> std::io::Result<u16> {
    let end = fill_head(from, buffer)
        .await?
        .ok_or(std::io::ErrorKind::UnexpectedEof)?;
    std::str::from_utf8(&buffer[..end])
        .ok()
        .and_then(|text| text.split(' ').nth(1))
        .and_then(|status| status.parse().ok())
        .ok_or_else(|| std::io::Error::other("not a response head"))
}

/// Relays the body of one plain request and returns its length. Bytes
/// already read are in `buffer`; what belongs to the next request stays
/// there.
async fn send_body<R, W>(
    body: Body,
    from: &mut R,
    to: &mut W,
    buffer: &mut Vec<u8>,
    idle: Duration,
    active: &Activity,
) -> std::io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut sent = 0_u64;
    match body {
        Body::None => {}
        Body::Length(length) => {
            send_exact(length, from, to, buffer, idle, active).await?;
            sent = length;
        }
        Body::Chunked => loop {
            // A chunk: its size line, its data, a line end. The last has
            // size zero and is followed by trailers and an empty line.
            let line = take_line(from, buffer, idle).await?;
            to.write_all(&line).await?;
            sent += line.len() as u64;
            let size = std::str::from_utf8(&line)
                .ok()
                .and_then(|text| text.trim().split(';').next())
                .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
                .ok_or_else(|| std::io::Error::other("not a chunk size"))?;
            if size == 0 {
                loop {
                    let trailer = take_line(from, buffer, idle).await?;
                    to.write_all(&trailer).await?;
                    sent += trailer.len() as u64;
                    if trailer == b"\r\n" {
                        break;
                    }
                }
                break;
            }
            send_exact(size + 2, from, to, buffer, idle, active).await?;
            sent += size + 2;
        },
    }
    active.touch();
    Ok(sent)
}

/// Relays exactly `length` bytes, first from `buffer`, then from `from`.
async fn send_exact<R, W>(
    length: u64,
    from: &mut R,
    to: &mut W,
    buffer: &mut Vec<u8>,
    idle: Duration,
    active: &Activity,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut left = length;
    let buffered = usize::try_from(left).map_or(buffer.len(), |left| left.min(buffer.len()));
    if buffered > 0 {
        to.write_all(&buffer[..buffered]).await?;
        buffer.drain(..buffered);
        left -= buffered as u64;
    }
    let mut chunk = vec![0_u8; CHUNK];
    while left > 0 {
        let want = usize::try_from(left).map_or(CHUNK, |left| left.min(CHUNK));
        let read = tokio::time::timeout(idle, from.read(&mut chunk[..want]))
            .await
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        to.write_all(&chunk[..read]).await?;
        active.touch();
        left -= read as u64;
    }
    Ok(())
}

/// Takes one line, with its line end, out of `buffer`, reading more as
/// needed.
async fn take_line<R: AsyncRead + Unpin>(
    from: &mut R,
    buffer: &mut Vec<u8>,
    idle: Duration,
) -> std::io::Result<Vec<u8>> {
    let mut chunk = [0_u8; 1024];
    loop {
        if let Some(end) = buffer.windows(2).position(|window| window == b"\r\n") {
            return Ok(buffer.drain(..end + 2).collect());
        }
        if buffer.len() > HEAD_LIMIT {
            return Err(std::io::Error::other("the line is too long"));
        }
        let read = tokio::time::timeout(idle, from.read(&mut chunk))
            .await
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use kage_core::domain::{ProxyEndpoint, ProxyScheme};
    use kage_core::ids::{PersonaId, ProxyId};
    use kage_core::secret::Secret;

    use super::{
        Body, Counted, Head, Meter, RouteRefusal, Tokens, base64_decode, base64_encode, target_of,
    };

    fn head(text: &str) -> Head {
        Head::parse(text).unwrap()
    }

    #[test]
    fn base64_round_trips_every_length() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
            ("user:pa:ss", "dXNlcjpwYTpzcw=="),
        ] {
            assert_eq!(base64_encode(plain.as_bytes()), encoded);
            assert_eq!(base64_decode(encoded).unwrap(), plain.as_bytes());
        }
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(base64_decode(&base64_encode(&bytes)).unwrap(), bytes);
        // Without the padding it is the same; with a foreign character it
        // is nothing.
        assert_eq!(base64_decode("Zm8").unwrap(), b"fo");
        assert_eq!(base64_decode("Zm8*"), None);
    }

    #[test]
    fn a_token_names_one_persona_and_is_the_same_every_time() {
        let tokens = Tokens::new(&[7; 32]);
        let (one, other) = (PersonaId::new(), PersonaId::new());

        let token = tokens.token(one);
        assert_eq!(token.len(), 32);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(token, tokens.token(one));
        assert_eq!(token, Tokens::new(&[7; 32]).token(one));
        assert_ne!(token, tokens.token(other));
        assert_ne!(token, Tokens::new(&[8; 32]).token(one));

        assert_eq!(tokens.verify(&one.to_string(), &token), Some(one));
        // Another persona's token, a shortened one, an empty one, a name
        // that is no id.
        assert_eq!(tokens.verify(&other.to_string(), &token), None);
        assert_eq!(tokens.verify(&one.to_string(), &token[..31]), None);
        assert_eq!(tokens.verify(&one.to_string(), ""), None);
        assert_eq!(tokens.verify("persona", &token), None);
        let mut wrong = token.clone().into_bytes();
        wrong[31] = if wrong[31] == b'0' { b'1' } else { b'0' };
        assert_eq!(
            tokens.verify(&one.to_string(), &String::from_utf8(wrong).unwrap()),
            None
        );
        assert_eq!(format!("{tokens:?}"), "Tokens(..)");
    }

    #[test]
    fn a_head_is_parsed_as_it_was_sent() {
        let connect = head(
            "CONNECT example.org:443 HTTP/1.1\r\nHost: example.org:443\r\n\
             Proxy-Connection: keep-alive\r\nUser-Agent: Mozilla/5.0\r\n\
             Proxy-Authorization: Basic dXNlcjpwYTpzcw==",
        );
        assert!(connect.is_connect());
        assert_eq!(connect.target, "example.org:443");
        assert_eq!(connect.header("HOST"), Some("example.org:443"));
        assert_eq!(connect.header("missing"), None);
        assert_eq!(
            connect.credentials(),
            Some(("user".to_owned(), "pa:ss".to_owned()))
        );
        assert_eq!(connect.body(), Body::None);

        for refused in [
            "",
            "GET",
            "GET / extra HTTP/1.1",
            "GET /",
            "GET / SPDY/3",
            " / HTTP/1.1",
        ] {
            assert_eq!(Head::parse(refused), None, "{refused:?}");
        }
        // Other schemes of authorization, and what is no base64 or has no
        // colon, are no credential.
        for line in [
            "Proxy-Authorization: Bearer dXNlcjpwYXNz",
            "Proxy-Authorization: Basic ***",
            "Proxy-Authorization: Basic dXNlcg==",
            "Proxy-Authorization: Basic",
            "Authorization: Basic dXNlcjpwYXNz",
        ] {
            let request = head(&format!("GET http://example.org/ HTTP/1.1\r\n{line}"));
            assert_eq!(request.credentials(), None, "{line}");
        }
    }

    fn upstream() -> ProxyEndpoint {
        ProxyEndpoint {
            id: ProxyId::new(),
            host: "198.51.100.7".to_owned(),
            port: 8080,
            username: Secret::new("provider"),
            password: Secret::new("secret"),
            scheme: ProxyScheme::Http,
        }
    }

    #[test]
    fn a_head_goes_upstream_as_it_came_with_the_providers_credentials() {
        let request = head(
            "GET http://example.org/a?b=1 HTTP/1.1\r\nHost: example.org\r\n\
             proxy-authorization: Basic cGVyc29uYTp0b2tlbg==\r\nAccept: */*\r\n\
             Proxy-Connection: keep-alive",
        );

        let rewritten = String::from_utf8(request.rewritten(&upstream())).unwrap();

        assert_eq!(
            rewritten,
            "GET http://example.org/a?b=1 HTTP/1.1\r\nHost: example.org\r\nAccept: */*\r\n\
             Proxy-Connection: keep-alive\r\n\
             Proxy-Authorization: Basic cHJvdmlkZXI6c2VjcmV0\r\n\r\n"
        );
        // The persona's own credential never goes upstream.
        assert!(!rewritten.contains("cGVyc29uYTp0b2tlbg"));
    }

    #[test]
    fn a_body_is_framed_by_its_length_or_in_chunks() {
        let framed =
            |lines: &str| head(&format!("POST http://example.org/ HTTP/1.1\r\n{lines}")).body();

        assert_eq!(framed("Content-Length: 12"), Body::Length(12));
        assert_eq!(framed("content-length: 0"), Body::None);
        assert_eq!(framed("Content-Length: many"), Body::None);
        assert_eq!(framed("Accept: */*"), Body::None);
        assert_eq!(framed("Transfer-Encoding: chunked"), Body::Chunked);
        assert_eq!(framed("Transfer-Encoding: gzip, Chunked"), Body::Chunked);
        // Chunked wins over a length, as the protocol says.
        assert_eq!(
            framed("Content-Length: 5\r\nTransfer-Encoding: chunked"),
            Body::Chunked
        );
        assert_eq!(framed("Transfer-Encoding: gzip"), Body::None);
    }

    #[test]
    fn a_target_is_an_authority_or_a_plain_http_url() {
        let target = |line: &str| {
            target_of(&head(line)).map(|(url, host, port)| (url.to_string(), host, port))
        };

        assert_eq!(
            target("CONNECT Example.org:443 HTTP/1.1"),
            Some((
                "http://example.org:443/".to_owned(),
                "example.org".to_owned(),
                443
            ))
        );
        assert_eq!(
            target("CONNECT [2001:db8::1]:8443 HTTP/1.1").map(|(_, host, port)| (host, port)),
            Some(("[2001:db8::1]".to_owned(), 8443))
        );
        assert_eq!(
            target("GET http://example.org/path?x=1 HTTP/1.1").map(|(_, host, port)| (host, port)),
            Some(("example.org".to_owned(), 80))
        );
        // A tunnel without a port, with a path, or to nothing; a request
        // that is not for a proxy; a scheme a proxy is not asked for.
        for refused in [
            "CONNECT example.org HTTP/1.1",
            "CONNECT example.org:443/path HTTP/1.1",
            "CONNECT example.org:443?x HTTP/1.1",
            "CONNECT :443 HTTP/1.1",
            "GET / HTTP/1.1",
            "GET https://example.org/ HTTP/1.1",
            "GET ftp://example.org/ HTTP/1.1",
            "OPTIONS * HTTP/1.1",
        ] {
            assert_eq!(target(refused), None, "{refused}");
        }
    }

    #[test]
    fn bytes_count_toward_the_domain_being_fetched_or_as_idle() {
        let meter = Arc::new(Meter::default());
        let (one, other) = (PersonaId::new(), PersonaId::new());

        meter.add(one, 100, 1000);
        {
            let _fetching = meter.label(one, "example.org");
            meter.add(one, 10, 20);
            meter.add(one, 1, 2);
            // Another persona's bytes are its own.
            meter.add(other, 5, 6);
        }
        meter.add(one, 7, 0);
        meter.add(one, 0, 0);

        let mut expected = vec![
            Counted {
                persona: one,
                domain: "(idle)".to_owned(),
                bytes_up: 107,
                bytes_down: 1000,
            },
            Counted {
                persona: one,
                domain: "example.org".to_owned(),
                bytes_up: 11,
                bytes_down: 22,
            },
            Counted {
                persona: other,
                domain: "(idle)".to_owned(),
                bytes_up: 5,
                bytes_down: 6,
            },
        ];
        expected.sort_by(|a, b| (a.persona, &a.domain).cmp(&(b.persona, &b.domain)));
        assert_eq!(meter.drain(), expected);
        // Drained is forgotten; the totals are since the start.
        assert_eq!(meter.drain(), []);
        assert_eq!(meter.totals(), (123, 1028));
    }

    #[test]
    fn refusals_have_stable_names() {
        let names: Vec<&str> = [
            RouteRefusal::UnknownPersona,
            RouteRefusal::PersonaNotActive,
            RouteRefusal::ProxyNotUsable,
            RouteRefusal::Unavailable,
        ]
        .iter()
        .map(|refusal| refusal.as_str())
        .collect();

        assert_eq!(
            names,
            [
                "unknown_persona",
                "persona_not_active",
                "proxy_not_usable",
                "unavailable"
            ]
        );
    }
}
