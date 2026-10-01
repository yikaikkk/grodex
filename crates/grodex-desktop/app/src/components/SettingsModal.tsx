import React, { useState, useEffect } from 'react';
import {
  X,
  Sliders,
  Shield,
  CheckCircle2,
  FileCode,
  Terminal,
  Search,
  Globe,
  GitPullRequest,
  Save,
  Loader2,
} from 'lucide-react';
import { PermissionRule, SettingsState, ToolName } from '../types';
import { approvalModeLabel, permissionsToMode } from '../lib/approval';

interface SettingsModalProps {
  isOpen: boolean;
  onClose: () => void;
  settings: SettingsState;
  onSave: (newSettings: SettingsState) => Promise<void>;
}

const SANDBOX_LABELS: Record<SettingsState['sandboxProfile'], string> = {
  workspace: '仅工作区（标准开发沙盒）',
  readonly: '严格只读（禁止写文件与执行命令）',
  restricted: '受限容器沙盒（禁止外部网络访问）',
  full: '完全宿主权限（允许特权系统调用）',
};

const SettingsModalInner: React.FC<SettingsModalProps> = ({
  isOpen,
  onClose,
  settings,
  onSave,
}) => {
  const [currentSettings, setCurrentSettings] = useState<SettingsState>({ ...settings });
  const [toastMessage, setToastMessage] = useState<string | null>(null);
  const [isSaving, setIsSaving] = useState(false);

  useEffect(() => {
    if (isOpen) {
      setCurrentSettings({ ...settings });
      setToastMessage(null);
      setIsSaving(false);
    }
  }, [isOpen, settings]);

  if (!isOpen) return null;

  const toolsList: { name: ToolName; label: string; desc: string; icon: React.ReactNode }[] = [
    { name: 'read_file', label: 'read_file', desc: '读取工作区源码与配置文件', icon: <FileCode className="w-3.5 h-3.5 text-accent" /> },
    { name: 'write_file', label: 'write_file', desc: '在磁盘上创建新文件', icon: <FileCode className="w-3.5 h-3.5 text-orange" /> },
    { name: 'edit_file', label: 'edit_file', desc: '替换现有文件的代码块与修改', icon: <FileCode className="w-3.5 h-3.5 text-orange" /> },
    { name: 'exec', label: 'exec', desc: '在容器环境中执行 Shell 命令与测试', icon: <Terminal className="w-3.5 h-3.5 text-green" /> },
    { name: 'apply_patch', label: 'apply_patch', desc: '将 Unified Git Patch 补丁应用到工作树', icon: <GitPullRequest className="w-3.5 h-3.5 text-accent" /> },
    { name: 'web_fetch', label: 'web_fetch', desc: '抓取外部开发文档与 Crate 元数据', icon: <Globe className="w-3.5 h-3.5 text-accent" /> },
    { name: 'grep', label: 'grep', desc: '在工作区内执行正则内容搜索', icon: <Search className="w-3.5 h-3.5 text-secondary" /> },
    { name: 'glob', label: 'glob', desc: '按 Glob 模式列出匹配文件路径', icon: <Search className="w-3.5 h-3.5 text-secondary" /> },
  ];

  const handlePermissionChange = (tool: ToolName, rule: PermissionRule) => {
    setCurrentSettings((prev) => ({
      ...prev,
      permissions: {
        ...prev.permissions,
        [tool]: rule,
      },
    }));
  };

  const handleSave = async () => {
    setIsSaving(true);
    try {
      await onSave(currentSettings);
      setToastMessage('权限配置已成功写入 ~/.grodex/config.toml');
      setIsSaving(false);
      setTimeout(() => setToastMessage(null), 2000);
    } catch (e: any) {
      setToastMessage(`保存失败: ${e?.message || '未知错误'}`);
      setIsSaving(false);
      setTimeout(() => setToastMessage(null), 3000);
    }
  };

  // Read-only rows reflect what the backend actually reads from
  // ~/.grodex/config.toml (sessions::get_config). The former provider /
  // wire-protocol / sandbox pickers were mock UI (never persisted) and are
  // removed — these values are only editable via the config file.
  const configRows: { label: string; value: string; mono?: boolean }[] = [
    { label: '模型提供商', value: currentSettings.provider, mono: true },
    { label: '模型', value: currentSettings.model, mono: true },
    { label: 'ACP 传输协议', value: 'ACP over stdio（唯一支持的传输）' },
    {
      label: '沙盒隔离模式',
      value: SANDBOX_LABELS[currentSettings.sandboxProfile] ?? currentSettings.sandboxProfile,
    },
  ];

  return (
    <div id="settings-modal-backdrop" className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/50">
      <div
        id="settings-modal-card"
        className="w-full max-w-5xl h-[85vh] rounded-2xl border border-hairline bg-canvas shadow-2xl overflow-hidden flex flex-col"
      >
        {/* Header */}
        <div className="flex items-center justify-between px-6 py-4 border-b border-hairline bg-white">
          <div className="flex items-center gap-2.5">
            <div className="p-2 rounded-2xl bg-accent-soft text-accent border border-accent-soft">
              <Sliders className="w-4 h-4" />
            </div>
            <div>
              <h3 className="text-sm font-bold text-primary">Agent 核心配置与工具权限</h3>
              <p className="text-xs text-secondary mt-0.5">生效配置只读展示；工具审批策略在此直接编辑并热加载</p>
            </div>
          </div>
          <button
            id="close-settings-btn"
            onClick={onClose}
            className="p-1.5 rounded-full hover:bg-black/[0.05] text-secondary hover:text-primary transition-colors"
          >
            <X className="w-4 h-4" />
          </button>
        </div>

        {/* Content */}
        <div className="flex-1 overflow-y-auto p-6 space-y-6 text-xs text-primary [scrollbar-gutter:stable]">
          {/* Toast Notification */}
          {toastMessage && (
            <div
              id="settings-toast-banner"
              className="p-3.5 rounded-2xl bg-green-soft border border-green-soft text-green-dark text-xs font-medium flex items-center gap-2"
            >
              <CheckCircle2 className="w-4 h-4 text-green-dark" />
              <span>{toastMessage}</span>
            </div>
          )}

          {/* Effective config — read-only, real values from config.toml */}
          <div className="space-y-2">
            <label className="text-[11px] uppercase font-sans text-secondary font-bold tracking-wider">
              生效配置（只读，来源 ~/.grodex/config.toml）
            </label>
            <div className="rounded-2xl border border-hairline bg-white overflow-hidden divide-y divide-hairline-2 shadow-xs">
              {configRows.map((row) => (
                <div key={row.label} className="flex items-center justify-between px-4 py-3">
                  <span className="text-secondary">{row.label}</span>
                  <span className={`text-primary font-medium ${row.mono ? 'font-mono' : 'font-sans'}`}>
                    {row.value}
                  </span>
                </div>
              ))}
            </div>
            <p className="text-[11px] text-tertiary">
              如需更换模型或提供商，请直接编辑 ~/.grodex/config.toml 后重启会话。
            </p>
          </div>

          {/* Granular Tool Permissions Table */}
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <label className="text-[11px] uppercase font-sans text-secondary font-bold tracking-wider flex items-center gap-1.5">
                <Shield className="w-3.5 h-3.5 text-accent" />
                细粒度工具执行策略
              </label>
              <span className="text-[11px] text-secondary">
                当前模式：
                <span className="text-accent font-semibold">
                  {approvalModeLabel(permissionsToMode(currentSettings.permissions))}
                </span>
                {' · '}始终允许 (allow) / 弹窗审批 (ask) / 彻底禁用 (deny)
              </span>
            </div>

            <div className="rounded-2xl border border-hairline bg-white overflow-hidden divide-y divide-hairline-2 shadow-xs">
              {toolsList.map((tool) => {
                const currentRule = currentSettings.permissions[tool.name] || 'ask';

                return (
                  <div
                    key={tool.name}
                    className="flex items-center justify-between px-4 py-3 hover:bg-well transition-colors"
                  >
                    <div className="flex items-center gap-2.5">
                      <div className="p-1.5 rounded-xl bg-well border border-hairline">
                        {tool.icon}
                      </div>
                      <div>
                        <div className="font-mono font-bold text-xs text-primary">
                          {tool.label}
                        </div>
                        <div className="text-[10px] text-secondary">{tool.desc}</div>
                      </div>
                    </div>

                    <div className="flex items-center gap-1.5">
                      {(['allow', 'ask', 'deny'] as PermissionRule[]).map((rule) => {
                        const isSelected = currentRule === rule;
                        const ruleLabel = rule === 'allow' ? '允许' : rule === 'ask' ? '审批' : '拒绝';
                        return (
                          <button
                            key={rule}
                            id={`perm-${tool.name}-${rule}`}
                            onClick={() => handlePermissionChange(tool.name, rule)}
                            className={`px-3 py-1 rounded-full text-[11px] font-sans font-medium transition-all ${
                              isSelected
                                ? rule === 'allow'
                                  ? 'bg-green-soft text-green-dark border border-green-soft shadow-xs'
                                  : rule === 'ask'
                                  ? 'bg-orange-soft text-orange-dark border border-orange-soft shadow-xs'
                                  : 'bg-red-soft text-red-dark border border-red-soft shadow-xs'
                                : 'bg-well text-secondary hover:text-primary border border-hairline'
                            }`}
                          >
                            {ruleLabel}
                          </button>
                        );
                      })}
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        </div>

        {/* Footer */}
        <div className="px-6 py-4 border-t border-hairline bg-white flex items-center justify-between">
          <span className="text-[11px] font-mono text-secondary">
            配置文件目标：~/.grodex/config.toml
          </span>
          <div className="flex items-center gap-2">
            <button
              id="cancel-settings-btn"
              onClick={onClose}
              className="px-4 py-2 rounded-full hover:bg-black/[0.05] text-secondary text-xs font-medium transition-colors"
            >
              取消
            </button>
            <button
              id="save-settings-btn"
              onClick={handleSave}
              disabled={isSaving}
              className="px-5 py-2 rounded-full bg-accent hover:bg-accent-hover text-white text-xs font-medium flex items-center gap-1.5 transition-colors shadow-xs disabled:opacity-70 disabled:hover:bg-accent"
            >
              {isSaving ? (
                <Loader2 className="w-3.5 h-3.5 animate-spin" />
              ) : (
                <Save className="w-3.5 h-3.5" />
              )}
              {isSaving ? '保存中...' : '保存配置'}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
};

/** Memoized: open modal must not re-render on streaming frame flushes. */
export const SettingsModal = React.memo(SettingsModalInner);
