//! The frontier on sites that are only tables: which page comes next and
//! how it is reached, what depth every page ends up with, and what is
//! flagged, skipped and counted.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{TimeZone, Utc};
use kage_core::config::DEFAULT_SKIPPED_EXTENSIONS;
use kage_core::urlnorm::NormalizeOptions;
use url::Url;

use super::{
    Entry, EntryId, FoundLink, Frontier, Left, Limits, PageRead, Pick, Source, State, Way,
};
use crate::scope::{Scope, ScopeRequest, Skip};

const SITE: &str = "shop.example";

fn url(path: &str) -> Url {
    if path.starts_with("http") {
        Url::parse(path).unwrap()
    } else {
        Url::parse(&format!("https://{SITE}{path}")).unwrap()
    }
}

/// A link as a page shows it: a path, with `~` in front when it stands in
/// the site's furniture and `!` when no reader can see it.
fn link(spec: &str) -> FoundLink {
    let hidden = spec.contains('!');
    let furniture = spec.contains('~');
    FoundLink {
        url: url(spec.trim_start_matches(['!', '~'])),
        hidden,
        furniture,
    }
}

/// Forty words nobody else has: a text of its own for every page.
fn text_of(name: &str) -> String {
    let mut state = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    }) | 1;
    (0..40)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            format!("w{}", state % 1_000_000)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A site as a table: what every page links to, where an address leads
/// when it redirects, and what a page says when it is not its own text.
#[derive(Default, Clone)]
struct Site {
    links: BTreeMap<String, Vec<String>>,
    redirects: BTreeMap<String, String>,
    texts: BTreeMap<String, String>,
}

impl Site {
    fn of(pages: &[(&str, &[&str])]) -> Self {
        Self {
            links: pages
                .iter()
                .map(|(path, links)| {
                    (
                        (*path).to_owned(),
                        links.iter().map(|link| (*link).to_owned()).collect(),
                    )
                })
                .collect(),
            ..Self::default()
        }
    }

    fn redirect(mut self, from: &str, to: &str) -> Self {
        self.redirects.insert(from.to_owned(), to.to_owned());
        self
    }

    fn saying(mut self, path: &str, text: &str) -> Self {
        self.texts.insert(path.to_owned(), text.to_owned());
        self
    }

    /// The page an address gives, after its redirect. A page of the
    /// site is known by its path, one of another host by its whole address.
    fn read(&self, asked: &Url) -> PageRead {
        let name = if asked.host_str() == Some(SITE) {
            asked.path().to_owned()
        } else {
            asked.as_str().to_owned()
        };
        let landed = self.redirects.get(&name).cloned().unwrap_or(name);
        let page = if landed == asked.path() {
            // No redirect: the address as it was asked for, its query too.
            asked.clone()
        } else {
            url(&landed)
        };
        let table = landed;
        PageRead {
            links: self
                .links
                .get(&table)
                .map(|links| links.iter().map(|spec| link(spec)).collect())
                .unwrap_or_default(),
            links_truncated: false,
            text: self
                .texts
                .get(&table)
                .cloned()
                .unwrap_or_else(|| text_of(&table)),
            final_url: page,
        }
    }
}

fn limits() -> Limits {
    Limits {
        max_pages: 1000,
        max_entries: 10_000,
        back_limit: 3,
        near_duplicate_bits: 6,
        sitemap_depth: 0,
    }
}

fn request(max_depth: u32) -> ScopeRequest {
    ScopeRequest {
        allow: Vec::new(),
        deny: Vec::new(),
        same_site_only: true,
        max_depth,
        skipped_extensions: DEFAULT_SKIPPED_EXTENSIONS
            .iter()
            .map(|&extension| extension.to_owned())
            .collect(),
    }
}

fn frontier_with(request: &ScopeRequest, limits: Limits, seeds: &[&str]) -> Frontier {
    let seeds: Vec<Url> = seeds.iter().map(|seed| url(seed)).collect();
    let mut frontier = Frontier::new(
        Scope::new(request, &seeds).unwrap(),
        limits,
        NormalizeOptions::default(),
    );
    for seed in &seeds {
        frontier.add_seed(seed).unwrap();
    }
    frontier
}

fn frontier(max_depth: u32) -> Frontier {
    frontier_with(&request(max_depth), limits(), &["/"])
}

/// How a page was reached, in a word.
fn way(pick: &Pick) -> String {
    match &pick.way {
        Way::Typed => "typed".to_owned(),
        Way::Link { backs: 0, .. } => "click".to_owned(),
        Way::Link { backs, .. } => format!("back {backs}"),
    }
}

/// One visitor reading a site: its tab, and what it did.
struct Reader {
    frontier: Frontier,
    tab: Vec<EntryId>,
    /// Every page read, with how it was reached.
    log: Vec<(String, String)>,
    /// A visit ends after this many pages: the tab starts empty again.
    visit: usize,
}

impl Reader {
    fn new(frontier: Frontier) -> Self {
        Self {
            frontier,
            tab: Vec::new(),
            log: Vec::new(),
            visit: usize::MAX,
        }
    }

    /// Reads one page of `site`; `false` when nothing is left to read.
    fn step(&mut self, pages: &Site, site: &str) -> bool {
        if !self.log.is_empty() && self.log.len().is_multiple_of(self.visit) {
            self.tab.clear();
        }
        let Some(pick) = self.frontier.next(site, &self.tab) else {
            return false;
        };
        assert!(self.frontier.begin(pick.entry));
        let asked = self.frontier.entry(pick.entry).unwrap().url.clone();
        match &pick.way {
            Way::Typed => self.tab.clear(),
            Way::Link { from, backs } => {
                let keep = self.tab.len() - usize::try_from(*backs).unwrap();
                self.tab.truncate(keep);
                // The page the link is clicked on is the one that is open.
                let open = self.frontier.entry(*self.tab.last().unwrap()).unwrap();
                assert_eq!(open.final_url.as_ref().unwrap_or(&open.url), from);
            }
        }
        self.tab.push(pick.entry);
        self.frontier.record_read(pick.entry, &pages.read(&asked));
        let mut name = asked.path().to_owned();
        if let Some(query) = asked.query() {
            name = format!("{name}?{query}");
        }
        self.log.push((name, way(&pick)));
        true
    }

    fn run(mut self, pages: &Site) -> Self {
        while self.step(pages, SITE) {}
        self
    }

    fn order(&self) -> Vec<&str> {
        self.log.iter().map(|(path, _)| path.as_str()).collect()
    }

    fn ways(&self) -> Vec<&str> {
        self.log.iter().map(|(_, way)| way.as_str()).collect()
    }

    fn entry(&self, path: &str) -> &Entry {
        let id = self.frontier.find(&url(path)).unwrap();
        self.frontier.entry(id).unwrap()
    }
}

/// A small site with its navigation on every page.
fn small_site() -> Site {
    Site::of(&[
        ("/", &["~/about", "/a", "/b"]),
        ("/a", &["~/about", "/a1", "/a2"]),
        ("/a1", &["~/about"]),
        ("/a2", &["~/about"]),
        ("/b", &["~/about", "/b1"]),
        ("/b1", &["~/about"]),
        ("/about", &["~/about"]),
    ])
}

#[test]
fn a_site_is_read_in_the_order_a_reader_goes() {
    let read = Reader::new(frontier(5)).run(&small_site());

    // The seed is typed. From there every page is a click on a link of
    // the text: of the open page while it has one the crawl has not read,
    // else of the nearest page the tab can go back to that has one (from
    // /a1 back to /a for /a2, from /a2 back to the home page for /b). The
    // menu, which every page has, is where the reader goes last: /about
    // is read when nothing in reach has a link of its text left.
    assert_eq!(
        read.order(),
        ["/", "/a", "/a1", "/a2", "/b", "/b1", "/about"]
    );
    assert_eq!(
        read.ways(),
        [
            "typed", "click", "click", "back 1", "back 2", "click", "click"
        ]
    );
    let depths: Vec<u32> = ["/", "/a", "/b", "/about", "/a1", "/a2", "/b1"]
        .iter()
        .map(|path| read.entry(path).depth)
        .collect();
    assert_eq!(depths, [0, 1, 1, 1, 2, 2, 2]);
    assert_eq!(read.entry("/").source, Source::Seed);
    assert_eq!(read.entry("/a1").source, Source::Link);
    assert_eq!(
        read.entry("/a1").found_on,
        read.frontier.find(&url("/a")),
        "found on the page that linked to it first"
    );
    assert!(read.entry("/about").furniture && !read.entry("/a").furniture);

    let counts = read.frontier.counts();
    assert_eq!((counts.discovered, counts.read, counts.waiting), (7, 7, 0));
    assert_eq!((counts.begun, counts.depth_reached), (7, 2));
    assert!(counts.skipped.is_empty());
}

#[test]
fn a_visitor_that_may_not_go_back_types_what_the_open_page_does_not_hold() {
    let mut frontier = frontier_with(
        &request(5),
        Limits {
            back_limit: 0,
            ..limits()
        },
        &["/"],
    );
    frontier.take_changed();
    let read = Reader::new(frontier).run(&small_site());
    // What is typed is the waiting page of the least depth, the oldest
    // first: /b was found before /about, both before the pages below /a.
    assert_eq!(
        read.order(),
        ["/", "/a", "/a1", "/about", "/b", "/b1", "/a2"]
    );
    assert_eq!(
        read.ways(),
        [
            "typed", "click", "click", "click", "typed", "click", "typed"
        ]
    );
}

#[test]
fn going_back_ends_where_the_limit_says() {
    // A chain down from /, and one more link on /: after the chain the
    // page that holds it is three entries back with a limit of three, and
    // out of reach with a limit of two.
    let pages = Site::of(&[
        ("/", &["/c1", "/other"]),
        ("/c1", &["/c2"]),
        ("/c2", &["/c3"]),
        ("/c3", &[]),
        ("/other", &[]),
    ]);
    let within = Reader::new(frontier(9)).run(&pages);
    assert_eq!(within.ways().last(), Some(&"back 3"));
    let beyond = Reader::new(frontier_with(
        &request(9),
        Limits {
            back_limit: 2,
            ..limits()
        },
        &["/"],
    ))
    .run(&pages);
    assert_eq!(beyond.order(), within.order());
    assert_eq!(beyond.ways().last(), Some(&"typed"));
}

#[test]
fn after_a_visit_the_tab_is_empty_and_the_next_page_is_typed() {
    let mut reader = Reader::new(frontier(5));
    reader.visit = 3;
    let read = reader.run(&small_site());
    assert_eq!(read.order().len(), 7);
    // Pages four and seven begin a visit: nothing is open to click on.
    assert_eq!(read.ways()[3], "typed");
    assert_eq!(read.ways()[6], "typed");
    assert_eq!(read.ways().iter().filter(|way| **way == "typed").count(), 3);
}

#[test]
fn a_page_found_too_deep_is_read_when_a_shorter_way_turns_up() {
    // /d is three links from the seed by /a and /c, and two by /b.
    let pages = Site::of(&[
        ("/", &["/a", "/b"]),
        ("/a", &["/c"]),
        ("/c", &["/d"]),
        ("/b", &["/d"]),
        ("/d", &["/e"]),
    ]);
    let mut reader = Reader::new(frontier(2));
    for _ in 0..3 {
        assert!(reader.step(&pages, SITE));
    }
    assert_eq!(reader.order(), ["/", "/a", "/c"]);
    assert_eq!(
        reader.entry("/d").state,
        State::Skipped(Left::Scope(Skip::TooDeep))
    );
    assert_eq!(reader.entry("/d").depth, 3);
    assert_eq!(reader.frontier.counts().skipped["too_deep"], 1);

    let read = reader.run(&pages);
    assert_eq!(read.order(), ["/", "/a", "/c", "/b", "/d"]);
    assert_eq!(read.ways()[3..], ["back 2", "click"]);
    assert_eq!(read.entry("/d").depth, 2);
    // What /d links to is three links away and stays unread.
    assert_eq!(
        read.entry("/e").state,
        State::Skipped(Left::Scope(Skip::TooDeep))
    );
    assert_eq!(read.entry("/e").depth, 3);
}

#[test]
fn a_shorter_way_to_a_page_that_was_read_lifts_what_it_links_to() {
    // /s is read three links deep, so /t is too deep; then /q shows that
    // /s is two links from the seed, and /t three.
    let pages = Site::of(&[
        ("/", &["/p", "/q"]),
        ("/p", &["/r"]),
        ("/r", &["/s"]),
        ("/s", &["/t"]),
        ("/t", &["/u"]),
        ("/q", &["/s"]),
    ]);
    let read = Reader::new(frontier(3)).run(&pages);
    assert_eq!(read.order(), ["/", "/p", "/r", "/s", "/q", "/t"]);
    assert_eq!(read.entry("/s").depth, 2);
    assert_eq!(read.entry("/t").depth, 3);
    assert_eq!(read.entry("/u").depth, 4);
    assert_eq!(
        read.entry("/u").state,
        State::Skipped(Left::Scope(Skip::TooDeep))
    );
    // /t was out of the tab's reach by then: typed.
    assert_eq!(read.ways().last(), Some(&"typed"));
}

/// A site of `count` pages with links drawn from a seed.
fn drawn_site(seed: u64, count: usize) -> Site {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut draw = |below: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        usize::try_from(state % u64::try_from(below).unwrap()).unwrap()
    };
    let name = |page: usize| {
        if page == 0 {
            "/".to_owned()
        } else {
            format!("/p{page}")
        }
    };
    let mut site = Site::default();
    for page in 0..count {
        let links = (0..=draw(3))
            .map(|_| {
                let to = draw(count);
                // Now and then a link of the navigation.
                if draw(4) == 0 {
                    format!("~{}", name(to))
                } else {
                    name(to)
                }
            })
            .collect();
        site.links.insert(name(page), links);
    }
    site
}

/// The least number of links from `/` to every page of a site.
fn distances(site: &Site) -> BTreeMap<String, u32> {
    let mut depth = BTreeMap::from([("/".to_owned(), 0_u32)]);
    let mut queue = VecDeque::from(["/".to_owned()]);
    while let Some(page) = queue.pop_front() {
        let here = depth[&page];
        for spec in site.links.get(&page).into_iter().flatten() {
            let to = spec.trim_start_matches('~').to_owned();
            if !depth.contains_key(&to) {
                depth.insert(to.clone(), here + 1);
                queue.push_back(to);
            }
        }
    }
    depth
}

#[test]
fn the_pages_inside_the_depth_are_the_same_in_every_order() {
    for seed in 1..=60_u64 {
        let pages = drawn_site(seed, 45);
        let near = distances(&pages);
        for max_depth in [1, 3] {
            let wanted: BTreeSet<&str> = near
                .iter()
                .filter(|(_, depth)| **depth <= max_depth)
                .map(|(page, _)| page.as_str())
                .collect();
            for (back_limit, visit) in [(3, usize::MAX), (0, usize::MAX), (3, 4), (10, 7)] {
                let mut reader = Reader::new(frontier_with(
                    &request(max_depth),
                    Limits {
                        back_limit,
                        ..limits()
                    },
                    &["/"],
                ));
                reader.visit = visit;
                let read = reader.run(&pages);
                let got: BTreeSet<&str> = read.order().into_iter().collect();
                assert_eq!(
                    got, wanted,
                    "seed {seed}, depth {max_depth}, back {back_limit}"
                );
                // Every page was read once, and has the depth of its
                // shortest way.
                assert_eq!(read.order().len(), wanted.len(), "seed {seed}");
                for page in &wanted {
                    assert_eq!(read.entry(page).depth, near[*page], "seed {seed}: {page}");
                }
                // And everything the crawl knows and did not read is one
                // link too far.
                for entry in read.frontier.entries() {
                    if entry.state != State::Read {
                        assert_eq!(entry.state, State::Skipped(Left::Scope(Skip::TooDeep)));
                        assert_eq!(entry.depth, max_depth + 1, "seed {seed}: {}", entry.key);
                    }
                }
            }
        }
    }
}

#[test]
fn a_crawl_asks_for_no_more_pages_than_it_may() {
    let pages = small_site();
    let mut reader = Reader::new(frontier_with(
        &request(5),
        Limits {
            max_pages: 3,
            ..limits()
        },
        &["/"],
    ));
    assert!(reader.frontier.may_begin());
    while reader.step(&pages, SITE) {}
    assert_eq!(reader.order(), ["/", "/a", "/a1"]);
    assert!(!reader.frontier.may_begin());
    assert_eq!(reader.frontier.next(SITE, &reader.tab), None);
    let counts = reader.frontier.counts();
    // What was found and not read still waits: the crawl stopped at its
    // cap, not at the end of the site.
    assert_eq!((counts.begun, counts.read, counts.waiting), (3, 3, 3));
    assert_eq!(counts.discovered, 6);

    // A page that was begun and could not be attempted goes back: it does
    // not count, and it is the next one again.
    let mut frontier = frontier_with(
        &request(5),
        Limits {
            max_pages: 1,
            ..limits()
        },
        &["/"],
    );
    let pick = frontier.next(SITE, &[]).unwrap();
    assert!(frontier.begin(pick.entry));
    assert!(!frontier.begin(pick.entry), "a page is begun once");
    assert_eq!(frontier.entry(pick.entry).unwrap().state, State::Reading);
    assert_eq!(frontier.next(SITE, &[]), None);
    frontier.put_back(pick.entry);
    frontier.put_back(pick.entry);
    assert_eq!(frontier.counts().begun, 0);
    assert_eq!(frontier.next(SITE, &[]), Some(pick.clone()));
    // A page that failed counts, and its links stay unknown.
    assert!(frontier.begin(pick.entry));
    frontier.record_failed(pick.entry);
    let counts = frontier.counts();
    assert_eq!((counts.begun, counts.failed, counts.discovered), (1, 1, 1));
    assert_eq!(frontier.next(SITE, &[]), None);
}

#[test]
fn what_the_scope_refuses_is_counted_by_its_reason_and_never_read() {
    let pages = Site::of(&[(
        "/",
        &[
            "/articles/one",
            "/articles/drafts/two",
            "/contact",
            "https://elsewhere.example/page",
            "/articles/report.pdf",
            "!/trap",
            "!/trap?from=hidden",
            "/articles/one#comments",
            "/articles/one?utm_source=home",
            "/",
        ],
    )]);
    let read = Reader::new(frontier_with(
        &ScopeRequest {
            allow: vec!["/articles/".to_owned()],
            deny: vec!["/drafts/".to_owned()],
            ..request(5)
        },
        limits(),
        &["/"],
    ))
    .run(&pages);

    // The seed is read though no `allow` pattern names it.
    assert_eq!(read.order(), ["/", "/articles/one"]);
    let counts = read.frontier.counts();
    let skipped: Vec<(&str, u64)> = counts
        .skipped
        .iter()
        .map(|(reason, count)| (*reason, *count))
        .collect();
    assert_eq!(
        skipped,
        [
            ("denied", 1),
            ("hidden", 2),
            ("not_a_page", 1),
            ("not_allowed", 1),
            ("off_site", 1),
        ]
    );
    // The link with a fragment and the one with a campaign tag are the
    // page itself, and so is the page's link to itself: eight addresses.
    assert_eq!(counts.discovered, 8);
    // The seed's links a reader can follow: five distinct addresses, the
    // hidden two not among them.
    assert_eq!(read.entry("/").links.len(), 5);
    assert_eq!(
        read.entry("/trap").state,
        State::Skipped(Left::Scope(Skip::Hidden))
    );
    // What is asked of the site is the link as it was found.
    assert_eq!(
        read.entry("/articles/one").url.as_str(),
        "https://shop.example/articles/one"
    );
}

#[test]
fn a_hidden_link_is_followed_once_a_page_shows_it() {
    let pages = Site::of(&[
        ("/", &["!/special", "/a"]),
        ("/a", &["/special", "!/a"]),
        ("/special", &[]),
    ]);
    let mut reader = Reader::new(frontier(5));
    assert!(reader.step(&pages, SITE));
    assert_eq!(
        reader.entry("/special").state,
        State::Skipped(Left::Scope(Skip::Hidden))
    );
    let read = reader.run(&pages);
    assert_eq!(read.order(), ["/", "/a", "/special"]);
    assert_eq!(read.ways(), ["typed", "click", "click"]);
    // It is where a reader could first see it: two links from the seed.
    assert_eq!(read.entry("/special").depth, 2);
    assert_eq!(
        read.entry("/special").found_on,
        read.frontier.find(&url("/a"))
    );
    // A hidden link to a page that is known changes nothing about it.
    assert_eq!(read.entry("/a").state, State::Read);
    assert!(read.frontier.counts().skipped.is_empty());
}

#[test]
fn a_redirect_is_one_page_whatever_the_address() {
    // /old leads to /a; so does /older.
    let pages = Site::of(&[
        ("/", &["/a", "/old", "/older"]),
        ("/a", &["/a1"]),
        ("/a1", &[]),
    ])
    .redirect("/old", "/a")
    .redirect("/older", "/a");
    let read = Reader::new(frontier(5)).run(&pages);
    // /a is read under its own address first; the two old addresses are
    // asked for (nobody knows where they lead) and are that page again.
    assert_eq!(read.order(), ["/", "/a", "/a1", "/old", "/older"]);
    let a = read.frontier.find(&url("/a"));
    for old in ["/old", "/older"] {
        let entry = read.entry(old);
        assert_eq!(entry.state, State::Read);
        assert_eq!(entry.duplicate_of, a, "{old}");
        assert_eq!(entry.final_url, Some(url("/a")));
        assert_eq!(entry.links, Vec::<EntryId>::new());
    }
    assert_eq!(read.frontier.counts().duplicates, 2);

    // The other way round: the old address is read first. The page it
    // leads to is not asked for a second time under its own address, and
    // a link to that address is a link to the page that was read.
    let pages = Site::of(&[
        ("/", &["/old", "/new"]),
        ("/new", &["/deeper"]),
        ("/deeper", &["/new"]),
    ])
    .redirect("/old", "/new");
    let read = Reader::new(frontier(5)).run(&pages);
    assert_eq!(read.order(), ["/", "/old", "/deeper"]);
    assert_eq!(read.entry("/new").state, State::Skipped(Left::AlreadyRead));
    assert_eq!(read.frontier.counts().skipped["already_read"], 1);
    assert_eq!(read.entry("/old").final_url, Some(url("/new")));
    assert_eq!(read.entry("/old").duplicate_of, None);
    // The click on /deeper happens on the page the browser shows: /new.
    assert_eq!(read.ways(), ["typed", "click", "click"]);
}

#[test]
fn a_redirect_to_an_address_nobody_linked_is_known_by_it_afterwards() {
    let pages = Site::of(&[
        ("/", &["/go", "/b"]),
        ("/landed", &["/x"]),
        ("/b", &["/landed"]),
        ("/x", &[]),
    ])
    .redirect("/go", "/landed");
    let read = Reader::new(frontier(5)).run(&pages);
    // /b links to /landed, which was read as the end of /go: no entry of
    // its own, and nothing is asked for twice.
    assert_eq!(read.order(), ["/", "/go", "/x", "/b"]);
    assert_eq!(read.frontier.counts().discovered, 4);
    assert_eq!(
        read.frontier.find(&url("/landed")),
        read.frontier.find(&url("/go"))
    );
}

#[test]
fn a_redirect_off_the_site_is_kept_and_not_read_on_from() {
    let pages = Site::of(&[
        ("/", &["/out", "/a"]),
        ("/a", &[]),
        (
            "https://elsewhere.example/landing",
            &["https://elsewhere.example/more"],
        ),
    ])
    .redirect("/out", "https://elsewhere.example/landing");
    let read = Reader::new(frontier(5)).run(&pages);
    assert_eq!(read.order(), ["/", "/out", "/a"]);
    let out = read.entry("/out");
    assert!(out.left_site);
    assert_eq!(out.state, State::Read);
    assert_eq!(out.links, Vec::<EntryId>::new());
    assert_eq!(read.frontier.counts().discovered, 3);
    // The page after it is not clicked on a page of another site.
    assert_eq!(read.ways(), ["typed", "click", "back 1"]);
}

#[test]
fn pages_that_say_the_same_are_flagged() {
    // An article of four hundred words.
    let article = (0..10)
        .map(|part| text_of(&format!("the article, part {part}")))
        .collect::<Vec<_>>()
        .join(" ");
    // The same article with another date line and a word more.
    let revised = {
        let mut words: Vec<&str> = article.split(' ').collect();
        words.insert(200, "moreover");
        format!("updated {}", words.join(" "))
    };
    let pages = Site::of(&[
        (
            "/",
            &[
                "/one", "/copy", "/revised", "/short-1", "/short-2", "/empty-1", "/empty-2",
            ],
        ),
        ("/one", &["/from-one"]),
        ("/copy", &["/only-from-the-copy"]),
        ("/revised", &["/from-revised"]),
    ])
    .saying("/one", &article)
    .saying("/copy", &format!("  {}\n", article.replace(' ', "  ")))
    .saying("/revised", &revised)
    .saying("/short-1", "Not found")
    .saying("/short-2", "Not found")
    .saying("/empty-1", "")
    .saying("/empty-2", " ");
    let read = Reader::new(frontier_with(
        &request(5),
        Limits {
            // Wide enough for one word in forty changed.
            near_duplicate_bits: 12,
            ..limits()
        },
        &["/"],
    ))
    .run(&pages);

    let one = read.frontier.find(&url("/one"));
    // Word for word the same, however the white space is set: a duplicate.
    // Its links are the original's and are not followed from it.
    let copy = read.entry("/copy");
    assert_eq!(copy.duplicate_of, one);
    assert_eq!(copy.content_hash, read.entry("/one").content_hash);
    assert_eq!(copy.links, Vec::<EntryId>::new());
    assert!(read.frontier.find(&url("/only-from-the-copy")).is_none());
    // Nearly the same: flagged with the bits between them, and read on
    // from like any page.
    let revised = read.entry("/revised");
    let (of, bits) = revised.near_duplicate_of.unwrap();
    assert_eq!(Some(of), one);
    assert!((1..=12).contains(&bits), "{bits}");
    assert_eq!(revised.duplicate_of, None);
    assert!(read.order().contains(&"/from-revised"));
    // Two short pages with one text are duplicates by their hash; a page
    // of a few words has no fingerprint and is nobody's near-duplicate.
    assert_eq!(
        read.entry("/short-2").duplicate_of,
        read.frontier.find(&url("/short-1"))
    );
    assert_eq!(read.entry("/short-1").simhash, None);
    assert_eq!(read.entry("/short-1").near_duplicate_of, None);
    // Two pages that say nothing are not copies of each other.
    assert_eq!(read.entry("/empty-2").duplicate_of, None);
    // Pages with texts of their own are nobody's.
    assert_eq!(read.entry("/from-one").near_duplicate_of, None);
    let counts = read.frontier.counts();
    assert_eq!((counts.duplicates, counts.near_duplicates), (2, 1));
}

#[test]
fn a_near_duplicate_names_the_nearest_page_inside_the_limit() {
    let article = text_of("another article");
    // Its first thirty words, and ten of another text.
    let nearly = format!(
        "{} {}",
        article.split(' ').take(30).collect::<Vec<_>>().join(" "),
        text_of("an ending")
            .split(' ')
            .take(10)
            .collect::<Vec<_>>()
            .join(" ")
    );
    let pages = Site::of(&[("/", &["/first", "/second"])])
        .saying("/first", &article)
        .saying("/second", &nearly);
    // With no bits allowed only the same fingerprint is near.
    let strict = Reader::new(frontier_with(
        &request(5),
        Limits {
            near_duplicate_bits: 0,
            ..limits()
        },
        &["/"],
    ))
    .run(&pages);
    let apart = crate::simhash::distance(
        strict.entry("/first").simhash.unwrap(),
        strict.entry("/second").simhash.unwrap(),
    );
    assert!(apart > 0, "the two texts have one fingerprint");
    assert_eq!(strict.entry("/second").near_duplicate_of, None);
    // At exactly the distance between them it is flagged; one bit less,
    // not.
    for (bits, flagged) in [(apart, true), (apart - 1, false)] {
        let read = Reader::new(frontier_with(
            &request(5),
            Limits {
                near_duplicate_bits: bits,
                ..limits()
            },
            &["/"],
        ))
        .run(&pages);
        assert_eq!(
            read.entry("/second").near_duplicate_of.is_some(),
            flagged,
            "{bits} of {apart}"
        );
    }
}

#[test]
fn what_a_sitemap_names_is_an_entry_point() {
    let pages = Site::of(&[
        ("/", &["/a"]),
        ("/a", &["/b"]),
        ("/b", &["/c"]),
        ("/c", &["/d"]),
        ("/only", &["/below"]),
        ("/below", &[]),
    ]);
    let mut frontier = frontier(1);
    let changed = Utc.with_ymd_and_hms(2026, 10, 5, 8, 30, 0).unwrap();
    let only = frontier
        .add_from_sitemap(&url("/only"), Some(changed))
        .unwrap();
    // On another site, or a file: named by the sitemap, refused by the
    // scope like a link.
    let off = frontier
        .add_from_sitemap(&url("https://elsewhere.example/x"), None)
        .unwrap();
    let file = frontier.add_from_sitemap(&url("/list.pdf"), None).unwrap();
    assert_eq!(frontier.entry(only).unwrap().source, Source::Sitemap);
    assert_eq!(frontier.entry(only).unwrap().depth, 0);
    assert_eq!(frontier.entry(only).unwrap().lastmod, Some(changed));
    assert_eq!(
        frontier.entry(off).unwrap().state,
        State::Skipped(Left::Scope(Skip::OffSite))
    );
    assert_eq!(
        frontier.entry(file).unwrap().state,
        State::Skipped(Left::Scope(Skip::NotAPage))
    );
    // The seed again: it stays the seed, and takes the date.
    let seed = frontier.add_from_sitemap(&url("/"), Some(changed)).unwrap();
    assert_eq!(frontier.entry(seed).unwrap().source, Source::Seed);
    assert_eq!(frontier.entry(seed).unwrap().lastmod, Some(changed));

    let mut reader = Reader::new(frontier);
    // The seed first, then what it links to; the sitemap's page is typed
    // when the tab has nothing left to click, and is read on from by a
    // click.
    for _ in 0..2 {
        assert!(reader.step(&pages, SITE));
    }
    assert_eq!(
        reader.entry("/b").state,
        State::Skipped(Left::Scope(Skip::TooDeep))
    );
    // A sitemap names a page the crawl found too deep: it is an entry
    // point now, and what it links to is one link from one.
    reader.frontier.add_from_sitemap(&url("/b"), None).unwrap();
    assert_eq!(reader.entry("/b").depth, 0);
    assert_eq!(reader.entry("/b").source, Source::Link);
    let read = reader.run(&pages);
    assert_eq!(read.order(), ["/", "/a", "/b", "/c", "/only", "/below"]);
    assert_eq!(
        read.ways(),
        ["typed", "click", "click", "click", "typed", "click"]
    );
    assert_eq!(read.entry("/c").depth, 1);
    assert_eq!(read.entry("/d").depth, 2);
    assert_eq!(read.entry("/below").depth, 1);
}

#[test]
fn sitemap_entries_can_be_given_a_depth_of_their_own() {
    let mut frontier = frontier_with(
        &request(1),
        Limits {
            sitemap_depth: 1,
            ..limits()
        },
        &["/"],
    );
    let listed = frontier.add_from_sitemap(&url("/listed"), None).unwrap();
    assert_eq!(frontier.entry(listed).unwrap().depth, 1);
    assert_eq!(frontier.entry(listed).unwrap().state, State::Waiting);
    // Where the crawl does not go that deep, the sitemap's page is too
    // deep like any other.
    let mut shallow = frontier_with(
        &request(0),
        Limits {
            sitemap_depth: 1,
            ..limits()
        },
        &["/"],
    );
    let listed = shallow.add_from_sitemap(&url("/listed"), None).unwrap();
    assert_eq!(
        shallow.entry(listed).unwrap().state,
        State::Skipped(Left::Scope(Skip::TooDeep))
    );
}

#[test]
fn a_full_frontier_counts_what_it_forgets() {
    let pages = Site::of(&[("/", &["/a", "/b", "/c", "/d"])]);
    let mut reader = Reader::new(frontier_with(
        &request(5),
        Limits {
            max_entries: 3,
            ..limits()
        },
        &["/"],
    ));
    assert!(reader.step(&pages, SITE));
    let counts = reader.frontier.counts();
    assert_eq!((counts.discovered, counts.dropped), (3, 2));
    assert_eq!(reader.entry("/").links.len(), 2);
    assert!(reader.frontier.find(&url("/c")).is_none());
    // A seed and a sitemap's address are forgotten like any other.
    assert_eq!(reader.frontier.add_seed(&url("/late")), None);
    assert_eq!(reader.frontier.add_from_sitemap(&url("/later"), None), None);
    assert_eq!(reader.frontier.counts().dropped, 4);
    // One the crawl knows is still found.
    assert!(reader.frontier.add_seed(&url("/a")).is_some());
}

#[test]
fn a_frontier_goes_on_where_it_was_stored() {
    let pages = Site::of(&[
        ("/", &["/go", "/a", "/b"]),
        ("/landed", &["/x"]),
        ("/a", &["/landed", "/a1"]),
        ("/a1", &[]),
        ("/b", &[]),
        ("/x", &[]),
    ])
    .redirect("/go", "/landed");
    let whole = Reader::new(frontier(5)).run(&pages);

    let mut reader = Reader::new(frontier(5));
    for _ in 0..3 {
        assert!(reader.step(&pages, SITE));
    }
    // A page is being read when the crawl changes hands.
    let pick = reader.frontier.next(SITE, &reader.tab).unwrap();
    assert!(reader.frontier.begin(pick.entry));
    let changed = reader.frontier.take_changed();
    assert_ne!(changed, Vec::<EntryId>::new());
    assert!(changed.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(reader.frontier.take_changed(), Vec::<EntryId>::new());

    let counts = reader.frontier.counts();
    let mut restored = Frontier::restore(
        Scope::new(&request(5), &[url("/")]).unwrap(),
        limits(),
        NormalizeOptions::default(),
        reader.frontier.entries().to_vec(),
        counts.begun,
        counts.dropped,
    );
    assert_eq!(restored.counts(), counts);
    assert_eq!(restored.take_changed(), Vec::<EntryId>::new());
    // The end of a redirect is still known by the page that led to it.
    assert_eq!(restored.find(&url("/landed")), restored.find(&url("/go")));
    // Nobody reads the page that was being read: it waits again and is
    // the next one, by the same way.
    assert_eq!(restored.entry(pick.entry).unwrap().state, State::Reading);
    restored.put_back(pick.entry);
    assert_eq!(restored.next(SITE, &reader.tab), Some(pick));

    let mut resumed = Reader::new(restored);
    resumed.tab.clone_from(&reader.tab);
    resumed.log.clone_from(&reader.log);
    let resumed = resumed.run(&pages);
    assert_eq!(resumed.order(), whole.order());
    assert_eq!(resumed.ways(), whole.ways());
    assert_eq!(resumed.frontier.counts(), whole.frontier.counts());
}

#[test]
fn every_site_of_a_crawl_has_its_own_readers() {
    let pages = Site::of(&[
        (
            "/",
            &[
                "https://blog.shop.example/post",
                "https://other.example/page",
                "/a",
            ],
        ),
        (
            "https://other.example/page",
            &["https://other.example/more"],
        ),
    ]);
    let mut reader = Reader::new(frontier_with(
        &ScopeRequest {
            same_site_only: false,
            ..request(5)
        },
        limits(),
        &["/"],
    ));
    assert_eq!(reader.frontier.sites_waiting(), [SITE]);
    assert!(reader.step(&pages, SITE));
    // Another host of the site is the site; another site is another.
    assert_eq!(reader.frontier.sites_waiting(), [SITE, "other.example"]);
    assert_eq!(reader.entry("https://blog.shop.example/post").site, SITE);
    // The visitor of one site is never sent to a page of another, however
    // near the link is.
    let next = reader.frontier.next(SITE, &reader.tab).unwrap();
    assert_eq!(
        reader.frontier.entry(next.entry).unwrap().url.host_str(),
        Some("blog.shop.example")
    );
    // The visitor of the other site has a tab of its own, with nothing
    // in it yet: its first page there is typed.
    let other = reader.frontier.next("other.example", &[]).unwrap();
    assert_eq!(other.way, Way::Typed);
    assert_eq!(reader.frontier.next("nowhere.example", &[]), None);
    while reader.step(&pages, SITE) {}
    assert_eq!(reader.frontier.sites_waiting(), ["other.example"]);
    reader.tab.clear();
    while reader.step(&pages, "other.example") {}
    assert_eq!(reader.ways()[3..], ["typed", "click"]);
    assert_eq!(reader.frontier.sites_waiting(), Vec::<String>::new());
    assert_eq!(reader.frontier.counts().read, 5);
}

#[test]
fn names_of_sources_and_reasons_are_read_back() {
    for source in [Source::Seed, Source::Sitemap, Source::Link] {
        assert_eq!(Source::parse(source.as_str()), Some(source));
    }
    assert_eq!(Source::parse("guess"), None);
    for left in
        Skip::ALL
            .into_iter()
            .map(Left::Scope)
            .chain([Left::AlreadyRead, Left::Guard, Left::Robots])
    {
        assert_eq!(Left::parse(left.as_str()), Some(left));
    }
    assert_eq!(Left::AlreadyRead.as_str(), "already_read");
    assert_eq!(Left::Guard.as_str(), "ssrf_denied");
    assert_eq!(Left::Robots.as_str(), "robots_denied");
    assert_eq!(Left::parse("because"), None);
}

#[test]
fn nothing_happens_to_an_entry_that_is_not_there() {
    let mut frontier = frontier(5);
    assert!(!frontier.begin(99));
    frontier.put_back(99);
    frontier.record_failed(99);
    let recorded = frontier.record_read(
        99,
        &PageRead {
            final_url: url("/"),
            links: Vec::new(),
            links_truncated: false,
            text: String::new(),
        },
    );
    assert_eq!(recorded, super::Recorded::default());
    assert_eq!(frontier.entry(99), None);
    assert_eq!(frontier.counts().discovered, 1);
}

#[test]
fn an_address_the_guard_refuses_is_left_out_and_never_counted_as_asked() {
    let mut frontier = frontier(5);
    let seed = frontier.find(&url("/")).unwrap();
    let site = frontier.entry(seed).unwrap().site.clone();
    assert!(frontier.next(&site, &[]).is_some());
    frontier.take_changed();

    assert!(frontier.refuse(seed));

    assert_eq!(
        frontier.entry(seed).unwrap().state,
        State::Skipped(Left::Guard)
    );
    assert_eq!(frontier.take_changed(), [seed]);
    let counts = frontier.counts();
    assert_eq!(counts.skipped["ssrf_denied"], 1);
    assert_eq!((counts.begun, counts.waiting), (0, 0));
    // Nobody reads it afterwards, and it is refused once.
    assert_eq!(frontier.next(&site, &[]), None);
    assert!(!frontier.begin(seed));
    assert!(!frontier.refuse(seed));
    assert!(!frontier.refuse(99));
    assert_eq!(frontier.take_changed(), Vec::<usize>::new());
}

#[test]
fn an_address_a_robots_txt_disallows_is_left_out_and_never_counted_as_asked() {
    let mut frontier = frontier(5);
    let site = frontier.entry(0).unwrap().site.clone();
    let home = frontier.next(&site, &[]).unwrap();
    frontier.begin(home.entry);
    frontier.record_read(
        home.entry,
        &PageRead {
            final_url: url("/"),
            links: vec![link("/private/a"), link("/open")],
            links_truncated: false,
            text: text_of("home"),
        },
    );
    let private = frontier.find(&url("/private/a")).unwrap();
    frontier.take_changed();

    assert!(frontier.disallow(private));

    assert_eq!(
        frontier.entry(private).unwrap().state,
        State::Skipped(Left::Robots)
    );
    assert_eq!(frontier.take_changed(), [private]);
    let counts = frontier.counts();
    assert_eq!(counts.skipped["robots_denied"], 1);
    // It was never asked for: one page was, and one still waits.
    assert_eq!((counts.begun, counts.waiting), (1, 1));
    // The reader goes on to what is allowed, and never there.
    let next = frontier.next(&site, &[home.entry]).unwrap();
    assert_eq!(next.url, url("/open"));
    assert!(!frontier.begin(private));
    assert!(!frontier.disallow(private));
    assert!(!frontier.disallow(home.entry));
    assert!(!frontier.disallow(99));
}

#[test]
fn a_link_is_followed_under_the_address_its_page_gives() {
    let mut frontier = frontier(5);
    let site = frontier.entry(0).unwrap().site.clone();
    let home = frontier.next(&site, &[]).unwrap();
    assert_eq!((home.way.clone(), home.url.clone()), (Way::Typed, url("/")));
    frontier.begin(home.entry);
    // The home page names an article with a campaign tag and the listing;
    // the listing names the same article without the tag, with its query
    // in another order, and the home page with a fragment.
    frontier.record_read(
        home.entry,
        &PageRead {
            final_url: url("/"),
            links: vec![link("/list"), link("/a?utm_source=home&b=2&a=1")],
            links_truncated: false,
            text: text_of("home"),
        },
    );
    let list = frontier.next(&site, &[home.entry]).unwrap();
    assert_eq!(list.url, url("/list"));
    frontier.begin(list.entry);
    frontier.record_read(
        list.entry,
        &PageRead {
            final_url: url("/list"),
            links: vec![link("/a?a=1&b=2"), link("/a?b=2&a=1#top"), link("/#top")],
            links_truncated: false,
            text: text_of("list"),
        },
    );

    // One article, whatever the spelling; first met with the tag.
    let article = frontier.find(&url("/a?a=1&b=2")).unwrap();
    assert_eq!(
        frontier.entry(article).unwrap().url,
        url("/a?utm_source=home&b=2&a=1")
    );
    assert_eq!(frontier.counts().discovered, 3);
    // From the listing it is asked for as the listing has it: a browser
    // finds the link it is told to click, and the site sees the address
    // its own page gave. The first of two links is the one a reader meets,
    // and a fragment is never sent.
    let pick = frontier.next(&site, &[home.entry, list.entry]).unwrap();
    assert_eq!(pick.entry, article);
    assert_eq!(
        pick.way,
        Way::Link {
            from: url("/list"),
            backs: 0
        }
    );
    assert_eq!(pick.url, url("/a?a=1&b=2"));
    // From the home page, one step back, it is the address with the tag.
    let from_home = frontier.next(&site, &[list.entry, home.entry]).unwrap();
    assert_eq!(from_home.url, url("/a?utm_source=home&b=2&a=1"));
    // Typed, it is the address the crawl first met.
    let typed = frontier.next(&site, &[]).unwrap();
    assert_eq!(
        (typed.way, typed.url),
        (Way::Typed, url("/a?utm_source=home&b=2&a=1"))
    );
    // Only what is spelled otherwise is kept, each target once.
    assert_eq!(
        frontier.entry(list.entry).unwrap().spellings,
        [(article, url("/a?a=1&b=2"))]
    );
    assert_eq!(frontier.entry(home.entry).unwrap().spellings, Vec::new());

    // Read under the listing's spelling, that is the address the browser
    // shows for the article: a link of the article is clicked from there,
    // not from the address with the tag, which no tab holds. It is one
    // page all the same, and no redirect.
    frontier.begin(article);
    let recorded = frontier.record_read(
        article,
        &PageRead {
            final_url: url("/a?a=1&b=2#top"),
            links: vec![link("/a/notes")],
            links_truncated: false,
            text: text_of("article"),
        },
    );
    assert_eq!(recorded.duplicate_of, None);
    let read = frontier.entry(article).unwrap();
    assert_eq!(read.final_url, Some(url("/a?a=1&b=2")));
    assert!(!read.left_site);
    let notes = frontier
        .next(&site, &[home.entry, list.entry, article])
        .unwrap();
    assert_eq!(
        notes.way,
        Way::Link {
            from: url("/a?a=1&b=2"),
            backs: 0
        }
    );
    assert_eq!(notes.url, url("/a/notes"));
    // A page read at the address the crawl first met shows that address.
    assert_eq!(frontier.entry(list.entry).unwrap().final_url, None);

    // A frontier that is restored follows the same spellings.
    let restored = Frontier::restore(
        Scope::new(&request(5), &[url("/")]).unwrap(),
        limits(),
        NormalizeOptions::default(),
        frontier.entries().to_vec(),
        3,
        0,
    );
    assert_eq!(
        restored
            .next(&site, &[home.entry, list.entry, article])
            .unwrap()
            .way,
        Way::Link {
            from: url("/a?a=1&b=2"),
            backs: 0
        }
    );
}

#[test]
fn a_reader_goes_back_to_the_listing_before_it_takes_the_menu() {
    // A listing of four articles, a menu on every page that leads away.
    let site = Site::of(&[
        ("/", &["~/list", "~/contact", "/list"]),
        (
            "/list",
            &["~/list", "~/contact", "/p1", "/p2", "/p3", "/p4"],
        ),
        ("/p1", &["~/list", "~/contact"]),
        ("/p2", &["~/list", "~/contact", "/p2/more"]),
        ("/p2/more", &["~/list", "~/contact"]),
        ("/p3", &["~/list", "~/contact"]),
        ("/p4", &["~/list", "~/contact"]),
        ("/contact", &["~/list", "~/contact"]),
    ]);

    let read = Reader::new(frontier(5)).run(&site);

    // Every article is a click on the listing: after each one the reader
    // goes back, however many pages the article led on to (two back from
    // /p2/more). The menu's link, which was on every page all along, is
    // taken when the listing has nothing left, and nothing but the seed is
    // typed. Taking the menu's link first would have left the listing
    // behind after the first article.
    assert_eq!(
        read.order(),
        [
            "/", "/list", "/p1", "/p2", "/p2/more", "/p3", "/p4", "/contact"
        ]
    );
    assert_eq!(
        read.ways(),
        [
            "typed", "click", "click", "back 1", "click", "back 2", "back 1", "click"
        ]
    );
    // Where the text ends and the menu begins is the page's own: the
    // listing is a link of the home page's text, and of every menu.
    let home = read.entry("/");
    assert_eq!((home.links.len(), home.text_links), (2, 1));
    let article = read.entry("/p1");
    assert_eq!((article.links.len(), article.text_links), (2, 0));
    let listing = read.entry("/list");
    assert_eq!((listing.links.len(), listing.text_links), (5, 4));
}
