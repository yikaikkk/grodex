//! MemoryRetriever — queries memory store and formats results.
//!
//! Searches memories by keyword relevance to the current user input
//! and formats them for injection into the system prompt or context.

use crate::entry::MemoryEntry;
use crate::store::MemoryStore;

/// Retrieves relevant memories for a given query.
pub struct MemoryRetriever {
    store: MemoryStore,
    /// Maximum number of memories to return.
    max_results: usize,
    /// Minimum keyword match length.
    min_keyword_len: usize,
}

impl MemoryRetriever {
    /// Create a new retriever backed by the given store.
    pub fn new(store: MemoryStore) -> Self {
        Self {
            store,
            max_results: 5,
            min_keyword_len: 3,
        }
    }

    /// Set the maximum number of results.
    pub fn with_max_results(mut self, max: usize) -> Self {
        self.max_results = max;
        self
    }

    /// Query memories relevant to the user input.
    ///
    /// Splits the input into keywords and searches the store.
    /// Returns entries sorted by importance (highest first), then by recency.
    pub fn query(&self, user_input: &str) -> Vec<MemoryEntry> {
        let keywords: Vec<&str> = user_input
            .split_whitespace()
            .filter(|w| w.len() >= self.min_keyword_len)
            .collect();

        if keywords.is_empty() {
            return Vec::new();
        }

        let mut results: Vec<&MemoryEntry> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // Search by each keyword, deduplicating.
        for kw in &keywords {
            for entry in self.store.search(kw) {
                if seen.insert(entry.id.clone()) {
                    results.push(entry);
                }
            }
        }

        // Also search by tag for the full input.
        let tag_matches = self.store.query_by_tag(&user_input.to_lowercase());
        for entry in tag_matches {
            if seen.insert(entry.id.clone()) {
                results.push(entry);
            }
        }

        // Sort by importance (desc), then recency.
        results.sort_by(|a, b| {
            b.importance
                .cmp(&a.importance)
                .then_with(|| b.created_at.cmp(&a.created_at))
        });

        results
            .into_iter()
            .take(self.max_results)
            .cloned()
            .collect()
    }

    /// Format retrieved memories for injection into the system prompt.
    pub fn format_for_prompt(entries: &[MemoryEntry]) -> String {
        if entries.is_empty() {
            return String::new();
        }

        let mut out = String::from("## Relevant Memory from Past Sessions\n\n");
        for entry in entries {
            out.push_str(&format!(
                "- **{}**: {}\n",
                entry
                    .tags
                    .first()
                    .map(|t| t.as_str())
                    .unwrap_or("memory"),
                entry.content
            ));
        }
        out.push('\n');
        out
    }

    /// Save a new memory.
    pub fn save(&mut self, entry: MemoryEntry) {
        self.store.save(entry);
    }

    /// Get the underlying store reference.
    pub fn store(&self) -> &MemoryStore {
        &self.store
    }

    /// Get mutable store reference.
    pub fn store_mut(&mut self) -> &mut MemoryStore {
        &mut self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::MemoryEntry;

    #[test]
    fn retrieves_by_keyword() {
        let mut store = MemoryStore::new();
        store.save(
            MemoryEntry::new("User prefers Rust for systems programming")
                .with_tags(vec!["preference".into()])
                .with_importance(80),
        );
        store.save(
            MemoryEntry::new("Project uses PostgreSQL database")
                .with_tags(vec!["tech-stack".into()])
                .with_importance(50),
        );
        store.save(
            MemoryEntry::new("Deploy to AWS ECS with Docker")
                .with_tags(vec!["deployment".into()]),
        );

        let retriever = MemoryRetriever::new(store);
        let results = retriever.query("How do we deploy the Rust project?");

        // "deploy" matches "deployment" tag and "Rust" matches content.
        assert!(!results.is_empty());
        // Highest importance should be first.
        assert_eq!(results[0].importance, 80);
    }

    #[test]
    fn formats_for_prompt() {
        let entries = vec![MemoryEntry::new("User prefers dark theme")
            .with_tags(vec!["ui".into()])
            .with_importance(70)];

        let formatted = MemoryRetriever::format_for_prompt(&entries);
        assert!(formatted.contains("Relevant Memory"));
        assert!(formatted.contains("dark theme"));
    }

    #[test]
    fn empty_query_returns_nothing() {
        let store = MemoryStore::new();
        let retriever = MemoryRetriever::new(store);
        let results = retriever.query("ab"); // too short
        assert!(results.is_empty());
    }
}
