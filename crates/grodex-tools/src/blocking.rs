//! 阻塞 I/O 的 spawn_blocking 调度 helper（T3）。
//!
//! 把同步文件 I/O（整文件读取、SHA-256、原子写）从 async runtime
//! worker 移到专门的 blocking 线程池，并通过全局 Semaphore 限制并发
//! 阻塞任务数，防止大文件操作挤满 tokio runtime worker —— 后者会
//! 让流式事件、审批和 TUI 状态转发出现可观察的卡顿。

use grodex_core::error::GrodexError;
use std::sync::OnceLock;
use tokio::sync::Semaphore;

/// 阻塞 I/O 任务的并发上限。8 个并发足以覆盖典型 Turn 的多工具并行
/// 调用，同时避免大文件读取/哈希同时跑满 blocking 线程池。
const BLOCKING_IO_CONCURRENCY: usize = 8;

static BLOCKING_IO_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();

fn semaphore() -> &'static Semaphore {
    BLOCKING_IO_SEMAPHORE.get_or_init(|| Semaphore::new(BLOCKING_IO_CONCURRENCY))
}

/// 在 `spawn_blocking` 上运行同步 I/O 闭包，并通过全局 Semaphore 限制
/// 并发阻塞任务数。
///
/// 把大文件读取、SHA-256、原子写等阻塞操作包入此函数，可避免它们
/// 占用 tokio runtime worker。注意：仅改用 `tokio::fs` 并不能消除整
/// 文件读取与内存复制问题；需配合流式 reader（T1/T4）才能根治。
pub async fn run_blocking_io<F, T>(f: F) -> Result<T, GrodexError>
where
    F: FnOnce() -> Result<T, GrodexError> + Send + 'static,
    T: Send + 'static,
{
    let _permit = semaphore()
        .acquire()
        .await
        .map_err(|e| GrodexError::ToolExecution(format!("blocking-io semaphore closed: {e}")))?;
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| GrodexError::ToolExecution(format!("blocking-io join error: {e}")))?
}
