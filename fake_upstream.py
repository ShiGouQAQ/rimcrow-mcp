#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""最小 Streamable HTTP MCP 上游（**测试替身，不是 RimCrow**）。

用途：本机 RimCrow 端点在测试窗口未写出时，用它验证 rimcrow-mcp 的**透传路径**：
bearer 注入、mcp-session-id 会话、tools/list 透传、tools/call 透传、
远端 isError 逐字回抛、上游 unknown tool 的版本增强（SPEC §5 场景 4）、
R8 上游/快照版本比对告警。

启动后把 endpoint 文件（schema_version=1）写到 --endpoint 指定路径，
自身进程名必须落在探活白名单（python.exe）内。

为什么不用 rmcp 自带的 server（feature ``transport-streamable-http-server`` /
``StreamableHttpService``；本 crate ``Cargo.toml`` 只启用 client 侧，未开该 feature）：

1. 要写 **RimCrow endpoint 文件契约**（``schema_version=1``、``started_at`` 落在探活
   窗口内、进程名进白名单），rmcp 的传输层不认这套文件契约；
2. 要**精确制造**异常样本：上游版本固定 ``9.9.9``（≠ 快照 → 触发 R8 版本错配告警）、
   固定远端 isError 文本、未知会话回 404（复现 FastMCP 同样的 404 → 触发 transport
   的 session recovery）、未知工具 → §5-S4 增强文案；
3. 替身与被测桥（rmcp）必须是**两套独立实现**，用 rmcp 写上游等于「拿 rmcp 测 rmcp」；
4. Python 标准库零依赖（``http.server`` / ``json`` / ``threading``），由
   ``scripts/smoke_bridge.py --fake`` 以 ``sys.executable`` 直接拉起。
"""
from __future__ import annotations

import os
import time


def _boot(msg: str) -> None:
    """启动阶段标记（仅在设置 FAKE_BOOT_LOG 时写，用于诊断 spawn 拉起延迟）。"""
    path = os.environ.get("FAKE_BOOT_LOG")
    if not path:
        return
    try:
        with open(path, "a", encoding="utf-8") as fh:
            fh.write(f"{time.time():.3f} {msg}\n")
            fh.flush()
    except OSError:
        pass


_boot("script-entered")

import argparse  # noqa: E402
import json  # noqa: E402
import sys  # noqa: E402
import threading  # noqa: E402
import uuid  # noqa: E402
from datetime import datetime  # noqa: E402
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer  # noqa: E402

_boot("stdlib-imports-done")

FAKE_TOOLS = [
    {
        "name": "echo_tool",
        "description": "FAKE tool: echo back arguments (proves passthrough, not snapshot).",
        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "remote_error_tool",
        "description": "FAKE tool: always returns isError with a fixed remote message.",
        "inputSchema": {"type": "object", "properties": {}},
        "annotations": {"readOnlyHint": False},
    },
]

SERVER_VERSION = "9.9.9"  # 故意 ≠ 快照 0.24.7，用来触发 R8 版本错配告警
REMOTE_ERROR_TEXT = "REMOTE_FAILURE: disk is read-only (fixed upstream text)"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "FakeMCP/0.1"
    token: str = "test-token"
    sessions: set[str] = set()
    log_enabled: bool = False

    def log_message(self, fmt, *args):  # noqa: A003
        if self.log_enabled:
            sys.stderr.write("[fake-upstream] " + (fmt % args) + "\n")

    # ---------- helpers ----------
    def _bearer_ok(self) -> bool:
        auth = self.headers.get("Authorization", "")
        return auth == f"Bearer {self.token}"

    def _send_sse(self, payload: dict, status: int = 200, extra: dict | None = None) -> None:
        body = f"event: message\r\ndata: {json.dumps(payload, ensure_ascii=False)}\r\n\r\n".encode(
            "utf-8"
        )
        self.send_response(status)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Content-Length", str(len(body)))
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def _send_json(self, payload: dict, status: int = 200) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _send_empty(self, status: int) -> None:
        self.send_response(status)
        self.send_header("Content-Length", "0")
        self.end_headers()

    # ---------- verbs ----------
    def do_GET(self):  # noqa: N802
        # 无 server-push 流：按规范回 405
        self.send_response(405)
        self.send_header("Allow", "POST")
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_DELETE(self):  # noqa: N802
        self._send_empty(200)

    def do_POST(self):  # noqa: N802
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b"{}"
        try:
            msg = json.loads(raw.decode("utf-8"))
        except ValueError:
            self._send_json({"error": "bad json"}, status=400)
            return

        if not self._bearer_ok():
            self._send_json({"error": "unauthorized"}, status=401)
            return

        method = msg.get("method", "")
        msg_id = msg.get("id")
        is_notification = "id" not in msg

        if is_notification:
            # 规范：通知回 202 Accepted，空体
            self._send_empty(202)
            return

        session = self.headers.get("mcp-session-id")
        if method == "initialize":
            session = uuid.uuid4().hex
            Handler.sessions.add(session)
            result = {
                "protocolVersion": msg.get("params", {}).get(
                    "protocolVersion", "2025-11-25"
                ),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "rimcrow-mcp", "version": SERVER_VERSION},
                "instructions": "FAKE_UPSTREAM_INSTRUCTIONS (not RimCrow)",
            }
            self._send_sse(
                {"jsonrpc": "2.0", "id": msg_id, "result": result},
                extra={"mcp-session-id": session},
            )
            return

        if session not in Handler.sessions:
            # 会话未知（FastMCP 同样会 404，触发 transport 的 session recovery）
            self._send_empty(404)
            return

        if method == "tools/list":
            result = {"tools": FAKE_TOOLS}
            self._send_sse({"jsonrpc": "2.0", "id": msg_id, "result": result})
            return

        if method == "tools/call":
            params = msg.get("params", {})
            name = params.get("name", "")
            if name == "echo_tool":
                result = {
                    "content": [
                        {
                            "type": "text",
                            "text": "FAKE_RESULT:"
                            + json.dumps(params.get("arguments"), ensure_ascii=False),
                        }
                    ],
                    "isError": False,
                }
            elif name == "remote_error_tool":
                result = {
                    "content": [{"type": "text", "text": REMOTE_ERROR_TEXT}],
                    "isError": True,
                }
            else:
                # 触发 bridge 的 §5 场景 4 增强
                result = {
                    "content": [{"type": "text", "text": f"Unknown tool: {name}"}],
                    "isError": True,
                }
            self._send_sse({"jsonrpc": "2.0", "id": msg_id, "result": result})
            return

        if method == "ping":
            self._send_sse({"jsonrpc": "2.0", "id": msg_id, "result": {}})
            return

        self._send_sse(
            {
                "jsonrpc": "2.0",
                "id": msg_id,
                "error": {"code": -32601, "message": f"method not found: {method}"},
            }
        )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--token", default="test-token")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args()
    _boot("args-parsed")

    Handler.token = args.token
    Handler.log_enabled = args.verbose

    httpd = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    _boot("server-bound")

    payload = {
        "schema_version": 1,
        "url": f"http://127.0.0.1:{args.port}/mcp",
        "pid": os.getpid(),
        "token": args.token,
        # 立刻写出：started_at 落在 [create-5s, create+300s] 内 → 探活 ready
        "started_at": datetime.now().astimezone().isoformat(),
        "mode": "desktop",
    }
    os.makedirs(os.path.dirname(args.endpoint), exist_ok=True)
    tmp = args.endpoint + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(payload, fh, ensure_ascii=False, indent=2)
        fh.flush()
        os.fsync(fh.fileno())
    os.replace(tmp, args.endpoint)
    _boot("endpoint-written")
    print(f"[fake-upstream] pid={os.getpid()} endpoint={args.endpoint}", flush=True)
    _boot("serving")

    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        try:
            os.unlink(args.endpoint)
        except OSError:
            pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
