import React, { useCallback, useEffect, useState } from 'react';
import { X, RefreshCw, Trash2, Wrench, AlertTriangle, Check } from 'lucide-react';
import * as acp from '../lib/acpClient';

interface MemoryManagerProps {
  isOpen: boolean;
  onClose: () => void;
}

const STATUS_COLOR: Record<string, string> = {
  active: 'bg-green-soft text-green-dark border-green-soft',
  candidate: 'bg-orange-soft text-orange-dark border-orange-soft',
  superseded: 'bg-well text-secondary border-hairline',
  conflicted: 'bg-red-soft text-red-dark border-red-soft',
  orphaned: 'bg-well text-secondary border-hairline',
};

export const MemoryManager: React.FC<MemoryManagerProps> = ({ isOpen, onClose }) => {
  const [data, setData] = useState<acp.MemoryOverview | null>(null);
  const [loading, setLoading] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [running, setRunning] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const d = await acp.listMemories();
      setData(d);
    } catch (e: any) {
      setNotice(`读取失败：${e?.message || e}`);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (isOpen) load();
  }, [isOpen, load]);

  const handleDelete = async (id: string) => {
    try {
      await acp.deleteMemory(id);
      setNotice(`已删除（orphaned）：${id.slice(0, 12)}…`);
      load();
    } catch (e: any) {
      setNotice(`删除失败：${e?.message || e}`);
    }
  };

  const handleMaintenance = async () => {
    setRunning(true);
    try {
      const r = await acp.runMemoryMaintenance();
      setNotice(`维护完成：units=${r.units}, pending 冲突=${r.conflictsPending}（governance/consolidation ${r.governanceOk && r.consolidationOk ? 'OK' : '异常'}）`);
      load();
    } catch (e: any) {
      setNotice(`维护失败：${e?.message || e}`);
    } finally {
      setRunning(false);
    }
  };

  if (!isOpen) return null;

  const units = data?.units ?? [];
  const conflicts = data?.conflicts ?? [];
  const badge = (s: string) => {
    const c = STATUS_COLOR[s] ?? STATUS_COLOR.orphaned;
    return (
      <span className={`px-1.5 py-0.5 rounded-full border text-[10px] font-mono ${c}`}>
        {s}
      </span>
    );
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/30 backdrop-blur-sm">
      <div className="w-full max-w-3xl h-[86vh] rounded-2xl bg-canvas border border-hairline shadow-2xl flex flex-col overflow-hidden">
        {/* Header */}
        <div className="px-5 py-3.5 bg-white border-b border-hairline flex items-center justify-between">
          <div className="flex items-center gap-2.5">
            <div className="p-1.5 rounded-xl bg-accent-soft text-accent border border-accent-soft">
              <Wrench className="w-4 h-4" />
            </div>
            <div>
              <h3 className="text-sm font-bold text-primary">记忆管理</h3>
              <p className="text-[11px] text-secondary">
                {units.length} 条记忆 · {conflicts.length} 条冲突（~/.grodex/memory.db）
              </p>
            </div>
          </div>
          <button
            onClick={onClose}
            className="p-1.5 rounded-full hover:bg-black/[0.05] text-secondary transition-colors"
          >
            <X className="w-5 h-5" />
          </button>
        </div>

        {notice && (
          <div className="px-5 py-2 bg-accent-soft text-accent text-xs flex items-center gap-2 border-b border-accent-soft">
            <Check className="w-3.5 h-3.5" /> {notice}
          </div>
        )}

        {/* Body */}
        <div className="flex-1 overflow-y-auto p-5 space-y-5 text-xs">
          {/* Conflicts */}
          <section>
            <div className="flex items-center justify-between mb-2">
              <h4 className="flex items-center gap-1.5 font-semibold text-primary">
                <AlertTriangle className="w-3.5 h-3.5 text-orange-dark" /> 冲突（pending 将自动按“说法时间新者胜”裁决）
              </h4>
            </div>
            {conflicts.length === 0 ? (
              <div className="px-3 py-4 rounded-xl border border-dashed border-hairline text-center text-secondary">
                暂无冲突
              </div>
            ) : (
              <div className="space-y-1.5">
                {conflicts.map((c) => (
                  <div key={c.conflictId} className="px-3 py-2 rounded-xl bg-white border border-hairline flex items-center justify-between gap-2">
                    <div className="min-w-0">
                      <div className="font-mono text-[11px] text-primary truncate">
                        {c.leftMemoryId.slice(0, 10)}… ⇄ {c.rightMemoryId.slice(0, 10)}…
                        <span className="ml-2 text-secondary">[{c.relation}] {c.status}</span>
                      </div>
                      {c.reason && <div className="text-[11px] text-secondary truncate">{c.reason}</div>}
                    </div>
                  </div>
                ))}
              </div>
            )}
          </section>

          {/* Units */}
          <section>
            <div className="flex items-center justify-between mb-2">
              <h4 className="font-semibold text-primary">记忆条目</h4>
              <span className="text-secondary text-[11px]">删除 = 标记 orphaned，不再被检索</span>
            </div>
            {units.length === 0 ? (
              <div className="px-3 py-4 rounded-xl border border-dashed border-hairline text-center text-secondary">
                暂无记忆条目
              </div>
            ) : (
              <div className="space-y-1.5">
                {units.map((u) => (
                  <div key={u.id} className="px-3 py-2 rounded-xl bg-white border border-hairline flex items-center justify-between gap-3">
                    <div className="min-w-0">
                      <div className="flex items-center gap-2 mb-0.5 flex-wrap">
                        {badge(u.status)}
                        <span className="font-mono text-[11px] text-accent">{u.kind}</span>
                        <span className="font-mono text-[10px] text-secondary">{u.scope}</span>
                        <span className="font-mono text-[10px] text-tertiary">{u.id.slice(0, 12)}…</span>
                      </div>
                      <div className="text-[12px] text-primary whitespace-pre-wrap leading-snug line-clamp-3">
                        {u.content}
                      </div>
                    </div>
                    <button
                      onClick={() => handleDelete(u.id)}
                      disabled={u.status === 'orphaned'}
                      className="p-1.5 rounded-lg text-tertiary hover:text-red-dark hover:bg-red-soft shrink-0 disabled:opacity-40"
                      title="删除（orphaned）"
                    >
                      <Trash2 className="w-4 h-4" />
                    </button>
                  </div>
                ))}
              </div>
            )}
          </section>
        </div>

        {/* Footer */}
        <div className="px-5 py-3 bg-white border-t border-hairline flex items-center justify-between">
          <span className="text-[11px] text-secondary">删除不会立刻从磁盘移除（保留审计），会停止被检索。</span>
          <div className="flex items-center gap-2">
            <button
              onClick={load}
              disabled={loading}
              className="px-3 py-1.5 rounded-full bg-white border border-hairline text-secondary text-xs flex items-center gap-1.5 hover:bg-black/[0.05]"
            >
              <RefreshCw className={`w-3.5 h-3.5 ${loading ? 'animate-spin' : ''}`} /> 刷新
            </button>
            <button
              onClick={handleMaintenance}
              disabled={running}
              className="px-3 py-1.5 rounded-full bg-accent hover:bg-accent-hover text-white text-xs flex items-center gap-1.5"
            >
              <Wrench className={`w-3.5 h-3.5 ${running ? 'animate-spin' : ''}`} /> 执行治理/合并
            </button>
          </div>
        </div>
      </div>
    </div>
  );
};
