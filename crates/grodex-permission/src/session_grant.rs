//! SessionPolicyGrant — session-level "always allow" grant (doc 10 §20.12).
//!
//! When the user selects "always allow this session", a SessionPolicyGrant
//! is created rather than modifying the original tool call or appending
//! broad rules to user/managed config. Default expires at session end.
//!
//! Unlike `PermissionLease` (single-use, revocation-epoch-bound), a
//! SessionPolicyGrant persists for the session and can authorize multiple
//! calls — up to `max_uses` if set, or unlimited if `None`.

use serde::{Deserialize, Serialize};

/// Session-level "always allow" grant (doc 10 §20.12). When the user
/// selects "always allow this session", a SessionPolicyGrant is created
/// rather than modifying the original tool call or appending broad rules
/// to user/managed config. Default expires at session end.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPolicyGrant {
    /// Unique grant identifier.
    pub grant_id: String,
    /// The approval ticket that originated this grant.
    pub origin_approval_id: String,
    /// Who this grant applies to (user/agent/role).
    pub subject_id: String,
    /// Stable capability id (not revision-bound).
    pub capability_id: String,
    /// Normalized operation matcher.
    pub normalized_operation_matcher: String,
    /// Normalized resource or command matcher (serialized).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_resource_or_command_matcher: Option<String>,
    /// Hash of the policy ceiling at creation time.
    pub ceiling_hash: String,
    /// Policy generation when this grant was created.
    pub policy_generation_created: u64,
    /// Creation timestamp.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Expiry timestamp. None = expires at session end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Max uses. None = unlimited (always allow). Some(1) = allow once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<u64>,
    /// If revoked, when.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl SessionPolicyGrant {
    /// Whether this grant is currently active (not revoked, not expired).
    pub fn is_active(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        if self.revoked_at.is_some() {
            return false;
        }
        if let Some(exp) = self.expires_at {
            if now > exp {
                return false;
            }
        }
        true
    }

    /// Whether this grant has expired (by timestamp, ignores revocation).
    pub fn is_expired(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.expires_at
            .map(|exp| now > exp)
            .unwrap_or(false)
    }

    /// Whether this grant has been revoked.
    pub fn is_revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    /// Revoke this grant at the given time.
    pub fn revoke(&mut self, now: chrono::DateTime<chrono::Utc>) {
        self.revoked_at = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn grant_with_expiry(exp: Option<chrono::DateTime<chrono::Utc>>) -> SessionPolicyGrant {
        SessionPolicyGrant {
            grant_id: "g1".into(),
            origin_approval_id: "a1".into(),
            subject_id: "user-1".into(),
            capability_id: "read_file".into(),
            normalized_operation_matcher: "tool=read_file".into(),
            normalized_resource_or_command_matcher: Some("path=/tmp/*".into()),
            ceiling_hash: "abc123".into(),
            policy_generation_created: 1,
            created_at: chrono::Utc.timestamp_opt(1000, 0).unwrap(),
            expires_at: exp,
            max_uses: None,
            revoked_at: None,
        }
    }

    #[test]
    fn active_when_no_expiry_no_revocation() {
        let g = grant_with_expiry(None);
        let now = chrono::Utc.timestamp_opt(2000, 0).unwrap();
        assert!(g.is_active(now));
        assert!(!g.is_expired(now));
        assert!(!g.is_revoked());
    }

    #[test]
    fn expired_after_expiry_time() {
        let exp = chrono::Utc.timestamp_opt(1500, 0).unwrap();
        let g = grant_with_expiry(Some(exp));
        let before = chrono::Utc.timestamp_opt(1400, 0).unwrap();
        let after = chrono::Utc.timestamp_opt(1600, 0).unwrap();
        assert!(g.is_active(before), "active before expiry");
        assert!(!g.is_active(after), "inactive after expiry");
        assert!(g.is_expired(after));
        assert!(!g.is_expired(before));
    }

    #[test]
    fn revoked_grant_is_inactive() {
        let mut g = grant_with_expiry(None);
        let now = chrono::Utc.timestamp_opt(2000, 0).unwrap();
        assert!(!g.is_revoked());
        g.revoke(now);
        assert!(g.is_revoked());
        assert!(!g.is_active(now));
    }

    #[test]
    fn revoked_grant_is_inactive_even_before_expiry() {
        let exp = chrono::Utc.timestamp_opt(3000, 0).unwrap();
        let mut g = grant_with_expiry(Some(exp));
        let now = chrono::Utc.timestamp_opt(2000, 0).unwrap();
        // Before expiry, should be active.
        assert!(g.is_active(now));
        g.revoke(now);
        // After revocation, inactive even though not expired.
        assert!(!g.is_active(now));
        assert!(!g.is_expired(now), "not expired, just revoked");
    }

    #[test]
    fn max_uses_none_means_unlimited() {
        let g = grant_with_expiry(None);
        assert!(g.max_uses.is_none(), "None max_uses = unlimited");
    }

    #[test]
    fn serde_roundtrip_preserves_fields() {
        let g = grant_with_expiry(Some(chrono::Utc.timestamp_opt(5000, 0).unwrap()));
        let json = serde_json::to_string(&g).unwrap();
        let g2: SessionPolicyGrant = serde_json::from_str(&json).unwrap();
        assert_eq!(g.grant_id, g2.grant_id);
        assert_eq!(g.capability_id, g2.capability_id);
        assert_eq!(g.ceiling_hash, g2.ceiling_hash);
        assert_eq!(g.expires_at, g2.expires_at);
        assert_eq!(g.normalized_resource_or_command_matcher, g2.normalized_resource_or_command_matcher);
    }

    #[test]
    fn serde_optional_fields_default_to_none() {
        // Minimal JSON with only required fields.
        let json = r#"{
            "grant_id": "g2",
            "origin_approval_id": "a2",
            "subject_id": "user-2",
            "capability_id": "exec",
            "normalized_operation_matcher": "tool=exec",
            "ceiling_hash": "hash",
            "policy_generation_created": 5,
            "created_at": "2024-01-01T00:00:00Z"
        }"#;
        let g: SessionPolicyGrant = serde_json::from_str(json).unwrap();
        assert!(g.normalized_resource_or_command_matcher.is_none());
        assert!(g.expires_at.is_none());
        assert!(g.max_uses.is_none());
        assert!(g.revoked_at.is_none());
        assert!(g.is_active(g.created_at));
    }
}
