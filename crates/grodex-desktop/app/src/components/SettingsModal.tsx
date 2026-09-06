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
    { name: 'read_file', label: 'read_file', desc: '读取工作区源码与配置文件', icon: <FileCode className="w-3.5 h-3.5 text-sky-400" /> },
    { name: 'write_file', label: 'write_file', desc: '在磁盘上创建新文件', icon: <FileCode className="w-3.5 h-3.5 text-amber-400" /> },
    { name: 'edit_file', label: 'edit_file', desc: '替换现有文件的代码块与修改', icon: <FileCode className="w-3.5 h-3.5 text-amber-400" /> },
    { name: 'exec', label: 'exec', desc: '在容器环境中执行 Shell 命令与测试', icon: <Terminal className="w-3.5 h-3.5 text-emerald-400" /> },
    { name: 'apply_patch', label: 'apply_patch', desc: '将 Unified Git Patch 补丁应用到工作树', icon: <GitPullRequest className="w-3.5 h-3.5 text-indigo-400" /> },
    { name: 'web_fetch', label: 'web_fetch', desc: '抓取外部开发文档与 Crate 元数据', icon: <Globe className="w-3.5 h-3.5 text-purple-400" /> },
    { name: 'grep', label: 'grep', desc: '在工作区内执行正则内容搜索', icon: <Search className="w-3.5 h-3.5 text-slate-400" /> },
    { name: 'glob', label: 'glob', desc: '按 Glob 模式列出匹配文件路径', icon: <Search className="w-3.5 h-3.5 text-slate-400" /> },
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
    <div id="settings-modal-backdrop" className="fixed inset-0 z-50 flex items-center justify-center p-4 bg-[#21262d]/40 backdrop-blur-xs animate-in fade-in duration-150">
      <div
        id="settings-modal-card"
        className="w-full max-w-2xl rounded-3xl border border-[#ded8cd] bg-[#faf9f7] shadow-2xl overflow-hidden flex flex-col max-h-[90vh]"
      >
        {/* Header */}
        <div className="flex items-center justify-between px-6 py-4 border-b border-[#ece6dc] bg-[#ffffff]">
          <div className="flex items-center gap-2.5">
            <div className="p-2 rounded-2xl bg-[#edf3fa] text-[#4a5f82] border border-[#d6e3f4]">
              <Sliders className="w-4 h-4" />
            </div>
            <div>
              <h3 className="text-sm font-bold text-[#282d33]">Agent 核心配置与工具权限</h3>
              <p className="text-xs text-[#6e7581] mt-0.5">管理推理模型提供商、ACP 通信协议以及细粒度工具安全审批策略</p>
            </div>
          </div>
          <button
            id="close-settings-btn"
            onClick={onClose}
            className="p-1.5 rounded-full hover:bg-[#f0ece5] text-[#717782] hover:text-[#282d33] transition-colors"
          >
            <X className="w-4 h-4" />
          </button>
        </div>

        {/* Content */}
        <div className="flex-1 overflow-y-auto p-6 space-y-6 text-xs text-[#383d45]">
          {/* Toast Notification */}
          {toastMessage && (
            <div
              id="settings-toast-banner"
              className="p-3.5 rounded-2xl bg-[#eef8ef] border border-[#cbe4cf] text-[#1c6422] text-xs font-medium flex items-center gap-2 animate-in fade-in slide-in-from-top-2"
            >
              <CheckCircle2 className="w-4 h-4 text-[#1c6422]" />
              <span>{toastMessage}</span>
            </div>
          )}

          {/* Model Provider Choice */}
          <div className="space-y-2">
            <label className="text-[11px] uppercase font-sans text-[#717782] font-bold tracking-wider">
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
                        ? 'border-[#4a5f82] bg-[#f0f4fa] ring-1 ring-[#4a5f82]/30 text-[#1e2632] shadow-xs'
                        : 'border-[#ded8cd] bg-[#ffffff] hover:border-[#cbbfb1] text-[#656c78] hover:text-[#282d33]'
                    }`}
                  >
                    <div className="font-semibold text-xs text-[#282d33] flex items-center justify-between">
                      <span>{p.label}</span>
                      {isSelected && <Check className="w-3.5 h-3.5 text-[#4a5f82]" />}
                    </div>
                    <div className="text-[10px] text-[#787f8c] mt-1 font-sans">{p.desc}</div>
                  </button>
                );
              })}
            </div>
          </div>

          {/* Wire Protocol & Sandbox profile */}
          <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
            <div className="space-y-1.5">
              <label className="text-[11px] uppercase font-sans text-[#717782] font-bold tracking-wider">
                ACP 传输协议
              </label>
              <select
                id="select-wire-protocol"
                value={currentSettings.wireProtocol}
                onChange={(e) => setCurrentSettings({ ...currentSettings, wireProtocol: e.target.value as any })}
                className="w-full p-2.5 rounded-xl bg-[#ffffff] border border-[#ded8cd] font-sans text-xs text-[#2c3138] focus:outline-none focus:border-[#4a5f82]"
              >
                <option value="acp_stdio">ACP over stdio（原生推荐）</option>
                <option value="acp_websocket">ACP over WebSocket (ws://localhost:4040)</option>
                <option value="sse_direct">Server-Sent Events (Direct HTTP 流)</option>
              </select>
            </div>

            <div className="space-y-1.5">
              <label className="text-[11px] uppercase font-sans text-[#717782] font-bold tracking-wider">
                沙盒隔离模式
              </label>
              <select
                id="select-sandbox-profile"
                value={currentSettings.sandboxProfile}
                onChange={(e) => setCurrentSettings({ ...currentSettings, sandboxProfile: e.target.value as any })}
                className="w-full p-2.5 rounded-xl bg-[#ffffff] border border-[#ded8cd] font-sans text-xs text-[#2c3138] focus:outline-none focus:border-[#4a5f82]"
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
              <label className="text-[11px] uppercase font-sans text-[#717782] font-bold tracking-wider flex items-center gap-1.5">
                <Shield className="w-3.5 h-3.5 text-[#4a5f82]" />
                细粒度工具执行策略
              </label>
              <span className="text-[11px] text-[#868d98]">始终允许 (allow) / 弹窗审批 (ask) / 彻底禁用 (deny)</span>
            </div>

            <div className="rounded-2xl border border-[#ded8cd] bg-[#ffffff] overflow-hidden divide-y divide-[#f0ece5] shadow-xs">
              {toolsList.map((tool) => {
                const currentRule = currentSettings.permissions[tool.name] || 'ask';

                return (
                  <div
                    key={tool.name}
                    className="flex items-center justify-between px-4 py-3 hover:bg-[#faf7f2] transition-colors"
                  >
                    <div className="flex items-center gap-2.5">
                      <div className="p-1.5 rounded-xl bg-[#f5f1ea] border border-[#e5dfd4]">
                        {tool.icon}
                      </div>
                      <div>
                        <div className="font-mono font-bold text-xs text-[#2b3036]">
                          {tool.label}
                        </div>
                        <div className="text-[10px] text-[#717782]">{tool.desc}</div>
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
                                  ? 'bg-[#eef8ef] text-[#1c6422] border border-[#cbe4cf] shadow-xs'
                                  : rule === 'ask'
                                  ? 'bg-[#fef8ea] text-[#935f12] border border-[#f5dfb4] shadow-xs'
                                  : 'bg-[#fdf0f0] text-[#b83838] border border-[#f8d4d4] shadow-xs'
                                : 'bg-[#f7f4ee] text-[#6d7480] hover:text-[#2c3138] border border-[#e8e2d6]'
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
        <div className="px-6 py-4 border-t border-[#ece6dc] bg-[#ffffff] flex items-center justify-between">
          <span className="text-[11px] font-mono text-[#787f8c]">
            配置文件目标：~/.grodex/config.toml
          </span>
          <div className="flex items-center gap-2">
            <button
              id="cancel-settings-btn"
              onClick={onClose}
              className="px-4 py-2 rounded-full hover:bg-[#f0ece5] text-[#616874] text-xs font-medium transition-colors"
            >
              取消
            </button>
            <button
              id="save-settings-btn"
              onClick={handleSave}
              className="px-5 py-2 rounded-full bg-[#4a5f82] hover:bg-[#3d5170] text-white text-xs font-medium flex items-center gap-1.5 transition-colors shadow-xs"
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
