//! Jobs: swarm as something another agent can drive.
//!
//! `swarm run` blocks until the goal is done, which suits a person at a terminal
//! and nobody else. An orchestrating agent (Hermes, Claude Code, Codex) wants to
//! launch several workers, carry on, and come back for results. A job is a
//! detached `swarm run` with a directory of its own:
//!
//! ```text
//! <jobs>/<id>/job.json   what was asked and the process that is doing it
//! <jobs>/<id>/store/     the run's durable store (events, checkpoints)
//! <jobs>/<id>/run.log    the process output
//! <jobs>/<id>/work/      the workspace, a git worktree when launched with --repo
//! <jobs>/<id>/patch.diff written by `result` for --repo jobs
//! ```
//!
//! Every verb returns JSON, so the caller never parses prose. Results come from
//! the run's own event log: the worker's `report_result` call (structured status,
//! metrics, artifacts), its final message, the files it changed, and for a
//! `--repo` job a patch against the commit it started from.

use std::collections::HashSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use swarm_core::RunStatus;
use swarm_core::event::{EventKind, RunEvent};
use swarm_store::Store;

/// Appended to the goal so a worker knows its caller reads a structured report.
const REPORT_FOOTER: &str = "\n\n---\nWhen you are done, call the report_result tool once with: status \
(success, partial or failed), a short summary, metrics (numbers a caller can compare, such as speedup or \
tests_passed) and artifacts (workspace files worth keeping). You were launched by another program that \
reads that report, so do not skip it.";

#[derive(Debug, Args)]
pub struct JobArgs {
    /// Where jobs live (default: $SWARM_JOBS, else ./.swarm-jobs).
    #[arg(long, global = true)]
    jobs: Option<PathBuf>,
    #[command(subcommand)]
    command: JobCommand,
}

#[derive(Debug, Subcommand)]
pub enum JobCommand {
    /// Start a job in the background and return its id at once.
    Submit(SubmitArgs),
    /// Where a job is: state, progress, last action.
    Status { id: String },
    /// Block until jobs finish (all of them, or the first with --any), or time out.
    Wait {
        #[arg(required = true)]
        ids: Vec<String>,
        /// Give up after this many seconds (default 600).
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// Return as soon as one job has finished.
        #[arg(long)]
        any: bool,
    },
    /// What a job produced: the worker's report, its summary, files, patch.
    Result { id: String },
    /// Stop a running job.
    Cancel { id: String },
    /// List jobs.
    List {
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        state: Option<String>,
    },
    /// Continue an interrupted job (its process died before the run finished).
    Resume { id: String },
    /// Remove a --repo job's worktree and branch (the patch and logs stay).
    Clean { id: String },
}

#[derive(Debug, Args, Default, Clone)]
pub struct SubmitArgs {
    /// What the worker should do.
    #[arg(long)]
    pub goal: Option<String>,
    /// Read the goal from a file instead.
    #[arg(long)]
    pub goal_file: Option<PathBuf>,
    /// An existing directory to work in.
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    /// A git repository: the job gets its own worktree and branch, so parallel
    /// jobs on one repo never touch each other's files.
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Commit, branch or tag to start the worktree from (default HEAD).
    #[arg(long)]
    pub base: Option<String>,
    /// A name for grouping jobs, e.g. one label per experiment.
    #[arg(long)]
    pub label: Option<String>,
    /// Submitting the same key again returns the existing job instead of a new one.
    #[arg(long)]
    pub key: Option<String>,
    /// Provider (mock, local, chatgpt-web, ...); default $SWARM_PROVIDER.
    #[arg(long)]
    pub provider: Option<String>,
    /// Provider endpoint override; default $SWARM_BASE_URL.
    #[arg(long)]
    pub base_url: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub agents: Option<u32>,
    /// Steps (model turns) the worker may take.
    #[arg(long)]
    pub max_steps: Option<u32>,
    /// Do not append the report_result instruction to the goal.
    #[arg(long)]
    pub no_report_footer: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct JobSpec {
    id: String,
    key: Option<String>,
    label: Option<String>,
    goal: String,
    workspace: PathBuf,
    store: PathBuf,
    repo: Option<PathBuf>,
    base: Option<String>,
    base_commit: Option<String>,
    branch: Option<String>,
    pid: Option<u32>,
    started_at: i64,
    #[serde(default)]
    cancelled: bool,
    /// Kept so an interrupted job can be resumed with the same provider.
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

pub async fn run(args: JobArgs) -> Result<(String, bool)> {
    let jobs = jobs_dir(args.jobs.as_deref());
    let (value, ok) = match args.command {
        JobCommand::Submit(submit) => (submit_job(&jobs, submit)?, true),
        JobCommand::Status { id } => {
            let status = job_status(&jobs, &id).await?;
            (status, true)
        }
        JobCommand::Wait { ids, timeout, any } => wait_jobs(&jobs, &ids, timeout, any).await?,
        JobCommand::Result { id } => (job_result(&jobs, &id).await?, true),
        JobCommand::Cancel { id } => (cancel_job(&jobs, &id).await?, true),
        JobCommand::List { label, state } => (list_jobs(&jobs, label.as_deref(), state.as_deref()).await?, true),
        JobCommand::Resume { id } => (resume_job(&jobs, &id).await?, true),
        JobCommand::Clean { id } => (clean_job(&jobs, &id)?, true),
    };
    Ok((serde_json::to_string_pretty(&value)?, ok))
}

pub fn jobs_dir(explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("SWARM_JOBS").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(".swarm-jobs"))
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn new_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("job_{:x}{:04x}", nanos / 1_000_000, (nanos as u64 ^ u64::from(std::process::id())) & 0xffff)
}

fn job_dir(jobs: &Path, id: &str) -> Result<PathBuf> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        bail!("invalid job id `{id}`");
    }
    let dir = jobs.join(id);
    if !dir.join("job.json").is_file() {
        bail!("no such job `{id}` in {}", jobs.display());
    }
    Ok(dir)
}

fn load_spec(jobs: &Path, id: &str) -> Result<(JobSpec, PathBuf)> {
    let dir = job_dir(jobs, id)?;
    let text = fs::read_to_string(dir.join("job.json")).with_context(|| format!("read job {id}"))?;
    Ok((serde_json::from_str(&text).with_context(|| format!("parse job {id}"))?, dir))
}

fn save_spec(dir: &Path, spec: &JobSpec) -> Result<()> {
    fs::write(dir.join("job.json"), serde_json::to_string_pretty(spec)?)?;
    Ok(())
}

fn all_specs(jobs: &Path) -> Vec<(JobSpec, PathBuf)> {
    let Ok(entries) = fs::read_dir(jobs) else { return Vec::new() };
    let mut specs: Vec<(JobSpec, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            let spec: JobSpec = serde_json::from_str(&fs::read_to_string(dir.join("job.json")).ok()?).ok()?;
            Some((spec, dir))
        })
        .collect();
    specs.sort_by_key(|(spec, _)| spec.started_at);
    specs
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| "run git (is it installed?)")?;
    if !output.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn submit_job(jobs: &Path, args: SubmitArgs) -> Result<Value> {
    let goal = match (&args.goal, &args.goal_file) {
        (Some(goal), None) => goal.clone(),
        (None, Some(file)) => fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?,
        _ => bail!("give exactly one of goal or goal_file"),
    };
    if goal.trim().is_empty() {
        bail!("the goal is empty");
    }
    if args.workspace.is_some() && args.repo.is_some() {
        bail!("give workspace or repo, not both");
    }
    let provider = args
        .provider
        .clone()
        .or_else(|| std::env::var("SWARM_PROVIDER").ok())
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| anyhow!("no provider: pass provider (mock, local, chatgpt-web, ...) or set SWARM_PROVIDER"))?;
    let base_url = args.base_url.clone().or_else(|| std::env::var("SWARM_BASE_URL").ok()).filter(|u| !u.is_empty());

    if let Some(key) = &args.key {
        if let Some((existing, _)) = all_specs(jobs).into_iter().find(|(spec, _)| spec.key.as_deref() == Some(key)) {
            return Ok(json!({"job_id": existing.id, "reused": true, "label": existing.label,
                             "workspace": existing.workspace, "note": "a job with this key already exists"}));
        }
    }

    let id = new_id();
    let dir = jobs.join(&id);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let (workspace, repo, base, base_commit, branch) = if let Some(repo) = &args.repo {
        let repo = fs::canonicalize(repo).with_context(|| format!("repo {}", repo.display()))?;
        let base = args.base.clone().unwrap_or_else(|| "HEAD".to_string());
        let commit = git(&repo, &["rev-parse", "--verify", &format!("{base}^{{commit}}")])
            .with_context(|| format!("`{base}` is not a commit in {}", repo.display()))?;
        let branch = format!("swarm/{id}");
        let work = dir.join("work");
        git(&repo, &["worktree", "add", "-b", &branch, &work.to_string_lossy(), &commit])?;
        (work, Some(repo), Some(base), Some(commit), Some(branch))
    } else if let Some(workspace) = &args.workspace {
        fs::create_dir_all(workspace)?;
        (fs::canonicalize(workspace)?, None, None, None, None)
    } else {
        let work = dir.join("work");
        fs::create_dir_all(&work)?;
        (work, None, None, None, None)
    };
    let store = dir.join("store");

    let full_goal = if args.no_report_footer { goal.clone() } else { format!("{goal}{REPORT_FOOTER}") };
    let exe = std::env::var_os("SWARM_JOB_EXE").map(PathBuf::from).map_or_else(std::env::current_exe, Ok)?;
    let log = File::create(dir.join("run.log"))?;
    let mut command = Command::new(exe);
    command
        .arg("run")
        .arg(&full_goal)
        .args(["--provider", &provider])
        .arg("--store")
        .arg(&store)
        .arg("--workspace")
        .arg(&workspace)
        .args(["--approve-all", "--plain"]);
    if let Some(url) = &base_url {
        command.args(["--base-url", url]);
    }
    if let Some(model) = &args.model {
        command.args(["--model", model]);
    }
    if let Some(agents) = args.agents {
        command.args(["--agents", &agents.to_string()]);
    }
    if let Some(steps) = args.max_steps {
        command.args(["--max-steps", &steps.to_string()]);
    }
    command.stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log));
    detach(&mut command);
    let child = command.spawn().context("start the job process")?;
    let pid = child.id();
    drop(child);

    let spec = JobSpec {
        id: id.clone(),
        key: args.key.clone(),
        label: args.label.clone(),
        goal,
        workspace: workspace.clone(),
        store,
        repo,
        base,
        base_commit,
        branch,
        pid: Some(pid),
        started_at: now_ms(),
        cancelled: false,
        provider: Some(provider),
        base_url,
        model: args.model.clone(),
    };
    save_spec(&dir, &spec)?;
    Ok(json!({"job_id": id, "state": "starting", "label": spec.label, "pid": pid,
              "workspace": workspace, "branch": spec.branch, "reused": false}))
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: outlives the caller, no console.
    command.creation_flags(0x0000_0008 | 0x0000_0200);
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

fn pid_alive(pid: u32) -> bool {
    if cfg!(windows) {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
            .unwrap_or(false)
    } else {
        Command::new("kill").args(["-0", &pid.to_string()]).output().map(|o| o.status.success()).unwrap_or(false)
    }
}

fn kill_tree(pid: u32) {
    if cfg!(windows) {
        let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).output();
    } else {
        let _ = Command::new("kill").args(["-TERM", &format!("-{pid}")]).output();
        let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).output();
    }
}

fn is_terminal(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "cancelled" | "interrupted")
}

/// A run's events, oldest first; empty until the job process has made its store.
async fn open_store(spec: &JobSpec) -> Option<Store> {
    if !spec.store.join("swarm.db").is_file() {
        return None;
    }
    Store::open(&spec.store).await.ok()
}

struct Snapshot {
    state: String,
    run: Option<swarm_core::run::Run>,
    events: Vec<RunEvent>,
    store: Option<Store>,
}

async fn snapshot(spec: &JobSpec) -> Snapshot {
    let alive = spec.pid.is_some_and(pid_alive);
    let store = open_store(spec).await;
    let run = match &store {
        Some(store) => store.list_runs().await.ok().and_then(|runs| runs.into_iter().next()),
        None => None,
    };
    let events = match (&store, &run) {
        (Some(store), Some(run)) => store.latest_events(&run.id, 20_000).await.unwrap_or_default(),
        _ => Vec::new(),
    };
    let state = if spec.cancelled {
        "cancelled"
    } else {
        match run.as_ref().map(|run| run.status) {
            Some(RunStatus::Completed) => "completed",
            Some(RunStatus::Failed) => "failed",
            Some(RunStatus::Cancelled) => "cancelled",
            Some(_) if alive => "running",
            Some(_) => "interrupted",
            None if alive => "starting",
            None => "failed",
        }
    }
    .to_string();
    Snapshot { state, run, events, store }
}

fn last_action(events: &[RunEvent]) -> Value {
    events
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::ToolRequested)
        .map(|event| {
            let name = event.payload.get("name").and_then(Value::as_str).unwrap_or("");
            let detail = event.payload.get("arguments").map(|a| a.to_string()).unwrap_or_default();
            json!({"tool": name, "args": detail.chars().take(160).collect::<String>(), "at": event.created_at})
        })
        .unwrap_or(Value::Null)
}

fn tail(path: &Path, lines: usize) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

async fn status_of(spec: &JobSpec, dir: &Path) -> Value {
    let snap = snapshot(spec).await;
    let mut out = json!({
        "job_id": spec.id,
        "label": spec.label,
        "state": snap.state,
        "workspace": spec.workspace,
        "branch": spec.branch,
    });
    let finished_at = snap.run.as_ref().filter(|_| is_terminal(&snap.state)).map(|run| run.updated_at);
    out["elapsed_s"] = json!(((finished_at.unwrap_or_else(now_ms) - spec.started_at).max(0)) / 1000);
    if let (Some(store), Some(run)) = (&snap.store, &snap.run) {
        out["run_id"] = json!(run.id.to_string());
        if let Ok(stats) = store.load_stats(&run.id).await {
            out["inference_calls"] = json!(stats.usage.inference_calls);
            out["tool_calls"] = json!(stats.usage.tool_calls);
            out["tokens"] = json!(stats.usage.total_tokens());
        }
        out["last_action"] = last_action(&snap.events);
        out["reported"] = json!(reported_result(&snap.events).is_some());
    }
    if matches!(snap.state.as_str(), "failed" | "interrupted") {
        out["log_tail"] = json!(tail(&dir.join("run.log"), 12));
    }
    out
}

pub async fn job_status(jobs: &Path, id: &str) -> Result<Value> {
    let (spec, dir) = load_spec(jobs, id)?;
    Ok(status_of(&spec, &dir).await)
}

pub async fn wait_jobs(jobs: &Path, ids: &[String], timeout_secs: u64, any: bool) -> Result<(Value, bool)> {
    for id in ids {
        job_dir(jobs, id)?;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        let mut statuses = Vec::new();
        for id in ids {
            statuses.push(job_status(jobs, id).await?);
        }
        let finished = |status: &Value| status["state"].as_str().is_some_and(is_terminal);
        let done = if any { statuses.iter().any(finished) } else { statuses.iter().all(finished) };
        let timed_out = !done && std::time::Instant::now() >= deadline;
        if done || timed_out {
            let all_ok = statuses.iter().all(|s| s["state"] == "completed");
            let still: Vec<&Value> = statuses.iter().filter(|s| !finished(s)).map(|s| &s["job_id"]).collect();
            return Ok((json!({"done": done, "timed_out": timed_out, "still_running": still, "jobs": statuses}), done && all_ok));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// The arguments of the worker's last accepted `report_result` call.
pub fn reported_result(events: &[RunEvent]) -> Option<Value> {
    let accepted: HashSet<&str> = events
        .iter()
        .filter(|event| {
            event.kind == EventKind::ToolResult
                && event.payload.get("name").and_then(Value::as_str) == Some("report_result")
                && event.payload.get("success").and_then(Value::as_bool) == Some(true)
        })
        .filter_map(|event| event.payload.get("call_id").and_then(Value::as_str))
        .collect();
    events
        .iter()
        .rev()
        .find(|event| {
            event.kind == EventKind::ToolRequested
                && event.payload.get("name").and_then(Value::as_str) == Some("report_result")
                && event.payload.get("call_id").and_then(Value::as_str).is_some_and(|id| accepted.contains(id))
        })
        .and_then(|event| event.payload.get("arguments").cloned())
}

fn final_summary(events: &[RunEvent]) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::Message && event.payload.get("summary").is_some())
        .and_then(|event| event.payload.get("summary").and_then(Value::as_str).map(str::to_string))
}

fn changed_files(events: &[RunEvent]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for event in events.iter().filter(|e| e.kind == EventKind::ToolResult) {
        let changed = event.payload.get("change").is_some_and(|c| !c.is_null());
        if let Some(path) = event.payload.get("arguments").and_then(|a| a.get("path")).and_then(Value::as_str) {
            if changed && seen.insert(path.to_string()) {
                files.push(path.to_string());
            }
        }
    }
    files
}

/// For a `--repo` job: everything the worker changed since the commit it started
/// from, as a patch file, plus a one-line stat. New files are included.
fn write_patch(spec: &JobSpec, dir: &Path) -> Option<Value> {
    let base = spec.base_commit.as_deref()?;
    let ws = &spec.workspace;
    if !ws.exists() {
        return None;
    }
    git(ws, &["add", "-N", "."]).ok();
    let patch = Command::new("git").arg("-C").arg(ws).args(["diff", "--binary", base]).output().ok()?;
    if !patch.status.success() {
        return None;
    }
    let path = dir.join("patch.diff");
    fs::write(&path, &patch.stdout).ok()?;
    let stat = git(ws, &["diff", "--shortstat", base]).unwrap_or_default();
    Some(json!({"path": path, "bytes": patch.stdout.len(), "stat": stat,
                "apply": format!("git apply \"{}\"", path.display())}))
}

pub async fn job_result(jobs: &Path, id: &str) -> Result<Value> {
    let (spec, dir) = load_spec(jobs, id)?;
    let snap = snapshot(&spec).await;
    let mut out = status_of(&spec, &dir).await;
    let reported = reported_result(&snap.events);
    if let Some(report) = &reported {
        let artifacts: Vec<Value> = report
            .get("artifacts")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(|rel| json!({"path": rel, "absolute": spec.workspace.join(rel)}))
                    .collect()
            })
            .unwrap_or_default();
        out["report"] = report.clone();
        out["artifacts"] = Value::Array(artifacts);
    } else {
        out["report"] = Value::Null;
    }
    out["summary"] = json!(final_summary(&snap.events));
    out["files_changed"] = json!(changed_files(&snap.events));
    if let Some(patch) = write_patch(&spec, &dir) {
        out["patch"] = patch;
    }
    if !is_terminal(&snap.state) {
        out["note"] = json!("the job has not finished; this is a partial view");
    } else if reported.is_none() {
        out["note"] = json!("the worker did not call report_result; read summary and files_changed instead");
    }
    Ok(out)
}

pub async fn cancel_job(jobs: &Path, id: &str) -> Result<Value> {
    let (mut spec, dir) = load_spec(jobs, id)?;
    let snap = snapshot(&spec).await;
    if is_terminal(&snap.state) && snap.state != "interrupted" {
        return Ok(json!({"job_id": id, "state": snap.state, "note": "already finished"}));
    }
    if let Some(pid) = spec.pid {
        kill_tree(pid);
    }
    spec.cancelled = true;
    save_spec(&dir, &spec)?;
    Ok(json!({"job_id": id, "state": "cancelled"}))
}

/// Continue a job whose process died before its run finished: same store, same
/// workspace, same provider, a new process.
pub async fn resume_job(jobs: &Path, id: &str) -> Result<Value> {
    let (mut spec, dir) = load_spec(jobs, id)?;
    let snap = snapshot(&spec).await;
    if snap.state != "interrupted" {
        bail!("job {id} is {}; only an interrupted job can be resumed", snap.state);
    }
    let run = snap.run.ok_or_else(|| anyhow!("job {id} has no run to resume"))?;
    let exe = std::env::var_os("SWARM_JOB_EXE").map(PathBuf::from).map_or_else(std::env::current_exe, Ok)?;
    let log = fs::OpenOptions::new().create(true).append(true).open(dir.join("run.log"))?;
    let mut command = Command::new(exe);
    command
        .arg("resume")
        .arg(run.id.to_string())
        .arg("--store")
        .arg(&spec.store)
        .arg("--workspace")
        .arg(&spec.workspace)
        .args(["--approve-all", "--plain"]);
    if let Some(provider) = &spec.provider {
        command.args(["--provider", provider]);
    }
    if let Some(url) = &spec.base_url {
        command.args(["--base-url", url]);
    }
    if let Some(model) = &spec.model {
        command.args(["--model", model]);
    }
    command.stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log));
    detach(&mut command);
    let child = command.spawn().context("start the resumed job process")?;
    spec.pid = Some(child.id());
    drop(child);
    spec.cancelled = false;
    save_spec(&dir, &spec)?;
    Ok(json!({"job_id": id, "state": "starting", "run_id": run.id.to_string(), "resumed": true}))
}

pub async fn list_jobs(jobs: &Path, label: Option<&str>, state: Option<&str>) -> Result<Value> {
    let mut rows = Vec::new();
    for (spec, dir) in all_specs(jobs) {
        if label.is_some() && spec.label.as_deref() != label {
            continue;
        }
        let status = status_of(&spec, &dir).await;
        if state.is_some() && status["state"].as_str() != state {
            continue;
        }
        rows.push(status);
    }
    Ok(json!({"jobs": rows}))
}

pub fn clean_job(jobs: &Path, id: &str) -> Result<Value> {
    let (spec, dir) = load_spec(jobs, id)?;
    if spec.pid.is_some_and(pid_alive) {
        bail!("job {id} is still running; cancel it first");
    }
    let (Some(repo), Some(branch)) = (&spec.repo, &spec.branch) else {
        return Ok(json!({"job_id": id, "removed": [], "note": "not a --repo job; nothing to clean"}));
    };
    // Keep the patch before the worktree disappears.
    write_patch(&spec, &dir);
    let mut removed = Vec::new();
    if spec.workspace.exists() {
        git(repo, &["worktree", "remove", "--force", &spec.workspace.to_string_lossy()])?;
        removed.push("worktree");
    }
    if git(repo, &["branch", "-D", branch]).is_ok() {
        removed.push("branch");
    }
    Ok(json!({"job_id": id, "removed": removed, "patch_kept": dir.join("patch.diff").is_file()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::RunId;

    fn event(kind: EventKind, payload: Value) -> RunEvent {
        RunEvent::new(kind, RunId::new(), payload)
    }

    fn call(id: &str, name: &str, args: Value) -> Vec<RunEvent> {
        vec![event(EventKind::ToolRequested, json!({"call_id": id, "name": name, "arguments": args}))]
    }

    fn result(id: &str, name: &str, ok: bool, args: Value, change: Value) -> RunEvent {
        event(
            EventKind::ToolResult,
            json!({"call_id": id, "name": name, "success": ok, "arguments": args, "change": change}),
        )
    }

    #[test]
    fn the_last_accepted_report_wins_and_a_rejected_one_is_ignored() {
        let first = json!({"status": "partial", "summary": "half", "metrics": {"speedup": 1.1}});
        let second = json!({"status": "success", "summary": "done", "metrics": {"speedup": 1.9}});
        let rejected = json!({"status": "great", "summary": "x"});
        let mut events = call("a", "report_result", first.clone());
        events.push(result("a", "report_result", true, first, Value::Null));
        events.extend(call("b", "report_result", second.clone()));
        events.push(result("b", "report_result", true, second.clone(), Value::Null));
        events.extend(call("c", "report_result", rejected.clone()));
        events.push(result("c", "report_result", false, rejected, Value::Null));
        assert_eq!(reported_result(&events), Some(second));
        assert_eq!(reported_result(&[]), None);
    }

    #[test]
    fn a_report_that_was_requested_but_never_accepted_is_not_a_result() {
        let args = json!({"status": "success", "summary": "x"});
        assert_eq!(reported_result(&call("a", "report_result", args)), None);
    }

    #[test]
    fn changed_files_lists_each_file_a_tool_really_changed_once() {
        let events = vec![
            result("1", "write_file", true, json!({"path": "a.cu"}), json!({"before": null})),
            result("2", "read_file", true, json!({"path": "b.cu"}), Value::Null),
            result("3", "edit_file", true, json!({"path": "a.cu"}), json!({"before": "x"})),
            result("4", "write_file", true, json!({"path": "c.cu"}), json!({"before": null})),
        ];
        assert_eq!(changed_files(&events), ["a.cu", "c.cu"]);
    }

    #[test]
    fn ids_are_safe_path_components() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["", "../x", "a/b", "a b", "a\\b"] {
            assert!(job_dir(dir.path(), bad).is_err(), "{bad}");
        }
        assert!(job_dir(dir.path(), "job_1").is_err(), "unknown but well-formed ids fail as unknown");
    }

    #[test]
    fn submit_needs_a_goal_a_provider_and_one_place_to_work() {
        let dir = tempfile::tempdir().unwrap();
        let ok_provider = SubmitArgs { goal: Some("g".into()), provider: Some("mock".into()), ..Default::default() };
        let none = SubmitArgs { provider: Some("mock".into()), ..Default::default() };
        assert!(submit_job(dir.path(), none).unwrap_err().to_string().contains("goal"));
        let both = SubmitArgs {
            workspace: Some(dir.path().join("w")),
            repo: Some(dir.path().join("r")),
            ..ok_provider.clone()
        };
        assert!(submit_job(dir.path(), both).unwrap_err().to_string().contains("not both"));
        let no_provider = SubmitArgs { provider: None, ..ok_provider };
        if std::env::var("SWARM_PROVIDER").is_err() {
            assert!(submit_job(dir.path(), no_provider).unwrap_err().to_string().contains("no provider"));
        }
    }
}
