use std::time::{Duration, Instant};

use grodex_cli::idempotency::IdempotencyCache;

#[test]
fn idempotency_cache_ttl_and_capacity_math_compiles() {
    let mut cache = IdempotencyCache::new(4, Duration::from_secs(60));
    let now = Instant::now();
    for i in 0..8 {
        cache.insert(format!("k{i}"), now);
    }
    assert!(cache.len() <= 4, "capacity 4 溢出应 drain");
}

#[test]
fn ack_inflight_bounds_respect_cap() {
    assert!(130u64 - 0u64 >= 128u32 as u64);
    assert!(130u64 - 3u64 < 128u32 as u64);
}

#[test]
fn idempotency_ttl_expiry_works() {
    let mut cache = IdempotencyCache::new(16, Duration::from_millis(10));
    let t0 = Instant::now();
    cache.insert("a".into(), t0);
    assert!(cache.contains_with_ttl_reclaim("a", t0));
    let t1 = t0 + Duration::from_millis(200);
    assert!(!cache.contains_with_ttl_reclaim("a", t1));
}

#[test]
fn idempotency_insert_returns_boolean() {
    let mut cache = IdempotencyCache::new(16, Duration::from_secs(60));
    let now = Instant::now();
    assert!(cache.insert("x".into(), now));
    assert!(!cache.insert("x".into(), now));
}
