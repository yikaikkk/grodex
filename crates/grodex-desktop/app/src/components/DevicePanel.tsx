import React, { useCallback, useEffect, useRef, useState } from 'react';
import { Smartphone, RefreshCw, Loader2, X, AlertTriangle } from 'lucide-react';
import * as acp from '../lib/acpClient';
import { AdbDevice } from '../lib/acpClient';

interface DevicePanelProps {
  isOpen: boolean;
  onClose: () => void;
}

const STATE_BADGE: Record<string, { label: string; cls: string }> = {
  device: { label: '在线', cls: 'bg-green-soft text-green-dark border-green-soft' },
  offline: { label: '离线', cls: 'bg-red-soft text-red-dark border-red-soft' },
  unauthorized: {
    label: '未授权',
    cls: 'bg-orange-soft text-orange-dark border-orange-soft',
  },
};

/**
 * Right-side device connection panel (docs/23). Shows adb-attached devices;
 * refreshes on open, every 5s while visible, and manually. Resident DOM is
 * unnecessary — data is one cheap `adb devices` call, so conditional mount
 * like the other side panels.
 */
const DevicePanelInner: React.FC<DevicePanelProps> = ({ isOpen, onClose }) => {
  const [devices, setDevices] = useState<AdbDevice[]>([]);
  const [toolsEnabled, setToolsEnabled] = useState<boolean>(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const gen = useRef(0);

  const load = useCallback(async () => {
    const seq = ++gen.current;
    setLoading(true);
    setError(null);
    try {
      const r = await acp.listAdbDevices();
      if (seq === gen.current) {
        setDevices(r.devices ?? []);
        setToolsEnabled(r.device_tools_enabled);
      }
    } catch (e: any) {
      if (seq === gen.current) setError(e?.message || String(e));
    } finally {
      if (seq === gen.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (!isOpen) return;
    load();
    const t = setInterval(load, 5000);
    return () => {
      clearInterval(t);
      gen.current += 1; // invalidate in-flight responses after close
    };
  }, [isOpen, load]);

  if (!isOpen) return null;

  return (
    <aside
      id="device-panel"
      className="w-80 sm:w-88 my-2 mr-2 sm:my-2.5 sm:mr-2.5 ml-1 sm:ml-1.5 rounded-2xl border border-hairline bg-white shadow-xs flex flex-col shrink-0 min-w-0 text-primary overflow-hidden select-none"
    >
      {/* Panel Header */}
      <div className="flex items-center justify-between px-4 py-3 border-b border-hairline bg-white shrink-0">
        <div className="flex items-center gap-2 min-w-0">
          <Smartphone className="w-4 h-4 text-accent shrink-0" />
          <h3 className="text-xs font-bold text-primary tracking-wide font-sans truncate">
            设备连接
          </h3>
          {loading && (
            <Loader2 className="w-3 h-3 text-tertiary animate-spin shrink-0" />
          )}
        </div>
        <div className="flex items-center gap-1 shrink-0">
          <button
            id="refresh-devices-btn"
            onClick={load}
            className="p-1 rounded-md hover:bg-canvas text-secondary hover:text-primary transition-colors"
            title="刷新设备列表"
          >
            <RefreshCw className="w-3.5 h-3.5" />
          </button>
          <button
            id="close-device-btn"
            onClick={onClose}
            className="p-1 rounded-md hover:bg-canvas text-secondary hover:text-primary transition-colors"
            title="关闭面板"
          >
            <X className="w-4 h-4" />
          </button>
        </div>
      </div>

      {/* adb / config notice */}
      <div className="px-3.5 py-2 bg-well border-b border-hairline text-[11px] text-secondary flex items-start gap-2 shrink-0">
        <Smartphone className="w-3.5 h-3.5 text-accent shrink-0 mt-0.5" />
        <span>
          {toolsEnabled
            ? '手机控制已启用（phone_* 工具可用）。操作类工具执行前会弹窗审批。'
            : '手机控制未启用：在 ~/.grodex/config.toml 的 [device] 段设置 enabled = true 并重启会话后，phone_* 工具才可用。'}
        </span>
      </div>

      {/* Device list */}
      <div className="flex-1 overflow-y-auto [scrollbar-gutter:stable] p-3 space-y-2 min-w-0">
        {error && (
          <div className="px-3 py-3 rounded-xl bg-red-soft text-red-dark text-[11px] flex items-start gap-2">
            <AlertTriangle className="w-3.5 h-3.5 shrink-0 mt-0.5" />
            <span className="min-w-0 break-all">{error}</span>
          </div>
        )}
        {!error && devices.length === 0 && !loading && (
          <div className="px-2 py-6 text-[11px] text-tertiary text-center">
            未检测到设备。请确认 USB 调试已开启，或使用 adb connect 连接无线设备。
          </div>
        )}
        {devices.map((d) => {
          const badge = STATE_BADGE[d.state] ?? {
            label: d.state,
            cls: 'bg-well text-secondary border-hairline',
          };
          const online = d.state === 'device';
          return (
            <div
              key={d.serial}
              id={`adb-device-${d.serial}`}
              className="w-full min-w-0 rounded-xl border border-hairline bg-white shadow-2xs p-3 space-y-1.5 transition-colors"
            >
              <div className="flex items-center justify-between gap-2 min-w-0">
                <div className="flex items-center gap-1.5 min-w-0 flex-1">
                  <Smartphone
                    className={`w-3.5 h-3.5 shrink-0 ${
                      online ? 'text-accent' : 'text-tertiary'
                    }`}
                  />
                  <span className="text-xs font-semibold text-primary truncate">
                    {d.model || '未知型号'}
                  </span>
                </div>
                <span
                  className={`px-1.5 py-0.5 rounded-full border text-[10px] font-medium shrink-0 ${badge.cls}`}
                >
                  {badge.label}
                </span>
              </div>
              <div
                className="text-[10px] font-mono text-tertiary truncate"
                title={d.serial}
              >
                {d.serial}
              </div>
            </div>
          );
        })}
      </div>
    </aside>
  );
};

/** Memoized: refresh cadence is internal; streaming frames skip it. */
export const DevicePanel = React.memo(DevicePanelInner);
