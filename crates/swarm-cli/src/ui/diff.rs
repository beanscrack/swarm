//! Code-diff rendering for the permission dialog (before an `edit_file` or
//! `write_file` call) and for the transcript (after one ran).
//!
//! Every returned line is self-contained: any styling opened on a line is
//! closed before the end of that line, so finished lines can be committed to
//! terminal scrollback one at a time. Rows look like
//!
//! ```text
//! 41 - let answer = 41;
//! 41 + let answer = 42;
//! 42   println!("{answer}");
//! ```
//!
//! a right-aligned line number, a `+`/`-`/space marker, then the text. Changed
//! rows carry a background band that fills the whole width; context rows are
//! plain grey. The text comes from files and tool arguments, so it is
//! sanitised (no escapes, no control characters, tabs become spaces) and never
//! wrapped: a row that does not fit is cut with `…`.

use std::time::Duration;

use similar::{ChangeTag, DiffOp, DiffTag, TextDiff};

use super::text;
use crate::style::{self, Color, Srgb};

/// Unchanged lines kept around each change in a whole-file diff.
const CONTEXT_LINES: usize = 3;
/// Rows are never narrower than this, whatever width the caller reports.
const MIN_WIDTH: usize = 8;
/// Line numbers are always shown in at least this many cells.
const MIN_GUTTER: usize = 2;
/// Upper bound on diffing time, so a huge rewrite cannot stall the UI (the
/// diff then degrades to a coarser, still correct, result).
const DIFF_TIMEOUT: Duration = Duration::from_millis(500);

/// Background of an added row, `rgb(34,92,43)`.
const ADDED_BG: &str = "#225C2B";
/// Text of an added row.
const ADDED_FG: &str = "#E6FFE6";
/// Background of a removed row, `rgb(122,41,54)`.
const REMOVED_BG: &str = "#7A2936";
/// Text of a removed row.
const REMOVED_FG: &str = "#FFE6E6";

/// How many lines a diff added and removed, counted over the whole diff (not
/// only the rows that were shown).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffSummary {
    /// Lines present only in the new text.
    pub added: usize,
    /// Lines present only in the old text.
    pub removed: usize,
}

/// A rendered diff: finished, self-contained terminal rows plus the totals.
#[derive(Debug, Clone)]
pub struct DiffView {
    /// One entry per terminal row, no trailing newline, no indent.
    pub lines: Vec<String>,
    /// Totals for the whole diff, even when `lines` was collapsed.
    pub summary: DiffSummary,
}

/// What a row says about its line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Added,
    Removed,
    Context,
}

impl Kind {
    fn marker(self) -> char {
        match self {
            Kind::Added => '+',
            Kind::Removed => '-',
            Kind::Context => ' ',
        }
    }
}

/// One row before layout: a source line, or the break between two hunks.
#[derive(Debug, Clone, Copy)]
enum Entry<'a> {
    Line { kind: Kind, number: usize, text: &'a str },
    Gap,
}

/// Where the columns of a row fall.
#[derive(Debug, Clone, Copy)]
struct Layout {
    /// Total cells per row, never below [`MIN_WIDTH`].
    width: usize,
    /// Cells of the line-number column; 0 when the terminal is too narrow to
    /// show numbers and still leave room for text.
    gutter: usize,
}

impl Layout {
    fn new(width: usize, widest_number: usize) -> Self {
        let width = width.max(MIN_WIDTH);
        let gutter = digits(widest_number).max(MIN_GUTTER);
        // Number, space, marker, space, and at least one cell of text.
        let gutter = if gutter + 4 <= width { gutter } else { 0 };
        Self { width, gutter }
    }

    /// Cells left of the marker: the number column and its trailing space.
    fn lead(&self) -> usize {
        if self.gutter == 0 { 0 } else { self.gutter + 1 }
    }

    /// Cells available for the line's own text.
    fn text_budget(&self) -> usize {
        self.width.saturating_sub(self.lead() + 2)
    }
}

/// Number of decimal digits in `value` (1 for 0).
fn digits(value: usize) -> usize {
    value.checked_ilog10().map_or(1, |log| log as usize + 1)
}

/// Snippet replace, as the `edit_file` tool describes it: `old` was replaced
/// by `new`, and `first_line` is the 1-based line of the file where `old`
/// starts (0 = unknown, numbering then starts at 1 relative to the snippet).
///
/// Lines the two sides share show as context rows; everything else is a
/// removed row (numbered from the old text) or an added row (numbered from the
/// new text). `width` is the row width in cells and `max_rows` caps the rows
/// returned (0 = no limit), the rest collapsing into `… +N more lines`.
pub fn snippet_diff(
    old: &str,
    new: &str,
    first_line: usize,
    width: usize,
    colors: Color,
    max_rows: usize,
) -> DiffView {
    let old = normalize(old);
    let new = normalize(new);
    let diff = line_diff(&old, &new);
    let summary = summarize(&diff);
    if summary == DiffSummary::default() {
        return DiffView { lines: Vec::new(), summary };
    }
    let groups = vec![diff.ops().to_vec()];
    let entries = collect(&diff, &groups, first_line.max(1));
    DiffView { lines: render(&entries, width, colors, max_rows), summary }
}

/// Whole-file diff: `before` of `None` means the file is new, so every line is
/// an added row. Changes come in hunks with three lines of context, separated
/// by a `⋮` row (`...` without colours). Identical content yields no rows.
pub fn file_diff(
    before: Option<&str>,
    after: &str,
    width: usize,
    colors: Color,
    max_rows: usize,
) -> DiffView {
    let old = normalize(before.unwrap_or(""));
    let new = normalize(after);
    let diff = line_diff(&old, &new);
    let summary = summarize(&diff);
    if summary == DiffSummary::default() {
        return DiffView { lines: Vec::new(), summary };
    }
    let groups = diff.grouped_ops(CONTEXT_LINES);
    let entries = collect(&diff, &groups, 1);
    DiffView { lines: render(&entries, width, colors, max_rows), summary }
}

/// `Added 3 lines, removed 1 line`, `Added 12 lines`, `Removed 1 line` or
/// `No changes`.
pub fn summary_text(summary: &DiffSummary) -> String {
    let noun = |count: usize| if count == 1 { "line" } else { "lines" };
    match (summary.added, summary.removed) {
        (0, 0) => "No changes".to_string(),
        (added, 0) => format!("Added {added} {}", noun(added)),
        (0, removed) => format!("Removed {removed} {}", noun(removed)),
        (added, removed) => {
            format!("Added {added} {}, removed {removed} {}", noun(added), noun(removed))
        }
    }
}

/// The 1-based line of `haystack` on which `needle` first starts (counting
/// `\n`), or `None` when it does not occur. An empty needle occurs at the very
/// start, so it reports line 1.
pub fn line_of(haystack: &str, needle: &str) -> Option<usize> {
    let at = haystack.find(needle)?;
    Some(haystack[..at].matches('\n').count() + 1)
}

/// `\r\n` becomes `\n`, and non-empty text always ends in a newline, so a
/// missing final newline never shows up as a change (nor does a final newline
/// create a phantom empty line).
fn normalize(text: &str) -> String {
    let mut out = text.replace("\r\n", "\n");
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn line_diff<'a>(old: &'a str, new: &'a str) -> TextDiff<'a, 'a, str> {
    TextDiff::configure().timeout(DIFF_TIMEOUT).diff_lines(old, new)
}

/// Added and removed line counts over every op of the diff.
fn summarize(diff: &TextDiff<'_, '_, str>) -> DiffSummary {
    let mut summary = DiffSummary::default();
    for op in diff.ops() {
        let (tag, old, new) = op.as_tag_tuple();
        match tag {
            DiffTag::Equal => {}
            DiffTag::Delete => summary.removed += old.len(),
            DiffTag::Insert => summary.added += new.len(),
            DiffTag::Replace => {
                summary.removed += old.len();
                summary.added += new.len();
            }
        }
    }
    summary
}

/// Turn groups of diff ops into entries, a [`Entry::Gap`] between groups.
/// Within a run of changes the removed lines come first, then the added ones.
/// `first` is the number of the first line of either side.
fn collect<'d>(
    diff: &'d TextDiff<'_, '_, str>,
    groups: &[Vec<DiffOp>],
    first: usize,
) -> Vec<Entry<'d>> {
    let number = |index: Option<usize>| first.saturating_add(index.unwrap_or(0));
    let mut entries = Vec::new();
    for (position, group) in groups.iter().enumerate() {
        if position > 0 {
            entries.push(Entry::Gap);
        }
        let mut removed = Vec::new();
        let mut added = Vec::new();
        for op in group {
            for change in diff.iter_changes(op) {
                let text = change.value();
                match change.tag() {
                    ChangeTag::Equal => {
                        entries.append(&mut removed);
                        entries.append(&mut added);
                        entries.push(Entry::Line {
                            kind: Kind::Context,
                            number: number(change.new_index()),
                            text,
                        });
                    }
                    ChangeTag::Delete => removed.push(Entry::Line {
                        kind: Kind::Removed,
                        number: number(change.old_index()),
                        text,
                    }),
                    ChangeTag::Insert => added.push(Entry::Line {
                        kind: Kind::Added,
                        number: number(change.new_index()),
                        text,
                    }),
                }
            }
        }
        entries.append(&mut removed);
        entries.append(&mut added);
    }
    entries
}

/// Lay the entries out as terminal rows, collapsing past `max_rows`.
fn render(entries: &[Entry<'_>], width: usize, colors: Color, max_rows: usize) -> Vec<String> {
    let (shown, hidden) = if max_rows > 0 && entries.len() > max_rows {
        let (shown, rest) = entries.split_at(max_rows - 1);
        (shown, rest.iter().filter(|entry| matches!(entry, Entry::Line { .. })).count())
    } else {
        (entries, 0)
    };
    let widest = shown
        .iter()
        .filter_map(|entry| match entry {
            Entry::Line { number, .. } => Some(*number),
            Entry::Gap => None,
        })
        .max()
        .unwrap_or(0);
    let layout = Layout::new(width, widest);

    let mut lines: Vec<String> = shown
        .iter()
        .map(|entry| match *entry {
            Entry::Line { kind, number, text } => line_row(kind, number, text, &layout, colors),
            Entry::Gap => gap_row(&layout, colors),
        })
        .collect();
    if hidden > 0 {
        lines.push(more_row(hidden, &layout, colors));
    }
    lines
}

/// One source line: number, marker, text, and for changed rows the padding
/// that turns the background into a full-width band.
fn line_row(kind: Kind, number: usize, raw: &str, layout: &Layout, colors: Color) -> String {
    let raw = raw.strip_suffix('\n').unwrap_or(raw);
    let clean = text::sanitize(raw);
    let fitted = text::truncate(&clean, layout.text_budget());

    let gutter = if layout.gutter == 0 {
        String::new()
    } else {
        format!("{number:>width$}", width = layout.gutter)
    };
    let lead = if layout.gutter == 0 { "" } else { " " };
    let mut body = format!("{lead}{} {fitted}", kind.marker());

    if colors == Color::Ansi && kind != Kind::Context {
        let pad = layout.width.saturating_sub(layout.gutter + text::width(&body));
        body.push_str(&" ".repeat(pad));
    } else if fitted.is_empty() {
        body.truncate(body.trim_end().len());
    }

    let (gutter_style, body_style) = match kind {
        Kind::Added => (Srgb::fg(style::SUBTLE).on(ADDED_BG), Srgb::fg(ADDED_FG).on(ADDED_BG)),
        Kind::Removed => {
            (Srgb::fg(style::SUBTLE).on(REMOVED_BG), Srgb::fg(REMOVED_FG).on(REMOVED_BG))
        }
        Kind::Context => (Srgb::fg(style::SUBTLE), Srgb::fg(style::TOOL)),
    };
    format!("{}{}", gutter_style.paint(colors, &gutter), body_style.paint(colors, &body))
}

/// The break between two hunks, right-aligned under the line numbers.
fn gap_row(layout: &Layout, colors: Color) -> String {
    let symbol = match colors {
        Color::Plain => "...",
        Color::Ansi => "\u{22ee}",
    };
    let row = format!("{symbol:>width$}", width = layout.gutter);
    Srgb::fg(style::SUBTLE).paint(colors, &row)
}

/// `… +N more lines`, indented past the line numbers. Collapsing hides at
/// least two rows and a hunk break is never last, so N is always at least 2.
fn more_row(hidden: usize, layout: &Layout, colors: Color) -> String {
    let row = format!("{}\u{2026} +{hidden} more lines", " ".repeat(layout.lead()));
    Srgb::fg(style::SUBTLE).paint(colors, &text::truncate(&row, layout.width))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Color = Color::Plain;
    const ANSI: Color = Color::Ansi;

    fn plain_snippet(old: &str, new: &str, first: usize) -> Vec<String> {
        snippet_diff(old, new, first, 80, PLAIN, 0).lines
    }

    fn plain_file(before: Option<&str>, after: &str) -> Vec<String> {
        file_diff(before, after, 80, PLAIN, 0).lines
    }

    /// Any styling a line opens must be closed on that same line.
    fn assert_self_contained(line: &str) {
        let mut open = false;
        let mut styled = false;
        for piece in text::pieces(line) {
            if let text::Piece::Escape(seq) = piece {
                styled = true;
                open = seq != "\x1b[0m";
            }
        }
        assert!(!open, "styling left open: {line:?}");
        if styled {
            assert!(line.ends_with("\x1b[0m"), "does not end with a reset: {line:?}");
        }
    }

    /// Small deterministic generator, so the property loop needs no dependency.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> usize {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as usize
        }
    }

    fn random_text(rng: &mut Lcg) -> String {
        const PARTS: [&str; 18] = [
            "a",
            "fn main() {",
            " ",
            "\t",
            "\n",
            "\r\n",
            "\r",
            "\u{4f60}\u{597d}",
            "\u{1f980}",
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}",
            "\u{2764}\u{fe0f}",
            "e\u{301}",
            "\x1b[31m",
            "\x1b]0;title\x07",
            "\x07",
            "\u{202e}",
            "a very long line that goes on and on and on and on and on and on and on",
            "}",
        ];
        let count = rng.next() % 24;
        (0..count).map(|_| PARTS[rng.next() % PARTS.len()]).collect()
    }

    fn twenty_lines() -> (String, String) {
        let before: String = (1..=20).map(|n| format!("l{n}\n")).collect();
        let after = before.replace("l2\n", "L2\n").replace("l18\n", "L18\n");
        (before, after)
    }

    #[test]
    fn snippet_single_line_replace() {
        assert_eq!(
            plain_snippet("let a = 1;", "let a = 2;", 10),
            vec!["10 - let a = 1;", "10 + let a = 2;"]
        );
    }

    #[test]
    fn snippet_unknown_first_line_numbers_from_one() {
        assert_eq!(
            plain_snippet("let a = 1;", "let a = 2;", 0),
            vec![" 1 - let a = 1;", " 1 + let a = 2;"]
        );
    }

    #[test]
    fn snippet_multi_line_replace_keeps_unchanged_middle_as_context() {
        assert_eq!(
            plain_snippet("x\nkeep\ny", "X\nkeep\nY", 5),
            vec![" 5 - x", " 5 + X", " 6   keep", " 7 - y", " 7 + Y"]
        );
    }

    #[test]
    fn snippet_pure_addition() {
        assert_eq!(plain_snippet("", "a\nb", 3), vec![" 3 + a", " 4 + b"]);
        assert_eq!(plain_snippet("a\nc", "a\nb\nc", 7), vec![" 7   a", " 8 + b", " 9   c"]);
    }

    #[test]
    fn snippet_pure_deletion() {
        assert_eq!(plain_snippet("a\nb", "", 3), vec![" 3 - a", " 4 - b"]);
        assert_eq!(plain_snippet("a\nb\nc", "a\nc", 7), vec![" 7   a", " 8 - b", " 8   c"]);
    }

    #[test]
    fn snippet_first_line_offset_forty_keeps_a_two_wide_gutter() {
        assert_eq!(
            plain_snippet("one\ntwo", "one\n2", 40),
            vec!["40   one", "41 - two", "41 + 2"]
        );
    }

    #[test]
    fn snippet_gutter_grows_when_numbers_reach_four_digits() {
        assert_eq!(
            plain_snippet("a\nb\nc\nd", "a\nb\nc\nX", 998),
            vec![" 998   a", " 999   b", "1000   c", "1001 - d", "1001 + X"]
        );
    }

    #[test]
    fn snippet_partial_line_change_shows_whole_lines() {
        assert_eq!(
            plain_snippet("let value = compute(1);", "let value = compute(2);", 3),
            vec![" 3 - let value = compute(1);", " 3 + let value = compute(2);"]
        );
    }

    #[test]
    fn snippet_identical_text_is_empty() {
        let view = snippet_diff("same\ntext", "same\ntext\n", 4, 80, PLAIN, 0);
        assert!(view.lines.is_empty());
        assert_eq!(view.summary, DiffSummary::default());
        let view = snippet_diff("", "", 0, 80, ANSI, 3);
        assert!(view.lines.is_empty());
    }

    #[test]
    fn snippet_summary_counts_added_and_removed() {
        let view = snippet_diff("a\nb\nc", "a\nX\nY\nZ\nc", 1, 80, PLAIN, 0);
        assert_eq!(view.summary, DiffSummary { added: 3, removed: 1 });
        assert_eq!(view.lines, vec![" 1   a", " 2 - b", " 2 + X", " 3 + Y", " 4 + Z", " 5   c"]);
    }

    #[test]
    fn removed_rows_come_before_added_rows() {
        let rows = plain_snippet("a\nb", "c\nd", 1);
        assert_eq!(rows, vec![" 1 - a", " 2 - b", " 1 + c", " 2 + d"]);
    }

    #[test]
    fn file_diff_new_file_is_all_additions() {
        assert_eq!(plain_file(None, "alpha\nbeta\n"), vec![" 1 + alpha", " 2 + beta"]);
        let view = file_diff(None, "alpha\nbeta\n", 80, PLAIN, 0);
        assert_eq!(view.summary, DiffSummary { added: 2, removed: 0 });
    }

    #[test]
    fn file_diff_modification_shows_three_lines_of_context() {
        let before = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n";
        let after = "l1\nl2\nl3\nl4\nCHANGED\nl6\nl7\nl8\n";
        assert_eq!(
            plain_file(Some(before), after),
            vec![
                " 2   l2",
                " 3   l3",
                " 4   l4",
                " 5 - l5",
                " 5 + CHANGED",
                " 6   l6",
                " 7   l7",
                " 8   l8",
            ]
        );
    }

    #[test]
    fn file_diff_separates_distant_hunks_with_dots() {
        let (before, after) = twenty_lines();
        let view = file_diff(Some(&before), &after, 80, PLAIN, 0);
        assert_eq!(
            view.lines,
            vec![
                " 1   l1",
                " 2 - l2",
                " 2 + L2",
                " 3   l3",
                " 4   l4",
                " 5   l5",
                "...",
                "15   l15",
                "16   l16",
                "17   l17",
                "18 - l18",
                "18 + L18",
                "19   l19",
                "20   l20",
            ]
        );
        assert_eq!(view.summary, DiffSummary { added: 2, removed: 2 });
    }

    #[test]
    fn file_diff_numbers_follow_both_sides_after_a_length_change() {
        let before = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let after = "a\nNEW1\nNEW2\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        assert_eq!(
            plain_file(Some(before), after),
            vec![" 1   a", " 2 + NEW1", " 3 + NEW2", " 4   b", " 5   c", " 6   d"]
        );
        let before = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n";
        let after = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        assert_eq!(
            plain_file(Some(before), after),
            vec![" 8   h", " 9   i", "10   j", "11 - k"]
        );
    }

    #[test]
    fn file_diff_identical_is_empty() {
        let view = file_diff(Some("a\nb\n"), "a\nb\n", 80, ANSI, 5);
        assert!(view.lines.is_empty());
        assert_eq!(view.summary, DiffSummary::default());
        assert!(file_diff(None, "", 80, PLAIN, 0).lines.is_empty());
    }

    #[test]
    fn file_diff_ignores_line_ending_and_final_newline_differences() {
        assert!(plain_file(Some("a\r\nb\r\n"), "a\nb\n").is_empty());
        assert!(plain_file(Some("a\nb"), "a\nb\n").is_empty());
        assert!(plain_file(Some("a\nb\n"), "a\r\nb").is_empty());
    }

    #[test]
    fn file_diff_crlf_content_renders_without_carriage_returns() {
        let rows = plain_file(Some("a\r\nb\r\nc\r\n"), "a\r\nB\r\nc\r\n");
        assert_eq!(rows, vec![" 1   a", " 2 - b", " 2 + B", " 3   c"]);
        assert!(rows.iter().all(|row| !row.contains('\r')));
    }

    #[test]
    fn file_diff_final_newline_does_not_create_a_phantom_line() {
        let view = file_diff(None, "only\n", 80, PLAIN, 0);
        assert_eq!(view.lines, vec![" 1 + only"]);
        assert_eq!(view.summary.added, 1);
        let view = file_diff(Some("a\n"), "a\n\n", 80, PLAIN, 0);
        assert_eq!(view.lines, vec![" 1   a", " 2 +"]);
    }

    #[test]
    fn summary_text_handles_singular_and_plural() {
        let text = |added, removed| summary_text(&DiffSummary { added, removed });
        assert_eq!(text(3, 1), "Added 3 lines, removed 1 line");
        assert_eq!(text(1, 3), "Added 1 line, removed 3 lines");
        assert_eq!(text(12, 0), "Added 12 lines");
        assert_eq!(text(1, 0), "Added 1 line");
        assert_eq!(text(0, 1), "Removed 1 line");
        assert_eq!(text(0, 2), "Removed 2 lines");
        assert_eq!(text(0, 0), "No changes");
    }

    #[test]
    fn line_of_finds_the_first_occurrence() {
        let haystack = "zero\none\ntwo\none\nthree";
        assert_eq!(line_of(haystack, "zero"), Some(1));
        assert_eq!(line_of(haystack, "one"), Some(2), "first occurrence wins");
        assert_eq!(line_of(haystack, "two\none"), Some(3), "multi-line needle starts on line 3");
        assert_eq!(line_of(haystack, "three"), Some(5), "needle on the last line");
        assert_eq!(line_of(haystack, "missing"), None);
        assert_eq!(line_of("", "x"), None);
        assert_eq!(line_of("a\nb\n", "b\n"), Some(2));
        assert_eq!(line_of("\u{4f60}\u{597d}\nx", "x"), Some(2), "multi-byte text before it");
    }

    #[test]
    fn collapsing_keeps_the_first_rows_and_counts_the_rest() {
        let after: String = (1..=10).map(|n| format!("row{n}\n")).collect();
        let view = file_diff(None, &after, 80, PLAIN, 4);
        assert_eq!(view.lines, vec![" 1 + row1", " 2 + row2", " 3 + row3", "   … +7 more lines"]);
        assert_eq!(view.summary, DiffSummary { added: 10, removed: 0 });
    }

    #[test]
    fn collapsing_applies_only_past_the_limit() {
        let after = "a\nb\nc\n";
        assert_eq!(file_diff(None, after, 80, PLAIN, 3).lines.len(), 3, "exactly max_rows fits");
        assert_eq!(file_diff(None, after, 80, PLAIN, 0).lines.len(), 3, "0 means no limit");
        assert_eq!(file_diff(None, after, 80, PLAIN, 100).lines.len(), 3);
        assert_eq!(
            file_diff(None, after, 80, PLAIN, 2).lines,
            vec![" 1 + a", "   … +2 more lines"]
        );
        assert_eq!(file_diff(None, after, 80, PLAIN, 1).lines, vec!["   … +3 more lines"]);
    }

    #[test]
    fn collapsing_snippets_keeps_the_whole_summary() {
        let new: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        let view = snippet_diff("old\n", &new, 100, 80, PLAIN, 5);
        assert_eq!(view.lines.len(), 5);
        assert_eq!(view.lines[4], "    … +27 more lines");
        assert_eq!(view.lines[0], "100 - old");
        assert_eq!(view.summary, DiffSummary { added: 30, removed: 1 });
    }

    #[test]
    fn collapsing_sizes_the_gutter_from_the_rows_shown() {
        let after: String = (1..=120).map(|n| format!("x{n}\n")).collect();
        let view = file_diff(None, &after, 80, PLAIN, 4);
        assert_eq!(view.lines[0], " 1 + x1");
        assert_eq!(view.lines[3], "   … +117 more lines");
    }

    #[test]
    fn collapsing_counts_only_lines_not_hunk_breaks() {
        let (before, after) = twenty_lines();
        let view = file_diff(Some(&before), &after, 80, PLAIN, 7);
        assert_eq!(view.lines.len(), 7);
        assert_eq!(view.lines[6], "   … +7 more lines");
        assert_eq!(view.lines[5], " 5   l5");
        let view = file_diff(Some(&before), &after, 80, PLAIN, 8);
        assert_eq!(view.lines[6], "...");
        assert_eq!(view.lines[7], "   … +7 more lines");
    }

    #[test]
    fn collapse_row_is_subtle_and_self_contained_in_ansi() {
        let view = file_diff(None, "a\nb\nc\nd\n", 40, ANSI, 3);
        assert_eq!(
            view.lines.last().map(String::as_str),
            Some("\x1b[38;2;102;102;102m   \u{2026} +2 more lines\x1b[0m")
        );
    }

    #[test]
    fn rows_never_exceed_the_width_for_wide_and_long_text() {
        let long = "x".repeat(500);
        let cjk = "\u{4f60}\u{597d}\u{4e16}\u{754c}".repeat(40);
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        let emoji = format!("\u{1f980}{family}\u{2764}\u{fe0f}").repeat(30);
        let combining = "e\u{301}".repeat(100);
        let mixed = "ab\u{4f60}c\u{1f980}d".repeat(25);
        let before = format!("{long}\n{cjk}\n{emoji}\n{combining}\n{mixed}\n");
        let after = format!("{mixed}\n{combining}\n{emoji}\n{cjk}\n{long}\n");
        for colors in [PLAIN, ANSI] {
            for cols in 10..=80 {
                let snippet = snippet_diff(&before, &after, 998, cols, colors, 0);
                let file = file_diff(Some(&before), &after, cols, colors, 0);
                assert!(!snippet.lines.is_empty() && !file.lines.is_empty());
                for line in snippet.lines.iter().chain(file.lines.iter()) {
                    assert!(
                        text::width(line) <= cols,
                        "{colors:?} width {cols}: {} cells in {line:?}",
                        text::width(line)
                    );
                }
            }
        }
    }

    #[test]
    fn long_lines_are_cut_with_an_ellipsis_not_wrapped() {
        let rows = snippet_diff("short", &"y".repeat(100), 1, 20, PLAIN, 0).lines;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1], " 1 + yyyyyyyyyyyyyy\u{2026}");
        assert_eq!(text::width(&rows[1]), 20);
    }

    #[test]
    fn tabs_expand_and_control_characters_are_dropped() {
        assert_eq!(plain_file(None, "\tindented\x01\x7f"), vec![" 1 +     indented"]);
    }

    #[test]
    fn plain_output_has_no_escapes_and_no_trailing_spaces() {
        let rows = snippet_diff("a\n\nb", "a\n\nc\n\n", 1, 60, PLAIN, 0).lines;
        for row in &rows {
            assert!(!row.contains('\x1b'), "{row:?}");
            assert_eq!(row.as_str(), row.trim_end(), "trailing whitespace in {row:?}");
        }
        assert_eq!(rows, vec![" 1   a", " 2", " 3 - b", " 3 + c", " 4 +"]);
    }

    #[test]
    fn ansi_lines_are_self_contained() {
        let before: String = (1..=30).map(|n| format!("line {n}\t\x1b[31mred\n")).collect();
        let after = before.replace("line 3\t", "LINE 3\t").replace("line 25\t", "LINE 25\t");
        for cols in [1, 8, 12, 40, 120] {
            for rows in [0, 4, 9] {
                let views = [
                    snippet_diff(&before, &after, 7, cols, ANSI, rows),
                    file_diff(Some(&before), &after, cols, ANSI, rows),
                    file_diff(None, &after, cols, ANSI, rows),
                ];
                for view in views {
                    assert!(!view.lines.is_empty());
                    for line in &view.lines {
                        assert_self_contained(line);
                    }
                }
            }
        }
    }

    #[test]
    fn ansi_changed_rows_fill_exactly_the_width() {
        for cols in [8, 9, 10, 12, 20, 33, 80] {
            let view = snippet_diff("keep\nold\nkeep2", "keep\nnew line\nkeep2", 9, cols, ANSI, 0);
            assert_eq!(view.lines.len(), 4);
            for index in [1, 2] {
                assert_eq!(text::width(&view.lines[index]), cols.max(MIN_WIDTH));
            }
            if cols >= 12 {
                for index in [0, 3] {
                    assert!(text::width(&view.lines[index]) < cols, "context rows are not padded");
                }
            }
        }
    }

    #[test]
    fn ansi_changed_rows_use_the_band_colours() {
        let view = snippet_diff("old", "new", 1, 20, ANSI, 0);
        let (removed, added) = (&view.lines[0], &view.lines[1]);
        assert!(removed.contains("\x1b[48;2;122;41;54m"), "{removed:?}");
        assert!(removed.contains("\x1b[38;2;255;230;230m"), "{removed:?}");
        assert!(removed.contains("\x1b[38;2;102;102;102m"), "gutter colour: {removed:?}");
        assert!(added.contains("\x1b[48;2;34;92;43m"), "{added:?}");
        assert!(added.contains("\x1b[38;2;230;255;230m"), "{added:?}");
        assert_eq!(text::strip_ansi(removed), format!("{:<20}", " 1 - old"));
        assert_eq!(text::strip_ansi(added), format!("{:<20}", " 1 + new"));
    }

    #[test]
    fn ansi_context_rows_have_no_background() {
        let view = snippet_diff("a\nold\nc", "a\nnew\nc", 1, 30, ANSI, 0);
        assert_eq!(view.lines.len(), 4);
        for index in [0, 3] {
            let row = &view.lines[index];
            assert!(!row.contains("48;2"), "{row:?}");
            assert!(row.contains("\x1b[38;2;153;153;153m"), "context text is grey: {row:?}");
        }
        let file = file_diff(Some("a\nb\nc\n"), "a\nB\nc\n", 30, ANSI, 0);
        assert!(!file.lines[0].contains("48;2") && !file.lines[3].contains("48;2"));
    }

    #[test]
    fn ansi_hunk_break_is_a_subtle_vertical_ellipsis() {
        let (before, after) = twenty_lines();
        let view = file_diff(Some(&before), &after, 30, ANSI, 0);
        assert_eq!(view.lines[6], "\x1b[38;2;102;102;102m \u{22ee}\x1b[0m");
    }

    #[test]
    fn untrusted_escapes_and_bells_never_reach_the_output() {
        let evil = "a\x1b[31mred\x07b\x1b]0;pwned\x07c";
        for colors in [PLAIN, ANSI] {
            let view = snippet_diff(evil, "safe", 1, 40, colors, 0);
            let all = view.lines.join("\n");
            assert!(!all.contains("\x1b[31m"), "{all:?}");
            assert!(!all.contains('\x07'), "{all:?}");
            assert!(!all.contains("pwned"), "{all:?}");
            assert!(text::strip_ansi(&view.lines[0]).contains("aredbc"), "{:?}", view.lines[0]);
            for line in &view.lines {
                assert_self_contained(line);
            }
        }
        assert_eq!(plain_file(None, "x\x1b[2Jy\x07"), vec![" 1 + xy"]);
    }

    #[test]
    fn degenerate_widths_and_limits_do_not_panic() {
        for cols in 0..=3 {
            for rows in 0..=3 {
                for colors in [PLAIN, ANSI] {
                    let snippet = snippet_diff("a\nb", "c\nd\ne", 0, cols, colors, rows);
                    let file = file_diff(Some("a\nb"), "c\nd\ne", cols, colors, rows);
                    for line in snippet.lines.iter().chain(file.lines.iter()) {
                        assert!(text::width(line) <= MIN_WIDTH, "{line:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn huge_line_numbers_neither_overflow_nor_overfill_the_row() {
        for first in [usize::MAX, usize::MAX - 1, 1 << 40, 99_999, 100_000] {
            for cols in [1, 8, 12, 24, 80] {
                for colors in [PLAIN, ANSI] {
                    let view = snippet_diff("a\nb", "c\nd", first, cols, colors, 0);
                    assert_eq!(view.lines.len(), 4);
                    for line in &view.lines {
                        assert!(text::width(line) <= cols.max(MIN_WIDTH), "{line:?}");
                        assert_self_contained(line);
                    }
                }
            }
        }
        let view = snippet_diff("a", "b", usize::MAX, 80, PLAIN, 0);
        assert_eq!(view.lines[0], format!("{} - a", usize::MAX));
    }

    #[test]
    fn narrow_terminals_drop_line_numbers_rather_than_overflow() {
        let view = snippet_diff("a", "b", 123_456, 8, PLAIN, 0);
        assert_eq!(view.lines, vec!["- a", "+ b"]);
        let view = snippet_diff("a", "b", 123_456, 14, PLAIN, 0);
        assert_eq!(view.lines, vec!["123456 - a", "123456 + b"]);
    }

    #[test]
    fn random_inputs_never_panic_and_never_overflow_the_width() {
        let mut rng = Lcg(0x5eed);
        for round in 0..400 {
            let old = random_text(&mut rng);
            let new = if round % 7 == 0 { old.clone() } else { random_text(&mut rng) };
            let first = match round % 5 {
                0 => 0,
                1 => 1 + rng.next() % 50,
                2 => 995 + rng.next() % 10,
                3 => rng.next(),
                _ => usize::MAX - rng.next() % 3,
            };
            let width = rng.next() % 41;
            let max_rows = rng.next() % 6;
            for colors in [PLAIN, ANSI] {
                let views = [
                    snippet_diff(&old, &new, first, width, colors, max_rows),
                    file_diff(Some(&old), &new, width, colors, max_rows),
                    file_diff(None, &new, width, colors, max_rows),
                ];
                for view in views {
                    if max_rows > 0 {
                        assert!(view.lines.len() <= max_rows);
                    }
                    for line in &view.lines {
                        assert!(
                            text::width(line) <= width.max(MIN_WIDTH),
                            "width {width}, {colors:?}: {line:?} for {old:?} -> {new:?}"
                        );
                        assert!(!line.contains(['\n', '\r', '\x07']), "{line:?}");
                        if colors == ANSI {
                            assert_self_contained(line);
                        } else {
                            assert!(!line.contains('\x1b'), "{line:?}");
                        }
                    }
                }
            }
        }
    }
}
