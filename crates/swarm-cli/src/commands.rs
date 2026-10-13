use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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

#[derive(Debug, Parser)]
#[command(
    name = "swarm",
    version,
    about = "Run and inspect durable LLM agent tasks"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Execute a goal with the selected provider.
    Run(RunArgs),
    /// Show a run's state and resource usage.
    Status(InspectArgs),
    /// List the tasks in a run.
    Tasks(InspectArgs),
    /// List the agents in a run.
    Agents(InspectArgs),
    /// Show recent events, optionally following new ones.
    Logs(LogsArgs),
}

#[derive(Debug, Args)]
struct StoreArgs {
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
}

#[derive(Debug, Args)]
struct InspectArgs {
    run_id: Option<String>,
    #[command(flatten)]
    store: StoreArgs,
}

#[derive(Debug, Args)]
struct LogsArgs {
    run_id: Option<String>,
    #[command(flatten)]
    store: StoreArgs,
    #[arg(long)]
    follow: bool,
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
}

pub async fn execute(cli: Cli) -> Result<CommandOutcome> {
    match cli.command {
        Command::Run(args) => run(args).await,
        Command::Status(args) => status(args).await,
        Command::Tasks(args) => tasks(args).await,
        Command::Agents(args) => agents(args).await,
        Command::Logs(args) => logs(args).await,
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

    if let Some(name) = &args.provider {
        if name == "local" {
            let local = local_provider_config();
            cfg.provider = local.name.clone();
            cfg.model = local.model.clone();
            cfg.providers.retain(|p| p.name != local.name);
            cfg.providers.push(local);
            cfg.zero_cost = true;
            cfg.scheduler.zero_cost = true;
        } else {
            cfg.provider = ProviderId::new(name.clone())?;
        }
    }

    let selected_index = cfg
        .providers
        .iter()
        .position(|p| p.name == cfg.provider)
        .ok_or_else(|| anyhow!("provider '{}' is not configured", cfg.provider))?;
    let selected = &mut cfg.providers[selected_index];
    if let Some(model) = &args.model {
        selected.model = model.clone();
        cfg.model = model.clone();
    } else {
        cfg.model = selected.model.clone();
    }
    if let Some(url) = &args.base_url {
        if selected.kind != swarm_core::provider::ProviderKind::OpenAiCompatible {
            bail!("--base-url requires an OpenAI-compatible provider");
        }
        selected.base_url = Some(url.clone());
    }
    if !cfg.may_run(&cfg.providers[selected_index]) {
        bail!(
            "provider '{}' is disallowed by the zero-cost policy",
            cfg.provider
        );
    }
    if args.approve_all {
        cfg.tools.approval = ApprovalPolicy::Never;
    }
    Ok(cfg)
}

async fn run(args: RunArgs) -> Result<CommandOutcome> {
    let cfg = run_config(&args)?;
    std::fs::create_dir_all(&args.workspace)
        .with_context(|| format!("create workspace {}", args.workspace.display()))?;
    let store = Arc::new(Store::open(&args.store.store).await?);
    let registry = Arc::new(build_registry(&cfg)?);
    if registry.find(&cfg.provider).is_none() {
        bail!("provider '{}' could not be started", cfg.provider);
    }
    let scheduler = Scheduler::new(scheduler_config(&cfg, &registry));
    let sandbox = SandboxConfig::new(&args.workspace)
        .command_timeout(Duration::from_secs(cfg.tools.command_timeout_secs))
        .output_cap_bytes(cfg.tools.max_output_bytes)
        .allow_network(cfg.tools.allow_network);
    let executor = Arc::new(ToolExecutor::new(sandbox));
    let runtime = Runtime::new(store.clone(), registry, executor, scheduler)
        .with_approval(cfg.tools.approval);
    let created = runtime.create_run(&cfg, RunMode::Coding).await?;
    let state = runtime.drive(&created.id).await?;
    let task_count = store.finished_task_count(&created.id).await?;
    let tokens = store.load_stats(&created.id).await?.usage.total_tokens();
    Ok(CommandOutcome {
        text: format!(
            "run {} {}: {} tasks, {} tokens",
            created.id, state, task_count, tokens
        ),
        success: state == RunStatus::Completed,
    })
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
    let workers: Vec<_> = tasks.iter()
        .filter(|task| task.role == swarm_core::AgentRole::Worker)
        .collect();
    for worker in &workers {
        if let Some(summary) = events.iter().rev()
            .find(|e| e.kind == swarm_core::event::EventKind::Message
                && e.task_id.as_ref() == Some(&worker.id))
            .and_then(|e| e.payload.get("summary").and_then(|v| v.as_str())) {
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
    let initial = store.latest_events(&run.id, 100).await?;
    let mut cursor = initial.last().map_or(0, |e| e.seq);
    let mut lines: Vec<String> = initial.iter().map(format_event).collect();
    if !args.follow {
        return Ok(CommandOutcome::success(lines.join("\n")));
    }
    for line in lines.drain(..) {
        println!("{line}");
    }
    loop {
        let events = store.events_since(&run.id, cursor, 100).await?;
        for event in &events {
            println!("{}", format_event(event));
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
    Ok(CommandOutcome::success(String::new()))
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
        let result = execute(Cli::try_parse_from([
            "swarm", "run", "divide the work", "--provider", "mock", "--agents", "3",
            "--store", store_path.to_str().unwrap(), "--workspace", workspace.to_str().unwrap(),
        ]).unwrap()).await.unwrap();
        assert!(result.success, "{}", result.text);
        let store = Store::open(&store_path).await.unwrap();
        let run = select_run(&store, None).await.unwrap();
        let tasks = store.list_tasks(&run.id).await.unwrap();
        assert_eq!(tasks.len(), 3);
        assert!(tasks.iter().all(|t| t.state == swarm_core::TaskState::Completed));
        let root = tasks.iter().find(|t| t.parent_id.is_none()).unwrap();
        assert_eq!(root.role, swarm_core::AgentRole::Planner);
        assert_eq!(tasks.iter().filter(|t| t.parent_id.as_ref() == Some(&root.id)).count(), 2);
        let status = execute(Cli::try_parse_from([
            "swarm", "status", "--store", store_path.to_str().unwrap(),
        ]).unwrap()).await.unwrap();
        assert!(status.text.contains("result    Complete independent work item"));
    }

    #[test]
    fn rejects_unconfigured_provider() {
        let Cli {
            command: Command::Run(args),
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
            "swarm", "run", "objective from cli", "--config", config_path.to_str().unwrap(),
            "--model", "overridden-mock", "--approve-all", "--store", store_path.to_str().unwrap(),
            "--workspace", workspace.to_str().unwrap(),
        ]).unwrap();
        let result = execute(cli).await.unwrap();
        assert!(result.success, "{}", result.text);
        let store = Store::open(&store_path).await.unwrap();
        let run = select_run(&store, None).await.unwrap();
        assert_eq!(run.objective, "objective from cli");
        assert_eq!(run.model.as_deref(), Some("overridden-mock"));
        let status = execute(Cli::try_parse_from([
            "swarm", "status", run.id.as_str(), "--store", store_path.to_str().unwrap(),
        ]).unwrap()).await.unwrap();
        assert!(status.text.contains("result    [mock:overridden-mock]"));
    }
}
