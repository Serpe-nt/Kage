//! Shared helpers for the integration tests of the http path:
//!
//! - [`TestProxy`]: a local HTTP proxy with Basic authentication. It
//!   forwards plain requests and opens `CONNECT` tunnels, resolves the test
//!   host names itself (as a real proxy resolves the target), and keeps the
//!   raw head of every request it saw, so a test can assert what went on
//!   the wire.
//! - [`TestEcho`]: the fingerprint echo with a throwaway certificate
//!   authority.
//! - [`ScriptedResolver`]: a resolver that answers from a table.
//!
//! Nothing here leaves the machine.

// Each test binary uses a different subset of these helpers.
#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::panic,
    clippy::struct_field_names,
    clippy::too_many_lines
)]

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kage_core::domain::{ClaimedOs, ProxyEndpoint, ProxyScheme};
use kage_core::ids::ProxyId;
use kage_core::secret::Secret;
use kage_http::echo::{EchoServer, EchoTls, Observation};
use kage_http::headers::IdentityOverride;
use kage_http::{ClientFactory, ClientSpec, HopResolver, HttpSettings, ProfileChoice};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The credentials the test proxy accepts.
pub const PROXY_USER: &str = "kage-test-user";
pub const PROXY_PASS: &str = "kage-test-pass-51";

/// The name the echo is reached by. A name, not an address: a client that
/// dials an address sends no server name, which changes its JA4.
pub const ECHO_HOST: &str = "echo.kage.test";

/// The same echo, offered HTTP/1.1 only.
pub const ECHO_H1_HOST: &str = "h1.echo.kage.test";

/// What Firefox on Linux really sends; wreq-util's own claims Ubuntu.
pub fn firefox_linux_user_agent(major: u32) -> String {
    format!("Mozilla/5.0 (X11; Linux x86_64; rv:{major}.0) Gecko/20100101 Firefox/{major}.0")
}

/// One request the proxy saw.
#[derive(Debug, Clone)]
pub struct Seen {
    /// `CONNECT`, `GET`, ...
    pub method: String,
    /// `host:port` for a tunnel, the absolute URL for a plain request.
    pub target: String,
    /// The request head exactly as the client sent it.
    pub head: String,
}

impl Seen {
    /// The header lines as `(name, value)`, in order, with the client's
    /// spelling.
    pub fn headers(&self) -> Vec<(String, String)> {
        self.head
            .split("\r\n")
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_owned(), value.trim().to_owned()))
            .collect()
    }

    /// The header names in order.
    pub fn header_names(&self) -> Vec<String> {
        self.headers().into_iter().map(|(name, _)| name).collect()
    }

    /// The first value of a header.
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers()
            .into_iter()
            .find(|(found, _)| found.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }
}

struct ProxyShared {
    expected: Mutex<String>,
    routes: Mutex<HashMap<String, SocketAddr>>,
    seen: Mutex<Vec<Seen>>,
    up: AtomicBool,
    refused: AtomicUsize,
    /// Milliseconds the proxy waits before it opens a tunnel or forwards.
    delay_ms: AtomicU64,
}

/// A local HTTP proxy with Basic authentication.
pub struct TestProxy {
    pub addr: SocketAddr,
    pub id: ProxyId,
    shared: Arc<ProxyShared>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let bytes = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let word = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if index <= chunk.len() {
                out.push(char::from(ALPHABET[((word >> shift) & 0x3f) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn basic(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64(format!("{username}:{password}").as_bytes())
    )
}

impl TestProxy {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(ProxyShared {
            expected: Mutex::new(basic(PROXY_USER, PROXY_PASS)),
            routes: Mutex::new(HashMap::new()),
            seen: Mutex::new(Vec::new()),
            up: AtomicBool::new(true),
            refused: AtomicUsize::new(0),
            delay_ms: AtomicU64::new(0),
        });
        let task = tokio::spawn({
            let shared = Arc::clone(&shared);
            async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let shared = Arc::clone(&shared);
                    tokio::spawn(async move {
                        let _ = serve(stream, &shared).await;
                    });
                }
            }
        });
        Self {
            addr,
            id: ProxyId::new(),
            shared,
            task,
        }
    }

    /// Makes `host:port` reachable through the proxy at a local address,
    /// the way a real proxy resolves a target's name.
    pub fn route(&self, authority: &str, to: SocketAddr) {
        self.shared
            .routes
            .lock()
            .unwrap()
            .insert(authority.to_owned(), to);
    }

    /// The proxy as the pool hands it to the http path.
    pub fn endpoint(&self) -> ProxyEndpoint {
        self.endpoint_with(PROXY_USER, PROXY_PASS)
    }

    pub fn endpoint_with(&self, username: &str, password: &str) -> ProxyEndpoint {
        ProxyEndpoint {
            id: self.id,
            host: self.addr.ip().to_string(),
            port: self.addr.port(),
            username: Secret::new(username),
            password: Secret::new(password),
            scheme: ProxyScheme::Http,
        }
    }

    /// Every request seen so far, tunnels included.
    pub fn seen(&self) -> Vec<Seen> {
        self.shared.seen.lock().unwrap().clone()
    }

    /// The plain (non-tunnel) requests seen so far.
    pub fn plain(&self) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|seen| seen.method != "CONNECT")
            .collect()
    }

    /// The tunnels opened so far.
    pub fn tunnels(&self) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|seen| seen.method == "CONNECT")
            .collect()
    }

    /// Requests refused for their credentials.
    pub fn refused(&self) -> usize {
        self.shared.refused.load(Ordering::SeqCst)
    }

    /// Takes the proxy down (connections are closed at once) or up again.
    pub fn set_up(&self, up: bool) {
        self.shared.up.store(up, Ordering::SeqCst);
    }

    /// Makes the proxy wait before it serves each request, as a slow
    /// proxy's tunnel setup does.
    pub fn set_delay(&self, delay: Duration) {
        self.shared
            .delay_ms
            .store(u64::try_from(delay.as_millis()).unwrap(), Ordering::SeqCst);
    }
}

async fn serve(mut client: TcpStream, shared: &ProxyShared) -> std::io::Result<()> {
    if !shared.up.load(Ordering::SeqCst) {
        return Ok(());
    }
    let mut buffer = Vec::new();
    let head_end = loop {
        let mut chunk = [0_u8; 4096];
        let read = client.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 64 * 1024 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let rest = buffer[head_end..].to_vec();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_owned();
    let headers: Vec<&str> = lines.filter(|line| !line.is_empty()).collect();
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(());
    };

    let authorization = headers.iter().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("proxy-authorization")
            .then(|| value.trim().to_owned())
    });
    if authorization.as_deref() != Some(shared.expected.lock().unwrap().as_str()) {
        shared.refused.fetch_add(1, Ordering::SeqCst);
        client
            .write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\n\
                  Proxy-Authenticate: Basic realm=\"test\"\r\n\
                  Content-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await?;
        return Ok(());
    }
    shared.seen.lock().unwrap().push(Seen {
        method: method.to_owned(),
        target: target.to_owned(),
        head: head.trim_end().to_owned(),
    });
    let delay = shared.delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    let route = |authority: &str| shared.routes.lock().unwrap().get(authority).copied();
    let bad_gateway = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n";

    if method.eq_ignore_ascii_case("CONNECT") {
        let Some(upstream) = route(target) else {
            client.write_all(bad_gateway).await?;
            return Ok(());
        };
        let Ok(mut upstream) = TcpStream::connect(upstream).await else {
            client.write_all(bad_gateway).await?;
            return Ok(());
        };
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        upstream.write_all(&rest).await?;
        tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
        return Ok(());
    }

    // A plain request in absolute form: http://host[:port]/path.
    let Some(without_scheme) = target.strip_prefix("http://") else {
        client
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            .await?;
        return Ok(());
    };
    let (authority, path) = match without_scheme.find('/') {
        Some(slash) => (&without_scheme[..slash], &without_scheme[slash..]),
        None => (without_scheme, "/"),
    };
    let authority = if authority.contains(':') {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    };
    let Some(upstream) = route(&authority) else {
        client.write_all(bad_gateway).await?;
        return Ok(());
    };
    let Ok(mut upstream) = TcpStream::connect(upstream).await else {
        client.write_all(bad_gateway).await?;
        return Ok(());
    };
    let mut forwarded = format!("{method} {path} HTTP/1.1\r\n");
    for line in headers {
        let name = line.split(':').next().unwrap_or_default();
        if name.eq_ignore_ascii_case("proxy-authorization")
            || name.eq_ignore_ascii_case("proxy-connection")
        {
            continue;
        }
        forwarded.push_str(line);
        forwarded.push_str("\r\n");
    }
    // One request per connection keeps the forwarding simple: later
    // requests of the client arrive on new connections and are seen too.
    forwarded.push_str("Connection: close\r\n\r\n");
    upstream.write_all(forwarded.as_bytes()).await?;
    upstream.write_all(&rest).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

/// The fingerprint echo on a loopback port, with a certificate from a
/// throwaway authority that the test clients trust.
pub struct TestEcho {
    pub addr: SocketAddr,
    /// The authority's certificate, PEM.
    pub ca_pem: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestEcho {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A throwaway certificate authority and a certificate for the echo's two
/// names, all PEM.
pub struct TestCertificates {
    pub ca_pem: String,
    pub cert_pem: String,
    pub key_pem: String,
}

impl TestCertificates {
    pub fn generate() -> Self {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(rcgen::DnType::CommonName, "Kage test authority");
        ca.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(ca, ca_key);

        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let mut leaf =
            rcgen::CertificateParams::new(vec![ECHO_HOST.to_owned(), ECHO_H1_HOST.to_owned()])
                .unwrap();
        leaf.distinguished_name
            .push(rcgen::DnType::CommonName, ECHO_HOST);
        leaf.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        leaf.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        let leaf_cert = leaf.signed_by(&leaf_key, &issuer).unwrap();
        Self {
            ca_pem: ca_cert.pem(),
            cert_pem: leaf_cert.pem(),
            key_pem: leaf_key.serialize_pem(),
        }
    }
}

impl TestEcho {
    pub async fn start() -> Self {
        let certificates = TestCertificates::generate();
        let tls = EchoTls::from_pem(
            certificates.cert_pem.as_bytes(),
            certificates.key_pem.as_bytes(),
        )
        .unwrap();
        let server = EchoServer::bind("127.0.0.1:0".parse().unwrap(), tls)
            .await
            .unwrap();
        let addr = server.local_addr().unwrap();
        Self {
            addr,
            ca_pem: certificates.ca_pem,
            task: tokio::spawn(server.serve()),
        }
    }

    /// Makes the echo reachable through a proxy under both of its names.
    pub fn route_through(&self, proxy: &TestProxy) {
        proxy.route(&format!("{ECHO_HOST}:443"), self.addr);
        proxy.route(&format!("{ECHO_H1_HOST}:443"), self.addr);
    }

    pub fn url(path: &str) -> url::Url {
        format!("https://{ECHO_HOST}{path}").parse().unwrap()
    }

    pub fn h1_url(path: &str) -> url::Url {
        format!("https://{ECHO_H1_HOST}{path}").parse().unwrap()
    }
}

/// Parses the echo's answer.
pub fn observation(body: &[u8]) -> Observation {
    serde_json::from_slice(body).unwrap_or_else(|error| {
        panic!(
            "not an echo answer ({error}): {}",
            String::from_utf8_lossy(body)
        )
    })
}

/// A client spec for a profile, with the User-Agent a Firefox-on-Linux
/// persona would supply.
pub fn spec(profile: &str, os: ClaimedOs) -> ClientSpec {
    let choice = ProfileChoice::named(profile, os).unwrap();
    let needs_user_agent = profile.starts_with("firefox_") && os == ClaimedOs::Linux;
    let major = choice.major();
    let spec = ClientSpec::new(choice);
    if needs_user_agent {
        spec.with_identity(IdentityOverride {
            user_agent: Some(firefox_linux_user_agent(major)),
            client_hint_brands: None,
        })
    } else {
        spec
    }
}

/// A factory with in-memory cookies that trusts the echo's authority.
pub fn factory(echo: &TestEcho) -> ClientFactory {
    ClientFactory::in_memory(HttpSettings::default()).with_trust_anchor(echo.ca_pem.clone())
}

/// A resolver that answers from a table and counts its questions.
#[derive(Debug, Default)]
pub struct ScriptedResolver {
    answers: Mutex<HashMap<String, Result<Vec<IpAddr>, String>>>,
    asked: Mutex<Vec<String>>,
}

impl ScriptedResolver {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn answer(&self, host: &str, addresses: &[&str]) {
        self.answers.lock().unwrap().insert(
            host.to_owned(),
            Ok(addresses.iter().map(|text| text.parse().unwrap()).collect()),
        );
    }

    pub fn fail(&self, host: &str, reason: &str) {
        self.answers
            .lock()
            .unwrap()
            .insert(host.to_owned(), Err(reason.to_owned()));
    }

    pub fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl HopResolver for ScriptedResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, String> {
        self.asked.lock().unwrap().push(host.to_owned());
        self.answers
            .lock()
            .unwrap()
            .get(host)
            .cloned()
            .unwrap_or_else(|| Ok(vec!["93.184.216.34".parse().unwrap()]))
    }
}
