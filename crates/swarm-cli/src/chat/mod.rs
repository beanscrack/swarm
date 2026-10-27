//! The interactive session: the Claude Code-style REPL around the run engine.
//!
//! Plain `swarm` opens it; `swarm chat <goal>` seeds a first turn and
//! `--continue` / `--resume <run>` carry a previous run's context forward.
//! Every goal is a normal durable run, so the store, checkpoints, budgets,
//! approvals and `swarm logs --transcript` all keep working; this module adds
//! the session around them: the conversation thread, slash commands, queueing,
//! Esc to interrupt, and `/goal`.
//!
//! The terminal belongs to the [`ui`](crate::ui) task. This code never prints
//! to it directly: it sends rows and state through a [`UiHandle`] and reacts to
//! the [`Action`]s the user's keys produce.

mod aside;
mod goal;
mod hooks_ui;
mod mentions;
mod repeat;
mod rewind;
mod session;
mod slash;
mod todos;
mod view;

use std::collections::VecDeque;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::json;
use swarm_core::config::RunConfig;
use swarm_core::event::{EventKind, RunEvent};
use swarm_core::run::RunMode;
use swarm_core::{RunId, message::ChatMessage};
use swarm_providers::{SendRequest, SessionConfig};
use swarm_runtime::build_registry;
use swarm_store::Store;

use crate::approval::{AllowList, SharedMode, mode_of, policy_of};
use crate::commands::{self, ChatArgs, TurnEnv, TurnOutcome};
use crate::custom;
use crate::hooks::Hooks;
use crate::style::{self, Color};
use crate::transcript::{self, Transcript};
use crate::ui::{Accent, Action, DialogSpec, Info, Mode, StartOptions, Status, Ui, UiHandle, echo_rows};
use goal::{Goal, Step, Verdict};
pub use session::{Session, Turn};
pub(crate) use session::clip;

const KV_NAMESPACE: &str = "chat";
const KV_ACTIVE: &str = "active";
const KV_GOAL: &str = "goal";

const PLAN_PREFIX: &str = "PLAN MODE. Do not change anything: no writes, no edits, no commands that \
modify state. You may read, list and search. Investigate the task, then reply with a concrete \
step-by-step plan (which files change and how, which commands run, what could go wrong). Do not \
carry the plan out.\n\nTask: ";

const INIT_PROMPT: &str = "Look through this project and create (or update) AGENTS.md in the \
workspace root with the instructions a new contributor or coding agent needs: what the project is, \
how to build, test and run it, the layout, and the conventions the code follows. Read the code \
before writing; keep it short and specific, and do not invent commands you have not seen.";

const REVIEW_PROMPT: &str = "Review code like a careful senior engineer, and change nothing: do not \
write, edit or delete any file.";

const REVIEW_SCOPE: &str = "Run `git status` and `git diff` (and `git diff --staged`) to see what has \
changed. If there are no uncommitted changes, review the latest commit with `git show`.";

const REVIEW_REPORT: &str = "Read the code around each change before judging it. Report concrete \
problems only, each with the file and line, why it is a problem, and a suggested fix, most severe first. \
If you find nothing wrong, say so plainly instead of inventing nits.";

const SECURITY_FOCUS: &str = "Look for security problems: injection (shell, SQL, template), path \
traversal, missing authentication or authorization checks, secrets committed or logged, unsafe \
deserialization, server-side request forgery, unsafe use of user input, weak cryptography, and \
dependencies added without need. Rate each finding by how exploitable it is.";

/// Whether the session continues after handling something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    Continue,
    Quit,
}

/// One request to the engine: what to remember it as, and what to send.
pub(crate) struct Request {
    pub display: String,
    pub prompt: String,
}

impl Request {
    fn user(text: &str) -> Self {
        Self { display: text.to_string(), prompt: text.to_string() }
    }
}

enum TurnEnd {
    Done(TurnOutcome),
    Interrupted(Option<String>),
    Failed(anyhow::Error),
    Quit,
}

pub(crate) struct Chat {
    pub(crate) store: Arc<Store>,
    /// Where the store lives on disk; custom commands are looked up beside it.
    pub(crate) store_dir: PathBuf,
    pub(crate) cfg: RunConfig,
    pub(crate) workspace: PathBuf,
    pub(crate) session: Session,
    pub(crate) goal: Option<Goal>,
    pub(crate) verbose: bool,
    /// `/focus`: tool activity that went well is not printed.
    pub(crate) focus: bool,
    pub(crate) mode: Mode,
    pub(crate) shared_mode: Arc<SharedMode>,
    pub(crate) allow: Arc<AllowList>,
    pub(crate) last_run: Option<RunId>,
    pub(crate) last_result: String,
    pub(crate) ui: UiHandle,
    pub(crate) colors: Color,
    pub(crate) context_chars: usize,
    pub(crate) show_transcript: bool,
    pub(crate) started: Instant,
    /// The id of a run that was stopped, for the exit message.
    interrupted: Option<String>,
    pub(crate) turns_run: u32,
    /// `/loop`: a prompt to run again while idle.
    pub(crate) loop_job: Option<repeat::Loop>,
    pub(crate) hooks: Arc<Hooks>,
    /// Hook files that were skipped or had mistakes in them, for `/hooks`.
    pub(crate) hook_notes: Vec<String>,
}

impl Chat {
    pub(crate) fn info(&self) -> Info {
        Info {
            provider: self.cfg.provider.to_string(),
            model: self.cfg.model.clone(),
            agents: self.cfg.agents,
            mode: self.mode,
            tokens: self.session.totals.tokens,
            goal: [self.goal.as_ref().and_then(Goal::chip), self.loop_chip()]
                .into_iter()
                .flatten()
                .reduce(|a, b| format!("{a}  {b}")),
        }
    }

    pub(crate) fn push_info(&self) {
        self.ui.info(self.info());
    }

    pub(crate) fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.shared_mode.set(mode);
        self.push_info();
    }

    pub(crate) fn cycle_mode(&mut self) {
        self.set_mode(self.mode.next());
    }

    pub(crate) fn toggle_verbose(&mut self) {
        self.verbose = !self.verbose;
        self.push_info();
        self.say(format!("transcript detail: {}", if self.verbose { "on" } else { "off" }));
    }

    /// A quiet one-line notice, indented like a result.
    pub(crate) fn say(&self, text: impl AsRef<str>) {
        self.say_lines(text.as_ref().lines().map(str::to_string).collect());
    }

    pub(crate) fn say_lines(&self, lines: Vec<String>) {
        self.ui.print(
            lines
                .into_iter()
                .map(|line| style::role(self.colors, "subtle", &format!("  {line}")))
                .collect(),
        );
    }

    pub(crate) fn fail(&self, error: &anyhow::Error) {
        self.ui.print(vec![style::role(self.colors, "fail", &format!("\u{2717} {error:#}"))]);
    }

    pub(crate) fn welcome(&self) {
        let tip = style::TIPS[(self.started.elapsed().subsec_nanos() as usize
            ^ std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.subsec_nanos() as usize)
                .unwrap_or(0))
            % style::TIPS.len()];
        let mode_label = self.cfg.tools.approval.to_string();
        let workspace = self.workspace.display().to_string();
        self.ui.print(view::welcome(
            self.colors,
            style::term_width(),
            &view::Welcome {
                workspace: &workspace,
                provider: self.cfg.provider.as_str(),
                model: &self.cfg.model,
                agents: self.cfg.agents,
                approvals: &mode_label,
                session_id: &self.session.id,
                resumed_turns: self.session.turns.len(),
            },
            tip,
        ));
    }

    pub(crate) async fn save(&self) {
        let _ = self.store.kv_set(KV_NAMESPACE, KV_ACTIVE, &self.session.to_json()).await;
        let goal = self.goal.as_ref().and_then(|goal| serde_json::to_string(goal).ok()).unwrap_or_default();
        let _ = self.store.kv_set(KV_NAMESPACE, KV_GOAL, &goal).await;
    }

    /// Handle one line the user submitted.
    pub(crate) async fn submit(&mut self, ui: &mut Ui, text: String) -> Flow {
        match self.dispatch(ui, &text).await {
            Ok(flow) => flow,
            Err(error) => {
                self.fail(&error);
                Flow::Continue
            }
        }
    }

    async fn dispatch(&mut self, ui: &mut Ui, text: &str) -> Result<Flow> {
        let trimmed = text.trim();
        // `#` saves a note to AGENTS.md (but `#!` is a shebang, not a note).
        if trimmed.starts_with('#') && !trimmed.starts_with("#!") {
            let chars = remember_note(&self.workspace, trimmed)?;
            self.say(format!("saved to AGENTS.md (+{chars} chars)"));
            return Ok(Flow::Continue);
        }
        if let Some(command) = trimmed.strip_prefix('!') {
            return self.shell(ui, command.trim()).await;
        }
        if let Some((word, args)) = crate::ui::registry::split(trimmed) {
            return self.slash(ui, word, args).await;
        }
        let expansion = mentions::expand(&self.workspace, trimmed);
        if !expansion.notes.is_empty() {
            self.say_lines(expansion.notes.clone());
        }
        self.work(ui, Request { display: trimmed.to_string(), prompt: expansion.text }).await
    }

    /// `!cmd`: run a shell command in the workspace yourself, not through the
    /// agent. Its output joins the conversation so the next goal knows about it.
    async fn shell(&mut self, ui: &mut Ui, command: &str) -> Result<Flow> {
        if command.is_empty() {
            self.say("usage: !<command>  runs it in the workspace and keeps the output as context");
            return Ok(Flow::Continue);
        }
        let mut transcript = Transcript::new(self.colors, self.verbose);
        transcript.set_width(style::term_width());
        let run = RunId::new();
        let request = RunEvent::new(
            EventKind::ToolRequested,
            run.clone(),
            json!({"name": "bash", "arguments": {"command": command}}),
        );
        self.ui.print(transcript.lines(&request));
        self.ui.busy(true);
        self.ui.status(Status { activity: "running command".into(), ..Status::default() });
        let command_owned = command.to_string();
        let workspace = self.workspace.clone();
        let running = Box::pin(run_shell(workspace, command_owned));
        tokio::pin!(running);
        let result = loop {
            tokio::select! {
                result = &mut running => break Some(result),
                action = ui.recv() => match action {
                    Some(Action::Interrupt) => break None,
                    Some(Action::Quit) | None => {
                        self.ui.busy(false);
                        return Ok(Flow::Quit);
                    }
                    Some(Action::Side(question)) => self.cmd_btw(&question).await,
                    _ => {}
                },
            }
        };
        let Some((output, code)) = result else {
            self.ui.busy(false);
            self.say("interrupted");
            return Ok(Flow::Continue);
        };
        let mut text = output.clone();
        if let Some(code) = code.filter(|code| *code != 0) {
            text.push_str(&format!("\n\n[exit code {code}]"));
        }
        let result = RunEvent::new(
            EventKind::ToolResult,
            run,
            json!({"name": "bash", "success": true, "truncated": false,
                   "arguments": {"command": command}, "output": text}),
        );
        self.ui.print(transcript.lines(&result));
        self.session.push(Turn {
            goal: format!("!{command}"),
            status: if code == Some(0) { "completed" } else { "failed" }.to_string(),
            summary: clip(&output, 300),
            run_id: String::new(),
            todos: None,
        });
        self.ui.busy(false);
        self.save().await;
        Ok(Flow::Continue)
    }

    /// Run a request, and then keep going while an active goal says it is not
    /// done. The UI stays "busy" throughout, so anything typed meanwhile queues.
    pub(crate) async fn work(&mut self, ui: &mut Ui, first: Request) -> Result<Flow> {
        let first = match self.prompt_hook(first).await {
            Ok(request) => request,
            Err(reason) => {
                self.ui.print(vec![style::role(self.colors, "fail", &format!("\u{2717} blocked by a hook: {reason}"))]);
                return Ok(Flow::Continue);
            }
        };
        self.ui.busy(true);
        let mut request = first;
        let mut flow = Flow::Continue;
        loop {
            match self.turn(ui, &request).await {
                TurnEnd::Done(outcome) => {
                    self.finish_turn(&request, &outcome).await;
                    if !outcome.succeeded() {
                        if let Some(goal) = self.goal.as_mut().filter(|goal| goal.is_active()) {
                            goal.pause("the turn failed");
                            self.say("\u{23f8} Goal paused \u{2014} the turn failed. /goal resume to try again, /goal clear to stop.");
                        }
                        break;
                    }
                    if self.mode == Mode::Plan && !outcome.summary.trim().is_empty() {
                        if let Some(next) = self.offer_to_proceed(ui).await {
                            request = next;
                            continue;
                        }
                        break;
                    }
                    match self.goal_step(ui, &outcome).await {
                        Some(next) => request = next,
                        None => break,
                    }
                }
                TurnEnd::Interrupted(id) => {
                    let hint = match &id {
                        Some(id) => format!("Interrupted \u{00b7} /resume {id}"),
                        None => "Interrupted".to_string(),
                    };
                    // The stopped run may already have edited files, and the next
                    // request should know it was cut short: keep it as a turn.
                    if let Some(run_id) = &id {
                        let todos = self.todos_of(run_id).await;
                        self.session.push(Turn {
                            goal: request.display.clone(),
                            status: "interrupted".to_string(),
                            summary: "stopped by the user before it finished".to_string(),
                            run_id: run_id.clone(),
                            todos,
                        });
                    }
                    self.interrupted = id;
                    self.ui.print(vec![style::role(self.colors, "warn", &format!("  \u{23bf}  {hint}"))]);
                    if let Some(goal) = self.goal.as_mut().filter(|goal| goal.is_active()) {
                        goal.pause("interrupted");
                        self.say("\u{23f8} Goal paused \u{2014} the turn was interrupted. /goal resume to continue, /goal clear to stop.");
                    }
                    break;
                }
                TurnEnd::Failed(error) => {
                    self.fail(&error);
                    if let Some(goal) = self.goal.as_mut().filter(|goal| goal.is_active()) {
                        goal.pause("the turn errored");
                        self.say("\u{23f8} Goal paused \u{2014} the turn errored. /goal resume to try again, /goal clear to stop.");
                    }
                    break;
                }
                TurnEnd::Quit => {
                    flow = Flow::Quit;
                    break;
                }
            }
        }
        if flow != Flow::Quit {
            self.stop_hook().await;
        }
        self.push_info();
        self.ui.busy(false);
        self.save().await;
        Ok(flow)
    }

    /// Drive one run to its end, reacting to keys meanwhile. Dropping the drive
    /// future is how Esc stops a turn: the run itself is durable.
    async fn turn(&mut self, ui: &mut Ui, request: &Request) -> TurnEnd {
        let prompt = if self.mode == Mode::Plan {
            format!("{PLAN_PREFIX}{}", request.prompt)
        } else {
            request.prompt.clone()
        };
        let context = self.session.render(self.context_chars);
        let objective = if context.is_empty() { prompt } else { format!("{context}{prompt}") };
        let mut cfg = self.cfg.clone();
        cfg.tools.approval = policy_of(self.mode);
        let env = TurnEnv {
            ui: self.ui.clone(),
            allow: Arc::clone(&self.allow),
            mode: Arc::clone(&self.shared_mode),
            verbose: self.verbose,
            focus: self.focus,
            hooks: Some(Arc::clone(&self.hooks)).filter(|hooks| !hooks.is_empty()),
            banner: false,
        };
        let current: Arc<Mutex<Option<RunId>>> = Arc::new(Mutex::new(None));
        // Everything the future borrows is a local, so `self` stays free for
        // handling keys while it runs.
        let store = Arc::clone(&self.store);
        let workspace = self.workspace.clone();
        let live = self.show_transcript;
        let drive = Box::pin(commands::drive_turn(
            &store,
            &cfg,
            &workspace,
            &objective,
            RunMode::Coding,
            None,
            live,
            &env,
            Arc::clone(&current),
        ));
        tokio::pin!(drive);
        loop {
            tokio::select! {
                result = &mut drive => {
                    return match result {
                        Ok(outcome) => TurnEnd::Done(outcome),
                        Err(error) => TurnEnd::Failed(error),
                    };
                }
                action = ui.recv() => match action {
                    Some(Action::Interrupt) => {
                        let id = current.lock().ok().and_then(|slot| slot.as_ref().map(|id| id.to_string()));
                        return TurnEnd::Interrupted(id);
                    }
                    Some(Action::Quit) | None => return TurnEnd::Quit,
                    Some(Action::CycleMode) => self.cycle_mode(),
                    Some(Action::ToggleVerbose) => self.toggle_verbose(),
                    Some(Action::Side(question)) => self.cmd_btw(&question).await,
                    // The UI queues what is typed during a turn, and Esc stops it
                    // rather than rewinding; nothing else arrives.
                    Some(Action::Submit(_)) | Some(Action::Rewind) => {}
                },
            }
        }
    }

    async fn finish_turn(&mut self, request: &Request, outcome: &TurnOutcome) {
        self.last_run = Some(outcome.run_id.clone());
        let todos = self.todos_of(&outcome.run_id.to_string()).await;
        self.session.push(Turn {
            goal: request.display.clone(),
            status: outcome.status.to_string(),
            summary: outcome.summary.clone(),
            run_id: outcome.run_id.to_string(),
            todos,
        });
        self.session.totals.add(outcome);
        self.last_result = outcome.summary.clone();
        self.turns_run += 1;
        self.ui.print(view::turn_footer(self.colors, outcome));
        self.push_info();
    }

    /// After a plan-mode turn: ask whether to act on the plan.
    async fn offer_to_proceed(&mut self, _ui: &mut Ui) -> Option<Request> {
        if !self.ui.is_live() {
            return None;
        }
        let mut spec = DialogSpec::pick(
            "Ready to proceed with this plan?",
            vec![
                "Yes, and ask before risky tools".to_string(),
                "Yes, and approve every tool automatically".to_string(),
                "No, keep planning".to_string(),
            ],
        );
        spec.accent = Accent::Permission;
        match self.ui.choose(spec).await {
            Some(0) => self.set_mode(Mode::Default),
            Some(1) => self.set_mode(Mode::Auto),
            _ => return None,
        }
        Some(Request {
            display: "proceed with the plan".to_string(),
            prompt: "Proceed with the plan you just wrote. Carry it out now.".to_string(),
        })
    }

    /// With an active goal: ask the judge, then continue, pause or finish.
    async fn goal_step(&mut self, ui: &mut Ui, outcome: &TurnOutcome) -> Option<Request> {
        let goal = self.goal.as_ref().filter(|goal| goal.is_active())?.clone();
        // A message you typed while the turn ran comes first; the goal picks
        // up again after it.
        if self.ui.queued() > 0 {
            return None;
        }
        self.ui.status(Status { activity: "checking the goal".into(), ..Status::default() });
        let work = outcome.stats.segments().join(", ");
        let reply = outcome.summary.clone();
        let cfg = self.cfg.clone();
        let judging = Box::pin(judge(cfg, goal.clone(), reply, work));
        tokio::pin!(judging);
        let verdict = loop {
            tokio::select! {
                verdict = &mut judging => break verdict,
                action = ui.recv() => match action {
                    Some(Action::Interrupt) | Some(Action::Quit) | None => {
                        if let Some(goal) = self.goal.as_mut() {
                            goal.pause("interrupted");
                        }
                        self.say("\u{23f8} Goal paused \u{2014} interrupted while checking. /goal resume to continue.");
                        return None;
                    }
                    Some(Action::Side(question)) => self.cmd_btw(&question).await,
                    _ => {}
                },
            }
        };
        let goal = self.goal.as_mut()?;
        let step = goal.advance(verdict);
        let (used, max) = (goal.turns_used, goal.max_turns);
        match step {
            Step::Finished(reason) => {
                let why = if reason.is_empty() { String::new() } else { format!(": {reason}") };
                self.ui.print(vec![style::role(self.colors, "ok", &format!("\u{2713} Goal achieved{why}"))]);
                None
            }
            Step::Paused(reason) => {
                self.ui.print(vec![style::role(
                    self.colors,
                    "warn",
                    &format!("\u{23f8} Goal paused \u{2014} {reason}. /goal resume to keep going, /goal clear to stop."),
                )]);
                None
            }
            Step::Continue { prompt, reason } => {
                self.ui.print(vec![style::role(
                    self.colors,
                    "subtle",
                    &format!("\u{21bb} Continuing toward the goal ({used}/{max}): {}", clip(&reason, 100)),
                )]);
                Some(Request { display: format!("{}{}", session::INTERNAL_PREFIX, clip(&reason, 60)), prompt })
            }
        }
    }

    /// What to print after the terminal is handed back.
    fn farewell(&self) -> Vec<String> {
        let dim = |text: &str| style::role(self.colors, "subtle", text);
        let mut rows = Vec::new();
        let turns = self.session.turns.len();
        if turns > 0 {
            rows.push(dim(&format!(
                "session {} \u{00b7} {turns} turn{} \u{00b7} {} tokens \u{00b7} {}",
                clip(&self.session.id, 24),
                if turns == 1 { "" } else { "s" },
                style::tokens(self.session.totals.tokens),
                style::elapsed(self.started.elapsed().as_secs_f64()),
            )));
            rows.push(dim("run `swarm` again to continue it, or `swarm chat --new` for a fresh one"));
        }
        if let Some(id) = &self.interrupted {
            rows.push(dim(&format!("the stopped run can be finished with `swarm resume {id}`")));
        }
        rows
    }
}

/// The `#` shortcut: a line starting with `#` is a note to keep, not a goal. It
/// lands in the workspace's `AGENTS.md`, which the runtime puts into every agent
/// prompt, so the next turn already knows it.
pub(crate) fn remember_note(workspace: &std::path::Path, text: &str) -> Result<usize> {
    let note = text.trim().trim_start_matches('#').trim();
    anyhow::ensure!(!note.is_empty(), "nothing to remember");
    let path = workspace.join("AGENTS.md");
    let mut body = std::fs::read_to_string(&path).unwrap_or_default();
    if body.trim().is_empty() {
        body = String::from("# Project instructions\n");
    }
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&format!("- {note}\n"));
    std::fs::write(&path, &body)?;
    Ok(note.chars().count())
}

/// `!cmd`: stdout then stderr, with a time limit; the child never reads the
/// keyboard and does not outlive a cancelled call.
async fn run_shell(workspace: PathBuf, command: String) -> (String, Option<i32>) {
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(&command)
        .current_dir(&workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => return (format!("could not start the command: {error}"), None),
    };
    match tokio::time::timeout(Duration::from_secs(120), child.wait_with_output()).await {
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.trim().is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&stderr);
            }
            if text.len() > 256 * 1024 {
                text.truncate(text.floor_char_boundary(256 * 1024));
            }
            (text, out.status.code())
        }
        Ok(Err(error)) => (format!("the command failed: {error}"), None),
        Err(_) => ("timed out after 120s".to_string(), None),
    }
}

/// One model call that reads the goal and the latest turn and answers with a
/// verdict. `None` when the model could not be reached or did not answer in the
/// requested form (the goal then fails open).
async fn judge(cfg: RunConfig, goal: Goal, reply: String, work: String) -> Option<Verdict> {
    let registry = build_registry(&cfg).ok()?;
    let provider = registry.find(&cfg.provider)?;
    let session = provider.start_session(SessionConfig::default()).await.ok()?;
    let (system, user) = goal::judge_messages(&goal, &reply, &work);
    let mut request = SendRequest::new(vec![ChatMessage::system(system), ChatMessage::user(user)]);
    request.zero_cost = cfg.zero_cost;
    request.deadline = Some(Duration::from_secs(90));
    let response = session.send(request).await;
    let _ = session.close().await;
    goal::parse_verdict(&response.ok()?.text)
}

async fn load_session(store: &Store, args: &ChatArgs) -> Result<Session> {
    let carried = |run: swarm_core::run::Run, summary: String| {
        let mut session = Session::with_id();
        session.push(Turn {
            goal: run.objective.clone(),
            status: run.status.to_string(),
            summary,
            run_id: run.id.to_string(),
            todos: None,
        });
        session
    };
    if let Some(id) = &args.resume {
        let run = store.load_run_required(&RunId::parse(id)?).await?;
        let summary = commands::run_summary(store, &run.id).await?;
        return Ok(carried(run, summary));
    }
    if args.continue_last
        && let Some(run) = store.list_runs().await?.into_iter().next()
    {
        let summary = commands::run_summary(store, &run.id).await?;
        return Ok(carried(run, summary));
    }
    if !args.new
        && let Some(json) = store.kv_get(KV_NAMESPACE, KV_ACTIVE).await?
        && let Some(session) = Session::from_json(&json)
    {
        return Ok(session);
    }
    Ok(Session::with_id())
}

async fn load_goal(store: &Store, args: &ChatArgs) -> Option<Goal> {
    if args.new || args.resume.is_some() || args.continue_last {
        return None;
    }
    let json = store.kv_get(KV_NAMESPACE, KV_GOAL).await.ok().flatten()?;
    serde_json::from_str::<Goal>(&json).ok().filter(|goal| goal.status != goal::GoalStatus::Done)
}

/// Enter the REPL.
pub async fn chat(args: ChatArgs) -> Result<commands::CommandOutcome> {
    let tty = io::stdout().is_terminal() && io::stdin().is_terminal();
    let store = Arc::new(Store::open(&commands::store_path(&args)).await?);
    let cfg = commands::chat_config(&args)?;
    let workspace = commands::workspace_for(&cfg, &args.workspace)?;
    std::fs::create_dir_all(&workspace)?;
    let colors = Color::detect(tty && transcript::ui_enabled(tty));
    let session = load_session(&store, &args).await?;
    let goal = load_goal(&store, &args).await;

    let mut ui = if tty && !args.oneshot {
        Ui::start(StartOptions {
            colors,
            history_path: Some(commands::store_path(&args).join("history")),
            workspace: workspace.clone(),
            input: true,
        })
        .unwrap_or_else(|_| Ui::plain(colors))
    } else {
        Ui::plain(colors)
    };
    let mode = mode_of(cfg.tools.approval);
    let store_dir = commands::store_path(&args);
    custom::install(custom::scan(&custom::dirs(&store_dir, &workspace)).commands);
    let (hooks, hook_notes) = hooks_ui::load(&ui.handle(), &store_dir, &workspace, &session.id).await;
    let mut chat = Chat {
        store,
        store_dir,
        cfg,
        workspace,
        session,
        goal,
        verbose: args.verbose,
        focus: false,
        mode,
        shared_mode: SharedMode::new(mode),
        allow: commands::allow_list(),
        last_run: None,
        last_result: String::new(),
        ui: ui.handle(),
        colors,
        context_chars: args.context_chars,
        show_transcript: !args.plain,
        started: Instant::now(),
        interrupted: None,
        turns_run: 0,
        loop_job: None,
        hooks,
        hook_notes,
    };
    chat.last_run = chat.session.turns.last().and_then(|turn| RunId::parse(&turn.run_id).ok());
    chat.welcome();
    if !chat.hook_notes.is_empty() {
        chat.say_lines(chat.hook_notes.iter().map(|note| format!("hooks: {note}")).collect());
    }
    chat.push_info();

    let mut pending: VecDeque<String> = VecDeque::new();
    if let Some(goal) = args.goal.clone() {
        pending.push_back(goal);
    }
    let mut quit = false;
    while let Some(text) = pending.pop_front() {
        chat.ui.print(echo_rows(colors, style::term_width(), &text));
        if chat.submit(&mut ui, text).await == Flow::Quit {
            quit = true;
        }
    }
    if args.oneshot {
        // Take exactly one goal from stdin when none was given.
        if args.goal.is_none() && !quit {
            while let Some(action) = ui.recv().await {
                match action {
                    Action::Submit(text) => {
                        chat.submit(&mut ui, text).await;
                        break;
                    }
                    Action::Quit => break,
                    _ => {}
                }
            }
        }
        quit = true;
    }
    while !quit {
        let due = chat.loop_job.as_ref().map(|job| job.next);
        let action = tokio::select! {
            // What you type always goes before the timer.
            biased;
            action = ui.recv() => match action {
                Some(action) => action,
                None => break,
            },
            () = async {
                match due {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                if chat.run_loop(&mut ui).await == Flow::Quit {
                    break;
                }
                continue;
            }
        };
        match action {
            Action::Submit(text) => {
                if chat.submit(&mut ui, text).await == Flow::Quit {
                    break;
                }
            }
            Action::Quit => break,
            Action::CycleMode => chat.cycle_mode(),
            Action::ToggleVerbose => chat.toggle_verbose(),
            Action::Rewind => {
                if let Err(error) = chat.cmd_rewind("").await {
                    chat.fail(&error);
                }
            }
            Action::Side(question) => chat.cmd_btw(&question).await,
            Action::Interrupt => {}
        }
    }
    chat.save().await;
    let live = chat.ui.is_live();
    let farewell = chat.farewell();
    let last = std::mem::take(&mut chat.last_result);
    ui.shutdown().await;
    if live {
        for row in farewell {
            println!("{row}");
        }
    }
    // A script reading the output wants the answer; a person already saw it.
    Ok(commands::CommandOutcome::plain(if tty { String::new() } else { last }, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::config::ProviderConfig;
    use swarm_core::provider::ProviderKind;

    /// A session with no terminal: scripted input, printed rows captured.
    struct Rig {
        chat: Chat,
        ui: Ui,
        rows: Arc<Mutex<Vec<String>>>,
        _dir: tempfile::TempDir,
    }

    impl Rig {
        fn text(&self) -> String {
            self.rows.lock().unwrap().iter().map(|row| crate::ui::text::strip_ansi(row)).collect::<Vec<_>>().join("\n")
        }
    }

    /// `agent_command` turns the chat into a managed-agent provider running
    /// that shell command, so replies (and the goal judge) can be canned JSON.
    async fn rig(agent_command: Option<&str>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("store")).await.unwrap());
        let workspace = dir.path().join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut cfg = RunConfig::mock("", 1);
        if let Some(command) = agent_command {
            let mut provider = ProviderConfig::mock("agent", "scripted");
            provider.kind = ProviderKind::ManagedAgent;
            provider.command = vec!["sh".into(), "-c".into(), command.into()];
            provider.workspace_dir = Some(workspace.clone());
            cfg.provider = provider.name.clone();
            cfg.providers = vec![provider];
        }
        let (ui, rows) = Ui::scripted(Color::Plain, vec![]);
        let mode = mode_of(cfg.tools.approval);
        let chat = Chat {
            store,
            store_dir: dir.path().join("store"),
            cfg,
            workspace,
            session: Session::new("chat-test"),
            goal: None,
            verbose: false,
            focus: false,
            mode,
            shared_mode: SharedMode::new(mode),
            allow: commands::allow_list(),
            last_run: None,
            last_result: String::new(),
            ui: ui.handle(),
            colors: Color::Plain,
            context_chars: 4000,
            show_transcript: true,
            started: Instant::now(),
            interrupted: None,
            turns_run: 0,
            loop_job: None,
            hooks: Arc::new(Hooks::new(Vec::new(), PathBuf::new(), "chat-test")),
            hook_notes: Vec::new(),
        };
        Rig { chat, ui, rows, _dir: dir }
    }

    #[tokio::test]
    async fn a_goal_runs_a_turn_records_it_and_closes_with_a_footer() {
        let mut rig = rig(None).await;
        let flow = rig.chat.submit(&mut rig.ui, "say hello".into()).await;
        assert_eq!(flow, Flow::Continue);
        assert_eq!(rig.chat.session.turns.len(), 1);
        assert_eq!(rig.chat.session.turns[0].goal, "say hello");
        assert_eq!(rig.chat.session.turns[0].status, "completed");
        assert!(rig.chat.last_result.contains("understood"), "{}", rig.chat.last_result);
        let text = rig.text();
        assert!(text.contains("\u{273b} "), "the turn closes with the flourish line: {text}");
        assert!(text.contains("139 tokens"), "{text}");
        // The next turn is told about the first.
        rig.chat.submit(&mut rig.ui, "and again".into()).await;
        assert_eq!(rig.chat.session.turns.len(), 2);
        assert!(rig.chat.session.totals.tokens > 139);
    }

    #[tokio::test]
    async fn slash_commands_are_handled_without_starting_a_run() {
        let mut rig = rig(None).await;
        assert_eq!(rig.chat.submit(&mut rig.ui, "/cost".into()).await, Flow::Continue);
        assert_eq!(rig.chat.submit(&mut rig.ui, "/hepl".into()).await, Flow::Continue);
        assert!(rig.text().contains("did you mean /help?"), "{}", rig.text());
        assert!(rig.chat.session.turns.is_empty(), "no run was started");
        assert_eq!(rig.chat.submit(&mut rig.ui, "/exit".into()).await, Flow::Quit);
        assert_eq!(rig.chat.submit(&mut rig.ui, "/quit".into()).await, Flow::Quit, "aliases work");
    }

    #[tokio::test]
    async fn undo_and_retry_rewind_the_conversation() {
        let mut rig = rig(None).await;
        rig.chat.submit(&mut rig.ui, "first".into()).await;
        rig.chat.submit(&mut rig.ui, "second".into()).await;
        rig.chat.submit(&mut rig.ui, "/undo".into()).await;
        assert_eq!(rig.chat.session.turns.len(), 1);
        assert!(rig.text().contains("undid 1 turn"), "{}", rig.text());
        rig.chat.submit(&mut rig.ui, "/retry".into()).await;
        assert_eq!(rig.chat.session.turns.len(), 1, "retry replaced the turn it re-ran");
        assert_eq!(rig.chat.session.turns[0].goal, "first");
        rig.chat.submit(&mut rig.ui, "/undo 5".into()).await;
        assert!(rig.chat.session.turns.is_empty());
        rig.chat.submit(&mut rig.ui, "/undo".into()).await;
        assert!(rig.text().contains("nothing to undo"));
    }

    /// Pretend the agent edited `files` during the newest turn: write them as
    /// the agent left them and record the edits the way the runtime does.
    async fn record_edits(rig: &Rig, edits: &[(&str, Option<&str>, &str)]) {
        let run = RunId::parse(&rig.chat.session.turns.last().unwrap().run_id).unwrap();
        for (path, before, after) in edits {
            std::fs::write(rig.chat.workspace.join(path), after).unwrap();
            let event = RunEvent::new(
                EventKind::ToolResult,
                run.clone(),
                json!({"name": "write_file", "success": true, "arguments": {"path": path},
                       "change": {"before": before, "created": before.is_none(),
                                  "after": swarm_tools::content_hash(after)}}),
            );
            rig.chat.store.append_event(&event).await.unwrap();
        }
    }

    #[tokio::test]
    async fn rewind_restores_files_and_the_conversation() {
        let mut rig = rig(None).await;
        std::fs::write(rig.chat.workspace.join("keep.txt"), "v1\n").unwrap();
        rig.chat.submit(&mut rig.ui, "first".into()).await;
        rig.chat.submit(&mut rig.ui, "second".into()).await;
        record_edits(&rig, &[("keep.txt", Some("v1\n"), "v2\n"), ("new.txt", None, "fresh\n")]).await;
        rig.chat.submit(&mut rig.ui, "third".into()).await;

        // Code only: the files go back, the conversation stays.
        rig.chat.submit(&mut rig.ui, "/rewind 2 code".into()).await;
        let work = &rig.chat.workspace;
        assert_eq!(std::fs::read_to_string(work.join("keep.txt")).unwrap(), "v1\n");
        assert!(!work.join("new.txt").exists(), "a file the agent created is removed");
        assert_eq!(rig.chat.session.turns.len(), 3);
        assert!(rig.text().contains("restored 1 file(s), removed 1 file(s) the agent created"), "{}", rig.text());
        assert!(rig.text().contains("shell commands the agent ran are not undone"), "{}", rig.text());

        // Conversation: back to before "second", which was asked after "first".
        rig.chat.submit(&mut rig.ui, "/rewind 2 chat".into()).await;
        let goals: Vec<&str> = rig.chat.session.turns.iter().map(|turn| turn.goal.as_str()).collect();
        assert_eq!(goals, ["first"]);
        assert!(rig.text().contains("rewound 2 turn(s)"), "{}", rig.text());
    }

    #[tokio::test]
    async fn rewind_leaves_a_hand_edited_file_alone() {
        let mut rig = rig(None).await;
        rig.chat.submit(&mut rig.ui, "edit it".into()).await;
        record_edits(&rig, &[("mine.txt", Some("old\n"), "agent\n")]).await;
        std::fs::write(rig.chat.workspace.join("mine.txt"), "my own change\n").unwrap();
        rig.chat.submit(&mut rig.ui, "/rewind 1".into()).await;
        assert_eq!(std::fs::read_to_string(rig.chat.workspace.join("mine.txt")).unwrap(), "my own change\n");
        assert!(rig.text().contains("left mine.txt alone"), "{}", rig.text());
        assert!(rig.chat.session.turns.is_empty(), "the conversation still rewound");
    }

    #[tokio::test]
    async fn rewind_explains_itself_without_a_terminal() {
        let mut rig = rig(None).await;
        rig.chat.submit(&mut rig.ui, "/rewind".into()).await;
        assert!(rig.text().contains("nothing to rewind to yet"), "{}", rig.text());
        rig.chat.submit(&mut rig.ui, "alpha".into()).await;
        rig.chat.submit(&mut rig.ui, "/rewind".into()).await;
        assert!(rig.text().contains("1. alpha"), "{}", rig.text());
        rig.chat.submit(&mut rig.ui, "/rewind 9".into()).await;
        assert!(rig.text().contains("only 1 request(s)"), "{}", rig.text());
        rig.chat.submit(&mut rig.ui, "/rewind sideways".into()).await;
        assert!(rig.text().contains("unknown rewind option `sideways`"), "{}", rig.text());
        assert_eq!(rig.chat.session.turns.len(), 1, "nothing was rewound by mistake");
    }

    #[tokio::test]
    async fn clear_new_and_compact_manage_the_session() {
        let mut rig = rig(None).await;
        for goal in ["a", "b", "c", "d", "e"] {
            rig.chat.submit(&mut rig.ui, goal.into()).await;
        }
        rig.chat.submit(&mut rig.ui, "/compact".into()).await;
        assert_eq!(rig.chat.session.turns.len(), 3);
        assert!(rig.chat.session.digest.is_some());
        let id = rig.chat.session.id.clone();
        rig.chat.submit(&mut rig.ui, "/clear".into()).await;
        assert!(rig.chat.session.turns.is_empty() && rig.chat.session.digest.is_none());
        assert_eq!(rig.chat.session.id, id, "clear keeps the session id");
        rig.chat.submit(&mut rig.ui, "/new".into()).await;
        assert_ne!(rig.chat.session.id, id, "new starts a fresh one");
    }

    #[tokio::test]
    async fn permissions_and_model_changes_apply() {
        let mut rig = rig(None).await;
        rig.chat.submit(&mut rig.ui, "/permissions auto".into()).await;
        assert_eq!(rig.chat.mode, Mode::Auto);
        assert_eq!(rig.chat.shared_mode.get(), Mode::Auto);
        rig.chat.submit(&mut rig.ui, "/permissions plan".into()).await;
        assert_eq!(rig.chat.mode, Mode::Plan);
        rig.chat.submit(&mut rig.ui, "/permissions bogus".into()).await;
        assert_eq!(rig.chat.mode, Mode::Plan, "an unknown mode changes nothing");
        rig.chat.submit(&mut rig.ui, "/model fancy-1".into()).await;
        assert_eq!(rig.chat.cfg.model, "fancy-1");
        rig.chat.submit(&mut rig.ui, "/agents 3".into()).await;
        assert_eq!(rig.chat.cfg.agents, 3);
        rig.chat.cycle_mode();
        assert_eq!(rig.chat.mode, Mode::Default, "plan -> default");
    }

    #[tokio::test]
    async fn a_hash_line_saves_a_note_and_a_bang_line_runs_a_command() {
        let mut rig = rig(None).await;
        rig.chat.submit(&mut rig.ui, "# prefer tabs".into()).await;
        let notes = std::fs::read_to_string(rig.chat.workspace.join("AGENTS.md")).unwrap();
        assert!(notes.contains("- prefer tabs"), "{notes}");
        rig.chat.submit(&mut rig.ui, "!echo from-the-shell".into()).await;
        let text = rig.text();
        assert!(text.contains("Bash(echo from-the-shell)") && text.contains("from-the-shell"), "{text}");
        let turn = rig.chat.session.turns.last().unwrap();
        assert_eq!(turn.goal, "!echo from-the-shell");
        assert!(turn.summary.contains("from-the-shell"), "the next goal can see the output");
        assert_eq!(rig.chat.session.totals.tokens, 0, "a shell command costs no tokens");
    }

    #[tokio::test]
    async fn a_mentioned_file_reaches_the_agent() {
        // The agent just echoes its prompt, so the result shows what it received.
        let mut rig = rig(Some("cat")).await;
        std::fs::write(rig.chat.workspace.join("notes.txt"), "the secret word is plugh\n").unwrap();
        rig.chat.submit(&mut rig.ui, "what does @notes.txt say?".into()).await;
        assert!(rig.text().contains("attached @notes.txt (1 line)"), "{}", rig.text());
        assert!(rig.chat.last_result.contains("plugh"), "{}", rig.chat.last_result);
        assert_eq!(rig.chat.session.turns[0].goal, "what does @notes.txt say?", "history keeps what was typed");
    }

    #[tokio::test]
    async fn a_custom_command_sends_its_filled_in_body_and_retries_as_a_command() {
        let _lock = crate::custom::TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut rig = rig(Some("cat")).await;
        let commands = rig.chat.store_dir.join("commands");
        std::fs::create_dir_all(&commands).unwrap();
        std::fs::write(
            commands.join("zzfix.md"),
            "---\ndescription: Fix a file\nargument-hint: <file>\n---\nPlease fix $1 and explain what was wrong.",
        )
        .unwrap();
        rig.chat.submit(&mut rig.ui, "/reload".into()).await;
        assert!(rig.text().contains("1 custom command(s): /zzfix"), "{}", rig.text());
        assert!(rig.text().contains("/zzfix") && crate::ui::registry::matching("zzf").len() == 1);

        rig.chat.submit(&mut rig.ui, "/zzfix src/main.rs".into()).await;
        assert!(rig.chat.last_result.contains("Please fix src/main.rs and explain"), "{}", rig.chat.last_result);
        assert_eq!(rig.chat.session.turns[0].goal, "/zzfix src/main.rs", "history keeps what was typed");

        // The file is read again on each use, so an edit applies at once.
        std::fs::write(commands.join("zzfix.md"), "Second thoughts about $ARGUMENTS").unwrap();
        rig.chat.submit(&mut rig.ui, "/retry".into()).await;
        assert!(rig.chat.last_result.contains("Second thoughts about src/main.rs"), "{}", rig.chat.last_result);
        assert_eq!(rig.chat.session.turns.len(), 1, "the retry replaced the turn it re-ran");

        // A command whose file is gone says so instead of sending "/zzfix" to the model.
        std::fs::remove_file(commands.join("zzfix.md")).unwrap();
        rig.chat.submit(&mut rig.ui, "/zzfix again".into()).await;
        assert!(rig.text().contains("could not read"), "{}", rig.text());
        assert_eq!(rig.chat.session.turns.len(), 1);
        crate::custom::install(Vec::new());
    }

    fn with_hooks(rig: &mut Rig, settings: &str) {
        let (entries, _, warnings) = crate::hooks::parse(settings);
        assert!(warnings.is_empty(), "{warnings:?}");
        rig.chat.hooks = Arc::new(Hooks::new(entries, rig.chat.workspace.clone(), "chat-test"));
    }

    #[tokio::test]
    async fn a_prompt_hook_can_stop_a_request_or_add_context_to_it() {
        let mut rig = rig(Some("cat")).await;
        rig.chat.submit(&mut rig.ui, "/hooks".into()).await;
        assert!(rig.text().contains("no hooks"), "{}", rig.text());

        with_hooks(
            &mut rig,
            r#"{"hooks": {"UserPromptSubmit": [{"hooks": [
                {"command": "grep -q secret && echo 'no secrets in prompts' >&2 && exit 2; echo 'today is Tuesday'"}]}]}}"#,
        );
        rig.chat.submit(&mut rig.ui, "what day is it?".into()).await;
        assert!(rig.chat.last_result.contains("what day is it?"), "{}", rig.chat.last_result);
        assert!(rig.chat.last_result.contains("Context from a hook:\ntoday is Tuesday"), "{}", rig.chat.last_result);
        assert_eq!(rig.chat.session.turns[0].goal, "what day is it?", "history keeps what was typed, not the context");

        rig.chat.submit(&mut rig.ui, "the secret is 42".into()).await;
        assert!(rig.text().contains("blocked by a hook: no secrets in prompts"), "{}", rig.text());
        assert_eq!(rig.chat.session.turns.len(), 1, "a blocked prompt never ran");
        // The scripted UI only hands out input while it is not busy, so this
        // arriving at all shows a blocked prompt left it idle.
        let next = tokio::time::timeout(Duration::from_millis(500), rig.ui.recv()).await;
        assert!(matches!(next, Ok(Some(Action::Quit))), "the prompt is usable again: {next:?}");

        rig.chat.submit(&mut rig.ui, "/hooks".into()).await;
        assert!(rig.text().contains("1 hook(s):") && rig.text().contains("UserPromptSubmit:"), "{}", rig.text());
    }

    #[tokio::test]
    async fn a_stop_hook_runs_once_per_request_even_for_a_goal_with_many_turns() {
        let mut rig = rig(Some(r#"cat >/dev/null; echo '{"verdict": "continue", "reason": "more"}'"#)).await;
        let counter = rig.chat.workspace.join("stops");
        with_hooks(
            &mut rig,
            &format!(r#"{{"hooks": {{"Stop": [{{"hooks": [{{"command": "echo x >> {}"}}]}}]}}}}"#, counter.display()),
        );
        rig.chat.goal = Some(Goal::new("keep going"));
        rig.chat.goal.as_mut().unwrap().max_turns = 3;
        rig.chat.submit(&mut rig.ui, "work on it".into()).await;
        assert!(rig.chat.session.turns.len() >= 3, "the goal ran several turns");
        let stops = std::fs::read_to_string(&counter).unwrap();
        assert_eq!(stops.lines().count(), 1, "one Stop for the whole request: {stops:?}");
        rig.chat.submit(&mut rig.ui, "/hooks".into()).await;
    }

    #[tokio::test]
    async fn the_checklist_is_read_back_from_the_run_and_shown_by_todos() {
        let mut rig = rig(None).await;
        rig.chat.submit(&mut rig.ui, "/todos".into()).await;
        assert!(rig.text().contains("no todo list"), "{}", rig.text());
        rig.chat.submit(&mut rig.ui, "plan the work".into()).await;
        let run_id = rig.chat.session.turns[0].run_id.clone();
        let run = RunId::parse(&run_id).unwrap();
        let write = |todos: serde_json::Value| {
            RunEvent::new(
                EventKind::ToolResult,
                run.clone(),
                json!({"name": "todo_write", "success": true, "arguments": {"todos": todos}, "output": "ok"}),
            )
        };
        rig.chat.store.append_event(&write(json!([{"content": "old", "status": "pending"}]))).await.unwrap();
        rig.chat
            .store
            .append_event(&write(json!([
                {"content": "write the parser", "status": "completed"},
                {"content": "test the parser", "status": "in_progress", "activeForm": "Testing the parser"}
            ])))
            .await
            .unwrap();
        let todos = rig.chat.todos_of(&run_id).await.expect("the run kept a list");
        assert_eq!(todos.len(), 2, "the last update is the list");
        rig.chat.session.turns[0].todos = Some(todos);
        rig.chat.submit(&mut rig.ui, "/todos".into()).await;
        let text = rig.text();
        assert!(text.contains("Todos (1 of 2 done)"), "{text}");
        assert!(text.contains("write the parser") && text.contains("Testing the parser"), "{text}");
    }

    #[tokio::test]
    async fn btw_answers_without_joining_the_conversation() {
        let mut rig = rig(None).await;
        rig.chat.submit(&mut rig.ui, "build the thing".into()).await;
        let before = rig.chat.session.clone();
        rig.chat.submit(&mut rig.ui, "/btw what did we just do?".into()).await;
        let text = rig.text();
        assert!(text.contains("btw: what did we just do?"), "{text}");
        assert!(text.contains("understood"), "the model's answer is shown: {text}");
        assert!(text.contains("not added to the conversation"), "{text}");
        assert_eq!(rig.chat.session, before, "nothing was recorded, not even tokens");
        rig.chat.submit(&mut rig.ui, "/btw".into()).await;
        assert!(rig.text().contains("usage: /btw <question>"), "{}", rig.text());
    }

    #[tokio::test]
    async fn a_loop_runs_its_prompt_reschedules_and_stops_itself() {
        use tokio::time::Instant as At;
        let mut rig = rig(None).await;
        rig.chat.submit(&mut rig.ui, "/loop 5m check the build".into()).await;
        assert!(rig.text().contains("/loop needs the interactive prompt"), "{}", rig.text());
        assert!(rig.chat.loop_job.is_none());

        let every = Duration::from_secs(300);
        rig.chat.loop_job = Some(repeat::Loop { every, prompt: "check the build".into(), next: At::now(), runs: 0 });
        assert_eq!(rig.chat.loop_chip().as_deref(), Some("\u{21bb} loop 5m"));
        assert!(rig.chat.info().goal.as_deref().is_some_and(|chip| chip.contains("loop 5m")), "shown in the footer");
        let flow = rig.chat.run_loop(&mut rig.ui).await;
        assert_eq!(flow, Flow::Continue);
        assert_eq!(rig.chat.session.turns.len(), 1);
        assert_eq!(rig.chat.session.turns[0].goal, "check the build", "run as if typed");
        let job = rig.chat.loop_job.as_ref().expect("a finished run keeps the loop going");
        assert_eq!(job.runs, 1);
        assert!(job.next > At::now() + Duration::from_secs(290), "the next run is a full interval away");

        // The last allowed run says so and stops.
        rig.chat.loop_job.as_mut().unwrap().runs = repeat::MAX_RUNS - 1;
        rig.chat.run_loop(&mut rig.ui).await;
        assert!(rig.chat.loop_job.is_none());
        assert!(rig.text().contains("loop stopped after 50 runs"), "{}", rig.text());

        // Status and stop.
        rig.chat.submit(&mut rig.ui, "/loop".into()).await;
        assert!(rig.text().contains("no loop"), "{}", rig.text());
    }

    #[tokio::test]
    async fn a_loop_stops_when_a_run_fails() {
        use tokio::time::Instant as At;
        let mut rig = rig(Some("exit 1")).await;
        rig.chat.loop_job = Some(repeat::Loop {
            every: Duration::from_secs(60),
            prompt: "poke it".into(),
            next: At::now(),
            runs: 0,
        });
        rig.chat.run_loop(&mut rig.ui).await;
        assert!(rig.chat.loop_job.is_none(), "an unattended loop does not keep failing");
        assert!(rig.text().contains("loop stopped: that run did not finish"), "{}", rig.text());
    }

    #[tokio::test]
    async fn subgoals_extend_the_active_goal_and_survive_a_restart() {
        let mut rig = rig(Some(r#"cat >/dev/null; echo '{"verdict": "done", "reason": "ok"}'"#)).await;
        rig.chat.submit(&mut rig.ui, "/subgoal tests are green".into()).await;
        assert!(rig.text().contains("no goal to add to"), "{}", rig.text());
        rig.chat.goal = Some(Goal::new("ship it"));
        rig.chat.submit(&mut rig.ui, "/subgoal tests are green".into()).await;
        rig.chat.submit(&mut rig.ui, "/subgoal docs mention it".into()).await;
        rig.chat.submit(&mut rig.ui, "/subgoal".into()).await;
        assert!(rig.text().contains("1. tests are green") && rig.text().contains("2. docs mention it"), "{}", rig.text());
        rig.chat.submit(&mut rig.ui, "/subgoal rm 1".into()).await;
        rig.chat.submit(&mut rig.ui, "/subgoal rm 9".into()).await;
        assert!(rig.text().contains("no condition 9"), "{}", rig.text());
        let saved = rig.chat.store.kv_get(KV_NAMESPACE, KV_GOAL).await.unwrap().unwrap();
        let reloaded: Goal = serde_json::from_str(&saved).unwrap();
        assert_eq!(reloaded.criteria, vec!["docs mention it".to_string()], "saved with the goal");
        rig.chat.submit(&mut rig.ui, "/subgoal clear".into()).await;
        assert!(rig.chat.goal.as_ref().unwrap().criteria.is_empty());
    }

    #[tokio::test]
    async fn focus_toggles_and_a_turn_still_completes_in_it() {
        let mut rig = rig(None).await;
        assert!(!rig.chat.focus);
        rig.chat.submit(&mut rig.ui, "/focus".into()).await;
        assert!(rig.chat.focus && rig.text().contains("focus view on"), "{}", rig.text());
        rig.chat.submit(&mut rig.ui, "/focus off".into()).await;
        assert!(!rig.chat.focus);
        rig.chat.submit(&mut rig.ui, "/focus on".into()).await;
        assert!(rig.chat.focus);
        // (The rig has no terminal, so no transcript rows print; what the focus
        // view shows is covered by the transcript tests.)
        rig.chat.submit(&mut rig.ui, "say hi".into()).await;
        assert!(rig.chat.last_result.contains("understood"), "{}", rig.chat.last_result);
    }

    #[tokio::test]
    async fn review_asks_for_findings_and_forbids_edits() {
        let mut rig = rig(Some("cat")).await;
        rig.chat.submit(&mut rig.ui, "/review".into()).await;
        let sent = rig.chat.last_result.clone();
        assert!(sent.contains("git diff") && sent.contains("change nothing"), "{sent}");
        rig.chat.submit(&mut rig.ui, "/security-review src/auth.rs".into()).await;
        let sent = rig.chat.last_result.clone();
        assert!(sent.contains("src/auth.rs") && sent.contains("injection"), "{sent}");
        assert!(!sent.contains("git status"), "an explicit target replaces the default scope");
        assert_eq!(rig.chat.session.turns[1].goal, "/security-review src/auth.rs");
    }

    #[tokio::test]
    async fn a_goal_runs_until_the_judge_says_done() {
        // The scripted agent answers every call, the turn and the judge alike,
        // with a "done" verdict.
        let mut rig = rig(Some(r#"cat >/dev/null; echo '{"verdict": "done", "reason": "all good"}'"#)).await;
        rig.chat.submit(&mut rig.ui, "/goal make it work".into()).await;
        let text = rig.text();
        assert!(text.contains("Goal set (20-turn budget): make it work"), "{text}");
        assert!(text.contains("\u{2713} Goal achieved: all good"), "{text}");
        let goal = rig.chat.goal.as_ref().unwrap();
        assert_eq!(goal.status, goal::GoalStatus::Done);
        assert_eq!(goal.turns_used, 1);
        assert_eq!(rig.chat.session.turns.len(), 1);
        assert_eq!(rig.chat.session.turns[0].goal, "/goal make it work");
        assert_eq!(rig.chat.info().goal, None, "a finished goal leaves the footer");
    }

    #[tokio::test]
    async fn a_goal_with_an_unreadable_judge_pauses_instead_of_looping_forever() {
        // Plain text is not a verdict, so the judge never answers usefully.
        let mut rig = rig(None).await;
        rig.chat.submit(&mut rig.ui, "/goal never ends".into()).await;
        let goal = rig.chat.goal.as_ref().unwrap();
        assert_eq!(goal.status, goal::GoalStatus::Paused, "{}", rig.text());
        assert_eq!(goal.turns_used, 3, "three unusable verdicts, then it stops");
        let text = rig.text();
        assert!(text.contains("Goal paused") && text.contains("three times"), "{text}");
        assert!(text.matches("Continuing toward the goal").count() == 2, "{text}");
        assert_eq!(rig.chat.info().goal.as_deref(), Some("\u{23f8} goal paused"));
        // /goal resume gives it a fresh budget; /goal clear drops it.
        rig.chat.submit(&mut rig.ui, "/goal clear".into()).await;
        assert!(rig.chat.goal.is_none());
    }

    #[tokio::test]
    async fn plan_mode_asks_the_agent_to_plan_and_not_to_act() {
        // The agent echoes its prompt, so we can read what plan mode sent.
        let mut rig = rig(Some("cat")).await;
        rig.chat.submit(&mut rig.ui, "/plan add caching".into()).await;
        assert_eq!(rig.chat.mode, Mode::Plan);
        assert!(rig.chat.last_result.contains("PLAN MODE") && rig.chat.last_result.contains("add caching"), "{}", rig.chat.last_result);
        assert!(rig.chat.last_result.contains("Do not change anything"), "{}", rig.chat.last_result);
    }

    #[tokio::test]
    async fn export_copy_memory_and_config_report_something_useful() {
        let mut rig = rig(None).await;
        rig.chat.submit(&mut rig.ui, "hello".into()).await;
        rig.chat.submit(&mut rig.ui, "/export notes.md".into()).await;
        let md = std::fs::read_to_string(rig.chat.workspace.join("notes.md")).unwrap();
        assert!(md.contains("## 1. hello") && md.contains("understood"), "{md}");
        rig.chat.submit(&mut rig.ui, "/copy".into()).await;
        assert!(rig.text().contains("[mock:mock-model] reply #1"), "no terminal: the text is printed instead");
        rig.chat.submit(&mut rig.ui, "/memory".into()).await;
        assert!(rig.text().contains("no AGENTS.md"), "{}", rig.text());
        rig.chat.submit(&mut rig.ui, "/config".into()).await;
        assert!(rig.text().contains("provider    mock") && rig.text().contains("approvals"), "{}", rig.text());
        rig.chat.submit(&mut rig.ui, "/context".into()).await;
        assert!(rig.text().contains("1 turn(s) in context"), "{}", rig.text());
        rig.chat.submit(&mut rig.ui, "/tools".into()).await;
        assert!(rig.text().contains("write_file"), "{}", rig.text());
    }

    #[test]
    fn a_hash_note_lands_in_agents_md() {
        let dir = std::env::temp_dir().join(format!("swarm-memory-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::remove_file(dir.join("AGENTS.md")).ok();
        assert_eq!(remember_note(&dir, "# use uv, not pip").unwrap(), 15);
        assert_eq!(remember_note(&dir, "#  run pytest -q  ").unwrap(), 13);
        let body = std::fs::read_to_string(dir.join("AGENTS.md")).unwrap();
        assert!(body.starts_with("# Project instructions\n"), "{body}");
        assert!(body.contains("- use uv, not pip") && body.contains("- run pytest -q"), "{body}");
        assert!(remember_note(&dir, "   #   ").is_err(), "an empty note is an error");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn shell_commands_capture_both_streams_and_the_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let (out, code) = run_shell(dir.path().to_path_buf(), "echo hello; echo oops >&2; exit 3".into()).await;
        assert!(out.contains("hello") && out.contains("oops"), "{out:?}");
        assert_eq!(code, Some(3));
        let (pwd, _) = run_shell(dir.path().to_path_buf(), "pwd".into()).await;
        assert_eq!(pwd.trim(), dir.path().canonicalize().unwrap().to_string_lossy());
    }

    #[tokio::test]
    async fn a_shell_command_cannot_read_the_keyboard() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let (out, code) = run_shell(dir.path().to_path_buf(), "cat; echo finished".into()).await;
        assert!(out.contains("finished") && code == Some(0), "{out:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn the_session_and_goal_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("store")).await.unwrap();
        let mut session = Session::new("chat-test");
        session.push(Turn { goal: "g".into(), status: "completed".into(), summary: "s".into(), run_id: String::new(), todos: None });
        store.kv_set(KV_NAMESPACE, KV_ACTIVE, &session.to_json()).await.unwrap();
        let goal = Goal::new("finish the work");
        store.kv_set(KV_NAMESPACE, KV_GOAL, &serde_json::to_string(&goal).unwrap()).await.unwrap();
        let args = commands::default_chat_args();
        let loaded = load_session(&store, &args).await.unwrap();
        assert_eq!(loaded, session);
        assert_eq!(load_goal(&store, &args).await.unwrap().text, "finish the work");
        let fresh = ChatArgs { new: true, ..commands::default_chat_args() };
        assert_ne!(load_session(&store, &fresh).await.unwrap().id, "chat-test", "--new starts over");
        assert!(load_goal(&store, &fresh).await.is_none());
        // A finished goal is not revived.
        let mut done = goal;
        done.status = goal::GoalStatus::Done;
        store.kv_set(KV_NAMESPACE, KV_GOAL, &serde_json::to_string(&done).unwrap()).await.unwrap();
        assert!(load_goal(&store, &args).await.is_none());
    }
}
