pub(crate) mod auth;
pub(crate) mod handler;
pub(crate) mod protocol;
pub(crate) mod stream;

use std::net::SocketAddr;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{timeout, Duration};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::{Error as WsIoError, Message};
use tokio_tungstenite::{accept_async, WebSocketStream};
use tracing::{debug, info, warn};

use crate::extensions::fs::ws::auth::{WsConnectionTracker, WsSession};
use crate::extensions::fs::ws::protocol::{
    map_fs_error, validate_path, StreamEnd, StreamStartResponse, StreamWriteReady, WsErrorCode,
    WsRequest, WsResponse, AUTH_TIMEOUT_SECS, DEFAULT_CHUNK_SIZE,
    DEFAULT_MAX_CONNECTIONS_PER_TENANT, IDLE_TIMEOUT_SECS, MAX_JSON_FRAME_BYTES,
    STREAMING_THRESHOLD,
};
use crate::extensions::fs::MAX_BYTES_PER_FILE;
use crate::pool::TikvClientPool;

const KEYSPACE_PREFIX: &str = "db9_tenant_";

pub(crate) async fn start_ws_server(
    listener: TcpListener,
    pool: Arc<TikvClientPool>,
    tls_acceptor: Option<Arc<TlsAcceptor>>,
) {
    let tracker = Arc::new(WsConnectionTracker::new(DEFAULT_MAX_CONNECTIONS_PER_TENANT));

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(err) => {
                warn!("fs9 ws accept error: {err}");
                continue;
            }
        };

        let pool = pool.clone();
        let tls_acceptor = tls_acceptor.clone();
        let tracker = tracker.clone();
        tokio::spawn(async move {
            if let Err(err) =
                handle_connection(stream, peer_addr, pool, tls_acceptor, tracker).await
            {
                warn!("fs9 ws connection {peer_addr} ended with error: {err}");
            }
        });
    }
}

struct StreamingWriteState {
    request_id: String,
    path: String,
    stream_id: u64,
    expected_size: Option<u64>,
    buffer: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct StreamEndRequest {
    id: Option<String>,
    stream: String,
    stream_id: u64,
    checksum: Option<String>,
}

async fn handle_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    pool: Arc<TikvClientPool>,
    tls_acceptor: Option<Arc<TlsAcceptor>>,
    tracker: Arc<WsConnectionTracker>,
) -> Result<(), WsIoError> {
    info!("fs9 ws connection accepted from {peer_addr}");

    if let Some(acceptor) = tls_acceptor {
        let tls_stream = match acceptor.accept(stream).await {
            Ok(s) => s,
            Err(err) => {
                warn!("fs9 ws TLS handshake failed for {peer_addr}: {err}");
                return Ok(());
            }
        };
        handle_ws_connection(tls_stream, peer_addr, pool, tracker).await
    } else {
        handle_ws_connection(stream, peer_addr, pool, tracker).await
    }
}

async fn handle_ws_connection<S>(
    stream: S,
    peer_addr: SocketAddr,
    pool: Arc<TikvClientPool>,
    tracker: Arc<WsConnectionTracker>,
) -> Result<(), WsIoError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut ws_stream = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(err) => {
            warn!("fs9 ws handshake failed for {peer_addr}: {err}");
            return Ok(());
        }
    };

    let auth_message = match timeout(Duration::from_secs(AUTH_TIMEOUT_SECS), ws_stream.next()).await
    {
        Ok(Some(Ok(msg))) => msg,
        Ok(Some(Err(err))) => return Err(err),
        Ok(None) => {
            debug!("fs9 ws closed before auth from {peer_addr}");
            return Ok(());
        }
        Err(_) => {
            warn!("fs9 ws auth timeout from {peer_addr}");
            let _ = ws_stream.close(None).await;
            return Ok(());
        }
    };

    let (auth_request, auth_id) = match parse_auth_request(auth_message) {
        Ok(req) => {
            let id = req.id().to_string();
            (req, id)
        }
        Err(resp) => {
            send_response(&mut ws_stream, &resp).await?;
            let _ = ws_stream.close(None).await;
            return Ok(());
        }
    };

    let WsRequest::Auth {
        id,
        username,
        password,
    } = auth_request
    else {
        send_response(
            &mut ws_stream,
            &WsResponse::error(&auth_id, WsErrorCode::Eproto, "first message must be auth"),
        )
        .await?;
        let _ = ws_stream.close(None).await;
        return Ok(());
    };

    let session = match auth::handle_auth(&id, &username, &password, &pool).await {
        Ok(session) => session,
        Err(err_response) => {
            send_response(&mut ws_stream, &err_response).await?;
            let _ = ws_stream.close(None).await;
            return Ok(());
        }
    };

    let _guard = match tracker.try_acquire(&session.keyspace) {
        Ok(guard) => guard,
        Err(mut err_response) => {
            err_response.id = id.clone();
            send_response(&mut ws_stream, &err_response).await?;
            let _ = ws_stream.close(None).await;
            return Ok(());
        }
    };

    let auth_user = session.user.clone();
    let auth_keyspace = session.keyspace.clone();
    let auth_success = WsResponse::success(
        &id,
        json!({
            "user": auth_user,
            "tenant": tenant_from_keyspace(&auth_keyspace),
            "keyspace": auth_keyspace,
        }),
    );
    send_response(&mut ws_stream, &auth_success).await?;

    let mut streaming_write: Option<StreamingWriteState> = None;
    loop {
        let msg = match timeout(Duration::from_secs(IDLE_TIMEOUT_SECS), ws_stream.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(err))) => return Err(err),
            Ok(None) => break,
            Err(_) => {
                debug!("fs9 ws idle timeout for {peer_addr}");
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                if text.len() > MAX_JSON_FRAME_BYTES {
                    let resp = WsResponse::error(
                        "",
                        WsErrorCode::Efbig,
                        format!(
                            "JSON frame too large: {} bytes exceeds limit {}",
                            text.len(),
                            MAX_JSON_FRAME_BYTES
                        ),
                    );
                    send_response(&mut ws_stream, &resp).await?;
                    break;
                }

                if let Some(state) = streaming_write.as_mut() {
                    let end = match parse_stream_end_request(&text) {
                        Ok(end) => end,
                        Err(resp) => {
                            send_response(&mut ws_stream, &resp).await?;
                            break;
                        }
                    };

                    if end.stream != "end" {
                        let resp = WsResponse::error(
                            &state.request_id,
                            WsErrorCode::Eproto,
                            "expected stream end frame",
                        );
                        send_response(&mut ws_stream, &resp).await?;
                        break;
                    }

                    if end.stream_id != state.stream_id {
                        let resp = WsResponse::error(
                            &state.request_id,
                            WsErrorCode::Eproto,
                            format!(
                                "stream id mismatch: expected {}, got {}",
                                state.stream_id, end.stream_id
                            ),
                        );
                        send_response(&mut ws_stream, &resp).await?;
                        break;
                    }

                    if let Some(end_id) = end.id.as_deref() {
                        if end_id != state.request_id {
                            let resp = WsResponse::error(
                                &state.request_id,
                                WsErrorCode::Eproto,
                                "stream end id mismatch",
                            );
                            send_response(&mut ws_stream, &resp).await?;
                            break;
                        }
                    }

                    if let Some(expected_size) = state.expected_size {
                        if state.buffer.len() as u64 != expected_size {
                            let resp = WsResponse::error(
                                &state.request_id,
                                WsErrorCode::Einval,
                                format!(
                                    "stream size mismatch: expected {expected_size}, got {}",
                                    state.buffer.len()
                                ),
                            );
                            send_response(&mut ws_stream, &resp).await?;
                            break;
                        }
                    }

                    if let Some(expected_checksum) = end.checksum.as_deref() {
                        if !stream::verify_checksum(&state.buffer, expected_checksum) {
                            let resp = WsResponse::error(
                                &state.request_id,
                                WsErrorCode::Eio,
                                "checksum mismatch",
                            );
                            send_response(&mut ws_stream, &resp).await?;
                            break;
                        }
                    }

                    let write_result = session.backend.write_file(&state.path, &state.buffer).await;
                    let response = match write_result {
                        Ok(written) => {
                            WsResponse::success(&state.request_id, json!({ "written": written }))
                        }
                        Err(err) => {
                            let (code, message) = map_fs_error(&err);
                            WsResponse::error(&state.request_id, code, message)
                        }
                    };
                    send_response(&mut ws_stream, &response).await?;
                    streaming_write = None;
                    continue;
                }

                let request = match serde_json::from_str::<WsRequest>(&text) {
                    Ok(request) => request,
                    Err(err) => {
                        let resp = WsResponse::error(
                            "",
                            WsErrorCode::Eproto,
                            format!("invalid JSON request: {err}"),
                        );
                        send_response(&mut ws_stream, &resp).await?;
                        break;
                    }
                };

                match &request {
                    WsRequest::Read {
                        id,
                        path,
                        offset,
                        length,
                        streaming,
                    } if *streaming => {
                        let data =
                            match streaming_read_data(&session, id, path, *offset, *length).await {
                                Ok(data) => data,
                                Err(resp) => {
                                    send_response(&mut ws_stream, &resp).await?;
                                    continue;
                                }
                            };

                        if data.len() >= STREAMING_THRESHOLD {
                            let stream_id = stream::next_stream_id();
                            let start = StreamStartResponse {
                                streaming: true,
                                stream_id,
                                size: data.len() as u64,
                                chunk_size: DEFAULT_CHUNK_SIZE,
                            };

                            let start_resp = WsResponse::success(
                                id,
                                serde_json::to_value(start).unwrap_or_else(|_| json!({})),
                            );
                            send_response(&mut ws_stream, &start_resp).await?;

                            for chunk in data.chunks(DEFAULT_CHUNK_SIZE) {
                                let frame = stream::encode_binary_frame(stream_id, chunk);
                                ws_stream.send(Message::Binary(frame)).await?;
                            }

                            let end = StreamEnd {
                                stream: "end".to_string(),
                                stream_id,
                                checksum: Some(stream::compute_checksum(&data)),
                            };
                            let end_resp = WsResponse::success(
                                id,
                                serde_json::to_value(end).unwrap_or_else(|_| json!({})),
                            );
                            send_response(&mut ws_stream, &end_resp).await?;
                        } else {
                            let inline_resp = WsResponse::success(
                                id,
                                json!({
                                    "content": STANDARD.encode(&data),
                                    "size": data.len(),
                                    "encoding": "base64"
                                }),
                            );
                            send_response(&mut ws_stream, &inline_resp).await?;
                        }
                    }
                    WsRequest::Write {
                        id,
                        path,
                        content,
                        streaming,
                        size,
                        ..
                    } if *streaming => {
                        if let Err((code, msg)) = validate_path(path) {
                            send_response(&mut ws_stream, &WsResponse::error(id, code, msg))
                                .await?;
                            continue;
                        }

                        if content.is_some() {
                            let resp = WsResponse::error(
                                id,
                                WsErrorCode::Einval,
                                "streaming write must not include content",
                            );
                            send_response(&mut ws_stream, &resp).await?;
                            continue;
                        }

                        if let Some(expected_size) = size {
                            if *expected_size > MAX_BYTES_PER_FILE as u64 {
                                let resp = WsResponse::error(
                                    id,
                                    WsErrorCode::Efbig,
                                    format!(
                                        "file too large: {} bytes exceeds limit {}",
                                        expected_size, MAX_BYTES_PER_FILE
                                    ),
                                );
                                send_response(&mut ws_stream, &resp).await?;
                                continue;
                            }
                        }

                        let stream_id = stream::next_stream_id();
                        let ready = StreamWriteReady {
                            ready: true,
                            stream_id,
                            chunk_size: DEFAULT_CHUNK_SIZE,
                        };
                        let ready_resp = WsResponse::success(
                            id,
                            serde_json::to_value(ready).unwrap_or_else(|_| json!({})),
                        );
                        send_response(&mut ws_stream, &ready_resp).await?;

                        let capacity = size
                            .and_then(|v| usize::try_from(v).ok())
                            .unwrap_or(DEFAULT_CHUNK_SIZE)
                            .min(MAX_BYTES_PER_FILE);
                        streaming_write = Some(StreamingWriteState {
                            request_id: id.clone(),
                            path: path.clone(),
                            stream_id,
                            expected_size: *size,
                            buffer: Vec::with_capacity(capacity),
                        });
                    }
                    _ => {
                        let response = handler::handle_request(&session, &request).await;
                        send_response(&mut ws_stream, &response).await?;
                    }
                }
            }
            Message::Binary(data) => {
                let Some(state) = streaming_write.as_mut() else {
                    let resp = WsResponse::error(
                        "",
                        WsErrorCode::Eproto,
                        "unexpected binary frame outside streaming write",
                    );
                    send_response(&mut ws_stream, &resp).await?;
                    break;
                };

                let Some((stream_id, chunk)) = stream::decode_binary_frame(&data) else {
                    let resp = WsResponse::error(
                        &state.request_id,
                        WsErrorCode::Eproto,
                        "invalid binary frame",
                    );
                    send_response(&mut ws_stream, &resp).await?;
                    break;
                };

                if stream_id != state.stream_id {
                    let resp = WsResponse::error(
                        &state.request_id,
                        WsErrorCode::Eproto,
                        format!(
                            "binary frame stream id mismatch: expected {}, got {}",
                            state.stream_id, stream_id
                        ),
                    );
                    send_response(&mut ws_stream, &resp).await?;
                    break;
                }

                let next_size = state.buffer.len().saturating_add(chunk.len());
                if next_size > MAX_BYTES_PER_FILE {
                    let resp = WsResponse::error(
                        &state.request_id,
                        WsErrorCode::Efbig,
                        format!(
                            "file too large: {} bytes exceeds limit {}",
                            next_size, MAX_BYTES_PER_FILE
                        ),
                    );
                    send_response(&mut ws_stream, &resp).await?;
                    break;
                }

                state.buffer.extend_from_slice(chunk);
            }
            Message::Ping(payload) => {
                ws_stream.send(Message::Pong(payload)).await?;
            }
            Message::Pong(_) => {}
            Message::Close(_) => break,
            Message::Frame(_) => {}
        }
    }

    let _ = ws_stream.close(None).await;
    info!("fs9 ws connection closed from {peer_addr}");
    Ok(())
}

fn parse_auth_request(message: Message) -> Result<WsRequest, WsResponse> {
    match message {
        Message::Text(text) => {
            if text.len() > MAX_JSON_FRAME_BYTES {
                return Err(WsResponse::error(
                    "",
                    WsErrorCode::Efbig,
                    format!(
                        "JSON frame too large: {} bytes exceeds limit {}",
                        text.len(),
                        MAX_JSON_FRAME_BYTES
                    ),
                ));
            }
            serde_json::from_str::<WsRequest>(&text).map_err(|err| {
                WsResponse::error(
                    "",
                    WsErrorCode::Eproto,
                    format!("invalid auth request: {err}"),
                )
            })
        }
        Message::Close(_) => Err(WsResponse::error(
            "",
            WsErrorCode::Eproto,
            "connection closed before auth",
        )),
        Message::Ping(_) | Message::Pong(_) => Err(WsResponse::error(
            "",
            WsErrorCode::Eproto,
            "first message must be auth",
        )),
        Message::Binary(_) | Message::Frame(_) => Err(WsResponse::error(
            "",
            WsErrorCode::Eproto,
            "first message must be JSON auth request",
        )),
    }
}

fn parse_stream_end_request(text: &str) -> Result<StreamEndRequest, WsResponse> {
    serde_json::from_str::<StreamEndRequest>(text).map_err(|err| {
        WsResponse::error(
            "",
            WsErrorCode::Eproto,
            format!("invalid stream end message: {err}"),
        )
    })
}

async fn send_response<S>(
    ws_stream: &mut WebSocketStream<S>,
    response: &WsResponse,
) -> Result<(), WsIoError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let payload = serde_json::to_string(response).map_err(|err| {
        WsIoError::Io(std::io::Error::other(format!(
            "failed to serialize websocket response: {err}"
        )))
    })?;
    ws_stream.send(Message::Text(payload)).await
}

fn tenant_from_keyspace(keyspace: &str) -> String {
    keyspace
        .strip_prefix(KEYSPACE_PREFIX)
        .unwrap_or(keyspace)
        .to_string()
}

async fn streaming_read_data(
    session: &WsSession,
    id: &str,
    path: &str,
    offset: Option<u64>,
    length: Option<usize>,
) -> Result<Vec<u8>, WsResponse> {
    if let Err((code, msg)) = validate_path(path) {
        return Err(WsResponse::error(id, code, msg));
    }

    let result = match (offset, length) {
        (Some(off), Some(len)) => session.backend.read_file_at(path, off, len).await,
        (None, None) => session.backend.read_file(path, MAX_BYTES_PER_FILE).await,
        _ => {
            return Err(WsResponse::error(
                id,
                WsErrorCode::Einval,
                "offset and length must be provided together",
            ));
        }
    };

    result.map_err(|err| {
        let (code, msg) = map_fs_error(&err);
        WsResponse::error(id, code, msg)
    })
}
