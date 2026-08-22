//! ChatStateActor — exclusive transcript owner.
//!
//! Following Grok's `ChatStateActor` pattern: a dedicated task that owns
//! the conversation transcript, token usage, and prompt index. SessionActor
//! communicates via mpsc commands + oneshot replies.
//!
//! Key invariants:
//!   - Only ChatStateActor mutates the transcript.
//!   - Dangling tool calls are repaired at EVERY write boundary.
//!   - Compaction replacement atomically swaps the transcript.

use crate::context_projection::ContextProjection;
use grodex_core::context::ContextItem;
use grodex_core::id::ToolCallId;
use std::collections::HashSet;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

// ── Commands ───────────────────────────────────────────────────────

/// Commands sent to the ChatStateActor.
pub enum ChatStateCommand {
    /// Append a user message.
    PushUserMessage {
        item: ContextItem,
        reply: oneshot::Sender<()>,
    },
    /// Append an assistant response (text only).
    PushAssistantResponse {
        content: String,
        reply: oneshot::Sender<()>,
    },
    /// Append a reasoning summary (DeepSeek/Qwen thinking mode). Pushed
    /// BEFORE the matching `PushAssistantResponse` so the projection layer
    /// can merge it into the assistant message's `reasoning_content`.
    PushReasoning {
        content: String,
        reply: oneshot::Sender<()>,
    },
    /// Append a tool result.
    PushToolResult {
        call_id: ToolCallId,
        content: String,
        is_error: bool,
        reply: oneshot::Sender<()>,
    },
    /// Append a tool call item.
    PushToolCall {
        call_id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
        reply: oneshot::Sender<()>,
    },
    /// Atomically replace the entire conversation (compaction).
    ReplaceConversation {
        items: Vec<ContextItem>,
        is_compaction: bool,
        reply: oneshot::Sender<()>,
    },
    /// Get a clone of the current conversation.
    GetConversation {
        reply: oneshot::Sender<Vec<ContextItem>>,
    },
    /// Get conversation length.
    GetConversationLen {
        reply: oneshot::Sender<usize>,
    },
    /// Get estimated total tokens.
    GetTotalTokens {
        reply: oneshot::Sender<u64>,
    },
    /// Rewind to a specific prompt index (cancel/undo).
    TruncateToPromptIndex {
        prompt_index: usize,
        reply: oneshot::Sender<Vec<ContextItem>>,
    },
    /// Shutdown the actor.
    Shutdown,
}

// ── Actor State ────────────────────────────────────────────────────

struct ChatState {
    /// Full transcript (rollout source, append-only).
    conversation: Vec<ContextItem>,
    /// Model-visible projection (lossy, replaceable by compaction).
    projection: ContextProjection,
    total_tokens: u64,
    prompt_index: usize,
    estimated_tokens_since_model: u64,
    estimate_at_last_response: u64,
}

impl ChatState {
    fn new() -> Self {
        Self {
            conversation: Vec::new(),
            projection: ContextProjection::new(),
            total_tokens: 0,
            prompt_index: 0,
            estimated_tokens_since_model: 0,
            estimate_at_last_response: 0,
        }
    }

    fn push(&mut self, item: ContextItem) {
        let tokens = estimate_item_tokens(&item);
        self.estimated_tokens_since_model += tokens;
        self.total_tokens += tokens;
        self.conversation.push(item.clone());
        self.projection.append(item, self.conversation.len() as u64);
    }

    fn estimate_total(&self) -> u64 {
        self.total_tokens + self.estimated_tokens_since_model
    }
}

/// Bytes-per-token heuristic.
fn estimate_item_tokens(item: &ContextItem) -> u64 {
    let chars = match item {
        ContextItem::System { content }
        | ContextItem::Developer { content }
        | ContextItem::User { content, .. }
        | ContextItem::Assistant { content }
        | ContextItem::ToolResult { content, .. } => content.len(),
        ContextItem::ToolCall { name, arguments, .. } => name.len() + arguments.to_string().len(),
        ContextItem::CompactionSummary { summary, .. } => summary.len(),
        ContextItem::ReasoningSummary { content } => content.len(),
        ContextItem::ImagePlaceholder { .. } => 340, // ~85 tokens × 4 chars
    };
    (chars as u64).div_ceil(4)
}

/// Repair dangling tool calls: dedup duplicate tool results, remove
/// orphaned tool calls that have no matching results, and remove empty
/// assistant messages whose tool_calls were all orphaned.
///
/// This fixes the ChatCompletions API error:
///   "An assistant message with 'tool_calls' must be followed by tool
///    messages responding to each 'tool_call_id'"
/// which occurs when the user cancels a turn after tool_calls have been
/// pushed to the context but before tool results arrive.
///
/// Called before user message push and at build-request time.
/// Operates on BOTH the raw conversation AND the model-visible projection
/// (they are separate structures — the projection is what `get_conversation`
/// returns to the turn coordinator for API requests).
fn ensure_conversation_integrity(conversation: &mut Vec<ContextItem>, projection: &mut ContextProjection) {
    // ── Step 1: Dedup duplicate ToolResults ──────────────────────
    let mut seen_results = HashSet::new();
    conversation.retain(|item| {
        if let ContextItem::ToolResult { call_id, .. } = item {
            if !seen_results.insert(*call_id) {
                return false; // duplicate — drop
            }
        }
        true
    });

    // ── Step 2: Collect result IDs and find orphaned ToolCalls ───
    let result_ids: HashSet<ToolCallId> = conversation
        .iter()
        .filter_map(|i| match i {
            ContextItem::ToolResult { call_id, .. } => Some(*call_id),
            _ => None,
        })
        .collect();

    // Track which ToolCall indices are orphaned (no matching result).
    let mut orphaned_indices: HashSet<usize> = HashSet::new();
    for (i, item) in conversation.iter().enumerate() {
        if let ContextItem::ToolCall { call_id, .. } = item {
            if !result_ids.contains(call_id) {
                orphaned_indices.insert(i);
            }
        }
    }

    // ── Step 3: Find Assistant messages whose ALL tool_calls are orphaned ──
    // In build_chat_body, an Assistant is merged with immediately following
    // ToolCall items into one API message with a tool_calls array. If ALL
    // those ToolCalls are orphaned, the entire assistant+tool_calls group
    // must be removed to keep the API happy.
    let mut assistant_remove: HashSet<usize> = HashSet::new();
    let mut i = 0;
    while i < conversation.len() {
        if matches!(conversation[i], ContextItem::Assistant { .. }) {
            let start = i + 1;
            let mut end = start;
            while end < conversation.len()
                && matches!(conversation[end], ContextItem::ToolCall { .. })
            {
                end += 1;
            }
            if end > start {
                // This assistant has tool_calls — check if ALL are orphaned.
                let all_orphaned = (start..end).all(|j| orphaned_indices.contains(&j));
                if all_orphaned {
                    // Only remove the assistant if it has no text content.
                    // An assistant with text + tool_calls is valid even after
                    // tool_calls are removed (build_chat_body emits it as a
                    // plain assistant message without tool_calls).
                    let has_text = match &conversation[i] {
                        ContextItem::Assistant { content } => !content.is_empty(),
                        _ => false,
                    };
                    if !has_text {
                        assistant_remove.insert(i);
                    }
                }
            }
            i = end;
        } else {
            i += 1;
        }
    }

    // ── Step 4: Rebuild conversation if anything changed ─────────
    if !orphaned_indices.is_empty() || !assistant_remove.is_empty() {
        let new_conv: Vec<ContextItem> = conversation
            .iter()
            .enumerate()
            .filter(|(idx, _)| !orphaned_indices.contains(idx) && !assistant_remove.contains(idx))
            .map(|(_, item)| item.clone())
            .collect();

        *conversation = new_conv;
        // Sync the projection — this is the structure `get_conversation` returns.
        projection.replace(conversation.clone());
    }
}

// ── Actor ──────────────────────────────────────────────────────────

/// Owns the conversation transcript. Spawned as a tokio task.
pub struct ChatStateActor {
    state: ChatState,
    cmd_rx: mpsc::UnboundedReceiver<ChatStateCommand>,
    cancel_token: CancellationToken,
}

impl ChatStateActor {
    /// Spawn the actor and return a handle.
    pub fn spawn() -> ChatStateHandle {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let actor = Self {
            state: ChatState::new(),
            cmd_rx,
            cancel_token: CancellationToken::new(),
        };
        tokio::spawn(actor.run());
        ChatStateHandle { cmd_tx }
    }

    async fn run(mut self) {
        loop {
            tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => break,
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(cmd) => self.handle(cmd),
                        None => break,
                    }
                }
            }
        }
    }

    fn handle(&mut self, cmd: ChatStateCommand) {
        match cmd {
            ChatStateCommand::PushUserMessage { item, reply } => {
                ensure_conversation_integrity(
                    &mut self.state.conversation,
                    &mut self.state.projection,
                );
                // Recalculate total_tokens after integrity repair.
                self.state.total_tokens =
                    self.state.conversation.iter().map(estimate_item_tokens).sum();
                self.state.push(item);
                self.state.prompt_index += 1;
                let _ = reply.send(());
            }
            ChatStateCommand::PushAssistantResponse { content, reply } => {
                let tokens = (content.len() as u64).div_ceil(4);
                let item = ContextItem::Assistant { content };
                self.state.push(item);
                self.state.estimated_tokens_since_model = 0;
                self.state.estimate_at_last_response = tokens;
                let _ = reply.send(());
            }
            ChatStateCommand::PushReasoning { content, reply } => {
                let item = ContextItem::ReasoningSummary { content };
                self.state.push(item);
                let _ = reply.send(());
            }
            ChatStateCommand::PushToolResult { call_id, content, is_error, reply } => {
                let item = ContextItem::ToolResult { call_id, content, is_error };
                self.state.push(item);
                let _ = reply.send(());
            }
            ChatStateCommand::PushToolCall { call_id, name, arguments, reply } => {
                let item = ContextItem::ToolCall { call_id, name, arguments };
                self.state.push(item);
                let _ = reply.send(());
            }
            ChatStateCommand::ReplaceConversation { items, is_compaction: _, reply } => {
                let new_total: u64 = items.iter().map(estimate_item_tokens).sum();
                self.state.conversation = items.clone();
                self.state.projection.replace(items);
                self.state.total_tokens = new_total;
                self.state.estimated_tokens_since_model = 0;
                self.state.estimate_at_last_response = new_total;
                let _ = reply.send(());
            }
            ChatStateCommand::GetConversation { reply } => {
                let _ = reply.send(self.state.projection.for_model().to_vec());
            }
            ChatStateCommand::GetConversationLen { reply } => {
                let _ = reply.send(self.state.conversation.len());
            }
            ChatStateCommand::GetTotalTokens { reply } => {
                let _ = reply.send(self.state.estimate_total());
            }
            ChatStateCommand::TruncateToPromptIndex { prompt_index, reply } => {
                // Count User items and truncate at the Nth user message.
                let mut user_count = 0usize;
                let mut idx = 0usize;
                for (i, item) in self.state.conversation.iter().enumerate() {
                    if matches!(item, ContextItem::User { .. }) {
                        if user_count == prompt_index {
                            idx = i;
                            break;
                        }
                        user_count += 1;
                    }
                }
                if idx > 0 {
                    self.state.conversation.truncate(idx);
                    self.state.prompt_index = prompt_index;
                    self.state.total_tokens =
                        self.state.conversation.iter().map(estimate_item_tokens).sum();
                    self.state.estimated_tokens_since_model = 0;
                }
                let _ = reply.send(self.state.conversation.clone());
            }
            ChatStateCommand::Shutdown => {
                self.cancel_token.cancel();
            }
        }
    }
}

// ── Handle ─────────────────────────────────────────────────────────

/// Cheaply-cloneable handle to the ChatStateActor.
#[derive(Debug, Clone)]
pub struct ChatStateHandle {
    cmd_tx: mpsc::UnboundedSender<ChatStateCommand>,
}

impl ChatStateHandle {
    pub async fn push_user_message(&self, item: ContextItem) {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd_tx.send(ChatStateCommand::PushUserMessage { item, reply: tx });
        let _ = rx.await;
    }

    pub async fn push_assistant_response(&self, content: String) {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd_tx.send(ChatStateCommand::PushAssistantResponse { content, reply: tx });
        let _ = rx.await;
    }

    /// Push a reasoning summary (thinking-mode CoT). Must be called BEFORE
    /// the matching `push_assistant_response` so ordering is preserved.
    pub async fn push_reasoning(&self, content: String) {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd_tx.send(ChatStateCommand::PushReasoning { content, reply: tx });
        let _ = rx.await;
    }

    pub async fn push_tool_result(&self, call_id: ToolCallId, content: String, is_error: bool) {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd_tx.send(ChatStateCommand::PushToolResult { call_id, content, is_error, reply: tx });
        let _ = rx.await;
    }

    pub async fn push_tool_call(&self, call_id: ToolCallId, name: String, arguments: serde_json::Value) {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd_tx.send(ChatStateCommand::PushToolCall { call_id, name, arguments, reply: tx });
        let _ = rx.await;
    }

    pub async fn replace_conversation(&self, items: Vec<ContextItem>, is_compaction: bool) {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd_tx.send(ChatStateCommand::ReplaceConversation { items, is_compaction, reply: tx });
        let _ = rx.await;
    }

    pub async fn get_conversation(&self) -> Vec<ContextItem> {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd_tx.send(ChatStateCommand::GetConversation { reply: tx });
        rx.await.unwrap_or_default()
    }

    pub async fn get_total_tokens(&self) -> u64 {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd_tx.send(ChatStateCommand::GetTotalTokens { reply: tx });
        rx.await.unwrap_or(0)
    }

    pub async fn truncate_to_prompt_index(&self, prompt_index: usize) -> Vec<ContextItem> {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd_tx.send(ChatStateCommand::TruncateToPromptIndex { prompt_index, reply: tx });
        rx.await.unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn push_and_retrieve() {
        let handle = ChatStateActor::spawn();
        handle.push_user_message(ContextItem::User { content: "hello".into(), message_id: None }).await;
        let conv = handle.get_conversation().await;
        assert_eq!(conv.len(), 1);
    }

    #[tokio::test]
    async fn compaction_replacement() {
        let handle = ChatStateActor::spawn();
        handle.push_user_message(ContextItem::User { content: "old".into(), message_id: None }).await;
        handle.replace_conversation(vec![ContextItem::System { content: "fresh".into() }], true).await;
        let conv = handle.get_conversation().await;
        assert_eq!(conv.len(), 1);
        assert!(matches!(conv[0], ContextItem::System { .. }));
    }

    #[tokio::test]
    async fn token_tracking() {
        let handle = ChatStateActor::spawn();
        handle.push_user_message(ContextItem::User { content: "hello world this is a test".into(), message_id: None }).await;
        let tokens = handle.get_total_tokens().await;
        assert!(tokens > 0);
    }

    /// Regression test: after cancel, orphaned ToolCalls and their
    /// empty Assistant message are removed before the next API request.
    #[tokio::test]
    async fn cancel_cleanup_orphaned_tool_calls() {
        let handle = ChatStateActor::spawn();
        let call_id_a = ToolCallId::new();
        let call_id_b = ToolCallId::new();

        // Simulate a turn: user → assistant (empty) → tool_calls (no results = cancel).
        handle.push_user_message(ContextItem::User { content: "do stuff".into(), message_id: None }).await;
        handle.push_assistant_response("".into()).await;
        handle.push_tool_call(call_id_a, "exec".into(), serde_json::json!({"command": "ls"})).await;
        handle.push_tool_call(call_id_b, "read".into(), serde_json::json!({"path": "/tmp"})).await;
        // User cancels here — no tool results pushed.

        // Next user message triggers ensure_conversation_integrity.
        handle.push_user_message(ContextItem::User { content: "nevermind".into(), message_id: None }).await;
        let conv = handle.get_conversation().await;

        // The orphaned ToolCalls and the empty Assistant should be gone.
        // Only the two User messages should remain.
        assert_eq!(conv.len(), 2, "expected 2 User messages, got: {conv:?}");
        assert!(matches!(conv[0], ContextItem::User { .. }));
        assert!(matches!(conv[1], ContextItem::User { .. }));
    }

    /// Assistant with text content is preserved even if all tool_calls are orphaned.
    #[tokio::test]
    async fn cancel_cleanup_keeps_assistant_with_text() {
        let handle = ChatStateActor::spawn();
        let call_id_a = ToolCallId::new();

        handle.push_user_message(ContextItem::User { content: "do stuff".into(), message_id: None }).await;
        handle.push_assistant_response("Let me call some tools.".into()).await;
        handle.push_tool_call(call_id_a, "exec".into(), serde_json::json!({"command": "ls"})).await;
        // Cancel — no results.

        handle.push_user_message(ContextItem::User { content: "nevermind".into(), message_id: None }).await;
        let conv = handle.get_conversation().await;

        // Assistant with text is kept (as a plain assistant message without tool_calls).
        // Only the orphaned ToolCall is removed.
        assert_eq!(conv.len(), 3, "expected User + Assistant(text) + User, got: {conv:?}");
        assert!(matches!(conv[1], ContextItem::Assistant { .. }));
    }

    /// Partial cancel: some tools completed, some didn't → remove only orphaned.
    #[tokio::test]
    async fn cancel_cleanup_partial_results() {
        let handle = ChatStateActor::spawn();
        let call_id_a = ToolCallId::new();
        let call_id_b = ToolCallId::new();

        handle.push_user_message(ContextItem::User { content: "do stuff".into(), message_id: None }).await;
        handle.push_assistant_response("".into()).await;
        handle.push_tool_call(call_id_a, "exec".into(), serde_json::json!({"command": "ls"})).await;
        handle.push_tool_call(call_id_b, "read".into(), serde_json::json!({"path": "/tmp"})).await;
        // Only tool A completed; tool B was cancelled.
        handle.push_tool_result(call_id_a, "file1\nfile2".into(), false).await;

        // Next user message triggers cleanup.
        handle.push_user_message(ContextItem::User { content: "continue".into(), message_id: None }).await;
        let conv = handle.get_conversation().await;

        // ToolCall B (orphaned) should be removed.
        // ToolCall A + ToolResult A should remain.
        // Assistant has empty text but ToolCall A survived → keep it.
        let has_orphaned = conv.iter().any(|i| matches!(i, ContextItem::ToolCall { call_id, .. } if *call_id == call_id_b));
        assert!(!has_orphaned, "orphaned ToolCall B should be removed");
        let has_a = conv.iter().any(|i| matches!(i, ContextItem::ToolCall { call_id, .. } if *call_id == call_id_a));
        assert!(has_a, "ToolCall A (with result) should remain");
    }
}
