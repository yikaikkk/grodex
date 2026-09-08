import React, { useState } from 'react';
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
}

function folderName(ws: string): string {
  const trimmed = ws.trim();
  if (!trimmed) return '未分组';
  const segs = trimmed.split('/').filter(Boolean);
  return segs.length > 0 ? segs[segs.length - 1] : trimmed;
}

export const Sidebar: React.FC<SidebarProps> = ({
  sessions,
  activeSessionId,
  onSelectSession,
  onNewSession,
  onResumeSession,
  onDeleteSession,
  onRefreshSessions,
}) => {
  const [listOpen, setListOpen] = useState(true);
  const [openFolders, setOpenFolders] = useState<Record<string, boolean>>({});

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
    }
  };

  const byWorkspace = new Map<string, Session[]>();
  for (const s of sessions) {
    const name = folderName(s.workspace);
    if (!byWorkspace.has(name)) byWorkspace.set(name, []);
    byWorkspace.get(name)!.push(s);
  }
  const folders = Array.from(byWorkspace.entries());
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
          onClick={onNewSession}
          className="w-full flex items-center justify-between pl-3 pr-2.5 py-2 rounded-full bg-accent text-white text-xs font-medium transition-all hover:bg-accent-hover active:scale-[0.98] shadow-sm"
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
                  className="flex items-center gap-1.5 px-2 py-1 rounded-lg text-xs font-medium text-secondary hover:bg-black/[0.05] hover:text-primary cursor-pointer select-none transition-colors"
                  title={folderOpen ? '收起该文件夹' : '展开该文件夹'}
                >
                  {folderOpen ? (
                    <ChevronDown className="w-3.5 h-3.5 text-tertiary" />
                  ) : (
                    <ChevronRight className="w-3.5 h-3.5 text-tertiary" />
                  )}
                  <Folder className="w-3.5 h-3.5 text-tertiary" />
                  <span className="truncate flex-1">{name}</span>
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
                          onClick={() => onSelectSession(session.id)}
                          className={`group relative pl-2.5 pr-1.5 py-1.5 rounded-lg transition-colors cursor-pointer text-xs flex items-center justify-between gap-1.5 ${
                            isActive
                              ? 'bg-black/[0.08] text-primary font-medium'
                              : 'text-secondary hover:bg-black/[0.04] hover:text-primary'
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
    </aside>
  );
};
