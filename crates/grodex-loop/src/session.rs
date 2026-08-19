//! Session — the long-lived conversation container.
//!
//! Holds the accumulated context, tracks current Turn, and enforces
//! SessionState transitions.

use chrono::{DateTime, Utc};
use grodex_config::LoadedConfig;
use grodex_core::context::ContextItem;
use grodex_core::id::{SessionId, TurnId};
use grodex_core::state::SessionState;

use crate::turn::Turn;

/// The long-lived session. There is exactly one per conversation.
#[derive(Debug)]
pub struct Session {
    pub id: SessionId,
    pub state: SessionState,
    /// Accumulated conversation history. Grows monotonically; compaction
    /// replaces the *projection* but never deletes from here.
    pub context: Vec<ContextItem>,
    /// The currently active Turn, if any.
    pub current_turn: Option<Turn>,
    /// Loaded configuration.
    pub config: LoadedConfig,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
}

impl Session {
    /// Create a new session in the Initializing state.
    pub fn new(config: LoadedConfig) -> Self {
        Self {
            id: SessionId::new(),
            state: SessionState::Initializing,
            context: Vec::new(),
            current_turn: None,
            config,
            created_at: Utc::now(),
        }
    }

    /// Transition to a new state. Returns an error if the transition
    /// is not valid.
    pub fn transition_to(&mut self, new_state: SessionState) -> Result<(), String> {
        let valid = matches!(
            (self.state, new_state),
            (SessionState::Initializing, SessionState::Idle)
                | (SessionState::Idle, SessionState::Running)
                | (SessionState::Running, SessionState::Idle)
                | (SessionState::Idle, SessionState::ShuttingDown)
                | (SessionState::Running, SessionState::ShuttingDown)
                | (SessionState::ShuttingDown, SessionState::Closed)
                | (SessionState::Idle, SessionState::Closed)
        );

        if valid {
            self.state = new_state;
            Ok(())
        } else {
            Err(format!("invalid state transition: {:?} -> {:?}", self.state, new_state))
        }
    }

    /// Admit a new Turn. Fails if a Turn is already in progress.
    pub fn admit_turn(&mut self, user_input: String) -> Result<TurnId, String> {
        if self.current_turn.is_some() {
            return Err("a turn is already in progress".into());
        }

        self.transition_to(SessionState::Running)?;

        let user_item = ContextItem::User {
            content: user_input.clone(),
            message_id: None,
        };
        self.context.push(user_item);

        let turn = Turn::new(user_input);
        let turn_id = turn.id;
        self.current_turn = Some(turn);

        Ok(turn_id)
    }

    /// Complete the current Turn, recording the assistant response.
    pub fn complete_turn(&mut self, assistant_text: &str) -> Result<(), String> {
        let _turn = self.current_turn.take().ok_or("no turn in progress")?;

        let assistant_item = ContextItem::Assistant {
            content: assistant_text.to_string(),
        };
        self.context.push(assistant_item);

        self.transition_to(SessionState::Idle)?;
        Ok(())
    }

    /// Cancel the current Turn, recording the cancellation.
    ///
    /// Idempotent: if the session is already Idle (e.g. a previous cancel
    /// already ran, or the turn completed normally), this is a no-op.
    /// This prevents "invalid state transition: Idle -> Idle" errors when
    /// the user presses Esc multiple times.
    pub fn cancel_turn(&mut self) -> Result<(), String> {
        self.current_turn = None;
        if self.state == SessionState::Idle {
            return Ok(());
        }
        self.transition_to(SessionState::Idle)?;
        Ok(())
    }

    /// Add a context item to the session history.
    pub fn add_context(&mut self, item: ContextItem) {
        self.context.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_lifecycle() {
        let config = LoadedConfig::empty();
        let mut session = Session::new(config);
        assert_eq!(session.state, SessionState::Initializing);

        session.transition_to(SessionState::Idle).unwrap();
        assert_eq!(session.state, SessionState::Idle);

        let turn_id = session.admit_turn("hello".into()).unwrap();
        assert_eq!(session.state, SessionState::Running);
        assert!(session.current_turn.is_some());
        assert_eq!(session.current_turn.as_ref().unwrap().id, turn_id);

        session.complete_turn("hi there").unwrap();
        assert_eq!(session.state, SessionState::Idle);
        assert!(session.current_turn.is_none());
        assert_eq!(session.context.len(), 2);
    }

    #[test]
    fn reject_double_turn() {
        let config = LoadedConfig::empty();
        let mut session = Session::new(config);
        session.transition_to(SessionState::Idle).unwrap();
        session.admit_turn("first".into()).unwrap();
        assert!(session.admit_turn("second".into()).is_err());
    }

    #[test]
    fn invalid_transition_rejected() {
        let config = LoadedConfig::empty();
        let mut session = Session::new(config);
        // Cannot go from Initializing directly to Closed.
        assert!(session.transition_to(SessionState::Closed).is_err());
    }
}
