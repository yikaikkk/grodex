import React, { useState, useEffect } from 'react';
import {
  ShieldAlert,
  Clock,
  Check,
  CheckCheck,
  XCircle,
  Sliders,
  AlertTriangle,
  Code2,
} from 'lucide-react';
import { ApprovalRequest } from '../types';

interface ApprovalModalProps {
  request: ApprovalRequest | null;
  onResolve: (
    approvalId: string,
    action: 'allowed_once' | 'always_allowed' | 'denied' | 'narrowed',
    narrowedParams?: any
  ) => void;
  onDismiss?: () => void;
}

/** Seconds left until the server-side approval window expires. Uses the
 * wall-clock `deadlineMs` captured at emission, so a ticket shown late from
 * the queue still shows the true remaining time (not a stale full window). */
function secondsUntilDeadline(request: ApprovalRequest | null): number {
  if (!request) return 60;
  const deadline = request.deadlineMs ?? Date.now() + request.remainingSec * 1000;
  return Math.max(0, Math.ceil((deadline - Date.now()) / 1000));
}

export const ApprovalModal: React.FC<ApprovalModalProps> = ({
  request,
  onResolve,
  onDismiss,
}) => {
  const [remainingSec, setRemainingSec] = useState(() =>
    secondsUntilDeadline(request)
  );
  const [isNarrowing, setIsNarrowing] = useState(false);
  const [narrowedJsonText, setNarrowedJsonText] = useState(
    request ? JSON.stringify(request.params, null, 2) : ''
  );
  const [jsonError, setJsonError] = useState<string | null>(null);

  // Recompute remaining against the server deadline every second.
  useEffect(() => {
    if (!request) return;

    setRemainingSec(secondsUntilDeadline(request));
    setIsNarrowing(false);
    setNarrowedJsonText(JSON.stringify(request.params, null, 2));
    setJsonError(null);

    const interval = setInterval(() => {
      setRemainingSec(secondsUntilDeadline(request));
    }, 1000);

    return () => clearInterval(interval);
  }, [request]);

  if (!request) return null;

  const isExpired = remainingSec <= 0;
  const total = Math.max(1, request.totalDurationSec);
  const progressPercent = (remainingSec / total) * 100;

  const handleNarrowSubmit = () => {
    try {
      const parsed = JSON.parse(narrowedJsonText);
      setJsonError(null);
      onResolve(request.id, 'narrowed', parsed);
    } catch (err: any) {
      setJsonError(err.message || '非法的 JSON 语法格式');
    }
  };

  return (
    <div id="approval-modal-backdrop" className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/30 backdrop-blur-sm animate-in fade-in duration-150">
      <div
        id="approval-modal-card"
        className="w-full max-w-2xl rounded-2xl border border-hairline bg-white shadow-2xl overflow-hidden flex flex-col max-h-[92vh]"
      >
        {/* Header with Circular Countdown Indicator */}
        <div className="flex items-center justify-between px-6 py-4.5 bg-canvas border-b border-hairline shrink-0">
          <div className="flex items-center gap-3">
            <div className="p-2 rounded-2xl bg-orange-soft text-orange-dark border border-orange-soft">
              <ShieldAlert className="w-5 h-5" />
            </div>
            <div>
              <h3 className="text-sm font-bold text-primary flex items-center gap-2">
                需人工安全审批
                <span className="text-[11px] px-2.5 py-0.5 rounded-full bg-well text-secondary border border-hairline font-mono font-normal">
                  {request.toolName}
                </span>
              </h3>
              <p className="text-xs text-secondary mt-0.5">
                执行目标: <span className="font-mono text-primary">{request.target}</span>
              </p>
            </div>
          </div>

          {/* Countdown badge */}
          <div className="flex items-center gap-2">
            <div className="relative w-9 h-9 flex items-center justify-center">
              <svg className="w-full h-full transform -rotate-90" viewBox="0 0 36 36">
                <path
                  className="text-hairline-2"
                  strokeWidth="3"
                  stroke="currentColor"
                  fill="none"
                  d="M18 2.0845 a 15.9155 15.9155 0 0 1 0 31.831 a 15.9155 15.9155 0 0 1 0 -31.831"
                />
                <path
                  className={isExpired ? 'text-red-dark' : 'text-orange transition-all duration-1000'}
                  strokeDasharray={`${progressPercent}, 100`}
                  strokeWidth="3"
                  strokeLinecap="round"
                  stroke="currentColor"
                  fill="none"
                  d="M18 2.0845 a 15.9155 15.9155 0 0 1 0 31.831 a 15.9155 15.9155 0 0 1 0 -31.831"
                />
              </svg>
              <span className={`absolute text-[11px] font-mono font-bold ${isExpired ? 'text-red-dark' : 'text-orange-dark'}`}>
                {remainingSec}s
              </span>
            </div>
          </div>
        </div>

        {/* Modal Body */}
        <div className="p-6 space-y-4 flex-1 min-h-0 overflow-y-auto text-xs">
          {/* Reason & Source Metadata */}
          <div className="p-3.5 rounded-2xl bg-orange-soft border border-orange-soft space-y-1.5">
            <div className="flex items-center gap-2 text-orange-dark font-semibold">
              <AlertTriangle className="w-4 h-4 text-orange shrink-0" />
              <span>安全评估说明</span>
            </div>
            <p className="text-primary text-xs leading-relaxed">
              {request.reason}
            </p>
            <div className="flex items-center gap-2 pt-1 text-[11px] text-secondary">
              <span>发起 Agent:</span>
              <span className="px-2 py-0.5 rounded-full bg-well text-accent font-mono border border-hairline">
                {request.sourceAgent}
              </span>
            </div>
          </div>

          {/* Structured Parameters Preview / Narrow Editor */}
          <div className="space-y-1.5">
            <div className="flex items-center justify-between">
              <span className="text-[11px] uppercase font-sans text-secondary font-semibold flex items-center gap-1.5">
                <Code2 className="w-3.5 h-3.5" />
                {isNarrowing ? '编辑并收紧参数 (JSON)' : '结构化工具调用载荷'}
              </span>
              <button
                id="toggle-narrow-mode-btn"
                onClick={() => setIsNarrowing(!isNarrowing)}
                className="text-[11px] text-accent hover:text-accent-hover flex items-center gap-1 font-medium underline-offset-2 hover:underline"
              >
                <Sliders className="w-3 h-3" />
                {isNarrowing ? '取消收紧' : '收紧权限范围 (Narrow)'}
              </button>
            </div>

            {isNarrowing ? (
              <div className="space-y-2">
                <textarea
                  id="narrow-parameters-textarea"
                  value={narrowedJsonText}
                  onChange={(e) => {
                    setNarrowedJsonText(e.target.value);
                    setJsonError(null);
                  }}
                  rows={6}
                  className="w-full p-3 rounded-xl bg-white border border-accent-soft font-mono text-xs text-primary focus:outline-none focus:ring-1 focus:ring-accent"
                />
                {jsonError && (
                  <p className="text-red-dark text-[11px] font-mono">{jsonError}</p>
                )}
                <div className="flex items-center justify-end gap-2">
                  <button
                    id="submit-narrowed-btn"
                    onClick={handleNarrowSubmit}
                    disabled={isExpired}
                    className="px-3.5 py-1.5 rounded-full bg-accent hover:bg-accent-hover disabled:opacity-50 text-white text-xs font-medium flex items-center gap-1.5 transition-colors shadow-xs"
                  >
                    <Check className="w-3.5 h-3.5" />
                    应用收紧参数
                  </button>
                </div>
              </div>
            ) : (
              <pre className="p-3.5 rounded-2xl bg-well border border-hairline text-[11px] font-mono text-primary overflow-x-auto leading-relaxed max-h-48">
                {JSON.stringify(request.params, null, 2)}
              </pre>
            )}
          </div>

          {/* Expired warning */}
          {isExpired && (
            <div className="p-3.5 rounded-2xl bg-red-soft border border-red-soft text-red-dark text-xs flex items-center gap-2">
              <Clock className="w-4 h-4 text-red-dark" />
              <span>审批已超时关闭。必须重新发起评估方可继续执行。</span>
            </div>
          )}
        </div>

        {/* Action Buttons */}
        <div className="px-6 py-4 bg-canvas border-t border-hairline flex items-center justify-between gap-3 shrink-0">
          <div className="flex items-center gap-2">
            <button
              id="approval-deny-btn"
              onClick={() => onResolve(request.id, 'denied')}
              className="px-3.5 py-2 rounded-full bg-red-soft hover:bg-red-soft text-red-dark border border-red-soft font-medium text-xs flex items-center gap-1.5 transition-colors shadow-xs"
            >
              <XCircle className="w-4 h-4" />
              拒绝 (Deny)
            </button>
            <button
              id="approval-narrow-btn"
              onClick={() => setIsNarrowing(!isNarrowing)}
              disabled={isExpired}
              className="px-3.5 py-2 rounded-full bg-well hover:bg-black/[0.05] disabled:opacity-40 text-secondary border border-hairline font-medium text-xs flex items-center gap-1.5 transition-colors shadow-xs"
            >
              <Sliders className="w-4 h-4" />
              收紧范围
            </button>
          </div>

          <div className="flex items-center gap-2.5">
            <button
              id="approval-allow-always-btn"
              onClick={() => onResolve(request.id, 'always_allowed')}
              disabled={isExpired}
              className="px-3.5 py-2 rounded-full bg-accent-soft hover:bg-accent-soft text-accent border border-accent-soft disabled:opacity-40 font-medium text-xs flex items-center gap-1.5 transition-colors shadow-xs"
            >
              <CheckCheck className="w-4 h-4" />
              始终允许 (Always)
            </button>
            <button
              id="approval-allow-once-btn"
              onClick={() => onResolve(request.id, 'allowed_once')}
              disabled={isExpired}
              className="px-4.5 py-2 rounded-full bg-green hover:bg-green text-white disabled:opacity-40 font-medium text-xs flex items-center gap-1.5 transition-all shadow-xs"
            >
              <Check className="w-4 h-4" />
              仅允许本次 (Once)
            </button>
          </div>
        </div>
      </div>
    </div>
  );
};
