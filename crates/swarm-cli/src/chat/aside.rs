//! `/btw <question>`: a side question answered from the conversation so far,
//! without interrupting a running turn and without joining the conversation.
//!
//! It is one model call with no tools, so it can say what it already knows (the
//! session summary) but cannot look at files or run anything.

use std::time::Duration;

use swarm_core::config::RunConfig;
use swarm_core::message::ChatMessage;
use swarm_providers::{SendRequest, SessionConfig};
use swarm_runtime::build_registry;

use super::Chat;
use crate::style::{self, Color, role};
use crate::ui::text::wrap_hang;

const SYSTEM: &str = "You answer a quick side question about an ongoing coding session. You are given \
a summary of the conversation so far. You cannot read files or run commands right now, so answer from \
the summary and general knowledge, in a few sentences. If the answer needs the code or a command, say \
so in one line instead of guessing.";

/// Ask the configured model. `Err` is something a person can read.
pub(super) async fn ask(cfg: RunConfig, context: String, question: String) -> Result<String, String> {
    let registry = build_registry(&cfg).map_err(|error| error.to_string())?;
    let provider = registry
        .find(&cfg.provider)
        .ok_or_else(|| format!("provider `{}` is not available", cfg.provider))?;
    let session = provider
        .start_session(SessionConfig::default())
        .await
        .map_err(|error| error.to_string())?;
    let mut request = SendRequest::new(vec![
        ChatMessage::system(SYSTEM.to_string()),
        ChatMessage::user(format!("{context}{question}")),
    ]);
    request.zero_cost = cfg.zero_cost;
    request.deadline = Some(Duration::from_secs(120));
    let response = session.send(request).await;
    let _ = session.close().await;
    let text = response.map_err(|error| error.to_string())?.text;
    let text = text.trim();
    if text.is_empty() { Err("the model gave no answer".to_string()) } else { Ok(text.to_string()) }
}

/// The question and its answer as transcript rows, set apart from the turn
/// around them.
pub(super) fn rows(colors: Color, cols: usize, question: &str, answer: &Result<String, String>) -> Vec<String> {
    let cols = cols.max(20);
    let mut out = vec![role(colors, "plan", &format!("  \u{25c6} btw: {}", super::clip(question, cols.saturating_sub(12))))];
    match answer {
        Ok(text) => {
            for line in text.lines() {
                let wrapped = wrap_hang(line.trim_end(), cols.saturating_sub(1), "    ", "    ");
                if wrapped.is_empty() {
                    out.push(String::new());
                }
                out.extend(wrapped.into_iter().map(|row| role(colors, "subtle", &row)));
            }
        }
        Err(why) => out.push(role(colors, "fail", &format!("    \u{2717} {why}"))),
    }
    out.push(role(colors, "subtle", "    (not added to the conversation)"));
    out
}

impl Chat {
    /// `/btw`: answer in the background while a turn runs; in a script, wait for it.
    pub(super) async fn cmd_btw(&mut self, question: &str) {
        let question = question.trim().to_string();
        if question.is_empty() {
            self.say("usage: /btw <question>  answers from the conversation so far without interrupting or joining it");
            return;
        }
        let cfg = self.cfg.clone();
        let context = self.session.render(self.context_chars);
        let (ui, colors) = (self.ui.clone(), self.colors);
        let answer = async move {
            let answer = ask(cfg, context, question.clone()).await;
            ui.print(rows(colors, style::term_width(), &question, &answer));
        };
        if self.ui.is_live() {
            tokio::spawn(answer);
        } else {
            answer.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::text::strip_ansi;

    #[test]
    fn an_answer_is_wrapped_and_marked_as_not_part_of_the_conversation() {
        let long = "word ".repeat(40);
        let out = rows(Color::Plain, 40, "why is the build slow?", &Ok(long));
        let text: Vec<String> = out.iter().map(|row| strip_ansi(row)).collect();
        assert!(text[0].contains("btw: why is the build slow?"), "{text:?}");
        assert!(text.iter().all(|row| row.chars().count() <= 40), "{text:?}");
        assert!(text.last().unwrap().contains("not added to the conversation"));
    }

    #[test]
    fn a_failed_question_says_why() {
        let out = rows(Color::Plain, 80, "q", &Err("provider down".into()));
        assert!(strip_ansi(&out.join("\n")).contains("provider down"));
    }
}
