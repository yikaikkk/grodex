//! Embedding Backfiller（增量回填）—— V2 骨架实现。
//!
//! 当前只提供空骨架，等用户启用 `[memory] enable_embedding=true`
//! 且 Eval recall 证据集积累后再真实跑。
//!
//! 设计（未来真实实现时遵循）：
//!   1. 扫 memory_units：WHERE unit_id NOT IN (SELECT DISTINCT doc_ref
//!      FROM document_embeddings WHERE embedding_model = ?)
//!   2. 取最多 max_documents 条，按 batch_size 调 embed_texts 批量写回
//!   3. 返回写回的文档数（用于可观测）

use crate::database::DbError;
use crate::database::MemoryDatabase;
use crate::embedding::{EmbeddingError, EmbeddingModel};

/// 增量回填缺失 embedding 的文档（当前骨架：返回 Ok(0)，不做真实扫描）。
///
/// 未来真实版本：批量拉无 embedding 的 memory_units，按 batch_size
/// 调 `model.embed_texts()` → `store.write_embedding()` 写回。
pub async fn backfill_missing_embeddings(
    _store: &MemoryDatabase,
    _model: &dyn EmbeddingModel,
    _max_documents: usize,
) -> Result<usize, EmbeddingError> {
    Ok(0)
}

/// 暴露给外部的同步/异步判断：当 embedding NotConfigured 时直接短路。
pub fn is_backfill_possible(model: Result<&dyn EmbeddingModel, &EmbeddingError>) -> bool {
    match model {
        Ok(_) => true,
        Err(e) => !matches!(e, EmbeddingError::NotConfigured),
    }
}

/// (Future) 批量切分 helper：按 chunk_size 切 texts 成若干 batch。
#[allow(dead_code)]
pub fn batch_chunks<T: Clone>(items: &[T], batch_size: usize) -> Vec<Vec<T>> {
    if batch_size == 0 || items.is_empty() {
        return Vec::new();
    }
    items
        .chunks(batch_size)
        .map(|c| c.to_vec())
        .collect()
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

    #[test]
    fn backfill_skeleton_returns_zero() {
        // 不需要真实 runtime / db，直接用 block_on 空实现。
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        // 构造一个最小的 model（动态 trait object 难构造，这里只测 Ok(0) 分支）。
        struct DummyModel;
        #[async_trait::async_trait]
        impl EmbeddingModel for DummyModel {
            async fn embed_texts(
                &self,
                _texts: &[String],
            ) -> Result<Vec<crate::embedding::EmbeddingVector>, EmbeddingError>
            {
                Ok(vec![])
            }
            fn dimension(&self) -> usize {
                0
            }
            fn model_id(&self) -> &str {
                "dummy"
            }
        }

        let db = MemoryDatabase::open_in_memory().unwrap();
        let model = DummyModel;
        let n = rt.block_on(backfill_missing_embeddings(&db, &model, 100)).unwrap();
        assert_eq!(n, 0);
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
