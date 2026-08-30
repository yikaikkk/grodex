//! Grodex CLI — AI coding agent entry point.
//!
//! Phase 2: interactive session loop using the Agent Loop runtime.

mod idempotency;
mod runtime;
mod telemetry_cmd;

use idempotency::IdempotencyCache;
use runtime::SessionRuntimeBuilder;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use clap::Parser;
use grodex_auth::AuthManager;
use grodex_config::{ConfigResolver, ConfigLayerSource, LoadedConfig};
use grodex_core::id::SessionId;
use grodex_core::policy::PolicyDecision;
use grodex_core::tool::Tool;
use grodex_loop::chat_state::ChatStateActor;
use grodex_loop::command::SessionEvent as LoopSessionEvent;
use grodex_loop::reducer::SessionReducer;
use grodex_loop::{Session, SessionCommand, SessionHandle, SessionSupervisor, TurnCoordinator};
use grodex_prompt::{DiscoveryConfig, EnvironmentInfo, InstructionDiscovery, PromptBuilder};
use grodex_protocol::acp::{
    AckBucket, ApprovalResolution, Command as AcpCommand, EventEnvelope, IndeterminateResolution,
    ResolveIndeterminateCommand, ReplayMode, SessionSnapshotPayload, UpdateContent,
};
use grodex_protocol::{ClientFrame, ServerFrame};
use grodex_rollout::store::{FileRolloutStore, RolloutStore};
use grodex_sampler::{SamplingActor, SamplingClient, SamplingClientConfig};
use grodex_tools::{ApplyPatchTool, EditTool, ExecTool, ReadFileTool, WriteFileTool};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

#[derive(clap::Subcommand, Debug)]
enum TelemetryCommand {
    /// List recent sessions.
    Sessions,
    /// List turns of one session.
    Session { session_id: String },
    /// Detail one turn: termination reason, model attempts, tool lifecycle.
    Turn { turn_id: String },
    /// Recent error-severity events.
    Errors {
        #[arg(default_value_t = 20)]
        limit: u32,
    },
    /// Tools ranked by average latency (with approval-wait breakdown).
    SlowTools {
        #[arg(default_value_t = 10)]
        limit: u32,
    },
    /// Models ranked by average latency (with cache-hit rate).
    SlowModels {
        #[arg(default_value_t = 10)]
        limit: u32,
    },
    /// Lifecycle anomalies: open turns, stuck tools, uncommitted results.
    Doctor,
    /// Prompt-cache hit rates per model (provider-reported tokens).
    Cache,
    /// Chronological turn timeline of one session.
    Timeline { session_id: String },
    /// Lifecycle anomalies: open turns, stuck tools, uncommitted results.
    Recovery,
    /// Checkpoint WAL + VACUUM the telemetry database.
    Vacuum,
    /// Export raw telemetry events as JSONL.
    Export {
        /// Restrict to one session.
        #[arg(long)]
        session: Option<String>,
        /// Output file (default: stdout).
        #[arg(long)]
        output: Option<String>,
    },
}

#[derive(Parser)]
#[command(name = "grodex", about = "AI coding agent", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Start a new interactive session.
    Run {
        /// Working directory to use (default: current dir).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Treat the workspace as explicitly trusted (equivalent to
        /// `[workspace] trusted = true` in config). If omitted, uses
        /// the config-discovered trust flag; if neither is set,
        /// AGENTS.md and .agent/rules content is EXCLUDED from the
        /// prompt (fail-closed).
        #[arg(long)]
        trusted: bool,
    },
    /// Start as an ACP server over stdio.
    Serve,
    /// Resume an existing session.
    Resume {
        session_id: String,
        /// Working directory to use (default: current dir).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Treat the workspace as explicitly trusted (equivalent to
        /// `[workspace] trusted = true` in config).
        #[arg(long)]
        trusted: bool,
    },
    /// Replay a session from rollout journal.
    Replay { session_id: String },
    /// Structured dump of each rollout event (seq, type, timestamp, payload summary).
    Inspect { session_id: String },
    /// Output raw rollout journal as JSONL (one event per line).
    Dump { session_id: String },
    /// Run Memory retrieval eval against a session's rollout journal.
    Eval {
        session_id: String,
        /// Path to the memory SQLite database (default: ~/.grodex/memory.db).
        #[arg(long)]
        db: Option<String>,
        /// Path to a JSON file with ground-truth labels.
        #[arg(long)]
        labels: Option<String>,
        /// Output eval report as JSON to this path (default: stdout human-readable).
        #[arg(long)]
        output: Option<String>,
    },
    /// Inspect and explain prompt assembly (Design Doc 19 §12, §18).
    Prompt {
        #[command(subcommand)]
        action: PromptCommand,
    },
    /// Run the OAuth authorization flow for an MCP server.
    McpAuth {
        /// The MCP server name from [[mcp_server]] config.
        server: String,
        /// Working directory used to resolve config (default: current dir).
        #[arg(long)]
        cwd: Option<PathBuf>,
    },
    /// Query the telemetry projection (~/.grodex/telemetry.db).
    Telemetry {
        #[command(subcommand)]
        cmd: TelemetryCommand,
        /// Explicit telemetry.db path (default: ~/.grodex/telemetry.db,
        /// overridable via GRODEX_TELEMETRY_DB).
        #[arg(long)]
        db: Option<String>,
    },
    /// Show version information.
    Version,
    /// Launch the Grodex terminal UI with ACP protocol support.
    Tui {
        /// Run agent as a subprocess via this command. Defaults to the
        /// CURRENT executable itself (`grodex tui` spawns `<self> serve`),
        /// so it works from `cargo run` without `grodex` being on PATH.
        /// Set explicitly to override (e.g. a different build).
        #[arg(long, default_value = "")]
        agent_cmd: String,
        /// Arguments passed to the agent command (default: ["serve"])
        #[arg(last = true, default_values_t = vec!["serve".to_string()])]
        agent_args: Vec<String>,
    },
}

#[derive(clap::Subcommand)]
enum PromptCommand {
    /// Explain how the system prompt would be assembled for the current
    /// workspace: list each instruction node with its zone, authority,
    /// scope, source, provenance hash, and whether it was trusted or
    /// excluded (untrusted workspace). Also reports the manifest hash,
    /// estimated tokens, and all config diagnostics (including
    /// requirement overrides).
    Explain {
        /// Working directory to use for discovery (default: current dir).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Treat the workspace as explicitly trusted (equivalent to
        /// `[workspace] trusted = true` in config). If omitted, uses the
        /// config-discovered trust flag; if neither is set, content from
        /// AGENTS.md and .agent/rules is EXCLUDED from the prompt
        /// (fail-closed).
        #[arg(long)]
        trusted: bool,
        /// Optional model binding id to record in the manifest (for cache-key
        /// validation against a specific model/provider).
        #[arg(long)]
        model_binding: Option<String>,
        /// Also print the full assembled prompt text (can be large).
        #[arg(long)]
        show_content: bool,
        /// Optional Zone C content (compaction baseline) for dry-run assembly.
        #[arg(long)]
        zone_c: Option<String>,
        /// Optional Zone D content (recent tail) for dry-run assembly.
        #[arg(long)]
        zone_d: Option<String>,
    },
    /// Dump the actual canonical assembled prompt (Design Doc 19 §12/§18).
    /// Pure content goes to stdout (metadata to stderr), so it can be piped
    /// / diffed for reproducibility.
    Dump {
        /// Working directory to use for discovery (default: current dir).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Treat the workspace as explicitly trusted (same semantics as
        /// `prompt explain --trusted`).
        #[arg(long)]
        trusted: bool,
        /// Optional model binding id to record in the manifest.
        #[arg(long)]
        model_binding: Option<String>,
        /// Optional Zone C content (compaction baseline) for dry-run assembly.
        #[arg(long)]
        zone_c: Option<String>,
        /// Optional Zone D content (recent tail) for dry-run assembly.
        #[arg(long)]
        zone_d: Option<String>,
        /// Print full unredacted content. Default is REDACTED: home/cwd
        /// and remaining absolute user paths are replaced by placeholders
        /// (Design Doc 19 §18: dump 默认脱敏路径和环境值).
        #[arg(long)]
        no_redact: bool,
    },
}

/// 安装可观测性订阅器:trace 写入文件(`~/.grodex/logs/grodex.log`,
/// 可用 `GRODEX_LOG_DIR` 覆盖),避免污染 TUI 的 alt-screen 终端输出。
/// 返回的 guard 必须在整个进程生命周期内存活,以保证 non-blocking
/// writer 在退出时 flush。级别受 `RUST_LOG` 控制;未设置时 grodex
/// crate 默认 info(grodex_loop=debug 以便看到 step/turn 细节),第三方
/// crate(hyper/reqwest/tokio)默认 warn 以降噪。
/// Process-global telemetry sink handle. Initialised once in `main`;
/// read by `build_session_parts` wherever a session runtime is built.
static TELEMETRY: std::sync::OnceLock<Option<Arc<dyn grodex_telemetry::TelemetrySink>>> =
    std::sync::OnceLock::new();

fn telemetry_sink() -> Option<Arc<dyn grodex_telemetry::TelemetrySink>> {
    TELEMETRY.get().and_then(|slot| slot.as_ref().cloned())
}

/// Open the SQLite telemetry DB (`~/.grodex/telemetry.db`, 0600). Fail-open:
/// if the DB cannot be opened, telemetry is disabled for the process and
/// the Agent Loop is unaffected.
fn init_telemetry() -> Option<grodex_telemetry::TelemetryGuard> {
    let path = std::env::var("GRODEX_TELEMETRY_DB")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".grodex").join("telemetry.db")))?;
    match grodex_telemetry::SqliteTelemetrySink::open(&path) {
        Ok((sink, guard)) => {
            let _ = TELEMETRY.set(Some(Arc::new(sink)));
            Some(guard)
        }
        Err(e) => {
            eprintln!("[warn] telemetry db unavailable ({e}) — telemetry disabled");
            let _ = TELEMETRY.set(None);
            None
        }
    }
}

fn init_observability() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::EnvFilter;

    let log_dir = std::env::var("GRODEX_LOG_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".grodex").join("logs")))
        .unwrap_or_else(std::env::temp_dir);
    let _ = std::fs::create_dir_all(&log_dir);
    let file_appender = tracing_appender::rolling::daily(&log_dir, "grodex.log");
    let (nb, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "warn,grodex_loop=debug,grodex_cli=info,grodex_sampler=info,\
             grodex_rollout=info,grodex_memory=info,grodex_subagent=info,\
             grodex_permission=info,grodex_capability=info",
        )
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(nb)
        .with_target(false)
        .with_ansi(false)
        .init();
    tracing::info!(target: "grodex_cli", log_dir = %log_dir.display(), "observability subscriber installed");
    Some(guard)
}

#[tokio::main]
async fn main() {
    let _log_guard = init_observability();
    let _telemetry_guard = init_telemetry();
    let cli = Cli::parse();

    match cli.command {
        Command::Run { cwd, trusted } => run_interactive_with(cwd, trusted, None).await,
        Command::Serve => {
            if let Err(e) = serve_acp().await {
                eprintln!("[serve_acp] fatal: {e}");
                std::process::exit(1);
            }
        }
        Command::Resume { session_id, cwd, trusted } => resume_session(&session_id, cwd, trusted).await,
        Command::Replay { session_id } => replay_session(&session_id).await,
        Command::Inspect { session_id } => inspect_session(&session_id).await,
        Command::Dump { session_id } => dump_session(&session_id).await,
        Command::Eval {
            session_id,
            db,
            labels,
            output,
        } => eval_session(&session_id, db.as_deref(), labels.as_deref(), output.as_deref()).await,
        Command::Prompt { action } => match action {
            PromptCommand::Explain {
                cwd,
                trusted,
                model_binding,
                show_content,
                zone_c,
                zone_d,
            } => prompt_explain(
                cwd.as_deref(),
                trusted,
                model_binding.as_deref(),
                show_content,
                zone_c.as_deref(),
                zone_d.as_deref(),
            ),
            PromptCommand::Dump {
                cwd,
                trusted,
                model_binding,
                zone_c,
                zone_d,
                no_redact,
            } => prompt_dump(
                cwd.as_deref(),
                trusted,
                model_binding.as_deref(),
                zone_c.as_deref(),
                zone_d.as_deref(),
                !no_redact,
            ),
        },
        Command::McpAuth { server, cwd } => {
            if let Err(e) = mcp_auth_command(&server, cwd.as_deref()).await {
                eprintln!("[mcp-auth] {e}");
                std::process::exit(1);
            }
        }
        Command::Telemetry { cmd, db } => {
            let result = match &cmd {
                TelemetryCommand::Sessions => telemetry_cmd::sessions(db.as_ref()),
                TelemetryCommand::Session { session_id } => telemetry_cmd::session(db.as_ref(), session_id),
                TelemetryCommand::Turn { turn_id } => telemetry_cmd::turn_detail(db.as_ref(), turn_id),
                TelemetryCommand::Errors { limit } => telemetry_cmd::errors(db.as_ref(), *limit),
                TelemetryCommand::SlowTools { limit } => telemetry_cmd::slow_tools(db.as_ref(), *limit),
                TelemetryCommand::SlowModels { limit } => telemetry_cmd::slow_models(db.as_ref(), *limit),
                TelemetryCommand::Doctor => telemetry_cmd::doctor(db.as_ref()),
                TelemetryCommand::Cache => telemetry_cmd::cache(db.as_ref()),
                TelemetryCommand::Timeline { session_id } => telemetry_cmd::timeline(db.as_ref(), session_id),
                TelemetryCommand::Recovery => telemetry_cmd::recovery(db.as_ref()),
                TelemetryCommand::Vacuum => telemetry_cmd::vacuum(db.as_ref()),
                TelemetryCommand::Export { session, output } => {
                    telemetry_cmd::export(db.as_ref(), session.as_deref(), output.as_deref())
                }
            };
            if let Err(e) = result {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        Command::Version => {
            println!("grodex {}", env!("CARGO_PKG_VERSION"));
        }
        Command::Tui { agent_cmd, agent_args } => {
            // Default agent binary: the current executable itself. The
            // previous default "grodex" required the binary to be on PATH,
            // which broke `cargo run -- tui` with "无法启动 agent 进程:
            // grodex serve". current_exe() always exists and supports the
            // `serve` subcommand.
            let agent_cmd = if agent_cmd.is_empty() {
                std::env::current_exe()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "grodex".to_string())
            } else {
                agent_cmd
            };
            if let Err(e) = grodex_tui::transport::stdio::run_with_stdio_transport(&agent_cmd, &agent_args) {
                eprintln!("TUI 启动失败: {e}");
                std::process::exit(1);
            }
        }
    }
}

#[allow(dead_code)]
struct SessionParts {
    handle: SessionHandle,
    session_id: SessionId,
    _supervisor_task: tokio::task::JoinHandle<()>,
}

async fn build_session_parts(
    cwd: PathBuf,
) -> Result<(
    SessionHandle,
    tokio::sync::mpsc::Receiver<LoopSessionEvent>,
    SessionId,
    Option<Arc<dyn RolloutStore>>,
    Option<Arc<tokio::sync::Mutex<grodex_loop::durable_subagent::DurableSubAgentSupervisor>>>,
)> {
    // Unified composition root: the builder assembles Config, Auth,
    // SamplingActor, PermissionManager (config `[rules]`), SandboxRuntime
    // (`sandbox_profile`), CapabilityManager (built-in tools), RolloutWriter,
    // PromptProvider (per-turn in supervisor) and MemoryProvider — all from
    // one place. ACP `serve_acp` and the CLI REPL both go through here now,
    // so a new module only needs to be wired once.
    let runtime = SessionRuntimeBuilder::new(cwd)
        .with_trusted(true) // ACP serve always trusts the workspace
        .with_telemetry(telemetry_sink())
        .build()
        .await?;

    let session_id = SessionId::from_string(&runtime.session_id)
        .map_err(|e| anyhow!("invalid session id from builder: {e}"))?;
    Ok((
        runtime.handle,
        runtime.event_rx,
        session_id,
        runtime.rollout_store,
        runtime.subagent_recovery,
    ))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

async fn write_protocol_error(
    stdout: &mut tokio::io::Stdout,
    cmd_id: Option<String>,
    code: &str,
    msg: String,
) {
    let f = ServerFrame::ProtocolError {
        code: code.into(),
        message: msg,
        reference_command_id: cmd_id,
    };
    let line = serde_json::to_string(&f).unwrap_or_default();
    let _ = stdout.write_all(line.as_bytes()).await;
    let _ = stdout.write_all(b"\n").await;
    let _ = stdout.flush().await;
}

async fn write_frame(stdout: &mut tokio::io::Stdout, frame: &ServerFrame) {
    let line = serde_json::to_string(frame).unwrap_or_else(|e| {
        format!(
            "{{\"frame_type\":\"protocol_error\",\"code\":\"SERIALIZE\",\"message\":\"{}\"}}",
            e.to_string().replace('"', "\\\"")
        )
    });
    let _ = stdout.write_all(line.as_bytes()).await;
    let _ = stdout.write_all(b"\n").await;
    let _ = stdout.flush().await;
}

fn map_loop_event_to_update(ev: LoopSessionEvent, session_id: SessionId, seq: u64) -> Option<ServerFrame> {
    match ev {
        LoopSessionEvent::TurnStarted { turn_id } => {
            let content = UpdateContent::ItemStarted {
                item_id: turn_id.to_string(),
                item_type: "turn".into(),
            };
            let env = EventEnvelope::wrap(seq, session_id, content);
            Some(ServerFrame::Event(env))
        }
        LoopSessionEvent::TextDelta { text } => {
            let content = UpdateContent::TextDelta { text };
            let env = EventEnvelope::wrap(seq, session_id, content);
            Some(ServerFrame::Event(env))
        }
        // ── Real-time reasoning / thinking stream (Grok-style thinking
        // panel). Mirrors ACP ThoughtDelta 1:1 — the TUI renders this
        // in the purple "Thinking…" rail above the answer.
        LoopSessionEvent::ReasoningDelta { text } => {
            let content = UpdateContent::ThoughtDelta { text };
            let env = EventEnvelope::wrap(seq, session_id, content);
            Some(ServerFrame::Event(env))
        }
        // ── Tool-call lifecycle: Start → Args (N chunks) → End → Result.
        // Each maps to the matching UpdateContent variant so the TUI can
        // show an in-progress card, the growing JSON args, the "running"
        // state, and finally the output payload with status badge.
        LoopSessionEvent::ToolCallStart { call_id, name } => {
            let content = UpdateContent::ToolCallStart { call_id, name };
            let env = EventEnvelope::wrap(seq, session_id, content);
            Some(ServerFrame::Event(env))
        }
        LoopSessionEvent::ToolCallArgs { call_id, args_delta } => {
            let content = UpdateContent::ToolCallArgs { call_id, args_delta };
            let env = EventEnvelope::wrap(seq, session_id, content);
            Some(ServerFrame::Event(env))
        }
        LoopSessionEvent::ToolCallEnd { call_id } => {
            let content = UpdateContent::ToolCallEnd { call_id };
            let env = EventEnvelope::wrap(seq, session_id, content);
            Some(ServerFrame::Event(env))
        }
        LoopSessionEvent::ToolResult { call_id, content, is_error } => {
            let upd = UpdateContent::ToolResult {
                call_id,
                content,
                is_error,
            };
            let env = EventEnvelope::wrap(seq, session_id, upd);
            Some(ServerFrame::Event(env))
        }
        LoopSessionEvent::StepCompleted { turn_id, text } => {
            // StepCompleted.outcome.final_text is the SUPERVISOR's
            // *aggregated step summary* (prompt-assembly output, file-list
            // hints, tool previews, etc.) — it is NOT the assistant's
            // streamed answer, which already went out via TextDelta chunks
            // above. Mapping StepCompleted → TextDelta used to cause the
            // exact symptom the user reported: "输入后看到一堆乱的
            // '文件：… 命令代码 … 帮你代码… 你的… 你的' 内容，然后卡住。"
            //
            // We keep the text for post-hoc debugging only: emit it as a
            // thought/annotation tag via a ServerFrame::FlowControl hint if
            // non-empty. Rendering this optional payload is the TUI's
            // choice (e.g. a "debug summary" pane); the main conversation
            // MUST ignore it.
            if !text.is_empty() {
                // Append to pending_logs on the TUI side via a protocol
                // side-channel? Simplest: send as Error tagged as [summary]
                // so the TUI's "error" list shows it (but is_error=false).
                // Actually we have no 'annotation' UpdateContent, so just
                // drop it here — the assistant TextDeltas already delivered
                // the real answer. Users who want the step log can look at
                // the rollout journal.
                let _ = turn_id; // silence unused later if any
                None
            } else {
                None
            }
        }
        LoopSessionEvent::TurnCompleted { turn_id, input_tokens, cached_tokens } => {
            let content = UpdateContent::TurnComplete {
                turn_id: turn_id.to_string(),
                input_tokens,
                cached_tokens,
            };
            let env = EventEnvelope::wrap(seq, session_id, content);
            Some(ServerFrame::Event(env))
        }
        LoopSessionEvent::Error { message } => {
            let content = UpdateContent::Error { message };
            let env = EventEnvelope::wrap(seq, session_id, content);
            Some(ServerFrame::Event(env))
        }
        LoopSessionEvent::Info { message } => {
            let content = UpdateContent::Info { message };
            let env = EventEnvelope::wrap(seq, session_id, content);
            Some(ServerFrame::Event(env))
        }
        // ── Context compaction lifecycle → TUI "会话压缩中…" 指示。
        LoopSessionEvent::CompactionStatus { phase } => {
            let content = UpdateContent::CompactionStatus { phase };
            let env = EventEnvelope::wrap(seq, session_id, content);
            Some(ServerFrame::Event(env))
        }
        LoopSessionEvent::SubagentProgress(p) => {
            // Flatten the structured loop event into the ACP wire form.
            let (id, label, phase, detail, ok) = match p {
                grodex_loop::delegate_tool::SubagentProgress::Started {
                    id,
                    label,
                    task_preview,
                } => (id, label, "started".to_string(), task_preview, None),
                grodex_loop::delegate_tool::SubagentProgress::Step { id, detail } => {
                    (id, String::new(), "step".to_string(), detail, None)
                }
                grodex_loop::delegate_tool::SubagentProgress::Finished {
                    id,
                    label,
                    ok,
                    summary,
                } => (id, label, "finished".to_string(), summary, Some(ok)),
            };
            let content = UpdateContent::SubagentProgress {
                id,
                label,
                phase,
                detail,
                ok,
            };
            let env = EventEnvelope::wrap(seq, session_id, content);
            Some(ServerFrame::Event(env))
        }
        LoopSessionEvent::Shutdown => None,
        LoopSessionEvent::SnapshotReady {
            last_seq,
            generation,
            current_turn_id,
            items_json,
        } => {
            let items: Vec<grodex_protocol::acp::SnapshotItem> =
                serde_json::from_str(&items_json).unwrap_or_default();
            let snapshot = SessionSnapshotPayload {
                session_id,
                last_seq,
                generation,
                current_turn_id,
                items,
            };
            Some(ServerFrame::Snapshot(snapshot))
        }
        LoopSessionEvent::ApprovalResolved {
            ticket_id: _,
            accepted: _,
        } => {
            // Silently consume — the TUI already resolved the ticket
            // locally via resolve_ticket() when the user pressed Enter.
            // Emitting a TextDelta here spams the chat with
            // "[approval] ticket=… resolved accepted=…" lines.
            None
        }
        // ── B5: Agent asks client for permission. Forward the ticket
        // to the frontend so it can render a pending approval row; the
        // CLI REPL prints a short notice, the TUI shows the full card.
        LoopSessionEvent::ApprovalRequested {
            ticket_id,
            tool_name,
            summary,
            risk,
            timeout_remaining_ms,
            args,
            call_id: _,
        } => {
            let payload = grodex_protocol::acp::RequestPermissionPayload {
                ticket_id,
                tool_name,
                summary,
                risk,
                arguments_snapshot: args,
                timeout_remaining_ms,
            };
            let env = EventEnvelope::wrap(seq, session_id, UpdateContent::RequestPermission(payload));
            Some(ServerFrame::Event(env))
        }
        LoopSessionEvent::IndeterminateToolCall { call_id, tool_name, message } => {
            // Forward to frontend with the dedicated IndeterminateToolCall
            // variant so the TUI can render a three-option resolution dialog.
            let content = UpdateContent::IndeterminateToolCall {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                message: message.clone(),
            };
            let env = EventEnvelope::wrap(seq, session_id, content);
            // Also print to stderr for log visibility (ACP path).
            eprintln!("[indeterminate] call_id={call_id} tool={tool_name}: {message}");
            Some(ServerFrame::Event(env))
        }
    }
}

async fn route_command(
    handle: &SessionHandle,
    cmd: AcpCommand,
    stdout: &mut tokio::io::Stdout,
    idem_cache: &mut IdempotencyCache,
    rollout_store: Option<Arc<dyn RolloutStore>>,
    subagent_recovery: Option<Arc<tokio::sync::Mutex<grodex_loop::durable_subagent::DurableSubAgentSupervisor>>>,
    session_id: SessionId,
    seq: &mut u64,
) -> (Option<AckBucket>, Option<SessionId>) {
    let command_id = match &cmd {
        AcpCommand::Prompt(p) => Some(p.command_id.clone()),
        AcpCommand::Steer(st) => Some(st.command_id.clone()),
        AcpCommand::Cancel(c) => Some(c.command_id.clone()),
        AcpCommand::ResolveApproval(r) => Some(r.command_id.clone()),
        AcpCommand::ResumeSession(r) => Some(r.command_id.clone()),
        AcpCommand::ResolveIndeterminate(r) => Some(r.command_id.clone()),
    };

    if let Some(ref idem_key) = match &cmd {
        AcpCommand::Prompt(p) => p.idempotency_key.as_ref(),
        AcpCommand::Steer(st) => st.idempotency_key.as_ref(),
        AcpCommand::Cancel(c) => c.idempotency_key.as_ref(),
        AcpCommand::ResolveApproval(r) => r.idempotency_key.as_ref(),
        AcpCommand::ResumeSession(r) => r.idempotency_key.as_ref(),
        AcpCommand::ResolveIndeterminate(r) => r.idempotency_key.as_ref(),
    } {
        let now = Instant::now();
        if idem_cache.contains_with_ttl_reclaim(idem_key, now) {
            write_protocol_error(
                stdout,
                command_id.clone(),
                "IDEMPOTENT_HIT",
                format!("idempotency_key already processed: {}", idem_key),
            )
            .await;
            return (None, None);
        }
        idem_cache.insert(idem_key.to_string(), now);
    }

    let mut ack_bucket_out: Option<AckBucket> = None;
    let mut rebind_session_id: Option<SessionId> = None;

    let result: Result<(), String> = match cmd {
        AcpCommand::Prompt(p) => handle
            .send(SessionCommand::StartTurn {
                user_input: p.text,
            })
            .await,
        AcpCommand::Steer(st) => {
            handle
                .send(SessionCommand::Steer {
                    user_input: st.text,
                })
                .await
        }
        AcpCommand::Cancel(_) => handle.send(SessionCommand::CancelTurn).await,
        AcpCommand::ResolveApproval(ra) => {
            let (decision, narrowed_args, always_allow) = match ra.resolution {
                ApprovalResolution::Allow => (PolicyDecision::Allow, None, false),
                ApprovalResolution::AlwaysAllow => (PolicyDecision::Allow, None, true),
                ApprovalResolution::Narrow { narrowed_args } => {
                    (PolicyDecision::Allow, Some(narrowed_args), false)
                }
                ApprovalResolution::Deny => (PolicyDecision::Deny, None, false),
                ApprovalResolution::Cancel => (PolicyDecision::Deny, None, false),
            };
            handle
                .send(SessionCommand::ResolveApproval {
                    ticket_id: ra.ticket_id,
                    decision,
                    narrowed_args,
                    always_allow,
                })
                .await
        }
        AcpCommand::ResumeSession(rs) => {
            ack_bucket_out = rs.ack_bucket.clone();
            let mode = rs.resume_from.mode.clone();
            let last_consumed = rs.resume_from.last_consumed_seq;

            // ── ResumeSession: 用 rs.session_id 打开 *旧会话* 的 FileRolloutStore ──
            // 之前的 bug：rollout_store 是 serve_acp 启动时创建的 *新* session
            // 目录，永远为空，所以 snapshot 空 + context 不注入 → 模型失忆。
            let base_dir = FileRolloutStore::default_dir();
            // FAIL-CLOSED: do NOT silently fall back to `SessionId::new()`.
            // An invalid or unknown session id must surface an error — the
            // previous behaviour of fabricating a random new session id
            // meant "I resumed a known session but all history vanished
            // and the model has amnesia". See user report in the top-level
            // priority list.
            let resume_sid = match SessionId::from_string(&rs.session_id) {
                Ok(s) => s,
                Err(e) => {
                    write_protocol_error(
                        stdout,
                        command_id.clone(),
                        "RESUME_BAD_ID",
                        format!("非法的 session_id 格式 `{}`：{e}", rs.session_id),
                    )
                    .await;
                    return (ack_bucket_out, None);
                }
            };
            // Also verify the directory / journal file actually exists on
            // disk. If the user passed a syntactically-valid but unknown id
            // (e.g. a typo), fail immediately instead of opening an empty
            // directory and pretending the resume "succeeded" with zero
            // items.
            let expected_journal = base_dir.join(resume_sid.to_string()).join("rollout.jsonl");
            if !expected_journal.exists() {
                write_protocol_error(
                    stdout,
                    command_id.clone(),
                    "RESUME_NOT_FOUND",
                    format!(
                        "找不到会话 `{}` 的 rollout journal ({} 不存在)。请确认 session id 拼写。",
                        rs.session_id,
                        expected_journal.display()
                    ),
                )
                .await;
                return (ack_bucket_out, None);
            }
            // Step 1: read the existing journal. Use the LEAN streaming
            // replay: it skips materializing redundant ContextRestored
            // payloads (legacy journals snowballed to hundreds of MB of
            // duplicated snapshots) and reduces the context in the same
            // pass. Fail-closed on corrupt journal.
            let resume_journal = base_dir
                .join(resume_sid.to_string())
                .join("rollout.jsonl");
            let (full_journal, last_seq, restored_context) =
                match grodex_loop::reducer::replay_journal_lean(&resume_journal, &resume_sid) {
                    Ok(t) => t,
                    Err(e) => {
                        write_protocol_error(
                            stdout,
                            command_id.clone(),
                            "RESUME_IO",
                            format!("读取旧会话 rollout journal 失败：{e}"),
                        )
                        .await;
                        return (ack_bucket_out, None);
                    }
                };

            let mut snapshot_items: Vec<grodex_protocol::acp::SnapshotItem> = Vec::new();
            for ev in &full_journal {
                use grodex_rollout::event::RolloutEventType;
                match &ev.event_type {
                    RolloutEventType::UserInputAccepted => {
                        if let Some(text) = ev.payload.get("text").and_then(|v| v.as_str()) {
                            snapshot_items.push(grodex_protocol::acp::SnapshotItem {
                                item_id: format!("u-{}", ev.seq),
                                item_type: "user".to_string(),
                                content: text.to_string(),
                                complete: true,
                            });
                        }
                    }
                    RolloutEventType::ModelItemProduced => {
                        if let Some(reasoning) =
                            ev.payload.get("reasoning").and_then(|v| v.as_str())
                        {
                            if !reasoning.is_empty() {
                                snapshot_items.push(
                                    grodex_protocol::acp::SnapshotItem {
                                        item_id: format!("th-{}", ev.seq),
                                        item_type: "thinking".to_string(),
                                        content: reasoning.to_string(),
                                        complete: true,
                                    },
                                );
                            }
                        }
                        if let Some(text) =
                            ev.payload.get("assistant_text").and_then(|v| v.as_str())
                        {
                            if !text.is_empty() {
                                snapshot_items.push(
                                    grodex_protocol::acp::SnapshotItem {
                                        item_id: format!("a-{}", ev.seq),
                                        item_type: "assistant".to_string(),
                                        content: text.to_string(),
                                        complete: true,
                                    },
                                );
                            }
                        }
                        if let Some(tool_calls) =
                            ev.payload.get("tool_calls").and_then(|v| v.as_array())
                        {
                            for (i, tc) in tool_calls.iter().enumerate() {
                                let call_id = tc
                                    .get("call_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let name = tc
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let args = tc
                                    .get("arguments")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                let merged = serde_json::json!({
                                    "call_id": call_id,
                                    "name": name,
                                    "arguments": args,
                                });
                                snapshot_items.push(
                                    grodex_protocol::acp::SnapshotItem {
                                        item_id: format!("tc-{}-{}", ev.seq, i),
                                        item_type: "tool_call".to_string(),
                                        content: merged.to_string(),
                                        complete: true,
                                    },
                                );
                            }
                        }
                    }
                    RolloutEventType::ToolResultCommitted => {
                        let call_id = ev
                            .payload
                            .get("call_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let content = ev
                            .payload
                            .get("content")
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                        let is_error = ev
                            .payload
                            .get("is_error")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let merged = serde_json::json!({
                            "call_id": call_id,
                            "content": content,
                            "is_error": is_error,
                        });
                        snapshot_items.push(grodex_protocol::acp::SnapshotItem {
                            item_id: format!("tr-{}", ev.seq),
                            item_type: "tool_result".to_string(),
                            content: merged.to_string(),
                            complete: true,
                        });
                    }
                    _ => {}
                }
            }

            // 1) Context projection was already rebuilt in the same lean
            //    replay pass above (`restored_context`) — inject it into
            //    the *current* session (supervisor) so future turns carry
            //    the resumed history.

            // 2) SnapshotThenLive: send Snapshot frame to TUI so chat
            //    history is replayed to the user (they see old turns).
            if matches!(mode, ReplayMode::SnapshotThenLive) {
                let snapshot = ServerFrame::Snapshot(SessionSnapshotPayload {
                    // Snapshot 展示给用户时用 *旧* session_id，确保前端
                    // 的 "当前会话号显示" 与 resume 目标一致。
                    session_id: resume_sid.clone(),
                    last_seq,
                    generation: 0,
                    current_turn_id: None,
                    items: snapshot_items.clone(),
                });
                write_frame(stdout, &snapshot).await;
            }

            // 3) CatchUp mode: emit real UpdateContent events for clients
            //    that listen only to the Event stream.
            let needs_event_replay = matches!(mode, ReplayMode::CatchUp);
            if needs_event_replay {
                let mut replay_seq = last_consumed;
                for ev in full_journal.iter().filter(|e| e.seq > last_consumed) {
                    replay_seq += 1;
                    use grodex_rollout::event::RolloutEventType;
                    let content = match &ev.event_type {
                        RolloutEventType::UserInputAccepted => ev
                            .payload
                            .get("text")
                            .and_then(|v| v.as_str())
                            .map(|t| UpdateContent::TextDelta {
                                text: t.to_string(),
                            }),
                        RolloutEventType::ModelItemProduced => ev
                            .payload
                            .get("assistant_text")
                            .and_then(|v| v.as_str())
                            .map(|t| UpdateContent::TextDelta {
                                text: t.to_string(),
                            }),
                        RolloutEventType::TurnCompleted => {
                            Some(UpdateContent::TurnComplete {
                                turn_id: ev
                                    .turn_id
                                    .as_ref()
                                    .map(|t| t.to_string())
                                    .unwrap_or_default(),
                                input_tokens: 0,
                                cached_tokens: 0,
                            })
                        }
                        _ => None,
                    };
                    if let Some(c) = content {
                        let env =
                            EventEnvelope::wrap(replay_seq, resume_sid.clone(), c);
                        write_frame(stdout, &ServerFrame::Event(env)).await;
                    }
                }
                *seq = (*seq).max(replay_seq);
            }

            // 4) FlowControl ack for Resume command.
            let flow_ctrl = ServerFrame::FlowControl {
                inflight_events: 0,
                requested_pause_ms: None,
            };
            write_frame(stdout, &flow_ctrl).await;

            // 5) CRITICAL — rebind the shared RolloutWriter BEFORE sending
            //    ResumeSession / RestoreContext so every journal write
            //    from this point on appends to the OLD session's durable
            //    directory (instead of the transient new-session empty
            //    journal). Every outstanding clone (supervisor,
            //    coordinator, durable sub-agent) sees the same swap
            //    because the writer inner is `Arc<RwLock<Inner>>`.
            //
            //    This is the FIX for: "I resumed <sid>, chatted more, then
            //    resumed again — the new turns had vanished".
            //
            // NOTE: we open a REAL (actor-backed) writable store here,
            // not `open_readonly`, because the resumed session will
            // keep appending to the OLD journal file. We seed
            // next_seq = last_seq + 1 so the actor's counter starts
            // where the old journal left off (see JournalHandle::start).
            use grodex_rollout::FsyncPolicy;
            let writable_store = match FileRolloutStore::new(
                &base_dir,
                &resume_sid.to_string(),
                last_seq.saturating_add(1),
                FsyncPolicy::default(),
            ).await {
                Ok(s) => s,
                Err(e) => {
                    write_protocol_error(
                        stdout,
                        command_id.clone(),
                        "RESUME_IO",
                        format!("无法启动旧会话的 writable rollout store：{e}"),
                    )
                    .await;
                    return (ack_bucket_out, None);
                }
            };
            let resume_store_arc: Arc<dyn RolloutStore> = Arc::new(writable_store);
            if let Err(e) = handle
                .send(SessionCommand::RebindRolloutWriter {
                    new_store: resume_store_arc,
                    new_session_id: resume_sid,
                    next_seq: last_seq + 1,
                })
                .await
            {
                write_protocol_error(
                    stdout,
                    command_id.clone(),
                    "RESUME_SUPERVISOR",
                    format!("RebindRolloutWriter 失败：{e}"),
                )
                .await;
                return (ack_bucket_out, None);
            }

            // 5.5) Recover sub-agent tasks from the rebound journal:
            // unfinished SubAgentTaskStarted entries are re-registered
            // (marked unrestored if in-flight) so delegation state is
            // consistent after a crash. Fail-open: a recovery error logs
            // and continues — sub-agent recovery must not block resume.
            if let Some(sup) = &subagent_recovery {
                match sup.lock().await.recover_from_journal().await {
                    Ok(n) if n > 0 => {
                        eprintln!("[resume] sub-agent recovery: {n} task(s) re-registered");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("[resume] sub-agent recovery failed (ignored): {e}");
                    }
                }
            }

            // 6) Tell supervisor to (a) emit its own SnapshotReady event and
            //    (b) rebuild the Session.context from the reduced history.
            //    Order matters: RestoreContext MUST be sent after
            //    ResumeSession returns so the reducer's context-write path
            //    doesn't race with a user StartTurn concurrently queued.
            //    We send both here and let the supervisor's FIFO queue
            //    serialize them: RebindRolloutWriter → ResumeSession → RestoreContext.
            if let Err(e) = handle
                .send(SessionCommand::ResumeSession {
                    last_seq,
                    idempotency_key: rs.idempotency_key.clone(),
                    // main.rs already reads the OLD session id's journal
                    // and writes ServerFrame::Snapshot directly to the
                    // TUI. The supervisor is attached to the NEW session
                    // id whose journal is still empty, so its
                    // SnapshotReady would only emit an items=0 snapshot
                    // and clobber the already-rendered 9 history items.
                    // Suppress that broadcast.
                    emit_snapshot_to_frontend: false,
                })
                .await
            {
                write_protocol_error(
                    stdout,
                    command_id.clone(),
                    "RESUME_SUPERVISOR",
                    format!("通知 supervisor ResumeSession 失败：{e}"),
                )
                .await;
                return (ack_bucket_out, None);
            }
            if !restored_context.is_empty() {
                if let Err(e) = handle
                    .send(SessionCommand::RestoreContext {
                        items: restored_context,
                        // persist=false: the writer was just rebound to
                        // THIS session's journal — every restored item is
                        // already on disk. Persisting the whole context
                        // again per resume snowballed journals (436 MB of
                        // duplicated ContextRestored events observed).
                        persist: false,
                    })
                    .await
                {
                    write_protocol_error(
                        stdout,
                        command_id.clone(),
                        "RESUME_SUPERVISOR",
                        format!("RestoreContext 注入 supervisor 失败：{e}"),
                    )
                    .await;
                    return (ack_bucket_out, None);
                }
            }
            rebind_session_id = Some(resume_sid);
            Ok(())
        }
        AcpCommand::ResolveIndeterminate(ri) => {
            let resolution = match ri.resolution {
                IndeterminateResolution::Succeeded => {
                    grodex_loop::command::IndeterminateResolution::Succeeded
                }
                IndeterminateResolution::Failed => {
                    grodex_loop::command::IndeterminateResolution::Failed
                }
                IndeterminateResolution::Retry => {
                    grodex_loop::command::IndeterminateResolution::Retry
                }
            };
            handle
                .send(SessionCommand::ResolveIndeterminate {
                    call_id: ri.call_id,
                    resolution,
                    content: ri.content,
                })
                .await
        }
    };

    if let Err(e) = result {
        write_protocol_error(stdout, command_id, "ROUTE_FAILED", e).await;
    }

    (ack_bucket_out, rebind_session_id)
}

async fn serve_acp() -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (handle, mut acp_rx, mut session_id, rollout_store, subagent_recovery) =
        match build_session_parts(cwd).await {
        Ok(x) => x,
        Err(e) => {
            eprintln!("[serve_acp] init failed: {e}");
            return Err(e);
        }
    };

    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut seq = 0u64;
    let mut idem_cache = IdempotencyCache::new(512, Duration::from_secs(60 * 60));

    let mut client_last_consumed = 0u64;
    let mut inflight_cap: u32 = 128;
    let mut requested_pause_until: Option<Instant> = None;

    // Server-side keepalive: long tool executions can leave the event
    // stream silent for minutes; a periodic Ping keeps the frontend from
    // treating an idle pipe as a dead connection. Pings carry no seq and
    // never interact with backpressure.
    let mut keepalive = tokio::time::interval(Duration::from_secs(15));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            Some(ev) = acp_rx.recv() => {
                let next_seq = seq + 1;
                let inflight = next_seq.saturating_sub(client_last_consumed);
                let mut waited_ms: u64 = 0;
                loop {
                    let paused = requested_pause_until.map(|t| Instant::now() < t).unwrap_or(false);
                    if inflight < inflight_cap as u64 && !paused {
                        break;
                    }
                    if waited_ms >= 10_000 {
                        break;
                    }
                    let pause_frame = ServerFrame::FlowControl {
                        inflight_events: inflight.min(u32::MAX as u64) as u32,
                        requested_pause_ms: Some(10u32),
                    };
                    write_frame(&mut stdout, &pause_frame).await;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    waited_ms += 10;
                }
                seq = next_seq;
                if let Some(frame) = map_loop_event_to_update(ev, session_id, seq) {
                    write_frame(&mut stdout, &frame).await;
                }
            }
            line_res = stdin.next_line() => {
                let line = match line_res {
                    Ok(Some(l)) => l,
                    Ok(None) => break,
                    Err(e) => {
                        write_protocol_error(&mut stdout, None, "IO_READ", e.to_string()).await;
                        continue;
                    }
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let frame: ClientFrame = match serde_json::from_str(trimmed) {
                    Ok(f) => f,
                    Err(e) => {
                        write_protocol_error(&mut stdout, None, "PARSE", e.to_string()).await;
                        continue;
                    }
                };
                match frame {
                    ClientFrame::Command { inner: cmd } => {
                        let (bucket, rebind_sid) = route_command(
                            &handle,
                            cmd,
                            &mut stdout,
                            &mut idem_cache,
                            rollout_store.clone(),
                            subagent_recovery.clone(),
                            session_id,
                            &mut seq,
                        ).await;
                        if let Some(sid) = rebind_sid {
                            // /resume <old_session_id> succeeded: the
                            // writer has been rebound; also align the
                            // outer session_id so subsequent Event
                            // envelopes wrap with the RESUMED id (not
                            // the transient boot-new-session one).
                            session_id = sid;
                        }
                        if let Some(ack) = bucket {
                            inflight_cap = ack.max_inflight_events.min(512).max(1);
                            if let Some(ms) = ack.requested_pause_ms {
                                requested_pause_until = Some(Instant::now() + Duration::from_millis(ms as u64));
                            }
                        }
                    }
                    ClientFrame::Ack { last_consumed_seq } => {
                        client_last_consumed = last_consumed_seq.max(client_last_consumed);
                    }
                    ClientFrame::Ping { sent_at_ms } => {
                        let pong = ServerFrame::Pong {
                            ping_sent_at_ms: sent_at_ms,
                            pong_at_ms: now_ms(),
                        };
                        write_frame(&mut stdout, &pong).await;
                    }
                }
            }
            _ = keepalive.tick() => {
                let ping = ServerFrame::Ping { sent_at_ms: now_ms() };
                write_frame(&mut stdout, &ping).await;
            }
        }
    }

    let _ = handle.send(SessionCommand::Shutdown).await;
    Ok(())
}

/// `grodex prompt explain` — dry-run prompt assembly and print a human-readable
/// explanation of every instruction node, its zone, authority, provenance,
/// the manifest-level hashes, config diagnostics (including enterprise
/// requirement overrides that take effect), and optionally the full prompt.
/// Shared assembly path for `prompt explain` / `prompt dump`: mirrors the
/// runtime build (config load → trust resolution → instruction discovery →
/// PromptBuilder) so both commands see exactly what a live session would.
fn assemble_prompt(
    cwd_arg: Option<&std::path::Path>,
    explicit_trusted: bool,
    model_binding: Option<&str>,
    zone_c: Option<&str>,
    zone_d: Option<&str>,
) -> (
    PathBuf,
    LoadedConfig,
    bool,
    grodex_prompt::DiscoveryResult,
    grodex_prompt::PromptManifest,
) {
    let cwd = cwd_arg
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    // 1. Load config: diagnostics + workspace trust flag + generation counters.
    let config = ConfigResolver::load(&cwd).unwrap_or_else(|_| LoadedConfig::empty());

    // 2. Resolve workspace trust: --trusted flag wins, else the workspace
    //    layer's flag, else untrusted (fail-closed).
    let config_trusted = config
        .raw_layers
        .iter()
        .find_map(|l| match &l.source {
            grodex_config::ConfigLayerSource::Workspace { trusted } => Some(*trusted),
            _ => None,
        })
        .unwrap_or(false);
    let workspace_trusted = explicit_trusted || config_trusted;

    // 3. Instruction discovery (Design Doc 19 §7).
    // Doc 19 §7.3: compat vendor dirs (.grok/.codex/.claude/.cursor) are
    // opt-in via `instruction_compat_vendors` — never scanned by default.
    let instruction_compat_vendors: std::collections::BTreeSet<String> = config
        .effective
        .values
        .get("instruction_compat_vendors")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let discovery_cfg = DiscoveryConfig {
        config_generation: config.generation.prompt,
        compat_vendors: instruction_compat_vendors,
        ..DiscoveryConfig::default()
    };
    let discovery = InstructionDiscovery::new(discovery_cfg);
    let discovery_result = discovery.discover(&cwd, workspace_trusted);

    // 4. Assemble the manifest exactly like the runtime would.
    let builder = PromptBuilder::new()
        .with_config_generation(config.generation.prompt)
        .with_env(EnvironmentInfo::snapshot())
        .with_discovered_nodes(discovery_result.nodes.clone());
    let builder = if let Some(mb) = model_binding {
        builder.with_model_binding(mb)
    } else {
        builder
    };
    let builder = if let Some(zc) = zone_c {
        builder.with_zone_c(zc)
    } else {
        builder
    };
    let builder = if let Some(zd) = zone_d {
        builder.with_zone_d(zd)
    } else {
        builder
    };
    let mut builder = builder;
    builder.load_project_rules(&cwd, workspace_trusted);
    let manifest = builder.build();

    (cwd, config, workspace_trusted, discovery_result, manifest)
}

fn prompt_explain(
    cwd: Option<&std::path::Path>,
    explicit_trusted: bool,
    model_binding: Option<&str>,
    show_content: bool,
    zone_c: Option<&str>,
    zone_d: Option<&str>,
) {
    let (cwd, config, workspace_trusted, discovery_result, manifest) =
        assemble_prompt(cwd, explicit_trusted, model_binding, zone_c, zone_d);

    println!("═══ Grodex Prompt Explain ═══");
    println!("Working directory: {}", cwd.display());

    // Trust resolution (flag + config layer) for fail-closed visibility.
    let config_trusted = config
        .raw_layers
        .iter()
        .find_map(|l| match &l.source {
            grodex_config::ConfigLayerSource::Workspace { trusted } => Some(*trusted),
            _ => None,
        })
        .unwrap_or(false);
    println!(
        "Workspace trust: {} (--trusted={}, config[workspace].trusted={})",
        if workspace_trusted { "TRUSTED" } else { "UNTRUSTED (fail-closed)" },
        explicit_trusted,
        config_trusted
    );
    println!("Config root generation: {}", config.generation.root);
    println!(
        "Config diagnostics: {} (merge/migration/enforcement)",
        config.effective.diagnostics.len()
    );

    println!(
        "\n── Instruction Discovery ──\n\
         Nodes discovered: {}\n\
         Oversized files skipped: {}\n\
         Duplicate canonical paths deduped: {}\n\
         Untrusted workspace files skipped: {}",
        discovery_result.nodes.len(),
        discovery_result.oversized.len(),
        discovery_result.duplicates.len(),
        discovery_result.untrusted_skipped.len()
    );
    if !discovery_result.diagnostics.is_empty() {
        for d in &discovery_result.diagnostics {
            println!("  ⚠ {d}");
        }
    }

    // Manifest-level explanation (zones, nodes, provenance, hash).
    println!("\n── Prompt Manifest ──");
    println!("{}", manifest.explain());

    // Config diagnostics: requirement overrides shaping the assembled prompt.
    if !config.effective.diagnostics.is_empty() {
        println!("\n── Config Diagnostics (merge / migration / requirement overrides) ──");
        for d in &config.effective.diagnostics {
            let level = format!("{:?}", d.level).to_uppercase();
            println!("  [{level:<7}] {} — {}", d.key_path, d.message);
        }
    }

    if show_content {
        println!("\n── Assembled Prompt Content ({:.1} KiB) ──", manifest.content.len() as f64 / 1024.0);
        println!("{}", manifest.content);
        println!("\n── End Content ──");
    } else {
        println!(
            "\n(Use --show-content to print the full assembled prompt. \
             Content size: {:.1} KiB / {} estimated tokens)",
            manifest.content.len() as f64 / 1024.0,
            manifest.estimated_tokens
        );
    }
}

/// `prompt dump` — print the actual canonical assembled prompt.
/// Pure content → stdout; metadata → stderr (pipe/diff friendly).
/// Redaction is ON by default (Design Doc 19 §18): home/cwd and remaining
/// absolute user paths become placeholders.
fn prompt_dump(
    cwd: Option<&std::path::Path>,
    explicit_trusted: bool,
    model_binding: Option<&str>,
    zone_c: Option<&str>,
    zone_d: Option<&str>,
    redact: bool,
) {
    let (cwd, _config, _trusted, _discovery, manifest) =
        assemble_prompt(cwd, explicit_trusted, model_binding, zone_c, zone_d);

    eprintln!("═══ Grodex Prompt Dump ═══");
    eprintln!("Working directory: {}", cwd.display());
    eprintln!("Manifest hash: {}", manifest.hash);
    eprintln!("Schema version: {}", manifest.prompt_schema_version);
    eprintln!("Estimated tokens: {}", manifest.estimated_tokens);
    if let Some(mb) = &manifest.model_binding_id {
        eprintln!("Model binding: {mb}");
    }
    eprintln!("Redacted: {redact} (use --no-redact for full content)");

    let content = if redact {
        redact_prompt_content(&manifest.content)
    } else {
        manifest.content.clone()
    };
    println!("{content}");
}

/// Redact sensitive paths from prompt content: home → `<HOME>`, the
/// resolved working directory → `<CWD>`, and any remaining absolute user
/// paths (`/Users/...`, `/home/...`) → `<PATH>`.
fn redact_prompt_content(content: &str) -> String {
    let mut out = content.to_string();
    if let Some(home) = dirs::home_dir().map(|p| p.display().to_string()) {
        if !home.is_empty() {
            out = out.replace(&home, "<HOME>");
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let cwd_s = cwd.display().to_string();
        if !cwd_s.is_empty() && cwd_s != "." {
            out = out.replace(&cwd_s, "<CWD>");
        }
    }
    redact_absolute_user_paths(&out)
}

/// Replace remaining absolute user paths (`/Users/x/...`, `/home/x/...`)
/// with `<PATH>`. Delimiters: whitespace, quotes, backticks, angle brackets.
fn redact_absolute_user_paths(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let starts_with = |i: usize, pat: &str| -> bool {
        chars[i..].iter().collect::<String>().starts_with(pat)
    };
    while i < chars.len() {
        let is_path_start = chars[i] == '/' && (starts_with(i, "/Users/") || starts_with(i, "/home/"));
        if is_path_start {
            // Consume until a delimiter; emit one placeholder.
            while i < chars.len()
                && !chars[i].is_whitespace()
                && !matches!(chars[i], '"' | '\'' | '`' | '<' | '>' | ')')
            {
                i += 1;
            }
            out.push_str("<PATH>");
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Run an interactive session, optionally seeding the transcript with a
/// context recovered from a prior journal (the `Resume` path).
///
/// `cwd_override` replaces `std::env::current_dir()` as the workspace
/// root (config discovery + prompt discovery + sandbox cwd).
///
/// `explicit_trusted` mirrors `--trusted` on `prompt explain`: when
/// true, workspace-layer content (`.grodex/config.toml`, AGENTS.md,
/// `.agent/rules`) is enabled regardless of the `[workspace] trusted`
/// TOML flag; when false, falls back to the config-discovered value;
/// if neither says trusted, workspace rule + config contribution is
/// quarantined (fail-closed, Design Doc 18 §10).
async fn run_interactive_with(
    cwd_override: Option<PathBuf>,
    explicit_trusted: bool,
    recovered: Option<Vec<grodex_core::context::ContextItem>>,
) {
    println!("═══ Grodex AI Coding Agent ═══");

    // 1. Resolve working directory.
    let cwd = cwd_override
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    // 2. Load config.
    let mut config = ConfigResolver::load(&cwd).unwrap_or_else(|_| LoadedConfig::empty());

    // 3. Apply `--trusted` override to Workspace layers in-place.
    //
    //    We mutate both the `ConfigLayerSource::Workspace { trusted }`
    //    discriminant and the `disabled_reason`, then re-run the public
    //    `merge_layers` routine so that values from the (now trusted)
    //    workspace config actually flow into `config.effective` instead
    //    of being dropped during the quarantine merge-step.
    let config_trusted = config
        .raw_layers
        .iter()
        .find_map(|l| match &l.source {
            ConfigLayerSource::Workspace { trusted } => Some(*trusted),
            _ => None,
        })
        .unwrap_or(false);
    let workspace_trusted = explicit_trusted || config_trusted;
    if explicit_trusted {
        let mut any_changed = false;
        for layer in config.raw_layers.iter_mut() {
            if let ConfigLayerSource::Workspace { trusted: src_trusted } = &mut layer.source {
                if !*src_trusted {
                    *src_trusted = true;
                    layer.disabled_reason = None;
                    any_changed = true;
                }
            }
        }
        if any_changed {
            if let Ok(new_effective) = grodex_config::merge::merge_layers(&config.raw_layers) {
                config.effective = new_effective;
            }
        }
    }
    println!(
        "Workspace trust: {} (--trusted={}, config[workspace].trusted={})",
        if workspace_trusted { "TRUSTED" } else { "UNTRUSTED (fail-closed)" },
        explicit_trusted,
        config_trusted,
    );

    // ── Resolve provider settings: [model_routes.default] TOML → flat keys → env → defaults ──
    //
    // Config.toml takes priority, env vars as fallback.
    let cfg = &config.effective.values;
    let route_toml = grodex_sampler::route::ModelRouteToml::from_config(cfg, "default");
    let first_candidate = route_toml.as_ref().and_then(|r| r.candidates.first());

    let provider_name = cfg.get("provider").and_then(|v| v.as_str()).map(|s| s.to_string())
        .or_else(|| first_candidate.map(|c| c.provider_id.clone()))
        .or_else(|| std::env::var("GRODEX_PROVIDER").ok())
        .unwrap_or_else(|| "openai".to_string());
    // Migration renames top-level `model` → `model_id` (v1→v2), so look up
    // the canonical `model_id` first, then fall back to the v1 `model` alias
    // for configs that haven't been migrated yet.
    let model_name = cfg.get("model_id").or_else(|| cfg.get("model")).and_then(|v| v.as_str()).map(|s| s.to_string())
        .or_else(|| first_candidate.map(|c| c.model_id.clone()))
        .or_else(|| std::env::var("GRODEX_MODEL").ok())
        .unwrap_or_else(|| "gpt-5".to_string());
    let wire_str = cfg.get("wire_protocol").and_then(|v| v.as_str())
        .or_else(|| first_candidate.map(|c| c.wire_protocol.as_str()))
        .map(|s| s.to_string())
        .or_else(|| std::env::var("GRODEX_WIRE_PROTOCOL").ok());
    let wire_protocol = match wire_str.as_deref() {
        Some("chat") | Some("chat_completions") => grodex_provider::descriptor::WireProtocol::ChatCompletions,
        Some("messages") => grodex_provider::descriptor::WireProtocol::Messages,
        _ => grodex_provider::descriptor::WireProtocol::Responses,
    };
    let endpoint = cfg.get("endpoint").and_then(|v| v.as_str()).map(|s| s.to_string())
        .or_else(|| first_candidate.map(|c| c.endpoint.clone()))
        .or_else(|| std::env::var("GRODEX_API_ENDPOINT").ok());
    let _api_key_from_cfg = cfg.get("api_key").and_then(|v| v.as_str()).map(|s| s.to_string());

    // Build a ModelRoute if TOML config was found — wired into the
    // SamplingActor via SessionRuntimeBuilder so FailoverToNextCandidate
    // can switch candidates at runtime.
    let model_route = route_toml.as_ref().map(|r| r.to_model_route());
    if let Some(ref route) = model_route {
        println!("ModelRoute: {} candidate(s), sticky={:?}{}", route.len(),
            route_toml.as_ref().map(|r| r.sticky_scope.as_str()).unwrap_or("turn"),
            if route.len() > 1 { format!(", failover enabled") } else { String::new() });
    }

    println!("Provider: {provider_name}  Model: {model_name}  Wire: {wire_protocol:?}");
    if let Some(ref ep) = endpoint { println!("Endpoint: {ep}"); }

    // ── Unified composition root ─────────────────────────────────
    // SessionRuntimeBuilder assembles Config/Auth/SamplingActor/PermissionManager
    // (config `[rules]`)/SandboxRuntime (`sandbox_profile`)/CapabilityManager
    // (built-in tools + delegate_task)/RolloutWriter/PromptProvider(per-turn
    // in supervisor)/MemoryProvider in ONE place. The CLI REPL and ACP
    // `serve_acp` both go through it, so the "module implemented but
    // production entry doesn't use it" gap is closed at the root.
    //
    // Resume (断链 #8): recovered context from a prior journal is injected
    // via the builder; the supervisor writes a `ContextRestored` event to
    // the new session's journal so a second crash does not lose it.
    if let Some(ref items) = recovered {
        if !items.is_empty() {
            println!("Restoring {} context items into the new session.", items.len());
        }
    }
    let runtime = SessionRuntimeBuilder::new(cwd.clone())
        .with_trusted(workspace_trusted)
        .with_recovered_context(recovered.unwrap_or_default())
        .with_model_route(model_route)
        .build()
        .await
        .expect("failed to build session runtime");
    let handle = runtime.handle;
    let mut event_rx = runtime.event_rx;
    let supervisor_task = runtime.supervisor_task;
    // Keep the rollout path for the startup banner.
    let session_id_str = runtime.session_id;
    let base_dir = grodex_rollout::store::FileRolloutStore::default_dir();
    println!("Session persisted to: {}", base_dir.join(&session_id_str).display());

    // Interactive input loop.
    println!("Session ready. Type /quit to exit.\n");

    loop {
        print!("You: ");
        io::stdout().flush().unwrap();

        let mut input = String::new();
        match io::stdin().read_line(&mut input) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                eprintln!("read error: {e}");
                break;
            }
        }

        let input = input.trim().to_string();
        if input.is_empty() {
            continue;
        }

        if input == "/quit" || input == "/exit" {
            break;
        }
        if input == "/help" {
            println!("Commands: /quit, /exit, /help, /compact, /resume <id>");
            continue;
        }
        if input == "/compact" {
            println!("Requesting context compaction...");
            // Compaction happens automatically; this is a manual trigger hint.
        }

        // Send the turn command.
        if let Err(e) = handle.send(SessionCommand::StartTurn { user_input: input }).await {
            eprintln!("error sending command: {e}");
            break;
        }

        // Receive and buffer streaming text until TurnCompleted.
        let mut response_buf = String::new();
        loop {
            match event_rx.recv().await {
                Some(LoopSessionEvent::TextDelta { text }) => {
                    response_buf.push_str(&text);
                }
                // ── Reasoning + tool events are UI-only details; the
                // simple CLI REPL doesn't print them. They still flow
                // over the streaming pipe for the `grodex-tui` binary.
                Some(LoopSessionEvent::ReasoningDelta { .. }) => {}
                Some(LoopSessionEvent::ToolCallStart { .. }) => {}
                Some(LoopSessionEvent::ToolCallArgs { .. }) => {}
                Some(LoopSessionEvent::ToolCallEnd { .. }) => {}
                Some(LoopSessionEvent::ToolResult { .. }) => {}
                Some(LoopSessionEvent::StepCompleted { .. }) => {}
                Some(LoopSessionEvent::TurnCompleted { .. }) => {
                    println!("\nGrodex: {}", response_buf.trim());
                    break;
                }
                Some(LoopSessionEvent::Error { message }) => {
                    eprintln!("\nError: {message}");
                    break;
                }
                Some(LoopSessionEvent::Info { message }) => {
                    // Informational confirmations (resume, export, …).
                    // Printed to stdout as a one-line banner so the simple
                    // REPL user sees success feedback instead of a scary
                    // Error. Never breaks the turn loop — Info is not a
                    // terminal event.
                    println!("ℹ {message}");
                }
                Some(LoopSessionEvent::CompactionStatus { phase }) => {
                    // 压缩是额外一轮模型调用，简单 REPL 也打一行状态，
                    // 避免长时间静默看起来像卡死。非终态事件，不打断 turn。
                    if phase == "started" {
                        println!("\n[compacting] 会话压缩中…");
                    }
                }
                Some(LoopSessionEvent::Shutdown) => {
                    println!("Session shut down.");
                    break;
                }
                Some(LoopSessionEvent::TurnStarted { .. }) => {}
                Some(LoopSessionEvent::SnapshotReady { .. }) | Some(LoopSessionEvent::ApprovalResolved { .. }) => {}
                // ── Approval requested: the REPL resolves it inline —
                // print the tool + args, read a one-line decision. Empty
                // input defers (ticket expires after its timeout).
                Some(LoopSessionEvent::ApprovalRequested { ticket_id, tool_name, risk, args, .. }) => {
                    println!("\n[approval] ticket={ticket_id} tool={tool_name} risk={risk}");
                    if let Some(a) = &args {
                        println!("  args: {a}");
                    }
                    print!("  批准？[y=允许 / a=总是允许 / n=拒绝 / 回车=等待超时] > ");
                    use std::io::Write as _;
                    let _ = std::io::stdout().flush();
                    let decision = tokio::task::spawn_blocking(|| {
                        let mut line = String::new();
                        let _ = std::io::stdin().read_line(&mut line);
                        line.trim().to_lowercase()
                    })
                    .await
                    .unwrap_or_default();
                    let (allow, always) = match decision.as_str() {
                        "y" | "yes" => (true, false),
                        "a" | "always" => (true, true),
                        "n" | "no" | "c" => (false, false),
                        _ => {
                            println!("  （未答复 — 工具将等待审批超时）");
                            continue;
                        }
                    };
                    let sent = handle
                        .send(SessionCommand::ResolveApproval {
                            ticket_id: ticket_id.clone(),
                            decision: if allow {
                                grodex_core::policy::PolicyDecision::Allow
                            } else {
                                grodex_core::policy::PolicyDecision::Deny
                            },
                            narrowed_args: None,
                            always_allow: always,
                        })
                        .await;
                    // A resolution arriving after the 120s ticket expiry is
                    // a no-op on the broker side — say so instead of a
                    // misleading "已回复".
                    if sent.is_ok() {
                        println!(
                            "  已回复：{}",
                            if always { "总是允许" } else if allow { "允许" } else { "拒绝" }
                        );
                    } else {
                        println!("  回复未送达（会话可能已关闭）");
                    }
                }
                Some(LoopSessionEvent::IndeterminateToolCall { call_id, tool_name, message }) => {
                    println!("\n[indeterminate] call_id={call_id} tool={tool_name}: {message}");
                }
                Some(LoopSessionEvent::SubagentProgress(p)) => {
                    // The simple REPL prints only lifecycle edges (start /
                    // finish); per-step detail is for the TUI card.
                    use grodex_loop::delegate_tool::SubagentProgress as SP;
                    match p {
                        SP::Started { label, task_preview, .. } => {
                            println!("\n[subagent '{label}'] 开始执行: {task_preview}");
                        }
                        SP::Step { .. } => {}
                        SP::Finished { label, ok, summary, .. } => {
                            let tag = if ok { "执行完成" } else { "执行失败" };
                            let preview: String = summary.chars().take(80).collect();
                            println!("\n[subagent '{label}'] {tag}: {preview}");
                        }
                    }
                }
                None => break,
            }
        }
    }

    // Shutdown.
    println!("\nGoodbye.");
    let _ = handle.send(SessionCommand::Shutdown).await;
    let _ = supervisor_task.await;
}

async fn resume_session(session_id: &str, cwd: Option<PathBuf>, trusted: bool) {
    use grodex_core::id::SessionId;
    println!("═══ Grodex Session Resume ═══");
    let base_dir = FileRolloutStore::default_dir();
    let sid = SessionId::from_string(session_id).unwrap_or_else(|_| {
        eprintln!("Invalid session id: {session_id}");
        std::process::exit(1);
    });
    let resume_journal = base_dir.join(sid.to_string()).join("rollout.jsonl");
    let (events, _last_seq, ctx) =
        match grodex_loop::reducer::replay_journal_lean(&resume_journal, &sid) {
            Ok(t) => t,
            Err(e) => { eprintln!("Cannot replay: {e}"); std::process::exit(1); }
        };
    if events.is_empty() {
        println!("No events found. Starting fresh session.");
        // Fall through to normal run...
        return;
    }
    println!("Resumed session with {} context items.", ctx.len());
    // Inject the rebuilt transcript into a fresh session instead of
    // discarding it. (断链 #8: previously this only printed the count and
    // then started an empty session.)
    run_interactive_with(cwd, trusted, Some(ctx)).await;
}

async fn replay_session(session_id: &str) {
    use grodex_core::id::SessionId;
    println!("═══ Grodex Rollout Replay ═══");
    println!("Session: {session_id}");

    let base_dir = FileRolloutStore::default_dir();
    let sid = SessionId::from_string(session_id).unwrap_or_else(|_| {
        eprintln!("Invalid session id: {session_id}");
        std::process::exit(1);
    });

    let events = match FileRolloutStore::replay_snapshot(&base_dir, session_id, 0) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Cannot replay: {e}");
            std::process::exit(1);
        }
    };

    if events.is_empty() {
        println!("No events found for session {session_id}");
        return;
    }

    println!("Events:  {}", events.len());

    let mut reducer = SessionReducer::new(sid);
    match reducer.apply_all(&events) {
        Ok(()) => {
            let ctx = reducer.into_context();
            println!("Context items: {}", ctx.len());
            for item in &ctx {
                match item {
                    grodex_core::context::ContextItem::User { content, .. } => {
                        println!("\nYou: {content}");
                    }
                    grodex_core::context::ContextItem::Assistant { content } => {
                        println!("Grodex: {content}");
                    }
                    grodex_core::context::ContextItem::ToolCall { name, arguments, .. } => {
                        println!("  [Tool: {name}({arguments})]");
                    }
                    grodex_core::context::ContextItem::ToolResult { content, is_error, .. } => {
                        let label = if *is_error { "Error" } else { "Result" };
                        println!("  [{label}: {content}]");
                    }
                    grodex_core::context::ContextItem::CompactionSummary { window_number, .. } => {
                        println!("  [Compaction #{window_number}]");
                    }
                    _ => {
                        println!("  [{item:?}]");
                    }
                }
            }
            println!("\n═══ Replay complete ═══");
        }
        Err(e) => {
            eprintln!("Replay error: {e}");
            std::process::exit(1);
        }
    }
}

/// Structured dump of each rollout event: seq, event_type, timestamp,
/// and a one-line payload summary. Useful for debugging journal contents
/// without reading raw JSONL.
async fn inspect_session(session_id: &str) {
    println!("═══ Grodex Rollout Inspect ═══");
    println!("Session: {session_id}");

    let base_dir = FileRolloutStore::default_dir();
    let events = match FileRolloutStore::replay_snapshot(&base_dir, session_id, 0) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Cannot open session: {e}");
            std::process::exit(1);
        }
    };

    if events.is_empty() {
        println!("No events found for session {session_id}");
        return;
    }

    println!("Events: {}\n", events.len());
    println!("{:>4}  {:<28}  {:<24}  {}", "seq", "event_type", "timestamp", "payload_summary");
    println!("{}", "─".repeat(100));

    for event in &events {
        let ts = event.timestamp.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        let summary = summarize_payload(&event.event_type, &event.payload);
        println!(
            "{:>4}  {:<28}  {:<24}  {}",
            event.seq,
            format!("{:?}", event.event_type),
            ts,
            summary,
        );
    }

    println!("\n═══ Inspect complete ({} events) ═══", events.len());
}

/// Output raw rollout journal as JSONL (one event per line). Pipes directly
/// to stdout for `grodex dump <sid> | jq .` workflows.
async fn dump_session(session_id: &str) {
    let base_dir = FileRolloutStore::default_dir();
    let events = match FileRolloutStore::replay_snapshot(&base_dir, session_id, 0) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Cannot open session: {e}");
            std::process::exit(1);
        }
    };

    for event in &events {
        match serde_json::to_string(event) {
            Ok(line) => println!("{line}"),
            Err(e) => eprintln!("serialize error: {e}"),
        }
    }
}

/// Run Memory retrieval eval against a session's rollout journal.
///
/// Extracts user queries from the rollout, runs IntentRouter + three-way
/// retrieval against the memory DB, optionally compares with ground-truth
/// labels, and prints/saves the eval report.
async fn eval_session(
    session_id: &str,
    db_path: Option<&str>,
    labels_path: Option<&str>,
    output_path: Option<&str>,
) {
    // 1. Read rollout events.
    let base_dir = FileRolloutStore::default_dir();
    let events = match FileRolloutStore::replay_snapshot(&base_dir, session_id, 0) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Cannot open session: {e}");
            std::process::exit(1);
        }
    };

    if events.is_empty() {
        eprintln!("No events found for session {session_id}");
        return;
    }

    // 2. Convert events to the tuple format the sampling module expects.
    let event_tuples: Vec<(u64, String, &serde_json::Value, chrono::DateTime<chrono::Utc>, Option<String>)> = events
        .iter()
        .map(|e| {
            (
                e.seq,
                format!("{:?}", e.event_type),
                &e.payload,
                e.timestamp,
                e.turn_id.as_ref().map(|t| t.to_string()),
            )
        })
        .collect();

    // 3. Open memory database.
    let db_path = db_path.map(std::path::PathBuf::from).unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".grodex")
            .join("memory.db")
    });

    let db = match grodex_memory::MemoryDatabase::open(&db_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Cannot open memory DB at {}: {e}", db_path.display());
            eprintln!("Hint: use --db <path> to specify the memory database location.");
            std::process::exit(1);
        }
    };

    // 4. Optionally load labels.
    let labels = if let Some(path) = labels_path {
        match std::fs::read_to_string(path) {
            Ok(json) => match grodex_memory::EvalLabels::from_json(&json) {
                Ok(l) => Some(l),
                Err(e) => {
                    eprintln!("Cannot parse labels JSON from {path}: {e}");
                    std::process::exit(1);
                }
            },
            Err(e) => {
                eprintln!("Cannot read labels file {path}: {e}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    // 5. Run eval cycle.
    let config = grodex_memory::RetrievalConfig::default();
    let (samples, metrics) = grodex_memory::run_eval_cycle(
        &db,
        &config,
        &event_tuples
            .iter()
            .map(|(s, t, p, ts, tid)| (*s, t.as_str(), *p, *ts, tid.clone()))
            .collect::<Vec<_>>(),
        labels.as_ref(),
    );

    // 6. Output results.
    if let Some(out_path) = output_path {
        // JSON output to file.
        let report = serde_json::json!({
            "session_id": session_id,
            "metrics": &metrics,
            "samples": &samples,
        });
        match serde_json::to_string_pretty(&report) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&out_path, json) {
                    eprintln!("Cannot write output to {out_path}: {e}");
                    std::process::exit(1);
                }
                println!("Eval report written to {out_path}");
            }
            Err(e) => {
                eprintln!("Cannot serialize report: {e}");
                std::process::exit(1);
            }
        }
    } else {
        // Human-readable output to stdout.
        println!("═══ Memory Retrieval Eval ═══");
        println!("Session: {session_id}");
        println!("Memory DB: {}", db_path.display());
        println!("Queries extracted: {}", samples.len());
        if labels.is_some() {
            println!("Labels: loaded");
        } else {
            println!("Labels: none (no ground-truth comparison)");
        }
        println!();
        println!("{}", grodex_memory::format_metrics_report(&metrics));

        // Print per-sample details.
        if !samples.is_empty() {
            println!("\n── Per-sample details ──");
            for s in &samples {
                println!(
                    "  [{}] query={:?}",
                    s.sample_id,
                    truncate(&s.query, 50)
                );
                println!(
                    "    router: skill={} mem={} ev={} | actual: mem={} ev={} skill={}",
                    s.router_decision.skill_enabled,
                    s.router_decision.memory_enabled,
                    s.router_decision.evidence_enabled,
                    s.actual_memory_ids.len(),
                    s.actual_evidence_ids.len(),
                    s.actual_skill_ids.len()
                );
                if !s.expected_memory_ids.is_empty() {
                    let hits = s
                        .expected_memory_ids
                        .iter()
                        .filter(|e| s.actual_memory_ids.contains(e))
                        .count();
                    println!(
                        "    expected mem: {} | hits: {}",
                        s.expected_memory_ids.len(),
                        hits
                    );
                }
            }
        }
    }
}

/// Truncate a string to max chars with ellipsis (used by eval output).

fn summarize_payload(
    event_type: &grodex_rollout::event::RolloutEventType,
    payload: &serde_json::Value,
) -> String {
    use grodex_rollout::event::RolloutEventType::*;
    match event_type {
        UserInputAccepted => {
            let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("?");
            truncate(text, 60)
        }
        ModelItemProduced => {
            let text = payload.get("assistant_text").and_then(|v| v.as_str()).unwrap_or("");
            let tool_count = payload.get("tool_calls")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            if text.is_empty() && tool_count > 0 {
                format!("(no text — {tool_count} tool call(s))")
            } else if text.is_empty() {
                "(no text)".to_string()
            } else {
                truncate(text, 60)
            }
        }
        ToolCallPrepared => {
            let phase = payload.get("phase").and_then(|v| v.as_str()).unwrap_or("prepared");
            format!("phase={phase}")
        }
        ToolExecutionStarted => {
            let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let call_id = payload.get("call_id").and_then(|v| v.as_str()).unwrap_or("?");
            format!("tool={name} call={call_id}")
        }
        ToolExecutionFinished | ToolResultCommitted => {
            let call_id = payload.get("call_id").and_then(|v| v.as_str()).unwrap_or("?");
            let is_error = payload.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
            if is_error {
                format!("call={call_id} ERROR")
            } else {
                format!("call={call_id} ok")
            }
        }
        RuntimeStateChanged => {
            let state = payload.get("state").and_then(|v| v.as_str()).unwrap_or("?");
            format!("state={state}")
        }
        TurnCompleted => {
            let turn = payload.get("turn_id").and_then(|v| v.as_str()).unwrap_or("?");
            format!("turn={turn} done")
        }
        CompactionCommitted => {
            let count = payload.get("items")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("compaction ({count} items)")
        }
        CompactionStarted | CompactionFailed | CompactionCandidateBuilt | ProjectionPruned | PromptSnapshotBuilt => {
            let s = payload.to_string();
            truncate(&s, 60)
        }
        SubAgentTaskStarted => {
            let label = payload.get("label").and_then(|v| v.as_str()).unwrap_or("?");
            let agent = payload.get("agent_id").and_then(|v| v.as_str()).unwrap_or("?");
            format!("subagent={label} agent={agent}")
        }
        SubAgentTaskFinished => {
            let status = payload.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            let task = payload.get("task_id").and_then(|v| v.as_str()).unwrap_or("?");
            format!("task={task} status={status}")
        }
        ModelRouteEvent => {
            let kind = payload.get("event_kind").and_then(|v| v.as_str()).unwrap_or("?");
            let candidate = payload.get("candidate_id").and_then(|v| v.as_str()).unwrap_or("?");
            format!("route={kind} candidate={candidate}")
        }
        CapabilityPromoted => {
            let cap = payload.get("capability_id").and_then(|v| v.as_str()).unwrap_or("?");
            format!("promoted cap={cap}")
        }
        CapabilityCallRejectedStale => {
            let cap = payload.get("capability_id").and_then(|v| v.as_str()).unwrap_or("?");
            let generation = payload.get("bound_generation").and_then(|v| v.as_u64()).unwrap_or(0);
            format!("rejected stale cap={cap} gen={generation}")
        }
        SessionStarted => {
            let cwd = payload.get("cwd").and_then(|v| v.as_str()).unwrap_or("?");
            format!("session start cwd={}", truncate(cwd, 40))
        }
        TurnStarted => {
            let chars = payload.get("input_chars").and_then(|v| v.as_u64()).unwrap_or(0);
            format!("turn started ({chars} chars)")
        }
        ModelAttemptStarted => {
            let provider = payload.get("provider").and_then(|v| v.as_str()).unwrap_or("?");
            let model = payload.get("model").and_then(|v| v.as_str()).unwrap_or("?");
            format!("attempt {provider}/{model}")
        }
        ModelAttemptFinished => {
            let status = payload.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            let attempts = payload.get("attempts").and_then(|v| v.as_u64()).unwrap_or(0);
            let ms = payload.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
            format!("attempt {status} x{attempts} {ms}ms")
        }
        EffectiveToolCallRevisionCreated => {
            let call = payload.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("?");
            let rev = payload.get("revision").and_then(|v| v.as_u64()).unwrap_or(0);
            format!("revision call={call} rev={rev}")
        }
        ContextRestored => {
            let n = payload.get("items").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            format!("restored {n} items")
        }
        ToolCallApproved => {
            let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let call_id = payload.get("call_id").and_then(|v| v.as_str()).unwrap_or("?");
            format!("approved tool={name} call={call_id}")
        }
        ToolOutcomeIndeterminate => {
            let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let call_id = payload.get("call_id").and_then(|v| v.as_str()).unwrap_or("?");
            let reason = payload.get("reason").and_then(|v| v.as_str()).unwrap_or("?");
            format!("INDETERMINATE tool={name} call={call_id} reason={reason}")
        }
        ToolOutcomeResolved => {
            let call_id = payload.get("call_id").and_then(|v| v.as_str()).unwrap_or("?");
            let resolution = payload.get("resolution").and_then(|v| v.as_str()).unwrap_or("?");
            format!("RESOLVED call={call_id} resolution={resolution}")
        }
        ApprovalRequested => {
            let ticket = payload.get("ticket_id").and_then(|v| v.as_str()).unwrap_or("?");
            let tool = payload.get("tool_name").and_then(|v| v.as_str()).unwrap_or("?");
            format!("approval requested ticket={ticket} tool={tool}")
        }
        ApprovalResolved => {
            let ticket = payload.get("ticket_id").and_then(|v| v.as_str()).unwrap_or("?");
            let resolution = payload.get("resolution").and_then(|v| v.as_str()).unwrap_or("?");
            format!("approval resolved ticket={ticket} {resolution}")
        }
        LeaseIssued => {
            let lease = payload.get("lease_id").and_then(|v| v.as_str()).unwrap_or("?");
            let call = payload.get("call_id").and_then(|v| v.as_str()).unwrap_or("?");
            format!("lease issued id={lease} call={call}")
        }
        LeaseConsumed => {
            let lease = payload.get("lease_id").and_then(|v| v.as_str()).unwrap_or("?");
            let call = payload.get("call_id").and_then(|v| v.as_str()).unwrap_or("?");
            format!("lease consumed id={lease} call={call}")
        }
        LeaseExpired => {
            let lease = payload.get("lease_id").and_then(|v| v.as_str()).unwrap_or("?");
            let reason = payload.get("reason").and_then(|v| v.as_str()).unwrap_or("?");
            format!("lease expired id={lease} reason={reason}")
        }
        SkillSnapshotRecorded => {
            let skill_gen = payload.get("skill_generation").and_then(|v| v.as_u64()).unwrap_or(0);
            let count = payload.get("skills")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("skill snapshot gen={skill_gen} count={count}")
        }
        AppOnlyToolCall => {
            let tool = payload.get("tool_name").and_then(|v| v.as_str()).unwrap_or("?");
            let reason = payload.get("reason").and_then(|v| v.as_str()).unwrap_or("?");
            format!("app-only tool={tool} reason={reason}")
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        // Take `max` characters (not bytes) to avoid splitting multi-byte
        // UTF-8 sequences, which would panic on `&s[..max]`.
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::redact_absolute_user_paths;

    #[test]
    fn redacts_users_and_home_paths() {
        let input = "cwd=/Users/alice/proj and log at /home/bob/x.log ok";
        let out = redact_absolute_user_paths(input);
        assert_eq!(out, "cwd=<PATH> and log at <PATH> ok");
    }

    #[test]
    fn stops_at_delimiters() {
        let input = "see </Users/alice/p> and \"/home/b/q\" and `/home/c/r`";
        let out = redact_absolute_user_paths(input);
        assert_eq!(out, "see <<PATH>> and \"<PATH>\" and `<PATH>`");
    }

    #[test]
    fn leaves_system_paths_alone() {
        let input = "/usr/bin/bash /etc/hosts /opt/x";
        assert_eq!(redact_absolute_user_paths(input), input);
    }
}

/// `grodex mcp-auth <server>` — drive the OAuth authorization-code flow
/// for one MCP server: build the authorization URL, let the user paste
/// the redirect URL back, exchange the code, and persist the master
/// token into ~/.grodex/credentials.json (restart survival).
async fn mcp_auth_command(server_name: &str, cwd: Option<&std::path::Path>) -> Result<()> {
    let cwd = cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let config = ConfigResolver::load(&cwd).unwrap_or_else(|_| LoadedConfig::empty());

    let servers = config
        .effective
        .values
        .get("mcp_server")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let server_cfg = servers
        .iter()
        .filter_map(|v| {
            let json = serde_json::to_value(v).ok()?;
            serde_json::from_value::<grodex_mcp::McpServerConfig>(json).ok()
        })
        .find(|c| c.name == server_name)
        .ok_or_else(|| anyhow!("config 中未找到 MCP server '{server_name}'"))?;
    if !server_cfg.requires_oauth() {
        return Err(anyhow!("server '{server_name}' 没有配置 oauth 块，无需授权"));
    }

    let mut coord = match std::env::var("HOME") {
        Ok(home) => grodex_mcp::McpOAuthCoordinator::with_secret_store(std::sync::Arc::new(
            grodex_auth::FileSecretStore::new(
                std::path::PathBuf::from(home)
                    .join(".grodex")
                    .join("credentials.json"),
            ),
        )),
        Err(_) => {
            eprintln!("[auth] HOME 不可用，凭证仅保存在内存");
            grodex_mcp::McpOAuthCoordinator::new()
        }
    };
    if !coord.register_server(&server_cfg)? {
        return Err(anyhow!("server '{server_name}' 注册失败（缺少 oauth 块）"));
    }

    let url = coord.begin_authorization(server_name, &[])?;
    println!("═ Grodex MCP OAuth ══");
    println!("Server : {server_name}");
    println!("请用浏览器打开以下 URL 并完成授权：\n\n  {url}\n");
    println!("授权完成后，把浏览器重定向的完整 URL 粘贴到这里：");
    print!("> ");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    let line = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line.trim().to_string()
    })
    .await
    .unwrap_or_default();

    // Extract code/state from the pasted redirect URL's query string.
    let query = line.split_once('?').map(|(_, q)| q).unwrap_or(&line);
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "code" => code = Some(urldecode(v)),
            "state" => state = Some(urldecode(v)),
            _ => {}
        }
    }
    let code = code.ok_or_else(|| anyhow!("重定向 URL 中未找到 code 参数"))?;
    let state = state.ok_or_else(|| anyhow!("重定向 URL 中未找到 state 参数"))?;

    let authorized = coord.complete_authorization(code, state).await?;
    println!("✓ 授权完成：server '{authorized}' 的凭证已保存（会话重启后仍有效）。");
    Ok(())
}

/// Minimal percent-decoding for query parameter values.
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (
                    bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16)),
                    bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16)),
                ) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}
