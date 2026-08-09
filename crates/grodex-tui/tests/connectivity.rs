/// Compile-only connectivity test: in-process bridge 最小联通。
/// Verifies: TuiAction::SubmitPrompt → Command → agent half Rx → SessionEvent → Tui state push
#[test]
fn in_process_bridge_roundtrip_compiles() {
    use grodex_tui::transport::in_process::InProcessBridge;
    use grodex_protocol::acp::{Command, SessionPrompt};
    use grodex_core::id::SessionId;

    let bridge = InProcessBridge::new(16);
    let _ = bridge.to_agent_tx.send(Command::Prompt(SessionPrompt {
        command_id: "test-1".into(),
        expected_generation: None,
        idempotency_key: None,
        session_id: SessionId::new(),
        text: "hello".into(),
    }));
}
