//! Live transcript for a run: tails the run's event log while the runtime
//! appends to it and hands rendered rows to the UI.
//!
//! This module never touches the terminal. Rows go to the [`UiHandle`] (the
//! single owner of the screen, or plain stdout when there is no terminal) and
//! progress goes to its status line, so the spinner, the transcript and a
//! permission dialog can never fight over a row.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use swarm_core::event::EventKind;
use swarm_core::state::{StateMachine, TaskState};
use swarm_core::RunId;
use swarm_store::Store;
use tokio::task::JoinHandle;

use crate::transcript::{Header, Transcript, TurnStats};
use crate::ui::{Status, UiHandle};

const POLL: Duration = Duration::from_millis(100);
/// Task counts cost a query, so they refresh about once a second.
const TASK_EVERY: u32 = 10;

/// A running tail of one run's event log.
pub struct LiveTail {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<TurnStats>>,
}

impl LiveTail {
    /// Optionally print the run banner, then tail events in the background.
    pub fn start(
        store: Arc<Store>,
        run_id: RunId,
        header: Option<&Header>,
        ui: UiHandle,
        verbose: bool,
    ) -> Self {
        if let Some(header) = header {
            ui.print(Transcript::new(ui.colors(), verbose).banner(header));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let join = {
            let stop = Arc::clone(&stop);
            tokio::spawn(async move { Self::pump(store, run_id, ui, verbose, stop).await })
        };
        Self { stop, join: Some(join) }
    }

    async fn pump(
        store: Arc<Store>,
        run_id: RunId,
        ui: UiHandle,
        verbose: bool,
        stop: Arc<AtomicBool>,
    ) -> TurnStats {
        let mut transcript = Transcript::new(ui.colors(), verbose);
        let mut cursor = 0u64;
        let mut activity = "starting";
        let mut tokens = 0u64;
        let mut tasks: Option<(u32, u32)> = None;
        let mut ticks = 0u32;
        loop {
            let events = store.events_since(&run_id, cursor, 200).await.unwrap_or_default();
            transcript.set_width(crate::style::term_width());
            let mut changed = !events.is_empty();
            let mut rows = Vec::new();
            for event in &events {
                cursor = event.seq;
                let number = |key: &str| event.payload.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0);
                tokens += number("input_tokens") + number("output_tokens");
                rows.extend(transcript.lines(event));
                activity = match event.kind {
                    EventKind::InferenceStarted | EventKind::InferenceCompleted => "thinking",
                    EventKind::ToolRequested => "running tool",
                    EventKind::ApprovalRequested => "waiting for approval",
                    EventKind::Message => "finishing",
                    _ => "working",
                };
            }
            ui.print(rows);
            ticks += 1;
            if ticks % TASK_EVERY == 1 {
                if let Ok(list) = store.list_tasks(&run_id).await {
                    let done = list.iter().filter(|task| task.state == TaskState::Completed).count() as u32;
                    let next = Some((done, list.len() as u32));
                    changed |= next != tasks;
                    tasks = next;
                }
            }
            if changed || ticks == 1 {
                ui.status(Status { activity: activity.to_string(), tokens, tasks });
            }
            let finished = ticks.is_multiple_of(5)
                && matches!(
                    store.load_run_required(&run_id).await.map(|run| run.status),
                    Ok(status) if status.is_terminal()
                );
            if (finished && events.is_empty()) || stop.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        ui.print(transcript.drain());
        transcript.stats().clone()
    }

    /// Stop the tail, let it flush its last events, and hand back what the turn did.
    pub async fn finish(mut self) -> TurnStats {
        self.stop.store(true, Ordering::Relaxed);
        match self.join.take() {
            Some(join) => tokio::time::timeout(Duration::from_secs(5), join)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default(),
            None => TurnStats::default(),
        }
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

    async fn store_with_run() -> (tempfile::TempDir, Arc<Store>, RunId) {
        let temp = tempfile::TempDir::new().unwrap();
        let store = Arc::new(Store::open(&temp.path().join("store")).await.unwrap());
        let run_id = RunId::new();
        let mut run = Run::new("goal", "mock", Some("mock".to_string()), 1, true, ResourceBudget::default(), 0);
        run.id = run_id.clone();
        run.status = RunStatus::Running;
        store.create_run(&run).await.unwrap();
        (temp, store, run_id)
    }

    /// Through the real store: events appended while a tail is live reach the
    /// stats, and the tail stops once the run is terminal.
    #[tokio::test]
    async fn tail_streams_appended_events_and_stops_on_completion() {
        let (_temp, store, run_id) = store_with_run().await;
        let tail = LiveTail::start(
            Arc::clone(&store),
            run_id.clone(),
            None,
            UiHandle::plain(crate::style::Color::Plain),
            false,
        );
        store
            .append_event(&RunEvent::new(
                EventKind::ToolResult,
                run_id.clone(),
                serde_json::json!({"name":"read_file","success":true,"truncated":false,
                    "arguments":{"path":"a.txt"},"output":"one\ntwo"}),
            ))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(700)).await;
        let mut running = store.load_run_required(&run_id).await.unwrap();
        running.status = RunStatus::Completed;
        store.update_run(&running).await.unwrap();
        let stats = tail.finish().await;
        assert_eq!(stats.reads, 1, "the tail rendered the event it saw");
    }

    #[tokio::test]
    async fn an_idle_tail_can_be_stopped_at_once() {
        let (_temp, store, run_id) = store_with_run().await;
        let tail = LiveTail::start(store, run_id, None, UiHandle::plain(crate::style::Color::Plain), false);
        let started = std::time::Instant::now();
        let stats = tail.finish().await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(stats, TurnStats::default());
    }
}
