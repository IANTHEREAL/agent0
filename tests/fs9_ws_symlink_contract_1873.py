#!/usr/bin/env python3
"""
Regression test for issue #1873: fs9 WebSocket symlink contract.

Exercises the user-visible symlink lifecycle over the fs9 WS protocol:
- auth
- write target file
- create symlink
- stat reports type=symlink
- readlink returns the original target
- rename moves the symlink itself
- unlink removes the symlink without deleting the target
- batch_write refuses to overwrite the symlink
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
from urllib.parse import urlparse


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
        description="Regression test for fs9 WebSocket symlink semantics (#1873)"
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


def expect_error(response: Dict[str, Any], request_id: str, code: str, message_substr: str) -> None:
    assert response["id"] == request_id, f"unexpected response id: {response}"
    assert response["ok"] is False, f"expected error for {request_id}: {response}"
    error = response.get("error") or {}
    assert error.get("code") == code, f"unexpected error code: {response}"
    assert message_substr in error.get("message", ""), f"unexpected error message: {response}"


def main() -> int:
    args = parse_args()
    parsed = urlparse(args.dsn)
    if not parsed.hostname or not parsed.username or parsed.password is None:
        raise SystemExit("invalid DSN for websocket auth")

    host = parsed.hostname
    ws_port = int(os.environ.get("FS9_WS_PORT", "5480"))
    username = parsed.username
    password = parsed.password
    actual_user = username.split(".", 1)[1] if "." in username else username.split(":", 1)[1] if ":" in username else username

    suffix = f"{int(time.time())}_{os.getpid()}"
    base = f"/regression_symlink_{suffix}"
    target = f"{base}/target.txt"
    link = f"{base}/link"
    renamed = f"{base}/link-renamed"
    sibling = f"{base}/sibling.txt"

    client = WsClient.connect(host, ws_port)
    try:
        auth_data = expect_ok(
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
        assert auth_data["user"] == actual_user, auth_data

        expect_ok(
            client.send_json(
                {"id": "mkdir-1", "op": "mkdir", "path": base, "recursive": True}
            ),
            "mkdir-1",
        )

        expect_ok(
            client.send_json(
                {
                    "id": "write-1",
                    "op": "write",
                    "path": target,
                    "content": base64.b64encode(b"payload").decode("ascii"),
                    "encoding": "base64",
                }
            ),
            "write-1",
        )

        expect_ok(
            client.send_json(
                {"id": "symlink-1", "op": "symlink", "path": link, "target": target}
            ),
            "symlink-1",
        )

        stat_data = expect_ok(
            client.send_json({"id": "stat-1", "op": "stat", "path": link}),
            "stat-1",
        )
        assert stat_data["type"] == "symlink", stat_data

        readlink_data = expect_ok(
            client.send_json({"id": "readlink-1", "op": "readlink", "path": link}),
            "readlink-1",
        )
        assert readlink_data["target"] == target, readlink_data

        batch_response = client.send_json(
            {
                "id": "batch-1",
                "op": "batch_write",
                "files": [
                    {
                        "path": link,
                        "content": base64.b64encode(b"overwrite").decode("ascii"),
                        "encoding": "base64",
                    },
                    {
                        "path": sibling,
                        "content": base64.b64encode(b"sibling").decode("ascii"),
                        "encoding": "base64",
                    },
                ],
            }
        )
        batch_data = expect_ok(batch_response, "batch-1")
        entries = batch_data["entries"]
        assert len(entries) == 2, entries
        expect_error({"id": "batch-1", "ok": entries[0]["ok"], "error": entries[0]["error"]}, "batch-1", "EINVAL", "cannot write to symlink as file; use readlink")
        assert entries[1]["ok"] is True, entries

        expect_ok(
            client.send_json(
                {
                    "id": "rename-1",
                    "op": "rename",
                    "old_path": link,
                    "new_path": renamed,
                }
            ),
            "rename-1",
        )

        renamed_readlink = expect_ok(
            client.send_json({"id": "readlink-2", "op": "readlink", "path": renamed}),
            "readlink-2",
        )
        assert renamed_readlink["target"] == target, renamed_readlink

        expect_ok(
            client.send_json({"id": "unlink-1", "op": "unlink", "path": renamed}),
            "unlink-1",
        )

        target_read = expect_ok(
            client.send_json({"id": "read-1", "op": "read", "path": target}),
            "read-1",
        )
        assert base64.b64decode(target_read["content"]) == b"payload", target_read

        print("PASS: fs9 WebSocket symlink contract")
        return 0
    finally:
        try:
            client.send_json({"id": "rm-1", "op": "rm", "path": base, "recursive": True})
        except Exception:
            pass
        client.close()


if __name__ == "__main__":
    raise SystemExit(main())
