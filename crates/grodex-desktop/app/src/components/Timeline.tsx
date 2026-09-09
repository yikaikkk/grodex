import React, { useEffect, useRef, useState } from 'react';
import ReactMarkdown from 'react-markdown';
import { Bot, Copy, Check, Clock, Cpu, Brain, ChevronDown, ChevronRight, Wrench } from 'lucide-react';
import { TimelineItem, ToolItem } from '../types';
import { ToolCard } from './ToolCard';

interface TimelineProps {
  items: TimelineItem[];
  onOpenDiff: (diffId: string) => void;
  /** Opaque value bumped each time the timeline is rebuilt from a snapshot for
   * the active session — on change, jump to the bottom (opening a session
   * must not require the user to scroll down). */
  scrollToKey?: number;
}

type Row =
  | { kind: 'single'; item: TimelineItem }
  | { kind: 'frame'; items: TimelineItem[] };

/** One USER message = exactly one thinking frame (all reasoning bursts +
 * interleaved tool calls, in arrival order) followed by ONE merged model
 * output bubble. Segmenting by user guarantees one frame per turn. */
function groupRows(items: TimelineItem[]): Row[] {
  const rows: Row[] = [];
  let seg: TimelineItem[] = [];

  const flush = () => {
    if (seg.length === 0) return;
    const frameItems = seg.filter((i) => i.type === 'thinking' || i.type === 'tool');
    const asstItems = seg.filter((i) => i.type === 'assistant');
    if (frameItems.length > 0) {
      rows.push({ kind: 'frame', items: frameItems });
    }
    if (asstItems.length > 0) {
      const first = asstItems[0] as Extract<TimelineItem, { type: 'assistant' }>;
      rows.push({
        kind: 'single',
        item: {
          id: first.id,
          type: 'assistant',
          content: (asstItems as Extract<TimelineItem, { type: 'assistant' }>[])
            .map((a) => a.content)
            .join('\n\n'),
          isStreaming: false,
          timestamp: first.timestamp,
        } as TimelineItem,
      });
    }
    seg = [];
  };

  for (const it of items) {
    if (it.type === 'user') {
      flush();
      rows.push({ kind: 'single', item: it });
    } else {
      seg.push(it);
    }
  }
  flush();
  return rows;
}

function FrameBody({
  items,
  expanded,
  onOpenDiff,
}: {
  items: TimelineItem[];
  expanded: boolean;
  onOpenDiff: (diffId: string) => void;
}) {
  const miniRef = useRef<HTMLDivElement>(null);
  const stuckRef = useRef(false); // true = user scrolled up inside the mini box

  // Inner smart-follow: auto-stick to the bottom as content (thinking text or
  // tool cards) streams in. Only paused while the user has scrolled UP; going
  // back to the bottom re-engages following.
  useEffect(() => {
    if (expanded) return;
    const el = miniRef.current;
    if (!el) return;
    if (stuckRef.current) return;
    el.scrollTop = el.scrollHeight;
  }, [items, expanded]);

  const onMiniScroll = () => {
    const el = miniRef.current;
    if (!el) return;
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 60;
    stuckRef.current = !nearBottom;
  };

  const content = (
    <>
      {items.map((it) => {
        if (it.type === 'thinking') {
          return (
            <div
              key={it.id}
              className="whitespace-pre-wrap leading-6 select-text pl-1 text-xs"
            >
              {it.content}
              {it.isStreaming && (
                <span className="inline-block w-1.5 h-3.5 ml-1 bg-accent animate-cursor-blink align-middle rounded-full" />
              )}
            </div>
          );
        }
        return <ToolCard key={it.id} item={it as ToolItem} onOpenDiff={onOpenDiff} compact />;
      })}
    </>
  );

  if (expanded) {
    // Expanded = large frame, show everything (no internal scroll).
    return <div className="space-y-2.5">{content}</div>;
  }
  // Collapsed = short fixed-height frame that scrolls its streamed content.
  return (
    <div
      ref={miniRef}
      onScroll={onMiniScroll}
      className="max-h-40 overflow-y-auto px-3.5 py-2.5 space-y-2.5"
    >
      {content}
    </div>
  );
}

/** A bordered "thinking" frame. Collapsed by default: a short box whose
 * streamed content scrolls inside (and only auto-follows when at the bottom).
 * Clicking the header expands it into a full-size frame. */
function ThinkingFrame({
  items,
  onOpenDiff,
}: {
  items: TimelineItem[];
  onOpenDiff: (diffId: string) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const toolCount = items.filter((i) => i.type === 'tool').length;

  return (
    <div className="rounded-2xl border border-hairline bg-card overflow-hidden shadow-xs">
      <button
        onClick={() => setExpanded(!expanded)}
        className="w-full flex items-center gap-2 px-3.5 py-2 border-b border-hairline bg-well text-left text-xs select-none"
        title={expanded ? '收起为小框' : '展开为大框'}
      >
        <span className="w-4 h-4 rounded-full bg-well flex items-center justify-center text-accent shrink-0">
          <Brain className="w-3 h-3" />
        </span>
        <span className="font-medium text-primary shrink-0">思考过程</span>
        <span className="flex items-center gap-1 px-1.5 py-0.5 rounded-full bg-well text-[10px] text-secondary font-mono shrink-0">
          <Wrench className="w-2.5 h-2.5" />
          {toolCount}
        </span>
        {expanded ? (
          <span className="ml-auto text-[10px] text-tertiary">收起为小框</span>
        ) : (
          <span className="ml-auto text-[10px] text-tertiary">展开为大框</span>
        )}
        <span className="text-tertiary shrink-0">
          {expanded ? <ChevronDown className="w-3.5 h-3.5" /> : <ChevronRight className="w-3.5 h-3.5" />}
        </span>
      </button>
      <FrameBody items={items} expanded={expanded} onOpenDiff={onOpenDiff} />
    </div>
  );
}

export const Timeline: React.FC<TimelineProps> = ({ items, onOpenDiff, scrollToKey }) => {
  const containerRef = useRef<HTMLDivElement>(null);
  const pageStuckRef = useRef(false); // true = user scrolled UP (pauses follow)
  const [copiedId, setCopiedId] = useState<string | null>(null);

  // A session's history was (re)loaded: reset any "scrolled up" pause and jump
  // straight to the newest content. Uses instant scroll (no long animation).
  useEffect(() => {
    const el = containerRef.current;
    if (!el || items.length === 0) return;
    pageStuckRef.current = false;
    el.scrollTop = el.scrollHeight;
  }, [scrollToKey]);

  // Page smart-follow:
  //  - while anything is live (thinking streaming / tool running / assistant
  //    streaming) keep sticking to the bottom as new output arrives;
  //  - for static changes, follow only when the user is already near bottom;
  //  - if the user scrolled up, pause until they return to the bottom.
  const isLive =
    items.some(
      (it) =>
        (it.type === 'thinking' && it.isStreaming) ||
        (it.type === 'tool' &&
          (it.status === 'running' || it.status === 'awaiting_approval')) ||
        (it.type === 'assistant' && it.isStreaming)
    );

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    if (pageStuckRef.current && !isLive) return;
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 160;
    if (nearBottom || isLive) {
      el.scrollTo({ top: el.scrollHeight, behavior: 'smooth' });
    }
  }, [items, isLive]);

  const onPageScroll = () => {
    const el = containerRef.current;
    if (!el) return;
    const nearBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 160;
    pageStuckRef.current = !nearBottom;
  };

  const handleCopyText = (id: string, text: string) => {
    navigator.clipboard.writeText(text);
    setCopiedId(id);
    setTimeout(() => setCopiedId(null), 2000);
  };

  const renderStandalone = (item: TimelineItem) => {
    if (item.type === 'user') {
      return (
        <div
          key={item.id}
          id={`timeline-user-msg-${item.id}`}
          className="flex justify-end my-3 pl-12 sm:pl-24"
        >
          <div className="flex flex-col items-end max-w-[85%] sm:max-w-[75%]">
            <div className="relative rounded-2xl rounded-br-md bg-accent text-white px-4 py-2.5 text-sm leading-relaxed shadow-sm break-words select-text">
              <p className="whitespace-pre-wrap font-sans text-sm bubble-wrap">
                {item.content}
              </p>
            </div>
            {item.timestamp && (
              <span className="text-[10px] text-tertiary mt-1 pr-1 font-sans">
                {item.timestamp}
              </span>
            )}
          </div>
        </div>
      );
    }

    if (item.type !== 'assistant') return null;
    return (
      <div
        key={item.id}
        id={`timeline-assistant-msg-${item.id}`}
        className="flex justify-start items-start gap-2.5 my-3 w-full pr-2 sm:pr-6"
      >
        <div className="w-8 h-8 rounded-lg bg-well border border-hairline flex items-center justify-center text-accent shrink-0 mt-0.5 shadow-2xs select-none">
          <Bot className="w-4 h-4" />
        </div>
        <div className="flex flex-col items-start flex-1 min-w-0">
          <div className="w-full relative rounded-2xl bg-white border border-hairline p-4.5 text-primary shadow-2xs text-sm leading-relaxed break-words">
            <div className="md-body prose prose-stone prose-sm max-w-none text-primary prose-headings:text-primary prose-headings:font-bold prose-p:leading-relaxed prose-pre:bg-well prose-pre:border prose-pre:border-hairline prose-pre:rounded-xl prose-pre:w-full prose-code:font-mono prose-code:text-primary prose-code:bg-well prose-code:px-1.5 prose-code:py-0.5 prose-code:rounded-md prose-code:border prose-code:border-hairline prose-strong:text-primary">
              <ReactMarkdown>{item.content}</ReactMarkdown>
            </div>
            <div className="flex items-center justify-between mt-3.5 pt-2.5 border-t border-hairline-2 text-[11px] font-sans text-secondary gap-4">
              <div className="flex items-center gap-3">
                <span className="flex items-center gap-1 font-sans">
                  <Cpu className="w-3 h-3 text-tertiary" />
                  {item.tokens ?? item.content.length} 令牌
                </span>
              </div>
              <button
                id={`copy-assistant-msg-${item.id}`}
                onClick={() => handleCopyText(item.id, item.content)}
                className="flex items-center gap-1 px-2 py-0.5 rounded-md hover:bg-well text-secondary hover:text-primary transition-colors"
                title="复制回复内容"
              >
                {copiedId === item.id ? (
                  <Check className="w-3 h-3 text-green-dark" />
                ) : (
                  <Copy className="w-3 h-3" />
                )}
                <span className="text-[10px]">{copiedId === item.id ? '已复制' : '复制'}</span>
              </button>
            </div>
          </div>
          {item.timestamp && (
            <span className="text-[10px] text-tertiary mt-1 pl-1 font-sans">
              {item.timestamp}
            </span>
          )}
        </div>
      </div>
    );
  };

  const rows = groupRows(items);

  return (
    <div
      ref={containerRef}
      onScroll={onPageScroll}
      id="session-timeline-container"
      className="flex-1 overflow-y-auto px-4 sm:px-6 py-5 w-full space-y-4 font-sans"
    >
      {rows.map((row) =>
        row.kind === 'frame' ? (
          <ThinkingFrame
            key={`frame-${row.items[0]?.id ?? 'root'}`}
            items={row.items}
            onOpenDiff={onOpenDiff}
          />
        ) : (
          renderStandalone(row.item)
        )
      )}
    </div>
  );
};
