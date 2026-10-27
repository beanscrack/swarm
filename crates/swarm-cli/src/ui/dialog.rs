//! Modal choice dialogs: the permission prompt and the pickers.
//!
//! A dialog replaces the prompt box while it is open. It owns the keyboard
//! (nothing typed reaches the editor) and answers through a oneshot channel,
//! so the engine task that asked simply awaits the choice.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::oneshot;

use super::text::{pad_right, truncate, width};
use crate::style::{Color, role};

/// Keys typed this soon after a dialog opens are ignored: they were meant for
/// the prompt, and a stray `y` must not approve a command nobody has read.
const GRACE: Duration = Duration::from_millis(350);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accent {
    /// A permission prompt: violet frame.
    Permission,
    /// A neutral picker.
    Info,
}

/// What to ask.
#[derive(Debug, Clone)]
pub struct DialogSpec {
    pub title: String,
    /// Pre-styled rows shown under the title (the command, a diff). They are
    /// truncated to the box, never wrapped.
    pub body: Vec<String>,
    pub question: String,
    pub options: Vec<String>,
    /// Single keys that pick an option outright, e.g. `('y', 0)`.
    pub shortcuts: Vec<(char, usize)>,
    pub default: usize,
    pub hint: String,
    pub accent: Accent,
}

impl DialogSpec {
    pub fn pick(title: &str, options: Vec<String>) -> Self {
        Self {
            title: title.to_string(),
            body: Vec::new(),
            question: String::new(),
            options,
            shortcuts: Vec::new(),
            default: 0,
            hint: "\u{2191}/\u{2193} to move \u{00b7} enter to select \u{00b7} esc to cancel".to_string(),
            accent: Accent::Info,
        }
    }
}

/// An open dialog.
pub struct Dialog {
    pub spec: DialogSpec,
    cursor: usize,
    opened: Instant,
    reply: Option<oneshot::Sender<Option<usize>>>,
}

/// What a key did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKey {
    /// Still open.
    Open,
    /// An answer was sent (a choice, or `None` for cancel).
    Closed,
}

impl Dialog {
    pub fn open(spec: DialogSpec, reply: oneshot::Sender<Option<usize>>, now: Instant) -> Self {
        let cursor = spec.default.min(spec.options.len().saturating_sub(1));
        Self { spec, cursor, opened: now, reply: Some(reply) }
    }

    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn finish(&mut self, choice: Option<usize>) -> DialogKey {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(choice);
        }
        DialogKey::Closed
    }

    /// Dismiss without choosing (Esc, Ctrl+C, shutdown).
    pub fn cancel(&mut self) -> DialogKey {
        self.finish(None)
    }

    pub fn key(&mut self, key: KeyEvent, now: Instant) -> DialogKey {
        let count = self.spec.options.len();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let early = now.saturating_duration_since(self.opened) < GRACE;
        match key.code {
            KeyCode::Esc => self.cancel(),
            KeyCode::Char('c') | KeyCode::Char('d') if ctrl => self.cancel(),
            KeyCode::Up | KeyCode::BackTab => {
                if count > 0 {
                    self.cursor = (self.cursor + count - 1) % count;
                }
                DialogKey::Open
            }
            KeyCode::Down | KeyCode::Tab => {
                if count > 0 {
                    self.cursor = (self.cursor + 1) % count;
                }
                DialogKey::Open
            }
            KeyCode::Enter if !early && count > 0 => self.finish(Some(self.cursor)),
            KeyCode::Char(ch) if !ctrl && !early => {
                if let Some(digit) = ch.to_digit(10) {
                    let index = digit as usize;
                    if index >= 1 && index <= count {
                        return self.finish(Some(index - 1));
                    }
                }
                let lower = ch.to_ascii_lowercase();
                if let Some((_, index)) = self.spec.shortcuts.iter().find(|(key, _)| *key == lower) {
                    return self.finish(Some(*index));
                }
                match ch {
                    'k' if count > 0 => self.cursor = (self.cursor + count - 1) % count,
                    'j' if count > 0 => self.cursor = (self.cursor + 1) % count,
                    _ => {}
                }
                DialogKey::Open
            }
            _ => DialogKey::Open,
        }
    }

    /// The dialog as rows: a rounded box, then the hint line. At most
    /// `max_rows` tall; the body gives way first.
    pub fn render(&self, cols: usize, max_rows: usize, colors: Color) -> Vec<String> {
        let accent = match self.spec.accent {
            Accent::Permission => "perm",
            Accent::Info => "rule",
        };
        let title_role = match self.spec.accent {
            Accent::Permission => "permbold",
            Accent::Info => "select",
        };
        let total = cols.clamp(24, 110);
        let inner = total - 4;
        let edge = |text: &str| role(colors, accent, text);
        let row = |content: &str| {
            let fitted = pad_right(&truncate(content, inner), inner);
            format!("{} {fitted} {}", edge("\u{2502}"), edge("\u{2502}"))
        };

        let fixed = 2 // top and bottom borders
            + 1 // title
            + usize::from(!self.spec.question.is_empty()) * 2 // blank + question
            + self.spec.options.len()
            + 1; // hint
        let budget = max_rows.saturating_sub(fixed + 1).max(2);
        let mut body: Vec<String> = self.spec.body.clone();
        if body.len() > budget {
            let hidden = body.len() - (budget - 1);
            body.truncate(budget - 1);
            body.push(role(colors, "subtle", &format!("\u{2026} +{hidden} more lines")));
        }

        let mut out = Vec::new();
        out.push(edge(&format!("\u{256d}{}\u{256e}", "\u{2500}".repeat(total - 2))));
        out.push(row(&role(colors, title_role, &self.spec.title)));
        if !body.is_empty() {
            out.push(row(""));
            for line in &body {
                out.push(row(line));
            }
        }
        if !self.spec.question.is_empty() {
            out.push(row(""));
            out.push(row(&self.spec.question));
        }
        for (index, option) in self.spec.options.iter().enumerate() {
            let line = if index == self.cursor {
                role(colors, "select", &format!("\u{276f} {}. {option}", index + 1))
            } else {
                format!("  {}. {option}", index + 1)
            };
            out.push(row(&line));
        }
        out.push(edge(&format!("\u{2570}{}\u{256f}", "\u{2500}".repeat(total - 2))));
        out.push(role(colors, "subtle", &format!("  {}", truncate(&self.spec.hint, cols.saturating_sub(2)))));
        debug_assert!(out.iter().all(|line| width(line) <= cols));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn approval() -> DialogSpec {
        DialogSpec {
            title: "Bash command".into(),
            body: vec!["  rm -rf build".into(), "  deletes the build directory".into()],
            question: "Do you want to proceed?".into(),
            options: vec!["Yes".into(), "Yes, and don't ask again".into(), "No (esc)".into()],
            shortcuts: vec![('y', 0), ('a', 1), ('n', 2)],
            default: 0,
            hint: "1/2/3 or y/a/n \u{00b7} esc cancels".into(),
            accent: Accent::Permission,
        }
    }

    fn open(spec: DialogSpec) -> (Dialog, oneshot::Receiver<Option<usize>>, Instant) {
        let (tx, rx) = oneshot::channel();
        let start = Instant::now();
        (Dialog::open(spec, tx, start), rx, start + Duration::from_secs(1))
    }

    #[test]
    fn enter_confirms_the_highlighted_option() {
        let (mut dialog, mut rx, later) = open(approval());
        assert_eq!(dialog.key(key(KeyCode::Down), later), DialogKey::Open);
        assert_eq!(dialog.cursor(), 1);
        assert_eq!(dialog.key(key(KeyCode::Enter), later), DialogKey::Closed);
        assert_eq!(rx.try_recv().unwrap(), Some(1));
    }

    #[test]
    fn number_keys_and_shortcuts_pick_directly() {
        let (mut dialog, mut rx, later) = open(approval());
        dialog.key(key(KeyCode::Char('3')), later);
        assert_eq!(rx.try_recv().unwrap(), Some(2));
        let (mut dialog, mut rx, later) = open(approval());
        dialog.key(key(KeyCode::Char('A')), later);
        assert_eq!(rx.try_recv().unwrap(), Some(1), "shortcuts ignore case");
        let (mut dialog, mut rx, later) = open(approval());
        assert_eq!(dialog.key(key(KeyCode::Char('9')), later), DialogKey::Open, "no option 9");
        assert!(rx.try_recv().is_err());
        dialog.key(key(KeyCode::Char('y')), later);
        assert_eq!(rx.try_recv().unwrap(), Some(0));
    }

    #[test]
    fn escape_and_ctrl_c_cancel() {
        let (mut dialog, mut rx, later) = open(approval());
        assert_eq!(dialog.key(key(KeyCode::Esc), later), DialogKey::Closed);
        assert_eq!(rx.try_recv().unwrap(), None);
        let (mut dialog, mut rx, later) = open(approval());
        dialog.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), later);
        assert_eq!(rx.try_recv().unwrap(), None);
    }

    #[test]
    fn the_cursor_wraps_both_ways() {
        let (mut dialog, _rx, later) = open(approval());
        dialog.key(key(KeyCode::Up), later);
        assert_eq!(dialog.cursor(), 2);
        dialog.key(key(KeyCode::Down), later);
        assert_eq!(dialog.cursor(), 0);
        dialog.key(key(KeyCode::Char('j')), later);
        dialog.key(key(KeyCode::Char('k')), later);
        assert_eq!(dialog.cursor(), 0);
    }

    /// A key typed while the user was still composing a message must not
    /// answer a prompt they have not seen yet.
    #[test]
    fn keys_in_the_first_moments_are_ignored() {
        let (tx, mut rx) = oneshot::channel();
        let start = Instant::now();
        let mut dialog = Dialog::open(approval(), tx, start);
        let too_soon = start + Duration::from_millis(50);
        assert_eq!(dialog.key(key(KeyCode::Char('y')), too_soon), DialogKey::Open);
        assert_eq!(dialog.key(key(KeyCode::Enter), too_soon), DialogKey::Open);
        assert_eq!(dialog.key(key(KeyCode::Char('1')), too_soon), DialogKey::Open);
        assert!(rx.try_recv().is_err());
        assert_eq!(dialog.key(key(KeyCode::Esc), too_soon), DialogKey::Closed, "escape is always honoured");
    }

    #[test]
    fn a_dropped_receiver_does_not_panic() {
        let (mut dialog, rx, later) = open(approval());
        drop(rx);
        assert_eq!(dialog.key(key(KeyCode::Char('y')), later), DialogKey::Closed);
    }

    #[test]
    fn the_box_has_even_borders_and_numbered_options() {
        let (dialog, _rx, _) = open(approval());
        let rows = dialog.render(60, 30, Color::Plain);
        assert!(rows[0].starts_with('\u{256d}') && rows[0].ends_with('\u{256e}'));
        let box_rows = &rows[..rows.len() - 1];
        assert!(box_rows.last().unwrap().starts_with('\u{2570}'));
        let widths: Vec<usize> = box_rows.iter().map(|row| width(row)).collect();
        assert!(widths.windows(2).all(|pair| pair[0] == pair[1]), "ragged box: {widths:?}");
        let text = rows.join("\n");
        assert!(text.contains("\u{276f} 1. Yes"), "{text}");
        assert!(text.contains("  2. Yes, and don't ask again"), "{text}");
        assert!(text.contains("  3. No (esc)"), "{text}");
        assert!(text.contains("rm -rf build"), "{text}");
        assert!(rows.last().unwrap().contains("esc cancels"));
    }

    #[test]
    fn a_tall_body_collapses_to_fit_the_screen() {
        let mut spec = approval();
        spec.body = (0..80).map(|n| format!("  line {n}")).collect();
        let (dialog, _rx, _) = open(spec);
        let rows = dialog.render(60, 16, Color::Plain);
        assert!(rows.len() <= 16, "{} rows", rows.len());
        let text = rows.join("\n");
        assert!(text.contains("more lines"), "{text}");
        assert!(text.contains("3. No (esc)"), "options must survive: {text}");
    }

    #[test]
    fn rows_never_exceed_the_terminal_width() {
        let mut spec = approval();
        spec.body = vec!["x".repeat(400), "\u{4f60}\u{597d}".repeat(100)];
        let (dialog, _rx, _) = open(spec);
        for cols in [24, 40, 80, 200] {
            for color in [Color::Plain, Color::Ansi] {
                for row in dialog.render(cols, 30, color) {
                    assert!(width(&row) <= cols, "{cols}: {}", width(&row));
                }
            }
        }
    }

    #[test]
    fn a_picker_has_no_question_row() {
        let spec = DialogSpec::pick("Resume which run?", vec!["one".into(), "two".into()]);
        let (dialog, _rx, _) = open(spec);
        let text = dialog.render(50, 20, Color::Plain).join("\n");
        assert!(text.contains("Resume which run?"));
        assert!(!text.contains("proceed"));
        assert!(text.contains("\u{276f} 1. one"));
    }
}
