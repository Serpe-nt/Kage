//! Why a client could not be built or a fetch did not finish.

use std::time::Duration;

use kage_core::ssrf::{RedirectDenied, SsrfDenial};

use crate::cookies::CookieBackendError;
use crate::headers::HeaderError;
use crate::profile::ProfileError;

/// What a fetch was doing when its time ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Opening the tunnel through the proxy or the TLS handshake.
    Connecting,
    /// Waiting for the response headers.
    WaitingForResponse,
    /// Reading the response body.
    ReadingBody,
}

impl Stage {
    /// A phrase for a message: "while ...".
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::WaitingForResponse => "waiting for the response",
            Self::ReadingBody => "reading the body",
        }
    }
}

/// Why a client could not be built or a fetch did not finish.
///
/// No variant carries proxy credentials. Messages name a target URL only
/// where the caller supplied it or a server sent it.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    /// The emulation profile cannot be used.
    #[error(transparent)]
    Profile(#[from] ProfileError),
    /// The request's headers were refused.
    #[error(transparent)]
    Header(#[from] HeaderError),
    /// The SSRF guard refused the URL itself.
    #[error("the URL was refused: {0}")]
    Denied(#[source] SsrfDenial),
    /// The SSRF guard refused a redirect hop. The attempt's outcome is
    /// `ssrf_denied`; nothing fetched on the earlier hops is kept.
    #[error(transparent)]
    RedirectDenied(RedirectDenied),
    /// The server redirected more often than allowed.
    #[error("more than {max} redirects; the last one pointed at {location:?}")]
    TooManyRedirects {
        /// The limit.
        max: usize,
        /// Where the redirect over the limit pointed.
        location: String,
    },
    /// A URL cannot be requested.
    #[error("{0:?} is not a URL that can be requested")]
    InvalidUrl(String),
    /// The `Referer` a caller gave is not an http(s) URL.
    #[error("the Referer {0:?} is not an http or https URL")]
    InvalidReferer(String),
    /// The method cannot be sent on the http path.
    #[error("the method {0} is not supported")]
    Method(String),
    /// The kind of request is not one a caller may ask for.
    #[error("a request for a {0} is made by the page that needs it, not asked for")]
    Kind(&'static str),
    /// A `fetch()` or an `XMLHttpRequest` was asked for without the page
    /// that makes it.
    #[error(
        "a fetch() or XMLHttpRequest needs the page that makes it: give its URL as the Referer"
    )]
    InitiatorRequired,
    /// A browser would ask the server first (a CORS preflight), and that
    /// exchange is not emulated.
    #[error(
        "a browser would send a preflight before this cross-origin {method}: {reason}; \
         preflights are not emulated"
    )]
    PreflightRequired {
        /// What makes the request one a browser asks about.
        reason: &'static str,
        /// The method of the request.
        method: String,
    },
    /// The attempt ran out of time.
    #[error("timed out after {after:?} while {}", stage.as_str())]
    Timeout {
        /// What it was doing.
        stage: Stage,
        /// How long it had run.
        after: Duration,
    },
    /// The proxy refused the credentials.
    #[error("the proxy refused the credentials")]
    ProxyAuth,
    /// No connection could be made through the proxy.
    #[error("cannot connect through the proxy: {0}")]
    Connect(&'static str),
    /// The TLS handshake with the target failed.
    #[error("the TLS handshake with the target failed")]
    Tls,
    /// The request failed after the connection was made.
    #[error("the request failed: {0}")]
    Request(&'static str),
    /// The response body could not be read to its end.
    #[error("the response body could not be read: {0}")]
    Body(&'static str),
    /// The HTTP client could not be built.
    #[error("cannot build the HTTP client: {0}")]
    Build(String),
    /// The cookie jar could not be loaded.
    #[error(transparent)]
    Cookies(#[from] CookieBackendError),
    /// A host name could not be resolved for the address check.
    #[error("cannot resolve {host:?} for the address check: {reason}")]
    Resolve {
        /// The host name.
        host: String,
        /// Why.
        reason: String,
    },
}

impl HttpError {
    /// Whether the SSRF guard stopped the fetch, on the URL or on a hop.
    pub const fn is_ssrf_denied(&self) -> bool {
        matches!(self, Self::Denied(_) | Self::RedirectDenied(_))
    }

    /// Whether the request itself cannot be sent as asked, whoever sends
    /// it: a refused header, a method, a missing page behind a script's
    /// request, a request a browser would preflight.
    pub const fn is_invalid_request(&self) -> bool {
        matches!(
            self,
            Self::Header(_)
                | Self::InvalidUrl(_)
                | Self::InvalidReferer(_)
                | Self::Method(_)
                | Self::Kind(_)
                | Self::InitiatorRequired
                | Self::PreflightRequired { .. }
        )
    }

    /// Whether the fetch ran out of time.
    pub const fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout { .. })
    }

    /// Whether the failure lies between Kage and the proxy, or in the
    /// proxy's reach, rather than with the request or the target's answer.
    pub const fn is_proxy_failure(&self) -> bool {
        matches!(self, Self::ProxyAuth | Self::Connect(_))
    }
}

/// Whether the proxy refused the credentials while the tunnel was set up.
/// wreq keeps that as text deep in the error chain.
fn is_proxy_auth(error: &wreq::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    while let Some(cause) = source {
        if cause.to_string().contains("proxy authorization required") {
            return true;
        }
        source = cause.source();
    }
    false
}

/// Turns a wreq error into ours. The text of a wreq error can hold the
/// proxy's address, so only its kind is kept.
pub(crate) fn from_wreq(error: &wreq::Error, stage: Stage, after: Duration) -> HttpError {
    if is_proxy_auth(error) {
        return HttpError::ProxyAuth;
    }
    if error.is_timeout() {
        return HttpError::Timeout { stage, after };
    }
    if error.is_proxy_connect() {
        return HttpError::Connect("the proxy did not open the tunnel");
    }
    if error.is_tls() {
        return HttpError::Tls;
    }
    if error.is_connect() {
        // A failed handshake surfaces as a connection error too.
        return HttpError::Connect("the connection could not be established");
    }
    if error.is_connection_reset() {
        return HttpError::Request("the connection was reset");
    }
    if error.is_body() || error.is_decode() {
        return HttpError::Body("the transfer broke off or the content coding is invalid");
    }
    match stage {
        Stage::ReadingBody => HttpError::Body("the transfer broke off"),
        _ => HttpError::Request("the exchange with the server failed"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_core::ssrf::{DeniedRange, SsrfDenial};

    use super::{HttpError, Stage};

    #[test]
    fn errors_explain_themselves_and_sort_into_kinds() {
        let denied = HttpError::Denied(SsrfDenial::Address {
            address: "10.0.0.1".parse().unwrap(),
            range: DeniedRange::Private,
        });
        assert!(denied.is_ssrf_denied());
        assert!(denied.to_string().contains("10.0.0.1"));

        let timeout = HttpError::Timeout {
            stage: Stage::ReadingBody,
            after: Duration::from_secs(30),
        };
        assert!(timeout.is_timeout() && !timeout.is_ssrf_denied());
        assert_eq!(
            timeout.to_string(),
            "timed out after 30s while reading the body"
        );

        assert!(HttpError::ProxyAuth.is_proxy_failure());
        assert!(HttpError::Connect("x").is_proxy_failure());
        assert!(!HttpError::Tls.is_proxy_failure());
        assert_eq!(Stage::Connecting.as_str(), "connecting");
        assert_eq!(
            Stage::WaitingForResponse.as_str(),
            "waiting for the response"
        );

        let many = HttpError::TooManyRedirects {
            max: 10,
            location: "/next".to_owned(),
        };
        assert!(many.to_string().contains("more than 10 redirects"));
        assert!(!many.is_ssrf_denied());
    }
}
