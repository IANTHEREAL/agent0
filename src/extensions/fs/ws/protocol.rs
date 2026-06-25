use std::fmt::{Display, Formatter};

use anyhow::Error;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::extensions::fs::backend::{FsFileInfo, FsStorage};
use crate::extensions::fs::embedded::types::EmbeddedFsError;

pub(crate) const STREAMING_THRESHOLD: usize = 1024 * 1024;
pub(crate) const DEFAULT_CHUNK_SIZE: usize = 64 * 1024;
pub(crate) const AUTH_TIMEOUT_SECS: u64 = 10;
/// WebSocket idle timeout.
///
/// Note: during presigned multipart uploads, the data plane runs directly between the client and
/// S3 and may produce long periods of no WS traffic. Clients must send periodic WS pings while
/// uploading parts so they can keep the control-plane connection alive for `complete_upload` /
/// `abort_upload`.
pub(crate) const IDLE_TIMEOUT_SECS: u64 = 300;
pub(crate) const DEFAULT_MAX_CONNECTIONS_PER_TENANT: u32 = 50;
/// Hard cap on a single WS JSON text frame.
///
/// This bounds request buffering + JSON parsing costs. Batch APIs have additional per-operation
/// size limits and should be tuned alongside this cap if larger payloads are desired.
pub(crate) const MAX_JSON_FRAME_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const DEFAULT_WS_PORT: u16 = 5480;
pub(crate) const DEFAULT_WS_LISTEN_ADDR: &str = "127.0.0.1";

fn default_encoding() -> String {
    "base64".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum WsErrorCode {
    Enoent,
    Eisdir,
    Enotdir,
    Eexist,
    Enotempty,
    Eacces,
    Efbig,
    Eagain,
    Eauth,
    Einval,
    Eproto,
    Enosys,
    Eio,
}

impl Display for WsErrorCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            Self::Enoent => "File or directory not found",
            Self::Eisdir => "Is a directory",
            Self::Enotdir => "Not a directory",
            Self::Eexist => "File or directory already exists",
            Self::Enotempty => "Directory not empty",
            Self::Eacces => "Permission denied",
            Self::Efbig => "File too large",
            Self::Eagain => "Resource temporarily unavailable",
            Self::Eauth => "Authentication failed",
            Self::Einval => "Invalid argument",
            Self::Eproto => "Protocol error",
            Self::Enosys => "Function not implemented",
            Self::Eio => "I/O error",
        };
        write!(f, "{msg}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub(crate) enum WsRequest {
    Auth {
        id: String,
        username: String,
        password: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        database: Option<String>,
    },
    Stat {
        id: String,
        path: String,
    },
    Readdir {
        id: String,
        path: String,
    },
    #[serde(rename = "readdir_recursive")]
    ReaddirRecursive {
        id: String,
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_depth: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max_entries: Option<usize>,
    },
    Mkdir {
        id: String,
        path: String,
        #[serde(default)]
        recursive: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
    },
    Unlink {
        id: String,
        path: String,
    },
    Rm {
        id: String,
        path: String,
        #[serde(default)]
        recursive: bool,
    },
    Read {
        id: String,
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        offset: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        length: Option<usize>,
        #[serde(default)]
        streaming: bool,
    },
    Write {
        id: String,
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default = "default_encoding")]
        encoding: String,
        #[serde(default)]
        streaming: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
    },
    Pwrite {
        id: String,
        path: String,
        offset: u64,
        content: String,
        #[serde(default = "default_encoding")]
        encoding: String,
    },
    Append {
        id: String,
        path: String,
        content: String,
        #[serde(default = "default_encoding")]
        encoding: String,
    },
    Truncate {
        id: String,
        path: String,
        size: u64,
    },
    Rename {
        id: String,
        old_path: String,
        new_path: String,
    },
    #[serde(rename = "create_upload")]
    CreateUpload {
        id: String,
        path: String,
        size: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checksum_algorithm: Option<String>,
    },
    #[serde(rename = "presign_part")]
    PresignPart {
        id: String,
        upload_token: String,
        part_number: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checksum_crc32c: Option<String>,
    },
    /// Batch presign: return presigned URLs for multiple part numbers in one RPC.
    #[serde(rename = "presign_parts")]
    PresignParts {
        id: String,
        upload_token: String,
        parts: Vec<PresignPartEntry>,
    },
    #[serde(rename = "complete_upload")]
    CompleteUpload {
        id: String,
        upload_token: String,
        parts: Vec<MultipartCompletedPartRequest>,
        #[serde(skip_serializing_if = "Option::is_none")]
        checksum: Option<String>,
    },
    #[serde(rename = "abort_upload")]
    AbortUpload {
        id: String,
        upload_token: String,
    },
    #[serde(rename = "prepare_download")]
    PrepareDownload {
        id: String,
        path: String,
    },
    Symlink {
        id: String,
        path: String,
        target: String,
    },
    Readlink {
        id: String,
        path: String,
    },
    Chmod {
        id: String,
        path: String,
        mode: u32,
    },
    /// BatchStat is a bounded helper that returns per-path results.
    ///
    /// The top-level WS response `ok` only indicates request-level parsing/validation success.
    /// Callers must inspect each entry.
    ///
    /// Security invariant: fs9 WS is currently superuser-only (see `ws/auth.rs`). If that is ever
    /// relaxed, batch APIs become a namespace enumeration oracle and must be revisited.
    #[serde(rename = "batch_stat")]
    BatchStat {
        id: String,
        paths: Vec<String>,
    },
    /// BatchInlineRead is a bounded helper for latency-sensitive tiny reads.
    ///
    /// It returns per-path results and is intentionally "inline-only": it does not use WS streaming
    /// and will reject entries over the configured size caps.
    ///
    /// The top-level WS response `ok` only indicates request-level parsing/validation success;
    /// callers must inspect each entry.
    ///
    /// Security invariant: fs9 WS is currently superuser-only (see `ws/auth.rs`). If that is ever
    /// relaxed, batch APIs become a namespace enumeration oracle and must be revisited.
    #[serde(rename = "batch_inline_read")]
    BatchInlineRead {
        id: String,
        paths: Vec<String>,
    },
    /// BatchWrite is a bounded, non-atomic convenience API for small inline-sized full replacement
    /// writes only.
    ///
    /// It may partially succeed: some entries can be written even if later entries fail. The
    /// top-level WS response `ok` only indicates request-level parsing/validation success; callers
    /// must inspect each entry.
    ///
    /// Security invariant: fs9 WS is currently superuser-only (see `ws/auth.rs`). If that is ever
    /// relaxed, batch APIs must be reviewed.
    #[serde(rename = "batch_write")]
    BatchWrite {
        id: String,
        files: Vec<BatchWriteFileRequest>,
    },
    /// BatchWriteAtomic is a capability-gated fast-path for bulk small-file uploads.
    ///
    /// Files are grouped by parent directory and each subgroup (bounded by
    /// `grouped_write_subgroup_size`) is committed atomically in a single TiKV
    /// transaction. Per-subgroup atomic semantics.
    ///
    /// Security invariant: fs9 WS is currently superuser-only (see `ws/auth.rs`).
    #[serde(rename = "batch_write_atomic")]
    BatchWriteAtomic {
        id: String,
        files: Vec<BatchWriteFileRequest>,
    },
    #[serde(rename = "watch_subscribe")]
    WatchSubscribe {
        id: String,
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        since_seq: Option<u64>,
    },
    #[serde(rename = "watch_unsubscribe")]
    WatchUnsubscribe {
        id: String,
    },
}

impl WsRequest {
    pub(crate) fn id(&self) -> &str {
        match self {
            Self::Auth { id, .. }
            | Self::Stat { id, .. }
            | Self::Readdir { id, .. }
            | Self::ReaddirRecursive { id, .. }
            | Self::Mkdir { id, .. }
            | Self::Unlink { id, .. }
            | Self::Rm { id, .. }
            | Self::Read { id, .. }
            | Self::Write { id, .. }
            | Self::Pwrite { id, .. }
            | Self::Append { id, .. }
            | Self::Truncate { id, .. }
            | Self::Rename { id, .. }
            | Self::CreateUpload { id, .. }
            | Self::PresignPart { id, .. }
            | Self::PresignParts { id, .. }
            | Self::CompleteUpload { id, .. }
            | Self::AbortUpload { id, .. }
            | Self::PrepareDownload { id, .. }
            | Self::Symlink { id, .. }
            | Self::Readlink { id, .. }
            | Self::Chmod { id, .. }
            | Self::BatchStat { id, .. }
            | Self::BatchInlineRead { id, .. }
            | Self::BatchWrite { id, .. }
            | Self::BatchWriteAtomic { id, .. }
            | Self::WatchSubscribe { id, .. }
            | Self::WatchUnsubscribe { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WsResponse {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WsErrorDetail>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WsErrorDetail {
    pub code: WsErrorCode,
    pub message: String,
}

impl WsResponse {
    pub(crate) fn success(id: &str, data: Value) -> Self {
        Self {
            id: id.to_string(),
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    pub(crate) fn error(id: &str, code: WsErrorCode, message: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            ok: false,
            data: None,
            error: Some(WsErrorDetail {
                code,
                message: message.into(),
            }),
        }
    }

    pub(crate) fn success_empty(id: &str) -> Self {
        Self::success(id, json!({}))
    }
}

// ---------------------------------------------------------------------------
// Server-push messages (watch protocol)
// ---------------------------------------------------------------------------

/// Server-initiated push message. Distinguished from `WsResponse` by the
/// presence of a `push` field instead of `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WsPushMessage {
    /// Push type: "watch_event", "watch_gap", "watch_heartbeat".
    pub push: String,
    pub subscription_id: String,
    #[serde(flatten)]
    pub data: Value,
}

/// Response data for a successful `watch_subscribe` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WatchSubscribeResponse {
    pub subscription_id: String,
    pub head_seq: u64,
    pub overflow: bool,
}

/// Data payload for a `watch_event` push.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WatchEventData {
    pub seq: u64,
    pub timestamp: i64,
    pub event_type: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub inode: u64,
    pub generation: u64,
    pub is_dir: bool,
    pub size: u64,
}

/// Data payload for a `watch_gap` push.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WatchGapData {
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_available_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileInfoResponse {
    pub path: String,
    #[serde(rename = "type")]
    pub file_type: String,
    pub size: u64,
    pub mode: u32,
    pub generation: u64,
    pub mtime: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<FsStorage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sealed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReaddirRecursiveResponse {
    pub entries: Vec<FileInfoResponse>,
    pub truncated: bool,
    pub total_dirs_scanned: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReaddirResponse {
    pub entries: Vec<FileInfoResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir_version: Option<u64>,
}

fn format_mtime_rfc3339(epoch_seconds: u64) -> String {
    i64::try_from(epoch_seconds)
        .ok()
        .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0))
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

impl From<FsFileInfo> for FileInfoResponse {
    fn from(value: FsFileInfo) -> Self {
        Self {
            path: value.path,
            file_type: if value.is_symlink {
                "symlink".to_string()
            } else if value.is_dir {
                "dir".to_string()
            } else {
                "file".to_string()
            },
            size: value.size,
            mode: value.mode,
            generation: value.generation,
            mtime: format_mtime_rfc3339(value.mtime),
            storage: value.storage,
            sealed: value.sealed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StreamStartResponse {
    pub streaming: bool,
    pub stream_id: u64,
    pub size: u64,
    pub chunk_size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StreamWriteReady {
    pub ready: bool,
    pub stream_id: u64,
    pub chunk_size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StreamEnd {
    pub stream: String,
    pub stream_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PresignPartEntry {
    pub part_number: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum_crc32c: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MultipartCompletedPartRequest {
    pub part_number: i32,
    pub etag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum_crc32c: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BatchWriteFileRequest {
    pub path: String,
    pub content: String,
    #[serde(default = "default_encoding")]
    pub encoding: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PresignedRequestResponse {
    pub method: String,
    pub url: String,
    pub headers: Vec<HeaderPairResponse>,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HeaderPairResponse {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CreateUploadResponse {
    pub upload_token: String,
    pub upload_id: String,
    pub part_size: usize,
    pub expires_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum_algorithm: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PrepareDownloadResponse {
    #[serde(flatten)]
    pub request: PresignedRequestResponse,
    pub size: u64,
    pub storage: FsStorage,
    pub range_supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BatchStatEntryResponse {
    pub path: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<FileInfoResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WsErrorDetail>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BatchWriteEntryResponse {
    pub path: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub written: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WsErrorDetail>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BatchInlineReadEntryResponse {
    pub path: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WsErrorDetail>,
}

pub(crate) fn map_fs_error(err: &Error) -> (WsErrorCode, String) {
    for cause in err.chain() {
        if let Some(fs_err) = cause.downcast_ref::<EmbeddedFsError>() {
            return match fs_err {
                EmbeddedFsError::NotFound(path) => (
                    WsErrorCode::Enoent,
                    format!("No such file or directory: {path}"),
                ),
                EmbeddedFsError::AlreadyExists(path) => (
                    WsErrorCode::Eexist,
                    format!("File or directory already exists: {path}"),
                ),
                EmbeddedFsError::IsDirectory(path) => {
                    (WsErrorCode::Eisdir, format!("Is a directory: {path}"))
                }
                EmbeddedFsError::NotDirectory(path) => {
                    (WsErrorCode::Enotdir, format!("Not a directory: {path}"))
                }
                EmbeddedFsError::DirectoryNotEmpty(path) => (
                    WsErrorCode::Enotempty,
                    format!("Directory not empty: {path}"),
                ),
                EmbeddedFsError::TooLarge(msg) => (WsErrorCode::Efbig, msg.clone()),
                EmbeddedFsError::PermissionDenied(msg) => {
                    (WsErrorCode::Eacces, format!("Permission denied: {msg}"))
                }
                EmbeddedFsError::Conflict(msg) => (
                    WsErrorCode::Eagain,
                    format!("Resource temporarily unavailable: {msg}"),
                ),
                EmbeddedFsError::RestartRequired(msg) => {
                    (WsErrorCode::Eio, format!("Restart required: {msg}"))
                }
                EmbeddedFsError::InvalidInput(msg) => {
                    (WsErrorCode::Einval, format!("Invalid argument: {msg}"))
                }
                EmbeddedFsError::Internal(msg) => (WsErrorCode::Eio, format!("I/O error: {msg}")),
            };
        }
    }

    let text = err.to_string();
    if text.contains("too large") {
        return (WsErrorCode::Efbig, text);
    }
    if text.contains("budget") {
        return (WsErrorCode::Eagain, text);
    }

    (WsErrorCode::Eio, text)
}

pub(crate) fn validate_path(path: &str) -> Result<(), (WsErrorCode, String)> {
    if path.is_empty() {
        return Err((WsErrorCode::Einval, "Path must not be empty".to_string()));
    }
    if !path.starts_with('/') {
        return Err((
            WsErrorCode::Einval,
            format!("Path must be absolute: {path}"),
        ));
    }
    if path.split('/').any(|segment| segment == "..") {
        return Err((
            WsErrorCode::Einval,
            format!("Path traversal is not allowed: {path}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;

    #[test]
    fn test_request_deserialize_auth() {
        let payload = r#"{"id":"1","op":"auth","username":"myapp.admin","password":"secret"}"#;
        let req: WsRequest = serde_json::from_str(payload).expect("auth request should parse");
        match req {
            WsRequest::Auth {
                id,
                username,
                password,
                database,
            } => {
                assert_eq!(id, "1");
                assert_eq!(username, "myapp.admin");
                assert_eq!(password, "secret");
                assert_eq!(database, None);
            }
            _ => panic!("expected auth request"),
        }
    }

    #[test]
    fn test_request_deserialize_auth_with_database() {
        let payload = r#"{"id":"1","op":"auth","username":"myapp.admin","password":"secret","database":"appdb"}"#;
        let req: WsRequest = serde_json::from_str(payload).expect("auth request should parse");
        match req {
            WsRequest::Auth {
                id,
                username,
                password,
                database,
            } => {
                assert_eq!(id, "1");
                assert_eq!(username, "myapp.admin");
                assert_eq!(password, "secret");
                assert_eq!(database.as_deref(), Some("appdb"));
            }
            _ => panic!("expected auth request"),
        }
    }

    #[test]
    fn test_request_deserialize_stat() {
        let payload = r#"{"id":"2","op":"stat","path":"/data/file.csv"}"#;
        let req: WsRequest = serde_json::from_str(payload).expect("stat request should parse");
        match req {
            WsRequest::Stat { id, path } => {
                assert_eq!(id, "2");
                assert_eq!(path, "/data/file.csv");
            }
            _ => panic!("expected stat request"),
        }
    }

    #[test]
    fn test_request_deserialize_readdir_recursive() {
        let payload = r#"{
            "id":"2b",
            "op":"readdir_recursive",
            "path":"/data",
            "max_depth":12,
            "max_entries":4096
        }"#;
        let req: WsRequest =
            serde_json::from_str(payload).expect("readdir_recursive request should parse");
        match req {
            WsRequest::ReaddirRecursive {
                id,
                path,
                max_depth,
                max_entries,
            } => {
                assert_eq!(id, "2b");
                assert_eq!(path, "/data");
                assert_eq!(max_depth, Some(12));
                assert_eq!(max_entries, Some(4096));
            }
            _ => panic!("expected readdir_recursive request"),
        }
    }

    #[test]
    fn test_request_deserialize_read_with_offset() {
        let payload =
            r#"{"id":"7","op":"read","path":"/data/big.bin","offset":1024,"length":4096}"#;
        let req: WsRequest = serde_json::from_str(payload).expect("read request should parse");
        match req {
            WsRequest::Read {
                id,
                path,
                offset,
                length,
                streaming,
            } => {
                assert_eq!(id, "7");
                assert_eq!(path, "/data/big.bin");
                assert_eq!(offset, Some(1024));
                assert_eq!(length, Some(4096));
                assert!(!streaming);
            }
            _ => panic!("expected read request"),
        }
    }

    #[test]
    fn test_request_deserialize_write() {
        let payload = r#"{"id":"8","op":"write","path":"/data/file.csv","content":"aGVsbG8=","encoding":"base64"}"#;
        let req: WsRequest = serde_json::from_str(payload).expect("write request should parse");
        match req {
            WsRequest::Write {
                id,
                path,
                content,
                encoding,
                streaming,
                size,
                mode,
            } => {
                assert_eq!(id, "8");
                assert_eq!(path, "/data/file.csv");
                assert_eq!(content, Some("aGVsbG8=".to_string()));
                assert_eq!(encoding, "base64");
                assert!(!streaming);
                assert_eq!(size, None);
                assert_eq!(mode, None);
            }
            _ => panic!("expected write request"),
        }
    }

    #[test]
    fn test_request_deserialize_write_with_mode() {
        let payload = r#"{"id":"8b","op":"write","path":"/bin/run.sh","content":"IyEvYmluL3No","encoding":"base64","mode":493}"#;
        let req: WsRequest =
            serde_json::from_str(payload).expect("write+mode request should parse");
        match req {
            WsRequest::Write { id, path, mode, .. } => {
                assert_eq!(id, "8b");
                assert_eq!(path, "/bin/run.sh");
                assert_eq!(mode, Some(0o755));
            }
            _ => panic!("expected write request"),
        }
    }

    #[test]
    fn test_request_deserialize_mkdir_default_recursive() {
        let payload = r#"{"id":"4","op":"mkdir","path":"/data/subdir"}"#;
        let req: WsRequest = serde_json::from_str(payload).expect("mkdir request should parse");
        match req {
            WsRequest::Mkdir {
                id,
                path,
                recursive,
                mode,
            } => {
                assert_eq!(id, "4");
                assert_eq!(path, "/data/subdir");
                assert!(!recursive);
                assert_eq!(mode, None);
            }
            _ => panic!("expected mkdir request"),
        }
    }

    #[test]
    fn test_request_deserialize_mkdir_with_mode() {
        let payload =
            r#"{"id":"4b","op":"mkdir","path":"/data/subdir","recursive":true,"mode":493}"#;
        let req: WsRequest =
            serde_json::from_str(payload).expect("mkdir+mode request should parse");
        match req {
            WsRequest::Mkdir {
                id,
                path,
                recursive,
                mode,
            } => {
                assert_eq!(id, "4b");
                assert_eq!(path, "/data/subdir");
                assert!(recursive);
                assert_eq!(mode, Some(0o755));
            }
            _ => panic!("expected mkdir request"),
        }
    }

    #[test]
    fn test_response_serialize_success() {
        let resp = WsResponse::success("req-1", json!({"written": 11}));
        let value = serde_json::to_value(resp).expect("response should serialize");
        assert_eq!(value["id"], Value::String("req-1".to_string()));
        assert_eq!(value["ok"], Value::Bool(true));
        assert_eq!(value["data"]["written"], Value::from(11));
    }

    #[test]
    fn test_response_serialize_error() {
        let resp = WsResponse::error("req-2", WsErrorCode::Enoent, "missing");
        let value = serde_json::to_value(resp).expect("error response should serialize");
        assert_eq!(value["id"], Value::String("req-2".to_string()));
        assert_eq!(value["ok"], Value::Bool(false));
        assert_eq!(value["error"]["code"], Value::String("ENOENT".to_string()));
        assert_eq!(
            value["error"]["message"],
            Value::String("missing".to_string())
        );
    }

    #[test]
    fn test_error_code_serialize() {
        let code =
            serde_json::to_string(&WsErrorCode::Eproto).expect("error code should serialize");
        assert_eq!(code, "\"EPROTO\"");
    }

    #[test]
    fn test_validate_path_valid() {
        assert!(validate_path("/data/file.txt").is_ok());
        assert!(validate_path("/").is_ok());
    }

    #[test]
    fn test_validate_path_traversal() {
        let result = validate_path("/data/../etc/passwd");
        assert!(result.is_err());
        let (code, _) = result.expect_err("path traversal must fail");
        assert_eq!(code, WsErrorCode::Einval);
    }

    #[test]
    fn test_validate_path_not_absolute() {
        let result = validate_path("data/file.txt");
        assert!(result.is_err());
        let (code, _) = result.expect_err("relative path must fail");
        assert_eq!(code, WsErrorCode::Einval);
    }

    #[test]
    fn test_file_info_from_fs_file_info() {
        let src = FsFileInfo {
            path: "/data/test.txt".to_string(),
            is_dir: false,
            is_symlink: false,
            size: 123,
            mode: 0o100644,
            generation: 1,
            mtime: 0,
            storage: Some(FsStorage::Object),
            sealed: Some(true),
        };

        let dst = FileInfoResponse::from(src);
        assert_eq!(dst.path, "/data/test.txt");
        assert_eq!(dst.file_type, "file");
        assert_eq!(dst.size, 123);
        assert_eq!(dst.mode, 0o100644);
        assert_eq!(dst.generation, 1);
        assert_eq!(dst.mtime, "1970-01-01T00:00:00Z");
        assert_eq!(dst.storage, Some(FsStorage::Object));
        assert_eq!(dst.sealed, Some(true));
    }

    #[test]
    fn test_request_deserialize_rename() {
        let payload =
            r#"{"id":"10","op":"rename","old_path":"/data/a.csv","new_path":"/data/b.csv"}"#;
        let req: WsRequest = serde_json::from_str(payload).expect("rename request should parse");
        match req {
            WsRequest::Rename {
                id,
                old_path,
                new_path,
            } => {
                assert_eq!(id, "10");
                assert_eq!(old_path, "/data/a.csv");
                assert_eq!(new_path, "/data/b.csv");
            }
            _ => panic!("expected rename request"),
        }
    }

    #[test]
    fn test_request_deserialize_rename_cross_directory() {
        let payload =
            r#"{"id":"11","op":"rename","old_path":"/src/file.txt","new_path":"/dst/file.txt"}"#;
        let req: WsRequest = serde_json::from_str(payload).expect("rename request should parse");
        match req {
            WsRequest::Rename {
                id,
                old_path,
                new_path,
            } => {
                assert_eq!(id, "11");
                assert_eq!(old_path, "/src/file.txt");
                assert_eq!(new_path, "/dst/file.txt");
            }
            _ => panic!("expected rename request"),
        }
    }

    #[test]
    fn test_request_deserialize_create_upload() {
        let payload =
            r#"{"id":"12","op":"create_upload","path":"/data/large.bin","size":10485760}"#;
        let req: WsRequest =
            serde_json::from_str(payload).expect("create_upload request should parse");
        match req {
            WsRequest::CreateUpload {
                id,
                path,
                size,
                mode,
                checksum_algorithm,
            } => {
                assert_eq!(id, "12");
                assert_eq!(path, "/data/large.bin");
                assert_eq!(size, 10 * 1024 * 1024);
                assert_eq!(mode, None);
                assert_eq!(checksum_algorithm, None);
            }
            _ => panic!("expected create_upload request"),
        }
    }

    #[test]
    fn test_request_deserialize_complete_upload_with_checksum() {
        let payload = r#"{
            "id":"13",
            "op":"complete_upload",
            "upload_token":"token-1",
            "parts":[
                {"part_number":2,"etag":"etag-2"},
                {"part_number":1,"etag":"etag-1"}
            ],
            "checksum":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        }"#;
        let req: WsRequest =
            serde_json::from_str(payload).expect("complete_upload request should parse");
        match req {
            WsRequest::CompleteUpload {
                id,
                upload_token,
                parts,
                checksum,
            } => {
                assert_eq!(id, "13");
                assert_eq!(upload_token, "token-1");
                assert_eq!(parts.len(), 2);
                assert_eq!(parts[0].part_number, 2);
                assert_eq!(parts[0].etag, "etag-2");
                assert_eq!(
                    checksum.as_deref(),
                    Some("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                );
            }
            _ => panic!("expected complete_upload request"),
        }
    }

    #[test]
    fn test_request_deserialize_create_upload_with_checksum_algorithm() {
        let payload = r#"{"id":"12","op":"create_upload","path":"/data/large.bin","size":10485760,"checksum_algorithm":"crc32c"}"#;
        let req: WsRequest = serde_json::from_str(payload)
            .expect("create_upload with checksum_algorithm should parse");
        match req {
            WsRequest::CreateUpload {
                id,
                path,
                size,
                mode,
                checksum_algorithm,
            } => {
                assert_eq!(id, "12");
                assert_eq!(path, "/data/large.bin");
                assert_eq!(size, 10 * 1024 * 1024);
                assert_eq!(mode, None);
                assert_eq!(checksum_algorithm.as_deref(), Some("crc32c"));
            }
            _ => panic!("expected create_upload request"),
        }
    }

    #[test]
    fn test_request_deserialize_presign_part_with_checksum() {
        let payload = r#"{"id":"15","op":"presign_part","upload_token":"tok-1","part_number":3,"checksum_crc32c":"aabbccdd"}"#;
        let req: WsRequest =
            serde_json::from_str(payload).expect("presign_part with checksum should parse");
        match req {
            WsRequest::PresignPart {
                id,
                upload_token,
                part_number,
                checksum_crc32c,
            } => {
                assert_eq!(id, "15");
                assert_eq!(upload_token, "tok-1");
                assert_eq!(part_number, 3);
                assert_eq!(checksum_crc32c.as_deref(), Some("aabbccdd"));
            }
            _ => panic!("expected presign_part request"),
        }
    }

    #[test]
    fn test_request_deserialize_presign_parts_batch() {
        let payload = r#"{
            "id":"17",
            "op":"presign_parts",
            "upload_token":"tok-batch",
            "parts":[
                {"part_number":1},
                {"part_number":2,"checksum_crc32c":"aabb"},
                {"part_number":3}
            ]
        }"#;
        let req: WsRequest = serde_json::from_str(payload).expect("presign_parts should parse");
        match req {
            WsRequest::PresignParts {
                id,
                upload_token,
                parts,
            } => {
                assert_eq!(id, "17");
                assert_eq!(upload_token, "tok-batch");
                assert_eq!(parts.len(), 3);
                assert_eq!(parts[0].part_number, 1);
                assert_eq!(parts[0].checksum_crc32c, None);
                assert_eq!(parts[1].part_number, 2);
                assert_eq!(parts[1].checksum_crc32c.as_deref(), Some("aabb"));
                assert_eq!(parts[2].part_number, 3);
            }
            _ => panic!("expected presign_parts request"),
        }
    }

    #[test]
    fn test_request_deserialize_complete_upload_with_part_checksums() {
        let payload = r#"{
            "id":"16",
            "op":"complete_upload",
            "upload_token":"tok-2",
            "parts":[
                {"part_number":1,"etag":"etag-1","checksum_crc32c":"AAAA"},
                {"part_number":2,"etag":"etag-2","checksum_crc32c":"BBBB"}
            ]
        }"#;
        let req: WsRequest = serde_json::from_str(payload)
            .expect("complete_upload with part checksums should parse");
        match req {
            WsRequest::CompleteUpload {
                id,
                upload_token,
                parts,
                checksum,
            } => {
                assert_eq!(id, "16");
                assert_eq!(upload_token, "tok-2");
                assert_eq!(parts.len(), 2);
                assert_eq!(parts[0].checksum_crc32c.as_deref(), Some("AAAA"));
                assert_eq!(parts[1].checksum_crc32c.as_deref(), Some("BBBB"));
                assert_eq!(checksum, None);
            }
            _ => panic!("expected complete_upload request"),
        }
    }

    #[test]
    fn test_request_deserialize_prepare_download() {
        let payload = r#"{"id":"14","op":"prepare_download","path":"/data/object.bin"}"#;
        let req: WsRequest =
            serde_json::from_str(payload).expect("prepare_download request should parse");
        match req {
            WsRequest::PrepareDownload { id, path } => {
                assert_eq!(id, "14");
                assert_eq!(path, "/data/object.bin");
            }
            _ => panic!("expected prepare_download request"),
        }
    }

    #[test]
    fn test_request_deserialize_batch_write() {
        let payload = r#"{
            "id":"15",
            "op":"batch_write",
            "files":[
                {"path":"/data/a.txt","content":"YQ=="},
                {"path":"/data/b.txt","content":"Yg==","encoding":"base64"}
            ]
        }"#;
        let req: WsRequest =
            serde_json::from_str(payload).expect("batch_write request should parse");
        match req {
            WsRequest::BatchWrite { id, files } => {
                assert_eq!(id, "15");
                assert_eq!(files.len(), 2);
                assert_eq!(files[0].path, "/data/a.txt");
                assert_eq!(files[0].encoding, "base64");
                assert_eq!(files[1].path, "/data/b.txt");
                assert_eq!(files[1].encoding, "base64");
            }
            _ => panic!("expected batch_write request"),
        }
    }

    #[test]
    fn test_request_deserialize_batch_inline_read() {
        let payload = r#"{
            "id":"16",
            "op":"batch_inline_read",
            "paths":["/data/a.txt","/data/b.txt"]
        }"#;
        let req: WsRequest =
            serde_json::from_str(payload).expect("batch_inline_read request should parse");
        match req {
            WsRequest::BatchInlineRead { id, paths } => {
                assert_eq!(id, "16");
                assert_eq!(paths, vec!["/data/a.txt", "/data/b.txt"]);
            }
            _ => panic!("expected batch_inline_read request"),
        }
    }

    #[test]
    fn test_map_fs_error_invalid_input() {
        let err = anyhow::anyhow!(EmbeddedFsError::InvalidInput(
            "cannot rename /a into its own subdirectory /a/b".to_string()
        ));
        let (code, msg) = map_fs_error(&err);
        assert_eq!(code, WsErrorCode::Einval);
        assert!(msg.contains("Invalid argument"));
        assert!(msg.contains("cannot rename /a into its own subdirectory /a/b"));
    }

    #[test]
    fn test_map_fs_error_not_found() {
        let err = anyhow::anyhow!(EmbeddedFsError::NotFound("/missing".to_string()));
        let (code, _msg) = map_fs_error(&err);
        assert_eq!(code, WsErrorCode::Enoent);
    }

    #[test]
    fn test_map_fs_error_permission_denied() {
        let err = anyhow::anyhow!(EmbeddedFsError::PermissionDenied(
            "cannot rename root".to_string()
        ));
        let (code, _msg) = map_fs_error(&err);
        assert_eq!(code, WsErrorCode::Eacces);
    }

    #[test]
    fn test_map_fs_error_too_large() {
        let err = anyhow::anyhow!(EmbeddedFsError::too_large(
            "batch_inline_read raw payload exceeds limit 1024 bytes",
        ));
        let (code, msg) = map_fs_error(&err);
        assert_eq!(code, WsErrorCode::Efbig);
        assert_eq!(
            msg,
            "batch_inline_read raw payload exceeds limit 1024 bytes"
        );
    }

    #[test]
    fn test_validate_path_rename_root_rejected() {
        // Root path is valid syntactically; rename-root rejection is at the backend level.
        // validate_path itself accepts "/" — the rename handler rejects root renaming.
        assert!(validate_path("/").is_ok());
    }

    #[test]
    fn test_validate_path_empty_rejected() {
        let result = validate_path("");
        assert!(result.is_err());
        let (code, _) = result.expect_err("empty path must fail");
        assert_eq!(code, WsErrorCode::Einval);
    }

    #[test]
    fn test_request_deserialize_symlink() {
        let payload = r#"{"id":"20","op":"symlink","path":"/data/link","target":"/data/real.txt"}"#;
        let req: WsRequest = serde_json::from_str(payload).expect("symlink request should parse");
        match req {
            WsRequest::Symlink { id, path, target } => {
                assert_eq!(id, "20");
                assert_eq!(path, "/data/link");
                assert_eq!(target, "/data/real.txt");
            }
            _ => panic!("expected symlink request"),
        }
    }

    #[test]
    fn test_request_deserialize_readlink() {
        let payload = r#"{"id":"21","op":"readlink","path":"/data/link"}"#;
        let req: WsRequest = serde_json::from_str(payload).expect("readlink request should parse");
        match req {
            WsRequest::Readlink { id, path } => {
                assert_eq!(id, "21");
                assert_eq!(path, "/data/link");
            }
            _ => panic!("expected readlink request"),
        }
    }

    #[test]
    fn test_request_deserialize_chmod() {
        let payload = r#"{"id":"22","op":"chmod","path":"/data/script.sh","mode":493}"#;
        let req: WsRequest = serde_json::from_str(payload).expect("chmod request should parse");
        match req {
            WsRequest::Chmod { id, path, mode } => {
                assert_eq!(id, "22");
                assert_eq!(path, "/data/script.sh");
                assert_eq!(mode, 0o755);
            }
            _ => panic!("expected chmod request"),
        }
    }

    #[test]
    fn test_file_info_from_symlink() {
        let src = FsFileInfo {
            path: "/data/link".to_string(),
            is_dir: false,
            is_symlink: true,
            size: 15,
            mode: 0o777,
            generation: 1,
            mtime: 0,
            storage: None,
            sealed: Some(false),
        };

        let dst = FileInfoResponse::from(src);
        assert_eq!(dst.file_type, "symlink");
        assert_eq!(dst.size, 15);
        assert_eq!(dst.generation, 1);
    }

    #[test]
    fn test_request_deserialize_watch_subscribe() {
        let payload = r#"{"id":"w1","op":"watch_subscribe","path":"/data","since_seq":42}"#;
        let req: WsRequest =
            serde_json::from_str(payload).expect("watch_subscribe request should parse");
        match req {
            WsRequest::WatchSubscribe {
                id,
                path,
                since_seq,
            } => {
                assert_eq!(id, "w1");
                assert_eq!(path, "/data");
                assert_eq!(since_seq, Some(42));
            }
            _ => panic!("expected watch_subscribe request"),
        }
    }

    #[test]
    fn test_request_deserialize_watch_subscribe_no_since() {
        let payload = r#"{"id":"w2","op":"watch_subscribe","path":"/"}"#;
        let req: WsRequest =
            serde_json::from_str(payload).expect("watch_subscribe request should parse");
        match req {
            WsRequest::WatchSubscribe {
                id,
                path,
                since_seq,
            } => {
                assert_eq!(id, "w2");
                assert_eq!(path, "/");
                assert_eq!(since_seq, None);
            }
            _ => panic!("expected watch_subscribe request"),
        }
    }

    #[test]
    fn test_request_deserialize_watch_unsubscribe() {
        let payload = r#"{"id":"w3","op":"watch_unsubscribe"}"#;
        let req: WsRequest =
            serde_json::from_str(payload).expect("watch_unsubscribe request should parse");
        match req {
            WsRequest::WatchUnsubscribe { id } => {
                assert_eq!(id, "w3");
            }
            _ => panic!("expected watch_unsubscribe request"),
        }
    }

    #[test]
    fn test_watch_subscribe_response_serialization() {
        let resp = WatchSubscribeResponse {
            subscription_id: "sub-abc".to_string(),
            head_seq: 42,
            overflow: false,
        };
        let json: Value = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["subscription_id"], "sub-abc");
        assert_eq!(json["head_seq"], 42);
        assert_eq!(json["overflow"], false);
    }

    #[test]
    fn test_push_message_serialization() {
        let push = WsPushMessage {
            push: "watch_event".to_string(),
            subscription_id: "sub-abc".to_string(),
            data: json!({"seq": 43, "event_type": "CREATE", "path": "/foo.txt"}),
        };
        let json: Value = serde_json::to_value(&push).unwrap();
        assert_eq!(json["push"], "watch_event");
        assert_eq!(json["subscription_id"], "sub-abc");
        assert_eq!(json["seq"], 43);
        assert_eq!(json["event_type"], "CREATE");
        assert_eq!(json["path"], "/foo.txt");
    }

    #[test]
    fn test_watch_gap_data_serialization() {
        let gap = WatchGapData {
            reason: "overflow".to_string(),
            oldest_available_seq: Some(100),
        };
        let json: Value = serde_json::to_value(&gap).unwrap();
        assert_eq!(json["reason"], "overflow");
        assert_eq!(json["oldest_available_seq"], 100);
    }

    #[test]
    fn test_push_message_has_no_id_field() {
        let push = WsPushMessage {
            push: "watch_heartbeat".to_string(),
            subscription_id: "sub-abc".to_string(),
            data: json!({"seq": 42}),
        };
        let json: Value = serde_json::to_value(&push).unwrap();
        assert!(
            json.get("id").is_none(),
            "push messages must not have an id field"
        );
        assert!(
            json.get("push").is_some(),
            "push messages must have a push field"
        );
    }
}
