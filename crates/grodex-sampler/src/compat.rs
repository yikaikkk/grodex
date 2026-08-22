//! CompatibilityGate — 7-dimension compatibility check between
//! CanonicalModelRequest and RouteEntry.

use crate::route_config::RouteEntry;
use grodex_core::context::ContextItem;
use grodex_provider::canonical_request::CanonicalModelRequest;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompatibilityIssue {
    StreamingSupport {
        code: String,
        description: String,
    },
    ToolCallingSupport {
        code: String,
        description: String,
    },
    ResponseFormatSupport {
        code: String,
        description: String,
    },
    VisionImageSupport {
        code: String,
        description: String,
    },
    ParallelToolSupport {
        code: String,
        description: String,
    },
    MinReasoningTokens {
        code: String,
        description: String,
    },
    ModelIdEquivalence {
        code: String,
        description: String,
    },
}

impl CompatibilityIssue {
    pub fn code(&self) -> &str {
        match self {
            Self::StreamingSupport { code, .. }
            | Self::ToolCallingSupport { code, .. }
            | Self::ResponseFormatSupport { code, .. }
            | Self::VisionImageSupport { code, .. }
            | Self::ParallelToolSupport { code, .. }
            | Self::MinReasoningTokens { code, .. }
            | Self::ModelIdEquivalence { code, .. } => code,
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Self::StreamingSupport { description, .. }
            | Self::ToolCallingSupport { description, .. }
            | Self::ResponseFormatSupport { description, .. }
            | Self::VisionImageSupport { description, .. }
            | Self::ParallelToolSupport { description, .. }
            | Self::MinReasoningTokens { description, .. }
            | Self::ModelIdEquivalence { description, .. } => description,
        }
    }
}

pub struct CompatibilityGate;

impl CompatibilityGate {
    pub fn evaluate(request: &CanonicalModelRequest, route: &RouteEntry) -> Vec<CompatibilityIssue> {
        let mut issues = Vec::new();

        issues.extend(Self::check_streaming(request, route));
        issues.extend(Self::check_tool_calling(request, route));
        issues.extend(Self::check_response_format(request, route));
        issues.extend(Self::check_vision_image(request, route));
        issues.extend(Self::check_parallel_tools(request, route));
        issues.extend(Self::check_min_reasoning_tokens(request, route));
        issues.extend(Self::check_model_id_equivalence(request, route));

        issues
    }

    fn has_capability(route: &RouteEntry, cap: &str) -> bool {
        route.capabilities.iter().any(|c| c == cap)
    }

    fn check_streaming(
        request: &CanonicalModelRequest,
        route: &RouteEntry,
    ) -> Option<CompatibilityIssue> {
        let wants_streaming = request
            .provider_state_in
            .as_ref()
            .and_then(|v| v.get("stream"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if wants_streaming && !Self::has_capability(route, "streaming") {
            return Some(CompatibilityIssue::StreamingSupport {
                code: "COMPAT_STREAMING".to_string(),
                description: format!(
                    "request requires streaming but route '{}' does not declare streaming capability",
                    route.name
                ),
            });
        }
        None
    }

    fn check_tool_calling(
        request: &CanonicalModelRequest,
        route: &RouteEntry,
    ) -> Option<CompatibilityIssue> {
        let has_tools = !request.tool_specs.is_empty();
        if has_tools && !Self::has_capability(route, "tool_calls") {
            return Some(CompatibilityIssue::ToolCallingSupport {
                code: "COMPAT_TOOL_CALLS".to_string(),
                description: format!(
                    "request has {} tool specs but route '{}' does not declare tool_calls capability",
                    request.tool_specs.len(),
                    route.name
                ),
            });
        }
        None
    }

    fn check_response_format(
        request: &CanonicalModelRequest,
        route: &RouteEntry,
    ) -> Option<CompatibilityIssue> {
        if request.response_format.is_some() && !Self::has_capability(route, "json_mode") {
            return Some(CompatibilityIssue::ResponseFormatSupport {
                code: "COMPAT_RESPONSE_FORMAT".to_string(),
                description: format!(
                    "request specifies response_format (JSON) but route '{}' does not declare json_mode capability",
                    route.name
                ),
            });
        }
        None
    }

    fn check_vision_image(
        request: &CanonicalModelRequest,
        route: &RouteEntry,
    ) -> Option<CompatibilityIssue> {
        let has_image = request.context_items.iter().any(|item| {
            matches!(item, ContextItem::ImagePlaceholder { .. })
        });

        if has_image && !Self::has_capability(route, "vision") {
            return Some(CompatibilityIssue::VisionImageSupport {
                code: "COMPAT_VISION".to_string(),
                description: format!(
                    "request contains image items but route '{}' does not declare vision capability",
                    route.name
                ),
            });
        }
        None
    }

    fn check_parallel_tools(
        request: &CanonicalModelRequest,
        route: &RouteEntry,
    ) -> Option<CompatibilityIssue> {
        if request.parallel_tool_calls && !Self::has_capability(route, "parallel_tools") {
            return Some(CompatibilityIssue::ParallelToolSupport {
                code: "COMPAT_PARALLEL_TOOLS".to_string(),
                description: format!(
                    "request enables parallel_tool_calls but route '{}' does not declare parallel_tools capability",
                    route.name
                ),
            });
        }
        None
    }

    fn check_min_reasoning_tokens(
        request: &CanonicalModelRequest,
        route: &RouteEntry,
    ) -> Option<CompatibilityIssue> {
        let wants_reasoning = request
            .reasoning_request
            .as_ref()
            .map(|r| r.effort.is_some() || r.summary.is_some())
            .unwrap_or(false);

        if wants_reasoning && !Self::has_capability(route, "reasoning") {
            return Some(CompatibilityIssue::MinReasoningTokens {
                code: "COMPAT_REASONING".to_string(),
                description: format!(
                    "request specifies reasoning_request but route '{}' does not declare reasoning capability",
                    route.name
                ),
            });
        }
        None
    }

    fn check_model_id_equivalence(
        request: &CanonicalModelRequest,
        route: &RouteEntry,
    ) -> Option<CompatibilityIssue> {
        let binding_id = request.model_binding_id.to_string();
        let matches = route.canonical_model_id == binding_id
            || route.compatible_aliases.iter().any(|a| a == &binding_id);

        if !matches {
            return Some(CompatibilityIssue::ModelIdEquivalence {
                code: "COMPAT_MODEL_ID".to_string(),
                description: format!(
                    "request model_binding_id '{}' does not match route '{}' canonical_model_id '{}' or its aliases {:?}",
                    binding_id,
                    route.name,
                    route.canonical_model_id,
                    route.compatible_aliases
                ),
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grodex_core::id::{SessionId, StepId, TurnId};
    use grodex_provider::binding::ModelBindingId;

    /// ModelBindingId is an opaque UUID, so the route's canonical id and
    /// aliases must be UUID strings that match the request's binding id.
    const CANONICAL_BINDING: &str = "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9";
    const ALIAS_BINDING: &str = "1b2c3d4e-5f60-7182-93a4-b5c6d7e8f90a";
    const MISMATCH_BINDING: &str = "2c3d4e5f-6071-8293-a4b5-c6d7e8f90a1b";

    fn make_route(caps: &[&str]) -> RouteEntry {
        RouteEntry {
            name: "test-route".into(),
            provider: "openai".into(),
            canonical_model_id: CANONICAL_BINDING.into(),
            compatible_aliases: vec![ALIAS_BINDING.into()],
            endpoint: "https://api.openai.com/v1".into(),
            weight: 100,
            max_rpm: None,
            max_tpm: None,
            priority: 1,
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
            auth_env_var: "TEST_KEY".into(),
        }
    }

    fn make_request(binding_id_str: &str) -> CanonicalModelRequest {
        CanonicalModelRequest {
            request_id: "req-1".into(),
            session_id: SessionId::new(),
            turn_id: TurnId::new(),
            step_id: StepId::new(),
            model_binding_id: ModelBindingId::from_string(binding_id_str)
                .unwrap_or_else(|_| ModelBindingId::new()),
            prompt_snapshot_hash: None,
            instructions: vec![],
            context_items: vec![],
            tool_specs: vec![],
            tool_choice: grodex_provider::canonical_request::ToolChoice::Auto,
            parallel_tool_calls: false,
            reasoning_request: None,
            response_format: None,
            max_output_tokens: None,
            provider_state_in: None,
        }
    }

    #[test]
    fn compatible_route_no_issues() {
        let route = make_route(&["streaming", "tool_calls", "json_mode", "vision", "parallel_tools", "reasoning"]);
        let mut req = make_request(CANONICAL_BINDING);
        req.provider_state_in = Some(serde_json::json!({ "stream": true }));
        req.tool_specs.push(grodex_provider::canonical_request::ToolSpec {
            name: "t".into(),
            description: "d".into(),
            parameters: serde_json::json!({}),
            required: vec![],
        });
        req.response_format = Some(grodex_provider::canonical_request::ResponseFormat { json_schema: None });
        req.parallel_tool_calls = true;
        req.reasoning_request = Some(grodex_provider::canonical_request::ReasoningRequest {
            effort: Some("high".into()),
            summary: None,
        });
        req.context_items.push(ContextItem::ImagePlaceholder {
            mime_type: "image/png".into(),
            artifact_ref: "ref".into(),
        });

        let issues = CompatibilityGate::evaluate(&req, &route);
        assert!(issues.is_empty(), "expected no issues, got: {:?}", issues);
    }

    #[test]
    fn missing_streaming_capability() {
        let route = make_route(&[]);
        let mut req = make_request(CANONICAL_BINDING);
        req.provider_state_in = Some(serde_json::json!({ "stream": true }));

        let issues = CompatibilityGate::evaluate(&req, &route);
        assert!(issues.iter().any(|i| i.code() == "COMPAT_STREAMING"));
    }

    #[test]
    fn missing_tool_calls_capability() {
        let route = make_route(&[]);
        let mut req = make_request(CANONICAL_BINDING);
        req.tool_specs.push(grodex_provider::canonical_request::ToolSpec {
            name: "t".into(),
            description: "d".into(),
            parameters: serde_json::json!({}),
            required: vec![],
        });

        let issues = CompatibilityGate::evaluate(&req, &route);
        assert!(issues.iter().any(|i| i.code() == "COMPAT_TOOL_CALLS"));
    }

    #[test]
    fn missing_json_mode_capability() {
        let route = make_route(&[]);
        let mut req = make_request(CANONICAL_BINDING);
        req.response_format = Some(grodex_provider::canonical_request::ResponseFormat { json_schema: None });

        let issues = CompatibilityGate::evaluate(&req, &route);
        assert!(issues.iter().any(|i| i.code() == "COMPAT_RESPONSE_FORMAT"));
    }

    #[test]
    fn missing_vision_capability() {
        let route = make_route(&[]);
        let mut req = make_request(CANONICAL_BINDING);
        req.context_items.push(ContextItem::ImagePlaceholder {
            mime_type: "image/png".into(),
            artifact_ref: "ref".into(),
        });

        let issues = CompatibilityGate::evaluate(&req, &route);
        assert!(issues.iter().any(|i| i.code() == "COMPAT_VISION"));
    }

    #[test]
    fn missing_parallel_tools_capability() {
        let route = make_route(&[]);
        let mut req = make_request(CANONICAL_BINDING);
        req.parallel_tool_calls = true;

        let issues = CompatibilityGate::evaluate(&req, &route);
        assert!(issues.iter().any(|i| i.code() == "COMPAT_PARALLEL_TOOLS"));
    }

    #[test]
    fn missing_reasoning_capability() {
        let route = make_route(&[]);
        let mut req = make_request(CANONICAL_BINDING);
        req.reasoning_request = Some(grodex_provider::canonical_request::ReasoningRequest {
            effort: Some("high".into()),
            summary: None,
        });

        let issues = CompatibilityGate::evaluate(&req, &route);
        assert!(issues.iter().any(|i| i.code() == "COMPAT_REASONING"));
    }

    #[test]
    fn model_id_mismatch() {
        let route = make_route(&[]);
        let req = make_request(MISMATCH_BINDING);
        let issues = CompatibilityGate::evaluate(&req, &route);
        assert!(issues.iter().any(|i| i.code() == "COMPAT_MODEL_ID"));
    }

    #[test]
    fn model_id_matches_alias() {
        let route = make_route(&[]);
        let req = make_request(ALIAS_BINDING);
        let issues = CompatibilityGate::evaluate(&req, &route);
        assert!(
            !issues.iter().any(|i| i.code() == "COMPAT_MODEL_ID"),
            "alias should match, got issues: {:?}",
            issues.iter().filter(|i| i.code() == "COMPAT_MODEL_ID").collect::<Vec<_>>()
        );
    }
}
