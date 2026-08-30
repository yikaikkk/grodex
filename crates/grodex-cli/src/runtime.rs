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
use grodex_core::tool::{Tool, ToolRuntime};
use grodex_loop::chat_state::ChatStateActor;
use grodex_loop::command::SessionEvent as LoopSessionEvent;
use grodex_loop::delegate_tool::DelegateTool;
use grodex_loop::rollout_writer::RolloutWriter;
use grodex_loop::supervisor::{infer_context_window, ModelConfig};
use grodex_loop::{Session, SessionHandle, SessionSupervisor, TurnCoordinator};
use grodex_permission::{PermissionManager, PermissionPolicy, PolicyRule};
use grodex_provider::descriptor::WireProtocol;
use grodex_rollout::store::{FileRolloutStore, RolloutStore};
use grodex_sampler::{ModelRoute, SamplingActor, SamplingClient, SamplingClientConfig};
use grodex_sandbox::SandboxRuntimeClient;
use grodex_subagent::supervisor::SubAgentConfig;
use grodex_tools::{ApplyPatchTool, EditTool, ExecTool, GlobTool, GrepTool, LoadSkillTool, ReadFileTool, WebFetchTool, WriteFileTool};
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
    /// MCP OAuth 协调器：仅当至少一个 `[[mcp_server]]` 配了 `oauth` 块时存在。
    /// 调用方用它驱动 begin/complete authorization 与 bearer 取值。
    pub mcp_oauth: Option<grodex_mcp::McpOAuthCoordinator>,
    /// Resolved model config (provider/model/wire) — exposed so the CLI
    /// can echo it in the startup banner.
    pub model_config: ModelConfig,
    /// Holds the supervisor task alive. Callers may `await` it after
    /// sending `Shutdown` to drain the session. Dropping it detaches
    /// (does NOT abort) the supervisor.
    pub supervisor_task: tokio::task::JoinHandle<()>,
    /// Durable sub-agent supervisor handle (when writer-backed). The
    /// resume path runs `recover_from_journal` on it after the rollout
    /// writer is rebound so unfinished sub-agent tasks are re-registered.
    pub subagent_recovery: Option<Arc<tokio::sync::Mutex<grodex_loop::durable_subagent::DurableSubAgentSupervisor>>>,
    /// Config hot-reload fs backend (Doc 18 §11): kept alive for the
    /// session so `notify` keeps feeding the ConfigWatcher pipeline.
    pub config_fs_backend: Option<grodex_config::FsConfigBackend>,
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
    /// Provider failover route built from `[model_routes.default]` TOML.
    /// When `Some`, the SamplingActor is wired with this route so that
    /// `RetryDecision::FailoverToNextCandidate` can switch candidates.
    model_route: Option<ModelRoute>,
    /// SQLite telemetry sink. When `Some`, the RolloutWriter emits a
    /// fire-and-forget telemetry record per journal append.
    telemetry: Option<Arc<dyn grodex_telemetry::TelemetrySink>>,
    /// Process-level run id attached to every telemetry record.
    run_id: String,
}

impl SessionRuntimeBuilder {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            trusted_override: None,
            recovered_context: None,
            model_config_override: None,
            model_route: None,
            telemetry: None,
            run_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    /// Attach the telemetry sink (SQLite, `~/.grodex/telemetry.db`).
    /// `None` (or a sink that later fails) simply disables telemetry —
    /// it never blocks the Agent Loop.
    pub fn with_telemetry(mut self, sink: Option<Arc<dyn grodex_telemetry::TelemetrySink>>) -> Self {
        self.telemetry = sink;
        self
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

    /// Inject a provider failover route (from `[model_routes.default]`).
    /// When set, the SamplingActor will be able to fail over to the next
    /// candidate on `RetryDecision::FailoverToNextCandidate`.
    pub fn with_model_route(mut self, route: Option<ModelRoute>) -> Self {
        self.model_route = route;
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

        // Doc 19 §7.3: compatibility vendor directories (.grok/.codex/
        // .claude/.cursor) are NEVER scanned silently — opt in explicitly
        // via `instruction_compat_vendors = ["claude", "cursor"]`.
        let instruction_compat_vendors: std::collections::BTreeSet<String> = cfg
            .get("instruction_compat_vendors")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // ── 1b. Config hot-reload pipeline (Doc 18 §11/§12) ─────────
        // The `notify` fs backend feeds every config-file change into
        // ConfigWatcher (hash dedup + publish breaker + per-domain LKG).
        // Decisions are surfaced on stderr; hot-ADOPT of the new
        // generation by live subsystems is out of scope here. Fail-open:
        // watcher failure only disables hot-reload, never the session.
        // Hot-adopt channel: the drain thread (std) hands recompiled
        // permission policies to an async forwarder task (declared here so
        // the receiver outlives the watcher block).
        let (policy_tx, mut policy_rx) =
            tokio::sync::mpsc::unbounded_channel::<grodex_permission::PermissionPolicy>();

        let config_fs_backend = {
            let sources: Vec<grodex_config::FsWatchSource> = config
                .paths
                .ordered_paths()
                .into_iter()
                .filter(|(_, p)| p.exists())
                .map(|(label, p)| grodex_config::FsWatchSource {
                    source_id: label.to_string(),
                    path: p.to_path_buf(),
                    domain: grodex_config::ConfigDomain::Root,
                })
                .collect();
            if sources.is_empty() {
                None
            } else {
                let watcher = Arc::new(std::sync::Mutex::new(
                    grodex_config::ConfigWatcher::default(),
                ));
                let (pub_tx, pub_rx) = std::sync::mpsc::channel();
                let counter = Arc::new(std::sync::atomic::AtomicU64::new(
                    config.effective.generation + 1,
                ));
                let counter2 = counter.clone();
                // Hot-ADOPT (P3 fix): on Published the drain thread
                // re-resolves the FULL merged config from disk (the watched
                // file may be any layer — compiling a policy from the single
                // changed file would silently drop rules from other layers)
                // and forwards the new PermissionPolicy to the supervisor,
                // which swaps it in with a revocation-epoch bump. The
                // channel is tokio-unbounded so a std thread can send.
                let adopt_cwd = self.cwd.clone();
                let validator: grodex_config::ConfigValidator = Arc::new(move |bytes| {
                    let s = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
                    toml::from_str::<toml::Value>(s).map_err(|e| e.to_string())?;
                    Ok(counter2.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
                });
                match grodex_config::FsConfigBackend::start(
                    sources,
                    watcher,
                    validator,
                    pub_tx,
                ) {
                    Ok(backend) => {
                        // Drain pipeline decisions to stderr.
                        let _ = std::thread::Builder::new()
                            .name("config-publish-drain".into())
                            .spawn(move || {
                                use grodex_config::WatchOutcome;
                                while let Ok(p) = pub_rx.recv() {
                                    match &p.outcome {
                                        WatchOutcome::Published { generation } => {
                                            eprintln!(
                                                "[config] {} hot-reload: published generation {generation}",
                                                p.source_id
                                            );
                                            // Adopt: recompile the permission
                                            // policy from the stashed valid
                                            // config and hand it to the
                                            // supervisor (fail-open).
                                            match ConfigResolver::load(&adopt_cwd) {
                                                Ok(config) => {
                                                    let policy =
                                                        build_permission_policy(&config.effective.values);
                                                    if policy_tx.send(policy).is_err() {
                                                        // session shutting down — nothing to adopt into
                                                    }
                                                }
                                                Err(e) => eprintln!(
                                                    "[config] hot-adopt skipped (re-resolve failed, last-known-good policy retained): {e}"
                                                ),
                                            }
                                        }
                                        WatchOutcome::Unchanged => {}
                                        WatchOutcome::Rejected { diagnostic } => eprintln!(
                                            "[config] {} hot-reload rejected (last-known-good retained): {diagnostic}",
                                            p.source_id
                                        ),
                                        WatchOutcome::CachedFailure { diagnostic } => eprintln!(
                                            "[config] {} still invalid: {diagnostic}",
                                            p.source_id
                                        ),
                                        WatchOutcome::BreakerOpen { retry_after } => eprintln!(
                                            "[config] {} publish breaker open, retry in {}s",
                                            p.source_id,
                                            retry_after.as_secs()
                                        ),
                                    }
                                }
                            });
                        Some(backend)
                    }
                    Err(e) => {
                        eprintln!("[warn] config fs watcher failed to start (hot-reload disabled): {e}");
                        None
                    }
                }
            }
        };

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
        // Credential broker：凭证以配置文件方式托管在 ~/.grodex/credentials.json
        // （0600 权限，仅当前用户可读写），不读取系统钥匙串。
        // HOME 不可用时降级为内存 broker（安全，仅不持久）。
        let mut broker = std::env::var("HOME")
            .ok()
            .map(|home| {
                let cred_path = std::path::PathBuf::from(home)
                    .join(".grodex")
                    .join("credentials.json");
                grodex_auth::CredentialBroker::with_secret_store(std::sync::Arc::new(
                    grodex_auth::FileSecretStore::new(cred_path),
                ))
            })
            .or_else(|| {
                eprintln!("[auth] HOME 不可用，凭证仅保存在内存");
                Some(grodex_auth::CredentialBroker::empty())
            });
        if let Some(b) = broker.as_mut() {
            // 优先从凭证文件重建（重启存活）；未命中再用环境/配置的 key
            // 注册并持久化，下次启动即可直接从文件恢复。
            let hydrated = b.hydrate_provider(&provider_name).await.unwrap_or(false);
            if !hydrated {
                if let Some(k) = master_key {
                    b.register_provider(&provider_name, k);
                    let _ = b.persist_provider(&provider_name).await;
                }
            }
        }
        let api_key: Option<String> = (|| {
            let b = broker.as_mut()?;
            let lease = b.issue_lease(&provider_name, &audience)?;
            b.resolve(&lease, &audience).ok()
        })();

        // ── 3. SamplingActor ───────────────────────────────────────
        // Resolve the effective ModelRoute: explicit caller override takes
        // priority; otherwise derive from `[model_routes.default]` config
        // so both CLI REPL and ACP `serve_acp` get failover support.
        let model_route = self.model_route.clone()
            .or_else(|| route_toml.as_ref().map(|r| r.to_model_route()));
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
        let mut actor = SamplingActor::new(client);
        if let Some(route) = model_route.clone() {
            actor = actor.with_route(route);
        }
        let mut sub_actor = SamplingActor::new(sub_client);
        if let Some(route) = model_route.clone() {
            sub_actor = sub_actor.with_route(route);
        }
        let sub_actor = Arc::new(sub_actor);
        let chat_state = ChatStateActor::spawn();

        // ── 4. PermissionManager (policy from config `[rules]`) ────
        let policy = build_permission_policy(cfg);
        // The SQLite-backed broker is created AFTER the session directory
        // exists (below, after session creation). For now just build the
        // policy; the manager is constructed at step 6.5.
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
        let rollout: Option<Arc<dyn RolloutStore>> = FileRolloutStore::new_session(&base_dir, &session_id_str)
            .await
            .ok()
            .map(|s| Arc::new(s) as Arc<dyn RolloutStore>);
        let writer = rollout.as_ref().map(|s| {
            let mut w = RolloutWriter::new(s.clone(), session.id);
            if let Some(sink) = self.telemetry.clone() {
                w = w.with_telemetry(sink, self.run_id.clone());
            }
            w
        });
        // Re-project the journal into telemetry.db so events lost when a
        // previous process died before a telemetry commit are restored
        // (idempotent: journal-derived event_ids are deterministic).
        if writer.is_some() && self.telemetry.is_some() {
            let w = writer.clone().unwrap();
            tokio::spawn(async move {
                w.reproject_telemetry().await;
            });
        }

        // ── 6.5. PermissionManager (SQLite-backed if session dir exists) ──
        // Place the approval ticket DB alongside the rollout journal so
        // pending approvals survive crashes and can be restored on resume.
        // Fail-open to in-memory if the directory is unavailable.
        let approval_db_path = std::path::Path::new(&base_dir)
            .join(&session_id_str)
            .join("approvals.db");
        let permission_mgr = PermissionManager::new_with_db(policy, &approval_db_path);
        // Attach the approval bus BEFORE Arc-wrapping, so the coordinator
        // AND the DelegateTool share the same PermissionManager instance
        // (deny rules + session grants apply to sub-agent calls too).
        let (approval_tx, approval_rx) = tokio::sync::mpsc::unbounded_channel();
        let permission_mgr = permission_mgr.with_approval_bus(approval_tx);
        let permission_mgr = Arc::new(tokio::sync::Mutex::new(permission_mgr));

        // Compaction trigger threshold (% of context_window) and the
        // per-tool-result size cap — both optional config overrides that
        // are wired into the TurnCoordinator below.
        let compaction_threshold_pct = cfg
            .get("compaction_threshold_percent")
            .and_then(|v| v.as_integer())
            .map(|v| v.clamp(1, 100) as u8);
        let max_tool_result_bytes = cfg
            .get("max_tool_result_bytes")
            .and_then(|v| v.as_integer())
            .filter(|v| *v > 0)
            .map(|v| v as usize);
        // Per-turn sampling step budget. Long multi-tool tasks used to die
        // at the old hardcoded cap of 10 steps; default is now 40.
        let max_steps_per_turn = cfg
            .get("max_steps_per_turn")
            .and_then(|v| v.as_integer())
            .filter(|v| *v > 0)
            .map(|v| v as usize);
        // T10: per-tool execution timeout. When a tool runs longer than
        // this, it is cancelled and an error result is returned. `0` (default)
        // disables the timeout. Prevents a single hung tool from blocking
        // the entire turn indefinitely.
        let tool_timeout_secs = cfg
            .get("tool_timeout_secs")
            .and_then(|v| v.as_integer())
            .filter(|v| *v >= 0)
            .map(|v| v as u64);

        let model_config = self.model_config_override.unwrap_or_else(|| {
            // context_window: explicit config > built-in model table > 1M
            // fallback. The compaction manager uses this to decide when to
            // trigger — a too-small value causes premature compaction or
            // context overflow.
            let ctx_window = cfg
                .get("context_window")
                .and_then(|v| v.as_integer())
                .map(|v| v as u64)
                .unwrap_or_else(|| infer_context_window(&model_name));
            ModelConfig {
                provider: provider_name,
                model: model_name,
                wire_protocol,
                context_window: ctx_window,
            }
        });

        // ── 7. CapabilityManager + TurnCoordinator ─────────────────
        // Inject permission + sandbox so the coordinator dispatches tool
        // calls through the config-loaded policy (not an empty default)
        // and validates against the config profile.
        //
        // Oversized tool results are offloaded into a managed blob store
        // (Doc 11 §22): content-addressed files under `{tmp}/grodex-blobs/`
        // whose lifetime is governed by the blob_refs projection and
        // reclaimed at session shutdown (supervisor → release_session_blobs).
        let blob_store = Arc::new(grodex_tools::ManagedBlobStore::new(
            grodex_tools::FileBlobStore::new(std::env::temp_dir().join("grodex-blobs")),
            std::time::Duration::from_secs(30),
        ));
        let mut coordinator = TurnCoordinator::new(actor, chat_state.clone())
            .with_permission_arc(permission_mgr.clone(), approval_rx)
            .with_sandbox(sandbox)
            .with_context_window(model_config.context_window)
            .with_blob_store(blob_store);
        if let Some(pct) = compaction_threshold_pct {
            coordinator = coordinator.with_compaction_threshold(pct);
        }
        if let Some(bytes) = max_tool_result_bytes {
            coordinator = coordinator.with_max_tool_result_bytes(bytes);
        }
        if let Some(steps) = max_steps_per_turn {
            coordinator = coordinator.with_max_steps(steps);
        }
        if let Some(secs) = tool_timeout_secs {
            coordinator = coordinator.with_tool_timeout_secs(secs);
        }

        // Register built-in tools. Each gets a fresh runtime instance +
        // its JSON schema. The delegate_task tool is wired with:
        //   - a shared SamplingActor (so it can actually run sub-agent turns)
        //   - a RolloutWriter clone (so spawn/complete are journaled)
        coordinator
            .register_tool("read_file", Arc::new(ReadFileTool::new()), ReadFileTool::new().input_schema())
            .await;
        // load_skill:复用 supervisor 的 SkillCatalog 发现(cwd/trusted 一致),
        // 共享一份 Arc<Mutex<_>>,避免重复扫描且保证 load 的是同一批 skill。
        let skill_catalog = Arc::new(std::sync::Mutex::new(
            grodex_skills::SkillCatalog::discover(&self.cwd, workspace_trusted),
        ));
        coordinator
            .register_tool(
                "load_skill",
                Arc::new(LoadSkillTool::new(skill_catalog)),
                LoadSkillTool::default().input_schema(),
            )
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
        coordinator
            .register_tool("web_fetch", Arc::new(WebFetchTool::new()), WebFetchTool::new().input_schema())
            .await;
        // grep / glob: read-only codebase search tools. Give the model
        // grep (content search) and glob (file-pattern search) so it
        // doesn't need read_file for every search operation.
        coordinator
            .register_tool("grep", Arc::new(GrepTool::new()), GrepTool::new().input_schema())
            .await;
        coordinator
            .register_tool("glob", Arc::new(GlobTool::new()), GlobTool::new().input_schema())
            .await;
        // ── Subagent progress channel ───────────────────────────
        // The DelegateTool sends structured lifecycle events
        // (started/step/finished) via this channel. The forwarder task
        // below converts them to `SessionEvent::SubagentProgress` so
        // the TUI renders each sub-agent as a collapsible card.
        let (subagent_progress_tx, mut subagent_progress_rx) =
            mpsc::unbounded_channel::<grodex_loop::delegate_tool::SubagentProgress>();
        let subagent_progress_tx = Arc::new(subagent_progress_tx);

        // Sub-agent caps: `max_subagents` = concurrent limit (default 4);
        // the session total defaults to 4x that.
        let max_subagents = cfg
            .get("max_subagents")
            .and_then(|v| v.as_integer())
            .filter(|v| *v > 0)
            .map(|v| v as usize);
        let max_subagents_total = cfg
            .get("max_subagents_per_session")
            .and_then(|v| v.as_integer())
            .filter(|v| *v > 0)
            .map(|v| v as usize);

        let mut delegate = DelegateTool::new(SubAgentConfig::default())
            .with_sampling(sub_actor, model_config.clone())
            .with_progress_sender(subagent_progress_tx.clone())
            .with_limits(max_subagents.unwrap_or(0), max_subagents_total.unwrap_or(0))
            // Sub-agents get read-only tools so analysis tasks can
            // actually inspect the codebase (bypasses the approval
            // round-trip — these have no side effects).
            .with_readonly_tools(vec![
                ("read_file".to_string(),
                 Arc::new(ReadFileTool::new()) as Arc<dyn ToolRuntime>,
                 ReadFileTool::new().input_schema()),
                ("grep".to_string(),
                 Arc::new(GrepTool::new()) as Arc<dyn ToolRuntime>,
                 GrepTool::new().input_schema()),
                ("glob".to_string(),
                 Arc::new(GlobTool::new()) as Arc<dyn ToolRuntime>,
                 GlobTool::new().input_schema()),
            ]);
        if let Some(ref w) = writer {
            delegate = delegate.with_writer(w.clone(), SubAgentConfig::default());
        }
        // P3 fix: sub-agent tool calls go through the shared permission
        // gate (deny rules apply; Ask fails closed inside a sub-agent).
        let delegate = delegate.with_permission(permission_mgr.clone());
        // Shared Arc: the collaboration-protocol followup executor reuses
        // the same DelegateTool child loop.
        let delegate = Arc::new(delegate);
        let subagent_recovery = delegate.durable_supervisor();
        let delegate_schema = delegate.input_schema();
        coordinator
            .register_tool("delegate_task", delegate.clone(), delegate_schema)
            .await;

        // ── 7c. Parent-child collaboration protocol tools (Doc 12) ──
        // send_message / followup_task / wait_agent / mailbox_read /
        // list_agents / interrupt_agent. The session itself is the root
        // agent; followup TaskRuns execute through the DelegateTool above.
        let protocol_host = Arc::new(grodex_loop::protocol_tools::ProtocolToolHost::new(
            max_subagents_total.unwrap_or(16),
            Default::default(),
        ));
        for (name, runtime, schema) in protocol_host.tool_set(Some(delegate.clone())) {
            coordinator.register_tool(name, runtime, schema).await;
        }

        // ── 7b. MCP tools (from config `[[mcp_server]]`) ──────────
        // For each enabled MCP server, spawn the process, list tools,
        // wrap each as McpToolAdapter, and register with the coordinator.
        // Fail-open: if a server can't spawn or list tools, log and skip —
        // the session continues without those MCP tools.
        // OAuth：带 `oauth` 块的 server 注册进 McpOAuthCoordinator，
        // 授权流程由调用方（TUI/CLI）在需要时驱动。
        let mut mcp_oauth: Option<grodex_mcp::McpOAuthCoordinator> = None;
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
                // 注册 OAuth 客户端配置（无 oauth 块时为 no-op）。
                if mcp_config.requires_oauth() {
                    // Persist OAuth master tokens so a separate
                    // `grodex mcp-auth <server>` run (or a session
                    // restart) can rehydrate credentials.
                    let coord = mcp_oauth.get_or_insert_with(|| {
                        match std::env::var("HOME") {
                            Ok(home) => {
                                let store = std::sync::Arc::new(
                                    grodex_auth::FileSecretStore::new(
                                        std::path::PathBuf::from(home)
                                            .join(".grodex")
                                            .join("credentials.json"),
                                    ),
                                );
                                grodex_mcp::McpOAuthCoordinator::with_secret_store(store)
                            }
                            Err(_) => grodex_mcp::McpOAuthCoordinator::new(),
                        }
                    });
                    match coord.register_server(&mcp_config) {
                        Ok(_) => eprintln!(
                            "[mcp] server '{}' 需要 OAuth 授权（已注册，等待 begin_authorization）",
                            mcp_config.name
                        ),
                        Err(e) => {
                            eprintln!("[warn] MCP server '{}' OAuth 注册失败: {e}", mcp_config.name);
                        }
                    }
                }
                // P3 telemetry: MCP spawn/list_tools timing (out-of-band,
                // fail-open — telemetry failures never affect the session).
                let emit_mcp = |phase: &'static str, status: &'static str, dur: u64, tool_count: usize, error: Option<&str>| {
                    if let Some(sink) = &self.telemetry {
                        let mut rec = grodex_telemetry::TelemetryRecord::out_of_band(
                            &self.run_id, &session_id_str, grodex_telemetry::kind::MCP_LIFECYCLE,
                        );
                        rec.payload_json = serde_json::json!({
                            "server_name": mcp_config.name,
                            "phase": phase,
                            "transport": "stdio",
                            "tool_count": tool_count,
                            "status": status,
                            "error_class": error,
                            "duration_ms": dur,
                        }).to_string();
                        rec.severity = if status == "failed" {
                            grodex_telemetry::Severity::Warn
                        } else {
                            grodex_telemetry::Severity::Info
                        };
                        sink.emit(rec);
                    }
                };

                let spawn_started = std::time::Instant::now();
                match grodex_mcp::McpProcess::spawn(mcp_config.clone()).await {
                    Ok(mut process) => {
                        emit_mcp("spawn", "ok", spawn_started.elapsed().as_millis() as u64, 0, None);
                        let list_started = std::time::Instant::now();
                        match process.list_tools().await {
                            Ok(tools) => {
                                emit_mcp("list_tools", "ok", list_started.elapsed().as_millis() as u64, tools.len(), None);
                                for tool in tools {
                                    let adapter = grodex_mcp::McpToolAdapter::new(
                                        mcp_config.clone(),
                                        tool.name.clone(),
                                        tool.description.clone(),
                                        tool.input_schema.clone(),
                                    );
                                    let schema = adapter.input_schema();
                                    let name = adapter.full_name().to_string();
                                    let metadata = adapter.metadata();
                                    coordinator
                                        .register_tool_with_metadata(&name, Arc::new(adapter), schema, metadata)
                                        .await;
                                }
                            }
                            Err(e) => {
                                emit_mcp("list_tools", "failed", list_started.elapsed().as_millis() as u64, 0, Some(&e.to_string()));
                                eprintln!("[warn] MCP server '{}' list_tools failed: {e}", mcp_config.name);
                            }
                        }
                    }
                    Err(e) => {
                        emit_mcp("spawn", "failed", spawn_started.elapsed().as_millis() as u64, 0, Some(&e.to_string()));
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
        // Opens the SQLite + FTS5 database. Path precedence:
        //   GRODEX_MEMORY_DB env > config `memory.path` > ~/.grodex/memory.db
        // Every candidate goes through `expand_user_path` (a literal `~`
        // from config/env must never reach the filesystem) and its parent
        // directory is created up front.
        // Fail-open: if the DB cannot be opened (permissions, disk full),
        // memory is set to None — turns proceed without RAG.
        let memory_db_path: Option<std::path::PathBuf> = match std::env::var("GRODEX_MEMORY_DB") {
            Ok(p) => Some(grodex_config::expand_user_path(&p)),
            Err(_) => cfg
                .get("memory")
                .and_then(|m| m.get("path"))
                .and_then(|v| v.as_str())
                .map(grodex_config::expand_user_path)
                .or_else(|| dirs::home_dir().map(|h| h.join(".grodex").join("memory.db"))),
        };
        let memory = memory_db_path.and_then(|db_path| {
            if let Some(parent) = db_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            grodex_memory::MemoryDatabase::open(&db_path)
                .ok()
                .map(Arc::new)
        });

        // Initial index scan + reconcile: walk the workspace for .md files,
        // diff against the indexed_files table, and apply deletions. New/changed
        // files are registered in indexed_files (content parsing into MemoryUnit
        // is deferred to a later phase when a Markdown parser is available).
        // Fail-open: scan errors must not block session startup.
        //
        // P3 fix: the scan previously ran ONCE at startup, so memory never
        // saw any file changed after boot. A periodic rescan task re-runs
        // the same reconcile (default every 10 min,
        // `GRODEX_MEMORY_RESCAN_SECS` overrides; 0 disables).
        if let Some(ref db) = memory {
            reindex_memory(db, &self.cwd);
            let rescan_secs: u64 = std::env::var("GRODEX_MEMORY_RESCAN_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600);
            if rescan_secs > 0 {
                let db = db.clone();
                let cwd = self.cwd.clone();
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(std::time::Duration::from_secs(rescan_secs));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    tick.tick().await; // first tick fires immediately — skip
                    loop {
                        tick.tick().await;
                        let db = db.clone();
                        let cwd = cwd.clone();
                        let _ = tokio::task::spawn_blocking(move || reindex_memory(&db, &cwd)).await;
                    }
                });
            }
        }

        // Embedding model for hybrid RAG (FTS5 + vector). All model
        // parameters come from the `[memory.embedding]` config section;
        // defaults live in `EmbeddingConfig`'s serde layer, never here.
        // Fail-open: if embedding is not configured (enabled=false or
        // missing API key env var), the model is None and
        // `retrieve_hybrid_memory` degrades to pure FTS5 ranking.
        let emb_cfg: grodex_memory::EmbeddingConfig = cfg
            .get("memory")
            .and_then(|m| m.get("embedding"))
            .cloned()
            .and_then(|v| v.try_into().ok())
            .unwrap_or_default();
        let backfill_max_documents = emb_cfg.backfill_max_documents;
        let embedding: Option<Arc<dyn grodex_memory::EmbeddingModel + Send + Sync>> = {
            match grodex_memory::embedding::OpenAiCompatibleModel::new(emb_cfg) {
                Ok(model) => Some(Arc::new(model) as Arc<dyn grodex_memory::EmbeddingModel + Send + Sync>),
                Err(_) => None, // not configured — pure FTS5
            }
        };

        // Startup backfill: embed active memory/evidence units that lack
        // vectors for the current model, in the background so session
        // startup never blocks on the embedding endpoint. Fail-open.
        if let (Some(db), Some(model)) = (&memory, &embedding) {
            let db = db.clone();
            let model = model.clone();
            tokio::spawn(async move {
                match grodex_memory::backfill_missing_embeddings(
                    &db,
                    model.as_ref(),
                    backfill_max_documents,
                )
                .await
                {
                    Ok(0) => {}
                    Ok(n) => eprintln!("[memory] embedding backfill: {n} documents"),
                    Err(e) => eprintln!("[warn] embedding backfill failed (staying pure FTS): {e}"),
                }
            });
        }

        // P1-3: Wire the memory database into the TurnCoordinator so
        // non-error tool results are captured as EvidenceUnit entries
        // (Tool Result → Evidence). The supervisor separately uses the
        // same `memory` Arc for RAG retrieval in the prompt.
        let coordinator = if let Some(ref db) = memory {
            coordinator.with_memory(db.clone())
        } else {
            coordinator
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
        // Doc 19 §7.3 compat vendor opt-in flows into instruction
        // discovery (cached once per session inside the supervisor).
        supervisor.set_discovery_config(grodex_prompt::DiscoveryConfig {
            compat_vendors: instruction_compat_vendors,
            ..Default::default()
        });

        let (event_broadcast_tx, event_broadcast_rx) =
            mpsc::channel::<LoopSessionEvent>(128);
        let supervisor_task = tokio::spawn(async move {
            supervisor.run().await;
        });

        // Fan out the supervisor's single event stream to multiple
        // consumers (ACP stdio writer, CLI REPL, future TUI). The
        // supervisor owns `event_rx`; we forward every event to the
        // broadcast channel and hand the receive end to callers.
        //
        // Also drain the subagent progress channel: each String message
        // from DelegateTool is converted to `SessionEvent::Info` so the
        // TUI renders subagent lifecycle notifications inline with the
        // conversation (instead of a silent block while the sub-agent
        // runs).
        let SessionHandle { cmd_tx, mut event_rx } = handle;
        // Forward hot-adopted permission policies from the config drain
        // thread (std) into the supervisor's command loop (async).
        {
            let cmd_tx = cmd_tx.clone();
            tokio::spawn(async move {
                while let Some(policy) = policy_rx.recv().await {
                    if cmd_tx
                        .send(grodex_loop::command::SessionCommand::AdoptPermissionPolicy { policy })
                        .await
                        .is_err()
                    {
                        break; // session gone
                    }
                }
            });
        }
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Supervisor events (text, tool calls, errors, …).
                    ev = event_rx.recv() => {
                        match ev {
                            Some(ev) => {
                                if event_broadcast_tx.send(ev).await.is_err() {
                                    break;
                                }
                            }
                            None => break, // supervisor shut down
                        }
                    }
                    // Subagent progress notifications → SubagentProgress events.
                    msg = subagent_progress_rx.recv() => {
                        match msg {
                            Some(progress) => {
                                let ev = LoopSessionEvent::SubagentProgress(progress);
                                if event_broadcast_tx.send(ev).await.is_err() {
                                    break;
                                }
                            }
                            None => {} // DelegateTool dropped — no more progress
                        }
                    }
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
            mcp_oauth,
            model_config,
            supervisor_task,
            config_fs_backend,
            subagent_recovery,
        })
    }
}

/// Build a `PermissionPolicy` from the config.
///
/// Two config forms are supported:
///
/// ## Simple form (backward-compatible)
/// ```toml
/// [rules]
/// read_file = "allow"
/// write_file = "ask"
/// exec = "deny"
/// ```
/// Each key is a tool name (or `*`), each value is `allow` / `ask` / `deny`.
///
/// ## Extended form (with matchers)
/// ```toml
/// [[permission_rules]]
/// tool = "exec"
/// decision = "allow"
/// priority = 10
/// command = { pattern = "git *" }
///
/// [[permission_rules]]
/// tool = "write_file"
/// decision = "deny"
/// resource = { arg_path = "/path", pattern = "/etc/*" }
///
/// [[permission_rules]]
/// tool = "read_file"
/// decision = "allow"
/// arg_patterns = [{ arg_path = "/path", pattern = "/tmp/*" }]
/// ```
///
/// Both forms can coexist in the same config. Simple `[rules]` entries
/// are loaded first (lower priority), then `[[permission_rules]]` entries
/// are loaded (can override with higher `priority`).
///
/// When neither `[rules]` nor `[[permission_rules]]` is present, a safe
/// default is applied: `read_file` → Allow, `*` → Ask.
fn build_permission_policy(cfg: &toml::Value) -> PermissionPolicy {
    let mut policy = PermissionPolicy::new();

    let has_simple = cfg.get("rules").and_then(|v| v.as_table()).is_some();
    let has_extended = cfg.get("permission_rules").and_then(|v| v.as_array()).is_some();

    if !has_simple && !has_extended {
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
    }

    // Simple form: [rules]
    if let Some(rules_tbl) = cfg.get("rules").and_then(|v| v.as_table()) {
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
    }

    // Extended form: [[permission_rules]]
    if let Some(ext_rules) = cfg.get("permission_rules").and_then(|v| v.as_array()) {
        for entry in ext_rules {
            let Some(tool) = entry.get("tool").and_then(|v| v.as_str()) else { continue };
            let Some(decision_str) = entry.get("decision").and_then(|v| v.as_str()) else { continue };
            let decision = match decision_str.to_lowercase().as_str() {
                "allow" => PolicyDecision::Allow,
                "deny" => PolicyDecision::Deny,
                "ask" => PolicyDecision::Ask,
                _ => continue,
            };
            let priority = entry.get("priority").and_then(|v| v.as_integer()).unwrap_or(0) as u8;

            // Parse optional matchers via serde_json conversion (toml Value → serde_json Value)
            let toml_to_json = |key: &str| -> Option<serde_json::Value> {
                entry.get(key).map(|v| toml_to_json_value(v))
            };

            let command = toml_to_json("command")
                .and_then(|v| serde_json::from_value(v).ok());
            let resource = toml_to_json("resource")
                .and_then(|v| serde_json::from_value(v).ok());
            let network = toml_to_json("network")
                .and_then(|v| serde_json::from_value(v).ok());
            let mcp = toml_to_json("mcp")
                .and_then(|v| serde_json::from_value(v).ok());
            let rule_id = entry.get("rule_id").and_then(|v| v.as_str()).map(|s| s.to_string());

            // Parse arg_patterns array
            let arg_patterns = entry.get("arg_patterns")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| {
                            let json_val = toml_to_json_value(v);
                            serde_json::from_value(json_val).ok()
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            policy.add_rule(PolicyRule {
                tool_pattern: tool.to_string(),
                arg_patterns,
                command,
                resource,
                rule_id,
                network,
                mcp,
                decision,
                priority,
            });
        }
    }

    policy
}

/// Convert a `toml::Value` to `serde_json::Value` for deserializing
/// complex matchers from TOML config.
fn toml_to_json_value(v: &toml::Value) -> serde_json::Value {
    match v {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::Number((*i).into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Array(arr) => serde_json::Value::Array(
            arr.iter().map(toml_to_json_value).collect(),
        ),
        toml::Value::Table(tbl) => {
            let map: serde_json::Map<String, serde_json::Value> = tbl
                .iter()
                .map(|(k, v)| (k.clone(), toml_to_json_value(v)))
                .collect();
            serde_json::Value::Object(map)
        }
        _ => serde_json::Value::Null,
    }
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

/// One memory index pass: scan the workspace for .md files, diff against
/// `indexed_files`, apply deletions and register new/changed files.
/// Blocking (fs walk) — call from `spawn_blocking`. Fail-open by caller.
fn reindex_memory(db: &Arc<grodex_memory::MemoryDatabase>, cwd: &std::path::Path) {
    let scanned = grodex_memory::scan_directory(cwd);
    let diff = grodex_memory::reconcile(db, &scanned);
    if diff.is_empty() {
        return;
    }
    // Apply deletions (orphan units, bump generation).
    let _ = grodex_memory::apply_deletions(db, &diff);
    // Register new/changed files in indexed_files so the index table
    // reflects the current filesystem state.
    let current_gen = db.read_generation().unwrap_or(1);
    for file in diff.new_files.iter().chain(diff.changed_files.iter()) {
        let indexed = grodex_memory::IndexedFile {
            path: file.key.clone(),
            source_kind: grodex_memory::types::SourceKind::Memory,
            mtime: file.mtime,
            size: file.size as i64,
            content_hash: file.content_hash.clone(),
            index_generation: current_gen,
            last_indexed_at: chrono::Utc::now(),
        };
        let _ = db.upsert_indexed_file(&indexed);
    }
    // Bump generation after inserts so the snapshot invalidates.
    let _ = db.bump_generation();
    eprintln!(
        "[memory] reindex: +{} new, ~{} changed, -{} removed",
        diff.new_files.len(),
        diff.changed_files.len(),
        diff.deleted_files.len()
    );
}
