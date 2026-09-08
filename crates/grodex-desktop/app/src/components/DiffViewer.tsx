import React, { useState, useEffect } from 'react';
import {
  FileCode,
  X,
  Columns,
  List,
  GitCommit,
  Layers,
  Copy,
  Check,
} from 'lucide-react';
import { DiffFile } from '../types';

// Structured per-file diffs are not yet produced by the real agent protocol
// (v1 milestone). When grodex emits diff objects this component should be fed
// them via props instead of a hard-coded mock. Kept dead + empty on purpose.
const MOCK_DIFF_FILES: DiffFile[] = [];

interface DiffViewerProps {
  isOpen: boolean;
  onClose: () => void;
  selectedFileId?: string;
}

export const DiffViewer: React.FC<DiffViewerProps> = ({
  isOpen,
  onClose,
  selectedFileId,
}) => {
  const [activeFileId, setActiveFileId] = useState<string>(
    selectedFileId || MOCK_DIFF_FILES[0]?.id || ''
  );
  const [viewMode, setViewMode] = useState<'split' | 'unified'>('split');
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (selectedFileId) {
      setActiveFileId(selectedFileId);
    }
  }, [selectedFileId]);

  if (!isOpen) return null;

  if (MOCK_DIFF_FILES.length === 0) {
    return (
      <div id="diff-viewer-modal-backdrop" className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/30 backdrop-blur-sm">
        <div className="w-full max-w-lg rounded-2xl bg-white shadow-2xl border border-hairline p-8 text-center">
          <FileCode className="w-8 h-8 mx-auto text-tertiary" />
          <p className="mt-3 text-sm font-semibold text-primary">暂无结构化代码变更</p>
          <p className="mt-1 text-xs text-secondary">
            结构化 diff 视图将由后续协议扩展提供（edit 结果会附带 diff 对象）。
          </p>
          <button
            onClick={onClose}
            className="mt-4 px-4 py-2 rounded-full bg-well hover:bg-black/[0.05] text-secondary text-xs font-medium"
          >
            关闭
          </button>
        </div>
      </div>
    );
  }

  const activeFile =
    MOCK_DIFF_FILES.find((f) => f.id === activeFileId) || MOCK_DIFF_FILES[0];

  const handleCopyPatch = () => {
    const patchContent = activeFile.hunks
      .flatMap((h) =>
        h.content.map((c) => {
          const prefix = c.type === 'add' ? '+' : c.type === 'delete' ? '-' : ' ';
          return `${prefix} ${c.text}`;
        })
      )
      .join('\n');

    navigator.clipboard.writeText(`--- a/${activeFile.path}\n+++ b/${activeFile.path}\n${patchContent}`);
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  };

  return (
    <div id="diff-viewer-modal-backdrop" className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/30 backdrop-blur-sm animate-in fade-in duration-150">
      <div
        id="diff-viewer-panel"
        className="w-full max-w-6xl h-[88vh] rounded-2xl border border-hairline bg-canvas shadow-2xl flex flex-col overflow-hidden"
      >
        {/* Header */}
        <div className="flex items-center justify-between px-6 py-4 bg-white border-b border-hairline">
          <div className="flex items-center gap-3">
            <div className="p-2 rounded-2xl bg-orange-soft text-orange-dark border border-orange-soft">
              <FileCode className="w-4 h-4" />
            </div>
            <div>
              <div className="flex items-center gap-2">
                <h2 className="text-sm font-bold text-primary">审查工作区代码变更</h2>
                <span className="text-[11px] px-2.5 py-0.5 rounded-full bg-well text-secondary font-mono font-medium">
                  {MOCK_DIFF_FILES.length} 个文件被修改
                </span>
              </div>
              <div className="flex items-center gap-3 text-[11px] text-secondary mt-0.5">
                <span className="flex items-center gap-1 font-mono text-secondary">
                  <GitCommit className="w-3 h-3 text-tertiary" />
                  基础快照: {activeFile.baseSnapshot}
                </span>
                <span>•</span>
                <span className="font-mono text-secondary">触发来源: {activeFile.toolCallOrigin}</span>
              </div>
            </div>
          </div>

          {/* Controls */}
          <div className="flex items-center gap-2">
            <div className="flex items-center p-0.5 rounded-full bg-well border border-hairline">
              <button
                id="diff-mode-split-btn"
                onClick={() => setViewMode('split')}
                className={`flex items-center gap-1.5 px-3 py-1 text-xs font-medium rounded-full transition-all ${
                  viewMode === 'split'
                    ? 'bg-white text-primary shadow-xs'
                    : 'text-secondary hover:text-primary'
                }`}
              >
                <Columns className="w-3.5 h-3.5" />
                <span>双栏对比</span>
              </button>
              <button
                id="diff-mode-unified-btn"
                onClick={() => setViewMode('unified')}
                className={`flex items-center gap-1.5 px-3 py-1 text-xs font-medium rounded-full transition-all ${
                  viewMode === 'unified'
                    ? 'bg-white text-primary shadow-xs'
                    : 'text-secondary hover:text-primary'
                }`}
              >
                <List className="w-3.5 h-3.5" />
                <span>统一视图</span>
              </button>
            </div>

            <button
              id="copy-diff-patch-btn"
              onClick={handleCopyPatch}
              className="p-2 rounded-full bg-white hover:bg-well text-secondary hover:text-primary border border-hairline transition-colors shadow-xs"
              title="复制 Git 补丁内容"
            >
              {copied ? <Check className="w-4 h-4 text-green-dark" /> : <Copy className="w-4 h-4 text-secondary" />}
            </button>

            <button
              id="close-diff-viewer-btn"
              onClick={onClose}
              className="p-2 rounded-full hover:bg-black/[0.05] text-secondary hover:text-primary transition-colors ml-1"
            >
              <X className="w-5 h-5" />
            </button>
          </div>
        </div>

        {/* Files Navigation Tabs */}
        <div className="flex items-center gap-1.5 px-5 py-2.5 bg-canvas border-b border-hairline overflow-x-auto text-xs">
          <Layers className="w-3.5 h-3.5 text-tertiary mr-1 shrink-0" />
          {MOCK_DIFF_FILES.map((file) => {
            const isActive = file.id === activeFile.id;
            return (
              <button
                key={file.id}
                id={`diff-file-tab-${file.id}`}
                onClick={() => setActiveFileId(file.id)}
                className={`flex items-center gap-2 px-3.5 py-1.5 rounded-full font-mono text-xs transition-colors shrink-0 ${
                  isActive
                    ? 'bg-white text-primary border border-hairline font-semibold shadow-xs'
                    : 'text-secondary hover:bg-black/[0.05] hover:text-primary'
                }`}
              >
                <span>{file.filename}</span>
                <span className="flex items-center gap-1 text-[11px] font-bold">
                  {file.additions > 0 && (
                    <span className="text-green-dark">+{file.additions}</span>
                  )}
                  {file.deletions > 0 && (
                    <span className="text-red-dark">-{file.deletions}</span>
                  )}
                </span>
              </button>
            );
          })}
        </div>

        {/* Diff Content */}
        <div className="flex-1 overflow-auto p-5 bg-canvas font-mono text-xs leading-relaxed">
          {viewMode === 'unified' ? (
            /* Unified Diff View */
            <div className="rounded-2xl border border-hairline overflow-hidden bg-white shadow-xs">
              {activeFile.hunks.map((hunk, hIdx) => (
                <div key={hIdx}>
                  <div className="px-4 py-1.5 bg-well text-secondary border-b border-hairline text-[11px] select-none font-semibold">
                    @@ -{hunk.oldStart},{hunk.oldLines} +{hunk.newStart},{hunk.newLines} @@
                  </div>
                  {hunk.content.map((line, lIdx) => (
                    <div
                      key={lIdx}
                      className={`flex items-start px-2 py-0.5 ${
                        line.type === 'add'
                          ? 'bg-green-soft text-green-dark'
                          : line.type === 'delete'
                          ? 'bg-red-soft text-red-dark line-through opacity-85'
                          : 'text-primary hover:bg-well'
                      }`}
                    >
                      <span className="w-8 text-right select-none text-tertiary text-[11px] pr-2 shrink-0">
                        {line.oldLineNo || ''}
                      </span>
                      <span className="w-8 text-right select-none text-tertiary text-[11px] pr-2 shrink-0">
                        {line.newLineNo || ''}
                      </span>
                      <span className="w-4 select-none text-secondary text-center shrink-0">
                        {line.type === 'add' ? '+' : line.type === 'delete' ? '-' : ' '}
                      </span>
                      <span className="flex-1 whitespace-pre">{line.text}</span>
                    </div>
                  ))}
                </div>
              ))}
            </div>
          ) : (
            /* Split / Side-by-Side Diff View */
            <div className="rounded-2xl border border-hairline overflow-hidden bg-white shadow-xs">
              {activeFile.hunks.map((hunk, hIdx) => {
                return (
                  <div key={hIdx}>
                    <div className="grid grid-cols-2 bg-well border-b border-hairline text-[11px] text-secondary select-none divide-x divide-hairline font-semibold">
                      <div className="px-4 py-1.5">变更前代码 ({activeFile.baseSnapshot})</div>
                      <div className="px-4 py-1.5 text-green-dark">变更后工作树</div>
                    </div>

                    <div className="divide-y divide-hairline-2">
                      {hunk.content.map((line, lIdx) => (
                        <div key={lIdx} className="grid grid-cols-2 divide-x divide-hairline">
                          {/* Left / Old Side */}
                          <div
                            className={`flex items-start px-2 py-0.5 ${
                              line.type === 'delete'
                                ? 'bg-red-soft text-red-dark'
                                : line.type === 'add'
                                ? 'bg-canvas opacity-20'
                                : 'text-primary'
                            }`}
                          >
                            <span className="w-7 text-right select-none text-tertiary text-[11px] pr-2 shrink-0">
                              {line.oldLineNo || ''}
                            </span>
                            <span className="w-4 select-none text-secondary text-center shrink-0">
                              {line.type === 'delete' ? '-' : ''}
                            </span>
                            <span className="flex-1 whitespace-pre overflow-x-hidden">
                              {line.type !== 'add' ? line.text : ''}
                            </span>
                          </div>

                          {/* Right / New Side */}
                          <div
                            className={`flex items-start px-2 py-0.5 ${
                              line.type === 'add'
                                ? 'bg-green-soft text-green-dark'
                                : line.type === 'delete'
                                ? 'bg-canvas opacity-20'
                                : 'text-primary'
                            }`}
                          >
                            <span className="w-7 text-right select-none text-tertiary text-[11px] pr-2 shrink-0">
                              {line.newLineNo || ''}
                            </span>
                            <span className="w-4 select-none text-green-dark text-center shrink-0">
                              {line.type === 'add' ? '+' : ''}
                            </span>
                            <span className="flex-1 whitespace-pre overflow-x-hidden">
                              {line.type !== 'delete' ? line.text : ''}
                            </span>
                          </div>
                        </div>
                      ))}
                    </div>
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
