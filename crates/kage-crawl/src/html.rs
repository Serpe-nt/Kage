//! Reading a page as a crawl needs it (spec §13): the links it holds and
//! the text it shows.
//!
//! [`read`] takes the HTML of a page (the document a browser rendered, or
//! the body the http path fetched) and the address it was loaded from, and
//! returns every link with the address it leads to, and the text of the page
//! twice: all of it, and what is left without the parts every page of a site
//! repeats (its navigation, header, footer and side bars), which is what two
//! pages are compared by.
//!
//! A link says whether the markup hides it: the `hidden` attribute, an
//! inline `display: none` or `visibility: hidden`, on the link or on
//! anything around it. Such a link is one no reader can follow, and a site
//! that plants one is looking for whoever does. The markup is all this
//! module sees; what a style sheet hides only a browser knows, and on the
//! browser path the browser is asked (spec §10.2).
//!
//! A page is parsed once. [`Document`] is that one tree: a crawl reads its
//! links and its text from it ([`Document::page`]), and extraction
//! (`kage-extract`, spec §14) reads everything else from the same tree
//! with the same rules for what is hidden and what is furniture.
//!
//! A page is hostile input, and how deep it nests is the site's choice.
//! Nothing here calls itself once per level: the parser keeps its open
//! elements on a stack of its own, and the walk that reads the links and
//! the text is a loop over the tree with a stack of what each open element
//! hands down. A page nested five thousand or fifty thousand elements deep
//! is read on the 2 MiB stack of a worker thread like any other (F97).

use ego_tree::iter::Edge;
use scraper::node::Node;
use scraper::{ElementRef, Html};
use url::Url;

/// The most links read from one page; a page with more is cut there.
pub const MAX_LINKS: usize = 5000;

/// One link of a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// Where it leads: the `href` resolved against the page's base address,
    /// as it is written, fragment included.
    pub url: Url,
    /// The text of the link, white space collapsed.
    pub text: String,
    /// The tokens of its `rel` attribute, in lower case.
    pub rel: Vec<String>,
    /// Whether the markup hides the link from a reader.
    pub hidden: bool,
    /// Whether it stands in what a site repeats on every page: inside
    /// navigation, a header, a footer or a side bar.
    pub furniture: bool,
}

/// What a crawl reads from a page.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Page {
    /// The `<title>`, white space collapsed; empty without one.
    pub title: String,
    /// Every link to an `http` or `https` address, in document order. A
    /// link that occurs twice is listed twice.
    pub links: Vec<Link>,
    /// Whether the page held more links than [`MAX_LINKS`].
    pub links_truncated: bool,
    /// The text a reader sees, white space collapsed.
    pub text: String,
    /// The same without navigation, header, footer and side bars; the whole
    /// text when the page marks none of them or little is left without.
    pub main_text: String,
}

/// Elements whose content is never text of the page.
pub const NOT_TEXT: [&str; 8] = [
    "script", "style", "noscript", "template", "head", "svg", "iframe", "object",
];

/// Elements that hold what a site repeats on every page.
const FURNITURE: [&str; 4] = ["nav", "header", "footer", "aside"];

/// The `role` values of the same.
const FURNITURE_ROLES: [&str; 4] = ["navigation", "banner", "contentinfo", "complementary"];

/// A main text of fewer words than this is no page of its own: the whole
/// text stands in for it.
const MIN_MAIN_WORDS: usize = 20;

/// Whether an element's own markup hides it and everything inside it.
pub fn hides(element: &scraper::node::Element) -> bool {
    if element.attr("hidden").is_some() {
        return true;
    }
    let Some(style) = element.attr("style") else {
        return false;
    };
    style.split(';').any(|declaration| {
        let mut parts = declaration.splitn(2, ':');
        let property = parts.next().unwrap_or_default().trim();
        let value = parts
            .next()
            .unwrap_or_default()
            .trim()
            .trim_end_matches("!important")
            .trim();
        (property.eq_ignore_ascii_case("display") && value.eq_ignore_ascii_case("none"))
            || (property.eq_ignore_ascii_case("visibility")
                && (value.eq_ignore_ascii_case("hidden") || value.eq_ignore_ascii_case("collapse")))
    })
}

/// Whether an element holds what a site repeats on every page: navigation,
/// a header, a footer or a side bar, by its name or its ARIA role.
pub fn is_furniture(element: &scraper::node::Element) -> bool {
    FURNITURE.contains(&element.name())
        || element.attr("role").is_some_and(|role| {
            FURNITURE_ROLES
                .iter()
                .any(|known| role.eq_ignore_ascii_case(known))
        })
}

/// Appends text with one space between it and what is there, white space
/// collapsed.
pub fn push_text(out: &mut String, text: &str) {
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
}

/// The text inside an element, white space collapsed.
pub fn text_of(element: ElementRef<'_>) -> String {
    let mut out = String::new();
    for piece in element.text() {
        push_text(&mut out, piece);
    }
    out
}

/// What the walk through the document carries from an element to what is
/// inside it.
#[derive(Clone, Copy)]
struct Inherited {
    /// Inside an element whose content is no text.
    silent: bool,
    /// Inside an element the markup hides.
    hidden: bool,
    /// Inside navigation, a header, a footer or a side bar.
    furniture: bool,
}

impl Inherited {
    /// Outside everything: the document's own element inherits nothing.
    const NOTHING: Self = Self {
        silent: false,
        hidden: false,
        furniture: false,
    };
}

struct Reader<'a> {
    base: &'a Url,
    page: Page,
}

impl Reader<'_> {
    /// Reads every element under `root`, `root` included, in document
    /// order: its links, and its text where a reader sees it.
    ///
    /// One loop over the tree, with a stack that holds what every open
    /// element hands down to what is inside it; never a call per level of
    /// nesting, so that a page nested as deep as a site likes is read on a
    /// worker's stack (F97).
    fn read(&mut self, root: ElementRef<'_>) {
        let mut open: Vec<Inherited> = Vec::new();
        for edge in root.traverse() {
            match edge {
                Edge::Open(node) => match node.value() {
                    Node::Element(value) => {
                        let inherited = open.last().copied().unwrap_or(Inherited::NOTHING);
                        let name = value.name();
                        let here = Inherited {
                            silent: inherited.silent || NOT_TEXT.contains(&name),
                            hidden: inherited.hidden || hides(value),
                            furniture: inherited.furniture || is_furniture(value),
                        };
                        if (name == "a" || name == "area")
                            && let Some(href) = value.attr("href")
                            && let Some(element) = ElementRef::wrap(node)
                        {
                            self.link(element, href, here);
                        }
                        open.push(here);
                    }
                    Node::Text(text) => {
                        // Text stands in the element that is open around it.
                        let Some(here) = open.last() else {
                            continue;
                        };
                        if !here.silent && !here.hidden {
                            push_text(&mut self.page.text, text);
                            if !here.furniture {
                                push_text(&mut self.page.main_text, text);
                            }
                        }
                    }
                    _ => {}
                },
                Edge::Close(node) => {
                    if matches!(node.value(), Node::Element(_)) {
                        open.pop();
                    }
                }
            }
        }
    }

    fn link(&mut self, element: ElementRef<'_>, href: &str, here: Inherited) {
        if self.page.links.len() >= MAX_LINKS {
            self.page.links_truncated = true;
            return;
        }
        // What a browser does with the attribute: white space around it is
        // not part of the address.
        let Ok(url) = self.base.join(href.trim()) else {
            return;
        };
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return;
        }
        let rel = element
            .value()
            .attr("rel")
            .map(|rel| {
                rel.split_ascii_whitespace()
                    .map(str::to_ascii_lowercase)
                    .collect()
            })
            .unwrap_or_default();
        self.page.links.push(Link {
            url,
            text: text_of(element),
            rel,
            hidden: here.hidden,
            furniture: here.furniture,
        });
    }
}

/// The address relative links of a document resolve against: its first
/// `<base href>` when that is an `http` or `https` address, else the address
/// the document was loaded from.
fn base_of(document: &Html, loaded_from: &Url) -> Url {
    document
        .tree
        .nodes()
        .filter_map(ElementRef::wrap)
        .find(|element| element.value().name() == "base" && element.value().attr("href").is_some())
        .and_then(|element| element.value().attr("href"))
        .and_then(|href| loaded_from.join(href.trim()).ok())
        .filter(|base| matches!(base.scheme(), "http" | "https"))
        .unwrap_or_else(|| loaded_from.clone())
}

/// A page parsed once: the tree a crawl reads its links and its text from,
/// and the one extraction works on.
#[derive(Debug, Clone)]
pub struct Document {
    tree: Html,
    loaded_from: Url,
    base: Url,
}

impl Document {
    /// Parses a page.
    ///
    /// `loaded_from` is the address of the document after redirects;
    /// relative links resolve against it, or against the document's
    /// `<base href>`. Nothing here fails: markup that is no HTML at all is
    /// a page without links, and what it holds is its text.
    pub fn parse(html: &str, loaded_from: &Url) -> Self {
        let tree = Html::parse_document(html);
        let base = base_of(&tree, loaded_from);
        Self {
            tree,
            loaded_from: loaded_from.clone(),
            base,
        }
    }

    /// The tree of the page.
    pub const fn tree(&self) -> &Html {
        &self.tree
    }

    /// The address the document was loaded from, after redirects.
    pub const fn loaded_from(&self) -> &Url {
        &self.loaded_from
    }

    /// The address relative links resolve against: the first `<base href>`
    /// that is an `http` or `https` address, else where the document was
    /// loaded from.
    pub const fn base(&self) -> &Url {
        &self.base
    }

    /// What a crawl reads from the page: its links and its text.
    pub fn page(&self) -> Page {
        let mut reader = Reader {
            base: &self.base,
            page: Page::default(),
        };
        let root = self.tree.root_element();
        reader.page.title = root
            .descendent_elements()
            .find(|element| element.value().name() == "title")
            .map(text_of)
            .unwrap_or_default();
        reader.read(root);
        let mut page = reader.page;
        if page.main_text.split(' ').count() < MIN_MAIN_WORDS {
            page.main_text.clone_from(&page.text);
        }
        page
    }
}

/// Reads a page: its links and its text. [`Document::parse`] and
/// [`Document::page`] in one step, for whoever needs nothing else of the
/// tree.
pub fn read(html: &str, loaded_from: &Url) -> Page {
    Document::parse(html, loaded_from).page()
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{Document, MAX_LINKS, Page, read};

    fn page(html: &str) -> Page {
        read(
            html,
            &Url::parse("https://shop.example/catalogue/list?page=2").unwrap(),
        )
    }

    fn hrefs(page: &Page) -> Vec<&str> {
        page.links.iter().map(|link| link.url.as_str()).collect()
    }

    #[test]
    fn links_resolve_against_the_address_of_the_page() {
        let read = page(
            r##"<html><body>
            <a href="/a">absolute path</a>
            <a href="b">relative</a>
            <a href="../c?x=1#part">up, with a query and a fragment</a>
            <a href="?page=3">only a query</a>
            <a href="#top">only a fragment</a>
            <a href="//cdn.example/d">another host, same scheme</a>
            <a href="http://Other.Example:8080/e">another site</a>
            <a href="  /padded  ">white space around</a>
            </body></html>"##,
        );
        assert_eq!(
            hrefs(&read),
            [
                "https://shop.example/a",
                "https://shop.example/catalogue/b",
                "https://shop.example/c?x=1#part",
                "https://shop.example/catalogue/list?page=3",
                "https://shop.example/catalogue/list?page=2#top",
                "https://cdn.example/d",
                "http://other.example:8080/e",
                "https://shop.example/padded",
            ]
        );
        assert_eq!(read.links[0].text, "absolute path");
        assert!(read.links.iter().all(|link| !link.hidden));
        assert!(!read.links_truncated);
    }

    #[test]
    fn a_base_element_moves_what_relative_links_resolve_against() {
        let read = page(
            r#"<html><head><base href="https://shop.example/elsewhere/"></head>
            <body><a href="x">x</a><a href="/y">y</a></body></html>"#,
        );
        assert_eq!(
            hrefs(&read),
            ["https://shop.example/elsewhere/x", "https://shop.example/y"]
        );
        // A base that is no web address is not one.
        let odd = page(r#"<base href="javascript:void(0)"><a href="x">x</a>"#);
        assert_eq!(hrefs(&odd), ["https://shop.example/catalogue/x"]);
        // Nor is a base without an address.
        let empty = page(r#"<base target="_blank"><a href="x">x</a>"#);
        assert_eq!(hrefs(&empty), ["https://shop.example/catalogue/x"]);
    }

    #[test]
    fn what_is_no_page_address_is_no_link() {
        let read = page(
            r#"<a href="mailto:someone@example.org">mail</a>
            <a href="javascript:void(0)">script</a>
            <a href="tel:+431234567">phone</a>
            <a href="data:text/plain,hello">data</a>
            <a href="ftp://files.example/a">ftp</a>
            <a>no address</a>
            <a href="http://">no host</a>
            <a href="https://[bad">not an address</a>
            <a href="/kept">kept</a>
            <map><area href="/from-a-map" alt="map"></map>
            <link rel="stylesheet" href="/style.css">"#,
        );
        assert_eq!(
            hrefs(&read),
            [
                "https://shop.example/kept",
                "https://shop.example/from-a-map"
            ]
        );
    }

    #[test]
    fn a_link_the_markup_hides_says_so() {
        let read = page(
            r#"<a href="/seen">seen</a>
            <a href="/none" style="display:none">hidden</a>
            <a href="/none-spaced" style="color: red; DISPLAY : None !important">hidden</a>
            <a href="/invisible" style="visibility:hidden">hidden</a>
            <a href="/attribute" hidden>hidden</a>
            <div style="display: none"><p><a href="/inside">inside a hidden box</a></p></div>
            <p hidden><a href="/inside-attribute">inside</a></p>
            <a href="/block" style="display:block">shown</a>
            <a href="/visible" style="visibility: visible; display: inline">shown</a>
            <a href="/aria" aria-hidden="true">shown to the eye</a>"#,
        );
        let hidden: Vec<(&str, bool)> = read
            .links
            .iter()
            .map(|link| (link.url.path(), link.hidden))
            .collect();
        assert_eq!(
            hidden,
            [
                ("/seen", false),
                ("/none", true),
                ("/none-spaced", true),
                ("/invisible", true),
                ("/attribute", true),
                ("/inside", true),
                ("/inside-attribute", true),
                ("/block", false),
                ("/visible", false),
                ("/aria", false),
            ]
        );
        // What is hidden is no text of the page either.
        assert_eq!(read.text, "seen shown shown shown to the eye");
    }

    #[test]
    fn rel_is_read_as_its_tokens() {
        let read = page(
            r#"<a href="/a" rel="NoFollow  noopener">a</a><a href="/b">b</a><a href="/c" rel="">c</a>"#,
        );
        assert_eq!(read.links[0].rel, ["nofollow", "noopener"]);
        assert_eq!(read.links[1].rel, Vec::<String>::new());
        assert_eq!(read.links[2].rel, Vec::<String>::new());
    }

    #[test]
    fn the_text_is_what_a_reader_sees() {
        let read = page(
            r#"<!doctype html><html><head><title>  A  title
            </title><style>p { color: red }</style><script>var hidden = 1;</script></head>
            <body><h1>Heading</h1>
            <p>First   paragraph,
               on two lines.</p>
            <script>document.write("never text")</script>
            <noscript>not text for a browser</noscript>
            <template><p>nor this</p></template>
            <svg><text>nor a drawing</text></svg>
            <p>Second <b>bold</b> paragraph.</p></body></html>"#,
        );
        assert_eq!(read.title, "A title");
        assert_eq!(
            read.text,
            "Heading First paragraph, on two lines. Second bold paragraph."
        );
        assert_eq!(read.main_text, read.text);
    }

    #[test]
    fn the_main_text_leaves_out_what_every_page_repeats() {
        let article = "word ".repeat(30);
        let read = page(&format!(
            r#"<body><header><p>The site's name</p></header>
            <nav><a href="/">Home</a> <a href="/list">List</a></nav>
            <div role="navigation">Breadcrumbs</div>
            <main><h1>Title</h1><p>{article}</p><aside>Related things</aside></main>
            <div role="complementary">A side bar</div>
            <footer>Imprint</footer><div role="contentinfo">Legal</div></body>"#
        ));
        assert_eq!(read.main_text, format!("Title {}", article.trim()));
        assert!(
            read.text
                .starts_with("The site's name Home List Breadcrumbs Title word")
        );
        assert!(
            read.text
                .ends_with("Related things A side bar Imprint Legal")
        );
        // Links in the furniture are links all the same, and say where
        // they stand: a reader goes through the text before the menu.
        assert_eq!(
            hrefs(&read),
            ["https://shop.example/", "https://shop.example/list"]
        );
        assert!(read.links.iter().all(|link| link.furniture));
        let mixed = page(
            r#"<main><p><a href="/a">in the text</a></p>
            <aside><a href="/b">beside it</a></aside></main>
            <div role="contentinfo"><a href="/c">below it</a></div>"#,
        );
        assert_eq!(
            mixed
                .links
                .iter()
                .map(|link| link.furniture)
                .collect::<Vec<_>>(),
            [false, true, true]
        );

        // A page that is all furniture has no main text of its own: the
        // whole text stands in.
        let thin = page("<nav>Home List About</nav><main><p>Nothing here.</p></main>");
        assert_eq!(thin.main_text, "Home List About Nothing here.");
    }

    #[test]
    fn a_page_with_too_many_links_is_cut_and_says_so() {
        let many = (0..MAX_LINKS + 3)
            .map(|index| format!(r#"<a href="/p{index}">p</a>"#))
            .collect::<Vec<_>>()
            .concat();
        let read = page(&many);
        assert_eq!(read.links.len(), MAX_LINKS);
        assert!(read.links_truncated);
        assert_eq!(
            read.links.last().unwrap().url.path(),
            format!("/p{}", MAX_LINKS - 1)
        );
    }

    #[test]
    fn a_document_is_parsed_once_and_read_as_often_as_wanted() {
        let loaded_from = Url::parse("https://shop.example/catalogue/list?page=2").unwrap();
        let html = r#"<html><head><base href="/elsewhere/"><title>T</title></head>
            <body><nav><a href="menu">menu</a></nav><p>Some text.</p></body></html>"#;
        let document = Document::parse(html, &loaded_from);
        assert_eq!(document.loaded_from(), &loaded_from);
        assert_eq!(document.base().as_str(), "https://shop.example/elsewhere/");
        assert_eq!(document.tree().root_element().value().name(), "html");
        // The same page every time, and the one `read` gives.
        assert_eq!(document.page(), document.page());
        assert_eq!(document.page(), read(html, &loaded_from));
        assert_eq!(
            hrefs(&document.page()),
            ["https://shop.example/elsewhere/menu"]
        );
    }

    #[test]
    fn what_is_no_html_reads_as_text_without_links() {
        let plain = page("just a line of text");
        assert_eq!(plain.text, "just a line of text");
        assert_eq!(plain.links, Vec::new());
        assert_eq!(plain.title, "");
        let empty = page("");
        assert_eq!(empty, Page::default());
        // A document cut off in the middle of a tag still gives what it had.
        let cut = page(r#"<p>before <a href="/kept">kept</a> and <a href="/lo"#);
        assert_eq!(hrefs(&cut), ["https://shop.example/kept"]);
        assert!(cut.text.starts_with("before kept and"));
    }
}
