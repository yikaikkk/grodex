//! Stdio ACP transport for the TUI.
//!
//! The child-process plumbing (`StdioClient`, ACK/backpressure, snapshot
//! queueing, graceful shutdown) now lives in the shared `grodex-acp-client`
//! crate so non-terminal frontends (Tauri desktop) reuse the exact same
//! transport. This module re-exports it and adds the TUI-specific bootstrap
//! (`run_with_stdio_transport`), which spawns the agent and drives the
//! ratatui render loop.

pub use grodex_acp_client::StdioClient;

use anyhow::Result;

pub fn run_with_stdio_transport(agent_cmd: &str, agent_args: &[String]) -> Result<()> {
    let args_vec: Vec<&str> = agent_args.iter().map(|s| s.as_str()).collect();
    let client = StdioClient::spawn_agent_subprocess(agent_cmd, &args_vec)
        .map_err(|e| anyhow::anyhow!("无法启动 agent（{agent_cmd}）。请先 cargo build -p grodex-cli，或用 --agent-cmd 指定 grodex 可执行文件路径: {e}"))?;
    let tui = crate::GrodexTui::init_with(client)?;
    tui.run_blocking()
}
