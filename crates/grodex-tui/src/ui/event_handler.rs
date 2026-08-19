//! Keyboard event dispatch.
//!
//! Follows the same three-mode split used by grok's `handle_key_event` in
//! `app/input.rs` (Normal / Prompt / Command) but keeps the routing free of
//! focus-stealing shortcuts while a draft is being edited. Key semantic rules
//! enforced here:
//!
//! * **Enter alone submits** the prompt when in Prompt mode — exactly like
//!   grok's `EnterOutcome::Submit`.
//! * **Alt/Shift + Enter inserts a real `\n` newline** (`EnterOutcome::NewlineInserted`).
//!   This is the only way to get a multi-line draft; raw Enter never produces
//!   a stray `\r` or `\n` that later renders as mysterious whitespace.
//! * Tab / Ctrl-I are left unhandled here so future completion UIs can own
//!   them (grok reserves them for prompt-suggestion acceptance).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::render::display_width;
use super::state::{ApprovalOption, BUILTIN_SLASH_COMMANDS, CTRL_C_QUICK_WINDOW_MS, InputMode, SlashLocalKind, TuiAppState};
use grodex_protocol::acp::ApprovalResolution;

#[derive(Debug)]
pub enum Dir {
    Up,
    Down,
}

#[derive(Debug)]
pub enum TuiAction {
    Quit,
    SubmitPrompt { text: String },
    SubmitCommand { cmd: String },
    ResolveApproval { ticket_idx: usize, resolution: ApprovalResolution },
    ScrollUp,
    ScrollDown,
    SwitchApprovalSelection(Dir),
    ToggleMode(InputMode),
    /// Cancel the currently-streaming turn. Sends `Command::Cancel` to the
    /// agent so it stops generating. No-op when not streaming.
    CancelTurn,
    /// Toggle the expansion of the most-recent Thinking (CoT) block.
    /// When expanded, the full CoT text is rendered (scrollable via
    /// normal conversation scroll). When collapsed, truncated to
    /// MAX_LINES=12 / MAX_CHARS=1400 with a hint.
    ToggleThinkingExpansion,
    /// A slash-command was resolved locally. The main loop applies this
    /// immediately (no round-trip through the agent).
    RunSlashLocal { kind: SlashLocalKind, args: String },
    /// Copy the most-recent assistant message text to the system
    /// clipboard. Dispatched from the Ctrl-Shift-C chord so it never
    /// collides with Ctrl-C's "cancel turn / quit" semantics.
    CopyLastAssistant,
    /// Insert raw text into the active input buffer at the current cursor
    /// position. Used for bracketed-paste events (CrosstermEvent::Paste)
    /// as well as Ctrl-Shift-V fallback.
    PasteText { text: String },
    /// Copy the active input buffer (or selection, once selection is
    /// tracked on state) to the system clipboard. Dispatched for
    /// Cmd-C on macOS (SUPER modifier), so users get the standard
    /// terminal-app "copy what I'm currently editing" behaviour
    /// without having to reach for Ctrl-Shift-C (which copies the
    /// most-recent *assistant* output — a different scoped action).
    CopyInputBuffer,
    /// Select-all in the active input buffer. Cmd-A on macOS.
    /// For now this just places the cursor at the end (no visual
    /// selection rect yet) so Cmd-A → Cmd-C still captures the
    /// whole draft; when we add a real selection rectangle the
    /// meaning will stay semantically correct.
    SelectAllInput,
}

pub fn handle_key(key: KeyEvent, state: &mut TuiAppState) -> Option<TuiAction> {
    // Skip release events entirely — we only react on Press/Repeat.
    // Release events only cause double-insert glitches.
    if matches!(key.kind, crossterm::event::KeyEventKind::Release) {
        return None;
    }
    let was_prompt = matches!(state.input_mode, InputMode::Prompt);
    let action = match state.input_mode {
        InputMode::Normal => handle_normal(key, state),
        InputMode::Prompt => handle_prompt(key, state),
        InputMode::Command => handle_command(key, state),
    };
    // 兜底：handle_prompt 内部有多个 return 路径（Alt+Enter、空文本 Enter、
    // \r/\n 过滤、Tab 补全、菜单导航）会跳过末尾的 recompute_slash_menu()，
    // 导致 slash 状态与 input_buffer 不一致（典型症状：执行完一个 slash
    // 命令后再输入 / 菜单不弹出）。这里在 handle_key 层面补一次 recompute，
    // 保证每次按键后菜单状态都是最新的。
    if was_prompt && matches!(state.input_mode, InputMode::Prompt) {
        state.recompute_slash_menu();
    }
    action
}

fn handle_normal(key: KeyEvent, state: &mut TuiAppState) -> Option<TuiAction> {
    // Clipboard shortcuts are handled BEFORE the main match. KeyModifiers
    // is a bitflags type; using a pattern-based match here would either
    // match too loosely (CONTROL | SHIFT pattern captures any set with
    // CONTROL, making plain CONTROL unreachable) or too strictly. An
    // explicit contains() check with SHIFT tested first is the only
    // reliable way to disambiguate Ctrl-C / Ctrl-Shift-C.
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let super_ = key.modifiers.contains(KeyModifiers::SUPER);
    if let KeyCode::Char(ch) = key.code {
        match (ctrl, shift, super_, ch) {
            (true, true, _, 'c') => return Some(TuiAction::CopyLastAssistant),
            (true, true, _, 'v') => return Some(TuiAction::PasteText { text: String::new() }),
            // macOS: Cmd-C without an active input buffer falls back to
            // copying the last assistant message so "Cmd-C to copy the
            // thing I'm looking at" still works intuitively even in
            // Normal mode.
            (_, _, true, 'c') => {
                if state.input_buffer.is_empty() {
                    return Some(TuiAction::CopyLastAssistant);
                } else {
                    return Some(TuiAction::CopyInputBuffer);
                }
            }
            (_, _, true, 'v') => return Some(TuiAction::PasteText { text: String::new() }),
            (true, false, _, 'c') => {
                if state.is_streaming() {
                    return Some(TuiAction::CancelTurn);
                }
                // 两阶段 Ctrl-C 安全退出：第一次不退出，给出 3 秒提示；
                // 第二次才真正退出。对齐 Grok / zsh 的误触防护。
                return try_two_stage_ctrl_c_quit(state);
            }
            _ => {}
        }
    }
    match (key.code, key.modifiers) {
        // 'q' 单键退出已移除：避免用户在 Normal 模式滚动时误触 q 直接
        // 退出（Grok / Codex / Claude Code 都没有单字母退出）。
        // 正确的退出方式是：两次 Ctrl+C 或者 /exit /quit。
        (KeyCode::Char('i'), _) => {
            state.input_mode = InputMode::Prompt;
            state.input_buffer.clear();
            state.input_cursor = 0;
            Some(TuiAction::ToggleMode(InputMode::Prompt))
        }
        (KeyCode::Char(':'), _) => {
            state.input_mode = InputMode::Command;
            state.command_buffer.clear();
            state.command_cursor = 0;
            Some(TuiAction::ToggleMode(InputMode::Command))
        }
        (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
            // Prefer approval-nav when approvals exist and selection can move.
            // Otherwise fall through to conversation line-scroll so Normal mode
            // stays useful even with zero pending approvals (the common case
            // after the agent is done with tools).
            if !state.pending_approvals.is_empty()
                && state.selected_approval_idx < state.pending_approvals.len() - 1
            {
                state.selected_approval_idx += 1;
                Some(TuiAction::SwitchApprovalSelection(Dir::Down))
            } else {
                state.scroll_down(None);
                Some(TuiAction::ScrollDown)
            }
        }
        (KeyCode::Char('k'), _) | (KeyCode::Up, _) => {
            if state.selected_approval_idx > 0 {
                state.selected_approval_idx -= 1;
                Some(TuiAction::SwitchApprovalSelection(Dir::Up))
            } else {
                state.scroll_up();
                Some(TuiAction::ScrollUp)
            }
        }
        (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
            // Ctrl-D = page-down half the screen (Vim convention).
            // No-op if only the approval moved; scroll takes precedence when
            // the approval list is empty.
            if state.pending_approvals.is_empty() {
                for _ in 0..10 { state.scroll_down(None); }
                Some(TuiAction::ScrollDown)
            } else {
                None
            }
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
            if state.pending_approvals.is_empty() {
                for _ in 0..10 { state.scroll_up(); }
                Some(TuiAction::ScrollUp)
            } else {
                None
            }
        }
        (KeyCode::Char('g'), KeyModifiers::NONE) => {
            // gg = jump to top (first keystroke: pretend no-op for now; the
            // full two-key chord would need a pending state). Best-effort: a
            // single 'g' with no approval moves jumps to chat top.
            if state.pending_approvals.is_empty() {
                state.scroll_conversation = 0;
                Some(TuiAction::ScrollUp)
            } else {
                None
            }
        }
        (KeyCode::Char('G'), _) => {
            // Shift+G = jump to bottom of chat. Value is clamped by render.
            // 对齐 Grok：手动触发 "跳到底部" 也重新进入 follow_bottom 模式，
            // 后续 streaming/text delta 继续钉在末尾。
            state.scroll_follow_bottom = true;
            state.scroll_conversation = u16::MAX;
            Some(TuiAction::ScrollDown)
        }
        // Ctrl-N / Ctrl-P = scroll the most-recent Thinking (CoT) block
        // **when collapsed**. When expanded the conversation scroll (j/k,
        // PageUp/Down) handles navigation instead. These must appear
        // BEFORE the generic `(Char('n'), _)` arm which handles approval
        // Narrow — otherwise Ctrl-N would be swallowed by that branch.
        (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
            state.thinking_scroll_down();
            Some(TuiAction::ScrollDown)
        }
        (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
            state.thinking_scroll_up();
            Some(TuiAction::ScrollUp)
        }
        (KeyCode::Char('a'), _) => {
            if !state.pending_approvals.is_empty() {
                Some(TuiAction::ResolveApproval {
                    ticket_idx: state.selected_approval_idx,
                    resolution: ApprovalResolution::Allow,
                })
            } else {
                None
            }
        }
        (KeyCode::Char('d'), _) => {
            if !state.pending_approvals.is_empty() {
                Some(TuiAction::ResolveApproval {
                    ticket_idx: state.selected_approval_idx,
                    resolution: ApprovalResolution::Deny,
                })
            } else {
                None
            }
        }
        (KeyCode::Char('c'), _) => {
            if !state.pending_approvals.is_empty() {
                Some(TuiAction::ResolveApproval {
                    ticket_idx: state.selected_approval_idx,
                    resolution: ApprovalResolution::Cancel,
                })
            } else {
                None
            }
        }
        (KeyCode::Char('n'), _) => {
            if !state.pending_approvals.is_empty() {
                Some(TuiAction::ResolveApproval {
                    ticket_idx: state.selected_approval_idx,
                    resolution: ApprovalResolution::Narrow {
                        narrowed_args: serde_json::Value::Null,
                    },
                })
            } else {
                None
            }
        }
        (KeyCode::PageUp, _) => {
            state.scroll_up();
            Some(TuiAction::ScrollUp)
        }
        (KeyCode::PageDown, _) => {
            state.scroll_down(None);
            Some(TuiAction::ScrollDown)
        }
        (KeyCode::Char('o'), KeyModifiers::CONTROL) => {
            state.thinking_expanded = !state.thinking_expanded;
            Some(TuiAction::ToggleThinkingExpansion)
        }
        _ => None,
    }
}

// ── Prompt mode: Enter submits, Alt-Enter inserts newline ──────────────

fn handle_prompt(key: KeyEvent, state: &mut TuiAppState) -> Option<TuiAction> {
    // ── Slash-menu capture layer (runs FIRST when open) ─────────────────
    //
    // Grok: when the inline slash dropdown is open, ↑/↓/Ctrl-N/P/Tab/Esc
    // belong to the dropdown before any other binding. Only Enter and
    // typing/backspace pass through because the user may still be editing
    // the args portion.
    let menu_open = state.slash.open && !state.slash.matches.is_empty();
    match (&key.code, menu_open) {
        (KeyCode::Up, true) => {
            state.move_slash_selection(-1);
            return None;
        }
        (KeyCode::Down, true) => {
            state.move_slash_selection(1);
            return None;
        }
        (KeyCode::Char('n'), true) | (KeyCode::Char('p'), true)
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            let delta = if matches!(key.code, KeyCode::Char('n')) { 1 } else { -1 };
            state.move_slash_selection(delta);
            return None;
        }
        (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
            // Tab = complete selected command (always tried, even when the
            // user hasn't opened the menu explicitly yet — e.g. `/ex<Tab>`).
            if state.complete_slash_selected() {
                return None;
            }
        }
        (KeyCode::Enter, true) => {
            // Enter on an open slash menu: complete the selected command
            // first, then fall through to the normal Enter=submit logic
            // below. This matches Grok: selecting from the dropdown and
            // pressing Enter executes the command, it doesn't just insert
            // the name and wait.
            //
            // If completion fails (e.g. no selection), fall through anyway
            // so the user's partial text still gets submitted/parsed.
            state.complete_slash_selected();
            // 显式关闭菜单。complete_slash_selected 内部的 recompute 通常
            // 已会关闭菜单（补全后带尾随空格），但显式关闭可防止边界情况
            // （例如补全失败时菜单仍打开导致下一帧 recompute 重新弹出）。
            state.slash.open = false;
            state.slash.matches.clear();
            state.slash.ghost = None;
            // Don't return — fall through to Enter handler below.
        }
        (KeyCode::Esc, true) => {
            // Level-1 Esc: close only the menu. Level-2 (on a second press)
            // falls through to the global Esc rule (clear draft → Normal).
            // Matches Grok's two-level cancel behavior.
            state.slash.open = false;
            state.slash.matches.clear();
            return None;
        }
        _ => {}
    }

    // ── Approval navigation layer (Prompt mode) ────────────────────────
    // When there are pending approvals, ↑/↓ navigates the option list
    // and Enter confirms the highlighted option. This mirrors codex's
    // list-selection approval UX and avoids letter-key shortcuts that
    // conflict with IME (Chinese input method) text entry.
    if !state.pending_approvals.is_empty()
        && key.modifiers == KeyModifiers::NONE
    {
        match key.code {
            KeyCode::Up => {
                if state.approval_option_idx > 0 {
                    state.approval_option_idx -= 1;
                }
                return None;
            }
            KeyCode::Down => {
                if state.approval_option_idx < ApprovalOption::ALL.len() - 1 {
                    state.approval_option_idx += 1;
                }
                return None;
            }
            KeyCode::Enter => {
                let opt = state.current_approval_option();
                let res = match opt {
                    ApprovalOption::Allow => ApprovalResolution::Allow,
                    ApprovalOption::Deny => ApprovalResolution::Deny,
                    ApprovalOption::Cancel => ApprovalResolution::Cancel,
                    ApprovalOption::Narrow => ApprovalResolution::Narrow {
                        narrowed_args: serde_json::Value::Null,
                    },
                };
                return Some(TuiAction::ResolveApproval {
                    ticket_idx: state.selected_approval_idx,
                    resolution: res,
                });
            }
            _ => {}
        }
    }

    let ret = match key.code {
        // ——— macOS ⌘ Cmd shortcuts (SUPER modifier) — runs before
        //    anything else so Ctrl-C can still mean "cancel turn" even
        //    when Cmd-C does "copy input buffer". These are the
        //    bindings users intuitively reach for on a Mac.
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::SUPER) => {
            return Some(TuiAction::CopyInputBuffer);
        }
        KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::SUPER) => {
            return Some(TuiAction::PasteText { text: String::new() });
        }
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::SUPER) => {
            // Cmd-A: put cursor at END so subsequent Cmd-C captures the
            // full buffer (CopyInputBuffer copies regardless of cursor
            // position today; but in future when selection is tracked
            // this will expand selection to the whole buffer first).
            return Some(TuiAction::SelectAllInput);
        }
        KeyCode::Esc => {
            if state.is_streaming() {
                // If there are pending approvals, Esc switches to Normal
                // mode so the user can press a/d/c/n to resolve — NOT
                // cancel the turn (which would leave the tool blocked).
                if !state.pending_approvals.is_empty() {
                    state.input_mode = InputMode::Normal;
                    Some(TuiAction::ToggleMode(InputMode::Normal))
                } else if state.cancel_sent {
                    // Already cancelled — don't send duplicate Cancel
                    // commands (causes "invalid state transition: Idle -> Idle").
                    None
                } else {
                    // Esc while streaming = cancel the turn (don't exit to
                    // Normal, user might want to keep their draft).
                    Some(TuiAction::CancelTurn)
                }
            } else {
                state.input_mode = InputMode::Normal;
                state.input_buffer.clear();
                state.input_cursor = 0;
                Some(TuiAction::ToggleMode(InputMode::Normal))
            }
        }
        // ————— Enter family — mirror grok's PromptWidget::route_enter ————
        KeyCode::Enter => {
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            let shift = key.modifiers.contains(KeyModifiers::SHIFT);
            if alt || shift {
                // Alt/Shift + Enter => insert real newline at cursor.
                // This is the ONLY way a '\n' enters the buffer.
                insert_at_cursor(&mut state.input_buffer, &mut state.input_cursor, "\n");
                return None;
            }
            // Plain Enter => submit.
            // BEFORE forwarding to the agent: check for LOCAL slash commands
            // (/exit, /clear, /help). Matching the Grok contract, server-
            // scoped commands (/compact, /model, etc.) still go to the
            // agent as regular prompt text — the menu is just discoverability.
            let text = std::mem::take(&mut state.input_buffer);
            state.input_cursor = 0;
            // NOTE: intentionally do NOT switch back to Normal mode after
            // submit. Grok stays in prompt-mode forever so you can type a
            // follow-up question without pressing `i` first.
            if text.trim().is_empty() {
                return None;
            }
            if let Some((kind, args)) = try_parse_local_slash(&text) {
                Some(TuiAction::RunSlashLocal { kind, args })
            } else {
                Some(TuiAction::SubmitPrompt { text })
            }
        }
        // ————— Deletion —————————————————————————————————————————————————
        KeyCode::Backspace => {
            if state.input_cursor > 0 {
                let cur = state.input_cursor;
                let mut byte = cur - 1;
                while byte > 0 && !state.input_buffer.is_char_boundary(byte) {
                    byte -= 1;
                }
                state.input_buffer.drain(byte..cur);
                state.input_cursor = byte;
            }
            None
        }
        KeyCode::Delete => {
            let cur = state.input_cursor;
            if cur < state.input_buffer.len() {
                let mut byte = cur + 1;
                while byte < state.input_buffer.len()
                    && !state.input_buffer.is_char_boundary(byte)
                {
                    byte += 1;
                }
                state.input_buffer.drain(cur..byte);
            }
            None
        }
        // ————— Cursor motion ————————————————————————————————————————————
        KeyCode::Left => {
            move_cursor_left(state);
            None
        }
        KeyCode::Right => {
            move_cursor_right(state);
            None
        }
        KeyCode::Home => {
            state.input_cursor = 0;
            None
        }
        KeyCode::End => {
            state.input_cursor = state.input_buffer.len();
            None
        }
        KeyCode::Up => {
            // 单行输入时 ↑ 滚动对话（alternate scroll mode 把滚轮翻译成 ↑/↓）。
            // 多行输入时 ↑ 移动光标到上一行。
            let total_lines = state.input_buffer.chars().filter(|c| *c == '\n').count() + 1;
            if total_lines <= 1 {
                for _ in 0..3 { state.scroll_up(); }
                None
            } else {
                move_cursor_up(state);
                None
            }
        }
        KeyCode::Down => {
            // 单行输入时 ↓ 滚动对话；多行时移动光标。
            let total_lines = state.input_buffer.chars().filter(|c| *c == '\n').count() + 1;
            if total_lines <= 1 {
                for _ in 0..3 { state.scroll_down(None); }
                None
            } else {
                move_cursor_down(state);
                None
            }
        }
        // ————— Ctrl shortcuts ———————————————————————————————————————————
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if state.is_streaming() {
                // Ctrl-C while streaming = cancel the turn (like Grok).
                // Don't clear the input — user might want to keep editing.
                Some(TuiAction::CancelTurn)
            } else if state.input_buffer.is_empty() {
                // 空输入框 + 非 streaming = 用户意图可能是退出。走两阶段
                // Ctrl-C 安全退出（与 Normal 模式下一致），对齐 Grok 的
                // "first Ctrl+C hints, second Ctrl+C quits"。
                try_two_stage_ctrl_c_quit(state)
            } else {
                // 输入框里有内容时，Ctrl-C 只清空输入（等价 Esc）。这样
                // 用户写完一大段 prompt 想重写，按 Ctrl-C 就可丢弃，
                // 不会意外触发退出提示。
                state.input_mode = InputMode::Normal;
                state.input_buffer.clear();
                state.input_cursor = 0;
                // 丢弃输入也重置 Ctrl-C 退出门，避免"先清空再 Ctrl-C
                // 退出"的两阶段被之前的 Ctrl-C 半触发。
                state.ctrl_c_first_press_at = None;
                Some(TuiAction::ToggleMode(InputMode::Normal))
            }
        }
        // Ctrl-J / Ctrl-K = scroll conversation down/up while staying in
        // Prompt mode. Grok uses the same binding so users don't have to
        // Esc → k/j → i just to peek at earlier output.
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.scroll_down(None);
            Some(TuiAction::ScrollDown)
        }
        KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.scroll_up();
            Some(TuiAction::ScrollUp)
        }
        // PageUp / PageDown also work in Prompt mode for larger scrolls.
        KeyCode::PageUp => {
            for _ in 0..10 { state.scroll_up(); }
            Some(TuiAction::ScrollUp)
        }
        KeyCode::PageDown => {
            for _ in 0..10 { state.scroll_down(None); }
            Some(TuiAction::ScrollDown)
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.input_buffer.drain(0..state.input_cursor);
            state.input_cursor = 0;
            None
        }
        // Ctrl-O = toggle Thinking (CoT) expansion. Works in Prompt mode
        // so the user doesn't have to Esc → Normal just to peek at the
        // full reasoning trace.
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.thinking_expanded = !state.thinking_expanded;
            Some(TuiAction::ToggleThinkingExpansion)
        }
        // Ctrl-N / Ctrl-P = scroll the collapsed Thinking (CoT) block.
        // Only active when the slash menu is closed (the slash-menu
        // capture layer above already handles these when the menu is
        // open for command navigation).
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.thinking_scroll_down();
            Some(TuiAction::ScrollDown)
        }
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.thinking_scroll_up();
            Some(TuiAction::ScrollUp)
        }
        // Note: Ctrl-K is bound to scroll-up (see above), not readline
        // kill-to-end. Use Ctrl-U to clear from start to cursor.
        // ————— Character insert —————————————————————————————————————————
        KeyCode::Char(c) => {
            // Mirror grok's per-char insert gate: the ONLY forbidden chars
            // on the character-by-character path are '\r' and '\n'.
            // EXCEPTION: if SHIFT or ALT modifier is present, the terminal
            // is encoding Shift+Enter / Alt+Enter as Char('\r') — insert a
            // real newline instead of dropping it. This fixes Shift+Enter
            // on terminals with kitty keyboard protocol (iTerm2, WezTerm,
            // Kitty, Alacritty) where Shift+Enter arrives as Char('\r')
            // with SHIFT modifier rather than KeyCode::Enter with SHIFT.
            if matches!(c, '\r' | '\n') {
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT)
                {
                    insert_at_cursor(&mut state.input_buffer, &mut state.input_cursor, "\n");
                }
                return None;
            }
            insert_at_cursor(&mut state.input_buffer, &mut state.input_cursor,
                &c.to_string());
            None
        }
        _ => None,
    };
    // Refresh the slash menu after every Prompt-mode keystroke so the
    // dropdown stays in sync with text + cursor. The `recompute` is O(n)
    // over ~20 builtins — essentially free.
    state.recompute_slash_menu();
    ret
}

/// Parse a line that the user pressed Enter on into a slash-command action.
///
/// **CRITICAL GROK-CONSISTENT CONTRACT (FAIL-CLOSED):**
///
/// If `text.trim()` starts with `/`, this function *always* returns
/// `Some(...)` — **it never returns `None`**. Every `/command` is intercepted
/// locally and NONE of them ever reach the LLM as a prompt. This matches
/// Grok's `SlashCommand::run()` return-type contract where every registered
/// command produces either `CommandResult::Action`, `QueueCommand`, or
/// `PassThrough` (local-only results), and an *un*registered `/token` is
/// ALSO handled locally as "unknown command" (shown as a diagnostic, never
/// silently forwarded because `/exot` might be a typo for `/exit` and the
/// user shouldn't burn tokens on it).
///
/// `None` is returned *only* when the text is a regular prompt (does not
/// start with `/` after trimming).
pub(crate) fn try_parse_local_slash(text: &str) -> Option<(SlashLocalKind, String)> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix('/')?;
    let mut splitn = rest.splitn(2, char::is_whitespace);
    let name = splitn.next()?.trim();
    if name.is_empty() {
        // Bare `/` with no command name: treat as unsupported so it doesn't
        // leak to the model. Users will see the menu the moment they type
        // `/` anyway, but Enter on a bare `/` still must not forward.
        return Some((SlashLocalKind::Unsupported, String::new()));
    }
    let args = splitn.next().unwrap_or("").to_string();
    // Special-case: `/mouse on|off|toggle|''` — the sub-command lives in
    // `args` and must be encoded inside the `SlashLocalKind::Mouse`
    // variant itself. BUILTIN_SLASH_COMMANDS still carries a placeholder
    // Mouse entry (with empty sub) so the menu lists the command; but
    // dispatch here overrides with a properly-populated variant so the
    // RunSlashLocal handler actually knows which direction to flip.
    if name.eq_ignore_ascii_case("mouse") {
        let sub = args.trim().to_lowercase();
        return Some((SlashLocalKind::Mouse { sub }, args));
    }
    for cmd in BUILTIN_SLASH_COMMANDS.iter() {
        if cmd.name.eq_ignore_ascii_case(name) {
            // EVERY recognized command is handled locally — the `Forward`
            // variant no longer exists by design (fail-closed).
            return Some((cmd.local.clone(), args));
        }
    }
    // UNRECOGNIZED `/something` → also block locally, never leak to LLM.
    // User typed something like `/exot` (typo for /exit) and shouldn't
    // pay tokens on their mis-typed command name.
    Some((SlashLocalKind::Unsupported, format!("/{name} {args}").trim().to_string()))
}

// ── Command mode — same sanitize as prompt, : prefixed colon —────────────

fn handle_command(key: KeyEvent, state: &mut TuiAppState) -> Option<TuiAction> {
    // macOS ⌘ Cmd shortcuts for command buffer copy/paste. Uses the
    // same CopyInputBuffer / SelectAllInput actions as prompt mode;
    // the main loop picks the *active* buffer (command vs prompt)
    // based on input_mode when dispatching.
    if let KeyCode::Char(ch) = key.code {
        let super_ = key.modifiers.contains(KeyModifiers::SUPER);
        if super_ && matches!(ch, 'c' | 'v' | 'a') {
            return match ch {
                'c' => Some(TuiAction::CopyInputBuffer),
                'v' => Some(TuiAction::PasteText { text: String::new() }),
                'a' => Some(TuiAction::SelectAllInput),
                _ => None,
            };
        }
    }
    match key.code {
        KeyCode::Esc => {
            state.input_mode = InputMode::Normal;
            state.command_buffer.clear();
            state.command_cursor = 0;
            Some(TuiAction::ToggleMode(InputMode::Normal))
        }
        KeyCode::Enter => {
            let cmd = std::mem::take(&mut state.command_buffer);
            state.command_cursor = 0;
            state.input_mode = InputMode::Normal;
            Some(TuiAction::SubmitCommand { cmd })
        }
        KeyCode::Backspace => {
            if state.command_cursor > 0 {
                let cur = state.command_cursor;
                let mut byte = cur - 1;
                while byte > 0 && !state.command_buffer.is_char_boundary(byte) {
                    byte -= 1;
                }
                state.command_buffer.drain(byte..cur);
                state.command_cursor = byte;
            }
            None
        }
        KeyCode::Delete => {
            let cur = state.command_cursor;
            if cur < state.command_buffer.len() {
                let mut byte = cur + 1;
                while byte < state.command_buffer.len()
                    && !state.command_buffer.is_char_boundary(byte)
                {
                    byte += 1;
                }
                state.command_buffer.drain(cur..byte);
            }
            None
        }
        KeyCode::Left => {
            if state.command_cursor > 0 {
                let mut byte = state.command_cursor - 1;
                while byte > 0 && !state.command_buffer.is_char_boundary(byte) {
                    byte -= 1;
                }
                state.command_cursor = byte;
            }
            None
        }
        KeyCode::Right => {
            if state.command_cursor < state.command_buffer.len() {
                let mut byte = state.command_cursor + 1;
                while byte < state.command_buffer.len()
                    && !state.command_buffer.is_char_boundary(byte)
                {
                    byte += 1;
                }
                state.command_cursor = byte;
            }
            None
        }
        KeyCode::Home => { state.command_cursor = 0; None }
        KeyCode::End  => { state.command_cursor = state.command_buffer.len(); None }
        KeyCode::Char(c) => {
            // Same gate as prompt: only '\r'/'\n' are rejected by us, the
            // terminal layer is trusted for everything else.
            if matches!(c, '\r' | '\n') {
                return None;
            }
            let buf = &mut state.command_buffer;
            let cur = &mut state.command_cursor;
            insert_at_cursor(buf, cur, &c.to_string());
            None
        }
        _ => None,
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Two-stage Ctrl-C quit safety gate.
///
/// Returns `Some(TuiAction::Quit)` only when the user has pressed Ctrl-C
/// twice within `CTRL_C_QUICK_WINDOW_MS`; the first press arms the gate
/// and writes a visible hint to `state.logs` so the turn-status / log
/// panel tells them what to do next. Pressing Ctrl-C *once* more than
/// `CTRL_C_QUICK_WINDOW_MS` after the first press arms the gate again
/// from scratch (fail-safe against stale arming).
fn try_two_stage_ctrl_c_quit(state: &mut TuiAppState) -> Option<TuiAction> {
    let now = std::time::Instant::now();
    match state.ctrl_c_first_press_at {
        Some(first) if first.elapsed().as_millis() <= CTRL_C_QUICK_WINDOW_MS as u128 => {
            // Second press within the safety window → real quit.
            state.ctrl_c_first_press_at = None;
            Some(TuiAction::Quit)
        }
        _ => {
            // First press, or stale first press → arm the gate and hint.
            state.ctrl_c_first_press_at = Some(now);
            state.push_log(format!(
                "[quit] 再按一次 Ctrl+C 退出 TUI（{} 秒内有效），或使用 /exit /quit 退出",
                CTRL_C_QUICK_WINDOW_MS / 1000
            ));
            None
        }
    }
}

/// Grok-style single-line sanitizer. The ONLY characters it removes are '\r'
/// (carriage return) and '\n' (line feed). Everything else — tabs, CJK,
/// emojis, curly quotes, RTL marks, you name it — is left untouched so the
/// terminal can render what the user actually typed/pasted.
///
/// Prior over-aggressive sanitization dropped any codepoint outside the
/// ASCII printable range OR that had `Unicode::is_control() == true`, which
/// silently lost characters and made the input feel "weird" — tabs turned to
/// nothing, emoji vanished, and the user blamed mysterious spaces.
pub(crate) fn sanitize_single_line(text: impl Into<String>) -> String {
    let mut text = text.into();
    text.retain(|c| !matches!(c, '\r' | '\n'));
    text
}

/// Insert `snippet` into `buf` at byte-position `cursor`, advancing the
/// cursor past the inserted bytes. Used for single chars and for multi-byte
/// UTF-8 sequences such as CJK characters.
fn insert_at_cursor(buf: &mut String, cursor: &mut usize, snippet: &str) {
    buf.insert_str(*cursor, snippet);
    *cursor += snippet.len();
}

fn move_cursor_left(state: &mut TuiAppState) {
    if state.input_cursor == 0 { return; }
    let mut byte = state.input_cursor - 1;
    while byte > 0 && !state.input_buffer.is_char_boundary(byte) {
        byte -= 1;
    }
    state.input_cursor = byte;
}

fn move_cursor_right(state: &mut TuiAppState) {
    if state.input_cursor >= state.input_buffer.len() { return; }
    let mut byte = state.input_cursor + 1;
    while byte < state.input_buffer.len()
        && !state.input_buffer.is_char_boundary(byte)
    {
        byte += 1;
    }
    state.input_cursor = byte;
}

/// Count bytes from the buffer start up to *and including* the Nth newline,
/// or buffer length if there are fewer than N lines. Used by Up/Down motion.
fn line_start_byte(text: &str, line_idx: usize) -> (usize, usize) {
    // Returns (start_byte, end_byte_before_newline) for the requested line.
    // If line_idx exceeds the last line, returns the last line.
    let mut start = 0usize;
    let mut current_line = 0usize;
    for (i, c) in text.char_indices() {
        if c == '\n' {
            if current_line == line_idx {
                return (start, i);
            }
            current_line += 1;
            start = i + 1;
        }
    }
    (start, text.len())
}

fn cursor_line_and_col(text: &str, cursor_byte: usize) -> (usize, usize) {
    let cursor_byte = cursor_byte.min(text.len());
    let line_idx = text[..cursor_byte].matches('\n').count();
    let (line_start, _) = line_start_byte(text, line_idx);
    // Column is now *displayed width* (not bytes) so Up/Down motion lands at
    // the same visual column across lines with mixed CJK / Latin content.
    // Using raw bytes used to land the cursor *inside* multi-byte UTF-8
    // sequences when jumping between, say, "字" (3 bytes / 2 cells) on one
    // line and "ab" (2 bytes / 2 cells) on the next — the terminal then drew
    // the cursor in "ghost space" between the two halves of a CJK glyph.
    let col_displayed = text[line_start..cursor_byte]
        .chars()
        .map(display_width)
        .sum::<usize>();
    (line_idx, col_displayed)
}

/// Given a displayed-width target column, walk the chars of the target line
/// (bounded by `line_start..line_end`) and return the byte offset in `text`
/// whose cumulative displayed column is ≤ target_col and is on a valid
/// UTF-8 char boundary. This prevents mid-character cursor landings.
fn byte_for_display_col(
    text: &str,
    line_start: usize,
    line_end: usize,
    target_col: usize,
) -> usize {
    let mut byte_pos = line_start;
    let mut col = 0usize;
    for (i, c) in text[line_start..line_end].char_indices() {
        let w = display_width(c);
        if col + w > target_col {
            // Would step past the target visual column; stay where we are.
            // (byte_pos already points at start of this char, which is a
            // valid char boundary.)
            break;
        }
        col += w;
        byte_pos = line_start + i + c.len_utf8();
    }
    byte_pos
}

fn move_cursor_up(state: &mut TuiAppState) {
    let (line, col) = cursor_line_and_col(&state.input_buffer, state.input_cursor);
    if line == 0 {
        state.input_cursor = 0;
        return;
    }
    let (prev_start, prev_end) = line_start_byte(&state.input_buffer, line - 1);
    state.input_cursor = byte_for_display_col(
        &state.input_buffer,
        prev_start,
        prev_end,
        col,
    );
}

fn move_cursor_down(state: &mut TuiAppState) {
    let total_lines = state.input_buffer.chars().filter(|c| *c == '\n').count() + 1;
    let (line, col) = cursor_line_and_col(&state.input_buffer, state.input_cursor);
    if line + 1 >= total_lines {
        // Go to end-of-buffer; floor to char boundary for safety.
        let mut end = state.input_buffer.len();
        while end > 0 && !state.input_buffer.is_char_boundary(end) {
            end -= 1;
        }
        state.input_cursor = end;
        return;
    }
    let (next_start, next_end) = line_start_byte(&state.input_buffer, line + 1);
    state.input_cursor = byte_for_display_col(
        &state.input_buffer,
        next_start,
        next_end,
        col,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press_char(state: &mut TuiAppState, c: char) {
        let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        handle_prompt(key, state);
    }

    fn press_alt_enter(state: &mut TuiAppState) {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        handle_prompt(key, state);
    }

    fn press_enter(state: &mut TuiAppState) -> Option<TuiAction> {
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_prompt(key, state)
    }

    #[test]
    fn plain_enter_submits_and_carries_no_embedded_newline() {
        let mut s = TuiAppState::new();
        s.input_mode = InputMode::Prompt;
        press_char(&mut s, 'h');
        press_char(&mut s, 'i');
        match press_enter(&mut s) {
            Some(TuiAction::SubmitPrompt { text }) => {
                assert_eq!(text, "hi");
                assert!(!text.contains('\n'), "submitted prompt must not carry newlines");
                assert!(!text.contains('\r'), "submitted prompt must not carry CR");
            }
            other => panic!("expected SubmitPrompt, got {other:?}"),
        }
    }

    #[test]
    fn alt_enter_inserts_real_newline_char() {
        let mut s = TuiAppState::new();
        s.input_mode = InputMode::Prompt;
        press_char(&mut s, 'a');
        press_alt_enter(&mut s);
        press_char(&mut s, 'b');
        assert_eq!(s.input_buffer, "a\nb");
        // Subsequent Enter submits the whole thing INCLUDING the newline.
        match press_enter(&mut s) {
            Some(TuiAction::SubmitPrompt { text }) => assert_eq!(text, "a\nb"),
            other => panic!("expected SubmitPrompt, got {other:?}"),
        }
    }

    #[test]
    fn sanitize_single_line_strips_only_cr_lf() {
        // Exactly mirrors grok: only \r and \n go away, everything else stays.
        assert_eq!(sanitize_single_line("one\r\ntwo\nthree\rfour"), "onetwothreefour");
        // Tabs, CJK, emojis, BEL — all untouched.
        assert_eq!(sanitize_single_line("a\tb 字 🚀 \x07"), "a\tb 字 🚀 \x07");
        // Ctrl-U style kill-line never touches sanitizer.
        assert_eq!(sanitize_single_line(""), "");
        // Per-char rejection at the gate.
        let mut s = TuiAppState::new();
        s.input_mode = InputMode::Prompt;
        press_char(&mut s, '\r'); // must be dropped
        press_char(&mut s, '\n'); // must be dropped
        assert_eq!(s.input_buffer, "");
        press_char(&mut s, '字'); // CJK ok
        press_char(&mut s, ' ');  // space ok
        assert_eq!(s.input_buffer, "字 ");
    }

    #[test]
    fn backspace_across_utf8_char_boundaries() {
        let mut s = TuiAppState::new();
        s.input_mode = InputMode::Prompt;
        press_char(&mut s, 'a');
        press_char(&mut s, '字'); // 3 bytes in UTF-8
        assert_eq!(s.input_cursor, "a字".len());
        // Backspace once → drops '字' entirely, cursor at "a" len (1).
        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        handle_prompt(key, &mut s);
        assert_eq!(s.input_buffer, "a");
        assert_eq!(s.input_cursor, 1);
    }

    #[test]
    fn up_down_navigation_across_hard_lines() {
        let mut s = TuiAppState::new();
        s.input_mode = InputMode::Prompt;
        // Build two lines, cursor at end of line 2 after "xy".
        s.input_buffer = "hello\nworld".into();
        s.input_cursor = s.input_buffer.len();

        // Up once → on line 1 at column matching "xy" length.
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        handle_prompt(up, &mut s);
        assert!(s.input_cursor <= "hello".len());

        // Down once → back to end.
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        handle_prompt(down, &mut s);
        assert_eq!(s.input_cursor, "hello\nworld".len());
    }

    #[test]
    fn ctrl_u_clears_from_start_to_cursor() {
        let mut s = TuiAppState::new();
        s.input_mode = InputMode::Prompt;
        s.input_buffer = "hello world".into();
        s.input_cursor = 5; // between "hello" and " world"

        let ctrl_u = KeyEvent::new(
            KeyCode::Char('u'), KeyModifiers::CONTROL);
        handle_prompt(ctrl_u, &mut s);
        assert_eq!(s.input_buffer, " world");
        assert_eq!(s.input_cursor, 0);
    }
}
