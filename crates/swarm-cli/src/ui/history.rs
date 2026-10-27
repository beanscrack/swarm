//! Prompt history: ↑/↓ browsing that keeps your half-typed draft, reverse
//! search, and persistence across sessions.
//!
//! On disk it is JSON lines (one string per line), so a multi-line prompt is a
//! single entry and nothing needs escaping by hand.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

/// Entries kept (and loaded). Older ones are dropped when the file is read.
const KEEP: usize = 1000;

#[derive(Debug, Default, Clone)]
pub struct History {
    items: Vec<String>,
    /// The entry being browsed; `None` when editing a fresh draft.
    pos: Option<usize>,
    /// The draft typed before browsing began, restored when ↓ walks past the end.
    draft: String,
    path: Option<PathBuf>,
}

impl History {
    /// In-memory only (tests, non-interactive use).
    pub fn memory() -> Self {
        Self::default()
    }

    /// Load from `path` (a missing or unreadable file is an empty history) and
    /// append future entries to it.
    pub fn load(path: PathBuf) -> Self {
        let mut items: Vec<String> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<String>(line).ok())
            .collect();
        if items.len() > KEEP {
            items.drain(..items.len() - KEEP);
        }
        Self { items, pos: None, draft: String::new(), path: Some(path) }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[cfg(test)]
    pub fn items(&self) -> &[String] {
        &self.items
    }

    /// Record a submitted prompt. Blank prompts and immediate repeats are
    /// skipped.
    pub fn push(&mut self, text: &str) {
        self.pos = None;
        self.draft.clear();
        let text = text.trim_end();
        if text.trim().is_empty() || self.items.last().map(String::as_str) == Some(text) {
            return;
        }
        self.items.push(text.to_string());
        if let Some(path) = &self.path {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{}", serde_json::to_string(text).unwrap_or_default());
            }
        }
    }

    /// Step to the previous (older) entry. `current` is what is in the editor
    /// now; it becomes the draft on the first step back.
    pub fn prev(&mut self, current: &str) -> Option<&str> {
        let target = match self.pos {
            None => {
                if self.items.is_empty() {
                    return None;
                }
                self.draft = current.to_string();
                self.items.len() - 1
            }
            Some(0) => return None,
            Some(index) => index - 1,
        };
        self.pos = Some(target);
        self.items.get(target).map(String::as_str)
    }

    /// Step to the next (newer) entry, or back to the draft past the newest.
    pub fn next(&mut self) -> Option<String> {
        let index = self.pos?;
        if index + 1 < self.items.len() {
            self.pos = Some(index + 1);
            self.items.get(index + 1).cloned()
        } else {
            self.pos = None;
            Some(std::mem::take(&mut self.draft))
        }
    }

    #[cfg(test)]
    pub fn browsing(&self) -> bool {
        self.pos.is_some()
    }

    /// The newest entry at an index below `before` containing `query`
    /// (case-insensitive). Pass `None` to search from the newest.
    pub fn search(&self, query: &str, before: Option<usize>) -> Option<(usize, &str)> {
        let end = before.unwrap_or(self.items.len()).min(self.items.len());
        let needle = query.to_lowercase();
        self.items[..end]
            .iter()
            .enumerate()
            .rev()
            .find(|(_, item)| item.to_lowercase().contains(&needle))
            .map(|(index, item)| (index, item.as_str()))
    }

    /// Stash the editor's text as a history entry without submitting it
    /// (Esc Esc discards a draft but keeps it recallable).
    pub fn keep_draft(&mut self, text: &str) {
        self.push(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled() -> History {
        let mut history = History::memory();
        for text in ["alpha", "beta", "gamma"] {
            history.push(text);
        }
        history
    }

    #[test]
    fn up_walks_back_and_down_returns_to_the_draft() {
        let mut history = filled();
        assert_eq!(history.prev("half typed"), Some("gamma"));
        assert_eq!(history.prev("ignored while browsing"), Some("beta"));
        assert_eq!(history.next().as_deref(), Some("gamma"));
        assert_eq!(history.next().as_deref(), Some("half typed"), "past the newest: the draft is back");
        assert_eq!(history.next(), None, "no longer browsing");
    }

    #[test]
    fn up_stops_at_the_oldest_entry() {
        let mut history = filled();
        history.prev("");
        history.prev("");
        assert_eq!(history.prev(""), Some("alpha"));
        assert_eq!(history.prev(""), None);
        assert_eq!(history.prev(""), None);
    }

    #[test]
    fn empty_history_does_nothing() {
        let mut history = History::memory();
        assert_eq!(history.prev("x"), None);
        assert_eq!(history.next(), None);
        assert!(history.is_empty());
    }

    #[test]
    fn blanks_and_repeats_are_not_recorded() {
        let mut history = History::memory();
        history.push("one");
        history.push("one");
        history.push("   ");
        history.push("");
        history.push("two");
        history.push("one");
        assert_eq!(history.items(), ["one", "two", "one"]);
    }

    #[test]
    fn pushing_ends_browsing() {
        let mut history = filled();
        history.prev("");
        assert!(history.browsing());
        history.push("delta");
        assert!(!history.browsing());
        assert_eq!(history.prev(""), Some("delta"));
    }

    #[test]
    fn search_finds_the_newest_match_then_older_ones() {
        let mut history = History::memory();
        for text in ["fix login", "write docs", "fix signup", "run tests"] {
            history.push(text);
        }
        let (first, text) = history.search("FIX", None).unwrap();
        assert_eq!(text, "fix signup");
        let (second, text) = history.search("fix", Some(first)).unwrap();
        assert_eq!(text, "fix login");
        assert!(history.search("fix", Some(second)).is_none());
        assert!(history.search("nothing", None).is_none());
    }

    #[test]
    fn it_persists_multiline_prompts_across_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("history");
        let mut first = History::load(path.clone());
        first.push("single");
        first.push("two\nlines with \"quotes\"");
        let second = History::load(path);
        assert_eq!(second.items(), ["single", "two\nlines with \"quotes\""]);
    }

    #[test]
    fn a_corrupt_history_file_is_survivable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history");
        std::fs::write(&path, "not json\n\"good\"\n{\"also\":\"bad\"}\n").unwrap();
        let history = History::load(path);
        assert_eq!(history.items(), ["good"]);
    }
}
