//! Mailbox — persistent inter-agent messaging.
//!
//! Design Doc 12 §14: Each AgentNode has a persistent mailbox. Messages
//! are delivered via the MailboxRouter, never by directly mutating target
//! memory (otherwise unload+reload would lose messages).
//!
//! Message kinds:
//! - `message`: queue-only, does NOT trigger a Turn. Target must
//!   `mailbox_read` to pull into its transcript.
//! - `followup`: creates a new TaskRun and triggers a Turn (if target is
//!   idle; if busy, queued until current Turn terminates).
//! - `completion`: bounded notification that a child task finished.
//!   Full results require explicit `task_get`.
//! - `control`: consumed by Runtime only (cancel, interrupt, etc.).

use crate::node::AgentId;
use crate::task::TaskId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(Uuid);

impl MessageId {
    pub fn new() -> Self { Self(Uuid::new_v4()) }

    /// Parse from a Uuid string. Returns None if invalid. Used by the
    /// collaboration protocol's read-then-ack confirmation path.
    pub fn from_string(s: &str) -> Option<Self> {
        Uuid::parse_str(s).ok().map(Self)
    }
}

impl std::fmt::Display for MessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl Default for MessageId {
    fn default() -> Self { Self::new() }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Message,
    Followup,
    Completion,
    Control,
    Request,
    Response,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    pub message_id: MessageId,
    pub author_agent_id: AgentId,
    pub target_agent_id: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_task_run_id: Option<TaskId>,
    pub kind: MessageKind,
    pub trigger_turn: bool,
    pub payload: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to: Option<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_at_ms: Option<u64>,
}

impl AgentMessage {
    pub fn message(author: AgentId, target: AgentId, payload: impl Into<String>) -> Self {
        let payload_str = payload.into();
        let preview = make_preview(&payload_str);
        Self {
            message_id: MessageId::new(),
            author_agent_id: author,
            target_agent_id: target,
            related_task_run_id: None,
            kind: MessageKind::Message,
            trigger_turn: false,
            payload: payload_str,
            preview: Some(preview),
            created_at: Utc::now(),
            in_reply_to: None,
            timeout_at_ms: None,
        }
    }

    pub fn followup(
        author: AgentId,
        target: AgentId,
        payload: impl Into<String>,
        related_task: Option<TaskId>,
    ) -> Self {
        let payload_str = payload.into();
        let preview = make_preview(&payload_str);
        Self {
            message_id: MessageId::new(),
            author_agent_id: author,
            target_agent_id: target,
            related_task_run_id: related_task,
            kind: MessageKind::Followup,
            trigger_turn: true,
            payload: payload_str,
            preview: Some(preview),
            created_at: Utc::now(),
            in_reply_to: None,
            timeout_at_ms: None,
        }
    }

    pub fn completion(
        author: AgentId,
        target: AgentId,
        task: TaskId,
        summary: impl Into<String>,
    ) -> Self {
        let summary_str = summary.into();
        let preview = make_preview(&summary_str);
        Self {
            message_id: MessageId::new(),
            author_agent_id: author,
            target_agent_id: target,
            related_task_run_id: Some(task),
            kind: MessageKind::Completion,
            trigger_turn: false,
            payload: summary_str,
            preview: Some(preview),
            created_at: Utc::now(),
            in_reply_to: None,
            timeout_at_ms: None,
        }
    }

    pub fn control(author: AgentId, target: AgentId, payload: impl Into<String>) -> Self {
        Self {
            message_id: MessageId::new(),
            author_agent_id: author,
            target_agent_id: target,
            related_task_run_id: None,
            kind: MessageKind::Control,
            trigger_turn: false,
            payload: payload.into(),
            preview: None,
            created_at: Utc::now(),
            in_reply_to: None,
            timeout_at_ms: None,
        }
    }
}

fn make_preview(s: &str) -> String {
    const MAX_PREVIEW: usize = 200;
    if s.len() <= MAX_PREVIEW {
        s.to_string()
    } else {
        format!("{}...", &s[..MAX_PREVIEW])
    }
}

const DEFAULT_MAILBOX_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mailbox {
    pub agent_id: AgentId,
    messages: VecDeque<AgentMessage>,
    cursor: usize,
    read_cursor: usize,
    capacity: usize,
}

impl Mailbox {
    pub fn new(agent_id: AgentId) -> Self {
        Self::with_capacity(agent_id, DEFAULT_MAILBOX_CAPACITY)
    }
    pub fn with_capacity(agent_id: AgentId, capacity: usize) -> Self {
        Self {
            agent_id,
            messages: VecDeque::new(),
            cursor: 0,
            read_cursor: 0,
            capacity,
        }
    }

    pub fn deliver(&mut self, msg: AgentMessage) {
        debug_assert_eq!(msg.target_agent_id, self.agent_id, "message delivered to wrong mailbox");
        self.messages.push_back(msg);
    }

    pub fn read(&self, limit: usize) -> Vec<&AgentMessage> {
        self.messages.iter().skip(self.read_cursor).take(limit).collect()
    }

    pub fn confirm(&mut self, message_id: MessageId) -> bool {
        let mut found_idx = None;
        for (i, msg) in self.messages.iter().enumerate() {
            if i < self.cursor { continue; }
            if msg.message_id == message_id {
                found_idx = Some(i + 1);
                break;
            }
        }
        if let Some(idx) = found_idx {
            self.cursor = idx;
            self.read_cursor = self.read_cursor.max(idx);
            return true;
        }
        false
    }

    pub fn unread_count(&self) -> usize {
        self.messages.len().saturating_sub(self.read_cursor)
    }
    pub fn unconfirmed_count(&self) -> usize {
        self.messages.len().saturating_sub(self.cursor)
    }
    pub fn has_pending_followup(&self) -> bool {
        self.messages.iter().skip(self.read_cursor).any(|m| m.trigger_turn)
    }

    pub fn pop_next_followup(&mut self) -> Option<AgentMessage> {
        let idx = self.messages.iter().position(|m| m.trigger_turn)?;
        let msg = self.messages.remove(idx)?;
        if idx < self.cursor { self.cursor -= 1; }
        if idx < self.read_cursor { self.read_cursor -= 1; }
        Some(msg)
    }

    pub fn pop_front_n(&mut self, n: usize) -> Vec<AgentMessage> {
        let take = n.min(self.messages.len());
        let mut out = Vec::with_capacity(take);
        for _ in 0..take {
            if let Some(msg) = self.messages.pop_front() {
                if self.cursor > 0 { self.cursor -= 1; }
                if self.read_cursor > 0 { self.read_cursor -= 1; }
                out.push(msg);
            }
        }
        out
    }

    pub fn capacity(&self) -> usize { self.capacity }
    pub fn len(&self) -> usize { self.messages.len() }
    pub fn is_full(&self) -> bool { self.messages.len() >= self.capacity }

    pub fn total_delivered(&self) -> usize { self.messages.len() }

    pub fn gc(&mut self) -> usize {
        let before = self.messages.len();
        let to_drain = self.cursor.min(self.messages.len());
        for _ in 0..to_drain { self.messages.pop_front(); }
        self.cursor -= to_drain;
        self.read_cursor = self.read_cursor.saturating_sub(to_drain);
        before - self.messages.len()
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum MailboxError {
    #[error("agent {0:?} not found in mailbox router")]
    AgentNotFound(AgentId),
    #[error("mailbox for agent {0:?} is full; oldest messages were evicted to make room")]
    MailboxFull(AgentId),
    #[error("poll timed out waiting for messages for agent {0:?}")]
    Timeout(AgentId),
}

#[derive(Debug, Clone)]
struct PendingRequest {
    request_id: MessageId,
    requester: AgentId,
    expires_at_ms: u64,
}

#[derive(Debug, Default)]
pub struct MailboxRouter {
    mailboxes: HashMap<AgentId, Mailbox>,
    pending_requests: Vec<PendingRequest>,
}

impl MailboxRouter {
    pub fn new() -> Self { Self::default() }

    pub fn register(&mut self, agent_id: AgentId) {
        self.mailboxes.insert(agent_id, Mailbox::new(agent_id));
    }

    pub fn register_with_capacity(&mut self, agent_id: AgentId, capacity: usize) {
        self.mailboxes.insert(agent_id, Mailbox::with_capacity(agent_id, capacity));
    }

    pub fn unregister(&mut self, agent_id: &AgentId) {
        self.mailboxes.remove(agent_id);
    }

    pub fn registered_agent_ids(&self) -> Vec<AgentId> {
        self.mailboxes.keys().copied().collect()
    }

    pub fn dispatch(&mut self, msg: AgentMessage) -> Result<(), MailboxError> {
        let target = msg.target_agent_id;
        let is_request = msg.kind == MessageKind::Request;
        let timeout_ms = msg.timeout_at_ms;
        let msg_id = msg.message_id;
        let author = msg.author_agent_id;

        let mailbox = self.mailboxes.get_mut(&target)
            .ok_or(MailboxError::AgentNotFound(target))?;

        if mailbox.is_full() {
            let cap = mailbox.capacity();
            let drop_n = (cap / 2).max(1);
            let _dropped = mailbox.pop_front_n(drop_n);
        }

        if mailbox.is_full() {
            return Err(MailboxError::MailboxFull(target));
        }

        mailbox.deliver(msg);

        if is_request {
            if let Some(expire_ms) = timeout_ms {
                self.pending_requests.push(PendingRequest {
                    request_id: msg_id,
                    requester: author,
                    expires_at_ms: expire_ms,
                });
            }
        }

        Ok(())
    }

    pub fn poll_for_agent(
        &mut self,
        agent_id: &AgentId,
        max_messages: usize,
        _timeout: Duration,
    ) -> Result<Vec<AgentMessage>, MailboxError> {
        let mailbox = self.mailboxes.get_mut(agent_id)
            .ok_or(MailboxError::AgentNotFound(*agent_id))?;
        let take = max_messages.min(mailbox.len());
        Ok(mailbox.pop_front_n(take))
    }

    pub fn tick_expire_requests(&mut self, now_ms: u64) -> Vec<(MessageId, AgentId)> {
        let mut expired = Vec::new();
        let mut i = 0;
        while i < self.pending_requests.len() {
            if self.pending_requests[i].expires_at_ms <= now_ms {
                let pr = self.pending_requests.remove(i);
                expired.push((pr.request_id, pr.requester));
            } else {
                i += 1;
            }
        }
        expired
    }

    pub fn deliver(&mut self, msg: AgentMessage) -> Result<(), String> {
        match self.dispatch(msg) {
            Ok(()) => Ok(()),
            Err(MailboxError::AgentNotFound(a)) => Err(format!("no mailbox registered for agent {a}")),
            Err(MailboxError::MailboxFull(a)) => Err(format!("mailbox full for agent {a}")),
            Err(MailboxError::Timeout(a)) => Err(format!("timeout for agent {a}")),
        }
    }

    pub fn read(&self, agent_id: &AgentId, limit: usize) -> Result<Vec<&AgentMessage>, String> {
        match self.mailboxes.get(agent_id) {
            Some(mb) => Ok(mb.read(limit)),
            None => Err(format!("no mailbox for agent {agent_id}")),
        }
    }

    pub fn confirm(&mut self, agent_id: &AgentId, msg_id: MessageId) -> Result<bool, String> {
        match self.mailboxes.get_mut(agent_id) {
            Some(mb) => Ok(mb.confirm(msg_id)),
            None => Err(format!("no mailbox for agent {agent_id}")),
        }
    }

    pub fn has_pending_followup(&self, agent_id: &AgentId) -> bool {
        self.mailboxes.get(agent_id).map(|mb| mb.has_pending_followup()).unwrap_or(false)
    }

    pub fn pop_followup(&mut self, agent_id: &AgentId) -> Option<AgentMessage> {
        self.mailboxes.get_mut(agent_id).and_then(|mb| mb.pop_next_followup())
    }

    pub fn unread_count(&self, agent_id: &AgentId) -> usize {
        self.mailboxes.get(agent_id).map(|mb| mb.unread_count()).unwrap_or(0)
    }

    pub fn gc(&mut self, agent_id: &AgentId) -> usize {
        self.mailboxes.get_mut(agent_id).map(|mb| mb.gc()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aid() -> AgentId { AgentId::new() }

    #[test]
    fn message_queue_only_does_not_trigger_turn() {
        let author = aid();
        let target = aid();
        let msg = AgentMessage::message(author, target, "hello");
        assert!(!msg.trigger_turn);
        assert_eq!(msg.kind, MessageKind::Message);
    }

    #[test]
    fn followup_triggers_turn() {
        let author = aid();
        let target = aid();
        let msg = AgentMessage::followup(author, target, "please review", None);
        assert!(msg.trigger_turn);
        assert_eq!(msg.kind, MessageKind::Followup);
    }

    #[test]
    fn dispatch_delivers_message() {
        let mut r = MailboxRouter::new();
        let t = aid();
        r.register(t);
        let msg = AgentMessage::message(aid(), t, "hi");
        assert!(r.dispatch(msg).is_ok());
        let msgs = r.poll_for_agent(&t, 10, Duration::from_secs(0)).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].payload, "hi");
    }

    #[test]
    fn dispatch_rejects_unknown_agent() {
        let mut r = MailboxRouter::new();
        let msg = AgentMessage::message(aid(), aid(), "hi");
        let err = r.dispatch(msg).unwrap_err();
        assert!(matches!(err, MailboxError::AgentNotFound(_)));
    }

    #[test]
    fn registered_agent_ids_works() {
        let mut r = MailboxRouter::new();
        let a = aid();
        let b = aid();
        r.register(a);
        r.register(b);
        let ids = r.registered_agent_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&a));
        assert!(ids.contains(&b));
    }

    #[test]
    fn tick_expire_requests_returns_expired() {
        let mut r = MailboxRouter::new();
        let a = aid();
        r.register(a);
        let mut msg = AgentMessage::message(aid(), a, "req");
        msg.kind = MessageKind::Request;
        msg.timeout_at_ms = Some(100);
        r.dispatch(msg).unwrap();

        let expired = r.tick_expire_requests(99);
        assert_eq!(expired.len(), 0);

        let expired = r.tick_expire_requests(200);
        assert_eq!(expired.len(), 1);
    }
}
