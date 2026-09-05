//! Evidence Unit Template — Phase 1 fixed output structure (Design 08 §7).
//!
//! Each Evidence Unit must follow a fixed four-section structure rather than
//! free-form Markdown. The machine-readable metadata is embedded in an HTML
//! comment immediately adjacent to the title; it is the source of truth, while
//! the SQLite row is only a projection. If the human-readable header fields
//! disagree with the comment metadata, indexing must stop and emit a diagnostic.

use crate::types::{EvidenceStatus, EvidenceUnit, MemoryScope};
use serde::{Deserialize, Serialize};
use sha2::Digest;

/// The four fixed sections of a Phase 1 Evidence Unit (doc 08 §7).
/// Each unit must follow this exact structure, not free-form Markdown.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceTemplate {
    /// Machine-readable metadata embedded in the HTML comment.
    pub metadata: EvidenceMetadata,
    /// Human-readable title after "## Evidence Unit: ".
    pub title: String,
    /// The four fixed sections in order.
    pub problem: String,
    pub diagnosis: String,
    pub resolution: String,
    pub verification: String,
}

/// Metadata embedded in the `<!-- evidence-unit: {...} -->` HTML comment.
/// This is the source of truth; SQLite only stores a projection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceMetadata {
    pub id: String,
    pub rollout_id: String,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default = "default_status")]
    pub status: String,
}

fn default_scope() -> String {
    "workspace".into()
}
fn default_status() -> String {
    "active".into()
}

/// Diagnostic result of validating a parsed Evidence Unit.
#[derive(Debug, Clone)]
pub struct EvidenceValidation {
    pub is_valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl EvidenceTemplate {
    /// Parse an Evidence Unit from its Markdown representation.
    /// Extracts the HTML comment metadata and the four fixed sections.
    pub fn from_markdown(md: &str) -> Result<Self, String> {
        // 1. Extract <!-- evidence-unit: {...} --> JSON
        let metadata = Self::extract_metadata(md)?;

        // 2. Parse the "## Evidence Unit: <title>" line
        let title = Self::extract_title(md)?;

        // 3. Extract Rollout-ID, Occurred-At (optional), Scope, Status header fields
        let header_rollout_id = Self::extract_field(md, "Rollout-ID:")?;
        let header_scope = Self::extract_field(md, "Scope:")?;
        let header_status = Self::extract_field(md, "Status:")?;
        // Occurred-At is informational only (not stored in the template struct);
        // its absence is not fatal so that round-trip rendering stays lossless.
        let _header_occurred_at = Self::extract_field_optional(md, "Occurred-At:");

        // 4. Extract the four fixed sections
        let problem = Self::extract_section(md, "Problem")?;
        let diagnosis = Self::extract_section(md, "Diagnosis")?;
        let resolution = Self::extract_section(md, "Resolution")?;
        let verification = Self::extract_section(md, "Verification")?;

        let template = Self {
            metadata,
            title,
            problem,
            diagnosis,
            resolution,
            verification,
        };

        // 5. Validate consistency between HTML comment metadata and header fields
        let validation = template.validate(&header_rollout_id, &header_scope, &header_status);
        if !validation.is_valid {
            return Err(validation.errors.join("; "));
        }

        Ok(template)
    }

    /// Render this Evidence Unit back to its Markdown representation.
    pub fn to_markdown(&self) -> String {
        let json = serde_json::to_string(&self.metadata)
            .unwrap_or_else(|_| "{}".to_string());

        let mut md = String::new();
        md.push_str(&format!("<!-- evidence-unit: {} -->\n", json));
        md.push_str(&format!("## Evidence Unit: {}\n", self.title));
        md.push_str(&format!("Rollout-ID: {}\n", self.metadata.rollout_id));
        md.push_str(&format!("Scope: {}\n", self.metadata.scope));
        md.push_str(&format!("Status: {}\n", self.metadata.status));
        md.push('\n');
        md.push_str(&format!("### Problem\n{}\n\n", self.problem));
        md.push_str(&format!("### Diagnosis\n{}\n\n", self.diagnosis));
        md.push_str(&format!("### Resolution\n{}\n\n", self.resolution));
        md.push_str(&format!("### Verification\n{}\n", self.verification));
        md
    }

    /// Validate that the HTML comment metadata is consistent with the
    /// human-readable header fields (doc 08 §7: "不一致时停止索引并产生诊断").
    pub fn validate(
        &self,
        header_rollout_id: &str,
        header_scope: &str,
        header_status: &str,
    ) -> EvidenceValidation {
        let mut errors = Vec::new();
        let warnings = Vec::new();

        if self.metadata.rollout_id != header_rollout_id {
            errors.push(format!(
                "rollout_id mismatch: metadata='{}' header='{}'",
                self.metadata.rollout_id, header_rollout_id
            ));
        }
        if self.metadata.scope != header_scope {
            errors.push(format!(
                "scope mismatch: metadata='{}' header='{}'",
                self.metadata.scope, header_scope
            ));
        }
        if self.metadata.status != header_status {
            errors.push(format!(
                "status mismatch: metadata='{}' header='{}'",
                self.metadata.status, header_status
            ));
        }

        let is_valid = errors.is_empty();
        EvidenceValidation {
            is_valid,
            errors,
            warnings,
        }
    }

    /// Convert this template into an EvidenceUnit for database storage.
    pub fn to_evidence_unit(&self, occurred_at: chrono::DateTime<chrono::Utc>) -> EvidenceUnit {
        let content = self.to_markdown();
        let content_hash = format!("{:x}", sha2::Sha256::digest(content.as_bytes()));
        let scope = MemoryScope::from_str(&self.metadata.scope)
            .unwrap_or(MemoryScope::Workspace);
        let status = EvidenceStatus::from_str(&self.metadata.status)
            .unwrap_or(EvidenceStatus::Active);
        let now = chrono::Utc::now();
        let path = String::new();
        let section = format!("Evidence Unit: {}", self.title);
        let fingerprint = EvidenceUnit::compute_fingerprint(
            &self.metadata.rollout_id,
            &path,
            &section,
            0,
            &content_hash,
        );

        EvidenceUnit {
            id: self.metadata.id.clone(),
            rollout_id: self.metadata.rollout_id.clone(),
            path,
            section,
            scope,
            status,
            content,
            content_hash,
            fingerprint,
            occurred_at,
            created_at: now,
            superseded_by: None,
            superseded_at: None,
            rollout_available: true,
            rollout_expired_at: None,
            subchunk_index: 0,
        }
    }

    /// Check if this unit exceeds the 1600 character hard limit and needs
    /// sub-chunking (doc 08 §7: only chunk if > 1600 chars).
    pub fn needs_subchunking(&self) -> bool {
        self.to_markdown().len() > 1600
    }

    // ── Private parsing helpers ──────────────────────────────────

    fn extract_metadata(md: &str) -> Result<EvidenceMetadata, String> {
        let marker = "<!-- evidence-unit:";
        let start = md
            .find(marker)
            .ok_or("missing '<!-- evidence-unit: ... -->' HTML comment")?;
        let after_start = &md[start + marker.len()..];
        let end = after_start
            .find("-->")
            .ok_or("missing '-->' closing comment for evidence-unit metadata")?;
        let json_str = after_start[..end].trim();
        serde_json::from_str(json_str)
            .map_err(|e| format!("failed to parse evidence-unit metadata JSON: {}", e))
    }

    fn extract_title(md: &str) -> Result<String, String> {
        for line in md.lines() {
            if let Some(rest) = line.trim().strip_prefix("## Evidence Unit:") {
                let title = rest.trim();
                if !title.is_empty() {
                    return Ok(title.to_string());
                }
            }
        }
        Err("missing '## Evidence Unit: <title>' line".into())
    }

    fn extract_field_optional(md: &str, field_name: &str) -> Option<String> {
        for line in md.lines() {
            if let Some(rest) = line.trim().strip_prefix(field_name) {
                let value = rest.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
        None
    }

    fn extract_field(md: &str, field_name: &str) -> Result<String, String> {
        Self::extract_field_optional(md, field_name)
            .ok_or_else(|| format!("missing or empty '{}' header field", field_name))
    }

    fn extract_section(md: &str, section_name: &str) -> Result<String, String> {
        let heading = format!("### {}", section_name);
        let lines: Vec<&str> = md.lines().collect();

        let start_idx = lines
            .iter()
            .position(|l| l.trim() == heading)
            .ok_or_else(|| format!("missing '### {}' section", section_name))?;

        let mut content_lines: Vec<&str> = Vec::new();
        for line in &lines[start_idx + 1..] {
            if line.trim().starts_with("### ") {
                break;
            }
            content_lines.push(line);
        }

        let result = content_lines.join("\n").trim().to_string();
        if result.is_empty() {
            return Err(format!("'### {}' section is empty", section_name));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_MD: &str = r#"<!-- evidence-unit: {"id":"ev_docx_preview_failure_019","rollout_id":"019abc","scope":"workspace","status":"active"} -->
## Evidence Unit: docx-preview-failure
Rollout-ID: 019abc
Occurred-At: 2026-07-29T14:02:57Z
Scope: workspace
Status: active

### Problem
DOCX 文件打开后没有正文显示。

### Diagnosis
资源加载链路未完成，而普通文本文件走的是另一条读取路径。

### Resolution
修复资源加载协议并保留错误状态展示。

### Verification
手动打开 test.docx 后正文可以显示。
"#;

    #[test]
    fn parse_full_evidence_unit() {
        let template = EvidenceTemplate::from_markdown(SAMPLE_MD).unwrap();
        assert_eq!(template.metadata.id, "ev_docx_preview_failure_019");
        assert_eq!(template.metadata.rollout_id, "019abc");
        assert_eq!(template.metadata.scope, "workspace");
        assert_eq!(template.metadata.status, "active");
        assert_eq!(template.title, "docx-preview-failure");
        assert!(template.problem.contains("DOCX"));
        assert!(template.diagnosis.contains("资源加载"));
        assert!(template.resolution.contains("修复"));
        assert!(template.verification.contains("test.docx"));
    }

    #[test]
    fn render_to_markdown_format_correct() {
        let template = EvidenceTemplate {
            metadata: EvidenceMetadata {
                id: "ev_test_001".into(),
                rollout_id: "rollout_1".into(),
                scope: "workspace".into(),
                status: "active".into(),
            },
            title: "test-issue".into(),
            problem: "The problem description".into(),
            diagnosis: "The root cause".into(),
            resolution: "The fix applied".into(),
            verification: "The verification step".into(),
        };
        let md = template.to_markdown();

        assert!(md.contains("<!-- evidence-unit:"));
        assert!(md.contains("\"id\":\"ev_test_001\""));
        assert!(md.contains("## Evidence Unit: test-issue"));
        assert!(md.contains("Rollout-ID: rollout_1"));
        assert!(md.contains("Scope: workspace"));
        assert!(md.contains("Status: active"));
        assert!(md.contains("### Problem"));
        assert!(md.contains("The problem description"));
        assert!(md.contains("### Diagnosis"));
        assert!(md.contains("The root cause"));
        assert!(md.contains("### Resolution"));
        assert!(md.contains("The fix applied"));
        assert!(md.contains("### Verification"));
        assert!(md.contains("The verification step"));
    }

    #[test]
    fn round_trip_parse_then_render_then_parse() {
        let original = EvidenceTemplate::from_markdown(SAMPLE_MD).unwrap();
        let rendered = original.to_markdown();
        let reparsed = EvidenceTemplate::from_markdown(&rendered).unwrap();
        assert_eq!(original, reparsed);
    }

    #[test]
    fn metadata_header_mismatch_reports_error() {
        let md = SAMPLE_MD.replace("Rollout-ID: 019abc", "Rollout-ID: WRONG_ID");
        let result = EvidenceTemplate::from_markdown(&md);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("rollout_id mismatch"), "got: {}", err);

        // Also test validate() directly.
        let template = EvidenceTemplate {
            metadata: EvidenceMetadata {
                id: "ev_x".into(),
                rollout_id: "meta_id".into(),
                scope: "workspace".into(),
                status: "active".into(),
            },
            title: "x".into(),
            problem: "p".into(),
            diagnosis: "d".into(),
            resolution: "r".into(),
            verification: "v".into(),
        };
        let validation = template.validate("header_id", "workspace", "active");
        assert!(!validation.is_valid);
        assert_eq!(validation.errors.len(), 1);
        assert!(validation.errors[0].contains("rollout_id mismatch"));
    }

    #[test]
    fn scope_mismatch_reports_error() {
        let template = EvidenceTemplate {
            metadata: EvidenceMetadata {
                id: "ev_x".into(),
                rollout_id: "r1".into(),
                scope: "workspace".into(),
                status: "active".into(),
            },
            title: "x".into(),
            problem: "p".into(),
            diagnosis: "d".into(),
            resolution: "r".into(),
            verification: "v".into(),
        };
        let validation = template.validate("r1", "global", "active");
        assert!(!validation.is_valid);
        assert!(validation.errors[0].contains("scope mismatch"));
    }

    #[test]
    fn status_mismatch_reports_error() {
        let template = EvidenceTemplate {
            metadata: EvidenceMetadata {
                id: "ev_x".into(),
                rollout_id: "r1".into(),
                scope: "workspace".into(),
                status: "active".into(),
            },
            title: "x".into(),
            problem: "p".into(),
            diagnosis: "d".into(),
            resolution: "r".into(),
            verification: "v".into(),
        };
        let validation = template.validate("r1", "workspace", "superseded");
        assert!(!validation.is_valid);
        assert!(validation.errors[0].contains("status mismatch"));
    }

    #[test]
    fn consistent_metadata_header_is_valid() {
        let template = EvidenceTemplate {
            metadata: EvidenceMetadata {
                id: "ev_x".into(),
                rollout_id: "r1".into(),
                scope: "workspace".into(),
                status: "active".into(),
            },
            title: "x".into(),
            problem: "p".into(),
            diagnosis: "d".into(),
            resolution: "r".into(),
            verification: "v".into(),
        };
        let validation = template.validate("r1", "workspace", "active");
        assert!(validation.is_valid);
        assert!(validation.errors.is_empty());
    }

    #[test]
    fn exceeds_1600_chars_needs_subchunking() {
        let long_text = "x".repeat(600);
        let template = EvidenceTemplate {
            metadata: EvidenceMetadata {
                id: "ev_big".into(),
                rollout_id: "r1".into(),
                scope: "workspace".into(),
                status: "active".into(),
            },
            title: "big-unit".into(),
            problem: long_text.clone(),
            diagnosis: long_text.clone(),
            resolution: long_text.clone(),
            verification: long_text,
        };
        assert!(template.needs_subchunking());
    }

    #[test]
    fn under_1600_chars_no_subchunking() {
        let template = EvidenceTemplate {
            metadata: EvidenceMetadata {
                id: "ev_small".into(),
                rollout_id: "r1".into(),
                scope: "workspace".into(),
                status: "active".into(),
            },
            title: "small".into(),
            problem: "p".into(),
            diagnosis: "d".into(),
            resolution: "r".into(),
            verification: "v".into(),
        };
        assert!(!template.needs_subchunking());
    }

    #[test]
    fn to_evidence_unit_preserves_metadata() {
        let template = EvidenceTemplate {
            metadata: EvidenceMetadata {
                id: "ev_conv_001".into(),
                rollout_id: "rollout_42".into(),
                scope: "global".into(),
                status: "active".into(),
            },
            title: "conv-test".into(),
            problem: "p".into(),
            diagnosis: "d".into(),
            resolution: "r".into(),
            verification: "v".into(),
        };
        let occurred_at = chrono::Utc::now();
        let unit = template.to_evidence_unit(occurred_at);

        assert_eq!(unit.id, "ev_conv_001");
        assert_eq!(unit.rollout_id, "rollout_42");
        assert_eq!(unit.scope, MemoryScope::Global);
        assert_eq!(unit.status, EvidenceStatus::Active);
        assert_eq!(unit.occurred_at, occurred_at);
        assert_eq!(unit.section, "Evidence Unit: conv-test");
        assert!(!unit.content.is_empty());
        assert!(!unit.content_hash.is_empty());
        assert_eq!(unit.subchunk_index, 0);
        assert!(unit.rollout_available);
        assert!(unit.superseded_by.is_none());
    }

    #[test]
    fn missing_metadata_comment_is_error() {
        let md = "## Evidence Unit: no-comment\nRollout-ID: r\nScope: workspace\nStatus: active\n\n### Problem\np\n\n### Diagnosis\nd\n\n### Resolution\nr\n\n### Verification\nv\n";
        let result = EvidenceTemplate::from_markdown(md);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("evidence-unit"));
    }

    #[test]
    fn missing_section_is_error() {
        let md = r#"<!-- evidence-unit: {"id":"ev_x","rollout_id":"r","scope":"workspace","status":"active"} -->
## Evidence Unit: missing-section
Rollout-ID: r
Scope: workspace
Status: active

### Problem
p

### Diagnosis
d

### Resolution
r
"#;
        let result = EvidenceTemplate::from_markdown(md);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Verification"));
    }
}
