//! The plain client of level 0 (spec §25, ADR 0012): an HTTP client and
//! nothing else.
//!
//! It is wreq with its own TLS and HTTP/2 settings, untouched (recorded as
//! `tests/fixtures/fingerprints/plain.json` and compared in the gate like
//! every profile), and a minimal, well-formed header set of Kage's, in this
//! order: `Host` (the `:authority` on HTTP/2), `User-Agent`
//! (`kage/<version>` unless the operator or the caller names another),
//! `Accept: */*` (unless the caller names one), `Accept-Encoding` of what
//! Kage decodes, then the caller's headers in the caller's order and
//! spelling. Untouched, wreq would send no `User-Agent`, no `Accept` and,
//! on HTTP/1.1, its `Host` after the other headers, which no common client
//! does (measured in M14's consults).
//!
//! There is no persona and no cookie jar. A cookie a hop of the redirect
//! chain sets is sent on the chain's later hops by the jar's rules, in
//! memory for that one request; nothing is kept. The caller's own `Cookie`
//! and `Authorization` go to the origin of the request only and never on a
//! hop to another origin. Redirects are followed one hop at a time through
//! the SSRF guard's chain, as on the persona's http path, and when the
//! factory was given a resolver (`security.resolve_on_host`) every hop's
//! name is resolved on the host and a hop into a denied range is refused
//! before it is asked for, as on that path (F98: the plain client once
//! skipped that check).
//!
//! Like every client of this crate it leaves through a proxy: the
//! forwarder, under a credential that names no persona, which connects to
//! the site's plain exit (the host or one proxy of the pool), checks every
//! address and counts every byte.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use chrono::Utc;
use http::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_TYPE, COOKIE, HeaderMap, HeaderName,
    HeaderValue, USER_AGENT,
};
use http::{Method, StatusCode, Uri};
use kage_core::domain::{Header, ProxyEndpoint, Timing};
use tokio::time::Instant;
use url::Url;
use wreq::cookie::{CookieStore, Cookies};
use wreq_proto::ext::OnPreserveHeaderCallback;

use crate::client::{ClientFactory, HopResolver, HttpSettings};
use crate::cookies::{CookieContext, CookieJar};
use crate::error::{HttpError, Stage, from_wreq};
use crate::fetch::{
    Fetched, Guard, Hop, REDIRECT_BODY_BYTES, Trail, add, check_addresses, millis, read_body,
    redirect_location, version_name,
};
use crate::headers::CallerHeaders;
use crate::site;
use crate::timing::{self, ConnectTimingLayer, Phases};

/// The encodings Kage decodes, as the plain client offers them.
pub const PLAIN_ACCEPT_ENCODING: &str = "gzip, deflate, br, zstd";

/// What a plain client asks for unless the caller says otherwise.
pub const PLAIN_ACCEPT: &str = "*/*";

/// What to fetch with the plain client.
#[derive(Debug, Clone)]
pub struct PlainSpec {
    /// The URL.
    pub url: Url,
    /// `GET`, `HEAD` or `POST`.
    pub method: Method,
    /// The caller's headers, as given, checked against the plain denylist
    /// ([`crate::headers::plain_denied_reason`]).
    pub headers: Vec<(String, String)>,
    /// The caller's body, as given.
    pub body: Option<Bytes>,
    /// Which checks the URL and its redirects must pass.
    pub guard: Guard,
    /// Longest the fetch may take; the client's setting when `None`.
    pub timeout: Option<Duration>,
    /// Read the body up to this many bytes; never more than the client's
    /// setting.
    pub max_body_bytes: Option<usize>,
    /// Redirect hops to follow; the client's setting when `None`.
    pub max_redirects: Option<usize>,
}

/// The plain client: wreq untouched, through the forwarder.
pub struct PlainClient {
    wreq: wreq::Client,
    settings: HttpSettings,
    user_agent: HeaderValue,
    /// The factory's resolver: every hop is checked by its addresses before
    /// it is asked for, when there is one.
    resolver: Option<Arc<dyn HopResolver>>,
}

impl std::fmt::Debug for PlainClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlainClient")
            .field("user_agent", &self.user_agent)
            .field("resolver", &self.resolver)
            .finish_non_exhaustive()
    }
}

impl ClientFactory {
    /// A plain client (level 0) that leaves through `proxy` (the forwarder,
    /// under a plain credential) and says `user_agent` unless a request
    /// names another. Nothing is kept: no cookies, no client. It checks
    /// every hop with the factory's resolver, when the factory was given
    /// one, as the persona's client does.
    ///
    /// # Errors
    ///
    /// Fails when the user agent cannot be sent or wreq rejects the options.
    pub fn plain(&self, proxy: &ProxyEndpoint, user_agent: &str) -> Result<PlainClient, HttpError> {
        let user_agent = HeaderValue::from_str(user_agent)
            .map_err(|_| HttpError::Build("the plain user agent cannot be sent".to_owned()))?;
        // The address holds no credentials; they are added as a header.
        let through = wreq::Proxy::all(proxy.url())
            .map_err(|_| HttpError::Build("the proxy address is not valid".to_owned()))?
            .basic_auth(proxy.username.expose(), proxy.password.expose());
        let settings = *self.settings();
        let mut builder = wreq::Client::builder()
            // No emulation: wreq's own TLS and HTTP/2 settings.
            .proxy(through)
            .redirect(wreq::redirect::Policy::none())
            .connect_timeout(settings.connect_timeout)
            .pool_idle_timeout(settings.idle_connection_timeout)
            .connector_layer(ConnectTimingLayer);
        if let Some(store) = self.trust_store()? {
            builder = builder.tls_cert_store(store);
        }
        let wreq = builder
            .build()
            .map_err(|_| HttpError::Build("the HTTP client rejected its options".to_owned()))?;
        Ok(PlainClient {
            wreq,
            settings,
            user_agent,
            resolver: self.resolver().cloned(),
        })
    }
}

/// The order and spelling of a plain request's headers on the wire.
#[derive(Debug, Clone)]
struct PlainOrder {
    /// The caller's headers in the caller's order, with the caller's
    /// spelling.
    caller: Vec<(HeaderName, String)>,
}

/// Kage's own headers, in their order, with their spelling on HTTP/1.1.
const OWN: [(HeaderName, &str); 3] = [
    (USER_AGENT, "User-Agent"),
    (ACCEPT, "Accept"),
    (ACCEPT_ENCODING, "Accept-Encoding"),
];

impl PlainOrder {
    fn names(&self) -> Vec<HeaderName> {
        let mut names: Vec<HeaderName> = OWN.iter().map(|(name, _)| name.clone()).collect();
        for (name, _) in &self.caller {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    }
}

impl OnPreserveHeaderCallback for PlainOrder {
    /// HTTP/2: Kage's fields, then the caller's, then whatever wreq added
    /// (a cookie of the chain, the length of a body). `host` is never a
    /// field on HTTP/2.
    fn call(&self, headers: &mut HeaderMap) {
        let mut sorted = HeaderMap::with_capacity(headers.len());
        for name in self.names() {
            for value in headers.get_all(&name) {
                sorted.append(name.clone(), value.clone());
            }
            headers.remove(&name);
        }
        for (name, value) in headers.iter() {
            sorted.append(name.clone(), value.clone());
        }
        *headers = sorted;
    }

    /// HTTP/1.1: `Host` first, then Kage's fields and the caller's in their
    /// spelling, then whatever wreq added.
    fn call_visit(
        &self,
        headers: &mut HeaderMap,
        dst: &mut dyn FnMut(&dyn AsRef<[u8]>, &HeaderValue),
    ) {
        if let Some(host) = headers.remove(http::header::HOST) {
            dst(&"Host", &host);
        }
        for (name, spelled) in &OWN {
            if let Some(value) = headers.remove(name) {
                dst(spelled, &value);
            }
        }
        for (name, spelled) in &self.caller {
            for value in headers.get_all(name) {
                dst(&spelled.as_str(), value);
            }
            headers.remove(name);
        }
        for (name, value) in headers.iter() {
            dst(&name.as_str(), value);
        }
        headers.clear();
    }
}

/// The cookies of one redirect chain: what its hops set is sent to its
/// later hops by the jar's rules, unless the caller's own `Cookie` header
/// is sent on that hop.
struct ChainCookies {
    jar: Arc<CookieJar>,
    url: Url,
    context: CookieContext,
    /// Whether the caller's own `Cookie` goes on this hop.
    callers: bool,
}

impl CookieStore for ChainCookies {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, _uri: &Uri) {
        let now = Utc::now();
        for header in cookie_headers {
            let _ = self
                .jar
                .store(&self.url, &String::from_utf8_lossy(header.as_bytes()), now);
        }
    }

    fn cookies(&self, _uri: &Uri, _version: http::Version) -> Cookies {
        if self.callers {
            return Cookies::Empty;
        }
        let pairs = self.jar.matching(&self.url, self.context, Utc::now());
        if pairs.is_empty() {
            return Cookies::Empty;
        }
        let line = pairs
            .into_iter()
            .map(|(name, value)| {
                if name.is_empty() {
                    value
                } else {
                    format!("{name}={value}")
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        HeaderValue::from_str(&line).map_or(Cookies::Empty, Cookies::Compressed)
    }
}

impl PlainClient {
    /// The `User-Agent` the client says unless a request names another.
    pub const fn user_agent(&self) -> &HeaderValue {
        &self.user_agent
    }

    /// The resolver each hop is checked with before it is asked for, when
    /// the factory was given one.
    pub const fn resolver(&self) -> Option<&Arc<dyn HopResolver>> {
        self.resolver.as_ref()
    }

    /// Fetches a URL as a plain client: the caller's method, headers and
    /// body as given, redirects followed one hop at a time.
    ///
    /// # Errors
    ///
    /// Fails when the method, a header or the URL is refused, a redirect
    /// hop is refused or one too many, the time runs out, or the exchange
    /// breaks. A response with an error status is not an error: it is
    /// returned.
    pub async fn fetch(&self, spec: &PlainSpec) -> Result<Fetched, HttpError> {
        if !matches!(spec.method, Method::GET | Method::HEAD | Method::POST) {
            return Err(HttpError::Method(spec.method.to_string()));
        }
        let mut caller = CallerHeaders::parse_plain(&spec.headers)?;
        let max_redirects = spec.max_redirects.unwrap_or(self.settings.max_redirects);
        let cap = spec
            .max_body_bytes
            .unwrap_or(self.settings.max_body_bytes)
            .min(self.settings.max_body_bytes);
        let mut trail = Trail::start(&spec.url, spec.guard, max_redirects)?;
        let started = Instant::now();
        let deadline = started + spec.timeout.unwrap_or(self.settings.timeout);
        let jar = Arc::new(CookieJar::new());
        let first = spec.url.clone();

        let mut method = spec.method.clone();
        let mut body = spec.body.clone();
        let mut hops = Vec::new();
        let (mut connect, mut tls) = (None, None);
        let (response, first_byte) = loop {
            // The address check of the guard, with a resolver: a hop whose
            // name points into a denied range is never asked for.
            check_addresses(self.resolver.as_deref(), &trail).await?;
            let url = trail.current().clone();
            let incoming = self
                .send(
                    &method,
                    &url,
                    body.clone(),
                    &caller,
                    &jar,
                    &first,
                    started,
                    deadline,
                )
                .await?;
            let first_byte = started.elapsed();
            connect = add(connect, incoming.setup.connect);
            tls = add(tls, incoming.setup.tls);
            let status = incoming.response.status();
            let location = redirect_location(&incoming.response);
            hops.push(Hop {
                url,
                status: status.as_u16(),
                http_version: version_name(incoming.response.version()),
                location: location.clone(),
                setup: incoming.setup,
                waited: incoming.waited,
            });
            let Some(location) = location else {
                break (incoming.response, first_byte);
            };
            // The body of a redirect is read only so far that the
            // connection is free for the next hop.
            let _ = read_body(incoming.response, REDIRECT_BODY_BYTES, started, deadline).await;
            trail.follow(&location)?;
            if continues_as_get(status, &method) {
                method = Method::GET;
                body = None;
                caller = caller.without(&CONTENT_TYPE);
            }
        };

        let status = response.status();
        let http_version = version_name(response.version());
        let headers = headers_of(&response);
        let (body, truncated) = if method == Method::HEAD {
            (Bytes::new(), false)
        } else {
            read_body(response, cap, started, deadline).await?
        };
        let total = started.elapsed();
        tracing::debug!(
            host = ?trail.current().host_str(),
            status = status.as_u16(),
            redirects = trail.redirects(),
            http_version,
            total_ms = millis(total),
            "plain fetch finished"
        );
        Ok(Fetched {
            status: status.as_u16(),
            final_url: trail.current().clone(),
            http_version,
            headers,
            body,
            truncated,
            hops,
            downgraded: trail.downgraded(),
            timing: Timing {
                dns_ms: None,
                connect_ms: connect.map(millis),
                tls_ms: tls.map(millis),
                ttfb_ms: Some(millis(first_byte)),
                dom_content_loaded_ms: None,
                load_ms: None,
                total_ms: millis(total),
            },
            subresources: None,
        })
    }

    /// Sends one hop: Kage's minimal headers and the caller's, the caller's
    /// credentials only to the origin of the request.
    #[allow(clippy::too_many_arguments)] // One hop and the chain it belongs to.
    async fn send(
        &self,
        method: &Method,
        url: &Url,
        body: Option<Bytes>,
        caller: &CallerHeaders,
        jar: &Arc<CookieJar>,
        first: &Url,
        started: Instant,
        deadline: Instant,
    ) -> Result<crate::fetch::Incoming, HttpError> {
        let home = site::same_origin(first, url);
        let mut headers = HeaderMap::with_capacity(8 + caller.names().count());
        let mut order = Vec::new();
        headers.insert(USER_AGENT, self.user_agent.clone());
        headers.insert(ACCEPT, HeaderValue::from_static(PLAIN_ACCEPT));
        headers.insert(
            ACCEPT_ENCODING,
            HeaderValue::from_static(PLAIN_ACCEPT_ENCODING),
        );
        let mut callers_cookie = false;
        for (name, spelled, value) in caller.entries() {
            // A caller's credentials never cross to another origin.
            if !home && (*name == COOKIE || *name == AUTHORIZATION) {
                continue;
            }
            callers_cookie |= *name == COOKIE;
            if *name == USER_AGENT || *name == ACCEPT {
                headers.insert(name.clone(), value.clone());
                continue;
            }
            headers.append(name.clone(), value.clone());
            order.push((name.clone(), spelled.to_owned()));
        }
        let uri: Uri = url
            .as_str()
            .parse()
            .map_err(|_| HttpError::InvalidUrl(url.to_string()))?;
        let mut request = wreq::Request::new(method.clone(), uri);
        *request.headers_mut() = headers;
        if let Some(body) = body {
            *request.body_mut() = Some(wreq::Body::from(body));
        }
        let mut carrier = http::Request::new(());
        wreq_proto::ext::on_preserve_header(&mut carrier, PlainOrder { caller: order });
        request
            .extensions_mut()
            .extend(carrier.into_parts().0.extensions);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(HttpError::Timeout {
                stage: Stage::WaitingForResponse,
                after: started.elapsed(),
            });
        }
        let sending = wreq::RequestBuilder::from_parts(self.wreq.clone(), request)
            .default_headers(false)
            .cookie_provider(Arc::new(ChainCookies {
                jar: Arc::clone(jar),
                url: url.clone(),
                context: CookieContext {
                    same_site: true,
                    top_level_navigation: true,
                    safe_method: matches!(*method, Method::GET | Method::HEAD),
                },
                callers: callers_cookie,
            }))
            .timeout(remaining)
            .send();
        let phases = Phases::new();
        let sent = Instant::now();
        let response =
            match tokio::time::timeout_at(deadline, timing::observe(Arc::clone(&phases), sending))
                .await
            {
                Err(_) => {
                    return Err(HttpError::Timeout {
                        stage: if phases.is_connecting() {
                            Stage::Connecting
                        } else {
                            Stage::WaitingForResponse
                        },
                        after: started.elapsed(),
                    });
                }
                Ok(Err(error)) => {
                    let stage = if phases.is_connecting() {
                        Stage::Connecting
                    } else {
                        Stage::WaitingForResponse
                    };
                    return Err(from_wreq(&error, stage, started.elapsed()));
                }
                Ok(Ok(response)) => response,
            };
        if url.scheme() == "http" && response.status() == StatusCode::PROXY_AUTHENTICATION_REQUIRED
        {
            return Err(HttpError::ProxyAuth);
        }
        Ok(crate::fetch::Incoming {
            response,
            setup: phases.setup(),
            waited: sent.elapsed(),
        })
    }
}

/// Whether a redirect continues as a `GET` without the body: 303 always
/// (a `HEAD` stays one), 301 and 302 after a `POST`; 307 and 308 repeat
/// the request as it was.
fn continues_as_get(status: StatusCode, method: &Method) -> bool {
    match status {
        StatusCode::SEE_OTHER => *method != Method::HEAD,
        StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND => *method == Method::POST,
        _ => false,
    }
}

/// The headers of a response as received, in order.
fn headers_of(response: &wreq::Response) -> Vec<Header> {
    response
        .headers()
        .iter()
        .map(|(name, value)| Header {
            name: name.as_str().to_owned(),
            value: String::from_utf8_lossy(value.as_bytes()).into_owned(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use http::header::{HeaderMap, HeaderName, HeaderValue};
    use wreq_proto::ext::OnPreserveHeaderCallback;

    use super::PlainOrder;

    fn map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn host_comes_first_then_kages_headers_then_the_callers_in_their_spelling() {
        let order = PlainOrder {
            caller: vec![
                (HeaderName::from_static("x-trace"), "X-Trace".to_owned()),
                (HeaderName::from_static("cookie"), "cookie".to_owned()),
            ],
        };
        // As wreq hands it over: in any order, `host` last.
        let mut headers = map(&[
            ("cookie", "a=1"),
            ("accept-encoding", "gzip, deflate, br, zstd"),
            ("content-length", "3"),
            ("x-trace", "7"),
            ("accept", "*/*"),
            ("user-agent", "kage/2.0.0"),
            ("host", "example.org"),
        ]);
        let mut lines = Vec::new();
        order.call_visit(&mut headers, &mut |name, value| {
            lines.push((
                String::from_utf8_lossy(name.as_ref()).into_owned(),
                value.to_str().unwrap().to_owned(),
            ));
        });
        let names: Vec<&str> = lines.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Host",
                "User-Agent",
                "Accept",
                "Accept-Encoding",
                "X-Trace",
                "cookie",
                "content-length"
            ]
        );
        assert!(headers.is_empty());

        let mut fields = map(&[
            ("x-trace", "7"),
            ("accept", "*/*"),
            ("user-agent", "kage/2.0.0"),
            ("content-type", "text/plain"),
            ("accept-encoding", "gzip"),
        ]);
        order.call(&mut fields);
        let names: Vec<&str> = fields.keys().map(HeaderName::as_str).collect();
        assert_eq!(
            names,
            [
                "user-agent",
                "accept",
                "accept-encoding",
                "x-trace",
                "content-type"
            ]
        );
    }
}
