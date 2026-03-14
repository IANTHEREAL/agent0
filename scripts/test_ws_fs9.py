#!/usr/bin/env python3
"""
End-to-end test client for the fs9 WebSocket API.

This covers:
- base filesystem operations
- bounded batch operations (`batch_write`, `batch_stat`)
- default inline-path streaming coverage that does not require S3
- optional object-path validation (`create_upload` / `presign_part` /
  `complete_upload` / `prepare_download`) plus large-file streaming routing
- optional 4 KiB small-file benchmark for the current batch implementation

Examples:
    python3 scripts/test_ws_fs9.py
    python3 scripts/test_ws_fs9.py --include-object
    python3 scripts/test_ws_fs9.py --bench-small-files 256
    python3 scripts/test_ws_fs9.py --url ws://127.0.0.1:15480

Dev TiKV + S3 runbook (the setup used for fs9-v2 branch validation):
    Run inside a dev pod that can:
    - reach PD at
      serverless-cluster-pd.tidb-serverless.svc.cluster.local:2379
    - read mounted TiKV client certs at /tls
    - access S3 through the db9-fs-access service account / dev credentials

    Recommended server env:
        export PD_ENDPOINTS=serverless-cluster-pd.tidb-serverless.svc.cluster.local:2379
        export FS9_TEST_TENANT=fs9v2_${USER}_$(date +%s)
        export PG_KEYSPACE=db9_tenant_${FS9_TEST_TENANT}
        export TIKV_CA_PATH=/tls/ca.crt
        export TIKV_CERT_PATH=/tls/tls.crt
        export TIKV_KEY_PATH=/tls/tls.key

        export FS9_S3_BUCKET=dev-us-west-2-f02-db9-fs
        export FS9_S3_REGION=us-west-2
        export FS9_S3_PREFIX=bench/fs9v2/current-branch/$USER/$(date +%s)
        export FS9_UPLOAD_TOKEN_SECRET=fs9-v2-dev-branch-secret

        export FS9_OBJECT_MIN=1MiB
        export FS9_INLINE_MAX=64KiB
        export FS9_BATCH_WRITE_MAX_FILES=32
        export FS9_BATCH_WRITE_MAX_TOTAL_BYTES=4MiB
        export FS9_BATCH_WRITE_MAX_ENCODED_BYTES=1MiB

        export DB9_DEV=1
        export DB9_DEV_ADMIN_PASSWORD=admin
        export FS9_WS_LISTEN_ADDR=0.0.0.0
        export FS9_WS_PORT=15480

        # Latest fs9-v2 rollout is format-versioned. Reuse neither an old
        # TiKV keyspace nor an old S3 prefix when validating a new branch tip.
        # The WS script derives auth tenant from PG_KEYSPACE when it is in the
        # standard db9_tenant_<tenant> form, so the default smoke can use the
        # same fresh tenant automatically.

    Start the current branch:
        cargo run

    Smoke:
        # Core smoke is intentionally S3-independent. It covers the inline path,
        # including WS binary streaming for a small file. Large-file/object cases
        # moved behind --include-object after fs9-v2 removed the old published
        # pagefs fallback; without S3, files above FS9_INLINE_MAX are expected
        # to fail at stream init.
        python3 scripts/test_ws_fs9.py --url ws://127.0.0.1:15480
        python3 scripts/test_ws_fs9.py --url ws://127.0.0.1:15480 --include-object

    Small-file batch benchmark:
        python3 scripts/test_ws_fs9.py \
          --url ws://127.0.0.1:15480 \
          --bench-small-files 256 \
          --small-file-size 4096 \
          --write-batch-size 32 \
          --stat-batch-size 256
"""

import argparse
import base64
import hashlib
import json
import ssl
import struct
import sys
import time
import urllib.error
import urllib.request

try:
    import websocket
except ImportError:
    websocket = None

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

def require_websocket_client():
    if websocket is None:
        raise RuntimeError(
            "python package 'websocket-client' is required; install with "
            "`python3 -m pip install websocket-client`"
        )
    return websocket

def connect_ws(url, insecure_tls=False):
    """Open a WebSocket connection, optionally disabling TLS verification."""
    wsmod = require_websocket_client()
    ws = wsmod.WebSocket()
    kwargs = {}
    if url.startswith("wss://") and insecure_tls:
        kwargs["sslopt"] = {"cert_reqs": ssl.CERT_NONE}
    ws.connect(url, **kwargs)
    return ws

def send_binary(ws, data):
    """Send a binary frame."""
    wsmod = require_websocket_client()
    ws.send(data, opcode=wsmod.ABNF.OPCODE_BINARY)

def recv_any(ws):
    """Receive a frame; return (is_binary, data)."""
    wsmod = require_websocket_client()
    opcode, data = ws.recv_data()
    is_binary = (opcode == wsmod.ABNF.OPCODE_BINARY)
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

def http_request(method, url, headers=None, body=None, timeout=120):
    """Issue an HTTP request and return (status_code, body, response_headers)."""
    req = urllib.request.Request(url, data=body, method=method)
    for name, value in headers or []:
        req.add_header(name, value)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            resp_body = resp.read()
            resp_headers = {k.lower(): v for k, v in resp.headers.items()}
            return resp.status, resp_body, resp_headers
    except urllib.error.HTTPError as err:
        resp_body = err.read()
        resp_headers = {k.lower(): v for k, v in err.headers.items()}
        return err.code, resp_body, resp_headers

def encode_base64(data):
    return base64.b64encode(data).decode()

def make_pattern_bytes(size_bytes):
    # Deterministic payloads simplify checksum and slice verification.
    return bytes((idx % 251) for idx in range(size_bytes))

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

def test_readdir_exact(ws, path, expected_paths):
    """Test readdir and require an exact child-path set."""
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "readdir", "path": path})
    assert_ok(resp, f"readdir_exact {path}")
    actual_paths = sorted(entry["path"] for entry in resp["data"]["entries"])
    expected_paths = sorted(expected_paths)
    if actual_paths != expected_paths:
        missing = sorted(set(expected_paths) - set(actual_paths))
        extra = sorted(set(actual_paths) - set(expected_paths))
        raise AssertionError(
            f"readdir exact mismatch for {path}: actual={actual_paths} "
            f"expected={expected_paths} missing={missing} extra={extra}"
        )
    print(f"  ✓ readdir exact OK — {path} ({len(actual_paths)} entries)")
    return resp["data"]["entries"]

def test_readdir_recursive_exact(
    ws,
    path,
    expected_paths,
    max_depth=None,
    max_entries=None,
    expected_truncated=False,
    expected_dirs_scanned=None,
):
    """Test recursive readdir and require an exact descendant-path set."""
    rid = next_id()
    payload = {"id": rid, "op": "readdir_recursive", "path": path}
    if max_depth is not None:
        payload["max_depth"] = max_depth
    if max_entries is not None:
        payload["max_entries"] = max_entries
    resp = send_json(ws, payload)
    assert_ok(resp, f"readdir_recursive {path}")
    data = resp["data"]
    actual_paths = sorted(entry["path"] for entry in data["entries"])
    expected_paths = sorted(expected_paths)
    if actual_paths != expected_paths:
        missing = sorted(set(expected_paths) - set(actual_paths))
        extra = sorted(set(actual_paths) - set(expected_paths))
        raise AssertionError(
            f"readdir_recursive exact mismatch for {path}: actual={actual_paths} "
            f"expected={expected_paths} missing={missing} extra={extra}"
        )
    assert data["truncated"] is expected_truncated, (
        f"readdir_recursive truncated mismatch for {path}: "
        f"{data['truncated']} != {expected_truncated}"
    )
    if expected_dirs_scanned is not None:
        assert data["total_dirs_scanned"] == expected_dirs_scanned, (
            f"readdir_recursive dirs_scanned mismatch for {path}: "
            f"{data['total_dirs_scanned']} != {expected_dirs_scanned}"
        )
    print(
        f"  ✓ readdir recursive OK — {path} "
        f"({len(actual_paths)} entries, dirs_scanned={data['total_dirs_scanned']})"
    )
    return data["entries"]

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

def ws_batch_write(ws, files):
    rid = next_id()
    payload = {
        "id": rid,
        "op": "batch_write",
        "files": [
            {
                "path": path,
                "content": encode_base64(data),
                "encoding": "base64",
            }
            for path, data in files
        ],
    }
    resp = send_json(ws, payload)
    assert_ok(resp, "batch_write")
    return resp["data"]["entries"]

def ws_batch_stat(ws, paths):
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "batch_stat", "paths": paths})
    assert_ok(resp, "batch_stat")
    return resp["data"]["entries"]

def ws_batch_inline_read(ws, paths):
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "batch_inline_read", "paths": paths})
    assert_ok(resp, "batch_inline_read")
    return resp["data"]["entries"]

def test_batch_write(ws, files):
    entries = ws_batch_write(ws, files)
    by_path = {entry["path"]: entry for entry in entries}
    for path, data in files:
        entry = by_path.get(path)
        assert entry is not None, f"missing batch_write entry for {path}"
        assert entry["ok"] is True, f"batch_write entry failed for {path}: {entry}"
        assert entry["written"] == len(data), f"batch_write written mismatch for {path}"
    print(f"  ✓ batch_write OK — {len(files)} files")

def test_batch_stat(ws, expected_files, expected_storage=None, expected_sealed=None):
    entries = ws_batch_stat(ws, [path for path, _ in expected_files])
    by_path = {entry["path"]: entry for entry in entries}
    for path, data in expected_files:
        entry = by_path.get(path)
        assert entry is not None, f"missing batch_stat entry for {path}"
        assert entry["ok"] is True, f"batch_stat entry failed for {path}: {entry}"
        info = entry["info"]
        assert info["size"] == len(data), f"batch_stat size mismatch for {path}"
        if expected_storage is not None:
            assert info.get("storage") == expected_storage, (
                f"batch_stat storage mismatch for {path}: {info.get('storage')} != {expected_storage}"
            )
        if expected_sealed is not None:
            assert info.get("sealed") == expected_sealed, (
                f"batch_stat sealed mismatch for {path}: {info.get('sealed')} != {expected_sealed}"
            )
    print(f"  ✓ batch_stat OK — {len(expected_files)} files")

def test_batch_readback(ws, expected_files):
    for path, data in expected_files:
        test_read(ws, path, data)
    if expected_files:
        sample_path, sample_data = expected_files[min(1, len(expected_files) - 1)]
        if sample_data:
            offset = 1 if len(sample_data) > 1 else 0
            length = min(3, len(sample_data) - offset)
            if length > 0:
                test_read_at(
                    ws,
                    sample_path,
                    offset,
                    length,
                    sample_data[offset:offset + length],
                )
    print(f"  ✓ batch_readback OK — {len(expected_files)} files")

def test_batch_inline_read(ws, expected_files):
    entries = ws_batch_inline_read(ws, [path for path, _ in expected_files])
    by_path = {entry["path"]: entry for entry in entries}
    for path, expected in expected_files:
        entry = by_path.get(path)
        assert entry is not None, f"missing batch_inline_read entry for {path}"
        assert entry["ok"] is True, f"batch_inline_read entry failed for {path}: {entry}"
        assert entry["encoding"] == "base64", f"unexpected encoding for {path}: {entry['encoding']}"
        assert entry["size"] == len(expected), f"size mismatch for {path}"
        actual = base64.b64decode(entry["content"])
        assert actual == expected, f"content mismatch for {path}"
    print(f"  ✓ batch_inline_read OK — {len(expected_files)} files")

def test_batch_sealed_mutation_boundary(ws, expected_files):
    if not expected_files:
        raise AssertionError("batch sealed mutation boundary requires at least one file")
    sample_path, _ = expected_files[0]
    test_object_sealed_mutation_rejected(ws, sample_path)
    print(f"  ✓ batch sealed mutation boundary OK — {sample_path}")

def parse_optional_bool(raw):
    if raw is None:
        return None
    value = raw.strip().lower()
    if value in ("true", "1", "yes", "y"):
        return True
    if value in ("false", "0", "no", "n"):
        return False
    raise argparse.ArgumentTypeError(f"invalid boolean value: {raw}")

def ws_create_upload(ws, path, size):
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "create_upload", "path": path, "size": size})
    assert_ok(resp, f"create_upload {path}")
    return resp["data"]

def ws_presign_part(ws, upload_token, part_number):
    rid = next_id()
    resp = send_json(ws, {
        "id": rid,
        "op": "presign_part",
        "upload_token": upload_token,
        "part_number": part_number,
    })
    assert_ok(resp, f"presign_part {part_number}")
    return resp["data"]

def ws_complete_upload(ws, upload_token, parts, checksum_hex):
    rid = next_id()
    resp = send_json(ws, {
        "id": rid,
        "op": "complete_upload",
        "upload_token": upload_token,
        "parts": parts,
        "checksum": f"sha256:{checksum_hex}",
    })
    assert_ok(resp, "complete_upload")
    return resp["data"]

def ws_prepare_download(ws, path):
    rid = next_id()
    resp = send_json(ws, {"id": rid, "op": "prepare_download", "path": path})
    assert_ok(resp, "prepare_download")
    return resp["data"]

def test_object_sealed_mutation_rejected(ws, path):
    pwrite_resp = send_json(ws, {
        "id": next_id(),
        "op": "pwrite",
        "path": path,
        "offset": 0,
        "content": encode_base64(b"X"),
        "encoding": "base64",
    })
    assert_err(pwrite_resp, "EINVAL", f"sealed_pwrite {path}")

    append_resp = send_json(ws, {
        "id": next_id(),
        "op": "append",
        "path": path,
        "content": encode_base64(b"Y"),
        "encoding": "base64",
    })
    assert_err(append_resp, "EINVAL", f"sealed_append {path}")

    truncate_resp = send_json(ws, {
        "id": next_id(),
        "op": "truncate",
        "path": path,
        "size": 1,
    })
    assert_err(truncate_resp, "EINVAL", f"sealed_truncate {path}")
    print(f"  ✓ sealed mutation boundary OK — {path}")

def test_object_roundtrip(ws, path, size_bytes):
    data = make_pattern_bytes(size_bytes)
    checksum_hex = hashlib.sha256(data).hexdigest()

    create = ws_create_upload(ws, path, len(data))
    upload_token = create["upload_token"]
    part_size = create["part_size"]
    assert part_size > 0, "part_size must be positive"

    completed_parts = []
    for index, offset in enumerate(range(0, len(data), part_size), start=1):
        chunk = data[offset:offset + part_size]
        presigned = ws_presign_part(ws, upload_token, index)
        status, body, resp_headers = http_request(
            presigned["method"],
            presigned["url"],
            [(h["name"], h["value"]) for h in presigned.get("headers", [])],
            chunk,
        )
        assert status in (200, 201), (
            f"upload part {index} failed: status={status} body={body[:200]!r}"
        )
        etag = resp_headers.get("etag")
        assert etag, f"missing etag for uploaded part {index}"
        completed_parts.append({"part_number": index, "etag": etag})

    complete = ws_complete_upload(ws, upload_token, completed_parts, checksum_hex)
    assert complete["written"] == len(data), "complete_upload written mismatch"

    stat = test_stat(ws, path, expect_type="file", expect_size=len(data))
    assert stat.get("storage") == "object", f"expected object storage, got {stat.get('storage')}"
    assert stat.get("sealed") is True, f"expected sealed=true, got {stat.get('sealed')}"

    mid = len(data) // 2
    slice_len = min(256, len(data) - mid)
    test_read_at(ws, path, mid, slice_len, data[mid:mid + slice_len])

    prepared = ws_prepare_download(ws, path)
    assert prepared["storage"] == "object", f"unexpected download storage {prepared['storage']}"
    assert prepared["range_supported"] is True, "object download must support ranges"

    download_headers = [(h["name"], h["value"]) for h in prepared.get("headers", [])]
    status, body, _ = http_request(prepared["method"], prepared["url"], download_headers)
    assert status == 200, f"prepare_download GET failed: {status}"
    assert body == data, "prepare_download body mismatch"

    range_headers = download_headers + [("Range", f"bytes={mid}-{mid + slice_len - 1}")]
    status, range_body, _ = http_request(prepared["method"], prepared["url"], range_headers)
    assert status in (200, 206), f"range GET failed: {status}"
    if status == 206:
        assert range_body == data[mid:mid + slice_len], "range GET body mismatch"
    else:
        assert range_body == data, "full-body fallback mismatch"

    test_object_sealed_mutation_rejected(ws, path)
    print(
        f"  ✓ object roundtrip OK — {path} ({len(data)} bytes, {len(completed_parts)} part(s))"
    )

def chunked(seq, size):
    for start in range(0, len(seq), size):
        yield seq[start:start + size]

def benchmark_small_files(ws, base_dir, file_count, file_size, write_batch_size, stat_batch_size):
    print(
        f"  → benchmark config: files={file_count}, file_size={file_size}, "
        f"write_batch_size={write_batch_size}, stat_batch_size={stat_batch_size}"
    )

    seq_dir = f"{base_dir}/seq"
    batch_dir = f"{base_dir}/batch"
    test_mkdir(ws, seq_dir, recursive=True)
    test_mkdir(ws, batch_dir, recursive=True)

    files = []
    for idx in range(file_count):
        payload = make_pattern_bytes(file_size)
        files.append((f"{seq_dir}/f{idx:04d}.bin", payload))
    batch_files = [(path.replace(seq_dir, batch_dir, 1), data) for path, data in files]

    start = time.perf_counter()
    for path, data in files:
        rid = next_id()
        resp = send_json(ws, {
            "id": rid,
            "op": "write",
            "path": path,
            "content": encode_base64(data),
            "encoding": "base64",
        })
        assert_ok(resp, f"bench_write {path}")
    seq_write_secs = time.perf_counter() - start

    start = time.perf_counter()
    for group in chunked(batch_files, write_batch_size):
        entries = ws_batch_write(ws, group)
        assert len(entries) == len(group), "batch_write returned unexpected entry count"
        for entry in entries:
            assert entry["ok"] is True, f"batch_write bench entry failed: {entry}"
    batch_write_secs = time.perf_counter() - start

    seq_paths = [path for path, _ in files]
    batch_paths = [path for path, _ in batch_files]

    start = time.perf_counter()
    for path in seq_paths:
        rid = next_id()
        resp = send_json(ws, {"id": rid, "op": "stat", "path": path})
        assert_ok(resp, f"bench_stat {path}")
    seq_stat_secs = time.perf_counter() - start

    start = time.perf_counter()
    for group in chunked(batch_paths, stat_batch_size):
        entries = ws_batch_stat(ws, group)
        assert len(entries) == len(group), "batch_stat returned unexpected entry count"
        for entry in entries:
            assert entry["ok"] is True, f"batch_stat bench entry failed: {entry}"
    batch_stat_secs = time.perf_counter() - start

    start = time.perf_counter()
    for group in chunked(batch_paths, stat_batch_size):
        entries = ws_batch_inline_read(ws, group)
        assert len(entries) == len(group), "batch_inline_read returned unexpected entry count"
        for entry in entries:
            assert entry["ok"] is True, f"batch_inline_read bench entry failed: {entry}"
    batch_inline_read_secs = time.perf_counter() - start

    start = time.perf_counter()
    for path in batch_paths:
        rid = next_id()
        resp = send_json(ws, {"id": rid, "op": "read", "path": path})
        assert_ok(resp, f"bench_read {path}")
    seq_read_secs = time.perf_counter() - start

    total_mib = (file_count * file_size) / (1024 * 1024)

    def fmt(label, seconds):
        files_per_sec = (file_count / seconds) if seconds > 0 else float("inf")
        mib_per_sec = (total_mib / seconds) if seconds > 0 else float("inf")
        print(
            f"  ✓ {label:<18} {seconds:7.3f}s  "
            f"{files_per_sec:8.1f} files/s  {mib_per_sec:8.2f} MiB/s"
        )

    fmt("single write", seq_write_secs)
    fmt("batch_write", batch_write_secs)
    fmt("single stat", seq_stat_secs)
    fmt("batch_stat", batch_stat_secs)
    fmt("batch_inline_read", batch_inline_read_secs)
    fmt("single read", seq_read_secs)

def test_streaming_write_read(
    ws,
    path,
    size_bytes,
    write_frame_bytes=None,
    expected_storage=None,
):
    """Test streaming write followed by streaming read for a file of the given size."""
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
    frame_bytes = chunk_size
    if write_frame_bytes is not None:
        frame_bytes = max(1, min(write_frame_bytes, chunk_size))
    print(
        "  → streaming write started: "
        f"stream_id={stream_id}, chunk_size={chunk_size}, frame_bytes={frame_bytes}"
    )

    # Send binary chunks
    offset = 0
    chunks_sent = 0
    while offset < len(data):
        chunk = data[offset:offset + frame_bytes]
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
    stat = test_stat(ws, path, expect_type="file", expect_size=size_bytes)
    if expected_storage is not None:
        assert stat.get("storage") == expected_storage, (
            f"streaming write storage mismatch: {stat.get('storage')} != {expected_storage}"
        )

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

def test_auth_failure(url, insecure_tls=False):
    """Test that bad credentials get rejected and connection is closed."""
    ws = connect_ws(url, insecure_tls=insecure_tls)
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

def test_no_auth_operation(url, insecure_tls=False):
    """Test that sending an operation before auth gets rejected."""
    ws = connect_ws(url, insecure_tls=insecure_tls)
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

def require_psycopg():
    try:
        import psycopg
    except ImportError as exc:
        raise RuntimeError(
            "python package 'psycopg' is required for --sql-dsn; install with "
            "`python3 -m pip install psycopg[binary]`"
        ) from exc
    return psycopg

def connect_sql(dsn):
    psycopg = require_psycopg()
    return psycopg.connect(dsn, autocommit=True)

def test_sql_reads_ws_write(conn, path, expected_text):
    with conn.cursor() as cur:
        cur.execute("SELECT fs9_read(%s)", (path,))
        row = cur.fetchone()
    assert row is not None, "missing row for fs9_read"
    assert row[0] == expected_text, f"SQL fs9_read mismatch for {path}: {row[0]!r}"
    print(f"  ✓ SQL reads WS write OK — {path}")

def test_sql_write_ws_read(conn, ws, path, expected_text):
    with conn.cursor() as cur:
        cur.execute("SELECT fs9_write(%s, %s)", (path, expected_text))
        row = cur.fetchone()
    expected_bytes = expected_text.encode()
    assert row is not None, "missing row for fs9_write"
    assert row[0] == len(expected_bytes), (
        f"SQL fs9_write returned {row[0]} != {len(expected_bytes)}"
    )
    test_read(ws, path, expected_bytes)
    print(f"  ✓ WS reads SQL write OK — {path}")

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(description="fs9 WebSocket API test client")
    parser.add_argument("--url", help="Explicit ws:// or wss:// URL")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=15480)
    parser.add_argument("--user", default="admin")
    parser.add_argument("--password", default="admin")
    parser.add_argument(
        "--insecure-tls",
        action="store_true",
        help="Disable certificate verification for wss:// connections",
    )
    parser.add_argument(
        "--include-object",
        action="store_true",
        help="Run object-path tests (requires S3-enabled server configuration)",
    )
    parser.add_argument(
        "--object-size",
        type=int,
        default=8 * 1024 * 1024 + 123,
        help="Payload size for object-path tests in bytes",
    )
    parser.add_argument(
        "--bench-small-files",
        type=int,
        default=0,
        metavar="COUNT",
        help="Run a 4 KiB small-file benchmark with COUNT files",
    )
    parser.add_argument(
        "--small-file-size",
        type=int,
        default=4096,
        help="Per-file size for --bench-small-files (default: 4096)",
    )
    parser.add_argument(
        "--write-batch-size",
        type=int,
        default=32,
        help="Files per request for batch_write benchmark mode",
    )
    parser.add_argument(
        "--stat-batch-size",
        type=int,
        default=256,
        help="Paths per request for batch_stat benchmark mode",
    )
    parser.add_argument(
        "--expected-batch-storage",
        choices=["inline", "pack", "object", "legacy"],
        default=None,
        help="Assert batch_stat storage for the smoke batch files",
    )
    parser.add_argument(
        "--expected-batch-sealed",
        type=parse_optional_bool,
        default=None,
        metavar="BOOL",
        help="Assert batch_stat sealed flag for the smoke batch files",
    )
    parser.add_argument(
        "--sql-dsn",
        default=None,
        help="Optional PostgreSQL DSN for SQL/WS fs9 interoperability checks",
    )
    args = parser.parse_args()

    url = args.url or f"ws://{args.host}:{args.port}"
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
        ("auth_failure", lambda: test_auth_failure(url, insecure_tls=args.insecure_tls)),
        ("no_auth_operation", lambda: test_no_auth_operation(url, insecure_tls=args.insecure_tls)),
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
    ws = connect_ws(url, insecure_tls=args.insecure_tls)
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
    batch_dir = f"{test_dir}/batch"
    test_nested = f"{test_dir}/a/b/c"
    stream_inline_file = f"{test_dir}/stream-inline.bin"
    stream_large_file = f"{test_dir}/stream-large.bin"
    object_file = f"{test_dir}/large-object.bin"
    ws_sql_file = f"{test_dir}/ws-sql.txt"
    sql_ws_file = f"{test_dir}/sql-ws.txt"

    batch_files = [
        (f"{batch_dir}/alpha.txt", b"alpha"),
        (f"{batch_dir}/beta.bin", b"\x00\x01\x02\x03\x04"),
        (f"{batch_dir}/gamma.json", b'{"ok":true}'),
    ]

    sql_conn = None
    if args.sql_dsn:
        try:
            sql_conn = connect_sql(args.sql_dsn)
        except Exception as e:
            failed += 1
            errors.append(("sql_connect", str(e)))
            print(f"  ✗ sql_connect FAILED — {e}")

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

        # Batch ops
        ("mkdir_batch_dir",    lambda: test_mkdir(ws, batch_dir, recursive=True)),
        ("batch_write",        lambda: test_batch_write(ws, batch_files)),
        ("batch_stat",         lambda: test_batch_stat(
            ws,
            batch_files,
            expected_storage=args.expected_batch_storage,
            expected_sealed=args.expected_batch_sealed,
        )),
        ("batch_readback",     lambda: test_batch_readback(ws, batch_files)),
        ("batch_inline_read",  lambda: test_batch_inline_read(ws, batch_files)),

        # Readdir
        ("readdir",            lambda: test_readdir(ws, test_dir)),
        ("readdir_exact_root", lambda: test_readdir_exact(ws, test_dir, [
            f"{test_dir}/a",
            batch_dir,
            test_file,
            test_file2,
        ])),
        ("readdir_exact_batch", lambda: test_readdir_exact(
            ws,
            batch_dir,
            [path for path, _ in batch_files],
        )),
        ("readdir_recursive_exact", lambda: test_readdir_recursive_exact(
            ws,
            test_dir,
            [
                f"{test_dir}/a",
                f"{test_dir}/a/b",
                f"{test_dir}/a/b/c",
                batch_dir,
                test_file,
                test_file2,
                *[path for path, _ in batch_files],
            ],
            max_depth=8,
            expected_dirs_scanned=5,
        )),

        # Unlink
        ("unlink",             lambda: test_unlink(ws, test_file)),
        ("stat_after_unlink",  lambda: test_stat_not_found(ws, test_file)),

        # Streaming write + read on the inline path. This stays in the default
        # smoke so non-S3 setups still validate WS binary streaming end to end.
        ("streaming_inline_write_read", lambda: test_streaming_write_read(
            ws,
            stream_inline_file,
            16 * 1024 + 17,
            write_frame_bytes=4096,
            expected_storage="inline",
        )),

        # Cleanup: rm recursive
        ("rm_recursive",       lambda: test_rm(ws, test_dir, recursive=True)),
        ("stat_after_rm",      lambda: test_stat_not_found(ws, test_dir)),
    ]

    if args.include_object:
        tests = (
            tests[:-2]
            + [(
                "streaming_large_write_read",
                lambda: test_streaming_write_read(
                    ws,
                    stream_large_file,
                    1024 * 1024 + 512,
                    expected_storage="object",
                ),
            )]
            + [("object_roundtrip", lambda: test_object_roundtrip(ws, object_file, args.object_size))]
            + tests[-2:]
        )

    if args.expected_batch_sealed is True:
        insert_at = next(idx for idx, (name, _) in enumerate(tests) if name == "readdir")
        tests.insert(
            insert_at,
            ("batch_sealed_mutation_boundary", lambda: test_batch_sealed_mutation_boundary(ws, batch_files)),
        )

    if sql_conn is not None:
        insert_at = next(idx for idx, (name, _) in enumerate(tests) if name == "unlink")
        sql_tests = [
            ("write_ws_sql_file", lambda: test_write(ws, ws_sql_file, b"hello from ws to sql")),
            ("sql_reads_ws_write", lambda: test_sql_reads_ws_write(
                sql_conn,
                ws_sql_file,
                "hello from ws to sql",
            )),
            ("sql_write_ws_read", lambda: test_sql_write_ws_read(
                sql_conn,
                ws,
                sql_ws_file,
                "hello from sql to ws",
            )),
        ]
        tests[insert_at:insert_at] = sql_tests

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
        elif name == "mkdir_batch_dir":
            print("\n▸ Batch ops")
        elif name == "readdir":
            print("\n▸ Readdir")
        elif name == "write_ws_sql_file":
            print("\n▸ SQL/WS interoperability")
        elif name == "unlink":
            print("\n▸ Unlink")
        elif name == "streaming_inline_write_read":
            print("\n▸ Streaming (inline, no S3 required)")
        elif name == "streaming_large_write_read":
            print("\n▸ Streaming (large/object-backed)")
        elif name == "object_roundtrip":
            print("\n▸ Object path (S3 + presigned multipart)")
        elif name == "rm_recursive":
            print("\n▸ Cleanup")

        try:
            fn()
            passed += 1
        except Exception as e:
            failed += 1
            errors.append((name, str(e)))
            print(f"  ✗ {name} FAILED — {e}")

    if failed == 0 and args.bench_small_files > 0:
        print("\n▸ 4 KiB small-file benchmark")
        bench_dir = f"/ws_bench_{int(time.time())}"
        try:
            benchmark_small_files(
                ws,
                bench_dir,
                args.bench_small_files,
                args.small_file_size,
                args.write_batch_size,
                args.stat_batch_size,
            )
            test_rm(ws, bench_dir, recursive=True)
            passed += 1
        except Exception as e:
            failed += 1
            errors.append(("bench_small_files", str(e)))
            print(f"  ✗ bench_small_files FAILED — {e}")

    if sql_conn is not None:
        sql_conn.close()
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
