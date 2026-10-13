//! The main content of a page as text and as markdown (spec §14).
//!
//! One walk writes either: the text a reader would copy out, one block
//! after another, or the same blocks in markdown with their headings,
//! lists, quotes, code, tables, links and images. Both come from the same
//! elements, so they never disagree about what the content is.
//!
//! Nothing here is recursive: every element that is a block or a mark of
//! its own has a frame on a stack, and a frame that closes hands what it
//! wrote to the one under it.

use std::collections::HashSet;

use ego_tree::NodeId;
use url::Url;

use crate::content::{Content, is_data_table};
use crate::dom::{
    Dropped, NodeRef, Step, as_element, as_text, heading_level, is_block, unseen, walk,
};

/// What is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Plain text: blocks with an empty line between them, the items of a
    /// list and the rows of a table one to a line.
    Text,
    /// Markdown.
    Markdown,
}

/// A heading of the content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Heading {
    /// 1 for `h1` to 6 for `h6`.
    pub(crate) level: u8,
    /// Its text.
    pub(crate) text: String,
    /// Its `id`, when it has one: the fragment that leads to it.
    pub(crate) id: Option<String>,
}

/// What was written of a page's content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Rendered {
    /// The blocks, joined.
    pub(crate) text: String,
    /// The headings met, in order.
    pub(crate) headings: Vec<Heading>,
}

/// What an open element is to the writer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// Holds blocks; loose text in it is paragraphs.
    Container,
    /// One paragraph.
    Paragraph,
    /// A term of a definition list.
    Term,
    Heading(u8),
    Quote,
    /// Text as it is written, white space kept.
    Pre,
    List {
        ordered: bool,
    },
    Item,
    /// A table of data, with its rows as they close.
    Table,
    Row,
    Cell,
    /// Inline text that is marked on both sides in markdown.
    Marked(&'static str),
    Code,
    Link(Option<String>),
}

#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // Four things a writer has to remember, none a state of another.
struct Frame {
    kind: Kind,
    /// Blocks that are finished.
    blocks: Vec<String>,
    /// The paragraph being written.
    inline: String,
    /// A space is owed before the next word.
    space_owed: bool,
    /// The text began with white space.
    led_by_space: bool,
    /// The last thing written was a line break.
    broke: bool,
    /// Items of a list, rows of a table.
    rows: Vec<Vec<String>>,
    /// Whether a row holds header cells.
    header: bool,
    /// The number of the next item of an ordered list.
    next: u64,
    /// The language of a code block.
    language: Option<String>,
    id: Option<String>,
    /// Whether a paragraph that would read as the start of a markdown
    /// block gets its first characters escaped.
    guarded: bool,
}

impl Frame {
    fn new(kind: Kind, mode: Mode) -> Self {
        let guarded = mode == Mode::Markdown
            && matches!(
                kind,
                Kind::Container | Kind::Paragraph | Kind::Quote | Kind::Item
            );
        Self {
            kind,
            blocks: Vec::new(),
            inline: String::new(),
            space_owed: false,
            led_by_space: false,
            broke: false,
            rows: Vec::new(),
            header: false,
            next: 1,
            language: None,
            id: None,
            guarded,
        }
    }

    const fn is_inline(&self) -> bool {
        matches!(self.kind, Kind::Marked(_) | Kind::Code | Kind::Link(_))
    }

    /// Appends inline text that is already written (words, a link).
    fn push(&mut self, written: &str, led: bool, trailed: bool) {
        if written.is_empty() {
            if led || trailed {
                if self.inline.is_empty() {
                    self.led_by_space = true;
                } else {
                    self.space_owed = true;
                }
            }
            return;
        }
        if self.inline.is_empty() {
            self.led_by_space = self.led_by_space || led || self.space_owed;
        } else if (self.space_owed || led) && !self.broke {
            self.inline.push(' ');
        }
        self.inline.push_str(written);
        self.space_owed = trailed;
        self.broke = false;
    }

    /// Ends the paragraph being written, when there is one.
    fn flush(&mut self) {
        let paragraph = self.inline.trim();
        if !paragraph.is_empty() {
            self.blocks.push(if self.guarded {
                guard_start(paragraph)
            } else {
                paragraph.to_owned()
            });
        }
        self.inline.clear();
        self.space_owed = false;
        self.broke = false;
    }
}

struct Writer<'a> {
    mode: Mode,
    base: &'a Url,
    dropped: &'a Dropped,
    stack: Vec<Frame>,
    /// For every open element, whether it has a frame of its own.
    framed: Vec<bool>,
    headings: Vec<Heading>,
    /// Whether the open element keeps its white space.
    in_pre: bool,
    /// The kind of every open `table`: whether it is one of data.
    tables: Vec<bool>,
}

/// Characters that would be read as markdown where they are text.
fn escape(text: &str, out: &mut String) {
    for c in text.chars() {
        if matches!(c, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|') {
            out.push('\\');
        }
        out.push(c);
    }
}

/// A paragraph whose first characters would start a markdown block gets
/// them escaped.
fn guard_start(block: &str) -> String {
    let digits = block.chars().take_while(char::is_ascii_digit).count();
    let numbered =
        digits > 0 && digits <= 9 && matches!(block[digits..].chars().next(), Some('.' | ')'));
    let marked = ["# ", "##", "+ ", "- ", "---", "==="]
        .iter()
        .any(|start| block.starts_with(start));
    if marked {
        format!("\\{block}")
    } else if numbered {
        format!("{}\\{}", &block[..digits], &block[digits..])
    } else {
        block.to_owned()
    }
}

/// The longest run of a character in a text.
fn longest_run(text: &str, of: char) -> usize {
    let (mut longest, mut run) = (0, 0);
    for c in text.chars() {
        run = if c == of { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    longest
}

/// A block with `first` before its first line and `rest` before the
/// others.
fn indent(block: &str, first: &str, rest: &str) -> String {
    let mut out = String::new();
    for (number, line) in block.lines().enumerate() {
        if number > 0 {
            out.push('\n');
        }
        let prefix = if number == 0 { first } else { rest };
        if line.is_empty() {
            out.push_str(prefix.trim_end());
        } else {
            out.push_str(prefix);
            out.push_str(line);
        }
    }
    out
}

/// The language a code block names: a `language-…` or `lang-…` class of
/// the element or of the `code` directly in it.
fn language_of(node: NodeRef<'_>) -> Option<String> {
    let named = |node: NodeRef<'_>| {
        as_element(node)?
            .attr("class")?
            .split_ascii_whitespace()
            .find_map(|class| {
                class
                    .strip_prefix("language-")
                    .or_else(|| class.strip_prefix("lang-"))
            })
            .filter(|language| {
                !language.is_empty()
                    && language.len() <= 32
                    && language.chars().all(|c| {
                        c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '#' | '_' | '.')
                    })
            })
            .map(str::to_ascii_lowercase)
    };
    named(node).or_else(|| {
        node.children()
            .filter(|child| as_element(*child).is_some_and(|element| element.name() == "code"))
            .find_map(named)
    })
}

impl Writer<'_> {
    fn top(&mut self) -> Option<&mut Frame> {
        self.stack.last_mut()
    }

    /// The innermost frame that holds blocks.
    fn holder(&mut self) -> Option<&mut Frame> {
        self.stack.iter_mut().rev().find(|frame| !frame.is_inline())
    }

    /// Where an address in the page leads, when that is a web address.
    fn address(&self, written: &str) -> Option<String> {
        let url = self.base.join(written.trim()).ok()?;
        matches!(url.scheme(), "http" | "https").then(|| url.to_string())
    }

    fn open(&mut self, node: NodeRef<'_>, element: &scraper::node::Element, blocky: bool) {
        let framed = self.begin(node, element, blocky);
        self.framed.push(framed);
    }

    /// Begins an element. Returns whether it got a frame of its own.
    fn begin(&mut self, node: NodeRef<'_>, element: &scraper::node::Element, blocky: bool) -> bool {
        let name = element.name();
        let mode = self.mode;
        if self.in_pre {
            // Inside text that keeps its white space nothing is a block:
            // a line break is one, everything else is its text.
            if name == "br"
                && let Some(top) = self.top()
            {
                top.inline.push('\n');
            }
            return false;
        }
        let in_data_table = self.tables.last().copied().unwrap_or(false);
        let kind = match name {
            "br" => {
                self.line_break();
                return false;
            }
            "hr" => {
                if let Some(holder) = self.holder() {
                    holder.flush();
                    if mode == Mode::Markdown {
                        holder.blocks.push("---".to_owned());
                    }
                }
                return false;
            }
            "img" => {
                self.image(element);
                return false;
            }
            "p" | "summary" | "figcaption" | "caption" | "address" => Kind::Paragraph,
            "dt" => Kind::Term,
            "blockquote" => Kind::Quote,
            "pre" | "listing" | "xmp" | "plaintext" => Kind::Pre,
            "ul" | "menu" | "dir" => Kind::List { ordered: false },
            "ol" => Kind::List { ordered: true },
            "li" => Kind::Item,
            "table" => {
                let data = is_data_table(node, self.dropped);
                self.tables.push(data);
                if data { Kind::Table } else { Kind::Container }
            }
            "tr" if in_data_table => Kind::Row,
            "td" | "th" if in_data_table => Kind::Cell,
            // An inline element that holds blocks is a container: its
            // blocks cannot stand in a paragraph.
            _ if blocky && !is_block(name) => Kind::Container,
            "em" | "i" | "cite" | "dfn" | "var" => Kind::Marked("*"),
            "strong" | "b" => Kind::Marked("**"),
            "del" | "s" | "strike" => Kind::Marked("~~"),
            "code" | "kbd" | "samp" | "tt" => Kind::Code,
            "a" => Kind::Link(element.attr("href").and_then(|href| self.address(href))),
            _ => match heading_level(name) {
                Some(level) => Kind::Heading(level),
                None if is_block(name) => Kind::Container,
                // Inline text without a mark of its own is written into
                // whatever holds it.
                None => return false,
            },
        };
        let mut frame = Frame::new(kind, mode);
        if !frame.is_inline()
            && let Some(top) = self.top().filter(|top| !top.is_inline())
        {
            // A block begins: the paragraph being written in what holds it
            // ends.
            top.flush();
        }
        match frame.kind {
            Kind::List { ordered: true } => {
                frame.next = element
                    .attr("start")
                    .and_then(|start| start.trim().parse().ok())
                    .unwrap_or(1);
            }
            Kind::Heading(_) => frame.id = element.attr("id").map(str::to_owned),
            Kind::Cell => frame.header = name == "th",
            Kind::Pre => {
                frame.language = language_of(node);
                self.in_pre = true;
            }
            _ => {}
        }
        self.stack.push(frame);
        true
    }

    /// A `<br>`: a line break, and after a second one a new paragraph.
    fn line_break(&mut self) {
        let mode = self.mode;
        let Some(top) = self.top() else {
            return;
        };
        if top.is_inline() || matches!(top.kind, Kind::Cell | Kind::Heading(_)) {
            top.space_owed = !top.inline.is_empty();
            return;
        }
        if top.broke {
            // The second break of a chain: what was written is a paragraph.
            top.flush();
            return;
        }
        if top.inline.trim().is_empty() {
            return;
        }
        let end = top.inline.trim_end().len();
        top.inline.truncate(end);
        top.inline
            .push_str(if mode == Mode::Markdown { "  \n" } else { "\n" });
        top.space_owed = false;
        top.broke = true;
    }

    fn image(&mut self, element: &scraper::node::Element) {
        if self.mode != Mode::Markdown {
            return;
        }
        let Some(src) = element.attr("src").and_then(|src| self.address(src)) else {
            return;
        };
        let alt = element
            .attr("alt")
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let mut written = String::from("![");
        escape(&alt, &mut written);
        written.push_str("](");
        written.push_str(&src);
        written.push(')');
        if let Some(top) = self.top() {
            top.push(&written, false, false);
        }
    }

    fn text(&mut self, text: &str) {
        let (mode, in_pre) = (self.mode, self.in_pre);
        let in_code = self.stack.iter().any(|frame| frame.kind == Kind::Code);
        let Some(top) = self.top() else {
            return;
        };
        if in_pre {
            top.inline.push_str(text);
            return;
        }
        // A zero-width space is where a line may break, and a byte order
        // mark is nothing: neither is a character a reader sees.
        let seen: String;
        let text = if text.contains(['\u{200b}', '\u{feff}']) {
            seen = text.replace(['\u{200b}', '\u{feff}'], "");
            seen.as_str()
        } else {
            text
        };
        let led = text.starts_with(char::is_whitespace);
        let trailed = text.ends_with(char::is_whitespace);
        let mut words = String::new();
        for word in text.split_whitespace() {
            if !words.is_empty() {
                words.push(' ');
            }
            if mode == Mode::Markdown && !in_code {
                escape(word, &mut words);
            } else {
                words.push_str(word);
            }
        }
        top.push(&words, led, trailed);
    }

    /// Hands a finished block to whatever holds the element that closed.
    fn hand_on(&mut self, block: String) {
        if block.is_empty() {
            return;
        }
        if let Some(holder) = self.holder() {
            holder.blocks.push(block);
        }
    }

    fn close(&mut self, element: &scraper::node::Element) {
        if !self.framed.pop().unwrap_or(false) {
            return;
        }
        if element.name() == "table" {
            self.tables.pop();
        }
        let Some(mut frame) = self.stack.pop() else {
            return;
        };
        let markdown = self.mode == Mode::Markdown;
        if frame.kind == Kind::Pre {
            self.in_pre = false;
            let code = frame.inline.trim_matches('\n').trim_end();
            if code.trim().is_empty() {
                return;
            }
            let block = if markdown {
                let fence = "`".repeat(longest_run(code, '`').max(2) + 1);
                format!(
                    "{fence}{}\n{code}\n{fence}",
                    frame.language.as_deref().unwrap_or_default()
                )
            } else {
                code.to_owned()
            };
            self.hand_on(block);
            return;
        }
        if frame.is_inline() {
            let inner = frame.inline.trim();
            let written = match (&frame.kind, markdown && !inner.is_empty()) {
                (Kind::Marked(mark), true) => format!("{mark}{inner}{mark}"),
                (Kind::Code, true) => {
                    let fence = "`".repeat(longest_run(inner, '`') + 1);
                    let pad = if inner.starts_with('`') || inner.ends_with('`') {
                        " "
                    } else {
                        ""
                    };
                    format!("{fence}{pad}{inner}{pad}{fence}")
                }
                (Kind::Link(Some(address)), true) => format!("[{inner}]({address})"),
                _ => inner.to_owned(),
            };
            if let Some(top) = self.top() {
                top.push(&written, frame.led_by_space, frame.space_owed);
            }
            return;
        }
        frame.flush();
        let block = self.finish(frame, markdown);
        self.hand_on(block);
    }

    /// What a block that closed wrote. A block that hands itself to
    /// something other than its holder (an item to its list, a cell to its
    /// row, a row to its table) returns nothing.
    #[allow(clippy::too_many_lines)] // One arm for every kind of block.
    fn finish(&mut self, mut frame: Frame, markdown: bool) -> String {
        let joined = |blocks: &[String]| blocks.join("\n\n");
        match frame.kind {
            Kind::Container | Kind::Paragraph => {
                // A container hands its blocks on as they are.
                if let Some(holder) = self.holder() {
                    holder.blocks.append(&mut frame.blocks);
                }
                String::new()
            }
            Kind::Term => {
                let term = joined(&frame.blocks);
                if markdown && !term.is_empty() {
                    format!("**{term}**")
                } else {
                    term
                }
            }
            Kind::Heading(level) => {
                let title = frame.blocks.join(" ").replace('\n', " ");
                if title.is_empty() {
                    return title;
                }
                if markdown {
                    format!("{} {title}", "#".repeat(usize::from(level)))
                } else {
                    self.headings.push(Heading {
                        level,
                        text: title.clone(),
                        id: frame.id.take().filter(|id| !id.is_empty()),
                    });
                    title
                }
            }
            Kind::Quote => {
                let quoted = joined(&frame.blocks);
                if markdown {
                    indent(&quoted, "> ", "> ")
                } else {
                    quoted
                }
            }
            Kind::List { ordered } => {
                let mut items = Vec::new();
                for (number, row) in (frame.next..).zip(&frame.rows) {
                    if markdown {
                        let marker = if ordered {
                            format!("{number}. ")
                        } else {
                            "- ".to_owned()
                        };
                        items.push(indent(
                            &row.join("\n\n"),
                            &marker,
                            &" ".repeat(marker.len()),
                        ));
                    } else {
                        items.push(row.join("\n"));
                    }
                }
                // Blocks that stood in the list outside any item.
                items.append(&mut frame.blocks);
                items.join("\n")
            }
            Kind::Item => {
                let blocks = std::mem::take(&mut frame.blocks);
                if blocks.is_empty() {
                    return String::new();
                }
                if let Some(list) = self
                    .stack
                    .iter_mut()
                    .rev()
                    .find(|frame| matches!(frame.kind, Kind::List { .. }))
                {
                    list.rows.push(blocks);
                    return String::new();
                }
                // An item outside a list is a paragraph.
                joined(&blocks)
            }
            Kind::Cell => {
                let cell = frame.blocks.join(" ").replace('\n', " ");
                if let Some(row) = self
                    .stack
                    .iter_mut()
                    .rev()
                    .find(|frame| frame.kind == Kind::Row)
                {
                    row.header = row.header || frame.header;
                    row.blocks.push(cell);
                    return String::new();
                }
                cell
            }
            Kind::Row => {
                let cells = std::mem::take(&mut frame.blocks);
                if let Some(table) = self
                    .stack
                    .iter_mut()
                    .rev()
                    .find(|frame| frame.kind == Kind::Table)
                {
                    if !cells.is_empty() {
                        table.rows.push(cells);
                    }
                    return String::new();
                }
                cells.join(" ")
            }
            Kind::Table => {
                let mut lines = Vec::new();
                let width = frame.rows.iter().map(Vec::len).max().unwrap_or(0);
                for (number, row) in frame.rows.iter().enumerate() {
                    if markdown {
                        let mut cells: Vec<&str> = row.iter().map(String::as_str).collect();
                        cells.resize(width, "");
                        lines.push(format!("| {} |", cells.join(" | ")));
                        if number == 0 {
                            lines.push(format!("|{}", " --- |".repeat(width)));
                        }
                    } else {
                        let cells: Vec<&str> = row
                            .iter()
                            .map(String::as_str)
                            .filter(|cell| !cell.is_empty())
                            .collect();
                        if !cells.is_empty() {
                            lines.push(cells.join(" "));
                        }
                    }
                }
                // A caption stands before its table.
                let caption = joined(&frame.blocks);
                let table = lines.join("\n");
                if caption.is_empty() || table.is_empty() {
                    format!("{caption}{table}")
                } else {
                    format!("{caption}\n\n{table}")
                }
            }
            Kind::Pre | Kind::Marked(_) | Kind::Code | Kind::Link(_) => String::new(),
        }
    }
}

/// The elements that are blocks or hold one.
fn blocky(root: NodeRef<'_>, dropped: &Dropped) -> HashSet<NodeId> {
    let mut found = HashSet::new();
    let mut open: Vec<bool> = vec![false];
    walk(root, dropped, |step| match step {
        Step::Open(node) => {
            if as_element(node).is_some() {
                open.push(false);
            }
        }
        Step::Close(node) => {
            let Some(element) = as_element(node) else {
                return;
            };
            let holds = open.pop().unwrap_or(false) || is_block(element.name());
            if holds {
                found.insert(node.id());
                if let Some(parent) = open.last_mut() {
                    *parent = true;
                }
            }
        }
    });
    found
}

/// Writes the content of a page in one mode. Addresses in it are resolved
/// against `base`.
pub(crate) fn render(content: &Content<'_>, base: &Url, mode: Mode) -> Rendered {
    let mut writer = Writer {
        mode,
        base,
        dropped: &content.dropped,
        stack: vec![Frame::new(Kind::Container, mode)],
        framed: Vec::new(),
        headings: Vec::new(),
        in_pre: false,
        tables: Vec::new(),
    };
    for part in &content.parts {
        let blocky = blocky(*part, &content.dropped);
        // Depth under an element a reader never sees.
        let mut unseen_depth = 0_usize;
        walk(*part, &content.dropped, |step| match step {
            Step::Open(node) => {
                if let Some(element) = as_element(node) {
                    if unseen_depth > 0 || unseen(element) {
                        unseen_depth += 1;
                    } else {
                        writer.open(node, element, blocky.contains(&node.id()));
                    }
                } else if let (Some(text), 0) = (as_text(node), unseen_depth) {
                    writer.text(text);
                }
            }
            Step::Close(node) => {
                if let Some(element) = as_element(node) {
                    if unseen_depth > 0 {
                        unseen_depth -= 1;
                    } else {
                        writer.close(element);
                    }
                }
            }
        });
        if let Some(root) = writer.stack.first_mut() {
            root.flush();
        }
    }
    let text = writer
        .stack
        .first()
        .map(|root| root.blocks.join("\n\n"))
        .unwrap_or_default();
    Rendered {
        text,
        headings: writer.headings,
    }
}

#[cfg(test)]
mod tests;
