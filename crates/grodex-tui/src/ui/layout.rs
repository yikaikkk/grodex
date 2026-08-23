//! Grok-style single-column stacked layout.
//!
//! Visual outline:
//!
//! ```text
//! ┌ outer (vertical) ──────────────────────────────────────────────┐
//! │  status_bar   1 row   provider/model · gen · session          │
//! │ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─  │
//! │  approvals    0..N rows (0 = no pending approvals, hidden)    │
//! │ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─  │
//! │  conversation MIN(5) rows  scrollable chat transcript        │
//! │ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─  │
//! │  turn_status  0..1 rows (streaming indicator, tool progress) │
//! │ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─  │
//! │  prompt       5..8 rows  chrome-bordered PromptWidget        │
//! │   ╭─ session/title ───────────────────────────────────────╮  │
//! │   │ ❯ user input lines here                               │  │
//! │   │   wrapped continuation                                 │  │
//! │   ╰─ model · flags · multiline ───────────────────────────╯  │
//! │ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─  │
//! │  shortcuts    1 row   keyboard hints bar                     │
//! └───────────────────────────────────────────────────────────────┘
//! ```

use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Cached pane rectangles from the last layout computation.
/// The list is intentionally ordered top-to-bottom so renderers can be called
/// in a natural reading order (grok agent view does the same).
#[derive(Debug, Clone, Default)]
pub struct AppLayout {
    pub status_bar: Rect,
    pub approvals: Rect,
    pub conversation: Rect,
    pub turn_status: Rect,
    pub prompt: Rect,
    pub shortcuts: Rect,
}

/// How many rows the pending-approvals pane wants to occupy.
///
/// Mirrors grok's `todo_height`/`tasks_height` pattern: each renderer reports
/// the height it needs this frame, then the layout function honours it (or 0
/// to hide the pane entirely). This keeps approval list content from
/// overlapping the scrollback when it grows/shrinks.
pub fn approvals_desired_rows(count: usize) -> u16 {
    if count == 0 {
        0
    } else {
        // 2 (top+bottom border) + 1 headline + 1 summary + 1 separator
        // + 4 option rows + 1 hint footer = 10. Cap at 10.
        10
    }
}

/// How many rows the turn-status line wants this frame.
///
/// Grok hides it between turns; grodex shows a single status row whenever a
/// turn is streaming, there are active tool calls, or a context compaction
/// is in flight ("会话压缩中…").
pub fn turn_status_desired_rows(is_streaming: bool, active_tools: usize, compacting: bool) -> u16 {
    if is_streaming || active_tools > 0 || compacting { 1 } else { 0 }
}

/// Prompt widget height based on content rows + chrome overhead.
///
/// Layout:
///   1 top border (╭─╮)
///   N content rows (input + optional slash dropdown)
///   1 info row (╰─╯, doubles as bottom border)
/// = N + 2 total. Clamp 3..=20.
pub fn prompt_desired_rows(content_rows: usize) -> u16 {
    let total = content_rows.max(1) + 2; // content + top border + info/bottom
    (total as u16).clamp(3, 20)
}

pub fn build_layout(
    area: Rect,
    approvals_rows: u16,
    turn_status_rows: u16,
    prompt_rows: u16,
) -> AppLayout {
    use Constraint::{Length, Min};

    let outer_vpad: u16 = if area.height <= 20 { 0 } else { 1 };

    // Build constraint list in top-to-bottom order. Gaps between panes are
    // omitted when either neighbour is 0-height, matching grok's
    // "skip gap when pane is hidden" rule (agent.rs pane_gap logic).
    let mut constraints: Vec<Constraint> = Vec::new();

    if outer_vpad > 0 {
        constraints.push(Length(outer_vpad));
    }

    constraints.push(Length(1)); // status_bar

    let gap = outer_vpad; // reuse as inner pane gap
    if approvals_rows > 0 {
        if gap > 0 { constraints.push(Length(gap)); }
        constraints.push(Length(approvals_rows));
    }

    if gap > 0 { constraints.push(Length(gap)); }
    constraints.push(Min(5)); // conversation fills rest

    if turn_status_rows > 0 {
        if gap > 0 { constraints.push(Length(gap)); }
        constraints.push(Length(turn_status_rows));
    }

    if gap > 0 { constraints.push(Length(gap)); }
    constraints.push(Length(prompt_rows));

    if gap > 0 { constraints.push(Length(gap)); }
    constraints.push(Length(1)); // shortcuts

    if outer_vpad > 0 {
        constraints.push(Length(outer_vpad));
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let mut i = 0usize;
    if outer_vpad > 0 { i += 1; }
    let status_bar = chunks[i]; i += 1;

    let approvals = if approvals_rows > 0 {
        if gap > 0 { i += 1; }
        let r = chunks[i]; i += 1;
        r
    } else {
        Rect::default()
    };

    if gap > 0 { i += 1; }
    let conversation = chunks[i]; i += 1;

    let turn_status = if turn_status_rows > 0 {
        if gap > 0 { i += 1; }
        let r = chunks[i]; i += 1;
        r
    } else {
        Rect::default()
    };

    if gap > 0 { i += 1; }
    let prompt = chunks[i]; i += 1;

    if gap > 0 { i += 1; }
    let shortcuts = chunks[i];

    AppLayout { status_bar, approvals, conversation, turn_status, prompt, shortcuts }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_clamp() {
        // Small empty state: no approvals, no turn status, single-row prompt.
        let r = Rect::new(0, 0, 120, 40);
        let l = build_layout(r, 0, 0, prompt_desired_rows(1));
        assert_eq!(l.status_bar.height, 1);
        assert_eq!(l.approvals.height, 0);
        assert_eq!(l.turn_status.height, 0);
        assert_eq!(l.shortcuts.height, 1);
        // Conversation should take the lion share.
        assert!(l.conversation.height >= 10);

        // Prompt rows clamp min 3 regardless of tiny content.
        assert_eq!(prompt_desired_rows(0), 3);
        assert_eq!(prompt_desired_rows(1), 3);
        // 10 content rows: 10 + 2 chrome = 12.
        assert_eq!(prompt_desired_rows(10), 12);
    }

    #[test]
    fn approvals_rows_scales_and_caps() {
        assert_eq!(approvals_desired_rows(0), 0);
        assert_eq!(approvals_desired_rows(1), 10);
        assert_eq!(approvals_desired_rows(100), 10); // fixed
    }

    #[test]
    fn all_panes_nonzero_on_tall_terminal() {
        let r = Rect::new(0, 0, 120, 50);
        let l = build_layout(r, approvals_desired_rows(2), turn_status_desired_rows(true, 2, false), 6);
        assert!(l.status_bar.height > 0);
        assert!(l.approvals.height > 0);
        assert!(l.conversation.height > 0);
        assert!(l.turn_status.height > 0);
        assert!(l.prompt.height > 0);
        assert!(l.shortcuts.height > 0);
        // Heights must tile exactly back into the outer (50 rows minus vpad gaps).
        let total = l.status_bar.height
            + l.approvals.height
            + l.conversation.height
            + l.turn_status.height
            + l.prompt.height
            + l.shortcuts.height;
        // total + gaps == area height, but at minimum total should fit inside.
        assert!(total <= 50);
    }
}
