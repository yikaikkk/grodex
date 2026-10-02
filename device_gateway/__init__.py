"""device_gateway — stdio JSON-RPC sidecar wrapping uiautomator2.

Protocol (line-delimited JSON on stdin/stdout, see docs/23-device-control-design.md §3):
  request : {"id": <int>, "method": "<snake_case>", "params": {...}}
  response: {"id": <int>, "ok": true, "result": {...}}
            {"id": <int>, "ok": false, "error": {"code": "...", "message": "..."}}

Run: python -m device_gateway
"""

from .server import serve

__all__ = ["serve"]
