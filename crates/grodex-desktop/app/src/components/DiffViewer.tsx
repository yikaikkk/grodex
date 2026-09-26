import React, { useEffect, useState, useMemo } from 'react';
import { X, FileCode, Columns2, AlignLeft } from 'lucide-react';
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

const CHANGE_LABEL: Record<string, string> = {
  created: '新增',
  updated: '修改',
  deleted: '删除',
  moved: '移动',
};

// ─── LCS-based line diff ───────────────────────────────────────────

type DiffLineType = 'equal' | 'add' | 'remove';

interface DiffLine {
  type: DiffLineType;
  oldLine: number | null; // 1-based, null for added lines
  newLine: number | null; // 1-based, null for removed lines
  content: string;
}

/** Safeguard against OOM on very large files. */
const MAX_DIFF_LINES = 3000;

function computeLineDiff(before: string, after: string): DiffLine[] {
  const beforeLines = before.length > 0 ? before.split('\n') : [];
  const afterLines = after.length > 0 ? after.split('\n') : [];

  if (beforeLines.length > MAX_DIFF_LINES || afterLines.length > MAX_DIFF_LINES) {
    // Fallback: show after content as all-added
    return afterLines.slice(0, MAX_DIFF_LINES).map((line, i) => ({
      type: 'add' as const,
      oldLine: null,
      newLine: i + 1,
      content: line,
    }));
  }

  const m = beforeLines.length;
  const n = afterLines.length;

  // Build LCS DP table (O(m*n) space — acceptable for files under 3000 lines)
  const dp: Uint32Array[] = new Array(m + 1);
  for (let i = 0; i <= m; i++) {
    dp[i] = new Uint32Array(n + 1);
  }
  for (let i = 1; i <= m; i++) {
    for (let j = 1; j <= n; j++) {
      if (beforeLines[i - 1] === afterLines[j - 1]) {
        dp[i][j] = dp[i - 1][j - 1] + 1;
      } else {
        dp[i][j] = Math.max(dp[i - 1][j], dp[i][j - 1]);
      }
    }
  }

  // Backtrack to produce the diff op sequence
  const result: DiffLine[] = [];
  let i = m;
  let j = n;
  while (i > 0 || j > 0) {
    if (i > 0 && j > 0 && beforeLines[i - 1] === afterLines[j - 1]) {
      result.unshift({
        type: 'equal',
        oldLine: i,
        newLine: j,
        content: beforeLines[i - 1],
      });
      i--;
      j--;
    } else if (j > 0 && (i === 0 || dp[i][j - 1] >= dp[i - 1][j])) {
      result.unshift({
        type: 'add',
        oldLine: null,
        newLine: j,
        content: afterLines[j - 1],
      });
      j--;
    } else {
      result.unshift({
        type: 'remove',
        oldLine: i,
        newLine: null,
        content: beforeLines[i - 1],
      });
      i--;
    }
  }
  return result;
}

// ─── Diff row rendering helpers ────────────────────────────────────

function lineBgClass(type: DiffLineType): string {
  switch (type) {
    case 'add':
      return 'bg-green-soft/40';
    case 'remove':
      return 'bg-red-soft/40';
    default:
      return '';
  }
}

function lineTextClass(type: DiffLineType): string {
  switch (type) {
    case 'add':
      return 'text-green-dark';
    case 'remove':
      return 'text-red-dark';
    default:
      return 'text-primary';
  }
}

function gutterSign(type: DiffLineType): string {
  switch (type) {
    case 'add':
      return '+';
    case 'remove':
      return '-';
    default:
      return ' ';
  }
}

// ─── Component ─────────────────────────────────────────────────────

export const DiffViewer: React.FC<DiffViewerProps> = ({ isOpen, onClose, diffId }) => {
  const [payload, setPayload] = useState<acp.DiffPayload | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [splitView, setSplitView] = useState(true);
  const [activeFileIdx, setActiveFileIdx] = useState(0);

  useEffect(() => {
    if (!isOpen || !diffId) return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    setPayload(null);
    setActiveFileIdx(0);
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

  const files = payload?.files ?? [];
  const currentFile = files[activeFileIdx];
  const isPartial = payload?.completeness === 'Partial';

  // Compute diff lines for the current file (memoised per file).
  // MUST be called before any early return to satisfy React's rules of hooks.
  const diffLines = useMemo<DiffLine[]>(() => {
    if (!currentFile) return [];
    const before = currentFile.before_content ?? '';
    const after = currentFile.after_content ?? '';

    if (currentFile.change_type === 'created') {
      // Pure add: all lines are additions
      return (after.length ? after.split('\n') : []).map((line, i) => ({
        type: 'add' as const,
        oldLine: null,
        newLine: i + 1,
        content: line,
      }));
    }
    if (currentFile.change_type === 'deleted') {
      // Pure remove: all lines are deletions
      return (before.length ? before.split('\n') : []).map((line, i) => ({
        type: 'remove' as const,
        oldLine: i + 1,
        newLine: null,
        content: line,
      }));
    }
    // updated / moved → real diff
    if (before === '' && after === '') return [];
    return computeLineDiff(before, after);
  }, [currentFile]);

  if (!isOpen) return null;

  // Stats for header
  const additions = diffLines.filter((l) => l.type === 'add').length;
  const deletions = diffLines.filter((l) => l.type === 'remove').length;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/30 backdrop-blur-sm">
      <div className="w-full max-w-6xl h-[88vh] rounded-2xl bg-canvas border border-hairline shadow-2xl flex flex-col overflow-hidden">
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

          {/* View toggle */}
          <div className="flex items-center gap-1.5 mr-2">
            <button
              onClick={() => setSplitView(true)}
              className={`px-2.5 py-1 rounded-lg text-[11px] font-medium flex items-center gap-1 transition-colors ${
                splitView
                  ? 'bg-accent-soft text-accent border border-accent-soft'
                  : 'text-secondary hover:text-primary border border-transparent'
              }`}
            >
              <Columns2 className="w-3.5 h-3.5" />
              双栏
            </button>
            <button
              onClick={() => setSplitView(false)}
              className={`px-2.5 py-1 rounded-lg text-[11px] font-medium flex items-center gap-1 transition-colors ${
                !splitView
                  ? 'bg-accent-soft text-accent border border-accent-soft'
                  : 'text-secondary hover:text-primary border border-transparent'
              }`}
            >
              <AlignLeft className="w-3.5 h-3.5" />
              统一
            </button>
          </div>

          <button
            onClick={onClose}
            className="p-1.5 rounded-full hover:bg-well text-secondary hover:text-primary transition-colors"
          >
            <X className="w-5 h-5" />
          </button>
        </div>

        {/* Completeness banner — 第三十七轮 diff 可信度 */}
        {isPartial && (
          <div className="px-4 py-2 bg-amber-50 border-b border-amber-200 text-[11px] text-amber-800 flex items-center gap-2">
            <span>⚠️ 本轮包含非精确工具（如 exec），已知变更如下，但可能存在未观测到的文件修改。</span>
          </div>
        )}
        {(payload?.warnings?.length ?? 0) > 0 &&
          payload!.warnings
            .filter((w) => !isPartial || !w.startsWith('An inexact tool'))
            .map((w, i) => (
              <div
                key={i}
                className="px-4 py-2 bg-well border-b border-hairline text-[11px] text-secondary"
              >
                {w}
              </div>
            ))}

        {/* File tabs */}
        {files.length > 0 && (
          <div className="px-4 py-2 bg-well border-b border-hairline flex items-center gap-1 overflow-x-auto">
            {files.map((f, i) => {
              const isActive = i === activeFileIdx;
              return (
                <button
                  key={i}
                  onClick={() => setActiveFileIdx(i)}
                  className={`px-3 py-1 rounded-lg text-[11px] font-mono whitespace-nowrap transition-colors ${
                    isActive
                      ? 'bg-white text-primary border border-hairline shadow-xs'
                      : 'text-secondary hover:text-primary border border-transparent'
                  }`}
                >
                  {f.path.split('/').pop()}
                  <span
                    className={`ml-1.5 text-[9px] ${
                      CHANGE_COLOR[f.change_type] ?? 'text-secondary'
                    }`}
                  >
                    {CHANGE_LABEL[f.change_type] ?? f.change_type}
                  </span>
                </button>
              );
            })}
          </div>
        )}

        {/* Body */}
        <div className="flex-1 overflow-y-auto bg-white">
          {loading && (
            <div className="px-5 py-8 text-secondary text-xs">正在加载 diff…</div>
          )}
          {error && (
            <div className="px-5 py-8 text-red-dark text-xs">加载失败：{error}</div>
          )}
          {!loading && !error && files.length === 0 && (
            <div className="px-5 py-8 text-secondary text-xs">无文件变更</div>
          )}

          {/* Current file header */}
          {currentFile && (
            <div className="px-5 py-2.5 border-b border-hairline flex items-center justify-between sticky top-0 bg-white z-10">
              <span className="font-mono text-[11px] text-primary truncate">
                {currentFile.path}
              </span>
              <div className="flex items-center gap-3 text-[10px] shrink-0">
                <span className="text-green-dark font-medium">+{additions}</span>
                <span className="text-red-dark font-medium">-{deletions}</span>
              </div>
            </div>
          )}

          {/* Diff content */}
          {currentFile && currentFile.change_type === 'moved' && (
            <div className="px-5 py-4 text-secondary text-xs">
              文件移动/重命名
            </div>
          )}

          {currentFile && currentFile.change_type !== 'moved' && splitView && (
            /* ─── Split / side-by-side view ─── */
            <div className="font-mono text-[11px]">
              {diffLines.map((line, idx) => (
                <div
                  key={idx}
                  className={`flex ${lineBgClass(line.type)} border-b border-hairline-2/40`}
                >
                  {/* Old side */}
                  <div className="w-1/2 flex items-start">
                    <span className="w-12 shrink-0 text-right pr-2 text-tertiary select-none border-r border-hairline-2/60 bg-well/30">
                      {line.oldLine ?? ''}
                    </span>
                    <span className="w-4 shrink-0 text-center select-none text-tertiary">
                      {line.type === 'remove' ? '-' : line.type === 'equal' ? ' ' : ''}
                    </span>
                    <pre
                      className={`flex-1 px-2 whitespace-pre-wrap break-all ${lineTextClass(line.type)}`}
                    >
                      {line.type === 'add' ? '' : line.content || ' '}
                    </pre>
                  </div>
                  {/* New side */}
                  <div className="w-1/2 flex items-start">
                    <span className="w-12 shrink-0 text-right pr-2 text-tertiary select-none border-l border-r border-hairline-2/60 bg-well/30">
                      {line.newLine ?? ''}
                    </span>
                    <span className="w-4 shrink-0 text-center select-none text-tertiary">
                      {line.type === 'add' ? '+' : line.type === 'equal' ? ' ' : ''}
                    </span>
                    <pre
                      className={`flex-1 px-2 whitespace-pre-wrap break-all ${lineTextClass(line.type)}`}
                    >
                      {line.type === 'remove' ? '' : line.content || ' '}
                    </pre>
                  </div>
                </div>
              ))}
            </div>
          )}

          {currentFile && currentFile.change_type !== 'moved' && !splitView && (
            /* ─── Unified view ─── */
            <div className="font-mono text-[11px]">
              {diffLines.map((line, idx) => (
                <div
                  key={idx}
                  className={`flex ${lineBgClass(line.type)}`}
                >
                  <span className="w-12 shrink-0 text-right pr-2 text-tertiary select-none border-r border-hairline-2/60 bg-well/30">
                    {line.oldLine ?? ''}
                  </span>
                  <span className="w-12 shrink-0 text-right pr-2 text-tertiary select-none border-r border-hairline-2/60 bg-well/30">
                    {line.newLine ?? ''}
                  </span>
                  <span className="w-4 shrink-0 text-center select-none text-tertiary">
                    {gutterSign(line.type)}
                  </span>
                  <pre
                    className={`flex-1 px-2 whitespace-pre-wrap break-all ${lineTextClass(line.type)}`}
                  >
                    {line.content || ' '}
                  </pre>
                </div>
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  );
};
