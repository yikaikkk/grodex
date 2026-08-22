# Grodex

**Grodex** 是一个 AI 编程代理 —— 一个能在你的项目中读、写、运行代码的命令行工具和 ACP 服务器。用 Rust 编写,围绕一组架构不变量设计,使其可审计、可恢复、可安全沙箱化。

- **Agent Loop** 三层架构：Session Supervisor → Turn Coordinator → Sampling Step
- **Rollout Journal**（仅追加事件日志）作为崩溃恢复的唯一事实源
- **内置工具**（read / write / edit / exec / apply_patch），带权限与沙箱强制
- **Sub-agent** 以 agentic 循环运行，带委托信封、权限上界、并发限额与 TUI 实时进度卡片
- **上下文管理**：上下文窗口自动探测、按百分比自动压缩、超大工具结果外置
- **Credential Broker** 从不泄露主凭证
- **ACP 协议** 含统一事件信封，支持流式推送与断线重同步
- **macOS Seatbelt** 内核级沙箱强制执行（Linux Landlock + bubblewrap 桩已备好）

---

## 快速开始

### 环境要求

- **Rust 1.85+**（edition 2024）
- 模型提供商的 API Key（`OPENAI_API_KEY`、`ANTHROPIC_API_KEY` 等）

### 构建 & 运行

```bash
# 从源码构建
cd grodex
cargo build --release

# 启动交互会话
cargo run -- run

# 从 rollout journal 恢复之前的会话
cargo run -- resume <session-id>

# 回放会话（打印完整对话历史）
cargo run -- replay <session-id>

# 以 ACP 服务器模式运行（用于 IDE 集成）
cargo run -- serve

# 显示版本号
cargo run -- version
```

### 配置文件

创建 `~/.grodex/config.toml` 或 `<project>/.grodex/config.toml`：

```toml
provider = "openai"
model = "gpt-5"
wire_protocol = "responses"   # "responses" | "chat" | "messages"
# endpoint = "https://api.openai.com/v1"
# api_key = "sk-..."          # 推荐使用环境变量 OPENAI_API_KEY

# ── 上下文窗口 ───────────────
# 不填时按模型名自动探测（内置表见 config.example.toml）
# context_window = 1048576
# compaction_threshold_percent = 85

# ── Agent Loop 限额 ──────────
# max_tool_result_bytes = 32768      # 超限结果外置到临时文件
# max_steps_per_turn = 40            # 耗尽后强制生成进展总结
# max_subagents = 4                  # sub-agent 并发上限
# max_subagents_per_session = 16     # sub-agent 会话总数上限

# ── 沙箱 ─────────────────────
sandbox_profile = "workspace"  # "workspace" | "readonly" | "restricted" | "full"

# ── 权限规则 ─────────────────
# [rules]
# read_file = "allow"
# write_file = "ask"
# exec = "ask"

# ── 长期记忆 ─────────────────
# [memory]
# enabled = true
# SQLite + FTS5 数据库路径（开头的 ~ 会展开为 home 目录）
# 优先级：GRODEX_MEMORY_DB 环境变量 > 此配置 > ~/.grodex/memory.db
# path = "~/.grodex/memory.db"
```

多供应商故障切换路由等详细配置见 `config.example.toml`。

### 环境变量

| 变量 | 说明 |
|---|---|
| `OPENAI_API_KEY` | OpenAI 提供商 API Key |
| `ANTHROPIC_API_KEY` | Anthropic 提供商 API Key |
| `GRODEX_PROVIDER` | 覆盖提供商名称 |
| `GRODEX_MODEL` | 覆盖模型名称 |
| `GRODEX_WIRE_PROTOCOL` | 覆盖通信协议（`responses`/`chat`/`messages`） |
| `GRODEX_API_ENDPOINT` | 覆盖 API 端点 URL |
| `GRODEX_MEMORY_DB` | 覆盖记忆 SQLite 数据库路径（`~` 会展开） |

---

## 架构

Grodex 是一个包含 **21 个 crate** 的 Rust 工作区，分为四层：

### 核心循环（`grodex-loop`、`grodex-core`、`grodex-provider`、`grodex-sampler`）

- **SessionSupervisor** — `tokio::select!` 事件循环，多路复用命令、轮次完成和超时。当一轮耗尽步数预算时发出可见警告。
- **TurnCoordinator** — 一个 Turn（一个用户目标）执行多个采样步骤（`max_steps_per_turn` 可配置，默认 40），并行调度工具，按模型发出顺序提交结果。步数耗尽时强制一次无工具采样生成进展总结，而非静默停止。
- **上下文管理** — 上下文窗口大小从内置模型表自动探测（可用 `context_window` 覆盖）；用量达到 `compaction_threshold_percent`（默认 85%）时自动触发压缩。超过 `max_tool_result_bytes`（默认 32KB）的工具结果外置到临时文件，上下文中只保留预览+文件引用，避免上下文膨胀。外置文件按会话隔离在 `$TMPDIR/grodex-tool-results/{session_id}/` 下，启动时按 7 天 TTL 清扫。
- **Canonical Model Request/Event** — 供应商无关的中间表示。Responses / Chat Completions / Messages API 的流式解码器产出统一规范事件。
- **Rollout Journal** — 每个状态变更记录到 `~/.grodex/sessions/{id}/rollout.jsonl`（仅追加 JSONL + 内容寻址 blob 存储）。`SessionReducer` 重放事件以在崩溃恢复时重建对话上下文。

### 能力与权限（`grodex-capability`、`grodex-permission`、`grodex-sandbox`、`grodex-tools`）

- **5 个内置工具**：`read_file`、`write_file`、`edit_file`、`exec`、`apply_patch`。每个工具声明其并发等级、副作用等级和默认策略。未注册元数据的工具（如 MCP 工具）默认 **Serial** 执行——批内任一工具为 Serial 时整批串行。
- **权限管线**：静态策略评估 → 审批票据 → 用户决议 → 权限租约 → 执行。所有规则使用 **最严格合并** 语义（Deny > Ask > Allow，priority 仅打破同级 tie）。
- **沙箱强制执行**：macOS 上通过 `sandbox-exec` 施加内核级 Seatbelt 限制（不是只生成字符串，而是真正 syscall 强制）。Linux Landlock 和 bubblewrap 桩已备好。Profile 生成 fail-closed（路径含引号/反斜杠/控制字符时拒绝生成整个 profile），路径校验对词法归一与 canonical 形态交叉匹配，堵住 `../` 与符号链接逃逸。
- **PreparedCapabilityCall**：调度时为每个工具调用绑定不可变快照（能力版本、策略生成、校验后的参数、参数 SHA-256）。

### Sub-agent 与委托（`grodex-subagent`）

- **SubAgentSupervisor** 监控子代理，强制超时，级联取消。
- **DurableSubAgentSupervisor** 通过共享 `RolloutWriter` 日志化每次 spawn/complete/fail/cancel，使重启会话可恢复。
- **DelegationEnvelope** 冻结父代理交给子代理的安全边界：能力子集、策略上界、沙箱配置、资源预算、权限等级。`authorize_tool_call` 强制不变量 #12（子代理权限 ≤ 父代理）。
- **Agentic 委托**：`delegate_task` 工具把每个 sub-agent 作为独立的多步循环运行（最多 15 个采样步，可调用 `read_file` 等只读工具）。并发（`max_subagents`，默认 4）与会话总数（`max_subagents_per_session`，默认 16）双重限额，超限委托以可操作提示拒绝而非报错。超过 8KB 的 sub-agent 报告外置到 `$TMPDIR/grodex-subagent-results/`，上下文中只留预览，保证父代理汇总时不会撑爆上下文窗口。启动时清扫 git worktree（`git worktree prune` + 残留目录删除），进程被杀也不会留下孤儿 worktree。
- **进度流式推送**：sub-agent 发出结构化进度事件（started / step / finished），经 ACP 以 `SubagentProgress` 更新推送，在 TUI 中渲染为可折叠卡片（见快捷键）。

### 协议与 UI（`grodex-protocol`、`grodex-cli`、`grodex-tui`）

- **ACP（Agent Client Protocol）** stdio 上 JSON-RPC。支持 `initialize`、`session/new`、`session/load`、`session/prompt`、`session/cancel`、`ResolveApproval`、`ResumeSession`。服务端每 15 秒发一次保活 `Ping`，长工具执行期间不会被前端误判为断连。
- **EventEnvelope** 将每个流式更新包裹进 seq、event_id、parent_event_id、causation_token 和 generation —— 支持 gap 检测、UI 拼接和 exactly-once 重放。
- **SessionSnapshot** 用于断线后快速重同步。
- **语义提交围栏**：流一旦已输出可见文本或开始工具调用，中途失败绝不静默重试或切换模型——由 Turn 级恢复处理半截内容。

### 认证与凭证（`grodex-auth`、`grodex-auth-types`）

- **CredentialBroker** 是主凭证的唯一持有者。代理永远看不到原始 token —— 它们通过 `broker.resolve()` 兑换一次性 `CredentialLease`。租约绑定端点、受 epoch 约束、首次使用即消费（防重放）。
- 可选 macOS Keychain 后端，跨重启持久化凭证。Linux/Windows 暂无原生密钥后端——自动降级为内存存储（安全，但重启后丢失）。

### Skill、MCP、记忆、配置、提示词（`grodex-skills`、`grodex-mcp`、`grodex-memory`、`grodex-config`、`grodex-prompt`）

- **Skills** 目录扫描发现 + YAML/Toml 清单。
- **MCP 客户端** 启动 MCP 服务进程，通过 stdio JSON-RPC 通信（`tools/list`、`call_tool`）。
- **Memory** SQLite + FTS5 数据库，三路检索管线（Skill / 长期记忆 / Evidence，BM25 打分），格式化注入系统提示词。
- **Config** 分层解析：system → enterprise → user → profile → workspace，带 merge trace 审计。
- **Prompt 组装**：指令优先级四区 A→C→B→D，`PromptBuilder` + `InstructionDiscovery` 自动加载 AGENTS.md / CLAUDE.md。

---

## 项目结构

```
grodex/
├── crates/
│   ├── grodex-core/            # 共享类型：ContextItem, ID, PolicyDecision, 错误
│   ├── grodex-loop/            # Agent Loop：Supervisor, TurnCoordinator, Reducer, RolloutWriter
│   ├── grodex-provider/        # 规范请求/事件模型，通信协议描述符
│   ├── grodex-sampler/         # HTTP 客户端，流式解码器（Responses/Chat/Messages）
│   ├── grodex-capability/      # 能力描述符，步骤快照，PreparedCapabilityCall
│   ├── grodex-permission/      # 策略引擎，审批 Broker，决议，权限租约
│   ├── grodex-sandbox/         # Profile 存储，路径校验，Seatbelt 强制，运行时
│   ├── grodex-sandbox-types/   # SandboxProfile, SandboxBinding 共享类型
│   ├── grodex-tools/           # 内置工具：read, write, edit, exec, patch
│   ├── grodex-subagent/        # Sub-agent 树，任务生命周期，委托信封
│   ├── grodex-auth/            # 凭证 Broker，认证管理器，密钥存储
│   ├── grodex-auth-types/      # 账户描述符，凭证句柄，租约类型
│   ├── grodex-rollout/         # RolloutEvent 类型，FileRolloutStore（JSONL + blobs）
│   ├── grodex-config/          # TOML 配置加载，分层合并，约束校验
│   ├── grodex-protocol/        # ACP 类型，EventEnvelope，SessionSnapshot，stdio 传输
│   ├── grodex-skills/          # Skill 目录，文件系统发现
│   ├── grodex-mcp/             # MCP 客户端，JSON-RPC 进程管理
│   ├── grodex-memory/          # 记忆存储，关键词/标签检索
│   ├── grodex-prompt/          # 提示词构建，指令组装
│   ├── grodex-cli/             # CLI 入口：run, serve, resume, replay
│   └── grodex-tui/             # 终端 UI（ratatui + crossterm）
├── docs/                       # V2 设计文档（13 份）
├── task/                       # 实现缺口审计与跟踪
├── config.example.toml
├── Cargo.toml                  # 工作区清单
├── README.md                   # 英文 README
└── README.zh.md                # 中文 README（本文件）
```

---

## 设计原则

Agent 围绕一组**不可变不变量**构建，在运行时强制并通过测试验证：

| # | 不变量 |
|---|---|
| 1 | Session 控制状态转移通过 Supervisor 串行 |
| 2 | 同一时间最多接纳一个 Turn |
| 3 | 模型只能调用当前 StepSnapshot 公开的工具 |
| 4 | 工具调用在调度时绑定到能力版本 |
| 5 | 权限放行前不产生副作用 |
| 6 | 工具结果按模型发出顺序提交 |
| 7 | 工具结果持久化后才可以下一步采样 |
| 8 | 取消操作等待清理完成后才接纳新 Turn |
| 9 | Compaction 不留悬空工具调用 |
| 10 | Memory 上下文快照在同一 Turn 内稳定 |
| 11 | 后台任务完成 ≠ 主 Agent 已读取 |
| 12 | 子代理权限 ≤ 父代理上界 |
| 13 | rollout.jsonl 是唯一事实源 |
| 14 | 迟到事件通过 generation 比对拒绝 |
| 15 | Tool / Skill / MCP 在同一 Turn 内稳定 |
| 16 | 撤销只能收紧（epoch 单调增） |
| 17 | AppOnly 动作记录进 rollout |

---

## 测试

```bash
# 运行全部测试（约 ~712 个，0 失败）
cargo test --workspace

# 运行单个 crate
cargo test -p grodex-loop

# 带回溯信息运行
RUST_BACKTRACE=1 cargo test --workspace
```

主要测试类别：
- **恢复测试**：6 个崩溃位置恢复测试（采样前 / 流式半途 / 工具结果写入前 / 写入后提交前 / Compaction 替换前 / TurnCompleted 持久化前）。
- **围栏测试**：generation 回退检测，store 失败时 commit fence 传播。
- **调度测试**：随机化并发工具完成，确定性模型顺序提交。
- **Golden 测试**：三种通信协议的 wire-event fixture 重放。
- **Seatbelt 测试**：macOS 内核级拒绝路径验证。
- **委托测试**：authority ceiling、policy ceiling、工具子集、撤销检查。
- **租约测试**：单次使用兑现、防重放、错误 audience 拒绝、epoch 撤销。

---

## 交互模式内置命令

| 命令 | 说明 |
|---|---|
| `/quit`、`/exit` | 退出会话 |
| `/help` | 显示帮助 |
| `/compact` | 触发上下文压缩 |
| `/rewind N` | 回退 N 轮对话（本地） |
| `/edit-prompt` | 用 `$EDITOR` 打开当前提示词草稿 |

### TUI 快捷键

| 按键 | 模式 | 功能 |
|---|---|---|
| `i` | Normal | 进入输入模式 |
| `:` | Normal | 进入命令模式 |
| `Enter` | Prompt | 发送提示 |
| `Alt`+`Enter` / `Shift`+`Enter` | Prompt | 插入换行 |
| `Esc` | Prompt | 返回 Normal 模式 / 取消生成 |
| `Ctrl`+`C` | Prompt | 取消当前生成（连按两次退出） |
| `↑`/`↓` 或 `k`/`j` | Normal | 滚动对话 / 导航审批选项 |
| `Ctrl`+`J` / `Ctrl`+`K` | Prompt | 对话向下/向上滚动 |
| `PageUp` / `PageDown` | 两者 | 翻页滚动 |
| `Ctrl`+`O` | 两者 | 展开/收起 CoT（思考过程）面板 |
| `Ctrl`+`E` | 两者 | 展开/收起 Subagent 执行日志 |
| `Ctrl`+`N` / `Ctrl`+`P` | 两者 | 折叠状态下滚动 CoT 内容 |
| `↑`/`↓` + `Enter` | Normal | 选择并确认审批选项 |

当有待审批项时，`↑`/`↓` 键导航审批选项，`Enter` 确认选择 —— 无需切换中英文输入法按 `a`/`d`/`c` 字母键。

触控板滚动手势被识别为突发（burst），只用于滚动对话 —— 不会驱动输入历史导航或审批选项选择，这些只响应真实的 `↑`/`↓` 按键。输入模式下 `↑`/`↓` 用于导航输入历史。

### TUI 中的 Subagent 卡片

每个被委托的 sub-agent 渲染为一张 tool 风格卡片：头部行（`⏺ Subagent '<label>' ▶ Running / ✓ Done / ✗ Failed · 耗时`）、暗色任务预览、以及执行日志（采样步骤与工具调用）。日志默认折叠为最近 3 行；`Ctrl`+`E` 可对最新一轮展开（最多 40 行）—— 与思考面板的 `Ctrl`+`O` 是独立开关。

---

## 许可证

MIT OR Apache-2.0

---

## 相关文档

- `docs/09-agent-loop-v2-design.md` — Agent Loop 架构
- `docs/11-context-management-v2-design.md` — Rollout Journal 与上下文投影
- `docs/14-provider-model-adapter-v2-design.md` — 供应商适配与规范事件
- `docs/15-built-in-tools-v2-design.md` — 内置工具规格
- `docs/16-permission-approval-execution-v2-design.md` — 权限与审批系统
- `docs/17-frontend-acp-protocol-v2-design.md` — ACP 协议规格
- `task/grodex-implementation-gap-audit.md` — 实现审计与缺口跟踪
