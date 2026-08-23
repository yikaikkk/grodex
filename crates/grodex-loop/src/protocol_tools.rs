//! Live-loop adapters for the parent-child collaboration protocol
//! (Doc 12 §5.2/§14): the six model-facing tools registered into the
//! TurnCoordinator's tool registry.
//!
//! | Tool            | Backend                                             |
//! |-----------------|-----------------------------------------------------|
//! | `send_message`  | queue-only delivery via the mailbox router          |
//! | `followup_task` | protocol state machine + injected child executor    |
//! | `wait_agent`    | bounded polling over descendant targets             |
//! | `mailbox_read`  | read-then-ack (confirm after the result is returned)|
//! | `list_agents`   | tree listing with status + unread counts            |
//! | `interrupt_agent` | cancel the target's run, keep the identity        |
//!
//! `followup_task` TaskRuns are executed by an injected `DelegateTool`
//! (reusing its child sampling loop); the FIFO follow-up chain is driven
//! through `CollaborationProtocol::finish_task_run`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use grodex_core::error::GrodexError;
use grodex_core::id::OperationId;
use grodex_core::tool::ToolRuntime;
use grodex_subagent::{
    AgentId, CollaborationProtocol, ContextFork, FollowupOutcome, MailboxRouter, ProtocolConfig,
    SubAgentManager, TaskBudget, TaskId, WaitResult,
};
use serde_json::{Value, json};

use crate::delegate_tool::DelegateTool;

/// Shared, thread-safe handle over the protocol facade.
pub type SharedProtocol = Arc<Mutex<CollaborationProtocol>>;

/// Hosts the collaboration protocol for one live session.
///
/// The session itself is the ROOT agent: every tool call issued by the
/// main loop runs as `caller = root`, so descendant/ownership checks
/// cover exactly the agents this session created.
pub struct ProtocolToolHost {
    protocol: SharedProtocol,
    caller: AgentId,
    default_budget: TaskBudget,
}

impl ProtocolToolHost {
    /// Create the host: fresh manager + router, root node registered.
    pub fn new(max_children: usize, config: ProtocolConfig) -> Self {
        let mut manager = SubAgentManager::new(max_children.max(1));
        let mut router = MailboxRouter::new();
        let root = manager.register_root();
        router.register(root);
        Self {
            protocol: Arc::new(Mutex::new(CollaborationProtocol::new(
                manager,
                router,
                config,
            ))),
            caller: root,
            default_budget: TaskBudget {
                max_turns: Some(8),
                max_duration_secs: None,
            },
        }
    }

    pub fn protocol(&self) -> SharedProtocol {
        self.protocol.clone()
    }

    pub fn caller(&self) -> AgentId {
        self.caller
    }

    pub fn default_budget(&self) -> TaskBudget {
        self.default_budget.clone()
    }

    /// Register a protocol-tracked child (node + mailbox) WITHOUT a
    /// TaskRun — for children whose execution happens elsewhere.
    pub fn spawn_child(&self, label: &str) -> Result<AgentId, String> {
        let mut p = self.protocol.lock().unwrap();
        let id = p.manager_mut().register_node(self.caller, label)?;
        p.router_mut().register(id);
        Ok(id)
    }

    /// Build the six `(name, runtime, input_schema)` triples for the
    /// TurnCoordinator tool registry. `executor` runs follow-up TaskRuns
    /// (None → runs are recorded but never executed).
    pub fn tool_set(
        self: &Arc<Self>,
        executor: Option<Arc<DelegateTool>>,
    ) -> Vec<(String, Arc<dyn ToolRuntime>, Value)> {
        use ProtocolToolKind::*;
        [
            (SendMessage, "send_message"),
            (FollowupTask, "followup_task"),
            (WaitAgent, "wait_agent"),
            (MailboxRead, "mailbox_read"),
            (ListAgents, "list_agents"),
            (InterruptAgent, "interrupt_agent"),
        ]
        .into_iter()
        .map(|(kind, name)| {
            let adapter = Arc::new(ProtocolToolAdapter {
                host: self.clone(),
                kind,
                executor: executor.clone(),
            });
            (
                name.to_string(),
                adapter.clone() as Arc<dyn ToolRuntime>,
                adapter.input_schema(),
            )
        })
        .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolToolKind {
    SendMessage,
    FollowupTask,
    WaitAgent,
    MailboxRead,
    ListAgents,
    InterruptAgent,
}

/// One model-facing tool backed by the collaboration protocol.
pub struct ProtocolToolAdapter {
    host: Arc<ProtocolToolHost>,
    kind: ProtocolToolKind,
    /// Executes follow-up TaskRuns; shared with the `delegate_task` tool.
    executor: Option<Arc<DelegateTool>>,
}

impl ProtocolToolAdapter {
    pub fn input_schema(&self) -> Value {
        let s = |props: Value, required: &[&str]| {
            json!({
                "type": "object",
                "properties": props,
                "required": required,
                "additionalProperties": false,
            })
        };
        let agent = json!({"type": "string", "description": "Target agent id (uuid)"});
        match self.kind {
            ProtocolToolKind::SendMessage => s(
                json!({
                    "target": agent,
                    "message": {"type": "string", "description": "Message body (queue-only; does NOT start a turn)"},
                }),
                &["target", "message"],
            ),
            ProtocolToolKind::FollowupTask => s(
                json!({
                    "target": agent,
                    "task": {"type": "string", "description": "Follow-up instruction; starts/resumes the target"},
                    "max_turns": {"type": "integer", "description": "Optional turn budget for the run"},
                }),
                &["target", "task"],
            ),
            ProtocolToolKind::WaitAgent => s(
                json!({
                    "targets": {"type": "array", "items": {"type": "string"}, "description": "Descendant agent ids to wait on"},
                    "timeout_secs": {"type": "integer", "description": "Requested timeout (clamped by the foreground lease)"},
                }),
                &["targets"],
            ),
            ProtocolToolKind::MailboxRead => s(
                json!({
                    "limit": {"type": "integer", "description": "Max messages to read (default 20)"},
                }),
                &[],
            ),
            ProtocolToolKind::ListAgents => s(
                json!({
                    "path_prefix": {"type": "string", "description": "Optional path filter, e.g. /main/reviewer"},
                }),
                &[],
            ),
            ProtocolToolKind::InterruptAgent => {
                s(json!({"target": agent}), &["target"])
            }
        }
    }

    fn parse_agent(args: &Value, key: &str) -> Result<AgentId, GrodexError> {
        let s = args
            .get(key)
            .and_then(|v| v.as_str())
            .ok_or_else(|| GrodexError::ToolExecution(format!("missing '{key}'")))?;
        AgentId::from_string(s).ok_or_else(|| GrodexError::InvalidId(s.to_string()))
    }

    fn protocol_err(e: grodex_subagent::ProtocolError) -> GrodexError {
        GrodexError::ToolExecution(e.to_string())
    }
}

#[async_trait]
impl ToolRuntime for ProtocolToolAdapter {
    async fn execute(&self, args: Value, _operation_id: OperationId) -> Result<Value, GrodexError> {
        let caller = self.host.caller();
        match self.kind {
            // 1. send_message — queue-only, never starts a Turn (§5.2).
            ProtocolToolKind::SendMessage => {
                let target = Self::parse_agent(&args, "target")?;
                let message = args
                    .get("message")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| GrodexError::ToolExecution("missing 'message'".into()))?;
                let id = self
                    .host
                    .protocol()
                    .lock()
                    .unwrap()
                    .send_message(caller, target, message)
                    .map_err(Self::protocol_err)?;
                Ok(json!({"message_id": id, "queued": true}))
            }

            // 2. followup_task — deliver + start/resume; busy targets
            // queue FIFO and run after the current TaskRun terminates.
            ProtocolToolKind::FollowupTask => {
                let target = Self::parse_agent(&args, "target")?;
                let task = args
                    .get("task")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| GrodexError::ToolExecution("missing 'task'".into()))?
                    .to_string();
                let mut budget = self.host.default_budget();
                if let Some(t) = args.get("max_turns").and_then(|v| v.as_u64()) {
                    budget.max_turns = Some(t as u32);
                }
                let outcome = self
                    .host
                    .protocol()
                    .lock()
                    .unwrap()
                    .followup_task(caller, target, task.clone(), ContextFork::None, budget)
                    .map_err(Self::protocol_err)?;
                match outcome {
                    FollowupOutcome::Triggered {
                        message_id,
                        task_run_id,
                    } => {
                        if let Some(executor) = self.executor.clone() {
                            tokio::spawn(run_child_chain(
                                self.host.clone(),
                                executor,
                                task_run_id,
                                task,
                            ));
                        }
                        Ok(json!({
                            "status": "triggered",
                            "message_id": message_id,
                            "task_run_id": task_run_id.to_string(),
                        }))
                    }
                    FollowupOutcome::Queued { message_id } => Ok(json!({
                        "status": "queued",
                        "message_id": message_id,
                        "note": "target busy; run starts after the current TaskRun terminates",
                    })),
                }
            }

            // 3. wait_agent — bounded wait over descendant targets.
            // The first call validates + computes the effective timeout;
            // we then poll task states and finish with one canonical call.
            ProtocolToolKind::WaitAgent => {
                let targets: Vec<AgentId> = args
                    .get("targets")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| GrodexError::ToolExecution("missing 'targets'".into()))?
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .and_then(AgentId::from_string)
                            .ok_or_else(|| GrodexError::InvalidId(v.to_string()))
                    })
                    .collect::<Result<_, _>>()?;
                let requested = args
                    .get("timeout_secs")
                    .and_then(|v| v.as_u64())
                    .map(Duration::from_secs);

                let first = self
                    .host
                    .protocol()
                    .lock()
                    .unwrap()
                    .wait_agent(caller, &targets, requested, None)
                    .map_err(Self::protocol_err)?;
                let result = if first.pending.is_empty() {
                    first
                } else {
                    let deadline = Instant::now() + first.effective_timeout;
                    loop {
                        let all_terminal = {
                            let proto = self.host.protocol();
                            let p = proto.lock().unwrap();
                            targets.iter().all(|t| {
                                // Match wait_agent semantics: never-run is
                                // NOT finished (nothing to observe yet).
                                p.manager()
                                    .tasks_of(t)
                                    .first()
                                    .map(|r| r.is_terminal())
                                    .unwrap_or(false)
                            })
                        };
                        if all_terminal || Instant::now() >= deadline {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    self.host
                        .protocol()
                        .lock()
                        .unwrap()
                        .wait_agent(caller, &targets, requested, None)
                        .map_err(Self::protocol_err)?
                };
                Ok(serialize_wait(&result))
            }

            // 4. mailbox_read — read-then-ack (§14.1). The StepRunner
            // commits the ToolResult to the transcript immediately after
            // `execute` returns, so confirming at the end of this call
            // preserves at-least-once delivery: a crash before commit
            // leaves the cursor untouched and messages redeliver.
            ProtocolToolKind::MailboxRead => {
                let limit = args
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .filter(|v| *v > 0)
                    .unwrap_or(20);
                let mut proto = self.host.protocol();
                let mut p = proto.lock().unwrap();
                let read = p
                    .mailbox_read(caller, limit)
                    .map_err(Self::protocol_err)?;
                let confirmed = p
                    .confirm_consumed(caller, &read.pending_confirmation)
                    .map_err(Self::protocol_err)?;
                let messages: Vec<Value> = read
                    .messages
                    .iter()
                    .map(|m| {
                        json!({
                            "message_id": m.message_id.to_string(),
                            "author": m.author_agent_id.to_string(),
                            "kind": format!("{:?}", m.kind),
                            "payload": m.payload,
                        })
                    })
                    .collect();
                Ok(json!({
                    "messages": messages,
                    "confirmed": confirmed,
                    "unread_remaining": read.next_cursor.saturating_sub(confirmed as u64),
                }))
            }

            // 5. list_agents — tree + status + latest run + unread.
            ProtocolToolKind::ListAgents => {
                let prefix = args.get("path_prefix").and_then(|v| v.as_str());
                let rows = self.host.protocol().lock().unwrap().list_agents(prefix);
                let agents: Vec<Value> = rows
                    .iter()
                    .map(|r| {
                        json!({
                            "agent_id": r.agent_id.to_string(),
                            "path": r.path,
                            "status": format!("{:?}", r.status),
                            "latest_task_run_id": r.latest_task_run_id.map(|t| t.to_string()),
                            "latest_task_terminal": r.latest_task_terminal,
                            "unread_messages": r.unread_messages,
                        })
                    })
                    .collect();
                Ok(json!({"agents": agents}))
            }

            // 6. interrupt_agent — cancel the target run, keep the node.
            ProtocolToolKind::InterruptAgent => {
                let target = Self::parse_agent(&args, "target")?;
                let n = self
                    .host
                    .protocol()
                    .lock()
                    .unwrap()
                    .interrupt_agent(caller, target)
                    .map_err(Self::protocol_err)?;
                Ok(json!({"target": target.to_string(), "interrupted_tasks": n}))
            }
        }
    }
}

fn serialize_wait(r: &WaitResult) -> Value {
    let states = |list: &[grodex_subagent::WaitTargetState]| -> Vec<Value> {
        list.iter()
            .map(|s| {
                json!({
                    "agent_id": s.agent_id.to_string(),
                    "finished": s.finished,
                    "latest_task_status": s.latest_task.map(|(_, st)| format!("{st:?}")),
                    "preview": s.preview,
                })
            })
            .collect()
    };
    json!({
        "finished": states(&r.finished),
        "pending": states(&r.pending),
        "timed_out": r.timed_out,
        "effective_timeout_secs": r.effective_timeout.as_secs(),
    })
}

/// Execute a follow-up TaskRun through the DelegateTool, then walk the
/// FIFO chain: each terminal run may release the next queued follow-up.
async fn run_child_chain(
    host: Arc<ProtocolToolHost>,
    executor: Arc<DelegateTool>,
    mut task_id: TaskId,
    mut payload: String,
) {
    loop {
        let outcome = executor
            .execute(json!({"task": payload}), OperationId::new())
            .await
            .map(|v| {
                v.get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .map_err(|e| e.to_string());
        let next = host.protocol().lock().unwrap().finish_task_run(
            task_id,
            outcome,
            ContextFork::None,
            host.default_budget(),
        );
        match next {
            Ok(Some((id, p))) => {
                task_id = id;
                payload = p;
            }
            _ => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Arc<ProtocolToolHost> {
        Arc::new(ProtocolToolHost::new(8, ProtocolConfig::default()))
    }

    fn adapter(host: &Arc<ProtocolToolHost>, kind: ProtocolToolKind) -> ProtocolToolAdapter {
        ProtocolToolAdapter {
            host: host.clone(),
            kind,
            executor: None,
        }
    }

    async fn exec(a: &ProtocolToolAdapter, args: Value) -> Result<Value, GrodexError> {
        a.execute(args, OperationId::new()).await
    }

    #[tokio::test]
    async fn tool_set_exposes_all_six_tools() {
        let h = host();
        let set = h.tool_set(None);
        assert_eq!(set.len(), 6);
        let names: Vec<&str> = set.iter().map(|(n, _, _)| n.as_str()).collect();
        for expected in [
            "send_message",
            "followup_task",
            "wait_agent",
            "mailbox_read",
            "list_agents",
            "interrupt_agent",
        ] {
            assert!(names.contains(&expected), "{expected} missing");
        }
    }

    #[tokio::test]
    async fn send_then_read_round_trip_confirms_cursor() {
        let h = host();
        let child = h.spawn_child("reviewer").unwrap();

        // Child → root message.
        {
            let proto = h.protocol();
            let mut p = proto.lock().unwrap();
            p.send_message(child, h.caller(), "analysis done").unwrap();
        }

        let read = exec(&adapter(&h, ProtocolToolKind::MailboxRead), json!({})).await.unwrap();
        let msgs = read["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["payload"], "analysis done");
        // read-then-ack: cursor advanced within the same call.
        assert_eq!(read["confirmed"], 1);
        assert_eq!(read["unread_remaining"], 0);

        // Second read returns nothing.
        let again = exec(&adapter(&h, ProtocolToolKind::MailboxRead), json!({})).await.unwrap();
        assert!(again["messages"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn followup_idle_triggers_busy_queues_fifo() {
        let h = host();
        let child = h.spawn_child("worker").unwrap();
        let ft = adapter(&h, ProtocolToolKind::FollowupTask);

        // Idle → triggered (no executor: the TaskRun stays recorded).
        let r1 = exec(
            &ft,
            json!({"target": child.to_string(), "task": "first job"}),
        )
        .await
        .unwrap();
        assert_eq!(r1["status"], "triggered");
        let run1 = TaskId::from_string(r1["task_run_id"].as_str().unwrap());

        // Busy → queued.
        let r2 = exec(
            &ft,
            json!({"target": child.to_string(), "task": "second job"}),
        )
        .await
        .unwrap();
        assert_eq!(r2["status"], "queued");

        // Finish the run → FIFO releases the queued follow-up.
        let next = {
            let proto = h.protocol();
            proto
                .lock()
                .unwrap()
                .finish_task_run(run1, Ok("done".into()), ContextFork::None, h.default_budget())
                .unwrap()
        };
        let (run2, payload) = next.expect("queued followup must be released");
        assert_eq!(payload, "second job");
        let proto = h.protocol();
        assert!(proto.lock().unwrap().manager().get_task(&run2).is_some());
    }

    #[tokio::test]
    async fn wait_agent_rejects_non_descendant_targets() {
        let h = host();
        let child = h.spawn_child("worker").unwrap();
        let other_host = host();
        let stranger = other_host.spawn_child("stranger").unwrap();

        let wait = adapter(&h, ProtocolToolKind::WaitAgent);
        // Own child: trigger a run, finish it, then wait → finished.
        let ft = adapter(&h, ProtocolToolKind::FollowupTask);
        let r = exec(&ft, json!({"target": child.to_string(), "task": "quick"}))
            .await
            .unwrap();
        let run = TaskId::from_string(r["task_run_id"].as_str().unwrap());
        {
            let proto = h.protocol();
            proto
                .lock()
                .unwrap()
                .finish_task_run(run, Ok("done".into()), ContextFork::None, h.default_budget())
                .unwrap();
        }
        let ok = exec(&wait, json!({"targets": [child.to_string()]})).await.unwrap();
        assert_eq!(ok["timed_out"], false);
        assert_eq!(ok["finished"].as_array().unwrap().len(), 1);
        // Agent from another session's tree: unknown here → not found.
        let err = exec(&wait, json!({"targets": [stranger.to_string()]}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
        // Root itself exists but is not a descendant of the caller
        // (self/ancestor waits are forbidden) → NotDescendant.
        let err = exec(&wait, json!({"targets": [h.caller().to_string()]}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not a descendant"));
    }

    #[tokio::test]
    async fn interrupt_cancels_run_and_keeps_node() {
        let h = host();
        let child = h.spawn_child("worker").unwrap();
        let ft = adapter(&h, ProtocolToolKind::FollowupTask);
        exec(&ft, json!({"target": child.to_string(), "task": "long job"}))
            .await
            .unwrap();

        let n = exec(
            &adapter(&h, ProtocolToolKind::InterruptAgent),
            json!({"target": child.to_string()}),
        )
        .await
        .unwrap();
        assert_eq!(n["interrupted_tasks"], 1);

        // Node survives and stays addressable.
        let list = exec(
            &adapter(&h, ProtocolToolKind::ListAgents),
            json!({"path_prefix": "/main"}),
        )
        .await
        .unwrap();
        let agents = list["agents"].as_array().unwrap();
        assert!(agents.iter().any(|a| a["agent_id"] == child.to_string()));
    }

    #[tokio::test]
    async fn unknown_agent_is_rejected() {
        let h = host();
        let ghost = AgentId::new();
        let err = exec(
            &adapter(&h, ProtocolToolKind::SendMessage),
            json!({"target": ghost.to_string(), "message": "hi"}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
