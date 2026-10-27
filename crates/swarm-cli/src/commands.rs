use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use swarm_core::config::{ApprovalPolicy, RunConfig};
use swarm_core::event::RunEvent;
use swarm_core::run::{Run, RunMode};
use swarm_core::{ProviderId, RunId, RunStatus};
use swarm_runtime::{Runtime, build_registry, local_provider_config, scheduler_config};
use swarm_scheduler::Scheduler;
use swarm_store::Store;
use swarm_tools::{SandboxConfig, ToolExecutor};

use crate::approval::AllowList;
use crate::transcript;

#[derive(Debug, Parser)]
#[command(
    name = "swarm",
    version,
    about = "Run and inspect durable LLM agent tasks"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the interactive session (the default with no subcommand).
    #[command(visible_alias = "c")]
    Chat(ChatArgs),
    /// Execute a goal with the selected provider.
    Run(RunArgs),
    /// Drive an unfinished run to completion (durable recovery).
    Resume(ResumeArgs),
    /// Show a run's state and resource usage.
    Status(InspectArgs),
    /// List the tasks in a run.
    Tasks(InspectArgs),
    /// List the agents in a run.
    Agents(InspectArgs),
    /// Show recent events, optionally following new ones.
    Logs(LogsArgs),
    /// Open a live dashboard for runs, tasks, agents, and events.
    Tui(InspectArgs),
}

#[derive(Debug, Args)]
pub struct StoreArgs {
    #[arg(long, default_value = ".swarm")]
    store: PathBuf,
}

#[derive(Debug, Args)]
struct RunArgs {
    goal: String,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    agents: Option<u32>,
    #[command(flatten)]
    store: StoreArgs,
    #[arg(long, default_value = "./work")]
    workspace: PathBuf,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    approve_all: bool,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    base_url: Option<String>,
    /// Print only the final one-line summary (no live transcript).
    #[arg(long)]
    plain: bool,
    /// Show full transcript detail (agent states, checkpoints, long replies).
    #[arg(long)]
    verbose: bool,
}

#[derive(Debug, Args)]
struct InspectArgs {
    run_id: Option<String>,
    #[command(flatten)]
    store: StoreArgs,
}

/// Arguments for the interactive session.
#[derive(Debug, Args)]
pub struct ChatArgs {
    /// First goal to run; the session stays open afterwards.
    pub goal: Option<String>,
    #[command(flatten)]
    pub store: StoreArgs,
    #[arg(long, default_value = "./work")]
    pub workspace: PathBuf,
    #[arg(long)]
    pub provider: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub base_url: Option<String>,
    #[arg(long)]
    pub agents: Option<u32>,
    #[arg(long)]
    pub approve_all: bool,
    /// Load a run's goal and result into the session as prior context.
    #[arg(long)]
    pub resume: Option<String>,
    /// Continue from the most recent run in the store.
    #[arg(long = "continue")]
    pub continue_last: bool,
    /// Start a fresh session instead of the stored one.
    #[arg(long)]
    pub new: bool,
    /// Run the goal and exit (no prompt).
    #[arg(long)]
    pub oneshot: bool,
    /// Compact transcript.
    #[arg(long)]
    pub plain: bool,
    /// Show full transcript detail.
    #[arg(long)]
    pub verbose: bool,
    /// How much of the session context travels with each goal.
    #[arg(long, default_value_t = 4000)]
    pub context_chars: usize,
}

/// Arguments for resuming an unfinished run.
#[derive(Debug, Args)]
struct ResumeArgs {
    pub run_id: String,
    #[command(flatten)]
    store: StoreArgs,
    #[arg(long, default_value = "./work")]
    workspace: PathBuf,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long)]
    agents: Option<u32>,
    #[arg(long)]
    approve_all: bool,
    #[arg(long)]
    plain: bool,
    #[arg(long)]
    verbose: bool,
}

#[derive(Debug, Args)]
struct LogsArgs {
    run_id: Option<String>,
    #[command(flatten)]
    store: StoreArgs,
    #[arg(long)]
    follow: bool,
    /// Replay the run as the live transcript (⏺ actions, ⎿ results) instead of
    /// raw event lines.
    #[arg(long)]
    transcript: bool,
    /// Include low-signal transcript rows (agent states, checkpoints, long text).
    #[arg(long)]
    verbose: bool,
}

/// Text to display and the process success status of a command.
pub struct CommandOutcome {
    pub text: String,
    pub success: bool,
}

impl CommandOutcome {
    fn success(text: String) -> Self {
        Self {
            text,
            success: true,
        }
    }

    pub(crate) fn plain(text: String, success: bool) -> Self {
        Self { text, success }
    }
}

/// `swarm` with no subcommand: the interactive session on a TTY, help otherwise.
fn default_chat_args() -> ChatArgs {
    ChatArgs {
        goal: None,
        store: StoreArgs { store: PathBuf::from(".swarm") },
        workspace: PathBuf::from("./work"),
        provider: None,
        model: None,
        base_url: None,
        agents: None,
        approve_all: false,
        resume: None,
        continue_last: false,
        new: false,
        oneshot: false,
        plain: false,
        verbose: false,
        context_chars: 4000,
    }
}

pub async fn execute(cli: Cli) -> Result<CommandOutcome> {
    match cli.command {
        None => {
            if io::stdout().is_terminal() && io::stdin().is_terminal() {
                crate::chat::chat(default_chat_args()).await
            } else {
                use clap::CommandFactory;
                Ok(CommandOutcome::plain(
                    Cli::command().render_help().to_string(),
                    true,
                ))
            }
        }
        Some(Command::Chat(args)) => crate::chat::chat(args).await,
        Some(Command::Run(args)) => run(args).await,
        Some(Command::Resume(args)) => resume(args).await,
        Some(Command::Status(args)) => status(args).await,
        Some(Command::Tasks(args)) => tasks(args).await,
        Some(Command::Agents(args)) => agents(args).await,
        Some(Command::Logs(args)) => logs(args).await,
        Some(Command::Tui(args)) => {
            crate::tui::run(args.store.store, args.run_id).await?;
            Ok(CommandOutcome::success(String::new()))
        }
    }
}

fn run_config(args: &RunArgs) -> Result<RunConfig> {
    let mut cfg: RunConfig = if let Some(path) = &args.config {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        toml::from_str(&source).with_context(|| format!("parse config {}", path.display()))?
    } else {
        RunConfig::mock(&args.goal, args.agents.unwrap_or(1))
    };
    cfg.objective = args.goal.clone();
    if let Some(agents) = args.agents {
        if agents == 0 {
            bail!("--agents must be at least 1");
        }
        cfg.agents = agents;
    }
    apply_provider_overrides(
        &mut cfg,
        args.provider.as_deref(),
        args.model.as_deref(),
        args.base_url.as_deref(),
        &args.workspace,
        args.approve_all,
    )?;
    Ok(cfg)
}

/// Apply `--provider/--model/--base-url/--approve-all` to a config. Shared by
/// `swarm run` and the interactive session so both resolve providers the same
/// way (presets, workspace binding, zero-cost policy).
fn apply_provider_overrides(
    cfg: &mut RunConfig,
    provider: Option<&str>,
    model: Option<&str>,
    base_url: Option<&str>,
    workspace: &Path,
    approve_all: bool,
) -> Result<()> {
    if let Some(name) = provider {
        let preset = match name {
            "local" => Some(local_provider_config()),
            "chatgpt-web" => Some(swarm_runtime::chatgpt_web_provider_config()),
            "gemini-web" => Some(swarm_runtime::gemini_web_provider_config()),
            _ => None,
        };
        if let Some(selected) = preset {
            cfg.provider = selected.name.clone();
            cfg.model = selected.model.clone();
            cfg.providers.retain(|p| p.name != selected.name);
            cfg.zero_cost = selected.zero_cost;
            cfg.scheduler.zero_cost = selected.zero_cost;
            cfg.providers.push(selected);
        } else {
            cfg.provider = ProviderId::new(name.to_string())?;
        }
    }

    let selected_index = cfg
        .providers
        .iter()
        .position(|p| p.name == cfg.provider)
        .ok_or_else(|| anyhow!("provider '{}' is not configured", cfg.provider))?;
    let selected = &mut cfg.providers[selected_index];
    if selected.kind == swarm_core::provider::ProviderKind::ManagedAgent
        && selected.workspace_dir.is_none()
    {
        selected.workspace_dir = Some(workspace.to_path_buf());
    }
    if let Some(model) = model {
        selected.model = model.to_string();
        cfg.model = model.to_string();
    } else {
        cfg.model = selected.model.clone();
    }
    if let Some(url) = base_url {
        if !matches!(
            selected.kind,
            swarm_core::provider::ProviderKind::OpenAiCompatible
                | swarm_core::provider::ProviderKind::GeminiWeb
                | swarm_core::provider::ProviderKind::ChatGptWeb
        ) {
            bail!("--base-url requires an OpenAI-compatible or browser-chat provider");
        }
        selected.base_url = Some(url.to_string());
    }
    if !cfg.may_run(&cfg.providers[selected_index]) {
        bail!(
            "provider '{}' is disallowed by the zero-cost policy",
            cfg.provider
        );
    }
    if approve_all {
        cfg.tools.approval = ApprovalPolicy::Never;
    }
    Ok(())
}

/// Session settings from `swarm chat` flags: a mock config (no goal yet, each
/// turn supplies one) with the same provider resolution as `swarm run`.
pub(crate) fn chat_config(args: &ChatArgs) -> Result<RunConfig> {
    let agents = args.agents.unwrap_or(1);
    if agents == 0 {
        bail!("--agents must be at least 1");
    }
    let mut cfg = RunConfig::mock("", agents);
    cfg.objective = String::new();
    apply_provider_overrides(
        &mut cfg,
        args.provider.as_deref(),
        args.model.as_deref(),
        args.base_url.as_deref(),
        &args.workspace,
        args.approve_all,
    )?;
    Ok(cfg)
}

/// Store root for a chat invocation (`StoreArgs` is private to this module).
pub(crate) fn store_path(args: &ChatArgs) -> PathBuf {
    args.store.store.clone()
}

/// Where tools run: a managed-agent provider pins its own workspace.
pub(crate) fn workspace_for(cfg: &RunConfig, fallback: &Path) -> Result<PathBuf> {
    let selected = cfg
        .providers
        .iter()
        .find(|provider| provider.name == cfg.provider)
        .ok_or_else(|| anyhow!("provider '{}' is not configured", cfg.provider))?;
    Ok(if selected.kind == swarm_core::provider::ProviderKind::ManagedAgent {
        selected
            .workspace_dir
            .clone()
            .unwrap_or_else(|| fallback.to_path_buf())
    } else {
        fallback.to_path_buf()
    })
}

/// The tools the model may ask for, for `/tools`.
pub(crate) fn tool_names(cfg: &RunConfig, workspace: &Path) -> Result<Vec<String>> {
    let sandbox = SandboxConfig::new(workspace)
        .command_timeout(Duration::from_secs(cfg.tools.command_timeout_secs))
        .output_cap_bytes(cfg.tools.max_output_bytes)
        .allow_network(cfg.tools.allow_network);
    Ok(ToolExecutor::new(sandbox).tool_names())
}

/// The session-wide allow list (`/allow`, and `a` on an approval prompt).
pub(crate) fn allow_list() -> Arc<AllowList> {
    AllowList::new()
}

/// What one finished turn produced, in the units the session adds up.
#[derive(Debug, Clone)]
pub(crate) struct TurnOutcome {
    pub run_id: RunId,
    pub status: RunStatus,
    /// The final assistant summary, empty when the run produced none.
    pub summary: String,
    /// Set when the turn did not complete.
    pub error: Option<String>,
    pub tasks: u32,
    pub tokens: u64,
    pub inferences: u64,
    pub tool_calls: u64,
    pub seconds: f64,
}

impl TurnOutcome {
    pub(crate) fn succeeded(&self) -> bool {
        self.status == RunStatus::Completed
    }

    pub(crate) fn footer(&self) -> String {
        transcript::footer(
            &self.status.to_string(),
            self.seconds,
            u64::from(self.tasks),
            self.inferences,
            self.tokens,
            self.tool_calls,
        )
    }
}

/// Run one goal to completion: create a durable run, show the live transcript,
/// wire the console approval handler, drive the scheduler, collect the numbers.
///
/// `quiet` is shared with the approval card (and the session's key reader, so
/// it stops stealing keystrokes while a prompt is up). `current` receives the
/// run id as soon as it exists: if the caller drops this future (Ctrl+C) the
/// run stays in the store and `/resume <id>` picks it up.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drive_turn(
    store: &Arc<Store>,
    cfg: &RunConfig,
    workspace: &Path,
    objective: &str,
    mode: RunMode,
    existing: Option<&RunId>,
    live: bool,
    verbose: bool,
    allow: &Arc<AllowList>,
    quiet: Arc<AtomicBool>,
    current: Arc<Mutex<Option<RunId>>>,
) -> Result<TurnOutcome> {
    let mut cfg = cfg.clone();
    cfg.objective = objective.to_string();
    let registry = Arc::new(build_registry(&cfg)?);
    if registry.find(&cfg.provider).is_none() {
        bail!("provider '{}' could not be started", cfg.provider);
    }
    let scheduler = Scheduler::new(scheduler_config(&cfg, &registry));
    let sandbox = SandboxConfig::new(workspace)
        .command_timeout(Duration::from_secs(cfg.tools.command_timeout_secs))
        .output_cap_bytes(cfg.tools.max_output_bytes)
        .allow_network(cfg.tools.allow_network);
    let executor = Arc::new(ToolExecutor::new(sandbox));
    let runtime = Runtime::new(store.clone(), registry, executor, scheduler)
        .with_approval(cfg.tools.approval);
    let created = match existing {
        // Resuming continues the run that is already in the store; a new goal
        // creates one. The scheduler picks up persisted task state either way.
        Some(id) => store.load_run_required(id).await?,
        None => runtime.create_run(&cfg, mode).await?,
    };
    if let Ok(mut slot) = current.lock() {
        *slot = Some(created.id.clone());
    }

    // Claude Code-style live view: banner, streamed transcript, one footer line.
    // Piped output and `--plain` keep the original one-line summary.
    let tty = transcript::ui_enabled(io::stdout().is_terminal() && io::stderr().is_terminal());
    let header = transcript::Header {
        run_id: created.id.to_string(),
        goal: created.objective.clone(),
        provider: created.provider.clone(),
        model: created.model.clone().unwrap_or_else(|| cfg.model.clone()),
        workspace: workspace.display().to_string(),
        approvals: cfg.tools.approval.to_string(),
        agents: created.agent_target,
    };
    let tail = (tty && live).then(|| {
        crate::live::LiveTail::start_shared(
            store.clone(),
            created.id.clone(),
            &header,
            tty,
            verbose,
            Arc::clone(&quiet),
        )
    });
    let runtime = if cfg.tools.approval == ApprovalPolicy::Never {
        runtime
    } else {
        runtime.with_approval_handler(Arc::new(
            crate::approval::ConsoleApproval::with_list(Arc::clone(&quiet), Arc::clone(allow)),
        ))
    };
    let started = Instant::now();
    let state = runtime.drive(&created.id).await?;
    if let Some(tail) = tail {
        tail.finish().await;
    }
    summarize_turn(store, &created.id, state, started.elapsed().as_secs_f64()).await
}

/// Collect the transcript-relevant numbers and final text for one run.
pub(crate) async fn summarize_turn(
    store: &Store,
    id: &RunId,
    status: RunStatus,
    seconds: f64,
) -> Result<TurnOutcome> {
    let tasks = store.finished_task_count(id).await?;
    let stats = store.load_stats(id).await?;
    let events = store.latest_events(id, 100).await?;
    let message = |key: &str| {
        events
            .iter()
            .rev()
            .find_map(|event| event.payload.get(key).and_then(|v| v.as_str()).map(str::to_owned))
    };
    Ok(TurnOutcome {
        run_id: id.clone(),
        status,
        summary: message("summary").unwrap_or_default(),
        error: if status == RunStatus::Completed {
            None
        } else {
            message("error")
        },
        tasks,
        tokens: stats.usage.total_tokens(),
        inferences: stats.usage.inference_calls,
        tool_calls: stats.usage.tool_calls,
        seconds,
    })
}

/// One line of context for the session: what a run was asked and what it said.
pub(crate) async fn run_summary(store: &Store, id: &RunId) -> Result<String> {
    let run = store.load_run_required(id).await?;
    let turn = summarize_turn(store, id, run.status, 0.0).await?;
    let body = turn
        .summary
        .lines()
        .collect::<Vec<_>>()
        .join(" ");
    Ok(if body.is_empty() {
        match turn.error {
            Some(error) => format!("{}: {error}", run.status),
            None => run.status.to_string(),
        }
    } else {
        body
    })
}

async fn run(args: RunArgs) -> Result<CommandOutcome> {
    let cfg = run_config(&args)?;
    let workspace = workspace_for(&cfg, &args.workspace)?;
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("create workspace {}", workspace.display()))?;
    let store = Arc::new(Store::open(&args.store.store).await?);
    let tty = transcript::ui_enabled(io::stdout().is_terminal() && io::stderr().is_terminal());
    let outcome = drive_turn(
        &store,
        &cfg,
        &workspace,
        &args.goal,
        RunMode::Coding,
        None,
        !args.plain,
        args.verbose,
        &allow_list(),
        Arc::new(AtomicBool::new(false)),
        Arc::new(Mutex::new(None)),
    )
    .await?;
    Ok(CommandOutcome {
        text: outcome_text(&outcome, args.plain || !tty),
        success: outcome.succeeded(),
    })
}

/// Continue an unfinished run: same transcript, same approvals, no new run.
async fn resume(args: ResumeArgs) -> Result<CommandOutcome> {
    let store = Arc::new(Store::open(&args.store.store).await?);
    let id = RunId::parse(&args.run_id)?;
    let run = store.load_run_required(&id).await?;
    if matches!(run.status, RunStatus::Completed | RunStatus::Cancelled) {
        bail!(
            "run {} is already {} \u{00b7} nothing to resume",
            run.id,
            run.status
        );
    }
    // Rebuild the run's settings: the stored provider name resolves through the
    // same preset path as `--provider`, and the stored budget is respected.
    let mut cfg = RunConfig::mock(&run.objective, run.agent_target.max(1));
    apply_provider_overrides(
        &mut cfg,
        args.provider.as_deref().or(Some(run.provider.as_str())),
        args.model.as_deref(),
        args.base_url.as_deref(),
        &args.workspace,
        args.approve_all,
    )?;
    cfg.model = run.model.clone().unwrap_or_else(|| cfg.model.clone());
    cfg.budget = run.budget;
    cfg.zero_cost = run.zero_cost;
    cfg.scheduler.zero_cost = run.zero_cost;
    let workspace = workspace_for(&cfg, &args.workspace)?;
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("create workspace {}", workspace.display()))?;
    let tty = transcript::ui_enabled(io::stdout().is_terminal() && io::stderr().is_terminal());
    let outcome = drive_turn(
        &store,
        &cfg,
        &workspace,
        &run.objective,
        run.mode,
        Some(&id),
        !args.plain,
        args.verbose,
        &allow_list(),
        Arc::new(AtomicBool::new(false)),
        Arc::new(Mutex::new(None)),
    )
    .await?;
    Ok(CommandOutcome {
        text: outcome_text(&outcome, args.plain || !tty),
        success: outcome.succeeded(),
    })
}

/// One line (`--plain`, piped) or transcript footer plus run id.
fn outcome_text(outcome: &TurnOutcome, compact: bool) -> String {
    let suffix = outcome
        .error
        .as_ref()
        .map(|message| format!("\nreason: {message}"))
        .unwrap_or_default();
    if compact {
        format!(
            "run {} {}: {} tasks, {} tokens{}",
            outcome.run_id, outcome.status, outcome.tasks, outcome.tokens, suffix
        )
    } else {
        format!("{}\nrun {}{}", outcome.footer(), outcome.run_id, suffix)
    }
}

/// Render one run for the interactive session's `/status`, `/tasks`, `/logs`.
pub(crate) async fn inspect_text(store: &Arc<Store>, verb: &str, run_id: String) -> Result<String> {
    let root = store.root().to_path_buf();
    let inspect = InspectArgs { run_id: Some(run_id.clone()), store: StoreArgs { store: root.clone() } };
    let outcome = match verb {
        "status" => status(inspect).await?,
        "tasks" => tasks(inspect).await?,
        _ => logs(LogsArgs {
            run_id: Some(run_id),
            store: StoreArgs { store: root },
            follow: false,
            transcript: true,
            verbose: false,
        })
        .await?,
    };
    Ok(outcome.text)
}

async fn select_run(store: &Store, id: Option<&str>) -> Result<Run> {
    if let Some(id) = id {
        return store.load_run_required(&RunId::parse(id)?).await;
    }
    store
        .list_runs()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no runs found in {}", store.root().display()))
}

async fn status(args: InspectArgs) -> Result<CommandOutcome> {
    let store = Store::open(&args.store.store).await?;
    let run = select_run(&store, args.run_id.as_deref()).await?;
    let stats = store.load_stats(&run.id).await?;
    let tasks = store.list_tasks(&run.id).await?;
    let mut text = format!(
        "run       {}\nstatus    {}\nobjective {}\nprovider  {}\ntasks     {}\ninference {}\ntokens    {}\ntools     {}",
        run.id,
        run.status,
        run.objective,
        run.provider,
        tasks.len(),
        stats.usage.inference_calls,
        stats.usage.total_tokens(),
        stats.usage.tool_calls,
    );
    let events = store.latest_events(&run.id, 1_000).await?;
    let workers: Vec<_> = tasks
        .iter()
        .filter(|task| task.role == swarm_core::AgentRole::Worker)
        .collect();
    for worker in &workers {
        if let Some(summary) = events
            .iter()
            .rev()
            .find(|e| {
                e.kind == swarm_core::event::EventKind::Message
                    && e.task_id.as_ref() == Some(&worker.id)
            })
            .and_then(|e| e.payload.get("summary").and_then(|v| v.as_str()))
        {
            if workers.len() == 1 {
                text.push_str(&format!("\nresult    {summary}"));
            } else {
                text.push_str(&format!("\nresult    {}: {summary}", worker.title));
            }
        }
    }
    Ok(CommandOutcome::success(text))
}

async fn tasks(args: InspectArgs) -> Result<CommandOutcome> {
    let store = Store::open(&args.store.store).await?;
    let run = select_run(&store, args.run_id.as_deref()).await?;
    let mut lines = vec![
        format!("tasks for run {}", run.id),
        format!("{:<42} {:<12} {:<8} {}", "ID", "STATE", "ATTEMPTS", "TITLE"),
    ];
    for task in store.list_tasks(&run.id).await? {
        lines.push(format!(
            "{:<42} {:<12} {:<8} {}",
            task.id, task.state, task.attempts, task.title
        ));
    }
    Ok(CommandOutcome::success(lines.join("\n")))
}

async fn agents(args: InspectArgs) -> Result<CommandOutcome> {
    let store = Store::open(&args.store.store).await?;
    let run = select_run(&store, args.run_id.as_deref()).await?;
    let mut lines = vec![
        format!("agents for run {}", run.id),
        format!("{:<42} {:<12} {:<8} {}", "ID", "STATE", "STEPS", "TASK"),
    ];
    for agent in store.agents_for_run(&run.id).await? {
        lines.push(format!(
            "{:<42} {:<12} {:<8} {}",
            agent.id, agent.state, agent.steps, agent.task_id
        ));
    }
    Ok(CommandOutcome::success(lines.join("\n")))
}

fn format_event(event: &RunEvent) -> String {
    format!("{:>6} {:<23} {}", event.seq, event.kind, event.payload)
}

async fn logs(args: LogsArgs) -> Result<CommandOutcome> {
    let store = Store::open(&args.store.store).await?;
    let run = select_run(&store, args.run_id.as_deref()).await?;
    let mut renderer = args.transcript.then(|| {
        transcript::Transcript::new(
            transcript::Color::detect(io::stdout().is_terminal()),
            args.verbose,
        )
    });
    let initial = store.latest_events(&run.id, 100).await?;
    let mut cursor = initial.last().map_or(0, |e| e.seq);
    let render = |renderer: &mut Option<transcript::Transcript>,
                  event: &swarm_core::event::RunEvent| {
        match renderer {
            Some(transcript) => transcript.lines(event),
            None => vec![format_event(event)],
        }
    };
    let mut lines: Vec<String> = Vec::new();
    if let (Some(transcript), true) = (renderer.as_mut(), args.transcript) {
        lines.extend(transcript.banner(&replay_header(&run)));
    }
    for event in &initial {
        lines.extend(render(&mut renderer, event));
    }
    if let Some(transcript) = renderer.as_mut() {
        lines.extend(transcript.drain());
    }
    if !args.follow {
        return Ok(CommandOutcome::success(lines.join("\n")));
    }
    for line in lines.drain(..) {
        println!("{line}");
    }
    loop {
        let events = store.events_since(&run.id, cursor, 100).await?;
        for event in &events {
            for line in render(&mut renderer, event) {
                println!("{line}");
            }
            cursor = event.seq;
        }
        let state = store.load_run_required(&run.id).await?.status;
        if matches!(
            state,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
        ) && events.is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if let Some(transcript) = renderer.as_mut() {
        for line in transcript.drain() {
            println!("{line}");
        }
    }
    Ok(CommandOutcome::success(String::new()))
}

/// Header for a replayed run. Workspace and approval policy are CLI-time
/// settings, not part of the persisted run, so a replay shows what it can.
fn replay_header(run: &Run) -> transcript::Header {
    transcript::Header {
        run_id: run.id.to_string(),
        goal: run.objective.clone(),
        provider: run.provider.clone(),
        model: run.model.clone().unwrap_or_else(|| "-".to_string()),
        workspace: ".".to_string(),
        approvals: "replay".to_string(),
        agents: run.agent_target,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn mock_run_is_inspectable() {
        let temp = TempDir::new().unwrap();
        let store = temp.path().join("store");
        let workspace = temp.path().join("work");
        let args = Cli::try_parse_from([
            "swarm",
            "run",
            "say hello",
            "--provider",
            "mock",
            "--store",
            store.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ])
        .unwrap();
        let result = execute(args).await.unwrap();
        assert!(result.success, "{}", result.text);
        assert!(result.text.contains("completed"));
        let status = execute(
            Cli::try_parse_from(["swarm", "status", "--store", store.to_str().unwrap()]).unwrap(),
        )
        .await
        .unwrap();
        assert!(status.text.contains("status    completed"));
        let tasks = execute(
            Cli::try_parse_from(["swarm", "tasks", "--store", store.to_str().unwrap()]).unwrap(),
        )
        .await
        .unwrap();
        assert!(tasks.text.contains("completed"));
        let agents = execute(
            Cli::try_parse_from(["swarm", "agents", "--store", store.to_str().unwrap()]).unwrap(),
        )
        .await
        .unwrap();
        assert!(agents.text.contains("completed"));
        let persisted = Store::open(&store).await.unwrap();
        let run = select_run(&persisted, None).await.unwrap();
        assert_eq!(persisted.agents_for_run(&run.id).await.unwrap()[0].steps, 1);
        assert!(status.text.contains("result    [mock:"));
    }

    #[tokio::test]
    async fn local_compatible_endpoint_executes_tool_and_persists_reply() {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let response_calls = calls.clone();
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |_: &wiremock::Request| {
                let first = response_calls.fetch_add(1, Ordering::SeqCst) == 0;
                let event = if first {
                    serde_json::json!({
                        "choices": [{"delta": {"tool_calls": [{
                            "index": 0, "id": "call_1",
                            "function": {"name": "write_file", "arguments": "{\"path\":\"hello.txt\",\"content\":\"hello\"}"}
                        }]}, "finish_reason": "tool_calls"}],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
                    })
                } else {
                    serde_json::json!({
                        "choices": [{"delta": {"content": "hello.txt created"}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 20, "completion_tokens": 4}
                    })
                };
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(format!("data: {event}\n\ndata: [DONE]\n\n"))
            })
            .mount(&server).await;

        let temp = TempDir::new().unwrap();
        let store = temp.path().join("store");
        let workspace = temp.path().join("work");
        let result = execute(
            Cli::try_parse_from([
                "swarm",
                "run",
                "create hello.txt",
                "--provider",
                "local",
                "--approve-all",
                "--base-url",
                &format!("{}/v1", server.uri()),
                "--store",
                store.to_str().unwrap(),
                "--workspace",
                workspace.to_str().unwrap(),
            ])
            .unwrap(),
        )
        .await
        .unwrap();
        assert!(result.success, "{}", result.text);
        assert_eq!(
            std::fs::read_to_string(workspace.join("hello.txt")).unwrap(),
            "hello"
        );
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 2);
        let second: serde_json::Value = serde_json::from_slice(&received[1].body).unwrap();
        assert_eq!(second["messages"][2]["tool_calls"][0]["id"], "call_1");
        assert_eq!(second["messages"][3]["tool_call_id"], "call_1");
        let status = execute(
            Cli::try_parse_from(["swarm", "status", "--store", store.to_str().unwrap()]).unwrap(),
        )
        .await
        .unwrap();
        assert!(status.text.contains("result    hello.txt created"));
    }

    #[tokio::test]
    async fn mock_multi_agent_run_creates_and_completes_workers() {
        let temp = TempDir::new().unwrap();
        let store_path = temp.path().join("store");
        let workspace = temp.path().join("work");
        let result = execute(
            Cli::try_parse_from([
                "swarm",
                "run",
                "divide the work",
                "--provider",
                "mock",
                "--agents",
                "3",
                "--store",
                store_path.to_str().unwrap(),
                "--workspace",
                workspace.to_str().unwrap(),
            ])
            .unwrap(),
        )
        .await
        .unwrap();
        assert!(result.success, "{}", result.text);
        let store = Store::open(&store_path).await.unwrap();
        let run = select_run(&store, None).await.unwrap();
        let tasks = store.list_tasks(&run.id).await.unwrap();
        assert_eq!(tasks.len(), 3);
        assert!(
            tasks
                .iter()
                .all(|t| t.state == swarm_core::TaskState::Completed)
        );
        let root = tasks.iter().find(|t| t.parent_id.is_none()).unwrap();
        assert_eq!(root.role, swarm_core::AgentRole::Planner);
        assert_eq!(
            tasks
                .iter()
                .filter(|t| t.parent_id.as_ref() == Some(&root.id))
                .count(),
            2
        );
        let status = execute(
            Cli::try_parse_from(["swarm", "status", "--store", store_path.to_str().unwrap()])
                .unwrap(),
        )
        .await
        .unwrap();
        assert!(
            status
                .text
                .contains("result    Complete independent work item")
        );
    }

    #[test]
    fn rejects_unconfigured_provider() {
        let Cli {
            command: Some(Command::Run(args)),
        } = Cli::try_parse_from(["swarm", "run", "goal", "--provider", "missing"]).unwrap()
        else {
            unreachable!()
        };
        assert!(
            run_config(&args)
                .unwrap_err()
                .to_string()
                .contains("not configured")
        );
    }

    #[tokio::test]
    async fn toml_config_and_cli_overrides_run_end_to_end() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("swarm.toml");
        let config = RunConfig::mock("objective from file", 1);
        std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
        let store_path = temp.path().join("store");
        let workspace = temp.path().join("work");
        let cli = Cli::try_parse_from([
            "swarm",
            "run",
            "objective from cli",
            "--config",
            config_path.to_str().unwrap(),
            "--model",
            "overridden-mock",
            "--approve-all",
            "--store",
            store_path.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ])
        .unwrap();
        let result = execute(cli).await.unwrap();
        assert!(result.success, "{}", result.text);
        let store = Store::open(&store_path).await.unwrap();
        let run = select_run(&store, None).await.unwrap();
        assert_eq!(run.objective, "objective from cli");
        assert_eq!(run.model.as_deref(), Some("overridden-mock"));
        let status = execute(
            Cli::try_parse_from([
                "swarm",
                "status",
                run.id.as_str(),
                "--store",
                store_path.to_str().unwrap(),
            ])
            .unwrap(),
        )
        .await
        .unwrap();
        assert!(status.text.contains("result    [mock:overridden-mock]"));
    }

    #[tokio::test]
    async fn managed_provider_runs_from_toml_config() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("swarm.toml");
        let mut config = RunConfig::mock("managed-prompt-token", 1);
        let mut provider = swarm_core::config::ProviderConfig::mock("managed", "stdin-agent");
        provider.kind = swarm_core::provider::ProviderKind::ManagedAgent;
        provider.command = if cfg!(windows) {
            vec!["cmd".into(), "/C".into(), "more".into()]
        } else {
            vec!["cat".into()]
        };
        config.provider = provider.name.clone();
        config.providers = vec![provider];
        std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
        let store_path = temp.path().join("store");
        let workspace = temp.path().join("work");
        let result = execute(
            Cli::try_parse_from([
                "swarm",
                "run",
                "managed-prompt-token",
                "--provider",
                "managed",
                "--config",
                config_path.to_str().unwrap(),
                "--store",
                store_path.to_str().unwrap(),
                "--workspace",
                workspace.to_str().unwrap(),
            ])
            .unwrap(),
        )
        .await
        .unwrap();
        assert!(result.success, "{}", result.text);
        let status = execute(
            Cli::try_parse_from(["swarm", "status", "--store", store_path.to_str().unwrap()])
                .unwrap(),
        )
        .await
        .unwrap();
        assert!(status.text.contains("managed-prompt-token"));
    }
}
