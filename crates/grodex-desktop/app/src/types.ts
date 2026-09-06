export type SessionStatus = 'running' | 'completed' | 'awaiting_approval' | 'crashed_recoverable';

export type ToolStatus = 'pending' | 'running' | 'finished' | 'failed' | 'awaiting_approval';

export type ToolName = 
  | 'read_file' 
  | 'write_file' 
  | 'edit_file' 
  | 'exec' 
  | 'glob' 
  | 'grep' 
  | 'apply_patch' 
  | 'web_fetch' 
  | 'delegate_task';

export type AgentRole = 'main' | 'sub-agent';

export interface Session {
  id: string;
  title: string;
  preview: string;
  workspace: string;
  createdAt: string;
  updatedAt: string;
  status: SessionStatus;
  tokensUsed: number;
  costEstimate: number;
  activeSubagents: number;
  treeDepth: number;
  crashReason?: string;
  resumeBreakpointIndex?: number;
  model?: string;
  provider?: string;
}

export interface UserMessageItem {
  id: string;
  type: 'user';
  content: string;
  timestamp: string;
}

export interface AssistantMessageItem {
  id: string;
  type: 'assistant';
  content: string;
  isStreaming: boolean;
  tokens?: number;
  durationSec?: number;
  timestamp: string;
}

export interface ThinkingItem {
  id: string;
  type: 'thinking';
  content: string;
  isStreaming: boolean;
  isCollapsed: boolean;
  durationSec: number;
  timestamp: string;
}

export interface ExecOutputLine {
  stream: 'stdout' | 'stderr' | 'system';
  text: string;
}

export interface ToolItem {
  id: string;
  type: 'tool';
  toolName: ToolName;
  params: Record<string, any>;
  status: ToolStatus;
  startTime: number;
  elapsedSec: number;
  sourceAgent: string; // e.g. "main" or "agent#2 (delegated: test-runner)"
  resultSummary?: string;
  execOutput?: ExecOutputLine[];
  exitCode?: number;
  error?: string;
  approvalId?: string;
  diffId?: string;
  timestamp: string;
  isExpanded?: boolean;
}

export type TimelineItem = UserMessageItem | AssistantMessageItem | ThinkingItem | ToolItem;

export interface ApprovalRequest {
  id: string;
  toolItemId: string;
  toolName: ToolName;
  params: Record<string, any>;
  target: string;
  sourceAgent: string;
  reason: string;
  totalDurationSec: number;
  remainingSec: number;
  status: 'pending' | 'allowed_once' | 'always_allowed' | 'denied' | 'narrowed' | 'expired';
  narrowedParams?: Record<string, any>;
  /** Wall-clock ms when the server's approval window expires (computed from
   * `timeout_remaining_ms` at emission). Lets a modal shown late from the
   * queue still reflect the true remaining time. */
  deadlineMs?: number;
}

export interface DiffHunk {
  oldStart: number;
  oldLines: number;
  newStart: number;
  newLines: number;
  content: {
    type: 'add' | 'delete' | 'context';
    text: string;
    oldLineNo?: number;
    newLineNo?: number;
  }[];
}

export interface DiffFile {
  id: string;
  filename: string;
  path: string;
  additions: number;
  deletions: number;
  hunks: DiffHunk[];
  baseSnapshot: string;
  toolCallOrigin: string;
}

export interface SubAgentNode {
  id: string;
  name: string;
  parentId: string;
  role: string;
  status: 'thinking' | 'running_tool' | 'waiting' | 'done' | 'interrupted';
  task: string;
  tokensUsed: number;
  durationSec: number;
  logs: string[];
}

export type PermissionRule = 'allow' | 'ask' | 'deny';

export interface SettingsState {
  provider: 'anthropic' | 'openai' | 'deepseek' | 'ollama';
  model: string;
  wireProtocol: 'acp_stdio' | 'acp_websocket' | 'sse_direct';
  sandboxProfile: 'workspace' | 'readonly' | 'restricted' | 'full';
  permissions: Record<ToolName, PermissionRule>;
}

export interface SteerSuggestion {
  id: string;
  message: string;
  actionText: string;
}
