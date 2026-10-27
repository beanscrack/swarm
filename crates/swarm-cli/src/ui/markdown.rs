//! Markdown to terminal lines, for assistant replies.
//!
//! Finished lines are committed one at a time into the terminal's scrollback,
//! so every line returned by [`render`] is self-contained: styling opened on a
//! line is reset before the line ends and re-opened on the next one when the
//! style continues (wrapped bold text, a multi-line quote, and so on). Lines
//! carry no gutter; the caller indents them.
//!
//! The renderer walks `pulldown-cmark` events. Block containers (the document,
//! block quotes, lists and list items) are frames that collect finished lines
//! at their own content width; closing a frame prefixes those lines with the
//! container's marker. Inline text is accumulated into a buffer with a style
//! stack, then wrapped with [`wrap_hang`], which carries styling across rows.

use pulldown_cmark::{Alignment, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::style::{self, Color, BRAND, SUBTLE, SUGGESTION, TEXT, TOOL};
use crate::ui::text::{pieces, sanitize, strip_ansi, truncate, width, wrap_hang, Piece};

const RESET: &str = "\x1b[0m";
/// Code block text: light grey (`#C8C8C8`).
const CODE: &str = "#C8C8C8";
/// Narrowest width the renderer lays out for; anything smaller is raised to it.
const MIN_WIDTH: usize = 8;
/// Cells a container always leaves for its content. Markers and indentation
/// give way before the text is squeezed below this.
const MIN_CONTENT: usize = 4;
/// Indent of code block lines.
const CODE_INDENT: usize = 2;
/// Longest horizontal rule.
const RULE_MAX: usize = 40;
/// Cells taken by the ` │ ` between table columns.
const COLUMN_GAP: usize = 3;

/// Render Markdown `text` into styled terminal lines no wider than `width`
/// cells (measured with [`width`]; a `width` below 8 is treated as 8).
///
/// The text is sanitised first, so nothing in it can inject escape sequences.
/// With [`Color::Plain`] the result contains no escape sequences at all.
/// With [`Color::Ansi`] every line is self-contained: any styling it opens is
/// reset before its end and re-opened on the following line when it continues.
/// Lines have no indent or prefix and there is no trailing blank line; empty
/// input yields an empty vector.
// Nothing outside the tests calls this yet; the allow also keeps the helpers
// below it from being reported. Remove it once the chat UI renders replies here.
#[allow(dead_code)]
pub fn render(text: &str, width: usize, colors: Color) -> Vec<String> {
    let limit = width.max(MIN_WIDTH);
    let clean = sanitize(text);
    if clean.trim().is_empty() {
        return Vec::new();
    }
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut renderer = Renderer::new(limit, colors);
    for event in Parser::new_ext(&clean, options) {
        renderer.event(event);
    }
    renderer.finish()
}

// ---------------------------------------------------------------------------
// Styling
// ---------------------------------------------------------------------------

/// One SGR attribute that can sit on the inline style stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    Bold,
    Dim,
    Italic,
    Underline,
    Strike,
    /// Truecolor foreground from a `#RRGGBB` palette constant.
    Fg(&'static str),
}

impl Look {
    fn escape(self) -> String {
        match self {
            Look::Bold => "\x1b[1m".to_string(),
            Look::Dim => "\x1b[2m".to_string(),
            Look::Italic => "\x1b[3m".to_string(),
            Look::Underline => "\x1b[4m".to_string(),
            Look::Strike => "\x1b[9m".to_string(),
            Look::Fg(hex) => {
                let (r, g, b) = style::rgb(hex);
                format!("\x1b[38;2;{r};{g};{b}m")
            }
        }
    }
}

/// `text` wrapped in `looks` and a reset; unchanged when colours are off.
fn paint(colors: Color, looks: &[Look], text: &str) -> String {
    if colors == Color::Plain || text.is_empty() {
        return text.to_string();
    }
    let mut out = String::new();
    for look in looks {
        out.push_str(&look.escape());
    }
    out.push_str(text);
    out.push_str(RESET);
    out
}

/// A marker such as `• ` or `10. `: the glyphs are painted, trailing spaces are
/// not (so a blank line can drop them cleanly).
fn style_marker(colors: Color, marker: &str, hex: &'static str) -> String {
    let core = marker.trim_end();
    let pad = &marker[core.len()..];
    format!("{}{pad}", paint(colors, &[Look::Fg(hex)], core))
}

/// The longest prefix of `text` that fits in `max` cells.
fn cut_cells(text: &str, max: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for ch in text.chars() {
        let cells = ch.width().unwrap_or(0);
        if used + cells > max {
            break;
        }
        used += cells;
        out.push(ch);
    }
    out
}

/// The text of an inline html event or decoded entity may still hold control
/// characters (`&#27;` decodes to ESC), so everything that reaches the output
/// goes through the sanitiser again.
fn clean(text: &str) -> String {
    sanitize(text)
}

// ---------------------------------------------------------------------------
// Inline buffer
// ---------------------------------------------------------------------------

/// Styled text under construction. Style changes are written lazily, just
/// before the next text, so empty spans leave no escapes behind and a row
/// never ends in a dangling `ESC[1m`.
struct Inline {
    paint: bool,
    buf: String,
    /// Styles that apply to text pushed from now on.
    stack: Vec<Look>,
    /// Styles already written into `buf` since its last reset.
    emitted: Vec<Look>,
}

impl Inline {
    fn new(colors: Color) -> Self {
        Self { paint: colors == Color::Ansi, buf: String::new(), stack: Vec::new(), emitted: Vec::new() }
    }

    fn open(&mut self, look: Look) {
        self.stack.push(look);
    }

    fn close(&mut self) {
        self.stack.pop();
    }

    /// Bring the escapes written so far in line with the style stack.
    fn sync(&mut self) {
        if !self.paint || self.emitted == self.stack {
            return;
        }
        if self.stack.starts_with(&self.emitted) {
            for look in &self.stack[self.emitted.len()..] {
                self.buf.push_str(&look.escape());
            }
        } else {
            if !self.emitted.is_empty() {
                self.buf.push_str(RESET);
            }
            for look in &self.stack {
                self.buf.push_str(&look.escape());
            }
        }
        self.emitted.clone_from(&self.stack);
    }

    /// Append text; a stray newline is a space (breaks are explicit).
    fn text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.sync();
        if text.contains('\n') {
            self.buf.push_str(&text.replace('\n', " "));
        } else {
            self.buf.push_str(text);
        }
    }

    /// A line break. Styling is closed first so each side is self-contained;
    /// the next text re-opens whatever is still on the stack.
    fn hard_break(&mut self) {
        if !self.emitted.is_empty() {
            self.buf.push_str(RESET);
            self.emitted.clear();
        }
        self.buf.push('\n');
    }

    /// The finished text, closed, leaving the style stack untouched.
    fn take(&mut self) -> String {
        let mut out = std::mem::take(&mut self.buf);
        if !self.emitted.is_empty() {
            out.push_str(RESET);
            self.emitted.clear();
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Block structure
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Root,
    Quote,
    /// A list; the number the next ordered item gets.
    List(Option<u64>),
    Item,
}

/// A container collecting lines at its content width.
struct Frame {
    kind: Kind,
    /// Width of the area this frame sits in.
    outer: usize,
    /// Cells left for content once the marker is placed.
    width: usize,
    /// The marker as wanted, and as cut to leave room for content.
    marker: String,
    shown: String,
    lines: Vec<String>,
    /// Inline style stack depth when the frame opened.
    mark: usize,
}

impl Frame {
    fn new(kind: Kind, outer: usize, marker: &str, mark: usize) -> Self {
        let mut frame = Frame {
            kind,
            outer,
            width: outer,
            marker: marker.to_string(),
            shown: String::new(),
            lines: Vec::new(),
            mark,
        };
        frame.refit();
        frame
    }

    /// Cut the marker so at least `MIN_CONTENT` cells remain for content; a
    /// deeply nested list in a narrow terminal loses its bullets, not its text.
    fn refit(&mut self) {
        self.shown = cut_cells(&self.marker, self.outer.saturating_sub(MIN_CONTENT));
        self.width = self.outer.saturating_sub(width(&self.shown));
    }

    /// Blocks in a document or quote are separated by a blank line; list
    /// items and their contents are packed.
    fn separates(&self) -> bool {
        matches!(self.kind, Kind::Root | Kind::Quote)
    }
}

/// Prefix `lines`: `first` on the first, `rest` on the others. A blank line
/// keeps only the visible part of its prefix.
fn compose(lines: Vec<String>, first: &str, rest: &str) -> Vec<String> {
    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let prefix = if index == 0 { first } else { rest };
            if line.is_empty() {
                prefix.trim_end().to_string()
            } else {
                format!("{prefix}{line}")
            }
        })
        .collect()
}

/// A link or image being read; its text is kept to decide whether the url
/// adds anything.
struct LinkState {
    url: String,
    text: String,
    /// Style stack depth before the link styled itself.
    mark: usize,
    show: bool,
}

struct Table {
    aligns: Vec<Alignment>,
    /// Header row first.
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    /// Style stack depth before the header made itself bold.
    mark: usize,
}

struct Renderer {
    colors: Color,
    /// Widest a line may be.
    limit: usize,
    root: Frame,
    stack: Vec<Frame>,
    inline: Inline,
    /// Text of the code block being read.
    code: Option<String>,
    table: Option<Table>,
    links: Vec<LinkState>,
    /// Style stack depth before the current heading styled itself.
    heading_mark: usize,
}

impl Renderer {
    fn new(limit: usize, colors: Color) -> Self {
        Self {
            colors,
            limit,
            root: Frame::new(Kind::Root, limit, "", 0),
            stack: Vec::new(),
            inline: Inline::new(colors),
            code: None,
            table: None,
            links: Vec::new(),
            heading_mark: 0,
        }
    }

    fn top(&self) -> &Frame {
        self.stack.last().unwrap_or(&self.root)
    }

    fn top_mut(&mut self) -> &mut Frame {
        self.stack.last_mut().unwrap_or(&mut self.root)
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => {
                self.inline.open(Look::Fg(SUGGESTION));
                self.add_text(&code);
                self.inline.close();
            }
            Event::Html(html) => self.html(&html),
            Event::InlineHtml(html) => self.add_text(&html),
            Event::SoftBreak => self.add_text(" "),
            Event::HardBreak => {
                for link in &mut self.links {
                    link.text.push(' ');
                }
                self.inline.hard_break();
            }
            Event::Rule => {
                self.flush();
                let cells = self.top().width.min(RULE_MAX);
                let line = match self.colors {
                    Color::Plain => "-".repeat(cells),
                    Color::Ansi => paint(self.colors, &[Look::Fg(SUBTLE)], &"\u{2500}".repeat(cells)),
                };
                self.push_block(vec![line]);
            }
            Event::TaskListMarker(done) => self.task_marker(done),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph | Tag::HtmlBlock => self.flush(),
            Tag::Heading { level, .. } => {
                self.flush();
                self.heading_mark = self.inline.stack.len();
                for look in heading_looks(level) {
                    self.inline.open(*look);
                }
            }
            Tag::BlockQuote(_) => {
                self.flush();
                let mark = self.inline.stack.len();
                self.inline.open(Look::Italic);
                self.inline.open(Look::Dim);
                let marker = if self.colors == Color::Ansi { "\u{258e} " } else { "> " };
                let outer = self.top().width;
                self.stack.push(Frame::new(Kind::Quote, outer, marker, mark));
            }
            Tag::CodeBlock(_) => {
                self.flush();
                self.code = Some(String::new());
            }
            Tag::List(start) => {
                self.flush();
                let outer = self.top().width;
                let mark = self.inline.stack.len();
                self.stack.push(Frame::new(Kind::List(start), outer, "", mark));
            }
            Tag::Item => self.open_item(),
            Tag::Table(aligns) => {
                self.flush();
                let mark = self.inline.stack.len();
                self.table = Some(Table { aligns, rows: Vec::new(), row: Vec::new(), mark });
            }
            Tag::TableHead => self.inline.open(Look::Bold),
            Tag::Emphasis => self.inline.open(Look::Italic),
            Tag::Strong => self.inline.open(Look::Bold),
            Tag::Strikethrough => self.inline.open(Look::Strike),
            Tag::Link { link_type, dest_url, .. } => {
                let show = !matches!(link_type, LinkType::Autolink | LinkType::Email);
                self.open_link(&dest_url, show, true);
            }
            Tag::Image { dest_url, .. } => self.open_link(&dest_url, true, false),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.inline.stack.truncate(self.heading_mark);
            }
            TagEnd::BlockQuote(_) | TagEnd::List(_) | TagEnd::Item => {
                self.flush();
                self.close_frame();
            }
            TagEnd::CodeBlock => {
                if let Some(code) = self.code.take() {
                    let lines = code_lines(&code, self.top().width, self.colors);
                    self.push_block(lines);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.inline.stack.truncate(table.mark);
                    let lines = table.render(self.top().width, self.colors);
                    self.push_block(lines);
                }
            }
            TagEnd::TableHead => {
                self.end_row();
                if let Some(table) = &self.table {
                    self.inline.stack.truncate(table.mark);
                }
            }
            TagEnd::TableRow => self.end_row(),
            TagEnd::TableCell => {
                let cell = self.inline.take().replace('\n', " ");
                if let Some(table) = &mut self.table {
                    table.row.push(cell);
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.inline.close(),
            TagEnd::Link | TagEnd::Image => self.close_link(),
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        match &mut self.code {
            Some(code) => code.push_str(&clean(text)),
            None => self.add_text(text),
        }
    }

    /// Inline text, also remembered as the label of any open link.
    fn add_text(&mut self, text: &str) {
        let text = clean(text);
        for link in &mut self.links {
            link.text.push_str(&text);
        }
        self.inline.text(&text);
    }

    /// Html is shown literally, line by line.
    fn html(&mut self, html: &str) {
        for (index, line) in clean(html).split('\n').enumerate() {
            if index > 0 {
                self.inline.hard_break();
            }
            self.inline.text(line);
        }
    }

    fn open_link(&mut self, url: &str, show: bool, underline: bool) {
        let mark = self.inline.stack.len();
        if underline {
            self.inline.open(Look::Underline);
            self.inline.open(Look::Fg(SUGGESTION));
        }
        self.links.push(LinkState { url: clean(url), text: String::new(), mark, show });
    }

    /// Follow the label with ` (url)` in grey when the url says something the
    /// label does not.
    fn close_link(&mut self) {
        let Some(link) = self.links.pop() else { return };
        self.inline.stack.truncate(link.mark);
        let label = link.text.trim();
        let url = link.url.as_str();
        let bare = url.strip_prefix("mailto:").unwrap_or(url);
        if !link.show || url.is_empty() || label == url || label == bare {
            return;
        }
        let shown = if label.is_empty() {
            url.to_string()
        } else {
            // The space stays outside the grey, so a row that breaks here does
            // not end in an empty coloured span.
            self.add_text(" ");
            format!("({url})")
        };
        self.inline.open(Look::Fg(SUBTLE));
        self.add_text(&shown);
        self.inline.close();
    }

    fn open_item(&mut self) {
        self.flush();
        let number = match &mut self.top_mut().kind {
            Kind::List(Some(next)) => {
                let current = *next;
                *next = next.saturating_add(1);
                Some(current)
            }
            _ => None,
        };
        let marker = match number {
            Some(number) => format!("{number}. "),
            None if self.colors == Color::Ansi => "\u{2022} ".to_string(),
            None => "- ".to_string(),
        };
        let outer = self.top().width;
        let mark = self.inline.stack.len();
        self.stack.push(Frame::new(Kind::Item, outer, &marker, mark));
    }

    /// `[ ]` / `[x]` joins the item's marker so wrapped text hangs after it.
    fn task_marker(&mut self, done: bool) {
        let boxed = match (self.colors, done) {
            (Color::Plain, false) => "[ ] ",
            (Color::Plain, true) => "[x] ",
            (Color::Ansi, false) => "\u{2610} ",
            (Color::Ansi, true) => "\u{2611} ",
        };
        let frame = self.top_mut();
        if frame.kind == Kind::Item && frame.lines.is_empty() {
            frame.marker.push_str(boxed);
            frame.refit();
        }
    }

    fn end_row(&mut self) {
        if let Some(table) = &mut self.table {
            let row = std::mem::take(&mut table.row);
            if !row.is_empty() {
                table.rows.push(row);
            }
        }
    }

    /// Finish the innermost container: prefix its lines with its marker and
    /// hand them to the container around it.
    fn close_frame(&mut self) {
        let Some(frame) = self.stack.pop() else { return };
        self.inline.stack.truncate(frame.mark);
        let lines = match frame.kind {
            Kind::Root | Kind::List(_) => frame.lines,
            Kind::Quote => {
                let bar = style_marker(self.colors, &frame.shown, SUBTLE);
                compose(frame.lines, &bar, &bar)
            }
            Kind::Item => {
                let first = style_marker(self.colors, &frame.shown, BRAND);
                let rest = " ".repeat(width(&frame.shown));
                let lines = if frame.lines.is_empty() { vec![String::new()] } else { frame.lines };
                compose(lines, &first, &rest)
            }
        };
        self.push_block(lines);
    }

    /// Add a finished block to the innermost container.
    fn push_block(&mut self, lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        let frame = self.top_mut();
        if frame.separates() && !frame.lines.is_empty() {
            frame.lines.push(String::new());
        }
        frame.lines.extend(lines);
    }

    /// Wrap the pending inline text into the innermost container.
    fn flush(&mut self) {
        let text = self.inline.take();
        if text.is_empty() || strip_ansi(&text).trim().is_empty() {
            return;
        }
        let cols = self.top().width;
        let mut rows: Vec<String> = Vec::new();
        for segment in text.split('\n') {
            for (index, row) in wrap_hang(segment, cols, "", "").into_iter().enumerate() {
                if index == 0 {
                    rows.push(row);
                    continue;
                }
                // A word that exactly fills a row leaves the space after it at
                // the head of the next; a wrapped row never starts with one.
                let row = trim_leading_spaces(&row);
                if !strip_ansi(&row).is_empty() {
                    rows.push(row);
                }
            }
        }
        while rows.last().is_some_and(|row| strip_ansi(row).trim().is_empty()) {
            rows.pop();
        }
        self.push_block(rows);
    }

    fn finish(mut self) -> Vec<String> {
        self.flush();
        while !self.stack.is_empty() {
            self.close_frame();
        }
        let mut lines = std::mem::take(&mut self.root.lines);
        while lines.last().is_some_and(|line| strip_ansi(line).trim().is_empty()) {
            lines.pop();
        }
        // Last resort for what cannot be laid out (a single cluster wider than
        // the row, too many table columns for the width): cut, never overflow.
        lines
            .into_iter()
            .map(|line| {
                let line = if width(&line) > self.limit { truncate(&line, self.limit) } else { line };
                tidy(&line)
            })
            .collect()
    }
}

fn heading_looks(level: HeadingLevel) -> &'static [Look] {
    match level {
        HeadingLevel::H1 => &[Look::Bold, Look::Underline, Look::Fg(TEXT)],
        HeadingLevel::H2 => &[Look::Bold],
        _ => &[Look::Bold, Look::Fg(TOOL)],
    }
}

/// Drop styles that are opened and closed with no text between them (a wrap at
/// a break inside a re-opened style leaves `ESC[3m ESC[0m` at the end of a row)
/// and resets with nothing left to reset.
fn tidy(line: &str) -> String {
    if !line.contains('\x1b') {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    // Styles seen since the last text; written only once text follows them.
    let mut pending: Vec<&str> = Vec::new();
    let mut open = false;
    for piece in pieces(line) {
        match piece {
            Piece::Text(run) => {
                for seq in pending.drain(..) {
                    out.push_str(seq);
                    open = true;
                }
                out.push_str(run);
            }
            Piece::Escape(RESET) => {
                pending.clear();
                if open {
                    out.push_str(RESET);
                    open = false;
                }
            }
            Piece::Escape(seq) => pending.push(seq),
        }
    }
    if open {
        out.push_str(RESET);
    }
    out
}

/// Drop the spaces at the head of a row, keeping any escapes in front of them.
fn trim_leading_spaces(row: &str) -> String {
    let mut out = String::with_capacity(row.len());
    let mut leading = true;
    for piece in pieces(row) {
        match piece {
            Piece::Escape(seq) => out.push_str(seq),
            Piece::Text(run) if leading => {
                let rest = run.trim_start_matches(' ');
                if !rest.is_empty() {
                    leading = false;
                    out.push_str(rest);
                }
            }
            Piece::Text(run) => out.push_str(run),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Code blocks
// ---------------------------------------------------------------------------

/// Code lines, indented, in light grey, never re-wrapped at words: a line wider
/// than the row is cut into rows with the same indent.
fn code_lines(code: &str, cols: usize, colors: Color) -> Vec<String> {
    let inner = cols.saturating_sub(CODE_INDENT).max(1);
    let indent = " ".repeat(CODE_INDENT);
    let mut source: Vec<&str> = code.split('\n').collect();
    while source.last().is_some_and(|line| line.trim().is_empty()) {
        source.pop();
    }
    let mut out = Vec::new();
    for line in source {
        if line.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        for chunk in split_cells(line, inner) {
            out.push(format!("{indent}{}", paint(colors, &[Look::Fg(CODE)], chunk)));
        }
    }
    out
}

/// Cut `line` into pieces of at most `max` cells, on grapheme boundaries.
fn split_cells(line: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (index, grapheme) in line.grapheme_indices(true) {
        let cells = grapheme.width();
        if used + cells > max && used > 0 {
            out.push(&line[start..index]);
            start = index;
            used = 0;
        }
        used += cells;
    }
    if start < line.len() {
        out.push(&line[start..]);
    }
    out
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

impl Table {
    /// Aligned columns, a rule under the header, widest columns shrunk (with
    /// `…`) when the table is wider than `cols`.
    fn render(&self, cols: usize, colors: Color) -> Vec<String> {
        let count = self.rows.iter().map(Vec::len).max().unwrap_or(0);
        if count == 0 {
            return Vec::new();
        }
        let natural: Vec<usize> = (0..count)
            .map(|column| {
                self.rows
                    .iter()
                    .filter_map(|row| row.get(column))
                    .map(|cell| width(cell))
                    .max()
                    .unwrap_or(0)
                    .max(1)
            })
            .collect();
        let widths = fit_columns(&natural, cols.saturating_sub(COLUMN_GAP * (count - 1)));
        let subtle = [Look::Fg(SUBTLE)];
        let gap = format!(" {} ", paint(colors, &subtle, "\u{2502}"));

        let line = |row: &[String]| -> String {
            let cells: Vec<String> = (0..count)
                .map(|column| {
                    let cell = row.get(column).map(String::as_str).unwrap_or("");
                    let align = self.aligns.get(column).copied().unwrap_or(Alignment::None);
                    align_cell(cell, widths[column], align, column + 1 == count)
                })
                .collect();
            cells.join(&gap).trim_end().to_string()
        };

        let mut out = Vec::new();
        for (index, row) in self.rows.iter().enumerate() {
            out.push(line(row));
            if index == 0 {
                let rule: Vec<String> = widths.iter().map(|cells| "\u{2500}".repeat(*cells)).collect();
                out.push(paint(colors, &subtle, &rule.join("\u{2500}\u{253c}\u{2500}")));
            }
        }
        out
    }
}

/// Column widths that fit `avail` cells: the natural widths when they fit,
/// otherwise the widest columns cut down to a common cap.
fn fit_columns(natural: &[usize], avail: usize) -> Vec<usize> {
    if natural.iter().sum::<usize>() <= avail {
        return natural.to_vec();
    }
    if natural.len() >= avail {
        return vec![1; natural.len()];
    }
    let total = |cap: usize| natural.iter().map(|&cells| cells.min(cap)).sum::<usize>();
    // Largest cap whose clamped total fits; a cap of 1 always does here.
    let (mut low, mut high) = (1, natural.iter().copied().max().unwrap_or(1));
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if total(mid) <= avail {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let mut widths: Vec<usize> = natural.iter().map(|&cells| cells.min(low)).collect();
    // Hand out what the cap left over, one cell each, to the cut columns.
    let mut spare = avail.saturating_sub(widths.iter().sum::<usize>());
    for (cells, &wanted) in widths.iter_mut().zip(natural) {
        if spare == 0 {
            break;
        }
        if wanted > low {
            *cells += 1;
            spare -= 1;
        }
    }
    widths
}

/// Cut `cell` to `cols` cells if needed and pad it per `align`. The last
/// column gets no trailing padding.
fn align_cell(cell: &str, cols: usize, align: Alignment, last: bool) -> String {
    let cell = if width(cell) > cols { truncate(cell, cols) } else { cell.to_string() };
    let slack = cols.saturating_sub(width(&cell));
    let (left, right) = match align {
        Alignment::Right => (slack, 0),
        Alignment::Center => (slack / 2, slack - slack / 2),
        Alignment::Left | Alignment::None => (0, slack),
    };
    let right = if last { 0 } else { right };
    format!("{}{cell}{}", " ".repeat(left), " ".repeat(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str, cols: usize) -> Vec<String> {
        render(text, cols, Color::Plain)
    }

    fn ansi(text: &str, cols: usize) -> Vec<String> {
        render(text, cols, Color::Ansi)
    }

    /// True when the line ends with no SGR style open.
    fn closed(line: &str) -> bool {
        let mut open = false;
        for piece in pieces(line) {
            if let Piece::Escape(seq) = piece {
                open = seq != RESET;
            }
        }
        !open
    }

    /// A style opened and reset with no text in between.
    fn has_empty_span(line: &str) -> bool {
        let found = pieces(line);
        found.windows(2).any(|pair| {
            matches!(pair, [Piece::Escape(open), Piece::Escape(close)] if *open != RESET && *close == RESET)
        })
    }

    /// Every escape in `line` is an SGR this module writes.
    fn only_own_escapes(line: &str) -> bool {
        pieces(line).into_iter().all(|piece| match piece {
            Piece::Text(_) => true,
            Piece::Escape(seq) => {
                let Some(params) = seq.strip_prefix("\x1b[").and_then(|rest| rest.strip_suffix('m')) else {
                    return false;
                };
                matches!(params, "0" | "1" | "2" | "3" | "4" | "9")
                    || params.strip_prefix("38;2;").is_some_and(|rgb| {
                        let channels: Vec<&str> = rgb.split(';').collect();
                        channels.len() == 3 && channels.iter().all(|c| c.parse::<u8>().is_ok())
                    })
            }
        })
    }

    const SAMPLE: &str = "\
# Title with \u{4f60}\u{597d}\u{4e16}\u{754c}

Some **bold text that wraps around** and *italic* and ~~strike~~ plus `inline code` and a [link](https://example.com/path) and <https://bare.example.org>.

## Lists

- first item with enough words to wrap around the narrow widths we test
  - nested \u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d}\u{4e16}\u{754c} item
    1. deep ordered item that also wraps nicely
- [x] done
- [ ] todo

> quoted **bold** text that is also quite long so that it wraps

```rust
let a_very_long_line_of_code = call(argument_one, argument_two, argument_three);
```

| Name | \u{8bf4}\u{660e} | Qty |
|:-----|:----:|----:|
| apple | \u{82f9}\u{679c}\u{5f88}\u{751c} | 3 |
| kiwi | tart | 12 |

---

supercalifragilisticexpialidocious_unbreakable_word_that_is_longer_than_any_width_tested_here
";

    // ---- plain mode: exact strings --------------------------------------

    #[test]
    fn empty_input_renders_nothing() {
        assert!(plain("", 40).is_empty());
        assert!(plain("  \n\n \t\n", 40).is_empty());
        assert!(ansi("", 40).is_empty());
    }

    #[test]
    fn paragraphs_wrap_and_are_separated_by_one_blank_line() {
        assert_eq!(
            plain("one two three four five six\n\nsecond paragraph", 11),
            vec!["one two", "three four", "five six", "", "second", "paragraph"]
        );
        assert_eq!(plain("short", 40), vec!["short"]);
    }

    #[test]
    fn no_trailing_blank_line() {
        assert_eq!(plain("text\n\n\n", 40), vec!["text"]);
        assert_eq!(plain("- a\n\n\n", 40), vec!["- a"]);
    }

    #[test]
    fn soft_break_is_a_space_and_hard_break_a_new_line() {
        assert_eq!(plain("a\nb", 40), vec!["a b"]);
        assert_eq!(plain("a  \nb", 40), vec!["a", "b"]);
        assert_eq!(plain("a\\\nb", 40), vec!["a", "b"]);
    }

    #[test]
    fn headings_drop_their_marks() {
        assert_eq!(
            plain("# Title\n\n## Sub\n\n### Third\n\n###### Sixth\n\ntext", 40),
            vec!["Title", "", "Sub", "", "Third", "", "Sixth", "", "text"]
        );
    }

    #[test]
    fn inline_marks_vanish_in_plain_mode() {
        assert_eq!(plain("**bold** *it* ~~gone~~ `code` _u_", 40), vec!["bold it gone code u"]);
    }

    #[test]
    fn links_show_their_url_unless_it_is_the_text() {
        assert_eq!(plain("[docs](https://x.test/a) here", 80), vec!["docs (https://x.test/a) here"]);
        assert_eq!(plain("<https://y.test> alone", 80), vec!["https://y.test alone"]);
        assert_eq!(plain("[https://z.test](https://z.test)", 80), vec!["https://z.test"]);
        assert_eq!(plain("<me@x.test>", 80), vec!["me@x.test"]);
        assert_eq!(plain("[me@x.test](mailto:me@x.test)", 80), vec!["me@x.test"]);
        assert_eq!(plain("[mail](mailto:me@x.test)", 80), vec!["mail (mailto:me@x.test)"]);
        assert_eq!(plain("[](https://e.test)", 80), vec!["https://e.test"]);
        assert_eq!(plain("![logo](https://i.test/a.png)", 80), vec!["logo (https://i.test/a.png)"]);
    }

    #[test]
    fn code_block_is_indented_and_preserved() {
        assert_eq!(
            plain("```rust\nfn main() {\n    println!(\"hi\");\n}\n```", 40),
            vec!["  fn main() {", "      println!(\"hi\");", "  }"]
        );
        assert_eq!(plain("```\na\n\nb\n```", 40), vec!["  a", "", "  b"]);
        assert_eq!(plain("para\n\n    indented\n    code", 40), vec!["para", "", "  indented", "  code"]);
        assert_eq!(plain("```\n```", 40), Vec::<String>::new());
    }

    #[test]
    fn long_code_line_is_hard_split_with_the_same_indent() {
        assert_eq!(
            plain("```\nabcdefghijklmnopqrstu\nshort\n```", 10),
            vec!["  abcdefgh", "  ijklmnop", "  qrstu", "  short"]
        );
    }

    #[test]
    fn unterminated_fence_runs_to_the_end() {
        assert_eq!(plain("intro\n\n```\nlet x = 1;\nlet y = 2;", 40), vec!["intro", "", "  let x = 1;", "  let y = 2;"]);
    }

    #[test]
    fn bullet_lists_are_compact_whether_tight_or_loose() {
        assert_eq!(plain("- one\n- two\n- three", 40), vec!["- one", "- two", "- three"]);
        assert_eq!(plain("- one\n\n- two\n\n- three", 40), vec!["- one", "- two", "- three"]);
        assert_eq!(plain("intro\n\n- a\n\noutro", 40), vec!["intro", "", "- a", "", "outro"]);
    }

    #[test]
    fn wrapped_bullet_text_hangs_after_the_marker() {
        assert_eq!(plain("- alpha bet gamma delta", 12), vec!["- alpha bet", "  gamma", "  delta"]);
    }

    #[test]
    fn ordered_lists_count_from_their_start() {
        assert_eq!(plain("1. a\n2. b\n3. c", 40), vec!["1. a", "2. b", "3. c"]);
        assert_eq!(plain("3. first\n4. second\n5. third", 40), vec!["3. first", "4. second", "5. third"]);
        assert_eq!(plain("1. one two three four", 12), vec!["1. one two", "   three", "   four"]);
        assert_eq!(plain("9. nine\n10. ten\n11. eleven", 40), vec!["9. nine", "10. ten", "11. eleven"]);
    }

    #[test]
    fn nested_lists_indent_with_their_parents() {
        assert_eq!(
            plain("- a\n  - b\n    - c\n- d", 40),
            vec!["- a", "  - b", "    - c", "- d"]
        );
        assert_eq!(plain("1. a\n   - b\n   - c\n2. d", 40), vec!["1. a", "   - b", "   - c", "2. d"]);
        assert_eq!(
            plain("- top\n  - nested item that wraps around", 14),
            vec!["- top", "  - nested", "    item that", "    wraps", "    around"]
        );
    }

    #[test]
    fn a_second_paragraph_in_an_item_continues_on_the_hanging_indent() {
        assert_eq!(
            plain("- first para\n\n  second para\n- next", 40),
            vec!["- first para", "  second para", "- next"]
        );
        assert_eq!(
            plain("1. install\n\n   ```sh\n   make\n   ```\n2. run", 40),
            vec!["1. install", "     make", "2. run"]
        );
    }

    #[test]
    fn task_lists_show_their_boxes() {
        assert_eq!(plain("- [ ] todo\n- [x] done", 40), vec!["- [ ] todo", "- [x] done"]);
        assert_eq!(plain("- [ ] one two three", 14), vec!["- [ ] one two", "      three"]);
        assert_eq!(ansi("- [ ] a\n- [x] b", 40).iter().map(|l| strip_ansi(l)).collect::<Vec<_>>(), vec![
            "\u{2022} \u{2610} a",
            "\u{2022} \u{2611} b",
        ]);
    }

    #[test]
    fn blockquotes_are_prefixed_on_every_line() {
        assert_eq!(plain("> quoted text", 40), vec!["> quoted text"]);
        assert_eq!(plain("> one\n>\n> two", 40), vec!["> one", ">", "> two"]);
        assert_eq!(plain("> aaa bbb ccc ddd", 12), vec!["> aaa bbb", "> ccc ddd"]);
        assert_eq!(plain("> > deep", 40), vec!["> > deep"]);
        assert_eq!(plain("> - a\n> - b", 40), vec!["> - a", "> - b"]);
        assert_eq!(plain("before\n\n> q\n\nafter", 40), vec!["before", "", "> q", "", "after"]);
    }

    #[test]
    fn horizontal_rule_is_capped_at_forty_cells() {
        assert_eq!(plain("a\n\n---\n\nb", 80), vec!["a".to_string(), String::new(), "-".repeat(40), String::new(), "b".to_string()]);
        assert_eq!(plain("---", 12), vec!["-".repeat(12)]);
        let styled = ansi("---", 80);
        assert_eq!(strip_ansi(&styled[0]), "\u{2500}".repeat(40));
    }

    #[test]
    fn tables_align_columns_and_honour_alignment_markers() {
        let table = "\
| Name | Qty | Price |
|:-----|:---:|------:|
| apple | 3 | 1.50 |
| kiwi | 12 | 10.00 |";
        assert_eq!(
            plain(table, 80),
            vec![
                "Name  \u{2502} Qty \u{2502} Price",
                "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{253c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{253c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
                "apple \u{2502}  3  \u{2502}  1.50",
                "kiwi  \u{2502} 12  \u{2502} 10.00",
            ]
        );
    }

    #[test]
    fn wide_tables_shrink_their_widest_columns() {
        let table = "\
| id | description |
|----|-------------|
| 1 | a rather long description cell |";
        let rows = plain(table, 20);
        assert_eq!(
            rows,
            vec![
                "id \u{2502} description",
                "\u{2500}\u{2500}\u{2500}\u{253c}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}",
                "1  \u{2502} a rather long \u{2026}",
            ]
        );
        assert!(rows.iter().all(|row| width(row) <= 20), "{rows:?}");
    }

    #[test]
    fn table_cells_may_be_empty_and_rows_ragged() {
        assert_eq!(
            plain("| a | b |\n|---|---|\n| | x |\n| y |", 40),
            vec!["a \u{2502} b", "\u{2500}\u{2500}\u{253c}\u{2500}\u{2500}", "  \u{2502} x", "y \u{2502}"]
        );
    }

    #[test]
    fn html_is_shown_literally() {
        assert_eq!(plain("<div>hi</div>", 40), vec!["<div>hi</div>"]);
        assert_eq!(plain("a <b>b</b> c", 40), vec!["a <b>b</b> c"]);
        assert_eq!(plain("<div>\n  <p>x</p>\n</div>", 40), vec!["<div>", "  <p>x</p>", "</div>"]);
    }

    #[test]
    fn wide_characters_wrap_by_cells() {
        assert_eq!(
            plain("\u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d}", 8),
            vec!["\u{4f60}\u{597d}\u{4e16}\u{754c}", "\u{4f60}\u{597d}\u{4e16}\u{754c}", "\u{4f60}\u{597d}"]
        );
    }

    #[test]
    fn a_word_that_exactly_fills_a_row_does_not_leave_a_leading_space() {
        assert_eq!(plain("abcdefghij klm", 10), vec!["abcdefghij", "klm"]);
    }

    // ---- ansi mode ------------------------------------------------------

    #[test]
    fn inline_styles_use_the_documented_sequences() {
        assert_eq!(ansi("**b**", 40), vec!["\x1b[1mb\x1b[0m"]);
        assert_eq!(ansi("*i*", 40), vec!["\x1b[3mi\x1b[0m"]);
        assert_eq!(ansi("~~s~~", 40), vec!["\x1b[9ms\x1b[0m"]);
        assert_eq!(ansi("`c`", 40), vec!["\x1b[38;2;147;167;247mc\x1b[0m"]);
        assert_eq!(
            ansi("[t](https://u.test)", 40),
            vec!["\x1b[4m\x1b[38;2;147;167;247mt\x1b[0m \x1b[38;2;102;102;102m(https://u.test)\x1b[0m"]
        );
        assert_eq!(
            ansi("a **b `c` d** e", 40),
            vec!["a \x1b[1mb \x1b[38;2;147;167;247mc\x1b[0m\x1b[1m d\x1b[0m e"]
        );
    }

    #[test]
    fn headings_bullets_and_quotes_are_styled() {
        assert_eq!(ansi("# T", 40), vec!["\x1b[1m\x1b[4m\x1b[38;2;255;255;255mT\x1b[0m"]);
        assert_eq!(ansi("## T", 40), vec!["\x1b[1mT\x1b[0m"]);
        assert_eq!(ansi("- x", 40), vec!["\x1b[38;2;215;119;87m\u{2022}\x1b[0m x"]);
        assert_eq!(
            ansi("> q", 40),
            vec!["\x1b[38;2;102;102;102m\u{258e}\x1b[0m \x1b[3m\x1b[2mq\x1b[0m"]
        );
        assert_eq!(ansi("1. x", 40), vec!["\x1b[38;2;215;119;87m1.\x1b[0m x"]);
    }

    #[test]
    fn code_is_light_grey_and_indented() {
        assert_eq!(ansi("```\nab\n\ncd\n```", 40), vec![
            "  \x1b[38;2;200;200;200mab\x1b[0m",
            "",
            "  \x1b[38;2;200;200;200mcd\x1b[0m",
        ]);
    }

    #[test]
    fn wrapped_styles_close_on_each_row_and_reopen_on_the_next() {
        let rows = ansi("**one two three four five six**", 10);
        assert!(rows.len() >= 3, "{rows:?}");
        for row in &rows {
            assert!(row.starts_with("\x1b[1m"), "{row:?}");
            assert!(row.ends_with(RESET), "{row:?}");
        }
        let quote = ansi("> one two three four five six seven", 12);
        for row in &quote {
            assert!(row.starts_with("\x1b[38;2;102;102;102m\u{258e}"), "{row:?}");
            assert!(closed(row), "{row:?}");
        }
    }

    #[test]
    fn hard_break_inside_bold_keeps_both_lines_self_contained() {
        let rows = ansi("**a\\\nb**", 40);
        assert_eq!(rows, vec!["\x1b[1ma\x1b[0m", "\x1b[1mb\x1b[0m"]);
    }

    #[test]
    fn every_ansi_line_is_self_contained() {
        for cols in [10, 24, 80] {
            for row in ansi(SAMPLE, cols) {
                assert!(closed(&row), "width {cols}: style left open in {row:?}");
                assert!(only_own_escapes(&row), "width {cols}: foreign escape in {row:?}");
            }
        }
    }

    #[test]
    fn no_line_exceeds_the_width_on_a_rich_document() {
        for cols in [10, 24, 80] {
            for mode in [Color::Plain, Color::Ansi] {
                let rows = render(SAMPLE, cols, mode);
                assert!(!rows.is_empty());
                for row in &rows {
                    assert!(width(row) <= cols, "{mode:?} width {cols}: {} cells in {row:?}", width(row));
                }
            }
        }
    }

    #[test]
    fn wrapping_leaves_no_empty_styled_spans() {
        for cols in [10, 14, 24, 30, 80] {
            for row in ansi(SAMPLE, cols) {
                assert!(!has_empty_span(&row), "width {cols}: empty span in {row:?}");
            }
        }
    }

    #[test]
    fn plain_output_has_no_escapes_and_matches_the_ansi_text() {
        for cols in [10, 24, 80] {
            let plain_rows = plain(SAMPLE, cols);
            assert!(plain_rows.iter().all(|row| !row.contains('\x1b')), "{plain_rows:?}");
            let stripped: Vec<String> = ansi(SAMPLE, cols).iter().map(|row| strip_ansi(row)).collect();
            assert_eq!(stripped.len(), plain_rows.len(), "width {cols}");
        }
    }

    #[test]
    fn untrusted_escapes_never_reach_the_output() {
        let hostile = "red \x1b[31mtext\x1b[0m and \x1b]0;pwned\x07title \x1b[2J\x1b[Hclear\n\n\
                       &#27;[31m entity &#x1b;]0;x\u{7} done\n\n```\ncode \x1b[31m red\n```\n\n\
                       [l](https://x.test/\x1b[31m) <span title=\"\x1b[1m\">h</span>";
        for cols in [10, 40] {
            let rows = plain(hostile, cols);
            assert!(rows.iter().all(|row| !row.contains('\x1b') && !row.contains('\u{7}')), "{rows:?}");
            assert!(rows.iter().all(|row| width(row) <= cols), "{rows:?}");
            let joined = rows.join(" ");
            assert!(joined.contains("red"), "text survives: {joined:?}");
            let styled = ansi(hostile, cols);
            assert!(styled.iter().all(|row| only_own_escapes(row) && closed(row)), "{styled:?}");
            assert!(styled.iter().all(|row| width(row) <= cols), "{styled:?}");
        }
    }

    // ---- robustness -----------------------------------------------------

    const NASTY: &[&str] = &[
        "```rust\nfn main() { let very_long_identifier_name_that_overflows = 1; }\n",
        "- a\n  - b\n    - c\n      - d wrapped text that goes on and on\n  - e",
        "1. one\n   1. two\n      1. three\n         - four with words to wrap",
        "[link](https://example.com/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/bbbbbbb?q=1)",
        "https://example.com/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "emoji \u{1f980}\u{1f980}\u{1f980} family \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466} flag \u{1f1ef}\u{1f1f5} heart \u{2764}\u{fe0f} done",
        "| a | b |\n|---|---|\n| | x |\n|  |  |",
        "| h1 | h2 | h3 | h4 | h5 |\n|---|---|---|---|---|\n| a | b | c | d | e |",
        "| \u{4f60}\u{597d} | long header text here |\n|:-:|--:|\n| \u{4e16}\u{754c}\u{4e16}\u{754c}\u{4e16}\u{754c} | `code` and **bold** |",
        "> > > deep quote with a very long wordwordwordwordwordword inside",
        "> - quoted list\n>   > nested quote in list",
        "# heading that is rather long and must wrap around several rows\n\n---\n\n",
        "- [ ] task item with long text that wraps\n- [x] done",
        "<div>\n  <span>html block</span>\n</div>\n\n<b>inline</b> html",
        "\u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d}\u{4e16}\u{754c} mixed with ascii words and \u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30c6}\u{30ad}\u{30b9}\u{30c8}",
        "**bold *nested italic ~~strike `code` more~~ text* end** after",
        "a\\\nhard\\\nbreak",
        "* * *\n\n- - -\n\n___",
        "1. \n2. \n-\n>\n#",
        "\u{200b}\u{202e}weird\u{200f} text\r\nwith\rcarriage returns",
        "",
    ];

    #[test]
    fn nasty_inputs_never_panic_and_respect_the_width_at_every_size() {
        for input in NASTY {
            for cols in 0..=40usize {
                let limit = cols.max(8);
                for mode in [Color::Plain, Color::Ansi] {
                    let rows = render(input, cols, mode);
                    for row in &rows {
                        assert!(
                            width(row) <= limit,
                            "{mode:?} width {cols}: {} cells in {row:?} for {input:?}",
                            width(row)
                        );
                        if mode == Color::Ansi {
                            assert!(closed(row), "{mode:?} width {cols}: open style in {row:?}");
                            assert!(!has_empty_span(row), "{mode:?} width {cols}: empty span in {row:?}");
                            assert!(only_own_escapes(row), "{row:?}");
                        } else {
                            assert!(!row.contains('\x1b'), "{row:?}");
                        }
                    }
                    assert!(
                        rows.last().is_none_or(|row| !strip_ansi(row).trim().is_empty()),
                        "trailing blank line for {input:?} at {cols}"
                    );
                }
            }
        }
    }

    #[test]
    fn huge_widths_and_zero_width_are_fine() {
        assert_eq!(plain("a b", usize::MAX), vec!["a b"]);
        assert_eq!(plain("- a\n- b", 0), vec!["- a", "- b"]);
        assert_eq!(plain("a", 1), vec!["a"]);
    }

    #[test]
    fn deeply_nested_containers_stay_fast_and_bounded() {
        let quotes = format!("{}deep text that must still wrap", "> ".repeat(300));
        let lists: String = (0..200).map(|depth| format!("{}- item {depth}\n", "  ".repeat(depth))).collect();
        for input in [quotes, lists] {
            for mode in [Color::Plain, Color::Ansi] {
                let rows = render(&input, 30, mode);
                assert!(!rows.is_empty());
                assert!(rows.iter().all(|row| width(row) <= 30));
            }
        }
    }

    #[test]
    fn narrow_deep_lists_lose_bullets_before_text() {
        let rows = plain("- a\n  - b\n    - c\n      - dddd", 8);
        assert!(rows.iter().all(|row| width(row) <= 8), "{rows:?}");
        assert!(rows.iter().any(|row| row.contains("dddd")), "{rows:?}");
    }

    #[test]
    fn tidy_drops_empty_spans_and_idle_resets() {
        assert_eq!(tidy("plain"), "plain");
        assert_eq!(tidy("\x1b[1mbold\x1b[0m"), "\x1b[1mbold\x1b[0m");
        assert_eq!(tidy("\x1b[1mbold\x1b[0m\x1b[3m\x1b[2m\x1b[0m"), "\x1b[1mbold\x1b[0m");
        assert_eq!(tidy("a\x1b[0m\x1b[1mb\x1b[0m\x1b[0m"), "a\x1b[1mb\x1b[0m");
        assert_eq!(tidy("\x1b[1m\x1b[3mx"), "\x1b[1m\x1b[3mx\x1b[0m");
        assert_eq!(tidy("\x1b[1m\x1b[0m"), "");
    }

    #[test]
    fn column_fitting_caps_the_widest_first() {
        assert_eq!(fit_columns(&[3, 4], 10), vec![3, 4]);
        assert_eq!(fit_columns(&[10, 3], 8), vec![5, 3]);
        assert_eq!(fit_columns(&[10, 10], 9), vec![5, 4]);
        assert_eq!(fit_columns(&[5, 5, 5], 2), vec![1, 1, 1]);
    }
}
