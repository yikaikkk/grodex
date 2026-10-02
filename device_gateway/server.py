"""stdio JSON-RPC server — one line per request, one line per response.

Methods (mirroring docs/23-device-control-design.md §4):
  phone_devices, phone_screen_elements, phone_screenshot, phone_current_app,
  phone_click, phone_click_xy, phone_swipe, phone_input, phone_key,
  phone_wait, phone_app_start, phone_app_stop

All device access goes through uiautomator2 (`import uiautomator2 as u2`).
Connections are cached per serial; `serial="auto"` picks the single
attached device and errors if several are present.
"""

from __future__ import annotations

import json
import re
import sys
import tempfile
import time
from typing import Any, Callable

from .compressor import compress

# uiautomator2 is imported lazily so the gateway can still boot (and report
# a clean installation error) on machines where it is not installed.
u2 = None


def _u2():
    global u2
    if u2 is None:
        try:
            import uiautomator2 as _u2
        except ImportError as e:  # noqa: BLE001
            raise GatewayError(
                "gateway_unavailable",
                f"uiautomator2 未安装：pip install -r device_gateway/requirements.txt ({e})",
            ) from e
        u2 = _u2
    return u2


_adbutils = None


def _adb():
    """adbutils handles raw adb listing; uiautomator2 3.x removed its
    module-level `adb` attribute, so listing via _u2().adb no longer works."""
    global _adbutils
    if _adbutils is None:
        try:
            import adbutils as _adb
        except ImportError as e:  # noqa: BLE001
            raise GatewayError(
                "gateway_unavailable",
                f"adbutils 未安装：pip install -r device_gateway/requirements.txt ({e})",
            ) from e
        _adbutils = _adb
    return _adbutils

MAX_RESULT_BYTES = 32 * 1024
KEY_CODES = {
    "back": 4,
    "home": 3,
    "enter": 66,
    "del": 67,
    "tab": 61,
    "volume_up": 24,
    "volume_down": 25,
    "power": 26,
    "recents": 187,
    "menu": 82,
}

_devices: dict[str, Any] = {}
# Last snapshots per device: snapshot_id → elements. The model only echoes
# the snapshot_id back for index-addressed clicks; element arrays never
# round-trip through the model context.
_snapshots: dict[str, dict] = {}
_snapshot_seq = 0
SNAPSHOTS_KEPT = 3


class GatewayError(Exception):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code
        self.message = message


def get_device(serial: str | None) -> Any:
    # NOTE: serials from wireless TLS/mDNS connections legitimately contain
    # single spaces — never reject or resanitize them; adb -s handles the
    # exact string fine.
    key = serial or "auto"
    if key in _devices:
        return _devices[key]
    if not serial:
        # `auto` only makes sense with exactly one device attached — check
        # BEFORE connecting so the ambiguity error is never masked by a
        # connect failure.
        serials = [serial for serial, _ in _adb_devices_raw()]
        if len(serials) > 1:
            raise GatewayError(
                "ambiguous_serial",
                f"检测到多台设备 {serials}，请显式指定 serial",
            )
        # Backfill: with exactly one device, connect EXPLICITLY by serial.
        # _u2().connect(None) delegates to adbutils device_list(), whose
        # whitespace splitting cannot parse serials containing spaces
        # (wireless TLS/mDNS) — the explicit path never touches that code.
        if len(serials) == 1:
            serial = serials[0]
            key = serial
    try:
        d = _u2().connect(serial) if serial else _u2().connect()
    except Exception as e:  # noqa: BLE001 — reported over the wire
        raise GatewayError("device_offline", f"无法连接设备 {key}: {e}") from e
    _devices[key] = d
    return d


def resolve_target(
    serial: str | None,
    index: int | None,
    snapshot_id: str | None,
    snapshot_obj: dict | None,
    rid: str | None,
    eid: str | None = None,
) -> tuple[Any, dict | None]:
    """Resolve an element target to (device, element).

    Index addressing validates against the snapshot that produced it:
    prefer snapshot_id (server-side store); a raw snapshot object is also
    accepted for backward compatibility. `eid` is a content-hash handle that
    survives across snapshots (searched globally).
    """
    if eid:
        for snap in _snapshots.values():
            for cand in [snap] + snap.get("history", []):
                for e in cand.get("elements", []):
                    if e.get("eid") == eid:
                        return get_device(serial), e
        raise GatewayError(
            "eid_not_found",
            f"eid={eid} 不在已知的屏幕快照中（页面可能已变化）。请重新 phone_screen_elements。",
        )
    if index is None:
        return get_device(serial), None
    elements: list | None = None
    if snapshot_id:
        key = serial or "auto"
        snap = _snapshots.get(key, {})
        if snap.get("id") == snapshot_id:
            elements = snap.get("elements")
        else:
            for h in snap.get("history", []):
                if h.get("id") == snapshot_id:
                    elements = h.get("elements")
                    break
        if elements is None:
            raise GatewayError(
                "snapshot_stale",
                f"snapshot {snapshot_id} 已过期或不存在，请重新调用 phone_screen_elements",
            )
    elif snapshot_obj:
        elements = snapshot_obj.get("elements")
    if elements is None:
        raise GatewayError(
            "snapshot_required",
            "index 寻址需要 snapshot_id（见 phone_screen_elements）",
        )
    for e in elements:
        if e["index"] == index:
            return get_device(serial), e
        start = e.get("index_start", e["index"])
        end = e.get("index_end", e["index"])
        if start <= index <= end and e.get("count"):
            raise GatewayError(
                "folded_item",
                f"index={index} 是折叠的列表展示项（共 {e['count']} 条，仅供阅读）。"
                "如需操作列表项，请滑动到可见位置后重新 phone_screen_elements。",
            )
    raise GatewayError(
        "snapshot_stale",
        f"index={index} 不在 snapshot 中，请重新调用 phone_screen_elements",
    )


def _store_snapshot(serial: str | None, elements: list) -> str:
    global _snapshot_seq
    _snapshot_seq += 1
    sid = f"s_{_snapshot_seq}"
    key = serial or "auto"
    old = _snapshots.get(key, {})
    history = [{"id": old["id"], "elements": old["elements"]}] if old else []
    history += old.get("history", [])[: SNAPSHOTS_KEPT - 2]
    _snapshots[key] = {"id": sid, "elements": elements, "history": history}
    return sid


def _keyboard_visible(d) -> bool:
    try:
        out = d.shell(
            "dumpsys input_method | grep -E 'mInputShown|mIsInputViewShown'"
        ).output
        return "true" in (out or "")
    except Exception:  # noqa: BLE001 — best-effort probe
        return False


def _current_app(d) -> dict:
    """Focused window from dumpsys — authoritative. u2's app_current() can
    return a STALE activity after fast app switches, which sent the model
    chasing the wrong app for entire turns (docs/23 real-world review)."""
    try:
        out = (
            d.shell("dumpsys window | grep -E 'mCurrentFocus|mFocusedApp'").output
            or ""
        )
        # 提取完整 component token（至少含一个 '/'），再规范化：
        # .Act → pkg.Act；com.other/com.act → 原样（有些 ROM 打印全路径）。
        m = re.search(r"u0 ([\w.]+(?:/[\w.$]+)+)", out) or re.search(
            r" ([\w.]+(?:/[\w.$]+)+)", out
        )
        if m:
            token = m.group(1)
            pkg, _, act = token.partition("/")
            if act.startswith("."):
                activity = pkg + act
            elif act.startswith(pkg):
                activity = act
            else:
                activity = token
            # activity 恒为完整形式（含包名）——调用方直接使用，不再拼接
            return {"package": pkg, "activity": activity}
    except Exception:  # noqa: BLE001
        pass
    try:
        app = d.app_current()
        pkg = app.get("package", "")
        act = app.get("activity", "")
        if act.startswith("."):
            act = pkg + act
        return {"package": pkg, "activity": act or f"{pkg}/"}
    except Exception:  # noqa: BLE001
        return {"package": "", "activity": ""}


def _current_ime(d) -> str:
    try:
        out = d.shell("dumpsys input_method | grep mCurMethodId").output or ""
        m = re.search(r"([\w.]+/[\w.]+)", out)
        return m.group(1) if m else ""
    except Exception:  # noqa: BLE001
        return ""


def _sq(s: str) -> str:
    """Single-quote for shell."""
    return "'" + s.replace("'", "'\\''") + "'"


def _injection_denied(e: Exception) -> bool:
    s = str(e)
    return "INJECT_EVENTS" in s or "permission" in s.lower()


# ── handlers ────────────────────────────────────────────────────────


def _adb_devices_raw() -> list[tuple[str, str]]:
    """Run `adb devices` and parse (serial, state) pairs.

    Delimiters are TAB or runs of 2+ spaces: wireless TLS/mDNS serials
    legitimately contain single spaces (e.g.
    'adb-xxxx (2)._adb-tls-connect._tcp'), so splitting on ANY whitespace —
    which adbutils device_list() does — corrupts them.
    """
    import re
    import shutil
    import subprocess

    adb_bin = shutil.which("adb")
    if not adb_bin:
        raise GatewayError("gateway_unavailable", "PATH 中找不到 adb 可执行文件")
    proc = subprocess.run(
        [adb_bin, "devices"], capture_output=True, text=True, timeout=10
    )
    pairs: list[tuple[str, str]] = []
    seen: set[str] = set()
    for line in (proc.stdout or "").splitlines()[1:]:
        line = line.rstrip("\r\n")
        if not line.strip():
            continue
        parts = re.split(r"\t|\s{2,}", line, maxsplit=1)
        if len(parts) != 2:
            continue
        serial = parts[0].strip()
        state = parts[1].strip()
        if not serial or serial in seen:
            continue
        seen.add(serial)
        pairs.append((serial, state))
    return pairs


def h_phone_devices(_params: dict) -> dict:
    out: list[dict] = []
    adb = _adb()
    for serial, state in _adb_devices_raw():
        info: dict = {"serial": serial, "state": state, "model": ""}
        try:
            model = adb.adb.device(serial).shell("getprop ro.product.model")
            if isinstance(model, list):
                model = "".join(model)
            info["model"] = str(model).strip()
        except Exception:  # noqa: BLE001 — model lookup is best-effort
            pass
        out.append(info)
    return {"devices": out}


def h_phone_screen_elements(params: dict) -> dict:
    d = get_device(params.get("serial"))
    ime = _current_ime(d)
    exclude = ["com.android.systemui"]
    if ime:
        exclude.append(ime.split("/")[0])  # 当前输入法的整个子树
    exclude += list(params.get("exclude_packages") or [])
    xml = d.dump_hierarchy()
    result = compress(
        xml,
        max_elements=int(params.get("max_elements", 120)),
        clickable_only=bool(params.get("clickable_only", True)),
        exclude_packages=exclude,
        offset=int(params.get("offset", 0)),
        text_contains=params.get("text_contains"),
        rid_contains=params.get("rid_contains"),
        class_contains=params.get("class_contains"),
        order=params.get("order", "visual"),
    )
    snapshot_id = _store_snapshot(params.get("serial"), result["elements"])
    focused = _current_app(d)   # dumpsys mCurrentFocus —— 权威，不复用旧值
    w, h = d.window_size()
    orientation = "横屏" if w > h else "竖屏"
    kb = _keyboard_visible(d)

    header = (
        f"snapshot={snapshot_id}  屏幕:{w}x{h}({orientation})  "
        f"focused_window={focused.get('activity', '')}"
    )
    if kb:
        header += "  ⚠ 软键盘已弹出，可能遮挡屏幕下半部分"
    if result.get("rid_prefix"):
        header += f"\nrid_package={result['rid_prefix']}/"

    # The model sees ONLY the table + metadata; element arrays stay in the
    # snapshot store so clicks address by snapshot_id + index.
    return {
        "snapshot_id": snapshot_id,
        "screen": f"{w}x{h}",
        "orientation": orientation,
        "keyboard_visible": kb,
        "focused_window": focused.get("activity", ""),
        "table": header + "\n" + result["table"],
        "shown": result["shown"],
        "total_nodes": result["total_nodes"],
        "truncated": result["truncated"],
    }


def h_phone_screenshot(params: dict) -> dict:
    """截屏。region={x,y,w,h} 裁剪；return_base64=true 时把 JPEG 以 base64
    内联返回——多模态模型可直接"看见"画面（a11y 树给不了的自绘价签/图片格子）。"""
    import base64
    import io

    d = get_device(params.get("serial"))
    img = d.screenshot().convert("RGB")

    region = params.get("region")
    if region:
        rx, ry = int(region.get("x", 0)), int(region.get("y", 0))
        rw, rh = int(region.get("w", 0)), int(region.get("h", 0))
        img = img.crop((rx, ry, min(rx + rw, img.width), min(ry + rh, img.height)))

    max_edge = int(params.get("max_edge", 720))
    if max(img.size) > max_edge:
        ratio = max_edge / max(img.size)
        img = img.resize((int(img.width * ratio), int(img.height * ratio)))

    quality = int(params.get("quality", 60))
    f = tempfile.NamedTemporaryFile(prefix="grodex_screen_", suffix=".jpg", delete=False)
    img.save(f.name, "JPEG", quality=quality)

    result = {"path": f.name, "width": img.width, "height": img.height}
    if params.get("return_base64"):
        bio = io.BytesIO()
        img.save(bio, "JPEG", quality=quality)
        b64 = base64.b64encode(bio.getvalue()).decode()
        result["image_base64"] = f"data:image/jpeg;base64,{b64}"
        result["base64_bytes"] = len(b64)
    return result


def h_phone_current_app(params: dict) -> dict:
    d = get_device(params.get("serial"))
    app = d.app_current()
    return {"package": app.get("package", ""), "activity": app.get("activity", "")}


def h_phone_click(params: dict) -> dict:
    d, e = resolve_target(
        params.get("serial"),
        params.get("index"),
        params.get("snapshot_id"),
        params.get("snapshot"),
        None,
        params.get("eid"),
    )
    if e is not None:
        cx, cy = e["center"]
    else:
        cx, cy = int(params.get("x", 0)), int(params.get("y", 0))
        if cx <= 0 and cy <= 0:
            raise GatewayError("bad_params", "需要 index+snapshot_id、eid 或 x,y")

    click_type = params.get("type", "tap")
    duration = int(params.get("duration_ms", 600)) / 1000.0
    verify = bool(params.get("verify", False))

    def _do():
        if click_type == "long_press":
            if hasattr(d, "long_click"):
                d.long_click(cx, cy, duration=duration)
            else:
                d.swipe(cx, cy, cx, cy, duration=duration)
        elif click_type == "double_tap":
            if hasattr(d, "double_click"):
                d.double_click(cx, cy)
            else:
                d.click(cx, cy)
                time.sleep(0.1)
                d.click(cx, cy)
        else:
            d.click(cx, cy)

    before_hash = None
    if verify:
        before_hash = hash(d.dump_hierarchy())
    try:
        _do()
    except Exception as ex:  # noqa: BLE001
        if _injection_denied(ex):
            raise GatewayError(
                "injection_denied",
                "触摸注入被系统拒绝（INJECT_EVENTS）。"
                "MIUI: 设置→更多设置→开发者选项→『USB 调试（安全设置）』打开后重试。",
            ) from ex
        raise

    result = {"clicked": [cx, cy], "type": click_type}
    if e is not None:
        result["element"] = e.get("text") or e.get("rid")
        result["eid"] = e.get("eid")
    if verify:
        time.sleep(0.6)
        after_hash = hash(d.dump_hierarchy())
        result["verified"] = "changed" if after_hash != before_hash else "no_change"
        if result["verified"] == "no_change":
            result["warning"] = "点击后画面无变化——可能点到了无效区域，或目标需要滚动到位。"
    return result


def h_phone_click_xy(params: dict) -> dict:
    d = get_device(params.get("serial"))
    x, y = int(params.get("x", 0)), int(params.get("y", 0))
    if x <= 0 and y <= 0:
        raise GatewayError("bad_params", "需要 x,y")
    d.click(x, y)
    return {"clicked": [x, y]}


def h_phone_swipe(params: dict) -> dict:
    d = get_device(params.get("serial"))
    direction = params.get("direction", "")
    pct = max(0.1, min(0.9, float(params.get("distance_pct", 0.5))))
    # 可选：在指定容器（index/rid）内滚动，而不是盲滚全屏
    container = None
    if params.get("index") is not None or params.get("rid"):
        _, container = resolve_target(
            params.get("serial"),
            params.get("index"),
            params.get("snapshot_id"),
            params.get("snapshot"),
            params.get("rid"),
        )
    if container is not None:
        x1, y1, x2, y2 = container["bounds"]
        cx = (x1 + x2) // 2
        cy = (y1 + y2) // 2
        span_x, span_y = (x2 - x1), (y2 - y1)
    else:
        w, hgt = d.window_size()
        cx, cy = w // 2, hgt // 2
        span_x, span_y = w, hgt
    deltas = {
        "up": (0, int(span_y * pct)),
        "down": (0, -int(span_y * pct)),
        "left": (int(span_x * pct), 0),
        "right": (-int(span_x * pct), 0),
    }
    if direction not in deltas:
        raise GatewayError("bad_params", f"direction 必须是 {list(deltas)}")
    dx, dy = deltas[direction]
    try:
        d.swipe(cx, cy, cx - dx, cy - dy, duration=float(params.get("duration", 0.3)))
    except Exception as ex:  # noqa: BLE001
        if _injection_denied(ex):
            raise GatewayError(
                "injection_denied",
                "触摸注入被系统拒绝（INJECT_EVENTS）。"
                "MIUI: 设置→更多设置→开发者选项→『USB 调试（安全设置）』打开后重试。",
            ) from ex
        raise
    where = f"容器 {container['rid'] or container['bounds']}" if container else "全屏"
    return {"swiped": direction, "distance_pct": pct, "within": where}


ADB_IME_ID = "com.android.adbkeyboard/.AdbIME"


def h_phone_input(params: dict) -> dict:
    d, e = resolve_target(
        params.get("serial"),
        params.get("index"),
        params.get("snapshot_id"),
        params.get("snapshot"),
        params.get("rid"),
        params.get("eid"),
    )
    text = str(params.get("text", ""))
    clear = bool(params.get("clear", True))
    if e is not None:
        d.click(*e["center"])
        time.sleep(0.3)
    if not text:
        return {"input": ""}

    ime = _current_ime(d)
    ascii_only = all(ord(c) < 128 for c in text)
    notes: list[str] = []
    switched_to: str | None = None
    try:
        if ascii_only:
            if clear:
                d.clear_text()
            d.shell(f"input text {_sq(text)}")
            return {"input": text, "via": "adb"}
        # 非 ASCII：必须经 ADBKeyBoard。当前不是 ADBKeyBoard 时临时切换、
        # 用后恢复（此前会静默失败且不报错）。
        if "adbkeyboard" not in ime.lower():
            installed = "adbkeyboard" in (
                (d.shell("ime list -s").output or "").lower()
            )
            if not installed:
                raise GatewayError(
                    "ime_unavailable",
                    f"当前 IME {ime or '未知'} 不支持中文注入，且设备未安装 "
                    "ADBKeyBoard（com.android.adbkeyboard）。请安装后重试。",
                )
            d.shell(f"ime enable {ADB_IME_ID}")
            d.shell(f"ime set {ADB_IME_ID}")
            switched_to = ime
            notes.append(f"IME 已临时切换：{ime} → ADBKeyBoard")
        d.send_keys(text, clear=clear)
        result = {"input": text, "via": "adbkeyboard"}
        if switched_to:
            result["notes"] = notes
            result["previous_ime"] = switched_to
        return result
    finally:
        if switched_to:
            d.shell(f"ime set {switched_to}")
            notes.append(f"IME 已恢复：{switched_to}")


def h_phone_key(params: dict) -> dict:
    d = get_device(params.get("serial"))
    key = params.get("key", "")
    if key == "home":
        d.press("home")
    elif key == "back":
        d.press("back")
    elif key == "recents":
        d.press("recent")
    elif key in KEY_CODES:
        d.shell(f"input keyevent {KEY_CODES[key]}")
    else:
        raise GatewayError("bad_params", f"key 必须是 {sorted(KEY_CODES) + ['home', 'back', 'recents']}")
    return {"key": key}


def h_phone_wait(params: dict) -> dict:
    d = get_device(params.get("serial"))
    timeout = float(params.get("timeout", 10))
    deadline = time.time() + timeout
    rid = params.get("rid")
    text = params.get("text")
    gone = bool(params.get("gone", False))
    stable_ms = int(params.get("stable_ms", 0))
    activity_contains = params.get("activity_contains")

    # 模式 1：画面稳定 N 毫秒（H5 渲染等待，替代猜 sleep）
    if stable_ms > 0:
        need = stable_ms / 1000.0
        last_hash = None
        stable_since = None
        while time.time() < deadline:
            h = hash(d.dump_hierarchy())
            now = time.time()
            if h == last_hash:
                if stable_since and now - stable_since >= need:
                    return {"stable": True, "waited": round(timeout - (deadline - now), 1)}
            else:
                last_hash = h
                stable_since = now
            time.sleep(0.4)
        return {"stable": False, "waited": timeout}

    # 模式 2：等 activity（应用切换/跳转完成）
    if activity_contains:
        while time.time() < deadline:
            app = _current_app(d)
            if activity_contains in app.get("activity", ""):
                return {"found": True, "activity": app["activity"]}
            time.sleep(0.5)
        return {"found": False, "waited": timeout}

    # 模式 3：等文本/rid 出现或消失
    if not rid and not text:
        raise GatewayError("bad_params", "需要 text/rid，或 stable_ms，或 activity_contains")
    while time.time() < deadline:
        xml = d.dump_hierarchy()
        present = (rid and rid in xml) or (text and text in xml)
        if gone:
            if not present:
                return {"gone": True, "waited": round(timeout - (deadline - time.time()), 1)}
        elif present:
            return {"found": True, "waited": round(timeout - (deadline - time.time()), 1)}
        time.sleep(0.5)
    return {("gone" if gone else "found"): False, "waited": timeout}


def h_phone_app_start(params: dict) -> dict:
    d = get_device(params.get("serial"))
    package = params.get("package", "")
    if params.get("uri") or params.get("action") or params.get("extras"):
        # Deep-link / action 路径 —— 本次实战中唯一可行的通道，一等公民化
        return h_phone_intent(params)
    if not package:
        raise GatewayError("bad_params", "需要 package（或 uri）")
    activity = params.get("activity")
    if activity:
        d.app_start(package, activity)
    else:
        d.app_start(package)
    landed = _current_app(d)
    return {"started": package, "landed": landed}


def _sq_cmd(parts: list[str]) -> str:
    return " ".join(_sq(str(p)) for p in parts)


def h_phone_intent(params: dict) -> dict:
    """Deep-link / intent 启动：am start -a ACTION -d URI -n cmp --es/--ei/--ez extras."""
    d = get_device(params.get("serial"))
    parts = ["am", "start"]
    if params.get("action"):
        parts += ["-a", params["action"]]
    if params.get("uri"):
        parts += ["-d", params["uri"]]
    if params.get("component"):
        parts += ["-n", params["component"]]
    if params.get("package"):
        parts += ["-p", params["package"]]
    for key, val in (params.get("extras") or {}).items():
        if isinstance(val, bool):
            parts += ["--ez", key, "true" if val else "false"]
        elif isinstance(val, int):
            parts += ["--ei", key, str(val)]
        else:
            parts += ["--es", key, str(val)]
    out = (d.shell(_sq_cmd(parts)).output or "").strip()
    m = re.search(r"cmp=(\S+)", out)
    ok = "Error" not in out and "Exception" not in out
    return {
        "ok": ok,
        "component": m.group(1) if m else None,
        "landed": _current_app(d),
        "output": out[:500],
    }


def h_phone_shell(params: dict) -> dict:
    """通用逃生舱：在设备上执行 shell 命令（serial 自动注入）。
    高危命令默认拒绝，allow_unsafe=true 显式放行。"""
    cmd = str(params.get("cmd") or params.get("command") or "").strip()
    if not cmd:
        raise GatewayError("bad_params", "需要 cmd")
    allow_unsafe = bool(params.get("allow_unsafe", False))
    unsafe_patterns = [
        r"\breboot\b", r"\brm\s+-rf\s+/(?!sdcard)", r"\bpm\s+uninstall\b",
        r"\bpm\s+clear\b", r"\bflash\w*\b", r"\bwipe\b", r"\bsetenforce\b",
    ]
    hit = next((p for p in unsafe_patterns if re.search(p, cmd)), None)
    if hit and not allow_unsafe:
        raise GatewayError(
            "unsafe_command",
            f"命令命中高危模式（{hit}）。确认确有必要时可用 allow_unsafe=true 重试。",
        )
    d = get_device(params.get("serial"))
    out = (d.shell(cmd).output or "")
    return {"cmd": cmd, "output": out[:8000]}


def h_phone_find(params: dict) -> dict:
    """按条件查找元素，只返回匹配项 —— 取代『全量 dump 反复翻找』。"""
    d = get_device(params.get("serial"))
    if not any(
        params.get(k) for k in ("text_contains", "rid_contains", "class_contains")
    ):
        raise GatewayError("bad_params", "至少提供一个过滤条件")
    ime = _current_ime(d)
    xml = d.dump_hierarchy()
    r = compress(
        xml,
        max_elements=500,
        clickable_only=False,
        text_contains=params.get("text_contains"),
        rid_contains=params.get("rid_contains"),
        class_contains=params.get("class_contains"),
        exclude_packages=["com.android.systemui"] + ([ime.split("/")[0]] if ime else []),
    )
    return {
        "count": r["shown"],
        "elements": [
            {
                "index": e["index"],
                "type": e["type"],
                "text": e["text"],
                "rid": e["rid"],
                "center": e["center"],
                "bounds": e["bounds"],
                "clickable": e["clickable"],
            }
            for e in r["elements"]
        ],
    }


def h_phone_scroll_to(params: dict) -> dict:
    """语义化滚动：滑动 + 检查目标文本，直到出现或耗尽次数。"""
    d = get_device(params.get("serial"))
    text = params.get("text")
    if not text:
        raise GatewayError("bad_params", "需要 text")
    direction = params.get("direction", "up")
    max_swipes = int(params.get("max_swipes", 6))
    last_hash = None
    for i in range(max_swipes):
        xml = d.dump_hierarchy() or ""
        if text in xml:
            return {"found": True, "swipes": i}
        h = hash(xml)
        h_phone_swipe(
            {
                "serial": params.get("serial"),
                "direction": direction,
                "distance_pct": 0.6,
            }
        )
        time.sleep(0.6)
        after = hash(d.dump_hierarchy() or "")
        if after == h:
            return {
                "found": False,
                "swipes": i + 1,
                "reached_end": True,
                "hint": "滑动后画面无变化——列表已到底，目标不在此列表中",
            }
        last_hash = after
    return {"found": False, "swipes": max_swipes, "hint": "尝试反方向、加大 max_swipes 或换关键词"}


def h_phone_capabilities(params: dict) -> dict:
    """一次性设备体检 —— 免去手工 getevent/id/dmesg 探查。"""
    d = get_device(params.get("serial"))

    def sh(c: str) -> str:
        return (d.shell(c).output or "").strip()

    ime = _current_ime(d)
    ime_list = sh("ime list -s")
    app = _current_app(d)
    w, h = d.window_size()
    return {
        "model": sh("getprop ro.product.model"),
        "sdk": sh("getprop ro.build.version.sdk"),
        "screen": f"{w}x{h}",
        "root": sh("id -u") == "0",
        "selinux": sh("getenforce") or "unknown",
        "current_ime": ime,
        "adbkeyboard_installed": "adbkeyboard" in ime_list.lower(),
        "focused_window": app.get("activity", ""),
    }


def h_phone_manifest(params: dict) -> dict:
    """深链考古：dumpsys package 过滤 Scheme / Activity / permission 行。"""
    d = get_device(params.get("serial"))
    package = params.get("package", "")
    if not package:
        raise GatewayError("bad_params", "需要 package")
    kind = params.get("kind", "schemes")
    keywords = {
        "schemes": ["Scheme"],
        "activities": ["Activity"],
        "permissions": ["permission"],
    }.get(kind)
    if not keywords:
        raise GatewayError("bad_params", f"kind 必须是 {list(keywords)}")
    out = d.shell(f"dumpsys package {_sq(package)}").output or ""
    lines = [
        ln.strip()
        for ln in out.splitlines()
        if any(k in ln for k in keywords)
    ]
    return {"package": package, "kind": kind, "lines": lines[:100]}


def h_phone_app_stop(params: dict) -> dict:
    d = get_device(params.get("serial"))
    package = params.get("package", "")
    if not package:
        raise GatewayError("bad_params", "需要 package")
    d.app_stop(package)
    return {"stopped": package}


def h_phone_webview_eval(params: dict) -> dict:
    """在 WebView 里执行 JS，返回结构化 JSON（H5 应用的正道）。
    前提：目标 WebView 开启了 setWebContentsDebuggingEnabled(true) ——
    release 构建通常关闭，此时返回明确的 webview_debugging_disabled。"""
    d = get_device(params.get("serial"))
    js = params.get("js")
    if not js:
        raise GatewayError("bad_params", "需要 js")
    if not hasattr(d, "webviews"):
        raise GatewayError(
            "webview_unsupported",
            "已安装的 uiautomator2 版本不支持 WebView 调试接口",
        )
    try:
        views = d.webviews
    except Exception as e:  # noqa: BLE001
        raise GatewayError(
            "webview_debugging_disabled",
            f"无法枚举 WebView：目标页需开启 setWebContentsDebuggingEnabled(true)，"
            f"release 构建通常关闭。({e})",
        ) from e
    if not views:
        raise GatewayError(
            "webview_not_found",
            "当前前台没有可调试的 WebView。深链考古可改用 phone_manifest + phone_intent 试错。",
        )
    try:
        wv = d.web(views[0]) if hasattr(d, "web") else None
        if wv is None:
            raise GatewayError(
                "webview_unsupported", "uiautomator2 未提供 web 执行接口"
            )
        return {"result": wv.eval_js(js), "webview": views[0]}
    except GatewayError:
        raise
    except Exception as e:  # noqa: BLE001
        raise GatewayError(
            "webview_eval_failed",
            f"JS 执行失败：{e}。注意 WebView 调试需开启 setWebContentsDebuggingEnabled(true)。",
        ) from e


def h_phone_intent_probe(params: dict) -> dict:
    """批量探测候选 deeplink：逐个 am start 试拉，回传各自落点。"""
    uris = params.get("uris") or []
    if not uris:
        raise GatewayError("bad_params", "需要 uris 数组")
    results = []
    for uri in uris:
        try:
            r = h_phone_intent({**params, "uri": uri, "action": params.get("action", "android.intent.action.VIEW")})
            results.append({"uri": uri, "ok": r["ok"], "landed": r["landed"]})
        except GatewayError as e:
            results.append({"uri": uri, "ok": False, "error": e.message})
    return {"results": results}


def h_phone_batch(params: dict) -> dict:
    """批处理：一次往返顺序执行多个已有 phone_* 方法。
    steps: [{"method": "phone_click", "params": {...}}, ...]
    任一步抛错即中止，返回已完成部分 + 错误。"""
    steps = params.get("steps") or []
    if not steps:
        raise GatewayError("bad_params", "需要 steps 数组")
    results = []
    for i, step in enumerate(steps):
        method = step.get("method", "")
        if method == "phone_batch":
            raise GatewayError("bad_params", "phone_batch 不支持嵌套")
        handler = HANDLERS.get(method)
        if handler is None:
            raise GatewayError(
                "unknown_method",
                f"第 {i} 步方法未知：{method}",
            )
        try:
            results.append({"method": method, "ok": True, "result": handler(step.get("params") or {})})
        except GatewayError as e:
            return {
                "results": results,
                "failed_at": i,
                "error": {"code": e.code, "message": e.message},
            }
    return {"results": results}


HANDLERS: dict[str, Callable[[dict], dict]] = {
    "phone_devices": h_phone_devices,
    "phone_screen_elements": h_phone_screen_elements,
    "phone_screenshot": h_phone_screenshot,
    "phone_current_app": h_phone_current_app,
    "phone_click": h_phone_click,
    "phone_click_xy": h_phone_click_xy,
    "phone_swipe": h_phone_swipe,
    "phone_input": h_phone_input,
    "phone_key": h_phone_key,
    "phone_wait": h_phone_wait,
    "phone_app_start": h_phone_app_start,
    "phone_app_stop": h_phone_app_stop,
    "phone_intent": h_phone_intent,
    "phone_shell": h_phone_shell,
    "phone_find": h_phone_find,
    "phone_scroll_to": h_phone_scroll_to,
    "phone_capabilities": h_phone_capabilities,
    "phone_manifest": h_phone_manifest,
    "phone_webview_eval": h_phone_webview_eval,
    "phone_intent_probe": h_phone_intent_probe,
    "phone_batch": h_phone_batch,
}

# ── stdio loop ──────────────────────────────────────────────────────


def _emit(obj: dict) -> None:
    line = json.dumps(obj, ensure_ascii=False)
    if len(line.encode("utf-8")) > MAX_RESULT_BYTES:
        obj = {
            k: v
            for k, v in obj.items()
            if k != "result"
        }
        obj["ok"] = False
        obj["error"] = {
            "code": "result_too_large",
            "message": f"结果超过 {MAX_RESULT_BYTES} 字节上限（compress 参数过宽？）",
        }
        line = json.dumps(obj, ensure_ascii=False)
    sys.stdout.write(line + "\n")
    sys.stdout.flush()


def dispatch(req: dict) -> None:
    rid = req.get("id")
    method = req.get("method", "")
    handler = HANDLERS.get(method)
    if handler is None:
        _emit(
            {
                "id": rid,
                "ok": False,
                "error": {"code": "unknown_method", "message": f"未知方法 {method}"},
            }
        )
        return
    try:
        result = handler(req.get("params") or {})
        _emit({"id": rid, "ok": True, "result": result})
    except GatewayError as e:
        _emit({"id": rid, "ok": False, "error": {"code": e.code, "message": e.message}})
    except Exception as e:  # noqa: BLE001 — reported over the wire
        _emit(
            {
                "id": rid,
                "ok": False,
                "error": {"code": "internal", "message": f"{type(e).__name__}: {e}"},
            }
        )


def serve() -> None:
    """Read line-delimited JSON-RPC requests from stdin until EOF."""
    for line in sys.stdin:
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError as e:
            _emit(
                {
                    "id": None,
                    "ok": False,
                    "error": {"code": "bad_frame", "message": f"JSON 解析失败: {e}"},
                }
            )
            continue
        dispatch(req)
