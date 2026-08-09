//! ContextFork — controls what context the child agent inherits from the parent.

use grodex_core::context::ContextItem;
use serde::{Deserialize, Serialize};

/// How much context a child agent receives from its parent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContextFork {
    /// No context: child starts with an empty conversation (except system prompt).
    None,
    /// Only the specific items given.
    Selection(Vec<ContextItem>),
    /// Child inherits the parent's entire visible context projection.
    Full(Vec<ContextItem>),
}

impl ContextFork {
    /// Extract the actual context items, if any.
    pub fn into_items(self) -> Vec<ContextItem> {
        match self {
            Self::None => Vec::new(),
            Self::Selection(items) => items,
            Self::Full(items) => items,
        }
    }

    /// Whether the child receives any context from the parent.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::None => true,
            Self::Selection(items) => items.is_empty(),
            Self::Full(items) => items.is_empty(),
        }
    }
}
