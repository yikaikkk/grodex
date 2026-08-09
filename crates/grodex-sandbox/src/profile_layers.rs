//! 7-Layer Sandbox Profile Intersection + Access Level Matrix (Doc 13 §7).

use grodex_sandbox_types::profile::{NetworkRule, SandboxProfile};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Doc 13 §7: 严格性从高到低的 7 层顺序（越靠前越能收紧）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ProfileLayer {
    PolicyCeiling = 0,
    UserBinding = 1,
    Capability = 2,
    Tool = 3,
    Supervisor = 4,
    OS = 5,
    Default = 6,
}

impl ProfileLayer {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProfileLayer::PolicyCeiling => "PolicyCeiling",
            ProfileLayer::UserBinding => "UserBinding",
            ProfileLayer::Capability => "Capability",
            ProfileLayer::Tool => "Tool",
            ProfileLayer::Supervisor => "Supervisor",
            ProfileLayer::OS => "OS",
            ProfileLayer::Default => "Default",
        }
    }
}

/// 7 层合并输入：每一层是一个 Option<SandboxProfile>（None 表示该层用户未绑定，不参与求交）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LayeredProfileInput {
    pub policy_ceiling: Option<SandboxProfile>,
    pub user_binding: Option<SandboxProfile>,
    pub capability: Option<SandboxProfile>,
    pub tool: Option<SandboxProfile>,
    pub supervisor: Option<SandboxProfile>,
    pub os_default: Option<SandboxProfile>,
    pub default: Option<SandboxProfile>,
}

impl LayeredProfileInput {
    /// 按 ProfileLayer 顺序（从最严格到最宽松）返回 (layer, profile_ref) 迭代。
    pub fn ordered_layers(&self) -> impl Iterator<Item = (ProfileLayer, Option<&SandboxProfile>)> {
        [
            (ProfileLayer::PolicyCeiling, self.policy_ceiling.as_ref()),
            (ProfileLayer::UserBinding, self.user_binding.as_ref()),
            (ProfileLayer::Capability, self.capability.as_ref()),
            (ProfileLayer::Tool, self.tool.as_ref()),
            (ProfileLayer::Supervisor, self.supervisor.as_ref()),
            (ProfileLayer::OS, self.os_default.as_ref()),
            (ProfileLayer::Default, self.default.as_ref()),
        ]
        .into_iter()
    }

    /// 按层取 profile 的可变引用（用于 AccessLevel 过滤时置 None）。
    pub fn get_layer_mut(&mut self, layer: ProfileLayer) -> &mut Option<SandboxProfile> {
        match layer {
            ProfileLayer::PolicyCeiling => &mut self.policy_ceiling,
            ProfileLayer::UserBinding => &mut self.user_binding,
            ProfileLayer::Capability => &mut self.capability,
            ProfileLayer::Tool => &mut self.tool,
            ProfileLayer::Supervisor => &mut self.supervisor,
            ProfileLayer::OS => &mut self.os_default,
            ProfileLayer::Default => &mut self.default,
        }
    }

    pub fn get_layer_ref(&self, layer: ProfileLayer) -> Option<&SandboxProfile> {
        match layer {
            ProfileLayer::PolicyCeiling => self.policy_ceiling.as_ref(),
            ProfileLayer::UserBinding => self.user_binding.as_ref(),
            ProfileLayer::Capability => self.capability.as_ref(),
            ProfileLayer::Tool => self.tool.as_ref(),
            ProfileLayer::Supervisor => self.supervisor.as_ref(),
            ProfileLayer::OS => self.os_default.as_ref(),
            ProfileLayer::Default => self.default.as_ref(),
        }
    }
}

/// 7 路求交结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntersectionResult {
    pub effective: SandboxProfile,
    pub contributing_layers: Vec<ProfileLayer>,
    pub diagnostics: Vec<String>,
}

/// Doc 13 §7.2: 接入等级（控制哪些 binding 允许被非管理员写入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessLevel {
    Level0,
    Level1,
    Level2,
}

impl AccessLevel {
    /// 给定某层 + 当前 access level，判断该层是否允许"参与求交"。
    pub fn allows_layer(&self, layer: ProfileLayer) -> bool {
        match self {
            AccessLevel::Level0 => matches!(
                layer,
                ProfileLayer::PolicyCeiling | ProfileLayer::OS | ProfileLayer::Default
            ),
            AccessLevel::Level1 => matches!(
                layer,
                ProfileLayer::PolicyCeiling
                    | ProfileLayer::Capability
                    | ProfileLayer::OS
                    | ProfileLayer::Default
            ),
            AccessLevel::Level2 => true,
        }
    }

    /// 返回 Level → 允许的 layers 集合（用于审计）。
    pub fn allowed_layers(&self) -> Vec<ProfileLayer> {
        let all = [
            ProfileLayer::PolicyCeiling,
            ProfileLayer::UserBinding,
            ProfileLayer::Capability,
            ProfileLayer::Tool,
            ProfileLayer::Supervisor,
            ProfileLayer::OS,
            ProfileLayer::Default,
        ];
        all.into_iter().filter(|l| self.allows_layer(*l)).collect()
    }
}

/// 传给 build_input_for_operation 的命名 profile 查询 hints。
#[derive(Debug, Clone, Default)]
pub struct ExtraProfileHints<'a> {
    pub policy_ceiling_name: Option<&'a str>,
    pub user_binding_name: Option<&'a str>,
    pub capability_profile_name: Option<&'a str>,
    pub tool_profile_name: Option<&'a str>,
    pub supervisor_profile_name: Option<&'a str>,
    pub os_default_name: &'a str,
    pub default_name: &'a str,
}

fn dedup_vec_str(mut v: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    v.retain(|s| seen.insert(s.clone()));
    v
}

fn intersect_vec_str(a: &[String], b: &[String]) -> Vec<String> {
    let set_a: HashSet<&String> = a.iter().collect();
    let set_b: HashSet<&String> = b.iter().collect();
    set_a
        .intersection(&set_b)
        .map(|s| (*s).clone())
        .collect()
}

fn union_vec_str(a: &[String], b: &[String]) -> Vec<String> {
    let out: Vec<String> = a.iter().chain(b.iter()).cloned().collect();
    dedup_vec_str(out)
}

fn has_deny_all(rules: &[NetworkRule]) -> bool {
    rules.iter().any(|r| matches!(r, NetworkRule::DenyAll))
}

fn collect_allow_rules(rules: &[NetworkRule]) -> Vec<NetworkRule> {
    rules
        .iter()
        .filter(|r| matches!(r, NetworkRule::Allow(_) | NetworkRule::AllowLocal))
        .cloned()
        .collect()
}

fn intersect_network(a: &[NetworkRule], b: &[NetworkRule]) -> Vec<NetworkRule> {
    if has_deny_all(a) || has_deny_all(b) {
        return vec![NetworkRule::DenyAll];
    }
    let allow_a = collect_allow_rules(a);
    let allow_b = collect_allow_rules(b);
    if allow_a.is_empty() || allow_b.is_empty() {
        return vec![NetworkRule::DenyAll];
    }
    let mut result = Vec::new();
    for ra in &allow_a {
        for rb in &allow_b {
            match (ra, rb) {
                (NetworkRule::AllowLocal, NetworkRule::AllowLocal) => {
                    if !result.contains(&NetworkRule::AllowLocal) {
                        result.push(NetworkRule::AllowLocal);
                    }
                }
                (NetworkRule::Allow(x), NetworkRule::Allow(y)) if x == y => {
                    let r = NetworkRule::Allow(x.clone());
                    if !result.contains(&r) {
                        result.push(r);
                    }
                }
                _ => {}
            }
        }
    }
    if result.is_empty() {
        vec![NetworkRule::DenyAll]
    } else {
        result
    }
}

/// 最严格合并（intersection）：顺序无关，算法 monotonic。
pub fn intersect_profiles(a: &SandboxProfile, b: &SandboxProfile) -> SandboxProfile {
    let read_only_paths = union_vec_str(&a.read_only_paths, &b.read_only_paths);
    let deny_paths = union_vec_str(&a.deny_paths, &b.deny_paths);
    let read_write_paths = intersect_vec_str(&a.read_write_paths, &b.read_write_paths);
    let network_rules = intersect_network(&a.network_rules, &b.network_rules);
    let allow_exec = a.allow_exec && b.allow_exec;
    let allow_fork = a.allow_fork && b.allow_fork;

    let mut result = SandboxProfile {
        name: format!("intersect({}, {})", a.name, b.name),
        read_only_paths: dedup_vec_str(read_only_paths),
        read_write_paths: dedup_vec_str(read_write_paths),
        deny_paths: dedup_vec_str(deny_paths),
        network_rules,
        allow_exec,
        allow_fork,
    };

    if result.read_write_paths.is_empty() {
        if !result.deny_paths.iter().any(|p| p == "/") {
            result.deny_paths.push("/".into());
        }
    }

    result
}

/// 对 LayeredProfileInput 按 ProfileLayer 顺序做 N 路 fold_left(intersect_profiles)。
/// 某层 None 直接跳过。结果 = 所有非 None 层的全局最严格交集。
pub fn intersect_layers(input: &LayeredProfileInput) -> IntersectionResult {
    let mut diagnostics: Vec<String> = Vec::new();
    let mut contributing_layers: Vec<ProfileLayer> = Vec::new();

    let mut ordered: Vec<(ProfileLayer, SandboxProfile)> = input
        .ordered_layers()
        .filter_map(|(layer, opt)| opt.map(|p| (layer, p.clone())))
        .collect();

    if ordered.is_empty() {
        return IntersectionResult {
            effective: SandboxProfile {
                name: "fail-closed-empty".into(),
                read_only_paths: vec![],
                read_write_paths: vec![],
                deny_paths: vec!["/".into()],
                network_rules: vec![NetworkRule::DenyAll],
                allow_exec: false,
                allow_fork: false,
            },
            contributing_layers: vec![],
            diagnostics: vec![
                "所有 7 层均为 None，fail-closed 到 deny-all profile".into()
            ],
        };
    }

    let (first_layer, first_profile) = ordered.remove(0);
    contributing_layers.push(first_layer);
    let mut effective = first_profile;

    let prev = effective.clone();
    for (layer, profile) in ordered {
        let before = effective.clone();
        effective = intersect_profiles(&before, &profile);
        contributing_layers.push(layer);

        if has_deny_all(&effective.network_rules) && !has_deny_all(&before.network_rules) {
            diagnostics.push(format!(
                "{} 收紧网络为 DenyAll（覆盖了上层更宽松的规则）",
                layer.as_str()
            ));
        }
        if !effective.allow_exec && before.allow_exec {
            diagnostics.push(format!(
                "{} 禁用了 exec（allow_exec=false）",
                layer.as_str()
            ));
        }
        if !effective.allow_fork && before.allow_fork {
            diagnostics.push(format!(
                "{} 禁用了 fork（allow_fork=false）",
                layer.as_str()
            ));
        }
    }

    let was_empty_after_intersect =
        effective.read_write_paths.is_empty() && !prev.read_write_paths.is_empty();
    if was_empty_after_intersect {
        diagnostics.push(
            "read_write_paths 交集为空，降级到 deny-all filesystem（deny_paths 追加 '/'）".into(),
        );
    }

    IntersectionResult {
        effective,
        contributing_layers,
        diagnostics,
    }
}
