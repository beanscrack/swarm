//! Interactive chat mode: the Claude Code / Hermes-style REPL around the run
//! engine. Plain `swarm` opens it, `swarm chat <goal>` seeds a first turn, and
//! `--continue` / `--resume <run>` carry a previous run's context forward.
//!
//! Each goal is a normal durable run, so everything the one-shot path gives you
//! (store, checkpoints, budgets, approvals, `swarm logs --transcript`) still
//! applies. What this module adds is the session around those runs: transcript
//! UI, slash commands, prompt history, multi-turn context, Ctrl-C interrupt, and
//! goals typed while a run is in flight (queued and executed next, Hermes-style
//! steering).

use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, poll, read};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, is_raw_mode_enabled};
use serde::{Deserialize, Serialize};
use swarm_core::config::{ApprovalPolicy, RunConfig};
use swarm_core::run::RunMode;
use swarm_core::RunId;
use swarm_store::Store;
use tokio::sync::mpsc;

use crate::approval::AllowList;
use crate::commands::{self, ChatArgs, TurnOutcome};
use crate::transcript::{self, Color, Header, paint};

const PROMPT: &str = "\u{25cf} ";
const KV_NAMESPACE: &str = "chat";
const KV_ACTIVE: &str = "active";
const SUMMARY_CHARS: usize = 400;

pub const HELP: &str = "\
commands
  /help                 this text
  /exit /quit           leave the session (Ctrl+D also works)
  /new                  start a fresh session (context cleared)
  /clear                forget the conversation, keep the session id
  /compact              fold older turns into a short digest
  /model <name>         model for the next turns
  /agents <n>           swarm size for the next turns
  /approve ask|danger|all   tool approval policy
  /allow [tool]       list/allow a tool for the rest of the session
  /allow-all          stop asking for approval this session
  /cost                 tokens, calls and time for this session
  /status [run]       run state and result (last run by default)
  /tasks [run]          tasks in a run
  /logs [run]           transcript replay of a run
  /runs                 recent runs in this store
  /history              turns in this session
  /tools                tools the agent may call
  /copy                 print the last result in full
  /resume <run>         load a run's goal and result into this session
keys
  enter                 send        \\ then enter  newline
  up / down             history     ctrl+r        search history
  tab                   complete /commands
  ctrl+c                interrupt a running turn (ctrl+d quits)
  ctrl+o                toggle transcript detail
  ctrl+l                redraw
  ctrl+a/e, ctrl+k/u/w, ctrl+b/f   line editing
";

/// Slash commands, used by `/help` and Tab completion.
pub const SLASHES: &[&str] = &[
    "/agents", "/allow", "/allow-all", "/approve", "/clear", "/compact", "/copy", "/cost",
    "/exit", "/help", "/history", "/logs", "/model", "/new", "/quit", "/resume", "/runs",
    "/status", "/tasks", "/tools", "/verbose",
];

/// One completed goal in a chat session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub goal: String,
    pub status: String,
    pub summary: String,
    pub run_id: String,
}

/// Session totals shown by `/cost`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    pub tokens: u64,
    pub inferences: u64,
    pub tool_calls: u64,
    pub seconds: f64,
}

impl Totals {
    fn add(&mut self, turn: &TurnOutcome) {
        self.tokens += turn.tokens;
        self.inferences += turn.inferences;
        self.tool_calls += turn.tool_calls;
        self.seconds += turn.seconds;
    }
}

/// A chat session: the turns so far, an optional compacted digest, and totals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub turns: Vec<Turn>,
    pub digest: Option<String>,
    pub totals: Totals,
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into(), turns: Vec::new(), digest: None, totals: Totals::default() }
    }

    fn with_id() -> Self {
        Self::new(new_session_id())
    }

    pub fn push(&mut self, turn: Turn) {
        self.turns.push(turn);
    }

    /// The context block handed to the next run. Empty sessions add nothing, so
    /// a first turn is byte-identical to `swarm run`.
    ///
    /// `max_chars` is a hard budget for the whole block (preamble, labels and
    /// trailer included): newest turns are kept, oldest are dropped.
    pub fn render(&self, max_chars: usize) -> String {
        const PREAMBLE: &str =
            "Conversation so far. Build on it; do not repeat finished work.\n";
        const LABEL: &str = "Turns in this session (oldest first):\n";
        const TRAILER: &str = "\n\nNew request: ";
        if self.turns.is_empty() && self.digest.is_none() {
            return String::new();
        }
        let overhead = PREAMBLE.chars().count() + LABEL.chars().count() + TRAILER.chars().count();
        let budget = max_chars.saturating_sub(overhead);
        // Each turn gets a share of the budget, so even one turn cannot blow it.
        let per_row = (budget / 3).clamp(40, SUMMARY_CHARS);
        let mut used = 0usize;
        let mut kept: Vec<String> = Vec::new();
        for (index, turn) in self.turns.iter().enumerate().rev() {
            let row = format!(
                "{}. [{}] {} -- {}",
                index + 1,
                turn.status,
                clip(&turn.goal, per_row / 2),
                clip(&turn.summary, per_row)
            );
            let cost = row.chars().count() + 1;
            if !kept.is_empty() && used + cost > budget {
                break;
            }
            used += cost;
            kept.push(row);
        }
        kept.reverse();
        let dropped = self.turns.len().saturating_sub(kept.len());
        let mut body = String::new();
        if let Some(digest) = &self.digest {
            let room = budget.saturating_sub(used);
            if room >= 60 {
                body.push_str(&format!("Earlier turns: {}\n", clip(digest, room)));
            }
        }
        if dropped > 0 {
            body.push_str(&format!("({dropped} older turn(s) omitted)\n"));
        }
        format!("{PREAMBLE}{LABEL}{body}{}{TRAILER}", kept.join("\n"))
    }

    /// Fold everything but the newest `keep` turns into the digest.
    pub fn compact(&mut self, keep: usize) -> usize {
        if self.turns.len() <= keep {
            return 0;
        }
        let folded: Vec<Turn> = self.turns.drain(..self.turns.len() - keep).collect();
        let count = folded.len();
        let mut lines: Vec<String> = Vec::new();
        lines.extend(self.digest.clone().map(|old| format!("{old}\n")));
        for turn in &folded {
            lines.push(format!("- [{}] {} -> {}", turn.status, clip(&turn.goal, 80),
                clip(&turn.summary, 120)));
        }
        self.digest = Some(lines.join("\n"));
        count
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }
}

fn clip(text: &str, max: usize) -> String {
    let flat: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    let flat = flat.trim();
    if flat.chars().count() <= max {
        return flat.to_string();
    }
    let head: String = flat.chars().take(max - 1).collect();
    format!("{head}\u{2026}")
}

fn new_session_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("chat-{nanos:x}-{:x}", SEQ.fetch_add(1, Ordering::Relaxed))
}

/// A parsed slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slash {
    Help,
    Exit,
    New,
    Clear,
    Compact,
    Verbose(Option<bool>),
    Cost,
    Status(Option<String>),
    Tasks(Option<String>),
    Logs(Option<String>),
    Runs,
    History,
    Tools,
    Copy,
    Resume(String),
    Model(String),
    Agents(u32),
    Approve(ApprovalPolicy),
    Allow(String),
    AllowAll,
    Unknown(String),
}

/// Parse a line starting with `/`. Anything else is a goal, not a command.
pub fn parse_slash(input: &str) -> Option<Slash> {
    let line = input.trim();
    let body = line.strip_prefix('/')?;
    let (name, rest) = match body.split_once(' ') {
        Some((name, rest)) => (name, rest.trim()),
        None => (body, ""),
    };
    let run = |name: &str| match name {
        "help" | "?" => Slash::Help,
        "exit" | "quit" | "q" => Slash::Exit,
        "new" => Slash::New,
        "clear" => Slash::Clear,
        "compact" => Slash::Compact,
        "verbose" => Slash::Verbose(match rest {
            "on" | "true" | "1" => Some(true),
            "off" | "false" | "0" => Some(false),
            _ => None,
        }),
        "cost" | "usage" => Slash::Cost,
        "status" => Slash::Status(shelf(rest)),
        "tasks" => Slash::Tasks(shelf(rest)),
        "logs" | "transcript" => Slash::Logs(shelf(rest)),
        "runs" => Slash::Runs,
        "history" | "turns" => Slash::History,
        "tools" => Slash::Tools,
        "copy" => Slash::Copy,
        "resume" => Slash::Resume(rest.to_string()),
        "model" => Slash::Model(rest.to_string()),
        "agents" => Slash::Agents(rest.parse().unwrap_or(0)),
        "allow" => Slash::Allow(rest.to_string()),
        "allow-all" | "yolo" => Slash::AllowAll,
        "approve" | "permissions" => Slash::Approve(match rest {
            "all" | "ask_all" | "every" => ApprovalPolicy::AskAll,
            "never" | "auto" | "yolo" => ApprovalPolicy::Never,
            _ => ApprovalPolicy::AskDangerous,
        }),
        other => Slash::Unknown(other.to_string()),
    };
    Some(run(name))
}

fn shelf(text: &str) -> Option<String> {
    (!text.is_empty()).then(|| text.to_string())
}

/// Complete a `/com` prefix. Returns the completion, or the candidates when the
/// prefix is ambiguous.
pub fn complete_slash(prefix: &str) -> Completion {
    let Some(body) = prefix.strip_prefix('/') else { return Completion::None };
    let matches: Vec<&str> =
        SLASHES.iter().copied().filter(|name| name[1..].starts_with(body)).collect();
    match matches.len() {
        0 => Completion::None,
        1 => Completion::Text(format!("/{}", matches[0].trim_start_matches('/'))),
        _ => Completion::Candidates(matches.iter().map(|name| name.to_string()).collect()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completion {
    None,
    Text(String),
    Candidates(Vec<String>),
}

/// Prompt history with up/down browsing and reverse search.
#[derive(Debug, Default, Clone)]
pub struct History {
    items: Vec<String>,
    cursor: usize,
}

impl History {
    pub fn push(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if self.items.last().map(String::as_str) == Some(text) {
            self.cursor = self.items.len();
            return;
        }
        self.items.push(text.to_string());
        self.cursor = self.items.len();
    }

    pub fn prev(&mut self) -> Option<&str> {
        if self.cursor == 0 || self.items.is_empty() {
            return None;
        }
        self.cursor -= 1;
        self.items.get(self.cursor).map(String::as_str)
    }

    pub fn next(&mut self) -> Option<&str> {
        if self.items.is_empty() {
            return None;
        }
        self.cursor = (self.cursor + 1).min(self.items.len());
        self.items.get(self.cursor).map(String::as_str)
    }

    pub fn search(&self, query: &str) -> Option<&str> {
        self.items.iter().rev().find(|item| item.contains(query)).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// What the input pump tells the REPL loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A goal to run now.
    Submit(String),
    /// A goal typed while a turn was running; run it next.
    Queued(String),
    /// Ctrl+C: cancel the running turn, or quit at an idle prompt.
    Interrupt,
    /// Ctrl+D on an empty prompt.
    Eof,
    /// Ctrl+L.
    Redraw,
    /// Ctrl+O.
    ToggleVerbose,
}

/// The prompt mode the input pump renders for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Prompt,
    /// A turn is running: input is queued, the prompt row is not drawn.
    Busy,
}

/// Input pump: one thread owns raw mode and the prompt row, the loop receives
/// finished actions.
pub struct Reader {
    pub rx: mpsc::UnboundedReceiver<Action>,
    pub mode: mpsc::UnboundedSender<Mode>,
}

/// Raw-mode keyboard reader. Only valid on a TTY.
///
/// This thread owns raw mode and stdin for the whole process, so it also
/// answers approval prompts: the card sends a request, the next key press is
/// the answer. Two threads reading the console handle races and loses keys.
pub fn spawn_tty_reader(quiet: Arc<AtomicBool>) -> io::Result<Reader> {
    let (tx_action, rx) = mpsc::unbounded_channel();
    let (tx_mode, mode_rx) = mpsc::unbounded_channel();
    let (tx_keys, key_rx) = tokio::sync::mpsc::unbounded_channel();
    crate::approval::install_key_bus(tx_keys);
    std::thread::spawn(move || tty_reader(tx_action, mode_rx, quiet, key_rx));
    Ok(Reader { rx, mode: tx_mode })
}

fn tty_reader(
    out: mpsc::UnboundedSender<Action>,
    mode_in: mpsc::UnboundedReceiver<Mode>,
    quiet: Arc<AtomicBool>,
    mut approval: tokio::sync::mpsc::UnboundedReceiver<crate::approval::KeyRequest>,
) {
    let mut mode_in = mode_in;
    let colors = Color::detect(true);
    let mut history = History::default();
    let mut buffer = String::new();
    let mut cursor = 0usize;
    let mut draft: Option<String> = None;
    let mut mode = Mode::Prompt;
    let opened_raw = !is_raw_mode_enabled().unwrap_or(false);
    if opened_raw {
        let _ = enable_raw_mode();
    }
    draw(&buffer, cursor, &mode, colors);
    loop {
        while let Ok(next) = mode_in.try_recv() {
            if next == Mode::Busy {
                clear_row();
            }
            mode = next;
        }
        // While an approval card owns the terminal, keystrokes belong to it.
        if quiet.load(Ordering::SeqCst) {
            if let Ok(reply) = approval.try_recv() {
                let _ = reply.send(read_approval_key());
            }
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        if !poll(Duration::from_millis(120)).unwrap_or(false) {
            continue;
        }
        let Ok(event) = read() else { break };
        match event {
            Event::Paste(text) if mode == Mode::Prompt => {
                insert(&mut buffer, &mut cursor, &text);
                draw(&buffer, cursor, &mode, colors);
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                let busy = mode == Mode::Busy;
                match key.code {
                    KeyCode::Char('c') if ctrl => {
                        if busy || buffer.is_empty() {
                            let _ = out.send(Action::Interrupt);
                        } else {
                            buffer.clear();
                            cursor = 0;
                            draw(&buffer, cursor, &mode, colors);
                        }
                    }
                    KeyCode::Char('d') if ctrl && buffer.is_empty() => {
                        let _ = out.send(Action::Eof);
                        break;
                    }
                    KeyCode::Char('d') if ctrl => {
                        remove(&mut buffer, &mut cursor, 1);
                        draw(&buffer, cursor, &mode, colors);
                    }
                    KeyCode::Char('l') if ctrl => {
                        print!("\x1b[2J\x1b[H");
                        let _ = io::stdout().flush();
                        draw(&buffer, cursor, &mode, colors);
                        let _ = out.send(Action::Redraw);
                    }
                    KeyCode::Char('o') if ctrl => {
                        let _ = out.send(Action::ToggleVerbose);
                        draw(&buffer, cursor, &mode, colors);
                    }
                    KeyCode::Char('r') if ctrl => {
                        if let Some(found) = reverse_search(&mut history, colors) {
                            buffer = found;
                            cursor = buffer.chars().count();
                            draw(&buffer, cursor, &mode, colors);
                        }
                    }
                    KeyCode::Char('a') if ctrl => {
                        cursor = 0;
                        draw(&buffer, cursor, &mode, colors);
                    }
                    KeyCode::Char('e') if ctrl => {
                        cursor = buffer.chars().count();
                        draw(&buffer, cursor, &mode, colors);
                    }
                    KeyCode::Char('b') if ctrl => {
                        cursor = cursor.saturating_sub(1);
                        draw(&buffer, cursor, &mode, colors);
                    }
                    KeyCode::Char('f') if ctrl => {
                        cursor = (cursor + 1).min(buffer.chars().count());
                        draw(&buffer, cursor, &mode, colors);
                    }
                    KeyCode::Char('k') if ctrl => {
                        let keep: String = buffer.chars().take(cursor).collect();
                        buffer = keep;
                        draw(&buffer, cursor, &mode, colors);
                    }
                    KeyCode::Char('u') if ctrl => {
                        buffer.clear();
                        cursor = 0;
                        draw(&buffer, cursor, &mode, colors);
                    }
                    KeyCode::Char('w') if ctrl => {
                        drop_word(&mut buffer, &mut cursor);
                        draw(&buffer, cursor, &mode, colors);
                    }
                    KeyCode::Char('p') if ctrl => {
                        browse(&mut buffer, &mut cursor, &mut history, &mut draft, true, colors, &mode);
                    }
                    KeyCode::Char('n') if ctrl => {
                        browse(&mut buffer, &mut cursor, &mut history, &mut draft, false, colors, &mode);
                    }
                    KeyCode::Up if !busy => {
                        browse(&mut buffer, &mut cursor, &mut history, &mut draft, true, colors, &mode);
                    }
                    KeyCode::Down if !busy => {
                        browse(&mut buffer, &mut cursor, &mut history, &mut draft, false, colors, &mode);
                    }
                    KeyCode::Enter if busy => {
                        let text = buffer.trim().to_string();
                        buffer.clear();
                        cursor = 0;
                        if !text.is_empty() {
                            history.push(&text);
                            println!(
                                "\n{} {}",
                                paint(colors, "meta", "\u{21b3} queued"),
                                paint(colors, "goal", &clip(&text, 70))
                            );
                            let _ = io::stdout().flush();
                            let _ = out.send(Action::Queued(text));
                        }
                    }
                    KeyCode::Enter if shift || alt => {
                        if buffer.ends_with('\\') {
                            buffer.pop();
                            cursor = cursor.saturating_sub(1);
                        }
                        insert(&mut buffer, &mut cursor, "\n");
                        draw(&buffer, cursor, &mode, colors);
                    }
                    KeyCode::Enter => {
                        let text = buffer.trim().to_string();
                        buffer.clear();
                        cursor = 0;
                        draft = None;
                        clear_row();
                        if text.is_empty() {
                            draw(&buffer, cursor, &mode, colors);
                            continue;
                        }
                        history.push(&text);
                        println!("{PROMPT}{}", text.replace('\n', "\n  "));
                        let _ = io::stdout().flush();
                        let _ = out.send(Action::Submit(text));
                    }
                    KeyCode::Tab if !busy => match complete_slash(&buffer) {
                        Completion::Text(text) => {
                            buffer = text;
                            cursor = buffer.chars().count();
                            draw(&buffer, cursor, &mode, colors);
                        }
                        Completion::Candidates(list) => {
                            clear_row();
                            println!("{} {}", paint(colors, "meta", "commands"), list.join("  "));
                            let _ = io::stdout().flush();
                            draw(&buffer, cursor, &mode, colors);
                        }
                        Completion::None => {}
                    },
                    KeyCode::Backspace => {
                        if cursor > 0 {
                            remove(&mut buffer, &mut cursor, 1);
                            cursor -= 1;
                            draw(&buffer, cursor, &mode, colors);
                        }
                    }
                    KeyCode::Delete => {
                        remove(&mut buffer, &mut cursor, 0);
                        draw(&buffer, cursor, &mode, colors);
                    }
                    KeyCode::Left => {
                        cursor = cursor.saturating_sub(1);
                        draw(&buffer, cursor, &mode, colors);
                    }
                    KeyCode::Right => {
                        cursor = (cursor + 1).min(buffer.chars().count());
                        draw(&buffer, cursor, &mode, colors);
                    }
                    KeyCode::Home => {
                        cursor = 0;
                        draw(&buffer, cursor, &mode, colors);
                    }
                    KeyCode::End => {
                        cursor = buffer.chars().count();
                        draw(&buffer, cursor, &mode, colors);
                    }
                    KeyCode::Esc if !busy => {
                        buffer.clear();
                        cursor = 0;
                        draw(&buffer, cursor, &mode, colors);
                    }
                    KeyCode::Char(ch) if !ctrl => {
                        insert(&mut buffer, &mut cursor, &ch.to_string());
                        draw(&buffer, cursor, &mode, colors);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if opened_raw && is_raw_mode_enabled().unwrap_or(false) {
        let _ = disable_raw_mode();
    }
}

/// A stdin line reader for pipes and CI (no raw mode, no echo).
pub fn spawn_line_reader() -> Reader {
    let (tx, rx) = mpsc::unbounded_channel();
    let (tx_mode, mode_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut mode_rx = mode_rx;
        let stdin = io::stdin();
        let mut line = String::new();
        loop {
            while let Ok(next) = mode_rx.try_recv() {
                if next == Mode::Busy {
                    let _ = tx.send(Action::Interrupt);
                }
            }
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) => {
                    let _ = tx.send(Action::Eof);
                    break;
                }
                Ok(_) => {
                    let text = line.trim_end_matches(['\n', '\r']).to_string();
                    if !text.trim().is_empty() {
                        let _ = tx.send(Action::Submit(text));
                    }
                }
                Err(_) => {
                    let _ = tx.send(Action::Eof);
                    break;
                }
            }
        }
    });
    Reader { rx, mode: tx_mode }
}

fn insert(buffer: &mut String, cursor: &mut usize, text: &str) {
    let head: String = buffer.chars().take(*cursor).collect();
    let tail: String = buffer.chars().skip(*cursor).collect();
    *buffer = format!("{head}{text}{tail}");
    *cursor += text.chars().count();
}

fn remove(buffer: &mut String, cursor: &mut usize, before: usize) {
    let mut chars: Vec<char> = buffer.chars().collect();
    if chars.is_empty() {
        return;
    }
    let index = if before > 0 {
        if *cursor == 0 {
            return;
        }
        *cursor - 1
    } else if *cursor >= chars.len() {
        return;
    } else {
        *cursor
    };
    chars.remove(index);
    *buffer = chars.into_iter().collect();
    if before > 0 {
        *cursor -= 1;
    }
}

fn drop_word(buffer: &mut String, cursor: &mut usize) {
    let chars: Vec<char> = buffer.chars().collect();
    let mut index = *cursor;
    while index > 0 && chars[index - 1].is_whitespace() {
        index -= 1;
    }
    while index > 0 && !chars[index - 1].is_whitespace() {
        index -= 1;
    }
    let keep: String = chars[..index].iter().collect();
    let tail: String = chars[*cursor..].iter().collect();
    *buffer = format!("{keep}{tail}");
    *cursor = index;
}

fn browse(
    buffer: &mut String,
    cursor: &mut usize,
    history: &mut History,
    draft: &mut Option<String>,
    backwards: bool,
    colors: Color,
    mode: &Mode,
) {
    let found = if backwards { history.prev() } else { history.next() };
    if let Some(text) = found {
        if draft.is_none() {
            *draft = Some(buffer.clone());
        }
        *buffer = text.to_string();
        *cursor = buffer.chars().count();
    } else if backwards {
        *buffer = draft.clone().unwrap_or_default();
        *cursor = buffer.chars().count();
    } else {
        *buffer = draft.take().unwrap_or_default();
        *cursor = buffer.chars().count();
    }
    draw(buffer, *cursor, mode, colors);
}

fn reverse_search(history: &mut History, colors: Color) -> Option<String> {
    let mut query = String::new();
    if history.is_empty() {
        return None;
    }
    loop {
        let matched = history.search(&query).map(|hit| clip(hit, 48)).unwrap_or_default();
        let depth = format!("of {}", history.len());
        eprint!(
            "\r{} {}  {} {}",
            paint(colors, "meta", "(search)"),
            query,
            paint(colors, "muted", &depth),
            paint(colors, "dimmed", if matched.is_empty() { "-" } else { &matched }),
        );
        let _ = io::stderr().flush();
        if !poll(Duration::from_secs(30)).unwrap_or(false) {
            clear_row_stderr();
            return None;
        }
        let Ok(event) = read() else { return None };
        let Event::Key(key) = event else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Enter => {
                clear_row_stderr();
                return history.search(&query).map(str::to_string);
            }
            KeyCode::Esc | KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                clear_row_stderr();
                return None;
            }
            KeyCode::Backspace => {
                query.pop();
            }
            KeyCode::Char(ch) => {
                query.push(ch);
            }
            _ => {}
        }
    }
}

/// One key press, as an approval answer. Used while the card is on screen.
fn read_approval_key() -> crate::approval::Decision {
    use crate::approval::Decision;
    // The card gives up after 600s; stop a little later so the two agree.
    for _ in 0..6_100 {
        if !poll(Duration::from_millis(100)).unwrap_or(false) {
            continue;
        }
        let Ok(event) = read() else { return Decision::Deny };
        let Event::Key(key) = event else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return Decision::Deny;
        }
        return match key.code {
            KeyCode::Char(ch) => Decision::from_key(ch),
            KeyCode::Esc => Decision::Deny,
            _ => Decision::Deny,
        };
    }
    Decision::Deny
}

fn draw(buffer: &str, cursor: usize, mode: &Mode, colors: Color) {
    if mode == &Mode::Busy {
        return;
    }
    clear_row();
    let shown = buffer.replace('\n', "\n  ");
    let prompt = paint(colors, "meta", PROMPT);
    print!("{prompt}{shown}");
    // Park the cursor at its index (single-line editing; multiline buffers place
    // it on the last row, which is where typing continues).
    let after = buffer.chars().skip(cursor).count();
    if after > 0 {
        print!("\x1b[{after}D");
    }
    let _ = io::stdout().flush();
}

fn clear_row() {
    print!("\r\x1b[K");
    let _ = io::stdout().flush();
}

fn clear_row_stderr() {
    eprint!("\r\x1b[K");
    let _ = io::stderr().flush();
}

/// Enter the REPL.
pub async fn chat(args: ChatArgs) -> Result<commands::CommandOutcome> {
    let tty = io::stdout().is_terminal() && io::stdin().is_terminal();
    let store = Arc::new(Store::open(&commands::store_path(&args)).await?);
    let mut cfg = commands::chat_config(&args)?;
    let workspace = commands::workspace_for(&cfg, &args.workspace)?;
    std::fs::create_dir_all(&workspace)?;
    let colors = Color::detect(tty && transcript::ui_enabled(tty));
    let mut session = load_session(&store, &args).await?;
    let mut verbose = args.verbose;
    let mut last_run: Option<RunId> =
        session.turns.last().and_then(|turn| RunId::parse(&turn.run_id).ok());
    let mut last_result = String::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let allow = commands::allow_list();
    // Shared with the approval card: the key reader stands down while a
    // prompt is on screen, and the spinner yields the row to it.
    let quiet = Arc::new(AtomicBool::new(false));
    let mut reader = if tty {
        spawn_tty_reader(Arc::clone(&quiet))?
    } else {
        spawn_line_reader()
    };
    let banner = |session: &Session, cfg: &RunConfig, verbose: bool| {
        print!(
            "{}",
            transcript::Transcript::new(colors, verbose)
                .banner(&chat_header(session, cfg, &workspace))
                .join("\n")
        );
        println!();
        let _ = io::stdout().flush();
    };
    banner(&session, &cfg, verbose);
    if tty {
        println!(
            "{}",
            paint(
                colors,
                "meta",
                "  /help for commands \u{00b7} ctrl+c interrupts \u{00b7} ctrl+d quits"
            )
        );
    }

    if let Some(goal) = args.goal.clone() {
        queue.push_back(goal);
    }
    if args.oneshot || !tty {
        // Piped stdin feeds the queue; `--oneshot` takes exactly one goal.
        if queue.is_empty() {
            while let Some(action) = reader.rx.recv().await {
                match action {
                    Action::Submit(text) => {
                        queue.push_back(text);
                        if args.oneshot {
                            break;
                        }
                    }
                    Action::Eof => break,
                    _ => {}
                }
            }
        }
        while let Some(goal) = queue.pop_front() {
            // A piped '/command' line is a command, the same as at a prompt.
            if let Some(command) = parse_slash(&goal) {
                match handle(
                    command, &store, &mut cfg, &workspace, &mut session, &mut verbose,
                    &mut reader, last_run.as_ref(), &last_result, &mut queue, colors, &allow,
                    args.resume.clone(),
                )
                .await
                {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(error) => eprintln!("{} {error:#}", paint(colors, "fail", "\u{2717}")),
                }
                continue;
            }
            let outcome = run_turn(
                &store, &cfg, &workspace, &session, &goal, verbose, &allow, &quiet, None,
                &mut queue, args.context_chars,
            )
            .await;
            match outcome {
                Ok(outcome) => last_result = remember(&mut session, &mut last_run, goal, &outcome),
                Err(error) => eprintln!("{} {error:#}", paint(colors, "fail", "\u{2717}")),
            }
            if args.oneshot {
                break;
            }
        }
        save_session(&store, &session).await?;
        return Ok(commands::CommandOutcome::plain(last_result, true));
    }

    loop {
        let goal = match queue.pop_front() {
            Some(goal) => Some(goal),
            None => loop {
                let _ = reader.mode.send(Mode::Prompt);
                match reader.rx.recv().await {
                    Some(Action::Submit(text)) | Some(Action::Queued(text)) => break Some(text),
                    Some(Action::Interrupt) => {
                        println!("{}", paint(colors, "meta", "  ctrl+d to leave"));
                    }
                    Some(Action::Eof) => break None,
                    Some(Action::Redraw) => banner(&session, &cfg, verbose),
                    Some(Action::ToggleVerbose) => {
                        verbose = !verbose;
                        note(
                            colors,
                            &format!("transcript detail: {}", if verbose { "on" } else { "off" }),
                        );
                    }
                    None => break None,
                }
            },
        };
        let Some(text) = goal else { break };
        if let Some(command) = parse_slash(&text) {
            match handle(
                command, &store, &mut cfg, &workspace, &mut session, &mut verbose, &mut reader,
                last_run.as_ref(), &last_result, &mut queue, colors, &allow, args.resume.clone(),
            )
            .await
            {
                Ok(true) => break,
                Ok(false) => {}
                Err(error) => eprintln!("{} {error:#}", paint(colors, "fail", "\u{2717}")),
            }
            continue;
        }
        let _ = reader.mode.send(Mode::Busy);
        let outcome = run_turn(
            &store, &cfg, &workspace, &session, &text, verbose, &allow, &quiet, Some(&mut reader),
            &mut queue, args.context_chars,
        )
        .await;
        let _ = reader.mode.send(Mode::Prompt);
        match outcome {
            Ok(outcome) => last_result = remember(&mut session, &mut last_run, text, &outcome),
            Err(error) => eprintln!("{} {error:#}", paint(colors, "fail", "\u{2717}")),
        }
        save_session(&store, &session).await?;
    }
    save_session(&store, &session).await?;
    Ok(commands::CommandOutcome::plain(last_result, true))
}

fn note(colors: Color, text: &str) {
    println!("{}", paint(colors, "meta", &format!("  {text}")));
    let _ = io::stdout().flush();
}

fn chat_header(session: &Session, cfg: &RunConfig, workspace: &std::path::Path) -> Header {
    Header {
        run_id: session.id.clone(),
        goal: "chat \u{00b7} enter a goal, /help for commands".to_string(),
        provider: cfg.provider.to_string(),
        model: cfg.model.clone(),
        workspace: workspace.display().to_string(),
        approvals: cfg.tools.approval.to_string(),
        agents: cfg.agents,
    }
}

fn remember(session: &mut Session, last_run: &mut Option<RunId>, goal: String, outcome: &TurnOutcome) -> String {
    *last_run = Some(outcome.run_id.clone());
    session.push(Turn {
        goal,
        status: outcome.status.to_string(),
        summary: outcome.summary.clone(),
        run_id: outcome.run_id.to_string(),
    });
    session.totals.add(outcome);
    outcome.summary.clone()
}

#[allow(clippy::too_many_arguments)]
async fn run_turn(
    store: &Arc<Store>,
    cfg: &RunConfig,
    workspace: &std::path::Path,
    session: &Session,
    goal: &str,
    verbose: bool,
    allow: &Arc<AllowList>,
    quiet: &Arc<AtomicBool>,
    reader: Option<&mut Reader>,
    queue: &mut VecDeque<String>,
    context_chars: usize,
) -> Result<TurnOutcome> {
    let context = session.render(context_chars);
    let objective = if context.is_empty() {
        goal.to_string()
    } else {
        format!("{context}{goal}")
    };
    let current: Arc<Mutex<Option<RunId>>> = Arc::new(Mutex::new(None));
    let drive = Box::pin(commands::drive_turn(
        store,
        cfg,
        workspace,
        &objective,
        RunMode::Coding,
        None,
        true,
        verbose,
        allow,
        Arc::clone(quiet),
        Arc::clone(&current),
    ));
    tokio::pin!(drive);
    // Without a prompt to read (piped stdin, `--oneshot`) just drive to the end.
    let Some(reader) = reader else { return drive.await };
    // Hermes-style steering: goals typed during the turn are queued and run
    // next. Ctrl+C drops this future, which stops the turn where it stands --
    // the run itself is durable, so the message says how to pick it up.
    loop {
        tokio::select! {
            result = &mut drive => return result,
            action = reader.rx.recv() => match action {
                Some(Action::Interrupt) => {
                    let id = current
                        .lock()
                        .ok()
                        .and_then(|slot| slot.as_ref().map(|id| id.to_string()));
                    return Err(match id {
                        Some(id) => anyhow!(
                            "turn interrupted \u{00b7} run {id} is in the store: /resume {id}"
                        ),
                        None => anyhow!("turn interrupted"),
                    });
                }
                Some(Action::Queued(text)) | Some(Action::Submit(text)) => queue.push_back(text),
                Some(_) => {}
                None => {}
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle(
    command: Slash,
    store: &Arc<Store>,
    cfg: &mut RunConfig,
    workspace: &std::path::Path,
    session: &mut Session,
    verbose: &mut bool,
    reader: &mut Reader,
    last_run: Option<&RunId>,
    last_result: &str,
    queue: &mut VecDeque<String>,
    colors: Color,
    allow: &Arc<AllowList>,
    resume: Option<String>,
) -> Result<bool> {
    let note = |text: String| println!("{}", paint(colors, "meta", &format!("  {text}")));
    let run_id = |explicit: Option<String>, last: Option<&RunId>| -> Result<String> {
        explicit.or_else(|| last.map(|id| id.to_string())).ok_or_else(|| {
            anyhow::anyhow!("no run in this session yet; pass a run id")
        })
    };
    match command {
        Slash::Help => print!("{}", indent(HELP)),
        Slash::Exit => return Ok(true),
        Slash::New => {
            *session = Session::with_id();
            allow.clear();
            note(format!("new session {}", session.id));
        }
        Slash::Clear => {
            session.turns.clear();
            session.digest = None;
            let dropped = std::mem::take(queue).len();
            note(if dropped == 0 {
                "conversation cleared".into()
            } else {
                format!("conversation cleared, {dropped} queued goal(s) dropped")
            });
        }
        Slash::Compact => {
            let folded = session.compact(3);
            note(if folded == 0 {
                "nothing to compact".into()
            } else {
                format!("folded {folded} turn(s) into a digest")
            });
        }
        Slash::Verbose(next) => {
            *verbose = next.unwrap_or(!*verbose);
            note(format!("transcript detail: {}", if *verbose { "on" } else { "off" }));
        }
        Slash::Cost => {
            let totals = session.totals;
            note(format!(
                "session {} \u{00b7} {} turn(s) \u{00b7} {} tokens \u{00b7} {} inference \u{00b7} {} tool calls \u{00b7} {:.1}s",
                session.id,
                session.turns.len(),
                totals.tokens,
                totals.inferences,
                totals.tool_calls,
                totals.seconds
            ));
        }
        Slash::Status(run) => {
            echo(inspect(store, "status", run_id(run, last_run)?).await?);
        }
        Slash::Tasks(run) => {
            echo(inspect(store, "tasks", run_id(run, last_run)?).await?);
        }
        Slash::Logs(run) => {
            echo(inspect(store, "logs", run_id(run, last_run)?).await?);
        }
        Slash::Runs => {
            for run in store.list_runs().await?.into_iter().take(10) {
                note(format!(
                    "{}  {:<10}  {}",
                    run.id,
                    run.status.to_string(),
                    clip(&run.objective, 60)
                ));
            }
        }
        Slash::Tools => {
            let names = commands::tool_names(cfg, workspace)?;
            note(format!("tools ({}): {}", names.len(), names.join(", ")));
        }
        Slash::Copy => {
            if last_result.is_empty() {
                note("no result yet".into());
            } else {
                println!("{}", last_result);
            }
        }
        Slash::History => {
            if session.turns.is_empty() {
                note("no turns yet".into());
            }
            for (index, turn) in session.turns.iter().enumerate() {
                note(format!(
                    "{}. [{}] {}  {}",
                    index + 1,
                    turn.status,
                    clip(&turn.goal, 60),
                    paint(colors, "meta", &format!("({})", turn.run_id))
                ));
            }
        }
        Slash::Allow(name) => {
            if name.is_empty() {
                let names = allow.names();
                note(if names.is_empty() {
                    "no session-allowed tools".into()
                } else {
                    format!("session-allowed tools: {}", names.join(", "))
                });
            } else {
                allow.allow(&name);
                note(format!("approvals: {name} allowed for this session"));
            }
        }
        Slash::AllowAll => {
            cfg.tools.approval = ApprovalPolicy::Never;
            note("approvals: everything allowed for this session".into());
        }
        Slash::Resume(id) => {
            let id = run_id(shelf(&id).or(resume), last_run)?;
            let run = store.load_run_required(&RunId::parse(&id)?).await?;
            let summary = commands::run_summary(store, &run.id).await?;
            session.push(Turn {
                goal: run.objective.clone(),
                status: run.status.to_string(),
                summary: summary.clone(),
                run_id: run.id.to_string(),
            });
            note(format!("resumed {} ({})", run.id, run.status));
        }
        Slash::Model(model) => {
            if model.is_empty() {
                note(format!("model: {}", cfg.model));
            } else {
                cfg.model = model.clone();
                if let Some(selected) =
                    cfg.providers.iter_mut().find(|p| p.name == cfg.provider)
                {
                    selected.model = model.clone();
                }
                note(format!("model -> {model}"));
            }
        }
        Slash::Agents(n) => {
            if n == 0 {
                note(format!("agents: {}", cfg.agents));
            } else {
                cfg.agents = n;
                cfg.budget.max_inference_calls = cfg.budget.max_inference_calls.max(u64::from(n) * 40);
                note(format!("agents -> {n}"));
            }
        }
        Slash::Approve(policy) => {
            cfg.tools.approval = policy;
            if policy == ApprovalPolicy::Never {
                allow.clear();
            }
            note(format!("approvals -> {policy}"));
        }
        Slash::Unknown(name) => {
            note(format!("unknown command /{name} \u{00b7} /help lists commands"));
        }
    }
    let _ = reader.mode.send(Mode::Prompt);
    Ok(false)
}

fn indent(text: &str) -> String {
    text.lines().map(|line| format!("  {line}\n")).collect()
}

fn echo(text: String) {
    for line in text.lines() {
        println!("  {line}");
    }
    let _ = io::stdout().flush();
}

async fn inspect(store: &Arc<Store>, verb: &str, run_id: String) -> Result<String> {
    commands::inspect_text(store, verb, run_id).await
}

async fn load_session(store: &Store, args: &ChatArgs) -> Result<Session> {
    if let Some(id) = &args.resume {
        let run = store.load_run_required(&RunId::parse(id)?).await?;
        let summary = commands::run_summary(store, &run.id).await?;
        let mut session = Session::with_id();
        session.push(Turn {
            goal: run.objective.clone(),
            status: run.status.to_string(),
            summary,
            run_id: run.id.to_string(),
        });
        return Ok(session);
    }
    if args.continue_last
        && let Some(run) = store.list_runs().await?.into_iter().next()
    {
        let summary = commands::run_summary(store, &run.id).await?;
        let mut session = Session::with_id();
        session.push(Turn {
            goal: run.objective.clone(),
            status: run.status.to_string(),
            summary,
            run_id: run.id.to_string(),
        });
        return Ok(session);
    }
    if !args.new
        && let Some(json) = store.kv_get(KV_NAMESPACE, KV_ACTIVE).await?
        && let Some(session) = Session::from_json(&json)
    {
        return Ok(session);
    }
    Ok(Session::with_id())
}

async fn save_session(store: &Store, session: &Session) -> Result<()> {
    store.kv_set(KV_NAMESPACE, KV_ACTIVE, &session.to_json()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(goal: &str, summary: &str) -> Turn {
        Turn {
            goal: goal.to_string(),
            status: "completed".to_string(),
            summary: summary.to_string(),
            run_id: "0".repeat(26),
        }
    }

    #[test]
    fn slash_commands_cover_the_session_surface() {
        assert_eq!(
            parse_slash("/allow write_file"),
            Some(Slash::Allow("write_file".to_string()))
        );
        assert_eq!(parse_slash("/allow"), Some(Slash::Allow(String::new())));
        assert_eq!(parse_slash("/allow-all"), Some(Slash::AllowAll));
        assert_eq!(parse_slash("/history"), Some(Slash::History));
        assert_eq!(parse_slash("/runs"), Some(Slash::Runs));
        assert_eq!(
            parse_slash("/approve never"),
            Some(Slash::Approve(ApprovalPolicy::Never))
        );
        assert_eq!(parse_slash("write hello.txt"), None);
        assert_eq!(parse_slash("/nope"), Some(Slash::Unknown("nope".to_string())));
    }

    #[test]
    fn tab_completes_unique_and_ambiguous_prefixes() {
        assert_eq!(
            complete_slash("/allow-a"),
            Completion::Text("/allow-all".to_string())
        );
        assert!(matches!(complete_slash("/allow"), Completion::Candidates(_)));
        assert_eq!(complete_slash("/zzz"), Completion::None);
        assert_eq!(complete_slash("no slash"), Completion::None);
    }

    #[test]
    fn prompt_history_walks_and_searches() {
        let mut history = History::default();
        assert!(history.is_empty());
        history.push("first goal");
        history.push("second goal");
        history.push("first goal again");
        history.push("first goal again");
        assert_eq!(history.len(), 3, "consecutive repeats are not recorded twice");
        assert_eq!(history.prev(), Some("first goal again"));
        assert_eq!(history.prev(), Some("second goal"));
        assert_eq!(history.next(), Some("first goal again"));
        assert_eq!(history.search("second"), Some("second goal"));
        assert_eq!(history.search("nothing like it"), None);
    }

    #[test]
    fn empty_session_adds_no_context() {
        assert_eq!(Session::new("s").render(4000), "");
    }

    #[test]
    fn context_lists_turns_oldest_first() {
        let mut session = Session::new("s");
        session.push(turn("first", "one"));
        session.push(turn("second", "two"));
        let text = session.render(4000);
        assert!(text.starts_with("Conversation so far"));
        assert!(text.find("first") < text.find("second"), "{text}");
        assert!(text.ends_with("New request: "));
        assert!(text.contains("[completed]"));
    }

    #[test]
    fn context_trims_oldest_turns_first() {
        let mut session = Session::new("s");
        for index in 0..30 {
            session.push(turn(&format!("goal {index}"), &"x".repeat(60)));
        }
        let text = session.render(400);
        assert!(text.chars().count() <= 420, "{} chars", text.chars().count());
        assert!(text.contains("goal 29"), "newest turn must survive");
        assert!(!text.contains("goal 0\n"), "oldest turn must be trimmed");
    }

    #[test]
    fn compact_folds_old_turns_into_a_digest() {
        let mut session = Session::new("s");
        for index in 0..6 {
            session.push(turn(&format!("goal {index}"), "done"));
        }
        assert_eq!(session.compact(3), 3);
        assert_eq!(session.turns.len(), 3);
        assert!(session.digest.as_deref().unwrap().contains("goal 0"));
        assert!(session.render(4000).contains("Earlier turns:"));
        assert_eq!(session.compact(3), 0, "nothing left to fold");
    }

    #[test]
    fn session_round_trips_through_the_store_format() {
        let mut session = Session::new("s-7");
        session.push(turn("goal", "summary"));
        session.totals.tokens = 42;
        let decoded = Session::from_json(&session.to_json()).unwrap();
        assert_eq!(decoded, session);
    }

    #[test]
    fn goals_are_not_commands() {
        assert_eq!(parse_slash("write hello.txt"), None);
        assert_eq!(parse_slash("1/2 of the file"), None);
    }

    #[test]
    fn slash_commands_parse() {
        assert_eq!(parse_slash("/help"), Some(Slash::Help));
        assert_eq!(parse_slash("  /quit  "), Some(Slash::Exit));
        assert_eq!(parse_slash("/model gpt-5.1"), Some(Slash::Model("gpt-5.1".into())));
        assert_eq!(parse_slash("/agents 3"), Some(Slash::Agents(3)));
        assert_eq!(
            parse_slash("/approve all"),
            Some(Slash::Approve(ApprovalPolicy::AskAll))
        );
        assert_eq!(
            parse_slash("/approve auto"),
            Some(Slash::Approve(ApprovalPolicy::Never))
        );
        assert_eq!(
            parse_slash("/verbose off"),
            Some(Slash::Verbose(Some(false)))
        );
        assert_eq!(parse_slash("/verbose"), Some(Slash::Verbose(None)));
        assert_eq!(
            parse_slash("/status run_1"),
            Some(Slash::Status(Some("run_1".into())))
        );
        assert_eq!(parse_slash("/wat"), Some(Slash::Unknown("wat".into())));
    }

    #[test]
    fn tab_completion_completes_and_lists() {
        assert_eq!(complete_slash("/cop"), Completion::Text("/copy".into()));
        assert_eq!(complete_slash("/com"), Completion::Text("/compact".into()));
        let ambiguous = complete_slash("/c");
        match ambiguous {
            Completion::Candidates(list) => {
                assert!(list.contains(&"/clear".to_string()));
                assert!(list.contains(&"/compact".to_string()));
            }
            other => panic!("expected candidates, got {other:?}"),
        }
        let all = complete_slash("/");
        match all {
            Completion::Candidates(list) => {
                assert!(list.contains(&"/help".to_string()));
                assert!(list.contains(&"/compact".to_string()));
            }
            other => panic!("expected candidates, got {other:?}"),
        }
        assert_eq!(complete_slash("plain text"), Completion::None);
    }

    #[test]
    fn history_browses_and_searches() {
        let mut history = History::default();
        history.push("alpha");
        history.push("beta");
        history.push("beta");
        assert_eq!(history.len(), 2, "consecutive duplicates collapse");
        assert_eq!(history.prev(), Some("beta"));
        assert_eq!(history.prev(), Some("alpha"));
        assert_eq!(history.prev(), None);
        assert_eq!(history.next(), Some("beta"));
        assert_eq!(history.search("alp"), Some("alpha"));
        assert_eq!(history.search("zzz"), None);
    }

    #[test]
    fn editing_helpers_track_the_cursor() {
        let mut buffer = String::from("hello.txt");
        let mut cursor = buffer.chars().count();
        insert(&mut buffer, &mut cursor, " more");
        assert_eq!(buffer, "hello.txt more");
        assert_eq!(cursor, 14);
        remove(&mut buffer, &mut cursor, 1);
        assert_eq!(buffer, "hello.txt mor");
        assert_eq!(cursor, 13);
        cursor = 9;
        drop_word(&mut buffer, &mut cursor);
        // Ctrl+W erases the word before the cursor, like a shell.
        assert_eq!(buffer, " mor");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn clip_flattens_and_marks_long_text() {
        assert_eq!(clip("one\ntwo", 20), "onetwo");
        assert_eq!(clip("abcdefghij", 5), "abcd\u{2026}");
    }
}
