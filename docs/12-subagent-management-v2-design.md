# Sub-agent 管理机制对比与 V2 设计
## 1. 文档定位
本文对比 Grok Build 与 Codex 当前源码中的 Sub-agent 创建、调度、通信、权限、结果回流、资源限制和恢复机制，并在此基础上提出一套新的 Sub-agent Runtime 设计。

源码范围：

+ Grok Build：`/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build`；
+ Codex：`/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs`。

本文提出的 V2 是后续演进方案，不代表任一项目已经完整实现。它是 [Agent Loop V2](./09-agent-loop-v2-design.md) 第 15 节的管理面深化，并复用 [Tool、Skill 与 MCP V2](./10-tool-skill-mcp-v2-design.md) 的 `CapabilityId`、`StepCapabilitySnapshot`、`DelegationEnvelope` 和权限上界。

本文重点回答：

+ 子 Agent 在 Runtime 中究竟是一次任务，还是一个长期存在的 Agent；
+ 父 Agent 怎样创建、等待、查询、继续和取消 child；
+ child 默认继承哪些上下文、权限、工具和工作区；
+ 后台完成为什么不等于父 Agent 已经读取结果；
+ 多个 child 怎样限制并发、释放内存并在重启后恢复；
+ 怎样同时获得 Grok 的任务治理能力与 Codex 的多 Agent 协作能力。

## 2. 先看结论
两者都采用同一个基础模型：

```latex
Parent Agent
  -> 调用内置委派 Tool
  -> Runtime 创建独立 child session / thread
  -> Child 运行自己的 Agent Loop
  -> 结果或事件通过 Runtime 回到 Parent
```

真正的区别是管理抽象：

```latex
Grok Build
  Sub-agent 更像“有句柄、有终态、有结果库的异步任务”

Codex V2
  Sub-agent 更像“有地址、有 mailbox、可反复唤醒的独立 Agent Thread”
```

Grok 的优势集中在：

+ Coordinator actor 对任务生命周期进行单写者管理；
+ 前台等待、超时转后台、结果查询和完成提醒语义清楚；
+ `parent_prompt_id` 支持按父 Turn 精确取消；
+ child 支持独立 Git worktree、snapshot 和 rehydrate；
+ 大结果可以落盘，内存只保留引用。

Codex V2 的优势集中在：

+ Agent 有稳定树形地址和独立持久化 Thread；
+ `send_message`、`followup_task`、`wait_agent` 等形成完整协作协议；
+ 支持 `none/all/N` 三种上下文 fork；
+ 执行并发限制、驻留上限和 LRU 卸载分层治理；
+ child 卸载或进程重启后可以从 rollout 按需恢复。

新设计不把二者硬塞进一个模糊的 `Subagent` 对象，而是明确拆成：

```latex
AgentNode = 可寻址、可通信、可恢复的 Agent 身份
TaskRun   = AgentNode 上一次有输入、预算、结果和终态的执行
```

这是融合方案最重要的建模决定：Codex 的长期协作能力属于 `AgentNode`，Grok 的任务状态机和结果库属于 `TaskRun`。

## 3. 共同概念
### 3.1 Session、AgentNode 和 TaskRun
三个概念不能混用：

| 概念 | 生命周期 | 主要职责 |
| --- | --- | --- |
| Session | 一个 Agent 的持久会话 | 保存 rollout、上下文投影和运行配置 |
| AgentNode | 父子树中的稳定身份 | 寻址、mailbox、权限上界和恢复 |
| TaskRun | 一次具体委派执行 | 输入、预算、状态、结果、错误和取消 |


一个 `AgentNode` 可以先执行一次 Task，完成后接收 follow-up，再生成新的 `TaskRun`。这样不会为了继续对话复制一个新 Agent，也不会把多轮结果挤进同一条永远不终止的 Task 记录。

### 3.2 前台和后台
前后台只描述父侧等待方式，不描述 child 是否独立：

+ 前台：父 Tool Call 等待本次 `TaskRun` 终态，并把摘要作为 Tool Result 回填；
+ 后台：父 Tool Call 立即得到 handle，child 继续运行，完成后只投递通知；
+ detach：前台等待超过预算后转为后台，但 Task 身份和执行不改变；
+ await-to-completion：显式要求不自动 detach。

### 3.3 通知和结果
必须区分：

```latex
Completion Notification
  = 某任务已经完成、结果在哪里、摘要是什么

Task Result
  = 完整输出、结构化产物、错误、证据引用和执行元数据
```

通知进入父 mailbox 不代表完整结果已经进入父 transcript。这个边界是控制上下文膨胀和正确恢复的基础。

## 4. Grok Build 当前实现
### 4.1 创建链路
Grok 的主链路是：

```latex
Task Tool
  -> SubagentRequest
  -> ChannelBackend
  -> SubagentCoordinator
  -> ChildRunner
  -> child session
  -> 独立 Agent Loop
```

`TaskTool` 先读取父 Session 注入的资源，检查深度、父 Session ID、父 Prompt ID 和模型参数，再向 backend 提交请求。入口见 [task/mod.rs](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build/task/mod.rs:120)。

`SubagentRequest` 至少包含：

```latex
id
prompt
description
subagent_type
parent_session_id
parent_prompt_id
resume_from
cwd
runtime_overrides
run_in_background
surface_completion
await_to_completion
fork_context
owner
cancel_token
```

定义见 [task/types.rs](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build/task/types.rs:59)。

其中：

+ `parent_prompt_id` 把 child 绑定到创建它的父 Turn；
+ `resume_from` 从已完成 child 恢复 transcript、Tool 状态和 model；
+ `fork_context` 只供内部 harness 使用，普通模型 Task 默认不会复制父历史；
+ `runtime_overrides` 可以覆盖 model、reasoning effort、persona、capability mode、worktree isolation、输出预算和 schema 等。

### 4.2 Coordinator actor
`SubagentCoordinator` 是一个 single-writer actor。它拥有所有生命周期集合和回复 channel：

```latex
pending
active
completed
waiters
workflow_cancel_waiters
pending_completions
runs
progress
```

外部只能发送命令，不能直接修改这些集合。actor 通过 `tokio::select!` 同时接收：

+ spawn/query/cancel/list 命令；
+ ChildRunner 启动确认；
+ child 完成 future；
+ progress 查询；
+ foreground deadline。

状态修改仍然在一个 actor 内串行完成，因此“取消刚好撞上 child 启动”“完成撞上 waiter 注册”等竞态可以在一个地方决定。Coordinator 接收 spawn 并加入运行集合的位置见 [coordinator.rs](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator.rs:165)。

### 4.3 前后台生命周期
Grok 默认 foreground budget 为 45 秒：

```latex
foreground child
  -> 45 秒内完成：直接返回结果
  -> 超过 45 秒：返回 background handle，child 继续运行
```

特殊分支：

+ `run_in_background=true`：立即返回 handle；
+ `await_to_completion=true`：不设置 foreground deadline；
+ 父等待 future 消失：普通 Task 自动转后台；
+ Workflow child 的父等待 future 消失：取消 child。

这个设计避免一个耗时 child 无限占住父 Tool Call，同时不会因为 UI 或父 Turn 暂停而误杀普通后台任务。

### 4.4 结果库和显式读取
完成后 Coordinator 保存 `CompletedChild`：

```latex
child_session_id
child_cwd
worktree_path
snapshot_ref
persisted_output_ref
effective_model_id
result
```

父 Agent 调用 `get_task_output(task_id, block)` 时：

+ completed：立即返回结果；
+ running 且 `block=false`：返回当前状态和进度；
+ running 且 `block=true`：登记 oneshot waiter，状态变化后由 Coordinator 回复。

因此等待是事件驱动的，不是固定间隔轮询。内存中只保留最近 1024 个 completed entry，pending completion reminder 最多 256 个；大结果可通过 `persisted_output_ref` 从磁盘恢复。完成写入和 waiter 派发见 [coordinator.rs](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build/task/coordinator.rs:570)。

### 4.5 上下文、Skill、MCP 和工作区
普通 Task 默认 fresh context。child 获得自己的 system prompt、Agent definition、toolset 和 Agent Loop，但不自动拿到父对话全文。

`resume_from` 与普通新建不同：

+ 恢复源 child 的 transcript；
+ 恢复 Tool 状态和 model；
+ system prompt 按当前环境重新渲染；
+ worktree 丢失时可根据 snapshot rehydrate；
+ 无法恢复隔离目录时才回落到共享 workspace。

Grok 还能按 Agent definition 控制 Skill 和 MCP 继承。插件 Agent 不允许自行声明任意 MCP server；普通 Agent 可以解析父 MCP 配置或按 inheritance 过滤父 MCP pool。

### 4.6 权限和深度
Grok 当前最大深度固定为 1：

```latex
Main Agent
  -> Child Agent
       -> 不允许再创建孙 Agent
```

Task Tool 在入口检查深度，child 构建 toolset 时还会移除 Task Tool，形成双重限制。

capability mode 会与 Agent definition 求交，再按 `ToolKind` 过滤 child toolset。但当前源码明确说明：没有 `ToolKind` 的 MCP/custom Tool 会被保留，见 [task/types.rs](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build/task/types.rs:206)。因此 capability mode 不能单独作为严格权限边界。

child 使用父侧传入的 permission handle，权限事件可以回到宿主。插件 Agent 的非默认 permission mode 会被忽略；受管策略禁止 yolo 时，`bypassPermissions` 会降级为 Default。不过普通 Agent definition 在未被受管策略禁止时仍可能申请 bypass，这还不是形式化的“child 永不宽于 parent”证明。

### 4.7 取消
Grok 同时使用：

+ `CancellationToken`：协作式通知；
+ `ChildControl.cancel()`：向已经启动的 child runtime 发取消；
+ Coordinator 状态迁移：决定 pending/active/completed 的最终状态；
+ `parent_prompt_id`：只取消当前父 Turn 创建的 children，不影响更早已经后台化的任务。

这种设计比“关闭整个父 Session 时杀死全部 child”更精确。

### 4.8 Grok 的优点
+ Actor 单写者让任务生命周期容易推理和测试；
+ 前台、后台、超时 detach 和显式等待边界完整；
+ 结果库、waiter 和磁盘引用适合耗时任务与大输出；
+ Turn 级取消范围细；
+ worktree 和 snapshot 对编码任务非常实用；
+ 普通 Task 默认 fresh，节省上下文并降低父历史污染。

### 4.9 Grok 的不足
+ child 主要仍是一次性 Task，缺少通用双向 mailbox 和长期 Agent 地址；
+ `resume_from` 更像从旧任务复制恢复源，不如对同一 AgentNode 继续发消息自然；
+ 最大深度固定为 1，无法表达受控的树形协作；
+ 当前没有看到独立的 active-child semaphore，批量 spawn 可能同时占用模型、进程和网络资源；
+ capability filter 对无 `ToolKind` 的 MCP/custom Tool 存在旁路；
+ permission mode 尚未被严格建模为 parent ceiling 的交集；
+ completed 内存上限解决了增长问题，但不是完整的驻留与按需恢复策略。

## 5. Codex V2 当前实现
### 5.1 创建链路
Codex V2 的主链路是：

```latex
spawn_agent Tool
  -> 构造 child Config
  -> AgentControl
  -> ThreadManagerState
  -> AgentRegistry / V2Residency / ExecutionLimiter
  -> 独立 child Thread / Session / rollout
  -> child Agent Loop
```

`AgentControl` 在一棵 root Agent tree 中共享，内部持有：

```latex
session_id
AgentRegistry
V2Residency
AgentExecutionLimiter
RolloutBudget
Weak<ThreadManagerState>
```

每个 child 则拥有独立 Thread ID、Session、rollout、上下文和 mailbox。

### 5.2 稳定 Agent 树和协作工具
Codex V2 为 child 分配稳定的 `AgentPath`，例如：

```latex
/root
/root/reviewer
/root/reviewer/security
```

当前协作工具包括：

| Tool | 语义 |
| --- | --- |
| `spawn_agent` | 创建 child 并触发第一个 Turn |
| `send_message` | 只写入目标 mailbox，不启动新 Turn |
| `followup_task` | 写入消息并启动或恢复目标 Agent |
| `wait_agent` | 等待 mailbox activity 或用户 steer |
| `list_agents` | 按 Agent path 查询树和状态 |
| `interrupt_agent` | 中断目标 Agent 当前 Turn |


`send_message` 与 `followup_task` 共享投递路径，只通过 `trigger_turn` 区分是否唤醒，见 [message_tool.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/core/src/tools/handlers/multi_agents_v2/message_tool.rs:51)。

这意味着 child 完成一次 Turn 后仍然是同一个 Agent，可以继续接收任务，不需要创建新的身份。

### 5.3 上下文 fork
Codex V2 的 `fork_turns` 支持：

+ `none`：fresh child；
+ `all`：继承完整可用历史，也是当前默认值；
+ 正整数 N：只继承最近 N 个 Turn。

参数解析见 [spawn.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs:182)。

fork 不是复制所有 runtime 事件。Codex 会过滤 Tool Call/Output、Reasoning、AdditionalTools 和 Agent 间通信，主要保留 system/developer/user、assistant final answer、compaction checkpoint 和必要 world state。过滤规则见 [agent/control/spawn.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/core/src/agent/control/spawn.rs:47)。

在真正 fork 前，它会 materialize 并 flush 父 rollout，保证 child 看到的是持久化一致点，而不是父 Session 内存中尚未写完的半个历史。

### 5.4 权限和运行环境继承
Codex 从当前父 Turn 的有效配置构造 child config，复制：

+ model/provider/reasoning；
+ developer instructions；
+ approval policy；
+ permission profile，也就是 sandbox；
+ cwd；
+ environment selection；
+ exec policy。

角色配置应用后会再次写回父 Turn 的 approval policy、permission profile 和 cwd，避免 role 把运行时安全配置覆盖掉。相关逻辑见 [multi_agents_common.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/core/src/tools/handlers/multi_agents_common.rs:170)。

这比直接 clone 一份启动配置可靠，但它目前表达的是“child 从 parent 当前有效策略开始”，还没有像本文 V2 那样持久化一个可验证的 `DelegationEnvelope`，也没有把所有后续能力刷新都形式化为与 parent ceiling 求交。

### 5.5 完成通知和继续任务
child Turn 完成或失败后，Session 找到 direct parent，将终态包装为 `InterAgentCommunication`，以 `trigger_turn=false` 写入父 mailbox，见 [session/mod.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/core/src/session/mod.rs:1861)。

正常完成消息携带 child final answer；完整 child 历史仍只存在于 child rollout，不会合并进父 rollout。错误文本有显式截断；正常 final answer 在当前这条封装路径没有统一的 1000-token 截断。

后续 `followup_task` 会：

1. 根据 Agent path 找到 Thread ID；
2. 如果 child 已卸载，则从 rollout 恢复；
3. 投递 mailbox message；
4. 启动新的 child Turn。

### 5.6 执行限制和驻留治理
Codex V2 将资源限制拆成不同层：

```latex
AgentRegistry
  -> 管身份、父子关系和 spawn reservation

AgentExecutionLimiter
  -> 限制当前真正执行 Turn 的 child 数

V2Residency
  -> 限制内存里加载的 child Thread 数

RolloutBudget
  -> 限制整个 Agent tree 的共享 rollout/token 消耗
```

`AgentExecutionLimiter` 只统计 V2 Sub-agent 正在启动的 Turn，见 [execution.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/core/src/agent/control/execution.rs:60)。

驻留达到上限时，`V2Residency` 按 LRU 寻找：

+ 已 Completed/Errored/Interrupted；
+ 没有 active Turn；
+ mailbox 没有 pending input。

满足条件的 child 会先确保 rollout 已落盘，再 shutdown 并从 ThreadManager 移除。需要通信时再按需恢复，见 [residency.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/core/src/agent/control/residency.rs:80)。

### 5.7 深度和恢复
Legacy multi-agent 会检查 `agent_max_depth`。当前 Codex V2 handler 会计算并记录 child depth，但测试明确要求 V2 忽略 legacy 的 configured max depth。V2 主要依靠并发、驻留和总资源限制，而不是硬深度限制。

Agent graph metadata 可以持久化。进程恢复时不自动重开所有 descendant runtime；需要通信或 follow-up 时，`ensure_v2_agent_loaded()` 才从 rollout 恢复对应 child。

### 5.8 Codex V2 的优点
+ Agent path、Thread 和 mailbox 形成真正的多 Agent 协作模型；
+ 同一个 child 可以持续接收 follow-up，不需要反复复制会话；
+ 上下文 fork 模式灵活，并在 fork 前建立持久化一致点；
+ 执行并发、内存驻留和共享预算分层治理；
+ rollout 驱动的 lazy reload 适合长时间和大规模 Agent 树；
+ 普通消息与触发 Turn 的消息分离，控制面语义清楚。

### 5.9 Codex V2 的不足
+ 默认 `fork_turns=all` 可能复制过多父上下文，增加 token、缓存和 prompt injection 传播面；
+ V2 忽略 legacy max depth，若只有并发限制，没有总节点/深度硬上界，可能形成过深 Agent 树；
+ completion 会自动携带 normal final answer，过长结果可能直接膨胀父 mailbox/context；
+ 当前 spawn 路径没有看到 Grok 那种自动 child worktree 和 snapshot rehydrate；
+ 状态更偏 Agent Thread，缺少 Grok 那种显式、统一、可按 task id 查询的大结果库；
+ `interrupt_agent` 面向 Thread 当前 Turn，缺少 Grok `parent_prompt_id` 那种父 Turn 所有权取消范围；
+ 权限虽然从父 Turn 恢复，但尚未形成持久化、可审计、每 Step 重求交的严格 child ceiling。

## 6. 两者逐项对比
| 维度 | Grok Build | Codex V2 |
| --- | --- | --- |
| 核心抽象 | 有终态的 Task | 可持续通信的 Agent Thread |
| 管理中心 | Coordinator actor | AgentControl + Registry + ThreadManager |
| 身份 | task/subagent ID | Thread ID + Agent path |
| 单次执行状态 | pending/active/completed | Agent status + active Turn |
| 默认上下文 | fresh | full-history fork |
| 上下文选择 | fresh、内部 fork、resume | none/all/last N |
| 前台等待 | Tool 内 await，45 秒可 detach | spawn 后通过 mailbox/wait 协作 |
| 后台完成 | notification + result store | final answer 投递 parent mailbox |
| 完整结果 | `get_task_output`，可落盘 | child rollout，父侧主要收 completion message |
| 后续任务 | `resume_from` 新建恢复任务 | `followup_task` 复用同一 child |
| 双向消息 | 较弱，偏结果回流 | 完整 mailbox 协议 |
| 最大深度 | 固定 1 | V2 不用 legacy depth limit |
| 执行并发 | 未见独立 semaphore | `AgentExecutionLimiter` |
| 内存治理 | completed/reminder 数量上限 | LRU unload + lazy reload |
| 总预算 | 以 child 自身配置为主 | Agent tree 共享 RolloutBudget |
| 文件隔离 | 可选 worktree + snapshot | 默认继承 cwd，未见自动 worktree |
| 取消 | token + control + parent prompt scope | Thread interrupt |
| 权限 | capability filter +共享审批 | 复制父 Turn policy/sandbox/exec policy |
| 恢复 | resume source + snapshot/output ref | agent graph + child rollout lazy reload |


## 7. 设计目标与非目标
### 7.1 目标
1. 同时支持一次性并行任务和长期 Agent 协作；
2. child 永远不能突破创建时 parent 的权限上界；
3. 后台完成不自动把大结果塞进父上下文；
4. 所有生命周期转换可持久化、回放和恢复；
5. 控制面等待不能阻塞 Session actor 或异步运行时线程；
6. 并发、驻留、总节点、深度、token 和外部进程都有独立上限；
7. 编码任务可以选择共享 workspace、只读 workspace 或独立 worktree；
8. 普通配置刷新默认不改变正在运行 Turn 的能力和权限语义；
9. UI 能准确展示“哪个 Agent 的哪次任务正在请求什么权限”；
10. 机制能被离线回放和故障注入验证。

### 7.2 非目标
+ V1 不实现跨机器分布式 Agent 调度；
+ V1 不允许后台 child 弹出交互审批；
+ 不让模型自行扩大深度、并发或权限预算；
+ 不通过共享可变内存让父子直接读写彼此 Context；
+ 不把所有 child rollout 自动合并进 root transcript；
+ 不承诺任意两个并行 worktree 可以自动无冲突合并。

## 8. 新架构总览
```latex
                         Management Plane

 Parent Agent Tool
       |
       v
 SubagentSupervisor  <---- UI / Host Control
       |
       +---- AgentRegistry -------- AgentNode tree
       +---- TaskRunStore ---------- states / results / refs
       +---- Scheduler ------------- queue / active permits
       +---- ResidencyManager ------ loaded / unloaded
       +---- WorkspaceManager ------ shared / readonly / worktree
       +---- ApprovalRouter -------- tagged approval requests
       +---- MailboxRouter --------- message / follow-up / completion
       |
       v
                       Execution Plane

 Child Session
   -> Child Turn
   -> Child StepSnapshot
   -> Tool execution
   -> rollout.jsonl
   -> TaskRun result commit
```

`SubagentSupervisor` 是每棵 root Agent tree 的控制面 actor。它只管理元数据、状态转换和路由，不在 actor 内执行模型或耗时 Tool。每个 child Agent Loop 仍在独立 async task/Session 中运行。

## 9. 双层数据模型
### 9.1 AgentNode
```latex
AgentNode {
  agent_id
  agent_path
  parent_agent_id
  child_session_id
  role
  created_at
  lifecycle_state
  delegation_envelope_ref
  mailbox_cursor
  residency_state
  latest_task_run_id
  rollout_ref
}
```

`AgentNode` 是稳定身份。`followup_task`、`send_message` 和恢复都以它为目标。一个 `AgentNode` 同时最多只能有一个非终态 `TaskRun`，因为多个运行不能安全地并发改写同一 child Session 的 Context Projection。`latest_task_run_id` 指向当前非终态任务，或最近一次已终结任务；它不是允许并发运行的集合。

### 9.2 TaskRun
```latex
TaskRun {
  task_run_id
  agent_id
  parent_task_run_id
  owner_parent_turn_id
  input_ref
  execution_mode
  state
  priority
  created_at
  started_at
  finished_at
  budget_snapshot
  result_summary
  result_ref
  artifact_refs
  error
  cancellation_reason
  consumed_by_parent
}
```

`TaskRun` 是一次执行。它有明确终态：

```latex
Queued
  -> Starting
  -> Running
  -> WaitingApproval | WaitingExternal | Running
  -> Completed | Failed | Cancelled | Interrupted | UnknownOutcome
```

`Backgrounded` 不是执行终态，而是父侧观察模式：

```latex
ParentWaitState = Foreground | Detached | NotWaiting
```

这样不会出现 Grok interim background response 被误判为任务已经失败或完成的问题。

### 9.3 为什么必须拆成两层
如果只保留 `TaskRun`：follow-up 只能通过复制旧任务恢复，长期协作别扭。

如果只保留 `AgentNode`：一次执行的 deadline、结果、错误、父 Turn 所有权和审计边界会混在 Thread 状态里。

双层模型的收益是：

+ 一个 Agent 可以执行多次有独立结果的任务；
+ 每次任务都能精确等待、取消、计费和评估；
+ child 可以卸载，但 Agent 身份、mailbox 和任务结果仍存在；
+ UI 可以分别展示 Agent 树和任务运行历史。

## 10. 稳定标识和所有权
标识必须分开：

```latex
AgentId        稳定 child 身份
AgentPath      人类和模型可读的树形地址
TaskRunId      一次执行
ChildSessionId child 的持久会话
ParentTurnId   创建或触发本次执行的父 Turn
StepSnapshotId child 当前 Step 的不可变运行快照
```

约束：

1. `AgentPath` 在同一 root tree 内唯一；
2. Agent path 改名不改变 `AgentId`；
3. 每个 `TaskRun` 只有一个 owner parent Turn；
4. follow-up 在同一 AgentNode 上串行创建新的 TaskRun；当前 TaskRun 未终结时只持久化 follow-up mailbox message，不提前创建第二个非终态 TaskRun；
5. resume 不复用旧 TaskRunId；
6. 所有取消、审批、结果和通知都必须至少携带 `AgentId + TaskRunId`；
7. Tool 完成迟到事件还必须携带 `ToolCallId + StepGeneration`。

## 11. DelegationEnvelope：权限与资源上界
创建 AgentNode 时必须持久化：

```latex
DelegationEnvelope {
  parent_agent_id
  parent_session_id
  parent_turn_id
  parent_step_snapshot_id
  authority_generation
  tool_allowlist
  skill_allowlist
  mcp_server_allowlist
  workspace_roots
  filesystem_ceiling
  network_ceiling
  sandbox_ceiling_hash
  approval_policy_ceiling
  max_depth_remaining
  max_child_nodes
  max_active_turns
  token_budget
  time_budget
  process_budget
  workspace_mode
  approval_route
}
```

child 的有效运行配置必须满足：

```latex
child_effective
  = parent_ceiling
    ∩ delegation_request
    ∩ role_policy
    ∩ current_global_policy
```

关键规则：

+ 放宽只影响以后新建的 AgentNode，不反向扩大已有 child；
+ 全局收紧通过 `LiveRevocationFence` 立即生效；
+ child 每个 Step 都把 Capability Snapshot 与 envelope allowlist 重新求交；
+ allowlist 使用稳定 `CapabilityId`，revision 单独校验；
+ 没有类型 metadata 的 MCP/custom Tool 也必须有稳定 CapabilityId 和显式 policy class，不能默认保留；
+ Skill 只提供说明，不授予 envelope 中不存在的 Tool、MCP、网络或文件权限；
+ child 再委派时，新 envelope 必须从自己的剩余上界派生。

## 12. Spawn 协议
### 12.1 请求
```latex
SpawnAgentRequest {
  task_name
  prompt_ref
  role
  context_mode
  execution_mode
  workspace_mode
  requested_model
  requested_reasoning_effort
  requested_capabilities
  budgets
  output_contract
}
```

### 12.2 原子创建流程
```latex
1. 校验父 StepSnapshot 和权限上界
2. 预留 Agent path、节点槽位、执行预算和 workspace lease
3. 构造并持久化 DelegationEnvelope
4. 写 AgentSpawnReserved 事件
5. 创建 AgentNode + TaskRun(Queued)
6. 准备 child Session / rollout / worktree
7. 写 AgentStarted 或 AgentStartFailed
8. 向 Scheduler 提交 TaskRun
9. 只有状态提交成功后才向父 Tool 返回 handle
```

任何一步失败都释放尚未 commit 的 reservation。进程在第 4 至 8 步崩溃时，恢复器根据 journal 决定幂等继续或标记 `StartFailed`，不能留下既无 child 又占用路径和配额的幽灵节点。

workspace 准备也属于 reservation journal。worktree 或 ephemeral 目录必须先创建在带 `AgentId` 的 staging 路径，完成后再原子登记为 active workspace；启动失败或恢复时只发现半成品目录，则由 `WorkspaceManager` 校验 owner marker 后删除残留并释放 workspace lease。不能删除没有匹配 owner marker 的既有目录。

### 12.3 上下文模式
协议定义以下模式，并按 Phase 分阶段交付：

| 模式 | 内容 | 默认用途 | 交付阶段 |
| --- | --- | --- | --- |
| `fresh` | system/developer + task prompt +显式附件 | 普通委派默认 | Phase 1 |
| `last_n_turns` | 过滤后的最近 N Turn | 强依赖近期对话 | Phase 4 |
| `full_fork` | 过滤后的完整可用父投影 | 明确要求镜像上下文 | Phase 4 |
| `resume_agent` | 同一 AgentNode 的既有 child context +恢复输入 | 崩溃恢复或 Runtime 内部重载 | Phase 3，内部接口 |
| `evidence_bundle` | 任务 prompt +选定文件/结果/记忆引用 | 低 token 精确委派 | Phase 4 |


默认采用 `fresh`，而不是 Codex 当前的 `all`。Runtime 可以提示模型在确实需要历史时选择 `last_n_turns` 或 `evidence_bundle`。`full_fork` 必须显式请求并计入更高预算。`resume_agent` 不作为模型侧继续协作的第二入口；它只服务恢复和内部 Residency reload，模型要让既有 Agent 继续工作只能调用 `followup_task`。

fork 前必须 materialize 和 flush 父 rollout，再从确定性 Context Projection 生成 child 输入。不能直接 clone 父内存消息数组。

## 13. 前台、后台和自动 detach
统一规则：

```latex
execution_mode = foreground
  -> 父 Tool future 等 TaskRun 终态
  -> 超过 foreground_lease 可显式 detach

execution_mode = background
  -> 创建成功后立即返回 handle
```

建议保留 Grok 的 45 秒作为默认 `foreground_lease` 初始值，但把它做成配置和事件，而不是隐藏行为：

```latex
TaskDetached {
  task_run_id
  reason = foreground_lease_expired | parent_turn_cancelled | user_request
  elapsed_ms
}
```

`await_to_completion=true` 只取消自动 detach，不取消全局 time/token/process budget。

父 Tool future 消失时：

+ 普通 Task 默认 detach；
+ workflow-owned、transactional 或显式 `cancel_with_parent` 的 Task 取消；
+ 是否 detach 必须来自 TaskRun policy，不能根据调用栈临时猜测。

## 14. Mailbox 与协作协议
每个 AgentNode 有持久 mailbox，支持：

```latex
send_message(target, message)
  -> QueueOnly，不启动 Turn

followup_task(target, message)
  -> 目标空闲：创建新 TaskRun 并立即 TriggerTurn
  -> 目标忙碌：持久化 follow-up 并标记 queued trigger，待当前 TaskRun 终结后再创建新 TaskRun

wait_agent(targets?, timeout)
  -> 等待 descendant 的 mailbox/task activity，事件驱动

mailbox_read(cursor?, limit?)
  -> 显式读取消息正文，并推进自己的 mailbox cursor

list_agents(path_prefix?)
  -> 查询树、状态、当前 TaskRun 和驻留情况

interrupt_agent(target)
  -> 中断当前 TaskRun，不删除 AgentNode

close_agent(target)
  -> 只有无 active TaskRun 时关闭身份；强制模式需单独权限
```

Mailbox message 使用版本化 envelope：

```latex
AgentMessage {
  message_id
  author_agent_id
  target_agent_id
  related_task_run_id
  kind = message | followup | completion | control
  trigger_turn
  payload_ref
  created_at
}
```

投递和 mailbox cursor 更新进入同一事件流。`send_message` 不允许通过旁路直接修改目标内存队列，否则卸载恢复后会丢消息。

`followup_task` 的 TriggerTurn 是“空闲时触发”而不是“并发触发”。目标存在非终态 TaskRun 时，Supervisor 只按 root rollout 顺序保存 follow-up message；当前任务提交终态后，Supervisor 才消费下一条 queued follow-up、创建新的 `TaskRunId` 并启动 Turn。同一节点积压多条 follow-up 时保持 FIFO，每条形成独立 TaskRun，除非调用方通过显式批处理协议要求合并。

### 14.1 消息怎样进入模型上下文
不同消息不能共用一个隐式注入通道：

| 消息类型 | 模型可见方式 |
| --- | --- |
| `message` | 不自动注入；`wait_agent` 只返回 metadata 和有界 preview，目标显式调用 `mailbox_read` 才读取正文 |
| `followup` | 作为新 TaskRun 的结构化输入，在目标 Turn 边界进入上下文；保留 `author_agent_id`，不能伪装成用户消息 |
| `completion` | 进入有界 completion notification；完整结果仍需 `task_get` |
| `control` | 默认只由 Runtime 消费；确需告知模型时使用 Loop V2 的版本化 runtime 合成消息协议 |


`mailbox_read` 的 Tool Result 才会把普通消息正文放入当前 transcript。消息 cursor 采用“先读、后确认”：只有该 Tool Result 已按 CommitSequence 写入目标 transcript，Loop 才向 Supervisor 提交 `AgentMessageConsumed` 并推进 cursor。崩溃最多造成恢复后重复返回尚未确认的消息，不能造成消息尚未进入 transcript 就被标记消费。`wait_agent` 返回 preview 不推进 cursor。

### 14.2 等待语义与 Step 提交
`wait_agent` 和 `task_wait` 都是普通 Tool future，仍受 Agent Loop V2 的 CommitSequence 约束。为避免一个长等待长期挡住同批后续 Tool Result：

1. 默认 timeout 为 30 秒；硬上限不得超过配置的 `foreground_lease`，也不得超过当前 Turn 剩余 wall-time budget；
2. timeout 时返回“尚未完成”的目标、已完成子集、mailbox metadata 和当前状态，不把 timeout 当成 Tool error；
3. 模型可以先处理已完成结果，下一 Step 再决定是否继续等待；
4. user steer 或父 Turn cancellation 到达时，按 Loop V2 Interjection 协议取消 wait future，返回 `interrupted=true` 和当时的状态快照；
5. wait 只订阅事件，不持有 child execution permit、workspace lease 或 rollout writer；
6. `wait_agent` 和 `task_wait` 只能等待调用者自己的 TaskRun 或 descendant Agent，禁止等待 ancestor、sibling 和任意横向目标，从协议上消除 A 等 B、B 等 A 的等待环。

等待期间控制面仍可接收取消、查询、审批和 steer；上述有界返回保证父 Step 的 transcript 也不会被无限冻结。

## 15. 任务结果库
结果库是 rollout 的派生投影，不是第二事实源：

```latex
root Supervisor rollout.jsonl
  -> TaskRun projection
  -> task_results table / blob refs
```

完成提交：

```latex
Child rollout: TaskExecutionFinished
  -> result payload 或 blob ref 已在 child Session 持久化
  -> 向 SubagentSupervisor 提交 terminal claim

Supervisor rollout: TaskResultCommitted
  -> TaskRun state = Completed/Failed
  -> CompletionNotificationEnqueued
```

`TaskExecutionFinished` 只是 child 已经结束执行的证据，不是 TaskRun 终态。只有 SubagentSupervisor 能在自己的单一事实流中提交 `TaskResultCommitted`、`TaskRunCancelled`、`TaskRunInterrupted` 或 `TaskRunUnknownOutcome`。如果 cancel 与 finish 竞争，Supervisor 接收并成功持久化的第一个合法 terminal commit 获胜；不比较父子两个 rollout 的 `seq`。

结果结构：

```latex
TaskResult {
  task_run_id
  status
  summary
  output_ref
  structured_output_ref
  artifact_refs
  evidence_refs
  changed_files
  validation_summary
  error
  usage
}
```

父 Agent 使用：

```latex
task_get(task_run_id, detail = summary | full | artifacts)
task_wait(task_run_ids, timeout, return_when = any | all)
```

完成通知只带：

+ Agent path；
+ TaskRunId；
+ status；
+ 有界摘要；
+ result fingerprint；
+ 是否存在未读取完整结果。

完整结果只有在显式 `task_get` 后才作为 Tool Result 进入父 transcript。与 mailbox 相同，只有 Tool Result 已按 CommitSequence 提交后，Supervisor 才写 `TaskResultConsumed` 并将 `consumed_by_parent=true`；失败重放允许重复读，不能提前丢失未提交结果。这比 Codex 自动投递任意长度 final answer更可控，也保留了 Grok 的显式结果读取能力。

## 16. Scheduler 与资源治理
### 16.1 资源维度
不能只设置一个 `max_agents`。至少拆成：

```latex
max_total_agent_nodes
max_tree_depth
max_children_per_agent
max_active_child_turns
max_resident_child_sessions
max_provider_requests
max_external_processes
tree_token_budget
task_token_budget
task_wall_time_budget
```

### 16.2 调度状态
```latex
Queued
  -> 等待 execution permit
Starting
  -> 初始化 Session/workspace
Running
  -> 持有 active-turn permit
WaitingApproval
  -> 释放模型采样 permit；是否保留进程 permit按 Tool 决定
WaitingExternal
  -> 按资源类型释放可释放的 permit
Terminal
  -> 释放全部 execution permit
```

### 16.3 预算记账精度
模型调用的 `usage_delta` 使用 [Provider / Model 适配层 V2 §15](./14-provider-model-adapter-v2-design.md#15-usage-与-rate-limit-统一模型) 的 `UsageRecord`：Provider 结算值优先，缺失时才使用带 `estimated=true` 的估算值。Supervisor 只聚合和仲裁树级预算，不让各 child 用不同 tokenizer 或字段含义自行记账。

树级预算由 SubagentSupervisor 持有权威计数。child provider 调用产生 usage 后，先写入 child rollout，再把带 `AgentId + TaskRunId + provider_request_id` 的 usage delta 上报 Supervisor；Supervisor 去重后更新树级投影并写 `AgentUsageRecorded`。

V1 的预算不是逐 token 的分布式硬实时闸门：

+ 启动一个新 TaskRun 或新 child Turn 前必须向 Supervisor 检查并预留最低预算；
+ 单个 Turn 内允许发生最多该 Turn 已授权上限的有限超支；
+ usage 回报后立即扣减树级余额，余额不足时不再启动下一 Turn；
+ task token、wall time 和 process budget 仍由 child 本地硬限制；
+ provider retry 必须复用 request identity 或单独记录 attempt，避免重复记账；
+ child 在收到 Supervisor 的 durable usage acknowledgement 前保留未确认 delta 并重试；恢复时只针对未确认 usage 读取对应 child rollout，不全量扫描整棵树；
+ UI 和诊断必须区分 `reserved/observed/remaining`，不能把异步 observed 值描述成精确实时余额。

这种精度先保证实现和恢复可解释；如果 Eval 证明单 Turn 超支不可接受，再引入每次 provider sampling 前的集中租约，不在 V1 假装拥有逐 token 强一致性。

### 16.4 公平性
V1 采用可解释的加权 FIFO：

1. 前台 Task 高于后台 Task；
2. 同优先级按 enqueue sequence；
3. 每个父 Agent 有并发配额，防止单个 child 占满全树；
4. 等待时间超过阈值可有限提升，但不能越过安全和预算上限；
5. 内部 harness Task 使用独立小配额，不能饿死用户任务。

这补上 Grok 缺少显式执行限流的问题，同时比只限制 Codex resident thread 更清楚地区分“存在”“在内存”“正在执行”。

## 17. Residency 和恢复
借鉴 Codex V2，AgentNode 与 resident Session 分离：

```latex
AgentNode exists
  ├─ Resident：Session 在内存
  └─ Unloaded：只保留 rollout、mailbox 和 metadata
```

允许卸载必须同时满足：

+ 没有 active TaskRun；
+ 没有未处理审批；
+ mailbox 没有需要立即触发的消息；
+ rollout、result 和 workspace metadata 已 flush；
+ 不在 protected set，例如当前 UI 正查看或父正在 `task_wait`。

达到上限时按 LRU 卸载。恢复流程：

```latex
1. 读取 AgentNode metadata 和 DelegationEnvelope
2. 验证当前 global revocation fence
3. 恢复 child rollout 与 Context Projection
4. 恢复 mailbox cursor 和 pending messages
5. 重新捕获允许范围内的新 TurnCapabilityBase
6. 注册 resident slot
7. 按消息 trigger_turn 决定是否开始新 TaskRun
```

启动 root Session 时不自动加载所有 descendants，避免大 Agent 树拖慢启动。

如果 envelope 允许的 Capability 在卸载期间已经从管理面消失，例如 MCP server 被删除，恢复后的有效 Tool 集合允许收窄，但不能用旧 revision 或旧 client handle 继续执行。缺失项写入 Capability V2 `capabilities.explain` 的诊断投影，包含 CapabilityId、原 revision、消失来源和受影响 TaskRun；这是可诊断的安全降级，不是恢复失败，也不能静默替换成同名能力。

## 18. Workspace 隔离和并行写
`workspace_mode` 支持：

| 模式 | 写入能力 | 适用场景 |
| --- | --- | --- |
| `shared_readonly` | 禁止写 | 搜索、分析、评审 |
| `shared_write` | 可写共享目录 | 串行或明确无冲突任务 |
| `worktree` | 写独立 Git worktree | 并行编码默认推荐 |
| `ephemeral` | 临时目录，结束按策略清理 | 生成物、实验和转换 |


规则：

1. 只读 shell 是否可并发由 sandbox profile 保证，不靠命令字符串猜测；
2. `shared_write` child 必须进入 workspace write scheduler；
3. 结构化文件 Tool 使用规范化路径锁；
4. 不可静态分析写集合的 bash 在可写 sandbox 中获取 workspace 级写 lease；
5. worktree child 完成后提交 changed-files manifest 和 base revision；
6. Runtime 不自动把冲突 merge 当成成功；
7. worktree 丢失时可由 snapshot rehydrate；无法证明恢复一致性则 fail closed，不静默回落共享写目录；
8. 只有显式允许降级的只读任务可以从隔离模式回落共享 workspace。

这里保留 Grok 的 worktree 优势，同时补上并行写资源锁和失败语义。

## 19. 审批路由
### 19.1 前台 child
前台 child 可以进入 `ask`，审批事件必须携带：

```latex
root_session_id
parent_agent_path
child_agent_path
agent_id
task_run_id
tool_call_id
capability_id
capability_revision
args_hash
policy_generation
delegation_envelope_hash
```

UI 必须展示来源 Agent、任务描述和实际参数，审批响应由 `ApprovalRouter` 定向回复对应 oneshot/promise。审批结果只能在 envelope 上界内 allow/deny，不能借交互扩大 child ceiling。

### 19.2 后台 child
V1 后台 child 不允许交互式 `ask`：

+ allow：执行；
+ deny：拒绝；
+ ask：返回 `approval_required_in_background`，Task 可失败或等待父 Agent 显式提升；
+ 用户可以把 Task 切到 foreground 后重新发起审批；
+ 已经在 UI 显示的审批不能因 Task detach 而悄悄改变归属。

这避免用户正在操作 root 对话时，突然审批一个几分钟前后台 Agent 的高风险命令。

`promote_task_to_foreground` 只改变后续父侧等待和审批路由，不重放原来的 Tool Call。原后台调用已经以 `approval_required_in_background` 终止；提升后由模型根据该 Tool Result 重新发起调用，生成新的 ToolCallId、args hash 和审批请求。旧审批决定不得复用于新调用。

## 20. Capability 与配置刷新
child 创建时从 parent `StepCapabilitySnapshot` 派生初始上界，但自己的 Turn 内仍遵循统一缓存规则：

+ 普通 MCP/Skill/Tool catalog 刷新默认下一 Turn 采纳；
+ Turn 内 Deferred promotion 只加入 `TurnCapabilityOverlay`；
+ 安全收紧由 `LiveRevocationFence` 立即生效；
+ stale revision 拒绝后才允许重建 Snapshot 并重采样；
+ child 新捕获的 capability 必须与 DelegationEnvelope allowlist 求交；
+ 父后续扩权不会自动进入既有 child；
+ capability、policy、sandbox 和 envelope hash 都写入 child StepSnapshot。

## 21. 取消和中断
取消目标分为：

```latex
interrupt_task(task_run_id)
  -> 中断当前执行，AgentNode 保留

cancel_owned_by_parent_turn(parent_turn_id)
  -> 取消该 Turn 创建且 policy=cancel_with_parent 的 TaskRun

close_agent(agent_id)
  -> 关闭身份和 mailbox，不默认删除 rollout/result

cancel_subtree(agent_id)
  -> 显式高影响操作，按后序遍历取消 descendants
```

传播规则：

1. parent 被 interrupt 不自动杀死所有历史后台 child；
2. 每个 TaskRun 的 `parent_cancel_policy` 在创建时固定；
3. child 当前 Tool 收到 cancellation token；
4. 有外部副作用的 Tool 若取消时无法判断结果，记录 `UnknownOutcome`；
5. 迟到完成事件必须校验 AgentId、TaskRunId、ToolCallId 和 generation；
6. 已 commit 的 TaskResult 不因迟到 cancel 改写为 Cancelled；
7. subtree cancel 不能绕过正在显示的审批和 journal，必须逐个进入可恢复状态迁移。

## 22. 单一事件日志
不新建独立 `subagent-events.jsonl`，但必须区分 root Supervisor rollout 与 child Session rollout 的事件归属。每份 rollout 的 `seq` 只在本文件内单调，**跨 Session 不存在可比较的全局 seq**。

### 22.1 事件归属
| 事实类型 | 唯一写入位置 | 作用 |
| --- | --- | --- |
| Agent tree、spawn reservation、TaskRun control state | root Session 的 Supervisor rollout | 重建 AgentRegistry、TaskRun 和 Scheduler |
| TaskRun terminal commit、结果引用、消费状态 | root Session 的 Supervisor rollout | 唯一终态仲裁与 Task result projection |
| mailbox enqueue、route、cursor/consume | root Session 的 Supervisor rollout | 重建持久 mailbox，不扫描所有 child 才能找消息 |
| child 模型、Tool、Step、usage、`TaskExecutionFinished` | 对应 child Session rollout | 深挖 child 执行、恢复 child context 和验证结果证据 |
| child 将 mailbox 输入加入模型上下文 | child Session rollout 的 `MailboxInputAttached` | 证明模型实际看到了哪条消息 |


Supervisor rollout 增加：

```latex
AgentSpawnReserved
AgentNodeCreated
AgentStarted
AgentStartFailed
AgentMessageEnqueued
AgentMessageConsumed
TaskRunQueued
TaskRunStarted
TaskDetached
TaskWaitingApproval
TaskResultCommitted
TaskResultConsumed
TaskRunCancelled
TaskRunInterrupted
TaskRunUnknownOutcome
AgentUsageRecorded
AgentResidencyLoaded
AgentResidencyUnloaded
AgentCompletionNotified
AgentClosed
```

child rollout 增加：

```latex
TaskExecutionFinished
TaskUsageObserved
MailboxInputAttached
```

事件使用 [Agent Loop V2](./09-agent-loop-v2-design.md) 已定义的版本化信封、文件内单调 `seq`、blob 外置、写前脱敏、尾部半行截断和中间损坏 fail-closed 规则。父子之间只用稳定 ID、result fingerprint 和 blob ref 关联，不使用 wall clock 或跨文件 seq 决定因果顺序。

### 22.2 单 writer 规则
SubagentSupervisor 虽然是独立 actor，但不能自行打开 root rollout 的第二个写句柄。它产生的所有控制事件都发送到 root Session 与 Agent Loop 共用的 rollout writer queue；只有这个 writer 可以分配 root `seq` 和 append。Supervisor 必须等待 durable append acknowledgement 后才能：

+ 对外确认 Agent 创建；
+ 宣布 terminal commit 获胜；
+ 派发 completion notification；
+ 推进 mailbox cursor；
+ 释放依赖持久状态的 reservation。

child 同样只通过自己的 Session writer 写 child rollout。这样父 Session 流式输出、Tool commit 和 Supervisor 终态事件在 root 文件中有一个确定顺序，不会出现两个 actor 竞争写同一文件。

Agent graph、TaskRun、Scheduler、mailbox 和 Task result table **只从 root Supervisor rollout 重建**。child rollout 只用于恢复和审计单个 child；正常 root 恢复不需要合并扫描所有 child 文件。共享 Agent graph store 仍是可重建加速投影，不参与终态仲裁。

这是 V1 有意接受的吞吐上限：整棵树的 mailbox、TaskRun control、终态和 usage acknowledgement 都串行经过 root writer，因此深树和高频消息会让 root rollout 成为热点并持续膨胀。实现必须通过有界队列、批量 flush、usage delta 合并和指标暴露控制压力，不能为提高吞吐给 Supervisor 打开第二个写句柄。只有 Eval 证明 root writer 已成为实际瓶颈后，才考虑 per-subtree Supervisor 分片；分片必须先重新设计跨分片终态仲裁，不能破坏本节的单一事实源。

### 22.3 Root rollout 保留资格
只要 Agent tree 中仍存在未进入 `Closed` 的 `AgentNode`，root Session 就仍是可恢复的活跃控制面，不得进入 rollout TTL 删除资格，即使 root 自身当前没有运行中的 Turn。关闭全部 AgentNode、提交所有 TaskRun 终态并处理保留期后，root rollout 才能按 Agent Loop V2 的终态 Session 规则参与清理。清理器必须从可重建 Agent graph projection 预检，并以 root rollout 中的 `AgentClosed` 事实复核，不能只依据最近一次 root Turn 的状态。

## 23. 崩溃恢复
恢复按状态处理：

| 最后持久事件 | 恢复行为 |
| --- | --- |
| `AgentSpawnReserved` | 检查资源是否已创建；幂等继续或释放 reservation |
| `AgentSpawnReserved` 后只有半创建 worktree | 校验 owner marker，删除 staging 残留，释放 workspace lease 和 spawn reservation |
| `AgentNodeCreated` 无 `AgentStarted` | 恢复启动或写 `AgentStartFailed` |
| `TaskRunQueued` | 重新进入 Scheduler，保持原 enqueue sequence |
| Supervisor 有 `TaskRunStarted`、无 terminal commit | 查询对应 child rollout/provider/tool journal；不能确认则由 Supervisor 提交 `Interrupted` 或 `UnknownOutcome` |
| child 有 `TaskExecutionFinished`、Supervisor 无 terminal commit | 校验 result fingerprint/blob ref 后，由 Supervisor 幂等提交 `TaskResultCommitted`；若已有其他终态则只记迟到诊断 |
| child 有 `MailboxInputAttached`、Supervisor 无对应 `AgentMessageConsumed` | 按 at-least-once 语义重新投递；允许目标再次看到同一 `message_id`，不得根据 child 事件跨文件推进 root cursor |
| `TaskResultCommitted` 无 notification | 补发有界完成通知 |
| notification 已发、未 consumed | 不重复注入；`task_get` 仍可读取 |
| Agent unloaded | 保持 unloaded，需要时 lazy reload |


`TaskExecutionFinished` 必须携带结果本体或 blob reference，否则“执行已完成、提交前崩溃”会无谓退化成 `UnknownOutcome`。恢复器不能拿 child seq 与 root seq 比较；它只检查 root 是否已有合法 terminal commit。

## 24. 与 Context 和 Compaction 的结合
父 Context Capsule 不复制 child rollout，只保存：

```latex
active_agent_refs
active_task_run_refs
pending_completion_refs
unconsumed_result_refs
pending_approval_refs
workspace_lease_refs
```

compaction 后 Runtime 根据这些结构化引用恢复提醒，不依赖摘要模型记住任务 ID。

child Context 仍独立压缩。父 full fork 只发生在 spawn 一致点；父后续 compaction 不改写 child 历史。child result 被父显式读取后，父 transcript 保存有界 Tool Result 和 result reference，而不是复制全部 child rollout。

## 25. 核心不变量
1. 一个 Agent path 在 root tree 内只映射一个 AgentId；
2. 一个 TaskRun 只能属于一个 AgentNode；
3. TaskRun 终态不可逆；
4. `Backgrounded/Detached` 不是执行终态；
5. child 有效权限永不宽于持久化 DelegationEnvelope；
6. child 再委派后的权限和预算只能继续收窄；
7. 安全收紧立即生效，权限放宽不回灌已有 child；
8. 后台 completion notification 不等于完整结果已进入父 transcript；
9. 完整结果只有 `TaskResultCommitted` 后可见；
10. `TaskResultCommitted` 必须先于 completion notification；
11. mailbox message 在触发 Turn 前必须持久化；
12. 任何 resident eviction 前必须 flush rollout 和 mailbox cursor；
13. 没有 active permit 的 TaskRun 不得启动模型采样；
14. 达到 total node、depth、resident 或 execution 任一上限都必须显式失败或排队；
15. worktree 恢复失败不得静默获得共享写权限；
16. 审批绑定 ToolCallId、CapabilityId、revision、args hash、policy generation 和 envelope hash；
17. 迟到事件必须通过 AgentId、TaskRunId、ToolCallId 和 generation fence；
18. root Supervisor rollout 是 Agent graph、TaskRun、mailbox 和 Task result 的事实源；child rollout 是 child 执行事实源；
19. 同一 root rollout 和策略版本必须重建相同 Agent/Task 投影，不需要跨 Session seq 全序；
20. root Session 恢复不要求一次性加载所有 descendants；
21. 一个 AgentNode 同时最多存在一个非终态 TaskRun，忙时 follow-up 只能排队；
22. 存在未关闭 AgentNode 的 root rollout 不具备 TTL 删除资格。

## 26. 关键失败模式
### 26.1 Spawn 风暴
模型连续创建大量 child。通过 total nodes、children per parent、active turns、provider requests 和 token budget 多重限制控制；不能只依赖最大深度。

### 26.2 权限逃逸
child role、Skill、MCP refresh 或 custom Tool 获得父没有的能力。通过 DelegationEnvelope、稳定 CapabilityId、每 Step 求交和 live revocation 阻断。

### 26.3 后台审批错位
用户不知道弹窗来自哪个 child。V1 后台 ask 直接失败；前台审批必须带完整路由身份。

### 26.4 结果污染上下文
多个 child 同时完成并把长输出塞入 root prompt。通知只带有界摘要和 TaskRunId，完整内容显式读取。

### 26.5 共享目录写冲突
两个 child 同时运行 bash 修改相同文件。只读由 sandbox 保证可并发；共享写 bash 获取 workspace write lease；推荐并行编码使用 worktree。

### 26.6 完成与取消竞态
不比较父子 rollout 的 event seq。finish 和 cancel 都向 SubagentSupervisor 提交 terminal claim，只有 Supervisor 能通过 root Session 的单 writer 写终态。root rollout 中先 durable commit 的 `TaskResultCommitted/TaskRunCancelled/TaskRunInterrupted/TaskRunUnknownOutcome` 获胜；迟到 claim 只记录诊断，不回滚已提交结果。

### 26.7 卸载丢消息
所有 mailbox 写入先持久化，再通知 resident Session；unloaded child 下次加载按 cursor 消费。

### 26.8 无限递归委派
同时使用 depth、total nodes、children per parent、tree token budget 和 DelegationEnvelope 剩余预算。Codex V2 当前忽略 legacy depth limit 的行为不能原样继承。

### 26.9 Agent 相互等待死锁
模型侧 `wait_agent/task_wait` 只能等待调用者拥有的 TaskRun 或 descendant Agent，禁止等待 ancestor、sibling 和横向任意 Agent，因此等待依赖沿树向下，不形成环。跨兄弟协作使用非阻塞 `send_message`；需要汇总时由共同 ancestor 等待两边。Runtime 仍应在诊断中记录超时和异常长等待链。

## 27. 对外 Tool 设计
模型侧 V1 暴露：

```latex
spawn_agent
send_message
followup_task
wait_agent
mailbox_read
list_agents
interrupt_agent
task_get
task_wait
```

管理/UI 侧额外提供：

```latex
close_agent
cancel_subtree
inspect_agent
inspect_task_run
list_unconsumed_results
promote_task_to_foreground
```

`promote_task_to_foreground` 不执行或重放 Tool，只改变 Task 的观察模式和未来审批路由。此前因后台 `ask` 失败的 Tool 必须由模型生成新 ToolCallId 后重新调用。

`spawn_agent` 返回：

```json
{
  "agent_path": "/root/reviewer",
  "agent_id": "...",
  "task_run_id": "...",
  "state": "queued",
  "execution_mode": "background",
  "workspace_mode": "worktree"
}
```

模型不直接得到内部 Thread handle、permission channel 或可变 Session 对象。

`resume_agent` 不出现在模型侧 Tool 列表。它是恢复器和 ResidencyManager 使用的内部 context mode；正常持续协作统一通过 `followup_task`，避免两套近似入口产生不同的 TaskRun、审批和审计语义。

## 28. 相对原实现的收益
### 28.1 相对 Grok Build
| Grok 当前问题 | 新设计 | 收益 |
| --- | --- | --- |
| child 偏一次性 Task | AgentNode + TaskRun | 同一 child 可持续 follow-up，同时保留每次任务终态 |
| 缺少通用双向通信 | 持久 mailbox +稳定 AgentPath | 支持协作、消息排队和卸载后恢复 |
| 固定深度 1 | 多维预算下的受控树 | 能表达 reviewer、researcher 等层级，不允许无限递归 |
| 未见 active semaphore | Scheduler + execution permits | 防止批量 spawn 打满模型和进程资源 |
| custom/MCP 无 kind 可绕过过滤 | 所有能力必须有 CapabilityId 与 policy class | child 能力上界可验证 |
| permission mode 不等于形式化 ceiling | 持久 DelegationEnvelope | 委派不能成为权限逃逸通道 |
| completed 数量淘汰 | residency + rollout lazy reload | 大树长期存在但不长期占内存 |
| resume_from 创建恢复任务 | resume AgentNode +新 TaskRun | 身份、历史和任务审计更清楚 |


### 28.2 相对 Codex V2
| Codex 当前问题 | 新设计 | 收益 |
| --- | --- | --- |
| 默认 full fork | fresh 默认 + evidence bundle/last N | 降低 token、缓存失效和父历史污染 |
| Agent Thread 与一次任务边界混合 | 独立 TaskRun 状态机 | 精确等待、取消、计费、结果和 Eval |
| completion 携带 normal final answer | 有界通知 +显式 task_get | 防止并发 child 结果冲爆父上下文 |
| 结果主要存在 child rollout | Task result projection + blob ref | 父 Agent 和 UI 可按任务稳定读取 |
| 未见自动 worktree | WorkspaceManager + worktree/snapshot | 并行编码隔离更强 |
| V2 忽略 legacy depth limit | depth + nodes + concurrency + budget | 防止深树和 spawn 风暴 |
| interrupt 面向 Thread 当前 Turn | TaskRun/ParentTurn/Subtree 多级取消 | 取消作用域更精确 |
| 继承当前父策略但无持久 ceiling | 每 Step 与 envelope 求交 | 后续刷新和恢复仍不能扩权 |


### 28.3 新增的系统级收益
+ **可解释性**：能回答“谁创建了哪个 Agent、它当前执行哪次任务、为什么拥有这些能力”；
+ **安全性**：权限、网络、文件、MCP 和再委派全部有单调收窄上界；
+ **上下文效率**：完成通知与完整结果分离，默认 fresh/evidence 委派；
+ **资源稳定性**：存在、驻留、执行、进程和 token 分别限流；
+ **恢复能力**：Agent 身份、mailbox、TaskRun 和结果都能由 rollout 重建；
+ **编码隔离**：worktree、workspace lease 和 changed-files manifest 进入统一协议；
+ **产品能力**：UI 可以同时展示 Agent 树、任务列表、审批来源、结果消费状态和工作区；
+ **可评估性**：每次 TaskRun 有明确输入、输出、预算和终态，可直接组成离线 Eval 样本。

### 28.4 关键设计决策的收益闭环
| 设计决策 | 解决的问题 | 为什么这样选 | 直接收益 | 验证指标 |
| --- | --- | --- | --- | --- |
| AgentNode 与 TaskRun 分离 | 长期身份和一次执行混在同一对象 | 二者生命周期、预算和终态不同 | Agent 可持续协作，每次任务仍可审计 | follow-up 成功率、状态歧义数 |
| 每节点最多一个非终态 TaskRun | 并发运行争用同一 child Context | 同一 transcript 只能有一个 writer | follow-up FIFO、上下文无竞态 | 非法并发拒绝数、队列延迟 |
| DelegationEnvelope 单调收窄 | 委派可能成为权限逃逸通道 | child authority 必须有持久上界 | 刷新、恢复和再委派都不能扩权 | escape attempt 阻断率 |
| 有界通知 +显式 `task_get` | 多 child 长结果自动冲爆父上下文 | “完成”与“父已消费”是不同状态 | 父只读取需要的结果 | 通知 token、结果遗忘率 |
| 持久 mailbox +先读后确认 | 卸载/崩溃造成消息丢失 | at-least-once 比提前确认更安全 | 消息不丢，重复可由 message_id 识别 | 丢失率、重复投递率 |
| Supervisor 单终态仲裁 + root writer | 跨 child rollout 的 seq 不可比较 | 终态竞争必须回到一个顺序域 | finish/cancel 恢复确定 | 双终态数、投影重建一致率 |
| Scheduler/Residency 分离 | Agent 存在、驻留和执行混为一个上限 | 三类资源成本不同 | 大树可存在但不挤满模型和内存 | permit 等待、resident 命中率 |
| worktree + workspace lease | 并行 child 互相覆盖文件 | 目录隔离与写锁分别处理可知/未知写集合 | 提高并行编码安全性 | 合并冲突率、共享写冲突数 |


## 29. 分阶段实现
### 29.1 跨文档实际顺序
不能把 Memory、Loop、Capability、Context 和本文各自列出的 Phase 1 理解为五条并行开发线。资源有限时，真正的最小可行切片是：

```latex
1. Agent Loop V2 Phase 1
2. Context Management V2 Phase 1
3. 同步预留 DelegationEnvelope、AgentId、TaskRunId 和事件类型 schema
4. Loop/Context 事实源和 Reducer 稳定后，再实现本文 Supervisor/TaskRun Phase 1
```

也就是说，在 Loop V2 Phase 1 中只预留 Sub-agent envelope 与事件字段，不同时实现完整 Supervisor、mailbox、结果库、调度和 worktree。本文后续 Phase 是 Sub-agent 子系统内部顺序，前置依赖是统一 rollout writer、Reducer、Step 状态机和随机化调度测试已经可用。

### 29.2 Sub-agent Phase 1：任务事实源和安全边界
+ 定义 AgentId、TaskRunId、AgentPath 和状态机；
+ 按 §22 归属在 root/child rollout 信封中加入 Agent/Task 事件；
+ 实现 SubagentSupervisor single state owner actor，并通过 root Session 唯一 rollout writer 持久化；
+ 实现 DelegationEnvelope、稳定 CapabilityId allowlist 和 live revocation；
+ 实现 total nodes、depth 和 active-turn permits；
+ 后台 ask fail closed；
+ 实现 child `TaskExecutionFinished` evidence -> Supervisor terminal claim -> root `TaskResultCommitted` journal；
+ 先支持 fresh context 和 shared readonly/shared write。

### 29.3 Sub-agent Phase 2：结果库和父子协议
+ 实现有界 completion notification；
+ 实现 `task_get/task_wait` 和 consumed 标记；
+ 实现持久 mailbox；
+ 实现 `send_message/followup_task/wait_agent/mailbox_read/list_agents/interrupt_agent`；
+ wait timeout、steer interjection、descendant-only 和有界 preview 进入协议测试；
+ 实现 ParentTurn ownership 和精确取消；
+ Context Capsule 保存 active/unconsumed refs。

### 29.4 Sub-agent Phase 3：隔离和恢复
+ 实现 worktree/ephemeral workspace；
+ 实现 workspace lease、路径锁和 changed-files manifest；
+ 实现 Agent graph projection；
+ 实现 ResidencyManager、LRU unload 和 lazy reload；
+ 实现 restart recovery 和未完成 journal 重放。

### 29.5 Sub-agent Phase 4：上下文与调度优化
+ 增加 last N、full fork 和 evidence bundle；
+ fork 前父 rollout 一致点；
+ 加权 FIFO、公平性和 provider/process permits；
+ tree rollout budget 和动态预算提醒；
+ 支持受控多层 Agent tree；
+ 根据 Eval 决定是否增加自动任务路由或结果摘要策略。

## 30. 测试与评估
### 30.1 状态机单测
覆盖：

+ spawn/start/cancel/complete 的所有合法和非法迁移；
+ foreground lease 到期只 detach、不终止 Task；
+ result commit 先于 notification；
+ follow-up 创建新 TaskRun 而不是复用旧终态；
+ 同一 AgentNode 不允许两个非终态 TaskRun，忙时 follow-up 按 FIFO 等待当前任务终结；
+ 迟到事件不能改写终态；
+ mailbox cursor 重放不重不漏；
+ wait timeout 返回已完成子集，steer 能中断 wait；
+ ancestor/sibling wait 被协议拒绝；
+ child finish seq 与 root cancel seq 不被跨文件比较。

### 30.2 随机化并发测试
随机交错：

+ child started 与 cancel；
+ result finished 与进程崩溃；
+ approval response 与 detach；
+ mailbox enqueue 与 residency unload；
+ follow-up 与 restart recovery；
+ 多条 follow-up 与当前 TaskRun terminal commit 的交错；
+ worktree snapshot 与 cancel；
+ global revocation 与 Tool execution；
+ root Loop event 与 Supervisor terminal commit 竞争同一 writer；
+ child usage 上报、Supervisor ack 与进程崩溃。

验证不变量、无死锁、无重复结果、无权限放宽。

### 30.3 故障注入
在以下位置强制崩溃：

+ AgentSpawnReserved 后；
+ child Session 创建后、AgentStarted 前；
+ TaskExecutionFinished 后、TaskResultCommitted 前；
+ result committed 后、notification 前；
+ mailbox 持久化后、resident wake 前；
+ rollout flush 后、residency remove 前；
+ worktree 创建或 snapshot 写入一半时。

### 30.4 离线 Eval
至少统计：

+ Task 成功率和验证通过率；
+ 父 Agent 是否读取了真正需要的 child 结果；
+ 无用 full fork 比例和每次委派 token 成本；
+ completion notification 对父 Context 的 token 占用；
+ 并发带来的墙钟收益与失败率变化；
+ worktree 合并冲突率；
+ 权限 ask/deny/escape attempt 数量；
+ unload/reload 延迟和恢复失败率；
+ child 结果被遗忘、重复读取和 stale 使用比例。

V1 将“结果被遗忘”定义为：`TaskResultCommitted` 后，所属父 Turn 已结束，并且经过后续 N=3 个父 Turn 或 30 分钟（先到者）仍没有 `TaskResultConsumed/task_get`，同时结果没有被显式标记为 `ignore` 或 `notification_only`。这两个标记来自创建 TaskRun 时持久化的 `output_contract`：`ignore` 表示调用方声明无需消费结果，`notification_only` 表示有界完成通知本身就是约定交付物；运行完成后不能为改善指标临时补标。Eval 必须同时报告 committed 总数、eligible 总数和 forgotten 数，避免把尚未给父 Agent 处理机会的结果算作遗忘。N 和时间窗是版本化 Eval 参数，不直接参与在线排名或自动惩罚。

只有 Eval 证明收益后，才增加更复杂的自动路由、自动深度或智能调度策略。

## 31. 最终判断
Grok Build 和 Codex V2 已经分别证明了两条互补的路线：

```latex
Grok Build
  子任务需要明确状态、等待、结果、取消和工作区隔离

Codex V2
  子 Agent 需要稳定身份、mailbox、持久 Thread 和驻留恢复
```

新的实现把二者统一为：

```latex
AgentNode tree
  + TaskRun state machine
  + DelegationEnvelope
  + persistent mailbox
  + explicit result store
  + scheduler and residency
  + workspace isolation
  + rollout-based recovery
```

最核心的收益不是“能够创建更多 Agent”，而是让委派同时具备五个可证明属性：

1. child 是谁、在做哪次任务，可以准确寻址；
2. child 能做什么，永远不超过创建时的权限上界；
3. child 是否完成、父是否读过结果，是两个不同状态；
4. child 不在内存或进程重启后，仍能从事实日志恢复；
5. 并发带来的速度收益不会以失控资源、上下文污染和共享目录冲突为代价。

