# fs9 v2 M0 Spike D: FUSE v1 Object-Read Policy (Dev Validation)

Date: 2026-03-11

Goal: freeze the pre-Phase-4 policy for **object-backed** files when accessed via the current **whole-file** APIs (`read_file`/`write_file`) used by the existing `db9 fs mount` (FUSE v1) and `db9 fs cp`.

This spike is not about final object performance. It is about preventing a bad product boundary where a large object is accidentally pulled through a whole-file path that buffers the entire file in memory.

## What We Verified

### 1) Current clients are whole-file

In `db9-cli` today:

- FUSE v1 `open()` calls `remote_read_file()` and buffers the full file into a `Vec<u8>` per open handle.
- `db9 fs cp` uploads via `std::fs::read(local)` (whole-file in memory), and downloads via `client.read_file(remote)` (whole-file in memory).

Even when the WS protocol uses binary frames (“streaming read/write”), the client still materializes a full buffer.

### 2) Dev server enforces a hard whole-file size cap today

On **dev** (`DB9_API_URL=https://dev.db9.ai/api`) as of **2026-03-11**, uploading a large file via `db9 fs cp` fails with:

- `EFBIG: file too large: 268435456 bytes exceeds limit 10485760`

So the currently deployed dev server caps whole-file operations at **10 MiB** (10,485,760 bytes).

Note: the local `db9-server` repo currently defines `MAX_BYTES_PER_FILE = 100 * 1024 * 1024` (100 MiB). The important conclusion for fs9 v2 is unchanged: **whole-file APIs must remain bounded** and **must not be the access path for large objects**.

### 3) A file can exceed the cap via `append`, but `read_file` still fails (dev proof)

We created a file larger than the cap using multiple append operations (which do not enforce the cap), then verified:

- `stat` returns a size over the cap (`11777900` bytes, about 11.2 MiB)
- `db9 fs cp <remote> <local>` fails with `EFBIG: exceeded max 10485760 bytes`

This confirms the product reality: **metadata can represent large files**, but **whole-file read paths must refuse**.

## Decision (Frozen for Pre-Phase-4)

Choose **size guard** (not “read-only presigned fast path”) for the current FUSE v1 / whole-file API surface.

### Policy

- Treat `read_file` / `write_file` as a **small-file convenience API only**.
- For `DataRef::Object` (and later `DataRef::PackEntry` if the entry is larger than the whole-file limit), enforce:
  - If `size > FS9_WHOLE_FILE_MAX_BYTES`: return `EFBIG` with a message that points users to the large-file path (`presigned` / multipart cp).
- Do not attempt to “make FUSE v1 work” for object-backed large files by piping presigned URLs through the existing handle model. That requires a handle-based redesign (FUSE v2) or range-read plumbing.

### Default Value

- `FS9_WHOLE_FILE_MAX_BYTES`: keep bounded and explicit.
  - Deployed dev observed: **10 MiB**
  - Repo constant today: **100 MiB**
  - Recommendation for fs9 v2: keep the default conservative and treat it as a tunable guardrail, but do not rely on it for large-file workflows.

## Implication for the fs9 v2 Rollout

- Object-backed large files must ship together with:
  - presigned multipart upload API
  - presigned download / range GET support (CLI first)
- Before that, fs9 v2 should avoid exposing `DataRef::Object` to current clients in a way that triggers whole-file `read_file`.

