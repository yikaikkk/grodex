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
    <div id="approval-modal-backdrop" className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-[#21262d]/40 backdrop-blur-xs animate-in fade-in duration-150">
      <div
        id="approval-modal-card"
        className="w-full max-w-2xl rounded-3xl border border-[#ded8cd] bg-[#ffffff] shadow-2xl overflow-hidden flex flex-col max-h-[92vh]"
      >
        {/* Header with Circular Countdown Indicator */}
        <div className="flex items-center justify-between px-6 py-4.5 bg-[#faf8f4] border-b border-[#ece6dc] shrink-0">
          <div className="flex items-center gap-3">
            <div className="p-2 rounded-2xl bg-[#fef8ea] text-[#b07419] border border-[#f5dfb4]">
              <ShieldAlert className="w-5 h-5" />
            </div>
            <div>
              <h3 className="text-sm font-bold text-[#2b3036] flex items-center gap-2">
                需人工安全审批
                <span className="text-[11px] px-2.5 py-0.5 rounded-full bg-[#f4efe8] text-[#555d69] border border-[#e5dfd4] font-mono font-normal">
                  {request.toolName}
                </span>
              </h3>
              <p className="text-xs text-[#717782] mt-0.5">
                执行目标: <span className="font-mono text-[#3a3f45]">{request.target}</span>
              </p>
            </div>
          </div>

          {/* Countdown badge */}
          <div className="flex items-center gap-2">
            <div className="relative w-9 h-9 flex items-center justify-center">
              <svg className="w-full h-full transform -rotate-90" viewBox="0 0 36 36">
                <path
                  className="text-[#ebe5dc]"
                  strokeWidth="3"
                  stroke="currentColor"
                  fill="none"
                  d="M18 2.0845 a 15.9155 15.9155 0 0 1 0 31.831 a 15.9155 15.9155 0 0 1 0 -31.831"
                />
                <path
                  className={isExpired ? 'text-[#b83838]' : 'text-[#c7831d] transition-all duration-1000'}
                  strokeDasharray={`${progressPercent}, 100`}
                  strokeWidth="3"
                  strokeLinecap="round"
                  stroke="currentColor"
                  fill="none"
                  d="M18 2.0845 a 15.9155 15.9155 0 0 1 0 31.831 a 15.9155 15.9155 0 0 1 0 -31.831"
                />
              </svg>
              <span className={`absolute text-[11px] font-mono font-bold ${isExpired ? 'text-[#b83838]' : 'text-[#9c6514]'}`}>
                {remainingSec}s
              </span>
            </div>
          </div>
        </div>

        {/* Modal Body */}
        <div className="p-6 space-y-4 flex-1 min-h-0 overflow-y-auto text-xs">
          {/* Reason & Source Metadata */}
          <div className="p-3.5 rounded-2xl bg-[#fdf9f2] border border-[#f5dfb4] space-y-1.5">
            <div className="flex items-center gap-2 text-[#9c6514] font-semibold">
              <AlertTriangle className="w-4 h-4 text-[#c7831d] shrink-0" />
              <span>安全评估说明</span>
            </div>
            <p className="text-[#434952] text-xs leading-relaxed">
              {request.reason}
            </p>
            <div className="flex items-center gap-2 pt-1 text-[11px] text-[#717782]">
              <span>发起 Agent:</span>
              <span className="px-2 py-0.5 rounded-full bg-[#f2eee7] text-[#4a5f82] font-mono border border-[#e4ded3]">
                {request.sourceAgent}
              </span>
            </div>
          </div>

          {/* Structured Parameters Preview / Narrow Editor */}
          <div className="space-y-1.5">
            <div className="flex items-center justify-between">
              <span className="text-[11px] uppercase font-sans text-[#717782] font-semibold flex items-center gap-1.5">
                <Code2 className="w-3.5 h-3.5" />
                {isNarrowing ? '编辑并收紧参数 (JSON)' : '结构化工具调用载荷'}
              </span>
              <button
                id="toggle-narrow-mode-btn"
                onClick={() => setIsNarrowing(!isNarrowing)}
                className="text-[11px] text-[#2c5b96] hover:text-[#1d4373] flex items-center gap-1 font-medium underline-offset-2 hover:underline"
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
                  className="w-full p-3 rounded-xl bg-[#ffffff] border border-[#cbd8eb] font-mono text-xs text-[#243c5e] focus:outline-none focus:ring-1 focus:ring-[#4a5f82]"
                />
                {jsonError && (
                  <p className="text-[#b83838] text-[11px] font-mono">{jsonError}</p>
                )}
                <div className="flex items-center justify-end gap-2">
                  <button
                    id="submit-narrowed-btn"
                    onClick={handleNarrowSubmit}
                    disabled={isExpired}
                    className="px-3.5 py-1.5 rounded-full bg-[#4a5f82] hover:bg-[#3b4f70] disabled:opacity-50 text-white text-xs font-medium flex items-center gap-1.5 transition-colors shadow-xs"
                  >
                    <Check className="w-3.5 h-3.5" />
                    应用收紧参数
                  </button>
                </div>
              </div>
            ) : (
              <pre className="p-3.5 rounded-2xl bg-[#f7f4ed] border border-[#e5dfd5] text-[11px] font-mono text-[#2e343c] overflow-x-auto leading-relaxed max-h-48">
                {JSON.stringify(request.params, null, 2)}
              </pre>
            )}
          </div>

          {/* Expired warning */}
          {isExpired && (
            <div className="p-3.5 rounded-2xl bg-[#fdf2f2] border border-[#f8d2d2] text-[#b83838] text-xs flex items-center gap-2">
              <Clock className="w-4 h-4 text-[#b83838]" />
              <span>审批已超时关闭。必须重新发起评估方可继续执行。</span>
            </div>
          )}
        </div>

        {/* Action Buttons */}
        <div className="px-6 py-4 bg-[#faf8f4] border-t border-[#ece6dc] flex items-center justify-between gap-3 shrink-0">
          <div className="flex items-center gap-2">
            <button
              id="approval-deny-btn"
              onClick={() => onResolve(request.id, 'denied')}
              className="px-3.5 py-2 rounded-full bg-[#fdf2f2] hover:bg-[#fce6e6] text-[#b83838] border border-[#f8d2d2] font-medium text-xs flex items-center gap-1.5 transition-colors shadow-xs"
            >
              <XCircle className="w-4 h-4" />
              拒绝 (Deny)
            </button>
            <button
              id="approval-narrow-btn"
              onClick={() => setIsNarrowing(!isNarrowing)}
              disabled={isExpired}
              className="px-3.5 py-2 rounded-full bg-[#f4efe8] hover:bg-[#ece6dc] disabled:opacity-40 text-[#525964] border border-[#e4ded5] font-medium text-xs flex items-center gap-1.5 transition-colors shadow-xs"
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
              className="px-3.5 py-2 rounded-full bg-[#eef4fe] hover:bg-[#e2edfd] text-[#2c5b96] border border-[#d2e2f9] disabled:opacity-40 font-medium text-xs flex items-center gap-1.5 transition-colors shadow-xs"
            >
              <CheckCheck className="w-4 h-4" />
              始终允许 (Always)
            </button>
            <button
              id="approval-allow-once-btn"
              onClick={() => onResolve(request.id, 'allowed_once')}
              disabled={isExpired}
              className="px-4.5 py-2 rounded-full bg-[#256e2c] hover:bg-[#1e5824] text-white disabled:opacity-40 font-medium text-xs flex items-center gap-1.5 transition-all shadow-xs"
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
