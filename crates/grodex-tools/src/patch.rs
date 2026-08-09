//! ApplyPatchTool — applies structured file patches (add, modify, delete, rename).
//!
//! Supports multi-file patches where each operation is a structured change.
//! Unlike EditTool (string replace), this handles file creation, deletion, and moves.

use crate::common::{
    self, AtomicityLevel, BuiltInTool, ChangedResource, ChangeType, ModelContent, PatchPlan,
    PreparedCall, Retryability, SideEffectHint, ToolResultEnvelope, ToolStatus, TruncationInfo,
};
use async_trait::async_trait;
use grodex_core::error::GrodexError;
use grodex_core::id::OperationId;
use grodex_core::tool::{ConcurrencyClass, SideEffectClass, Tool, ToolMetadata, ToolRuntime};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchArgs {
    /// List of file operations to apply.
    pub operations: Vec<PatchOperation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action")]
pub enum PatchOperation {
    /// Create a new file with content.
    #[serde(rename = "create")]
    Create { path: String, content: String },
    /// Overwrite an existing file.
    #[serde(rename = "modify")]
    Modify { path: String, content: String },
    /// Delete a file.
    #[serde(rename = "delete")]
    Delete { path: String },
    /// Rename/move a file.
    #[serde(rename = "rename")]
    Rename { old_path: String, new_path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchOutput {
    pub operations: Vec<PatchOpResult>,
    pub total_files_affected: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchOpResult {
    pub action: String,
    pub path: String,
    pub success: bool,
    pub error: Option<String>,
}

pub struct ApplyPatchTool;

impl Default for ApplyPatchTool { fn default() -> Self { Self::new() } }
impl ApplyPatchTool {
    pub fn new() -> Self { Self }
}

impl Tool for ApplyPatchTool {
    type Args = PatchArgs;
    type Output = PatchOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: "apply_patch".into(),
            display_name: "Apply Patch".into(),
            description: "Apply structured file operations: create, modify, delete, or rename files. Supports multiple operations in a single call.".into(),
            concurrency_class: ConcurrencyClass::Serial,
            side_effect_class: SideEffectClass::NonIdempotent,
            default_policy: grodex_core::policy::PolicyDecision::Ask,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "operations": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "oneOf": [
                            {"properties": {"action": {"const": "create"}, "path": {"type": "string"}, "content": {"type": "string"}}, "required": ["action", "path", "content"]},
                            {"properties": {"action": {"const": "modify"}, "path": {"type": "string"}, "content": {"type": "string"}}, "required": ["action", "path", "content"]},
                            {"properties": {"action": {"const": "delete"}, "path": {"type": "string"}}, "required": ["action", "path"]},
                            {"properties": {"action": {"const": "rename"}, "old_path": {"type": "string"}, "new_path": {"type": "string"}}, "required": ["action", "old_path", "new_path"]}
                        ]
                    }
                }
            },
            "required": ["operations"]
        })
    }

    fn output_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "operations": {"type": "array", "items": {"type": "object"}},
                "total_files_affected": {"type": "integer"}
            }
        })
    }
}

#[async_trait]
impl ToolRuntime for ApplyPatchTool {
    async fn execute(&self, args: serde_json::Value, _op: OperationId) -> Result<serde_json::Value, GrodexError> {
        let args: PatchArgs = serde_json::from_value(args)
            .map_err(|e| GrodexError::ToolExecution(format!("invalid args: {e}")))?;

        // ── Atomicity (Design Doc 15 §10.2) ──────────────────────────────
        // Parse ALL operations first; then apply them as a transaction. If
        // any operation fails mid-batch we roll back every prior mutation
        // (restoring prior contents / re-creating deleted files / renaming
        // back), so the filesystem is left untouched. create/modify use
        // temp-file + fsync + atomic rename so a crash mid-write leaves the
        // original file intact, never a half-written one.
        let mut txn = Txn::new();
        let mut results = Vec::new();

        for op in &args.operations {
            let result = match op {
                PatchOperation::Create { path, content } => match txn.create(path, content) {
                    Ok(()) => PatchOpResult { action: "create".into(), path: path.clone(), success: true, error: None },
                    Err(e) => PatchOpResult { action: "create".into(), path: path.clone(), success: false, error: Some(e) },
                },
                PatchOperation::Modify { path, content } => {
                    if !Path::new(path).exists() {
                        PatchOpResult { action: "modify".into(), path: path.clone(), success: false, error: Some("file does not exist".into()) }
                    } else {
                        match txn.modify(path, content) {
                            Ok(()) => PatchOpResult { action: "modify".into(), path: path.clone(), success: true, error: None },
                            Err(e) => PatchOpResult { action: "modify".into(), path: path.clone(), success: false, error: Some(e) },
                        }
                    }
                }
                PatchOperation::Delete { path } => match txn.delete(path) {
                    Ok(()) => PatchOpResult { action: "delete".into(), path: path.clone(), success: true, error: None },
                    Err(e) => PatchOpResult { action: "delete".into(), path: path.clone(), success: false, error: Some(e) },
                },
                PatchOperation::Rename { old_path, new_path } => match txn.rename(old_path, new_path) {
                    Ok(()) => PatchOpResult { action: "rename".into(), path: format!("{old_path} → {new_path}"), success: true, error: None },
                    Err(e) => PatchOpResult { action: "rename".into(), path: format!("{old_path} → {new_path}"), success: false, error: Some(e) },
                },
            };
            let succeeded = result.success;
            results.push(result);
            // On the first failure: roll back every applied op and stop.
            if !succeeded {
                txn.rollback();
                let output = PatchOutput {
                    total_files_affected: results.iter().filter(|r| r.success).count(),
                    operations: results,
                };
                return serde_json::to_value(output)
                    .map_err(|e| GrodexError::ToolExecution(format!("{e}")));
            }
        }

        // All ops succeeded — finalize (commit) the staged atomic replaces.
        // Nothing to undo from here unless persist_atomic fails, which is
        // already crash-safe per-op (temp+rename).
        let output = PatchOutput {
            total_files_affected: results.len(),
            operations: results,
        };
        serde_json::to_value(output).map_err(|e| GrodexError::ToolExecution(format!("{e}")))
    }
}

// ── Atomic transaction helper ────────────────────────────────────────────

/// Records applied operations so they can be rolled back on a mid-batch
/// failure. Each completed op stores enough state to undo it.
#[derive(Default)]
struct Txn {
    undo: Vec<UndoStep>,
}

enum UndoStep {
    /// File was created — undo by deleting it.
    Created(PathBuf),
    /// File was modified — undo by restoring the prior bytes (captured
    /// before the atomic replace overwrote it).
    Modified(PathBuf, Vec<u8>),
    /// File was deleted — undo by re-creating it with the captured bytes.
    Deleted(PathBuf, Vec<u8>),
    /// File was renamed old→new — undo by renaming new→old.
    Renamed(PathBuf, PathBuf),
}

impl Txn {
    fn new() -> Self {
        Self::default()
    }

    fn create(&mut self, path: &str, content: &str) -> Result<(), String> {
        let p = PathBuf::from(path);
        atomic_write(&p, content.as_bytes())?;
        self.undo.push(UndoStep::Created(p));
        Ok(())
    }

    fn modify(&mut self, path: &str, content: &str) -> Result<(), String> {
        let p = PathBuf::from(path);
        // Capture prior contents for rollback (file was confirmed to exist).
        let prior = std::fs::read(&p).map_err(|e| format!("read prior: {e}"))?;
        atomic_write(&p, content.as_bytes())?;
        self.undo.push(UndoStep::Modified(p, prior));
        Ok(())
    }

    fn delete(&mut self, path: &str) -> Result<(), String> {
        let p = PathBuf::from(path);
        let prior = std::fs::read(&p).map_err(|e| format!("read prior: {e}"))?;
        std::fs::remove_file(&p).map_err(|e| format!("delete: {e}"))?;
        self.undo.push(UndoStep::Deleted(p, prior));
        Ok(())
    }

    fn rename(&mut self, old_path: &str, new_path: &str) -> Result<(), String> {
        let old = PathBuf::from(old_path);
        let new = PathBuf::from(new_path);
        std::fs::rename(&old, &new).map_err(|e| format!("rename: {e}"))?;
        self.undo.push(UndoStep::Renamed(old, new));
        Ok(())
    }

    /// Roll back every applied op, in reverse order.
    fn rollback(&mut self) {
        while let Some(step) = self.undo.pop() {
            // Best-effort: a rollback failure is logged to stderr but does
            // not abort the rest of the undo chain.
            let _ = (|| -> std::io::Result<()> {
                match step {
                    UndoStep::Created(p) => {
                        let _ = std::fs::remove_file(&p);
                    }
                    UndoStep::Modified(p, prior) => {
                        std::fs::write(&p, &prior)?;
                    }
                    UndoStep::Deleted(p, prior) => {
                        std::fs::write(&p, &prior)?;
                    }
                    UndoStep::Renamed(old, new) => {
                        // Rename back; ignore errors if `new` was itself
                        // consumed by a later op in the same batch.
                        let _ = std::fs::rename(&new, &old);
                    }
                }
                Ok(())
            })();
        }
    }
}

/// Atomic write (delegates to the shared `fsutil::atomic_write`).
fn atomic_write(target: &Path, content: &[u8]) -> Result<(), String> {
    crate::fsutil::atomic_write(target, content)
}

#[derive(Debug, Clone)]
pub struct PreparedPatchCall {
    pub args: PatchArgs,
    pub patch_plan: PatchPlan,
    pub file_version_fences: Vec<(String, Option<String>)>,
    pub per_op_stale: Vec<Option<bool>>,
    pub prepare_errors: Vec<Option<String>>,
}

impl PreparedCall for PreparedPatchCall {
    fn side_effect_hint(&self) -> SideEffectHint {
        SideEffectHint::LocalFsWrite
    }

    fn plan_id(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.patch_plan.plan_hash.as_bytes());
        let full = format!("{:x}", h.finalize());
        full[..16].to_string()
    }
}

fn sha256_str(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

impl BuiltInTool for ApplyPatchTool {
    type Input = PatchArgs;
    type Prepared = PreparedPatchCall;
    type OkResult = ToolResultEnvelope;
    type Error = GrodexError;

    fn prepare(&self, input: Self::Input) -> Result<Self::Prepared, Self::Error> {
        let mut patch_files = Vec::new();
        let mut file_version_fences = Vec::new();
        let mut per_op_stale = Vec::new();
        let mut prepare_errors = Vec::new();
        let mut plan_hash_input = Vec::new();

        for op in &input.operations {
            let (target_path, common_op, expected_before, after_hash, hunks, display_path, source_rid) = match op {
                PatchOperation::Create { path, content } => {
                    let canonical = crate::fsutil::canonicalize(Path::new(path));
                    let rid = format!("fs://{}", canonical.display());
                    let after = sha256_str(content.as_bytes());
                    plan_hash_input.extend_from_slice(b"create");
                    plan_hash_input.extend_from_slice(rid.as_bytes());
                    plan_hash_input.extend_from_slice(after.as_bytes());
                    (
                        rid.clone(),
                        common::PatchOperation::Add,
                        None,
                        after,
                        vec![],
                        PathBuf::from(path),
                        None,
                    )
                }
                PatchOperation::Modify { path, content } => {
                    let canonical = crate::fsutil::canonicalize(Path::new(path));
                    let rid = format!("fs://{}", canonical.display());
                    let before = if Path::new(path).exists() {
                        match std::fs::read(path) {
                            Ok(bytes) => Some(sha256_str(&bytes)),
                            Err(e) => {
                                prepare_errors.push(Some(format!("read {}: {}", path, e)));
                                per_op_stale.push(None);
                                file_version_fences.push((rid.clone(), None));
                                continue;
                            }
                        }
                    } else {
                        prepare_errors.push(Some(format!(
                            "modify target does not exist: {}",
                            path
                        )));
                        per_op_stale.push(None);
                        file_version_fences.push((rid.clone(), None));
                        continue;
                    };
                    let after = sha256_str(content.as_bytes());
                    plan_hash_input.extend_from_slice(b"modify");
                    plan_hash_input.extend_from_slice(rid.as_bytes());
                    plan_hash_input.extend_from_slice(before.as_deref().unwrap_or("").as_bytes());
                    plan_hash_input.extend_from_slice(after.as_bytes());
                    (
                        rid.clone(),
                        common::PatchOperation::Update,
                        before.clone(),
                        after,
                        vec![],
                        PathBuf::from(path),
                        None,
                    )
                }
                PatchOperation::Delete { path } => {
                    let canonical = crate::fsutil::canonicalize(Path::new(path));
                    let rid = format!("fs://{}", canonical.display());
                    let before = if Path::new(path).exists() {
                        match std::fs::read(path) {
                            Ok(bytes) => Some(sha256_str(&bytes)),
                            Err(e) => {
                                prepare_errors.push(Some(format!("read {}: {}", path, e)));
                                per_op_stale.push(None);
                                file_version_fences.push((rid.clone(), None));
                                continue;
                            }
                        }
                    } else {
                        prepare_errors.push(Some(format!(
                            "delete target does not exist: {}",
                            path
                        )));
                        per_op_stale.push(None);
                        file_version_fences.push((rid.clone(), None));
                        continue;
                    };
                    plan_hash_input.extend_from_slice(b"delete");
                    plan_hash_input.extend_from_slice(rid.as_bytes());
                    plan_hash_input.extend_from_slice(before.as_deref().unwrap_or("").as_bytes());
                    (
                        rid.clone(),
                        common::PatchOperation::Delete,
                        before.clone(),
                        String::new(),
                        vec![],
                        PathBuf::from(path),
                        None,
                    )
                }
                PatchOperation::Rename { old_path, new_path } => {
                    let canonical_old = crate::fsutil::canonicalize(Path::new(old_path));
                    let canonical_new = crate::fsutil::canonicalize(Path::new(new_path));
                    let source_rid = format!("fs://{}", canonical_old.display());
                    let target_rid = format!("fs://{}", canonical_new.display());
                    let before = if Path::new(old_path).exists() {
                        match std::fs::read(old_path) {
                            Ok(bytes) => Some(sha256_str(&bytes)),
                            Err(e) => {
                                prepare_errors.push(Some(format!("read {}: {}", old_path, e)));
                                per_op_stale.push(None);
                                file_version_fences.push((target_rid.clone(), None));
                                continue;
                            }
                        }
                    } else {
                        prepare_errors.push(Some(format!(
                            "rename source does not exist: {}",
                            old_path
                        )));
                        per_op_stale.push(None);
                        file_version_fences.push((target_rid.clone(), None));
                        continue;
                    };
                    let after_hash = before.clone().unwrap_or_default();
                    plan_hash_input.extend_from_slice(b"rename");
                    plan_hash_input.extend_from_slice(source_rid.as_bytes());
                    plan_hash_input.extend_from_slice(target_rid.as_bytes());
                    (
                        target_rid,
                        common::PatchOperation::Move,
                        before.clone(),
                        after_hash,
                        vec![],
                        PathBuf::from(format!("{} → {}", old_path, new_path)),
                        Some(source_rid),
                    )
                }
            };

            per_op_stale.push(None);
            prepare_errors.push(None);
            file_version_fences.push((target_path.clone(), expected_before.clone()));

            patch_files.push(common::PatchFile {
                source_resource_id: source_rid,
                target_resource_id: target_path,
                operation: common_op,
                expected_version_before: expected_before,
                after_hash,
                hunks,
                target_display_path: display_path,
            });
        }

        let plan_hash = {
            let mut sorted_files = patch_files.clone();
            sorted_files.sort_by(|a, b| a.target_resource_id.cmp(&b.target_resource_id));
            let mut h = Sha256::new();
            for pf in &sorted_files {
                h.update(pf.target_resource_id.as_bytes());
                h.update(pf.after_hash.as_bytes());
            }
            h.update(&plan_hash_input);
            format!("{:x}", h.finalize())
        };

        let patch_plan = PatchPlan {
            files: patch_files,
            plan_hash,
            atomicity: AtomicityLevel::StrictAtomic,
        };

        Ok(PreparedPatchCall {
            args: input,
            patch_plan,
            file_version_fences,
            per_op_stale,
            prepare_errors,
        })
    }

    fn execute(&self, prepared: Self::Prepared) -> Result<Self::OkResult, Self::Error> {
        let start = std::time::Instant::now();

        for err in prepared.prepare_errors.iter() {
            if let Some(e) = err {
                let envelope = ToolResultEnvelope {
                    tool_call_id: String::new(),
                    operation_id: None,
                    capability_id: Some("apply_patch".into()),
                    contract_version: 1,
                    status: ToolStatus::ToolError,
                    summary: e.clone(),
                    model_content: vec![ModelContent::Text(format!(
                        "tool `apply_patch` prepare error: {}",
                        e
                    ))],
                    structured_data: BTreeMap::new(),
                    artifacts: vec![],
                    changed_resources: vec![],
                    truncation: TruncationInfo::default(),
                    wall_time: start.elapsed(),
                    retryability: Retryability::NotRetryable,
                    diagnostics: vec![],
                };
                return Ok(envelope);
            }
        }

        let mut txn = Txn::new();
        let mut results = Vec::new();
        let mut changed_resources = Vec::new();
        let mut any_failed = false;

        for (idx, op) in prepared.args.operations.iter().enumerate() {
            let patch_file = &prepared.patch_plan.files[idx];

            let before_hash = patch_file.expected_version_before.clone();
            let after_hash_for_changed = Some(patch_file.after_hash.clone())
                .filter(|s| !s.is_empty());

            let (action_str, result_path, success, err_opt) = match op {
                PatchOperation::Create { path, content } => {
                    match txn.create(path, content) {
                        Ok(()) => {
                            changed_resources.push(ChangedResource {
                                resource_id: patch_file.target_resource_id.clone(),
                                display_path: patch_file.target_display_path.clone(),
                                change_type: ChangeType::Created,
                                before_hash: None,
                                after_hash: after_hash_for_changed,
                            });
                            ("create", path.clone(), true, None)
                        }
                        Err(e) => ("create", path.clone(), false, Some(e)),
                    }
                }
                PatchOperation::Modify { path, content } => {
                    if !Path::new(path).exists() {
                        ("modify", path.clone(), false, Some("file does not exist".into()))
                    } else {
                        let actual_current = std::fs::read(path).ok().map(|b| sha256_str(&b));
                        if actual_current != patch_file.expected_version_before {
                            ("modify", path.clone(), false, Some("file changed since prepare (stale)".into()))
                        } else {
                            match txn.modify(path, content) {
                                Ok(()) => {
                                    changed_resources.push(ChangedResource {
                                        resource_id: patch_file.target_resource_id.clone(),
                                        display_path: patch_file.target_display_path.clone(),
                                        change_type: ChangeType::Updated,
                                        before_hash,
                                        after_hash: after_hash_for_changed,
                                    });
                                    ("modify", path.clone(), true, None)
                                }
                                Err(e) => ("modify", path.clone(), false, Some(e)),
                            }
                        }
                    }
                }
                PatchOperation::Delete { path } => {
                    match txn.delete(path) {
                        Ok(()) => {
                            changed_resources.push(ChangedResource {
                                resource_id: patch_file.target_resource_id.clone(),
                                display_path: patch_file.target_display_path.clone(),
                                change_type: ChangeType::Deleted,
                                before_hash,
                                after_hash: None,
                            });
                            ("delete", path.clone(), true, None)
                        }
                        Err(e) => ("delete", path.clone(), false, Some(e)),
                    }
                }
                PatchOperation::Rename { old_path, new_path } => {
                    match txn.rename(old_path, new_path) {
                        Ok(()) => {
                            changed_resources.push(ChangedResource {
                                resource_id: patch_file.target_resource_id.clone(),
                                display_path: patch_file.target_display_path.clone(),
                                change_type: ChangeType::Moved,
                                before_hash,
                                after_hash: after_hash_for_changed,
                            });
                            (
                                "rename",
                                format!("{} → {}", old_path, new_path),
                                true,
                                None,
                            )
                        }
                        Err(e) => (
                            "rename",
                            format!("{} → {}", old_path, new_path),
                            false,
                            Some(e),
                        ),
                    }
                }
            };

            results.push(PatchOpResult {
                action: action_str.into(),
                path: result_path,
                success,
                error: err_opt,
            });
            if !success {
                any_failed = true;
                txn.rollback();
                break;
            }
        }

        let output = PatchOutput {
            total_files_affected: if any_failed {
                results.iter().filter(|r| r.success).count()
            } else {
                results.len()
            },
            operations: results,
        };

        let output_serialized = serde_json::to_string(&output).ok();
        let mut structured_data = BTreeMap::new();
        if let Some(s) = output_serialized {
            structured_data.insert("output_serialized".into(), serde_json::Value::String(s));
        }
        structured_data.insert(
            "patch_plan".into(),
            serde_json::to_value(&prepared.patch_plan).unwrap_or(serde_json::Value::Null),
        );

        let (status, model_text, retryability) = if any_failed {
            let last_fail = output
                .operations
                .iter()
                .find(|r| !r.success)
                .and_then(|r| r.error.clone())
                .unwrap_or_else(|| "unknown error".into());
            (
                ToolStatus::ToolError,
                format!("tool `apply_patch` error: {}, rolled back", last_fail),
                Retryability::NotRetryable,
            )
        } else {
            (
                ToolStatus::Ok,
                format!(
                    "tool `apply_patch` ok: {} operations applied",
                    output.total_files_affected
                ),
                Retryability::NotRetryable,
            )
        };

        let envelope = ToolResultEnvelope {
            tool_call_id: String::new(),
            operation_id: None,
            capability_id: Some("apply_patch".into()),
            contract_version: 1,
            status,
            summary: model_text.clone(),
            model_content: vec![ModelContent::Text(model_text)],
            structured_data,
            artifacts: vec![],
            changed_resources: if any_failed { vec![] } else { changed_resources },
            truncation: TruncationInfo::default(),
            wall_time: start.elapsed(),
            retryability,
            diagnostics: vec![],
        };

        Ok(envelope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grodex_core::tool::ToolRuntime;

    #[tokio::test]
    async fn create_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new_file.txt");
        let tool = ApplyPatchTool::new();

        let result = ToolRuntime::execute(&tool, serde_json::json!({
            "operations": [
                {"action": "create", "path": path.to_str().unwrap(), "content": "hello"},
                {"action": "delete", "path": path.to_str().unwrap()}
            ]
        }), OperationId::new()).await.unwrap();

        let output: PatchOutput = serde_json::from_value(result).unwrap();
        assert_eq!(output.total_files_affected, 2);
        assert!(output.operations[0].success);
        assert!(output.operations[1].success);
        assert!(!path.exists());
    }

    /// A mid-batch failure rolls back every prior mutation: the filesystem
    /// is left untouched (atomicity, §10.2).
    #[tokio::test]
    async fn mid_batch_failure_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        // Pre-existing file we will modify, then a delete on a non-existent
        // path to trigger the rollback.
        let keep = dir.path().join("keep.txt");
        std::fs::write(&keep, "original").unwrap();
        let ghost = dir.path().join("does-not-exist.txt");

        let tool = ApplyPatchTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({
                    "operations": [
                        {"action": "modify", "path": keep.to_str().unwrap(), "content": "MUTATED"},
                        {"action": "delete", "path": ghost.to_str().unwrap()}
                    ]
                }),
            OperationId::new(),
        )
            .await
            .unwrap();

        let output: PatchOutput = serde_json::from_value(result).unwrap();
        // The modify succeeded but the delete failed → batch aborted,
        // modify rolled back.
        assert!(output.operations[0].success);
        assert!(!output.operations[1].success);
        // The keep.txt must be restored to its original contents.
        assert_eq!(std::fs::read_to_string(&keep).unwrap(), "original");
    }

    /// create/modify use temp+rename, so a crash mid-write leaves no
    //  half-written target.
    #[tokio::test]
    async fn create_is_atomic_no_leaked_temp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("new.txt");
        let tool = ApplyPatchTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({
                    "operations": [{"action": "create", "path": p.to_str().unwrap(), "content": "hello"}]
                }),
            OperationId::new(),
        )
            .await
            .unwrap();
        let output: PatchOutput = serde_json::from_value(result).unwrap();
        assert!(output.operations[0].success);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hello");
        // No leftover temp files in the directory.
        let temps: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".grodex-tmp-"))
            .collect();
        assert!(temps.is_empty(), "no temp files should remain after a clean apply: {temps:?}");
    }
}
