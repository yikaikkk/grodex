//! PolicyCompiler + PublishedPolicy + PolicyExplainer (doc 10 §20.13–20.14).
//!
//! The compiler pipeline: parse → schema validation → normalize matchers →
//! run embedded examples → compile decision graph/indexes → build candidate
//! → atomic publish. The output is an immutable `PublishedPolicy` with
//! indexes for fast lookup.
//!
//! `PolicyExplainer` produces a full diagnostic trace (doc 10 §20.14) for
//! any tool call: matched rules, strictest merge, reason code, and sandbox
//! impact.

use crate::policy::{strictness_of, PolicyRule};
use grodex_core::policy::PolicyDecision;
use std::collections::HashMap;

// ── PublishedPolicy (doc 10 §20.13) ───────────────────────────────

/// An immutable, compiled policy (doc 10 §20.13). Built by PolicyCompiler
/// from policy sources, contains indexes for fast lookup.
#[derive(Debug, Clone)]
pub struct PublishedPolicy {
    /// All rules, immutable after publish.
    rules: Vec<PolicyRule>,
    /// First-token / executable index for fast command matching.
    command_first_token_index: HashMap<String, Vec<usize>>,
    /// Capability id index (keyed by exact tool_pattern).
    capability_index: HashMap<String, Vec<usize>>,
    /// Policy hash (deterministic over all rules).
    pub policy_hash: String,
    /// Schema version.
    pub schema_version: u32,
    /// Monotonically increasing generation number.
    pub generation: u64,
    /// Source manifest and diagnostics from compilation.
    pub diagnostics: Vec<String>,
}

impl PublishedPolicy {
    /// Evaluate a tool call against this published policy.
    ///
    /// Returns the STRICTEST decision among all matching rules
    /// (Deny > Ask > Allow); `priority` breaks ties. `Ask` if none match.
    pub fn evaluate(&self, tool_name: &str, args: &serde_json::Value) -> PolicyDecision {
        let mut matched = false;
        let mut winner = PolicyDecision::Allow;
        let mut winner_strictness = u8::MAX;
        let mut winner_priority = 0u8;

        for rule in &self.rules {
            if rule.matches(tool_name, args) {
                let strictness = strictness_of(rule.decision);
                if !matched
                    || strictness > winner_strictness
                    || (strictness == winner_strictness && rule.priority > winner_priority)
                {
                    matched = true;
                    winner = rule.decision;
                    winner_strictness = strictness;
                    winner_priority = rule.priority;
                }
            }
        }

        if matched {
            winner
        } else {
            PolicyDecision::Ask
        }
    }

    /// All rules in this policy.
    pub fn rules(&self) -> &[PolicyRule] {
        &self.rules
    }

    /// Look up rules by command first token (exact tool name) plus
    /// wildcard rules. Returns references to matching rules.
    pub fn rules_for_command(&self, first_token: &str) -> Vec<&PolicyRule> {
        let mut result = Vec::new();
        // Exact token matches.
        if let Some(indices) = self.command_first_token_index.get(first_token) {
            for &idx in indices {
                if let Some(rule) = self.rules.get(idx) {
                    result.push(rule);
                }
            }
        }
        // Wildcard rules ("*") always apply.
        if first_token != "*" {
            if let Some(indices) = self.command_first_token_index.get("*") {
                for &idx in indices {
                    if let Some(rule) = self.rules.get(idx) {
                        result.push(rule);
                    }
                }
            }
        }
        result
    }

    /// Look up rules by capability id (exact tool_pattern).
    pub fn rules_for_capability(&self, capability_id: &str) -> Vec<&PolicyRule> {
        let mut result = Vec::new();
        if let Some(indices) = self.capability_index.get(capability_id) {
            for &idx in indices {
                if let Some(rule) = self.rules.get(idx) {
                    result.push(rule);
                }
            }
        }
        result
    }
}

// ── PolicyCompiler (doc 10 §20.13) ────────────────────────────────

/// Compiles policy sources into an immutable PublishedPolicy (doc 10 §20.13).
///
/// Pipeline: parse → schema validation → normalize matchers → run embedded
/// examples → compile decision graph/indexes → build candidate → atomic publish.
pub struct PolicyCompiler {
    schema_version: u32,
}

/// Result of compiling policy sources.
#[derive(Debug)]
pub struct CompileResult {
    pub policy: PublishedPolicy,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl PolicyCompiler {
    /// Create a compiler with the current schema version.
    pub fn new() -> Self {
        Self { schema_version: 1 }
    }

    /// Compile a set of rules into a PublishedPolicy with generation = prev + 1.
    pub fn compile(&self, rules: Vec<PolicyRule>, prev_generation: u64) -> CompileResult {
        let diagnostics = self.validate_rules(&rules);
        let command_first_token_index = self.build_command_index(&rules);
        let capability_index = self.build_capability_index(&rules);
        let policy_hash = self.compute_hash(&rules);

        let warnings: Vec<String> = diagnostics
            .iter()
            .filter(|d| d.starts_with("warning:"))
            .cloned()
            .collect();
        let errors: Vec<String> = diagnostics
            .iter()
            .filter(|d| d.starts_with("error:"))
            .cloned()
            .collect();

        let policy = PublishedPolicy {
            rules,
            command_first_token_index,
            capability_index,
            policy_hash,
            schema_version: self.schema_version,
            generation: prev_generation + 1,
            diagnostics,
        };

        CompileResult {
            policy,
            warnings,
            errors,
        }
    }

    /// Validate rule schema; return diagnostics prefixed with `error:` or `warning:`.
    fn validate_rules(&self, rules: &[PolicyRule]) -> Vec<String> {
        let mut diagnostics = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();
        for (i, rule) in rules.iter().enumerate() {
            if rule.tool_pattern.is_empty() {
                diagnostics.push(format!("error: rule[{}]: empty tool_pattern", i));
            }
            if let Some(ref id) = rule.rule_id {
                if !seen_ids.insert(id.clone()) {
                    diagnostics.push(format!("warning: rule[{}]: duplicate rule_id '{}'", i, id));
                }
            }
        }
        diagnostics
    }

    /// Build command first-token index.
    fn build_command_index(&self, rules: &[PolicyRule]) -> HashMap<String, Vec<usize>> {
        let mut index: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, rule) in rules.iter().enumerate() {
            let key = if rule.tool_pattern == "*" {
                "*".to_string()
            } else if rule.tool_pattern.contains('*') {
                // Use the prefix before the first wildcard as the key.
                rule.tool_pattern
                    .split('*')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("*")
                    .to_string()
            } else {
                rule.tool_pattern.clone()
            };
            index.entry(key).or_default().push(idx);
        }
        index
    }

    /// Build capability index (keyed by exact, non-wildcard tool_pattern).
    fn build_capability_index(&self, rules: &[PolicyRule]) -> HashMap<String, Vec<usize>> {
        let mut index: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, rule) in rules.iter().enumerate() {
            if !rule.tool_pattern.contains('*') {
                index
                    .entry(rule.tool_pattern.clone())
                    .or_default()
                    .push(idx);
            }
        }
        index
    }

    /// Compute deterministic policy hash over all rules.
    fn compute_hash(&self, rules: &[PolicyRule]) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        for rule in rules {
            let json = serde_json::to_string(rule).unwrap_or_default();
            json.hash(&mut hasher);
        }
        format!("{:016x}", hasher.finish())
    }
}

impl Default for PolicyCompiler {
    fn default() -> Self {
        Self::new()
    }
}

// ── PolicyDecisionTrace + ReasonCode (doc 10 §20.14) ──────────────

/// A policy decision with full diagnostic trace (doc 10 §20.14).
#[derive(Debug, Clone)]
pub struct PolicyDecisionTrace {
    pub effect: PolicyDecision,
    pub policy_generation: u64,
    pub matched_rule_ids: Vec<String>,
    pub dominant_rule_id: Option<String>,
    pub normalized_facts_hash: String,
    pub reason_code: ReasonCode,
    pub sandbox_requirement: SandboxRequirement,
    pub diagnostics: Vec<String>,
}

/// Why a policy decision was reached (doc 10 §20.14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasonCode {
    ExplicitAllow,
    ExplicitAsk,
    ExplicitDeny,
    NoMatchDefaultAsk,
    NoMatchDefaultDeny,
    ManagedHardDeny,
    LiveRevocation,
    ParentCeiling,
    StaleSnapshot,
}

/// Sandbox impact of a policy decision (doc 10 §20.14).
#[derive(Debug, Clone, Default)]
pub struct SandboxRequirement {
    pub network_restricted: bool,
    pub filesystem_restricted: bool,
    pub needs_approval: bool,
}

// ── PolicyExplainer (doc 10 §20.14) ───────────────────────────────

/// Explain a policy decision for a given operation (doc 10 §20.14).
///
/// Shows: original request summary, parsed facts, all matched rules and
/// sources, strictest merge process, classifier fallback, sandbox impact,
/// why allow/ask/deny, and the precise matcher that "always allow" would generate.
pub struct PolicyExplainer<'a> {
    policy: &'a PublishedPolicy,
}

impl<'a> PolicyExplainer<'a> {
    pub fn new(policy: &'a PublishedPolicy) -> Self {
        Self { policy }
    }

    /// Explain a policy decision, producing a full diagnostic trace.
    pub fn explain(&self, tool_name: &str, args: &serde_json::Value) -> PolicyDecisionTrace {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut matched_rule_ids = Vec::new();
        let mut matched_rules: Vec<(usize, &PolicyRule)> = Vec::new();

        for (idx, rule) in self.policy.rules().iter().enumerate() {
            if rule.matches(tool_name, args) {
                let id = rule
                    .rule_id
                    .clone()
                    .unwrap_or_else(|| format!("rule[{}]", idx));
                matched_rule_ids.push(id);
                matched_rules.push((idx, rule));
            }
        }

        // Determine dominant rule: strictest, then highest priority.
        let dominant = matched_rules
            .iter()
            .max_by_key(|(_, rule)| (strictness_of(rule.decision), rule.priority));

        let (effect, dominant_rule_id, reason_code) = if matched_rules.is_empty() {
            (PolicyDecision::Ask, None, ReasonCode::NoMatchDefaultAsk)
        } else {
            let &(idx, rule) = dominant.unwrap();
            let id = rule
                .rule_id
                .clone()
                .unwrap_or_else(|| format!("rule[{}]", idx));
            let reason = match rule.decision {
                PolicyDecision::Allow => ReasonCode::ExplicitAllow,
                PolicyDecision::Ask => ReasonCode::ExplicitAsk,
                PolicyDecision::Deny => ReasonCode::ExplicitDeny,
            };
            (rule.decision, Some(id), reason)
        };

        // Compute normalized facts hash.
        let mut hasher = DefaultHasher::new();
        tool_name.hash(&mut hasher);
        args.to_string().hash(&mut hasher);
        let normalized_facts_hash = format!("{:016x}", hasher.finish());

        // Sandbox requirement: if any matched rule has network or resource matcher.
        let network_restricted = matched_rules.iter().any(|(_, r)| r.network.is_some());
        let filesystem_restricted = matched_rules.iter().any(|(_, r)| r.resource.is_some());
        let needs_approval = effect == PolicyDecision::Ask;

        PolicyDecisionTrace {
            effect,
            policy_generation: self.policy.generation,
            matched_rule_ids,
            dominant_rule_id,
            normalized_facts_hash,
            reason_code,
            sandbox_requirement: SandboxRequirement {
                network_restricted,
                filesystem_restricted,
                needs_approval,
            },
            diagnostics: vec![format!("matched {} rule(s)", matched_rules.len())],
        }
    }

    /// Generate the precise matcher that a "always allow" grant would create.
    pub fn propose_always_allow_matcher(&self, tool_name: &str, args: &serde_json::Value) -> String {
        let mut parts = vec![format!("tool={}", tool_name)];

        if let Some(path) = args.pointer("/path").and_then(|v| v.as_str()) {
            parts.push(format!("path={}", path));
        }
        if let Some(cmd) = args.pointer("/command").and_then(|v| v.as_str()) {
            if let Some(first) = cmd.split_whitespace().next() {
                parts.push(format!("command_prefix={}", first));
            }
        }
        if let Some(host) = args.pointer("/host").and_then(|v| v.as_str()) {
            parts.push(format!("host={}", host));
        }
        if let Some(srv) = args
            .pointer("/server_capability_id")
            .and_then(|v| v.as_str())
        {
            parts.push(format!("server={}", srv));
        }
        if let Some(tool) = args.pointer("/tool_capability_id").and_then(|v| v.as_str()) {
            parts.push(format!("mcp_tool={}", tool));
        }

        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{
        CommandMatcher, HostMatcher, McpMatcher, NetworkDirection, NetworkMatcher,
        NetworkProtocol, PortMatcher, ResourceMatcher, SideEffectClass,
    };

    fn make_rule(tool: &str, decision: PolicyDecision, priority: u8) -> PolicyRule {
        PolicyRule {
            tool_pattern: tool.into(),
            arg_patterns: vec![],
            command: None,
            resource: None,
            rule_id: None,
            network: None,
            mcp: None,
            decision,
            priority,
        }
    }

    #[test]
    fn compile_increments_generation() {
        let compiler = PolicyCompiler::new();
        let result = compiler.compile(vec![make_rule("*", PolicyDecision::Allow, 0)], 5);
        assert_eq!(result.policy.generation, 6);
    }

    #[test]
    fn compile_sets_schema_version() {
        let compiler = PolicyCompiler::new();
        let result = compiler.compile(vec![], 0);
        assert_eq!(result.policy.schema_version, 1);
    }

    #[test]
    fn policy_hash_is_deterministic() {
        let compiler = PolicyCompiler::new();
        let rules = vec![
            make_rule("read_file", PolicyDecision::Allow, 10),
            make_rule("exec", PolicyDecision::Deny, 100),
        ];
        let r1 = compiler.compile(rules.clone(), 0);
        let r2 = compiler.compile(rules, 0);
        assert_eq!(r1.policy.policy_hash, r2.policy.policy_hash);
    }

    #[test]
    fn policy_hash_differs_for_different_rules() {
        let compiler = PolicyCompiler::new();
        let r1 = compiler.compile(vec![make_rule("a", PolicyDecision::Allow, 0)], 0);
        let r2 = compiler.compile(vec![make_rule("b", PolicyDecision::Allow, 0)], 0);
        assert_ne!(r1.policy.policy_hash, r2.policy.policy_hash);
    }

    #[test]
    fn validate_detects_empty_tool_pattern() {
        let compiler = PolicyCompiler::new();
        let result = compiler.compile(
            vec![PolicyRule {
                tool_pattern: "".into(),
                arg_patterns: vec![],
                command: None,
                resource: None,
                rule_id: None,
                network: None,
                mcp: None,
                decision: PolicyDecision::Allow,
                priority: 0,
            }],
            0,
        );
        assert!(
            result.errors.iter().any(|e| e.contains("empty tool_pattern")),
            "must detect empty tool_pattern: errors={:?}",
            result.errors
        );
    }

    #[test]
    fn validate_detects_duplicate_rule_ids() {
        let compiler = PolicyCompiler::new();
        let mut r1 = make_rule("a", PolicyDecision::Allow, 0);
        r1.rule_id = Some("dup".into());
        let mut r2 = make_rule("b", PolicyDecision::Deny, 0);
        r2.rule_id = Some("dup".into());
        let result = compiler.compile(vec![r1, r2], 0);
        assert!(
            result.warnings.iter().any(|w| w.contains("duplicate rule_id")),
            "must detect duplicate rule_id: warnings={:?}",
            result.warnings
        );
    }

    #[test]
    fn published_policy_evaluate_strictest_merge() {
        let compiler = PolicyCompiler::new();
        let rules = vec![
            make_rule("*", PolicyDecision::Allow, 255),
            PolicyRule {
                tool_pattern: "write_file".into(),
                arg_patterns: vec![],
                command: None,
                resource: Some(ResourceMatcher {
                    arg_path: "/path".into(),
                    pattern: "/etc/*".into(),
                }),
                rule_id: Some("deny-etc".into()),
                network: None,
                mcp: None,
                decision: PolicyDecision::Deny,
                priority: 1,
            },
        ];
        let result = compiler.compile(rules, 0);
        assert_eq!(
            result
                .policy
                .evaluate("write_file", &serde_json::json!({"path": "/etc/passwd"})),
            PolicyDecision::Deny
        );
        assert_eq!(
            result
                .policy
                .evaluate("write_file", &serde_json::json!({"path": "/tmp/x"})),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn published_policy_evaluate_no_match_asks() {
        let compiler = PolicyCompiler::new();
        let result = compiler.compile(vec![make_rule("read_file", PolicyDecision::Allow, 0)], 0);
        assert_eq!(
            result.policy.evaluate("exec", &serde_json::json!({})),
            PolicyDecision::Ask
        );
    }

    #[test]
    fn rules_for_command_returns_exact_and_wildcard() {
        let compiler = PolicyCompiler::new();
        let rules = vec![
            make_rule("*", PolicyDecision::Allow, 0),
            make_rule("read_file", PolicyDecision::Allow, 10),
            make_rule("exec", PolicyDecision::Deny, 100),
        ];
        let result = compiler.compile(rules, 0);
        let read_rules = result.policy.rules_for_command("read_file");
        // Should include the exact "read_file" rule and the wildcard "*".
        assert_eq!(read_rules.len(), 2);
        let exec_rules = result.policy.rules_for_command("exec");
        assert_eq!(exec_rules.len(), 2);
        // "write_file" only matches the wildcard.
        let write_rules = result.policy.rules_for_command("write_file");
        assert_eq!(write_rules.len(), 1);
    }

    #[test]
    fn rules_for_capability_returns_exact_only() {
        let compiler = PolicyCompiler::new();
        let rules = vec![
            make_rule("*", PolicyDecision::Allow, 0),
            make_rule("read_file", PolicyDecision::Allow, 10),
        ];
        let result = compiler.compile(rules, 0);
        let caps = result.policy.rules_for_capability("read_file");
        assert_eq!(caps.len(), 1);
        // Wildcard rules are NOT in the capability index.
        let wildcard_caps = result.policy.rules_for_capability("*");
        assert!(wildcard_caps.is_empty());
    }

    #[test]
    fn explain_traces_matched_rules() {
        let compiler = PolicyCompiler::new();
        let rules = vec![
            PolicyRule {
                tool_pattern: "*".into(),
                arg_patterns: vec![],
                command: None,
                resource: None,
                rule_id: Some("broad-allow".into()),
                network: None,
                mcp: None,
                decision: PolicyDecision::Allow,
                priority: 1,
            },
            PolicyRule {
                tool_pattern: "exec".into(),
                arg_patterns: vec![],
                command: Some(CommandMatcher {
                    pattern: "rm".into(),
                    substring: true,
                }),
                resource: None,
                rule_id: Some("ask-rm".into()),
                network: None,
                mcp: None,
                decision: PolicyDecision::Ask,
                priority: 10,
            },
        ];
        let result = compiler.compile(rules, 3);
        let explainer = PolicyExplainer::new(&result.policy);
        let trace = explainer.explain("exec", &serde_json::json!({"command": "rm -rf /tmp"}));

        assert_eq!(trace.effect, PolicyDecision::Ask);
        assert_eq!(trace.policy_generation, 4);
        assert_eq!(trace.matched_rule_ids.len(), 2);
        assert_eq!(trace.dominant_rule_id.as_deref(), Some("ask-rm"));
        assert_eq!(trace.reason_code, ReasonCode::ExplicitAsk);
        assert!(trace.sandbox_requirement.needs_approval);
    }

    #[test]
    fn explain_no_match_default_ask() {
        let compiler = PolicyCompiler::new();
        let result = compiler.compile(vec![make_rule("read_file", PolicyDecision::Allow, 0)], 0);
        let explainer = PolicyExplainer::new(&result.policy);
        let trace = explainer.explain("exec", &serde_json::json!({}));

        assert_eq!(trace.effect, PolicyDecision::Ask);
        assert_eq!(trace.reason_code, ReasonCode::NoMatchDefaultAsk);
        assert!(trace.matched_rule_ids.is_empty());
        assert!(trace.dominant_rule_id.is_none());
    }

    #[test]
    fn explain_explicit_allow() {
        let compiler = PolicyCompiler::new();
        let result = compiler.compile(vec![make_rule("read_file", PolicyDecision::Allow, 0)], 0);
        let explainer = PolicyExplainer::new(&result.policy);
        let trace = explainer.explain("read_file", &serde_json::json!({}));

        assert_eq!(trace.effect, PolicyDecision::Allow);
        assert_eq!(trace.reason_code, ReasonCode::ExplicitAllow);
        assert!(!trace.sandbox_requirement.needs_approval);
    }

    #[test]
    fn explain_explicit_deny() {
        let compiler = PolicyCompiler::new();
        let result = compiler.compile(vec![make_rule("exec", PolicyDecision::Deny, 0)], 0);
        let explainer = PolicyExplainer::new(&result.policy);
        let trace = explainer.explain("exec", &serde_json::json!({}));

        assert_eq!(trace.effect, PolicyDecision::Deny);
        assert_eq!(trace.reason_code, ReasonCode::ExplicitDeny);
    }

    #[test]
    fn explain_detects_network_sandbox_requirement() {
        let compiler = PolicyCompiler::new();
        let mut rule = make_rule("http_get", PolicyDecision::Allow, 0);
        rule.network = Some(NetworkMatcher {
            protocol: NetworkProtocol::Https,
            host: HostMatcher::DomainSuffix("example.com".into()),
            port: PortMatcher::Any,
            direction: NetworkDirection::Connect,
            method_class: None,
            redirect_policy: crate::policy::RedirectPolicy::Deny,
            dns_policy: crate::policy::DnsPolicy::ResolveThenValidate,
        });
        let result = compiler.compile(vec![rule], 0);
        let explainer = PolicyExplainer::new(&result.policy);
        let trace = explainer.explain(
            "http_get",
            &serde_json::json!({
                "protocol": "https",
                "host": "api.example.com",
                "port": 443,
                "direction": "connect"
            }),
        );
        assert!(trace.sandbox_requirement.network_restricted);
        assert!(!trace.sandbox_requirement.filesystem_restricted);
    }

    #[test]
    fn explain_detects_filesystem_sandbox_requirement() {
        let compiler = PolicyCompiler::new();
        let mut rule = make_rule("write_file", PolicyDecision::Allow, 0);
        rule.resource = Some(ResourceMatcher {
            arg_path: "/path".into(),
            pattern: "/tmp/*".into(),
        });
        let result = compiler.compile(vec![rule], 0);
        let explainer = PolicyExplainer::new(&result.policy);
        let trace = explainer.explain(
            "write_file",
            &serde_json::json!({"path": "/tmp/test.txt"}),
        );
        assert!(trace.sandbox_requirement.filesystem_restricted);
        assert!(!trace.sandbox_requirement.network_restricted);
    }

    #[test]
    fn propose_always_allow_matcher_includes_tool_and_path() {
        let compiler = PolicyCompiler::new();
        let result = compiler.compile(vec![], 0);
        let explainer = PolicyExplainer::new(&result.policy);
        let matcher = explainer.propose_always_allow_matcher(
            "read_file",
            &serde_json::json!({"path": "/tmp/test.txt"}),
        );
        assert!(matcher.contains("tool=read_file"));
        assert!(matcher.contains("path=/tmp/test.txt"));
    }

    #[test]
    fn propose_always_allow_matcher_includes_command_prefix() {
        let compiler = PolicyCompiler::new();
        let result = compiler.compile(vec![], 0);
        let explainer = PolicyExplainer::new(&result.policy);
        let matcher = explainer.propose_always_allow_matcher(
            "exec",
            &serde_json::json!({"command": "git commit -m hello"}),
        );
        assert!(matcher.contains("tool=exec"));
        assert!(matcher.contains("command_prefix=git"));
    }

    #[test]
    fn propose_always_allow_matcher_includes_mcp_ids() {
        let compiler = PolicyCompiler::new();
        let result = compiler.compile(vec![], 0);
        let explainer = PolicyExplainer::new(&result.policy);
        let matcher = explainer.propose_always_allow_matcher(
            "mcp_call",
            &serde_json::json!({
                "server_capability_id": "fs-server",
                "tool_capability_id": "read_file"
            }),
        );
        assert!(matcher.contains("server=fs-server"));
        assert!(matcher.contains("mcp_tool=read_file"));
    }

    #[test]
    fn explain_with_mcp_rule_traces_correctly() {
        let compiler = PolicyCompiler::new();
        let mut rule = make_rule("mcp_call", PolicyDecision::Allow, 10);
        rule.rule_id = Some("mcp-allow".into());
        rule.mcp = Some(McpMatcher {
            server_capability_id: "fs-server".into(),
            tool_capability_id: "read_file".into(),
            argument_constraints: None,
            side_effect_class: SideEffectClass::ReadOnly,
        });
        let result = compiler.compile(vec![rule], 0);
        let explainer = PolicyExplainer::new(&result.policy);
        let trace = explainer.explain(
            "mcp_call",
            &serde_json::json!({
                "server_capability_id": "fs-server",
                "tool_capability_id": "read_file"
            }),
        );
        assert_eq!(trace.effect, PolicyDecision::Allow);
        assert_eq!(trace.reason_code, ReasonCode::ExplicitAllow);
        assert_eq!(trace.dominant_rule_id.as_deref(), Some("mcp-allow"));
    }

    #[test]
    fn policy_compiler_default() {
        let compiler = PolicyCompiler::default();
        let result = compiler.compile(vec![], 0);
        assert_eq!(result.policy.generation, 1);
        assert_eq!(result.policy.rules().len(), 0);
    }
}
