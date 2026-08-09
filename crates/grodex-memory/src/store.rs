//! MemoryStore — 旧版 in-memory 存储（HashMap 版），保持向后兼容。
//!
//! V2 主存储是 `database::MemoryDatabase`（SQLite + FTS5 + 向量 blob）。
//! 此文件提供同名 API 的空实现，以便旧代码不破坏编译：
//!   - write_embedding / search_bruteforce_cosine （空/占位）
//!   - retrieve_hybrid （走 legacy search + RRF 纯 FTS 退化）

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::embedding::{EmbeddingModel, EmbeddingVector};
use crate::entry::MemoryEntry;
use crate::retrievers::reciprocal_rank_fusion;

/// Legacy in-memory store (HashMap)。
#[derive(Debug, Clone)]
pub struct MemoryStore {
    entries: HashMap<String, MemoryEntry>,
    file_path: Option<PathBuf>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            file_path: None,
        }
    }

    pub fn with_file(path: PathBuf) -> Self {
        let mut store = Self {
            entries: HashMap::new(),
            file_path: Some(path),
        };
        store.load();
        store
    }

    pub fn save(&mut self, entry: MemoryEntry) {
        self.entries.insert(entry.id.clone(), entry);
    }

    pub fn get(&mut self, id: &str) -> Option<&MemoryEntry> {
        if let Some(entry) = self.entries.get_mut(id) {
            entry.record_access();
            Some(entry)
        } else {
            None
        }
    }

    pub fn query_by_tag(&self, tag: &str) -> Vec<&MemoryEntry> {
        self.entries
            .values()
            .filter(|e| e.tags.iter().any(|t| t == tag))
            .collect()
    }

    pub fn search(&self, keyword: &str) -> Vec<&MemoryEntry> {
        let lower = keyword.to_lowercase();
        self.entries
            .values()
            .filter(|e| e.content.to_lowercase().contains(&lower))
            .collect()
    }

    pub fn delete(&mut self, id: &str) -> bool {
        self.entries.remove(id).is_some()
    }

    pub fn list(&self) -> Vec<&MemoryEntry> {
        let mut entries: Vec<&MemoryEntry> = self.entries.values().collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.created_at));
        entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn persist(&self) -> Result<(), std::io::Error> {
        if let Some(ref path) = self.file_path {
            let entries: Vec<&MemoryEntry> = self.entries.values().collect();
            let json = serde_json::to_string_pretty(&entries)?;
            std::fs::write(path, json)?;
        }
        Ok(())
    }

    fn load(&mut self) {
        if let Some(ref path) = self.file_path {
            if let Ok(data) = std::fs::read_to_string(path) {
                if let Ok(entries) = serde_json::from_str::<Vec<MemoryEntry>>(&data) {
                    for entry in entries {
                        self.entries.insert(entry.id.clone(), entry);
                    }
                }
            }
        }
    }

    // ───────── V2 Embedding / Hybrid (Legacy store stubs) ─────────

    /// Legacy store 不支持持久化向量 → 直接返回 Ok。
    /// V2 真实实现见 `MemoryDatabase::write_embedding`。
    pub async fn write_embedding(
        &self,
        _doc_ref: &str,
        _chunk: usize,
        _model: &str,
        dim: usize,
        vec: &EmbeddingVector,
    ) -> Result<(), crate::database::DbError> {
        if vec.len() != dim {
            return Err(crate::database::DbError::Embedding(format!(
                "vec len {} != dim {}",
                vec.len(),
                dim
            )));
        }
        Ok(())
    }

    /// Legacy store 无向量表 → Fail-soft 返回空 Vec。
    pub async fn search_bruteforce_cosine(
        &self,
        _query_vec: &EmbeddingVector,
        _top_k: usize,
        _model_filter: &str,
    ) -> Result<Vec<(String, f32)>, crate::database::DbError> {
        Ok(Vec::new())
    }

    /// Legacy store 版 Hybrid 检索（永远退化到纯 FTS = legacy search）。
    pub async fn retrieve_hybrid(
        &self,
        query: &str,
        top_k: usize,
        _emb: Option<&Arc<dyn EmbeddingModel + Send + Sync>>,
    ) -> Result<Vec<crate::database::RetrievedUnit>, crate::database::DbError> {
        let hits: Vec<&MemoryEntry> = self.search(query);
        let fts_ids: Vec<String> = hits.iter().take(top_k * 2).map(|e| e.id.clone()).collect();
        let fused = reciprocal_rank_fusion(&fts_ids, &[], top_k, 60.0);

        let mut out = Vec::with_capacity(fused.len());
        for id in fused {
            if let Some(entry) = self.entries.get(&id) {
                out.push(crate::database::RetrievedUnit {
                    unit_id: entry.id.clone(),
                    path: String::new(),
                    content: entry.content.clone(),
                    source: crate::types::ResultSource::Memory,
                });
            }
        }
        Ok(out)
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_retrieve() {
        let mut store = MemoryStore::new();
        let entry = MemoryEntry::new("The user prefers Rust").with_tags(vec!["preference".into()]);
        let id = entry.id.clone();
        store.save(entry);

        assert_eq!(store.len(), 1);
        assert!(store.get(&id).is_some());
    }

    #[test]
    fn query_by_tag() {
        let mut store = MemoryStore::new();
        store.save(MemoryEntry::new("memory a").with_tags(vec!["work".into()]));
        store.save(MemoryEntry::new("memory b").with_tags(vec!["personal".into()]));
        store.save(MemoryEntry::new("memory c").with_tags(vec!["work".into()]));

        assert_eq!(store.query_by_tag("work").len(), 2);
        assert_eq!(store.query_by_tag("personal").len(), 1);
        assert_eq!(store.query_by_tag("nonexistent").len(), 0);
    }

    #[test]
    fn search_by_keyword() {
        let mut store = MemoryStore::new();
        store.save(MemoryEntry::new("Rust is a systems language"));
        store.save(MemoryEntry::new("Python is great for scripting"));
        store.save(MemoryEntry::new("Rust has great tooling"));

        assert_eq!(store.search("rust").len(), 2);
        assert_eq!(store.search("python").len(), 1);
    }

    #[test]
    fn delete_entry() {
        let mut store = MemoryStore::new();
        let entry = MemoryEntry::new("temporary");
        let id = entry.id.clone();
        store.save(entry);

        assert!(store.delete(&id));
        assert!(store.get(&id).is_none());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn file_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.json");

        let mut store = MemoryStore::with_file(path.clone());
        store.save(MemoryEntry::new("persisted memory"));
        store.persist().unwrap();

        let store2 = MemoryStore::with_file(path);
        assert_eq!(store2.len(), 1);
    }
}
