import React, { useEffect, useState } from 'react';
import {
  FileText,
  FileCode,
  Terminal,
  Search,
  CheckCircle2,
  AlertCircle,
  Clock,
  ChevronDown,
  ChevronRight,
  GitPullRequest,
  Bot,
  ExternalLink,
  ShieldAlert,
  Loader2,
} from 'lucide-react';
import { ToolItem, ToolName } from '../types';
import { ExecTerminal } from './ExecTerminal';

interface ToolCardProps {
  item: ToolItem;
  onOpenDiff?: (diffId: string) => void;
  /** Compact rendering for tool calls nested inside a thinking frame. */
  compact?: boolean;
}

export const ToolCard: React.FC<ToolCardProps> = ({ item, onOpenDiff, compact }) => {
  const [isExpanded, setIsExpanded] = useState(true);
  const [liveElapsed, setLiveElapsed] = useState(item.elapsedSec || 0);

  // Dynamic seconds counter while tool is running
  useEffect(() => {
    if (item.status === 'running') {
      const interval = setInterval(() => {
        const elapsed = (Date.now() - item.startTime) / 1000;
        setLiveElapsed(elapsed);
      }, 100);
      return () => clearInterval(interval);
    } else {
      setLiveElapsed(item.elapsedSec || 0);
    }
  }, [item.status, item.startTime, item.elapsedSec]);

  const getToolIcon = (name: ToolName) => {
    switch (name) {
      case 'read_file':
        return <FileText className="w-4 h-4 text-[#3d6594]" />;
      case 'write_file':
      case 'edit_file':
        return <FileCode className="w-4 h-4 text-[#a06814]" />;
      case 'exec':
        return <Terminal className="w-4 h-4 text-[#2e6d34]" />;
      case 'grep':
      case 'glob':
        return <Search className="w-4 h-4 text-[#604f85]" />;
      case 'apply_patch':
        return <GitPullRequest className="w-4 h-4 text-[#4a5f82]" />;
      case 'delegate_task':
        return <Bot className="w-4 h-4 text-[#8a4253]" />;
      default:
        return <FileText className="w-4 h-4 text-[#666d78]" />;
    }
  };

  const getStatusBadge = () => {
    switch (item.status) {
      case 'pending':
        return (
          <span className="flex items-center gap-1 text-[11px] px-2.5 py-0.5 rounded-full bg-[#f2eee7] text-[#6b655a] border border-[#e4ded3]">
            <Clock className="w-3 h-3" />
            排队中
          </span>
        );
      case 'running':
        return (
          <span className="flex items-center gap-1.5 text-[11px] px-2.5 py-0.5 rounded-full bg-[#eef4fe] text-[#2c5b96] border border-[#d2e2f9] font-mono">
            <Loader2 className="w-3 h-3 animate-spin text-[#3a6ea5]" />
            {liveElapsed.toFixed(1)}s
          </span>
        );
      case 'awaiting_approval':
        return (
          <span className="flex items-center gap-1.5 text-[11px] px-3 py-0.5 rounded-full bg-[#fef8ea] text-[#935f12] border border-[#f5dfb4] font-medium shadow-xs">
            <ShieldAlert className="w-3.5 h-3.5 text-[#b07419]" />
            等待审批
          </span>
        );
      case 'finished':
        return (
          <span className="flex items-center gap-1 text-[11px] px-2.5 py-0.5 rounded-full bg-[#eaf5eb] text-[#256e2c] border border-[#d0e9d4] font-mono">
            <CheckCircle2 className="w-3 h-3" />
            {liveElapsed.toFixed(1)}s
          </span>
        );
      case 'failed':
        return (
          <span className="flex items-center gap-1 text-[11px] px-2.5 py-0.5 rounded-full bg-[#fdf1f1] text-[#b83838] border border-[#f8d4d4] font-mono">
            <AlertCircle className="w-3 h-3" />
            失败
          </span>
        );
    }
  };

  const mainTarget = item.params.path || item.params.command || item.params.pattern || item.toolName;

  return (
    <div
      id={`tool-card-${item.id}`}
      className={`${compact ? 'my-1.5 rounded-xl' : 'my-3 rounded-2xl'} border transition-all duration-150 ${
        item.status === 'awaiting_approval'
          ? 'border-[#f5dfb4] bg-[#fefcf9] shadow-md ring-1 ring-[#f5dfb4]'
          : item.status === 'running'
          ? 'border-[#cbd8eb] bg-[#f8fbff] shadow-xs'
          : 'border-[#ebe5dc] bg-[#ffffff] shadow-xs hover:border-[#ded7cc]'
      }`}
    >
      {/* Tool Header */}
      <div
        className={`flex items-center justify-between ${compact ? 'px-2.5 py-1.5' : 'px-3.5 py-2.5'}`}
      >
        <div className="flex items-center gap-2.5 min-w-0 flex-1">
          <button
            id={`toggle-tool-${item.id}`}
            onClick={() => setIsExpanded(!isExpanded)}
            className="p-0.5 rounded-full hover:bg-[#f0ece5] text-[#868d98] hover:text-[#3a3f45] transition-colors"
          >
            {isExpanded ? <ChevronDown className="w-4 h-4" /> : <ChevronRight className="w-4 h-4" />}
          </button>

          <div className="p-1.5 rounded-xl bg-[#f4f0e8] border border-[#e8e2d7] shrink-0">
            {getToolIcon(item.toolName)}
          </div>

          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2 flex-wrap">
              <span className="text-xs font-mono font-bold text-[#2d3238] uppercase tracking-wide">
                {item.toolName}
              </span>
              {item.sourceAgent && item.sourceAgent !== 'main' && (
                <span className="text-[10px] px-2 py-0.2 rounded-full bg-[#eef2f8] text-[#4a638b] border border-[#dce5f2] font-mono">
                  {item.sourceAgent}
                </span>
              )}
            </div>
            <p className="text-xs font-mono text-[#656c76] truncate mt-0.5">
              {mainTarget}
            </p>
          </div>
        </div>

        <div className="flex items-center gap-2.5 shrink-0">
          {item.diffId && onOpenDiff && (
            <button
              id={`view-diff-btn-${item.id}`}
              onClick={() => onOpenDiff(item.diffId!)}
              className="px-3 py-1 text-xs font-medium rounded-full bg-[#fdf6e9] hover:bg-[#faeed6] text-[#935f12] border border-[#f4dfb5] flex items-center gap-1.5 transition-colors shadow-xs"
            >
              <FileCode className="w-3.5 h-3.5" />
              <span>查看变更</span>
              <span className="text-[10px] text-[#256e2c] font-mono font-bold">+18</span>
              <span className="text-[10px] text-[#b83838] font-mono font-bold">-6</span>
              <ExternalLink className="w-3 h-3 opacity-70" />
            </button>
          )}

          {getStatusBadge()}
        </div>
      </div>

      {/* Expanded Content Area */}
      {isExpanded && (
        <div
          className={`${compact ? 'px-3 py-2.5 rounded-b-xl' : 'px-4 py-3.5 rounded-b-2xl'} border-t border-[#ebe5dc] bg-[#faf8f4] space-y-3 text-xs`}
        >
          {/* Structured parameters JSON (hidden in compact so the nested tool
              stays a small box) */}
          {!compact && (
            <div className="space-y-1">
              <div className="text-[10px] uppercase font-sans text-[#7a818c] font-semibold tracking-wider">
                工具输入参数 (Payload)
              </div>
              <pre className="p-3 rounded-xl bg-[#f4f1ea] border border-[#e5dfd4] text-[11px] font-mono text-[#383d44] overflow-x-auto">
                {JSON.stringify(item.params, null, 2)}
              </pre>
            </div>
          )}

          {/* Exec terminal output if exec tool */}
          {item.toolName === 'exec' && (
            <ExecTerminal
              command={item.params.command || 'exec'}
              cwd={item.params.cwd}
              output={item.execOutput}
              exitCode={item.exitCode}
              isRunning={item.status === 'running'}
            />
          )}

          {/* Result summary */}
          {item.resultSummary && item.toolName !== 'exec' && (
            <div className="pt-2 border-t border-[#ebe5dc]">
              <div className="text-[10px] uppercase font-sans text-[#7a818c] font-semibold tracking-wider mb-1">
                工具返回结果
              </div>
              <div className="p-3 rounded-xl bg-[#f4f1ea] border border-[#e5dfd4] text-xs text-[#383d44] font-mono leading-relaxed">
                {item.resultSummary}
              </div>
            </div>
          )}

          {/* Error notice if failed */}
          {item.error && (
            <div className="p-3 rounded-xl bg-[#fdf1f1] border border-[#f8d4d4] text-xs text-[#b83838] font-mono">
              执行错误: {item.error}
            </div>
          )}
        </div>
      )}
    </div>
  );
};
