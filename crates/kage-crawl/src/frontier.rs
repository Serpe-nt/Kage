//! The frontier of a crawl (spec §13): every address the crawl knows, what
//! became of each, and which one is read next.
//!
//! # The order
//!
//! A crawl reads a site the way a reader goes through it, because the way
//! from one page to the next is then a click on a link the open page shows
//! and not an address typed out of nowhere. The next page of a visitor is
//!
//! 1. a link in the text of the page that is open that the crawl has not
//!    read; else
//! 2. a link in the text of a page the visitor's tab can go back to, the
//!    nearest first, at most [`Limits::back_limit`] entries back; else
//! 3. a link of the open page's menu (what the site repeats on every page:
//!    its navigation, header, footer, side bar); else
//! 4. a link of the menu of a page within those entries back; else
//! 5. the waiting address of the least depth, the oldest first: a seed, an
//!    address a sitemap named, or a link whose page is out of reach. It is
//!    reached by a typed address.
//!
//! A reader who has finished an article goes back to the listing and picks
//! the next one; the menu is where a reader goes when the listing is
//! exhausted. Taking a menu link of the open page before going back (the
//! first version of this order) leaves the listing behind: measured on a
//! listing of ten articles with a menu on every page, eleven of eighteen
//! addresses were typed that way and one this way
//! (`examples/reader_order.rs`). Within a page the links are followed in
//! the order of the document.
//!
//! # Depth
//!
//! The depth of a page is the least number of links from a seed, whatever
//! way the crawl took. When a shorter way to a page is found after it was
//! read, the pages it links to move up with it, and one that was too deep
//! is read after all. The pages a crawl reads inside its depth are
//! therefore the same in every order.
//!
//! # What is kept
//!
//! Everything here is plain data and no call waits or fails: the worker
//! that drives a crawl writes what [`Frontier::take_changed`] names to the
//! store and gives the same rows back to [`Frontier::restore`] when the job
//! changes hands.

use std::collections::{BTreeMap, HashMap, VecDeque};

use chrono::{DateTime, Utc};
use kage_core::urlnorm::{NormalizeOptions, UrlKey, normalize, request_url};
use url::Url;

use crate::scope::{Scope, Skip};
use crate::simhash::{content_hash, distance, simhash};
use crate::site;

/// Names an entry of one frontier: its place in the order of discovery.
pub type EntryId = usize;

/// How the crawl came to know an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    /// The caller named it.
    Seed,
    /// A sitemap of the site named it.
    Sitemap,
    /// A page linked to it.
    Link,
}

impl Source {
    /// The name the store and a job show it by.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::Sitemap => "sitemap",
            Self::Link => "link",
        }
    }

    /// The source of a stored name.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "seed" => Some(Self::Seed),
            "sitemap" => Some(Self::Sitemap),
            "link" => Some(Self::Link),
            _ => None,
        }
    }
}

/// Why an address the crawl knows is not read, beyond the reasons of the
/// scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Left {
    /// The scope says no.
    Scope(Skip),
    /// It was read already, as the page another address redirected to.
    AlreadyRead,
    /// The guard against requests into private networks refuses it (spec
    /// §19): it is never asked for.
    Guard,
}

impl Left {
    /// The name a job shows the reason by.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scope(skip) => skip.as_str(),
            Self::AlreadyRead => "already_read",
            Self::Guard => "ssrf_denied",
        }
    }

    /// The reason of a stored name.
    pub fn parse(name: &str) -> Option<Self> {
        if name == "already_read" {
            return Some(Self::AlreadyRead);
        }
        if name == "ssrf_denied" {
            return Some(Self::Guard);
        }
        Skip::ALL
            .into_iter()
            .find(|skip| skip.as_str() == name)
            .map(Self::Scope)
    }
}

/// What became of an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// It waits to be read.
    Waiting,
    /// A visitor is reading it.
    Reading,
    /// It was read.
    Read,
    /// Reading it failed.
    Failed,
    /// It is not read, and why.
    Skipped(Left),
}

/// One address of a crawl.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its normalized form: one entry per key.
    pub key: String,
    /// The address as it was found, less its fragment: what is asked for.
    pub url: Url,
    /// The site it is on: its registrable domain.
    pub site: String,
    /// Links from the nearest seed.
    pub depth: u32,
    /// What became of it.
    pub state: State,
    /// How the crawl came to know it.
    pub source: Source,
    /// The page it was first found on.
    pub found_on: Option<EntryId>,
    /// Whether, where it was found, it stood in the site's furniture.
    pub furniture: bool,
    /// The pages it links to that a reader can follow, the links of the
    /// text first; empty until it is read.
    pub links: Vec<EntryId>,
    /// How many of `links`, from the first, are links of the text; the
    /// rest are links of the menu only.
    pub text_links: usize,
    /// The address the page was read at, when that is not the address the
    /// crawl first met: where a redirect led, or the page's address as the
    /// link that was followed spelled it. It is the address a browser shows
    /// for the page, and so the one a link of the page is clicked from.
    pub final_url: Option<Url>,
    /// Whether it ended on another site than the crawl stays on.
    pub left_site: bool,
    /// The hash of its text.
    pub content_hash: Option<String>,
    /// The fingerprint of its text.
    pub simhash: Option<u64>,
    /// The page it says the same as, word for word.
    pub duplicate_of: Option<EntryId>,
    /// The page it says nearly the same as, and the bits between them.
    pub near_duplicate_of: Option<(EntryId, u32)>,
    /// When the site's sitemap says it last changed.
    pub lastmod: Option<DateTime<Utc>>,
    /// The links of this page that spell their target otherwise than the
    /// crawl first met it (another campaign tag, another order of the
    /// query): the entry, and the address as this page has it. A reader
    /// who follows a link asks for the address the page gives.
    pub spellings: Vec<(EntryId, Url)>,
}

/// The limits of a crawl.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Pages the crawl asks of the sites it reads, at most.
    pub max_pages: u32,
    /// Addresses the crawl keeps, at most; what is found beyond them is
    /// counted and forgotten.
    pub max_entries: usize,
    /// How many entries of its tab a visitor goes back to find a link.
    pub back_limit: u32,
    /// Two pages at most this many bits apart are near-duplicates.
    pub near_duplicate_bits: u32,
    /// The depth of an address a sitemap names.
    pub sitemap_depth: u32,
}

/// A link as a page showed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundLink {
    /// Where it leads, as written.
    pub url: Url,
    /// Whether a reader cannot see it.
    pub hidden: bool,
    /// Whether it stands in the site's furniture.
    pub furniture: bool,
}

/// A page as it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRead {
    /// The address the page ended on, after redirects.
    pub final_url: Url,
    /// Its links, in the order of the document.
    pub links: Vec<FoundLink>,
    /// Whether the page held more links than were read.
    pub links_truncated: bool,
    /// The text two pages are compared by.
    pub text: String,
}

/// How a page is to be reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Way {
    /// By its address, typed.
    Typed,
    /// By a click on its link on a page of the visitor's tab.
    Link {
        /// The address of the page that holds the link.
        from: Url,
        /// How many entries back in the tab that page is; 0 for the open
        /// page.
        backs: u32,
    },
}

/// The page a visitor reads next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    /// The entry.
    pub entry: EntryId,
    /// How to get there.
    pub way: Way,
    /// The address to ask for: as the page whose link is followed spells
    /// it, else as the crawl first met it.
    pub url: Url,
}

/// What reading a page told the crawl.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Recorded {
    /// Links of the page a reader can follow.
    pub links_found: u32,
    /// Of those, the addresses the crawl did not know.
    pub links_new: u32,
    /// The page it is an exact duplicate of.
    pub duplicate_of: Option<EntryId>,
    /// The page it is a near-duplicate of, and the bits between them.
    pub near_duplicate_of: Option<(EntryId, u32)>,
}

/// The counters of a crawl.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Counts {
    /// Addresses the crawl knows.
    pub discovered: u64,
    /// Of them, waiting to be read.
    pub waiting: u64,
    /// Being read.
    pub reading: u64,
    /// Read.
    pub read: u64,
    /// Failed.
    pub failed: u64,
    /// Not read, by reason.
    pub skipped: BTreeMap<&'static str, u64>,
    /// Addresses found after the frontier was full, and forgotten.
    pub dropped: u64,
    /// Pages that say exactly what another page says.
    pub duplicates: u64,
    /// Pages that say nearly what another page says.
    pub near_duplicates: u64,
    /// The depth of the deepest page read.
    pub depth_reached: u32,
    /// Pages begun: what `max_pages` counts.
    pub begun: u32,
}

/// See the module documentation.
#[derive(Debug, Clone)]
pub struct Frontier {
    scope: Scope,
    limits: Limits,
    options: NormalizeOptions,
    entries: Vec<Entry>,
    by_key: HashMap<String, EntryId>,
    /// Entries whose stored form is out of date.
    changed: Vec<EntryId>,
    begun: u32,
    dropped: u64,
}

impl Frontier {
    /// An empty frontier.
    pub fn new(scope: Scope, limits: Limits, options: NormalizeOptions) -> Self {
        Self {
            scope,
            limits,
            options,
            entries: Vec::new(),
            by_key: HashMap::new(),
            changed: Vec::new(),
            begun: 0,
            dropped: 0,
        }
    }

    /// A frontier as it was stored: the entries in the order they were
    /// found, the pages begun so far, and the addresses forgotten. An entry
    /// that was being read when the crawl changed hands is still
    /// [`State::Reading`]: whoever restores gives back what nobody reads any
    /// more ([`Self::put_back`]).
    pub fn restore(
        scope: Scope,
        limits: Limits,
        options: NormalizeOptions,
        entries: Vec<Entry>,
        begun: u32,
        dropped: u64,
    ) -> Self {
        let mut by_key: HashMap<String, EntryId> = entries
            .iter()
            .enumerate()
            .map(|(id, entry)| (entry.key.clone(), id))
            .collect();
        // The end of a redirect is known by the page that led to it, unless
        // it has an entry of its own.
        for (id, entry) in entries.iter().enumerate() {
            if let Some(landed) = &entry.final_url {
                by_key
                    .entry(normalize(landed, &options).as_str().to_owned())
                    .or_insert(id);
            }
        }
        Self {
            scope,
            limits,
            options,
            entries,
            by_key,
            changed: Vec::new(),
            begun,
            dropped,
        }
    }

    /// The entry with this id.
    pub fn entry(&self, id: EntryId) -> Option<&Entry> {
        self.entries.get(id)
    }

    /// Every entry, in the order of discovery.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The entry of an address, when the crawl knows it.
    pub fn find(&self, url: &Url) -> Option<EntryId> {
        self.by_key
            .get(normalize(url, &self.options).as_str())
            .copied()
    }

    /// The entries that changed since this was last asked, each once, in
    /// the order of discovery: what the store has to be told.
    pub fn take_changed(&mut self) -> Vec<EntryId> {
        let mut changed = std::mem::take(&mut self.changed);
        changed.sort_unstable();
        changed.dedup();
        changed
    }

    fn touch(&mut self, id: EntryId) {
        self.changed.push(id);
    }

    fn key_of(&self, url: &Url) -> UrlKey {
        normalize(url, &self.options)
    }

    /// Adds an address the crawl was given: a seed, or one a sitemap named.
    /// Returns its entry, or `None` when the frontier is full.
    fn add(
        &mut self,
        url: &Url,
        source: Source,
        depth: u32,
        found_on: Option<EntryId>,
        furniture: bool,
        state: State,
    ) -> Option<EntryId> {
        if self.entries.len() >= self.limits.max_entries {
            self.dropped += 1;
            return None;
        }
        let key = self.key_of(url).as_str().to_owned();
        let id = self.entries.len();
        self.entries.push(Entry {
            key: key.clone(),
            url: request_url(url),
            site: site::of(url).unwrap_or_default(),
            depth,
            state,
            source,
            found_on,
            furniture,
            links: Vec::new(),
            text_links: 0,
            final_url: None,
            left_site: false,
            content_hash: None,
            simhash: None,
            duplicate_of: None,
            near_duplicate_of: None,
            lastmod: None,
            spellings: Vec::new(),
        });
        self.by_key.insert(key, id);
        self.touch(id);
        Some(id)
    }

    /// Adds a seed. A seed is read whatever the scope's patterns say; one
    /// the crawl knows already stays what it is.
    pub fn add_seed(&mut self, url: &Url) -> Option<EntryId> {
        if let Some(known) = self.find(url) {
            return Some(known);
        }
        self.add(url, Source::Seed, 0, None, false, State::Waiting)
    }

    /// Adds an address a sitemap named, judged by the scope like a link at
    /// the depth sitemap addresses have. One the crawl knows already keeps
    /// its place and takes the date.
    pub fn add_from_sitemap(
        &mut self,
        url: &Url,
        lastmod: Option<DateTime<Utc>>,
    ) -> Option<EntryId> {
        let depth = self.limits.sitemap_depth;
        let id = if let Some(known) = self.find(url) {
            self.lower(known, depth);
            known
        } else {
            let state = self.judged(url, &self.key_of(url), depth);
            self.add(url, Source::Sitemap, depth, None, false, state)?
        };
        if lastmod.is_some() && self.entries[id].lastmod != lastmod {
            self.entries[id].lastmod = lastmod;
            self.touch(id);
        }
        Some(id)
    }

    /// Gives an entry a smaller depth when `depth` is one, and with it
    /// everything it is known to link to. An entry that was too deep and no
    /// longer is waits to be read.
    fn lower(&mut self, id: EntryId, depth: u32) {
        let mut queue = VecDeque::from([(id, depth)]);
        while let Some((id, depth)) = queue.pop_front() {
            if depth >= self.entries[id].depth {
                continue;
            }
            self.entries[id].depth = depth;
            if self.entries[id].state == State::Skipped(Left::Scope(Skip::TooDeep))
                && depth <= self.scope.max_depth()
            {
                // Depth was the first thing that stood in its way; the rest
                // of the scope is asked now.
                let url = self.entries[id].url.clone();
                self.entries[id].state = self.judged(&url, &self.key_of(&url), depth);
            }
            self.touch(id);
            for next in self.entries[id].links.clone() {
                queue.push_back((next, depth.saturating_add(1)));
            }
        }
    }

    /// Whether the crawl may begin another page.
    pub const fn may_begin(&self) -> bool {
        self.begun < self.limits.max_pages
    }

    /// The sites that have a page waiting, in the order of their first.
    pub fn sites_waiting(&self) -> Vec<String> {
        let mut sites: Vec<String> = Vec::new();
        for entry in &self.entries {
            if entry.state == State::Waiting && !sites.contains(&entry.site) {
                sites.push(entry.site.clone());
            }
        }
        sites
    }

    /// The first link of a page that waits to be read on `site`: among the
    /// links of its text, or among those of its menu.
    fn waiting_link(&self, page: EntryId, site: &str, menu: bool) -> Option<EntryId> {
        let holder = self.entries.get(page)?;
        let text = holder.text_links.min(holder.links.len());
        let part = if menu {
            &holder.links[text..]
        } else {
            &holder.links[..text]
        };
        part.iter().copied().find(|link| {
            self.entries
                .get(*link)
                .is_some_and(|entry| entry.state == State::Waiting && entry.site == site)
        })
    }

    /// The link a visitor follows next from what its tab holds: a link of
    /// the text of the open page, else of a page within reach, the nearest
    /// first; then, in the same order, a link of a menu. With the page that
    /// holds it and how many entries back that page is.
    fn link_in_reach(&self, site: &str, tab: &[EntryId]) -> Option<(EntryId, EntryId, usize)> {
        let reach = usize::try_from(self.limits.back_limit).unwrap_or(usize::MAX);
        [false, true].into_iter().find_map(|menu| {
            tab.iter()
                .rev()
                .take(reach.saturating_add(1))
                .enumerate()
                .find_map(|(backs, page)| {
                    Some((self.waiting_link(*page, site, menu)?, *page, backs))
                })
        })
    }

    /// The page a visitor of `site` reads next. `tab` is what the visitor's
    /// tab holds: the pages it read in this sitting, the open one last.
    /// `None` when nothing of the site waits or the crawl has asked for as
    /// many pages as it may.
    pub fn next(&self, site: &str, tab: &[EntryId]) -> Option<Pick> {
        if !self.may_begin() {
            return None;
        }
        if let Some((link, page, backs)) = self.link_in_reach(site, tab)
            && let Some(holder) = self.entries.get(page)
        {
            return Some(Pick {
                entry: link,
                url: holder
                    .spellings
                    .iter()
                    .find(|(target, _)| *target == link)
                    .map(|(_, spelled)| spelled.clone())
                    .or_else(|| self.entries.get(link).map(|entry| entry.url.clone()))?,
                way: Way::Link {
                    // The address the browser shows for that page.
                    from: holder
                        .final_url
                        .clone()
                        .unwrap_or_else(|| holder.url.clone()),
                    backs: u32::try_from(backs).unwrap_or(u32::MAX),
                },
            });
        }
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.state == State::Waiting && entry.site == site)
            .min_by_key(|(id, entry)| (entry.depth, *id))
            .map(|(id, entry)| Pick {
                entry: id,
                way: Way::Typed,
                url: entry.url.clone(),
            })
    }

    /// Marks a page as being read and counts it against `max_pages`.
    /// Returns whether it was waiting.
    pub fn begin(&mut self, id: EntryId) -> bool {
        match self.entries.get_mut(id) {
            Some(entry) if entry.state == State::Waiting => {
                entry.state = State::Reading;
                self.begun = self.begun.saturating_add(1);
                self.touch(id);
                true
            }
            _ => false,
        }
    }

    /// Gives a page that was begun and not attempted back to the queue.
    pub fn put_back(&mut self, id: EntryId) {
        if let Some(entry) = self.entries.get_mut(id)
            && entry.state == State::Reading
        {
            entry.state = State::Waiting;
            self.begun = self.begun.saturating_sub(1);
            self.touch(id);
        }
    }

    /// Leaves out a page that waits because the guard refuses its address:
    /// it is never asked for and does not count against `max_pages`.
    /// Returns whether it was waiting.
    pub fn refuse(&mut self, id: EntryId) -> bool {
        match self.entries.get_mut(id) {
            Some(entry) if entry.state == State::Waiting => {
                entry.state = State::Skipped(Left::Guard);
                self.touch(id);
                true
            }
            _ => false,
        }
    }

    /// Records that reading a page failed. Its links stay unknown.
    pub fn record_failed(&mut self, id: EntryId) {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.state = State::Failed;
            self.touch(id);
        }
    }

    /// What the scope makes of an address at a depth.
    fn judged(&self, url: &Url, key: &UrlKey, depth: u32) -> State {
        match self.scope.judge(url, key, depth) {
            Ok(()) => State::Waiting,
            Err(skip) => State::Skipped(Left::Scope(skip)),
        }
    }

    /// Notes where a page ended when a redirect led elsewhere. Returns the
    /// page it is, when that page was read under its own address already.
    fn note_landing(&mut self, id: EntryId, landed: &Url) -> Option<EntryId> {
        // The address the browser shows for the page from here on: the
        // same page may have been asked for under another spelling than
        // the one the crawl first met.
        let shown = request_url(landed);
        self.entries[id].final_url = (shown != self.entries[id].url).then_some(shown);
        let final_key = self.key_of(landed).as_str().to_owned();
        if final_key == self.entries[id].key {
            return None;
        }
        self.entries[id].left_site = !self.scope.on_site(landed);
        let Some(other) = self.by_key.get(&final_key).copied() else {
            // Nobody asks for the end of the redirect a second time.
            self.by_key.insert(final_key, id);
            return None;
        };
        if other == id {
            return None;
        }
        match self.entries[other].state {
            // The page it ended on was read under its own address.
            State::Read => Some(other),
            // It waits under its own address: it has been read now.
            State::Waiting | State::Skipped(Left::Scope(Skip::TooDeep)) => {
                self.entries[other].state = State::Skipped(Left::AlreadyRead);
                self.touch(other);
                None
            }
            _ => None,
        }
    }

    /// Compares what a page says with every page read before it: the page
    /// that says the same word for word, else the nearest one inside the
    /// limit with the bits between them.
    fn compare(&mut self, id: EntryId, text: &str) -> (Option<EntryId>, Option<(EntryId, u32)>) {
        let hash = content_hash(text);
        let fingerprint = simhash(text);
        let says_something = !text.trim().is_empty();
        let mut same = None;
        let mut nearest: Option<(EntryId, u32)> = None;
        for (other, entry) in self.entries.iter().enumerate() {
            if other == id || entry.state != State::Read || entry.duplicate_of.is_some() {
                continue;
            }
            if says_something && entry.content_hash.as_deref() == Some(hash.as_str()) {
                same = Some(other);
                break;
            }
            if let (Some(a), Some(b)) = (fingerprint, entry.simhash) {
                let bits = distance(a, b);
                if bits <= self.limits.near_duplicate_bits
                    && nearest.is_none_or(|(_, least)| bits < least)
                {
                    nearest = Some((other, bits));
                }
            }
        }
        self.entries[id].content_hash = Some(hash);
        self.entries[id].simhash = fingerprint;
        if same.is_some() {
            (same, None)
        } else {
            (None, nearest)
        }
    }

    /// The entry of a link a page shows, made or moved up as the link
    /// says. `None` for a link the crawl does not keep: one no reader sees,
    /// or one the full frontier has no room for. The flag says whether the
    /// address was new to the crawl.
    fn follow(&mut self, page: EntryId, link: &FoundLink, depth: u32) -> (Option<EntryId>, bool) {
        let key = self.key_of(&link.url);
        let known = self.by_key.get(key.as_str()).copied();
        if known == Some(page) {
            return (None, false);
        }
        if link.hidden {
            // A link no reader sees is never followed; it is counted once,
            // unless the crawl knows the address already.
            if known.is_none() {
                self.add(
                    &link.url,
                    Source::Link,
                    depth,
                    Some(page),
                    link.furniture,
                    State::Skipped(Left::Scope(Skip::Hidden)),
                );
            }
            return (None, false);
        }
        let Some(known) = known else {
            let state = self.judged(&link.url, &key, depth);
            let added = self.add(
                &link.url,
                Source::Link,
                depth,
                Some(page),
                link.furniture,
                state,
            );
            return (added, added.is_some());
        };
        if self.entries[known].state == State::Skipped(Left::Scope(Skip::Hidden)) {
            // Until now only hidden links led to it: this page shows it.
            self.entries[known].depth = depth;
            self.entries[known].found_on = Some(page);
            self.entries[known].furniture = link.furniture;
            self.entries[known].state = self.judged(&link.url, &key, depth);
            self.touch(known);
        } else {
            self.lower(known, depth);
        }
        (Some(known), false)
    }

    /// Records a page that was read: where it ended, what it says, and
    /// what it links to.
    pub fn record_read(&mut self, id: EntryId, page: &PageRead) -> Recorded {
        let mut recorded = Recorded::default();
        if id >= self.entries.len() {
            return recorded;
        }
        self.entries[id].state = State::Read;
        self.touch(id);

        if let Some(original) = self.note_landing(id, &page.final_url) {
            self.entries[id].duplicate_of = Some(original);
            recorded.duplicate_of = Some(original);
            return recorded;
        }
        let (same, nearest) = self.compare(id, &page.text);
        if same.is_some() {
            // The same page under another address: its links are the
            // original's and are not followed from here.
            self.entries[id].duplicate_of = same;
            recorded.duplicate_of = same;
            return recorded;
        }
        self.entries[id].near_duplicate_of = nearest;
        recorded.near_duplicate_of = nearest;
        if self.entries[id].left_site {
            // A reader's browser would have followed the redirect too; the
            // crawl does not go on from another site.
            return recorded;
        }

        // What it links to: the links of the text first.
        let depth = self.entries[id].depth.saturating_add(1);
        let mut links: Vec<EntryId> = Vec::new();
        let mut spellings: Vec<(EntryId, Url)> = Vec::new();
        let mut text_links: Option<usize> = None;
        let ordered = page
            .links
            .iter()
            .filter(|link| !link.furniture)
            .chain(page.links.iter().filter(|link| link.furniture));
        for link in ordered {
            if link.furniture && text_links.is_none() {
                // Everything taken so far is a link of the text; a page
                // the menu names too is followed from the text.
                text_links = Some(links.len());
            }
            let (target, new) = self.follow(id, link, depth);
            if new {
                recorded.links_new += 1;
            }
            if let Some(target) = target
                && !links.contains(&target)
            {
                links.push(target);
                // The first link to it is the one a reader meets: when it
                // spells the address otherwise, that spelling is kept.
                let spelled = request_url(&link.url);
                if spelled != self.entries[target].url {
                    spellings.push((target, spelled));
                }
            }
        }
        recorded.links_found = u32::try_from(links.len()).unwrap_or(u32::MAX);
        self.entries[id].text_links = text_links.unwrap_or(links.len());
        self.entries[id].links = links;
        self.entries[id].spellings = spellings;
        recorded
    }

    /// The counters of the crawl as it stands.
    pub fn counts(&self) -> Counts {
        let mut counts = Counts {
            discovered: self.entries.len() as u64,
            dropped: self.dropped,
            begun: self.begun,
            ..Counts::default()
        };
        for entry in &self.entries {
            match entry.state {
                State::Waiting => counts.waiting += 1,
                State::Reading => counts.reading += 1,
                State::Failed => counts.failed += 1,
                State::Read => {
                    counts.read += 1;
                    counts.depth_reached = counts.depth_reached.max(entry.depth);
                    if entry.duplicate_of.is_some() {
                        counts.duplicates += 1;
                    } else if entry.near_duplicate_of.is_some() {
                        counts.near_duplicates += 1;
                    }
                }
                State::Skipped(left) => *counts.skipped.entry(left.as_str()).or_default() += 1,
            }
        }
        counts
    }
}

#[cfg(test)]
mod tests;
