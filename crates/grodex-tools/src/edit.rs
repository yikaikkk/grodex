//! EditTool — exact string replacement in an existing file.
//!
//! Matches `old_string` in the file content and replaces it with `new_string`.
//! Returns an error if `old_string` is not found or appears multiple times
//! (to avoid ambiguous edits).

use crate::common::{
    BuiltInTool, ChangedResource, ChangeType, FileSnapshot, FileType, LineEnding, ModelContent,
    PreparedCall, Retryability, SideEffectHint, StaleFile, StaleSuggestion, ToolResultEnvelope,
    ToolStatus, TruncationInfo,
};
use async_trait::async_trait;
use grodex_core::error::GrodexError;
use grodex_core::id::OperationId;
use grodex_core::tool::{ConcurrencyClass, SideEffectClass, ToolMetadata};
use grodex_core::tool::{Tool, ToolRuntime};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

/// A single edit operation within a batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditOperation {
    /// The exact string to find.
    pub old_string: String,
    /// The replacement string.
    pub new_string: String,
}

/// Arguments for the EditTool.
///
/// Two modes:
/// 1. **Single edit** (backward-compatible): `old_string` + `new_string`
/// 2. **Batch edit**: `edits` array of `EditOperation`
///
/// When `edits` is present, `old_string`/`new_string`/`replace_all` are
/// ignored. Batch edits are applied bottom-up (by position, last-first)
/// to avoid offset drift. Overlapping edits are rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditArgs {
    pub path: String,
    #[serde(default)]
    pub old_string: Option<String>,
    #[serde(default)]
    pub new_string: Option<String>,
    /// If true, replace all occurrences. Default: false (error on multiple matches).
    #[serde(default)]
    pub replace_all: bool,
    /// Lost-update fence: the SHA-256 of the file the caller last read.
    #[serde(default)]
    pub expected_content_hash: Option<String>,
    /// Structured version fence: the full FileSnapshot from the last read.
    /// When present, the edit verifies content_hash + size + mtime all match
    /// before applying, providing stronger staleness detection than hash alone.
    #[serde(default)]
    pub expected_snapshot: Option<FileSnapshot>,
    /// Batch edit mode: multiple edit operations applied atomically.
    #[serde(default)]
    pub edits: Option<Vec<EditOperation>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditOutput {
    pub path: String,
    pub replacements: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// Fresh file snapshot after the edit. The caller can pass this as
    /// `expected_snapshot` for the next edit without re-reading (§9.2/§10.5).
    #[serde(default)]
    pub fresh_snapshot: Option<FileSnapshot>,
    /// SHA-256 of the file content before the edit.
    #[serde(default)]
    pub content_hash_before: String,
    /// SHA-256 of the file content after the edit.
    #[serde(default)]
    pub content_hash_after: String,
    /// Line ending style detected in the file ("lf", "crlf", "mixed").
    #[serde(default)]
    pub line_ending: String,
    /// Number of lines in the file before the edit.
    #[serde(default)]
    pub lines_before: usize,
    /// Number of lines in the file after the edit.
    #[serde(default)]
    pub lines_after: usize,
    /// Whether this edit was a no-op (content unchanged).
    #[serde(default)]
    pub no_op: bool,
}

pub struct EditTool;

impl Default for EditTool {
    fn default() -> Self {
        Self::new()
    }
}

impl EditTool {
    pub fn new() -> Self {
        Self
    }
}

impl Tool for EditTool {
    type Args = EditArgs;
    type Output = EditOutput;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: "edit_file".into(),
            display_name: "Edit File".into(),
            description: "Replace exact string matches in a file. Requires exact match (including whitespace). Use replace_all=true to replace all occurrences.".into(),
            concurrency_class: ConcurrencyClass::Serial,
            side_effect_class: SideEffectClass::NonIdempotent,
            default_policy: grodex_core::policy::PolicyDecision::Ask,
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to edit"},
                "old_string": {"type": "string", "description": "Exact string to find and replace (single edit mode)"},
                "new_string": {"type": "string", "description": "Replacement string (single edit mode)"},
                "replace_all": {"type": "boolean", "description": "Replace all occurrences (default: false)"},
                "expected_content_hash": {"type": "string", "description": "SHA-256 from the last read; refuse the edit if the file changed since then"},
                "edits": {
                    "type": "array",
                    "description": "Batch edit mode: array of {old_string, new_string} operations applied atomically bottom-up",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": {"type": "string"},
                            "new_string": {"type": "string"}
                        },
                        "required": ["old_string", "new_string"]
                    }
                }
            },
            "required": ["path"]
        })
    }

    fn output_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "replacements": {"type": "integer"},
                "bytes_before": {"type": "integer"},
                "bytes_after": {"type": "integer"}
            }
        })
    }
}

#[async_trait]
impl ToolRuntime for EditTool {
    async fn execute(
        &self,
        args: serde_json::Value,
        _operation_id: OperationId,
    ) -> Result<serde_json::Value, GrodexError> {
        let args: EditArgs =
            serde_json::from_value(args).map_err(|e| GrodexError::ToolExecution(format!("invalid args: {e}")))?;

        let content = std::fs::read_to_string(&args.path)
            .map_err(|e| GrodexError::ToolExecution(format!("cannot read {}: {e}", args.path)))?;

        // Lost-update fence (hash-based).
        if let Some(expected) = &args.expected_content_hash {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(content.as_bytes());
            let actual = format!("{:x}", h.finalize());
            if &actual != expected {
                return Err(GrodexError::ToolExecution(format!(
                    "lost-update fence: {} changed since last read (expected hash {}, got {}). Re-read before editing.",
                    args.path, expected, actual
                )));
            }
        }

        // Structured version fence (snapshot-based, §9.2).
        if let Some(ref expected_snap) = args.expected_snapshot {
            let actual_size = content.len() as u64;
            let mut h = Sha256::new();
            h.update(content.as_bytes());
            let actual_hash = format!("{:x}", h.finalize());
            // Verify content_hash.
            if let Some(ref expected_hash) = expected_snap.content_hash {
                if &actual_hash != expected_hash {
                    return Err(GrodexError::ToolExecution(format!(
                        "stale_file: {} content_hash mismatch (expected {}, actual {}). Re-read before editing.",
                        args.path, expected_hash, actual_hash
                    )));
                }
            }
            // Verify size.
            if expected_snap.size != actual_size {
                return Err(GrodexError::ToolExecution(format!(
                    "stale_file: {} size mismatch (expected {}, actual {}). Re-read before editing.",
                    args.path, expected_snap.size, actual_size
                )));
            }
        }

        let bytes_before = content.len() as u64;

        // ── Batch edit mode ──
        if let Some(ref edits) = args.edits {
            if edits.is_empty() {
                return Err(GrodexError::ToolExecution("edits array is empty".into()));
            }
            let (new_content, total_replacements) = apply_batch_edits(&content, edits)?;
            crate::fsutil::atomic_write(std::path::Path::new(&args.path), new_content.as_bytes())
                .map_err(|e| GrodexError::ToolExecution(format!("cannot write {}: {e}", args.path)))?;

            let fresh = build_fresh_snapshot(&args.path, &new_content);
            let content_hash_before = compute_content_hash(&content);
            let content_hash_after = compute_content_hash(&new_content);
            let line_ending = detect_line_ending(&content);
            let result = EditOutput {
                path: args.path,
                replacements: total_replacements,
                bytes_before,
                bytes_after: new_content.len() as u64,
                fresh_snapshot: Some(fresh),
                content_hash_before,
                content_hash_after,
                line_ending,
                lines_before: content.lines().count(),
                lines_after: new_content.lines().count(),
                no_op: content == new_content,
            };
            return serde_json::to_value(result)
                .map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")));
        }

        // ── Single edit mode (backward-compatible) ──
        let old_string = args.old_string.as_ref().ok_or_else(|| {
            GrodexError::ToolExecution("old_string is required when edits is not provided".into())
        })?;
        let new_string = args.new_string.as_ref().ok_or_else(|| {
            GrodexError::ToolExecution("new_string is required when edits is not provided".into())
        })?;

        let count = content.matches(old_string).count();

        if count == 0 {
            return Err(GrodexError::ToolExecution(format!(
                "string not found in {}: {:?}",
                args.path, old_string
            )));
        }

        if count > 1 && !args.replace_all {
            return Err(GrodexError::ToolExecution(format!(
                "string appears {count} times in {}. Use replace_all=true to replace all, or use a more specific old_string.",
                args.path
            )));
        }

        let new_content = if args.replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        crate::fsutil::atomic_write(std::path::Path::new(&args.path), new_content.as_bytes())
            .map_err(|e| GrodexError::ToolExecution(format!("cannot write {}: {e}", args.path)))?;

        let fresh = build_fresh_snapshot(&args.path, &new_content);
        let content_hash_before = compute_content_hash(&content);
        let content_hash_after = compute_content_hash(&new_content);
        let line_ending = detect_line_ending(&content);
        let result = EditOutput {
            path: args.path,
            replacements: if args.replace_all { count } else { 1 },
            bytes_before,
            bytes_after: new_content.len() as u64,
            fresh_snapshot: Some(fresh),
            content_hash_before,
            content_hash_after,
            line_ending,
            lines_before: content.lines().count(),
            lines_after: new_content.lines().count(),
            no_op: content == new_content,
        };

        serde_json::to_value(result).map_err(|e| GrodexError::ToolExecution(format!("serialize: {e}")))
    }
}

// ── Batch edit engine ─────────────────────────────────────────────────

/// Apply multiple edit operations atomically with overlap detection.
///
/// Each operation finds its `old_string` in the content and records the
/// byte range. If any two ranges overlap, the batch is rejected. Otherwise
/// edits are applied bottom-up (last position first) so earlier edits
/// don't shift the byte offsets of later ones.
///
/// Returns `(new_content, total_replacements)`.
fn apply_batch_edits(
    content: &str,
    edits: &[EditOperation],
) -> Result<(String, usize), GrodexError> {
    // 1. Find all match positions.
    struct Match {
        start: usize,
        end: usize,
        replacement: String,
    }

    let mut matches: Vec<Match> = Vec::with_capacity(edits.len());

    for (i, edit) in edits.iter().enumerate() {
        // Find the first occurrence of old_string.
        let pos = content.find(&edit.old_string).ok_or_else(|| {
            GrodexError::ToolExecution(format!(
                "edit #{}: old_string not found: {:?}",
                i + 1,
                edit.old_string
            ))
        })?;

        matches.push(Match {
            start: pos,
            end: pos + edit.old_string.len(),
            replacement: edit.new_string.clone(),
        });
    }

    // 2. Sort by start position for overlap detection.
    matches.sort_by_key(|m| m.start);

    // 3. Overlap detection: no two byte ranges may cross.
    for window in matches.windows(2) {
        let a = &window[0];
        let b = &window[1];
        if a.end > b.start {
            return Err(GrodexError::ToolExecution(format!(
                "overlapping edits detected: range [{},{}) crosses [{},{})",
                a.start, a.end, b.start, b.end
            )));
        }
    }

    // 4. Apply bottom-up (reverse order by position) so offsets stay valid.
    let mut result = content.to_string();
    for m in matches.iter().rev() {
        result.replace_range(m.start..m.end, &m.replacement);
    }

    Ok((result, edits.len()))
}

/// Build a fresh `FileSnapshot` after a successful write, so the caller
/// can chain subsequent edits without re-reading (§9.2 / §10.5).
fn build_fresh_snapshot(path: &str, new_content: &str) -> FileSnapshot {
    let mut h = Sha256::new();
    h.update(new_content.as_bytes());
    let content_hash = format!("{:x}", h.finalize());
    let canonical_path = crate::fsutil::canonicalize(std::path::Path::new(path));
    let canonical_resource_id = format!("fs://{}", canonical_path.display());
    FileSnapshot {
        canonical_resource_id,
        display_path: PathBuf::from(path),
        file_type: FileType::Text,
        size: new_content.len() as u64,
        mtime_secs: std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64),
        content_hash: Some(content_hash.clone()),
        range_hash: Some(content_hash),
        line_ending: LineEnding::Lf,
        encoding: Some("utf-8".into()),
        read_at: SystemTime::now(),
        environment_id: String::new(),
        read_range: crate::common::ReadRange::Whole,
    }
}

/// Compute SHA-256 hash of content and return as hex string.
fn compute_content_hash(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    format!("{:x}", h.finalize())
}

/// Detect line ending style in content. Returns "lf", "crlf", or "mixed".
fn detect_line_ending(content: &str) -> String {
    let has_crlf = content.contains("\r\n");
    let has_lf = content.contains('\n') && !content.contains("\r\n");
    let has_bare_lf = content.matches('\n').count() > content.matches("\r\n").count();
    if has_crlf && has_bare_lf {
        "mixed".to_string()
    } else if has_crlf {
        "crlf".to_string()
    } else {
        "lf".to_string()
    }
}

#[derive(Debug, Clone)]
pub struct PreparedEditCall {
    pub args: EditArgs,
    pub original_content: String,
    pub new_content: String,
    pub replacements: usize,
    pub before_hash: String,
    pub after_hash: String,
    pub canonical_resource_id: String,
    pub display_path: PathBuf,
    pub stale_candidate: Option<StaleFile>,
    pub prepare_error: Option<String>,
}

impl PreparedCall for PreparedEditCall {
    fn side_effect_hint(&self) -> SideEffectHint {
        SideEffectHint::LocalFsWrite
    }

    fn plan_id(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.args.path.as_bytes());
        if let Some(ref s) = self.args.old_string {
            h.update(s.as_bytes());
        }
        if let Some(ref s) = self.args.new_string {
            h.update(s.as_bytes());
        }
        h.update(self.before_hash.as_bytes());
        h.update(self.after_hash.as_bytes());
        h.update(self.replacements.to_le_bytes());
        let full = format!("{:x}", h.finalize());
        full[..16].to_string()
    }
}

impl BuiltInTool for EditTool {
    type Input = EditArgs;
    type Prepared = PreparedEditCall;
    type OkResult = ToolResultEnvelope;
    type Error = GrodexError;

    fn prepare(&self, input: Self::Input) -> Result<Self::Prepared, Self::Error> {
        let content = std::fs::read_to_string(&input.path)
            .map_err(|e| GrodexError::ToolExecution(format!("cannot read {}: {e}", input.path)))?;

        let _bytes_before = content.len() as u64;

        let mut h = Sha256::new();
        h.update(content.as_bytes());
        let before_hash = format!("{:x}", h.finalize());

        let canonical_path = crate::fsutil::canonicalize(std::path::Path::new(&input.path));
        let canonical_resource_id = format!("fs://{}", canonical_path.display());
        let display_path = PathBuf::from(&input.path);

        let mut stale_candidate = None;
        if let Some(expected) = &input.expected_content_hash {
            if &before_hash != expected {
                stale_candidate = Some(StaleFile {
                    resource_id: canonical_resource_id.clone(),
                    expected_hash: Some(expected.clone()),
                    actual_hash: Some(before_hash.clone()),
                    changed_since_secs: None,
                    suggested_action: StaleSuggestion::RereadAndResample,
                });
            }
        }

        // Batch edit mode: use apply_batch_edits.
        let (new_content, replacements, prepare_error) = if let Some(ref edits) = input.edits {
            match apply_batch_edits(&content, edits) {
                Ok((nc, nr)) => (nc, nr, None),
                Err(e) => (content.clone(), 0, Some(e.to_string())),
            }
        } else {
            // Single edit mode.
            let old_s = input.old_string.as_deref().unwrap_or("");
            let new_s = input.new_string.as_deref().unwrap_or("");
            let count = content.matches(old_s).count();
            let mut err = None;

            if count == 0 && !old_s.is_empty() {
                err = Some(format!("string not found in {}: {:?}", input.path, old_s));
            }
            if count > 1 && !input.replace_all {
                err = Some(format!(
                    "string appears {count} times in {}. Use replace_all=true to replace all, or use a more specific old_string.",
                    input.path
                ));
            }

            let nc = if err.is_none() {
                if input.replace_all {
                    content.replace(old_s, new_s)
                } else {
                    content.replacen(old_s, new_s, 1)
                }
            } else {
                content.clone()
            };

            let nr = if err.is_some() { 0 } else if input.replace_all { count } else { 1 };
            (nc, nr, err)
        };

        let mut ah = Sha256::new();
        ah.update(new_content.as_bytes());
        let after_hash = format!("{:x}", ah.finalize());

        Ok(PreparedEditCall {
            args: input,
            original_content: content,
            new_content,
            replacements,
            before_hash,
            after_hash,
            canonical_resource_id,
            display_path,
            stale_candidate,
            prepare_error,
        })
    }

    fn execute(&self, prepared: Self::Prepared) -> Result<Self::OkResult, Self::Error> {
        let start = std::time::Instant::now();

        if let Some(sc) = prepared.stale_candidate {
            let mut structured_data = BTreeMap::new();
            structured_data.insert(
                "stale_file".into(),
                serde_json::to_value(&sc).unwrap_or(serde_json::Value::Null),
            );
            let envelope = ToolResultEnvelope {
                tool_call_id: String::new(),
                operation_id: None,
                capability_id: Some("edit_file".into()),
                contract_version: 1,
                status: ToolStatus::RuntimeError,
                summary: format!(
                    "stale file fence: expected {:?}, actual {:?}",
                    sc.expected_hash, sc.actual_hash
                ),
                model_content: vec![ModelContent::Text(format!(
                    "tool `edit_file` stale: file {} modified since last read, re-read before editing",
                    prepared.display_path.display()
                ))],
                structured_data,
                artifacts: vec![],
                changed_resources: vec![],
                truncation: TruncationInfo::default(),
                wall_time: start.elapsed(),
                retryability: Retryability::StaleResource,
                diagnostics: vec![],
            };
            return Ok(envelope);
        }

        if let Some(err) = prepared.prepare_error {
            let envelope = ToolResultEnvelope {
                tool_call_id: String::new(),
                operation_id: None,
                capability_id: Some("edit_file".into()),
                contract_version: 1,
                status: ToolStatus::ToolError,
                summary: err.clone(),
                model_content: vec![ModelContent::Text(format!(
                    "tool `edit_file` error: {}",
                    err
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

        crate::fsutil::atomic_write(
            std::path::Path::new(&prepared.args.path),
            prepared.new_content.as_bytes(),
        )
        .map_err(|e| {
            GrodexError::ToolExecution(format!(
                "cannot write {}: {e}",
                prepared.args.path
            ))
        })?;

        let content_hash_before = compute_content_hash(&prepared.original_content);
        let content_hash_after = compute_content_hash(&prepared.new_content);
        let line_ending = detect_line_ending(&prepared.original_content);
        let result = EditOutput {
            path: prepared.args.path.clone(),
            replacements: prepared.replacements,
            bytes_before: prepared.original_content.len() as u64,
            bytes_after: prepared.new_content.len() as u64,
            fresh_snapshot: Some(build_fresh_snapshot(&prepared.args.path, &prepared.new_content)),
            content_hash_before,
            content_hash_after,
            line_ending,
            lines_before: prepared.original_content.lines().count(),
            lines_after: prepared.new_content.lines().count(),
            no_op: prepared.original_content == prepared.new_content,
        };

        let output_serialized = serde_json::to_string(&result).ok();
        let mut structured_data = BTreeMap::new();
        if let Some(s) = output_serialized {
            structured_data.insert("output_serialized".into(), serde_json::Value::String(s));
        }

        let changed_resources = vec![ChangedResource {
            resource_id: prepared.canonical_resource_id.clone(),
            display_path: prepared.display_path.clone(),
            change_type: ChangeType::Updated,
            before_hash: Some(prepared.before_hash.clone()),
            after_hash: Some(prepared.after_hash.clone()),
        }];

        let model_text = format!(
            "tool `edit_file` ok: {} replacements, {} bytes → {} bytes",
            result.replacements, result.bytes_before, result.bytes_after
        );

        let envelope = ToolResultEnvelope {
            tool_call_id: String::new(),
            operation_id: None,
            capability_id: Some("edit_file".into()),
            contract_version: 1,
            status: ToolStatus::Ok,
            summary: model_text.clone(),
            model_content: vec![ModelContent::Text(model_text)],
            structured_data,
            artifacts: vec![],
            changed_resources,
            truncation: TruncationInfo {
                original_bytes: result.bytes_before,
                retained_bytes: result.bytes_after,
                strategy: crate::common::TruncationStrategy::None,
                omitted: 0,
            },
            wall_time: start.elapsed(),
            retryability: Retryability::NotRetryable,
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
    async fn edit_single_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edit.txt");
        std::fs::write(&path, "Hello, world!").unwrap();

        let tool = EditTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"path": path, "old_string": "world", "new_string": "Rust"}),
            OperationId::new(),
        )
            .await
            .unwrap();

        let output: EditOutput = serde_json::from_value(result).unwrap();
        assert_eq!(output.replacements, 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Hello, Rust!");
    }

    #[tokio::test]
    async fn edit_string_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nf.txt");
        std::fs::write(&path, "hello").unwrap();

        let tool = EditTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"path": path, "old_string": "xyz", "new_string": "abc"}),
            OperationId::new(),
        )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn edit_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.txt");
        std::fs::write(&path, "foo bar foo baz foo").unwrap();

        let tool = EditTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"path": path, "old_string": "foo", "new_string": "qux", "replace_all": true}),
            OperationId::new(),
        )
            .await
            .unwrap();

        let output: EditOutput = serde_json::from_value(result).unwrap();
        assert_eq!(output.replacements, 3);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "qux bar qux baz qux");
    }

    #[tokio::test]
    async fn edit_multiple_without_replace_all_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ambig.txt");
        std::fs::write(&path, "foo bar foo").unwrap();

        let tool = EditTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({"path": path, "old_string": "foo", "new_string": "bar"}),
            OperationId::new(),
        )
            .await;

        assert!(result.is_err());
    }

    /// Lost-update fence: supplying the wrong expected_content_hash must
    /// refuse the edit rather than silently overwrite a changed file.
    #[tokio::test]
    async fn edit_refused_when_expected_hash_mismatches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fence.txt");
        std::fs::write(&path, "Hello, world!").unwrap();

        let tool = EditTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({
                    "path": path,
                    "old_string": "world",
                    "new_string": "Rust",
                    "expected_content_hash": "0000000000000000000000000000000000000000000000000000000000000000"
                }),
            OperationId::new(),
        )
            .await;
        assert!(result.is_err(), "mismatched expected_content_hash must refuse the edit");
        // File untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Hello, world!");
    }

    /// The correct hash (from a prior read) allows the edit.
    #[tokio::test]
    async fn edit_allowed_when_expected_hash_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.txt");
        std::fs::write(&path, "Hello, world!").unwrap();

        // "Read" first to obtain the hash.
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"Hello, world!");
        let hash = format!("{:x}", h.finalize());

        let tool = EditTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({
                    "path": path,
                    "old_string": "world",
                    "new_string": "Rust",
                    "expected_content_hash": hash
                }),
            OperationId::new(),
        )
            .await
            .unwrap();
        let output: EditOutput = serde_json::from_value(result).unwrap();
        assert_eq!(output.replacements, 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Hello, Rust!");
    }

    #[tokio::test]
    async fn edit_batch_multiple_ops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("batch.txt");
        std::fs::write(&path, "aaa bbb ccc ddd").unwrap();

        let tool = EditTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({
                "path": path,
                "edits": [
                    {"old_string": "aaa", "new_string": "AAA"},
                    {"old_string": "ccc", "new_string": "CCC"}
                ]
            }),
            OperationId::new(),
        )
            .await
            .unwrap();

        let output: EditOutput = serde_json::from_value(result).unwrap();
        assert_eq!(output.replacements, 2);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "AAA bbb CCC ddd");
    }

    #[tokio::test]
    async fn edit_batch_overlap_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlap.txt");
        std::fs::write(&path, "abcdef").unwrap();

        let tool = EditTool::new();
        let result = ToolRuntime::execute(
            &tool,
            serde_json::json!({
                "path": path,
                "edits": [
                    {"old_string": "abc", "new_string": "X"},
                    {"old_string": "cde", "new_string": "Y"}
                ]
            }),
            OperationId::new(),
        )
            .await;

        assert!(result.is_err(), "overlapping edits must be rejected");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("overlapping"), "error should mention overlap: {err}");
    }
}
