//! The writer of text and markdown, one kind of element at a time.

use scraper::Html;
use url::Url;

use super::{Mode, guard_start, indent, longest_run, render};
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
