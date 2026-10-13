//! The main content of a page: what a reader would call the page itself,
//! without what the site wraps around it (spec §14).
//!
//! The rules are those of Mozilla's Readability, which descend from Arc90's:
//! every paragraph gives points to the elements around it for its length
//! and its commas, the names of an element (`class`, `id`) raise or lower
//! it, links count against it, and the element with the most points is the
//! content, together with those of its siblings that read like it. What is
//! left inside is then cleaned of what is mostly links, forms or buttons.
//! A pass that finds less than a few sentences is made again with fewer of
//! the rules, and the pass that found the most text stands.
//!
//! Readability rewrites the document as it goes; this module does not. The
//! tree stays as the page reader parsed it, and what is no content is a set
//! of node ids. Where Readability wraps loose text of a `div` into a
//! paragraph, or turns a run of text between two `<br>` into one, the same
//! run of text is scored here as if it had been wrapped.
//!
//! Three things are Kage's own. Navigation, side bars, footers, dialogs
//! and the like are never content, whatever their score: where the markup
//! says what an element is, nothing is guessed (the page reader's rule for
//! what a site repeats on every page). What the markup hides is never
//! content, as it is never text of the page. And a page without a
//! candidate has no content: Readability then takes the whole body, which
//! on the shell of a script-driven page is its "Loading…".

use std::collections::HashMap;

use ego_tree::NodeId;

use crate::dom::{
    Dropped, NodeRef, Step, as_element, as_text, chars_of, heading_level, is_block, names_of,
    text_under, unseen, walk,
};

/// A pass that finds fewer characters than this is made again with fewer
/// rules: Readability's own threshold.
const ENOUGH_CHARS: usize = 500;

/// A paragraph shorter than this gives no points.
const MIN_PARAGRAPH_CHARS: usize = 25;

/// How far up a paragraph's points go.
const SCORED_ANCESTORS: usize = 5;

/// How many of the best candidates are looked at for a common ancestor.
const TOP_CANDIDATES: usize = 5;

/// How far above the chosen element a shape that repeats is looked for.
const REPEATED_LEVELS: usize = 5;

/// A byline is shorter than this, in characters: Readability's bound.
const MAX_BYLINE_CHARS: usize = 100;

/// Names of a line that says who wrote a page, or when.
const BYLINE: [&str; 5] = ["byline", "author", "dateline", "writtenby", "p-author"];

/// Names that make an element unlikely to be content, unless one of
/// [`MAYBE`] is there too.
const UNLIKELY: [&str; 31] = [
    "-ad-",
    "ai2html",
    "banner",
    "breadcrumbs",
    "combx",
    "comment",
    "community",
    "cover-wrap",
    "disqus",
    "extra",
    "footer",
    "gdpr",
    "header",
    "legends",
    "menu",
    "related",
    "remark",
    "replies",
    "rss",
    "shoutbox",
    "sidebar",
    "skyscraper",
    "social",
    "sponsor",
    "supplemental",
    "ad-break",
    "agegate",
    "pagination",
    "pager",
    "popup",
    "yom-remote",
];

/// Names that keep an element with an unlikely name.
const MAYBE: [&str; 8] = [
    "and", "article", "body", "column", "content", "main", "mathjax", "shadow",
];

/// Names that raise an element.
const POSITIVE: [&str; 13] = [
    "article",
    "body",
    "content",
    "entry",
    "hentry",
    "h-entry",
    "main",
    "page",
    "pagination",
    "post",
    "text",
    "blog",
    "story",
];

/// Names that lower an element.
const NEGATIVE: [&str; 24] = [
    "-ad-",
    "hidden",
    "banner",
    "combx",
    "comment",
    "com-",
    "contact",
    "footer",
    "gdpr",
    "masthead",
    "media",
    "meta",
    "outbrain",
    "promo",
    "related",
    "scroll",
    "share",
    "shoutbox",
    "sidebar",
    "skyscraper",
    "sponsor",
    "shopping",
    "tags",
    "widget",
];

/// Elements that are what a site repeats on every page, by their name.
const FURNITURE: [&str; 3] = ["nav", "aside", "footer"];

/// The same, by their ARIA role, and what lies over a page.
const FURNITURE_ROLES: [&str; 10] = [
    "navigation",
    "complementary",
    "contentinfo",
    "banner",
    "search",
    "dialog",
    "alertdialog",
    "alert",
    "menu",
    "menubar",
];

/// Elements whose whole text is one paragraph to score.
const SCORED: [&str; 9] = ["section", "h2", "h3", "h4", "h5", "h6", "p", "td", "pre"];

/// Elements a `div` holds when it is more than one paragraph of text.
const NOT_A_PARAGRAPH: [&str; 9] = [
    "blockquote",
    "dl",
    "div",
    "img",
    "ol",
    "p",
    "pre",
    "table",
    "ul",
];

/// Elements that are never content, cleaned out of what was chosen.
const CLEANED: [&str; 8] = [
    "object", "embed", "link", "iframe", "input", "textarea", "select", "button",
];

/// Elements that are judged by what they hold, and taken out when that is
/// mostly links, images or inputs.
const JUDGED: [&str; 6] = ["form", "fieldset", "table", "ul", "div", "td"];

/// Elements that count as text when a block is judged.
const TEXTISH: [&str; 12] = [
    "span",
    "li",
    "td",
    "blockquote",
    "dl",
    "div",
    "img",
    "ol",
    "p",
    "pre",
    "table",
    "ul",
];

/// What a block says when it is only a label of an advertisement or of
/// something still loading.
const FILLER: [&str; 16] = [
    "ad",
    "advertising",
    "advertisement",
    "pub",
    "publicité",
    "werb",
    "werbung",
    "anzeige",
    "anuncio",
    "реклама",
    "广告",
    "loading",
    "chargement",
    "cargando",
    "загрузка",
    "正在加载",
];

/// The main content of a page.
#[derive(Debug, Clone, Default)]
pub(crate) struct Content<'a> {
    /// The elements that hold it, in document order. Empty when the page
    /// has none.
    pub(crate) parts: Vec<NodeRef<'a>>,
    /// What under them is no content, each with everything under it.
    pub(crate) dropped: Dropped,
}

impl Content<'_> {
    /// How many characters of text the content has.
    pub(crate) fn chars(&self) -> usize {
        self.parts
            .iter()
            .map(|part| chars_of(&text_under(*part, &self.dropped)))
            .sum()
    }
}

/// Which rules a pass applies.
#[derive(Debug, Clone, Copy)]
struct Rules {
    /// Elements with unlikely names are left out before scoring.
    strip_unlikely: bool,
    /// The names of an element raise or lower it.
    weigh_names: bool,
    /// What was chosen is cleaned of blocks that are mostly links.
    clean: bool,
}

/// What a pass knows about an element after measuring the page.
#[derive(Debug, Clone, Copy, Default)]
struct Measure {
    /// Characters of text under it.
    chars: usize,
    /// Characters of that text that stand in links; a link to a place on
    /// the same page counts three tenths.
    linked: f64,
    /// Commas in that text.
    commas: usize,
    /// Whether it is a block or holds one.
    blocky: bool,
    /// Whether it holds what makes a `div` more than a paragraph.
    more_than_a_paragraph: bool,
}

impl Measure {
    fn link_density(&self) -> f64 {
        if self.chars == 0 {
            0.0
        } else {
            self.linked / to_f64(self.chars)
        }
    }
}

#[allow(clippy::cast_precision_loss)] // Lengths of text: far below 2^52.
const fn to_f64(count: usize) -> f64 {
    count as f64
}

fn has_any(names: &str, wanted: &[&str]) -> bool {
    wanted.iter().any(|name| names.contains(name))
}

/// Whether the names hold `hid` as a word of its own: Readability's
/// `^hid$| hid$| hid |^hid `.
fn has_hid(names: &str) -> bool {
    names.split(' ').any(|name| name == "hid")
}

/// What the names of an element add to its score: 25 for a name that
/// sounds like content, 25 off for one that sounds like what surrounds it,
/// for the class and for the id each.
fn name_weight(element: &scraper::node::Element, rules: Rules) -> f64 {
    if !rules.weigh_names {
        return 0.0;
    }
    let mut weight = 0.0;
    for names in [element.attr("class"), element.attr("id")]
        .into_iter()
        .flatten()
    {
        let names = names.to_ascii_lowercase();
        if names.is_empty() {
            continue;
        }
        if has_any(&names, &NEGATIVE) || has_hid(&names) {
            weight -= 25.0;
        }
        if has_any(&names, &POSITIVE) {
            weight += 25.0;
        }
    }
    weight
}

/// What an element starts with when it first gets points.
fn starting_score(element: &scraper::node::Element, rules: Rules) -> f64 {
    let by_tag = match element.name() {
        "div" => 5.0,
        "pre" | "td" | "blockquote" => 3.0,
        "address" | "ol" | "ul" | "dl" | "dd" | "dt" | "li" | "form" => -3.0,
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "th" => -5.0,
        _ => 0.0,
    };
    by_tag + name_weight(element, rules)
}

fn role_of(element: &scraper::node::Element) -> Option<String> {
    element
        .attr("role")
        .map(|role| role.trim().to_ascii_lowercase())
}

/// Whether the markup says an element is what a site wraps around its
/// content, or what lies over it.
fn is_furniture(element: &scraper::node::Element) -> bool {
    FURNITURE.contains(&element.name())
        || role_of(element).is_some_and(|role| FURNITURE_ROLES.contains(&role.as_str()))
}

/// Characters Readability counts as commas.
fn commas_in(text: &str) -> usize {
    text.chars()
        .filter(|c| {
            matches!(
                c,
                ',' | '\u{060C}'
                    | '\u{FE50}'
                    | '\u{FE10}'
                    | '\u{FE11}'
                    | '\u{2E41}'
                    | '\u{2E34}'
                    | '\u{2E32}'
                    | '\u{FF0C}'
            )
        })
        .count()
}

/// Whether an element is a line that says who wrote the page or when: it
/// is marked as the author (`rel`, `itemprop`) or named like a byline, and
/// short. Readability takes the first such line out as the article's
/// byline; here every one goes, because who wrote a page and when is its
/// metadata, not its text.
fn is_byline(node: NodeRef<'_>, element: &scraper::node::Element) -> bool {
    if matches!(element.name(), "html" | "body") {
        return false;
    }
    let marked = element.attr("rel").is_some_and(|rel| {
        rel.split_ascii_whitespace()
            .any(|token| token.eq_ignore_ascii_case("author"))
    }) || element
        .attr("itemprop")
        .is_some_and(|property| property.to_ascii_lowercase().contains("author"))
        || has_any(&names_of(element), &BYLINE);
    marked && (1..MAX_BYLINE_CHARS).contains(&chars_of(&text_under(node, &Dropped::new())))
}

/// Whether two elements have one shape: the same name, and a class in
/// common or none at all.
fn alike(a: NodeRef<'_>, b: NodeRef<'_>) -> bool {
    let (Some(a), Some(b)) = (as_element(a), as_element(b)) else {
        return false;
    };
    let classes = |element: &scraper::node::Element| -> Vec<String> {
        element
            .attr("class")
            .map(|class| class.split_ascii_whitespace().map(str::to_owned).collect())
            .unwrap_or_default()
    };
    let (of_a, of_b) = (classes(a), classes(b));
    a.name() == b.name()
        && ((of_a.is_empty() && of_b.is_empty()) || of_a.iter().any(|class| of_b.contains(class)))
}

/// Whether two elements are of one named kind: the same name and a class
/// in common. Elements without a class are everywhere and say nothing.
fn same_kind(a: NodeRef<'_>, b: NodeRef<'_>) -> bool {
    as_element(a).is_some_and(|element| {
        element
            .attr("class")
            .is_some_and(|class| !class.trim().is_empty())
    }) && alike(a, b)
}

/// The elements that repeat the shape of the chosen one beside it: when
/// the element chosen stands in one of at least three alike (the posts of
/// a thread, the answers to a question), what stands in the same place in
/// each of the others is content like it. Looks up to
/// [`REPEATED_LEVELS`] elements above the chosen one, and returns them all
/// in document order, the chosen one among them.
fn repeated<'a>(
    top: NodeRef<'a>,
    dropped: &Dropped,
    measures: &HashMap<NodeId, Measure>,
) -> Option<Vec<NodeRef<'a>>> {
    let live = |node: &NodeRef<'a>| as_element(*node).is_some() && !dropped.contains(&node.id());
    // From the chosen element up to the child of the one that repeats.
    let mut path: Vec<NodeRef<'a>> = Vec::new();
    let mut one = top;
    for _ in 0..REPEATED_LEVELS {
        let holder = one.parent().filter(|node| as_element(*node).is_some())?;
        path.push(one);
        one = holder;
        let around = one.parent().filter(|node| as_element(*node).is_some())?;
        if around
            .children()
            .filter(live)
            .filter(|other| same_kind(*other, one))
            .count()
            < 3
        {
            continue;
        }
        let mut parts = Vec::new();
        for other in around
            .children()
            .filter(live)
            .filter(|other| same_kind(*other, one))
        {
            if other.id() == one.id() {
                parts.push(top);
                continue;
            }
            let mut at = Some(other);
            for step in path.iter().rev() {
                at = at.and_then(|at| {
                    at.children()
                        .filter(live)
                        .find(|child| alike(*child, *step))
                });
            }
            if let Some(found) = at.filter(|found| {
                measures
                    .get(&found.id())
                    .is_some_and(|known| known.chars >= MIN_PARAGRAPH_CHARS)
            }) {
                parts.push(found);
            }
        }
        if parts.len() >= 3 {
            return Some(parts);
        }
    }
    None
}

/// What is left out before anything is scored: what a reader never sees,
/// what the markup says is furniture, a byline, and, under the rule for
/// it, what has an unlikely name.
fn strip(root: NodeRef<'_>, rules: Rules) -> Dropped {
    let mut dropped = Dropped::new();
    // Depth under a dropped element; under a table; under code.
    let (mut under, mut in_table, mut in_code) = (0_usize, 0_usize, 0_usize);
    walk(root, &Dropped::new(), |step| match step {
        Step::Open(node) => {
            let Some(element) = as_element(node) else {
                return;
            };
            if under > 0 {
                under += 1;
                return;
            }
            let name = element.name();
            let unlikely = || {
                let names = names_of(element);
                rules.strip_unlikely
                    && in_table == 0
                    && in_code == 0
                    && !matches!(name, "html" | "body" | "a")
                    && has_any(&names, &UNLIKELY)
                    && !has_any(&names, &MAYBE)
            };
            if unseen(element) || is_furniture(element) || unlikely() || is_byline(node, element) {
                dropped.insert(node.id());
                under = 1;
                return;
            }
            match name {
                "table" => in_table += 1,
                "code" => in_code += 1,
                _ => {}
            }
        }
        Step::Close(node) => {
            let Some(element) = as_element(node) else {
                return;
            };
            if under > 0 {
                under -= 1;
                return;
            }
            match element.name() {
                "table" => in_table = in_table.saturating_sub(1),
                "code" => in_code = in_code.saturating_sub(1),
                _ => {}
            }
        }
    });
    dropped
}

/// Measures every element that is left.
fn measure(root: NodeRef<'_>, dropped: &Dropped) -> HashMap<NodeId, Measure> {
    let mut measures = HashMap::new();
    let mut open: Vec<Measure> = vec![Measure::default()];
    walk(root, dropped, |step| match step {
        Step::Open(node) => {
            if as_element(node).is_some() {
                open.push(Measure::default());
            } else if let (Some(text), Some(top)) = (as_text(node), open.last_mut()) {
                top.chars += chars_of(text);
                top.commas += commas_in(text);
            }
        }
        Step::Close(node) => {
            let Some(element) = as_element(node) else {
                return;
            };
            let Some(mut own) = open.pop() else {
                return;
            };
            let name = element.name();
            if name == "a" {
                let in_page = element
                    .attr("href")
                    .is_some_and(|href| href.trim_start().starts_with('#'));
                own.linked = to_f64(own.chars) * if in_page { 0.3 } else { 1.0 };
            }
            let block = is_block(name);
            own.blocky = own.blocky || block;
            measures.insert(node.id(), own);
            if let Some(parent) = open.last_mut() {
                parent.chars += own.chars;
                parent.linked += own.linked;
                parent.commas += own.commas;
                parent.blocky = parent.blocky || own.blocky;
                parent.more_than_a_paragraph = parent.more_than_a_paragraph
                    || own.more_than_a_paragraph
                    || NOT_A_PARAGRAPH.contains(&name);
            }
        }
    });
    measures
}

/// A run of loose text directly in an element: what Readability wraps
/// into a paragraph.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Run {
    chars: usize,
    commas: usize,
}

/// The loose text directly in an element: runs of text and inline elements
/// between its blocks and between its chains of two or more `<br>`.
/// Returns the runs and whether a chain of `<br>` split them.
fn runs_of(
    node: NodeRef<'_>,
    dropped: &Dropped,
    measures: &HashMap<NodeId, Measure>,
) -> (Vec<Run>, bool) {
    let mut runs = Vec::new();
    let mut run = Run::default();
    let mut breaks = 0_usize;
    let mut chained = false;
    let mut end = |run: &mut Run| {
        if run.chars > 0 {
            runs.push(*run);
        }
        *run = Run::default();
    };
    for child in node.children() {
        if dropped.contains(&child.id()) {
            continue;
        }
        if let Some(text) = as_text(child) {
            let chars = chars_of(text);
            if chars > 0 {
                breaks = 0;
                run.chars += chars;
                run.commas += commas_in(text);
            }
            continue;
        }
        let Some(element) = as_element(child) else {
            continue;
        };
        if element.name() == "br" {
            breaks += 1;
            if breaks == 2 {
                chained = true;
                end(&mut run);
            }
            continue;
        }
        breaks = 0;
        let known = measures.get(&child.id()).copied().unwrap_or_default();
        if known.blocky {
            end(&mut run);
        } else {
            run.chars += known.chars;
            run.commas += known.commas;
        }
    }
    end(&mut run);
    (runs, chained)
}

/// The points of every element that got some, in the order they first did.
#[derive(Debug, Default)]
struct Scores {
    order: Vec<NodeId>,
    points: HashMap<NodeId, f64>,
}

impl Scores {
    /// Gives the points of one paragraph to the elements around it,
    /// starting at `from`: all of them to the first, half to the second,
    /// and to the one `n` levels up a share of one in three times `n`.
    fn give(&mut self, from: Option<NodeRef<'_>>, chars: usize, commas: usize, rules: Rules) {
        if chars < MIN_PARAGRAPH_CHARS {
            return;
        }
        let points = 1.0 + to_f64(commas) + to_f64((chars / 100).min(3));
        let mut next = from;
        for level in 0..SCORED_ANCESTORS {
            let Some(node) = next else {
                return;
            };
            let Some(element) = as_element(node) else {
                return;
            };
            // The document's own element is no candidate.
            if node.parent().and_then(as_element).is_none() {
                return;
            }
            let divider = match level {
                0 => 1.0,
                1 => 2.0,
                _ => to_f64(level * 3),
            };
            let score = self.points.entry(node.id()).or_insert_with(|| {
                self.order.push(node.id());
                starting_score(element, rules)
            });
            *score += points / divider;
            next = node.parent();
        }
    }
}

/// Scores the page: every paragraph gives points to what is around it.
fn score(
    root: NodeRef<'_>,
    dropped: &Dropped,
    measures: &HashMap<NodeId, Measure>,
    rules: Rules,
) -> Scores {
    let mut scores = Scores::default();
    walk(root, dropped, |step| {
        let Step::Open(node) = step else {
            return;
        };
        let Some(element) = as_element(node) else {
            return;
        };
        let name = element.name();
        let known = measures.get(&node.id()).copied().unwrap_or_default();
        if !known.blocky {
            return;
        }
        let (runs, chained) = runs_of(node, dropped, measures);
        let is_paragraph = name == "div" && !known.more_than_a_paragraph && !chained;
        if SCORED.contains(&name) || is_paragraph {
            scores.give(node.parent(), known.chars, known.commas, rules);
        }
        // Loose text of a `div`, and text between chains of `<br>` in
        // anything, is a paragraph of the element it stands in.
        if (name == "div" && !is_paragraph) || chained {
            for run in runs {
                scores.give(Some(node), run.chars, run.commas, rules);
            }
        }
    });
    scores
}

/// The element with the most points, and the siblings that read like it.
#[allow(clippy::too_many_lines)] // Readability's own steps, one after the other.
fn choose<'a>(
    root: NodeRef<'a>,
    dropped: &Dropped,
    measures: &HashMap<NodeId, Measure>,
    mut scores: Scores,
) -> Vec<NodeRef<'a>> {
    let tree = root.tree();
    let density = |id: NodeId| measures.get(&id).map_or(0.0, Measure::link_density);
    // Links count against a candidate.
    for (id, points) in &mut scores.points {
        *points *= 1.0 - density(*id);
    }
    let points = |id: NodeId| scores.points.get(&id).copied();
    let mut ranked = scores.order.clone();
    // The earlier of two with the same points stays first.
    ranked.sort_by(|a, b| {
        points(*b)
            .unwrap_or(0.0)
            .total_cmp(&points(*a).unwrap_or(0.0))
    });
    ranked.truncate(TOP_CANDIDATES);
    let Some(mut top) = ranked.first().and_then(|id| tree.get(*id)) else {
        return Vec::new();
    };
    let is_body =
        |node: NodeRef<'_>| as_element(node).is_some_and(|element| element.name() == "body");
    let top_points = points(top.id()).unwrap_or(0.0);

    // What repeats the shape of the best candidate beside it is content
    // like it: looked for first, because the rule after this one would
    // take whatever holds them all, with everything that stands between
    // them.
    if let Some(parts) = repeated(top, dropped, measures) {
        return parts;
    }

    // When several of the best candidates stand under one element, that
    // element is the content rather than the best of them.
    let rivals: Vec<Vec<NodeId>> = ranked
        .iter()
        .skip(1)
        .filter(|id| top_points > 0.0 && points(**id).unwrap_or(0.0) / top_points >= 0.75)
        .filter_map(|id| tree.get(*id))
        .map(|node| node.ancestors().map(|ancestor| ancestor.id()).collect())
        .collect();
    if rivals.len() >= 3 {
        let mut parent = top.parent();
        while let Some(node) = parent.filter(|node| as_element(*node).is_some() && !is_body(*node))
        {
            if rivals
                .iter()
                .filter(|line| line.contains(&node.id()))
                .count()
                >= 3
            {
                top = node;
                break;
            }
            parent = node.parent();
        }
    }

    // An ancestor with more points than what it holds is the content.
    let mut last = points(top.id()).unwrap_or(0.0);
    let floor = last / 3.0;
    let mut parent = top.parent();
    while let Some(node) = parent.filter(|node| as_element(*node).is_some() && !is_body(*node)) {
        parent = node.parent();
        let Some(score) = points(node.id()) else {
            continue;
        };
        if score < floor {
            break;
        }
        if score > last {
            top = node;
            break;
        }
        last = score;
    }

    // An only child stands for its parent.
    let live_elements = |node: NodeRef<'a>| {
        node.children()
            .filter(|child| as_element(*child).is_some() && !dropped.contains(&child.id()))
            .count()
    };
    while let Some(node) = top
        .parent()
        .filter(|node| as_element(*node).is_some() && !is_body(*node) && live_elements(*node) == 1)
    {
        top = node;
    }

    // Siblings that read like the content belong to it.
    let top_points = points(top.id()).unwrap_or(0.0);
    let threshold = (top_points * 0.2).max(10.0);
    let class_of = |node: NodeRef<'a>| {
        as_element(node)
            .and_then(|element| element.attr("class"))
            .filter(|class| !class.is_empty())
    };
    let Some(parent) = top.parent().filter(|node| as_element(*node).is_some()) else {
        return vec![top];
    };
    // Every sibling, and whether it is part of the content.
    let mut siblings: Vec<(NodeRef<'a>, bool)> = Vec::new();
    for sibling in parent.children() {
        let Some(tag) = as_element(sibling).map(scraper::node::Element::name) else {
            continue;
        };
        if dropped.contains(&sibling.id()) {
            continue;
        }
        siblings.push((sibling, false));
        let mut parts = PartOf(&mut siblings);
        if sibling.id() == top.id() {
            parts.push(sibling);
            continue;
        }
        let bonus = if class_of(top).is_some() && class_of(sibling) == class_of(top) {
            top_points * 0.2
        } else {
            0.0
        };
        if points(sibling.id()).is_some_and(|score| score + bonus >= threshold) {
            parts.push(sibling);
            continue;
        }
        if tag == "p" {
            let known = measures.get(&sibling.id()).copied().unwrap_or_default();
            let density = known.link_density();
            let long = known.chars > 80 && density < 0.25;
            let sentence = || {
                let text = text_under(sibling, dropped);
                text.ends_with('.') || text.contains(". ")
            };
            if long || (known.chars > 0 && known.chars < 80 && density == 0.0 && sentence()) {
                parts.push(sibling);
            }
        }
    }
    // A figure that stands between two parts of the content belongs to it:
    // it gets no points, because its caption is no paragraph.
    let first = siblings.iter().position(|(_, part)| *part);
    let last = siblings.iter().rposition(|(_, part)| *part);
    if let (Some(first), Some(last)) = (first, last) {
        for (sibling, part) in &mut siblings[first..=last] {
            if as_element(*sibling).is_some_and(|element| element.name() == "figure") {
                *part = true;
            }
        }
    }
    siblings
        .into_iter()
        .filter_map(|(sibling, part)| part.then_some(sibling))
        .collect()
}

/// Marks the sibling looked at last as part of the content.
struct PartOf<'s, 'a>(&'s mut Vec<(NodeRef<'a>, bool)>);

impl<'a> PartOf<'_, 'a> {
    fn push(&mut self, sibling: NodeRef<'a>) {
        if let Some(last) = self
            .0
            .last_mut()
            .filter(|(node, _)| node.id() == sibling.id())
        {
            last.1 = true;
        }
    }
}

/// What a block of the content holds, counted as it is cleaned.
#[derive(Debug, Clone, Copy, Default)]
struct Held {
    chars: usize,
    linked: f64,
    commas: usize,
    paragraphs: usize,
    images: usize,
    items: usize,
    inputs: usize,
    heading_chars: usize,
    list_chars: usize,
    /// Whether any of its text stands in an element that holds text.
    textish: bool,
    /// Whether it holds a table of data.
    data_table: bool,
    /// Elements directly in it, and those of them that hold more than one.
    children: usize,
    crowded_children: usize,
}

impl Held {
    fn take(&mut self, child: &Self) {
        self.chars += child.chars;
        self.linked += child.linked;
        self.commas += child.commas;
        self.paragraphs += child.paragraphs;
        self.images += child.images;
        self.items += child.items;
        self.inputs += child.inputs;
        self.heading_chars += child.heading_chars;
        self.list_chars += child.list_chars;
        self.textish = self.textish || child.textish;
        self.data_table = self.data_table || child.data_table;
    }
}

/// Whether a table holds data rather than laying out a page: Readability's
/// own signs of one.
pub(crate) fn is_data_table(table: NodeRef<'_>, dropped: &Dropped) -> bool {
    let Some(element) = as_element(table) else {
        return false;
    };
    if role_of(element).as_deref() == Some("presentation") || element.attr("datatable") == Some("0")
    {
        return false;
    }
    if element.attr("summary").is_some() {
        return true;
    }
    let (mut rows, mut columns, mut in_row, mut nested, mut marked) =
        (0_usize, 0_usize, 0_usize, false, false);
    let mut depth = 0_usize;
    walk(table, dropped, |step| match step {
        Step::Open(node) => {
            let Some(element) = as_element(node) else {
                return;
            };
            depth += 1;
            match element.name() {
                "table" if depth > 1 => nested = true,
                "col" | "colgroup" | "tfoot" | "thead" | "th" => marked = true,
                "caption" if node.has_children() => marked = true,
                "tr" => {
                    rows += 1;
                    in_row = 0;
                }
                "td" => {
                    in_row += element
                        .attr("colspan")
                        .and_then(|span| span.trim().parse::<usize>().ok())
                        .unwrap_or(1)
                        .max(1);
                    columns = columns.max(in_row);
                }
                _ => {}
            }
        }
        Step::Close(node) => {
            if as_element(node).is_some() {
                depth = depth.saturating_sub(1);
            }
        }
    });
    if marked {
        return true;
    }
    if nested {
        return false;
    }
    rows >= 10 || columns > 4 || rows * columns > 10
}

/// Whether the names of an element say it is for sharing the page.
fn is_share(names: &str) -> bool {
    names
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .flat_map(|name| name.split('-'))
        .any(|word| word == "share" || word == "sharedaddy")
}

/// Whether a block of the content is taken out for what it holds.
fn judged_out(
    node: NodeRef<'_>,
    element: &scraper::node::Element,
    held: &Held,
    in_figure: bool,
    dropped: &Dropped,
    rules: Rules,
) -> bool {
    let name = element.name();
    let weight = name_weight(element, rules);
    if weight < 0.0 {
        return true;
    }
    if held.commas >= 10 {
        return false;
    }
    let chars = to_f64(held.chars);
    let ratio = |part: usize| {
        if held.chars == 0 {
            0.0
        } else {
            to_f64(part) / chars
        }
    };
    let is_list = matches!(name, "ul" | "ol") || ratio(held.list_chars) > 0.9;
    if held.chars < 40 {
        let said = text_under(node, dropped).to_lowercase();
        let said = said.trim_end_matches(['.', '…']).trim();
        if FILLER.contains(&said) {
            return true;
        }
    }
    let density = if held.chars == 0 {
        0.0
    } else {
        held.linked / chars
    };
    let (paragraphs, images) = (held.paragraphs, held.images);
    let out = (!in_figure && images > 1 && to_f64(paragraphs) / to_f64(images) < 0.5)
        // A hundred items more than paragraphs, outside a list.
        || (!is_list && held.items > paragraphs + 100)
        // Inputs are counted against a form: everywhere else they are
        // cleaned out before anything is judged.
        || (matches!(name, "form" | "fieldset") && held.inputs > paragraphs / 3)
        || (!is_list
            && !in_figure
            && ratio(held.heading_chars) < 0.9
            && held.chars < 25
            && (images == 0 || images > 2)
            && density > 0.0)
        || (!is_list && weight < 25.0 && density > 0.2)
        || (weight >= 25.0 && density > 0.5)
        || (images == 0 && !held.textish);
    // A list of images, one to an item, stays.
    if is_list && out {
        if held.crowded_children > 0 {
            return true;
        }
        return images != held.items;
    }
    out
}

/// Cleans what was chosen: what is never content goes, and so does what is
/// mostly links, images or inputs. Adds to `dropped`.
#[allow(clippy::too_many_lines)] // One pass from the leaves up, with everything it counts.
fn clean(
    part: NodeRef<'_>,
    dropped: &mut Dropped,
    measures: &HashMap<NodeId, Measure>,
    rules: Rules,
) {
    let before = dropped.clone();
    let mut held: Vec<Held> = vec![Held::default()];
    // Depth under a figure, under a table of data, under code.
    let (mut in_figure, mut in_data, mut in_code) = (0_usize, 0_usize, 0_usize);
    let mut data_tables: Vec<bool> = Vec::new();
    walk(part, &before, |step| match step {
        Step::Open(node) => {
            if let Some(element) = as_element(node) {
                held.push(Held::default());
                match element.name() {
                    "figure" => in_figure += 1,
                    "code" => in_code += 1,
                    "table" => {
                        let data = is_data_table(node, &before);
                        data_tables.push(data);
                        if data {
                            in_data += 1;
                        }
                    }
                    _ => {}
                }
            } else if let (Some(text), Some(top)) = (as_text(node), held.last_mut()) {
                top.chars += chars_of(text);
                top.commas += commas_in(text);
            }
        }
        Step::Close(node) => {
            let Some(element) = as_element(node) else {
                return;
            };
            let Some(mut own) = held.pop() else {
                return;
            };
            let name = element.name();
            let mut of_data = false;
            match name {
                "figure" => in_figure = in_figure.saturating_sub(1),
                "code" => in_code = in_code.saturating_sub(1),
                "table" => {
                    of_data = data_tables.pop().unwrap_or(false);
                    if of_data {
                        in_data = in_data.saturating_sub(1);
                    }
                }
                _ => {}
            }
            if name == "a" {
                let in_page = element
                    .attr("href")
                    .is_some_and(|href| href.trim_start().starts_with('#'));
                own.linked = to_f64(own.chars) * if in_page { 0.3 } else { 1.0 };
            }
            let known = measures.get(&node.id()).copied().unwrap_or_default();
            // Loose text of a `div`, and of a cell, is its paragraphs, as
            // it was scored.
            if matches!(name, "div" | "td") {
                if known.more_than_a_paragraph || name == "td" {
                    let (runs, _) = runs_of(node, &before, measures);
                    own.paragraphs += runs.len();
                    own.textish = own.textish || !runs.is_empty();
                } else if own.chars > 0 {
                    own.textish = true;
                }
            }
            let names = names_of(element);
            let protected = of_data || in_data > 0 || in_code > 0 || own.data_table;
            // What was chosen is judged like what it holds: a choice that is
            // mostly links is none, and the next pass looks again.
            let out = CLEANED.contains(&name)
                || (is_share(&names) && own.chars < ENOUGH_CHARS)
                || (matches!(name, "h1" | "h2") && name_weight(element, rules) < 0.0)
                || (rules.clean
                    && JUDGED.contains(&name)
                    && !protected
                    && judged_out(node, element, &own, in_figure > 0, &before, rules));
            if out {
                dropped.insert(node.id());
                // An input that goes still counts against the form it
                // stood in.
                if let (Some(parent), "input") = (held.last_mut(), name) {
                    parent.inputs += 1;
                }
                return;
            }
            let Some(parent) = held.last_mut() else {
                return;
            };
            parent.take(&own);
            parent.children += 1;
            if own.children > 1 {
                parent.crowded_children += 1;
            }
            parent.data_table = parent.data_table || of_data;
            let is_paragraph = name == "p" || (name == "div" && !known.more_than_a_paragraph);
            if is_paragraph {
                parent.paragraphs += 1;
            }
            match name {
                "img" => parent.images += 1,
                "li" => parent.items += 1,
                "ul" | "ol" => parent.list_chars += own.chars,
                _ => {}
            }
            if heading_level(name).is_some() {
                parent.heading_chars += own.chars;
            }
            if TEXTISH.contains(&name) && (own.chars > 0 || name == "img") {
                parent.textish = true;
            }
        }
    });
}

/// One pass under one set of rules.
fn pass(root: NodeRef<'_>, rules: Rules) -> Content<'_> {
    let mut dropped = strip(root, rules);
    let measures = measure(root, &dropped);
    let scores = score(root, &dropped, &measures, rules);
    let parts = choose(root, &dropped, &measures, scores);
    for part in &parts {
        clean(*part, &mut dropped, &measures, rules);
    }
    Content { parts, dropped }
}

/// Finds the main content of the page under `root`.
///
/// A pass that finds too little is made again without the rule for
/// unlikely names, then without the weight of names, then without the
/// cleaning; of passes that all found too little, the one that found the
/// most stands. A page nobody would call an article (a listing, a shell
/// that scripts fill) has what content the rules find, which may be none.
pub(crate) fn find(root: NodeRef<'_>) -> Content<'_> {
    let attempts = [
        (true, true, true),
        (false, true, true),
        (false, false, true),
        (false, false, false),
    ];
    let mut best: Option<(usize, Content<'_>)> = None;
    for (strip_unlikely, weigh_names, clean) in attempts {
        let content = pass(
            root,
            Rules {
                strip_unlikely,
                weigh_names,
                clean,
            },
        );
        let chars = content.chars();
        if chars >= ENOUGH_CHARS {
            return content;
        }
        if best.as_ref().is_none_or(|(most, _)| chars > *most) {
            best = Some((chars, content));
        }
    }
    best.map(|(_, content)| content).unwrap_or_default()
}

#[cfg(test)]
mod tests;
