//! The forwarder on real sockets (follow-up F10): a persona's client
//! reaches its own proxy and no other, the provider's credentials are
//! added on the way, every byte is counted, and what must not be relayed
//! is not. Since 2.0 (spec §25) also a plain client's exit, and the route
//! with no upstream, which resolves and checks its targets itself.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::proxy::{PROXY_PASS, PROXY_USER, TestProxy, basic};
use common::{config, id, persona};
use kage_core::domain::{Exit, IDLE_DOMAIN, ProxyEndpoint, ProxyScheme};
use kage_core::ids::{PersonaId, ProxyId};
use kage_http::HopResolver;
use kage_persona::{Egress, Via};
use kage_scheduler::forwarder::RouteRefusal;
use kage_scheduler::{Forwarder, ForwarderSettings, PlainRoute, Routes, Tokens, Upstream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: [u8; 32] = [7; 32];

/// Which proxy each persona's traffic leaves through, and each plain
/// client's.
#[derive(Debug, Default)]
struct Table {
    routes: Mutex<HashMap<PersonaId, Result<Upstream, RouteRefusal>>>,
    plain: Mutex<Vec<(PlainRoute, Result<Upstream, RouteRefusal>)>>,
}

impl Table {
    fn set(&self, persona: PersonaId, route: Result<ProxyEndpoint, RouteRefusal>) {
        self.routes
            .lock()
            .unwrap()
            .insert(persona, route.map(Upstream::Proxy));
    }

    /// The persona leaves from the host's own connection.
    fn set_direct(&self, persona: PersonaId) {
        self.routes
            .lock()
            .unwrap()
            .insert(persona, Ok(Upstream::Direct));
    }

    fn set_plain(&self, route: PlainRoute, upstream: Result<Upstream, RouteRefusal>) {
        self.plain.lock().unwrap().push((route, upstream));
    }
}

#[async_trait]
impl Routes for Table {
    async fn upstream(&self, persona: PersonaId) -> Result<Upstream, RouteRefusal> {
        self.routes
            .lock()
            .unwrap()
            .get(&persona)
            .cloned()
            .unwrap_or(Err(RouteRefusal::UnknownPersona))
    }

    async fn plain(&self, route: PlainRoute) -> Result<Upstream, RouteRefusal> {
        self.plain
            .lock()
            .unwrap()
            .iter()
            .find(|(known, _)| *known == route)
            .map_or(Err(RouteRefusal::NotConfigured), |(_, upstream)| {
                upstream.clone()
            })
    }
}

/// Names the forwarder resolves for a route with no upstream, from a
/// table: `shop.kage.test` and `other.kage.test` are this machine, one
/// name is the cloud's metadata address, and one has a public and a
/// private address.
#[derive(Debug)]
struct Names(HashMap<&'static str, Vec<IpAddr>>);

impl Default for Names {
    fn default() -> Self {
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        Self(HashMap::from([
            ("shop.kage.test", vec![loopback]),
            ("other.kage.test", vec![loopback]),
            (
                "metadata.kage.test",
                vec![IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))],
            ),
            (
                "mixed.kage.test",
                vec![
                    IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
                    IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7)),
                ],
            ),
        ]))
    }
}

#[async_trait]
impl HopResolver for Names {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        self.0
            .get(host)
            .cloned()
            .ok_or_else(|| format!("{host} is unknown"))
    }
}

/// A forwarder on a free loopback port, in front of one provider proxy
/// that persona 0 leaves through.
struct Rig {
    forwarder: Arc<Forwarder>,
    table: Arc<Table>,
    provider: TestProxy,
    tokens: Tokens,
    stop: Option<oneshot::Sender<()>>,
    serving: Option<tokio::task::JoinHandle<()>>,
}

fn settings() -> ForwarderSettings {
    ForwarderSettings {
        advertised: ("kage".to_owned(), 0),
        connect_timeout: Duration::from_secs(2),
        idle_timeout: Duration::from_secs(5),
        max_connections: 64,
        operator_hosts: HashSet::new(),
        private_names: HashSet::new(),
        resolver: Arc::new(Names::default()),
    }
}

impl Rig {
    async fn start(settings: ForwarderSettings) -> Self {
        let provider = TestProxy::start().await;
        let table = Arc::new(Table::default());
        table.set(id(0), Ok(provider.endpoint()));
        let routes: Arc<dyn Routes> = table.clone();
        let (forwarder, listener) = Forwarder::bind(
            "127.0.0.1:0".parse().unwrap(),
            settings,
            Tokens::new(&KEY),
            routes,
        )
        .await
        .unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let serving = tokio::spawn(Arc::clone(&forwarder).serve(listener, async move {
            let _ = stopped.await;
        }));
        Self {
            forwarder,
            table,
            provider,
            tokens: Tokens::new(&KEY),
            stop: Some(stop),
            serving: Some(serving),
        }
    }

    /// The `Proxy-Authorization` of a persona's clients.
    fn credential(&self, persona: PersonaId) -> String {
        basic(&persona.to_string(), &self.tokens.token(persona))
    }

    /// The `Proxy-Authorization` of a plain client of `route`.
    fn plain_credential(&self, route: PlainRoute) -> String {
        basic(&route.username(), &self.tokens.plain_token(route))
    }

    /// A client that asked for a tunnel with `credential`, and the head
    /// of the answer.
    async fn tunnel_with(&self, credential: &str, authority: &str) -> (Client, String) {
        let mut client = self.client().await;
        client
            .send(&format!(
                "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Authorization: {credential}\r\n\r\n"
            ))
            .await;
        let head = client.head().await;
        (client, head)
    }

    async fn client(&self) -> Client {
        Client::connect(self.forwarder.local_addr()).await
    }

    /// A client that asked for a tunnel to `authority` as `persona`, and
    /// the head of the answer.
    async fn tunnel(&self, persona: PersonaId, authority: &str) -> (Client, String) {
        let mut client = self.client().await;
        client
            .send(&format!(
                "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Authorization: {}\r\n\r\n",
                self.credential(persona)
            ))
            .await;
        let head = client.head().await;
        (client, head)
    }

    async fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(serving) = self.serving.take() {
            serving.await.unwrap();
        }
    }
}

/// A client on a raw socket: what it reads is exactly what was sent to it.
struct Client {
    stream: TcpStream,
    sent: usize,
    received: Vec<u8>,
    /// Bytes of `received` already handed out.
    taken: usize,
}

impl Client {
    async fn connect(to: SocketAddr) -> Self {
        Self {
            stream: TcpStream::connect(to).await.unwrap(),
            sent: 0,
            received: Vec::new(),
            taken: 0,
        }
    }

    async fn send(&mut self, text: &str) {
        self.stream.write_all(text.as_bytes()).await.unwrap();
        self.sent += text.len();
    }

    /// Reads more; `false` at the end of the connection or after three
    /// seconds of nothing.
    async fn more(&mut self) -> bool {
        let mut chunk = [0_u8; 8192];
        let read = tokio::time::timeout(Duration::from_secs(3), self.stream.read(&mut chunk)).await;
        match read {
            Ok(Ok(read)) if read > 0 => {
                self.received.extend_from_slice(&chunk[..read]);
                true
            }
            _ => false,
        }
    }

    /// The next response head, with the empty line that ends it; what was
    /// received when the connection ended first.
    async fn head(&mut self) -> String {
        loop {
            let unread = &self.received[self.taken..];
            if let Some(end) = unread.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&unread[..end + 4]).into_owned();
                self.taken += end + 4;
                return head;
            }
            if !self.more().await {
                let rest = String::from_utf8_lossy(&self.received[self.taken..]).into_owned();
                self.taken = self.received.len();
                return rest;
            }
        }
    }

    /// Everything up to and including `needle`.
    async fn until(&mut self, needle: &str) -> String {
        loop {
            let unread = &self.received[self.taken..];
            if let Some(at) = unread
                .windows(needle.len())
                .position(|window| window == needle.as_bytes())
            {
                let text = String::from_utf8_lossy(&unread[..at + needle.len()]).into_owned();
                self.taken += at + needle.len();
                return text;
            }
            let more = self.more().await;
            assert!(
                more,
                "the connection ended before {needle:?}: {:?}",
                String::from_utf8_lossy(&self.received[self.taken..])
            );
        }
    }

    /// Everything until the connection ends.
    async fn rest(&mut self) -> String {
        while self.more().await {}
        let rest = String::from_utf8_lossy(&self.received[self.taken..]).into_owned();
        self.taken = self.received.len();
        rest
    }
}

async fn shop() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello from the shop"))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_tunnel_goes_through_the_personas_own_proxy_and_every_byte_is_counted() {
    let mut rig = Rig::start(settings()).await;
    let shop = shop().await;
    rig.provider.route("shop.kage.test:443", *shop.address());
    let fetching = rig.forwarder.meter().label(id(0), "shop.kage.test");

    let (mut client, head) = rig.tunnel(id(0), "shop.kage.test:443").await;

    assert_eq!(head, "HTTP/1.1 200 Connection Established\r\n\r\n");
    client
        .send("GET /page HTTP/1.1\r\nHost: shop.kage.test\r\nConnection: close\r\n\r\n")
        .await;
    let answer = client.rest().await;
    assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
    assert!(answer.ends_with("hello from the shop"), "{answer}");

    // The provider saw its own credentials, never the persona's token.
    let tunnels = rig.provider.tunnels();
    assert_eq!(tunnels.len(), 1);
    assert_eq!(tunnels[0].target, "shop.kage.test:443");
    assert_eq!(
        tunnels[0].header("proxy-authorization"),
        Some(basic(PROXY_USER, PROXY_PASS))
    );
    assert!(!tunnels[0].head.contains(&rig.tokens.token(id(0))));
    assert_eq!(rig.provider.refused(), 0);

    // What the forwarder counted is what the provider would bill: every
    // byte it received from Kage and every byte it sent back.
    drop(fetching);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let counted = rig.forwarder.meter().drain();
    assert_eq!(counted.len(), 1, "{counted:?}");
    assert_eq!(counted[0].persona, Some(id(0)));
    assert_eq!(counted[0].egress, Exit::Pool);
    assert_eq!(counted[0].domain, "shop.kage.test");
    assert_eq!(
        (counted[0].bytes_up, counted[0].bytes_down),
        rig.provider.bytes()
    );
    assert_eq!(counted[0].bytes_down, client.received.len() as u64);
    assert!(counted[0].bytes_up > 100 && counted[0].bytes_down > 100);
    assert_eq!(rig.forwarder.counts(), (1, 0));
    assert_eq!(
        rig.forwarder.meter().totals(),
        (counted[0].bytes_up, counted[0].bytes_down)
    );
    rig.stop().await;
}

#[tokio::test]
async fn bytes_outside_a_fetch_count_as_idle() {
    let mut rig = Rig::start(settings()).await;
    let shop = shop().await;
    rig.provider.route("shop.kage.test:443", *shop.address());

    // A browser's own background traffic: nobody is fetching anything.
    let (mut client, _) = rig.tunnel(id(0), "shop.kage.test:443").await;
    client
        .send("GET /page HTTP/1.1\r\nHost: shop.kage.test\r\nConnection: close\r\n\r\n")
        .await;
    client.rest().await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let counted = rig.forwarder.meter().drain();
    assert_eq!(counted.len(), 1, "{counted:?}");
    assert_eq!(counted[0].domain, IDLE_DOMAIN);
    assert_eq!(
        (counted[0].bytes_up, counted[0].bytes_down),
        rig.provider.bytes()
    );
    rig.stop().await;
}

#[tokio::test]
async fn a_client_without_a_personas_credential_is_asked_for_one() {
    let mut rig = Rig::start(settings()).await;
    let other = rig.tokens.token(id(1));
    let attempts = [
        String::new(),
        format!("Proxy-Authorization: {}\r\n", basic(PROXY_USER, PROXY_PASS)),
        // Another persona's token under this persona's name.
        format!(
            "Proxy-Authorization: {}\r\n",
            basic(&id(0).to_string(), &other)
        ),
        format!("Proxy-Authorization: {}\r\n", basic(&id(0).to_string(), "")),
        format!("Proxy-Authorization: Bearer {other}\r\n"),
        "Proxy-Authorization: Basic !!!not-base64!!!\r\n".to_owned(),
    ];

    for credential in &attempts {
        let mut client = rig.client().await;
        client
            .send(&format!(
                "CONNECT shop.kage.test:443 HTTP/1.1\r\nHost: shop.kage.test:443\r\n{credential}\r\n"
            ))
            .await;

        let answer = client.rest().await;

        assert!(
            answer.starts_with("HTTP/1.1 407 Proxy Authentication Required\r\n"),
            "{credential:?}: {answer}"
        );
        assert!(answer.contains("Proxy-Authenticate: Basic realm=\"proxy\"\r\n"));
    }
    // Nothing reached the provider and nothing was counted.
    assert_eq!(rig.provider.seen().len(), 0);
    assert_eq!(rig.provider.refused(), 0);
    assert_eq!(rig.forwarder.meter().totals(), (0, 0));
    assert_eq!(rig.forwarder.counts(), (0, attempts.len() as u64));
    rig.stop().await;
}

#[tokio::test]
async fn what_the_guard_refuses_is_not_relayed() {
    let mut rig = Rig::start(settings()).await;
    let denied = [
        "localhost:8080",
        "127.0.0.1:9222",
        "10.0.0.5:443",
        "192.168.1.1:80",
        "169.254.169.254:80",
        "[::1]:8080",
        // The names of the compose network.
        "sidecar-chromium:8080",
        "postgres:5432",
        "kage.internal:443",
        "printer.local:631",
        "shop.kage.test:0",
    ];

    for authority in denied {
        let (mut client, head) = rig.tunnel(id(0), authority).await;

        // A bare refusal: to a browser, a connection that failed.
        assert_eq!(
            head, "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
            "{authority}"
        );
        assert_eq!(client.rest().await, "", "{authority}");
    }
    // A plain request for such a target gets no answer at all: no page
    // ever reads a response the forwarder wrote.
    for url in [
        "http://localhost:8080/",
        "http://127.0.0.1/admin",
        "http://sidecar-firefox:8080/v1/contexts",
        "http://user:secret@shop.kage.test/",
    ] {
        let mut client = rig.client().await;
        client
            .send(&format!(
                "GET {url} HTTP/1.1\r\nHost: x\r\nProxy-Authorization: {}\r\n\r\n",
                rig.credential(id(0))
            ))
            .await;
        assert_eq!(client.rest().await, "", "{url}");
    }
    assert_eq!(rig.provider.seen().len(), 0);
    assert_eq!(rig.forwarder.counts(), (0, denied.len() as u64 + 4));

    // A public address by number is a target like any other: a page may
    // load from one.
    let shop = shop().await;
    rig.provider.route("93.184.216.34:443", *shop.address());
    let (_client, head) = rig.tunnel(id(0), "93.184.216.34:443").await;
    assert_eq!(head, "HTTP/1.1 200 Connection Established\r\n\r\n");
    rig.stop().await;
}

#[tokio::test]
async fn the_operators_own_self_test_hosts_pass_the_guard_by_name_and_port() {
    let mut rig = Rig::start(ForwarderSettings {
        operator_hosts: HashSet::from([("echo".to_owned(), 8443)]),
        ..settings()
    })
    .await;
    let shop = shop().await;
    for authority in ["echo:8443", "ECHO:8443", "echo:9000"] {
        rig.provider.route(authority, *shop.address());
    }

    let (_client, head) = rig.tunnel(id(0), "echo:8443").await;
    assert_eq!(head, "HTTP/1.1 200 Connection Established\r\n\r\n");
    // In any spelling of the name.
    let (_client, head) = rig.tunnel(id(0), "ECHO:8443").await;
    assert_eq!(head, "HTTP/1.1 200 Connection Established\r\n\r\n");

    // Only that port.
    let (_client, head) = rig.tunnel(id(0), "echo:9000").await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    rig.stop().await;
}

#[tokio::test]
async fn a_persona_that_may_not_send_is_not_relayed() {
    let mut rig = Rig::start(settings()).await;
    let refusals = [
        RouteRefusal::UnknownPersona,
        RouteRefusal::PersonaNotActive,
        RouteRefusal::ProxyNotUsable,
        RouteRefusal::Unavailable,
    ];

    for (number, refusal) in (1_u8..).zip(refusals) {
        rig.table.set(id(number), Err(refusal));

        let (mut client, head) = rig.tunnel(id(number), "shop.kage.test:443").await;

        assert!(head.starts_with("HTTP/1.1 403 "), "{refusal:?}: {head}");
        assert_eq!(client.rest().await, "");
    }
    // A persona nobody ever heard of holds a valid token all the same:
    // the token only says who is asking.
    let (_client, head) = rig.tunnel(id(77), "shop.kage.test:443").await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    assert_eq!(rig.provider.seen().len(), 0);
    assert_eq!(rig.forwarder.counts(), (0, 5));
    rig.stop().await;
}

#[tokio::test]
async fn each_persona_leaves_through_its_own_proxy_only() {
    let mut rig = Rig::start(settings()).await;
    let second = TestProxy::start().await;
    rig.table.set(id(1), Ok(second.endpoint()));
    let shop = shop().await;
    rig.provider.route("shop.kage.test:443", *shop.address());
    second.route("shop.kage.test:443", *shop.address());

    let (_first, head) = rig.tunnel(id(0), "shop.kage.test:443").await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    let (_other, head) = rig.tunnel(id(1), "shop.kage.test:443").await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    let (_again, head) = rig.tunnel(id(1), "shop.kage.test:443").await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");

    assert_eq!(rig.provider.tunnels().len(), 1);
    assert_eq!(second.tunnels().len(), 2);
    rig.stop().await;
}

#[tokio::test]
async fn while_stopped_no_new_connection_is_relayed() {
    let mut rig = Rig::start(settings()).await;
    let shop = shop().await;
    rig.provider.route("shop.kage.test:443", *shop.address());
    let (mut open, _) = rig.tunnel(id(0), "shop.kage.test:443").await;

    rig.forwarder.set_stop(Some("egress_unsafe"));

    let (mut client, head) = rig.tunnel(id(0), "shop.kage.test:443").await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    assert_eq!(client.rest().await, "");
    let mut plain = rig.client().await;
    plain
        .send(&format!(
            "GET http://shop.kage.test/page HTTP/1.1\r\nHost: shop.kage.test\r\nProxy-Authorization: {}\r\n\r\n",
            rig.credential(id(0))
        ))
        .await;
    assert_eq!(plain.rest().await, "");
    assert_eq!(rig.provider.seen().len(), 1);

    // A tunnel that was open is not cut: closing browsers is what ends
    // those.
    open.send("GET /page HTTP/1.1\r\nHost: shop.kage.test\r\nConnection: close\r\n\r\n")
        .await;
    assert!(open.rest().await.ends_with("hello from the shop"));

    rig.forwarder.set_stop(None);
    let (_client, head) = rig.tunnel(id(0), "shop.kage.test:443").await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    rig.stop().await;
}

#[tokio::test]
async fn a_plain_request_is_relayed_with_the_providers_credentials() {
    let mut rig = Rig::start(settings()).await;
    let shop = shop().await;
    rig.provider.route("shop.kage.test:80", *shop.address());
    let mut client = rig.client().await;

    client
        .send(&format!(
            "GET http://shop.kage.test/page HTTP/1.1\r\nHost: shop.kage.test\r\nAccept: text/html\r\nProxy-Authorization: {}\r\nProxy-Connection: keep-alive\r\n\r\n",
            rig.credential(id(0))
        ))
        .await;
    let answer = client.rest().await;

    assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
    assert!(answer.ends_with("hello from the shop"), "{answer}");
    let plain = rig.provider.plain();
    assert_eq!(plain.len(), 1);
    assert_eq!(plain[0].target, "http://shop.kage.test/page");
    // The head went on as the client wrote it, line for line, with the
    // provider's credential in place of the persona's.
    assert_eq!(
        plain[0].head,
        format!(
            "GET http://shop.kage.test/page HTTP/1.1\r\nHost: shop.kage.test\r\nAccept: text/html\r\nProxy-Connection: keep-alive\r\nProxy-Authorization: {}",
            basic(PROXY_USER, PROXY_PASS)
        )
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let counted = rig.forwarder.meter().drain();
    assert_eq!(
        (counted[0].bytes_up, counted[0].bytes_down),
        rig.provider.bytes()
    );
    assert_eq!(counted[0].bytes_down, client.received.len() as u64);
    rig.stop().await;
}

/// Every request a [`KeepAlive`] proxy saw: its head and its body.
type Requests = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// A provider proxy that keeps a connection and answers every request on
/// it, so that several requests of one connection can be told apart.
struct KeepAlive {
    addr: SocketAddr,
    seen: Requests,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for KeepAlive {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl KeepAlive {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Requests = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn({
            let seen = Arc::clone(&seen);
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(Self::serve(stream, Arc::clone(&seen)));
                }
            }
        });
        Self { addr, seen, task }
    }

    fn endpoint(&self) -> ProxyEndpoint {
        ProxyEndpoint {
            id: ProxyId::new(),
            host: self.addr.ip().to_string(),
            port: self.addr.port(),
            username: kage_core::secret::Secret::new(PROXY_USER),
            password: kage_core::secret::Secret::new(PROXY_PASS),
            scheme: ProxyScheme::Http,
        }
    }

    async fn serve(mut stream: TcpStream, seen: Requests) {
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            let end = loop {
                if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                    break end + 4;
                }
                let mut chunk = [0_u8; 4096];
                match stream.read(&mut chunk).await {
                    Ok(read) if read > 0 => buffer.extend_from_slice(&chunk[..read]),
                    _ => return,
                }
            };
            let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
            buffer.drain(..end);
            let lower = head.to_ascii_lowercase();
            let length = lower
                .split("\r\n")
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok());
            let body = if lower.contains("transfer-encoding: chunked") {
                // Read to the last chunk and its empty trailer.
                loop {
                    if let Some(end) = buffer.windows(5).position(|window| window == b"0\r\n\r\n") {
                        break buffer.drain(..end + 5).collect();
                    }
                    let mut chunk = [0_u8; 4096];
                    match stream.read(&mut chunk).await {
                        Ok(read) if read > 0 => buffer.extend_from_slice(&chunk[..read]),
                        _ => return,
                    }
                }
            } else {
                let length = length.unwrap_or(0);
                while buffer.len() < length {
                    let mut chunk = [0_u8; 4096];
                    match stream.read(&mut chunk).await {
                        Ok(read) if read > 0 => buffer.extend_from_slice(&chunk[..read]),
                        _ => return,
                    }
                }
                buffer.drain(..length).collect::<Vec<u8>>()
            };
            let number = {
                let mut seen = seen.lock().unwrap();
                seen.push((head, body));
                seen.len()
            };
            let text = format!("answer {number}");
            let answer = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{text}",
                text.len()
            );
            if stream.write_all(answer.as_bytes()).await.is_err() {
                return;
            }
        }
    }
}

#[tokio::test]
async fn every_request_of_a_kept_connection_is_checked_and_relayed_whole() {
    let mut rig = Rig::start(settings()).await;
    let provider = KeepAlive::start().await;
    rig.table.set(id(0), Ok(provider.endpoint()));
    let credential = rig.credential(id(0));
    let mut client = rig.client().await;

    // A request with a body of a known length.
    client
        .send(&format!(
            "POST http://shop.kage.test/form HTTP/1.1\r\nHost: shop.kage.test\r\nContent-Length: 11\r\nProxy-Authorization: {credential}\r\n\r\nhello=world"
        ))
        .await;
    assert!(
        client
            .until("answer 1")
            .await
            .starts_with("HTTP/1.1 200 OK")
    );
    // One with a chunked body, and the next request right behind it in
    // the same write.
    client
        .send(&format!(
            "POST http://shop.kage.test/upload HTTP/1.1\r\nHost: shop.kage.test\r\nTransfer-Encoding: chunked\r\nProxy-Authorization: {credential}\r\n\r\n5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\nGET http://shop.kage.test/next HTTP/1.1\r\nHost: shop.kage.test\r\nProxy-Authorization: {credential}\r\n\r\n"
        ))
        .await;
    client.until("answer 2").await;
    client.until("answer 3").await;

    {
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(
            seen[0]
                .0
                .starts_with("POST http://shop.kage.test/form HTTP/1.1\r\n")
        );
        assert_eq!(seen[0].1, b"hello=world");
        assert_eq!(seen[1].1, b"5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\n\r\n");
        assert!(
            seen[2]
                .0
                .starts_with("GET http://shop.kage.test/next HTTP/1.1\r\n")
        );
        for (head, _) in seen.iter() {
            assert!(
                head.contains(&format!(
                    "Proxy-Authorization: {}\r\n",
                    basic(PROXY_USER, PROXY_PASS)
                )),
                "{head}"
            );
            assert!(!head.contains(&credential), "{head}");
        }
    }

    // A later request of the connection for a target the guard refuses
    // ends the connection without an answer.
    client
        .send(&format!(
            "GET http://localhost:8080/ HTTP/1.1\r\nHost: localhost\r\nProxy-Authorization: {credential}\r\n\r\n"
        ))
        .await;
    assert_eq!(client.rest().await, "");
    assert_eq!(provider.seen.lock().unwrap().len(), 3);

    // So does one under another credential, and a tunnel asked for inside
    // a plain connection.
    for second in [
        format!(
            "GET http://shop.kage.test/b HTTP/1.1\r\nHost: shop.kage.test\r\nProxy-Authorization: {}\r\n\r\n",
            rig.credential(id(1))
        ),
        format!(
            "CONNECT shop.kage.test:443 HTTP/1.1\r\nHost: shop.kage.test:443\r\nProxy-Authorization: {credential}\r\n\r\n"
        ),
    ] {
        let before = provider.seen.lock().unwrap().len();
        let mut client = rig.client().await;
        client
            .send(&format!(
                "GET http://shop.kage.test/a HTTP/1.1\r\nHost: shop.kage.test\r\nProxy-Authorization: {credential}\r\n\r\n"
            ))
            .await;
        client.until("answer").await;
        client.send(&second).await;
        let rest = client.rest().await;
        assert!(!rest.contains("HTTP/1.1"), "{rest}");
        assert_eq!(provider.seen.lock().unwrap().len(), before + 1);
    }
    rig.stop().await;
}

#[tokio::test]
async fn a_proxy_that_refuses_or_is_gone_is_a_failed_connection() {
    let mut rig = Rig::start(ForwarderSettings {
        connect_timeout: Duration::from_millis(400),
        ..settings()
    })
    .await;

    // The provider does not know the credentials Kage holds: its answer
    // reaches the client as it came, and no tunnel exists.
    rig.table
        .set(id(1), Ok(rig.provider.endpoint_with("stale", "stale")));
    let (mut client, head) = rig.tunnel(id(1), "shop.kage.test:443").await;
    assert!(
        head.starts_with("HTTP/1.1 407 Proxy Authentication Required\r\n"),
        "{head}"
    );
    assert!(head.contains("realm=\"provider\""), "{head}");
    assert_eq!(client.rest().await, "");

    // The provider cannot reach the target.
    let (_client, head) = rig.tunnel(id(0), "nowhere.kage.test:443").await;
    assert!(head.starts_with("HTTP/1.1 502 "), "{head}");

    // Nothing listens where the proxy should be.
    let gone = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed = gone.local_addr().unwrap();
    drop(gone);
    let mut endpoint = rig.provider.endpoint();
    endpoint.port = closed.port();
    rig.table.set(id(2), Ok(endpoint));
    let (_client, head) = rig.tunnel(id(2), "shop.kage.test:443").await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");

    // A proxy that accepts and then says nothing.
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut endpoint = rig.provider.endpoint();
    endpoint.port = silent.local_addr().unwrap().port();
    rig.table.set(id(3), Ok(endpoint));
    let holding = tokio::spawn(async move {
        let accepted = silent.accept().await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        drop(accepted);
    });
    let started = std::time::Instant::now();
    let (_client, head) = rig.tunnel(id(3), "shop.kage.test:443").await;
    assert_eq!(head, "");
    assert!(started.elapsed() < Duration::from_secs(2));
    holding.abort();

    assert_eq!(rig.forwarder.counts().0, 0);
    rig.stop().await;
}

#[tokio::test]
async fn what_is_not_a_request_to_a_proxy_gets_no_answer() {
    let mut rig = Rig::start(settings()).await;
    let credential = rig.credential(id(0));

    for request in [
        "HELLO\r\n\r\n".to_owned(),
        // A request to a web server, not to a proxy.
        format!("GET / HTTP/1.1\r\nHost: kage\r\nProxy-Authorization: {credential}\r\n\r\n"),
        format!(
            "GET https://shop.kage.test/ HTTP/1.1\r\nProxy-Authorization: {credential}\r\n\r\n"
        ),
        format!("CONNECT shop.kage.test HTTP/1.1\r\nProxy-Authorization: {credential}\r\n\r\n"),
        format!(
            "CONNECT shop.kage.test:443/path HTTP/1.1\r\nProxy-Authorization: {credential}\r\n\r\n"
        ),
        format!("GET http://shop.kage.test/ HTTP/2\r\nProxy-Authorization: {credential}\r\n\r\n"),
    ] {
        let mut client = rig.client().await;
        client.send(&request).await;
        assert_eq!(client.rest().await, "", "{request}");
    }
    // A client that connects and says nothing, then leaves.
    drop(rig.client().await);
    assert_eq!(rig.provider.seen().len(), 0);
    rig.stop().await;
}

#[tokio::test]
async fn no_more_connections_are_relayed_than_the_limit() {
    let mut rig = Rig::start(ForwarderSettings {
        max_connections: 1,
        ..settings()
    })
    .await;
    let shop = shop().await;
    rig.provider.route("shop.kage.test:443", *shop.address());
    let (open, head) = rig.tunnel(id(0), "shop.kage.test:443").await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");

    // Full: the next client sees a connection that closes.
    let (_client, head) = rig.tunnel(id(0), "shop.kage.test:443").await;
    assert_eq!(head, "");

    drop(open);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (_client, head) = rig.tunnel(id(0), "shop.kage.test:443").await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    rig.stop().await;
}

#[tokio::test]
async fn a_tunnel_on_which_nothing_moves_is_closed() {
    let mut rig = Rig::start(ForwarderSettings {
        idle_timeout: Duration::from_millis(300),
        ..settings()
    })
    .await;
    let shop = shop().await;
    rig.provider.route("shop.kage.test:443", *shop.address());
    let (mut client, head) = rig.tunnel(id(0), "shop.kage.test:443").await;
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");

    let started = std::time::Instant::now();
    assert_eq!(client.rest().await, "");

    let idle = started.elapsed();
    assert!(
        idle >= Duration::from_millis(250) && idle < Duration::from_secs(2),
        "{idle:?}"
    );
    rig.stop().await;
}

#[tokio::test]
async fn stopping_the_forwarder_drops_what_is_under_way() {
    let mut rig = Rig::start(settings()).await;
    let shop = shop().await;
    rig.provider.route("shop.kage.test:443", *shop.address());
    let (mut client, _) = rig.tunnel(id(0), "shop.kage.test:443").await;

    rig.stop().await;

    assert_eq!(client.rest().await, "");
    assert!(
        TcpStream::connect(rig.forwarder.local_addr())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn every_persona_is_handed_the_forwarder_under_its_own_credential() {
    let mut rig = Rig::start(settings()).await;
    let egress = rig.forwarder.egress();
    let upstream = rig.provider.endpoint();
    let bound = rig.forwarder.local_addr();

    let browser = egress.endpoint(&persona(4, false), &upstream, Via::Browser);
    let http = egress.endpoint(&persona(4, false), &upstream, Via::Http);

    // The sidecars reach it by its name on the compose network, this
    // process on loopback; a port that was not fixed is the bound one.
    assert_eq!(
        (browser.host.as_str(), browser.port),
        ("kage", bound.port())
    );
    assert_eq!((http.host.as_str(), http.port), ("127.0.0.1", bound.port()));
    for endpoint in [&browser, &http] {
        assert_eq!(endpoint.id, upstream.id);
        assert_eq!(endpoint.scheme, ProxyScheme::Http);
        assert_eq!(endpoint.username.expose(), id(4).to_string());
        assert_eq!(endpoint.password.expose().len(), 32);
        assert_eq!(
            rig.tokens
                .verify(endpoint.username.expose(), endpoint.password.expose()),
            Some(id(4))
        );
        // Nothing of the provider's is in it.
        assert_ne!(endpoint.username.expose(), PROXY_USER);
        assert_ne!(endpoint.password.expose(), PROXY_PASS);
    }
    let other = egress.endpoint(&persona(5, false), &upstream, Via::Browser);
    assert_ne!(other.password.expose(), browser.password.expose());
    rig.stop().await;
}

#[tokio::test]
async fn a_route_with_no_upstream_connects_only_to_addresses_it_checked() {
    let mut rig = Rig::start(ForwarderSettings {
        private_names: HashSet::from([
            "shop.kage.test".to_owned(),
            "metadata.kage.test".to_owned(),
            "mixed.kage.test".to_owned(),
        ]),
        ..settings()
    })
    .await;
    rig.table.set_plain(PlainRoute::Host, Ok(Upstream::Direct));
    rig.table.set_direct(id(1));
    let shop = shop().await;
    let port = shop.address().port();
    let host = rig.plain_credential(PlainRoute::Host);

    // A plain request is sent on in origin form, without the credential.
    let mut client = rig.client().await;
    client
        .send(&format!(
            "GET http://shop.kage.test:{port}/page HTTP/1.1\r\nHost: shop.kage.test:{port}\r\n\
             Proxy-Authorization: {host}\r\nConnection: close\r\n\r\n"
        ))
        .await;
    let answer = client.rest().await;
    assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
    assert!(answer.ends_with("hello from the shop"), "{answer}");
    let received = shop.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].url.path(), "/page");
    assert!(!received[0].headers.contains_key("proxy-authorization"));

    // The forwarder opens a tunnel itself, for a plain client and for a
    // host persona alike.
    let authority = format!("shop.kage.test:{port}");
    for credential in [host.clone(), rig.credential(id(1))] {
        let (mut client, head) = rig.tunnel_with(&credential, &authority).await;
        assert_eq!(head, "HTTP/1.1 200 Connection established\r\n\r\n");
        client
            .send("GET /page HTTP/1.1\r\nHost: shop.kage.test\r\nConnection: close\r\n\r\n")
            .await;
        let answer = client.rest().await;
        assert!(answer.ends_with("hello from the shop"), "{answer}");
    }
    // Nothing went through the pool.
    assert_eq!(rig.provider.seen().len(), 0);

    // A name the operator did not list may not resolve to this machine;
    // a listed one never to the metadata address, and not to a private
    // address beside a public one either unless it is listed.
    for refused in [
        format!("other.kage.test:{port}"),
        "metadata.kage.test:80".to_owned(),
        "unknown.kage.test:443".to_owned(),
    ] {
        let (mut client, head) = rig.tunnel_with(&host, &refused).await;
        assert!(head.starts_with("HTTP/1.1 403 "), "{refused}: {head}");
        assert_eq!(client.rest().await, "", "{refused}");
    }
    let (_client, head) = rig.tunnel_with(&host, "mixed.kage.test:9").await;
    assert!(!head.starts_with("HTTP/1.1 200 "), "{head}");

    // The bytes are counted by their exit: the plain client's toward the
    // host it went to, the host persona's as its own.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let counted = rig.forwarder.meter().drain();
    assert!(
        counted
            .iter()
            .all(|counted| counted.egress == Exit::Host && counted.bytes_down > 0),
        "{counted:?}"
    );
    let plain: Vec<_> = counted
        .iter()
        .filter(|counted| counted.persona.is_none())
        .collect();
    assert_eq!(plain.len(), 1, "{counted:?}");
    assert_eq!(plain[0].domain, "shop.kage.test");
    assert!(
        counted
            .iter()
            .any(|counted| counted.persona == Some(id(1)) && counted.domain == IDLE_DOMAIN)
    );

    // The stops of the pool do not hold the host's connection.
    rig.forwarder.set_stop(Some("bandwidth_exhausted"));
    let (_client, head) = rig.tunnel_with(&host, &authority).await;
    assert_eq!(head, "HTTP/1.1 200 Connection established\r\n\r\n");
    let (_client, head) = rig.tunnel(id(0), "shop.kage.test:443").await;
    assert!(head.starts_with("HTTP/1.1 403 "), "{head}");
    rig.stop().await;
}

#[tokio::test]
async fn a_plain_clients_credential_names_its_exit_and_nothing_else() {
    let mut rig = Rig::start(settings()).await;
    let shop = shop().await;
    rig.provider.route("shop.kage.test:443", *shop.address());
    let (exit, other) = (
        ProxyId::from_uuid(uuid::Uuid::from_u128(41)),
        ProxyId::from_uuid(uuid::Uuid::from_u128(42)),
    );
    rig.table.set_plain(
        PlainRoute::Pool(exit),
        Ok(Upstream::Proxy(rig.provider.endpoint())),
    );
    rig.table
        .set_plain(PlainRoute::Pool(other), Err(RouteRefusal::ProxyNotUsable));

    // The site's plain exit: through that proxy, with the provider's
    // credentials, counted toward the host for nobody.
    let credential = rig.plain_credential(PlainRoute::Pool(exit));
    let (mut client, head) = rig.tunnel_with(&credential, "shop.kage.test:443").await;
    assert_eq!(head, "HTTP/1.1 200 Connection Established\r\n\r\n");
    client
        .send("GET /page HTTP/1.1\r\nHost: shop.kage.test\r\nConnection: close\r\n\r\n")
        .await;
    assert!(client.rest().await.ends_with("hello from the shop"));
    let tunnels = rig.provider.tunnels();
    assert_eq!(tunnels.len(), 1);
    assert_eq!(
        tunnels[0].header("proxy-authorization"),
        Some(basic(PROXY_USER, PROXY_PASS))
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let counted = rig.forwarder.meter().drain();
    assert_eq!(counted.len(), 1, "{counted:?}");
    assert_eq!(
        (
            counted[0].persona,
            counted[0].egress,
            counted[0].domain.as_str()
        ),
        (None, Exit::Pool, "shop.kage.test")
    );

    // Another exit's token under this exit's name is no credential.
    let forged = basic(
        &PlainRoute::Pool(exit).username(),
        &rig.tokens.plain_token(PlainRoute::Pool(other)),
    );
    let (_client, head) = rig.tunnel_with(&forged, "shop.kage.test:443").await;
    assert!(head.starts_with("HTTP/1.1 407 "), "{head}");
    // Nor is a persona's token, nor a plain token under a persona's name.
    let as_persona = basic(&PlainRoute::Host.username(), &rig.tokens.token(id(0)));
    let (_client, head) = rig.tunnel_with(&as_persona, "shop.kage.test:443").await;
    assert!(head.starts_with("HTTP/1.1 407 "), "{head}");
    let as_plain = basic(
        &id(0).to_string(),
        &rig.tokens.plain_token(PlainRoute::Host),
    );
    let (_client, head) = rig.tunnel_with(&as_plain, "shop.kage.test:443").await;
    assert!(head.starts_with("HTTP/1.1 407 "), "{head}");

    // An exit that cannot carry traffic, and the host's connection where
    // the operator did not configure it, are refused.
    for route in [PlainRoute::Pool(other), PlainRoute::Host] {
        let credential = rig.plain_credential(route);
        let (_client, head) = rig.tunnel_with(&credential, "shop.kage.test:443").await;
        assert!(head.starts_with("HTTP/1.1 403 "), "{route:?}: {head}");
    }
    assert_eq!(rig.provider.tunnels().len(), 1);
    rig.stop().await;
}

#[test]
fn the_settings_come_from_the_configuration() {
    let mut config = config(&[
        ("KAGE__FORWARDER__ADVERTISE", "http://kage-host:9911"),
        ("KAGE__FORWARDER__CONNECT_TIMEOUT", "7s"),
        ("KAGE__FORWARDER__IDLE_TIMEOUT", "2m"),
        ("KAGE__FORWARDER__MAX_CONNECTIONS", "12"),
        ("KAGE__SELFTEST__HTTP__ECHO_URL", "https://Echo:8443/"),
    ]);
    config.egress.direct.private_names = vec!["Fixture.Kage.Test.".to_owned()];

    let settings = ForwarderSettings::from_config(&config);

    assert_eq!(settings.advertised, ("kage-host".to_owned(), 9911));
    assert_eq!(settings.connect_timeout, Duration::from_secs(7));
    assert_eq!(settings.idle_timeout, Duration::from_mins(2));
    assert_eq!(settings.max_connections, 12);
    assert!(settings.operator_hosts.contains(&("echo".to_owned(), 8443)));
    // A listed name in any spelling is its lower-case form.
    assert_eq!(
        settings.private_names,
        HashSet::from(["fixture.kage.test".to_owned()])
    );
}
