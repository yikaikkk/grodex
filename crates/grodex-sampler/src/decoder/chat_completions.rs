//! ChatCompletionsDecoder — OpenAI Chat Completions SSE → CanonicalModelEvent.
//!
//! Transforms Chat Completions streaming chunks into canonical events.
//! Handles: content delta, tool call delta (per-index accumulation), finish reason.

use super::pending_tool_call::PendingToolCall;
use crate::line_buffer::LineBuffer;
use crate::streaming::{DecoderState, StreamingDecoder};
use grodex_core::id::ToolCallId;
use grodex_provider::canonical_event::{
    CanonicalModelResponse, CanonicalResponseItem, StopReason,
};
use grodex_provider::usage::SettledUsage;
use grodex_provider::{CanonicalModelEvent, ProviderError};
use serde::Deserialize;
use std::collections::BTreeMap;

/// A Chat Completions SSE chunk.
#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: ChatDelta,
    #[serde(default)]
    finish_reason: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    index: u32,
}

#[derive(Debug, Default, Deserialize)]
struct ChatDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChatToolCallDelta>>,
    #[allow(dead_code)]
    #[serde(default)]
    role: Option<String>,
    /// DeepSeek / Qwen reasoning models stream the chain-of-thought here.
    /// It must be captured and replayed as `reasoning_content` on the
    /// assistant message in subsequent multi-turn requests, otherwise the
    /// API rejects with 400 "reasoning_content must be passed back".
    #[serde(default)]
    reasoning_content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatToolCallDelta {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ChatFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct ChatFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

pub struct ChatCompletionsDecoder {
    state: DecoderState,
    request_id: String,
    line_buf: LineBuffer,
    pending_tool_calls: BTreeMap<u32, PendingToolCall>,
    chunk_index: u64,
    text_acc: String,
    /// Accumulated reasoning_content (DeepSeek thinking mode).
    reasoning_acc: String,
    has_semantic: bool,
    finish_reason: Option<String>,
    model: Option<String>,
    usage: Option<ChatUsage>,
}

impl ChatCompletionsDecoder {
    pub fn new(request_id: String) -> Self {
        Self {
            state: DecoderState::Created,
            request_id,
            line_buf: LineBuffer::new(),
            pending_tool_calls: BTreeMap::new(),
            chunk_index: 0,
            text_acc: String::new(),
            reasoning_acc: String::new(),
            has_semantic: false,
            finish_reason: None,
            model: None,
            usage: None,
        }
    }

    fn process_chunk_inner(&mut self, chunk: ChatChunk) -> Vec<CanonicalModelEvent> {
        let mut events = Vec::new();

        if self.state == DecoderState::Created {
            self.state = DecoderState::Started;
        }

        if let Some(ref m) = chunk.model {
            self.model = Some(m.clone());
        }
        if let Some(ref u) = chunk.usage {
            self.usage = Some(ChatUsage { ..*u });
        }

        for choice in chunk.choices {
            if let Some(ref content) = choice.delta.content {
                if !content.is_empty() {
                    self.has_semantic = true;
                    self.state = DecoderState::StreamingText;
                    self.text_acc.push_str(content);
                    self.chunk_index += 1;
                    events.push(CanonicalModelEvent::TextDelta {
                        text: content.clone(),
                        chunk_index: self.chunk_index,
                    });
                }
            }

            // DeepSeek thinking mode: capture reasoning_content deltas and
            // surface them as ReasoningDelta so the TUI can render the
            // thinking panel. The accumulated text is also stored in
            // reasoning_acc so finalize() can attach a ReasoningSummary item
            // for context projection / replay.
            if let Some(ref reasoning) = choice.delta.reasoning_content {
                if !reasoning.is_empty() {
                    self.has_semantic = true;
                    self.reasoning_acc.push_str(reasoning);
                    self.chunk_index += 1;
                    events.push(CanonicalModelEvent::ReasoningDelta {
                        text: reasoning.clone(),
                        chunk_index: self.chunk_index,
                    });
                }
            }

            if let Some(ref tool_calls) = choice.delta.tool_calls {
                self.has_semantic = true;
                self.state = DecoderState::StreamingToolCalls;
                for tc in tool_calls {
                    let entry = self.pending_tool_calls.entry(tc.index).or_insert_with(|| {
                        PendingToolCall::new(
                            tc.index.to_string(),
                            String::new(),
                        )
                    });

                    if let Some(ref id) = tc.id {
                        entry.canonical_tool_call_id =
                            ToolCallId::from_string(id).unwrap_or_else(|_| ToolCallId::new());
                    }

                    if let Some(ref func) = tc.function {
                        if let Some(ref name) = func.name {
                            if !name.is_empty() && entry.name_buffer.is_empty() {
                                entry.name_buffer = name.clone();
                                events.push(CanonicalModelEvent::ToolCallStarted {
                                    call_id: entry.canonical_tool_call_id,
                                    name: name.clone(),
                                    tool_index: tc.index,
                                });
                            }
                        }
                        if let Some(ref args) = func.arguments {
                            entry.append_args(args);
                            events.push(CanonicalModelEvent::ToolCallArgumentsDelta {
                                call_id: entry.canonical_tool_call_id,
                                tool_index: tc.index,
                                arguments_delta: args.clone(),
                            });
                        }
                    }
                }
            }

            if let Some(ref reason) = choice.finish_reason {
                if !reason.is_empty() {
                    self.finish_reason = Some(reason.clone());
                    self.state = DecoderState::Completing;
                }
            }
        }

        events
    }
}

impl StreamingDecoder for ChatCompletionsDecoder {
    fn process_chunk(&mut self, chunk: &[u8]) -> Result<Vec<CanonicalModelEvent>, ProviderError> {
        let lines = self.line_buf.feed(chunk);
        let mut events = Vec::new();

        for line in &lines {
            let data = line.trim();
            if let Some(payload) = data.strip_prefix("data: ") {
                if payload == "[DONE]" {
                    self.state = DecoderState::Completing;
                    break;
                }
                if let Ok(chunk) = serde_json::from_str::<ChatChunk>(payload) {
                    events.extend(self.process_chunk_inner(chunk));
                }
            }
        }

        Ok(events)
    }

    fn finalize(&mut self) -> Result<Vec<CanonicalModelEvent>, ProviderError> {
        if self.is_terminal() {
            return Ok(Vec::new());
        }

        // Build final events.
        let mut events = Vec::new();

        // Process any remaining incomplete line.
        if let Some(remaining) = self.line_buf.flush() {
            if let Some(payload) = remaining.trim().strip_prefix("data: ") {
                if payload != "[DONE]" {
                    if let Ok(chunk) = serde_json::from_str::<ChatChunk>(payload) {
                        events.extend(self.process_chunk_inner(chunk));
                    }
                }
            }
        }

        // Emit completed tool calls.
        for (ti, pending) in &self.pending_tool_calls {
            if !pending.args_buffer.is_empty() {
                events.push(CanonicalModelEvent::ToolCallCompleted {
                    call_id: pending.canonical_tool_call_id,
                    tool_index: *ti,
                    arguments: pending.args_buffer.clone(),
                });
            }
        }

        // Build response.
        let mut items = Vec::new();
        // Reasoning summary first so the turn coordinator can push it into
        // chat_state before the assistant text — this preserves the
        // [ReasoningSummary, AssistantText, ToolCall] ordering that the
        // ChatCompletions projection merges into a single assistant message
        // carrying `reasoning_content`.
        if !self.reasoning_acc.is_empty() {
            items.push(CanonicalResponseItem::ReasoningSummary {
                content: self.reasoning_acc.clone(),
            });
        }
        if !self.text_acc.is_empty() {
            items.push(CanonicalResponseItem::AssistantText {
                content: self.text_acc.clone(),
            });
        }
        for pending in self.pending_tool_calls.values() {
            if !pending.args_buffer.is_empty() {
                if let Ok(args) = serde_json::from_str(&pending.args_buffer) {
                    items.push(CanonicalResponseItem::ToolCall {
                        call_id: pending.canonical_tool_call_id,
                        name: pending.name_buffer.clone(),
                        arguments: args,
                    });
                }
            }
        }

        let stop_reason = match self.finish_reason.as_deref() {
            Some("stop") => Some(StopReason::Stop),
            Some("length") => Some(StopReason::Length),
            Some("tool_calls") => Some(StopReason::ToolCalls),
            Some("content_filter") => Some(StopReason::ContentFilter),
            _ if !self.pending_tool_calls.is_empty() => Some(StopReason::ToolCalls),
            _ => Some(StopReason::Stop),
        };

        let usage = self.usage.as_ref().map(|u| SettledUsage {
            estimated: false,
            input_tokens: u.prompt_tokens.unwrap_or(0),
            cached_input_tokens: 0,
            cache_creation_tokens: 0,
            output_tokens: u.completion_tokens.unwrap_or(0),
            reasoning_tokens: 0,
            total_tokens: u.total_tokens.unwrap_or(0),
            cost_micro_units: None,
            currency: None,
        }).unwrap_or_else(|| SettledUsage {
            estimated: true,
            input_tokens: 0, cached_input_tokens: 0, cache_creation_tokens: 0,
            output_tokens: 0, reasoning_tokens: 0, total_tokens: 0,
            cost_micro_units: None, currency: None,
        });

        let response = CanonicalModelResponse {
            request_id: self.request_id.clone(),
            items,
            stop_reason,
            usage,
        };

        events.push(CanonicalModelEvent::ResponseCompleted(response));
        self.state = DecoderState::Completed;
        Ok(events)
    }

    fn state(&self) -> DecoderState { self.state }
    fn pending_tool_calls(&self) -> &BTreeMap<u32, PendingToolCall> { &self.pending_tool_calls }
    fn has_semantic_content(&self) -> bool { self.has_semantic }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_delta() {
        let mut dec = ChatCompletionsDecoder::new("req_1".into());
        // SSE frames are newline-terminated; the LineBuffer only emits a
        // line once it sees its trailing `\n`.
        let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"index\":0}]}\n";
        let events = dec.process_chunk(chunk.as_bytes()).unwrap();
        assert!(events.iter().any(|e| matches!(e, CanonicalModelEvent::TextDelta { .. })));
    }

    #[test]
    fn parses_tool_call_delta() {
        let mut dec = ChatCompletionsDecoder::new("req_2".into());
        let chunk = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n";
        let events = dec.process_chunk(chunk.as_bytes()).unwrap();
        assert!(events.iter().any(|e| matches!(e, CanonicalModelEvent::ToolCallStarted { .. })));
    }

    #[test]
    fn done_marker_triggers_terminal() {
        let mut dec = ChatCompletionsDecoder::new("req_3".into());
        dec.process_chunk(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n").unwrap();
        let events = dec.finalize().unwrap();
        assert!(matches!(events.last().unwrap(), CanonicalModelEvent::ResponseCompleted(_)));
    }
}
