//! ACP stdio transport — JSON-RPC over stdin/stdout.
//!
//! Implements the transport layer from Design Doc 17 §5.
//! Messages are newline-delimited JSON, one per line.
//! The server reads from stdin and writes to stdout.

use crate::acp;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// 一条客户端 → 服务端（TUI → agent）的线消息（JSON line）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "frame_type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// ACP 顶层命令。所有 Command（Prompt/ResolveApproval/ResumeSession/Cancel）统一包这层。
    Command {
        #[serde(flatten)]
        inner: acp::Command,
    },
    /// 客户端 ACK（消费了 seq <= N 的事件，用于背压）。
    Ack { last_consumed_seq: u64 },
    /// 客户端心跳，服务端回 Pong（ServerFrame::Pong）。
    Ping { sent_at_ms: u64 },
}

/// 一条服务端 → 客户端（agent → TUI）的线消息（JSON line）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "frame_type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// ACP EventEnvelope（会话事件单条）。
    Event(acp::EventEnvelope),
    /// Snapshot 首包（Resume 时先推 snapshot，再推 delta）。
    Snapshot(acp::SessionSnapshotPayload),
    /// 对应 Ack 的背压反馈：服务端可按 max_inflight 调节。
    FlowControl {
        inflight_events: u32,
        requested_pause_ms: Option<u32>,
    },
    Pong {
        ping_sent_at_ms: u64,
        pong_at_ms: u64,
    },
    /// 服务端主动心跳（长工具执行期间事件流可能长时间静默，
    /// 周期性 Ping 防止前端把闲置连接误判为断开）。
    /// 客户端无需回复，也不占用事件 seq / inflight 窗口。
    Ping {
        sent_at_ms: u64,
    },
    /// 服务端错误（协议解析错误、命令校验失败）。注意：不是业务错误。
    ProtocolError {
        code: String,
        message: String,
        reference_command_id: Option<String>,
    },
}

/// A JSON-RPC framed message over stdio.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method")]
pub enum AcpMessage {
    #[serde(rename = "initialize")]
    Initialize {
        params: acp::InitializeRequest,
        id: u64,
    },
    #[serde(rename = "session/new")]
    SessionNew {
        params: acp::SessionNewRequest,
        id: u64,
    },
    #[serde(rename = "session/load")]
    SessionLoad {
        params: acp::SessionLoadRequest,
        id: u64,
    },
    #[serde(rename = "session/prompt")]
    SessionPrompt {
        params: acp::SessionPrompt,
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<acp::CommandMeta>,
    },
    #[serde(rename = "session/cancel")]
    SessionCancel {
        params: acp::SessionCancel,
        id: u64,
    },
    /// Resume a session after disconnect — client sends the last seq it
    /// processed; server responds with a snapshot or streams missed events.
    #[serde(rename = "session/resume")]
    SessionResume {
        params: acp::SessionResumeRequest,
        id: u64,
    },
    /// Resolve a pending approval ticket (Allow/Narrow/Deny/Cancel).
    #[serde(rename = "session/resolve_approval")]
    ResolveApproval {
        params: acp::ResolveApprovalRequest,
        id: u64,
    },
    /// ACK from the client for backpressure — confirms receipt up to `acked_seq`.
    #[serde(rename = "session/ack")]
    SessionAck {
        params: acp::SessionAck,
    },
}

/// A response sent to the client.
#[derive(Debug, Serialize)]
pub struct AcpResponse {
    pub id: u64,
    pub result: serde_json::Value,
}

/// A notification (no id) sent to the client.
#[derive(Debug, Serialize)]
pub struct AcpNotification {
    pub method: String,
    pub params: serde_json::Value,
}

/// Stdio transport: reads JSON-RPC from stdin, writes to stdout.
pub struct StdioTransport {
    reader: BufReader<tokio::io::Stdin>,
    writer: tokio::io::Stdout,
}

impl Default for StdioTransport {
    fn default() -> Self { Self::new() }
}

impl StdioTransport {
    pub fn new() -> Self {
        Self {
            reader: BufReader::new(tokio::io::stdin()),
            writer: tokio::io::stdout(),
        }
    }

    /// Read the next message from stdin.
    pub async fn read_message(&mut self) -> Option<AcpMessage> {
        let mut line = String::new();
        match self.reader.read_line(&mut line).await {
            Ok(0) => None, // EOF
            Ok(_) => serde_json::from_str(&line).ok(),
            Err(_) => None,
        }
    }

    /// Send a response to stdout.
    pub async fn send_response(&mut self, response: &AcpResponse) -> std::io::Result<()> {
        let json = serde_json::to_string(response).unwrap_or_default();
        self.writer.write_all(json.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await
    }

    /// Send a notification to stdout.
    pub async fn send_notification(&mut self, notification: &AcpNotification) -> std::io::Result<()> {
        let json = serde_json::to_string(notification).unwrap_or_default();
        self.writer.write_all(json.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await
    }

    /// Send a session update (streaming event) to the client.
    pub async fn send_update(
        &mut self,
        session_id: &str,
        update: acp::UpdateContent,
    ) -> std::io::Result<()> {
        let notification = AcpNotification {
            method: "session/update".into(),
            params: serde_json::json!({
                "session_id": session_id,
                "content": update,
            }),
        };
        self.send_notification(&notification).await
    }

    /// Send a full `EventEnvelope` to the client — the enriched form of
    /// `send_update` that carries `seq`, `event_id`, `parent_event_id`,
    /// `causation_token`, and `generation` for replay, ordering, and
    /// back-pressure (Design Doc 17 §7).
    pub async fn send_envelope(
        &mut self,
        envelope: &acp::EventEnvelope,
    ) -> std::io::Result<()> {
        let notification = AcpNotification {
            method: "session/update".into(),
            params: serde_json::json!({
                "session_id": envelope.session_id,
                "content": envelope.content,
                "envelope": envelope,
            }),
        };
        self.send_notification(&notification).await
    }

    /// Send a `SessionSnapshot` update to the client — used on initial
    /// connect, reconnection, or when the client signals it lost events.
    pub async fn send_snapshot(
        &mut self,
        snapshot: acp::SessionSnapshotPayload,
    ) -> std::io::Result<()> {
        let session_id = snapshot.session_id.to_string();
        self.send_update(
            &session_id,
            acp::UpdateContent::SessionSnapshot { snapshot },
        )
        .await
    }

    /// Send an item-lifecycle event (`ItemStarted`/`ItemAborted`/`ItemReplacement`).
    pub async fn send_item_event(
        &mut self,
        session_id: &str,
        update: acp::UpdateContent,
    ) -> std::io::Result<()> {
        self.send_update(session_id, update).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::{
        ApprovalResolutionKind, ResolveApprovalRequest, SessionAck, SessionResumeRequest,
        SnapshotItem, SessionSnapshotPayload, UpdateContent,
    };
    use grodex_core::id::SessionId;

    fn sid() -> SessionId {
        SessionId::new()
    }

    #[test]
    fn session_resume_round_trips() {
        let s = sid();
        let msg = AcpMessage::SessionResume {
            params: SessionResumeRequest {
                session_id: s,
                last_seq: 42,
                idempotency_key: Some("key-abc".into()),
            },
            id: 7,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"method\":\"session/resume\""));
        assert!(json.contains("\"last_seq\":42"));
        assert!(json.contains("\"idempotency_key\":\"key-abc\""));

        let back: AcpMessage = serde_json::from_str(&json).unwrap();
        match back {
            AcpMessage::SessionResume { params, id } => {
                assert_eq!(id, 7);
                assert_eq!(params.last_seq, 42);
                assert_eq!(params.idempotency_key.as_deref(), Some("key-abc"));
            }
            _ => panic!("expected SessionResume"),
        }
    }

    #[test]
    fn resolve_approval_round_trips() {
        let s = sid();
        let msg = AcpMessage::ResolveApproval {
            params: ResolveApprovalRequest {
                session_id: s,
                ticket_id: "tkt-1".into(),
                resolution: ApprovalResolutionKind::Deny,
            },
            id: 3,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"method\":\"session/resolve_approval\""));
        assert!(json.contains("\"ticket_id\":\"tkt-1\""));
        assert!(json.contains("\"deny\""));

        let back: AcpMessage = serde_json::from_str(&json).unwrap();
        match back {
            AcpMessage::ResolveApproval { params, id } => {
                assert_eq!(id, 3);
                assert_eq!(params.ticket_id, "tkt-1");
                assert!(matches!(params.resolution, ApprovalResolutionKind::Deny));
            }
            _ => panic!("expected ResolveApproval"),
        }
    }

    #[test]
    fn resolve_approval_narrow_carries_args() {
        let s = sid();
        let narrowed = serde_json::json!({"path": "/safe/dir"});
        let msg = AcpMessage::ResolveApproval {
            params: ResolveApprovalRequest {
                session_id: s,
                ticket_id: "tkt-2".into(),
                resolution: ApprovalResolutionKind::Narrow { narrowed_args: narrowed },
            },
            id: 9,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: AcpMessage = serde_json::from_str(&json).unwrap();
        match back {
            AcpMessage::ResolveApproval { params, .. } => {
                match params.resolution {
                    ApprovalResolutionKind::Narrow { narrowed_args } => {
                        assert_eq!(narrowed_args["path"], "/safe/dir");
                    }
                    _ => panic!("expected Narrow"),
                }
            }
            _ => panic!("expected ResolveApproval"),
        }
    }

    #[test]
    fn session_ack_round_trips() {
        let s = sid();
        let msg = AcpMessage::SessionAck {
            params: SessionAck {
                session_id: s,
                acked_seq: 100,
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"method\":\"session/ack\""));
        assert!(json.contains("\"acked_seq\":100"));

        let back: AcpMessage = serde_json::from_str(&json).unwrap();
        match back {
            AcpMessage::SessionAck { params } => {
                assert_eq!(params.acked_seq, 100);
            }
            _ => panic!("expected SessionAck"),
        }
    }

    #[test]
    fn session_prompt_carries_optional_meta() {
        let s = sid();
        // Without meta — should omit the field. Params carry inline tri-fields (B4).
        let msg = AcpMessage::SessionPrompt {
            params: acp::SessionPrompt {
                command_id: "cmd-inline-a".into(),
                expected_generation: None,
                idempotency_key: None,
                session_id: s,
                text: "hello".into(),
            },
            id: 1,
            meta: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("meta"));
        assert!(json.contains("\"command_id\":\"cmd-inline-a\""));

        // With meta (legacy outer wrapper) — should also be valid.
        let s2 = sid();
        let msg2 = AcpMessage::SessionPrompt {
            params: acp::SessionPrompt {
                command_id: "cmd-inline-b".into(),
                expected_generation: Some(3),
                idempotency_key: Some("idem-b".into()),
                session_id: s2,
                text: "hello".into(),
            },
            id: 2,
            meta: Some(acp::CommandMeta {
                command_id: Some("cmd-1".into()),
                expected_generation: Some(5),
                idempotency_key: None,
            }),
        };
        let json2 = serde_json::to_string(&msg2).unwrap();
        assert!(json2.contains("\"meta\""));
        assert!(json2.contains("\"command_id\":\"cmd-inline-b\""));
        assert!(json2.contains("\"expected_generation\":3"));

        // Round-trip with meta.
        let back: AcpMessage = serde_json::from_str(&json2).unwrap();
        match back {
            AcpMessage::SessionPrompt { params, meta, .. } => {
                assert_eq!(params.command_id.as_str(), "cmd-inline-b");
                assert_eq!(params.expected_generation, Some(3));
                let m = meta.unwrap();
                assert_eq!(m.command_id.as_deref(), Some("cmd-1"));
                assert_eq!(m.expected_generation, Some(5));
            }
            _ => panic!("expected SessionPrompt"),
        }
    }

    #[test]
    fn snapshot_payload_round_trips() {
        let s = sid();
        let payload = SessionSnapshotPayload {
            session_id: s,
            last_seq: 15,
            generation: 3,
            current_turn_id: Some("turn-7".into()),
            items: vec![
                SnapshotItem {
                    item_id: "i1".into(),
                    item_type: "text".into(),
                    content: "hello world".into(),
                    complete: true,
                },
                SnapshotItem {
                    item_id: "i2".into(),
                    item_type: "tool_call".into(),
                    content: "read_file".into(),
                    complete: false,
                },
            ],
        };
        let update = UpdateContent::SessionSnapshot { snapshot: payload };
        let json = serde_json::to_string(&update).unwrap();
        assert!(json.contains("\"type\":\"SessionSnapshot\""));
        assert!(json.contains("\"last_seq\":15"));
        assert!(json.contains("\"generation\":3"));
        assert!(json.contains("\"current_turn_id\":\"turn-7\""));

        let back: UpdateContent = serde_json::from_str(&json).unwrap();
        match back {
            UpdateContent::SessionSnapshot { snapshot } => {
                assert_eq!(snapshot.last_seq, 15);
                assert_eq!(snapshot.generation, 3);
                assert_eq!(snapshot.current_turn_id.as_deref(), Some("turn-7"));
                assert_eq!(snapshot.items.len(), 2);
                assert_eq!(snapshot.items[0].item_id, "i1");
                assert!(snapshot.items[0].complete);
                assert!(!snapshot.items[1].complete);
            }
            _ => panic!("expected SessionSnapshot"),
        }
    }

    #[test]
    fn item_lifecycle_events_round_trip() {
        let started = UpdateContent::ItemStarted {
            item_id: "i1".into(),
            item_type: "text".into(),
        };
        let json = serde_json::to_string(&started).unwrap();
        assert!(json.contains("\"type\":\"ItemStarted\""));
        let back: UpdateContent = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, UpdateContent::ItemStarted { .. }));

        let aborted = UpdateContent::ItemAborted {
            item_id: "i1".into(),
            reason: "turn cancelled".into(),
        };
        let json = serde_json::to_string(&aborted).unwrap();
        assert!(json.contains("\"type\":\"ItemAborted\""));
        assert!(json.contains("\"reason\":\"turn cancelled\""));

        let replacement = UpdateContent::ItemReplacement {
            item_id: "i2".into(),
            replaces: "i1".into(),
        };
        let json = serde_json::to_string(&replacement).unwrap();
        assert!(json.contains("\"type\":\"ItemReplacement\""));
        assert!(json.contains("\"replaces\":\"i1\""));
    }
}
