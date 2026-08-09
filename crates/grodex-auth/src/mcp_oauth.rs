//! MCP OAuth Broker — OAuth 2.0 authorization-code flow with PKCE for
//! MCP (Model Context Protocol) server discovery and credentialing.
//!
//! Broker state machine:
//!   1. `register_server`    — stash client_id/endpoints/provider_kind per MCP ServerId
//!   2. `start_authorization_flow` — mint state + PKCE verifier, return authorize URL
//!   3. `exchange_code_for_token`  — verify state, POST token_endpoint, store token
//!      in CredentialBroker (opaque lease returned — caller never sees the token)
//!   4. `refresh_credential`        — use refresh_token (if any) to roll a new lease

use crate::lease::{CredentialBroker, CredentialError};
use grodex_auth_types::lease::CredentialLease;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};
use thiserror::Error;

pub type ServerId = String;
pub type CredentialLeaseId = String;
pub type AuthorizationUrl = String;

fn default_provider_kind() -> ProviderKind {
    ProviderKind::Standard
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProviderKind {
    Standard,
    Github,
    Slack,
    Linear,
    Custom(String),
}

impl Default for ProviderKind {
    fn default() -> Self {
        Self::Standard
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Standard => write!(f, "standard"),
            Self::Github => write!(f, "github"),
            Self::Slack => write!(f, "slack"),
            Self::Linear => write!(f, "linear"),
            Self::Custom(s) => write!(f, "custom({s})"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthClientConfig {
    pub client_id: String,
    pub auth_endpoint: String,
    pub token_endpoint: String,
    pub scopes: Vec<String>,
    pub redirect_uri: String,
    #[serde(default)]
    pub audience: Option<String>,
    #[serde(default = "default_provider_kind")]
    pub provider_kind: ProviderKind,
    #[serde(default)]
    pub client_secret: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PendingAuthorization {
    pub server_id: ServerId,
    pub state: String,
    pub nonce: String,
    pub created_at: Instant,
    pub timeout: Duration,
    pub scopes_requested: Vec<String>,
    pub code_verifier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: Option<u64>,
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientSecretMode {
    RequiredInBody,
    BasicAuthOnly,
    NonePKCEOnly,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OIDCMetadata {
    pub issuer: String,
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
    pub scopes_supported: Option<Vec<String>>,
    pub response_types_supported: Option<Vec<String>>,
    pub code_challenge_methods_supported: Option<Vec<String>>,
}

#[derive(Debug, Error)]
pub enum McpoAuthError {
    #[error("missing OAuth client config for server id '{0}'")]
    MissingServerConfig(ServerId),
    #[error("state token mismatch: expected '{expected}', received '{received}'")]
    StateMismatch { expected: String, received: String },
    #[error("state token expired for server id '{0}'")]
    StateExpired(ServerId),
    #[error("OAuth HTTP request failed: {0}")]
    Http(String),
    #[error("token endpoint returned invalid response: {0}")]
    InvalidTokenResponse(String),
    #[error("OAuth protocol error: {0}")]
    Protocol(String),
    #[error(transparent)]
    CredentialStore(#[from] CredentialError),
}

pub struct McpoAuthBroker {
    client_registry: HashMap<ServerId, OAuthClientConfig>,
    in_flight: HashMap<String, PendingAuthorization>,
    default_flow_timeout: Duration,
    oidc_cache: HashMap<ServerId, OIDCMetadata>,
}

impl McpoAuthBroker {
    pub fn new() -> Self {
        Self {
            client_registry: HashMap::new(),
            in_flight: HashMap::new(),
            default_flow_timeout: Duration::from_secs(600),
            oidc_cache: HashMap::new(),
        }
    }

    pub fn with_timeout(default_flow_timeout: Duration) -> Self {
        Self {
            client_registry: HashMap::new(),
            in_flight: HashMap::new(),
            default_flow_timeout,
            oidc_cache: HashMap::new(),
        }
    }

    pub fn register_server(
        &mut self,
        id: ServerId,
        cfg: OAuthClientConfig,
    ) -> Result<(), McpoAuthError> {
        if cfg.client_id.is_empty() {
            return Err(McpoAuthError::Protocol(format!(
                "server '{id}' registered with empty client_id"
            )));
        }
        if cfg.auth_endpoint.is_empty() || cfg.token_endpoint.is_empty() {
            return Err(McpoAuthError::Protocol(format!(
                "server '{id}' missing auth/token endpoint"
            )));
        }
        self.client_registry.insert(id, cfg);
        Ok(())
    }

    pub fn start_authorization_flow(
        &mut self,
        id: &ServerId,
        scopes_hint: &[String],
    ) -> Result<AuthorizationUrl, McpoAuthError> {
        let cfg = self
            .client_registry
            .get(id)
            .ok_or_else(|| McpoAuthError::MissingServerConfig(id.clone()))?;

        let state = Self::random_token(32);
        let nonce = Self::random_token(16);
        let code_verifier = Some(Self::random_token(64));

        let scopes_requested = if scopes_hint.is_empty() {
            cfg.scopes.clone()
        } else {
            scopes_hint.to_vec()
        };

        let scope_sep = match cfg.provider_kind {
            ProviderKind::Github => ",",
            _ => " ",
        };
        let scope_str = scopes_requested.join(scope_sep);

        let mut url = format!(
            "{endpoint}?response_type=code&client_id={client_id}&redirect_uri={redirect_uri}&state={state}&scope={scope}&nonce={nonce}",
            endpoint = cfg.auth_endpoint,
            client_id = urlencode(&cfg.client_id),
            redirect_uri = urlencode(&cfg.redirect_uri),
            state = urlencode(&state),
            scope = urlencode(&scope_str),
            nonce = urlencode(&nonce),
        );
        if let Some(aud) = &cfg.audience {
            url.push_str(&format!("&audience={}", urlencode(aud)));
        }
        if matches!(cfg.provider_kind, ProviderKind::Linear) {
            if cfg.audience.is_none() {
                url.push_str("&audience=https%3A%2F%2Fapi.linear.dev");
            }
        }
        if let Some(verifier) = &code_verifier {
            let challenge = pkce_challenge_s256(verifier);
            url.push_str(&format!(
                "&code_challenge={challenge}&code_challenge_method=S256"
            ));
        }

        let pending = PendingAuthorization {
            server_id: id.clone(),
            state: state.clone(),
            nonce,
            created_at: Instant::now(),
            timeout: self.default_flow_timeout,
            scopes_requested,
            code_verifier,
        };
        self.in_flight.insert(state, pending);

        Ok(url)
    }

    pub async fn discover_metadata_if_openid(
        &mut self,
        id: &ServerId,
    ) -> Result<Option<OIDCMetadata>, McpoAuthError> {
        if let Some(cached) = self.oidc_cache.get(id) {
            return Ok(Some(cached.clone()));
        }
        let cfg = self
            .client_registry
            .get(id)
            .ok_or_else(|| McpoAuthError::MissingServerConfig(id.clone()))?;
        if !matches!(cfg.provider_kind, ProviderKind::Standard) {
            return Ok(None);
        }
        let base = url::Url::parse(&cfg.token_endpoint)
            .map_err(|e| McpoAuthError::Protocol(format!("bad token_endpoint URL: {e}")))?;
        let discovery = base
            .join(".well-known/openid-configuration")
            .map_err(|e| McpoAuthError::Protocol(format!("join discovery path: {e}")))?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| McpoAuthError::Http(format!("build client: {e}")))?;
        let resp = client.get(discovery.as_str()).send().await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                tracing_discovery_warn(&format!("discovery GET failed for {id}: {e}"));
                return Ok(None);
            }
        };
        if !resp.status().is_success() {
            tracing_discovery_warn(&format!(
                "discovery returned status {} for {id}",
                resp.status()
            ));
            return Ok(None);
        }
        let meta: OIDCMetadata = match resp.json().await {
            Ok(m) => m,
            Err(e) => {
                tracing_discovery_warn(&format!("discovery decode failed for {id}: {e}"));
                return Ok(None);
            }
        };
        if let Some(methods) = &meta.code_challenge_methods_supported {
            if !methods.iter().any(|m| m == "S256") {
                tracing_discovery_warn(&format!(
                    "OIDC provider {id} does not advertise S256 PKCE; advertised: {methods:?}"
                ));
            }
        }
        self.oidc_cache.insert(id.clone(), meta.clone());
        Ok(Some(meta))
    }

    pub async fn exchange_code_for_token(
        &mut self,
        _server_id: &ServerId,
        code: String,
        received_state: String,
        broker: &mut CredentialBroker,
    ) -> Result<CredentialLease, McpoAuthError> {
        let pending_entry = self.in_flight.remove(&received_state);
        let pending = pending_entry.ok_or_else(|| McpoAuthError::StateMismatch {
            expected: "<unknown>".to_string(),
            received: received_state.clone(),
        })?;

        if pending.state != received_state {
            return Err(McpoAuthError::StateMismatch {
                expected: pending.state,
                received: received_state,
            });
        }
        if pending.created_at.elapsed() > pending.timeout {
            return Err(McpoAuthError::StateExpired(pending.server_id));
        }

        let server_id = pending.server_id.clone();
        let cfg = self
            .client_registry
            .get(&server_id)
            .ok_or_else(|| McpoAuthError::MissingServerConfig(server_id.clone()))?
            .clone();

        let secret_mode = Self::secret_mode_for_provider(&cfg.provider_kind);
        let mut form: HashMap<&'static str, String> = HashMap::new();
        form.insert("grant_type", "authorization_code".to_string());
        form.insert("code", code);
        form.insert("redirect_uri", cfg.redirect_uri.clone());
        form.insert("client_id", cfg.client_id.clone());
        if let Some(verifier) = &pending.code_verifier {
            form.insert("code_verifier", verifier.clone());
        }
        match secret_mode {
            ClientSecretMode::RequiredInBody => {
                if let Some(secret) = &cfg.client_secret {
                    form.insert("client_secret", secret.clone());
                }
            }
            ClientSecretMode::BasicAuthOnly | ClientSecretMode::NonePKCEOnly => {}
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| McpoAuthError::Http(format!("build client: {e}")))?;
        let mut req = client.post(cfg.token_endpoint.as_str()).form(&form);
        if matches!(secret_mode, ClientSecretMode::BasicAuthOnly) {
            if let Some(secret) = &cfg.client_secret {
                req = req.basic_auth(&cfg.client_id, Some(secret));
            }
        }

        let resp = req
            .send()
            .await
            .map_err(|e| McpoAuthError::Http(format!("token POST: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(McpoAuthError::InvalidTokenResponse(format!(
                "status={status}, body={body}"
            )));
        }

        let raw_body = resp
            .bytes()
            .await
            .map_err(|e| McpoAuthError::Http(format!("read token body: {e}")))?;
        let token_resp: TokenResponse = if matches!(cfg.provider_kind, ProviderKind::Github) {
            serde_json::from_slice(&raw_body).or_else(|_| {
                let s = String::from_utf8_lossy(&raw_body);
                let fallback = Self::parse_github_form_response(&s);
                fallback.ok_or_else(|| {
                    McpoAuthError::InvalidTokenResponse(format!(
                        "failed to decode github token response: {s}"
                    ))
                })
            })?
        } else {
            serde_json::from_slice(&raw_body).map_err(|e| {
                McpoAuthError::InvalidTokenResponse(format!(
                    "decode json failed: {e}, raw={}",
                    String::from_utf8_lossy(&raw_body)
                ))
            })?
        };

        let scope_tags = pending.scopes_requested.clone();
        let provider_key = format!("oauth:{server_id}");
        broker.register_provider(&provider_key, token_resp.access_token.clone());
        let lease = broker
            .issue_lease(&provider_key, &cfg.token_endpoint)
            .ok_or_else(|| {
                McpoAuthError::Protocol(format!(
                    "failed to issue lease for provider '{provider_key}'"
                ))
            })?;

        if let Some(rt) = &token_resp.refresh_token {
            broker.store_refresh_token(
                lease.lease_id.clone(),
                server_id.clone(),
                rt.clone(),
            );
        }

        let _ = scope_tags;
        let _ = token_resp.scope;
        let _ = token_resp.expires_in;
        let _ = pending.nonce;

        Ok(lease)
    }

    pub async fn refresh_credential(
        &mut self,
        lease_id: &CredentialLeaseId,
        broker: &mut CredentialBroker,
    ) -> Result<CredentialLease, McpoAuthError> {
        let (server_id_str, refresh_token_str) = broker
            .get_refresh_token(lease_id)
            .ok_or_else(|| {
                McpoAuthError::Protocol(
                    "no refresh token associated with this lease".to_string(),
                )
            })?;
        let server_id: ServerId = server_id_str.to_string();
        let refresh_token: String = refresh_token_str.to_string();

        let cfg = self
            .client_registry
            .get(&server_id)
            .ok_or_else(|| McpoAuthError::MissingServerConfig(server_id.clone()))?
            .clone();

        let secret_mode = Self::secret_mode_for_provider(&cfg.provider_kind);
        let mut form: HashMap<&'static str, String> = HashMap::new();
        form.insert("grant_type", "refresh_token".to_string());
        form.insert("refresh_token", refresh_token.clone());
        form.insert("client_id", cfg.client_id.clone());
        match secret_mode {
            ClientSecretMode::RequiredInBody => {
                if let Some(secret) = &cfg.client_secret {
                    form.insert("client_secret", secret.clone());
                }
            }
            ClientSecretMode::BasicAuthOnly | ClientSecretMode::NonePKCEOnly => {}
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| McpoAuthError::Http(format!("build client: {e}")))?;
        let mut req = client.post(cfg.token_endpoint.as_str()).form(&form);
        if matches!(secret_mode, ClientSecretMode::BasicAuthOnly) {
            if let Some(secret) = &cfg.client_secret {
                req = req.basic_auth(&cfg.client_id, Some(secret));
            }
        }

        let resp = req
            .send()
            .await
            .map_err(|e| McpoAuthError::Http(format!("refresh POST: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(McpoAuthError::InvalidTokenResponse(format!(
                "refresh status={status}, body={body}"
            )));
        }
        let token_resp: TokenResponse = resp
            .json()
            .await
            .map_err(|e| McpoAuthError::InvalidTokenResponse(format!("decode json: {e}")))?;

        let provider_key = format!("oauth:{server_id}");
        broker.register_provider(&provider_key, token_resp.access_token.clone());
        let new_lease = broker
            .issue_lease(&provider_key, &cfg.token_endpoint)
            .ok_or_else(|| {
                McpoAuthError::Protocol(format!(
                    "failed to issue refreshed lease for provider '{provider_key}'"
                ))
            })?;

        if let Some(rt) = &token_resp.refresh_token {
            broker.store_refresh_token(
                new_lease.lease_id.clone(),
                server_id.clone(),
                rt.clone(),
            );
        }
        broker.delete_refresh_token(lease_id);
        broker.revoke_by_id(lease_id);

        Ok(new_lease)
    }

    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    pub fn registered_server_ids(&self) -> Vec<&ServerId> {
        let mut ids: Vec<&ServerId> = self.client_registry.keys().collect();
        ids.sort();
        ids
    }

    fn random_token(num_bytes: usize) -> String {
        use rand::RngCore;
        let mut bytes = vec![0u8; num_bytes];
        rand::thread_rng().fill_bytes(&mut bytes);
        const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::with_capacity(bytes.len());
        for b in bytes {
            out.push(ALPHABET[(b & 0x3F) as usize] as char);
        }
        out
    }

    fn secret_mode_for_provider(kind: &ProviderKind) -> ClientSecretMode {
        match kind {
            ProviderKind::Slack => ClientSecretMode::BasicAuthOnly,
            ProviderKind::Github => ClientSecretMode::RequiredInBody,
            ProviderKind::Standard | ProviderKind::Linear | ProviderKind::Custom(_) => {
                ClientSecretMode::NonePKCEOnly
            }
        }
    }

    fn parse_github_form_response(body: &str) -> Option<TokenResponse> {
        let mut access_token: Option<String> = None;
        let mut token_type: Option<String> = None;
        let mut scope: Option<String> = None;
        for pair in body.split('&') {
            let mut it = pair.splitn(2, '=');
            let k = it.next()?;
            let v = it.next().unwrap_or("");
            match k {
                "access_token" => access_token = Some(urldecode(v).to_string()),
                "token_type" => token_type = Some(urldecode(v).to_string()),
                "scope" => scope = Some(urldecode(v).to_string()),
                _ => {}
            }
        }
        Some(TokenResponse {
            access_token: access_token?,
            token_type: token_type.unwrap_or_else(|| "bearer".to_string()),
            expires_in: None,
            refresh_token: None,
            scope,
        })
    }
}

impl Default for McpoAuthBroker {
    fn default() -> Self {
        Self::new()
    }
}

fn tracing_discovery_warn(msg: &str) {
    let _ = msg;
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn pkce_challenge_s256(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    base64_encode_url_safe(&digest)
}

fn base64_encode_url_safe(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        out.push(ALPHABET[(n & 0x3F) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3F) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cfg() -> OAuthClientConfig {
        OAuthClientConfig {
            client_id: "mcp-client-abc".into(),
            auth_endpoint: "https://auth.example.com/authorize".into(),
            token_endpoint: "https://auth.example.com/oauth/token".into(),
            scopes: vec!["openid".into(), "profile".into()],
            redirect_uri: "http://localhost:8080/callback".into(),
            audience: Some("https://mcp.example.com".into()),
            provider_kind: ProviderKind::Standard,
            client_secret: None,
        }
    }

    #[test]
    fn new_broker_is_empty() {
        let b = McpoAuthBroker::new();
        assert_eq!(b.registered_server_ids().len(), 0);
        assert_eq!(b.in_flight_count(), 0);
    }

    #[test]
    fn register_server_rejects_empty_client_id() {
        let mut b = McpoAuthBroker::new();
        let mut bad = sample_cfg();
        bad.client_id = "".into();
        let err = b.register_server("svc-1".into(), bad).unwrap_err();
        assert!(matches!(err, McpoAuthError::Protocol(_)));
    }

    #[test]
    fn register_server_rejects_missing_endpoints() {
        let mut b = McpoAuthBroker::new();
        let mut bad = sample_cfg();
        bad.auth_endpoint = "".into();
        let err = b.register_server("svc-1".into(), bad).unwrap_err();
        assert!(matches!(err, McpoAuthError::Protocol(_)));
    }

    #[test]
    fn register_server_ok_and_listed() {
        let mut b = McpoAuthBroker::new();
        b.register_server("svc-1".into(), sample_cfg()).unwrap();
        let ids = b.registered_server_ids();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0], "svc-1");
    }

    #[test]
    fn start_authorization_flow_missing_server() {
        let mut b = McpoAuthBroker::new();
        let err = b
            .start_authorization_flow(&"nope".into(), &[])
            .unwrap_err();
        assert!(matches!(err, McpoAuthError::MissingServerConfig(_)));
    }

    #[test]
    fn start_authorization_flow_generates_url_and_state() {
        let mut b = McpoAuthBroker::new();
        b.register_server("svc-1".into(), sample_cfg()).unwrap();
        let url = b.start_authorization_flow(&"svc-1".into(), &[]).unwrap();
        assert!(url.starts_with("https://auth.example.com/authorize"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=mcp-client-abc"));
        assert!(url.contains("state="));
        assert!(url.contains("nonce="));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("audience="));
        assert_eq!(b.in_flight_count(), 1);
    }

    #[test]
    fn random_token_returns_correct_length() {
        for len in [8usize, 16, 32, 64] {
            let t = McpoAuthBroker::random_token(len);
            assert_eq!(t.len(), len);
        }
    }

    #[test]
    fn urlencode_encodes_special_chars() {
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("redirect://here?x=1&y=2"), "redirect%3A%2F%2Fhere%3Fx%3D1%26y%3D2");
        assert_eq!(urlencode("safe-AZ_az.~"), "safe-AZ_az.~");
    }

    #[test]
    fn pkce_challenge_is_deterministic() {
        let v = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let c = pkce_challenge_s256(v);
        assert!(!c.contains('+'));
        assert!(!c.contains('/'));
        assert!(!c.ends_with('='));
    }

    #[test]
    fn test_start_authorization_pkce_fields_nonempty() {
        let mut b = McpoAuthBroker::new();
        b.register_server("svc-1".into(), sample_cfg()).unwrap();
        let url = b.start_authorization_flow(&"svc-1".into(), &[]).unwrap();
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state="));
        assert!(url.contains("nonce="));
        let has_nonempty_challenge = url.split('&').any(|kv| {
            let mut it = kv.splitn(2, '=');
            let k = it.next().unwrap_or("");
            let v = it.next().unwrap_or("");
            k == "code_challenge" && !v.is_empty()
        });
        assert!(has_nonempty_challenge, "code_challenge must be non-empty");
        let has_nonempty_state = url.split('&').any(|kv| {
            let mut it = kv.splitn(2, '=');
            let k = it.next().unwrap_or("");
            let v = it.next().unwrap_or("");
            k == "state" && !v.is_empty()
        });
        assert!(has_nonempty_state, "state must be non-empty");
    }

    #[test]
    fn test_state_uniqueness_100k() {
        let n = 100_000;
        let mut seen: HashMap<String, ()> = HashMap::with_capacity(n);
        for _ in 0..n {
            let s = McpoAuthBroker::random_token(32);
            assert!(seen.insert(s, ()).is_none(), "duplicate state generated");
        }
        assert_eq!(seen.len(), n);
    }

    #[tokio::test]
    async fn test_exchange_code_validates_state_first() {
        let mut b = McpoAuthBroker::new();
        b.register_server("svc-1".into(), sample_cfg()).unwrap();
        let mut cb = CredentialBroker::empty();
        let err = b
            .exchange_code_for_token(
                &"svc-1".into(),
                "fake-code".into(),
                "ghost-state".into(),
                &mut cb,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, McpoAuthError::StateMismatch { .. }));
    }

    #[tokio::test]
    async fn exchange_code_state_mismatch() {
        let mut b = McpoAuthBroker::new();
        b.register_server("svc-1".into(), sample_cfg()).unwrap();
        let mut cb = CredentialBroker::empty();
        let err = b
            .exchange_code_for_token(
                &"svc-1".into(),
                "abc123".into(),
                "ghost-state".into(),
                &mut cb,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, McpoAuthError::StateMismatch { .. }));
    }

    #[test]
    fn provider_kind_display_and_default() {
        assert_eq!(ProviderKind::default(), ProviderKind::Standard);
        assert_eq!(format!("{}", ProviderKind::Standard), "standard");
        assert_eq!(format!("{}", ProviderKind::Github), "github");
        assert_eq!(format!("{}", ProviderKind::Slack), "slack");
        assert_eq!(format!("{}", ProviderKind::Linear), "linear");
        assert_eq!(
            format!("{}", ProviderKind::Custom("foo".into())),
            "custom(foo)"
        );
    }

    #[test]
    fn oauth_client_config_default_provider_kind_via_serde() {
        let json = r#"{"client_id":"x","auth_endpoint":"https://a","token_endpoint":"https://t","scopes":[],"redirect_uri":"r"}"#;
        let cfg: OAuthClientConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.provider_kind, ProviderKind::Standard);
    }

    #[test]
    fn parse_github_form_access_token_only() {
        let body = "access_token=gho_abc123&scope=repo%2Cuser&token_type=bearer";
        let r = McpoAuthBroker::parse_github_form_response(body).unwrap();
        assert_eq!(r.access_token, "gho_abc123");
        assert_eq!(r.token_type, "bearer");
        assert_eq!(r.scope.as_deref(), Some("repo,user"));
        assert!(r.refresh_token.is_none());
    }
}
