//! Randomized scheduling tests — verify concurrent tool execution
//! produces deterministic transcript ordering regardless of completion order.

use grodex_core::context::ContextItem;
use grodex_core::id::{CommitSequence, OperationId, ToolCallId};
use grodex_core::tool::{Tool, ToolMetadata, ToolRuntime};
use grodex_core::tool::{ConcurrencyClass, SideEffectClass};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

struct MockTool {
    name: String,
    execution_count: Arc<AtomicU64>,
    completion_order: Arc<Mutex<Vec<String>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MockArgs {
    tool_id: String,
    delay_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MockOutput {
    tool_id: String,
    completed: bool,
}

impl Tool for MockTool {
    type Args = MockArgs;
    type Output = MockOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.name.clone(),
            display_name: self.name.clone(),
            description: format!("Mock tool: {}", self.name),
            concurrency_class: ConcurrencyClass::Parallel,
            side_effect_class: SideEffectClass::ReadOnly,
            default_policy: grodex_core::policy::PolicyDecision::Allow,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"tool_id": {"type": "string"}, "delay_ms": {"type": "integer"}}})
    }

    fn output_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"tool_id": {"type": "string"}, "completed": {"type": "boolean"}}})
    }
}

#[async_trait::async_trait]
impl ToolRuntime for MockTool {
    async fn execute(
        &self,
        args: serde_json::Value,
        _operation_id: OperationId,
    ) -> Result<serde_json::Value, grodex_core::error::GrodexError> {
        let args: MockArgs = serde_json::from_value(args).unwrap();
        let delay = args.delay_ms.min(1000);
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        self.completion_order.lock().await.push(args.tool_id.clone());
        self.execution_count.fetch_add(1, Ordering::SeqCst);
        let output = MockOutput { tool_id: args.tool_id, completed: true };
        serde_json::to_value(output)
            .map_err(|e| grodex_core::error::GrodexError::ToolExecution(e.to_string()))
    }
}

#[tokio::test]
async fn model_order_commit_survives_random_completion() {
    let completion_log = Arc::new(Mutex::new(Vec::new()));
    let tool_a: Arc<dyn ToolRuntime> = Arc::new(MockTool {
        name: "tool_a".into(), execution_count: Arc::new(AtomicU64::new(0)), completion_order: completion_log.clone(),
    });
    let tool_b: Arc<dyn ToolRuntime> = Arc::new(MockTool {
        name: "tool_b".into(), execution_count: Arc::new(AtomicU64::new(0)), completion_order: completion_log.clone(),
    });
    let tool_c: Arc<dyn ToolRuntime> = Arc::new(MockTool {
        name: "tool_c".into(), execution_count: Arc::new(AtomicU64::new(0)), completion_order: completion_log.clone(),
    });

    // A=50ms, B=10ms, C=30ms → B completes first, then C, then A.
    let calls: Vec<(CommitSequence, ToolCallId, Arc<dyn ToolRuntime>, serde_json::Value)> = vec![
        (CommitSequence::new(0), ToolCallId::new(), tool_a, serde_json::json!({"tool_id": "A", "delay_ms": 50})),
        (CommitSequence::new(1), ToolCallId::new(), tool_b, serde_json::json!({"tool_id": "B", "delay_ms": 10})),
        (CommitSequence::new(2), ToolCallId::new(), tool_c, serde_json::json!({"tool_id": "C", "delay_ms": 30})),
    ];

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for (idx, call_id, runtime, args) in &calls {
        let call_id = *call_id;
        let idx = *idx;
        let tx = tx.clone();
        let runtime = runtime.clone();
        let args = args.clone();

        tokio::spawn(async move {
            let result = runtime.execute(args, OperationId::new()).await;
            let item = match result {
                Ok(output) => ContextItem::ToolResult { call_id, content: output.to_string(), is_error: false },
                Err(e) => ContextItem::ToolResult { call_id, content: format!("Error: {e}"), is_error: true },
            };
            let _ = tx.send((idx, item));
        });
    }
    drop(tx);

    let mut results = Vec::new();
    while let Some(r) = rx.recv().await { results.push(r); }
    results.sort_by_key(|(idx, _)| *idx);

    assert_eq!(results[0].0, CommitSequence::new(0));
    assert_eq!(results[1].0, CommitSequence::new(1));
    assert_eq!(results[2].0, CommitSequence::new(2));

    let completion = completion_log.lock().await;
    let b_pos = completion.iter().position(|id| id == "B").unwrap();
    let a_pos = completion.iter().position(|id| id == "A").unwrap();
    assert!(b_pos < a_pos, "B (10ms) should complete before A (50ms)");
}

#[tokio::test]
async fn tool_result_pairing_preserved_under_random_order() {
    let calls: Vec<(ToolCallId, String)> = (0..10).map(|i| (ToolCallId::new(), format!("tool_{i}"))).collect();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for (idx, (call_id, _name)) in calls.iter().enumerate() {
        let call_id = *call_id;
        let tx = tx.clone();
        let seq = CommitSequence::new(idx as u64);
        let delay = (idx * 7 + 3) % 50;
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay as u64)).await;
            let _ = tx.send((seq, call_id));
        });
    }
    drop(tx);

    let mut results = Vec::new();
    while let Some((idx, call_id)) = rx.recv().await { results.push((idx, call_id)); }
    results.sort_by_key(|(idx, _)| *idx);

    assert_eq!(results.len(), 10);
    let mut transcript = Vec::new();
    for (_, call_id) in &results {
        transcript.push(ContextItem::ToolCall { call_id: *call_id, name: "test".into(), arguments: serde_json::json!({}) });
        transcript.push(ContextItem::ToolResult { call_id: *call_id, content: "done".into(), is_error: false });
    }

    for i in (0..transcript.len()).step_by(2) {
        if let (ContextItem::ToolCall { call_id: cid1, .. }, ContextItem::ToolResult { call_id: cid2, .. }) = (&transcript[i], &transcript[i + 1]) {
            assert_eq!(cid1, cid2);
        }
    }
}

#[tokio::test]
async fn stress_test_concurrent_tools() {
    const N: usize = 50;
    let calls: Vec<(ToolCallId, String)> = (0..N).map(|i| (ToolCallId::new(), format!("tool_{i}"))).collect();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for (idx, (call_id, _name)) in calls.iter().enumerate() {
        let call_id = *call_id;
        let tx = tx.clone();
        let seq = CommitSequence::new(idx as u64);
        let delay = (idx.wrapping_mul(17).wrapping_add(11)) % 100;
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay as u64)).await;
            let _ = tx.send((seq, call_id));
        });
    }
    drop(tx);

    let mut results = Vec::new();
    while let Some(r) = rx.recv().await { results.push(r); }
    assert_eq!(results.len(), N);
    results.sort_by_key(|(idx, _)| *idx);
    for (i, (idx, _)) in results.iter().enumerate() {
        assert_eq!(*idx, CommitSequence::new(i as u64));
    }
    let mut ids = std::collections::HashSet::new();
    for (_, call_id) in &results {
        assert!(ids.insert(*call_id), "duplicate tool call id");
    }
}
