<div align="center">

# Grodex

**用 Rust 构建的开源 AI 编程代理 —— 可审计、可恢复、安全沙箱化、全程可观测。**

[![Crates.io](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Build](https://img.shields.io/badge/build-passing-green.svg)]()

[English](README.md) · [中文](README.zh.md)

</div>

---

Grodex 是一个能在你的项目中读、写、运行代码的 CLI 工具和 ACP 服务器。它具备崩溃可恢复的 Agent Loop、内核级沙箱强制、SQLite 全程可观测、以及支持 OpenAI / Anthropic / DeepSeek 等多供应商的协议无关架构。

## 为什么选择 Grodex？

| | Grodex | 典型 AI 编程工具 |
|---|---|---|
| **崩溃恢复** | 仅追加 Rollout Journal —— 从精确崩溃点恢复任意会话 | 崩溃丢失上下文 |
| **可观测** | SQLite 遥测投影：哪个 Turn 慢、哪个工具卡、审批等了多久、缓存命中率，一条命令可查 | 黑盒或纯文本日志 |
| **沙箱** | macOS Seatbelt 内核级强制 + exec 资源限制（内存/CPU/进程数）+ 进程组整组击杀 | 可选或无 |
| **可审计** | 17 条运行时不变量，每个动作记入 journal；环境凭证自动剥离 | 黑盒 |
| **供应商锁定** | 3 种通信协议，多候选故障切换（凭证+端点随候选切换） | 单一厂商 |
| **Sub-agent** | 统一 agent 树：委托、消息、等待、中断，权限上界 | 扁平或无 |
| **透明度** | 开源，22 个 crate，919 个测试 | 闭源 |

## 功能特性

- **Agent Loop** — Session Supervisor → Turn Coordinator → Sampling Step；并行工具调度 + 模型顺序提交；repair sampling 兜底（纯问答不触发）
- **崩溃恢复** — 每次状态变更记录到仅追加 JSONL Journal（gap-free seq）；`resume` 从精确崩溃点重建，含未完成工具的悬空修复
- **可观测系统** — 每个生命周期事件落 SQLite（WAL）：`model_attempts`（TTFT/缓存命中率/重试）、`tool_executions`（审批等待 vs 执行耗时）、`security_decisions`、`subagent_runs`；崩溃后自动重投影补齐
- **12 个内置工具** — `read_file`、`write_file`、`edit_file`、`exec`、`apply_patch`、`web_fetch`、`grep`、`glob`、`load_skill`、`read_artifact`、`process_io`、`delegate_task`，全部带真实描述、权限门禁与沙箱强制
- **审批工作流** — 允许 / 总是允许（本会话持久 grant）/ 拒绝 / **Narrow**（TUI 内编辑参数 JSON 限定作用域）；过期清扫器自动回收超时票据
- **Sub-agent 委托** — 冻结权限边界 + 资源限额，统一 agent 树支持 `send_message` / `wait_agent` / `interrupt_agent` / `list_agents`
- **上下文管理** — 窗口自动探测、自动压缩（提交前协议校验）、超大结果外置 blob、CJK 感知的提示词总预算
- **exec 资源限制** — 内存（RLIMIT_AS）/ CPU / 文件大小 / 进程数 rlimits + setsid 进程组隔离，超时/取消整组击杀；凭证类环境变量自动剥离
- **多供应商** — OpenAI Responses、Chat Completions（DeepSeek/Qwen 思考模式）、Anthropic Messages；故障切换时凭证与端点随候选切换
- **Credential Broker** — 主凭证对代理不可见；一次性租约防重放；MCP OAuth 授权码流 CLI
- **长期记忆** — SQLite + FTS5 混合检索（BM25 + 向量 RRF），周期重建索引，注入提示词带注入防御框架
- **MCP 支持** — stdio JSON-RPC，60s 超时 + 按 id 关联 + 乱序缓冲，OAuth 授权流
- **TUI** — Vim 风格模态界面：思考面板、Sub-agent 卡片、审批卡（含参数编辑）、流式中输入自动 Steer

## 快速开始

### 环境要求

- **Rust 1.85+**（edition 2024）
- 模型供应商的 API Key

### 安装 & 运行

```bash
git clone https://github.com/yikaikkk/grodex.git
cd grodex
cargo build --release

# 交互会话（REPL）
./target/release/grodex run --trusted

# 终端 UI（自动拉起 `grodex serve` 子进程）
./target/release/grodex tui

# ACP 服务器（供 Zed 等前端接入）
./target/release/grodex serve

# 恢复崩溃会话
./target/release/grodex resume <session-id>
```

### 配置文件

创建 `~/.grodex/config.toml`：

```toml
provider = "openai"
model = "gpt-5"
wire_protocol = "responses"   # "responses" | "chat" | "messages"

# 沙箱配置
sandbox_profile = "workspace"  # "workspace" | "readonly" | "restricted" | "full"

# 权限规则
[rules]
read_file = "allow"
web_fetch = "allow"
write_file = "ask"
exec = "ask"
apply_patch = "ask"

# exec 资源限制（可选）
# exec_memory_limit_mb = 8192
# exec_cpu_limit_secs = 600

# 多供应商故障切换
# [model_routes.default]
# ...完整示例见 config.example.toml
```

> 完整配置（多供应商故障切换、MCP 服务器、记忆、压缩阈值等）见 [config.example.toml](config.example.toml)。

### 环境变量

| 变量 | 说明 |
|---|---|
| `OPENAI_API_KEY` | OpenAI API Key（亦支持 `ANTHROPIC_API_KEY` 等） |
| `GRODEX_PROVIDER` / `GRODEX_MODEL` / `GRODEX_WIRE_PROTOCOL` / `GRODEX_API_ENDPOINT` | 覆盖供应商 / 模型 / 协议 / 端点 |
| `GRODEX_TELEMETRY_DB` | 遥测数据库路径（默认 `~/.grodex/telemetry.db`） |
| `GRODEX_TELEMETRY_RETENTION_DAYS` | 遥测保留天数（默认 30，0 关闭） |
| `GRODEX_MEMORY_DB` | 记忆库路径（默认 `~/.grodex/memory.db`） |
| `GRODEX_MEMORY_RESCAN_SECS` | 记忆重建索引周期（默认 600 秒，0 关闭） |
| `GRODEX_PROMPT_BUDGET_TOKENS` | 系统提示词 token 预算（默认 40000，CJK 感知估算） |
| `GRODEX_LOG_DIR` | 日志目录（默认 `~/.grodex/logs`） |

## CLI 命令

| 命令 | 说明 |
|---|---|
| `grodex run` | 交互 REPL 会话 |
| `grodex tui` | 终端 UI |
| `grodex serve` | ACP 服务器（stdio） |
| `grodex resume <id>` | 恢复会话（含未完成工具修复、子代理树重建） |
| `grodex replay / inspect / dump <id>` | 会话重放 / 事件检视 / 原始导出 |
| `grodex eval <id>` | 记忆检索质量评估 |
| `grodex prompt explain / dump` | 检视系统提示词组装（默认脱敏） |
| `grodex telemetry sessions / turn <id> / slow-tools / slow-models / cache / errors / recovery / doctor / timeline / vacuum / export` | 遥测查询与运维 |
| `grodex mcp-auth <server>` | MCP OAuth 授权（凭证持久化） |

## 架构

```
┌─────────────────────────────────────────────────────────────────┐
│                        grodex-cli / grodex-tui                   │
│              (交互 TUI · ACP 客户端 · telemetry CLI)              │
├─────────────────────────────────────────────────────────────────┤
│                        grodex-protocol (ACP)                     │
│              EventEnvelope · SessionSnapshot · stdio             │
├─────────────────────────────────────────────────────────────────┤
│                        grodex-loop                               │
│   SessionSupervisor → TurnCoordinator → SamplingStep → Reducer  │
│        RolloutWriter (JSONL Journal) ──► TelemetrySink           │
├──────────────┬──────────────┬───────────────┬───────────────────┤
│ grodex-      │ grodex-      │ grodex-       │ grodex-           │
│ sampler      │ provider     │ capability    │ subagent          │
│ (HTTP +      │ (规范 IR +   │ (Prepared     │ (委托信封 +        │
│  流式解码)   │  故障切换)   │  Call 围栏)   │  协作协议)        │
├──────────────┴──────────────┴───────────────┴───────────────────┤
│  grodex-tools · grodex-permission · grodex-sandbox · grodex-auth │
├─────────────────────────────────────────────────────────────────┤
│  grodex-prompt · grodex-memory · grodex-config · grodex-skills  │
│      grodex-mcp · grodex-rollout · grodex-telemetry              │
└─────────────────────────────────────────────────────────────────┘
```

<details>
<summary><strong>22 个 crate</strong> — 点击展开项目结构</summary>

```
grodex/
├── crates/
│   ├── grodex-core/            # 共享类型：ContextItem, ID, PolicyDecision
│   ├── grodex-loop/            # Agent Loop：Supervisor, TurnCoordinator, Reducer
│   ├── grodex-provider/        # 规范请求/事件模型，通信协议描述符
│   ├── grodex-sampler/         # HTTP 客户端，流式解码器（3 种协议），故障切换
│   ├── grodex-capability/      # 能力描述符，PreparedCapabilityCall
│   ├── grodex-permission/      # 策略引擎，审批 Broker，权限租约，会话 grant
│   ├── grodex-sandbox/         # Seatbelt 强制，路径/网络校验，资源限制
│   ├── grodex-tools/           # 内置工具：read/write/edit/exec/patch/web_fetch/...
│   ├── grodex-subagent/        # Sub-agent 树，委托信封，邮箱，协作协议
│   ├── grodex-auth/            # 凭证 Broker，密钥存储，MCP OAuth
│   ├── grodex-config/          # TOML 配置，分层合并，热更新管线
│   ├── grodex-protocol/        # ACP 类型，EventEnvelope，stdio 传输
│   ├── grodex-skills/          # Skill 目录，渐进式披露，trust 标记
│   ├── grodex-mcp/             # MCP 客户端，JSON-RPC 进程管理，OAuth 协调
│   ├── grodex-memory/          # SQLite + FTS5 记忆存储与检索
│   ├── grodex-prompt/          # 提示词四区装配，指令发现，预算裁剪
│   ├── grodex-rollout/         # JSONL journal 单写者 actor，崩溃恢复
│   ├── grodex-telemetry/       # SQLite 遥测投影，查询，保留期
│   ├── grodex-cli/             # CLI 入口：run/serve/resume/telemetry/mcp-auth/...
│   └── grodex-tui/             # 终端 UI（ratatui + crossterm）
├── docs/                       # 设计文档（14 份）
└── config.example.toml
```

</details>

## 设计不变量

Grodex 强制 **17 条运行时不变量** —— 不只是文档，而是在测试中验证：

| # | 不变量 |
|---|---|
| 1 | Session 状态转移通过 Supervisor 串行 |
| 2 | 同一时间最多接纳一个 Turn |
| 3 | 模型只能调用当前 StepSnapshot 公开的工具 |
| 4 | 工具调用在调度时绑定到能力版本 |
| 5 | 权限放行前不产生副作用 |
| 6 | 工具结果按模型发出顺序提交 |
| 7 | 结果持久化后才可以下一步采样 |
| 8 | 取消等待清理完成后才接纳新 Turn |
| 9 | Compaction 不留悬空工具调用 |
| 10 | Memory 快照在同一 Turn 内稳定 |
| 11 | 后台任务完成 ≠ 主 Agent 已读取 |
| 12 | 子代理权限 ≤ 父代理上界 |
| 13 | `rollout.jsonl` 是唯一事实源 |
| 14 | 迟到事件通过 generation 比对拒绝 |
| 15 | Tool / Skill / MCP 在同一 Turn 内稳定 |
| 16 | 撤销只能收紧（epoch 单调增） |
| 17 | AppOnly 动作记录进 rollout |

## TUI

Vim 风格模态界面，含思考面板、Sub-agent 卡片和集成审批工作流：

| 按键 | 功能 |
|---|---|
| `i` / `:` | 进入输入 / 命令模式 |
| `Enter` | 发送提示（流式中自动转为 Steer） |
| `Ctrl`+`O` | 展开/收起思考（CoT）面板 |
| `Ctrl`+`E` | 展开/收起 Sub-agent 执行日志 |
| `↑`/`↓` + `Enter` | 审批选项：Allow / **Always allow** / Deny / Cancel / **Narrow（参数编辑器）** |
| `Ctrl`+`C` | 取消生成（连按两次退出） |

## 测试

```bash
# 运行全部测试（919 个）
cargo test --workspace

# 运行单个 crate
cargo test -p grodex-loop
```

主要覆盖：崩溃恢复（6 种场景）、generation 围栏、并发调度、Golden wire-event 重放（3 种协议）、macOS Seatbelt 内核拒绝路径、委托权限上界、凭证租约防重放、遥测投影与崩溃重投影幂等、审批 narrow/过期/会话 grant。

## 文档

深度设计文档在 [`docs/`](docs/)：

| 文档 | 说明 |
|---|---|
| [Agent Loop](docs/09-agent-loop-v2-design.md) | Supervisor → TurnCoordinator → SamplingStep |
| [上下文管理](docs/11-context-management-v2-design.md) | Rollout Journal、压缩、投影 |
| [供应商适配](docs/14-provider-model-adapter-v2-design.md) | 规范事件、通信协议解码、故障切换 |
| [内置工具](docs/15-built-in-tools-v2-design.md) | 工具规格与契约 |
| [权限系统](docs/16-permission-approval-execution-v2-design.md) | 策略引擎、审批、租约、会话 grant |
| [ACP 协议](docs/17-frontend-acp-protocol-v2-design.md) | 事件信封、流式推送、重同步 |
| [可观测系统](docs/21-telemetry-observability-design.md) | 双层记录、SQLite 投影、查询与运维 |

## 参与贡献

欢迎贡献！请：

1. Fork 本仓库
2. 创建功能分支（`git checkout -b feature/amazing-feature`）
3. 确保测试通过（`cargo test --workspace`）
4. 提交更改（`git commit -m 'feat: add amazing feature'`）
5. 发起 Pull Request

## 许可证

双重许可：

- [MIT 许可证](LICENSE-MIT)
- [Apache 2.0 许可证](LICENSE-Apache)

任选其一。

---

<div align="center">

**用 Rust 🦀 构建 —— 为安全、透明、可恢复而设计。**

</div>
