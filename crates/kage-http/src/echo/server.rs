//! The echo's listener: reads the `ClientHello` off the socket before TLS
//! is terminated, then speaks HTTP/2 frame by frame, or HTTP/1.1 line by
//! line, and answers each request with what it saw.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;

use super::clienthello::{ClientHello, HelloError, hex_list};
use super::h2::{self, Connection, H2Error, Step};
use super::{
    ECHO_NAME, ECHO_VERSION, Http1Observation, Http2Observation, Observation, TlsObservation,
};

/// Longest a client may take for the TLS handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a connection may sit idle between requests.
const IDLE_TIMEOUT: Duration = Duration::from_secs(20);

/// Most connections served at once; further ones wait.
const MAX_CONNECTIONS: usize = 256;

/// Longest `ClientHello` and longest HTTP/1.1 request head read.
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Longest request body read and dropped.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// A host name with this prefix is offered HTTP/1.1 only.
const HTTP1_ONLY_PREFIX: &str = "h1.";

/// Why the echo could not start.
#[derive(Debug, thiserror::Error)]
pub enum EchoError {
    /// The certificate or the key is not usable PEM.
    #[error("the echo's {what} is not valid PEM: {reason}")]
    Pem {
        /// `certificate chain` or `private key`.
        what: &'static str,
        /// What is wrong.
        reason: String,
    },
    /// rustls refused the certificate and key.
    #[error("the echo's certificate and key do not make a TLS configuration: {0}")]
    Tls(#[from] rustls::Error),
    /// The listener could not be opened.
    #[error("the echo cannot listen: {0}")]
    Io(#[from] io::Error),
}

/// The echo's TLS side: one certificate, offered with HTTP/2 and HTTP/1.1,
/// or with HTTP/1.1 only.
#[derive(Clone)]
pub struct EchoTls {
    both: TlsAcceptor,
    http1_only: TlsAcceptor,
}

impl std::fmt::Debug for EchoTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EchoTls").finish_non_exhaustive()
    }
}

impl EchoTls {
    /// The TLS side for a certificate chain and its private key, both PEM.
    ///
    /// # Errors
    ///
    /// Fails when either does not parse or they do not belong together.
    pub fn from_pem(certificate_chain: &[u8], private_key: &[u8]) -> Result<Self, EchoError> {
        let certificates: Vec<CertificateDer<'static>> =
            CertificateDer::pem_slice_iter(certificate_chain)
                .collect::<Result<_, _>>()
                .map_err(|error| EchoError::Pem {
                    what: "certificate chain",
                    reason: error.to_string(),
                })?;
        if certificates.is_empty() {
            return Err(EchoError::Pem {
                what: "certificate chain",
                reason: "it holds no certificate".to_owned(),
            });
        }
        let key = PrivateKeyDer::from_pem_slice(private_key).map_err(|error| EchoError::Pem {
            what: "private key",
            reason: error.to_string(),
        })?;
        let acceptor = |protocols: &[&[u8]], key: PrivateKeyDer<'static>| {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let mut config = ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()?
                .with_no_client_auth()
                .with_single_cert(certificates.clone(), key)?;
            config.alpn_protocols = protocols.iter().map(|name| name.to_vec()).collect();
            Ok::<_, rustls::Error>(TlsAcceptor::from(Arc::new(config)))
        };
        Ok(Self {
            both: acceptor(&[b"h2", b"http/1.1"], key.clone_key())?,
            http1_only: acceptor(&[b"http/1.1"], key)?,
        })
    }
}

/// A stream that first gives back bytes that were already read from it.
struct Replay<S> {
    head: Vec<u8>,
    consumed: usize,
    inner: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for Replay<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.consumed < self.head.len() {
            let count = buffer.remaining().min(self.head.len() - self.consumed);
            let start = self.consumed;
            buffer.put_slice(&self.head[start..start + count]);
            self.consumed += count;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Replay<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(context, bytes)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Reads TLS records until the first handshake message is complete.
/// Returns the records as read, to be replayed to the TLS library, and the
/// handshake message.
async fn read_client_hello<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut records = Vec::new();
    let mut handshake = Vec::new();
    loop {
        let mut header = [0_u8; 5];
        stream.read_exact(&mut header).await?;
        if header[0] != 22 {
            return Err(invalid(
                "the connection does not start with a TLS handshake",
            ));
        }
        let length = usize::from(u16::from_be_bytes([header[3], header[4]]));
        if length == 0 || records.len() + length > MAX_HEAD_BYTES {
            return Err(invalid("the ClientHello is empty or too large"));
        }
        let mut fragment = vec![0_u8; length];
        stream.read_exact(&mut fragment).await?;
        records.extend_from_slice(&header);
        records.extend_from_slice(&fragment);
        handshake.extend_from_slice(&fragment);
        if let Some(length) = handshake.get(1..4) {
            let needed = 4
                + ((usize::from(length[0]) << 16)
                    | (usize::from(length[1]) << 8)
                    | usize::from(length[2]));
            if handshake.len() >= needed {
                handshake.truncate(needed);
                return Ok((records, handshake));
            }
        }
    }
}

impl From<&ClientHello> for TlsObservation {
    fn from(hello: &ClientHello) -> Self {
        let hex = |values: &[u16]| {
            hex_list(values)
                .split(',')
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect()
        };
        Self {
            ja4: hello.ja4(),
            ja4_r: hello.ja4_raw(),
            version: hello.version().to_owned(),
            sni: hello.sni.clone(),
            alpn: hello.alpn.clone(),
            ciphers: hex(&hello.ciphers),
            extensions: hex(&hello.extensions),
            groups: hex(&hello.groups),
            signature_algorithms: hex(&hello.signature_algorithms),
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum ConnectionError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Hello(#[from] HelloError),
    #[error("{0}")]
    Http2(#[from] H2Error),
    #[error("the handshake took too long")]
    Slow,
}

const RESPONSE_FIELDS: [(&str, &str); 3] = [
    ("content-type", "application/json"),
    ("cache-control", "no-store"),
    ("access-control-allow-origin", "*"),
];

fn body_of(observation: &Observation) -> Vec<u8> {
    // A struct of strings and numbers always serializes.
    let mut body = serde_json::to_vec_pretty(observation).unwrap_or_default();
    body.push(b'\n');
    body
}

async fn serve_http2<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    peer: SocketAddr,
    tls: &TlsObservation,
) -> Result<(), ConnectionError> {
    let mut preface = [0_u8; 24];
    stream.read_exact(&mut preface).await?;
    if &preface != h2::PREFACE {
        return Err(invalid("the HTTP/2 connection does not start with the preface").into());
    }
    // The echo's own settings: all defaults.
    stream
        .write_all(&h2::frame(h2::SETTINGS, 0, 0, &[]))
        .await?;
    stream.flush().await?;

    let mut connection = Connection::default();
    loop {
        let Ok(frame) = timeout(IDLE_TIMEOUT, h2::read_frame(stream)).await else {
            return Ok(());
        };
        let Some(frame) = frame? else {
            return Ok(());
        };
        match connection.on_frame(&frame)? {
            Step::Continue => {}
            Step::Close => return Ok(()),
            Step::Write(bytes) => {
                stream.write_all(&bytes).await?;
                stream.flush().await?;
            }
            Step::Respond(request) => {
                let field = |name: &str| {
                    request
                        .fields
                        .iter()
                        .find(|(found, _)| found == name)
                        .map(|(_, value)| value.clone())
                        .unwrap_or_default()
                };
                let observation = Observation {
                    echo: ECHO_NAME.to_owned(),
                    version: ECHO_VERSION,
                    peer: peer.ip().to_string(),
                    method: field(":method"),
                    path: field(":path"),
                    http_version: "h2".to_owned(),
                    tls: tls.clone(),
                    http2: Some(Http2Observation {
                        akamai: connection.akamai(&request),
                        settings: connection.settings.clone().unwrap_or_default(),
                        window_update: connection.window_update,
                        priority_frames: connection
                            .priorities
                            .iter()
                            .map(h2::StreamPriority::fingerprint)
                            .collect(),
                        headers_priority: request
                            .priority
                            .as_ref()
                            .map(h2::StreamPriority::fingerprint),
                        pseudo_headers: h2::pseudo_order(&request.fields),
                        headers: request
                            .fields
                            .iter()
                            .filter(|(name, _)| !name.starts_with(':'))
                            .cloned()
                            .collect(),
                    }),
                    http1: None,
                };
                tracing::info!(
                    peer = %observation.peer,
                    ja4 = %observation.tls.ja4,
                    akamai = %connection.akamai(&request),
                    "echoed an HTTP/2 request"
                );
                let body = body_of(&observation);
                let length = body.len().to_string();
                let mut fields = vec![(":status", "200"), ("content-length", length.as_str())];
                fields.extend(RESPONSE_FIELDS);
                stream
                    .write_all(&h2::response(request.stream, &fields, &body))
                    .await?;
                stream.flush().await?;
            }
        }
    }
}

/// Reads one HTTP/1.1 request head. `None` when the client closed the
/// connection or stayed silent.
async fn read_head<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
) -> io::Result<Option<Vec<u8>>> {
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let rest = buffer.split_off(end + 4);
            let head = std::mem::replace(buffer, rest);
            return Ok(Some(head));
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Err(invalid("the request head is too large"));
        }
        let mut chunk = [0_u8; 4096];
        let Ok(read) = timeout(IDLE_TIMEOUT, stream.read(&mut chunk)).await else {
            return Ok(None);
        };
        let read = read?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

async fn serve_http1<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    peer: SocketAddr,
    tls: &TlsObservation,
) -> Result<(), ConnectionError> {
    let mut buffer = Vec::new();
    while let Some(head) = read_head(stream, &mut buffer).await? {
        let text = String::from_utf8_lossy(&head);
        let mut lines = text.split("\r\n").filter(|line| !line.is_empty());
        let request_line = lines.next().unwrap_or_default().to_owned();
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_owned(), value.trim().to_owned()))
            .collect();
        let header = |wanted: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
                .map(|(_, value)| value.as_str())
        };
        // A request body is read and dropped.
        let mut body_left = header("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        if body_left > MAX_BODY_BYTES {
            return Err(invalid("the request body is too large").into());
        }
        let buffered = body_left.min(buffer.len());
        buffer.drain(..buffered);
        body_left -= buffered;
        while body_left > 0 {
            let mut chunk = [0_u8; 4096];
            let wanted = body_left.min(chunk.len());
            let read = timeout(IDLE_TIMEOUT, stream.read(&mut chunk[..wanted]))
                .await
                .map_err(|_| invalid("the request body did not arrive"))??;
            if read == 0 {
                return Ok(());
            }
            body_left -= read;
        }

        let mut words = request_line.split(' ');
        let observation = Observation {
            echo: ECHO_NAME.to_owned(),
            version: ECHO_VERSION,
            peer: peer.ip().to_string(),
            method: words.next().unwrap_or_default().to_owned(),
            path: words.next().unwrap_or_default().to_owned(),
            http_version: "http/1.1".to_owned(),
            tls: tls.clone(),
            http2: None,
            http1: Some(Http1Observation {
                request_line: request_line.clone(),
                headers: headers.clone(),
            }),
        };
        tracing::info!(
            peer = %observation.peer,
            ja4 = %observation.tls.ja4,
            "echoed an HTTP/1.1 request"
        );
        let close = header("connection").is_some_and(|value| value.eq_ignore_ascii_case("close"));
        let body = body_of(&observation);
        let mut response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n", body.len());
        for (name, value) in RESPONSE_FIELDS {
            response.push_str(name);
            response.push_str(": ");
            response.push_str(value);
            response.push_str("\r\n");
        }
        if close {
            response.push_str("Connection: close\r\n");
        }
        response.push_str("\r\n");
        stream.write_all(response.as_bytes()).await?;
        stream.write_all(&body).await?;
        stream.flush().await?;
        if close {
            break;
        }
    }
    Ok(())
}

async fn serve_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    tls: &EchoTls,
) -> Result<(), ConnectionError> {
    let (records, handshake) = timeout(HANDSHAKE_TIMEOUT, read_client_hello(&mut stream))
        .await
        .map_err(|_| ConnectionError::Slow)??;
    let hello = ClientHello::parse(&handshake)?;
    let acceptor = if hello
        .sni
        .as_deref()
        .is_some_and(|name| name.starts_with(HTTP1_ONLY_PREFIX))
    {
        &tls.http1_only
    } else {
        &tls.both
    };
    let replay = Replay {
        head: records,
        consumed: 0,
        inner: stream,
    };
    let mut stream = timeout(HANDSHAKE_TIMEOUT, acceptor.accept(replay))
        .await
        .map_err(|_| ConnectionError::Slow)??;
    let observation = TlsObservation::from(&hello);
    let http2 = stream.get_ref().1.alpn_protocol() == Some(b"h2".as_slice());
    let served = if http2 {
        serve_http2(&mut stream, peer, &observation).await
    } else {
        serve_http1(&mut stream, peer, &observation).await
    };
    let _ = stream.shutdown().await;
    served
}

/// The fingerprint echo, listening.
#[derive(Debug)]
pub struct EchoServer {
    listener: TcpListener,
    tls: EchoTls,
}

impl EchoServer {
    /// Opens the listener.
    ///
    /// # Errors
    ///
    /// Fails when the address cannot be bound.
    pub async fn bind(address: SocketAddr, tls: EchoTls) -> Result<Self, EchoError> {
        Ok(Self {
            listener: TcpListener::bind(address).await?,
            tls,
        })
    }

    /// The address the echo listens on.
    ///
    /// # Errors
    ///
    /// Fails when the socket has no local address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serves connections until the future is dropped. A connection that
    /// fails is logged and closed; it never stops the echo.
    pub async fn serve(self) {
        let tls = Arc::new(self.tls);
        let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        loop {
            let Ok(slot) = Arc::clone(&slots).acquire_owned().await else {
                return;
            };
            let (stream, peer) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!(%error, "the echo could not accept a connection");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let tls = Arc::clone(&tls);
            tokio::spawn(async move {
                if let Err(error) = serve_connection(stream, peer, &tls).await {
                    tracing::debug!(peer = %peer.ip(), %error, "an echo connection ended early");
                }
                drop(slot);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EchoError, EchoTls, Replay, read_client_hello, read_head};
    use crate::echo::clienthello::tests::chrome_like;
    use tokio::io::AsyncReadExt;

    fn record(fragment: &[u8]) -> Vec<u8> {
        let mut bytes = vec![22, 3, 1];
        bytes.extend(u16::try_from(fragment.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(fragment);
        bytes
    }

    #[tokio::test]
    async fn the_client_hello_is_read_whole_and_kept_for_replay() {
        let hello = chrome_like(&(0..18).collect::<Vec<_>>());

        // In one record, with bytes of the next flight behind it.
        let mut wire = record(&hello);
        wire.extend_from_slice(b"later");
        let mut stream = wire.as_slice();
        let (records, handshake) = read_client_hello(&mut stream).await.unwrap();
        assert_eq!(handshake, hello);
        assert_eq!(records, record(&hello));
        assert_eq!(stream, b"later");

        // Split over two records.
        let (first, second) = hello.split_at(100);
        let wire = [record(first), record(second)].concat();
        let (records, handshake) = read_client_hello(&mut wire.as_slice()).await.unwrap();
        assert_eq!(handshake, hello);
        assert_eq!(records, wire);
    }

    #[tokio::test]
    async fn what_is_not_a_client_hello_is_refused() {
        // A plain HTTP request sent to the TLS port.
        assert!(
            read_client_hello(&mut b"GET / HTTP/1.1\r\n\r\n".as_slice())
                .await
                .is_err()
        );
        // An empty record, a record that ends early, and one that never ends.
        assert!(
            read_client_hello(&mut [22, 3, 1, 0, 0].as_slice())
                .await
                .is_err()
        );
        assert!(
            read_client_hello(&mut [22, 3, 1, 0, 9, 1].as_slice())
                .await
                .is_err()
        );
        let endless = [
            record(&[1, 0xff, 0xff, 0xff]),
            record(&vec![0; 16_000]).repeat(5),
        ]
        .concat();
        assert!(read_client_hello(&mut endless.as_slice()).await.is_err());
    }

    #[tokio::test]
    async fn replayed_bytes_come_before_the_stream() {
        let mut replay = Replay {
            head: b"hello ".to_vec(),
            consumed: 0,
            inner: b"world".as_slice(),
        };

        let mut small = [0_u8; 4];
        assert_eq!(replay.read(&mut small).await.unwrap(), 4);
        assert_eq!(&small, b"hell");
        let mut rest = String::new();
        replay.read_to_string(&mut rest).await.unwrap();
        assert_eq!(rest, "o world");
    }

    #[tokio::test]
    async fn a_request_head_is_split_from_what_follows_it() {
        let mut buffer = Vec::new();
        let mut stream = b"GET /a HTTP/1.1\r\nHost: x\r\n\r\nGET /b HTTP/1.1\r\n\r\n".as_slice();

        let first = read_head(&mut stream, &mut buffer).await.unwrap().unwrap();
        assert_eq!(first, b"GET /a HTTP/1.1\r\nHost: x\r\n\r\n");
        let second = read_head(&mut stream, &mut buffer).await.unwrap().unwrap();
        assert_eq!(second, b"GET /b HTTP/1.1\r\n\r\n");
        assert_eq!(read_head(&mut stream, &mut buffer).await.unwrap(), None);

        let mut huge = Vec::new();
        let endless = vec![b'a'; 70_000];
        assert!(read_head(&mut endless.as_slice(), &mut huge).await.is_err());
    }

    #[test]
    fn certificates_and_keys_that_are_not_pem_are_refused() {
        let error = EchoTls::from_pem(b"not pem", b"not pem").unwrap_err();
        assert!(
            matches!(
                error,
                EchoError::Pem {
                    what: "certificate chain",
                    ..
                }
            ),
            "{error}"
        );
        assert!(error.to_string().contains("certificate chain"));
    }
}
