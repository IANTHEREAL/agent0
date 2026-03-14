use anyhow::Error;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::json;
use std::collections::HashSet;
use tokio::time::{timeout, Duration};

use crate::extensions::fs::backend::{
    FsBatchWriteFile, FsMultipartCompletedPart, FsRecursiveReaddirOptions, FsRecursiveReaddirResult,
};
use crate::extensions::fs::config::fs9_config;
use crate::extensions::fs::embedded::types::EmbeddedFsError;
use crate::extensions::fs::ws::auth::WsSession;
use crate::extensions::fs::ws::protocol::{
    map_fs_error, validate_path, BatchInlineReadEntryResponse, BatchStatEntryResponse,
    BatchWriteEntryResponse, CreateUploadResponse, FileInfoResponse, HeaderPairResponse,
    MultipartCompletedPartRequest, PrepareDownloadResponse, PresignedRequestResponse,
    ReaddirRecursiveResponse, WsErrorCode, WsErrorDetail, WsRequest, WsResponse,
    MAX_JSON_FRAME_BYTES,
};
use crate::extensions::fs::MAX_BYTES_PER_FILE;

pub(crate) async fn handle_request(session: &WsSession, request: &WsRequest) -> WsResponse {
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
        } => handle_mkdir(session, id, path, *recursive).await,
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
            ..
        } => {
            if *streaming {
                return WsResponse::error(
                    id,
                    WsErrorCode::Eproto,
                    "streaming must be handled at connection level",
                );
            }
            handle_write(session, id, path, content.as_deref(), encoding).await
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
        WsRequest::CreateUpload { id, path, size } => {
            handle_create_upload(session, id, path, *size).await
        }
        WsRequest::PresignPart {
            id,
            upload_token,
            part_number,
        } => handle_presign_part(session, id, upload_token, *part_number).await,
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
        WsRequest::BatchStat { id, paths } => handle_batch_stat(session, id, paths).await,
        WsRequest::BatchInlineRead { id, paths } => {
            handle_batch_inline_read(session, id, paths).await
        }
        WsRequest::BatchWrite { id, files } => handle_batch_write(session, id, files).await,
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

    let result = session.backend.readdir(path).await;
    match result {
        Ok(entries) => {
            let entries: Vec<FileInfoResponse> =
                entries.into_iter().map(FileInfoResponse::from).collect();
            WsResponse::success(id, json!({ "entries": entries }))
        }
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            WsResponse::error(id, code, msg)
        }
    }
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

async fn handle_mkdir(session: &WsSession, id: &str, path: &str, recursive: bool) -> WsResponse {
    if let Err((code, msg)) = validate_path(path) {
        return WsResponse::error(id, code, msg);
    }

    let result = session.backend.mkdir(path, recursive).await;
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

    let result = session.backend.write_file(path, &data).await;
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

async fn handle_create_upload(session: &WsSession, id: &str, path: &str, size: u64) -> WsResponse {
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

    match session.backend.create_upload(path, size).await {
        Ok(upload) => {
            let upload_token = upload.upload_token.clone();
            let payload = match serde_json::to_value(CreateUploadResponse {
                upload_token: upload_token.clone(),
                upload_id: upload.upload_id,
                part_size: upload.part_size,
                expires_at: format_mtime(upload.expires_at),
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
) -> WsResponse {
    match session
        .backend
        .presign_upload_part(upload_token, part_number)
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

async fn handle_batch_write(
    session: &WsSession,
    id: &str,
    files: &[crate::extensions::fs::ws::protocol::BatchWriteFileRequest],
) -> WsResponse {
    let max_files = fs9_config().batch_write_max_files;
    if files.len() > max_files {
        return WsResponse::error(
            id,
            WsErrorCode::Efbig,
            format!(
                "batch_write supports at most {} files per request",
                max_files
            ),
        );
    }

    let inline_max = fs9_config().inline_max_bytes;
    let max_total_raw = fs9_config().batch_write_max_total_bytes;
    let max_total_encoded = fs9_config().batch_write_max_encoded_bytes;
    let mut decoded_files = Vec::with_capacity(files.len());
    let mut total_raw = 0usize;
    let mut total_encoded = 0usize;
    let mut seen_paths = HashSet::with_capacity(files.len());

    for file in files {
        if let Err((code, msg)) = validate_path(&file.path) {
            return WsResponse::error(id, code, msg);
        }
        if !seen_paths.insert(file.path.clone()) {
            return WsResponse::error(
                id,
                WsErrorCode::Einval,
                format!("batch_write requires unique paths: {}", file.path),
            );
        }
        total_encoded = total_encoded.saturating_add(file.content.len());
        if total_encoded > max_total_encoded {
            return WsResponse::error(
                id,
                WsErrorCode::Efbig,
                format!(
                    "batch_write encoded payload exceeds limit {} bytes",
                    max_total_encoded
                ),
            );
        }

        let data = match decode_base64_content(id, &file.content, &file.encoding) {
            Ok(data) => data,
            Err(resp) => return resp,
        };
        if data.len() > inline_max {
            return WsResponse::error(
                id,
                WsErrorCode::Efbig,
                format!(
                    "batch_write file {} exceeds inline limit {} bytes",
                    file.path, inline_max
                ),
            );
        }
        total_raw = total_raw.saturating_add(data.len());
        if total_raw > max_total_raw {
            return WsResponse::error(
                id,
                WsErrorCode::Efbig,
                format!(
                    "batch_write raw payload exceeds limit {} bytes",
                    max_total_raw
                ),
            );
        }
        decoded_files.push((file.path.clone(), data));
    }

    let batch_files = decoded_files
        .into_iter()
        .map(|(path, data)| FsBatchWriteFile { path, data })
        .collect::<Vec<_>>();
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
        map_batch_inline_read_error, parse_sha256_checksum, recursive_readdir_response,
    };
    use crate::extensions::fs::backend::{FsFileInfo, FsRecursiveReaddirResult};
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
}
