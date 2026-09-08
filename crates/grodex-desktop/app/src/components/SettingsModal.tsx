import React, { useState, useEffect } from 'react';
import {
  X,
  Sliders,
  Shield,
  Check,
  CheckCircle2,
  Cpu,
  Radio,
  FileCode,
  Terminal,
  Search,
  Globe,
  GitPullRequest,
  Save,
} from 'lucide-react';
import { PermissionRule, SettingsState, ToolName } from '../types';

interface SettingsModalProps {
  isOpen: boolean;
  onClose: () => void;
  settings: SettingsState;
  onSave: (newSettings: SettingsState) => void;
}

export const SettingsModal: React.FC<SettingsModalProps> = ({
  isOpen,
  onClose,
  settings,
  onSave,
}) => {
  const [currentSettings, setCurrentSettings] = useState<SettingsState>({ ...settings });
  const [toastMessage, setToastMessage] = useState<string | null>(null);

  useEffect(() => {
    if (isOpen) {
      setCurrentSettings({ ...settings });
      setToastMessage(null);
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

  const handleSave = () => {
    onSave(currentSettings);
    setToastMessage('配置已成功写入 ~/.grodex/config.toml');
    setTimeout(() => {
      setToastMessage(null);
      onClose();
    }, 1500);
  };

  return (
    <div id="settings-modal-backdrop" className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-black/30 backdrop-blur-sm animate-in fade-in duration-150">
      <div
        id="settings-modal-card"
        className="w-full max-w-2xl rounded-2xl border border-hairline bg-canvas shadow-2xl overflow-hidden flex flex-col max-h-[90vh]"
      >
        {/* Header */}
        <div className="flex items-center justify-between px-6 py-4 border-b border-hairline bg-white">
          <div className="flex items-center gap-2.5">
            <div className="p-2 rounded-2xl bg-accent-soft text-accent border border-accent-soft">
              <Sliders className="w-4 h-4" />
            </div>
            <div>
              <h3 className="text-sm font-bold text-primary">Agent 核心配置与工具权限</h3>
              <p className="text-xs text-secondary mt-0.5">管理推理模型提供商、ACP 通信协议以及细粒度工具安全审批策略</p>
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
        <div className="flex-1 overflow-y-auto p-6 space-y-6 text-xs text-primary">
          {/* Toast Notification */}
          {toastMessage && (
            <div
              id="settings-toast-banner"
              className="p-3.5 rounded-2xl bg-green-soft border border-green-soft text-green-dark text-xs font-medium flex items-center gap-2 animate-in fade-in slide-in-from-top-2"
            >
              <CheckCircle2 className="w-4 h-4 text-green-dark" />
              <span>{toastMessage}</span>
            </div>
          )}

          {/* Model Provider Choice */}
          <div className="space-y-2">
            <label className="text-[11px] uppercase font-sans text-secondary font-bold tracking-wider">
              AI 推理模型提供商
            </label>
            <div className="grid grid-cols-2 sm:grid-cols-4 gap-2.5">
              {[
                { id: 'anthropic', label: 'Anthropic', desc: 'Claude 3.7 Sonnet', recommended: true },
                { id: 'openai', label: 'OpenAI', desc: 'GPT-5 Coding', recommended: false },
                { id: 'deepseek', label: 'DeepSeek', desc: 'DeepSeek-V3 推理', recommended: false },
                { id: 'ollama', label: 'Ollama', desc: '本地 Llama 3.3', recommended: false },
              ].map((p) => {
                const isSelected = currentSettings.provider === p.id;
                return (
                  <button
                    key={p.id}
                    id={`provider-card-${p.id}`}
                    onClick={() => setCurrentSettings({ ...currentSettings, provider: p.id as any })}
                    className={`p-3.5 rounded-2xl border text-left transition-all ${
                      isSelected
                        ? 'border-accent bg-accent-soft ring-1 ring-accent/30 text-primary shadow-xs'
                        : 'border-hairline bg-white hover:border-hairline text-secondary hover:text-primary'
                    }`}
                  >
                    <div className="font-semibold text-xs text-primary flex items-center justify-between">
                      <span>{p.label}</span>
                      {isSelected && <Check className="w-3.5 h-3.5 text-accent" />}
                    </div>
                    <div className="text-[10px] text-secondary mt-1 font-sans">{p.desc}</div>
                  </button>
                );
              })}
            </div>
          </div>

          {/* Wire Protocol & Sandbox profile */}
          <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
            <div className="space-y-1.5">
              <label className="text-[11px] uppercase font-sans text-secondary font-bold tracking-wider">
                ACP 传输协议
              </label>
              <select
                id="select-wire-protocol"
                value={currentSettings.wireProtocol}
                onChange={(e) => setCurrentSettings({ ...currentSettings, wireProtocol: e.target.value as any })}
                className="w-full p-2.5 rounded-xl bg-white border border-hairline font-sans text-xs text-primary focus:outline-none focus:border-accent"
              >
                <option value="acp_stdio">ACP over stdio（原生推荐）</option>
                <option value="acp_websocket">ACP over WebSocket (ws://localhost:4040)</option>
                <option value="sse_direct">Server-Sent Events (Direct HTTP 流)</option>
              </select>
            </div>

            <div className="space-y-1.5">
              <label className="text-[11px] uppercase font-sans text-secondary font-bold tracking-wider">
                沙盒隔离模式
              </label>
              <select
                id="select-sandbox-profile"
                value={currentSettings.sandboxProfile}
                onChange={(e) => setCurrentSettings({ ...currentSettings, sandboxProfile: e.target.value as any })}
                className="w-full p-2.5 rounded-xl bg-white border border-hairline font-sans text-xs text-primary focus:outline-none focus:border-accent"
              >
                <option value="workspace">仅工作区 (标准开发沙盒)</option>
                <option value="readonly">严格只读 (禁止写文件与执行命令)</option>
                <option value="restricted">受限容器沙盒 (禁止外部网络访问)</option>
                <option value="full">完全宿主权限 (允许特权系统调用)</option>
              </select>
            </div>
          </div>

          {/* Granular Tool Permissions Table */}
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <label className="text-[11px] uppercase font-sans text-secondary font-bold tracking-wider flex items-center gap-1.5">
                <Shield className="w-3.5 h-3.5 text-accent" />
                细粒度工具执行策略
              </label>
              <span className="text-[11px] text-secondary">始终允许 (allow) / 弹窗审批 (ask) / 彻底禁用 (deny)</span>
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
              className="px-5 py-2 rounded-full bg-accent hover:bg-accent-hover text-white text-xs font-medium flex items-center gap-1.5 transition-colors shadow-xs"
            >
              <Save className="w-3.5 h-3.5" />
              保存配置
            </button>
          </div>
        </div>
      </div>
    </div>
  );
};
