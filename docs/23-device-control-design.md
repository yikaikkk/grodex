# 23. 设备控制（adb + uiautomator2）设计

> 状态：已评审待实现
> 路线：**grodex 原生 tool + Python sidecar（stdio JSON-RPC）**
> 备选路线 MCP 已评估并放弃（Rust 侧零代码但硬防线依赖 server 自觉、复用价值当前为 0）；HTTP sidecar 已评估并放弃（生命周期管理成本高）。

## 1. 目标

让 grodex 作为 AI 中枢控制 Android 手机：模型通过结构化工具完成「看屏幕 → 定位元素 → 点击/输入 → 验证结果」的闭环。核心设计原则：

1. **原始 UI XML 不进模型上下文**——hierarchy 100–300KB，必须压缩成 ~2KB 的元素索引表；
2. **所有操作经 tool 管道**——权限（allow/ask/deny）、审批弹窗、telemetry 审计零旁路；
3. **硬防线在 Rust 层**——支付类前台拦截、危险操作不提供工具，不依赖 Python 侧自觉。

## 2. 总体架构

```
模型 (grodex-loop)
  ↓ tool call: phone_click / phone_screen_elements / ...
grodex-tools/src/device.rs（Rust：schema + 权限 + 审计 + snapshot 校验兜底）
  ↓ spawn python -m device_gateway（懒启动）
    stdio，行分隔 JSON-RPC（带 id 多路复用），60s 空闲自动回收
device_gateway（Python：uiautomator2 + 元素压缩 + snapshot 缓存 + IME）
  ↓ adb（USB / wireless）
Android 设备（多台，serial 寻址；单设备 auto 模式）
```

选 stdio 而非 HTTP 的原因：复用 `grodex serve` 已验证的子进程托管模式——生命周期随父进程、崩溃自动 respawn、无端口冲突、无需鉴权。

## 3. stdio JSON-RPC 协议

- 帧：**行分隔 JSON**（`\n` 结尾），请求 `{"id": u64, "method": "...", "params": {...}}`，响应 `{"id": u64, "ok": true, "result": {...}}` / `{"id": u64, "ok": false, "error": {"code": "...", "message": "..."}}`；
- method 与下方工具一一对应（snake_case 同名）；
- Python 侧串行处理即可（u2 操作本身近串行）；id 用于超时治理与未来并发；
- Rust 侧客户端 `DeviceGatewayClient`：
  - 懒启动：首次工具调用 spawn，60s 无请求 kill；
  - crash 透明 respawn：下一次调用自动重启，对模型表现为一次可重试错误；
  - 每请求 15s 超时（dump 大屏可放宽到 30s）；
  - stdout 日志行（非 JSON）丢弃并 trace，不污染协议。

## 4. 工具清单

### 只读类（[rules] 默认建议 allow）

| 工具 | 入参 | 返回 |
|---|---|---|
| `phone_devices` | — | `[{serial, model, state}]` |
| `phone_screen_elements` | `serial?`, `max_elements?=120`, `clickable_only?=true` | `snapshot_id` + 压缩元素表（见 §5） |
| `phone_screenshot` | `serial?` | 本地 png 路径 + 宽高（**不回传 base64**；图像是可选增强，默认靠 XML 定位） |
| `phone_current_app` | `serial?` | `{package, activity}` |

### 操作类（[rules] 默认建议 ask）

| 工具 | 入参 | 说明 |
|---|---|---|
| `phone_click` | `snapshot_id`, `index`（首选）/ `x,y`（兜底） | index 校验 snapshot（§6） |
| `phone_swipe` | `serial?`, `direction`, `distance_pct?` | 语义化滚动 |
| `phone_input` | `serial?`, `rid?`, `index?`, `text`, `clear?` | 中文经 ADBKeyBoard |
| `phone_key` | `serial?`, `key`（back/home/enter…） | KEYCODE 白名单映射 |
| `phone_wait` | `serial?`, `text?`/`rid?`, `timeout?=10` | 轮询等元素，动画时序标配 |
| `phone_app_start` | `serial?`, `package`, `activity?` | |
| `phone_app_stop` | `serial?`, `package` | 高危，默认 ask |

不提供：卸载、清除数据、恢复出厂、`shell rm` —— 从工具面上就不存在。

## 5. 元素压缩格式（token 成败点）

`phone_screen_elements` 把 uiautomator2 的 dump_hierarchy 压成行式索引表：

```
snapshot: s_17a3  screen: 1080x2400  app: com.tencent.mm/.ui.LauncherUI
[1]  Button   "登录"            rid=com.tencent.mm/btn_login   (40,800,200,860)  clickable
[2]  EditText placeholder="搜索"  rid=…/search_tip               (60,120,1020,190) clickable
[3]  FrameLayout                                            (0,0,1080,120)    scrollable-horizontal
…共 87 个元素，显示交互节点 34 个
```

规则：默认只保留 clickable/scrollable/EditText/带文本的节点；剥掉 PixelX/Y、NAF 等噪声属性；超长 text 截断 40 字符；`max_elements` 硬上限 + 截断标记。单次输出预算 ≤3KB。

## 6. snapshot 过期校验（防呆核心）

- Rust 侧 `DeviceState` 记录每个 serial 的 `latest_snapshot_id`；
- `phone_click(index)` 要求 index 属于 latest snapshot；期间发生过新的 dump、旋转、前台切换（由 Rust 侧在每次 dump 前后比对 `app_current`）→ 判过期，返回结构化错误 `snapshot_stale`，提示模型重新 dump；
- 简化实现：过期状态由**调用序**推定（任何 phone_* 写操作成功后 snapshot 失效），不在 Python 侧维护状态机。

## 7. 安全防线（Rust 层，硬编码）

1. `phone_app_stop`、`phone_input`、`phone_click` 默认 ask（写入 builtin 默认 [rules] 文档与设置页工具表）；
2. **支付/银行包名黑名单**（可配置）：`com.alipay*`、`com.unionpay*`、`com.tencent.midas*` 等；前台命中黑名单时 `phone_click/phone_input/phone_swipe` 直接拒绝（错误信息含当前包名），并要求 ApprovalModal 级人工介入；
3. 工具结果统一截断（32KB 上限）防异常 XML 透传。

## 8. 配置（config.toml，复用热加载 watcher）

```toml
[device]
enabled = true
gateway_command = ["python3", "-m", "device_gateway"]  # 可指向 venv 绝对路径
working_dir = "/path/to/grodex-repo"   # python -m 解析 device_gateway 包的目录；
                                       # 省略 = 继承 serve 进程 cwd（需 pip 安装 device_gateway）
default_serial = "auto"        # 单设备自动选，多设备必须显式 serial
idle_timeout_secs = 60         # 空闲回收网关进程（下次调用自动 respawn）
payment_guard = true
payment_packages = ["com.alipay", "com.unionpay", "com.tencent.midas"]
```

`working_dir` 省略时自动发现：从 serve 进程 cwd、可执行文件位置向上查找
`device_gateway/__main__.py`，最后回退到编译期仓库根（dev 构建开箱即用）；
都找不到则启动报错并给出配置指引。网关进程的 stderr 会被捕获——启动即退出
（缺模块等）时直接把真实报错带回给模型，并对死亡进程自动重试一次。

网关未启动/设备离线 → 返回结构化错误（`gateway_unavailable` / `device_offline`），同类错误 60s 去重，禁止模型刷屏重试。

## 9. 文件落点

| 文件 | 内容 |
|---|---|
| `device_gateway/`（仓库根，Python 包） | stdio JSON-RPC server、u2 封装、压缩器、ADBKeyBoard 初始化 |
| `device_gateway/README.md` | 安装：`pip install -U uiautomator2`、`python -m uiautomator2 init` |
| `crates/grodex-tools/src/device.rs` | `DeviceGatewayClient`（stdio 客户端）+ 8 个 Tool 实现 + snapshot 校验 + 黑名单防线 |
| `crates/grodex-tools/src/registry.rs` | 注册 8 个工具 |
| `crates/grodex-config` | `[device]` 段解析 |
| 前端 `types.ts` + `SettingsModal` toolsList | 追加 8 个工具名（权限表可见） |

## 10. 分期与验收

| 阶段 | 内容 | 验收 |
|---|---|---|
| P0 | Python 网关（dump/click/input/key 四方法）+ README | 手动 stdio 会话完成「打开设置」 |
| P1 | Rust device.rs 全部 8 工具 + registry + 配置 + snapshot | 「打开设置→进入开发者选项」模型自主完成 |
| P2 | 黑名单防线、IM 初始化自检、压缩调优、设置页工具表 | 中文 App 流程、token ≤3KB/dump |
| P3 | 桌面设备指示器、小窗投屏、常用流程入 memory | — |

## 11. 风险

1. 厂商差异（MIUI 后台弹出权限、鸿蒙兼容）：网关 init 自检并输出报告；
2. 中文输入依赖 ADBKeyBoard：首次安装需设备端确认，网关启动自检其存在；
3. 时序：动画页必须 `phone_wait`——写入 system prompt 的工具使用说明；
4. uiautomator2 版本漂移：网关锁版本（requirements.txt）。
