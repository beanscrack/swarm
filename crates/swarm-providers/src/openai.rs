//! OpenAI-compatible chat-completions transport.
//!
//! Works with any endpoint speaking the OpenAI chat-completions dialect
//! (OpenAI, OpenRouter, Groq, Together, Ollama, vLLM, LM Studio, ...).
//! Supports non-streaming JSON and SSE streaming (text, incremental
//! tool-call deltas, and a final usage chunk), and maps HTTP failures onto
//! [`ProviderErrorClass`] so the quota gate can cool down on rate limits and
//! stop on quota/auth failures instead of retrying unbounded.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream;
use serde_json::{Value, json};

use swarm_core::budget::{BillingMode, TokenUsage};
use swarm_core::provider::{
    ProviderCapabilities, ProviderErrorClass, ProviderKind, ProviderTextChunk,
};
use swarm_core::tool::ToolSpec;
use swarm_core::{CoreError, ProviderId, SessionId};

use crate::error::ProviderError;
use crate::pricing::ModelPricing;
use crate::provider::{
    FinishReason, NativeToolCall, Provider, ProviderStream, SendRequest, Session, SessionConfig,
    ToolProtocol, build_chunks,
};

type Result<T> = std::result::Result<T, ProviderError>;

/// Configuration for an OpenAI-compatible endpoint.
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    /// Sampling temperature; `0.0` omits the field and uses the server default.
    pub temperature: f32,
    pub max_tokens: Option<u32>,
    /// Whole-call timeout applied to the HTTP client.
    pub timeout_ms: u64,
    pub billing: BillingMode,
    pub pricing: ModelPricing,
    /// Prefer SSE streaming when set.
    pub streaming: bool,
    /// Extra static headers (e.g. a site key for a local server).
    pub extra_headers: Vec<(String, String)>,
    pub chat_template_kwargs: serde_json::Map<String, Value>,
    pub min_output_tokens: Option<u32>,
}

impl OpenAiConfig {
    pub fn new(base_url: &str, model: &str) -> std::result::Result<Self, CoreError> {
        let base = base_url.trim_end_matches('/');
        // Fail at construction if the URL is malformed.
        reqwest::Url::parse(base)
            .map_err(|e| CoreError::Config(format!("invalid base_url `{base}`: {e}")))?;
        Ok(Self {
            base_url: base.to_string(),
            model: model.to_string(),
            api_key: None,
            temperature: 0.0,
            max_tokens: None,
            timeout_ms: 60_000,
            billing: BillingMode::OnDemand,
            pricing: ModelPricing::default(),
            streaming: true,
            extra_headers: Vec::new(),
            chat_template_kwargs: Default::default(),
            min_output_tokens: None,
        })
    }

    pub fn api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }
    pub fn billing(mut self, billing: BillingMode) -> Self {
        self.billing = billing;
        self
    }
    pub fn pricing(mut self, pricing: ModelPricing) -> Self {
        self.pricing = pricing;
        self
    }
    pub fn streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self
    }
    pub fn temperature(mut self, t: f32) -> Self {
        self.temperature = t;
        self
    }
    pub fn max_tokens(mut self, max: Option<u32>) -> Self {
        self.max_tokens = max;
        self
    }
    pub fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout_ms = ms;
        self
    }
    pub fn header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((key.into(), value.into()));
        self
    }
}

/// A provider backend backed by an OpenAI-compatible endpoint.
#[derive(Clone)]
pub struct OpenAiProvider {
    id: ProviderId,
    config: OpenAiConfig,
    client: reqwest::Client,
    caps: ProviderCapabilities,
}

impl OpenAiProvider {
    pub fn new(id: ProviderId, config: OpenAiConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .map_err(|e| ProviderError::Transport(format!("client: {e}")))?;
        let caps = ProviderCapabilities {
            streaming: config.streaming,
            tools: true,
            json_mode: true,
            reasoning: false,
            managed_agent: false,
            max_output_tokens: config.max_tokens,
            max_concurrency: Some(4),
            notes: Some(format!("openai-compatible: {}", config.model)),
        };
        Ok(Self {
            id,
            config,
            client,
            caps,
        })
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAiCompatible
    }
    fn model(&self) -> &str {
        &self.config.model
    }
    fn capabilities(&self) -> ProviderCapabilities {
        self.caps.clone()
    }
    fn billing(&self) -> BillingMode {
        self.config.billing
    }
    fn pricing(&self) -> ModelPricing {
        self.config.pricing
    }
    async fn start_session(&self, config: SessionConfig) -> Result<Box<dyn Session>> {
        Ok(Box::new(OpenAiSession {
            id: self.id.clone(),
            model: self.config.model.clone(),
            client: self.client.clone(),
            config: self.config.clone(),
            session_config: config,
            session_id: SessionId::new(),
        }))
    }
}

/// One conversation with an OpenAI-compatible endpoint.
#[derive(Clone)]
struct OpenAiSession {
    id: ProviderId,
    model: String,
    client: reqwest::Client,
    config: OpenAiConfig,
    session_config: SessionConfig,
    session_id: SessionId,
}

#[async_trait]
impl Session for OpenAiSession {
    fn provider_id(&self) -> &ProviderId {
        &self.id
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    async fn stream(&self, request: &SendRequest) -> Result<ProviderStream> {
        // Defense-in-depth zero-cost gate (the registry also enforces it).
        if request.zero_cost && !self.config.billing.is_zero_cost() {
            return Err(CoreError::ZeroCostViolation {
                provider: self.id.as_str().to_string(),
                billing: self.config.billing.as_str(),
            }
            .into());
        }

        let do_stream = self.config.streaming;
        let body = self.build_body(request, do_stream);

        let req = self.client.post(self.chat_url())
            .header("Accept", if do_stream { "text/event-stream" } else { "application/json" });
        let response = match req
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout(Duration::from_millis(self.config.timeout_ms))
                } else {
                    ProviderError::Transport(e.to_string())
                }
            })? {
            r => r,
        };

        let status = response.status();
        if status.is_client_error() || status.is_server_error() {
            return Err(self.http_error(response).await);
        }

        let text = response
            .text()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        let chunks = if do_stream && text.lines().any(|line| line.starts_with("data:")) {
            parse_sse_body(&text)?
        } else {
            let v: Value = serde_json::from_str(&text)
                .map_err(|e| ProviderError::Malformed(format!("invalid JSON: {e}")))?;
            parse_json_body(&v)
        };
        Ok(Box::pin(stream::iter(chunks.into_iter().map(Ok))))
    }
}

impl OpenAiSession {
    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.config.base_url)
    }

    fn build_body(&self, request: &SendRequest, do_stream: bool) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        if let Some(system) = &self.session_config.system {
            messages.push(json!({"role": "system", "content": system}));
        }
        for m in &request.messages {
            let mut obj = serde_json::Map::new();
            obj.insert("role".to_string(), json!(m.role.as_str()));
            // A pure assistant tool-call turn has no text; OpenAI wants `null` there.
            if m.content.is_empty() && !m.tool_calls.is_empty() {
                obj.insert("content".to_string(), Value::Null);
            } else {
                obj.insert("content".to_string(), json!(m.content));
            }
            if !m.tool_calls.is_empty() {
                let calls: Vec<Value> = m
                    .tool_calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.name,
                                // OpenAI requires `arguments` as a JSON-encoded string.
                                "arguments": c.arguments.to_string(),
                            },
                        })
                    })
                    .collect();
                obj.insert("tool_calls".to_string(), Value::Array(calls));
            }
            if let Some(id) = &m.tool_call_id {
                obj.insert("tool_call_id".to_string(), json!(id));
            }
            messages.push(Value::Object(obj));
        }

        let has_tools = !request.tools.is_empty();
        let tools_as_text =
            has_tools && request.tool_protocol == ToolProtocol::Text;
        if tools_as_text {
            if let Some(t) = request.tools_as_text() {
                messages.push(json!({"role": "system", "content": t}));
            }
        }

        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "stream": do_stream,
        });
        if has_tools && !tools_as_text {
            let tools: Vec<Value> = request.tools.iter().map(openai_tool_schema).collect();
            body["tools"] = json!(tools);
        }
        if do_stream {
            body["stream_options"] = json!({"include_usage": true});
        }
        if !self.config.chat_template_kwargs.is_empty() {
            body["chat_template_kwargs"] = json!(self.config.chat_template_kwargs);
        }
        if let Some(temperature) = request.temperature.or(self.session_config.temperature) {
            body["temperature"] = json!(temperature);
        } else if self.config.temperature > 0.0 {
            body["temperature"] = json!(self.config.temperature);
        }
        if let Some(max) = request.max_tokens.or(self.session_config.max_tokens).or(self.config.max_tokens) {
            body["max_tokens"] = json!(max.max(self.config.min_output_tokens.unwrap_or(0)));
        } else if let Some(min) = self.config.min_output_tokens {
            body["max_tokens"] = json!(min);
        }
        body
    }

    async fn http_error(&self, response: reqwest::Response) -> ProviderError {
        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let body = response.text().await.unwrap_or_default();
        let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let message = parsed
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or(&body)
            .to_string();
        ProviderError::Api {
            status: status.as_u16(),
            message,
            class: classify_http_status(status.as_u16(), retry_after),
        }
    }
}

/// Map an HTTP status to the normalized error class the gate understands.
pub fn classify_http_status(status: u16, retry_after_ms: Option<u64>) -> ProviderErrorClass {
    match status {
        401 | 403 => ProviderErrorClass::Auth,
        429 => ProviderErrorClass::RateLimited { retry_after_ms },
        402 => ProviderErrorClass::QuotaExhausted,
        400 | 404 | 413 | 422 => ProviderErrorClass::Permanent,
        500..=599 => ProviderErrorClass::Transient,
        _ => ProviderErrorClass::Permanent,
    }
}

fn openai_tool_schema(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": spec.name,
            "description": spec.description,
            "parameters": spec.input_schema.as_value(),
        }
    })
}

// ---------------------------------------------------------------------------
// Response parsing (pure, unit-tested)
// ---------------------------------------------------------------------------

/// Parse a non-streaming chat-completions JSON body into normalized chunks.
fn parse_json_body(v: &Value) -> Vec<ProviderTextChunk> {
    let text = v
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut calls = parse_tool_calls(v.pointer("/choices/0/message"));
    // Text tool protocol fallback. An endpoint with no native tool_calls answers the tools
    // prompt (SendRequest::tools_as_text) with a JSON object inside prose; with only the
    // native path, the loop sees no call, spends max_format_retries nagging, and dies - the
    // repair loop standing in for a protocol. Recovering the call lets a tool-call-free
    // server drive the agent. Tolerance rules (fences, key aliases, truncation) and the
    // requirement that a tool NAME be present live in text_tools, so a reply that merely
    // talks about tools produces no call. Streaming parity is a known gap; see
    // docs/TEXT-TOOLS-PLAN.md.
    if calls.is_empty() {
        if let Ok(parsed) = crate::text_tools::parse_tool_call(&text) {
            calls.push(NativeToolCall {
                id: "text-0".to_string(),
                name: parsed.name,
                arguments: parsed.arguments,
            });
        }
    }
    let finish = FinishReason::from_provider_str(
        v.pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            .unwrap_or("stop"),
    );
    let usage = parse_usage(v.get("usage"));
    build_chunks(&text, &calls, finish, usage)
}

/// Parse a full SSE chat-completions body into normalized chunks.
///
/// The body is buffered and parsed whole (rather than streamed token-by-token)
/// so the normalized chunk sequence is identical to the non-streaming path and
/// deterministic. The `Session::send` contract collects the whole response
/// either way.
fn parse_sse_body(body: &str) -> Result<Vec<ProviderTextChunk>> {
    let mut text = String::new();
    let mut tools: BTreeMap<usize, ToolAcc> = BTreeMap::new();
    let mut finish = FinishReason::Stop;
    let mut usage: Option<TokenUsage> = None;

    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if let Some(err_obj) = v.get("error") {
            if !err_obj.is_null() {
                let msg = err_obj
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("provider error")
                    .to_string();
                return Err(ProviderError::Api {
                    status: 200,
                    message: msg,
                    class: ProviderErrorClass::Permanent,
                });
            }
        }
        if let Some(u) = v.get("usage") {
            usage = parse_usage(Some(u));
        }
        if let Some(fr) = v.pointer("/choices/0/finish_reason").and_then(Value::as_str) {
            finish = FinishReason::from_provider_str(fr);
        }
        if let Some(arr) = v.pointer("/choices/0/delta/tool_calls").and_then(Value::as_array) {
            for tc in arr {
                let idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let acc = tools.entry(idx).or_default();
                if let Some(id) = tc.get("id").and_then(Value::as_str) {
                    acc.id = id.to_string();
                }
                if let Some(n) = tc.pointer("/function/name").and_then(Value::as_str) {
                    acc.name.push_str(n);
                }
                if let Some(a) = tc.pointer("/function/arguments").and_then(Value::as_str) {
                    acc.args.push_str(a);
                }
            }
        }
        if let Some(t) = v.pointer("/choices/0/delta/content").and_then(Value::as_str) {
            text.push_str(t);
        }
    }

    let calls: Vec<NativeToolCall> = tools
        .values()
        .map(|a| NativeToolCall::new(a.id.clone(), a.name.clone(), parse_tool_arguments(&a.args)))
        .collect();
    // Streaming parity for the text tool protocol. The non-streaming parser recovers a
    // tool object from prose; a streamed reply must get the same treatment, because swarm
    // asks for SSE by default and a text-protocol endpoint that streams would otherwise
    // produce no call at all - the protocol would only exist where streaming is off.
    let mut calls = calls;
    if calls.is_empty() {
        if let Ok(parsed) = crate::text_tools::parse_tool_call(&text) {
            calls.push(NativeToolCall {
                id: "text-0".to_string(),
                name: parsed.name,
                arguments: parsed.arguments,
            });
        }
    }
    Ok(build_chunks(&text, &calls, finish, usage))
}

#[derive(Default)]
struct ToolAcc {
    id: String,
    name: String,
    args: String,
}

fn parse_tool_calls(message: Option<&Value>) -> Vec<NativeToolCall> {
    let Some(arr) = message.and_then(|m| m.pointer("/tool_calls").and_then(Value::as_array))
    else {
        return Vec::new();
    };
    arr.iter()
        .enumerate()
        .map(|(i, tc)| {
            let id = tc
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{i}"));
            let name = tc
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let args_raw = tc
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("null");
            NativeToolCall::new(id, name, parse_tool_arguments(args_raw))
        })
        .collect()
}

fn parse_tool_arguments(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn parse_usage(u: Option<&Value>) -> Option<TokenUsage> {
    let u = u?;
    Some(TokenUsage {
        input_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
        output_tokens: u.get("completion_tokens").and_then(Value::as_u64).unwrap_or(0),
        cached_input_tokens: u
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::message::ChatMessage;
    use swarm_core::MessageToolCall;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn req(text: &str) -> SendRequest {
        SendRequest::new(vec![ChatMessage::user(text)])
    }

    /// Serve a single canned body at `status` on the chat-completions endpoint.
    async fn serve(status: u16, body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
        server
    }

    async fn provider(uri: &str, streaming: bool) -> OpenAiProvider {
        let cfg = OpenAiConfig::new(&format!("{uri}/v1"), "test-model")
            .unwrap()
            .streaming(streaming);
        OpenAiProvider::new(ProviderId::new("openai-test").unwrap(), cfg).unwrap()
    }

    #[tokio::test]
    async fn non_streaming_parses_text_tools_and_usage() {
        let body = json!({
            "model": "test-model",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "done: ",
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12}
        });
        let body = body.to_string();
        let server = serve(200, &body).await;
        let p = provider(&server.uri(), false).await;
        let session = p.start_session(SessionConfig::default()).await.unwrap();
        let resp = session.send(req("hello")).await.unwrap();
        assert_eq!(resp.text, "done: ");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "bash");
        assert_eq!(resp.tool_calls[0].arguments["command"], "ls");
        assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
        assert_eq!(resp.usage.input_tokens, 5);
        assert_eq!(resp.usage.output_tokens, 7);
    }

    #[tokio::test]
    async fn native_tool_calls_round_trip_on_the_wire() {
        let body = json!({
            "model": "test-model",
            "choices": [{"message": {"role": "assistant", "content": "done"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })
        .to_string();
        let server = serve(200, &body).await;
        let p = provider(&server.uri(), false).await;
        let session = p.start_session(SessionConfig::default()).await.unwrap();

        let messages = vec![
            ChatMessage::user("go"),
            ChatMessage::assistant_with_tool_calls(
                "",
                vec![MessageToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    arguments: serde_json::json!({"path": "hello.txt"}),
                }],
            ),
            ChatMessage::tool("c1", "ok"),
        ];
        session.send(SendRequest::new(messages)).await.unwrap();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let sent: serde_json::Value = received[0].body_json().unwrap();
        let msgs = sent["messages"].as_array().unwrap();
        let assistant = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(assistant["content"], Value::Null);
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "write_file");
        // `arguments` must be a JSON-encoded string, not an inline object.
        let args = &assistant["tool_calls"][0]["function"]["arguments"];
        assert!(args.is_string());
        assert!(args.as_str().unwrap().contains("hello.txt"));
        let tool = msgs.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool["tool_call_id"], "c1");
    }

    #[tokio::test]
    async fn streaming_sse_assembles_text_usage_and_tool_calls() {
        let events = vec![
            json!({"choices": [{"delta": {"content": "he"}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "tc1", "function": {"name": "bash", "arguments": ""}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"command\": \"ls\"}"}}]}}]}),
            json!({"choices": [{"delta": {"content": "llo"}}, {"delta": {"finish_reason": "stop"}}], "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}}),
        ];
        let mut body = String::new();
        for e in &events {
            body.push_str(&format!("data: {e}\n\n"));
        }
        body.push_str("data: [DONE]\n\n");
        let server = serve(200, &body).await;
        let p = provider(&server.uri(), true).await;
        let session = p.start_session(SessionConfig::default()).await.unwrap();
        let resp = session.send(req("hi")).await.unwrap();
        assert_eq!(resp.text, "hello");
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].name, "bash");
        assert_eq!(resp.tool_calls[0].arguments["command"], "ls");
        assert_eq!(resp.usage.input_tokens, 3);
        assert_eq!(resp.usage.output_tokens, 2);
    }

    #[tokio::test]
    async fn rate_limit_is_classified_and_retryable() {
        let server = serve(429, r#"{"error": {"message": "slow down"}}"#).await;
        let p = provider(&server.uri(), false).await;
        let session = p.start_session(SessionConfig::default()).await.unwrap();
        let err = session.send(req("hi")).await.unwrap_err();
        assert!(matches!(
            err,
            ProviderError::Api {
                status: 429,
                class: ProviderErrorClass::RateLimited { .. },
                ..
            }
        ));
        assert!(err.is_retryable());
    }

    #[tokio::test]
    async fn auth_error_is_not_retryable() {
        let server = serve(401, r#"{"error": {"message": "bad key"}}"#).await;
        let p = provider(&server.uri(), false).await;
        let session = p.start_session(SessionConfig::default()).await.unwrap();
        let err = session.send(req("hi")).await.unwrap_err();
        assert!(!err.is_retryable());
        assert_eq!(err.classify(), ProviderErrorClass::Auth);
    }

    #[test]
    fn status_classification_table() {
        use ProviderErrorClass::*;
        assert_eq!(classify_http_status(401, None), Auth);
        assert_eq!(classify_http_status(402, None), QuotaExhausted);
        assert_eq!(
            classify_http_status(429, Some(5)),
            RateLimited {
                retry_after_ms: Some(5)
            }
        );
        assert_eq!(classify_http_status(400, None), Permanent);
        assert_eq!(classify_http_status(503, None), Transient);
    }
    #[tokio::test]
    async fn native_options_and_text_stream_usage_reach_the_wire() {
        let server = serve(200, r#"{"choices":[{"message":{"content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":17,"completion_tokens":3,"total_tokens":20}}"#).await;
        let mut cfg = OpenAiConfig::new(&format!("{}/v1",server.uri()),"swift").unwrap();
        cfg.chat_template_kwargs.insert("enable_thinking".into(),json!(false));
        cfg.min_output_tokens = Some(4096);
        let p = OpenAiProvider::new(ProviderId::new("options").unwrap(), cfg).unwrap();
        let session = p.start_session(SessionConfig::default()).await.unwrap();
        let mut request = req("go"); request.tool_protocol = ToolProtocol::Text; request.max_tokens = Some(100);
        let response = session.send(request).await.unwrap();
        assert_eq!(response.usage.input_tokens,17);
        assert_eq!(response.usage.output_tokens,3);
        let received = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(body["max_tokens"],4096);
        assert_eq!(body["stream_options"]["include_usage"],true);
        assert_eq!(received[0].headers.get("accept").unwrap(),"text/event-stream");
    }

}
