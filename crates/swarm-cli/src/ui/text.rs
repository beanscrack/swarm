//! ANSI-aware text measurement, truncation and wrapping.
//!
//! The live region can only be repainted correctly if every line's printed
//! width is known exactly, so everything here measures *cells*, not bytes or
//! chars: escape sequences count for zero, East Asian wide characters and most
//! emoji for two, combining marks for zero.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const RESET: &str = "\x1b[0m";

/// A run of printable text, or one escape sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece<'a> {
    Text(&'a str),
    Escape(&'a str),
}

/// Split a string into printable runs and escape sequences (CSI, OSC and the
/// short two/three byte forms).
pub fn pieces(text: &str) -> Vec<Piece<'_>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != 0x1b {
            index += 1;
            continue;
        }
        if start < index {
            out.push(Piece::Text(&text[start..index]));
        }
        let end = escape_end(bytes, index);
        out.push(Piece::Escape(&text[index..end]));
        index = end;
        start = end;
    }
    if start < bytes.len() {
        out.push(Piece::Text(&text[start..]));
    }
    out
}

/// Byte index just past the escape sequence starting at `at` (an ESC byte).
fn escape_end(bytes: &[u8], at: usize) -> usize {
    let mut i = at + 1;
    match bytes.get(i) {
        // CSI: parameters, intermediates, one final byte in 0x40..=0x7e.
        Some(b'[') => {
            i += 1;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            (i + 1).min(bytes.len())
        }
        // OSC (hyperlinks, titles): ends at BEL or ESC \.
        Some(b']') => {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    return i + 1;
                }
                if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                    return i + 2;
                }
                i += 1;
            }
            bytes.len()
        }
        // Charset selection and friends: ESC, intermediates, one final byte.
        Some(_) => {
            while i < bytes.len() && (0x20..=0x2f).contains(&bytes[i]) {
                i += 1;
            }
            (i + 1).min(bytes.len())
        }
        None => bytes.len(),
    }
}

/// Remove every escape sequence.
pub fn strip_ansi(text: &str) -> String {
    pieces(text)
        .into_iter()
        .filter_map(|piece| match piece {
            Piece::Text(text) => Some(text),
            Piece::Escape(_) => None,
        })
        .collect()
}

/// Printed width in terminal cells.
pub fn width(text: &str) -> usize {
    pieces(text)
        .into_iter()
        .map(|piece| match piece {
            Piece::Text(text) => text.width(),
            Piece::Escape(_) => 0,
        })
        .sum()
}

/// Make untrusted text safe to print: escape sequences and control characters
/// are dropped (a tool that `cat`s a binary must not be able to repaint, retitle
/// or reconfigure the user's terminal), tabs become spaces, `\r\n` becomes `\n`.
/// Newlines survive; callers split on them.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for piece in pieces(text) {
        let Piece::Text(run) = piece else { continue };
        let mut chars = run.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\r' => {
                    // A bare CR is a progress-bar redraw; keep the last frame
                    // by dropping what it would overwrite, but keep CRLF as LF.
                    if chars.peek() != Some(&'\n') {
                        while out.chars().last().is_some_and(|last| last != '\n') {
                            out.pop();
                        }
                    }
                }
                '\t' => out.push_str("    "),
                '\n' => out.push('\n'),
                // C0, DEL, C1 controls, and bidi/format overrides that can
                // reorder what the user sees.
                ch if ch.is_control() => {}
                '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200b}'..='\u{200f}' => {}
                ch => out.push(ch),
            }
        }
    }
    out
}

/// Cut `text` to at most `max` cells, ending with `…` when something was cut.
/// Open styling is closed so a cut line cannot bleed colour into the next row.
pub fn truncate(text: &str, max: usize) -> String {
    if width(text) <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let budget = max - 1;
    let mut out = String::new();
    let mut used = 0;
    let mut styled = false;
    'outer: for piece in pieces(text) {
        match piece {
            Piece::Escape(seq) => {
                out.push_str(seq);
                if is_sgr(seq) {
                    styled = !is_reset(seq);
                }
            }
            Piece::Text(run) => {
                for grapheme in run.graphemes(true) {
                    let cells = grapheme.width();
                    if used + cells > budget {
                        break 'outer;
                    }
                    out.push_str(grapheme);
                    used += cells;
                }
            }
        }
    }
    out.push('\u{2026}');
    if styled {
        out.push_str(RESET);
    }
    out
}

/// Pad with spaces on the right up to `target` cells (never truncates).
pub fn pad_right(text: &str, target: usize) -> String {
    let have = width(text);
    if have >= target {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(target - have))
    }
}

/// `left` and `right` on one row of `cols` cells, the gap between them filled
/// with spaces. When they do not both fit, `right` is dropped.
pub fn spread(left: &str, right: &str, cols: usize) -> String {
    let (lw, rw) = (width(left), width(right));
    if right.is_empty() || lw + rw + 2 > cols {
        return truncate(left, cols);
    }
    format!("{left}{}{right}", " ".repeat(cols - lw - rw))
}

fn is_sgr(seq: &str) -> bool {
    seq.starts_with("\x1b[") && seq.ends_with('m')
}

fn is_reset(seq: &str) -> bool {
    matches!(seq, "\x1b[0m" | "\x1b[m")
}

/// One printable cell group (a grapheme cluster) or an escape sequence.
enum Item {
    Escape(String),
    Cluster { text: String, cells: usize, space: bool },
}

/// Wrap to `cols` cells with a hanging indent: `first` prefixes the first row,
/// `rest` every following one. Breaks fall on spaces where possible and inside
/// a word only when the word alone is wider than the row. Styling that is open
/// at a break is closed on the row and reopened on the next, so wrapped coloured
/// text keeps its colour and never bleeds.
pub fn wrap_hang(text: &str, cols: usize, first: &str, rest: &str) -> Vec<String> {
    let first_w = width(first);
    let rest_w = width(rest);
    let cols = cols.max(first_w.max(rest_w) + 4);

    let mut items: Vec<Item> = Vec::new();
    for piece in pieces(text) {
        match piece {
            Piece::Escape(seq) => items.push(Item::Escape(seq.to_string())),
            Piece::Text(run) => {
                for grapheme in run.graphemes(true) {
                    items.push(Item::Cluster {
                        cells: grapheme.width(),
                        space: grapheme == " ",
                        text: grapheme.to_string(),
                    });
                }
            }
        }
    }

    let mut rows: Vec<String> = Vec::new();
    // Items of the row being built, the cell count so far, and the best break
    // opportunity: (index into `row` after which to cut, SGR state there).
    let mut row: Vec<&Item> = Vec::new();
    let mut used = 0usize;
    let mut active: Vec<String> = Vec::new();
    let mut brk: Option<(usize, Vec<String>)> = None;
    let mut row_start_state: Vec<String> = Vec::new();

    let emit = |row: &[&Item], start_state: &[String], prefix: &str, end_state: &[String]| {
        let mut line = String::from(prefix);
        for seq in start_state {
            line.push_str(seq);
        }
        let mut tail = row.len();
        // Trailing spaces are the break, not content.
        while tail > 0
            && matches!(row[tail - 1], Item::Cluster { space: true, .. })
        {
            tail -= 1;
        }
        for item in &row[..tail] {
            match item {
                Item::Escape(seq) => line.push_str(seq),
                Item::Cluster { text, .. } => line.push_str(text),
            }
        }
        if !end_state.is_empty() {
            line.push_str(RESET);
        }
        line
    };

    for item in &items {
        match item {
            Item::Escape(seq) => {
                if is_sgr(seq) {
                    if is_reset(seq) {
                        active.clear();
                    } else {
                        active.push(seq.clone());
                    }
                }
                row.push(item);
            }
            Item::Cluster { cells, space, .. } => {
                // Normally one cut is enough; a wide cluster arriving on a row
                // that a long word already filled can need a second.
                //
                // A space never forces a cut: it may hang past the edge (it is
                // trimmed when the row is emitted), so a word that exactly fills
                // a row does not push the space after it onto the next one.
                while !*space
                    && used + cells + if rows.is_empty() { first_w } else { rest_w } > cols
                    && !row.is_empty()
                {
                    // Break at the last space if there was one, else mid-word.
                    let (cut, state_at_cut) = match brk.take() {
                        Some((index, state)) if index > 0 => (index, state),
                        _ => (row.len(), active.clone()),
                    };
                    let prefix = if rows.is_empty() { first } else { rest };
                    rows.push(emit(&row[..cut], &row_start_state, prefix, &state_at_cut));
                    let mut carried: Vec<&Item> = row[cut..].to_vec();
                    // The next row starts after the break; skip the spaces.
                    while matches!(carried.first(), Some(Item::Cluster { space: true, .. })) {
                        carried.remove(0);
                    }
                    row_start_state = state_at_cut;
                    used = carried
                        .iter()
                        .map(|it| match it {
                            Item::Cluster { cells, .. } => *cells,
                            Item::Escape(_) => 0,
                        })
                        .sum();
                    row = carried;
                }
                // A break is only worth taking after some real content, or a
                // line indented with spaces would wrap to an empty row.
                if *space && row.iter().any(|it| matches!(it, Item::Cluster { space: false, .. })) {
                    row.push(item);
                    used += cells;
                    brk = Some((row.len(), active.clone()));
                } else {
                    row.push(item);
                    used += cells;
                }
            }
        }
    }
    let prefix = if rows.is_empty() { first } else { rest };
    rows.push(emit(&row, &row_start_state, prefix, &active));
    rows
}

/// [`wrap_hang`] with no indent.
#[cfg(test)]
pub fn wrap(text: &str, cols: usize) -> Vec<String> {
    wrap_hang(text, cols, "", "")
}

/// Wrap every `\n`-separated line of `text`, preserving blank lines.
#[cfg(test)]
pub fn wrap_lines(text: &str, cols: usize, first: &str, rest: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (index, line) in text.split('\n').enumerate() {
        let lead = if index == 0 { first } else { rest };
        out.extend(wrap_hang(line, cols, lead, rest));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_ignores_escapes_and_counts_wide_characters() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("\x1b[38;2;215;119;87mabc\x1b[0m"), 3);
        assert_eq!(width("\u{4f60}\u{597d}"), 4, "CJK characters are two cells");
        assert_eq!(width("\x1b]8;;https://x.test\x07link\x1b]8;;\x07"), 4, "OSC 8 is invisible");
        assert_eq!(width("e\u{301}"), 1, "a combining mark adds nothing");
        assert_eq!(width(""), 0);
    }

    #[test]
    fn strip_ansi_leaves_only_text() {
        assert_eq!(strip_ansi("\x1b[1mbold\x1b[0m plain \x1b[2K"), "bold plain ");
        assert_eq!(strip_ansi("no escapes"), "no escapes");
        assert_eq!(strip_ansi("lone \x1b"), "lone ");
    }

    #[test]
    fn sanitize_removes_terminal_control_but_keeps_text() {
        assert_eq!(sanitize("a\x1b[31mred\x1b[0m b"), "ared b");
        assert_eq!(sanitize("title\x1b]0;pwned\x07 ok"), "title ok");
        assert_eq!(sanitize("one\r\ntwo"), "one\ntwo");
        assert_eq!(sanitize("a\tb"), "a    b");
        assert_eq!(sanitize("bell\x07 nul\x00 del\x7f"), "bell nul del");
        assert_eq!(sanitize("progress 10%\rprogress 100%"), "progress 100%");
        assert_eq!(sanitize("evil\u{202e}txt"), "eviltxt", "bidi override is dropped");
        assert_eq!(sanitize("\u{4f60}\u{597d} \u{1f980}"), "\u{4f60}\u{597d} \u{1f980}");
    }

    #[test]
    fn truncate_cuts_by_cells_and_closes_styles() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello\u{2026}");
        assert_eq!(truncate("\u{4f60}\u{597d}\u{4e16}\u{754c}", 5), "\u{4f60}\u{597d}\u{2026}");
        let cut = truncate("\x1b[31mred text that is long\x1b[0m", 8);
        assert_eq!(strip_ansi(&cut), "red tex\u{2026}");
        assert!(cut.ends_with("\x1b[0m"), "an open colour must be closed: {cut:?}");
        assert_eq!(width(&cut), 8);
        assert_eq!(truncate("anything", 0), "");
    }

    #[test]
    fn spread_right_aligns_and_drops_the_right_when_cramped() {
        let row = spread("left", "right", 20);
        assert_eq!(row.len(), 20);
        assert!(row.starts_with("left") && row.ends_with("right"));
        assert_eq!(spread("left", "right", 8), "left", "no room: the right side goes");
    }

    #[test]
    fn wrap_breaks_on_spaces() {
        assert_eq!(wrap("the quick brown fox", 10), vec!["the quick", "brown fox"]);
        assert_eq!(wrap("short", 10), vec!["short"]);
        assert_eq!(wrap("", 10), vec![""]);
    }

    #[test]
    fn wrap_splits_a_word_longer_than_the_row() {
        let rows = wrap("abcdefghijklmnopqrstuvwxyz", 10);
        assert!(rows.iter().all(|row| width(row) <= 10), "{rows:?}");
        assert_eq!(rows.concat(), "abcdefghijklmnopqrstuvwxyz");
    }

    #[test]
    fn wrap_hangs_continuation_rows() {
        let rows = wrap_hang("one two three four five six", 14, "- ", "  ");
        assert_eq!(rows[0], "- one two");
        assert!(rows[1..].iter().all(|row| row.starts_with("  ")), "{rows:?}");
        assert!(rows.iter().all(|row| width(row) <= 14), "{rows:?}");
    }

    #[test]
    fn wrap_carries_colour_across_rows_and_never_bleeds() {
        let rows = wrap("\x1b[31mred words that wrap around\x1b[0m done", 12);
        assert!(rows.len() >= 3, "{rows:?}");
        for row in &rows[..rows.len() - 1] {
            if strip_ansi(row).contains("red") || row.contains("\x1b[31m") {
                assert!(row.ends_with(RESET), "row must close its colour: {row:?}");
            }
        }
        assert!(rows[1].starts_with("\x1b[31m"), "colour reopens on the next row: {rows:?}");
        assert!(rows.iter().all(|row| width(row) <= 12), "{rows:?}");
    }

    #[test]
    fn wrap_respects_wide_characters() {
        let rows = wrap("\u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d}", 8);
        assert!(rows.iter().all(|row| width(row) <= 8), "{rows:?}");
        assert_eq!(rows.concat(), "\u{4f60}\u{597d}\u{4e16}\u{754c}\u{4f60}\u{597d}");
    }

    /// A word that exactly fills a row must not push the space after it onto
    /// the next row, where it would cost a cell.
    #[test]
    fn wrap_lets_an_exactly_full_row_keep_its_trailing_space() {
        assert_eq!(wrap("three four", 10), vec!["three four"], "10 cells fit in 10");
        assert_eq!(wrap("dddd eeee", 4), vec!["dddd", "eeee"]);
        assert_eq!(wrap("aaaa bbbb cccc", 9), vec!["aaaa bbbb", "cccc"]);
        assert_eq!(wrap_hang("ab cd", 8, "- ", "  "), vec!["- ab cd"]);
    }

    #[test]
    fn wrap_lines_keeps_blank_lines() {
        let rows = wrap_lines("a\n\nb", 10, "> ", "> ");
        assert_eq!(rows, vec!["> a", "> ", "> b"]);
    }
}
