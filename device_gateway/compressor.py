"""UI hierarchy compressor — raw XML never leaves this module.

Turns a uiautomator2 dump_hierarchy XML into a compact, index-addressed
element table. Design goals (docs/23-device-control-design.md):

Token reduction:
  - only interactive/labeled nodes survive the clickable_only filter
  - repeated list items (same class + rid, same parent) are folded into one
    line: `[12..31] ListItem ×20 "张三│李四│…"`
  - resource-id package prefix is stated once in the header, rows carry only
    the id tail
  - bounds → center + size (all the model needs to click)

Comprehension aids:
  - up to 4 levels of indentation to expose dialog/tab structure
  - flags for [disabled] [focus] [pwd] [scroll] [on]/[off]
  - a `── 对话框 ──` divider when dialog-class nodes appear
  - header lines: app, orientation, soft-keyboard warning, rid prefix

Indexes are stable within one snapshot; the server keeps the element arrays
so the model only ever echoes back the snapshot_id.
"""

from __future__ import annotations

import hashlib
import xml.etree.ElementTree as ET

MAX_TEXT_LEN = 40
MAX_DEPTH_INDENT = 4
FOLD_MIN_RUN = 3


def _clip(text: str) -> str:
    text = (text or "").strip().replace("\n", " ")
    return text[:MAX_TEXT_LEN] + ("…" if len(text) > MAX_TEXT_LEN else "")


def _bounds(node) -> tuple[int, int, int, int] | None:
    b = node.get("bounds", "")
    try:
        coords = b.strip("[]").replace("][", ",").split(",")
        x1, y1, x2, y2 = (int(v) for v in coords)
        return (x1, y1, x2, y2)
    except (ValueError, AttributeError):
        return None


def _fold_siblings(elements: list[dict]) -> list[dict]:
    """Collapse consecutive same-class/same-rid same-parent runs into one
    folded entry carrying up to 5 sample texts.

    Only NON-clickable runs are folded — clickable list items must keep
    their individual indexes or they could no longer be tapped separately.
    """
    folded: list[dict] = []
    i = 0
    n = len(elements)
    while i < n:
        e = elements[i]
        j = i
        if not e["clickable"]:
            while (
                j + 1 < n
                and not elements[j + 1]["clickable"]
                and elements[j + 1]["type"] == e["type"]
                and elements[j + 1]["rid_full"] == e["rid_full"]
                and elements[j + 1]["pid"] == e["pid"]
            ):
                j += 1
        run = elements[i : j + 1]
        if len(run) >= FOLD_MIN_RUN:
            texts = [r["text"] for r in run if r["text"]]
            first = dict(e)
            first["index_start"] = e["index"]
            first["index_end"] = run[-1]["index"]
            first["count"] = len(run)
            first["samples"] = texts[:5]
            folded.append(first)
        else:
            folded.extend(run)
        i = j + 1
    return folded


def compress(
    xml: str,
    max_elements: int,
    clickable_only: bool,
    exclude_packages: list[str] | None = None,
    offset: int = 0,
    text_contains: str | None = None,
    rid_contains: str | None = None,
    class_contains: str | None = None,
    order: str = "visual",
) -> dict:
    """Parse hierarchy XML → element table + metadata.

    Returns:
        {
          "elements": [...],        # NOT returned to the model; kept server-side
          "table": "<index-addressed listing>",
          "rid_prefix": "com.tencent.mm",
          "total_nodes": int, "shown": int, "truncated": bool,
        }
    """
    root = ET.fromstring(xml)

    # ── pass 1: recursive walk, collect candidates with parent + depth ──
    candidates: list[dict] = []
    total_nodes = 0
    dialog_seen = False
    excl = set(exclude_packages or [])
    needle_t = (text_contains or "").lower()
    needle_r = (rid_contains or "").lower()
    needle_c = (class_contains or "").lower()

    def walk(node, depth: int, pid: int, anc_clickable: bool = False) -> None:
        nonlocal total_nodes, dialog_seen
        # Excluded packages (status bar, IME, ...) are skipped with their
        # whole subtree — they pollute the table and shove content out of
        # the max_elements window.
        if excl and node.get("package", "") in excl:
            return
        total_nodes += 1
        my_pid = len(candidates)  # provisional parent id
        cls = node.get("class", "")
        clickable = node.get("clickable") == "true"
        # Dialog detection must happen for EVERY node, not just collected
        # ones — the dialog container itself is usually non-interactive.
        if "Dialog" in cls or cls.endswith("PopupWindow"):
            dialog_seen = True
        is_edit = cls.endswith("EditText")
        scrollable = node.get("scrollable") == "true"
        text = _clip(node.get("text", ""))
        desc = _clip(node.get("content-desc", ""))
        rid = node.get("resource-id", "") or ""
        has_label = bool(text or desc or rid)

        # Clickability heuristic (docs/23 field review): a node with text and
        # bounds under a clickable ancestor is tappable even when its own
        # clickable=false — bare flags lie (e.g. date-strip items).
        tappable = clickable or anc_clickable

        if (
            (clickable or scrollable or is_edit or has_label)
            and (
                not clickable_only
                or clickable
                or is_edit
                or scrollable
                or (has_label and anc_clickable)
            )
            and (not needle_t or needle_t in (text + desc + rid).lower())
            and (not needle_r or needle_r in rid.lower())
            and (not needle_c or needle_c in cls.lower())
        ):
            b = _bounds(node)
            if b is not None and (b[2] - b[0]) * (b[3] - b[1]) > 0:
                candidates.append(
                    {
                        "type": cls.split(".")[-1],
                        "text": text or desc,
                        "rid_full": rid,
                        "bounds": list(b),
                        "center": [(b[0] + b[2]) // 2, (b[1] + b[3]) // 2],
                        "clickable": clickable,
                        "tappable": tappable,
                        "scrollable": scrollable,
                        "is_edit": is_edit,
                        "disabled": node.get("enabled") == "false",
                        "focused": node.get("focused") == "true",
                        "password": node.get("password") == "true",
                        "checked": node.get("checked") == "true"
                        if node.get("checkable") == "true"
                        else None,
                        "depth": depth,
                        "pid": pid,
                    }
                )
                candidates[-1]["eid"] = "e" + hashlib.md5(
                    (
                        candidates[-1]["type"]
                        + "|"
                        + candidates[-1]["rid_full"]
                        + "|"
                        + candidates[-1]["text"]
                        + "|"
                        + str(candidates[-1]["center"])
                    ).encode()
                ).hexdigest()[:8]
        for child in node:
            walk(child, depth + 1, my_pid if candidates else pid, tappable)

    walk(root, 0, 0)

    # ── pass 2.5: stable visual order (top→bottom, left→right) so indexes
    # don't reshuffle when the window tree shape changes between dumps ──
    if order == "visual":
        candidates.sort(key=lambda e: (e["bounds"][1], e["bounds"][0]))

    # ── pass 3: rid package prefix ──
    packages: dict[str, int] = {}
    for e in candidates:
        parts = e["rid_full"].split("/", 1)
        if len(parts) == 2 and parts[0]:
            packages[parts[0]] = packages.get(parts[0], 0) + 1
    rid_prefix = max(packages, key=packages.get) if packages else ""
    if rid_prefix:
        for e in candidates:
            e["rid"] = e["rid_full"].split("/", 1)[1] if e["rid_full"].startswith(rid_prefix + "/") else e["rid_full"]
    else:
        for e in candidates:
            e["rid"] = e["rid_full"]

    # ── pass 3.5: fold repeated list items (after sort, before numbering) ──
    for k, e in enumerate(candidates):
        e["index"] = k + 1
    candidates = _fold_siblings(candidates)

    # ── pass 4: offset window + numbering (offset-aware, 1-based) ──
    truncated = len(candidates) > offset + max_elements
    candidates = candidates[offset : offset + max_elements]
    for k, e in enumerate(candidates):
        e["index"] = offset + k + 1
        if e.get("count"):
            e["index_start"] = e["index"]
            e["index_end"] = e["index"] + e["count"] - 1

    # ── pass 5: render table ──
    lines: list[str] = []
    for e in candidates:
        indent = "  " * min(e["depth"], MAX_DEPTH_INDENT)
        idx = f'[{e["index"]}]'
        type_s = e["type"]
        if e.get("count"):
            type_s += f' ×{e["count"]}'
        flags = []
        if e["disabled"]:
            flags.append("disabled")
        if e.get("tappable") and not e["clickable"]:
            flags.append("tap*")
        if e["focused"]:
            flags.append("focus")
        if e["password"]:
            flags.append("pwd")
        if e["scrollable"]:
            flags.append("scroll")
        if e["checked"] is not None:
            flags.append("on" if e["checked"] else "off")
        flag_s = (" " + " ".join(f"[{f}]" for f in flags)) if flags else ""
        label = e["text"]
        label_from_rid = False
        if e.get("count") and e.get("samples"):
            label = "│".join(e["samples"]) + ("│…" if e["count"] > len(e["samples"]) else "")
        elif not label and e["rid"]:
            label = e["rid"]
            label_from_rid = True
        rid_s = f" rid={e['rid']}" if e["rid"] else ""
        cx, cy = e["center"]
        w = e["bounds"][2] - e["bounds"][0]
        h = e["bounds"][3] - e["bounds"][1]
        shown_label = label if label_from_rid else f'"{label}"'
        lines.append(
            f'{indent}{idx} {type_s} {shown_label}{rid_s} @({cx},{cy}) {w}x{h}{flag_s}'
        )

    table = "\n".join(lines)
    if dialog_seen:
        table = "── 检测到对话框 ──\n" + table
    if truncated:
        table += f"\n（已截断，仅显示前 {max_elements} 个元素）"

    return {
        "elements": candidates,
        "table": table,
        "rid_prefix": rid_prefix,
        "total_nodes": total_nodes,
        "shown": len(candidates),
        "truncated": truncated,
    }
