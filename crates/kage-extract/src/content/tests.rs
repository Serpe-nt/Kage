//! The rules for what the content of a page is, one at a time.

use scraper::Html;

use super::{
    Rules, alike, clean, commas_in, find, has_hid, is_byline, is_data_table, is_share, measure,
    name_weight, runs_of, strip,
};
use crate::dom::{Dropped, as_element, text_under};

const ALL: Rules = Rules {
    strip_unlikely: true,
    weigh_names: true,
    clean: true,
};

/// A paragraph a reader would call one: 30 words, three commas.
const PARAGRAPH: &str = "The council met on Tuesday, in the old hall by the river, and agreed, \
                         after a long evening of argument, on the budget for the coming year \
                         and on the repairs to the bridge.";

fn paragraphs(count: usize) -> String {
    format!("<p>{PARAGRAPH}</p>").repeat(count)
}

/// The content of a page as one line of text per part.
fn content_of(html: &str) -> Vec<String> {
    let html = Html::parse_document(html);
    let content = find(html.tree.root());
    content
        .parts
        .iter()
        .map(|part| text_under(*part, &content.dropped))
        .filter(|text| !text.is_empty())
        .collect()
}

fn one(html: &str) -> String {
    content_of(html).join(" | ")
}

#[test]
fn the_content_is_the_element_its_paragraphs_give_the_most_points() {
    let page = format!(
        r#"<body><div id="page"><div class="top"><a href="/">Home</a> <a href="/a">A</a> <a href="/b">B</a></div>
        <div class="story"><h1>The budget</h1>{}</div>
        <div class="side"><p>Elsewhere: <a href="/x">a link</a>, <a href="/y">another link</a>, <a href="/z">a third link to read</a>.</p></div>
        </div></body>"#,
        paragraphs(5)
    );
    let found = one(&page);
    assert!(found.starts_with("The budget The council met"), "{found}");
    assert!(found.ends_with("repairs to the bridge."), "{found}");
    assert!(!found.contains("Home") && !found.contains("Elsewhere"));
}

#[test]
fn what_the_markup_calls_furniture_is_never_content() {
    let page = format!(
        r#"<body><main><article>{}
        <nav><p>{PARAGRAPH}</p></nav><aside><p>{PARAGRAPH}</p></aside><footer><p>{PARAGRAPH}</p></footer>
        <div role="dialog"><p>We use cookies, and so does everybody, which is why this box is here, there and everywhere.</p></div>
        <div role="Navigation"><p>{PARAGRAPH}</p></div><div role="complementary"><p>{PARAGRAPH}</p></div>
        </article></main></body>"#,
        paragraphs(4)
    );
    let found = one(&page);
    assert_eq!(found.matches("The council met").count(), 4, "{found}");
    assert!(!found.contains("cookies"));
}

#[test]
fn a_page_without_a_candidate_has_no_content() {
    assert_eq!(content_of(""), Vec::<String>::new());
    assert_eq!(
        content_of(
            r#"<body><noscript>Enable JavaScript.</noscript><div id="root"></div><div aria-hidden="true">Loading…</div></body>"#
        ),
        Vec::<String>::new()
    );
    // Nothing but furniture.
    assert_eq!(
        content_of(&format!("<body><nav><p>{PARAGRAPH}</p></nav></body>")),
        Vec::<String>::new()
    );
    // Text that stands in no paragraph gives no points to anything.
    assert_eq!(content_of("<body>Loading</body>"), Vec::<String>::new());
}

#[test]
fn a_short_page_is_found_by_a_pass_with_fewer_rules() {
    // One paragraph in an element with an unlikely name: the first pass
    // strips it, and the pass without that rule finds it.
    let page = format!(r#"<body><div class="sidebar"><p>{PARAGRAPH}</p></div></body>"#);
    assert_eq!(one(&page), PARAGRAPH);
    // The same with a name that lowers it and nothing else on the page.
    let page = format!(r#"<body><div class="promo"><p>{PARAGRAPH}</p></div></body>"#);
    assert_eq!(one(&page), PARAGRAPH);
}

#[test]
fn elements_with_unlikely_names_are_left_out_unless_a_name_keeps_them() {
    let page = format!(
        r#"<body><div class="post-content">{}
        <div class="comments"><p>{PARAGRAPH}</p><p>{PARAGRAPH}</p></div>
        <div class="social-footer"><p>{PARAGRAPH}</p></div>
        <div class="main-menu-and-content"><p>Kept: its name says content too, whatever else it says.</p></div>
        </div></body>"#,
        paragraphs(5)
    );
    let found = one(&page);
    assert_eq!(found.matches("The council met").count(), 5, "{found}");
    assert!(found.contains("Kept: its name says content too"));
    // In a table and in code a name means nothing.
    let html = Html::parse_document(
        r#"<body><table><tr><td class="comment">kept</td></tr></table><code><span class="comment">// kept</span></code>
        <a class="social">kept link</a><p class="comment">dropped</p></body>"#,
    );
    let dropped = strip(html.tree.root(), ALL);
    assert_eq!(
        text_under(html.tree.root(), &dropped),
        "kept // kept kept link"
    );
}

#[test]
fn bylines_are_not_content() {
    let page = format!(
        r#"<body><article><h1>The budget</h1>
        <p class="byline">By <a rel="author" href="/a">Ada Example</a></p>
        <div class="post-meta"><span itemprop="author">Ada Example</span> <span class="dateline">Riverside, Tuesday</span></div>
        {}<p>She said the <span class="author-note">author of the plan</span> was pleased.</p></article></body>"#,
        paragraphs(5)
    );
    let found = one(&page);
    assert!(
        !found.contains("Ada Example") && !found.contains("Riverside"),
        "{found}"
    );
    assert!(found.contains("She said the was pleased."), "{found}");
    for (markup, name, expected) in [
        (r#"<p class="byline">By A</p>"#, "p", true),
        (r#"<a rel="nofollow AUTHOR">A</a>"#, "a", true),
        (r#"<span itemprop="Author">A</span>"#, "span", true),
        (
            r#"<div id="written-by" class="writtenby">A</div>"#,
            "div",
            true,
        ),
        (r#"<p class="lead">By A</p>"#, "p", false),
        (r#"<p class="byline"></p>"#, "p", false),
    ] {
        let html = Html::parse_document(&format!("<body>{markup}</body>"));
        let node = html
            .tree
            .nodes()
            .find(|node| as_element(*node).is_some_and(|element| element.name() == name))
            .unwrap();
        assert_eq!(
            is_byline(node, as_element(node).unwrap()),
            expected,
            "{markup}"
        );
    }
    // A long text under such a name is no byline.
    let long = format!(r#"<body><div class="author"><p>{PARAGRAPH}</p></div></body>"#);
    assert_eq!(one(&long), PARAGRAPH);
}

#[test]
fn what_repeats_the_shape_of_the_content_is_content() {
    let post = |number: usize, class: &str| {
        format!(
            r#"<div class="post {class}"><div class="inner"><dl class="profile"><dt>user{number}</dt><dd>Posts: 12</dd></dl>
            <div class="postbody"><h3>Re: the bridge</h3><div class="content">Post {number} says this: {PARAGRAPH}<br><br>{PARAGRAPH}</div>
            <div class="signature">Signature {number}</div></div></div></div>"#
        )
    };
    let page = format!(
        "<body><div id=\"wrap\"><h2>The bridge</h2>{}{}{}{}</div></body>",
        post(1, "bg1"),
        post(2, "bg2"),
        post(3, "bg1"),
        post(4, "bg2")
    );
    let found = content_of(&page);
    assert_eq!(found.len(), 4, "{found:?}");
    for (number, part) in found.iter().enumerate() {
        assert!(
            part.starts_with(&format!("Post {} says this:", number + 1)),
            "{part}"
        );
        assert!(!part.contains("Signature") && !part.contains("Posts: 12"));
    }
    // Two alike are not a pattern: the one chosen stands alone.
    let two = format!(
        "<body><div id=\"wrap\">{}{}</div></body>",
        post(1, "bg1"),
        post(2, "bg2")
    );
    assert!(content_of(&two).len() < 3);
    let html = Html::parse_document(
        r#"<body><div class="a b">1</div><div class="b c">2</div><div class="d">3</div><p>4</p><div>5</div><div>6</div></body>"#,
    );
    let nodes: Vec<_> = html
        .tree
        .nodes()
        .filter(|node| {
            as_element(*node).is_some_and(|element| matches!(element.name(), "div" | "p"))
        })
        .collect();
    assert!(alike(nodes[0], nodes[1]));
    assert!(!alike(nodes[0], nodes[2]));
    assert!(!alike(nodes[2], nodes[3]));
    assert!(!alike(nodes[2], nodes[4]));
    assert!(alike(nodes[4], nodes[5]));
}

#[test]
fn siblings_that_read_like_the_content_and_figures_between_them_belong() {
    let page = format!(
        r#"<body><article>
        <div class="intro"><p>{PARAGRAPH}</p><p>{PARAGRAPH}</p></div>
        <figure><img src="/bridge.jpg" alt=""><figcaption>The bridge at dawn.</figcaption></figure>
        <div class="body-text">{}</div>
        <figure><img src="/after.jpg" alt=""><figcaption>After the content.</figcaption></figure>
        <div class="tiny"><a href="/1">One</a> <a href="/2">Two</a></div>
        <p>A closing sentence that is long enough to stand with the article as a paragraph of its own, said plainly.</p>
        </article></body>"#,
        paragraphs(6)
    );
    let found = content_of(&page);
    let all = found.join(" | ");
    assert!(all.contains("The bridge at dawn."), "{all}");
    assert!(all.contains("A closing sentence"), "{all}");
    // The figure after the last part is not between two parts... but the
    // closing paragraph is a part, so it is.
    assert!(all.contains("After the content."), "{all}");
    assert!(!all.contains("One Two"), "{all}");
    assert_eq!(all.matches("The council met").count(), 8);
}

#[test]
fn what_is_mostly_links_or_inputs_is_cleaned_out() {
    let page = format!(
        r#"<body><div class="entry-content">{}
        <ul class="related-posts"><li><a href="/1">One more story to read</a></li><li><a href="/2">Another story to read</a></li></ul>
        <div class="box"><a href="/a">A link</a> <a href="/b">and another link</a> <a href="/c">and a third</a></div>
        <form><p>Your name</p><input name="n"><input name="e"><textarea></textarea><button>Send</button></form>
        <div class="sharedaddy"><p>Share this page with a friend.</p></div>
        <div class="ad-slot">Advertisement</div><div>Publicité</div>
        <ul><li>A plain item of the article.</li><li>And a second one.</li></ul>
        <h2 class="comment-title">Lowered heading</h2><h2>A real heading</h2>
        <table class="wikitable"><tr><th>Year</th><th>Event</th></tr><tr><td>1901</td><td>Built</td></tr></table>
        </div></body>"#,
        paragraphs(6)
    );
    let found = one(&page);
    for gone in [
        "One more story",
        "A link",
        "Your name",
        "Send",
        "Share this",
        "Advertisement",
        "Publicité",
        "Lowered heading",
    ] {
        assert!(!found.contains(gone), "{gone} in {found}");
    }
    for kept in [
        "A plain item of the article.",
        "A real heading",
        "1901 Built",
    ] {
        assert!(found.contains(kept), "{kept} not in {found}");
    }
    assert_eq!(found.matches("The council met").count(), 6);
}

#[test]
fn a_table_of_data_is_told_from_one_that_lays_a_page_out() {
    let is = |markup: &str| {
        let html = Html::parse_document(&format!("<body>{markup}</body>"));
        let table = html
            .tree
            .nodes()
            .find(|node| as_element(*node).is_some_and(|element| element.name() == "table"))
            .unwrap();
        is_data_table(table, &Dropped::new())
    };
    let rows = |rows: usize, columns: usize| {
        format!(
            "<table>{}</table>",
            format!("<tr>{}</tr>", "<td>x</td>".repeat(columns)).repeat(rows)
        )
    };
    assert!(is("<table><tr><th>A</th></tr><tr><td>1</td></tr></table>"));
    assert!(is("<table summary=\"Fees\"><tr><td>1</td></tr></table>"));
    assert!(is(
        "<table><caption>Fees <b>2026</b></caption><tr><td>1</td></tr></table>"
    ));
    assert!(is(
        "<table><thead><tr><td>A</td></tr></thead><tr><td>1</td></tr></table>"
    ));
    assert!(is(
        "<table><colgroup><col></colgroup><tr><td>1</td></tr></table>"
    ));
    assert!(!is(
        "<table role=\"presentation\"><tr><th>A</th></tr></table>"
    ));
    assert!(!is("<table datatable=\"0\"><tr><th>A</th></tr></table>"));
    assert!(!is(
        "<table><tr><td><table><tr><td>inner</td></tr></table></td></tr></table>"
    ));
    assert!(!is("<table><caption></caption><tr><td>1</td></tr></table>"));
    // By its size: ten rows, more than four columns, or more than ten cells.
    assert!(is(&rows(10, 1)));
    assert!(!is(&rows(9, 1)));
    assert!(is(&rows(1, 5)));
    assert!(!is(&rows(2, 4)));
    assert!(is(&rows(3, 4)));
    assert!(!is(&rows(5, 2)));
    assert!(is("<table><tr><td colspan=\"5\">wide</td></tr></table>"));
    assert!(!is(
        "<table><tr><td colspan=\"x\">not wide</td></tr></table>"
    ));
}

#[test]
fn text_between_line_breaks_is_scored_as_paragraphs() {
    // The page of 2002: one cell, paragraphs separated by two breaks, a
    // menu of links separated by one.
    let page = format!(
        "<body><table><tr><td width=150><a href=/a>History</a><br><a href=/b>Members</a><br><a href=/c>Links</a><br><a href=/d>Contact us today</a></td>\
         <td><font size=5><b>The railway</b></font><br><br>{PARAGRAPH}<br><br>{PARAGRAPH}<br><br>{PARAGRAPH}<br><br>{PARAGRAPH}<br><br>{PARAGRAPH}</td></tr></table></body>"
    );
    let found = one(&page);
    assert!(found.starts_with("The railway The council met"), "{found}");
    assert!(!found.contains("Members"), "{found}");
    assert_eq!(found.matches("The council met").count(), 5);

    let html = Html::parse_document(
        "<body><div id=x>Loose <b>bold</b> text<p>block</p>after<br>one break<br><br>after two<br> <br>\n<br>after three<span></span></div></body>",
    );
    let measures = measure(html.tree.root(), &Dropped::new());
    let div = html
        .tree
        .nodes()
        .find(|node| as_element(*node).is_some_and(|element| element.name() == "div"))
        .unwrap();
    let (runs, chained) = runs_of(div, &Dropped::new(), &measures);
    assert!(chained);
    let chars: Vec<usize> = runs.iter().map(|run| run.chars).collect();
    // "Loose bold text", "after one break", "after two", "after three".
    assert_eq!(chars, [16, 16, 10, 12]);
}

#[test]
fn small_rules() {
    assert_eq!(commas_in("a, b，c、d, e"), 3);
    assert!(has_hid("post hid") && has_hid("hid") && !has_hid("hidden") && !has_hid("orchid"));
    for (names, expected) in [
        ("share", true),
        ("post-share", true),
        ("share_buttons", true),
        ("sharedaddy sd-block", true),
        ("shareholder", false),
        ("timeshare", false),
        ("", false),
    ] {
        assert_eq!(is_share(names), expected, "{names}");
    }
    let weight = |markup: &str, rules: Rules| {
        let html = Html::parse_document(&format!("<body>{markup}</body>"));
        let element = html
            .tree
            .nodes()
            .filter_map(as_element)
            .find(|element| element.name() == "div")
            .unwrap();
        name_weight(element, rules)
    };
    assert!((weight(r#"<div class="article" id="main">x</div>"#, ALL) - 50.0).abs() < f64::EPSILON);
    assert!((weight(r#"<div class="sidebar" id="content">x</div>"#, ALL)).abs() < f64::EPSILON);
    assert!((weight(r#"<div class="widget related">x</div>"#, ALL) + 25.0).abs() < f64::EPSILON);
    assert!((weight(r#"<div class="article-comment">x</div>"#, ALL)).abs() < f64::EPSILON);
    assert!((weight(r#"<div class="hid">x</div>"#, ALL) + 25.0).abs() < f64::EPSILON);
    assert!((weight(r#"<div class="" id="">x</div>"#, ALL)).abs() < f64::EPSILON);
    let unweighed = Rules {
        weigh_names: false,
        ..ALL
    };
    assert!((weight(r#"<div class="article">x</div>"#, unweighed)).abs() < f64::EPSILON);

    // A cleaned part is no part: nothing of it is written.
    let html = Html::parse_document(r#"<body><div class="comment"><p>x</p></div></body>"#);
    let part = html
        .tree
        .nodes()
        .find(|node| as_element(*node).is_some_and(|element| element.name() == "div"))
        .unwrap();
    let mut dropped = Dropped::new();
    let measures = measure(html.tree.root(), &dropped);
    clean(part, &mut dropped, &measures, ALL);
    assert!(dropped.contains(&part.id()));
}
