# System Prompt 与指令装配：Grok、Codex 对比及 V2 设计
## 1. 问题边界
Context V2 定义了 Zone A/C/B/D 的位置和缓存稳定性，但没有定义 Zone A 到底由什么组成、AGENTS.md 如何发现、冲突如何解释、运行时新规则如何注入，以及 slash command 是否具有指令权限。

本文把 Prompt 当作版本化构建产物，而不是若干字符串随手拼接。最终产物是可解释的 `PromptManifest + ContextItems`。

## 2. Grok 当前实现
Grok 的 prompt 层已经支持：

+ 内置 system prompt/template；
+ 环境、cwd、shell、日期、VCS、Tool/Skill/MCP 信息；
+ `~/.grok` 全局规则；
+ 从 git root 到 cwd 的层级 AGENTS.md；
+ `.grok/rules/*.md`，并可兼容 `.claude`、`.cursor`；
+ gitignore、canonical path 去重和稳定排序；
+ Session 运行中新发现规则后发送 synthetic reminder；
+ sub-agent 使用独立 prompt；
+ 自定义 system prompt 与 slash command。

AGENTS/rules 的发现入口见 [agents_md.rs](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-agent/src/prompt/agents_md.rs:1)，Session 装配见 [prompt_build.rs](/Users/zhengguilin/Documents/个人项目/grodex/grok/grok-build/crates/codegen/xai-grok-shell/src/session/acp_session_impl/prompt_build.rs:220)。

优点是兼容范围广、运行时发现能力强、产品交互完整。不足是：兼容文件过多时信任/优先级不容易解释；部分规则以 synthetic user item 表达；prompt 片段版本、来源 hash 和冲突诊断尚未统一成为 manifest。

## 3. Codex 当前实现
Codex 将模型默认 base instructions、developer/user instructions、AGENTS.md、Skills 与环境上下文分别构造，再由 Turn/Prompt builder 组装。`agents_md.rs` 负责从配置的 project root marker 到 cwd 读取指令，支持 fallback filename、大小上限和层级拼接，见 [agents_md.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/core/src/agents_md.rs:1)。

模型 base instructions 是模型元数据的一部分，协议层提供内置默认 prompt，见 [models.rs](/Users/zhengguilin/Documents/个人项目/grodex/codex/codex/codex-rs/protocol/src/models.rs:1270)。Session/Turn 捕获稳定 base instructions，有利于 Provider compatibility 和 prompt cache。

优点是角色边界、模型指令和 Turn 构造较清楚，base prompt 与 model metadata 绑定。不足是相比 Grok，运行时新目录规则、slash/custom prompt 产品面和多 vendor 兼容不够集中；各类 instruction 的最终顺序仍需要跨模块理解。

## 4. V2 原则
1. 指令的 authority、scope、位置和覆盖关系是四个不同维度。
2. 仓库文本永远不能提升为 managed/system authority。
3. Prompt 由 manifest 确定性构建，相同输入得到相同 hash。
4. Zone A/C/B/D 按变化频率从低到高排列，继续采用 Context V2 的 `A -> C -> B -> D`。
5. 普通热更新默认下一 Turn 采用，安全收紧通过 runtime fence 立即生效。
6. slash command 是输入变换或 Runtime command，不自动获得 system authority。
7. 隐藏 reasoning 不写入 prompt manifest；Provider reasoning envelope 由 Adapter 管理。

## 5. Instruction Node
```latex
InstructionNode {
  instruction_id
  kind: managed | base | user_global | project | path_rule | runtime
  authority
  scope
  source_uri
  source_hash
  trust_state
  path_predicate?
  content_blob_ref
  content_hash
  schema_version
  discovered_at_generation
}
```

`authority` 决定冲突时谁优先；`scope` 决定何时适用；Prompt 中出现得更靠后不等于权限更高。

## 6. Authority 顺序
从高到低：

```latex
Host/Managed safety instructions
  > Runtime protocol invariants
  > Model/provider base instructions
  > User explicit session instruction
  > User global instruction
  > Trusted workspace/project instruction
  > Path-local rule
  > Skill/custom prompt content
  > Repository/tool/web/MCP 普通内容
```

项目 AGENTS.md 可以指导代码风格和构建方式，但不能要求泄露密钥、放宽 sandbox、修改 Global UserPreference 或绕过 approval。检测到此类内容时标注 conflict，不执行高权限效果。

## 7. 发现规则
### 7.1 固定根
```latex
managed prompt bundle
~/.agent/AGENTS.md
~/.agent/rules/*.md
```

### 7.2 Workspace 链
1. canonicalize cwd；
2. 使用配置定义的 project root marker，优先 git worktree root；
3. 从 root 到 cwd 逐层扫描 `AGENTS.md`；
4. 每层扫描 `.agent/rules/*.md`，文件名稳定排序；
5. symlink canonical path 去重；
6. 遵守安全的 ignore 与文件大小限制；
7. workspace 未信任时只发现元数据，不加载正文。

越靠近 cwd 的项目规则 scope 越窄，可覆盖同 authority 的上层项目规则，但不能覆盖用户或 managed authority。

### 7.3 兼容模式
`.grok`、`.codex`、`.claude`、`.cursor` 和备用文件名通过显式 compatibility 配置启用。冲突时 canonical `.agent` 优先，并产生诊断。不能默认扫描所有 vendor 目录后静默拼接，避免同一规则重复和恶意仓库扩大攻击面。

## 8. PromptManifest
```latex
PromptManifest {
  prompt_schema_version
  assembler_version
  tokenizer_version
  model_binding_id
  config_prompt_generation
  workspace_trust_hash
  nodes[] { id, source_hash, authority, scope, position }
  environment_snapshot_hash
  tool_schema_hash
  final_projection_hash
}
```

Manifest 进入 PromptSnapshot/rollout，正文通过 content-addressed blob 引用。日志不得复制 secret；环境变量只记录允许注入的 key 与 value hash。

## 9. Zone 装配
### Zone A：Session 稳定前缀
+ managed/runtime invariants；
+ model/provider base instructions；
+ user global instructions；
+ Session 创建时确认的 workspace instruction baseline；  
-稳定环境身份：OS family、shell type、workspace roots；
+ prompt schema/version markers。

Zone A 在 Session/明确重建边界才改变。模型切换导致 base prompt 不兼容时创建新 Prompt Baseline，不在旧 Step 中热替换。

### Zone C：Compaction baseline
+ conversation summary；
+ state capsule；
+ compaction 时仍有效的 instruction refs/hash；
+ unresolved constraints。

### Zone B：Turn context
+ 当前用户目标；  
-本 Turn Memory retrieval；
+ 当前 cwd/VCS/env delta；
+ 本 Turn 捕获的 capability/config generation；
+ pending input summary。

### Zone D：Recent tail
+ 最近原始对话；
+ Tool Calls/Results；
+ runtime synthetic messages；
+ Provider-required reasoning envelope。

## 10. 环境信息注入
环境必须结构化、最小化且稳定：

```latex
<environment_context version="2">
  <cwd>...</cwd>
  <workspace_roots>...</workspace_roots>
  <os>...</os>
  <shell>...</shell>
  <date timezone="...">...</date>
  <vcs branch="..." dirty="..." />
  <sandbox profile="..." />
</environment_context>

```

不注入完整环境变量、用户名目录清单或 credential。高频变化的 git diff、文件列表不放 Zone A，只在 Tool 结果或 Turn delta 中按需提供。

## 11. 运行时新指令
Agent 进入此前未扫描的子目录时，可能发现更窄的 AGENTS.md。规则：

1. 验证 workspace trust、路径归属和 source hash；
2. 生成 `InstructionDiscovered` durable event；
3. 当前 Step 不改 leading prefix；
4. 下一 Step 通过 Loop V2 的 `author=runtime` 合成消息告知新规则摘要和 source；
5. 下一 Turn 把它纳入新的 Prompt baseline；
6. 规则删除/变化同样产生 invalidation event。

这样保留 Grok 的运行时发现优势，又避免 Turn 中途破坏 prompt cache 和输入 hash。

## 12. 冲突与 explain
Assembler 不尝试用 LLM 猜所有冲突。先使用可解释规则：

+ authority 高者优先；
+ 同 authority、同 key 时 scope 更具体者优先；
+ safety deny 不可被 allow 覆盖；
+ 无法结构化判断的自然语言冲突全部保留，并生成 `InstructionConflict` 供模型/用户看到；
+ project 内容试图改变 Policy/credential/managed 行为时直接标为越界。

`prompt explain` 返回每个 node 来源、authority、适用路径、是否被遮蔽和最终位置；`prompt dump --redacted` 输出实际 canonical prompt，便于复现。

## 13. Slash Command 与自定义命令
分成三类：

| 类型 | 示例 | 语义 |
| --- | --- | --- |
| Runtime command | `/model`、`/compact`、`/cancel` | 不发给模型，调用版本化 Runtime command |
| Prompt macro | 自定义 review prompt | 展开为带 provenance 的 user input |
| Skill invocation | `/skill release` | 走 Skill registry/read，不内联伪装成 system |


用户定义 prompt macro 无论放在哪个文件，都不自动成为 system/developer instruction。参数使用结构化模板和转义；禁止默认执行 shell substitution。命令名冲突时内置 Runtime command 保留命名空间，用户命令使用 `user:<name>` 或明确 override 配置。

## 14. Compaction 与恢复
compaction 不把所有 Zone A 正文复制进 summary，只保存 manifest refs、适用 scope 和关键 unresolved constraint。恢复 Session 时：

+ 读取原 Session pin 的 PromptManifest；
+ source 仍存在且 hash 一致则复用；
+ source 已改变时，历史 Step 仍按旧 hash解释，新 Turn采用新 generation；  
-旧 blob 已过 TTL 时保留 hash/provenance，不能假装内容仍可重建；
+ deterministic replay 使用历史 manifest，不使用当前磁盘文件重写过去。

## 15. 安全规则
+ 未信任 workspace 指令不加载正文；
+ 指令文件不得通过 include 逃出允许 root；
+ symlink、大小、编码和递归 include 都有限制；
+ 仓库/MCP/web 内容不能生成 Global/UserPreference；
+ prompt dump 默认脱敏路径和环境值；
+ managed prompt bundle 必须签名并有 LKG；
+ assembler 失败回退到签名内置 prompt或同等安全 LKG，不使用半成品。

## 16. 相对原实现的收益
### 相对 Grok
| 当前问题 | V2 | 收益 |
| --- | --- | --- |
| 多 vendor 规则可能隐式叠加 | 显式 compat + canonical 优先级 | 可预测、减少重复和注入面 |
| synthetic reminder 与 baseline 边界分散 | 当前 Step reminder、下一 Turn baseline | 缓存稳定且规则及时可见 |
| 来源信息未统一 | PromptManifest | 可回放、可 explain |
| slash command 权限边界模糊 | 三类命令分流 | 不把宏错误提升为 system |


### 相对 Codex
| 当前问题 | V2 | 收益 |
| --- | --- | --- |
| 运行时局部规则产品面较弱 | InstructionDiscovered 协议 | 进入新目录可及时采纳 |
| base/project/env 跨模块理解 | 单一 assembler/manifest | 最终 prompt 可审计 |
| custom prompt 与 Skill 入口分散 | command registry 分型 | 用户体验统一 |
| project trust 与 prompt 装配联系不完整 | trust hash 进入 manifest | 仓库注入不能静默持久化 |


## 17. 关键决策收益闭环
| 决策 | 解决的问题 | 收益 | 指标 |
| --- | --- | --- | --- |
| Instruction Node/Manifest | 字符串拼接不可解释 | 确定性、可审计 | projection hash 一致率 |
| authority 与位置分离 | 后出现文本被误认为高权限 | 安全冲突裁决 | 越权指令拦截数 |
| root->cwd scope | 多层 AGENTS 语义不清 | 局部规则自然覆盖 | path fixture 通过率 |
| 下一 Turn 更新 baseline | 运行时规则打爆 cache | 及时性与缓存平衡 | cache hit、采纳延迟 |
| command 三分 | slash 行为混乱 | 权限与 UX 清楚 | 误发模型/误执行数 |
| refs 跨 compaction | summary 重复大 prompt | 节省 token、保留 provenance | compaction 后指令覆盖率 |


## 18. 实施与验收
Phase 1：Instruction Node、固定发现顺序、PromptManifest、`prompt explain/dump`。

Phase 2：Zone assembler、runtime discovery event、compaction refs 和 cache metrics。

Phase 3：统一 slash/Skill/custom command registry，增加兼容迁移器。

验收包括：

1. 相同文件与配置得到相同 prompt hash；
2. root/cwd 多层规则顺序稳定；
3. 未信任仓库规则不注入；
4. 进入新目录不改变当前 Step prefix，下一 Turn 生效；
5. project 指令不能放宽 Policy/Sandbox；
6. compaction/resume 后有效指令不丢且不重复；
7. 模型切换的 base prompt compatibility 可诊断；
8. runtime slash command 不进入模型历史；
9. prompt macro 不拥有 system authority；
10. oversized/include/symlink 攻击被拒绝并有明确诊断。
