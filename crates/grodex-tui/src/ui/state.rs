use grodex_protocol::acp::{RequestPermissionPayload, SessionEvent};

#[derive(Clone)]
pub struct TimestampedEvent {
    pub seq: u64,
    pub at_ms: u64,
    pub event: SessionEvent,
    pub consumed: bool,
}

#[derive(Clone)]
pub struct PendingApprovalRow {
    pub ticket_id: String,
    pub tool_name: String,
    pub summary: String,
    pub risk: String,
    pub remaining_s: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Prompt,
    Command,
}

/// A chat message in the conversation view.
/// TextDelta / ThoughtDelta events are accumulated into a single block
/// for SSE streaming effect. ToolCallStart / Args / End / Result are
/// mapped to a single Tool card indexed by call_id.
#[derive(Clone)]
pub enum ChatMessage {
    /// User-submitted prompt.
    User { text: String },
    /// Assistant response — accumulates TextDelta chunks.
    Assistant { text: String, done: bool },
    /// Reasoning / thinking — accumulates ThoughtDelta chunks (Grok-style).
    Thinking { text: String, done: bool },
    /// Full tool call card — Start / Args / End / Result aggregate here.
    /// Status progression: starting → parsing-args → running → done/error.
    Tool {
        name: String,
        call_id: Option<String>,
        args: String,
        result: Option<String>,
        is_error: bool,
        done: bool,       // ToolCallEnd: agent side finished generating call
        has_result: bool, // ToolResult: agent returned output
    },
    /// System / error message.
    System { text: String, is_error: bool },
}

// ── Slash command table (faithfully mirrors Grok's builtin_commands) ─
//
// **Critical rule (matching Grok/xai-grok-pager/src/slash/commands/mod.rs
// builtin_commands() + shell_collision_contract_covers_every_pager_command_and_alias
// test)**:
//
//   RECOGNIZED `/commands` ARE **NEVER** FORWARDED TO THE MODEL. They all
//   produce a local result — either a real TUI action, an ACP-scope action
//   (sent to the session/agent as a *command*, not as user prompt text),
//   or an explicit "this feature isn't wired up yet" diagnostic. The ONLY
//   input that reaches the LLM is: (a) plain text that does not start
//   with `/`; (b) an explicit mid-text `/token` that was NOT recognized
//   (i.e. truly unknown; those are also *blocked* here, not silently
//   leaked, because they're almost always typos and the user shouldn't
//   burn tokens on a mis-typed `/exit`).
//
// Commands are grouped by their execution-scope tag in `SlashLocalKind`.
// Aliases follow the exact mapping used in Grok's contract test so that
// `/m` = `/model`, `/undo` = `/rewind`, `/cost` = `/usage`, `/summarize`
// = `/recap`, `/welcome` = `/help`, `/title` = `/rename`, `/sessions` =
// `/history`, `/full` = `/fullscreen`, `/minimal` = screen-mode switch,
// `/show-plan`/`/plan-view` = `/view-plan`, `/prefs`/`/preferences` =
// `/settings`, `/config` = `/config-agents`, `/yolo` = `/auto`, `/ml` =
// `/multiline`, `/changelog` = `/release-notes`, `/guides`/`/howto` =
// `/docs`, `/cloud` = `/share`, `/chat` = `/home`, `/clear` = `/delete`,
// `/log` = `/history`, `/terminal-check`/`/terminal-info`/`/terminal-setup`
// = `/doctor`, `/t` = `/tasks`, `/tour` = `/tutorial`, `/onboarding` =
// `/tutorial`, `/marketplace` = plugins/marketplace, `/skills` =
// plugins/skills, `/hooks` = plugins/hooks, `/plugins` = plugins/list.

#[derive(Debug, Clone)]
pub struct SlashCommandDef {
    /// Name without leading `/`, e.g. "exit" or an alias like "quit".
    pub name: &'static str,
    /// One-line description shown in the dropdown (dim).
    pub description: &'static str,
    /// Execution scope. See `SlashLocalKind` for the exact semantics.
    pub local: SlashLocalKind,
}

/// Execution scope of a recognized slash command. Every recognized command
/// resolves to exactly one of these — **NONE** of them reach the model as
/// a prompt. This is the critical invariant Grok enforces via its
/// `SlashCommand::run()` return-type contract (`CommandResult::Action` /
/// `QueueCommand` / `PassThrough`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashLocalKind {
    // ── TUI-local: the TUI process can handle them without round-tripping
    //    to the agent. We implement these fully.
    /// Exit the TUI (like `q` in Normal mode). Commands: /exit /quit /q.
    Exit,
    /// Show the built-in help block. Commands: /help /? /welcome.
    Help,
    /// Delete chat transcript (session-scoped, but done locally by
    /// clearing `messages` + emitting an ACP `DeleteCurrentSession` hint
    /// for the backend so disk state stays in sync). Commands: /delete /clear.
    DeleteCurrentSession,
    /// Clear the current input buffer only. Command: /reset.
    ClearInput,
    /// Toggle terminal mouse capture.
    ///   /mouse on      → enable wheel scroll inside the app (default)
    ///   /mouse off     → disable capture so terminal native text selection works
    ///   /mouse toggle  → flip current state
    ///   /mouse         → show current state
    Mouse { sub: String },

    // ── ACP / session-scoped: logically belong to Grok's "Action" set.
    //    We intercept them here so they never leak to the LLM; for the
    //    subset the agent backend actually understands we emit a
    //    `TuiAction::RunAcpSlash` so the main loop can forward it over
    //    the transport as a structured `SessionCommand` frame. Everything
    //    else in this bucket prints a "[ACP] not wired yet" system event
    //    so users never silently get an LLM response.
    /// Start a brand-new blank session. /new.
    AcpNewSession,
    /// Return to the agent dashboard (exit current session, keep history).
    /// /home /chat.
    AcpExitToDashboard,
    /// Fork the current session (preserve history, continue independently).
    /// /fork.
    AcpForkSession,
    /// Resume a previous session by id. /resume <session_id>.
    AcpResumeSession,
    /// List recent sessions. /history /sessions /log.
    AcpListSessions,
    /// Jump to a specific turn id. /jump <turn_id>.
    AcpJumpToTurn,
    /// Rewind to a specific turn id and re-generate from there. /rewind
    /// /undo <turn_id>.
    AcpRewind,
    /// Trigger a manual context compaction (optionally with a hint).
    /// /compact [reason] /summarize <hint> when no recap.
    AcpCompact,
    /// Display / manage loaded context nodes. /context [list|drop <id>].
    AcpContext,
    /// Manually request a session recap. /recap /summarize.
    AcpRecap,
    /// Tell the long-term memory system to remember <fact>.
    /// /remember [fact].
    AcpRemember,
    /// Switch default model. /model /m <name or display>.
    AcpSetModel,
    /// Set / toggle effort level. /effort [low|med|high|max].
    AcpSetEffort,
    /// Toggle "always approve" (yolo-lite). /always-approve /yolo when auto=false.
    AcpToggleAlwaysApprove,
    /// Toggle full-auto mode (approve everything). /auto /yolo.
    AcpToggleAuto,
    /// Toggle multiline input mode. /multiline /ml.
    AcpToggleMultiline,
    /// Toggle compact (density) UI mode. /compact-mode.
    AcpToggleCompactMode,
    /// Toggle Vim keybindings in scrollback panes. /vim-mode.
    AcpToggleVimMode,
    /// Switch render mode to minimal / fullscreen. /minimal /fullscreen /full.
    AcpSwitchScreenMode,
    /// Change current working directory (dashboard-only in Grok). /cd <path>.
    AcpChdir,
    /// Display current plan. /plan <on|off|text> for toggle + set; /view-plan
    /// /show-plan /plan-view just display.
    AcpPlan,
    /// Show / manage queues (scheduled / background tasks). /queue.
    AcpQueue,
    /// Show / manage task tracker entries. /tasks /t.
    AcpTasks,
    /// MCP servers list / management. /mcp /mcps.
    AcpMcpServers,
    /// Workflows list / management. /workflows.
    AcpWorkflows,
    /// Hooks (plugin lifecycle) commands. /hooks.
    AcpHooks,
    /// Plugins list. /plugins.
    AcpPlugins,
    /// Plugin marketplace. /marketplace.
    AcpMarketplace,
    /// Skills list (plugins/skills command in Grok). /skills.
    AcpSkills,
    /// Session rename. /rename /title <new name>.
    AcpRename,
    /// Session info (metadata). /session-info.
    AcpSessionInfo,
    /// Share session (public link). /share /cloud.
    AcpShare,
    /// Run workspace doctor. /doctor /terminal-check /terminal-info /terminal-setup.
    AcpDoctor,
    /// Usage / billing submenu. /usage /cost [show|manage].
    AcpUsage,
    /// Settings editor. /settings /preferences /prefs /config /config-agents.
    AcpSettings,
    /// Persona (role) selector + editor. /personas /agents /roles.
    AcpPersonas,
    /// Theme / color-scheme switcher. /theme.
    AcpTheme,
    /// Release notes viewer. /release-notes /changelog.
    AcpReleaseNotes,
    /// Tutorial / onboarding walkthrough. /tutorial /tour /onboarding.
    AcpTutorial,
    /// Documentation viewer. /docs /guides /howto.
    AcpDocs,
    /// Find-in-scrollback. /find <query>.
    AcpFindInScrollback,
    /// Export session. /export /transcript.
    AcpExport,
    /// Copy last assistant reply. /copy.
    AcpCopy,
    /// Voice (microphone) mode toggle. /voice.
    AcpVoice,
    /// Loop (scheduled agent invocation). /loop <cron> <prompt>.
    AcpLoop,
    /// Image generation. /imagine <prompt>.
    AcpImagine,
    /// Video generation. /imagine-video <prompt>.
    AcpImagineVideo,
    /// BTW mode toggle (agent answers in-line without creating new turns).
    /// /btw.
    AcpBtw,
    /// Feedback reporter. /feedback.
    AcpFeedback,
    /// Announcements viewer. /announcements.
    AcpAnnouncements,
    /// Toggle timestamps on messages. /timestamps.
    AcpTimestamps,
    /// Timeline (turn list) viewer. /timeline.
    AcpTimeline,
    /// Import ChatGPT / Claude history. /import-claude.
    AcpImportHistory,
    /// Login (grok auth). /login.
    AcpLogin,
    /// Logout. /logout.
    AcpLogout,
    /// Toggle mouse reporting. /toggle-mouse-reporting.
    AcpToggleMouse,
    /// Privacy center. /privacy.
    AcpPrivacy,
    /// Edit the current prompt draft in a separate editor. /edit-prompt (minimal-mode only in Grok).
    AcpEditPrompt,
    /// Expand to fullscreen (minimal-mode only). /expand.
    AcpExpand,
    /// View dashboard. /dashboard.
    AcpDashboard,
    /// /config-agents alias handled above.

    // ── Grodex-specific additions (not in Grok but useful for us).
    /// Toggle workspace trust level on next session. /trust on|off.
    GrodexTrust,
    /// Switch active provider. /provider <name>.
    GrodexProvider,
    /// Print current working directory. /cwd.
    GrodexShowCwd,
    /// List available tools for the current trust level. /tools.
    GrodexListTools,
    /// List all models supported by the active provider. /models.
    GrodexListModels,
    /// Show last N transport+event diagnostics (TUI-local debug HUD).
    /// /debug /logs /scroll-debug.
    GrodexDebugLog,
    /// Let agent forget a topic. /forget <terms>.
    GrodexForget,

    // ── Reserved but intentionally hidden from the menu. Grok keeps these
    //    registered (so bare invocation never leaks to the model) but never
    //    offers them in completion. Match Grok's fail-closed contract.
    /// Easter-egg game. Hidden. /gboom.
    HiddenGboom,
    /// Scroll-debug HUD toggle. Hidden. /scroll-debug handled above.
    HiddenScrollDebug,
    /// Generic debug flag toggles. Hidden. /debug handled above.
    HiddenDebug,

    // ── IMPORTANT: `Forward` no longer exists. Any command that ends up
    //    mapped here was a slip in the builtin table; we treat it as
    //    Unsupported (block locally) rather than silently leaking to LLM.
    /// Fallback bucket. Same runtime behaviour as ACP-unwired: block, log,
    /// never LLM. Kept as a distinct discriminant only so the old
    /// `Forward` string in call sites can be grepped and removed.
    Unsupported,
}

// 75 + aliases ≈ 120 entries total. Menu groups mirror Grok:
//   (session) → (workspace) → (context) → (capability) → (mode) →
//   (sharing/diagnostics) → (Grodex extras) → (hidden).
pub const BUILTIN_SLASH_COMMANDS: &[SlashCommandDef] = &[
    // ── TUI-local ──────────────────────────────────────────────────────
    SlashCommandDef { name: "exit",        description: "退出 TUI（同 q）",                                                       local: SlashLocalKind::Exit },
    SlashCommandDef { name: "quit",        description: "退出 TUI（/exit 别名）",                                                 local: SlashLocalKind::Exit },
    SlashCommandDef { name: "q",           description: "退出 TUI（/exit 短别名，Normal 肌肉记忆）",                               local: SlashLocalKind::Exit },
    SlashCommandDef { name: "help",        description: "显示内置帮助（键盘 + 命令一览）",                                         local: SlashLocalKind::Help },
    SlashCommandDef { name: "?",           description: "显示帮助（/help 别名）",                                                  local: SlashLocalKind::Help },
    SlashCommandDef { name: "welcome",     description: "显示帮助 + 欢迎界面（/help 别名）",                                       local: SlashLocalKind::Help },
    SlashCommandDef { name: "delete",      description: "结束并删除当前会话（聊天记录清空）",                                       local: SlashLocalKind::DeleteCurrentSession },
    SlashCommandDef { name: "clear",       description: "清空会话（/delete 别名，注意：不等同清输入，那是 /reset）",                local: SlashLocalKind::DeleteCurrentSession },
    SlashCommandDef { name: "reset",       description: "仅清空当前输入框（会话完整保留）",                                         local: SlashLocalKind::ClearInput },
    // NOTE: /mouse uses run-local interpretation (not the SlashLocalKind
    // variant), because the subcommand (on/off/toggle) is encoded in the
    // args string, not in a static SlashLocalKind payload. We still
    // register the definition so `/mou<TAB>` completion works, and the
    // command entry is visible in /help.
    SlashCommandDef { name: "mouse",       description: "显示鼠标模式（滚轮+选择始终同时可用）",                                          local: SlashLocalKind::Mouse { sub: String::new() } },

    // ── ACP: Session & lifecycle ──────────────────────────────────────
    SlashCommandDef { name: "new",         description: "开启全新的空白会话",                                                      local: SlashLocalKind::AcpNewSession },
    SlashCommandDef { name: "home",        description: "返回 agent 主仪表板，结束当前会话（保留历史）",                            local: SlashLocalKind::AcpExitToDashboard },
    SlashCommandDef { name: "chat",        description: "同 /home，切回会话列表入口",                                              local: SlashLocalKind::AcpExitToDashboard },
    SlashCommandDef { name: "fork",        description: "分叉当前会话：上下文保留，之后各自独立",                                    local: SlashLocalKind::AcpForkSession },
    SlashCommandDef { name: "resume",      description: "恢复某会话（/resume <session_id>）",                                       local: SlashLocalKind::AcpResumeSession },
    SlashCommandDef { name: "history",     description: "列出最近会话历史（可选择恢复）",                                          local: SlashLocalKind::AcpListSessions },
    SlashCommandDef { name: "sessions",    description: "同 /history（别名）",                                                     local: SlashLocalKind::AcpListSessions },
    SlashCommandDef { name: "log",         description: "同 /history（别名）",                                                     local: SlashLocalKind::AcpListSessions },
    SlashCommandDef { name: "jump",        description: "跳转到指定 turn（/jump <turn_id>）",                                      local: SlashLocalKind::AcpJumpToTurn },
    SlashCommandDef { name: "rewind",      description: "回退到某 turn 并重新生成（/rewind <turn_id>）",                           local: SlashLocalKind::AcpRewind },
    SlashCommandDef { name: "undo",        description: "同 /rewind（别名）",                                                      local: SlashLocalKind::AcpRewind },

    // ── ACP: Workspace & environment ──────────────────────────────────
    SlashCommandDef { name: "cd",          description: "切换当前工作目录（/cd <path>）",                                          local: SlashLocalKind::AcpChdir },
    SlashCommandDef { name: "dashboard",   description: "显示 agent 仪表板首页",                                                   local: SlashLocalKind::AcpDashboard },

    // ── ACP: Context management ───────────────────────────────────────
    SlashCommandDef { name: "compact",     description: "手动触发上下文压缩（/compact [reason]）",                                 local: SlashLocalKind::AcpCompact },
    SlashCommandDef { name: "context",     description: "显示/管理当前上下文装载（/context list|drop <id>）",                      local: SlashLocalKind::AcpContext },
    SlashCommandDef { name: "recap",       description: "生成当前会话 recap 摘要（手动）",                                         local: SlashLocalKind::AcpRecap },
    SlashCommandDef { name: "summarize",   description: "同 /recap（别名）",                                                       local: SlashLocalKind::AcpRecap },
    SlashCommandDef { name: "remember",    description: "写入长期记忆（/remember <事实>，无参数进入交互模式）",                     local: SlashLocalKind::AcpRemember },

    // ── ACP: Capability discovery / integration ───────────────────────
    SlashCommandDef { name: "mcps",        description: "列出/启动/停止 MCP 服务器",                                              local: SlashLocalKind::AcpMcpServers },
    SlashCommandDef { name: "mcp",         description: "同 /mcps（别名）",                                                         local: SlashLocalKind::AcpMcpServers },
    SlashCommandDef { name: "workflows",   description: "列出/管理 Workflow 定义和运行",                                           local: SlashLocalKind::AcpWorkflows },
    SlashCommandDef { name: "hooks",       description: "插件钩子（lifecycle）命令",                                              local: SlashLocalKind::AcpHooks },
    SlashCommandDef { name: "plugins",     description: "已安装插件列表",                                                          local: SlashLocalKind::AcpPlugins },
    SlashCommandDef { name: "marketplace", description: "插件市场浏览",                                                            local: SlashLocalKind::AcpMarketplace },
    SlashCommandDef { name: "skills",      description: "当前可用 Skills 清单（如 Grok plugins::SkillsCommand）",                  local: SlashLocalKind::AcpSkills },

    // ── ACP: Model & execution mode ───────────────────────────────────
    SlashCommandDef { name: "model",       description: "切换默认模型（/model <display|id>）",                                     local: SlashLocalKind::AcpSetModel },
    SlashCommandDef { name: "m",           description: "同 /model（短别名）",                                                     local: SlashLocalKind::AcpSetModel },
    SlashCommandDef { name: "effort",      description: "设置 agent 思考力度（low|medium|high|max）",                               local: SlashLocalKind::AcpSetEffort },
    SlashCommandDef { name: "always-approve", description: "切换「总是允许中低风险操作」审批模式",                                 local: SlashLocalKind::AcpToggleAlwaysApprove },
    SlashCommandDef { name: "auto",        description: "切换「完全自动」模式（不询问直接执行所有动作）",                           local: SlashLocalKind::AcpToggleAuto },
    SlashCommandDef { name: "yolo",        description: "同 /auto（别名）",                                                        local: SlashLocalKind::AcpToggleAuto },
    SlashCommandDef { name: "multiline",   description: "切换多行输入模式（Enter 换行 vs 发送的定义受此影响）",                     local: SlashLocalKind::AcpToggleMultiline },
    SlashCommandDef { name: "ml",          description: "同 /multiline（短别名）",                                                 local: SlashLocalKind::AcpToggleMultiline },
    SlashCommandDef { name: "compact-mode", description: "切换紧凑 UI 模式（消息密度更高）",                                       local: SlashLocalKind::AcpToggleCompactMode },
    SlashCommandDef { name: "vim-mode",    description: "切换滚动区 Vim 风格键绑定",                                               local: SlashLocalKind::AcpToggleVimMode },
    SlashCommandDef { name: "minimal",     description: "切换到最小化渲染模式（弹出独立窗口）",                                     local: SlashLocalKind::AcpSwitchScreenMode },
    SlashCommandDef { name: "fullscreen",  description: "切换回全屏 TUI 模式",                                                     local: SlashLocalKind::AcpSwitchScreenMode },
    SlashCommandDef { name: "full",        description: "同 /fullscreen（短别名）",                                                local: SlashLocalKind::AcpSwitchScreenMode },

    // ── ACP: Plan / scheduling / tasks ────────────────────────────────
    SlashCommandDef { name: "plan",        description: "计划模式：开关 or /plan <要求> 创建计划",                                  local: SlashLocalKind::AcpPlan },
    SlashCommandDef { name: "view-plan",   description: "仅显示当前执行计划",                                                      local: SlashLocalKind::AcpPlan },
    SlashCommandDef { name: "show-plan",   description: "同 /view-plan（别名）",                                                   local: SlashLocalKind::AcpPlan },
    SlashCommandDef { name: "plan-view",   description: "同 /view-plan（别名）",                                                   local: SlashLocalKind::AcpPlan },
    SlashCommandDef { name: "queue",       description: "显示/管理待处理队列",                                                     local: SlashLocalKind::AcpQueue },
    SlashCommandDef { name: "tasks",       description: "任务清单（task tracker）",                                               local: SlashLocalKind::AcpTasks },
    SlashCommandDef { name: "t",           description: "同 /tasks（短别名）",                                                     local: SlashLocalKind::AcpTasks },
    SlashCommandDef { name: "loop",        description: "循环调度：/loop <schedule cron> <prompt>",                                local: SlashLocalKind::AcpLoop },

    // ── ACP: Session metadata & sharing ───────────────────────────────
    SlashCommandDef { name: "rename",      description: "重命名当前会话（/rename <新名称>）",                                       local: SlashLocalKind::AcpRename },
    SlashCommandDef { name: "title",       description: "同 /rename（别名）",                                                      local: SlashLocalKind::AcpRename },
    SlashCommandDef { name: "session-info", description: "显示当前会话元信息（id / 起止 / 长度等）",                                local: SlashLocalKind::AcpSessionInfo },
    SlashCommandDef { name: "share",       description: "导出并分享会话链接",                                                      local: SlashLocalKind::AcpShare },
    SlashCommandDef { name: "cloud",       description: "同 /share（别名）",                                                       local: SlashLocalKind::AcpShare },

    // ── ACP: UI & diagnostics ─────────────────────────────────────────
    SlashCommandDef { name: "find",        description: "滚动区内全文搜索（/find <pattern>）",                                     local: SlashLocalKind::AcpFindInScrollback },
    SlashCommandDef { name: "export",      description: "导出会话为 Markdown / JSON / Transcript",                                 local: SlashLocalKind::AcpExport },
    SlashCommandDef { name: "transcript",  description: "同 /export transcript（别名）",                                           local: SlashLocalKind::AcpExport },
    SlashCommandDef { name: "copy",        description: "复制最近一条 assistant 回复到剪贴板",                                      local: SlashLocalKind::AcpCopy },
    SlashCommandDef { name: "theme",       description: "切换 UI 主题 / 配色方案",                                                local: SlashLocalKind::AcpTheme },
    SlashCommandDef { name: "feedback",    description: "提交反馈",                                                                local: SlashLocalKind::AcpFeedback },
    SlashCommandDef { name: "announcements", description: "查看系统公告（critical / promo）",                                       local: SlashLocalKind::AcpAnnouncements },
    SlashCommandDef { name: "release-notes", description: "查看 Grodex 发行说明",                                                  local: SlashLocalKind::AcpReleaseNotes },
    SlashCommandDef { name: "changelog",   description: "同 /release-notes（别名）",                                               local: SlashLocalKind::AcpReleaseNotes },
    SlashCommandDef { name: "tutorial",    description: "启动新手引导教程",                                                        local: SlashLocalKind::AcpTutorial },
    SlashCommandDef { name: "tour",        description: "同 /tutorial（别名）",                                                    local: SlashLocalKind::AcpTutorial },
    SlashCommandDef { name: "onboarding",  description: "同 /tutorial（别名）",                                                    local: SlashLocalKind::AcpTutorial },
    SlashCommandDef { name: "docs",        description: "浏览内置文档（命令/协议/概念速查）",                                       local: SlashLocalKind::AcpDocs },
    SlashCommandDef { name: "guides",      description: "同 /docs（别名）",                                                        local: SlashLocalKind::AcpDocs },
    SlashCommandDef { name: "howto",       description: "同 /docs（别名）",                                                        local: SlashLocalKind::AcpDocs },
    SlashCommandDef { name: "doctor",      description: "运行工作区自检 + 常见问题自动修复",                                        local: SlashLocalKind::AcpDoctor },
    SlashCommandDef { name: "terminal-check", description: "同 /doctor（别名）",                                                   local: SlashLocalKind::AcpDoctor },
    SlashCommandDef { name: "terminal-info", description: "同 /doctor（别名）",                                                    local: SlashLocalKind::AcpDoctor },
    SlashCommandDef { name: "terminal-setup", description: "同 /doctor（别名）",                                                   local: SlashLocalKind::AcpDoctor },
    SlashCommandDef { name: "usage",       description: "用量 / 账单中心（/usage [show|manage]）",                                 local: SlashLocalKind::AcpUsage },
    SlashCommandDef { name: "cost",        description: "同 /usage（别名）",                                                       local: SlashLocalKind::AcpUsage },
    SlashCommandDef { name: "settings",    description: "打开设置编辑器（本地持久化偏好）",                                         local: SlashLocalKind::AcpSettings },
    SlashCommandDef { name: "preferences", description: "同 /settings（别名）",                                                    local: SlashLocalKind::AcpSettings },
    SlashCommandDef { name: "prefs",       description: "同 /settings（别名）",                                                    local: SlashLocalKind::AcpSettings },
    SlashCommandDef { name: "config",      description: "同 /config-agents：打开 agent 配置（/settings 别名）",                    local: SlashLocalKind::AcpSettings },
    SlashCommandDef { name: "config-agents", description: "打开 agent 配置",                                                       local: SlashLocalKind::AcpSettings },
    SlashCommandDef { name: "personas",    description: "角色（persona / role / agent）选择与编辑",                                 local: SlashLocalKind::AcpPersonas },
    SlashCommandDef { name: "agents",      description: "同 /personas（别名）",                                                    local: SlashLocalKind::AcpPersonas },
    SlashCommandDef { name: "roles",       description: "同 /personas（别名）",                                                    local: SlashLocalKind::AcpPersonas },
    SlashCommandDef { name: "timestamps",  description: "切换是否显示每条消息的时间戳",                                            local: SlashLocalKind::AcpTimestamps },
    SlashCommandDef { name: "timeline",    description: "显示会话 turn 时间线视图",                                                local: SlashLocalKind::AcpTimeline },
    SlashCommandDef { name: "privacy",     description: "隐私中心（导出/删除我的数据）",                                            local: SlashLocalKind::AcpPrivacy },
    SlashCommandDef { name: "login",       description: "登录 Grok / provider 账户",                                               local: SlashLocalKind::AcpLogin },
    SlashCommandDef { name: "logout",      description: "退出登录",                                                                local: SlashLocalKind::AcpLogout },
    SlashCommandDef { name: "import-claude", description: "从 Claude / ChatGPT 导入历史会话",                                       local: SlashLocalKind::AcpImportHistory },
    SlashCommandDef { name: "toggle-mouse-reporting", description: "切换鼠标事件上报（滚动/点击）",                                 local: SlashLocalKind::AcpToggleMouse },
    SlashCommandDef { name: "edit-prompt", description: "在外部编辑器打开当前草稿（minimal 模式独有）",                             local: SlashLocalKind::AcpEditPrompt },
    SlashCommandDef { name: "expand",      description: "从 minimal 展开到 fullscreen TUI",                                        local: SlashLocalKind::AcpExpand },
    SlashCommandDef { name: "voice",       description: "启用/停用 语音输入（麦克风）模式",                                         local: SlashLocalKind::AcpVoice },
    SlashCommandDef { name: "imagine",     description: "图片生成：/imagine <描述>",                                                local: SlashLocalKind::AcpImagine },
    SlashCommandDef { name: "imagine-video", description: "视频生成：/imagine-video <描述>",                                        local: SlashLocalKind::AcpImagineVideo },
    SlashCommandDef { name: "btw",         description: "BTW 模式：不产生新 turn，直接补充上下文（切开关）",                         local: SlashLocalKind::AcpBtw },

    // ── Grodex extensions (not in upstream Grok) ──────────────────────
    SlashCommandDef { name: "trust",       description: "切换下一会话的工作区信任（/trust on|off）",                                local: SlashLocalKind::GrodexTrust },
    SlashCommandDef { name: "provider",    description: "切换供应商（/provider <name>）",                                          local: SlashLocalKind::GrodexProvider },
    SlashCommandDef { name: "cwd",         description: "打印当前工作目录",                                                        local: SlashLocalKind::GrodexShowCwd },
    SlashCommandDef { name: "tools",       description: "列出当前权限级别可用的 builtin tools",                                    local: SlashLocalKind::GrodexListTools },
    SlashCommandDef { name: "models",      description: "列出当前 provider 所有可用模型名",                                        local: SlashLocalKind::GrodexListModels },
    SlashCommandDef { name: "debug",       description: "显示 transport/事件诊断日志（TUI 内部调试 HUD）",                          local: SlashLocalKind::GrodexDebugLog },
    SlashCommandDef { name: "logs",        description: "同 /debug（别名）",                                                       local: SlashLocalKind::GrodexDebugLog },
    SlashCommandDef { name: "forget",      description: "让 agent 忘记某个主题（/forget <terms>）",                                 local: SlashLocalKind::GrodexForget },
];

/// A single matched row in the slash dropdown.
#[derive(Debug, Clone)]
pub struct SlashMatchRow {
    /// Index into BUILTIN_SLASH_COMMANDS.
    pub cmd_idx: usize,
    /// Match score (higher = better). Used for sort ordering.
    pub score: usize,
    /// Character positions in the command NAME (0-based, relative to name
    /// start) that matched the query — for fuzzy/accent highlight rendering.
    /// Prefix match = positions 0..query.len(); empty = no highlight (bare
    /// `/` shows everything).
    pub match_indices: Vec<u32>,
}

/// Ghost suffix for inline completion preview.
///
/// Example: user typed `/mo` → query = "mo", selected command = "model"
/// → ghost_suffix = Some("del"). Rendered dim/italic right after the user's
/// partial input so they see the "model" completion without typing it.
#[derive(Debug, Clone, Default)]
pub struct InlineGhost {
    /// The remaining characters of the selected command after the query.
    pub suffix: String,
    /// Full command name (for Tab completion consistency).
    pub full_name: String,
}

/// UI snapshot of the slash-menu state. Rendered from the prompt widget.
#[derive(Debug, Clone, Default)]
pub struct SlashMenuState {
    /// True when the user has typed a `/` command token and the menu is
    /// visible. Always false in non-Prompt input modes.
    pub open: bool,
    /// Matches for the current query (prefix-matched command list).
    pub matches: Vec<SlashMatchRow>,
    /// Currently selected index within `matches`.
    pub selected: usize,
    /// Byte range of the current `/command` token in `input_buffer`.
    pub token_range: std::ops::Range<usize>,
    /// Query text (without leading `/`). Non-empty even when menu shows all
    /// commands after just `/`.
    pub query: String,
    /// Inline ghost suffix preview for the currently-selected match.
    /// None when the query perfectly matches the selected name (nothing to
    /// add) or when the menu is closed.
    pub ghost: Option<InlineGhost>,
}

pub struct TuiAppState {
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub capability_generation: u64,
    pub events: Vec<TimestampedEvent>,
    /// Chat-style messages for the main conversation view.
    pub messages: Vec<ChatMessage>,
    pub pending_approvals: Vec<PendingApprovalRow>,
    pub input_mode: InputMode,
    /// Raw prompt text, may contain real '\n' newlines from Alt+Enter.
    /// Unlike the previous design, Enter alone does NOT insert here.
    pub input_buffer: String,
    /// Byte-position cursor inside input_buffer. Ranges from 0..=input_buffer.len().
    pub input_cursor: usize,
    pub command_buffer: String,
    pub command_cursor: usize,
    pub selected_approval_idx: usize,
    pub scroll_conversation: u16,
    /// Grok scrollback "follow_mode"：true = 追加内容时自动滚到底部（默认）。
    /// 用户一旦向上滚动（scroll_up）就置 false，保持用户的滚动位置不被
    /// 新消息拉回底部；只有用户再滚动到底部（scroll_conversation 达上限）
    /// 或点 Shift+G / 发送新消息，才重新进入 follow_mode。
    pub scroll_follow_bottom: bool,
    pub logs: Vec<String>,
    /// Model/provider info for header display.
    pub model_label: String,
    pub provider_label: String,
    /// Trust flag rendered on the prompt info line.
    pub workspace_trusted: bool,
    /// ── 本地会话级 flag（不再只抛 ACP 未接入） ─────────────
    pub always_approve: bool,       // /always-approve
    pub yolo_mode: bool,            // /auto /yolo
    pub compact_ui_mode: bool,      // /compact-mode
    pub vim_mode: bool,             // /vim-mode
    pub btw_mode: bool,             // /btw
    pub loop_mode: bool,            // /loop
    pub show_timestamps: bool,      // /timestamps
    pub session_title: Option<String>,  // /rename /title
    pub tasks: Vec<(String, bool)>, // /tasks /plan /queue — (description, done)
    /// Map tool call_id → index in `messages` (the Tool card).
    /// Used to route ToolCallArgs / ToolCallEnd / ToolResult delta events
    /// to the correct card when several tool calls interleave.
    pub call_id_index: std::collections::HashMap<String, usize>,
    /// Slash-command inline menu. Rebuilt on every keystroke in Prompt mode
    /// so the dropdown stays consistent with input.
    pub slash: SlashMenuState,
    next_seq: u64,
}

impl Default for TuiAppState {
    fn default() -> Self {
        Self::new()
    }
}

impl TuiAppState {
    pub fn new() -> Self {
        Self {
            session_id: None,
            turn_id: None,
            capability_generation: 0,
            events: Vec::new(),
            messages: Vec::new(),
            pending_approvals: Vec::new(),
            // Grok/codex 都没有“先按 i 才能聊天”的设计——默认就是 prompt-ready
            // 状态，避免启动后用户因“输入没反应”而困惑。Esc 才退出到
            // Normal（用于纯滚动/审批浏览）。
            input_mode: InputMode::Prompt,
            input_buffer: String::new(),
            input_cursor: 0,
            command_buffer: String::new(),
            command_cursor: 0,
            selected_approval_idx: 0,
            scroll_conversation: 0,
            scroll_follow_bottom: true,
            logs: Vec::new(),
            model_label: String::new(),
            provider_label: String::new(),
            workspace_trusted: false,
            always_approve: false,
            yolo_mode: false,
            compact_ui_mode: false,
            vim_mode: true, // 我们的 Normal 模式滚动已经是 Vim 键位
            btw_mode: false,
            loop_mode: false,
            show_timestamps: false,
            session_title: None,
            tasks: Vec::new(),
            call_id_index: std::collections::HashMap::new(),
            slash: SlashMenuState::default(),
            next_seq: 1,
        }
    }

    // ── Slash completion ────────────────────────────────────────────────

    /// Max visible rows in the slash dropdown. Renderer + layout both use
    /// this to size the prompt chrome so the dropdown never clips.
    pub const MAX_SLASH_ROWS: usize = 6;

    /// Recompute the slash-menu snapshot from current `input_buffer` + cursor.
    /// Call this after every mutation that changes `input_buffer` / cursor
    /// position. Cheap: a scan over ~20 builtin commands.
    pub fn recompute_slash_menu(&mut self) {
        // 保留旧的 selected：同一组 matches（相同 query）下，用户的选择
        // 不应被重置（之前每帧重置为 0 导致"上下键不移动"的 bug）。
        let prev_selected = self.slash.selected;
        self.slash = self.compute_slash_menu(prev_selected);
        // compute_slash_menu 内部 ghost 推导是对 selected=prev_selected 的，
        // 但 compute_slash_menu 的 ghost 逻辑是 hardcode 选第 0 条的（没传
        // selected）——对齐 refresh_ghost_for_selected 的逻辑在这里重算。
        self.refresh_ghost_for_selected();
    }

    fn compute_slash_menu(&self, prev_selected: usize) -> SlashMenuState {
        if !matches!(self.input_mode, InputMode::Prompt) {
            return SlashMenuState::default();
        }
        let text = &self.input_buffer;
        let cursor = self.input_cursor.min(text.len());

        // Walk backwards from cursor to find a `/` that is either at the
        // buffer start or preceded by whitespace. Like Grok's mid-text slash
        // token scan (slash/mod.rs `scan_inline_slash_tokens`), we activate
        // the menu anywhere the user inserts a `/`.
        let bytes = text.as_bytes();
        let mut slash_pos: Option<usize> = None;
        // Scan back from cursor. Using direct bytes is safe because '/' is
        // single-byte ASCII.
        let mut i = cursor;
        while i > 0 {
            i -= 1;
            if bytes[i] == b'/' {
                // Is this / the start of a token (preceded by ws or BOF)?
                if i == 0 || (bytes[i - 1] as char).is_whitespace() {
                    slash_pos = Some(i);
                }
                break; // stop at the nearest `/` regardless
            }
            let c = bytes[i] as char;
            if c.is_whitespace() {
                break; // no `/` in the current token, nothing to complete
            }
        }
        let Some(slash_start) = slash_pos else {
            return SlashMenuState::default();
        };

        // Find the command-token end (whitespace or cursor = end of command).
        let mut token_end = text.len();
        for (idx, ch) in text[slash_start..].char_indices() {
            let abs = slash_start + idx;
            if abs >= cursor {
                // Token extends to cursor (query ends where the user stopped).
                token_end = cursor;
                break;
            }
            if ch.is_whitespace() {
                // Command ended before cursor; user is now typing args.
                token_end = abs;
                break;
            }
        }
        // query = content between `/` and token_end.
        let q_start = slash_start + 1;
        let query = if q_start <= token_end {
            text[q_start..token_end].to_string()
        } else {
            String::new()
        };

        // Prefix match over the builtin command list. Case-insensitive when
        // the user typed all-lowercase (matches Grok's SmartCase rule in
        // `command_prefix_matches_smart`).
        let case_sensitive = query.chars().any(|c| c.is_uppercase());
        let mut matches: Vec<SlashMatchRow> = Vec::new();
        for (i, cmd) in BUILTIN_SLASH_COMMANDS.iter().enumerate() {
            if query.is_empty() {
                // Bare `/` → show everything (deduped by name for user sanity).
                matches.push(SlashMatchRow {
                    cmd_idx: i,
                    score: 0,
                    match_indices: Vec::new(),
                });
            } else {
                let name = cmd.name;
                let ok = if case_sensitive {
                    name.starts_with(query.as_str())
                } else {
                    // ascii-case-insensitive prefix.
                    name.len() >= query.len()
                        && name.chars().zip(query.chars()).all(|(n, q)| n.eq_ignore_ascii_case(&q))
                };
                if ok {
                    // Score: exact match = 2, prefix match longer name →
                    // higher is prefix score. Simple heuristic works for
                    // ~20 commands.
                    let bonus = if name.len() == query.len() { 2 } else { 1 };
                    // Match indices = first N positions of the name where
                    // N = query length (prefix match). This drives the
                    // fuzzy-style highlight in the dropdown.
                    let match_indices: Vec<u32> = (0..query.len() as u32).collect();
                    matches.push(SlashMatchRow {
                        cmd_idx: i,
                        score: bonus,
                        match_indices,
                    });
                }
            }
        }
        // Sort: exact matches up top, then by score desc, then by name.
        matches.sort_by(|a, b| {
            b.score.cmp(&a.score).then_with(|| {
                BUILTIN_SLASH_COMMANDS[a.cmd_idx]
                    .name
                    .cmp(BUILTIN_SLASH_COMMANDS[b.cmd_idx].name)
            })
        });
        // NOTE: intentionally do NOT truncate the matches list here. The
        // `slash_menu_rows()` helper returns only MAX_SLASH_ROWS for the
        // layout, but the full list is preserved so users can scroll past
        // the first 6 rows via PageDown / Ctrl-J. Previously the
        // `truncate(MAX_SLASH_ROWS * 2)` call cut the list to 12 entries
        // whenever the user typed just `/`, which made most of the
        // built-in commands undiscoverable (same symptom as "input /
        // shows nothing" for anything beyond the first screenful).

        // NOTE: ghost 不在这里计算。因为 compute_slash_menu 的 selected
        // 会继承 prev_selected，但 ghost 是对 *当前选中的那条* 做的后缀
        // 预览——hardcode sel_idx=0 会和 clamped_selected 不一致。
        // recompute_slash_menu() 末尾会统一调 refresh_ghost_for_selected()。
        let ghost: Option<InlineGhost> = None;

        let clamped_selected = if matches.is_empty() {
            0
        } else {
            prev_selected.min(matches.len() - 1)
        };
        SlashMenuState {
            open: !matches.is_empty() || query.is_empty(),
            matches,
            selected: clamped_selected,
            token_range: slash_start..q_start.max(token_end),
            query,
            ghost,
        }
    }

    /// How many rows the slash dropdown currently occupies (0 = hidden).
    pub fn slash_menu_rows(&self) -> usize {
        if !self.slash.open { return 0; }
        self.slash.matches.len().min(Self::MAX_SLASH_ROWS)
    }

    /// Currently-selected slash command index within BUILTIN_SLASH_COMMANDS.
    pub fn selected_slash_index(&self) -> Option<usize> {
        if !self.slash.open || self.slash.matches.is_empty() { return None; }
        let idx = self.slash.selected.min(self.slash.matches.len() - 1);
        Some(self.slash.matches[idx].cmd_idx)
    }

    /// Move the slash selection up (negative delta) or down. Wraps around.
    /// Also re-computes the inline ghost so the preview follows selection.
    pub fn move_slash_selection(&mut self, delta: isize) {
        if !self.slash.open || self.slash.matches.is_empty() { return; }
        let len = self.slash.matches.len() as isize;
        let cur = self.slash.selected as isize;
        let next = (cur + delta).rem_euclid(len) as usize;
        self.slash.selected = next;
        // Re-derive ghost for the newly-selected row.
        self.refresh_ghost_for_selected();
    }

    /// Recompute `self.slash.ghost` for `self.slash.selected`. Used after
    /// arrow nav changes the selection and the ghost should follow.
    fn refresh_ghost_for_selected(&mut self) {
        if self.slash.matches.is_empty() || self.slash.query.is_empty() {
            self.slash.ghost = None;
            return;
        }
        let sel_idx = self.slash.selected.min(self.slash.matches.len() - 1);
        let sel_row = &self.slash.matches[sel_idx];
        let cmd_name = BUILTIN_SLASH_COMMANDS[sel_row.cmd_idx].name;
        let query = &self.slash.query;
        let case_sensitive = query.chars().any(|c| c.is_uppercase());
        let can_ghost = if case_sensitive {
            cmd_name.starts_with(query.as_str()) && cmd_name.len() > query.len()
        } else {
            cmd_name.len() >= query.len()
                && cmd_name.chars().zip(query.chars()).all(|(n, q)| n.eq_ignore_ascii_case(&q))
                && cmd_name.len() > query.len()
        };
        if can_ghost {
            let suffix: String = cmd_name.chars().skip(query.chars().count()).collect();
            self.slash.ghost = Some(InlineGhost {
                suffix,
                full_name: cmd_name.to_string(),
            });
        } else {
            self.slash.ghost = None;
        }
    }

    /// Replace the current `/command…` token with the selected command
    /// (called for Tab completion). Returns true on a non-trivial change.
    pub fn complete_slash_selected(&mut self) -> bool {
        let Some(cmd_idx) = self.selected_slash_index() else { return false; };
        let cmd = &BUILTIN_SLASH_COMMANDS[cmd_idx];
        // New text: `/name ` with a trailing space, matching Grok's
        // SuggestionRow::from_command behavior ("takes_args -> append space").
        let replaced = format!("/{} ", cmd.name);
        let range = self.slash.token_range.clone();
        if range.start > self.input_buffer.len() || range.end > self.input_buffer.len() {
            return false;
        }
        self.input_buffer.replace_range(range.clone(), &replaced);
        // Cursor lands after the inserted replacement.
        self.input_cursor = range.start + replaced.len();
        // Recompute state immediately so the menu reflects any remaining args
        // completion the next time the renderer reads it.
        self.recompute_slash_menu();
        true
    }

    // ── Draft helpers (for /edit-prompt external editor round-trip) ────

    /// Snapshot the current prompt draft as a plain String (for external
    /// editor use, e.g. /edit-prompt). Preserves all characters except
    /// the ones already sanitised on keystroke (\r, \n were stripped on
    /// way in except for the Alt+Enter inserted newlines).
    pub fn draft_text(&self) -> String {
        self.input_buffer.clone()
    }

    /// Replace the prompt draft wholesale after an external editor run.
    /// Sanitises \r away, then positions cursor at the END so the user
    /// sees the new content immediately without scrolling.
    pub fn set_draft_text(&mut self, text: String) {
        let sanitised: String = text.chars().filter(|c| *c != '\r').collect();
        self.input_buffer = sanitised;
        self.input_cursor = self.input_buffer.len();
        // Slash menu / command buffer get recomputed lazily the next
        // time we draw a frame; recompute here to keep invariants tight.
        self.recompute_slash_menu();
    }

    // ── Cursor helpers ──────────────────────────────────────────────────

    /// Clamp input_cursor back into [0, len] after edits.
    pub fn clamp_input_cursor(&mut self) {
        let max = self.input_buffer.len();
        if self.input_cursor > max {
            self.input_cursor = max;
        }
    }

    pub fn clamp_command_cursor(&mut self) {
        let max = self.command_buffer.len();
        if self.command_cursor > max {
            self.command_cursor = max;
        }
    }

    /// Number of hard line breaks + 1 currently present in the prompt.
    /// Uses CJK display-width-aware wrap so layout reserves the correct
    /// number of rows when the user types Chinese/Japanese/Korean — this
    /// used to underestimate the line count (every CJK glyph counts as 2
    /// displayed cells), causing the prompt pane to clip content.
    pub fn prompt_content_lines(&self, wrap_width: usize) -> usize {
        if self.input_buffer.is_empty() {
            return 1;
        }
        use super::render::{display_width, cjk_aware_wrapped_rows};
        let mut rows = 0usize;
        let wrap = wrap_width.max(1);
        for para in self.input_buffer.split('\n') {
            if para.is_empty() {
                rows += 1;
                continue;
            }
            rows += cjk_aware_wrapped_rows(para, wrap);
        }
        let _ = display_width; // keep it reachable even if lto inlines
        rows.max(1)
    }

    // ── Streaming / tool activity ───────────────────────────────────────

    /// True if the last assistant / thinking / unfinished-tool message is
    /// still streaming (delta chunks / tool results could still arrive).
    /// Used to poll the transport at 1 ms instead of 10 ms during bursts.
    pub fn is_streaming(&self) -> bool {
        for m in self.messages.iter().rev() {
            match m {
                ChatMessage::Assistant { done: false, .. } | ChatMessage::Thinking { done: false, .. } => {
                    return true;
                }
                ChatMessage::Tool { done, has_result, .. } => {
                    // Tool counts as still "streaming" only when BOTH:
                    //   - no result yet (has_result=false): ToolResult hasn't landed
                    //   - not explicitly done (done=false):  ToolCallEnd hasn't landed
                    // If EITHER has_result=true OR done=true, the tool card is
                    // effectively finished — the ⏳ indicator should stop for it.
                    // This fixes the classic stuck-streaming bug where a backend
                    // emits ToolCallStart → ToolResult but skips ToolCallEnd.
                    if !*has_result && !*done {
                        return true;
                    }
                }
                ChatMessage::Assistant { done: true, .. } => {
                    // Completed answer — but maybe younger Thinking / Tool
                    // are still active. Don't fall through to User boundary;
                    // keep scanning.
                }
                ChatMessage::User { .. } => {
                    return false;
                }
                _ => {}
            }
        }
        false
    }

    /// Count of ToolCallStart events that have no matching ToolResult yet —
    /// rendered on the turn-status line.
    pub fn active_tool_count(&self) -> usize {
        let mut pending = 0usize;
        let mut closed = 0usize;
        for ev in &self.events {
            match &ev.event {
                SessionEvent::ToolCallStart { .. } => pending += 1,
                SessionEvent::ToolResult { .. } => closed += 1,
                _ => {}
            }
        }
        pending.saturating_sub(closed)
    }

    // ── Event ingestion ─────────────────────────────────────────────────

    pub fn push_event(&mut self, e: SessionEvent) {
        let seq = self.next_seq;
        self.next_seq += 1;

        if let SessionEvent::RequestPermission(payload) = &e {
            self.pending_approvals.push(PendingApprovalRow {
                ticket_id: payload.ticket_id.clone(),
                tool_name: payload.tool_name.clone(),
                summary: payload.summary.clone(),
                risk: payload.risk.clone(),
                remaining_s: (payload.timeout_remaining_ms / 1000) as u32,
            });
        }

        if let SessionEvent::SessionSnapshot { snapshot } = &e {
            self.session_id = Some(snapshot.session_id.to_string());
            self.capability_generation = snapshot.generation;
            self.turn_id = snapshot.current_turn_id.clone();
        }

        if let SessionEvent::TurnComplete { turn_id } = &e {
            self.turn_id = Some(turn_id.clone());
            // Close the two per-turn open blocks independently.
            //
            // BUG-FIX: previous code iterated .rev() and broke on the
            // FIRST Assistant / Thinking hit. That would leave whichever
            // block appeared earlier still marked `done=false`, producing
            // the exact symptom the user reported: "⏳ streaming… stuck
            // forever even after the answer was fully printed".
            //
            // Tool / System rows inserted AFTER TextDelta / ThoughtDelta
            // (e.g. the `read_file ok` badge) mean .last() is not the
            // assistant block — so we still walk in reverse, but track
            // two separate closed flags and never early-abort.
            let mut closed_assistant = false;
            let mut closed_thinking = false;
            for m in self.messages.iter_mut().rev() {
                match m {
                    ChatMessage::Assistant { done, .. } if !closed_assistant => {
                        *done = true;
                        closed_assistant = true;
                    }
                    ChatMessage::Thinking { done, .. } if !closed_thinking => {
                        *done = true;
                        closed_thinking = true;
                    }
                    _ => {}
                }
                if closed_assistant && closed_thinking {
                    break;
                }
            }
            // Also close any still-open Tool cards — the loop reports
            // done=false when ToolCallEnd was never emitted (cancelled
            // turns), so a TurnComplete forces them into a terminal UI
            // state to match the "turn is over" contract.
            for m in self.messages.iter_mut().rev() {
                if let ChatMessage::Tool { done, has_result, .. } = m {
                    if !*done {
                        *done = true;
                        if !*has_result {
                            *has_result = true;
                        }
                    }
                }
            }
        }

        // ── Delta accumulation & tool routing ─────────────────────────────
        //
        // TextDelta   → most recent in-progress Assistant (same turn).
        // ThoughtDelta→ most recent in-progress Thinking  (same turn,
        //               separate block so thinking stays visually distinct
        //               from the final answer, like Grok's 💭 panel).
        // ToolCallStart / Args / End / Result → routed via call_id_index,
        //               which maps call_id → position in messages. If the
        //               agent did not emit a call_id we fall back to the
        //               last Tool card (backward-compat behaviour).
        match &e {
            SessionEvent::TextDelta { text } => {
                let mut found = false;
                for m in self.messages.iter_mut().rev() {
                    match m {
                        ChatMessage::Assistant { text: existing, done, .. } => {
                            if *done {
                                break;
                            }
                            existing.push_str(text);
                            found = true;
                            break;
                        }
                        ChatMessage::User { .. } => {
                            break;
                        }
                        _ => {} // Skip Thinking/Tool/System (same turn)
                    }
                }
                if !found {
                    self.messages.push(ChatMessage::Assistant {
                        text: text.clone(),
                        done: false,
                    });
                }
            }

            SessionEvent::ThoughtDelta { text } => {
                let mut found = false;
                for m in self.messages.iter_mut().rev() {
                    match m {
                        ChatMessage::Thinking { text: existing, done, .. } => {
                            if *done {
                                break;
                            }
                            existing.push_str(text);
                            found = true;
                            break;
                        }
                        ChatMessage::Assistant { done: true, .. } => {
                            // Assistant from a previous completed sub-phase
                            // within the same turn → keep looking (there
                            // might be a younger Thinking ahead of it).
                            continue;
                        }
                        ChatMessage::User { .. } => {
                            break;
                        }
                        _ => {} // Skip Tool/System (same turn)
                    }
                }
                if !found {
                    self.messages.push(ChatMessage::Thinking {
                        text: text.clone(),
                        done: false,
                    });
                }
            }

            SessionEvent::ToolCallStart { call_id, name } => {
                let idx = self.messages.len();
                if !call_id.is_empty() {
                    self.call_id_index.insert(call_id.clone(), idx);
                }
                self.messages.push(ChatMessage::Tool {
                    name: name.clone(),
                    call_id: Some(call_id.clone()).filter(|s| !s.is_empty()),
                    args: String::new(),
                    result: None,
                    is_error: false,
                    done: false,
                    has_result: false,
                });
            }

            SessionEvent::ToolCallArgs { call_id, args_delta } => {
                let found = self
                    .call_id_index
                    .get(call_id)
                    .copied()
                    .and_then(|i| self.messages.get_mut(i));
                let target = match found {
                    Some(m) => Some(m),
                    None => self.messages.iter_mut().rev().find(|m| matches!(m, ChatMessage::Tool { .. })),
                };
                if let Some(ChatMessage::Tool { args, .. }) = target {
                    args.push_str(args_delta);
                }
            }

            SessionEvent::ToolCallEnd { call_id } => {
                let found = self
                    .call_id_index
                    .get(call_id)
                    .copied()
                    .and_then(|i| self.messages.get_mut(i));
                let target = match found {
                    Some(m) => Some(m),
                    None => self.messages.iter_mut().rev().find(|m| matches!(m, ChatMessage::Tool { done: false, .. })),
                };
                if let Some(ChatMessage::Tool { done: d, .. }) = target {
                    *d = true;
                }
            }

            SessionEvent::ToolResult { call_id, content, is_error } => {
                let found = self
                    .call_id_index
                    .get(call_id)
                    .copied()
                    .and_then(|i| self.messages.get_mut(i));
                let target = match found {
                    Some(m) => Some(m),
                    None => self.messages.iter_mut().rev().find(|m| matches!(m, ChatMessage::Tool { done: false, has_result: false, .. })),
                };
                if let Some(ChatMessage::Tool { result: r, is_error: ie, has_result: hr, done: d, .. }) = target {
                    *r = Some(content.clone());
                    *ie = *is_error;
                    *hr = true;
                    // Backend may skip ToolCallEnd when emitting ToolResult
                    // (e.g. exec-style one-shot tools): treat arrival of a
                    // result as implicit end-of-tool so ⏳ streaming clears.
                    *d = true;
                }
            }

            SessionEvent::Error { message } => {
                self.messages.push(ChatMessage::System {
                    text: message.clone(),
                    is_error: true,
                });
            }

            SessionEvent::ItemStarted { item_type, .. } if item_type == "turn" => {
                // A new turn started — next TextDelta/ThoughtDelta will begin
                // a fresh Assistant/Thinking block thanks to the
                // "stop at completed Assistant" guard above.
            }

            _ => {}
        }

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        // Auto-scroll to bottom on content-producing events so the user
        // always sees the latest assistant / tool output as it arrives.
        // Users reading history can press k / Up in Normal mode afterwards;
        // we don't implement a "view lock" flag yet so the behaviour is to
        // follow new content (matches Grok's default streaming UX).
        //
        // NOTE: scroll_conversation is clamped to actual max_offset in the
        // renderer, so u16::MAX is just a sentinel meaning "stick to tail".
        //
        // (Runs BEFORE moving `e` into the events vector so we still own it.)
        // Grok follow_mode：只有在 follow_bottom（用户没有手动往上滚动查看
        // 历史内容）时才自动跳到底部。否则保持原 scroll 位置——用户正在
        // 回看 earlier output，不能被新消息拽回去。
        match &e {
            SessionEvent::TextDelta { .. }
            | SessionEvent::ThoughtDelta { .. }
            | SessionEvent::ToolCallStart { .. }
            | SessionEvent::ToolCallArgs { .. }
            | SessionEvent::ToolCallEnd { .. }
            | SessionEvent::ToolResult { .. }
            | SessionEvent::Error { .. }
            | SessionEvent::TurnComplete { .. } => {
                if self.scroll_follow_bottom {
                    self.scroll_conversation = u16::MAX;
                }
            }
            _ => {}
        }

        self.events.push(TimestampedEvent {
            seq,
            at_ms: now_ms,
            event: e,
            consumed: false,
        });
    }

    pub fn push_user_message(&mut self, text: &str) {
        self.messages.push(ChatMessage::User {
            text: text.to_string(),
        });
        // 用户主动发送消息：显式进入 follow_bottom + 跳到底部。
        // 对齐 Grok：dispatch_send_prompt 后重新钉住底部，让 prompt + 后续
        // streaming 始终可见（即便用户之前在回看历史）。
        self.scroll_follow_bottom = true;
        self.scroll_conversation = u16::MAX;
    }

    pub fn push_event_with_envelope(&mut self, e: SessionEvent, seq: u64, generation: Option<u64>) {
        if let Some(g) = generation {
            self.capability_generation = g;
        }
        if seq >= self.next_seq {
            self.next_seq = seq + 1;
        }
        self.push_event(e);
    }

    pub fn resolve_ticket(&mut self, ticket_id: &str) {
        self.pending_approvals.retain(|r| r.ticket_id != ticket_id);
        if self.selected_approval_idx >= self.pending_approvals.len() && !self.pending_approvals.is_empty() {
            self.selected_approval_idx = self.pending_approvals.len() - 1;
        }
    }

    pub fn scroll_up(&mut self) {
        let prev = self.scroll_conversation;
        self.scroll_conversation = prev.saturating_sub(1);
        // Grok follow_mode 语义：只要用户向上滚了一下（离开底部），
        // follow_bottom 就退出，后续新消息不把用户拉回底部。
        // 只有当 scroll 已经是 0 时（saturating 不产生真位移）保留原状态。
        if self.scroll_conversation < prev {
            self.scroll_follow_bottom = false;
        }
    }

    pub fn scroll_down(&mut self, max_offset: Option<u16>) {
        let prev = self.scroll_conversation;
        let next = prev.saturating_add(1);
        self.scroll_conversation = next;
        // Grok follow_mode：手动滚到底部（>= max_offset，或达到 u16::MAX 上限）
        // 时，重新进入 follow_bottom 模式，后续新消息再次钉到底部。
        if next > prev {
            if let Some(max) = max_offset {
                if next >= max {
                    self.scroll_follow_bottom = true;
                }
            }
        }
    }

    pub fn push_log(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        self.logs.push(msg.clone());
        if self.logs.len() > 50 {
            self.logs.remove(0);
        }
        // 同时推到 messages 使其在 conversation 中可见。
        // render.rs 不渲染 logs，仅推 logs 会导致 slash 命令反馈完全不可见，
        // 用户按 Enter 后看不到任何反应（"回车不生效"的根因）。
        self.messages.push(ChatMessage::System {
            text: msg,
            is_error: false,
        });
        // 只有 follow_bottom 时才跳到底部——用户如果正在向上回看之前的
        // 命令输出，不要每次追加一条 system log 都把用户拉回去。
        if self.scroll_follow_bottom {
            self.scroll_conversation = u16::MAX;
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut result: String = s.chars().take(max).collect();
        result.push('…');
        result
    }
}

impl From<RequestPermissionPayload> for PendingApprovalRow {
    fn from(p: RequestPermissionPayload) -> Self {
        Self {
            ticket_id: p.ticket_id,
            tool_name: p.tool_name,
            summary: p.summary,
            risk: p.risk,
            remaining_s: (p.timeout_remaining_ms / 1000) as u32,
        }
    }
}
