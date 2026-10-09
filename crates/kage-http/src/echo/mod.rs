//! The fingerprint echo (spec §7.4): a small TLS server that tells a client
//! what its connection and its request looked like on the wire.
//!
//! For every request it answers with one JSON document, an
//! [`Observation`]: the JA4 of the `ClientHello`, the HTTP/2 ("Akamai")
//! fingerprint, the pseudo-header order, and every header in the order and
//! spelling it was sent in. Over HTTP/1.1 it reports the request line and
//! the header lines as sent instead.
//!
//! It exists so that the http path's fingerprint can be checked
//!
//! - on every change, on this machine, with no network: an integration
//!   test sends each mapped profile through a local proxy to this echo and
//!   compares the result with `tests/fixtures/fingerprints/`;
//! - against the real browsers: milestones M4 and M5 point Chrome and
//!   Camoufox at the same echo, so that both paths are compared by the same
//!   code on the same data;
//! - in a deployment, by the operator, without showing any third party the
//!   exit address of a proxy (`kage echo serve`, see
//!   `docs/runbooks/fingerprint-echo.md`).
//!
//! The echo answers every path and method the same way. A host name that
//! starts with `h1.` is offered HTTP/1.1 only, which is how the HTTP/1.1
//! header tables are exercised against a client that prefers HTTP/2.
//!
//! - [`clienthello`]: the `ClientHello` and JA4.
//! - [`hpack`]: the header compression of HTTP/2.
//! - [`h2`]: HTTP/2 frames and the Akamai fingerprint.
//! - [`server`]: the listener.

pub mod clienthello;
pub mod h2;
pub mod hpack;
mod huffman;
pub mod server;

use serde::{Deserialize, Serialize};

pub use clienthello::ClientHello;
pub use server::{EchoError, EchoServer, EchoTls};

/// The value of [`Observation::echo`]: tells this echo's answers from
/// another service's.
pub const ECHO_NAME: &str = "kage-echo";

/// The version of the [`Observation`] format.
pub const ECHO_VERSION: u32 = 1;

/// What the TLS handshake revealed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsObservation {
    /// The JA4 fingerprint.
    pub ja4: String,
    /// JA4 with its lists written out instead of hashed.
    pub ja4_r: String,
    /// The highest TLS version offered: `13`, `12`, ...
    pub version: String,
    /// The server name the client asked for.
    pub sni: Option<String>,
    /// The ALPN protocols offered, in order.
    pub alpn: Vec<String>,
    /// The cipher suites as sent, GREASE included, in hex.
    pub ciphers: Vec<String>,
    /// The extension types as sent, GREASE included, in hex. Chrome
    /// shuffles them per connection.
    pub extensions: Vec<String>,
    /// The key exchange groups as sent, in hex.
    pub groups: Vec<String>,
    /// The signature algorithms as sent, in hex.
    pub signature_algorithms: Vec<String>,
}

/// What an HTTP/2 connection and request revealed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Http2Observation {
    /// The Akamai fingerprint:
    /// `settings|window update|priority frames|pseudo-header order`.
    pub akamai: String,
    /// The client's `SETTINGS`, in the order sent.
    pub settings: Vec<(u16, u32)>,
    /// The increment of the first connection-level `WINDOW_UPDATE`.
    pub window_update: Option<u32>,
    /// The `PRIORITY` frames, as `stream:exclusive:dependency:weight`.
    pub priority_frames: Vec<String>,
    /// The priority fields of the request's `HEADERS` frame, in the same
    /// form.
    pub headers_priority: Option<String>,
    /// The pseudo-headers in the order sent, by initial: `m`, `a`, `s`,
    /// `p`.
    pub pseudo_headers: Vec<String>,
    /// The header fields in the order sent, without the pseudo-headers.
    pub headers: Vec<(String, String)>,
}

/// What an HTTP/1.1 request looked like.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Http1Observation {
    /// The request line as sent.
    pub request_line: String,
    /// The header lines in the order and spelling sent.
    pub headers: Vec<(String, String)>,
}

/// What the echo saw of one request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    /// [`ECHO_NAME`].
    pub echo: String,
    /// [`ECHO_VERSION`].
    pub version: u32,
    /// The address the connection came from: through a proxy, its exit
    /// address.
    pub peer: String,
    /// The request method.
    pub method: String,
    /// The request path with its query.
    pub path: String,
    /// `h2` or `http/1.1`.
    pub http_version: String,
    /// The TLS handshake.
    pub tls: TlsObservation,
    /// The HTTP/2 side; `None` over HTTP/1.1.
    pub http2: Option<Http2Observation>,
    /// The HTTP/1.1 side; `None` over HTTP/2.
    pub http1: Option<Http1Observation>,
}

impl Observation {
    /// The request's headers in the order sent: lowercase names over
    /// HTTP/2, the client's own spelling over HTTP/1.1.
    pub fn headers(&self) -> &[(String, String)] {
        match (&self.http2, &self.http1) {
            (Some(http2), _) => &http2.headers,
            (None, Some(http1)) => &http1.headers,
            (None, None) => &[],
        }
    }

    /// The header names in the order sent.
    pub fn header_names(&self) -> Vec<&str> {
        self.headers()
            .iter()
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// The first value of a header, whatever its spelling.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers()
            .iter()
            .find(|(found, _)| found.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}
