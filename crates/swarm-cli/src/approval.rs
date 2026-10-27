//! Serialized terminal prompts for agent tool approvals.

use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, is_raw_mode_enabled};
use swarm_runtime::{ApprovalHandler, ApprovalRequest};
use tokio::sync::{Mutex as AsyncMutex, mpsc};

use crate::live::clear_line;
use crate::transcript::{Color, args_summary, paint};

/// Tools the user approved for the rest of the session (`a` at the prompt),
/// Claude Code's "yes, and don't ask again for this tool".
#[derive(Default)]
pub struct AllowList(Mutex<HashSet<String>>);

impl AllowList {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    pub fn allow(&self, name: &str) {
        if let Ok(mut set) = self.0.lock() {
            set.insert(name.to_string());
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.0.lock().map(|set| set.contains(name)).unwrap_or(false)
    }

    pub fn names(&self) -> Vec<String> {
        self.0
            .lock()
            .map(|set| set.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    }

    pub fn clear(&self) {
        if let Ok(mut set) = self.0.lock() {
            set.clear();
        }
    }
}

/// A request for one key press, answered by the thread that owns the keyboard.
pub type KeyRequest = mpsc::UnboundedSender<Decision>;

/// The prompt reader's key bus. A terminal session has exactly one reader
/// thread, so a process-wide slot is enough, and it keeps the approval card
/// from opening a second console reader that would race it for input.
static KEY_BUS: std::sync::OnceLock<mpsc::UnboundedSender<KeyRequest>> = std::sync::OnceLock::new();

pub fn install_key_bus(tx: mpsc::UnboundedSender<KeyRequest>) {
    let _ = KEY_BUS.set(tx);
}

pub fn key_bus() -> Option<&'static mpsc::UnboundedSender<KeyRequest>> {
    KEY_BUS.get()
}

/// The answer to one approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Deny,
    AllowOnce,
    AllowAlways,
}

impl Decision {
    pub fn approved(self) -> bool {
        self != Decision::Deny
    }

    /// One key press to a decision (pure, so the mapping is testable). Number
    /// keys pick the matching row of the panel; `y`/`a` keep the old one-keystroke
    /// shorthand; anything else denies, matching the capital-D default below.
    pub fn from_key(ch: char) -> Self {
        match ch {
            'y' | 'Y' | '1' => Self::AllowOnce,
            'a' | 'A' | '2' => Self::AllowAlways,
            '3' | 'n' | 'N' => Self::Deny,
            _ => Self::Deny,
        }
    }
}

/// The rows of the Claude Code permission dialog: box, the exact call, the
/// numbered choices with `❯` on the cursor row, then the reason.
pub fn panel_lines(
    title: &str,
    command: &str,
    choices: &[&str],
    description: &str,
    cursor: usize,
    width: usize,
) -> Vec<String> {
    // Claude Code sizes the panel 46..76 cells and clamps to the terminal.
    let inner = width.clamp(46, 76).saturating_sub(4).max(24);
    let total = inner + 4;
    let row = |body: &str| {
        // Never let a long line push the right border out of alignment.
        let body: std::borrow::Cow<'_, str> = if body.chars().count() > inner {
            std::borrow::Cow::Owned(body.chars().take(inner).collect())
        } else {
            std::borrow::Cow::Borrowed(body)
        };
        format!("\u{2502} {body:<inner$} \u{2502}")
    };
    let mut out = Vec::new();

    let head = format!("\u{256d}\u{2500} {title} ");
    let mut top = head.chars().take(total.saturating_sub(1)).collect::<String>();
    top.push_str(&"\u{2500}".repeat(total.saturating_sub(top.chars().count() + 1)));
    top.push('\u{256e}');
    out.push(top);

    let mut wrapped = Vec::new();
    for line in command.lines() {
        for chunk in wrap(line, inner.saturating_sub(2)) {
            wrapped.push(chunk);
        }
    }
    let hidden = wrapped.len().saturating_sub(4);
    wrapped.truncate(4);
    for line in wrapped {
        out.push(row(&line));
    }
    if hidden > 0 {
        out.push(row(&format!(
            "\u{2026} {hidden} more lines \u{00b7} ctrl+o shows detail"
        )));
    }
    out.push(row(""));
    for (index, choice) in choices.iter().enumerate() {
        let mark = if index == cursor { "\u{276f}" } else { " " };
        out.push(row(&format!(
            "{mark} {}. {:<cw$}",
            index + 1,
            choice,
            cw = inner.saturating_sub(5)
        )));
    }
    if !description.is_empty() {
        out.push(row(""));
        for line in wrap(description, inner.saturating_sub(2)).into_iter().take(6) {
            out.push(row(&line));
        }
    }
    out.push(format!(
        "\u{2570}{}\u{256f}",
        "\u{2500}".repeat(total.saturating_sub(2))
    ));
    out
}

/// Naive word wrap that never splits a word shorter than the width.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let mut word = word;
        // A word wider than the panel (a long path, a base64 blob) gets broken.
        while word.chars().count() > width {
            let head: String = word.chars().take(width).collect();
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            lines.push(head.clone());
            word = &word[head.len()..];
        }
        let room = current.chars().count();
        if room == 0 {
            current.push_str(word);
        } else if room + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// The numbered rows, in Claude Code's wording.
pub const CHOICES: [&str; 3] = [
    "Yes, allow once",
    "Yes, allow for this session",
    "No, deny this call",
];

/// The full card, coloured, ready to print. Shared by the raw-mode path and
/// the key-bus path so an interactive session and a bare `swarm run` show the
/// same dialog.
fn card(request: &ApprovalRequest) -> String {
    let width = crossterm::terminal::size()
        .map(|(cols, _)| cols as usize)
        .unwrap_or(76);
    let colors = Color::detect(true);
    let title = format!("\u{26a0}  Allow {}?", request.name);
    let command = format!("{}({})", request.name, args_summary(&request.arguments));
    let body = panel_lines(&title, &command, &CHOICES, &request.risk, 0, width);
    let mut out = String::from("\n");
    for (index, line) in body.iter().enumerate() {
        let painted = if index == 0 {
            paint(colors, "warn", line)
        } else if line.contains('\u{276f}') {
            paint(colors, "brand", line)
        } else {
            paint(colors, "meta", line)
        };
        out.push_str(&painted);
        out.push('\n');
    }
    out.push_str(&paint(
        colors,
        "meta",
        "  1/2/3 to choose \u{00b7} y/a/n shorthand \u{00b7} Esc denies",
    ));
    // The card is printed with `eprint!`, so it owns its own line ending: without
    // this the next transcript row lands on the hint row (`...Esc denies● ...`).
    out.push('\n');
    out
}

pub struct ConsoleApproval {
    prompt_lock: AsyncMutex<()>,
    quiet: Arc<AtomicBool>,
    allow: Arc<AllowList>,
}

impl Default for ConsoleApproval {
    fn default() -> Self {
        Self::new(Arc::new(AtomicBool::new(false)))
    }
}

impl ConsoleApproval {
    /// `quiet` is shared with the live tail so the status line yields the row.
    pub fn new(quiet: Arc<AtomicBool>) -> Self {
        Self {
            prompt_lock: AsyncMutex::new(()),
            quiet,
            allow: AllowList::new(),
        }
    }

    /// Share the session-wide allow list so `/approve` and `/new` can reset it.
    pub fn with_list(quiet: Arc<AtomicBool>, allow: Arc<AllowList>) -> Self {
        Self {
            prompt_lock: AsyncMutex::new(()),
            quiet,
            allow,
        }
    }
}

struct RawModeGuard {
    was_enabled: bool,
}

impl RawModeGuard {
    fn enter() -> io::Result<Self> {
        let was_enabled = is_raw_mode_enabled().unwrap_or(false);
        if !was_enabled {
            enable_raw_mode()?;
        }
        Ok(Self { was_enabled })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // Restore, never unconditionally disable: an interactive session keeps
        // raw mode enabled for its own prompt reader.
        if !self.was_enabled && is_raw_mode_enabled().unwrap_or(false) {
            let _ = disable_raw_mode();
        }
    }
}

/// Restores the spinner after the prompt finishes, on every exit path.
struct QuietGuard(Arc<AtomicBool>);

impl Drop for QuietGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[async_trait]
impl ApprovalHandler for ConsoleApproval {
    async fn approve(&self, request: &ApprovalRequest) -> bool {
        let _lock = self.prompt_lock.lock().await;
        if self.allow.contains(&request.name) {
            return true;
        }
        self.quiet.store(true, Ordering::SeqCst);
        let _quiet = QuietGuard(Arc::clone(&self.quiet));
        clear_line();
        // In an interactive session the prompt reader owns raw mode and stdin;
        // asking it is the only way not to race it for keystrokes.
        if let Some(bus) = key_bus() {
            let (reply_tx, mut reply_rx) = mpsc::unbounded_channel();
            if bus.send(reply_tx).is_ok() {
                clear_line();
                eprint!("{}", card(request).replace('\n', "\r\n"));
                let _ = io::stderr().flush();
                let decided = tokio::time::timeout(Duration::from_secs(600), reply_rx.recv()).await;
                return self
                    .resolve(
                        request,
                        decided.ok().flatten().unwrap_or(Decision::Deny),
                    )
                    .await;
            }
        }
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            eprintln!(
                "tool {} requires approval, but no interactive terminal is available; denied",
                request.name
            );
            return false;
        }
        let raw = match RawModeGuard::enter() {
            Ok(raw) => raw,
            Err(error) => {
                eprintln!("could not read approval: {error}; denied");
                return false;
            }
        };
        // Discard input entered before the user sees this particular request.
        while matches!(event::poll(Duration::ZERO), Ok(true)) {
            if event::read().is_err() {
                return false;
            }
        }
        // Claude Code-style panel: the exact call, its risk, numbered choices.
        eprint!("{}", card(request).replace('\n', "\r\n"));
        let _ = io::stderr().flush();
        let decision = loop {
            match event::poll(Duration::from_millis(50)) {
                Ok(true) => match event::read() {
                    Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            break Decision::Deny;
                        }
                        match key.code {
                            KeyCode::Char(ch) => break Decision::from_key(ch),
                            KeyCode::Esc => break Decision::Deny,
                            _ => {}
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break Decision::Deny,
                },
                Ok(false) => {}
                Err(_) => break Decision::Deny,
            }
        };
        drop(raw);
        self.resolve(request, decision).await
    }
}

impl ConsoleApproval {
    async fn resolve(&self, request: &ApprovalRequest, decision: Decision) -> bool {
        if matches!(decision, Decision::AllowAlways) {
            self.allow.allow(&request.name);
        }
        // The transcript renders the `ApprovalResolved` event as `⎿ approved`, so
        // echoing it here too printed the row twice. The card already ended its own
        // line, so nothing more is printed.
        decision.approved()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AllowList, CHOICES, ConsoleApproval, Decision, Decision::*, KeyRequest, install_key_bus,
        key_bus, panel_lines, wrap,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use swarm_core::{AgentId, RunId, TaskId};
    use swarm_runtime::{ApprovalHandler, ApprovalRequest};

    /// The card no longer opens its own console reader: it asks the prompt
    /// reader for the key, so the two cannot race for stdin.
    #[tokio::test]
    async fn the_prompt_reader_answers_the_card() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<KeyRequest>();
        if key_bus().is_some() {
            return; // another test in this process already owns the keyboard
        }
        install_key_bus(tx);
        let quiet = Arc::new(AtomicBool::new(false));
        let allow = AllowList::new();
        let approval = ConsoleApproval::with_list(Arc::clone(&quiet), Arc::clone(&allow));
        let answering = tokio::spawn(async move {
            while let Some(reply) = rx.recv().await {
                let _ = reply.send(AllowAlways);
            }
        });
        let request = ApprovalRequest {
            run_id: RunId::new(),
            task_id: TaskId::new(),
            agent_id: AgentId::new(),
            call_id: "call_1".to_string(),
            name: "write_file".to_string(),
            arguments: serde_json::json!({"path": "a.txt", "content": "hi"}),
            risk: "writes a.txt".to_string(),
        };
        assert!(approval.approve(&request).await, "the reader answers 'a'");
        assert!(allow.contains("write_file"), "'a' pins the tool");
        assert!(
            approval.approve(&request).await,
            "the next call is answered by the allow list"
        );
        assert!(!quiet.load(Ordering::SeqCst), "the card gives the terminal back");
        answering.abort();
    }

    #[test]
    fn approval_keys_map_to_decisions() {
        assert_eq!(Decision::from_key('y'), Decision::AllowOnce);
        assert_eq!(Decision::from_key('Y'), Decision::AllowOnce);
        assert_eq!(Decision::from_key('a'), Decision::AllowAlways);
        assert_eq!(Decision::from_key('n'), Decision::Deny);
        assert_eq!(Decision::from_key(' '), Decision::Deny);
        assert_eq!(Decision::from_key('\r'), Decision::Deny);
        assert!(Decision::AllowAlways.approved());
        assert!(!Decision::Deny.approved());
    }

    #[test]
    fn number_keys_pick_the_numbered_row() {
        assert_eq!(Decision::from_key('1'), Decision::AllowOnce);
        assert_eq!(Decision::from_key('2'), Decision::AllowAlways);
        assert_eq!(Decision::from_key('3'), Decision::Deny);
        assert_eq!(CHOICES.len(), 3, "three rows, three number keys");
    }

    #[test]
    fn the_panel_is_a_box_with_numbered_choices() {
        let lines = panel_lines(
            "\u{26a0}  Allow write_file?",
            "write_file({\"path\": \"a.txt\"})",
            &CHOICES,
            "writes a.txt",
            0,
            60,
        );
        assert!(lines[0].starts_with("\u{256d}\u{2500} \u{26a0}"), "{lines:?}");
        assert!(lines[0].ends_with('\u{256e}'));
        assert!(lines.last().unwrap().starts_with('\u{2570}'));
        assert!(lines.last().unwrap().ends_with('\u{256f}'));
        // Every row is the same printed width: a real box, not a suggestion.
        let widths: Vec<usize> = lines.iter().map(|line| line.chars().count()).collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "ragged panel: {widths:?}"
        );
        let body = lines.join("\n");
        assert!(body.contains("\u{276f} 1. Yes, allow once"), "{body}");
        assert!(body.contains("  2. Yes, allow for this session"));
        assert!(body.contains("  3. No, deny this call"));
        assert!(body.contains("writes a.txt"));
    }

    #[test]
    fn the_cursor_moves() {
        let third = panel_lines("t", "c", &CHOICES, "", 2, 60).join("\n");
        assert!(third.contains("\u{276f} 3. No, deny this call"), "{third}");
        assert!(!third.contains("\u{276f} 1."), "cursor should be on row 3");
    }

    #[test]
    fn a_long_command_is_capped_with_a_hint() {
        let long = "x".repeat(400);
        let body = panel_lines("t", &long, &CHOICES, "", 0, 60).join("\n");
        assert!(body.contains("more lines"), "{body}");
        assert!(body.contains("ctrl+o shows detail"), "{body}");
    }

    #[test]
    fn wrapping_keeps_words_intact() {
        let lines = wrap("allow this tool to write files under the workspace", 12);
        assert!(lines.iter().all(|line| line.chars().count() <= 12), "{lines:?}");
        assert!(lines.iter().any(|line| line == "write files"), "{lines:?}");
    }

    #[test]
    fn allow_list_tracks_tools() {
        let allow = AllowList::new();
        assert!(!allow.contains("write_file"));
        allow.allow("write_file");
        assert!(allow.contains("write_file"));
        assert!(!allow.contains("run_command"));
        assert_eq!(allow.names(), vec!["write_file".to_string()]);
        allow.clear();
        assert!(!allow.contains("write_file"));
    }
}
