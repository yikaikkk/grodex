import React, { useCallback, useEffect, useState } from 'react';
import { X, RefreshCw, Activity, Gauge, Database, Stethoscope } from 'lucide-react';
import * as acp from '../lib/acpClient';
import { eventBus } from '../lib/eventBus';
import { Session } from '../types';

interface ObservabilityPanelProps {
  isOpen: boolean;
  onClose: () => void;
  sessions: Session[];
  activeSessionId: string;
}

function fmtMs(ms: number | null): string {
  if (ms == null) return '—';
  if (ms >= 1000) return `${(ms / 1000).toFixed(1)}s`;
  return `${Math.round(ms)}ms`;
}

function fmtTokens(n: number | null): string {
  if (n == null) return '—';
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  return `${n}`;
}

function pct(rate: number | null): string {
  if (rate == null) return '—';
  return `${(rate * 100).toFixed(1)}%`;
}

function StatCard({ label, value, hint }: { label: string; value: string; hint?: string }) {
  return (
    <div className="flex-1 min-w-[110px] px-3 py-2.5 rounded-xl bg-white border border-hairline">
      <div className="text-[10px] text-secondary uppercase tracking-wide">{label}</div>
      <div className="text-lg font-bold text-primary mt-0.5">{value}</div>
      {hint && <div className="text-[10px] text-tertiary mt-0.5">{hint}</div>}
    </div>
  );
}

export const ObservabilityPanel: React.FC<ObservabilityPanelProps> = ({
  isOpen,
  onClose,
  sessions,
  activeSessionId,
}) => {
  const [overview, setOverview] = useState<acp.TelemetryOverview | null>(null);
  const [doctor, setDoctor] = useState<acp.DoctorReport | null>(null);
  const [detail, setDetail] = useState<acp.SessionDetail | null>(null);
  const [selectedSessionId, setSelectedSessionId] = useState<string>('');
  const [loading, setLoading] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setNotice(null);
    try {
      setOverview(await acp.telemetryOverview());
      setDoctor(await acp.telemetryDoctor());
    } catch (e: any) {
      setNotice(`读取失败：${e?.message || e}`);
    }
    if (selectedSessionId) {
      try {
        setDetail(await acp.telemetrySession(selectedSessionId));
      } catch (e: any) {
        setNotice(`会话明细读取失败：${e?.message || e}`);
      }
    }
    setLoading(false);
  }, [selectedSessionId]);

  // Sync the selected session to the active one each time the panel opens.
  useEffect(() => {
    if (isOpen) setSelectedSessionId((prev) => prev || activeSessionId);
  }, [isOpen, activeSessionId]);

  // Load on open + auto-refresh when a turn completes while the panel is up.
  useEffect(() => {
    if (!isOpen) return;
    load();
    const off = eventBus.on('sessionStateChanged', (d: any) => {
      if (d?.status === 'completed') load();
    });
    return () => off();
  }, [isOpen, load]);

  const handleSelectSession = (id: string) => setSelectedSessionId(id);

  if (!isOpen) return null;

  const models = overview?.models ?? [];
  const cache = overview?.cache ?? [];
  const turns = detail?.turns ?? [];

  // Per-turn rolled-up figures (first attempt's TTFT + summed tokens).
  const turnStats = turns.map((t) => {
    const firstTtft = t.attempts.find((a) => a.firstTokenMs != null)?.firstTokenMs ?? null;
    const input = t.attempts.reduce((s, a) => s + (a.inputTokens ?? 0), 0);
    const output = t.attempts.reduce((s, a) => s + (a.outputTokens ?? 0), 0);
    const cached = t.attempts.reduce((s, a) => s + (a.cachedInputTokens ?? 0), 0);
    const cacheRate = input > 0 ? cached / input : null;
    return { t, firstTtft, input, output, cached, cacheRate };
  });

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/30 backdrop-blur-sm">
      <div className="w-full max-w-5xl h-[88vh] rounded-2xl bg-canvas border border-hairline shadow-2xl flex flex-col overflow-hidden">
        {/* Header */}
        <div className="px-5 py-3.5 bg-white border-b border-hairline flex items-center justify-between">
          <div className="flex items-center gap-2.5">
            <div className="p-1.5 rounded-xl bg-accent-soft text-accent border border-accent-soft">
              <Activity className="w-4 h-4" />
            </div>
            <div>
              <h3 className="text-sm font-bold text-primary">可观测</h3>
              <p className="text-[11px] text-secondary">
                {overview
                  ? `${overview.sessions} 会话 · ${overview.turns} 轮 · 首 token / token 消耗 / cache 命中率（~/.grodex/telemetry.db）`
                  : '加载中…'}
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
          <div className="px-5 py-2 bg-red-soft text-red-dark text-xs border-b border-red-soft">
            {notice}
          </div>
        )}

        {/* Body */}
        <div className="flex-1 overflow-y-auto p-5 space-y-6 text-xs">
          {/* Global overview */}
          <section>
            <h4 className="flex items-center gap-1.5 font-semibold text-primary mb-2">
              <Gauge className="w-3.5 h-3.5 text-accent" /> 全局概览
            </h4>
            <div className="flex flex-wrap gap-2">
              <StatCard label="会话" value={overview ? `${overview.sessions}` : '—'} />
              <StatCard label="轮次 (turn)" value={overview ? `${overview.turns}` : '—'} />
              <StatCard
                label="输入 token"
                value={overview ? fmtTokens(overview.totalInputTokens) : '—'}
                hint={overview ? `输出 ${fmtTokens(overview.totalOutputTokens)}` : undefined}
              />
              <StatCard
                label="缓存命中"
                value={overview ? fmtTokens(overview.totalCachedTokens) : '—'}
              />
              <StatCard
                label="cache 命中率"
                value={overview ? pct(overview.overallCacheHitRate) : '—'}
                hint={overview ? `创建 ${fmtTokens(overview.totalCacheCreationTokens)}` : undefined}
              />
            </div>
          </section>

          {/* Per-model */}
          {models.length > 0 && (
            <section>
              <h4 className="flex items-center gap-1.5 font-semibold text-primary mb-2">
                <Database className="w-3.5 h-3.5 text-accent" /> 按模型（首 token / 延迟 / 缓存）
              </h4>
              <div className="rounded-xl bg-white border border-hairline overflow-hidden">
                <table className="w-full text-[11px]">
                  <thead className="bg-well text-secondary">
                    <tr>
                      <th className="text-left px-3 py-1.5 font-medium">模型</th>
                      <th className="text-right px-3 py-1.5 font-medium">调用</th>
                      <th className="text-right px-3 py-1.5 font-medium">错误</th>
                      <th className="text-right px-3 py-1.5 font-medium">avg TTFT</th>
                      <th className="text-right px-3 py-1.5 font-medium">avg 耗时</th>
                      <th className="text-right px-3 py-1.5 font-medium">cache 命中</th>
                      <th className="text-right px-3 py-1.5 font-medium">输入 token</th>
                    </tr>
                  </thead>
                  <tbody>
                    {models.map((m) => (
                      <tr key={`${m.provider}/${m.model}`} className="border-t border-hairline-2">
                        <td className="px-3 py-1.5 font-mono text-primary">
                          {m.provider}/{m.model}
                        </td>
                        <td className="text-right px-3 py-1.5">{m.calls}</td>
                        <td className={`text-right px-3 py-1.5 ${m.errors > 0 ? 'text-red-dark' : ''}`}>{m.errors}</td>
                        <td className="text-right px-3 py-1.5 font-mono">{fmtMs(m.avgFirstTokenMs)}</td>
                        <td className="text-right px-3 py-1.5 font-mono">{fmtMs(m.avgMs)}</td>
                        <td className="text-right px-3 py-1.5 font-mono">{pct(m.cacheHitRate)}</td>
                        <td className="text-right px-3 py-1.5 font-mono">{fmtTokens(m.totalInputTokens)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </section>
          )}

          {/* Cache detail */}
          {cache.length > 0 && (
            <section>
              <h4 className="font-semibold text-primary mb-2">缓存明细</h4>
              <div className="rounded-xl bg-white border border-hairline overflow-hidden">
                <table className="w-full text-[11px]">
                  <thead className="bg-well text-secondary">
                    <tr>
                      <th className="text-left px-3 py-1.5 font-medium">模型</th>
                      <th className="text-right px-3 py-1.5 font-medium">输入</th>
                      <th className="text-right px-3 py-1.5 font-medium">命中</th>
                      <th className="text-right px-3 py-1.5 font-medium">创建</th>
                      <th className="text-right px-3 py-1.5 font-medium">命中率</th>
                    </tr>
                  </thead>
                  <tbody>
                    {cache.map((c) => (
                      <tr key={`${c.provider}/${c.model}`} className="border-t border-hairline-2">
                        <td className="px-3 py-1.5 font-mono text-primary">
                          {c.provider}/{c.model}
                        </td>
                        <td className="text-right px-3 py-1.5 font-mono">{fmtTokens(c.inputTokens)}</td>
                        <td className="text-right px-3 py-1.5 font-mono">{fmtTokens(c.cachedInputTokens)}</td>
                        <td className="text-right px-3 py-1.5 font-mono">{fmtTokens(c.cacheCreationTokens)}</td>
                        <td className="text-right px-3 py-1.5 font-mono">{pct(c.cacheHitRate)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </section>
          )}

          {/* Session drill-down */}
          <section>
            <div className="flex items-center justify-between mb-2">
              <h4 className="font-semibold text-primary">会话下钻</h4>
              <select
                value={selectedSessionId}
                onChange={(e) => handleSelectSession(e.target.value)}
                className="px-2 py-1 rounded-lg bg-white border border-hairline text-xs text-primary max-w-[320px]"
              >
                <option value="">选择会话…</option>
                {sessions.map((s) => (
                  <option key={s.id} value={s.id}>
                    {s.title.slice(0, 40) || s.id.slice(0, 12)}
                  </option>
                ))}
              </select>
            </div>

            {!selectedSessionId ? (
              <div className="px-3 py-4 rounded-xl border border-dashed border-hairline text-center text-secondary">
                选择会话查看每轮的首 token 耗时 / token 消耗 / 记忆检索耗时
              </div>
            ) : turns.length === 0 ? (
              <div className="px-3 py-4 rounded-xl border border-dashed border-hairline text-center text-secondary">
                该会话暂无 telemetry 轮次记录
              </div>
            ) : (
              <div className="space-y-1.5">
                {turnStats.map(({ t, firstTtft, input, output, cached, cacheRate }) => (
                  <details
                    key={t.turnId}
                    className="group rounded-xl bg-white border border-hairline open:shadow-sm"
                  >
                    <summary className="px-3 py-2 cursor-pointer list-none flex items-center gap-2">
                      <span className="font-mono text-[10px] text-tertiary">{t.turnId.slice(0, 8)}…</span>
                      <span className="flex-1 text-secondary truncate">
                        {t.startedAt?.slice(11, 19) ?? '—'} · {t.status}
                        {t.terminationReason ? ` (${t.terminationReason})` : ''}
                      </span>
                      <span className="font-mono text-[11px] text-accent">TTFT {fmtMs(firstTtft)}</span>
                      <span className="font-mono text-[11px] text-primary">耗时 {fmtMs(t.durationMs)}</span>
                      <span className="font-mono text-[11px] text-secondary">
                        in {fmtTokens(input)} / out {fmtTokens(output)}
                      </span>
                      <span className="font-mono text-[11px] text-orange-dark">
                        命中率 {pct(cacheRate)}
                      </span>
                      <span className="font-mono text-[11px] text-tertiary">
                        cache {cached > 0 ? fmtTokens(cached) : '0'}
                      </span>
                    </summary>
                    <div className="px-3 pb-2 space-y-1.5">
                      {/* Memory retrieval latency */}
                      {t.memoryRetrievals.length > 0 && (
                        <div className="text-[11px] text-secondary">
                          {t.memoryRetrievals.map((m, i) => (
                            <div key={i} className="flex items-center gap-2 py-0.5">
                              <span className="text-tertiary">记忆检索</span>
                              <span className="font-mono text-accent">{fmtMs(m.durationMs)}</span>
                              <span className="text-tertiary">
                                命中 {m.selectedCount ?? 0} 条 · {m.routerKind ?? '—'}
                              </span>
                            </div>
                          ))}
                        </div>
                      )}
                      {/* Model attempts */}
                      {t.attempts.length > 0 && (
                        <div className="rounded-lg bg-well border border-hairline-2 overflow-hidden">
                          <table className="w-full text-[10px]">
                            <thead className="bg-well text-secondary">
                              <tr>
                                <th className="text-left px-2 py-1 font-medium">模型</th>
                                <th className="text-right px-2 py-1 font-medium">TTFT</th>
                                <th className="text-right px-2 py-1 font-medium">耗时</th>
                                <th className="text-right px-2 py-1 font-medium">in</th>
                                <th className="text-right px-2 py-1 font-medium">cached</th>
                                <th className="text-right px-2 py-1 font-medium">命中率</th>
                                <th className="text-right px-2 py-1 font-medium">out</th>
                                <th className="text-left px-2 py-1 font-medium">状态</th>
                              </tr>
                            </thead>
                            <tbody>
                              {t.attempts.map((a, i) => (
                                <tr key={i} className="border-t border-hairline-2">
                                  <td className="px-2 py-1 font-mono text-primary">
                                    {a.provider}/{a.model}
                                  </td>
                                  <td className="text-right px-2 py-1 font-mono">{fmtMs(a.firstTokenMs)}</td>
                                  <td className="text-right px-2 py-1 font-mono">{fmtMs(a.durationMs)}</td>
                                  <td className="text-right px-2 py-1 font-mono">{fmtTokens(a.inputTokens)}</td>
                                  <td className="text-right px-2 py-1 font-mono">{fmtTokens(a.cachedInputTokens)}</td>
                                  <td className="text-right px-2 py-1 font-mono text-orange-dark">
                                    {pct(
                                      a.inputTokens != null && a.inputTokens > 0
                                        ? (a.cachedInputTokens ?? 0) / a.inputTokens
                                        : null
                                    )}
                                  </td>
                                  <td className="text-right px-2 py-1 font-mono">{fmtTokens(a.outputTokens)}</td>
                                  <td className="px-2 py-1">
                                    <span className={a.status === 'error' ? 'text-red-dark' : 'text-secondary'}>
                                      {a.status ?? '—'}
                                      {a.errorClass ? ` (${a.errorClass})` : ''}
                                    </span>
                                  </td>
                                </tr>
                              ))}
                            </tbody>
                          </table>
                        </div>
                      )}
                    </div>
                  </details>
                ))}
              </div>
            )}
          </section>

          {/* Doctor / health */}
          {doctor && (
            <section>
              <h4 className="flex items-center gap-1.5 font-semibold text-primary mb-2">
                <Stethoscope className="w-3.5 h-3.5 text-accent" /> 诊断
              </h4>
              <div className="flex flex-wrap gap-2">
                <StatCard label="open turns" value={`${doctor.openTurns}`} hint="未完成（崩溃候选）" />
                <StatCard label="running tools" value={`${doctor.runningTools}`} hint="started 未 finished" />
                <StatCard label="uncommitted" value={`${doctor.uncommittedResults}`} hint="执行了未提交" />
                <StatCard label="failed attempts" value={`${doctor.failedAttempts}`} />
                <StatCard label="indeterminate" value={`${doctor.indeterminateTools}`} />
              </div>
              {doctor.errors.length > 0 && (
                <div className="mt-2 rounded-xl bg-white border border-hairline overflow-hidden">
                  <div className="px-3 py-1.5 bg-well text-secondary font-medium">近期 error 事件</div>
                  {doctor.errors.map((e, i) => (
                    <div key={i} className="px-3 py-1 border-t border-hairline-2 font-mono text-[10px] text-red-dark">
                      {e.occurredAt.slice(0, 19)} · {e.kind}
                      {e.callId ? ` · ${e.callId.slice(0, 8)}` : ''}
                    </div>
                  ))}
                </div>
              )}
            </section>
          )}
        </div>

        {/* Footer */}
        <div className="px-5 py-3 bg-white border-t border-hairline flex items-center justify-between">
          <span className="text-[11px] text-secondary">数据来自 serve 进程实时写入的 telemetry.db，轮次结束后自动刷新。</span>
          <button
            onClick={load}
            disabled={loading}
            className="px-3 py-1.5 rounded-full bg-white border border-hairline text-secondary text-xs flex items-center gap-1.5 hover:bg-black/[0.05] disabled:opacity-50"
          >
            <RefreshCw className={`w-3.5 h-3.5 ${loading ? 'animate-spin' : ''}`} /> 刷新
          </button>
        </div>
      </div>
    </div>
  );
};
