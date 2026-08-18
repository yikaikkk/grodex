//! SamplingClient — HTTP transport for model requests.
//!
//! Phase 1: simple reqwest wrapper that POSTs to the Responses endpoint
//! and returns an SSE byte stream. Phase 3 will add retry, auth refresh,
//! and WebSocket fallback.

use grodex_provider::binding::ModelBinding;
use grodex_provider::canonical_request::CanonicalModelRequest;
use grodex_core::context::ContextItem;
use grodex_provider::{ProviderError, WireProtocol};
use std::time::Duration;

/// Configuration for the HTTP client.
#[derive(Debug, Clone)]
pub struct SamplingClientConfig {
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
    pub user_agent: String,
    pub api_key: Option<String>,
    pub endpoint: Option<String>,
}

impl Default for SamplingClientConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(300),
            connect_timeout: Duration::from_secs(30),
            user_agent: format!("grodex/{}", env!("CARGO_PKG_VERSION")),
            api_key: None,
            endpoint: None,
        }
    }
}

/// Lightweight HTTP client for model sampling.
///
/// In Phase 1, this handles:
///   - Encoding a CanonicalModelRequest to the Responses API format
///   - POSTing to the provider endpoint
///   - Returning an SSE byte stream
///
/// It does NOT handle retry, auth refresh, or failover (Phase 3).
#[derive(Clone)]
pub struct SamplingClient {
    inner: reqwest::Client,
    #[allow(dead_code)]
    config: SamplingClientConfig,
}

impl SamplingClient {
    /// Create a new SamplingClient.
    pub fn new(config: SamplingClientConfig) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .user_agent(&config.user_agent)
            .build()
            .map_err(|e| ProviderError::Internal(format!("failed to build client: {e}")))?;

        Ok(Self { inner: client, config })
    }

    /// Send a canonical request and receive a byte stream.
    ///
    /// Returns a stream of raw bytes (SSE `data:` lines). The caller
    /// is responsible for creating a StreamingDecoder and feeding bytes in.
    ///
    /// # Errors
    /// Returns `ProviderError` for connection failures, HTTP errors,
    /// or unsupported wire protocols.
    pub async fn stream_raw(
        &self,
        binding: &ModelBinding,
        request: &CanonicalModelRequest,
    ) -> Result<impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>>, ProviderError> {
        let base = self.config.endpoint.as_deref().unwrap_or("https://api.openai.com/v1");
        let (url, body) = match binding.wire_protocol {
            WireProtocol::Responses => (format!("{base}/responses"), self.build_responses_body(binding, request)),
            WireProtocol::ChatCompletions => (format!("{base}/chat/completions"), self.build_chat_body(binding, request)),
            WireProtocol::Messages => (format!("{base}/messages"), self.build_messages_body(binding, request)),
        };

        let mut req = self.inner.post(&url).header("Content-Type", "application/json");

        // Add auth header if configured.
        if let Some(ref key) = self.config.api_key {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        let response = req.json(&body).send().await.map_err(|e| {
            if e.is_timeout() {
                ProviderError::Transport {
                    message: format!("request timeout: {e}"),
                }
            } else if e.is_connect() {
                ProviderError::Transport {
                    message: format!("connection failed: {e}"),
                }
            } else {
                ProviderError::Transport {
                    message: format!("request failed: {e}"),
                }
            }
        })?;

        // Check for HTTP errors.
        let status = response.status();
        if !status.is_success() {
            let status_code = status.as_u16();
            let message = response.text().await.unwrap_or_default();
            return match status_code {
                401 | 403 => Err(ProviderError::Auth {
                    message,
                    status_code: Some(status_code),
                }),
                429 => Err(ProviderError::RateLimited {
                    retry_after_secs: None,
                    message,
                }),
                _ => Err(ProviderError::Api {
                    status_code,
                    message,
                    retry_after_secs: None,
                }),
            };
        }

        // Return the raw byte stream.
        Ok(response.bytes_stream())
    }

    /// Build the Responses API request body from a canonical request.
    ///
    /// Maps ContextItems to the Responses API format.
    /// Phase 1: simple mapping for text-only items.
    fn build_responses_body(&self, binding: &ModelBinding, request: &CanonicalModelRequest) -> serde_json::Value {
        let mut input: Vec<serde_json::Value> = Vec::new();

        // Map instructions.
        for inst in &request.instructions {
            input.push(serde_json::json!({
                "role": match inst.role {
                    grodex_provider::canonical_request::InstructionRole::System => "system",
                    grodex_provider::canonical_request::InstructionRole::Developer => "developer",
                },
                "content": inst.content,
            }));
        }

        // Map context items.
        for item in &request.context_items {
            let mapped = self.map_context_item(item);
            if let Some(m) = mapped {
                input.push(m);
            }
        }

        // Map tools.
        let tools: Vec<serde_json::Value> = request
            .tool_specs
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                })
            })
            .collect();

        let mut body = serde_json::json!({
            "model": binding.model_id,
            "input": input,
        });

        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools);
            body["tool_choice"] = match &request.tool_choice {
                grodex_provider::canonical_request::ToolChoice::Auto => {
                    serde_json::json!("auto")
                }
                grodex_provider::canonical_request::ToolChoice::Required { name } => {
                    serde_json::json!({"type": "function", "name": name})
                }
                grodex_provider::canonical_request::ToolChoice::None => {
                    serde_json::json!("none")
                }
            };
        }

        if let Some(max_tokens) = request.max_output_tokens {
            body["max_output_tokens"] = serde_json::json!(max_tokens);
        }

        body
    }

    /// Map a single ContextItem to the Responses API input format.
    fn map_context_item(&self, item: &grodex_core::context::ContextItem) -> Option<serde_json::Value> {
        use grodex_core::context::ContextItem;
        match item {
            ContextItem::System { content } => Some(serde_json::json!({
                "role": "system",
                "content": content,
            })),
            ContextItem::Developer { content } => Some(serde_json::json!({
                "role": "developer",
                "content": content,
            })),
            ContextItem::User { content, .. } => Some(serde_json::json!({
                "role": "user",
                "content": content,
            })),
            ContextItem::Assistant { content } => Some(serde_json::json!({
                "role": "assistant",
                "content": content,
            })),
            ContextItem::ToolCall {
                call_id,
                name,
                arguments,
            } => Some(serde_json::json!({
                "type": "function_call",
                "call_id": call_id.to_string(),
                "name": name,
                "arguments": arguments.to_string(),
            })),
            ContextItem::ToolResult {
                call_id,
                content,
                is_error: _is_error,
            } => Some(serde_json::json!({
                "type": "function_call_output",
                "call_id": call_id.to_string(),
                "output": content,
            })),
            // Items that cannot be losslessly mapped: emit as developer message
            // so context is preserved rather than silently dropped.
            ContextItem::CompactionSummary { summary, .. } => Some(serde_json::json!({
                "role": "developer",
                "content": format!("[Previous conversation summary]:\n{summary}"),
            })),
            ContextItem::ReasoningSummary { content } => Some(serde_json::json!({
                "role": "developer",
                "content": format!("[Previous reasoning]:\n{content}"),
            })),
            ContextItem::ImagePlaceholder { mime_type, artifact_ref } => Some(serde_json::json!({
                "role": "user",
                "content": format!("[Image: {mime_type}, ref: {artifact_ref}]"),
            })),
        }
    }

    /// Map a ContextItem to Chat Completions API format.
    /// Chat API only supports roles: system, user, assistant, tool.
    /// Tool calls go inside the assistant message, not as separate messages.
    fn map_chat_item(&self, item: &ContextItem) -> Option<serde_json::Value> {
        match item {
            ContextItem::System { content } | ContextItem::Developer { content } => {
                Some(serde_json::json!({"role": "system", "content": content}))
            }
            ContextItem::User { content, .. } => {
                Some(serde_json::json!({"role": "user", "content": content}))
            }
            ContextItem::Assistant { content } => {
                Some(serde_json::json!({"role": "assistant", "content": content}))
            }
            ContextItem::ToolCall { call_id, name, arguments } => {
                // Tool calls go inside the assistant message, not as standalone.
                // Return an assistant message with tool_calls array.
                Some(serde_json::json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": call_id.to_string(),
                        "type": "function",
                        "function": {"name": name, "arguments": arguments.to_string()}
                    }]
                }))
            }
            ContextItem::ToolResult { call_id, content, is_error: _ } => {
                Some(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": call_id.to_string(),
                    "content": content,
                }))
            }
            ContextItem::CompactionSummary { summary, .. } => {
                Some(serde_json::json!({"role": "system", "content": format!("[Previous conversation summary]:\n{summary}")}))
            }
            ContextItem::ReasoningSummary { content } => {
                Some(serde_json::json!({"role": "assistant", "content": format!("[Previous reasoning]:\n{content}")}))
            }
            ContextItem::ImagePlaceholder { mime_type, artifact_ref } => {
                Some(serde_json::json!({"role": "user", "content": format!("[Image: {mime_type}, ref: {artifact_ref}]")}))
            }
        }
    }

    /// Build Chat Completions API body.
    fn build_chat_body(&self, _binding: &ModelBinding, request: &CanonicalModelRequest) -> serde_json::Value {
        let mut messages: Vec<serde_json::Value> = Vec::new();
        // Add system instructions first.
        for inst in &request.instructions {
            messages.push(serde_json::json!({"role": "system", "content": inst.content}));
        }
        // Add context items, merging consecutive ToolCall items into a
        // single assistant message's tool_calls array.
        //
        // ChatCompletions protocol requires: an assistant message with
        // tool_calls must be followed by exactly one "role":"tool" message
        // per tool_call_id. If we emit each ToolCall as a separate assistant
        // message, the API rejects with 400 "insufficient tool messages".
        // Parallel tool calls from the same step MUST share one assistant
        // message.
        let mut iter = request.context_items.iter().peekable();
        // Pending reasoning captured from a preceding ReasoningSummary item.
        // It is attached to the next assistant message's `reasoning_content`
        // field, which DeepSeek/Qwen thinking-mode APIs require to be echoed
        // back on multi-turn requests.
        let mut pending_reasoning: Option<String> = None;
        while let Some(item) = iter.next() {
            match item {
                ContextItem::ReasoningSummary { content } => {
                    pending_reasoning = Some(content.clone());
                }
                ContextItem::Assistant { content } => {
                    // Look ahead: merge consecutive ToolCall items into this
                    // assistant message's tool_calls array.
                    let mut tool_calls: Vec<serde_json::Value> = Vec::new();
                    while let Some(ContextItem::ToolCall { call_id, name, arguments }) = iter.peek() {
                        tool_calls.push(serde_json::json!({
                            "id": call_id.to_string(),
                            "type": "function",
                            "function": {"name": name, "arguments": arguments.to_string()}
                        }));
                        iter.next();
                    }
                    let mut msg = serde_json::json!({"role": "assistant", "content": content});
                    if !tool_calls.is_empty() {
                        msg["tool_calls"] = serde_json::Value::Array(tool_calls);
                    }
                    if let Some(r) = pending_reasoning.take() {
                        msg["reasoning_content"] = serde_json::json!(r);
                    }
                    messages.push(msg);
                }
                ContextItem::ToolCall { call_id, name, arguments } => {
                    // ToolCall without a preceding Assistant text item.
                    // Merge all consecutive ToolCall items into one assistant
                    // message with a tool_calls array.
                    let mut tool_calls = vec![serde_json::json!({
                        "id": call_id.to_string(),
                        "type": "function",
                        "function": {"name": name, "arguments": arguments.to_string()}
                    })];
                    while let Some(ContextItem::ToolCall { call_id, name, arguments }) = iter.peek() {
                        tool_calls.push(serde_json::json!({
                            "id": call_id.to_string(),
                            "type": "function",
                            "function": {"name": name, "arguments": arguments.to_string()}
                        }));
                        iter.next();
                    }
                    let mut msg = serde_json::json!({
                        "role": "assistant",
                        "content": null,
                        "tool_calls": tool_calls,
                    });
                    if let Some(r) = pending_reasoning.take() {
                        msg["reasoning_content"] = serde_json::json!(r);
                    }
                    messages.push(msg);
                }
                // All other variants use the 1:1 mapping.
                other => {
                    // Flush any pending reasoning as a developer message so it
                    // is not silently dropped when no assistant message follows.
                    if let Some(r) = pending_reasoning.take() {
                        messages.push(serde_json::json!({
                            "role": "developer",
                            "content": format!("[Previous reasoning]:\n{r}")
                        }));
                    }
                    if let Some(m) = self.map_chat_item(other) {
                        messages.push(m);
                    }
                }
            }
        }
        let tools: Vec<serde_json::Value> = request.tool_specs.iter().map(|t| serde_json::json!({
            "type": "function",
            "function": {"name": t.name, "description": t.description, "parameters": t.parameters}
        })).collect();

        let mut body = serde_json::json!({
            "model": _binding.model_id,
            "messages": messages,
            "stream": true,
        });
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools);
            body["tool_choice"] = match &request.tool_choice {
                grodex_provider::canonical_request::ToolChoice::Auto => serde_json::json!("auto"),
                grodex_provider::canonical_request::ToolChoice::Required { name } => serde_json::json!({"type": "function", "function": {"name": name}}),
                grodex_provider::canonical_request::ToolChoice::None => serde_json::json!("none"),
            };
        }
        if let Some(max_tokens) = request.max_output_tokens {
            body["max_tokens"] = serde_json::json!(max_tokens);
        }
        body
    }

    /// Build Anthropic Messages API body.
    fn build_messages_body(&self, _binding: &ModelBinding, request: &CanonicalModelRequest) -> serde_json::Value {
        let mut messages = Vec::new();
        let mut system = String::new();
        for item in &request.context_items {
            match item {
                ContextItem::System { content } | ContextItem::Developer { content } => {
                    system.push_str(content);
                    system.push('\n');
                }
                _ => {
                    if let Some(mapped) = self.map_context_item(item) {
                        messages.push(mapped);
                    }
                }
            }
        }
        let tools: Vec<serde_json::Value> = request.tool_specs.iter().map(|t| serde_json::json!({
            "name": t.name, "description": t.description, "input_schema": t.parameters
        })).collect();

        let mut body = serde_json::json!({
            "model": _binding.model_id,
            "messages": messages,
            "max_tokens": request.max_output_tokens.unwrap_or(4096),
            "stream": true,
        });
        if !system.is_empty() {
            body["system"] = serde_json::json!(system.trim());
        }
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools);
        }
        body
    }
}
