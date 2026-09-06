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
      <div id="diff-viewer-modal-backdrop" className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-[#21262d]/40 backdrop-blur-xs">
        <div className="w-full max-w-lg rounded-3xl bg-white shadow-2xl border border-[#e2ddd3] p-8 text-center">
          <FileCode className="w-8 h-8 mx-auto text-[#9aa0aa]" />
          <p className="mt-3 text-sm font-semibold text-[#2b3036]">暂无结构化代码变更</p>
          <p className="mt-1 text-xs text-[#717782]">
            结构化 diff 视图将由后续协议扩展提供（edit 结果会附带 diff 对象）。
          </p>
          <button
            onClick={onClose}
            className="mt-4 px-4 py-2 rounded-full bg-[#f4efe8] hover:bg-[#ece6dc] text-[#525964] text-xs font-medium"
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
    <div id="diff-viewer-modal-backdrop" className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-[#21262d]/40 backdrop-blur-xs animate-in fade-in duration-150">
      <div
        id="diff-viewer-panel"
        className="w-full max-w-6xl h-[88vh] rounded-3xl border border-[#ded8cd] bg-[#faf9f7] shadow-2xl flex flex-col overflow-hidden"
      >
        {/* Header */}
        <div className="flex items-center justify-between px-6 py-4 bg-[#ffffff] border-b border-[#ece6dc]">
          <div className="flex items-center gap-3">
            <div className="p-2 rounded-2xl bg-[#fef8ea] text-[#b07419] border border-[#f5dfb4]">
              <FileCode className="w-4 h-4" />
            </div>
            <div>
              <div className="flex items-center gap-2">
                <h2 className="text-sm font-bold text-[#2b3036]">审查工作区代码变更</h2>
                <span className="text-[11px] px-2.5 py-0.5 rounded-full bg-[#f2ede4] text-[#656c78] font-mono font-medium">
                  {MOCK_DIFF_FILES.length} 个文件被修改
                </span>
              </div>
              <div className="flex items-center gap-3 text-[11px] text-[#717782] mt-0.5">
                <span className="flex items-center gap-1 font-mono text-[#575e6a]">
                  <GitCommit className="w-3 h-3 text-[#9aa0aa]" />
                  基础快照: {activeFile.baseSnapshot}
                </span>
                <span>•</span>
                <span className="font-mono text-[#575e6a]">触发来源: {activeFile.toolCallOrigin}</span>
              </div>
            </div>
          </div>

          {/* Controls */}
          <div className="flex items-center gap-2">
            <div className="flex items-center p-0.5 rounded-full bg-[#f4efe8] border border-[#e5dfd4]">
              <button
                id="diff-mode-split-btn"
                onClick={() => setViewMode('split')}
                className={`flex items-center gap-1.5 px-3 py-1 text-xs font-medium rounded-full transition-all ${
                  viewMode === 'split'
                    ? 'bg-[#ffffff] text-[#2c3138] shadow-xs'
                    : 'text-[#6b727d] hover:text-[#2c3138]'
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
                    ? 'bg-[#ffffff] text-[#2c3138] shadow-xs'
                    : 'text-[#6b727d] hover:text-[#2c3138]'
                }`}
              >
                <List className="w-3.5 h-3.5" />
                <span>统一视图</span>
              </button>
            </div>

            <button
              id="copy-diff-patch-btn"
              onClick={handleCopyPatch}
              className="p-2 rounded-full bg-[#ffffff] hover:bg-[#f5f1ea] text-[#616874] hover:text-[#2b3036] border border-[#ded8cd] transition-colors shadow-xs"
              title="复制 Git 补丁内容"
            >
              {copied ? <Check className="w-4 h-4 text-[#256e2c]" /> : <Copy className="w-4 h-4 text-[#616874]" />}
            </button>

            <button
              id="close-diff-viewer-btn"
              onClick={onClose}
              className="p-2 rounded-full hover:bg-[#f0ece5] text-[#717782] hover:text-[#2b3036] transition-colors ml-1"
            >
              <X className="w-5 h-5" />
            </button>
          </div>
        </div>

        {/* Files Navigation Tabs */}
        <div className="flex items-center gap-1.5 px-5 py-2.5 bg-[#f6f3ec] border-b border-[#ece6dc] overflow-x-auto text-xs">
          <Layers className="w-3.5 h-3.5 text-[#8b929e] mr-1 shrink-0" />
          {MOCK_DIFF_FILES.map((file) => {
            const isActive = file.id === activeFile.id;
            return (
              <button
                key={file.id}
                id={`diff-file-tab-${file.id}`}
                onClick={() => setActiveFileId(file.id)}
                className={`flex items-center gap-2 px-3.5 py-1.5 rounded-full font-mono text-xs transition-colors shrink-0 ${
                  isActive
                    ? 'bg-[#ffffff] text-[#2c323a] border border-[#ded8cd] font-semibold shadow-xs'
                    : 'text-[#69707c] hover:bg-[#eee9df] hover:text-[#33383f]'
                }`}
              >
                <span>{file.filename}</span>
                <span className="flex items-center gap-1 text-[11px] font-bold">
                  {file.additions > 0 && (
                    <span className="text-[#256e2c]">+{file.additions}</span>
                  )}
                  {file.deletions > 0 && (
                    <span className="text-[#b83838]">-{file.deletions}</span>
                  )}
                </span>
              </button>
            );
          })}
        </div>

        {/* Diff Content */}
        <div className="flex-1 overflow-auto p-5 bg-[#faf9f7] font-mono text-xs leading-relaxed">
          {viewMode === 'unified' ? (
            /* Unified Diff View */
            <div className="rounded-2xl border border-[#e2ddd3] overflow-hidden bg-[#ffffff] shadow-xs">
              {activeFile.hunks.map((hunk, hIdx) => (
                <div key={hIdx}>
                  <div className="px-4 py-1.5 bg-[#f5f1ea] text-[#6b727e] border-b border-[#e5dfd5] text-[11px] select-none font-semibold">
                    @@ -{hunk.oldStart},{hunk.oldLines} +{hunk.newStart},{hunk.newLines} @@
                  </div>
                  {hunk.content.map((line, lIdx) => (
                    <div
                      key={lIdx}
                      className={`flex items-start px-2 py-0.5 ${
                        line.type === 'add'
                          ? 'bg-[#eef8ef] text-[#1c6422]'
                          : line.type === 'delete'
                          ? 'bg-[#fdf0f0] text-[#b83838] line-through opacity-85'
                          : 'text-[#383e46] hover:bg-[#faf7f2]'
                      }`}
                    >
                      <span className="w-8 text-right select-none text-[#9aa0aa] text-[11px] pr-2 shrink-0">
                        {line.oldLineNo || ''}
                      </span>
                      <span className="w-8 text-right select-none text-[#9aa0aa] text-[11px] pr-2 shrink-0">
                        {line.newLineNo || ''}
                      </span>
                      <span className="w-4 select-none text-[#717782] text-center shrink-0">
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
            <div className="rounded-2xl border border-[#e2ddd3] overflow-hidden bg-[#ffffff] shadow-xs">
              {activeFile.hunks.map((hunk, hIdx) => {
                return (
                  <div key={hIdx}>
                    <div className="grid grid-cols-2 bg-[#f5f1ea] border-b border-[#e5dfd5] text-[11px] text-[#555c66] select-none divide-x divide-[#e5dfd5] font-semibold">
                      <div className="px-4 py-1.5">变更前代码 ({activeFile.baseSnapshot})</div>
                      <div className="px-4 py-1.5 text-[#256e2c]">变更后工作树</div>
                    </div>

                    <div className="divide-y divide-[#f0ece5]">
                      {hunk.content.map((line, lIdx) => (
                        <div key={lIdx} className="grid grid-cols-2 divide-x divide-[#e8e3da]">
                          {/* Left / Old Side */}
                          <div
                            className={`flex items-start px-2 py-0.5 ${
                              line.type === 'delete'
                                ? 'bg-[#fdf0f0] text-[#b83838]'
                                : line.type === 'add'
                                ? 'bg-[#faf8f4] opacity-20'
                                : 'text-[#383e46]'
                            }`}
                          >
                            <span className="w-7 text-right select-none text-[#9aa0aa] text-[11px] pr-2 shrink-0">
                              {line.oldLineNo || ''}
                            </span>
                            <span className="w-4 select-none text-[#717782] text-center shrink-0">
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
                                ? 'bg-[#eef8ef] text-[#1c6422]'
                                : line.type === 'delete'
                                ? 'bg-[#faf8f4] opacity-20'
                                : 'text-[#383e46]'
                            }`}
                          >
                            <span className="w-7 text-right select-none text-[#9aa0aa] text-[11px] pr-2 shrink-0">
                              {line.newLineNo || ''}
                            </span>
                            <span className="w-4 select-none text-[#256e2c] text-center shrink-0">
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
