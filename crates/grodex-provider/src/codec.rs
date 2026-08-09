//! ContextItem to wire format mapping.
//!
//! When mapping a Grodex ContextItem to a specific wire protocol, the
//! result is Supported (lossless), Lossy (some info dropped), or
//! Unsupported (cannot be expressed in this wire format).

use crate::canonical_request::{InstructionBlock, ToolSpec};
use grodex_core::context::ContextItem;

/// Outcome of mapping a ContextItem to a wire format.
#[derive(Debug, Clone)]
pub enum MappingResult {
    /// Item was mapped losslessly.
    Supported,
    /// Item was mapped but some information was dropped (e.g. reasoning
    /// summary truncated, image resolution reduced).
    Lossy { reason: String },
    /// Item cannot be expressed in this wire format (e.g. ImagePlaceholder
    /// in a text-only model).
    Unsupported { reason: String },
}

impl MappingResult {
    /// Returns true if the mapping succeeded (possibly with loss).
    pub fn is_mapped(&self) -> bool {
        matches!(self, Self::Supported | Self::Lossy { .. })
    }

    /// Returns true if the mapping lost information.
    pub fn is_lossy(&self) -> bool {
        matches!(self, Self::Lossy { .. })
    }
}

/// Trait for mapping Grodex types to wire-specific formats.
///
/// Each wire backend (Responses, Chat Completions, Messages) implements
/// this trait to define how the canonical types are serialized.
/// Phase 1 only needs a minimal implementation for Responses;
/// full implementations come in Phase 2.
pub trait ContextItemMapper {
    /// Map a single ContextItem to this wire format.
    fn map_context_item(&self, item: &ContextItem) -> MappingResult;

    /// Map an instruction block to this wire format.
    fn map_instruction(&self, instruction: &InstructionBlock) -> MappingResult;

    /// Map a tool specification to this wire format.
    fn map_tool_spec(&self, tool: &ToolSpec) -> MappingResult;
}

/// Default mapper that marks everything as unsupported.
/// Used as a base or when no real mapping is available.
pub struct NoopMapper;

impl ContextItemMapper for NoopMapper {
    fn map_context_item(&self, _item: &ContextItem) -> MappingResult {
        MappingResult::Unsupported {
            reason: "no mapper configured".into(),
        }
    }

    fn map_instruction(&self, _instruction: &InstructionBlock) -> MappingResult {
        MappingResult::Unsupported {
            reason: "no mapper configured".into(),
        }
    }

    fn map_tool_spec(&self, _tool: &ToolSpec) -> MappingResult {
        MappingResult::Unsupported {
            reason: "no mapper configured".into(),
        }
    }
}
