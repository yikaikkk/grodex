//! ResponsesDecoder: OpenAI Responses API SSE → CanonicalModelEvent.
//!
//! This is the Phase 1 main path. It transforms raw `ResponseStreamEvent`
//! items into `CanonicalModelEvent` items, maintaining:
//!   - output_index → tool_index mapping for function call accumulation
//!   - Content-aware idle timeout (heartbeats don't reset the timer)
//!   - Exactly one terminal event guarantee
//!
//! Pattern follows Grok's `stream/responses.rs` but maps to Grodex canonical types.

use super::pending_tool_call::PendingToolCall;
use crate::line_buffer::LineBuffer;
use crate::streaming::{DecoderState, StreamingDecoder};
use crate::wire_types::{ResponseStreamEvent, WireOutputItem, WireResponse};
use grodex_provider::canonical_event::{CanonicalModelResponse, CanonicalResponseItem, StopReason};
use grodex_provider::usage::SettledUsage;
use grodex_provider::{CanonicalModelEvent, ProviderError};
use std::collections::BTreeMap;

/// Streaming decoder for OpenAI Responses API SSE stream.
pub struct ResponsesDecoder {
    state: DecoderState,
    request_id: String,
    line_buf: LineBuffer,
    /// Maps Responses output_index → Grodex tool_index.
    output_to_tool_index: BTreeMap<u32, u32>,
    /// Accumulating tool calls, keyed by tool_index.
    pending_tool_calls: BTreeMap<u32, PendingToolCall>,
    /// Next tool_index to assign.
    next_tool_index: u32,
    /// Monotonic chunk counter (spans text + reasoning).
    chunk_index: u64,
    /// Accumulated visible text content.
    text_acc: String,
    /// Accumulated reasoning content.
    reasoning_acc: String,
    /// The full wire Response, set when ResponseCompleted arrives.
    final_response: Option<WireResponse>,
    /// Whether any semantic content has been received.
    has_semantic: bool,
    /// Whether the first token has been emitted (for TTFB tracking).
    first_token_emitted: bool,
}

impl ResponsesDecoder {
    /// Create a new Responses decoder for the given request.
    pub fn new(request_id: String) -> Self {
        Self {
            state: DecoderState::Created,
            request_id,
            line_buf: LineBuffer::new(),
            output_to_tool_index: BTreeMap::new(),
            pending_tool_calls: BTreeMap::new(),
            next_tool_index: 0,
            chunk_index: 0,
            text_acc: String::new(),
            reasoning_acc: String::new(),
            final_response: None,
            has_semantic: false,
            first_token_emitted: false,
        }
    }

    /// Process a single parsed ResponseStreamEvent into canonical events.
    pub fn process_event(&mut self, event: ResponseStreamEvent) -> Vec<CanonicalModelEvent> {
        let mut events = Vec::new();

        // Transition from Created → Started on first meaningful event.
        if self.state == DecoderState::Created && event.has_meaningful_content() {
            self.state = DecoderState::Started;
        }

        match event {
            ResponseStreamEvent::OutputItemAdded { output_index, item } => {
                if let WireOutputItem::FunctionCall { call_id: _, name, .. } = item {
                    let tool_index = self.next_tool_index;
                    self.next_tool_index += 1;
                    self.output_to_tool_index.insert(output_index, tool_index);

                    let pending = PendingToolCall::new(output_index.to_string(), name.clone());
                    let call_id = pending.canonical_tool_call_id;
                    self.pending_tool_calls.insert(tool_index, pending);
                    self.state = DecoderState::StreamingToolCalls;
                    self.has_semantic = true;

                    events.push(CanonicalModelEvent::ToolCallStarted {
                        call_id,
                        name,
                        tool_index,
                    });
                }
            }

            ResponseStreamEvent::OutputTextDelta { delta, .. } => {
                if delta.is_empty() {
                    return events;
                }
                self.has_semantic = true;
                self.state = DecoderState::StreamingText;
                self.text_acc.push_str(&delta);
                self.chunk_index += 1;

                if !self.first_token_emitted {
                    self.first_token_emitted = true;
                }

                events.push(CanonicalModelEvent::TextDelta {
                    text: delta,
                    chunk_index: self.chunk_index,
                });
            }

            ResponseStreamEvent::FunctionCallArgumentsDelta { output_index, delta } => {
                if delta.is_empty() {
                    return events;
                }
                self.has_semantic = true;
                let tool_index = self.output_to_tool_index.get(&output_index).copied();

                if let Some(ti) = tool_index {
                    if let Some(pending) = self.pending_tool_calls.get_mut(&ti) {
                        pending.append_args(&delta);

                        events.push(CanonicalModelEvent::ToolCallArgumentsDelta {
                            call_id: pending.canonical_tool_call_id,
                            tool_index: ti,
                            arguments_delta: delta,
                        });
                    }
                }
                // If tool_index is unknown, we drop the delta (consistent with Grok).
            }

            ResponseStreamEvent::ReasoningTextDelta { delta, .. } => {
                if delta.is_empty() {
                    return events;
                }
                self.has_semantic = true;
                self.state = DecoderState::StreamingReasoning;
                self.reasoning_acc.push_str(&delta);
                self.chunk_index += 1;

                events.push(CanonicalModelEvent::ReasoningDelta {
                    text: delta,
                    chunk_index: self.chunk_index,
                });
            }

            ResponseStreamEvent::ReasoningSummaryTextDelta { delta, .. } => {
                if delta.is_empty() {
                    return events;
                }
                self.has_semantic = true;
                self.chunk_index += 1;
                // Reasoning summary is treated as ReasoningDelta for now.
                events.push(CanonicalModelEvent::ReasoningDelta {
                    text: delta,
                    chunk_index: self.chunk_index,
                });
            }

            ResponseStreamEvent::ResponseCompleted { response } => {
                self.state = DecoderState::Completing;
                self.final_response = Some(response);
            }

            ResponseStreamEvent::ResponseFailed { .. } => {
                self.state = DecoderState::Completing;
            }

            ResponseStreamEvent::Error { .. } => {
                self.state = DecoderState::Completing;
            }

            // Heartbeat events: do nothing, don't reset content timer.
            ResponseStreamEvent::ResponseQueued
            | ResponseStreamEvent::ResponseInProgress { .. }
            | ResponseStreamEvent::AnnotationAdded { .. }
            | ResponseStreamEvent::Unknown => {}
        }

        events
    }

    /// Finalize pending tool calls. Called when ResponseCompleted arrives.
    fn finalize_tool_calls(&mut self) -> Vec<CanonicalModelEvent> {
        let mut events = Vec::new();
        for (ti, pending) in &self.pending_tool_calls {
            // Only emit completed events for calls that received arguments.
            // Calls with no name are malformed and skipped.
            if !pending.name_buffer.is_empty() && !pending.args_buffer.is_empty() {
                let mut pc = pending.clone();
                pc.mark_completed();
                events.push(CanonicalModelEvent::ToolCallCompleted {
                    call_id: pc.canonical_tool_call_id,
                    tool_index: *ti,
                    arguments: std::mem::take(&mut pc.args_buffer),
                });
            }
        }
        events
    }

    /// Build the CanonicalModelResponse from the wire Response.
    fn build_response(&self) -> Result<CanonicalModelResponse, ProviderError> {
        let wire = self
            .final_response
            .as_ref()
            .ok_or_else(|| ProviderError::Internal("final_response not set".into()))?;

        let mut items = Vec::new();

        // Add text content if present.
        if !self.text_acc.is_empty() {
            items.push(CanonicalResponseItem::AssistantText {
                content: self.text_acc.clone(),
            });
        }

        // Add completed tool calls.
        for pending in self.pending_tool_calls.values() {
            if !pending.args_buffer.is_empty() {
                match pending.parse_args() {
                    Ok(args) => {
                        items.push(CanonicalResponseItem::ToolCall {
                            call_id: pending.canonical_tool_call_id,
                            name: pending.name_buffer.clone(),
                            arguments: args,
                        });
                    }
                    Err(_) => {
                        return Err(ProviderError::IncompleteToolCall {
                            call_id: pending.canonical_tool_call_id.to_string(),
                        });
                    }
                }
            }
        }

        // Add reasoning if present.
        if !self.reasoning_acc.is_empty() {
            items.push(CanonicalResponseItem::ReasoningSummary {
                content: self.reasoning_acc.clone(),
            });
        }

        // Determine stop reason.
        let stop_reason = if !self.pending_tool_calls.is_empty() {
            Some(StopReason::ToolCalls)
        } else {
            Some(StopReason::Stop)
        };

        // Build settled usage from wire usage.
        let usage = wire
            .usage
            .as_ref()
            .map(|u| {
                let cached = u.input_tokens_details.as_ref().map(|d| d.cached_tokens).unwrap_or(0);
                let cache_create = u
                    .input_tokens_details
                    .as_ref()
                    .map(|d| d.cache_creation_tokens)
                    .unwrap_or(0);
                let reasoning = u
                    .output_tokens_details
                    .as_ref()
                    .map(|d| d.reasoning_tokens)
                    .unwrap_or(0);

                SettledUsage {
                    estimated: false,
                    input_tokens: u.input_tokens,
                    cached_input_tokens: cached,
                    cache_creation_tokens: cache_create,
                    output_tokens: u.output_tokens,
                    reasoning_tokens: reasoning,
                    total_tokens: u.total_tokens,
                    cost_micro_units: None,
                    currency: None,
                }
            })
            .unwrap_or_else(|| SettledUsage {
                estimated: true,
                input_tokens: 0,
                cached_input_tokens: 0,
                cache_creation_tokens: 0,
                output_tokens: 0,
                reasoning_tokens: 0,
                total_tokens: 0,
                cost_micro_units: None,
                currency: None,
            });

        Ok(CanonicalModelResponse {
            request_id: self.request_id.clone(),
            items,
            stop_reason,
            usage,
        })
    }
}

impl StreamingDecoder for ResponsesDecoder {
    fn process_chunk(&mut self, chunk: &[u8]) -> Result<Vec<CanonicalModelEvent>, ProviderError> {
        let lines = self.line_buf.feed(chunk);
        let mut events = Vec::new();

        for line in &lines {
            let data = line.trim();
            if data.is_empty() {
                continue;
            }
            // SSE format: "data: <json>" or "data: [DONE]"
            let json_str = if let Some(payload) = data.strip_prefix("data: ") {
                if payload == "[DONE]" {
                    // Stream end marker. finalize() will handle the terminal event.
                    break;
                }
                payload
            } else {
                continue;
            };

            match serde_json::from_str::<ResponseStreamEvent>(json_str) {
                Ok(event) => {
                    events.extend(self.process_event(event));
                }
                Err(e) => {
                    // Log unknown event but don't fail the stream.
                    // In production this would be a debug log.
                    let _ = e;
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

        // Flush any trailing incomplete line held by the LineBuffer. A real
        // SSE transport terminates every line with `\n`; but the last chunk
        // a provider sends (and unit-test fixtures) may omit it, in which
        // case the final event would otherwise be stranded in the buffer.
        if let Some(remaining) = self.line_buf.flush() {
            let data = remaining.trim();
            if !data.is_empty() {
                if let Some(payload) = data.strip_prefix("data: ") {
                    if payload != "[DONE]" {
                        if let Ok(event) = serde_json::from_str::<ResponseStreamEvent>(payload) {
                            events.extend(self.process_event(event));
                        }
                    }
                }
            }
        }

        // If we have a ResponseCompleted, finalize tool calls and build response.
        if self.final_response.is_some() && self.state == DecoderState::Completing {
            events.extend(self.finalize_tool_calls());

            match self.build_response() {
                Ok(response) => {
                    events.push(CanonicalModelEvent::ResponseCompleted(response));
                    self.state = DecoderState::Completed;
                    Ok(events)
                }
                Err(e) => {
                    self.state = DecoderState::Failed;
                    events.push(CanonicalModelEvent::ResponseFailed(e));
                    Ok(events)
                }
            }
        } else if self.state == DecoderState::Completing {
            // ResponseFailed or Error — emit failure.
            let err = ProviderError::Api {
                status_code: 500,
                message: "response failed".into(),
                retry_after_secs: None,
            };
            self.state = DecoderState::Failed;
            Ok(vec![CanonicalModelEvent::ResponseFailed(err)])
        } else if !self.is_terminal() {
            // Stream ended without a terminal event.
            let err = ProviderError::Transport {
                message: "stream ended without completion".into(),
            };
            self.state = DecoderState::Failed;
            Ok(vec![CanonicalModelEvent::ResponseFailed(err)])
        } else {
            Ok(events)
        }
    }

    fn state(&self) -> DecoderState {
        self.state
    }

    fn pending_tool_calls(&self) -> &BTreeMap<u32, PendingToolCall> {
        &self.pending_tool_calls
    }

    fn has_semantic_content(&self) -> bool {
        self.has_semantic
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_text_delta_event(text: &str) -> ResponseStreamEvent {
        ResponseStreamEvent::OutputTextDelta {
            output_index: 0,
            content_index: 0,
            delta: text.to_string(),
        }
    }

    fn make_completed_event() -> ResponseStreamEvent {
        ResponseStreamEvent::ResponseCompleted {
            response: WireResponse {
                id: "resp_123".into(),
                object: "response".into(),
                status: Some("completed".into()),
                output: vec![],
                usage: None,
                model: Some("gpt-5".into()),
            },
        }
    }

    fn make_function_call_added(name: &str, output_index: u32) -> ResponseStreamEvent {
        ResponseStreamEvent::OutputItemAdded {
            output_index,
            item: WireOutputItem::FunctionCall {
                id: format!("fc_{output_index}"),
                call_id: format!("call_{output_index}"),
                name: name.to_string(),
                arguments: String::new(),
                status: None,
            },
        }
    }

    fn make_args_delta(output_index: u32, delta: &str) -> ResponseStreamEvent {
        ResponseStreamEvent::FunctionCallArgumentsDelta {
            output_index,
            delta: delta.to_string(),
        }
    }

    #[test]
    fn text_completes_with_stop() {
        let mut dec = ResponsesDecoder::new("req_1".into());
        dec.process_event(make_text_delta_event("Hello"));
        dec.process_event(make_completed_event());

        let final_events = dec.finalize().unwrap();
        assert_eq!(final_events.len(), 1);

        let resp = match &final_events[0] {
            CanonicalModelEvent::ResponseCompleted(r) => r,
            other => panic!("expected ResponseCompleted, got {:?}", other),
        };
        assert_eq!(resp.stop_reason, Some(StopReason::Stop));
        assert_eq!(resp.items.len(), 1);
        assert!(matches!(&resp.items[0], CanonicalResponseItem::AssistantText { content } if content == "Hello"));
    }

    #[test]
    fn single_function_call_completes() {
        let mut dec = ResponsesDecoder::new("req_2".into());
        dec.process_event(make_function_call_added("read_file", 0));
        dec.process_event(make_args_delta(0, r#"{"path":"#));
        dec.process_event(make_args_delta(0, r#""/tmp/test.txt"}"#));
        dec.process_event(make_completed_event());

        let final_events = dec.finalize().unwrap();
        // ToolCallCompleted + ResponseCompleted
        assert!(final_events.len() >= 2);

        let resp = final_events.last().unwrap();
        assert!(matches!(resp, CanonicalModelEvent::ResponseCompleted(_)));
    }

    #[test]
    fn multiple_function_calls_get_distinct_indices() {
        let mut dec = ResponsesDecoder::new("req_3".into());
        dec.process_event(make_function_call_added("read_file", 0));
        dec.process_event(make_function_call_added("exec", 1));
        dec.process_event(make_args_delta(0, r#"{"path":"/a"}"#));
        dec.process_event(make_args_delta(1, r#"{"cmd":"ls"}"#));
        dec.process_event(make_completed_event());

        let final_events = dec.finalize().unwrap();
        let completed_count = final_events
            .iter()
            .filter(|e| matches!(e, CanonicalModelEvent::ToolCallCompleted { .. }))
            .count();
        assert_eq!(completed_count, 2);

        let tool_indices: Vec<u32> = final_events
            .iter()
            .filter_map(|e| match e {
                CanonicalModelEvent::ToolCallCompleted { tool_index, .. } => Some(*tool_index),
                _ => None,
            })
            .collect();
        assert_ne!(tool_indices[0], tool_indices[1]);
    }

    #[test]
    fn stream_ends_without_terminal_produces_failed() {
        let mut dec = ResponsesDecoder::new("req_4".into());
        dec.process_event(make_text_delta_event("partial..."));
        // No completion event before finalize.

        let final_events = dec.finalize().unwrap();
        assert_eq!(final_events.len(), 1);
        assert!(matches!(final_events[0], CanonicalModelEvent::ResponseFailed(_)));
    }

    #[test]
    fn empty_text_delta_produces_no_event() {
        let mut dec = ResponsesDecoder::new("req_5".into());
        let events = dec.process_event(ResponseStreamEvent::OutputTextDelta {
            output_index: 0,
            content_index: 0,
            delta: String::new(),
        });
        assert!(events.is_empty());
    }

    #[test]
    fn exactly_one_terminal_event() {
        let mut dec = ResponsesDecoder::new("req_6".into());
        dec.process_event(make_text_delta_event("done"));
        dec.process_event(make_completed_event());

        let first = dec.finalize().unwrap();
        assert_eq!(first.len(), 1);
        assert!(matches!(first[0], CanonicalModelEvent::ResponseCompleted(_)));

        // Second finalize should return empty — terminal already emitted.
        let second = dec.finalize().unwrap();
        assert!(second.is_empty());
    }

    #[test]
    fn unknown_event_is_handled_gracefully() {
        let mut dec = ResponsesDecoder::new("req_7".into());
        // Unknown event should not panic.
        let events = dec.process_event(ResponseStreamEvent::Unknown);
        assert!(events.is_empty());
    }
}
