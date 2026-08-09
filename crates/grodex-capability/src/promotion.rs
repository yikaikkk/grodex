use crate::descriptor::{TurnCapabilityBase, TurnCapabilityOverlay};
use crate::id::CapabilityId;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeferredPromotionDecision {
    Approved,
    Rejected { reason: String },
    TimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeferredPromotionRequest {
    pub request_id: String,
    pub turn_id: String,
    pub capability_id: CapabilityId,
    pub reason: String,
    pub requested_at: SystemTime,
    pub requester: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeferredPromotionRecord {
    pub request: DeferredPromotionRequest,
    pub decision: DeferredPromotionDecision,
    pub approver: Option<String>,
    pub decided_at: SystemTime,
}

impl DeferredPromotionRequest {
    pub fn new(
        base: &TurnCapabilityBase,
        capability_id: CapabilityId,
        reason: impl Into<String>,
        requester: Option<String>,
    ) -> Result<Self, String> {
        if !base.deferred_promotion_allowed {
            return Err(format!(
                "cannot request promotion for {}: deferred_promotion not allowed in turn {}",
                capability_id.canonical_name, base.turn_id,
            ));
        }
        if !base.promotable_ids.contains(&capability_id) {
            return Err(format!(
                "capability {} is not in turn {}'s promotable_ids list",
                capability_id.canonical_name, base.turn_id
            ));
        }
        use rand::{distributions::Alphanumeric, Rng};
        let nonce: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(8)
            .map(char::from)
            .collect();
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(capability_id.stable_hash_input());
        h.update(&base.turn_id);
        h.update(&nonce);
        let full_hash = format!("{:x}", h.finalize());
        let request_id = full_hash[..16].to_string();
        Ok(Self {
            request_id,
            turn_id: base.turn_id.clone(),
            capability_id,
            reason: reason.into(),
            requested_at: SystemTime::now(),
            requester,
        })
    }
}

impl DeferredPromotionRecord {
    pub fn decide(
        request: DeferredPromotionRequest,
        decision: DeferredPromotionDecision,
        approver: Option<String>,
    ) -> Self {
        Self {
            request,
            decision,
            approver,
            decided_at: SystemTime::now(),
        }
    }

    pub fn apply_to_overlay(&self, overlay: &mut TurnCapabilityOverlay) {
        if matches!(self.decision, DeferredPromotionDecision::Approved) {
            let id = self.request.capability_id.clone();
            if !overlay.deferred_promoted_ids.contains(&id) {
                overlay.deferred_promoted_ids.push(id);
            }
        }
    }
}
