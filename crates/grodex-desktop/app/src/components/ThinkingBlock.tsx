import React, { useEffect, useState } from 'react';
import { Brain, ChevronDown, ChevronRight, Sparkles, Wrench } from 'lucide-react';
import { ThinkingItem } from '../types';

interface ThinkingBlockProps {
  item: ThinkingItem;
  /** Optional tool calls nested inside this thinking block. */
  children?: React.ReactNode;
}

export const ThinkingBlock: React.FC<ThinkingBlockProps> = ({ item, children }) => {
  // Expand while streaming, auto-collapse once the thought finishes.
  const [isExpanded, setIsExpanded] = useState<boolean>(item.isStreaming === true);

  useEffect(() => {
    setIsExpanded(item.isStreaming === true);
  }, [item.isStreaming]);

  const hasTools = children != null && React.Children.count(children) > 0;

  const preview = item.content
    .split('\n')
    .map((l) => l.trim())
    .find((l) => l.length > 0) ?? '';

  return (
    <div id={`thinking-block-${item.id}`} className="my-2">
      {!isExpanded ? (
        /* Collapsed pill capsule with a short content preview */
        <button
          id={`toggle-thinking-${item.id}`}
          onClick={() => setIsExpanded(true)}
          className="inline-flex max-w-full items-center gap-2 px-3 py-1 rounded-full bg-well hover:bg-black/[0.05] text-secondary border border-hairline text-xs font-medium transition-all shadow-xs group"
          title={item.content ? `思考：${item.content.slice(0, 200)}（点击展开全文）` : '点击展开查看思考过程与工具调用'}
        >
          <div className="w-4 h-4 rounded-full bg-well flex items-center justify-center text-accent group-hover:text-accent-hover shrink-0">
            {item.isStreaming ? (
              <Sparkles className="w-2.5 h-2.5 animate-spin text-orange-dark" />
            ) : (
              <Brain className="w-2.5 h-2.5" />
            )}
          </div>
          <span className="shrink-0">{item.isStreaming ? '正在思考…' : '思考过程'}</span>
          {preview && (
            <span className="truncate max-w-[30ch] text-secondary normal-case">
              {preview}
            </span>
          )}
          {hasTools && (
            <span className="flex shrink-0 items-center gap-1 px-1.5 py-0.5 rounded-full bg-well text-[10px] text-secondary font-mono">
              <Wrench className="w-2.5 h-2.5" />
              {React.Children.count(children)}
            </span>
          )}
          {item.durationSec !== undefined && (
            <span className="shrink-0 text-[11px] text-secondary font-mono">
              {item.durationSec.toFixed(1)}s
            </span>
          )}
          <ChevronRight className="w-3 h-3 text-tertiary group-hover:translate-x-0.5 transition-transform shrink-0" />
        </button>
      ) : (
        /* Expanded thinking card (reasoning + nested tool calls) */
        <div className="rounded-2xl border border-hairline bg-card overflow-hidden shadow-xs transition-all">
          <div className="flex items-center justify-between px-3.5 py-2 border-b border-hairline bg-well">
            <button
              id={`toggle-thinking-${item.id}`}
              onClick={() => setIsExpanded(false)}
              className="flex items-center gap-2 text-xs font-medium text-primary hover:text-primary transition-colors"
            >
              <div className="w-4 h-4 rounded-full bg-well flex items-center justify-center text-accent">
                {item.isStreaming ? (
                  <Sparkles className="w-2.5 h-2.5 animate-spin text-orange-dark" />
                ) : (
                  <Brain className="w-2.5 h-2.5" />
                )}
              </div>
              <span>{item.isStreaming ? '正在思考…' : '思考过程'}</span>
              {hasTools && (
                <span className="flex items-center gap-1 px-1.5 py-0.5 rounded-full bg-well text-[10px] text-secondary font-mono">
                  <Wrench className="w-2.5 h-2.5" />
                  {React.Children.count(children)} 个工具
                </span>
              )}
              {item.durationSec !== undefined && (
                <span className="text-[10px] text-secondary font-mono">
                  ({item.durationSec.toFixed(1)}s)
                </span>
              )}
            </button>

            <button
              onClick={() => setIsExpanded(false)}
              className="flex items-center gap-1 text-[11px] text-secondary hover:text-secondary px-2 py-0.5 rounded-full hover:bg-black/[0.05] transition-colors"
            >
              <span>收起</span>
              <ChevronDown className="w-3 h-3" />
            </button>
          </div>

          <div className="px-4 py-3 text-xs font-sans text-primary leading-relaxed">
            <div className="whitespace-pre-wrap leading-6 select-text">
              {item.content}
              {item.isStreaming && (
                <span className="inline-block w-1.5 h-3.5 ml-1 bg-accent animate-cursor-blink align-middle rounded-full" />
              )}
            </div>

            {/* Nested tool calls */}
            {hasTools && (
              <div className="mt-3 space-y-2">{children}</div>
            )}

            <div className="pt-2 mt-2 text-[10px] text-tertiary flex items-center justify-between border-t border-hairline">
              <span>独立深度推理流</span>
              <span className="font-mono">acp.reasoning.v1</span>
            </div>
          </div>
        </div>
      )}
    </div>
  );
};
