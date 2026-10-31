//! A crawl as a job (spec §13, §16.6): what `POST /v1/jobs` takes for one,
//! what it is checked for, what a crawl keeps about itself while it is
//! worked, and what it shows.
//!
//! A crawl reads a site the way a reader does. It starts at its seeds; from
//! then on its next page is a link of the page that is open, else of a page
//! a few steps back in the tab, and only when the tab holds nothing more an
//! address is typed: the oldest that waits, of the least depth. One persona
//! reads one site (its registrable domain) unless the caller asks for more
//! `visitors`, and every page goes through the scheduler like any other
//! fetch: pacing, visits, waking hours, the breaker and the cooldowns are
//! the same.
//!
//! The pages are the job's items. A crawl has no items when it is
//! accepted: an item is added for every page it asks a site for, so
//! `max_pages` is what the job can grow to.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use kage_core::config::{MAX_CRAWL_DEPTH, MAX_CRAWL_SEEDS};
use kage_core::domain::Level;
use kage_core::ids::PersonaId;
use kage_core::urlnorm::{NormalizeOptions, normalize};
use kage_crawl::frontier::{Counts, Limits};
use kage_crawl::scope::{Scope, ScopeError, ScopeRequest};
use kage_crawl::site;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use url::Url;
use utoipa::ToSchema;

use super::request::{Checks, WebhookField};
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::fetch::request::{FetchRequest, Planned, is_credential, level_of, plan, url_refused};
use crate::fetch::terms::{CacheUse, settle};
use crate::resolve::guard;
use crate::schema::FreeForm;

/// What a page of a crawl never is: these are the page's own, or the
/// job's, or make of a page something a reader's browser does not ask for.
const NOT_A_DEFAULT: [(&str, &str); 5] = [
    ("url", "the address of a page is the crawl's to find"),
    (
        "session_id",
        "a session is the job's: name it beside the seeds",
    ),
    ("method", "a crawl reads pages: every one is a GET"),
    ("body", "a crawl reads pages: none is sent a body"),
    (
        "request_kind",
        "a crawl reads pages: every one is a navigation",
    ),
];

/// A crawl: a site read from its seeds along its links, the way a reader
/// goes through it.
///
/// One persona reads one site. Its first page is typed; every later page
/// is reached by a click on a link of the page that is open or of one a few
/// steps back in its tab, and only when the tab holds nothing more an
/// address is typed. It rests between pages, takes its breaks and sleeps
/// like every other visit, so a crawl takes the time a reader takes: under
/// the `human` pacing about an hour for 50 pages of one site. `visitors`
/// and a faster `pacing` in `defaults` trade that for speed, as in a batch.
///
/// Every page the crawl asks for is an item of the job, in the order it was
/// asked for, with its depth, where its link was found, how it was reached
/// and what it repeats.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(example = json!({
    "type": "crawl",
    "seeds": ["https://example.org/"],
    "deny": ["/login", "\\?sort="],
    "max_depth": 3,
    "max_pages": 50,
    "defaults": { "level": 3, "include": ["dom", "headers"] }
}))]
pub(crate) struct CrawlJob {
    /// Where the crawl starts: 1 to `crawl.max_seeds` (100) addresses. A
    /// seed is read whatever `allow` says; one that `deny` matches is
    /// refused.
    #[schema(min_items = 1, example = json!(["https://example.org/"]))]
    pub(crate) seeds: Vec<String>,
    /// Regular expressions on the normalized address (spec §13). With any,
    /// a page is read only when one of them matches. At most 32, each at
    /// most 512 bytes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schema(max_items = 32, example = json!(["^https://example\\.org/articles/"]))]
    pub(crate) allow: Vec<String>,
    /// Regular expressions on the normalized address: a page one of them
    /// matches is never read, whatever `allow` says.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schema(max_items = 32, example = json!(["/login"]))]
    pub(crate) deny: Vec<String>,
    /// How many links from a seed a page may be, by the shortest way the
    /// crawl finds: `crawl.default_max_depth` (3) unless said, at most 32.
    /// 0 reads the seeds only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(maximum = 32, example = 3)]
    pub(crate) max_depth: Option<u32>,
    /// How many pages the crawl asks the sites for at most, a page that
    /// failed included: `crawl.default_max_pages` (100, or `jobs.max_items`
    /// where that is fewer) unless said, at most `jobs.max_items`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, example = 100)]
    pub(crate) max_pages: Option<u32>,
    /// Whether the crawl stays on the sites of its seeds (their registrable
    /// domains); true unless said. With false it follows a link to any
    /// site, each with a visitor of its own, inside the same limits; every
    /// address passes the guard against private networks before it is
    /// asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) same_site_only: Option<bool>,
    /// Whether the sitemaps of the seeds' sites are read first and the
    /// pages they name become places to start from; false unless said. A
    /// reader does not ask for `/sitemap.xml`: a site that watches for the
    /// request sees it. It is therefore made by another persona than the
    /// one that reads the site (a scout), and never when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) use_sitemaps: Option<bool>,
    /// What every page is fetched with: any field of a fetch request but
    /// `url`, `session_id`, `method`, `body` and `request_kind`. Its
    /// `level` is required (`level_required`). At level 0 the links are
    /// read from the page's markup and every page is typed, with no
    /// `Referer`; a caller's `Cookie` and `Authorization` are not taken in
    /// a job, which would have to keep them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<FreeForm>)]
    pub(crate) defaults: Option<Map<String, Value>>,
    /// How many personas read one site for the crawl at a time; 1 unless
    /// said, at most the `domain_concurrency` of the pacing in force for
    /// the site. More is refused, not reduced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, example = 1)]
    pub(crate) visitors: Option<u32>,
    /// An open session of this key: its persona reads every page, and no
    /// other, with a fetch's rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(max_length = 64, pattern = "^[A-Za-z0-9_.-]{1,64}$")]
    pub(crate) session_id: Option<String>,
    /// Where the job's events are posted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) webhook: Option<WebhookField>,
}

/// A crawl that passed every check.
#[derive(Debug, Clone)]
pub(crate) struct PlannedCrawl {
    /// The seeds as the pipeline takes them, in the order given: what a
    /// session's persona is held against.
    pub(crate) seeds: Vec<Planned>,
    /// What the crawl shows before any of it was worked.
    pub(crate) start: Kept,
}

/// What a crawl is run with: the request, with what it left out filled in
/// from the configuration in force.
#[derive(Debug, Clone)]
pub(crate) struct CrawlRules {
    /// The seeds.
    pub(crate) seeds: Vec<Url>,
    /// Which addresses the crawl reads.
    pub(crate) scope: Scope,
    /// Its limits.
    pub(crate) limits: Limits,
    /// How an address becomes the key the crawl knows a page by.
    pub(crate) normalize: NormalizeOptions,
    /// Whether sitemaps are read.
    pub(crate) use_sitemaps: bool,
    /// How many personas read one site at a time.
    pub(crate) visitors: u32,
}

fn refused(field: &str, error: &ScopeError) -> ApiError {
    ApiError::field(field, error.to_string())
}

/// The field a refusal of the scope is about.
const fn field_of(error: &ScopeError) -> &'static str {
    match error {
        ScopeError::TooManyPatterns { list, .. }
        | ScopeError::PatternTooLong { list, .. }
        | ScopeError::BadPattern { list, .. } => list,
    }
}

/// The complete fetch request of one page of a crawl.
pub(crate) fn page_request(
    defaults: Option<&Map<String, Value>>,
    url: &Url,
    session: Option<&str>,
) -> Value {
    let mut request = defaults.cloned().unwrap_or_default();
    request.insert("url".to_owned(), json!(url.as_str()));
    if let Some(session) = session {
        request.insert("session_id".to_owned(), json!(session));
    }
    Value::Object(request)
}

/// What a crawl is run with under the configuration in force.
///
/// # Errors
///
/// The first thing about the crawl's own fields that is not acceptable.
pub(crate) fn rules(crawl: &CrawlJob, checks: Checks<'_>) -> ApiResult<CrawlRules> {
    let config = &checks.config.crawl;
    let most_seeds = usize::try_from(config.max_seeds.min(MAX_CRAWL_SEEDS)).unwrap_or(usize::MAX);
    if crawl.seeds.is_empty() {
        return Err(ApiError::field(
            "seeds",
            "a crawl starts somewhere: at least one seed",
        ));
    }
    if crawl.seeds.len() > most_seeds {
        return Err(ApiError::field(
            "seeds",
            format!(
                "a crawl has at most {most_seeds} seeds (crawl.max_seeds); this one has {}",
                crawl.seeds.len()
            ),
        )
        .with("maximum", u64::try_from(most_seeds).unwrap_or(u64::MAX)));
    }
    let max_depth = crawl.max_depth.unwrap_or(config.default_max_depth);
    if max_depth > MAX_CRAWL_DEPTH {
        return Err(
            ApiError::field("max_depth", format!("at most {MAX_CRAWL_DEPTH}"))
                .with("maximum", MAX_CRAWL_DEPTH),
        );
    }
    let most_pages = checks.config.jobs.max_items;
    // Where a job may have fewer items than the default, that is the
    // default.
    let max_pages = crawl
        .max_pages
        .unwrap_or_else(|| config.default_max_pages.min(most_pages));
    if max_pages == 0 || max_pages > most_pages {
        return Err(ApiError::field(
            "max_pages",
            format!("between 1 and {most_pages} (jobs.max_items)"),
        )
        .with("maximum", most_pages));
    }
    let visitors = crawl.visitors.unwrap_or(1);
    if visitors == 0 {
        return Err(ApiError::field("visitors", "at least 1"));
    }
    let mut seeds = Vec::with_capacity(crawl.seeds.len());
    for (index, seed) in crawl.seeds.iter().enumerate() {
        let url = Url::parse(seed).map_err(|error| {
            ApiError::new(ErrorCode::InvalidUrl, format!("seeds[{index}]: {error}"))
                .with("field", "seeds")
                .with("item", u64::try_from(index).unwrap_or(u64::MAX))
        })?;
        seeds.push(url);
    }
    let scope = Scope::new(
        &ScopeRequest {
            allow: crawl.allow.clone(),
            deny: crawl.deny.clone(),
            same_site_only: crawl.same_site_only.unwrap_or(true),
            max_depth,
            skipped_extensions: config.skipped_extensions.clone(),
        },
        &seeds,
    )
    .map_err(|error| refused(field_of(&error), &error))?;
    let normalize = checks.config.urls.clone();
    Ok(CrawlRules {
        seeds,
        scope,
        limits: Limits {
            max_pages,
            max_entries: usize::try_from(config.max_frontier).unwrap_or(usize::MAX),
            back_limit: config.back_limit,
            near_duplicate_bits: config.near_duplicate_bits,
            sitemap_depth: 0,
        },
        normalize,
        use_sitemaps: crawl.use_sitemaps.unwrap_or(false),
        visitors: if crawl.session_id.is_some() {
            1
        } else {
            visitors
        },
    })
}

/// Checks a crawl: its own fields, and every seed as the fetch it is.
///
/// # Errors
///
/// The first refusal; one about a seed names its position as `item`.
pub(crate) async fn plan_crawl(crawl: &CrawlJob, checks: Checks<'_>) -> ApiResult<PlannedCrawl> {
    if let Some(defaults) = &crawl.defaults {
        for (field, why) in NOT_A_DEFAULT {
            if defaults.contains_key(field) {
                return Err(ApiError::field(&format!("defaults.{field}"), why));
            }
        }
    }
    check_level(crawl)?;
    plan_seeds(crawl, checks).await
}

/// What a crawl's defaults say about its level (spec §25): one is named,
/// a crawl at level 0 has no visitors, and no caller's credential is kept
/// for its pages.
fn check_level(crawl: &CrawlJob) -> ApiResult<()> {
    let defaults = crawl.defaults.as_ref();
    let named = defaults.and_then(|defaults| defaults.get("level"));
    if named.is_none() {
        return Err(level_of(None, "defaults.level").err().unwrap_or_else(|| {
            ApiError::new(ErrorCode::LevelRequired, "defaults.level is required")
        }));
    }
    if let Some(Value::Object(headers)) = defaults.and_then(|defaults| defaults.get("headers"))
        && let Some(name) = headers.keys().find(|name| is_credential(name))
    {
        return Err(ApiError::new(
            ErrorCode::HeaderDenied,
            format!(
                "the header {name:?} is the caller's own credential, and a job keeps its pages' \
                 requests until they run: Kage holds no credential"
            ),
        )
        .with("header", name.as_str()));
    }
    let plain = named.and_then(Value::as_u64) == Some(u64::from(Level::Plain.number()));
    if plain && crawl.visitors.is_some() {
        return Err(ApiError::field(
            "visitors",
            "visitors are personas, and level 0 has none: leave visitors out",
        ));
    }
    Ok(())
}

/// Checks a crawl and turns its seeds into orders.
async fn plan_seeds(crawl: &CrawlJob, checks: Checks<'_>) -> ApiResult<PlannedCrawl> {
    let rules = rules(crawl, checks)?;
    let at = |error: ApiError, index: usize| {
        error
            .with("item", u64::try_from(index).unwrap_or(u64::MAX))
            .with("seed", crawl.seeds[index].as_str())
    };
    let mut planned = Vec::with_capacity(rules.seeds.len());
    for (index, seed) in rules.seeds.iter().enumerate() {
        let input = page_request(crawl.defaults.as_ref(), seed, crawl.session_id.as_deref());
        let request: FetchRequest = serde_json::from_value(input).map_err(|error| {
            at(
                ApiError::new(ErrorCode::ValidationFailed, error.to_string()),
                index,
            )
        })?;
        let mut one = plan(&request, &checks.policies.pacing).map_err(|error| at(error, index))?;
        // A page of a crawl is a visit every time; what it read fills the
        // cache when its defaults name a lifetime.
        settle(
            &mut one,
            &request,
            checks.config,
            checks.policies,
            checks.key,
            CacheUse::Fill,
        )
        .map_err(|error| at(error, index))?;
        guard(
            &one.order.url,
            checks.config.security.ssrf(),
            checks.resolver,
        )
        .await
        .map_err(|denial| at(url_refused(&denial), index))?;
        let key = normalize(&one.order.url, &rules.normalize);
        if rules.scope.judge_seed(&key).is_err() {
            return Err(at(
                ApiError::field(
                    "seeds",
                    "a pattern of deny matches this seed: a crawl never reads what deny names",
                ),
                index,
            ));
        }
        if site::of(&one.order.url).is_none() {
            return Err(at(
                ApiError::new(ErrorCode::InvalidUrl, "a seed has a host that is a site's"),
                index,
            ));
        }
        // More visitors than the pacing in force for the seed's site
        // allows are refused, as for a batch.
        if rules.visitors > 1 {
            let host = one.order.url.host_str().unwrap_or_default();
            let policy = checks.policies.domains.resolve(host);
            let pacing = policy
                .pacing_for(one.order.pacing.as_ref(), &checks.policies.pacing)
                .map_err(|error| {
                    at(
                        ApiError::new(ErrorCode::InvalidPacing, error.to_string()),
                        index,
                    )
                })?;
            let most = pacing.profile.domain_concurrency;
            if rules.visitors > most {
                return Err(ApiError::field(
                    "visitors",
                    format!(
                        "{host} takes at most {most} visitors at a time under the pacing in \
                         force for it; ask for no more, or for a pacing that allows more where \
                         the policy does not lock it"
                    ),
                )
                .with("maximum", most)
                .with("site", host));
            }
        }
        planned.push(one);
    }
    let mut start = Kept::default();
    start.progress.max_pages = rules.limits.max_pages;
    start.progress.max_depth = rules.scope.max_depth();
    Ok(PlannedCrawl {
        seeds: planned,
        start,
    })
}

// --- what a crawl keeps about itself -------------------------------------

/// One visitor of a site: the persona, once the crawl has one there, and
/// what its tab holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Slot {
    /// The persona that reads the site in this place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) persona: Option<PersonaId>,
    /// The positions of the pages it read in this sitting, the open one
    /// last.
    #[serde(default)]
    pub(crate) tab: Vec<u64>,
}

/// A page that is being read: who reads it and how it is reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Flight {
    /// The visitor's place among the visitors of the site.
    pub(crate) slot: usize,
    /// The position of the page whose link is followed; `None` for a
    /// typed address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) from: Option<u64>,
}

/// How reading one sitemap file went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub(crate) struct SitemapView {
    /// The site it belongs to.
    #[schema(example = "example.org")]
    pub(crate) site: String,
    /// The file.
    #[schema(example = "https://example.org/sitemap.xml")]
    pub(crate) url: String,
    /// `waiting`, `read` (a list of pages), `index` (a list of sitemaps),
    /// `missing` (the site has none there), `sitemap_unreadable` (the
    /// scout's browser cannot show what it is: a gzipped file in Chrome,
    /// or no sitemap at all), `failed` (the request did not get through),
    /// `no_scout` (no persona but the site's reader could have asked: it
    /// was not asked for), or `not_read` (the crawl ended first, or the
    /// site has more files than `crawl.max_sitemaps`).
    #[schema(example = "read")]
    pub(crate) state: String,
    /// The addresses it named that the crawl took in.
    pub(crate) urls: u64,
}

/// How a page of a crawl was reached.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReachedField {
    /// Its address was typed: the first page of a sitting, and a page no
    /// page in the tab links to.
    Typed,
    /// A click on its link on the page that was open.
    Click,
    /// A click on its link after going back to a page read earlier.
    BackClick,
    /// On the http path, which has no tab: the request named the page
    /// that holds the link as its `Referer`, as the click would.
    Referer,
}

impl ReachedField {
    /// Every way, in declaration order.
    pub(crate) const ALL: [Self; 4] = [Self::Typed, Self::Click, Self::BackClick, Self::Referer];

    /// The name a metric and a counter know it by.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Typed => "typed",
            Self::Click => "click",
            Self::BackClick => "back_click",
            Self::Referer => "referer",
        }
    }
}

/// How the pages of a crawl were reached, in numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub(crate) struct ReachedCounts {
    /// Addresses typed.
    #[serde(default)]
    pub(crate) typed: u64,
    /// Links clicked on the open page.
    #[serde(default)]
    pub(crate) click: u64,
    /// Links clicked after going back.
    #[serde(default)]
    pub(crate) back_click: u64,
    /// Pages of the http path that named the page before them.
    #[serde(default)]
    pub(crate) referer: u64,
}

impl ReachedCounts {
    pub(crate) const fn count(&mut self, reached: ReachedField) {
        match reached {
            ReachedField::Typed => self.typed += 1,
            ReachedField::Click => self.click += 1,
            ReachedField::BackClick => self.back_click += 1,
            ReachedField::Referer => self.referer += 1,
        }
    }
}

/// How far a crawl has come (spec §13).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub(crate) struct CrawlView {
    /// Addresses the crawl knows: its seeds, what sitemaps named and what
    /// pages linked to, each once however often and however spelled.
    pub(crate) discovered: u64,
    /// Of them, pages that wait to be read.
    pub(crate) waiting: u64,
    /// Pages being read now.
    pub(crate) fetching: u64,
    /// Pages read.
    pub(crate) fetched: u64,
    /// Pages that could not be read: the fetch failed, or the site
    /// answered with an error status (its item is then a fetch that
    /// succeeded, with the status in its result).
    pub(crate) failed: u64,
    /// Addresses the crawl knows and does not read, by reason: `off_site`,
    /// `too_deep`, `denied`, `not_allowed`, `not_a_page` (a file),
    /// `hidden` (no reader can see the link), `already_read` (another
    /// address redirected there), `ssrf_denied`, `robots_denied` (the
    /// site's `robots.txt` disallows it and its policy keeps to the file).
    #[schema(value_type = FreeForm, example = json!({ "too_deep": 12, "hidden": 1 }))]
    pub(crate) skipped: BTreeMap<String, u64>,
    /// Addresses found after the crawl knew `crawl.max_frontier` of them:
    /// counted and forgotten.
    pub(crate) dropped: u64,
    /// Pages that say word for word what a page read before them says.
    pub(crate) duplicates: u64,
    /// Pages that say nearly what a page read before them says.
    pub(crate) near_duplicates: u64,
    /// The depth of the deepest page read.
    pub(crate) depth_reached: u32,
    /// Pages asked of the sites so far: what `max_pages` counts.
    pub(crate) pages: u32,
    /// The most pages the crawl asks for.
    pub(crate) max_pages: u32,
    /// The furthest a page may be from a seed.
    pub(crate) max_depth: u32,
    /// How the pages were reached.
    pub(crate) reached: ReachedCounts,
    /// The sitemaps of a crawl with `use_sitemaps`.
    pub(crate) sitemaps: Vec<SitemapView>,
    /// Why the crawl stopped reading: `completed` (nothing inside its
    /// scope is left), `max_pages`, `cancelled` or `expired`; null while it
    /// goes on.
    #[schema(example = "completed")]
    pub(crate) ended: Option<String>,
}

impl CrawlView {
    /// Takes the counters of the frontier as it stands.
    pub(crate) fn count(&mut self, counts: &Counts) {
        self.discovered = counts.discovered;
        self.waiting = counts.waiting;
        self.fetching = counts.reading;
        self.fetched = counts.read;
        self.failed = counts.failed;
        self.skipped = counts
            .skipped
            .iter()
            .map(|(reason, count)| ((*reason).to_owned(), *count))
            .collect();
        self.dropped = counts.dropped;
        self.duplicates = counts.duplicates;
        self.near_duplicates = counts.near_duplicates;
        self.depth_reached = counts.depth_reached;
        self.pages = counts.begun;
    }
}

/// What a crawl keeps about itself besides its frontier, as it is stored
/// with the job. Only `progress` is ever shown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Kept {
    /// What the job shows.
    #[serde(default)]
    pub(crate) progress: CrawlView,
    /// The visitors of each site and their tabs.
    #[serde(default)]
    pub(crate) slots: BTreeMap<String, Vec<Slot>>,
    /// The pages being read, by position.
    #[serde(default)]
    pub(crate) flights: BTreeMap<u64, Flight>,
    /// The personas that asked for a sitemap, by site, the last one last:
    /// none of them ever reads a page of the crawl.
    #[serde(default)]
    pub(crate) scouts: BTreeMap<String, Vec<PersonaId>>,
    /// Whether the seeds were put into the frontier.
    #[serde(default)]
    pub(crate) seeded: bool,
}

impl Kept {
    /// What a stored job says, or nothing yet.
    pub(crate) fn read(stored: Value) -> Self {
        serde_json::from_value(stored).unwrap_or_default()
    }

    /// The part of a stored crawl that its key sees.
    pub(crate) fn view(stored: Value) -> Option<CrawlView> {
        match stored {
            Value::Null => None,
            stored => Some(Self::read(stored).progress),
        }
    }
}

// --- what an item of a crawl says ----------------------------------------

/// The page a page nearly repeats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub(crate) struct NearDuplicateView {
    /// The page read before it that says nearly the same.
    pub(crate) url: String,
    /// In how many of the 64 bits of their fingerprints the two differ.
    pub(crate) bits: u32,
}

/// The links of a page as the crawl took them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub(crate) struct LinksView {
    /// Links a reader can follow, each address once.
    pub(crate) found: u32,
    /// Of those, the addresses the crawl did not know before this page.
    pub(crate) new: u32,
}

/// How the scrolling of a page ended (spec §10.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub(crate) struct ScrollView {
    /// The patience the persona had with this page, in milliseconds: how
    /// long it would have gone on scrolling at most.
    pub(crate) patience_ms: Option<u64>,
    /// How far down the document it came, from 0 to 1.
    pub(crate) reached: Option<f64>,
    /// `depth` (as far as it meant to go), `patience` (its patience ended
    /// first) or `page` (the page does not go further).
    #[schema(example = "depth")]
    pub(crate) ended_by: Option<String>,
}

/// What a page of a crawl was to the crawl.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub(crate) struct CrawlItemView {
    /// The least number of links from a seed the crawl knew when it asked
    /// for the page.
    pub(crate) depth: u32,
    /// How the crawl came to know the address: `seed`, `sitemap` or `link`.
    #[schema(example = "link")]
    pub(crate) found_by: String,
    /// The page its link was first seen on.
    pub(crate) found_on: Option<String>,
    /// How it was reached; null when it was not.
    pub(crate) reached: Option<ReachedField>,
    /// How many pages back in its tab the persona went before the click.
    pub(crate) backs: Option<u32>,
    /// Where the page ended when a redirect led elsewhere.
    pub(crate) final_url: Option<String>,
    /// The SHA-256 of what the page says, without what the site repeats on
    /// every page, as hexadecimal.
    pub(crate) content_hash: Option<String>,
    /// The page read before it that says the same word for word. Its own
    /// links were not followed from here: they are that page's.
    pub(crate) duplicate_of: Option<String>,
    /// The page read before it that says nearly the same.
    pub(crate) near_duplicate_of: Option<NearDuplicateView>,
    /// What it links to.
    pub(crate) links: Option<LinksView>,
    /// How its scrolling ended, for a page read in a browser.
    pub(crate) scroll: Option<ScrollView>,
    /// When the site's sitemap says the page last changed.
    pub(crate) lastmod: Option<DateTime<Utc>>,
}

impl CrawlItemView {
    /// What a stored item of a crawl says about its page.
    pub(crate) fn of(output: Option<&Value>) -> Option<Self> {
        serde_json::from_value(output?.get("crawl")?.clone()).ok()
    }

    /// As it is stored with the item.
    pub(crate) fn stored(&self) -> Value {
        json!({ "crawl": self })
    }
}

#[cfg(test)]
mod tests {
    use kage_core::config::Config;
    use kage_core::pacing::PacingProfiles;
    use kage_core::policy::{DomainPolicies, PolicySet};
    use serde_json::{Value, json};

    use super::{CrawlItemView, CrawlJob, Kept, ReachedField, plan_crawl, rules};
    use crate::error::{ApiError, ErrorCode};
    use crate::jobs::request::Checks;

    const PACING: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../policies/pacing-profiles.yaml"
    ));
    const SIGNATURES: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../policies/block-signatures.yaml"
    ));

    fn config() -> Config {
        let base = [
            ("KAGE__DATABASE__URL", "postgres://kage@localhost/kage"),
            ("KAGE__SIDECAR__TOKEN", "token"),
            (
                "KAGE__SECRETS__MASTER_KEY",
                "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
            ),
        ];
        Config::load(
            None,
            base.iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
        )
        .unwrap()
    }

    fn policies(domains: &str) -> PolicySet {
        PolicySet {
            domains: DomainPolicies::from_yaml(domains).unwrap(),
            pacing: PacingProfiles::from_yaml(PACING).unwrap(),
            signatures: kage_core::classifier::SignatureSet::from_yaml(SIGNATURES).unwrap(),
        }
    }

    /// A crawl as written, at level 3 where its defaults name none.
    fn crawl(mut body: Value) -> CrawlJob {
        let mut defaults = body.get("defaults").cloned().unwrap_or_else(|| json!({}));
        if defaults.get("level").is_none() {
            defaults["level"] = json!(3);
            body["defaults"] = defaults;
        }
        serde_json::from_value(body).unwrap()
    }

    async fn planned(body: Value) -> Result<usize, ApiError> {
        let config = config();
        let policies = policies("");
        let checks = Checks {
            config: &config,
            policies: &policies,
            resolver: None,
            key: kage_core::ids::ApiKeyId::new(),
        };
        plan_crawl(&crawl(body), checks)
            .await
            .map(|planned| planned.seeds.len())
    }

    /// A member of the problem document of a refusal, as text.
    fn said(error: &ApiError, name: &str) -> String {
        match error.problem().extensions.get(name) {
            Some(Value::String(text)) => text.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        }
    }

    fn field(error: &ApiError) -> String {
        said(error, "field")
    }

    #[tokio::test]
    async fn a_crawl_is_its_seeds_with_what_it_left_out_from_the_configuration() {
        let config = config();
        let policies = policies("");
        let checks = Checks {
            config: &config,
            policies: &policies,
            resolver: None,
            key: kage_core::ids::ApiKeyId::new(),
        };
        let asked = crawl(json!({ "seeds": ["https://example.org/", "https://example.org/b"] }));

        let run = rules(&asked, checks).unwrap();

        assert_eq!(run.seeds.len(), 2);
        assert_eq!((run.limits.max_pages, run.scope.max_depth()), (100, 3));
        assert_eq!(
            (run.limits.back_limit, run.limits.near_duplicate_bits),
            (3, 6)
        );
        assert_eq!(run.limits.max_entries, 50_000);
        assert!(!run.use_sitemaps);
        assert_eq!(run.visitors, 1);
        assert_eq!(plan_crawl(&asked, checks).await.unwrap().seeds.len(), 2);

        // What it says is what it gets; a session reads alone.
        let asked = crawl(json!({
            "seeds": ["https://example.org/"], "max_depth": 0, "max_pages": 7,
            "use_sitemaps": true, "visitors": 3, "session_id": "s1",
            "same_site_only": false,
        }));
        let run = rules(&asked, checks).unwrap();
        assert_eq!((run.limits.max_pages, run.scope.max_depth()), (7, 0));
        assert!(run.use_sitemaps);
        assert_eq!(run.visitors, 1);
        // An unknown member is refused, as everywhere.
        assert!(serde_json::from_value::<CrawlJob>(json!({ "seeds": [], "depth": 3 })).is_err());
    }

    #[tokio::test]
    async fn a_crawl_that_cannot_run_as_asked_is_refused_and_says_what_about() {
        let seeds = json!(["https://example.org/"]);
        let refused = |mut body: Value| async {
            if body.get("seeds").is_none() {
                body["seeds"] = json!(["https://example.org/"]);
            }
            planned(body).await.unwrap_err()
        };

        let none = planned(json!({ "seeds": [] })).await.unwrap_err();
        assert_eq!(field(&none), "seeds");
        let many: Vec<String> = (0..101)
            .map(|n| format!("https://example.org/{n}"))
            .collect();
        let error = planned(json!({ "seeds": many })).await.unwrap_err();
        assert_eq!(field(&error), "seeds");
        assert_eq!(said(&error, "maximum"), "100");

        for (body, about) in [
            (json!({ "max_depth": 33 }), "max_depth"),
            (json!({ "max_pages": 0 }), "max_pages"),
            (json!({ "max_pages": 10_001 }), "max_pages"),
            (json!({ "visitors": 0 }), "visitors"),
            (json!({ "allow": ["("] }), "allow"),
            (json!({ "deny": ["a".repeat(513)] }), "deny"),
            (json!({ "deny": vec!["x"; 33] }), "deny"),
            (
                json!({ "defaults": { "url": "https://example.org/" } }),
                "defaults.url",
            ),
            (
                json!({ "defaults": { "session_id": "s" } }),
                "defaults.session_id",
            ),
            (
                json!({ "defaults": { "method": "POST" } }),
                "defaults.method",
            ),
            (json!({ "defaults": { "body": "x" } }), "defaults.body"),
            (
                json!({ "defaults": { "request_kind": "xhr" } }),
                "defaults.request_kind",
            ),
        ] {
            let error = refused(body.clone()).await;
            assert_eq!(error.code(), ErrorCode::ValidationFailed, "{body}");
            assert_eq!(field(&error), about, "{body}");
        }
        // More visitors than the pacing in force allows.
        let error = refused(json!({ "visitors": 4 })).await;
        assert_eq!(field(&error), "visitors");
        assert_eq!(said(&error, "maximum"), "3");
        assert_eq!(
            planned(json!({ "seeds": seeds, "visitors": 3 }))
                .await
                .unwrap(),
            1
        );

        // A seed is refused for what a fetch is refused for, and says
        // which seed it was.
        let error = planned(json!({ "seeds": ["https://example.org/", "ftp://example.org/x"] }))
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidUrl);
        assert_eq!(said(&error, "item"), "1");
        let error = planned(json!({ "seeds": ["https://example.org/", "not a url"] }))
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidUrl);
        assert_eq!(said(&error, "item"), "1");
        let error = planned(json!({ "seeds": ["http://127.0.0.1/admin"] }))
            .await
            .unwrap_err();
        assert_eq!(error.code(), ErrorCode::SsrfDenied);
        assert_eq!(said(&error, "item"), "0");
        let error = planned(json!({
            "seeds": ["https://example.org/"], "defaults": { "timeout_ms": 5 }
        }))
        .await
        .unwrap_err();
        assert_eq!(said(&error, "item"), "0");
        // What deny names is never read, a seed included; allow does not
        // keep a seed out.
        let error = planned(json!({
            "seeds": ["https://example.org/", "https://example.org/login"], "deny": ["/login"]
        }))
        .await
        .unwrap_err();
        assert_eq!(field(&error), "seeds");
        assert_eq!(said(&error, "item"), "1");
        assert_eq!(said(&error, "seed"), "https://example.org/login");
        assert_eq!(
            planned(json!({ "seeds": ["https://example.org/"], "allow": ["/articles/"] }))
                .await
                .unwrap(),
            1
        );
    }

    #[test]
    fn what_a_crawl_keeps_is_read_back_and_only_its_progress_is_shown() {
        assert_eq!(Kept::read(Value::Null), Kept::default());
        assert_eq!(Kept::view(Value::Null), None);
        let mut kept = Kept::default();
        kept.progress.discovered = 12;
        kept.progress.reached.count(ReachedField::BackClick);
        kept.progress.reached.count(ReachedField::Typed);
        kept.progress.reached.count(ReachedField::Click);
        kept.progress.reached.count(ReachedField::Referer);
        kept.seeded = true;
        let stored = serde_json::to_value(&kept).unwrap();
        assert_eq!(Kept::read(stored.clone()), kept);
        let shown = serde_json::to_value(Kept::view(stored).unwrap()).unwrap();
        assert_eq!(shown["discovered"], 12);
        assert_eq!(
            shown["reached"],
            json!({ "typed": 1, "click": 1, "back_click": 1, "referer": 1 })
        );
        // Nothing of who reads is in what is shown.
        assert_eq!(shown.get("slots"), None);
        assert_eq!(shown.get("scouts"), None);
        for reached in [
            ReachedField::Typed,
            ReachedField::Click,
            ReachedField::BackClick,
            ReachedField::Referer,
        ] {
            assert_eq!(json!(reached), json!(reached.as_str()));
        }

        // An item says about its page what was stored with it.
        assert_eq!(CrawlItemView::of(None), None);
        assert_eq!(CrawlItemView::of(Some(&json!({ "step": {} }))), None);
        let page = CrawlItemView {
            depth: 2,
            found_by: "link".to_owned(),
            found_on: Some("https://example.org/".to_owned()),
            reached: Some(ReachedField::BackClick),
            backs: Some(2),
            final_url: None,
            content_hash: Some("ab".repeat(32)),
            duplicate_of: None,
            near_duplicate_of: None,
            links: None,
            scroll: None,
            lastmod: None,
        };
        assert_eq!(CrawlItemView::of(Some(&page.stored())), Some(page));
    }
}
