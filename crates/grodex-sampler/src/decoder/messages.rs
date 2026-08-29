//! MessagesDecoder — Anthropic Messages SSE → CanonicalModelEvent.
//!
//! Transforms Anthropic streaming events (message_start, content_block_start/delta/stop,
//! message_delta, message_stop) into canonical events.

use super::pending_tool_call::PendingToolCall;
use crate::line_buffer::LineBuffer;
use crate::streaming::{DecoderState, StreamingDecoder};
use grodex_provider::canonical_event::{
    CanonicalModelResponse, CanonicalResponseItem, StopReason,
};
use grodex_provider::usage::SettledUsage;
use grodex_provider::{CanonicalModelEvent, ProviderError};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum MessageEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: MessageData },
    #[serde(rename = "content_block_start")]
    ContentBlockStart { index: u32, content_block: ContentBlock },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: u32, delta: ContentDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: u32 },
    #[serde(rename = "message_delta")]
    MessageDelta { delta: MsgDelta, usage: Option<MsgUsage> },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct MessageData {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<MsgUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String, input: serde_json::Value },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct MsgDelta {
    #[serde(default)]
    stop_reason: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    stop_sequence: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MsgUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    /// Prompt-cache fields: `cache_read_input_tokens` is the cache-hit
    /// subset; `cache_creation_input_tokens` is what was written to cache.
    /// Both are ADDITIONAL to `input_tokens` in Anthropic accounting.
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

pub struct MessagesDecoder {
    state: DecoderState,
    request_id: String,
    line_buf: LineBuffer,
    pending_tool_calls: BTreeMap<u32, PendingToolCall>,
    chunk_index: u64,
    text_acc: String,
    has_semantic: bool,
    stop_reason: Option<String>,
    model: Option<String>,
    usage: Option<MsgUsage>,
}

impl MessagesDecoder {
    pub fn new(request_id: String) -> Self {
        Self {
            state: DecoderState::Created,
            request_id,
            line_buf: LineBuffer::new(),
            pending_tool_calls: BTreeMap::new(),
            chunk_index: 0,
            text_acc: String::new(),
            has_semantic: false,
            stop_reason: None,
            model: None,
            usage: None,
        }
    }

    fn process_event(&mut self, event: MessageEvent) -> Vec<CanonicalModelEvent> {
        let mut events = Vec::new();

        match event {
            MessageEvent::MessageStart { message } => {
                self.state = DecoderState::Started;
                self.model = message.model;
                self.usage = message.usage;
            }
            MessageEvent::ContentBlockStart { index, content_block } => {
                match content_block {
                    ContentBlock::Text { text } => {
                        self.has_semantic = true;
                        self.state = DecoderState::StreamingText;
                        self.text_acc.push_str(&text);
                        self.chunk_index += 1;
                        events.push(CanonicalModelEvent::TextDelta {
                            text,
                            chunk_index: self.chunk_index,
                        });
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        self.has_semantic = true;
                        self.state = DecoderState::StreamingToolCalls;
                        let call_id = grodex_core::id::ToolCallId::from_string(&id)
                            .unwrap_or_else(|_| grodex_core::id::ToolCallId::new());
                        let mut pending = PendingToolCall::new(index.to_string(), name.clone());
                        pending.canonical_tool_call_id = call_id;
                        let args_str = input.to_string();
                        if args_str != "{}" {
                            pending.append_args(&args_str);
                        }
                        self.pending_tool_calls.insert(index, pending);

                        events.push(CanonicalModelEvent::ToolCallStarted {
                            call_id,
                            name,
                            tool_index: index,
                        });
                        if args_str != "{}" {
                            events.push(CanonicalModelEvent::ToolCallArgumentsDelta {
                                call_id,
                                tool_index: index,
                                arguments_delta: args_str,
                            });
                        }
                    }
                    _ => {}
                }
            }
            MessageEvent::ContentBlockDelta { index, delta } => {
                match delta {
                    ContentDelta::TextDelta { text } => {
                        self.has_semantic = true;
                        self.text_acc.push_str(&text);
                        self.chunk_index += 1;
                        events.push(CanonicalModelEvent::TextDelta {
                            text,
                            chunk_index: self.chunk_index,
                        });
                    }
                    ContentDelta::InputJsonDelta { partial_json } => {
                        if let Some(pending) = self.pending_tool_calls.get_mut(&index) {
                            pending.append_args(&partial_json);
                            events.push(CanonicalModelEvent::ToolCallArgumentsDelta {
                                call_id: pending.canonical_tool_call_id,
                                tool_index: index,
                                arguments_delta: partial_json,
                            });
                        }
                    }
                    _ => {}
                }
            }
            MessageEvent::ContentBlockStop { index } => {
                if let Some(pending) = self.pending_tool_calls.get(&index) {
                    events.push(CanonicalModelEvent::ToolCallCompleted {
                        call_id: pending.canonical_tool_call_id,
                        tool_index: index,
                        arguments: pending.args_buffer.clone(),
                    });
                }
            }
            MessageEvent::MessageDelta { delta, usage } => {
                self.stop_reason = delta.stop_reason;
                if let Some(u) = usage {
                    self.usage = Some(u);
                }
                self.state = DecoderState::Completing;
            }
            MessageEvent::MessageStop => {
                self.state = DecoderState::Completing;
            }
            MessageEvent::Ping | MessageEvent::Unknown => {}
        }

        events
    }
}

impl StreamingDecoder for MessagesDecoder {
    fn process_chunk(&mut self, chunk: &[u8]) -> Result<Vec<CanonicalModelEvent>, ProviderError> {
        let lines = self.line_buf.feed(chunk);
        let mut events = Vec::new();

        for line in &lines {
            let data = line.trim();
            if let Some(payload) = data.strip_prefix("data: ") {
                if let Ok(event) = serde_json::from_str::<MessageEvent>(payload) {
                    events.extend(self.process_event(event));
                }
            }
        }

        Ok(events)
    }

    fn finalize(&mut self) -> Result<Vec<CanonicalModelEvent>, ProviderError> {
        if self.is_terminal() {
            return Ok(Vec::new());
        }

        let mut events = Vec::new();
        let mut items = Vec::new();
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

        let stop_reason = match self.stop_reason.as_deref() {
            Some("end_turn") | Some("stop_sequence") | Some("pause_turn") => {
                Some(StopReason::Stop)
            }
            Some("max_tokens") => Some(StopReason::Length),
            Some("tool_use") => Some(StopReason::ToolCalls),
            _ if !self.pending_tool_calls.is_empty() => Some(StopReason::ToolCalls),
            _ => Some(StopReason::Stop),
        };

        let usage = self.usage.as_ref().map(|u| {
            // Anthropic reports cache tokens SEPARATELY from input_tokens
            // (input_tokens only counts uncached input). Normalize to the
            // SettledUsage convention where input_tokens INCLUDES the
            // cached subset.
            let base = u.input_tokens.unwrap_or(0);
            let cache_read = u.cache_read_input_tokens.unwrap_or(0);
            let cache_create = u.cache_creation_input_tokens.unwrap_or(0);
            let input_total = base + cache_read + cache_create;
            SettledUsage {
                estimated: false,
                input_tokens: input_total,
                cached_input_tokens: cache_read,
                cache_creation_tokens: cache_create,
                output_tokens: u.output_tokens.unwrap_or(0),
                reasoning_tokens: 0,
                total_tokens: input_total + u.output_tokens.unwrap_or(0),
                cost_micro_units: None,
                currency: None,
            }
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
        let mut dec = MessagesDecoder::new("req_1".into());
        // SSE frames are newline-terminated; the LineBuffer only emits a
        // line once it sees its trailing `\n`.
        let chunk = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n";
        let ev = dec.process_chunk(chunk.as_bytes()).unwrap();
        assert!(ev.iter().any(|e| matches!(e, CanonicalModelEvent::TextDelta { .. })));
    }

    #[test]
    fn parses_tool_use() {
        let mut dec = MessagesDecoder::new("req_2".into());
        let chunk = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"read\",\"input\":{}}}\n";
        let ev = dec.process_chunk(chunk.as_bytes()).unwrap();
        assert!(ev.iter().any(|e| matches!(e, CanonicalModelEvent::ToolCallStarted { .. })));
    }
}
