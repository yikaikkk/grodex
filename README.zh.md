<div align="center">

# Grodex

**用 Rust 构建的开源 AI 编程代理 —— 可审计、可恢复、安全沙箱化。**

[![Crates.io](https://img.shields.io/badge/rust-1.85%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](LICENSE)
[![Build](https://img.shields.io/badge/build-passing-green.svg)]()

[English](README.md) · [中文](README.zh.md)

</div>

---

Grodex 是一个能在你的项目中读、写、运行代码的 CLI 工具和 ACP 服务器。它具备崩溃可恢复的 Agent Loop、内核级沙箱强制、以及支持 OpenAI / Anthropic / DeepSeek 等多供应商的协议无关架构。

## 为什么选择 Grodex？

| | Grodex | 典型 AI 编程工具 |
|---|---|---|
| **崩溃恢复** | 仅追加 Rollout Journal —— 从精确崩溃点恢复任意会话 | 崩溃丢失上下文 |
| **沙箱** | macOS Seatbelt 内核级强制，Linux Landlock 就绪 | 可选或无 |
| **可审计** | 17 条运行时不变量，每个动作均记入日志 | 黑盒 |
| **供应商锁定** | 3 种通信协议，多供应商故障切换 | 单一厂商 |
| **Sub-agent** | 带权限上界的委托信封 | 扁平或无 |
| **透明度** | 开源，21 个 crate，550+ 测试 | 闭源 |

## 功能特性

- **Agent Loop** — Session Supervisor → Turn Coordinator → Sampling Step，并行工具调度 + 模型顺序提交
- **崩溃恢复** — 每次状态变更记录到仅追加 JSONL Journal；`resume` 从精确崩溃点重建
- **5 个内置工具** — `read_file`、`write_file`、`edit_file`、`exec`、`apply_patch`，带权限门禁与沙箱强制
- **Sub-agent 委托** — 以冻结权限边界生成子代理，带并发限额与 TUI 实时进度卡片
- **上下文管理** — 窗口大小自动探测、按百分比自动压缩、超大结果外置
- **多供应商** — OpenAI Responses API、Chat Completions（DeepSeek/Qwen 思考模式）、Anthropic Messages API
- **Credential Broker** — 主凭证对代理不可见；一次性租约防重放
- **长期记忆** — SQLite + FTS5 混合检索（BM25 + 向量），注入系统提示词
- **MCP 支持** — 启动 MCP 服务进程，通过 stdio JSON-RPC 通信
- **TUI** — Vim 风格模态界面，含思考面板、Sub-agent 卡片、审批工作流

## 快速开始

### 环境要求

- **Rust 1.85+**（edition 2024）
- 模型供应商的 API Key

### 安装 & 运行

```bash
# 克隆并构建
git clone https://github.com/yikaikkk/grodex.git
cd grodex
cargo build --release

# 启动交互会话
./target/release/grodex run

# 或通过 cargo
cargo run -- run
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
# [rules]
# read_file = "allow"
# write_file = "ask"
# exec = "ask"
```

> 多供应商故障切换、上下文窗口调优等完整配置见 [config.example.toml](config.example.toml)。

### 环境变量

| 变量 | 说明 |
|---|---|
| `OPENAI_API_KEY` | OpenAI API Key |
| `ANTHROPIC_API_KEY` | Anthropic API Key |
| `GRODEX_PROVIDER` | 覆盖供应商（`openai` / `anthropic` / `deepseek`） |
| `GRODEX_MODEL` | 覆盖模型名称 |
| `GRODEX_WIRE_PROTOCOL` | 覆盖通信协议 |
| `GRODEX_API_ENDPOINT` | 覆盖 API 端点 URL |

## 架构

```
┌─────────────────────────────────────────────────────────────────┐
│                        grodex-cli / grodex-tui                   │
│                      (交互 TUI · ACP 客户端)                     │
├─────────────────────────────────────────────────────────────────┤
│                      grodex-protocol (ACP)                       │
│              EventEnvelope · SessionSnapshot · stdio             │
├─────────────────────────────────────────────────────────────────┤
│                        grodex-loop                               │
│   SessionSupervisor → TurnCoordinator → SamplingStep → Reducer  │
│                     RolloutWriter (JSONL Journal)                │
├──────────────┬──────────────┬───────────────┬───────────────────┤
│ grodex-      │ grodex-      │ grodex-       │ grodex-           │
│ sampler      │ provider     │ capability    │ subagent          │
│ (HTTP +      │ (规范 IR)    │ (Prepared     │ (委托信封)        │
│  流式解码)   │              │  Call 围栏)   │                   │
├──────────────┴──────────────┴───────────────┴───────────────────┤
│  grodex-tools · grodex-permission · grodex-sandbox · grodex-auth │
├─────────────────────────────────────────────────────────────────┤
│  grodex-prompt · grodex-memory · grodex-config · grodex-skills  │
│                      grodex-mcp                                 │
└─────────────────────────────────────────────────────────────────┘
```

<details>
<summary><strong>21 个 crate</strong> — 点击展开项目结构</summary>

```
grodex/
├── crates/
│   ├── grodex-core/            # 共享类型：ContextItem, ID, PolicyDecision
│   ├── grodex-loop/            # Agent Loop：Supervisor, TurnCoordinator, Reducer
│   ├── grodex-provider/        # 规范请求/事件模型，通信协议描述符
│   ├── grodex-sampler/         # HTTP 客户端，流式解码器（3 种协议）
│   ├── grodex-capability/      # 能力描述符，PreparedCapabilityCall
│   ├── grodex-permission/      # 策略引擎，审批 Broker，权限租约
│   ├── grodex-sandbox/         # Seatbelt 强制，路径校验
│   ├── grodex-tools/           # 内置工具：read, write, edit, exec, patch
│   ├── grodex-subagent/        # Sub-agent 树，委托信封
│   ├── grodex-auth/            # 凭证 Broker，密钥存储
│   ├── grodex-config/          # TOML 配置，分层合并
│   ├── grodex-protocol/        # ACP 类型，EventEnvelope，stdio 传输
│   ├── grodex-skills/          # Skill 目录，文件系统发现
│   ├── grodex-mcp/             # MCP 客户端，JSON-RPC 进程管理
│   ├── grodex-memory/          # SQLite + FTS5 记忆存储
│   ├── grodex-prompt/          # 提示词构建，指令组装
│   ├── grodex-cli/             # CLI 入口：run, serve, resume, replay
│   └── grodex-tui/             # 终端 UI（ratatui + crossterm）
├── docs/                       # 设计文档（13 份）
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
| `Enter` | 发送提示 |
| `Ctrl`+`O` | 展开/收起思考（CoT）面板 |
| `Ctrl`+`E` | 展开/收起 Sub-agent 执行日志 |
| `↑`/`↓` + `Enter` | 选择并确认审批 |
| `Ctrl`+`C` | 取消生成（连按两次退出） |

## 测试

```bash
# 运行全部测试（550+）
cargo test --workspace

# 运行单个 crate
cargo test -p grodex-loop
```

主要覆盖：崩溃恢复（6 种场景）、generation 围栏、并发调度、Golden wire-event 重放（3 种协议）、macOS Seatbelt 内核拒绝路径、委托权限上界、凭证租约防重放。

## 文档

深度设计文档在 [`docs/`](docs/)：

| 文档 | 说明 |
|---|---|
| [Agent Loop](docs/09-agent-loop-v2-design.md) | Supervisor → TurnCoordinator → SamplingStep |
| [上下文管理](docs/11-context-management-v2-design.md) | Rollout Journal、压缩、投影 |
| [供应商适配](docs/14-provider-model-adapter-v2-design.md) | 规范事件、通信协议解码 |
| [内置工具](docs/15-built-in-tools-v2-design.md) | 工具规格与契约 |
| [权限系统](docs/16-permission-approval-execution-v2-design.md) | 策略引擎、审批、租约 |
| [ACP 协议](docs/17-frontend-acp-protocol-v2-design.md) | 事件信封、流式推送、重同步 |

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
- [Apache 2.0 许可证](LICENSE-APACHE)

任选其一。

---

<div align="center">

**用 Rust 🦀 构建 —— 为安全、透明、可恢复而设计。**

</div>
