//! CredentialBroker — the TRUSTED holder of master credentials.
//!
//! Audit (Phase 4-1) fix: the broker previously cloned the master token
//! straight into `ActiveLease.token`, so callers handled the raw secret and
//! the design's "agents never see the token" isolation was unenforced. The
//! master token now lives ONLY inside the broker. `issue_lease` returns a
//! bounded `CredentialLease` (no token); the only way to obtain an
//! Authorization value is `broker.resolve(&lease)`, which the broker may
//! fail-closed (expired / over-use / revoked). A lease thus caps exposure:
//! even a leaked lease id is useless without the broker, and a consumed /
//! expired lease cannot be replayed.
//!
//! For the Phase-1 HTTP transport that still needs a `Bearer <token>`
//! header, the broker exposes a deliberately-narrow
//! `resolve_token_for_provider` gateway — but the master token never
//! crosses a struct boundary that promises not to log/persist it.

use grodex_auth_types::lease::CredentialLease;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::secret_store::{SecretStore, SecretStoreError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialError {
    SecretStore(String),
    InitFailed(String),
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SecretStore(m) => write!(f, "credential secret store error: {m}"),
            Self::InitFailed(m) => write!(f, "credential broker init failed: {m}"),
        }
    }
}

impl std::error::Error for CredentialError {}

impl From<SecretStoreError> for CredentialError {
    fn from(e: SecretStoreError) -> Self {
        Self::SecretStore(e.to_string())
    }
}

/// A lease the broker has issued. Carries NO copy of the master token —
/// only the lease metadata + a use counter. Callers redeem it via
/// `CredentialBroker::resolve` to get a one-time materialized credential.
pub struct ActiveLease {
    pub lease: CredentialLease,
    pub issued_at: Instant,
    pub ttl: Duration,
    pub use_count: u32,
}

impl ActiveLease {
    /// Whether the lease is still redeemable (uses left and unexpired).
    pub fn is_valid(&self) -> bool {
        self.use_count < self.lease.max_uses && self.issued_at.elapsed() < self.ttl
    }

    /// Remaining uses (0 = exhausted).
    pub fn remaining_uses(&self) -> u32 {
        self.lease.max_uses.saturating_sub(self.use_count)
    }
}

/// Errors from lease redemption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseError {
    /// The lease id is unknown / already redeemed-and-evicted.
    Unknown,
    /// The lease has expired (TTL elapsed).
    Expired,
    /// The lease's max_uses are consumed.
    Exhausted,
    /// Provider/target mismatch — the lease was bound to a different endpoint.
    EndpointMismatch,
    /// A global revocation advanced past the lease's epoch.
    Revoked,
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => write!(f, "unknown lease"),
            Self::Expired => write!(f, "lease expired"),
            Self::Exhausted => write!(f, "lease uses exhausted"),
            Self::EndpointMismatch => write!(f, "lease endpoint mismatch"),
            Self::Revoked => write!(f, "lease revoked (epoch advanced)"),
        }
    }
}

impl std::error::Error for LeaseError {}

/// The trusted broker. Holds master tokens keyed by provider; the master
/// tokens NEVER leave this struct. External code only ever sees leases and
/// redeemed one-shot `LeasedToken`s.
pub struct CredentialBroker {
    /// provider_id → master token. Held internally, never cloned out.
    master_tokens: HashMap<String, String>,
    /// lease_id → issued ActiveLease (the broker keeps the bookkeeping).
    leases: HashMap<String, ActiveLease>,
    /// lease_id → (server_id, refresh_token).  Held internally, never
    /// exposed to external crates.  Only the OAuth broker may touch this.
    refresh_tokens: HashMap<String, (String, String)>,
    /// Monotonic revocation epoch — bumped by `revoke_all`, invalidating
    /// every lease minted at an earlier epoch.
    revocation_epoch: u64,
    /// Default lease TTL.
    default_ttl: Duration,
    /// Optional durable secret store for credential persistence.
    /// When present, master tokens are also persisted here for restart
    /// survival (file-backed `~/.grodex/credentials.json` by default —
    /// Grodex never reads the OS keychain).
    secret_store: Option<Arc<dyn SecretStore>>,
}

impl CredentialBroker {
    /// Construct a broker seeded with a single master token. (Legacy entry
    /// point for the Phase-1 single-provider path.)
    pub fn new(master_token: String) -> Self {
        let mut master_tokens = HashMap::new();
        master_tokens.insert("default".to_string(), master_token);
        Self {
            master_tokens,
            leases: HashMap::new(),
            refresh_tokens: HashMap::new(),
            revocation_epoch: 1,
            default_ttl: Duration::from_secs(300),
            secret_store: None,
        }
    }

    /// Construct an empty broker; register master tokens via `register_provider`.
    pub fn empty() -> Self {
        Self {
            master_tokens: HashMap::new(),
            leases: HashMap::new(),
            refresh_tokens: HashMap::new(),
            revocation_epoch: 1,
            default_ttl: Duration::from_secs(300),
            secret_store: None,
        }
    }

    /// Construct an empty broker with a caller-supplied [`SecretStore`]
    /// backend (any platform). Used for tests and for plugging in durable
    /// backends — production wiring uses [`crate::secret_store::FileSecretStore`].
    pub fn with_secret_store(store: Arc<dyn SecretStore>) -> Self {
        Self {
            master_tokens: HashMap::new(),
            leases: HashMap::new(),
            refresh_tokens: HashMap::new(),
            revocation_epoch: 1,
            default_ttl: Duration::from_secs(300),
            secret_store: Some(store),
        }
    }

    /// Store a refresh token associated with a lease.  `pub(crate)` so only
    /// this crate's OAuth broker may call it; external crates can never see
    /// refresh tokens.
    pub(crate) fn store_refresh_token(
        &mut self,
        lease_id: String,
        server_id: String,
        refresh_token: String,
    ) {
        self.refresh_tokens
            .insert(lease_id, (server_id, refresh_token));
    }

    /// Look up a refresh token.  `pub(crate)` only.
    pub(crate) fn get_refresh_token(
        &self,
        lease_id: &str,
    ) -> Option<(&str, &str)> {
        self.refresh_tokens
            .get(lease_id)
            .map(|(s, t)| (s.as_str(), t.as_str()))
    }

    /// Delete a refresh token (e.g. after rotation or revocation).
    pub(crate) fn delete_refresh_token(&mut self, lease_id: &str) {
        self.refresh_tokens.remove(lease_id);
    }

    /// Revoke a single lease by id (remove + mark as unredeemable).
    pub(crate) fn revoke_by_id(&mut self, lease_id: &str) {
        self.leases.remove(lease_id);
        self.refresh_tokens.remove(lease_id);
    }

    /// Register (or rotate) a master token for `provider_id`. Old leases for
    /// that provider are NOT auto-revoked — call `revoke_all` if you need to.
    pub fn register_provider(&mut self, provider_id: impl Into<String>, master_token: String) {
        self.master_tokens.insert(provider_id.into(), master_token);
    }

    /// Whether this broker has an OS-backed secret store attached (i.e.
    /// master tokens can survive a process restart).
    pub fn has_secret_store(&self) -> bool {
        self.secret_store.is_some()
    }

    /// Persist `provider_id`'s current master token into the secret store.
    /// No-op (returns `Ok(false)`) when no secret store is attached or the
    /// provider is unregistered. The token already lives in memory; this only
    /// adds restart durability. Master tokens are keyed as `master:<provider>`.
    pub async fn persist_provider(&self, provider_id: &str) -> Result<bool, CredentialError> {
        let Some(store) = &self.secret_store else {
            return Ok(false);
        };
        let Some(token) = self.master_tokens.get(provider_id) else {
            return Ok(false);
        };
        store.store(&format!("master:{provider_id}"), token).await?;
        Ok(true)
    }

    /// Hydrate `provider_id`'s master token from the secret store into
    /// memory, if not already present. Returns `true` when the token is
    /// usable afterwards (either already in memory or successfully loaded).
    /// This is the restart-survival path: a fresh broker re-registers
    /// credentials persisted by a previous process.
    pub async fn hydrate_provider(&mut self, provider_id: &str) -> Result<bool, CredentialError> {
        if self.master_tokens.contains_key(provider_id) {
            return Ok(true);
        }
        let Some(store) = &self.secret_store else {
            return Ok(false);
        };
        match store.retrieve(&format!("master:{provider_id}")).await? {
            Some(token) if !token.is_empty() => {
                self.master_tokens.insert(provider_id.to_string(), token);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Bump the revocation epoch. All outstanding leases minted at an earlier
    /// epoch become unredeemable. (Mirrors PermissionManager::revoke_all.)
    pub fn revoke_all(&mut self) {
        self.revocation_epoch = self.revocation_epoch.checked_add(1).expect("revocation epoch overflow");
        // Evict every outstanding lease — they're now invalid by epoch.
        self.leases.clear();
    }

    pub fn revocation_epoch(&self) -> u64 {
        self.revocation_epoch
    }

    /// Issue a single-use lease bound to `audience` (endpoint URL) for
    /// `provider_id`. The returned lease contains NO token; redeem it via
    /// `resolve`. Fail-closed if the provider has no master token.
    pub fn issue_lease(&mut self, provider_id: &str, audience: &str) -> Option<CredentialLease> {
        if !self.master_tokens.contains_key(provider_id) {
            return None;
        }
        let lease = CredentialLease {
            lease_id: format!("lease-{}-{}", provider_id, uuid::Uuid::new_v4()),
            handle_id: format!("handle-{provider_id}"),
            endpoint_binding: audience.to_string(),
            issued_to_process_identity: std::process::id().to_string(),
            max_uses: 1,
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(self.default_ttl.as_secs() as i64),
            revocation_epoch: self.revocation_epoch,
        };
        let lease_id = lease.lease_id.clone();
        self.leases.insert(
            lease_id,
            ActiveLease {
                lease: lease.clone(),
                issued_at: Instant::now(),
                ttl: self.default_ttl,
                use_count: 0,
            },
        );
        Some(lease)
    }

    /// Redeem a lease for the materialized master token. The master token
    /// leaves the broker ONLY here, and ONLY if the lease is still valid
    /// (unexpired, unused, un-revoked, endpoint-matched). Each successful
    /// redemption consumes one use, so a single-use lease cannot be replayed.
    pub fn resolve(&mut self, lease: &CredentialLease, audience: &str) -> Result<String, LeaseError> {
        // 1. epoch check (revocation invalidates old leases).
        if lease.revocation_epoch != self.revocation_epoch {
            return Err(LeaseError::Revoked);
        }
        // 2. endpoint binding check.
        if lease.endpoint_binding != audience {
            return Err(LeaseError::EndpointMismatch);
        }
        // 3. look up the bookkeeping entry.
        let entry = self.leases.get_mut(&lease.lease_id).ok_or(LeaseError::Unknown)?;
        if !entry.is_valid() {
            // Classify expired vs exhausted for telemetry.
            let err = if entry.use_count >= entry.lease.max_uses {
                LeaseError::Exhausted
            } else {
                LeaseError::Expired
            };
            // Evict dead leases to bound memory.
            let lease_id = lease.lease_id.clone();
            self.leases.remove(&lease_id);
            return Err(err);
        }
        // 4. consume one use.
        entry.use_count += 1;
        let provider = &entry.lease.handle_id["handle-".len()..].to_string();
        // 5. materialize the master token for this one request.
        let token = self
            .master_tokens
            .get(provider)
            .cloned()
            .ok_or(LeaseError::Unknown)?;
        Ok(token)
    }

    /// Narrow gateway escape-hatch for transports (e.g. the Phase-1 HTTP
    /// client) that build a `Bearer <token>` header at request time. This is
    /// the ONLY way to obtain the raw token without going through a lease,
    /// and it is intentionally prominent so the master-token-leak surface
    /// is auditable. Prefer `issue_lease` + `resolve` in new code.
    ///
    /// Returns `None` if the provider isn't registered.
    pub fn resolve_token_for_provider(&self, provider_id: &str) -> Option<String> {
        if provider_id.is_empty() {
            // Legacy callers passed "default".
            self.master_tokens.get("default").cloned()
        } else {
            self.master_tokens
                .get(provider_id)
                .or_else(|| self.master_tokens.get("default"))
                .cloned()
        }
    }

    /// Number of outstanding (not-yet-redeemed-and-evicted) leases.
    pub fn outstanding_lease_count(&self) -> usize {
        self.leases.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_lease_carries_no_token() {
        let mut broker = CredentialBroker::new("sk-master-secret".into());
        let lease = broker.issue_lease("default", "https://api.openai.com/v1").unwrap();
        // The lease struct has no token field at all — by construction the
        // master secret cannot leak through `issue_lease`.
        assert!(!format!("{lease:?}").contains("sk-master-secret"));
    }

    #[test]
    fn resolve_consumes_single_use_and_rejects_replay() {
        let mut broker = CredentialBroker::new("sk-master-secret".into());
        let audience = "https://api.openai.com/v1";
        let lease = broker.issue_lease("default", audience).unwrap();
        // First redemption yields the master token.
        let t1 = broker.resolve(&lease, audience).expect("first resolve");
        assert_eq!(t1, "sk-master-secret");
        // Second redemption of the same lease must fail (single-use).
        let err = broker.resolve(&lease, audience).unwrap_err();
        assert_eq!(err, LeaseError::Exhausted, "replay must be rejected");
    }

    #[test]
    fn resolve_rejects_wrong_audience() {
        let mut broker = CredentialBroker::new("sk-master-secret".into());
        let lease = broker.issue_lease("default", "https://api.openai.com/v1").unwrap();
        let err = broker
            .resolve(&lease, "https://evil.example.com/v1")
            .unwrap_err();
        assert_eq!(err, LeaseError::EndpointMismatch);
    }

    #[test]
    fn revoke_all_invalidates_outstanding_leases() {
        let mut broker = CredentialBroker::new("sk-master-secret".into());
        let audience = "https://api.openai.com/v1";
        let lease = broker.issue_lease("default", audience).unwrap();
        broker.revoke_all();
        let err = broker.resolve(&lease, audience).unwrap_err();
        // Epoch mismatch is checked first → Revoked (even before Unknown,
        // because the lease object still carries its original epoch).
        assert_eq!(err, LeaseError::Revoked);
    }

    #[test]
    fn unknown_provider_fail_closed() {
        let mut broker = CredentialBroker::new("sk-master-secret".into());
        assert!(broker.issue_lease("not-registered", "https://x").is_none());
    }

    #[tokio::test]
    async fn persist_then_hydrate_roundtrip_survives_restart() {
        // Share one store across two brokers to simulate process restart.
        let store = Arc::new(crate::secret_store::InMemorySecretStore::new());
        let mut first = CredentialBroker::with_secret_store(store.clone());
        first.register_provider("openai", "sk-durable".to_string());
        assert!(first.persist_provider("openai").await.unwrap());

        // Fresh broker ("restarted process") rehydrates from the store.
        let mut second = CredentialBroker::with_secret_store(store.clone());
        assert!(second.hydrate_provider("openai").await.unwrap());
        assert_eq!(
            second.resolve_token_for_provider("openai").as_deref(),
            Some("sk-durable")
        );
    }

    #[tokio::test]
    async fn persist_hydrate_noop_without_secret_store() {
        let mut broker = CredentialBroker::empty();
        broker.register_provider("openai", "sk-ephemeral".to_string());
        // No store attached: persist is a no-op, hydrate finds nothing.
        assert!(!broker.persist_provider("openai").await.unwrap());
        assert!(!broker.hydrate_provider("other").await.unwrap());
        assert!(!broker.has_secret_store());
    }
}

