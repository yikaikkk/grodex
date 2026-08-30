# Telemetry 可观测系统设计

## 1. 文档定位

本文定义 Grodex 的单机可观测系统：在既有 `rollout.jsonl` 事实源之上，增加一层可查询的运行观测投影（SQLite），回答"这个 Turn 为什么结束、哪一步卡住、哪个模型慢、哪个工具失败、审批等了多久、成本是多少"。

本文同时是设计与实现说明：所述架构已全部落地（crates/grodex-telemetry 及各 crate 接入点），实现现状以"实现状态"标注区分于设计意图。核心原则：

1. **rollout.jsonl 是事实源**——负责"发生了什么，恢复时必须相信什么"；
2. **telemetry.db 是查询投影**——负责"哪里慢、哪里失败、为什么停、成本是多少"；
3. **tracing 文件日志是人工调试补充**——`~/.grodex/logs/grodex.log`，不承担查询职责。

## 2. 要解决的问题

在可观测系统之前，代码库已有三类数据，但都不足以回答运维问题：

| 现有设施 | 用途 | 不足 |
| --- | --- | --- |
| `rollout.jsonl` | 恢复、审计、重放 | 面向正确性，不适合做统计查询；崩溃前未提交的遥测无法事后分析 |
| tracing 文本日志 | 开发调试 | 非结构化，无法按 Turn/工具/模型聚合 |
| memory.db（SQLite） | 记忆索引 | 可重建索引、有大量检索读；混入高频遥测写会产生锁竞争，保留周期与数据权限也不同 |

具体缺口（均为设计前实测）：TurnMetrics 只在 Turn 结束 tracing 打印；UsageRecord 定义了但零构造点；PromptSnapshotBuilt、ModelRouteEvent 只有类型/writer 没有发射链路；工具、审批、沙箱、MCP、Memory、Sub-agent 的耗时与失败原因没有统一关联；无进程级 run_id，多进程并行无法区分；事件敏感级别写死 Normal。

## 3. 总体架构

### 3.1 双层记录

```text
业务正确性层                          运行观测层
RolloutWriter ──► rollout.jsonl       TelemetrySink ──► telemetry.db (SQLite WAL)
恢复 / 审计 / 重放                     查询 / 统计 / 诊断
```

### 3.2 核心不变量

1. **遥测失败绝不影响 Agent Loop**。写库失败只 tracing warn；队列满只丢低优先级事件；`TelemetrySink::emit` 保证非阻塞。
2. **每条记录带 `journal_seq`**。journal 派生的遥测记录 `event_id = "{session_id}:{seq}"`，确定性生成——重投影天然幂等（INSERT OR IGNORE）。
3. **遥测不是恢复真相**。删除 telemetry.db 不影响任何正确性；重启时从 journal 全量补投影。
4. **默认不落敏感内容**。payload 超 64KB 截断为合法 JSON 标记；prompt 只存 hash 与 token 估算；`sensitivity` 字段区分 normal / personal / credential。

### 3.3 数据流

```text
业务线程 (tokio task)
    │ RolloutWriter::write() 成功返回 journal_seq（单汇聚点）
    ▼
bounded channel (4096)  ──满──► 丢弃并计数（queue-full shedding）
    ▼
telemetry writer 线程（单写者，照抄 journal_actor 模式，但用 std 线程 + rusqlite）
    ▼ 批量事务：64 条或 100ms
SQLite (PRAGMA: journal_mode=WAL, synchronous=NORMAL, busy_timeout=5000, foreign_keys=ON)
    ▼ 同事务内
投影维护：sessions / turns / model_attempts / tool_executions / …
```

路径：`~/.grodex/telemetry.db`（`GRODEX_TELEMETRY_DB` 覆盖），文件权限 0600。进程退出由 guard（仿 `tracing_appender::WorkerGuard`）执行最终 flush + 关闭。打开失败 fail-open：遥测整体禁用，不影响会话。

## 4. 事件接入模型

### 4.1 单点接入（journal 派生）

`RolloutWriter::write()` 是所有 journal 写入的唯一汇聚点且返回落盘 seq。接入即在 append 成功后 fire-and-forget 一条遥测记录：kind 取 RolloutEventType 的稳定 snake_case 映射，payload 原样序列化（截断），severity 由 `is_error` 推导，sensitivity 由事件自身的 `SensitivityLevel` 映射。**因此 journal 里已有的事件（tool 全生命周期、compaction、approval、lease、skill snapshot、subagent 生命周期）零新增发射代码自动进入遥测。**

### 4.2 事件补齐（durable / 非 durable 分层）

接入时补齐了缺失的生命周期锚点：

| 事件 | durable | 发射点 | 作用 |
| --- | --- | --- | --- |
| `SessionStarted` | 是 | supervisor 启动 + resume rebind | sessions.started_at；payload 含 cwd / model（cwd 在投影层只存 SHA-256） |
| `TurnStarted` | 是 | supervisor start_turn | turns.started_at（否则崩溃在 Turn 中段的 Turn 对投影不可见） |
| `ModelAttemptStarted` / `ModelAttemptFinished` | 否（同 ModelRouteEvent "observability-only" 先例） | turn_coordinator 采样前后 | 每轮采样一条记录 |
| `PromptSnapshotBuilt`（补发射） | 否 | turn_coordinator prompt build 后 | prompt hash + 条数 + token 估算 |
| `TurnCompleted` payload 扩展 | 是 | supervisor | 结构化 `termination_reason` + 聚合计数器 |

`termination_reason` 枚举：`final_answer | step_budget_exhausted | cancelled | sampling_error | journal_failure`（`TurnOutcome.termination_reason`，协调器在终止路径上判定）。

### 4.3 Route 事件打通

sampler 的 `RouteEvent`（CandidateSelected/Succeeded/Failed/Rejected/RouteExhausted/BreakerOpened）此前积压在 `ModelRoute.pending_events` 只有测试消费。现在 `SamplingOutcome.route_events` 由 actor 在调用结束后 drain 带出，coordinator 逐条转 `write_route_event` 落 journal，再经单点接入进遥测。

### 4.4 Out-of-band 遥测（不进 journal）

外围模块的计时类观测走 `RolloutWriter::emit_out_of_band_telemetry(kind, turn_id, call_id, payload)`（或 runtime 构造期直接持有 sink），用 uuid event_id：

| kind | 发射点 | 内容 |
| --- | --- | --- |
| `memory_retrieval` | supervisor 检索前后计时 | query_chars、selected_count、duration_ms、router_kind |
| `mcp_lifecycle` | runtime MCP 初始化循环 | server_name、phase（spawn/list_tools）、transport、tool_count、status、error_class、duration_ms |

## 5. Schema 与投影

`user_version` 迁移（照抄 grodex-memory 模式），当前版本 4。设计为"一张原始事件表 + 多张查询投影表"：

### 5.1 v1 — 骨架

- `telemetry_events`：原始事件表，保留所有 kind（包括投影还不认识的），`journal_seq` 与 rollout.jsonl 逐条对齐；
- `sessions`（run_id / started_at / cwd_hash / model）；
- `turns`（started/finished/status/termination_reason/计数器/duration/input_chars）；
- `projection_cursors`（source='journal' 的 last_journal_seq 高水位）。

### 5.2 v2 — 诊断

- `model_attempts`：每轮采样——provider/model、attempts（重试次数）、duration、status、error_class、http_status、retry_after_secs、first_token_ms、provider_request_id、六项 token 计数。Started 插 running 行，Finished 按 (session, step) 更新；Started 丢失时有 finished-only 兜底。
- `tool_executions`：由 durable 的 prepared→approved→started→finished→committed 事件链组装——各阶段时间戳、`approval_wait_ms`（由 security_decisions 中 requested/resolved 时间戳差值计算）、duration/exit_code/is_error/status。
- `security_decisions`：approval_requested/resolved、lease 三事件、capability_stale——回答"为什么这个工具没执行"。

### 5.3 v3 — 上下文与成本

- `prompt_builds`：prompt_snapshot_hash / context_item_count / estimated_input_tokens。**红线：不存 prompt 内容**——hash 连续一致即缓存前缀稳定的证据，内容本体留在 journal / blob store。
- `compactions`：Started→Candidate→Committed/Failed 生命周期。
- 视图：`v_session_timeline`、`v_turn_summary`（含未闭环工具数/失败模型调用数）、`v_tool_lifecycle`（anomaly 分类：stuck_running / uncommitted / indeterminate）、`v_model_usage`（含平均 TTFT）、`v_cache_stats`、`v_recovery_anomalies`（四类异常 UNION）。

### 5.4 v4 — 外围模块

- `subagent_runs`（task/agent/parent/label/tokens/status）；
- `skill_activations`（name/source/path/content_hash/generation）；
- `memory_retrievals`、`mcp_lifecycle`（out-of-band）。

## 5.5 成本口径说明

缓存命中率 = `SUM(cached_input_tokens) / SUM(input_tokens)`，取的是**供应商上报的 cached tokens（计费口径）**，不是本地 prompt hash。本地 hash 只说明请求是否稳定，不能替代供应商计费数据。`model_attempts.estimated` 列区分 provider 返回的 usage 与本地估算回退。

## 6. 查询能力

`grodex telemetry <subcommand>`（SQL 全部在 grodex-telemetry::query，CLI 只做格式化；只读打开，`--db` / `GRODEX_TELEMETRY_DB` 指定路径）：

| 命令 | 回答的问题 |
| --- | --- |
| `sessions` / `session <id>` | 有哪些会话；一个会话内每个 Turn 的终止原因与计数器 |
| `turn <id>` | 单 Turn 详情：终止原因 + 每次模型尝试（attempts/耗时/TTFT/http/error/cache%）+ 工具生命周期（审批等待 vs 执行耗时，卡住的行直接标注） |
| `timeline <id>` | 会话内 Turn 时间线 |
| `errors [n]` | 最近 error 级事件 |
| `slow-tools [n]` | 工具按平均耗时排序，单列平均审批等待——拆分"审批慢还是执行慢" |
| `slow-models [n]` | 模型按平均耗时排序，含错误数与缓存命中率 |
| `cache` | 按模型的 prompt 缓存命中明细 + 全局总命中率 |
| `recovery` | 跨会话生命周期异常清单（open_turn / stuck_tool / uncommitted_result / indeterminate_tool） |
| `doctor` | 健康检查：未结束的 Turn、running 工具、未提交结果、indeterminate、失败模型调用、in-flight compaction |
| `vacuum` | WAL checkpoint + VACUUM |
| `export [--session] [--output]` | 原始事件 JSONL 导出 |

## 7. 崩溃补偿（re-projection）

进程在 journal append 成功但遥测 commit 之前死掉，会留下投影缺口。补偿机制：

1. `RolloutWriter::reproject_telemetry()`：读全量 journal，按确定性 event_id 重新 ingest（`INSERT OR IGNORE` 幂等）；
2. CLI 每次构建会话 runtime 时后台执行一次；
3. `ingest` 带 5 秒有界重试（启动期队列可能瞬时满）；
4. `projection_cursors` 记录高水位，可增量补投影（当前实现从 0 全量重放，幂等保证正确性）。

## 8. 运维策略

- **保留期**：打开数据库时自动删除 30 天前的 `telemetry_events` 原始行（`GRODEX_TELEMETRY_RETENTION_DAYS` 覆盖，0 关闭）。投影表行保留——体积小且保证会话历史可读。
- **碎屑回收**：`telemetry vacuum`（WAL checkpoint TRUNCATE + VACUUM）。
- **丢载可见性**：队列满丢弃有计数（`SqliteTelemetrySink::dropped_count`），writer 退出时 warn 汇总。
- **写失败语义**：批量提交失败整批丢弃并 warn，不上抛、不重试（重投影会补回 journal 派生的部分）。

## 9. 非目标

+ 不做分布式/跨机聚合（单机 SQLite 足够，导出 JSONL 供外部分析）；
+ 不做实时 metrics/OTLP 导出（tracing 层保持独立，将来可作为第二 sink 并存）；
+ 不在遥测中存 prompt 内容、工具结果全文、API 凭证（分别留在 journal/blob store/credentials.json）；
+ 不让遥测 schema 变化反向影响 journal schema（journal schema_version 保持 2 不动）。

## 10. 实现对照

| 组件 | 位置 |
| --- | --- |
| TelemetryRecord / Sink trait / kind 常量 / payload 截断 | `crates/grodex-telemetry/src/record.rs` |
| SQLite sink（单写者线程 / 批量 / flush / guard） | `crates/grodex-telemetry/src/sqlite.rs` |
| schema 迁移 + 视图 | `crates/grodex-telemetry/src/schema.rs` |
| 查询函数（供 CLI 复用） | `crates/grodex-telemetry/src/query.rs` |
| 单点接入 + sensitivity + re-projection + out-of-band | `crates/grodex-loop/src/rollout_writer.rs` |
| 事件补齐（SessionStarted/TurnStarted/ModelAttempt/PromptSnapshot/termination_reason） | `crates/grodex-rollout/src/event.rs`、`grodex-loop/src/{supervisor,turn_coordinator,step}.rs` |
| Route 事件 drain + TTFT 测量 + provider_request_id | `crates/grodex-sampler/src/{actor.rs,decoder/*}`、`grodex-provider/src/canonical_event.rs` |
| CLI 装配（run_id / sink / guard / reproject） | `crates/grodex-cli/src/{main.rs,runtime.rs}` |
| 测试 | `crates/grodex-telemetry/tests/sink.rs`（投影/幂等/丢载/保留期）、`crates/grodex-loop/tests/telemetry_projection.rs`（seq 关联/崩溃重投影） |
