//! McpOAuthCoordinator — wires the grodex-auth OAuth broker into the MCP
//! server connection flow.
//!
//! The heavy lifting (PKCE authorization-code flow, OIDC discovery, token
//! exchange, refresh) lives in [`grodex_auth::McpoAuthBroker`]; this module
//! is the integration layer that the MCP runtime actually calls:
//!
//! 1. At session build time, every `[[mcp_server]]` config carrying an
//!    `oauth` block is registered via [`McpOAuthCoordinator::register_server`].
//! 2. When a server needs authorization, [`McpOAuthCoordinator::begin_authorization`]
//!    returns the URL the user must open; the redirect callback hands
//!    `code` + `state` back to [`McpOAuthCoordinator::complete_authorization`].
//! 3. HTTP transports then call [`McpOAuthCoordinator::bearer_token`] to
//!    redeem a one-shot access token for each request (lease model — the
//!    raw token never sits in the MCP layer longer than one request).
//!    Expired leases are transparently rolled via the refresh token.

use std::collections::HashMap;

use grodex_auth::{
    AuthBreakerState, AuthCircuitBreaker, AuthErrorKind, AuthorizationUrl, CredentialBroker,
    CredentialLease, McpoAuthBroker, McpoAuthError, SecretStore, ServerId,
};

use crate::server::McpServerConfig;

/// Per-server OAuth state held by the coordinator.
#[derive(Debug)]
struct ServerOAuthState {
    /// The most recent valid lease minted by token exchange / refresh, plus
    /// the audience it is bound to (the server's token endpoint).
    lease: CredentialLease,
}

/// Coordinates OAuth authorization for MCP servers.
///
/// Owns both the protocol broker ([`McpoAuthBroker`]) and the trusted
/// credential holder ([`CredentialBroker`]) so refresh tokens and access
/// tokens never cross the grodex-auth boundary into MCP code. Each server
/// also carries its own circuit-breaker entry (Doc 20 §10): an auth
/// failure quarantines ONLY that server, and a permanently revoked refresh
/// token stops the loop instead of retrying forever.
pub struct McpOAuthCoordinator {
    oauth: McpoAuthBroker,
    credentials: CredentialBroker,
    /// server_id → live OAuth state (current lease).
    servers: HashMap<ServerId, ServerOAuthState>,
    /// Per-server auth circuit breaker (Doc 20 §10). MCP transports report
    /// 401/403-class errors via [`Self::record_auth_failure`]; refresh
    /// attempts are gated by [`Self::refresh_allowed`].
    breaker: AuthCircuitBreaker,
}

impl McpOAuthCoordinator {
    /// Create a coordinator with in-memory credential storage.
    pub fn new() -> Self {
        Self {
            oauth: McpoAuthBroker::new(),
            credentials: CredentialBroker::empty(),
            servers: HashMap::new(),
            breaker: AuthCircuitBreaker::new(),
        }
    }

    /// Create a coordinator whose credential broker persists master tokens
    /// into `store` (e.g. the macOS Keychain) for restart survival.
    pub fn with_secret_store(store: std::sync::Arc<dyn SecretStore>) -> Self {
        Self {
            oauth: McpoAuthBroker::new(),
            credentials: CredentialBroker::with_secret_store(store),
            servers: HashMap::new(),
            breaker: AuthCircuitBreaker::new(),
        }
    }

    /// Register a server's OAuth client config (from its `[[mcp_server]]`
    /// config block). Returns `false` when the server has no `oauth` block
    /// (nothing to register — stdio servers never need OAuth).
    pub fn register_server(&mut self, config: &McpServerConfig) -> Result<bool, McpoAuthError> {
        let Some(oauth_cfg) = &config.oauth else {
            return Ok(false);
        };
        self.oauth.register_server(config.name.clone(), oauth_cfg.clone())?;
        Ok(true)
    }

    /// Whether `server_id` has an OAuth client registered.
    pub fn is_configured(&self, server_id: &str) -> bool {
        self.oauth
            .registered_server_ids()
            .iter()
            .any(|id| id.as_str() == server_id)
    }

    /// Whether `server_id` still needs the user to complete authorization
    /// (configured but has no live lease).
    pub fn requires_authorization(&self, server_id: &str) -> bool {
        self.is_configured(server_id) && !self.servers.contains_key(server_id)
    }

    /// Start the authorization-code flow for `server_id`; returns the URL
    /// the user must open in a browser. `scopes_hint` may be empty to use
    /// the configured defaults.
    pub fn begin_authorization(
        &mut self,
        server_id: &str,
        scopes_hint: &[String],
    ) -> Result<AuthorizationUrl, McpoAuthError> {
        let id = server_id.to_string();
        self.oauth.start_authorization_flow(&id, scopes_hint)
    }

    /// Complete the flow with the `code` + `state` from the redirect
    /// callback. On success the resulting lease is stored and the server
    /// becomes authorized. Returns the server_id that was authorized.
    pub async fn complete_authorization(
        &mut self,
        code: String,
        state: String,
    ) -> Result<ServerId, McpoAuthError> {
        // The server id is recovered from the in-flight pending entry, so
        // the caller does not need to repeat it (and cannot lie about it).
        let lease = self
            .oauth
            .exchange_code_for_token(&ServerId::default(), code, state, &mut self.credentials)
            .await?;
        // Recover which server this lease belongs to: the pending entry was
        // consumed by the exchange; look it up via the lease handle which is
        // `handle-oauth:{server_id}`.
        let provider_key = lease.handle_id.trim_start_matches("handle-");
        let server_id = provider_key
            .trim_start_matches("oauth:")
            .to_string();
        self.servers.insert(
            server_id.clone(),
            ServerOAuthState { lease },
        );
        Ok(server_id)
    }

    /// Redeem a one-shot `Bearer` access token for `server_id`.
    ///
    /// Returns `Ok(None)` when the server is not configured for OAuth or
    /// has not been authorized yet. Expired / exhausted leases are rolled
    /// via the stored refresh token before failing — but only while the
    /// server's circuit breaker admits a refresh (Doc 20 §10: a revoked
    /// refresh token enters ReauthRequired and never loops).
    pub async fn bearer_token(&mut self, server_id: &str) -> Result<Option<String>, McpoAuthError> {
        let Some(entry) = self.servers.get(server_id) else {
            return Ok(None);
        };
        let lease = entry.lease.clone();
        let audience = lease.endpoint_binding.clone();
        match self.credentials.resolve(&lease, &audience) {
            Ok(token) => return Ok(Some(token)),
            // Single-use leases are consumed by design — and TTL expiry is
            // expected between turns. Roll a fresh lease via the refresh
            // token, then retry once.
            Err(
                grodex_auth::LeaseError::Exhausted
                | grodex_auth::LeaseError::Expired
                | grodex_auth::LeaseError::Revoked,
            ) => {}
            Err(e) => return Err(McpoAuthError::Protocol(format!("lease resolve failed: {e}"))),
        }

        // Breaker gate: a terminal key (ReauthRequired/Revoked) or a key in
        // cooldown must NOT trigger another refresh attempt.
        if !self.breaker.allow_refresh(server_id) {
            return Err(McpoAuthError::Protocol(format!(
                "auth breaker open for server '{server_id}' (state {:?}); re-authentication required",
                self.breaker.state(server_id)
            )));
        }
        self.breaker.mark_refreshing(server_id);

        match self
            .oauth
            .refresh_credential(&lease.lease_id, &mut self.credentials)
            .await
        {
            Ok(new_lease) => {
                let token = self
                    .credentials
                    .resolve(&new_lease, &new_lease.endpoint_binding)
                    .map_err(|e| {
                        McpoAuthError::Protocol(format!("refreshed lease resolve failed: {e}"))
                    })?;
                self.breaker.record_success(server_id);
                self.servers.insert(
                    server_id.to_string(),
                    ServerOAuthState { lease: new_lease },
                );
                Ok(Some(token))
            }
            Err(e) => {
                // Classify: an invalid/expired refresh token is permanent; the
                // breaker turns terminal so we never loop (acceptance #4).
                // Network-class failures stay transient (bounded backoff).
                let kind = match &e {
                    McpoAuthError::InvalidTokenResponse(_) => AuthErrorKind::RefreshRevoked,
                    McpoAuthError::Http(_) => AuthErrorKind::Transient,
                    _ => AuthErrorKind::RefreshRevoked,
                };
                self.breaker.record_failure(server_id, kind);
                Err(e)
            }
        }
    }

    /// Report an auth error observed by an MCP transport for `server_id`
    /// (e.g. a 401/403 on an API call). Errors are classified into the
    /// Doc 20 §9 taxonomy; only the affected server is quarantined.
    pub fn record_auth_failure(&mut self, server_id: &str, kind: AuthErrorKind) {
        self.breaker.record_failure(server_id, kind);
    }

    /// Whether a refresh attempt is currently admitted for `server_id`
    /// (breaker gate, Doc 20 §10).
    pub fn refresh_allowed(&self, server_id: &str) -> bool {
        matches!(
            self.breaker.state(server_id),
            AuthBreakerState::Healthy | AuthBreakerState::Refreshing | AuthBreakerState::HalfOpen
        )
    }

    /// Current breaker state for `server_id`.
    pub fn auth_breaker_state(&self, server_id: &str) -> AuthBreakerState {
        self.breaker.state(server_id)
    }

    /// Clear a terminal breaker state after the user re-authenticated.
    pub fn reset_auth_after_reauth(&mut self, server_id: &str) {
        self.breaker.reset_after_reauth(server_id);
    }

    /// Number of authorization flows currently awaiting callback.
    pub fn in_flight_count(&self) -> usize {
        self.oauth.in_flight_count()
    }

    /// Servers that have completed authorization (have a live lease).
    pub fn authorized_servers(&self) -> Vec<&str> {
        self.servers.keys().map(String::as_str).collect()
    }
}

impl Default for McpOAuthCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grodex_auth::{AuthBreakerState, AuthErrorKind, OAuthClientConfig, RefreshPolicy};

    fn oauth_cfg() -> OAuthClientConfig {
        OAuthClientConfig {
            client_id: "client-123".to_string(),
            auth_endpoint: "https://auth.example.com/authorize".to_string(),
            token_endpoint: "https://auth.example.com/token".to_string(),
            scopes: vec!["read".to_string()],
            redirect_uri: "http://localhost:8791/callback".to_string(),
            audience: None,
            provider_kind: Default::default(),
            client_secret: None,
        }
    }

    #[test]
    fn register_oauth_server_from_config() {
        let mut coord = McpOAuthCoordinator::new();
        let mut cfg = McpServerConfig::new("linear", "mcp-linear");
        assert!(!cfg.requires_oauth());
        cfg.oauth = Some(oauth_cfg());

        assert!(coord.register_server(&cfg).unwrap());
        assert!(coord.is_configured("linear"));
        assert!(coord.requires_authorization("linear"));

        // Stdio server without oauth block: nothing registered.
        let plain = McpServerConfig::new("fs", "mcp-fs");
        assert!(!coord.register_server(&plain).unwrap());
        assert!(!coord.is_configured("fs"));
    }

    #[test]
    fn begin_authorization_returns_url_with_state() {
        let mut coord = McpOAuthCoordinator::new();
        let mut cfg = McpServerConfig::new("linear", "mcp-linear");
        cfg.oauth = Some(oauth_cfg());
        coord.register_server(&cfg).unwrap();

        let url = coord.begin_authorization("linear", &[]).unwrap();
        assert!(url.starts_with("https://auth.example.com/authorize?"));
        assert!(url.contains("client_id=client-123"));
        assert!(url.contains("code_challenge_method=S256"), "PKCE must be on");
        assert_eq!(coord.in_flight_count(), 1);
    }

    #[tokio::test]
    async fn complete_authorization_rejects_unknown_state() {
        let mut coord = McpOAuthCoordinator::new();
        let mut cfg = McpServerConfig::new("linear", "mcp-linear");
        cfg.oauth = Some(oauth_cfg());
        coord.register_server(&cfg).unwrap();

        // No flow was started → the state is unknown → fail closed before
        // any network call.
        let err = coord
            .complete_authorization("code-abc".into(), "bogus-state".into())
            .await
            .unwrap_err();
        assert!(matches!(err, McpoAuthError::StateMismatch { .. }));
    }

    #[tokio::test]
    async fn bearer_token_none_when_unauthorized() {
        let mut coord = McpOAuthCoordinator::new();
        assert_eq!(coord.bearer_token("unknown").await.unwrap(), None);

        let mut cfg = McpServerConfig::new("linear", "mcp-linear");
        cfg.oauth = Some(oauth_cfg());
        coord.register_server(&cfg).unwrap();
        // Configured but not yet authorized → still None.
        assert_eq!(coord.bearer_token("linear").await.unwrap(), None);
        assert!(coord.authorized_servers().is_empty());
    }

    #[test]
    fn breaker_quarantines_only_affected_server() {
        // Doc 20 §10: MCP auth 失败只隔离对应 server。
        let mut coord = McpOAuthCoordinator::new();
        // Permanent failure on one server → terminal, never loops.
        coord.record_auth_failure("linear", AuthErrorKind::RefreshRevoked);
        assert_eq!(
            coord.auth_breaker_state("linear"),
            AuthBreakerState::ReauthRequired
        );
        assert!(!coord.refresh_allowed("linear"));
        // Other servers are unaffected.
        assert!(coord.refresh_allowed("github"));
        assert_eq!(coord.auth_breaker_state("github"), AuthBreakerState::Healthy);

        // 403-class errors must not refresh (acceptance #3): they are not
        // even breaker-tripping events — the policy layer refuses refresh.
        assert_eq!(
            RefreshPolicy::for_error(AuthErrorKind::PolicyDenied),
            RefreshPolicy::FailWithoutRefresh
        );

        // Re-auth clears the terminal state.
        coord.reset_auth_after_reauth("linear");
        assert!(coord.refresh_allowed("linear"));
    }
}
