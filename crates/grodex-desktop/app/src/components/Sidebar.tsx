import React, { useEffect, useRef, useState } from 'react';
import { Plus, Folder, Trash2, ChevronRight, ChevronDown, RotateCw } from 'lucide-react';
import { Session, SessionStatus } from '../types';

interface SidebarProps {
  sessions: Session[];
  activeSessionId: string;
  onSelectSession: (sessionId: string) => void;
  onNewSession: () => void;
  onResumeSession: (sessionId: string) => void;
  onDeleteSession: (sessionId: string) => void;
  /** Reload the session list from disk. */
  onRefreshSessions: () => void;
  workspace: string;
  onChangeWorkspace: (newPath: string) => void;
  onRunDemo: () => void;
  /** True while a turn is running: switching sessions would interrupt it. */
  switchLocked?: boolean;
}

/** Last path segment of an absolute workspace path — the group label.
 *  Non-path keys (e.g. 「（无工作目录）」) pass through unchanged. */
function lastSegment(path: string): string {
  const segs = path.split('/').filter(Boolean);
  return segs.length > 0 ? segs[segs.length - 1] : path;
}

const SidebarInner: React.FC<SidebarProps> = ({  sessions,
  activeSessionId,
  onSelectSession,
  onNewSession,
  onResumeSession,
  onDeleteSession,
  onRefreshSessions,
  /** True while a turn is running: switching sessions would interrupt it. */
  switchLocked = false,
}) => {
  const [listOpen, setListOpen] = useState(true);
  const [openFolders, setOpenFolders] = useState<Record<string, boolean>>({});
  // Custom tooltip: after hovering a session row for 1.5s, show its absolute
  // workspace path near the cursor (native `title` fires too early/inconsistently).
  const [tip, setTip] = useState<{ x: number; y: number; text: string } | null>(null);
  const tipTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const clearTip = () => {
    if (tipTimer.current) clearTimeout(tipTimer.current);
    tipTimer.current = null;
    setTip(null);
  };
  const scheduleTip = (e: React.MouseEvent, text: string) => {
    if (tipTimer.current) clearTimeout(tipTimer.current);
    const { clientX: x, clientY: y } = e;
    tipTimer.current = setTimeout(() => setTip({ x, y, text }), 1500);
  };
  useEffect(() => clearTip, []);

  const toggleFolder = (name: string) =>
    setOpenFolders((prev) => ({ ...prev, [name]: !(prev[name] ?? true) }));

  const isFolderOpen = (name: string) => openFolders[name] ?? true;

  const getStatusDot = (status: SessionStatus) => {
    switch (status) {
      case 'running':
        return <span className="w-2 h-2 rounded-full bg-accent animate-breathe shrink-0" />;
      case 'completed':
        return <span className="w-2 h-2 rounded-full bg-green shrink-0" />;
      case 'awaiting_approval':
        return <span className="w-2 h-2 rounded-full bg-orange shrink-0" />;
      case 'crashed_recoverable':
        return <span className="w-2 h-2 rounded-full bg-red shrink-0" />;
      case 'cancelling':
        return <span className="w-2 h-2 rounded-full bg-tertiary animate-breathe shrink-0" />;
    }
  };

  // Group by the ABSOLUTE workspace path (not the folder name) — two
  // different directories sharing a folder name must not collide.
  const byWorkspace = new Map<string, Session[]>();
  for (const s of sessions) {
    const key = s.workspace || '（无工作目录）';
    if (!byWorkspace.has(key)) byWorkspace.set(key, []);
    byWorkspace.get(key)!.push(s);
  }
  const folders = Array.from(byWorkspace.entries()).sort(([a], [b]) =>
    a.localeCompare(b)
  );
  const totalTasks = sessions.length;

  return (
    <aside
      id="sessions-sidebar"
      className="w-60 h-full flex flex-col shrink-0 select-none text-primary"
    >
      {/* Top: New Task Action Button */}
      <div className="p-3">
        <button
          id="new-session-btn"
          onClick={() => {
            if (switchLocked) return; // guard notice comes from App
            onNewSession();
          }}
          disabled={switchLocked}
          className={`w-full flex items-center justify-between pl-3 pr-2.5 py-2 rounded-full text-xs font-medium transition-all shadow-sm ${
            switchLocked
              ? 'bg-accent/40 cursor-not-allowed text-white/70'
              : 'bg-accent hover:bg-accent-hover active:scale-[0.98] text-white'
          }`}
          title={switchLocked ? '当前会话任务未结束，请先停止' : '新建任务'}
        >
          <span className="flex items-center gap-1.5">
            <Plus className="w-3.5 h-3.5" />
            新建任务
          </span>
          <span className="font-mono text-[10px] text-white/70">⌘N</span>
        </button>
      </div>

      {/* Section Header: 任务列表 */}
      <div className="mt-1 px-3 py-1.5 flex items-center justify-between">
        <button
          onClick={() => setListOpen(!listOpen)}
          className="flex items-center gap-1.5 text-[11px] font-semibold text-secondary hover:text-primary transition-colors"
          title={listOpen ? '收起任务列表' : '展开任务列表'}
        >
          {listOpen ? (
            <ChevronDown className="w-3.5 h-3.5" />
          ) : (
            <ChevronRight className="w-3.5 h-3.5" />
          )}
          <span>任务列表</span>
          <span className="text-[10px] font-mono text-tertiary">{totalTasks}</span>
        </button>

        <button
          id="refresh-sessions-btn"
          onClick={onRefreshSessions}
          className="p-1 rounded-md text-tertiary hover:text-primary hover:bg-black/[0.05] transition-colors"
          title="刷新任务列表"
        >
          <RotateCw className="w-3.5 h-3.5" />
        </button>
      </div>

      {/* Task tree */}
      {listOpen && (
        <div className="flex-1 overflow-y-auto px-2.5 py-1 space-y-0.5">
          {folders.length === 0 && (
            <div className="px-2.5 py-3 text-[11px] text-tertiary text-center">
              暂无任务
            </div>
          )}

          {folders.map(([name, items]) => {
            const folderOpen = isFolderOpen(name);
            return (
              <div key={name} className="space-y-0.5">
                <div
                  onClick={() => toggleFolder(name)}
                  onMouseEnter={(e) => scheduleTip(e, name)}
                  onMouseLeave={clearTip}
                  className="flex items-center gap-1.5 px-2 py-1 rounded-lg text-xs font-medium text-secondary hover:bg-black/[0.05] hover:text-primary cursor-pointer select-none transition-colors"
                >
                  {folderOpen ? (
                    <ChevronDown className="w-3.5 h-3.5 text-tertiary" />
                  ) : (
                    <ChevronRight className="w-3.5 h-3.5 text-tertiary" />
                  )}
                  <Folder className="w-3.5 h-3.5 text-tertiary shrink-0" />
                  {/* Label shows only the folder name; the full absolute
                      path appears in the 1.5s hover tooltip. */}
                  <span className="truncate flex-1">{lastSegment(name)}</span>
                  <span className="text-[10px] font-mono text-tertiary">{items.length}</span>
                </div>

                {folderOpen && (
                  <div className="pl-3 space-y-0.5">
                    {items.map((session) => {
                      const isActive = session.id === activeSessionId;
                      const isRecoverable = session.status === 'crashed_recoverable';
                      return (
                        <div
                          key={session.id}
                          id={`session-item-${session.id}`}
                          onClick={() => {
                            clearTip();
                            if (switchLocked && !isActive) return; // App shows the notice
                            onSelectSession(session.id);
                          }}
                          onMouseEnter={(e) =>
                            scheduleTip(e, session.workspace || '（无工作目录）')
                          }
                          onMouseLeave={clearTip}
                          title={
                            switchLocked && !isActive
                              ? '当前会话任务未结束，无法切换'
                              : undefined
                          }
                          className={`group relative pl-2.5 pr-1.5 py-1.5 rounded-lg transition-colors text-xs flex items-center justify-between gap-1.5 ${
                            isActive
                              ? 'bg-black/[0.08] text-primary font-medium cursor-default'
                              : switchLocked
                              ? 'text-secondary opacity-50 cursor-not-allowed'
                              : 'cursor-pointer text-secondary hover:bg-black/[0.04] hover:text-primary'
                          }`}
                        >
                          <div className="flex items-center gap-2 min-w-0 flex-1">
                            {getStatusDot(session.status)}
                            <span className="truncate">{session.title}</span>
                          </div>

                          {isRecoverable && (
                            <button
                              id={`resume-session-btn-${session.id}`}
                              onClick={(e) => {
                                e.stopPropagation();
                                onResumeSession(session.id);
                              }}
                              className="px-1.5 py-0.5 rounded-md bg-red-soft text-red-dark text-[10px] font-medium shrink-0 hover:bg-red/20 transition-colors"
                              title="恢复崩溃会话"
                            >
                              恢复
                            </button>
                          )}

                          <button
                            id={`delete-session-btn-${session.id}`}
                            onClick={(e) => {
                              e.stopPropagation();
                              onDeleteSession(session.id);
                            }}
                            disabled={isActive}
                            title={isActive ? '当前打开的会话不能删除' : '删除会话'}
                            className={`p-1 rounded transition-colors shrink-0 ${
                              isActive
                                ? 'opacity-30 cursor-not-allowed text-tertiary'
                                : 'opacity-0 group-hover:opacity-100 text-tertiary hover:text-red-dark hover:bg-red-soft'
                            }`}
                          >
                            <Trash2 className="w-3.5 h-3.5" />
                          </button>
                        </div>
                      );
                    })}
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
      {/* Deferred absolute-path tooltip (1.5s hover) */}
      {tip && (
        <div
          className="fixed z-[70] max-w-md px-2.5 py-1.5 rounded-lg bg-[#1f2430] text-white text-[11px] font-mono shadow-lg pointer-events-none break-all"
          style={{
            left: Math.min(tip.x + 14, window.innerWidth - 24),
            top: Math.min(tip.y + 18, window.innerHeight - 48),
          }}
        >
          {tip.text}
        </div>
      )}
    </aside>
  );
};

/** Memoized: streaming frame flushes must not re-render the session list. */
export const Sidebar = React.memo(SidebarInner);
