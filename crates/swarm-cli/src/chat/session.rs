//! The conversation a chat session carries between goals: what was asked, what
//! came back, and the totals shown by `/cost`.
//!
//! Every goal is its own durable run; the session is the thread of summaries
//! handed to the next run so it can build on earlier work.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::commands::TurnOutcome;

pub const SUMMARY_CHARS: usize = 400;

/// Turns the session loop wrote itself start with this, not with user text.
pub const INTERNAL_PREFIX: &str = "(goal) ";

/// One completed goal in a chat session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub goal: String,
    pub status: String,
    pub summary: String,
    pub run_id: String,
}

/// Session totals shown by `/cost`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    pub tokens: u64,
    pub inferences: u64,
    pub tool_calls: u64,
    pub seconds: f64,
}

impl Totals {
    pub fn add(&mut self, turn: &TurnOutcome) {
        self.tokens += turn.tokens;
        self.inferences += turn.inferences;
        self.tool_calls += turn.tool_calls;
        self.seconds += turn.seconds;
    }
}

/// A chat session: the turns so far, an optional compacted digest, and totals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub turns: Vec<Turn>,
    pub digest: Option<String>,
    pub totals: Totals,
}

impl Session {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into(), turns: Vec::new(), digest: None, totals: Totals::default() }
    }

    pub fn with_id() -> Self {
        Self::new(new_session_id())
    }

    pub fn push(&mut self, turn: Turn) {
        self.turns.push(turn);
    }

    /// Take back the newest turn, whatever it is.
    #[cfg(test)]
    pub fn pop_turn(&mut self) -> Option<Turn> {
        self.turns.pop()
    }

    /// Take back the newest *request*: the goal loop's own continuation turns
    /// (marked `(goal)`) go with the request they belong to, so what comes
    /// back is something the user actually typed.
    pub fn pop_request(&mut self) -> Option<Turn> {
        loop {
            let turn = self.turns.pop()?;
            if !turn.goal.starts_with(INTERNAL_PREFIX) {
                return Some(turn);
            }
        }
    }

    /// The context block handed to the next run. Empty sessions add nothing, so
    /// a first turn is byte-identical to `swarm run`.
    ///
    /// `max_chars` is a hard budget for the whole block (preamble, labels and
    /// trailer included): newest turns are kept, oldest are dropped.
    pub fn render(&self, max_chars: usize) -> String {
        const PREAMBLE: &str = "Conversation so far. Build on it; do not repeat finished work.\n";
        const LABEL: &str = "Turns in this session (oldest first):\n";
        const TRAILER: &str = "\n\nNew request: ";
        if self.turns.is_empty() && self.digest.is_none() {
            return String::new();
        }
        let overhead = PREAMBLE.chars().count() + LABEL.chars().count() + TRAILER.chars().count();
        let budget = max_chars.saturating_sub(overhead);
        // Each turn gets a share of the budget, so even one turn cannot blow it.
        let per_row = (budget / 3).clamp(40, SUMMARY_CHARS);
        let mut used = 0usize;
        let mut kept: Vec<String> = Vec::new();
        for (index, turn) in self.turns.iter().enumerate().rev() {
            let row = format!(
                "{}. [{}] {} -- {}",
                index + 1,
                turn.status,
                clip(&turn.goal, per_row / 2),
                clip(&turn.summary, per_row)
            );
            let cost = row.chars().count() + 1;
            if !kept.is_empty() && used + cost > budget {
                break;
            }
            used += cost;
            kept.push(row);
        }
        kept.reverse();
        let dropped = self.turns.len().saturating_sub(kept.len());
        let mut body = String::new();
        if let Some(digest) = &self.digest {
            let room = budget.saturating_sub(used);
            if room >= 60 {
                body.push_str(&format!("Earlier turns: {}\n", clip(digest, room)));
            }
        }
        if dropped > 0 {
            body.push_str(&format!("({dropped} older turn(s) omitted)\n"));
        }
        format!("{PREAMBLE}{LABEL}{body}{}{TRAILER}", kept.join("\n"))
    }

    /// Fold everything but the newest `keep` turns into the digest.
    pub fn compact(&mut self, keep: usize) -> usize {
        if self.turns.len() <= keep {
            return 0;
        }
        let folded: Vec<Turn> = self.turns.drain(..self.turns.len() - keep).collect();
        let count = folded.len();
        let mut lines: Vec<String> = Vec::new();
        lines.extend(self.digest.clone().map(|old| format!("{old}\n")));
        for turn in &folded {
            lines.push(format!("- [{}] {} -> {}", turn.status, clip(&turn.goal, 80), clip(&turn.summary, 120)));
        }
        self.digest = Some(lines.join("\n"));
        count
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }
}

/// One line, at most `max` characters, with `…` marking a cut.
pub fn clip(text: &str, max: usize) -> String {
    let flat: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    let flat = flat.trim();
    if flat.chars().count() <= max {
        return flat.to_string();
    }
    let head: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{head}\u{2026}")
}

pub fn new_session_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("chat-{nanos:x}-{:x}", SEQ.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(goal: &str, summary: &str) -> Turn {
        Turn {
            goal: goal.to_string(),
            status: "completed".to_string(),
            summary: summary.to_string(),
            run_id: "0".repeat(26),
        }
    }

    #[test]
    fn empty_session_adds_no_context() {
        assert_eq!(Session::new("s").render(4000), "");
    }

    #[test]
    fn context_lists_turns_oldest_first() {
        let mut session = Session::new("s");
        session.push(turn("first", "one"));
        session.push(turn("second", "two"));
        let text = session.render(4000);
        assert!(text.starts_with("Conversation so far"));
        assert!(text.find("first") < text.find("second"), "{text}");
        assert!(text.ends_with("New request: "));
        assert!(text.contains("[completed]"));
    }

    #[test]
    fn context_trims_oldest_turns_first() {
        let mut session = Session::new("s");
        for index in 0..30 {
            session.push(turn(&format!("goal {index}"), &"x".repeat(60)));
        }
        let text = session.render(400);
        assert!(text.chars().count() <= 420, "{} chars", text.chars().count());
        assert!(text.contains("goal 29"), "newest turn must survive");
        assert!(!text.contains("goal 0\n"), "oldest turn must be trimmed");
    }

    #[test]
    fn compact_folds_old_turns_into_a_digest() {
        let mut session = Session::new("s");
        for index in 0..6 {
            session.push(turn(&format!("goal {index}"), "done"));
        }
        assert_eq!(session.compact(3), 3);
        assert_eq!(session.turns.len(), 3);
        assert!(session.digest.as_deref().unwrap().contains("goal 0"));
        assert!(session.render(4000).contains("Earlier turns:"));
        assert_eq!(session.compact(3), 0, "nothing left to fold");
    }

    #[test]
    fn session_round_trips_through_the_store_format() {
        let mut session = Session::new("s-7");
        session.push(turn("goal", "summary"));
        session.totals.tokens = 42;
        let decoded = Session::from_json(&session.to_json()).unwrap();
        assert_eq!(decoded, session);
        assert!(Session::from_json("not json").is_none());
    }

    #[test]
    fn pop_turn_takes_back_the_newest() {
        let mut session = Session::new("s");
        session.push(turn("a", "1"));
        session.push(turn("b", "2"));
        assert_eq!(session.pop_turn().unwrap().goal, "b");
        assert_eq!(session.pop_turn().unwrap().goal, "a");
        assert!(session.pop_turn().is_none());
    }

    #[test]
    fn pop_request_skips_the_goal_loops_own_turns() {
        let mut session = Session::new("s");
        session.push(turn("fix the build", "ok"));
        session.push(turn("/goal make tests pass", "working"));
        session.push(turn("(goal) 2 tests fail", "working"));
        session.push(turn("(goal) 1 test fails", "working"));
        assert_eq!(session.pop_request().unwrap().goal, "/goal make tests pass");
        assert_eq!(session.turns.len(), 1, "the continuations went with it");
        assert_eq!(session.pop_request().unwrap().goal, "fix the build");
        assert!(session.pop_request().is_none());
    }

    #[test]
    fn clip_flattens_and_marks_long_text() {
        assert_eq!(clip("one\ntwo", 20), "onetwo");
        assert_eq!(clip("abcdefghij", 5), "abcd\u{2026}");
        assert_eq!(clip("  padded  ", 20), "padded");
    }

    #[test]
    fn session_ids_are_unique() {
        assert_ne!(new_session_id(), new_session_id());
    }
}
