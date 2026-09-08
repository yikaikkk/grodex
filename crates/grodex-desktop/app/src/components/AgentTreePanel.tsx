import React, { useState } from 'react';
import {
  GitBranch,
  Bot,
  CheckCircle2,
  AlertTriangle,
  Clock,
  Square,
  Eye,
  ChevronDown,
  ChevronRight,
  Sparkles,
  Loader2,
  X,
} from 'lucide-react';
import { SubAgentNode } from '../types';

interface AgentTreePanelProps {
  isOpen: boolean;
  onClose: () => void;
  subagents: SubAgentNode[];
  onFocusAgent: (agentId: string) => void;
  onInterruptAgent: (agentId: string) => void;
}

export const AgentTreePanel: React.FC<AgentTreePanelProps> = ({
  isOpen,
  onClose,
  subagents,
  onFocusAgent,
  onInterruptAgent,
}) => {
  const [expandedAgents, setExpandedAgents] = useState<Record<string, boolean>>({
    agent_main: true,
    agent_sub_1: true,
  });

  if (!isOpen) return null;

  const toggleExpand = (id: string) => {
    setExpandedAgents((prev) => ({ ...prev, [id]: !prev[id] }));
  };

  const getStatusBadge = (status: SubAgentNode['status']) => {
    switch (status) {
      case 'thinking':
        return (
          <span className="flex items-center gap-1 text-[10px] px-2 py-0.5 rounded-full bg-accent-soft text-accent border border-accent-soft font-mono">
            <Sparkles className="w-2.5 h-2.5 animate-spin text-accent" />
            思考中
          </span>
        );
      case 'running_tool':
        return (
          <span className="flex items-center gap-1 text-[10px] px-2 py-0.5 rounded-full bg-accent-soft text-accent border border-accent-soft font-mono">
            <Loader2 className="w-2.5 h-2.5 animate-spin text-accent" />
            执行工具中
          </span>
        );
      case 'waiting':
        return (
          <span className="flex items-center gap-1 text-[10px] px-2 py-0.5 rounded-full bg-orange-soft text-orange border border-orange-soft font-mono">
            <Clock className="w-2.5 h-2.5 text-orange" />
            等待中
          </span>
        );
      case 'done':
        return (
          <span className="flex items-center gap-1 text-[10px] px-2 py-0.5 rounded-full bg-green-soft text-green-dark border border-green-soft font-mono">
            <CheckCircle2 className="w-2.5 h-2.5 text-green-dark" />
            已就绪
          </span>
        );
      case 'interrupted':
        return (
          <span className="flex items-center gap-1 text-[10px] px-2 py-0.5 rounded-full bg-red-soft text-red-dark border border-red-soft font-mono">
            <AlertTriangle className="w-2.5 h-2.5 text-red-dark" />
            已中断
          </span>
        );
    }
  };

  return (
    <aside
      id="agent-tree-panel"
      className="w-80 sm:w-88 my-2 mr-2 sm:my-2.5 sm:mr-2.5 ml-1 sm:ml-1.5 rounded-2xl border border-hairline bg-white shadow-xs flex flex-col shrink-0 text-primary overflow-hidden select-none"
    >
      {/* Panel Header */}
      <div className="flex items-center justify-between px-4 py-3 border-b border-hairline bg-white">
        <div className="flex items-center gap-2">
          <GitBranch className="w-4 h-4 text-accent" />
          <h3 className="text-xs font-bold text-primary tracking-wide font-sans">
            AGENT 协同树与任务委派
          </h3>
        </div>
        <button
          id="close-agent-tree-btn"
          onClick={onClose}
          className="p-1 rounded-md hover:bg-canvas text-secondary hover:text-primary transition-colors"
          title="关闭面板"
        >
          <X className="w-4 h-4" />
        </button>
      </div>

      {/* Info notice */}
      <div className="px-3.5 py-2 bg-well border-b border-hairline text-[11px] text-secondary flex items-center gap-2">
        <Bot className="w-3.5 h-3.5 text-accent shrink-0" />
        <span>子 Agent 在独立的隔离上下文中并行协作运行。</span>
      </div>

      {/* Tree list */}
      <div className="flex-1 overflow-y-auto p-3 space-y-2.5">
        {subagents.map((agent) => {
          const isExpanded = expandedAgents[agent.id] ?? false;
          const isChild = agent.parentId !== '';

          return (
            <div
              key={agent.id}
              id={`agent-node-${agent.id}`}
              className={`rounded-xl border transition-all ${
                isChild
                  ? 'ml-3 border-l-2 border-l-accent border-hairline bg-card shadow-2xs'
                  : 'border-hairline bg-white shadow-2xs'
              }`}
            >
              {/* Node Header */}
              <div className="p-3 space-y-2">
                <div className="flex items-center justify-between">
                  <button
                    onClick={() => toggleExpand(agent.id)}
                    className="flex items-center gap-1.5 font-medium text-xs text-primary hover:text-primary text-left"
                  >
                    {isExpanded ? (
                      <ChevronDown className="w-3.5 h-3.5 text-secondary" />
                    ) : (
                      <ChevronRight className="w-3.5 h-3.5 text-secondary" />
                    )}
                    <span className="font-semibold">{agent.name}</span>
                  </button>
                  {getStatusBadge(agent.status)}
                </div>

                <p className="text-[11px] text-secondary font-sans leading-tight pl-5 line-clamp-2">
                  {agent.task}
                </p>

                <div className="flex items-center justify-between text-[10px] text-tertiary font-sans pl-5 pt-0.5">
                  <span>{agent.tokensUsed.toLocaleString()} 令牌</span>
                  <span>运行 {agent.durationSec}s</span>
                </div>

                {/* Agent Actions */}
                <div className="flex items-center justify-end gap-1.5 pt-1.5 pl-5 border-t border-hairline-2">
                  <button
                    id={`focus-agent-btn-${agent.id}`}
                    onClick={() => onFocusAgent(agent.id)}
                    className="px-2.5 py-1 rounded-md bg-well hover:bg-well text-primary text-[10px] font-medium flex items-center gap-1 transition-colors"
                  >
                    <Eye className="w-3 h-3 text-secondary" />
                    定位追踪
                  </button>
                  {agent.status !== 'done' && agent.status !== 'interrupted' && (
                    <button
                      id={`interrupt-agent-btn-${agent.id}`}
                      onClick={() => onInterruptAgent(agent.id)}
                      className="px-2.5 py-1 rounded-md bg-red-soft hover:bg-red-soft text-red-dark text-[10px] font-medium flex items-center gap-1 transition-colors border border-red-soft"
                    >
                      <Square className="w-2.5 h-2.5 fill-red-dark text-red-dark" />
                      紧急中断
                    </button>
                  )}
                </div>
              </div>

              {/* Logs / Conversation preview */}
              {isExpanded && agent.logs.length > 0 && (
                <div className="px-3 py-2 border-t border-hairline-2 bg-well rounded-b-xl space-y-1">
                  <div className="text-[9px] uppercase font-sans text-secondary font-bold tracking-wider">
                    实时执行日志
                  </div>
                  <div className="space-y-0.5 max-h-28 overflow-y-auto font-mono text-[10px] text-secondary leading-normal">
                    {agent.logs.map((log, lIdx) => (
                      <div key={lIdx} className="truncate flex items-center gap-1.5">
                        <span className="text-tertiary">›</span>
                        <span>{log}</span>
                      </div>
                    ))}
                  </div>
                </div>
              )}
            </div>
          );
        })}
      </div>
    </aside>
  );
};
