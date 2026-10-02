//! Device control tools — grodex as an AI hub for Android phones via
//! adb + uiautomator2. Design: docs/23-device-control-design.md.
//!
//! Architecture: a thin Python sidecar (`device_gateway`, stdio JSON-RPC)
//! wraps uiautomator2; these tools are the only sanctioned entry point.
//! Raw UI XML never crosses into the model context — the gateway compresses
//! hierarchies into index-addressed element tables and this layer enforces
//! snapshot validity, payment-package guard and result size caps.
//!
//! Transport: the sidecar is lazy-spawned (`[device] gateway_command` +
//! `working_dir` from config.toml, docs §8), line-delimited JSON-RPC with
//! request ids, idle-reaped after `idle_timeout_secs`, transparently
//! respawned on the next call after a crash.

use async_trait::async_trait;
use grodex_core::error::GrodexError;
use grodex_core::id::OperationId;
use grodex_core::policy::PolicyDecision;
use grodex_core::tool::{ConcurrencyClass, SideEffectClass, ToolMetadata};
use grodex_core::tool::{Tool, ToolRuntime};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

/// Locate the directory containing the `device_gateway` package: config
/// first, then walking up from the current dir / the executable, then the
/// compile-time repo root (dev builds). None → spawn will fail with a
/// clear, actionable error.
fn resolve_working_dir(configured: Option<&PathBuf>) -> Option<PathBuf> {
    if let Some(d) = configured {
        return Some(d.clone());
    }
    let marker = Path::new("device_gateway").join("__main__.py");
    let mut starts: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        starts.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            starts.push(dir.to_path_buf());
        }
    }
    // Dev-build fallback: the repo root at compile time.
    starts.push(
        PathBuf::from(
            env!("CARGO_MANIFEST_DIR")
                .trim_end_matches("crates/grodex-tools"),
        ),
    );
    for start in starts {
        let mut dir = Some(start);
        while let Some(d) = dir {
            if d.join(&marker).exists() {
                return Some(d);
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
    }
    None
}

fn stderr_tail(buf: &str) -> String {
    let tail: String = buf
        .lines()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    tail.chars().rev().take(1500).collect::<String>().chars().rev().collect()
}

/// Per-request timeout — dumps on large screens can be slow.
const CALL_TIMEOUT_SECS: u64 = 30;
/// Hard cap on any single tool result (bytes, UTF-8) — protects the model
/// context from a gateway anomaly dumping raw XML.
const MAX_RESULT_BYTES: usize = 32 * 1024;

/// Resolved `[device]` config section (docs/23 §8). The host (grodex-cli)
/// parses the merged config.toml into this; defaults match the doc.
#[derive(Debug, Clone)]
pub struct DeviceConfig {
    /// argv of the gateway, e.g. ["python3", "-m", "device_gateway"].
    pub gateway_command: Vec<String>,
    /// Working directory the gateway is spawned in — `python -m
    /// device_gateway` must resolve the package from here. None = inherit
    /// the serve process's cwd (requires device_gateway to be importable,
    /// e.g. pip-installed).
    pub working_dir: Option<PathBuf>,
    /// "auto" (single-device autoselect) or an explicit adb serial.
    pub default_serial: String,
    /// Kill the sidecar after this many idle seconds; respawned on demand.
    pub idle_timeout_secs: u64,
    /// Reject mutating phone tools while a payment app is in the foreground.
    pub payment_guard: bool,
    /// Package-name prefixes treated as payment/banking apps.
    pub payment_packages: Vec<String>,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            gateway_command: vec!["python3".into(), "-m".into(), "device_gateway".into()],
            working_dir: None,
            default_serial: "auto".into(),
            idle_timeout_secs: 60,
            payment_guard: true,
            payment_packages: vec![
                "com.alipay".into(),
                "com.unionpay".into(),
                "com.tencent.midas".into(),
                "com.eg.android.AlipayGphone".into(),
            ],
        }
    }
}

// ── Gateway client ──────────────────────────────────────────────────

struct GatewayInner {
    child: Option<Child>,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    /// Last lines of the gateway's stderr — surfaced when it dies so boot
    /// failures (missing module, bad interpreter) are self-explanatory.
    stderr_buf: Arc<tokio::sync::RwLock<String>>,
}

/// Shared stdio JSON-RPC client to the device_gateway sidecar.
/// Lazy-spawned on first call; a dead child is transparently respawned
/// on the next request; idle-reaped per config.
pub struct DeviceGateway {
    config: DeviceConfig,
    inner: Mutex<Option<GatewayInner>>,
    /// Bumped on every call — the idle reaper compares against it.
    activity_gen: AtomicU64,
    /// Weak self-handle so the idle reaper never keeps the gateway alive.
    self_weak: Mutex<Option<std::sync::Weak<DeviceGateway>>>,
}

impl DeviceGateway {
    pub fn new(config: DeviceConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(None),
            activity_gen: AtomicU64::new(0),
            self_weak: Mutex::new(None),
        }
    }

    /// Arc constructor that arms the idle reaper's weak self-handle.
    pub fn new_shared(config: DeviceConfig) -> std::sync::Arc<Self> {
        let arc = std::sync::Arc::new(Self::new(config));
        if let Ok(mut slot) = arc.self_weak.try_lock() {
            slot.replace(std::sync::Arc::downgrade(&arc));
        }
        arc
    }

    async fn spawn_locked(&self) -> Result<GatewayInner, GrodexError> {
        let (program, args) = self
            .config
            .gateway_command
            .split_first()
            .ok_or_else(|| {
                GrodexError::ToolExecution("[device] gateway_command 为空".to_string())
            })?;
        let working_dir = resolve_working_dir(self.config.working_dir.as_ref());
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = &working_dir {
            // PYTHONPATH so `-m device_gateway` resolves even if the caller's
            // cwd differs from the package location.
            let existing = std::env::var("PYTHONPATH").unwrap_or_default();
            let merged = format!("{}:{existing}", dir.display());
            cmd.env("PYTHONPATH", merged).current_dir(dir);
        }
        let mut child = cmd.spawn().map_err(|e| {
            GrodexError::ToolExecution(format!(
                "device_gateway 启动失败（{program} {args:?}）: {e}。\
                 请确认已安装 Python 与 uiautomator2，或在 ~/.grodex/config.toml \
                 [device] 段配置 gateway_command / working_dir。"
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| GrodexError::ToolExecution("gateway stdin 缺失".to_string()))?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| GrodexError::ToolExecution("gateway stdout 缺失".to_string()))?,
        );
        let stderr_buf: Arc<tokio::sync::RwLock<String>> =
            Arc::new(tokio::sync::RwLock::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            let sink = stderr_buf.clone();
            tokio::spawn(async move {
                let mut buf = String::new();
                let mut reader = BufReader::new(stderr);
                loop {
                    buf.clear();
                    match reader.read_line(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let mut w = sink.write().await;
                            if w.len() > 4096 {
                                *w = w[2048..].to_string();
                            }
                            w.push_str(&buf);
                        }
                    }
                }
            });
        }

        let mut inner = GatewayInner {
            child: Some(child),
            stdin,
            stdout,
            next_id: 1,
            stderr_buf,
        };

        // Boot check: a gateway that dies within 300ms is a startup failure
        // (missing module, bad interpreter). Surface its stderr directly
        // instead of letting the first request die on a broken pipe.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let mut child = inner.child.take();
        let status = match child.as_mut() {
            Some(c) => c.try_wait().ok().flatten(),
            None => None,
        };
        if let Some(status) = status {
            let stderr = inner.stderr_buf.read().await.clone();
            let tail = stderr_tail(&stderr);
            return Err(GrodexError::ToolExecution(format!(
                "device_gateway 启动即退出（{status}）。stderr:\n{tail}\n\n\
                 常见原因：device_gateway 包不在 PYTHONPATH（配置 [device] working_dir \
                 指向仓库根）或 uiautomator2 未安装。"
            )));
        }
        inner.child = child;
        Ok(inner)
    }

    /// One JSON-RPC round-trip. Holds the lock for the duration —
    /// uiautomator2 operations are effectively serial per device anyway.
    /// One JSON-RPC round-trip. Holds the lock for the duration —
    /// uiautomator2 operations are effectively serial per device anyway.
    /// Up to two attempts: if the gateway died since the last call (idle
    /// reaper race, crash, boot failure), respawn once and retry, surfacing
    /// the gateway's stderr on final failure.
    pub async fn call(&self, method: &str, mut params: Value) -> Result<Value, GrodexError> {
        // Inject the configured default serial when the caller omitted one.
        // "auto" is forwarded as an omission so the gateway autoselects.
        if let Some(obj) = params.as_object_mut() {
            if !obj.contains_key("serial") && self.config.default_serial != "auto" {
                obj.insert(
                    "serial".into(),
                    Value::String(self.config.default_serial.clone()),
                );
            }
        }

        let mut guard = self.inner.lock().await;

        let mut last_err: Option<GrodexError> = None;
        for attempt in 0..2u8 {
            // (Re)spawn lazily; a dead child is replaced transparently.
            let needs_spawn = match guard.as_ref().and_then(|g| g.child.as_ref()) {
                None => true,
                Some(child) => child.id().is_none(), // exited
            };
            if needs_spawn {
                *guard = Some(self.spawn_locked().await?);
            }
            let inner = guard.as_mut().expect("just spawned");

            let id = inner.next_id;
            inner.next_id += 1;
            let frame = serde_json::json!({"id": id, "method": method, "params": params});
            let mut line = frame.to_string();
            line.push('\n');

            // ── write ──
            let write = async {
                inner
                    .stdin
                    .write_all(line.as_bytes())
                    .await
                    .map_err(|e| {
                        GrodexError::ToolExecution(format!("gateway stdin 写入失败: {e}"))
                    })?;
                inner.stdin.flush().await.map_err(|e| {
                    GrodexError::ToolExecution(format!("gateway stdin flush 失败: {e}"))
                })
            };
            if let Err(e) = write.await {
                last_err = Some(e);
                continue; // dead pipe → respawn on next iteration
            }

            // ── read (with timeout) ──
            let read = async {
                loop {
                    let mut buf = String::new();
                    let n = inner
                        .stdout
                        .read_line(&mut buf)
                        .await
                        .map_err(|e| {
                            GrodexError::ToolExecution(format!(
                                "gateway stdout 读取失败: {e}"
                            ))
                        })?;
                    if n == 0 {
                        return Err(GrodexError::ToolExecution(
                            "gateway 已退出（EOF）".to_string(),
                        ));
                    }
                    let trimmed = buf.trim();
                    if trimmed.is_empty() || trimmed.starts_with('#') {
                        continue;
                    }
                    let resp: Value = serde_json::from_str(trimmed).map_err(|e| {
                        GrodexError::ToolExecution(format!("gateway 响应非 JSON: {e}"))
                    })?;
                    if resp.get("id").and_then(Value::as_u64) != Some(id) {
                        continue; // stale/foreign frame — skip
                    }
                    return if resp.get("ok").and_then(Value::as_bool) == Some(true) {
                        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
                    } else {
                        let err = resp.get("error").cloned().unwrap_or(Value::Null);
                        let code = err
                            .get("code")
                            .and_then(Value::as_str)
                            .unwrap_or("internal")
                            .to_string();
                        let msg = err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown gateway error")
                            .to_string();
                        Err(GrodexError::ToolExecution(format!("[{code}] {msg}")))
                    };
                }
            };

            match tokio::time::timeout(
                std::time::Duration::from_secs(CALL_TIMEOUT_SECS),
                read,
            )
            .await
            {
                Err(_) => {
                    return Err(GrodexError::ToolExecution(format!(
                        "gateway 调用 {method} 超时（{CALL_TIMEOUT_SECS}s）"
                    )));
                }
                Ok(Ok(result)) => {
                    // Result size cap — a gateway anomaly must not flood the
                    // model context. Screenshots get a larger budget: inline
                    // base64 (multimodal viewing) legitimately runs ~200KB.
                    let cap = if method == "phone_screenshot" {
                        256 * 1024
                    } else {
                        MAX_RESULT_BYTES
                    };
                    if result.to_string().len() > cap {
                        return Err(GrodexError::ToolExecution(format!(
                            "gateway 结果超过 {cap} 字节上限，请收窄请求参数"
                        )));
                    }

                    // Idle reaper: schedule a kill when the gateway stays
                    // unused; newer calls bump the generation and invalidate
                    // the pending reaper.
                    let idle = self.config.idle_timeout_secs;
                    if idle > 0 {
                        let gen_id = self.activity_gen.fetch_add(1, Ordering::SeqCst) + 1;
                        let weak: Option<std::sync::Weak<DeviceGateway>> =
                            self.self_weak.lock().await.clone();
                        if let Some(weak) = weak {
                            tokio::spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_secs(idle)).await;
                                if let Some(gw) = weak.upgrade() {
                                    if gw.activity_gen.load(Ordering::SeqCst) == gen_id {
                                        let mut guard = gw.inner.lock().await;
                                        if let Some(mut inner) = guard.take() {
                                            if let Some(mut child) = inner.child.take() {
                                                let _ = child.kill().await;
                                            }
                                        }
                                    }
                                }
                            });
                        }
                    }

                    return Ok(result);
                }
                Ok(Err(e)) => {
                    // Application-level error from the gateway — not a
                    // transport failure, do not retry.
                    return Err(e);
                }
            }
        }

        // Both attempts failed at the transport level — include stderr.
        let stderr = if let Some(inner) = guard.as_ref() {
            inner.stderr_buf.read().await.clone()
        } else {
            String::new()
        };
        let tail = stderr_tail(&stderr);
        Err(match last_err {
            Some(e) => GrodexError::ToolExecution(format!(
                "{e}\n{}",
                if tail.is_empty() {
                    String::new()
                } else {
                    format!("\ngateway stderr:\n{tail}")
                }
            )),
            None => GrodexError::ToolExecution(
                "device_gateway 不可用：请检查 [device] 配置与 Python 环境".to_string(),
            ),
        })
    }

    async fn foreground_package(&self) -> Result<String, GrodexError> {
        let r = self.call("phone_current_app", serde_json::json!({})).await?;
        Ok(r.get("package")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }

    /// Payment guard (docs/23 §7): mutating tools are rejected while a
    /// payment/banking app is in the foreground. Hard-coded at this layer —
    /// not bypassable by prompt injection into the gateway protocol.
    async fn payment_guard_check(&self, tool: &str) -> Result<(), GrodexError> {
        if !self.config.payment_guard || self.config.payment_packages.is_empty() {
            return Ok(());
        }
        let package = self.foreground_package().await?;
        for prefix in &self.config.payment_packages {
            if !prefix.is_empty() && package.starts_with(prefix.as_str()) {
                return Err(GrodexError::ToolExecution(format!(
                    "支付防线：检测到前台为支付/银行类应用（{package}），已拒绝执行 {tool}。\
                     请交由用户人工操作，或在 ~/.grodex/config.toml [device] 中调整 payment_packages。"
                )));
            }
        }
        Ok(())
    }
}

// ── Generic phone tool ──────────────────────────────────────────────

/// One parameterized tool per gateway method. Schemas are declared per tool
/// below; execution is a 1:1 forward to the gateway.
pub struct PhoneTool {
    gateway: std::sync::Arc<DeviceGateway>,
    def: PhoneToolDef,
}

#[derive(serde::Deserialize)]
pub struct PhoneToolDef {
    pub name: String,
    pub description: String,
    pub read_only: bool,
    pub input_schema: Value,
}

impl PhoneTool {
    pub fn new(gateway: std::sync::Arc<DeviceGateway>, def: PhoneToolDef) -> Self {
        Self { gateway, def }
    }
}

impl Tool for PhoneTool {
    type Args = Value;
    type Output = Value;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            name: self.def.name.clone(),
            display_name: self.def.name.clone(),
            description: self.def.description.clone(),
            concurrency_class: ConcurrencyClass::Serial,
            side_effect_class: if self.def.read_only {
                SideEffectClass::ReadOnly
            } else {
                SideEffectClass::NonIdempotent
            },
            default_policy: if self.def.read_only {
                PolicyDecision::Allow
            } else {
                PolicyDecision::Ask
            },
        }
    }

    fn input_schema(&self) -> Value {
        self.def.input_schema.clone()
    }

    fn output_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
}

#[async_trait]
impl ToolRuntime for PhoneTool {
    async fn execute(
        &self,
        args: Value,
        _operation_id: OperationId,
    ) -> Result<Value, GrodexError> {
        if !self.def.read_only {
            self.gateway.payment_guard_check(&self.def.name).await?;
        }
        self.gateway.call(&self.def.name, args).await
    }
}

// ── Tool definitions ────────────────────────────────────────────────

/// Build the full set of phone tools sharing one gateway client.
pub fn phone_tool_set(config: DeviceConfig) -> Vec<PhoneTool> {
    let gateway = std::sync::Arc::new(DeviceGateway::new(config));
    let defs: Vec<PhoneToolDef> = serde_json::from_value(serde_json::json!([
        {
            "name": "phone_devices",
            "description": "列出通过 adb 连接的 Android 设备（serial/model/state）。控制手机前先调用确认设备在线。",
            "read_only": true,
            "input_schema": {"type": "object", "properties": {}}
        },
        {
            "name": "phone_screen_elements",
            "description": "获取当前屏幕的压缩元素索引表。返回 snapshot_id 和 table（每行一个交互元素：[index] 类型 \"文本\" 坐标，含 disabled/焦点/软键盘警告等标记）。后续 phone_click 传 snapshot_id+index。屏幕变化后必须重新获取。",
            "read_only": true,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string", "description": "设备 serial，单设备可省略"},
                "max_elements": {"type": "integer", "description": "最多返回元素数，默认 120"},
                "clickable_only": {"type": "boolean", "description": "只返回可交互元素，默认 true"}
            }}
        },
        {
            "name": "phone_screenshot",
            "description": "截屏。return_base64=true 时图像内联返回（多模态模型可直接看：自绘价签/图片格子等 a11y 树给不了的信息）。region 裁剪局部。另有 path 供文件引用。",
            "read_only": true,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"},
                "quality": {"type": "integer", "description": "JPEG 质量，默认 60"},
                "max_edge": {"type": "integer", "description": "最长边缩放上限 px，默认 720"},
                "region": {"type": "object", "description": "裁剪区域 {x,y,w,h}（设备像素）"},
                "return_base64": {"type": "boolean", "description": "内联返回图像（多模态模型用）"}
            }}
        },
        {
            "name": "phone_current_app",
            "description": "返回当前前台应用包名与 activity。",
            "read_only": true,
            "input_schema": {"type": "object", "properties": {"serial": {"type": "string"}}}
        },
        {
            "name": "phone_click",
            "description": "点击屏幕元素。优先用 snapshot_id+index 寻址（来自 phone_screen_elements）；坐标 x,y 仅作兜底。屏幕变化后旧 index 会报 snapshot_stale，需重新获取。",
            "read_only": false,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"},
                "snapshot_id": {"type": "string", "description": "phone_screen_elements 返回的 snapshot_id"},
                "index": {"type": "integer", "description": "snapshot 中的元素 index"},
                "x": {"type": "integer"}, "y": {"type": "integer"}
            }}
        },
        {
            "name": "phone_swipe",
            "description": "滑动屏幕。direction: up/down/left/right（语义化为滚动方向，如 up=内容上滚）。列表滚动优先用它，配合 phone_wait 等待加载。",
            "read_only": false,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"},
                "direction": {"type": "string", "enum": ["up", "down", "left", "right"]},
                "distance_pct": {"type": "number", "description": "滑动距离占屏比，默认 0.5"},
                "duration": {"type": "number", "description": "滑动时长秒，默认 0.3"}
            }, "required": ["direction"]}
        },
        {
            "name": "phone_input",
            "description": "向输入框输入文本（支持中文，经 ADBKeyBoard）。给 index/rid 定位输入框并自动聚焦；clear=true 先清空。",
            "read_only": false,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"},
                "index": {"type": "integer"}, "rid": {"type": "string"},
                "eid": {"type": "string"},
                "text": {"type": "string"}, "clear": {"type": "boolean", "description": "默认 true"}
            }, "required": ["text"]}
        },
        {
            "name": "phone_key",
            "description": "按系统键。key ∈ back/home/enter/del/tab/volume_up/volume_down/power/recents/menu。",
            "read_only": false,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"},
                "key": {"type": "string"}
            }, "required": ["key"]}
        },
        {
            "name": "phone_wait",
            "description": "三种模式：① stable_ms=N 等画面稳定 N 毫秒（H5 渲染等待，替代猜 sleep）；② activity_contains=包名/关键字等应用跳转完成；③ text/rid 等元素出现，gone=true 等消失。点击/滑动后页面有动画时必须先 wait 再获取元素。",
            "read_only": true,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"},
                "text": {"type": "string"}, "rid": {"type": "string"},
                "gone": {"type": "boolean", "description": "等文本/rid 消失"},
                "stable_ms": {"type": "integer", "description": "画面稳定判定窗口 ms"},
                "activity_contains": {"type": "string", "description": "等目标 activity 关键字"},
                "timeout": {"type": "number", "description": "默认 10 秒"}
            }}
        },
        {
            "name": "phone_app_start",
            "description": "启动应用；支持 deeplink：传 uri（可加 action/extras）直接拉起目标页。返回实际落点 activity。",
            "read_only": false,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"},
                "package": {"type": "string"}, "activity": {"type": "string"},
                "uri": {"type": "string", "description": "deeplink，如 meituanapp://foodsearch/result?q=关键词"},
                "action": {"type": "string", "description": "如 android.intent.action.VIEW"},
                "extras": {"type": "object", "description": "键值 extras（字符串/整数/布尔）"}
            }}
        },
        {
            "name": "phone_app_stop",
            "description": "强制停止指定应用（高危操作）。",
            "read_only": false,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"},
                "package": {"type": "string"}
            }, "required": ["package"]}
        },
        {
            "name": "phone_intent",
            "description": "发 Intent（am start）：deeplink/action/component/extras 一等公民。返回实际落点 activity。先用 phone_manifest 查目标包的 Scheme。",
            "read_only": false,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"},
                "uri": {"type": "string", "description": "deeplink"},
                "action": {"type": "string"},
                "component": {"type": "string", "description": "pkg/activity 全名"},
                "package": {"type": "string", "description": "限定目标包"},
                "extras": {"type": "object", "description": "字符串/整数/布尔 extras"}
            }}
        },
        {
            "name": "phone_shell",
            "description": "在设备上执行任意 shell 命令（serial 自动注入）。高危命令（reboot/pm uninstall 等）默认拒绝。适合 dumpsys/getprop/logcat 诊断与探查。",
            "read_only": false,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"},
                "cmd": {"type": "string"},
                "allow_unsafe": {"type": "boolean", "description": "默认 false"}
            }, "required": ["cmd"]}
        },
        {
            "name": "phone_find",
            "description": "按 text/rid/class 条件查找元素，只返回匹配项（含 bounds 和跨调用稳定的 eid——可直接给 phone_click 用，不受 snapshot 刷新影响）。",
            "read_only": true,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"},
                "text_contains": {"type": "string"},
                "rid_contains": {"type": "string"},
                "class_contains": {"type": "string"}
            }}
        },
        {
            "name": "phone_scroll_to",
            "description": "语义化滚动：滑动并检查目标文本，直到找到、滑动无变化（reached_end=true，列表到底）或耗尽次数。",
            "read_only": false,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"},
                "text": {"type": "string", "description": "要滚动到的可见文本"},
                "direction": {"type": "string", "enum": ["up", "down", "left", "right"], "description": "默认 up"},
                "max_swipes": {"type": "integer", "description": "默认 6"}
            }, "required": ["text"]}
        },
        {
            "name": "phone_capabilities",
            "description": "设备一次性体检：root/selinux/当前输入法/ADBKeyBoard 是否安装/前台窗口/分辨率。操作手机前先调用，避免盲试。",
            "read_only": true,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"}
            }}
        },
        {
            "name": "phone_manifest",
            "description": "查询应用 manifest：kind=schemes 返回 URI Scheme（深链考古），activities 返回 Activity 列表。",
            "read_only": true,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"},
                "package": {"type": "string"},
                "kind": {"type": "string", "enum": ["schemes", "activities", "permissions"]}
            }, "required": ["package"]}
        },
        {
            "name": "phone_webview_eval",
            "description": "在 WebView 里执行 JS，返回结构化 JSON（H5 应用的结构化提取正道）。要求目标 WebView 开启 setWebContentsDebuggingEnabled(true)——release 构建通常关闭，会返回明确错误。",
            "read_only": true,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"},
                "js": {"type": "string", "description": "如 JSON.stringify(document.querySelectorAll('.item').map(e=>e.innerText))"}
            }, "required": ["js"]}
        },
        {
            "name": "phone_intent_probe",
            "description": "批量探测候选 deeplink：逐个试拉并回传各自落点 activity。配合 phone_manifest 的 schemes 使用。",
            "read_only": false,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"},
                "uris": {"type": "array", "items": {"type": "string"}, "description": "候选 uri 列表"},
                "action": {"type": "string", "description": "默认 android.intent.action.VIEW"}
            }, "required": ["uris"]}
        },
        {
            "name": "phone_batch",
            "description": "批处理：一次往返顺序执行多个 phone_* 步骤（如 tap→wait stable→dump），大幅减少往返次数。任一步失败即中止并返回已完成部分。",
            "read_only": false,
            "input_schema": {"type": "object", "properties": {
                "serial": {"type": "string"},
                "steps": {"type": "array", "items": {"type": "object", "properties": {
                    "method": {"type": "string", "description": "phone_* 方法名"},
                    "params": {"type": "object", "description": "该方法参数"}
                }, "required": ["method"]}}
            }, "required": ["steps"]}
        }
    ]))
    .expect("static phone tool defs");

    defs.into_iter()
        .map(|def| PhoneTool::new(gateway.clone(), def))
        .collect()
}
