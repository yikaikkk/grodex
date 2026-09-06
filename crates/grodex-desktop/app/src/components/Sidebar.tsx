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
  // Top-level task list collapse.
  const [listOpen, setListOpen] = useState(true);
  // Per-folder collapse (independent of the top level).
  const [openFolders, setOpenFolders] = useState<Record<string, boolean>>({});

  const toggleFolder = (name: string) =>
    setOpenFolders((prev) => ({ ...prev, [name]: !(prev[name] ?? true) }));

  const isFolderOpen = (name: string) => openFolders[name] ?? true;

  const getStatusDot = (status: SessionStatus) => {
    switch (status) {
      case 'running':
        return <span className="w-2 h-2 rounded-full bg-blue-500 animate-pulse shrink-0" />;
      case 'completed':
        return <span className="w-2 h-2 rounded-full bg-emerald-500 shrink-0" />;
      case 'awaiting_approval':
        return <span className="w-2 h-2 rounded-full bg-amber-500 shrink-0" />;
      case 'crashed_recoverable':
        return <span className="w-2 h-2 rounded-full bg-rose-500 shrink-0" />;
    }
  };

  // Group sessions by workspace folder.
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
      className="w-64 h-full border-r border-[#e5e5e8] bg-[#f7f7f8] flex flex-col shrink-0 select-none text-[#33373e]"
    >
      {/* Top: New Task Action Button */}
      <div className="p-3">
        <button
          id="new-session-btn"
          onClick={onNewSession}
          className="w-full flex items-center justify-between px-3 py-2 rounded-lg bg-[#e8e8eb] hover:bg-[#dedee2] text-[#20232a] font-medium text-xs transition-colors shadow-2xs"
        >
          <div className="flex items-center gap-2">
            <Plus className="w-3.5 h-3.5 text-[#20232a]" />
            <span>新建任务</span>
          </div>
          <span className="font-mono text-[10px] text-[#70757f] bg-[#dedee2] px-1.5 py-0.5 rounded">
            ⌘^N
          </span>
        </button>
      </div>

      {/* Section Header: 任务列表 — top-level collapse + refresh */}
      <div className="mt-2 px-2 py-1.5 flex items-center justify-between text-xs text-[#8c919c]">
        <button
          onClick={() => setListOpen(!listOpen)}
          className="flex items-center gap-1.5 font-medium text-[11px] text-[#20232a] hover:text-[#4a5f82] transition-colors"
          title={listOpen ? '收起任务列表' : '展开任务列表'}
        >
          {listOpen ? (
            <ChevronDown className="w-3.5 h-3.5" />
          ) : (
            <ChevronRight className="w-3.5 h-3.5" />
          )}
          <span>任务列表</span>
          <span className="text-[10px] font-mono text-[#a0a6b0]">{totalTasks}</span>
        </button>

        <div className="flex items-center gap-0.5">
          <button
            id="refresh-sessions-btn"
            onClick={onRefreshSessions}
            className="p-1 rounded-md hover:bg-[#e8e8eb] hover:text-[#20232a] text-[#8c919c] transition-colors"
            title="刷新任务列表"
          >
            <RotateCw className="w-3.5 h-3.5" />
          </button>
        </div>
      </div>

      {/* Task tree (top-level list content) */}
      {listOpen && (
        <div className="flex-1 overflow-y-auto px-2 py-1 space-y-1">
          {folders.length === 0 && (
            <div className="px-2.5 py-3 text-[11px] text-[#a0a6b0] text-center">
              暂无任务
            </div>
          )}

          {folders.map(([name, items]) => {
            const folderOpen = isFolderOpen(name);
            return (
              <div key={name} className="space-y-0.5">
                {/* Per-folder row (own collapse/expand) */}
                <div
                  onClick={() => toggleFolder(name)}
                  className="flex items-center gap-1.5 px-2 py-1.5 rounded-md text-xs font-medium text-[#3b4049] hover:bg-[#ececee] cursor-pointer select-none"
                  title={folderOpen ? '收起该文件夹' : '展开该文件夹'}
                >
                  {folderOpen ? (
                    <ChevronDown className="w-3.5 h-3.5 text-[#737985]" />
                  ) : (
                    <ChevronRight className="w-3.5 h-3.5 text-[#737985]" />
                  )}
                  <Folder className="w-3.5 h-3.5 text-[#737985]" />
                  <span className="truncate flex-1">{name}</span>
                  <span className="text-[10px] font-mono text-[#a0a6b0]">{items.length}</span>
                </div>

                {/* Tasks inside this folder */}
                {folderOpen && (
                  <div className="pl-4 space-y-0.5">
                    {items.map((session) => {
                      const isActive = session.id === activeSessionId;
                      const isRecoverable = session.status === 'crashed_recoverable';
                      return (
                        <div
                          key={session.id}
                          id={`session-item-${session.id}`}
                          onClick={() => onSelectSession(session.id)}
                          className={`group relative px-2.5 py-1.5 rounded-lg transition-all cursor-pointer text-xs flex items-center justify-between gap-2 ${
                            isActive
                              ? 'bg-[#e8e8eb] text-[#1a1c20] font-medium'
                              : 'text-[#4e545e] hover:bg-[#ececee] hover:text-[#1a1c20]'
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
                              className="px-1.5 py-0.5 rounded bg-rose-100 hover:bg-rose-200 text-rose-700 text-[10px] font-medium shrink-0"
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
                                ? 'opacity-30 cursor-not-allowed text-[#a0a6b0]'
                                : 'opacity-0 group-hover:opacity-100 text-[#8a909c] hover:text-[#b83838] hover:bg-[#f3e4e4]'
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
