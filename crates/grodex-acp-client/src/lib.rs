//! Grodex ACP stdio client — reusable for any frontend that drives the
//! `grodex serve` agent process over its ACP JSON-lines transport.
//!
//! Extracted from `grodex-tui` so non-terminal frontends (e.g. the Tauri
//! desktop app) can reuse the battle-tested child-process handling:
//! spawn / poll / ACK-backpressure / snapshot queue / graceful shutdown.
//!
//! The agent protocol lives in `grodex-protocol`:
//!   - client → agent:  [`grodex_protocol::ClientFrame`] (Command / Ack / Ping)
//!   - agent → client:  [`grodex_protocol::ServerFrame`] (Event / Snapshot / …)

pub mod stdio;

pub use stdio::StdioClient;
