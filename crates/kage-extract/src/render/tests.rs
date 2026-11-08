//! The writer of text and markdown, one kind of element at a time.

use scraper::Html;
use url::Url;

use super::{Mode, guard_heading_end, guard_start, indent, longest_run, render};
use crate::content::Content;
use crate::dom::Dropped;

/// Everything in the markup, written in one mode.
fn write(markup: &str, mode: Mode) -> String {
    let html = Html::parse_document(&format!("<body>{markup}</body>"));
    let content = Content {
        parts: vec![html.tree.root()],
        dropped: Dropped::new(),
    };
    let base = Url::parse("https://docs.example/guide/page").unwrap();
    render(&content, &base, mode).text
}

fn markdown(markup: &str) -> String {
    write(markup, Mode::Markdown)
}

fn text(markup: &str) -> String {
    write(markup, Mode::Text)
}

#[test]
fn blocks_are_paragraphs_with_an_empty_line_between_them() {
    let markup = "<h1>A  title</h1><p>First   paragraph,\n on two lines.</p>\
                  <div>Loose text.<p>Then a paragraph.</p>More loose text.</div>";
    let expected = "A title\n\nFirst paragraph, on two lines.\n\nLoose text.\n\nThen a \
                    paragraph.\n\nMore loose text.";
    assert_eq!(text(markup), expected);
    assert_eq!(markdown(markup), format!("# {expected}"));
    assert_eq!(text(""), "");
    assert_eq!(markdown("<p>  </p><div></div>"), "");
}

#[test]
fn headings_have_their_level_and_are_listed() {
    let markup = r#"<h2 id="one">One</h2><p>x</p><h3>Two <em>levels</em> down</h3><h6 id="">Six</h6><h4> </h4>"#;
    assert_eq!(
        markdown(markup),
        "## One\n\nx\n\n### Two *levels* down\n\n###### Six"
    );
    let html = Html::parse_document(&format!("<body>{markup}</body>"));
    let content = Content {
        parts: vec![html.tree.root()],
        dropped: Dropped::new(),
    };
    let base = Url::parse("https://docs.example/").unwrap();
    let rendered = render(&content, &base, Mode::Text);
    let listed: Vec<(u8, &str, Option<&str>)> = rendered
        .headings
        .iter()
        .map(|heading| (heading.level, heading.text.as_str(), heading.id.as_deref()))
        .collect();
    assert_eq!(
        listed,
        [
            (2, "One", Some("one")),
            (3, "Two levels down", None),
            (6, "Six", None)
        ]
    );
    // Markdown does not list them a second time.
    assert_eq!(render(&content, &base, Mode::Markdown).headings, Vec::new());
}

#[test]
fn inline_marks_keep_the_spaces_around_them() {
    let markup = "<p>A <b>bold</b> word, an<i> italic </i>one, <del>gone</del>, \
                  <code>code()</code> and <strong></strong>nothing.</p>";
    assert_eq!(
        markdown(markup),
        "A **bold** word, an *italic* one, ~~gone~~, `code()` and nothing."
    );
    assert_eq!(
        text(markup),
        "A bold word, an italic one, gone, code() and nothing."
    );
    // No space is invented where the page has none.
    assert_eq!(markdown("<p>un<b>bold</b>ed</p>"), "un**bold**ed");
    assert_eq!(
        text(r#"<p>see <a href="/x">the papers</a>, which</p>"#),
        "see the papers, which"
    );
    // Code keeps what would be a mark, and a backtick in it is fenced wider.
    assert_eq!(
        markdown("<p><code>a_b*c</code> and <code>a`b</code> and <code>`</code></p>"),
        "`a_b*c` and ``a`b`` and `` ` ``"
    );
    assert_eq!(
        markdown("<p>Stars * and _under_ and [brackets] and a|pipe and back\\slash.</p>"),
        "Stars \\* and \\_under\\_ and \\[brackets\\] and a\\|pipe and back\\\\slash."
    );
    // A mark nobody sees is not written: the anchor of a heading that
    // holds a zero-width space.
    assert_eq!(
        markdown("<h2>Section<a href=\"#section\">\u{200b}</a></h2>"),
        "## Section"
    );
}

#[test]
fn links_and_images_lead_to_absolute_addresses() {
    let markup = r##"<p><a href="../other#part">Relative</a>, <a href="https://else.example/">absolute</a>,
        <a href="javascript:void(0)">no address</a>, <a>no href</a>, <a href="#top"></a>
        <a href="/img"><img src="/pic.png" alt="A  picture"></a> <img src="data:image/gif;base64,AAAA" alt="inline">
        <img alt="no source"></p>"##;
    assert_eq!(
        markdown(markup),
        "[Relative](https://docs.example/other#part), [absolute](https://else.example/), no \
         address, no href, [![A picture](https://docs.example/pic.png)](https://docs.example/img)"
    );
    // A reader of the text sees the words of the links, and no picture.
    assert_eq!(text(markup), "Relative, absolute, no address, no href,");
}

#[test]
fn lists_are_numbered_and_nested() {
    let markup = "<ul><li>One</li><li>Two<ul><li>Two a</li><li>Two b</li></ul></li>\
                  <li><p>Three</p><p>with a second paragraph</p></li><li> </li></ul>\
                  <ol start=\"4\"><li>Four</li><li>Five</li></ol>";
    assert_eq!(
        markdown(markup),
        "- One\n- Two\n\n  - Two a\n  - Two b\n- Three\n\n  with a second paragraph\n\n4. \
         Four\n5. Five"
    );
    assert_eq!(
        text(markup),
        "One\nTwo\nTwo a\nTwo b\nThree\nwith a second paragraph\n\nFour\nFive"
    );
    // An item without a list is a paragraph.
    assert_eq!(markdown("<li>Alone</li>"), "Alone");
}

#[test]
fn quotes_code_and_rules() {
    let markup = "<blockquote><p>First line.</p><p>Second line.</p></blockquote><hr>\
                  <pre><code class=\"language-Rust\">fn main() {\n    let x = 1;\n}\n</code></pre>\
                  <pre>plain\n  kept</pre><pre> </pre>";
    assert_eq!(
        markdown(markup),
        "> First line.\n>\n> Second line.\n\n---\n\n```rust\nfn main() {\n    let x = 1;\n}\n```\n\n```\nplain\n  kept\n```"
    );
    assert_eq!(
        text(markup),
        "First line.\n\nSecond line.\n\nfn main() {\n    let x = 1;\n}\n\nplain\n  kept"
    );
    // Lines a page breaks with <br> inside marked-up code stay lines, and
    // a fence is longer than any run of backticks in the code.
    let lines = "<pre class=\"lang-toml\"><code><span>a = 1</span><br><span>b = \"```\"</span><br></code></pre>";
    assert_eq!(markdown(lines), "````toml\na = 1\nb = \"```\"\n````");
    assert_eq!(longest_run("a``b```c", '`'), 3);
    assert_eq!(indent("a\n\nb", "> ", "> "), "> a\n>\n> b");
}

#[test]
fn a_table_of_data_is_a_table_and_a_table_for_layout_is_its_cells() {
    let data = "<table><caption>Fees</caption><thead><tr><th>Kind</th><th>Fee</th></tr></thead>\
                <tbody><tr><td>Small</td><td>10 | 12</td></tr><tr><td>Large</td></tr></tbody></table>";
    assert_eq!(
        markdown(data),
        "Fees\n\n| Kind | Fee |\n| --- | --- |\n| Small | 10 \\| 12 |\n| Large |  |"
    );
    assert_eq!(text(data), "Fees\n\nKind Fee\nSmall 10 | 12\nLarge");
    // A table that lays a page out has no rows to speak of.
    let layout = "<table><tr><td><p>Left column.</p></td><td>Right column.</td></tr></table>";
    assert_eq!(markdown(layout), "Left column.\n\nRight column.");
    assert_eq!(text(layout), "Left column.\n\nRight column.");
}

#[test]
fn line_breaks_and_what_two_of_them_mean() {
    let markup =
        "<div>First line<br>second line<br><br>A new paragraph<br>  <br>\n<br>and a third</div>";
    assert_eq!(
        text(markup),
        "First line\nsecond line\n\nA new paragraph\n\nand a third"
    );
    assert_eq!(
        markdown(markup),
        "First line  \nsecond line\n\nA new paragraph\n\nand a third"
    );
    // A break in text without a mark of its own breaks the block it
    // stands in; in a heading or a link it is a space.
    assert_eq!(
        text("<table><tr><td><font>One<br><br>Two</font></td></tr></table>"),
        "One\n\nTwo"
    );
    assert_eq!(
        text(r#"<h2>One<br>line</h2><p><a href="/x">a<br>b</a></p>"#),
        "One line\n\na b"
    );
    assert_eq!(
        text("<p><br>Starts after a break</p>"),
        "Starts after a break"
    );
}

#[test]
fn terms_figures_and_inline_elements_that_hold_blocks() {
    let markup = r#"<dl><dt>Term</dt><dd>Its meaning.</dd></dl>
        <figure><img src="/a.png" alt="A"><figcaption>A caption.</figcaption></figure>
        <a href="/card"><div>A card</div><p>with text</p></a>
        <details><summary>More</summary><p>Hidden until opened.</p></details>"#;
    assert_eq!(
        markdown(markup),
        "**Term**\n\nIts meaning.\n\n![A](https://docs.example/a.png)\n\nA caption.\n\nA \
         card\n\nwith text\n\nMore\n\nHidden until opened."
    );
}

#[test]
fn what_would_start_a_markdown_block_is_escaped() {
    for (written, guarded) in [
        ("# not a heading", "\\# not a heading"),
        ("## nor this", "\\## nor this"),
        ("- not an item", "\\- not an item"),
        ("+ nor this", "\\+ nor this"),
        ("--- no rule", "\\--- no rule"),
        ("1. not a list", "1\\. not a list"),
        ("12) nor this", "12\\) nor this"),
        ("1984 was a year", "1984 was a year"),
        ("#hashtag", "#hashtag"),
        ("-5 degrees", "-5 degrees"),
        ("plain", "plain"),
    ] {
        assert_eq!(guard_start(written), guarded, "{written}");
    }
    assert_eq!(
        markdown("<p># heading?</p><div>1. first</div>"),
        "\\# heading?\n\n1\\. first"
    );
    // What a reader never sees is not written.
    assert_eq!(
        text(
            r#"<p>seen</p><p hidden>not</p><script>var x;</script><span aria-hidden="true">decoration</span>"#
        ),
        "seen"
    );
}

/// QA round two's http-07 (F173): quotes and lists nested in one another
/// past [`super::MAX_NESTING`] are written as the blocks they hold, so the
/// markdown of a page grows with the page and not with the square of its
/// depth, and its writing takes time in proportion.
#[test]
fn nesting_grows_the_markdown_linearly() {
    let nested = |kind: &str, depth: usize| {
        let (open, close) = match kind {
            "quote" => ("<blockquote>word ", "</blockquote>"),
            _ => ("<ul><li>word ", "</li></ul>"),
        };
        format!("{}bottom{}", open.repeat(depth), close.repeat(depth))
    };
    for kind in ["quote", "list"] {
        let page = nested(kind, 4_000);
        // The parse is html5ever's, bounded on its own (F147): the time
        // measured is the writing's.
        let html = Html::parse_document(&format!("<body>{page}</body>"));
        let content = Content {
            parts: vec![html.tree.root()],
            dropped: Dropped::new(),
        };
        let base = Url::parse("https://docs.example/guide/page").unwrap();
        let started = std::time::Instant::now();
        let written = render(&content, &base, Mode::Markdown).text;
        let plain = render(&content, &base, Mode::Text).text;
        // Every word is there, and no line carries more prefixes than
        // the writer nests.
        assert_eq!(written.matches("word").count(), 4_000, "{kind}");
        assert!(written.ends_with("bottom"), "{kind}");
        assert_eq!(plain.matches("word").count(), 4_000, "{kind}");
        let widest = written.lines().map(str::len).max().unwrap_or(0);
        assert!(widest <= 3 * super::MAX_NESTING + 8, "{kind}: {widest}");
        assert!(
            written.len() < 40 * page.len(),
            "{kind}: {} chars",
            written.len()
        );
        // Four thousand levels took 22 s on the finding's machine; the
        // writing takes tens of milliseconds.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{kind}"
        );
    }
    // Up to the bound a quote is a quote and a list a list.
    let shallow = markdown(
        "<blockquote>a <blockquote>b <blockquote>c</blockquote></blockquote></blockquote>",
    );
    assert_eq!(shallow, "> a\n>\n> > b\n> >\n> > > c");
    let deepest = markdown(&nested("quote", super::MAX_NESTING + 2));
    let last = deepest.lines().last().unwrap_or_default();
    assert_eq!(
        last,
        format!("{}word bottom", "> ".repeat(super::MAX_NESTING))
    );
    let nested_list = markdown(&nested("list", super::MAX_NESTING + 2));
    let long: Vec<&str> = nested_list
        .lines()
        .filter(|line| line.len() > 2 * super::MAX_NESTING + 12)
        .collect();
    assert_eq!(long, Vec::<&str>::new(), "{nested_list}");
}

/// QA round one's extract-10 (F142): what the page says is what a markdown
/// reader shows: tildes that would open a fence or strike text through,
/// an `&` that would read as an entity, a parenthesis in an address and a
/// `|` in code in a table's cell are escaped.
#[test]
fn markdown_says_what_the_page_says() {
    assert_eq!(
        markdown("<p>~~~</p><p>after the tildes</p><p>~~not struck~~</p>"),
        "\\~\\~\\~\n\nafter the tildes\n\n\\~\\~not struck\\~\\~"
    );
    assert_eq!(
        markdown(
            "<p>&amp;amp; means an ampersand, &amp;#38; and &amp;#x26; too, &amp; alone, AT&amp;T</p>"
        ),
        r"\&amp; means an ampersand, \&#38; and \&#x26; too, & alone, AT&T"
    );
    assert_eq!(
        markdown(r#"<p><a href="/a)b">paren</a> <img src="/i(1).png" alt="i"></p>"#),
        r"[paren](https://docs.example/a\)b) ![i](https://docs.example/i\(1\).png)"
    );
    let table = "<table><thead><tr><th>a</th><th>b</th><th>c</th></tr></thead><tbody>\
                 <tr><td>1|2</td><td><code>x|y</code></td><td><pre>p|q</pre></td></tr></tbody></table>";
    assert_eq!(
        markdown(table),
        "| a | b | c |\n| --- | --- | --- |\n| 1\\|2 | `x\\|y` | ``` p\\|q ``` |"
    );
    // Outside a table, code keeps its `|`.
    assert_eq!(markdown("<p><code>x|y</code></p>"), "`x|y`");
}

/// QA round three's extract-01: a code span holds no code span, so nested
/// `<code>` elements are one span with their text, and the markdown grows
/// with the page, not with the square of its depth.
#[test]
fn nested_code_is_one_code_span() {
    assert_eq!(
        markdown("<p><code>a <code>b <code>c</code></code> d</code></p>"),
        "`a b c d`"
    );
    // What is inline in code is its text: marks and links are not written
    // where a reader would see them as characters.
    assert_eq!(
        markdown(r#"<p><code>x <b>y</b> <a href="/z">z</a> <kbd>k</kbd></code></p>"#),
        "`x y z k`"
    );
    // Code in a link or in a mark is still code.
    assert_eq!(
        markdown(r#"<p><a href="/z"><code>z</code></a> <b><code>y</code></b></p>"#),
        "[`z`](https://docs.example/z) **`y`**"
    );
    assert_eq!(text("<p><code>a <code>b</code> c</code></p>"), "a b c");
    let depth = 20_000;
    let page = format!(
        "<p>{}bottom{}</p>",
        "<code>w ".repeat(depth),
        "</code>".repeat(depth)
    );
    let html = Html::parse_document(&format!("<body>{page}</body>"));
    let content = Content {
        parts: vec![html.tree.root()],
        dropped: Dropped::new(),
    };
    let base = Url::parse("https://docs.example/guide/page").unwrap();
    let started = std::time::Instant::now();
    let written = render(&content, &base, Mode::Markdown).text;
    assert_eq!(written.matches('`').count(), 2);
    assert_eq!(written.matches('w').count(), depth);
    assert!(written.len() < page.len(), "{} chars", written.len());
    // Four thousand levels took 26 s in the finding, twenty thousand more
    // than half an hour.
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

/// QA round three's extract-02: what is text in the page is text in the
/// markdown also where a hard break starts a line, where a mark stands
/// alone, before a link, and at a heading's end.
#[test]
fn text_reads_as_text_after_a_break_alone_and_before_a_link() {
    for (written, guarded) in [
        ("#", r"\#"),
        ("-", r"\-"),
        ("+", r"\+"),
        ("###", r"\###"),
        ("=", r"\="),
        ("===", r"\==="),
        ("== ", r"\== "),
        ("#\tx", "\\#\tx"),
        (
            "a  \n- b  \n1. c  \n# d  \n===",
            "a  \n\\- b  \n1\\. c  \n\\# d  \n\\===",
        ),
        ("--", r"\--"),
        ("-- ", r"\-- "),
        ("=x", "=x"),
        ("-x", "-x"),
        ("--x", "--x"),
        ("+1", "+1"),
    ] {
        assert_eq!(guard_start(written), guarded, "{written:?}");
    }
    assert_eq!(
        markdown(r#"<p>Wow!<a href="/x">link</a> and wow! <a href="/y">spaced</a></p>"#),
        r"Wow\![link](https://docs.example/x) and wow! [spaced](https://docs.example/y)"
    );
    assert_eq!(markdown("<p>#</p><p>-</p><p>+</p>"), "\\#\n\n\\-\n\n\\+");
    assert_eq!(
        markdown("<p>Features:<br>- Waterproof<br>1. First</p>"),
        "Features:  \n\\- Waterproof  \n1\\. First"
    );
    assert_eq!(
        markdown("<p>Title<br># hash</p><p>Title two<br>===</p><p>Title four<br>-</p>"),
        "Title  \n\\# hash\n\nTitle two  \n\\===\n\nTitle four  \n\\-"
    );
    // Two dashes alone made a setext `<h2>` of the line before them (the
    // planner's review of extract-02).
    assert_eq!(markdown("<p>Title<br>--</p>"), "Title  \n\\--");
    assert_eq!(markdown("<h2>Section #</h2>"), r"## Section \#");
    assert_eq!(markdown("<h3>#</h3>"), r"### \#");
    assert_eq!(markdown("<h3>C#</h3>"), "### C#");
    assert_eq!(guard_heading_end("a ##".to_owned()), r"a \##");
    assert_eq!(guard_heading_end("plain".to_owned()), "plain");
    // Text keeps every character as it is.
    assert_eq!(text(r#"<p>Wow!<a href="/x">link</a></p>"#), "Wow!link");
}
