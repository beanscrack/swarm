//! Serialized terminal prompts for agent tool approvals.

use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use async_trait::async_trait;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use swarm_runtime::{ApprovalHandler, ApprovalRequest};
use tokio::sync::Mutex;

#[derive(Default)]
pub struct ConsoleApproval {
    prompt_lock: Mutex<()>,
}

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

#[async_trait]
impl ApprovalHandler for ConsoleApproval {
    async fn approve(&self, request: &ApprovalRequest) -> bool {
        let _lock = self.prompt_lock.lock().await;
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
        let arguments = serde_json::to_string_pretty(&request.arguments)
            .unwrap_or_else(|_| request.arguments.to_string());
        let prompt = format!(
            "\nApproval requested for run {} task {}\nTool: {}\nArguments:\n{}\nRisk: {}\nApprove this call? [y/N]\n",
            request.run_id, request.task_id, request.name, arguments, request.risk,
        );
        eprint!("{}", prompt.replace('\n', "\r\n"));
        let _ = io::stderr().flush();
        let decision = loop {
            match event::poll(Duration::ZERO) {
                Ok(true) => match event::read() {
                    Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        break matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'))
                            && !key.modifiers.contains(KeyModifiers::CONTROL);
                    }
                    Ok(_) => {}
                    Err(_) => break false,
                },
                Ok(false) => tokio::time::sleep(Duration::from_millis(50)).await,
                Err(_) => break false,
            }
        };
        drop(raw);
        eprintln!("{}", if decision { "Approved" } else { "Denied" });
        decision
    }
}
