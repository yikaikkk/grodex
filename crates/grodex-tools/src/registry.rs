//! ToolRegistry — collects all built-in tools into a single registry.

use crate::edit::EditTool;
use crate::exec::ExecTool;
use crate::patch::ApplyPatchTool;
use crate::process_io::{ProcessIoTool, ProcessManager};
use crate::read::ReadFileTool;
use crate::write::WriteFileTool;
use grodex_core::tool::Tool;
use grodex_core::tool::ToolMetadata;
use std::collections::HashMap;

/// A registry of available tools, keyed by tool name.
pub struct ToolRegistry {
    tools: HashMap<String, ToolMetadata>,
    /// Shared process manager for Exec + ProcessIo coordination.
    pub process_manager: ProcessManager,
}

impl ToolRegistry {
    /// Create a new registry with all built-in tools.
    pub fn builtin() -> Self {
        let process_manager = ProcessManager::new();
        let mut registry = Self {
            tools: HashMap::new(),
            process_manager: process_manager.clone(),
        };

        registry.register(ReadFileTool::new().metadata());
        registry.register(WriteFileTool::new().metadata());
        registry.register(EditTool::new().metadata());
        registry.register(ExecTool::new().metadata());
        registry.register(ApplyPatchTool::new().metadata());
        registry.register(ProcessIoTool::new(process_manager).metadata());

        registry
    }

    /// Register a tool's metadata.
    pub fn register(&mut self, metadata: ToolMetadata) {
        self.tools.insert(metadata.name.clone(), metadata);
    }

    /// Get metadata for a tool by name.
    pub fn get(&self, name: &str) -> Option<&ToolMetadata> {
        self.tools.get(name)
    }

    /// List all registered tool names (sorted, deterministic).
    pub fn tool_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.tools.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::builtin()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_has_all_tools() {
        let registry = ToolRegistry::builtin();
        assert_eq!(registry.len(), 6);
        assert!(registry.get("read_file").is_some());
        assert!(registry.get("write_file").is_some());
        assert!(registry.get("edit_file").is_some());
        assert!(registry.get("exec").is_some());
        assert!(registry.get("apply_patch").is_some(), "apply_patch must be in the builtin registry");
        assert!(registry.get("process_io").is_some(), "process_io must be in the builtin registry");
    }
}
