//! Live transcript streaming for `swarm run`: tails the run's event log while
//! the runtime appends to it, renders the transcript, and shows a Claude
//! Code-style status line while the model is thinking.
//!
//! Transcript lines go to stdout (pipes and redirects stay clean); the status
//! line is the only stderr traffic and only on a TTY. The status line yields to
//! the approval prompt through the shared `quiet` flag.

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use swarm_core::event::EventKind;
use swarm_core::{RunId, RunStatus};
use swarm_store::Store;
use tokio::task::JoinHandle;

use crate::transcript::{Color, Header, Transcript, paint};

const FRAMES: [&str; 10] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{283e}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{2803}",
];
/// The status row is padded to this width so a shorter line wipes the longer
/// line it replaces.
const ROW: usize = 100;

/// Wipe the in-place status row (the approval prompt calls this before printing
/// its card so no spinner fragments are left behind).
pub fn clear_line() {
    if crate::transcript::ui_enabled(std::io::stderr().is_terminal()) {
        eprint!("\r{}\r", " ".repeat(ROW));
        let _ = std::io::stderr().flush();
    }
}

/// In-place status line: `⠹ thinking · 12s · 300 tokens`.
struct Spinner {
    quiet: Arc<AtomicBool>,
    started: Instant,
    shown: bool,
}

impl Spinner {
    fn new(quiet: Arc<AtomicBool>) -> Self {
        Self {
            quiet,
            started: Instant::now(),
            shown: false,
        }
    }

    fn tick(&mut self, activity: &str, tokens: u64) {
        if self.quiet.load(Ordering::Relaxed) {
            self.clear();
            return;
        }
        let frame = FRAMES[(self.started.elapsed().as_millis() / 120) as usize % FRAMES.len()];
        let mut text = format!(
            "{frame} {activity} \u{00b7} {}s",
            self.started.elapsed().as_secs()
        );
        if tokens > 0 {
            text.push_str(&format!(" \u{00b7} {tokens} tokens"));
        }
        let pad = " ".repeat(ROW.saturating_sub(text.chars().count()));
        eprint!("\r{}\r", paint(Color::Ansi, "meta", &text) + &pad);
        let _ = std::io::stderr().flush();
        self.shown = true;
    }

    fn clear(&mut self) {
        if self.shown {
            clear_line();
            self.shown = false;
        }
    }
}

/// A running tail of one run's event log.
pub struct LiveTail {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl LiveTail {
    /// Print the run banner, then tail events in the background.
    ///
    /// `quiet` is the flag the caller shares with the approval prompt, so the
    /// status row and the approval card never fight over the same line.
    pub fn start_shared(
        store: Arc<Store>,
        run_id: RunId,
        header: &Header,
        tty: bool,
        verbose: bool,
        quiet: Arc<AtomicBool>,
    ) -> Self {
        let transcript = Transcript::new(Color::detect(tty), verbose);
        for line in transcript.banner(header) {
            println!("{line}");
        }
        let _ = std::io::stdout().flush();
        let stop = Arc::new(AtomicBool::new(false));
        let join = {
            let (stop, quiet) = (Arc::clone(&stop), Arc::clone(&quiet));
            tokio::spawn(async move { Self::pump(store, run_id, tty, verbose, stop, quiet).await })
        };
        Self {
            stop,
            join: Some(join),
        }
    }

    async fn pump(
        store: Arc<Store>,
        run_id: RunId,
        tty: bool,
        verbose: bool,
        stop: Arc<AtomicBool>,
        quiet: Arc<AtomicBool>,
    ) {
        let mut transcript = Transcript::new(Color::detect(tty), verbose);
        let mut spinner = Spinner::new(Arc::clone(&quiet));
        let mut cursor = 0u64;
        let mut activity = "starting";
        let mut tokens = 0u64;
        let mut ticks = 0u32;
        loop {
            let events = store
                .events_since(&run_id, cursor, 200)
                .await
                .unwrap_or_default();
            let mut printed = false;
            for event in &events {
                cursor = event.seq;
                let number = |key: &str| {
                    event
                        .payload
                        .get(key)
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0)
                };
                tokens += number("input_tokens") + number("output_tokens");
                let lines = transcript.lines(event);
                if lines.is_empty() {
                    continue;
                }
                spinner.clear();
                for line in lines {
                    println!("{line}");
                }
                let _ = std::io::stdout().flush();
                printed = true;
                activity = match event.kind {
                    EventKind::InferenceStarted | EventKind::InferenceCompleted => "thinking",
                    EventKind::ToolRequested => "running tool",
                    EventKind::ApprovalRequested => "waiting for approval",
                    EventKind::Message => "finishing",
                    _ => "working",
                };
            }
            ticks += 1;
            let finished = ticks.is_multiple_of(5)
                && matches!(
                    store.load_run_required(&run_id).await.map(|run| run.status),
                    Ok(RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled)
                );
            if (finished && events.is_empty()) || stop.load(Ordering::Relaxed) {
                break;
            }
            if tty && !printed && events.is_empty() {
                spinner.tick(activity, tokens);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        spinner.clear();
        for line in transcript.drain() {
            println!("{line}");
        }
        let _ = std::io::stdout().flush();
    }

    /// Stop the tail and let it flush its last events.
    pub async fn finish(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), join).await;
        }
        clear_line();
        let _ = std::io::stdout().flush();
    }
}

impl Drop for LiveTail {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            join.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::RunStatus;
    use swarm_core::budget::ResourceBudget;
    use swarm_core::event::RunEvent;
    use swarm_core::run::Run;

    /// Through the real store: events appended while a tail is live must reach
    /// the transcript, and the tail must stop once the run reaches a terminal
    /// state.
    #[tokio::test]
    async fn tail_streams_appended_events_and_stops_on_completion() {
        let temp = tempfile::TempDir::new().unwrap();
        let store = Arc::new(Store::open(&temp.path().join("store")).await.unwrap());
        let run_id = RunId::new();
        let mut run = Run::new(
            "goal",
            "mock",
            Some("mock".to_string()),
            1,
            true,
            ResourceBudget::default(),
            0,
        );
        run.id = run_id.clone();
        run.status = RunStatus::Running;
        store.create_run(&run).await.unwrap();
        let header = Header {
            run_id: run_id.to_string(),
            goal: "goal".into(),
            provider: "mock".into(),
            model: "mock".into(),
            workspace: ".".into(),
            approvals: "ask_all".into(),
            agents: 1,
        };
        // The session shares this flag with the approval card; a plain run hands
        // it a flag nobody raises.
        let quiet = Arc::new(AtomicBool::new(false));
        let tail = LiveTail::start_shared(
            Arc::clone(&store),
            run_id.clone(),
            &header,
            false,
            false,
            Arc::clone(&quiet),
        );
        assert!(!quiet.load(Ordering::SeqCst));
        store
            .append_event(&RunEvent::new(
                EventKind::ToolRequested,
                run_id.clone(),
                serde_json::json!({"name":"write_file","arguments":{"path":"a.txt"}}),
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(700)).await;
        let mut running = store.load_run_required(&run_id).await.unwrap();
        running.status = RunStatus::Completed;
        store.update_run(&running).await.unwrap();
        tail.finish().await;
    }
}
