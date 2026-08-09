//! AuthManager — session-scoped credential management.
//!
//! Resolves credentials for providers, supports API key rotation detection,
//! and provides the single entry point for the Provider adapter.

use crate::store::CredentialStore;

/// Session-scoped authentication manager.
///
/// Created once per session. Resolves credentials for provider requests.
/// Environment variable changes are detected on each resolution call
/// (no caching of the actual secret value).
#[derive(Debug)]
pub struct AuthManager {
    store: CredentialStore,
}

impl AuthManager {
    /// Create a new manager and auto-discover credentials from environment.
    pub fn new() -> Self {
        let mut store = CredentialStore::new();
        store.discover_from_env("");
        Self { store }
    }

    /// Create a manager with a specific env prefix for discovery.
    pub fn with_prefix(prefix: &str) -> Self {
        let mut store = CredentialStore::new();
        store.discover_from_env(prefix);
        Self { store }
    }

    /// Register an account manually.
    pub fn register_account(&mut self, account: grodex_auth_types::account::AccountDescriptor) {
        self.store.register(account);
    }

    /// Resolve the API key for a provider.
    ///
    /// Returns `None` if no credential is configured for this provider.
    /// The key is read from the environment at call time.
    pub fn resolve_for_provider(&self, provider_id: &str) -> Option<String> {
        let account = self.store.find_by_provider(provider_id)?;
        self.store.resolve_key(&account.account_id)
    }

    /// Check if a provider has credentials configured.
    pub fn has_credentials(&self, provider_id: &str) -> bool {
        self.store.find_by_provider(provider_id).is_some()
    }

    /// List all configured providers.
    pub fn configured_providers(&self) -> Vec<String> {
        self.store.list().iter().map(|a| a.provider_id.clone()).collect()
    }
}

impl Default for AuthManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_discovers_env_keys() {
        let mut store = CredentialStore::new();
        store.discover_from_iter(
            [("GRODEX_TEST_OPENAI_API_KEY".into(), "sk-test123".into())],
            "GRODEX_TEST",
        );
        assert!(store.find_by_provider("grodex_test_openai").is_some());
    }

    #[test]
    fn no_credentials_for_unknown_provider() {
        let mgr = AuthManager::new();
        assert!(!mgr.has_credentials("unknown_provider_xyz"));
        assert!(mgr.resolve_for_provider("unknown_provider_xyz").is_none());
    }
}
