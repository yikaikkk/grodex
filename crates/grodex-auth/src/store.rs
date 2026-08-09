//! CredentialStore — loads credentials from environment variables and config files.

use grodex_auth_types::account::{AccountDescriptor, AccountStatus, AuthMethod};
use std::collections::HashMap;

/// Stores credential metadata (not the secrets themselves).
///
/// Secrets are loaded from environment variables at request time
/// and never stored in memory longer than necessary.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    accounts: HashMap<String, AccountDescriptor>,
    /// Ephemeral: API keys loaded during discovery. Cleared after each resolution cycle in production.
    keys: HashMap<String, String>,
}

impl CredentialStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self {
            accounts: HashMap::new(),
            keys: HashMap::new(),
        }
    }

    /// Register an account.
    pub fn register(&mut self, account: AccountDescriptor) {
        self.accounts.insert(account.account_id.clone(), account);
    }

    /// Auto-discover accounts from environment variables.
    ///
    /// Looks for `<PREFIX>_API_KEY` env vars and registers accounts
    /// for each one found.
    ///
    /// Fail-soft: any environment variable whose name or value isn't
    /// valid UTF-8 is silently skipped (rather than panicking, which
    /// is what `std::env::vars()` would do on those entries via its
    /// internal `.to_str().unwrap()` inside `Iterator::next`).
    pub fn discover_from_env(&mut self, prefix: &str) {
        let iter = std::env::vars_os().filter_map(|(k_os, v_os)| {
            let k = k_os.into_string().ok()?;
            let v = v_os.into_string().ok()?;
            Some((k, v))
        });
        self.discover_from_iter(iter, prefix);
    }

    /// Discover from an explicit iterator of (key, value) pairs.
    /// Used for testing without mutating global env.
    pub fn discover_from_iter(&mut self, iter: impl IntoIterator<Item = (String, String)>, prefix: &str) {
        for (key, value) in iter {
            if let Some(provider) = key.strip_suffix("_API_KEY") {
                let provider_id = provider.to_lowercase();
                self.keys.insert(provider_id.clone(), value);
                if key.starts_with(prefix) || prefix.is_empty() {
                    let account = AccountDescriptor {
                        account_id: format!("env:{provider_id}"),
                        provider_id,
                        principal_display: format!("environment variable {key}"),
                        auth_method: AuthMethod::ApiKey,
                        tenant_id: None,
                        scopes: vec![],
                        audiences: vec![],
                        status: AccountStatus::Active,
                        metadata_generation: 1,
                    };
                    self.register(account);
                }
            }
        }
    }

    /// Resolve the actual API key for an account.
    pub fn resolve_key(&self, account_id: &str) -> Option<String> {
        let account = self.accounts.get(account_id)?;
        match account.auth_method {
            AuthMethod::ApiKey => {
                // Check in-memory store first (from discover), then env.
                self.keys.get(&account.provider_id).cloned().or_else(|| {
                    let env_key = format!("{}_API_KEY", account.provider_id.to_uppercase());
                    std::env::var(&env_key).ok()
                })
            }
            _ => None,
        }
    }

    /// Get an account by id.
    pub fn get(&self, account_id: &str) -> Option<&AccountDescriptor> {
        self.accounts.get(account_id)
    }

    /// List all accounts.
    pub fn list(&self) -> Vec<&AccountDescriptor> {
        self.accounts.values().collect()
    }

    /// Find an account by provider id.
    pub fn find_by_provider(&self, provider_id: &str) -> Option<&AccountDescriptor> {
        self.accounts.values().find(|a| a.provider_id == provider_id)
    }
}

impl Default for CredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_resolve() {
        let mut store = CredentialStore::new();
        store.register(AccountDescriptor {
            account_id: "test".into(),
            provider_id: "openai".into(),
            principal_display: "test".into(),
            auth_method: AuthMethod::ApiKey,
            tenant_id: None,
            scopes: vec![],
            audiences: vec![],
            status: AccountStatus::Active,
            metadata_generation: 1,
        });

        assert!(store.get("test").is_some());
    }

    #[test]
    fn find_by_provider() {
        let mut store = CredentialStore::new();
        store.register(AccountDescriptor {
            account_id: "acc1".into(),
            provider_id: "anthropic".into(),
            principal_display: "test".into(),
            auth_method: AuthMethod::ApiKey,
            tenant_id: None,
            scopes: vec![],
            audiences: vec![],
            status: AccountStatus::Active,
            metadata_generation: 1,
        });

        let found = store.find_by_provider("anthropic");
        assert!(found.is_some());
        assert!(store.find_by_provider("nonexistent").is_none());
    }
}
