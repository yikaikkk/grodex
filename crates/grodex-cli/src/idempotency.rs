use std::collections::HashMap;
use std::time::{Duration, Instant};

pub struct IdempotencyCache {
    map: HashMap<String, Instant>,
    capacity: usize,
    ttl: Duration,
}

impl IdempotencyCache {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            map: HashMap::with_capacity(capacity),
            capacity,
            ttl,
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn contains_with_ttl_reclaim(&mut self, key: &str, now: Instant) -> bool {
        let mut removed = 0usize;
        let mut to_remove: Vec<String> = Vec::new();
        for (k, &v) in self.map.iter() {
            if removed >= 256 {
                break;
            }
            if now.duration_since(v) >= self.ttl {
                to_remove.push(k.clone());
                removed += 1;
            }
        }
        for k in to_remove {
            self.map.remove(&k);
        }

        if let Some(&inserted) = self.map.get(key) {
            if now.duration_since(inserted) >= self.ttl {
                self.map.remove(key);
                return false;
            }
            true
        } else {
            false
        }
    }

    pub fn insert(&mut self, key: String, at: Instant) -> bool {
        if self.map.contains_key(&key) {
            return false;
        }
        if self.map.len() >= self.capacity {
            let drain_count = (self.capacity / 8).max(1);
            let mut entries: Vec<(String, Instant)> = self
                .map
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect();
            entries.sort_by_key(|(_, v)| *v);
            for (k, _) in entries.iter().take(drain_count) {
                self.map.remove(k);
            }
        }
        self.map.insert(key, at);
        true
    }

    pub fn check_and_insert(&mut self, key: &str) -> bool {
        let now = Instant::now();
        if self.contains_with_ttl_reclaim(key, now) {
            return true;
        }
        self.insert(key.to_string(), now);
        false
    }
}
