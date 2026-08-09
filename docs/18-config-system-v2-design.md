# 配置系统与多模型路由：Grok、Codex 对比及熔断降级 V2 设计
## 1. 目标
配置系统不只是读取 TOML。它必须回答：配置来自哪里、谁覆盖谁、workspace 是否可信、热更新何时生效、坏配置如何隔离、运行中 Session 使用哪一代，以及安全策略不可用时能否继续。

本文选择 TOML 作为用户可编辑格式，采用“分层值 + 独立约束 + 原子 generation 发布”。本文所说的主要“熔断与降级”是**模型请求的运行时路由**：用户按优先级配置多个 Provider/Model 候选，高优先级模型源暂时不可用时跳过或熔断该候选，请求转到下一候选；恢复后通过 half-open 探测重新启用高优先级候选。

配置文件自身仍有 last-known-good 保护，但它只解决坏 TOML/坏候选配置不能替换当前有效 generation，不等同于模型能力降级。

## 2. Grok 当前实现
Grok 已有类型化 config、用户/项目/managed 配置、watcher/reloader、远端 team managed config 同步和大量 resolve 函数。配置加载后并非直接散布原始 TOML，而是解析为 Workspace、Permission、MCP、Memory、UI 等类型。

它还具备值得保留的可靠性特征：

+ watcher 隔离更新；
+ managed config 原子写和跨进程锁；
+ session-start、login、background 使用不同 retry budget；
+ managed policy 支持 fail-closed；
+ auth/config 读取短暂失败时不会立即删除既有 managed policy；
+ 项目 trust 与 hook/skill 等高风险配置关联。

入口可见 [config/mod.rs](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-shell/src/config/mod.rs:1)、[reloader.rs](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-shell/src/config/reloader.rs:1) 和 [managed_config.rs](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-shell/src/managed_config.rs:1)。

不足：

+ 配置来源、最终 origin 和 generation 没有像 Codex 一样形成统一公共模型；
+ 不同子系统自行 resolve/reload，容易出现部分刷新时序差异；
+ watcher 反复看到同一个坏文件时可能持续产生诊断噪声；
+ “保留旧配置”“禁用子系统”“拒绝启动”缺少统一降级矩阵。

## 3. Codex 当前实现
Codex 的 `codex-config` 将配置表示为 `ConfigLayerStack`，能输出 effective TOML、逐 key origin、每层 fingerprint 和 disabled reason，见 [loader README](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/config/src/loader/README.md:1)。

当前 layer 包括 System、EnterpriseManaged、User、Profile、Project、SessionFlags、MDM/legacy managed。层按稳定 precedence 合并，递归 table merge，并对部分特殊结构定义替换/规范化语义，见 [config_layer_source.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/config/src/config_layer_source.rs:5) 和 [merge.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/config/src/merge.rs:31)。

Codex 热更新对 malformed 配置保留旧有效配置，测试覆盖 user config reload；它还将 managed requirements 与普通 values 分开，不允许普通高优先级值绕过企业约束。

不足：

+ reload 仍以具体子系统更新为主，缺少统一 candidate generation 发布状态机；
+ 配置失败的 circuit breaker 和 half-open 恢复不是顶层契约；
+ 部分错误保留旧值、部分警告继续，产品层难解释当前到底降级了什么；
+ Grok 式远端 managed 同步、产品 UI 状态和 workspace trust 联动更集中。

## 4. V2 的两个平面
### 4.1 Values Plane
表达用户选择，例如模型、UI、Memory 参数和 MCP server：

```latex
Built-in defaults
  < System defaults
  < Enterprise defaults
  < User config/profile
  < Trusted workspace config
  < Session overrides
```

高层覆盖低层，但每个最终 key 保留 origin、source version 和 merge trace。

### 4.2 Requirements Plane
表达不可被用户放宽的约束：

+ managed deny/allow ceiling；
+ forced provider/region；
+ required sandbox；
+ disabled feature；
+ allowed MCP/Plugin source；
+ credential storage policy；
+ retention/compliance requirement。

Requirements 不参与普通“谁最后写谁赢”。最终结果是：

```latex
EffectiveConfig = merge(ValueLayers) constrained_by Requirements
```

冲突时输出明确诊断；不能偷偷把 user 值裁掉而不告诉 UI。

## 5. 文件布局与作用域
```latex
/etc/agent/config.toml                 # system values
managed service / MDM                  # requirements + managed defaults
~/.agent/config.toml                   # user
~/.agent/profiles/<name>.toml          # user profile
<workspace>/.agent/config.toml         # trusted workspace
CLI/session flags                      # current session
```

兼容期读取 Grok/Codex 原路径，但转换为内部 layer，不在运行时同时维护两套语义。新格式统一为 TOML，是因为两边已有成熟 TOML 类型、诊断和迁移基础；JSON/YAML 只用于远端传输，进入管理面前转为 canonical config model。

## 6. Workspace Trust
未信任 workspace 的 `.agent/config.toml` 不进入 effective config，只以 quarantined layer 展示。首次信任绑定：

```latex
canonical workspace identity
+ repository remote fingerprint
+ config content hash
+ requested high-risk capabilities summary
```

普通无风险字段变化可以重新验证后热更新；新增 hook、MCP executable、Skill、网络域、workspace 外路径等高风险配置使 trust 进入 `review_required`。仓库内容永远不能修改 Global UserPreference 或 managed requirements。

## 7. 类型化 Schema 与迁移
每个配置文件必须声明或推断 `schema_version`。加载管线：

```latex
read bytes
  -> parse TOML
  -> normalize aliases
  -> migrate old schema in memory
  -> typed deserialize
  -> per-field validation
  -> cross-reference validation
  -> requirements constraint
  -> compile derived artifacts
```

迁移默认 dry-run，UI 展示 old key、new key 和行为差异。自动写回必须使用 temp file + fsync + atomic rename，并校验读取时 hash，避免覆盖用户并发编辑。

未知 key 默认 warning；在 managed/permission/sandbox/credential 区域使用 strict error。废弃 key 至少保留两个 minor release 的诊断窗口。

## 8. Generation 与采用边界
配置不是一个全局数字。使用 domain generation 避免 UI 小改动打爆 Tool/Prompt cache：

```latex
ConfigGeneration {
  root_generation
  prompt_generation
  capability_generation
  policy_generation
  sandbox_generation
  provider_generation
  memory_generation
  ui_generation
}
```

默认采用规则：

+ UI-only：立即生效；
+ 安全收紧/revocation：立即进入 live fence；
+ Policy 放宽：下一 Turn/新审批生效；
+ Tool/MCP/Prompt/Provider 普通变化：下一 Turn 捕获；
+ 正在执行的 Tool 使用其 Snapshot，但受实时 revocation 上界约束；
+ Session 可以 pin model/provider，不因 watcher 在 Turn 中途漂移。

## 9. 多 Provider/Model 有序路由
### 9.1 配置格式
候选顺序就是路由优先级，不再同时引入容易冲突的 `priority` 数字：

```toml
[model_routes.default]
strategy = "ordered_failover"
sticky_scope = "turn"
retry_higher_priority = "next_turn"
minimum_capabilities = ["tools"]

[[model_routes.default.candidates]]
id = "primary"
provider = "openai"
model = "gpt-5.6-sol"
account = "work-openai"
timeout_ms = 30000

[[model_routes.default.candidates]]
id = "secondary"
provider = "deepseek"
model = "deepseek-reasoner"
account = "work-deepseek"
timeout_ms = 45000

[[model_routes.default.candidates]]
id = "local-fallback"
provider = "local"
model = "qwen-coder"
optional_capability_degradation = ["reasoning", "parallel_tools"]
```

Route 可以按用途拆分，例如 `default`、`fast`、`compaction`、`memory_extract`、`subagent`。一次请求先选择 route，再严格按候选列表从前向后选择，不能由 HashMap 遍历顺序决定。

### 9.2 候选身份和独立熔断
```latex
ModelCandidateKey =
  provider_id + endpoint_revision + wire_backend
  + model_id/model_revision + account_id + region
```

每个候选独立维护 `Closed / Open / HalfOpen`。同一 Provider 下两个模型、同一模型的两个 endpoint 或两个账号不能共用一个模糊 breaker，否则一个局部故障会误伤全部路径。

选择流程：

```latex
按配置顺序遍历候选
  -> managed policy / workspace data boundary 是否允许
  -> breaker 是否 Open
  -> credential 是否可用
  -> 请求所需能力是否满足
  -> context 是否能装入
  -> 获取并发/rate-limit permit
  -> 创建 ModelBinding 并请求
```

Open 候选直接跳过；HalfOpen 只允许一个 probe，其他请求继续走低优先级候选，避免恢复瞬间形成探测风暴。

### 9.3 哪些失败触发降级
| 失败 | 同候选动作 | 是否进入下一候选 |
| --- | --- | --- |
| DNS/连接超时/连接拒绝 | 有界本地 retry 后计入 breaker | 是 |
| 408、429 | 尊重 Retry-After；超过当前请求预算 | 是 |
| 5xx、Provider overload | 有界 retry，计入 breaker | 是 |
| model unavailable/not found | 标记候选 unhealthy/config diagnostic | 是 |
| 401/token expired | 对该候选 single-flight refresh 一次 | 刷新仍失败时，仅配置允许才是 |
| 403/区域或组织策略拒绝 | 不盲目 refresh | 默认否；显式 route 可允许其他已批准账号/源 |
| context length | 先执行兼容 compaction | 只有下一候选窗口足够时是 |
| capability/schema 不支持 | 标记 candidate incompatible | 是，但不得丢失必需能力 |
| 400 参数错误/客户端 codec bug | 修复请求，不应掩盖 | 否 |
| content safety/refusal | 这是语义结果 | 否 |
| 用户 cancel/deadline | 终止请求 | 否 |


失败分类由 Provider Adapter 的 canonical error taxonomy 给出，Route Manager 不解析错误字符串。

### 9.4 语义提交栅栏
自动换源只允许发生在当前 attempt **尚未产生任何可提交语义事件**时：

```latex
连接/headers/空 keepalive
  -> 可以换源

首个 reasoning item、assistant content、refusal、完整/部分 Tool Call
  -> semantic_commit_fence crossed
  -> 不得透明换源
```

跨过栅栏后连接中断，本 attempt 标为 aborted/unknown；partial 内容可供 UI 展示但不透明拼接另一模型输出。否则两个模型可能各生成一个 Tool Call，造成重复副作用或上下文因果不一致。

### 9.5 能力降级契约
“降级到低优先级模型”允许模型更便宜、更慢或能力更弱，但必须区分必需能力与可降级能力：

```latex
RequestCapabilityRequirement {
  required: tools, input_modalities, response_schema, min_context
  optional: reasoning, parallel_tools, prompt_cache
  declared_degradation_transforms[]
}
```

+ 必需能力不满足时跳过候选；
+ 图片是用户任务必需输入时，不能静默删图后请求纯文本模型；
+ Tool Calling 必需时，不能降到仅文本模型并假装可继续 Agent Loop；
+ structured output 必需时，只有 Provider Adapter 有验证/修复协议才可降级；
+ reasoning effort、parallel tool call、prompt cache 等可按声明关闭；
+ context window 更小时先 compaction，仍装不下则跳过；
+ 每次发生能力收窄，记录 `ModelCapabilityDegraded` 并通知 UI。

### 9.6 Turn 粘滞与恢复高优先级
一次 Step 在首个语义事件前从 primary 降到 secondary 后：

+ 创建新的 ModelBinding、attempt id 和 Step generation；
+ 当前 Turn 后续 Step 默认粘在 secondary，避免每个 Tool round 都重新撞 primary；
+ Provider reasoning envelope 不跨不兼容候选传递；
+ 下一 Turn 重新从最高优先级开始，但 Open 候选仍跳过；
+ breaker cooldown 到期后由一个 half-open probe 尝试恢复；
+ probe 成功关闭 breaker，后续新 Turn 恢复使用高优先级候选；
+ 不在已经开始 streaming 的 Turn 中途为了“恢复主源”主动切回。

`sticky_scope` 可配置为 `step | turn | session`，默认 `turn`。`session` 适合需要 Provider conversation state 的接口，但会延迟恢复主源，必须显式选择。

### 9.7 预算与防风暴
Route 具有总预算，而不是每个候选各自用满 retry：

```latex
RouteAttemptBudget {
  max_candidates
  max_total_attempts
  max_elapsed
  max_auth_refreshes
  turn_deadline
}
```

例如三个候选每个重试三次会产生九次请求，必须由总预算截断。Sub-agent 继承更窄预算；后台任务不能通过多源降级无限消耗配额。

### 9.8 事件与可观测性
```latex
ModelRouteSelected
ModelCandidateAttemptStarted
ModelCandidateFailed
ModelCircuitOpened / HalfOpened / Closed
ModelFailoverSelected
ModelCapabilityDegraded
ModelResponseCommitted
```

事件记录 route、candidate id、binding、错误类别、breaker 状态、耗时和 usage，不记录 credential。UI 必须能展示“当前正在使用 secondary，因为 primary 429/Open”，不能只显示最终模型名。

## 10. 候选配置发布状态机
```latex
Detected
  -> Loading
  -> Parsed
  -> Validated
  -> Compiled
  -> ShadowChecked
  -> CandidateReady
  -> Published

任一步失败 -> Rejected -> Circuit accounting
```

只有 Published 才更新 generation。文件 mtime 变化不等于配置生效。发布前编译 Tool registry、Policy、Prompt manifest、Provider binding 和 Sandbox profile，确保不存在“配置已宣布成功，依赖对象随后构造失败”的窗口。

发布由单 writer 原子交换不可变 `Arc<ConfigSnapshot>`，随后发 `ConfigGenerationPublished`。各 Session 在约定边界采纳。

## 11. 配置发布熔断器
### 11.1 为什么需要
编辑器原子保存、半截 TOML、远端 managed 短暂返回坏 bundle、依赖文件丢失，都可能让 watcher 高频触发同一个失败。只做“解析失败保留旧配置”仍会持续消耗 CPU、刷日志和重复重连 MCP。

### 11.2 熔断键
```latex
BreakerKey = source_id + content_hash + affected_domain
```

同一坏 hash 已失败后，后续 watcher event 直接复用诊断，不重复 compile。内容 hash 改变可立即尝试新候选。

### 11.3 状态
```latex
Closed
  -> 连续/窗口内失败达到阈值 -> Open
Open
  -> cooldown 到期或内容变化 -> HalfOpen
HalfOpen
  -> 单次完整验证成功 -> Closed + publish
  -> 失败 -> Open，指数增加 cooldown
```

阈值、窗口和 cooldown 是运维参数，不写死在业务逻辑；默认值必须通过故障注入和实际 telemetry 调整。手工 `config validate --force` 可以触发一次 half-open probe，但不能跳过校验。

### 11.4 熔断的对象
熔断针对“候选发布/子系统重建”，不是阻止读取诊断，也不是让旧配置永久不更新。不同 domain 独立熔断：坏 UI theme 不应阻止 Policy 收紧；坏 MCP server 也不应阻止 Provider 更新。

## 12. 配置 Last-Known-Good 与子系统故障隔离
每个 domain 保存最近一次成功发布的 snapshot hash 和 generation：

| 失败域 | 默认降级 | 安全要求 |
| --- | --- | --- |
| UI | LKG 或内置主题 | 可继续 |
| Memory tuning | LKG；无 LKG 则 FTS-only/关闭自动检索 | 不影响权限 |
| 单个 MCP server | 隔离该 server，其他能力继续 | 不自动换到同名不可信 server |
| Skill/Hook | 隔离坏条目 | 未信任内容不执行 |
| Model route 配置 | LKG route；运行时按该 route 的有序候选降级 | 不采用半编译候选链 |
| Prompt | LKG；无 LKG 则内置 signed base prompt | 不加载半截项目指令 |
| User convenience policy | LKG 或更严格默认 | 不放宽 |
| Managed requirements | 保留最后验证版本 | 无 LKG 且 fail_closed 时拒绝高风险操作/启动 |
| Sandbox compiler | LKG profile；无法证明不更宽则拒绝执行 | 永不退到 no-sandbox |
| Credential storage policy | LKG 或最严格 backend | 不落明文兜底 |


这里的 LKG 是“配置发布失败的保护”，不是模型请求降级本身。模型请求的熔断与降级按 §9 执行。任何自动换源仍必须处于 managed policy、账号、区域和数据边界内。

## 13. 远端配置与缓存
managed bundle 使用签名、version、issued_at、expires_at 和 monotonic revision。流程：

```latex
fetch -> verify signature -> validate schema -> stage journal
      -> compile -> atomic publish -> update LKG
```

过期策略由 requirements 声明：

+ `retain_last_verified`：继续使用并告警；
+ `fail_closed_after_grace`：grace 后禁止受控能力；
+ 不允许自动删除 policy 并回到个人默认。

远端 fetch circuit breaker 与 config compile breaker 分开：网络失败不代表已有 bundle 无效，签名/Schema 失败也不应通过网络重试掩盖。

## 14. 诊断与 explain
UI/CLI 提供：

```latex
config list-layers
config explain <dotted.key>
config validate [path]
config diff --effective
config generations
config breaker status
config rollback <domain> <generation>
model route explain <route>
model route health [route]
```

`explain` 返回 effective value（敏感值只显示存在性）、winning source、被覆盖层、requirements constraint、采用边界和当前 breaker/LKG 状态。

## 15. 与其他 V2 的契约
+ Loop Snapshot 记录各 domain generation；
+ Capability Manager 只消费已发布 capability config；
+ Prompt assembler 只消费 PromptManifest；
+ Approval binding 使用 policy_generation；
+ Sandbox Operation 使用 sandbox_generation 和 revocation epoch；
+ Provider Adapter 执行错误归一、Compatibility Gate 和语义提交栅栏；Route Manager 只按有序候选和 breaker 选路；
+ Auth bootstrap 只读取极小 bootstrap config，避免 managed config 与登录循环依赖；route 中跨账号候选必须提前配置并受 policy 允许；
+ UI 通过 [ACP V2](./17-frontend-acp-protocol-v2-design.md) 接收 candidate rejected/published/degraded 事件。

## 16. 相对原实现的收益
### 相对 Grok
| 当前问题 | V2 | 收益 |
| --- | --- | --- |
| 子系统 resolve 分散 | 统一 candidate publish | 不再部分生效 |
| 来源解释不足 | per-key origin/fingerprint | 能回答“为什么是这个值” |
| 坏文件可反复触发 | content-hash breaker | 降低抖动、日志和重建成本 |
| 降级依赖各模块判断 | 显式 domain matrix | 故障行为可预测 |
| 单一模型源故障会终止请求 | 有序 ModelRoute + candidate breaker | 自动切换到低优先级模型源 |


保留 Grok 的 watcher、managed sync、原子文件更新、fail-closed 和 workspace trust 产品能力。

### 相对 Codex
| 当前问题 | V2 | 收益 |
| --- | --- | --- |
| layer 很强但发布边界分散 | generation state machine | Session 采用时机确定 |
| reload 失败主要保留旧值 | LKG + breaker + half-open | 可治理持续坏配置 |
| domain 降级不统一 | typed degradation matrix | 安全与可用性边界明确 |
| managed fetch 与本地 reload 分离 | 统一 staging/journal | 远端和本地配置一致恢复 |
| Provider failover 不是默认一等配置 | 有序候选、能力契约、Turn 粘滞 | 多模型源可用且行为确定 |


## 17. 关键决策收益闭环
| 决策 | 解决的问题 | 直接收益 | 指标 |
| --- | --- | --- | --- |
| Values/Requirements 分离 | 用户覆盖 managed 约束 | 管理策略不可绕过 | constraint violation 数 |
| TOML + typed schema | 两套格式迁移困难 | 可读、可迁移、两边复用 | migration 成功率 |
| 原子 generation 发布 | 半更新和语义漂移 | Snapshot 可重放 | partial publish 数应为零 |
| content-hash breaker | 同一坏配置重复重建 | 稳定、低噪声 | suppressed reload 次数 |
| domain LKG | 一个模块失败拖垮全局 | 局部降级 | 可用 Session 比例 |
| 安全单调降级 | 故障导致扩权 | fail-safe | 降级扩权事件应为零 |
| per-key provenance | 用户无法理解配置 | 可诊断 | explain 覆盖率 |
| 有序 ModelRoute | 主模型源故障直接中断 | 按用户顺序自动降级 | failover 成功率、可用率 |
| candidate 独立 breaker | 持续请求已坏源 | 快速跳过并自动探测恢复 | Open 时无效请求数、恢复时延 |
| semantic commit fence | 半流后换源产生重复输出/Tool | 保持一次采样因果唯一 | fence 后透明 failover 数应为零 |
| required/optional capability | 弱模型静默丢功能 | 能力降级可控可见 | 必需能力丢失数应为零 |


## 18. 实施与验收
Phase 1：canonical TOML schema、layer/origin/fingerprint、静态 validate 和迁移 dry-run。

Phase 2：candidate generation、原子发布、Session 采用边界、ModelRoute 和 UI diagnostics。

Phase 3：candidate breaker、语义提交栅栏、能力降级、配置发布 breaker/LKG 和远端 signed bundle journal。

验收包括：

1. 每个 key 可解释来源与约束；
2. malformed 文件永不替换有效 generation；
3. 相同坏 hash 不重复重建；
4. 内容修复后能从 half-open 自动恢复；
5. UI 配置失败不阻断 Policy 收紧；
6. managed policy 断网不被删除；
7. 无安全 LKG 时拒绝风险能力，而不是降级为 unrestricted；
8. watcher rename/atomic-save/重复事件不产生多次 publish；
9. 并发手工编辑与自动迁移不会覆盖新内容；
10. Grok/Codex 配置导入 dry-run 能列出全部行为差异。
11. primary 连接失败、429 或 5xx 时按配置顺序切到 secondary；
12. primary Open 时新请求不再等待其 timeout；
13. half-open probe 成功后，新 Turn 自动恢复 primary；
14. 已出现文本或 Tool Call delta 后不会透明换源；
15. fallback 缺少图片、Tool Calling或结构化输出等必需能力时被跳过；
16. reasoning/parallel-tools 等允许降级时有明确事件和 UI 状态；
17. 整条 route 的尝试不超过总时间和次数预算；
18. 当前 Turn failover 后不会在每个 Step 来回抖动。

