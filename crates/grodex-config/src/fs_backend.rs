//! Filesystem event backend for [`ConfigWatcher`] (Doc 18 §11 wiring).
//!
//! `ConfigWatcher` is fs-backend agnostic: it only ever sees content fed
//! through [`ConfigWatcher::observe`]. This module supplies the missing
//! real backend — a `notify` watcher over the discovered config files
//! whose events are read, deduped and validated through the pipeline.
//!
//! Design notes:
//! - we watch each config file's PARENT DIRECTORY (non-recursive) so
//!   atomic saves (write-temp + rename) surface as events on the final
//!   path (acceptance #8);
//! - events are filtered to the registered paths, the file content is
//!   read fresh (a transient unreadable read is skipped — the next event
//!   covers it), and everything from dedup to breaker lives in
//!   `ConfigWatcher`;
//! - every pipeline decision is surfaced as a [`ConfigPublish`] so the
//!   runtime can log / adopt generations without owning the plumbing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

use notify::{Event, RecursiveMode, Watcher};

use crate::error::ConfigError;
use crate::watcher::{ConfigDomain, ConfigWatcher, WatchOutcome};

/// One file to watch, tagged with its publish identity.
#[derive(Debug, Clone)]
pub struct FsWatchSource {
    /// Layer label used as the breaker's source id (`user`, `workspace`…).
    pub source_id: String,
    pub path: PathBuf,
    pub domain: ConfigDomain,
}

/// One pipeline decision for runtime subscribers.
#[derive(Debug)]
pub struct ConfigPublish {
    pub source_id: String,
    pub path: PathBuf,
    pub domain: ConfigDomain,
    pub outcome: WatchOutcome,
}

/// Full-candidate validation (parse → validate → compile, §10) supplied
/// by the host; returns the generation to publish on success.
pub type ConfigValidator = Arc<dyn Fn(&[u8]) -> Result<u64, String> + Send + Sync>;

/// The running backend: keeps the `notify` watcher alive and pumps its
/// events through the [`ConfigWatcher`] pipeline on a dedicated thread.
/// Dropping it drops the watcher, which closes the event channel and
/// ends the pump thread.
pub struct FsConfigBackend {
    // Dropping the watcher stops fs event delivery — keep it alive.
    _watcher: notify::RecommendedWatcher,
    #[allow(dead_code)]
    pump: std::thread::JoinHandle<()>,
}

impl FsConfigBackend {
    /// Start watching `sources`. The pump thread feeds every relevant
    /// fs event into `watcher.observe` and forwards each outcome to
    /// `publishes` (send errors are ignored — a closed channel just
    /// means nobody is listening anymore).
    pub fn start(
        sources: Vec<FsWatchSource>,
        watcher: Arc<Mutex<ConfigWatcher>>,
        validator: ConfigValidator,
        publishes: mpsc::Sender<ConfigPublish>,
    ) -> Result<Self, ConfigError> {
        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
        let mut nw = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .map_err(|e| ConfigError::Watch {
            message: e.to_string(),
        })?;

        // Canonical paths key the identity map — macOS reports
        // `/private/var/...` for `/var/...` temp dirs.
        let mut watched: HashMap<PathBuf, (String, ConfigDomain)> = HashMap::new();
        for s in &sources {
            let key = std::fs::canonicalize(&s.path).unwrap_or_else(|_| s.path.clone());
            watched.insert(key, (s.source_id.clone(), s.domain));
            // Watch the parent directory so rename-based atomic saves
            // land on the registered path (acceptance #8).
            let dir = s
                .path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            if dir.exists() {
                if let Err(e) = nw.watch(dir, RecursiveMode::NonRecursive) {
                    eprintln!(
                        "[warn] config watcher: cannot watch {}: {e}",
                        dir.display()
                    );
                }
            }
        }

        let pump = std::thread::Builder::new()
            .name("config-fs-watch".into())
            .spawn(move || pump(rx, watched, watcher, validator, publishes))
            .map_err(|e| ConfigError::Watch {
                message: e.to_string(),
            })?;
        Ok(Self {
            _watcher: nw,
            pump,
        })
    }
}

/// Pump loop: filter to registered paths + data events, read content,
/// run the pipeline, forward the decision.
fn pump(
    rx: mpsc::Receiver<notify::Result<Event>>,
    watched: HashMap<PathBuf, (String, ConfigDomain)>,
    watcher: Arc<Mutex<ConfigWatcher>>,
    validator: ConfigValidator,
    publishes: mpsc::Sender<ConfigPublish>,
) {
    while let Ok(res) = rx.recv() {
        let Ok(event) = res else { continue };
        // Only content-affecting events; access/remove bursts are noise
        // (a removed file simply misses its next read).
        if !(event.kind.is_create() || event.kind.is_modify()) {
            continue;
        }
        for path in event.paths {
            let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let Some((source_id, domain)) = watched.get(&key) else {
                continue;
            };
            // Mid-write reads can fail transiently — the next event in
            // the burst re-covers it (dedup keeps publishes at one).
            let Ok(content) = std::fs::read(&path) else {
                continue;
            };
            let outcome = watcher.lock().unwrap().observe(
                *domain,
                source_id,
                &content,
                Instant::now(),
                |c| validator(c),
            );
            let _ = publishes.send(ConfigPublish {
                source_id: source_id.clone(),
                path,
                domain: *domain,
                outcome,
            });
        }
    }
}

/// Deterministic single-event entry point (also the unit-testable core):
/// read `path` and push its content through the pipeline. Returns `None`
/// when the file cannot be read right now.
pub fn observe_fs_event(
    watcher: &mut ConfigWatcher,
    source_id: &str,
    domain: ConfigDomain,
    path: &Path,
    now: Instant,
    validate: impl FnOnce(&[u8]) -> Result<u64, String>,
) -> Option<WatchOutcome> {
    let content = std::fs::read(path).ok()?;
    Some(watcher.observe(domain, source_id, &content, now, validate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn ok_validate(generation: u64) -> impl FnOnce(&[u8]) -> Result<u64, String> {
        move |_| Ok(generation)
    }

    fn toml_validate(generation: u64) -> impl FnOnce(&[u8]) -> Result<u64, String> {
        move |c| {
            let s = std::str::from_utf8(c).map_err(|e| e.to_string())?;
            toml::from_str::<toml::Value>(s).map_err(|e| e.to_string())?;
            Ok(generation)
        }
    }

    #[test]
    fn observe_fs_event_publishes_and_dedups() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, b"model_id = \"a\"").unwrap();

        let mut w = ConfigWatcher::default();
        let now = Instant::now();
        let r1 = observe_fs_event(&mut w, "user", ConfigDomain::Root, &path, now, ok_validate(1));
        assert_eq!(r1, Some(WatchOutcome::Published { generation: 1 }));
        // Identical content (duplicate fs event) → deduped.
        let r2 = observe_fs_event(&mut w, "user", ConfigDomain::Root, &path, now, ok_validate(2));
        assert_eq!(r2, Some(WatchOutcome::Unchanged));
        assert_eq!(w.compile_attempts(ConfigDomain::Root), 1);
    }

    #[test]
    fn observe_fs_event_missing_file_is_skipped() {
        let mut w = ConfigWatcher::default();
        let r = observe_fs_event(
            &mut w,
            "user",
            ConfigDomain::Root,
            Path::new("/nonexistent/config.toml"),
            Instant::now(),
            ok_validate(1),
        );
        assert!(r.is_none(), "unreadable file must not reach the pipeline");
        assert_eq!(w.total_publishes(), 0);
    }

    #[test]
    fn observe_fs_event_rejects_malformed_content() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, b"[[[broken").unwrap();

        let mut w = ConfigWatcher::default();
        let r = observe_fs_event(
            &mut w,
            "user",
            ConfigDomain::Root,
            &path,
            Instant::now(),
            toml_validate(1),
        );
        assert!(matches!(r, Some(WatchOutcome::Rejected { .. })));
        assert_eq!(w.total_publishes(), 0);
    }

    #[test]
    fn notify_backend_detects_real_file_change() {
        // End-to-end: a real `notify` watcher feeds the pipeline when the
        // file changes. Timing is OS-dependent, so poll with a deadline.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, b"model_id = \"a\"").unwrap();

        let watcher = Arc::new(Mutex::new(ConfigWatcher::default()));
        let (tx, rx) = mpsc::channel();
        let counter = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let g2 = counter.clone();
        let validator: ConfigValidator = Arc::new(move |c| {
            let s = std::str::from_utf8(c).map_err(|e| e.to_string())?;
            toml::from_str::<toml::Value>(s).map_err(|e| e.to_string())?;
            Ok(g2.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
        });
        let backend = FsConfigBackend::start(
            vec![FsWatchSource {
                source_id: "user".into(),
                path: path.clone(),
                domain: ConfigDomain::Root,
            }],
            watcher.clone(),
            validator,
            tx,
        )
        .unwrap();

        // Mutate AFTER the watcher is up.
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "model_id = \"b\"").unwrap();
        drop(f);

        // Poll until a Published decision arrives (deadline guards CI).
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let mut published = false;
        while Instant::now() < deadline && !published {
            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(p) if matches!(p.outcome, WatchOutcome::Published { .. }) => published = true,
                _ => {}
            }
        }
        assert!(published, "fs change must flow through the pipeline");
        assert!(watcher.lock().unwrap().total_publishes() >= 1);
        drop(backend);
    }
}
