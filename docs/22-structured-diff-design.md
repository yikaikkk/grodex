# 22 · 结构化 Diff 设计（Structured Diff）

> 目标：让 Desktop 能对 `write_file` / `edit_file` / `apply_patch` 展示**真实、净变化、懒加载**的 diff，而不是 mock 或重复内联全文到 rollout。

## 现状盘点（可复用地基）

- `ChangedResource { resource_id, display_path, change_type(Created/Updated/Deleted/Moved/Metadata), before_hash, after_hash }` — `grodex-tools/src/common.rs:387`，write/edit/patch 的 `BuiltInTool::execute` 路径已产出。
- `PatchPlan` / `PatchFile { source_rid, target_rid, operation(Add/Update/Delete/Move), expected_version_before, after_hash, hunks }` — `patch.rs`。
- `FileBlobStore`（SHA-256 内容寻址）+ `ManagedBlobStore`（`BlobRefLedger` 引用计数 + 宽限 GC）— `blob_store.rs` / `blob_refs.rs`。
- Rollout 事件 `ToolExecutionFinished` / `ToolResultCommitted`（只存 `content`，不存 diff）。
- ACP 无 diff 事件；`DiffViewer.tsx` 是 mock（`MOCK_DIFF_FILES = []`）。

**关键缺口 / 架构事实**：
1. 工具只产出 `before_hash/after_hash`（哈希），**无 old/new 内容**，无法渲染 diff。
2. `changed_resources` **loop 侧未消费**（`grep changed_resources crates/grodex-loop` 空）。
3. **loop 的真实执行路径是 `ToolRuntime::execute`（返回原始 JSON），不是 `BuiltInTool::execute`（产出 envelope/changed_resources）**。见 `turn_coordinator.rs:2484` `rt.execute(effective_args, prepared.operation_id)`。这是 Phase 3 的关键决策点。

## 数据模型

`ChangedResource` 增加两个 bounded 内容字段（工具在 prepare/execute 时已读过旧文件、已知新内容）：

```rust
pub struct ChangedResource {
    // ...现有字段...
    /// 渲染 diff 所需的 old/new 全文；超过 N 字节置 None（renderable=false，退化为统计视图）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_content: Option<String>,
}
```

新增一等类型 `AppliedChangeDelta`（净变化集）：

```rust
pub struct AppliedChangeDelta {
    pub changes: Vec<AppliedFileChange>,
    pub exact: bool, // exec 等无法精确追踪时 false
}
```

## 架构流

```text
write_file / edit_file / apply_patch
        ↓ 捕获 old/new 内容
ChangedResource（含内容）
        ↓
TurnDiffTracker（每 Turn 内存态，净变化合并）
        ↓
净 unified/结构化 diff → FileBlobStore（diff_id = sha256）
        ↓
rollout 只记 DiffAvailable { diff_id, summary, hashes }
        ↓
ACP DiffAvailable（摘要）→ Desktop 按需 GetDiff → DiffPayload（bounded/分页）
```

## 阶段（每阶段独立验收）

1. **类型**：`ChangedResource` 加 `before_content/after_content`；新增 `AppliedFileChange`/`AppliedChangeDelta`；单测序列化/兼容。
2. **工具捕获**：write/edit/patch 在 `BuiltInTool::execute` 路径填内容（超限退化）。
3. **TurnDiffTracker**：净变化（多次更新/还原/rename/delete+add）+ 单测；`exec` 默认 invalidate。
4. **Blob**：`BlobOwnerKind::Diff` + `BlobRefKind::DiffBody`；`store_owned` 落盘去重 + GC。
5. **Rollout**：`DiffAvailable` 轻量事件（只摘要，不内联全文）。
6. **ACP**：`DiffAvailable` / `GetDiff` / `DiffPayload` + session/generation 校验。
7. **Desktop**：DiffViewer 按 `diff_id` 懒加载。
8. **审批**：PreviewDiff（审批前）vs AppliedDiff（执行后）+ old hash 校验（`FILE_CHANGED_SINCE_PREVIEW`）。
9. **exec / Git fallback**：评估，可后置。

## 决策点（默认值）

- 内容字节上限：**64KB**（超出退化统计视图）。
- `exec` diff：**默认 invalidate**（受控 snapshot 后置）。
- diff 存储格式：**`application/vnd.grodex.diff+json`**（结构化，前端渲染稳）。
- 分支：**独立 feature 分支**推进。

## 关键风险

- **Phase 3 的执行路径问题**：loop 用 `ToolRuntime::execute`，而内容捕获在 `BuiltInTool::execute`。Phase 3 需二选一：(a) 让 `ToolRuntime::execute` 也捕获内容并写入输出 JSON 的保留键、loop 提取后剥离；(b) 把 loop 切到 prepared 路径。**在 Phase 2 完成后、Phase 3 前拍板。**
- 超大文件 diff：内容字段设上限，避免内存/rollout 膨胀。
- blob 缺失：resume 不阻断，UI 显示 `unavailable`。
