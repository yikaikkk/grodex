use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command as StdCommand, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use grodex_protocol::acp::{Command as AcpCommand, EventEnvelope, SessionSnapshotPayload};
use grodex_protocol::{ClientFrame, ServerFrame};

pub struct StdioClient {
    child: Child,
    stdin: Box<dyn Write + Send>,
    /// Lines received from agent stdout, delivered via a channel by a
    /// background reader thread. This keeps `poll_event` non-blocking.
    stdout_rx: mpsc::Receiver<String>,
    stderr_rx: mpsc::Receiver<String>,
    pending_logs: Vec<String>,
    last_snapshot: Option<SessionSnapshotPayload>,
    /// Queue of snapshots received from the agent — drained by the TUI
    /// via `take_snapshots()` and translated into `ChatMessage`s so the
    /// chat history can be visually "replayed" after `/resume` (same UX
    /// as Claude Code / Codex). We queue them rather than returning them
    /// inline from `poll_event` because a snapshot isn't an event and
    /// must be handled on the state-management side.
    pending_snapshots: Vec<SessionSnapshotPayload>,
    max_inflight_events: u32,
    inflight_events: u32,
    rtt_ms: Option<u64>,
    protocol_errors: Vec<String>,
    /// Internal event queue. When multiple ServerFrames arrive in a single
    /// `poll_event` read cycle (common during streaming — multiple
    /// TextDelta chunks + TurnComplete can land between polls), we buffer
    /// them here and return one per `poll_event` call. Previously the
    /// extras were silently dropped into `pending_logs`, which caused
    /// `TurnComplete` to be lost and the "⏳ streaming…" indicator to
    /// stick forever.
    event_queue: Vec<EventEnvelope>,
    /// Highest event seq number we've consumed (returned from poll_event).
    /// Used to send `ClientFrame::Ack` so the agent can release backpressure.
    /// Without ACKs, the agent's `inflight = seq - client_last_consumed`
    /// grows unboundedly; after 128 events the agent starts sleeping 10ms
    /// per event, making streaming extremely slow.
    last_consumed_seq: u64,
    /// Dirty flag: set when last_consumed_seq advances, cleared after
    /// sending an Ack. Avoids spamming Ack frames on every poll.
    ack_dirty: bool,
}

impl StdioClient {
    pub fn spawn_agent_subprocess(cmd: &str, args: &[&str]) -> Result<Self> {
        let mut child = StdCommand::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("无法启动 agent 进程: {cmd} {}", args.join(" ")))?;

        let stdin = Box::new(child.stdin.take().ok_or_else(|| anyhow!("无法获取子进程 stdin"))?);
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("无法获取子进程 stdout"))?;
        let stderr = child.stderr.take();

        // Background thread: read stdout lines → channel (non-blocking try_recv)
        let (stdout_tx, stdout_rx) = mpsc::channel::<String>();
        let stdout_reader = BufReader::new(stdout);
        std::thread::spawn(move || {
            for line in stdout_reader.lines() {
                match line {
                    Ok(l) => {
                        if stdout_tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Background thread: read stderr lines → channel
        let (stderr_tx, stderr_rx) = mpsc::channel::<String>();
        if let Some(stderr) = stderr {
            let stderr_reader = BufReader::new(stderr);
            std::thread::spawn(move || {
                for line in stderr_reader.lines() {
                    match line {
                        Ok(l) => {
                            if stderr_tx.send(l).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        Ok(Self {
            child,
            stdin,
            stdout_rx,
            stderr_rx,
            pending_logs: Vec::new(),
            last_snapshot: None,
            pending_snapshots: Vec::new(),
            max_inflight_events: 64,
            inflight_events: 0,
            rtt_ms: None,
            protocol_errors: Vec::new(),
            event_queue: Vec::new(),
            last_consumed_seq: 0,
            ack_dirty: false,
        })
    }

    pub fn send_acp_command(&mut self, c: &AcpCommand) -> Result<()> {
        // ResolveApproval and ResolveIndeterminate are critical-path
        // commands: they unblock the agent's permission gate / crash
        // recovery flow. If we apply backpressure to them, the agent
        // can get stuck waiting while the TUI waits for the agent to
        // drain — a classic deadlock. Exempt them from the inflight cap.
        let is_approval = matches!(c, AcpCommand::ResolveApproval(_) | AcpCommand::ResolveIndeterminate(_));

        if !is_approval {
            let mut retries = 0usize;
            // Wait up to 500ms (50 × 10ms) for the agent to drain before
            // giving up. Subagent spawning can take a few hundred ms;
            // the old 100ms (10 retries) was too aggressive and caused
            // "背压限制 inflight=1 > max_inflight=1" errors when the
            // user tried to approve delegate_task tool calls.
            let max_retries = 50usize;
            loop {
                if self.inflight_events < self.max_inflight_events {
                    break;
                }
                if retries >= max_retries {
                    return Err(anyhow!(
                        "背压限制：inflight={} 超过 max_inflight={}",
                        self.inflight_events,
                        self.max_inflight_events
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
                retries += 1;
            }
        }

        let frame = ClientFrame::Command { inner: c.clone() };
        let line = serde_json::to_string(&frame).context("序列化 ClientFrame 失败")?;
        writeln!(self.stdin, "{line}").context("写入子进程 stdin 失败")?;
        self.stdin.flush().context("flush stdin 失败")?;
        if !is_approval {
            self.inflight_events += 1;
        }
        Ok(())
    }

    pub fn poll_event(&mut self, _timeout: Duration) -> Option<EventEnvelope> {
        // Drain stderr lines (non-blocking).
        while let Ok(line) = self.stderr_rx.try_recv() {
            let line = line.trim();
            if !line.is_empty() {
                self.pending_logs
                    .push(format!("[agent-stderr] {}", truncate_str(line, 300)));
            }
        }

        // Drain ALL available stdout lines into the internal event queue.
        // Previously this loop only kept the first event and silently
        // dropped the rest into pending_logs ("extra event"), which caused
        // TurnComplete to be lost when it arrived in the same read batch
        // as a TextDelta — leading to the "⏳ streaming…" stuck forever
        // bug. Now every parsed event is queued and returned one-per-call.
        while let Ok(line) = self.stdout_rx.try_recv() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if line.len() > 1024 * 1024 {
                self.pending_logs
                    .push(format!("[tui transport] line too long: {} bytes", line.len()));
                continue;
            }
            match serde_json::from_str::<ServerFrame>(line) {
                Ok(frame) => {
                    if let Some(env) = self.handle_server_frame(frame) {
                        self.event_queue.push(env);
                    }
                }
                Err(_) => {
                    if let Ok(env) = serde_json::from_str::<EventEnvelope>(line) {
                        self.inflight_events = self.inflight_events.saturating_sub(1);
                        self.event_queue.push(env);
                    } else {
                        self.pending_logs.push(format!(
                            "无法解析 stdout 行: {}",
                            truncate_str(line, 200)
                        ));
                    }
                }
            }
        }

        // Return the next queued event (FIFO order preserves causal
        // sequencing: TextDelta before TurnComplete).
        if let Some(env) = self.event_queue.first().cloned() {
            self.event_queue.remove(0);
            // Track highest seq for ACK. The agent uses
            // `inflight = next_seq - client_last_consumed` for
            // backpressure; without ACKs it grows unboundedly and the
            // agent starts sleeping 10ms/event after 128 inflight.
            if env.seq > self.last_consumed_seq {
                self.last_consumed_seq = env.seq;
                self.ack_dirty = true;
            }
            // Send ACK proactively when dirty — cheap (one writeln) and
            // keeps the agent's inflight window clear so streaming
            // TextDelta chunks don't hit backpressure.
            if self.ack_dirty {
                let _ = self.send_ack();
            }
            Some(env)
        } else {
            None
        }
    }

    /// Send a `ClientFrame::Ack` with the highest consumed seq number.
    /// This tells the agent it can release backpressure — without this,
    /// the agent's inflight counter grows without bound and it starts
    /// sleeping 10ms per event after 128 pending, making streaming
    /// extremely slow (the "output very slow" bug).
    fn send_ack(&mut self) -> Result<()> {
        let frame = ClientFrame::Ack {
            last_consumed_seq: self.last_consumed_seq,
        };
        let line = serde_json::to_string(&frame)
            .context("序列化 Ack 失败")?;
        writeln!(self.stdin, "{line}").context("写入 Ack 失败")?;
        self.stdin.flush().context("flush Ack 失败")?;
        self.ack_dirty = false;
        Ok(())
    }

    fn handle_server_frame(&mut self, frame: ServerFrame) -> Option<EventEnvelope> {
        // 服务端主动心跳：不是任何 Command 的响应，
        // 不能扣减 inflight（否则背压窗口会被慢慢磨穿）。
        if matches!(frame, ServerFrame::Ping { .. }) {
            return None;
        }
        // Each ServerFrame corresponds to exactly one inbound Command (1:1
        // pairing for ACP). **All** frame kinds decrement inflight:
        // previously only `Event` subtracted 1, so a ResumeSession command
        // (whose response is a `Snapshot` + optionally N replay `Event`s
        // plus a flow control) left inflight stuck at 1 indefinitely, which
        // caused the classic "背压限制 inflight=1 > max_inflight=1" error
        // on the very next user prompt after /resume.
        self.inflight_events = self.inflight_events.saturating_sub(1);
        match frame {
            ServerFrame::Event(env) => {
                Some(env)
            }
            ServerFrame::Snapshot(payload) => {
                self.last_snapshot = Some(payload.clone());
                self.pending_snapshots.push(payload);
                None
            }
            ServerFrame::FlowControl {
                inflight_events,
                requested_pause_ms,
            } => {
                self.max_inflight_events = inflight_events.max(4);
                if let Some(ms) = requested_pause_ms {
                    self.pending_logs
                        .push(format!("[transport] FlowControl: 暂停 {ms}ms"));
                }
                None
            }
            ServerFrame::Pong {
                ping_sent_at_ms,
                pong_at_ms,
            } => {
                let rtt = pong_at_ms.saturating_sub(ping_sent_at_ms);
                self.rtt_ms = Some(rtt);
                None
            }
            ServerFrame::Ping { .. } => None, // 已在函数开头提前返回，此分支不可达
            ServerFrame::ProtocolError {
                code,
                message,
                reference_command_id,
            } => {
                let err = format!(
                    "[ProtocolError] code={}, ref_cmd={:?}, msg={}",
                    code,
                    reference_command_id.as_deref(),
                    truncate_str(&message, 300)
                );
                self.pending_logs.push(err.clone());
                self.protocol_errors.push(err);
                None
            }
        }
    }

    pub fn take_pending_logs(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_logs)
    }

    pub fn last_snapshot(&self) -> Option<&SessionSnapshotPayload> {
        self.last_snapshot.as_ref()
    }

    /// Drain all snapshots received since the last call. Intended to be
    /// called from the TUI main loop after `poll_event` so the UI can
    /// rebuild its chat history (snapshot → ChatMessage vector).
    pub fn take_snapshots(&mut self) -> Vec<SessionSnapshotPayload> {
        std::mem::take(&mut self.pending_snapshots)
    }

    pub fn take_protocol_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.protocol_errors)
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        // Graceful shutdown path:
        //   1) Close the agent's stdin pipe → serve_acp's read loop sees
        //      EOF, breaks out, sends SessionCommand::Shutdown →
        //      supervisor.run() drains into shutdown() which runs
        //      rollout-extractor + blob reclaim + empty-dir cleanup.
        //   2) Busy-poll try_wait() up to a bounded window; if the agent
        //      exits itself we're done cleanly.
        //   3) Otherwise SIGKILL as a hard fallback.
        //
        // The previous drop used `child.kill()` unconditionally —
        // supervisor.shutdown() was never invoked on the TUI (ACP) path,
        // so session-close evidence extraction was effectively disabled.
        // (The CLI REPL path was fine because it awaited supervisor_task.)

        // Explicitly drop the Write side of the stdin pipe so the agent's
        // `next_line().await` returns Ok(None) / EOF. We replace the
        // boxed writer with a no-op so subsequent accidental writes in
        // sibling drops don't panic.
        {
            let old_stdin =
                std::mem::replace(&mut self.stdin, Box::new(NoopWrite) as Box<dyn Write + Send>);
            drop(old_stdin);
        }

        const GRACE_MS: u64 = 1500;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(GRACE_MS);
        let mut exited = false;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => { exited = true; break; }
                Ok(None) => {}
                Err(_) => break,
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !exited {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct NoopWrite;
impl std::io::Write for NoopWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> { Ok(buf.len()) }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut r: String = s.chars().take(max).collect();
        r.push('…');
        r
    }
}

pub fn run_with_stdio_transport(agent_cmd: &str, agent_args: &[String]) -> Result<()> {
    let args_vec: Vec<&str> = agent_args.iter().map(|s| s.as_str()).collect();
    let client = StdioClient::spawn_agent_subprocess(agent_cmd, &args_vec)
        .map_err(|e| anyhow!("无法启动 agent（{agent_cmd}）。请先 cargo build -p grodex-cli，或用 --agent-cmd 指定 grodex 可执行文件路径: {e}"))?;
    let tui = crate::GrodexTui::init_with(client)?;
    tui.run_blocking()
}
