//! Golden fixture tests — replay recorded SSE streams through ResponsesDecoder.
//!
//! Each fixture file contains one line per SSE event (JSON).
//! The decoder processes them and produces canonical events.
//! These tests assert exact event sequences and invariants.

use grodex_provider::{CanonicalModelEvent, StopReason};
use grodex_sampler::decoder::responses::ResponsesDecoder;
use grodex_sampler::streaming::StreamingDecoder;
use std::fs;

fn load_fixture(name: &str) -> Vec<String> {
    let path = format!("tests/golden/{name}");
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read fixture {path}: {e}"));
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// Feed SSE lines to the decoder and collect all canonical events.
fn replay_fixture(name: &str) -> Vec<CanonicalModelEvent> {
    let lines = load_fixture(name);
    let mut decoder = ResponsesDecoder::new(format!("req_{name}"));
    let mut events = Vec::new();

    for line in &lines {
        // Each fixture line is one `data:` SSE event. Re-append the `\n`
        // that `lines()` stripped — the LineBuffer only emits a line once it
        // sees its terminating newline (or via finalize()). Feeding without
        // it would strand every line in the buffer except the last.
        let mut chunk = line.as_bytes().to_vec();
        chunk.push(b'\n');
        match decoder.process_chunk(&chunk) {
            Ok(chunk_events) => events.extend(chunk_events),
            Err(e) => {
                events.push(CanonicalModelEvent::ResponseFailed(e));
            }
        }
        if decoder.is_terminal() {
            break;
        }
    }

    if !decoder.is_terminal() {
        match decoder.finalize() {
            Ok(final_events) => events.extend(final_events),
            Err(e) => {
                events.push(CanonicalModelEvent::ResponseFailed(e));
            }
        }
    }

    events
}

#[test]
fn golden_text_only() {
    let events = replay_fixture("text_only.jsonl");
    assert!(!events.is_empty());

    // Should have at least TextDelta events.
    let text_events: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, CanonicalModelEvent::TextDelta { .. }))
        .collect();
    assert!(!text_events.is_empty(), "expected text events");

    // Should end with ResponseCompleted.
    let last = events.last().unwrap();
    assert!(
        matches!(last, CanonicalModelEvent::ResponseCompleted(_)),
        "expected ResponseCompleted, got {last:?}"
    );

    // Verify stop reason.
    if let CanonicalModelEvent::ResponseCompleted(resp) = last {
        assert_eq!(resp.stop_reason, Some(StopReason::Stop));
        assert!(!resp.usage.estimated);
        assert_eq!(resp.usage.total_tokens, 15);
    }
}

#[test]
fn golden_function_call() {
    let events = replay_fixture("function_call.jsonl");

    // Should have ToolCallStarted.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CanonicalModelEvent::ToolCallStarted { .. })),
        "expected ToolCallStarted"
    );

    // Should have ToolCallArgumentsDelta events.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CanonicalModelEvent::ToolCallArgumentsDelta { .. })),
        "expected ToolCallArgumentsDelta"
    );

    // Should end with ResponseCompleted with ToolCalls stop reason.
    let last = events.last().unwrap();
    if let CanonicalModelEvent::ResponseCompleted(resp) = last {
        assert_eq!(resp.stop_reason, Some(StopReason::ToolCalls));
    } else {
        panic!("expected ResponseCompleted, got {last:?}");
    }
}

#[test]
fn golden_multi_function_call() {
    let events = replay_fixture("multi_function_call.jsonl");

    // Two distinct tool calls.
    let started: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, CanonicalModelEvent::ToolCallStarted { .. }))
        .collect();
    assert_eq!(started.len(), 2, "expected 2 tool calls, got events: {events:?}");

    // Two ToolCallCompleted events.
    let completed: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, CanonicalModelEvent::ToolCallCompleted { .. }))
        .collect();
    assert_eq!(completed.len(), 2);

    // Distinct tool_indices.
    let indices: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            CanonicalModelEvent::ToolCallStarted { tool_index, .. } => Some(*tool_index),
            _ => None,
        })
        .collect();
    assert_ne!(indices[0], indices[1], "tool indices should be distinct");
}

#[test]
fn golden_reasoning() {
    let events = replay_fixture("reasoning.jsonl");

    // Should have ReasoningDelta events.
    let reasoning: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, CanonicalModelEvent::ReasoningDelta { .. }))
        .collect();
    assert!(!reasoning.is_empty(), "expected reasoning events");

    // Should have TextDelta after reasoning.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CanonicalModelEvent::TextDelta { .. })),
        "expected text events after reasoning"
    );

    // Should end with ResponseCompleted.
    assert!(matches!(
        events.last().unwrap(),
        CanonicalModelEvent::ResponseCompleted(_)
    ));
}

#[test]
fn golden_stream_error() {
    let events = replay_fixture("stream_error.jsonl");

    // Should end with ResponseFailed (not Completed).
    let last = events.last().unwrap();
    assert!(
        matches!(last, CanonicalModelEvent::ResponseFailed(_)),
        "expected ResponseFailed, got {last:?}"
    );
}

#[test]
fn golden_exactly_one_terminal() {
    for fixture in &[
        "text_only.jsonl",
        "function_call.jsonl",
        "multi_function_call.jsonl",
        "reasoning.jsonl",
        "stream_error.jsonl",
    ] {
        let events = replay_fixture(fixture);
        let terminal_count = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    CanonicalModelEvent::ResponseCompleted(_)
                        | CanonicalModelEvent::ResponseFailed(_)
                )
            })
            .count();
        assert_eq!(
            terminal_count, 1,
            "{fixture}: expected exactly 1 terminal event, got {terminal_count}"
        );
    }
}
