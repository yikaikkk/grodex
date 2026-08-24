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
            let raw_message = response.text().await.unwrap_or_default();
            // Clean up the message: proxy error pages (openresty, nginx,
            // Cloudflare) return HTML bodies. Strip to a plain-text summary
            // so the user sees a readable message instead of <html> soup.
            let message = if raw_message.trim_start().starts_with('<') {
                // Likely HTML — extract a readable summary.
                if status_code == 413 {
                    "请求体过大 (413 Request Entity Too Large)。上下文可能超出限制，正在尝试压缩后重试…".to_string()
                } else if status_code == 502 {
                    format!("上游服务不可用 (502 Bad Gateway)")
                } else if status_code == 503 {
                    format!("上游服务暂不可用 (503 Service Unavailable)")
                } else if status_code == 504 {
                    format!("上游服务超时 (504 Gateway Timeout)")
                } else {
                    // Generic HTML: strip tags via simple heuristic.
                    let text = strip_html_tags(&raw_message);
                    if text.len() > 300 { format!("{}…", &text[..300]) } else { text }
                }
            } else {
                raw_message
            };
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

        // Map instructions. PREFIX-CACHE AWARE: the FIRST block (the stable
        // system prompt) goes at the head; any trailing blocks are volatile
        // (e.g. per-turn memory RAG) and are emitted AFTER the conversation
        // history so they don't invalidate the cached prefix.
        if let Some(first) = request.instructions.first() {
            input.push(serde_json::json!({
                "role": match first.role {
                    grodex_provider::canonical_request::InstructionRole::System => "system",
                    grodex_provider::canonical_request::InstructionRole::Developer => "developer",
                },
                "content": first.content,
            }));
        }

        // Map context items.
        for item in &request.context_items {
            let mapped = self.map_context_item(item);
            if let Some(m) = mapped {
                input.push(m);
            }
        }

        // Trailing volatile instruction blocks at the END of the input.
        for inst in request.instructions.iter().skip(1) {
            input.push(serde_json::json!({
                "role": match inst.role {
                    grodex_provider::canonical_request::InstructionRole::System => "system",
                    grodex_provider::canonical_request::InstructionRole::Developer => "developer",
                },
                "content": inst.content,
            }));
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
                // Set reasoning_content FIELD for thinking-mode providers.
                Some(serde_json::json!({"role": "assistant", "content": "", "reasoning_content": content}))
            }
            ContextItem::ImagePlaceholder { mime_type, artifact_ref } => {
                Some(serde_json::json!({"role": "user", "content": format!("[Image: {mime_type}, ref: {artifact_ref}]")}))
            }
        }
    }

    /// Build Chat Completions API body.
    fn build_chat_body(&self, _binding: &ModelBinding, request: &CanonicalModelRequest) -> serde_json::Value {
        let mut messages: Vec<serde_json::Value> = Vec::new();
        // PREFIX-CACHE AWARE: only the FIRST instruction block (the stable
        // system prompt) goes at the head. Trailing blocks are volatile
        // (e.g. per-turn memory RAG) and are appended AFTER the conversation
        // history — a changing system message would invalidate the whole
        // cached prefix (system + tools + history).
        if let Some(first) = request.instructions.first() {
            messages.push(serde_json::json!({"role": "system", "content": first.content}));
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
                    // Flush any pending reasoning as an assistant message so it
                    // is not silently dropped when no assistant message follows.
                    // NOTE: must NOT use role "developer" — Chat Completions
                    // endpoints (esp. third-party) reject unknown roles; this
                    // flush path triggers after mid-tool-call interrupts when a
                    // ReasoningSummary is followed directly by a ToolResult.
                    //
                    // CRITICAL: set the `reasoning_content` FIELD (not just
                    // text content). DeepSeek/Qwen thinking-mode APIs require
                    // this field to be echoed back verbatim — putting it in
                    // the message text does NOT satisfy the requirement and
                    // triggers a 400 "reasoning_content must be passed back".
                    if let Some(r) = pending_reasoning.take() {
                        messages.push(serde_json::json!({
                            "role": "assistant",
                            "content": "",
                            "reasoning_content": r,
                        }));
                    }
                    if let Some(m) = self.map_chat_item(other) {
                        messages.push(m);
                    }
                }
            }
        }
        // Trailing volatile instruction blocks (e.g. per-turn memory RAG)
        // go AFTER the conversation history so the stable prefix
        // (system + history) remains cacheable across turns.
        // Chat Completions has a single instruction role: `developer` is
        // mapped to `system` — many endpoints reject unknown roles.
        for inst in request.instructions.iter().skip(1) {
            let role = match inst.role {
                grodex_provider::canonical_request::InstructionRole::System => "system",
                grodex_provider::canonical_request::InstructionRole::Developer => "system",
            };
            messages.push(serde_json::json!({"role": role, "content": inst.content}));
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
        // The FIRST instruction block (stable system prompt) leads the
        // system field. NOTE: instructions were previously dropped entirely
        // in this protocol — the model never saw the system prompt.
        if let Some(first) = request.instructions.first() {
            system.push_str(&first.content);
            system.push('\n');
        }
        for item in &request.context_items {
            match item {
                ContextItem::System { content } | ContextItem::Developer { content } => {
                    system.push_str(content);
                    system.push('\n');
                }
                // Anthropic-native formats — must NOT reuse map_context_item
                // here: it emits Responses-API shapes (function_call /
                // role "developer") that the Messages endpoint rejects.
                ContextItem::ToolCall { call_id, name, arguments } => {
                    messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": [{
                            "type": "tool_use",
                            "id": call_id.to_string(),
                            "name": name,
                            "input": arguments,
                        }]
                    }));
                }
                ContextItem::ToolResult { call_id, content, is_error } => {
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": call_id.to_string(),
                            "content": content,
                            "is_error": is_error,
                        }]
                    }));
                }
                ContextItem::CompactionSummary { summary, .. } => {
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": format!("[Previous conversation summary]:\n{summary}"),
                    }));
                }
                ContextItem::ReasoningSummary { content } => {
                    messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": format!("[Previous reasoning]:\n{content}"),
                    }));
                }
                ContextItem::ImagePlaceholder { mime_type, artifact_ref } => {
                    messages.push(serde_json::json!({
                        "role": "user",
                        "content": format!("[Image: {mime_type}, ref: {artifact_ref}]"),
                    }));
                }
                ContextItem::User { content, .. } => {
                    messages.push(serde_json::json!({"role": "user", "content": content}));
                }
                ContextItem::Assistant { content } => {
                    messages.push(serde_json::json!({"role": "assistant", "content": content}));
                }
            }
        }
        // Trailing volatile instruction blocks (e.g. per-turn memory RAG)
        // go AFTER the history as a user message so the stable prefix
        // (system + tools + history) stays cacheable.
        for inst in request.instructions.iter().skip(1) {
            messages.push(serde_json::json!({
                "role": "user",
                "content": format!("<context>\n{}\n</context>", inst.content),
            }));
        }
        let mut tools: Vec<serde_json::Value> = request.tool_specs.iter().map(|t| serde_json::json!({
            "name": t.name, "description": t.description, "input_schema": t.parameters
        })).collect();
        // Anthropic supports up to 4 cache breakpoints per request.
        // Place an ephemeral breakpoint on the LAST tool so the
        // system + tools prefix is independently cacheable even when
        // messages change every step (tools schemas are the largest
        // static block — often 5k+ tokens).
        if let Some(last) = tools.last_mut() {
            last.as_object_mut().unwrap().insert(
                "cache_control".into(),
                serde_json::json!({"type": "ephemeral"}),
            );
        }

        let mut body = serde_json::json!({
            "model": _binding.model_id,
            "messages": messages,
            "max_tokens": request.max_output_tokens.unwrap_or(4096),
            "stream": true,
        });
        if !system.is_empty() {
            // Block-array form with an explicit cache_control breakpoint:
            // Anthropic caches tools + system up to the breakpoint, so the
            // (stable) system prompt + tool schemas hit the cache every step.
            body["system"] = serde_json::json!([{
                "type": "text",
                "text": system.trim(),
                "cache_control": {"type": "ephemeral"},
            }]);
        }
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools);
        }
        body
    }
}

/// Strip HTML tags from a string, returning plain text.
/// Simple heuristic: remove everything between `<` and `>`, decode
/// common HTML entities, collapse whitespace.
fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => { in_tag = true; result.push(' '); }
            '>' => { in_tag = false; }
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    // Decode common entities.
    result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        // Collapse runs of whitespace into a single space.
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

#[cfg(test)]
mod wire_role_tests {
    use super::*;
    use grodex_core::id::{SessionId, StepId, ToolCallId, TurnId};
    use grodex_provider::canonical_request::{InstructionBlock, InstructionRole, ToolChoice};

    fn test_client() -> SamplingClient {
        SamplingClient::new(SamplingClientConfig::default()).unwrap()
    }

    fn chat_binding() -> ModelBinding {
        ModelBinding::new("p".into(), 1, "m".into(), 1, WireProtocol::ChatCompletions)
    }

    fn messages_binding() -> ModelBinding {
        ModelBinding::new("p".into(), 1, "m".into(), 1, WireProtocol::Messages)
    }

    fn request_with(items: Vec<ContextItem>, instructions: Vec<InstructionBlock>) -> CanonicalModelRequest {
        CanonicalModelRequest {
            request_id: "req-test".into(),
            session_id: SessionId::new(),
            turn_id: TurnId::new(),
            step_id: StepId::new(),
            model_binding_id: grodex_provider::binding::ModelBindingId::new(),
            prompt_snapshot_hash: None,
            instructions,
            context_items: items,
            tool_specs: Vec::new(),
            tool_choice: ToolChoice::Auto,
            parallel_tool_calls: false,
            reasoning_request: None,
            response_format: None,
            max_output_tokens: None,
            provider_state_in: None,
        }
    }

    /// Regression: mid-tool-call interrupt leaves ReasoningSummary directly
    /// followed by ToolResult; the reasoning flush must NOT emit role
    /// "developer" (Chat Completions endpoints reject unknown roles).
    #[test]
    fn chat_body_never_emits_developer_role_after_interrupt() {
        let req = request_with(
            vec![
                ContextItem::User { content: "run tests".into(), message_id: None },
                ContextItem::ReasoningSummary { content: "thinking...".into() },
                ContextItem::ToolResult {
                    call_id: ToolCallId::new(),
                    content: "interrupted".into(),
                    is_error: true,
                },
            ],
            Vec::new(),
        );
        let body = test_client().build_chat_body(&chat_binding(), &req);
        let roles: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert!(!roles.contains(&"developer"), "roles: {roles:?}");
        // The flushed reasoning must still be present (not dropped).
        assert!(roles.contains(&"assistant"));
    }

    /// Regression: trailing volatile instruction blocks with Developer role
    /// must map to "system" on the chat wire.
    #[test]
    fn chat_trailing_developer_instruction_maps_to_system() {
        let req = request_with(
            vec![ContextItem::User { content: "hi".into(), message_id: None }],
            vec![
                InstructionBlock { role: InstructionRole::System, content: "base".into(), priority: 100 },
                InstructionBlock { role: InstructionRole::Developer, content: "rag".into(), priority: 10 },
            ],
        );
        let body = test_client().build_chat_body(&chat_binding(), &req);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs.last().unwrap()["role"], "system", "Developer trailing block must map to system");
    }

    /// Regression: Anthropic Messages wire must use native tool_use /
    /// tool_result shapes and never emit role "developer" or
    /// Responses-style function_call items.
    #[test]
    fn messages_body_uses_anthropic_shapes_only() {
        let call_id = ToolCallId::new();
        let req = request_with(
            vec![
                ContextItem::ToolCall { call_id: call_id.clone(), name: "exec".into(), arguments: serde_json::json!({"cmd": "ls"}) },
                ContextItem::ToolResult { call_id, content: "ok".into(), is_error: false },
                ContextItem::CompactionSummary { summary: "sum".into(), window_number: 1 },
            ],
            vec![InstructionBlock { role: InstructionRole::Developer, content: "base".into(), priority: 100 }],
        );
        let body = test_client().build_messages_body(&messages_binding(), &req);
        let msgs = body["messages"].as_array().unwrap();
        let serialized = serde_json::to_string(&body).unwrap();
        assert!(!serialized.contains("\"developer\""), "no developer role on Messages wire");
        assert!(!serialized.contains("function_call"), "no Responses shapes on Messages wire");
        assert_eq!(msgs[0]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["role"], "user");
    }
}
