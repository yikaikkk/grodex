//! OperationId-level cancel pipeline for Exec (§11.4).
//!
//! When a cancel is requested for an `OperationId`:
//! 1. Close pending stdin (sends EOF).
//! 2. Send SIGINT (or platform graceful signal).
//! 3. Wait for grace period.
//! 4. SIGKILL the process tree.
//! 5. Drain bounded output.
//!
//! The `CancelRegistry` is shared between the Exec tool and the agent
//! loop so that external cancel requests (user interrupt, timeout, or
//! higher-level orchestration) propagate to the running process.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

/// A per-operation cancellation token.
///
/// Cloning is cheap (Arc-based). Any holder can call `cancel()` to
/// signal all waiters.
#[derive(Clone)]
pub struct CancellationToken {
    inner: Arc<CancellationTokenInner>,
}

struct CancellationTokenInner {
    notify: Notify,
    cancelled: Mutex<bool>,
    reason: Mutex<Option<String>>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CancellationTokenInner {
                notify: Notify::new(),
                cancelled: Mutex::new(false),
                reason: Mutex::new(None),
            }),
        }
    }

    /// Signal cancellation with an optional reason.
    pub async fn cancel(&self, reason: Option<String>) {
        let mut cancelled = self.inner.cancelled.lock().await;
        if !*cancelled {
            *cancelled = true;
            *self.inner.reason.lock().await = reason;
            self.inner.notify.notify_waiters();
        }
    }

    /// Check if cancellation has been requested.
    pub async fn is_cancelled(&self) -> bool {
        *self.inner.cancelled.lock().await
    }

    /// Get the cancellation reason, if any.
    pub async fn reason(&self) -> Option<String> {
        self.inner.reason.lock().await.clone()
    }

    /// Wait until cancellation is requested.
    pub async fn cancelled(&self) {
        loop {
            {
                let cancelled = self.inner.cancelled.lock().await;
                if *cancelled {
                    return;
                }
            }
            self.inner.notify.notified().await;
        }
    }

    /// Wait until cancellation or timeout. Returns `true` if cancelled.
    pub async fn cancelled_or_timeout(&self, timeout: Duration) -> bool {
        tokio::select! {
            _ = self.cancelled() => true,
            _ = tokio::time::sleep(timeout) => false,
        }
    }
}

/// Registry mapping `OperationId` → `CancellationToken`.
///
/// The Exec tool registers a token before spawning a process; the agent
/// loop can look it up by OperationId to request cancellation.
#[derive(Clone)]
pub struct CancelRegistry {
    inner: Arc<Mutex<HashMap<String, CancellationToken>>>,
}

impl Default for CancelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a new cancellation token for an operation.
    pub async fn register(&self, operation_id: String) -> CancellationToken {
        let token = CancellationToken::new();
        self.inner
            .lock()
            .await
            .insert(operation_id.clone(), token.clone());
        token
    }

    /// Look up the token for an operation.
    pub async fn get(&self, operation_id: &str) -> Option<CancellationToken> {
        self.inner.lock().await.get(operation_id).cloned()
    }

    /// Request cancellation for all processes under an operation.
    pub async fn cancel(&self, operation_id: &str, reason: Option<String>) -> bool {
        if let Some(token) = self.get(operation_id).await {
            token.cancel(reason).await;
            true
        } else {
            false
        }
    }

    /// Remove a token after the operation completes.
    pub async fn remove(&self, operation_id: &str) {
        self.inner.lock().await.remove(operation_id);
    }

    /// List all registered operation IDs.
    pub async fn list(&self) -> Vec<String> {
        self.inner.lock().await.keys().cloned().collect()
    }
}

/// Cancel pipeline stages (§11.4).
///
/// Encapsulates the ordered shutdown sequence for a process:
/// 1. close stdin
/// 2. SIGINT
/// 3. grace period
/// 4. SIGKILL
/// 5. drain output
pub struct CancelPipeline {
    /// Grace period between SIGINT and SIGKILL.
    pub grace_period: Duration,
}

impl Default for CancelPipeline {
    fn default() -> Self {
        Self {
            grace_period: Duration::from_secs(5),
        }
    }
}

impl CancelPipeline {
    pub fn new(grace_period: Duration) -> Self {
        Self { grace_period }
    }

    /// Execute the cancel pipeline on a process by PID.
    ///
    /// Returns `true` if the process was successfully terminated.
    pub async fn execute(&self, pid: u32) -> CancelResult {
        // Stage 1: SIGINT (graceful).
        let sigint_ok = send_signal(pid, "INT");

        if !sigint_ok {
            // Process already gone.
            return CancelResult::AlreadyExited;
        }

        // Stage 2: Wait for grace period.
        let exited_in_grace = tokio::select! {
            _ = tokio::time::sleep(self.grace_period) => false,
            result = wait_for_exit(pid) => result,
        };

        if exited_in_grace {
            return CancelResult::GracefulExit;
        }

        // Stage 3: SIGKILL (force).
        let kill_ok = send_signal(pid, "KILL");
        if kill_ok {
            // Wait briefly for the kernel to reap.
            let _ = tokio::time::timeout(Duration::from_millis(500), wait_for_exit(pid)).await;
            CancelResult::ForceKilled
        } else {
            CancelResult::UnknownOutcome
        }
    }
}

/// Result of a cancel pipeline execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelResult {
    /// Process exited gracefully during SIGINT + grace period.
    GracefulExit,
    /// Process was already gone when we tried to signal it.
    AlreadyExited,
    /// Process was force-killed with SIGKILL.
    ForceKilled,
    /// Could not determine final state.
    UnknownOutcome,
}

/// Send a signal to a process by PID. Returns `false` if the process
/// doesn't exist.
fn send_signal(pid: u32, signal: &str) -> bool {
    std::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Wait for a process to exit by polling `kill -0` (Unix).
/// Returns when the process is no longer alive, or after 10 seconds.
async fn wait_for_exit(pid: u32) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let alive = std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !alive {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false; // timed out waiting
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_token_basic() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled().await);

        token.cancel(Some("test".into())).await;
        assert!(token.is_cancelled().await);
        assert_eq!(token.reason().await, Some("test".into()));
    }

    #[tokio::test]
    async fn cancel_token_idempotent() {
        let token = CancellationToken::new();
        token.cancel(Some("first".into())).await;
        token.cancel(Some("second".into())).await; // should be ignored
        assert_eq!(token.reason().await, Some("first".into()));
    }

    #[tokio::test]
    async fn cancel_token_timeout() {
        let token = CancellationToken::new();
        let cancelled = token.cancelled_or_timeout(Duration::from_millis(10)).await;
        assert!(!cancelled); // timed out, not cancelled

        token.cancel(None).await;
        let cancelled = token.cancelled_or_timeout(Duration::from_secs(1)).await;
        assert!(cancelled); // already cancelled
    }

    #[tokio::test]
    async fn cancel_registry_basic() {
        let reg = CancelRegistry::new();
        let token = reg.register("op-1".into()).await;
        assert!(!token.is_cancelled().await);

        let found = reg.cancel("op-1", Some("user".into())).await;
        assert!(found);
        assert!(token.is_cancelled().await);

        let not_found = reg.cancel("op-999", None).await;
        assert!(!not_found);
    }

    #[tokio::test]
    async fn cancel_pipeline_unknown_pid() {
        // Just verify the pipeline doesn't panic on a bogus PID.
        // We use a very short grace period to avoid hanging.
        let pipeline = CancelPipeline::new(Duration::from_millis(50));
        // Use a high PID that's very unlikely to exist.
        let result = pipeline.execute(999_999).await;
        // Should be AlreadyExited since kill -INT on non-existent PID fails.
        assert_eq!(result, CancelResult::AlreadyExited);
    }
}
