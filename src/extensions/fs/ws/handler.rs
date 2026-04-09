use anyhow::Error;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::json;
use std::collections::HashSet;
use tokio::time::{timeout, Duration};

use crate::extensions::fs::backend::{
    FsBatchWriteFile, FsMultipartCompletedPart, FsReaddirResult, FsRecursiveReaddirOptions,
    FsRecursiveReaddirResult,
};
use crate::extensions::fs::config::fs9_config;
use crate::extensions::fs::embedded::types::EmbeddedFsError;
use crate::extensions::fs::ws::auth::{FsAccessMode, WsSession};
use crate::extensions::fs::ws::protocol::{
    map_fs_error, validate_path, BatchInlineReadEntryResponse, BatchStatEntryResponse,
    BatchWriteEntryResponse, CreateUploadResponse, FileInfoResponse, HeaderPairResponse,
    MultipartCompletedPartRequest, PrepareDownloadResponse, PresignPartEntry,
    PresignedRequestResponse, ReaddirRecursiveResponse, ReaddirResponse, WsErrorCode,
    WsErrorDetail, WsRequest, WsResponse, MAX_JSON_FRAME_BYTES,
};
use crate::extensions::fs::MAX_BYTES_PER_FILE;

/// Returns `true` if the request is a write operation that mutates the filesystem.
fn is_write_operation(request: &WsRequest) -> bool {
    matches!(
        request,
        WsRequest::Write { .. }
            | WsRequest::Pwrite { .. }
            | WsRequest::Append { .. }
            | WsRequest::Truncate { .. }
            | WsRequest::Mkdir { .. }
            | WsRequest::Unlink { .. }
            | WsRequest::Rm { .. }
            | WsRequest::Rename { .. }
            | WsRequest::Symlink { .. }
            | WsRequest::Chmod { .. }
            | WsRequest::CreateUpload { .. }
            | WsRequest::PresignPart { .. }
            | WsRequest::PresignParts { .. }
            | WsRequest::CompleteUpload { .. }
            | WsRequest::AbortUpload { .. }
            | WsRequest::BatchWrite { .. }
            | WsRequest::BatchWriteAtomic { .. }
    )
}

pub(crate) async fn handle_request(session: &WsSession, request: &WsRequest) -> WsResponse {
    if session.access_mode == FsAccessMode::ReadOnly && is_write_operation(request) {
        return WsResponse::error(
            request.id(),
            WsErrorCode::Eacces,
            "fs9: read-only session — write operations are not permitted",
        );
    }

    match request {
        WsRequest::Auth { id, .. } => {
            WsResponse::error(id, WsErrorCode::Eproto, "already authenticated")
        }
        WsRequest::Stat { id, path } => handle_stat(session, id, path).await,
        WsRequest::Readdir { id, path } => handle_readdir(session, id, path).await,
        WsRequest::ReaddirRecursive {
            id,
            path,
            max_depth,
            max_entries,
        } => handle_readdir_recursive(session, id, path, *max_depth, *max_entries).await,
        WsRequest::Mkdir {
            id,
            path,
            recursive,
            mode,
        } => handle_mkdir(session, id, path, *recursive, *mode).await,
        WsRequest::Unlink { id, path } => handle_unlink(session, id, path).await,
        WsRequest::Rm {
            id,
            path,
            recursive,
        } => handle_rm(session, id, path, *recursive).await,
        WsRequest::Read {
            id,
            path,
            offset,
            length,
            streaming,
        } => {
            if *streaming {
                return WsResponse::error(
                    id,
                    WsErrorCode::Eproto,
                    "streaming must be handled at connection level",
                );
            }
            handle_read(session, id, path, *offset, *length).await
        }
        WsRequest::Write {
            id,
            path,
            content,
            encoding,
            streaming,
            mode,
            ..
        } => {
            if *streaming {
                return WsResponse::error(
                    id,
                    WsErrorCode::Eproto,
                    "streaming must be handled at connection level",
                );
            }
            handle_write(session, id, path, content.as_deref(), encoding, *mode).await
        }
        WsRequest::Pwrite {
            id,
            path,
            offset,
            content,
            encoding,
        } => handle_pwrite(session, id, path, *offset, content, encoding).await,
        WsRequest::Append {
            id,
            path,
            content,
            encoding,
        } => handle_append(session, id, path, content, encoding).await,
        WsRequest::Truncate { id, path, size } => handle_truncate(session, id, path, *size).await,
        WsRequest::Rename {
            id,
            old_path,
            new_path,
        } => handle_rename(session, id, old_path, new_path).await,
        WsRequest::CreateUpload {
            id,
            path,
            size,
            mode,
            checksum_algorithm,
        } => {
            handle_create_upload(
                session,
                id,
                path,
                *size,
                *mode,
                checksum_algorithm.as_deref(),
            )
            .await
        }
        WsRequest::PresignPart {
            id,
            upload_token,
            part_number,
            checksum_crc32c,
        } => {
            handle_presign_part(
                session,
                id,
                upload_token,
                *part_number,
                checksum_crc32c.as_deref(),
            )
            .await
        }
        WsRequest::PresignParts {
            id,
            upload_token,
            parts,
        } => handle_presign_parts(session, id, upload_token, parts).await,
        WsRequest::CompleteUpload {
            id,
            upload_token,
            parts,
            checksum,
        } => handle_complete_upload(session, id, upload_token, parts, checksum.as_deref()).await,
        WsRequest::AbortUpload { id, upload_token } => {
            handle_abort_upload(session, id, upload_token).await
        }
        WsRequest::PrepareDownload { id, path } => handle_prepare_download(session, id, path).await,
        WsRequest::Symlink { id, path, target } => handle_symlink(session, id, path, target).await,
        WsRequest::Readlink { id, path } => handle_readlink(session, id, path).await,
        WsRequest::Chmod { id, path, mode } => handle_chmod(session, id, path, *mode).await,
        WsRequest::BatchStat { id, paths } => handle_batch_stat(session, id, paths).await,
        WsRequest::BatchInlineRead { id, paths } => {
            handle_batch_inline_read(session, id, paths).await
        }
        WsRequest::BatchWrite { id, files } => handle_batch_write(session, id, files).await,
        WsRequest::BatchWriteAtomic { id, files } => {
            handle_batch_write_atomic(session, id, files).await
        }
        WsRequest::WatchSubscribe { id, .. } | WsRequest::WatchUnsubscribe { id, .. } => {
            WsResponse::error(
                id,
                WsErrorCode::Eproto,
                "watch operations must be handled at connection level",
            )
        }
    }
}

async fn handle_stat(session: &WsSession, id: &str, path: &str) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }

    let result = session.backend.stat(path).await;
    match result {
        Ok(info) => {
            let response = FileInfoResponse::from(info);
            match serde_json::to_value(response) {
                Ok(value) => WsResponse::success(id, value),
                Err(err) => WsResponse::error(
                    id,
                    WsErrorCode::Eio,
                    format!("failed to serialize stat response: {err}"),
                ),
            }
        }
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_readdir(session: &WsSession, id: &str, path: &str) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }

    let result = session.backend.readdir_with_meta(path).await;
    match result {
        Ok(result) => readdir_response(id, result),
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

fn readdir_response(id: &str, result: FsReaddirResult) -> WsResponse {
    let payload = match serde_json::to_value(ReaddirResponse {
        entries: result
            .entries
            .into_iter()
            .map(FileInfoResponse::from)
            .collect(),
        dir_version: result.dir_version,
    }) {
        Ok(payload) => payload,
        Err(err) => {
            return WsResponse::error(
                id,
                WsErrorCode::Eio,
                format!("failed to serialize readdir response: {err}"),
            );
        }
    };
    WsResponse::success(id, payload)
}

async fn handle_readdir_recursive(
    session: &WsSession,
    id: &str,
    path: &str,
    max_depth: Option<usize>,
    max_entries: Option<usize>,
) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }

    if max_entries == Some(0) {
        return WsResponse::error(
            id,
            WsErrorCode::Einval,
            "max_entries must be greater than 0",
        );
    }

    let config = fs9_config();
    let opts = FsRecursiveReaddirOptions {
        max_depth: max_depth
            .unwrap_or(config.readdir_recursive_max_depth)
            .min(config.readdir_recursive_max_depth),
        max_entries: max_entries
            .unwrap_or(config.readdir_recursive_max_entries)
            .min(config.readdir_recursive_max_entries),
        exclude_set: None,
    };

    let result = match timeout(
        Duration::from_secs(config.readdir_recursive_timeout_secs),
        session.backend.readdir_recursive(path, opts),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            return WsResponse::error(
                id,
                WsErrorCode::Eagain,
                format!(
                    "readdir_recursive timed out after {} seconds",
                    config.readdir_recursive_timeout_secs
                ),
            )
        }
    };

    match result {
        Ok(result) => recursive_readdir_response(
            id,
            result,
            config
                .readdir_recursive_max_response_bytes
                .min(MAX_JSON_FRAME_BYTES),
        ),
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_mkdir(
    session: &WsSession,
    id: &str,
    path: &str,
    recursive: bool,
    mode: Option<u32>,
) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }

    let result = session
        .backend
        .mkdir(path, recursive, mode.map(|m| m & 0o7777))
        .await;
    match result {
        Ok(()) => WsResponse::success_empty(id),
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_symlink(session: &WsSession, id: &str, path: &str, target: &str) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }
    if target.is_empty() {
        return WsResponse::error(id, WsErrorCode::Einval, "symlink target must not be empty");
    }
    if target.contains('\0') {
        return WsResponse::error(
            id,
            WsErrorCode::Einval,
            "symlink target must not contain NUL bytes",
        );
    }

    let result = session.backend.symlink(path, target).await;
    match result {
        Ok(()) => WsResponse::success_empty(id),
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_readlink(session: &WsSession, id: &str, path: &str) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }

    let result = session.backend.readlink(path).await;
    match result {
        Ok(target) => WsResponse::success(id, json!({ "target": target })),
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_chmod(session: &WsSession, id: &str, path: &str, mode: u32) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }

    let result = session.backend.chmod(path, mode).await;
    match result {
        Ok(()) => WsResponse::success_empty(id),
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_unlink(session: &WsSession, id: &str, path: &str) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }

    let result = session.backend.remove(path).await;
    match result {
        Ok(()) => WsResponse::success_empty(id),
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_rm(session: &WsSession, id: &str, path: &str, recursive: bool) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }

    if recursive {
        let result = session.backend.remove_recursive(path).await;
        return match result {
            Ok(removed) => WsResponse::success(id, json!({ "removed": removed })),
            Err(err) => {
                let (code, msg) = map_fs_error(&err);
                WsResponse::error(id, code, msg)
            }
        };
    }

    let result = session.backend.remove(path).await;
    match result {
        Ok(()) => WsResponse::success_empty(id),
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_read(
    session: &WsSession,
    id: &str,
    path: &str,
    offset: Option<u64>,
    length: Option<usize>,
) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }

    let result = match (offset, length) {
        (Some(off), Some(len)) => session.backend.read_file_at(path, off, len).await,
        (None, None) => session.backend.read_file(path, MAX_BYTES_PER_FILE).await,
        _ => {
            return WsResponse::error(
                id,
                WsErrorCode::Einval,
                "offset and length must be provided together",
            );
        }
    };

    match result {
        Ok(data) => {
            let encoded = STANDARD.encode(&data);
            WsResponse::success(
                id,
                json!({
                    "content": encoded,
                    "size": data.len(),
                    "encoding": "base64"
                }),
            )
        }
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_write(
    session: &WsSession,
    id: &str,
    path: &str,
    content: Option<&str>,
    encoding: &str,
    mode: Option<u32>,
) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }

    let Some(content) = content else {
        return WsResponse::error(id, WsErrorCode::Einval, "missing content");
    };

    let data = match decode_base64_content(id, content, encoding) {
        Ok(data) => data,
        Err(resp) => return resp,
    };

    if data.len() > MAX_BYTES_PER_FILE {
        return WsResponse::error(
            id,
            WsErrorCode::Efbig,
            format!(
                "file too large: {} bytes exceeds limit {}",
                data.len(),
                MAX_BYTES_PER_FILE
            ),
        );
    }

    let result = session
        .backend
        .write_file(path, &data, mode.map(|m| m & 0o7777))
        .await;
    match result {
        Ok(written) => WsResponse::success(id, json!({ "written": written })),
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_pwrite(
    session: &WsSession,
    id: &str,
    path: &str,
    offset: u64,
    content: &str,
    encoding: &str,
) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }

    let data = match decode_base64_content(id, content, encoding) {
        Ok(data) => data,
        Err(resp) => return resp,
    };

    let result = session.backend.write_file_at(path, offset, &data).await;
    match result {
        Ok(written) => WsResponse::success(id, json!({ "written": written })),
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_append(
    session: &WsSession,
    id: &str,
    path: &str,
    content: &str,
    encoding: &str,
) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }

    let data = match decode_base64_content(id, content, encoding) {
        Ok(data) => data,
        Err(resp) => return resp,
    };

    let result = session.backend.append_file(path, &data).await;
    match result {
        Ok(written) => WsResponse::success(id, json!({ "written": written })),
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_truncate(session: &WsSession, id: &str, path: &str, size: u64) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }

    let result = session.backend.truncate(path, size).await;
    match result {
        Ok(()) => WsResponse::success_empty(id),
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_rename(
    session: &WsSession,
    id: &str,
    old_path: &str,
    new_path: &str,
) -> WsResponse {
    if let Err((code, msg)) = validate_path(old_path) {
        return WsResponse::error(id, code, msg);
    }
    if let Err((code, msg)) = validate_path(new_path) {
        return WsResponse::error(id, code, msg);
    }

    let result = session.backend.rename(old_path, new_path).await;
    match result {
        Ok(()) => WsResponse::success_empty(id),
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_create_upload(
    session: &WsSession,
    id: &str,
    path: &str,
    size: u64,
    mode: Option<u32>,
    checksum_algorithm: Option<&str>,
) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }

    let permit = match session.try_acquire_upload_slot() {
        Some(permit) => permit,
        None => {
            return WsResponse::error(
                id,
                WsErrorCode::Eagain,
                format!(
                    "fs9: too many in-flight uploads for this websocket connection (limit: {})",
                    fs9_config().ws_max_inflight_uploads_per_connection
                ),
            );
        }
    };

    match session
        .backend
        .create_upload(path, size, mode.map(|m| m & 0o7777), checksum_algorithm)
        .await
    {
        Ok(upload) => {
            let upload_token = upload.upload_token.clone();
            let payload = match serde_json::to_value(CreateUploadResponse {
                upload_token: upload_token.clone(),
                upload_id: upload.upload_id,
                part_size: upload.part_size,
                expires_at: format_mtime(upload.expires_at),
                checksum_algorithm: upload.checksum_algorithm,
            }) {
                Ok(value) => value,
                Err(err) => {
                    return WsResponse::error(
                        id,
                        WsErrorCode::Eio,
                        format!("failed to serialize create_upload response: {err}"),
                    );
                }
            };

            session.register_inflight_upload(upload_token, permit).await;
            WsResponse::success(id, payload)
        }
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_presign_part(
    session: &WsSession,
    id: &str,
    upload_token: &str,
    part_number: i32,
    checksum_crc32c: Option<&str>,
) -> WsResponse {
    match session
        .backend
        .presign_upload_part(upload_token, part_number, checksum_crc32c)
        .await
    {
        Ok(request) => WsResponse::success(
            id,
            serde_json::to_value(to_presigned_response(request)).unwrap_or_else(|_| json!({})),
        ),
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_presign_parts(
    session: &WsSession,
    id: &str,
    upload_token: &str,
    parts: &[PresignPartEntry],
) -> WsResponse {
    let max_parts = fs9_config().batch_presign_max_parts;
    if parts.len() > max_parts {
        return WsResponse::error(
            id,
            WsErrorCode::Efbig,
            format!(
                "presign_parts supports at most {} parts per request",
                max_parts
            ),
        );
    }
    let mut results = Vec::with_capacity(parts.len());
    for entry in parts {
        match session
            .backend
            .presign_upload_part(
                upload_token,
                entry.part_number,
                entry.checksum_crc32c.as_deref(),
            )
            .await
        {
            Ok(request) => {
                results.push(json!({
                    "part_number": entry.part_number,
                    "request": to_presigned_response(request),
                }));
            }
            Err(err) => {
                let (code, msg) = map_fs_error(&err);
                return WsResponse::error(id, code, msg);
            }
        }
    }
    WsResponse::success(id, json!({ "presigned": results }))
}

async fn handle_complete_upload(
    session: &WsSession,
    id: &str,
    upload_token: &str,
    parts: &[MultipartCompletedPartRequest],
    checksum: Option<&str>,
) -> WsResponse {
    let checksum = match checksum {
        Some(raw) => match parse_sha256_checksum(id, raw) {
            Ok(value) => Some(value),
            Err(resp) => return resp,
        },
        None => None,
    };

    let parts: Vec<FsMultipartCompletedPart> = parts
        .iter()
        .map(|part| FsMultipartCompletedPart {
            part_number: part.part_number,
            etag: part.etag.clone(),
            checksum_crc32c: part.checksum_crc32c.clone(),
        })
        .collect();

    match session
        .backend
        .complete_upload(upload_token, parts, checksum)
        .await
    {
        Ok(written) => {
            session.release_inflight_upload(upload_token).await;
            WsResponse::success(id, json!({ "written": written }))
        }
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_abort_upload(session: &WsSession, id: &str, upload_token: &str) -> WsResponse {
    match session.backend.abort_upload(upload_token).await {
        Ok(()) => {
            session.release_inflight_upload(upload_token).await;
            WsResponse::success_empty(id)
        }
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_prepare_download(session: &WsSession, id: &str, path: &str) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }

    match session.backend.prepare_download(path).await {
        Ok(download) => WsResponse::success(
            id,
            serde_json::to_value(PrepareDownloadResponse {
                request: to_presigned_response(download.request),
                size: download.size,
                storage: download.storage,
                range_supported: download.range_supported,
            })
            .unwrap_or_else(|_| json!({})),
        ),
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

async fn handle_batch_stat(session: &WsSession, id: &str, paths: &[String]) -> WsResponse {
    let max_files = fs9_config().batch_stat_max_files;
    if paths.len() > max_files {
        return WsResponse::error(
            id,
            WsErrorCode::Efbig,
            format!(
                "batch_stat supports at most {} paths per request",
                max_files
            ),
        );
    }
    for path in paths {
        if let Err((code, msg)) = validate_path(path) {
            return WsResponse::error(id, code, msg);
        }
    }

    batch_stat_response(id, paths, session.backend.batch_stat(paths).await)
}

fn batch_stat_response(
    id: &str,
    paths: &[String],
    result: anyhow::Result<Vec<anyhow::Result<crate::extensions::fs::backend::FsFileInfo>>>,
) -> WsResponse {
    let entries: Vec<BatchStatEntryResponse> = match result {
        Ok(results) => {
            if results.len() != paths.len() {
                let code = WsErrorCode::Eio;
                let message = format!(
                    "batch_stat backend returned {} results for {} input paths",
                    results.len(),
                    paths.len()
                );
                paths
                    .iter()
                    .cloned()
                    .map(|path| BatchStatEntryResponse {
                        path,
                        ok: false,
                        info: None,
                        error: Some(WsErrorDetail {
                            code,
                            message: message.clone(),
                        }),
                    })
                    .collect()
            } else {
                paths
                    .iter()
                    .cloned()
                    .zip(results)
                    .map(|(path, result)| match result {
                        Ok(info) => BatchStatEntryResponse {
                            path,
                            ok: true,
                            info: Some(FileInfoResponse::from(info)),
                            error: None,
                        },
                        Err(err) => {
                            let (code, msg) = map_fs_error(&err);
                            BatchStatEntryResponse {
                                path,
                                ok: false,
                                info: None,
                                error: Some(WsErrorDetail { code, message: msg }),
                            }
                        }
                    })
                    .collect()
            }
        }
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            paths
                .iter()
                .cloned()
                .map(|path| BatchStatEntryResponse {
                    path,
                    ok: false,
                    info: None,
                    error: Some(WsErrorDetail {
                        code,
                        message: msg.clone(),
                    }),
                })
                .collect()
        }
    };

    WsResponse::success(id, json!({ "entries": entries }))
}

async fn handle_batch_inline_read(session: &WsSession, id: &str, paths: &[String]) -> WsResponse {
    let max_files = fs9_config().batch_inline_read_max_files;
    if paths.len() > max_files {
        return WsResponse::error(
            id,
            WsErrorCode::Efbig,
            format!(
                "batch_inline_read supports at most {} paths per request",
                max_files
            ),
        );
    }

    for path in paths {
        if let Err((code, msg)) = validate_path(path) {
            return WsResponse::error(id, code, msg);
        }
    }

    let mut seen_paths = HashSet::with_capacity(paths.len());
    for path in paths {
        if !seen_paths.insert(path.clone()) {
            return WsResponse::error(
                id,
                WsErrorCode::Einval,
                format!("batch_inline_read requires unique paths: {path}"),
            );
        }
    }

    let max_file_bytes = fs9_config().batch_inline_read_max_file_bytes;
    let max_total_bytes = fs9_config().batch_inline_read_max_total_bytes;
    match session
        .backend
        .batch_inline_read(paths, max_file_bytes, max_total_bytes)
        .await
    {
        Ok(results) => batch_inline_read_response(id, paths, results),
        Err(err) => {
            let (code, msg) = map_batch_inline_read_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
}

fn map_batch_inline_read_error(err: &Error) -> (WsErrorCode, String) {
    map_fs_error(err)
}

fn map_batch_inline_read_entry_error(err: &Error) -> (WsErrorCode, String) {
    for cause in err.chain() {
        if let Some(EmbeddedFsError::IsDirectory(_)) = cause.downcast_ref::<EmbeddedFsError>() {
            return (WsErrorCode::Eisdir, "Is a directory".to_string());
        }
    }

    map_batch_inline_read_error(err)
}

fn batch_inline_read_response(
    id: &str,
    paths: &[String],
    results: Vec<anyhow::Result<Vec<u8>>>,
) -> WsResponse {
    if results.len() != paths.len() {
        return WsResponse::error(
            id,
            WsErrorCode::Eio,
            format!(
                "batch_inline_read backend returned {} results for {} input paths",
                results.len(),
                paths.len()
            ),
        );
    }

    let entries = paths
        .iter()
        .cloned()
        .zip(results)
        .map(|(path, result)| match result {
            Ok(data) => BatchInlineReadEntryResponse {
                path,
                ok: true,
                content: Some(STANDARD.encode(&data)),
                size: Some(data.len()),
                encoding: Some("base64".to_string()),
                error: None,
            },
            Err(err) => {
                let (code, msg) = map_batch_inline_read_entry_error(&err);
                BatchInlineReadEntryResponse {
                    path,
                    ok: false,
                    content: None,
                    size: None,
                    encoding: None,
                    error: Some(WsErrorDetail { code, message: msg }),
                }
            }
        })
        .collect::<Vec<_>>();

    WsResponse::success(id, json!({ "entries": entries }))
}

fn recursive_readdir_response(
    id: &str,
    result: FsRecursiveReaddirResult,
    max_response_bytes: usize,
) -> WsResponse {
    let payload = match serde_json::to_value(ReaddirRecursiveResponse {
        entries: result
            .entries
            .into_iter()
            .map(FileInfoResponse::from)
            .collect(),
        truncated: result.truncated,
        total_dirs_scanned: result.total_dirs_scanned,
    }) {
        Ok(value) => value,
        Err(err) => {
            return WsResponse::error(
                id,
                WsErrorCode::Eio,
                format!("failed to serialize recursive readdir payload: {err}"),
            )
        }
    };

    let response = WsResponse::success(id, payload);
    match serde_json::to_vec(&response) {
        Ok(bytes) if bytes.len() <= max_response_bytes => response,
        Ok(bytes) => WsResponse::error(
            id,
            WsErrorCode::Efbig,
            format!(
                "readdir_recursive response exceeds limit {} bytes (actual {})",
                max_response_bytes,
                bytes.len()
            ),
        ),
        Err(err) => WsResponse::error(
            id,
            WsErrorCode::Eio,
            format!("failed to serialize recursive readdir response: {err}"),
        ),
    }
}

/// Shared validation + decoding for batch_write and batch_write_atomic.
/// Returns decoded files ready for backend, or an early error response.
fn validate_and_decode_batch_write(
    id: &str,
    op_name: &str,
    files: &[crate::extensions::fs::ws::protocol::BatchWriteFileRequest],
) -> Result<Vec<FsBatchWriteFile>, WsResponse> {
    let config = fs9_config();
    let max_files = config.batch_write_max_files;
    if files.len() > max_files {
        return Err(WsResponse::error(
            id,
            WsErrorCode::Efbig,
            format!("{op_name} supports at most {max_files} files per request"),
        ));
    }

    let inline_max = config.inline_max_bytes;
    let max_total_raw = config.batch_write_max_total_bytes;
    let max_total_encoded = config.batch_write_max_encoded_bytes;
    let mut decoded_files = Vec::with_capacity(files.len());
    let mut total_raw = 0usize;
    let mut total_encoded = 0usize;
    let mut seen_paths = HashSet::with_capacity(files.len());

    for file in files {
        if let Err((code, msg)) = validate_path(&file.path) {
            return Err(WsResponse::error(id, code, msg));
        }
        if !seen_paths.insert(file.path.clone()) {
            return Err(WsResponse::error(
                id,
                WsErrorCode::Einval,
                format!("{op_name} requires unique paths: {}", file.path),
            ));
        }
        total_encoded = total_encoded.saturating_add(file.content.len());
        if total_encoded > max_total_encoded {
            return Err(WsResponse::error(
                id,
                WsErrorCode::Efbig,
                format!("{op_name} encoded payload exceeds limit {max_total_encoded} bytes"),
            ));
        }

        let data = decode_base64_content(id, &file.content, &file.encoding)?;
        if data.len() > inline_max {
            return Err(WsResponse::error(
                id,
                WsErrorCode::Efbig,
                format!(
                    "{op_name} file {} exceeds inline limit {inline_max} bytes",
                    file.path
                ),
            ));
        }
        total_raw = total_raw.saturating_add(data.len());
        if total_raw > max_total_raw {
            return Err(WsResponse::error(
                id,
                WsErrorCode::Efbig,
                format!("{op_name} raw payload exceeds limit {max_total_raw} bytes"),
            ));
        }
        decoded_files.push(FsBatchWriteFile {
            path: file.path.clone(),
            data,
            mode: file.mode.map(|m| m & 0o7777),
        });
    }

    Ok(decoded_files)
}

async fn handle_batch_write(
    session: &WsSession,
    id: &str,
    files: &[crate::extensions::fs::ws::protocol::BatchWriteFileRequest],
) -> WsResponse {
    let batch_files = match validate_and_decode_batch_write(id, "batch_write", files) {
        Ok(files) => files,
        Err(resp) => return resp,
    };
    let results = match session.backend.batch_write(batch_files).await {
        Ok(results) => results,
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            return WsResponse::success(
                id,
                json!({
                    "entries": files.iter().map(|file| BatchWriteEntryResponse {
                        path: file.path.clone(),
                        ok: false,
                        written: None,
                        error: Some(WsErrorDetail {
                            code,
                            message: msg.clone(),
                        }),
                    }).collect::<Vec<_>>()
                }),
            );
        }
    };

    let mut entries = Vec::with_capacity(results.len());
    for entry in results {
        match entry.result {
            Ok(written) => entries.push(BatchWriteEntryResponse {
                path: entry.path,
                ok: true,
                written: Some(written),
                error: None,
            }),
            Err(err) => {
                let (code, msg) = map_fs_error(&err);
                entries.push(BatchWriteEntryResponse {
                    path: entry.path,
                    ok: false,
                    written: None,
                    error: Some(WsErrorDetail { code, message: msg }),
                });
            }
        }
    }

    WsResponse::success(id, json!({ "entries": entries }))
}

/// Capability-gated fast-path for bulk small-file uploads.
///
/// Files are grouped by parent directory and each subgroup (bounded by
/// `grouped_write_subgroup_size`) is committed atomically in a single TiKV
/// transaction. Per-subgroup atomic semantics: all files within a subgroup
/// succeed or fail together. Cross-subgroup partial success is possible.
///
/// Response includes a `summary` object for observability:
///   strategy_used, subgroup_count (actual txn count from backend),
///   entries_committed, entries_failed, fallback_reason_counts
async fn handle_batch_write_atomic(
    session: &WsSession,
    id: &str,
    files: &[crate::extensions::fs::ws::protocol::BatchWriteFileRequest],
) -> WsResponse {
    // ── Capability gate: reject if backend doesn't support atomic writes ─
    if !session.backend.supports_batch_write_atomic() {
        return WsResponse::error(
            id,
            WsErrorCode::Enosys,
            "batch_write_atomic is not supported by this backend",
        );
    }

    // ── Validation (shared with batch_write) ────────────────────────────
    let batch_files = match validate_and_decode_batch_write(id, "batch_write_atomic", files) {
        Ok(files) => files,
        Err(resp) => return resp,
    };

    // ── Execute grouped write ────────────────────────────────────────────
    let grouped_result = match session.backend.batch_write_grouped(batch_files).await {
        Ok(result) => result,
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            return WsResponse::success(
                id,
                json!({
                    "entries": files.iter().map(|file| BatchWriteEntryResponse {
                        path: file.path.clone(),
                        ok: false,
                        written: None,
                        error: Some(WsErrorDetail {
                            code,
                            message: msg.clone(),
                        }),
                    }).collect::<Vec<_>>(),
                    "summary": {
                        "strategy_used": "per_subgroup_atomic",
                        "subgroup_count": 0,
                        "entries_committed": 0,
                        "entries_failed": files.len(),
                        "retry_count": 0,
                        "retries_exhausted": 0,
                        "fallback_reason_counts": {}
                    }
                }),
            );
        }
    };
    let subgroup_count = grouped_result.actual_subgroup_count;
    let total_retries = grouped_result.total_retries;
    let retries_exhausted = grouped_result.retries_exhausted;

    // ── Build response ───────────────────────────────────────────────────
    let mut entries = Vec::with_capacity(grouped_result.entries.len());
    let mut committed = 0usize;
    let mut failed = 0usize;
    let mut category_counts: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for entry in grouped_result.entries {
        match entry.result {
            Ok(written) => {
                committed += 1;
                entries.push(BatchWriteEntryResponse {
                    path: entry.path,
                    ok: true,
                    written: Some(written),
                    error: None,
                });
            }
            Err(err) => {
                failed += 1;
                if let Some(category) = entry.failure_category {
                    *category_counts.entry(category).or_insert(0) += 1;
                }
                let (code, msg) = map_fs_error(&err);
                entries.push(BatchWriteEntryResponse {
                    path: entry.path,
                    ok: false,
                    written: None,
                    error: Some(WsErrorDetail { code, message: msg }),
                });
            }
        }
    }

    let mut fallback_reasons = serde_json::Map::new();
    for (category, count) in &category_counts {
        fallback_reasons.insert(
            category.to_string(),
            serde_json::Value::Number((*count).into()),
        );
    }

    WsResponse::success(
        id,
        json!({
            "entries": entries,
            "summary": {
                "strategy_used": "per_subgroup_atomic",
                "subgroup_count": subgroup_count,
                "entries_committed": committed,
                "entries_failed": failed,
                "retry_count": total_retries,
                "retries_exhausted": retries_exhausted,
                "fallback_reason_counts": fallback_reasons
            }
        }),
    )
}

fn decode_base64_content(id: &str, content: &str, encoding: &str) -> Result<Vec<u8>, WsResponse> {
    if encoding != "base64" {
        return Err(WsResponse::error(
            id,
            WsErrorCode::Einval,
            format!("unsupported encoding: {encoding}"),
        ));
    }
    STANDARD
        .decode(content)
        .map_err(|e| WsResponse::error(id, WsErrorCode::Einval, format!("invalid base64: {e}")))
}

fn parse_sha256_checksum(id: &str, raw: &str) -> Result<[u8; 32], WsResponse> {
    let Some(hex_part) = raw.strip_prefix("sha256:") else {
        return Err(WsResponse::error(
            id,
            WsErrorCode::Einval,
            "checksum must use sha256:<hex> format",
        ));
    };
    let bytes = hex::decode(hex_part).map_err(|e| {
        WsResponse::error(
            id,
            WsErrorCode::Einval,
            format!("invalid checksum hex: {e}"),
        )
    })?;
    bytes
        .try_into()
        .map_err(|_| WsResponse::error(id, WsErrorCode::Einval, "sha256 checksum must be 32 bytes"))
}

fn to_presigned_response(
    request: crate::extensions::fs::backend::FsPresignedRequest,
) -> PresignedRequestResponse {
    PresignedRequestResponse {
        method: request.method,
        url: request.url,
        headers: request
            .headers
            .into_iter()
            .map(|(name, value)| HeaderPairResponse { name, value })
            .collect(),
        expires_at: format_mtime(request.expires_at),
    }
}

fn format_mtime(epoch_seconds: i64) -> String {
    if epoch_seconds <= 0 {
        return "1970-01-01T00:00:00Z".to_string();
    }
    chrono::DateTime::<chrono::Utc>::from_timestamp(epoch_seconds, 0)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        batch_inline_read_response, batch_stat_response, decode_base64_content,
        map_batch_inline_read_error, parse_sha256_checksum, readdir_response,
        recursive_readdir_response,
    };
    use crate::extensions::fs::backend::{FsFileInfo, FsReaddirResult, FsRecursiveReaddirResult};
    use crate::extensions::fs::ws::protocol::WsErrorCode;
    use anyhow::anyhow;
    use serde_json::Value;

    #[test]
    fn test_decode_base64_valid() {
        let data = decode_base64_content("req-1", "aGVsbG8=", "base64")
            .expect("valid base64 should decode successfully");
        assert_eq!(data, b"hello");
    }

    #[test]
    fn test_decode_base64_invalid() {
        let err = decode_base64_content("req-2", "***", "base64")
            .expect_err("invalid base64 should return error");
        let detail = err.error.expect("error detail should be present");
        assert_eq!(detail.code, WsErrorCode::Einval);
        assert!(detail.message.contains("invalid base64"));
    }

    #[test]
    fn test_decode_base64_wrong_encoding() {
        let err = decode_base64_content("req-3", "aGVsbG8=", "utf8")
            .expect_err("unsupported encoding should return error");
        let detail = err.error.expect("error detail should be present");
        assert_eq!(detail.code, WsErrorCode::Einval);
        assert_eq!(detail.message, "unsupported encoding: utf8");
    }

    #[test]
    fn test_parse_sha256_checksum_valid() {
        let checksum = parse_sha256_checksum(
            "req-1",
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .expect("valid checksum should parse");
        assert_eq!(checksum[0], 0x01);
        assert_eq!(checksum[31], 0xef);
    }

    #[test]
    fn test_parse_sha256_checksum_requires_prefix() {
        let err = parse_sha256_checksum("req-2", "0123456789abcdef")
            .expect_err("checksum without prefix must fail");
        assert_eq!(err.id, "req-2");
        let detail = err.error.expect("error detail should be present");
        assert_eq!(detail.code, WsErrorCode::Einval);
        assert_eq!(detail.message, "checksum must use sha256:<hex> format");
    }

    #[test]
    fn test_parse_sha256_checksum_requires_32_bytes() {
        let err =
            parse_sha256_checksum("req-3", "sha256:abcd").expect_err("short checksum must fail");
        assert_eq!(err.id, "req-3");
        let detail = err.error.expect("error detail should be present");
        assert_eq!(detail.code, WsErrorCode::Einval);
        assert_eq!(detail.message, "sha256 checksum must be 32 bytes");
    }

    #[test]
    fn test_batch_stat_response_converts_backend_error_to_per_entry_failures() {
        let paths = vec!["/a".to_string(), "/b".to_string()];
        let resp = batch_stat_response("req-4", &paths, Err(anyhow!("backend exploded")));

        assert!(
            resp.ok,
            "batch_stat must keep top-level ok for backend read failures"
        );
        let data = resp.data.expect("batch_stat success must include data");
        let entries = data["entries"]
            .as_array()
            .expect("entries must be an array");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["path"], Value::String("/a".to_string()));
        assert_eq!(entries[0]["ok"], Value::Bool(false));
        assert_eq!(entries[1]["path"], Value::String("/b".to_string()));
        assert_eq!(entries[1]["ok"], Value::Bool(false));
    }

    #[test]
    fn test_batch_stat_response_preserves_mixed_entry_results() {
        let paths = vec!["/ok".to_string(), "/missing".to_string()];
        let resp = batch_stat_response(
            "req-5",
            &paths,
            Ok(vec![
                Ok(FsFileInfo {
                    path: "/ok".to_string(),
                    is_dir: false,
                    is_symlink: false,
                    size: 5,
                    mode: 0o644,
                    generation: 1,
                    mtime: 0,
                    storage: None,
                    sealed: None,
                }),
                Err(anyhow!(
                    crate::extensions::fs::embedded::types::EmbeddedFsError::not_found("/missing")
                )),
            ]),
        );

        assert!(resp.ok);
        let data = resp.data.expect("batch_stat success must include data");
        let entries = data["entries"]
            .as_array()
            .expect("entries must be an array");
        assert_eq!(entries[0]["ok"], Value::Bool(true));
        assert_eq!(entries[1]["ok"], Value::Bool(false));
        assert_eq!(
            entries[1]["error"]["code"],
            Value::String("ENOENT".to_string())
        );
    }

    #[test]
    fn test_batch_stat_response_rejects_short_backend_result_vectors() {
        let paths = vec!["/a".to_string(), "/b".to_string()];
        let resp = batch_stat_response(
            "req-6",
            &paths,
            Ok(vec![Ok(FsFileInfo {
                path: "/a".to_string(),
                is_dir: false,
                is_symlink: false,
                size: 1,
                mode: 0o644,
                generation: 1,
                mtime: 0,
                storage: None,
                sealed: None,
            })]),
        );

        assert!(resp.ok);
        let data = resp.data.expect("batch_stat success must include data");
        let entries = data["entries"]
            .as_array()
            .expect("entries must be an array");
        assert_eq!(entries.len(), 2);
        for entry in entries {
            assert_eq!(entry["ok"], Value::Bool(false));
            assert_eq!(entry["error"]["code"], Value::String("EIO".to_string()));
            assert!(entry["error"]["message"]
                .as_str()
                .expect("message must be a string")
                .contains("returned 1 results for 2 input paths"));
        }
    }

    #[test]
    fn test_batch_stat_response_rejects_long_backend_result_vectors() {
        let paths = vec!["/a".to_string()];
        let resp = batch_stat_response(
            "req-7",
            &paths,
            Ok(vec![
                Ok(FsFileInfo {
                    path: "/a".to_string(),
                    is_dir: false,
                    is_symlink: false,
                    size: 1,
                    mode: 0o644,
                    generation: 1,
                    mtime: 0,
                    storage: None,
                    sealed: None,
                }),
                Ok(FsFileInfo {
                    path: "/extra".to_string(),
                    is_dir: false,
                    is_symlink: false,
                    size: 1,
                    mode: 0o644,
                    generation: 1,
                    mtime: 0,
                    storage: None,
                    sealed: None,
                }),
            ]),
        );

        assert!(resp.ok);
        let data = resp.data.expect("batch_stat success must include data");
        let entries = data["entries"]
            .as_array()
            .expect("entries must be an array");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["ok"], Value::Bool(false));
        assert_eq!(
            entries[0]["error"]["code"],
            Value::String("EIO".to_string())
        );
        assert!(entries[0]["error"]["message"]
            .as_str()
            .expect("message must be a string")
            .contains("returned 2 results for 1 input paths"));
    }

    #[test]
    fn test_batch_inline_read_response_preserves_mixed_entry_results() {
        let paths = vec!["/ok".to_string(), "/missing".to_string()];
        let resp = batch_inline_read_response(
            "req-8",
            &paths,
            vec![
                Ok(b"hello".to_vec()),
                Err(anyhow!(
                    crate::extensions::fs::embedded::types::EmbeddedFsError::not_found("/missing")
                )),
            ],
        );

        assert!(resp.ok);
        let data = resp
            .data
            .expect("batch_inline_read success must include data");
        let entries = data["entries"]
            .as_array()
            .expect("entries must be an array");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["ok"], Value::Bool(true));
        assert_eq!(entries[0]["size"], Value::Number(5usize.into()));
        assert_eq!(entries[0]["encoding"], Value::String("base64".to_string()));
        assert_eq!(entries[1]["ok"], Value::Bool(false));
        assert_eq!(
            entries[1]["error"]["code"],
            Value::String("ENOENT".to_string())
        );
    }

    #[test]
    fn test_batch_inline_read_response_preserves_directory_wire_message() {
        let paths = vec!["/dir".to_string()];
        let resp = batch_inline_read_response(
            "req-8b",
            &paths,
            vec![Err(anyhow!(
                crate::extensions::fs::embedded::types::EmbeddedFsError::is_directory("/dir")
            ))],
        );

        assert!(resp.ok);
        let data = resp
            .data
            .expect("batch_inline_read success must include data");
        let entries = data["entries"]
            .as_array()
            .expect("entries must be an array");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["ok"], Value::Bool(false));
        assert_eq!(
            entries[0]["error"]["code"],
            Value::String("EISDIR".to_string())
        );
        assert_eq!(
            entries[0]["error"]["message"],
            Value::String("Is a directory".to_string())
        );
    }

    #[test]
    fn test_map_batch_inline_read_error_preserves_payload_contract() {
        let err = crate::extensions::fs::backend::batch_inline_read_payload_too_large_error(17, 16);
        let (code, msg) = map_batch_inline_read_error(&err);
        assert_eq!(code, WsErrorCode::Efbig);
        assert_eq!(msg, "batch_inline_read raw payload exceeds limit 16 bytes");
    }

    #[test]
    fn test_batch_inline_read_response_rejects_backend_result_length_mismatch() {
        let paths = vec!["/a".to_string(), "/b".to_string()];
        let resp = batch_inline_read_response("req-9", &paths, vec![Ok(vec![1u8])]);

        assert!(!resp.ok);
        assert!(resp.data.is_none());
        let detail = resp.error.expect("error detail should be present");
        assert_eq!(detail.code, WsErrorCode::Eio);
        assert!(detail
            .message
            .contains("backend returned 1 results for 2 input paths"));
    }

    #[test]
    fn test_readdir_response_serializes_dir_version() {
        let resp = readdir_response(
            "req-9a",
            FsReaddirResult {
                entries: vec![FsFileInfo {
                    path: "/root/a.txt".to_string(),
                    is_dir: false,
                    is_symlink: false,
                    size: 5,
                    mode: 0o644,
                    generation: 1,
                    mtime: 0,
                    storage: None,
                    sealed: None,
                }],
                dir_version: Some(42),
            },
        );

        assert!(resp.ok);
        let data = resp.data.expect("readdir success must include data");
        assert_eq!(data["dir_version"], Value::from(42u64));
        let entries = data["entries"]
            .as_array()
            .expect("entries must be an array");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["path"], Value::String("/root/a.txt".to_string()));
    }

    #[test]
    fn test_recursive_readdir_response_serializes_payload() {
        let resp = recursive_readdir_response(
            "req-10",
            FsRecursiveReaddirResult {
                entries: vec![
                    FsFileInfo {
                        path: "/root/a.txt".to_string(),
                        is_dir: false,
                        is_symlink: false,
                        size: 5,
                        mode: 0o644,
                        generation: 1,
                        mtime: 0,
                        storage: None,
                        sealed: None,
                    },
                    FsFileInfo {
                        path: "/root/sub".to_string(),
                        is_dir: true,
                        is_symlink: false,
                        size: 0,
                        mode: 0o755,
                        generation: 1,
                        mtime: 0,
                        storage: None,
                        sealed: Some(false),
                    },
                ],
                truncated: true,
                total_dirs_scanned: 3,
            },
            4096,
        );

        assert!(resp.ok);
        let data = resp
            .data
            .expect("recursive readdir success must include data");
        assert_eq!(data["truncated"], Value::Bool(true));
        assert_eq!(data["total_dirs_scanned"], Value::from(3usize));
        let entries = data["entries"]
            .as_array()
            .expect("entries must be an array");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["path"], Value::String("/root/a.txt".to_string()));
        assert_eq!(entries[1]["type"], Value::String("dir".to_string()));
    }

    #[test]
    fn test_recursive_readdir_response_rejects_oversized_payload() {
        let resp = recursive_readdir_response(
            "req-11",
            FsRecursiveReaddirResult {
                entries: vec![FsFileInfo {
                    path: "/root/very-long-file-name.txt".to_string(),
                    is_dir: false,
                    is_symlink: false,
                    size: 5,
                    mode: 0o644,
                    generation: 1,
                    mtime: 0,
                    storage: None,
                    sealed: None,
                }],
                truncated: false,
                total_dirs_scanned: 1,
            },
            64,
        );

        assert!(!resp.ok);
        let detail = resp.error.expect("error detail should be present");
        assert_eq!(detail.code, WsErrorCode::Efbig);
        assert!(detail
            .message
            .contains("readdir_recursive response exceeds limit"));
    }

    // ── batch_write_atomic contract tests ────────────────────────────────

    use super::validate_and_decode_batch_write;
    use crate::extensions::fs::ws::protocol::BatchWriteFileRequest;

    fn make_file_request(path: &str, content: &[u8]) -> BatchWriteFileRequest {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        BatchWriteFileRequest {
            path: path.to_string(),
            content: STANDARD.encode(content),
            encoding: "base64".to_string(),
            mode: None,
        }
    }

    #[test]
    fn validate_batch_write_accepts_valid_files() {
        let files = vec![
            make_file_request("/dir/a.txt", b"hello"),
            make_file_request("/dir/b.txt", b"world"),
        ];
        let result = validate_and_decode_batch_write("req-v1", "batch_write_atomic", &files);
        let decoded = result.expect("valid files must decode successfully");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].path, "/dir/a.txt");
        assert_eq!(decoded[0].data, b"hello");
        assert_eq!(decoded[1].path, "/dir/b.txt");
        assert_eq!(decoded[1].data, b"world");
    }

    #[test]
    fn validate_batch_write_rejects_duplicate_paths() {
        let files = vec![
            make_file_request("/dir/a.txt", b"first"),
            make_file_request("/dir/a.txt", b"second"),
        ];
        let err = validate_and_decode_batch_write("req-v2", "batch_write_atomic", &files)
            .expect_err("duplicate paths must be rejected");
        let detail = err.error.expect("error detail should be present");
        assert_eq!(detail.code, WsErrorCode::Einval);
        assert!(detail.message.contains("unique paths"));
    }

    #[test]
    fn validate_batch_write_rejects_bad_encoding() {
        let files = vec![BatchWriteFileRequest {
            path: "/dir/a.txt".to_string(),
            content: "not-base64".to_string(),
            encoding: "utf8".to_string(),
            mode: None,
        }];
        let err = validate_and_decode_batch_write("req-v3", "batch_write_atomic", &files)
            .expect_err("unsupported encoding must be rejected");
        let detail = err.error.expect("error detail should be present");
        assert_eq!(detail.code, WsErrorCode::Einval);
        assert!(detail.message.contains("unsupported encoding"));
    }

    #[test]
    fn validate_batch_write_rejects_invalid_path() {
        let files = vec![make_file_request("no-leading-slash", b"data")];
        let err = validate_and_decode_batch_write("req-v4", "batch_write_atomic", &files)
            .expect_err("path without leading slash must be rejected");
        assert!(err.error.is_some());
    }

    #[test]
    fn validate_batch_write_applies_mode_mask() {
        use base64::Engine;
        let files = vec![BatchWriteFileRequest {
            path: "/dir/a.txt".to_string(),
            content: base64::engine::general_purpose::STANDARD.encode(b"x"),
            encoding: "base64".to_string(),
            mode: Some(0o100644),
        }];
        let decoded = validate_and_decode_batch_write("req-v5", "batch_write_atomic", &files)
            .expect("valid file with mode must decode");
        assert_eq!(
            decoded[0].mode,
            Some(0o644),
            "mode must be masked to 0o7777"
        );
    }

    // ── Handler-level contract tests with mock backend ──────────────────

    mod handler_contract {
        use super::*;
        use crate::extensions::fs::backend::{
            FsBackend, FsBatchWriteEntry, FsBatchWriteFile, FsBatchWriteGroupedResult,
            FsCreateUpload, FsFileInfo, FsMultipartCompletedPart, FsPreparedDownload,
            FsPresignedRequest, FsWriteStream, FsWriteStreamOptions,
        };
        use crate::extensions::fs::ws::auth::WsSession;
        use crate::extensions::fs::ws::protocol::WsErrorCode;
        use anyhow::{anyhow, Result};
        use async_trait::async_trait;
        use std::sync::Arc;
        use tokio::io::AsyncBufRead;

        /// Minimal mock backend with configurable batch_write_atomic support
        /// and optional per-directory failure simulation.
        struct MockFsBackend {
            atomic_supported: bool,
            /// Parent dirs that should fail in batch_write_grouped.
            fail_dirs: std::collections::HashSet<String>,
            /// Optional readdir result for readdir_with_meta testing.
            readdir_result: Option<FsReaddirResult>,
        }

        #[async_trait]
        impl FsBackend for MockFsBackend {
            async fn stat(&self, _path: &str) -> Result<FsFileInfo> {
                Err(anyhow!("not implemented"))
            }
            async fn readdir(&self, _path: &str) -> Result<Vec<FsFileInfo>> {
                if let Some(ref result) = self.readdir_result {
                    return Ok(result.entries.clone());
                }
                Err(anyhow!("not implemented"))
            }
            async fn readdir_with_meta(&self, _path: &str) -> Result<FsReaddirResult> {
                if let Some(ref result) = self.readdir_result {
                    return Ok(result.clone());
                }
                Err(anyhow!("not implemented"))
            }
            async fn read_file(&self, _path: &str, _max_bytes: usize) -> Result<Vec<u8>> {
                Err(anyhow!("not implemented"))
            }
            async fn read_file_stream(
                &self,
                _path: &str,
                _max_bytes: usize,
            ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
                Err(anyhow!("not implemented"))
            }
            async fn remove(&self, _path: &str) -> Result<()> {
                Err(anyhow!("not implemented"))
            }
            async fn remove_recursive(&self, _path: &str) -> Result<u64> {
                Err(anyhow!("not implemented"))
            }
            async fn mkdir(&self, _path: &str, _recursive: bool, _mode: Option<u32>) -> Result<()> {
                Err(anyhow!("not implemented"))
            }
            async fn write_file(
                &self,
                _path: &str,
                data: &[u8],
                _mode: Option<u32>,
            ) -> Result<usize> {
                // Legacy batch_write calls write_file per entry via default impl.
                Ok(data.len())
            }
            fn supports_batch_write_atomic(&self) -> bool {
                self.atomic_supported
            }
            async fn batch_write_grouped(
                &self,
                files: Vec<FsBatchWriteFile>,
            ) -> Result<FsBatchWriteGroupedResult> {
                if !self.atomic_supported {
                    return Err(anyhow!(
                        "batch_write_grouped is not supported by this backend"
                    ));
                }
                // Replicate real backend grouping + chunking logic:
                // 1. Group files by parent directory
                // 2. Chunk each group by grouped_write_subgroup_size
                // 3. Count chunks as actual subgroups
                // Dirs in fail_dirs produce per-entry errors.
                use std::collections::HashMap;
                let max_subgroup =
                    crate::extensions::fs::config::fs9_config().grouped_write_subgroup_size;

                let mut groups: HashMap<String, Vec<&FsBatchWriteFile>> = HashMap::new();
                for file in &files {
                    let parent = file
                        .path
                        .rsplit_once('/')
                        .map(|(p, _)| if p.is_empty() { "/" } else { p })
                        .unwrap_or("/")
                        .to_string();
                    groups.entry(parent).or_default().push(file);
                }

                let mut entries = Vec::with_capacity(files.len());
                let mut actual_subgroup_count = 0usize;
                for (parent, group_files) in &groups {
                    for chunk in group_files.chunks(max_subgroup) {
                        actual_subgroup_count += 1;
                        let failed = self.fail_dirs.contains(parent.as_str());
                        for file in chunk {
                            let result = if failed {
                                Err(anyhow!("simulated subgroup failure for dir {}", parent))
                            } else {
                                Ok(file.data.len())
                            };
                            let failure_category = if failed {
                                Some("execution.txn_conflict")
                            } else {
                                None
                            };
                            entries.push(FsBatchWriteEntry {
                                path: file.path.clone(),
                                result,
                                failure_category,
                            });
                        }
                    }
                }
                Ok(FsBatchWriteGroupedResult {
                    entries,
                    actual_subgroup_count,
                    total_retries: 0,
                    retries_exhausted: 0,
                })
            }
            async fn begin_write_stream(
                &self,
                _path: &str,
                _opts: FsWriteStreamOptions,
            ) -> Result<Box<dyn FsWriteStream>> {
                Err(anyhow!("not implemented"))
            }
            async fn read_file_at(
                &self,
                _path: &str,
                _offset: u64,
                _length: usize,
            ) -> Result<Vec<u8>> {
                Err(anyhow!("not implemented"))
            }
            async fn write_file_at(
                &self,
                _path: &str,
                _offset: u64,
                _data: &[u8],
            ) -> Result<usize> {
                Err(anyhow!("not implemented"))
            }
            async fn append_file(&self, _path: &str, _data: &[u8]) -> Result<usize> {
                Err(anyhow!("not implemented"))
            }
            async fn truncate(&self, _path: &str, _size: u64) -> Result<()> {
                Err(anyhow!("not implemented"))
            }
            async fn rename(&self, _old_path: &str, _new_path: &str) -> Result<()> {
                Err(anyhow!("not implemented"))
            }
            async fn create_upload(
                &self,
                _path: &str,
                _expected_size: u64,
                _mode: Option<u32>,
                _checksum_algorithm: Option<&str>,
            ) -> Result<FsCreateUpload> {
                Err(anyhow!("not implemented"))
            }
            async fn presign_upload_part(
                &self,
                _upload_token: &str,
                _part_number: i32,
                _checksum_crc32c: Option<&str>,
            ) -> Result<FsPresignedRequest> {
                Err(anyhow!("not implemented"))
            }
            async fn complete_upload(
                &self,
                _upload_token: &str,
                _parts: Vec<FsMultipartCompletedPart>,
                _checksum: Option<[u8; 32]>,
            ) -> Result<usize> {
                Err(anyhow!("not implemented"))
            }
            async fn abort_upload(&self, _upload_token: &str) -> Result<()> {
                Err(anyhow!("not implemented"))
            }
            async fn prepare_download(&self, _path: &str) -> Result<FsPreparedDownload> {
                Err(anyhow!("not implemented"))
            }
            async fn symlink(&self, _path: &str, _target: &str) -> Result<()> {
                Err(anyhow!("not implemented"))
            }
            async fn readlink(&self, _path: &str) -> Result<String> {
                Err(anyhow!("not implemented"))
            }
            async fn chmod(&self, _path: &str, _mode: u32) -> Result<()> {
                Err(anyhow!("not implemented"))
            }
        }

        fn mock_session(atomic_supported: bool) -> WsSession {
            WsSession::new_for_test(Arc::new(MockFsBackend {
                atomic_supported,
                fail_dirs: std::collections::HashSet::new(),
                readdir_result: None,
            }))
        }

        fn mock_session_with_fail_dirs(fail_dirs: Vec<&str>) -> WsSession {
            WsSession::new_for_test(Arc::new(MockFsBackend {
                atomic_supported: true,
                fail_dirs: fail_dirs.into_iter().map(String::from).collect(),
                readdir_result: None,
            }))
        }

        fn mock_session_with_readdir(result: FsReaddirResult) -> WsSession {
            WsSession::new_for_test(Arc::new(MockFsBackend {
                atomic_supported: false,
                fail_dirs: std::collections::HashSet::new(),
                readdir_result: Some(result),
            }))
        }

        #[tokio::test]
        async fn handle_readdir_serializes_dir_version_from_backend_meta() {
            let session = mock_session_with_readdir(FsReaddirResult {
                entries: vec![FsFileInfo {
                    path: "/dir/a.txt".to_string(),
                    is_dir: false,
                    is_symlink: false,
                    size: 7,
                    mode: 0o644,
                    generation: 3,
                    mtime: 0,
                    storage: None,
                    sealed: None,
                }],
                dir_version: Some(77),
            });

            let resp = super::super::handle_readdir(&session, "req-rd", "/dir").await;

            assert!(resp.ok);
            let data = resp.data.expect("readdir success must include data");
            assert_eq!(data["dir_version"], Value::from(77u64));
            let entries = data["entries"]
                .as_array()
                .expect("entries must be an array");
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0]["path"], Value::String("/dir/a.txt".to_string()));
        }

        #[test]
        fn test_readdir_response_omits_dir_version_when_absent() {
            let resp = readdir_response(
                "req-none",
                FsReaddirResult {
                    entries: vec![FsFileInfo {
                        path: "/root/b.txt".to_string(),
                        is_dir: false,
                        is_symlink: false,
                        size: 10,
                        mode: 0o644,
                        generation: 1,
                        mtime: 0,
                        storage: None,
                        sealed: None,
                    }],
                    dir_version: None,
                },
            );

            assert!(resp.ok);
            let data = resp.data.expect("readdir success must include data");
            assert!(
                data.get("dir_version").is_none(),
                "dir_version should be omitted from JSON when None, not serialized as null"
            );
        }

        #[tokio::test]
        async fn batch_write_atomic_rejects_unsupported_backend_with_enosys() {
            let session = mock_session(false);
            let files = vec![make_file_request("/dir/a.txt", b"hello")];
            let resp = super::super::handle_batch_write_atomic(&session, "req-1", &files).await;

            assert!(!resp.ok, "unsupported backend must return error");
            let detail = resp.error.expect("error detail should be present");
            assert_eq!(detail.code, WsErrorCode::Enosys);
            assert!(detail.message.contains("not supported"));
        }

        #[tokio::test]
        async fn batch_write_atomic_returns_correct_summary_on_supported_backend() {
            let session = mock_session(true);
            let files = vec![
                make_file_request("/dir_a/x.txt", b"aaa"),
                make_file_request("/dir_a/y.txt", b"bbb"),
                make_file_request("/dir_b/z.txt", b"ccc"),
            ];
            let resp = super::super::handle_batch_write_atomic(&session, "req-2", &files).await;

            assert!(resp.ok, "supported backend must succeed");
            let data = resp.data.expect("success must include data");
            let summary = &data["summary"];

            assert_eq!(summary["strategy_used"], "per_subgroup_atomic");
            assert_eq!(summary["subgroup_count"], 2, "2 distinct parent dirs");
            assert_eq!(summary["entries_committed"], 3);
            assert_eq!(summary["entries_failed"], 0);

            let entries = data["entries"].as_array().expect("entries must be array");
            assert_eq!(entries.len(), 3);
            for entry in entries {
                assert_eq!(entry["ok"], true);
                assert!(entry["written"].as_u64().unwrap() > 0);
            }
        }

        #[tokio::test]
        async fn batch_write_atomic_single_dir_reports_one_subgroup() {
            let session = mock_session(true);
            let files = vec![
                make_file_request("/same/a.txt", b"1"),
                make_file_request("/same/b.txt", b"22"),
                make_file_request("/same/c.txt", b"333"),
            ];
            let resp = super::super::handle_batch_write_atomic(&session, "req-3", &files).await;

            assert!(resp.ok);
            let data = resp.data.expect("success must include data");
            assert_eq!(
                data["summary"]["subgroup_count"], 1,
                "single dir = 1 subgroup"
            );
            assert_eq!(data["summary"]["entries_committed"], 3);
        }

        #[tokio::test]
        async fn legacy_batch_write_has_no_summary_field() {
            let session = mock_session(false);
            let files = vec![
                make_file_request("/dir/a.txt", b"hello"),
                make_file_request("/dir/b.txt", b"world"),
            ];
            let resp = super::super::handle_batch_write(&session, "req-4", &files).await;

            assert!(
                resp.ok,
                "legacy batch_write must succeed via default write_file"
            );
            let data = resp.data.expect("success must include data");

            // Legacy batch_write must NOT have a summary field
            assert!(
                data.get("summary").is_none(),
                "legacy batch_write must not include summary"
            );

            let entries = data["entries"].as_array().expect("entries must be array");
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0]["ok"], true);
            assert_eq!(entries[0]["written"], 5);
            assert_eq!(entries[1]["ok"], true);
            assert_eq!(entries[1]["written"], 5);
        }

        /// Pins the auth response JSON shape by calling the same
        /// `build_auth_success_data()` helper used by the real WebSocket
        /// handler in ws/mod.rs. No logic duplication.
        #[test]
        fn auth_response_includes_capability_on_supported_backend() {
            let session = mock_session(true);
            let auth_data = session.build_auth_success_data();

            assert_eq!(auth_data["user"], "test_user");
            assert_eq!(auth_data["keyspace"], "db9_tenant_test");
            assert!(auth_data.get("tenant").is_some(), "tenant field must exist");

            let caps = auth_data["capabilities"]
                .as_array()
                .expect("capabilities must be an array");
            assert!(
                caps.contains(&serde_json::json!("watch")),
                "watch capability must be present"
            );
            assert!(
                caps.contains(&serde_json::json!("batch_write_atomic")),
                "batch_write_atomic capability must be present"
            );
        }

        #[test]
        fn auth_response_has_watch_capability_on_unsupported_backend() {
            let session = mock_session(false);
            let auth_data = session.build_auth_success_data();

            assert_eq!(auth_data["user"], "test_user");
            assert_eq!(auth_data["keyspace"], "db9_tenant_test");

            let caps = auth_data["capabilities"]
                .as_array()
                .expect("capabilities must be an array");
            assert_eq!(
                caps.len(),
                1,
                "unsupported backend should only have watch capability"
            );
            assert_eq!(caps[0], "watch");
        }

        /// Pins the partial-success boundary: one directory subgroup fails
        /// while another succeeds. The handler must report both in entries
        /// and reflect the split in summary counts.
        #[tokio::test]
        async fn batch_write_atomic_cross_dir_partial_success() {
            let session = mock_session_with_fail_dirs(vec!["/fail_dir"]);
            let files = vec![
                make_file_request("/ok_dir/a.txt", b"good"),
                make_file_request("/ok_dir/b.txt", b"also good"),
                make_file_request("/fail_dir/c.txt", b"will fail"),
                make_file_request("/fail_dir/d.txt", b"also fails"),
            ];
            let resp =
                super::super::handle_batch_write_atomic(&session, "req-partial", &files).await;

            assert!(resp.ok, "top-level response must be ok (partial success)");
            let data = resp.data.expect("success must include data");
            let summary = &data["summary"];

            assert_eq!(summary["strategy_used"], "per_subgroup_atomic");
            assert_eq!(summary["subgroup_count"], 2, "2 distinct parent dirs");
            assert_eq!(summary["entries_committed"], 2, "ok_dir files succeed");
            assert_eq!(summary["entries_failed"], 2, "fail_dir files fail");

            let entries = data["entries"].as_array().expect("entries must be array");
            assert_eq!(entries.len(), 4);

            // Check by path (HashMap iteration order is non-deterministic)
            let find = |path: &str| -> &serde_json::Value {
                entries
                    .iter()
                    .find(|e| e["path"] == path)
                    .unwrap_or_else(|| panic!("entry for {path} not found"))
            };

            // ok_dir entries succeed
            assert_eq!(find("/ok_dir/a.txt")["ok"], true);
            assert!(find("/ok_dir/a.txt")["written"].as_u64().unwrap() > 0);
            assert_eq!(find("/ok_dir/b.txt")["ok"], true);

            // fail_dir entries fail
            assert_eq!(find("/fail_dir/c.txt")["ok"], false);
            assert!(find("/fail_dir/c.txt")["error"].is_object());
            assert_eq!(find("/fail_dir/d.txt")["ok"], false);

            // fallback_reason_counts should categorize the execution-level failures
            let reasons = &summary["fallback_reason_counts"];
            assert_eq!(
                reasons["execution.txn_conflict"], 2,
                "2 failed entries should be categorized as txn_conflict"
            );
        }

        /// Exercises the subgroup chunking path directly at the backend level
        /// (bypassing handler's request-level file count limit). When a single
        /// directory has more files than grouped_write_subgroup_size, the mock
        /// backend (which replicates real chunking logic) splits into multiple
        /// subgroups. CI-covered, pins actual_subgroup_count against config.
        #[tokio::test]
        async fn mock_backend_subgroup_chunking_produces_correct_count() {
            let backend = Arc::new(MockFsBackend {
                atomic_supported: true,
                fail_dirs: std::collections::HashSet::new(),
                readdir_result: None,
            });
            let subgroup_size =
                crate::extensions::fs::config::fs9_config().grouped_write_subgroup_size;

            // Generate enough files in one dir to force 3 subgroups
            let file_count = subgroup_size * 2 + 1;
            let files: Vec<FsBatchWriteFile> = (0..file_count)
                .map(|i| FsBatchWriteFile {
                    path: format!("/chunked/f_{:04}.dat", i),
                    data: vec![0x42u8; 16],
                    mode: None,
                })
                .collect();

            let result = backend
                .batch_write_grouped(files)
                .await
                .expect("grouped write must succeed");

            let expected_subgroups = file_count.div_ceil(subgroup_size);
            assert_eq!(
                result.actual_subgroup_count, expected_subgroups,
                "ceil({file_count}/{subgroup_size}) = {expected_subgroups} subgroups"
            );
            assert_eq!(result.entries.len(), file_count);
            for entry in &result.entries {
                assert!(entry.result.is_ok(), "entry {} must succeed", entry.path);
            }
        }

        /// Same chunking test with multiple directories: 2 dirs, one exceeding
        /// subgroup_size. Verifies subgroup count = chunks(dir_a) + chunks(dir_b).
        #[tokio::test]
        async fn mock_backend_multi_dir_chunking() {
            let backend = Arc::new(MockFsBackend {
                atomic_supported: true,
                fail_dirs: std::collections::HashSet::new(),
                readdir_result: None,
            });
            let subgroup_size =
                crate::extensions::fs::config::fs9_config().grouped_write_subgroup_size;

            // dir_a: subgroup_size + 1 files -> 2 subgroups
            // dir_b: 2 files -> 1 subgroup
            let mut files = Vec::new();
            for i in 0..(subgroup_size + 1) {
                files.push(FsBatchWriteFile {
                    path: format!("/dir_a/f_{:04}.dat", i),
                    data: vec![0x41u8; 8],
                    mode: None,
                });
            }
            files.push(FsBatchWriteFile {
                path: "/dir_b/x.txt".to_string(),
                data: vec![0x42u8; 8],
                mode: None,
            });
            files.push(FsBatchWriteFile {
                path: "/dir_b/y.txt".to_string(),
                data: vec![0x42u8; 8],
                mode: None,
            });

            let result = backend
                .batch_write_grouped(files)
                .await
                .expect("grouped write must succeed");

            // dir_a: ceil((subgroup_size+1)/subgroup_size) = 2 subgroups
            // dir_b: ceil(2/subgroup_size) = 1 subgroup
            let expected = 2 + 1;
            assert_eq!(
                result.actual_subgroup_count, expected,
                "dir_a=2 + dir_b=1 = {expected} subgroups"
            );
            assert_eq!(result.entries.len(), subgroup_size + 1 + 2);
            for entry in &result.entries {
                assert!(entry.result.is_ok(), "entry {} must succeed", entry.path);
            }
        }

        fn mock_readonly_session() -> WsSession {
            use crate::extensions::fs::ws::auth::FsAccessMode;
            WsSession::new_for_test_with_mode(
                Arc::new(MockFsBackend {
                    atomic_supported: false,
                    fail_dirs: std::collections::HashSet::new(),
                    readdir_result: None,
                }),
                FsAccessMode::ReadOnly,
            )
        }

        #[tokio::test]
        async fn readonly_session_rejects_write() {
            use crate::extensions::fs::ws::protocol::WsRequest;
            let session = mock_readonly_session();
            let request = WsRequest::Write {
                id: "r1".to_string(),
                path: "/test.txt".to_string(),
                content: Some("aGVsbG8=".to_string()),
                encoding: "base64".to_string(),
                streaming: false,
                size: None,
                mode: None,
            };
            let resp = crate::extensions::fs::ws::handler::handle_request(&session, &request).await;
            assert!(!resp.ok);
            let detail = resp.error.expect("error detail should be present");
            assert_eq!(detail.code, WsErrorCode::Eacces);
            assert!(detail.message.contains("read-only session"));
        }

        #[tokio::test]
        async fn readonly_session_rejects_mkdir() {
            use crate::extensions::fs::ws::protocol::WsRequest;
            let session = mock_readonly_session();
            let request = WsRequest::Mkdir {
                id: "r2".to_string(),
                path: "/newdir".to_string(),
                recursive: false,
                mode: None,
            };
            let resp = crate::extensions::fs::ws::handler::handle_request(&session, &request).await;
            assert!(!resp.ok);
            let detail = resp.error.expect("error detail should be present");
            assert_eq!(detail.code, WsErrorCode::Eacces);
        }

        #[tokio::test]
        async fn readonly_session_allows_stat() {
            use crate::extensions::fs::ws::protocol::WsRequest;
            let session = mock_readonly_session();
            let request = WsRequest::Stat {
                id: "r3".to_string(),
                path: "/test.txt".to_string(),
            };
            let resp = crate::extensions::fs::ws::handler::handle_request(&session, &request).await;
            // Stat will fail because mock backend returns "not implemented",
            // but it should NOT fail with Eacces — the read-only guard must not block it.
            if let Some(detail) = &resp.error {
                assert_ne!(
                    detail.code,
                    WsErrorCode::Eacces,
                    "read operations must not be blocked by read-only mode"
                );
            }
        }
    }
}
