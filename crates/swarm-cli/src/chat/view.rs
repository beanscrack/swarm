//! Text the session prints: the welcome box, `/help`, the per-turn footer, the
//! session export and the clipboard escape. All pure, so all tested.

use std::fmt::Write as _;

use super::session::{Session, clip};
use crate::commands::TurnOutcome;
use crate::style::{self, Color, role};
use crate::ui::registry::{self, Group};
use crate::ui::text::{pad_right, truncate};

/// What the welcome box shows.
pub struct Welcome<'a> {
    pub workspace: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub agents: u32,
    pub approvals: &'a str,
    pub session_id: &'a str,
    /// Turns carried over from a stored session; 0 for a fresh one.
    pub resumed_turns: usize,
}

/// Claude Code's welcome box, then a tip.
pub fn welcome(colors: Color, cols: usize, info: &Welcome, tip: &str) -> Vec<String> {
    let total = cols.clamp(30, 72);
    let inner = total - 4;
    let edge = |text: &str| role(colors, "bullet", text);
    let row = |content: &str| {
        format!("{} {} {}", edge("\u{2502}"), pad_right(&truncate(content, inner), inner), edge("\u{2502}"))
    };
    let dim = |text: &str| role(colors, "subtle", text);
    let agents = format!("{} agent{}", info.agents, if info.agents == 1 { "" } else { "s" });
    let mut out = vec![edge(&format!("\u{256d}{}\u{256e}", "\u{2500}".repeat(total - 2)))];
    out.push(row(&format!(
        "{} {}",
        role(colors, "bullet", "\u{273b}"),
        role(colors, "action", "Welcome to swarm!")
    )));
    out.push(row(""));
    out.push(row(&format!("  {}", dim("/help for help, /status for your current setup"))));
    out.push(row(""));
    out.push(row(&format!("  {}", dim(&format!("cwd: {}", info.workspace)))));
    out.push(row(&format!(
        "  {}",
        dim(&format!("{} \u{00b7} {} \u{00b7} {} \u{00b7} approvals {}", info.provider, info.model, agents, info.approvals))
    )));
    if info.resumed_turns > 0 {
        out.push(row(&format!(
            "  {}",
            dim(&format!("session {} \u{00b7} {} earlier turn(s)", clip(info.session_id, 24), info.resumed_turns))
        )));
    }
    out.push(edge(&format!("\u{2570}{}\u{256f}", "\u{2500}".repeat(total - 2))));
    out.push(String::new());
    out.push(format!(" {} {}", dim("\u{203b} Tip:"), dim(tip)));
    out
}

const KEYS: &[(&str, &str)] = &[
    ("enter", "send"),
    ("shift+enter / ctrl+j / \\ enter", "newline"),
    ("esc", "stop the running turn (twice clears the prompt)"),
    ("ctrl+c", "clear the prompt, stop a turn, press twice to exit"),
    ("ctrl+d", "exit on an empty prompt (press twice)"),
    ("shift+tab", "cycle permission mode"),
    ("ctrl+r", "search prompt history"),
    ("ctrl+o", "toggle transcript detail"),
    ("ctrl+g", "edit the prompt in $EDITOR"),
    ("ctrl+l", "clear the screen"),
    ("ctrl+z", "suspend (fg to resume)"),
    ("tab", "complete a /command or @file; queues text while running"),
    ("@path  !cmd  #note", "attach a file / run a shell command / save to AGENTS.md"),
];

/// `/help`: commands by group, then keys. `filter` narrows commands by name or
/// description.
pub fn help(colors: Color, filter: Option<&str>) -> Vec<String> {
    let filter = filter.map(|text| text.trim().trim_start_matches('/').to_lowercase()).filter(|text| !text.is_empty());
    let dim = |text: &str| role(colors, "subtle", text);
    let mut out = Vec::new();
    let mut shown = 0;
    for group in [Group::Session, Group::Run, Group::Setup, Group::Info] {
        let specs: Vec<_> = registry::COMMANDS
            .iter()
            .filter(|spec| spec.group == group)
            .filter(|spec| match &filter {
                Some(needle) => {
                    spec.name.contains(needle.as_str())
                        || spec.desc.to_lowercase().contains(needle.as_str())
                        || spec.aliases.iter().any(|alias| alias.contains(needle.as_str()))
                }
                None => true,
            })
            .collect();
        if specs.is_empty() {
            continue;
        }
        out.push(format!("  {}", role(colors, "action", group.title())));
        for spec in specs {
            shown += 1;
            let usage = if spec.args.is_empty() {
                format!("/{}", spec.name)
            } else {
                format!("/{} {}", spec.name, spec.args)
            };
            out.push(format!("    {}  {}", role(colors, "select", &pad_right(&usage, 38)), dim(spec.desc)));
        }
        out.push(String::new());
    }
    if let (0, Some(needle)) = (shown, &filter) {
        return vec![dim(&format!("  no command matches \"{needle}\" \u{00b7} /help lists them all"))];
    }
    if filter.is_none() {
        out.push(format!("  {}", role(colors, "action", "Keys")));
        for (keys, what) in KEYS {
            out.push(format!("    {}  {}", role(colors, "select", &pad_right(keys, 30)), dim(what)));
        }
    } else if out.last().is_some_and(String::is_empty) {
        out.pop();
    }
    out
}

/// The line that closes a turn, and the notices that follow it.
///
/// `✻ Cogitated for 12.4s · edited 2 files +18 -3 · ran 3 commands · 2.7k tokens`
/// keeps Claude Code's flourish and adds what Hermes reports about the work.
pub fn turn_footer(colors: Color, outcome: &TurnOutcome) -> Vec<String> {
    let mut parts = vec![format!(
        "{} for {}",
        style::past_verb((outcome.seconds * 10.0) as u64 ^ outcome.tokens),
        style::elapsed(outcome.seconds)
    )];
    parts.extend(outcome.stats.segments());
    if outcome.tokens > 0 {
        parts.push(format!("{} tokens", style::tokens(outcome.tokens)));
    }
    let mut out = Vec::new();
    if outcome.succeeded() {
        out.push(role(colors, "subtle", &format!("\u{273b} {}", parts.join(" \u{00b7} "))));
    } else {
        let why = outcome.error.as_deref().map(|error| format!(": {}", clip(error, 120))).unwrap_or_default();
        out.push(role(
            colors,
            "fail",
            &format!("\u{2717} Run {} after {}{why}", outcome.status, style::elapsed(outcome.seconds)),
        ));
    }
    out.extend(outcome.stats.failure_rows(colors));
    out
}

/// The session as a markdown document (`/export`).
pub fn export_markdown(session: &Session) -> String {
    let mut out = format!("# swarm session {}\n\n", session.id);
    let _ = writeln!(
        out,
        "{} turn(s) \u{00b7} {} tokens \u{00b7} {:.1}s\n",
        session.turns.len(),
        session.totals.tokens,
        session.totals.seconds
    );
    if let Some(digest) = &session.digest {
        let _ = writeln!(out, "## Earlier turns (compacted)\n\n{digest}\n");
    }
    for (index, turn) in session.turns.iter().enumerate() {
        let _ = writeln!(out, "## {}. {}\n", index + 1, clip(&turn.goal, 80));
        let _ = writeln!(out, "**Request**\n\n{}\n", turn.goal.trim());
        let _ = writeln!(out, "**Result** ({}, run `{}`)\n\n{}\n", turn.status, turn.run_id, turn.summary.trim());
    }
    out
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// The OSC 52 sequence that puts `text` on the clipboard of the terminal the
/// user is looking at, even over SSH. tmux needs it wrapped to pass through.
pub fn osc52(text: &str, in_tmux: bool) -> String {
    let osc = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    if in_tmux {
        format!("\x1bPtmux;{}\x1b\\", osc.replace('\x1b', "\x1b\x1b"))
    } else {
        osc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::session::Turn;
    use crate::transcript::{FailedEdit, TurnStats};
    use crate::ui::text::{strip_ansi, width};
    use swarm_core::{RunId, RunStatus};

    fn outcome(status: RunStatus) -> TurnOutcome {
        TurnOutcome {
            run_id: RunId::new(),
            status,
            summary: String::new(),
            error: None,
            tasks: 1,
            tokens: 2_700,
            inferences: 2,
            tool_calls: 3,
            seconds: 12.4,
            stats: TurnStats::default(),
        }
    }

    #[test]
    fn the_welcome_box_is_even_and_informative() {
        let info = Welcome {
            workspace: "/home/me/project",
            provider: "mock",
            model: "mock-model",
            agents: 3,
            approvals: "ask_dangerous",
            session_id: "chat-abc",
            resumed_turns: 0,
        };
        let rows = welcome(Color::Plain, 100, &info, "a tip");
        let boxed = &rows[..rows.len() - 2];
        let widths: Vec<usize> = boxed.iter().map(|row| width(row)).collect();
        assert!(widths.windows(2).all(|pair| pair[0] == pair[1]), "ragged: {widths:?}");
        assert!(boxed[0].starts_with('\u{256d}') && boxed.last().unwrap().starts_with('\u{2570}'));
        let text = rows.join("\n");
        for needle in ["Welcome to swarm!", "/help for help", "cwd: /home/me/project", "mock \u{00b7} mock-model \u{00b7} 3 agents", "Tip: a tip"] {
            assert!(text.contains(needle), "{needle} missing from\n{text}");
        }
        assert!(!text.contains("earlier turn"), "a fresh session says nothing about history");
    }

    #[test]
    fn a_resumed_session_says_so() {
        let info = Welcome {
            workspace: ".",
            provider: "p",
            model: "m",
            agents: 1,
            approvals: "a",
            session_id: "chat-1234567890abcdef-0",
            resumed_turns: 4,
        };
        assert!(welcome(Color::Plain, 80, &info, "t").join("\n").contains("4 earlier turn(s)"));
    }

    #[test]
    fn the_welcome_box_fits_narrow_terminals() {
        let info = Welcome { workspace: &"/very/long/path".repeat(10), provider: "p", model: "m", agents: 1, approvals: "a", session_id: "s", resumed_turns: 0 };
        for cols in [30, 40, 80] {
            for row in welcome(Color::Ansi, cols, &info, "tip") {
                assert!(width(&row) <= cols, "{cols}: {}", strip_ansi(&row));
            }
        }
    }

    #[test]
    fn help_lists_every_command_and_the_keys() {
        let text = help(Color::Plain, None).join("\n");
        for spec in registry::COMMANDS {
            assert!(text.contains(&format!("/{}", spec.name)), "/{} missing", spec.name);
            assert!(text.contains(spec.desc), "{} description missing", spec.name);
        }
        for group in ["Session", "Work", "Setup", "Inspect", "Keys"] {
            assert!(text.contains(group), "{group}");
        }
        assert!(text.contains("shift+tab") && text.contains("ctrl+r"));
    }

    #[test]
    fn help_filters_by_name_or_description() {
        let text = help(Color::Plain, Some("/clip")).join("\n");
        assert!(text.contains("/copy"), "matched by description: {text}");
        assert!(!text.contains("/model") && !text.contains("Keys"), "{text}");
        let none = help(Color::Plain, Some("zzzzzz")).join("\n");
        assert!(none.contains("no command matches"), "{none}");
        assert!(help(Color::Plain, Some("quit")).join("\n").contains("/exit"), "aliases are searched too");
    }

    #[test]
    fn a_finished_turn_closes_with_the_flourish_time_work_and_tokens() {
        let mut done = outcome(RunStatus::Completed);
        done.stats.files_edited.insert("a.rs".into());
        done.stats.files_edited.insert("b.rs".into());
        done.stats.added = 18;
        done.stats.removed = 3;
        done.stats.commands = 3;
        let rows = turn_footer(Color::Plain, &done);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].starts_with("\u{273b} "), "{}", rows[0]);
        assert!(rows[0].contains(" for 12.4s \u{00b7} edited 2 files +18 -3 \u{00b7} ran 3 commands \u{00b7} 2.7k tokens"), "{}", rows[0]);
        assert_eq!(rows, turn_footer(Color::Plain, &done), "the verb is a pure function of the turn");
    }

    #[test]
    fn a_failed_turn_says_why_and_a_failed_edit_is_called_out() {
        let mut failed = outcome(RunStatus::Failed);
        failed.error = Some("provider HTTP error 504".into());
        failed.stats.failed_edits.push(FailedEdit {
            tool: "edit_file".into(),
            path: "src/x.rs".into(),
            error: "'find' not found".into(),
        });
        let rows = turn_footer(Color::Plain, &failed);
        assert!(rows[0].starts_with("\u{2717} Run failed after 12.4s: provider HTTP error 504"), "{}", rows[0]);
        assert!(rows[1].contains("1 file edit did not apply"), "{rows:?}");
        assert!(rows[2].contains("src/x.rs"), "{rows:?}");
    }

    #[test]
    fn export_is_a_readable_markdown_document() {
        let mut session = Session::new("chat-1");
        session.push(Turn { goal: "add a login page".into(), status: "completed".into(), summary: "Created login.py".into(), run_id: "run_1".into() });
        session.digest = Some("- earlier work".into());
        session.totals.tokens = 99;
        let md = export_markdown(&session);
        assert!(md.starts_with("# swarm session chat-1"), "{md}");
        assert!(md.contains("1 turn(s) \u{00b7} 99 tokens"));
        assert!(md.contains("## Earlier turns (compacted)") && md.contains("- earlier work"));
        assert!(md.contains("## 1. add a login page") && md.contains("Created login.py") && md.contains("`run_1`"));
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (input, expected) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(base64(input.as_bytes()), expected, "{input:?}");
        }
        assert_eq!(base64("h\u{e9}llo \u{1f980}".as_bytes()), "aMOpbGxvIPCfpoA=");
    }

    #[test]
    fn the_clipboard_sequence_is_osc52_and_tmux_gets_it_wrapped() {
        let plain = osc52("hi", false);
        assert_eq!(plain, "\x1b]52;c;aGk=\x07");
        let tmux = osc52("hi", true);
        assert!(tmux.starts_with("\x1bPtmux;\x1b\x1b]52;c;aGk=\x07") && tmux.ends_with("\x1b\\"), "{tmux:?}");
    }
}
