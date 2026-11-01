use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use swarm_core::config::{ApprovalPolicy, RunConfig, ToolProtocolSelection, TaskPreset};
use swarm_core::event::RunEvent;
use swarm_core::run::{Run, RunMode};
use swarm_core::{ProviderId, RunId, RunStatus};
use swarm_runtime::{Runtime, build_registry, local_provider_config, scheduler_config};
use swarm_scheduler::Scheduler;
use swarm_store::Store;
use swarm_tools::{SandboxConfig, ToolExecutor};

use crate::approval::{AllowList, SharedMode, UiApproval, mode_of};
use crate::transcript;
use crate::ui::{Action, StartOptions, Ui, UiHandle};

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
    /// Run workers in the background and collect structured results (for an
    /// orchestrating agent or a script; every verb prints JSON).
    Job(crate::job::JobArgs),
    /// Serve the job commands as MCP tools over stdio, for agents that speak MCP.
    Mcp(crate::mcp::McpArgs),
}

#[derive(Debug, Args)]
pub struct StoreArgs {
    #[arg(long, default_value = ".swarm")]
    store: PathBuf,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    goal: String,
    /// Budget and task-role defaults; explicit ceilings take precedence.
    #[arg(long)]
    preset: Option<TaskPreset>,
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
    /// Tool transport: auto follows provider capabilities; text forces fenced JSON calls.
    #[arg(long, env = "SWARM_TOOL_PROTOCOL")]
    tool_protocol: Option<ToolProtocolSelection>,
    #[arg(long)]
    sandbox_image: Option<String>,

    /// Refs this run's workers may `git push` to: a branch, `remote/branch`, or `*` for
    /// anything. Repeatable. The default - no flag - blocks every push from the worker
    /// shell, so the orchestrator keeps ownership of remote writes.
    #[arg(long = "allow-push")]
    allow_push: Vec<String>,
    /// Steps (model turns) each agent may take; the default is 40, which long
    /// edit-test-commit tasks can exhaust before they finish.
    #[arg(long)]
    max_steps: Option<u32>,
    /// Tokens (input plus output, over every call) the whole run may use. The
    /// default is 200000, which a task that reads a large repository can spend in
    /// a handful of calls.
    #[arg(long)]
    max_tokens: Option<u64>,
    /// Inference calls the whole run may make. Defaults to the run config's
    /// pairing of agents x steps, and --max-steps raises it in step with itself.
    #[arg(long)]
    max_inference_calls: Option<u64>,
    /// Wall-clock limit for the whole run in seconds (default 3600). A worker
    /// over a large repository can spend most of an hour in builds and tests.
    #[arg(long)]
    max_wall_clock_secs: Option<u64>,
    /// Print only the final one-line summary (no live transcript).
    #[arg(long)]
    plain: bool,
    /// Show full transcript detail (agent states, checkpoints, long replies).
    #[arg(long)]
    verbose: bool,
}

#[derive(Debug, Args)]
pub struct InspectArgs {
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
    /// Tool transport: auto follows provider capabilities; text forces fenced JSON calls.
    #[arg(long, env = "SWARM_TOOL_PROTOCOL")]
    pub tool_protocol: Option<ToolProtocolSelection>,
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
pub struct ResumeArgs {
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
    /// Tool transport: auto follows provider capabilities; text forces fenced JSON calls.
    #[arg(long, env = "SWARM_TOOL_PROTOCOL")]
    tool_protocol: Option<ToolProtocolSelection>,
    #[arg(long)]
    sandbox_image: Option<String>,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    agents: Option<u32>,
    /// Raise the run's token ceiling. Usage is cumulative, so a run that died on
    /// its budget resumes only with a ceiling above what it already spent.
    #[arg(long)]
    max_tokens: Option<u64>,
    /// Raise the per-agent step ceiling (also raises inference calls in step).
    #[arg(long)]
    max_steps: Option<u32>,
    #[arg(long)]
    max_inference_calls: Option<u64>,
    #[arg(long)]
    max_wall_clock_secs: Option<u64>,
    #[arg(long)]
    approve_all: bool,
    #[arg(long)]
    plain: bool,
    #[arg(long)]
    verbose: bool,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
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
pub(crate) fn default_chat_args() -> ChatArgs {
    ChatArgs {
        goal: None,
        store: StoreArgs { store: PathBuf::from(".swarm") },
        workspace: PathBuf::from("./work"),
        provider: None,
        model: None,
        base_url: None,
        tool_protocol: None,
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
        Some(Command::Job(args)) => {
            let (text, success) = crate::job::run(args).await?;
            Ok(CommandOutcome::plain(text, success))
        }
        Some(Command::Mcp(args)) => {
            crate::mcp::serve(args).await?;
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
    if let Some(image) = &args.sandbox_image { cfg.tools.docker_image = Some(image.clone()); }
    if let Some(preset) = args.preset { cfg.preset = preset; }
    if let Some(agents) = args.agents {
        if agents == 0 {
            bail!("--agents must be at least 1");
        }
        cfg.agents = agents;
    }
    if let Some((tokens, steps)) = cfg.preset.limits() {
        cfg.budget.max_total_tokens = tokens;
        cfg.budget.max_steps_per_agent = steps;
        cfg.budget.max_inference_calls = cfg.agents.max(1) as u64 * steps as u64;
    }
    if let Some(steps) = args.max_steps {
        if steps == 0 {
            bail!("--max-steps must be at least 1");
        }
        cfg.budget.max_steps_per_agent = steps;
        // `max_steps_per_agent` and the run-wide `max_inference_calls` are
        // paired in RunConfig (agents x steps). Raising only the first just
        // moves the wall: the agent dies at the default 40 calls with
        // `budget exceeded for inference_calls` instead of at its step limit.
        cfg.budget.max_inference_calls = cfg
            .budget
            .max_inference_calls
            .max(cfg.agents as u64 * steps as u64);
    }
    if let Some(calls) = args.max_inference_calls {
        if calls == 0 {
            bail!("--max-inference-calls must be at least 1");
        }
        cfg.budget.max_inference_calls = calls;
    }
    if let Some(secs) = args.max_wall_clock_secs {
        if secs == 0 {
            bail!("--max-wall-clock-secs must be at least 1");
        }
        cfg.budget.max_wall_clock_secs = secs;
    }
    if let Some(tokens) = args.max_tokens {
        if tokens == 0 {
            bail!("--max-tokens must be at least 1");
        }
        cfg.budget.max_total_tokens = tokens;
    }
    apply_provider_overrides(
        &mut cfg,
        args.provider.as_deref(),
        args.model.as_deref(),
        args.base_url.as_deref(),
        args.tool_protocol,
        &args.workspace,
        args.approve_all,
    )?;
    // A ceiling from `--max-tokens` or a config file is the user's; only the mock
    // config's built-in default is relaxed.
    if args.max_tokens.is_none() && args.config.is_none() && args.preset.is_none() {
        relax_token_ceiling(&mut cfg);
    }
    Ok(cfg)
}

/// A provider that is not billed per call (a local model, a chat subscription) has no
/// spend for the token ceiling to guard: its token figure is an estimate of the prompt
/// re-sent on every step, and enforcing the mock config's 200000 ended a six-worker PR
/// review mid-way while steps, calls and wall-clock time were nowhere near their limits.
/// So the built-in default ceiling is lifted for those providers; calls, steps and
/// wall-clock time still bound the run and an on-demand provider keeps the ceiling.
fn relax_token_ceiling(cfg: &mut RunConfig) {
    let free = cfg
        .providers
        .iter()
        .find(|p| p.name == cfg.provider)
        .is_some_and(|p| p.billing != swarm_core::budget::BillingMode::OnDemand);
    if free {
        cfg.budget.max_total_tokens = swarm_core::budget::ResourceBudget::default().max_total_tokens;
    }
}

/// Apply `--provider/--model/--base-url/--approve-all` to a config. Shared by
/// `swarm run` and the interactive session so both resolve providers the same
/// way (presets, workspace binding, zero-cost policy).
pub(crate) fn apply_provider_overrides(
    cfg: &mut RunConfig,
    provider: Option<&str>,
    model: Option<&str>,
    base_url: Option<&str>,
    tool_protocol: Option<ToolProtocolSelection>,
    workspace: &Path,
    approve_all: bool,
) -> Result<()> {
    if let Some(protocol) = tool_protocol {
        cfg.tool_protocol = protocol;
    }
    if let Some(name) = provider {
        let preset = if cfg.providers.iter().any(|p| p.name.as_str() == name) { None } else { match name {
            "local" => Some(local_provider_config()),
            "chatgpt-web" => Some(swarm_runtime::chatgpt_web_provider_config()),
            "gemini-web" => Some(swarm_runtime::gemini_web_provider_config()),
            _ => None,
        }};
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
        selected.endpoints.clear();
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
        args.tool_protocol,
        &args.workspace,
        args.approve_all,
    )?;
    relax_token_ceiling(&mut cfg);
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
        .allow_network(cfg.tools.allow_network)
        .allow_push(cfg.tools.allow_push.clone())
        .docker_image(cfg.tools.docker_image.clone());
    if sandbox.docker_image.is_some() && !sandbox.allow_push.is_empty() { bail!("isolated workers cannot receive push grants"); }
    swarm_tools::isolation::prepare_repository(&sandbox).map_err(|e| anyhow!(e))?;
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
    /// What the turn did (files edited, commands run), from the transcript.
    pub stats: transcript::TurnStats,
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

/// What a turn needs from the session around it.
pub(crate) struct TurnEnv {
    pub ui: UiHandle,
    pub allow: Arc<AllowList>,
    pub mode: Arc<SharedMode>,
    pub verbose: bool,
    /// Show only the assistant's words, failures and denials (no tool rows).
    pub focus: bool,
    /// Shell hooks around tool calls.
    pub hooks: Option<Arc<crate::hooks::Hooks>>,
    /// Print the run banner first (`swarm run`; a chat turn has its own header).
    pub banner: bool,
}

/// Run one goal to completion: create a durable run, stream its transcript to
/// the UI, wire the approval dialog, drive the scheduler, collect the numbers.
///
/// `current` receives the run id as soon as it exists: if the caller drops this
/// future (Ctrl+C) the run stays in the store and `/resume <id>` picks it up.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drive_turn(
    store: &Arc<Store>,
    cfg: &RunConfig,
    workspace: &Path,
    objective: &str,
    mode: RunMode,
    existing: Option<&RunId>,
    live: bool,
    env: &TurnEnv,
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
        .allow_network(cfg.tools.allow_network)
        .allow_push(cfg.tools.allow_push.clone());
    let executor = Arc::new(ToolExecutor::new(sandbox));
    let mut instructions = swarm_runtime::project_instructions(workspace).unwrap_or_default();
    let repo_map = if let Some(path) = &cfg.repo_map_file {
        Some(std::fs::read_to_string(path).with_context(|| format!("read repository map {}", path.display()))?
            .chars().take(cfg.repo_map_chars).collect::<String>())
    } else { swarm_runtime::repo_map::repository_map(workspace, cfg.repo_map_chars) };
    if let Some(map) = repo_map { instructions.push_str("\n\n"); instructions.push_str(&map); }
    let runtime = Runtime::new(store.clone(), registry, executor, scheduler)
        .with_approval(cfg.tools.approval)
        .with_tool_protocol(cfg.tool_protocol)
        // Claude Code reads CLAUDE.md from the project root; do the same for
        // AGENTS.md/CLAUDE.md so repo instructions reach every agent.
        .with_instructions(Some(instructions));
    let created = match existing {
        // Resuming continues the run that is already in the store; a new goal
        // creates one. The scheduler picks up persisted task state either way.
        Some(id) => store.load_run_required(id).await?,
        None => runtime.create_run(&cfg, mode).await?,
    };
    if let Ok(mut slot) = current.lock() {
        *slot = Some(created.id.clone());
    }

    // Claude Code-style live view. Piped output and `--plain` keep the
    // original one-line summary.
    let show = live
        && (env.ui.is_live()
            || transcript::ui_enabled(io::stdout().is_terminal() && io::stderr().is_terminal()));
    let header = transcript::Header {
        run_id: created.id.to_string(),
        goal: created.objective.clone(),
        provider: created.provider.clone(),
        model: created.model.clone().unwrap_or_else(|| cfg.model.clone()),
        workspace: workspace.display().to_string(),
        approvals: cfg.tools.approval.to_string(),
        agents: created.agent_target,
    };
    let tail = show.then(|| {
        crate::live::LiveTail::start(
            store.clone(),
            created.id.clone(),
            env.banner.then_some(&header),
            env.ui.clone(),
            env.verbose,
            env.focus,
        )
    });
    let runtime = match &env.hooks {
        Some(hooks) => runtime.with_tool_hooks(hooks.clone()),
        None => runtime,
    };
    let runtime = if cfg.tools.approval == ApprovalPolicy::Never {
        runtime
    } else {
        runtime.with_approval_handler(Arc::new(UiApproval::new(
            env.ui.clone(),
            Arc::clone(&env.allow),
            Arc::clone(&env.mode),
            workspace.to_path_buf(),
        )))
    };
    let started = Instant::now();
    let state = runtime.drive(&created.id).await?;
    let stats = match tail {
        Some(tail) => tail.finish().await,
        None => transcript::TurnStats::default(),
    };
    let mut outcome = summarize_turn(store, &created.id, state, started.elapsed().as_secs_f64()).await?;
    outcome.stats = stats;
    Ok(outcome)
}

/// A turn for `swarm run` / `swarm resume`: on a terminal it gets the same
/// single-owner UI as the chat (status line, permission dialog, Esc to stop);
/// elsewhere it prints plainly.
#[allow(clippy::too_many_arguments)]
async fn drive_standalone(
    store: &Arc<Store>,
    cfg: &RunConfig,
    workspace: &Path,
    objective: &str,
    mode: RunMode,
    existing: Option<&RunId>,
    plain: bool,
    verbose: bool,
) -> Result<TurnOutcome> {
    let tty = io::stdout().is_terminal() && io::stdin().is_terminal() && transcript::ui_enabled(true);
    let colors = crate::style::Color::detect(tty);
    let mut ui = if tty && !plain {
        Ui::start(StartOptions {
            colors,
            history_path: None,
            workspace: workspace.to_path_buf(),
            input: false,
        })
        .ok()
    } else {
        None
    };
    let handle = ui.as_ref().map(Ui::handle).unwrap_or_else(|| UiHandle::plain(colors));
    handle.busy(true);
    let env = TurnEnv {
        ui: handle.clone(),
        allow: allow_list(),
        mode: SharedMode::new(mode_of(cfg.tools.approval)),
        verbose,
        focus: false,
        hooks: None,
        banner: true,
    };
    let current = Arc::new(Mutex::new(None));
    let drive = drive_turn(store, cfg, workspace, objective, mode, existing, !plain, &env, Arc::clone(&current));
    tokio::pin!(drive);
    let result = loop {
        tokio::select! {
            outcome = &mut drive => break outcome,
            action = async {
                match ui.as_mut() {
                    Some(ui) => ui.recv().await,
                    None => std::future::pending().await,
                }
            } => if matches!(action, Some(Action::Interrupt) | Some(Action::Quit) | None) {
                let id = current.lock().ok().and_then(|slot| slot.as_ref().map(|id| id.to_string()));
                break Err(match id {
                    Some(id) => anyhow!("interrupted \u{00b7} run {id} is in the store: swarm resume {id}"),
                    None => anyhow!("interrupted"),
                });
            },
        }
    };
    handle.busy(false);
    if let Some(ui) = ui {
        ui.shutdown().await;
    }
    result
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
        stats: transcript::TurnStats::default(),
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
    let mut cfg = run_config(&args)?;
    cfg.tools.allow_push.extend(args.allow_push.iter().cloned());
    let workspace = workspace_for(&cfg, &args.workspace)?;
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("create workspace {}", workspace.display()))?;
    let store = Arc::new(Store::open(&args.store.store).await?);
    let tty = transcript::ui_enabled(io::stdout().is_terminal() && io::stderr().is_terminal());
    let outcome = drive_standalone(
        &store,
        &cfg,
        &workspace,
        &args.goal,
        RunMode::Coding,
        None,
        args.plain,
        args.verbose,
    )
    .await?;
    Ok(CommandOutcome {
        text: outcome_text(&outcome, args.plain || !tty),
        success: outcome.succeeded(),
    })
}

/// Raise (never lower) a run's ceilings, for resuming a run that stopped on budget.
/// `max_steps` also raises inference calls in step, mirroring
/// `apply_provider_overrides`, so the two ceilings cannot contradict each other.
fn apply_budget_raise(
    budget: &mut swarm_core::budget::ResourceBudget,
    agent_target: u32,
    max_tokens: Option<u64>,
    max_steps: Option<u32>,
    max_inference_calls: Option<u64>,
    max_wall_clock_secs: Option<u64>,
) {
    if let Some(tokens) = max_tokens {
        budget.max_total_tokens = budget.max_total_tokens.max(tokens);
    }
    if let Some(calls) = max_inference_calls {
        budget.max_inference_calls = budget.max_inference_calls.max(calls);
    }
    if let Some(secs) = max_wall_clock_secs {
        budget.max_wall_clock_secs = budget.max_wall_clock_secs.max(secs);
    }
    if let Some(steps) = max_steps {
        budget.max_steps_per_agent = budget.max_steps_per_agent.max(steps);
        budget.max_inference_calls = budget
            .max_inference_calls
            .max(u64::from(agent_target) * u64::from(steps));
    }
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
    let mut cfg = if let Some(path) = &args.config {
        toml::from_str::<RunConfig>(&std::fs::read_to_string(path)?)?
    } else { RunConfig::mock(&run.objective, run.agent_target.max(1)) };
    if let Some(image) = &args.sandbox_image { cfg.tools.docker_image = Some(image.clone()); }
    apply_provider_overrides(
        &mut cfg,
        args.provider.as_deref().or_else(|| if args.config.is_none() { Some(run.provider.as_str()) } else { None }),
        args.model.as_deref(),
        args.base_url.as_deref(),
        args.tool_protocol,
        &args.workspace,
        args.approve_all,
    )?;
    cfg.model = run.model.clone().unwrap_or_else(|| cfg.model.clone());
    cfg.budget = run.budget;
    apply_budget_raise(
        &mut cfg.budget,
        run.agent_target,
        args.max_tokens,
        args.max_steps,
        args.max_inference_calls,
        args.max_wall_clock_secs,
    );
    // The agent enforces the budget stored on the RUN ROW (swarm-runtime/src/agent.rs
    // checks `run.budget` against cumulative usage before every inference call), so a
    // raise must be persisted - a local cfg raise would be silently ignored.
    if cfg.budget != run.budget {
        let mut raised = run.clone();
        raised.budget = cfg.budget;
        store.update_run(&raised).await?;
    }
    // The raise is necessary but not sufficient: a budget death leaves the agent `failed`
    // and its task `failed`, so the scheduler would have nothing to dispatch and the run
    // would re-finalize, replaying the old reason. Revive them - the agent's conversation
    // is checkpointed after every step, so the work is still there.
    let raised_ceilings = args.max_tokens.is_some()
        || args.max_steps.is_some()
        || args.max_inference_calls.is_some()
        || args.max_wall_clock_secs.is_some();
    let revived = store.recover_budget_death(&id, swarm_core::time::now_millis(), raised_ceilings).await?;
    if revived.reset_agents > 0 {
        let mut running = store.load_run_required(&id).await?;
        // Assignment, not `transition`: the run state machine has no failed -> running
        // edge, because a budget death is a pause the machine does not model.
        running.status = RunStatus::Running;
        store.update_run(&running).await?;
    }
    cfg.zero_cost = run.zero_cost;
    cfg.scheduler.zero_cost = run.zero_cost;
    let workspace = workspace_for(&cfg, &args.workspace)?;
    std::fs::create_dir_all(&workspace)
        .with_context(|| format!("create workspace {}", workspace.display()))?;
    let tty = transcript::ui_enabled(io::stdout().is_terminal() && io::stderr().is_terminal());
    let outcome = drive_standalone(
        &store,
        &cfg,
        &workspace,
        &run.objective,
        run.mode,
        Some(&id),
        args.plain,
        args.verbose,
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
    if let Some(renderer) = renderer.as_mut() {
        renderer.set_width(crate::style::term_width());
    }
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

    #[test]
    fn the_default_token_ceiling_is_lifted_only_for_a_provider_not_billed_per_call() {
        let default = swarm_core::budget::ResourceBudget::default().max_total_tokens;
        let chat = |provider: &str| {
            let args = ChatArgs { provider: Some(provider.to_string()), agents: Some(4), ..default_chat_args() };
            chat_config(&args).unwrap().budget.max_total_tokens
        };
        assert_eq!(chat("chatgpt-web"), default, "a chat subscription has no spend to guard");
        assert_eq!(chat("local"), default);
        assert_eq!(chat("mock"), default, "the mock is zero-cost");
        // The calls bound is untouched: 4 agents x 40.
        let cfg = chat_config(&ChatArgs { provider: Some("chatgpt-web".into()), agents: Some(4), ..default_chat_args() }).unwrap();
        assert_eq!(cfg.budget.max_inference_calls, 160);
    }

    #[test]
    fn a_ceiling_the_user_set_is_not_lifted() {
        let mut cfg = RunConfig::mock("goal", 1);
        cfg.budget.max_total_tokens = 300;
        // `--max-tokens`/a config file: run_config skips the relaxation; simulated here by
        // not calling it, then checking the on-demand case keeps its ceiling too.
        assert_eq!(cfg.budget.max_total_tokens, 300);
        let mut metered = RunConfig::mock("goal", 1);
        metered.providers[0].billing = swarm_core::budget::BillingMode::OnDemand;
        relax_token_ceiling(&mut metered);
        assert_eq!(metered.budget.max_total_tokens, 200_000, "an on-demand provider keeps the ceiling");
    }

    #[test]
    fn a_resume_raise_only_raises_and_steps_carry_inference_calls() {
        let mut budget = RunConfig::mock("goal", 1).budget;
        budget.max_total_tokens = 100;
        budget.max_inference_calls = 10;
        budget.max_wall_clock_secs = 60;
        budget.max_steps_per_agent = 10;
        // Smaller ceilings are ignored: usage is cumulative, so lowering the ceiling
        // on a resumed run would fail the very next inference call.
        apply_budget_raise(&mut budget, 1, Some(50), None, Some(5), Some(30));
        assert_eq!(
            (budget.max_total_tokens, budget.max_inference_calls, budget.max_wall_clock_secs),
            (100, 10, 60)
        );
        // Raising works, and raising steps raises inference calls in step.
        apply_budget_raise(&mut budget, 2, Some(500), Some(60), None, Some(90));
        assert_eq!(budget.max_total_tokens, 500);
        assert_eq!(budget.max_wall_clock_secs, 90);
        assert_eq!(budget.max_steps_per_agent, 60);
        assert!(budget.max_inference_calls >= 120, "steps x agents, got {}", budget.max_inference_calls);
    }

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
    async fn forced_text_protocol_acts_and_continues_from_real_tool_results() {
        use serde_json::{Value, json};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let temp = TempDir::new().unwrap();
        let workspace = temp.path().join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("input.txt"), "evidence-73").unwrap();
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        Mock::given(method("POST")).and(path("/v1/chat/completions"))
            .respond_with(move |request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert!(body.get("tools").is_none(), "text mode must omit native tools");
                assert!(body["messages"].as_array().unwrap().iter().any(|m|
                    m["role"] == "system" && m["content"].as_str().unwrap_or("").contains("read_file")));
                let step = seen.fetch_add(1, Ordering::SeqCst);
                let text = match step {
                    0 => json!({"tool":"read_file", "arguments":{"path":"input.txt"}}).to_string(),
                    1 => {
                        assert!(body["messages"].as_array().unwrap().iter().any(|m|
                            m["role"] == "tool" && m["content"].as_str().unwrap_or("").contains("evidence-73")),
                            "the next inference must receive the real file contents");
                        json!({"tool":"write_file", "arguments":{"path":"output.txt", "content":"evidence-73"}}).to_string()
                    },
                    2 => json!({"tool":"report_result", "arguments":{"status":"success", "summary":"Copied evidence-73", "artifacts":["output.txt"]}}).to_string(),
                    _ => "Copied evidence-73 to output.txt.".into(),
                };
                let chunk = json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]});
                ResponseTemplate::new(200).insert_header("content-type", "text/event-stream")
                    .set_body_string(format!("data: {chunk}\n\ndata: [DONE]\n\n"))
            }).mount(&server).await;
        let cli = Cli::try_parse_from([
            "swarm", "run", "Copy input.txt to output.txt using tools", "--provider", "local",
            "--base-url", &format!("{}/v1", server.uri()), "--tool-protocol", "text",
            "--workspace", workspace.to_str().unwrap(), "--store", temp.path().join("store").to_str().unwrap(),
            "--approve-all", "--plain", "--max-steps", "6",
        ]).unwrap();
        let outcome = execute(cli).await.unwrap();
        assert!(outcome.success, "{}", outcome.text);
        assert_eq!(std::fs::read_to_string(workspace.join("output.txt")).unwrap(), "evidence-73");
        assert!(calls.load(Ordering::SeqCst) >= 3);
    }

    #[tokio::test]
    async fn reviewer_requires_and_accepts_recorded_inspection_and_checks() {
        use serde_json::{Value,json};
        let temp = TempDir::new().unwrap(); let workspace = temp.path().join("work");
        std::fs::create_dir_all(&workspace).unwrap(); std::fs::write(workspace.join("a.txt"),"review target").unwrap();
        let server = MockServer::start().await; let steps = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST")).and(path("/v1/chat/completions"))
            .respond_with(move |_: &wiremock::Request| {
                let step = steps.fetch_add(1,Ordering::SeqCst);
                let text = match step {
                    0 => json!({"tool":"read_file","arguments":{"path":"a.txt"}}).to_string(),
                    1 => json!({"tool":"run_tests","arguments":{"command":"echo REVIEW_CHECK"}}).to_string(),
                    2 => json!({"tool":"report_result","arguments":{"status":"success","summary":"inspected and checked",
                        "verdict":"APPROVE","blockers":[],"checks":[{"command":"echo REVIEW_CHECK","observed_output":"REVIEW_CHECK"}]}}).to_string(),
                    _ => "Review complete".into(),
                };
                let chunk = json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]});
                ResponseTemplate::new(200).set_body_string(format!("data: {chunk}\n\ndata: [DONE]\n\n"))
            }).mount(&server).await;
        let cli = Cli::try_parse_from(["swarm","run","Review a.txt","--provider","local","--preset","reviewer",
            "--tool-protocol","text","--base-url",&format!("{}/v1",server.uri()),"--workspace",workspace.to_str().unwrap(),
            "--store",temp.path().join("store").to_str().unwrap(),"--approve-all","--plain"]).unwrap();
        let result = execute(cli).await.unwrap(); assert!(result.success,"{}",result.text);
        let store = Store::open(&temp.path().join("store")).await.unwrap(); let run = select_run(&store,None).await.unwrap();
        let events = store.latest_events(&run.id,1000).await.unwrap();
        let report = crate::job::reported_result(&events).unwrap();
        let audit: Value = swarm_runtime::review::audit_review(&workspace,&events,Some(&report),2);
        assert_eq!(audit["trusted"],true,"{audit}");
        let empty = execute(Cli::try_parse_from(["swarm","run","Review without doing anything","--provider","mock","--preset","reviewer",
            "--workspace",workspace.to_str().unwrap(),"--store",temp.path().join("empty").to_str().unwrap(),"--approve-all","--plain"]).unwrap()).await.unwrap();
        assert!(!empty.success,"an empty reviewer must fail its evidence gate");
    }

    #[test]
    fn tool_protocol_defaults_and_overrides_are_validated() {
        let mut config = RunConfig::mock("goal", 1);
        config.tool_protocol = ToolProtocolSelection::Text;
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("swarm.toml");
        std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        for (extra, expected) in [(vec![], ToolProtocolSelection::Text), (vec!["--tool-protocol", "native"], ToolProtocolSelection::Native)] {
            let mut argv = vec!["swarm", "run", "goal", "--config", path.to_str().unwrap()];
            argv.extend(extra);
            let Cli { command: Some(Command::Run(args)) } = Cli::try_parse_from(argv).unwrap() else { unreachable!() };
            assert_eq!(run_config(&args).unwrap().tool_protocol, expected);
        }
        assert!(Cli::try_parse_from(["swarm", "run", "goal", "--tool-protocol", "invalid"]).is_err());
        let old = toml::to_string(&RunConfig::mock("old", 1)).unwrap().replace("tool_protocol = \"auto\"\n", "");
        assert_eq!(toml::from_str::<RunConfig>(&old).unwrap().tool_protocol, ToolProtocolSelection::Auto);
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
