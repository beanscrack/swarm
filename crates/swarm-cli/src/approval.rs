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

    /// One key press to a decision (pure, so the mapping is testable).
    pub fn from_key(ch: char) -> Self {
        match ch {
            'y' | 'Y' => Self::AllowOnce,
            'a' | 'A' => Self::AllowAlways,
            _ => Self::Deny,
        }
    }
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
                let colors = Color::detect(true);
                let card = format!(
                    "\n\u{23fa} {}({})\n  {} {}\n  approve? [y/N/a] ",
                    request.name,
                    args_summary(&request.arguments),
                    paint(colors, "meta", "risk"),
                    request.risk,
                );
                eprint!("{}", card.replace('\n', "\r\n"));
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
        // Claude Code-style card: the exact call, its risk, then the question.
        let colors = Color::detect(true);
        let card = format!(
            "\n\u{23fa} {}({})\n  {} {}\n  approve? [y/N/a] ",
            request.name,
            args_summary(&request.arguments),
            paint(colors, "meta", "risk"),
            request.risk,
        );
        eprint!("{}", card.replace('\n', "\r\n"));
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
        let answer = match decision {
            Decision::Deny => "denied".to_string(),
            Decision::AllowOnce => "approved".to_string(),
            Decision::AllowAlways => {
                self.allow.allow(&request.name);
                format!("approved (always: {})", request.name)
            }
        };
        eprintln!("{}", format!("  \u{23bf} {answer}").replace('\n', "\r\n"));
        let _ = io::stderr().flush();
        decision.approved()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AllowList, ConsoleApproval, Decision, Decision::*, KeyRequest, install_key_bus, key_bus,
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
