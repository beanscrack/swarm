//! Slash commands. The table lives in [`crate::ui::registry`]; this is what
//! each one does.

use std::time::Duration;

use anyhow::Result;
use swarm_core::RunId;

use super::goal::{self, Goal, GoalStatus};
use super::session::{Session, Turn, clip};
use super::{
    Chat, Flow, INIT_PROMPT, REVIEW_PROMPT, REVIEW_REPORT, REVIEW_SCOPE, Request, SECURITY_FOCUS, mentions, view,
};
use crate::commands;
use crate::custom;
use crate::style::{self, role};
use crate::ui::registry::{self, Group, Resolved};
use crate::ui::{DialogSpec, Mode, Ui};

impl Chat {
    pub(super) async fn slash(&mut self, ui: &mut Ui, word: &str, args: &str) -> Result<Flow> {
        let spec = match registry::resolve(word) {
            Resolved::Exact(spec) | Resolved::Unique(spec) => spec,
            Resolved::Ambiguous(names) => {
                let list = names.iter().map(|name| format!("/{name}")).collect::<Vec<_>>().join("  ");
                self.say(format!("ambiguous command /{word}: {list}"));
                return Ok(Flow::Continue);
            }
            Resolved::Unknown(near) => {
                let hint = match near.as_slice() {
                    [] => " \u{00b7} /help lists the commands".to_string(),
                    [one] => format!(" \u{2014} did you mean /{one}?"),
                    many => format!(
                        " \u{2014} did you mean {}?",
                        many.iter().map(|name| format!("/{name}")).collect::<Vec<_>>().join(", ")
                    ),
                };
                self.say(format!("unknown command /{word}, nothing was sent{hint}"));
                return Ok(Flow::Continue);
            }
        };
        if spec.group == Group::Custom {
            return self.cmd_custom(ui, spec.name, args).await;
        }
        match spec.name {
            "help" => self.ui.print(view::help(self.colors, (!args.is_empty()).then_some(args))),
            "exit" => return Ok(Flow::Quit),
            "clear" => self.cmd_clear(false),
            "new" => self.cmd_clear(true),
            "compact" => {
                let folded = self.session.compact(3);
                self.say(if folded == 0 {
                    "nothing to compact".to_string()
                } else {
                    format!("folded {folded} turn(s) into a digest")
                });
            }
            "resume" => self.cmd_resume(args).await?,
            "rewind" => self.cmd_rewind(args).await?,
            "undo" => self.cmd_undo(args),
            "retry" => return self.cmd_retry(ui).await,
            "copy" => self.cmd_copy(),
            "export" => self.cmd_export(args)?,
            "btw" => self.cmd_btw(args).await,
            "loop" => self.cmd_loop(args),
            "goal" => return self.cmd_goal(ui, args).await,
            "subgoal" => self.cmd_subgoal(args).await,
            "plan" => {
                if args.is_empty() {
                    self.say("usage: /plan <task>  explores read-only first, then asks before changing anything");
                } else {
                    self.set_mode(Mode::Plan);
                    return self.work(ui, Request::user(args)).await;
                }
            }
            "init" => return self.work(ui, Request { display: "/init".into(), prompt: INIT_PROMPT.into() }).await,
            "review" | "security-review" => {
                let security = spec.name == "security-review";
                let target = if args.is_empty() { REVIEW_SCOPE.to_string() } else { format!("Review this: {args}.") };
                let focus = if security { format!("{SECURITY_FOCUS}\n\n") } else { String::new() };
                let prompt = format!("{REVIEW_PROMPT}\n\n{target}\n\n{focus}{REVIEW_REPORT}");
                let display = if args.is_empty() { format!("/{}", spec.name) } else { format!("/{} {args}", spec.name) };
                return self.work(ui, Request { display, prompt }).await;
            }
            "reload" => self.cmd_reload(),
            "agents" => self.cmd_agents(args),
            "model" => self.cmd_model(args),
            "permissions" => self.cmd_permissions(args).await,
            "allow" => self.cmd_allow(args),
            "allow-all" => {
                self.set_mode(Mode::Auto);
                self.say("approvals: every tool is allowed for this session");
            }
            "verbose" => {
                self.verbose = match args {
                    "on" | "true" | "1" => true,
                    "off" | "false" | "0" => false,
                    _ => !self.verbose,
                };
                self.push_info();
                self.say(format!("transcript detail: {}", if self.verbose { "on" } else { "off" }));
            }
            "focus" => {
                self.focus = match args {
                    "on" | "true" | "1" => true,
                    "off" | "false" | "0" => false,
                    _ => !self.focus,
                };
                self.say(if self.focus {
                    "focus view on \u{00b7} tool activity is hidden unless it fails; /focus turns it off"
                } else {
                    "focus view off"
                });
            }
            "memory" => self.cmd_memory(args)?,
            "config" => self.cmd_config(),
            "status" => self.cmd_status(args).await?,
            "cost" => self.cmd_cost(),
            "context" => self.cmd_context(),
            "todos" => self.cmd_todos()?,
            "tasks" => self.cmd_inspect("tasks", args).await?,
            "logs" => self.cmd_inspect("logs", args).await?,
            "runs" => self.cmd_runs().await?,
            "history" => self.cmd_history(),
            "tools" => {
                let names = commands::tool_names(&self.cfg, &self.workspace)?;
                self.say(format!("tools ({}): {}", names.len(), names.join(", ")));
            }
            "hooks" => self.cmd_hooks(),
            "diff" => self.cmd_diff(args).await,
            "doctor" => self.cmd_doctor().await,
            // `/queue` and `/busy` are handled by the UI before they get here.
            "queue" | "busy" | "redraw" | "vim" => self.say("this command works in the interactive prompt"),
            other => self.say(format!("/{other} is not implemented")),
        }
        Ok(Flow::Continue)
    }

    /// A command from a markdown file: its body, filled in, is the request.
    async fn cmd_custom(&mut self, ui: &mut Ui, name: &str, args: &str) -> Result<Flow> {
        let Some(command) = custom::find(name) else {
            self.say(format!("/{name} is gone \u{00b7} /reload re-reads the command files"));
            return Ok(Flow::Continue);
        };
        let body = match custom::body_of(&command) {
            Ok(body) if !body.trim().is_empty() => body,
            Ok(_) => {
                self.say(format!("/{name} is empty: {}", command.path.display()));
                return Ok(Flow::Continue);
            }
            Err(error) => {
                self.say(format!("could not read {}: {error}", command.path.display()));
                return Ok(Flow::Continue);
            }
        };
        let expansion = mentions::expand(&self.workspace, &custom::expand(&body, args));
        if !expansion.notes.is_empty() {
            self.say_lines(expansion.notes.clone());
        }
        let display = if args.is_empty() { format!("/{name}") } else { format!("/{name} {args}") };
        self.work(ui, Request { display, prompt: expansion.text }).await
    }

    /// `/reload`: look for command files again.
    fn cmd_reload(&mut self) {
        let dirs = custom::dirs(&self.store_dir, &self.workspace);
        let scan = custom::scan(&dirs);
        let names: Vec<String> = scan.commands.iter().map(|command| format!("/{}", command.name)).collect();
        custom::install(scan.commands);
        if names.is_empty() {
            self.say("no custom commands \u{00b7} add markdown files to .swarm/commands/ (/fix-tests = fix-tests.md)");
        } else {
            self.say(format!("{} custom command(s): {}", names.len(), names.join("  ")));
        }
        for (name, why) in scan.skipped {
            self.say(format!("skipped {name}: {why}"));
        }
    }

    /// `/clear` forgets the conversation; `/new` also starts a new session id.
    fn cmd_clear(&mut self, fresh: bool) {
        if fresh {
            self.session = Session::with_id();
            self.hooks.set_session(&self.session.id);
            self.allow.clear();
        } else {
            self.session.turns.clear();
            self.session.digest = None;
        }
        self.goal = None;
        self.last_result.clear();
        self.ui.clear_screen();
        self.welcome();
        self.push_info();
    }

    async fn cmd_resume(&mut self, args: &str) -> Result<()> {
        let id = if args.is_empty() {
            let runs: Vec<_> = self.store.list_runs().await?.into_iter().take(12).collect();
            if runs.is_empty() {
                self.say("no runs in this store yet");
                return Ok(());
            }
            let options = runs
                .iter()
                .map(|run| format!("{:<10} {}  {}", run.status.to_string(), clip(&run.id.to_string(), 14), clip(&run.objective, 60)))
                .collect();
            match self.ui.choose(DialogSpec::pick("Resume which run?", options)).await {
                Some(index) => runs[index].id.to_string(),
                None => {
                    if !self.ui.is_live() {
                        self.say("pass a run id: /resume <run>");
                    }
                    return Ok(());
                }
            }
        } else {
            args.to_string()
        };
        let run = self.store.load_run_required(&RunId::parse(&id)?).await?;
        let summary = commands::run_summary(&self.store, &run.id).await?;
        self.session.push(Turn {
            goal: run.objective.clone(),
            status: run.status.to_string(),
            summary,
            run_id: run.id.to_string(),
            todos: None,
        });
        self.last_run = Some(run.id.clone());
        self.say(format!("loaded {} ({}) into this session as context", run.id, run.status));
        Ok(())
    }

    /// `/undo [n]`: take the last turns out of the conversation and put the
    /// newest request back in the prompt to edit. The files the agent changed
    /// stay changed; this only rewinds what it remembers.
    fn cmd_undo(&mut self, args: &str) {
        let count: usize = args.parse().unwrap_or(1).max(1);
        let mut last = None;
        let mut undone = 0;
        for _ in 0..count {
            match self.session.pop_request() {
                Some(turn) => {
                    undone += 1;
                    last = Some(turn);
                }
                None => break,
            }
        }
        let Some(turn) = last else {
            self.say("nothing to undo");
            return;
        };
        self.ui.prefill(turn.goal.clone());
        self.say(format!(
            "undid {undone} turn{} \u{00b7} the request is back in the prompt; file changes are not reverted",
            if undone == 1 { "" } else { "s" }
        ));
        self.push_info();
    }

    async fn cmd_retry(&mut self, ui: &mut Ui) -> Result<Flow> {
        match self.session.pop_request() {
            Some(turn) => {
                self.say(format!("retrying: {}", clip(&turn.goal, 70)));
                // Through the front door, so a retried `/command` runs again
                // as a command instead of being sent to the model as text.
                Box::pin(self.dispatch(ui, &turn.goal)).await
            }
            None => {
                self.say("nothing to retry");
                Ok(Flow::Continue)
            }
        }
    }

    fn cmd_copy(&mut self) {
        if self.last_result.trim().is_empty() {
            self.say("no result to copy yet");
            return;
        }
        let text = self.last_result.clone();
        if self.ui.is_live() {
            // The terminal sets the clipboard from this escape, even over SSH.
            let sequence = view::osc52(&text, std::env::var_os("TMUX").is_some());
            self.ui.print(vec![format!(
                "{sequence}{}",
                role(self.colors, "subtle", &format!("  copied {} characters to the clipboard", text.chars().count()))
            )]);
        } else {
            self.ui.print(text.lines().map(str::to_string).collect());
        }
    }

    fn cmd_export(&mut self, args: &str) -> Result<()> {
        let name = if args.is_empty() { format!("swarm-session-{}.md", clip(&self.session.id, 20)) } else { args.to_string() };
        let path = self.workspace.join(&name);
        std::fs::write(&path, view::export_markdown(&self.session))?;
        self.say(format!("wrote {} turn(s) to {}", self.session.turns.len(), path.display()));
        Ok(())
    }

    async fn cmd_goal(&mut self, ui: &mut Ui, args: &str) -> Result<Flow> {
        match goal::parse_command(args) {
            goal::Command::Status => {
                match &self.goal {
                    Some(goal) => self.say(goal.describe()),
                    None => self.say("no goal set \u{00b7} /goal <condition> keeps working until it is met"),
                }
                Ok(Flow::Continue)
            }
            goal::Command::Set(text) if text.is_empty() => {
                self.say("usage: /goal <what must be true when you are done>");
                Ok(Flow::Continue)
            }
            goal::Command::Set(text) => {
                let goal = Goal::new(&text);
                self.ui.print(vec![role(
                    self.colors,
                    "plan",
                    &format!("\u{2299} Goal set ({}-turn budget): {}", goal.max_turns, clip(&goal.text, 100)),
                )]);
                self.say("/goal pause \u{00b7} /goal resume \u{00b7} /goal clear");
                let request = Request { display: format!("/goal {text}"), prompt: goal.kickoff_prompt() };
                self.goal = Some(goal);
                self.push_info();
                self.work(ui, request).await
            }
            goal::Command::Pause => {
                match self.goal.as_mut().filter(|goal| goal.status == GoalStatus::Active) {
                    Some(goal) => {
                        goal.pause("paused by you");
                        self.say("\u{23f8} Goal paused \u{00b7} /goal resume to continue");
                    }
                    None => self.say("no active goal"),
                }
                self.push_info();
                self.save().await;
                Ok(Flow::Continue)
            }
            goal::Command::Resume => {
                let Some(goal) = self.goal.as_mut().filter(|goal| goal.status != GoalStatus::Done) else {
                    self.say("no goal to resume");
                    return Ok(Flow::Continue);
                };
                goal.resume();
                let (prompt, budget) = (goal.resume_prompt(), goal.max_turns);
                self.say(format!("\u{2299} Goal resumed ({budget}-turn budget)"));
                self.push_info();
                self.work(ui, Request { display: "/goal resume".into(), prompt }).await
            }
            goal::Command::Clear => {
                let had_goal = self.goal.take().is_some();
                self.say(if had_goal { "goal cleared" } else { "no goal set" });
                self.push_info();
                self.save().await;
                Ok(Flow::Continue)
            }
        }
    }

    /// `/subgoal`: extra conditions the active goal's judge also checks.
    async fn cmd_subgoal(&mut self, args: &str) {
        let Some(goal) = self.goal.as_mut().filter(|goal| goal.status != GoalStatus::Done) else {
            self.say("no goal to add to \u{00b7} /goal <condition> sets one");
            return;
        };
        match goal::parse_subcommand(args) {
            goal::SubCommand::List => {
                let lines = if goal.criteria.is_empty() {
                    vec!["no extra conditions \u{00b7} /subgoal <text> adds one the goal must also meet".to_string()]
                } else {
                    goal.criteria.iter().enumerate().map(|(index, text)| format!("{}. {text}", index + 1)).collect()
                };
                self.say_lines(lines);
                return;
            }
            goal::SubCommand::Add(text) => {
                if goal.criteria.len() >= goal::MAX_CRITERIA {
                    self.say(format!("a goal can carry {} extra conditions; /subgoal rm <n> frees one", goal::MAX_CRITERIA));
                    return;
                }
                let text = clip(&text, goal::CRITERION_CHARS);
                goal.criteria.push(text.clone());
                let count = goal.criteria.len();
                self.say(format!("added condition {count}: {text}"));
            }
            goal::SubCommand::Remove(n) => {
                if n == 0 || n > goal.criteria.len() {
                    self.say(format!("no condition {n} \u{00b7} /subgoal lists them"));
                    return;
                }
                let gone = goal.criteria.remove(n - 1);
                self.say(format!("removed: {gone}"));
            }
            goal::SubCommand::Clear => {
                let count = std::mem::take(&mut goal.criteria).len();
                self.say(format!("removed {count} condition(s)"));
            }
        }
        self.save().await;
    }

    fn cmd_agents(&mut self, args: &str) {
        match args.parse::<u32>() {
            Ok(n) if n >= 1 => {
                self.cfg.agents = n;
                self.cfg.budget.max_inference_calls = self.cfg.budget.max_inference_calls.max(u64::from(n) * 40);
                self.say(format!("agents \u{2192} {n}"));
                self.push_info();
            }
            _ => self.say(format!("agents: {} \u{00b7} /agents <n> sets the swarm size", self.cfg.agents)),
        }
    }

    fn cmd_model(&mut self, args: &str) {
        if args.is_empty() {
            self.say(format!("model: {} ({})", self.cfg.model, self.cfg.provider));
            return;
        }
        self.cfg.model = args.to_string();
        if let Some(selected) = self.cfg.providers.iter_mut().find(|p| p.name == self.cfg.provider) {
            selected.model = args.to_string();
        }
        self.say(format!("model \u{2192} {args}"));
        self.push_info();
    }

    async fn cmd_permissions(&mut self, args: &str) {
        let mode = match args.to_ascii_lowercase().as_str() {
            "" => {
                let options = vec![
                    "Default: ask before shell, python, git and tests".to_string(),
                    "Ask before every tool".to_string(),
                    "Auto: approve every tool".to_string(),
                    "Plan: read-only, then ask to proceed".to_string(),
                ];
                match self.ui.choose(DialogSpec::pick("Permission mode", options)).await {
                    Some(0) => Mode::Default,
                    Some(1) => Mode::AskAll,
                    Some(2) => Mode::Auto,
                    Some(3) => Mode::Plan,
                    _ => {
                        if !self.ui.is_live() {
                            self.say(format!("mode: {}", self.cfg.tools.approval));
                        }
                        return;
                    }
                }
            }
            "ask" | "danger" | "dangerous" | "default" => Mode::Default,
            "all" | "ask_all" | "every" => Mode::AskAll,
            "auto" | "never" | "yolo" => Mode::Auto,
            "plan" => Mode::Plan,
            other => {
                self.say(format!("unknown mode \"{other}\" \u{00b7} ask, all, auto or plan"));
                return;
            }
        };
        if mode == Mode::Default {
            self.allow.clear();
        }
        self.set_mode(mode);
        self.say(format!("permissions \u{2192} {}", crate::approval::policy_of(mode)));
    }

    fn cmd_allow(&mut self, args: &str) {
        if args.is_empty() {
            let names = self.allow.names();
            self.say(if names.is_empty() {
                "no session-allowed tools".to_string()
            } else {
                format!("session-allowed tools: {}", names.join(", "))
            });
        } else {
            self.allow.allow(args);
            self.say(format!("approvals: {args} is allowed for this session"));
        }
    }

    fn cmd_memory(&mut self, args: &str) -> Result<()> {
        let path = self.workspace.join("AGENTS.md");
        if args == "edit" {
            if !path.exists() {
                std::fs::write(&path, "# Project instructions\n")?;
            }
            self.ui.edit_file(path);
            return Ok(());
        }
        match std::fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => {
                let total = text.lines().count();
                let mut rows = vec![format!("{} ({total} lines) \u{00b7} /memory edit opens it", path.display())];
                rows.extend(text.lines().take(14).map(|line| format!("  {line}")));
                if total > 14 {
                    rows.push(format!("  +{} more lines", total - 14));
                }
                self.say_lines(rows);
            }
            _ => self.say(format!("no AGENTS.md at {} yet \u{00b7} save notes with # or run /init", path.display())),
        }
        Ok(())
    }

    fn cmd_config(&self) {
        let base = self
            .cfg
            .providers
            .iter()
            .find(|p| p.name == self.cfg.provider)
            .and_then(|p| p.base_url.clone())
            .unwrap_or_else(|| "-".into());
        self.say_lines(vec![
            format!("provider    {} ({})", self.cfg.provider, base),
            format!("model       {}", self.cfg.model),
            format!("agents      {}", self.cfg.agents),
            format!("approvals   {}", self.cfg.tools.approval),
            format!("zero cost   {}", self.cfg.zero_cost),
            format!(
                "budget      {} steps/agent \u{00b7} {} inferences \u{00b7} {} tokens",
                self.cfg.budget.max_steps_per_agent, self.cfg.budget.max_inference_calls, self.cfg.budget.max_total_tokens
            ),
            format!("workspace   {}", self.workspace.display()),
            format!("store       {}", self.store.root().display()),
            format!("context     {} chars travel with each goal", self.context_chars),
            format!("verbose     {}", self.verbose),
        ]);
    }

    async fn cmd_status(&mut self, args: &str) -> Result<()> {
        if !args.is_empty() {
            return self.cmd_inspect("status", args).await;
        }
        let mut rows = vec![
            format!("swarm {}", env!("CARGO_PKG_VERSION")),
            format!("session     {} \u{00b7} {} turn(s)", self.session.id, self.session.turns.len()),
            format!("provider    {} \u{00b7} model {}", self.cfg.provider, self.cfg.model),
            format!("agents      {} \u{00b7} approvals {}", self.cfg.agents, self.cfg.tools.approval),
            format!("workspace   {}", self.workspace.display()),
            format!("store       {}", self.store.root().display()),
        ];
        if let Some(goal) = &self.goal {
            rows.push(goal.describe());
        }
        self.say_lines(rows);
        if let Some(run) = self.last_run.clone() {
            self.cmd_inspect("status", &run.to_string()).await?;
        }
        Ok(())
    }

    fn cmd_cost(&self) {
        let totals = self.session.totals;
        self.say_lines(vec![
            format!("session {} \u{00b7} {} turn(s)", self.session.id, self.session.turns.len()),
            format!(
                "{} tokens \u{00b7} {} inferences \u{00b7} {} tool calls \u{00b7} {}",
                style::tokens(totals.tokens),
                totals.inferences,
                totals.tool_calls,
                style::elapsed(totals.seconds)
            ),
        ]);
    }

    fn cmd_context(&self) {
        let budget = self.context_chars.max(1);
        let used = self.session.render(budget).chars().count();
        let fraction = used as f64 / budget as f64;
        self.say_lines(vec![
            format!("{} {used} of {budget} characters", style::gauge(fraction, 20)),
            format!(
                "{} turn(s) in context{}",
                self.session.turns.len(),
                if self.session.digest.is_some() { " + a digest of earlier ones" } else { "" }
            ),
            "/compact folds older turns into the digest; --context-chars changes the budget".to_string(),
        ]);
    }

    async fn cmd_inspect(&mut self, verb: &str, args: &str) -> Result<()> {
        let id = if args.is_empty() {
            self.last_run
                .as_ref()
                .map(ToString::to_string)
                .ok_or_else(|| anyhow::anyhow!("no run in this session yet; pass a run id"))?
        } else {
            args.to_string()
        };
        let text = commands::inspect_text(&self.store, verb, id).await?;
        self.ui.print(text.lines().map(|line| format!("  {line}")).collect());
        Ok(())
    }

    async fn cmd_runs(&mut self) -> Result<()> {
        let runs = self.store.list_runs().await?;
        if runs.is_empty() {
            self.say("no runs in this store yet");
            return Ok(());
        }
        let rows = runs
            .into_iter()
            .take(10)
            .map(|run| format!("{}  {:<10}  {}", run.id, run.status.to_string(), clip(&run.objective, 60)))
            .collect();
        self.say_lines(rows);
        Ok(())
    }

    fn cmd_history(&self) {
        if self.session.turns.is_empty() {
            self.say("no turns yet");
            return;
        }
        let rows = self
            .session
            .turns
            .iter()
            .enumerate()
            .map(|(index, turn)| {
                format!("{}. [{}] {}  ({})", index + 1, turn.status, clip(&turn.goal, 60), clip(&turn.run_id, 14))
            })
            .collect();
        self.say_lines(rows);
    }

    /// `/diff`: what changed in the workspace, if it is a git checkout.
    async fn cmd_diff(&mut self, args: &str) {
        let stat_only = args.contains("--stat");
        let git = |extra: &[&str]| {
            let mut cmd = tokio::process::Command::new("git");
            cmd.arg("-C")
                .arg(&self.workspace)
                .arg("--no-pager")
                .args(extra)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true);
            async move { tokio::time::timeout(Duration::from_secs(15), cmd.output()).await }
        };
        let inside = git(&["rev-parse", "--is-inside-work-tree"]).await;
        if !matches!(&inside, Ok(Ok(out)) if out.status.success()) {
            self.say("the workspace is not a git repository, so there is no diff to show");
            return;
        }
        let args: Vec<&str> = if stat_only { vec!["diff", "--no-color", "--stat"] } else { vec!["diff", "--no-color"] };
        let Ok(Ok(out)) = git(&args).await else {
            self.say("could not run git diff");
            return;
        };
        let text = crate::ui::text::sanitize(&String::from_utf8_lossy(&out.stdout));
        if text.trim().is_empty() {
            self.say("no uncommitted changes");
            return;
        }
        let lines: Vec<&str> = text.lines().collect();
        let mut rows: Vec<String> = lines
            .iter()
            .take(160)
            .map(|line| {
                let styled = if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") {
                    role(self.colors, "action", line)
                } else if line.starts_with('+') {
                    role(self.colors, "ok", line)
                } else if line.starts_with('-') {
                    role(self.colors, "fail", line)
                } else if line.starts_with("@@") {
                    role(self.colors, "suggest", line)
                } else {
                    (*line).to_string()
                };
                format!("  {styled}")
            })
            .collect();
        if lines.len() > 160 {
            rows.push(role(self.colors, "subtle", &format!("  +{} more lines", lines.len() - 160)));
        }
        self.ui.print(rows);
    }

    /// `/doctor`: the things that most often explain "it does not work".
    async fn cmd_doctor(&mut self) {
        let rows: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        // `Some(true)` passes, `Some(false)` fails; `None` is only advice.
        let grade = |state: Option<bool>, name: &str, detail: String| {
            let mark = match state {
                Some(true) => role(self.colors, "ok", "\u{2713}"),
                Some(false) => role(self.colors, "fail", "\u{2717}"),
                None => role(self.colors, "warn", "!"),
            };
            rows.borrow_mut().push(format!("  {mark} {name} \u{2014} {detail}"));
        };
        let check = |ok: bool, name: &str, detail: String| grade(Some(ok), name, detail);

        let probe = self.workspace.join(".swarm-doctor");
        let writable = std::fs::write(&probe, "x").is_ok();
        let _ = std::fs::remove_file(&probe);
        check(writable, "workspace", format!("{} {}", self.workspace.display(), if writable { "is writable" } else { "is not writable" }));
        check(self.store.root().exists(), "store", self.store.root().display().to_string());

        let provider = self.cfg.providers.iter().find(|p| p.name == self.cfg.provider).cloned();
        match provider {
            None => check(false, "provider", format!("{} is not configured", self.cfg.provider)),
            Some(provider) => match provider.base_url.as_deref().and_then(host_port) {
                Some(target) => {
                    let shown = target.clone();
                    let reachable = tokio::task::spawn_blocking(move || {
                        use std::net::ToSocketAddrs;
                        target
                            .to_socket_addrs()
                            .ok()
                            .and_then(|mut addrs| addrs.next())
                            .is_some_and(|addr| std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(800)).is_ok())
                    })
                    .await
                    .unwrap_or(false);
                    check(
                        reachable,
                        "provider",
                        format!(
                            "{} at {shown} {}",
                            provider.name,
                            if reachable { "is reachable" } else { "did not answer (is the bridge or server running?)" }
                        ),
                    );
                }
                None => check(true, "provider", format!("{} ({} model {})", provider.name, provider.kind, provider.model)),
            },
        }

        let git = tokio::process::Command::new("git")
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
            .await;
        check(git.as_ref().is_ok_and(|out| out.status.success()), "git", "needed by the git tool and /diff".to_string());
        let has_agents = self.workspace.join("AGENTS.md").exists();
        grade(
            has_agents.then_some(true),
            "AGENTS.md",
            if has_agents { "project instructions the agents read".to_string() } else { "none yet; /init writes one, # adds notes".to_string() },
        );
        let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).ok();
        grade(
            editor.is_some().then_some(true),
            "$EDITOR",
            editor.unwrap_or_else(|| "unset, so ctrl+g and /memory edit use vi".to_string()),
        );
        let term = std::env::var("TERM").unwrap_or_default();
        let truecolor = std::env::var("COLORTERM").is_ok_and(|value| value.contains("truecolor") || value.contains("24bit"));
        check(!term.is_empty() && term != "dumb", "terminal", format!("TERM={term} \u{00b7} {}", if truecolor { "truecolor" } else { "256 colours or fewer" }));
        let mut rows = rows.into_inner();
        rows.insert(0, String::new());
        self.ui.print(rows);
    }
}

/// `host:port` from a base URL such as `http://127.0.0.1:8766/v1`.
fn host_port(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split('/').next()?;
    if authority.is_empty() {
        return None;
    }
    if authority.contains(':') {
        Some(authority.to_string())
    } else {
        Some(format!("{authority}:{}", if scheme == "https" { 443 } else { 80 }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_and_port_come_out_of_base_urls() {
        assert_eq!(host_port("http://127.0.0.1:8766"), Some("127.0.0.1:8766".into()));
        assert_eq!(host_port("http://127.0.0.1:8080/v1"), Some("127.0.0.1:8080".into()));
        assert_eq!(host_port("https://api.example.com/v1"), Some("api.example.com:443".into()));
        assert_eq!(host_port("http://localhost"), Some("localhost:80".into()));
        assert_eq!(host_port("not a url"), None);
        assert_eq!(host_port("http://"), None);
    }
}
