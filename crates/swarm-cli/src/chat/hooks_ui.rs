//! Hooks in a session: loading them (asking before a project's own are
//! trusted), the prompt and stop events around a request, and `/hooks`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::{Chat, Request};
use crate::hooks::{self, Hooks};
use crate::style::role;
use crate::ui::{Accent, DialogSpec, UiHandle};

/// Find the hooks in force. A project file's hooks only count once approved:
/// asked here, on a terminal; anywhere else they are left out and said so.
pub(super) async fn load(
    ui: &UiHandle,
    store_dir: &Path,
    workspace: &Path,
    session_id: &str,
) -> (Arc<Hooks>, Vec<String>) {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let trust_file = home.as_deref().map(hooks::trust_file);
    let mut entries = Vec::new();
    let mut notes = Vec::new();
    for candidate in hooks::discover(store_dir, workspace, home.as_deref()) {
        let shown = candidate.path.display().to_string();
        for warning in &candidate.warnings {
            notes.push(format!("{shown}: {warning}"));
        }
        if candidate.entries.is_empty() {
            continue;
        }
        let approved = if !candidate.project {
            true
        } else if trust_file.as_deref().is_some_and(|file| hooks::is_trusted(file, &candidate)) {
            true
        } else if ui.is_live() {
            let mut spec = DialogSpec::pick(
                "This project runs commands as hooks",
                vec!["Yes, trust these hooks".to_string(), "No, ignore them".to_string()],
            );
            spec.accent = Accent::Permission;
            spec.default = 1;
            spec.body.push(role(ui.colors(), "subtle", &format!("  {shown}")));
            for entry in candidate.entries.iter().take(8) {
                spec.body.push(format!("    {}", entry.describe()));
            }
            if candidate.entries.len() > 8 {
                spec.body.push(role(ui.colors(), "subtle", &format!("    \u{2026} and {} more", candidate.entries.len() - 8)));
            }
            let yes = ui.choose(spec).await == Some(0);
            if yes && let Some(file) = &trust_file {
                if let Err(error) = hooks::trust(file, &candidate) {
                    notes.push(format!("could not remember the approval: {error}"));
                }
            }
            yes
        } else {
            false
        };
        if approved {
            entries.extend(candidate.entries);
        } else {
            notes.push(format!("ignored the hooks in {shown} (not approved; start swarm on a terminal to review them)"));
        }
    }
    (Arc::new(Hooks::new(entries, workspace.to_path_buf(), session_id)), notes)
}

impl Chat {
    /// `UserPromptSubmit`: a hook may stop the request or add context to it.
    pub(super) async fn prompt_hook(&self, mut request: Request) -> Result<Request, String> {
        if !self.hooks.entries().iter().any(|entry| entry.event == hooks::Event::UserPromptSubmit) {
            return Ok(request);
        }
        let extra = self.hooks.user_prompt(&request.display).await?;
        if !extra.is_empty() {
            request.prompt = format!("{}\n\nContext from a hook:\n{extra}", request.prompt);
        }
        Ok(request)
    }

    /// `Stop`: the request, and any goal it started, has been worked to its end.
    pub(super) async fn stop_hook(&self) {
        if !self.hooks.entries().iter().any(|entry| entry.event == hooks::Event::Stop) {
            return;
        }
        let status = self.session.turns.last().map_or("completed", |turn| turn.status.as_str());
        self.hooks.stop(status, &self.last_result).await;
    }

    /// `/hooks`: what runs, and what was left out.
    pub(super) fn cmd_hooks(&self) {
        let entries = self.hooks.entries();
        if entries.is_empty() && self.hook_notes.is_empty() {
            self.say(
                "no hooks \u{00b7} add them to ~/.swarm/settings.json or .swarm/settings.json \
                 (PreToolUse, PostToolUse, UserPromptSubmit, Stop); see the README",
            );
            return;
        }
        let mut lines = vec![format!("{} hook(s):", entries.len())];
        lines.extend(entries.iter().map(|entry| format!("  {}", entry.describe())));
        lines.extend(self.hook_notes.iter().map(|note| format!("  ! {note}")));
        self.say_lines(lines);
    }
}
