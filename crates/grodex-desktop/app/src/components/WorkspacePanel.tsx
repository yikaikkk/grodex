import React, { useCallback, useEffect, useRef, useState } from 'react';
import {
  Folder,
  FolderOpen,
  FileText,
  FileCode,
  ChevronDown,
  ChevronRight,
  Search,
  Hash,
  FileSearch,
  Loader2,
  X,
  RefreshCw,
} from 'lucide-react';
import * as acp from '../lib/acpClient';
import { WorkspaceEntryJson, WorkspaceMatchJson } from '../lib/acpClient';

interface WorkspacePanelProps {
  isOpen: boolean;
  onClose: () => void;
  /** Active workspace root (empty → panel shows a hint). */
  workspace: string;
  /** Open the file preview, optionally scrolled to line/column. */
  onOpenFile: (path: string, line?: number, column?: number) => void;
}

type Tab = 'tree' | 'search';

const FILE_ICON_COLORS: Record<string, string> = {
  ts: 'text-blue-500',
  tsx: 'text-blue-500',
  js: 'text-yellow-600',
  jsx: 'text-yellow-600',
  rs: 'text-orange-600',
  py: 'text-green-600',
  md: 'text-secondary',
  json: 'text-yellow-600',
  toml: 'text-secondary',
  css: 'text-sky-500',
  html: 'text-orange-500',
};

/**
 * Right-side workspace panel: lazy file tree + filename/content search.
 * Layout is width-stable (`min-w-0`/`truncate`/`overflow-hidden`) so long
 * paths never push the layout around.
 */
const WorkspacePanelInner: React.FC<WorkspacePanelProps> = ({
  isOpen,
  onClose,
  workspace,
  onOpenFile,
}) => {
  const [tab, setTab] = useState<Tab>('tree');
  const [query, setQuery] = useState('');
  const [searchMode, setSearchMode] = useState<'filename' | 'content'>('filename');
  const [results, setResults] = useState<WorkspaceMatchJson[]>([]);
  const [searching, setSearching] = useState(false);
  const [searchError, setSearchError] = useState<string | null>(null);

  // Tree state: roots loaded on open; children loaded on first expand.
  const [roots, setRoots] = useState<WorkspaceEntryJson[]>([]);
  const [children, setChildren] = useState<Record<string, WorkspaceEntryJson[]>>({});
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const [loadingDirs, setLoadingDirs] = useState<Record<string, boolean>>({});
  const [treeError, setTreeError] = useState<string | null>(null);
  const loadSeq = useRef(0);

  const loadRoots = useCallback(async () => {
    if (!workspace) return;
    const seq = ++loadSeq.current;
    setTreeError(null);
    try {
      const entries = await acp.listWorkspaceEntries(workspace);
      if (seq === loadSeq.current) {
        setRoots(entries);
        setChildren({});
        setExpanded({});
      }
    } catch (e: any) {
      if (seq === loadSeq.current) setTreeError(e?.message || String(e));
    }
  }, [workspace]);

  useEffect(() => {
    if (isOpen && workspace) loadRoots();
  }, [isOpen, workspace, loadRoots]);

  const toggleDir = async (node: WorkspaceEntryJson) => {
    const path = node.path;
    if (expanded[path]) {
      setExpanded((prev) => ({ ...prev, [path]: false }));
      return;
    }
    setExpanded((prev) => ({ ...prev, [path]: true }));
    if (children[path]) return; // already loaded
    const seq = ++loadSeq.current;
    setLoadingDirs((prev) => ({ ...prev, [path]: true }));
    try {
      const kids = await acp.listWorkspaceEntries(workspace, path);
      if (seq === loadSeq.current) setChildren((prev) => ({ ...prev, [path]: kids }));
    } catch (e: any) {
      if (seq === loadSeq.current) setTreeError(e?.message || String(e));
    } finally {
      if (seq === loadSeq.current) {
        setLoadingDirs((prev) => ({ ...prev, [path]: false }));
      }
    }
  };

  // Search: only re-runs when the panel is open and query/mode/tab/workspace
  // change — never on Timeline streaming re-renders. Debounced per keystroke.
  useEffect(() => {
    if (!isOpen || tab !== 'search' || !workspace) return;
    const q = query.trim();
    if (!q) {
      setResults([]);
      setSearchError(null);
      setSearching(false);
      return;
    }
    let cancelled = false;
    setSearching(true);
    setSearchError(null);
    const timer = setTimeout(async () => {
      try {
        const hits = await acp.searchWorkspace(workspace, q, searchMode);
        if (!cancelled) setResults(hits);
      } catch (e: any) {
        if (!cancelled) setSearchError(e?.message || String(e));
      } finally {
        if (!cancelled) setSearching(false);
      }
    }, 300);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [isOpen, query, searchMode, tab, workspace]);

  if (!isOpen) return null;

  const renderIcon = (node: WorkspaceEntryJson, isOpenDir: boolean) => {
    if (node.is_dir) {
      return isOpenDir ? (
        <FolderOpen className="w-3.5 h-3.5 text-accent shrink-0" />
      ) : (
        <Folder className="w-3.5 h-3.5 text-accent shrink-0" />
      );
    }
    const color = FILE_ICON_COLORS[node.ext || ''] || 'text-secondary';
    return node.ext === 'md' ? (
      <FileText className={`w-3.5 h-3.5 ${color} shrink-0`} />
    ) : (
      <FileCode className={`w-3.5 h-3.5 ${color} shrink-0`} />
    );
  };

  const renderNode = (node: WorkspaceEntryJson, depth: number) => {
    const isOpenDir = !!expanded[node.path];
    return (
      <div key={node.path}>
        <button
          id={`ws-node-${node.path.replace(/\//g, '-')}`}
          onClick={() => (node.is_dir ? toggleDir(node) : onOpenFile(node.path))}
          className={`w-full flex items-center gap-1.5 px-2 py-1 rounded-md text-left text-xs transition-colors ${
            node.is_dir
              ? 'text-primary hover:bg-well'
              : 'text-secondary hover:bg-well hover:text-primary'
          }`}
          style={{ paddingLeft: 8 + depth * 14 }}
          title={node.path}
        >
          {node.is_dir ? (
            loadingDirs[node.path] ? (
              <Loader2 className="w-3 h-3 text-tertiary animate-spin shrink-0" />
            ) : isOpenDir ? (
              <ChevronDown className="w-3 h-3 text-tertiary shrink-0" />
            ) : (
              <ChevronRight className="w-3 h-3 text-tertiary shrink-0" />
            )
          ) : (
            <span className="w-3 shrink-0" />
          )}
          {renderIcon(node, isOpenDir)}
          <span className="truncate flex-1 min-w-0">{node.name}</span>
        </button>
        {node.is_dir && isOpenDir &&
          (children[node.path] || []).map((child) => renderNode(child, depth + 1))}
      </div>
    );
  };

  return (
    <aside
      id="workspace-panel"
      className="w-80 sm:w-88 my-2 mr-2 sm:my-2.5 sm:mr-2.5 ml-1 sm:ml-1.5 rounded-2xl border border-hairline bg-white shadow-xs flex flex-col shrink-0 min-w-0 text-primary overflow-hidden select-none"
    >
      {/* Panel Header */}
      <div className="flex items-center justify-between px-4 py-3 border-b border-hairline bg-white shrink-0">
        <div className="flex items-center gap-2 min-w-0">
          <Folder className="w-4 h-4 text-accent shrink-0" />
          <h3 className="text-xs font-bold text-primary tracking-wide font-sans truncate">
            工作区文件
          </h3>
        </div>
        <button
          id="close-workspace-btn"
          onClick={onClose}
          className="p-1 rounded-md hover:bg-canvas text-secondary hover:text-primary transition-colors shrink-0"
          title="关闭面板"
        >
          <X className="w-4 h-4" />
        </button>
      </div>

      {/* Tabs */}
      <div className="flex items-center gap-1 px-3 py-2 border-b border-hairline bg-white shrink-0">
        {(
          [
            { id: 'tree', label: '文件树', icon: <Folder className="w-3.5 h-3.5" /> },
            { id: 'search', label: '搜索', icon: <Search className="w-3.5 h-3.5" /> },
          ] as const
        ).map((t) => (
          <button
            key={t.id}
            id={`ws-tab-${t.id}`}
            onClick={() => setTab(t.id)}
            className={`px-3 py-1 rounded-lg text-[11px] font-medium flex items-center gap-1 transition-colors ${
              tab === t.id
                ? 'bg-accent-soft text-accent'
                : 'text-secondary hover:text-primary hover:bg-well'
            }`}
          >
            {t.icon}
            {t.label}
          </button>
        ))}
      </div>

      {/* Workspace path */}
      {workspace ? (
        <div
          className="px-3.5 py-1.5 bg-well border-b border-hairline text-[10px] text-tertiary font-mono truncate shrink-0"
          title={workspace}
        >
          {workspace}
        </div>
      ) : (
        <div className="px-4 py-8 text-xs text-secondary text-center shrink-0">
          还没有选择工作目录，先发送一条消息创建会话。
        </div>
      )}

      {/* Body */}
      {workspace && tab === 'tree' && (
        <div className="flex-1 overflow-y-auto [scrollbar-gutter:stable] p-2 min-w-0">
          {treeError && (
            <div className="px-2 py-3 text-[11px] text-red-dark flex items-center justify-between gap-2">
              <span className="truncate">{treeError}</span>
              <button
                onClick={loadRoots}
                className="p-1 rounded-md hover:bg-well text-secondary shrink-0"
                title="重试"
              >
                <RefreshCw className="w-3 h-3" />
              </button>
            </div>
          )}
          {roots.map((node) => renderNode(node, 0))}
          {roots.length === 0 && !treeError && (
            <div className="px-2 py-6 text-[11px] text-tertiary text-center">空目录</div>
          )}
        </div>
      )}

      {workspace && tab === 'search' && (
        <div className="flex-1 flex flex-col min-w-0 overflow-hidden">
          {/* Search controls */}
          <div className="px-3 pt-3 pb-2 space-y-2 shrink-0">
            <div className="flex items-center gap-1.5 px-2.5 py-1.5 rounded-lg bg-well border border-hairline">
              <Search className="w-3.5 h-3.5 text-tertiary shrink-0" />
              <input
                id="workspace-search-input"
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder={searchMode === 'filename' ? '搜索文件名…' : '搜索文件内容…'}
                className="flex-1 min-w-0 bg-transparent text-xs text-primary placeholder-tertiary focus:outline-none"
              />
              {searching && <Loader2 className="w-3 h-3 text-tertiary animate-spin shrink-0" />}
            </div>
            <div className="flex items-center gap-1">
              {(
                [
                  { id: 'filename', label: '文件名', icon: <Hash className="w-3 h-3" /> },
                  { id: 'content', label: '文件内容', icon: <FileSearch className="w-3 h-3" /> },
                ] as const
              ).map((m) => (
                <button
                  key={m.id}
                  id={`ws-search-mode-${m.id}`}
                  onClick={() => setSearchMode(m.id)}
                  className={`px-2.5 py-0.5 rounded-full text-[10px] font-medium flex items-center gap-1 transition-colors ${
                    searchMode === m.id
                      ? 'bg-accent-soft text-accent'
                      : 'text-secondary hover:text-primary bg-well'
                  }`}
                >
                  {m.icon}
                  {m.label}
                </button>
              ))}
            </div>
          </div>

          {/* Results */}
          <div className="flex-1 overflow-y-auto [scrollbar-gutter:stable] px-2 pb-2 min-w-0">
            {searchError && (
              <div className="px-2 py-3 text-[11px] text-red-dark">{searchError}</div>
            )}
            {!searchError && query.trim() && !searching && results.length === 0 && (
              <div className="px-2 py-6 text-[11px] text-tertiary text-center">没有匹配结果</div>
            )}
            {results.map((hit, i) => (
              <button
                key={`${hit.path}-${i}`}
                id={`ws-search-hit-${i}`}
                onClick={() => onOpenFile(hit.path, hit.line ?? undefined, hit.column ?? undefined)}
                className="w-full text-left px-2 py-1.5 rounded-md hover:bg-well transition-colors min-w-0"
              >
                <div className="flex items-center gap-1.5 min-w-0">
                  <FileCode className="w-3 h-3 text-accent shrink-0" />
                  <span className="text-[11px] font-mono text-primary truncate flex-1 min-w-0">
                    {hit.path}
                  </span>
                </div>
                {hit.text && (
                  <div className="pl-4.5 mt-0.5 text-[10px] font-mono text-secondary truncate">
                    {hit.line}:{hit.column} · {hit.text}
                  </div>
                )}
              </button>
            ))}
          </div>
        </div>
      )}
    </aside>
  );
};

/** Memoized: tree/search state lives inside; streaming frames skip it. */
export const WorkspacePanel = React.memo(WorkspacePanelInner);
