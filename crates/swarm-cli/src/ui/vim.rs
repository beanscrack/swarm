//! Vim keys for the prompt (`/vim`), as Claude Code's editor mode: a normal
//! mode with motions, `d`/`c`/`y` operators and counts, on top of the ordinary
//! editor. Insert mode is the prompt as it always was.
//!
//! It only ever sees a key before the prompt does. Anything it does not own
//! (control keys, arrows, Enter, Tab) passes straight through, so submitting,
//! history and menus keep working in either mode.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::editor::Editor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Insert,
    Normal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Delete,
    Change,
    Yank,
}

impl Op {
    fn key(self) -> char {
        match self {
            Op::Delete => 'd',
            Op::Change => 'c',
            Op::Yank => 'y',
        }
    }
}

/// What the prompt should do with a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Vim dealt with it.
    Handled,
    /// Not vim's: handle it as usual.
    Pass,
    /// `j` / `k`: one row down or up, falling back to history at the ends.
    Move { down: bool },
}

#[derive(Debug)]
pub struct Vim {
    enabled: bool,
    mode: Mode,
    count: Option<usize>,
    operator: Option<Op>,
    /// The register holds whole lines (`yy`, `dd`), which `p` puts on their own line.
    linewise: bool,
}

impl Vim {
    pub fn new(enabled: bool) -> Self {
        Self { enabled, mode: Mode::Insert, count: None, operator: None, linewise: false }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        self.mode = Mode::Insert;
        self.count = None;
        self.operator = None;
    }

    /// What the footer shows.
    pub fn label(&self) -> Option<&'static str> {
        self.enabled.then(|| match self.mode {
            Mode::Insert => "-- INSERT --",
            Mode::Normal => "-- NORMAL --",
        })
    }

    #[cfg(test)]
    pub fn is_normal(&self) -> bool {
        self.enabled && self.mode == Mode::Normal
    }

    /// Back to insert mode and clean, as after a submit.
    pub fn reset(&mut self) {
        self.mode = Mode::Insert;
        self.count = None;
        self.operator = None;
    }

    pub fn key(&mut self, editor: &mut Editor, key: KeyEvent) -> Outcome {
        if !self.enabled {
            return Outcome::Pass;
        }
        let plain = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match (self.mode, key.code) {
            (Mode::Insert, KeyCode::Esc) => {
                self.mode = Mode::Normal;
                // Like vim: the cursor steps back onto the last character typed.
                let (start, _) = editor.line_bounds();
                if editor.cursor() > start {
                    editor.left();
                }
                Outcome::Handled
            }
            (Mode::Insert, _) => Outcome::Pass,
            (Mode::Normal, KeyCode::Esc) => {
                let pending = self.operator.take().is_some() | self.count.take().is_some();
                if pending { Outcome::Handled } else { Outcome::Pass }
            }
            (Mode::Normal, KeyCode::Char(ch)) if plain => self.normal(editor, ch),
            // Control keys, arrows, Enter, Tab: the prompt's own.
            (Mode::Normal, _) => {
                self.operator = None;
                self.count = None;
                Outcome::Pass
            }
        }
    }

    fn insert(&mut self) -> Outcome {
        self.mode = Mode::Insert;
        Outcome::Handled
    }

    fn normal(&mut self, editor: &mut Editor, ch: char) -> Outcome {
        // A count: `3w`, `2dd`. A lone `0` is the line start, not a count.
        if let Some(digit) = ch.to_digit(10).filter(|digit| *digit > 0 || self.count.is_some()) {
            let count = self.count.unwrap_or(0).saturating_mul(10).saturating_add(digit as usize);
            self.count = Some(count.min(999));
            return Outcome::Handled;
        }
        let count = self.count.take().unwrap_or(1).max(1);

        if let Some(op) = self.operator.take() {
            if ch == op.key() {
                self.linewise(editor, op, count);
            } else if let Some((start, end)) = range_of(editor, ch, count, Some(op)) {
                self.apply(editor, op, start, end);
            }
            // An unknown motion cancels the operator.
            self.finish(editor);
            return Outcome::Handled;
        }

        let outcome = match ch {
            'd' => self.start(Op::Delete, count),
            'c' => self.start(Op::Change, count),
            'y' => self.start(Op::Yank, count),
            'i' => return self.insert(),
            'a' => {
                let (_, end) = editor.line_bounds();
                if editor.cursor() < end {
                    editor.right();
                }
                return self.insert();
            }
            'I' => {
                editor.line_home();
                return self.insert();
            }
            'A' => {
                editor.line_end_key();
                return self.insert();
            }
            'o' => {
                editor.line_end_key();
                editor.newline();
                return self.insert();
            }
            'O' => {
                editor.line_home();
                editor.newline();
                editor.left();
                return self.insert();
            }
            'x' => {
                for _ in 0..count {
                    let (_, end) = editor.line_bounds();
                    if editor.cursor() < end {
                        let next = editor.next_grapheme(editor.cursor());
                        editor.cut_range(editor.cursor(), next);
                    }
                }
                self.linewise = false;
                Outcome::Handled
            }
            'X' => {
                for _ in 0..count {
                    let (start, _) = editor.line_bounds();
                    if editor.cursor() > start {
                        let prev = editor.prev_grapheme(editor.cursor());
                        editor.cut_range(prev, editor.cursor());
                    }
                }
                self.linewise = false;
                Outcome::Handled
            }
            'D' | 'C' => {
                let op = if ch == 'D' { Op::Delete } else { Op::Change };
                let (_, end) = editor.line_bounds();
                self.apply(editor, op, editor.cursor(), end);
                Outcome::Handled
            }
            's' => {
                let end = editor.next_grapheme(editor.cursor()).min(editor.line_bounds().1);
                self.apply(editor, Op::Change, editor.cursor(), end);
                Outcome::Handled
            }
            'S' => {
                self.linewise(editor, Op::Change, 1);
                Outcome::Handled
            }
            'Y' => {
                self.linewise(editor, Op::Yank, count);
                Outcome::Handled
            }
            'p' | 'P' => {
                self.put(editor, ch == 'P', count);
                Outcome::Handled
            }
            'u' => {
                for _ in 0..count {
                    editor.undo();
                }
                Outcome::Handled
            }
            'j' => return Outcome::Move { down: true },
            'k' => return Outcome::Move { down: false },
            _ if is_motion(ch) => {
                if let Some((start, end)) = range_of(editor, ch, count, None) {
                    // A motion moves to its far end: forward ones to `end`, back
                    // ones to `start`; `e` lands on the last character, not past it.
                    let target = if ch == 'e' {
                        editor.prev_grapheme(end)
                    } else if end > editor.cursor() {
                        end
                    } else {
                        start
                    };
                    editor.set_cursor(target);
                }
                Outcome::Handled
            }
            _ => Outcome::Handled,
        };
        self.finish(editor);
        outcome
    }

    fn start(&mut self, op: Op, count: usize) -> Outcome {
        self.operator = Some(op);
        // `2d3w` is rare enough to treat as `3w`; keep the count for the motion.
        self.count = (count > 1).then_some(count);
        Outcome::Handled
    }

    /// Normal mode keeps the cursor on a character, not past the last one.
    fn finish(&mut self, editor: &mut Editor) {
        if self.mode == Mode::Normal {
            let (start, end) = editor.line_bounds();
            if editor.cursor() >= end && end > start {
                editor.set_cursor(editor.prev_grapheme(end));
            }
        }
    }

    fn apply(&mut self, editor: &mut Editor, op: Op, start: usize, end: usize) {
        self.linewise = false;
        if start >= end {
            if op == Op::Change {
                self.mode = Mode::Insert;
            }
            return;
        }
        match op {
            Op::Delete => editor.cut_range(start, end),
            Op::Change => {
                editor.cut_range(start, end);
                self.mode = Mode::Insert;
            }
            Op::Yank => {
                let text = editor.text()[start..end].to_string();
                editor.set_kill(&text);
                editor.set_cursor(start);
            }
        }
    }

    /// `dd`, `cc`, `yy` (and `3dd`): whole lines.
    fn linewise(&mut self, editor: &mut Editor, op: Op, count: usize) {
        let (first, mut last) = editor.line_bounds();
        for _ in 1..count {
            if last < editor.text().len() {
                last = editor.text()[last + 1..].find('\n').map_or(editor.text().len(), |at| last + 1 + at);
            }
        }
        let lines = editor.text()[first..last].to_string();
        match op {
            Op::Yank => {
                editor.set_kill(&format!("{lines}\n"));
                self.linewise = true;
            }
            Op::Change => {
                editor.cut_range(first, last);
                editor.set_kill(&format!("{lines}\n"));
                self.linewise = true;
                self.mode = Mode::Insert;
            }
            Op::Delete => {
                // The newline that ends the lines goes too, or the one before when it is the last.
                let (from, to) = if last < editor.text().len() {
                    (first, last + 1)
                } else if first > 0 {
                    (first - 1, last)
                } else {
                    (first, last)
                };
                editor.cut_range(from, to);
                editor.set_kill(&format!("{lines}\n"));
                self.linewise = true;
                let (start, _) = editor.line_bounds();
                editor.set_cursor(start);
            }
        }
    }

    fn put(&mut self, editor: &mut Editor, before: bool, count: usize) {
        let text = editor.kill_text().to_string();
        if text.is_empty() {
            return;
        }
        if self.linewise {
            let body = text.trim_end_matches('\n').repeat(1);
            let block = vec![body; count].join("\n");
            if before {
                editor.line_home();
                let at = editor.cursor();
                editor.insert_str(&format!("{block}\n"));
                editor.set_cursor(at);
            } else {
                editor.line_end_key();
                editor.newline();
                editor.insert_str(&block);
                editor.line_home();
            }
        } else {
            if !before {
                let (_, end) = editor.line_bounds();
                if editor.cursor() < end {
                    editor.right();
                }
            }
            let at = editor.cursor();
            editor.insert_str(&text.repeat(count));
            editor.set_cursor(editor.cursor().saturating_sub(1).max(at));
        }
        self.finish(editor);
    }
}

fn is_motion(ch: char) -> bool {
    matches!(ch, 'h' | 'l' | 'w' | 'b' | 'e' | '0' | '^' | '$')
}

/// The span a motion covers from the cursor, for an operator (`op`) or a plain
/// move. `cw` stops at the end of the word; `dw` and `yw` do not run on to the
/// next line when the word is the last on its own.
fn range_of(editor: &Editor, motion: char, count: usize, op: Option<Op>) -> Option<(usize, usize)> {
    let text = editor.text();
    let at = editor.cursor();
    let (line_start, line_end) = editor.line_bounds();
    match motion {
        'h' => {
            let mut start = at;
            for _ in 0..count {
                if start > line_start {
                    start = editor.prev_grapheme(start);
                }
            }
            Some((start, at))
        }
        'l' => {
            let mut end = at;
            for _ in 0..count {
                if end < line_end {
                    end = editor.next_grapheme(end);
                }
            }
            Some((at, end))
        }
        'w' if op == Some(Op::Change) => {
            let mut end = at;
            for _ in 0..count {
                end = word_end(text, end);
            }
            Some((at, end))
        }
        'w' => {
            let mut end = at;
            for _ in 0..count {
                end = next_word_start(text, end);
            }
            if op.is_some() {
                end = end.min(line_end);
            }
            Some((at, end))
        }
        'e' => {
            let mut end = at;
            for _ in 0..count {
                end = word_end(text, editor.next_grapheme(end));
            }
            Some((at, end))
        }
        'b' => {
            let mut start = at;
            for _ in 0..count {
                start = prev_word_start(text, start);
            }
            Some((start, at))
        }
        '0' => Some((line_start, at)),
        '^' => {
            let first = text[line_start..line_end].find(|c: char| !c.is_whitespace()).map_or(line_end, |i| line_start + i);
            Some((first.min(at), first.max(at)))
        }
        '$' => Some((at, line_end)),
        _ => None,
    }
}

#[derive(PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(ch: char) -> Class {
    if ch.is_whitespace() {
        Class::Space
    } else if ch.is_alphanumeric() || ch == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// Start of the next word: skip the rest of this one, then the space.
fn next_word_start(text: &str, from: usize) -> usize {
    let mut chars = text[from..].char_indices().peekable();
    let Some(&(_, first)) = chars.peek() else { return from };
    let here = class(first);
    if here != Class::Space {
        while chars.next_if(|(_, c)| class(*c) == here).is_some() {}
    }
    while chars.next_if(|(_, c)| class(*c) == Class::Space).is_some() {}
    chars.peek().map_or(text.len(), |(index, _)| from + index)
}

/// Just past the last character of the word at (or after) `from`.
fn word_end(text: &str, from: usize) -> usize {
    let mut chars = text[from..].char_indices().peekable();
    while chars.next_if(|(_, c)| class(*c) == Class::Space && *c != '\n').is_some() {}
    let Some(&(_, first)) = chars.peek() else { return text.len() };
    if first == '\n' {
        return from + chars.peek().map_or(0, |(index, _)| *index);
    }
    let here = class(first);
    while chars.next_if(|(_, c)| class(*c) == here).is_some() {}
    chars.peek().map_or(text.len(), |(index, _)| from + index)
}

/// Start of the word before `from`.
fn prev_word_start(text: &str, from: usize) -> usize {
    let head: Vec<(usize, char)> = text[..from].char_indices().collect();
    let mut i = head.len();
    while i > 0 && class(head[i - 1].1) == Class::Space && head[i - 1].1 != '\n' {
        i -= 1;
    }
    if i == 0 {
        return 0;
    }
    let here = class(head[i - 1].1);
    if head[i - 1].1 == '\n' {
        return head[i - 1].0;
    }
    while i > 0 && class(head[i - 1].1) == here && head[i - 1].1 != '\n' {
        i -= 1;
    }
    head.get(i).map_or(from, |(index, _)| *index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A prompt in normal mode with `text`, cursor at `cursor`.
    fn rig(text: &str, cursor: usize) -> (Vim, Editor) {
        let mut editor = Editor::new();
        editor.set(text);
        editor.set_cursor(cursor);
        let mut vim = Vim::new(true);
        vim.mode = Mode::Normal;
        (vim, editor)
    }

    fn keys(vim: &mut Vim, editor: &mut Editor, sequence: &str) {
        for ch in sequence.chars() {
            let key = if ch == '\u{1b}' { KeyCode::Esc } else { KeyCode::Char(ch) };
            vim.key(editor, KeyEvent::new(key, KeyModifiers::NONE));
        }
    }

    fn run(text: &str, cursor: usize, sequence: &str) -> (String, usize) {
        let (mut vim, mut editor) = rig(text, cursor);
        keys(&mut vim, &mut editor, sequence);
        (editor.text().to_string(), editor.cursor())
    }

    #[test]
    fn insert_mode_is_the_ordinary_prompt_until_escape() {
        let mut vim = Vim::new(true);
        let mut editor = Editor::new();
        assert_eq!(vim.label(), Some("-- INSERT --"));
        let outcome = vim.key(&mut editor, KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(outcome, Outcome::Pass, "typing is the prompt's job");
        editor.set("hello");
        assert_eq!(vim.key(&mut editor, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Outcome::Handled);
        assert_eq!(vim.label(), Some("-- NORMAL --"));
        assert_eq!(editor.cursor(), 4, "the cursor steps back onto the last character");
        // A second Esc is the prompt's (clear, rewind), and so are control keys, Enter and arrows.
        assert_eq!(vim.key(&mut editor, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Outcome::Pass);
        assert_eq!(vim.key(&mut editor, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Outcome::Pass);
        assert_eq!(vim.key(&mut editor, KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), Outcome::Pass);
        assert_eq!(vim.key(&mut editor, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)), Outcome::Pass);
        keys(&mut vim, &mut editor, "i");
        assert_eq!(vim.label(), Some("-- INSERT --"));
    }

    #[test]
    fn off_means_off() {
        let mut vim = Vim::new(false);
        let mut editor = Editor::new();
        assert_eq!(vim.label(), None);
        assert_eq!(vim.key(&mut editor, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Outcome::Pass);
        assert!(!vim.is_normal());
    }

    #[test]
    fn typed_letters_in_normal_mode_never_reach_the_text() {
        assert_eq!(run("hello", 0, "qzZ"), ("hello".to_string(), 0));
    }

    #[test]
    fn motions_move_by_character_word_and_line() {
        assert_eq!(run("hello world", 0, "l").1, 1);
        assert_eq!(run("hello world", 0, "3l").1, 3);
        assert_eq!(run("hello world", 4, "h").1, 3);
        assert_eq!(run("hello world", 0, "w").1, 6);
        assert_eq!(run("hello world", 8, "b").1, 6);
        assert_eq!(run("hello world", 6, "b").1, 0);
        assert_eq!(run("hello world", 0, "e").1, 4, "onto the last character of the word");
        assert_eq!(run("hello world", 4, "e").1, 10, "from the end of a word, the next word's end");
        assert_eq!(run("one\ntwo", 0, "w").1, 4, "w crosses to the next line");
        assert_eq!(run("one\ntwo", 0, "ww").1, 6, "and stops on the last character of the text");
        assert_eq!(run("one two\nx", 4, "dw"), ("one \nx".to_string(), 3), "dw stays on its line");
        assert_eq!(run("hello world", 3, "$").1, 10, "onto the last character");
        assert_eq!(run("hello world", 7, "0").1, 0);
        assert_eq!(run("  indented", 8, "^").1, 2);
        assert_eq!(run("foo.bar baz", 0, "w").1, 3, "punctuation is its own word");
        assert_eq!(run("a\nbc", 0, "$").1, 0, "motions stay on the line");
        assert_eq!(run("a\nbc", 2, "0").1, 2);
    }

    #[test]
    fn operators_take_motions_and_counts() {
        assert_eq!(run("hello world", 0, "dw"), ("world".to_string(), 0));
        assert_eq!(run("one two three", 0, "2dw"), ("three".to_string(), 0));
        assert_eq!(run("hello world", 6, "d$"), ("hello ".to_string(), 5), "the cursor lands on the last character");
        assert_eq!(run("hello world", 6, "db"), ("world".to_string(), 0));
        assert_eq!(run("hello world", 5, "d0"), (" world".to_string(), 0));
        assert_eq!(run("hello world", 0, "x"), ("ello world".to_string(), 0));
        assert_eq!(run("hello world", 0, "3x"), ("lo world".to_string(), 0));
        assert_eq!(run("hello", 4, "X"), ("helo".to_string(), 3));
        assert_eq!(run("hello world", 0, "D"), (String::new(), 0));
        assert_eq!(run("hello world", 0, "dz"), ("hello world".to_string(), 0), "an unknown motion cancels it");
    }

    #[test]
    fn change_deletes_and_starts_typing() {
        let (mut vim, mut editor) = rig("hello world", 0);
        keys(&mut vim, &mut editor, "cw");
        assert_eq!(editor.text(), " world", "cw changes the word, not the space after it");
        assert_eq!(vim.label(), Some("-- INSERT --"));
        let (mut vim, mut editor) = rig("hello world", 6);
        keys(&mut vim, &mut editor, "C");
        assert_eq!(editor.text(), "hello ");
        assert!(!vim.is_normal());
        let (mut vim, mut editor) = rig("hello", 1);
        keys(&mut vim, &mut editor, "s");
        assert_eq!(editor.text(), "hllo");
        assert!(!vim.is_normal());
    }

    #[test]
    fn whole_lines_delete_change_and_yank() {
        assert_eq!(run("one\ntwo\nthree", 5, "dd"), ("one\nthree".to_string(), 4));
        assert_eq!(run("one\ntwo\nthree", 10, "dd"), ("one\ntwo".to_string(), 4), "the last line takes its newline with it");
        assert_eq!(run("only", 2, "dd"), (String::new(), 0));
        assert_eq!(run("a\nb\nc\nd", 0, "2dd"), ("c\nd".to_string(), 0));
        let (mut vim, mut editor) = rig("one\ntwo", 0);
        keys(&mut vim, &mut editor, "cc");
        assert_eq!(editor.text(), "\ntwo", "the line is emptied but stays");
        assert!(!vim.is_normal());
    }

    #[test]
    fn yank_and_put_work_for_words_and_for_lines() {
        assert_eq!(run("one two", 0, "ywP").0, "one one two", "yw takes the space too; P puts before the cursor");
        assert_eq!(run("ab", 0, "ylp").0, "aab", "p puts after the cursor");
        assert_eq!(run("one\ntwo", 0, "yyp"), ("one\none\ntwo".to_string(), 4), "a line goes on its own line, below");
        assert_eq!(run("one\ntwo", 4, "yyP").0, "one\ntwo\ntwo", "above for P");
        assert_eq!(run("a\nb", 0, "ddp").0, "b\na", "a deleted line comes back with p");
        assert_eq!(run("x", 0, "p").0, "x", "an empty register puts nothing");
        assert_eq!(run("ab", 0, "xp").0, "ba");
    }

    #[test]
    fn opening_lines_and_appending() {
        let (mut vim, mut editor) = rig("one\ntwo", 0);
        keys(&mut vim, &mut editor, "oabc");
        // `o` made the new line; typing is the prompt's job, so the vim layer only switched modes.
        assert!(!vim.is_normal());
        assert_eq!(editor.text(), "one\n\ntwo", "a line opened below");
        let (mut vim, mut editor) = rig("ab", 0);
        keys(&mut vim, &mut editor, "a");
        assert_eq!((editor.cursor(), vim.is_normal()), (1, false), "a appends after the character");
        let (mut vim, mut editor) = rig("ab cd", 4);
        keys(&mut vim, &mut editor, "I");
        assert_eq!(editor.cursor(), 0);
        keys(&mut vim, &mut editor, "\u{1b}A");
        assert_eq!(editor.cursor(), 5, "A is after the last character");
        let (mut vim, mut editor) = rig("one", 0);
        keys(&mut vim, &mut editor, "O");
        assert_eq!(editor.text(), "\none", "a line opened above");
        assert_eq!(editor.cursor(), 0);
    }

    #[test]
    fn undo_and_vertical_motion() {
        let (mut vim, mut editor) = rig("hello", 0);
        keys(&mut vim, &mut editor, "xx");
        assert_eq!(editor.text(), "llo");
        keys(&mut vim, &mut editor, "u");
        assert_eq!(editor.text(), "ello");
        keys(&mut vim, &mut editor, "2u");
        assert_eq!(editor.text(), "hello");
        assert_eq!(vim.key(&mut editor, KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)), Outcome::Move { down: true });
        assert_eq!(vim.key(&mut editor, KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)), Outcome::Move { down: false });
    }

    #[test]
    fn escape_cancels_a_half_typed_command() {
        let (mut vim, mut editor) = rig("hello world", 0);
        keys(&mut vim, &mut editor, "d");
        assert_eq!(vim.key(&mut editor, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Outcome::Handled);
        keys(&mut vim, &mut editor, "w");
        assert_eq!(editor.text(), "hello world", "the d was cancelled, w only moved");
        assert_eq!(editor.cursor(), 6);
        assert_eq!(vim.key(&mut editor, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Outcome::Pass);
    }

    #[test]
    fn unicode_text_is_edited_whole_characters_at_a_time() {
        assert_eq!(run("héllo wörld", 0, "lx").0, "hllo wörld");
        assert_eq!(run("日本語 text", 0, "dw").0, "text");
        assert_eq!(run("a🙂b", 1, "x").0, "ab", "an emoji goes as one unit");
        assert_eq!(run("café", 0, "$").1, 3);
    }

    #[test]
    fn random_keys_never_panic_or_leave_the_cursor_off_a_boundary() {
        let alphabet: Vec<char> = "dcyxXDCsSYpPuiaIAoOwbe0^$hljk123 \u{1b}é日".chars().collect();
        let mut seed = 0x1234_5678_u64;
        for round in 0..300 {
            let text = ["", "a", "hello world", "one\ntwo\n\nthree", "  é日🙂 x", "\n\n"][round % 6];
            let (mut vim, mut editor) = rig(text, 0);
            for _ in 0..40 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let ch = alphabet[(seed >> 33) as usize % alphabet.len()];
                let key = if ch == '\u{1b}' { KeyCode::Esc } else { KeyCode::Char(ch) };
                vim.key(&mut editor, KeyEvent::new(key, KeyModifiers::NONE));
                assert!(editor.cursor() <= editor.text().len());
                assert!(editor.text().is_char_boundary(editor.cursor()), "{:?} {}", editor.text(), editor.cursor());
            }
        }
    }
}
