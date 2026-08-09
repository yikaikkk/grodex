//! SamplingError — production error taxonomy with rich classifier methods.
//!
//! Follows Grok's `SamplingError` pattern: 11 variants, each carrying enough
//! structured context for the retry loop and failover logic to make decisions
//! without inspecting raw HTTP status codes or error strings.
//!
//! Key design decisions (from Grok's codebase):
//!   - 401 is auth error; 403 is NOT (authenticated but not permitted)
//!   - Serialization errors are NEVER retryable (deterministic parse failure)
//!   - IdleTimeout is NOT retryable (model stuck; replay would stall again)
//!   - Context-length errors are detected via message patterns, not status codes
//!   - `x-should-retry: false` header overrides status-based logic

use std::time::Duration;

// ── Supporting types ───────────────────────────────────────────────

/// Transport error classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportSource {
    Timeout,
    Connect,
    Request,
    Body,
    Tls,
    Other,
}

impl std::fmt::Display for TransportSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "timeout"),
            Self::Connect => write!(f, "connect"),
            Self::Request => write!(f, "request"),
            Self::Body => write!(f, "body"),
            Self::Tls => write!(f, "tls"),
            Self::Other => write!(f, "other"),
        }
    }
}

impl std::error::Error for TransportSource {}

/// Context captured when a response is completed but empty.
#[derive(Debug, Clone)]
pub struct EmptyResponseContext {
    /// Why the response was empty.
    pub reason: String,
    /// Whether reasoning items were present.
    pub had_reasoning: bool,
    /// Total character count of visible output.
    pub content_len: usize,
    /// Number of tool calls in the response.
    pub tool_call_count: usize,
    /// Stop reason from the model, if any.
    pub finish_reason: Option<String>,
    /// Total tokens consumed.
    pub total_tokens: Option<u64>,
}

// ── Main error enum ────────────────────────────────────────────────

/// Unified sampling error. Every failure path through the sampler
/// produces one of these variants.
///
#[derive(Debug, Clone, thiserror::Error)]
pub enum SamplingError {
    /// Authentication failure (401). NOT 403 — that's a permission error.
    #[error("authentication failed: {message}")]
    Auth { message: String, status_code: Option<u16> },

    /// Network-level transport error.
    #[error("transport error: {message}")]
    Transport {
        message: String,
        source: Option<TransportSource>,
    },

    /// API-level error with HTTP status code.
    #[error("API error ({status}): {message}")]
    Api {
        status: u16,
        message: String,
        /// Server-provided Retry-After hint.
        retry_after_secs: Option<u64>,
        /// Server `x-should-retry` header value.
        should_retry: Option<bool>,
    },

    /// JSON serialization/deserialization failure. NEVER retryable.
    #[error("serialization error: {message}")]
    Serialization { message: String },

    /// Rate limit (429) with retry hint.
    #[error("rate limited (retry after {retry_after_secs:?}s)")]
    RateLimited {
        retry_after_secs: Option<u64>,
        message: String,
    },

    /// Stream stalled — no chunks received within the idle timeout.
    #[error("idle timeout after {elapsed_secs}s")]
    IdleTimeout { elapsed_secs: u64 },

    /// Response completed but produced no visible output.
    #[error("empty response: {context:?}")]
    EmptyResponse { context: EmptyResponseContext },

    /// Model output truncated by max_tokens.
    #[error("max tokens truncation")]
    MaxTokensTruncation,

    /// Tool call was incomplete when stream ended.
    #[error("incomplete tool call: {call_id}")]
    IncompleteToolCall { call_id: String },

    /// Context length exceeded the model window.
    #[error("context length exceeded: {message}")]
    ContextLengthExceeded { message: String },

    /// Internal/unexpected error.
    #[error("internal error: {message}")]
    Internal { message: String },
}

// ── Classifier methods ─────────────────────────────────────────────

impl SamplingError {
    // ── Core classification ──────────────────────────────────────

    /// Whether this error is worth retrying.
    ///
    /// Retryable: Transport, 5xx API, RateLimited, EmptyResponse.
    /// NOT retryable: Auth, 4xx client errors, Serialization, IdleTimeout,
    ///                MaxTokensTruncation, ContextLengthExceeded, IncompleteToolCall.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport { .. } => true,
            Self::Api { status, message, .. } => {
                // 5xx: transient server error.
                if *status >= 500 {
                    // Context-length overflow can surface as 500 from some providers.
                    if is_context_length_message(message) {
                        return false;
                    }
                    return true;
                }
                false
            }
            Self::RateLimited { .. } => true,
            Self::EmptyResponse { .. } => true,
            Self::Auth { .. } => false,
            Self::Serialization { .. } => false,
            Self::IdleTimeout { .. } => false,
            Self::MaxTokensTruncation => false,
            Self::IncompleteToolCall { .. } => false,
            Self::ContextLengthExceeded { .. } => false,
            Self::Internal { .. } => false,
        }
    }

    /// Whether this error qualifies for transparent provider failover.
    ///
    /// Only Transport, 5xx, and RateLimited. Auth errors and client errors
    /// would fail the same way on any provider — switching helps nothing.
    pub fn is_failover_eligible(&self) -> bool {
        match self {
            Self::Transport { .. } => true,
            Self::Api { status, .. } => *status >= 500,
            Self::RateLimited { .. } => true,
            _ => false,
        }
    }

    /// Whether the error indicates a broken *request*, not a broken *server*.
    pub fn is_client_error(&self) -> bool {
        match self {
            Self::Serialization { .. } | Self::IncompleteToolCall { .. } | Self::ContextLengthExceeded { .. } => true,
            Self::Api { status, .. } => *status < 500 && *status != 429,
            _ => false,
        }
    }

    // ── Specific checks ──────────────────────────────────────────

    /// Whether this is an authentication error. 401 only — NOT 403.
    /// 403 = authenticated but forbidden (content safety, policy denial).
    /// Retrying 403 after token refresh is pointless and destructive.
    pub fn is_auth_error(&self) -> bool {
        matches!(
            self,
            Self::Auth {
                status_code: Some(401),
                ..
            }
        ) || matches!(self, Self::Api { status: 401, .. })
    }

    /// Whether this is a rate-limiting error.
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Api { status: 429, .. })
    }

    /// Whether this is a context-length error (cannot fit input in window).
    /// Uses message pattern matching, not status codes, because providers
    /// surface this differently (400, 500, stream error, etc.).
    pub fn is_context_length_error(&self) -> bool {
        match self {
            Self::ContextLengthExceeded { .. } => true,
            Self::Api { message, .. } => is_context_length_message(message),
            _ => false,
        }
    }

    /// Whether the server explicitly asked us not to retry.
    pub fn should_retry_header(&self) -> Option<bool> {
        match self {
            Self::Api { should_retry, .. } => *should_retry,
            _ => None,
        }
    }

    /// The `Retry-After` duration from the server, if any.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Api { retry_after_secs, .. } | Self::RateLimited { retry_after_secs, .. } => {
                retry_after_secs.map(Duration::from_secs)
            }
            _ => None,
        }
    }

    /// Whether the request payload was too large (413).
    pub fn is_payload_too_large(&self) -> bool {
        matches!(self, Self::Api { status: 413, .. })
    }

    /// Short human-readable error kind label for telemetry.
    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::Auth { .. } => "auth",
            Self::Transport { .. } => "transport",
            Self::Api { status, .. } if *status >= 500 => "server_error",
            Self::Api { .. } => "api_error",
            Self::Serialization { .. } => "serialization",
            Self::RateLimited { .. } => "rate_limited",
            Self::IdleTimeout { .. } => "idle_timeout",
            Self::EmptyResponse { .. } => "empty_response",
            Self::MaxTokensTruncation => "max_tokens",
            Self::IncompleteToolCall { .. } => "incomplete_tool_call",
            Self::ContextLengthExceeded { .. } => "context_length",
            Self::Internal { .. } => "internal",
        }
    }
}

// ── Constructor helpers ────────────────────────────────────────────

impl SamplingError {
    /// Create an auth error from a status code and message.
    pub fn auth(status_code: Option<u16>, message: impl Into<String>) -> Self {
        Self::Auth {
            message: message.into(),
            status_code,
        }
    }

    /// Create a transport error.
    pub fn transport(message: impl Into<String>, source: Option<TransportSource>) -> Self {
        Self::Transport {
            message: message.into(),
            source,
        }
    }

    /// Create an API error.
    pub fn api(
        status: u16,
        message: impl Into<String>,
        retry_after_secs: Option<u64>,
        should_retry: Option<bool>,
    ) -> Self {
        Self::Api {
            status,
            message: message.into(),
            retry_after_secs,
            should_retry,
        }
    }

    /// Create a serialization error. ALWAYS non-retryable.
    pub fn serialization(message: impl Into<String>) -> Self {
        Self::Serialization {
            message: message.into(),
        }
    }

    /// Create a rate-limit error.
    pub fn rate_limited(retry_after_secs: Option<u64>, message: impl Into<String>) -> Self {
        Self::RateLimited {
            retry_after_secs,
            message: message.into(),
        }
    }

    /// Create an internal error.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }
}

// ── Context-length message detection ───────────────────────────────

/// Pattern match against provider error messages to detect context-length
/// overflow. Following Grok's `is_context_length_error()` free function.
pub fn is_context_length_message(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("too long for this model")
        || lower.contains("prompt is too long")
        || lower.contains("maximum prompt length")
        || lower.contains("maximum context length")
        || lower.contains("context_length_exceeded")
        || lower.contains("reduce the length")
        || lower.contains("input is too long")
        || (lower.contains("current message") && lower.contains("exceeds budget"))
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Classification matrix ───────────────────────────────────

    #[test]
    fn transport_is_retryable() {
        let err = SamplingError::transport("connection reset", Some(TransportSource::Connect));
        assert!(err.is_retryable());
        assert!(err.is_failover_eligible());
        assert!(!err.is_client_error());
    }

    #[test]
    fn server_error_is_retryable() {
        let err = SamplingError::api(502, "bad gateway", None, None);
        assert!(err.is_retryable());
        assert!(err.is_failover_eligible());
    }

    #[test]
    fn rate_limited_is_retryable() {
        let err = SamplingError::rate_limited(Some(30), "too many requests");
        assert!(err.is_retryable());
        assert!(err.is_failover_eligible());
    }

    #[test]
    fn auth_is_not_retryable() {
        let err = SamplingError::auth(Some(401), "invalid key");
        assert!(!err.is_retryable());
        assert!(!err.is_failover_eligible());
        assert!(err.is_auth_error());
    }

    #[test]
    fn forbidden_is_not_auth_error() {
        // 403 is NOT auth — it's a permission error. Refreshing tokens won't help.
        let err = SamplingError::api(403, "forbidden", None, None);
        assert!(!err.is_auth_error());
        assert!(!err.is_retryable());
        assert!(err.is_client_error());
    }

    #[test]
    fn serialization_is_never_retryable() {
        let err = SamplingError::serialization("invalid JSON at line 5");
        assert!(!err.is_retryable());
        assert!(!err.is_failover_eligible());
        assert!(err.is_client_error());
    }

    #[test]
    fn idle_timeout_is_not_retryable() {
        let err = SamplingError::IdleTimeout { elapsed_secs: 30 };
        assert!(!err.is_retryable());
    }

    #[test]
    fn client_error_4xx_is_not_retryable() {
        let err = SamplingError::api(400, "bad request", None, None);
        assert!(!err.is_retryable());
        assert!(err.is_client_error());
    }

    #[test]
    fn context_length_500_is_not_retryable() {
        // Some providers return 500 for context overflow.
        let err = SamplingError::api(500, "prompt is too long for this model", None, None);
        assert!(!err.is_retryable()); // caught by is_context_length_message
    }

    #[test]
    fn context_length_400_is_not_retryable() {
        let err = SamplingError::api(400, "maximum context length exceeded", None, None);
        assert!(err.is_client_error());
        assert!(!err.is_retryable());
    }

    #[test]
    fn should_retry_header_overrides() {
        let err = SamplingError::api(500, "error", None, Some(false));
        assert_eq!(err.should_retry_header(), Some(false));
    }

    #[test]
    fn retry_after_extraction() {
        let err = SamplingError::rate_limited(Some(60), "wait");
        assert_eq!(err.retry_after(), Some(Duration::from_secs(60)));
    }

    #[test]
    fn empty_response_is_retryable() {
        let err = SamplingError::EmptyResponse {
            context: EmptyResponseContext {
                reason: "reasoning only".into(),
                had_reasoning: true,
                content_len: 0,
                tool_call_count: 0,
                finish_reason: None,
                total_tokens: Some(50),
            },
        };
        assert!(err.is_retryable());
    }

    // ── Context-length message detection ─────────────────────────

    #[test]
    fn detects_context_length_messages() {
        assert!(is_context_length_message("prompt is too long"));
        assert!(is_context_length_message("maximum context length exceeded"));
        assert!(is_context_length_message("context_length_exceeded"));
        assert!(is_context_length_message("input is too long for this model"));
        assert!(is_context_length_message("reduce the length of the prompt"));
    }

    #[test]
    fn context_length_negative_cases() {
        assert!(!is_context_length_message("internal server error"));
        assert!(!is_context_length_message("rate limit exceeded"));
    }

    // ── Kind labels ──────────────────────────────────────────────

    #[test]
    fn kind_labels_are_stable() {
        assert_eq!(SamplingError::auth(None, "x").kind_label(), "auth");
        assert_eq!(SamplingError::transport("x", None).kind_label(), "transport");
        assert_eq!(SamplingError::api(502, "x", None, None).kind_label(), "server_error");
        assert_eq!(SamplingError::rate_limited(None, "x").kind_label(), "rate_limited");
        assert_eq!(SamplingError::serialization("x").kind_label(), "serialization");
    }

    // ── Payload too large ────────────────────────────────────────

    #[test]
    fn payload_too_large_detection() {
        let err = SamplingError::api(413, "entity too large", None, None);
        assert!(err.is_payload_too_large());
    }
}
