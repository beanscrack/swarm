//! `/goal`: set a completion condition and keep working across turns until a
//! judge says it is met.
//!
//! This is the loop Claude Code and Hermes both have. The pieces here are pure:
//! parsing the command, the judge's prompt and verdict, and the state machine
//! that decides after each turn whether to continue, pause or finish. The
//! session loop supplies the model call and runs the turns.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Turns a goal may use before it pauses for a human decision.
pub const DEFAULT_MAX_TURNS: u32 = 20;
/// Consecutive unusable judge replies before the goal pauses.
const MAX_BAD_REPLIES: u32 = 3;
const GOAL_CHARS: usize = 2000;
const REPLY_CHARS: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GoalStatus {
    Active,
    Paused,
    Done,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Goal {
    pub text: String,
    pub status: GoalStatus,
    pub turns_used: u32,
    pub max_turns: u32,
    pub bad_replies: u32,
    /// Why it paused or finished.
    pub note: String,
    /// Extra conditions added with `/subgoal`; the judge wants all of them.
    #[serde(default)]
    pub criteria: Vec<String>,
}

/// The judge's reading of the latest turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Done(String),
    Continue(String),
    Blocked(String),
}

/// What to do after a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The goal is met.
    Finished(String),
    /// Stopped for the user: blocked, out of turns, or a broken judge.
    Paused(String),
    /// Run another turn.
    Continue { prompt: String, reason: String },
}

/// What the user typed after `/goal`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Set(String),
    Status,
    Pause,
    Resume,
    Clear,
}

/// `/goal` alone shows status; a bare control word acts; anything else is the
/// goal. `-- text` forces text that happens to start with a control word.
pub fn parse_command(args: &str) -> Command {
    let args = args.trim();
    if let Some(text) = args.strip_prefix("--") {
        return Command::Set(text.trim().to_string());
    }
    match args.to_ascii_lowercase().as_str() {
        "" | "status" | "show" => Command::Status,
        "pause" => Command::Pause,
        "resume" | "continue" | "unpause" => Command::Resume,
        "clear" | "stop" | "done" | "off" => Command::Clear,
        _ => Command::Set(args.to_string()),
    }
}

impl Goal {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.trim().to_string(),
            status: GoalStatus::Active,
            turns_used: 0,
            max_turns: DEFAULT_MAX_TURNS,
            bad_replies: 0,
            note: String::new(),
            criteria: Vec::new(),
        }
    }

    /// The goal and its `/subgoal` conditions, as one block of prompt text.
    pub fn statement(&self) -> String {
        if self.criteria.is_empty() {
            return self.text.clone();
        }
        let list: Vec<String> =
            self.criteria.iter().enumerate().map(|(index, text)| format!("{}. {text}", index + 1)).collect();
        format!("{}\n\nIt is only achieved when all of these also hold:\n{}", self.text, list.join("\n"))
    }

    pub fn is_active(&self) -> bool {
        self.status == GoalStatus::Active
    }

    /// The footer chip: `⊙ goal 3/20` while active, `⏸ goal paused` while not.
    pub fn chip(&self) -> Option<String> {
        match self.status {
            GoalStatus::Active => Some(format!("\u{2299} goal {}/{}", self.turns_used, self.max_turns)),
            GoalStatus::Paused => Some("\u{23f8} goal paused".to_string()),
            GoalStatus::Done => None,
        }
    }

    /// A line for `/goal` and `/status`.
    pub fn describe(&self) -> String {
        let state = match self.status {
            GoalStatus::Active => format!("active, {}/{} turns", self.turns_used, self.max_turns),
            GoalStatus::Paused => format!("paused, {}/{} turns", self.turns_used, self.max_turns),
            GoalStatus::Done => "achieved".to_string(),
        };
        let mut line = format!("\u{2299} Goal ({state}): {}", self.text);
        if !self.note.is_empty() {
            line.push_str(&format!(" \u{2014} {}", self.note));
        }
        for (index, text) in self.criteria.iter().enumerate() {
            line.push_str(&format!("\n    {}. {text}", index + 1));
        }
        line
    }

    pub fn pause(&mut self, note: &str) {
        self.status = GoalStatus::Paused;
        self.note = note.to_string();
    }

    /// Back to work with a fresh turn budget.
    pub fn resume(&mut self) {
        self.status = GoalStatus::Active;
        self.turns_used = 0;
        self.bad_replies = 0;
        self.note.clear();
    }

    /// The request that starts the goal.
    pub fn kickoff_prompt(&self) -> String {
        format!(
            "{}\n\nWork until this is completely achieved. Check your work as you go, and finish \
             with a short summary of what is now true.",
            self.statement()
        )
    }

    /// The request that picks a paused goal back up.
    pub fn resume_prompt(&self) -> String {
        format!(
            "Continue working toward the goal: {}\n\nWork had been paused. Take the next step; when \
             everything is done, verify it and summarise.",
            self.statement()
        )
    }

    fn continuation_prompt(&self, reason: &str) -> String {
        let reason = if reason.trim().is_empty() { "it is not finished yet" } else { reason.trim() };
        format!(
            "Continue working toward the goal: {}\n\nThe last check found that {}. Take the next \
             step; when everything is done, verify it and summarise.",
            self.statement(), reason
        )
    }

    /// Account for a finished turn and decide what comes next. `verdict` is the
    /// judge's reading, or `None` when it could not be obtained or understood.
    pub fn advance(&mut self, verdict: Option<Verdict>) -> Step {
        self.turns_used += 1;
        match verdict {
            Some(Verdict::Done(reason)) => {
                self.status = GoalStatus::Done;
                self.note = reason.clone();
                Step::Finished(reason)
            }
            Some(Verdict::Blocked(reason)) => {
                self.bad_replies = 0;
                self.pause(&reason);
                Step::Paused(format!("judged unachievable as things stand: {reason}"))
            }
            Some(Verdict::Continue(reason)) => {
                self.bad_replies = 0;
                self.next_or_pause(reason)
            }
            None => {
                self.bad_replies += 1;
                if self.bad_replies >= MAX_BAD_REPLIES {
                    let note = "the judge gave no usable answer three times in a row";
                    self.pause(note);
                    return Step::Paused(note.to_string());
                }
                self.next_or_pause("the judge could not be reached, so it is assumed unfinished".into())
            }
        }
    }

    fn next_or_pause(&mut self, reason: String) -> Step {
        if self.turns_used >= self.max_turns {
            let note = format!("{}/{} turns used", self.turns_used, self.max_turns);
            self.pause(&note);
            return Step::Paused(note);
        }
        Step::Continue { prompt: self.continuation_prompt(&reason), reason }
    }
}

/// Most extra conditions a goal may carry, and the longest one.
pub const MAX_CRITERIA: usize = 10;
pub const CRITERION_CHARS: usize = 300;

/// What the user typed after `/subgoal`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubCommand {
    List,
    Add(String),
    /// 1-based position, as the list shows it.
    Remove(usize),
    Clear,
}

/// `/subgoal` alone lists; `rm <n>` and `clear` edit; anything else is a new
/// condition.
pub fn parse_subcommand(args: &str) -> SubCommand {
    let args = args.trim();
    let mut words = args.splitn(2, char::is_whitespace);
    let first = words.next().unwrap_or("").to_ascii_lowercase();
    let rest = words.next().unwrap_or("").trim();
    match (first.as_str(), rest.parse::<usize>()) {
        ("", _) | ("list", _) | ("show", _) => SubCommand::List,
        ("clear", _) if rest.is_empty() => SubCommand::Clear,
        ("rm" | "remove" | "del" | "delete", Ok(n)) => SubCommand::Remove(n),
        _ => SubCommand::Add(args.to_string()),
    }
}

/// The judge's system and user messages for one turn. It sees the goal, the
/// assistant's last reply, and a report of what the turn did (the reply alone
/// can claim more than the work shows).
pub fn judge_messages(goal: &Goal, reply: &str, work: &str) -> (String, String) {
    let system = "You are a strict reviewer deciding whether a goal has been fully achieved. \
        Answer with exactly one line of JSON and nothing else: \
        {\"verdict\": \"done\" | \"continue\" | \"blocked\", \"reason\": \"one short sentence\"}. \
        Say \"done\" only when the assistant's reply and the work report together show the goal \
        (and every extra condition listed with it) is complete and verified. Say \"blocked\" only when it cannot proceed without something from \
        the user. Otherwise say \"continue\"."
        .to_string();
    let user = format!(
        "Goal:\n{}\n\nWhat the last turn did: {}\n\nAssistant's last reply:\n{}",
        cut(&goal.statement(), GOAL_CHARS),
        if work.is_empty() { "nothing recorded" } else { work },
        cut(reply, REPLY_CHARS),
    );
    (system, user)
}

fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    // Keep the end: conclusions come last.
    let tail: Vec<char> = text.chars().rev().take(max).collect();
    format!("\u{2026}{}", tail.into_iter().rev().collect::<String>())
}

/// Read the judge's reply: the first JSON object in it, tolerating a code fence
/// or chatter around it.
pub fn parse_verdict(reply: &str) -> Option<Verdict> {
    let start = reply.find('{')?;
    let end = reply.rfind('}')?;
    if end < start {
        return None;
    }
    let value: Value = serde_json::from_str(&reply[start..=end]).ok()?;
    let reason = value.get("reason").and_then(Value::as_str).unwrap_or("").trim().to_string();
    match value.get("verdict")?.as_str()?.to_ascii_lowercase().as_str() {
        "done" | "complete" | "achieved" => Some(Verdict::Done(reason)),
        "continue" | "wait" | "not_done" => Some(Verdict::Continue(reason)),
        "blocked" | "unachievable" => Some(Verdict::Blocked(reason)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subgoals_are_parsed_and_folded_into_every_prompt() {
        assert_eq!(parse_subcommand(""), SubCommand::List);
        assert_eq!(parse_subcommand("list"), SubCommand::List);
        assert_eq!(parse_subcommand("rm 2"), SubCommand::Remove(2));
        assert_eq!(parse_subcommand("clear"), SubCommand::Clear);
        assert_eq!(parse_subcommand("clear the cache too"), SubCommand::Add("clear the cache too".into()));
        assert_eq!(parse_subcommand("rm the old files"), SubCommand::Add("rm the old files".into()));
        assert_eq!(parse_subcommand("tests are green"), SubCommand::Add("tests are green".into()));

        let mut goal = Goal::new("ship the parser");
        assert_eq!(goal.statement(), "ship the parser", "no criteria, no change to the prompts");
        goal.criteria = vec!["tests are green".into(), "docs mention it".into()];
        for prompt in [goal.kickoff_prompt(), goal.resume_prompt(), goal.continuation_prompt("docs are missing")] {
            assert!(prompt.contains("1. tests are green") && prompt.contains("2. docs mention it"), "{prompt}");
        }
        let (_, user) = judge_messages(&goal, "done!", "edited 1 file");
        assert!(user.contains("2. docs mention it"), "the judge sees the conditions: {user}");
        assert!(goal.describe().contains("1. tests are green"));
        // An old saved goal with no `criteria` field still loads.
        let old: Goal = serde_json::from_str(
            r#"{"text":"x","status":"Active","turns_used":1,"max_turns":20,"bad_replies":0,"note":""}"#,
        )
        .unwrap();
        assert!(old.criteria.is_empty());
    }

    #[test]
    fn the_command_forms() {
        assert_eq!(parse_command(""), Command::Status);
        assert_eq!(parse_command("status"), Command::Status);
        assert_eq!(parse_command("  Pause "), Command::Pause);
        assert_eq!(parse_command("resume"), Command::Resume);
        assert_eq!(parse_command("clear"), Command::Clear);
        assert_eq!(parse_command("stop"), Command::Clear);
        assert_eq!(parse_command("make the tests pass"), Command::Set("make the tests pass".into()));
        assert_eq!(
            parse_command("clear the build cache"),
            Command::Set("clear the build cache".into()),
            "a control word inside a sentence is just text"
        );
        assert_eq!(parse_command("-- clear"), Command::Set("clear".into()), "-- forces text");
    }

    #[test]
    fn verdicts_parse_from_noisy_replies() {
        assert_eq!(
            parse_verdict("{\"verdict\": \"done\", \"reason\": \"tests pass\"}"),
            Some(Verdict::Done("tests pass".into()))
        );
        assert_eq!(
            parse_verdict("```json\n{\"verdict\":\"continue\",\"reason\":\"2 tests fail\"}\n```"),
            Some(Verdict::Continue("2 tests fail".into()))
        );
        assert_eq!(
            parse_verdict("Here you go: {\"verdict\": \"BLOCKED\", \"reason\": \"needs an API key\"} thanks"),
            Some(Verdict::Blocked("needs an API key".into()))
        );
        assert_eq!(parse_verdict("{\"verdict\": \"done\"}"), Some(Verdict::Done(String::new())));
        assert_eq!(parse_verdict("{\"verdict\": \"wait\", \"reason\": \"x\"}"), Some(Verdict::Continue("x".into())));
        assert_eq!(parse_verdict("no json here"), None);
        assert_eq!(parse_verdict("{\"verdict\": \"maybe\"}"), None);
        assert_eq!(parse_verdict("{not json}"), None);
        assert_eq!(parse_verdict("} backwards {"), None);
        assert_eq!(parse_verdict("{\"reason\": \"no verdict key\"}"), None);
    }

    #[test]
    fn a_done_verdict_finishes_the_goal() {
        let mut goal = Goal::new("ship it");
        let step = goal.advance(Some(Verdict::Done("shipped".into())));
        assert_eq!(step, Step::Finished("shipped".into()));
        assert_eq!(goal.status, GoalStatus::Done);
        assert_eq!(goal.turns_used, 1);
        assert_eq!(goal.chip(), None, "a finished goal leaves the footer");
    }

    #[test]
    fn continue_builds_a_prompt_that_carries_the_reason() {
        let mut goal = Goal::new("fix the build");
        match goal.advance(Some(Verdict::Continue("one test still fails".into()))) {
            Step::Continue { prompt, reason } => {
                assert!(prompt.contains("fix the build") && prompt.contains("one test still fails"), "{prompt}");
                assert_eq!(reason, "one test still fails");
            }
            other => panic!("{other:?}"),
        }
        assert!(goal.is_active());
        assert_eq!(goal.chip().as_deref(), Some("\u{2299} goal 1/20"));
    }

    #[test]
    fn blocked_pauses_with_the_reason() {
        let mut goal = Goal::new("deploy");
        let step = goal.advance(Some(Verdict::Blocked("needs credentials".into())));
        assert!(matches!(&step, Step::Paused(text) if text.contains("needs credentials")), "{step:?}");
        assert_eq!(goal.status, GoalStatus::Paused);
        assert_eq!(goal.chip().as_deref(), Some("\u{23f8} goal paused"));
    }

    #[test]
    fn running_out_of_turns_pauses_instead_of_looping_forever() {
        let mut goal = Goal::new("endless");
        goal.max_turns = 3;
        let mut last = Step::Finished(String::new());
        for _ in 0..3 {
            last = goal.advance(Some(Verdict::Continue("more".into())));
        }
        assert_eq!(last, Step::Paused("3/3 turns used".into()));
        assert_eq!(goal.status, GoalStatus::Paused);
    }

    #[test]
    fn done_wins_even_on_the_last_turn() {
        let mut goal = Goal::new("g");
        goal.max_turns = 1;
        assert!(matches!(goal.advance(Some(Verdict::Done("ok".into()))), Step::Finished(_)));
    }

    #[test]
    fn an_unreadable_judge_fails_open_then_pauses() {
        let mut goal = Goal::new("g");
        assert!(matches!(goal.advance(None), Step::Continue { .. }), "one bad reply: keep going");
        assert!(matches!(goal.advance(None), Step::Continue { .. }));
        let third = goal.advance(None);
        assert!(matches!(&third, Step::Paused(text) if text.contains("three times")), "{third:?}");
        assert_eq!(goal.status, GoalStatus::Paused);
    }

    #[test]
    fn a_good_reply_resets_the_bad_reply_count() {
        let mut goal = Goal::new("g");
        goal.advance(None);
        goal.advance(None);
        goal.advance(Some(Verdict::Continue("fine".into())));
        assert_eq!(goal.bad_replies, 0);
        assert!(matches!(goal.advance(None), Step::Continue { .. }));
    }

    #[test]
    fn resume_restores_the_budget() {
        let mut goal = Goal::new("g");
        goal.max_turns = 2;
        goal.advance(Some(Verdict::Continue("x".into())));
        goal.advance(Some(Verdict::Continue("x".into())));
        assert_eq!(goal.status, GoalStatus::Paused);
        goal.resume();
        assert!(goal.is_active());
        assert_eq!(goal.turns_used, 0);
        assert!(goal.note.is_empty());
    }

    #[test]
    fn the_judge_sees_the_goal_the_work_and_the_end_of_a_long_reply() {
        let goal = Goal::new("make tests pass");
        let reply = format!("{}THE CONCLUSION", "padding ".repeat(2000));
        let (system, user) = judge_messages(&goal, &reply, "edited 2 files, ran 3 commands");
        assert!(system.contains("\"verdict\""));
        assert!(user.contains("make tests pass"));
        assert!(user.contains("edited 2 files, ran 3 commands"));
        assert!(user.contains("THE CONCLUSION"), "the tail of a long reply is what matters");
        assert!(user.chars().count() < 4300 + 400, "{} chars", user.chars().count());
        let (_, none) = judge_messages(&goal, "r", "");
        assert!(none.contains("nothing recorded"));
    }

    #[test]
    fn descriptions_and_persistence() {
        let mut goal = Goal::new("  tidy up  ");
        assert_eq!(goal.text, "tidy up");
        assert!(goal.describe().contains("active, 0/20 turns"), "{}", goal.describe());
        goal.pause("budget");
        assert!(goal.describe().contains("paused") && goal.describe().contains("budget"));
        let json = serde_json::to_string(&goal).unwrap();
        assert_eq!(serde_json::from_str::<Goal>(&json).unwrap(), goal);
        assert!(goal.kickoff_prompt().starts_with("tidy up"));
    }
}
