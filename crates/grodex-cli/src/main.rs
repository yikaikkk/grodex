//! Grodex CLI — AI coding agent entry point.
//!
//! Phase 2: interactive session loop using the Agent Loop runtime.

mod idempotency;

use idempotency::IdempotencyCache;
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
    AckBucket, ApprovalResolution, Command as AcpCommand, EventEnvelope, ReplayMode,
    SessionSnapshotPayload, UpdateContent,
};
use grodex_protocol::{ClientFrame, ServerFrame};
use grodex_rollout::store::{FileRolloutStore, RolloutStore};
use grodex_sampler::{SamplingActor, SamplingClient, SamplingClientConfig};
use grodex_tools::{ApplyPatchTool, EditTool, ExecTool, ReadFileTool, WriteFileTool};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

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
    /// Show version information.
    Version,
    /// Launch the Grodex terminal UI with ACP protocol support.
    Tui {
        /// Run agent as a subprocess via this command (default: "grodex")
        #[arg(long, default_value = "grodex")]
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
}

#[tokio::main]
async fn main() {
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
        },
        Command::Version => {
            println!("grodex {}", env!("CARGO_PKG_VERSION"));
        }
        Command::Tui { agent_cmd, agent_args } => {
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
)> {
    let mut config = ConfigResolver::load(&cwd).unwrap_or_else(|_| LoadedConfig::empty());

    // Apply trusted override: if any Workspace layer is untrusted, force it
    // trusted and re-merge so workspace-layer config values (provider, model,
    // endpoint, api_key) flow into `effective`.
    let needs_remerge = config.raw_layers.iter().any(|l| {
        matches!(&l.source, grodex_config::ConfigLayerSource::Workspace { trusted } if !*trusted)
    });
    if needs_remerge {
        for layer in &mut config.raw_layers {
            if let grodex_config::ConfigLayerSource::Workspace { trusted } = &mut layer.source {
                *trusted = true;
            }
        }
        config.effective = grodex_config::merge::merge_layers(&config.raw_layers)?;
    }

    let cfg = &config.effective.values;
    let route_toml = grodex_sampler::route::ModelRouteToml::from_config(cfg, "default");
    let first_candidate = route_toml.as_ref().and_then(|r| r.candidates.first());

    // Config.toml takes priority, env vars as fallback.
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
    let api_key_from_cfg = cfg.get("api_key").and_then(|v| v.as_str()).map(|s| s.to_string());

    let session = Session::new(config);
    let session_id = session.id;

    let auth = AuthManager::new();
    let master_key = auth.resolve_for_provider(&provider_name).or(api_key_from_cfg);
    let audience = endpoint.as_deref().unwrap_or("https://api.openai.com/v1").to_string();
    let mut broker = master_key
        .map(|k| {
            let mut b = grodex_auth::CredentialBroker::empty();
            b.register_provider(&provider_name, k);
            b
        });
    let api_key: Option<String> = (|| {
        let b = broker.as_mut()?;
        let lease = b.issue_lease(&provider_name, &audience)?;
        b.resolve(&lease, &audience).ok()
    })();

    let client_config = SamplingClientConfig {
        api_key,
        endpoint,
        ..SamplingClientConfig::default()
    };
    let client = SamplingClient::new(client_config).map_err(|e| anyhow!("failed to create sampling client: {e}"))?;
    let actor = SamplingActor::new(client);
    let chat_state = ChatStateActor::spawn();
    let coordinator = TurnCoordinator::new(actor, chat_state.clone());

    coordinator.register_tool("read_file", Arc::new(ReadFileTool::new()), ReadFileTool::new().input_schema()).await;
    coordinator.register_tool("write_file", Arc::new(WriteFileTool::new()), WriteFileTool::new().input_schema()).await;
    coordinator.register_tool("edit_file", Arc::new(EditTool::new()), EditTool::new().input_schema()).await;
    coordinator.register_tool("exec", Arc::new(ExecTool::new()), ExecTool::new().input_schema()).await;
    coordinator.register_tool("apply_patch", Arc::new(ApplyPatchTool::new()), ApplyPatchTool::new().input_schema()).await;

    let delegate = grodex_loop::delegate_tool::DelegateTool::new(grodex_subagent::supervisor::SubAgentConfig::default());
    let delegate_schema = delegate.input_schema();
    coordinator.register_tool("delegate_task", Arc::new(delegate), delegate_schema).await;

    let session_id_str = session.id.to_string();
    let base_dir = FileRolloutStore::default_dir();
    let rollout: Option<Arc<dyn RolloutStore>> =
        FileRolloutStore::new(&base_dir, &session_id_str)
            .ok()
            .map(|s| Arc::new(s) as Arc<dyn RolloutStore>);
    let rollout_clone = rollout.clone();

    let (mut supervisor, handle) = SessionSupervisor::new(
        session,
        chat_state,
        coordinator,
        rollout,
        None,
        Some(grodex_memory::LegacyRetriever::new(grodex_memory::MemoryStore::new())),
        grodex_loop::supervisor::ModelConfig {
            provider: provider_name.clone(),
            model: model_name.clone(),
            wire_protocol,
        },
    );

    let (event_broadcast_tx, event_broadcast_rx) = tokio::sync::mpsc::channel::<LoopSessionEvent>(128);

    let supervisor_task = tokio::spawn(async move {
        supervisor.run().await;
    });

    let SessionHandle { cmd_tx, mut event_rx } = handle;
    tokio::spawn(async move {
        while let Some(ev) = event_rx.recv().await {
            if event_broadcast_tx.send(ev).await.is_err() {
                break;
            }
        }
    });

    let (_dummy_tx, dummy_rx) = tokio::sync::mpsc::channel::<LoopSessionEvent>(1);
    let cmd_handle = SessionHandle {
        cmd_tx,
        event_rx: dummy_rx,
    };
    let _ = supervisor_task;

    Ok((cmd_handle, event_broadcast_rx, session_id, rollout_clone))
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
        LoopSessionEvent::TurnCompleted { turn_id } => {
            let content = UpdateContent::TurnComplete {
                turn_id: turn_id.to_string(),
            };
            let env = EventEnvelope::wrap(seq, session_id, content);
            Some(ServerFrame::Event(env))
        }
        LoopSessionEvent::Error { message } => {
            let content = UpdateContent::Error { message };
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
            ticket_id,
            accepted,
        } => {
            let content = UpdateContent::TextDelta {
                text: format!("[approval] ticket={} resolved accepted={}", ticket_id, accepted),
            };
            let env = EventEnvelope::wrap(seq, session_id, content);
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
    session_id: SessionId,
    seq: &mut u64,
) -> Option<AckBucket> {
    let command_id = match &cmd {
        AcpCommand::Prompt(p) => Some(p.command_id.clone()),
        AcpCommand::Cancel(c) => Some(c.command_id.clone()),
        AcpCommand::ResolveApproval(r) => Some(r.command_id.clone()),
        AcpCommand::ResumeSession(r) => Some(r.command_id.clone()),
    };

    if let Some(ref idem_key) = match &cmd {
        AcpCommand::Prompt(p) => p.idempotency_key.as_ref(),
        AcpCommand::Cancel(c) => c.idempotency_key.as_ref(),
        AcpCommand::ResolveApproval(r) => r.idempotency_key.as_ref(),
        AcpCommand::ResumeSession(r) => r.idempotency_key.as_ref(),
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
            return None;
        }
        idem_cache.insert(idem_key.to_string(), now);
    }

    let mut ack_bucket_out: Option<AckBucket> = None;

    let result: Result<(), String> = match cmd {
        AcpCommand::Prompt(p) => handle
            .send(SessionCommand::StartTurn {
                user_input: p.text,
            })
            .await,
        AcpCommand::Cancel(_) => handle.send(SessionCommand::CancelTurn).await,
        AcpCommand::ResolveApproval(ra) => {
            let (decision, narrowed_args) = match ra.resolution {
                ApprovalResolution::Allow => (PolicyDecision::Allow, None),
                ApprovalResolution::Narrow { narrowed_args } => {
                    (PolicyDecision::Allow, Some(narrowed_args))
                }
                ApprovalResolution::Deny => (PolicyDecision::Deny, None),
                ApprovalResolution::Cancel => (PolicyDecision::Deny, None),
            };
            handle
                .send(SessionCommand::ResolveApproval {
                    ticket_id: ra.ticket_id,
                    decision,
                    narrowed_args,
                })
                .await
        }
        AcpCommand::ResumeSession(rs) => {
            ack_bucket_out = rs.ack_bucket.clone();
            let mode = rs.resume_from.mode.clone();
            let last_consumed = rs.resume_from.last_consumed_seq;

            let needs_replay = matches!(mode, ReplayMode::CatchUp | ReplayMode::SnapshotThenLive);

            if needs_replay {
                if let Some(ref store) = rollout_store {
                    match store.replay_from(last_consumed.saturating_add(1)).await {
                        Ok(journal_events) => {
                            let mut replay_seq = last_consumed;
                            let mode_str = match mode {
                                ReplayMode::CatchUp => "catch_up",
                                ReplayMode::SnapshotThenLive => "snapshot_then_live",
                                _ => "live_only",
                            };
                            for _ev in &journal_events {
                                replay_seq += 1;
                                let content = UpdateContent::TextDelta {
                                    text: format!(
                                        "frame_type=resume_replay mode={mode_str} seq={replay_seq}"
                                    ),
                                };
                                let env = EventEnvelope::wrap(replay_seq, session_id, content);
                                let frame = ServerFrame::Event(env);
                                write_frame(stdout, &frame).await;
                            }
                            *seq = (*seq).max(replay_seq);
                        }
                        Err(e) => {
                            write_protocol_error(
                                stdout,
                                command_id.clone(),
                                "RESUME_IO",
                                format!("cannot read rollout journal: {e}"),
                            )
                            .await;
                            return ack_bucket_out;
                        }
                    }
                }
            }

            let flow_ctrl = ServerFrame::FlowControl {
                inflight_events: 0,
                requested_pause_ms: None,
            };
            write_frame(stdout, &flow_ctrl).await;

            handle
                .send(SessionCommand::ResumeSession {
                    last_seq: rs.resume_from.last_consumed_seq,
                    idempotency_key: rs.idempotency_key,
                })
                .await
        }
    };

    if let Err(e) = result {
        write_protocol_error(stdout, command_id, "ROUTE_FAILED", e).await;
    }

    ack_bucket_out
}

async fn serve_acp() -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (handle, mut acp_rx, session_id, rollout_store) = match build_session_parts(cwd).await {
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
                        let bucket = route_command(
                            &handle,
                            cmd,
                            &mut stdout,
                            &mut idem_cache,
                            rollout_store.clone(),
                            session_id,
                            &mut seq,
                        ).await;
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
        }
    }

    let _ = handle.send(SessionCommand::Shutdown).await;
    Ok(())
}

/// `grodex prompt explain` — dry-run prompt assembly and print a human-readable
/// explanation of every instruction node, its zone, authority, provenance,
/// the manifest-level hashes, config diagnostics (including enterprise
/// requirement overrides that take effect), and optionally the full prompt.
fn prompt_explain(
    cwd: Option<&std::path::Path>,
    explicit_trusted: bool,
    model_binding: Option<&str>,
    show_content: bool,
    zone_c: Option<&str>,
    zone_d: Option<&str>,
) {
    let cwd = cwd
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    println!("═══ Grodex Prompt Explain ═══");
    println!("Working directory: {}", cwd.display());

    // 1. Load config so we can surface its diagnostics (requirement overrides,
    //    migration warnings, merge issues), extract the workspace trust flag,
    //    and obtain the domain generation counters.
    let config = ConfigResolver::load(&cwd).unwrap_or_else(|_| LoadedConfig::empty());

    // 2. Resolve workspace trust:
    //    a. --trusted CLI flag wins
    //    b. else [workspace] trusted = true in the loaded workspace layer
    //    c. else untrusted (fail-closed — project/path-rule content excluded)
    let config_trusted = config
        .raw_layers
        .iter()
        .find_map(|l| match &l.source {
            grodex_config::ConfigLayerSource::Workspace { trusted } => Some(*trusted),
            _ => None,
        })
        .unwrap_or(false);
    let workspace_trusted = explicit_trusted || config_trusted;

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

    // 3. Run instruction discovery (Design Doc 19 §7) with the resolved trust
    //    flag and the config's prompt generation for stamping discovered nodes.
    let discovery_cfg = DiscoveryConfig {
        config_generation: config.generation.prompt,
        ..DiscoveryConfig::default()
    };
    let discovery = InstructionDiscovery::new(discovery_cfg);
    let discovery_result = discovery.discover(&cwd, workspace_trusted);

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

    // 4. Build PromptManifest via PromptBuilder. The builder also layers in
    //    base instructions, skill/tool listings, environment snapshot, and
    //    any explicitly-configured project rules — so the explain output
    //    matches what the runtime would actually send.
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
    // Load the .grodex/AGENTS.md and .grodex.d/ project-level rules *in
    // addition to* the discovery-result nodes we already pre-seeded.
    // (These are two complementary sources; discovery handled the
    //  per-path tree walk, this handles the explicit config locations.)
    let mut builder = builder;
    builder.load_project_rules(&cwd, workspace_trusted);

    let manifest = builder.build();

    // 5. Print the manifest-level explanation.
    println!("\n── Prompt Manifest ──");
    println!("{}", manifest.explain());

    // 6. Print config diagnostics so users can see requirement overrides
    //    that shape which provider/sandbox/features the assembled prompt
    //    will actually run under (fail-closed visibility).
    if !config.effective.diagnostics.is_empty() {
        println!("\n── Config Diagnostics (merge / migration / requirement overrides) ──");
        for d in &config.effective.diagnostics {
            let level = format!("{:?}", d.level).to_uppercase();
            println!("  [{level:<7}] {} — {}", d.key_path, d.message);
        }
    }

    // 7. Optionally print the full assembled prompt text.
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
    let api_key_from_cfg = cfg.get("api_key").and_then(|v| v.as_str()).map(|s| s.to_string());

    // Build a ModelRoute if TOML config was found (for future failover use).
    let _model_route = route_toml.as_ref().map(|r| r.to_model_route());
    if let Some(ref route) = _model_route {
        println!("ModelRoute: {} candidate(s), sticky={:?}{}", route.len(),
            route_toml.as_ref().map(|r| r.sticky_scope.as_str()).unwrap_or("turn"),
            if route.len() > 1 { format!(", failover enabled") } else { String::new() });
    }

    println!("Provider: {provider_name}  Model: {model_name}  Wire: {wire_protocol:?}");
    if let Some(ref ep) = endpoint { println!("Endpoint: {ep}"); }

    // Create session.
    let session = Session::new(config);

    // Resolve API key: env → config.toml → CredentialBroker.
    //
    // The master key is registered with the CredentialBroker and NEVER handed
    // to the sampler directly. We mint a single-use lease bound to the
    // provider's endpoint, then redeem it through the broker's `resolve`
    // gateway — the only path that materializes a usable Bearer token, and it
    // consumes the lease in the process (no replay). The broker stays owned
    // here for the session lifetime; the sampler gets the redeemed token only.
    let auth = AuthManager::new();
    let master_key = auth.resolve_for_provider(&provider_name).or(api_key_from_cfg);
    let audience = endpoint.as_deref().unwrap_or("https://api.openai.com/v1").to_string();
    let mut broker = master_key
        .map(|key| {
            // Register the key under the real provider name so leases
            // minted for `provider_name` resolve correctly. (Using
            // `empty()` + `register_provider` instead of `new(key)`
            // which would store it under "default" and not match.)
            let mut b = grodex_auth::CredentialBroker::empty();
            b.register_provider(&provider_name, key);
            b
        });
    let api_key: Option<String> = (|| {
        let b = broker.as_mut()?;
        let lease = b.issue_lease(&provider_name, &audience)?;
        b.resolve(&lease, &audience).ok()
    })();
    let has_key = api_key.is_some();
    if !has_key { println!("Warning: No API key configured. Set OPENAI_API_KEY or add api_key to .grodex/config.toml"); }

    let client_config = SamplingClientConfig {
        api_key,
        endpoint,
        ..SamplingClientConfig::default()
    };
    let client = SamplingClient::new(client_config).expect("failed to create sampling client");
    let actor = SamplingActor::new(client);
    let chat_state = ChatStateActor::spawn();

    // Resume (断链 #8): recovered context from a prior journal is passed to
    // the supervisor, which both injects it into the live chat state AND
    // writes a `ContextRestored` event to the new session's journal so a
    // second crash does not lose the recovered history.
    if let Some(ref items) = recovered {
        if !items.is_empty() {
            println!("Restoring {} context items into the new session.", items.len());
        }
    }

    let coordinator = TurnCoordinator::new(actor, chat_state.clone());

    // Register built-in tools.
    coordinator.register_tool("read_file", Arc::new(ReadFileTool::new()), ReadFileTool::new().input_schema()).await;
    coordinator.register_tool("write_file", Arc::new(WriteFileTool::new()), WriteFileTool::new().input_schema()).await;
    coordinator.register_tool("edit_file", Arc::new(EditTool::new()), EditTool::new().input_schema()).await;
    coordinator.register_tool("exec", Arc::new(ExecTool::new()), ExecTool::new().input_schema()).await;
    coordinator.register_tool("apply_patch", Arc::new(ApplyPatchTool::new()), ApplyPatchTool::new().input_schema()).await;
    // Register delegate tool for sub-agent spawning.
    let delegate = grodex_loop::delegate_tool::DelegateTool::new(grodex_subagent::supervisor::SubAgentConfig::default());
    let delegate_schema = delegate.input_schema();
    coordinator.register_tool("delegate_task", Arc::new(delegate), delegate_schema).await;

    if has_key {
        println!("API key found for provider.");
    } else {
        println!("Warning: No OPENAI_API_KEY set. Model calls will fail.");
    }

    // Create rollout store for session persistence.
    let session_id_str = session.id.to_string();
    let base_dir = grodex_rollout::store::FileRolloutStore::default_dir();
    let rollout: Option<Arc<dyn grodex_rollout::store::RolloutStore>> =
        match grodex_rollout::store::FileRolloutStore::new(&base_dir, &session_id_str) {
            Ok(store) => {
                println!("Session persisted to: {}", base_dir.join(&session_id_str).display());
                Some(Arc::new(store))
            }
            Err(_) => None,
        };

    // Create supervisor and handle.
    let (mut supervisor, mut handle) = SessionSupervisor::new(session, chat_state, coordinator, rollout, recovered, Some(grodex_memory::LegacyRetriever::new(grodex_memory::MemoryStore::new())), grodex_loop::supervisor::ModelConfig { provider: provider_name.clone(), model: model_name.clone(), wire_protocol });

    // Spawn the supervisor in a background task.
    let supervisor_task = tokio::spawn(async move {
        supervisor.run().await;
    });

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
            match handle.recv().await {
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
                Some(LoopSessionEvent::Shutdown) => {
                    println!("Session shut down.");
                    break;
                }
                Some(LoopSessionEvent::TurnStarted { .. }) => {}
                Some(LoopSessionEvent::SnapshotReady { .. }) | Some(LoopSessionEvent::ApprovalResolved { .. }) => {}
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
    let store = match FileRolloutStore::new(&base_dir, session_id) {
        Ok(s) => s,
        Err(e) => { eprintln!("Cannot open session: {e}"); std::process::exit(1); }
    };
    let events = match store.replay_from(0) {
        Ok(e) => e,
        Err(e) => { eprintln!("Cannot replay: {e}"); std::process::exit(1); }
    };
    if events.is_empty() {
        println!("No events found. Starting fresh session.");
        // Fall through to normal run...
        return;
    }
    // Rebuild context from events.
    let mut reducer = SessionReducer::new(sid);
    if let Err(e) = reducer.apply_all(&events) {
        eprintln!("Replay error: {e}");
        std::process::exit(1);
    }
    let ctx = reducer.into_context();
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

    let store = match FileRolloutStore::new(&base_dir, session_id) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Cannot open session: {e}");
            std::process::exit(1);
        }
    };

    let events = match store.replay_from(0) {
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
    let store = match FileRolloutStore::new(&base_dir, session_id) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Cannot open session: {e}");
            std::process::exit(1);
        }
    };

    let events = match store.replay_from(0) {
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
    let store = match FileRolloutStore::new(&base_dir, session_id) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Cannot open session: {e}");
            std::process::exit(1);
        }
    };

    let events = match store.replay_from(0) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Cannot replay: {e}");
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
    let store = match FileRolloutStore::new(&base_dir, session_id) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Cannot open session: {e}");
            std::process::exit(1);
        }
    };

    let events = match store.replay_from(0) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Cannot replay: {e}");
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
        EffectiveToolCallRevisionCreated => {
            let call = payload.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("?");
            let rev = payload.get("revision").and_then(|v| v.as_u64()).unwrap_or(0);
            format!("revision call={call} rev={rev}")
        }
        ContextRestored => {
            let n = payload.get("items").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            format!("restored {n} items")
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
