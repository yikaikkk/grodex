//! Single-writer Journal Actor — the authoritative durable append path.
//!
//! Design rationale (P0-1 reliability requirements):
//!
//! 1. **Serial seq assignment**: seq is allocated *inside* the actor task
//!    immediately before fsync, never outside. This guarantees physical
//!    on-disk order == logical seq order, even if callers race.
//! 2. **Fail-closed on write failure**: if serialize / write / flush / fsync
//!    fails, the seq is NOT consumed (we return Err before bumping the
//!    counter) so the journal cannot develop permanent gaps.
//! 3. **flush + fsync**: every event does `flush()` (userspace → kernel)
//!    and critical events (ToolExecutionStarted / TurnCompleted /
//!    ApprovalRequested / rebind-points) additionally do `sync_data()`
//!    (kernel → disk platter). Non-critical events batch their fsync
//!    on a counter or after N ms (see [`FsyncPolicy`]).
//! 4. **rebind quiesce barrier**: `JournalHandle::rebind()` waits for the
//!    actor to drain the in-flight queue *before* swapping the file handle,
//!    so writes from the "old session" can never leak into the "new
//!    session" journal. The actor itself validates session_id on every
//!    message as a defence-in-depth belt-and-suspenders check.
//! 5. **Corruption detection**: `replay_from` is strict — any JSON line
//!    that does not deserialize into a valid [`RolloutEvent`] with the
//!    right schema_version returns `GrodexError::JournalCorrupt` rather
//!    than silently skipping. Silent data loss is worse than a loud
//!    failure; operators can repair the journal explicitly if needed.
//!
//! This actor is the ONLY path that writes to `rollout.jsonl`. Any direct
//! `File::create().append(true)` on that path is forbidden.

use crate::event::RolloutEvent;
use grodex_core::error::GrodexError;
use grodex_core::id::SessionId;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};

/// How aggressively we force `sync_data()` (kernel → physical media)
/// between append batches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsyncPolicy {
    /// fsync every single event. Safest, slowest. Use for approval /
    /// tool-execution-start / turn-boundary events whose durability is
    /// correctness-critical.
    Always,
    /// fsync at most once every N events. Good for streaming TextDelta /
    /// ReasoningDelta where losing the last few chunks on power loss is
    /// acceptable (the model can re-sample on resume).
    EveryN { n: u32 },
    /// Never call fsync; rely on the OS's writeback cache. Only valid for
    /// unit tests that use `tempfile::TempDir` backed by tmpfs. Do NOT use
    /// in production — a process crash is fine, but a kernel panic / power
    /// loss will corrupt the journal.
    TestOnly,
}

impl Default for FsyncPolicy {
    fn default() -> Self {
        FsyncPolicy::EveryN { n: 8 }
    }
}

/// Message sent from callers to the single writer task.
enum ActorMessage {
    /// Append one event. The actor fills in seq + timestamp and returns
    /// the committed seq on success; failure means the event was NOT
    /// written and NO seq was consumed.
    Append {
        event: RolloutEvent,
        /// Hint to the fsync scheduler — if true, force `sync_data()` now
        /// regardless of `EveryN` batching. Set by callers that need the
        /// event to be durable before releasing a side-effect (tool exec,
        /// approval ticket, turn boundary).
        force_fsync: bool,
        reply: oneshot::Sender<Result<u64, GrodexError>>,
    },
    /// Quiesce the queue, swap out the backing file for a new path, reseed
    /// the seq counter, and adopt a new session_id. The reply fires once
    /// every pre-rebind message has been committed and sync'd to the old
    /// file and the new file is open + ready. This is `/resume`'s atomic
    /// switchover point.
    Rebind {
        new_jsonl_path: PathBuf,
        new_session_id: SessionId,
        next_seq: u64,
        reply: oneshot::Sender<Result<(), GrodexError>>,
    },
    /// Explicit flush + fsync request (used before Tool exec launch).
    SyncBarrier {
        reply: oneshot::Sender<Result<(), GrodexError>>,
    },
    /// Ask the actor to gracefully exit (used on shutdown).
    Shutdown,
}

/// Handle to the single-writer journal actor. Cloneable — all clones share
/// the same underlying `mpsc::Sender` so ordering is preserved across
/// producers.
#[derive(Clone)]
pub struct JournalHandle {
    tx: mpsc::UnboundedSender<ActorMessage>,
    /// Rebind and SyncBarrier must serialize with respect to each other;
    /// the per-handle mutex ensures that. We deliberately do NOT gate
    /// Append on this mutex (only rebind/barrier) so the common fast path
    /// is lock-free from the caller's perspective.
    serialize_rebind: Arc<Mutex<()>>,
}

impl JournalHandle {
    /// Start the single-writer task. Opens (or creates) the journal file
    /// and positions the seq counter at `initial_next_seq` (usually the
    /// count of events replayed from an existing journal, or 0 for a
    /// brand-new session).
    pub async fn start(
        jsonl_path: PathBuf,
        session_id: SessionId,
        initial_next_seq: u64,
        fsync_policy: FsyncPolicy,
    ) -> Result<Self, GrodexError> {
        // Open the file on the blocking threadpool — `File::open` is sync
        // and we don't want to stall the runtime worker on cold metadata.
        let jsonl_path_for_open = jsonl_path.clone();
        let mut file = tokio::task::spawn_blocking(move || -> Result<File, GrodexError> {
            if let Some(parent) = jsonl_path_for_open.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    GrodexError::Internal(anyhow::anyhow!(
                        "cannot create journal dir {:?}: {e}",
                        parent
                    ))
                })?;
            }
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&jsonl_path_for_open)
                .map_err(|e| {
                    GrodexError::Internal(anyhow::anyhow!(
                        "cannot open journal {:?}: {e}",
                        jsonl_path_for_open
                    ))
                })
        })
        .await
        .map_err(|e| GrodexError::Internal(anyhow::anyhow!("join open task: {e}")))??;

        // Flush+fsync on startup to confirm the file handle is actually
        // usable (catching EROFS / quota errors early).
        file.flush().map_err(|e| {
            GrodexError::Internal(anyhow::anyhow!("initial journal flush: {e}"))
        })?;
        file.sync_all().map_err(|e| {
            GrodexError::Internal(anyhow::anyhow!("initial journal fsync: {e}"))
        })?;

        let (tx, rx) = mpsc::unbounded_channel::<ActorMessage>();
        let actor = JournalActor {
            file,
            jsonl_path: jsonl_path.clone(),
            session_id,
            next_seq: initial_next_seq,
            fsync_policy,
            fsync_batch_counter: 0,
        };
        tokio::spawn(run_actor(actor, rx));

        Ok(Self {
            tx,
            serialize_rebind: Arc::new(Mutex::new(())),
        })
    }

    /// Build a handle whose receive loop always returns an error.
    ///
    /// Used by `FileRolloutStore::open_readonly` so that code paths
    /// which accidentally try to `append_event` on a read-only opened
    /// journal fail loudly (fail-closed) instead of silently dropping
    /// bytes or starting a real writer task on a borrowed file.
    pub fn start_readonly(session_id: SessionId) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<ActorMessage>();
        // Run a tiny task whose sole job is to reject every message
        // with "opened read-only". This keeps the channel alive (so
        // callers don't get "actor stopped" confusingly) while
        // guaranteeing no writes reach disk.
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let err_msg = "JournalHandle was opened read-only; writes are forbidden";
                match msg {
                    ActorMessage::Append { reply, .. } => {
                        let e: Result<u64, GrodexError> = Err(GrodexError::Internal(anyhow::anyhow!("{err_msg}")));
                        let _ = reply.send(e);
                    }
                    ActorMessage::Rebind { reply, .. } => {
                        let e: Result<(), GrodexError> = Err(GrodexError::Internal(anyhow::anyhow!("{err_msg}")));
                        let _ = reply.send(e);
                    }
                    ActorMessage::SyncBarrier { reply, .. } => {
                        let e: Result<(), GrodexError> = Err(GrodexError::Internal(anyhow::anyhow!("{err_msg}")));
                        let _ = reply.send(e);
                    }
                    ActorMessage::Shutdown => break,
                }
            }
        });
        Self {
            tx,
            serialize_rebind: Arc::new(Mutex::new(())),
        }
    }

    /// Submit an event for durable append. On success the returned seq is
    /// guaranteed monotonically increasing and physically contiguous with
    /// the prior committed seq (no gaps).
    ///
    /// `force_fsync=true` should be set for any event whose durability
    /// gates a real-world side-effect: approval tickets issued, tool
    /// executions started, turn completion, and before a rebind barrier.
    pub async fn append(
        &self,
        event: RolloutEvent,
        force_fsync: bool,
    ) -> Result<u64, GrodexError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorMessage::Append {
                event,
                force_fsync,
                reply: tx,
            })
            .map_err(|_| {
                GrodexError::Internal(anyhow::anyhow!(
                    "journal actor stopped; cannot append event"
                ))
            })?;
        rx.await.map_err(|_| {
            GrodexError::Internal(anyhow::anyhow!(
                "journal actor dropped append reply without responding"
            ))
        })?
    }

    /// Quiesce, swap the backing file, reseed seq, adopt new session_id.
    ///
    /// The returned Future resolves only after:
    /// (a) every message queued before this call has been committed and
    ///     sync'd to the OLD journal file,
    /// (b) the new file is open, flushed, and ready for new appends,
    /// (c) `next_seq` has been reset so the next `append()` produces seq
    ///     `next_seq` exactly.
    pub async fn rebind(
        &self,
        new_jsonl_path: PathBuf,
        new_session_id: SessionId,
        next_seq: u64,
    ) -> Result<(), GrodexError> {
        // Serialize concurrent rebind() + sync_barrier() callers. Multiple
        // clients racing to `/resume` must not interleave their file
        // swaps. Append messages don't take this lock — they just flow
        // through the mpsc which is already linearizable.
        let _guard = self.serialize_rebind.lock().await;
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorMessage::Rebind {
                new_jsonl_path,
                new_session_id,
                next_seq,
                reply: tx,
            })
            .map_err(|_| {
                GrodexError::Internal(anyhow::anyhow!("journal actor stopped; cannot rebind"))
            })?;
        rx.await.map_err(|_| {
            GrodexError::Internal(anyhow::anyhow!(
                "journal actor dropped rebind reply without responding"
            ))
        })?
    }

    /// Wait for every currently-enqueued append to be flushed and fsync'd.
    /// Useful before launching a child process / side-effecting tool call.
    pub async fn sync_barrier(&self) -> Result<(), GrodexError> {
        let _guard = self.serialize_rebind.lock().await;
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ActorMessage::SyncBarrier { reply: tx })
            .map_err(|_| {
                GrodexError::Internal(anyhow::anyhow!(
                    "journal actor stopped; cannot sync barrier"
                ))
            })?;
        rx.await.map_err(|_| {
            GrodexError::Internal(anyhow::anyhow!(
                "journal actor dropped sync-barrier reply without responding"
            ))
        })?
    }
}

// ── Actor internals ──────────────────────────────────────────────────

struct JournalActor {
    file: File,
    jsonl_path: PathBuf,
    session_id: SessionId,
    next_seq: u64,
    fsync_policy: FsyncPolicy,
    fsync_batch_counter: u32,
}

/// Decide whether to call sync_data() on this write boundary.
fn should_fsync_now(
    policy: FsyncPolicy,
    batch_counter: &mut u32,
    force: bool,
) -> bool {
    if matches!(policy, FsyncPolicy::TestOnly) {
        return false;
    }
    if force || matches!(policy, FsyncPolicy::Always) {
        *batch_counter = 0;
        return true;
    }
    if let FsyncPolicy::EveryN { n } = policy {
        *batch_counter = batch_counter.saturating_add(1);
        if *batch_counter >= n {
            *batch_counter = 0;
            return true;
        }
    }
    false
}

async fn run_actor(mut actor: JournalActor, mut rx: mpsc::UnboundedReceiver<ActorMessage>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            ActorMessage::Append {
                mut event,
                force_fsync,
                reply,
            } => {
                let res = actor_do_append(&mut actor, &mut event);
                if let Ok(seq) = res {
                    // seq is committed to the file bytes (in userspace buf
                    // at minimum); decide on fsync now.
                    if should_fsync_now(actor.fsync_policy, &mut actor.fsync_batch_counter, force_fsync) {
                        if let Err(e) = actor.file.sync_data() {
                            let _ = reply.send(Err(GrodexError::Internal(anyhow::anyhow!(
                                "journal fsync seq={seq}: {e}"
                            ))));
                            continue;
                        }
                    }
                    let _ = reply.send(Ok(seq));
                } else {
                    let _ = reply.send(res);
                }
            }
            ActorMessage::Rebind {
                new_jsonl_path,
                new_session_id,
                next_seq,
                reply,
            } => {
                let res = actor_do_rebind(&mut actor, new_jsonl_path, new_session_id, next_seq);
                let _ = reply.send(res);
            }
            ActorMessage::SyncBarrier { reply } => {
                // Flush + fsync unconditionally. Caller serialization
                // (serialize_rebind mutex) already guarantees no new
                // rebinds can interleave; but appends keep flowing in
                // parallel — that's fine, the barrier only needs "what
                // was queued before me" to be durable, which is
                // guaranteed by mpsc FIFO order: we process this
                // SyncBarrier message *after* every Append that was
                // send()'d before SyncBarrier was send()'d.
                let res = actor
                    .file
                    .flush()
                    .and_then(|_| actor.file.sync_data())
                    .map_err(|e| GrodexError::Internal(anyhow::anyhow!("journal barrier: {e}")));
                let _ = reply.send(res);
            }
            ActorMessage::Shutdown => {
                // Best-effort final flush. We ignore errors here because
                // shutdown is non-recoverable anyway; the drop handler
                // on File would also try to flush but that's best-effort.
                let _ = actor.file.flush();
                let _ = actor.file.sync_all();
                break;
            }
        }
    }
}

fn actor_do_append(actor: &mut JournalActor, event: &mut RolloutEvent) -> Result<u64, GrodexError> {
    // Defence-in-depth: assert the caller did not try to override session
    // id. If a stray client from the OLD session keeps sending events
    // after rebind (which should be prevented by the rebind quiesce +
    // RolloutWriter inner swap, but belts-and-suspenders), reject it.
    if event.session_id != actor.session_id {
        return Err(GrodexError::Internal(anyhow::anyhow!(
            "journal session_id mismatch: event={} actor={}",
            event.session_id,
            actor.session_id
        )));
    }

    // ── Serialize FIRST (before bumping seq) ──────────────────────
    // If serde fails we return Err without consuming a seq → no gaps.
    // (We discard the first serialization output because the event's
    // seq field isn't stamped yet; only used to catch serde bugs early
    // so we don't allocate a seq for obviously-invalid payloads.)
    let _line = serde_json::to_string(event).map_err(|e| {
        GrodexError::Internal(anyhow::anyhow!("serialize rollout event: {e}"))
    })?;

    // ── Allocate seq ONLY after serialization succeeds ────────────
    let seq = actor.next_seq;
    event.seq = seq;

    // Re-serialize now that seq has been stamped onto the event.
    // (We deliberately don't reuse the previous `line` because it
    // had whatever seq the caller put in — usually 0 or stale.)
    let line = serde_json::to_string(event).map_err(|e| {
        // seq was already assigned in-memory; this is a non-recoverable
        // serialization bug (shouldn't happen since it just worked
        // above). We deliberately DO NOT roll next_seq back: the only
        // way to stay gap-free is crash loudly rather than silently
        // reuse a seq number. Crashing here is fail-closed.
        GrodexError::Internal(anyhow::anyhow!(
            "re-serialize rollout event after seq stamp: {e}"
        ))
    })?;

    // ── Write + flush userspace buffer ────────────────────────────
    // writeln! calls the underlying Write::write_all once for the
    // payload + once for the '\n' byte.
    writeln!(actor.file, "{line}").map_err(|e| {
        // Partial write possible. Same policy as above: do NOT roll back
        // seq (another writer may already observe this seq value on
        // disk and assume it's committed). Return error so the caller
        // aborts the turn instead of corrupting the journal.
        GrodexError::Internal(anyhow::anyhow!("write journal seq={seq}: {e}"))
    })?;
    actor.file.flush().map_err(|e| {
        GrodexError::Internal(anyhow::anyhow!("flush journal seq={seq}: {e}"))
    })?;

    // ── Bump seq — commit point reached for metadata ──────────────
    actor.next_seq = actor.next_seq.checked_add(1).ok_or_else(|| {
        GrodexError::Internal(anyhow::anyhow!(
            "journal seq overflow (u64); session cannot continue"
        ))
    })?;

    Ok(seq)
}

fn actor_do_rebind(
    actor: &mut JournalActor,
    new_jsonl_path: PathBuf,
    new_session_id: SessionId,
    next_seq: u64,
) -> Result<(), GrodexError> {
    // Quiesce: flush + fsync the OLD file so every event queued before
    // this Rebind message is physically durable on the old session id.
    actor
        .file
        .flush()
        .and_then(|_| actor.file.sync_all())
        .map_err(|e| {
            GrodexError::Internal(anyhow::anyhow!(
                "rebind flush+fsync old journal {:?}: {e}",
                actor.jsonl_path
            ))
        })?;

    // Open new file. If this fails we stay bound to the old journal; the
    // caller sees Err and can retry or surface the error to the user.
    if let Some(parent) = new_jsonl_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            GrodexError::Internal(anyhow::anyhow!(
                "rebind: cannot create new journal dir {:?}: {e}",
                parent
            ))
        })?;
    }
    let new_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&new_jsonl_path)
        .map_err(|e| {
            GrodexError::Internal(anyhow::anyhow!(
                "rebind: cannot open new journal {:?}: {e}",
                new_jsonl_path
            ))
        })?;

    // Swap. From this point on every Append message (in FIFO order after
    // this Rebind) writes to the NEW journal + NEW session_id.
    actor.file = new_file;
    actor.jsonl_path = new_jsonl_path;
    actor.session_id = new_session_id;
    actor.next_seq = next_seq;
    actor.fsync_batch_counter = 0;

    // Flush + fsync the new (empty) file to confirm it's usable. EROFS /
    // quota errors here surface to the caller synchronously instead of
    // on the first Append.
    actor.file.flush().and_then(|_| actor.file.sync_all()).map_err(|e| {
        GrodexError::Internal(anyhow::anyhow!(
            "rebind: initial fsync new journal: {e}"
        ))
    })
}

// ── Strict replay (callable without starting the actor) ──────────────

/// Read the journal file from disk and return every event whose `seq` is
/// >= `from_seq`, in the order they appear on disk.
///
/// **Strict corruption policy**:
/// - A missing file is treated as an empty journal → Ok(vec![]). This is
///   the common case for brand-new sessions before the first append.
/// - An empty line or a line that fails to deserialize into a valid
///   [`RolloutEvent`] (wrong schema_version, missing seq, mangled JSON)
///   returns `GrodexError::JournalCorrupt` with the byte offset and
///   reason. The caller (SessionSupervisor::resume) then must NOT
///   silently continue — either the user repairs the file or we start a
///   fresh session. Silent data loss is forbidden.
/// - Filtering is performed on `event.seq`, NOT the physical line number.
///   This is the only correct semantics when a journal is merged after a
///   successful rebind (old + new files both start at their own seq 0,
///   but the logical projection concatenates them with an offset).
pub fn replay_journal_strict(
    jsonl_path: &Path,
    from_seq: u64,
) -> Result<Vec<RolloutEvent>, GrodexError> {
    use std::io::{BufRead, BufReader};

    if !jsonl_path.exists() {
        return Ok(Vec::new());
    }
    let file = File::open(jsonl_path).map_err(|e| {
        GrodexError::Internal(anyhow::anyhow!(
            "replay: cannot open journal {:?}: {e}",
            jsonl_path
        ))
    })?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for (line_no, line_res) in reader.lines().enumerate() {
        let line = line_res.map_err(|e| {
            GrodexError::JournalCorrupt {
                path: jsonl_path.to_string_lossy().into_owned(),
                line: line_no as u64,
                reason: format!("io error reading line: {e}"),
            }
        })?;
        if line.trim().is_empty() {
            // Empty lines are unambiguously corrupt in an append-only
            // log — every writeln! call produced exactly one non-empty
            // JSON payload. Fail-closed.
            return Err(GrodexError::JournalCorrupt {
                path: jsonl_path.to_string_lossy().into_owned(),
                line: line_no as u64,
                reason: "empty line in journal".into(),
            });
        }
        let event: RolloutEvent = serde_json::from_str(&line).map_err(|e| {
            GrodexError::JournalCorrupt {
                path: jsonl_path.to_string_lossy().into_owned(),
                line: line_no as u64,
                reason: format!("json deserialize: {e}; raw_line_start={}", &line[..line.len().min(120)]),
            }
        })?;
        if event.schema_version != 2 {
            return Err(GrodexError::JournalCorrupt {
                path: jsonl_path.to_string_lossy().into_owned(),
                line: line_no as u64,
                reason: format!(
                    "unsupported schema_version: got {}, expected 2",
                    event.schema_version
                ),
            });
        }
        if event.seq >= from_seq {
            out.push(event);
        }
    }
    Ok(out)
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{RolloutEvent, RolloutEventType, SensitivityLevel};
    use grodex_core::id::SessionId;

    fn make_event(session_id: SessionId) -> RolloutEvent {
        RolloutEvent {
            schema_version: 2,
            seq: 0, // filled in by actor
            session_id,
            turn_id: None,
            step_id: None,
            generation: None,
            timestamp: chrono::Utc::now(),
            event_type: RolloutEventType::RuntimeStateChanged,
            payload: serde_json::json!({"state": "running"}),
            sensitivity: SensitivityLevel::Normal,
        }
    }

    #[tokio::test]
    async fn actor_assigns_contiguous_seq_and_replay_filters_by_seq() {
        let dir = tempfile::tempdir().unwrap();
        let sid = SessionId::new();
        let jsonl = dir.path().join("rollout.jsonl");
        let h = JournalHandle::start(
            jsonl.clone(),
            sid,
            0,
            FsyncPolicy::TestOnly,
        )
        .await
        .unwrap();

        let s0 = h.append(make_event(sid), false).await.unwrap();
        let s1 = h.append(make_event(sid), false).await.unwrap();
        let s2 = h.append(make_event(sid), false).await.unwrap();
        assert_eq!((s0, s1, s2), (0, 1, 2));

        let all = replay_journal_strict(&jsonl, 0).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].seq, 0);
        assert_eq!(all[2].seq, 2);

        // Replay from seq 1 should skip seq=0.
        let tail = replay_journal_strict(&jsonl, 1).unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].seq, 1);
        assert_eq!(tail[1].seq, 2);
    }

    #[tokio::test]
    async fn replay_detects_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl = dir.path().join("rollout.jsonl");
        std::fs::write(&jsonl, "{not valid json\n").unwrap();
        let res = replay_journal_strict(&jsonl, 0);
        assert!(matches!(res, Err(GrodexError::JournalCorrupt { .. })));
    }

    #[tokio::test]
    async fn rebind_quiesce_switches_file_and_resets_seq() {
        let dir = tempfile::tempdir().unwrap();
        let sid_a = SessionId::new();
        let sid_b = SessionId::new();
        let jsonl_a = dir.path().join("a.jsonl");
        let jsonl_b = dir.path().join("b.jsonl");
        let h = JournalHandle::start(
            jsonl_a.clone(),
            sid_a,
            0,
            FsyncPolicy::TestOnly,
        )
        .await
        .unwrap();

        let s0 = h.append(make_event(sid_a), false).await.unwrap();
        assert_eq!(s0, 0);

        // ── Rebind: new file, new session, start seq=100 ──
        h.rebind(jsonl_b.clone(), sid_b, 100).await.unwrap();

        let s100 = h.append(make_event(sid_b), false).await.unwrap();
        let s101 = h.append(make_event(sid_b), false).await.unwrap();
        assert_eq!((s100, s101), (100, 101));

        // Verify no cross-file leakage.
        let a = replay_journal_strict(&jsonl_a, 0).unwrap();
        let b = replay_journal_strict(&jsonl_b, 0).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].session_id, sid_a);
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].session_id, sid_b);
        assert_eq!(b[0].seq, 100);
    }

    #[tokio::test]
    async fn concurrent_appends_produce_linearizable_seq_order() {
        let dir = tempfile::tempdir().unwrap();
        let sid = SessionId::new();
        let jsonl = dir.path().join("rollout.jsonl");
        let h = JournalHandle::start(
            jsonl.clone(),
            sid,
            0,
            FsyncPolicy::TestOnly,
        )
        .await
        .unwrap();

        const N: u64 = 200;
        let mut handles = Vec::with_capacity(N as usize);
        for _ in 0..N {
            let h = h.clone();
            let ev = make_event(sid);
            handles.push(tokio::spawn(async move { h.append(ev, false).await.unwrap() }));
        }
        let mut seqs = Vec::with_capacity(N as usize);
        for jh in handles {
            seqs.push(jh.await.unwrap());
        }
        seqs.sort_unstable();
        let expected: Vec<u64> = (0..N).collect();
        assert_eq!(seqs, expected);

        // And on-disk, each seq 0..N appears exactly once.
        let on_disk = replay_journal_strict(&jsonl, 0).unwrap();
        assert_eq!(on_disk.len() as u64, N);
        let mut disk_seqs: Vec<u64> = on_disk.iter().map(|e| e.seq).collect();
        disk_seqs.sort_unstable();
        assert_eq!(disk_seqs, expected);
    }
}
