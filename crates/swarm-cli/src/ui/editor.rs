//! The prompt's text buffer: multi-line, grapheme-aware, with undo, a kill
//! ring, word-wrapped layout and visual-row cursor movement.
//!
//! It knows nothing about terminals. The cursor is a byte offset that always
//! sits on a grapheme boundary, so emoji, combining marks and CJK text edit as
//! single units. All behaviour here is plain data in, plain data out, which is
//! what makes the keyboard handling testable.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const UNDO_LIMIT: usize = 100;

/// A pasted blob collapsed to a placeholder in the buffer, restored on submit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Paste {
    placeholder: String,
    content: String,
}

#[derive(Debug, Clone, Default)]
pub struct Editor {
    text: String,
    cursor: usize,
    undo: Vec<(String, usize)>,
    kill: String,
    pastes: Vec<Paste>,
    paste_seq: usize,
    /// True right after a character insert, so a run of typing is one undo step.
    typing: bool,
    /// The last typed character was whitespace: the next word starts a new step.
    after_space: bool,
}

/// One wrapped row of the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Byte range of the row in the buffer (the `\n` that ends it excluded).
    pub start: usize,
    pub end: usize,
}

/// The buffer laid out for a given width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub rows: Vec<Row>,
    /// Where the cursor sits: row index and display column within that row.
    pub cursor: (usize, usize),
}

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replace the whole buffer, cursor at the end. Not an undoable edit: this
    /// is history recall and completion, which have their own way back.
    pub fn set(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.len();
        self.typing = false;
    }

    pub fn clear(&mut self) {
        self.snapshot();
        self.text.clear();
        self.cursor = 0;
        self.pastes.clear();
        self.typing = false;
    }

    /// The text to send: paste placeholders replaced by what was pasted.
    pub fn expanded(&self) -> String {
        let mut text = self.text.clone();
        for paste in &self.pastes {
            text = text.replace(&paste.placeholder, &paste.content);
        }
        text
    }

    /// Call after submitting so the next prompt starts clean.
    pub fn reset(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.undo.clear();
        self.pastes.clear();
        self.paste_seq = 0;
        self.typing = false;
    }

    fn snapshot(&mut self) {
        if self.undo.last().map(|(text, _)| text.as_str()) == Some(self.text.as_str()) {
            return;
        }
        self.undo.push((self.text.clone(), self.cursor));
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
    }

    pub fn undo(&mut self) -> bool {
        match self.undo.pop() {
            Some((text, cursor)) => {
                self.text = text;
                self.cursor = cursor.min(self.text.len());
                self.typing = false;
                true
            }
            None => false,
        }
    }

    // ---- insertion ------------------------------------------------------

    pub fn insert_char(&mut self, ch: char) {
        // A word and the space after it are one undo step.
        if !self.typing || (self.after_space && !ch.is_whitespace()) {
            self.snapshot();
        }
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.typing = true;
        self.after_space = ch.is_whitespace();
    }

    pub fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.snapshot();
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.typing = false;
    }

    pub fn newline(&mut self) {
        self.insert_char('\n');
    }

    /// Insert pasted text. Big pastes collapse to `[Pasted text #1 +42 lines]`
    /// so the prompt stays readable; the content is restored on submit.
    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let lines = text.matches('\n').count();
        if lines >= 6 || text.chars().count() > 1000 {
            self.paste_seq += 1;
            let placeholder = if lines > 0 {
                format!("[Pasted text #{} +{} lines]", self.paste_seq, lines + 1)
            } else {
                format!("[Pasted text #{} {} chars]", self.paste_seq, text.chars().count())
            };
            self.insert_str(&placeholder);
            self.pastes.push(Paste { placeholder, content: text });
        } else {
            self.insert_str(&text);
        }
    }

    // ---- deletion -------------------------------------------------------

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // A paste placeholder is one unit, as in Claude Code.
        if let Some(index) = self
            .pastes
            .iter()
            .position(|paste| self.text[..self.cursor].ends_with(&paste.placeholder))
        {
            self.snapshot();
            let len = self.pastes[index].placeholder.len();
            self.text.replace_range(self.cursor - len..self.cursor, "");
            self.cursor -= len;
            self.pastes.remove(index);
            self.typing = false;
            return;
        }
        let start = self.prev_boundary(self.cursor);
        if !self.typing {
            self.snapshot();
        }
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        self.snapshot();
        let end = self.next_boundary(self.cursor);
        self.text.replace_range(self.cursor..end, "");
        self.typing = false;
    }

    /// Ctrl+K: delete to the end of the line; at the end of a line, join the
    /// next one.
    pub fn kill_to_end(&mut self) {
        let end = self.line_end(self.cursor);
        let end = if end == self.cursor && end < self.text.len() { end + 1 } else { end };
        self.cut(self.cursor, end);
    }

    /// Ctrl+U: delete back to the start of the line.
    pub fn kill_to_start(&mut self) {
        let start = self.line_start(self.cursor);
        let start = if start == self.cursor && start > 0 { start - 1 } else { start };
        self.cut(start, self.cursor);
    }

    /// Ctrl+W / Alt+Backspace: delete the word before the cursor.
    pub fn kill_word_back(&mut self) {
        let start = self.word_left_of(self.cursor);
        self.cut(start, self.cursor);
    }

    /// Alt+D: delete the word after the cursor.
    pub fn kill_word_forward(&mut self) {
        let end = self.word_right_of(self.cursor);
        self.cut(self.cursor, end);
    }

    fn cut(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        self.snapshot();
        self.kill = self.text[start..end].to_string();
        self.text.replace_range(start..end, "");
        self.cursor = start;
        self.typing = false;
    }

    /// Ctrl+Y: put back the last killed text.
    pub fn yank(&mut self) {
        let text = self.kill.clone();
        self.insert_str(&text);
    }

    // ---- movement -------------------------------------------------------

    pub fn left(&mut self) {
        self.cursor = self.prev_boundary(self.cursor);
        self.typing = false;
    }

    pub fn right(&mut self) {
        self.cursor = self.next_boundary(self.cursor);
        self.typing = false;
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_left_of(self.cursor);
        self.typing = false;
    }

    pub fn word_right(&mut self) {
        self.cursor = self.word_right_of(self.cursor);
        self.typing = false;
    }

    /// Home / Ctrl+A: start of the current logical line.
    pub fn line_home(&mut self) {
        self.cursor = self.line_start(self.cursor);
        self.typing = false;
    }

    /// End / Ctrl+E: end of the current logical line.
    pub fn line_end_key(&mut self) {
        self.cursor = self.line_end(self.cursor);
        self.typing = false;
    }

    /// Move one visual row up or down, keeping the column. Returns false when
    /// there is no row in that direction, so the caller can walk history.
    pub fn move_vertical(&mut self, down: bool, cols: usize) -> bool {
        let layout = self.layout(cols);
        let (row, col) = layout.cursor;
        let target = if down {
            if row + 1 >= layout.rows.len() {
                return false;
            }
            row + 1
        } else {
            if row == 0 {
                return false;
            }
            row - 1
        };
        self.cursor = self.byte_at_column(&layout.rows[target], col);
        self.typing = false;
        true
    }

    /// Byte offset in `row` whose display column is closest to `col`.
    fn byte_at_column(&self, row: &Row, col: usize) -> usize {
        let mut used = 0;
        for (offset, grapheme) in self.text[row.start..row.end].grapheme_indices(true) {
            let cells = grapheme.width();
            if used + cells > col {
                return row.start + offset;
            }
            used += cells;
        }
        row.end
    }

    fn prev_boundary(&self, from: usize) -> usize {
        self.text[..from]
            .grapheme_indices(true)
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0)
    }

    fn next_boundary(&self, from: usize) -> usize {
        self.text[from..]
            .graphemes(true)
            .next()
            .map(|grapheme| from + grapheme.len())
            .unwrap_or(from)
    }

    fn line_start(&self, at: usize) -> usize {
        self.text[..at].rfind('\n').map(|index| index + 1).unwrap_or(0)
    }

    fn line_end(&self, at: usize) -> usize {
        self.text[at..].find('\n').map(|index| at + index).unwrap_or(self.text.len())
    }

    fn word_left_of(&self, from: usize) -> usize {
        let head = &self.text[..from];
        let trimmed = head.trim_end_matches(|c: char| c.is_whitespace());
        let word_start = trimmed
            .rfind(|c: char| c.is_whitespace())
            .map(|index| index + trimmed[index..].chars().next().map_or(1, char::len_utf8))
            .unwrap_or(0);
        word_start
    }

    fn word_right_of(&self, from: usize) -> usize {
        let tail = &self.text[from..];
        let skipped = tail.len() - tail.trim_start_matches(|c: char| c.is_whitespace()).len();
        let rest = &tail[skipped..];
        let word = rest
            .find(|c: char| c.is_whitespace())
            .unwrap_or(rest.len());
        from + skipped + word
    }

    /// The word being typed at the cursor and where it starts: the run of
    /// non-whitespace characters ending at the cursor.
    pub fn word_at_cursor(&self) -> (usize, &str) {
        let head = &self.text[..self.cursor];
        let start = head
            .rfind(|c: char| c.is_whitespace())
            .map(|index| index + head[index..].chars().next().map_or(1, char::len_utf8))
            .unwrap_or(0);
        (start, &self.text[start..self.cursor])
    }

    /// Replace `start..cursor` with `with`.
    pub fn replace_before_cursor(&mut self, start: usize, with: &str) {
        self.snapshot();
        self.text.replace_range(start..self.cursor, with);
        self.cursor = start + with.len();
        self.typing = false;
    }

    // ---- layout ---------------------------------------------------------

    /// Wrap the buffer to `cols` display columns. Breaks fall after the last
    /// space that fits, or inside a word that is wider than the row.
    pub fn layout(&self, cols: usize) -> Layout {
        let cols = cols.max(2);
        let mut rows: Vec<Row> = Vec::new();
        let mut line_start = 0usize;
        for line in self.text.split('\n') {
            let line_end = line_start + line.len();
            rows.extend(wrap_line(&self.text, line_start, line_end, cols));
            line_start = line_end + 1;
        }
        // Cursor at the very end of a full row (or after a trailing newline)
        // needs a row of its own to sit on.
        if let Some(last) = rows.last() {
            let full = self.text[last.start..last.end].width() >= cols;
            if self.cursor == self.text.len() && full {
                rows.push(Row { start: self.text.len(), end: self.text.len() });
            }
        }
        let mut cursor = (0, 0);
        for (index, row) in rows.iter().enumerate() {
            // The last row whose start is at or before the cursor wins, so a
            // cursor on a wrap boundary shows at the start of the next row.
            if row.start <= self.cursor {
                let upto = self.cursor.min(row.end);
                cursor = (index, self.text[row.start..upto].width());
            }
        }
        Layout { rows, cursor }
    }

    pub fn row_text(&self, row: &Row) -> &str {
        self.text[row.start..row.end].trim_end_matches(' ')
    }
}

fn wrap_line(text: &str, start: usize, end: usize, cols: usize) -> Vec<Row> {
    let line = &text[start..end];
    if line.width() <= cols {
        return vec![Row { start, end }];
    }
    let mut rows = Vec::new();
    let mut row_start = start;
    let mut used = 0usize;
    // Byte offset just after the most recent space on this row.
    let mut break_at: Option<usize> = None;
    for (offset, grapheme) in line.grapheme_indices(true) {
        let at = start + offset;
        let cells = grapheme.width();
        if used + cells > cols && at > row_start {
            let cut = match break_at {
                Some(cut) if cut > row_start => cut,
                _ => at,
            };
            rows.push(Row { start: row_start, end: cut });
            row_start = cut;
            used = text[cut..at].width();
            break_at = None;
        }
        used += cells;
        if grapheme == " " {
            break_at = Some(at + 1);
        }
    }
    rows.push(Row { start: row_start, end });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(text: &str) -> Editor {
        let mut editor = Editor::new();
        editor.set(text);
        editor
    }

    #[test]
    fn typing_inserts_at_the_cursor() {
        let mut editor = Editor::new();
        for ch in "helo".chars() {
            editor.insert_char(ch);
        }
        editor.left();
        editor.insert_char('l');
        assert_eq!(editor.text(), "hello");
        assert_eq!(editor.cursor(), 4);
    }

    /// Regression: backspace used to move the cursor twice, so `abc` + ⌫ + `X`
    /// came out as `aXb`, and at index 1 it underflowed and killed the reader.
    #[test]
    fn backspace_moves_the_cursor_exactly_once() {
        let mut editor = editor("abc");
        editor.backspace();
        assert_eq!((editor.text(), editor.cursor()), ("ab", 2));
        editor.insert_char('X');
        assert_eq!(editor.text(), "abX");

        let mut editor = Editor::new();
        editor.insert_char('a');
        editor.backspace();
        assert_eq!((editor.text(), editor.cursor()), ("", 0));
        editor.backspace();
        assert_eq!((editor.text(), editor.cursor()), ("", 0), "backspace on empty is a no-op");
    }

    #[test]
    fn backspace_and_delete_remove_whole_graphemes() {
        let mut editor = editor("a\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}e\u{301}");
        editor.backspace();
        assert_eq!(editor.text(), "a\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}", "e + combining accent goes together");
        editor.backspace();
        assert_eq!(editor.text(), "a", "a ZWJ family is one grapheme");
        let mut editor = Editor::new();
        editor.set("\u{4f60}\u{597d}");
        editor.line_home();
        editor.delete();
        assert_eq!(editor.text(), "\u{597d}");
    }

    #[test]
    fn horizontal_movement_respects_graphemes_and_bounds() {
        let mut editor = editor("a\u{4f60}b");
        editor.left();
        editor.left();
        assert_eq!(editor.cursor(), 1);
        editor.left();
        editor.left();
        assert_eq!(editor.cursor(), 0, "left stops at the start");
        editor.right();
        editor.right();
        editor.right();
        editor.right();
        assert_eq!(editor.cursor(), editor.text().len(), "right stops at the end");
    }

    #[test]
    fn word_movement_and_kills() {
        let mut editor = editor("fix the login bug");
        editor.word_left();
        assert_eq!(&editor.text()[editor.cursor()..], "bug");
        editor.word_left();
        assert_eq!(&editor.text()[editor.cursor()..], "login bug");
        editor.word_right();
        assert_eq!(&editor.text()[..editor.cursor()], "fix the login");

        let mut editor = self::editor("fix the login bug");
        editor.kill_word_back();
        assert_eq!(editor.text(), "fix the login ");
        editor.kill_word_back();
        assert_eq!(editor.text(), "fix the ");
        editor.yank();
        assert_eq!(editor.text(), "fix the login ");
    }

    #[test]
    fn kill_to_end_and_start_work_per_line() {
        let mut editor = editor("one\ntwo three\nfour");
        editor.cursor = 9; // "two t|hree"
        editor.kill_to_end();
        assert_eq!(editor.text(), "one\ntwo t\nfour");
        editor.kill_to_start();
        assert_eq!(editor.text(), "one\n\nfour");
        editor.yank();
        assert_eq!(editor.text(), "one\ntwo t\nfour");
    }

    #[test]
    fn kill_at_a_line_edge_joins_lines() {
        let mut editor = editor("ab\ncd");
        editor.cursor = 2;
        editor.kill_to_end();
        assert_eq!(editor.text(), "abcd");
    }

    #[test]
    fn undo_steps_back_by_word() {
        let mut editor = Editor::new();
        for ch in "fix it now".chars() {
            editor.insert_char(ch);
        }
        assert_eq!(editor.text(), "fix it now");
        assert!(editor.undo());
        assert_eq!(editor.text(), "fix it ");
        assert!(editor.undo());
        assert_eq!(editor.text(), "fix ");
        assert!(editor.undo());
        assert_eq!(editor.text(), "");
        assert!(!editor.undo(), "nothing left to undo");
    }

    #[test]
    fn newline_inserts_a_line_break() {
        let mut editor = editor("ab");
        editor.left();
        editor.newline();
        assert_eq!(editor.text(), "a\nb");
        assert_eq!(editor.cursor(), 2);
    }

    #[test]
    fn small_pastes_insert_verbatim() {
        let mut editor = Editor::new();
        editor.paste("two\nlines");
        assert_eq!(editor.text(), "two\nlines");
        assert_eq!(editor.expanded(), "two\nlines");
    }

    #[test]
    fn big_pastes_collapse_and_expand_on_submit() {
        let mut editor = Editor::new();
        editor.insert_str("review ");
        let big: String = (0..40).map(|n| format!("line {n}\n")).collect();
        editor.paste(&big);
        assert_eq!(editor.text(), "review [Pasted text #1 +41 lines]");
        assert_eq!(editor.expanded(), format!("review {big}"));
        editor.backspace();
        assert_eq!(editor.text(), "review ", "backspace removes the placeholder whole");
        assert_eq!(editor.expanded(), "review ");
    }

    #[test]
    fn crlf_pastes_normalise() {
        let mut editor = Editor::new();
        editor.paste("a\r\nb\rc");
        assert_eq!(editor.text(), "a\nb\nc");
    }

    #[test]
    fn layout_wraps_at_spaces() {
        let editor = editor("the quick brown fox");
        let layout = editor.layout(10);
        let rows: Vec<&str> = layout.rows.iter().map(|row| editor.row_text(row)).collect();
        assert_eq!(rows, vec!["the quick", "brown fox"]);
        assert_eq!(layout.cursor, (1, 9));
    }

    #[test]
    fn layout_splits_long_words() {
        let editor = editor("abcdefghijklmnopqrstuvwxyz");
        let layout = editor.layout(10);
        let rows: Vec<&str> = layout.rows.iter().map(|row| editor.row_text(row)).collect();
        assert_eq!(rows, vec!["abcdefghij", "klmnopqrst", "uvwxyz"]);
        assert_eq!(layout.cursor, (2, 6));
    }

    #[test]
    fn layout_gives_a_full_last_row_a_row_for_the_cursor() {
        let editor = editor("0123456789");
        let layout = editor.layout(10);
        assert_eq!(layout.rows.len(), 2);
        assert_eq!(layout.cursor, (1, 0));
    }

    #[test]
    fn layout_follows_newlines_and_blank_lines() {
        let editor = editor("one\n\nthree");
        let layout = editor.layout(20);
        let rows: Vec<&str> = layout.rows.iter().map(|row| editor.row_text(row)).collect();
        assert_eq!(rows, vec!["one", "", "three"]);
        assert_eq!(layout.cursor, (2, 5));
        let trailing = self::editor("one\n");
        assert_eq!(trailing.layout(20).cursor, (1, 0), "after a trailing newline the cursor is on a new row");
    }

    #[test]
    fn layout_measures_wide_characters_in_cells() {
        let editor = editor("\u{4f60}\u{597d}\u{4e16}\u{754c}");
        let layout = editor.layout(6);
        let rows: Vec<&str> = layout.rows.iter().map(|row| editor.row_text(row)).collect();
        assert_eq!(rows, vec!["\u{4f60}\u{597d}\u{4e16}", "\u{754c}"]);
        assert_eq!(layout.cursor, (1, 2));
    }

    #[test]
    fn vertical_movement_keeps_the_column() {
        let mut editor = editor("abcdef\nxy\nghijkl");
        editor.cursor = 5; // after 'e' on the first row
        assert!(editor.move_vertical(true, 20));
        assert_eq!(editor.cursor(), 9, "short row clamps to its end");
        assert!(editor.move_vertical(true, 20));
        assert_eq!(&editor.text()[..editor.cursor()], "abcdef\nxy\ngh");
        assert!(!editor.move_vertical(true, 20), "no row below");
        editor.cursor = 2;
        assert!(!editor.move_vertical(false, 20), "no row above");
    }

    #[test]
    fn vertical_movement_walks_wrapped_rows() {
        let mut editor = editor("the quick brown fox");
        editor.cursor = editor.text().len();
        assert!(editor.move_vertical(false, 10));
        assert_eq!(editor.layout(10).cursor.0, 0);
        assert!(editor.move_vertical(true, 10));
        assert_eq!(editor.layout(10).cursor.0, 1);
    }

    #[test]
    fn word_at_cursor_finds_the_token_being_typed() {
        let mut editor = editor("open @src/ma");
        assert_eq!(editor.word_at_cursor(), (5, "@src/ma"));
        editor.replace_before_cursor(5, "@src/main.rs ");
        assert_eq!(editor.text(), "open @src/main.rs ");
        let empty = Editor::new();
        assert_eq!(empty.word_at_cursor(), (0, ""));
    }

    #[test]
    fn set_and_clear() {
        let mut editor = editor("hello");
        editor.clear();
        assert!(editor.is_empty());
        assert!(editor.undo(), "clearing is undoable");
        assert_eq!(editor.text(), "hello");
        editor.reset();
        assert!(!editor.undo(), "reset forgets undo history");
    }
}
