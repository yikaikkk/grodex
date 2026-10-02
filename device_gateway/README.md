# device_gateway

grodex 设备控制网关（stdio JSON-RPC sidecar，封装 uiautomator2）。
设计文档：`docs/23-device-control-design.md`。

## 安装

```bash
pip install -r device_gateway/requirements.txt
python -m uiautomator2 init   # 向设备安装 atx-agent / ATX app（每台设备一次）
adb devices                   # 确认设备在线
```

中文输入依赖 ADBKeyBoard，首次 `phone_input` 时 uiautomator2 会自动安装，
设备上需手动允许一次。

## 手动测试（无需 grodex）

```bash
python -m device_gateway
{"id": 1, "method": "phone_devices", "params": {}}
{"id": 2, "method": "phone_screen_elements", "params": {}}
{"id": 3, "method": "phone_click", "params": {"snapshot_id": "s_1", "index": 1}}
```

## 协议

行分隔 JSON（详见设计文档 §3）。任何以 `#` 开头的行被忽略，便于人工调试。

## 注意

- `serial` 省略时为 auto 模式：仅允许恰好一台设备在线，多台时报
  `ambiguous_serial`；
- dump 原始 XML 不会出现在任何输出中，模型只见压缩后的元素索引表；
- 单条响应硬上限 32KB，超出返回 `result_too_large`。
