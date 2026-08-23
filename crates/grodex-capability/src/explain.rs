//! `capabilities.explain` — the visibility causal chain (Doc 10 §23,
//! acceptance #9).
//!
//! Every absent capability must be attributable to ONE decisive stage,
//! evaluated in a fixed causal order:
//!
//! 1. Not discovered at all;
//! 2. Disabled by configuration;
//! 3. Name conflict (qualified display name took over);
//! 4. Provider failure;
//! 5. Exposure is not model-visible (AppOnly / Internal / Disabled);
//! 6. Filtered by Policy;
//! 7. Over the listing/schema budget (demoted or dropped);
//! 8. MCP auth required or connection failed;
//! 9. The current Step carries a stale generation;
//! 10. A stale revision was rejected.
//!
//! `Deferred` exposure is NOT invisible — the verdict is
//! `VisibleDeferred` (searchable via Tool Search). The explainer returns
//! the full evaluated chain up to and including the decisive factor so
//! UI/ops can show WHY, never just WHAT.

use crate::exposure::ToolExposure;
use crate::id::CapabilityId;
use serde::{Deserialize, Serialize};

/// Provider health at catalog build time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderStatus {
    Healthy,
    /// Provider failed to initialize / refresh; the previous complete
    /// catalog stays in effect (Doc 10 acceptance #2), but THIS new
    /// capability is not published.
    Failed(String),
}

/// MCP authentication / connection state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpConnStatus {
    Ok,
    /// Server requires authentication before its tools can be listed.
    AuthRequired,
    /// Connection attempt failed.
    ConnectionFailed(String),
}

/// Outcome of the Policy projection for this capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyVisibility {
    Allowed,
    /// Filtered by a matched policy rule; carries the rule source for
    /// attribution.
    Filtered(String),
}

/// Budget status when the Direct schema budget is exceeded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BudgetStatus {
    Within,
    /// Demoted Direct→Deferred by the deterministic "external first,
    /// Core protected" rule; still discoverable via search.
    DemotedToDeferred,
    /// Dropped entirely (diagnostic event recorded).
    Dropped,
}

/// Generation freshness of the Step querying the capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GenerationStatus {
    Current,
    /// The Step snapshot's capability generation predates the published
    /// state the capability was expected in.
    Stale { step_generation: u64, published_generation: u64 },
}

/// Revision freshness (deferred promotion pin, approval pin, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RevisionStatus {
    Current,
    /// A pinned revision was rejected as stale at assembly time.
    StaleRejected { pinned_revision: u64, current_revision: u64 },
}

/// All pipeline facts collected about one capability at one snapshot.
/// The caller (catalog builder) fills this in; the explainer only orders
/// and evaluates it — no hidden heuristics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityVisibilityFacts {
    pub capability_id: CapabilityId,
    pub snapshot_id: String,
    /// Was the capability discovered by any source at all?
    pub discovered: bool,
    /// Explicitly disabled in configuration.
    pub config_disabled: bool,
    /// Lost a deterministic name-conflict resolution (carries the winner).
    pub name_conflict_with: Option<CapabilityId>,
    pub provider: ProviderStatus,
    pub mcp_conn: McpConnStatus,
    pub exposure: ToolExposure,
    pub policy: PolicyVisibility,
    pub budget: BudgetStatus,
    pub generation: GenerationStatus,
    pub revision: RevisionStatus,
}

/// One evaluated link in the causal chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalLink {
    /// Fixed stage identifier in evaluation order.
    pub stage: VisibilityStage,
    /// Whether this stage passed (no blocking finding).
    pub passed: bool,
    /// Human-readable detail — for the decisive link, the exact reason.
    pub detail: String,
}

/// Fixed evaluation order of the causal chain (Doc 10 §23 list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VisibilityStage {
    Discovery,
    Config,
    NameConflict,
    Provider,
    Exposure,
    Policy,
    Budget,
    McpConnection,
    Generation,
    Revision,
}

/// Final verdict of `explain`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum VisibilityVerdict {
    /// In the model's Direct tool set.
    VisibleDirect,
    /// Not in the initial request but discoverable via Tool Search.
    VisibleDeferred,
    /// Not visible / not callable; `decisive` is the first blocking link.
    Invisible { decisive: VisibilityStage, reason: String },
}

/// The full explanation: verdict + every evaluated link up to and
/// including the decisive one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityExplanation {
    pub capability_id: CapabilityId,
    pub snapshot_id: String,
    pub verdict: VisibilityVerdict,
    pub chain: Vec<CausalLink>,
}

/// Stateless explainer: `capabilities.explain(capability_id, snapshot_id)`
/// semantics over caller-supplied facts.
#[derive(Debug, Clone, Copy, Default)]
pub struct CapabilityExplainer;

impl CapabilityExplainer {
    /// Evaluate the causal chain in fixed order. Evaluation stops at the
    /// first blocking factor (that is the decisive cause); otherwise all
    /// stages are evaluated and the verdict is derived from exposure +
    /// budget demotion.
    pub fn explain(facts: &CapabilityVisibilityFacts) -> CapabilityExplanation {
        let mut chain: Vec<CausalLink> = Vec::new();

        macro_rules! blocking {
            ($stage:expr, $cond:expr, $detail:expr) => {
                let cond = $cond;
                chain.push(CausalLink {
                    stage: $stage,
                    passed: !cond,
                    detail: if cond { $detail.clone() } else { String::from("ok") },
                });
                if cond {
                    return CapabilityExplanation {
                        capability_id: facts.capability_id.clone(),
                        snapshot_id: facts.snapshot_id.clone(),
                        verdict: VisibilityVerdict::Invisible {
                            decisive: $stage,
                            reason: $detail.clone(),
                        },
                        chain,
                    };
                }
            };
        }

        // 1. Discovery.
        blocking!(
            VisibilityStage::Discovery,
            !facts.discovered,
            format!(
                "capability {} was not discovered by any source",
                facts.capability_id.canonical_name
            )
        );
        // 2. Configuration.
        blocking!(
            VisibilityStage::Config,
            facts.config_disabled,
            format!("capability {} is disabled by configuration", facts.capability_id.canonical_name)
        );
        // 3. Name conflict.
        let conflict_detail = match &facts.name_conflict_with {
            Some(winner) => format!(
                "canonical name {} lost conflict resolution to {}",
                facts.capability_id.canonical_name, winner.canonical_name
            ),
            None => String::new(),
        };
        blocking!(
            VisibilityStage::NameConflict,
            facts.name_conflict_with.is_some(),
            conflict_detail
        );
        // 4. Provider health.
        let provider_detail = match &facts.provider {
            ProviderStatus::Failed(msg) => {
                format!("provider failed: {msg}")
            }
            ProviderStatus::Healthy => String::new(),
        };
        blocking!(
            VisibilityStage::Provider,
            matches!(facts.provider, ProviderStatus::Failed(_)),
            provider_detail
        );
        // 5. Exposure: AppOnly / Internal / Disabled never reach the model.
        //    Deferred is NOT blocking — handled in the verdict below.
        let exposure_blocking = matches!(
            facts.exposure,
            ToolExposure::AppOnly | ToolExposure::Internal | ToolExposure::Disabled
        );
        blocking!(
            VisibilityStage::Exposure,
            exposure_blocking,
            format!("exposure {:?} is not model-visible", facts.exposure)
        );
        // 6. Policy projection.
        let policy_detail = match &facts.policy {
            PolicyVisibility::Filtered(rule) => format!("filtered by policy rule: {rule}"),
            PolicyVisibility::Allowed => String::new(),
        };
        blocking!(
            VisibilityStage::Policy,
            matches!(facts.policy, PolicyVisibility::Filtered(_)),
            policy_detail
        );
        // 7. Budget: dropped is blocking; demoted-to-deferred is not.
        let budget_detail = match &facts.budget {
            BudgetStatus::Dropped => {
                "dropped: Direct schema budget exceeded and capability could not be demoted"
                    .to_string()
            }
            _ => String::new(),
        };
        blocking!(
            VisibilityStage::Budget,
            matches!(facts.budget, BudgetStatus::Dropped),
            budget_detail
        );
        // 8. MCP auth / connection.
        let mcp_blocking = !matches!(facts.mcp_conn, McpConnStatus::Ok);
        let mcp_detail = match &facts.mcp_conn {
            McpConnStatus::AuthRequired => "MCP server requires authentication".to_string(),
            McpConnStatus::ConnectionFailed(msg) => format!("MCP connection failed: {msg}"),
            McpConnStatus::Ok => String::new(),
        };
        blocking!(VisibilityStage::McpConnection, mcp_blocking, mcp_detail);
        // 9. Generation freshness.
        let gen_detail = match &facts.generation {
            GenerationStatus::Stale { step_generation, published_generation } => format!(
                "step capability generation {step_generation} predates published generation {published_generation}"
            ),
            GenerationStatus::Current => String::new(),
        };
        blocking!(
            VisibilityStage::Generation,
            matches!(facts.generation, GenerationStatus::Stale { .. }),
            gen_detail
        );
        // 10. Revision freshness.
        let rev_detail = match &facts.revision {
            RevisionStatus::StaleRejected { pinned_revision, current_revision } => format!(
                "pinned revision {pinned_revision} rejected: current revision is {current_revision}"
            ),
            RevisionStatus::Current => String::new(),
        };
        blocking!(
            VisibilityStage::Revision,
            matches!(facts.revision, RevisionStatus::StaleRejected { .. }),
            rev_detail
        );

        // Nothing blocked: verdict follows exposure + budget demotion.
        // AppOnly / Internal / Disabled were already rejected above, so
        // only Direct / Deferred / CodeMode can reach here.
        let verdict = if facts.exposure == ToolExposure::CodeMode {
            // CodeMode is callable only inside code-mode sub-agents —
            // from the model's ordinary perspective it is invisible.
            VisibilityVerdict::Invisible {
                decisive: VisibilityStage::Exposure,
                reason: "exposure CodeMode is only callable from code-mode sub-agents".into(),
            }
        } else if facts.exposure == ToolExposure::Direct
            && facts.budget == BudgetStatus::Within
        {
            VisibilityVerdict::VisibleDirect
        } else {
            // Deferred exposure, or Direct demoted by the budget rule.
            VisibilityVerdict::VisibleDeferred
        };
        CapabilityExplanation {
            capability_id: facts.capability_id.clone(),
            snapshot_id: facts.snapshot_id.clone(),
            verdict,
            chain,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::Authority;
    use crate::id::CapabilityKind;

    fn facts() -> CapabilityVisibilityFacts {
        CapabilityVisibilityFacts {
            capability_id: CapabilityId::new(
                Authority::Mcp,
                "srv",
                CapabilityKind::Tool,
                "mcp.deploy",
            ),
            snapshot_id: "snap-1".into(),
            discovered: true,
            config_disabled: false,
            name_conflict_with: None,
            provider: ProviderStatus::Healthy,
            mcp_conn: McpConnStatus::Ok,
            exposure: ToolExposure::Direct,
            policy: PolicyVisibility::Allowed,
            budget: BudgetStatus::Within,
            generation: GenerationStatus::Current,
            revision: RevisionStatus::Current,
        }
    }

    #[test]
    fn healthy_direct_capability_is_visible_with_full_chain() {
        let ex = CapabilityExplainer::explain(&facts());
        assert_eq!(ex.verdict, VisibilityVerdict::VisibleDirect);
        // All ten stages evaluated, all passed.
        assert_eq!(ex.chain.len(), 10);
        assert!(ex.chain.iter().all(|l| l.passed));
    }

    #[test]
    fn decisive_factor_is_the_first_blocking_stage() {
        // Both config-disabled AND policy-filtered — the causal chain must
        // stop at Config (the earlier stage), not conflate the two.
        let mut f = facts();
        f.config_disabled = true;
        f.policy = PolicyVisibility::Filtered("deny-*".into());
        let ex = CapabilityExplainer::explain(&f);
        match &ex.verdict {
            VisibilityVerdict::Invisible { decisive, reason } => {
                assert_eq!(*decisive, VisibilityStage::Config);
                assert!(reason.contains("disabled by configuration"));
            }
            other => panic!("expected Invisible, got {other:?}"),
        }
        // Chain stops at the decisive stage (2 links: Discovery + Config).
        assert_eq!(ex.chain.len(), 2);
        assert!(ex.chain[0].passed);
        assert!(!ex.chain[1].passed);
    }

    #[test]
    fn each_acceptance9_cause_is_distinguishable() {
        // Provider / auth / config / Exposure / Policy / budget must each
        // produce a DISTINCT decisive stage (acceptance #9).
        let mut seen = Vec::new();

        let mut f = facts();
        f.provider = ProviderStatus::Failed("timeout".into());
        seen.push(CapabilityExplainer::explain(&f));

        let mut f = facts();
        f.mcp_conn = McpConnStatus::AuthRequired;
        seen.push(CapabilityExplainer::explain(&f));

        let mut f = facts();
        f.config_disabled = true;
        seen.push(CapabilityExplainer::explain(&f));

        let mut f = facts();
        f.exposure = ToolExposure::Internal;
        seen.push(CapabilityExplainer::explain(&f));

        let mut f = facts();
        f.policy = PolicyVisibility::Filtered("rule-x".into());
        seen.push(CapabilityExplainer::explain(&f));

        let mut f = facts();
        f.budget = BudgetStatus::Dropped;
        seen.push(CapabilityExplainer::explain(&f));

        let decisives: Vec<VisibilityStage> = seen
            .iter()
            .map(|ex| match &ex.verdict {
                VisibilityVerdict::Invisible { decisive, .. } => *decisive,
                other => panic!("expected Invisible, got {other:?}"),
            })
            .collect();
        assert_eq!(
            decisives,
            vec![
                VisibilityStage::Provider,
                VisibilityStage::McpConnection,
                VisibilityStage::Config,
                VisibilityStage::Exposure,
                VisibilityStage::Policy,
                VisibilityStage::Budget,
            ]
        );
        // Auth-required carries the actionable detail.
        match &seen[1].verdict {
            VisibilityVerdict::Invisible { reason, .. } => {
                assert!(reason.contains("authentication"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn deferred_is_searchable_not_invisible() {
        let mut f = facts();
        f.exposure = ToolExposure::Deferred;
        let ex = CapabilityExplainer::explain(&f);
        assert_eq!(ex.verdict, VisibilityVerdict::VisibleDeferred);
    }

    #[test]
    fn budget_demotion_lands_in_deferred() {
        let mut f = facts();
        f.budget = BudgetStatus::DemotedToDeferred;
        let ex = CapabilityExplainer::explain(&f);
        assert_eq!(ex.verdict, VisibilityVerdict::VisibleDeferred);
        // The budget link is present and passed (demotion is not blocking).
        let budget_link = ex.chain.iter().find(|l| l.stage == VisibilityStage::Budget).unwrap();
        assert!(budget_link.passed);
    }

    #[test]
    fn stale_generation_and_stale_revision_are_separate_causes() {
        let mut f = facts();
        f.generation = GenerationStatus::Stale { step_generation: 3, published_generation: 5 };
        let ex = CapabilityExplainer::explain(&f);
        match &ex.verdict {
            VisibilityVerdict::Invisible { decisive, reason } => {
                assert_eq!(*decisive, VisibilityStage::Generation);
                assert!(reason.contains("3") && reason.contains("5"));
            }
            other => panic!("expected Invisible, got {other:?}"),
        }

        let mut f = facts();
        f.revision = RevisionStatus::StaleRejected { pinned_revision: 4, current_revision: 6 };
        let ex = CapabilityExplainer::explain(&f);
        match &ex.verdict {
            VisibilityVerdict::Invisible { decisive, reason } => {
                assert_eq!(*decisive, VisibilityStage::Revision);
                assert!(reason.contains("4") && reason.contains("6"));
            }
            other => panic!("expected Invisible, got {other:?}"),
        }
    }

    #[test]
    fn undiscovered_short_circuits_the_chain() {
        let mut f = facts();
        f.discovered = false;
        let ex = CapabilityExplainer::explain(&f);
        assert_eq!(ex.chain.len(), 1);
        match &ex.verdict {
            VisibilityVerdict::Invisible { decisive, .. } => {
                assert_eq!(*decisive, VisibilityStage::Discovery);
            }
            other => panic!("expected Invisible, got {other:?}"),
        }
    }

    #[test]
    fn name_conflict_is_attributed_to_the_winner() {
        let mut f = facts();
        f.name_conflict_with = Some(CapabilityId::new(
            Authority::Core,
            "builtin",
            CapabilityKind::Tool,
            "deploy",
        ));
        let ex = CapabilityExplainer::explain(&f);
        match &ex.verdict {
            VisibilityVerdict::Invisible { decisive, reason } => {
                assert_eq!(*decisive, VisibilityStage::NameConflict);
                assert!(reason.contains("deploy"));
            }
            other => panic!("expected Invisible, got {other:?}"),
        }
    }

    #[test]
    fn explanation_is_serializable_for_rollout() {
        let ex = CapabilityExplainer::explain(&facts());
        let json = serde_json::to_string(&ex).unwrap();
        let back: CapabilityExplanation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.verdict, ex.verdict);
        assert_eq!(back.chain.len(), ex.chain.len());
    }
}
