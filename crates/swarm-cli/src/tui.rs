//! Read-only terminal dashboard backed by the durable store.

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use swarm_core::RunId;
use swarm_core::agent::Agent;
use swarm_core::budget::BudgetUsage;
use swarm_core::event::RunEvent;
use swarm_core::run::Run;
use swarm_core::task::Task;
use swarm_store::Store;

const REFRESH: Duration = Duration::from_millis(500);

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
            disable_raw_mode()?;
            return Err(error.into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

struct Snapshot {
    run: Run,
    usage: BudgetUsage,
    tasks: Vec<Task>,
    agents: Vec<Agent>,
    events: Vec<RunEvent>,
}

async fn snapshot(store: &Store, id: &RunId) -> Result<Snapshot> {
    Ok(Snapshot {
        run: store.load_run_required(id).await?,
        usage: store.load_stats(id).await?.usage,
        tasks: store.list_tasks(id).await?,
        agents: store.agents_for_run(id).await?,
        events: store.latest_events(id, 200).await?,
    })
}

/// Open the dashboard. Tab and Shift-Tab change runs; Q exits.
pub async fn run(store_path: PathBuf, requested_id: Option<String>) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("swarm tui requires an interactive terminal");
    }
    let store = Store::open(&store_path).await?;
    let mut runs = store.list_runs().await?;
    if runs.is_empty() {
        bail!("no runs found in {}", store.root().display());
    }
    let pinned = requested_id.is_some();
    let mut selected = match requested_id {
        Some(id) => RunId::parse(&id)?,
        None => runs[0].id.clone(),
    };
    if !runs.iter().any(|run| run.id == selected) {
        return Err(anyhow!(
            "run {selected} not found in {}",
            store.root().display()
        ));
    }
    let mut current = snapshot(&store, &selected).await?;

    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    let mut refreshed = Instant::now();
    loop {
        let position = runs.iter().position(|run| run.id == selected).unwrap_or(0);
        terminal.draw(|frame| render(frame, &current, position, runs.len(), pinned))?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Tab | KeyCode::Right if !pinned => {
                            selected = runs[(position + 1) % runs.len()].id.clone();
                            current = snapshot(&store, &selected).await?;
                        }
                        KeyCode::BackTab | KeyCode::Left if !pinned => {
                            selected = runs[(position + runs.len() - 1) % runs.len()].id.clone();
                            current = snapshot(&store, &selected).await?;
                        }
                        KeyCode::Char('r') => refreshed = Instant::now() - REFRESH,
                        _ => {}
                    }
                }
            }
        }
        if refreshed.elapsed() >= REFRESH {
            runs = store.list_runs().await?;
            current = snapshot(&store, &selected).await?;
            refreshed = Instant::now();
        }
    }
    Ok(())
}

fn render(frame: &mut Frame<'_>, current: &Snapshot, position: usize, total: usize, pinned: bool) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(5),
            Constraint::Percentage(45),
            Constraint::Min(5),
        ])
        .split(area);
    let navigation = if pinned {
        "Q quit · R refresh"
    } else {
        "Tab/←/→ change run · Q quit · R refresh"
    };
    let title = format!(
        "SWARM  run {}/{}  {}  {}",
        position + 1,
        total,
        current.run.status,
        navigation
    );
    frame.render_widget(
        Paragraph::new(title).style(Style::default().fg(Color::Cyan)),
        rows[0],
    );

    let usage = &current.usage;
    let summary = format!(
        "Objective: {}\nProvider: {}  Model: {}  Agents requested: {}\nInference: {}  Tokens: {}  Tools: {}  Spend: ${:.6}",
        current.run.objective,
        current.run.provider,
        current.run.model.as_deref().unwrap_or("—"),
        current.run.agent_target,
        usage.inference_calls,
        usage.total_tokens(),
        usage.tool_calls,
        usage.spend_micros as f64 / 1_000_000.0,
    );
    frame.render_widget(
        Paragraph::new(summary).wrap(Wrap { trim: true }).block(
            Block::default()
                .title(format!("Run {}", current.run.id))
                .borders(Borders::ALL),
        ),
        rows[1],
    );

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(rows[2]);
    render_tasks(frame, columns[0], &current.tasks);
    render_agents(frame, columns[1], &current.agents);
    render_events(frame, rows[3], &current.events);
}

fn render_tasks(frame: &mut Frame<'_>, area: Rect, tasks: &[Task]) {
    let text = tasks
        .iter()
        .map(|task| format!("{}  {}  {}", task.state, task.role, task.title))
        .collect::<Vec<_>>()
        .join("\n");
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .title(format!("Tasks ({})", tasks.len()))
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_agents(frame: &mut Frame<'_>, area: Rect, agents: &[Agent]) {
    let text = agents
        .iter()
        .map(|agent| format!("{}  {}  {} steps", agent.state, agent.role, agent.steps))
        .collect::<Vec<_>>()
        .join("\n");
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .title(format!("Agents ({})", agents.len()))
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn render_events(frame: &mut Frame<'_>, area: Rect, events: &[RunEvent]) {
    let visible = area.height.saturating_sub(2) as usize;
    let text = events
        .iter()
        .rev()
        .take(visible)
        .rev()
        .map(|event| format!("{:>5} {:<20} {}", event.seq, event.kind, event.payload))
        .collect::<Vec<_>>()
        .join("\n");
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .title("Recent events")
                .borders(Borders::ALL),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use swarm_core::budget::ResourceBudget;

    #[test]
    fn renders_dashboard_at_normal_and_small_sizes() {
        let current = Snapshot {
            run: Run::new(
                "make a thing",
                "mock",
                None,
                1,
                true,
                ResourceBudget::default(),
                0,
            ),
            usage: BudgetUsage::default(),
            tasks: Vec::new(),
            agents: Vec::new(),
            events: Vec::new(),
        };
        for (width, height) in [(100, 30), (25, 8)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render(frame, &current, 0, 1, false))
                .unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("SWARM"));
        }
    }
}
