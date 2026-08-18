//! SessionRuntimeBuilder — unified production composition root.
//!
//! Single entry point that assembles every component a Session needs:
//!   1. Config          (grodex-config, layer merge + trusted re-merge)
//!   2. Auth            (AuthManager + CredentialBroker → API key lease)
//!   3. SamplingActor   (grodex-sampler, provider/model/endpoint from config)
//!   4. PermissionManager (policy loaded from config `[rules]`, approval bus)
//!   5. SandboxRuntime  (SandboxManager built from `sandbox_profile`)
//!   6. CapabilityManager (built-in tools + delegate_task registered)
//!   7. RolloutWriter   (FileRolloutStore, shared by supervisor + coordinator)
//!   8. PromptProvider   (assembled per-turn inside supervisor.start_turn;
//!                        memory retriever injected for RAG context)
//!   9. MemoryProvider   (LegacyRetriever over MemoryStore)
//!
//! Before this module existed, `build_session_parts` (CLI chat), the resume
//! path and `serve_acp` each re-implemented the wiring — and several
//! components (PermissionManager, SandboxRuntime) were left at their empty
//! `default()`, so config rules never reached the runtime and the approval
//! main chain was broken. The builder is the single chokepoint: adding a
//! new module now means extending ONE place, not three.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use grodex_auth::AuthManager;
use grodex_config::{ConfigLayerSource, ConfigResolver, LoadedConfig};
use grodex_core::policy::PolicyDecision;
use grodex_core::tool::Tool;
use grodex_loop::chat_state::ChatStateActor;
use grodex_loop::command::SessionEvent as LoopSessionEvent;
use grodex_loop::delegate_tool::DelegateTool;
use grodex_loop::rollout_writer::RolloutWriter;
use grodex_loop::supervisor::ModelConfig;
use grodex_loop::{Session, SessionHandle, SessionSupervisor, TurnCoordinator};
use grodex_permission::{PermissionManager, PermissionPolicy, PolicyRule};
use grodex_provider::descriptor::WireProtocol;
use grodex_rollout::store::{FileRolloutStore, RolloutStore};
use grodex_sampler::{SamplingActor, SamplingClient, SamplingClientConfig};
use grodex_sandbox::SandboxRuntimeClient;
use grodex_subagent::supervisor::SubAgentConfig;
use grodex_tools::{ApplyPatchTool, EditTool, ExecTool, ReadFileTool, WriteFileTool};
use tokio::sync::mpsc;

/// A fully-wired session runtime ready to serve turns.
///
/// The supervisor task is already spawned; callers drive the session
/// through `handle` (send commands, recv events) and use `rollout_store`
/// for ACP `ResumeSession` journal replay.
pub struct SessionRuntime {
    pub handle: SessionHandle,
    /// Broadcast event stream (fan-out of the supervisor's single
    /// `event_rx`). ACP drains this to serialize `ServerFrame::Event`s
    /// over stdio; the CLI REPL reads it for `TextDelta`/`TurnCompleted`.
    pub event_rx: mpsc::Receiver<LoopSessionEvent>,
    pub session_id: String,
    pub rollout_store: Option<Arc<dyn RolloutStore>>,
    /// Resolved model config (provider/model/wire) — exposed so the CLI
    /// can echo it in the startup banner.
    pub model_config: ModelConfig,
    /// Holds the supervisor task alive. Callers may `await` it after
    /// sending `Shutdown` to drain the session. Dropping it detaches
    /// (does NOT abort) the supervisor.
    pub supervisor_task: tokio::task::JoinHandle<()>,
}

/// Builder for [`SessionRuntime`]. Start with [`SessionRuntimeBuilder::new`],
/// optionally override trust / recovered context, then call [`build`].
///
/// [`build`]: SessionRuntimeBuilder::build
pub struct SessionRuntimeBuilder {
    cwd: PathBuf,
    trusted_override: Option<bool>,
    recovered_context: Option<Vec<grodex_core::context::ContextItem>>,
    /// Caller-supplied model config override (e.g. when the caller already
    /// resolved provider/model from CLI flags). When `None`, the builder
    /// derives it from the loaded config (config.toml > env > defaults).
    model_config_override: Option<ModelConfig>,
}

impl SessionRuntimeBuilder {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            trusted_override: None,
            recovered_context: None,
            model_config_override: None,
        }
    }

    /// Force the workspace trust flag (equivalent to `--trusted`). When
    /// set, any untrusted Workspace layer is forced trusted and re-merged
    /// so workspace-layer config values (provider, model, endpoint,
    /// api_key) flow into `effective`. Fail-closed otherwise.
    pub fn with_trusted(mut self, trusted: bool) -> Self {
        self.trusted_override = Some(trusted);
        self
    }

    /// Inject context items recovered from a prior session (for `resume`).
    /// The supervisor persists them as a `ContextRestored` rollout event so
    /// a second crash does not lose the recovered history.
    pub fn with_recovered_context(
        mut self,
        ctx: Vec<grodex_core::context::ContextItem>,
    ) -> Self {
        self.recovered_context = Some(ctx);
        self
    }

    /// Override the model config (provider/model/wire) from CLI flags.
    pub fn with_model_config(mut self, cfg: ModelConfig) -> Self {
        self.model_config_override = Some(cfg);
        self
    }

    /// Assemble all nine components and spawn the supervisor.
    pub async fn build(self) -> Result<SessionRuntime> {
        // ── 1. Config ───────────────────────────────────────────────
        let mut config = ConfigResolver::load(&self.cwd)
            .unwrap_or_else(|_| LoadedConfig::empty());
        if let Some(true) = self.trusted_override {
            let needs_remerge = config.raw_layers.iter().any(|l| {
                matches!(&l.source, ConfigLayerSource::Workspace { trusted } if !*trusted)
            });
            if needs_remerge {
                for layer in &mut config.raw_layers {
                    if let ConfigLayerSource::Workspace { trusted } = &mut layer.source {
                        *trusted = true;
                    }
                }
                config.effective = grodex_config::merge::merge_layers(&config.raw_layers)?;
            }
        }
        let cfg = &config.effective.values;

        // Resolve the effective workspace trust flag AFTER the override
        // has been applied. This is the flag InstructionDiscovery uses
        // (fail-closed: untrusted → AGENTS.md content excluded).
        let workspace_trusted = config.raw_layers.iter().any(|l| {
            matches!(&l.source, ConfigLayerSource::Workspace { trusted } if *trusted)
        }) || self.trusted_override.unwrap_or(false);

        // ── 2. Auth (CredentialBroker → API key lease) ─────────────
        let route_toml = grodex_sampler::route::ModelRouteToml::from_config(cfg, "default");
        let first_candidate = route_toml.as_ref().and_then(|r| r.candidates.first());

        let provider_name = cfg.get("provider").and_then(|v| v.as_str()).map(String::from)
            .or_else(|| first_candidate.map(|c| c.provider_id.clone()))
            .or_else(|| std::env::var("GRODEX_PROVIDER").ok())
            .unwrap_or_else(|| "openai".to_string());
        let model_name = cfg.get("model_id").or_else(|| cfg.get("model"))
            .and_then(|v| v.as_str()).map(String::from)
            .or_else(|| first_candidate.map(|c| c.model_id.clone()))
            .or_else(|| std::env::var("GRODEX_MODEL").ok())
            .unwrap_or_else(|| "gpt-5".to_string());
        let wire_str = cfg.get("wire_protocol").and_then(|v| v.as_str())
            .or_else(|| first_candidate.map(|c| c.wire_protocol.as_str()))
            .map(String::from)
            .or_else(|| std::env::var("GRODEX_WIRE_PROTOCOL").ok());
        let wire_protocol = match wire_str.as_deref() {
            Some("chat") | Some("chat_completions") => WireProtocol::ChatCompletions,
            Some("messages") => WireProtocol::Messages,
            _ => WireProtocol::Responses,
        };
        let endpoint = cfg.get("endpoint").and_then(|v| v.as_str()).map(String::from)
            .or_else(|| first_candidate.map(|c| c.endpoint.clone()))
            .or_else(|| std::env::var("GRODEX_API_ENDPOINT").ok());
        let api_key_from_cfg = cfg.get("api_key").and_then(|v| v.as_str()).map(String::from);

        let auth = AuthManager::new();
        let master_key = auth.resolve_for_provider(&provider_name).or(api_key_from_cfg);
        let audience = endpoint.as_deref().unwrap_or("https://api.openai.com/v1").to_string();
        let mut broker = master_key.map(|k| {
            let mut b = grodex_auth::CredentialBroker::empty();
            b.register_provider(&provider_name, k);
            b
        });
        let api_key: Option<String> = (|| {
            let b = broker.as_mut()?;
            let lease = b.issue_lease(&provider_name, &audience)?;
            b.resolve(&lease, &audience).ok()
        })();

        // ── 3. SamplingActor ───────────────────────────────────────
        let client_config = SamplingClientConfig {
            api_key,
            endpoint,
            ..SamplingClientConfig::default()
        };
        let client = SamplingClient::new(client_config)
            .map_err(|e| anyhow!("failed to create sampling client: {e}"))?;
        // Clone the client for the sub-agent (DelegateTool) so it can run
        // its own sampling turns. reqwest::Client::clone shares the
        // connection pool — cheap.
        let sub_client = client.clone();
        let actor = SamplingActor::new(client);
        let sub_actor = Arc::new(SamplingActor::new(sub_client));
        let chat_state = ChatStateActor::spawn();

        // ── 4. PermissionManager (policy from config `[rules]`) ────
        let policy = build_permission_policy(cfg);
        let permission_mgr = PermissionManager::new(policy);

        // ── 5. SandboxRuntime (from `sandbox_profile`) ─────────────
        // Apply 7-layer intersection: Default layer = the config profile,
        // UserBinding layer = the same (user explicitly chose it). The
        // intersection of identical profiles is the profile itself — but
        // if the permission policy later injects a PolicyCeiling layer,
        // it will further restrict. Doc 13 §7: ro/deny union, rw
        // intersection, empty rw auto-deny with "/".
        let mut sandbox = build_sandbox(cfg);
        let default_profile = sandbox.active_profile().cloned()
            .unwrap_or_else(|| grodex_sandbox_types::profile::SandboxProfile {
                name: "workspace".into(),
                read_only_paths: vec!["/".into()],
                read_write_paths: vec![".".into()],
                deny_paths: vec!["/etc".into(), "/System".into(), "~/.ssh".into()],
                network_rules: vec![grodex_sandbox_types::profile::NetworkRule::AllowLocal],
                allow_exec: true,
                allow_fork: true,
            });
        let layered = grodex_sandbox::profile_layers::LayeredProfileInput {
            default: Some(default_profile.clone()),
            user_binding: Some(default_profile),
            ..Default::default()
        };
        sandbox.apply_layered(&layered, grodex_sandbox::profile_layers::AccessLevel::Level2);

        // Effective profile after the 7-layer intersection. Cloned here
        // (before `sandbox` moves into the coordinator) so the `ExecTool`
        // can be wired with it for kernel-enforced execution.
        let effective_profile = sandbox.active_profile().cloned();

        // Sandbox runtime client — opt-in OS-level enforcement for the
        // `exec` tool. Two config switches (both under `[sandbox]`):
        //   - `enforce = true`            → in-process sandbox-exec (macOS)
        //   - `external_supervisor = true`→ fork+exec the `grodex-supervisor`
        //     binary (also macOS; the binary itself exists on all platforms
        //     but only macOS has an enforcement backend).
        // When either is set, the ExecTool routes `sh -c <cmd>` through
        // `run_dispatched()` so deny/network rules are kernel-enforced.
        // Fail-closed: on a platform without an enforcement backend, the
        // runtime refuses and the tool returns an error instead of running
        // unsandboxed. When neither switch is set, the ExecTool keeps its
        // direct-spawn behaviour (back-compat; no surprise breakage on
        // Linux dev machines).
        let sandbox_cfg_tbl = cfg.get("sandbox").and_then(|v| v.as_table());
        let enforce_exec = sandbox_cfg_tbl
            .and_then(|t| t.get("enforce"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let external_supervisor = sandbox_cfg_tbl
            .and_then(|t| t.get("external_supervisor"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let supervisor_path = sandbox_cfg_tbl
            .and_then(|t| t.get("supervisor_path"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let enable_sandbox_exec = enforce_exec || external_supervisor;
        let sandbox_runtime_client: Option<SandboxRuntimeClient> = if enable_sandbox_exec {
            #[cfg(target_os = "macos")]
            {
                if external_supervisor {
                    let path = supervisor_path
                        .as_deref()
                        .unwrap_or("grodex-supervisor");
                    Some(
                        SandboxRuntimeClient::new_external(
                            std::path::PathBuf::from(path),
                            30_000,
                        )
                        .unwrap_or_else(|_| {
                            eprintln!(
                                "[warn] external sandbox supervisor unavailable, \
                                 falling back to in-process enforcement"
                            );
                            let mut c = SandboxRuntimeClient::new();
                            if let Some(p) = supervisor_path {
                                c = c.with_supervisor_path(p);
                            }
                            c
                        }),
                    )
                } else {
                    let mut c = SandboxRuntimeClient::new();
                    if let Some(p) = supervisor_path {
                        c = c.with_supervisor_path(p);
                    }
                    Some(c)
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                let mut c = SandboxRuntimeClient::new();
                if let Some(p) = supervisor_path {
                    c = c.with_supervisor_path(p);
                }
                Some(c)
            }
        } else {
            None
        };
        // Pair the client with the effective profile so the ExecTool has
        // everything it needs to build a `PreparedOperation` per call.
        let mut exec_sandbox = match (sandbox_runtime_client, effective_profile) {
            (Some(c), Some(p)) => Some((c, p)),
            _ => None,
        };

        // ── 6. Session + RolloutWriter (moved before coordinator) ──
        // The writer must be created before tool registration so the
        // DelegateTool can receive a clone for DurableSubAgentSupervisor.
        // Clone config so the `cfg` borrow (used by MCP/embedding steps
        // below) stays valid. The original goes into the Session.
        let session = Session::new(config.clone());
        let session_id = session.id;
        let session_id_str = session_id.to_string();
        let base_dir = FileRolloutStore::default_dir();
        let rollout: Option<Arc<dyn RolloutStore>> = FileRolloutStore::new(&base_dir, &session_id_str)
            .ok()
            .map(|s| Arc::new(s) as Arc<dyn RolloutStore>);
        let writer = rollout
            .as_ref()
            .map(|s| RolloutWriter::new(s.clone(), session.id));

        let model_config = self.model_config_override.unwrap_or(ModelConfig {
            provider: provider_name,
            model: model_name,
            wire_protocol,
        });

        // ── 7. CapabilityManager + TurnCoordinator ─────────────────
        // Inject permission + sandbox so the coordinator dispatches tool
        // calls through the config-loaded policy (not an empty default)
        // and validates against the config profile.
        let coordinator = TurnCoordinator::new(actor, chat_state.clone())
            .with_permission(permission_mgr)
            .with_sandbox(sandbox);

        // Register built-in tools. Each gets a fresh runtime instance +
        // its JSON schema. The delegate_task tool is wired with:
        //   - a shared SamplingActor (so it can actually run sub-agent turns)
        //   - a RolloutWriter clone (so spawn/complete are journaled)
        coordinator
            .register_tool("read_file", Arc::new(ReadFileTool::new()), ReadFileTool::new().input_schema())
            .await;
        coordinator
            .register_tool("write_file", Arc::new(WriteFileTool::new()), WriteFileTool::new().input_schema())
            .await;
        coordinator
            .register_tool("edit_file", Arc::new(EditTool::new()), EditTool::new().input_schema())
            .await;
        // ExecTool: when the session enabled OS-level sandbox enforcement
        // (`[sandbox] enforce`/`external_supervisor`), wire the runtime
        // client + effective profile so `sh -c <cmd>` runs under
        // kernel-enforced deny/network rules. Otherwise register the plain
        // direct-spawn tool (back-compat).
        let exec_tool = match exec_sandbox.take() {
            Some((client, profile)) => ExecTool::new().with_sandbox_runtime(client, profile),
            None => ExecTool::new(),
        };
        let exec_schema = exec_tool.input_schema();
        coordinator
            .register_tool("exec", Arc::new(exec_tool), exec_schema)
            .await;
        coordinator
            .register_tool("apply_patch", Arc::new(ApplyPatchTool::new()), ApplyPatchTool::new().input_schema())
            .await;
        let mut delegate = DelegateTool::new(SubAgentConfig::default())
            .with_sampling(sub_actor, model_config.clone());
        if let Some(ref w) = writer {
            delegate = delegate.with_writer(w.clone(), SubAgentConfig::default());
        }
        let delegate_schema = delegate.input_schema();
        coordinator
            .register_tool("delegate_task", Arc::new(delegate), delegate_schema)
            .await;

        // ── 7b. MCP tools (from config `[[mcp_server]]`) ──────────
        // For each enabled MCP server, spawn the process, list tools,
        // wrap each as McpToolAdapter, and register with the coordinator.
        // Fail-open: if a server can't spawn or list tools, log and skip —
        // the session continues without those MCP tools.
        if let Some(mcp_servers) = cfg.get("mcp_server").and_then(|v| v.as_array()) {
            for server_cfg in mcp_servers {
                // Convert toml::Value → serde_json::Value for deserialization.
                let json_cfg = serde_json::to_value(server_cfg).unwrap_or(serde_json::Value::Null);
                let mcp_config = match serde_json::from_value::<grodex_mcp::McpServerConfig>(json_cfg) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("[warn] MCP server config parse error: {e}");
                        continue;
                    }
                };
                if !mcp_config.enabled {
                    continue;
                }
                match grodex_mcp::McpProcess::spawn(mcp_config.clone()).await {
                    Ok(mut process) => {
                        match process.list_tools().await {
                            Ok(tools) => {
                                for tool in tools {
                                    let adapter = grodex_mcp::McpToolAdapter::new(
                                        mcp_config.clone(),
                                        tool.name.clone(),
                                        tool.description.clone(),
                                        tool.input_schema.clone(),
                                    );
                                    let schema = adapter.input_schema();
                                    let name = adapter.full_name().to_string();
                                    coordinator
                                        .register_tool(&name, Arc::new(adapter), schema)
                                        .await;
                                }
                            }
                            Err(e) => {
                                eprintln!("[warn] MCP server '{}' list_tools failed: {e}", mcp_config.name);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[warn] MCP server '{}' spawn failed: {e}", mcp_config.name);
                    }
                }
            }
        }

        // ── 8 & 9. PromptProvider + MemoryProvider + Embedding ────
        // Prompt assembly happens per-turn inside `supervisor.start_turn`
        // (PromptBuilder + InstructionDiscovery + memory query). The memory
        // database is injected here so RAG context flows into the prompt.
        //
        // Opens the SQLite + FTS5 database at `~/.grodex/memory.db`.
        // Fail-open: if the DB cannot be opened (permissions, disk full),
        // memory is set to None — turns proceed without RAG.
        let memory = match std::env::var("GRODEX_MEMORY_DB") {
            Ok(path) => grodex_memory::MemoryDatabase::open(std::path::Path::new(&path))
                .ok()
                .map(Arc::new),
            Err(_) => {
                let default_db = dirs::home_dir().map(|h| h.join(".grodex").join("memory.db"));
                if let Some(ref db_path) = default_db {
                    if let Some(parent) = db_path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    grodex_memory::MemoryDatabase::open(db_path)
                        .ok()
                        .map(Arc::new)
                } else {
                    None
                }
            }
        };

        // Embedding model for hybrid RAG (FTS5 + vector). Fail-open:
        // if embedding is not configured (enable_embedding=false or
        // missing API key env var), the model is None and
        // `retrieve_hybrid_memory` degrades to pure FTS5 ranking.
        let embedding: Option<Arc<dyn grodex_memory::EmbeddingModel + Send + Sync>> = {
            let emb_cfg = grodex_memory::embedding::EmbeddingConfig {
                enable_embedding: cfg.get("memory")
                    .and_then(|m| m.get("enable_embedding"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                endpoint: cfg.get("memory")
                    .and_then(|m| m.get("embedding_endpoint"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("https://api.openai.com/v1/embeddings")
                    .to_string(),
                model: cfg.get("memory")
                    .and_then(|m| m.get("embedding_model"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("text-embedding-3-small")
                    .to_string(),
                api_key_env_var: cfg.get("memory")
                    .and_then(|m| m.get("embedding_api_key_env_var"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                expected_dimension: cfg.get("memory")
                    .and_then(|m| m.get("embedding_dim"))
                    .and_then(|v| v.as_integer())
                    .map(|v| v as usize)
                    .unwrap_or(1536),
                batch_size: 64,
            };
            match grodex_memory::embedding::OpenAiCompatibleModel::new(emb_cfg) {
                Ok(model) => Some(Arc::new(model) as Arc<dyn grodex_memory::EmbeddingModel + Send + Sync>),
                Err(_) => None, // not configured — pure FTS5
            }
        };

        // ── Supervisor ─────────────────────────────────────────────
        let (mut supervisor, handle) = SessionSupervisor::new(
            session,
            chat_state,
            coordinator,
            writer,
            self.recovered_context,
            memory,
            embedding,
            model_config.clone(),
            self.cwd.clone(),
            workspace_trusted,
        );

        let (event_broadcast_tx, event_broadcast_rx) =
            mpsc::channel::<LoopSessionEvent>(128);
        let supervisor_task = tokio::spawn(async move {
            supervisor.run().await;
        });

        // Fan out the supervisor's single event stream to multiple
        // consumers (ACP stdio writer, CLI REPL, future TUI). The
        // supervisor owns `event_rx`; we forward every event to the
        // broadcast channel and hand the receive end to callers.
        let SessionHandle { cmd_tx, mut event_rx } = handle;
        tokio::spawn(async move {
            while let Some(ev) = event_rx.recv().await {
                if event_broadcast_tx.send(ev).await.is_err() {
                    break;
                }
            }
        });

        // Dummy event_rx so the returned handle satisfies the type without
        // giving callers direct access to the supervisor's raw stream (they
        // must use `event_rx` below). The supervisor reads commands from
        // `cmd_tx` regardless.
        let (_dummy_tx, dummy_rx) = mpsc::channel::<LoopSessionEvent>(1);
        let cmd_handle = SessionHandle {
            cmd_tx,
            event_rx: dummy_rx,
        };

        Ok(SessionRuntime {
            handle: cmd_handle,
            event_rx: event_broadcast_rx,
            session_id: session_id_str,
            rollout_store: rollout,
            model_config,
            supervisor_task,
        })
    }
}

/// Build a `PermissionPolicy` from the config `[rules]` table.
///
/// Supported config forms (config.example.toml):
///   ```toml
///   [rules]
///   read_file = "allow"
///   write_file = "ask"
///   exec = "deny"
///   ```
/// Each key is a tool name (or `*`), each value is `allow` / `ask` / `deny`.
/// When no `[rules]` table is present, a safe default is applied:
///   - `read_file` → Allow
///   - `*`         → Ask   (every side-effecting tool prompts)
/// This avoids the prior footgun where an empty `PermissionPolicy::new()`
/// made *every* tool `Ask`, and — because the approval chain was broken —
/// every tool call timed out.
fn build_permission_policy(cfg: &toml::Value) -> PermissionPolicy {
    let mut policy = PermissionPolicy::new();

    let Some(rules_tbl) = cfg.get("rules").and_then(|v| v.as_table()) else {
        // Safe default: reads allowed, everything else asks.
        policy.add_rule(PolicyRule {
            tool_pattern: "read_file".into(),
            arg_patterns: Vec::new(),
            command: None,
            resource: None,
            rule_id: None,
            network: None,
            mcp: None,
            decision: PolicyDecision::Allow,
            priority: 0,
        });
        policy.add_rule(PolicyRule {
            tool_pattern: "*".into(),
            arg_patterns: Vec::new(),
            command: None,
            resource: None,
            rule_id: None,
            network: None,
            mcp: None,
            decision: PolicyDecision::Ask,
            priority: 0,
        });
        return policy;
    };

    for (tool, decision_val) in rules_tbl {
        let Some(decision_str) = decision_val.as_str() else { continue };
        let decision = match decision_str.to_lowercase().as_str() {
            "allow" => PolicyDecision::Allow,
            "deny" => PolicyDecision::Deny,
            "ask" => PolicyDecision::Ask,
            _ => continue,
        };
        policy.add_rule(PolicyRule {
            tool_pattern: tool.clone(),
            arg_patterns: Vec::new(),
            command: None,
            resource: None,
            rule_id: None,
            network: None,
            mcp: None,
            decision,
            priority: 0,
        });
    }
    policy
}

/// Build a `SandboxManager` from the `sandbox_profile` config value.
///
/// Recognized profiles: `workspace` (default), `readonly`, `restricted`,
/// `full`. The profile name is forwarded to `SandboxManager::new`, which
/// resolves it against the built-in `ProfileStore`. Full profile →
/// `SandboxProfile` translation (doc 16 §14, doc 13) is a later phase;
/// today this selects the active profile name the manager validates
/// against.
fn build_sandbox(cfg: &toml::Value) -> grodex_sandbox::SandboxManager {
    let profile = cfg
        .get("sandbox_profile")
        .and_then(|v| v.as_str())
        .unwrap_or("workspace");
    grodex_sandbox::SandboxManager::new(profile)
}
