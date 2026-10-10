//! A local HTTP proxy with Basic authentication that stands in for a
//! provider's proxy. It forwards plain requests and opens `CONNECT`
//! tunnels, resolves the test host names itself (as a real proxy resolves
//! the target), and keeps the raw head of every request it saw, so a test
//! can assert what went on the wire. Nothing here leaves the machine.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use kage_core::domain::{ProxyEndpoint, ProxyScheme};
use kage_core::ids::ProxyId;
use kage_core::secret::Secret;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The credentials the test proxy accepts: the provider's.
pub const PROXY_USER: &str = "provider-user";
pub const PROXY_PASS: &str = "provider-pass-51";

/// One request the proxy saw.
#[derive(Debug, Clone)]
pub struct Seen {
    /// `CONNECT`, `GET`, ...
    pub method: String,
    /// `host:port` for a tunnel, the absolute URL for a plain request.
    pub target: String,
    /// The request head exactly as the client sent it, without the empty
    /// line that ends it.
    pub head: String,
}

impl Seen {
    /// The first value of a header.
    pub fn header(&self, name: &str) -> Option<String> {
        self.head
            .split("\r\n")
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .find(|(found, _)| found.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim().to_owned())
    }
}

struct ProxyShared {
    expected: String,
    routes: Mutex<HashMap<String, SocketAddr>>,
    seen: Mutex<Vec<Seen>>,
    up: AtomicBool,
    refused: AtomicUsize,
    /// Bytes received from clients and sent to them, tunnels included.
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
}

/// See the module documentation.
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

/// Base64 with padding.
pub fn base64(input: &[u8]) -> String {
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

/// The value of a `Proxy-Authorization` header.
pub fn basic(username: &str, password: &str) -> String {
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
            expected: basic(PROXY_USER, PROXY_PASS),
            routes: Mutex::new(HashMap::new()),
            seen: Mutex::new(Vec::new()),
            up: AtomicBool::new(true),
            refused: AtomicUsize::new(0),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
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
                        let _ = Box::pin(serve(stream, &shared)).await;
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

    /// The proxy with the provider's credentials.
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

    /// Bytes the proxy received from its clients and sent to them: what a
    /// provider bills.
    pub fn bytes(&self) -> (u64, u64) {
        (
            self.shared.bytes_in.load(Ordering::SeqCst),
            self.shared.bytes_out.load(Ordering::SeqCst),
        )
    }

    /// Takes the proxy down (connections are closed at once) or up again.
    pub fn set_up(&self, up: bool) {
        self.shared.up.store(up, Ordering::SeqCst);
    }
}

async fn answer(client: &mut TcpStream, shared: &ProxyShared, bytes: &[u8]) -> std::io::Result<()> {
    client.write_all(bytes).await?;
    shared
        .bytes_out
        .fetch_add(bytes.len() as u64, Ordering::SeqCst);
    Ok(())
}

/// Relays both ways until both ends are done, counting the client's side.
async fn relay(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    shared: &ProxyShared,
) -> std::io::Result<()> {
    let (mut client_read, mut client_write) = client.split();
    let (mut upstream_read, mut upstream_write) = upstream.split();
    let up = async {
        let mut chunk = [0_u8; 8192];
        loop {
            let read = client_read.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            shared.bytes_in.fetch_add(read as u64, Ordering::SeqCst);
            if upstream_write.write_all(&chunk[..read]).await.is_err() {
                break;
            }
        }
        let _ = upstream_write.shutdown().await;
    };
    let down = async {
        let mut chunk = [0_u8; 8192];
        loop {
            let read = upstream_read.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            if client_write.write_all(&chunk[..read]).await.is_err() {
                break;
            }
            shared.bytes_out.fetch_add(read as u64, Ordering::SeqCst);
        }
        let _ = client_write.shutdown().await;
    };
    tokio::join!(up, down);
    Ok(())
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
        shared.bytes_in.fetch_add(read as u64, Ordering::SeqCst);
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
    if authorization.as_deref() != Some(shared.expected.as_str()) {
        shared.refused.fetch_add(1, Ordering::SeqCst);
        return answer(
            &mut client,
            shared,
            b"HTTP/1.1 407 Proxy Authentication Required\r\n\
              Proxy-Authenticate: Basic realm=\"provider\"\r\n\
              Content-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
    }
    shared.seen.lock().unwrap().push(Seen {
        method: method.to_owned(),
        target: target.to_owned(),
        head: head.trim_end().to_owned(),
    });
    let route = |authority: &str| shared.routes.lock().unwrap().get(authority).copied();
    let bad_gateway = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n";

    if method.eq_ignore_ascii_case("CONNECT") {
        let Some(upstream) = route(target) else {
            return answer(&mut client, shared, bad_gateway).await;
        };
        let Ok(mut upstream) = TcpStream::connect(upstream).await else {
            return answer(&mut client, shared, bad_gateway).await;
        };
        answer(
            &mut client,
            shared,
            b"HTTP/1.1 200 Connection Established\r\n\r\n",
        )
        .await?;
        upstream.write_all(&rest).await?;
        return Box::pin(relay(&mut client, &mut upstream, shared)).await;
    }

    // A plain request in absolute form: http://host[:port]/path.
    let Some(without_scheme) = target.strip_prefix("http://") else {
        return answer(
            &mut client,
            shared,
            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
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
        return answer(&mut client, shared, bad_gateway).await;
    };
    let Ok(mut upstream) = TcpStream::connect(upstream).await else {
        return answer(&mut client, shared, bad_gateway).await;
    };
    let mut forwarded = format!("{method} {path} HTTP/1.1\r\n");
    for line in headers {
        let name = line.split(':').next().unwrap_or_default();
        if name.eq_ignore_ascii_case("proxy-authorization")
            || name.eq_ignore_ascii_case("proxy-connection")
            || name.eq_ignore_ascii_case("connection")
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
    Box::pin(relay(&mut client, &mut upstream, shared)).await
}
