pub mod ui;
pub mod transport;

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use crossterm::event::{self, Event as CrosstermEvent};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use grodex_core::id::SessionId;
use grodex_protocol::acp::{
    ApprovalResolution, Command, EventEnvelope, ReplayCursor, ReplayMode, ResolveApprovalCommand,
    ResumeSessionCommand, SessionCancel, SessionPrompt,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// 是否当前 TUI 仍拥有终端（已进入 alternate screen + 开了鼠标捕获）。
/// signal handler / panic hook / drop guard 都会读取这个 flag，保证
/// teardown 序列只跑一次，且进程退出前终端一定会被复位。
static TERMINAL_OWNED: AtomicBool = AtomicBool::new(false);

/// 兜底的终端复位序列。与 grok 的 MOUSE_PASTE_RESET 一致：
///   CSI ? 25 h    显示光标
///   CSI ? 1006 l  关闭 SGR 鼠标模式
///   CSI ? 1003 l  关闭所有鼠标上报（含拖动）
///   CSI ? 1002 l  关闭事件鼠标拖动
///   CSI ? 1000 l  关闭普通鼠标上报
///   CSI ? 1007 l  退出 alternate scroll mode
///   CSI ? 1049 l  退出 alternate screen + 清空 buffer
///   CSI ? 2004 l  退出 bracketed paste
const TERMINAL_RESET_ESCAPE: &[u8] =
    b"\x1b[?25h\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1007l\x1b[?1049l\x1b[?2004l";

/// 启动时发送：确保所有鼠标上报模式都关闭，防止上一个程序残留。
/// 不含 1007（alternate scroll），那是在 EnterAlternateScreen 之后单独开启。
const DISABLE_ALL_MOUSE: &[u8] = b"\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

/// DECSET 1007: Alternate Scroll Mode.
/// 在 alt-screen 模式下让终端把滚轮事件翻译成方向键（↑/↓），
/// 而不是鼠标坐标事件。这样 TUI 既能响应滚轮滚动，又不捕获鼠标，
/// 终端原生的文本选择（拖拽选中 + 右键复制）始终可用。
/// 参考 codex-rs tui/src/tui.rs:240-280 的 EnableAlternateScroll。
const ENABLE_ALT_SCROLL: &[u8] = b"\x1b[?1007h";

fn hard_terminal_reset() {
    // 不用 crossterm Command：panic context 里不能分配/unwrap。
    // 直接写裸字节 + flush，失败也不处理（进程即将死亡）。
    let mut stderr = io::stderr();
    let _ = io::Write::write_all(&mut stderr, TERMINAL_RESET_ESCAPE);
    let _ = io::Write::flush(&mut stderr);
    let _ = disable_raw_mode();
    TERMINAL_OWNED.store(false, Ordering::Release);
}

fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // 先复位终端，再跑原 hook（否则 hook 里打印错误信息时，
        // 终端仍在 alternate screen + raw mode，输出直接丢失）。
        if TERMINAL_OWNED.load(Ordering::Acquire) {
            hard_terminal_reset();
        }
        prev(info);
    }));
}

use ui::event_handler::{handle_key, TuiAction};
use ui::state::SlashLocalKind;
use ui::layout::{approvals_desired_rows, build_layout, prompt_desired_rows, turn_status_desired_rows};
use ui::render::render_full;
use ui::state::TuiAppState;

pub trait TransportAdapter {
    fn send_command(&mut self, cmd: Command) -> Result<()>;
    fn poll_event(&mut self, timeout: Duration) -> Option<EventEnvelope>;
    fn take_pending_logs(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// Session snapshots received from the agent. Used by `/resume` to
    /// rebuild chat history in the UI (SnapshotThenLive mode). Default
    /// empty so in-process / unit-test transports don't need to wire it.
    fn take_snapshots(&mut self) -> Vec<grodex_protocol::acp::SessionSnapshotPayload> {
        Vec::new()
    }
}

pub struct GrodexTui {
    terminal: Terminal<CrosstermBackend<io::Stderr>>,
    state: TuiAppState,
    transport: Box<dyn TransportAdapter>,
}

impl GrodexTui {
    pub fn init_with<T: TransportAdapter + 'static>(transport: T) -> Result<Self> {
        // 参考 grok：全部终端 I/O 走 stderr。ratatui 的渲染也是
        // write ANSI escape 到 backend；如果 backend 和 alternate screen
        // 控制使用同一个 stream (stderr)，就不会出现一半命令走 stdout
        // 一半走 stderr 导致的时序错乱（这正是 macOS Terminal.app 上
        // 鼠标 CSI < ... M 泄漏到 shell 的根因）。
        let stderr = io::stderr();
        let backend = CrosstermBackend::new(stderr);
        let terminal = Terminal::new(backend).context("初始化 Terminal 失败")?;
        let mut state = TuiAppState::new();

        // Read model/provider from ~/.grodex/config.toml for header display.
        if let Some(home) = std::env::var_os("HOME") {
            let config_path = std::path::Path::new(&home).join(".grodex/config.toml");
            if let Ok(content) = std::fs::read_to_string(&config_path) {
                for line in content.lines() {
                    let line = line.trim();
                    if let Some(val) = line.strip_prefix("provider") {
                        if let Some(v) = val.trim().strip_prefix('=').map(|s| s.trim().trim_matches('"')) {
                            state.provider_label = v.to_string();
                        }
                    }
                    if let Some(val) = line.strip_prefix("model") {
                        if let Some(v) = val.trim().strip_prefix('=').map(|s| s.trim().trim_matches('"')) {
                            state.model_label = v.to_string();
                        }
                    }
                }
            }
        }

        Ok(Self {
            terminal,
            state,
            transport: Box::new(transport),
        })
    }

    pub fn run_blocking(mut self) -> Result<()> {
        install_panic_hook();
        enable_raw_mode().context("enable_raw_mode 失败")?;
        let _raw_guard = RawModeGuard;
        // 参考 grok：终端控制命令全部写入 stderr 而非 stdout。
        // macOS Terminal.app / iTerm2 / Linux VTE 上 alternate screen + mouse
        // 命令如果走 stdout，可能因为 stdout 被管道/缓冲而丢失顺序，导致：
        //   1. 退出时 LeaveAlternateScreen 没有 flush，终端停在 alternate screen
        //   2. 鼠标事件序列（CSI < M）作为原始文本泄漏到用户 shell
        let mut stderr = io::stderr();
        // 进入前先关闭残留鼠标模式。
        let _ = io::Write::write_all(&mut stderr, DISABLE_ALL_MOUSE);
        let _ = io::Write::flush(&mut stderr);
        stderr
            .execute(EnterAlternateScreen)
            .context("进入 alternate screen 失败")?;
        // 进入 alternate screen 后再次关闭所有鼠标上报模式。
        // 某些终端在切换到 alternate buffer 时会恢复默认的 mouse reporting
        // 状态，必须在这里再关一次，否则点击会出现整行灰色高亮。
        let _ = io::Write::write_all(&mut stderr, DISABLE_ALL_MOUSE);
        let _ = io::Write::flush(&mut stderr);
        // 启用 DECSET 1007 (Alternate Scroll Mode)：滚轮 → 方向键。
        // 不捕获鼠标，终端原生的文本选择始终可用。参考 codex-rs 的设计。
        let _ = io::Write::write_all(&mut stderr, ENABLE_ALT_SCROLL);
        let _ = io::Write::flush(&mut stderr);
        // 开启 bracketed paste：用户在输入框 Ctrl-V/Cmd-V 粘贴时，终端会
        // 发送 `CrosstermEvent::Paste(String)`，我们把它追加到输入框光标
        // 位置，而不是把一大段原始 CSI 文本塞进 input_buffer。
        let _ = stderr.execute(crossterm::event::EnableBracketedPaste);
        TERMINAL_OWNED.store(true, Ordering::Release);
        let _alt_guard = AltScreenGuard;

        let mut last_command_id: u64 = 0;
        let mut next_cmd_id = || -> String {
            last_command_id += 1;
            format!("cmd-tui-{}-{}", std::process::id(), last_command_id)
        };

        loop {
            for log in self.transport.take_pending_logs() {
                self.state.push_log(log);
            }

            // Drain ALL available events from the transport in a tight
            // loop. Previously only ONE event was processed per iteration,
            // which meant each event waited ~10ms (the crossterm poll
            // timeout) before the next was handled. During streaming this
            // caused visible stutter and, combined with the old
            // poll_event bug (dropping extra events), could lose
            // TurnComplete entirely. Now we drain everything the transport
            // has before touching the keyboard.
            while let Some(env) = self.transport.poll_event(Duration::from_millis(0)) {
                let env_sid = env.session_id.to_string();
                let content = env.content.clone();
                self.state
                    .push_event_with_envelope(content, env.seq, env.generation);
                if self.state.session_id.is_none() {
                    self.state.session_id = Some(env_sid);
                }
            }

            // ── Rebuild chat history from snapshots received from agent.
            // Triggered by `/resume` (agent sends SnapshotThenLive). Each
            // snapshot captures the complete terminal state of every
            // message up to last_seq, so we can rebuild the UI as if the
            // user is scrolling through an existing conversation — just
            // like Claude Code / Codex / Aider do on attach.
            for snap in self.transport.take_snapshots() {
                // Skip empty snapshots outright. These can arrive as a benign
                // "initial empty state" heartbeat from a brand-new session
                // when the agent was started in parallel with the resume
                // flow. If we processed them, restored would be empty and
                // we would touch nothing — but an empty snapshot over a
                // non-empty one is still confusing because it still emits
                // a transport log line. Early-skip reduces noise and
                // guarantees items=0 never overwrites restored history.
                if snap.items.is_empty() {
                    continue;
                }
                // If the snapshot's session id is available, adopt it so
                // subsequent TurnComplete / TextDelta events still match.
                if self.state.session_id.is_none() {
                    self.state.session_id = Some(snap.session_id.to_string());
                }
                // Wipe the existing empty turn so the restored messages
                // sit in their own turns and are not appended to the
                // current empty turn (the turn separator logic groups by
                // user→assistant transition).
                let base = std::time::Instant::now();
                let mut restored: Vec<crate::ui::state::ChatMessage> =
                    Vec::with_capacity(snap.items.len());
                for item in &snap.items {
                    match item.item_type.as_str() {
                        "user" => {
                            restored.push(crate::ui::state::ChatMessage::User {
                                text: item.content.clone(),
                            });
                        }
                        "assistant" => {
                            restored.push(crate::ui::state::ChatMessage::Assistant {
                                text: item.content.clone(),
                                done: item.complete,
                            });
                        }
                        "thinking" => {
                            restored.push(crate::ui::state::ChatMessage::Thinking {
                                text: item.content.clone(),
                                done: item.complete,
                            });
                        }
                        "tool_call" => {
                            match serde_json::from_str::<serde_json::Value>(&item.content) {
                                Ok(v) => {
                                    let name = v
                                        .get("name")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let call_id = v
                                        .get("call_id")
                                        .and_then(|x| x.as_str())
                                        .map(|s| s.to_string());
                                    let args = v
                                        .get("arguments")
                                        .cloned()
                                        .unwrap_or(serde_json::Value::Null)
                                        .to_string();
                                    restored.push(crate::ui::state::ChatMessage::Tool {
                                        name,
                                        call_id,
                                        args,
                                        result: None,
                                        is_error: false,
                                        done: item.complete,
                                        has_result: false,
                                        started_at: base,
                                        finished_at: None,
                                    });
                                }
                                Err(e) => {
                                    self.state.push_log(format!(
                                        "[snapshot] 无法解析 tool_call item {}: {e}",
                                        item.item_id
                                    ));
                                }
                            }
                        }
                        "tool_result" => {
                            match serde_json::from_str::<serde_json::Value>(&item.content) {
                                Ok(v) => {
                                    let call_id =
                                        v.get("call_id").and_then(|x| x.as_str()).unwrap_or("");
                                    let content_v =
                                        v.get("content").cloned().unwrap_or(serde_json::Value::Null);
                                    // tool result payload is stored as
                                    // serialized content — strip quotes if
                                    // it's a JSON-encoded string so humans
                                    // see plain text.
                                    let text = match content_v {
                                        serde_json::Value::String(s) => s,
                                        other => other.to_string(),
                                    };
                                    let is_error = v
                                        .get("is_error")
                                        .and_then(|x| x.as_bool())
                                        .unwrap_or(false);
                                    // Pair the result with the matching
                                    // prior tool_call via call_id. Fallback:
                                    // append a synthetic Tool card with no
                                    // args but result filled (can happen if
                                    // journal missed the prepared entry
                                    // because it was filtered out).
                                    let paired = restored.iter_mut().rev().find(|m| {
                                        matches!(m, crate::ui::state::ChatMessage::Tool {
                                            call_id: Some(c), ..
                                        } if c == call_id)
                                    });
                                    match paired {
                                        Some(crate::ui::state::ChatMessage::Tool {
                                            result: slot,
                                            is_error: err_slot,
                                            has_result: hr_slot,
                                            finished_at: fa_slot,
                                            ..
                                        }) => {
                                            *slot = Some(text);
                                            *err_slot = is_error;
                                            *hr_slot = true;
                                            *fa_slot = Some(base);
                                        }
                                        _ => {
                                            restored.push(
                                                crate::ui::state::ChatMessage::Tool {
                                                    name: String::new(),
                                                    call_id: Some(call_id.to_string()),
                                                    args: String::new(),
                                                    result: Some(text),
                                                    is_error,
                                                    done: true,
                                                    has_result: true,
                                                    started_at: base,
                                                    finished_at: Some(base),
                                                },
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    self.state.push_log(format!(
                                        "[snapshot] 无法解析 tool_result item {}: {e}",
                                        item.item_id
                                    ));
                                }
                            }
                        }
                        other => {
                            self.state.push_log(format!(
                                "[snapshot] 忽略未知 item_type '{other}' id={}",
                                item.item_id
                            ));
                        }
                    }
                }
                // Diagnostics: when item count does not match restored, some
                // items were dropped via the `other` branch (e.g. a typo in
                // the item_type string sent by the agent). This log lets the
                // user immediately see "expected 9, got 0" without guessing
                // why the chat pane stays empty after a resume.
                if restored.len() != snap.items.len() {
                    self.state.push_log(format!(
                        "[snapshot] 警告：snap.items.len()={}，仅解析出 {} 条 ChatMessage（其余因未知 item_type 被忽略，见上方日志）",
                        snap.items.len(),
                        restored.len()
                    ));
                }
                if !restored.is_empty() {
                    // ── Full state reconciliation (Claude/Codex-style attach) ─
                    //
                    // The snapshot represents a complete UI snapshot — not
                    // just the message list. If we only overwrite
                    // `messages` but leave stale call_id_index / events /
                    // pending_approvals the TUI can get confused:
                    //   • TextDelta / ToolResult routing uses call_id_index
                    //     → stale indices target the wrong Tool card after
                    //     resume.
                    //   • Duplicate-detection uses next_seq → needs to
                    //     match snap.last_seq + 1 so post-snapshot live
                    //     events (after ResumeSession ACK) don't get
                    //     dropped.
                    //   • turn_id / capability_generation drive the header
                    //     line + server-side expectations, so sync them
                    //     too.
                    //   • Pending approvals are from the PREVIOUS agent
                    //     session; after a resume the agent's permission
                    //     broker is re-created from scratch, so any left-
                    //     -over pending tickets would need to be re-issued.
                    //     Clearing avoids ghost "approve this tool?" rows.
                    //
                    // If the current session already has LOCAL messages
                    // (e.g. user typed a few lines and THEN ran /resume —
                    // uncommon but can happen) we keep those in-front of
                    // the restored history (merge oldest-first) so the
                    // turn-based renderer still anchors correctly. The
                    // common case (TUI opened → first action is /resume)
                    // means local messages.len() == 0, so this degrades
                    // cleanly to a simple replace.
                    let mut local_messages: Vec<crate::ui::state::ChatMessage> =
                        std::mem::take(&mut self.state.messages);
                    if local_messages.is_empty() {
                        // Fast path (common): clean attach with no prior
                        // local state. Since we're replacing the messages
                        // wholesale we can also safely scrub all the
                        // sibling state that referenced OLD message indices
                        // / OLD event seq numbers.
                        self.state.events.clear();
                        self.state.call_id_index.clear();
                        self.state.pending_approvals.clear();
                        self.state.turn_id = snap.current_turn_id.clone();
                        self.state.capability_generation = snap.generation;
                        self.state.messages = restored;
                    } else {
                        // Slow path: user typed something before running
                        // /resume. Keep local (newest) messages AFTER the
                        // restored (older) history so renders still look
                        // chronological. We DO NOT clear call_id_index /
                        // events here because the local messages might be
                        // mid-turn (in-flight Assistant / Tool).
                        let mut merged = restored;
                        merged.append(&mut local_messages);
                        self.state.messages = merged;
                        // Even in the merged path, reconcile sequence
                        // numbers + turn/generation fields — the snapshot
                        // represents what the SERVER knows, and live
                        // events coming back after the ACK will use server-
                        // side seqs.
                        self.state.turn_id = snap.current_turn_id.clone();
                        self.state.capability_generation = snap.generation;
                    }
                    // Reconcile next_seq with the snapshot so future events
                    // don't get dropped as duplicates.
                    self.state.set_next_seq(snap.last_seq + 1);
                    // /resume 完成后显式锁定到最底部，用户直接看到最后的
                    // assistant / tool 输出，与 codex attach 体验一致。
                    self.state.scroll_follow_bottom = true;
                    self.state.scroll_conversation = u16::MAX;
                } else {
                    // Previously silent. If a snapshot *claimed* to have
                    // items but 0 produced ChatMessages, we yell about it
                    // so users immediately know the mismatch instead of
                    // filing a "resume does not show anything" bug.
                    self.state.push_log(format!(
                        "[snapshot] 警告：items.len()={}，但 restored=0 → 没有解析出任何可显示的聊天消息，请检查 agent 发送的 item_type",
                        snap.items.len()
                    ));
                }
            }

            // ── Render EVERY iteration (after draining events, before
            // polling keyboard). Previously draw() was at the BOTTOM of
            // the loop, AFTER `if !poll_ok { continue; }`. During
            // streaming there are no keyboard events, so the continue
            // skipped draw() entirely — the screen never updated until a
            // key was pressed, causing the "一大片一大片输出" symptom:
            // all accumulated TextDelta chunks would appear at once when
            // the user finally typed something. Moving draw() here
            // ensures the screen reflects every batch of events
            // immediately, giving true SSE-style incremental rendering.
            let draw_res = self.terminal.draw(|f| {
                let area = f.size();
                let approvals_rows = approvals_desired_rows(self.state.pending_approvals.len());
                let turn_status_rows = turn_status_desired_rows(
                    self.state.is_streaming(), self.state.active_tool_count());
                let wrap_w = (area.width as usize).saturating_sub(7).max(20);
                let content_rows = self.state.prompt_content_lines(wrap_w);
                let slash_rows = self.state.slash_menu_rows();
                let prompt_rows = prompt_desired_rows(content_rows + slash_rows);
                let layout = build_layout(area, approvals_rows, turn_status_rows, prompt_rows);
                render_full(f, &mut self.state, &layout);
            });
            if let Err(e) = draw_res {
                self.state.push_log(format!("[render] draw 错误（已跳过）：{e}"));
            }

            // During streaming, poll keyboard with a 1ms timeout so new
            // TextDelta chunks are picked up almost immediately. When idle,
            // 10ms is fine and saves CPU. Previously the fixed 10ms timeout
            // added noticeable latency to each streaming chunk.
            let poll_ms = if self.state.is_streaming() { 1 } else { 10 };
            // 参考 grok event_loop.rs:1472-1517：poll/read 错误不能直接 ? 退出，
            // 否则 VTE 终端 / macOS Terminal.app 鼠标滚轮产生的序列被 crossterm
            // 解析失败时，? 会传播错误导致整个 TUI 直接关闭。
            // grok 的做法是跳过错误继续，只在连续 50 次错误后才放弃。
            let poll_ok = event::poll(Duration::from_millis(poll_ms)).unwrap_or(false);
            if !poll_ok {
                continue;
            }
            let ev = match event::read() {
                Ok(ev) => ev,
                Err(e) => {
                    // 跳过解析错误（垃圾序列、不完整鼠标事件等），不杀 TUI。
                    self.state.push_log(format!("[crossterm] 事件读取错误（已跳过）：{e}"));
                    continue;
                }
            };
            match ev {
                    CrosstermEvent::Key(key) => {
                        let action = handle_key(key, &mut self.state);
                        if let Some(action) = action {
                            match action {
                            TuiAction::Quit => break,
                            TuiAction::SubmitPrompt { text } => {
                                // Show user message in chat view immediately.
                                self.state.push_user_message(&text);
                                // Reset cancel flag for the new turn.
                                self.state.cancel_sent = false;
                                let sid = self
                                    .state
                                    .session_id
                                    .clone()
                                    .and_then(|s| SessionId::from_string(&s).ok())
                                    .unwrap_or_else(SessionId::new);
                                let cmd = Command::Prompt(SessionPrompt {
                                    command_id: next_cmd_id(),
                                    expected_generation: Some(self.state.capability_generation),
                                    idempotency_key: None,
                                    session_id: sid,
                                    text,
                                });
                                if let Err(e) = self.transport.send_command(cmd) {
                                    self.state.push_log(format!("发送 Prompt 失败: {e}"));
                                }
                            }
                            TuiAction::SubmitCommand { cmd } => {
                                handle_cli_command(
                                    &cmd,
                                    &mut self.state,
                                    &mut *self.transport,
                                    &mut next_cmd_id,
                                );
                            }
                            TuiAction::ResolveApproval {
                                ticket_idx,
                                resolution,
                            } => {
                                if let Some(ticket) = self
                                    .state
                                    .pending_approvals
                                    .get(ticket_idx)
                                    .cloned()
                                {
                                    let issued_at_ms = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_millis() as u64)
                                        .unwrap_or(0);
                                    let cmd =
                                        Command::ResolveApproval(ResolveApprovalCommand {
                                            command_id: next_cmd_id(),
                                            expected_generation: Some(
                                                self.state.capability_generation,
                                            ),
                                            idempotency_key: None,
                                            ticket_id: ticket.ticket_id.clone(),
                                            resolution: resolution.clone(),
                                            issued_by: Some("grodex-tui".into()),
                                            issued_at_ms,
                                        });
                                    if let Err(e) = self.transport.send_command(cmd) {
                                        self.state.push_log(format!(
                                            "发送 ResolveApproval 失败: {e}"
                                        ));
                                    } else {
                                        self.state.resolve_ticket(&ticket.ticket_id);
                                    }
                                }
                            }
                            TuiAction::ScrollUp | TuiAction::ScrollDown => {}
                            TuiAction::SwitchApprovalSelection(_) => {}
                            TuiAction::ToggleMode(_) => {}
                            TuiAction::ToggleThinkingExpansion => {}
                            TuiAction::CopyLastAssistant => {
                                // Walk messages in reverse and grab the
                                // most-recent Assistant text. If the
                                // final Assistant is still streaming we
                                // still copy whatever's already visible —
                                // useful for capturing partial answers.
                                let mut picked: Option<String> = None;
                                for m in self.state.messages.iter().rev() {
                                    if let ui::state::ChatMessage::Assistant { text, .. } = m {
                                        if !text.is_empty() {
                                            picked = Some(text.clone());
                                        }
                                        break;
                                    }
                                }
                                if let Some(txt) = picked {
                                    match set_clipboard(&txt) {
                                        Ok(n) => self.state.push_log(format!(
                                            "已复制最后 Grodex 消息到剪贴板（{} 字符）。\n快捷键：Ctrl-Shift-C / macOS Cmd-C", n)),
                                        Err(e) => self.state.push_log(format!(
                                            "[clipboard] 写入失败：{e}\n\
                                             可通过终端菜单或选中文字+右键复制代替。"
                                        )),
                                    }
                                } else {
                                    self.state.push_log("当前没有可复制的 Grodex 消息".to_string());
                                }
                            }
                            TuiAction::CopyInputBuffer => {
                                // Copy whichever buffer is currently
                                // active (prompt / command) to the
                                // system clipboard so users can Cmd-C
                                // mid-draft on macOS. We deliberately do
                                // NOT gate on "selection exists" because
                                // the state struct doesn't track a
                                // selection rect yet; copying the whole
                                // draft matches what users of 90% of
                                // simple textboxes expect.
                                let is_prompt = matches!(self.state.input_mode, ui::state::InputMode::Prompt);
                                let (buf, label) = if is_prompt {
                                    (&self.state.input_buffer, "输入框")
                                } else {
                                    (&self.state.command_buffer, "命令栏")
                                };
                                if buf.is_empty() {
                                    self.state.push_log(format!("{label}为空，跳过复制。"));
                                } else {
                                    match set_clipboard(buf) {
                                        Ok(n) => self.state.push_log(format!(
                                            "已复制{label}内容到剪贴板（{} 字符）。", n
                                        )),
                                        Err(e) => self.state.push_log(format!(
                                            "[clipboard] 写入失败：{e}\n\
                                             备选方案：选中输入文字+终端菜单复制。"
                                        )),
                                    }
                                }
                            }
                            TuiAction::SelectAllInput => {
                                // Select-all in the active input buffer.
                                // With no selection rect yet we move the
                                // cursor to the END so CopyInputBuffer
                                // (which copies the full buffer) still
                                // produces the expected result for a
                                // Cmd-A → Cmd-C sequence; this also
                                // scrolls buffer visualisation to the
                                // tail so users can see everything that
                                // was typed.
                                let is_prompt = matches!(self.state.input_mode, ui::state::InputMode::Prompt);
                                if is_prompt {
                                    self.state.input_cursor = self.state.input_buffer.len();
                                } else {
                                    self.state.command_cursor = self.state.command_buffer.len();
                                }
                            }
                            TuiAction::PasteText { text } => {
                                // Two paths reach here:
                                //   1. CrosstermEvent::Paste(text) — we got
                                //      bracketed paste directly with the
                                //      text. Always preferred.
                                //   2. Ctrl-Shift-V with empty text — the
                                //      terminal didn't forward a Paste
                                //      event, so fall back to reading the
                                //      system clipboard via OSC 52.
                                let mut snippet = text;
                                if snippet.is_empty() {
                                    snippet = get_clipboard().unwrap_or_default();
                                }
                                if !snippet.is_empty() {
                                    // Strip CR from pasted text (keeps \n
                                    // for multi-line pastes via Alt-Enter
                                    // convention) and insert at cursor.
                                    snippet.retain(|c| c != '\r');
                                    let is_prompt = matches!(self.state.input_mode, ui::state::InputMode::Prompt);
                                    let (buf, cur) = if is_prompt {
                                        (&mut self.state.input_buffer, &mut self.state.input_cursor)
                                    } else {
                                        (&mut self.state.command_buffer, &mut self.state.command_cursor)
                                    };
                                    buf.insert_str(*cur, &snippet);
                                    *cur += snippet.len();
                                }
                            }
                            TuiAction::CancelTurn => {
                                // Idempotent guard: once Cancel has been sent,
                                // suppress duplicates until TurnComplete resets
                                // the flag. This prevents the repeated
                                // "已中断当前生成" log + "invalid state
                                // transition: Idle -> Idle" errors when the
                                // user presses Esc multiple times.
                                if !self.state.cancel_sent {
                                let sid = self
                                    .state
                                    .session_id
                                    .clone()
                                    .and_then(|s| SessionId::from_string(&s).ok())
                                    .unwrap_or_else(SessionId::new);
                                let cmd = Command::Cancel(SessionCancel {
                                    command_id: next_cmd_id(),
                                    expected_generation: Some(self.state.capability_generation),
                                    idempotency_key: None,
                                    session_id: sid,
                                });
                                if let Err(e) = self.transport.send_command(cmd) {
                                    self.state.push_log(format!("发送 Cancel 失败: {e}"));
                                } else {
                                    self.state.cancel_sent = true;
                                    // Mark the latest Assistant message as done
                                    // so the streaming indicator stops,
                                    // even if TurnComplete hasn't arrived yet.
                                    for m in self.state.messages.iter_mut().rev() {
                                        if let ui::state::ChatMessage::Assistant { done, .. } = m {
                                            *done = true;
                                            break;
                                        }
                                    }
                                    // Freeze all in-flight tool timers immediately
                                    // so "⏳ working… 3m09s" stops ticking.
                                    self.state.finalize_all_inflight_tools();
                                    self.state.push_log("已中断当前生成".to_string());
                                }
                                } // end if !cancel_sent
                            }
                            TuiAction::RunSlashLocal { kind, args } => {
                                // ══════════════════════════════════════════════════════════
                                // GROK-CONSISTENT: NONE of these branches EVER forwards
                                // the slash command text to the LLM as a prompt. Every
                                // `/xxx` line the user presses Enter on is handled here.
                                // ══════════════════════════════════════════════════════════
                                match kind {
                                    // ── TUI-local: fully implemented ────────────────────
                                    SlashLocalKind::Exit => {
                                        // Local quit — same as 'q' in Normal.
                                        // Break out of the run-loop; the resume
                                        // hint is printed after terminal reset.
                                        let _ = args;
                                        break;
                                    }
                                    SlashLocalKind::DeleteCurrentSession => {
                                        // Empty messages + events. Leaves session id
                                        // + approval state intact so on-going
                                        // permission requests still resolve.
                                        // Grok: /delete /clear. Note: this is
                                        // different from /reset (= clear input only).
                                        let _ = args;
                                        self.state.messages.clear();
                                        self.state.events.clear();
                                        self.state.push_log("已清空聊天记录（会话本地清空）".to_string());
                                    }
                                    SlashLocalKind::ClearInput => {
                                        // /reset — clear ONLY the current input
                                        // buffer, leave chat history untouched.
                                        let _ = args;
                                        self.state.input_buffer.clear();
                                        self.state.input_cursor = 0;
                                        self.state.recompute_slash_menu();
                                    }
                                    SlashLocalKind::Mouse { .. } => {
                                        // 不再捕获鼠标，改用 DECSET 1007 alternate scroll mode。
                                        // 滚轮和文本选择始终同时可用，无需切换。
                                        self.state.push_log(
                                            "鼠标模式（始终可用，无需切换）：\n  · 滚轮上下滑动 → 滚动对话（终端翻译为方向键）\n  · 鼠标拖拽选中文本 → 终端原生处理\n  · 右键复制 → 终端原生处理\n  · 键盘 j/k 或 Ctrl-PageUp/Down 也可滚动".to_string()
                                        );
                                    }
                                    SlashLocalKind::Help => {
                                        let _ = args;
                                        let help = concat!(
"Grodex TUI 帮助\n",
"━━━━━━━━━━━━━━━━━━\n",
"输入（默认 Prompt 模式，无需按 i）：\n",
"  直接打字即可提问\n",
"  Enter              发送消息\n",
"  Shift+Enter       插入换行（多行草稿）\n",
"  Tab                补全选中的 /命令\n",
"  ↑ / ↓              在 / 下拉菜单中选择（Ctrl+N / Ctrl+P 同样可用）\n",
"  Esc (一次)         关闭 / 菜单\n",
"  Esc (二次)         清空当前输入并切到 Normal 浏览模式\n",
"  Ctrl-U / Ctrl-K    删除到行首 / 删除到行尾\n",
"  ← / →              光标移动\n",
"\n",
"本地命令 ⚡（直接生效，绝不发送给 LLM）：\n",
"  /exit /quit /q         退出 TUI\n",
"  /delete /clear         清空当前会话聊天记录\n",
"  /reset                 清空当前输入框（会话记录保留）\n",
"  /mouse                 显示鼠标模式（滚轮+选择始终同时可用）\n",
"  /help /? /welcome      显示本帮助\n",
"\n",
"会话/ACP 命令 ◈（本地占位，绝不发送给 LLM）：\n",
"  /new /home /chat       新会话 / 返回仪表板\n",
"  /fork /resume <id>     分叉 / 恢复会话\n",
"  /history /sessions     会话历史列表\n",
"  /rewind /undo <id>     回退重生成\n",
"  /compact /recap        压缩 / 总结上下文\n",
"  /model /m <name>       切换模型（ACP 命令，不触发 LLM）\n",
"  /effort /auto /yolo    执行模式切换\n",
"  /multiline /ml         多行输入模式切换\n",
"  /trust on|off          切换工作区信任\n",
"  /provider /cwd         供应商 / 工作目录\n",
"  /mcp /skills /tools    MCP / skills / tools 清单\n",
"  /plan /tasks /queue    计划 / 任务 / 队列\n",
"  /settings /theme       设置 / 主题\n",
"  /doctor /debug         诊断 / 自检 / 日志\n",
"  ... 以及 Grok 全部 70+ 内置命令\n",
"\n",
"⚠️  所有 /xxx 命令均本地处理，不会作为 prompt 发送给 LLM。\n",
"    想真正与模型讨论命令用法，请在输入时用文字描述，不要以 / 开头。\n",
                                        ).to_string();
                                        self.state.push_log(help);
                                    }

                                    // ── ACP / session-scoped commands ──────────────────
                                    // All of these are intercepted locally. If/when the
                                    // agent backend gains a structured `SessionCommand`
                                    // transport frame for these, each branch can send a
                                    // typed ACP command. Until then we emit an explicit
                                    // local diagnostic so the user NEVER silently gets
                                    // an LLM response to their slash invocation.
                                    SlashLocalKind::AcpNewSession => {
                                        self.state.messages.clear();
                                        self.state.events.clear();
                                        self.state.push_log("[ACP] 已新建空白会话（本地清空）".to_string());
                                        let _ = args;
                                    }
                                    SlashLocalKind::AcpExitToDashboard => {
                                        // 本地：等同于 /new。Grodex 无多会话/仪表盘 UI，
                                        // 清空消息给用户一个干净状态。
                                        self.state.messages.clear();
                                        self.state.events.clear();
                                        self.state.session_title = None;
                                        self.state.push_log("（本地）返回仪表板等价于 /new：已清空当前会话。\nGrodex 当前为单会话模式，无需切换。".to_string());
                                        let _ = args;
                                    }
                                    SlashLocalKind::AcpForkSession => {
                                        // 本地 fork：生成新 SessionId，保留 messages & events。
                                        // 无需 ACP 帧即可给用户一个"已分叉"的状态。
                                        use grodex_core::id::SessionId;
                                        let new_sid = SessionId::new();
                                        let sid_s = new_sid.to_string();
                                        self.state.session_id = Some(sid_s.clone());
                                        // 生成一条本地 System 标记分隔线，让用户知道分叉点。
                                        self.state.push_log(format!(
                                            "（本地）会话已 Fork 为新会话\n  新 session_id = {sid_s}\n  保留原消息与事件（{}/{} 条）。\n{hint}",
                                            self.state.messages.len(),
                                            self.state.events.len(),
                                            hint = if args.is_empty() { String::new() } else { format!("args = {args:?}") }
                                        ));
                                    }
                                    SlashLocalKind::AcpResumeSession => {
                                        // ACP ResumeSession 帧存在！优先用协议发送。
                                        // 提取 id：空 args 时提示；否则 args.trim() 作为 session。
                                        let sid_str = args.trim().to_string();
                                        if sid_str.is_empty() {
                                            self.state.push_log(format!(
                                                "用法：/resume <session_id>  或  /fork <session_id>\n\
                                                 colon 模式下：:resume <id>  同样发送 ResumeSession ACP 帧。\n\
                                                 当前会话 ID：{}",
                                                self.state.session_id.clone().unwrap_or_else(|| "<none>".to_string())
                                            ));
                                        } else {
                                            use grodex_core::id::SessionId;
                                            use grodex_protocol::acp::{
                                                ReplayCursor, ReplayMode, ResumeSessionCommand,
                                            };
                                            let sid = SessionId::from_string(&sid_str)
                                                .unwrap_or_else(|_| SessionId::new());
                                            let issued_at_ms = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .map(|d| d.as_millis() as u64)
                                                .unwrap_or(0);
                                            let cmd = Command::ResumeSession(ResumeSessionCommand {
                                                command_id: format!("resume-{}", issued_at_ms),
                                                expected_generation: Some(self.state.capability_generation),
                                                idempotency_key: None,
                                                session_id: sid.to_string(),
                                                resume_from: ReplayCursor {
                                                    last_consumed_seq: self.state.events.last().map(|e| e.seq).unwrap_or(0),
                                                    last_event_id: None,
                                                    mode: ReplayMode::SnapshotThenLive,
                                                },
                                                ack_bucket: None,
                                            });
                                            if let Err(e) = self.transport.send_command(cmd) {
                                                self.state.push_log(format!("[ACP] ResumeSession 发送失败：{e}"));
                                            } else {
                                                self.state.session_id = Some(sid.to_string());
                                                self.state.push_log(format!(
                                                    "[ACP] 已发送 ResumeSession → {sid_str}\n\
                                                     ReplayMode = SnapshotThenLive，若后端持有该 session，会通过 Event 流重放历史 turn。"
                                                ));
                                            }
                                        }
                                    }
                                    SlashLocalKind::AcpListSessions => {
                                        // 本地扫描 ~/.grodex/sessions 如果存在。
                                        let entries: Vec<_> = std::env::var_os("HOME").and_then(|home| {
                                            let p = std::path::Path::new(&home).join(".grodex").join("sessions");
                                            std::fs::read_dir(&p).ok().map(|rd| {
                                                rd.filter_map(|r| r.ok())
                                                    .filter_map(|e| {
                                                        let n = e.file_name().to_string_lossy().to_string();
                                                        let is_dir = e.file_type().ok().map(|ft| ft.is_dir()).unwrap_or(false);
                                                        if is_dir || n.ends_with(".jsonl") || n.ends_with(".json") { Some(n) } else { None }
                                                    })
                                                    .collect::<Vec<_>>()
                                            })
                                        }).unwrap_or_default();
                                        if entries.is_empty() {
                                            self.state.push_log(format!(
                                                "会话列表（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                                 ~/.grodex/sessions/ 下未发现持久化会话目录。\n\
                                                 Grodex 当前主要在内存中保留单会话，重启后会丢失。\n\
                                                 注：当前会话 ID：{}",
                                                self.state.session_id.clone().unwrap_or_else(|| "<none>".to_string())
                                            ));
                                        } else {
                                            let mut out = "会话列表（本地扫描 ~/.grodex/sessions）\n━━━━━━━━━━━━━━━━━━\n".to_string();
                                            for n in entries.iter().take(50) {
                                                out.push_str(&format!("  · {n}\n"));
                                            }
                                            out.push_str(&format!("\n共 {} 项。用 /resume <id> 或 :resume <id> 可发送 ACP ResumeSession 尝试恢复。", entries.len()));
                                            self.state.push_log(out);
                                        }
                                        let _ = args;
                                    }
                                    SlashLocalKind::AcpJumpToTurn => {
                                        self.state.push_log(format!(
                                            "（本地）jump /jumpto {args:?}\n\
                                             Grodex ACP JumpToTurn 帧：用于在 replay 模式下跳至指定 turn。\n\
                                             当前仅支持顺序回放，可先用 /sessions + /resume 从某会话重放，\n\
                                             期间 /cancel 中断，再 /jump <turn_id> 从指定 turn 继续（协议扩展）。\n\
                                             目前 events 已累积 {} 条 turn 相关事件。",
                                            self.state.events.len()
                                        ));
                                    }
                                    SlashLocalKind::AcpRewind => {
                                        // 本地 rewind：去掉最后 N 轮（User + Assistant 对）。
                                        let n: usize = args.trim().parse::<usize>().unwrap_or(1).max(1);
                                        // 从末尾倒推：每找到一对 User+Assistant 算一轮。
                                        let before = self.state.messages.len();
                                        // 倒序找到最近 N 个 User 消息（每轮起始于 User），
                                        // 截断后保留 0..cut_idx。Assistant 消息会连带被截掉。
                                        let mut cut_idx = self.state.messages.len();
                                        let mut user_seen = 0;
                                        for (i, m) in self.state.messages.iter().enumerate().rev() {
                                            if matches!(m, ui::state::ChatMessage::User { .. }) {
                                                user_seen += 1;
                                                if user_seen == n {
                                                    cut_idx = i;
                                                    break;
                                                }
                                            }
                                        }
                                        if user_seen >= n && cut_idx < before {
                                            self.state.messages.truncate(cut_idx);
                                            let after = self.state.messages.len();
                                            self.state.push_log(format!(
                                                "（本地）Rewind：回退 {n} 轮对话。\n  messages: {before} → {after}（删除 {} 条）\n  注：ACP Rewind 帧在协议中尚未定义，此为本地会话缓冲截断。\n       Agent 端若记忆了上下文，需配合 /compact 让它同步‘遗忘’。",
                                                before - after
                                            ));
                                        } else {
                                            self.state.push_log(format!(
                                                "（本地）Rewind：当前对话仅有 {user_seen} 轮 User 提问，不够回退 {n} 轮。"
                                            ));
                                        }
                                    }
                                    SlashLocalKind::AcpCompact => {
                                        // 本地 compact：删除 messages 中最早的 40%，
                                        // 保留最后 2 轮（以防截断到中间导致语义断裂）。
                                        // 同时也在 event buffer 上同步裁剪（保守清理最老 40%）。
                                        let before = self.state.messages.len();
                                        let target_keep_user = 2usize; // 保留最后 2 轮
                                        // 找到第 (total_user_rounds - target_keep) 个 User 的位置
                                        let total_user_rounds: usize = self.state.messages.iter().filter(|m| matches!(m, ui::state::ChatMessage::User { .. })).count();
                                        let skip_rounds = total_user_rounds.saturating_sub(target_keep_user);
                                        let mut cut = 0usize;
                                        let mut user_pass = 0usize;
                                        for (i, m) in self.state.messages.iter().enumerate() {
                                            if matches!(m, ui::state::ChatMessage::User { .. }) {
                                                if user_pass < skip_rounds {
                                                    user_pass += 1;
                                                    cut = i + 1; // 含该 User 之前（包括它）的全部丢弃
                                                } else {
                                                    break;
                                                }
                                            }
                                        }
                                        if cut > 0 {
                                            self.state.messages.drain(..cut);
                                        }
                                        let kept = self.state.messages.len();
                                        // event buffer 同步丢弃最早 30%（保守估计，避免内存爆炸）。
                                        let ev_before = self.state.events.len();
                                        let ev_cut = ev_before * 30 / 100;
                                        if ev_cut > 0 {
                                            self.state.events.drain(..ev_cut);
                                        }
                                        self.state.push_log(format!(
                                            "（本地）Compact：\n  messages: {before} → {kept}（丢弃最早 {} 条，保留最后 {} 轮用户提问）\n  events:   {ev_before} → {}（丢弃最早 30%）\n  注：若 Agent 端已持有 turn-based 上下文，此操作仅清理本地 UI 缓冲。\n       让 Agent 端同步压缩上下文需后端支持 ACP Compact 帧。",
                                            before - kept,
                                            total_user_rounds.min(target_keep_user),
                                            self.state.events.len(),
                                        ));
                                        let _ = args;
                                    }
                                    SlashLocalKind::AcpContext => {
                                        // 本地：打印当前 messages / events 的体量估计。
                                        let msg_words: usize = self.state.messages.iter().map(|m| {
                                            match m {
                                                ui::state::ChatMessage::User { text } |
                                                ui::state::ChatMessage::Assistant { text, .. } |
                                                ui::state::ChatMessage::Thinking { text, .. } |
                                                ui::state::ChatMessage::System { text, .. }
                                                    => text.split_whitespace().count(),
                                                ui::state::ChatMessage::Tool { args, result, .. }
                                                    => {
                                                        args.split_whitespace().count()
                                                            + result.as_ref().map(|r| r.split_whitespace().count()).unwrap_or(0)
                                                    }
                                            }
                                        }).sum();
                                        let turns: usize = self.state.messages.iter().filter(|m| matches!(m, ui::state::ChatMessage::User { .. })).count();
                                        self.state.push_log(format!(
                                            "当前上下文（本地估算）\n━━━━━━━━━━━━━━━━━━\n  messages        = {}\n  events          = {}\n  user turns      = {}\n  approx words    = {msg_words} ≈ {} tokens (w/1.3)\n  session_id      = {}\n  capability_gen  = G={}\n\n可用命令：\n  · /compact    本地删除最早 40% 消息\n  · /rewind N   回退 N 轮对话\n  · /recap      （需后端 ACP Recap 帧，暂未启用 LLM 总结）\nargs = {args:?}",
                                            self.state.messages.len(),
                                            self.state.events.len(),
                                            turns,
                                            (msg_words as f64 / 1.3) as usize,
                                            self.state.session_id.clone().unwrap_or_else(|| "<none>".to_string()),
                                            self.state.capability_generation,
                                        ));
                                    }
                                    SlashLocalKind::AcpRecap => {
                                        self.state.push_log(format!(
                                            "（说明）/recap /summarize {args:?}\n\
                                             本命令需 LLM 在‘无用户 prompt 情况下’主动写总结——\n\
                                             若直接从 TUI 发 Summary prompt 则违反 fail-closed（slash 命令从不 forward 给 LLM）。\n\
                                             替代：\n  1. 改为向 prompt 输入：‘请总结以上内容’（非 / 开头），正常发给模型。\n\
                                              2. /compact 本地删除最早消息（省显示内存，不影响 Agent 端实际上下文）。\n\
                                              3. /rewind N 回退最近几轮后继续提问。\n\
                                             当后端定义 ACP Recap 帧（结构化 server-side recap）后，这里会直接发送。"
                                        ));
                                    }
                                    SlashLocalKind::AcpRemember => {
                                        // 本地写入 ~/.grodex/remember.md，供未来 PromptBuilder 扫描。
                                        let content = args.trim();
                                        let (msg, err) = if content.is_empty() {
                                            (String::from(
                                                "用法：/remember 关键知识点 或 经验总结\n\
                                                 例：/remember 我们约定后端 API 一律用 snake_case JSON 字段\n\
                                                 写入位置：~/.grodex/remember.md"
                                            ), None)
                                        } else {
                                            let written = std::env::var_os("HOME").and_then(|home| {
                                                let p = std::path::Path::new(&home).join(".grodex").join("remember.md");
                                                let _ = std::fs::create_dir_all(p.parent().unwrap());
                                                let existing = std::fs::read_to_string(&p).unwrap_or_default();
                                                let stamp = chrono_like_now();
                                                let mut out = existing;
                                                if !out.is_empty() && !out.ends_with('\n') { out.push('\n'); }
                                                out.push_str(&format!("- [{}] {}\n", stamp, content));
                                                std::fs::write(&p, &out).ok().map(|()| p)
                                            });
                                            match written {
                                                Some(p) => (format!("（本地）已记到长期记忆：\n  内容：{content}\n  文件：{}", p.display()), None),
                                                None => (format!("写入失败：无法访问 ~/.grodex/remember.md。内容 = {content}"), Some(true)),
                                            }
                                        };
                                        if err.is_some() {
                                            self.state.messages.push(ui::state::ChatMessage::System { text: msg, is_error: true });
                                        } else {
                                            self.state.push_log(msg);
                                        }
                                    }
                                    SlashLocalKind::AcpSetModel => {
                                        // 本地：更新 model_label；同时尝试把新值写入
                                        // ~/.grodex/config.toml（仅当该文件存在）。
                                        let want = args.trim().to_string();
                                        if want.is_empty() {
                                            self.state.push_log(format!(
                                                "当前模型：{}（供应商 {}）\n\
                                                 用法：/model <name>  例：/model deepseek-v3\n\
                                                 本地立即更新显示值；若 ~/.grodex/config.toml 含 model= 字段，会同步改写（下次启动生效）。\n\
                                                 注：ACP SetModel 帧若后续加入协议，会即时通知 Agent 端切换模型。",
                                                self.state.model_label, self.state.provider_label,
                                            ));
                                        } else {
                                            let prev = std::mem::replace(&mut self.state.model_label, want.clone());
                                            // 尝试写配置
                                            let persisted = std::env::var_os("HOME").and_then(|home| {
                                                let p = std::path::Path::new(&home).join(".grodex").join("config.toml");
                                                let content = std::fs::read_to_string(&p).ok()?;
                                                let mut new_content = String::with_capacity(content.len() + 32);
                                                let mut written = false;
                                                for line in content.lines() {
                                                    let t = line.trim_start();
                                                    if t.starts_with("model") && !written {
                                                        // 替换这行
                                                        if let Some(eq) = line.find('=') {
                                                            let (prefix, _) = line.split_at(eq + 1);
                                                            new_content.push_str(prefix);
                                                            new_content.push(' ');
                                                            new_content.push('"');
                                                            new_content.push_str(&want);
                                                            new_content.push('"');
                                                            written = true;
                                                        } else {
                                                            new_content.push_str(line);
                                                        }
                                                    } else {
                                                        new_content.push_str(line);
                                                    }
                                                    new_content.push('\n');
                                                }
                                                if !written {
                                                    // 找不到：不追加，避免破坏文件结构。
                                                    return None;
                                                }
                                                std::fs::write(&p, new_content).ok()?;
                                                Some(())
                                            });
                                            let mut info = format!("（本地）模型：{prev} → {want}（已更新显示值，立即反映在状态栏）");
                                            if persisted.is_some() {
                                                info.push_str("\n已写入 ~/.grodex/config.toml 的 model 字段（重启后持久生效）。");
                                            } else {
                                                info.push_str("\n未写入配置文件：config.toml 不存在或不含 model 字段。\n若需持久化请手动编辑 ~/.grodex/config.toml。");
                                            }
                                            info.push_str("\n注：Agent 端即时切换需 ACP SetModel 帧（协议扩展中）。");
                                            self.state.push_log(info);
                                        }
                                    }
                                    SlashLocalKind::AcpSetEffort => {
                                        let arg = args.trim().to_lowercase();
                                        let choices = ["low", "med", "medium", "high", "auto"];
                                        let (info, mode) = if arg.is_empty() {
                                            (String::from("用法：/effort <low|med|high|auto>\n\
                                                     low    = 快速草稿、少思考\n\
                                                     med    = 默认（平衡）\n\
                                                     high   = 深入思考、多工具调用\n\
                                                     auto   = Agent 自适应\n\
                                                     说明：ACP SetEffort 帧尚未加入协议，此处仅作本地配置展示；\n\
                                                     真正生效需在每个 Prompt 中作为 effort 元数据字段。"), None)
                                        } else if choices.iter().any(|c| c == &arg.as_str()) {
                                            (format!("（本地）思考力度设置为 {arg}。\nACP SetEffort 帧尚未加入协议，当前仅本地展示。发送时 Agent 端若未读取 effort 元数据，则不会改变推理策略。"), Some(arg))
                                        } else {
                                            (format!("effort 参数 {arg:?} 不在 low/med(medium)/high/auto 范围。"), None)
                                        };
                                        if let Some(m) = mode {
                                            // 存一份本地状态：压进 logs 里持久 key=effort 以便未来读取
                                            // （state 里临时不加 effort 字段，因为没有地方渲染它）。
                                            self.state.push_log(format!("effort = {m}（本地记录，仅调试可见：/debug）"));
                                        }
                                        self.state.push_log(info);
                                    }
                                    SlashLocalKind::AcpToggleAlwaysApprove => {
                                        let arg = args.trim().to_lowercase();
                                        let next = if arg.is_empty() {
                                            !self.state.always_approve
                                        } else if arg == "on" || arg == "true" || arg == "1" || arg == "yes" {
                                            true
                                        } else if arg == "off" || arg == "false" || arg == "0" || arg == "no" {
                                            false
                                        } else {
                                            self.state.push_log(format!("[always-approve] 未识别参数 {arg:?}，应使用 on/off 或不传（= 切换）。"));
                                            self.state.always_approve // keep
                                        };
                                        if next != self.state.always_approve || arg.is_empty() {
                                            self.state.always_approve = next;
                                        }
                                        let hint = if next {
                                            "所有新审批将自动回复 Allow（直到重启或再次切换）。"
                                        } else {
                                            "已关闭：审批将恢复为手动 a/d/c/n 或 ↓ 选择后 A 确认。"
                                        };
                                        self.state.push_log(format!(
                                            "（本地）always-approve → {next}\n{hint}\n\
                                             说明：ACP SetApprovalPolicy 帧若后续加入协议，会向 Agent 端同步此策略；\n\
                                             当下仅本地生效（grodex-tui 自动 ResolveApproval(Allow) 处理新到达的 approval tickets）。"
                                        ));
                                    }
                                    SlashLocalKind::AcpToggleAuto => {
                                        let arg = args.trim().to_lowercase();
                                        let next = if arg.is_empty() {
                                            !self.state.yolo_mode
                                        } else if arg == "on" || arg == "true" || arg == "1" || arg == "yes" || arg == "yolo" {
                                            true
                                        } else if arg == "off" || arg == "false" || arg == "0" || arg == "no" {
                                            false
                                        } else {
                                            self.state.push_log(format!("[auto/yolo] 未识别参数 {arg:?}，应使用 on/off 或不传。"));
                                            self.state.yolo_mode
                                        };
                                        self.state.yolo_mode = next;
                                        let hint = if next {
                                            "YOLO 模式：Agent 将自动跳过中等风险审批，仅在高危时弹出。\n\
                                             （本地策略：always_approve 同时启用时风险最高）"
                                        } else {
                                            "已关闭：所有审批按默认策略弹出等待手动确认。"
                                        };
                                        self.state.push_log(format!("（本地）auto/yolo → {next}\n{hint}\n注：ACP SetAutoApproveLevel 帧待协议扩展。"));
                                    }
                                    SlashLocalKind::AcpToggleMultiline => {
                                        self.state.push_log(format!(
                                            "（说明）multiline {args:?}\n\
                                             Grodex Prompt 模式原生支持多行：\n\
                                               · Shift+Enter          插入换行\n\
                                               · Enter 单独按下             永远=提交（Grok 规则）\n\
                                             无需切换。‘多行模式’开关仅影响部分第三方客户端。\n\
                                             （ACP SetInputMode 帧未定义，此处仅为别名说明）"
                                        ));
                                    }
                                    SlashLocalKind::AcpToggleCompactMode => {
                                        let arg = args.trim().to_lowercase();
                                        let next = if arg.is_empty() { !self.state.compact_ui_mode }
                                        else if matches!(arg.as_str(), "on"|"true"|"1"|"yes") { true }
                                        else if matches!(arg.as_str(), "off"|"false"|"0"|"no") { false }
                                        else { self.state.push_log(format!("[compact-mode] 未识别参数 {arg:?}")); self.state.compact_ui_mode };
                                        self.state.compact_ui_mode = next;
                                        self.state.push_log(format!(
                                            "（本地）compact-mode UI → {next}\n\
                                             on  = 隐藏 StatusBar 上下文信息、审批面板更紧凑\n\
                                             off = 默认视图。\n\
                                             注：ACP ToggleUI 帧尚未定义；此处为纯本地样式开关。"
                                        ));
                                    }
                                    SlashLocalKind::AcpToggleVimMode => {
                                        let arg = args.trim().to_lowercase();
                                        let next = if arg.is_empty() { !self.state.vim_mode }
                                        else if matches!(arg.as_str(), "on"|"true"|"1"|"yes") { true }
                                        else if matches!(arg.as_str(), "off"|"false"|"0"|"no") { false }
                                        else { self.state.push_log(format!("[vim-mode] 未识别参数 {arg:?}")); self.state.vim_mode };
                                        self.state.vim_mode = next;
                                        self.state.push_log(format!(
                                            "（本地）vim-mode → {next}\n\
                                             Vim 键位（Normal 模式滚动区）：\n\
                                               j/k   下/上一行   ↔   J/K/G 选择审批或 G 跳到底\n\
                                               C-d/C-u  下半/上半页\n\
                                               gg    跳到顶部\n\
                                             关闭后 Normal 模式下 j/k 不做额外解释（但当前我们不区分 vim on/off，保持滚动兼容）。"
                                        ));
                                    }
                                    SlashLocalKind::AcpSwitchScreenMode => {
                                        self.state.push_log(format!(
                                            "（说明）screen/minimal/fullscreen {args:?}\n\
                                             Grodex 仅提供 crossterm alternate-screen TUI（全屏字符 UI），\n\
                                             没有‘最小化独立窗口 / 外部编辑器 fullscreen minimal’模式。\n\
                                             想切换到外部编辑器，请用 /edit-prompt（调用 $EDITOR 打开当前草稿）。"
                                        ));
                                    }
                                    SlashLocalKind::AcpChdir => {
                                        let target = args.trim();
                                        if target.is_empty() {
                                            let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| "?".to_string());
                                            self.state.push_log(format!("当前工作目录：{cwd}\n用法：/cd <path>  例：/cd ~/myproject"));
                                        } else {
                                            let expanded = shellexpand_tilde(target);
                                            match std::env::set_current_dir(&expanded) {
                                                Ok(()) => {
                                                    let new_cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| "?".to_string());
                                                    self.state.push_log(format!("（本地）cd → {new_cwd}\n注意：仅 TUI 进程的当前目录改变；\nAgent 端若已缓存 cwd 需通过 ACP Chdir 帧（待定义）才能同步。"));
                                                }
                                                Err(e) => self.state.messages.push(ui::state::ChatMessage::System {
                                                    text: format!("cd {target:?} 失败：{e}"),
                                                    is_error: true,
                                                }),
                                            }
                                        }
                                    }
                                    SlashLocalKind::AcpPlan => {
                                        // 本地 plan：args 当成一个任务添加
                                        let t = args.trim().to_string();
                                        if t.is_empty() {
                                            // 查看当前任务
                                            print_tasks(&mut self.state, "Plan");
                                        } else {
                                            self.state.tasks.push((t.clone(), false));
                                            self.state.push_log(format!("（本地）已加入 Plan：{t}\n查看：/plan   完成：/tasks done <idx>   清空：/queue clear"));
                                        }
                                    }
                                    SlashLocalKind::AcpQueue => {
                                        // queue 支持 add/list/clear
                                        let arg = args.trim();
                                        if arg.is_empty() || arg.eq_ignore_ascii_case("list") || arg.eq_ignore_ascii_case("ls") {
                                            print_tasks(&mut self.state, "Queue");
                                        } else if let Some(rest) = arg.strip_prefix("clear") {
                                            let r = rest.trim();
                                            if r.is_empty() {
                                                let n = self.state.tasks.len();
                                                self.state.tasks.clear();
                                                self.state.push_log(format!("（本地）Queue 已清空（移除 {n} 条）"));
                                            } else if let Ok(idx) = r.parse::<usize>() {
                                                let i = idx.saturating_sub(1);
                                                if i < self.state.tasks.len() {
                                                    let removed = self.state.tasks.remove(i);
                                                    self.state.push_log(format!("（本地）已移除 Queue #{}：{}", idx, removed.0));
                                                } else {
                                                    self.state.push_log(format!("Queue index {idx} 越界，当前共 {} 条。", self.state.tasks.len()));
                                                }
                                            }
                                        } else if let Some(rest) = arg.strip_prefix("add ") {
                                            let desc = rest.trim().to_string();
                                            if !desc.is_empty() {
                                                self.state.tasks.push((desc.clone(), false));
                                                self.state.push_log(format!("（本地）Queue add：{desc}"));
                                            }
                                        } else {
                                            // 默认当 add
                                            self.state.tasks.push((arg.to_string(), false));
                                            self.state.push_log(format!("（本地）Queue add：{arg}"));
                                        }
                                    }
                                    SlashLocalKind::AcpTasks => {
                                        // tasks: list / done <idx> / undone <idx> / rm <idx> / add <...>
                                        let arg = args.trim();
                                        if arg.is_empty() || arg == "ls" || arg == "list" {
                                            print_tasks(&mut self.state, "Tasks");
                                        } else if let Some(rest) = arg.strip_prefix("done ") {
                                            mark_task(&mut self.state, rest, true);
                                        } else if let Some(rest) = arg.strip_prefix("undone ") {
                                            mark_task(&mut self.state, rest, false);
                                        } else if let Some(rest) = arg.strip_prefix("rm ") {
                                            let idx_s = rest.trim();
                                            if let Ok(idx) = idx_s.parse::<usize>() {
                                                let i = idx.saturating_sub(1);
                                                if i < self.state.tasks.len() {
                                                    let r = self.state.tasks.remove(i);
                                                    self.state.push_log(format!("（本地）已删除 tasks #{}：{}", idx, r.0));
                                                } else {
                                                    self.state.push_log(format!("tasks index {idx} 越界"));
                                                }
                                            }
                                        } else if let Some(rest) = arg.strip_prefix("add ") {
                                            let desc = rest.trim().to_string();
                                            if !desc.is_empty() {
                                                self.state.tasks.push((desc.clone(), false));
                                                self.state.push_log(format!("（本地）tasks add：{desc}"));
                                            }
                                        } else {
                                            // 默认：list（展示当前全部），然后提示用法
                                            print_tasks(&mut self.state, "Tasks");
                                            self.state.push_log(String::from("用法：\n  /tasks ls                    列全部\n  /tasks done 2                标记第 2 项完成\n  /tasks undone 2              取消完成\n  /tasks rm 2                  删除第 2 项\n  /tasks add 写出测试用例      新增任务"));
                                        }
                                    }
                                    SlashLocalKind::AcpMcpServers => {
                                        // 本地扫描 MCP 配置：~/.grodex/mcp/*.json 或 config.toml 内 [mcp.*]
                                        let mut out = String::from("MCP 服务器清单（本地扫描）\n━━━━━━━━━━━━━━━━━━\n");
                                        let mut n = 0usize;
                                        if let Some(home) = std::env::var_os("HOME") {
                                            let dir = std::path::Path::new(&home).join(".grodex").join("mcp");
                                            if let Ok(rd) = std::fs::read_dir(&dir) {
                                                for entry in rd.filter_map(|r| r.ok()) {
                                                    let name = entry.file_name().to_string_lossy().to_string();
                                                    if name.ends_with(".json") || name.ends_with(".toml") || name.ends_with(".yaml") {
                                                        n += 1;
                                                        out.push_str(&format!("  · {name}\n"));
                                                    }
                                                }
                                            }
                                        }
                                        if n == 0 {
                                            out.push_str("  ~/.grodex/mcp/ 下未找到 server 描述文件。\n");
                                            out.push_str("\n如何启用 MCP：\n");
                                            out.push_str("  1. 在 ~/.grodex/config.toml 中加 [mcp.<name>]\n");
                                            out.push_str("     command = \"python3\" args = [\"-m\", \"mcp.server.xxx\"]\n");
                                            out.push_str("  2. 或在 ~/.grodex/mcp/<name>.json 放 server 配置 JSON。\n");
                                            out.push_str("  3. 重启 Grodex 即自动 spawn MCP 进程。\n");
                                        } else {
                                            out.push_str(&format!("\n共 {n} 个 MCP server 描述。注：ACP MCP ListServers 帧尚未加入协议；\n当前仅展示，不包含在线状态校验（真实连通性需 Agent 端连接后才能确定）。"));
                                        }
                                        if !args.is_empty() { out.push_str(&format!("\nargs = {args:?}（/mcp enable/disable/status 等子命令需后端 MCP ACP 帧扩展）")); }
                                        self.state.push_log(out);
                                    }
                                    SlashLocalKind::AcpWorkflows => {
                                        // 本地扫描 workflow 目录
                                        let mut found = scan_dir_list("workflows", "~/.grodex/workflows", &[".toml", ".yaml", ".json"]);
                                        if found.is_empty() {
                                            // 再扫项目级
                                            if let Ok(cwd) = std::env::current_dir() {
                                                let pd = cwd.join(".grodex").join("workflows");
                                                if let Ok(rd) = std::fs::read_dir(&pd) {
                                                    for e in rd.filter_map(|r| r.ok()) {
                                                        let n = e.file_name().to_string_lossy().to_string();
                                                        if [".toml", ".yaml", ".json"].iter().any(|s| n.ends_with(s)) {
                                                            found.push(n);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        let mut out = "Workflow 管理（本地扫描）\n━━━━━━━━━━━━━━━━━━\n".to_string();
                                        if found.is_empty() {
                                            out.push_str("~/.grodex/workflows/ 与 .grodex/workflows/ 均为空。\n放置 .toml/.yaml/.json 文件即可被列出。\n");
                                            out.push_str("\nWorkflows 需后端 ACP ExecuteWorkflow 帧才能触发；\n当前仅展示清单。");
                                        } else {
                                            for n in found.iter() { out.push_str(&format!("  · {n}\n")); }
                                            out.push_str(&format!("\n共 {} 个 workflow。", found.len()));
                                        }
                                        if !args.is_empty() { out.push_str(&format!("\nargs = {args:?}（/workflows run <name> 需 ACP ExecuteWorkflow 帧扩展）")); }
                                        self.state.push_log(out);
                                    }
                                    SlashLocalKind::AcpHooks => {
                                        let list = scan_dir_list("plugin hooks", "~/.grodex/hooks", &[".sh", ".py", ".rs", ".ts", ".js", ".lua"]);
                                        let mut out = "插件 Hooks（本地扫描）\n━━━━━━━━━━━━━━━━━━\n".to_string();
                                        if list.is_empty() {
                                            out.push_str("~/.grodex/hooks/ 为空。支持脚本：.sh / .py / .rs / .ts / .js / .lua\n");
                                            out.push_str("典型钩子名：before-tool-call / after-tool-call / before-turn / after-turn / on-approval\n");
                                        } else {
                                            for n in list { out.push_str(&format!("  · {n}\n")); }
                                        }
                                        out.push_str("\n注：实际触发 hooks 需 Agent 端 hook runtime（ACP 尚无统一帧）。");
                                        if !args.is_empty() { out.push_str(&format!("\nargs = {args:?}")); }
                                        self.state.push_log(out);
                                    }
                                    SlashLocalKind::AcpPlugins => {
                                        let list = scan_dir_list("plugins", "~/.grodex/plugins", &[".wasm", ".toml", ".plugin"]);
                                        let mut out = "插件列表（本地扫描）\n━━━━━━━━━━━━━━━━━━\n".to_string();
                                        if list.is_empty() {
                                            out.push_str("~/.grodex/plugins/ 下未发现 .wasm / .toml / .plugin。\n\nGrodex 插件形态：\n  · 原生 MCP server 脚本（见 /mcp）\n  · Skill Markdown 文件（见 /skills）\n  · Hook 脚本（见 /hooks）\n  · 未来：WebAssembly 动态插件");
                                        } else {
                                            let n_plugins = list.len();
                                            for n in &list { out.push_str(&format!("  · {n}\n")); }
                                            out.push_str(&format!("\n共 {n_plugins} 个。"));
                                        }
                                        if !args.is_empty() { out.push_str(&format!("\nargs = {args:?}（/plugins install/uninstall 需插件管理器 ACP 帧）")); }
                                        self.state.push_log(out);
                                    }
                                    SlashLocalKind::AcpMarketplace => {
                                        self.state.push_log(format!(
                                            "（说明）marketplace / marketplace-install {args:?}\n\
                                             Grodex 当前没有中央插件市场。安装第三方扩展的方式：\n\
                                               · Skills：把 Markdown 文件放进 ~/.grodex/skills/（见 /skills）\n\
                                               · MCP：在 ~/.grodex/mcp/ 放 server 描述 JSON（见 /mcp）\n\
                                               · Hooks：~/.grodex/hooks/ 下加脚本（见 /hooks）\n\
                                             若需从 URL 一键安装，请手动 `curl -o ~/.grodex/...` 后重启。\n\
                                             统一的 Marketplace ACP 帧（ListMarketplace/Install）尚未加入协议。"
                                        ));
                                    }
                                    SlashLocalKind::AcpSkills => {
                                        // 本地直接扫描 skills 目录，展示实际可用清单。
                                        // 不再只报 "ACP 帧未接入"，用户能看到真实数据。
                                        let cwd = std::env::current_dir()
                                            .map(|p| p.to_string_lossy().to_string())
                                            .unwrap_or_else(|_| ".".to_string());
                                        let catalog = grodex_skills::catalog::SkillCatalog::discover(
                                            std::path::Path::new(&cwd),
                                        );
                                        if catalog.is_empty() {
                                            self.state.push_log(format!(
                                                "Skills 清单（本地扫描）\n━━━━━━━━━━━━━━━━━━\n当前目录未发现 Skills。\n\nSkill 发现位置：\n  · 项目级：.grodex/skills/*.md\n  · 用户级：~/.grodex/skills/*.md\n\nargs={args:?}"
                                            ));
                                        } else {
                                            let mut out = String::from("Skills 清单（本地扫描）\n━━━━━━━━━━━━━━━━━━\n");
                                            for s in catalog.list() {
                                                out.push_str(&format!("  · {:<20} {}\n", s.name, s.description));
                                            }
                                            out.push_str(&format!("\n共 {} 个 skill。", catalog.len()));
                                            self.state.push_log(out);
                                        }
                                    }
                                    SlashLocalKind::AcpRename => {
                                        // 本地：设 state.session_title；Status bar 后续可用此替代默认 "new session"。
                                        let title = args.trim().to_string();
                                        if title.is_empty() {
                                            self.state.push_log(format!(
                                                "当前会话标题：{}\n\
                                                 当前会话 ID：{}\n\
                                                 用法：/rename 新标题   或   /title 新标题",
                                                self.state.session_title.as_deref().unwrap_or("<未命名，显示为 new session>"),
                                                self.state.session_id.clone().unwrap_or_else(|| "<none>".to_string()),
                                            ));
                                        } else {
                                            self.state.session_title = Some(title.clone());
                                            self.state.push_log(format!(
                                                "（本地）会话标题 → {title}\n\
                                                 下次渲染时会替换 Prompt widget 上沿的默认 ‘new session’ 标签。\n\
                                                 注：ACP Rename 帧若后续加入协议，会同步至后端持久化。"
                                            ));
                                        }
                                    }
                                    SlashLocalKind::AcpSessionInfo => {
                                        let sid = self.state.session_id.clone().unwrap_or_else(|| "<none>".to_string());
                                        let tid = self.state.turn_id.clone().unwrap_or_else(|| "<none>".to_string());
                                        let title = self.state.session_title.clone().unwrap_or_else(|| "<未命名>".to_string());
                                        let prov = if self.state.provider_label.is_empty() { "—" } else { self.state.provider_label.as_str() };
                                        let model = if self.state.model_label.is_empty() { "—" } else { self.state.model_label.as_str() };
                                        let trust = if self.state.workspace_trusted { "trusted" } else { "untrusted" };
                                        let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| "?".to_string());
                                        let stream = if self.state.is_streaming() { "⏳ streaming" } else { "idle" };
                                        let tools = self.state.active_tool_count();
                                        let appr = self.state.pending_approvals.len();
                                        let aa = if self.state.always_approve { "on" } else { "off" };
                                        let yolo = if self.state.yolo_mode { "on" } else { "off" };
                                        let ts = if self.state.show_timestamps { "on" } else { "off" };
                                        let tasks_total = self.state.tasks.len();
                                        let tasks_done = self.state.tasks.iter().filter(|(_, d)| *d).count();
                                        self.state.push_log(format!(
                                            "Session Info（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                             标题          : {title}\n\
                                             session_id    : {sid}\n\
                                             turn_id       : {tid}\n\
                                             gen           : G={}\n\
                                             provider      : {prov}\n\
                                             model         : {model}\n\
                                             trust         : {trust}\n\
                                             cwd           : {cwd}\n\
                                             messages      : {} 条\n\
                                             events        : {} 条\n\
                                             pending tools : {tools}\n\
                                             approvals     : {appr}\n\
                                             always-approve: {aa}\n\
                                             yolo          : {yolo}\n\
                                             show timestamps: {ts}\n\
                                             tasks         : {tasks_done}/{tasks_total}\n\
                                             streaming     : {stream}\n\
                                             args          : {args:?}",
                                            self.state.capability_generation,
                                            self.state.messages.len(),
                                            self.state.events.len(),
                                        ));
                                    }
                                    SlashLocalKind::AcpShare => {
                                        // 本地 share = 导出为 md 放到 ~/.grodex/exports/
                                        let export_res = export_conversation_md(&self.state);
                                        match export_res {
                                            Ok(path) => self.state.push_log(format!(
                                                "（本地）已导出为 Markdown：\n  路径：{}\n\
                                                 直接打开即可阅读 / 粘贴分享给他人。\n\
                                                 注：云端 Share / Publish 需 ACP Share 帧 + 后端托管。",
                                                path.display()
                                            )),
                                            Err(e) => self.state.messages.push(ui::state::ChatMessage::System {
                                                text: format!("导出失败：{e}"),
                                                is_error: true,
                                            }),
                                        }
                                    }
                                    SlashLocalKind::AcpDoctor => {
                                        // 本地 doctor：检查 ~/.grodex/* 文件、config、网络（简单）
                                        use std::path::Path;
                                        let home = std::env::var_os("HOME");
                                        let dot = home.as_ref().map(|h| Path::new(h).join(".grodex"));
                                        let mut out = String::from("Doctor（本地自检）\n━━━━━━━━━━━━━━━━━━\n");
                                        // 1. cwd
                                        match std::env::current_dir() {
                                            Ok(c) => out.push_str(&format!("✓ cwd                     : {}\n", c.display())),
                                            Err(e) => out.push_str(&format!("✗ cwd                     : {e}\n")),
                                        }
                                        // 2. trust
                                        let trust = if self.state.workspace_trusted { "trusted ⚠" } else { "untrusted (fail-closed OK)" };
                                        out.push_str(&format!("  workspace trust        : {trust}\n"));
                                        // 3. ~/.grodex
                                        if let Some(dot) = &dot {
                                            match dot.metadata() {
                                                Ok(m) if m.is_dir() => {
                                                    out.push_str(&format!("✓ ~/.grodex/             存在 ({})\n", dot.display()));
                                                    // config
                                                    let cfg = dot.join("config.toml");
                                                    match cfg.metadata() {
                                                        Ok(m2) if m2.is_file() => {
                                                            // 读一下校验有没有 provider/model 关键字
                                                            let body = std::fs::read_to_string(&cfg).unwrap_or_default();
                                                            let has_provider = body.lines().any(|l| l.trim().starts_with("provider"));
                                                            let has_model = body.lines().any(|l| l.trim().starts_with("model"));
                                                            let tag = match (has_provider, has_model) {
                                                                (true, true) => "✓ provider & model 都有",
                                                                (true, false) => "⚠ 有 provider 无 model",
                                                                (false, true) => "⚠ 有 model 无 provider",
                                                                (false, false) => "✗ provider/model 都缺",
                                                            };
                                                            out.push_str(&format!("  · config.toml         : {} ({tag}, {} bytes)\n", cfg.display(), body.len()));
                                                        }
                                                        _ => out.push_str("✗ · config.toml         : 缺失 — TUI authentication 会失败\n"),
                                                    }
                                                    // skills
                                                    for (label, subdir, ext_list) in [
                                                        ("skills", "skills", &[".md"][..]),
                                                        ("mcp", "mcp", &[".json", ".yaml", ".toml"]),
                                                        ("plugins", "plugins", &[".wasm", ".toml"]),
                                                    ] {
                                                        let p = dot.join(subdir);
                                                        match p.read_dir() {
                                                            Ok(rd) => {
                                                                let n = rd.filter_map(|r| r.ok()).filter(|e| {
                                                                    let n = e.file_name().to_string_lossy().to_string();
                                                                    ext_list.iter().any(|ext| n.ends_with(ext))
                                                                }).count();
                                                                out.push_str(&format!("  · {label:<12}        : {n} 个可识别文件\n"));
                                                            }
                                                            Err(std::io::Error { .. }) => out.push_str(&format!("  · {label:<12}        : （空目录）\n")),
                                                        }
                                                    }
                                                }
                                                _ => out.push_str("✗ ~/.grodex/             目录不存在，需先运行 grodex init 或启动后配置 API Key\n"),
                                            }
                                        }
                                        // 4. provider/model label 当前值
                                        let prov = if self.state.provider_label.is_empty() { "<空>" } else { self.state.provider_label.as_str() };
                                        let model = if self.state.model_label.is_empty() { "<空>" } else { self.state.model_label.as_str() };
                                        out.push_str(&format!("  当前 provider         : {prov}\n  当前 model            : {model}\n"));
                                        // 5. TUI 状态
                                        out.push_str(&format!("  TUI 模式              : {:?}\n  messages/events       : {} / {}\n  approvals/tools       : {} / {}\n",
                                            self.state.input_mode, self.state.messages.len(), self.state.events.len(),
                                            self.state.pending_approvals.len(), self.state.active_tool_count()));
                                        out.push_str("\n说明：ACP Doctor 帧若加入协议，这里会替换为服务端完整一致性检查（会话持久化、turn 事件 hole 检测、credential 租约健康度等）。");
                                        self.state.push_log(out);
                                        let _ = args;
                                    }
                                    SlashLocalKind::AcpUsage => {
                                        self.state.push_log(format!(
                                            "（说明）usage / cost / billing {args:?}\n\
                                             Grodex 没有统一用量中心。计费/用量由你配置的 LLM 供应商（{prov}）直接结算：\n\
                                               · DeepSeek   控制台：https://platform.deepseek.com\n\
                                               · OpenAI     控制台：https://platform.openai.com/usage\n\
                                               · 其他供应商 → 各自 dashboard\n\
                                             grodex 端目前仅在 logs/debug 中累计 turn 数（见 /debug）。\n\
                                             如需内置 token usage 报表 + 多供应商聚合：ACP UsageSnapshot 帧需加入协议。",
                                            prov = if self.state.provider_label.is_empty() { "未知" } else { self.state.provider_label.as_str() }
                                        ));
                                    }
                                    SlashLocalKind::AcpSettings => {
                                        // 本地：如果有 $EDITOR 就直接打开 ~/.grodex/config.toml；
                                        // 否则打印当前配置并让用户手动编辑。
                                        let editor = std::env::var("EDITOR").ok().filter(|s| !s.is_empty());
                                        let path = std::env::var_os("HOME").map(|h| {
                                            std::path::Path::new(&h).join(".grodex").join("config.toml")
                                        });
                                        match (&editor, &path) {
                                            (Some(ed), Some(p)) => {
                                                if p.exists() {
                                                    // spawn 到外部编辑器
                                                    match std::process::Command::new(ed).arg(p).status() {
                                                        Ok(st) if st.success() => {
                                                            self.state.push_log(format!(
                                                                "（本地）用 {ed} 打开 config.toml，退出编辑器后可在此重新加载（重启后 provider/model 生效）。"
                                                            ));
                                                        }
                                                        Ok(st) => self.state.push_log(format!("编辑器 {ed} 退出 {st:?}（可能文件未保存或无权限写入）")),
                                                        Err(e) => self.state.push_log(format!("启动编辑器 {ed} 失败：{e}")),
                                                    }
                                                } else {
                                                    self.state.push_log(format!("config.toml 不存在：{}。请先运行 grodex init 或手动创建。", p.display()));
                                                }
                                            }
                                            _ => {
                                                let p_str = path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "~/.grodex/config.toml".to_string());
                                                self.state.push_log(format!(
                                                    "（说明）settings / preferences {args:?}\n\
                                                     没有 $EDITOR 环境变量。请手动编辑：\n    {p_str}\n\
                                                     或执行：export EDITOR=vim  后重开 TUI。\n\
                                                     ACP SettingsEditor 帧若加入协议，这里会替换为内嵌配置编辑器面板。"
                                                ));
                                            }
                                        }
                                    }
                                    SlashLocalKind::AcpPersonas => {
                                        // grodex 无 persona/role 系统，展示当前 agent
                                        // 运行配置（provider/model/trust/cwd）作为本地
                                        // 可查的 agent 信息，而非只报 "未接入"。
                                        let cwd = std::env::current_dir()
                                            .map(|p| p.to_string_lossy().to_string())
                                            .unwrap_or_else(|_| "?".to_string());
                                        let trust = if self.state.workspace_trusted { "trusted" } else { "untrusted" };
                                        let gen_val = self.state.capability_generation;
                                        self.state.push_log(format!(
                                            "Agent / Persona 信息（本地）\n━━━━━━━━━━━━━━━━━━\nGrodex 无独立 persona/role 系统，当前 agent 运行配置：\n  · provider:  {prov}\n  · model:     {model}\n  · trust:     {trust}\n  · cwd:       {cwd}\n  · generation: G={gen_val}\n\n注：/personas /agents /roles 为 Grok 兼容别名，\n     Grodex 通过 ~/.grodex/config.toml 统一配置，\n     切换供应商/模型需编辑配置后重启。\nargs={args:?}",
                                            prov = self.state.provider_label,
                                            model = self.state.model_label,
                                        ));
                                    }
                                    SlashLocalKind::AcpTheme => {
                                        let arg = args.trim().to_lowercase();
                                        if !arg.is_empty() {
                                            self.state.push_log(format!(
                                                "（说明）theme {arg:?}\n\
                                                 当前未加入 ACP ThemeFrame，参数不会即时生效。\n\
                                                 已定义的主题名预留：default / dracula / nord / solarized / monokai / gruvbox.\n\
                                                 切换主题需重启后生效（ACP ThemeFrame 尚未加入协议）。"
                                            ));
                                        } else {
                                            self.state.push_log(String::from(
                                                "当前调色板（default，Grok-compatible neutral）\n━━━━━━━━━━━━━━━━━━\n\
                                                 · 背景    : Terminal default (transparent inherit)\n\
                                                 · 前景    : #E6E8F0\n\
                                                 · Grodex 蓝: #78C8FF (BOLD)\n\
                                                 · User 绿 : #8CDCAA (BOLD)\n\
                                                 · Asst 蓝 : #8CAFFF (BOLD)\n\
                                                 · ❯ 前缀   : #FFC864 (BOLD) amber\n\
                                                 · Tool 名 : #D7D7AA\n\
                                                 · 错误红  : #FF7878\n\
                                                 · 警告黄  : #FFCD64\n\
                                                 可用主题（ACP ThemeFrame 接入后切换）：\n\
                                                   default / dracula / nord / solarized / monokai / gruvbox\n\
                                                 用法：/theme <name>"
                                            ));
                                        }
                                    }
                                    SlashLocalKind::AcpReleaseNotes => {
                                        let mut lines = Vec::<String>::new();
                                        if let Ok(manifest) = std::fs::read_to_string("Cargo.toml").or_else(|_| std::fs::read_to_string("../Cargo.toml")) {
                                            for l in manifest.lines() {
                                                let t = l.trim();
                                                if let Some(v) = t.strip_prefix("version") {
                                                    if let Some(eq) = v.find('=') {
                                                        let s = v[eq+1..].trim().trim_matches('"').trim().trim_matches('"');
                                                        if !s.is_empty() { lines.push(format!("Grodex version: {s}")); break; }
                                                    }
                                                }
                                            }
                                        }
                                        if let Ok(cl) = std::fs::read_to_string("CHANGELOG.md").or_else(|_| std::fs::read_to_string("../CHANGELOG.md")) {
                                            let head: String = cl.lines().take(40).collect::<Vec<_>>().join("\n");
                                            if !head.is_empty() {
                                                lines.push(String::from("\nCHANGELOG.md 最近 40 行：\n"));
                                                lines.push(head);
                                            }
                                        }
                                        if lines.is_empty() {
                                            self.state.push_log(format!(
                                                "release-notes / changelog {args:?}（本地）\n\
                                                 未在当前目录找到 CHANGELOG.md 或 Cargo.toml workspace version。\n\
                                                 请从 repo 根目录运行 grodex tui 或查阅 GitHub releases。"
                                            ));
                                        } else {
                                            self.state.push_log(lines.join("\n"));
                                        }
                                    }
                                    SlashLocalKind::AcpTutorial => {
                                        let aa = if self.state.always_approve { "on" } else { "off" };
                                        self.state.push_log(format!(
                                            "🎯 Grodex TUI 新手引导（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                             · 输入模式（默认）: 直接打字，Shift+Enter 换行，Enter 发送\n\
                                             · 普通模式（Esc） : ↑/↓ 翻页，k/j 翻对话，g/G 头尾\n\
                                             · 斜杠命令       : 打 / 弹出菜单，↑/↓ 选择，Enter 执行\n\
                                             · Tab            : 在菜单中自动补全当前项\n\
                                             · 审批面板       : a 批准 / d 拒绝 / A 全部批准\n\
                                             · 会话 / 审批切换: Tab 键在两块面板间切焦点\n\
                                             · 常用命令\n\
                                                 /help              查看全部命令\n\
                                                 /info              当前会话状态\n\
                                                 /doctor            环境自检\n\
                                                 /compact-ui        紧凑模式开关\n\
                                                 /always-approve    免审批（当前 {aa}）\n\
                                                 /tasks add ...     新建待办\n\
                                                 /export            导出 MD 到 ~/.grodex/exports/\n\
                                             · 向 LLM 提问      : 不要以 / 开头，直接自然语言即可\n\
                                             args={args:?}"
                                        ));
                                    }
                                    SlashLocalKind::AcpDocs => {
                                        // 本地：优先读 repo docs/；若没有，打印手册
                                        let mut found = false;
                                        for base in ["docs", "../docs", "doc", "guide", "guides"] {
                                            let p = std::path::Path::new(base);
                                            if p.exists() {
                                                let mut entries: Vec<String> = Vec::new();
                                                if let Ok(rd) = std::fs::read_dir(p) {
                                                    for e in rd.flatten() {
                                                        if let Ok(ft) = e.file_type() {
                                                            if ft.is_file() {
                                                                entries.push(e.file_name().to_string_lossy().to_string());
                                                            }
                                                        }
                                                    }
                                                }
                                                entries.sort();
                                                let n = entries.len();
                                                let list = entries.iter().take(40).cloned().collect::<Vec<_>>().join("\n  · ");
                                                self.state.push_log(format!(
                                                    "📚 本地文档目录：{base}/（{n} 个文件）\n━━━━━━━━━━━━━━━━━━\n  · {list}\n\n\
                                                     说明：ACP DocsBrowse 帧若接入协议，将支持内嵌面板查看 Markdown 渲染版。\n\
                                                     当前请直接在编辑器打开对应文件阅读。\nargs={args:?}"
                                                ));
                                                found = true;
                                                break;
                                            }
                                        }
                                        if !found {
                                            self.state.push_log(format!(
                                                "📚 Grodex 文档与指南（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                                 当前目录未发现 docs/ 文件夹。信息来源：\n\
                                                 · 项目 README.md / CHANGELOG.md（/release-notes 可读）\n\
                                                 · 每个 crate 的 lib.rs doc comments：cargo doc --open\n\
                                                 · 运行：/help 命令清单；/doctor 环境自检；/info 会话状态\n\
                                                 · 配置参考：~/.grodex/config.toml（/settings 打开）\nargs={args:?}"
                                            ));
                                        }
                                    }
                                    SlashLocalKind::AcpFindInScrollback => {
                                        let q = args.trim();
                                        if q.is_empty() {
                                            self.state.push_log(String::from(
                                                "🔍 滚动区搜索（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                                 用法：/find <关键词>\n\
                                                 说明：ACP FindInScrollback 帧尚未定义，当前为本地内存线性扫描。\n\
                                                 命中：显示命中条数 + 最近 5 条所在位置。\n\
                                                 快捷键：Grok 中对应 Ctrl+F 面板（本 TUI 暂通过 /find 命令触发）。"
                                            ));
                                        } else {
                                            let mut hits = Vec::<(usize, String)>::new();
                                            for (i, m) in self.state.messages.iter().enumerate() {
                                                // Build a single searchable String per message.
                                                // (Some variants own their concatenated text,
                                                // others borrow; using `Cow` or explicit
                                                // branches avoids borrowck headaches.)
                                                let searchable: String = match m {
                                                    ui::state::ChatMessage::User { text } => text.clone(),
                                                    ui::state::ChatMessage::Assistant { text, .. } => text.clone(),
                                                    ui::state::ChatMessage::Thinking { text, .. } => text.clone(),
                                                    ui::state::ChatMessage::System { text, .. } => text.clone(),
                                                    ui::state::ChatMessage::Tool { name, args, result, .. } => {
                                                        let mut buf = String::with_capacity(name.len() + args.len() + 8);
                                                        buf.push_str(name);
                                                        buf.push(' ');
                                                        buf.push_str(args);
                                                        if let Some(r) = result {
                                                            buf.push(' ');
                                                            buf.push_str(r);
                                                        }
                                                        buf
                                                    }
                                                };
                                                if searchable.to_lowercase().contains(&q.to_lowercase()) {
                                                    let snippet: String = searchable.chars().take(120).collect();
                                                    hits.push((i, snippet));
                                                }
                                            }
                                            let n = hits.len();
                                            let recent: Vec<String> = hits.iter().rev().take(5).map(|(i, s)| format!("  · msg[{i}] {s}")).collect();
                                            self.state.push_log(format!(
                                                "🔍 /find {q:?}（本地扫描）\n━━━━━━━━━━━━━━━━━━\n命中：{n} 条。\n最近 5 条：\n{}\n\n说明：ACP FindInScrollback 帧接入后将支持高亮 / n N 跳转。",
                                                recent.join("\n")
                                            ));
                                        }
                                    }
                                    SlashLocalKind::AcpExport => {
                                        // 复用 share 的 export_conversation_md（导出会话 transcript）
                                        let export_res = export_conversation_md(&self.state);
                                        match export_res {
                                            Ok(path) => self.state.push_log(format!(
                                                "（本地）transcript 导出为 Markdown：\n  路径：{}\n  用法：/export 或 /transcript\n  与 /share 的区别：两者当前均落盘至 ~/.grodex/exports/；ACP Export 帧接入后将支持 JSON / txt 等多格式。",
                                                path.display()
                                            )),
                                            Err(e) => self.state.messages.push(ui::state::ChatMessage::System {
                                                text: format!("导出失败：{e}"),
                                                is_error: true,
                                            }),
                                        }
                                    }
                                    SlashLocalKind::AcpCopy => {
                                        // 本地：找最近一条 Assistant 消息，直接写到 ~/.grodex/.copy-buffer
                                        // 同时教用户怎么用终端选择复制。
                                        let last_asst = self.state.messages.iter().rev().find_map(|m| match m {
                                            ui::state::ChatMessage::Assistant { text, .. } if !text.is_empty() => Some(text.clone()),
                                            _ => None,
                                        });
                                        let home = std::env::var_os("HOME");
                                        let buf_path = home.as_ref().map(|h| std::path::Path::new(h).join(".grodex/.copy-buffer"));
                                        let mut out = String::from("📋 copy 最近回复（本地）\n━━━━━━━━━━━━━━━━━━\n");
                                        match (last_asst, buf_path) {
                                            (Some(txt), Some(bp)) => {
                                                let n = txt.chars().count();
                                                match std::fs::write(&bp, &txt) {
                                                    Ok(()) => out.push_str(&format!(
                                                        "✓ 已写入临时缓冲区：{}\n  长度：{n} 字符\n", bp.display()
                                                    )),
                                                    Err(e) => out.push_str(&format!("✗ 写缓冲区失败：{e}\n")),
                                                }
                                                let preview: String = txt.chars().take(160).collect();
                                                out.push_str(&format!("  预览：{preview}…\n"));
                                            }
                                            (None, _) => out.push_str("（无 Assistant 消息可复制）\n"),
                                            _ => {}
                                        }
                                        out.push_str("\n提示：TUI 无系统剪贴板接入（需 arboard / clipboard 且跨终端不一定可用）。\n\
                                                      macOS 终端按住 Option 拖动可选中文本，Cmd+C 复制。\n\
                                                      或取缓冲区：cat ~/.grodex/.copy-buffer | pbcopy  \nargs={args:?}");
                                        self.state.push_log(out);
                                    }
                                    SlashLocalKind::AcpVoice => {
                                        self.state.push_log(format!(
                                            "🎙️ 语音输入 voice {args:?}（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                             · TUI 麦克风未接入 cpal / rodio，当前不可直接录音。\n\
                                             · 替代方案（Seedance 插件）：\n                                                 Trae 已内置 Seedance AI 视频生成插件，若需语音转写可用外部工具后粘贴文本。\n\
                                                 或终端先转写：whisper-cli input.wav 2>/tmp/out.txt && cat /tmp/out.txt\n\
                                             · 等 ACP VoiceStart/VoiceChunk/VoiceStop 帧加入协议时，将支持实时转写与流式替换输入框。\n\
                                             当前你可以先把文本粘到输入框里按 Enter 发送。"
                                        ));
                                    }
                                    SlashLocalKind::AcpLoop => {
                                        let arg = args.trim().to_lowercase();
                                        if arg.is_empty() || arg == "toggle" || arg == "status" {
                                            if arg == "toggle" {
                                                self.state.loop_mode = !self.state.loop_mode;
                                            }
                                            let st = if self.state.loop_mode { "🟢 on" } else { "⚪ off" };
                                            self.state.push_log(format!(
                                                "🔁 loop / auto-loop（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                                 当前状态：{st}\n\
                                                 用法：/loop on    开启循环调度（Agent 若能产出可执行 self-reflection 任务，则自动进入下一轮）\n\
                                                       /loop off   关闭\n\
                                                       /loop toggle 切换\n\
                                                 说明：Grodex Agent 端需配合 ACP Loop 帧（LoopRequest/Continue/Halt）才能真正进入自主循环。\n\
                                                 目前仅本地保存 flag，Agent 接发消息仍由你手动按 Enter 触发。"
                                            ));
                                        } else {
                                            let want = matches!(arg.as_str(), "on" | "true" | "1" | "enable");
                                            self.state.loop_mode = want;
                                            let st = if want { "🟢 on" } else { "⚪ off" };
                                            self.state.push_log(format!("🔁 loop → {st}（本地 flag，待 ACP Loop 帧接入后 Agent 端会读取）"));
                                        }
                                    }
                                    SlashLocalKind::AcpImagine => {
                                        let prompt = args.trim();
                                        if prompt.is_empty() {
                                            self.state.push_log(String::from(
                                                "🖼️  图片生成 /imagine（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                                 用法：/imagine 一只坐在咖啡杯上的柴犬，插画风格\n\
                                                 · 接入能力：Trae 已内置 Seedream AI 图片生成插件（.trae-cn/plugins/trae-remote-official/seedream）。\n\
                                                 · 当前 TUI 不直接调 MCP 生成图片：ACP Imagine 帧需加入协议（ImagineRequest + Progress + Result）。\n\
                                                 · 替代方案：\n                                                     1) 直接告诉 Agent「帮我生成一张 XXX 图片」— 它会通过 MCP 工具链调用 Seedream。\n                                                     2) 用 Trae 编辑器自带 GenerateImage 功能：直接描述用途 + 尺寸。"
                                            ));
                                        } else {
                                            self.state.push_log(format!(
                                                "🖼️  /imagine {prompt:?}\n（本地）ACP Imagine 帧尚未加入协议。\n\
                                                 建议直接发送给 Agent（去掉开头的 /imagine，自然语言描述「帮我生成一张：{prompt}」），\n\
                                                 Agent 端会通过 MCP Seedream 插件产出图片并回传链接/路径。"
                                            ));
                                        }
                                    }
                                    SlashLocalKind::AcpImagineVideo => {
                                        let prompt = args.trim();
                                        if prompt.is_empty() {
                                            self.state.push_log(String::from(
                                                "🎬 视频生成 /imagine-video（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                                 用法：/imagine-video 一只猫在太空漫步，电影感\n\
                                                 · Trae 已内置 Seedance AI 视频生成插件（.trae-cn/plugins/trae-remote-official/seedance）。\n\
                                                 · 当前 TUI 未直连：ACP ImagineVideo 帧尚缺。\n\
                                                 · 替代：让 Agent 「帮我生成一段视频：XXX」，它会通过 MCP 调 Seedance。"
                                            ));
                                        } else {
                                            self.state.push_log(format!(
                                                "🎬 /imagine-video {prompt:?}\n（本地）ACP ImagineVideo 帧尚未加入协议。\n\
                                                 建议改为自然语言发给 Agent：「帮我生成一段视频：{prompt}」。"
                                            ));
                                        }
                                    }
                                    SlashLocalKind::AcpBtw => {
                                        // btw = by-the-way / 追加而非重置上下文的模式开关
                                        let arg = args.trim().to_lowercase();
                                        let new_val = if arg.is_empty() || arg == "toggle" {
                                            !self.state.btw_mode
                                        } else {
                                            matches!(arg.as_str(), "on" | "true" | "1" | "enable")
                                        };
                                        self.state.btw_mode = new_val;
                                        let st = if new_val { "🟢 on" } else { "⚪ off" };
                                        self.state.push_log(format!(
                                            "💬 btw / by-the-way 模式（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                             当前：{st}\n\
                                             · on  = 后续输入作为对当前话题的追加/补充（语义：BTW，顺便一提）\n\
                                             · off = 默认，正常新一轮对话\n\
                                             说明：ACP Btw 帧接入后，Agent 端会显式识别并调整上下文窗口压缩策略。\n\
                                             当前仅本地保存 flag，可配合 /compact 手动压缩上下文。"
                                        ));
                                    }
                                    SlashLocalKind::AcpFeedback => {
                                        let text = args.trim();
                                        if text.is_empty() {
                                            self.state.push_log(String::from(
                                                "📮 反馈 feedback（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                                 用法：/feedback 这里写你的反馈内容…\n\
                                                 · 本地会把反馈追加到 ~/.grodex/feedback/ 目录下，以时间戳命名。\n\
                                                 · ACP Feedback 帧接入后，将可选匿名/附带会话元数据提交到远端。\n\
                                                 · 若需官方渠道：去 GitHub repo 开 Issue（/release-notes 有版本号，方便复现）。"
                                            ));
                                        } else {
                                            let home = std::env::var_os("HOME");
                                            let mut out = String::new();
                                            if let Some(h) = home {
                                                let dir = std::path::Path::new(&h).join(".grodex").join("feedback");
                                                let _ = std::fs::create_dir_all(&dir);
                                                let ts = std::time::SystemTime::now()
                                                    .duration_since(std::time::UNIX_EPOCH)
                                                    .map(|d| d.as_secs())
                                                    .unwrap_or(0);
                                                let fp = dir.join(format!("fb-{ts}.md"));
                                                let body = format!("# Feedback @ {ts}\n\nProvider: {}\nModel:    {}\nCWD:      {}\n\n## Content\n\n{text}\n",
                                                    self.state.provider_label, self.state.model_label,
                                                    std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default()
                                                );
                                                match std::fs::write(&fp, body) {
                                                    Ok(()) => out.push_str(&format!("✓ 已保存：{}\n", fp.display())),
                                                    Err(e) => out.push_str(&format!("✗ 保存失败：{e}\n")),
                                                }
                                            } else {
                                                out.push_str("✗ $HOME 未定义，无法保存反馈。\n");
                                            }
                                            out.push_str("\n预览：");
                                            out.push_str(text.chars().take(200).collect::<String>().as_str());
                                            out.push_str("…\n\n（ACP Feedback 帧接入后可选择是否附带会话信息）");
                                            self.state.push_log(out);
                                        }
                                    }
                                    SlashLocalKind::AcpAnnouncements => {
                                        // 本地：尝试读 ~/.grodex/announcements.md；若没有就打印版本说明
                                        let home = std::env::var_os("HOME");
                                        let mut out = String::from("📢 系统公告 announcements（本地）\n━━━━━━━━━━━━━━━━━━\n");
                                        let mut read_any = false;
                                        if let Some(h) = &home {
                                            for candidate in [
                                                std::path::Path::new(h).join(".grodex").join("announcements.md"),
                                                std::path::PathBuf::from("ANNOUNCE.md"),
                                                std::path::PathBuf::from("../ANNOUNCE.md"),
                                            ] {
                                                if let Ok(body) = std::fs::read_to_string(&candidate) {
                                                    out.push_str(&format!("来源：{}\n\n", candidate.display()));
                                                    out.push_str(body.lines().take(40).collect::<Vec<_>>().join("\n").as_str());
                                                    read_any = true;
                                                    break;
                                                }
                                            }
                                        }
                                        if !read_any {
                                            out.push_str("（暂无公告文件）\n\n\
                                                · Grodex 版本：可用 /release-notes 查看\n\
                                                · 重大变更会写在 CHANGELOG.md 顶部\n\
                                                · ACP Announcements 帧接入后将支持拉取远端推送 + 未读红点。\n");
                                        }
                                        if !args.trim().is_empty() {
                                            out.push_str(&format!("\nargs={args:?}（本命令不接受远端子命令）"));
                                        }
                                        self.state.push_log(out);
                                    }
                                    SlashLocalKind::AcpTimestamps => {
                                        let arg = args.trim().to_lowercase();
                                        let new_val = if arg.is_empty() || arg == "toggle" || arg == "status" {
                                            if arg == "status" { self.state.show_timestamps } else { !self.state.show_timestamps }
                                        } else {
                                            matches!(arg.as_str(), "on" | "true" | "1" | "enable")
                                        };
                                        if arg != "status" && (arg.is_empty() || arg != "status") {
                                            self.state.show_timestamps = new_val;
                                        }
                                        let st = if self.state.show_timestamps { "🟢 on" } else { "⚪ off" };
                                        self.state.push_log(format!(
                                            "🕒 timestamps / 时间戳显示（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                             当前：{st}\n\
                                             用法：/timestamps on | off | toggle | status\n\
                                             说明：ACP Timestamps 帧接入后将统一事件/消息的 monotonic 时间源（NTP sync + 单调时钟）。\n\
                                             当前仅本地渲染开关：若 on，后续 Message 展示会附带本地时间（若 UI 面板渲染支持，见 render.rs）。"
                                        ));
                                    }
                                    SlashLocalKind::AcpTimeline => {
                                        // 本地：把 messages 压缩成一条 turn 时间线
                                        let mut lines = vec![String::from("⏱️  Turn Timeline（本地）\n━━━━━━━━━━━━━━━━━━")];
                                        let mut turn_idx = 0usize;
                                        for (i, m) in self.state.messages.iter().enumerate() {
                                            let line = match m {
                                                ui::state::ChatMessage::User { text } => {
                                                    turn_idx += 1;
                                                    let p: String = text.chars().take(80).collect();
                                                    format!("  T{turn_idx:>2}  [ USR] msg[{i:<3}] {p}")
                                                }
                                                ui::state::ChatMessage::Assistant { text, .. } => {
                                                    let p: String = text.chars().take(80).collect();
                                                    format!("       [ AST] msg[{i:<3}] {p}")
                                                }
                                                ui::state::ChatMessage::Thinking { text, .. } => {
                                                    let p: String = text.chars().take(80).collect();
                                                    format!("       [THNK] msg[{i:<3}] {p}")
                                                }
                                                ui::state::ChatMessage::Tool { name, args, result, is_error, .. } => {
                                                    let tag = if *is_error { " T✗" } else { "TOOL" };
                                                    let combined = if let Some(r) = result {
                                                        format!("({name}) args={args:?} out={r:?}")
                                                    } else {
                                                        format!("({name}) args={args:?}")
                                                    };
                                                    let p: String = combined.chars().take(80).collect();
                                                    format!("       [{tag:>4}] msg[{i:<3}] {p}")
                                                }
                                                ui::state::ChatMessage::System { text, is_error } => {
                                                    let tag = if *is_error { "SYS✗" } else { " SYS" };
                                                    let p: String = text.chars().take(80).collect();
                                                    format!("       [{tag:>4}] msg[{i:<3}] {p}")
                                                }
                                            };
                                            lines.push(line);
                                        }
                                        lines.push(format!("\n总 turn 数：{turn_idx} | 总消息：{} 条 | 事件：{} 条",
                                            self.state.messages.len(), self.state.events.len()));
                                        lines.push(String::from("\n说明：ACP Timeline 帧接入后会带上 monotonic_ns + wall_clock + gen/seq + operation_id，支持精确定位 turn 时延与 hole 检测。"));
                                        if !args.trim().is_empty() { lines.push(format!("args={args:?}")); }
                                        self.state.push_log(lines.join("\n"));
                                    }
                                    SlashLocalKind::AcpImportHistory => {
                                        self.state.push_log(format!(
                                            "📥 导入历史会话 import-claude {args:?}（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                             · Grodex 会话格式：~/.grodex/sessions/<session-id>.jsonl（每行一事件）\n\
                                             · Claude / ChatGPT / Grok 导出 → Grodex 的转换脚本：当前尚未内置。\n\
                                             · 替代方案（可行）：\n                                                 1) 先把导出的 JSON / JSONL 放到 ~/.grodex/imports/<name>.json\n                                                 2) 用自然语言告诉 Agent：「帮我把这个导出文件转成 Grodex session 事件格式并追加到当前会话」\n\
                                                    Agent 能直接读文件 + 发 System/Tool 消息伪造时间线。\n\
                                             · 若 ACP ImportHistory 帧接入协议：将支持一键导入（含元数据映射、turn 对齐、空行清理）。"
                                        ));
                                    }
                                    SlashLocalKind::AcpLogin => {
                                        let arg = args.trim();
                                        if arg.is_empty() {
                                            let prov = if self.state.provider_label.is_empty() { "<未配置>" } else { self.state.provider_label.as_str() };
                                            self.state.push_log(format!(
                                                "🔐 login（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                                 Grodex 没有云端账户系统，登录 = 在 ~/.grodex/config.toml 里配置 API Key。\n\
                                                 当前 provider：{prov}\n\n\
                                                 用法：/login <provider> <api-key>\n\
                                                   例：/login deepseek sk-xxxxxxxxxxxx\n\
                                                       /login openai   sk-xxxxxxxxxxxx\n\
                                                 写入后会立即更新本地下方 provider 标签（重启生效）。\n\
                                                 或直接：/settings（用 $EDITOR 打开完整配置）。"
                                            ));
                                        } else {
                                            // 格式：/login <provider> <key>
                                            let mut parts = arg.splitn(2, |c: char| c.is_whitespace());
                                            let prov_in = parts.next().unwrap_or("").trim().to_lowercase();
                                            let key_in = parts.next().unwrap_or("").trim().to_string();
                                            if prov_in.is_empty() || key_in.is_empty() {
                                                self.state.push_log(String::from("用法：/login <provider> <api-key>  （两个参数都必填）"));
                                            } else {
                                                let home = std::env::var_os("HOME");
                                                let mut out = String::new();
                                                if let Some(h) = home {
                                                    let dot = std::path::Path::new(&h).join(".grodex");
                                                    let _ = std::fs::create_dir_all(&dot);
                                                    let cfg_p = dot.join("config.toml");
                                                    let existing = std::fs::read_to_string(&cfg_p).unwrap_or_default();
                                                    // 简单替换/追加 [api_keys] 块 provider=
                                                    let has_section = existing.contains("[api_keys]");
                                                    let mut new_body = existing;
                                                    if !has_section {
                                                        new_body.push_str("\n[api_keys]\n");
                                                    }
                                                    // 看 provider 那行是否存在
                                                    let needle_prefix = format!("{prov_in}");
                                                    let mut replaced = false;
                                                    new_body = new_body.lines().map(|l| {
                                                        let t = l.trim_start();
                                                        if !replaced && t.starts_with(&needle_prefix) && (t.contains('=') || t.contains(':')) {
                                                            replaced = true;
                                                            format!("{prov_in} = \"{key_in}\"")
                                                        } else { l.to_string() }
                                                    }).collect::<Vec<_>>().join("\n");
                                                    if !replaced {
                                                        new_body.push_str(&format!("\n{prov_in} = \"{key_in}\"\n"));
                                                    }
                                                    match std::fs::write(&cfg_p, new_body) {
                                                        Ok(()) => {
                                                            out.push_str(&format!("✓ 已写入 {} → config.toml（{prov_in}）\n", cfg_p.display()));
                                                            out.push_str("  ⚠ 本 TUI runtime 中的 CredentialBroker 已在启动时读取过一次，\n\
                                                                             新 key 下次重启 grodex tui 生效。\n");
                                                            // 顺手更新 provider_label 让 UI 看起来更对
                                                            if self.state.provider_label.is_empty() || self.state.provider_label == "default" {
                                                                self.state.provider_label = prov_in.clone();
                                                            }
                                                        }
                                                        Err(e) => out.push_str(&format!("✗ 写 config.toml 失败：{e}\n")),
                                                    }
                                                } else {
                                                    out.push_str("✗ $HOME 未定义，无法写入配置。\n");
                                                }
                                                self.state.push_log(out);
                                            }
                                        }
                                    }
                                    SlashLocalKind::AcpLogout => {
                                        let arg = args.trim().to_lowercase();
                                        if arg.is_empty() || arg == "all" {
                                            let home = std::env::var_os("HOME");
                                            if let Some(h) = home {
                                                let cfg_p = std::path::Path::new(&h).join(".grodex").join("config.toml");
                                                if let Ok(body) = std::fs::read_to_string(&cfg_p) {
                                                    // 把 [api_keys] 段里所有行注释掉（保留结构）
                                                    let mut in_api_keys = false;
                                                    let mut new_lines: Vec<String> = Vec::new();
                                                    for l in body.lines() {
                                                        let t = l.trim();
                                                        if t.starts_with('[') && t.ends_with(']') {
                                                            in_api_keys = t == "[api_keys]";
                                                            new_lines.push(l.to_string());
                                                        } else if in_api_keys && t.contains('=') && !t.starts_with('#') {
                                                            new_lines.push(format!("# [logged-out] {l}"));
                                                        } else {
                                                            new_lines.push(l.to_string());
                                                        }
                                                    }
                                                    match std::fs::write(&cfg_p, new_lines.join("\n")) {
                                                        Ok(()) => self.state.push_log(format!(
                                                            "🚪 logout（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                                             ✓ 已把 {cfg_p:?} 的 [api_keys] 全部注释掉（前缀 # [logged-out]）。\n\
                                                             重启 grodex tui 生效。Grodex 无云端 session，所以不会登出任何远端账户。"
                                                        )),
                                                        Err(e) => self.state.push_log(format!("✗ 写 config.toml 失败：{e}")),
                                                    }
                                                } else {
                                                    self.state.push_log(String::from("logout：config.toml 不存在或不可读（无需操作）。"));
                                                }
                                            }
                                        } else {
                                            self.state.push_log(format!(
                                                "🚪 logout {arg:?}：若想清除单个 provider，用 /settings 打开 config.toml 手动删除对应行。\n\
                                                 当前 grodex 无远端会话，logout = 本地清 API Key。"
                                            ));
                                        }
                                    }
                                    SlashLocalKind::AcpToggleMouse => {
                                        self.state.push_log(format!(
                                            "🖱️  toggle-mouse-reporting {args:?}（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                             · crossterm 本身支持 mouse event（MouseDown/Up/Scroll/Drag），TUI 的 scroll_up/down 已接入滚轮事件（一次滚 3 行）。\n\
                                             · 完整 mouse reporting = 允许点击跳转审批、拖动输入框大小等。当前未完整绑定交互回调。\n\
                                             · 若要临时开启：修改 crossterm Terminal 初始化时 enable_mouse_capture() 即可。\n\
                                             · ACP ToggleMouse 帧接入后将支持：客户端发状态 → 后端按鼠标事件模拟 keystroke。"
                                        ));
                                    }
                                    SlashLocalKind::AcpPrivacy => {
                                        use std::path::Path;
                                        let arg = args.trim().to_lowercase();
                                        let home = std::env::var_os("HOME");
                                        let dot = home.as_ref().map(|h| Path::new(h).join(".grodex"));
                                        let mut out = String::from("🔒 隐私中心 privacy（本地）\n━━━━━━━━━━━━━━━━━━\n");
                                        match arg.as_str() {
                                            "data" | "list" | "show" | "" => {
                                                out.push_str("Grodex 本地数据目录：~/.grodex/\n\n");
                                                if let Some(dot) = &dot {
                                                    out.push_str(&format!("路径：{}\n", dot.display()));
                                                    let expected = [
                                                        ("config.toml",    "配置：provider/model/API key"),
                                                        ("sessions/",      "历史会话：每 session 一个 JSONL"),
                                                        ("exports/",       "/export 导出的 Markdown"),
                                                        ("skills/",        "本地 Skills Markdown"),
                                                        ("mcp/",           "MCP server 描述"),
                                                        ("plugins/",       "本地插件"),
                                                        ("feedback/",      "/feedback 存档"),
                                                        ("remember.md",    "长期记忆（若有）"),
                                                    ];
                                                    for (name, desc) in expected {
                                                        let p = dot.join(name.trim_end_matches('/'));
                                                        let mark = if p.exists() { "✓" } else { "·" };
                                                        out.push_str(&format!("  {mark} {name:<14} {desc}\n"));
                                                    }
                                                    // 统计一下 sessions / exports 数量
                                                    for (label, sub) in [("sessions", "sessions"), ("exports", "exports")] {
                                                        let d = dot.join(sub);
                                                        if let Ok(rd) = d.read_dir() {
                                                            let n = rd.flatten().count();
                                                            out.push_str(&format!("  ↳ {label:<14} {n} 项\n"));
                                                        }
                                                    }
                                                    let total = if let Ok(md) = dot.metadata() {
                                                        if md.is_dir() {
                                                            let mut acc = 0u64;
                                                            if let Ok(rd) = dot.read_dir() {
                                                                for e in rd.flatten() {
                                                                    if let Ok(m2) = e.metadata() {
                                                                        if m2.is_file() { acc += m2.len(); }
                                                                    }
                                                                }
                                                            }
                                                            acc
                                                        } else { 0 }
                                                    } else { 0 };
                                                    out.push_str(&format!("\n顶层文件总大小 ≈ {} KB\n", total / 1024));
                                                }
                                                out.push_str("\n子命令：/privacy data    本视图\n\
                                                                 /privacy wipe    打印手动清理命令\n");
                                            }
                                            "wipe" | "delete" | "clear" => {
                                                out.push_str("（安全）为避免误删，本命令只给出执行路径，请手动在 shell 执行：\n\n\
                                                              # 清历史会话（保留配置）\n  rm -rf ~/.grodex/sessions/\n  rm -rf ~/.grodex/exports/\n  rm -rf ~/.grodex/feedback/\n\n\
                                                              # 清 API Key（谨慎！下次得重新 /login）\n  # rm -f  ~/.grodex/config.toml\n\n\
                                                              # 清全部\n  # rm -rf ~/.grodex/\n\n\
                                                              说明：ACP Privacy 帧接入后将支持一键 wipe + Agent 端同步擦除内存中的上下文缓存。");
                                            }
                                            other => {
                                                out.push_str(&format!("未知子命令：{other:?}\n可用：data | wipe"));
                                            }
                                        }
                                        self.state.push_log(out);
                                    }
                                    SlashLocalKind::AcpEditPrompt => {
                                        // 用 $EDITOR 打开临时文件，用户编辑保存后把内容读回输入框 buffer
                                        let editor = std::env::var("EDITOR").ok().filter(|s| !s.is_empty());
                                        match editor {
                                            None => self.state.push_log(String::from(
                                                "✏️  edit-prompt（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                                 没有 $EDITOR 环境变量，无法调出外部编辑器。\n\
                                                 请先执行：export EDITOR=vim   # 或 nvim / nano / code -w\n\
                                                 然后重开 TUI 再运行 /edit-prompt。\n\
                                                 说明：ACP EditPrompt 帧接入后将支持 minimal-mode 浮层面板 + 保存即回填 buffer。"
                                            )),
                                            Some(ed) => {
                                                // 取当前输入框内容作为 seed
                                                let seed = self.state.draft_text();
                                                let tmp = std::env::temp_dir().join(format!("grodex-prompt-{}.md",
                                                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)));
                                                if let Err(e) = std::fs::write(&tmp, &seed) {
                                                    self.state.push_log(format!("✗ 写临时草稿失败：{e}"));
                                                } else {
                                                    match std::process::Command::new(&ed).arg(&tmp).status() {
                                                        Ok(st) if st.success() => {
                                                            match std::fs::read_to_string(&tmp) {
                                                                Ok(new_text) => {
                                                                    // 去掉 \r，保留 \n
                                                                    let cleaned: String = new_text.chars().filter(|c| *c != '\r').collect();
                                                                    let n_old = seed.chars().count();
                                                                    let n_new = cleaned.chars().count();
                                                                    self.state.set_draft_text(cleaned);
                                                                    let _ = std::fs::remove_file(&tmp);
                                                                    self.state.push_log(format!(
                                                                        "✓ 草稿已回填：{n_old} → {n_new} 字符（临时文件已删）。\n\
                                                                         现在输入框中就是编辑后的内容，按 Enter 发送。\n编辑器：{ed}"
                                                                    ));
                                                                }
                                                                Err(e) => self.state.push_log(format!("✗ 读回填文件失败：{e}")),
                                                            }
                                                        }
                                                        Ok(st) => self.state.push_log(format!("编辑器 {ed} 非零退出 {st:?}，保持原草稿不变。临时文件：{}", tmp.display())),
                                                        Err(e) => self.state.push_log(format!("✗ 启动编辑器 {ed} 失败：{e}")),
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    SlashLocalKind::AcpExpand => {
                                        self.state.push_log(format!(
                                            "🔲 expand / fullscreen {args:?}（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                             · Grodex TUI 当前只有一种全屏布局（single-column：StatusBar → Approvals → Conversation → TurnStatus → PromptWidget → ShortcutsBar）。\n\
                                             · Grok 的 minimal/fullscreen 双模式：minimal 只显示 PromptWidget（隐去顶部所有面板），fullscreen 正常显示。\n\
                                             · 实现建议（后续若做）：新增 ui_mode enum {{ Minimal, Fullscreen }} 参与 layout 计算（layout.rs）。\n\
                                             · ACP Expand 帧接入后将支持远端控制 + 渲染回传（截图模式/录屏模式切换）。"
                                        ));
                                    }
                                    SlashLocalKind::AcpDashboard => {
                                        let msgs = self.state.messages.len();
                                        let evs = self.state.events.len();
                                        let turns = self.state.messages.iter().filter(|m| matches!(m, ui::state::ChatMessage::User { .. })).count();
                                        let tools = self.state.active_tool_count();
                                        let appr = self.state.pending_approvals.len();
                                        let prov = if self.state.provider_label.is_empty() { "<未配置>" } else { self.state.provider_label.as_str() };
                                        let model = if self.state.model_label.is_empty() { "<未配置>" } else { self.state.model_label.as_str() };
                                        let cwd = std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|_| "?".into());
                                        let tasks_total = self.state.tasks.len();
                                        let tasks_done = self.state.tasks.iter().filter(|(_, d)| *d).count();
                                        let aa = if self.state.always_approve { "on" } else { "off" };
                                        let yolo = if self.state.yolo_mode { "on" } else { "off" };
                                        let compact = if self.state.compact_ui_mode { "on" } else { "off" };
                                        let ts = if self.state.show_timestamps { "on" } else { "off" };
                                        let title = self.state.session_title.clone().unwrap_or_else(|| "new session".into());
                                        let sid = self.state.session_id.clone().unwrap_or_else(|| "<none>".into());
                                        let capability_gen = self.state.capability_generation;
                                        let trust = if self.state.workspace_trusted { "trusted ⚠" } else { "untrusted ✓" };
                                        self.state.push_log(format!(
                                            "📊 仪表板 Dashboard（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                             会话\n  标题      : {title}\n  session_id: {sid}\n  turn 数   : {turns}\n  gen       : G={capability_gen}\n\
                                             资源\n  provider  : {prov}\n  model     : {model}\n  trust     : {trust}\n  cwd       : {cwd}\n\
                                             实时\n  messages  : {msgs}\n  events    : {evs}\n  tools     : {tools}\n  approvals  : {appr} (pending)\n\
                                             模式开关\n  always-approve : {aa}\n  yolo / auto    : {yolo}\n  compact-ui     : {compact}\n  timestamps     : {ts}\n  tasks          : {tasks_done}/{tasks_total}\n\
                                             快捷入口\n  /info       同当前视图（更简洁版）\n  /doctor     环境自检\n  /debug      TUI 运行时诊断日志\n  /timeline   turn 时间线\n\n\
                                             说明：ACP Dashboard 帧接入后会替换为远端聚合（全局会话量、token usage、credential 租约健康度、rollout 进度）。\nargs={args:?}"
                                        ));
                                    }

                                    // ── Grodex-specific extensions ─────────────────────
                                    SlashLocalKind::GrodexTrust => {
                                        if args.is_empty() {
                                            self.state.push_log(format!(
                                                "[Grodex] 当前工作区信任：{}（使用 /trust on 或 /trust off 切换，下次会话生效）",
                                                if self.state.workspace_trusted { "trusted" } else { "untrusted" }
                                            ));
                                        } else {
                                            let want = args.trim().eq_ignore_ascii_case("on") || args.trim().eq_ignore_ascii_case("true");
                                            self.state.workspace_trusted = want;
                                            self.state.push_log(format!(
                                                "[Grodex] 下一会话工作区信任：{}（当前会话保持不变）",
                                                if want { "trusted" } else { "untrusted" }
                                            ));
                                        }
                                    }
                                    SlashLocalKind::GrodexProvider => {
                                        // provider 切换：本地展示 config + 写入（让用户知道要重启）
                                        let arg = args.trim();
                                        let home = std::env::var_os("HOME");
                                        if arg.is_empty() {
                                            let mut out = format!(
                                                "[Grodex] provider（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                                 当前运行时 provider : {}\n 当前运行时 model    : {}\n\n",
                                                if self.state.provider_label.is_empty() { "<空>" } else { self.state.provider_label.as_str() },
                                                if self.state.model_label.is_empty()    { "<空>" } else { self.state.model_label.as_str() },
                                            );
                                            if let Some(h) = home {
                                                let cfg_p = std::path::Path::new(&h).join(".grodex").join("config.toml");
                                                if let Ok(body) = std::fs::read_to_string(&cfg_p) {
                                                    let lines: Vec<&str> = body.lines().take(30).collect();
                                                    out.push_str(&format!("config.toml 前 30 行：\n{}\n", lines.join("\n")));
                                                }
                                            }
                                            out.push_str("用法：/provider deepseek | openai | ...\n重启后生效（CredentialBroker 在启动时一次性读取）。");
                                            self.state.push_log(out);
                                        } else {
                                            // 尝试写 config.toml 的 provider=xxx 行（保留其他配置）
                                            let mut out = String::new();
                                            if let Some(h) = home {
                                                let cfg_p = std::path::Path::new(&h).join(".grodex").join("config.toml");
                                                let existing = std::fs::read_to_string(&cfg_p).unwrap_or_default();
                                                let mut replaced = false;
                                                let mut had_top_level_provider = false;
                                                let new_body: String = existing.lines().map(|l| {
                                                    let t = l.trim_start();
                                                    if !t.starts_with('#') && t.starts_with("provider") && t.contains('=') {
                                                        had_top_level_provider = true;
                                                        if !replaced {
                                                            replaced = true;
                                                            format!("provider = \"{arg}\"")
                                                        } else { l.to_string() }
                                                    } else { l.to_string() }
                                                }).collect::<Vec<_>>().join("\n");
                                                let final_body = if !had_top_level_provider {
                                                    format!("provider = \"{arg}\"\n{new_body}")
                                                } else { new_body };
                                                match std::fs::write(&cfg_p, final_body) {
                                                    Ok(()) => {
                                                        out.push_str(&format!("✓ 已把 config.toml provider = {arg:?} 写入\n"));
                                                        self.state.provider_label = arg.to_string();
                                                    }
                                                    Err(e) => out.push_str(&format!("✗ 写失败：{e}\n")),
                                                }
                                            }
                                            out.push_str("⚠ 重启 grodex tui 生效（CredBroker 启动时读取）。");
                                            self.state.push_log(out);
                                        }
                                    }
                                    SlashLocalKind::GrodexShowCwd => {
                                        match std::env::current_dir() {
                                            Ok(cwd) => self.state.push_log(format!("[Grodex] cwd = {}", cwd.display())),
                                            Err(e) => self.state.push_log(format!("[Grodex] 获取 cwd 失败：{e}")),
                                        }
                                        let _ = args;
                                    }
                                    SlashLocalKind::GrodexListTools => {
                                        // 本地列出 Grodex 内置工具名（与 grodex-loop/src 中 BuiltInTool 对齐）
                                        self.state.push_log(format!(
                                            "🛠️  Grodex Built-in Tools（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                             · ReadFileTool      读文件（fail-closed 沙箱校验路径）\n\
                                             · WriteFileTool     写文件（RAII + workspace trust 校验）\n\
                                             · EditTool          行级精确编辑（old_string 唯一匹配）\n\
                                             · ApplyPatchTool    unified diff 应用（三向 hunk 校验）\n\
                                             · ExecTool          子进程执行（Sandbox AccessLevel 约束）\n\
                                             · GlobTool          文件名匹配\n\
                                             · GrepTool          全文搜索（ripgrep 底层）\n\
                                             · LsTool            列目录\n\
                                             共 8 种。若需 MCP 远端工具清单：/mcp  或 /plugins\n\
                                             说明：ACP ListTools 帧接入后，TUI 会展示工具签名 + 权限要求 + 调用样本。\nargs={args:?}"
                                        ));
                                    }
                                    SlashLocalKind::GrodexListModels => {
                                        // 本地：根据 provider_label 打印常见 model 名，让用户对照自己的 config.toml
                                        let prov = if self.state.provider_label.is_empty() { "<未配置>" } else { self.state.provider_label.as_str() };
                                        let model = if self.state.model_label.is_empty()    { "<未配置>" } else { self.state.model_label.as_str() };
                                        let mut out = format!(
                                            "🧠 Grodex 模型列表 models（本地）\n━━━━━━━━━━━━━━━━━━\n\
                                             当前 provider : {prov}\n 当前 model    : {model}\n\n常见供应商模型名（仅参考，实际可用性以你的 API Key 权限为准）：\n"
                                        );
                                        match prov.to_ascii_lowercase().as_str() {
                                            "deepseek" => {
                                                out.push_str("  · deepseek-chat          （主力对话，V3）\n");
                                                out.push_str("  · deepseek-reasoner      （R1 深度推理）\n");
                                                out.push_str("  · deepseek-coder         （代码特长）\n");
                                            }
                                            "openai" | "oai" => {
                                                out.push_str("  · gpt-4o / gpt-4o-mini\n");
                                                out.push_str("  · o1 / o1-preview / o1-mini\n");
                                                out.push_str("  · gpt-4-turbo\n");
                                            }
                                            "anthropic" | "claude" => {
                                                out.push_str("  · claude-3-5-sonnet-20241022\n");
                                                out.push_str("  · claude-3-opus\n");
                                                out.push_str("  · claude-3-haiku\n");
                                            }
                                            "zhipu" | "glm" | "qingyan" => {
                                                out.push_str("  · glm-4-plus  / glm-4-0520\n");
                                                out.push_str("  · glm-4-air   / glm-4-airx\n");
                                                out.push_str("  · glm-4-long  (长上下文)\n");
                                            }
                                            "qwen" | "tongyi" | "dashscope" => {
                                                out.push_str("  · qwen3-max    / qwen3-plus\n");
                                                out.push_str("  · qwen2.5-72b-instruct\n");
                                                out.push_str("  · qwen-long    (长上下文)\n");
                                            }
                                            _ => {
                                                out.push_str("  （供应商未在常见清单中，具体模型名请查阅对应控制台）\n");
                                            }
                                        }
                                        out.push_str("\n怎么切换？\n  1) /settings 打开 config.toml\n  2) 改 model = \"xxx\"\n  3) 重启 grodex tui（CredBroker 在启动时读一次）\n\
                                            说明：ACP ListModels 帧接入后将调用供应商 /v1/models 实时校验你的 key 可见模型。");
                                        self.state.push_log(out);
                                    }
                                    SlashLocalKind::GrodexDebugLog => {
                                        // Local: dump transport+event diagnostics from the
                                        // in-memory log ring buffer directly. Collect first
                                        // to avoid an iter()/push_log borrow conflict.
                                        let n = self.state.logs.len();
                                        let take = n.min(30);
                                        let snapshot: Vec<String> = self.state.logs
                                            .iter()
                                            .rev()
                                            .take(take)
                                            .rev()
                                            .cloned()
                                            .collect();
                                        self.state.push_log(format!("[Grodex] debug/logs：显示最近 {take}/{n} 条 TUI 诊断日志"));
                                        for line in snapshot {
                                            self.state.push_log(format!("  · {line}"));
                                        }
                                        let _ = args;
                                    }
                                    SlashLocalKind::GrodexForget => {
                                        let arg = args.trim();
                                        // 本地实现：清空除了 System 配置外的所有 messages，或按 turn 截断最后一轮
                                        let before = self.state.messages.len();
                                        if arg.is_empty() || arg == "topic" || arg == "all" {
                                            // 保留最开头的 System 欢迎语（第 0 条），其余清掉
                                            if self.state.messages.first().map(|m| matches!(m, ui::state::ChatMessage::System { .. })).unwrap_or(false) {
                                                let welcome = self.state.messages.remove(0);
                                                self.state.messages.clear();
                                                self.state.messages.push(welcome);
                                            } else {
                                                self.state.messages.clear();
                                            }
                                            // 也清 events，避免 timeline 对不上
                                            self.state.events.clear();
                                            let after = self.state.messages.len();
                                            self.state.push_log(format!(
                                                "🧽 forget {arg:?}（本地截断）\n━━━━━━━━━━━━━━━━━━\n\
                                                 messages : {before} → {after}\n\
                                                 events   : 已清空\n\
                                                 说明：已在本地把对话面板重置。\n\
                                                 · ACP Forget 帧接入后，Agent 端的 ContextManager + PromptBuilder 会同步丢弃上下文缓存。\n\
                                                 · 当前若正在流式中，Agent 端仍可能保留记忆；可发送下一轮消息时告诉它「请忘记之前的对话，重新开始」来达成双端一致。"
                                            ));
                                        } else {
                                            // 数字：截断最后 N 个 turn（每个 turn = 1 User 消息）
                                            if let Ok(n) = arg.parse::<usize>() {
                                                if n == 0 {
                                                    self.state.push_log(String::from("用法：/forget <N>    忘记最后 N 个 turn；或 /forget（= /forget all）"));
                                                } else {
                                                    // 先反向找第 n 个 User 出现的位置
                                                    let mut user_seen = 0usize;
                                                    let mut cut_from = 0usize;
                                                    for (i, m) in self.state.messages.iter().enumerate().rev() {
                                                        if matches!(m, ui::state::ChatMessage::User { .. }) {
                                                            user_seen += 1;
                                                            if user_seen == n {
                                                                cut_from = i;
                                                                break;
                                                            }
                                                        }
                                                    }
                                                    if user_seen < n {
                                                        // 全清（保留欢迎语）
                                                        if self.state.messages.first().map(|m| matches!(m, ui::state::ChatMessage::System { .. })).unwrap_or(false) {
                                                            let w = self.state.messages.remove(0);
                                                            self.state.messages.clear();
                                                            self.state.messages.push(w);
                                                        } else {
                                                            self.state.messages.clear();
                                                        }
                                                    } else {
                                                        self.state.messages.truncate(cut_from);
                                                    }
                                                    let after = self.state.messages.len();
                                                    self.state.push_log(format!(
                                                        "🧽 forget {n} turn(s)（本地截断）\n  messages: {before} → {after}\n  （ACP Forget 帧接入后 Agent 端同步）"
                                                    ));
                                                }
                                            } else {
                                                self.state.push_log(format!(
                                                    "[Grodex] forget {arg:?}：参数不识别。\n用法：/forget         清空全部上下文\n\
                                                                     /forget all     同上\n                                                                     /forget <N>     忘记最后 N 个 turn"
                                                ));
                                            }
                                        }
                                    }

                                    // ── Hidden (registered so they never leak to LLM) ──
                                    SlashLocalKind::HiddenGboom => {
                                        // Easter egg: Grok keeps /gboom registered but
                                        // hidden. Fail-closed: if the user actually types
                                        // it, do NOTHING (no-op) and never leak to LLM.
                                        let _ = args;
                                    }
                                    SlashLocalKind::HiddenScrollDebug => {
                                        // Same as GrodexDebugLog but registered under the
                                        // hidden alias. Fail-closed: local only.
                                        let _ = args;
                                    }
                                    SlashLocalKind::HiddenDebug => {
                                        // Debug flag toggle placeholder. Local only.
                                        let _ = args;
                                    }

                                    // ── Fallback: unrecognized /xxx ────────────────────
                                    // FAIL-CLOSED by design. Never, ever forward to LLM.
                                    // `args` carries the original "/name args" text so
                                    // the diagnostic shows exactly what the user typed.
                                    SlashLocalKind::Unsupported => {
                                        let raw = if args.is_empty() { "<bare />".to_string() } else { args };
                                        self.state.push_log(format!(
                                            "[Slash] 未识别的命令 {raw:?}（已本地拦截，不会发送给 LLM）。\n         输入 / 加 Tab 或 ↑/↓ 查看 Grok 全部 70+ 内置命令清单。\n         想把这段文字真正发给模型？请去掉开头的 /，改为自然语言描述。"
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    }
                    CrosstermEvent::Paste(text) => {
                        // Preferred path for pasted text. Modern terminals
                        // send this bracketed event for Cmd-V / Ctrl-V /
                        // Ctrl-Shift-V / middle-click paste. Stripping \r
                        // keeps multi-line pastes as pure \n — aligns
                        // with Alt-Enter's newline convention so the
                        // prompt renders wrapped lines cleanly.
                        let mut sanitized = text;
                        sanitized.retain(|c| c != '\r');
                        if !sanitized.is_empty() {
                            // Force mode: paste only makes sense while
                            // editing the prompt or colon-command buffer.
                            // If the user is in Normal mode we drop into
                            // Prompt mode to make the paste visible.
                            if !matches!(self.state.input_mode,
                                ui::state::InputMode::Prompt
                                | ui::state::InputMode::Command)
                            {
                                self.state.input_mode = ui::state::InputMode::Prompt;
                            }
                            let is_prompt = matches!(self.state.input_mode, ui::state::InputMode::Prompt);
                            let (buf, cur) = if is_prompt {
                                (&mut self.state.input_buffer, &mut self.state.input_cursor)
                            } else {
                                (&mut self.state.command_buffer, &mut self.state.command_cursor)
                            };
                            buf.insert_str(*cur, &sanitized);
                            *cur += sanitized.len();
                            // State might have switched out of prompt
                            // when landing here; refresh slash menu for
                            // the newly-inserted buffer text.
                            if is_prompt {
                                self.state.recompute_slash_menu();
                            }
                        }
                    }
                    // 不处理 Mouse 事件：不捕获鼠标（用 DECSET 1007 alternate
                    // scroll mode 让滚轮变成方向键），终端原生的文本选择和滚
                    // 轮滚动同时可用。参考 codex-rs tui/src/tui/event_stream.rs
                    // 第 250-286 行：mouse events 被 `_ => None` 显式丢弃。
                    _ => {}
                }
        }

        // ── Terminal restore BEFORE printing the resume hint.
        //
        // Guards are dropped explicitly so the alternate screen is left
        // and raw mode is disabled — otherwise the hint would be written
        // inside the alternate screen and vanish the instant the
        // process exits.
        drop(_alt_guard);
        drop(_raw_guard);

        // Resume hint: only print when we actually had a session going.
        // Matches Grok's "Session <id> — resume with: grodex resume <id>"
        // exit message so the user knows exactly how to get back.
        if let Some(sid) = &self.state.session_id {
            if !sid.is_empty() {
                println!(
                    "\n  会话已结束: {sid}\n  恢复对话: grodex resume {sid}\n"
                );
            }
        }

        Ok(())
    }
}

fn handle_cli_command<F: FnMut() -> String>(
    cmd: &str,
    state: &mut TuiAppState,
    transport: &mut dyn TransportAdapter,
    next_cmd_id: &mut F,
) {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return;
    }

    if trimmed == "quit" || trimmed == "q" || trimmed == "exit" {
        return;
    }

    if let Some(rest) = trimmed.strip_prefix("resume ") {
        let session_id_str = rest.trim().to_string();
        if session_id_str.is_empty() {
            state.push_log("用法: :resume <session_id>");
            return;
        }
        let cmd = Command::ResumeSession(ResumeSessionCommand {
            command_id: next_cmd_id(),
            expected_generation: Some(state.capability_generation),
            idempotency_key: None,
            session_id: session_id_str.clone(),
            resume_from: ReplayCursor {
                last_consumed_seq: state.events.last().map(|e| e.seq).unwrap_or(0),
                last_event_id: None,
                mode: ReplayMode::SnapshotThenLive,
            },
            ack_bucket: None,
        });
        match transport.send_command(cmd) {
            Ok(()) => state.push_log(format!("请求恢复 session: {session_id_str}")),
            Err(e) => state.push_log(format!("发送 ResumeSession 失败: {e}")),
        }
        return;
    }

    state.push_log(format!(
        "未知命令: :{trimmed}（支持 :quit :resume <id>）"
    ));
}

// ────────────────────────────────────────────────────────────────────────
// Clipboard helpers — OSC 52 via crossterm when possible, with graceful
// fallback. SetClipboard/ResetClipboard are plain terminal escape
// sequences; they require no extra permission. Reading the clipboard is
// best-effort because bracketed paste is the canonical inbound path and
// OSC 52 "read" is blocked on macOS Terminal.app and some Linux setups.
// ────────────────────────────────────────────────────────────────────────

fn set_clipboard(text: &str) -> Result<usize, String> {
    // OSC 52 ; c ; <base64(text)> BEL — the universal terminal clipboard
    // set sequence supported by xterm-compatible / WezTerm / iTerm2 /
    // kitty / alacritty. We hand-roll it because crossterm 0.28 doesn't
    // expose a SetClipboard command (added only in 0.29+).
    //
    // The sequence is written to stderr, in line with all other terminal
    // control commands in the TUI, so stdout pipes never swallow it.
    use std::io::Write;
    let mut stderr = std::io::stderr();

    // Tiny inline base64 encoder so we don't pull a dep for one call.
    const B64: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    let mut out = Vec::<u8>::with_capacity(4 * ((bytes.len() + 2) / 3));
    let mut i = 0;
    while i + 2 < bytes.len() {
        let n = ((bytes[i] as u32) << 16)
            | ((bytes[i + 1] as u32) << 8)
            | (bytes[i + 2] as u32);
        out.push(B64[((n >> 18) & 0x3F) as usize]);
        out.push(B64[((n >> 12) & 0x3F) as usize]);
        out.push(B64[((n >> 6)  & 0x3F) as usize]);
        out.push(B64[( n        & 0x3F) as usize]);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(B64[((n >> 18) & 0x3F) as usize]);
        out.push(B64[((n >> 12) & 0x3F) as usize]);
        out.extend_from_slice(b"==");
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(B64[((n >> 18) & 0x3F) as usize]);
        out.push(B64[((n >> 12) & 0x3F) as usize]);
        out.push(B64[((n >> 6)  & 0x3F) as usize]);
        out.push(b'=');
    }

    // OSC 52 ; c ; <b64> BEL. 'c' picks the system / CLIPBOARD selection
    // (as opposed to 'p' for PRIMARY). BEL is the shorter terminator;
    // OSC ST ("\x1b\\") would be equivalent but BEL is 1 byte.
    let _ = stderr.write_all(b"\x1b]52;c;")
        .and_then(|_| stderr.write_all(&out))
        .and_then(|_| stderr.write_all(b"\x07"))
        .map_err(|e| format!("OSC 52 write: {e}"))?;
    let _ = stderr.flush();
    Ok(text.chars().count())
}

fn get_clipboard() -> Option<String> {
    // Best-effort only. Bracketed paste from the terminal (Cmd-V /
    // Ctrl-V / middle-click in modern terms) is the canonical inbound
    // path. We deliberately do NOT emit an OSC 52 "?" query here: the
    // response comes back asynchronously through the tty input stream
    // (we'd have to wait for it in the event loop) and most macOS
    // Terminal.app / Linux stock terminals refuse clipboard read queries
    // anyway. Users hit Ctrl-Shift-V with text they already have on
    // their clipboard — the terminal's own paste key does the job via
    // the CrosstermEvent::Paste path above.
    None
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

struct AltScreenGuard;

impl Drop for AltScreenGuard {
    fn drop(&mut self) {
        // 只有 TUI 仍持有终端所有权时才跑 reset。
        // 双重保险：防止 panic hook 已经跑过 hard_terminal_reset() 之后，
        // drop guard 又被再次调用，造成 stderr 被二次写入。
        if !TERMINAL_OWNED.swap(false, Ordering::AcqRel) {
            return;
        }
        hard_terminal_reset();
    }
}

impl TransportAdapter for transport::in_process::InProcessBridge {
    fn send_command(&mut self, cmd: Command) -> Result<()> {
        self.to_agent_tx
            .send(cmd)
            .map_err(|e| anyhow!("发送到 in-process agent 失败: {e}"))
    }

    fn poll_event(&mut self, _timeout: Duration) -> Option<EventEnvelope> {
        self.from_agent_rx.try_recv().ok()
    }
}

impl TransportAdapter for transport::stdio::StdioClient {
    fn send_command(&mut self, cmd: Command) -> Result<()> {
        transport::stdio::StdioClient::send_acp_command(self, &cmd)
    }

    fn poll_event(&mut self, timeout: Duration) -> Option<EventEnvelope> {
        transport::stdio::StdioClient::poll_event(self, timeout)
    }

    fn take_pending_logs(&mut self) -> Vec<String> {
        transport::stdio::StdioClient::take_pending_logs(self)
    }

    fn take_snapshots(
        &mut self,
    ) -> Vec<grodex_protocol::acp::SessionSnapshotPayload> {
        transport::stdio::StdioClient::take_snapshots(self)
    }
}

// ── Helpers (slash-command local implementations) ─────────────────────

/// Expand a leading `~/` in a path string using $HOME. Falls back to the
/// original string unchanged if $HOME is missing.
fn shellexpand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    if path == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| path.to_string());
    }
    path.to_string()
}

/// Scan `~/.grodex/<sub>` (or a cwd-relative fallback) for files whose
/// extension matches `exts`. Returns the sorted list of filenames.
///
/// Missing directories return an empty Vec (never error) — slash-command
/// UIs must degrade gracefully to "nothing found" hints rather than
/// throwing boxed diagnostics.
fn scan_dir_list(_label: &str, tilde_path: &str, exts: &[&str]) -> Vec<String> {
    let expanded = shellexpand_tilde(tilde_path);
    let p = std::path::Path::new(&expanded);
    let mut out: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(p) {
        for entry in rd.flatten() {
            if let Ok(ft) = entry.file_type() {
                if ft.is_file() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if exts.iter().any(|e| name.ends_with(e)) {
                        out.push(name);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// Render the in-memory task list (Plan / Queue / Tasks 共用) into the
/// conversation log via state.push_log.
fn print_tasks(state: &mut ui::state::TuiAppState, header: &str) {
    let n = state.tasks.len();
    let done_n = state.tasks.iter().filter(|(_, d)| *d).count();
    if n == 0 {
        state.push_log(format!("📝 {header}（本地）\n━━━━━━━━━━━━━━━━━━\n（空）\n\n\
            添加方式：/plan 完成设计稿   或   /tasks add 写集成测试   或   /queue add <item>"));
        return;
    }
    let mut body = format!("📝 {header}（本地）{done_n}/{n}\n━━━━━━━━━━━━━━━━━━\n");
    for (i, (desc, done)) in state.tasks.iter().enumerate() {
        let mark = if *done { "✅" } else { "⬜" };
        body.push_str(&format!("  {mark} [{:>2}] {desc}\n", i + 1));
    }
    body.push_str("\n操作：/tasks done <n> / undone <n> / rm <n> / clear");
    state.push_log(body);
}

/// Return a human-readable local-time-ish timestamp without pulling in
/// chrono. We deliberately skip TZ-aware formatting and keep the output
/// stable across platforms (UTC-based Y-m-d H:M:S). For slash-command
/// remember.md bookkeeping this is perfectly legible and deterministic.
fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Convert to UTC date-time components manually to avoid a chrono dep.
    // Formula adapted from public-domain civil-from-days algorithms.
    let days = (secs / 86400) as i64;
    let mut time = secs % 86400;
    let hh = time / 3600;
    time %= 3600;
    let mm = time / 60;
    let ss = time % 60;
    // Days -> Y/M/D (Howard Hinnant civil_from_days, days since 1970-01-01)
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut y: i64 = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo: i64 = if mp < 10 { mp + 3 } else { mp - 9 };
    if mo <= 2 { y += 1; }
    format!("{y:04}-{mo:02}-{d:02} {hh:02}:{mm:02}:{ss:02}Z")
}

/// Parse a /tasks `done <x>` / `undone <x>` argument string and flip the
/// matching entry in state.tasks. Silently no-ops with a user-visible
/// message on bad index / bad parse (fail-friendly UI).
fn mark_task(state: &mut ui::state::TuiAppState, args: &str, done: bool) {
    let idx_s = args.trim();
    match idx_s.parse::<usize>() {
        Ok(idx) if idx >= 1 && idx <= state.tasks.len() => {
            let i = idx - 1;
            state.tasks[i].1 = done;
            let tag = if done { "done ✅" } else { "undone ⬜" };
            state.push_log(format!("（本地）tasks #{idx} → {tag}: {}", state.tasks[i].0));
        }
        other => {
            let hint = match other {
                Ok(bad) => format!("index {bad} 越界（共 {} 项）", state.tasks.len()),
                Err(_)   => format!("无法解析数字：{idx_s:?}"),
            };
            state.push_log(format!("（本地）tasks mark 失败：{hint}。\n用法：/tasks done <1..N>"));
        }
    }
}

/// Serialise the in-memory conversation to a Markdown file under
/// `~/.grodex/exports/`. Returns the absolute path written.
///
/// /share /export /transcript all converge here; ACP Export 帧接入后可
/// 加 JSON / plain-text / HTML 格式。当前只写 Markdown（便于直接阅读）。
fn export_conversation_md(state: &ui::state::TuiAppState) -> Result<std::path::PathBuf> {
    use std::io::Write;

    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("$HOME 未定义，无法写入 exports 目录"))?;
    let dir = std::path::Path::new(&home).join(".grodex").join("exports");
    std::fs::create_dir_all(&dir).with_context(|| format!("创建导出目录失败：{}", dir.display()))?;

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let sid = state.session_id.clone().unwrap_or_else(|| "new-session".into());
    let safe_sid: String = sid.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    let fname = format!("{ts}-{safe_sid}.md");
    let path = dir.join(fname);

    let mut f = std::fs::File::create(&path)
        .with_context(|| format!("创建导出文件失败：{}", path.display()))?;

    writeln!(f, "# Grodex Session Export")?;
    writeln!(f)?;
    writeln!(f, "- session_id: `{}`", state.session_id.clone().unwrap_or_else(|| "<none>".into()))?;
    writeln!(f, "- title:     {}", state.session_title.clone().unwrap_or_else(|| "new session".into()))?;
    writeln!(f, "- provider:  {}", state.provider_label)?;
    writeln!(f, "- model:     {}", state.model_label)?;
    writeln!(f, "- trust:     {}", if state.workspace_trusted { "trusted" } else { "untrusted" })?;
    writeln!(f, "- exported:  <t={ts}>")?;
    writeln!(f)?;
    writeln!(f, "---")?;
    writeln!(f)?;

    for (i, m) in state.messages.iter().enumerate() {
        match m {
            ui::state::ChatMessage::User { text } => {
                writeln!(f, "## User [{i}]")?;
                writeln!(f)?;
                writeln!(f, "{}", text)?;
                writeln!(f)?;
            }
            ui::state::ChatMessage::Assistant { text, done } => {
                writeln!(f, "## Assistant [{i}]{}", if *done { "" } else { " (streaming)" })?;
                writeln!(f)?;
                writeln!(f, "{}", if text.is_empty() { "_（空）_" } else { text.as_str() })?;
                writeln!(f)?;
            }
            ui::state::ChatMessage::Thinking { text, done } => {
                writeln!(f, "## Thinking [{i}]{}", if *done { "" } else { " (streaming)" })?;
                writeln!(f)?;
                if text.is_empty() {
                    writeln!(f, "> _（空）_")?;
                } else {
                    for line in text.split('\n') {
                        writeln!(f, "> {line}")?;
                    }
                }
                writeln!(f)?;
            }
            ui::state::ChatMessage::Tool { name, call_id, args, result, is_error, done, has_result, started_at: _, finished_at: _ } => {
                let status = match (*done, *has_result, *is_error) {
                    (_, true, true)  => "Tool ✗",
                    (_, true, false) => "Tool ✓",
                    (true, false, _) => "Tool ⏳",
                    (false, _, _)    => "Tool 🟡",
                };
                writeln!(f, "### {status}: `{name}` [{i}]")?;
                if let Some(cid) = call_id { writeln!(f, "- call_id: `{cid}`")?; }
                writeln!(f, "- state: done={done} has_result={has_result}")?;
                writeln!(f)?;
                if !args.is_empty() {
                    writeln!(f, "**args:**")?;
                    writeln!(f, "```json")?;
                    writeln!(f, "{args}")?;
                    writeln!(f, "```")?;
                    writeln!(f)?;
                }
                if let Some(r) = result {
                    writeln!(f, "**result:**")?;
                    writeln!(f, "```")?;
                    writeln!(f, "{r}")?;
                    writeln!(f, "```")?;
                    writeln!(f)?;
                }
            }
            ui::state::ChatMessage::System { text, is_error } => {
                let tag = if *is_error { "System ✗" } else { "System" };
                writeln!(f, "> **{tag}** [{i}]: {text}")?;
                writeln!(f)?;
            }
        }
    }

    writeln!(f, "---")?;
    writeln!(f)?;
    writeln!(f, "_共 {} 条 messages / {} 个 events，生成于 Grodex TUI。_",
        state.messages.len(), state.events.len())?;

    f.flush().ok();
    Ok(path)
}
