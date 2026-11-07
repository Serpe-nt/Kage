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
//!
//! The work and the output grow with the page, not with the square of its
//! depth (F173): an element that only holds blocks (a `div`, a paragraph)
//! writes them straight into the block that keeps them, so a block is
//! moved once however many such elements stand around it; and past
//! [`MAX_NESTING`] quotes and lists in one another a quote or a list is
//! written as the blocks it holds, so no line carries more than that many
//! prefixes and no block is copied more than that many times.

use std::collections::HashSet;

use ego_tree::NodeId;
use url::Url;

use crate::content::{Content, is_data_table};
use crate::dom::{
    Dropped, NodeRef, Step, as_element, as_text, heading_level, is_block, unseen, walk,
};

/// How many quotes and lists in one another are written as such. One
/// deeper is written as the paragraphs it holds, in the innermost quote or
/// item that is: a markdown reader shows sixteen levels as well as it
/// shows a hundred, and a line of a page nested fifty thousand quotes deep
/// would otherwise carry fifty thousand `> `.
pub(crate) const MAX_NESTING: usize = 16;

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

impl Kind {
    const fn is_inline(&self) -> bool {
        matches!(self, Self::Marked(_) | Self::Code | Self::Link(_))
    }

    /// Whether a frame of this kind keeps the blocks written in it. A
    /// container or a paragraph hands them on as they are, so it writes
    /// them straight into the frame that keeps them.
    const fn keeps_blocks(&self) -> bool {
        !self.is_inline() && !matches!(self, Self::Container | Self::Paragraph)
    }
}

#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // Five things a writer has to remember, none a state of another.
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
    /// Whether this is a quote or a list nested past [`MAX_NESTING`],
    /// written as a container.
    capped: bool,
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
            capped: false,
        }
    }

    const fn is_inline(&self) -> bool {
        self.kind.is_inline()
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

    /// Ends the paragraph being written and returns it, when there is one.
    fn take_paragraph(&mut self) -> Option<String> {
        let paragraph = self.inline.trim();
        let taken = (!paragraph.is_empty()).then(|| {
            if self.guarded {
                guard_start(paragraph)
            } else {
                paragraph.to_owned()
            }
        });
        self.inline.clear();
        self.space_owed = false;
        self.broke = false;
        taken
    }
}

#[allow(clippy::struct_field_names)] // `open_*` are the frames open of a kind.
struct Writer<'a> {
    mode: Mode,
    /// Where an address written in the page leads.
    resolve: &'a dyn Fn(&str) -> Option<Url>,
    dropped: &'a Dropped,
    stack: Vec<Frame>,
    /// For every open element, whether it has a frame of its own.
    framed: Vec<bool>,
    headings: Vec<Heading>,
    /// Whether the open element keeps its white space.
    in_pre: bool,
    /// The kind of every open `table`: whether it is one of data.
    tables: Vec<bool>,
    /// The positions on the stack of the frames that keep blocks, the
    /// root's first: the last is where a block that ends goes.
    keepers: Vec<usize>,
    /// The positions of the open lists, rows and tables of data: where an
    /// item, a cell and a row go.
    open_lists: Vec<usize>,
    open_rows: Vec<usize>,
    open_tables: Vec<usize>,
    /// How many code spans and cells are open.
    open_code: usize,
    open_cells: usize,
    /// How many quotes and lists are open, and how many of those past
    /// [`MAX_NESTING`].
    nesting: usize,
    capped: usize,
}

/// Characters that would be read as markdown where they are text: the
/// marks, and an `&` that would read as an entity.
fn escape(text: &str, out: &mut String) {
    for (at, c) in text.char_indices() {
        let marked = matches!(
            c,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|' | '~'
        );
        if marked || (c == '&' && names_an_entity(&text[at + c.len_utf8()..])) {
            out.push('\\');
        }
        out.push(c);
    }
}

/// Whether what follows an `&` reads as the rest of a character
/// reference: a name or a number, then `;`.
fn names_an_entity(rest: &str) -> bool {
    let name = rest
        .strip_prefix("#x")
        .or_else(|| rest.strip_prefix("#X"))
        .or_else(|| rest.strip_prefix('#'))
        .unwrap_or(rest);
    let length = name
        .bytes()
        .take(33)
        .take_while(u8::is_ascii_alphanumeric)
        .count();
    (1..=32).contains(&length) && name.as_bytes().get(length) == Some(&b';')
}

/// An address as the destination of a markdown link or image: a
/// parenthesis or a backslash in it is escaped, so the destination ends
/// where the address does.
fn destination(address: &str) -> String {
    let mut out = String::with_capacity(address.len());
    for c in address.chars() {
        if matches!(c, '(' | ')' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
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
    let mut out = String::with_capacity(block.len() + first.len());
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

    /// The position of the innermost frame that holds blocks. Inline
    /// frames stand above it only while no block is in them, so this looks
    /// at a few frames at most.
    fn holder_at(&self) -> Option<usize> {
        self.stack.iter().rposition(|frame| !frame.is_inline())
    }

    /// The position of the frame that keeps what the frame at `at` writes:
    /// itself, or the innermost keeper under it.
    fn keeper_of(&self, at: usize) -> usize {
        let below = self.keepers.partition_point(|keeper| *keeper <= at);
        below
            .checked_sub(1)
            .and_then(|index| self.keepers.get(index))
            .copied()
            .unwrap_or(0)
    }

    /// Ends the paragraph the frame at `at` is writing, into the frame
    /// that keeps its blocks.
    fn flush_at(&mut self, at: usize) {
        let Some(paragraph) = self.stack.get_mut(at).and_then(Frame::take_paragraph) else {
            return;
        };
        let keeper = self.keeper_of(at);
        if let Some(frame) = self.stack.get_mut(keeper) {
            frame.blocks.push(paragraph);
        }
    }

    /// Puts a frame on the stack, and on the lists of what is open.
    fn push_frame(&mut self, frame: Frame) {
        let at = self.stack.len();
        if frame.kind.keeps_blocks() {
            self.keepers.push(at);
        }
        match frame.kind {
            Kind::List { .. } => self.open_lists.push(at),
            Kind::Row => self.open_rows.push(at),
            Kind::Table => self.open_tables.push(at),
            Kind::Code => self.open_code += 1,
            Kind::Cell => self.open_cells += 1,
            _ => {}
        }
        if matches!(frame.kind, Kind::Quote | Kind::List { .. }) {
            self.nesting += 1;
        }
        if frame.capped {
            self.capped += 1;
        }
        self.stack.push(frame);
    }

    /// Takes the top frame off the stack, and off the lists of what is
    /// open.
    fn pop_frame(&mut self) -> Option<Frame> {
        let frame = self.stack.pop()?;
        let at = self.stack.len();
        if self.keepers.last() == Some(&at) {
            self.keepers.pop();
        }
        match frame.kind {
            Kind::List { .. } => {
                self.open_lists.pop();
            }
            Kind::Row => {
                self.open_rows.pop();
            }
            Kind::Table => {
                self.open_tables.pop();
            }
            Kind::Code => self.open_code = self.open_code.saturating_sub(1),
            Kind::Cell => self.open_cells = self.open_cells.saturating_sub(1),
            _ => {}
        }
        if matches!(frame.kind, Kind::Quote | Kind::List { .. }) {
            self.nesting = self.nesting.saturating_sub(1);
        }
        if frame.capped {
            self.capped = self.capped.saturating_sub(1);
        }
        Some(frame)
    }

    /// Where an address in the page leads, when that is a web address.
    fn address(&self, written: &str) -> Option<String> {
        let url = (self.resolve)(written.trim())?;
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
                if let Some(holder) = self.holder_at() {
                    self.flush_at(holder);
                    if mode == Mode::Markdown {
                        let keeper = self.keeper_of(holder);
                        if let Some(frame) = self.stack.get_mut(keeper) {
                            frame.blocks.push("---".to_owned());
                        }
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
        // Past the nesting a writer keeps, a quote or a list is the blocks
        // it holds, and so is an item of a list that is.
        let capped = matches!(kind, Kind::Quote | Kind::List { .. }) && self.nesting >= MAX_NESTING;
        let kind = if capped || (kind == Kind::Item && self.capped > 0) {
            Kind::Container
        } else {
            kind
        };
        let mut frame = Frame::new(kind, mode);
        frame.capped = capped;
        if !frame.is_inline()
            && let Some(at) = self.stack.len().checked_sub(1)
            && self.stack.get(at).is_some_and(|top| !top.is_inline())
        {
            // A block begins: the paragraph being written in what holds it
            // ends.
            self.flush_at(at);
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
        self.push_frame(frame);
        true
    }

    /// A `<br>`: a line break, and after a second one a new paragraph.
    fn line_break(&mut self) {
        let mode = self.mode;
        let Some(at) = self.stack.len().checked_sub(1) else {
            return;
        };
        let Some(top) = self.stack.get_mut(at) else {
            return;
        };
        if top.is_inline() || matches!(top.kind, Kind::Cell | Kind::Heading(_)) {
            top.space_owed = !top.inline.is_empty();
            return;
        }
        if top.broke {
            // The second break of a chain: what was written is a paragraph.
            self.flush_at(at);
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
        written.push_str(&destination(&src));
        written.push(')');
        if let Some(top) = self.top() {
            top.push(&written, false, false);
        }
    }

    fn text(&mut self, text: &str) {
        let (mode, in_pre) = (self.mode, self.in_pre);
        let in_code = self.open_code > 0;
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

    /// Hands a finished block to the frame that keeps the blocks of what
    /// is open.
    fn hand_on(&mut self, block: String) {
        if block.is_empty() {
            return;
        }
        let keeper = self.keepers.last().copied().unwrap_or(0);
        if let Some(frame) = self.stack.get_mut(keeper) {
            frame.blocks.push(block);
        }
    }

    /// Code as it stands in a markdown table's cell: a `|` in it would
    /// end the cell, in a code span too (GFM), so it is escaped there.
    fn in_cell(&self, code: &str) -> String {
        if self.open_cells > 0 {
            code.replace('|', "\\|")
        } else {
            code.to_owned()
        }
    }

    fn close(&mut self, element: &scraper::node::Element) {
        if !self.framed.pop().unwrap_or(false) {
            return;
        }
        if element.name() == "table" {
            self.tables.pop();
        }
        let Some(mut frame) = self.pop_frame() else {
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
                    "{fence}{}\n{}\n{fence}",
                    frame.language.as_deref().unwrap_or_default(),
                    self.in_cell(code)
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
                    format!("{fence}{pad}{}{pad}{fence}", self.in_cell(inner))
                }
                (Kind::Link(Some(address)), true) => {
                    format!("[{inner}]({})", destination(address))
                }
                _ => inner.to_owned(),
            };
            if let Some(top) = self.top() {
                top.push(&written, frame.led_by_space, frame.space_owed);
            }
            return;
        }
        // What the frame was writing ends: in itself when it keeps its
        // blocks, else in the frame that keeps them.
        if let Some(paragraph) = frame.take_paragraph() {
            if frame.kind.keeps_blocks() {
                frame.blocks.push(paragraph);
            } else {
                self.hand_on(paragraph);
            }
        }
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
                // A container wrote its blocks where they are kept as it
                // went.
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
                    .open_lists
                    .last()
                    .copied()
                    .and_then(|at| self.stack.get_mut(at))
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
                    .open_rows
                    .last()
                    .copied()
                    .and_then(|at| self.stack.get_mut(at))
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
                    .open_tables
                    .last()
                    .copied()
                    .and_then(|at| self.stack.get_mut(at))
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
#[cfg(test)]
pub(crate) fn render(content: &Content<'_>, base: &Url, mode: Mode) -> Rendered {
    render_with(content, &|href| base.join(href).ok(), mode)
}

/// Writes the content of a page in one mode. Addresses in it are resolved
/// by `resolve`: as the document resolves them, against its base and with
/// a query in its encoding (F161).
pub(crate) fn render_with(
    content: &Content<'_>,
    resolve: &dyn Fn(&str) -> Option<Url>,
    mode: Mode,
) -> Rendered {
    let mut writer = Writer {
        mode,
        resolve,
        dropped: &content.dropped,
        stack: vec![Frame::new(Kind::Container, mode)],
        framed: Vec::new(),
        headings: Vec::new(),
        in_pre: false,
        tables: Vec::new(),
        // The root keeps what is written.
        keepers: vec![0],
        open_lists: Vec::new(),
        open_rows: Vec::new(),
        open_tables: Vec::new(),
        open_code: 0,
        open_cells: 0,
        nesting: 0,
        capped: 0,
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
        writer.flush_at(0);
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
