// Typed in-app event hub. The desktop UI's components subscribe here; the
// ACP transport (see `acpClient.ts`) publishes real `grodex serve` events
// into it under these names. Nothing here generates data anymore — mock
// demo sessions were removed.

export type ACPEventType =
  | 'sessionStateChanged'
  | 'userMessage'
  | 'thinkingDelta'
  | 'assistantTextDelta'
  | 'toolStarted'
  | 'toolProgress'
  | 'approvalRequested'
  | 'approvalResolved'
  | 'toolFinished'
  | 'subagentUpdate'
  // Desktop-specific (not in the old demo bus):
  | 'sessionSnapshot' // resume: rebuild timeline for a session
  | 'systemNotice' // one-line info/log entry (from ACP Info)
  | 'indeterminateRequested' // crash-recovery decision modal
  | 'compactionStatus' // context-compaction indicator
  | 'sessionReady'; // fresh respawn reconciled to a real rollout session id

export type ACPEventHandler = (payload: any) => void;

class ACPEventBus {
  private listeners: Map<ACPEventType, Set<ACPEventHandler>> = new Map();
  // Duplicate-fire guard: if the exact same (event, payload) is emitted twice
  // within a short window — the classic symptom of a stale HMR listener set —
  // drop the second copy. Real streaming updates always differ (cumulative
  // text grows), so legitimate events are never caught by this.
  private lastDup: { event: ACPEventType; json: string; at: number } | null = null;

  public on(event: ACPEventType, handler: ACPEventHandler) {
    if (!this.listeners.has(event)) {
      this.listeners.set(event, new Set());
    }
    this.listeners.get(event)!.add(handler);
    return () => this.off(event, handler);
  }

  public off(event: ACPEventType, handler: ACPEventHandler) {
    this.listeners.get(event)?.delete(handler);
  }

  public emit(event: ACPEventType, payload: any) {
    const now = Date.now();
    const json = JSON.stringify(payload ?? null);
    if (
      this.lastDup &&
      this.lastDup.event === event &&
      this.lastDup.json === json &&
      now - this.lastDup.at < 150
    ) {
      return; // duplicate fire from a stale listener set — ignore
    }
    this.lastDup = { event, json, at: now };

    const handlers = this.listeners.get(event);
    if (handlers) {
      handlers.forEach((handler) => {
        try {
          handler(payload);
        } catch (err) {
          console.error(`Error in event handler for ${event}:`, err);
        }
      });
    }
  }
}

export const eventBus = new ACPEventBus();
