# 24. 会话记忆召回（rollout recall）设计

> 状态：方案评审中，未实现
> 依赖：grodex-rollout（`rollout.jsonl` 事件账本）、context-management（docs/11 压缩管线）

## 1. 问题

上下文压缩（compaction）把旧回合折叠成摘要后，**逐字信息不可逆丢失**：

- 工具调用的完整参数、原始输出、错误信息；
- 用户早先给过的约束原话（「用 Rust 别用 Python」「端口必须是 8080」）；
- 当时的 diff 摘要、审批决策、深层链结果。

这些信息其实都在：每个会话在 `~/.grodex/sessions/<uuid>/rollout.jsonl` 里有**append-only 的完整事件账本**（`SessionStarted / UserInputAccepted / ModelItemProduced / ToolCallPrepared / ToolCallApproved / ToolExecutionFinished / ToolResultCommitted / PromptInjected / CompactionCommitted / TurnCompleted / DiffAvailable …`，见 `grodex-rollout/src/event.rs`）。问题不是数据不存在，而是**模型不知道账本存在、也没有查询通道**。

目标：让模型在发现「记忆里没有」时，能**按需检索本会话的逐字历史**——按需，而不是把历史塞回上下文。

## 2. 设计原则

1. **按需检索，不自动回灌**。自动把历史塞回上下文等于放弃压缩的收益；只在模型主动查询时付费。
2. **压缩时保留指针，不保留内容**。压缩摘要想附带一小块「索引/指针」，让模型知道该往哪查。
3. **投影而非原文**。raw JSONL 一行可能几 KB；工具在服务端做投影（字段裁剪、按事件类型过滤、截断），模型只看到面向任务的视图。
4. **只读 + 会话内**。默认只能查当前会话的 rollout，不提供跨会话、不提供任意路径读取。

## 3. 方案：三层

### L1 — 查询工具 `session_recall`（核心）

一个只读工具，四个 action：

| action | 入参 | 返回 |
|---|---|---|
| `list_turns` | `offset?, limit?=50` | 回合索引：`[{turn_id, seq_start, seq_end, first_user_msg(截断), termination_reason, started_at}]` —— 回答「我们都聊过什么」 |
| `read_turn` | `turn_id`, `offset?, limit?=100` | 该回合的投影事件流：用户输入原文、每次工具调用的 `name + args（截断） + is_error + content（截断）`、模型文本项、PromptInjected |
| `search` | `text`, `event_types?`, `tool_name?`, `is_error?`, `limit?=30` | 全账本正则/子串匹配，返回命中行（带 seq + turn_id），按时间排序 —— 回答「之前那个报错/那句话是什么」 |
| `read_range` | `from_seq`, `max_lines?` | 原始行（转义后截断），兜底兜到 search 覆盖不了的场景 |

公共行为：
- 事件流按 `seq` 升序；返回体硬上限 32KB，超出带 `next_offset` 让模型续读；
- 每条返回带 `seq` 与 `turn_id`，模型可以继续下钻；
- 字段裁剪规则：`args`/`content` 超 500 字符截断并标注 `…(N chars, 用 read_range 看原文)`。

### L2 — 压缩时注入指针（关键配套）

`CompactionCommitted` 时，在压缩产物**末尾追加一段固定格式的指针块**：

```
[历史指针] 更早回合的逐字记录未丢失，存于本会话 rollout 账本。
被压缩回合：turn_012 ~ turn_047（最后事件 seq=1823）。
检索：session_recall(action="list_turns" | "search" | "read_turn", ...)。
典型用途：找回用户早期约束的原文、旧工具调用的参数与输出、历史报错全文。
```

实现位置：compaction 摘要构建处（`CompactionCandidateBuilt → CompactionCommitted` 管线）。指针块本身也会随下次压缩被摘要，但摘要模型会保留「rollout 可查」这个事实——配合 L3 的静态说明双保险。

### L3 — 系统提示静态说明

prompt 组装（docs/19）固定附加一段：

> 当前会话拥有完整的逐字事件账本（rollout）。当发现早期对话细节（用户约束原文、工具输出、历史报错）在上下文中缺失或被压缩时，使用 `session_recall` 工具检索；先 `list_turns` 或 `search` 定位，再 `read_turn` 下钻。

## 4. 实现落点

| 位置 | 内容 |
|---|---|
| `crates/grodex-tools/src/session_recall.rs`（新） | 工具定义 + 投影/过滤/截断逻辑；入参含 `session_id` 由宿主注入当前会话（模型不可传任意 id） |
| `crates/grodex-cli/src/runtime.rs` | 注册工具时把当前会话的 `rollout.jsonl` 绝对路径 + `session_id` 注入工具构造（RolloutWriter 已知自身路径，`store.rs:208` 已有 `path()` 形态接口） |
| 读取方式 | 直接按行流式读 JSONL（append-only 文件读到底即当前状态，无需 journal_actor 参与）；`journal_actor::start_readonly` 可作后续复用 |
| compaction 管线（grodex-loop） | L2 指针块追加 |
| prompt 组装（grodex-prompt） | L3 静态说明 |
| 前端 | 无需改动（普通只读工具，走现有工具卡/权限表） |

## 5. 安全与权限

1. **只读**，default_policy = Allow（与 read_file 同级）；
2. **路径不暴露给模型**：session_id 由宿主注入，工具不接收路径参数；`cross_session=true`（查其他会话）可作为扩展项，默认拒绝（要开就走 Ask）；
3. 大小双重防护：单次返回 32KB 上限 + 单字段 500 字符截断，防账本异常膨胀刷爆上下文；
4. 遥测：`session_recall` 调用本身走现有工具遥测，可观测「模型多久查一次历史」作为压缩质量信号。

## 6. 与 memory 的边界（不重复建设）

| | session_recall（本方案） | grodex-memory |
|---|---|---|
| 范围 | 当前会话 | 跨会话 |
| 形态 | 逐字事件账本的按需检索 | 语义化的长期记忆条目 |
| 生命周期 | 会话即生命周期 | 独立生命周期（用户管理） |
| 典型问题 | 「三小时前那个报错的完整堆栈」「用户最早说的端口号」 | 「这个项目用什么测试命令」 |

两者互补：压缩丢的本会话细节找 recall；跨会话该记住的找 memory。

## 7. 分期

| 阶段 | 内容 |
|---|---|
| P0 | `session_recall` 工具（list_turns + search + read_turn）+ L3 静态说明 —— 一天内可用 |
| P1 | L2 压缩指针块（需要动 compaction 摘要构建） |
| P2 | `cross_session` 扩展（Ask 门控）、`read_range`、turn 级摘要缓存（首次 list 后缓存于内存） |

## 8. 风险

1. **token 失控**：模型可能反复大范围查询——靠 32KB 上限 + limit 默认值 + 遥测观测；如失控可把工具默认策略降为 Ask；
2. **账本schema 演进**：投影层按 `event_type` 白名单输出，未知事件类型跳过不报错（向前兼容）；
3. **本回合事件不可见**：工具调用发生在回合内，本回合自身的事件可能尚未落账本——文档里说明（模型查到「当前回合查不到自己」属预期）；
4. **敏感信息**：rollout 含审批决策、环境信息等——本会话内模型本来看得到这些，无新增暴露面；`cross_session` 默认关是边界。
