use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;

use crate::extensions::fs::backend::{FsBatchWriteFile, FsMultipartCompletedPart};
use crate::extensions::fs::config::fs9_config;
use crate::extensions::fs::ws::auth::WsSession;
use crate::extensions::fs::ws::protocol::{
    map_fs_error, validate_path, BatchInlineReadEntryResponse, BatchStatEntryResponse,
    BatchWriteEntryResponse, CreateUploadResponse, FileInfoResponse, HeaderPairResponse,
    MultipartCompletedPartRequest, PrepareDownloadResponse, PresignedRequestResponse, WsErrorCode,
    WsErrorDetail, WsRequest, WsResponse,
};
use crate::extensions::fs::MAX_BYTES_PER_FILE;

pub(crate) async fn handle_request(session: &WsSession, request: &WsRequest) -> WsResponse {
    match request {
        WsRequest::Auth { id, .. } => {
            WsResponse::error(id, WsErrorCode::Eproto, "already authenticated")
        }
        WsRequest::Stat { id, path } => handle_stat(session, id, path).await,
        WsRequest::Readdir { id, path } => handle_readdir(session, id, path).await,
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

    let concurrency = fs9_config().batch_stat_concurrency.max(1);
    let backend = session.backend.clone();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut join_set = tokio::task::JoinSet::new();

    for (idx, path) in paths.iter().cloned().enumerate() {
        let backend = backend.clone();
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("batch_stat semaphore must not be closed");
        join_set.spawn(async move {
            let _permit = permit;
            let entry = match backend.stat(&path).await {
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
            };
            (idx, entry)
        });
    }

    let mut entries: Vec<(usize, BatchStatEntryResponse)> = Vec::with_capacity(paths.len());
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(entry) => entries.push(entry),
            Err(err) => {
                return WsResponse::error(
                    id,
                    WsErrorCode::Eio,
                    format!("batch_stat task failed: {err}"),
                );
            }
        }
    }
    entries.sort_by_key(|(idx, _)| *idx);
    let entries: Vec<BatchStatEntryResponse> =
        entries.into_iter().map(|(_, entry)| entry).collect();

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
    let max_total_bytes_u64 = max_total_bytes as u64;

    let concurrency = fs9_config().batch_stat_concurrency.max(1);
    let backend = session.backend.clone();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut join_set = tokio::task::JoinSet::new();

    for (idx, path) in paths.iter().cloned().enumerate() {
        let backend = backend.clone();
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("batch_inline_read semaphore must not be closed");
        join_set.spawn(async move {
            let _permit = permit;
            let result = backend.stat(&path).await;
            (idx, path, result)
        });
    }

    // Phase 1: stat + bounds preflight (so we can enforce total payload cap before doing reads).
    let mut entries: Vec<Option<BatchInlineReadEntryResponse>> = vec![None; paths.len()];
    let mut eligible: Vec<Option<u64>> = vec![None; paths.len()];
    let mut total_planned = 0u64;

    while let Some(result) = join_set.join_next().await {
        let (idx, path, stat_result) = match result {
            Ok(tuple) => tuple,
            Err(err) => {
                return WsResponse::error(
                    id,
                    WsErrorCode::Eio,
                    format!("batch_inline_read task failed: {err}"),
                );
            }
        };

        match stat_result {
            Ok(info) => {
                if info.is_dir {
                    entries[idx] = Some(BatchInlineReadEntryResponse {
                        path,
                        ok: false,
                        content: None,
                        size: None,
                        encoding: None,
                        error: Some(WsErrorDetail {
                            code: WsErrorCode::Eisdir,
                            message: "Is a directory".to_string(),
                        }),
                    });
                    continue;
                }

                if info.size > max_file_bytes as u64 {
                    entries[idx] = Some(BatchInlineReadEntryResponse {
                        path,
                        ok: false,
                        content: None,
                        size: None,
                        encoding: None,
                        error: Some(WsErrorDetail {
                            code: WsErrorCode::Efbig,
                            message: format!(
                                "file too large for batch_inline_read: {} bytes exceeds limit {}",
                                info.size, max_file_bytes
                            ),
                        }),
                    });
                    continue;
                }

                total_planned = total_planned.saturating_add(info.size);
                eligible[idx] = Some(info.size);
            }
            Err(err) => {
                let (code, msg) = map_fs_error(&err);
                entries[idx] = Some(BatchInlineReadEntryResponse {
                    path,
                    ok: false,
                    content: None,
                    size: None,
                    encoding: None,
                    error: Some(WsErrorDetail { code, message: msg }),
                });
            }
        }
    }

    if total_planned > max_total_bytes_u64 {
        return WsResponse::error(
            id,
            WsErrorCode::Efbig,
            format!(
                "batch_inline_read raw payload exceeds limit {} bytes",
                max_total_bytes
            ),
        );
    }

    // Phase 2: bounded reads for eligible entries.
    let backend = session.backend.clone();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut join_set = tokio::task::JoinSet::new();
    for (idx, (path, planned_size)) in paths
        .iter()
        .cloned()
        .zip(eligible.iter().copied())
        .enumerate()
    {
        let Some(_planned) = planned_size else {
            continue;
        };
        let backend = backend.clone();
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("batch_inline_read semaphore must not be closed");
        join_set.spawn(async move {
            let _permit = permit;
            let result = backend.read_file(&path, max_file_bytes).await;
            (idx, path, result)
        });
    }

    while let Some(result) = join_set.join_next().await {
        let (idx, path, read_result) = match result {
            Ok(tuple) => tuple,
            Err(err) => {
                return WsResponse::error(
                    id,
                    WsErrorCode::Eio,
                    format!("batch_inline_read task failed: {err}"),
                );
            }
        };
        match read_result {
            Ok(data) => {
                entries[idx] = Some(BatchInlineReadEntryResponse {
                    path,
                    ok: true,
                    content: Some(STANDARD.encode(&data)),
                    size: Some(data.len()),
                    encoding: Some("base64".to_string()),
                    error: None,
                });
            }
            Err(err) => {
                let (code, msg) = map_fs_error(&err);
                entries[idx] = Some(BatchInlineReadEntryResponse {
                    path,
                    ok: false,
                    content: None,
                    size: None,
                    encoding: None,
                    error: Some(WsErrorDetail { code, message: msg }),
                });
            }
        }
    }

    let entries: Vec<BatchInlineReadEntryResponse> = entries
        .into_iter()
        .map(|entry| {
            entry.expect("batch_inline_read must produce a result entry for each input path")
        })
        .collect();
    WsResponse::success(id, json!({ "entries": entries }))
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
                            code: code.clone(),
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
    use super::{decode_base64_content, parse_sha256_checksum};
    use crate::extensions::fs::ws::protocol::WsErrorCode;

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
}
