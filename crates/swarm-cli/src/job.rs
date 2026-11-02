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
use swarm_core::config::{ToolProtocolSelection, TaskPreset};
use crate::job_policy::{JobPolicy, audit_review, work_calls};
use swarm_core::event::{EventKind, RunEvent};
use swarm_store::Store;

/// Token budget of a job unless the caller sets one.
const DEFAULT_JOB_MAX_TOKENS: u64 = 5_000_000;

/// Worker runs a --verify job gets, and the time one check may take.
const DEFAULT_VERIFY_ROUNDS: u32 = 3;
const DEFAULT_VERIFY_TIMEOUT_SECS: u64 = 1_800;

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
    /// Continue a job: one that was interrupted (its process died before the run
    /// finished), or one that died on its budget, which needs a raised ceiling.
    Resume {
        id: String,
        /// Raise the token ceiling for the resumed run (whole-job total).
        #[arg(long)]
        max_tokens: Option<u64>,
        /// Raise the per-agent step ceiling (also raises inference calls in step).
        #[arg(long)]
        max_steps: Option<u32>,
        #[arg(long)]
        max_inference_calls: Option<u64>,
        #[arg(long)]
        max_wall_clock_secs: Option<u64>,
    },
    /// Remove a --repo job's worktree and branch (the patch and logs stay).
    Clean { id: String },
    /// Runs a --verify job's worker rounds and verifications (started by `submit`; not for people).
    #[command(hide = true)]
    Supervise { id: String },
}

#[derive(Debug, Args, Default, Clone)]
pub struct SubmitArgs {
    #[command(flatten)]
    pub policy: JobPolicy,
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
    /// Tool transport override, also saved for verification rounds and resume.
    #[arg(long, env = "SWARM_TOOL_PROTOCOL")]
    pub tool_protocol: Option<ToolProtocolSelection>,
    /// Full provider configuration, including native options and endpoint pools.
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub agents: Option<u32>,
    /// Steps (model turns) the worker may take.
    #[arg(long)]
    pub max_steps: Option<u32>,
    /// Inference calls the worker may make (defaults to max-steps x agents).
    #[arg(long)]
    pub max_inference_calls: Option<u64>,
    /// Wall-clock limit for the job in seconds (default 3600).
    #[arg(long)]
    pub max_wall_clock_secs: Option<u64>,
    /// Tokens the worker may use over the whole job, input plus output, counted on
    /// every call (the whole conversation is sent each time). Default 5000000.
    #[arg(long)]
    pub max_tokens: Option<u64>,
    /// Do not append the report_result instruction to the goal.
    #[arg(long)]
    pub no_report_footer: bool,
    /// A command run in the workspace once the worker stops; exit code 0 means the
    /// work holds. When it fails, its output goes to a fresh worker run on the same
    /// workspace, up to --max-rounds. The job's outcome is `verified` only when it passes.
    #[arg(long)]
    pub verify: Option<String>,
    /// Worker runs a --verify job may use, the first one included (default 3).
    #[arg(long)]
    pub max_rounds: Option<u32>,
    /// Seconds a single --verify run may take before it counts as failed (default 1800).
    #[arg(long)]
    pub verify_timeout_secs: Option<u64>,
    /// Refs the worker may `git push` to (branch, `remote/branch`, or `*`). Repeatable.
    /// With none, every push the worker attempts is refused by the tool layer.
    #[arg(long = "allow-push")]
    pub allow_push: Vec<String>,
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
    tool_protocol: ToolProtocolSelection,
    #[serde(default)]
    model: Option<String>,
    /// The worker's limits, kept so a verify job's later rounds run with the same ones.
    #[serde(default)]
    agents: Option<u32>,
    #[serde(default)]
    max_steps: Option<u32>,
    #[serde(default)]
    max_inference_calls: Option<u64>,
    #[serde(default)]
    max_wall_clock_secs: Option<u64>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    no_report_footer: bool,
    /// Verification: the command, how many worker runs it may use, and how long it may take.
    #[serde(default)]
    verify: Option<String>,
    #[serde(default)]
    max_rounds: u32,
    #[serde(default)]
    verify_timeout_secs: u64,
    /// Refs the worker's shell may push to; empty blocks every push. Kept on the job so a
    /// resume and a verify job's later rounds run under the same grant.
    #[serde(default)]
    allow_push: Vec<String>,
    /// Where a verify job's supervisor has got to (`verify.json`); not part of job.json.
    #[serde(skip)]
    live: Option<VerifyState>,
    #[serde(default)]
    policy: JobPolicy,
    #[serde(default)]
    baseline_gate: Option<Value>,
    #[serde(default)]
    freshness_ref: Option<String>,
    #[serde(default)]
    freshness_commit: Option<String>,
    #[serde(default)]
    config: Option<PathBuf>,
}

/// A verify job's progress. Only the supervisor writes it, so it never races the
/// `cancelled` flag in job.json.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
struct VerifyState {
    /// `worker`, `verifying` or `done`.
    phase: String,
    round: u32,
    /// The store of the round in progress (the latest round once done).
    store: PathBuf,
    /// One entry per finished verification: round, passed, exit_code, output (tail).
    rounds: Vec<Value>,
    passed: Option<bool>,
}

/// Verify output kept per round, and handed back to the next worker: the end of it is
/// where a failing test says why, and every call of the worker carries it.
const VERIFY_OUTPUT_CAP: usize = 4_000;

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
        JobCommand::Resume { id, max_tokens, max_steps, max_inference_calls, max_wall_clock_secs } => {
            let raise = BudgetRaise { max_tokens, max_steps, max_inference_calls, max_wall_clock_secs };
            (resume_job(&jobs, &id, raise).await?, true)
        }
        JobCommand::Clean { id } => (clean_job(&jobs, &id)?, true),
        JobCommand::Supervise { id } => (supervise_job(&jobs, &id).await?, true),
    };
    Ok((serde_json::to_string_pretty(&value)?, ok))
}

pub fn jobs_dir(explicit: Option<&Path>) -> PathBuf {
    let root = explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("SWARM_JOBS").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(".swarm-jobs"));
    std::path::absolute(&root).unwrap_or(root)
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

/// Fold a verify job's `verify.json` into its spec: the store to read is the latest
/// round's, and `live` says whether the supervisor is still going.
fn overlay_verify(spec: &mut JobSpec, dir: &Path) {
    if spec.verify.is_none() {
        return;
    }
    if let Some(state) = fs::read_to_string(dir.join("verify.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<VerifyState>(&text).ok())
    {
        if !state.store.as_os_str().is_empty() {
            spec.store = state.store.clone();
        }
        spec.live = Some(state);
    }
}

fn load_spec(jobs: &Path, id: &str) -> Result<(JobSpec, PathBuf)> {
    let dir = job_dir(jobs, id)?;
    let text = fs::read_to_string(dir.join("job.json")).with_context(|| format!("read job {id}"))?;
    let mut spec: JobSpec = serde_json::from_str(&text).with_context(|| format!("parse job {id}"))?;
    overlay_verify(&mut spec, &dir);
    Ok((spec, dir))
}

fn save_spec(dir: &Path, spec: &JobSpec) -> Result<()> {
    let temp = dir.join(format!("job-{}.tmp", swarm_core::RunId::new()));
    fs::write(&temp, serde_json::to_string_pretty(spec)?)?;
    fs::rename(temp, dir.join("job.json"))?;
    Ok(())
}

fn all_specs(jobs: &Path) -> Vec<(JobSpec, PathBuf)> {
    let Ok(entries) = fs::read_dir(jobs) else { return Vec::new() };
    let mut specs: Vec<(JobSpec, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            let mut spec: JobSpec = serde_json::from_str(&fs::read_to_string(dir.join("job.json")).ok()?).ok()?;
            overlay_verify(&mut spec, &dir);
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
    if args.policy.max_active_jobs == 0 { bail!("--max-active-jobs must be positive"); }
    if args.policy.needs_red_gate() && args.verify.as_deref().unwrap_or("").trim().is_empty() {
        bail!("builder/test-writer jobs require --verify with a failing baseline check; use --preset reviewer for reviews or --preset general for exploration");
    }
    let configured = args.config.as_ref().map(|path| -> Result<swarm_core::config::RunConfig> {
        Ok(toml::from_str(&fs::read_to_string(path)?)?)
    }).transpose()?;
    let configured_provider = configured.as_ref().map(|cfg| cfg.provider.to_string());
    let provider = args
        .provider
        .clone()
        .or(configured_provider)
        .or_else(|| std::env::var("SWARM_PROVIDER").ok())
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| anyhow!("no provider: pass provider (mock, local, chatgpt-web, ...) or set SWARM_PROVIDER"))?;
    let base_url = args.base_url.clone().or_else(|| std::env::var("SWARM_BASE_URL").ok()).filter(|u| !u.is_empty());

    let tool_protocol = match args.tool_protocol {
        Some(protocol) => protocol,
        None => match std::env::var("SWARM_TOOL_PROTOCOL") {
            Ok(value) => value.parse()?,
            Err(_) => configured.as_ref().map(|cfg| cfg.tool_protocol).unwrap_or_default(),
        },
    };

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
        let freshness_ref = args.policy.freshness_ref.clone().or_else(||
            git(&repo, &["rev-parse", "--verify", "origin/main"]).ok().map(|_| "origin/main".to_string()));
        if let Some(reference) = freshness_ref {
            let current = git(&repo, &["rev-parse", "--verify", &format!("{reference}^{{commit}}")])?;
            if git(&repo, &["merge-base", "--is-ancestor", &current, &commit]).is_err() {
                bail!("stale base: {base} does not include {reference} ({current}); refresh before dispatch");
            }
        }
        let branch = format!("codex/{id}");
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
    let verify = args.verify.clone().filter(|command| !command.trim().is_empty());
    if args.verify.is_some() && verify.is_none() {
        bail!("--verify is empty");
    }
    let mut spec = JobSpec {
        id: id.clone(),
        key: args.key.clone(),
        label: args.label.clone(),
        goal,
        workspace: workspace.clone(),
        store: dir.join("store"),
        repo,
        base,
        base_commit,
        branch,
        pid: None,
        started_at: now_ms(),
        cancelled: false,
        provider: Some(provider),
        base_url,
        tool_protocol,
        model: args.model.clone(),
        agents: args.agents,
        max_steps: args.max_steps,
        max_inference_calls: args.max_inference_calls,
        max_wall_clock_secs: args.max_wall_clock_secs,
        max_tokens: args.max_tokens,
        no_report_footer: args.no_report_footer,
        max_rounds: if verify.is_some() { args.max_rounds.unwrap_or(DEFAULT_VERIFY_ROUNDS).max(1) } else { 0 },
        verify_timeout_secs: args.verify_timeout_secs.unwrap_or(DEFAULT_VERIFY_TIMEOUT_SECS).max(1),
        verify,
        allow_push: args.allow_push.clone(),
        live: None,
        policy: args.policy.clone(),
        baseline_gate: None,
        freshness_ref: None,
        freshness_commit: None,
        config: args.config.as_ref().map(fs::canonicalize).transpose()?,
    };

    if let Some(repo) = &spec.repo {
        let reference = spec.policy.freshness_ref.clone().or_else(||
            git(repo, &["rev-parse", "--verify", "origin/main"]).ok().map(|_| "origin/main".to_string()));
        if let Some(reference) = reference {
            let current = git(repo, &["rev-parse", "--verify", &format!("{reference}^{{commit}}")])?;
            if git(repo, &["merge-base", "--is-ancestor", &current, spec.base_commit.as_deref().unwrap()]).is_err() {
                bail!("stale base: {} does not include {reference} ({current}); refresh the base before dispatch", spec.base.as_deref().unwrap_or("HEAD"));
            }
            spec.freshness_ref = Some(reference);
            spec.freshness_commit = Some(current);
        }
    }
    if spec.policy.sandbox_image.is_none() { spec.policy.sandbox_image = configured.as_ref().and_then(|cfg| cfg.tools.docker_image.clone()); }
    if spec.policy.sandbox_image.is_some() && !spec.allow_push.is_empty() { bail!("isolated workers cannot receive push grants"); }
    if spec.policy.needs_red_gate() {
        let command = spec.verify.as_deref().unwrap();
        let (code, output) = baseline_check(&spec, command)?;
        spec.baseline_gate = Some(json!({"command":command, "exit_code":code, "output":output, "red":code != 0}));
        save_spec(&dir, &spec)?;
        if code == 0 { bail!("baseline gate is green: `{command}` already passes; refusing builder dispatch"); }
    }
    if spec.policy.sandbox_image.is_none() {
        spec.policy.sandbox_image = configured.as_ref().and_then(|cfg| cfg.tools.docker_image.clone());
    }
    let exe = job_exe()?;
    let log = File::create(dir.join("run.log"))?;
    // Every job enters the same bounded, priority-aware queue.
    save_spec(&dir, &spec)?;
    let mut command = Command::new(exe);
    command.arg("job").arg("--jobs").arg(jobs).arg("supervise").arg(&id);
    command.env("SWARM_JOB_ID", &id);
    command.stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log));
    detach(&mut command);
    let child = command.spawn().context("start the job process")?;
    let pid = child.id();
    drop(child);

    spec.pid = Some(pid);
    save_spec(&dir, &spec)?;
    Ok(json!({"job_id": id, "state": "starting", "label": spec.label, "pid": pid,
              "workspace": workspace, "branch": spec.branch, "reused": false,
              "verify": spec.verify, "max_rounds": spec.verify.as_ref().map(|_| spec.max_rounds)}))
}

fn job_exe() -> Result<PathBuf> {
    Ok(std::env::var_os("SWARM_JOB_EXE").map(PathBuf::from).map_or_else(std::env::current_exe, Ok)?)
}

fn baseline_check(spec: &JobSpec, command: &str) -> Result<(i32, String)> {
    let isolated = verification_container(spec, command)?;
    let mut process: Command = if let Some(container) = &isolated {
        let original = container.command.as_std();
        let mut process = Command::new(original.get_program()); process.args(original.get_args()); process
    } else { swarm_tools::shell::shell().command(command).into_std() };
    process.current_dir(&spec.workspace).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = process.spawn().context("start baseline gate")?;
    let pid = child.id();
    let (send, recv) = std::sync::mpsc::channel();
    std::thread::spawn(move || { let _ = send.send(child.wait_with_output()); });
    match recv.recv_timeout(Duration::from_secs(spec.verify_timeout_secs)) {
        Ok(result) => {
            let output = result?;
            let code = output.status.code().ok_or_else(|| anyhow!("baseline gate terminated without exit code"))?;
            if code == 126 || code == 127 || code == 9009 || (code == 125 && spec.policy.sandbox_image.is_some()) {
                bail!("baseline check could not execute (exit {code}): {}", String::from_utf8_lossy(&output.stderr));
            }
            let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            Ok((code, tail_chars(&text, VERIFY_OUTPUT_CAP)))
        }
        Err(_) => { kill_tree(pid); bail!("baseline gate timed out; timeout is not a red gate"); }
    }
}

fn verification_container(spec: &JobSpec, command: &str) -> Result<Option<swarm_tools::isolation::ContainerCommand>> {
    let Some(image) = &spec.policy.sandbox_image else { return Ok(None); };
    let config = swarm_tools::SandboxConfig::new(&spec.workspace).docker_image(Some(image.clone()));
    swarm_tools::isolation::prepare_repository(&config).map_err(|e| anyhow!(e))?;
    Ok(Some(swarm_tools::isolation::container_command(&config, "/bin/sh", &["-c".into(), command.into()]).map_err(|e| anyhow!(e))?))
}

fn freshness(spec: &JobSpec) -> Value {
    let Some(reference) = &spec.freshness_ref else {
        return json!({"base_drift":null, "checked":false});
    };
    let current = spec.repo.as_ref().and_then(|repo| git(repo, &["rev-parse", "--verify", &format!("{reference}^{{commit}}")]).ok());
    json!({"checked":current.is_some(), "reference":reference,
        "submitted_commit":spec.freshness_commit, "current_commit":current,
        "base_drift":current.as_ref().zip(spec.freshness_commit.as_ref()).map(|(a,b)| a != b)})
}

impl JobSpec {
    /// The goal as the worker sees it: the caller's text plus the report instruction.
    fn full_goal(&self) -> String {
        let mut goal = if self.no_report_footer { self.goal.clone() } else { format!("{}{REPORT_FOOTER}", self.goal) };
        if self.policy.preset == TaskPreset::Reviewer {
            goal.push_str("\nREVIEW CONTRACT: read relevant diffs/files and attempt real checks. Do not modify files. Call report_result with status, summary, verdict (APPROVE, REQUEST_CHANGES, INCONCLUSIVE), blockers (file, line, claim, verification: executed|code_inspection|unverified, repro_command and observed_output or null), and checks (command, observed_output). Use exact commands and an exact nonempty output excerpt. Swarm matches evidence against recorded tool results. Code inspection is not an executed reproduction. An incomplete review must be INCONCLUSIVE.");
        }
        goal
    }
}

/// `swarm run` for this job, writing to `store`. The caller sets stdio.
fn worker_command(spec: &JobSpec, goal: &str, store: &Path) -> Result<Command> {
    let mut command = Command::new(job_exe()?);
    command
        .arg("run")
        .arg(goal)
        .args(["--provider", spec.provider.as_deref().unwrap_or_default()])
        .arg("--store")
        .arg(store)
        .arg("--workspace")
        .arg(&spec.workspace)
        .args(["--approve-all", "--plain"]);
    command.args(["--tool-protocol", spec.tool_protocol.as_str()]);
    if let Some(config) = &spec.config { command.arg("--config").arg(config); }
    if let Some(url) = &spec.base_url {
        command.args(["--base-url", url]);
    }
    command.args(["--preset", spec.policy.preset.as_str()]);
    if let Some(image) = &spec.policy.sandbox_image { command.args(["--sandbox-image", image]); }
    if let Some(model) = &spec.model {
        command.args(["--model", model]);
    }
    for granted in &spec.allow_push {
        command.args(["--allow-push", granted]);
    }
    if let Some(agents) = spec.agents {
        command.args(["--agents", &agents.to_string()]);
    }
    if let Some(steps) = spec.max_steps.or_else(|| spec.policy.preset.limits().map(|(_, steps)| steps)) {
        command.args(["--max-steps", &steps.to_string()]);
    }
    if let Some(calls) = spec.max_inference_calls {
        command.args(["--max-inference-calls", &calls.to_string()]);
    }
    if let Some(secs) = spec.max_wall_clock_secs {
        command.args(["--max-wall-clock-secs", &secs.to_string()]);
    }
    // Jobs are autonomous and read real code, so they get far more than the 200000
    // a plain run does (that figure is the offline mock's); it is still a bound.
    command.args(["--max-tokens", &spec.max_tokens.or_else(|| spec.policy.preset.limits().map(|(tokens, _)| tokens)).unwrap_or(DEFAULT_JOB_MAX_TOKENS).to_string()]);
    Ok(command)
}

/// What a verify job's next worker is told: the original goal, then why the last
/// attempt did not hold. The workspace still has that attempt's changes.
fn retry_goal(full_goal: &str, verify: &str, round: u32, exit_code: Option<i32>, output: &str) -> String {
    let code = exit_code.map_or_else(|| "none (it was killed or timed out)".to_string(), |code| code.to_string());
    format!(
        "{full_goal}\n\n---\nVERIFICATION FAILED after attempt {round}. A check runs when you stop, and it failed.\n\
         Check command: {verify}\nExit code: {code}\nOutput (end):\n```\n{output}\n```\n\
         The workspace still holds the changes that attempt made (see `git status` and `git diff`). Find the cause \
         and fix it, then run the check command yourself before you stop. Do not weaken, skip or edit the check to \
         make it pass."
    )
}

/// The last `cap` bytes of `text`, on a character boundary.
fn tail_chars(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("\u{2026}{}", &text[start..])
}

fn write_verify(dir: &Path, state: &VerifyState) -> Result<()> {
    let temp = dir.join("verify.json.tmp");
    fs::write(&temp, serde_json::to_string_pretty(state)?)?;
    fs::rename(&temp, dir.join("verify.json"))?;
    Ok(())
}

/// Run `command` in the workspace: its exit code (None when killed or timed out)
/// and the end of its combined output.
async fn run_verify(spec: &JobSpec, command: &str) -> (Option<i32>, String) {
    let mut isolated = match verification_container(spec, command) {
        Ok(value) => value, Err(error) => return (None, error.to_string()),
    };
    let mut host_process = swarm_tools::shell::shell().command(command);
    let process = match &mut isolated { Some(container) => &mut container.command, None => &mut host_process };
    process
        .current_dir(&spec.workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = match process.spawn() {
        Ok(child) => child,
        Err(error) => return (None, format!("could not start the check: {error}")),
    };
    let pid = child.id();
    let wait = tokio::time::timeout(Duration::from_secs(spec.verify_timeout_secs), child.wait_with_output());
    match wait.await {
        Ok(Ok(output)) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            (output.status.code(), tail_chars(text.trim(), VERIFY_OUTPUT_CAP))
        }
        Ok(Err(error)) => (None, format!("the check failed to run: {error}")),
        Err(_) => {
            if let Some(pid) = pid {
                kill_tree(pid);
            }
            (None, format!("the check ran past {} seconds and was stopped", spec.verify_timeout_secs))
        }
    }
}

/// The process behind a --verify job: worker, check, and if the check fails a new
/// worker told why, until the check passes or the rounds run out. It records where it
/// is in verify.json, and leaves job.json (and so `cancel`) alone.
pub async fn supervise_job(jobs: &Path, id: &str) -> Result<Value> {
    let (spec, dir) = load_spec(jobs, id)?;
    let _capacity = await_job_slot(jobs, &spec, &dir).await?;
    let resume_args = dir.join("resume-args.json");
    if spec.verify.is_none() || resume_args.exists() {
        let mut worker = if resume_args.exists() {
            let args: Vec<String> = serde_json::from_slice(&fs::read(&resume_args)?)?;
            let mut command = Command::new(job_exe()?); command.args(args); command
        } else { worker_command(&spec, &spec.full_goal(), &spec.store)? };
        let log = fs::OpenOptions::new().create(true).append(true).open(dir.join("run.log"))?;
        worker.stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log));
        let status = tokio::task::spawn_blocking(move || worker.status()).await??;
        let _ = fs::remove_file(resume_args);
        fs::write(dir.join("dispatch.json"), json!({"phase":"done"}).to_string())?;
        return Ok(json!({"job_id":id,"worker_success":status.success()}));
    }
    let verify = spec.verify.clone().unwrap();
    let base_goal = spec.full_goal();
    let mut state = VerifyState::default();
    let mut goal = base_goal.clone();
    for round in 1..=spec.max_rounds {
        let store = if round == 1 { dir.join("store") } else { dir.join(format!("store.r{round}")) };
        state.round = round;
        state.phase = "worker".to_string();
        state.store = store.clone();
        write_verify(&dir, &state)?;
        println!("[verify] round {round}/{}: worker starting", spec.max_rounds);

        let log = fs::OpenOptions::new().create(true).append(true).open(dir.join("run.log"))?;
        let mut worker = worker_command(&spec, &goal, &store)?;
        worker.stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log));
        let status = tokio::task::spawn_blocking(move || worker.status()).await??;
        println!("[verify] round {round}: worker exited ({status})");

        state.phase = "verifying".to_string();
        write_verify(&dir, &state)?;
        let (code, output) = run_verify(&spec, &verify).await;
        let passed = code == Some(0);
        println!("[verify] round {round}: check {} (exit {code:?})", if passed { "passed" } else { "failed" });
        state.rounds.push(json!({"round": round, "passed": passed, "exit_code": code, "output": output}));
        state.passed = Some(passed);
        if passed {
            break;
        }
        goal = retry_goal(&base_goal, &verify, round, code, &output);
    }
    state.phase = "done".to_string();
    write_verify(&dir, &state)?;
    fs::write(dir.join("dispatch.json"), json!({"phase":"done"}).to_string())?;
    Ok(json!({"job_id": id, "passed": state.passed, "rounds": state.rounds.len()}))
}

/// Keep a helper process (tasklist, taskkill) from flashing a console window when
/// it is started from a process that has no visible console of its own.
#[cfg(windows)]
fn hide_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_window(_command: &mut Command) {}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP: outlives the caller and owns a
    // hidden console that its children inherit. DETACHED_PROCESS (no console at all)
    // made every console program the worker ran (git, go, python, cmd) open its own
    // visible terminal window.
    command.creation_flags(0x0800_0000 | 0x0000_0200);
    stop_inheriting_stdio();
}

/// Windows lets a child inherit every inheritable handle its parent has, not only
/// the three it was given. A caller that captures our output (a shell tool, an MCP
/// host) hands us an inheritable pipe, the worker inherits it, and the pipe never
/// closes until the worker exits, so `submit` appears to block for the whole job.
/// Marking our own standard handles non-inheritable before the spawn closes that
/// leak; the worker still gets its explicit log file as stdout and stderr.
#[cfg(windows)]
fn stop_inheriting_stdio() {
    use std::ffi::c_void;
    unsafe extern "system" {
        fn GetStdHandle(which: u32) -> *mut c_void;
        fn SetHandleInformation(handle: *mut c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    for which in [-10i32, -11, -12] {
        // SAFETY: plain Win32 calls on the process's own standard handles; an
        // invalid or null handle makes SetHandleInformation fail, which we ignore.
        unsafe {
            let handle = GetStdHandle(which as u32);
            if !handle.is_null() && handle as isize != -1 {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

fn pid_alive(pid: u32) -> bool {
    if pid == std::process::id() { return true; }
    if cfg!(windows) {
        let mut tasklist = Command::new("tasklist");
        tasklist.args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]);
        hide_window(&mut tasklist);
        tasklist
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
            .unwrap_or(false)
    } else {
        Command::new("kill").args(["-0", &pid.to_string()]).output().map(|o| o.status.success()).unwrap_or(false)
    }
}

async fn await_job_slot(jobs: &Path, spec: &JobSpec, dir: &Path) -> Result<File> {
    let dispatch = dir.join("dispatch.json");
    fs::write(&dispatch, json!({"phase":"queued", "pid":std::process::id()}).to_string())?;
    let lock_path = jobs.join("dispatch.lock");
    loop {
        if load_spec(jobs, &spec.id)?.0.cancelled { bail!("job cancelled while queued"); }
        let lock = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&lock_path)?;
        if lock.try_lock().is_ok() {
            let mut queued = Vec::new();
            let mut cap = spec.policy.max_active_jobs;
            for (other, path) in all_specs(jobs) {
                let state: Value = fs::read(path.join("dispatch.json")).ok()
                    .and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
                if other.cancelled || state["phase"] == "done" { continue; }
                let alive = state["pid"].as_u64().is_some_and(|pid| pid_alive(pid as u32));
                if !alive { continue; }
                cap = cap.min(other.policy.max_active_jobs.max(1));
                if state["phase"] == "queued" {
                    queued.push((other.policy.priority_at(other.started_at, now_ms()), other.started_at, other.id));
                }
            }
            queued.sort_by(|a,b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
            if queued.first().is_some_and(|q| q.2 == spec.id) {
                for i in 0..cap {
                    let slot = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
                        .open(jobs.join(format!("active-{i}.lock")))?;
                    if slot.try_lock().is_ok() {
                        fs::write(&dispatch, json!({"phase":"active", "pid":std::process::id()}).to_string())?;
                        return Ok(slot);
                    }
                }
            }
        }
        drop(lock);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn kill_tree(pid: u32) {
    if cfg!(windows) {
        let mut taskkill = Command::new("taskkill");
        taskkill.args(["/PID", &pid.to_string(), "/T", "/F"]);
        hide_window(&mut taskkill);
        let _ = taskkill.output();
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

async fn snapshot(spec: &mut JobSpec) -> Snapshot {
    let alive = spec.pid.is_some_and(pid_alive);
    // Re-read completion after checking the PID. The supervisor may have finished
    // between load_spec and this snapshot; a stale verifying phase must not turn
    // that successful exit into an interrupted job.
    let dir = spec.store.parent().unwrap_or(Path::new(".")).to_path_buf();
    overlay_verify(spec, &dir);
    let store = open_store(spec).await;
    let run = match &store {
        Some(store) => store.list_runs().await.ok().and_then(|runs| runs.into_iter().next()),
        None => None,
    };
    let events = match (&store, &run) {
        (Some(store), Some(run)) => store.latest_events(&run.id, 20_000).await.unwrap_or_default(),
        _ => Vec::new(),
    };
    // A verify job is running until its supervisor says done, whatever the latest
    // worker run says: the check and the next round have not happened yet.
    let verify_pending = spec.live.as_ref().is_some_and(|live| live.phase != "done");
    let queued = alive && fs::read(spec.store.parent().unwrap_or(Path::new(".")).join("dispatch.json"))
        .ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).is_some_and(|v| v["phase"] == "queued");
    let state = if spec.cancelled {
        "cancelled"
    } else if queued {
        "queued"
    } else if verify_pending {
        if alive { "running" } else { "interrupted" }
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
    let mut current = spec.clone();
    let snap = snapshot(&mut current).await;
    let spec = &current;
    let mut out = json!({
        "job_id": spec.id,
        "label": spec.label,
        "state": snap.state,
        "workspace": spec.workspace,
        "branch": spec.branch,
    });
    if let Some(verify) = &spec.verify {
        let live = spec.live.as_ref();
        out["verify"] = json!({
            "command": verify,
            "phase": live.map_or("starting", |live| live.phase.as_str()),
            "round": live.map_or(0, |live| live.round),
            "max_rounds": spec.max_rounds,
            "passed": live.and_then(|live| live.passed),
        });
    }
    let finished_at = snap.run.as_ref().filter(|_| is_terminal(&snap.state)).map(|run| run.updated_at);
    out["elapsed_s"] = json!(((finished_at.unwrap_or_else(now_ms) - spec.started_at).max(0)) / 1000);
    if let (Some(store), Some(run)) = (&snap.store, &snap.run) {
        out["run_id"] = json!(run.id.to_string());
        if let Ok(stats) = store.load_stats(&run.id).await {
            out["inference_calls"] = json!(stats.usage.inference_calls);
            out["tool_calls"] = json!(stats.usage.tool_calls);
            out["tokens"] = json!(stats.usage.total_tokens());
            out["total_tokens"] = json!(stats.usage.total_tokens());
            out["usage_estimated"] = json!(snap.events.iter().any(|e| e.kind == EventKind::InferenceCompleted && e.payload["usage_source"] == "estimated"));
        }
        // Next to the usage so "ran out of budget" is visible without reading logs.
        out["token_limit"] = json!(run.budget.max_total_tokens);
        out["last_action"] = last_action(&snap.events);
        out["reported"] = json!(reported_result(&snap.events).is_some());
    }
    if matches!(snap.state.as_str(), "failed" | "interrupted") {
        let log_tail = tail(&dir.join("run.log"), 12);
        // The run prints why it failed as a `reason:` line ("budget exceeded for
        // tokens: requested N against limit M", "transport error: ...").
        let reason = log_tail.lines().rev().find_map(|line| line.strip_prefix("reason: ")).map(str::to_string);
        out["reason"] = json!(reason);
        out["log_tail"] = json!(log_tail);
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
    let accepted: HashSet<_> = events
        .iter()
        .filter(|event| {
            event.kind == EventKind::ToolResult
                && event.payload.get("name").and_then(Value::as_str) == Some("report_result")
                && event.payload.get("success").and_then(Value::as_bool) == Some(true)
        })
        .filter_map(|event| event.payload.get("call_id").and_then(Value::as_str).map(|id| (event.agent_id.as_ref(), id)))
        .collect();
    events
        .iter()
        .rev()
        .find(|event| {
            event.kind == EventKind::ToolRequested
                && event.payload.get("name").and_then(Value::as_str) == Some("report_result")
                && event.payload.get("call_id").and_then(Value::as_str).is_some_and(|id| accepted.contains(&(event.agent_id.as_ref(), id)))
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
    // Byte-code caches and the Python tool's temp script are by-products of running
    // things, not part of what the worker changed.
    let skip = [":(exclude)**/__pycache__/**", ":(exclude)*.pyc", ":(exclude).swarm_tmp_run.py", ":(exclude).swarm/**"];
    let mut add = vec!["add", "-N", "--", "."];
    add.extend(skip);
    git(ws, &add).ok();
    let mut diff = vec!["diff", "--binary", base, "--", "."];
    diff.extend(skip);
    let patch = Command::new("git").arg("-C").arg(ws).args(&diff).output().ok()?;
    if !patch.status.success() {
        return None;
    }
    let path = dir.join("patch.diff");
    fs::write(&path, &patch.stdout).ok()?;
    let mut stat_args = vec!["diff", "--shortstat", base, "--", "."];
    stat_args.extend(skip);
    let stat = git(ws, &stat_args).unwrap_or_default();
    Some(json!({"path": path, "bytes": patch.stdout.len(), "stat": stat,
                "apply": format!("git apply \"{}\"", path.display())}))
}

pub async fn job_result(jobs: &Path, id: &str) -> Result<Value> {
    let (mut spec, dir) = load_spec(jobs, id)?;
    let snap = snapshot(&mut spec).await;
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
    let patch = write_patch(&spec, &dir);
    let changed = out["files_changed"].as_array().is_some_and(|files| !files.is_empty())
        || patch.as_ref().is_some_and(|p| p["bytes"].as_u64().unwrap_or(0) > 0);
    if let Some(patch) = patch {
        out["patch"] = patch;
    }
    // What the job actually amounts to. `state` only says the process ended: a
    // worker that gave up, or never touched a file, still ends as `completed`.
    out["outcome"] = if !is_terminal(&snap.state) {
        Value::Null
    } else if let Some(status) = reported.as_ref().and_then(|r| r.get("status")).and_then(Value::as_str) {
        json!(status)
    } else if snap.state != "completed" {
        // Failed, cancelled or interrupted without a report: say so, whatever it
        // had already written (that is still in files_changed and the patch).
        json!(snap.state)
    } else if changed {
        json!("changed_without_report")
    } else {
        json!("no_work")
    };
    // A verify job's outcome is what its check said, not what the worker claimed.
    if let (Some(verify), Some(live), true) = (&spec.verify, &spec.live, is_terminal(&snap.state)) {
        if live.phase == "done" {
            out["verify"]["rounds"] = Value::Array(live.rounds.clone());
            // Tools the worker really used (planning and reporting are not work). A check that passes on an
            // untouched tree proves nothing when the worker never looked at the tree: a chat model can answer
            // from its own connectors and never call a swarm tool.
            let worked = work_calls(&snap.events);
            out["verify"]["worker_tool_calls"] = json!(worked);
            out["outcome"] = match live.passed {
                Some(true) if changed => json!("verified"),
                Some(true) if worked == 0 => json!("unverified_no_work"),
                Some(true) => json!("verified_no_change"),
                _ => json!("verify_failed"),
            };
            if live.passed == Some(true) && !changed && worked == 0 {
                out["note"] = json!(
                    "the check passed, but the worker changed nothing and used no tools (it may have answered from the                      model's own connectors), so nothing was verified; read summary skeptically or resubmit"
                );
            }
            if live.passed != Some(true) {
                out["note"] = json!(format!(
                    "the check `{verify}` still failed after {} round(s); see verify.rounds for its output",
                    live.rounds.len()
                ));
            }
        }
    }
    if spec.policy.preset == TaskPreset::Reviewer || reported.as_ref().is_some_and(|r| r.get("verdict").is_some()) {
        out["review_evidence"] = audit_review(&spec.workspace, &snap.events, reported.as_ref(), spec.policy.min_tool_calls);
        if is_terminal(&snap.state) {
            out["outcome"] = if out["review_evidence"]["trusted"] == true && snap.state == "completed" { json!("review_verified") } else { json!("unverified_review") };
        }
    }
    out["baseline_gate"] = json!(spec.baseline_gate);
    out["base_freshness"] = freshness(&spec);
    out["base_drift"] = out["base_freshness"]["base_drift"].clone();
    if !is_terminal(&snap.state) {
        out["note"] = json!("the job has not finished; this is a partial view");
    } else if reported.is_none() && out["note"].is_null() {
        out["note"] = json!("the worker did not call report_result; read summary and files_changed instead");
    }
    Ok(out)
}

pub async fn cancel_job(jobs: &Path, id: &str) -> Result<Value> {
    let (mut spec, dir) = load_spec(jobs, id)?;
    let snap = snapshot(&mut spec).await;
    if is_terminal(&snap.state) && snap.state != "interrupted" {
        return Ok(json!({"job_id": id, "state": snap.state, "note": "already finished"}));
    }
    if let Some(pid) = spec.pid {
        kill_tree(pid);
    }
    if spec.policy.sandbox_image.is_some() {
        let output = Command::new("docker").args(["ps", "-q", "--filter", &format!("label=swarm.job={id}")]).output()?;
        for container in String::from_utf8_lossy(&output.stdout).split_whitespace() {
            if container.chars().all(|c| c.is_ascii_hexdigit()) {
                let _ = Command::new("docker").args(["rm", "-f", container]).output();
            }
        }
    }
    spec.cancelled = true;
    save_spec(&dir, &spec)?;
    Ok(json!({"job_id": id, "state": "cancelled"}))
}

/// Ceilings to raise when resuming a job that stopped on its budget. Usage is
/// cumulative, so a budget-exhausted run needs a ceiling above what it spent.
#[derive(Debug, Clone, Copy, Default)]
pub struct BudgetRaise {
    pub max_tokens: Option<u64>,
    pub max_steps: Option<u32>,
    pub max_inference_calls: Option<u64>,
    pub max_wall_clock_secs: Option<u64>,
}

impl BudgetRaise {
    fn is_empty(&self) -> bool {
        let Self { max_tokens, max_steps, max_inference_calls, max_wall_clock_secs } = *self;
        max_tokens.is_none() && max_steps.is_none() && max_inference_calls.is_none()
            && max_wall_clock_secs.is_none()
    }
}

/// True when a dead job's log says the run ended on a budget limit. The runner
/// prints the cause as a `reason:` line ("budget exceeded for tokens: requested N
/// against limit M") - the same text `job status` reports as `reason`.
fn budget_death_in(dir: &Path) -> bool {
    tail(&dir.join("run.log"), 40)
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("reason: "))
        .is_some_and(|reason| reason.contains("budget exceeded"))
}

/// Continue a job whose process died before its run finished, or one that stopped
/// on its budget (with a raised ceiling): same store, same workspace, same provider,
/// a new process.
pub async fn resume_job(jobs: &Path, id: &str, raise: BudgetRaise) -> Result<Value> {
    let (mut spec, dir) = load_spec(jobs, id)?;
    if spec.verify.is_some() {
        bail!("job {id} has a --verify loop, which cannot be resumed; submit a new job with --workspace {} to carry on from its files", spec.workspace.display());
    }
    let snap = snapshot(&mut spec).await;
    // A budget-exhausted run is a pause, not a failure: its transcript is in the
    // store and it continues from its last checkpoint. Because usage is cumulative,
    // resuming it without a raised ceiling would die on the very next call.
    let budget_death = snap.state == "failed" && budget_death_in(&dir);
    if snap.state != "interrupted" && !budget_death {
        bail!(
            "job {id} is {}; only an interrupted job, or one that died on its budget, can be resumed",
            snap.state
        );
    }
    if budget_death && raise.is_empty() {
        bail!(
            "job {id} died on its budget; resume it with a raised ceiling, e.g. --max-tokens <above the tokens shown by `job status`>"
        );
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
    command.args(["--tool-protocol", spec.tool_protocol.as_str()]);
    if let Some(config) = &spec.config { command.arg("--config").arg(config); }
    if let Some(image) = &spec.policy.sandbox_image { command.args(["--sandbox-image", image]); }
    if let Some(url) = &spec.base_url {
        command.args(["--base-url", url]);
    }
    if let Some(model) = &spec.model {
        command.args(["--model", model]);
    }
    for granted in &spec.allow_push {
        command.args(["--allow-push", granted]);
    }
    // Ceilings travel to `swarm resume`, which persists them on the run row: the
    // agent enforces the stored budget against cumulative usage.
    if let Some(tokens) = raise.max_tokens {
        command.args(["--max-tokens", &tokens.to_string()]);
    }
    if let Some(steps) = raise.max_steps {
        command.args(["--max-steps", &steps.to_string()]);
    }
    if let Some(calls) = raise.max_inference_calls {
        command.args(["--max-inference-calls", &calls.to_string()]);
    }
    if let Some(secs) = raise.max_wall_clock_secs {
        command.args(["--max-wall-clock-secs", &secs.to_string()]);
    }
    let worker_args: Vec<String> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
    fs::write(dir.join("resume-args.json"), serde_json::to_vec(&worker_args)?)?;
    let mut command = Command::new(job_exe()?);
    command.arg("job").arg("--jobs").arg(jobs).arg("supervise").arg(id);
    command.env("SWARM_JOB_ID", id);
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

    #[test]
    fn a_budget_death_is_told_apart_from_a_real_failure() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("run.log");
        std::fs::write(
            &log,
            "step 40 done\nrun 7 failed\nreason: budget: budget exceeded for tokens: requested 4500000 against limit 4500000\n",
        )
        .unwrap();
        assert!(budget_death_in(dir.path()), "a ceiling the worker ran out of is resumable");
        std::fs::write(&log, "step 4 done\nreason: transport error: connection refused\n").unwrap();
        assert!(!budget_death_in(dir.path()), "a real failure stays a failure");
        std::fs::write(&log, "step 4 done\n").unwrap();
        assert!(!budget_death_in(dir.path()), "no reason line is not a budget death");
    }

    #[test]
    fn an_empty_raise_is_detected() {
        assert!(BudgetRaise::default().is_empty());
        assert!(!BudgetRaise { max_tokens: Some(1), ..Default::default() }.is_empty());
    }

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
    fn job_protocol_is_backward_compatible_and_forwarded_to_worker() {
        assert!(jobs_dir(Some(Path::new("relative-jobs"))).is_absolute());
        let legacy = json!({
            "id":"job_test", "key":null, "label":null, "goal":"g", "workspace":"work",
            "store":"store", "repo":null, "base":null, "base_commit":null,
            "branch":null, "pid":null, "started_at":0
        });
        let mut spec: JobSpec = serde_json::from_value(legacy).unwrap();
        assert_eq!(spec.tool_protocol, ToolProtocolSelection::Auto);
        spec.tool_protocol = ToolProtocolSelection::Text;
        let saved = serde_json::to_string(&spec).unwrap();
        let loaded: JobSpec = serde_json::from_str(&saved).unwrap();
        let command = worker_command(&loaded, "g", Path::new("store")).unwrap();
        let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.windows(2).any(|pair| pair == ["--tool-protocol", "text"]));
    }

    #[test]
    fn the_retry_goal_keeps_the_original_task_and_says_why_it_failed() {
        let goal = retry_goal("fix the parser", "go test ./...", 2, Some(1), "FAIL TestParse");
        assert!(goal.starts_with("fix the parser"), "{goal}");
        assert!(goal.contains("attempt 2") && goal.contains("go test ./...") && goal.contains("FAIL TestParse"));
        assert!(goal.contains("Exit code: 1") && goal.contains("Do not weaken"), "{goal}");
        let killed = retry_goal("g", "c", 1, None, "");
        assert!(killed.contains("killed or timed out"), "{killed}");
    }

    #[test]
    fn verify_output_keeps_its_end_on_a_character_boundary() {
        assert_eq!(tail_chars("short", 100), "short");
        let text = format!("{}{}", "a".repeat(50), "\u{e9}".repeat(10));
        let cut = tail_chars(&text, 7);
        assert!(cut.starts_with('\u{2026}') && cut.ends_with('\u{e9}'), "{cut}");
        assert!(cut.chars().count() <= 8);
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
        let ok_provider = SubmitArgs { policy: JobPolicy { preset: TaskPreset::General, ..Default::default() }, goal: Some("g".into()), provider: Some("mock".into()), ..Default::default() };
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
    #[tokio::test]
    async fn queued_review_gets_capacity_before_builder() {
        let dir = tempfile::tempdir().unwrap();
        let make = |id: &str, preset: &str| -> JobSpec {
            serde_json::from_value(json!({"id":id,"key":null,"label":null,"goal":"g","workspace":dir.path(),
                "store":dir.path().join(id).join("store"),"repo":null,"base":null,"base_commit":null,
                "branch":null,"pid":std::process::id(),"started_at":now_ms(),
                "policy":{"preset":preset,"max_active_jobs":1}})).unwrap()
        };
        let low = make("low","builder"); let high = make("high","reviewer");
        for spec in [&low,&high] {
            let path = dir.path().join(&spec.id); fs::create_dir_all(&path).unwrap(); save_spec(&path,spec).unwrap();
            fs::write(path.join("dispatch.json"),json!({"phase":"queued","pid":std::process::id()}).to_string()).unwrap();
        }
        let low_dir = dir.path().join("low"); let high_dir = dir.path().join("high");
        assert!(tokio::time::timeout(Duration::from_millis(200),await_job_slot(dir.path(),&low,&low_dir)).await.is_err());
        let slot = tokio::time::timeout(Duration::from_secs(5),await_job_slot(dir.path(),&high,&high_dir)).await.unwrap().unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(200),await_job_slot(dir.path(),&low,&low_dir)).await.is_err());
        drop(slot); fs::write(high_dir.join("dispatch.json"),json!({"phase":"done"}).to_string()).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(5),await_job_slot(dir.path(),&low,&low_dir)).await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn completed_supervisor_wins_over_a_stale_verifying_snapshot() {
        let dir = tempfile::tempdir().unwrap(); let store_path = dir.path().join("store");
        let store = Store::open(&store_path).await.unwrap();
        let mut run = swarm_core::run::Run::new("g","mock",None,1,true,Default::default(),now_ms());
        run.status = RunStatus::Completed; store.create_run(&run).await.unwrap();
        let mut spec: JobSpec = serde_json::from_value(json!({"id":"stale","key":null,"label":null,
            "goal":"g","workspace":dir.path(),"store":store_path,"repo":null,"base":null,
            "base_commit":null,"branch":null,"pid":null,"started_at":now_ms(),"verify":"true"})).unwrap();
        spec.live = Some(VerifyState { phase:"verifying".into(), store:store_path.clone(), ..Default::default() });
        write_verify(dir.path(), &VerifyState { phase:"done".into(),store:store_path,passed:Some(true),..Default::default() }).unwrap();
        let status = status_of(&spec,dir.path()).await;
        assert_eq!(status["state"],"completed","{status}");
        assert_eq!(status["verify"]["phase"],"done");
        assert_eq!(status["verify"]["passed"],true);
    }

}
