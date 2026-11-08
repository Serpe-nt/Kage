//! One fetch on the http path (spec §9): a navigation, its redirects, and
//! the response with its body.
//!
//! [`HttpClient::fetch`] sends a request the way the client's browser would
//! and follows redirects one hop at a time. Every hop
//!
//! - passes the SSRF guard again ([`kage_core::ssrf::RedirectChain`]) and,
//!   when the factory was given a resolver, has its name resolved and
//!   judged before it is asked for (`check_addresses`): a refused hop
//!   ends the fetch with [`HttpError::RedirectDenied`] and nothing fetched
//!   before it is returned;
//! - gets its own headers: the fetch metadata follow the whole chain, the
//!   `Referer` is cut down for the hop's origin, and the cookies are the
//!   ones a browser would send to that hop;
//! - stores the cookies its response sets.
//!
//! The body is read up to a cap; what lies beyond is dropped and the result
//! says so. The body returned is the content after `Content-Encoding` was
//! undone, as a browser hands it to a page.
//!
//! The http path never runs JavaScript and never pretends to (spec §9): a
//! `Refresh` header or a `<meta>` redirect is not followed.
//!
//! # Requests of a page's script
//!
//! A fetch is a navigation unless its [`FetchSpec::kind`] says it is a
//! `fetch()` or an `XMLHttpRequest`. Such a request has a page behind it,
//! named by the caller's `Referer`; without one it is refused, because
//! Kage never invents the page. It is sent as the browser sends it:
//!
//! - `Origin` whenever it is not a same-origin `GET` or `HEAD`; after a
//!   redirect that left an origin other than the page's for yet another
//!   one, `null`, as the Fetch standard says;
//! - cookies only to the page's own origin, and none are stored from
//!   another: the default of both APIs (`credentials: same-origin`);
//! - never where a browser would ask first. A cross-origin request that is
//!   not "simple" is preceded by a preflight in a browser; the preflight
//!   is not emulated, so such a request is refused
//!   ([`HttpError::PreflightRequired`]).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use http::header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue, LOCATION, REFERER};
use http::{Method, StatusCode, Uri, Version};
use http_body_util::BodyExt;
use kage_core::domain::{Header, Timing};
use kage_core::seed::Seed;
use kage_core::ssrf::{RedirectChain, SsrfDenial, SsrfPolicy, Target, check_resolved_with};
use tokio::time::Instant;
use url::Url;

use crate::client::{HopResolver, HttpClient};
use crate::cookies::{CookieContext, HopCookies};
use crate::error::{HttpError, Stage, from_wreq};
use crate::headers::{self, CallerHeaders, FetchSite, HeaderPlan, RequestKind};
use crate::site;
use crate::subresources::SubresourceReport;
use crate::timing::{self, ConnectionSetup, Phases};

/// How much of a redirect's own body is read before the connection is
/// given up on, so that the connection can be used for the next hop.
pub(crate) const REDIRECT_BODY_BYTES: usize = 64 * 1024;

/// Which checks a URL and its redirect hops must pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// A target a caller named: the SSRF guard checks the URL and every
    /// redirect hop (spec §19).
    Target(SsrfPolicy),
    /// An endpoint the operator configured, such as an echo of a health
    /// probe or of the self-test. It may be on this machine, so the guard
    /// does not apply. Never use this for a URL a caller supplied.
    OperatorEndpoint,
}

/// What to fetch.
#[derive(Debug, Clone)]
pub struct FetchSpec {
    /// The URL.
    pub url: Url,
    /// `GET`, `HEAD` or `POST`.
    pub method: Method,
    /// The caller's headers, checked against the denylist. A `Referer`
    /// names the page the request comes from and turns a typed navigation
    /// into a followed link.
    pub headers: Vec<(String, String)>,
    /// The request body.
    pub body: Option<Bytes>,
    /// What the request is: a navigation ([`RequestKind::Document`]), a
    /// `fetch()` or an `XMLHttpRequest`. The last two need a `Referer`.
    pub kind: RequestKind,
    /// Which checks the URL and its redirects must pass.
    pub guard: Guard,
    /// Longest the fetch may take; the client's setting when `None`.
    pub timeout: Option<Duration>,
    /// Read the body up to this many bytes; never more than the client's
    /// setting.
    pub max_body_bytes: Option<usize>,
    /// Redirect hops to follow; the client's setting when `None`.
    pub max_redirects: Option<usize>,
    /// Whether to fetch the page's own assets afterwards; the client's
    /// setting when `None`.
    pub fetch_subresources: Option<bool>,
    /// Seed of the pauses between the page and its assets. A fresh one is
    /// drawn and reported when `None`.
    pub seed: Option<Seed>,
}

impl FetchSpec {
    /// A `GET` of a caller's target, guarded by the SSRF policy.
    pub const fn get(url: Url, policy: SsrfPolicy) -> Self {
        Self {
            url,
            method: Method::GET,
            headers: Vec::new(),
            body: None,
            kind: RequestKind::Document,
            guard: Guard::Target(policy),
            timeout: None,
            max_body_bytes: None,
            max_redirects: None,
            fetch_subresources: None,
            seed: None,
        }
    }

    /// A `GET` of an endpoint the operator configured. Redirects are not
    /// followed and assets are not fetched: an echo has no reason for
    /// either.
    pub const fn operator_get(url: Url) -> Self {
        Self {
            url,
            method: Method::GET,
            headers: Vec::new(),
            body: None,
            kind: RequestKind::Document,
            guard: Guard::OperatorEndpoint,
            timeout: None,
            max_body_bytes: None,
            max_redirects: Some(0),
            fetch_subresources: Some(false),
            seed: None,
        }
    }

    /// The same fetch with one more caller header.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// One request that was sent, and what came back for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    /// The URL requested.
    pub url: Url,
    /// The response status.
    pub status: u16,
    /// `h2` or `http/1.1`.
    pub http_version: &'static str,
    /// The `Location` of a redirect; `None` for the final response.
    pub location: Option<String>,
    /// What opening the connection cost, when one was opened.
    pub setup: ConnectionSetup,
    /// From sending the request to the response headers.
    pub waited: Duration,
}

/// The result of a fetch.
#[derive(Debug, Clone, PartialEq)]
pub struct Fetched {
    /// The status of the final response.
    pub status: u16,
    /// The URL of the final response.
    pub final_url: Url,
    /// `h2` or `http/1.1`, of the final response.
    pub http_version: &'static str,
    /// The final response's headers, in the order received.
    pub headers: Vec<Header>,
    /// The body after `Content-Encoding` was undone, up to the cap.
    pub body: Bytes,
    /// Whether the body was longer than the cap and is cut off.
    pub truncated: bool,
    /// Every request sent, the final one last.
    pub hops: Vec<Hop>,
    /// Whether a redirect led from `https` to plain `http`.
    pub downgraded: bool,
    /// Time per phase. `connect_ms` and `tls_ms` add up the connections
    /// the hops opened and are `None` when every hop found one open;
    /// `ttfb_ms` runs from the start of the fetch to the headers of the
    /// final response; `total_ms` to the end of its body. There is no
    /// `dns_ms`: the proxy resolves the target, inside `connect_ms`.
    pub timing: Timing,
    /// What subresource emulation did; `None` when it was off or the
    /// response was not a page.
    pub subresources: Option<SubresourceReport>,
    /// The body as text, decoded once at its first use and shared by the
    /// clones (F213); a new `Fetched` starts with `TextCache::default()`.
    pub text_cache: TextCache,
}

/// The text of a [`Fetched`] body and its encoding, decoded at the first
/// use and kept: the subresource emulation, the classifier and the reader
/// of a page each read it, and each decoded it again (F213). It says
/// nothing of the response: two caches always compare equal. It holds the
/// text of the body it was filled from; a body changed afterwards is
/// decoded on its own, never read from it.
#[derive(Clone, Default)]
pub struct TextCache(Arc<OnceLock<DecodedBody>>);

/// What a [`TextCache`] keeps: which body it was decoded from, the text,
/// and the encoding.
struct DecodedBody {
    /// Where the body's bytes were and how many, when they were decoded.
    of: (usize, usize),
    text: String,
    encoding: &'static encoding_rs::Encoding,
}

impl std::fmt::Debug for TextCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextCache")
            .field("filled", &self.0.get().is_some())
            .finish()
    }
}

impl PartialEq for TextCache {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Fetched {
    /// The first value of a response header.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(|header| header.value.as_str())
    }

    /// The redirects that led to the final response.
    pub fn redirects(&self) -> &[Hop] {
        &self.hops[..self.hops.len().saturating_sub(1)]
    }

    /// The body as text, read in the encoding a browser would take for it
    /// ([`crate::charset::decode_from`]: its byte order mark, the `charset`
    /// of its `Content-Type`, its `<meta>`, the detector's guess for the
    /// site). Bytes that are not valid in that encoding are replaced. Until
    /// F100 every body was read as UTF-8.
    pub fn text(&self) -> String {
        self.decoded().0
    }

    /// The body as text ([`Fetched::text`]) and the encoding it was read
    /// in: the document's encoding, in which a link's query is written
    /// (F161).
    pub fn decoded(&self) -> (String, &'static encoding_rs::Encoding) {
        let (text, encoding) = self.decoded_ref();
        (text.into_owned(), encoding)
    }

    /// [`Fetched::decoded`] without a copy: the text decoded at the first
    /// call is lent from [`Fetched::text_cache`].
    pub fn decoded_ref(&self) -> (std::borrow::Cow<'_, str>, &'static encoding_rs::Encoding) {
        let of = (self.body.as_ptr().addr(), self.body.len());
        let kept = self.text_cache.0.get_or_init(|| self.decode_now(of));
        if kept.of == of {
            return (std::borrow::Cow::Borrowed(&kept.text), kept.encoding);
        }
        let fresh = self.decode_now(of);
        (std::borrow::Cow::Owned(fresh.text), fresh.encoding)
    }

    fn decode_now(&self, of: (usize, usize)) -> DecodedBody {
        // A body cut at `max_body_bytes` ends at its last whole character
        // (QA round three's extract-08).
        let decoded = crate::charset::decode_cut_from(
            Some(&self.final_url),
            self.header("content-type"),
            &self.body,
            self.truncated,
        );
        DecodedBody {
            of,
            text: decoded.text.into_owned(),
            encoding: decoded.encoding,
        }
    }
}

/// Whether the answer to a plain `http://` request is the proxy's: a 407
/// with the `Proxy-Authenticate` a proxy that asks for credentials sends
/// (RFC 9110 §15.5.8, §11.7.1). A 407 without it is the site's own answer,
/// returned as `ok` with its status like any other 4xx (spec §8.3; QA round
/// one's policy-11, F143). Through a tunnel only the site answers.
pub(crate) fn proxy_asked(url: &Url, status: StatusCode, headers: &http::HeaderMap) -> bool {
    url.scheme() == "http"
        && status == StatusCode::PROXY_AUTHENTICATION_REQUIRED
        && headers.contains_key(http::header::PROXY_AUTHENTICATE)
}

/// The URLs of a fetch, hop by hop, with the checks its guard asks for.
pub(crate) enum Trail {
    Guarded(RedirectChain),
    Open { hops: Vec<Url>, max: usize },
}

impl Trail {
    pub(crate) fn start(url: &Url, guard: Guard, max: usize) -> Result<Self, HttpError> {
        match guard {
            Guard::Target(policy) => RedirectChain::start(url, policy, max)
                .map(Self::Guarded)
                .map_err(HttpError::Denied),
            Guard::OperatorEndpoint => {
                if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
                    return Err(HttpError::InvalidUrl(url.to_string()));
                }
                Ok(Self::Open {
                    hops: vec![url.clone()],
                    max,
                })
            }
        }
    }

    pub(crate) fn current(&self) -> &Url {
        match self {
            Self::Guarded(chain) => chain.current(),
            // `hops` always holds the first URL.
            Self::Open { hops, .. } => &hops[hops.len() - 1],
        }
    }

    pub(crate) fn redirects(&self) -> usize {
        match self {
            Self::Guarded(chain) => chain.redirects(),
            Self::Open { hops, .. } => hops.len() - 1,
        }
    }

    /// The first hop that left `https` for `http`: its number in the
    /// chain (1 for the first redirect) and the addresses on either side.
    pub(crate) fn downgrade(&self) -> Option<(usize, &Url, &Url)> {
        let urls: Vec<&Url> = match self {
            Self::Guarded(chain) => chain.hops().iter().map(|hop| &hop.url).collect(),
            Self::Open { hops, .. } => hops.iter().collect(),
        };
        urls.windows(2)
            .enumerate()
            .find(|(_, pair)| pair[0].scheme() == "https" && pair[1].scheme() == "http")
            .map(|(at, pair)| (at + 1, pair[0], pair[1]))
    }

    /// Whether the chain left `https` for `http`; a downgrade that was
    /// followed, as a browser follows it, is recorded (spec §9, QA round
    /// three's level0-05): kage's log names the hop and the hosts, never the
    /// addresses, whose paths and queries may be the caller's.
    pub(crate) fn note_downgrade(&self) -> bool {
        let Some((hop, from, to)) = self.downgrade() else {
            return false;
        };
        tracing::info!(
            hop,
            from_host = ?from.host_str(),
            to_host = ?to.host_str(),
            "a redirect of the http path downgraded https to http; it was followed, as a browser follows it"
        );
        true
    }

    pub(crate) fn follow(&mut self, location: &str) -> Result<Url, HttpError> {
        match self {
            Self::Guarded(chain) => match chain.follow(location) {
                Ok(next) => Ok(next.clone()),
                Err(denied) => Err(match denied.reason {
                    SsrfDenial::TooManyRedirects(max) => HttpError::TooManyRedirects {
                        max,
                        location: denied.location,
                    },
                    _ => HttpError::RedirectDenied(denied),
                }),
            },
            Self::Open { hops, max } => {
                if hops.len() > *max {
                    return Err(HttpError::TooManyRedirects {
                        max: *max,
                        location: location.trim().to_owned(),
                    });
                }
                let next = hops[hops.len() - 1]
                    .join(location.trim())
                    .ok()
                    .filter(|next| matches!(next.scheme(), "http" | "https"))
                    .ok_or_else(|| HttpError::InvalidUrl(location.trim().to_owned()))?;
                hops.push(next.clone());
                Ok(next)
            }
        }
    }
}

/// One request to send.
pub(crate) struct Outgoing<'a> {
    pub kind: RequestKind,
    pub method: &'a Method,
    pub url: &'a Url,
    pub site: FetchSite,
    /// The page the request comes from.
    pub referrer: Option<&'a Url>,
    /// The request names no origin but `null`: a scripted request whose
    /// redirect chain crossed origins twice.
    pub opaque_origin: bool,
    /// Whether cookies are sent with the request and stored from its
    /// response. A script's request to another origin carries none.
    pub credentials: bool,
    pub body: Option<Bytes>,
    pub caller: &'a CallerHeaders,
    pub conditional: &'a [(HeaderName, HeaderValue)],
    /// When the fetch this request belongs to began, for error messages.
    pub started: Instant,
    pub deadline: Instant,
}

/// A response whose headers have arrived.
pub(crate) struct Incoming {
    pub response: wreq::Response,
    pub setup: ConnectionSetup,
    pub waited: Duration,
}

/// The response a fetch ended on, and what the hops to it added up to.
struct Arrived {
    response: wreq::Response,
    /// From the start of the fetch to this response's headers.
    first_byte: Duration,
    /// The method of the final request: a redirect can turn a POST into a
    /// GET.
    method: Method,
    hops: Vec<Hop>,
    connect: Option<Duration>,
    tls: Option<Duration>,
}

pub(crate) const fn version_name(version: Version) -> &'static str {
    match version {
        Version::HTTP_2 => "h2",
        Version::HTTP_3 => "h3",
        Version::HTTP_10 => "http/1.0",
        _ => "http/1.1",
    }
}

/// The content types a script may send to another origin without the
/// browser asking first (the Fetch standard's CORS-safelisted values).
const SIMPLE_CONTENT_TYPES: [&str; 3] = [
    "application/x-www-form-urlencoded",
    "multipart/form-data",
    "text/plain",
];

/// Refuses a script's request that a browser would preflight: one to
/// another origin than its page's that sets a header beyond the safelist
/// or a content type outside it. The methods the http path sends (`GET`,
/// `HEAD`, `POST`) are all safelisted.
///
/// # Errors
///
/// [`HttpError::PreflightRequired`], saying what makes the request one a
/// browser asks about.
pub fn check_preflight(
    page: &Url,
    target: &Url,
    method: &Method,
    caller: &CallerHeaders,
) -> Result<(), HttpError> {
    if site::same_origin(page, target) {
        return Ok(());
    }
    let refused = |reason: &'static str| {
        Err(HttpError::PreflightRequired {
            reason,
            method: method.to_string(),
        })
    };
    if let Some(content_type) = caller.get(&CONTENT_TYPE) {
        let essence = content_type
            .to_str()
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !SIMPLE_CONTENT_TYPES.contains(&essence.as_str()) {
            return refused("its Content-Type is not one a form could send");
        }
    }
    if caller
        .names()
        .any(|name| !(name == CONTENT_TYPE || name == ACCEPT || name == REFERER))
    {
        return refused("it sets a header a page may not send across origins unasked");
    }
    Ok(())
}

/// The address check of the SSRF guard (spec §19) for the hop a trail
/// points at: the hop's name is resolved with `resolver`, and the hop is
/// refused when any of its addresses lies in a denied range or when it has
/// none. A name the resolver cannot look up counts as one with no address:
/// a hop nobody can resolve is not asked for. Nothing is looked up without
/// a resolver (the proxy resolves then; see [`HopResolver`]), for a literal
/// address (judged by the URL already) or for an operator's endpoint.
///
/// Both clients of the http path run it before every request they send,
/// the persona's and the plain one of level 0, so that a redirect hop is
/// judged before it is asked for, as the first URL is. A refused hop ends
/// the fetch; nothing of the hop is requested, read, returned or kept.
pub(crate) async fn check_addresses(
    resolver: Option<&dyn HopResolver>,
    trail: &Trail,
) -> Result<(), HttpError> {
    let (Some(resolver), Trail::Guarded(chain)) = (resolver, trail) else {
        return Ok(());
    };
    let Some(name) = chain.target().name() else {
        return Ok(());
    };
    check_target_addresses(resolver, chain.target())
        .await
        .map_err(|reason| {
            let hop = chain.redirects();
            tracing::info!(
                host = name,
                hop,
                %reason,
                "the SSRF guard refused a hop of the http path by its addresses"
            );
            if hop == 0 {
                HttpError::Denied(reason)
            } else {
                HttpError::RedirectDenied(chain.denied(reason))
            }
        })
}

/// The address check of the SSRF guard (spec §19) for one target the URL
/// check let through: its name is resolved with `resolver`, and it is
/// refused when any of its addresses lies in a denied range or when it has
/// none. A name the resolver cannot look up counts as one with no address
/// (F98). A literal address was judged by the URL already and is not
/// looked up.
///
/// The http path runs it on every hop before the hop is asked for
/// ([`check_addresses`]); the browser path runs the same function on every
/// hop of a navigation's chain and of a flow's pages after the fact (F57),
/// so that both roads judge a name by the same rule.
///
/// # Errors
///
/// The guard's refusal: [`SsrfDenial::Address`] or
/// [`SsrfDenial::Unresolved`].
pub async fn check_target_addresses(
    resolver: &dyn HopResolver,
    target: &Target,
) -> Result<(), SsrfDenial> {
    let Some(name) = target.name() else {
        return Ok(());
    };
    let addresses = match resolver.resolve(name).await {
        Ok(addresses) => addresses,
        Err(reason) => {
            tracing::debug!(host = name, reason, "a name could not be looked up");
            Vec::new()
        }
    };
    // A name the operator listed for the host's own connection may resolve
    // into the operator's networks, as the forwarder lets it (F181).
    check_resolved_with(target, &addresses, resolver.may_be_private(name))
}

pub(crate) fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(crate) fn add(total: Option<Duration>, part: Option<Duration>) -> Option<Duration> {
    match (total, part) {
        (Some(total), Some(part)) => Some(total + part),
        (total, part) => total.or(part),
    }
}

/// Where a redirect points, for the statuses a browser follows.
pub(crate) fn redirect_location(response: &wreq::Response) -> Option<String> {
    let status = response.status();
    let follows = matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    );
    if !follows {
        return None;
    }
    let location = response.headers().get(LOCATION)?;
    let text = String::from_utf8_lossy(location.as_bytes())
        .trim()
        .to_owned();
    (!text.is_empty()).then_some(text)
}

/// Reads a body up to `cap` bytes. The flag says whether more was there.
pub(crate) async fn read_body(
    response: wreq::Response,
    cap: usize,
    started: Instant,
    deadline: Instant,
) -> Result<(Bytes, bool), HttpError> {
    let mut stream = Box::pin(response);
    let mut body = BytesMut::new();
    loop {
        let frame = match tokio::time::timeout_at(deadline, stream.frame()).await {
            Err(_) => {
                return Err(HttpError::Timeout {
                    stage: Stage::ReadingBody,
                    after: started.elapsed(),
                });
            }
            Ok(None) => return Ok((body.freeze(), false)),
            Ok(Some(Err(error))) => {
                return Err(from_wreq(&error, Stage::ReadingBody, started.elapsed()));
            }
            Ok(Some(Ok(frame))) => frame,
        };
        // Trailers carry no content.
        let Ok(data) = frame.into_data() else {
            continue;
        };
        let room = cap - body.len();
        if data.len() > room {
            body.extend_from_slice(&data[..room]);
            // Dropping the response abandons the rest of the transfer.
            return Ok((body.freeze(), true));
        }
        body.extend_from_slice(&data);
    }
}

impl HttpClient {
    /// Fetches a URL as the client's browser would navigate to it.
    ///
    /// Cookies the responses set are kept and saved even when the fetch
    /// then fails, as a browser keeps them.
    ///
    /// # Errors
    ///
    /// Fails when a header or the URL is refused, a redirect hop is refused
    /// or one too many, the time runs out, or the exchange breaks. A
    /// response with an error status is not an error: it is returned.
    pub async fn fetch(&self, spec: &FetchSpec) -> Result<Fetched, HttpError> {
        self.fetch_traced(spec, &mut Vec::new()).await
    }

    /// [`Self::fetch`], with every address a request was sent to pushed onto
    /// `asked`, in order, the one that failed included: the first URL and
    /// each redirect hop. A failure does not say where it happened; this
    /// does (F102: a hop the forwarder refused under `egress.mode: direct`).
    ///
    /// # Errors
    ///
    /// As [`Self::fetch`].
    pub async fn fetch_traced(
        &self,
        spec: &FetchSpec,
        asked: &mut Vec<Url>,
    ) -> Result<Fetched, HttpError> {
        let outcome = self.navigate(spec, asked).await;
        self.persist().await;
        outcome
    }

    async fn navigate(&self, spec: &FetchSpec, asked: &mut Vec<Url>) -> Result<Fetched, HttpError> {
        if !matches!(spec.method, Method::GET | Method::HEAD | Method::POST) {
            return Err(HttpError::Method(spec.method.to_string()));
        }
        // Only a navigation and a script's request are a caller's to ask
        // for; a page's own assets are fetched by the emulation.
        if !(spec.kind == RequestKind::Document || spec.kind.is_scripted()) {
            return Err(HttpError::Kind(spec.kind.dest()));
        }
        let caller = CallerHeaders::parse(&spec.headers)?;
        let referrer = match caller.referer() {
            None => None,
            Some(text) => Some(
                Url::parse(text)
                    .ok()
                    .filter(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
                    .ok_or_else(|| HttpError::InvalidReferer(text.to_owned()))?,
            ),
        };
        if spec.kind.is_scripted() {
            let Some(page) = &referrer else {
                return Err(HttpError::InitiatorRequired);
            };
            check_preflight(page, &spec.url, &spec.method, &caller)?;
        }
        let max_redirects = spec.max_redirects.unwrap_or(self.settings.max_redirects);
        let cap = spec
            .max_body_bytes
            .unwrap_or(self.settings.max_body_bytes)
            .min(self.settings.max_body_bytes);
        let mut trail = Trail::start(&spec.url, spec.guard, max_redirects)?;
        let started = Instant::now();
        let deadline = started + spec.timeout.unwrap_or(self.settings.timeout);

        let arrived = self
            .follow(
                spec,
                (&mut trail, asked),
                caller,
                referrer.as_ref(),
                started,
                deadline,
            )
            .await?;

        let response = arrived.response;
        let status = response.status();
        let http_version = version_name(response.version());
        let headers: Vec<Header> = response
            .headers()
            .iter()
            .map(|(name, value)| Header {
                name: name.as_str().to_owned(),
                value: String::from_utf8_lossy(value.as_bytes()).into_owned(),
            })
            .collect();
        let (body, truncated) = if arrived.method == Method::HEAD {
            (Bytes::new(), false)
        } else {
            read_body(response, cap, started, deadline).await?
        };
        let total = started.elapsed();

        let mut fetched = Fetched {
            status: status.as_u16(),
            final_url: trail.current().clone(),
            http_version,
            headers,
            body,
            truncated,
            hops: arrived.hops,
            downgraded: trail.note_downgrade(),
            timing: Timing {
                dns_ms: None,
                connect_ms: arrived.connect.map(millis),
                tls_ms: arrived.tls.map(millis),
                ttfb_ms: Some(millis(arrived.first_byte)),
                dom_content_loaded_ms: None,
                load_ms: None,
                total_ms: millis(total),
            },
            subresources: None,
            text_cache: TextCache::default(),
        };
        // Only a page that was asked for with a GET has a first paint.
        if arrived.method == Method::GET
            && spec.kind == RequestKind::Document
            && spec
                .fetch_subresources
                .unwrap_or(self.settings.fetch_subresources)
        {
            fetched.subresources = self
                .fetch_assets(&fetched, spec.guard, spec.seed, deadline)
                .await;
        }
        tracing::debug!(
            proxy = %self.proxy,
            host = ?fetched.final_url.host_str(),
            status = fetched.status,
            redirects = trail.redirects(),
            http_version,
            total_ms = fetched.timing.total_ms,
            "http fetch finished"
        );
        Ok(fetched)
    }

    /// Sends the request and follows its redirects, hop by hop, until a
    /// response arrives that is not one.
    async fn follow(
        &self,
        spec: &FetchSpec,
        (trail, asked): (&mut Trail, &mut Vec<Url>),
        mut caller: CallerHeaders,
        referrer: Option<&Url>,
        started: Instant,
        deadline: Instant,
    ) -> Result<Arrived, HttpError> {
        let mut method = spec.method.clone();
        let mut body = spec.body.clone();
        let mut site = referrer.map_or(FetchSite::None, |page| {
            FetchSite::between(page, trail.current())
        });
        let mut hops = Vec::new();
        let (mut connect, mut tls) = (None, None);
        // Whether the chain of a script's request left an origin that was
        // not its page's for yet another one.
        let mut opaque_origin = false;
        loop {
            check_addresses(self.resolver.as_deref(), trail).await?;
            let url = trail.current().clone();
            asked.push(url.clone());
            let incoming = self
                .send(Outgoing {
                    kind: spec.kind,
                    method: &method,
                    url: &url,
                    site,
                    referrer,
                    opaque_origin,
                    // A script's request carries cookies to its page's
                    // own origin only.
                    credentials: !spec.kind.is_scripted()
                        || referrer.is_some_and(|page| site::same_origin(page, &url)),
                    body: body.clone(),
                    caller: &caller,
                    conditional: &[],
                    started,
                    deadline,
                })
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
                return Ok(Arrived {
                    response: incoming.response,
                    first_byte,
                    method,
                    hops,
                    connect,
                    tls,
                });
            };
            // A browser does not read the body of a redirect. A short one
            // is read so that the connection is free for the next hop.
            let _ = read_body(incoming.response, REDIRECT_BODY_BYTES, started, deadline).await;
            let previous = trail.current().clone();
            let next = trail.follow(&location)?;
            site = site.after_redirect(referrer, &next);
            if spec.kind.is_scripted()
                && let Some(page) = referrer
            {
                if !site::same_origin(&previous, &next) && !site::same_origin(page, &previous) {
                    opaque_origin = true;
                }
                // A redirect that takes the request to another origin makes
                // it one a browser would have asked about first.
                check_preflight(page, &next, &method, &caller)?;
            }
            // 303 always, and 301 and 302 for a POST, continue as a GET
            // without the body; 307 and 308 repeat the request as it was.
            let to_get = match status {
                StatusCode::SEE_OTHER => method != Method::HEAD,
                StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND => method == Method::POST,
                _ => false,
            };
            if to_get {
                method = Method::GET;
                body = None;
                caller = caller.without(&CONTENT_TYPE);
            }
        }
    }

    /// Sends one request with the headers and cookies the client's browser
    /// would give it, and waits for the response headers.
    #[allow(clippy::too_many_lines)] // One request, from its plan to its response's head.
    pub(crate) async fn send(&self, outgoing: Outgoing<'_>) -> Result<Incoming, HttpError> {
        let Outgoing {
            kind,
            method,
            url,
            site,
            referrer,
            opaque_origin,
            credentials,
            body,
            caller,
            conditional,
            started,
            deadline,
        } = outgoing;
        let referer = referrer.and_then(|page| site::referer_for(page, url));
        let origin = if opaque_origin {
            Some("null".to_owned())
        } else {
            referrer.map(site::serialize_origin)
        };
        let (headers, order) = headers::build(&HeaderPlan {
            identity: &self.identity,
            kind,
            method,
            url,
            site,
            referer: referer.as_deref(),
            origin: origin.as_deref(),
            has_body: body.is_some(),
            caller,
            conditional,
        })?;
        let uri: Uri = url
            .as_str()
            .parse()
            .map_err(|_| HttpError::InvalidUrl(url.to_string()))?;

        let mut request = wreq::Request::new(method.clone(), uri);
        *request.headers_mut() = headers;
        if let Some(body) = body {
            *request.body_mut() = Some(wreq::Body::from(body));
        }
        // The hook that orders the headers travels in the request's
        // extensions. wreq-proto installs it on an `http::Request`.
        let mut carrier = http::Request::new(());
        wreq_proto::ext::on_preserve_header(&mut carrier, order);
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
        // A cookie counts as sent to its own site unless a page of another
        // site caused this request; a redirect chain does not change that.
        let cookies = CookieContext {
            same_site: referrer.is_none_or(|page| site::same_site(page, url)),
            top_level_navigation: kind == RequestKind::Document,
            safe_method: matches!(*method, Method::GET | Method::HEAD),
        };
        let sending = wreq::RequestBuilder::from_parts(self.wreq.clone(), request)
            .default_headers(false)
            .cookie_provider(Arc::new(
                HopCookies::new(Arc::clone(&self.jar), url.clone(), cookies)
                    .with_credentials(credentials),
            ))
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
        // A plain request goes to the proxy itself, which answers 407 when
        // it does not accept the login.
        if proxy_asked(url, response.status(), response.headers()) {
            return Err(HttpError::ProxyAuth);
        }
        Ok(Incoming {
            response,
            setup: phases.setup(),
            waited: sent.elapsed(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::sync::Arc;

    use bytes::Bytes;
    use encoding_rs::{UTF_8, WINDOWS_1251};
    use kage_core::domain::{Header, Timing};
    use url::Url;

    use super::{Fetched, TextCache};

    fn page(body: &'static [u8]) -> Fetched {
        Fetched {
            status: 200,
            final_url: Url::parse("http://shop.kage.test/").unwrap(),
            http_version: "http/1.1",
            headers: vec![Header {
                name: "Content-Type".to_owned(),
                value: "text/html".to_owned(),
            }],
            body: Bytes::from_static(body),
            truncated: false,
            hops: Vec::new(),
            downgraded: false,
            timing: Timing::default(),
            subresources: None,
            text_cache: TextCache::default(),
        }
    }

    /// F213: the subresource emulation, the classifier and the reader of a
    /// page each asked for its text and each decoded the body again. It is
    /// decoded once, and every clone of the fetch lends the same text.
    #[test]
    fn a_body_is_decoded_once_and_its_clones_share_the_text() {
        let fetched = page(b"<meta charset=windows-1251><p>\xcf\xf0\xe8</p>");
        assert!(fetched.text_cache.0.get().is_none());
        let (text, encoding) = fetched.decoded();
        assert_eq!(encoding, WINDOWS_1251);
        assert!(text.contains("При"), "{text}");
        assert!(fetched.text_cache.0.get().is_some());

        let copy = fetched.clone();
        assert!(Arc::ptr_eq(&fetched.text_cache.0, &copy.text_cache.0));
        let (lent, encoding) = copy.decoded_ref();
        assert!(matches!(lent, Cow::Borrowed(_)));
        assert_eq!((lent.as_ref(), encoding), (text.as_str(), WINDOWS_1251));
        assert_eq!(copy.text(), text);

        // A body put in afterwards is read on its own, never from the
        // cache of the one before.
        let mut changed = fetched.clone();
        changed.body = Bytes::from_static(b"<meta charset=utf-8><p>ok</p>");
        let (other, encoding) = changed.decoded_ref();
        assert!(matches!(other, Cow::Owned(_)));
        assert_eq!(encoding, UTF_8);
        assert!(other.contains("ok"));
        assert_eq!(fetched.text(), text);

        // The cache is not part of the response.
        assert_eq!(page(b"x"), fetched_and_read(page(b"x")));
    }

    fn fetched_and_read(fetched: Fetched) -> Fetched {
        let _ = fetched.text();
        fetched
    }

    /// QA round three's level0-05: the hop that left `https` for `http`
    /// is found, to be recorded, on a guarded chain and on an open one.
    #[test]
    fn the_hop_that_left_https_for_http_is_found() {
        let url = |text: &str| Url::parse(text).unwrap();
        let policy = kage_core::ssrf::SsrfPolicy::default();
        let mut guarded = super::Trail::start(
            &url("https://shop.example/a"),
            super::Guard::Target(policy),
            10,
        )
        .unwrap();
        assert_eq!(guarded.downgrade(), None);
        assert!(!guarded.note_downgrade());
        guarded.follow("https://shop.example/b").unwrap();
        guarded.follow("http://shop.example/c").unwrap();
        guarded.follow("http://shop.example/d").unwrap();
        let (hop, from, to) = guarded.downgrade().unwrap();
        assert_eq!(
            (hop, from.as_str(), to.as_str()),
            (2, "https://shop.example/b", "http://shop.example/c")
        );
        assert!(guarded.note_downgrade());

        let mut open = super::Trail::start(
            &url("http://hook.example/"),
            super::Guard::OperatorEndpoint,
            10,
        )
        .unwrap();
        open.follow("https://hook.example/x").unwrap();
        assert_eq!(open.downgrade(), None);
        open.follow("http://hook.example/y").unwrap();
        assert_eq!(open.downgrade().map(|(hop, _, _)| hop), Some(2));
        assert!(open.note_downgrade());
    }
}
