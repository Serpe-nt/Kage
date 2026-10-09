//! The cookie jar of one client (spec §6.4, §9): storing what responses
//! set and choosing what a request carries, by a browser's rules.
//!
//! A server can tell a browser from an HTTP library by its cookies: a
//! library sends a `SameSite=Strict` cookie along with a request that came
//! from another site, accepts a cookie for `co.uk`, or returns cookies in
//! the order it happens to hold them. This jar follows the rules both
//! browsers follow (RFC 6265bis and Chromium's cookie store):
//!
//! - **Storing.** `Domain` must match the host and may not be a public
//!   suffix; `Secure` and the `__Secure-` and `__Host-` prefixes need a
//!   secure origin; an insecure origin cannot replace a secure cookie;
//!   `SameSite=None` needs `Secure`; `Max-Age` wins over `Expires`; a
//!   lifetime is cut to 400 days; a name and value over 4096 bytes are
//!   refused; at most 180 cookies per site and 3000 in all, the least
//!   recently used go first.
//! - **Sending.** Host-only cookies go to their exact host; `Secure`
//!   cookies only to secure origins; `SameSite=Strict` only with requests
//!   from the cookie's own site, `Lax` (also when the attribute is missing)
//!   additionally with a top-level navigation by a safe method; the order is
//!   longest path first, then oldest first.
//!
//! # Persistence
//!
//! A jar is loaded from and saved to a [`CookieBackend`], keyed by the proxy
//! the client leaves through (one persona per proxy; milestone M6 adds the
//! persona). [`StoreCookies`] keeps jars in Postgres and refuses the jar of
//! a retired proxy: cookies must never follow a persona to a new address.
//!
//! Session cookies are persisted too, because a restart of Kage is not the
//! user closing the browser. They are dropped when the jar is loaded after
//! it sat idle for longer than `http.session_cookie_idle`: the browser was
//! closed overnight.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use http::{HeaderValue, Uri, Version};
use kage_core::domain::{CookieSameSite, HttpCookie};
use kage_core::ids::ProxyId;
use kage_store::{Store, StoreError};
use url::Url;
use wreq::cookie::{CookieStore, Cookies};

use crate::site;

/// Longest name plus value of one cookie, in bytes.
pub const MAX_COOKIE_BYTES: usize = 4096;

/// Longest value of one cookie attribute, in bytes.
const MAX_ATTRIBUTE_BYTES: usize = 1024;

/// Most cookies of one site (registrable domain).
pub const MAX_COOKIES_PER_SITE: usize = 180;

/// Most cookies in one jar.
pub const MAX_COOKIES: usize = 3000;

/// Longest lifetime a cookie can ask for.
const MAX_LIFETIME_DAYS: i64 = 400;

/// A cookie's last-use time is persisted only when it moved this far, so
/// that reading cookies does not rewrite the jar on every request.
const TOUCH_GRANULARITY_MINUTES: i64 = 5;

/// What a request is, as far as cookies care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CookieContext {
    /// Whether the request comes from the target's own site, or directly
    /// from the user. `false` for a request a page of another site caused.
    pub same_site: bool,
    /// Whether the request is a top-level navigation.
    pub top_level_navigation: bool,
    /// Whether the method is safe (`GET`, `HEAD`).
    pub safe_method: bool,
}

impl CookieContext {
    /// A navigation the user caused directly: every cookie applies.
    pub const DIRECT: Self = Self {
        same_site: true,
        top_level_navigation: true,
        safe_method: true,
    };
}

/// What became of a `Set-Cookie` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetCookieVerdict {
    /// The cookie was stored, or replaced an older one.
    Stored,
    /// The header asked to delete a cookie (it was already expired).
    Removed,
    /// The header was ignored.
    Rejected(&'static str),
}

struct Entry {
    cookie: HttpCookie,
    /// Breaks ties between cookies created in the same instant.
    sequence: u64,
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    next_sequence: u64,
    dirty: bool,
}

/// The cookies of one client.
#[derive(Default)]
pub struct CookieJar {
    state: Mutex<State>,
}

impl fmt::Debug for CookieJar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the values: they are session identifiers.
        f.debug_struct("CookieJar")
            .field("cookies", &self.len())
            .finish()
    }
}

/// The fields of one `Set-Cookie` header.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedSetCookie {
    name: String,
    value: String,
    domain: Option<String>,
    path: Option<String>,
    expires: Option<DateTime<Utc>>,
    max_age: Option<i64>,
    secure: bool,
    http_only: bool,
    same_site: Option<CookieSameSite>,
}

fn has_control(text: &str) -> bool {
    text.bytes()
        .any(|byte| (byte < 0x20 && byte != b'\t') || byte == 0x7f)
}

fn parse_set_cookie(header: &str) -> Result<ParsedSetCookie, &'static str> {
    let mut parts = header.split(';');
    let pair = parts.next().unwrap_or_default();
    let (name, value) = match pair.split_once('=') {
        Some((name, value)) => (name.trim(), value.trim()),
        // A header without `=` is a cookie without a name.
        None => ("", pair.trim()),
    };
    if name.is_empty() && value.is_empty() {
        return Err("the cookie has neither a name nor a value");
    }
    if has_control(name) || has_control(value) {
        return Err("the cookie holds a control character");
    }
    if name.len() + value.len() > MAX_COOKIE_BYTES {
        return Err("the cookie is larger than 4096 bytes");
    }
    let mut parsed = ParsedSetCookie {
        name: name.to_owned(),
        value: value.to_owned(),
        domain: None,
        path: None,
        expires: None,
        max_age: None,
        secure: false,
        http_only: false,
        same_site: None,
    };
    for attribute in parts {
        let (key, value) = match attribute.split_once('=') {
            Some((key, value)) => (key.trim(), value.trim()),
            None => (attribute.trim(), ""),
        };
        if value.len() > MAX_ATTRIBUTE_BYTES {
            continue;
        }
        // The last occurrence of an attribute wins.
        match key.to_ascii_lowercase().as_str() {
            "expires" => parsed.expires = parse_http_date(value).or(parsed.expires),
            "max-age" => {
                let digits = value.strip_prefix('-').unwrap_or(value);
                if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
                    // More digits than fit is "for ever" or "long ago".
                    parsed.max_age = Some(value.parse().unwrap_or(if value.starts_with('-') {
                        i64::MIN
                    } else {
                        i64::MAX
                    }));
                }
            }
            "domain" => {
                let domain = value
                    .strip_prefix('.')
                    .unwrap_or(value)
                    .to_ascii_lowercase();
                parsed.domain = (!domain.is_empty()).then_some(domain);
            }
            "path" => {
                parsed.path = value.starts_with('/').then(|| value.to_owned());
            }
            "secure" => parsed.secure = true,
            "httponly" => parsed.http_only = true,
            "samesite" => {
                parsed.same_site = match value.to_ascii_lowercase().as_str() {
                    "strict" => Some(CookieSameSite::Strict),
                    "lax" => Some(CookieSameSite::Lax),
                    "none" => Some(CookieSameSite::None),
                    _ => None,
                };
            }
            _ => {}
        }
    }
    Ok(parsed)
}

/// Reads a date from a header the lenient way browsers read a cookie date
/// (RFC 6265 §5.1.1): the tokens may come in any order, and only a time, a
/// day, a month name and a year need to be found. Every HTTP date format is
/// read by this.
pub(crate) fn parse_http_date(text: &str) -> Option<DateTime<Utc>> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let mut time: Option<(u32, u32, u32)> = None;
    let mut day: Option<u32> = None;
    let mut month: Option<u32> = None;
    let mut year: Option<i32> = None;
    let is_delimiter =
        |character: char| matches!(character, '\t' | ' '..='/' | ';'..='@' | '['..='`' | '{'..='~');
    for token in text.split(is_delimiter).filter(|token| !token.is_empty()) {
        if time.is_none() && token.contains(':') {
            let mut fields = token.split(':').map(|field| {
                let digits: String = field.chars().take_while(char::is_ascii_digit).collect();
                (!digits.is_empty() && digits.len() <= 2)
                    .then(|| digits.parse::<u32>().ok())
                    .flatten()
            });
            if let (Some(Some(hour)), Some(Some(minute)), Some(Some(second))) =
                (fields.next(), fields.next(), fields.next())
            {
                time = Some((hour, minute, second));
                continue;
            }
        }
        let digits: String = token.chars().take_while(char::is_ascii_digit).collect();
        if day.is_none() && (1..=2).contains(&digits.len()) {
            day = digits.parse().ok();
            continue;
        }
        if month.is_none()
            && let Some(prefix) = token.get(..3)
            && let Some(index) = MONTHS
                .iter()
                .position(|name| name.eq_ignore_ascii_case(prefix))
        {
            month = u32::try_from(index + 1).ok();
            continue;
        }
        if year.is_none() && (2..=4).contains(&digits.len()) {
            year = digits.parse().ok();
        }
    }
    let (hour, minute, second) = time?;
    let year = match year? {
        year @ 70..=99 => year + 1900,
        year @ 0..=69 => year + 2000,
        year => year,
    };
    if year < 1601 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    NaiveDate::from_ymd_opt(year, month?, day?)?
        .and_hms_opt(hour, minute, second)
        .map(|naive| naive.and_utc())
}

/// Whether `host` is `domain` or a subdomain of it (RFC 6265 §5.1.3).
fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || (!site::is_ip_literal(host)
            && host.len() > domain.len()
            && host.ends_with(domain)
            && host.as_bytes()[host.len() - domain.len() - 1] == b'.')
}

/// The path a cookie without a `Path` attribute gets (RFC 6265 §5.1.4).
fn default_path(request_path: &str) -> &str {
    if !request_path.starts_with('/') {
        return "/";
    }
    match request_path.rfind('/') {
        Some(0) | None => "/",
        Some(last) => &request_path[..last],
    }
}

/// Whether a cookie for `cookie_path` is sent with a request to
/// `request_path` (RFC 6265 §5.1.4).
fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    request_path == cookie_path
        || (request_path.starts_with(cookie_path)
            && (cookie_path.ends_with('/')
                || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')))
}

/// Whether two cookies are the same cookie: storing one replaces the other.
fn same_cookie(left: &HttpCookie, right: &HttpCookie) -> bool {
    left.name == right.name
        && left.domain == right.domain
        && left.host_only == right.host_only
        && left.path == right.path
}

/// Decides, from the header and the response's URL alone, whether a
/// `Set-Cookie` header is acceptable, and what cookie it describes. The
/// flag says whether the response came from a secure origin.
fn admit(url: &Url, header: &str, now: DateTime<Utc>) -> Result<(HttpCookie, bool), &'static str> {
    let parsed = parse_set_cookie(header)?;
    let host = site::host_of(url).ok_or("the response has no host")?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("the response is not from an http(s) URL");
    }
    let secure_origin = site::is_trustworthy(url);

    // A Domain attribute that is a public suffix is only acceptable when it
    // is the host itself, and then the cookie is host-only.
    let (domain, host_only) = match parsed.domain {
        None => (host, true),
        Some(domain) if site::is_public_suffix(&domain) || site::is_ip_literal(&domain) => {
            if domain != host {
                return Err("the Domain attribute is a public suffix or another address");
            }
            (host, true)
        }
        Some(domain) => {
            if !domain_matches(&host, &domain) {
                return Err("the Domain attribute is another site's");
            }
            (domain, false)
        }
    };
    let path = parsed
        .path
        .unwrap_or_else(|| default_path(url.path()).to_owned());

    if parsed.secure && !secure_origin {
        return Err("a Secure cookie from an insecure origin");
    }
    let name_lower = parsed.name.to_ascii_lowercase();
    if name_lower.starts_with("__secure-") && !(parsed.secure && secure_origin) {
        return Err("a __Secure- cookie must be Secure");
    }
    if name_lower.starts_with("__host-")
        && !(parsed.secure && secure_origin && host_only && path == "/")
    {
        return Err("a __Host- cookie must be Secure, host-only and for the path /");
    }
    if parsed.same_site == Some(CookieSameSite::None) && !parsed.secure {
        return Err("SameSite=None needs Secure");
    }

    let limit = now + Duration::days(MAX_LIFETIME_DAYS);
    let expires_at = match (parsed.max_age, parsed.expires) {
        (Some(seconds), _) if seconds <= 0 => Some(now),
        (Some(seconds), _) => Some(
            Duration::try_seconds(seconds)
                .and_then(|age| now.checked_add_signed(age))
                .map_or(limit, |at| at.min(limit)),
        ),
        (None, Some(expires)) => Some(expires.min(limit)),
        (None, None) => None,
    };
    Ok((
        HttpCookie {
            domain,
            host_only,
            path,
            name: parsed.name,
            value: parsed.value,
            secure: parsed.secure,
            http_only: parsed.http_only,
            same_site: parsed.same_site,
            expires_at,
            created_at: now,
            last_used_at: now,
        },
        secure_origin,
    ))
}

/// The site a cookie counts toward for the per-site limit.
fn site_key(domain: &str) -> String {
    site::registrable_domain(domain).unwrap_or_else(|| domain.to_owned())
}

impl CookieJar {
    /// An empty jar.
    pub fn new() -> Self {
        Self::default()
    }

    /// A jar holding stored cookies. Expired cookies are left out, and so
    /// are session cookies when nothing in the jar was used for longer than
    /// `session_idle`: the browser has been closed in between.
    pub fn from_stored(
        cookies: Vec<HttpCookie>,
        now: DateTime<Utc>,
        session_idle: std::time::Duration,
    ) -> Self {
        let loaded = cookies.len();
        let last_activity = cookies.iter().map(|cookie| cookie.last_used_at).max();
        let idle_limit = Duration::from_std(session_idle).unwrap_or(Duration::MAX);
        let session_over = last_activity.is_some_and(|last| now - last > idle_limit);
        let mut state = State::default();
        for cookie in cookies {
            if cookie.is_expired(now) || (session_over && cookie.is_session()) {
                continue;
            }
            state.entries.push(Entry {
                cookie,
                sequence: state.next_sequence,
            });
            state.next_sequence += 1;
        }
        // What was dropped must also go from the store.
        state.dirty = state.entries.len() != loaded;
        Self {
            state: Mutex::new(state),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stores what one `Set-Cookie` header of a response from `url` asks
    /// for.
    pub fn store(&self, url: &Url, header: &str, now: DateTime<Utc>) -> SetCookieVerdict {
        let (cookie, secure_origin) = match admit(url, header, now) {
            Ok(admitted) => admitted,
            Err(reason) => return SetCookieVerdict::Rejected(reason),
        };
        let expired = cookie.is_expired(now);

        let mut state = self.lock();
        if !secure_origin
            && state.entries.iter().any(|entry| {
                let other = &entry.cookie;
                other.secure
                    && !other.is_expired(now)
                    && other.name == cookie.name
                    && (domain_matches(&cookie.domain, &other.domain)
                        || domain_matches(&other.domain, &cookie.domain))
                    && path_matches(&cookie.path, &other.path)
            })
        {
            return SetCookieVerdict::Rejected("an insecure origin cannot replace a Secure cookie");
        }

        let existing = state
            .entries
            .iter()
            .position(|entry| same_cookie(&entry.cookie, &cookie));
        if expired {
            if let Some(index) = existing {
                state.entries.remove(index);
                state.dirty = true;
            }
            return SetCookieVerdict::Removed;
        }
        if let Some(index) = existing {
            // A replaced cookie keeps its age, and so its place in the
            // order it is sent in.
            let entry = &mut state.entries[index];
            entry.cookie = HttpCookie {
                created_at: entry.cookie.created_at,
                ..cookie
            };
        } else {
            let sequence = state.next_sequence;
            state.next_sequence += 1;
            let site = site_key(&cookie.domain);
            state.entries.push(Entry { cookie, sequence });
            evict(&mut state, &site, now);
        }
        state.dirty = true;
        SetCookieVerdict::Stored
    }

    /// The cookies a request to `url` carries, as `(name, value)` pairs in
    /// the order a browser sends them.
    pub fn matching(
        &self,
        url: &Url,
        context: CookieContext,
        now: DateTime<Utc>,
    ) -> Vec<(String, String)> {
        let Some(host) = site::host_of(url) else {
            return Vec::new();
        };
        if !matches!(url.scheme(), "http" | "https") {
            return Vec::new();
        }
        let secure_origin = site::is_trustworthy(url);
        let path = if url.path().is_empty() {
            "/"
        } else {
            url.path()
        };
        let granularity = Duration::minutes(TOUCH_GRANULARITY_MINUTES);

        let mut state = self.lock();
        let mut touched = false;
        let mut chosen: Vec<(usize, DateTime<Utc>, u64, String, String)> = Vec::new();
        for entry in &mut state.entries {
            let cookie = &mut entry.cookie;
            let applies = !cookie.is_expired(now)
                && (if cookie.host_only {
                    host == cookie.domain
                } else {
                    domain_matches(&host, &cookie.domain)
                })
                && path_matches(path, &cookie.path)
                && (!cookie.secure || secure_origin)
                && match cookie.same_site {
                    Some(CookieSameSite::None) => true,
                    Some(CookieSameSite::Strict) => context.same_site,
                    // A missing attribute is treated as Lax.
                    Some(CookieSameSite::Lax) | None => {
                        context.same_site || (context.top_level_navigation && context.safe_method)
                    }
                };
            if !applies {
                continue;
            }
            if now - cookie.last_used_at >= granularity {
                cookie.last_used_at = now;
                touched = true;
            }
            chosen.push((
                cookie.path.len(),
                cookie.created_at,
                entry.sequence,
                cookie.name.clone(),
                cookie.value.clone(),
            ));
        }
        state.dirty |= touched;
        drop(state);
        chosen.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then(left.1.cmp(&right.1))
                .then(left.2.cmp(&right.2))
        });
        chosen
            .into_iter()
            .map(|(_, _, _, name, value)| (name, value))
            .collect()
    }

    /// Every unexpired cookie, oldest first: what is persisted.
    pub fn snapshot(&self, now: DateTime<Utc>) -> Vec<HttpCookie> {
        let state = self.lock();
        let mut cookies: Vec<(DateTime<Utc>, u64, HttpCookie)> = state
            .entries
            .iter()
            .filter(|entry| !entry.cookie.is_expired(now))
            .map(|entry| {
                (
                    entry.cookie.created_at,
                    entry.sequence,
                    entry.cookie.clone(),
                )
            })
            .collect();
        drop(state);
        cookies.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
        cookies.into_iter().map(|(_, _, cookie)| cookie).collect()
    }

    /// Puts a cookie into the jar as it is, replacing the same cookie. For
    /// state that arrives from the browser path (milestone M6).
    pub fn import(&self, cookie: HttpCookie) {
        let mut state = self.lock();
        let existing = state
            .entries
            .iter()
            .position(|entry| same_cookie(&entry.cookie, &cookie));
        if let Some(index) = existing {
            state.entries[index].cookie = cookie;
        } else {
            let sequence = state.next_sequence;
            state.next_sequence += 1;
            state.entries.push(Entry { cookie, sequence });
        }
        state.dirty = true;
    }

    /// Whether the jar changed since this was last asked. Asking resets it.
    pub fn take_dirty(&self) -> bool {
        std::mem::take(&mut self.lock().dirty)
    }

    /// Marks the jar as changed, after a save that did not go through.
    pub fn mark_dirty(&self) {
        self.lock().dirty = true;
    }

    /// How many cookies the jar holds, expired ones included until they are
    /// next looked at.
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether the jar holds no cookie.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Drops expired cookies, then the least recently used ones of `site` and
/// of the jar until both are within their limits.
fn evict(state: &mut State, site: &str, now: DateTime<Utc>) {
    state.entries.retain(|entry| !entry.cookie.is_expired(now));
    let oldest = |entries: &[Entry], only_site: Option<&str>| {
        entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                only_site.is_none_or(|site| site_key(&entry.cookie.domain) == site)
            })
            .min_by_key(|(_, entry)| (entry.cookie.last_used_at, entry.sequence))
            .map(|(index, _)| index)
    };
    while state
        .entries
        .iter()
        .filter(|entry| site_key(&entry.cookie.domain) == site)
        .count()
        > MAX_COOKIES_PER_SITE
    {
        match oldest(&state.entries, Some(site)) {
            Some(index) => {
                state.entries.remove(index);
            }
            None => break,
        }
    }
    while state.entries.len() > MAX_COOKIES {
        match oldest(&state.entries, None) {
            Some(index) => {
                state.entries.remove(index);
            }
            None => break,
        }
    }
}

/// The jar as wreq sees it for one request: wreq asks it for the `Cookie`
/// header once it knows whether the connection speaks HTTP/2, and hands it
/// the `Set-Cookie` headers of the response.
pub(crate) struct HopCookies {
    jar: Arc<CookieJar>,
    url: Url,
    context: CookieContext,
}

impl HopCookies {
    pub(crate) const fn new(jar: Arc<CookieJar>, url: Url, context: CookieContext) -> Self {
        Self { jar, url, context }
    }
}

impl CookieStore for HopCookies {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, _uri: &Uri) {
        let now = Utc::now();
        for header in cookie_headers {
            let text = String::from_utf8_lossy(header.as_bytes());
            let verdict = self.jar.store(&self.url, &text, now);
            if let SetCookieVerdict::Rejected(reason) = verdict {
                tracing::debug!(host = ?self.url.host_str(), reason, "ignored a Set-Cookie header");
            }
        }
    }

    fn cookies(&self, _uri: &Uri, version: Version) -> Cookies {
        let pairs = self.jar.matching(&self.url, self.context, Utc::now());
        if pairs.is_empty() {
            return Cookies::Empty;
        }
        let crumbs = pairs.into_iter().map(|(name, value)| {
            if name.is_empty() {
                value
            } else {
                format!("{name}={value}")
            }
        });
        if matches!(version, Version::HTTP_2 | Version::HTTP_3) {
            // Both browsers send one field per cookie on HTTP/2, which
            // compresses better.
            Cookies::Uncompressed(
                crumbs
                    .filter_map(|crumb| HeaderValue::from_str(&crumb).ok())
                    .collect(),
            )
        } else {
            HeaderValue::from_str(&crumbs.collect::<Vec<_>>().join("; "))
                .map_or(Cookies::Empty, Cookies::Compressed)
        }
    }
}

/// Why a jar could not be loaded or saved.
#[derive(Debug, thiserror::Error)]
pub enum CookieBackendError {
    /// The proxy is retired: its cookies must not be used again.
    #[error("proxy {0} is retired; its cookies are not loaded")]
    ProxyRetired(ProxyId),
    /// The proxy is not in the store.
    #[error("proxy {0} is not known")]
    UnknownProxy(ProxyId),
    /// The store failed.
    #[error("cookie store: {0}")]
    Store(#[from] StoreError),
}

/// Where jars are kept between clients and across restarts.
#[async_trait]
pub trait CookieBackend: Send + Sync + fmt::Debug {
    /// The stored cookies of the client that leaves through `proxy`.
    ///
    /// # Errors
    ///
    /// Fails when the jar must not or cannot be loaded.
    async fn load(&self, proxy: ProxyId) -> Result<Vec<HttpCookie>, CookieBackendError>;

    /// Replaces the stored cookies of that client.
    ///
    /// # Errors
    ///
    /// Fails when the jar cannot be saved.
    async fn save(&self, proxy: ProxyId, cookies: &[HttpCookie]) -> Result<(), CookieBackendError>;
}

/// Jars kept in memory: for probes, tests and tools that must leave no
/// trace.
#[derive(Debug, Default)]
pub struct MemoryCookies {
    jars: Mutex<HashMap<ProxyId, Vec<HttpCookie>>>,
}

impl MemoryCookies {
    /// No jars.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl CookieBackend for MemoryCookies {
    async fn load(&self, proxy: ProxyId) -> Result<Vec<HttpCookie>, CookieBackendError> {
        Ok(self
            .jars
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&proxy)
            .cloned()
            .unwrap_or_default())
    }

    async fn save(&self, proxy: ProxyId, cookies: &[HttpCookie]) -> Result<(), CookieBackendError> {
        self.jars
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(proxy, cookies.to_vec());
        Ok(())
    }
}

/// Jars kept in Postgres, in `http_cookies`.
#[derive(Debug, Clone)]
pub struct StoreCookies {
    store: Store,
}

impl StoreCookies {
    /// Jars in this store.
    pub const fn new(store: Store) -> Self {
        Self { store }
    }

    async fn live(&self, proxy: ProxyId) -> Result<(), CookieBackendError> {
        match self.store.proxies().get(proxy).await? {
            None => Err(CookieBackendError::UnknownProxy(proxy)),
            Some(found) if !found.state.is_live() => Err(CookieBackendError::ProxyRetired(proxy)),
            Some(_) => Ok(()),
        }
    }
}

#[async_trait]
impl CookieBackend for StoreCookies {
    async fn load(&self, proxy: ProxyId) -> Result<Vec<HttpCookie>, CookieBackendError> {
        self.live(proxy).await?;
        Ok(self.store.http_cookies().list(proxy).await?)
    }

    async fn save(&self, proxy: ProxyId, cookies: &[HttpCookie]) -> Result<(), CookieBackendError> {
        // A proxy retired while a request was in flight keeps nothing.
        self.live(proxy).await?;
        Ok(self.store.http_cookies().replace(proxy, cookies).await?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration as StdDuration;

    use chrono::{DateTime, Duration, TimeZone, Utc};
    use http::{HeaderValue, Uri, Version};
    use kage_core::domain::{CookieSameSite, HttpCookie};
    use kage_core::ids::ProxyId;
    use url::Url;
    use wreq::cookie::{CookieStore, Cookies};

    use super::{
        CookieBackend, CookieContext, CookieJar, HopCookies, MAX_COOKIES_PER_SITE, MemoryCookies,
        SetCookieVerdict, default_path, domain_matches, parse_http_date, parse_set_cookie,
        path_matches,
    };

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap()
    }

    const CROSS_SITE_LINK: CookieContext = CookieContext {
        same_site: false,
        top_level_navigation: true,
        safe_method: true,
    };
    const CROSS_SITE_POST: CookieContext = CookieContext {
        same_site: false,
        top_level_navigation: true,
        safe_method: false,
    };
    const CROSS_SITE_SUBRESOURCE: CookieContext = CookieContext {
        same_site: false,
        top_level_navigation: false,
        safe_method: true,
    };

    fn names(jar: &CookieJar, target: &str, context: CookieContext) -> Vec<String> {
        jar.matching(&url(target), context, now())
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    fn stored(jar: &CookieJar, from: &str, header: &str) -> SetCookieVerdict {
        jar.store(&url(from), header, now())
    }

    #[test]
    fn a_set_cookie_header_is_read_with_all_its_attributes() {
        let parsed = parse_set_cookie(
            "sid = abc123 ; Domain=.Example.ORG; Path=/shop; Max-Age=3600; \
             Expires=Wed, 21 Oct 2026 07:28:00 GMT; Secure; HttpOnly; SameSite=Lax; Unknown=1",
        )
        .unwrap();

        assert_eq!(parsed.name, "sid");
        assert_eq!(parsed.value, "abc123");
        assert_eq!(parsed.domain.as_deref(), Some("example.org"));
        assert_eq!(parsed.path.as_deref(), Some("/shop"));
        assert_eq!(parsed.max_age, Some(3600));
        assert_eq!(
            parsed.expires,
            Some(Utc.with_ymd_and_hms(2026, 10, 21, 7, 28, 0).unwrap())
        );
        assert!(parsed.secure && parsed.http_only);
        assert_eq!(parsed.same_site, Some(CookieSameSite::Lax));

        // A value may hold `=`; the last attribute wins; odd values are
        // ignored, not fatal.
        let parsed = parse_set_cookie(
            "token=a=b==; path=relative; Path=/a; path=/b; max-age=soon; samesite=whatever; \
             domain=; secure=yes",
        )
        .unwrap();
        assert_eq!(parsed.value, "a=b==");
        assert_eq!(parsed.path.as_deref(), Some("/b"));
        assert_eq!(parsed.max_age, None);
        assert_eq!(parsed.same_site, None);
        assert_eq!(parsed.domain, None);
        assert!(parsed.secure);

        assert_eq!(parse_set_cookie("flag").unwrap().name, "");
        assert_eq!(parse_set_cookie("flag").unwrap().value, "flag");
        assert_eq!(
            parse_set_cookie("a=1; Max-Age=-5").unwrap().max_age,
            Some(-5)
        );
        assert_eq!(
            parse_set_cookie("a=1; Max-Age=99999999999999999999999")
                .unwrap()
                .max_age,
            Some(i64::MAX)
        );
        assert!(parse_set_cookie("").is_err());
        assert!(parse_set_cookie("=; Path=/").is_err());
        assert!(parse_set_cookie("a=b\u{1}c").is_err());
        assert!(parse_set_cookie(&format!("a={}", "x".repeat(4096))).is_err());
        assert!(parse_set_cookie(&format!("a={}", "x".repeat(4095))).is_ok());
        // An attribute value that is too long is skipped, the cookie is kept.
        let long = parse_set_cookie(&format!("a=1; Path=/{}", "p".repeat(1024))).unwrap();
        assert_eq!(long.path, None);
    }

    #[test]
    fn cookie_dates_are_read_leniently() {
        let expected = Utc.with_ymd_and_hms(2026, 10, 21, 7, 28, 0).unwrap();
        for text in [
            "Wed, 21 Oct 2026 07:28:00 GMT",
            "Wed, 21-Oct-2026 07:28:00 GMT",
            "Wednesday, 21-Oct-26 07:28:00 GMT",
            "Wed Oct 21 07:28:00 2026",
            "21 October 2026 07:28:00",
            "07:28:00 21 oct 2026",
        ] {
            assert_eq!(parse_http_date(text), Some(expected), "{text}");
        }
        assert_eq!(
            parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"),
            Some(Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap())
        );
        assert_eq!(
            parse_http_date("Tue, 19 Jan 38 03:14:07 GMT"),
            Some(Utc.with_ymd_and_hms(2038, 1, 19, 3, 14, 7).unwrap())
        );
        for text in [
            "",
            "tomorrow",
            "Wed, 21 Oct 2026",
            "Wed, 32 Oct 2026 07:28:00 GMT",
            "Wed, 21 Foo 2026 07:28:00 GMT",
            "Wed, 21 Oct 2026 25:28:00 GMT",
            "Wed, 21 Oct 1500 07:28:00 GMT",
            "30 Feb 2026 07:28:00",
        ] {
            assert_eq!(parse_http_date(text), None, "{text}");
        }
    }

    #[test]
    fn domain_and_path_matching_follow_the_rfc() {
        assert!(domain_matches("example.org", "example.org"));
        assert!(domain_matches("www.example.org", "example.org"));
        assert!(domain_matches("a.b.example.org", "example.org"));
        assert!(!domain_matches("badexample.org", "example.org"));
        assert!(!domain_matches("example.org", "www.example.org"));
        assert!(!domain_matches("203.0.113.9", "0.113.9"));
        assert!(domain_matches("203.0.113.9", "203.0.113.9"));

        assert_eq!(default_path("/"), "/");
        assert_eq!(default_path("/index.html"), "/");
        assert_eq!(default_path("/shop/item"), "/shop");
        assert_eq!(default_path("/shop/item/"), "/shop/item");
        assert_eq!(default_path(""), "/");

        assert!(path_matches("/shop", "/shop"));
        assert!(path_matches("/shop/item", "/shop"));
        assert!(path_matches("/shop/item", "/shop/"));
        assert!(path_matches("/anything", "/"));
        assert!(!path_matches("/shopping", "/shop"));
        assert!(!path_matches("/", "/shop"));
    }

    #[test]
    fn host_only_and_domain_cookies_reach_the_right_hosts() {
        let jar = CookieJar::new();
        assert_eq!(
            stored(&jar, "https://www.example.org/", "host=1"),
            SetCookieVerdict::Stored
        );
        assert_eq!(
            stored(
                &jar,
                "https://www.example.org/",
                "site=1; Domain=example.org"
            ),
            SetCookieVerdict::Stored
        );

        assert_eq!(
            names(&jar, "https://www.example.org/", CookieContext::DIRECT),
            ["host", "site"]
        );
        assert_eq!(
            names(&jar, "https://api.example.org/", CookieContext::DIRECT),
            ["site"]
        );
        assert_eq!(
            names(&jar, "https://example.org/", CookieContext::DIRECT),
            ["site"]
        );
        assert_eq!(
            names(&jar, "https://example.com/", CookieContext::DIRECT),
            Vec::<String>::new()
        );
        assert_eq!(
            names(&jar, "https://notexample.org/", CookieContext::DIRECT),
            Vec::<String>::new()
        );
        assert_eq!(jar.len(), 2);
        assert!(!jar.is_empty());
        assert_eq!(format!("{jar:?}"), "CookieJar { cookies: 2 }");
    }

    #[test]
    fn a_cookie_for_another_site_or_a_public_suffix_is_refused() {
        let jar = CookieJar::new();

        for (from, header) in [
            ("https://www.example.org/", "a=1; Domain=example.com"),
            ("https://www.example.org/", "a=1; Domain=other.example.org"),
            ("https://www.example.co.uk/", "a=1; Domain=co.uk"),
            ("https://www.example.org/", "a=1; Domain=org"),
            ("https://alice.github.io/", "a=1; Domain=github.io"),
            ("https://203.0.113.9/", "a=1; Domain=113.9"),
        ] {
            assert!(
                matches!(stored(&jar, from, header), SetCookieVerdict::Rejected(_)),
                "{from} {header}"
            );
        }
        assert!(jar.is_empty());

        // A public suffix that is the host itself gets a host-only cookie.
        assert_eq!(
            stored(&jar, "https://github.io/", "a=1; Domain=github.io"),
            SetCookieVerdict::Stored
        );
        assert_eq!(
            names(&jar, "https://github.io/", CookieContext::DIRECT),
            ["a"]
        );
        assert_eq!(
            names(&jar, "https://alice.github.io/", CookieContext::DIRECT),
            Vec::<String>::new()
        );
        // The same for an address.
        assert_eq!(
            stored(&jar, "https://203.0.113.9/", "b=1; Domain=203.0.113.9"),
            SetCookieVerdict::Stored
        );
    }

    #[test]
    fn secure_cookies_and_prefixes_need_a_secure_origin() {
        let jar = CookieJar::new();

        for header in [
            "a=1; Secure",
            "__Secure-a=1; Secure",
            "__Secure-a=1",
            "__Host-a=1; Secure; Path=/",
        ] {
            assert!(
                matches!(
                    stored(&jar, "http://www.example.org/", header),
                    SetCookieVerdict::Rejected(_)
                ),
                "{header}"
            );
        }
        for header in [
            "__Secure-a=1",
            "__Host-a=1; Secure",
            "__Host-a=1; Secure; Path=/; Domain=example.org",
            "__host-b=1; Path=/",
            "c=1; SameSite=None",
        ] {
            assert!(
                matches!(
                    stored(&jar, "https://www.example.org/dir/page", header),
                    SetCookieVerdict::Rejected(_)
                ),
                "{header}"
            );
        }
        assert!(jar.is_empty());

        for header in [
            "__Secure-a=1; Secure",
            "__Host-b=1; Secure; Path=/",
            "c=1; SameSite=None; Secure",
        ] {
            assert_eq!(
                stored(&jar, "https://www.example.org/dir/page", header),
                SetCookieVerdict::Stored,
                "{header}"
            );
        }
        // A secure cookie is never sent over plain http.
        assert_eq!(
            names(
                &jar,
                "http://www.example.org/dir/other",
                CookieContext::DIRECT
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            names(
                &jar,
                "https://www.example.org/dir/other",
                CookieContext::DIRECT
            ),
            ["__Secure-a", "c", "__Host-b"]
        );
        // Without a Path attribute a cookie belongs to the directory it was
        // set from.
        assert_eq!(
            names(&jar, "https://www.example.org/", CookieContext::DIRECT),
            ["__Host-b"]
        );
        // This machine counts as secure, as it does in a browser.
        assert_eq!(
            stored(&jar, "http://127.0.0.1:8080/", "local=1; Secure"),
            SetCookieVerdict::Stored
        );
    }

    #[test]
    fn an_insecure_origin_cannot_replace_a_secure_cookie() {
        let jar = CookieJar::new();
        assert_eq!(
            stored(
                &jar,
                "https://www.example.org/",
                "sid=secure; Secure; Domain=example.org"
            ),
            SetCookieVerdict::Stored
        );

        assert!(matches!(
            stored(
                &jar,
                "http://www.example.org/",
                "sid=evil; Domain=example.org"
            ),
            SetCookieVerdict::Rejected(_)
        ));
        assert!(matches!(
            stored(&jar, "http://www.example.org/", "sid=evil"),
            SetCookieVerdict::Rejected(_)
        ));
        // Another name is fine.
        assert_eq!(
            stored(&jar, "http://www.example.org/", "other=1"),
            SetCookieVerdict::Stored
        );
        assert_eq!(
            jar.matching(
                &url("https://www.example.org/"),
                CookieContext::DIRECT,
                now()
            ),
            [
                ("sid".to_owned(), "secure".to_owned()),
                ("other".to_owned(), "1".to_owned())
            ]
        );
    }

    #[test]
    fn same_site_decides_what_a_request_from_another_site_carries() {
        let jar = CookieJar::new();
        for header in [
            "strict=1; SameSite=Strict",
            "lax=1; SameSite=Lax",
            "unset=1",
            "none=1; SameSite=None; Secure",
        ] {
            assert_eq!(
                stored(&jar, "https://www.example.org/", header),
                SetCookieVerdict::Stored
            );
        }
        let target = "https://www.example.org/";

        // From the site itself, or typed by the user: everything.
        assert_eq!(
            names(&jar, target, CookieContext::DIRECT),
            ["strict", "lax", "unset", "none"]
        );
        // A link on another site: not the strict one.
        assert_eq!(
            names(&jar, target, CROSS_SITE_LINK),
            ["lax", "unset", "none"]
        );
        // A form on another site, or a subresource: only SameSite=None.
        assert_eq!(names(&jar, target, CROSS_SITE_POST), ["none"]);
        assert_eq!(names(&jar, target, CROSS_SITE_SUBRESOURCE), ["none"]);
    }

    #[test]
    fn cookies_are_sent_longest_path_first_then_oldest_first() {
        let jar = CookieJar::new();
        let base = now();
        let from = url("https://www.example.org/shop/cart/view");
        for (offset, header) in [
            (0, "root_old=1; Path=/"),
            (1, "deep=1; Path=/shop/cart"),
            (2, "mid=1; Path=/shop"),
            (3, "root_new=1; Path=/"),
            (4, "default=1"),
        ] {
            assert_eq!(
                jar.store(&from, header, base + Duration::seconds(offset)),
                SetCookieVerdict::Stored
            );
        }

        assert_eq!(
            names(
                &jar,
                "https://www.example.org/shop/cart/view",
                CookieContext::DIRECT
            ),
            ["deep", "default", "mid", "root_old", "root_new"]
        );
        assert_eq!(
            names(
                &jar,
                "https://www.example.org/shop/other",
                CookieContext::DIRECT
            ),
            ["mid", "root_old", "root_new"]
        );
        assert_eq!(
            names(&jar, "https://www.example.org/", CookieContext::DIRECT),
            ["root_old", "root_new"]
        );

        // Replacing a cookie keeps its age, and so its place.
        assert_eq!(
            jar.store(&from, "root_old=2; Path=/", base + Duration::seconds(60)),
            SetCookieVerdict::Stored
        );
        assert_eq!(
            jar.matching(
                &url("https://www.example.org/"),
                CookieContext::DIRECT,
                now()
            ),
            [
                ("root_old".to_owned(), "2".to_owned()),
                ("root_new".to_owned(), "1".to_owned())
            ]
        );
    }

    #[test]
    fn expiry_follows_max_age_then_expires_and_is_capped() {
        let jar = CookieJar::new();
        let from = url("https://www.example.org/");

        jar.store(&from, "short=1; Max-Age=60", now());
        jar.store(
            &from,
            "both=1; Max-Age=120; Expires=Wed, 21 Oct 2099 07:28:00 GMT",
            now(),
        );
        jar.store(
            &from,
            "dated=1; Expires=Sat, 10 Oct 2026 12:00:00 GMT",
            now(),
        );
        jar.store(&from, "forever=1; Max-Age=99999999999", now());
        jar.store(&from, "far=1; Expires=Wed, 21 Oct 2099 07:28:00 GMT", now());
        jar.store(&from, "session=1", now());

        let by_name = |name: &str| {
            jar.snapshot(now())
                .into_iter()
                .find(|cookie| cookie.name == name)
                .unwrap()
        };
        assert_eq!(
            by_name("short").expires_at,
            Some(now() + Duration::seconds(60))
        );
        assert_eq!(
            by_name("both").expires_at,
            Some(now() + Duration::seconds(120))
        );
        assert_eq!(by_name("dated").expires_at, Some(now() + Duration::days(1)));
        assert_eq!(
            by_name("forever").expires_at,
            Some(now() + Duration::days(400))
        );
        assert_eq!(by_name("far").expires_at, Some(now() + Duration::days(400)));
        assert_eq!(by_name("session").expires_at, None);
        assert!(by_name("session").is_session());

        // Time passes: the short one is neither sent nor saved.
        let later = now() + Duration::seconds(90);
        let sent: Vec<String> = jar
            .matching(&from, CookieContext::DIRECT, later)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(sent, ["both", "dated", "forever", "far", "session"]);
        assert_eq!(jar.snapshot(later).len(), 5);
    }

    #[test]
    fn an_expired_set_cookie_deletes_the_cookie() {
        let jar = CookieJar::new();
        stored(&jar, "https://www.example.org/", "sid=1; Path=/");
        stored(&jar, "https://www.example.org/", "keep=1; Path=/");
        assert!(jar.take_dirty());
        assert!(!jar.take_dirty());

        assert_eq!(
            stored(&jar, "https://www.example.org/", "sid=; Path=/; Max-Age=0"),
            SetCookieVerdict::Removed
        );
        assert!(jar.take_dirty());
        assert_eq!(
            names(&jar, "https://www.example.org/", CookieContext::DIRECT),
            ["keep"]
        );

        // Deleting what is not there changes nothing.
        assert_eq!(
            stored(
                &jar,
                "https://www.example.org/",
                "ghost=; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
            ),
            SetCookieVerdict::Removed
        );
        assert!(!jar.take_dirty());
        // Another path is another cookie.
        assert_eq!(
            stored(
                &jar,
                "https://www.example.org/",
                "keep=; Path=/other; Max-Age=0"
            ),
            SetCookieVerdict::Removed
        );
        assert_eq!(jar.len(), 1);
    }

    #[test]
    fn a_site_cannot_fill_the_jar() {
        let jar = CookieJar::new();
        let base = now();
        for index in 0..MAX_COOKIES_PER_SITE + 20 {
            let host = if index % 2 == 0 { "a" } else { "b" };
            jar.store(
                &url(&format!("https://{host}.example.org/")),
                &format!("c{index}=1"),
                base + Duration::seconds(i64::try_from(index).unwrap()),
            );
        }
        jar.store(&url("https://other.example.net/"), "kept=1", base);

        assert_eq!(jar.len(), MAX_COOKIES_PER_SITE + 1);
        let snapshot = jar.snapshot(base + Duration::hours(1));
        // The least recently used of that site went; the other site's stayed.
        assert!(!snapshot.iter().any(|cookie| cookie.name == "c0"));
        assert!(!snapshot.iter().any(|cookie| cookie.name == "c19"));
        assert!(snapshot.iter().any(|cookie| cookie.name == "c20"));
        assert!(snapshot.iter().any(|cookie| cookie.name == "kept"));
    }

    fn cookie(name: &str, expires_at: Option<DateTime<Utc>>, used: DateTime<Utc>) -> HttpCookie {
        HttpCookie {
            domain: "www.example.org".to_owned(),
            host_only: true,
            path: "/".to_owned(),
            name: name.to_owned(),
            value: "1".to_owned(),
            secure: false,
            http_only: false,
            same_site: None,
            expires_at,
            created_at: used - Duration::days(1),
            last_used_at: used,
        }
    }

    #[test]
    fn a_loaded_jar_drops_what_expired_and_ends_the_session_after_long_idleness() {
        let idle = StdDuration::from_hours(12);
        let recently = now() - Duration::hours(2);
        let cookies = vec![
            cookie("session", None, recently),
            cookie("lasting", Some(now() + Duration::days(30)), recently),
            cookie("expired", Some(now() - Duration::minutes(1)), recently),
        ];

        // Used two hours ago: the session goes on.
        let jar = CookieJar::from_stored(cookies.clone(), now(), idle);
        assert_eq!(
            names(&jar, "http://www.example.org/", CookieContext::DIRECT),
            ["session", "lasting"]
        );
        // The expired cookie has to go from the store too.
        assert!(jar.take_dirty());

        // Nothing used for thirteen hours: the browser was closed.
        let later = now() + Duration::hours(11);
        let jar = CookieJar::from_stored(cookies.clone(), later, idle);
        assert_eq!(
            jar.matching(
                &url("http://www.example.org/"),
                CookieContext::DIRECT,
                later
            )
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>(),
            ["lasting"]
        );

        // One recently used cookie keeps the whole session alive.
        let mut mixed = cookies;
        mixed.push(cookie(
            "fresh",
            Some(later + Duration::days(1)),
            later - Duration::hours(1),
        ));
        let jar = CookieJar::from_stored(mixed, later, idle);
        assert_eq!(jar.len(), 3);

        // Nothing to drop: nothing to write.
        let clean = CookieJar::from_stored(vec![cookie("a", None, recently)], now(), idle);
        assert!(!clean.take_dirty());
        assert!(CookieJar::from_stored(Vec::new(), now(), idle).is_empty());
    }

    #[test]
    fn reading_cookies_rewrites_the_jar_only_now_and_then() {
        let jar = CookieJar::new();
        let target = url("https://www.example.org/");
        jar.store(&target, "sid=1", now());
        assert!(jar.take_dirty());

        jar.matching(
            &target,
            CookieContext::DIRECT,
            now() + Duration::seconds(30),
        );
        assert!(!jar.take_dirty());
        jar.matching(&target, CookieContext::DIRECT, now() + Duration::minutes(6));
        assert!(jar.take_dirty());
        assert_eq!(
            jar.snapshot(now())[0].last_used_at,
            now() + Duration::minutes(6)
        );
        jar.mark_dirty();
        assert!(jar.take_dirty());
    }

    #[test]
    fn a_cookie_from_the_browser_is_imported_as_it_is() {
        let jar = CookieJar::new();
        let mut from_browser = cookie("cf_clearance", Some(now() + Duration::days(1)), now());
        from_browser.domain = "example.org".to_owned();
        from_browser.host_only = false;
        from_browser.secure = true;
        from_browser.http_only = true;
        from_browser.same_site = Some(CookieSameSite::None);

        jar.import(from_browser.clone());
        assert!(jar.take_dirty());
        assert_eq!(
            names(&jar, "https://shop.example.org/", CROSS_SITE_SUBRESOURCE),
            ["cf_clearance"]
        );

        let mut replaced = from_browser.clone();
        replaced.value = "2".to_owned();
        jar.import(replaced);
        assert_eq!(jar.len(), 1);
        assert_eq!(jar.snapshot(now())[0].value, "2");
    }

    #[test]
    fn nothing_is_stored_or_sent_for_urls_that_are_not_http() {
        let jar = CookieJar::new();
        assert!(matches!(
            stored(&jar, "ftp://example.org/", "a=1"),
            SetCookieVerdict::Rejected(_)
        ));
        assert!(matches!(
            stored(&jar, "data:text/plain,hi", "a=1"),
            SetCookieVerdict::Rejected(_)
        ));
        stored(&jar, "https://example.org/", "a=1");
        assert_eq!(
            names(&jar, "ftp://example.org/", CookieContext::DIRECT),
            Vec::<String>::new()
        );
        assert_eq!(
            names(&jar, "data:text/plain,hi", CookieContext::DIRECT),
            Vec::<String>::new()
        );
    }

    #[test]
    fn wreq_gets_one_field_per_cookie_on_http2_and_one_line_on_http1() {
        let jar = Arc::new(CookieJar::new());
        let target = url("https://www.example.org/");
        let uri: Uri = "https://www.example.org/".parse().unwrap();
        let hop = HopCookies::new(Arc::clone(&jar), target.clone(), CookieContext::DIRECT);

        assert!(matches!(hop.cookies(&uri, Version::HTTP_2), Cookies::Empty));

        let headers = [
            HeaderValue::from_static("a=1; Path=/"),
            HeaderValue::from_static("b=2; Path=/"),
            HeaderValue::from_static("nameless"),
            HeaderValue::from_static("bad=1; Domain=example.com"),
        ];
        hop.set_cookies(&mut headers.iter(), &uri);
        assert_eq!(jar.len(), 3);

        let Cookies::Uncompressed(fields) = hop.cookies(&uri, Version::HTTP_2) else {
            panic!("HTTP/2 cookies are sent as separate fields");
        };
        assert_eq!(fields, ["a=1", "b=2", "nameless"]);
        let Cookies::Compressed(line) = hop.cookies(&uri, Version::HTTP_11) else {
            panic!("HTTP/1.1 cookies are sent as one line");
        };
        assert_eq!(line, "a=1; b=2; nameless");

        // Another host gets nothing.
        let elsewhere = HopCookies::new(jar, url("https://example.com/"), CookieContext::DIRECT);
        assert!(matches!(
            elsewhere.cookies(&uri, Version::HTTP_11),
            Cookies::Empty
        ));
    }

    #[tokio::test]
    async fn the_memory_backend_keeps_one_jar_per_proxy() {
        let backend = MemoryCookies::new();
        let (first, second) = (ProxyId::new(), ProxyId::new());

        assert_eq!(backend.load(first).await.unwrap(), []);
        backend
            .save(first, &[cookie("a", None, now())])
            .await
            .unwrap();
        backend
            .save(
                second,
                &[cookie("b", None, now()), cookie("c", None, now())],
            )
            .await
            .unwrap();

        assert_eq!(backend.load(first).await.unwrap().len(), 1);
        assert_eq!(backend.load(second).await.unwrap().len(), 2);
        backend.save(first, &[]).await.unwrap();
        assert_eq!(backend.load(first).await.unwrap(), []);
    }
}
