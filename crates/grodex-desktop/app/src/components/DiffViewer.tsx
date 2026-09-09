import React, { useEffect, useState } from 'react';
import { X, FileCode } from 'lucide-react';
import * as acp from '../lib/acpClient';

interface DiffViewerProps {
  isOpen: boolean;
  onClose: () => void;
  diffId: string | null;
}

const CHANGE_COLOR: Record<string, string> = {
  created: 'text-green-dark',
  updated: 'text-secondary',
  deleted: 'text-red-dark',
  moved: 'text-accent',
};

export const DiffViewer: React.FC<DiffViewerProps> = ({ isOpen, onClose, diffId }) => {
  const [payload, setPayload] = useState<acp.DiffPayload | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!isOpen || !diffId) return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    setPayload(null);
    acp
      .getDiff(diffId)
      .then((p) => {
        if (!cancelled) setPayload(p);
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
  }, [isOpen, diffId]);

  if (!isOpen) return null;

  const files = payload?.files ?? [];

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/30 backdrop-blur-sm">
      <div className="w-full max-w-4xl h-[88vh] rounded-2xl bg-canvas border border-hairline shadow-2xl flex flex-col overflow-hidden">
        {/* Header */}
        <div className="px-5 py-3.5 bg-white border-b border-hairline flex items-center justify-between">
          <div className="flex items-center gap-2.5 min-w-0">
            <div className="p-1.5 rounded-lg bg-accent-soft text-accent shrink-0">
              <FileCode className="w-4 h-4" />
            </div>
            <div className="min-w-0">
              <h3 className="text-sm font-bold text-primary">工作区变更</h3>
              <p className="text-[11px] text-secondary">
                {payload
                  ? `${files.length} 个文件 · ${payload.diff_id.slice(0, 12)}…`
                  : '加载中…'}
              </p>
            </div>
          </div>
          <button
            onClick={onClose}
            className="p-1.5 rounded-full hover:bg-well text-secondary hover:text-primary transition-colors"
          >
            <X className="w-5 h-5" />
          </button>
        </div>

        {/* Body */}
        <div className="flex-1 overflow-y-auto p-5 space-y-3 text-xs">
          {loading && <div className="text-secondary">正在加载 diff…</div>}
          {error && <div className="text-red-dark">加载失败：{error}</div>}
          {!loading && !error && files.length === 0 && (
            <div className="text-secondary">无文件变更</div>
          )}

          {files.map((f, i) => (
            <div key={i} className="rounded-xl bg-white border border-hairline overflow-hidden">
              <div className="px-3 py-2 bg-well border-b border-hairline flex items-center justify-between gap-2">
                <span className="font-mono text-[11px] text-primary truncate">{f.path}</span>
                <span className={`text-[10px] font-medium shrink-0 ${CHANGE_COLOR[f.change_type] ?? 'text-secondary'}`}>
                  {f.change_type}
                </span>
              </div>

              {(f.change_type === 'created' || f.change_type === 'updated') &&
                f.after_content != null && (
                  <pre className="px-3 py-2 text-[11px] font-mono text-primary overflow-x-auto whitespace-pre-wrap max-h-64">
                    {f.after_content}
                  </pre>
                )}

              {f.change_type === 'deleted' && f.before_content != null && (
                <pre className="px-3 py-2 text-[11px] font-mono text-tertiary overflow-x-auto whitespace-pre-wrap max-h-64 line-through">
                  {f.before_content}
                </pre>
              )}

              {f.change_type === 'moved' && (
                <div className="px-3 py-2 text-secondary">文件移动/重命名</div>
              )}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
};
