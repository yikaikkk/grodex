//! PermissionPolicy — static permission rules engine.
//!
//! Evaluates tool calls against configured rules to produce
//! Allow / Ask / Deny decisions before any user interaction.
//!
//! Merge semantics: when multiple rules match a call, the result is the
//! STRICTEST (ceiling) decision of all matches, not the first match.
//! `Deny > Ask > Allow`. This is the security-correct direction — a narrow
//! deny rule (e.g. "deny writes under /etc") must override a broad allow
//! ("allow writes") even if the allow was added later or has higher
//! priority. `priority` only breaks *ties* between equally-strict rules
//! (it no longer silently picks a more permissive winner).

use grodex_core::policy::PolicyDecision;
use serde::{Deserialize, Serialize};

// ── Network matching (doc 10 §20.8) ────────────────────────────────

/// Match network operations (doc 10 §20.8). Policy compiler compiles
/// network rules into NetworkLease upper bounds for the external sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkMatcher {
    pub protocol: NetworkProtocol,
    /// Exact domain or domain suffix (e.g. ".example.com").
    pub host: HostMatcher,
    pub port: PortMatcher,
    pub direction: NetworkDirection,
    /// HTTP method class restriction (only for http/https).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method_class: Option<MethodClass>,
    pub redirect_policy: RedirectPolicy,
    pub dns_policy: DnsPolicy,
}

impl NetworkMatcher {
    /// Whether this matcher matches the given network operation.
    pub fn matches(
        &self,
        protocol: NetworkProtocol,
        host: &str,
        port: u16,
        direction: NetworkDirection,
    ) -> bool {
        if self.protocol != protocol {
            return false;
        }
        if self.direction != direction {
            return false;
        }
        if !self.host.matches(host) {
            return false;
        }
        if !self.port.matches(port) {
            return false;
        }
        true
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkProtocol {
    Http,
    Https,
    Tcp,
    Udp,
    Unix,
}

/// Parse a protocol string into `NetworkProtocol`.
fn parse_protocol(s: &str) -> Option<NetworkProtocol> {
    match s.to_lowercase().as_str() {
        "http" => Some(NetworkProtocol::Http),
        "https" => Some(NetworkProtocol::Https),
        "tcp" => Some(NetworkProtocol::Tcp),
        "udp" => Some(NetworkProtocol::Udp),
        "unix" => Some(NetworkProtocol::Unix),
        _ => None,
    }
}

/// Match a hostname: exact or domain suffix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HostMatcher {
    /// Exact hostname match.
    Exact(String),
    /// Domain suffix match (e.g. "example.com" matches "api.example.com").
    DomainSuffix(String),
}

impl HostMatcher {
    pub fn matches(&self, host: &str) -> bool {
        match self {
            HostMatcher::Exact(h) => h == host,
            HostMatcher::DomainSuffix(suffix) => {
                host == suffix.as_str() || host.ends_with(&format!(".{}", suffix))
            }
        }
    }
}

/// Match a port: any, exact, set, or range.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PortMatcher {
    Any,
    Exact(u16),
    Set(Vec<u16>),
    Range { start: u16, end: u16 },
}

impl PortMatcher {
    pub fn matches(&self, port: u16) -> bool {
        match self {
            PortMatcher::Any => true,
            PortMatcher::Exact(p) => *p == port,
            PortMatcher::Set(ports) => ports.contains(&port),
            PortMatcher::Range { start, end } => port >= *start && port <= *end,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkDirection {
    Connect,
    Listen,
}

/// Parse a direction string into `NetworkDirection`.
fn parse_direction(s: &str) -> Option<NetworkDirection> {
    match s.to_lowercase().as_str() {
        "connect" => Some(NetworkDirection::Connect),
        "listen" => Some(NetworkDirection::Listen),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MethodClass {
    Get,
    Post,
    Put,
    Delete,
    Patch,
    Safe,
    Unsafe,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RedirectPolicy {
    /// No redirect allowed.
    Deny,
    /// Redirect allowed only within same domain.
    SameDomain,
    /// Redirect allowed to any approved host.
    AnyApproved,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DnsPolicy {
    /// Resolve and validate against policy before connecting.
    ResolveThenValidate,
    /// Use pre-resolved IP only (no DNS).
    PreResolvedOnly,
}

// ── MCP matching (doc 10 §20.9) ────────────────────────────────────

/// Match MCP tool calls (doc 10 §20.9). External MCP description
/// self-claiming read-only does NOT constitute a permission fact.
/// `side_effect_class` comes from trusted config/user management.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpMatcher {
    /// Stable capability id of the MCP server.
    pub server_capability_id: String,
    /// Stable capability id of the specific MCP tool.
    pub tool_capability_id: String,
    /// Optional argument constraints (V1: exact/set/path/host only,
    /// no arbitrary JSONPath).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_constraints: Option<McpArgumentConstraints>,
    /// Side effect classification from trusted source. Unknown → Ask.
    pub side_effect_class: SideEffectClass,
}

/// V1 argument constraints for MCP tool calls.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpArgumentConstraints {
    #[serde(default)]
    pub exact: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub set: std::collections::HashMap<String, Vec<String>>,
    #[serde(default)]
    pub path: Vec<String>,
    #[serde(default)]
    pub host: Vec<String>,
}

/// Side-effect classification of an MCP tool (doc 10 §20.9).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectClass {
    ReadOnly,
    LocalState,
    NetworkOutbound,
    Destructive,
    Unknown,
}

// ── Policy rule ────────────────────────────────────────────────────

/// A single permission rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRule {
    /// Pattern to match against tool names (supports `*` wildcard).
    pub tool_pattern: String,
    /// Arg path patterns (e.g. `path` for file paths, `command` for exec).
    /// If empty, matches all arguments.
    pub arg_patterns: Vec<ArgPattern>,
    /// Optional command matcher for `exec`/`bash`-style tools: restricts
    /// the rule to calls whose argv matches. None = matches any command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<CommandMatcher>,
    /// Optional resource matcher: restricts the rule to calls whose
    /// resource (file path / host / etc.) matches. None = any resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceMatcher>,
    /// Optional stable rule identifier for tracing and explain (doc 10 §20.14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    /// Optional network matcher: restricts the rule to calls whose network
    /// operation matches (doc 10 §20.8). None = no network constraint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkMatcher>,
    /// Optional MCP tool matcher: restricts the rule to specific MCP
    /// server/tool capability ids (doc 10 §20.9). None = not MCP-specific.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpMatcher>,
    /// The decision when this rule matches.
    pub decision: PolicyDecision,
    /// Higher priority breaks ties between equally-strict matching rules.
    pub priority: u8,
}

impl PolicyRule {
    /// Whether this rule matches the given tool call.
    ///
    /// Checks tool name, arg patterns, command matcher, resource matcher,
    /// network matcher, and MCP matcher. ALL must match for the rule to
    /// match (AND semantics).
    pub fn matches(&self, tool_name: &str, args: &serde_json::Value) -> bool {
        // Tool name match (with wildcard support).
        if !matches_pattern(tool_name, &self.tool_pattern) {
            return false;
        }

        // Arg patterns: ALL must match.
        for ap in &self.arg_patterns {
            if let Some(value) = args.pointer(&ap.arg_path) {
                let value_str = match value {
                    serde_json::Value::String(s) => s.as_str(),
                    other => &other.to_string(),
                };
                if !matches_pattern(value_str, &ap.pattern) {
                    return false;
                }
            } else {
                // Arg path not present — rule doesn't match.
                return false;
            }
        }

        // Command matcher for exec-like tools.
        if let Some(ref cm) = self.command {
            let Some(cmd) = args.pointer("/command").and_then(|v| v.as_str()) else {
                return false;
            };
            if cm.substring {
                if !cmd.contains(&cm.pattern) {
                    return false;
                }
            } else if !matches_pattern(cmd, &cm.pattern) {
                return false;
            }
        }

        // Resource matcher.
        if let Some(ref rm) = self.resource {
            let Some(value) = args.pointer(&rm.arg_path) else {
                return false;
            };
            let value_str = match value {
                serde_json::Value::String(s) => s.as_str(),
                other => &other.to_string(),
            };
            if !matches_pattern(value_str, &rm.pattern) {
                return false;
            }
        }

        // Network matcher (doc 10 §20.8).
        if let Some(ref nm) = self.network {
            let host = args.pointer("/host").and_then(|v| v.as_str()).unwrap_or("");
            let port = args
                .pointer("/port")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u16;
            let protocol = args
                .pointer("/protocol")
                .and_then(|v| v.as_str())
                .and_then(parse_protocol);
            let direction = args
                .pointer("/direction")
                .and_then(|v| v.as_str())
                .and_then(parse_direction);
            match (protocol, direction) {
                (Some(p), Some(d)) => {
                    if !nm.matches(p, host, port, d) {
                        return false;
                    }
                }
                _ => return false,
            }
        }

        // MCP matcher (doc 10 §20.9).
        if let Some(ref mm) = self.mcp {
            let server_cap = args
                .pointer("/server_capability_id")
                .and_then(|v| v.as_str());
            let tool_cap = args
                .pointer("/tool_capability_id")
                .and_then(|v| v.as_str());
            match (server_cap, tool_cap) {
                (Some(s), Some(t))
                    if s == mm.server_capability_id && t == mm.tool_capability_id => {}
                _ => return false,
            }
        }

        true
    }
}

/// Match the command string of an `exec`-like tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandMatcher {
    /// Glob over the full command string (e.g. `rm *`, `git *`).
    pub pattern: String,
    /// If true, also match when the pattern is a substring (useful for
    /// forbidding `sudo` / `rm -rf` anywhere in the command).
    #[serde(default)]
    pub substring: bool,
}

/// Match a resource (file path, URL host, etc.) referenced by the call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceMatcher {
    /// JSON pointer to the resource value (e.g. `/path`, `/host`).
    pub arg_path: String,
    /// Glob pattern for the resource value (e.g. `/etc/*`, `*.internal`).
    pub pattern: String,
}

/// A pattern for matching specific argument values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArgPattern {
    /// JSON pointer path to the argument (e.g. `/path`, `/command`).
    pub arg_path: String,
    /// Glob pattern for the value.
    pub pattern: String,
}

/// Session-scoped permission policy.
///
/// Evaluation collects EVERY matching rule and returns the strictest
/// decision among them (Deny > Ask > Allow); `priority` only breaks ties.
/// If no rule matches, returns `Ask` (conservative default).
#[derive(Debug, Clone)]
pub struct PermissionPolicy {
    rules: Vec<PolicyRule>,
}

impl PermissionPolicy {
    /// Create an empty policy (Ask for everything).
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Create a permissive policy (Allow everything).
    pub fn permissive() -> Self {
        Self {
            rules: vec![PolicyRule {
                tool_pattern: "*".into(),
                arg_patterns: vec![],
                command: None,
                resource: None,
                rule_id: None,
                network: None,
                mcp: None,
                decision: PolicyDecision::Allow,
                priority: 0,
            }],
        }
    }

    /// Create a strict deny-by-default policy.
    pub fn default_deny() -> Self {
        Self {
            rules: vec![PolicyRule {
                tool_pattern: "*".into(),
                arg_patterns: vec![],
                command: None,
                resource: None,
                rule_id: None,
                network: None,
                mcp: None,
                decision: PolicyDecision::Deny,
                priority: 0,
            }],
        }
    }

    /// Add a rule. Order of insertion is irrelevant — evaluate() merges
    /// all matches by strictness.
    pub fn add_rule(&mut self, rule: PolicyRule) {
        self.rules.push(rule);
    }

    /// Evaluate whether a tool call is allowed.
    ///
    /// Returns the STRICTEST decision among all matching rules
    /// (Deny > Ask > Allow); `priority` breaks ties. `Ask` if none match.
    pub fn evaluate(&self, tool_name: &str, args: &serde_json::Value) -> PolicyDecision {
        let mut matched = false;
        let mut winner = PolicyDecision::Allow; // overwritten once something matches
        let mut winner_strictness = u8::MAX; // strictness: Deny=2, Ask=1, Allow=0
        let mut winner_priority = 0u8;

        for rule in &self.rules {
            if rule.matches(tool_name, args) {
                let strictness = strictness_of(rule.decision);
                // Take this rule if it is stricter, OR equal-strictness with
                // higher priority.
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
            PolicyDecision::Ask // conservative default
        }
    }
}

/// Glob pattern match: `*` matches anything, `prefix*` matches prefix,
/// otherwise exact match.
fn matches_pattern(value: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if pattern.contains('*') {
        let prefix = pattern.strip_suffix('*').unwrap_or(pattern);
        value.starts_with(prefix)
    } else {
        value == pattern
    }
}

/// Strictness ranking used by strictest-merge: Deny(2) > Ask(1) > Allow(0).
pub(crate) fn strictness_of(d: PolicyDecision) -> u8 {
    match d {
        PolicyDecision::Allow => 0,
        PolicyDecision::Ask => 1,
        PolicyDecision::Deny => 2,
    }
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissive_allows_all() {
        let policy = PermissionPolicy::permissive();
        assert_eq!(
            policy.evaluate("read_file", &serde_json::json!({})),
            PolicyDecision::Allow
        );
        assert_eq!(
            policy.evaluate("exec", &serde_json::json!({"command": "rm -rf /"})),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn default_deny_blocks_all() {
        let policy = PermissionPolicy::default_deny();
        assert_eq!(
            policy.evaluate("read_file", &serde_json::json!({})),
            PolicyDecision::Deny
        );
    }

    #[test]
    fn empty_policy_asks() {
        let policy = PermissionPolicy::new();
        assert_eq!(policy.evaluate("anything", &serde_json::json!({})), PolicyDecision::Ask);
    }

    #[test]
    fn priority_order() {
        let mut policy = PermissionPolicy::new();
        // Low priority: allow all
        policy.add_rule(PolicyRule {
            tool_pattern: "*".into(),
            arg_patterns: vec![],
            command: None,
            resource: None,
            rule_id: None,
            network: None,
            mcp: None,
            decision: PolicyDecision::Allow,
            priority: 10,
        });
        // High priority: deny exec
        policy.add_rule(PolicyRule {
            tool_pattern: "exec".into(),
            arg_patterns: vec![],
            command: None,
            resource: None,
            rule_id: None,
            network: None,
            mcp: None,
            decision: PolicyDecision::Deny,
            priority: 100,
        });

        assert_eq!(
            policy.evaluate("read_file", &serde_json::json!({})),
            PolicyDecision::Allow
        );
        assert_eq!(policy.evaluate("exec", &serde_json::json!({})), PolicyDecision::Deny);
    }

    #[test]
    fn arg_pattern_matching() {
        let mut policy = PermissionPolicy::new();
        policy.add_rule(PolicyRule {
            tool_pattern: "read_file".into(),
            arg_patterns: vec![ArgPattern {
                arg_path: "/path".into(),
                pattern: "/tmp/*".into(),
            }],
            command: None,
            resource: None,
            rule_id: None,
            network: None,
            mcp: None,
            decision: PolicyDecision::Allow,
            priority: 10,
        });

        assert_eq!(
            policy.evaluate("read_file", &serde_json::json!({"path": "/tmp/test.txt"})),
            PolicyDecision::Allow
        );
        assert_eq!(
            policy.evaluate("read_file", &serde_json::json!({"path": "/etc/passwd"})),
            PolicyDecision::Ask
        );
    }

    /// The security-critical case the audit flagged: a narrow deny must
    /// override a broad allow EVEN when the allow has higher priority —
    /// strictest-merge, not first-match.
    #[test]
    fn strictest_merge_narrow_deny_beats_broad_allow() {
        let mut policy = PermissionPolicy::new();
        // Broad allow-all with very high priority.
        policy.add_rule(PolicyRule {
            tool_pattern: "*".into(),
            arg_patterns: vec![],
            command: None,
            resource: None,
            rule_id: None,
            network: None,
            mcp: None,
            decision: PolicyDecision::Allow,
            priority: 255,
        });
        // Narrow deny for writes under /etc, LOW priority.
        policy.add_rule(PolicyRule {
            tool_pattern: "write_file".into(),
            arg_patterns: vec![],
            command: None,
            resource: Some(ResourceMatcher {
                arg_path: "/path".into(),
                pattern: "/etc/*".into(),
            }),
            rule_id: None,
            network: None,
            mcp: None,
            decision: PolicyDecision::Deny,
            priority: 1,
        });

        assert_eq!(
            policy.evaluate("write_file", &serde_json::json!({"path": "/etc/passwd"})),
            PolicyDecision::Deny,
            "narrow deny must override broad allow even with lower priority"
        );
        // A write outside /etc still allowed (only the broad allow matches).
        assert_eq!(
            policy.evaluate("write_file", &serde_json::json!({"path": "/tmp/x"})),
            PolicyDecision::Allow
        );
    }

    /// Ask is stricter than Allow: a matching Ask rule upgrades an Allow.
    #[test]
    fn ask_upgrades_allow_under_strictest_merge() {
        let mut policy = PermissionPolicy::new();
        policy.add_rule(PolicyRule {
            tool_pattern: "exec".into(),
            arg_patterns: vec![],
            command: None,
            resource: None,
            rule_id: None,
            network: None,
            mcp: None,
            decision: PolicyDecision::Allow,
            priority: 50,
        });
        policy.add_rule(PolicyRule {
            tool_pattern: "exec".into(),
            arg_patterns: vec![],
            command: Some(CommandMatcher { pattern: "rm".into(), substring: true }),
            resource: None,
            rule_id: None,
            network: None,
            mcp: None,
            decision: PolicyDecision::Ask,
            priority: 1,
        });

        assert_eq!(
            policy.evaluate("exec", &serde_json::json!({"command": "rm -rf /tmp/x"})),
            PolicyDecision::Ask,
            "matching Ask must upgrade a matching Allow to Ask"
        );
        // Non-rm exec still allowed.
        assert_eq!(
            policy.evaluate("exec", &serde_json::json!({"command": "ls"})),
            PolicyDecision::Allow
        );
    }

    // ── NetworkMatcher tests ───────────────────────────────────────

    #[test]
    fn network_exact_host_match() {
        let nm = NetworkMatcher {
            protocol: NetworkProtocol::Https,
            host: HostMatcher::Exact("api.example.com".into()),
            port: PortMatcher::Exact(443),
            direction: NetworkDirection::Connect,
            method_class: None,
            redirect_policy: RedirectPolicy::Deny,
            dns_policy: DnsPolicy::ResolveThenValidate,
        };
        assert!(nm.matches(
            NetworkProtocol::Https,
            "api.example.com",
            443,
            NetworkDirection::Connect
        ));
        assert!(!nm.matches(
            NetworkProtocol::Https,
            "other.example.com",
            443,
            NetworkDirection::Connect
        ));
    }

    #[test]
    fn network_domain_suffix_match() {
        let nm = NetworkMatcher {
            protocol: NetworkProtocol::Https,
            host: HostMatcher::DomainSuffix("example.com".into()),
            port: PortMatcher::Any,
            direction: NetworkDirection::Connect,
            method_class: None,
            redirect_policy: RedirectPolicy::SameDomain,
            dns_policy: DnsPolicy::ResolveThenValidate,
        };
        // Direct match on the suffix itself.
        assert!(nm.matches(
            NetworkProtocol::Https,
            "example.com",
            443,
            NetworkDirection::Connect
        ));
        // Subdomain matches.
        assert!(nm.matches(
            NetworkProtocol::Https,
            "api.example.com",
            8443,
            NetworkDirection::Connect
        ));
        // Unrelated domain does not match.
        assert!(!nm.matches(
            NetworkProtocol::Https,
            "notexample.com",
            443,
            NetworkDirection::Connect
        ));
    }

    #[test]
    fn network_port_range_and_set() {
        let nm_range = NetworkMatcher {
            protocol: NetworkProtocol::Tcp,
            host: HostMatcher::Exact("db.local".into()),
            port: PortMatcher::Range { start: 5430, end: 5440 },
            direction: NetworkDirection::Connect,
            method_class: None,
            redirect_policy: RedirectPolicy::Deny,
            dns_policy: DnsPolicy::PreResolvedOnly,
        };
        assert!(nm_range.matches(
            NetworkProtocol::Tcp,
            "db.local",
            5432,
            NetworkDirection::Connect
        ));
        assert!(!nm_range.matches(
            NetworkProtocol::Tcp,
            "db.local",
            5450,
            NetworkDirection::Connect
        ));

        let nm_set = NetworkMatcher {
            protocol: NetworkProtocol::Tcp,
            host: HostMatcher::Exact("db.local".into()),
            port: PortMatcher::Set(vec![80, 443, 8080]),
            direction: NetworkDirection::Connect,
            method_class: None,
            redirect_policy: RedirectPolicy::Deny,
            dns_policy: DnsPolicy::PreResolvedOnly,
        };
        assert!(nm_set.matches(
            NetworkProtocol::Tcp,
            "db.local",
            443,
            NetworkDirection::Connect
        ));
        assert!(!nm_set.matches(
            NetworkProtocol::Tcp,
            "db.local",
            22,
            NetworkDirection::Connect
        ));
    }

    #[test]
    fn network_protocol_and_direction_mismatch() {
        let nm = NetworkMatcher {
            protocol: NetworkProtocol::Https,
            host: HostMatcher::Exact("x.com".into()),
            port: PortMatcher::Any,
            direction: NetworkDirection::Connect,
            method_class: None,
            redirect_policy: RedirectPolicy::Deny,
            dns_policy: DnsPolicy::ResolveThenValidate,
        };
        // Wrong protocol.
        assert!(!nm.matches(
            NetworkProtocol::Tcp,
            "x.com",
            443,
            NetworkDirection::Connect
        ));
        // Wrong direction.
        assert!(!nm.matches(
            NetworkProtocol::Https,
            "x.com",
            443,
            NetworkDirection::Listen
        ));
    }

    #[test]
    fn network_rule_in_policy() {
        let mut policy = PermissionPolicy::new();
        policy.add_rule(PolicyRule {
            tool_pattern: "http_get".into(),
            arg_patterns: vec![],
            command: None,
            resource: None,
            rule_id: Some("net-allow-api".into()),
            network: Some(NetworkMatcher {
                protocol: NetworkProtocol::Https,
                host: HostMatcher::DomainSuffix("api.example.com".into()),
                port: PortMatcher::Any,
                direction: NetworkDirection::Connect,
                method_class: Some(MethodClass::Get),
                redirect_policy: RedirectPolicy::SameDomain,
                dns_policy: DnsPolicy::ResolveThenValidate,
            }),
            mcp: None,
            decision: PolicyDecision::Allow,
            priority: 10,
        });

        // Matching network call → Allow.
        assert_eq!(
            policy.evaluate(
                "http_get",
                &serde_json::json!({
                    "protocol": "https",
                    "host": "v2.api.example.com",
                    "port": 443,
                    "direction": "connect"
                })
            ),
            PolicyDecision::Allow
        );
        // Non-matching host → Ask (no rule matches).
        assert_eq!(
            policy.evaluate(
                "http_get",
                &serde_json::json!({
                    "protocol": "https",
                    "host": "evil.com",
                    "port": 443,
                    "direction": "connect"
                })
            ),
            PolicyDecision::Ask
        );
    }

    // ── McpMatcher tests ───────────────────────────────────────────

    #[test]
    fn mcp_rule_matches_correct_capability_ids() {
        let mut policy = PermissionPolicy::new();
        policy.add_rule(PolicyRule {
            tool_pattern: "mcp_call".into(),
            arg_patterns: vec![],
            command: None,
            resource: None,
            rule_id: Some("mcp-allow-readonly".into()),
            network: None,
            mcp: Some(McpMatcher {
                server_capability_id: "fs-server".into(),
                tool_capability_id: "read_file".into(),
                argument_constraints: None,
                side_effect_class: SideEffectClass::ReadOnly,
            }),
            decision: PolicyDecision::Allow,
            priority: 10,
        });

        assert_eq!(
            policy.evaluate(
                "mcp_call",
                &serde_json::json!({
                    "server_capability_id": "fs-server",
                    "tool_capability_id": "read_file"
                })
            ),
            PolicyDecision::Allow
        );
        // Wrong tool_capability_id → no match → Ask.
        assert_eq!(
            policy.evaluate(
                "mcp_call",
                &serde_json::json!({
                    "server_capability_id": "fs-server",
                    "tool_capability_id": "delete_file"
                })
            ),
            PolicyDecision::Ask
        );
        // Wrong server_capability_id → no match → Ask.
        assert_eq!(
            policy.evaluate(
                "mcp_call",
                &serde_json::json!({
                    "server_capability_id": "other-server",
                    "tool_capability_id": "read_file"
                })
            ),
            PolicyDecision::Ask
        );
    }

    #[test]
    fn mcp_argument_constraints_default() {
        let ac = McpArgumentConstraints::default();
        assert!(ac.exact.is_empty());
        assert!(ac.set.is_empty());
        assert!(ac.path.is_empty());
        assert!(ac.host.is_empty());
    }

    #[test]
    fn mcp_side_effect_unknown_upgrades_to_ask() {
        let mut policy = PermissionPolicy::new();
        // Broad allow.
        policy.add_rule(PolicyRule {
            tool_pattern: "*".into(),
            arg_patterns: vec![],
            command: None,
            resource: None,
            rule_id: None,
            network: None,
            mcp: None,
            decision: PolicyDecision::Allow,
            priority: 1,
        });
        // MCP tool with Unknown side effect → Ask.
        policy.add_rule(PolicyRule {
            tool_pattern: "mcp_call".into(),
            arg_patterns: vec![],
            command: None,
            resource: None,
            rule_id: Some("mcp-unknown".into()),
            network: None,
            mcp: Some(McpMatcher {
                server_capability_id: "exec-server".into(),
                tool_capability_id: "run".into(),
                argument_constraints: None,
                side_effect_class: SideEffectClass::Unknown,
            }),
            decision: PolicyDecision::Ask,
            priority: 10,
        });

        assert_eq!(
            policy.evaluate(
                "mcp_call",
                &serde_json::json!({
                    "server_capability_id": "exec-server",
                    "tool_capability_id": "run"
                })
            ),
            PolicyDecision::Ask,
            "Unknown side-effect MCP tool must upgrade to Ask"
        );
    }
}
