//! The slash-command table: one source for the popup menu, `/help`, prefix
//! expansion and "did you mean".

/// One slash command.
#[derive(Debug, PartialEq, Eq)]
pub struct SlashSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// Argument hint shown after the name, e.g. `<n>` or `[run]`.
    pub args: &'static str,
    pub desc: &'static str,
    /// The group `/help` files it under.
    pub group: Group,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Session,
    Run,
    Setup,
    Info,
}

impl Group {
    pub fn title(self) -> &'static str {
        match self {
            Group::Session => "Session",
            Group::Run => "Work",
            Group::Setup => "Setup",
            Group::Info => "Inspect",
        }
    }
}

macro_rules! cmd {
    ($group:ident, $name:literal, [$($alias:literal),*], $args:literal, $desc:literal) => {
        SlashSpec { name: $name, aliases: &[$($alias),*], args: $args, desc: $desc, group: Group::$group }
    };
}

pub const COMMANDS: &[SlashSpec] = &[
    cmd!(Session, "help", ["?"], "[filter]", "Show commands and shortcuts"),
    cmd!(Session, "clear", ["reset"], "", "Clear the conversation and the screen"),
    cmd!(Session, "new", [], "", "Start a fresh session"),
    cmd!(Session, "compact", [], "", "Fold older turns into a short digest"),
    cmd!(Session, "resume", [], "[run]", "Pick up a previous run"),
    cmd!(Session, "undo", [], "[n]", "Drop the last turn from context and edit it again"),
    cmd!(Session, "retry", [], "", "Drop the last turn and send it again"),
    cmd!(Session, "copy", [], "", "Copy the last result to the clipboard"),
    cmd!(Session, "export", [], "[file]", "Write this session to a markdown file"),
    cmd!(Session, "exit", ["quit", "q"], "", "Leave the session"),
    cmd!(Run, "goal", [], "[text|status|pause|resume|clear]", "Keep working until a goal is met"),
    cmd!(Run, "plan", [], "<task>", "Plan a task read-only before changing anything"),
    cmd!(Run, "init", [], "", "Create AGENTS.md with project instructions"),
    cmd!(Run, "queue", [], "[list|rm <n>|clear]", "Show or edit goals waiting to run"),
    cmd!(Run, "busy", [], "[queue|interrupt]", "What Enter does while a turn is running"),
    cmd!(Run, "agents", [], "[n]", "Swarm size for the next turns"),
    cmd!(Setup, "model", [], "[name]", "Show or change the model"),
    cmd!(Setup, "permissions", ["approve", "perm"], "[ask|danger|all]", "Tool approval policy"),
    cmd!(Setup, "allow", [], "[tool]", "List or allow a tool for this session"),
    cmd!(Setup, "allow-all", ["yolo"], "", "Stop asking for approval this session"),
    cmd!(Setup, "verbose", [], "[on|off]", "Toggle full transcript detail (ctrl+o)"),
    cmd!(Setup, "memory", [], "[edit]", "Show or edit AGENTS.md"),
    cmd!(Setup, "config", [], "", "Show the effective configuration"),
    cmd!(Info, "status", [], "[run]", "Session setup, or the state of a run"),
    cmd!(Info, "cost", ["usage"], "", "Tokens, calls and time for this session"),
    cmd!(Info, "context", [], "", "How much conversation travels with each goal"),
    cmd!(Info, "tasks", [], "[run]", "Tasks in a run"),
    cmd!(Info, "logs", ["transcript"], "[run]", "Replay a run's transcript"),
    cmd!(Info, "runs", [], "", "Recent runs in this store"),
    cmd!(Info, "history", ["turns"], "", "Turns in this session"),
    cmd!(Info, "tools", [], "", "Tools the agent may call"),
    cmd!(Info, "diff", [], "[--stat]", "Show uncommitted changes in the workspace"),
    cmd!(Info, "doctor", [], "", "Check the setup for problems"),
];

/// Commands whose name or alias starts with `prefix` (no leading slash),
/// best first: exact names, then registry order.
pub fn matching(prefix: &str) -> Vec<&'static SlashSpec> {
    let prefix = prefix.to_ascii_lowercase();
    let mut hits: Vec<(u8, usize, &'static SlashSpec)> = COMMANDS
        .iter()
        .enumerate()
        .filter_map(|(index, spec)| {
            if spec.name == prefix {
                Some((0, index, spec))
            } else if spec.name.starts_with(&prefix) {
                Some((1, index, spec))
            } else if spec.aliases.iter().any(|alias| alias.starts_with(&prefix)) {
                Some((2, index, spec))
            } else {
                None
            }
        })
        .collect();
    hits.sort_by_key(|(rank, index, _)| (*rank, *index));
    hits.into_iter().map(|(_, _, spec)| spec).collect()
}

/// What a typed command word means.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The name or an alias, exactly.
    Exact(&'static SlashSpec),
    /// A prefix that only one command starts with.
    Unique(&'static SlashSpec),
    /// A prefix several commands start with.
    Ambiguous(Vec<&'static str>),
    /// Nothing matches; the closest names, if any are close.
    Unknown(Vec<&'static str>),
}

pub fn resolve(word: &str) -> Resolved {
    let word = word.to_ascii_lowercase();
    if let Some(spec) = COMMANDS
        .iter()
        .find(|spec| spec.name == word || spec.aliases.contains(&word.as_str()))
    {
        return Resolved::Exact(spec);
    }
    if !word.is_empty() {
        let hits: Vec<&'static SlashSpec> = COMMANDS
            .iter()
            .filter(|spec| spec.name.starts_with(&word))
            .collect();
        match hits.len() {
            1 => return Resolved::Unique(hits[0]),
            n if n > 1 => return Resolved::Ambiguous(hits.iter().map(|spec| spec.name).collect()),
            _ => {}
        }
    }
    let mut scored: Vec<(usize, &'static str)> = COMMANDS
        .iter()
        .flat_map(|spec| std::iter::once(spec.name).chain(spec.aliases.iter().copied()))
        .map(|name| (distance(&word, name), name))
        .filter(|(dist, name)| *dist <= 2.max(name.len() / 3))
        .collect();
    scored.sort();
    scored.dedup_by_key(|(_, name)| *name);
    Resolved::Unknown(scored.into_iter().take(3).map(|(_, name)| name).collect())
}

/// Levenshtein distance, for "did you mean".
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut row = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            row.push((prev[j] + cost).min(prev[j + 1] + 1).min(row[j] + 1));
        }
        prev = row;
    }
    prev[b.len()]
}

/// Split `/name rest of the line` into the command word and its arguments.
pub fn split(line: &str) -> Option<(&str, &str)> {
    let body = line.trim().strip_prefix('/')?;
    // `/Users/me/file.md` is a path, not a command: a command word has no
    // further slash in it.
    let word_end = body.find(char::is_whitespace).unwrap_or(body.len());
    if body[..word_end].contains('/') {
        return None;
    }
    Some((&body[..word_end], body[word_end..].trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(resolved: &Resolved) -> &'static str {
        match resolved {
            Resolved::Exact(spec) | Resolved::Unique(spec) => spec.name,
            other => panic!("expected a command, got {other:?}"),
        }
    }

    #[test]
    fn names_and_aliases_resolve_exactly() {
        assert_eq!(name(&resolve("help")), "help");
        assert_eq!(name(&resolve("quit")), "exit");
        assert_eq!(name(&resolve("yolo")), "allow-all");
        assert_eq!(name(&resolve("usage")), "cost");
        assert_eq!(name(&resolve("HELP")), "help");
        assert!(matches!(resolve("help"), Resolved::Exact(_)));
    }

    #[test]
    fn a_unique_prefix_expands() {
        assert!(matches!(resolve("comp"), Resolved::Unique(spec) if spec.name == "compact"));
        assert!(matches!(resolve("cop"), Resolved::Unique(spec) if spec.name == "copy"));
        assert!(matches!(resolve("doc"), Resolved::Unique(spec) if spec.name == "doctor"));
    }

    #[test]
    fn an_ambiguous_prefix_lists_candidates() {
        match resolve("c") {
            Resolved::Ambiguous(names) => {
                assert!(names.contains(&"clear") && names.contains(&"compact") && names.contains(&"cost"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_typo_suggests_the_nearest_command() {
        match resolve("modle") {
            Resolved::Unknown(near) => assert_eq!(near.first(), Some(&"model"), "{near:?}"),
            other => panic!("{other:?}"),
        }
        match resolve("hepl") {
            Resolved::Unknown(near) => assert!(near.contains(&"help"), "{near:?}"),
            other => panic!("{other:?}"),
        }
        match resolve("zzzzzzzz") {
            Resolved::Unknown(near) => assert!(near.is_empty(), "{near:?}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn menu_matching_puts_exact_names_first() {
        let all = matching("");
        assert_eq!(all.len(), COMMANDS.len());
        let hits = matching("c");
        assert!(hits.iter().all(|spec| spec.name.starts_with('c')
            || spec.aliases.iter().any(|alias| alias.starts_with('c'))));
        assert_eq!(matching("cost")[0].name, "cost");
        assert_eq!(matching("zzz").len(), 0);
        // An alias match still surfaces its command.
        assert!(matching("qui").iter().any(|spec| spec.name == "exit"));
    }

    #[test]
    fn names_are_unique_and_lowercase() {
        let mut seen = std::collections::HashSet::new();
        for spec in COMMANDS {
            assert!(seen.insert(spec.name), "duplicate command {}", spec.name);
            assert_eq!(spec.name, spec.name.to_ascii_lowercase());
            for alias in spec.aliases {
                assert!(seen.insert(alias), "alias {alias} collides");
            }
            assert!(!spec.desc.is_empty());
        }
    }

    #[test]
    fn split_separates_the_word_from_its_arguments() {
        assert_eq!(split("/model gpt-5"), Some(("model", "gpt-5")));
        assert_eq!(split("  /help  "), Some(("help", "")));
        assert_eq!(split("/goal   ship it  "), Some(("goal", "ship it")));
        assert_eq!(split("fix the bug"), None);
        assert_eq!(split("/Users/me/file.md is broken"), None, "a path is not a command");
        assert_eq!(split("1/2 of the file"), None);
        assert_eq!(split("/"), Some(("", "")));
    }
}
