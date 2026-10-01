import React, { useEffect, useMemo, useRef, useState } from 'react';
import { X, FileText, Loader2 } from 'lucide-react';
import * as acp from '../lib/acpClient';

export interface FilePreviewTarget {
  path: string;
  line?: number;
  column?: number;
}

interface FilePreviewProps {
  isOpen: boolean;
  onClose: () => void;
  /** Workspace the file is resolved against. */
  workspace?: string;
  target: FilePreviewTarget | null;
}

const MAX_RENDER_LINES = 5000;

/**
 * Read-only file viewer backed by the `preview_file` Tauri command (the
 * backend enforces workspace containment, size cap and UTF-8). Layout is
 * width-stable: fixed-width sticky line-number gutter, horizontally
 * scrollable code area, truncated path in the header.
 */
const FilePreviewInner: React.FC<FilePreviewProps> = ({
  isOpen,
  onClose,
  workspace,
  target,
}) => {
  const [content, setContent] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const scrollRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!isOpen || !target?.path || !workspace) return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    setContent(null);
    acp
      .previewFile(workspace, target.path)
      .then((c) => {
        if (!cancelled) setContent(c);
      })
      .catch((e) => {
        if (!cancelled) setError(e?.message || String(e));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [isOpen, workspace, target?.path]);

  const lines = useMemo(() => {
    if (content === null) return [];
    return content.split('\n').slice(0, MAX_RENDER_LINES);
  }, [content]);

  // Scroll the requested line into view once content is rendered.
  useEffect(() => {
    if (content === null || !target?.line) return;
    const el = scrollRef.current?.querySelector(`[data-line="${target.line}"]`);
    el?.scrollIntoView({ block: 'center' });
  }, [content, target?.line]);

  if (!isOpen) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/50">
      <div className="w-full max-w-4xl h-[85vh] rounded-2xl bg-canvas border border-hairline shadow-2xl flex flex-col overflow-hidden">
        {/* Header */}
        <div className="px-5 py-3.5 bg-white border-b border-hairline flex items-center justify-between gap-3">
          <div className="flex items-center gap-2.5 min-w-0 flex-1">
            <div className="p-1.5 rounded-lg bg-accent-soft text-accent shrink-0">
              <FileText className="w-4 h-4" />
            </div>
            <div className="min-w-0">
              <h3 className="text-sm font-bold text-primary truncate">
                {target?.path.split('/').pop() || '文件预览'}
              </h3>
              <p className="text-[11px] text-secondary font-mono truncate" title={target?.path}>
                {target?.path || ''}
                {target?.line ? `:${target.line}` : ''}
              </p>
            </div>
          </div>
          <button
            id="close-file-preview-btn"
            onClick={onClose}
            className="p-1.5 rounded-full hover:bg-well text-secondary hover:text-primary transition-colors shrink-0"
          >
            <X className="w-5 h-5" />
          </button>
        </div>

        {/* Body */}
        <div ref={scrollRef} className="flex-1 overflow-auto bg-white [scrollbar-gutter:stable]">
          {loading && (
            <div className="px-5 py-8 text-secondary text-xs flex items-center gap-2">
              <Loader2 className="w-3.5 h-3.5 animate-spin" />
              正在加载文件…
            </div>
          )}
          {error && (
            <div className="px-5 py-8 text-red-dark text-xs">无法预览：{error}</div>
          )}
          {!loading && !error && content !== null && lines.length === 0 && (
            <div className="px-5 py-8 text-secondary text-xs">空文件</div>
          )}
          {content !== null && lines.length > 0 && (
            <div className="font-mono text-[11px] leading-5 py-2 min-w-max">
              {lines.map((line, idx) => {
                const lineNo = idx + 1;
                const isTarget = target?.line === lineNo;
                return (
                  <div
                    key={idx}
                    data-line={lineNo}
                    className={`flex ${isTarget ? 'bg-accent-soft/60' : ''}`}
                  >
                    <span
                      className={`sticky left-0 z-10 w-14 shrink-0 text-right pr-2 select-none border-r border-hairline-2/60 ${
                        isTarget ? 'text-accent font-semibold bg-white' : 'text-tertiary bg-white'
                      }`}
                    >
                      {lineNo}
                    </span>
                    <pre className="px-3 whitespace-pre pr-8 text-primary">
                      {line || ' '}
                    </pre>
                  </div>
                );
              })}
            </div>
          )}
        </div>
      </div>
    </div>
  );
};

/** Memoized: open modal must not re-render on streaming frame flushes. */
export const FilePreview = React.memo(FilePreviewInner);
