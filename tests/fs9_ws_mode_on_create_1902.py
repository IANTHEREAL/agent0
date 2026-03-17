#!/usr/bin/env python3
"""
Regression test for issue #1902: fs9 mode-on-create.

Exercises mode parameter on file/directory creation over the fs9 WS protocol:
- write with mode creates file with requested mode
- write without mode defaults to 0o644
- mkdir with mode applies to target directory only
- mkdir recursive: intermediate dirs get 0o755, leaf gets requested mode
- overwrite of existing file preserves original mode
"""

import argparse
import base64
import hashlib
import json
import os
import socket
import struct
import time
from dataclasses import dataclass
from typing import Any, Dict


@dataclass
class WsClient:
    sock: socket.socket

    @staticmethod
    def connect(host: str, port: int) -> "WsClient":
        sock = socket.create_connection((host, port), timeout=10)
        sock.settimeout(10)

        key = base64.b64encode(os.urandom(16)).decode("ascii")
        request = (
            f"GET / HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            "\r\n"
        )
        sock.sendall(request.encode("ascii"))

        response = b""
        while b"\r\n\r\n" not in response:
            chunk = sock.recv(4096)
            if not chunk:
                raise RuntimeError("websocket handshake closed unexpectedly")
            response += chunk

        header_blob = response.split(b"\r\n\r\n", 1)[0].decode("ascii", errors="replace")
        if "101 Switching Protocols" not in header_blob:
            raise RuntimeError(f"websocket handshake failed: {header_blob}")

        expected_accept = base64.b64encode(
            hashlib.sha1(
                (
                    key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
                ).encode("ascii")
            ).digest()
        ).decode("ascii")
        accept_line = next(
            (
                line.split(":", 1)[1].strip()
                for line in header_blob.split("\r\n")
                if line.lower().startswith("sec-websocket-accept:")
            ),
            None,
        )
        if accept_line != expected_accept:
            raise RuntimeError(
                f"unexpected Sec-WebSocket-Accept: got={accept_line!r} expected={expected_accept!r}"
            )

        return WsClient(sock=sock)

    def close(self) -> None:
        try:
            self._send_frame(opcode=0x8, payload=b"")
        except OSError:
            pass
        self.sock.close()

    def send_json(self, payload: Dict[str, Any]) -> Dict[str, Any]:
        self._send_frame(opcode=0x1, payload=json.dumps(payload).encode("utf-8"))
        return self._recv_json()

    def _send_frame(self, opcode: int, payload: bytes) -> None:
        first_byte = 0x80 | (opcode & 0x0F)
        mask_key = os.urandom(4)
        masked = bytes(b ^ mask_key[i % 4] for i, b in enumerate(payload))
        length = len(payload)

        if length < 126:
            header = struct.pack("!BB", first_byte, 0x80 | length)
        elif length < (1 << 16):
            header = struct.pack("!BBH", first_byte, 0x80 | 126, length)
        else:
            header = struct.pack("!BBQ", first_byte, 0x80 | 127, length)

        self.sock.sendall(header + mask_key + masked)

    def _recv_json(self) -> Dict[str, Any]:
        while True:
            opcode, payload = self._recv_frame()
            if opcode == 0x1:
                return json.loads(payload.decode("utf-8"))
            if opcode == 0x9:
                self._send_frame(opcode=0xA, payload=payload)
                continue
            if opcode == 0x8:
                raise RuntimeError("server closed websocket")
            raise RuntimeError(f"unexpected websocket opcode: {opcode}")

    def _recv_frame(self) -> tuple[int, bytes]:
        header = self._recv_exact(2)
        first_byte, second_byte = struct.unpack("!BB", header)
        opcode = first_byte & 0x0F
        masked = (second_byte & 0x80) != 0
        length = second_byte & 0x7F

        if length == 126:
            length = struct.unpack("!H", self._recv_exact(2))[0]
        elif length == 127:
            length = struct.unpack("!Q", self._recv_exact(8))[0]

        mask_key = self._recv_exact(4) if masked else b""
        payload = self._recv_exact(length)
        if masked:
            payload = bytes(b ^ mask_key[i % 4] for i, b in enumerate(payload))
        return opcode, payload

    def _recv_exact(self, size: int) -> bytes:
        chunks = []
        remaining = size
        while remaining > 0:
            chunk = self.sock.recv(remaining)
            if not chunk:
                raise RuntimeError("socket closed while reading websocket frame")
            chunks.append(chunk)
            remaining -= len(chunk)
        return b"".join(chunks)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Regression test for fs9 mode-on-create (#1902)"
    )
    parser.add_argument(
        "--dsn",
        required=True,
        help="PostgreSQL DSN (e.g. postgres://user:pass@host:port/db)",
    )
    return parser.parse_args()


def expect_ok(response: Dict[str, Any], request_id: str) -> Dict[str, Any]:
    assert response["id"] == request_id, f"unexpected response id: {response}"
    assert response["ok"] is True, f"expected success for {request_id}: {response}"
    return response.get("data") or {}


def main() -> int:
    args = parse_args()
    parsed = __import__("urllib.parse", fromlist=["urlparse"]).urlparse(args.dsn)
    if not parsed.hostname or not parsed.username or parsed.password is None:
        raise SystemExit("invalid DSN for websocket auth")

    host = parsed.hostname
    ws_port = int(os.environ.get("FS9_WS_PORT", "5480"))
    username = parsed.username
    password = parsed.password

    suffix = f"{int(time.time())}_{os.getpid()}"
    base = f"/regression_mode_on_create_{suffix}"

    client = WsClient.connect(host, ws_port)
    try:
        expect_ok(
            client.send_json(
                {
                    "id": "auth-1",
                    "op": "auth",
                    "username": username,
                    "password": password,
                }
            ),
            "auth-1",
        )

        # 1. Write with explicit mode
        expect_ok(
            client.send_json(
                {"id": "mkdir-base", "op": "mkdir", "path": base, "recursive": True}
            ),
            "mkdir-base",
        )

        expect_ok(
            client.send_json(
                {
                    "id": "write-exec",
                    "op": "write",
                    "path": f"{base}/exec.sh",
                    "content": base64.b64encode(b"#!/bin/sh\necho hi").decode("ascii"),
                    "encoding": "base64",
                    "mode": 0o755,
                }
            ),
            "write-exec",
        )

        stat_exec = expect_ok(
            client.send_json({"id": "stat-exec", "op": "stat", "path": f"{base}/exec.sh"}),
            "stat-exec",
        )
        assert stat_exec["mode"] == 0o755, f"expected mode 0o755, got {oct(stat_exec['mode'])}"

        # 2. Write without mode defaults to 0o644
        expect_ok(
            client.send_json(
                {
                    "id": "write-default",
                    "op": "write",
                    "path": f"{base}/default.txt",
                    "content": base64.b64encode(b"hello").decode("ascii"),
                    "encoding": "base64",
                }
            ),
            "write-default",
        )

        stat_default = expect_ok(
            client.send_json({"id": "stat-default", "op": "stat", "path": f"{base}/default.txt"}),
            "stat-default",
        )
        assert stat_default["mode"] == 0o644, f"expected mode 0o644, got {oct(stat_default['mode'])}"

        # 3. Overwrite preserves original mode
        expect_ok(
            client.send_json(
                {
                    "id": "write-overwrite",
                    "op": "write",
                    "path": f"{base}/exec.sh",
                    "content": base64.b64encode(b"#!/bin/sh\necho updated").decode("ascii"),
                    "encoding": "base64",
                    "mode": 0o644,
                }
            ),
            "write-overwrite",
        )

        stat_overwrite = expect_ok(
            client.send_json({"id": "stat-overwrite", "op": "stat", "path": f"{base}/exec.sh"}),
            "stat-overwrite",
        )
        assert stat_overwrite["mode"] == 0o755, f"overwrite should preserve original mode 0o755, got {oct(stat_overwrite['mode'])}"

        # 4. mkdir with explicit mode
        expect_ok(
            client.send_json(
                {
                    "id": "mkdir-mode",
                    "op": "mkdir",
                    "path": f"{base}/restricted",
                    "mode": 0o700,
                }
            ),
            "mkdir-mode",
        )

        stat_dir = expect_ok(
            client.send_json({"id": "stat-dir", "op": "stat", "path": f"{base}/restricted"}),
            "stat-dir",
        )
        assert stat_dir["mode"] == 0o700, f"expected dir mode 0o700, got {oct(stat_dir['mode'])}"

        # 5. Recursive mkdir: intermediate dirs get 0o755, leaf gets requested mode
        expect_ok(
            client.send_json(
                {
                    "id": "mkdir-recursive",
                    "op": "mkdir",
                    "path": f"{base}/a/b/c",
                    "recursive": True,
                    "mode": 0o700,
                }
            ),
            "mkdir-recursive",
        )

        stat_intermediate = expect_ok(
            client.send_json({"id": "stat-inter", "op": "stat", "path": f"{base}/a"}),
            "stat-inter",
        )
        assert stat_intermediate["mode"] == 0o755, f"intermediate dir should be 0o755, got {oct(stat_intermediate['mode'])}"

        stat_leaf = expect_ok(
            client.send_json({"id": "stat-leaf", "op": "stat", "path": f"{base}/a/b/c"}),
            "stat-leaf",
        )
        assert stat_leaf["mode"] == 0o700, f"leaf dir should be 0o700, got {oct(stat_leaf['mode'])}"

        # 6. Mode sanitize: non-permission bits stripped
        expect_ok(
            client.send_json(
                {
                    "id": "write-sanitize",
                    "op": "write",
                    "path": f"{base}/sanitized.txt",
                    "content": base64.b64encode(b"test").decode("ascii"),
                    "encoding": "base64",
                    "mode": 0o100755,
                }
            ),
            "write-sanitize",
        )

        stat_sanitized = expect_ok(
            client.send_json({"id": "stat-sanitize", "op": "stat", "path": f"{base}/sanitized.txt"}),
            "stat-sanitize",
        )
        assert stat_sanitized["mode"] == 0o755, f"mode should be sanitized to 0o755, got {oct(stat_sanitized['mode'])}"

        # 7. batch_write with per-file mode
        expect_ok(
            client.send_json(
                {
                    "id": "batch-mode",
                    "op": "batch_write",
                    "files": [
                        {
                            "path": f"{base}/batch-exec.sh",
                            "content": base64.b64encode(b"#!/bin/sh").decode("ascii"),
                            "encoding": "base64",
                            "mode": 0o755,
                        },
                        {
                            "path": f"{base}/batch-data.txt",
                            "content": base64.b64encode(b"data").decode("ascii"),
                            "encoding": "base64",
                        },
                    ],
                }
            ),
            "batch-mode",
        )

        stat_batch_exec = expect_ok(
            client.send_json({"id": "stat-batch-exec", "op": "stat", "path": f"{base}/batch-exec.sh"}),
            "stat-batch-exec",
        )
        assert stat_batch_exec["mode"] == 0o755, f"batch file with mode should be 0o755, got {oct(stat_batch_exec['mode'])}"

        stat_batch_data = expect_ok(
            client.send_json({"id": "stat-batch-data", "op": "stat", "path": f"{base}/batch-data.txt"}),
            "stat-batch-data",
        )
        assert stat_batch_data["mode"] == 0o644, f"batch file without mode should default to 0o644, got {oct(stat_batch_data['mode'])}"

        print("PASS: fs9 mode-on-create contract")
        return 0
    finally:
        try:
            client.send_json({"id": "rm-1", "op": "rm", "path": base, "recursive": True})
        except Exception:
            pass
        client.close()


if __name__ == "__main__":
    raise SystemExit(main())
