//! Turn — one user goal and its resulting model interaction steps.

use chrono::{DateTime, Utc};
use grodex_core::id::{StepId, TurnId};
use grodex_core::state::TurnState;
use grodex_provider::WireProtocol;
use grodex_provider::binding::ModelBinding;
use grodex_provider::canonical_event::CanonicalModelResponse;
use grodex_provider::canonical_request::InstructionBlock;
use grodex_provider::usage::SettledUsage;

/// One user goal within a Session.
///
/// A Turn may contain multiple Steps (model samples + tool batches)
/// but Phase 1 uses a single Step for text-only interaction.
#[derive(Debug)]
pub struct Turn {
    pub id: TurnId,
    pub state: TurnState,
    /// The original user input that started this Turn.
    pub user_input: String,
    /// Results of each sampling Step within this Turn.
    pub steps: Vec<StepResult>,
    /// When the Turn was created.
    pub started_at: DateTime<Utc>,
}

impl Turn {
    pub fn new(user_input: String) -> Self {
        Self {
            id: TurnId::new(),
            state: TurnState::Admitted,
            user_input,
            steps: Vec::new(),
            started_at: Utc::now(),
        }
    }

    /// Record a completed Step.
    pub fn record_step(&mut self, result: StepResult) {
        self.steps.push(result);
    }
}

/// The result of one sampling Step.
#[derive(Debug, Clone)]
pub struct StepResult {
    pub step_id: StepId,
    pub response: Option<CanonicalModelResponse>,
    pub error: Option<grodex_sampler::SamplingError>,
    pub usage: Option<SettledUsage>,
    pub tool_calls: Vec<crate::step::ToolCallResult>,
    pub elapsed_ms: u64,
}

/// Frozen context for a Turn, used when building model requests.
#[derive(Debug, Clone)]
pub struct TurnContext {
    pub session_id: grodex_core::id::SessionId,
    pub turn_id: TurnId,
    /// The model binding to use for all Steps in this Turn.
    pub model_binding: ModelBinding,
    /// Instructions prepended to every model request.
    pub instructions: Vec<InstructionBlock>,
}

impl TurnContext {
    /// Create a TurnContext with a default ModelBinding for the Responses protocol.
    pub fn new(
        session_id: grodex_core::id::SessionId,
        turn_id: TurnId,
        instructions: Vec<InstructionBlock>,
    ) -> Self {
        let model_binding = ModelBinding::new(
            "openai".into(), 1, "gpt-5".into(), 1, WireProtocol::Responses,
        );
        Self { session_id, turn_id, model_binding, instructions }
    }

    /// Create with explicit model configuration.
    pub fn with_model(
        session_id: grodex_core::id::SessionId,
        turn_id: TurnId,
        instructions: Vec<InstructionBlock>,
        provider: &str,
        model: &str,
        wire: WireProtocol,
    ) -> Self {
        let model_binding = ModelBinding::new(
            provider.to_string(), 1, model.to_string(), 1, wire,
        );
        Self { session_id, turn_id, model_binding, instructions }
    }
}
