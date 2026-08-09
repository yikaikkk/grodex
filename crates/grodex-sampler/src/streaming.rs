//! StreamingDecoder trait and DecoderState — the contract for all wire backends.
//!
//! Every wire-specific decoder implements `StreamingDecoder`. The trait
//! enforces the invariants from Design Doc 14, Section 10.2:
//!   1. Exactly one terminal event per request.
//!   2. Tool call args accumulate incrementally — never assume valid JSON mid-stream.
//!   3. Partial tool calls are rejected, never executed.

use crate::decoder::pending_tool_call::PendingToolCall;
use grodex_provider::CanonicalModelEvent;
use grodex_provider::ProviderError;
use std::collections::BTreeMap;

/// The explicit state machine for a streaming decoder.
///
/// Every decoder transitions through these states in order.
/// The `is_terminal` check gates the "exactly one terminal event" invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderState {
    /// Decoder created; no chunks processed yet.
    Created,
    /// First chunk received; stream is alive.
    Started,
    /// Actively receiving text content.
    StreamingText,
    /// Actively receiving reasoning/thinking content.
    StreamingReasoning,
    /// Actively receiving tool call content.
    StreamingToolCalls,
    /// Terminal event received from wire; assembling final response.
    Completing,
    /// Terminal event emitted (Completed).
    Completed,
    /// Terminal event emitted (Failed).
    Failed,
    /// Stream was aborted externally.
    Aborted,
}

/// The trait that all wire-specific streaming decoders implement.
///
/// Decoders transform raw byte chunks (SSE lines, etc.) into canonical events.
/// Each decoder is created for one request and consumed to completion.
pub trait StreamingDecoder: Send {
    /// Process the next raw chunk from the transport.
    ///
    /// Returns canonical events (may be empty for intermediate/no-op chunks).
    /// An empty vec from a non-terminal state means "more chunks needed."
    fn process_chunk(&mut self, chunk: &[u8]) -> Result<Vec<CanonicalModelEvent>, ProviderError>;

    /// Signal that the transport stream has ended.
    ///
    /// The decoder MUST produce a terminal event, or an error if the stream
    /// ended without a valid completion.
    fn finalize(&mut self) -> Result<Vec<CanonicalModelEvent>, ProviderError>;

    /// Current decoder state.
    fn state(&self) -> DecoderState;

    /// Whether the decoder has emitted its terminal event.
    fn is_terminal(&self) -> bool {
        matches!(
            self.state(),
            DecoderState::Completed | DecoderState::Failed | DecoderState::Aborted
        )
    }

    /// Currently-accumulating pending tool calls, keyed by tool_index.
    fn pending_tool_calls(&self) -> &BTreeMap<u32, PendingToolCall>;

    /// Whether any semantic content has been received (text, tool call delta,
    /// reasoning, refusal). Used for the semantic commit fence check.
    fn has_semantic_content(&self) -> bool;
}

/// Result of the terminal event check.
pub enum DecoderOutcome {
    /// Decoder produced a successful terminal event.
    Completed(Vec<CanonicalModelEvent>),
    /// Decoder produced a failure terminal event.
    Failed(Vec<CanonicalModelEvent>),
    /// Decoder has not yet terminated — more chunks needed.
    Pending,
}
