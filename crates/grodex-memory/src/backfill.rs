//! Embedding Backfiller（增量回填）。
//!
//! 启动时（或显式调用）扫描 `memory_units` / `evidence_units` 中尚无
//! 当前模型向量行的活跃单元，按 `model.batch_size()` 批量调
//! `embed_texts` 写回 `document_embeddings`。
//!
//! 幂等：已有（命名空间或裸 id）向量行的单元不重复嵌入；
//! Fail-Open：任何一批失败即停止并返回已写回数量，不抛给调用方致命错误。

use crate::database::DbError;
use crate::database::MemoryDatabase;
use crate::embedding::{EmbeddingError, EmbeddingModel};

/// 增量回填缺失 embedding 的文档（memory + evidence 活跃单元）。
///
/// 返回本次写回的文档数。`max_documents` 限制单次扫描上限，
/// 防止大库阻塞启动；下次调用继续补齐剩余部分。
pub async fn backfill_missing_embeddings(
    store: &MemoryDatabase,
    model: &dyn EmbeddingModel,
    max_documents: usize,
) -> Result<usize, EmbeddingError> {
    if max_documents == 0 {
        return Ok(0);
    }
    let missing = store
        .units_missing_embeddings(model.model_id(), max_documents)
        .map_err(EmbeddingError::from)?;
    if missing.is_empty() {
        return Ok(0);
    }

    let batch_size = model.batch_size().max(1);
    let dim = model.dimension();
    let model_id = model.model_id().to_string();
    let mut written = 0usize;

    for batch in batch_chunks(&missing, batch_size) {
        let texts: Vec<String> = batch.iter().map(|(_, content)| content.clone()).collect();
        let vecs = match model.embed_texts(&texts).await {
            Ok(v) => v,
            // Fail-open：这批失败不阻塞会话，已写回的保留。
            Err(_) => break,
        };
        if vecs.len() != batch.len() {
            // 返回数量与请求不一致 → 无法对齐，停止回填。
            break;
        }
        for ((doc_ref, _), vec) in batch.iter().zip(vecs) {
            if vec.len() != dim {
                continue; // 维度异常的向量不入库，其余继续。
            }
            if store
                .write_embedding_sync(doc_ref, 0, &model_id, dim, &vec)
                .is_ok()
            {
                written += 1;
            }
        }
    }

    Ok(written)
}

/// 暴露给外部的同步/异步判断：当 embedding NotConfigured 时直接短路。
pub fn is_backfill_possible(model: Result<&dyn EmbeddingModel, &EmbeddingError>) -> bool {
    match model {
        Ok(_) => true,
        Err(e) => !matches!(e, EmbeddingError::NotConfigured),
    }
}

/// 批量切分 helper：按 chunk_size 切 items 成若干 batch。
pub fn batch_chunks<T: Clone>(items: &[T], batch_size: usize) -> Vec<Vec<T>> {
    if batch_size == 0 || items.is_empty() {
        return Vec::new();
    }
    items.chunks(batch_size).map(|c| c.to_vec()).collect()
}

/// 将 DbError 映射为 EmbeddingError（跨模块调用时用）。
impl From<DbError> for EmbeddingError {
    fn from(e: DbError) -> Self {
        EmbeddingError::Deserialize(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MemoryUnit, MemoryKind, MemoryScope, UnitStatus};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 确定性 stub：按文本长度生成伪向量，维度固定。
    struct StubModel {
        dim: usize,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl EmbeddingModel for StubModel {
        async fn embed_texts(
            &self,
            texts: &[String],
        ) -> Result<Vec<crate::embedding::EmbeddingVector>, EmbeddingError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(texts
                .iter()
                .map(|t| {
                    let seed = t.len() as f32 + 1.0;
                    (0..self.dim).map(|i| seed + i as f32 * 0.01).collect()
                })
                .collect())
        }
        fn dimension(&self) -> usize {
            self.dim
        }
        fn model_id(&self) -> &str {
            "stub-model"
        }
        fn batch_size(&self) -> usize {
            2
        }
    }

    fn make_memory_unit(id: &str, content: &str) -> MemoryUnit {
        MemoryUnit {
            id: id.to_string(),
            path: "MEMORY.md".to_string(),
            section: format!("MEMORY.md#{id}"),
            kind: MemoryKind::Fact,
            scope: MemoryScope::Workspace,
            status: UnitStatus::Active,
            content: content.to_string(),
            content_hash: format!("hash-{id}"),
            updated_at: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn backfill_writes_missing_and_is_idempotent() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        db.upsert_memory_unit(&make_memory_unit("m1", "deploy step runs cargo build")).unwrap();
        db.upsert_memory_unit(&make_memory_unit("m2", "release notes go to changelog")).unwrap();
        db.upsert_memory_unit(&make_memory_unit("m3", "third unit about testing")).unwrap();

        let model = StubModel { dim: 8, calls: AtomicUsize::new(0) };
        let n = backfill_missing_embeddings(&db, &model, 100).await.unwrap();
        assert_eq!(n, 3);
        // batch_size=2 → 3 条分 2 批。
        assert_eq!(model.calls.load(Ordering::SeqCst), 2);

        // 第二次运行：全部已有向量 → 0。
        let n2 = backfill_missing_embeddings(&db, &model, 100).await.unwrap();
        assert_eq!(n2, 0);
        assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn backfilled_vectors_are_retrievable_via_hybrid() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        db.upsert_memory_unit(&make_memory_unit("m1", "kubernetes deployment workflow")).unwrap();

        let model = StubModel { dim: 8, calls: AtomicUsize::new(0) };
        backfill_missing_embeddings(&db, &model, 100).await.unwrap();

        let model: std::sync::Arc<dyn crate::embedding::EmbeddingModel + Send + Sync> =
            std::sync::Arc::new(model);
        let results = db
            .retrieve_hybrid_memory("kubernetes deployment workflow", 5, Some(&model))
            .await
            .unwrap();
        // 向量路径命中（query 与文档同文本 → 余弦=1）后剥前缀仍能加载回单元。
        assert!(results.iter().any(|r| r.unit_id == "m1"));
    }

    #[tokio::test]
    async fn backfill_respects_max_documents() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        for i in 0..5 {
            db.upsert_memory_unit(&make_memory_unit(&format!("m{i}"), &format!("content {i}")))
                .unwrap();
        }
        let model = StubModel { dim: 8, calls: AtomicUsize::new(0) };
        let n = backfill_missing_embeddings(&db, &model, 2).await.unwrap();
        assert_eq!(n, 2);
    }

    #[tokio::test]
    async fn backfill_zero_max_is_noop() {
        let db = MemoryDatabase::open_in_memory().unwrap();
        let model = StubModel { dim: 8, calls: AtomicUsize::new(0) };
        assert_eq!(backfill_missing_embeddings(&db, &model, 0).await.unwrap(), 0);
    }

    #[test]
    fn batch_chunks_handles_various_sizes() {
        let items: Vec<usize> = (0..10).collect();
        let chunks = batch_chunks(&items, 3);
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0], vec![0, 1, 2]);
        assert_eq!(chunks[3], vec![9]);
    }

    #[test]
    fn batch_chunks_empty_or_zero() {
        assert!(batch_chunks::<usize>(&[], 3).is_empty());
        assert!(batch_chunks(&[1, 2, 3], 0).is_empty());
    }
}
