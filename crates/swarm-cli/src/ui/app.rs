//! The UI state machine: keys and messages in, effects and a [`Frame`] out.
//!
//! Nothing here touches a terminal, which is what makes the keyboard handling
//! testable: feed it key events, read back what it wants printed, which
//! actions it raised, and what the live region looks like.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::oneshot;

use super::dialog::{Dialog, DialogKey, DialogSpec};
use super::editor::Editor;
use super::files::FileIndex;
use super::history::History;
use super::menu::{Menu, MenuItem, MenuKind};
use super::registry;
use super::screen::Frame;
use super::text::{pad_right, spread, truncate, width};
use crate::style::{self, Color, SPINNER, SPINNER_MS, role};

/// How long a "press again" prompt stays armed.
const ARM_FOR: Duration = Duration::from_secs(2);
/// How long a transient footer notice stays.
const NOTICE_FOR: Duration = Duration::from_secs(4);
const PROMPT: &str = "\u{276f} ";

/// What the UI asks the session loop to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A goal or command. The UI has already echoed it.
    Submit(String),
    /// Stop the running turn (Esc / Ctrl+C while busy).
    Interrupt,
    /// Leave the session (Ctrl+C or Ctrl+D pressed twice at an idle prompt).
    Quit,
    /// Shift+Tab: next permission mode.
    CycleMode,
    /// Ctrl+O: transcript detail on/off.
    ToggleVerbose,
}

/// Permission mode, as the footer shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Default,
    AskAll,
    Auto,
    Plan,
}

impl Mode {
    pub fn next(self) -> Self {
        match self {
            Mode::Default => Mode::AskAll,
            Mode::AskAll => Mode::Auto,
            Mode::Auto => Mode::Plan,
            Mode::Plan => Mode::Default,
        }
    }
}

/// What Enter does while a turn is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BusyMode {
    /// Run it after the current turn.
    #[default]
    Queue,
    /// Stop the current turn and run it now.
    Interrupt,
}

/// Session facts the footer shows. The session loop owns them and pushes a new
/// copy whenever one changes.
#[derive(Debug, Clone, Default)]
pub struct Info {
    pub provider: String,
    pub model: String,
    pub agents: u32,
    pub mode: Mode,
    /// Session token total.
    pub tokens: u64,
    /// One-line goal status, e.g. `\u{2299} goal 3/20`.
    pub goal: Option<String>,
}

/// Live status of the running turn.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub activity: String,
    pub tokens: u64,
    /// Finished and total tasks, when the run has a plan.
    pub tasks: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueOp {
    List,
    Remove(usize),
    Clear,
}

/// Messages from the rest of the program.
pub enum Msg {
    Print(Vec<String>),
    Info(Info),
    Busy(bool),
    Status(Status),
    Dialog(DialogSpec, oneshot::Sender<Option<usize>>),
    /// Put text in the prompt (for `/undo`).
    Prefill(String),
    /// Wipe the visible screen (the session was cleared).
    ClearScreen,
    /// Open this file in `$EDITOR`.
    EditFile(PathBuf),
    Shutdown,
}

/// What the shell must do after the app handled something.
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    Action(Action),
    /// Commit these rows to the scrollback.
    Print(Vec<String>),
    ClearScreen,
    Suspend,
    /// Edit this text in `$EDITOR`, then hand it back.
    EditExternal(String),
    /// Open a file in `$EDITOR`.
    EditFile(PathBuf),
    Quit,
}

/// A submitted line as the transcript shows it: a blank row, then the text on a
/// shaded band, as Claude Code echoes your message.
pub fn echo_rows(colors: Color, cols: usize, text: &str) -> Vec<String> {
    let cols = cols.max(8);
    let mut out = vec![String::new()];
    for (index, line) in text.lines().enumerate() {
        let lead = if index == 0 { PROMPT } else { "  " };
        let wrapped = super::text::wrap_hang(line, cols.saturating_sub(1), lead, "  ");
        for row in wrapped {
            let padded = pad_right(&row, cols.saturating_sub(1));
            out.push(if index == 0 && row.starts_with(PROMPT) {
                format!("{}{}", role(colors, "brand", PROMPT), role(colors, "echo", &padded[PROMPT.len()..]))
            } else {
                role(colors, "echo", &padded)
            });
        }
    }
    out
}

pub struct Options {
    /// Kept equal to the number of queued messages, for readers outside the UI.
    pub queue_len: Arc<AtomicUsize>,
    pub colors: Color,
    pub history: History,
    pub workspace: PathBuf,
    /// False for `swarm run`: no prompt, only status and dialogs.
    pub input: bool,
}

struct Busy {
    started: Instant,
    verb: &'static str,
    status: Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    CtrlC,
    CtrlD,
    Esc,
}

struct Search {
    query: String,
    /// Index and text of the current match.
    hit: Option<(usize, String)>,
}

pub struct App {
    queue_len: Arc<AtomicUsize>,
    colors: Color,
    input: bool,
    pub info: Info,
    editor: Editor,
    history: History,
    files: FileIndex,
    menu: Option<Menu>,
    /// The buffer the user dismissed the menu for with Esc.
    dismissed_for: Option<String>,
    search: Option<Search>,
    busy: Option<Busy>,
    queued: VecDeque<String>,
    busy_mode: BusyMode,
    dialog: Option<Dialog>,
    waiting_dialogs: VecDeque<(DialogSpec, oneshot::Sender<Option<usize>>)>,
    armed: Option<(Arm, Instant)>,
    notice: Option<(String, Instant)>,
    show_help: bool,
    cols: usize,
    rows: usize,
    placeholder: usize,
}

const PLACEHOLDERS: &[&str] = &[
    "Try \"fix the failing test\"",
    "Try \"explain how this project is laid out\"",
    "Try \"add input validation to the signup form\"",
    "Try \"write tests for the parser\"",
    "Try \"find and fix the bug in main.rs\"",
];

impl App {
    pub fn new(options: Options) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.subsec_nanos() as usize)
            .unwrap_or(0);
        Self {
            queue_len: options.queue_len,
            colors: options.colors,
            input: options.input,
            info: Info::default(),
            editor: Editor::new(),
            history: options.history,
            files: FileIndex::new(options.workspace),
            menu: None,
            dismissed_for: None,
            search: None,
            busy: None,
            queued: VecDeque::new(),
            busy_mode: BusyMode::Queue,
            dialog: None,
            waiting_dialogs: VecDeque::new(),
            armed: None,
            notice: None,
            show_help: false,
            cols: 80,
            rows: 24,
            placeholder: seed % PLACEHOLDERS.len(),
        }
    }

    pub fn set_size(&mut self, cols: usize, rows: usize) {
        self.cols = cols.max(1);
        self.rows = rows.max(2);
    }

    #[cfg(test)]
    pub fn editor_text(&self) -> &str {
        self.editor.text()
    }

    #[cfg(test)]
    pub fn queued(&self) -> Vec<String> {
        self.queued.iter().cloned().collect()
    }

    fn paint(&self, name: &str, text: &str) -> String {
        role(self.colors, name, text)
    }

    // ---- messages ----------------------------------------------------------

    pub fn on_msg(&mut self, msg: Msg, now: Instant) -> Vec<Effect> {
        let effects = self.handle_msg(msg, now);
        self.queue_len.store(self.queued.len(), Ordering::SeqCst);
        effects
    }

    fn handle_msg(&mut self, msg: Msg, now: Instant) -> Vec<Effect> {
        match msg {
            Msg::Print(lines) => vec![Effect::Print(lines)],
            Msg::Info(info) => {
                self.info = info;
                Vec::new()
            }
            Msg::Busy(true) => {
                let seed = now.elapsed().subsec_nanos() as u64
                    ^ std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|since| since.subsec_nanos() as u64)
                        .unwrap_or(0);
                self.busy = Some(Busy {
                    started: now,
                    verb: style::verb(seed),
                    status: Status { activity: "starting".into(), ..Status::default() },
                });
                Vec::new()
            }
            Msg::Busy(false) => {
                self.busy = None;
                self.release_next()
            }
            Msg::Status(status) => {
                if let Some(busy) = &mut self.busy {
                    busy.status = status;
                }
                Vec::new()
            }
            Msg::Dialog(spec, reply) => {
                if self.dialog.is_some() {
                    self.waiting_dialogs.push_back((spec, reply));
                } else {
                    self.dialog = Some(Dialog::open(spec, reply, now));
                }
                Vec::new()
            }
            Msg::Prefill(text) => {
                self.editor.set(&text);
                self.refresh_menu();
                Vec::new()
            }
            Msg::ClearScreen => vec![Effect::ClearScreen],
            Msg::EditFile(path) => vec![Effect::EditFile(path)],
            Msg::Shutdown => {
                if let Some(dialog) = &mut self.dialog {
                    dialog.cancel();
                }
                self.dialog = None;
                for (_, reply) in self.waiting_dialogs.drain(..) {
                    let _ = reply.send(None);
                }
                vec![Effect::Quit]
            }
        }
    }

    /// The turn ended: run the next queued goal, if any.
    fn release_next(&mut self) -> Vec<Effect> {
        match self.queued.pop_front() {
            Some(text) => vec![Effect::Print(self.echo(&text)), Effect::Action(Action::Submit(text))],
            None => Vec::new(),
        }
    }

    fn queue_op(&mut self, op: QueueOp) -> Vec<Effect> {
        let say = |app: &App, lines: Vec<String>| {
            vec![Effect::Print(lines.into_iter().map(|line| app.paint("subtle", &format!("  {line}"))).collect())]
        };
        match op {
            QueueOp::List if self.queued.is_empty() => say(self, vec!["nothing queued".into()]),
            QueueOp::List => {
                let lines = self
                    .queued
                    .iter()
                    .enumerate()
                    .map(|(index, text)| format!("{}. {}", index + 1, truncate(&text.replace('\n', " "), 80)))
                    .collect();
                say(self, lines)
            }
            QueueOp::Remove(n) if n >= 1 && n <= self.queued.len() => {
                let gone = self.queued.remove(n - 1).unwrap_or_default();
                say(self, vec![format!("removed: {}", truncate(&gone.replace('\n', " "), 60))])
            }
            QueueOp::Remove(_) => say(self, vec!["no such queue entry".into()]),
            QueueOp::Clear => {
                let count = self.queued.len();
                self.queued.clear();
                say(self, vec![format!("cleared {count} queued")])
            }
        }
    }

    // ---- time --------------------------------------------------------------

    /// Expire timed state. Returns true when the picture may have changed.
    pub fn on_tick(&mut self, now: Instant) -> bool {
        let mut changed = self.busy.is_some() && self.dialog.is_none();
        if self.armed.is_some_and(|(_, until)| now >= until) {
            self.armed = None;
            changed = true;
        }
        if self.notice.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.notice = None;
            changed = true;
        }
        changed
    }

    // ---- keys --------------------------------------------------------------

    pub fn on_paste(&mut self, text: &str) {
        if self.dialog.is_some() || !self.input || self.search.is_some() {
            return;
        }
        self.editor.paste(text);
        self.show_help = false;
        self.refresh_menu();
    }

    pub fn on_key(&mut self, key: KeyEvent, now: Instant) -> Vec<Effect> {
        let effects = self.handle_key(key, now);
        self.queue_len.store(self.queued.len(), Ordering::SeqCst);
        effects
    }

    fn handle_key(&mut self, key: KeyEvent, now: Instant) -> Vec<Effect> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        if let Some(dialog) = &mut self.dialog {
            if dialog.key(key, now) == DialogKey::Closed {
                self.dialog = None;
                if let Some((spec, reply)) = self.waiting_dialogs.pop_front() {
                    self.dialog = Some(Dialog::open(spec, reply, now));
                }
            }
            return Vec::new();
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if !self.input {
            return if matches!(key.code, KeyCode::Esc) || (ctrl && key.code == KeyCode::Char('c')) {
                vec![Effect::Action(Action::Interrupt)]
            } else {
                Vec::new()
            };
        }
        if self.search.is_some() {
            return self.search_key(key);
        }
        let was_help = std::mem::take(&mut self.show_help);
        let effects = self.edit_key(key, now, was_help);
        // Anything but the key that asked for them cancels the "press again" prompts.
        if !matches!(key.code, KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('d')) {
            self.armed = None;
        }
        effects
    }

    fn edit_key(&mut self, key: KeyEvent, now: Instant, was_help: bool) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let edit_cols = self.cols.saturating_sub(PROMPT.chars().count()).max(2);
        match key.code {
            // --- leaving, interrupting ---
            KeyCode::Char('c') if ctrl => {
                if self.busy.is_some() {
                    return vec![Effect::Action(Action::Interrupt)];
                }
                if !self.editor.is_empty() {
                    self.editor.clear();
                    self.menu = None;
                    return Vec::new();
                }
                self.confirm_exit(Arm::CtrlC, "Ctrl-C", now)
            }
            KeyCode::Char('d') if ctrl => {
                if self.editor.is_empty() {
                    self.confirm_exit(Arm::CtrlD, "Ctrl-D", now)
                } else {
                    self.editor.delete();
                    self.refresh_menu();
                    Vec::new()
                }
            }
            KeyCode::Esc => {
                if self.menu.is_some() {
                    self.dismissed_for = Some(self.editor.text().to_string());
                    self.menu = None;
                    return Vec::new();
                }
                if was_help {
                    return Vec::new();
                }
                if self.busy.is_some() {
                    return vec![Effect::Action(Action::Interrupt)];
                }
                if !self.editor.is_empty() {
                    if self.is_armed(Arm::Esc, now) {
                        // The draft is not lost: it goes to history first.
                        let text = self.editor.expanded();
                        self.history.keep_draft(&text);
                        self.editor.clear();
                        self.armed = None;
                    } else {
                        self.armed = Some((Arm::Esc, now + ARM_FOR));
                    }
                }
                Vec::new()
            }
            KeyCode::Char('z') if ctrl => vec![Effect::Suspend],
            KeyCode::Char('l') if ctrl => vec![Effect::ClearScreen],
            KeyCode::Char('o') if ctrl => vec![Effect::Action(Action::ToggleVerbose)],
            KeyCode::Char('g') if ctrl => vec![Effect::EditExternal(self.editor.expanded())],
            KeyCode::BackTab => vec![Effect::Action(Action::CycleMode)],
            KeyCode::Char('r') if ctrl => {
                if !self.history.is_empty() {
                    let hit = self.history.search("", None).map(|(i, t)| (i, t.to_string()));
                    self.search = Some(Search { query: String::new(), hit });
                }
                Vec::new()
            }

            // --- submitting ---
            KeyCode::Enter if shift || alt => {
                self.editor.newline();
                self.refresh_menu();
                Vec::new()
            }
            KeyCode::Char('j') if ctrl => {
                self.editor.newline();
                self.refresh_menu();
                Vec::new()
            }
            KeyCode::Enter => {
                if let Some(effects) = self.accept_menu(true) {
                    return effects;
                }
                // `\` then Enter is a newline, as in Claude Code.
                if self.editor.text().ends_with('\\') && self.editor.cursor() == self.editor.text().len() {
                    self.editor.backspace();
                    self.editor.newline();
                    return Vec::new();
                }
                self.submit(now)
            }
            KeyCode::Tab => {
                if let Some(effects) = self.accept_menu(false) {
                    return effects;
                }
                // Hermes: Tab queues what you typed while a turn is running.
                if self.busy.is_some() && !self.editor.is_empty() {
                    return self.queue_current();
                }
                Vec::new()
            }

            // --- history and menu navigation ---
            KeyCode::Up => self.up(edit_cols),
            KeyCode::Down => self.down(edit_cols),
            KeyCode::Char('p') if ctrl => self.up(edit_cols),
            KeyCode::Char('n') if ctrl => self.down(edit_cols),

            // --- cursor movement ---
            KeyCode::Left if ctrl || alt => {
                self.editor.word_left();
                Vec::new()
            }
            KeyCode::Right if ctrl || alt => {
                self.editor.word_right();
                Vec::new()
            }
            KeyCode::Char('b') if alt => {
                self.editor.word_left();
                Vec::new()
            }
            KeyCode::Char('f') if alt => {
                self.editor.word_right();
                Vec::new()
            }
            KeyCode::Left | KeyCode::Char('b') if matches!(key.code, KeyCode::Left) || ctrl => {
                self.editor.left();
                Vec::new()
            }
            KeyCode::Right | KeyCode::Char('f') if matches!(key.code, KeyCode::Right) || ctrl => {
                self.editor.right();
                Vec::new()
            }
            KeyCode::Home => {
                self.editor.line_home();
                Vec::new()
            }
            KeyCode::Char('a') if ctrl => {
                self.editor.line_home();
                Vec::new()
            }
            KeyCode::End => {
                self.editor.line_end_key();
                Vec::new()
            }
            KeyCode::Char('e') if ctrl => {
                self.editor.line_end_key();
                Vec::new()
            }

            // --- deleting ---
            KeyCode::Backspace if alt => {
                self.editor.kill_word_back();
                self.refresh_menu();
                Vec::new()
            }
            KeyCode::Backspace | KeyCode::Char('h') if matches!(key.code, KeyCode::Backspace) || ctrl => {
                self.editor.backspace();
                self.refresh_menu();
                Vec::new()
            }
            KeyCode::Delete => {
                self.editor.delete();
                self.refresh_menu();
                Vec::new()
            }
            KeyCode::Char('w') if ctrl => {
                self.editor.kill_word_back();
                self.refresh_menu();
                Vec::new()
            }
            KeyCode::Char('d') if alt => {
                self.editor.kill_word_forward();
                self.refresh_menu();
                Vec::new()
            }
            KeyCode::Char('k') if ctrl => {
                self.editor.kill_to_end();
                self.refresh_menu();
                Vec::new()
            }
            KeyCode::Char('u') if ctrl => {
                self.editor.kill_to_start();
                self.refresh_menu();
                Vec::new()
            }
            KeyCode::Char('y') if ctrl => {
                self.editor.yank();
                self.refresh_menu();
                Vec::new()
            }
            KeyCode::Char('_') | KeyCode::Char('/') if ctrl => {
                self.editor.undo();
                self.refresh_menu();
                Vec::new()
            }

            // --- typing ---
            KeyCode::Char('?') if self.editor.is_empty() && !ctrl && !alt => {
                self.show_help = !was_help;
                Vec::new()
            }
            KeyCode::Char(ch) if !ctrl && !alt => {
                self.editor.insert_char(ch);
                self.refresh_menu();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn is_armed(&self, what: Arm, now: Instant) -> bool {
        self.armed.is_some_and(|(arm, until)| arm == what && now < until)
    }

    /// Ctrl+C / Ctrl+D at an idle, empty prompt: the first press asks, the
    /// second (within two seconds) quits.
    fn confirm_exit(&mut self, what: Arm, label: &str, now: Instant) -> Vec<Effect> {
        if self.is_armed(what, now) {
            self.armed = None;
            return vec![Effect::Action(Action::Quit)];
        }
        self.armed = Some((what, now + ARM_FOR));
        self.notice = Some((format!("Press {label} again to exit"), now + ARM_FOR));
        Vec::new()
    }

    fn up(&mut self, edit_cols: usize) -> Vec<Effect> {
        if let Some(menu) = &mut self.menu {
            menu.step(false);
            return Vec::new();
        }
        // While a turn runs, Up takes back the newest queued message to edit it.
        if self.busy.is_some() && self.editor.is_empty() {
            if let Some(text) = self.queued.pop_back() {
                self.editor.set(&text);
                return Vec::new();
            }
        }
        if self.editor.move_vertical(false, edit_cols) {
            return Vec::new();
        }
        let current = self.editor.expanded();
        if let Some(text) = self.history.prev(&current).map(str::to_string) {
            self.editor.set(&text);
            self.refresh_menu();
        }
        Vec::new()
    }

    fn down(&mut self, edit_cols: usize) -> Vec<Effect> {
        if let Some(menu) = &mut self.menu {
            menu.step(true);
            return Vec::new();
        }
        if self.editor.move_vertical(true, edit_cols) {
            return Vec::new();
        }
        if let Some(text) = self.history.next() {
            self.editor.set(&text);
            self.refresh_menu();
        }
        Vec::new()
    }

    fn queue_current(&mut self) -> Vec<Effect> {
        let text = self.editor.expanded().trim().to_string();
        self.history.push(&text);
        self.editor.reset();
        self.menu = None;
        if text.is_empty() {
            return Vec::new();
        }
        self.queued.push_back(text);
        Vec::new()
    }

    fn submit(&mut self, now: Instant) -> Vec<Effect> {
        let text = self.editor.expanded().trim().to_string();
        if text.is_empty() {
            return Vec::new();
        }
        self.history.push(&text);
        self.editor.reset();
        self.menu = None;
        self.dismissed_for = None;
        if let Some(effects) = self.local_command(&text, now) {
            return effects;
        }
        if self.busy.is_some() {
            self.queued.push_back(text);
            return if self.busy_mode == BusyMode::Interrupt {
                vec![Effect::Action(Action::Interrupt)]
            } else {
                Vec::new()
            };
        }
        vec![Effect::Print(self.echo(&text)), Effect::Action(Action::Submit(text))]
    }

    /// `/queue` and `/busy` are about the UI's own state, so they work even
    /// while a turn is running.
    fn local_command(&mut self, text: &str, now: Instant) -> Option<Vec<Effect>> {
        let (word, args) = registry::split(text)?;
        let word = match registry::resolve(word) {
            registry::Resolved::Exact(spec) | registry::Resolved::Unique(spec) => spec.name,
            _ => return None,
        };
        match word {
            "queue" => {
                let mut parts = args.split_whitespace();
                let op = match (parts.next(), parts.next()) {
                    (None, _) | (Some("list"), _) | (Some("ls"), _) => QueueOp::List,
                    (Some("clear"), _) => QueueOp::Clear,
                    (Some("rm"), Some(n)) | (Some("remove"), Some(n)) => {
                        QueueOp::Remove(n.parse().unwrap_or(0))
                    }
                    _ => {
                        self.notice = Some(("usage: /queue [list|rm <n>|clear]".into(), now + NOTICE_FOR));
                        return Some(Vec::new());
                    }
                };
                Some(self.queue_op(op))
            }
            "busy" => {
                match args {
                    "queue" => self.busy_mode = BusyMode::Queue,
                    "interrupt" => self.busy_mode = BusyMode::Interrupt,
                    "" | "status" => {}
                    _ => {
                        self.notice = Some(("usage: /busy [queue|interrupt]".into(), now + NOTICE_FOR));
                        return Some(Vec::new());
                    }
                }
                let line = match self.busy_mode {
                    BusyMode::Queue => "while a turn runs, enter queues your message for after it",
                    BusyMode::Interrupt => "while a turn runs, enter stops it and sends your message now",
                };
                Some(vec![Effect::Print(vec![self.paint("subtle", &format!("  {line}"))])])
            }
            _ => None,
        }
    }

    // ---- echo --------------------------------------------------------------

    /// How a submitted line appears in the transcript.
    pub fn echo(&self, text: &str) -> Vec<String> {
        echo_rows(self.colors, self.cols, text)
    }

    // ---- menus -------------------------------------------------------------

    /// Recompute the suggestion menu for the current buffer and cursor.
    fn refresh_menu(&mut self) {
        let text = self.editor.text().to_string();
        if self.dismissed_for.as_deref() == Some(text.as_str()) {
            self.menu = None;
            return;
        }
        self.dismissed_for = None;
        let cursor = self.editor.cursor();
        let slash_word = text
            .strip_prefix('/')
            .filter(|rest| cursor >= 1 && !text[..cursor].contains(char::is_whitespace) && !rest.contains(char::is_whitespace))
            .filter(|rest| !rest.contains('/'));
        if let Some(word) = slash_word {
            let query = format!("/{word}");
            let items: Vec<MenuItem> = registry::matching(word)
                .into_iter()
                .map(|spec| MenuItem {
                    label: format!("/{}", spec.name),
                    detail: spec.desc.to_string(),
                    insert: if spec.args.is_empty() {
                        format!("/{}", spec.name)
                    } else {
                        format!("/{} ", spec.name)
                    },
                    required: spec.args.starts_with('<'),
                })
                .collect();
            self.set_menu(MenuKind::Slash, items, query, 0);
            return;
        }
        let (start, token) = self.editor.word_at_cursor();
        if let Some(query) = token.strip_prefix('@') {
            let query = query.to_string();
            let items: Vec<MenuItem> = self
                .files
                .search(&query, 8)
                .into_iter()
                .map(|path| MenuItem {
                    label: path.clone(),
                    detail: String::new(),
                    insert: if path.ends_with('/') { format!("@{path}") } else { format!("@{path} ") },
                    required: false,
                })
                .collect();
            self.set_menu(MenuKind::Mention, items, format!("@{query}"), start);
            return;
        }
        self.menu = None;
    }

    fn set_menu(&mut self, kind: MenuKind, items: Vec<MenuItem>, query: String, start: usize) {
        if items.is_empty() {
            self.menu = None;
            return;
        }
        let keep = self.menu.as_ref().filter(|menu| menu.kind == kind && menu.query == query).map(|menu| menu.sel);
        let mut menu = Menu::new(kind, items, query, start);
        if let Some(sel) = keep {
            menu.sel = sel.min(menu.items.len() - 1);
        }
        self.menu = Some(menu);
    }

    /// Accept the highlighted suggestion. `submit` is true for Enter: a slash
    /// command that takes no arguments then runs straight away. Returns `None`
    /// when no menu was open (the key falls through to its normal meaning).
    fn accept_menu(&mut self, submit: bool) -> Option<Vec<Effect>> {
        let menu = self.menu.take()?;
        let item = menu.selected()?.clone();
        match menu.kind {
            MenuKind::Slash => {
                let typed = self.editor.text().trim().to_string();
                let name = item.insert.trim_end().to_string();
                if !submit {
                    // Tab completes and leaves room for arguments.
                    self.editor.set(&item.insert);
                    self.dismissed_for = Some(self.editor.text().to_string());
                    return Some(Vec::new());
                }
                if typed == name {
                    // Already the whole command word: Enter runs it as typed.
                    self.menu = None;
                    return None;
                }
                if item.required {
                    // `/plan` needs a task: complete it and wait.
                    self.editor.set(&item.insert);
                    self.dismissed_for = Some(self.editor.text().to_string());
                    return Some(Vec::new());
                }
                // A partial command whose arguments are optional: complete and run.
                self.editor.set(&name);
                self.menu = None;
                None
            }
            MenuKind::Mention => {
                self.editor.replace_before_cursor(menu.token_start, &item.insert);
                if item.insert.ends_with('/') {
                    self.refresh_menu();
                } else {
                    self.dismissed_for = Some(self.editor.text().to_string());
                }
                Some(Vec::new())
            }
        }
    }

    // ---- reverse search ----------------------------------------------------

    fn search_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(search) = &mut self.search else { return Vec::new() };
        match key.code {
            KeyCode::Esc => self.search = None,
            KeyCode::Char('c') | KeyCode::Char('g') if ctrl => self.search = None,
            KeyCode::Enter | KeyCode::Tab | KeyCode::Right => {
                if let Some((_, text)) = search.hit.take() {
                    self.editor.set(&text);
                }
                self.search = None;
                self.refresh_menu();
            }
            KeyCode::Char('r') if ctrl => {
                let before = search.hit.as_ref().map(|(index, _)| *index);
                if let Some((index, text)) = self.history.search(&search.query, before) {
                    search.hit = Some((index, text.to_string()));
                }
            }
            KeyCode::Backspace => {
                search.query.pop();
                search.hit = self.history.search(&search.query, None).map(|(i, t)| (i, t.to_string()));
            }
            KeyCode::Char(ch) if !ctrl => {
                search.query.push(ch);
                search.hit = self.history.search(&search.query, None).map(|(i, t)| (i, t.to_string()));
            }
            _ => {}
        }
        Vec::new()
    }

    // ---- view --------------------------------------------------------------

    pub fn view(&self, cols: usize, rows: usize, now: Instant) -> Frame {
        let cols = cols.max(1);
        let mut lines: Vec<String> = Vec::new();
        let mut cursor: Option<(usize, usize)> = None;

        if let Some(dialog) = &self.dialog {
            lines.push(String::new());
            lines.extend(dialog.render(cols, rows.saturating_sub(2).max(8), self.colors));
            return Frame { lines, cursor: None };
        }

        lines.push(String::new());
        if let Some(busy) = &self.busy {
            lines.push(self.spinner_row(busy, cols, now));
            for text in self.queued.iter().take(3) {
                let flat = text.replace('\n', " ");
                lines.push(self.paint("subtle", &truncate(&format!("  {PROMPT}{flat}  (queued)"), cols)));
            }
            if self.queued.len() > 3 {
                lines.push(self.paint("subtle", &format!("  +{} more queued", self.queued.len() - 3)));
            }
            lines.push(String::new());
        }

        if self.input {
            let rule = self.paint("rule", &"\u{2500}".repeat(cols));
            lines.push(rule.clone());
            let prompt_w = PROMPT.chars().count();
            let layout = self.editor.layout(cols.saturating_sub(prompt_w).max(2));
            let (mark, mark_role) = match self.editor.text().chars().next() {
                Some('!') => ("! ", "warn"),
                Some('#') => ("# ", "suggest"),
                _ => (PROMPT, "brand"),
            };
            let first = lines.len();
            if self.editor.is_empty() {
                let hint = if self.busy.is_some() {
                    "type to queue a message \u{00b7} esc to interrupt".to_string()
                } else {
                    PLACEHOLDERS[self.placeholder].to_string()
                };
                lines.push(format!(
                    "{}{}",
                    self.paint(mark_role, mark),
                    self.paint("subtle", &truncate(&hint, cols.saturating_sub(prompt_w)))
                ));
            } else {
                // In bash and note mode the marker stands in for the `!` / `#`.
                let skip = usize::from(mark != PROMPT);
                for (index, row) in layout.rows.iter().enumerate() {
                    let lead = if index == 0 { self.paint(mark_role, mark) } else { " ".repeat(prompt_w) };
                    let text = self.editor.row_text(row);
                    let text = if index == 0 { text.get(skip..).unwrap_or("") } else { text };
                    lines.push(format!("{lead}{}", truncate(text, cols.saturating_sub(prompt_w))));
                }
            }
            lines.push(rule);
            let shift = if layout.cursor.0 == 0 { usize::from(mark != PROMPT) } else { 0 };
            cursor = Some((first + layout.cursor.0, (prompt_w + layout.cursor.1.saturating_sub(shift)).min(cols - 1)));
            let tail = self.tail(cols);
            lines.extend(tail);
            if self.search.is_some() {
                cursor = None;
            }
        } else {
            lines.push(self.footer(cols));
        }

        // Never taller than the screen: keep the bottom (the prompt) in view.
        let max = rows.saturating_sub(1).max(1);
        if lines.len() > max {
            let drop = lines.len() - max;
            lines.drain(..drop);
            cursor = cursor.and_then(|(row, col)| row.checked_sub(drop).map(|row| (row, col)));
        }
        Frame { lines, cursor }
    }

    /// Rows under the prompt: search, menu, shortcut sheet or the footer.
    fn tail(&self, cols: usize) -> Vec<String> {
        if let Some(search) = &self.search {
            let hit = search.hit.as_ref().map(|(_, text)| text.replace('\n', " ")).unwrap_or_default();
            let head = format!("  reverse search: {}", search.query);
            let room = cols.saturating_sub(width(&head) + 6);
            let shown = if hit.is_empty() && !search.query.is_empty() {
                "no match".to_string()
            } else {
                truncate(&hit, room)
            };
            return vec![
                self.paint("select", &truncate(&head, cols)),
                self.paint("subtle", &truncate(&format!("    \u{21b3} {shown}  \u{00b7} enter accepts \u{00b7} ctrl+r older \u{00b7} esc cancels"), cols)),
            ];
        }
        if let Some(menu) = &self.menu {
            return menu.render(cols, self.colors);
        }
        if self.show_help {
            return self.shortcut_sheet(cols);
        }
        vec![self.footer(cols)]
    }

    fn shortcut_sheet(&self, cols: usize) -> Vec<String> {
        let rows = [
            ("! for bash mode", "esc esc to clear input", "ctrl+r search history"),
            ("/ for commands", "shift+tab cycles approvals", "ctrl+o transcript detail"),
            ("@ for file paths", "\\ then enter for a newline", "ctrl+g edit in $EDITOR"),
            ("# to save a note", "tab queues while running", "ctrl+l clear the screen"),
        ];
        let col_w = (cols.saturating_sub(4) / 3).clamp(18, 34);
        rows.iter()
            .map(|(a, b, c)| {
                let line = format!("  {}{}{}", pad_right(a, col_w), pad_right(b, col_w), c);
                self.paint("subtle", &truncate(&line, cols))
            })
            .collect()
    }

    fn footer(&self, cols: usize) -> String {
        let left = if let Some((text, _)) = &self.notice {
            self.paint("warn", &format!("  {text}"))
        } else if let Some((Arm::Esc, _)) = self.armed {
            self.paint("subtle", "  Esc again to clear")
        } else if self.busy.is_some() && !self.input {
            self.paint("subtle", "  esc to interrupt")
        } else if let Some(mode) = self.mode_text() {
            mode
        } else {
            if let Some(usage) = self.usage_hint() {
                self.paint("subtle", &format!("  {usage}"))
            } else {
            match self.editor.text().chars().next() {
                Some('!') => self.paint("subtle", "  ! bash mode \u{00b7} runs in the workspace, not by the agent"),
                Some('#') => self.paint("subtle", "  # saves a note to AGENTS.md"),
                _ if self.busy.is_some() => self.paint("subtle", &format!(
                    "  {} \u{00b7} esc to interrupt",
                    match self.busy_mode {
                        BusyMode::Queue => "enter queues",
                        BusyMode::Interrupt => "enter interrupts",
                    }
                )),
                _ => self.paint("subtle", "  ? for shortcuts"),
            }
            }
        };
        let mut right = Vec::new();
        if let Some(goal) = &self.info.goal {
            right.push(goal.clone());
        }
        if !self.info.model.is_empty() {
            right.push(format!("{} \u{00b7} {}", self.info.provider, self.info.model));
        }
        if self.info.agents > 1 {
            right.push(format!("{} agents", self.info.agents));
        }
        if self.info.tokens > 0 {
            right.push(format!("{} tokens", style::tokens(self.info.tokens)));
        }
        let right = self.paint("subtle", &right.join(" \u{00b7} "));
        spread(&left, &format!("{right} "), cols)
    }

    /// `usage: /model [name]` while the typed command word is complete and the
    /// command takes arguments.
    fn usage_hint(&self) -> Option<String> {
        let text = self.editor.text();
        let (word, _) = registry::split(text)?;
        if !text.contains(char::is_whitespace) {
            return None;
        }
        match registry::resolve(word) {
            registry::Resolved::Exact(spec) | registry::Resolved::Unique(spec) if !spec.args.is_empty() => {
                Some(format!("/{} {} \u{2014} {}", spec.name, spec.args, spec.desc))
            }
            _ => None,
        }
    }

    fn mode_text(&self) -> Option<String> {
        let (glyph, text, role_name) = match self.info.mode {
            Mode::Default => return None,
            Mode::AskAll => ("\u{23f8}", "asking before every tool", "warn"),
            Mode::Auto => ("\u{23f5}\u{23f5}", "auto-approving every tool", "warn"),
            Mode::Plan => ("\u{23f8}", "plan mode: read-only", "plan"),
        };
        Some(format!(
            "  {}{}",
            self.paint(role_name, &format!("{glyph} {text}")),
            self.paint("subtle", " (shift+tab to cycle)")
        ))
    }

    fn spinner_row(&self, busy: &Busy, cols: usize, now: Instant) -> String {
        let elapsed = now.saturating_duration_since(busy.started);
        let frame = SPINNER[(elapsed.as_millis() as u64 / SPINNER_MS) as usize % SPINNER.len()];
        let secs = elapsed.as_secs();
        let clock = if secs < 60 { format!("{secs}s") } else { format!("{}m {:02}s", secs / 60, secs % 60) };
        let mut parts = vec![clock];
        if !busy.status.activity.is_empty() {
            parts.push(busy.status.activity.clone());
        }
        if let Some((done, total)) = busy.status.tasks {
            if total > 0 {
                parts.push(format!("{done}/{total} tasks"));
            }
        }
        if busy.status.tokens > 0 {
            parts.push(format!("\u{2193} {} tokens", style::tokens(busy.status.tokens)));
        }
        let head = self.paint("brand", &format!("{frame} {}\u{2026}", busy.verb));
        let build = |parts: &[String]| {
            format!("{head}{}", self.paint("subtle", &format!(" ({})", parts.join(" \u{00b7} "))))
        };
        // The footer repeats the interrupt hint, so it is the first thing to go
        // on a narrow terminal rather than being clipped mid-word.
        let mut with_hint = parts.clone();
        with_hint.push("esc to interrupt".into());
        let full = build(&with_hint);
        truncate(&if width(&full) <= cols { full } else { build(&parts) }, cols)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::text::strip_ansi;

    fn app() -> App {
        let mut app = App::new(Options {
            queue_len: Arc::default(),
            colors: Color::Plain,
            history: History::memory(),
            workspace: PathBuf::from("/definitely/not/here"),
            input: true,
        });
        app.set_size(60, 20);
        app.placeholder = 0;
        app
    }

    fn press(app: &mut App, code: KeyCode) -> Vec<Effect> {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE), Instant::now())
    }

    fn ctrl(app: &mut App, ch: char) -> Vec<Effect> {
        app.on_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL), Instant::now())
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            press(app, KeyCode::Char(ch));
        }
    }

    fn view_text(app: &App) -> String {
        app.view(60, 20, Instant::now()).lines.iter().map(|line| strip_ansi(line)).collect::<Vec<_>>().join("\n")
    }

    fn submitted(effects: &[Effect]) -> Option<&str> {
        effects.iter().find_map(|effect| match effect {
            Effect::Action(Action::Submit(text)) => Some(text.as_str()),
            _ => None,
        })
    }

    #[test]
    fn typing_then_enter_submits_and_echoes() {
        let mut app = app();
        type_text(&mut app, "fix the bug");
        let effects = press(&mut app, KeyCode::Enter);
        assert_eq!(submitted(&effects), Some("fix the bug"));
        let echo = effects.iter().find_map(|e| if let Effect::Print(lines) = e { Some(lines.clone()) } else { None }).unwrap();
        assert_eq!(echo[0], "", "a blank row separates turns");
        assert!(echo[1].starts_with("\u{276f} fix the bug"), "{echo:?}");
        assert!(app.editor_text().is_empty(), "the prompt clears");
    }

    #[test]
    fn enter_on_an_empty_prompt_does_nothing() {
        let mut app = app();
        assert!(press(&mut app, KeyCode::Enter).is_empty());
        type_text(&mut app, "   ");
        assert!(submitted(&press(&mut app, KeyCode::Enter)).is_none());
    }

    /// The bug that started this rewrite.
    #[test]
    fn backspace_deletes_one_character_and_the_cursor_follows() {
        let mut app = app();
        type_text(&mut app, "abc");
        press(&mut app, KeyCode::Backspace);
        type_text(&mut app, "X");
        assert_eq!(app.editor_text(), "abX");
        let frame = app.view(60, 20, Instant::now());
        let (row, col) = frame.cursor.unwrap();
        assert_eq!(strip_ansi(&frame.lines[row]), "\u{276f} abX");
        assert_eq!(col, 5, "the cursor is just after the X");
    }

    #[test]
    fn backslash_enter_and_ctrl_j_and_alt_enter_make_newlines() {
        let mut app = app();
        type_text(&mut app, "line one\\");
        assert!(press(&mut app, KeyCode::Enter).is_empty());
        assert_eq!(app.editor_text(), "line one\n");
        type_text(&mut app, "two");
        ctrl(&mut app, 'j');
        type_text(&mut app, "three");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), Instant::now());
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), Instant::now());
        assert_eq!(app.editor_text(), "line one\ntwo\nthree\n\n");
        let text = view_text(&app);
        assert!(text.contains("\u{276f} line one"), "{text}");
    }

    #[test]
    fn a_submitted_multiline_prompt_arrives_whole() {
        let mut app = app();
        type_text(&mut app, "a");
        ctrl(&mut app, 'j');
        type_text(&mut app, "b");
        assert_eq!(submitted(&press(&mut app, KeyCode::Enter)), Some("a\nb"));
    }

    #[test]
    fn up_and_down_walk_history_and_keep_the_draft() {
        let mut app = app();
        for text in ["first", "second"] {
            type_text(&mut app, text);
            press(&mut app, KeyCode::Enter);
        }
        type_text(&mut app, "dra");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.editor_text(), "second");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.editor_text(), "first");
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.editor_text(), "dra", "the half-typed draft comes back");
    }

    #[test]
    fn ctrl_c_clears_a_draft_then_asks_before_quitting() {
        let mut app = app();
        type_text(&mut app, "something");
        assert!(ctrl(&mut app, 'c').is_empty());
        assert!(app.editor_text().is_empty(), "first Ctrl-C clears the draft");
        assert!(ctrl(&mut app, 'c').is_empty(), "second only arms");
        assert!(view_text(&app).contains("Press Ctrl-C again to exit"));
        let effects = ctrl(&mut app, 'c');
        assert_eq!(effects, vec![Effect::Action(Action::Quit)]);
    }

    #[test]
    fn the_exit_prompt_expires() {
        let mut app = app();
        let now = Instant::now();
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), now);
        assert!(app.on_tick(now + Duration::from_secs(3)), "expiry repaints");
        let effects = app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), now + Duration::from_secs(3));
        assert!(effects.is_empty(), "too late: it only re-arms");
    }

    #[test]
    fn ctrl_d_quits_only_on_an_empty_prompt_and_deletes_otherwise() {
        let mut app = app();
        type_text(&mut app, "ab");
        press(&mut app, KeyCode::Left);
        assert!(ctrl(&mut app, 'd').is_empty());
        assert_eq!(app.editor_text(), "a", "deleted the character under the cursor");
        press(&mut app, KeyCode::Backspace);
        assert!(ctrl(&mut app, 'd').is_empty());
        assert_eq!(ctrl(&mut app, 'd'), vec![Effect::Action(Action::Quit)]);
    }

    #[test]
    fn a_key_in_between_cancels_the_exit_prompt() {
        let mut app = app();
        ctrl(&mut app, 'c');
        type_text(&mut app, "x");
        press(&mut app, KeyCode::Backspace);
        assert!(ctrl(&mut app, 'c').is_empty(), "armed state was reset by typing");
    }

    #[test]
    fn escape_twice_discards_a_draft_into_history() {
        let mut app = app();
        type_text(&mut app, "keep me");
        assert!(press(&mut app, KeyCode::Esc).is_empty());
        assert!(view_text(&app).contains("Esc again to clear"));
        press(&mut app, KeyCode::Esc);
        assert!(app.editor_text().is_empty());
        press(&mut app, KeyCode::Up);
        assert_eq!(app.editor_text(), "keep me", "the discarded draft is recallable");
    }

    #[test]
    fn busy_enter_queues_and_the_queue_runs_after_the_turn() {
        let mut app = app();
        let now = Instant::now();
        app.on_msg(Msg::Busy(true), now);
        type_text(&mut app, "next goal");
        let effects = press(&mut app, KeyCode::Enter);
        assert!(effects.is_empty(), "queued, not submitted: {effects:?}");
        assert_eq!(app.queued(), vec!["next goal"]);
        assert!(view_text(&app).contains("next goal  (queued)"));
        let released = app.on_msg(Msg::Busy(false), now);
        assert_eq!(submitted(&released), Some("next goal"));
        assert!(app.queued().is_empty());
    }

    #[test]
    fn up_takes_back_a_queued_message_to_edit() {
        let mut app = app();
        app.on_msg(Msg::Busy(true), Instant::now());
        type_text(&mut app, "oops typo");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.editor_text(), "oops typo");
        assert!(app.queued().is_empty());
    }

    #[test]
    fn escape_interrupts_a_running_turn_but_only_after_closing_a_menu() {
        let mut app = app();
        app.on_msg(Msg::Busy(true), Instant::now());
        assert_eq!(press(&mut app, KeyCode::Esc), vec![Effect::Action(Action::Interrupt)]);
        assert_eq!(ctrl(&mut app, 'c'), vec![Effect::Action(Action::Interrupt)]);
        type_text(&mut app, "/he");
        assert!(press(&mut app, KeyCode::Esc).is_empty(), "esc closes the menu first");
        assert_eq!(press(&mut app, KeyCode::Esc), vec![Effect::Action(Action::Interrupt)]);
    }

    #[test]
    fn busy_interrupt_mode_stops_the_turn_and_queues_the_message() {
        let mut app = app();
        let now = Instant::now();
        type_text(&mut app, "/busy interrupt");
        press(&mut app, KeyCode::Enter);
        app.on_msg(Msg::Busy(true), now);
        type_text(&mut app, "do this instead");
        let effects = press(&mut app, KeyCode::Enter);
        assert_eq!(effects, vec![Effect::Action(Action::Interrupt)]);
        assert_eq!(submitted(&app.on_msg(Msg::Busy(false), now)), Some("do this instead"));
    }

    #[test]
    fn queue_commands_work_while_busy() {
        let mut app = app();
        app.on_msg(Msg::Busy(true), Instant::now());
        for goal in ["one", "two"] {
            type_text(&mut app, goal);
            press(&mut app, KeyCode::Enter);
        }
        type_text(&mut app, "/queue rm 1");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.queued(), vec!["two"]);
        type_text(&mut app, "/queue clear");
        press(&mut app, KeyCode::Enter);
        assert!(app.queued().is_empty());
    }

    #[test]
    fn typing_a_slash_opens_the_command_menu() {
        let mut app = app();
        type_text(&mut app, "/");
        let text = view_text(&app);
        assert!(text.contains("/help"), "{text}");
        assert!(text.contains("Show commands and shortcuts"), "{text}");
        type_text(&mut app, "comp");
        let text = view_text(&app);
        assert!(text.contains("/compact"), "{text}");
        assert!(!text.contains("/help"), "the menu filters as you type: {text}");
    }

    #[test]
    fn tab_completes_the_highlighted_command_without_running_it() {
        let mut app = app();
        type_text(&mut app, "/mod");
        let effects = press(&mut app, KeyCode::Tab);
        assert!(effects.is_empty());
        assert_eq!(app.editor_text(), "/model ", "a command that takes arguments gets a space");
        type_text(&mut app, "gpt-5");
        assert_eq!(submitted(&press(&mut app, KeyCode::Enter)), Some("/model gpt-5"));
    }

    #[test]
    fn enter_on_a_partial_command_completes_and_runs_it() {
        let mut app = app();
        type_text(&mut app, "/comp");
        let effects = press(&mut app, KeyCode::Enter);
        assert_eq!(submitted(&effects), Some("/compact"));
    }

    #[test]
    fn enter_on_a_partial_command_with_a_required_argument_waits_for_it() {
        let mut app = app();
        type_text(&mut app, "/pla");
        let effects = press(&mut app, KeyCode::Enter);
        assert!(submitted(&effects).is_none(), "{effects:?}");
        assert_eq!(app.editor_text(), "/plan ", "completed, cursor ready for the task");
        type_text(&mut app, "refactor the parser");
        assert_eq!(submitted(&press(&mut app, KeyCode::Enter)), Some("/plan refactor the parser"));
    }

    /// Regression: `/help` + Enter did nothing because `[filter]` was treated
    /// as an argument the command must wait for.
    #[test]
    fn a_complete_command_runs_even_when_its_arguments_are_optional() {
        let mut app = app();
        type_text(&mut app, "/help");
        assert_eq!(submitted(&press(&mut app, KeyCode::Enter)), Some("/help"));
        let mut app = self::app();
        type_text(&mut app, "/mod");
        assert_eq!(submitted(&press(&mut app, KeyCode::Enter)), Some("/model"), "a partial one completes and runs");
        let mut app = self::app();
        type_text(&mut app, "/model gpt-5");
        assert_eq!(submitted(&press(&mut app, KeyCode::Enter)), Some("/model gpt-5"));
    }

    #[test]
    fn arrows_move_the_menu_selection_and_enter_takes_it() {
        let mut app = app();
        type_text(&mut app, "/c");
        press(&mut app, KeyCode::Down);
        let second = app.menu.as_ref().unwrap().selected().unwrap().insert.clone();
        let effects = press(&mut app, KeyCode::Enter);
        assert_eq!(submitted(&effects), Some(second.trim()));
    }

    #[test]
    fn escape_dismisses_the_menu_until_the_text_changes() {
        let mut app = app();
        type_text(&mut app, "/he");
        press(&mut app, KeyCode::Esc);
        assert!(app.menu.is_none());
        assert!(!view_text(&app).contains("Show commands"));
        type_text(&mut app, "l");
        assert!(app.menu.is_some(), "typing brings it back");
    }

    #[test]
    fn a_path_that_starts_with_a_slash_is_not_a_command() {
        let mut app = app();
        type_text(&mut app, "/Users/me/notes.md is wrong");
        assert!(app.menu.is_none());
        assert_eq!(submitted(&press(&mut app, KeyCode::Enter)), Some("/Users/me/notes.md is wrong"));
    }

    #[test]
    fn at_signs_complete_workspace_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        let mut app = App::new(Options {
            queue_len: Arc::default(),
            colors: Color::Plain,
            history: History::memory(),
            workspace: dir.path().to_path_buf(),
            input: true,
        });
        app.set_size(60, 20);
        type_text(&mut app, "look at @src/ma");
        assert!(view_text(&app).contains("src/main.rs"));
        let effects = press(&mut app, KeyCode::Tab);
        assert!(effects.is_empty());
        assert_eq!(app.editor_text(), "look at @src/main.rs ");
        assert!(app.menu.is_none(), "the menu closes after a file is chosen");
    }

    #[test]
    fn question_mark_toggles_the_shortcut_sheet_on_an_empty_prompt_only() {
        let mut app = app();
        press(&mut app, KeyCode::Char('?'));
        assert!(view_text(&app).contains("for bash mode"));
        press(&mut app, KeyCode::Char('?'));
        assert!(!view_text(&app).contains("for bash mode"));
        type_text(&mut app, "why");
        press(&mut app, KeyCode::Char('?'));
        assert_eq!(app.editor_text(), "why?", "a question mark in a sentence is just text");
    }

    #[test]
    fn ctrl_r_searches_history_and_enter_accepts() {
        let mut app = app();
        for text in ["fix login", "write docs", "fix signup"] {
            type_text(&mut app, text);
            press(&mut app, KeyCode::Enter);
        }
        ctrl(&mut app, 'r');
        type_text(&mut app, "fix");
        assert!(view_text(&app).contains("fix signup"));
        ctrl(&mut app, 'r');
        assert!(view_text(&app).contains("fix login"), "ctrl+r again finds the next older match");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.editor_text(), "fix login");
    }

    #[test]
    fn escape_cancels_a_search_and_keeps_the_draft() {
        let mut app = app();
        type_text(&mut app, "old");
        press(&mut app, KeyCode::Enter);
        type_text(&mut app, "draft");
        ctrl(&mut app, 'r');
        type_text(&mut app, "ol");
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.editor_text(), "draft");
    }

    #[test]
    fn big_pastes_collapse_in_the_prompt_and_arrive_whole() {
        let mut app = app();
        let blob: String = (0..30).map(|n| format!("row {n}\n")).collect();
        app.on_paste(&blob);
        assert!(view_text(&app).contains("[Pasted text #1 +31 lines]"));
        let effects = press(&mut app, KeyCode::Enter);
        assert_eq!(submitted(&effects), Some(blob.trim()));
    }

    #[test]
    fn a_dialog_takes_every_key_and_answers() {
        let mut app = app();
        type_text(&mut app, "draft");
        let (tx, mut rx) = oneshot::channel();
        let now = Instant::now();
        let mut spec = DialogSpec::pick("Pick", vec!["a".into(), "b".into()]);
        spec.shortcuts = vec![('b', 1)];
        app.on_msg(Msg::Dialog(spec, tx), now);
        let later = now + Duration::from_secs(1);
        assert!(app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), later).is_empty());
        assert_eq!(app.editor_text(), "draft", "typing does not reach the prompt");
        assert!(view_text(&app).contains("Pick"));
        app.on_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE), later);
        assert_eq!(rx.try_recv().unwrap(), Some(1));
        assert!(!view_text(&app).contains("Pick"), "the prompt returns");
    }

    #[test]
    fn dialogs_queue_up_and_open_in_order() {
        let mut app = app();
        let now = Instant::now();
        let later = now + Duration::from_secs(1);
        let (tx1, mut rx1) = oneshot::channel();
        let (tx2, mut rx2) = oneshot::channel();
        app.on_msg(Msg::Dialog(DialogSpec::pick("First", vec!["x".into()]), tx1), now);
        app.on_msg(Msg::Dialog(DialogSpec::pick("Second", vec!["y".into()]), tx2), now);
        assert!(view_text(&app).contains("First"));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), later);
        assert_eq!(rx1.try_recv().unwrap(), None);
        assert!(view_text(&app).contains("Second"), "the next dialog opens");
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), later);
        assert_eq!(rx2.try_recv().unwrap(), None);
    }

    #[test]
    fn shutdown_answers_open_dialogs_so_no_task_hangs() {
        let mut app = app();
        let (tx, mut rx) = oneshot::channel();
        let (tx2, mut rx2) = oneshot::channel();
        let now = Instant::now();
        app.on_msg(Msg::Dialog(DialogSpec::pick("A", vec!["x".into()]), tx), now);
        app.on_msg(Msg::Dialog(DialogSpec::pick("B", vec!["y".into()]), tx2), now);
        assert_eq!(app.on_msg(Msg::Shutdown, now), vec![Effect::Quit]);
        assert_eq!(rx.try_recv().unwrap(), None);
        assert_eq!(rx2.try_recv().unwrap(), None);
    }

    #[test]
    fn the_spinner_shows_verb_time_activity_and_tokens() {
        let mut app = app();
        let start = Instant::now();
        app.on_msg(Msg::Busy(true), start);
        app.on_msg(
            Msg::Status(Status { activity: "running tool".into(), tokens: 1234, tasks: Some((1, 3)) }),
            start,
        );
        let frame = app.view(100, 20, start + Duration::from_secs(12));
        let spinner = strip_ansi(&frame.lines[1]);
        assert!(spinner.contains("12s"), "{spinner}");
        assert!(spinner.contains("running tool"), "{spinner}");
        assert!(spinner.contains("1/3 tasks"), "{spinner}");
        assert!(spinner.contains("\u{2193} 1.2k tokens"), "{spinner}");
        assert!(spinner.contains("esc to interrupt"), "{spinner}");
        assert!(spinner.contains('\u{2026}'));
    }

    #[test]
    fn the_footer_shows_mode_goal_and_session_numbers() {
        let mut app = app();
        app.on_msg(
            Msg::Info(Info {
                provider: "mock".into(),
                model: "mock-model".into(),
                agents: 3,
                mode: Mode::Auto,
                tokens: 41_200,
                goal: Some("\u{2299} goal 3/20".into()),
                ..Info::default()
            }),
            Instant::now(),
        );
        let text = app
            .view(130, 20, Instant::now())
            .lines
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("auto-approving every tool"), "{text}");
        assert!(text.contains("shift+tab to cycle"), "{text}");
        assert!(text.contains("\u{2299} goal 3/20"), "{text}");
        assert!(text.contains("mock \u{00b7} mock-model"), "{text}");
        assert!(text.contains("3 agents"), "{text}");
        assert!(text.contains("41.2k tokens"), "{text}");
    }

    #[test]
    fn a_narrow_spinner_drops_the_hint_instead_of_clipping_it() {
        let mut app = app();
        let start = Instant::now();
        app.on_msg(Msg::Busy(true), start);
        app.on_msg(
            Msg::Status(Status { activity: "running tool".into(), tokens: 1234, tasks: Some((1, 3)) }),
            start,
        );
        let narrow = strip_ansi(&app.view(70, 20, start).lines[1]);
        assert!(!narrow.contains("inter"), "{narrow}");
        assert!(narrow.contains("tokens)"), "{narrow}");
        assert!(width(&narrow) <= 70);
    }

    #[test]
    fn typing_a_command_that_takes_arguments_shows_its_usage() {
        let mut app = app();
        type_text(&mut app, "/model ");
        assert!(view_text(&app).contains("/model [name] \u{2014} Show or change the model"));
        let mut app = self::app();
        type_text(&mut app, "/help");
        assert!(!view_text(&app).contains("usage"), "no hint while still choosing the command");
    }

    #[test]
    fn the_queue_length_is_shared_for_readers_outside_the_ui() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut app = App::new(Options {
            queue_len: Arc::clone(&counter),
            colors: Color::Plain,
            history: History::memory(),
            workspace: PathBuf::from("."),
            input: true,
        });
        app.set_size(60, 20);
        app.on_msg(Msg::Busy(true), Instant::now());
        type_text(&mut app, "one");
        press(&mut app, KeyCode::Enter);
        type_text(&mut app, "two");
        press(&mut app, KeyCode::Enter);
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        app.on_msg(Msg::Busy(false), Instant::now());
        assert_eq!(counter.load(Ordering::SeqCst), 1, "one was released to run");
    }

    #[test]
    fn the_default_footer_offers_shortcuts() {
        let app = app();
        assert!(view_text(&app).contains("? for shortcuts"));
    }

    #[test]
    fn bash_and_note_modes_change_the_prompt() {
        let mut app = app();
        type_text(&mut app, "!ls");
        let text = view_text(&app);
        assert!(text.contains("\n! ls"), "the marker replaces the bang: {text}");
        assert!(!text.contains("! !ls"), "{text}");
        assert!(text.contains("bash mode"), "{text}");
        let frame = app.view(60, 20, Instant::now());
        let (row, col) = frame.cursor.unwrap();
        assert_eq!(strip_ansi(&frame.lines[row]), "! ls");
        assert_eq!(col, 4, "the cursor follows the shifted text");
        let mut app = self::app();
        type_text(&mut app, "#use uv");
        assert!(view_text(&app).contains("saves a note"));
    }

    #[test]
    fn the_frame_never_exceeds_the_terminal() {
        let mut app = app();
        app.on_msg(Msg::Busy(true), Instant::now());
        for _ in 0..30 {
            type_text(&mut app, "a long line of text that keeps going and going");
            ctrl(&mut app, 'j');
        }
        for (cols, rows) in [(20, 8), (40, 12), (80, 24), (120, 40)] {
            let frame = app.view(cols, rows, Instant::now());
            assert!(frame.lines.len() <= rows - 1, "{} lines for {rows} rows", frame.lines.len());
            for line in &frame.lines {
                assert!(width(line) <= cols, "{cols}: {:?}", strip_ansi(line));
            }
            let (row, col) = frame.cursor.expect("the cursor stays visible");
            assert!(row < frame.lines.len() && col < cols);
        }
    }

    #[test]
    fn the_cursor_sits_on_the_input_row_under_a_wrapped_line() {
        let mut app = app();
        type_text(&mut app, &"word ".repeat(30));
        let frame = app.view(40, 20, Instant::now());
        let (row, _) = frame.cursor.unwrap();
        assert!(strip_ansi(&frame.lines[row + 1]).starts_with('\u{2500}'), "the bottom rule is right under the last input row");
    }

    #[test]
    fn run_mode_has_no_prompt_and_esc_interrupts() {
        let mut app = App::new(Options {
            queue_len: Arc::default(),
            colors: Color::Plain,
            history: History::memory(),
            workspace: PathBuf::from("."),
            input: false,
        });
        app.on_msg(Msg::Busy(true), Instant::now());
        let text = app.view(60, 20, Instant::now()).lines.join("\n");
        assert!(!text.contains('\u{276f}') && !text.contains('\u{2500}'), "{text}");
        assert_eq!(press(&mut app, KeyCode::Esc), vec![Effect::Action(Action::Interrupt)]);
        assert!(press(&mut app, KeyCode::Char('x')).is_empty());
    }

    #[test]
    fn shift_tab_cycles_the_mode_and_ctrl_o_toggles_detail() {
        let mut app = app();
        assert_eq!(press(&mut app, KeyCode::BackTab), vec![Effect::Action(Action::CycleMode)]);
        assert_eq!(ctrl(&mut app, 'o'), vec![Effect::Action(Action::ToggleVerbose)]);
        assert_eq!(ctrl(&mut app, 'l'), vec![Effect::ClearScreen]);
        assert_eq!(ctrl(&mut app, 'z'), vec![Effect::Suspend]);
    }

    #[test]
    fn emacs_line_editing_works() {
        let mut app = app();
        type_text(&mut app, "hello world");
        ctrl(&mut app, 'a');
        type_text(&mut app, ">> ");
        ctrl(&mut app, 'e');
        ctrl(&mut app, 'w');
        assert_eq!(app.editor_text(), ">> hello ");
        ctrl(&mut app, 'u');
        assert_eq!(app.editor_text(), "");
        ctrl(&mut app, 'y');
        assert_eq!(app.editor_text(), ">> hello ", "yank restores the kill");
    }

    #[test]
    fn echo_wraps_and_marks_every_row() {
        let app = app();
        let rows = app.echo("one two three four five six seven eight nine ten eleven twelve thirteen fourteen");
        assert_eq!(rows[0], "");
        assert!(rows[1].starts_with("\u{276f} one"));
        assert!(rows.len() >= 3, "{rows:?}");
        assert!(rows[2..].iter().all(|row| row.starts_with("  ")), "{rows:?}");
        assert!(rows.iter().all(|row| width(row) <= 60));
    }
}
