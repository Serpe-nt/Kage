//! Walking the one tree of a page (spec §14).
//!
//! Everything in this crate reads the tree `kage_crawl::html::Document`
//! parsed, and never changes it: what is no content is a set of node ids
//! ([`Dropped`]) that every walk leaves out with everything under it. The
//! walks are loops, not recursion, so a page nested a hundred thousand
//! elements deep is read like any other.

use std::collections::HashSet;

use ego_tree::NodeId;
use kage_crawl::html::{NOT_TEXT, hides};
use scraper::Node;
use scraper::node::Element;

/// A node of a page's tree.
pub(crate) type NodeRef<'a> = ego_tree::NodeRef<'a, Node>;

/// The nodes a walk leaves out, each with everything under it.
pub(crate) type Dropped = HashSet<NodeId>;

/// One step of a walk through a tree.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Step<'a> {
    /// A node is entered, before anything under it.
    Open(NodeRef<'a>),
    /// A node is left, after everything under it.
    Close(NodeRef<'a>),
}

/// Walks the tree under `root` in document order, `root` included, leaving
/// out every node of `dropped` with what is under it.
pub(crate) fn walk<'a>(root: NodeRef<'a>, dropped: &Dropped, mut visit: impl FnMut(Step<'a>)) {
    if dropped.contains(&root.id()) {
        return;
    }
    let live = |mut next: Option<NodeRef<'a>>| {
        while let Some(node) = next {
            if !dropped.contains(&node.id()) {
                return Some(node);
            }
            next = node.next_sibling();
        }
        None
    };
    let mut node = root;
    visit(Step::Open(node));
    loop {
        if let Some(child) = live(node.first_child()) {
            node = child;
            visit(Step::Open(node));
            continue;
        }
        loop {
            visit(Step::Close(node));
            if node.id() == root.id() {
                return;
            }
            if let Some(next) = live(node.next_sibling()) {
                node = next;
                visit(Step::Open(node));
                break;
            }
            let Some(parent) = node.parent() else {
                return;
            };
            node = parent;
        }
    }
}

/// The element a node is, when it is one.
pub(crate) fn as_element(node: NodeRef<'_>) -> Option<&Element> {
    match node.value() {
        Node::Element(element) => Some(element),
        _ => None,
    }
}

/// The text a node is, when it is one.
pub(crate) fn as_text(node: NodeRef<'_>) -> Option<&str> {
    match node.value() {
        Node::Text(text) => Some(text),
        _ => None,
    }
}

/// Whether nothing under an element is ever text a reader sees: what the
/// page reader calls no text (`script`, `style`, `noscript` and the like),
/// what the markup hides, and what it hides from assistive technology,
/// which is how a page marks decoration and copies of what it shows
/// elsewhere.
pub(crate) fn unseen(element: &Element) -> bool {
    NOT_TEXT.contains(&element.name())
        || hides(element)
        || element
            .attr("aria-hidden")
            .is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

/// Elements that start a block of their own: text on either side of one
/// is two pieces of text.
const BLOCKS: [&str; 46] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "caption",
    "center",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "html",
    "li",
    "main",
    "menu",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "ul",
];

/// Whether an element starts a block of its own.
pub(crate) fn is_block(name: &str) -> bool {
    BLOCKS.contains(&name)
}

/// The level of a heading element, 1 to 6.
pub(crate) fn heading_level(name: &str) -> Option<u8> {
    match name {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

/// The characters of a piece of text as a reader counts them: its words
/// and one space after each.
pub(crate) fn chars_of(text: &str) -> usize {
    text.split_whitespace()
        .map(|word| word.chars().count() + 1)
        .sum()
}

/// The class and the id of an element in lower case, as one string to look
/// for names in.
pub(crate) fn names_of(element: &Element) -> String {
    let mut names = String::new();
    for value in [element.attr("class"), element.attr("id")]
        .into_iter()
        .flatten()
    {
        names.push_str(&value.to_ascii_lowercase());
        names.push(' ');
    }
    names
}

/// The text under a node as a reader sees it, white space collapsed,
/// without what `dropped` leaves out and what is never seen.
pub(crate) fn text_under(root: NodeRef<'_>, dropped: &Dropped) -> String {
    let mut out = String::new();
    let mut unseen_depth = 0_usize;
    walk(root, dropped, |step| match step {
        Step::Open(node) => {
            if let Some(element) = as_element(node) {
                if unseen_depth > 0 || unseen(element) {
                    unseen_depth += 1;
                }
            } else if let (Some(text), 0) = (as_text(node), unseen_depth) {
                kage_crawl::html::push_text(&mut out, text);
            }
        }
        Step::Close(node) => {
            if as_element(node).is_some() && unseen_depth > 0 {
                unseen_depth -= 1;
            }
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use scraper::Html;

    use super::{
        Dropped, Step, as_element, chars_of, is_block, names_of, text_under, unseen, walk,
    };

    fn names(html: &Html, dropped: &Dropped) -> Vec<String> {
        let mut seen = Vec::new();
        walk(html.tree.root(), dropped, |step| {
            let (mark, node) = match step {
                Step::Open(node) => ("+", node),
                Step::Close(node) => ("-", node),
            };
            if let Some(element) = as_element(node) {
                seen.push(format!("{mark}{}", element.name()));
            }
        });
        seen
    }

    #[test]
    fn a_walk_opens_and_closes_every_node_in_document_order() {
        let html =
            Html::parse_document("<html><body><p>a<b>b</b></p><ul><li>c</li></ul></body></html>");
        assert_eq!(
            names(&html, &Dropped::new()),
            [
                "+html", "+head", "-head", "+body", "+p", "+b", "-b", "-p", "+ul", "+li", "-li",
                "-ul", "-body", "-html"
            ]
        );
    }

    #[test]
    fn a_walk_leaves_out_what_is_dropped_with_everything_under_it() {
        let html =
            Html::parse_document("<body><p>a<b>b</b></p><ul><li>c</li></ul><div>d</div></body>");
        let find = |name: &str| {
            html.tree
                .nodes()
                .find(|node| as_element(*node).is_some_and(|element| element.name() == name))
                .unwrap()
                .id()
        };
        let dropped: Dropped = [find("p"), find("li")].into_iter().collect();
        assert_eq!(
            names(&html, &dropped),
            [
                "+html", "+head", "-head", "+body", "+ul", "-ul", "+div", "-div", "-body", "-html"
            ]
        );
        assert_eq!(text_under(html.tree.root(), &dropped), "d");
        // A dropped root is no walk at all.
        let root: Dropped = [html.tree.root().id()].into_iter().collect();
        assert_eq!(names(&html, &root), Vec::<String>::new());
    }

    #[test]
    fn a_tree_far_deeper_than_a_stack_is_walked() {
        let depth = 60_000;
        let html = Html::parse_fragment(&format!(
            "{}deep{}",
            "<span>".repeat(depth),
            "</span>".repeat(depth)
        ));
        let mut opened = 0_usize;
        walk(html.tree.root(), &Dropped::new(), |step| {
            if matches!(step, Step::Open(node) if as_element(node).is_some()) {
                opened += 1;
            }
        });
        assert!(opened >= depth, "{opened}");
        assert_eq!(text_under(html.tree.root(), &Dropped::new()), "deep");
    }

    #[test]
    fn what_a_reader_never_sees_is_no_text() {
        let html = Html::parse_document(
            r#"<body><p>seen</p><script>var a;</script><p hidden>gone</p>
            <div style="display:none">gone</div><span aria-hidden="TRUE">decoration</span>
            <noscript>fallback</noscript><p aria-hidden="false">kept</p></body>"#,
        );
        assert_eq!(text_under(html.tree.root(), &Dropped::new()), "seen kept");
        let is = |markup: &str, name: &str| {
            let html = Html::parse_document(&format!("<body>{markup}</body>"));
            let node = html
                .tree
                .nodes()
                .filter_map(|node| as_element(node))
                .find(|element| element.name() == name)
                .unwrap();
            unseen(node)
        };
        assert!(is("<template>x</template>", "template"));
        assert!(is(r#"<i aria-hidden="true">x</i>"#, "i"));
        assert!(!is(r#"<i aria-hidden="mixed">x</i>"#, "i"));
        assert!(!is("<p>x</p>", "p"));
    }

    #[test]
    fn small_things_about_elements() {
        assert!(is_block("div") && is_block("td") && is_block("h4"));
        assert!(!is_block("span") && !is_block("a") && !is_block("font"));
        assert_eq!(super::heading_level("h3"), Some(3));
        assert_eq!(super::heading_level("header"), None);
        assert_eq!(chars_of("  two   words "), 10);
        assert_eq!(chars_of(""), 0);
        let html = Html::parse_fragment(r#"<p class="Entry-Content Wide" id="Main">x</p>"#);
        let p = html
            .tree
            .nodes()
            .filter_map(|node| as_element(node))
            .find(|element| element.name() == "p")
            .unwrap();
        assert_eq!(names_of(p), "entry-content wide main ");
        // The list of blocks is sorted and each is there once.
        let mut sorted = super::BLOCKS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, super::BLOCKS);
    }
}
