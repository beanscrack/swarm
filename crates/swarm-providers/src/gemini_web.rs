//! Gemini web chat through Swarm's local browser bridge.
//!
//! The bridge owns one browser page per session. Swarm retains the durable
//! conversation and tool loop; this provider sends only new turns to the page.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream;
use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use swarm_core::budget::BillingMode;
use swarm_core::message::ChatRole;
use swarm_core::provider::{ProviderCapabilities, ProviderErrorClass, ProviderKind};
use swarm_core::{ProviderId, SessionId};

use crate::error::ProviderError;
use crate::pricing::ModelPricing;
use crate::provider::{
    FinishReason, NativeToolCall, Provider, ProviderStream, SendRequest, Session, SessionConfig,
    ToolProtocol, build_chunks,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub struct GeminiWebConfig {
    pub base_url: String,
    pub model: String,
    pub billing: BillingMode,
    pub max_concurrency: u32,
}

impl GeminiWebConfig {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            billing: BillingMode::Subscription,
            max_concurrency: 4,
        }
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
pub struct GeminiWebProvider {
    id: ProviderId,
    config: GeminiWebConfig,
    client: Client,
}

impl GeminiWebProvider {
    pub fn new(id: ProviderId, config: GeminiWebConfig) -> Result<Self, ProviderError> {
        let url = Url::parse(&config.base_url)
            .map_err(|e| ProviderError::Session(format!("invalid Gemini bridge URL: {e}")))?;
        if url.scheme() != "http" || !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")) {
            return Err(ProviderError::Session(
                "Gemini browser bridge must use loopback HTTP".into(),
            ));
        }
        let client = Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        Ok(Self { id, config, client })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.config.base_url.trim_end_matches('/'), path)
    }
}

#[derive(Debug, Deserialize)]
struct SessionCreated {
    id: String,
}

#[async_trait]
impl Provider for GeminiWebProvider {
    fn id(&self) -> &ProviderId { &self.id }
    fn kind(&self) -> ProviderKind { ProviderKind::GeminiWeb }
    fn model(&self) -> &str { &self.config.model }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tools: false,
            json_mode: false,
            reasoning: true,
            managed_agent: false,
            max_output_tokens: None,
            max_concurrency: Some(self.config.max_concurrency),
            notes: Some("Gemini web chat; tool calls use Swarm fenced JSON".into()),
        }
    }
    fn billing(&self) -> BillingMode { self.config.billing }
    fn pricing(&self) -> ModelPricing { ModelPricing::default() }

    async fn start_session(&self, _config: SessionConfig) -> Result<Box<dyn Session>, ProviderError> {
        let response = self.client.post(self.endpoint("/sessions"))
            .json(&json!({"model": self.config.model}))
            .send().await.map_err(|e| ProviderError::Transport(e.to_string()))?;
        let response = checked(response).await?;
        let created: SessionCreated = response.json().await
            .map_err(|e| ProviderError::Malformed(format!("Gemini session: {e}")))?;
        if created.id.is_empty() {
            return Err(ProviderError::Malformed("Gemini session has no ID".into()));
        }
        Ok(Box::new(GeminiWebSession {
            id: SessionId::new(),
            remote_id: created.id,
            provider_id: self.id.clone(),
            model: self.config.model.clone(),
            base_url: self.config.base_url.clone(),
            billing: self.config.billing,
            client: self.client.clone(),
            sent_count: Arc::new(Mutex::new(0)),
        }))
    }
}

#[derive(Debug, Deserialize)]
struct BrowserReply {
    text: String,
    #[allow(dead_code)]
    chat_url: Option<String>,
}

struct GeminiWebSession {
    id: SessionId,
    remote_id: String,
    provider_id: ProviderId,
    model: String,
    base_url: String,
    billing: BillingMode,
    client: Client,
    sent_count: Arc<Mutex<usize>>,
}

impl GeminiWebSession {
    fn endpoint(&self) -> String {
        format!("{}/sessions/{}/messages", self.base_url.trim_end_matches('/'), self.remote_id)
    }
}

#[async_trait]
impl Session for GeminiWebSession {
    fn provider_id(&self) -> &ProviderId { &self.provider_id }
    fn model(&self) -> &str { &self.model }
    fn session_id(&self) -> &SessionId { &self.id }

    async fn stream(&self, request: &SendRequest) -> Result<ProviderStream, ProviderError> {
        if request.zero_cost && !self.billing.is_zero_cost() {
            return Err(ProviderError::Session("Gemini subscription access is unavailable in a zero-cost run".into()));
        }
        let mut sent_count = self.sent_count.lock().await;
        if request.messages.len() < *sent_count {
            return Err(ProviderError::Session("Gemini conversation history moved backwards".into()));
        }
        let prompt = format_prompt(request, *sent_count);
        if prompt.trim().is_empty() {
            return Err(ProviderError::Session("no new Gemini prompt to send".into()));
        }
        let request_id = format!("{}-{}", self.id, request.messages.len());
        let response = self.client.post(self.endpoint())
            .timeout(request.deadline.unwrap_or(DEFAULT_TIMEOUT))
            .json(&json!({"request_id": request_id, "prompt": prompt}))
            .send().await.map_err(|e| ProviderError::Transport(e.to_string()))?;
        let response = checked(response).await?;
        let reply: BrowserReply = response.json().await
            .map_err(|e| ProviderError::Malformed(format!("Gemini reply: {e}")))?;
        if reply.text.trim().is_empty() {
            return Err(ProviderError::Malformed("Gemini returned an empty reply".into()));
        }
        *sent_count = request.messages.len();
        let (calls, finish) = match parse_tool_calls(&reply.text, &request_id) {
            Ok(calls) if calls.is_empty() => (calls, FinishReason::Stop),
            Ok(calls) => (calls, FinishReason::ToolCalls),
            Err(_) => (Vec::new(), FinishReason::MalformedTool),
        };
        let chunks = build_chunks(&reply.text, &calls, finish, None);
        Ok(Box::pin(stream::iter(chunks.into_iter().map(Ok))))
    }

    async fn close(&self) -> Result<(), ProviderError> {
        let endpoint = format!("{}/sessions/{}", self.base_url.trim_end_matches('/'), self.remote_id);
        let response = self.client.delete(endpoint).send().await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        checked(response).await?;
        Ok(())
    }
}

fn format_prompt(request: &SendRequest, sent_count: usize) -> String {
    let mut out = String::new();
    if sent_count == 0 {
        out.push_str("You are the reasoning model for Swarm, an agent runtime. Follow the SYSTEM instructions below. Swarm executes tools and returns their results. To request one tool, reply with only a fenced ```swarm JSON block containing {\"tool\":{\"name\":\"tool_name\",\"arguments\":{...}}}. Otherwise answer normally. Never claim a tool ran until Swarm returns its result.\n");
        if request.tool_protocol == ToolProtocol::Text {
            if let Some(tools) = request.tools_as_text() {
                out.push_str(&tools);
            }
        }
    }
    for message in &request.messages[sent_count..] {
        if sent_count > 0 && message.role == ChatRole::Assistant {
            continue; // already visible in the Gemini chat
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
    out
}

fn parse_tool_calls(text: &str, request_id: &str) -> Result<Vec<NativeToolCall>, ProviderError> {
    let mut remaining = text;
    let mut value = None;
    while let Some(start) = remaining.find("```") {
        let after_open = &remaining[start + 3..];
        let Some(end) = after_open.find("```") else { break; };
        let block = after_open[..end].trim();
        // Gemini's rendered code blocks may lose their language tag, and the
        // model sometimes adds punctuation just outside the JSON object.
        if let (Some(first), Some(last)) = (block.find('{'), block.rfind('}')) {
            if first <= last {
                match serde_json::from_str::<Value>(&block[first..=last]) {
                    Ok(parsed) if parsed.get("tool").is_some() || parsed.get("tool_calls").is_some() => {
                        value = Some(parsed);
                        break;
                    }
                    Err(_) if block[first..=last].contains("\"tool\"") || block[first..=last].contains("\"tool_calls\"") => {
                        return Err(ProviderError::Malformed("Gemini tool block is not valid JSON".into()));
                    }
                    _ => {}
                }
            }
        }
        remaining = &after_open[end + 3..];
    }
    let Some(value) = value else { return Ok(Vec::new()); };
    let values: Vec<&Value> = if let Some(tool) = value.get("tool") {
        vec![tool]
    } else if let Some(calls) = value.get("tool_calls").and_then(Value::as_array) {
        calls.iter().collect()
    } else {
        return Ok(Vec::new()); // planner and finding blocks are not tool calls
    };
    values.into_iter().enumerate().map(|(index, call)| {
        let name = call.get("name").and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ProviderError::Malformed("Gemini tool call has no name".into()))?;
        let args = call.get("arguments").or_else(|| call.get("input"))
            .filter(|v| v.is_object())
            .ok_or_else(|| ProviderError::Malformed("Gemini tool call needs object arguments".into()))?;
        Ok(NativeToolCall::new(format!("{request_id}-{index}"), name, args.clone()))
    }).collect()
}

#[derive(Debug, Deserialize, Serialize)]
struct BridgeError { error: String }

async fn checked(response: reqwest::Response) -> Result<reqwest::Response, ProviderError> {
    let status = response.status();
    if status.is_success() { return Ok(response); }
    let message = response.json::<BridgeError>().await
        .map(|body| body.error)
        .unwrap_or_else(|_| "browser bridge request failed".into());
    let class = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => ProviderErrorClass::Auth,
        StatusCode::TOO_MANY_REQUESTS => ProviderErrorClass::QuotaExhausted,
        s if s.is_server_error() => ProviderErrorClass::Transient,
        _ => ProviderErrorClass::Permanent,
    };
    Err(ProviderError::Api { status: status.as_u16(), message, class })
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::message::ChatMessage;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use wiremock::matchers::{method, path};

    #[test]
    fn prompt_sends_only_new_non_assistant_turns() {
        let mut request = SendRequest::new(vec![
            ChatMessage::system("follow the task"),
            ChatMessage::user("make a file"),
            ChatMessage::assistant("calling a tool"),
            ChatMessage::tool("c1", "file written"),
        ]);
        request.tool_protocol = ToolProtocol::Text;
        let first = format_prompt(&request, 0);
        assert!(first.contains("follow the task"));
        assert!(first.contains("make a file"));
        let second = format_prompt(&request, 2);
        assert!(!second.contains("make a file"));
        assert!(!second.contains("calling a tool"));
        assert!(second.contains("file written"));
    }

    #[test]
    fn tool_block_becomes_validated_native_call() {
        let text = "```swarm\n{\"tool\":{\"name\":\"write_file\",\"arguments\":{\"path\":\"a.txt\",\"content\":\"hi\"}}}\n```";
        let calls = parse_tool_calls(text, "request-1").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "write_file");
        assert_eq!(calls[0].arguments["path"], "a.txt");
        assert!(parse_tool_calls("```swarm\n{\"plan\":{\"steps\":[]}}\n```", "r").unwrap().is_empty());
        let copied = "```\n{\"tool\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"x.txt\"}}}:\n```";
        assert_eq!(parse_tool_calls(copied, "r").unwrap()[0].name, "read_file");
        let malformed = "```\n{\"tool\":{\"name\":\"read_file\",\"arguments\":\"x.txt\"}}\n```";
        assert!(matches!(parse_tool_calls(malformed, "r"), Err(ProviderError::Malformed(_))));
    }

    #[tokio::test]
    async fn bridge_session_sends_prompt_and_parses_tool_call() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/sessions"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id":"remote-1"})))
            .expect(1).mount(&server).await;
        let tool = "```swarm\n{\"tool\":{\"name\":\"write_file\",\"arguments\":{\"path\":\"x.txt\",\"content\":\"hi\"}}}\n```";
        Mock::given(method("POST")).and(path("/sessions/remote-1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"text":tool,"chat_url":"https://gemini.google.com/app/1"})))
            .expect(1).mount(&server).await;
        Mock::given(method("DELETE")).and(path("/sessions/remote-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"closed":true})))
            .expect(1).mount(&server).await;
        let provider = GeminiWebProvider::new(ProviderId::new("gemini-web").unwrap(),
            GeminiWebConfig::new(server.uri(), "4 Argon")).unwrap();
        let session = provider.start_session(SessionConfig::default()).await.unwrap();
        let mut request = SendRequest::new(vec![ChatMessage::system("follow policy"), ChatMessage::user("make x.txt")]);
        request.tool_protocol = ToolProtocol::Text;
        let response = session.send(request).await.unwrap();
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
        assert_eq!(response.tool_calls[0].name, "write_file");
        session.close().await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
        let prompt = body["prompt"].as_str().unwrap();
        assert!(prompt.contains("follow policy"));
        assert!(prompt.contains("make x.txt"));
    }
}
