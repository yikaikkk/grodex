# 内置 Tool 本体对比与 V2 设计
## 1. 文档定位
[Tool、Skill 与 MCP V2](./10-tool-skill-mcp-v2-design.md) 已经设计了 Tool 如何注册、暴露、冻结、审批和执行，但没有定义模型每天真正使用的内置 Tool 应该具有什么行为。

本文从 Grok Build 与 Codex 当前实现出发，重点设计三类基础能力：

+ `Read`：模型怎样稳定读取文本、图片和文档；
+ `Edit`：小范围精确编辑与多文件 patch 怎样共存；
+ `Exec`：短命令、长进程、stdin、cwd、env、取消和输出怎样管理。

这里的“Tool 本体”不是 Pipeline。二者边界是：

```latex
Tool Pipeline
  负责 validation、policy、approval、sandbox、journal、commit order

Built-in Tool Runtime
  负责具体文件/进程语义、结果格式、版本栅栏和副作用事实
```

Tool 的 description、输入 Schema 和 output 格式会直接改变模型行为，其重要性不低于 system prompt，必须作为版本化协议设计和评估。

## 2. 先看结论
新设计不在 Grok 与 Codex 之间二选一：

1. **Read 采用 Grok 的结构化读取能力**，增加稳定 `FileSnapshot` 和统一结果信封；
2. **Edit 同时保留两条路径**：Codex 风格 `apply_patch` 负责多文件/结构化修改，Grok hashline/replace-range 负责小范围精确修改；
3. **所有写操作共享文件版本栅栏**，不允许基于过期读取静默覆盖；
4. **Exec 采用 Codex 的显式 process handle 和 head+tail 输出治理**，同时保留 Grok 的简单单次命令入口；
5. 默认命令是新进程，不把隐式持久 shell 当状态；需要交互或长任务时显式返回 process handle；
6. 每个结果都包含 machine-readable metadata 和 bounded model view，大结果进入 blob/artifact store；
7. Tool 契约按版本冻结到 `CapabilityDescriptor.capability_revision`，变更 description/output 也要进入 Eval。

## 3. Grok Build 当前 Tool 本体
### 3.1 Read 是独立的结构化 Tool
Grok `ReadFile` 支持 path、offset、limit，以及 PDF pages/format。文本默认返回带行号的内容，并设置行数和 token 上限；还处理图片、PDF、PPTX、notebook 等类型。实现见 [read_file/mod.rs](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build/read_file/mod.rs)。

普通输出示意：

```latex
1→fn main() {
println!("hello");
}
```

当前实现为了降低视觉噪声，并非每行都重复行号，而是在首行和周期边界标注。

### 3.2 Hashline Read/Edit 提供新鲜度锚点
Grok 还有 `hashline_read`，每行返回由行内容和上下文生成的锚点：

```latex
22:abc:rst→  let value = compute();
```

`hashline_edit` 使用锚点定位，执行前验证锚点仍匹配读取时的内容，检测重叠 edit，自底向上应用，并返回新锚点。实现见：

+ [hashline read](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build_hashline/read_file.rs)
+ [hashline edit apply](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build_hashline/edit/apply.rs)

它解决的是“模型看到第 22 行以后，外部编辑让第 22 行已经不是原内容”的问题。

### 3.3 Edit 实际上有多种实验路径
Grok 仓库不是只有 string replace，而是同时存在：

+ search/replace；
+ hashline edit；
+ write；
+ OpenCode 风格 edit；
+ Codex 风格 `apply_patch`；
+ 不同 contract version 的 read/edit。

Codex patch parser 的 Grok 移植实现见 [codex/apply_patch](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-tools/src/implementations/codex/apply_patch)。

这说明 Grok 已经在探索“精确局部编辑”和“结构化多文件 patch”的不同成功率，而不是形成了唯一标准编辑原语。

### 3.4 Bash 提供一次性与后台语义
Grok Bash input 包含 command、timeout、description 和 `is_background`。Terminal 层负责启动、输出和任务管理。实现见：

+ [bash/mod.rs](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-tools/src/implementations/grok_build/bash/mod.rs)
+ [terminal.rs](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-tools/src/computer/local/terminal.rs)

这种入口对模型简单，但“后台”如果只由 shell `&` 或布尔字段表达，容易让进程生命周期、取消和结果读取分成多套语义。

### 3.5 Grok 的优势
+ 独立 Read Tool 对模型友好，不必反复构造 `sed`/`cat`；
+ 多模态和 Office/PDF 类型处理较丰富；
+ hashline 把外部修改检测变成模型可使用的编辑协议；
+ 多套 Edit 已经提供真实比较基础；
+ Tool 使用类型化 input/output 和 SharedResources，测试与替换方便。

### 3.6 Grok 的不足
1. 多套 Edit 的选择策略、输出规范和默认暴露没有统一结论。
2. hashline 适合局部编辑，但不天然覆盖多文件 add/delete/move。
3. Read 的行号格式、原始内容、hash/mtime 和截断元数据没有统一成跨 Tool 的 `FileSnapshot`。
4. Bash 后台、Monitor、任务输出和取消可能形成平行生命周期。
5. 不同 contract version 和 Tool 实现增加行为矩阵，需要 Eval 而非只靠兼容逻辑维持。

## 4. Codex 当前 Tool 本体
### 4.1 Apply Patch 是核心文件编辑原语
Codex `apply_patch` 使用专用 patch 格式，支持：

+ `Add File`；
+ `Update File`；
+ `Delete File`；
+ `Move to`；
+ 多文件和多 hunk；
+ context 定位与有限 fuzzy matching；
+ streaming patch parser。

实现见：

+ [apply-patch parser](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/apply-patch/src/parser.rs)
+ [streaming_parser.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/apply-patch/src/streaming_parser.rs)
+ [Tool handler](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/core/src/tools/handlers/apply_patch.rs)

Shell 中出现 `apply_patch` 调用时，Codex 还能拦截并转入内置 patch runtime，使权限、diff tracker 和结果格式保持一致。

### 4.2 没有把通用 ReadFile 作为默认核心依赖
当前 Codex core 有 `view_image`，普通文本探索大量依赖 `exec_command` 下的 `rg`、`sed`、`git` 等命令，而不是像 Grok 那样提供一个能力丰富的统一 ReadFile。

优点是复用成熟 CLI，模型能组合高效搜索；缺点是：

+ 简单读取也经过 shell 解析、权限和输出截断；
+ 行号与截断格式随命令变化；
+ 文件版本没有作为结构化 read result 返回；
+ Windows/remote environment 的命令差异更明显。

### 4.3 Unified Exec 有显式进程管理
Codex `exec_command` 分配 process id，短命令等待完成，超出 yield 时间的进程可以通过 `write_stdin` 继续交互/轮询。`UnifiedExecProcessManager` 管理进程、后台 watcher 和释放。实现见：

+ [exec_command.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/core/src/tools/handlers/unified_exec/exec_command.rs)
+ [process_manager.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/core/src/unified_exec/process_manager.rs)

它不是把所有命令放进一个永不结束的共享 shell；持久性通过显式 process handle 和 shell snapshot 分别表达。

### 4.4 输出保留 head + tail
Codex 的 `HeadTailBuffer` 在输出过大时保留稳定头部和尾部，丢弃中间并插入 omission marker。实现见 [head_tail_buffer.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/core/src/unified_exec/head_tail_buffer.rs)。

这比单纯保留前 N 字节更适合编译/测试：错误摘要常在尾部，启动上下文常在头部。

Tool output 最终统一转换为 Responses item，并对文字、图片、MCP output、telemetry preview 和 truncation 分别治理，见 [tools/context.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/core/src/tools/context.rs)。

### 4.5 Codex 的优势
+ `apply_patch` 多文件能力强，模型训练/使用经验丰富；
+ streaming parser 可尽早检查 patch 结构；
+ Unified Exec 的 process handle、stdin、yield 和取消边界清楚；
+ head+tail 输出保留对真实命令更实用；
+ Tool output 与 transcript、Hook、diff tracker、remote environment 深度集成。

### 4.6 Codex 的不足
1. 文本读取依赖 shell，缺少统一 `FileSnapshot` 和编辑基线。
2. apply_patch 的 context/fuzzy matching 能容忍小偏差，也可能让“基于哪个版本编辑”不够显式。
3. 多文件 patch 失败时，错误定位和部分应用语义必须非常谨慎。
4. shell 能力强，但也让权限分类、跨平台差异和输出治理成为主负载。
5. shell snapshot 与 process session 是两个概念，对不熟悉源码的实现者容易混淆。

## 5. 两者对比
| 维度 | Grok Build | Codex |
| --- | --- | --- |
| 文本读取 | 独立 ReadFile，offset/limit/多类型 | 通常通过 exec + CLI |
| 图片读取 | ReadFile 多模态路径 | `view_image` 独立 Tool |
| 小范围编辑 | search-replace、hashline edit | 主要 apply_patch |
| 多文件编辑 | 已集成 Codex apply_patch 路径 | apply_patch 主路径成熟 |
| 新鲜度 | hashline anchor 验证 | patch context/运行时检查，统一 snapshot 不突出 |
| 命令执行 | Bash + background/monitor | Unified Exec + process handle/write_stdin |
| Shell 状态 | 工具/terminal 管理 | 新进程 + shell snapshot；长进程显式 handle |
| 大输出 | 有 truncation 配置 | head+tail + omitted bytes + token truncation |
| 远程环境 | 资源层可替换 | environment abstraction 集成较深 |


## 6. V2 设计目标
1. Read/Edit 共享稳定文件版本协议。
2. 小编辑和多文件编辑各用适合的原语，不强行统一成一个超大 Schema。
3. patch 在写入前完成全量 prepare，默认原子提交。
4. Exec 的进程、stdin、cancel 和 background 使用一个生命周期模型。
5. output 同时服务模型、UI、审计和恢复，不让一段字符串承担全部责任。
6. Tool description/input/output 版本化，并通过离线 Eval 决定默认曝光。
7. 所有路径解析、symlink、权限和沙箱语义与外置 Sandbox V2 对齐。

## 7. 统一 Tool 契约
```latex
BuiltInToolContract {
  capability_id
  contract_version
  input_schema
  output_schema
  side_effect_class
  concurrency_class
  resource_resolver
  policy_projection
  sandbox_requirement
  result_budget
  model_description_hash
}
```

执行分两阶段：

```latex
prepare(input, snapshot)
  -> validated input
  -> resolved resources
  -> version fences
  -> planned effects
  -> policy facts

execute(prepared, operation_context)
  -> side effects
  -> structured evidence
  -> bounded model result
  -> artifact/blob refs
```

`prepare` 不产生外部副作用。审批和资源锁绑定 prepare 后的对象，执行阶段不能重新解析模型原始字符串得到另一组目标。

## 8. 统一结果信封
```latex
ToolResultEnvelope {
  tool_call_id
  operation_id
  capability_id
  contract_version
  status
  summary
  model_content[]
  structured_data
  artifacts[]
  changed_resources[]
  truncation
  wall_time
  retryability
  diagnostics[]
}
```

其中：

+ `summary` 是短、稳定、面向模型的结论；
+ `model_content` 可以包含文本或图片；
+ `structured_data` 给 UI、Hook 和恢复器使用；
+ `artifacts` 指向完整输出、diff、二进制或日志；
+ `truncation` 明确原始大小、保留策略和 omitted 数量；
+ `changed_resources` 是副作用事实，不靠模型从文本猜。

大结果先完整写入 blob，再生成 bounded model view；不能只保留截断文本后丢掉证据。

## 9. Read V2
### 9.1 输入
```latex
ReadInput {
  path
  range = Lines(start, count) | Bytes(start, count) | Pages(range) | Whole
  render = Text | Image | Metadata | Auto
  max_output_tokens?
  expected_type?
  environment_id?
}
```

V1 对普通文本默认 `Lines(1, 1000)`，同时受 token budget 限制。`Whole` 不是无限读取，只表示由类型 handler 选择安全上限。

### 9.2 FileSnapshot
```latex
FileSnapshot {
  canonical_resource_id
  display_path
  file_type
  size
  mtime
  content_hash
  range_hash
  line_ending
  encoding
  read_at
  environment_id
}
```

`content_hash` 对小文件可直接计算；大文件至少保存 stat identity + selected range hash，并明确 confidence。后续 Edit 绑定 `canonical_resource_id + expected_version`。

### 9.3 输出格式
普通文本采用每行显式编号的稳定格式：

```latex
L1: fn main() {
L2:     println!("hello");
L3: }
```

不继续使用“只有首行和每十行显示编号”的压缩格式作为 V2 canonical output。原因是精确引用、错误定位和 Eval 更容易；如果 token 成本过高，可以由 UI 渲染隐藏重复前缀，但模型收到的协议保持稳定。

Hashline 是可选投影：

```latex
L22@abc.rst:     let value = compute();
```

它不替代 file content hash，而是给局部编辑提供行级锚点。

### 9.4 文件类型路由
| 类型 | 默认行为 |
| --- | --- |
| UTF 文本 | 解码、行号、范围和 hash |
| 未知编码文本 | 检测后返回 encoding；不可靠时要求 bytes/metadata |
| 二进制 | metadata + MIME + 小型 hex preview，不把乱码塞给模型 |
| 图片 | 结构化 metadata + image content item |
| PDF/Office | extractor adapter；原文件 hash 与提取器版本进入结果 |
| notebook | cell-aware 输出，不伪装成普通连续文本 |
| 超大文件 | range required 或返回可读区间建议 |


文档提取失败是 `unsupported_or_extractor_failed`，不能返回空字符串假装文件为空。

### 9.5 Read 的并发语义
Read prepare 解析路径并获取 read lease；读取完成后生成 snapshot。若文件在 stat-read-stat 之间变化，重试一次；持续变化则返回 `unstable_file`。Read 不持有跨 Step 锁，Edit 依靠版本 fence 检测过期。

## 10. Edit V2：双原语而不是二选一
### 10.1 `edit_range` / hashline edit
适合：

+ 单文件少量位置；
+ 模型刚读取过目标行；
+ 需要明确 stale detection；
+ 替换、插入或删除小段内容。

```latex
EditRangeInput {
  path
  expected_file_version
  edits[] {
    anchor_or_range
    expected_old_text?
    replacement
  }
}
```

每个 edit 同时校验 file version 和 anchor/old text。多个 edit 检测重叠后按底部到顶部应用。

### 10.2 `apply_patch`
适合：

+ 多文件联动；
+ add/delete/move；
+ 多个相隔较远的 hunk；
+ 模型以 diff 思维表达完整变更。

V2 保留 Codex patch grammar，但在 parse 后生成结构化 `PatchPlan`：

```latex
PatchPlan {
  files[] {
    source_resource_id?
    target_resource_id
    operation
    expected_version?
    hunks[]
    before_hash?
    after_hash
  }
  plan_hash
}
```

模型可以不显式传每个 before hash，但 Runtime prepare 时必须读取所有目标、解析 hunk，并把实际 before hash 固化到 PreparedCall。审批后任何目标变化都会使旧 plan stale。

### 10.3 原子性
默认流程：

```latex
parse all
 -> resolve all paths
 -> policy/sandbox check all targets
 -> acquire deterministic path locks
 -> validate all versions/hunks
 -> write temp files
 -> fsync where required
 -> atomic rename/metadata operations
 -> emit changed-files manifest
```

跨文件系统 move 无法提供同等级原子性时，prepare 必须标记 `atomicity=best_effort` 并触发更高审批或拒绝。Patch 中任一 hunk 验证失败，默认一个文件都不写。

### 10.4 为什么保留两个 Tool
把两者塞进一个 Schema 会让模型难以选择并增加参数错误。V2 暴露两个清晰工具：

| Tool | 优先场景 | 核心安全机制 |
| --- | --- | --- |
| `edit_range` | 小范围、刚读过 | snapshot + anchor |
| `apply_patch` | 多文件、结构化 diff | plan + per-file version fence |


哪个进入 Core Direct 由 model family Eval 决定。默认两者都可用，但 description 明确分工；如果 Tool schema 预算不足，保留 `apply_patch` Direct，`edit_range` 可按模型能力配置为 Direct/Deferred。

### 10.5 Edit 结果
```latex
EditResult {
  changed_files[]
  before_after_hashes[]
  line_change_summary
  diff_ref
  fresh_snapshots[]
  atomicity
}
```

模型拿到 fresh snapshot/anchor，后续继续编辑不必立刻全量重读；但跨 Step 的外部变化仍由 version fence 检测。

## 11. Exec V2
### 11.1 默认一次命令一个进程树
```latex
ExecInput {
  argv_or_script
  shell_mode
  cwd
  env_delta
  timeout
  yield_time
  tty
  output_budget
  environment_id
}
```

默认：

+ 明确 argv 的命令优先不用 `sh -c`；
+ 需要管道、重定向或 compound command 时显式 `shell_mode=script`；
+ cwd 从 Turn environment 解析，不继承上一次命令隐式 `cd`；
+ env 使用结构化 delta，不把 `export` 作为永久状态；
+ 每次调用创建独立 process tree 和 OperationId。

这与 Permission Policy 的结构化 segment 判定相配合。

### 11.2 长进程和交互进程
命令在 `yield_time` 内未结束时返回：

```latex
ProcessHandle {
  process_id
  operation_id
  environment_id
  created_at
  state
  stdin_open
  tty
  lease_expires_at
}
```

后续使用统一 `process_io`：

```latex
process_io(process_id, stdin?, poll_timeout?, signal?)
```

不要再用 shell `&` 作为 Runtime 认可的后台协议。模型写 `cmd &` 时，policy classifier 应识别并默认 ask/规范化；可靠后台任务必须由 process manager 持有 handle。

### 11.3 Shell snapshot 的边界
可以采用 Codex 的 shell snapshot 恢复 aliases/functions/env，用于新进程的启动环境，但必须区分：

+ **ShellSnapshot**：可版本化的启动环境；
+ **ProcessHandle**：仍在运行的具体进程；
+ **cwd/env_delta**：本次调用显式参数。

snapshot 刷新只在显式边界发生，不能因为某个命令执行了 `cd/export` 就隐式改变所有后续 Tool Call。

### 11.4 取消
```latex
cancel OperationId
  -> close pending stdin
  -> SIGINT / platform graceful signal
  -> grace period
  -> kill process tree
  -> drain bounded output
  -> persist terminal evidence
```

取消完成的定义是 Runtime 已经观察到进程树终态，而不是“已经发送信号”。Supervisor 重启后的重关联和 UnknownOutcome 规则继承 [外置 Sandbox V2 §18](./13-external-sandbox-runtime-v2-design.md#18-journal恢复与取消)。

### 11.5 输出治理
V2 采用 Codex 的 head+tail 思路，并增加完整 blob：

```latex
ExecOutput {
  exit_code?
  signal?
  wall_time
  process_id?
  retained_head
  retained_tail
  omitted_bytes
  original_token_estimate
  full_output_ref?
  cwd
  status
}
```

规则：

1. stdout/stderr 的时间顺序尽可能保留；需要分流时提供 channel tag；
2. 模型 view 使用 head+tail，不只保留头部；
3. 完整输出达到 artifact 阈值时写 blob；
4. binary output 不做 UTF-8 lossy 后冒充完整文本；
5. timeout、cancel、non-zero exit 是不同 status；
6. exit code 非零仍是正常 Tool Result，不等于 Runtime internal error。

## 12. 文件新鲜度与外部并发
### 12.1 Version fence
```latex
FileVersion {
  resource_id
  content_hash
  metadata_identity
  observed_at
}
```

Edit prepare 记录 `expected_version`，execute 在持锁后重新读取/校验。变化后返回：

```latex
stale_file {
  expected_hash
  actual_hash
  changed_since
  suggested_action = reread_and_resample
}
```

不能自动把旧 patch fuzzy 应用到新内容后宣称成功。允许 fuzzy match 的 patch 也必须在 prepare 阶段固化实际匹配位置和 before hash，审批后不得重新 fuzzy 搜索。

### 12.2 锁
+ 结构化 Read 使用共享路径 lease；
+ Edit/Patch 按 canonical resource id 排序获取写锁；
+ Exec 并发等级由 Sandbox profile 决定，而不是尝试预测命令会写哪些文件；
+ workspace-write Exec 获取 workspace 写 lease；
+ read-only Sandbox Exec 可并发运行；
+ 外部编辑绕过 Runtime 锁时由 version fence 兜底。

## 13. 路径、安全和 Sandbox
所有 File Tool 必须：

1. 相对路径基于绑定的 environment cwd 解析；
2. lexical normalize 后再由平台层 canonicalize；
3. 校验 symlink、mount 和最终 parent；
4. policy 匹配使用 resolved resource，同时保留 display path；
5. 审批绑定 resolved target/hash；
6. 写前在 Sandbox/Supervisor 侧重新检查；
7. 不信任模型提供的 content type 或 `expected_type`。

Level 1 接入时，现有 Agent 的 File Tool 仍可进程内写 workspace；Level 2 才把 prepare plan 发送 File Broker 执行。这个代价和边界继承 [外置 Sandbox V2 §9](./13-external-sandbox-runtime-v2-design.md#9-三种接入等级)。

## 14. Tool Description 和输出也要版本化
`contract_version` 至少覆盖：

+ Tool name/description；
+ input/output schema；
+ 行号和 patch grammar；
+ default limits；
+ truncation marker；
+ exit/timeout/error 文案的结构化 reason code；
+ side-effect 和 concurrency metadata。

只改 description 也可能改变模型选择率，因此：

+ 当前 Turn 继续使用冻结的 descriptor；
+ 新版默认下一 Turn 生效；
+ Eval 样本记录 contract version；
+ A/B 结果按模型 family 分开，不假设一个 Tool 契约适合所有模型。

## 15. 与 Capability、Loop 和 Context 的接口
```latex
CapabilityDescriptor
  -> BuiltInToolContract revision
  -> PreparedCapabilityCall
       -> PreparedRead / PatchPlan / PreparedExec
  -> PolicyBinding
  -> SandboxBinding
  -> Operation journal
  -> ToolResultEnvelope
  -> deterministic CommitSequence
```

+ Tool schema 由 Capability V2 冻结；
+ prepare/execute/commit 由 Loop V2 排序；
+ 大结果和 blob 由 Context V2 治理；
+ command/path 决策由 Permission Policy Language 产生；
+ 实际进程/文件边界由 Sandbox V2 强制。

## 16. 相对 Grok Build 的收益
| Grok 当前问题 | V2 改动 | 收益 |
| --- | --- | --- |
| 多套 Edit 缺少明确分工 | `edit_range` + `apply_patch` 双原语 | 小改和多文件各走成功率更高的路径 |
| Read/Edit 版本信息分散 | 统一 FileSnapshot/FileVersion | 外部修改可稳定检测 |
| hashline 不能覆盖多文件 | patch plan 加 per-file fence | 保留锚点优点并补齐 add/delete/move |
| Bash background/monitor 语义分散 | ProcessHandle + process_io | 取消、恢复和结果读取统一 |
| 截断结果不一定保留完整证据 | bounded view + blob ref | 模型 token 可控且可定点读取原文 |
| contract 版本多但缺统一 Eval 键 | contract revision 进入 trace | 可比较不同 Read/Edit 契约真实成功率 |


保留的 Grok 强项包括结构化 Read、多模态、hashline、类型化 Tool 和 SharedResources。

## 17. 相对 Codex 的收益
| Codex 当前问题 | V2 改动 | 收益 |
| --- | --- | --- |
| 文本读取主要依赖 shell | 新增结构化 Read/FileSnapshot | 降低简单读取成本并建立编辑基线 |
| apply_patch context 可能基于旧内容 | prepare 固化 before hash/version | 审批后不重新模糊定位 |
| 单一 patch 不总适合小编辑 | 增加 hashline edit_range | 小修改参数更短、stale 更明确 |
| shell 输出进入多层截断 | 统一 envelope + full blob | UI、模型、恢复共享同一事实结果 |
| process 与 shell 环境概念复杂 | 明确 Snapshot/Handle/cwd 三分 | 行为更容易理解和测试 |


保留的 Codex 强项包括 apply_patch、Unified Exec、write_stdin、head+tail 和进程树取消。

## 18. 关键决策的收益闭环
| 设计决策 | 为什么这样选 | 直接收益 | 验证指标 |
| --- | --- | --- | --- |
| 结构化 Read + FileSnapshot | shell read 缺少稳定版本事实 | Read/Edit 可闭环 | stale overwrite 数应为 0 |
| 两种 Edit Tool | 小编辑和多文件 patch 的最优表达不同 | 提升首次编辑成功率 | first-attempt edit success |
| patch prepare 全量固化 | 审批后重新定位会改变实际操作 | 审批对象等于执行对象 | plan hash mismatch 拒绝数 |
| 默认新进程，长进程显式 handle | 隐式共享 shell 难恢复和取消 | 生命周期确定 | orphan process 数 |
| head+tail + blob | 错误常在尾部，完整证据又不能常驻上下文 | token 与可追溯性兼得 | useful-tail retention、artifact get 率 |
| contract version 进 trace | 描述和格式会影响模型行为 | Tool 改动可回放比较 | 各版本 task success/token |
| Sandbox profile 决定 Exec 并发 | Bash 静态写集合不可知 | 并发判定有内核保证 | 并发写冲突率 |


## 19. 分阶段实现
### Phase 1：结果信封与 Read/FileVersion
1. 定义 BuiltInToolContract 和 ToolResultEnvelope；
2. 迁移 Grok Read 为 canonical Read V2；
3. 输出 FileSnapshot、截断和类型 metadata；
4. 大结果写 blob；
5. 建立 golden output fixtures。

### Phase 2：双 Edit 与原子 PatchPlan
1. 规范 `edit_range`；
2. 复用 Codex apply_patch grammar；
3. 增加 per-file version fence；
4. prepare 全量校验，execute 原子提交；
5. changed-files manifest 和 diff ref；
6. 建立编辑成功率 Eval。

### Phase 3：统一 Exec
1. ProcessManager、ProcessHandle 和 process_io；
2. head+tail + complete output blob；
3. OperationId cancel tree；
4. shell snapshot 与 env/cwd 显式分离；
5. 对接外置 Sandbox Exec Broker。

### Phase 4：远程环境与按模型优化
1. environment-neutral resource id；
2. remote File/Exec runtime；
3. 根据 Eval 调整 Direct Tool 组合和 description；
4. 不在无数据时自动选择编辑器或自由加权。

## 20. 测试和验收
### 20.1 Read
+ CRLF、无末尾换行、超长行、非法 UTF-8、空文件；
+ offset/limit/token limit 的边界；
+ 读取中途文件变化返回 stable 或 unstable，不混合两个版本；
+ binary/image/PDF/Office failure 不伪装为空内容；
+ 相同 snapshot 得到确定性输出。

### 20.2 Edit
+ anchor 过期、old text 不匹配、外部编辑；
+ 多 hunk 重叠；
+ add/delete/move 和跨文件系统 move；
+ 任一 hunk 失败默认零文件写入；
+ 审批后目标变化使 plan stale；
+ 崩溃点覆盖 temp write、fsync、rename 和 journal commit。

### 20.3 Exec
+ timeout、cancel、SIGINT 无效后 kill tree；
+ stdout/stderr 大量交错；
+ process handle 过期、重复 poll、stdin closed；
+ Supervisor 重启重关联；
+ head/tail omission 字节数正确；
+ read-only 与 workspace-write Sandbox 的并发等级正确。

### 20.4 Eval 指标
+ 首次 Read 后定位正确率；
+ first-attempt edit success；
+ stale edit 正确拒绝率；
+ 完成任务的 Tool Call 数和 token；
+ patch 平均修复轮次；
+ Exec orphan/unknown outcome 率；
+ 大输出后模型找到关键错误的比例。

### 20.5 验收标准
1. 任一文件写调用都能反查它基于的 FileVersion 或明确标记 blind-create。
2. 外部修改发生后，旧 edit/patch 不会静默覆盖新内容。
3. 多文件 Patch prepare 失败时无部分写入；无法原子保证时事前明确降级。
4. 后台/长进程都有 ProcessHandle，Runtime 不把裸 `&` 当可靠任务管理。
5. cancel 返回前进程树已终止或结果明确标为 UnknownOutcome。
6. Tool model output 的截断都有机器可读 metadata 和完整 artifact 获取路径。
7. Tool contract 变化进入 capability revision、PromptSnapshot 和 Eval 样本。
8. Read/Edit/Exec 的错误都有稳定 reason code，不要求模型从自然语言猜失败类型。
9. 权限审批绑定 prepare 后的 resolved resources 和 plan hash，不绑定未解析原始字符串。

## 21. 最终判断
Grok 的 Tool 本体强在“给模型直接可用的结构化文件能力”，Codex 强在“patch 与进程执行的工程化深度”。融合后应形成：

```latex
Grok Read + Hashline
        |
        v
统一 FileSnapshot / Version Fence
        |
        +--> edit_range
        +--> Codex apply_patch + PatchPlan

Grok 简单 Bash 入口
        +
Codex Unified Exec / head+tail
        |
        v
统一 ProcessHandle / Operation journal
```

这比简单选择一家的 Tool 集合更有价值：它把模型编辑成功率、外部并发安全、进程恢复和上下文预算放进同一套可测试协议。

## 22. 性能优化（T1–T10）

以下优化消除 Tool 执行路径中的多次全量扫描、内存复制和重复哈希，同时增加超时和耗时追踪。

### T1b/T4：read_file 字节范围零分配映射
旧实现在 `ReadRange::Bytes` 分支调用 `all_lines.join("\n")` 仅为取总长度，这会分配整文件副本。新实现用前缀和公式零分配得到等价总长（`Σ line.len() + (行数-1) 个 '\n'`），再单遍扫描把字节偏移映射到行索引。O(n) 时间、O(1) 额外空间。

### T8：apply_batch_edits 单遍正向拼接
旧实现 `content.to_string()` 整文件复制后对每个编辑调用 `replace_range`，每次因字节平移为 O(n)，k 个编辑共 O(k·n)。新实现先收集所有 match 位置并排序验证不重叠，然后单遍正向拼接：`result.push_str(&content[cursor..m.start]); result.push_str(&m.replacement); cursor = m.end;`。时间复杂度降为 O(n + Σ replacement)，无整文件复制、无逐次平移。

### T9：build_fresh_snapshot 接收预计算哈希
旧实现在 edit/write 操作后对新内容哈希 2 次（`fresh_snapshot` 一次 + `content_hash_after` 一次）。新实现让 `build_fresh_snapshot` 接收调用方已算好的 `content_hash` 与 `size`，不再重复哈希整文件内容。

### T6：write_file 大内容预算 + Artifact 追加模式
- `WRITE_HARD_CAP_BYTES = 16MB`：单次写入不得超过此大小，防止一次性写入超大文件撑爆内存与磁盘。
- `append = true` 追加模式：用于分块构建大文件（每块 ≤ 16MB），直接 append 新字节（不重写整文件），fsync 持久化。
- `stream_hash_file`：追加模式后用 BufReader 逐块（64KB）喂 SHA-256 计算整文件哈希，避免把整个大文件读进 String。

### T5：Tool Result offload 时机提前
旧实现在 receiver 循环中收到完整结果后才 offload 大结果到 blob store / temp file，大 payload 先经过 channel 再被替换。新实现在 `execute_single_tool` 内 `rt.execute()` 返回后立即调用 `early_offload_tool_result`，在结果进入 channel 之前就替换为预览 + 路径引用。并行工具各自独立 offload，不再排队等待 receiver 顺序处理。

### T10：Tool 执行超时 + 分阶段耗时
- `tool_timeout_secs` 配置（默认 0 = 不限）：用 `tokio::time::timeout` 包装 `rt.execute()`，超时时取消 tool future（async 工具直接 drop），返回 error 结果防止单个卡死工具阻塞整个 turn。
- `duration_ms` 追踪：在 call site 用 `Instant::now` 测量 `execute_single_tool` 的完整墙钟时间（含权限检查、沙箱验证、审批流程），写入 `ToolExecutionFinished` journal 事件的 `duration_ms` 字段，旧值为 `None`（"measured later" 但从未测量）。
