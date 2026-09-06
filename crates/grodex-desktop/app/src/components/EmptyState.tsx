import React, { useState, useRef } from 'react';
import {
  Plus,
  Hand,
  ChevronDown,
  Mic,
  Monitor,
  Folder,
  Smartphone,
  FileText,
  Dices,
  FileCode2,
  Sparkles,
  Cpu,
} from 'lucide-react';

interface EmptyStateProps {
  onSelectPrompt: (prompt: string) => void;
  onRunDemo: () => void;
  /** Real configured model (avoid hard-coded demo labels). */
  modelName?: string;
}

export const EmptyState: React.FC<EmptyStateProps> = ({
  onSelectPrompt,
  onRunDemo,
  modelName,
}) => {
  const [inputText, setInputText] = useState('');
  const [approvalMode, setApprovalMode] = useState('手动审批');
  const [executionMode, setExecutionMode] = useState('Auto Mode');
  const composingRef = useRef(false);

  const handleKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    // IME composition (Chinese pinyin): Enter confirms the candidate, not send.
    if (
      composingRef.current ||
      e.nativeEvent.isComposing ||
      e.nativeEvent.keyCode === 229
    ) {
      return;
    }
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      if (inputText.trim()) {
        onSelectPrompt(inputText.trim());
      }
    }
  };

  const handleSend = () => {
    if (inputText.trim()) {
      onSelectPrompt(inputText.trim());
    } else {
      // If empty, launch demo
      onRunDemo();
    }
  };

  const quickTags = [
    { label: '应用开发', icon: <Smartphone className="w-3.5 h-3.5 text-[#555a64]" />, prompt: '为当前工程构建一个现代化的全栈任务管理模块' },
    { label: '项目理解', icon: <FileText className="w-3.5 h-3.5 text-[#555a64]" />, prompt: '解析当前 grodex 项目架构、核心模块依赖与入口流程' },
    { label: '游戏创意', icon: <Dices className="w-3.5 h-3.5 text-[#555a64]" />, prompt: '基于 HTML5 Canvas 和 TypeScript 构建一个流畅的太空战机小游戏' },
    { label: '工具脚本', icon: <FileCode2 className="w-3.5 h-3.5 text-[#555a64]" />, prompt: '编写自动化打包构建与 Rust/TypeScript 质量检查测试脚本' },
  ];

  return (
    <div
      id="empty-state-view"
      className="flex-1 flex flex-col items-center justify-center px-4 py-8 max-w-4xl w-full mx-auto select-none"
    >
      {/* Title from TRAE: </> Code with Grodex */}
      <div className="flex items-center gap-3 mb-8">
        <span className="font-mono text-3xl sm:text-4xl font-black text-[#1e2229] tracking-tight">
          &lt;/&gt;
        </span>
        <h1 className="text-3xl sm:text-4xl font-bold text-[#1e2229] tracking-tight font-sans">
          Code with Grodex
        </h1>
      </div>

      {/* Main Central Input Card (TRAE style) */}
      <div className="w-full max-w-2xl rounded-2xl border border-[#e2e2e6] bg-white shadow-sm p-4 transition-all focus-within:border-[#7c3aed] focus-within:ring-2 focus-within:ring-[#7c3aed]/10">
        <textarea
          rows={3}
          value={inputText}
          onChange={(e) => setInputText(e.target.value)}
          onKeyDown={handleKeyDown}
          onCompositionStart={() => (composingRef.current = true)}
          onCompositionEnd={() => (composingRef.current = false)}
          placeholder="帮你编写代码、调试 Bug、优化性能等开发工作，交付生产级代码产物。"
          className="w-full resize-none bg-transparent text-sm text-[#20242c] placeholder-[#9aa0a9] focus:outline-none font-sans leading-relaxed"
        />

        {/* Bottom Toolbar inside Card */}
        <div className="flex items-center justify-between pt-2 mt-1 border-t border-[#f2f2f4]">
          {/* Left options: +, 手动审批, Model pill */}
          <div className="flex items-center gap-2">
            <button
              onClick={onRunDemo}
              className="p-1 rounded-md hover:bg-[#f2f2f5] text-[#555a64] transition-colors"
              title="添加上下文文件或附件"
            >
              <Plus className="w-4 h-4" />
            </button>

            {/* 手动审批 dropdown pill */}
            <div className="flex items-center gap-1 px-2.5 py-1 rounded-md bg-[#f6f6f8] hover:bg-[#ececee] text-xs text-[#3a3f47] cursor-pointer transition-colors">
              <Hand className="w-3.5 h-3.5 text-[#656b77]" />
              <span className="font-medium text-xs">{approvalMode}</span>
              <ChevronDown className="w-3 h-3 text-[#8a909c]" />
            </div>

            {/* Model logos indicator badge */}
            <div
              className="flex items-center gap-1 px-2 py-0.5 rounded-md bg-[#f6f6f8] border border-[#ebebed] text-[11px] text-[#6d28d9] font-medium"
              title="当前配置模型（~/.grodex/config.toml）"
            >
              <span className="w-2 h-2 rounded-full bg-[#7c3aed]" />
              <span className="truncate max-w-[120px]">{modelName || '模型未配置'}</span>
            </div>
          </div>

          {/* Right options: Auto Mode, Mic, Purple Audio/Send Button */}
          <div className="flex items-center gap-2">
            <div className="flex items-center gap-1 text-xs text-[#525762] hover:text-[#1d2026] cursor-pointer font-medium">
              <span>{executionMode}</span>
              <ChevronDown className="w-3 h-3 text-[#8c929e]" />
            </div>

            <button
              className="p-1.5 rounded-md hover:bg-[#f2f2f5] text-[#686e7a] transition-colors"
              title="语音输入"
            >
              <Mic className="w-4 h-4" />
            </button>

            {/* Purple soundwave/send pill button from screenshot */}
            <button
              id="empty-state-send-btn"
              onClick={handleSend}
              className="w-8 h-8 rounded-lg bg-[#4f46e5] hover:bg-[#4338ca] text-white flex items-center justify-center transition-transform active:scale-95 shadow-xs"
              title="发送指令"
            >
              <div className="flex items-center gap-0.5">
                <span className="w-0.5 h-2.5 bg-white rounded-full animate-pulse" />
                <span className="w-0.5 h-4 bg-white rounded-full" />
                <span className="w-0.5 h-2.5 bg-white rounded-full animate-pulse" />
              </div>
            </button>
          </div>
        </div>
      </div>

      {/* Directory Pills below Card (本地 ˅, grodex ˅) */}
      <div className="w-full max-w-2xl flex items-center gap-3 px-1 mt-2.5 text-xs text-[#6e7480]">
        <div className="flex items-center gap-1.5 cursor-pointer hover:text-[#20242c]">
          <Monitor className="w-3.5 h-3.5 text-[#888e99]" />
          <span>本地</span>
          <ChevronDown className="w-3 h-3 text-[#9aa0a9]" />
        </div>

        <div className="flex items-center gap-1.5 cursor-pointer hover:text-[#20242c]">
          <Folder className="w-3.5 h-3.5 text-[#888e99]" />
          <span className="font-mono">grodex</span>
          <ChevronDown className="w-3 h-3 text-[#9aa0a9]" />
        </div>
      </div>

      {/* Quick Action Tag Pills (应用开发, 项目理解, 游戏创意, 工具脚本) */}
      <div className="flex items-center justify-center gap-2.5 mt-8 flex-wrap">
        {quickTags.map((tag, idx) => (
          <button
            key={idx}
            onClick={() => onSelectPrompt(tag.prompt)}
            className="flex items-center gap-1.5 px-3.5 py-1.5 rounded-lg border border-[#e2e2e6] bg-white hover:bg-[#f8f8f9] hover:border-[#d4d4d8] text-xs text-[#3b4049] transition-all shadow-2xs group"
          >
            {tag.icon}
            <span className="font-medium group-hover:text-[#181a1f]">{tag.label}</span>
          </button>
        ))}
      </div>
    </div>
  );
};
