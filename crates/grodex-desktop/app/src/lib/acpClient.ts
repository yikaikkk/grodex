// Real ACP client for the desktop UI.
//
// Listens to the Rust backend's forwarded `grodex serve` event stream
// (`acp_event` / `acp_snapshot` / `acp_log`) and re-publishes each event in
// the shape the existing React components expect (see `eventBus.ts`). All
// user actions are sent back as full ACP `Command` JSON via `invoke`.

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import {
  Session,
  SettingsState,
  SubAgentNode,
  TimelineItem,
  ToolItem,
  ApprovalRequest,
} from '../types';
import { eventBus } from './eventBus';

// ── Wire types (mirror grodex_protocol::acp) ───────────────────────────

interface AcpEnvelope {
  seq: number;
  event_id?: string;
  parent_event_id?: string;
  causation_token?: string;
  generation?: number;
  session_id: string;
  content: AcpContent;
}

interface AcpContent {
  type: string;
  text?: string;
  call_id?: string;
  name?: string;
  args_delta?: string;
  content?: string;
  is_error?: boolean;
  id?: string;
  label?: string;
  phase?: string;
  detail?: string;
  ok?: boolean | null;
  turn_id?: string;
  input_tokens?: number;
  cached_tokens?: number;
  message?: string;
  ticket_id?: string;
  tool_name?: string;
  summary?: string;
  risk?: string;
  arguments_snapshot?: Record<string, unknown>;
  timeout_remaining_ms?: number;
  item_id?: string;
  item_type?: string;
  snapshot?: AcpSnapshot;
  tool_call?: unknown;
}

interface AcpSnapshotItem {
  item_id: string;
  item_type: string;
  content: string;
  complete: boolean;
}

interface AcpSnapshot {
  session_id: string;
  last_seq: number;
  generation?: number;
  current_turn_id?: string | null;
  items: AcpSnapshotItem[];
}

interface SessionSummaryJson {
  id: string;
  workspace?: string | null;
  model?: string | null;
  provider?: string | null;
  title: string;
  preview: string;
  status: string;
  createdAt?: string | null;
  updatedAt?: string | null;
}

interface ConfigSummaryJson {
  provider?: string | null;
  model?: string | null;
  sandboxProfile?: string | null;
  wireProtocol: string;
  configPath?: string | null;
}

// ── Helpers ─────────────────────────────────────────────────────────────

const uid = (): string =>
  typeof crypto !== 'undefined' && crypto.randomUUID
    ? crypto.randomUUID()
    : `id-${Date.now()}-${Math.random().toString(36).slice(2)}`;

const clockTime = () => new Date().toLocaleTimeString();

const DEFAULT_PERMISSIONS: SettingsState['permissions'] = {
  read_file: 'allow',
  write_file: 'ask',
  edit_file: 'ask',
  exec: 'ask',
  glob: 'allow',
  grep: 'allow',
  apply_patch: 'ask',
  web_fetch: 'allow',
  delegate_task: 'allow',
};

// ── Per-"session" streaming state ───────────────────────────────────────
//
// The agent protocol is turn-ordered; we keep only lightweight buffers to
// assemble cumulative text / tool args, then emit the exact events the UI
// timeline reducer understands. The `clientSessionId` is the id the UI keys
// the timeline by. It starts as a locally-minted UUID for a respawned (new)
// `grodex serve` and is reconciled to the real rollout session id as soon
// as the first envelope of that process arrives (see `sessionReady`).

let clientSessionId = uid();
let wireSessionId = uid(); // syntactically valid UUID sent in commands
let inited = false;

let asst = { id: '', buf: '' };
let activeTurn = false;
// Reasoning arrives in bursts interleaved with tool calls. Each burst gets
// its own thinking item so the timeline shows text + tool calls in the real
// output order (instead of lumping all text together, then all tools).
let openThinkId = '';
let openThinkBuf = '';

interface ToolStub {
  id: string;
  toolName: string;
  status: 'running' | 'awaiting_approval' | 'finished' | 'failed';
  startTime: number;
  sourceAgent: string;
  params: Record<string, unknown>;
  approvalId?: string;
  diffId?: string;
}

const tools = new Map<string, ToolStub>();
const argsBuf = new Map<string, string>();
const toolNameByCall = new Map<string, string>();
const lastCallByTool = new Map<string, string>();
const nodes = new Map<string, SubAgentNode>();

function resetStreaming() {
  asst = { id: '', buf: '' };
  openThinkId = '';
  openThinkBuf = '';
  activeTurn = false;
}

function finalizeAssistant() {
  if (asst.id && asst.buf) {
    eventBus.emit('assistantTextDelta', {
      id: asst.id,
      content: asst.buf,
      isStreaming: false,
      timestamp: clockTime(),
    });
  }
}

/** End the current reasoning burst (collapses its thinking item). */
function endThoughtBurst() {
  if (openThinkId && openThinkBuf) {
    eventBus.emit('thinkingDelta', {
      id: openThinkId,
      content: openThinkBuf,
      isStreaming: false,
      durationSec: 0,
    });
  }
  openThinkId = '';
  openThinkBuf = '';
}

/** Append text to the current reasoning burst, opening a fresh thinking item
 * when needed so each burst renders as its own box at its arrival position. */
function appendThought(text: string) {
  if (!openThinkId) {
    openThinkId = `think_${uid()}`;
    openThinkBuf = '';
  }
  openThinkBuf += text;
  eventBus.emit('thinkingDelta', {
    id: openThinkId,
    content: openThinkBuf,
    isStreaming: true,
    durationSec: 0,
  });
}

function beginAssistant() {
  if (!asst.id) {
    asst.id = `asst_${uid()}`;
    asst.buf = '';
  }
}

function toolStubToItem(t: ToolStub): ToolItem {
  return {
    id: t.id,
    type: 'tool',
    toolName: t.toolName as ToolItem['toolName'],
    params: t.params,
    status: t.status as ToolItem['status'],
    startTime: t.startTime,
    elapsedSec: (Date.now() - t.startTime) / 1000,
    sourceAgent: t.sourceAgent,
    approvalId: t.approvalId,
    diffId: t.diffId,
    timestamp: clockTime(),
  };
}

function upsertTool(t: ToolStub, extra?: Partial<ToolItem>) {
  eventBus.emit('toolStarted', {
    ...toolStubToItem(t),
    ...extra,
  });
}

function summaryOf(toolName: string, args: Record<string, unknown>): string {
  const p = args as Record<string, unknown>;
  return (
    (typeof p.path === 'string' ? (p.path as string) : '') ||
    (typeof p.command === 'string' ? (p.command as string) : '') ||
    (typeof p.pattern === 'string' ? (p.pattern as string) : '') ||
    toolName
  );
}

// ── Snapshot → timeline items (resume replay) ───────────────────────────

function snapshotToTimeline(snap: AcpSnapshot): TimelineItem[] {
  const items: TimelineItem[] = [];
  const pendingTools = new Map<string, ToolItem>();
  // One USER message may span many snapshot steps (each ModelItemProduced is a
  // separate assistant/thinking entry). Coalesce them per user segment so the
  // UI shows ONE thinking block and ONE model-output bubble per turn.
  let segAsst: TimelineItem | null = null;
  let segThink: TimelineItem | null = null;
  for (const it of snap.items) {
    const id = it.item_id || `i_${items.length}`;
    switch (it.item_type) {
      case 'user':
        items.push({ id, type: 'user', content: it.content, timestamp: '' });
        segAsst = null;
        segThink = null;
        break;
      case 'thinking': {
        if (segThink && segThink.type === 'thinking') {
          segThink.content += it.content ? `\n${it.content}` : '';
        } else {
          const t: TimelineItem = {
            id,
            type: 'thinking',
            content: it.content,
            isStreaming: false,
            isCollapsed: true,
            durationSec: 0,
            timestamp: '',
          };
          items.push(t);
          segThink = t;
        }
        break;
      }
      case 'assistant': {
        if (segAsst && segAsst.type === 'assistant') {
          segAsst.content += it.content ? `\n\n${it.content}` : '';
        } else {
          const a: TimelineItem = {
            id,
            type: 'assistant',
            content: it.content,
            isStreaming: false,
            timestamp: '',
          };
          items.push(a);
          segAsst = a;
        }
        break;
      }
      case 'tool_call': {
        let name = '';
        let args: Record<string, unknown> = {};
        try {
          const parsed = JSON.parse(it.content);
          name = parsed.name || '';
          args = parsed.arguments || {};
        } catch {
          name = it.content;
        }
        // Snapshot tools are HISTORY — mark them finished so ToolCard never
        // starts a live elapsed counter just because the session was opened.
        // (Only genuinely live tools, streamed while a turn runs, tick.)
        const t: ToolItem = {
          id,
          type: 'tool',
          toolName: (name || 'tool') as ToolItem['toolName'],
          params: args,
          status: 'finished',
          startTime: 0,
          elapsedSec: 0,
          sourceAgent: 'main',
          timestamp: '',
        };
        pendingTools.set(id, t);
        items.push(t);
        break;
      }
      case 'tool_result': {
        let content = it.content;
        let isError = false;
        let callId = id;
        try {
          const parsed = JSON.parse(it.content);
          content = parsed.content ?? content;
          isError = !!parsed.is_error;
          callId = parsed.call_id ?? id;
        } catch {
          /* raw text */
        }
        // Pair with the most recent open tool of the same kind.
        const targetId = callId !== id && pendingTools.has(callId) ? callId : id;
        const found = items.find((x) => x.id === targetId && x.type === 'tool');
        if (found && found.type === 'tool') {
          found.status = isError ? 'failed' : 'finished';
          found.elapsedSec = 0;
          found.resultSummary = content;
        }
        pendingTools.delete(targetId);
        break;
      }
      default:
        break;
    }
  }
  return items;
}

// ── ACP event mapping ───────────────────────────────────────────────────

function onEvent(envelope: AcpEnvelope) {
  const realSid = envelope.session_id;
  if (wireSessionId !== realSid) {
    const from = clientSessionId;
    wireSessionId = realSid;
    clientSessionId = realSid;
    eventBus.emit('sessionReady', { from, to: realSid });
  }

  const c: AcpContent = envelope.content;
  switch (c.type) {
    case 'ItemStarted': {
      // A new turn (or a resumed sub-item) began. Do NOT collapse the
      // thinking here — step/sub-turn boundaries (e.g. after an approval)
      // happen mid-turn and the thought should stay open until it truly ends.
      activeTurn = true;
      eventBus.emit('sessionStateChanged', {
        status: 'running',
        isRunning: true,
        sessionId: clientSessionId,
      });
      break;
    }
    case 'TextDelta': {
      // Assistant narration: close the current reasoning burst (it becomes a
      // collapsible pill) so tool/text ordering stays chronological.
      endThoughtBurst();
      beginAssistant();
      asst.buf += c.text ?? '';
      eventBus.emit('assistantTextDelta', {
        id: asst.id,
        content: asst.buf,
        isStreaming: true,
        timestamp: clockTime(),
      });
      break;
    }
    case 'ThoughtDelta': {
      // New reasoning token — appends to the current burst, or starts a fresh
      // burst (new thinking item) if we last saw a tool/text event.
      appendThought(c.text ?? '');
      break;
    }
    case 'ToolCallStart': {
      // Tool call begins: close the current reasoning burst (collapses to a
      // pill). Any later reasoning opens a NEW burst, so text + tools render
      // interleaved in output order.
      endThoughtBurst();
      const callId = c.call_id!;
      const name = c.name || 'tool';
      toolNameByCall.set(callId, name);
      argsBuf.set(callId, '');
      const stub: ToolStub = {
        id: callId,
        toolName: name,
        status: 'running',
        startTime: Date.now(),
        sourceAgent: 'main',
        params: {},
      };
      tools.set(callId, stub);
      lastCallByTool.set(name, callId);
      upsertTool(stub);
      break;
    }
    case 'ToolCallArgs': {
      const callId = c.call_id!;
      const prev = argsBuf.get(callId) || '';
      argsBuf.set(callId, prev + (c.args_delta ?? ''));
      break;
    }
    case 'ToolCallEnd': {
      const callId = c.call_id!;
      const raw = argsBuf.get(callId) || '';
      const stub = tools.get(callId);
      if (stub) {
        if (raw.trim()) {
          try {
            stub.params = JSON.parse(raw);
          } catch {
            stub.params = { args: raw };
          }
        }
        upsertTool(stub); // refresh params on the card
      }
      break;
    }
    case 'RequestPermission': {
      const ticketId = c.ticket_id || uid();
      const toolName = c.tool_name || 'tool';
      // Do NOT guess-link the ticket to an existing running tool card.
      // Approval happens BEFORE the tool actually starts (ToolCallStart only
      // arrives after the decision), so matching by name here can pin the WRONG
      // (earlier) card into `awaiting_approval` forever. The modal alone carries
      // the request; the real tool card shows up once it runs after approval.
      const params = (c.arguments_snapshot as Record<string, unknown>) || {};
      const timeoutMs = Math.max(0, c.timeout_remaining_ms ?? 0);
      const remaining = Math.max(0, Math.ceil(timeoutMs / 1000));
      const request: ApprovalRequest = {
        id: ticketId,
        toolItemId: '',
        toolName: toolName as ApprovalRequest['toolName'],
        params,
        target: summaryOf(toolName, params),
        sourceAgent: 'main',
        reason: c.summary || c.risk || '需要操作员授权执行该工具',
        totalDurationSec: remaining,
        remainingSec: remaining,
        deadlineMs: Date.now() + timeoutMs,
        status: 'pending',
      };
      eventBus.emit('approvalRequested', request);
      break;
    }
    case 'ToolResult': {
      const callId = c.call_id || '';
      const stub = tools.get(callId);
      const name = toolNameByCall.get(callId) || (stub ? stub.toolName : 'tool');
      const content = c.content || '';
      const isError = !!c.is_error;
      if (stub) {
        stub.status = isError ? 'failed' : 'finished';
      }
      const base: Partial<ToolItem> = {
        id: callId,
        status: isError ? 'failed' : 'finished',
        elapsedSec: stub ? (Date.now() - stub.startTime) / 1000 : 0,
        error: isError ? content : undefined,
        params: stub?.params,
      };
      if (name === 'exec') {
        const lines = content
          .split(/\r?\n/)
          .filter((l) => l.trim() !== '' || content.indexOf('\n') === -1)
          .map((l) => ({ stream: 'stdout' as const, text: l }));
        base.execOutput =
          lines.length > 0 ? lines : [{ stream: 'stdout' as const, text: content }];
        base.resultSummary = isError ? content : `执行完成 · ${lines.length} 行输出`;
      } else {
        const max = 800;
        base.resultSummary =
          content.length > max ? `${content.slice(0, max)}…` : content;
      }
      eventBus.emit('toolFinished', base);
      tools.delete(callId);
      argsBuf.delete(callId);
      if (lastCallByTool.get(name) === callId) lastCallByTool.delete(name);
      toolNameByCall.delete(callId);
      break;
    }
    case 'SubagentProgress': {
      console.debug('[acp] SubagentProgress', c.id, c.phase, c.label);
      const subId = c.id!;
      const phase = c.phase || '';
      const detail = c.detail || '';
      const label = c.label || '子 Agent';
      let node = nodes.get(subId);
      if (phase === 'started' || !node) {
        node = {
          id: subId,
          name: label,
          parentId: '',
          role: label,
          status: 'running_tool',
          task: detail || label,
          tokensUsed: 0,
          durationSec: 0,
          logs: detail ? [detail] : [],
        };
        nodes.set(subId, node);
      } else {
        node.logs = [...(node.logs || []), detail].filter((l) => l.length > 0);
        if (phase === 'finished') {
          node.status = c.ok === false ? 'interrupted' : 'done';
        } else if (phase === 'step') {
          node.status = 'running_tool';
        }
        nodes.set(subId, { ...node });
      }
      eventBus.emit('subagentUpdate', node);
      break;
    }
    case 'TurnComplete': {
      finalizeAssistant();
      endThoughtBurst();
      activeTurn = false;
      eventBus.emit('sessionStateChanged', {
        status: 'completed',
        isRunning: false,
        sessionId: clientSessionId,
        inputTokens: c.input_tokens || 0,
        cachedTokens: c.cached_tokens || 0,
      });
      // Fresh ids/buffers for the next turn so content never merges into a
      // previous turn's bubble.
      resetStreaming();
      break;
    }
    case 'CompactionStatus': {
      eventBus.emit('compactionStatus', { phase: c.phase || 'finished' });
      break;
    }
    case 'Info': {
      eventBus.emit('systemNotice', { message: c.message || '', kind: 'info' });
      break;
    }
    case 'Error': {
      finalizeAssistant();
      endThoughtBurst();
      resetStreaming();
      eventBus.emit('systemNotice', { message: c.message || '', kind: 'error' });
      eventBus.emit('sessionStateChanged', {
        status: 'completed',
        isRunning: false,
        sessionId: clientSessionId,
      });
      break;
    }
    case 'IndeterminateToolCall': {
      eventBus.emit('indeterminateRequested', {
        call_id: c.call_id,
        tool_name: c.tool_name,
        message: c.message,
      });
      break;
    }
    case 'SessionSnapshot': {
      const snap = c.snapshot!;
      if (snap.session_id) {
        clientSessionId = snap.session_id;
        wireSessionId = snap.session_id;
      }
      resetStreaming();
      eventBus.emit('sessionSnapshot', {
        sessionId: snap.session_id || clientSessionId,
        items: snapshotToTimeline(snap),
      });
      break;
    }
    default:
      // SessionLifecycle / legacy ItemAborted / ItemReplacement etc. — no UI
      // effect needed yet.
      break;
  }
}

function onSnapshot(snap: AcpSnapshot) {
  if (snap.session_id) {
    clientSessionId = snap.session_id;
    wireSessionId = snap.session_id;
  }
  resetStreaming();
  eventBus.emit('sessionSnapshot', {
    sessionId: snap.session_id || clientSessionId,
    items: snapshotToTimeline(snap),
  });
}

// ── Transport bootstrap ─────────────────────────────────────────────────
//
// IMPORTANT: init() must install EXACTLY ONE set of tauri listeners even when
//   (a) React StrictMode double-mounts the App effect in dev, and
//   (b) Vite HMR hot-swaps this module (old `listen` callbacks are not torn
//       down automatically).
// Two races caused duplicate bubbles: two async init() calls attaching two
// `listen` sets that both write into the SAME streaming buffers → every event
// processed twice → text doubled. We keep a single global registry plus an
// in-flight lock: concurrent callers share one install; a fresh module
// instance (HMR) tears the previous set down first, then installs once.

const GLOBAL_KEY = '__grodex_acp_listeners__';

interface ListenerRegistry {
  inFlight?: Promise<void>;
  unlisteners?: (() => void)[];
}

export async function init(): Promise<void> {
  const g = globalThis as any;
  const reg = g[GLOBAL_KEY] as ListenerRegistry | undefined;

  // Another init is mid-install (e.g. StrictMode's second mount): wait for it
  // and reuse its single listener set.
  if (reg?.inFlight) {
    await reg.inFlight;
    return;
  }

  // Already installed by a previous module instance: this is an HMR reload,
  // so swap to the current module's handlers (still exactly one set).
  if (reg?.unlisteners) {
    for (const un of reg.unlisteners) {
      try {
        un();
      } catch {
        /* ignore */
      }
    }
    delete g[GLOBAL_KEY];
  }
  if (inited) return;

  const p = (async () => {
    const unlisteners: (() => void)[] = [];
    unlisteners.push(
      await listen<AcpEnvelope>('acp_event', (e) => onEvent(e.payload))
    );
    unlisteners.push(
      await listen<AcpSnapshot>('acp_snapshot', (e) => onSnapshot(e.payload))
    );
    unlisteners.push(
      await listen<string>('acp_log', (e) => {
        eventBus.emit('systemNotice', { message: e.payload, kind: 'log' });
      })
    );
    g[GLOBAL_KEY] = { unlisteners };
    inited = true;
  })();
  // Publish the in-flight promise synchronously so a concurrent init() call
  // cannot start its own second install.
  g[GLOBAL_KEY] = { inFlight: p };
  await p;
}

// ── Command builders (server-side ACP `Command` JSON) ───────────────────

function sendWire(cmd: Record<string, unknown>): Promise<void> {
  return invoke('send_command', { command: cmd }).then(() => undefined);
}

function withSession(fields: Record<string, unknown>): Record<string, unknown> {
  return { session_id: wireSessionId, ...fields };
}

// ── Public actions ──────────────────────────────────────────────────────

/** Send a user prompt and optimistically show the user bubble. */
export async function sendPrompt(text: string): Promise<void> {
  resetStreaming();
  eventBus.emit('userMessage', {
    id: `msg_${uid()}`,
    content: text,
    timestamp: clockTime(),
  });
  eventBus.emit('sessionStateChanged', {
    status: 'running',
    isRunning: true,
    sessionId: clientSessionId,
  });
  const cmd = {
    type: 'Prompt',
    command_id: uid(),
    session_id: wireSessionId,
    text,
  };
  await sendWire(cmd);
}

/** Stop a cancel/error: mark any tool still running/awaiting as failed so its
 * timer stops and the card never hangs on "running". */
function finalizeAllRunningTools(reason: string) {
  for (const [callId, stub] of Array.from(tools.entries())) {
    if (stub.status === 'running' || stub.status === 'awaiting_approval') {
      stub.status = 'failed';
      eventBus.emit('toolFinished', {
        id: callId,
        status: 'failed',
        error: reason,
        elapsedSec: (Date.now() - stub.startTime) / 1000,
        params: stub.params,
      });
      tools.delete(callId);
      toolNameByCall.delete(callId);
      argsBuf.delete(callId);
      const nm = stub.toolName;
      if (lastCallByTool.get(nm) === callId) lastCallByTool.delete(nm);
    }
  }
}

export async function stop(): Promise<void> {
  const cmd = withSession({ type: 'Cancel', command_id: uid() });
  await sendWire(cmd);
  finalizeAllRunningTools('已停止');
  finalizeAssistant();
  endThoughtBurst();
  resetStreaming();
  eventBus.emit('sessionStateChanged', {
    status: 'completed',
    isRunning: false,
    sessionId: clientSessionId,
  });
}

/** Remove phantom (empty / no-conversation) session dirs. */
export async function purgeEmptySessions(): Promise<number> {
  return invoke<number>('purge_empty_sessions');
}

/** Ensure a `grodex serve` process exists (spawn in `cwd` only if none). */
export async function ensureAgent(cwd: string): Promise<boolean> {
  return invoke<boolean>('ensure_agent', { cwd });
}

/** Open/resume a historical session from the rollout root. */
export async function resumeSession(sessionId: string): Promise<void> {
  if (sessionId === clientSessionId) return;
  clientSessionId = sessionId;
  wireSessionId = sessionId;
  resetStreaming();
  const cmd = {
    type: 'ResumeSession',
    command_id: uid(),
    session_id: sessionId,
    resume_from: { last_consumed_seq: 0, mode: 'snapshot_then_live' },
  };
  await sendWire(cmd);
}

/** Start a brand-new session in `cwd` (respawns `grodex serve`). */
export async function newSession(cwd: string): Promise<void> {
  clientSessionId = uid();
  wireSessionId = uid();
  resetStreaming();
  tools.clear();
  argsBuf.clear();
  toolNameByCall.clear();
  lastCallByTool.clear();
  nodes.clear();
  await invoke<string>('new_session', { cwd });
}

export async function resolveApproval(
  approvalId: string,
  action: 'allowed_once' | 'always_allowed' | 'denied' | 'narrowed',
  narrowedParams?: Record<string, unknown>,
): Promise<void> {
  let resolution: unknown;
  switch (action) {
    case 'allowed_once':
      resolution = 'allow';
      break;
    case 'always_allowed':
      resolution = 'always_allow';
      break;
    case 'denied':
      resolution = 'deny';
      break;
    case 'narrowed':
      resolution = { narrow: { narrowed_args: narrowedParams ?? {} } };
      break;
  }
  // Deny: proactively finish the tool card tied to this ticket so its timer
  // stops immediately — some paths never emit a ToolResult for a denied call,
  // which previously left the card "running" forever.
  if (action === 'denied') {
    for (const [callId, stub] of Array.from(tools.entries())) {
      if (stub.approvalId !== approvalId) continue;
      stub.status = 'failed';
      eventBus.emit('toolFinished', {
        id: callId,
        status: 'failed',
        error: '已拒绝执行该工具',
        elapsedSec: (Date.now() - stub.startTime) / 1000,
        params: stub.params,
      });
      tools.delete(callId);
      toolNameByCall.delete(callId);
      argsBuf.delete(callId);
      const nm = stub.toolName;
      if (lastCallByTool.get(nm) === callId) lastCallByTool.delete(nm);
    }
  }

  const cmd = {
    type: 'ResolveApproval',
    command_id: uid(),
    ticket_id: approvalId,
    resolution,
    issued_by: 'grodex-desktop',
    issued_at_ms: Date.now(),
  };
  await sendWire(cmd);
  eventBus.emit('approvalResolved', { approvalId });
}

export async function resolveIndeterminate(
  callId: string,
  resolution: 'succeeded' | 'failed' | 'retry',
): Promise<void> {
  const cmd = {
    type: 'ResolveIndeterminate',
    command_id: uid(),
    call_id: callId,
    resolution,
  };
  await sendWire(cmd);
}

// ── Backend queries ─────────────────────────────────────────────────────

function toSession(s: SessionSummaryJson): Session {
  return {
    id: s.id,
    title: s.title,
    preview: s.preview || s.title,
    workspace: s.workspace || '',
    createdAt: s.createdAt || '',
    updatedAt: s.updatedAt || '',
    status: 'completed',
    tokensUsed: 0,
    costEstimate: 0,
    activeSubagents: 0,
    treeDepth: 1,
    model: s.model || undefined,
    provider: s.provider || undefined,
  };
}

export async function listSessions(): Promise<Session[]> {
  const raw = await invoke<SessionSummaryJson[]>('list_sessions');
  return (raw || []).map(toSession);
}

/** Permanently delete a session's rollout directory. */
export async function deleteSession(sessionId: string): Promise<void> {
  await invoke('delete_session', { sessionId });
}

// ── Memory management ────────────────────────────────────────────────────

export interface MemoryRow {
  id: string;
  status: string;
  kind: string;
  scope: string;
  content: string;
  createdAt: string;
  updatedAt: string;
}

export interface ConflictRow {
  conflictId: string;
  leftMemoryId: string;
  rightMemoryId: string;
  relation: string;
  status: string;
  reason: string;
}

export interface MemoryOverview {
  units: MemoryRow[];
  conflicts: ConflictRow[];
}

export interface MaintenanceReport {
  units: number;
  conflictsPending: number;
  governanceOk: boolean;
  consolidationOk: boolean;
}

export async function listMemories(): Promise<MemoryOverview> {
  return invoke<MemoryOverview>('list_memories');
}

/** Soft-delete a memory unit (status → orphaned; excluded from retrieval). */
export async function deleteMemory(id: string): Promise<void> {
  await invoke('delete_memory', { id });
}

export async function runMemoryMaintenance(): Promise<MaintenanceReport> {
  return invoke<MaintenanceReport>('run_memory_maintenance');
}

export async function loadConfig(): Promise<SettingsState> {
  const cfg = await invoke<ConfigSummaryJson>('get_config');
  const provider = (cfg.provider || 'deepseek') as SettingsState['provider'];
  return {
    provider,
    model: cfg.model || 'deepseek-v4-flash',
    wireProtocol: 'acp_stdio',
    sandboxProfile: (cfg.sandboxProfile as SettingsState['sandboxProfile']) || 'workspace',
    permissions: { ...DEFAULT_PERMISSIONS },
  };
}

// Shared default for a brand-new workspace before config is read.
export function defaultPermissions(): SettingsState['permissions'] {
  return { ...DEFAULT_PERMISSIONS };
}

export function currentSessionId(): string {
  return clientSessionId;
}
