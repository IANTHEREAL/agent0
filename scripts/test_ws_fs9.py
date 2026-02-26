#!/usr/bin/env python3
"""
End-to-end test client for fs9 WebSocket API.

Usage:
    python3 scripts/test_ws_fs9.py [--host HOST] [--port PORT] [--user USER] [--password PASSWORD]

Defaults:
    host=127.0.0.1  port=15480  user=admin  password=admin
"""

import argparse
import base64
import hashlib
import json
import struct
import sys
import time

import websocket

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_req_counter = 0

def next_id():
    global _req_counter
    _req_counter += 1
    return f"req-{_req_counter}"

def send_json(ws, payload):
    """Send a JSON text frame and return the parsed JSON response."""
    raw = json.dumps(payload)
    ws.send(raw)
    resp_raw = ws.recv()
    if isinstance(resp_raw, bytes):
        raise RuntimeError(f"Expected text frame, got binary ({len(resp_raw)} bytes)")
    return json.loads(resp_raw)

def send_binary(ws, data):
    """Send a binary frame."""
    ws.send(data, opcode=websocket.ABNF.OPCODE_BINARY)

def recv_any(ws):
    """Receive a frame; return (is_binary, data)."""
    opcode, data = ws.recv_data()
    is_binary = (opcode == websocket.ABNF.OPCODE_BINARY)
    return is_binary, data

def assert_ok(resp, ctx=""):
    prefix = f"[{ctx}] " if ctx else ""
    if not resp.get("ok"):
        err = resp.get("error", {})
        raise AssertionError(
            f'{prefix}Expected ok=true, got error: {err.get("code")} — {err.get("message")}'
        )

def assert_err(resp, expected_code, ctx=""):
    prefix = f"[{ctx}] " if ctx else ""
    if resp.get("ok"):
        raise AssertionError(f"{prefix}Expected error {expected_code}, got ok=true")
    code = resp.get("error", {}).get("code")
    if code != expected_code:
        raise AssertionError(f"{prefix}Expected error code {expected_code}, got {code}")

# ---------------------------------------------------------------------------
# Test cases
# ---------------------------------------------------------------------------

def test_auth(ws, user, password):
    """Test authentication."""
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "auth", "username": user, "password": password})
    assert_ok(resp, "auth")
    assert resp["id"] == rid, f"id mismatch: {resp['id']} != {rid}"
    data = resp["data"]
    assert "user" in data, "missing 'user' in auth response"
    assert "keyspace" in data, "missing 'keyspace' in auth response"
    print(f"  ✓ auth OK — user={data['user']}, keyspace={data['keyspace']}")
    return data

def test_mkdir(ws, path, recursive=False):
    """Test mkdir."""
    rid = next_id()
    payload = {"id": rid, "op": "mkdir", "path": path}
    if recursive:
        payload["recursive"] = True
    resp = send_json(ws, payload)
    assert_ok(resp, f"mkdir {path}")
    print(f"  ✓ mkdir OK — {path} (recursive={recursive})")

def test_write(ws, path, content_bytes):
    """Test write (inline base64)."""
    rid = next_id()
    encoded = base64.b64encode(content_bytes).decode()
    resp = send_json(ws, {
        "id": rid,
        "op": "write",
        "path": path,
        "content": encoded,
        "encoding": "base64",
    })
    assert_ok(resp, f"write {path}")
    written = resp["data"]["written"]
    assert written == len(content_bytes), f"written {written} != expected {len(content_bytes)}"
    print(f"  ✓ write OK — {path} ({written} bytes)")

def test_read(ws, path, expected_bytes):
    """Test read (inline base64) and verify content."""
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "read", "path": path})
    assert_ok(resp, f"read {path}")
    data = resp["data"]
    assert data["encoding"] == "base64", f"unexpected encoding: {data['encoding']}"
    actual = base64.b64decode(data["content"])
    assert actual == expected_bytes, f"content mismatch: {len(actual)} bytes != {len(expected_bytes)} bytes"
    print(f"  ✓ read OK — {path} ({data['size']} bytes, content verified)")

def test_read_at(ws, path, offset, length, expected_bytes):
    """Test partial read with offset/length."""
    rid = next_id()
    resp = send_json(ws, {
        "id": rid,
        "op": "read",
        "path": path,
        "offset": offset,
        "length": length,
    })
    assert_ok(resp, f"read_at {path}@{offset}+{length}")
    actual = base64.b64decode(resp["data"]["content"])
    assert actual == expected_bytes, f"partial read mismatch at offset {offset}"
    print(f"  ✓ read_at OK — {path} offset={offset} length={length} ({len(actual)} bytes)")

def test_stat(ws, path, expect_type=None, expect_size=None):
    """Test stat."""
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "stat", "path": path})
    assert_ok(resp, f"stat {path}")
    data = resp["data"]
    if expect_type:
        assert data["type"] == expect_type, f"type mismatch: {data['type']} != {expect_type}"
    if expect_size is not None:
        assert data["size"] == expect_size, f"size mismatch: {data['size']} != {expect_size}"
    print(f"  ✓ stat OK — {path} type={data['type']} size={data['size']} mtime={data.get('mtime','?')}")
    return data

def test_readdir(ws, path, expected_count=None):
    """Test readdir."""
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "readdir", "path": path})
    assert_ok(resp, f"readdir {path}")
    entries = resp["data"]["entries"]
    if expected_count is not None:
        assert len(entries) == expected_count, f"entry count {len(entries)} != expected {expected_count}"
    names = [e["path"] for e in entries]
    print(f"  ✓ readdir OK — {path} ({len(entries)} entries: {names})")
    return entries

def test_pwrite(ws, path, offset, content_bytes):
    """Test pwrite (offset write)."""
    rid = next_id()
    encoded = base64.b64encode(content_bytes).decode()
    resp = send_json(ws, {
        "id": rid,
        "op": "pwrite",
        "path": path,
        "offset": offset,
        "content": encoded,
        "encoding": "base64",
    })
    assert_ok(resp, f"pwrite {path}@{offset}")
    written = resp["data"]["written"]
    assert written == len(content_bytes), f"pwrite written {written} != {len(content_bytes)}"
    print(f"  ✓ pwrite OK — {path} offset={offset} ({written} bytes)")

def test_append(ws, path, content_bytes):
    """Test append."""
    rid = next_id()
    encoded = base64.b64encode(content_bytes).decode()
    resp = send_json(ws, {
        "id": rid,
        "op": "append",
        "path": path,
        "content": encoded,
        "encoding": "base64",
    })
    assert_ok(resp, f"append {path}")
    written = resp["data"]["written"]
    assert written == len(content_bytes), f"append written {written} != {len(content_bytes)}"
    print(f"  ✓ append OK — {path} ({written} bytes)")

def test_truncate(ws, path, size):
    """Test truncate."""
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "truncate", "path": path, "size": size})
    assert_ok(resp, f"truncate {path} to {size}")
    print(f"  ✓ truncate OK — {path} → {size} bytes")

def test_unlink(ws, path):
    """Test unlink (delete file)."""
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "unlink", "path": path})
    assert_ok(resp, f"unlink {path}")
    print(f"  ✓ unlink OK — {path}")

def test_rm(ws, path, recursive=False):
    """Test rm (delete file/dir)."""
    rid = next_id()
    payload = {"id": rid, "op": "rm", "path": path}
    if recursive:
        payload["recursive"] = True
    resp = send_json(ws, payload)
    assert_ok(resp, f"rm {path}")
    removed = resp["data"].get("removed")
    print(f"  ✓ rm OK — {path} (recursive={recursive}, removed={removed})")

def test_stat_not_found(ws, path):
    """Test that stat returns ENOENT for missing path."""
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "stat", "path": path})
    assert_err(resp, "ENOENT", f"stat_not_found {path}")
    print(f"  ✓ stat_not_found OK — {path} → ENOENT")

def test_path_traversal(ws):
    """Test that path traversal is rejected."""
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "stat", "path": "/data/../etc/passwd"})
    assert_err(resp, "EINVAL", "path_traversal")
    print(f"  ✓ path_traversal OK — rejected with EINVAL")

def test_relative_path(ws):
    """Test that relative paths are rejected."""
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "stat", "path": "relative/path"})
    assert_err(resp, "EINVAL", "relative_path")
    print(f"  ✓ relative_path OK — rejected with EINVAL")

def test_streaming_write_read(ws, path, size_bytes=1024 * 1024 + 100):
    """Test streaming write followed by streaming read for a large file."""
    CHUNK_SIZE = 64 * 1024  # 64KB, matching server default

    # Generate test data
    data = bytes(range(256)) * (size_bytes // 256) + bytes(range(size_bytes % 256))
    assert len(data) == size_bytes
    checksum = "sha256:" + hashlib.sha256(data).hexdigest()

    # --- Streaming write ---
    rid_w = next_id()
    resp = send_json(ws, {
        "id": rid_w,
        "op": "write",
        "path": path,
        "streaming": True,
        "size": size_bytes,
    })
    assert_ok(resp, "streaming_write_init")
    ready = resp["data"]
    assert ready["ready"] is True, "expected ready=true"
    stream_id = ready["stream_id"]
    chunk_size = ready.get("chunk_size", CHUNK_SIZE)
    print(f"  → streaming write started: stream_id={stream_id}, chunk_size={chunk_size}")

    # Send binary chunks
    offset = 0
    chunks_sent = 0
    while offset < len(data):
        chunk = data[offset:offset + chunk_size]
        frame = struct.pack(">Q", stream_id) + chunk
        send_binary(ws, frame)
        offset += len(chunk)
        chunks_sent += 1

    # Send stream end
    end_resp = send_json(ws, {
        "id": rid_w,
        "stream": "end",
        "stream_id": stream_id,
        "checksum": checksum,
    })
    assert_ok(end_resp, "streaming_write_end")
    written = end_resp["data"]["written"]
    assert written == size_bytes, f"streaming write: written {written} != {size_bytes}"
    print(f"  ✓ streaming write OK — {path} ({written} bytes, {chunks_sent} chunks)")

    # --- Streaming read ---
    rid_r = next_id()
    resp = send_json(ws, {
        "id": rid_r,
        "op": "read",
        "path": path,
        "streaming": True,
    })
    assert_ok(resp, "streaming_read_init")
    start = resp["data"]
    assert start["streaming"] is True, "expected streaming=true"
    read_stream_id = start["stream_id"]
    total_size = start["size"]
    assert total_size == size_bytes, f"streaming read size {total_size} != {size_bytes}"
    print(f"  → streaming read started: stream_id={read_stream_id}, size={total_size}")

    # Receive binary chunks
    received_data = bytearray()
    chunks_received = 0
    while True:
        is_binary, frame_data = recv_any(ws)
        if is_binary:
            # Binary frame: [8-byte stream_id][chunk]
            if len(frame_data) < 8:
                raise AssertionError(f"Binary frame too short: {len(frame_data)} bytes")
            sid = struct.unpack(">Q", frame_data[:8])[0]
            assert sid == read_stream_id, f"stream_id mismatch: {sid} != {read_stream_id}"
            received_data.extend(frame_data[8:])
            chunks_received += 1
        else:
            # Text frame: should be stream end
            end = json.loads(frame_data)
            assert_ok(end, "streaming_read_end")
            end_data = end["data"]
            assert end_data.get("stream") == "end", f"expected stream=end, got {end_data}"
            assert end_data["stream_id"] == read_stream_id
            # Verify checksum
            if "checksum" in end_data:
                assert end_data["checksum"] == checksum, "streaming read checksum mismatch"
            break

    assert bytes(received_data) == data, "streaming read data mismatch!"
    print(f"  ✓ streaming read OK — {path} ({len(received_data)} bytes, {chunks_received} chunks, checksum verified)")

def test_auth_failure(host, port):
    """Test that bad credentials get rejected and connection is closed."""
    ws = websocket.WebSocket()
    ws.connect(f"ws://{host}:{port}")
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "auth", "username": "admin", "password": "wrongpassword"})
    assert_err(resp, "EAUTH", "auth_failure")
    # Connection should be closed by server
    try:
        ws.recv()
        # If we get here without exception, it might be a close frame
    except Exception:
        pass
    ws.close()
    print(f"  ✓ auth_failure OK — rejected with EAUTH")

def test_no_auth_operation(host, port):
    """Test that sending an operation before auth gets rejected."""
    ws = websocket.WebSocket()
    ws.connect(f"ws://{host}:{port}")
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "stat", "path": "/data"})
    assert_err(resp, "EPROTO", "no_auth_operation")
    ws.close()
    print(f"  ✓ no_auth_operation OK — rejected with EPROTO")

def test_double_auth(ws):
    """Test that sending auth again after authenticated gets EPROTO."""
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "auth", "username": "admin", "password": "admin"})
    assert_err(resp, "EPROTO", "double_auth")
    print(f"  ✓ double_auth OK — rejected with EPROTO")

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="fs9 WebSocket API test client")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=15480)
    parser.add_argument("--user", default="admin")
    parser.add_argument("--password", default="admin")
    args = parser.parse_args()

    url = f"ws://{args.host}:{args.port}"
    passed = 0
    failed = 0
    errors = []

    print(f"═══════════════════════════════════════════════════════")
    print(f"  fs9 WebSocket API — End-to-End Test")
    print(f"  Target: {url}")
    print(f"═══════════════════════════════════════════════════════\n")

    # --- Pre-auth tests (separate connections) ---
    print("▸ Pre-auth tests")
    for name, fn in [
        ("auth_failure", lambda: test_auth_failure(args.host, args.port)),
        ("no_auth_operation", lambda: test_no_auth_operation(args.host, args.port)),
    ]:
        try:
            fn()
            passed += 1
        except Exception as e:
            failed += 1
            errors.append((name, str(e)))
            print(f"  ✗ {name} FAILED — {e}")

    # --- Main session tests ---
    print("\n▸ Authentication")
    ws = websocket.WebSocket()
    ws.connect(url)
    try:
        test_auth(ws, args.user, args.password)
        passed += 1
    except Exception as e:
        failed += 1
        errors.append(("auth", str(e)))
        print(f"  ✗ auth FAILED — {e}")
        ws.close()
        print(f"\n{'='*55}")
        print(f"  RESULTS: {passed} passed, {failed} failed")
        print(f"{'='*55}")
        sys.exit(1)

    # Use a unique test directory to avoid collisions
    test_dir = f"/ws_test_{int(time.time())}"
    test_file = f"{test_dir}/hello.txt"
    test_file2 = f"{test_dir}/data.bin"
    test_subdir = f"{test_dir}/sub"
    test_nested = f"{test_dir}/a/b/c"
    stream_file = f"{test_dir}/large.bin"

    tests = [
        # Security tests
        ("double_auth",        lambda: test_double_auth(ws)),
        ("path_traversal",     lambda: test_path_traversal(ws)),
        ("relative_path",      lambda: test_relative_path(ws)),

        # Directory operations
        ("mkdir",              lambda: test_mkdir(ws, test_dir)),
        ("mkdir_recursive",    lambda: test_mkdir(ws, test_nested, recursive=True)),
        ("stat_dir",           lambda: test_stat(ws, test_dir, expect_type="dir")),
        ("stat_nested",        lambda: test_stat(ws, test_nested, expect_type="dir")),

        # Write + Read
        ("write",              lambda: test_write(ws, test_file, b"Hello, WebSocket fs9!")),
        ("stat_file",          lambda: test_stat(ws, test_file, expect_type="file", expect_size=21)),
        ("read",               lambda: test_read(ws, test_file, b"Hello, WebSocket fs9!")),
        ("read_at",            lambda: test_read_at(ws, test_file, 7, 9, b"WebSocket")),

        # Pwrite + Append
        ("pwrite",             lambda: test_pwrite(ws, test_file, 7, b"WEBSOCKET")),
        ("read_after_pwrite",  lambda: test_read(ws, test_file, b"Hello, WEBSOCKET fs9!")),
        ("append",             lambda: test_append(ws, test_file, b" - appended")),
        ("read_after_append",  lambda: test_read(ws, test_file, b"Hello, WEBSOCKET fs9! - appended")),

        # Truncate
        ("truncate",           lambda: test_truncate(ws, test_file, 5)),
        ("read_after_truncate", lambda: test_read(ws, test_file, b"Hello")),
        ("stat_after_truncate", lambda: test_stat(ws, test_file, expect_size=5)),

        # Write another file for readdir
        ("write_file2",        lambda: test_write(ws, test_file2, b"\x00\x01\x02\x03")),

        # Readdir
        ("readdir",            lambda: test_readdir(ws, test_dir)),

        # Unlink
        ("unlink",             lambda: test_unlink(ws, test_file)),
        ("stat_after_unlink",  lambda: test_stat_not_found(ws, test_file)),

        # Streaming write + read (>= 1MB)
        ("streaming_write_read", lambda: test_streaming_write_read(ws, stream_file, 1024 * 1024 + 512)),

        # Cleanup: rm recursive
        ("rm_recursive",       lambda: test_rm(ws, test_dir, recursive=True)),
        ("stat_after_rm",      lambda: test_stat_not_found(ws, test_dir)),
    ]

    current_section = ""
    for name, fn in tests:
        # Section headers
        if name in ("double_auth",):
            print("\n▸ Security validation")
        elif name == "mkdir":
            print("\n▸ Directory operations")
        elif name == "write":
            print("\n▸ Write + Read")
        elif name == "pwrite":
            print("\n▸ Pwrite + Append")
        elif name == "truncate":
            print("\n▸ Truncate")
        elif name == "readdir":
            print("\n▸ Readdir")
        elif name == "unlink":
            print("\n▸ Unlink")
        elif name == "streaming_write_read":
            print("\n▸ Streaming (large file ≥ 1MB)")
        elif name == "rm_recursive":
            print("\n▸ Cleanup")

        try:
            fn()
            passed += 1
        except Exception as e:
            failed += 1
            errors.append((name, str(e)))
            print(f"  ✗ {name} FAILED — {e}")

    ws.close()

    # --- Summary ---
    print(f"\n{'═'*55}")
    if failed == 0:
        print(f"  ✅ ALL PASSED: {passed} tests")
    else:
        print(f"  ❌ RESULTS: {passed} passed, {failed} failed")
        print(f"\n  Failures:")
        for name, err in errors:
            print(f"    • {name}: {err}")
    print(f"{'═'*55}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
