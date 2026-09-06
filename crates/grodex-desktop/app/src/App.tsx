import React, { useState, useEffect, useRef } from 'react';
import {
  ApprovalRequest,
  Session,
  SettingsState,
  SubAgentNode,
  TimelineItem,
  ToolItem,
} from './types';
import * as acp from './lib/acpClient';
import { eventBus } from './lib/eventBus';
import { Sidebar } from './components/Sidebar';
import { Header } from './components/Header';
import { Timeline } from './components/Timeline';
import { Composer } from './components/Composer';
import { AgentTreePanel } from './components/AgentTreePanel';
import { ApprovalModal } from './components/ApprovalModal';
import { SettingsModal } from './components/SettingsModal';
import { EmptyState } from './components/EmptyState';
import { MemoryManager } from './components/MemoryManager';
import { AlertTriangle, RotateCcw, XCircle, Check, Loader2, Trash2 } from 'lucide-react';

interface IndeterminateReq {
  call_id: string;
  tool_name?: string;
  message?: string;
}

function makeDefaultSettings(): SettingsState {
  return {
    provider: 'deepseek',
    model: 'deepseek-v4-flash',
    wireProtocol: 'acp_stdio',
    sandboxProfile: 'workspace',
    permissions: acp.defaultPermissions(),
  };
}

export default function App() {
  const [sessions, setSessions] = useState<Session[]>([]);
  const [activeSessionId, setActiveSessionId] = useState<string>('');
  const [workspace, setWorkspace] = useState<string>('');
  const [timelines, setTimelines] = useState<Record<string, TimelineItem[]>>({});
  const [subagents, setSubagents] = useState<SubAgentNode[]>([]);
  const [isRunning, setIsRunning] = useState<boolean>(false);
  // Multiple approvals can be outstanding at once (e.g. a parallel tool
  // batch). Keep a FIFO queue and surface one modal at a time, answering
  // each ticket as it is shown — otherwise later requests would overwrite
  // the modal and earlier tickets would silently time out.
  const [pendingApprovals, setPendingApprovals] = useState<ApprovalRequest[]>([]);
  const [isCompacting, setIsCompacting] = useState(false);
  const [indeterminate, setIndeterminate] = useState<IndeterminateReq | null>(null);
  const [notice, setNotice] = useState<{ message: string; kind: string } | null>(null);
  const [isAgentTreeOpen, setIsAgentTreeOpen] = useState<boolean>(true);
  const [isSettingsOpen, setIsSettingsOpen] = useState<boolean>(false);
  const [isMemoryOpen, setIsMemoryOpen] = useState<boolean>(false);
  const [settings, setSettings] = useState<SettingsState>(makeDefaultSettings);

  // App-owned dialogs (window.confirm / window.prompt are unavailable in the
  // Tauri webview, so confirmation + directory input must be in-app UI).
  const [confirmDelete, setConfirmDelete] = useState<{
    sessionId: string;
    title: string;
  } | null>(null);
  const [isDirDialogOpen, setIsDirDialogOpen] = useState(false);
  const [dirInput, setDirInput] = useState('');
  const dirResolveRef = useRef<(path: string) => void>(() => {});

  const activeSession =
    sessions.find((s) => s.id === activeSessionId) || sessions[0];
  const currentTimeline = timelines[activeSessionId] || [];

  const showNotice = (message: string, kind = 'info') => {
    setNotice({ message, kind });
  };

  // Auto-dismiss notices.
  useEffect(() => {
    if (!notice) return;
    const t = setTimeout(() => setNotice(null), 6000);
    return () => clearTimeout(t);
  }, [notice]);

  // ── Event bus subscriptions ─────────────────────────────────────────
  useEffect(() => {
    const unsubSessionState = eventBus.on('sessionStateChanged', (data: any) => {
      setIsRunning(data.isRunning ?? false);
      if (data.sessionId && data.status) {
        setSessions((prev) =>
          prev.map((s) =>
            s.id === (data.sessionId || activeSessionId)
              ? {
                  ...s,
                  status: data.status,
                  tokensUsed: (s.tokensUsed || 0) + (data.inputTokens || 0),
                }
              : s
          )
        );
      }
    });

    const unsubUserMsg = eventBus.on('userMessage', (data: any) => {
      setTimelines((prev) => ({
        ...prev,
        [activeSessionId]: [
          ...(prev[activeSessionId] || []),
          {
            id: data.id,
            type: 'user',
            content: data.content,
            timestamp: data.timestamp,
          },
        ],
      }));
    });

    const unsubThinking = eventBus.on('thinkingDelta', (data: any) => {
      setTimelines((prev) => {
        const list = prev[activeSessionId] || [];
        const existingIdx = list.findIndex((it) => it.id === data.id);
        if (existingIdx >= 0) {
          const updated = [...list];
          updated[existingIdx] = {
            ...updated[existingIdx],
            content: data.content,
            isStreaming: data.isStreaming,
            durationSec: data.durationSec,
          } as any;
          return { ...prev, [activeSessionId]: updated };
        }
        return {
          ...prev,
          [activeSessionId]: [
            ...list,
            {
              id: data.id,
              type: 'thinking',
              content: data.content,
              isStreaming: data.isStreaming,
              isCollapsed: false,
              durationSec: data.durationSec,
              timestamp: new Date().toLocaleTimeString(),
            },
          ],
        };
      });
    });

    const unsubAssistantText = eventBus.on('assistantTextDelta', (data: any) => {
      setTimelines((prev) => {
        const list = prev[activeSessionId] || [];
        const existingIdx = list.findIndex((it) => it.id === data.id);
        if (existingIdx >= 0) {
          const updated = [...list];
          updated[existingIdx] = {
            ...updated[existingIdx],
            content: data.content,
            isStreaming: data.isStreaming,
            tokens: data.tokens,
            durationSec: data.durationSec,
          } as any;
          return { ...prev, [activeSessionId]: updated };
        }
        return {
          ...prev,
          [activeSessionId]: [
            ...list,
            {
              id: data.id,
              type: 'assistant',
              content: data.content,
              isStreaming: data.isStreaming,
              tokens: data.tokens,
              durationSec: data.durationSec,
              timestamp: data.timestamp || new Date().toLocaleTimeString(),
            },
          ],
        };
      });
    });

    const unsubToolStarted = eventBus.on('toolStarted', (toolItem: ToolItem) => {
      setTimelines((prev) => {
        const list = prev[activeSessionId] || [];
        const existingIdx = list.findIndex((it) => it.id === toolItem.id && it.type === 'tool');
        if (existingIdx >= 0) {
          const updated = [...list];
          updated[existingIdx] = { ...(updated[existingIdx] as any), ...toolItem };
          return { ...prev, [activeSessionId]: updated };
        }
        return {
          ...prev,
          [activeSessionId]: [...list, toolItem],
        };
      });
    });

    const unsubToolProgress = eventBus.on('toolProgress', (data: any) => {
      setTimelines((prev) => {
        const list = prev[activeSessionId] || [];
        const updated = list.map((it) =>
          it.id === data.id && it.type === 'tool'
            ? { ...it, status: data.status || it.status, execOutput: data.execOutput || it.execOutput }
            : it
        );
        return { ...prev, [activeSessionId]: updated };
      });
    });

    const unsubToolFinished = eventBus.on('toolFinished', (data: any) => {
      setTimelines((prev) => {
        const list = prev[activeSessionId] || [];
        const updated = list.map((it) =>
          it.id === data.id && it.type === 'tool'
            ? {
                ...it,
                status: data.status || 'finished',
                elapsedSec: data.elapsedSec ?? it.elapsedSec,
                resultSummary: data.resultSummary || it.resultSummary,
                execOutput: data.execOutput || it.execOutput,
                exitCode: data.exitCode !== undefined ? data.exitCode : it.exitCode,
                error: data.error,
                params: data.params || it.params,
              }
            : it
        );
        return { ...prev, [activeSessionId]: updated };
      });
    });

    const unsubApprovalRequested = eventBus.on('approvalRequested', (req: ApprovalRequest) => {
      setPendingApprovals((prev) =>
        prev.some((a) => a.id === req.id) ? prev : [...prev, req]
      );
    });

    const unsubApprovalResolved = eventBus.on('approvalResolved', (payload: any) => {
      const id = payload?.approvalId;
      setPendingApprovals((prev) =>
        id ? prev.filter((a) => a.id !== id) : prev.slice(1)
      );
    });

    const unsubSubagent = eventBus.on('subagentUpdate', (node: SubAgentNode) => {
      console.debug('[app] subagentUpdate', node.id, node.status, node.logs?.length);
      setSubagents((prev) => {
        const existingIdx = prev.findIndex((s) => s.id === node.id);
        if (existingIdx >= 0) {
          const copy = [...prev];
          copy[existingIdx] = { ...copy[existingIdx], ...node };
          return copy;
        }
        return [...prev, node];
      });
    });

    // ── Desktop-specific handlers ────────────────────────────────────
    const unsubSnapshot = eventBus.on('sessionSnapshot', (payload: any) => {
      const sid = payload.sessionId;
      if (!sid) return;
      setTimelines((prev) => ({ ...prev, [sid]: payload.items as TimelineItem[] }));
      // Make sure a row exists for a resumed session that wasn't listed yet.
      setSessions((prev) => {
        if (prev.some((s) => s.id === sid)) return prev;
        return [
          {
            id: sid,
            title: `会话 ${sid.slice(0, 8)}`,
            preview: '',
            workspace,
            createdAt: '',
            updatedAt: '刚刚',
            status: 'completed',
            tokensUsed: 0,
            costEstimate: 0,
            activeSubagents: 0,
            treeDepth: 1,
          },
          ...prev,
        ];
      });
    });

    const unsubReady = eventBus.on('sessionReady', (payload: any) => {
      const from = payload.from;
      const to = payload.to;
      if (!from || !to || from === to) return;
      // Reconcile the temp id of a freshly respawned session to its real id.
      setSessions((prev) => prev.map((s) => (s.id === from ? { ...s, id: to } : s)));
      setTimelines((prev) => {
        if (!prev[from]) return prev;
        const copy = { ...prev, [to]: prev[from] };
        delete copy[from];
        return copy;
      });
      setActiveSessionId((cur) => (cur === from ? to : cur));
    });

    const unsubNotice = eventBus.on('systemNotice', (payload: any) => {
      if (payload && payload.message) {
        // acp_log noise is surfaced as a subtle notice too; cap length.
        const msg = String(payload.message).slice(0, 400);
        showNotice(msg, payload.kind || 'info');
      }
    });

    const unsubCompact = eventBus.on('compactionStatus', (payload: any) => {
      setIsCompacting(payload?.phase === 'started');
    });

    const unsubIndeterminate = eventBus.on('indeterminateRequested', (payload: any) => {
      setIndeterminate(payload as IndeterminateReq);
    });

    return () => {
      unsubSessionState();
      unsubUserMsg();
      unsubThinking();
      unsubAssistantText();
      unsubToolStarted();
      unsubToolProgress();
      unsubToolFinished();
      unsubApprovalRequested();
      unsubApprovalResolved();
      unsubSubagent();
      unsubSnapshot();
      unsubReady();
      unsubNotice();
      unsubCompact();
      unsubIndeterminate();
    };
  }, [activeSessionId, workspace]);

  // ── Startup: init transport + load real session list ──────────────
  useEffect(() => {
    let cancelled = false;
    (async () => {
      await acp.init();
      const cfg = await acp.loadConfig();
      if (!cancelled) setSettings(cfg);
      // Remove phantom (empty boot) sessions so merely opening the app never
      // leaves a "session created with no conversation".
      await acp.purgeEmptySessions();
      const list = await acp.listSessions();
      if (cancelled) return;
      setSessions(list);
      const first = list[0];
      if (first) {
        // Just select the most recent session — do NOT spawn `grodex serve`
        // yet. A process (and its boot session dir) is only created lazily on
        // the first real send / when the user explicitly opens a session.
        const ws = first.workspace || '';
        setWorkspace(ws);
        setActiveSessionId(first.id);
      } else {
        showNotice('没有找到历史会话。点击「新建任务」选择一个工作目录开始。');
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // ── Actions ────────────────────────────────────────────────────────
  /** Resolve a workspace directory, prompting via an in-app dialog when the
   * current workspace is empty (window.prompt is unavailable in Tauri). */
  const ensureWorkspacePath = (): Promise<string> => {
    if (workspace) return Promise.resolve(workspace);
    setDirInput('');
    setIsDirDialogOpen(true);
    return new Promise((resolve) => {
      dirResolveRef.current = resolve;
    });
  };

  const submitDirDialog = () => {
    const path = dirInput.trim();
    setIsDirDialogOpen(false);
    if (path) setWorkspace(path);
    dirResolveRef.current(path);
    dirResolveRef.current = () => {};
  };

  /** "新建任务" only switches to an empty page — it does NOT spawn a process
   * or create a session. A real session is only created on the first message
   * (see prepareSessionForTurn), so an idle new page leaves no session. */
  const handleNewSession = async (): Promise<void> => {
    if (isRunning) await acp.stop();
    setPendingApprovals([]);
    setIndeterminate(null);
    setSubagents([]);
    setActiveSessionId('');
    setNotice(null);
  };

  /** First real message on the empty page: choose a workspace, spawn the agent
   * and register the session row, so nothing is created until the user sends. */
  const prepareSessionForTurn = async (): Promise<boolean> => {
    const cwd = await ensureWorkspacePath();
    if (!cwd) return false;
    setWorkspace(cwd);
    try {
      await acp.newSession(cwd);
    } catch (e: any) {
      showNotice(`无法启动会话：${e?.message || e}`, 'error');
      return false;
    }
    const id = acp.currentSessionId();
    setActiveSessionId(id);
    setTimelines((prev) => ({ ...prev, [id]: [] }));
    setSessions((prev) => [
      {
        id,
        title: '新建任务',
        preview: '等待输入开发指令…',
        workspace: cwd,
        createdAt: '刚刚',
        updatedAt: '刚刚',
        status: 'running',
        tokensUsed: 0,
        costEstimate: 0,
        activeSubagents: 0,
        treeDepth: 1,
      },
      ...prev,
    ]);
    return true;
  };

  const handleSelectSession = async (sessionId: string) => {
    if (sessionId === activeSessionId) return;
    if (isRunning) await acp.stop();
    setPendingApprovals([]);
    setIndeterminate(null);
    setSubagents([]);
    const target = sessions.find((s) => s.id === sessionId);
    const cwd = target?.workspace || workspace;
    if (cwd) setWorkspace(cwd);
    setActiveSessionId(sessionId);
    // ResumeSession needs a live agent process: spawn one for the session's
    // workspace if none is running yet (no-op if a process already exists).
    if (cwd) {
      try {
        await acp.ensureAgent(cwd);
      } catch (e: any) {
        showNotice(`启动 agent 失败：${e?.message || e}`, 'error');
        return;
      }
    }
    try {
      await acp.resumeSession(sessionId);
    } catch (e: any) {
      showNotice(`打开会话失败：${e?.message || e}`, 'error');
    }
  };

  const handleSendPrompt = async (text: string) => {
    // No session yet → create one on the first real message only.
    if (!activeSessionId) {
      const ok = await prepareSessionForTurn();
      if (!ok) return;
    }
    const boundToActive = acp.currentSessionId() === activeSessionId;
    if (!boundToActive) {
      // A historical/just-selected session: make sure a live process exists,
      // then bind it to this session (resume/rebind) BEFORE sending — so a
      // message continues THIS conversation, never creates a fresh one.
      if (isRunning) await acp.stop();
      const target = sessions.find((s) => s.id === activeSessionId);
      const cwd = target?.workspace || workspace;
      if (cwd) {
        try {
          await acp.ensureAgent(cwd);
        } catch (e: any) {
          showNotice(`启动 agent 失败：${e?.message || e}`, 'error');
          return;
        }
      }
      try {
        await acp.resumeSession(activeSessionId);
      } catch (e: any) {
        showNotice(`打开会话失败：${e?.message || e}`, 'error');
        return;
      }
    }
    try {
      await acp.sendPrompt(text);
    } catch (e: any) {
      showNotice(`发送失败：${e?.message || e}`, 'error');
      setIsRunning(false);
    }
  };

  const handleRefreshSessions = async () => {
    try {
      const list = await acp.listSessions();
      setSessions(list);
      showNotice(`任务列表已刷新（${list.length} 条）`);
    } catch (e: any) {
      showNotice(`刷新失败：${e?.message || e}`, 'error');
    }
  };

  const handleStop = async () => {
    try {
      await acp.stop();
    } catch {
      /* ignore */
    }
    setIsRunning(false);
  };

  const handleResolveApproval = async (
    approvalId: string,
    action: 'allowed_once' | 'always_allowed' | 'denied' | 'narrowed',
    narrowedParams?: any,
  ) => {
    try {
      await acp.resolveApproval(approvalId, action, narrowedParams);
    } catch (e: any) {
      showNotice(`审批回复失败：${e?.message || e}`, 'error');
    }
  };

  /** Show the in-app delete confirmation (window.confirm is unavailable). */
  const handleDeleteSession = (sessionId: string) => {
    if (sessionId === activeSessionId) {
      showNotice('当前打开的会话不能删除，请先切换到其他会话', 'error');
      return;
    }
    const target = sessions.find((s) => s.id === sessionId);
    setConfirmDelete({ sessionId, title: target?.title || sessionId });
  };

  const performDeleteSession = async (sessionId: string) => {
    try {
      await acp.deleteSession(sessionId);
    } catch (e: any) {
      showNotice(`删除失败：${e?.message || e}`, 'error');
      return;
    }
    setSessions((prev) => prev.filter((s) => s.id !== sessionId));
    setTimelines((prev) => {
      const copy = { ...prev };
      delete copy[sessionId];
      return copy;
    });
    setConfirmDelete(null);
    showNotice('会话已删除');
  };

  const handleResolveIndeterminate = async (
    callId: string,
    resolution: 'succeeded' | 'failed' | 'retry',
  ) => {
    try {
      await acp.resolveIndeterminate(callId, resolution);
    } catch (e: any) {
      showNotice(`裁决失败：${e?.message || e}`, 'error');
    }
    setIndeterminate(null);
  };

  const noopSteerAdopt = () => showNotice('此版本未提供干预建议');
  const onDiffUnavailable = () => showNotice('结构化 diff 查看将在后续协议扩展中提供');

  return (
    <div id="grodex-desktop-root" className="h-screen w-screen flex flex-col bg-[#faf9f7] text-[#3a3f45] overflow-hidden font-sans antialiased">
      {/* Top Application Header */}
      <Header
        session={activeSession}
        onToggleAgentTree={() => setIsAgentTreeOpen(!isAgentTreeOpen)}
        isAgentTreeOpen={isAgentTreeOpen}
        onOpenSettings={() => setIsSettingsOpen(true)}
        onOpenMemory={() => setIsMemoryOpen(true)}
      />

      {/* Transient banners (compaction / notices) */}
      {(isCompacting || notice) && (
        <div className="px-4 flex items-center justify-center bg-transparent -mt-1 relative z-10 pointer-events-none">
          {notice && (
            <div
              className={`mt-1 px-3 py-1.5 rounded-full text-xs shadow-sm border pointer-events-auto ${
                notice.kind === 'error'
                  ? 'bg-[#fdf0f0] text-[#b83838] border-[#f6cfcf]'
                  : notice.kind === 'log'
                  ? 'bg-[#f6f6f8] text-[#5b626c] border-[#e2e2e6]'
                  : 'bg-[#eef2f9] text-[#2a4060] border-[#d2e0f7]'
              }`}
            >
              {notice.message}
            </div>
          )}
          {isCompacting && (
            <div className="mt-1 ml-2 px-3 py-1.5 rounded-full text-xs bg-[#fef8ea] text-[#935f12] border border-[#f5dfb4] shadow-sm pointer-events-auto flex items-center gap-1.5">
              <Loader2 className="w-3 h-3 animate-spin" /> 会话压缩中…
            </div>
          )}
        </div>
      )}

      {/* Main Workspace Body */}
      <div className="flex-1 flex min-h-0 overflow-hidden bg-[#f4f4f6]">
        {/* Left Sessions Sidebar */}
        <Sidebar
          sessions={sessions}
          activeSessionId={activeSessionId}
          onSelectSession={handleSelectSession}
          onNewSession={handleNewSession}
          onResumeSession={(sid) => handleSelectSession(sid)}
          onDeleteSession={handleDeleteSession}
          onRefreshSessions={handleRefreshSessions}
          workspace={workspace}
          onChangeWorkspace={(ws) => setWorkspace(ws)}
          onRunDemo={handleNewSession}
        />

        {/* Center: Main Floating White Canvas */}
        <main
          id="main-timeline-area"
          className={`flex-1 flex flex-col min-w-0 bg-white rounded-2xl border border-[#e5e5e8] shadow-xs relative overflow-hidden transition-all ${
            isAgentTreeOpen ? 'my-2 ml-2 sm:my-2.5 sm:ml-2.5 mr-1 sm:mr-1.5' : 'm-2 sm:m-2.5'
          }`}
        >
          {currentTimeline.length === 0 ? (
            <EmptyState
              onSelectPrompt={(t) => handleSendPrompt(t)}
              onRunDemo={handleNewSession}
              modelName={settings.model}
            />
          ) : (
            <>
              <Timeline items={currentTimeline} onOpenDiff={onDiffUnavailable} />

              <Composer
                onSend={(text) => handleSendPrompt(text)}
                onStop={handleStop}
                isRunning={isRunning}
                activeSessionId={activeSession?.id || activeSessionId}
                tokensUsed={activeSession?.tokensUsed || 0}
                costEstimate={activeSession?.costEstimate || 0}
                treeDepth={activeSession?.treeDepth || 1}
                activeSubagents={subagents.length}
                modelName={settings.model || activeSession?.model}
                steerSuggestion={null}
                onAdoptSteer={noopSteerAdopt}
                onDismissSteer={() => {}}
                onOpenDiff={onDiffUnavailable}
                onResumeCrashed={() => showNotice('崩溃恢复：从任务列表重新打开该会话即可续接')}
                onClearTimeline={() =>
                  setTimelines((prev) => ({ ...prev, [activeSessionId]: [] }))
                }
              />
            </>
          )}
        </main>

        {/* Right Collapsible Agent Tree Panel */}
        <AgentTreePanel
          isOpen={isAgentTreeOpen}
          onClose={() => setIsAgentTreeOpen(false)}
          subagents={subagents}
          onFocusAgent={(id) => showNotice(`已聚焦 ${id}（预览占位）`)}
          onInterruptAgent={(id) => showNotice(`子代理中断在当前 ACP 版本暂不支持`, 'error')}
        />
      </div>

      {/* Operator Permission Approval Modal — one at a time from the queue */}
      {pendingApprovals[0] && (
        <ApprovalModal
          request={pendingApprovals[0]}
          onResolve={handleResolveApproval}
          onDismiss={() => {}}
        />
      )}

      {/* Indeterminate (crash-recovery) resolution modal */}
      {indeterminate && (
        <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-[#21262d]/40 backdrop-blur-xs">
          <div className="w-full max-w-md rounded-3xl bg-white shadow-2xl border border-[#e2ddd3] overflow-hidden">
            <div className="px-6 py-4 bg-[#fdf9f2] border-b border-[#ece6dc] flex items-center gap-2.5">
              <AlertTriangle className="w-5 h-5 text-[#b07419]" />
              <div>
                <h3 className="text-sm font-bold text-[#2b3036]">工具结果未知（崩溃恢复）</h3>
                <p className="text-[11px] text-[#717782] font-mono">{indeterminate.tool_name}</p>
              </div>
            </div>
            <div className="p-6 text-xs text-[#434952] space-y-4">
              <p className="leading-relaxed">
                {indeterminate.message || '会话在工具执行中被中断，无法确认该副作用是否已生效。请根据磁盘实际情况裁决：'}
              </p>
              <div className="flex flex-col gap-2">
                <button
                  onClick={() => handleResolveIndeterminate(indeterminate.call_id, 'succeeded')}
                  className="flex items-center gap-2 px-3.5 py-2 rounded-xl bg-[#eef8ef] text-[#1c6422] border border-[#cbe4cf] font-medium text-left"
                >
                  <Check className="w-4 h-4" /> 已成功执行（副作用已生效）
                </button>
                <button
                  onClick={() => handleResolveIndeterminate(indeterminate.call_id, 'failed')}
                  className="flex items-center gap-2 px-3.5 py-2 rounded-xl bg-[#fdf0f0] text-[#b83838] border border-[#f6cfcf] font-medium text-left"
                >
                  <XCircle className="w-4 h-4" /> 失败/未生效
                </button>
                <button
                  onClick={() => handleResolveIndeterminate(indeterminate.call_id, 'retry')}
                  className="flex items-center gap-2 px-3.5 py-2 rounded-xl bg-[#f4efe8] text-[#525964] border border-[#e4ded5] font-medium text-left"
                >
                  <RotateCcw className="w-4 h-4" /> 丢弃本次调用，让模型重试
                </button>
              </div>
            </div>
          </div>
        </div>
      )}

      {/* Delete-session confirmation dialog */}
      {confirmDelete && (
        <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-[#21262d]/40 backdrop-blur-xs">
          <div className="w-full max-w-sm rounded-3xl bg-white shadow-2xl border border-[#e2ddd3] overflow-hidden">
            <div className="px-6 py-4 bg-[#fdf2f2] border-b border-[#f3d9d9] flex items-center gap-2.5">
              <AlertTriangle className="w-5 h-5 text-[#b83838]" />
              <h3 className="text-sm font-bold text-[#2b3036]">删除会话</h3>
            </div>
            <div className="p-6 text-xs text-[#434952] space-y-4">
              <p>
                确定删除会话「{confirmDelete.title}」？
                <br />
                <span className="font-mono text-[#8a919e]">
                  ~/.grodex/sessions/{confirmDelete.sessionId}
                </span>
                <br />
                下的全部记录将永久移除，无法恢复。
              </p>
              <div className="flex items-center justify-end gap-2">
                <button
                  onClick={() => setConfirmDelete(null)}
                  className="px-4 py-2 rounded-full bg-[#f4efe8] hover:bg-[#ece6dc] text-[#525964] text-xs font-medium"
                >
                  取消
                </button>
                <button
                  onClick={() => performDeleteSession(confirmDelete.sessionId)}
                  className="px-5 py-2 rounded-full bg-[#b83838] hover:bg-[#a02f2f] text-white text-xs font-medium flex items-center gap-1.5"
                >
                  <Trash2 className="w-3.5 h-3.5" /> 确认删除
                </button>
              </div>
            </div>
          </div>
        </div>
      )}

      {/* Directory picker dialog (window.prompt unavailable in Tauri) */}
      {isDirDialogOpen && (
        <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-[#21262d]/40 backdrop-blur-xs">
          <div className="w-full max-w-md rounded-3xl bg-white shadow-2xl border border-[#e2ddd3] overflow-hidden">
            <div className="px-6 py-4 bg-[#f8f8fa] border-b border-[#e5e5e8]">
              <h3 className="text-sm font-bold text-[#2b3036]">选择工作目录</h3>
              <p className="text-[11px] text-[#717782] mt-0.5">
                输入要运行 grodex 的项目绝对路径
              </p>
            </div>
            <div className="p-6 space-y-4">
              <input
                autoFocus
                value={dirInput}
                onChange={(e) => setDirInput(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') submitDirDialog();
                  if (e.key === 'Escape') {
                    setIsDirDialogOpen(false);
                    dirResolveRef.current('');
                    dirResolveRef.current = () => {};
                  }
                }}
                placeholder="/Users/you/dev/my-project"
                className="w-full p-3 rounded-xl bg-white border border-[#e2e2e6] font-mono text-xs text-[#2b3036] focus:outline-none focus:border-[#4f46e5]"
              />
              <div className="flex items-center justify-end gap-2">
                <button
                  onClick={() => {
                    setIsDirDialogOpen(false);
                    dirResolveRef.current('');
                    dirResolveRef.current = () => {};
                  }}
                  className="px-4 py-2 rounded-full bg-[#f4efe8] hover:bg-[#ece6dc] text-[#525964] text-xs font-medium"
                >
                  取消
                </button>
                <button
                  onClick={submitDirDialog}
                  className="px-5 py-2 rounded-full bg-[#4f46e5] hover:bg-[#4338ca] text-white text-xs font-medium"
                >
                  确定
                </button>
              </div>
            </div>
          </div>
        </div>
      )}

      {/* Settings Modal (v1 read-only view of effective config) */}
      {isSettingsOpen && (
        <SettingsModal
          isOpen={isSettingsOpen}
          onClose={() => setIsSettingsOpen(false)}
          settings={settings}
          onSave={async (s) => {
            setSettings(s);
            showNotice('首版为只读展示：provider/model 来自 ~/.grodex/config.toml；工具权限由服务端 [rules] 决定');
          }}
        />
      )}

      {/* Memory management panel */}
      <MemoryManager
        isOpen={isMemoryOpen}
        onClose={() => setIsMemoryOpen(false)}
      />
    </div>
  );
}
