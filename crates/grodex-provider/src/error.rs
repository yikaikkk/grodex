//! ProviderError — unified error taxonomy for all provider failures.
//!
//! Every failure path (transport, API, serialization, rate limit, timeout)
//! produces one of these. The error carries enough structured information
//! for the retry loop and failover logic to make decisions without
//! inspecting HTTP status codes or error strings.

/// Unified provider error.
///
/// Methods like `is_retryable()` and `is_failover_eligible()` encode the
/// retry decision matrix from Design Doc 14, Section 13.1 as pure functions
/// on this type. The retry loop calls these rather than matching on the enum.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderError {
    #[error("authentication failed: {message}")]
    Auth { message: String, status_code: Option<u16> },

    #[error("transport error: {message}")]
    Transport { message: String },

    #[error("API error ({status_code}): {message}")]
    Api {
        status_code: u16,
        message: String,
        retry_after_secs: Option<u64>,
    },

    #[error("serialization error: {message}")]
    Serialization { message: String },

    #[error("rate limited (retry after {retry_after_secs:?})")]
    RateLimited {
        retry_after_secs: Option<u64>,
        message: String,
    },

    #[error("idle timeout after {elapsed_secs}s")]
    IdleTimeout { elapsed_secs: u64 },

    #[error("empty response")]
    EmptyResponse { context: Option<String> },

    #[error("max tokens truncation")]
    MaxTokensTruncation,

    #[error("context mapping failed: {reason}")]
    ContextMappingFailed { reason: String },

    #[error("incomplete tool call: {call_id}")]
    IncompleteToolCall { call_id: String },

    #[error("internal error: {0}")]
    Internal(String),
}

impl ProviderError {
    /// Whether a retry is worth attempting.
    ///
    /// Returns true for transient failures (transport, 5xx, rate limits)
    /// and false for structural failures (auth, 4xx client errors, serialization).
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport { .. } => true,
            Self::Api {
                status_code, message, ..
            } => {
                // 429 is handled by the RateLimited variant.
                // 5xx = transient server error.
                // Context length overflow (typically 400) is NOT retryable.
                if *status_code >= 500 {
                    return true;
                }
                let lower = message.to_lowercase();
                if lower.contains("context") && lower.contains("length") {
                    return false;
                }
                false
            }
            Self::RateLimited { .. } => true,
            Self::EmptyResponse { .. } => true,
            Self::IdleTimeout { .. } => false,
            Self::MaxTokensTruncation => false,
            Self::Auth { .. } => false,
            Self::Serialization { .. } => false,
            Self::ContextMappingFailed { .. } => false,
            Self::IncompleteToolCall { .. } => false,
            Self::Internal(_) => false,
        }
    }

    /// Whether this error is eligible for transparent provider failover.
    ///
    /// Only transport errors, 5xx, and rate limits qualify. Auth errors,
    /// client errors, and structural failures do not — they would fail the
    /// same way on any provider.
    pub fn is_failover_eligible(&self) -> bool {
        match self {
            Self::Transport { .. } => true,
            Self::Api { status_code, .. } => *status_code >= 500,
            Self::RateLimited { .. } => true,
            _ => false,
        }
    }

    /// Whether this error indicates a broken request (not a broken server).
    pub fn is_client_error(&self) -> bool {
        match self {
            Self::ContextMappingFailed { .. } | Self::Serialization { .. } | Self::IncompleteToolCall { .. } => true,
            Self::Api { status_code, .. } => *status_code < 500 && *status_code != 429,
            _ => false,
        }
    }
}

impl From<anyhow::Error> for ProviderError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(format!("{e:#}"))
    }
}
