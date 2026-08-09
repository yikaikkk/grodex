use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StopDecision {
    Stop,
    Continue {
        reason_code: String,
        template_version: String,
        runtime_message_payload: serde_json::Value,
    },
}

#[derive(Debug, Clone)]
pub struct GateContext {
    pub has_pending_inputs: bool,
    pub steps_executed_so_far: u32,
    pub max_steps: u32,
    pub active_todo_count: u32,
    pub errors_in_current_turn: u32,
    pub last_model_content_is_terminal_answer: bool,
}

pub trait TerminationGate: Send + Sync {
    fn gate_id(&self) -> &str;
    fn evaluate(&self, ctx: &GateContext) -> StopDecision;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminationGateEvaluated {
    pub gate_id: String,
    pub inputs_hash: String,
    pub decision: StopDecision,
    pub evaluated_at: DateTime<Utc>,
}

pub fn evaluate_chain(
    gates: &[Box<dyn TerminationGate>],
    ctx: &GateContext,
) -> (StopDecision, Vec<TerminationGateEvaluated>) {
    let mut events: Vec<TerminationGateEvaluated> = Vec::with_capacity(gates.len());
    for gate in gates {
        let decision = if ctx.has_pending_inputs {
            StopDecision::Continue {
                reason_code: "pending_input_queued".into(),
                template_version: "termination/v1/pending-input".into(),
                runtime_message_payload: serde_json::json!({
                    "info": "user inputs are waiting; turn will continue after commit",
                }),
            }
        } else {
            gate.evaluate(ctx)
        };
        let inputs_hash = compute_gate_input_hash(gate.gate_id(), ctx);
        events.push(TerminationGateEvaluated {
            gate_id: gate.gate_id().to_string(),
            inputs_hash,
            decision,
            evaluated_at: Utc::now(),
        });
        if let StopDecision::Continue { .. } = events.last().unwrap().decision {
            if !ctx.has_pending_inputs {
                let final_decision = events.last().unwrap().decision.clone();
                return (final_decision, events);
            }
        }
    }
    let final_decision = if ctx.has_pending_inputs {
        events.iter().find_map(|e| match &e.decision {
            s @ StopDecision::Continue { .. } => Some(s.clone()),
            _ => None,
        }).unwrap_or(StopDecision::Stop)
    } else {
        StopDecision::Stop
    };
    (final_decision, events)
}

fn compute_gate_input_hash(gate_id: &str, ctx: &GateContext) -> String {
    let mut h = Sha256::new();
    h.update(gate_id);
    h.update((ctx.has_pending_inputs as u8).to_le_bytes());
    h.update(ctx.steps_executed_so_far.to_le_bytes());
    h.update(ctx.max_steps.to_le_bytes());
    h.update(ctx.active_todo_count.to_le_bytes());
    h.update(ctx.errors_in_current_turn.to_le_bytes());
    h.update((ctx.last_model_content_is_terminal_answer as u8).to_le_bytes());
    format!("{:x}", h.finalize())[..16].to_string()
}

pub struct MaxStepsGate;
impl TerminationGate for MaxStepsGate {
    fn gate_id(&self) -> &str { "turn.max_steps" }
    fn evaluate(&self, ctx: &GateContext) -> StopDecision {
        if ctx.steps_executed_so_far >= ctx.max_steps {
            StopDecision::Continue {
                reason_code: "max_steps_exceeded".into(),
                template_version: "termination/v1/max-steps".into(),
                runtime_message_payload: serde_json::json!({
                    "steps_done": ctx.steps_executed_so_far,
                    "max_steps": ctx.max_steps,
                    "message": "step budget exhausted; please summarize or request continuation"
                }),
            }
        } else {
            StopDecision::Stop
        }
    }
}

pub struct OpenTodosGate;
impl TerminationGate for OpenTodosGate {
    fn gate_id(&self) -> &str { "turn.open_todos" }
    fn evaluate(&self, ctx: &GateContext) -> StopDecision {
        if ctx.active_todo_count > 0 {
            StopDecision::Continue {
                reason_code: "open_todos_remaining".into(),
                template_version: "termination/v1/open-todos".into(),
                runtime_message_payload: serde_json::json!({
                    "active_todo_count": ctx.active_todo_count,
                    "message": "todo items still incomplete"
                }),
            }
        } else { StopDecision::Stop }
    }
}

pub struct TerminalAnswerGate;
impl TerminationGate for TerminalAnswerGate {
    fn gate_id(&self) -> &str { "turn.terminal_answer" }
    fn evaluate(&self, ctx: &GateContext) -> StopDecision {
        if !ctx.last_model_content_is_terminal_answer {
            StopDecision::Continue {
                reason_code: "no_terminal_answer".into(),
                template_version: "termination/v1/no-terminal".into(),
                runtime_message_payload: serde_json::json!({
                    "message": "model has not produced a final answer; please request a summary"
                }),
            }
        } else { StopDecision::Stop }
    }
}
