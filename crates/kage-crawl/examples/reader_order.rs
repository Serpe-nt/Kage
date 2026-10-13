//! Measures how a crawl reaches its pages (spec §13) on sites that are only
//! tables of links: how many addresses are typed, how many are a click on a
//! link of the open page, and how many a click after going back.
//!
//! ```text
//! cargo run --package kage-crawl --example reader_order
//! ```
//!
//! Three orders are compared on every site, each for a crawl of the whole
//! site inside three links and for one of a hundred pages:
//!
//! - `breadth`: the nearest unread page first; it is a click only when the
//!   open page happens to hold its link.
//! - `page first`: the first version of the reader's order: a link of the
//!   open page, the links of its text before those of its menu; else a link
//!   of a page up to three back in the tab; else the oldest unread address
//!   of the least depth, typed.
//! - `reader`: the frontier's own order ([`Frontier::next`]), which prefers
//!   a link of the text anywhere within those three steps back over any
//!   link of a menu. The measurement below is why.
//!
//! A typed address arrives from nowhere (no `Referer`, `Sec-Fetch-Site:
//! none`), which no reader does page after page: the share of typed
//! addresses is what the order is judged by.
//!
//! A second table (spec §15) asks what a response cache would do to that
//! order if a crawl were served from it: a page that is served is not
//! visited, so it is never open in the tab, and what it links to cannot be
//! clicked from it. For shares of a site that are in the cache (which pages
//! are is drawn from a seed, five seeds a row) it says how many pages are
//! still asked of the site and how those are reached.

// An example over tables of its own: an address that does not parse is a
// mistake in this file.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::io::Write as _;

use kage_core::config::DEFAULT_SKIPPED_EXTENSIONS;
use kage_core::urlnorm::NormalizeOptions;
use kage_crawl::frontier::{EntryId, FoundLink, Frontier, Limits, PageRead, State, Way};
use kage_crawl::scope::{Scope, ScopeRequest};
use url::Url;

const HOST: &str = "https://site.example";
const BACK_LIMIT: usize = 3;

/// A site: for every page the links of its text and the links of its menu.
#[derive(Default)]
struct Site {
    name: &'static str,
    pages: BTreeMap<String, (Vec<String>, Vec<String>)>,
}

impl Site {
    fn page(&mut self, path: String, text: Vec<String>, menu: &[&str]) {
        let menu = menu
            .iter()
            .filter(|link| **link != path)
            .map(|link| (*link).to_owned())
            .collect();
        self.pages.insert(path, (text, menu));
    }
}

/// A small generator: the same site every time.
struct Draw(u64);

impl Draw {
    fn below(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let bound = u64::try_from(bound.max(1)).unwrap_or(1);
        usize::try_from((self.0 >> 33) % bound).unwrap_or(0)
    }
}

/// Six categories of three listing pages, 360 items with four related each.
fn shop() -> Site {
    let mut site = Site {
        name: "shop: 6 categories of 3 pages, 360 items with 4 related each",
        ..Site::default()
    };
    let menu = [
        "/", "/c0", "/c1", "/c2", "/c3", "/c4", "/c5", "/about", "/contact",
    ];
    let mut draw = Draw(7);
    site.page(
        "/".to_owned(),
        (0..8).map(|n| format!("/i{}", n * 45)).collect(),
        &menu,
    );
    site.page("/about".to_owned(), Vec::new(), &menu);
    site.page("/contact".to_owned(), Vec::new(), &menu);
    for category in 0..6 {
        for page in 0..3 {
            let path = if page == 0 {
                format!("/c{category}")
            } else {
                format!("/c{category}?page={}", page + 1)
            };
            let mut text: Vec<String> = (0..20)
                .map(|n| format!("/i{}", category * 60 + page * 20 + n))
                .collect();
            if page < 2 {
                text.push(format!("/c{category}?page={}", page + 2));
            }
            site.page(path, text, &menu);
        }
    }
    for item in 0..360 {
        let related = (0..4).map(|_| format!("/i{}", draw.below(360))).collect();
        site.page(format!("/i{item}"), related, &menu);
    }
    site
}

/// Thirty index pages of ten posts; a post links the one before, the one
/// after and a tag.
fn blog() -> Site {
    let mut site = Site {
        name: "blog: 30 index pages of 10 posts, 300 posts with previous, next and a tag",
        ..Site::default()
    };
    let menu = ["/", "/archive", "/about"];
    let index = |page: usize| {
        if page == 0 {
            "/".to_owned()
        } else {
            format!("/page/{}", page + 1)
        }
    };
    for page in 0..30 {
        let mut text: Vec<String> = (0..10).map(|n| format!("/p{}", page * 10 + n)).collect();
        if page < 29 {
            text.push(index(page + 1));
        }
        site.page(index(page), text, &menu);
    }
    site.page("/archive".to_owned(), (0..30).map(index).collect(), &menu);
    site.page("/about".to_owned(), Vec::new(), &menu);
    for post in 0..300_usize {
        let mut text = vec![format!("/tag/{}", post % 12)];
        if post > 0 {
            text.push(format!("/p{}", post - 1));
        }
        if post < 299 {
            text.push(format!("/p{}", post + 1));
        }
        site.page(format!("/p{post}"), text, &menu);
    }
    for tag in 0..12_usize {
        let text = (0..25).map(|n| format!("/p{}", n * 12 + tag)).collect();
        site.page(format!("/tag/{tag}"), text, &menu);
    }
    site
}

/// Eight chapters of twelve sections, the chapters in a side bar on every
/// page.
fn manual() -> Site {
    let mut site = Site {
        name: "manual: 8 chapters of 12 sections, a side bar on every page",
        ..Site::default()
    };
    let menu = [
        "/", "/ch0", "/ch1", "/ch2", "/ch3", "/ch4", "/ch5", "/ch6", "/ch7",
    ];
    site.page("/".to_owned(), vec!["/ch0".to_owned()], &menu);
    for chapter in 0..8 {
        let text = (0..12).map(|n| format!("/ch{chapter}/s{n}")).collect();
        site.page(format!("/ch{chapter}"), text, &menu);
        for section in 0..12 {
            let text = if section < 11 {
                vec![format!("/ch{chapter}/s{}", section + 1)]
            } else {
                Vec::new()
            };
            site.page(format!("/ch{chapter}/s{section}"), text, &menu);
        }
    }
    site
}

/// The reading room of the fixture site (`tests/fixtures/site/crawl.mjs`):
/// a home page, a listing of ten articles, a menu on every page.
fn reading_room() -> Site {
    let mut site = Site {
        name: "reading room: a listing of ten articles, a menu on every page",
        ..Site::default()
    };
    let menu = ["/", "/articles", "/about"];
    let leaf = |site: &mut Site, path: &str, text: &[&str]| {
        site.page(
            path.to_owned(),
            text.iter().map(|link| (*link).to_owned()).collect(),
            &menu,
        );
    };
    leaf(&mut site, "/", &["/articles", "/a", "/about", "/tall"]);
    leaf(
        &mut site,
        "/articles",
        &[
            "/a", "/b", "/c", "/dup-1", "/dup-2", "/near-1", "/near-2", "/deep/1", "/moved",
            "/missing",
        ],
    );
    leaf(&mut site, "/a", &["/a/notes"]);
    leaf(&mut site, "/about", &["/annex", "/private"]);
    leaf(&mut site, "/deep/1", &["/deep/2"]);
    leaf(&mut site, "/deep/2", &["/deep/3"]);
    for path in [
        "/a/notes", "/b", "/c", "/dup-1", "/dup-2", "/near-1", "/near-2", "/moved", "/missing",
        "/tall", "/annex", "/private", "/deep/3",
    ] {
        leaf(&mut site, path, &[]);
    }
    site
}

fn url(path: &str) -> Url {
    Url::parse(&format!("{HOST}{path}")).unwrap()
}

fn path_of(url: &Url) -> String {
    match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    }
}

fn frontier(max_pages: u32) -> Option<Frontier> {
    let seed = url("/");
    let scope = Scope::new(
        &ScopeRequest {
            allow: Vec::new(),
            deny: Vec::new(),
            same_site_only: true,
            max_depth: 3,
            skipped_extensions: DEFAULT_SKIPPED_EXTENSIONS
                .iter()
                .map(|extension| (*extension).to_owned())
                .collect(),
        },
        std::slice::from_ref(&seed),
    )
    .ok()?;
    let mut frontier = Frontier::new(
        scope,
        Limits {
            max_pages,
            max_entries: 100_000,
            back_limit: u32::try_from(BACK_LIMIT).unwrap_or(3),
            near_duplicate_bits: 6,
            sitemap_depth: 0,
        },
        // The shop's listing pages differ only in their query.
        NormalizeOptions::default(),
    );
    frontier.add_seed(&seed)?;
    Some(frontier)
}

/// How the pages of one crawl were reached.
#[derive(Default)]
struct Ways {
    typed: usize,
    click: usize,
    back: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    Breadth,
    PageFirst,
    Reader,
}

/// The first link of `page` in `links` that waits to be read.
fn waiting(frontier: &Frontier, links: &[String]) -> Option<EntryId> {
    links.iter().find_map(|link| {
        let id = frontier.find(&url(link))?;
        (frontier.entry(id)?.state == State::Waiting).then_some(id)
    })
}

/// The page to read next under `order`, and how many entries back in the
/// tab its link is; `None` for a typed address.
fn pick(
    site: &Site,
    frontier: &Frontier,
    tab: &[EntryId],
    order: Order,
) -> Option<(EntryId, Option<usize>)> {
    let site_key = frontier.entries().first()?.site.clone();
    let links_of = |id: EntryId| {
        let path = path_of(&frontier.entry(id)?.url);
        site.pages.get(&path)
    };
    match order {
        Order::Reader => {
            let pick = frontier.next(&site_key, tab)?;
            let backs = match pick.way {
                Way::Typed => None,
                Way::Link { backs, .. } => usize::try_from(backs).ok(),
            };
            Some((pick.entry, backs))
        }
        Order::PageFirst => tab
            .iter()
            .rev()
            .take(BACK_LIMIT + 1)
            .enumerate()
            .find_map(|(backs, page)| {
                let (text, menu) = links_of(*page)?;
                let link = waiting(frontier, text).or_else(|| waiting(frontier, menu))?;
                Some((link, Some(backs)))
            })
            .or_else(|| frontier.next(&site_key, &[]).map(|pick| (pick.entry, None))),
        Order::Breadth => {
            let next = frontier.next(&site_key, &[])?.entry;
            let target = path_of(&frontier.entry(next)?.url);
            let held = tab
                .last()
                .and_then(|open| links_of(*open))
                .is_some_and(|links| links.0.contains(&target) || links.1.contains(&target));
            Some((next, held.then_some(0)))
        }
    }
}

/// Which pages a response cache holds: a share of the site in percent,
/// drawn per page from a seed.
#[derive(Clone, Copy)]
struct Cache {
    percent: u64,
    seed: u64,
}

impl Cache {
    /// Whether the page at `path` is in the cache (FNV-1a over the seed
    /// and the path: the same pages for the same seed, whatever the order
    /// they are met in).
    fn holds(self, path: &str) -> bool {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ self.seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        for byte in path.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        (hash >> 20) % 100 < self.percent
    }
}

/// Reads a site in `order`. With a cache, a page it holds is served and
/// not visited: its links are known, the tab never holds it. Returns how
/// the pages asked of the site were reached and how many those were.
fn crawl(site: &Site, max_pages: u32, order: Order, cache: Option<Cache>) -> Option<(Ways, usize)> {
    let mut frontier = frontier(max_pages)?;
    let mut tab: Vec<EntryId> = Vec::new();
    let mut ways = Ways::default();
    let mut read = 0;
    while frontier.may_begin() {
        let Some((entry, backs)) = pick(site, &frontier, &tab, order) else {
            break;
        };
        let path = path_of(&frontier.entry(entry)?.url);
        let served = cache.is_some_and(|cache| cache.holds(&path));
        if !served {
            match backs {
                None => ways.typed += 1,
                Some(0) => ways.click += 1,
                Some(backs) => {
                    ways.back += 1;
                    tab.truncate(tab.len().saturating_sub(backs));
                }
            }
        }
        frontier.begin(entry);
        let page = frontier.entry(entry)?.url.clone();
        let (text, menu) = site.pages.get(&path_of(&page)).cloned().unwrap_or_default();
        let link = |path: &String, furniture: bool| FoundLink {
            url: url(path),
            hidden: false,
            furniture,
        };
        // The menu comes first in a document; the frontier puts the links
        // of the text before it.
        let links = menu
            .iter()
            .map(|path| link(path, true))
            .chain(text.iter().map(|path| link(path, false)))
            .collect();
        frontier.record_read(
            entry,
            &PageRead {
                final_url: page,
                links,
                links_truncated: false,
                text: String::new(),
            },
        );
        if served {
            continue;
        }
        read += 1;
        tab.push(entry);
        if tab.len() > BACK_LIMIT + 1 {
            tab.remove(0);
        }
    }
    Some((ways, read))
}

/// The second table: the reader's order with shares of each site served
/// from a cache.
fn with_a_cache(out: &mut impl std::io::Write) -> Result<(), Box<dyn std::error::Error>> {
    const SEEDS: [u64; 5] = [1, 2, 3, 4, 5];
    writeln!(out)?;
    writeln!(
        out,
        "site | in the cache | pages asked of the site | typed, of those asked (fewest to most over five draws) | click on the open page | back, then click"
    )?;
    for site in [shop(), blog(), manual(), reading_room()] {
        for percent in [0, 10, 25, 50, 75, 90, 100] {
            let mut asked = Vec::new();
            let mut typed = Vec::new();
            let mut total = Ways::default();
            for seed in SEEDS {
                let cache = Cache { percent, seed };
                let (ways, read) = crawl(&site, 10_000, Order::Reader, Some(cache))
                    .ok_or("the site has no seed")?;
                asked.push(read);
                typed.push(
                    (ways.typed * 1000 + read / 2)
                        .checked_div(read)
                        .unwrap_or(0),
                );
                total.typed += ways.typed;
                total.click += ways.click;
                total.back += ways.back;
            }
            let all: usize = asked.iter().sum();
            let share = |count: usize| {
                format!(
                    "{} %",
                    (count * 100 + all / 2).checked_div(all).unwrap_or(0)
                )
            };
            let tenth = |value: usize| format!("{}.{} %", value / 10, value % 10);
            writeln!(
                out,
                "{} | {percent} % | {} | {} ({} to {}) | {} | {}",
                site.name,
                all / SEEDS.len(),
                share(total.typed),
                tenth(typed.iter().copied().min().unwrap_or(0)),
                tenth(typed.iter().copied().max().unwrap_or(0)),
                share(total.click),
                share(total.back),
            )?;
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "site | max_pages | order | pages read | typed | click on the open page | back, then click"
    )?;
    for site in [shop(), blog(), manual(), reading_room()] {
        for max_pages in [10_000, 100] {
            for (name, order) in [
                ("breadth", Order::Breadth),
                ("page first", Order::PageFirst),
                ("reader", Order::Reader),
            ] {
                let (ways, read) =
                    crawl(&site, max_pages, order, None).ok_or("the site has no seed")?;
                let share = |count: usize| format!("{} %", (count * 100 + read / 2) / read.max(1));
                writeln!(
                    out,
                    "{} | {max_pages} | {name} | {read} | {} ({}) | {} | {}",
                    site.name,
                    share(ways.typed),
                    ways.typed,
                    share(ways.click),
                    share(ways.back),
                )?;
            }
        }
    }
    with_a_cache(&mut out)
}
