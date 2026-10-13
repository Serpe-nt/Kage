//! A crawl being worked (spec §13, §16.6).
//!
//! The runner of a crawl holds the crawl's frontier in memory, as the
//! store has it, and writes every change through before it acts on it.
//! Its loop is the batch's: start what may start, wait, take what came
//! back. What may start is one page per visitor of a site: the page the
//! frontier names for what that visitor's tab holds, reached the way the
//! frontier says (a click on a link of a page in the tab, else a typed
//! address). The page is an item of the job from the moment it is asked
//! for, and is a fetch like any other: the scheduler paces it, keeps the
//! persona's visits, breaks and nights, and counts it for the breaker.
//!
//! With `use_sitemaps` the sitemaps of the seeds' sites are read before
//! the first page of each site, by a persona that is not the site's reader
//! (a scout) and never by the reader itself: a reader does not ask for
//! `/sitemap.xml`. A scout is a Firefox persona when one is there, because
//! Chrome saves a gzipped sitemap as a file and shows nothing of it.
//!
//! What a crawl keeps besides its frontier (its counters, the tab of every
//! visitor, the pages under way, its sitemaps) is stored with the job
//! after every page, so that a crawl that changes hands goes on where it
//! stands: a page that was being read is taken up again under its request.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use kage_browser::contract::{BodyEncoding, NavigationErrorCode, PageResult};
use kage_core::domain::{
    Engine, FrontierEntry, FrontierSource, FrontierState, JobItem, JobItemState, JobState,
    RenderMode,
};
use kage_core::ids::{FrontierId, JobItemId, PersonaId, RequestId};
use kage_core::outcome::Outcome;
use kage_core::schedule::planner::Visitors;
use kage_core::schedule::queue::Priority;
use kage_crawl::frontier::{
    Entry, EntryId, FoundLink, Frontier, Left, PageRead, Recorded, Source, State, Way,
};
use kage_crawl::{html, site, sitemap};
use kage_scheduler::{Arrival, FetchOrder, FetchReport, Reached};
use kage_store::StoreError;
use serde_json::{Map, Value, json};
use tokio::sync::OwnedSemaphorePermit;
use tokio::task::JoinSet;
use url::Url;

use super::{
    Ended, Ending, HOLD, ItemEnd, PATIENCE, Runner, STARVED, Tune, WorkError, default_wait,
    error_of, later,
};
use crate::error::ErrorCode;
use crate::fetch::result::ScrollEndField;
use crate::jobs::crawl::{
    CrawlItemView, CrawlRules, Flight, Kept, LinksView, NearDuplicateView, ReachedField,
    ScrollView, SitemapView, Slot, page_request, rules,
};
use crate::jobs::request::{Checks, JobRequest, stored_request};
use crate::resolve::guard;

/// A sitemap file that waits to be read.
const WAITING: &str = "waiting";

/// What came back from the scheduler.
enum Back {
    /// A page of the crawl.
    Page {
        entry: EntryId,
        flight: Flight,
        ended: Box<Ended>,
    },
    /// A sitemap file a scout asked for.
    Sitemap {
        site: String,
        url: String,
        end: Box<ItemEnd>,
    },
}

/// What became of an attempt to begin a page.
enum Begun {
    /// It is in the scheduler.
    Sent,
    /// The site has no page for this visitor now.
    Nothing,
    /// Every place in the scheduler is taken.
    Starved,
    /// The page is another worker's: so is the job.
    Lost,
}

/// A page that came back: its entry, who read it and on which site.
#[derive(Clone, Copy)]
struct Landed<'a> {
    entry: EntryId,
    flight: &'a Flight,
    site: &'a str,
}

/// A corrupt row of the frontier, as the store would say it.
fn corrupt(column: &'static str, reason: &str) -> WorkError {
    WorkError::Store(StoreError::Corrupt {
        table: "crawl_frontier",
        column,
        reason: reason.to_owned(),
    })
}

fn position(id: EntryId) -> u64 {
    u64::try_from(id).unwrap_or(u64::MAX)
}

fn entry_at(seq: u64) -> EntryId {
    usize::try_from(seq).unwrap_or(usize::MAX)
}

/// An entry of the frontier as the store returned it.
fn entry_of(row: &FrontierEntry) -> Result<Entry, WorkError> {
    let state = match row.state {
        FrontierState::Queued => State::Waiting,
        FrontierState::Fetching => State::Reading,
        FrontierState::Done => State::Read,
        FrontierState::Failed => State::Failed,
        FrontierState::Skipped => State::Skipped(
            row.skip_reason
                .as_deref()
                .and_then(Left::parse)
                .ok_or_else(|| corrupt("skip_reason", "not a reason this version knows"))?,
        ),
    };
    Ok(Entry {
        key: row.url_key.clone(),
        url: Url::parse(&row.url).map_err(|error| corrupt("url", &error.to_string()))?,
        site: row.site.clone(),
        depth: row.depth,
        state,
        source: match row.source {
            FrontierSource::Seed => Source::Seed,
            FrontierSource::Sitemap => Source::Sitemap,
            FrontierSource::Link => Source::Link,
        },
        found_on: row.found_on.map(entry_at),
        furniture: row.furniture,
        links: row.links.iter().copied().map(entry_at).collect(),
        text_links: usize::try_from(row.text_links).unwrap_or(usize::MAX),
        spellings: row
            .spellings
            .iter()
            .map(|(to, spelled)| {
                Url::parse(spelled)
                    .map(|spelled| (entry_at(*to), spelled))
                    .map_err(|error| corrupt("spellings", &error.to_string()))
            })
            .collect::<Result<_, _>>()?,
        final_url: row
            .final_url
            .as_deref()
            .map(Url::parse)
            .transpose()
            .map_err(|error| corrupt("final_url", &error.to_string()))?,
        left_site: row.left_site,
        content_hash: row.content_hash.clone(),
        simhash: row.simhash,
        duplicate_of: row.duplicate_of.map(entry_at),
        near_duplicate_of: row.near_duplicate_of.map(|(of, bits)| (entry_at(of), bits)),
        lastmod: row.lastmod,
    })
}

/// How a page was reached, as an item says it, and how many pages back.
fn reached_of(report: &FetchReport) -> (ReachedField, Option<u32>) {
    match report.last.as_ref().and_then(|last| last.reached) {
        Some(Reached::Link { backs: Some(0) }) => (ReachedField::Click, Some(0)),
        Some(Reached::Link { backs: Some(backs) }) => (ReachedField::BackClick, Some(backs)),
        Some(Reached::Link { backs: None }) => (ReachedField::Referer, None),
        Some(Reached::Typed) | None => (ReachedField::Typed, None),
    }
}

/// How the scrolling of a page a browser read ended.
fn scroll_of(page: &PageResult) -> Option<ScrollView> {
    let behavior = page.behavior.as_ref()?;
    Some(ScrollView {
        patience_ms: behavior.patience_ms,
        reached: behavior.scroll_reached,
        ended_by: behavior
            .scroll_ended_by
            .and_then(ScrollEndField::of)
            .map(|end| end.as_str().to_owned()),
    })
}

/// A page as the crawl takes it from a fetch that got it: where it ended,
/// what it links to and what it says. The links are the browser's own
/// when it brought them (with whether a reader can see each), else those
/// of the markup.
fn page_of(report: &FetchReport, asked: &Url) -> PageRead {
    let last = report.last.as_ref();
    let landed = last
        .and_then(|last| last.final_url.as_deref())
        .and_then(|url| Url::parse(url).ok())
        .unwrap_or_else(|| asked.clone());
    let document = last.and_then(|last| {
        last.page
            .as_ref()
            .and_then(|page| page.dom_html.clone())
            .or_else(|| last.fetched.as_ref().map(|fetched| fetched.text()))
    });
    let read = document.map(|document| html::read(&document, &landed));
    let seen = last.and_then(|last| last.page.as_ref()).and_then(|page| {
        page.links
            .as_ref()
            .map(|links| (links, page.links_truncated))
    });
    let (links, links_truncated) = match (seen, &read) {
        (Some((links, truncated)), _) => (
            links
                .iter()
                .filter_map(|link| {
                    Some(FoundLink {
                        url: Url::parse(&link.url).ok()?,
                        hidden: !link.visible,
                        furniture: link.furniture,
                    })
                })
                .collect(),
            truncated,
        ),
        (None, Some(read)) => (
            read.links
                .iter()
                .map(|link| FoundLink {
                    url: link.url.clone(),
                    hidden: link.hidden,
                    furniture: link.furniture,
                })
                .collect(),
            read.links_truncated,
        ),
        (None, None) => (Vec::new(), false),
    };
    PageRead {
        final_url: landed,
        links,
        links_truncated,
        text: read.map(|read| read.main_text).unwrap_or_default(),
    }
}

/// The body of the main response of a page a browser loaded, as bytes.
fn body_of(page: &PageResult) -> Option<Vec<u8>> {
    let body = page.raw_body.as_ref()?;
    match body.encoding {
        BodyEncoding::Utf8 => Some(body.data.clone().into_bytes()),
        BodyEncoding::Base64 => base64::engine::general_purpose::STANDARD
            .decode(body.data.as_bytes())
            .ok(),
    }
}

/// One crawl in the hands of its runner.
struct Crawl {
    rules: CrawlRules,
    defaults: Option<Map<String, Value>>,
    session: Option<String>,
    /// The persona of the job's session, which reads every page.
    pinned: Option<PersonaId>,
    frontier: Frontier,
    /// The stored identity of every entry, by position: its id and when
    /// the crawl came to know it.
    known: Vec<(FrontierId, DateTime<Utc>)>,
    /// The item of every page that was asked for.
    items: HashMap<EntryId, JobItemId>,
    /// When each page was read.
    read_at: HashMap<EntryId, DateTime<Utc>>,
    kept: Kept,
    /// Pages whose item goes into the scheduler again: what was deferred,
    /// and what was under way when the crawl changed hands.
    again: BTreeMap<String, VecDeque<(JobItem, EntryId)>>,
    /// Sites left alone until a moment, and what they wait for.
    held: HashMap<String, (DateTime<Utc>, String)>,
    /// The visitors that are reading a page: a site and a place.
    busy: HashSet<(String, usize)>,
    /// The sites a scout is reading a sitemap of.
    scouting: HashSet<String>,
}

impl Crawl {
    /// The crawl of a job as it stands in the store.
    async fn restore(runner: &Runner) -> Result<Result<Self, String>, WorkError> {
        let job = runner.job.id;
        let state = &runner.work.state;
        let Ok(JobRequest::Crawl(request)) = stored_request(&runner.job.spec) else {
            return Ok(Err(ErrorCode::ValidationFailed.as_str().to_owned()));
        };
        let policies = state.policies().current();
        let checks = Checks {
            config: state.config(),
            policies: &policies,
            resolver: state.resolver(),
        };
        let rules = match rules(&request, checks) {
            Ok(rules) => rules,
            Err(error) => return Ok(Err(error.code().as_str().to_owned())),
        };
        let kept = Kept::read(runner.store().jobs().crawl(job).await?);
        let rows = runner.store().crawl_frontier().all(job).await?;
        let mut entries = Vec::with_capacity(rows.len());
        let mut known = Vec::with_capacity(rows.len());
        let mut items = HashMap::new();
        let mut read_at = HashMap::new();
        for (id, row) in rows.iter().enumerate() {
            if row.seq != position(id) {
                return Err(corrupt("seq", "the positions of a crawl have a gap"));
            }
            entries.push(entry_of(row)?);
            known.push((row.id, row.created_at));
            if let Some(item) = row.item_id {
                items.insert(id, item);
            }
            if let Some(at) = row.fetched_at {
                read_at.insert(id, at);
            }
        }
        // Pages asked of the sites so far: what is being read, was read,
        // or failed.
        let begun = entries
            .iter()
            .filter(|entry| matches!(entry.state, State::Reading | State::Read | State::Failed))
            .count();
        let frontier = Frontier::restore(
            rules.scope.clone(),
            rules.limits,
            rules.normalize.clone(),
            entries,
            u32::try_from(begun).unwrap_or(u32::MAX),
            kept.progress.dropped,
        );
        let pinned = match &runner.work.session {
            Some(session) => runner
                .work
                .engine
                .personas
                .sessions()
                .resolve(runner.work.key.id, session)
                .await
                .ok()
                .flatten()
                .map(|found| found.persona.id),
            None => None,
        };
        Ok(Ok(Self {
            rules,
            defaults: request.defaults.clone(),
            session: request.session_id.clone(),
            pinned,
            frontier,
            known,
            items,
            read_at,
            kept,
            again: BTreeMap::new(),
            held: HashMap::new(),
            busy: HashSet::new(),
            scouting: HashSet::new(),
        }))
    }

    /// An entry as the store takes it.
    fn row(&self, runner: &Runner, id: EntryId) -> Option<FrontierEntry> {
        let entry = self.frontier.entry(id)?;
        let (identity, created_at) = *self.known.get(id)?;
        let (state, skip_reason) = match entry.state {
            State::Waiting => (FrontierState::Queued, None),
            State::Reading => (FrontierState::Fetching, None),
            State::Read => (FrontierState::Done, None),
            State::Failed => (FrontierState::Failed, None),
            State::Skipped(left) => (FrontierState::Skipped, Some(left.as_str().to_owned())),
        };
        Some(FrontierEntry {
            id: identity,
            job_id: runner.job.id,
            seq: position(id),
            url: entry.url.to_string(),
            url_key: entry.key.clone(),
            site: entry.site.clone(),
            depth: entry.depth,
            state,
            skip_reason,
            source: match entry.source {
                Source::Seed => FrontierSource::Seed,
                Source::Sitemap => FrontierSource::Sitemap,
                Source::Link => FrontierSource::Link,
            },
            found_on: entry.found_on.map(position),
            discovered_from: entry.found_on.and_then(|on| self.shown(on)),
            furniture: entry.furniture,
            links: entry.links.iter().copied().map(position).collect(),
            text_links: u32::try_from(entry.text_links).unwrap_or(u32::MAX),
            spellings: entry
                .spellings
                .iter()
                .map(|(to, spelled)| (position(*to), spelled.to_string()))
                .collect(),
            final_url: entry.final_url.as_ref().map(Url::to_string),
            left_site: entry.left_site,
            content_hash: entry.content_hash.clone(),
            simhash: entry.simhash,
            duplicate_of: entry.duplicate_of.map(position),
            near_duplicate_of: entry
                .near_duplicate_of
                .map(|(of, bits)| (position(of), bits)),
            lastmod: entry.lastmod,
            item_id: self.items.get(&id).copied(),
            created_at,
            fetched_at: self.read_at.get(&id).copied(),
        })
    }

    /// The address an item names another page of the crawl by: the one
    /// the page was read at when that is the same page under another
    /// spelling (it is what that page's own item is listed under), else
    /// the one the crawl first met. The end of a redirect is another page.
    fn shown(&self, id: EntryId) -> Option<String> {
        let entry = self.frontier.entry(id)?;
        let read_at = entry.final_url.as_ref().filter(|landed| {
            kage_core::urlnorm::normalize(landed, &self.rules.normalize).as_str() == entry.key
        });
        Some(read_at.unwrap_or(&entry.url).to_string())
    }

    /// The address a browser shows for a page that was read: where it
    /// ended.
    fn open_at(&self, id: EntryId) -> Option<Url> {
        let entry = self.frontier.entry(id)?;
        Some(entry.final_url.clone().unwrap_or_else(|| entry.url.clone()))
    }

    /// Writes what changed: the entries, then what the crawl keeps and
    /// shows. Counts what was newly skipped and newly found repeated.
    async fn persist(&mut self, runner: &Runner) -> Result<(), WorkError> {
        let now = crate::stored(runner.now());
        while self.known.len() < self.frontier.entries().len() {
            self.known.push((FrontierId::new(), now));
        }
        let rows: Vec<FrontierEntry> = self
            .frontier
            .take_changed()
            .into_iter()
            .filter_map(|id| self.row(runner, id))
            .collect();
        runner.store().crawl_frontier().save(&rows).await?;
        let counts = self.frontier.counts();
        for (reason, count) in &counts.skipped {
            let before = self
                .kept
                .progress
                .skipped
                .get(*reason)
                .copied()
                .unwrap_or(0);
            if *count > before {
                metrics::counter!("kage_crawl_skipped_total", "reason" => *reason)
                    .increment(*count - before);
            }
        }
        for (kind, count, before) in [
            ("exact", counts.duplicates, self.kept.progress.duplicates),
            (
                "near",
                counts.near_duplicates,
                self.kept.progress.near_duplicates,
            ),
        ] {
            if count > before {
                metrics::counter!("kage_crawl_duplicates_total", "kind" => kind)
                    .increment(count - before);
            }
        }
        self.kept.progress.count(&counts);
        self.kept.progress.max_pages = self.rules.limits.max_pages;
        self.kept.progress.max_depth = self.rules.scope.max_depth();
        let kept = serde_json::to_value(&self.kept).unwrap_or(Value::Null);
        runner
            .store()
            .jobs()
            .set_crawl(runner.job.id, &kept)
            .await?;
        Ok(())
    }

    /// The seeds, once, and the sitemaps of their sites when the crawl
    /// reads them.
    async fn seed(&mut self, runner: &Runner) -> Result<(), WorkError> {
        if self.kept.seeded {
            return Ok(());
        }
        let most =
            usize::try_from(runner.work.state.config().crawl.max_sitemaps).unwrap_or(usize::MAX);
        for seed in &self.rules.seeds.clone() {
            self.frontier.add_seed(seed);
            let (Some(site), true) = (site::of(seed), self.rules.use_sitemaps) else {
                continue;
            };
            let Ok(file) = seed.join("/sitemap.xml") else {
                continue;
            };
            let sitemaps = &mut self.kept.progress.sitemaps;
            let known = sitemaps.iter().any(|one| one.url == file.as_str());
            let of_site = sitemaps.iter().filter(|one| one.site == site).count();
            if !known && of_site < most {
                sitemaps.push(SitemapView {
                    site,
                    url: file.to_string(),
                    state: WAITING.to_owned(),
                    urls: 0,
                });
            }
        }
        self.kept.seeded = true;
        self.persist(runner).await
    }

    /// Takes up what was under way when the crawl changed hands: an open
    /// item goes into the scheduler again under its request; an item
    /// whose page is recorded is ended as the page ended; a page that was
    /// claimed and never asked for waits again.
    async fn resume(&mut self, runner: &Runner) -> Result<(), WorkError> {
        let mut open: HashMap<JobItemId, JobItem> = runner
            .store()
            .job_items()
            .open(runner.job.id)
            .await?
            .into_iter()
            .map(|item| (item.id, item))
            .collect();
        let by_item: HashMap<JobItemId, EntryId> = self
            .items
            .iter()
            .map(|(entry, item)| (*item, *entry))
            .collect();
        let reading: Vec<EntryId> = self
            .frontier
            .entries()
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.state == State::Reading)
            .map(|(id, _)| id)
            .collect();
        for id in reading {
            let item = self.items.get(&id).and_then(|item| open.remove(item));
            let Some(item) = item else {
                // Claimed, and the worker went away before it was asked
                // for: nobody reads it.
                self.items.remove(&id);
                self.kept.flights.remove(&position(id));
                self.frontier.put_back(id);
                continue;
            };
            let site = self
                .frontier
                .entry(id)
                .map(|entry| entry.site.clone())
                .unwrap_or_default();
            if let Some(until) = item.not_before {
                let reason = runner
                    .job
                    .paused_reason
                    .clone()
                    .unwrap_or_else(|| "waiting".to_owned());
                self.held.insert(site.clone(), (until, reason));
            }
            self.again.entry(site).or_default().push_back((item, id));
        }
        // What is left has a page that is recorded already, or none.
        for (id, item) in open {
            let entry = by_item
                .get(&id)
                .and_then(|entry| self.frontier.entry(*entry));
            let read = entry.is_some_and(|entry| entry.state == State::Read);
            let (state, error) = if read {
                (JobItemState::Succeeded, None)
            } else {
                (JobItemState::Failed, Some("interrupted".to_owned()))
            };
            let outcome = match runner.work.earlier(item.request_id).await? {
                Err(ItemEnd::Stored { outcome, .. }) => Some(outcome),
                _ => None,
            };
            runner.finish_item(item, state, outcome, error).await?;
        }
        self.persist(runner).await
    }

    /// The visitors of a site: as many places as the crawl has visitors.
    fn slots(&mut self, site: &str) -> &mut Vec<Slot> {
        let most = usize::try_from(self.rules.visitors.max(1)).unwrap_or(1);
        let slots = self.kept.slots.entry(site.to_owned()).or_default();
        slots.resize_with(most, Slot::default);
        slots
    }

    /// The visitor of a site in a place.
    fn slot(&mut self, site: &str, place: usize) -> &mut Slot {
        let slots = self.slots(site);
        let place = place.min(slots.len().saturating_sub(1));
        &mut slots[place]
    }

    /// Whether a sitemap of the site is still to be read: its pages wait
    /// for what the sitemaps name.
    fn sitemaps_open(&self, site: &str) -> bool {
        self.scouting.contains(site)
            || self
                .kept
                .progress
                .sitemaps
                .iter()
                .any(|one| one.site == site && one.state == WAITING)
    }

    /// Whether anything of the crawl is left to start.
    fn has_work(&self) -> bool {
        self.again.values().any(|queue| !queue.is_empty())
            || self
                .kept
                .progress
                .sitemaps
                .iter()
                .any(|one| one.state == WAITING)
            || (self.frontier.may_begin() && !self.frontier.sites_waiting().is_empty())
    }

    /// The personas that may not read a page of a site in a place: the
    /// scouts, and the site's other visitors.
    fn excluded(&self, site: &str, slot: usize) -> Vec<PersonaId> {
        let mut excluded: Vec<PersonaId> = self.kept.scouts.values().flatten().copied().collect();
        if let Some(slots) = self.kept.slots.get(site) {
            for (place, other) in slots.iter().enumerate() {
                if place != slot
                    && let Some(persona) = other.persona
                {
                    excluded.push(persona);
                }
            }
        }
        excluded.sort_unstable();
        excluded.dedup();
        excluded
    }

    /// Hands a page to the scheduler.
    fn send(
        &mut self,
        runner: &Runner,
        set: &mut JoinSet<Back>,
        item: JobItem,
        entry: EntryId,
        place: OwnedSemaphorePermit,
    ) {
        let seq = position(entry);
        let flight = self.kept.flights.get(&seq).cloned().unwrap_or(Flight {
            slot: 0,
            from: None,
        });
        let site = self
            .frontier
            .entry(entry)
            .map(|entry| entry.site.clone())
            .unwrap_or_default();
        let arrival = flight
            .from
            .and_then(|from| self.open_at(entry_at(from)))
            .map_or(Arrival::Typed, |from| Arrival::Link {
                from,
                max_backs: self.rules.limits.back_limit,
            });
        let tune = Tune {
            arrival: Some(arrival),
            exclude: self.excluded(&site, flight.slot),
            links: true,
        };
        // The page is its visitor's: the persona of its place once the
        // crawl has one there, and nobody else while that one can go on.
        let visitors = self.session.is_none().then(|| Visitors {
            known: self
                .kept
                .slots
                .get(&site)
                .and_then(|slots| slots.get(flight.slot))
                .and_then(|slot| slot.persona)
                .into_iter()
                .collect(),
            max: 1,
        });
        self.busy.insert((site.clone(), flight.slot));
        let work = runner.work.clone();
        let stop = runner.stop.subscribe();
        set.spawn(async move {
            let ended = Box::pin(work.item(item, site, visitors, stop, place, tune)).await;
            Back::Page {
                entry,
                flight,
                ended: Box::new(ended),
            }
        });
    }

    /// Begins the next page of a site for the visitor in a place: picks
    /// it, makes it the job's item and hands it to the scheduler.
    async fn begin_page(
        &mut self,
        runner: &Runner,
        set: &mut JoinSet<Back>,
        site: &str,
        slot: usize,
    ) -> Result<Begun, WorkError> {
        let ssrf = runner.work.state.config().security.ssrf();
        let tab: Vec<EntryId> = self
            .slot(site, slot)
            .tab
            .iter()
            .copied()
            .map(entry_at)
            .collect();
        // A page the guard refuses is left out and the next one looked
        // at: nothing is asked of an address that leads into a private
        // network.
        let (pick, url) = loop {
            let Some(pick) = self.frontier.next(site, &tab) else {
                return Ok(Begun::Nothing);
            };
            // The address as the page whose link is followed has it.
            let url = pick.url.clone();
            if guard(&url, ssrf, runner.work.state.resolver())
                .await
                .is_ok()
            {
                break (pick, url);
            }
            self.frontier.refuse(pick.entry);
            self.persist(runner).await?;
        };
        let Ok(place) = Arc::clone(&runner.hub().in_flight).try_acquire_owned() else {
            return Ok(Begun::Starved);
        };
        let seq = position(pick.entry);
        let item = JobItem {
            id: JobItemId::new(),
            job_id: runner.job.id,
            seq: 0,
            state: JobItemState::Queued,
            input: page_request(self.defaults.as_ref(), &url, self.session.as_deref()),
            site: Some(site.to_owned()),
            request_id: None,
            outcome: None,
            output: None,
            error: None,
            not_before: None,
            claims: 0,
            created_at: crate::stored(runner.now()),
            started_at: None,
            finished_at: None,
        };
        // The entry is the item's before the item is the job's: whatever
        // happens between the two, no page is asked for twice and none is
        // counted that was not asked for.
        if !runner
            .store()
            .crawl_frontier()
            .claim(runner.job.id, seq, item.id)
            .await?
        {
            tracing::warn!(job = %runner.job.id, seq, "a page of a crawl is another worker's");
            return Ok(Begun::Lost);
        }
        self.frontier.begin(pick.entry);
        self.items.insert(pick.entry, item.id);
        let from = match &pick.way {
            Way::Typed => None,
            Way::Link { backs, .. } => usize::try_from(*backs)
                .ok()
                .and_then(|backs| tab.len().checked_sub(backs + 1))
                .and_then(|at| tab.get(at))
                .copied()
                .map(position),
        };
        self.kept.flights.insert(seq, Flight { slot, from });
        let Some(at) = runner.store().job_items().append(&item).await? else {
            return Ok(Begun::Lost);
        };
        self.persist(runner).await?;
        let item = JobItem { seq: at, ..item };
        self.send(runner, set, item, pick.entry, place);
        Ok(Begun::Sent)
    }

    /// Starts the pages that may start: per site and visitor one, the
    /// deferred ones first. Returns whether a page could not start because
    /// every place in the scheduler is taken, and `None` when a page turned
    /// out to be another worker's: the job is not this runner's any more.
    async fn start_pages(
        &mut self,
        runner: &Runner,
        set: &mut JoinSet<Back>,
        now: DateTime<Utc>,
    ) -> Result<Option<bool>, WorkError> {
        let mut sites: Vec<String> = self.again.keys().cloned().collect();
        for site in self.frontier.sites_waiting() {
            if !sites.contains(&site) {
                sites.push(site);
            }
        }
        for site in sites {
            if self.held.get(&site).is_some_and(|(until, _)| *until > now) {
                continue;
            }
            // What was sent before goes first, into its own place.
            while let Some((_, entry)) = self.again.get(&site).and_then(VecDeque::front) {
                let slot = self
                    .kept
                    .flights
                    .get(&position(*entry))
                    .map_or(0, |flight| flight.slot);
                if self.busy.contains(&(site.clone(), slot)) {
                    break;
                }
                let Ok(place) = Arc::clone(&runner.hub().in_flight).try_acquire_owned() else {
                    return Ok(Some(true));
                };
                let Some((item, entry)) = self.again.get_mut(&site).and_then(VecDeque::pop_front)
                else {
                    break;
                };
                self.held.remove(&site);
                self.send(runner, set, item, entry, place);
            }
            if self.again.get(&site).is_some_and(|queue| !queue.is_empty())
                || self.sitemaps_open(&site)
            {
                continue;
            }
            let places = self.slots(&site).len();
            for slot in 0..places {
                if self.busy.contains(&(site.clone(), slot)) {
                    continue;
                }
                match self.begin_page(runner, set, &site, slot).await? {
                    Begun::Sent => {}
                    Begun::Nothing => break,
                    Begun::Starved => return Ok(Some(true)),
                    Begun::Lost => return Ok(None),
                }
            }
        }
        Ok(Some(false))
    }

    // --- sitemaps --------------------------------------------------------

    /// Who may read a sitemap of a site, as an order says it: the engines
    /// a scout is taken from and the personas it must not be. `None` when
    /// nobody but the site's reader could ask.
    async fn scout_for(
        &self,
        runner: &Runner,
        site: &str,
        host: &str,
    ) -> Result<Option<(Vec<Engine>, Vec<PersonaId>)>, WorkError> {
        let policy = runner.work.state.policies().current().domains.resolve(host);
        let mut readers: Vec<PersonaId> = self
            .kept
            .slots
            .get(site)
            .into_iter()
            .flatten()
            .filter_map(|slot| slot.persona)
            .collect();
        readers.extend(self.pinned);
        let personas = runner.work.engine.pipeline.personas().await?;
        let able: Vec<_> = personas
            .iter()
            .filter(|persona| persona.ready && policy.engines.contains(&persona.engine))
            .collect();
        let others: Vec<_> = able
            .iter()
            .filter(|persona| !readers.contains(&persona.id))
            .collect();
        // The scout takes one persona and the site's readers need theirs.
        let needed = if self.pinned.is_some() {
            1
        } else {
            1 + usize::try_from(self.rules.visitors.max(1)).unwrap_or(1)
        };
        if others.is_empty() || (self.pinned.is_none() && able.len() < needed) {
            return Ok(None);
        }
        // Chrome saves a gzipped sitemap as a file and shows nothing of
        // it; Firefox reads it.
        let firefox = others
            .iter()
            .any(|persona| persona.engine == Engine::Firefox);
        let engines = if firefox {
            vec![Engine::Firefox]
        } else {
            Vec::new()
        };
        Ok(Some((engines, readers)))
    }

    /// Sends a scout for the next sitemap of every site that has one
    /// waiting and nobody reading. Returns whether one could not start
    /// because every place in the scheduler is taken.
    async fn start_sitemaps(
        &mut self,
        runner: &Runner,
        set: &mut JoinSet<Back>,
        now: DateTime<Utc>,
    ) -> Result<bool, WorkError> {
        let waiting: Vec<(String, String)> = self
            .kept
            .progress
            .sitemaps
            .iter()
            .filter(|one| one.state == WAITING)
            .map(|one| (one.site.clone(), one.url.clone()))
            .collect();
        for (site, file) in waiting {
            if self.scouting.contains(&site)
                || self.held.get(&site).is_some_and(|(until, _)| *until > now)
            {
                continue;
            }
            let Ok(url) = Url::parse(&file) else {
                self.note_sitemap(&file, "failed", 0);
                continue;
            };
            let host = url.host_str().unwrap_or_default().to_owned();
            let Some((engines, readers)) = self.scout_for(runner, &site, &host).await? else {
                // Nobody but the reader could ask, and a reader does not.
                self.note_sitemap(&file, "no_scout", 0);
                self.persist(runner).await?;
                continue;
            };
            let Ok(place) = Arc::clone(&runner.hub().in_flight).try_acquire_owned() else {
                return Ok(true);
            };
            let mut order = FetchOrder::get(url, Priority::Job);
            order.render = RenderMode::Browser;
            // The file as the server sent it, beside what the browser
            // made of it.
            order.capture.raw = true;
            order.api_key = Some(runner.work.key.id);
            order.job = Some(runner.job.id);
            order.patience = Some(PATIENCE);
            order.engines = engines;
            order.exclude = readers;
            order.arrival = Some(Arrival::Typed);
            // A scout asks once: a file that did not come is not asked for
            // again from another address.
            order.max_retries = Some(0);
            order.visitors = self
                .kept
                .scouts
                .get(&site)
                .and_then(|scouts| scouts.last())
                .map(|scout| Visitors {
                    known: vec![*scout],
                    max: 1,
                });
            order.spec = json!({ "url": file.as_str(), "sitemap": true });
            self.scouting.insert(site.clone());
            self.held.remove(&site);
            let work = runner.work.clone();
            let stop = runner.stop.subscribe();
            set.spawn(async move {
                let _place = place;
                let end = match work.spend_page().await {
                    Ok(()) => work.fetch(RequestId::new(), order, None, stop).await,
                    Err(end) => end,
                };
                Back::Sitemap {
                    site,
                    url: file,
                    end: Box::new(end),
                }
            });
        }
        Ok(false)
    }

    /// Writes down how reading a sitemap file went.
    fn note_sitemap(&mut self, file: &str, state: &str, urls: u64) {
        if let Some(one) = self
            .kept
            .progress
            .sitemaps
            .iter_mut()
            .find(|one| one.url == file)
        {
            state.clone_into(&mut one.state);
            one.urls = urls;
        }
    }

    /// Takes what a sitemap file names: pages become places to start
    /// from, further sitemaps of the same site wait to be read.
    fn take_sitemap(&mut self, runner: &Runner, site: &str, file: &str, report: &FetchReport) {
        let config = &runner.work.state.config().crawl;
        let last = report.last.as_ref();
        let status = last.and_then(|last| last.status);
        if matches!(status, Some(404 | 410)) {
            self.note_sitemap(file, "missing", 0);
            return;
        }
        let page = last.and_then(|last| last.page.as_deref());
        // A browser saves a file it cannot show (a gzipped sitemap) and says
        // the navigation was aborted: no failure of the site's. Measured:
        // Camoufox 152 hands the bytes over all the same, Chrome 155 hands
        // over nothing.
        let saved = page
            .and_then(|page| page.error.as_ref())
            .is_some_and(|error| error.code == NavigationErrorCode::Aborted);
        if !(report.outcome.is_success() || saved)
            || !status.is_some_and(|status| (200..300).contains(&status))
        {
            self.note_sitemap(file, "failed", 0);
            return;
        }
        let limits = sitemap::Limits {
            max_entries: usize::try_from(config.max_sitemap_urls).unwrap_or(usize::MAX),
            ..sitemap::Limits::default()
        };
        let read = page
            .and_then(body_of)
            .and_then(|body| sitemap::parse(&body, limits).ok());
        let Some(read) = read else {
            // The browser showed nothing a sitemap can be read from: a
            // gzipped file in Chrome, or what is there is no sitemap.
            self.note_sitemap(file, "sitemap_unreadable", 0);
            return;
        };
        let mut taken = 0_u64;
        match read.kind {
            sitemap::Kind::Pages => {
                for entry in &read.entries {
                    if self
                        .frontier
                        .add_from_sitemap(&entry.url, entry.lastmod)
                        .is_some()
                    {
                        taken += 1;
                    }
                }
                self.note_sitemap(file, "read", taken);
            }
            sitemap::Kind::Index => {
                let most = usize::try_from(config.max_sitemaps).unwrap_or(usize::MAX);
                for entry in &read.entries {
                    let sitemaps = &mut self.kept.progress.sitemaps;
                    // A sitemap of another site is that site's.
                    if site::of(&entry.url).as_deref() != Some(site)
                        || sitemaps.iter().any(|one| one.url == entry.url.as_str())
                    {
                        continue;
                    }
                    let of_site = sitemaps.iter().filter(|one| one.site == site).count();
                    sitemaps.push(SitemapView {
                        site: site.to_owned(),
                        url: entry.url.to_string(),
                        state: if of_site < most { WAITING } else { "not_read" }.to_owned(),
                        urls: 0,
                    });
                    taken += 1;
                }
                self.note_sitemap(file, "index", taken);
            }
        }
    }

    /// Takes a sitemap that came back.
    async fn sitemap_back(
        &mut self,
        runner: &Runner,
        site: String,
        file: &str,
        end: ItemEnd,
    ) -> Result<(), WorkError> {
        self.scouting.remove(&site);
        match end {
            ItemEnd::Finished(report) => {
                // Whoever asked is a scout from here on, and no reader.
                let scouts = self.kept.scouts.entry(site.clone()).or_default();
                let asked = report
                    .attempts
                    .iter()
                    .map(|attempt| attempt.persona_id)
                    .chain(report.persona.as_ref().map(|persona| persona.id));
                for persona in asked {
                    scouts.retain(|scout| *scout != persona);
                    scouts.push(persona);
                }
                self.take_sitemap(runner, &site, file, &report);
            }
            ItemEnd::Deferred { reason, until } => {
                // It waits, and the site with it.
                let until = until.unwrap_or_else(|| later(runner.now(), default_wait(&reason)));
                self.held.insert(site, (until, reason));
            }
            // Nobody asked it to stop yet: it waits its turn again.
            ItemEnd::Stopped => {}
            ItemEnd::Stored { .. } | ItemEnd::Failed { .. } => {
                self.note_sitemap(file, "failed", 0);
            }
        }
        self.persist(runner).await
    }

    // --- pages -----------------------------------------------------------

    /// What an item of the crawl says about its page.
    fn view(
        &self,
        entry: EntryId,
        reached: Option<(ReachedField, Option<u32>)>,
        recorded: Option<&Recorded>,
        scroll: Option<ScrollView>,
    ) -> CrawlItemView {
        let page = self.frontier.entry(entry);
        CrawlItemView {
            depth: page.map_or(0, |page| page.depth),
            found_by: page.map_or("link", |page| page.source.as_str()).to_owned(),
            found_on: page
                .and_then(|page| page.found_on)
                .and_then(|on| self.shown(on)),
            reached: reached.map(|(reached, _)| reached),
            backs: reached.and_then(|(_, backs)| backs),
            // Where a redirect led: another page than the one asked for.
            // (The same page under another spelling is the item's own
            // address.)
            final_url: page
                .and_then(|page| page.final_url.as_ref().map(|landed| (page, landed)))
                .filter(|(page, landed)| {
                    kage_core::urlnorm::normalize(landed, &self.rules.normalize).as_str()
                        != page.key
                })
                .map(|(_, landed)| landed.to_string()),
            content_hash: page.and_then(|page| page.content_hash.clone()),
            duplicate_of: page
                .and_then(|page| page.duplicate_of)
                .and_then(|of| self.shown(of)),
            near_duplicate_of: page.and_then(|page| page.near_duplicate_of).and_then(
                |(of, bits)| {
                    Some(NearDuplicateView {
                        url: self.shown(of)?,
                        bits,
                    })
                },
            ),
            // The links of a page that repeats another are that page's:
            // nothing was taken from here.
            links: recorded
                .filter(|recorded| recorded.duplicate_of.is_none())
                .map(|recorded| LinksView {
                    found: recorded.links_found,
                    new: recorded.links_new,
                }),
            scroll,
            lastmod: page.and_then(|page| page.lastmod),
        }
    }

    /// Records a page a fetch came back with, and ends its item.
    async fn page_fetched(
        &mut self,
        runner: &Runner,
        mut item: JobItem,
        landed: Landed<'_>,
        report: &FetchReport,
    ) -> Result<(), WorkError> {
        let Landed {
            entry,
            flight,
            site,
        } = landed;
        let seq = position(entry);
        let back_limit = usize::try_from(self.rules.limits.back_limit).unwrap_or(usize::MAX);
        // Who reads the site in this place from now on: whoever got
        // through; one that was turned away is not waited for.
        let slot = self.slot(site, flight.slot);
        let before = slot.persona;
        for attempt in &report.attempts {
            if attempt.outcome.is_block() && slot.persona == Some(attempt.persona_id) {
                slot.persona = None;
            }
        }
        let read = report.outcome.is_success();
        if read && let Some(persona) = &report.persona {
            slot.persona = Some(persona.id);
        }
        if slot.persona != before {
            // Another persona has another tab.
            slot.tab.clear();
        }
        let view = if read {
            let reached = reached_of(report);
            // What the tab holds now: a click on a page it went back to
            // dropped what came after that page.
            if !matches!(reached.0, ReachedField::Typed)
                && let Some(at) = flight
                    .from
                    .and_then(|from| slot.tab.iter().rposition(|held| *held == from))
            {
                slot.tab.truncate(at + 1);
            }
            slot.tab.push(seq);
            if slot.tab.len() > back_limit.saturating_add(1) {
                let extra = slot.tab.len() - back_limit.saturating_add(1);
                slot.tab.drain(..extra);
            }
            let asked = self
                .frontier
                .entry(entry)
                .map_or_else(|| report_url(report), |entry| Some(entry.url.clone()));
            // What the site answered with an error is no page of the
            // crawl: it was reached and is stored like any answer, and
            // nothing is taken from it (its links are the error page's).
            let status = report.last.as_ref().and_then(|last| last.status);
            let answered = status.is_none_or(|status| (200..300).contains(&status));
            let recorded = asked.filter(|_| answered).map(|asked| {
                let page = page_of(report, &asked);
                self.frontier.record_read(entry, &page)
            });
            if recorded.is_none() {
                self.frontier.record_failed(entry);
            }
            self.kept.progress.reached.count(reached.0);
            metrics::counter!("kage_crawl_pages_total", "reached" => reached.0.as_str())
                .increment(1);
            let scroll = report
                .last
                .as_ref()
                .and_then(|last| last.page.as_deref())
                .and_then(scroll_of);
            self.view(entry, Some(reached), recorded.as_ref(), scroll)
        } else {
            // The browser left a wall or the page did not come: whatever
            // the tab held is not relied on.
            slot.tab.clear();
            self.frontier.record_failed(entry);
            self.view(entry, None, None, None)
        };
        self.kept.flights.remove(&seq);
        self.read_at.insert(entry, crate::stored(runner.now()));
        // The page is recorded before its item is ended: a crawl that
        // changes hands between the two ends the item as the page ended.
        self.persist(runner).await?;
        item.output = Some(view.stored());
        let state = if read {
            JobItemState::Succeeded
        } else {
            JobItemState::Failed
        };
        let error = error_of(report.outcome, report.reason.as_deref());
        runner
            .finish_item(item, state, Some(report.outcome), error)
            .await?;
        Ok(())
    }

    /// Records a page that ended without a fetch at hand, and ends its
    /// item: its result was stored before, or it could not be run at all.
    async fn page_ended(
        &mut self,
        runner: &Runner,
        mut item: JobItem,
        landed: Landed<'_>,
        outcome: Option<Outcome>,
        error: Option<String>,
    ) -> Result<(), WorkError> {
        let Landed {
            entry,
            flight,
            site,
        } = landed;
        let read = outcome.is_some_and(Outcome::is_success);
        if read {
            // It was read and what it held is not at hand any more: the
            // crawl knows the page and none of its links.
            if let Some(asked) = self.frontier.entry(entry).map(|entry| entry.url.clone()) {
                self.frontier.record_read(
                    entry,
                    &PageRead {
                        final_url: asked,
                        links: Vec::new(),
                        links_truncated: false,
                        text: String::new(),
                    },
                );
            }
        } else {
            self.frontier.record_failed(entry);
        }
        self.slot(site, flight.slot).tab.clear();
        self.kept.flights.remove(&position(entry));
        self.read_at.insert(entry, crate::stored(runner.now()));
        self.persist(runner).await?;
        item.output = Some(self.view(entry, None, None, None).stored());
        let state = if read {
            JobItemState::Succeeded
        } else {
            JobItemState::Failed
        };
        runner.finish_item(item, state, outcome, error).await?;
        Ok(())
    }

    /// Takes a page that came back while the crawl goes on.
    async fn page_back(
        &mut self,
        runner: &Runner,
        entry: EntryId,
        flight: &Flight,
        ended: Ended,
    ) -> Result<(), WorkError> {
        let Ended {
            mut item,
            site,
            end,
        } = ended;
        self.busy.remove(&(site.clone(), flight.slot));
        let landed = Landed {
            entry,
            flight,
            site: &site,
        };
        match end {
            ItemEnd::Finished(report) => self.page_fetched(runner, item, landed, &report).await,
            ItemEnd::Stored { outcome, reason } => {
                let error = error_of(outcome, reason.as_deref());
                self.page_ended(runner, item, landed, Some(outcome), error)
                    .await
            }
            ItemEnd::Failed { error, outcome } => {
                self.page_ended(runner, item, landed, outcome, Some(error))
                    .await
            }
            ItemEnd::Deferred { reason, until } => {
                let until = until.unwrap_or_else(|| later(runner.now(), default_wait(&reason)));
                runner
                    .store()
                    .job_items()
                    .defer(item.id, Some(until))
                    .await?;
                item.state = JobItemState::Queued;
                item.not_before = Some(until);
                self.held.insert(site.clone(), (until, reason));
                self.again
                    .entry(site)
                    .or_default()
                    .push_front((item, entry));
                Ok(())
            }
            ItemEnd::Stopped => {
                // Nobody asked it to stop yet: it waits its turn again.
                runner.store().job_items().defer(item.id, None).await?;
                item.state = JobItemState::Queued;
                self.again
                    .entry(site)
                    .or_default()
                    .push_front((item, entry));
                Ok(())
            }
        }
    }

    /// Stops working the crawl: what is in the scheduler comes back, what
    /// waited is ended or left as the reason says, and the job is ended or
    /// given back.
    async fn end(
        &mut self,
        runner: &Runner,
        ending: Ending,
        mut set: JoinSet<Back>,
    ) -> Result<(), WorkError> {
        let _ = runner.stop.send(true);
        let waited_for = self
            .held
            .values()
            .map(|(_, reason)| reason.as_str())
            .next()
            .unwrap_or("waiting")
            .to_owned();
        let expired = format!("expired: {waited_for}");
        let (state, reason) = match ending {
            Ending::Cancelled => (JobItemState::Cancelled, "cancelled"),
            Ending::Expired => (JobItemState::Failed, expired.as_str()),
            Ending::Shutdown | Ending::Lost => (JobItemState::Queued, ""),
        };
        let over = matches!(ending, Ending::Cancelled | Ending::Expired);
        while let Some(joined) = set.join_next().await {
            match (ending, joined?) {
                // Not this runner's job any more: whoever has it finds
                // everything as it stands.
                (Ending::Lost, _) => {}
                (_, Back::Sitemap { site, url, end }) => {
                    self.sitemap_back(runner, site, &url, *end).await?;
                }
                (
                    _,
                    Back::Page {
                        entry,
                        flight,
                        ended,
                    },
                ) => {
                    let waits = matches!(ended.end, ItemEnd::Deferred { .. } | ItemEnd::Stopped);
                    if !waits {
                        // A page that was under way is finished and kept.
                        self.page_back(runner, entry, &flight, *ended).await?;
                    } else if over {
                        // It was never asked for.
                        runner.work.abandon(&ended.item, reason).await;
                        self.kept.flights.remove(&position(entry));
                        self.frontier.put_back(entry);
                        runner
                            .finish_item(ended.item, state, None, Some(reason.to_owned()))
                            .await?;
                    } else {
                        let until = match &ended.end {
                            ItemEnd::Deferred { until, .. } => *until,
                            _ => None,
                        };
                        runner
                            .store()
                            .job_items()
                            .defer(ended.item.id, until)
                            .await?;
                    }
                }
            }
        }
        match ending {
            Ending::Lost => {}
            Ending::Shutdown => {
                self.persist(runner).await?;
                runner.release(None).await?;
            }
            Ending::Cancelled | Ending::Expired => {
                // What waited to go into the scheduler again was never
                // asked for either.
                for (item, entry) in std::mem::take(&mut self.again).into_values().flatten() {
                    runner.work.abandon(&item, reason).await;
                    self.kept.flights.remove(&position(entry));
                    self.frontier.put_back(entry);
                    runner
                        .finish_item(item, state, None, Some(reason.to_owned()))
                        .await?;
                }
                for one in &mut self.kept.progress.sitemaps {
                    if one.state == WAITING {
                        "not_read".clone_into(&mut one.state);
                    }
                }
                // A crawl that was stopped before it was through says
                // so, and is cancelled whatever its pages so far came to;
                // one that had nothing left ended as it would have.
                let unread = self.frontier.may_begin() && self.frontier.counts().waiting > 0;
                let cancelled = ending == Ending::Cancelled;
                self.kept.progress.ended = Some(
                    match (unread, cancelled) {
                        (true, true) => "cancelled",
                        (true, false) => "expired",
                        (false, _) if self.frontier.may_begin() => "completed",
                        (false, _) => "max_pages",
                    }
                    .to_owned(),
                );
                self.persist(runner).await?;
                let forced = (cancelled && unread).then_some(JobState::Cancelled);
                runner.finalize_as(forced, None).await?;
            }
        }
        Ok(())
    }

    /// Ends a crawl that has nothing left to start.
    async fn complete(&mut self, runner: &Runner) -> Result<(), WorkError> {
        let through = self.frontier.may_begin() || self.frontier.counts().waiting == 0;
        self.kept.progress.ended = Some(if through { "completed" } else { "max_pages" }.to_owned());
        self.persist(runner).await?;
        runner.finalize(None).await?;
        Ok(())
    }
}

/// The address a report's request asked for, when it says.
fn report_url(report: &FetchReport) -> Option<Url> {
    report
        .last
        .as_ref()
        .and_then(|last| last.final_url.as_deref())
        .and_then(|url| Url::parse(url).ok())
}

impl Runner {
    /// Works a crawl: see the module documentation.
    pub(super) async fn crawl(&mut self) -> Result<(), WorkError> {
        let mut crawl = match Crawl::restore(self).await? {
            Ok(crawl) => crawl,
            Err(reason) => return self.fail_all(&reason).await,
        };
        crawl.seed(self).await?;
        crawl.resume(self).await?;
        let mut set: JoinSet<Back> = JoinSet::new();

        loop {
            if let Some(ending) = self.ending() {
                return crawl.end(self, ending, set).await;
            }
            let now = self.now();
            // What a site waited for is over once its time has come.
            crawl.held.retain(|_, (until, _)| *until > now);
            let mut starved = crawl.start_sitemaps(self, &mut set, now).await?;
            match crawl.start_pages(self, &mut set, now).await? {
                Some(full) => starved |= full,
                // Another worker has the job.
                None => return crawl.end(self, Ending::Lost, set).await,
            }

            // When to look again if nothing comes back first.
            let next = crawl.held.values().map(|(until, _)| *until).min();
            if set.is_empty() && !starved {
                if !crawl.has_work() {
                    return crawl.complete(self).await;
                }
                if let Some(until) = next.filter(|until| *until > later(now, HOLD)) {
                    // Everything left waits: the job is given back.
                    let reason = crawl
                        .held
                        .values()
                        .min_by_key(|(until, _)| *until)
                        .map_or("waiting", |(_, reason)| reason.as_str())
                        .to_owned();
                    crawl.persist(self).await?;
                    self.release(Some((until, &reason))).await?;
                    return Ok(());
                }
            }
            let wake = if starved {
                STARVED
            } else if let Some(until) = next {
                (until - now).to_std().unwrap_or(Duration::ZERO)
            } else if set.is_empty() {
                // Something is left and nothing holds it: looked at again
                // at once, never slept on.
                Duration::from_secs(1)
            } else {
                Duration::from_hours(1)
            };
            tokio::select! {
                Some(joined) = set.join_next(), if !set.is_empty() => {
                    match joined? {
                        Back::Page { entry, flight, ended } => {
                            crawl.page_back(self, entry, &flight, *ended).await?;
                        }
                        Back::Sitemap { site, url, end } => {
                            crawl.sitemap_back(self, site, &url, *end).await?;
                        }
                    }
                }
                _ = self.signal.changed() => {}
                () = tokio::time::sleep(wake) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use kage_browser::contract::{EXAMPLES_JSON, PageResult};
    use kage_core::domain::{FrontierEntry, FrontierSource, FrontierState};
    use kage_core::ids::{FrontierId, JobId, JobItemId};
    use kage_crawl::frontier::{Left, Source, State};
    use kage_crawl::scope::Skip;
    use serde_json::{Value, json};

    use super::{body_of, entry_at, entry_of, position, scroll_of};

    fn row() -> FrontierEntry {
        let at = Utc.with_ymd_and_hms(2026, 10, 12, 9, 0, 0).unwrap();
        FrontierEntry {
            id: FrontierId::new(),
            job_id: JobId::new(),
            seq: 4,
            url: "https://example.org/a?utm_source=home".to_owned(),
            url_key: "https://example.org/a".to_owned(),
            site: "example.org".to_owned(),
            depth: 2,
            state: FrontierState::Done,
            skip_reason: None,
            source: FrontierSource::Link,
            found_on: Some(1),
            discovered_from: Some("https://example.org/".to_owned()),
            furniture: true,
            links: vec![5, 6],
            text_links: 1,
            spellings: vec![(6, "https://example.org/b?ref=a".to_owned())],
            final_url: Some("https://example.org/a/".to_owned()),
            left_site: false,
            content_hash: Some("ab".repeat(32)),
            simhash: Some(0xF000_0000_0000_000F),
            duplicate_of: Some(2),
            near_duplicate_of: Some((3, 5)),
            lastmod: Some(at),
            item_id: Some(JobItemId::new()),
            created_at: at,
            fetched_at: Some(at),
        }
    }

    fn page(change: impl FnOnce(&mut Value)) -> PageResult {
        let examples: Value = serde_json::from_str(EXAMPLES_JSON).unwrap();
        let mut page = examples["PageResult"][0].clone();
        change(&mut page);
        serde_json::from_value(page).unwrap()
    }

    #[test]
    fn a_stored_entry_is_the_frontiers_entry_and_a_broken_one_is_said_to_be() {
        type Break = fn(&mut FrontierEntry);
        let stored = row();
        let entry = entry_of(&stored).unwrap();
        assert_eq!(entry.key, stored.url_key);
        assert_eq!(entry.url.as_str(), stored.url);
        assert_eq!((entry.depth, entry.state), (2, State::Read));
        assert_eq!(entry.source, Source::Link);
        assert_eq!((entry.found_on, entry.furniture), (Some(1), true));
        assert_eq!(
            (entry.links.as_slice(), entry.text_links),
            ([5, 6].as_slice(), 1)
        );
        assert_eq!(entry.spellings[0].0, 6);
        assert_eq!(entry.spellings[0].1.as_str(), "https://example.org/b?ref=a");
        assert_eq!(
            entry.final_url.as_ref().map(url::Url::as_str),
            Some("https://example.org/a/")
        );
        assert_eq!(entry.duplicate_of, Some(2));
        assert_eq!(entry.near_duplicate_of, Some((3, 5)));
        assert_eq!(entry.lastmod, stored.lastmod);

        // Every state, and why a page is not read.
        for (state, reason, expected) in [
            (FrontierState::Queued, None, State::Waiting),
            (FrontierState::Fetching, None, State::Reading),
            (FrontierState::Failed, None, State::Failed),
            (
                FrontierState::Skipped,
                Some("hidden"),
                State::Skipped(Left::Scope(Skip::Hidden)),
            ),
            (
                FrontierState::Skipped,
                Some("already_read"),
                State::Skipped(Left::AlreadyRead),
            ),
            (
                FrontierState::Skipped,
                Some("ssrf_denied"),
                State::Skipped(Left::Guard),
            ),
        ] {
            let mut stored = row();
            stored.state = state;
            stored.skip_reason = reason.map(str::to_owned);
            assert_eq!(entry_of(&stored).unwrap().state, expected);
        }
        for (source, expected) in [
            (FrontierSource::Seed, Source::Seed),
            (FrontierSource::Sitemap, Source::Sitemap),
        ] {
            let mut stored = row();
            stored.source = source;
            assert_eq!(entry_of(&stored).unwrap().source, expected);
        }

        // What cannot be an entry is a corrupt row, never a guess.
        let broken: [(Break, &str); 5] = [
            (|row| row.url = "not an address".to_owned(), "url"),
            (
                |row| row.final_url = Some("nor this".to_owned()),
                "final_url",
            ),
            (
                |row| row.spellings = vec![(1, "nor this".to_owned())],
                "spellings",
            ),
            (
                |row| {
                    row.state = FrontierState::Skipped;
                    row.skip_reason = Some("because".to_owned());
                },
                "skip_reason",
            ),
            (
                |row| {
                    row.state = FrontierState::Skipped;
                    row.skip_reason = None;
                },
                "skip_reason",
            ),
        ];
        for (change, column) in broken {
            let mut stored = row();
            change(&mut stored);
            let error = entry_of(&stored).unwrap_err().to_string();
            assert!(error.contains(column), "{error}");
        }
        assert_eq!(entry_at(position(7)), 7);
    }

    #[test]
    fn the_body_of_a_page_is_read_whatever_it_is_written_in() {
        let none = page(|page| page["raw_body"] = Value::Null);
        assert_eq!(body_of(&none), None);
        let text = page(|page| {
            page["raw_body"] =
                json!({ "data": "<urlset/>", "encoding": "utf8", "bytes": 9, "truncated": false });
        });
        assert_eq!(body_of(&text).as_deref(), Some(b"<urlset/>".as_slice()));
        // A gzipped file comes as base64: its first bytes are gzip's.
        let packed = page(|page| {
            page["raw_body"] =
                json!({ "data": "H4sIAAAA", "encoding": "base64", "bytes": 6, "truncated": false });
        });
        assert_eq!(body_of(&packed).unwrap()[..2], [0x1f, 0x8b]);
        let broken = page(|page| {
            page["raw_body"] = json!({ "data": "not base64!", "encoding": "base64", "bytes": 6, "truncated": false });
        });
        assert_eq!(body_of(&broken), None);
    }

    #[test]
    fn the_end_of_a_pages_scrolling_is_shown_as_the_browser_said_it() {
        let read = |end: &str| {
            scroll_of(&page(|page| {
                page["behavior"]["scroll_ended_by"] = json!(end);
                page["behavior"]["scroll_reached"] = json!(0.25);
                page["behavior"]["patience_ms"] = json!(7000);
            }))
            .unwrap()
        };
        for end in ["depth", "patience", "page"] {
            let scroll = read(end);
            assert_eq!(scroll.ended_by.as_deref(), Some(end));
            assert_eq!(
                (scroll.reached, scroll.patience_ms),
                (Some(0.25), Some(7000))
            );
        }
        // Kage gives every page a patience: the time to reach it never
        // ends the scrolling, and is not shown as if it had.
        assert_eq!(read("budget").ended_by, None);
        assert_eq!(
            scroll_of(&page(|page| page["behavior"] = Value::Null)),
            None
        );
    }
}
