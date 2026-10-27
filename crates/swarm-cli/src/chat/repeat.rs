//! `/loop <interval> <prompt>`: run a prompt again and again while the session
//! is idle, as Claude Code's `/loop` does.
//!
//! The loop lives in this session only (it is not saved), runs one prompt at a
//! time and never while you are typing a turn's worth of work: the timer is
//! only looked at when the prompt is idle, and anything you type goes first.
//! It stops by itself when a run fails or is interrupted, or after
//! [`MAX_RUNS`], so an unattended loop cannot run away.

use std::time::Duration;

use tokio::time::Instant;

use super::{Chat, Flow};
use crate::style;
use crate::ui::{Ui, echo_rows};

pub(super) const MIN_INTERVAL: Duration = Duration::from_secs(10);
const MAX_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
pub(super) const MAX_RUNS: u32 = 50;

/// A loop that is running.
pub(crate) struct Loop {
    pub every: Duration,
    pub prompt: String,
    pub next: Instant,
    pub runs: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command {
    Status,
    Stop,
    Start { every: Duration, prompt: String },
    /// Why the arguments were not understood.
    Invalid(&'static str),
}

/// `30s`, `5m`, `2h`; a bare number is minutes.
pub(super) fn parse_interval(word: &str) -> Option<Duration> {
    let word = word.trim().to_ascii_lowercase();
    let split = word.find(|ch: char| !ch.is_ascii_digit()).unwrap_or(word.len());
    let (digits, unit) = word.split_at(split);
    let n: u64 = digits.parse().ok().filter(|n| *n > 0)?;
    let seconds = match unit {
        "s" | "sec" | "secs" => n,
        "" | "m" | "min" | "mins" => n.checked_mul(60)?,
        "h" | "hr" | "hrs" => n.checked_mul(3600)?,
        _ => return None,
    };
    Some(Duration::from_secs(seconds))
}

pub(super) fn parse(args: &str) -> Command {
    let args = args.trim();
    match args.to_ascii_lowercase().as_str() {
        "" | "status" => return Command::Status,
        "stop" | "off" | "clear" | "cancel" => return Command::Stop,
        _ => {}
    }
    let (first, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let Some(every) = parse_interval(first) else {
        return Command::Invalid("start with how often: 30s, 5m or 2h");
    };
    if every < MIN_INTERVAL {
        return Command::Invalid("the shortest interval is 10s");
    }
    if every > MAX_INTERVAL {
        return Command::Invalid("the longest interval is 24h");
    }
    let prompt = rest.trim();
    if prompt.is_empty() {
        return Command::Invalid("say what to run, e.g. /loop 5m check the build");
    }
    Command::Start { every, prompt: prompt.to_string() }
}

pub(super) fn describe(every: Duration) -> String {
    let secs = every.as_secs();
    match secs {
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

impl Chat {
    /// The footer chip while a loop is running.
    pub(crate) fn loop_chip(&self) -> Option<String> {
        self.loop_job.as_ref().map(|job| format!("\u{21bb} loop {}", describe(job.every)))
    }

    pub(super) fn cmd_loop(&mut self, args: &str) {
        match parse(args) {
            Command::Status => match &self.loop_job {
                Some(job) => {
                    let wait = job.next.saturating_duration_since(Instant::now()).as_secs();
                    self.say(format!(
                        "\u{21bb} every {}: {} \u{00b7} run {} of {MAX_RUNS}, next in {wait}s \u{00b7} /loop stop",
                        describe(job.every),
                        super::clip(&job.prompt, 60),
                        job.runs
                    ));
                }
                None => self.say("no loop \u{00b7} /loop 5m <prompt> runs a prompt every 5 minutes while the session is idle"),
            },
            Command::Stop => {
                let had = self.loop_job.take().is_some();
                self.say(if had { "loop stopped" } else { "no loop is running" });
                self.push_info();
            }
            Command::Invalid(why) => self.say(format!("/loop: {why}")),
            Command::Start { every, prompt } => {
                if !self.ui.is_live() {
                    self.say("/loop needs the interactive prompt");
                    return;
                }
                self.say(format!(
                    "\u{21bb} running \"{}\" every {} while idle (up to {MAX_RUNS} times) \u{00b7} /loop stop",
                    super::clip(&prompt, 50),
                    describe(every)
                ));
                self.loop_job = Some(Loop { every, prompt, next: Instant::now() + every, runs: 0 });
                self.push_info();
            }
        }
    }

    /// The timer fired while idle: run the prompt as if it had been typed.
    pub(super) async fn run_loop(&mut self, ui: &mut Ui) -> Flow {
        let Some(job) = self.loop_job.as_mut() else { return Flow::Continue };
        job.runs += 1;
        let (prompt, runs) = (job.prompt.clone(), job.runs);
        self.say(format!("\u{21bb} /loop run {runs}/{MAX_RUNS}"));
        self.ui.print(echo_rows(self.colors, style::term_width(), &prompt));
        let before = self.session.turns.len();
        let flow = self.submit(ui, prompt).await;
        let finished = self.session.turns.len() > before
            && self.session.turns.last().is_some_and(|turn| turn.status == "completed");
        if let Some(job) = self.loop_job.as_mut() {
            if !finished {
                self.say("loop stopped: that run did not finish");
                self.loop_job = None;
            } else if job.runs >= MAX_RUNS {
                self.say(format!("loop stopped after {MAX_RUNS} runs"));
                self.loop_job = None;
            } else {
                job.next = Instant::now() + job.every;
            }
        }
        self.push_info();
        flow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_have_units_and_limits() {
        assert_eq!(parse_interval("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_interval("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_interval("2H"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_interval("10"), Some(Duration::from_secs(600)), "a bare number is minutes");
        for bad in ["", "0m", "m", "5x", "-5m", "1.5h"] {
            assert_eq!(parse_interval(bad), None, "{bad:?}");
        }
        assert_eq!(parse_interval("99999999999999999999h"), None, "overflow is not a loop");
    }

    #[test]
    fn the_command_forms() {
        assert_eq!(parse(""), Command::Status);
        assert_eq!(parse("stop"), Command::Stop);
        assert_eq!(
            parse("5m check the build"),
            Command::Start { every: Duration::from_secs(300), prompt: "check the build".into() }
        );
        assert_eq!(
            parse("1h /review"),
            Command::Start { every: Duration::from_secs(3600), prompt: "/review".into() },
            "the prompt may be a command"
        );
        assert!(matches!(parse("5s too fast"), Command::Invalid(why) if why.contains("10s")));
        assert!(matches!(parse("48h too slow"), Command::Invalid(why) if why.contains("24h")));
        assert!(matches!(parse("5m"), Command::Invalid(why) if why.contains("what to run")));
        assert!(matches!(parse("check the build"), Command::Invalid(why) if why.contains("how often")));
    }

    #[test]
    fn intervals_read_back_the_way_they_were_typed() {
        assert_eq!(describe(Duration::from_secs(90)), "90s");
        assert_eq!(describe(Duration::from_secs(300)), "5m");
        assert_eq!(describe(Duration::from_secs(7200)), "2h");
    }
}
