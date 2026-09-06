import React, { useState, useRef, useEffect, KeyboardEvent } from 'react';
import {
  Send,
  Square,
  Sparkles,
  ChevronDown,
  ShieldCheck,
  Cpu,
  Layers,
  Zap,
  RotateCcw,
  Maximize2,
  FileCode,
  Trash2,
  Terminal,
} from 'lucide-react';
import { SteerSuggestion } from '../types';

interface ComposerProps {
  onSend: (message: string, mode: string, model: string) => void;
  onStop: () => void;
  isRunning: boolean;
  activeSessionId: string;
  tokensUsed: number;
  costEstimate: number;
  treeDepth: number;
  activeSubagents: number;
  /** Real model name from the backend config (NOT hard-coded). */
  modelName?: string;
  steerSuggestion: SteerSuggestion | null;
  onAdoptSteer: (suggestion: SteerSuggestion) => void;
  onDismissSteer: () => void;
  onOpenDiff: () => void;
  onResumeCrashed?: () => void;
  onClearTimeline?: () => void;
}

interface SlashCommand {
  command: string;
  label: string;
  description: string;
  icon: React.ReactNode;
}

const SLASH_COMMANDS: SlashCommand[] = [
  { command: '/compact', label: '/compact', description: '压缩上下文并清理冗余输出', icon: <Layers className="w-3.5 h-3.5" /> },
  { command: '/retry', label: '/retry', description: '重试上一轮 Agent 执行步骤', icon: <RotateCcw className="w-3.5 h-3.5" /> },
  { command: '/clear', label: '/clear', description: '清空当前会话的时间线消息', icon: <Trash2 className="w-3.5 h-3.5" /> },
];

export const Composer: React.FC<ComposerProps> = ({
  onSend,
  onStop,
  isRunning,
  activeSessionId,
  tokensUsed,
  costEstimate,
  treeDepth,
  activeSubagents,
  steerSuggestion,
  onAdoptSteer,
  onDismissSteer,
  onOpenDiff,
  onResumeCrashed,
  onClearTimeline,
  modelName,
}) => {
  const [inputText, setInputText] = useState('');
  const [selectedMode, setSelectedMode] = useState<'Auto' | 'Plan' | 'Build' | 'Review'>('Auto');
  const [isModeOpen, setIsModeOpen] = useState(false);

  // Slash commands popup state
  const [showSlashMenu, setShowSlashMenu] = useState(false);
  const [slashQuery, setSlashQuery] = useState('');
  const [selectedSlashIndex, setSelectedSlashIndex] = useState(0);

  const textareaRef = useRef<HTMLTextAreaElement>(null);
  // Reliable IME state: `keydown.isComposing` is unreliable in some WebKit
  // builds, so track composition explicitly via start/end events.
  const composingRef = useRef(false);

  // Handle textarea text change and detect slash
  const handleChange = (e: React.ChangeEvent<HTMLTextAreaElement>) => {
    const val = e.target.value;
    setInputText(val);

    // If starts with / and no spaces yet
    if (val.startsWith('/') && !val.includes(' ')) {
      setShowSlashMenu(true);
      setSlashQuery(val);
      setSelectedSlashIndex(0);
    } else {
      setShowSlashMenu(false);
    }
  };

  const filteredSlashCommands = SLASH_COMMANDS.filter((cmd) =>
    cmd.command.toLowerCase().includes(slashQuery.toLowerCase())
  );

  const executeSlashCommand = (cmd: SlashCommand) => {
    setShowSlashMenu(false);
    setInputText('');

    if (cmd.command === '/clear' && onClearTimeline) {
      onClearTimeline();
    } else {
      // Execute command as prompt
      onSend(cmd.command, selectedMode, modelName ?? '');
    }
  };

  const handleKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    // IME composition (e.g. Chinese pinyin): Enter confirms the candidate —
    // it must NOT submit/send or trigger a slash command. Guard on all signals:
    // tracked composition, isComposing, and the IME keyCode 229 sentinel.
    if (
      composingRef.current ||
      e.nativeEvent.isComposing ||
      e.nativeEvent.keyCode === 229
    ) {
      return;
    }

    if (showSlashMenu && filteredSlashCommands.length > 0) {
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        setSelectedSlashIndex((prev) => (prev + 1) % filteredSlashCommands.length);
        return;
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault();
        setSelectedSlashIndex((prev) => (prev - 1 + filteredSlashCommands.length) % filteredSlashCommands.length);
        return;
      }
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        executeSlashCommand(filteredSlashCommands[selectedSlashIndex]);
        return;
      }
      if (e.key === 'Escape') {
        setShowSlashMenu(false);
        return;
      }
    }

    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      handleSubmit();
    }
  };

  const handleSubmit = () => {
    if (!inputText.trim() || isRunning) return;
    onSend(inputText.trim(), selectedMode, modelName ?? '');
    setInputText('');
    setShowSlashMenu(false);
  };

  return (
    <div id="composer-container" className="w-full px-4 sm:px-6 pb-4 pt-1 select-none">
      {/* Auto-steer banner during streaming */}
      {isRunning && steerSuggestion && (
        <div
          id="steer-suggestion-banner"
          className="mb-2 p-2.5 px-3.5 rounded-xl bg-[#eef2f9] border border-[#d2e0f7] shadow-2xs flex items-center justify-between gap-3 text-xs"
        >
          <div className="flex items-center gap-2 text-[#2a4060]">
            <Zap className="w-4 h-4 text-[#c77f16] shrink-0" />
            <span className="font-medium">{steerSuggestion.message}</span>
          </div>
          <div className="flex items-center gap-2 shrink-0">
            <button
              id="adopt-steer-btn"
              onClick={() => onAdoptSteer(steerSuggestion)}
              className="px-3 py-1 rounded-lg bg-[#4f46e5] hover:bg-[#4338ca] text-white font-medium text-xs transition-colors shadow-2xs"
            >
              采纳建议
            </button>
            <button
              id="dismiss-steer-btn"
              onClick={onDismissSteer}
              className="px-2.5 py-1 rounded-lg hover:bg-[#dfeaf8] text-[#63738b] hover:text-[#2a4060] text-xs transition-colors"
            >
              忽略
            </button>
          </div>
        </div>
      )}

      {/* Slash command floating palette */}
      {showSlashMenu && filteredSlashCommands.length > 0 && (
        <div
          id="slash-command-palette"
          className="absolute bottom-28 left-10 mb-2 w-80 rounded-2xl border border-[#e2e2e6] bg-white shadow-xl overflow-hidden z-40"
        >
          <div className="px-3.5 py-2 bg-[#f8f8fa] border-b border-[#ececed] text-[10px] font-sans uppercase text-[#7a818c] font-semibold tracking-wider flex items-center justify-between">
            <span>快捷指令</span>
            <span>↑↓ 切换 · ↵ 选择</span>
          </div>
          <div className="p-1.5 max-h-56 overflow-y-auto space-y-1">
            {filteredSlashCommands.map((cmd, idx) => (
              <button
                key={cmd.command}
                id={`slash-cmd-${cmd.command.replace('/', '')}`}
                onClick={() => executeSlashCommand(cmd)}
                className={`w-full flex items-center gap-2.5 px-3 py-2 rounded-xl text-left text-xs transition-colors ${
                  idx === selectedSlashIndex
                    ? 'bg-[#eef2f9] text-[#243550] font-medium border border-[#cbd8eb]'
                    : 'text-[#4b515a] hover:bg-[#f7f5f0]'
                }`}
              >
                <span className="p-1.5 rounded-lg bg-[#f0ede6] text-[#4a5f82]">
                  {cmd.icon}
                </span>
                <div className="min-w-0 flex-1">
                  <div className="font-mono font-semibold text-[#2b3036]">{cmd.label}</div>
                  <div className="text-[11px] text-[#717782] truncate">{cmd.description}</div>
                </div>
              </button>
            ))}
          </div>
        </div>
      )}

      {/* Main Composer Box - TRAE rounded white card */}
      <div className="rounded-2xl border border-[#e2e2e6] bg-white shadow-xs p-3.5 transition-all focus-within:border-[#4f46e5] focus-within:ring-2 focus-within:ring-[#4f46e5]/10">
        <textarea
          ref={textareaRef}
          id="composer-textarea"
          value={inputText}
          onChange={handleChange}
          onKeyDown={handleKeyDown}
          onCompositionStart={() => (composingRef.current = true)}
          onCompositionEnd={() => (composingRef.current = false)}
          rows={2}
          placeholder={isRunning ? "Grodex 正在执行中… 您可以追加指令或点击停止。" : "帮你编写代码、调试 Bug、优化性能等开发工作，交付生产级代码产物。（输入 / 呼出快捷指令）"}
          className="w-full resize-none bg-transparent text-sm text-[#20242c] placeholder-[#9aa0a9] focus:outline-none font-sans leading-relaxed"
        />

        {/* Action Toolbar inside Card */}
        <div className="flex items-center justify-between pt-2.5 mt-1 border-t border-[#f2f2f4]">
          {/* Left Controls: +, 手动审批, Model pill */}
          <div className="flex items-center gap-2">
            <button
              onClick={onOpenDiff}
              className="p-1 rounded-md hover:bg-[#f2f2f5] text-[#555a64] transition-colors"
              title="查看工作区代码差异"
            >
              <FileCode className="w-4 h-4" />
            </button>

            {/* 手动审批 pill */}
            <div className="flex items-center gap-1 px-2.5 py-1 rounded-md bg-[#f6f6f8] hover:bg-[#ececee] text-xs text-[#3a3f47] cursor-pointer transition-colors">
              <ShieldCheck className="w-3.5 h-3.5 text-[#656b77]" />
              <span className="font-medium text-xs">手动审批</span>
              <ChevronDown className="w-3 h-3 text-[#8a909c]" />
            </div>

            {/* Model pill — read-only, reflects the real configured model */}
            <div
              id="composer-model-pill"
              className="flex items-center gap-1.5 px-2.5 py-1 rounded-md bg-[#f6f6f8] border border-[#ebebed] text-xs text-[#6d28d9] font-medium"
              title={`当前模型：${modelName || '未知'}（由 ~/.grodex/config.toml 决定）`}
            >
              <span className="w-2 h-2 rounded-full bg-[#7c3aed]" />
              <span className="truncate max-w-[140px]">{modelName || '模型未配置'}</span>
            </div>
          </div>

          {/* Right Controls: Mode, Mic, Purple Send/Stop button */}
          <div className="flex items-center gap-2">
            {/* Mode Dropdown */}
            <div className="relative">
              <button
                id="composer-mode-selector-btn"
                onClick={() => setIsModeOpen(!isModeOpen)}
                className="flex items-center gap-1 text-xs text-[#525762] hover:text-[#1d2026] cursor-pointer font-medium px-2 py-1 rounded-md hover:bg-[#f2f2f5] transition-colors"
              >
                <span>{selectedMode} Mode</span>
                <ChevronDown className="w-3 h-3 text-[#8c929e]" />
              </button>

              {isModeOpen && (
                <div className="absolute bottom-full right-0 mb-2 w-36 rounded-xl border border-[#e2e2e6] bg-white shadow-xl py-1.5 z-30 text-xs">
                  {(['Auto', 'Plan', 'Build', 'Review'] as const).map((mode) => (
                    <button
                      key={mode}
                      onClick={() => {
                        setSelectedMode(mode);
                        setIsModeOpen(false);
                      }}
                      className={`w-full px-3.5 py-1.5 text-left transition-colors ${
                        selectedMode === mode
                          ? 'bg-[#f5f3ff] text-[#6d28d9] font-semibold'
                          : 'text-[#4b515a] hover:bg-[#f7f7f8]'
                      }`}
                    >
                      {mode === 'Auto' ? '自动 (Auto)' : mode === 'Plan' ? '规划 (Plan)' : mode === 'Build' ? '构建 (Build)' : '审查 (Review)'}
                    </button>
                  ))}
                </div>
              )}
            </div>

            {/* Stop or Send Button (Purple TRAE soundwave / pill style) */}
            {isRunning ? (
              <button
                id="composer-stop-btn"
                onClick={onStop}
                className="px-3 py-1.5 rounded-lg bg-rose-600 hover:bg-rose-700 text-white font-medium text-xs flex items-center gap-1.5 transition-transform active:scale-95 shadow-2xs"
                title="停止当前任务"
              >
                <Square className="w-3 h-3 fill-current" />
                <span>停止</span>
              </button>
            ) : (
              <button
                id="composer-send-btn"
                onClick={handleSubmit}
                disabled={!inputText.trim()}
                className="w-8 h-8 rounded-lg bg-[#4f46e5] hover:bg-[#4338ca] disabled:opacity-40 disabled:hover:bg-[#4f46e5] text-white flex items-center justify-center transition-transform active:scale-95 shadow-2xs"
                title="发送消息"
              >
                <div className="flex items-center gap-0.5">
                  <span className="w-0.5 h-2.5 bg-white rounded-full animate-pulse" />
                  <span className="w-0.5 h-4 bg-white rounded-full" />
                  <span className="w-0.5 h-2.5 bg-white rounded-full animate-pulse" />
                </div>
              </button>
            )}
          </div>
        </div>
      </div>

      {/* Directory and status indicators below card */}
      <div className="flex items-center justify-between px-1.5 mt-2 text-xs text-[#707682]">
        <div className="flex items-center gap-3">
          <div className="flex items-center gap-1 cursor-pointer hover:text-[#20242c]">
            <span>本地</span>
            <ChevronDown className="w-3 h-3 text-[#9aa0a9]" />
          </div>
          <span>•</span>
          <div className="flex items-center gap-1 cursor-pointer hover:text-[#20242c] font-mono">
            <span>grodex</span>
            <ChevronDown className="w-3 h-3 text-[#9aa0a9]" />
          </div>
        </div>

        <div className="flex items-center gap-3 text-[11px] font-mono">
          <span>{tokensUsed.toLocaleString()} 令牌</span>
          <span>•</span>
          <span>~${costEstimate.toFixed(3)}</span>
        </div>
      </div>
    </div>
  );
};
