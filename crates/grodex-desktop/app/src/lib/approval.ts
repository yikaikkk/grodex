import { PermissionRule, SettingsState, ToolName } from '../types';

/**
 * The three coarse approval modes exposed by the composer pills. Each maps
 * onto a uniform permission rule set written to ~/.grodex/config.toml:
 *
 *   manual   → every tool `ask`   (审批弹窗)
 *   auto     → every tool `allow` (直接放行)
 *   disabled → every tool `deny`  (一律拒绝)
 *
 * `settings.permissions` stays the single source of truth: the pills derive
 * their highlighted mode from it, and SettingsModal edits flow back into it,
 * so per-tool changes surface as `custom` in the pills.
 */
export type ApprovalMode = 'manual' | 'auto' | 'disabled';
export type ApprovalModeOrCustom = ApprovalMode | 'custom';

export const ALL_TOOLS: ToolName[] = [
  'read_file',
  'write_file',
  'edit_file',
  'exec',
  'glob',
  'grep',
  'apply_patch',
  'web_fetch',
  'delegate_task',
];

export const APPROVAL_MODE_OPTIONS: { id: ApprovalMode; label: string; desc: string }[] = [
  { id: 'manual', label: '手动审批', desc: '所有工具执行前弹窗确认' },
  { id: 'auto', label: '自动允许', desc: '所有工具直接放行，不再弹窗' },
  { id: 'disabled', label: '禁用工具', desc: '所有工具一律拒绝执行' },
];

export function approvalModeLabel(mode: ApprovalModeOrCustom): string {
  if (mode === 'custom') return '自定义权限';
  return APPROVAL_MODE_OPTIONS.find((o) => o.id === mode)?.label ?? '自定义权限';
}

/** Uniform permission set for a mode: 手动审批→ask / 自动允许→allow / 禁用工具→deny. */
export function modeToPermissions(mode: ApprovalMode): SettingsState['permissions'] {
  const rule: PermissionRule = mode === 'auto' ? 'allow' : mode === 'manual' ? 'ask' : 'deny';
  return Object.fromEntries(ALL_TOOLS.map((t) => [t, rule])) as SettingsState['permissions'];
}

/** Inverse: derive the pill mode from a permission set. Mixed sets → 'custom'. */
export function permissionsToMode(p: SettingsState['permissions']): ApprovalModeOrCustom {
  const values = new Set(Object.values(p));
  if (values.size !== 1) return 'custom';
  const rule = values.values().next().value;
  if (rule === 'ask') return 'manual';
  if (rule === 'allow') return 'auto';
  if (rule === 'deny') return 'disabled';
  return 'custom';
}
