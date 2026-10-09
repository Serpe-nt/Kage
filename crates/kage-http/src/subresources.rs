//! Subresource emulation (spec §9, `http.fetch_subresources`, off by
//! default): after a page, fetch some of the assets a browser fetches for
//! its first paint.
//!
//! # What this is and is not
//!
//! A client that only ever asks for documents is easy to tell from a
//! browser by its request log. A partial imitation is a different tell: a
//! browser also fetches images, fonts and third-party scripts, and runs the
//! scripts. So this helps against checks of the kind "fetched the page,
//! never an asset" and against nothing that runs JavaScript, and it costs
//! bandwidth. When it is on it is shaped like the start of a real page
//! load:
//!
//! - **What.** Only after a `200` HTML response to a `GET`. In document
//!   order: stylesheets (`<link rel=stylesheet>`), scripts (`<script src>`
//!   without `nomodule` and with a script type), and style or script
//!   preloads; then the icon (`<link rel~=icon>`, else `/favicon.ico`).
//!   `<base href>` is honoured. Markup inside comments, scripts, styles,
//!   `<noscript>` and `<template>` is not markup and is skipped.
//! - **First-party only.** An asset is fetched when it is on the page's own
//!   site (or origin, by configuration). Never another site's: a CDN, an
//!   analytics host or a bot-management vendor would see a script that is
//!   downloaded and never run, which is worse than not asking.
//! - **Shape.** The assets start 5 to 40 ms after the page's body arrived,
//!   in document order, a few milliseconds apart, at most six in flight,
//!   on the page's own connection where the protocol allows. The icon comes
//!   last, 100 to 600 ms after the others are done. The pauses come from a
//!   seeded generator and the seed is reported.
//! - **Bodies** are read to the end and dropped: abandoning a transfer
//!   half-way is a tell and wastes the connection. Each asset and the page
//!   as a whole have a byte limit.
//! - **Cache.** A returning visitor has these assets cached. The client
//!   keeps, in memory, each asset's validators and how long it stays fresh
//!   (`Cache-Control`, `Expires`, or a tenth of its age as browsers
//!   estimate). A fresh asset is not asked for again; a stale one is
//!   revalidated with `If-None-Match` or `If-Modified-Since`. The cache is
//!   lost at a restart, which looks like a cleared cache.
//!
//! A failing asset never fails the fetch. The header values of asset
//! requests (their `Accept`, `Priority` and order) are written from
//! published captures and are confirmed when M4 and M5 record the real
//! browsers.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use http::header::{
    CACHE_CONTROL, ETAG, EXPIRES, HeaderMap, HeaderName, HeaderValue, IF_MODIFIED_SINCE,
    IF_NONE_MATCH, LAST_MODIFIED, LOCATION,
};
use http::{Method, StatusCode};
use kage_core::config::SubresourceScope;
use kage_core::seed::Seed;
use kage_core::ssrf::check_url;
use tokio::time::Instant;
use url::Url;

use crate::client::HttpClient;
use crate::cookies::parse_http_date;
use crate::fetch::{Fetched, Guard, Outgoing, read_body};
use crate::headers::{CallerHeaders, FetchSite, RequestKind};
use crate::site;

/// How much of a page is searched for assets.
const SCAN_LIMIT: usize = 512 * 1024;

/// Redirects followed for one asset.
const ASSET_REDIRECTS: usize = 3;

/// Longest the assets of one page may take together.
const ASSETS_TIMEOUT: Duration = Duration::from_secs(20);

/// Most assets the freshness cache remembers.
const CACHE_ENTRIES: usize = 4096;

/// Longest an asset is taken as fresh, whatever it claims.
const MAX_FRESH: Duration = Duration::from_hours(365 * 24);

/// Longest an asset without an expiry is taken as fresh by its age.
const MAX_HEURISTIC_FRESH: Duration = Duration::from_hours(24);

/// The contents of these elements are text, not markup.
const RAW_TEXT: [&str; 9] = [
    "script", "style", "textarea", "title", "noscript", "template", "xmp", "iframe", "noframes",
];

/// What a page's markup asks a browser to fetch, as written.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Discovered {
    /// `<base href>`, the first one.
    pub base: Option<String>,
    /// Stylesheets and scripts in document order.
    pub assets: Vec<(RequestKind, String)>,
    /// `<link rel~=icon>`, the first one.
    pub icon: Option<String>,
}

fn lower(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).to_ascii_lowercase()
}

/// `&amp;` and its relatives, as they appear in an `href`.
fn decode_entities(value: &str) -> String {
    if !value.contains('&') {
        return value.to_owned();
    }
    value
        .replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&#x26;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// Reads the attributes of a start tag from `at`, the byte after the tag
/// name. Returns them with lowercase names and the index after the `>`.
fn attributes(html: &[u8], mut at: usize) -> (Vec<(String, String)>, usize) {
    let mut found = Vec::new();
    loop {
        while at < html.len() && (html[at].is_ascii_whitespace() || html[at] == b'/') {
            at += 1;
        }
        if at >= html.len() {
            return (found, at);
        }
        if html[at] == b'>' {
            return (found, at + 1);
        }
        let name_start = at;
        while at < html.len()
            && !html[at].is_ascii_whitespace()
            && !matches!(html[at], b'=' | b'>' | b'/')
        {
            at += 1;
        }
        let name = lower(&html[name_start..at]);
        while at < html.len() && html[at].is_ascii_whitespace() {
            at += 1;
        }
        let mut value = String::new();
        if at < html.len() && html[at] == b'=' {
            at += 1;
            while at < html.len() && html[at].is_ascii_whitespace() {
                at += 1;
            }
            if at < html.len() && matches!(html[at], b'"' | b'\'') {
                let quote = html[at];
                at += 1;
                let value_start = at;
                while at < html.len() && html[at] != quote {
                    at += 1;
                }
                value = String::from_utf8_lossy(&html[value_start..at]).into_owned();
                at = (at + 1).min(html.len());
            } else {
                let value_start = at;
                while at < html.len() && !html[at].is_ascii_whitespace() && html[at] != b'>' {
                    at += 1;
                }
                value = String::from_utf8_lossy(&html[value_start..at]).into_owned();
            }
        }
        if !name.is_empty() {
            found.push((name, decode_entities(value.trim())));
        } else if at < html.len() && html[at] == b'=' {
            // A stray `=`: step over it so the loop always advances.
            at += 1;
        }
    }
}

fn find(html: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    html.get(from..)?
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
        .map(|position| from + position)
}

/// Finds what a page asks a browser to fetch for its first paint.
pub(crate) fn scan(html: &[u8]) -> Discovered {
    let html = &html[..html.len().min(SCAN_LIMIT)];
    let mut discovered = Discovered::default();
    let mut at = 0;
    while let Some(open) = find(html, at, b"<") {
        at = open + 1;
        if html[at..].starts_with(b"!--") {
            match find(html, at, b"-->") {
                Some(end) => at = end + 3,
                None => break,
            }
            continue;
        }
        let name_start = at;
        while at < html.len() && html[at].is_ascii_alphanumeric() {
            at += 1;
        }
        if at == name_start {
            // An end tag, a doctype, or a `<` that is just text.
            continue;
        }
        let name = lower(&html[name_start..at]);
        let (attrs, after) = attributes(html, at);
        at = after;
        let attr = |wanted: &str| {
            attrs
                .iter()
                .find(|(name, _)| name == wanted)
                .map(|(_, value)| value.as_str())
        };
        match name.as_str() {
            "base" => {
                if discovered.base.is_none()
                    && let Some(href) = attr("href").filter(|href| !href.is_empty())
                {
                    discovered.base = Some(href.to_owned());
                }
            }
            "link" => {
                let Some(href) = attr("href").filter(|href| !href.is_empty()) else {
                    continue;
                };
                let rel = attr("rel").unwrap_or_default().to_ascii_lowercase();
                let rel: Vec<&str> = rel.split_ascii_whitespace().collect();
                if rel.contains(&"stylesheet") {
                    // An alternate stylesheet is only loaded on request,
                    // a disabled one not at all.
                    if !rel.contains(&"alternate") && attr("disabled").is_none() {
                        discovered
                            .assets
                            .push((RequestKind::Style, href.to_owned()));
                    }
                } else if rel.contains(&"preload") {
                    match attr("as").unwrap_or_default().to_ascii_lowercase().as_str() {
                        "style" => discovered
                            .assets
                            .push((RequestKind::Style, href.to_owned())),
                        "script" => {
                            discovered
                                .assets
                                .push((RequestKind::Script, href.to_owned()));
                        }
                        _ => {}
                    }
                } else if rel.contains(&"icon") && discovered.icon.is_none() {
                    discovered.icon = Some(href.to_owned());
                }
            }
            "script" => {
                let kind = attr("type").unwrap_or_default().trim().to_ascii_lowercase();
                let runs = kind.is_empty()
                    || kind == "module"
                    || kind.ends_with("/javascript")
                    || kind.ends_with("/ecmascript");
                if runs
                    && attr("nomodule").is_none()
                    && let Some(src) = attr("src").filter(|src| !src.is_empty())
                {
                    discovered
                        .assets
                        .push((RequestKind::Script, src.to_owned()));
                }
            }
            _ => {}
        }
        if RAW_TEXT.contains(&name.as_str()) {
            let close = format!("</{name}");
            match find(html, at, close.as_bytes()) {
                Some(end) => at = end,
                None => break,
            }
        }
    }
    discovered
}

/// One asset to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Asset {
    pub kind: RequestKind,
    pub url: Url,
}

/// Turns what the markup names into the first-party assets to fetch, in
/// document order, and the icon.
pub(crate) fn plan(
    document: &Url,
    discovered: &Discovered,
    scope: SubresourceScope,
    max: usize,
) -> (Vec<Asset>, Option<Asset>) {
    let base = discovered
        .base
        .as_deref()
        .and_then(|href| document.join(href.trim()).ok())
        .filter(|base| matches!(base.scheme(), "http" | "https"))
        .unwrap_or_else(|| document.clone());
    let own = |url: &Url| match scope {
        SubresourceScope::SameSite => site::same_site(document, url),
        SubresourceScope::SameOrigin => site::same_origin(document, url),
    };
    let resolve = |href: &str| -> Option<Url> {
        let href = href.trim();
        if href.is_empty() || href.starts_with('#') {
            return None;
        }
        let mut url = base.join(href).ok()?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return None;
        }
        url.set_fragment(None);
        // A page over https does not load plain http assets.
        if document.scheme() == "https" && url.scheme() != "https" {
            return None;
        }
        own(&url).then_some(url)
    };

    let icon = match &discovered.icon {
        Some(href) => resolve(href),
        None => document.join("/favicon.ico").ok(),
    }
    .map(|url| Asset {
        kind: RequestKind::Image,
        url,
    });
    let room = max.saturating_sub(usize::from(icon.is_some()));
    let mut seen = HashSet::new();
    let mut assets = Vec::new();
    for (kind, href) in &discovered.assets {
        if assets.len() >= room {
            break;
        }
        let Some(url) = resolve(href) else {
            continue;
        };
        if Some(&url) == icon.as_ref().map(|icon| &icon.url) || !seen.insert(url.clone()) {
            continue;
        }
        assets.push(Asset { kind: *kind, url });
    }
    (assets, if max == 0 { None } else { icon })
}

#[derive(Debug, Clone)]
struct CacheEntry {
    etag: Option<HeaderValue>,
    last_modified: Option<HeaderValue>,
    /// `None`: stored, but to be revalidated every time.
    fresh_until: Option<DateTime<Utc>>,
    stored_at: DateTime<Utc>,
}

/// What the client remembers about assets it has fetched: their validators
/// and how long they stay fresh. Never a body.
#[derive(Debug, Default)]
pub struct AssetCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
}

/// What to do about an asset, given the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CacheDecision {
    /// Still fresh: a browser would not ask.
    Fresh,
    /// Ask, with these validators.
    Revalidate(Vec<(HeaderName, HeaderValue)>),
    /// Never seen, or not cacheable: ask plainly.
    Fetch,
}

fn cache_directives(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(CACHE_CONTROL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|directive| directive.trim().to_ascii_lowercase())
        .filter(|directive| !directive.is_empty())
        .collect()
}

impl AssetCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, CacheEntry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn decide(&self, url: &Url, now: DateTime<Utc>) -> CacheDecision {
        let entries = self.lock();
        let Some(entry) = entries.get(url.as_str()) else {
            return CacheDecision::Fetch;
        };
        if entry.fresh_until.is_some_and(|until| until > now) {
            return CacheDecision::Fresh;
        }
        let mut validators = Vec::new();
        if let Some(etag) = &entry.etag {
            validators.push((IF_NONE_MATCH, etag.clone()));
        }
        if let Some(last_modified) = &entry.last_modified {
            validators.push((IF_MODIFIED_SINCE, last_modified.clone()));
        }
        if validators.is_empty() {
            CacheDecision::Fetch
        } else {
            CacheDecision::Revalidate(validators)
        }
    }

    /// Records what a `200` or `304` response says about an asset.
    fn record(&self, url: &Url, status: StatusCode, headers: &HeaderMap, now: DateTime<Utc>) {
        let directives = cache_directives(headers);
        let has = |name: &str| directives.iter().any(|directive| directive == name);
        let mut entries = self.lock();
        if has("no-store") {
            entries.remove(url.as_str());
            return;
        }
        let previous = entries.get(url.as_str()).cloned();
        let (etag, last_modified) = if status == StatusCode::NOT_MODIFIED {
            // A 304 may repeat the validators or leave them out.
            (
                headers
                    .get(ETAG)
                    .cloned()
                    .or_else(|| previous.as_ref().and_then(|entry| entry.etag.clone())),
                headers.get(LAST_MODIFIED).cloned().or_else(|| {
                    previous
                        .as_ref()
                        .and_then(|entry| entry.last_modified.clone())
                }),
            )
        } else {
            (
                headers.get(ETAG).cloned(),
                headers.get(LAST_MODIFIED).cloned(),
            )
        };
        let max_age = directives.iter().find_map(|directive| {
            directive
                .strip_prefix("max-age=")
                .and_then(|seconds| seconds.trim_matches('"').parse::<u64>().ok())
        });
        let date = |name: HeaderName| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_http_date)
        };
        let lifetime = if has("no-cache") {
            None
        } else if has("immutable") {
            Some(MAX_FRESH)
        } else if let Some(seconds) = max_age {
            Some(Duration::from_secs(seconds).min(MAX_FRESH))
        } else if let Some(expires) = date(EXPIRES) {
            Some(
                (expires - now)
                    .to_std()
                    .unwrap_or(Duration::ZERO)
                    .min(MAX_FRESH),
            )
        } else {
            // No expiry given: browsers take a tenth of the asset's age.
            date(LAST_MODIFIED)
                .and_then(|modified| (now - modified).to_std().ok())
                .map(|age| (age / 10).min(MAX_HEURISTIC_FRESH))
        };
        let fresh_until = lifetime
            .filter(|lifetime| !lifetime.is_zero())
            .and_then(|lifetime| chrono::Duration::from_std(lifetime).ok())
            .and_then(|lifetime| now.checked_add_signed(lifetime));
        if fresh_until.is_none() && etag.is_none() && last_modified.is_none() {
            // Nothing to be fresh by and nothing to revalidate with.
            entries.remove(url.as_str());
            return;
        }
        if entries.len() >= CACHE_ENTRIES
            && !entries.contains_key(url.as_str())
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.stored_at)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(
            url.as_str().to_owned(),
            CacheEntry {
                etag,
                last_modified,
                fresh_until,
                stored_at: now,
            },
        );
    }

    /// How many assets the cache remembers.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the cache remembers nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What became of one asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetRecord {
    /// The asset.
    pub url: Url,
    /// `style`, `script` or `image`.
    pub kind: &'static str,
    /// The status of the response; `None` when no request was sent or it
    /// failed.
    pub status: Option<u16>,
    /// Body bytes read and dropped.
    pub bytes: u64,
    /// Whether the asset was still fresh in the cache and not asked for.
    pub from_cache: bool,
    /// Whether the request carried validators.
    pub revalidated: bool,
    /// Why the asset was not fetched to its end.
    pub error: Option<String>,
}

/// What subresource emulation did for one page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubresourceReport {
    /// The seed of the pauses.
    pub seed: Seed,
    /// Every asset considered, in the order they were started.
    pub assets: Vec<AssetRecord>,
    /// Requests actually sent, redirects included.
    pub requests: u32,
    /// Body bytes read for all assets together.
    pub bytes: u64,
    /// From the end of the page's body to the end of the last asset.
    pub duration_ms: u64,
}

fn is_html(fetched: &Fetched) -> bool {
    fetched.header("content-type").is_some_and(|value| {
        let media = value.split(';').next().unwrap_or_default().trim();
        media.eq_ignore_ascii_case("text/html")
            || media.eq_ignore_ascii_case("application/xhtml+xml")
    })
}

struct AssetRun<'a> {
    client: &'a HttpClient,
    document: &'a Url,
    guard: Guard,
    budget: &'a AtomicUsize,
    requests: &'a AtomicUsize,
    deadline: Instant,
}

impl AssetRun<'_> {
    async fn fetch(&self, asset: Asset, start_at: Instant) -> AssetRecord {
        tokio::time::sleep_until(start_at).await;
        let mut record = AssetRecord {
            url: asset.url.clone(),
            kind: asset.kind.dest(),
            status: None,
            bytes: 0,
            from_cache: false,
            revalidated: false,
            error: None,
        };
        let conditional = match self.client.assets.decide(&asset.url, Utc::now()) {
            CacheDecision::Fresh => {
                record.from_cache = true;
                return record;
            }
            CacheDecision::Revalidate(validators) => {
                record.revalidated = true;
                validators
            }
            CacheDecision::Fetch => Vec::new(),
        };
        if let Err(error) = self.request(&asset, &conditional, &mut record).await {
            record.error = Some(error);
        }
        record
    }

    async fn request(
        &self,
        asset: &Asset,
        conditional: &[(HeaderName, HeaderValue)],
        record: &mut AssetRecord,
    ) -> Result<(), String> {
        let started = Instant::now();
        let caller = CallerHeaders::none();
        let mut url = asset.url.clone();
        let mut site = FetchSite::between(self.document, &url);
        for _ in 0..=ASSET_REDIRECTS {
            if let Guard::Target(policy) = self.guard {
                check_url(&url, policy).map_err(|denial| denial.to_string())?;
            }
            let incoming = self
                .client
                .send(Outgoing {
                    kind: asset.kind,
                    method: &Method::GET,
                    url: &url,
                    site,
                    referrer: Some(self.document),
                    body: None,
                    caller: &caller,
                    conditional,
                    started,
                    deadline: self.deadline,
                })
                .await
                .map_err(|error| error.to_string())?;
            self.requests.fetch_add(1, Ordering::Relaxed);
            let status = incoming.response.status();
            let headers = incoming.response.headers().clone();
            record.status = Some(status.as_u16());

            let each = self.client.settings.subresources.max_bytes_each;
            let allowance = each.min(self.budget.load(Ordering::Relaxed));
            let (body, truncated) = read_body(incoming.response, allowance, started, self.deadline)
                .await
                .map_err(|error| error.to_string())?;
            self.budget
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                    Some(left.saturating_sub(body.len()))
                })
                .ok();
            record.bytes += u64::try_from(body.len()).unwrap_or(u64::MAX);
            if truncated {
                return Err("larger than the byte limit".to_owned());
            }

            let location = status
                .is_redirection()
                .then(|| headers.get(LOCATION))
                .flatten()
                .filter(|_| status != StatusCode::NOT_MODIFIED)
                .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
            let Some(location) = location else {
                if status == StatusCode::OK || status == StatusCode::NOT_MODIFIED {
                    self.client
                        .assets
                        .record(&asset.url, status, &headers, Utc::now());
                }
                return Ok(());
            };
            let next = url
                .join(location.trim())
                .map_err(|_| "redirected to something that is not a URL".to_owned())?;
            let stays = match self.client.settings.subresources.scope {
                SubresourceScope::SameSite => site::same_site(self.document, &next),
                SubresourceScope::SameOrigin => site::same_origin(self.document, &next),
            };
            if !stays {
                return Err("redirected to another site".to_owned());
            }
            site = site.after_redirect(Some(self.document), &next);
            url = next;
        }
        Err("too many redirects".to_owned())
    }
}

impl HttpClient {
    /// Fetches a page's own assets the way the first paint of a browser
    /// would. `None` when the response is not a page such emulation applies
    /// to.
    pub(crate) async fn fetch_assets(
        &self,
        page: &Fetched,
        guard: Guard,
        seed: Option<Seed>,
        fetch_deadline: Instant,
    ) -> Option<SubresourceReport> {
        let settings = self.settings.subresources;
        if page.status != 200 || !is_html(page) || settings.max == 0 {
            return None;
        }
        let document = &page.final_url;
        let (assets, icon) = plan(document, &scan(&page.body), settings.scope, settings.max);

        let seed = seed.unwrap_or_else(|| {
            let nanos = Utc::now().timestamp_nanos_opt().unwrap_or_default();
            Seed::derive(
                "http-subresources",
                &[document.as_str().as_bytes(), &nanos.to_be_bytes()],
            )
        });
        let mut pauses = seed.child("pauses").rng();
        let began = Instant::now();
        let deadline = fetch_deadline.min(began + ASSETS_TIMEOUT);
        let budget = AtomicUsize::new(settings.max_bytes_total);
        let requests = AtomicUsize::new(0);
        let run = AssetRun {
            client: self,
            document,
            guard,
            budget: &budget,
            requests: &requests,
            deadline,
        };

        // The parser reaches the head's assets a few milliseconds apart.
        let mut start_at = began + Duration::from_secs_f64(pauses.uniform(0.005, 0.040));
        let mut starts = Vec::with_capacity(assets.len());
        for _ in &assets {
            starts.push(start_at);
            start_at += Duration::from_secs_f64(pauses.uniform(0.0, 0.006));
        }
        let mut records: Vec<AssetRecord> = futures_util::stream::iter(
            assets
                .into_iter()
                .zip(starts)
                .map(|(asset, start_at)| run.fetch(asset, start_at)),
        )
        .buffered(settings.max_concurrent.max(1))
        .collect()
        .await;

        if let Some(icon) = icon {
            let pause = Duration::from_secs_f64(pauses.uniform(0.100, 0.600));
            records.push(run.fetch(icon, Instant::now() + pause).await);
        }
        let bytes = records.iter().map(|record| record.bytes).sum();
        Some(SubresourceReport {
            seed,
            assets: records,
            requests: u32::try_from(requests.load(Ordering::Relaxed)).unwrap_or(u32::MAX),
            bytes,
            duration_ms: u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX),
        })
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Duration, TimeZone, Utc};
    use http::StatusCode;
    use http::header::{HeaderMap, HeaderName, HeaderValue};
    use kage_core::config::SubresourceScope;
    use url::Url;

    use super::{Asset, AssetCache, CacheDecision, Discovered, plan, scan};
    use crate::headers::RequestKind;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap()
    }

    fn style(href: &str) -> (RequestKind, String) {
        (RequestKind::Style, href.to_owned())
    }

    fn script(src: &str) -> (RequestKind, String) {
        (RequestKind::Script, src.to_owned())
    }

    #[test]
    fn a_page_names_its_stylesheets_scripts_and_icon_in_document_order() {
        let found = scan(
            br#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Shop</title>
  <link rel="preconnect" href="https://fonts.example.net">
  <link rel="stylesheet" href="/css/main.css?v=3&amp;theme=dark">
  <link href='/css/print.css' rel='stylesheet' media='print'>
  <LINK REL=StyleSheet HREF=/css/unquoted.css>
  <link rel="preload" as="script" href="/js/early.js">
  <link rel="preload" as="font" href="/fonts/a.woff2" crossorigin>
  <link rel="preload" as="style" href="/css/late.css">
  <link rel="alternate stylesheet" href="/css/alt.css" title="High contrast">
  <link rel="stylesheet" href="/css/off.css" disabled>
  <link rel="shortcut icon" href="/static/favicon.png">
  <link rel="icon" href="/static/second.ico">
  <link rel="canonical" href="https://www.example.org/shop">
  <script src="/js/app.js" defer></script>
  <script type="module" src="/js/module.mjs"></script>
  <script nomodule src="/js/legacy.js"></script>
  <script type="application/ld+json">{"@type": "Store"}</script>
  <script type="text/template" src="/tpl/row.html"></script>
  <script>var inline = 1;</script>
</head>
<body>
  <img src="/img/hero.jpg">
  <script src="/js/body.js"></script>
</body>
</html>"#,
        );

        assert_eq!(found.base, None);
        assert_eq!(
            found.assets,
            [
                style("/css/main.css?v=3&theme=dark"),
                style("/css/print.css"),
                style("/css/unquoted.css"),
                script("/js/early.js"),
                style("/css/late.css"),
                script("/js/app.js"),
                script("/js/module.mjs"),
                script("/js/body.js"),
            ]
        );
        assert_eq!(found.icon.as_deref(), Some("/static/favicon.png"));
    }

    #[test]
    fn markup_inside_comments_scripts_and_inert_elements_is_not_markup() {
        let found = scan(
            br#"<head>
<!-- <link rel="stylesheet" href="/commented.css"> -->
<script>document.write('<link rel="stylesheet" href="/written.css">');
  var s = "<script src='/nested.js'><\/script>";</script>
<style>/* <link rel="stylesheet" href="/in-style.css"> */</style>
<noscript><link rel="stylesheet" href="/noscript.css"></noscript>
<template><script src="/template.js"></script></template>
<textarea><script src="/textarea.js"></script></textarea>
<title><link rel="stylesheet" href="/title.css"></title>
<link rel="stylesheet" href="/real.css">
</head>"#,
        );

        assert_eq!(found.assets, [style("/real.css")]);
    }

    #[test]
    fn malformed_markup_is_survived() {
        for html in [
            &b""[..],
            b"<",
            b"<<<<",
            b"<link",
            b"<link rel=stylesheet href=",
            b"<link rel=\"stylesheet\" href=\"/open",
            b"<script src=/a.js",
            b"<!-- never closed <link rel=stylesheet href=/x.css>",
            b"<script>never closed <link rel=stylesheet href=/x.css>",
            b"<link = = = >",
            b"<link rel=stylesheet href>",
            b"<link rel=stylesheet href=''>",
            b"\xff\xfe<\x00l\x00i\x00n\x00k\x00",
            b"< link rel=stylesheet href=/spaced.css>",
        ] {
            let found = scan(html);
            assert!(
                found.assets.len() <= 1,
                "{:?}: {found:?}",
                String::from_utf8_lossy(html)
            );
        }
        // Unclosed quotes and tags still give up what was complete before.
        let found = scan(b"<link rel=stylesheet href=/first.css><script src=\"/open");
        assert_eq!(found.assets[0], style("/first.css"));
        // Attributes in any case, with odd spacing and self-closing slashes.
        let found =
            scan(b"<LINK\n  HREF = \"/a.css\"\n  REL = \"STYLESHEET\" />\n<base href=\"/sub/\"/>");
        assert_eq!(found.assets, [style("/a.css")]);
        assert_eq!(found.base.as_deref(), Some("/sub/"));
    }

    #[test]
    fn only_the_first_base_and_the_first_icon_count() {
        let found = scan(
            b"<base href='https://cdn.example.org/a/'><base href='/b/'>\
              <link rel=icon href=one.ico><link rel='icon' href=two.ico>",
        );

        assert_eq!(found.base.as_deref(), Some("https://cdn.example.org/a/"));
        assert_eq!(found.icon.as_deref(), Some("one.ico"));
    }

    fn planned(
        document: &str,
        discovered: &Discovered,
        scope: SubresourceScope,
        max: usize,
    ) -> (Vec<String>, Option<String>) {
        let (assets, icon) = plan(&url(document), discovered, scope, max);
        (
            assets.iter().map(|asset| asset.url.to_string()).collect(),
            icon.map(|icon| icon.url.to_string()),
        )
    }

    #[test]
    fn only_first_party_assets_are_planned() {
        let discovered = Discovered {
            base: None,
            assets: vec![
                style("/css/main.css#section"),
                style("css/relative.css"),
                script("//static.example.org/js/app.js"),
                script("https://cdn.example.net/lib.js"),
                script("https://www.googletagmanager.com/gtm.js?id=X"),
                style("/css/main.css"),
                script("data:text/javascript,alert(1)"),
                script("javascript:void(0)"),
                script("http://www.example.org/js/insecure.js"),
                script("#"),
                script(""),
            ],
            icon: None,
        };

        let (assets, icon) = planned(
            "https://www.example.org/shop/list?page=2",
            &discovered,
            SubresourceScope::SameSite,
            12,
        );
        assert_eq!(
            assets,
            [
                "https://www.example.org/css/main.css",
                "https://www.example.org/shop/css/relative.css",
                "https://static.example.org/js/app.js",
            ]
        );
        // No icon named: a browser asks for /favicon.ico.
        assert_eq!(icon.as_deref(), Some("https://www.example.org/favicon.ico"));

        // Same origin only: the sibling host goes too.
        let (assets, _) = planned(
            "https://www.example.org/shop/list",
            &discovered,
            SubresourceScope::SameOrigin,
            12,
        );
        assert_eq!(
            assets,
            [
                "https://www.example.org/css/main.css",
                "https://www.example.org/shop/css/relative.css",
            ]
        );
    }

    #[test]
    fn the_limit_counts_the_icon_and_a_base_moves_relative_urls() {
        let discovered = Discovered {
            base: Some("/assets/v2/".to_owned()),
            assets: (0..20)
                .map(|index| script(&format!("s{index}.js")))
                .collect(),
            icon: Some("icon.png".to_owned()),
        };

        let (assets, icon) = planned(
            "https://www.example.org/",
            &discovered,
            SubresourceScope::SameSite,
            4,
        );
        assert_eq!(
            assets,
            [
                "https://www.example.org/assets/v2/s0.js",
                "https://www.example.org/assets/v2/s1.js",
                "https://www.example.org/assets/v2/s2.js",
            ]
        );
        assert_eq!(
            icon.as_deref(),
            Some("https://www.example.org/assets/v2/icon.png")
        );

        let (assets, icon) = planned(
            "https://www.example.org/",
            &discovered,
            SubresourceScope::SameSite,
            0,
        );
        assert_eq!(assets, Vec::<String>::new());
        assert_eq!(icon, None);

        // An icon on another site is not fetched, and nothing replaces it.
        let foreign = Discovered {
            base: None,
            assets: Vec::new(),
            icon: Some("https://cdn.example.net/icon.png".to_owned()),
        };
        let (_, icon) = planned(
            "https://www.example.org/",
            &foreign,
            SubresourceScope::SameSite,
            12,
        );
        assert_eq!(icon, None);

        // A base on another site makes every relative asset foreign.
        let moved = Discovered {
            base: Some("https://cdn.example.net/".to_owned()),
            assets: vec![script("app.js"), script("https://www.example.org/own.js")],
            icon: None,
        };
        let (assets, _) = planned(
            "https://www.example.org/",
            &moved,
            SubresourceScope::SameSite,
            12,
        );
        assert_eq!(assets, ["https://www.example.org/own.js"]);

        let plain = plan(
            &url("http://site.kage.test/"),
            &Discovered {
                base: None,
                assets: vec![style("/a.css")],
                icon: None,
            },
            SubresourceScope::SameSite,
            12,
        );
        assert_eq!(
            plain.0,
            [Asset {
                kind: RequestKind::Style,
                url: url("http://site.kage.test/a.css")
            }]
        );
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn a_fresh_asset_is_not_asked_for_again_and_a_stale_one_is_revalidated() {
        let cache = AssetCache::default();
        let asset = url("https://www.example.org/css/main.css");
        assert_eq!(cache.decide(&asset, now()), CacheDecision::Fetch);
        assert!(cache.is_empty());

        cache.record(
            &asset,
            StatusCode::OK,
            &headers(&[
                ("cache-control", "public, max-age=600"),
                ("etag", "\"v1\""),
                ("last-modified", "Mon, 05 Oct 2026 08:00:00 GMT"),
            ]),
            now(),
        );
        assert_eq!(cache.len(), 1);
        assert_eq!(
            cache.decide(&asset, now() + Duration::seconds(599)),
            CacheDecision::Fresh
        );

        let stale = cache.decide(&asset, now() + Duration::seconds(600));
        let CacheDecision::Revalidate(validators) = stale else {
            panic!("a stale asset with validators is revalidated, got {stale:?}");
        };
        assert_eq!(
            validators,
            [
                (
                    http::header::IF_NONE_MATCH,
                    HeaderValue::from_static("\"v1\"")
                ),
                (
                    http::header::IF_MODIFIED_SINCE,
                    HeaderValue::from_static("Mon, 05 Oct 2026 08:00:00 GMT")
                ),
            ]
        );

        // A 304 without validators keeps the old ones and renews freshness.
        let later = now() + Duration::seconds(700);
        cache.record(
            &asset,
            StatusCode::NOT_MODIFIED,
            &headers(&[("cache-control", "max-age=60")]),
            later,
        );
        assert_eq!(
            cache.decide(&asset, later + Duration::seconds(30)),
            CacheDecision::Fresh
        );
        assert!(matches!(
            cache.decide(&asset, later + Duration::seconds(61)),
            CacheDecision::Revalidate(validators) if validators.len() == 2
        ));
    }

    #[test]
    fn freshness_follows_the_response_headers_the_way_a_browser_reads_them() {
        let asset = url("https://www.example.org/js/app.js");
        let fresh_for = |pairs: &[(&str, &str)]| -> Option<i64> {
            let cache = AssetCache::default();
            cache.record(&asset, StatusCode::OK, &headers(pairs), now());
            let entries = cache.lock();
            entries
                .get(asset.as_str())
                .and_then(|entry| entry.fresh_until)
                .map(|until| (until - now()).num_seconds())
        };

        assert_eq!(fresh_for(&[("cache-control", "max-age=3600")]), Some(3600));
        assert_eq!(
            fresh_for(&[("cache-control", "public, immutable")]),
            Some(365 * 24 * 3600)
        );
        assert_eq!(
            fresh_for(&[("cache-control", "max-age=99999999999")]),
            Some(365 * 24 * 3600)
        );
        assert_eq!(
            fresh_for(&[("expires", "Fri, 09 Oct 2026 13:00:00 GMT")]),
            Some(3600)
        );
        // max-age wins over Expires.
        assert_eq!(
            fresh_for(&[
                ("cache-control", "max-age=60"),
                ("expires", "Fri, 09 Oct 2026 13:00:00 GMT")
            ]),
            Some(60)
        );
        // No expiry: a tenth of the age, at most a day.
        assert_eq!(
            fresh_for(&[("last-modified", "Fri, 09 Oct 2026 02:00:00 GMT")]),
            Some(3600)
        );
        assert_eq!(
            fresh_for(&[("last-modified", "Fri, 09 Oct 2020 02:00:00 GMT")]),
            Some(24 * 3600)
        );
        // To be revalidated every time, but remembered.
        assert_eq!(
            fresh_for(&[("cache-control", "no-cache"), ("etag", "\"a\"")]),
            None
        );
        assert_eq!(
            fresh_for(&[("cache-control", "max-age=0"), ("etag", "\"a\"")]),
            None
        );
        assert_eq!(
            fresh_for(&[
                ("expires", "Thu, 01 Jan 1970 00:00:00 GMT"),
                ("etag", "\"a\"")
            ]),
            None
        );
    }

    #[test]
    fn what_cannot_be_reused_is_not_remembered() {
        let cache = AssetCache::default();
        let asset = url("https://www.example.org/js/app.js");

        // Nothing to be fresh by and nothing to revalidate with.
        cache.record(&asset, StatusCode::OK, &headers(&[]), now());
        assert!(cache.is_empty());
        cache.record(
            &asset,
            StatusCode::OK,
            &headers(&[("cache-control", "no-cache")]),
            now(),
        );
        assert!(cache.is_empty());

        cache.record(
            &asset,
            StatusCode::OK,
            &headers(&[("etag", "\"a\"")]),
            now(),
        );
        assert_eq!(cache.len(), 1);
        assert!(matches!(
            cache.decide(&asset, now()),
            CacheDecision::Revalidate(_)
        ));
        // no-store forgets what was there.
        cache.record(
            &asset,
            StatusCode::OK,
            &headers(&[("cache-control", "no-store"), ("etag", "\"b\"")]),
            now(),
        );
        assert!(cache.is_empty());
        assert_eq!(cache.decide(&asset, now()), CacheDecision::Fetch);
    }

    #[test]
    fn the_cache_forgets_its_oldest_entry_when_it_is_full() {
        let cache = AssetCache::default();
        let fresh = headers(&[("cache-control", "max-age=600")]);
        for index in 0..super::CACHE_ENTRIES {
            cache.record(
                &url(&format!("https://www.example.org/a{index}.js")),
                StatusCode::OK,
                &fresh,
                now() + Duration::seconds(i64::try_from(index).unwrap()),
            );
        }
        assert_eq!(cache.len(), super::CACHE_ENTRIES);

        let later = now() + Duration::hours(1);
        cache.record(
            &url("https://www.example.org/new.js"),
            StatusCode::OK,
            &fresh,
            later,
        );

        assert_eq!(cache.len(), super::CACHE_ENTRIES);
        assert_eq!(
            cache.decide(&url("https://www.example.org/a0.js"), now()),
            CacheDecision::Fetch
        );
        assert_eq!(
            cache.decide(&url("https://www.example.org/new.js"), later),
            CacheDecision::Fresh
        );
    }
}
