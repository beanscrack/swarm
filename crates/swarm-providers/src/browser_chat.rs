//! Consumer chat UIs used as Swarm's reasoning model, through a local bridge.
//!
//! Swarm does not talk to ChatGPT or Gemini directly: a loopback Node bridge
//! owns one browser tab per Swarm session and drives the site's own chat page.
//! Swarm keeps the durable conversation, the tool loop, budgets, and approvals;
//! the bridge only receives the new turns and returns the rendered reply.
//!
//! Bridge protocol (loopback HTTP, JSON):
//!
//! * `GET  /health`                      -> `{ready, sessions, access_points, chat_url}`
//! * `POST /sessions` `{model}`          -> `{id, access_point}`
//! * `POST /sessions/{id}/messages`      -> `{text, chat_url, model?, usage?}`
//!   `{request_id, prompt}`, idempotent per `request_id`
//! * `DELETE /sessions/{id}`             -> `{closed}`
//!
//! The sites have no tool calling, so Swarm asks for a fenced ```swarm JSON
//! block and parses the reply: [`parse_tool_calls`].

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream;
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use swarm_core::budget::{BillingMode, TokenUsage};
use swarm_core::message::ChatRole;
use swarm_core::provider::{ProviderCapabilities, ProviderErrorClass, ProviderKind};
use swarm_core::{ProviderId, SessionId};

use crate::error::ProviderError;
use crate::pricing::ModelPricing;
use crate::provider::{
    FinishReason, NativeToolCall, Provider, ProviderStream, SendRequest, Session, SessionConfig,
    ToolProtocol, build_chunks,
};

/// The consumer chat sites are a human-speed channel: a turn that streams a
/// 4k-character tool payload can take a quarter of an hour, and the reply keeps
/// arriving after a 10-minute window has closed, which turns a slow turn into a
/// failed run. The window is generous by default; a fast self-hosted bridge can
/// shorten it with SWARM_BROWSER_CHAT_TIMEOUT_MS.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1500);

fn request_timeout() -> Duration {
    std::env::var("SWARM_BROWSER_CHAT_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|millis| *millis >= 30_000)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_TIMEOUT)
}

/// Which consumer chat site the bridge is driving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatSite {
    ChatGpt,
    Gemini,
}

impl ChatSite {
    /// Name used in prompts and error messages.
    pub fn display(self) -> &'static str {
        match self {
            ChatSite::ChatGpt => "ChatGPT",
            ChatSite::Gemini => "Gemini",
        }
    }

    pub fn kind(self) -> ProviderKind {
        match self {
            ChatSite::ChatGpt => ProviderKind::ChatGptWeb,
            ChatSite::Gemini => ProviderKind::GeminiWeb,
        }
    }

    /// The bridge port this site's preset expects.
    pub fn default_bridge_url(self) -> &'static str {
        match self {
            ChatSite::ChatGpt => "http://127.0.0.1:8766",
            ChatSite::Gemini => "http://127.0.0.1:8765",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BrowserChatConfig {
    pub site: ChatSite,
    pub base_url: String,
    pub model: String,
    pub billing: BillingMode,
    pub max_concurrency: u32,
}

impl BrowserChatConfig {
    pub fn new(site: ChatSite, base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            site,
            base_url: base_url.into(),
            model: model.into(),
            billing: BillingMode::Subscription,
            max_concurrency: 4,
        }
    }

    /// ChatGPT web chat through the bridge on `base_url`.
    pub fn chatgpt(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self::new(ChatSite::ChatGpt, base_url, model)
    }

    /// Gemini web chat through the bridge on `base_url`.
    pub fn gemini(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self::new(ChatSite::Gemini, base_url, model)
    }

    pub fn billing(mut self, billing: BillingMode) -> Self {
        self.billing = billing;
        self
    }

    pub fn max_concurrency(mut self, max: u32) -> Self {
        self.max_concurrency = max.max(1);
        self
    }
}

#[derive(Debug, Clone)]
pub struct BrowserChatProvider {
    id: ProviderId,
    config: BrowserChatConfig,
    client: Client,
}

impl BrowserChatProvider {
    pub fn new(id: ProviderId, config: BrowserChatConfig) -> Result<Self, ProviderError> {
        let site = config.site.display();
        let url = Url::parse(&config.base_url)
            .map_err(|e| ProviderError::Session(format!("invalid {site} bridge URL: {e}")))?;
        if url.scheme() != "http"
            || !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
        {
            return Err(ProviderError::Session(format!(
                "{site} browser bridge must use loopback HTTP"
            )));
        }
        let client = Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        Ok(Self { id, config, client })
    }

    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}{}",
            self.config.base_url.trim_end_matches('/'),
            path
        )
    }
}

#[derive(Debug, Deserialize)]
struct SessionCreated {
    id: String,
}

#[async_trait]
impl Provider for BrowserChatProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }
    fn kind(&self) -> ProviderKind {
        self.config.site.kind()
    }
    fn model(&self) -> &str {
        &self.config.model
    }
    fn capabilities(&self) -> ProviderCapabilities {
        let site = self.config.site.display();
        ProviderCapabilities {
            streaming: false,
            tools: false,
            json_mode: false,
            reasoning: true,
            managed_agent: false,
            max_output_tokens: None,
            max_concurrency: Some(self.config.max_concurrency),
            notes: Some(format!("{site} web chat; tool calls use Swarm fenced JSON")),
        }
    }
    fn billing(&self) -> BillingMode {
        self.config.billing
    }
    fn pricing(&self) -> ModelPricing {
        ModelPricing::default()
    }

    async fn start_session(&self, _config: SessionConfig) -> Result<Box<dyn Session>, ProviderError> {
        let site = self.config.site.display();
        let response = self
            .client
            .post(self.endpoint("/sessions"))
            .json(&json!({"model": self.config.model}))
            .send()
            .await
            .map_err(|e| ProviderError::Transport(format!("{site} bridge unreachable: {e}")))?;
        let response = checked(response).await?;
        let created: SessionCreated = response.json().await
            .map_err(|e| ProviderError::Malformed(format!("{site} session: {e}")))?;
        if created.id.is_empty() {
            return Err(ProviderError::Malformed(format!(
                "{site} session has no ID"
            )));
        }
        Ok(Box::new(BrowserChatSession {
            id: SessionId::new(),
            remote_id: created.id,
            provider_id: self.id.clone(),
            site: self.config.site,
            model: self.config.model.clone(),
            base_url: self.config.base_url.clone(),
            billing: self.config.billing,
            client: self.client.clone(),
            sent_count: Arc::new(Mutex::new(0)),
            summary: Arc::new(Mutex::new(None)),
        }))
    }
}

/// Token counts the bridge read back from the site (ChatGPT reports them on its
/// conversation API; the web UI itself shows none).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BridgeUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
}

impl From<BridgeUsage> for TokenUsage {
    fn from(usage: BridgeUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cached_input_tokens: usage.cached_input_tokens,
        }
    }
}

#[derive(Debug, Deserialize)]
struct BrowserReply {
    text: String,
    #[allow(dead_code)]
    chat_url: Option<String>,
    /// Model the site actually answered with, when the site reports it.
    #[allow(dead_code)]
    model: Option<String>,
    usage: Option<BridgeUsage>,
}

struct BrowserChatSession {
    id: SessionId,
    remote_id: String,
    provider_id: ProviderId,
    site: ChatSite,
    model: String,
    base_url: String,
    billing: BillingMode,
    client: Client,
    sent_count: Arc<Mutex<usize>>,
    /// Model-written summary of the steps before `covered` (a message count into
    /// the steps after the task), refreshed as more steps fall out of the window.
    summary: Arc<Mutex<Option<(usize, String)>>>,
}

impl BrowserChatSession {
    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/sessions/{}{path}",
            self.base_url.trim_end_matches('/'),
            self.remote_id
        )
    }

    /// Ask the model, in its own fresh chat, to fold the steps that no longer fit
    /// into the running summary. Returns the text and its estimated token cost,
    /// or `None` when it fails: the prompt then keeps its one-line digest.
    async fn summarize(
        &self,
        request: &SendRequest,
        from: usize,
        to: usize,
        previous: Option<&str>,
    ) -> Option<(String, u64)> {
        let steps = steps_of(request);
        let lines: Vec<String> = steps.get(from..to.min(steps.len()))?.iter().map(summary_line).collect();
        let prompt = format!(
            "You are compacting an agent's working notes. Write a plain-text summary of at most {SUMMARY_CHARS} \
             characters covering the previous summary and the new steps below: what was done, concrete findings \
             (file paths, commands, results, errors), decisions made, and what is still unfinished. Reply with \
             the summary only: no preamble, no JSON, no code fences.\n\n[PREVIOUS SUMMARY]\n{}\n\n[NEW STEPS]\n{}\n",
            previous.unwrap_or("(none)"),
            clip_middle(&lines.join("\n"), 7_000),
        );
        let response = self
            .client
            .post(self.endpoint("/messages"))
            .timeout(Duration::from_secs(90))
            .json(&json!({"request_id": format!("{}-sum-{to}", self.id), "prompt": prompt, "fresh": true}))
            .send()
            .await
            .ok()?;
        let reply: BrowserReply = checked(response).await.ok()?.json().await.ok()?;
        let text: String = reply.text.trim().chars().take(SUMMARY_CHARS + 300).collect();
        if text.is_empty() || text.contains("```swarm") {
            return None;
        }
        let tokens = estimate_tokens(&prompt).saturating_add(estimate_tokens(&text));
        Some((text, tokens))
    }
}

#[async_trait]
impl Session for BrowserChatSession {
    fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn session_id(&self) -> &SessionId {
        &self.id
    }

    async fn stream(&self, request: &SendRequest) -> Result<ProviderStream, ProviderError> {
        let site = self.site.display();
        if request.zero_cost && !self.billing.is_zero_cost() {
            return Err(ProviderError::Session(format!(
                "{site} subscription access is unavailable in a zero-cost run"
            )));
        }
        let mut sent_count = self.sent_count.lock().await;
        let stateless = stateless_mode();
        if !stateless && request.messages.len() < *sent_count {
            return Err(ProviderError::Session(format!(
                "{site} conversation history moved backwards"
            )));
        }
        let mut summary_tokens = 0u64;
        let prompt = if stateless {
            let mut summary = self.summary.lock().await;
            // History that shrank (a rewind or a restarted agent) invalidates a
            // summary that covered steps no longer in it.
            if summary.as_ref().is_some_and(|(covered, _)| *covered > steps_of(request).len()) {
                *summary = None;
            }
            let budget = prompt_budget();
            let current = summary.as_ref().map(|(covered, text)| (*covered, text.as_str()));
            let (mut prompt, first_kept) = compile_with_summary(request, self.site, budget, current);
            let covered = summary.as_ref().map_or(0, |(covered, _)| *covered);
            if request.tool_protocol == ToolProtocol::Text
                && first_kept.saturating_sub(covered) >= SUMMARY_REFRESH_MESSAGES
            {
                let previous = summary.as_ref().map(|(_, text)| text.clone());
                if let Some((text, tokens)) =
                    self.summarize(request, covered, first_kept, previous.as_deref()).await
                {
                    prompt = compile_with_summary(request, self.site, budget, Some((first_kept, &text))).0;
                    *summary = Some((first_kept, text));
                    summary_tokens = tokens;
                }
            }
            prompt
        } else {
            format_prompt(request, *sent_count, self.site)
        };
        if prompt.trim().is_empty() {
            return Err(ProviderError::Session(format!(
                "no new {site} prompt to send"
            )));
        }
        let request_id = format!("{}-{}", self.id, request.messages.len());
        let response = self
            .client
            .post(self.endpoint("/messages"))
            .timeout(request.deadline.unwrap_or_else(request_timeout))
            .json(&json!({"request_id": request_id, "prompt": prompt, "fresh": stateless}))
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let response = checked(response).await?;
        let reply: BrowserReply = response.json().await
            .map_err(|e| ProviderError::Malformed(format!("{site} reply: {e}")))?;
        if reply.text.trim().is_empty() {
            return Err(ProviderError::Malformed(format!(
                "{site} returned an empty reply"
            )));
        }
        *sent_count = request.messages.len();
        let (calls, finish) = match parse_tool_calls(&reply.text, &request_id) {
            Ok(calls) if calls.is_empty() => (calls, FinishReason::Stop),
            Ok(calls) => (calls, FinishReason::ToolCalls),
            Err(_) => (Vec::new(), FinishReason::MalformedTool),
        };
        // The site reports no usage on the web UI, so a missing figure is estimated
        // (about four characters a token) instead of recorded as zero. The summary
        // call, when there was one, is part of this step's cost.
        let usage = reply.usage.map(TokenUsage::from).unwrap_or_else(|| TokenUsage {
            input_tokens: estimate_tokens(&prompt).saturating_add(summary_tokens),
            output_tokens: estimate_tokens(&reply.text),
            cached_input_tokens: 0,
        });
        let chunks = build_chunks(&reply.text, &calls, finish, Some(usage));
        Ok(Box::pin(stream::iter(chunks.into_iter().map(Ok))))
    }

    async fn close(&self) -> Result<(), ProviderError> {
        let response = self
            .client
            .delete(self.endpoint(""))
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        checked(response).await?;
        Ok(())
    }
}

/// Builds the text sent to the chat page. Swarm owns the transcript, so only
/// turns the site has not already seen are sent; assistant turns are already
/// visible in the chat and are skipped once the session is warm.
fn format_prompt(request: &SendRequest, sent_count: usize, site: ChatSite) -> String {
    let name = site.display();
    let mut out = String::new();
    // A call with no tools (the /goal judge, /btw) asks for its own answer shape.
    // The tool contract below would contradict it ("exactly one line of JSON" vs
    // "exactly one fenced swarm block"), and the site answers such a prompt with
    // nothing at all.
    if request.tool_protocol != ToolProtocol::Text {
        if sent_count == 0 {
            out.push_str("You are a reasoning model working for Swarm, an agent runtime. Follow the SYSTEM instructions below and answer in exactly the form they ask for. You have no tools for this reply.\n");
        }
        for message in &request.messages[sent_count..] {
            if sent_count > 0 && message.role == ChatRole::Assistant {
                continue;
            }
            out.push_str(match message.role {
                ChatRole::System => "\n[SYSTEM]\n",
                ChatRole::User => "\n[USER]\n",
                ChatRole::Assistant => "\n[ASSISTANT]\n",
                ChatRole::Tool => "\n[TOOL RESULT]\n",
            });
            out.push_str(&message.content);
            out.push('\n');
        }
        let _ = name;
        return out;
    }
    if sent_count == 0 {
        out.push_str(TOOL_PREAMBLE);
        if request.tool_protocol == ToolProtocol::Text {
            if let Some(tools) = request.tools_as_text() {
                out.push_str(&tools);
            }
        }
    }
    for message in &request.messages[sent_count..] {
        if sent_count > 0 && message.role == ChatRole::Assistant {
            continue; // already visible in the chat page
        }
        out.push_str("\n[");
        out.push_str(match message.role {
            ChatRole::System => "SYSTEM",
            ChatRole::User => "USER",
            ChatRole::Assistant => "ASSISTANT",
            ChatRole::Tool => "TOOL RESULT",
        });
        if let Some(id) = &message.tool_call_id {
            out.push(' ');
            out.push_str(id);
        }
        out.push_str("]\n");
        out.push_str(&message.content);
        if !message.tool_calls.is_empty() {
            out.push_str("\nRequested tools: ");
            out.push_str(&serde_json::to_string(&message.tool_calls).unwrap_or_default());
        }
        out.push('\n');
    }
    // The contract is repeated on every turn, as the last thing the model reads.
    // Chat sites have no function-calling channel and drift out of a preamble
    // sent once, and an "otherwise answer normally" allowance lets the site
    // complete the task in its own sandbox while Swarm records zero tool calls.
    out.push_str(TOOL_CONTRACT);
    let _ = name;
    out
}

const TOOL_PREAMBLE: &str = "You are the reasoning model for Swarm, an agent runtime. Follow the SYSTEM instructions below. Swarm executes tools and returns their results. To request one tool, reply with only a fenced ```swarm JSON block containing {\"tool\":{\"name\":\"tool_name\",\"arguments\":{...}}}. Never claim a tool ran until Swarm returns its result.\n";

const TOOL_CONTRACT: &str = "\nSWARM PROTOCOL (applies to this reply): you have no files, shell, browser, canvas, \
     or download ability, and anything you make there does not exist for Swarm. Reply with \
     exactly one fenced ```swarm block and nothing else: \
     {\"tool\":{\"name\":\"<tool name from the list above>\",\"arguments\":{...}}}. Only when \
     Swarm has already returned tool results that complete the task may you reply with a \
     plain-text summary instead.";

// ---- Stateless prompt compiler -------------------------------------------------
//
// An API client re-sends the whole conversation on every call and the server
// caches the prefix; Codex and Claude Code both work that way and compact when the
// window fills. A chat site cannot be driven like that incrementally: it drops the
// 6th message of a chat and a long chat degrades. So each call here is its own
// chat (`fresh`), and the prompt is compiled from Swarm's own transcript under a
// size budget: goal and tools always in full, the newest steps in full, older steps
// condensed to one line each, and the latest checklist kept verbatim.

/// Characters of prompt the site takes comfortably in one message (18k measured).
const DEFAULT_PROMPT_CHARS: usize = 16_000;
const RESULT_CHARS: usize = 3_000;
const NEWEST_RESULT_CHARS: usize = 6_000;
const DIGEST_CHARS: usize = 2_500;
const CHECKLIST_CHARS: usize = 1_500;
/// Ask for a fresh summary once this many messages (about six steps) have fallen
/// out of the window since the last one: one extra call per ~6 steps.
const SUMMARY_REFRESH_MESSAGES: usize = 12;
const SUMMARY_CHARS: usize = 900;

/// About four characters to a token: a floor-level estimate for sites that report
/// no usage, good enough to make cost and budget figures mean something.
fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// The messages after the task itself: everything from the model's first reply.
fn steps_of(request: &SendRequest) -> &[swarm_core::message::ChatMessage] {
    let first_reply = request
        .messages
        .iter()
        .position(|message| message.role == ChatRole::Assistant)
        .unwrap_or(request.messages.len());
    &request.messages[first_reply..]
}

/// A step as the summarizer reads it: more detail than the one-line digest.
fn summary_line(message: &swarm_core::message::ChatMessage) -> String {
    match message.role {
        ChatRole::Tool => format!("RESULT: {}", one_line(&message.content, 300)),
        _ if !message.tool_calls.is_empty() => message
            .tool_calls
            .iter()
            .map(|call| format!("CALL {}({})", call.name, one_line(&call.arguments.to_string(), 160)))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => format!("SAID: {}", one_line(&message.content, 200)),
    }
}

fn prompt_budget() -> usize {
    std::env::var("SWARM_PROMPT_CHARS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|chars| *chars >= 4_000)
        .unwrap_or(DEFAULT_PROMPT_CHARS)
}

/// One chat per call is the default; `SWARM_BROWSER_STATEFUL=1` restores the
/// incremental behaviour (new turns only, one long chat).
fn stateless_mode() -> bool {
    !matches!(std::env::var("SWARM_BROWSER_STATEFUL").as_deref(), Ok(value) if !value.is_empty() && value != "0")
}

fn clip_middle(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let head = max * 2 / 3;
    let tail = max - head;
    let start: String = text.chars().take(head).collect();
    let end: String = text.chars().skip(count - tail).collect();
    format!("{start}\n[... {} characters omitted ...]\n{end}", count - max)
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}\u{2026}", flat.chars().take(max).collect::<String>())
    }
}

fn role_label(role: ChatRole) -> &'static str {
    match role {
        ChatRole::System => "SYSTEM",
        ChatRole::User => "USER",
        ChatRole::Assistant => "ASSISTANT",
        ChatRole::Tool => "TOOL RESULT",
    }
}

fn render_message(message: &swarm_core::message::ChatMessage, cap: usize) -> String {
    let mut out = format!("\n[{}", role_label(message.role));
    if let Some(id) = &message.tool_call_id {
        out.push(' ');
        out.push_str(id);
    }
    out.push_str("]\n");
    out.push_str(&clip_middle(&message.content, cap));
    if !message.tool_calls.is_empty() {
        let calls = serde_json::to_string(&message.tool_calls).unwrap_or_default();
        out.push_str("\nRequested tools: ");
        out.push_str(&clip_middle(&calls, 2_000));
    }
    out.push('\n');
    out
}

/// One line for a step that no longer fits in full.
fn digest_line(message: &swarm_core::message::ChatMessage) -> String {
    match message.role {
        ChatRole::Tool => format!("  -> {}", one_line(&message.content, 90)),
        _ if !message.tool_calls.is_empty() => message
            .tool_calls
            .iter()
            .map(|call| format!("- {}({})", call.name, one_line(&call.arguments.to_string(), 70)))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => format!("- {}: {}", role_label(message.role).to_lowercase(), one_line(&message.content, 90)),
    }
}

/// The whole prompt for one call, built from the full transcript.
fn compile_prompt(request: &SendRequest, site: ChatSite, budget: usize) -> String {
    compile_with_summary(request, site, budget, None).0
}

/// [`compile_prompt`] with an optional model-written summary of the first
/// `covered` step messages, and the index of the first step kept in full.
fn compile_with_summary(
    request: &SendRequest,
    site: ChatSite,
    budget: usize,
    summary: Option<(usize, &str)>,
) -> (String, usize) {
    if request.tool_protocol != ToolProtocol::Text {
        return (format_prompt(request, 0, site), 0);
    }
    let mut head = String::from(TOOL_PREAMBLE);
    if let Some(tools) = request.tools_as_text() {
        head.push_str(&tools);
    }
    // Everything before the model's first reply is the task itself: keep it whole.
    // The system message carries the project's AGENTS.md / CLAUDE.md (up to 8,000
    // characters on top of the base prompt), so it gets the larger allowance.
    let steps = steps_of(request);
    let task = &request.messages[..request.messages.len() - steps.len()];
    for message in task {
        let cap = if message.role == ChatRole::System { 14_000 } else { 8_000 };
        head.push_str(&render_message(message, cap));
    }
    let room = budget.saturating_sub(head.chars().count() + TOOL_CONTRACT.chars().count());

    // Newest first, in full, always keeping the latest call and its result.
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut first_kept = steps.len();
    for (index, message) in steps.iter().enumerate().rev() {
        let cap = if index + 1 == steps.len() { NEWEST_RESULT_CHARS } else { RESULT_CHARS };
        let block = render_message(message, cap);
        let size = block.chars().count();
        if kept.len() >= 2 && used + size > room {
            break;
        }
        used += size;
        kept.push(block);
        first_kept = index;
    }
    kept.reverse();

    let mut out = head;
    let earlier = &steps[..first_kept];
    if !earlier.is_empty() {
        // Steps a summary already covers are not repeated as digest lines.
        let covered = summary.map_or(0, |(covered, _)| covered.min(first_kept));
        if let Some((_, text)) = summary {
            out.push_str("\n[SUMMARY OF EARLIER WORK]\n");
            out.push_str(text);
            out.push('\n');
        }
        let mut lines: Vec<String> = earlier[covered..].iter().map(digest_line).collect();
        let mut total = 0usize;
        let mut from = lines.len();
        while from > 0 && total + lines[from - 1].chars().count() < DIGEST_CHARS {
            from -= 1;
            total += lines[from].chars().count() + 1;
        }
        let omitted = from;
        lines.drain(..from);
        if !lines.is_empty() {
            out.push_str("\n[EARLIER STEPS, condensed");
            if omitted > 0 {
                out.push_str(&format!("; {omitted} older lines dropped"));
            }
            out.push_str("]\n");
            out.push_str(&lines.join("\n"));
            out.push('\n');
        }
        // The checklist is the agent's own record of progress: never condense it.
        let checklist = earlier
            .iter()
            .rev()
            .flat_map(|message| message.tool_calls.iter().rev())
            .find(|call| call.name == "todo_write");
        if let Some(call) = checklist {
            out.push_str("[LATEST CHECKLIST]\n");
            out.push_str(&clip_middle(&call.arguments.to_string(), CHECKLIST_CHARS));
            out.push('\n');
        }
        out.push_str("[RECENT STEPS, in full]\n");
    }
    for block in kept {
        out.push_str(&block);
    }
    out.push_str(TOOL_CONTRACT);
    (out, first_kept)
}

/// Find a Swarm tool request in text with no usable fence, by scanning for a
/// balanced top-level JSON object that mentions `tool`/`tool_calls`. String
/// contents are tracked so braces inside a file body cannot end the object.
fn scan_tool_json(text: &str) -> Option<Value> {
    let bytes = text.as_bytes();
    let mut stack: Vec<u8> = Vec::new();
    let mut start: Option<usize> = None;
    let mut in_string = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match *byte {
            b'"' => in_string = true,
            open @ (b'{' | b'[') => {
                if stack.is_empty() {
                    start = Some(index);
                }
                stack.push(open);
            }
            b'}' | b']' => {
                stack.pop();
                if stack.is_empty() {
                    if let Some(from) = start.take() {
                        if let Some(parsed) = parse_tool_object(&text[from..=index]) {
                            return Some(parsed);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    // The 2026 site can drop the last character of a rendered code block, so a
    // real tool call can arrive one `}` short: `…"status":"pending"}}` where the
    // source had three closers. That is a one-character channel loss, not a
    // malformed plan, and losing it ends the run as "completed" mid-task. Close
    // what is open, within a small budget, and only if nothing was cut mid-value.
    if let (Some(from), false) = (start, stack.is_empty()) {
        if stack.len() <= 4 {
            if let Some(parsed) = parse_tool_object(&repair_truncated(&text[from..], &stack)) {
                return Some(parsed);
            }
        }
    }
    None
}

fn parse_tool_object(candidate: &str) -> Option<Value> {
    if !candidate.contains("\"tool\"") && !candidate.contains("\"tool_calls\"") {
        return None;
    }
    let parsed = serde_json::from_str::<Value>(candidate).ok()?;
    if parsed.get("tool").is_some() || parsed.get("tool_calls").is_some() {
        Some(parsed)
    } else {
        None
    }
}

/// Append the closers a truncated payload lost. Refuses (by returning the
/// fragment unchanged, which then fails to parse) when the cut fell inside a
/// string with content: that is a real truncation, and inventing a `"` there
/// would hand a tool a silently shortened file body.
fn repair_truncated(fragment: &str, stack: &[u8]) -> String {
    let mut out = fragment.trim_end().trim_end_matches(',').to_string();
    if out.ends_with('\\') {
        out.pop();
    }
    match open_string_tail(&out) {
        OpenString::None => {}
        // `"args":{"path":"a.txt","content":""` — nothing lost but the closers.
        OpenString::Empty => out.push('"'),
        OpenString::Content => return fragment.to_string(),
    }
    for open in stack.iter().rev() {
        out.push(if *open == b'{' { '}' } else { ']' });
    }
    out
}

enum OpenString {
    None,
    Empty,
    Content,
}

fn open_string_tail(text: &str) -> OpenString {
    let bytes = text.as_bytes();
    let mut in_string = false;
    let mut opened_at = 0usize;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
        } else if *byte == b'"' {
            in_string = true;
            opened_at = index;
        }
    }
    if !in_string {
        return OpenString::None;
    }
    if text[opened_at + 1..].is_empty() {
        OpenString::Empty
    } else {
        OpenString::Content
    }
}

/// Reads a Swarm tool request out of rendered chat text.
fn parse_tool_calls(text: &str, request_id: &str) -> Result<Vec<NativeToolCall>, ProviderError> {
    let mut remaining = text;
    let mut value = None;
    while let Some(start) = remaining.find("```") {
        let after_open = &remaining[start + 3..];
        let Some(end) = after_open.find("```") else { break };
        let block = after_open[..end].trim();
        // Rendered code blocks may lose their language tag, and the model
        // sometimes adds punctuation just outside the JSON object.
        if let (Some(first), Some(last)) = (block.find('{'), block.rfind('}')) {
            if first <= last {
                let inner = &block[first..=last];
                match serde_json::from_str::<Value>(inner) {
                    Ok(parsed)
                        if parsed.get("tool").is_some() || parsed.get("tool_calls").is_some() =>
                    {
                        value = Some(parsed);
                        break;
                    }
                    Err(_) if inner.contains("\"tool\"") || inner.contains("\"tool_calls\"") => {
                        return Err(ProviderError::Malformed(
                            "chat tool block is not valid JSON".into(),
                        ));
                    }
                    _ => {}
                }
            }
        }
        remaining = &after_open[end + 3..];
    }
    let value = match value {
        Some(value) => value,
        // Rendered 2026 chat often keeps only the fence's language label, so the
        // reply arrives as `swarm{"tool":…}` with no backticks to find.
        None => match scan_tool_json(text) {
            Some(parsed) => parsed,
            None => return Ok(Vec::new()),
        },
    };
    let values: Vec<&Value> = if let Some(tool) = value.get("tool") {
        vec![tool]
    } else if let Some(calls) = value.get("tool_calls").and_then(Value::as_array) {
        calls.iter().collect()
    } else {
        return Ok(Vec::new()); // planner and finding blocks are not tool calls
    };
    values
        .into_iter()
        .enumerate()
        .map(|(index, call)| {
            let name = call
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| ProviderError::Malformed("chat tool call has no name".into()))?;
            let args = call
                .get("arguments")
                .or_else(|| call.get("input"))
                .filter(|v| v.is_object())
                .ok_or_else(|| {
                    ProviderError::Malformed("chat tool call needs object arguments".into())
                })?;
            Ok(NativeToolCall::new(
                format!("{request_id}-{index}"),
                name,
                args.clone(),
            ))
        })
        .collect()
}

#[derive(Debug, Deserialize, Serialize)]
struct BridgeError {
    error: String,
}

async fn checked(response: reqwest::Response) -> Result<reqwest::Response, ProviderError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let message = response
        .json::<BridgeError>()
        .await
        .map(|body| body.error)
        .unwrap_or_else(|_| "browser bridge request failed".into());
    let class = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ProviderErrorClass::Auth,
        StatusCode::TOO_MANY_REQUESTS => ProviderErrorClass::QuotaExhausted,
        s if s.is_server_error() => ProviderErrorClass::Transient,
        _ => ProviderErrorClass::Permanent,
    };
    Err(ProviderError::Api {
        status: status.as_u16(),
        message,
        class,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::message::ChatMessage;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn request() -> SendRequest {
        let mut request = SendRequest::new(vec![
            ChatMessage::system("follow the task"),
            ChatMessage::user("make a file"),
            ChatMessage::assistant("calling a tool"),
            ChatMessage::tool("c1", "file written"),
        ]);
        request.tool_protocol = ToolProtocol::Text;
        request
    }

    #[test]
    fn prompt_sends_only_new_non_assistant_turns() {
        let request = request();
        let first = format_prompt(&request, 0, ChatSite::ChatGpt);
        assert!(first.contains("follow the task"));
        assert!(first.contains("make a file"));
        let second = format_prompt(&request, 2, ChatSite::ChatGpt);
        assert!(!second.contains("make a file"));
        assert!(!second.contains("calling a tool"));
        assert!(second.contains("file written"));
    }

    fn step(n: usize, body: usize) -> Vec<ChatMessage> {
        vec![
            ChatMessage::assistant_with_tool_calls(
                "",
                vec![swarm_core::message::MessageToolCall {
                    id: format!("c{n}"),
                    name: "bash".into(),
                    arguments: json!({"command": format!("run step {n}")}),
                }],
            ),
            ChatMessage::tool(format!("c{n}"), format!("BEGIN{n}BEGIN {} END{n}END", "x".repeat(body))),
        ]
    }

    fn convo(steps: usize, body: usize) -> SendRequest {
        let mut messages = vec![ChatMessage::system("be careful"), ChatMessage::user("GOAL-TEXT fix issue 7")];
        for n in 1..=steps {
            messages.extend(step(n, body));
        }
        let mut request = SendRequest::new(messages);
        request.tool_protocol = ToolProtocol::Text;
        request
    }

    #[test]
    fn a_short_conversation_is_sent_whole_every_time() {
        let prompt = compile_prompt(&convo(2, 200), ChatSite::ChatGpt, 16_000);
        for needle in ["GOAL-TEXT", "END1END", "END2END", "SWARM PROTOCOL"] {
            assert!(prompt.contains(needle), "{needle} missing");
        }
        assert!(!prompt.contains("EARLIER STEPS"), "nothing to condense yet");
    }

    #[test]
    fn a_long_conversation_condenses_old_steps_and_keeps_what_matters() {
        let mut request = convo(30, 2_500);
        // The agent's own checklist, early on: it must survive condensing.
        request.messages.insert(
            2,
            ChatMessage::assistant_with_tool_calls(
                "",
                vec![swarm_core::message::MessageToolCall {
                    id: "todo".into(),
                    name: "todo_write".into(),
                    arguments: json!({"todos": [{"content": "PIN-ME", "status": "pending"}]}),
                }],
            ),
        );
        request.messages.insert(3, ChatMessage::tool("todo", "Todo list updated"));
        let prompt = compile_prompt(&request, ChatSite::ChatGpt, 12_000);
        let full: usize = request.messages.iter().map(|m| m.content.len()).sum();
        assert!(prompt.chars().count() < full / 3, "{} chars vs {full} raw", prompt.chars().count());
        assert!(prompt.contains("GOAL-TEXT") && prompt.contains("be careful"), "the task stays whole");
        assert!(prompt.contains("END30END"), "the newest result stays in full");
        assert!(prompt.contains("EARLIER STEPS, condensed"), "older steps are condensed");
        assert!(prompt.contains("PIN-ME"), "the latest checklist is kept verbatim");
        assert!(!prompt.contains("END2END"), "an old result body is not carried");
        assert!(prompt.contains("run step 2"), "but the call that made it is still listed");
        assert!(prompt.ends_with(TOOL_CONTRACT), "the contract is the last thing read");
    }

    #[test]
    fn the_prompt_depends_only_on_the_transcript() {
        // Stateless: the same history compiles to the same prompt, so a retry or a
        // fresh chat sees exactly what the first attempt saw.
        let request = convo(12, 1_800);
        assert_eq!(
            compile_prompt(&request, ChatSite::ChatGpt, 10_000),
            compile_prompt(&request, ChatSite::ChatGpt, 10_000)
        );
    }

    #[test]
    fn a_call_without_tools_gets_no_tool_contract() {
        // The /goal judge asks for one line of JSON; the tool contract asks for a
        // fenced swarm block. Sending both gets an empty reply from the site.
        let mut request = SendRequest::new(vec![
            ChatMessage::system("Answer with exactly one line of JSON"),
            ChatMessage::user("Goal: done?"),
        ]);
        request.tool_protocol = ToolProtocol::None;
        let prompt = format_prompt(&request, 0, ChatSite::ChatGpt);
        assert!(prompt.contains("Answer with exactly one line of JSON"));
        assert!(prompt.contains("Goal: done?"));
        assert!(!prompt.contains("SWARM PROTOCOL"), "{prompt}");
        assert!(!prompt.contains("```swarm"), "{prompt}");
        // An agent turn keeps the contract.
        let tooled = format_prompt(&self::request(), 0, ChatSite::ChatGpt);
        assert!(tooled.contains("SWARM PROTOCOL") && tooled.contains("```swarm"));
    }

    #[test]
    fn tool_block_becomes_validated_native_call() {
        let text = "```swarm\n{\"tool\":{\"name\":\"write_file\",\"arguments\":{\"path\":\"a.txt\",\"content\":\"hi\"}}}\n```";
        let calls = parse_tool_calls(text, "request-1").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "write_file");
        assert_eq!(calls[0].arguments["path"], "a.txt");
        assert!(
            parse_tool_calls("```swarm\n{\"plan\":{\"steps\":[]}}\n```", "r")
                .unwrap()
                .is_empty()
        );
        let copied = "```\n{\"tool\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"x.txt\"}}}\n```";
        assert_eq!(parse_tool_calls(copied, "r").unwrap()[0].name, "read_file");
        let malformed = "```\n{\"tool\":{\"name\":\"read_file\",\"arguments\":\"x.txt\"}}\n```";
        assert!(matches!(
            parse_tool_calls(malformed, "r"),
            Err(ProviderError::Malformed(_))
        ));
    }

    #[test]
    fn bridge_must_stay_on_loopback() {
        let config = BrowserChatConfig::chatgpt("https://chatgpt.com", "auto");
        assert!(matches!(
            BrowserChatProvider::new(ProviderId::new("chatgpt-web").unwrap(), config),
            Err(ProviderError::Session(_))
        ));
    }

    #[test]
    fn each_site_reports_its_own_kind_and_default_port() {
        assert_eq!(ChatSite::ChatGpt.kind(), ProviderKind::ChatGptWeb);
        assert_eq!(ChatSite::Gemini.kind(), ProviderKind::GeminiWeb);
        assert_eq!(ChatSite::ChatGpt.default_bridge_url(), "http://127.0.0.1:8766");
        assert_eq!(ChatSite::Gemini.default_bridge_url(), "http://127.0.0.1:8765");
    }

    async fn provider_for(server: &MockServer, site: ChatSite) -> BrowserChatProvider {
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "remote-1"})))
            .expect(1)
            .mount(server)
            .await;
        BrowserChatProvider::new(
            ProviderId::new("chat").unwrap(),
            BrowserChatConfig::new(site, server.uri(), "auto"),
        )
        .unwrap()
    }

    #[test]
    fn tool_request_survives_a_lost_fence() {
        // Rendered 2026 chat keeps the fence's language label and drops the
        // backticks, so the reply arrives as `swarm{"tool":…}`.
        let text = "swarm{\"tool\":{\"name\":\"write_file\",\"arguments\":{\"path\":\"a.txt\",\"content\":\"x\"}}}";
        let calls = parse_tool_calls(text, "req").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "write_file");
    }

    #[test]
    fn braces_inside_a_string_do_not_end_the_object() {
        let text = "Sure! swarm{\"tool\":{\"name\":\"write_file\",\"arguments\":{\"path\":\"a.txt\",\"content\":\"}{ not the end\"}}}";
        let calls = parse_tool_calls(text, "req").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "write_file");
    }

    #[test]
    fn tool_request_survives_a_dropped_closing_brace() {
        // Captured live: the site rendered the code block one character short,
        // `…"status":"pending"}}` where the source had `]}}}`. Losing that call
        // ended a 20-minute run as "completed" with the work half done.
        let text = "swarm{\"tool\":{\"name\":\"todo_write\",\"arguments\":{\"todos\":[{\"content\":\"Identify the fixture-v1 composition\",\"activeForm\":\"Checking docs\",\"status\":\"in_progress\"},{\"content\":\"Run go build ./... and go test ./cmd/coalign-analyze/...\",\"status\":\"pending\"}]}}";
        let calls = parse_tool_calls(text, "req").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "todo_write");
        let todos = calls[0].arguments["todos"].as_array().unwrap();
        assert_eq!(todos.len(), 2);
    }

    #[test]
    fn a_payload_cut_mid_string_is_not_repaired() {
        // The same one-character budget must not invent a closing quote inside a
        // file body: that would write a silently truncated file.
        let text = "swarm{\"tool\":{\"name\":\"write_file\",\"arguments\":{\"path\":\"a.txt\",\"content\":\"package main\\nfunc main() { fmt.Println(";
        assert!(parse_tool_calls(text, "req").unwrap().is_empty());
    }

    #[test]
    fn prose_and_plans_are_not_tool_calls() {
        assert!(parse_tool_calls("I created the file for you.", "req")
            .unwrap()
            .is_empty());
        assert!(parse_tool_calls("{\"plan\":{\"steps\":[]}}", "req")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn fenced_request_still_wins_and_parses() {
        let text = "```swarm\n{\"tool\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.txt\"}}}\n```";
        let calls = parse_tool_calls(text, "req").unwrap();
        assert_eq!(calls[0].name, "read_file");
    }

    #[tokio::test]
    async fn chatgpt_session_reports_usage_from_the_bridge() {
        let server = MockServer::start().await;
        let provider = provider_for(&server, ChatSite::ChatGpt).await;
        let tool = "```swarm\n{\"tool\":{\"name\":\"write_file\",\"arguments\":{\"path\":\"x.txt\",\"content\":\"hi\"}}}\n```";
        Mock::given(method("POST"))
            .and(path("/sessions/remote-1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "text": tool,
                "chat_url": "https://chatgpt.com/c/abc",
                "model": "gpt-5.1",
                "usage": {"input_tokens": 120, "output_tokens": 30, "cached_input_tokens": 0}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/sessions/remote-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"closed": true})))
            .expect(1)
            .mount(&server)
            .await;

        let session = provider.start_session(SessionConfig::default()).await.unwrap();
        let response = session.send(request()).await.unwrap();
        assert_eq!(provider.kind(), ProviderKind::ChatGptWeb);
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
        assert_eq!(response.tool_calls[0].name, "write_file");
        assert_eq!(response.usage.input_tokens, 120);
        assert_eq!(response.usage.output_tokens, 30);
        session.close().await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
        let prompt = body["prompt"].as_str().unwrap();
        assert!(prompt.contains("follow policy") || prompt.contains("follow the task"));
        assert!(prompt.contains("make a file"));
    }

    #[tokio::test]
    async fn gemini_session_without_usage_still_works() {
        let server = MockServer::start().await;
        let provider = provider_for(&server, ChatSite::Gemini).await;
        Mock::given(method("POST"))
            .and(path("/sessions/remote-1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "text": "hello from the page"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let session = provider.start_session(SessionConfig::default()).await.unwrap();
        let response = session.send(request()).await.unwrap();
        assert_eq!(provider.kind(), ProviderKind::GeminiWeb);
        assert_eq!(response.finish_reason, FinishReason::Stop);
        // The site reported no usage, so the figure is an estimate, not zero.
        assert!(response.usage.input_tokens > 0 && response.usage.output_tokens > 0, "{:?}", response.usage);
        assert!(response.text.contains("hello from the page"));
    }

    #[test]
    fn a_summary_replaces_the_digest_of_the_steps_it_covers() {
        let (prompt, first_kept) = compile_with_summary(
            &convo(30, 2_500),
            ChatSite::ChatGpt,
            12_000,
            Some((10, "SUMMARY-TEXT: edited a.go and ran its tests")),
        );
        assert!(prompt.contains("SUMMARY OF EARLIER WORK") && prompt.contains("SUMMARY-TEXT"), "{prompt}");
        // Steps 1-5 are messages 0..10 and are covered; step 8 is not.
        assert!(!prompt.contains("run step 3\"}"), "a covered step is not repeated");
        assert!(prompt.contains("run step 8\"}"), "a step after the summary is still listed");
        assert!(first_kept > 10, "the window moved past the summary: {first_kept}");
        assert!(prompt.contains("END30END") && prompt.contains("GOAL-TEXT"));
    }

    #[test]
    fn a_full_instruction_file_in_the_system_message_is_not_clipped() {
        let instructions = format!("INSTRUCTIONS-START {} INSTRUCTIONS-END", "rule. ".repeat(1_300));
        assert!(instructions.len() > 7_000 && instructions.len() <= 8_100);
        let mut request = convo(1, 100);
        request.messages[0] = ChatMessage::system(format!("base prompt\n\n{instructions}"));
        let prompt = compile_prompt(&request, ChatSite::ChatGpt, 16_000);
        assert!(prompt.contains("INSTRUCTIONS-START") && prompt.contains("INSTRUCTIONS-END"));
        assert!(!prompt.contains("characters omitted"), "nothing clipped");
    }

    #[tokio::test]
    async fn old_steps_are_summarized_by_the_model_and_the_summary_is_reused() {
        let server = MockServer::start().await;
        let provider = provider_for(&server, ChatSite::ChatGpt).await;
        // The summary request is the one that says it is compacting notes.
        Mock::given(method("POST"))
            .and(path("/sessions/remote-1/messages"))
            .and(wiremock::matchers::body_string_contains("compacting an agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": "SUMMARY-FROM-MODEL: wrote 3 files"})))
            .expect(1)
            .mount(&server)
            .await;
        let tool = "```swarm\n{\"tool\":{\"name\":\"bash\",\"arguments\":{\"command\":\"ls\"}}}\n```";
        Mock::given(method("POST"))
            .and(path("/sessions/remote-1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": tool})))
            .mount(&server)
            .await;
        let session = provider.start_session(SessionConfig::default()).await.unwrap();

        let first = session.send(convo(40, 2_500)).await.unwrap();
        assert_eq!(first.finish_reason, FinishReason::ToolCalls);
        assert!(first.usage.input_tokens > 2_000, "summary cost is counted: {:?}", first.usage);
        // One more step: not enough has fallen out of the window to ask again.
        session.send(convo(41, 2_500)).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let bodies: Vec<Value> = requests
            .iter()
            .filter(|r| r.url.path().ends_with("/messages"))
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        assert_eq!(bodies.len(), 3, "summary + two steps");
        assert!(bodies[0]["prompt"].as_str().unwrap().contains("compacting an agent"));
        for step in &bodies[1..] {
            assert_eq!(step["fresh"], true, "every call is its own chat");
            let prompt = step["prompt"].as_str().unwrap();
            assert!(prompt.contains("SUMMARY-FROM-MODEL"), "the summary rides along: {prompt}");
            assert!(prompt.contains("SWARM PROTOCOL"));
        }
        assert!(bodies[1]["prompt"].as_str().unwrap().chars().count() < 30_000);
    }

    #[tokio::test]
    async fn a_failed_summary_falls_back_to_the_digest() {
        let server = MockServer::start().await;
        let provider = provider_for(&server, ChatSite::ChatGpt).await;
        Mock::given(method("POST"))
            .and(path("/sessions/remote-1/messages"))
            .and(wiremock::matchers::body_string_contains("compacting an agent"))
            .respond_with(ResponseTemplate::new(502).set_body_json(json!({"error": "empty reply"})))
            .mount(&server)
            .await;
        let tool = "```swarm\n{\"tool\":{\"name\":\"bash\",\"arguments\":{\"command\":\"ls\"}}}\n```";
        Mock::given(method("POST"))
            .and(path("/sessions/remote-1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text": tool})))
            .mount(&server)
            .await;
        let session = provider.start_session(SessionConfig::default()).await.unwrap();
        let response = session.send(convo(40, 2_500)).await.unwrap();
        assert_eq!(response.finish_reason, FinishReason::ToolCalls, "the step still ran");
        let requests = server.received_requests().await.unwrap();
        let last: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        let prompt = last["prompt"].as_str().unwrap();
        assert!(prompt.contains("EARLIER STEPS, condensed") && !prompt.contains("SUMMARY OF EARLIER WORK"));
    }

    #[tokio::test]
    async fn bridge_rate_limit_maps_to_quota_exhausted() {
        let server = MockServer::start().await;
        let provider = provider_for(&server, ChatSite::ChatGpt).await;
        Mock::given(method("POST"))
            .and(path("/sessions/remote-1/messages"))
            .respond_with(
                ResponseTemplate::new(429).set_body_json(json!({"error": "You have no more messages"})),
            )
            .mount(&server)
            .await;
        let session = provider.start_session(SessionConfig::default()).await.unwrap();
        let error = session.send(request()).await.unwrap_err();
        assert!(matches!(
            error,
            ProviderError::Api { status: 429, class: ProviderErrorClass::QuotaExhausted, .. }
        ));
    }
}
