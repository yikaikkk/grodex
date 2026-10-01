import React from 'react';
import { Settings, GitBranch, Brain, Activity, Folder, Files } from 'lucide-react';
import { Session } from '../types';

interface HeaderProps {
  session?: Session;
  onToggleAgentTree: () => void;
  isAgentTreeOpen: boolean;
  onToggleWorkspace: () => void;
  isWorkspaceOpen: boolean;
  onOpenSettings: () => void;
  onOpenMemory: () => void;
  onOpenObservability: () => void;
}

const HeaderInner: React.FC<HeaderProps> = ({
  session,
  onToggleAgentTree,
  isAgentTreeOpen,
  onToggleWorkspace,
  isWorkspaceOpen,
  onOpenSettings,
  onOpenMemory,
  onOpenObservability,
}) => {
  return (
    <header
      id="app-header"
      className="h-12 border-b border-hairline bg-canvas/80 backdrop-blur-xl flex items-center justify-between px-3 select-none shrink-0 z-20"
      data-tauri-drag-region
    >
      {/* Left: workspace / session breadcrumb */}
      <div className="flex items-center gap-2 min-w-0">
        <div className="flex items-center gap-1.5 text-[13px] leading-none min-w-0">
          <span className="font-semibold text-primary tracking-tight shrink-0">grodex</span>
          <span className="text-tertiary">/</span>
          {session?.title && (
            <span className="text-secondary truncate max-w-md">
              {session.title}
            </span>
          )}
        </div>
        {session?.workspace && (
          <span className="hidden lg:flex items-center gap-1 text-[11px] text-tertiary font-mono bg-well px-2 py-0.5 rounded-full">
            <Folder className="w-3 h-3" />
            <span className="truncate max-w-[260px]">{session.workspace}</span>
          </span>
        )}
      </div>

      {/* Right: actions */}
      <div className="flex items-center gap-0.5">
        <button
          id="header-workspace-toggle-btn"
          onClick={onToggleWorkspace}
          className={`flex items-center gap-1 p-1.5 rounded-lg text-xs font-medium transition-all active:scale-95 ${
            isWorkspaceOpen
              ? 'bg-accent-soft text-accent'
              : 'text-secondary hover:bg-black/[0.05] hover:text-primary'
          }`}
          title="展开/收起工作区文件面板"
        >
          <Files className="w-4 h-4" />
        </button>

        <button
          id="header-agent-tree-toggle-btn"
          onClick={onToggleAgentTree}
          className={`flex items-center gap-1 p-1.5 rounded-lg text-xs font-medium transition-all active:scale-95 ${
            isAgentTreeOpen
              ? 'bg-accent-soft text-accent'
              : 'text-secondary hover:bg-black/[0.05] hover:text-primary'
          }`}
          title="展开/收起子 Agent 协同树"
        >
          <GitBranch className="w-4 h-4" />
        </button>

        <button
          id="header-memory-btn"
          onClick={onOpenMemory}
          className="p-1.5 rounded-lg text-secondary hover:bg-black/[0.05] hover:text-primary transition-all active:scale-95"
          title="记忆管理"
        >
          <Brain className="w-4 h-4" />
        </button>

        <button
          id="header-observability-btn"
          onClick={onOpenObservability}
          className="p-1.5 rounded-lg text-secondary hover:bg-black/[0.05] hover:text-primary transition-all active:scale-95"
          title="可观测"
        >
          <Activity className="w-4 h-4" />
        </button>

        <button
          id="header-settings-btn"
          onClick={onOpenSettings}
          className="p-1.5 rounded-lg text-secondary hover:bg-black/[0.05] hover:text-primary transition-all active:scale-95"
          title="配置与权限"
        >
          <Settings className="w-4 h-4" />
        </button>
      </div>
    </header>
  );
};

/** Memoized: during streaming the App re-renders every animation frame;
 * skipping Header keeps the top-bar buttons responsive. */
export const Header = React.memo(HeaderInner);
