//! The headers of a request: which ones, with which values, in which order
//! and spelling (spec §9).
//!
//! kage-http owns the complete header list. A wreq-util profile sends one
//! fixed set, a typed-URL navigation over HTTP/2 to an `https` origin, for
//! every request, and appends whatever a request adds at the end. A browser
//! does neither: its headers depend on what kind of request it is, on where
//! it comes from, on the scheme and on the HTTP version, and `Referer` and
//! `Cookie` sit in the middle of the list. So the profile only supplies the
//! values of the identity headers ([`crate::profile::BaseHeaders`]) and this
//! module decides everything else:
//!
//! - [`build`] returns the header map of one request and a [`HeaderOrder`].
//! - The [`HeaderOrder`] is handed to wreq as the hook that runs when the
//!   request is serialized. On HTTP/2 it sorts the fields; on HTTP/1.1 it
//!   writes them in order with the browser's spelling, adds the connection
//!   header and leaves out the fields a browser only sends on HTTP/2. wreq
//!   itself adds `Host`, `Content-Length`, `Cookie` (from the jar, once it
//!   knows the HTTP version) and, for a plain request through the proxy,
//!   `Proxy-Authorization`; the order puts those in their places too.
//!
//! # What the tables encode
//!
//! One ordered table per browser family and kind of request
//! ([`RequestKind`]), valid for the current generation of each browser (the
//! majors [`crate::profile::mapping_table`] covers). The document tables are
//! checked on the wire by the fingerprint self-test against the in-repo
//! echo. The subresource tables and the positions of headers a navigation
//! rarely carries (`Authorization`, the conditional headers, a form POST)
//! are written from published captures and are confirmed when M4 and M5
//! record the real browsers against the same echo.
//!
//! # What a caller may set
//!
//! [`CallerHeaders::parse`] refuses, with an error naming the header, every
//! header that is part of the persona's identity or of the transport (spec
//! §9 and what a browser itself forbids a page to set). A header the family
//! has a place for goes to that place; any other name goes directly before
//! `Cookie`.
//!
//! # Plain `http://` origins
//!
//! A browser sends client hints and fetch metadata only to a potentially
//! trustworthy origin, and offers `br` and `zstd` only there. The same holds
//! here ([`crate::site::is_trustworthy`]).

use std::fmt::{self, Write as _};

use http::Method;
use http::header::{
    ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE, CACHE_CONTROL, HeaderMap, HeaderName, HeaderValue,
    ORIGIN, REFERER, USER_AGENT,
};
use kage_core::domain::ClaimedOs;
use url::Url;
use wreq_proto::ext::OnPreserveHeaderCallback;

use crate::profile::{ClientHints, Family, ProfileChoice};
use crate::site;

/// Most headers one request may carry on behalf of a caller.
pub const MAX_CALLER_HEADERS: usize = 64;

/// The languages a client prefers when none are given: what both browsers
/// send out of the box in an English (United States) installation.
pub const DEFAULT_LANGUAGES: [&str; 2] = ["en-US", "en"];

/// What a request is for. It decides `Accept`, the fetch metadata and the
/// place of every header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestKind {
    /// A top-level navigation: a typed address, a bookmark or a followed
    /// link. The only kind a caller can ask for.
    Document,
    /// A stylesheet a document links to.
    Style,
    /// A script a document links to.
    Script,
    /// An image a document links to; here, its icon.
    Image,
}

impl RequestKind {
    /// The `Sec-Fetch-Dest` value.
    pub const fn dest(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Style => "style",
            Self::Script => "script",
            Self::Image => "image",
        }
    }

    /// The `Sec-Fetch-Mode` value.
    pub const fn mode(self) -> &'static str {
        match self {
            Self::Document => "navigate",
            Self::Style | Self::Script | Self::Image => "no-cors",
        }
    }

    const fn is_document(self) -> bool {
        matches!(self, Self::Document)
    }
}

/// The `Sec-Fetch-Site` value of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FetchSite {
    /// The user caused it directly: a typed address or a bookmark.
    None,
    /// Initiated by a page of the same origin.
    SameOrigin,
    /// Initiated by a page of the same site but another origin.
    SameSite,
    /// Initiated by a page of another site.
    CrossSite,
}

impl FetchSite {
    /// The header value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::SameOrigin => "same-origin",
            Self::SameSite => "same-site",
            Self::CrossSite => "cross-site",
        }
    }

    /// The value for a request to `target` that a page at `initiator`
    /// caused.
    pub fn between(initiator: &Url, target: &Url) -> Self {
        match site::relation(initiator, target) {
            site::Relation::SameOrigin => Self::SameOrigin,
            site::Relation::SameSite => Self::SameSite,
            site::Relation::CrossSite => Self::CrossSite,
        }
    }

    /// The value after a redirect to `target`: a chain is as far from its
    /// initiator as its furthest hop, and a navigation the user caused
    /// directly stays `none`.
    #[must_use]
    pub fn after_redirect(self, initiator: Option<&Url>, target: &Url) -> Self {
        match (self, initiator) {
            (Self::None, _) | (_, None) => self,
            (_, Some(initiator)) => self.max(Self::between(initiator, target)),
        }
    }

    /// Whether a browser treats the request as coming from the target's own
    /// site when it chooses cookies.
    pub const fn is_same_site(self) -> bool {
        !matches!(self, Self::CrossSite)
    }
}

impl fmt::Display for FetchSite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a set of headers was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HeaderError {
    /// A caller tried to set a header it may not set.
    #[error("the header {name:?} cannot be set by a caller: {reason}")]
    Denied {
        /// The header, as the caller wrote it.
        name: String,
        /// Why it is refused.
        reason: &'static str,
    },
    /// A header name is not a valid field name.
    #[error("{0:?} is not a valid header name")]
    InvalidName(String),
    /// A header value holds a control character or is not ASCII.
    #[error("the value of the header {name:?} is not a valid field value")]
    InvalidValue {
        /// The header.
        name: String,
    },
    /// The same header was given twice.
    #[error("the header {0:?} is given more than once")]
    Duplicate(String),
    /// More headers than one request may carry.
    #[error("{count} headers given, at most {max} are allowed")]
    TooMany {
        /// How many were given.
        count: usize,
        /// The limit.
        max: usize,
    },
    /// A language tag is not well formed.
    #[error("{0:?} is not a language tag such as de-DE")]
    InvalidLanguage(String),
    /// The language list is empty.
    #[error("a client needs at least one language")]
    NoLanguages,
    /// The profile's own identity must not be used; one has to be supplied.
    #[error("{profile} needs an explicit {field}: {reason}")]
    IdentityRequired {
        /// The profile.
        profile: String,
        /// What is missing.
        field: &'static str,
        /// Why the profile's own value is refused.
        reason: &'static str,
    },
    /// A supplied identity value is not a valid header value.
    #[error("the supplied {0} is not a valid header value")]
    InvalidIdentity(&'static str),
}

/// Identity values that replace the profile's own. Only code that knows the
/// persona supplies them (milestone M6); a caller of the API never can.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdentityOverride {
    /// The exact `User-Agent` of the persona's browser.
    pub user_agent: Option<String>,
    /// The exact `sec-ch-ua` brand list of the persona's browser.
    pub client_hint_brands: Option<String>,
}

/// Who a client is: the values behind the identity headers of every request
/// it sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    family: Family,
    user_agent: HeaderValue,
    client_hints: Option<ClientHints>,
    accept_document: HeaderValue,
    accept_encoding: HeaderValue,
    accept_language: HeaderValue,
}

impl Identity {
    /// The identity of a profile, speaking `languages` (most preferred
    /// first), with the persona's own values where `overrides` has them.
    ///
    /// # Errors
    ///
    /// Fails when the language list is empty or malformed, when an override
    /// is not a valid header value, and when the profile's own `User-Agent`
    /// must not be used: wreq-util's Firefox on Linux claims an Ubuntu build
    /// (`X11; Ubuntu; Linux x86_64`), which no Camoufox persona is, so that
    /// combination needs the persona's `User-Agent`.
    pub fn new<L: AsRef<str>>(
        choice: &ProfileChoice,
        languages: &[L],
        overrides: &IdentityOverride,
    ) -> Result<Self, HeaderError> {
        let base = choice
            .base_headers()
            .map_err(|_| HeaderError::InvalidIdentity("emulation profile"))?;
        let user_agent = match &overrides.user_agent {
            Some(user_agent) => HeaderValue::from_str(user_agent)
                .map_err(|_| HeaderError::InvalidIdentity("User-Agent"))?,
            None if choice.family() == Family::Firefox && choice.os() == ClaimedOs::Linux => {
                return Err(HeaderError::IdentityRequired {
                    profile: choice.to_string(),
                    field: "User-Agent",
                    reason: "the profile's own claims an Ubuntu build of Firefox, which is not \
                             what a persona runs",
                });
            }
            None => base.user_agent,
        };
        let client_hints = match (base.client_hints, &overrides.client_hint_brands) {
            (Some(hints), Some(brands)) => Some(ClientHints {
                brands: HeaderValue::from_str(brands)
                    .map_err(|_| HeaderError::InvalidIdentity("sec-ch-ua"))?,
                ..hints
            }),
            (hints, _) => hints,
        };
        let accept_language = HeaderValue::from_str(&accept_language(choice.family(), languages)?)
            .map_err(|_| HeaderError::InvalidIdentity("Accept-Language"))?;
        Ok(Self {
            family: choice.family(),
            user_agent,
            client_hints,
            accept_document: base.accept_document,
            accept_encoding: base.accept_encoding,
            accept_language,
        })
    }

    /// The browser family.
    pub const fn family(&self) -> Family {
        self.family
    }

    /// The `User-Agent` value.
    pub const fn user_agent(&self) -> &HeaderValue {
        &self.user_agent
    }

    /// The `Accept-Language` value.
    pub const fn accept_language(&self) -> &HeaderValue {
        &self.accept_language
    }
}

/// The `Accept-Language` value a browser of `family` derives from a list of
/// languages, most preferred first.
///
/// Chrome lowers the weight by 0.1 per language and never below 0.1:
/// `de-DE,de;q=0.9,en-US;q=0.8,en;q=0.7`. Firefox spreads the weights
/// evenly over the list and rounds to one decimal:
/// `de,en-US;q=0.7,en;q=0.3`.
///
/// # Errors
///
/// Fails when the list is empty or holds something that is not a language
/// tag.
pub fn accept_language<L: AsRef<str>>(
    family: Family,
    languages: &[L],
) -> Result<String, HeaderError> {
    if languages.is_empty() {
        return Err(HeaderError::NoLanguages);
    }
    let count = languages.len();
    let mut value = String::new();
    for (index, language) in languages.iter().enumerate() {
        let language = language.as_ref();
        let well_formed = !language.is_empty()
            && language.len() <= 35
            && !language.starts_with('-')
            && !language.ends_with('-')
            && language
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        if !well_formed {
            return Err(HeaderError::InvalidLanguage(language.to_owned()));
        }
        if index > 0 {
            value.push(',');
        }
        value.push_str(language);
        if index == 0 {
            continue;
        }
        match family {
            Family::Chrome => {
                let tenths = 10_usize.saturating_sub(index).max(1);
                let _ = write!(value, ";q=0.{tenths}");
            }
            Family::Firefox => {
                // q = 1 - index / count, to one decimal for up to ten
                // languages and to two beyond, rounded half up.
                let scale = if count <= 10 { 10 } else { 100 };
                let scaled = (((count - index) * scale * 2 + count) / (count * 2)).max(1);
                if scale == 10 {
                    let _ = write!(value, ";q=0.{scaled}");
                } else {
                    let _ = write!(value, ";q=0.{scaled:02}");
                }
            }
        }
    }
    Ok(value)
}

/// Why a caller may not set a header; `None` when it may.
///
/// The names are spec §9's (`User-Agent`, `Sec-CH-*`, `Sec-Fetch-*`,
/// `Accept-Encoding`) and the ones a browser never lets a page set, or that
/// belong to the transport or to the persona.
pub fn denied_reason(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    if name.starts_with("sec-") {
        return Some("Sec- headers describe the browser and the request and are set by Kage");
    }
    if name.starts_with("proxy-") {
        return Some("Proxy- headers belong to the connection to the proxy");
    }
    if name.starts_with("access-control-request-") {
        return Some("it belongs to a preflight request, which only a browser sends");
    }
    match name.as_str() {
        "user-agent" => Some("it is the persona's identity"),
        "accept-language" => {
            Some("it is the persona's identity; choose a persona from another country instead")
        }
        "accept-encoding" => Some("it is part of the browser fingerprint"),
        "cookie" | "cookie2" => Some("cookies come only from the persona's cookie jar"),
        "origin" => Some("it is derived from the Referer"),
        "upgrade-insecure-requests" | "priority" => Some("it is set by the kind of request"),
        "host" | "connection" | "keep-alive" | "content-length" | "transfer-encoding" | "te"
        | "trailer" | "upgrade" | "expect" | "via" => Some("it belongs to the transport"),
        "accept-charset" | "date" | "set-cookie" => Some("a browser never sends it for a page"),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallerHeader {
    name: HeaderName,
    /// The name as the caller spelled it, for HTTP/1.1.
    spelled: String,
    value: HeaderValue,
}

/// The headers a caller passed, checked against the denylist.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallerHeaders {
    entries: Vec<CallerHeader>,
}

impl CallerHeaders {
    /// No caller headers.
    pub const fn none() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Checks the headers a caller passed.
    ///
    /// # Errors
    ///
    /// Fails, naming the header, when one is on the denylist, is given
    /// twice, or has a name or value that cannot be sent.
    pub fn parse<N: AsRef<str>, V: AsRef<str>>(pairs: &[(N, V)]) -> Result<Self, HeaderError> {
        if pairs.len() > MAX_CALLER_HEADERS {
            return Err(HeaderError::TooMany {
                count: pairs.len(),
                max: MAX_CALLER_HEADERS,
            });
        }
        let mut entries: Vec<CallerHeader> = Vec::with_capacity(pairs.len());
        for (name, value) in pairs {
            let spelled = name.as_ref().trim();
            let name = HeaderName::from_bytes(spelled.as_bytes())
                .map_err(|_| HeaderError::InvalidName(spelled.to_owned()))?;
            if let Some(reason) = denied_reason(name.as_str()) {
                return Err(HeaderError::Denied {
                    name: spelled.to_owned(),
                    reason,
                });
            }
            if entries.iter().any(|entry| entry.name == name) {
                return Err(HeaderError::Duplicate(spelled.to_owned()));
            }
            let value = HeaderValue::from_str(value.as_ref().trim()).map_err(|_| {
                HeaderError::InvalidValue {
                    name: spelled.to_owned(),
                }
            })?;
            entries.push(CallerHeader {
                name,
                spelled: spelled.to_owned(),
                value,
            });
        }
        Ok(Self { entries })
    }

    /// The value the caller gave for a header.
    pub fn get(&self, name: &HeaderName) -> Option<&HeaderValue> {
        self.entries
            .iter()
            .find(|entry| entry.name == *name)
            .map(|entry| &entry.value)
    }

    /// The `Referer` the caller gave: the page the request comes from.
    pub fn referer(&self) -> Option<&str> {
        self.get(&REFERER).and_then(|value| value.to_str().ok())
    }

    /// Whether the caller passed no headers.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The same headers without one of them: a redirect that turns a POST
    /// into a GET drops what described the body.
    #[must_use]
    pub fn without(&self, name: &HeaderName) -> Self {
        Self {
            entries: self
                .entries
                .iter()
                .filter(|entry| entry.name != *name)
                .cloned()
                .collect(),
        }
    }
}

/// One place in a header table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// A header: its lowercase name and its spelling on HTTP/1.1.
    H(&'static str, &'static str),
    /// HTTP/1.1 only: `Connection: keep-alive`, or `Proxy-Connection` when
    /// the request goes to the proxy in absolute form.
    Connection,
    /// Where the caller's headers go that the table has no place for.
    Custom,
}

use Slot::{Connection, Custom, H};

/// Chrome, a document navigation. HTTP/2 and HTTP/1.1.
const CHROME_DOCUMENT: &[Slot] = &[
    H("host", "Host"),
    Connection,
    H("proxy-authorization", "Proxy-Authorization"),
    H("content-length", "Content-Length"),
    H("pragma", "Pragma"),
    H("cache-control", "Cache-Control"),
    H("authorization", "Authorization"),
    H("sec-ch-ua", "sec-ch-ua"),
    H("sec-ch-ua-mobile", "sec-ch-ua-mobile"),
    H("sec-ch-ua-platform", "sec-ch-ua-platform"),
    H("upgrade-insecure-requests", "Upgrade-Insecure-Requests"),
    H("origin", "Origin"),
    H("content-type", "Content-Type"),
    H("user-agent", "User-Agent"),
    H("accept", "Accept"),
    H("sec-fetch-site", "Sec-Fetch-Site"),
    H("sec-fetch-mode", "Sec-Fetch-Mode"),
    H("sec-fetch-user", "Sec-Fetch-User"),
    H("sec-fetch-dest", "Sec-Fetch-Dest"),
    H("referer", "Referer"),
    H("accept-encoding", "Accept-Encoding"),
    H("accept-language", "Accept-Language"),
    Custom,
    H("cookie", "Cookie"),
    H("if-none-match", "If-None-Match"),
    H("if-modified-since", "If-Modified-Since"),
    H("range", "Range"),
    H("priority", "Priority"),
];

/// Chrome, a subresource of a document.
const CHROME_SUBRESOURCE: &[Slot] = &[
    H("host", "Host"),
    Connection,
    H("proxy-authorization", "Proxy-Authorization"),
    H("sec-ch-ua-platform", "sec-ch-ua-platform"),
    H("user-agent", "User-Agent"),
    H("sec-ch-ua", "sec-ch-ua"),
    H("sec-ch-ua-mobile", "sec-ch-ua-mobile"),
    H("accept", "Accept"),
    H("sec-fetch-site", "Sec-Fetch-Site"),
    H("sec-fetch-mode", "Sec-Fetch-Mode"),
    H("sec-fetch-dest", "Sec-Fetch-Dest"),
    H("referer", "Referer"),
    H("accept-encoding", "Accept-Encoding"),
    H("accept-language", "Accept-Language"),
    Custom,
    H("cookie", "Cookie"),
    H("if-none-match", "If-None-Match"),
    H("if-modified-since", "If-Modified-Since"),
    H("priority", "Priority"),
];

/// Firefox, a document navigation.
const FIREFOX_DOCUMENT: &[Slot] = &[
    H("host", "Host"),
    H("user-agent", "User-Agent"),
    H("accept", "Accept"),
    H("accept-language", "Accept-Language"),
    H("accept-encoding", "Accept-Encoding"),
    H("content-type", "Content-Type"),
    H("content-length", "Content-Length"),
    H("origin", "Origin"),
    H("dnt", "DNT"),
    Connection,
    H("proxy-authorization", "Proxy-Authorization"),
    H("referer", "Referer"),
    Custom,
    H("cookie", "Cookie"),
    H("upgrade-insecure-requests", "Upgrade-Insecure-Requests"),
    H("sec-fetch-dest", "Sec-Fetch-Dest"),
    H("sec-fetch-mode", "Sec-Fetch-Mode"),
    H("sec-fetch-site", "Sec-Fetch-Site"),
    H("sec-fetch-user", "Sec-Fetch-User"),
    H("authorization", "Authorization"),
    H("if-modified-since", "If-Modified-Since"),
    H("if-none-match", "If-None-Match"),
    H("range", "Range"),
    H("priority", "Priority"),
    H("pragma", "Pragma"),
    H("cache-control", "Cache-Control"),
    H("te", "TE"),
];

/// Firefox, a subresource of a document.
const FIREFOX_SUBRESOURCE: &[Slot] = &[
    H("host", "Host"),
    H("user-agent", "User-Agent"),
    H("accept", "Accept"),
    H("accept-language", "Accept-Language"),
    H("accept-encoding", "Accept-Encoding"),
    Connection,
    H("proxy-authorization", "Proxy-Authorization"),
    H("referer", "Referer"),
    Custom,
    H("cookie", "Cookie"),
    H("sec-fetch-dest", "Sec-Fetch-Dest"),
    H("sec-fetch-mode", "Sec-Fetch-Mode"),
    H("sec-fetch-site", "Sec-Fetch-Site"),
    H("if-modified-since", "If-Modified-Since"),
    H("if-none-match", "If-None-Match"),
    H("priority", "Priority"),
    H("te", "TE"),
];

const fn table(family: Family, kind: RequestKind) -> &'static [Slot] {
    match (family, kind.is_document()) {
        (Family::Chrome, true) => CHROME_DOCUMENT,
        (Family::Chrome, false) => CHROME_SUBRESOURCE,
        (Family::Firefox, true) => FIREFOX_DOCUMENT,
        (Family::Firefox, false) => FIREFOX_SUBRESOURCE,
    }
}

/// The headers a family sends on HTTP/2 only.
const fn http2_only(family: Family) -> &'static [&'static str] {
    match family {
        // Chrome puts its priority signal into the header only where the
        // protocol has stream priorities.
        Family::Chrome => &["priority"],
        Family::Firefox => &["te"],
    }
}

/// What a browser offers a non-trustworthy (`http://`) origin.
const PLAIN_ACCEPT_ENCODING: HeaderValue = HeaderValue::from_static("gzip, deflate");

fn accept_for(identity: &Identity, kind: RequestKind) -> HeaderValue {
    match (identity.family, kind) {
        (_, RequestKind::Document) => identity.accept_document.clone(),
        (_, RequestKind::Style) => HeaderValue::from_static("text/css,*/*;q=0.1"),
        (_, RequestKind::Script) => HeaderValue::from_static("*/*"),
        (Family::Chrome, RequestKind::Image) => HeaderValue::from_static(
            "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
        ),
        (Family::Firefox, RequestKind::Image) => HeaderValue::from_static(
            "image/avif,image/webp,image/png,image/svg+xml,image/*;q=0.8,*/*;q=0.5",
        ),
    }
}

fn priority_for(family: Family, kind: RequestKind) -> HeaderValue {
    HeaderValue::from_static(match (family, kind) {
        (_, RequestKind::Document) => "u=0, i",
        (Family::Chrome, RequestKind::Style) => "u=0",
        (Family::Chrome, RequestKind::Script) => "u=1",
        (Family::Chrome, RequestKind::Image) => "u=1, i",
        (Family::Firefox, RequestKind::Style | RequestKind::Script) => "u=2",
        (Family::Firefox, RequestKind::Image) => "u=6",
    })
}

/// Everything that decides the headers of one request.
#[derive(Debug, Clone, Copy)]
pub struct HeaderPlan<'a> {
    /// Who the client is.
    pub identity: &'a Identity,
    /// What the request is for.
    pub kind: RequestKind,
    /// The method.
    pub method: &'a Method,
    /// The target.
    pub url: &'a Url,
    /// How the target relates to the page that caused the request.
    pub site: FetchSite,
    /// The `Referer` value, already cut down to what the page would send.
    pub referer: Option<&'a str>,
    /// The origin of the page that caused the request, for a request with a
    /// body.
    pub origin: Option<&'a str>,
    /// Whether the request carries a body.
    pub has_body: bool,
    /// The caller's headers.
    pub caller: &'a CallerHeaders,
    /// Headers Kage itself adds: the validators of a cached subresource.
    pub conditional: &'a [(HeaderName, HeaderValue)],
}

/// The headers of one request and the order they go on the wire in.
///
/// # Errors
///
/// Fails when a referer or origin is not a valid header value.
pub fn build(plan: &HeaderPlan<'_>) -> Result<(HeaderMap, HeaderOrder), HeaderError> {
    let identity = plan.identity;
    let family = identity.family;
    let trustworthy = site::is_trustworthy(plan.url);
    let slots = table(family, plan.kind);
    let mut headers = HeaderMap::with_capacity(24);

    headers.insert(USER_AGENT, identity.user_agent.clone());
    if trustworthy && let Some(hints) = &identity.client_hints {
        headers.insert("sec-ch-ua", hints.brands.clone());
        headers.insert("sec-ch-ua-mobile", hints.mobile.clone());
        headers.insert("sec-ch-ua-platform", hints.platform.clone());
    }
    headers.insert(ACCEPT, accept_for(identity, plan.kind));
    headers.insert(
        ACCEPT_ENCODING,
        if trustworthy {
            identity.accept_encoding.clone()
        } else {
            PLAIN_ACCEPT_ENCODING
        },
    );
    headers.insert(ACCEPT_LANGUAGE, identity.accept_language.clone());
    if plan.kind.is_document() {
        headers.insert("upgrade-insecure-requests", HeaderValue::from_static("1"));
    }
    if trustworthy {
        headers.insert(
            "sec-fetch-site",
            HeaderValue::from_static(plan.site.as_str()),
        );
        headers.insert("sec-fetch-mode", HeaderValue::from_static(plan.kind.mode()));
        headers.insert("sec-fetch-dest", HeaderValue::from_static(plan.kind.dest()));
        if plan.kind.is_document() {
            // A navigation is always the user's doing here: a typed
            // address or a followed link.
            headers.insert("sec-fetch-user", HeaderValue::from_static("?1"));
        }
    }
    headers.insert("priority", priority_for(family, plan.kind));
    if family == Family::Firefox {
        headers.insert("te", HeaderValue::from_static("trailers"));
    }
    if let Some(referer) = plan.referer {
        headers.insert(
            REFERER,
            HeaderValue::from_str(referer).map_err(|_| HeaderError::InvalidValue {
                name: "Referer".to_owned(),
            })?,
        );
    }
    if plan.has_body && plan.kind.is_document() {
        if let Some(origin) = plan.origin {
            headers.insert(
                ORIGIN,
                HeaderValue::from_str(origin).map_err(|_| HeaderError::InvalidValue {
                    name: "Origin".to_owned(),
                })?,
            );
        }
        if family == Family::Chrome && plan.method == Method::POST {
            // Chrome marks a form submission as not to be served from cache.
            headers.insert(CACHE_CONTROL, HeaderValue::from_static("max-age=0"));
        }
    }
    for (name, value) in plan.conditional {
        headers.insert(name.clone(), value.clone());
    }

    let mut custom = Vec::new();
    for entry in &plan.caller.entries {
        // The Referer is the page the request comes from; `plan.referer`
        // already holds what that page would send.
        if entry.name == REFERER {
            continue;
        }
        if !is_slotted(slots, entry.name.as_str()) {
            custom.push((entry.name.clone(), entry.spelled.clone()));
        }
        headers.insert(entry.name.clone(), entry.value.clone());
    }

    let connection = if plan.url.scheme() == "http" {
        // A plain request is sent to the proxy in absolute form. Chrome
        // then names the connection header after the proxy.
        match family {
            Family::Chrome => "Proxy-Connection",
            Family::Firefox => "Connection",
        }
    } else {
        "Connection"
    };
    Ok((
        headers,
        HeaderOrder {
            family,
            slots,
            custom,
            connection,
        },
    ))
}

fn is_slotted(slots: &[Slot], name: &str) -> bool {
    slots
        .iter()
        .any(|slot| matches!(slot, H(lower, _) if *lower == name))
}

/// `x-requested-with` as `X-Requested-With`.
fn title_case(name: &str) -> String {
    let mut spelled = String::with_capacity(name.len());
    let mut start = true;
    for character in name.chars() {
        if start {
            spelled.push(character.to_ascii_uppercase());
        } else {
            spelled.push(character);
        }
        start = character == '-';
    }
    spelled
}

/// The order and spelling of one request's headers on the wire. wreq calls
/// it when it serializes the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderOrder {
    family: Family,
    slots: &'static [Slot],
    /// The caller's headers without a place in the table, in the caller's
    /// order, each with the caller's spelling.
    custom: Vec<(HeaderName, String)>,
    /// The name of the HTTP/1.1 connection header.
    connection: &'static str,
}

impl HeaderOrder {
    /// Headers present that are neither in the table nor announced by the
    /// caller. Nothing adds such a header today; if something does, it is
    /// sent at the caller's place rather than lost.
    fn strays(&self, headers: &HeaderMap) -> Vec<HeaderName> {
        headers
            .keys()
            .filter(|name| {
                !is_slotted(self.slots, name.as_str())
                    && !self.custom.iter().any(|(custom, _)| custom == *name)
            })
            .cloned()
            .collect()
    }

    /// The header names in the order they go out on HTTP/2, for a map that
    /// holds what wreq will have added by then. For tests and evidence.
    pub fn http2_names(&self, headers: &HeaderMap) -> Vec<String> {
        let mut sorted = headers.clone();
        self.call(&mut sorted);
        let mut names: Vec<String> = Vec::new();
        for name in sorted.keys() {
            names.push(name.as_str().to_owned());
        }
        names
    }

    /// The header lines as they go out on HTTP/1.1, for a map that holds
    /// what wreq will have added by then. For tests and evidence.
    pub fn http1_lines(&self, headers: &HeaderMap) -> Vec<(String, String)> {
        let mut remaining = headers.clone();
        let mut lines = Vec::new();
        self.call_visit(&mut remaining, &mut |name, value| {
            lines.push((
                String::from_utf8_lossy(name.as_ref()).into_owned(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            ));
        });
        lines
    }
}

impl OnPreserveHeaderCallback for HeaderOrder {
    /// HTTP/2: sorts the fields into the table's order.
    fn call(&self, headers: &mut HeaderMap) {
        let strays = self.strays(headers);
        let mut sorted = HeaderMap::with_capacity(headers.len());
        let mut take = |headers: &mut HeaderMap, name: &HeaderName| {
            for value in headers.get_all(name) {
                sorted.append(name.clone(), value.clone());
            }
            headers.remove(name);
        };
        for slot in self.slots {
            match slot {
                H(lower, _) => {
                    // `host` is never a field on HTTP/2; the authority is a
                    // pseudo-header.
                    if let Ok(name) = HeaderName::from_bytes(lower.as_bytes()) {
                        take(headers, &name);
                    }
                }
                Connection => {}
                Custom => {
                    for (name, _) in &self.custom {
                        take(headers, name);
                    }
                    for name in &strays {
                        take(headers, name);
                    }
                }
            }
        }
        *headers = sorted;
    }

    /// HTTP/1.1: writes the fields in the table's order with the browser's
    /// spelling, adds the connection header, and leaves out what the family
    /// sends on HTTP/2 only.
    fn call_visit(
        &self,
        headers: &mut HeaderMap,
        dst: &mut dyn FnMut(&dyn AsRef<[u8]>, &HeaderValue),
    ) {
        let strays = self.strays(headers);
        let keep_alive = HeaderValue::from_static("keep-alive");
        for slot in self.slots {
            match slot {
                H(lower, spelled) => {
                    if !http2_only(self.family).contains(lower) {
                        for value in headers.get_all(*lower) {
                            dst(spelled, value);
                        }
                    }
                    headers.remove(*lower);
                }
                Connection => dst(&self.connection, &keep_alive),
                Custom => {
                    for (name, spelled) in &self.custom {
                        for value in headers.get_all(name) {
                            dst(spelled, value);
                        }
                        headers.remove(name);
                    }
                    for name in &strays {
                        let spelled = title_case(name.as_str());
                        for value in headers.get_all(name) {
                            dst(&spelled, value);
                        }
                        headers.remove(name);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use http::Method;
    use http::header::{COOKIE, HOST, HeaderMap, HeaderName, HeaderValue, REFERER};
    use kage_core::domain::ClaimedOs;
    use url::Url;

    use super::{
        CallerHeaders, DEFAULT_LANGUAGES, FetchSite, HeaderError, HeaderPlan, Identity,
        IdentityOverride, MAX_CALLER_HEADERS, RequestKind, accept_language, build, denied_reason,
        title_case,
    };
    use crate::profile::{Family, ProfileChoice};

    const FIREFOX_LINUX_UA: &str =
        "Mozilla/5.0 (X11; Linux x86_64; rv:151.0) Gecko/20100101 Firefox/151.0";

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn identity(profile: &str, os: ClaimedOs, languages: &[&str]) -> Identity {
        Identity::new(
            &ProfileChoice::named(profile, os).unwrap(),
            languages,
            &IdentityOverride::default(),
        )
        .unwrap()
    }

    fn chrome() -> Identity {
        identity("chrome_149", ClaimedOs::Windows, &DEFAULT_LANGUAGES)
    }

    fn firefox() -> Identity {
        identity("firefox_151", ClaimedOs::Windows, &DEFAULT_LANGUAGES)
    }

    struct Case<'a> {
        identity: &'a Identity,
        kind: RequestKind,
        method: Method,
        url: &'a str,
        site: FetchSite,
        referer: Option<&'a str>,
        origin: Option<&'a str>,
        has_body: bool,
        caller: CallerHeaders,
    }

    impl<'a> Case<'a> {
        fn navigation(identity: &'a Identity, url: &'a str) -> Self {
            Self {
                identity,
                kind: RequestKind::Document,
                method: Method::GET,
                url,
                site: FetchSite::None,
                referer: None,
                origin: None,
                has_body: false,
                caller: CallerHeaders::none(),
            }
        }

        fn built(&self) -> (HeaderMap, super::HeaderOrder) {
            let target = url(self.url);
            build(&HeaderPlan {
                identity: self.identity,
                kind: self.kind,
                method: &self.method,
                url: &target,
                site: self.site,
                referer: self.referer,
                origin: self.origin,
                has_body: self.has_body,
                caller: &self.caller,
                conditional: &[],
            })
            .unwrap()
        }

        /// The HTTP/2 field names once wreq has added the cookies.
        fn http2(&self, with_cookie: bool) -> Vec<String> {
            let (mut headers, order) = self.built();
            if with_cookie {
                headers.append(COOKIE, HeaderValue::from_static("a=1"));
                headers.append(COOKIE, HeaderValue::from_static("b=2"));
            }
            if self.has_body {
                headers.insert("content-length", HeaderValue::from_static("7"));
            }
            order.http2_names(&headers)
        }

        /// The HTTP/1.1 header lines once wreq has added host and cookies.
        fn http1(&self, with_cookie: bool) -> Vec<(String, String)> {
            let (mut headers, order) = self.built();
            headers.insert(HOST, HeaderValue::from_static("example.org"));
            if with_cookie {
                headers.insert(COOKIE, HeaderValue::from_static("a=1; b=2"));
            }
            if self.has_body {
                headers.insert("content-length", HeaderValue::from_static("7"));
            }
            order.http1_lines(&headers)
        }
    }

    fn names(lines: &[(String, String)]) -> Vec<&str> {
        lines.iter().map(|(name, _)| name.as_str()).collect()
    }

    fn value<'a>(lines: &'a [(String, String)], name: &str) -> Option<&'a str> {
        lines
            .iter()
            .find(|(found, _)| found == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn a_typed_chrome_navigation_over_http2_is_the_profiles_own_set() {
        let chrome = chrome();
        let case = Case::navigation(&chrome, "https://example.org/");

        assert_eq!(
            case.http2(false),
            [
                "sec-ch-ua",
                "sec-ch-ua-mobile",
                "sec-ch-ua-platform",
                "upgrade-insecure-requests",
                "user-agent",
                "accept",
                "sec-fetch-site",
                "sec-fetch-mode",
                "sec-fetch-user",
                "sec-fetch-dest",
                "accept-encoding",
                "accept-language",
                "priority",
            ]
        );
        let (headers, _) = case.built();
        assert_eq!(headers["sec-fetch-site"], "none");
        assert_eq!(headers["sec-fetch-mode"], "navigate");
        assert_eq!(headers["sec-fetch-user"], "?1");
        assert_eq!(headers["sec-fetch-dest"], "document");
        assert_eq!(headers["accept-encoding"], "gzip, deflate, br, zstd");
        assert_eq!(headers["accept-language"], "en-US,en;q=0.9");
        assert_eq!(headers["priority"], "u=0, i");
        assert_eq!(headers["sec-ch-ua-platform"], "\"Windows\"");
    }

    #[test]
    fn chrome_puts_the_referer_after_the_fetch_metadata_and_cookies_before_priority() {
        let chrome = chrome();
        let mut case = Case::navigation(&chrome, "https://example.org/item/9");
        case.site = FetchSite::SameOrigin;
        case.referer = Some("https://example.org/list");

        assert_eq!(
            case.http2(true),
            [
                "sec-ch-ua",
                "sec-ch-ua-mobile",
                "sec-ch-ua-platform",
                "upgrade-insecure-requests",
                "user-agent",
                "accept",
                "sec-fetch-site",
                "sec-fetch-mode",
                "sec-fetch-user",
                "sec-fetch-dest",
                "referer",
                "accept-encoding",
                "accept-language",
                "cookie",
                "priority",
            ]
        );
        let (headers, _) = case.built();
        assert_eq!(headers["sec-fetch-site"], "same-origin");
        assert_eq!(headers[REFERER], "https://example.org/list");
        // A followed link is still a navigation the user caused.
        assert_eq!(headers["sec-fetch-mode"], "navigate");
        assert_eq!(headers["sec-fetch-user"], "?1");
    }

    #[test]
    fn a_typed_firefox_navigation_over_http2_is_the_profiles_own_set() {
        let firefox = firefox();
        let case = Case::navigation(&firefox, "https://example.org/");

        assert_eq!(
            case.http2(false),
            [
                "user-agent",
                "accept",
                "accept-language",
                "accept-encoding",
                "upgrade-insecure-requests",
                "sec-fetch-dest",
                "sec-fetch-mode",
                "sec-fetch-site",
                "sec-fetch-user",
                "priority",
                "te",
            ]
        );
        let (headers, _) = case.built();
        assert_eq!(headers["accept-language"], "en-US,en;q=0.5");
        assert_eq!(headers["te"], "trailers");
        assert!(!headers.contains_key("sec-ch-ua"));
    }

    #[test]
    fn firefox_puts_referer_and_cookies_before_upgrade_insecure_requests() {
        let firefox = firefox();
        let mut case = Case::navigation(&firefox, "https://shop.example.org/");
        case.site = FetchSite::SameSite;
        case.referer = Some("https://www.example.org/");

        assert_eq!(
            case.http2(true),
            [
                "user-agent",
                "accept",
                "accept-language",
                "accept-encoding",
                "referer",
                "cookie",
                "upgrade-insecure-requests",
                "sec-fetch-dest",
                "sec-fetch-mode",
                "sec-fetch-site",
                "sec-fetch-user",
                "priority",
                "te",
            ]
        );
        assert_eq!(case.built().0["sec-fetch-site"], "same-site");
    }

    #[test]
    fn every_cookie_crumb_keeps_its_place_on_http2() {
        let chrome = chrome();
        let case = Case::navigation(&chrome, "https://example.org/");
        let (mut headers, order) = case.built();
        headers.append(COOKIE, HeaderValue::from_static("a=1"));
        headers.append(COOKIE, HeaderValue::from_static("b=2"));
        headers.append(COOKIE, HeaderValue::from_static("c=3"));

        let mut sorted = headers.clone();
        wreq_proto::ext::OnPreserveHeaderCallback::call(&order, &mut sorted);

        let crumbs: Vec<&str> = sorted
            .get_all(COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect();
        assert_eq!(crumbs, ["a=1", "b=2", "c=3"]);
        assert_eq!(sorted.len(), headers.len());
        let flat: Vec<&str> = sorted.iter().map(|(name, _)| name.as_str()).collect();
        let first_cookie = flat.iter().position(|name| *name == "cookie").unwrap();
        assert_eq!(
            &flat[first_cookie..],
            ["cookie", "cookie", "cookie", "priority"]
        );
    }

    #[test]
    fn chrome_over_http1_inside_a_tunnel_uses_its_own_spelling_and_no_priority() {
        let chrome = chrome();
        let mut case = Case::navigation(&chrome, "https://example.org/item/9");
        case.site = FetchSite::CrossSite;
        case.referer = Some("https://www.example.com/");

        let lines = case.http1(true);

        assert_eq!(
            names(&lines),
            [
                "Host",
                "Connection",
                "sec-ch-ua",
                "sec-ch-ua-mobile",
                "sec-ch-ua-platform",
                "Upgrade-Insecure-Requests",
                "User-Agent",
                "Accept",
                "Sec-Fetch-Site",
                "Sec-Fetch-Mode",
                "Sec-Fetch-User",
                "Sec-Fetch-Dest",
                "Referer",
                "Accept-Encoding",
                "Accept-Language",
                "Cookie",
            ]
        );
        assert_eq!(value(&lines, "Connection"), Some("keep-alive"));
        assert_eq!(value(&lines, "Sec-Fetch-Site"), Some("cross-site"));
        assert_eq!(value(&lines, "Cookie"), Some("a=1; b=2"));
    }

    #[test]
    fn a_plain_http_origin_gets_no_client_hints_no_fetch_metadata_and_no_brotli() {
        let chrome = chrome();
        let mut case = Case::navigation(&chrome, "http://site.kage.test/list");
        case.site = FetchSite::SameOrigin;
        case.referer = Some("http://site.kage.test/");

        let lines = case.http1(true);

        assert_eq!(
            names(&lines),
            [
                "Host",
                "Proxy-Connection",
                "Upgrade-Insecure-Requests",
                "User-Agent",
                "Accept",
                "Referer",
                "Accept-Encoding",
                "Accept-Language",
                "Cookie",
            ]
        );
        assert_eq!(value(&lines, "Accept-Encoding"), Some("gzip, deflate"));
        assert_eq!(value(&lines, "Proxy-Connection"), Some("keep-alive"));

        let firefox = firefox();
        let lines = Case::navigation(&firefox, "http://site.kage.test/").http1(false);
        assert_eq!(
            names(&lines),
            [
                "Host",
                "User-Agent",
                "Accept",
                "Accept-Language",
                "Accept-Encoding",
                "Connection",
                "Upgrade-Insecure-Requests",
                "Priority",
            ]
        );
        assert_eq!(value(&lines, "Accept-Encoding"), Some("gzip, deflate"));
    }

    #[test]
    fn this_machine_is_trustworthy_even_over_plain_http() {
        let chrome = chrome();
        let lines = Case::navigation(&chrome, "http://127.0.0.1:8080/ip").http1(false);

        assert!(names(&lines).contains(&"sec-ch-ua"));
        assert_eq!(value(&lines, "Sec-Fetch-Site"), Some("none"));
        assert_eq!(
            value(&lines, "Accept-Encoding"),
            Some("gzip, deflate, br, zstd")
        );
    }

    #[test]
    fn firefox_over_http1_keeps_priority_and_drops_te() {
        let firefox = firefox();
        let mut case = Case::navigation(&firefox, "https://example.org/a");
        case.site = FetchSite::SameOrigin;
        case.referer = Some("https://example.org/");

        let lines = case.http1(true);

        assert_eq!(
            names(&lines),
            [
                "Host",
                "User-Agent",
                "Accept",
                "Accept-Language",
                "Accept-Encoding",
                "Connection",
                "Referer",
                "Cookie",
                "Upgrade-Insecure-Requests",
                "Sec-Fetch-Dest",
                "Sec-Fetch-Mode",
                "Sec-Fetch-Site",
                "Sec-Fetch-User",
                "Priority",
            ]
        );
    }

    #[test]
    fn a_form_submission_carries_length_origin_and_type_in_the_browsers_places() {
        let caller =
            CallerHeaders::parse(&[("Content-Type", "application/x-www-form-urlencoded")]).unwrap();
        let chrome = chrome();
        let mut case = Case::navigation(&chrome, "https://example.org/login");
        case.method = Method::POST;
        case.has_body = true;
        case.site = FetchSite::SameOrigin;
        case.referer = Some("https://example.org/login");
        case.origin = Some("https://example.org");
        case.caller = caller.clone();

        assert_eq!(
            case.http2(false),
            [
                "content-length",
                "cache-control",
                "sec-ch-ua",
                "sec-ch-ua-mobile",
                "sec-ch-ua-platform",
                "upgrade-insecure-requests",
                "origin",
                "content-type",
                "user-agent",
                "accept",
                "sec-fetch-site",
                "sec-fetch-mode",
                "sec-fetch-user",
                "sec-fetch-dest",
                "referer",
                "accept-encoding",
                "accept-language",
                "priority",
            ]
        );
        assert_eq!(case.built().0["cache-control"], "max-age=0");

        let firefox = firefox();
        let mut case = Case::navigation(&firefox, "https://example.org/login");
        case.method = Method::POST;
        case.has_body = true;
        case.site = FetchSite::SameOrigin;
        case.referer = Some("https://example.org/login");
        case.origin = Some("https://example.org");
        case.caller = caller;
        assert_eq!(
            case.http2(true),
            [
                "user-agent",
                "accept",
                "accept-language",
                "accept-encoding",
                "content-type",
                "content-length",
                "origin",
                "referer",
                "cookie",
                "upgrade-insecure-requests",
                "sec-fetch-dest",
                "sec-fetch-mode",
                "sec-fetch-site",
                "sec-fetch-user",
                "priority",
                "te",
            ]
        );
        assert!(!case.built().0.contains_key("cache-control"));
    }

    #[test]
    fn subresources_have_their_own_accept_metadata_and_order() {
        let chrome = chrome();
        for (kind, accept, dest, priority) in [
            (RequestKind::Style, "text/css,*/*;q=0.1", "style", "u=0"),
            (RequestKind::Script, "*/*", "script", "u=1"),
            (
                RequestKind::Image,
                "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
                "image",
                "u=1, i",
            ),
        ] {
            let mut case = Case::navigation(&chrome, "https://example.org/static/a");
            case.kind = kind;
            case.site = FetchSite::SameOrigin;
            case.referer = Some("https://example.org/");
            let (headers, _) = case.built();

            assert_eq!(headers["accept"], accept);
            assert_eq!(headers["sec-fetch-dest"], dest);
            assert_eq!(headers["sec-fetch-mode"], "no-cors");
            assert_eq!(headers["priority"], priority);
            assert!(!headers.contains_key("upgrade-insecure-requests"));
            assert!(!headers.contains_key("sec-fetch-user"));
            assert_eq!(
                case.http2(true),
                [
                    "sec-ch-ua-platform",
                    "user-agent",
                    "sec-ch-ua",
                    "sec-ch-ua-mobile",
                    "accept",
                    "sec-fetch-site",
                    "sec-fetch-mode",
                    "sec-fetch-dest",
                    "referer",
                    "accept-encoding",
                    "accept-language",
                    "cookie",
                    "priority",
                ]
            );
        }

        let firefox = firefox();
        let mut case = Case::navigation(&firefox, "https://example.org/static/a.css");
        case.kind = RequestKind::Style;
        case.site = FetchSite::SameOrigin;
        case.referer = Some("https://example.org/");
        assert_eq!(
            case.http2(true),
            [
                "user-agent",
                "accept",
                "accept-language",
                "accept-encoding",
                "referer",
                "cookie",
                "sec-fetch-dest",
                "sec-fetch-mode",
                "sec-fetch-site",
                "priority",
                "te",
            ]
        );
        assert_eq!(case.built().0["priority"], "u=2");
    }

    #[test]
    fn caller_headers_go_to_the_familys_place_and_unknown_names_before_the_cookies() {
        let caller = CallerHeaders::parse(&[
            ("X-Requested-With", "XMLHttpRequest"),
            ("Authorization", "Bearer abc"),
            ("accept", "application/json"),
            ("If-None-Match", "\"v1\""),
            ("x-trace", "7"),
            ("Referer", "https://example.org/whole/page?q=1"),
        ])
        .unwrap();
        let chrome = chrome();
        let mut case = Case::navigation(&chrome, "https://example.org/api");
        case.caller = caller.clone();
        case.site = FetchSite::SameOrigin;
        case.referer = Some("https://example.org/whole/page?q=1");

        assert_eq!(
            case.http2(true),
            [
                "authorization",
                "sec-ch-ua",
                "sec-ch-ua-mobile",
                "sec-ch-ua-platform",
                "upgrade-insecure-requests",
                "user-agent",
                "accept",
                "sec-fetch-site",
                "sec-fetch-mode",
                "sec-fetch-user",
                "sec-fetch-dest",
                "referer",
                "accept-encoding",
                "accept-language",
                "x-requested-with",
                "x-trace",
                "cookie",
                "if-none-match",
                "priority",
            ]
        );
        let (headers, _) = case.built();
        assert_eq!(headers["accept"], "application/json");

        // On HTTP/1.1 a custom name keeps the caller's spelling.
        let lines = case.http1(true);
        let position = |name: &str| names(&lines).iter().position(|found| *found == name);
        assert!(position("X-Requested-With").is_some());
        assert!(position("x-trace").is_some());
        assert!(position("x-trace") < position("Cookie"));
        assert!(position("Authorization") < position("sec-ch-ua"));

        let firefox = firefox();
        let mut case = Case::navigation(&firefox, "https://example.org/api");
        case.caller = caller;
        let order = case.http2(true);
        let position = |name: &str| order.iter().position(|found| found == name).unwrap();
        assert!(position("x-requested-with") < position("cookie"));
        assert!(position("authorization") > position("sec-fetch-user"));
        assert!(position("if-none-match") < position("priority"));
    }

    #[test]
    fn a_header_nobody_announced_is_sent_at_the_callers_place() {
        let chrome = chrome();
        let case = Case::navigation(&chrome, "https://example.org/");
        let (mut headers, order) = case.built();
        headers.insert("x-added-later", HeaderValue::from_static("1"));
        headers.insert(COOKIE, HeaderValue::from_static("a=1"));

        let names = order.http2_names(&headers);
        let position = |name: &str| names.iter().position(|found| found == name).unwrap();
        assert!(position("x-added-later") > position("accept-language"));
        assert!(position("x-added-later") < position("cookie"));

        headers.insert(HOST, HeaderValue::from_static("example.org"));
        let lines = order.http1_lines(&headers);
        assert!(lines.iter().any(|(name, _)| name == "X-Added-Later"));
        assert_eq!(title_case("x-requested-with"), "X-Requested-With");
    }

    #[test]
    fn the_denylist_names_what_it_refuses() {
        for name in [
            "User-Agent",
            "user-agent",
            "Sec-CH-UA",
            "sec-ch-ua-platform",
            "Sec-Fetch-Site",
            "sec-fetch-dest",
            "Sec-GPC",
            "Accept-Encoding",
            "Accept-Language",
            "Cookie",
            "Host",
            "Connection",
            "Content-Length",
            "Transfer-Encoding",
            "TE",
            "Upgrade",
            "Keep-Alive",
            "Proxy-Authorization",
            "Proxy-Connection",
            "Priority",
            "Upgrade-Insecure-Requests",
            "Origin",
            "Expect",
            "Via",
            "Access-Control-Request-Method",
        ] {
            assert!(denied_reason(name).is_some(), "{name} should be denied");
            let error = CallerHeaders::parse(&[(name, "x")]).unwrap_err();
            assert!(
                matches!(&error, HeaderError::Denied { name: found, .. } if found == name),
                "{name}: {error:?}"
            );
            assert!(error.to_string().contains(name), "{error}");
        }
        for name in [
            "Accept",
            "Referer",
            "Content-Type",
            "Authorization",
            "Cache-Control",
            "Pragma",
            "If-None-Match",
            "If-Modified-Since",
            "Range",
            "DNT",
            "X-Requested-With",
            "X-Api-Key",
            "Secret-Token",
            "Security",
        ] {
            assert_eq!(denied_reason(name), None, "{name} should be allowed");
        }
    }

    #[test]
    fn malformed_duplicate_and_too_many_headers_are_refused() {
        assert_eq!(
            CallerHeaders::parse(&[("X Bad", "1")]).unwrap_err(),
            HeaderError::InvalidName("X Bad".to_owned())
        );
        assert_eq!(
            CallerHeaders::parse(&[("X-Ok", "line\r\nInjected: 1")]).unwrap_err(),
            HeaderError::InvalidValue {
                name: "X-Ok".to_owned()
            }
        );
        assert_eq!(
            CallerHeaders::parse(&[("X-Ok", "1"), ("x-ok", "2")]).unwrap_err(),
            HeaderError::Duplicate("x-ok".to_owned())
        );
        let many: Vec<(String, String)> = (0..=MAX_CALLER_HEADERS)
            .map(|index| (format!("x-{index}"), "1".to_owned()))
            .collect();
        assert_eq!(
            CallerHeaders::parse(&many).unwrap_err(),
            HeaderError::TooMany {
                count: MAX_CALLER_HEADERS + 1,
                max: MAX_CALLER_HEADERS
            }
        );

        let parsed = CallerHeaders::parse(&[(" Referer ", " https://example.org/a ")]).unwrap();
        assert_eq!(parsed.referer(), Some("https://example.org/a"));
        assert!(!parsed.is_empty());
        assert!(CallerHeaders::none().is_empty());
        assert_eq!(CallerHeaders::none().referer(), None);
        assert_eq!(
            parsed.get(&HeaderName::from_static("referer")),
            Some(&HeaderValue::from_static("https://example.org/a"))
        );
    }

    #[test]
    fn accept_language_is_written_the_way_each_browser_writes_it() {
        let chrome = |languages: &[&str]| accept_language(Family::Chrome, languages).unwrap();
        let firefox = |languages: &[&str]| accept_language(Family::Firefox, languages).unwrap();

        assert_eq!(chrome(&["en-US", "en"]), "en-US,en;q=0.9");
        assert_eq!(
            chrome(&["de-DE", "de", "en-US", "en"]),
            "de-DE,de;q=0.9,en-US;q=0.8,en;q=0.7"
        );
        assert_eq!(chrome(&["fr"]), "fr");
        let twelve: Vec<String> = (0..12).map(|index| format!("x{index}")).collect();
        assert!(
            accept_language(Family::Chrome, &twelve)
                .unwrap()
                .ends_with("x10;q=0.1,x11;q=0.1")
        );

        assert_eq!(firefox(&["en-US", "en"]), "en-US,en;q=0.5");
        assert_eq!(firefox(&["de", "en-US", "en"]), "de,en-US;q=0.7,en;q=0.3");
        assert_eq!(
            firefox(&["fr", "fr-FR", "en-US", "en"]),
            "fr,fr-FR;q=0.8,en-US;q=0.5,en;q=0.3"
        );
        assert_eq!(firefox(&["it"]), "it");
        let value = accept_language(Family::Firefox, &twelve).unwrap();
        assert!(value.starts_with("x0,x1;q=0.92,x2;q=0.83"), "{value}");
        assert!(value.ends_with("x11;q=0.08"), "{value}");

        assert_eq!(
            accept_language::<&str>(Family::Chrome, &[]),
            Err(HeaderError::NoLanguages)
        );
        for bad in ["", "de DE", "de_DE", "-de", "de-", "en;q=1", "en,de"] {
            assert_eq!(
                accept_language(Family::Firefox, &[bad]),
                Err(HeaderError::InvalidLanguage(bad.to_owned())),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_persona_can_supply_its_own_user_agent_and_brand_list() {
        let choice = ProfileChoice::named("chrome_149", ClaimedOs::Macos).unwrap();
        let overrides = IdentityOverride {
            user_agent: Some("Mozilla/5.0 (Macintosh) Persona/1".to_owned()),
            client_hint_brands: Some("\"Persona\";v=\"1\"".to_owned()),
        };
        let identity = Identity::new(&choice, &["de-DE", "de"], &overrides).unwrap();

        assert_eq!(identity.user_agent(), "Mozilla/5.0 (Macintosh) Persona/1");
        assert_eq!(identity.accept_language(), "de-DE,de;q=0.9");
        assert_eq!(identity.family(), Family::Chrome);
        let case = Case::navigation(&identity, "https://example.org/");
        let (headers, _) = case.built();
        assert_eq!(headers["sec-ch-ua"], "\"Persona\";v=\"1\"");
        assert_eq!(headers["sec-ch-ua-platform"], "\"macOS\"");
        assert_eq!(headers["user-agent"], "Mozilla/5.0 (Macintosh) Persona/1");

        for (field, overrides) in [
            (
                "User-Agent",
                IdentityOverride {
                    user_agent: Some("bad\nvalue".to_owned()),
                    client_hint_brands: None,
                },
            ),
            (
                "sec-ch-ua",
                IdentityOverride {
                    user_agent: None,
                    client_hint_brands: Some("bad\nvalue".to_owned()),
                },
            ),
        ] {
            assert_eq!(
                Identity::new(&choice, &DEFAULT_LANGUAGES, &overrides).unwrap_err(),
                HeaderError::InvalidIdentity(field)
            );
        }
        assert_eq!(
            Identity::new(&choice, &["not a tag"], &IdentityOverride::default()).unwrap_err(),
            HeaderError::InvalidLanguage("not a tag".to_owned())
        );
    }

    /// Consult 2: wreq-util's Firefox on Linux claims an Ubuntu build. It is
    /// refused rather than sent; the persona's own User-Agent is required.
    #[test]
    fn firefox_on_linux_is_refused_without_the_personas_user_agent() {
        let choice = ProfileChoice::named("firefox_151", ClaimedOs::Linux).unwrap();
        assert!(
            choice
                .base_headers()
                .unwrap()
                .user_agent
                .to_str()
                .unwrap()
                .contains("Ubuntu"),
            "the reason for the rule is gone: wreq-util no longer claims Ubuntu"
        );

        let error =
            Identity::new(&choice, &DEFAULT_LANGUAGES, &IdentityOverride::default()).unwrap_err();
        assert!(
            matches!(
                &error,
                HeaderError::IdentityRequired {
                    field: "User-Agent",
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(
            error.to_string().contains("firefox_151 on linux"),
            "{error}"
        );

        let identity = Identity::new(
            &choice,
            &DEFAULT_LANGUAGES,
            &IdentityOverride {
                user_agent: Some(FIREFOX_LINUX_UA.to_owned()),
                client_hint_brands: None,
            },
        )
        .unwrap();
        assert_eq!(identity.user_agent(), FIREFOX_LINUX_UA);

        // Every other combination has a usable identity of its own.
        for (profile, os) in [
            ("firefox_151", ClaimedOs::Windows),
            ("firefox_151", ClaimedOs::Macos),
            ("chrome_149", ClaimedOs::Linux),
        ] {
            let choice = ProfileChoice::named(profile, os).unwrap();
            assert!(
                Identity::new(&choice, &DEFAULT_LANGUAGES, &IdentityOverride::default()).is_ok(),
                "{profile} {os}"
            );
        }
    }

    #[test]
    fn fetch_site_follows_the_initiator_over_a_redirect_chain() {
        let page = url("https://www.example.org/list");

        assert_eq!(
            FetchSite::between(&page, &url("https://www.example.org/a")),
            FetchSite::SameOrigin
        );
        assert_eq!(
            FetchSite::between(&page, &url("https://api.example.org/a")),
            FetchSite::SameSite
        );
        assert_eq!(
            FetchSite::between(&page, &url("https://example.com/")),
            FetchSite::CrossSite
        );

        // A chain is as far from its initiator as its furthest hop.
        let start = FetchSite::between(&page, &url("https://www.example.org/out"));
        let left = start.after_redirect(Some(&page), &url("https://tracker.example.net/r"));
        assert_eq!(left, FetchSite::CrossSite);
        let back = left.after_redirect(Some(&page), &url("https://www.example.org/in"));
        assert_eq!(back, FetchSite::CrossSite);
        let near = start.after_redirect(Some(&page), &url("https://api.example.org/r"));
        assert_eq!(near, FetchSite::SameSite);
        assert_eq!(
            near.after_redirect(Some(&page), &url("https://www.example.org/in")),
            FetchSite::SameSite
        );
        // A typed navigation stays `none` wherever it is sent.
        assert_eq!(
            FetchSite::None.after_redirect(None, &url("https://example.com/")),
            FetchSite::None
        );
        assert_eq!(
            FetchSite::None.after_redirect(Some(&page), &url("https://example.com/")),
            FetchSite::None
        );

        assert!(FetchSite::None.is_same_site());
        assert!(FetchSite::SameOrigin.is_same_site());
        assert!(FetchSite::SameSite.is_same_site());
        assert!(!FetchSite::CrossSite.is_same_site());
        assert_eq!(FetchSite::CrossSite.to_string(), "cross-site");
        assert_eq!(RequestKind::Document.mode(), "navigate");
        assert_eq!(RequestKind::Image.dest(), "image");
    }
}
