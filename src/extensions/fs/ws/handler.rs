use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::json;

use crate::extensions::fs::MAX_BYTES_PER_FILE;
use crate::extensions::fs::ws::auth::WsSession;
use crate::extensions::fs::ws::protocol::{
    FileInfoResponse, WsErrorCode, WsRequest, WsResponse, map_fs_error, validate_path,
};

pub(crate) async fn handle_request(session: &WsSession, request: &WsRequest) -> WsResponse {
    match request {
        WsRequest::Auth { id, .. } => WsResponse::error(id, WsErrorCode::Eproto, "already authenticated"),
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
            let entries: Vec<FileInfoResponse> = entries.into_iter().map(FileInfoResponse::from).collect();
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

#[cfg(test)]
mod tests {
    use super::decode_base64_content;
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
}
