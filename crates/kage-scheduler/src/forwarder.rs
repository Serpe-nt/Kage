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
//!
//! Since 2.0 (spec §25) two more kinds of route pass here:
//!
//! - **A plain client** (level 0) presents a credential that names no
//!   persona, only its exit: `plain-host`, or `plain-<proxy id>` for the
//!   one proxy of the pool that is the site's plain exit. Its bytes belong
//!   to no persona and count toward the host they went to.
//! - **No upstream.** A route to the host's own connection (a host persona
//!   under `egress.mode: direct`, or a plain client under
//!   `egress.plain: host`) has no proxy to resolve names: the forwarder
//!   resolves the target itself, refuses it when any of its addresses lies
//!   in a range the guard refuses (unless the operator named it in
//!   `egress.direct.private_names`; link-local, metadata and multicast
//!   addresses are refused for every name), connects to an address it
//!   checked and to no other, answers a `CONNECT` itself and sends an
//!   absolute-form request on in origin form. The stops of the pool (a
//!   leak, the provider's budget) do not hold it: it uses neither.
//!
//! Under `security.resolve_on_host` (F57, the security review's G1) every
//! name relayed through a proxy is looked up here too, as the http path
//! looks up its hops: a name with an address in a range the guard refuses,
//! or with no address the host can find, is refused before the proxy is
//! asked for it, and the refusal is kept for the caller like any other of
//! the guard's. The proxy resolves the name again where it connects (G2,
//! accepted). Each name is looked up once while its answer is kept
//! ([`NAME_KEPT`]); a page's many connections to one host wait for the
//! one lookup.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use hmac::{Hmac, KeyInit, Mac};
use kage_core::domain::{Exit, IDLE_DOMAIN, Persona, ProxyEndpoint, ProxyScheme};
use kage_core::ids::{PersonaId, ProxyId};
use kage_core::secret::Secret;
use kage_core::ssrf::{DeniedRange, SsrfPolicy, check_url, classify_ip};
use kage_http::HopResolver;
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

/// What the token of a plain client's exit is made from, besides the key.
const PLAIN_TOKEN_LABEL: &[u8] = b"kage-forwarder-plain-token-v1";

/// The user name of a plain client begins with this. Never a colon: a
/// Basic credential ends its user name at the first one.
const PLAIN_PREFIX: &str = "plain-";

/// Hexadecimal characters of a token: 128 bits.
const TOKEN_CHARS: usize = 32;

/// How long the forwarder keeps the answer of a name it looked up (F57).
/// The system's resolver does not say a name's TTL, so the forwarder keeps
/// every answer for less than a browser keeps one in its own host cache (a
/// minute in Chromium) and leaves longer caching to the resolver beneath
/// it, which honours the TTL. The answer decides what the forwarder lets
/// through, never where a proxy connects: the proxy resolves again (G2).
/// Only an answer with addresses is kept: a lookup that failed or found no
/// address is asked again by the next connection.
pub const NAME_KEPT: Duration = Duration::from_secs(30);

/// How many names' answers are kept at most.
const NAMES_KEPT: usize = 4096;

/// How long one lookup may take; a name without an answer by then has no
/// address.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a plain client's traffic leaves (spec §25): the host's own
/// connection, or the one proxy of the pool that is a site's plain exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlainRoute {
    /// The host's own connection.
    Host,
    /// This proxy of the pool.
    Pool(ProxyId),
}

impl PlainRoute {
    /// The user name a plain client of this route presents.
    pub fn username(self) -> String {
        match self {
            Self::Host => format!("{PLAIN_PREFIX}host"),
            Self::Pool(proxy) => format!("{PLAIN_PREFIX}{}", proxy.as_uuid()),
        }
    }

    fn parse(rest: &str) -> Option<Self> {
        if rest == "host" {
            return Some(Self::Host);
        }
        Uuid::parse_str(rest)
            .ok()
            .map(|uuid| Self::Pool(ProxyId::from_uuid(uuid)))
    }

    /// The connection the route's bytes leave through.
    pub const fn exit(self) -> Exit {
        match self {
            Self::Host => Exit::Host,
            Self::Pool(_) => Exit::Pool,
        }
    }
}

/// Who presented a credential to the forwarder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Caller {
    /// A persona's browser or http client.
    Persona(PersonaId),
    /// A plain client of level 0, by its exit.
    Plain(PlainRoute),
}

impl Caller {
    /// The persona, when it is one's.
    pub const fn persona(self) -> Option<PersonaId> {
        match self {
            Self::Persona(persona) => Some(persona),
            Self::Plain(_) => None,
        }
    }
}

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

    /// The token of a plain client's exit.
    pub fn plain_token(&self, route: PlainRoute) -> String {
        let mut text = Hmac::<Sha256>::new_from_slice(&self.key).map_or_else(
            |_| String::new(),
            |mut mac| {
                mac.update(PLAIN_TOKEN_LABEL);
                match route {
                    PlainRoute::Host => mac.update(b"host"),
                    PlainRoute::Pool(proxy) => mac.update(proxy.as_uuid().as_bytes()),
                }
                hex(&mac.finalize().into_bytes())
            },
        );
        text.truncate(TOKEN_CHARS);
        text
    }

    /// The persona a credential names, when the token is that persona's.
    pub fn verify(&self, username: &str, password: &str) -> Option<PersonaId> {
        match self.verify_caller(username, password)? {
            Caller::Persona(persona) => Some(persona),
            Caller::Plain(_) => None,
        }
    }

    /// Who a credential names: a persona or a plain client's exit, when the
    /// token is theirs.
    pub fn verify_caller(&self, username: &str, password: &str) -> Option<Caller> {
        let (caller, expected) = if let Some(rest) = username.strip_prefix(PLAIN_PREFIX) {
            let route = PlainRoute::parse(rest)?;
            (Caller::Plain(route), self.plain_token(route))
        } else {
            let persona = PersonaId::from_uuid(Uuid::parse_str(username).ok()?);
            (Caller::Persona(persona), self.token(persona))
        };
        // Compared without stopping at the first difference.
        let equal = expected.len() == password.len()
            && expected
                .bytes()
                .zip(password.bytes())
                .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
                == 0;
        (equal && !expected.is_empty()).then_some(caller)
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

impl ForwarderEgress {
    /// The forwarder as this process's plain client reaches it, under the
    /// credential of `route`. The endpoint's id is the route's proxy, or
    /// the nil id for the host.
    pub fn plain_endpoint(&self, route: PlainRoute) -> ProxyEndpoint {
        let id = match route {
            PlainRoute::Host => ProxyId::from_uuid(Uuid::nil()),
            PlainRoute::Pool(proxy) => proxy,
        };
        ProxyEndpoint {
            id,
            host: self.local.ip().to_string(),
            port: self.local.port(),
            username: Secret::new(route.username()),
            password: Secret::new(self.tokens.plain_token(route)),
            scheme: ProxyScheme::Http,
        }
    }
}

impl ForwarderEgress {
    /// The forwarder as a client that leaves `via` reaches it, under the
    /// credential of `persona`, for the proxy `upstream`: what a persona's
    /// browser or http client is handed, and what the stand-in of a proxy's
    /// self-test is handed (F105), which has an id and no persona row.
    pub fn endpoint_for(&self, persona: PersonaId, upstream: ProxyId, via: Via) -> ProxyEndpoint {
        let (host, port) = match via {
            Via::Browser => self.advertised.clone(),
            Via::Http => (self.local.ip().to_string(), self.local.port()),
        };
        ProxyEndpoint {
            id: upstream,
            host,
            port,
            username: Secret::new(persona.to_string()),
            password: Secret::new(self.tokens.token(persona)),
            scheme: ProxyScheme::Http,
        }
    }
}

impl Egress for ForwarderEgress {
    fn endpoint(&self, persona: &Persona, upstream: &ProxyEndpoint, via: Via) -> ProxyEndpoint {
        self.endpoint_for(persona.id, upstream.id, via)
    }
}

// --- Routes ----------------------------------------------------------------

/// Why a persona's traffic is not relayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteRefusal {
    /// There is no such persona.
    UnknownPersona,
    /// The route is one the configuration does not have: a plain client
    /// of the host under `egress.mode: proxies`, or of the pool under
    /// `direct`.
    NotConfigured,
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
            Self::NotConfigured => "not_configured",
            Self::PersonaNotActive => "persona_not_active",
            Self::ProxyNotUsable => "proxy_not_usable",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Where the forwarder connects for a caller.
#[derive(Debug, Clone)]
pub enum Upstream {
    /// A proxy of the pool, with the provider's credentials.
    Proxy(ProxyEndpoint),
    /// The host's own connection: no proxy.
    Direct,
}

impl Upstream {
    /// The connection the bytes leave through.
    pub const fn exit(&self) -> Exit {
        match self {
            Self::Proxy(_) => Exit::Pool,
            Self::Direct => Exit::Host,
        }
    }
}

/// Finds the one way a caller's traffic may leave.
#[async_trait]
pub trait Routes: Send + Sync + fmt::Debug {
    /// The persona's own proxy with the provider's credentials, or the
    /// host's connection for a host persona.
    async fn upstream(&self, persona: PersonaId) -> Result<Upstream, RouteRefusal>;

    /// The exit of a plain client: the host's connection, or the proxy it
    /// names when that proxy can carry traffic.
    async fn plain(&self, route: PlainRoute) -> Result<Upstream, RouteRefusal>;
}

/// Resolves names with the system's resolver, for the routes that have no
/// proxy to resolve them.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemResolver;

#[async_trait]
impl HopResolver for SystemResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        let found = tokio::net::lookup_host((host, 0))
            .await
            .map_err(|error| error.to_string())?;
        let mut addresses: Vec<IpAddr> = found.map(|address| address.ip()).collect();
        addresses.dedup();
        Ok(addresses)
    }
}

/// One lookup of a name, shared by everyone who asks while it is kept.
type Lookup = Arc<tokio::sync::OnceCell<Result<Vec<IpAddr>, String>>>;

/// The forwarder's lookups: one per name while its answer is kept
/// ([`NAME_KEPT`]), however many connections ask for the name at once.
#[derive(Debug)]
struct Names {
    resolver: Arc<dyn HopResolver>,
    kept: Mutex<HashMap<String, (Instant, Lookup)>>,
}

impl Names {
    fn new(resolver: Arc<dyn HopResolver>) -> Self {
        Self {
            resolver,
            kept: Mutex::new(HashMap::new()),
        }
    }

    /// The addresses of `host` (lower case), looked up now or kept.
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        let lookup = {
            let now = Instant::now();
            let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
            match kept.get(host) {
                Some((at, lookup)) if now.duration_since(*at) < NAME_KEPT => Arc::clone(lookup),
                _ => {
                    if kept.len() >= NAMES_KEPT {
                        kept.retain(|_, (at, _)| now.duration_since(*at) < NAME_KEPT);
                        if kept.len() >= NAMES_KEPT {
                            kept.clear();
                        }
                    }
                    let lookup = Lookup::default();
                    kept.insert(host.to_owned(), (now, Arc::clone(&lookup)));
                    lookup
                }
            }
        };
        let answer = lookup
            .get_or_init(|| async {
                tokio::time::timeout(LOOKUP_TIMEOUT, self.resolver.resolve(host))
                    .await
                    .unwrap_or_else(|_| Err(format!("no answer within {LOOKUP_TIMEOUT:?}")))
            })
            .await
            .clone();
        // Only an answer with addresses is kept. A lookup that failed, ran
        // out of time or found no address is the answer of the connections
        // that waited for it, and the next connection asks again at once:
        // one hiccup of the resolver must not refuse a name for every
        // caller while it is kept, as the http path's own check never keeps
        // a failure either. A name that keeps failing costs one lookup per
        // connection, which the resolver beneath caches by its own rules.
        if !matches!(&answer, Ok(addresses) if !addresses.is_empty()) {
            let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
            if kept
                .get(host)
                .is_some_and(|(_, held)| Arc::ptr_eq(held, &lookup))
            {
                kept.remove(host);
            }
        }
        answer
    }
}

// --- The meter -------------------------------------------------------------

/// Bytes one persona, or the plain client, exchanged through one exit
/// while one domain was being fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counted {
    /// The persona; `None` for a plain client.
    pub persona: Option<PersonaId>,
    /// The connection the bytes left through.
    pub egress: Exit,
    /// The domain being fetched, or `(idle)`.
    pub domain: String,
    /// Bytes sent towards the proxy.
    pub bytes_up: u64,
    /// Bytes received from the proxy.
    pub bytes_down: u64,
}

/// Whose bytes the meter counts: a persona's, or a plain client's toward
/// the host it went to, and through which exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payer {
    /// The persona; `None` for a plain client.
    pub persona: Option<PersonaId>,
    /// The connection the bytes leave through.
    pub egress: Exit,
    /// The domain a plain client's connection goes to; a persona's bytes
    /// count toward its label instead.
    pub domain: Option<String>,
}

type MeterKey = (Option<PersonaId>, Exit, String);

/// A caller's connection to one target: the caller, the target's host in
/// lower case, and its port.
type Unrelayed = (Caller, String, u16);

/// How long a connection the forwarder could not relay is remembered:
/// longer than any attempt waits for its page.
const UNRELAYED_KEPT: Duration = Duration::from_mins(30);

#[derive(Debug, Default)]
struct MeterState {
    labels: HashMap<PersonaId, String>,
    bytes: HashMap<MeterKey, (u64, u64)>,
    /// When each caller's connection to each target was last not relayed
    /// because its proxy failed.
    unrelayed: HashMap<Unrelayed, Instant>,
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

    /// Adds bytes of a persona through the pool.
    pub fn add(&self, persona: PersonaId, up: u64, down: u64) {
        self.add_for(
            &Payer {
                persona: Some(persona),
                egress: Exit::Pool,
                domain: None,
            },
            up,
            down,
        );
    }

    /// Adds bytes of whoever pays for them.
    pub fn add_for(&self, payer: &Payer, up: u64, down: u64) {
        if up == 0 && down == 0 {
            return;
        }
        let mut state = self.state();
        let domain = payer
            .domain
            .clone()
            .or_else(|| {
                payer
                    .persona
                    .and_then(|persona| state.labels.get(&persona).cloned())
            })
            .unwrap_or_else(|| IDLE_DOMAIN.to_owned());
        let entry = state
            .bytes
            .entry((payer.persona, payer.egress, domain))
            .or_default();
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
            .map(
                |((persona, egress, domain), (bytes_up, bytes_down))| Counted {
                    persona,
                    egress,
                    domain,
                    bytes_up,
                    bytes_down,
                },
            )
            .collect();
        counted.sort_by(|a, b| {
            (a.persona, a.egress, &a.domain).cmp(&(b.persona, b.egress, &b.domain))
        });
        counted
    }

    /// Notes that a caller's connection to `host`:`port` was not relayed
    /// because its proxy failed: the proxy could not be reached, did not
    /// answer, refused the provider's credential, or is not usable. A
    /// page of that target that fails meanwhile without an answer failed
    /// at the proxy, not at the site (F108). A proxy that answered a
    /// tunnel with a refusal of its own (a `502` for a target it could not
    /// reach) and a client that went away are not the proxy's failure.
    pub fn note_unrelayed(&self, caller: Caller, host: &str, port: u16) {
        let now = Instant::now();
        let mut state = self.state();
        state
            .unrelayed
            .retain(|_, at| now.duration_since(*at) < UNRELAYED_KEPT);
        state
            .unrelayed
            .insert((caller, host.to_ascii_lowercase(), port), now);
    }

    /// Whether a connection of `caller` to one of `targets` (host and
    /// port) was not relayed because its proxy failed, at `since` or later.
    pub fn unrelayed_since<'a>(
        &self,
        caller: Caller,
        targets: impl IntoIterator<Item = (&'a str, u16)>,
        since: Instant,
    ) -> bool {
        let state = self.state();
        targets.into_iter().any(|(host, port)| {
            state
                .unrelayed
                .get(&(caller, host.to_ascii_lowercase(), port))
                .is_some_and(|at| *at >= since)
        })
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

    /// The head as it goes to the target itself, with no proxy between: in
    /// origin form, without anything addressed to a proxy.
    fn origin_form(&self, url: &Url) -> Vec<u8> {
        let mut path = url.path().to_owned();
        if let Some(query) = url.query() {
            path.push('?');
            path.push_str(query);
        }
        let mut text = format!("{} {} {}\r\n", self.method, path, self.version);
        for line in &self.lines {
            let name = header_name(line);
            if !name.eq_ignore_ascii_case("proxy-authorization")
                && !name.eq_ignore_ascii_case("proxy-connection")
            {
                text.push_str(line);
                text.push_str("\r\n");
            }
        }
        text.push_str("\r\n");
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
    /// Names, lower case, that may resolve to a private or loopback
    /// address on a route with no upstream (`egress.direct.private_names`).
    pub private_names: HashSet<String>,
    /// How a route with no upstream resolves its targets, and every route
    /// when `resolve_names` is on.
    pub resolver: Arc<dyn HopResolver>,
    /// Whether every name relayed through a proxy is looked up here too
    /// and refused for an address in a denied range or for having none
    /// (`security.resolve_on_host`, F57). A route with no upstream always
    /// looks its names up.
    pub resolve_names: bool,
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
            private_names: config
                .egress
                .direct
                .private_names
                .iter()
                .filter_map(|name| kage_core::policy::normalize_host(name))
                .collect(),
            resolver: Arc::new(SystemResolver),
            resolve_names: config.security.resolve_on_host,
        }
    }
}

/// Why a target's name was refused for its addresses: on a route with no
/// upstream, or on every route under `resolve_names`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AddressRefusal {
    /// An address of the name lies in a range the guard refuses.
    Denied(String),
    /// The name has no address, or could not be looked up.
    Unresolved(String),
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
    /// A route with no upstream found no address it may connect to.
    Address(String),
    /// The persona's proxy could not be reached, did not answer, or
    /// refused the provider's credential: the proxy's own failure.
    Upstream(String),
    /// The target could not be reached (a proxy's refusal of the tunnel,
    /// no address that answered), or the client went away: not the
    /// proxy's own failure.
    Unreached(String),
}

// --- What the guard refused ------------------------------------------------

/// How many refusals are kept per caller: the latest of them.
const DENIALS_KEPT: usize = 32;

/// One target the forwarder refused by the SSRF guard (F102).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    /// Its place in the order of every refusal the forwarder recorded.
    pub seq: u64,
    /// The host the caller asked for, lower case, as it was asked for.
    pub host: String,
    /// Why the guard refused it.
    pub reason: String,
}

/// The targets the forwarder refused by the SSRF guard, per caller (F102).
///
/// A refusal looks like a network that does not answer to whoever asked
/// (a page must not read a response Kage wrote), so a browser or an http
/// client cannot say that the guard stopped it. The attempt executor can:
/// it takes a mark before an attempt and asks afterwards whether the
/// forwarder refused one of the hosts the attempt was going to. Only the
/// guard's refusals are kept: a target refused by name or range, and under
/// `egress.mode: direct` a name refused for one of its addresses. A name
/// that does not resolve, a stop or a route that is not usable are not the
/// guard's.
#[derive(Debug, Default)]
pub struct Denials {
    next: AtomicU64,
    kept: Mutex<HashMap<Caller, VecDeque<Denial>>>,
}

impl Denials {
    /// The mark of this moment: what is recorded afterwards comes after it.
    pub fn mark(&self) -> u64 {
        self.next.load(Ordering::SeqCst)
    }

    /// Records a refusal of `host` for `caller`.
    pub fn record(&self, caller: Caller, host: &str, reason: &str) {
        let seq = self.next.fetch_add(1, Ordering::SeqCst);
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        let list = kept.entry(caller).or_default();
        if list.len() >= DENIALS_KEPT {
            list.pop_front();
        }
        list.push_back(Denial {
            seq,
            host: host.trim_end_matches('.').to_ascii_lowercase(),
            reason: reason.to_owned(),
        });
    }

    /// The first refusal for `caller` since `mark` of one of `hosts`.
    pub fn since(&self, caller: Caller, mark: u64, hosts: &[String]) -> Option<Denial> {
        let kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        kept.get(&caller)?
            .iter()
            .find(|denial| {
                denial.seq >= mark
                    && hosts.iter().any(|host| {
                        host.trim_end_matches('.')
                            .eq_ignore_ascii_case(&denial.host)
                    })
            })
            .cloned()
    }
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
    /// What the guard refused, per caller.
    denials: Arc<Denials>,
    /// The names looked up, and their answers while they are kept.
    names: Names,
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
            names: Names::new(Arc::clone(&settings.resolver)),
            settings,
            tokens: Arc::new(tokens),
            routes,
            meter: Arc::new(Meter::default()),
            stop: Mutex::new(None),
            local,
            refused: AtomicU64::new(0),
            relayed: AtomicU64::new(0),
            denials: Arc::new(Denials::default()),
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

    /// What the SSRF guard refused, per caller (F102): the attempt
    /// executor reads it to tell a refused target from a failed network.
    pub const fn denials(&self) -> &Arc<Denials> {
        &self.denials
    }

    /// The egress that hands every persona this forwarder.
    pub fn egress(&self) -> ForwarderEgress {
        ForwarderEgress {
            tokens: Arc::clone(&self.tokens),
            advertised: self.settings.advertised.clone(),
            local: self.local,
        }
    }

    /// Refuses every new connection through the pool for a reason (a leak,
    /// the provider's bandwidth cap), or relays again with `None`. A route
    /// with no upstream uses neither and is not stopped.
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
        let Some(caller) = head
            .credentials()
            .and_then(|(username, password)| self.tokens.verify_caller(&username, &password))
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
        if let Err(reason) = self.check_target(&url, &host, port) {
            self.denials.record(caller, &host, &reason);
            return refuse(client, tunnel, Refused::Target(reason)).await;
        }
        let route = match caller {
            Caller::Persona(persona) => self.routes.upstream(persona).await,
            Caller::Plain(route) => self.routes.plain(route).await,
        };
        let upstream = match route {
            Ok(upstream) => upstream,
            Err(refusal) => {
                if refusal == RouteRefusal::ProxyNotUsable {
                    self.meter.note_unrelayed(caller, &host, port);
                }
                return refuse(client, tunnel, Refused::Route(refusal)).await;
            }
        };
        let payer = Payer {
            persona: caller.persona(),
            egress: upstream.exit(),
            // A plain client's connection counts toward the host it goes to.
            domain: matches!(caller, Caller::Plain(_)).then(|| host.clone()),
        };
        let upstream = match upstream {
            Upstream::Proxy(upstream) => upstream,
            Upstream::Direct => {
                return self
                    .direct(client, head, buffer, caller, &payer, (&host, port))
                    .await;
            }
        };
        if let Some(reason) = self.stopped() {
            return refuse(client, tunnel, Refused::Stopped(reason)).await;
        }
        if let Err(refused) = self.screen(caller, &host, port).await {
            return refuse(client, tunnel, refused).await;
        }
        let connecting = TcpStream::connect((upstream.host.as_str(), upstream.port));
        let mut server = match tokio::time::timeout(self.settings.connect_timeout, connecting).await
        {
            Ok(Ok(server)) => server,
            Ok(Err(error)) => {
                self.meter.note_unrelayed(caller, &host, port);
                return refuse(client, tunnel, Refused::Upstream(error.to_string())).await;
            }
            Err(_) => {
                self.meter.note_unrelayed(caller, &host, port);
                let reason = Refused::Upstream("the proxy did not answer".to_owned());
                return refuse(client, tunnel, reason).await;
            }
        };
        let _ = server.set_nodelay(true);
        tracing::trace!(?caller, %host, port, tunnel = head.is_connect(), "relaying");

        if head.is_connect() {
            let tunnelled = self
                .tunnel(client, &mut server, &head, buffer, &payer, &upstream)
                .await;
            if matches!(tunnelled, Err(Refused::Upstream(_))) {
                self.meter.note_unrelayed(caller, &host, port);
            }
            tunnelled
        } else {
            self.plain(
                client,
                server,
                head,
                buffer,
                caller,
                &payer,
                Some(&upstream),
            )
            .await;
            Ok(())
        }
    }

    /// A route with no upstream: resolves the target, checks every address,
    /// connects to a checked one, and relays a tunnel or plain requests.
    async fn direct(
        &self,
        client: &mut TcpStream,
        head: Head,
        buffer: Vec<u8>,
        caller: Caller,
        payer: &Payer,
        (host, port): (&str, u16),
    ) -> Result<(), Refused> {
        let tunnel = head.is_connect();
        let private = self.settings.private_names.contains(host);
        let addresses = match self.addresses(host, port, private).await {
            Ok(addresses) => addresses,
            Err(refusal) => {
                let reason = self.refused_address(caller, host, refusal);
                return refuse(client, tunnel, Refused::Address(reason)).await;
            }
        };
        let mut server = None;
        for address in addresses {
            let connecting = TcpStream::connect(SocketAddr::new(address, port));
            if let Ok(Ok(connected)) =
                tokio::time::timeout(self.settings.connect_timeout, connecting).await
            {
                server = Some(connected);
                break;
            }
        }
        let Some(mut server) = server else {
            let reason = Refused::Unreached(format!("no address of {host} answered"));
            return refuse(client, tunnel, reason).await;
        };
        let _ = server.set_nodelay(true);
        tracing::trace!(?caller, %host, port, tunnel, "relaying with no upstream");
        if !tunnel {
            self.plain(client, server, head, buffer, caller, payer, None)
                .await;
            return Ok(());
        }
        // The forwarder is the proxy here: it opens the tunnel itself.
        let answer: &[u8] = b"HTTP/1.1 200 Connection established\r\n\r\n";
        let failed = |error: std::io::Error| Refused::Unreached(error.to_string());
        client.write_all(answer).await.map_err(failed)?;
        self.relayed.fetch_add(1, Ordering::Relaxed);
        if !buffer.is_empty() {
            server.write_all(&buffer).await.map_err(failed)?;
            self.meter.add_for(payer, buffer.len() as u64, 0);
        }
        self.pump_both(client, &mut server, payer).await;
        Ok(())
    }

    /// Records a refusal of `host` for its addresses as the guard's, and
    /// says why. A name that resolves into a denied range always is the
    /// guard's refusal; a name with no address only under `resolve_names`,
    /// where a name the host cannot look up counts as one with no address
    /// (F98) and is never asked of a proxy that might know it. Otherwise it
    /// is a failed network.
    fn refused_address(&self, caller: Caller, host: &str, refusal: AddressRefusal) -> String {
        match refusal {
            AddressRefusal::Denied(reason) => {
                self.denials.record(caller, host, &reason);
                reason
            }
            AddressRefusal::Unresolved(reason) => {
                if self.settings.resolve_names {
                    self.denials.record(caller, host, &reason);
                }
                reason
            }
        }
    }

    /// The address check of a name relayed through a proxy, under
    /// `resolve_names` (F57): the name is looked up here, and one with an
    /// address in a denied range, or with none, is refused before the
    /// proxy is asked for it. Only the operator's own self-test hosts may
    /// resolve to a private or loopback address; `egress.direct.private_names`
    /// is for the host's own connection and does not reach the pool.
    /// Without the setting the proxy resolves and nothing is looked up.
    async fn screen(&self, caller: Caller, host: &str, port: u16) -> Result<(), Refused> {
        if !self.settings.resolve_names {
            return Ok(());
        }
        match self.addresses(host, port, false).await {
            Ok(_) => Ok(()),
            Err(refusal) => {
                let reason = self.refused_address(caller, host, refusal);
                tracing::info!(
                    host,
                    port,
                    %reason,
                    "the forwarder refused a name by its addresses"
                );
                Err(Refused::Address(reason))
            }
        }
    }

    /// The addresses of a target that the forwarder may let a client reach:
    /// every address the name has must be one the guard allows, or a
    /// private or loopback one of a name the operator listed (`private`)
    /// or of the operator's own self-test hosts; link-local, metadata and
    /// multicast addresses never. A name the host cannot look up has no
    /// address.
    async fn addresses(
        &self,
        host: &str,
        port: u16,
        private: bool,
    ) -> Result<Vec<IpAddr>, AddressRefusal> {
        let operator = self
            .settings
            .operator_hosts
            .contains(&(host.to_owned(), port));
        let named = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .ok();
        let addresses = match named {
            Some(address) => vec![address],
            None => self.names.resolve(host).await.map_err(|reason| {
                AddressRefusal::Unresolved(format!("{host} could not be looked up: {reason}"))
            })?,
        };
        if addresses.is_empty() {
            return Err(AddressRefusal::Unresolved(format!("{host} has no address")));
        }
        let private = operator || private;
        for address in &addresses {
            match classify_ip(*address) {
                None => {}
                Some(
                    DeniedRange::Loopback
                    | DeniedRange::Private
                    | DeniedRange::Cgnat
                    | DeniedRange::UniqueLocal,
                ) if private => {}
                Some(range) => {
                    return Err(AddressRefusal::Denied(format!(
                        "{host} resolves to {address}, in {range:?}"
                    )));
                }
            }
        }
        Ok(addresses)
    }

    /// Relays both directions of an open tunnel, counting.
    async fn pump_both(&self, client: &mut TcpStream, server: &mut TcpStream, payer: &Payer) {
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
                self.meter.add_for(payer, bytes, 0);
            },
        );
        let down = pump(
            &mut server_read,
            &mut client_write,
            idle,
            &active,
            |bytes| {
                self.meter.add_for(payer, 0, bytes);
            },
        );
        tokio::join!(up, down);
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
    /// `Refused::Upstream` only when the proxy itself failed: it could not
    /// be written to, did not answer, or refused the credential (`407`).
    async fn tunnel(
        &self,
        client: &mut TcpStream,
        server: &mut TcpStream,
        head: &Head,
        early: Vec<u8>,
        payer: &Payer,
        upstream: &ProxyEndpoint,
    ) -> Result<(), Refused> {
        let request = head.rewritten(upstream);
        let failed = |error: std::io::Error| Refused::Upstream(error.to_string());
        let gone = |error: std::io::Error| Refused::Unreached(error.to_string());
        server.write_all(&request).await.map_err(failed)?;
        self.meter.add_for(payer, request.len() as u64, 0);

        let mut answer = Vec::new();
        let status = tokio::time::timeout(
            self.settings.connect_timeout,
            read_status(server, &mut answer),
        )
        .await
        .map_err(|_| Refused::Upstream("the proxy did not answer the tunnel".to_owned()))?
        .map_err(failed)?;
        self.meter.add_for(payer, 0, answer.len() as u64);
        // The proxy's own answer goes to the client as it came: a refusal
        // of the proxy is a failed tunnel to the browser.
        client.write_all(&answer).await.map_err(gone)?;
        if status == 407 {
            return Err(Refused::Upstream(
                "the proxy refused the provider's credential".to_owned(),
            ));
        }
        if !(200..300).contains(&status) {
            // A proxy that could not reach the target says so (a `502`, a
            // `504`): the target's failure, not the proxy's.
            return Err(Refused::Unreached(format!(
                "the proxy answered the tunnel with {status}"
            )));
        }
        self.relayed.fetch_add(1, Ordering::Relaxed);
        if !early.is_empty() {
            server.write_all(&early).await.map_err(gone)?;
            self.meter.add_for(payer, early.len() as u64, 0);
        }
        self.pump_both(client, server, payer).await;
        Ok(())
    }

    /// Plain `http://` requests: every request head of the connection gets
    /// the provider's credentials (or, with no upstream, goes on in origin
    /// form to the one target the connection was opened to) and passes the
    /// guard; answers are relayed as they come.
    #[allow(clippy::too_many_arguments)] // One connection and what it is for.
    #[allow(clippy::too_many_lines)] // Both directions of one connection.
    async fn plain(
        &self,
        client: &mut TcpStream,
        mut server: TcpStream,
        first: Head,
        mut buffer: Vec<u8>,
        caller: Caller,
        payer: &Payer,
        upstream: Option<&ProxyEndpoint>,
    ) {
        // With no upstream the connection reaches one target, and only
        // requests for that target may follow on it.
        let opened_for = target_of(&first).map(|(_, host, port)| (host, port));
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
                self.meter.add_for(payer, 0, bytes);
            },
        );
        let up = async {
            let mut head = first;
            loop {
                let request = match (upstream, target_of(&head)) {
                    (Some(upstream), _) => head.rewritten(upstream),
                    (None, Some((url, _, _))) => head.origin_form(&url),
                    (None, None) => break,
                };
                if server_write.write_all(&request).await.is_err() {
                    break;
                }
                self.meter.add_for(payer, request.len() as u64, 0);
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
                    Ok(bytes) => self.meter.add_for(payer, bytes, 0),
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
                let same_caller = next.credentials().and_then(|(username, password)| {
                    self.tokens.verify_caller(&username, &password)
                }) == Some(caller);
                let allowed = same_caller
                    && !next.is_connect()
                    && (upstream.is_none() || self.stopped().is_none())
                    && self
                        .allows_next(&next, caller, upstream.is_some(), opened_for.as_ref())
                        .await;
                if !allowed {
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

    /// Whether a later request of a kept plain connection of `caller` may
    /// go on: its target passes the guard (a refusal is kept for the
    /// caller), and through a proxy its name passes the address check
    /// under `resolve_names`; with no upstream it is the one target the
    /// connection was opened to and checked for.
    async fn allows_next(
        &self,
        next: &Head,
        caller: Caller,
        through_proxy: bool,
        opened_for: Option<&(String, u16)>,
    ) -> bool {
        let Some((url, host, port)) = target_of(next) else {
            return false;
        };
        if let Err(reason) = self.check_target(&url, &host, port) {
            self.denials.record(caller, &host, &reason);
            return false;
        }
        if through_proxy {
            self.screen(caller, &host, port).await.is_ok()
        } else {
            opened_for == Some(&(host, port))
        }
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

    use kage_core::domain::{Exit, ProxyEndpoint, ProxyScheme};
    use kage_core::ids::{PersonaId, ProxyId};
    use kage_core::secret::Secret;

    use super::{
        Body, Caller, Counted, Head, Meter, Payer, PlainRoute, RouteRefusal, Tokens, base64_decode,
        base64_encode, target_of,
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
    fn a_plain_clients_credential_names_its_exit_and_survives_basic_auth() {
        let tokens = Tokens::new(&[7; 32]);
        let proxy = ProxyId::new();
        let routes = [PlainRoute::Host, PlainRoute::Pool(proxy)];
        assert_eq!(routes[0].username(), "plain-host");
        assert_eq!(routes[1].username(), format!("plain-{}", proxy.as_uuid()));

        for route in routes {
            let token = tokens.plain_token(route);
            assert_eq!(token.len(), 32);
            // A Basic credential ends the user name at its first colon.
            let username = route.username();
            assert!(!username.contains(':'), "{username}");
            let encoded = base64_encode(format!("{username}:{token}").as_bytes());
            let decoded = String::from_utf8(base64_decode(&encoded).unwrap()).unwrap();
            let (user, password) = decoded.split_once(':').unwrap();
            assert_eq!(
                tokens.verify_caller(user, password),
                Some(Caller::Plain(route))
            );
            // Nobody's persona.
            assert_eq!(tokens.verify(user, password), None);
        }
        // One exit's token is not another's, nor a persona's.
        let host = tokens.plain_token(PlainRoute::Host);
        assert_eq!(tokens.verify_caller(&routes[1].username(), &host), None);
        let persona = PersonaId::new();
        assert_eq!(
            tokens.verify_caller("plain-host", &tokens.token(persona)),
            None
        );
        assert_eq!(tokens.verify_caller(&persona.to_string(), &host), None);
        assert_eq!(tokens.verify_caller("plain-nobody", &host), None);
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

    #[tokio::test(start_paused = true)]
    async fn the_meter_says_whose_connection_was_not_relayed_and_since_when() {
        let meter = Meter::default();
        let (one, other) = (PersonaId::new(), PersonaId::new());
        let before = tokio::time::Instant::now();
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        let site = [("shop.kage.test", 80)];
        assert!(!meter.unrelayed_since(Caller::Persona(one), site, before));

        meter.note_unrelayed(Caller::Persona(one), "Shop.Kage.Test", 80);
        assert!(meter.unrelayed_since(Caller::Persona(one), site, before));
        // Another target of the same caller is its own: a third party's
        // host, or the site on another port.
        let others = [("cdn.kage.test", 443), ("shop.kage.test", 443)];
        assert!(!meter.unrelayed_since(Caller::Persona(one), others, before));
        // One of the attempt's targets is enough: a redirect's host.
        let chain = [("cdn.kage.test", 443), ("shop.kage.test", 80)];
        assert!(meter.unrelayed_since(Caller::Persona(one), chain, before));
        // Another caller's route is its own.
        assert!(!meter.unrelayed_since(Caller::Persona(other), site, before));
        let exit = Caller::Plain(PlainRoute::Pool(ProxyId::new()));
        assert!(!meter.unrelayed_since(exit, site, before));
        meter.note_unrelayed(exit, "shop.kage.test", 80);
        assert!(meter.unrelayed_since(exit, site, before));
        // An attempt that began after it saw nothing of it.
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        let later = tokio::time::Instant::now();
        assert!(!meter.unrelayed_since(Caller::Persona(one), site, later));
        // What is older than any attempt is forgotten at the next note.
        tokio::time::advance(super::UNRELAYED_KEPT).await;
        meter.note_unrelayed(exit, "other.kage.test", 80);
        assert_eq!(meter.state().unrelayed.len(), 1);
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
        // A plain client's bytes are nobody's, counted by their exit
        // toward the host they went to.
        for egress in [Exit::Host, Exit::Pool] {
            meter.add_for(
                &Payer {
                    persona: None,
                    egress,
                    domain: Some("example.net".to_owned()),
                },
                3,
                4,
            );
        }

        let counted = |persona, egress, domain: &str, bytes_up, bytes_down| Counted {
            persona,
            egress,
            domain: domain.to_owned(),
            bytes_up,
            bytes_down,
        };
        let mut expected = vec![
            counted(Some(one), Exit::Pool, "(idle)", 107, 1000),
            counted(Some(one), Exit::Pool, "example.org", 11, 22),
            counted(Some(other), Exit::Pool, "(idle)", 5, 6),
            counted(None, Exit::Host, "example.net", 3, 4),
            counted(None, Exit::Pool, "example.net", 3, 4),
        ];
        expected.sort_by(|a, b| {
            (a.persona, a.egress, &a.domain).cmp(&(b.persona, b.egress, &b.domain))
        });
        assert_eq!(meter.drain(), expected);
        // Drained is forgotten; the totals are since the start.
        assert_eq!(meter.drain(), []);
        assert_eq!(meter.totals(), (129, 1036));
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
