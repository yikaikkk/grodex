# 前端协议与 UI 事件流：Grok、Codex 对比及 ACP V2 设计
## 1. 为什么必须独立设计
现有 V2 文档反复出现 `Session Supervisor emit UI event`，但如果事件格式、顺序、重连和交互命令没有公共契约，“TUI、Desktop、编辑器都只依赖协议”就只是愿望。

本文将前端协议视为产品公共 API。结论是：**采用 ACP 作为兼容主干，不另造一套完全私有协议；ACP 无法表达的 Agent tree、durable task、配置代次和高级审批，通过经过能力协商的 namespaced extension 补齐。**

## 2. Grok 当前实现
Grok 已实现 `acp::Agent`，在 `initialize` 中协商 client 类型、模型状态、认证方式以及 xAI 扩展能力，见 [acp_agent.rs](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-shell/src/agent/mvp_agent/acp_agent.rs:31)。Session 通过标准 ACP `SessionUpdate` 发送文本、thought、Tool Call 和 mode 更新。

其事件发送路径已经包含产品协议需要的几个关键能力：

+ 每个通知带 SessionId；
+ 扩展 meta 中包含 eventId、promptId、agentTimestampMs 和 chunkId；
+ 高频文本/思考 chunk 经过缓冲、合并与 debounce；
+ 低频状态事件直接发送；
+ ReplayBuffer 支持重放；
+ canonical Tool Call 持久化，瞬时 delta 可以不单独持久化；
+ permission 通过 ACP reverse request 或 Hub 协议回到 Agent。

发送入口见 [updates.rs](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-shell/src/session/acp_session_impl/updates.rs:55)。其中 mode update 必须进入同一 FIFO 管线，源码明确说明直接发送会产生更高 event id 却更早到达，导致客户端把仍在队列中的文本 chunk 当成旧事件丢弃。这证明 UI 协议不能只定义 payload，还必须定义顺序和重放语义。

不足：

+ 标准 ACP 与 xAI 扩展混用，扩展能力缺少统一版本和稳定性等级；
+ Sub-agent tree、后台 Task、Memory/Capability generation 主要依赖私有通知；
+ 不同客户端可通过 meta 获得不同能力，容易形成隐式产品分叉；
+ 瞬时 delta、canonical state 和持久化事件的边界尚未成为独立协议规范。

## 3. Codex 当前实现
Codex 使用自己的 `Op -> Event` Session 协议。`EventMsg` 覆盖 Session 配置、Turn 生命周期、assistant/reasoning delta、Tool begin/end、审批、MCP、diff、usage 和错误，定义集中在 [protocol.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/protocol/src/protocol.rs:640)。

它的优势是内部状态表达很完整：

+ 每种 Tool 有细粒度 begin/delta/end；
+ `ExecApprovalRequest`、`ApplyPatchApprovalRequest` 等交互类型明确；
+ `TurnDiffEvent` 是一等事件；
+ TurnId、CallId 和 ItemId 能关联 streaming item；
+ Desktop/App Server、TUI 可以复用同一 Rust protocol model。

不足：

+ 协议是 Codex 产品内部 API，第三方编辑器不能直接按 ACP 接入；
+ 事件类型多且演进快，兼容窗口和未知事件处理需要客户端紧跟；
+ legacy event 与新 turn item 并存，投影边界较复杂；
+ Sub-agent 和审批语义虽然丰富，但不具备 ACP 生态兼容性。

## 4. 为什么选 ACP，而不是照搬任一实现
| 选择 | 优点 | 问题 |
| --- | --- | --- |
| 完全自定义协议 | 能原生表达全部 V2 概念 | 编辑器生态、SDK、互操作都要重建 |
| 只用标准 ACP | Zed 等客户端可直接接入 | Agent tree、durable task、恢复和配置诊断表达不足 |
| ACP Core + 版本化扩展 | 保留互操作，同时覆盖 V2 | 必须认真治理扩展兼容性 |


因此采用第三种。设计原则是：**能映射为标准 ACP 的内容必须使用标准类型；只有标准 ACP 无法无损表达的概念才进入 **`x-agent/*`** 扩展。**

## 5. 协议分层
```latex
Transport
  stdio JSON-RPC | local socket | WebSocket/mTLS
       |
ACP Core
  initialize, session/new, session/load, session/prompt,
  session/cancel, session/update, request_permission
       |
x-agent/v2 Extensions
  agent tree, durable task, approval ticket, config diagnostics,
  generation, replay cursor, capability explain
       |
Frontend Projection
  TUI | Desktop | Zed/ACP client | test harness
```

TUI 与 Desktop **共用完全相同的事件流和 command 流**。两者只允许拥有不同的 view state，例如面板是否打开、滚动位置和本地快捷键；不能让 TUI 绕过协议直接读取 Session 内存。

## 6. 初始化与能力协商
客户端发送：

```latex
InitializeRequest.meta.x-agent = {
  protocol_versions: ["2.0", "2.1"],
  extensions: {
    replay_cursor: 1,
    agent_tree: 1,
    approval_v2: 1,
    structured_diff: 1,
    config_diagnostics: 1
  },
  client: { kind, name, version },
  rendering: { markdown, images, ansi, max_chunk_bytes }
}
```

Agent 返回所选协议版本、启用的扩展、未知事件策略和限额。规则如下：

1. Major 不兼容时拒绝连接并给出支持范围；
2. Minor 只允许新增可忽略字段或可协商事件；
3. 未协商的扩展事件不得发送；
4. 客户端必须忽略同一 major 下未知的可选字段；
5. 安全交互事件不能标记为可忽略。

## 7. 统一事件信封
所有标准 ACP update 和扩展事件都放入统一传输信封：

```latex
EventEnvelope {
  protocol_version
  session_id
  event_id
  seq
  turn_id?
  step_id?
  generation?
  agent_id?
  task_id?
  causation_id?
  correlation_id?
  timestamp
  durability: transient | durable
  payload_type
  payload
}
```

+ `seq` 是单 Session rollout writer 分配的顺序；
+ `event_id` 用于幂等去重；
+ `causation_id` 指向导致该事件的 command/Tool Call；
+ transient delta 可以合并或丢弃，durable 事件必须可重放；
+ generation 用于拒绝迟到的旧 Turn/旧能力事件。

扩展字段放在 ACP `meta` 或 namespaced notification 中，但进入 Runtime 后必须统一还原为该信封，不能让 transport 细节污染 Reducer。

## 8. 事件分类
### 8.1 Canonical durable events
必须进入 rollout，可重建 Session：

+ Session/Turn/Step started、completed、aborted；
+ 完整 assistant message；
+ canonical Tool Call、Tool Result；
+ ApprovalRequested/Resolved；
+ Task/Agent tree 状态；
+ DiffCommitted；
+ ConfigGenerationAdopted；
+ Compaction committed；
+ error terminal state。

### 8.2 Transient streaming events
用于体验，不作为恢复事实源：

+ assistant/reasoning text delta；
+ Tool stdout/stderr delta；
+ Tool argument preview；
+ progress spinner、token estimate；
+ typing/status hint。

瞬时事件必须指向最终 canonical item id。客户端丢失 delta 后，可以用最终 item 覆盖修正。

### 8.3 Snapshot/projection events
重连时发送当前投影，而不是伪造整个历史流：

```latex
SessionSnapshot {
  projection_version
  through_seq
  active_turn
  messages/item summaries
  pending_approvals
  running_tools
  agent_tree
  task_notifications
  config/capability generations
}
```

Snapshot 后再发送 `seq > through_seq` 的增量事件。

## 9. Streaming 渲染协议
每个 streaming item 使用稳定 `item_id`，状态为：

```latex
ItemStarted
  -> ItemDelta*       // transient, 可合并
  -> ItemCompleted    // durable canonical body
  | ItemAborted       // durable，partial body 仅供展示
```

约束：

+ delta 只能 append 到指定 channel/part，不能隐式重写旧文本；
+ 需要重写时使用 `ItemReplacement`，带 replacement revision；
+ UTF-8 边界、Markdown fence 和 Tool JSON 增量由 Runtime assembler 处理，UI 不猜测半截 JSON；
+ reasoning 与 final answer 使用不同 channel；
+ aborted partial response 不进入下一次模型输入，但可在 UI 灰显；
+ backpressure 时优先合并 delta，不能丢 canonical completion。

## 10. Tool 与 Diff 展示
Tool UI 使用通用生命周期，不为每个 Tool 发明互不兼容的事件：

```latex
ToolCallPrepared
ToolCallApprovalPending?
ToolExecutionStarted
ToolOutputDelta*
ToolExecutionFinished
ToolResultCommitted
```

payload 中允许 typed presentation hint，但事实字段始终是 CapabilityId、ToolCallId、args preview/hash、status 和 result/blob ref。

Diff 使用结构化模型：

```latex
DiffArtifact {
  diff_id
  base_snapshot_hash
  files[] { path, old_hash, new_hash, status, hunks[] }
  unified_diff_blob_ref
  generated_by_tool_call_ids[]
}
```

TUI 可以渲染 unified diff，Desktop 可以渲染 side-by-side，但二者看到同一个 artifact。超大 diff 外置 blob，事件只带摘要与引用。

## 11. 审批交互契约
审批复用 [权限、审批与执行 V2](./16-permission-approval-execution-v2-design.md) 的 Ticket/Resolution：

```latex
ApprovalRequested {
  approval_id, tool_call_id, agent_id, task_id,
  capability, args_preview, requested_scope,
  reason, deadline, available_decisions
}

ResolveApproval {
  approval_id, resolution_nonce,
  decision: allow | deny | narrow | cancel,
  effective_scope?, effective_args?, user_message?
}
```

UI 必须展示来源 Agent/Task；不能只显示命令文本。Narrow 只能提交 Tool 声明支持的结构化变换。审批卡片关闭不是 Resolution，网络断开也不是 allow。

标准 ACP client 不支持 V2 Narrow/Agent tree 时，降级为 ACP 可表达的 allow once/reject，并隐藏不可安全表达的选项。

## 12. Sub-agent 与 Task 扩展
ACP Core 保持主 Session 对话兼容，扩展事件表达：

+ `x-agent/agent_node_upserted`；
+ `x-agent/task_run_updated`；
+ `x-agent/mailbox_activity`；
+ `x-agent/task_result_available`；
+ `x-agent/agent_focus_changed`。

完成通知只带有界 preview 和 task result handle。主 Agent 是否读取完整结果仍走显式 `task_get`，UI 不把完整 child transcript偷偷注入父上下文。

## 13. Command 流
客户端到 Runtime 的 command 同样版本化：

+ prompt/steer/queue input；
+ cancel turn/tool/task；
+ resolve approval；
+ select model/mode；
+ load/replay session；
+ focus agent/task；
+ config edit request；
+ auth flow response。

每个 command 带 `command_id`、expected generation 和 idempotency key。修改状态的命令由 Session Supervisor 串行提交；重复 command 返回原结果，不执行两次。

## 14. 重连、ACK 与背压
客户端持久保存最后处理的 durable `seq`，重连发送 cursor：

```latex
ResumeSession { session_id, after_seq, projection_version }
```

Runtime 可以：

1. 从 rollout 增量重放；
2. cursor 已过 TTL 时返回 snapshot + tail；
3. projection version 不兼容时强制 full snapshot。

ACK 只用于回收 transport replay buffer，不代表业务事件已提交。慢客户端的策略是合并 transient delta、保留 durable event；超过硬上限时断开并允许 cursor 重连，不能无限占用 Session 内存。

## 15. 安全边界
+ UI 不能发送任意“内部事件”，只能发送 schema 中的 command；
+ local socket 使用 peer credential 和 Session capability token；
+ WebSocket 使用 mTLS/OAuth，禁止把 bearer 放进 URL；
+ args、stdout、diff 和错误在发 UI 前执行敏感信息分级与脱敏；
+ Desktop 的 AppOnly 操作也必须产生审计事件；
+ 未受信客户端不能请求显示隐藏 Capability 或原始 credential；
+ replay 数据遵守 rollout TTL 和删除级联。

## 16. 相对原实现的收益
### 相对 Grok
| 当前问题 | 新设计 | 收益 |
| --- | --- | --- |
| ACP 与私有扩展边界分散 | ACP Core + 统一 namespaced extensions | 保留编辑器兼容，扩展可治理 |
| meta 字段隐式演进 | initialize 能力协商 + version | 客户端不会静默误解新事件 |
| replay buffer 与 durable state 概念混杂 | transient/durable/snapshot 三分 | 恢复与渲染职责清楚 |
| 不同 client type 容易分叉 | 所有前端同一事件流 | TUI/Desktop 行为一致 |


### 相对 Codex
| 当前问题 | 新设计 | 收益 |
| --- | --- | --- |
| 私有 EventMsg 生态绑定 | 映射到 ACP Core | 可直接接入 ACP 编辑器 |
| 新旧事件类型较多 | canonical lifecycle + typed hints | UI 实现面更稳定 |
| 重连依赖产品内部协议 | cursor + snapshot 标准扩展 | 远端和多客户端更可靠 |
| Sub-agent 只在内部模型表达 | versioned agent-tree extension | Desktop/TUI 可一致展示 |


## 17. 关键决策收益闭环
| 决策 | 解决的问题 | 直接收益 | 验证指标 |
| --- | --- | --- | --- |
| ACP 优先 | 自定义协议失去编辑器生态 | Zed/第三方客户端可接入 | ACP conformance 通过率 |
| 扩展能力协商 | ACP 表达力不足 | 安全增加 Agent tree/审批 | 未协商事件发送数应为零 |
| 单一事件流 | TUI/Desktop 逻辑漂移 | 前端可替换、行为一致 | 跨前端 golden test 一致率 |
| durable/transient 分离 | delta 丢失破坏恢复 | 可降载且不丢事实 | 重连 projection hash 一致率 |
| seq + cursor + snapshot | 断线后重复或缺事件 | 可幂等恢复 | 重连重复/缺失事件数 |
| 结构化 Diff/Approval | UI 猜字符串 | 交互安全且可测试 | 错误审批目标数、diff 一致率 |


## 18. 实施顺序与验收
Phase 1：定义 envelope、ACP 映射、event catalog 和 golden fixtures，让 TUI 先完全改走协议。

Phase 2：Desktop 使用相同 stream；加入 cursor、snapshot、backpressure 和 canonical item replacement。

Phase 3：加入 approval_v2、agent_tree、task、config diagnostics 扩展；提供标准 ACP 降级路径。

验收必须包括：

1. 标准 ACP client 能完成普通对话、Tool 展示和 allow/reject；
2. TUI/Desktop 对相同 fixture 生成相同业务投影；
3. 任意 delta 丢失后 canonical completion 能修正 UI；
4. 每个事件边界断线重连均不重复 commit；
5. mode update 与文本 chunk 不会因乱序导致文本丢失；
6. child 审批始终展示 task 来源；
7. 慢客户端不会拖垮 Session writer；
8. 未协商扩展不会发送，未知 optional 字段不会导致崩溃。
