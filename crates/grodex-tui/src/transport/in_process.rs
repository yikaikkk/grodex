use tokio::sync::mpsc;

use grodex_protocol::acp::{Command, EventEnvelope};

pub struct InProcessAgentHalf {
    pub from_tui_rx: mpsc::UnboundedReceiver<Command>,
    pub to_tui_tx: mpsc::UnboundedSender<EventEnvelope>,
}

pub struct InProcessBridge {
    pub to_agent_tx: mpsc::UnboundedSender<Command>,
    pub from_agent_rx: mpsc::UnboundedReceiver<EventEnvelope>,
    pub agent_half: InProcessAgentHalf,
}

impl InProcessBridge {
    pub fn new(buffer: usize) -> Self {
        let (tui_to_agent_tx, tui_to_agent_rx) = mpsc::unbounded_channel();
        let (agent_to_tui_tx, agent_to_tui_rx) = mpsc::unbounded_channel();
        let _ = buffer;

        Self {
            to_agent_tx: tui_to_agent_tx,
            from_agent_rx: agent_to_tui_rx,
            agent_half: InProcessAgentHalf {
                from_tui_rx: tui_to_agent_rx,
                to_tui_tx: agent_to_tui_tx,
            },
        }
    }
}
