import { Settings, GitBranch, Brain } from 'lucide-react';
import { Session } from '../types';

interface HeaderProps {
  session?: Session;
  onToggleAgentTree: () => void;
  isAgentTreeOpen: boolean;
  onOpenSettings: () => void;
  onOpenMemory: () => void;
}

export const Header: React.FC<HeaderProps> = ({
  session,
  onToggleAgentTree,
  isAgentTreeOpen,
  onOpenSettings,
  onOpenMemory,
}) => {
  return (
    <header
      id="app-header"
      className="h-11 border-b border-[#e5e5e8] bg-[#f4f4f6] flex items-center justify-between px-3 select-none shrink-0 z-20"
    >
      {/* Left: session title */}
      {session && (
        <div className="flex items-center gap-1.5 text-xs text-[#525760] min-w-0">
          <span className="text-[#8e939d]">grodex</span>
          <span className="text-[#c1c5cc]">/</span>
          <span className="font-medium text-[#20232a] truncate max-w-md">
            {session.title}
          </span>
        </div>
      )}

      {/* Right: actions */}
      <div className="flex items-center gap-1.5">
        <button
          id="header-agent-tree-toggle-btn"
          onClick={onToggleAgentTree}
          className={`flex items-center gap-1 px-2 py-1 rounded-md text-xs font-medium border transition-all ${
            isAgentTreeOpen
              ? 'bg-[#eef2fa] text-[#2c4e85] border-[#cbd8ed]'
              : 'bg-[#ffffff] hover:bg-[#f0f0f2] text-[#3e444c] border-[#dcdce0]'
          }`}
          title="展开/收起子 Agent 协同树"
        >
          <GitBranch className="w-4 h-4 text-[#4a638b]" />
        </button>

        <button
          id="header-memory-btn"
          onClick={onOpenMemory}
          className="p-1.5 rounded-md bg-[#ffffff] hover:bg-[#f0f0f2] text-[#4d535b] border border-[#dcdce0] transition-all shadow-xs"
          title="记忆管理"
        >
          <Brain className="w-4 h-4 text-[#4a638b]" />
        </button>

        <button
          id="header-settings-btn"
          onClick={onOpenSettings}
          className="p-1.5 rounded-md bg-[#ffffff] hover:bg-[#f0f0f2] text-[#4d535b] border border-[#dcdce0] transition-all shadow-xs"
          title="配置与权限"
        >
          <Settings className="w-4 h-4" />
        </button>
      </div>
    </header>
  );
};
