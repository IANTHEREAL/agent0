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
use sha2::{Digest, Sha256};
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::{Error as WsIoError, Message};
use tokio_tungstenite::{accept_async, WebSocketStream};
use tracing::{debug, info, warn};

use crate::extensions::fs::backend::{FsWriteStream, FsWriteStreamOptions};
use crate::extensions::fs::config::fs9_config;
use crate::extensions::fs::ws::auth::{FsAccessMode, WsConnectionTracker, WsSession};
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
    default_keyspace: Option<String>,
    tcp_keepalive_idle_ms: u64,
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

        // Enable TCP keepalive so NLBs and firewalls do not silently kill idle
        // connections.  Uses the same DB9_TCP_KEEPALIVE_IDLE_MS config as pgwire;
        // 0 disables keepalive.
        if tcp_keepalive_idle_ms > 0 {
            if let Err(e) = configure_ws_tcp_keepalive(&stream, tcp_keepalive_idle_ms) {
                warn!("fs9 ws: failed to set TCP keepalive for {peer_addr}: {e}");
            }
        }

        let pool = pool.clone();
        let tls_acceptor = tls_acceptor.clone();
        let tracker = tracker.clone();
        let default_keyspace = default_keyspace.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(
                stream,
                peer_addr,
                pool,
                tls_acceptor,
                tracker,
                default_keyspace,
            )
            .await
            {
                warn!("fs9 ws connection {peer_addr} ended with error: {err}");
            }
        });
    }
}

fn configure_ws_tcp_keepalive(stream: &TcpStream, idle_ms: u64) -> std::io::Result<()> {
    let sock_ref = SockRef::from(stream);
    let keepalive = TcpKeepalive::new().with_time(Duration::from_millis(idle_ms));
    sock_ref.set_tcp_keepalive(&keepalive)
}

struct StreamingWriteState {
    request_id: String,
    stream_id: u64,
    expected_size: Option<u64>,
    bytes_written: u64,
    hasher: Sha256,
    writer: Box<dyn FsWriteStream>,
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
    default_keyspace: Option<String>,
) -> Result<(), WsIoError> {
    info!("fs9 ws connection accepted from {peer_addr}");

    if let Some(acceptor) = tls_acceptor {
        let tls_stream = match acceptor.accept(stream).await {
            Ok(s) => s,
            Err(err) => {
                match err.kind() {
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset => {
                        debug!("fs9 ws TLS handshake closed early for {peer_addr}: {err}");
                    }
                    _ => {
                        warn!("fs9 ws TLS handshake failed for {peer_addr}: {err}");
                    }
                }
                return Ok(());
            }
        };
        handle_ws_connection(tls_stream, peer_addr, pool, tracker, default_keyspace, true).await
    } else {
        handle_ws_connection(stream, peer_addr, pool, tracker, default_keyspace, false).await
    }
}

async fn handle_ws_connection<S>(
    stream: S,
    peer_addr: SocketAddr,
    pool: Arc<TikvClientPool>,
    tracker: Arc<WsConnectionTracker>,
    default_keyspace: Option<String>,
    is_secure: bool,
) -> Result<(), WsIoError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
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

    let session = match auth::handle_auth(
        &id,
        &username,
        &password,
        &pool,
        default_keyspace.as_deref(),
        is_secure,
    )
    .await
    {
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

    let auth_success = WsResponse::success(&id, session.build_auth_success_data());
    send_response(&mut ws_stream, &auth_success).await?;

    let session = Arc::new(session);
    let max_inflight = fs9_config().ws_max_inflight_requests_per_connection.max(1);
    let request_slots = Arc::new(tokio::sync::Semaphore::new(max_inflight));
    // Outgoing queue needs to absorb pipelined JSON responses and (optionally) streaming frames.
    // Keep it bounded and proportional to the in-flight request cap.
    let outgoing_capacity = (max_inflight.saturating_mul(16)).clamp(256, 4096);
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(outgoing_capacity);

    let (mut ws_sink, mut ws_source) = ws_stream.split();
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    let mut streaming_write: Option<StreamingWriteState> = None;
    // Server-initiated ping keeps the connection alive through NLBs/firewalls
    // that track idle connections (e.g. AWS NLB default 350s timeout).
    let mut ping_interval = tokio::time::interval(Duration::from_secs(60));
    ping_interval.reset(); // don't fire immediately

    // Idle deadline is independent of the ping interval — only reset when the
    // *client* sends a real message (text/binary).  Server-originated pings do
    // NOT extend the idle deadline, so silent clients still get disconnected
    // after IDLE_TIMEOUT_SECS even though pings keep the transport alive.
    let idle_deadline = tokio::time::sleep(Duration::from_secs(IDLE_TIMEOUT_SECS));
    tokio::pin!(idle_deadline);

    loop {
        let msg = tokio::select! {
            biased; // prefer real messages over pings
            result = ws_source.next() => {
                match result {
                    Some(Ok(msg)) => msg,
                    Some(Err(err)) => {
                        // Abort any in-flight streaming write before propagating WS error
                        if let Some(state) = streaming_write.take() {
                            abort_streaming_write(state).await;
                        }
                        writer.abort();
                        return Err(err);
                    }
                    None => break,
                }
            }
            _ = &mut idle_deadline => {
                debug!("fs9 ws idle timeout for {peer_addr}");
                break;
            }
            _ = ping_interval.tick() => {
                // Send a WebSocket Ping to keep the transport alive through
                // NLBs without resetting the idle deadline.  Use try_send to
                // avoid blocking the main loop if the outbound queue is full
                // (a slow/non-reading client should not prevent idle timeout).
                let _ = out_tx.try_send(Message::Ping(vec![]));
                continue;
            }
        };

        // Reset the idle deadline on real client data frames (Text/Binary)
        // BEFORE dispatch — some Text branches `continue` early, and we
        // must still credit the client for sending data.  Control frames
        // (Ping/Pong/Close) are auto-generated and do NOT reset.
        if matches!(msg, Message::Text(_) | Message::Binary(_)) {
            idle_deadline
                .as_mut()
                .reset(tokio::time::Instant::now() + Duration::from_secs(IDLE_TIMEOUT_SECS));
        } else if idle_deadline.is_elapsed() {
            // Control frame arrived but idle deadline already expired.
            // The `biased` select may have picked ws_source over the
            // expired deadline — enforce the timeout now.
            debug!("fs9 ws idle timeout for {peer_addr}");
            break;
        }

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
                    let _ = send_response_tx(&out_tx, &resp).await;
                    break;
                }

                if let Some(state) = streaming_write.take() {
                    let end = match parse_stream_end_request(&text) {
                        Ok(end) => end,
                        Err(resp) => {
                            abort_streaming_write(state).await;
                            let _ = send_response_tx(&out_tx, &resp).await;
                            break;
                        }
                    };

                    if end.stream != "end" {
                        let resp = WsResponse::error(
                            &state.request_id,
                            WsErrorCode::Eproto,
                            "expected stream end frame",
                        );
                        abort_streaming_write(state).await;
                        let _ = send_response_tx(&out_tx, &resp).await;
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
                        abort_streaming_write(state).await;
                        let _ = send_response_tx(&out_tx, &resp).await;
                        break;
                    }

                    if let Some(end_id) = end.id.as_deref() {
                        if end_id != state.request_id {
                            let resp = WsResponse::error(
                                &state.request_id,
                                WsErrorCode::Eproto,
                                "stream end id mismatch",
                            );
                            abort_streaming_write(state).await;
                            let _ = send_response_tx(&out_tx, &resp).await;
                            break;
                        }
                    }

                    if let Some(expected_size) = state.expected_size {
                        if state.bytes_written != expected_size {
                            let resp = WsResponse::error(
                                &state.request_id,
                                WsErrorCode::Einval,
                                format!(
                                    "stream size mismatch: expected {expected_size}, got {}",
                                    state.bytes_written
                                ),
                            );
                            abort_streaming_write(state).await;
                            let _ = send_response_tx(&out_tx, &resp).await;
                            break;
                        }
                    }

                    if let Some(expected_checksum) = end.checksum.as_deref() {
                        let actual_checksum =
                            format!("sha256:{}", hex::encode(state.hasher.clone().finalize()));
                        if actual_checksum != expected_checksum {
                            let resp = WsResponse::error(
                                &state.request_id,
                                WsErrorCode::Eio,
                                "checksum mismatch",
                            );
                            abort_streaming_write(state).await;
                            let _ = send_response_tx(&out_tx, &resp).await;
                            break;
                        }
                    }

                    let write_result = state.writer.finish().await;
                    let response = match write_result {
                        Ok(written) => {
                            WsResponse::success(&state.request_id, json!({ "written": written }))
                        }
                        Err(err) => {
                            let (code, message) = map_fs_error(&err);
                            WsResponse::error(&state.request_id, code, message)
                        }
                    };
                    let _ = send_response_tx(&out_tx, &response).await;
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
                        let _ = send_response_tx(&out_tx, &resp).await;
                        break;
                    }
                };

                match request {
                    WsRequest::Read {
                        id,
                        path,
                        offset,
                        length,
                        streaming,
                    } => {
                        let permit = request_slots
                            .clone()
                            .acquire_owned()
                            .await
                            .expect("ws request semaphore must not be closed");
                        let session = session.clone();
                        let out_tx = out_tx.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(err) = handle_ws_read_tx(
                                &out_tx, &session, &id, &path, offset, length, streaming,
                            )
                            .await
                            {
                                let resp = WsResponse::error(
                                    &id,
                                    WsErrorCode::Eio,
                                    format!("read failed: {err}"),
                                );
                                let _ = send_response_tx(&out_tx, &resp).await;
                            }
                        });
                    }
                    WsRequest::Write {
                        id,
                        path,
                        content,
                        streaming: true,
                        size,
                        mode,
                        encoding: _,
                    } => {
                        if session.access_mode == FsAccessMode::ReadOnly {
                            let resp = WsResponse::error(
                                &id,
                                WsErrorCode::Eacces,
                                "fs9: read-only session — write operations are not permitted",
                            );
                            let _ = send_response_tx(&out_tx, &resp).await;
                            continue;
                        }

                        if let Err((code, msg)) = validate_path(&path) {
                            let _ =
                                send_response_tx(&out_tx, &WsResponse::error(&id, code, msg)).await;
                            continue;
                        }

                        if content.is_some() {
                            let resp = WsResponse::error(
                                &id,
                                WsErrorCode::Einval,
                                "streaming write must not include content",
                            );
                            let _ = send_response_tx(&out_tx, &resp).await;
                            continue;
                        }

                        if let Some(expected_size) = size {
                            if expected_size > MAX_BYTES_PER_FILE as u64 {
                                let resp = WsResponse::error(
                                    &id,
                                    WsErrorCode::Efbig,
                                    format!(
                                        "file too large: {} bytes exceeds limit {}",
                                        expected_size, MAX_BYTES_PER_FILE
                                    ),
                                );
                                let _ = send_response_tx(&out_tx, &resp).await;
                                continue;
                            }
                        }

                        let writer = match session
                            .backend
                            .begin_write_stream(
                                &path,
                                FsWriteStreamOptions {
                                    expected_size: size,
                                    mode: mode.map(|m| m & 0o7777),
                                },
                            )
                            .await
                        {
                            Ok(writer) => writer,
                            Err(err) => {
                                let (code, message) = map_fs_error(&err);
                                let resp = WsResponse::error(&id, code, message);
                                let _ = send_response_tx(&out_tx, &resp).await;
                                continue;
                            }
                        };

                        let stream_id = stream::next_stream_id();
                        let ready = StreamWriteReady {
                            ready: true,
                            stream_id,
                            chunk_size: DEFAULT_CHUNK_SIZE,
                        };
                        let ready_resp = WsResponse::success(
                            &id,
                            serde_json::to_value(ready).unwrap_or_else(|_| json!({})),
                        );
                        if send_response_tx(&out_tx, &ready_resp).await.is_err() {
                            // Writer exists but is not in streaming_write yet — abort directly.
                            let _ = writer.abort().await;
                            break;
                        }
                        streaming_write = Some(StreamingWriteState {
                            request_id: id,
                            stream_id,
                            expected_size: size,
                            bytes_written: 0,
                            hasher: Sha256::new(),
                            writer,
                        });
                    }
                    request => {
                        let permit = request_slots
                            .clone()
                            .acquire_owned()
                            .await
                            .expect("ws request semaphore must not be closed");
                        let session = session.clone();
                        let out_tx = out_tx.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            let response = handler::handle_request(&session, &request).await;
                            let _ = send_response_tx(&out_tx, &response).await;
                        });
                    }
                }
            }
            Message::Binary(data) => {
                let Some(mut state) = streaming_write.take() else {
                    let resp = WsResponse::error(
                        "",
                        WsErrorCode::Eproto,
                        "unexpected binary frame outside streaming write",
                    );
                    let _ = send_response_tx(&out_tx, &resp).await;
                    break;
                };

                let Some((stream_id, chunk)) = stream::decode_binary_frame(&data) else {
                    let resp = WsResponse::error(
                        &state.request_id,
                        WsErrorCode::Eproto,
                        "invalid binary frame",
                    );
                    abort_streaming_write(state).await;
                    let _ = send_response_tx(&out_tx, &resp).await;
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
                    abort_streaming_write(state).await;
                    let _ = send_response_tx(&out_tx, &resp).await;
                    break;
                }

                let next_size = state.bytes_written.saturating_add(chunk.len() as u64);
                if next_size > MAX_BYTES_PER_FILE as u64 {
                    let resp = WsResponse::error(
                        &state.request_id,
                        WsErrorCode::Efbig,
                        format!(
                            "file too large: {} bytes exceeds limit {}",
                            next_size, MAX_BYTES_PER_FILE
                        ),
                    );
                    abort_streaming_write(state).await;
                    let _ = send_response_tx(&out_tx, &resp).await;
                    break;
                }

                if let Err(err) = state.writer.write_chunk(chunk).await {
                    let (code, message) = map_fs_error(&err);
                    let resp = WsResponse::error(&state.request_id, code, message);
                    abort_streaming_write(state).await;
                    let _ = send_response_tx(&out_tx, &resp).await;
                    break;
                }

                state.bytes_written = next_size;
                state.hasher.update(chunk);
                streaming_write = Some(state);
            }
            Message::Ping(payload) => {
                if out_tx.send(Message::Pong(payload)).await.is_err() {
                    break;
                }
            }
            Message::Pong(_) => {}
            Message::Close(_) => break,
            Message::Frame(_) => {}
        }

    }

    if let Some(state) = streaming_write.take() {
        abort_streaming_write(state).await;
    }

    writer.abort();
    info!("fs9 ws connection closed from {peer_addr}");
    Ok(())
}

async fn abort_streaming_write(state: StreamingWriteState) {
    let _ = state.writer.abort().await;
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

async fn send_message_tx(out_tx: &mpsc::Sender<Message>, msg: Message) -> Result<(), WsIoError> {
    out_tx
        .send(msg)
        .await
        .map_err(|_| WsIoError::ConnectionClosed)
}

async fn send_response_tx(
    out_tx: &mpsc::Sender<Message>,
    response: &WsResponse,
) -> Result<(), WsIoError> {
    let payload = serde_json::to_string(response).map_err(|err| {
        WsIoError::Io(std::io::Error::other(format!(
            "failed to serialize websocket response: {err}"
        )))
    })?;
    send_message_tx(out_tx, Message::Text(payload)).await
}

pub(crate) fn tenant_from_keyspace(keyspace: &str) -> String {
    keyspace
        .strip_prefix(KEYSPACE_PREFIX)
        .unwrap_or(keyspace)
        .to_string()
}

async fn handle_ws_read_tx(
    out_tx: &mpsc::Sender<Message>,
    session: &WsSession,
    id: &str,
    path: &str,
    offset: Option<u64>,
    length: Option<usize>,
    requested_streaming: bool,
) -> Result<(), WsIoError> {
    if let Err((code, msg)) = validate_path(path) {
        send_response_tx(out_tx, &WsResponse::error(id, code, msg)).await?;
        return Ok(());
    }

    let file_info = match session.backend.stat(path).await {
        Ok(info) => info,
        Err(err) => {
            let (code, msg) = map_fs_error(&err);
            send_response_tx(out_tx, &WsResponse::error(id, code, msg)).await?;
            return Ok(());
        }
    };

    if file_info.is_dir {
        send_response_tx(
            out_tx,
            &WsResponse::error(id, WsErrorCode::Eisdir, format!("Is a directory: {path}")),
        )
        .await?;
        return Ok(());
    }

    if file_info.is_symlink {
        send_response_tx(
            out_tx,
            &WsResponse::error(
                id,
                WsErrorCode::Einval,
                format!("cannot read symlink as file; use readlink: {path}"),
            ),
        )
        .await?;
        return Ok(());
    }

    let actual_size = match compute_read_size(id, file_info.size, offset, length) {
        Ok(size) => size,
        Err(resp) => {
            send_response_tx(out_tx, &resp).await?;
            return Ok(());
        }
    };

    if actual_size > MAX_BYTES_PER_FILE as u64 {
        send_response_tx(
            out_tx,
            &WsResponse::error(
                id,
                WsErrorCode::Efbig,
                format!(
                    "file too large: {} bytes exceeds limit {}",
                    actual_size, MAX_BYTES_PER_FILE
                ),
            ),
        )
        .await?;
        return Ok(());
    }

    let should_stream = should_stream_read(requested_streaming, actual_size);
    if !should_stream || actual_size == 0 {
        let data = match (offset, length) {
            (Some(off), Some(len)) => session.backend.read_file_at(path, off, len).await,
            (None, None) => session.backend.read_file(path, MAX_BYTES_PER_FILE).await,
            _ => unreachable!("read size validation already checked offset/length pairing"),
        };
        let response = match data {
            Ok(data) => WsResponse::success(
                id,
                json!({
                    "content": STANDARD.encode(&data),
                    "size": data.len(),
                    "encoding": "base64"
                }),
            ),
            Err(err) => {
                let (code, msg) = map_fs_error(&err);
                WsResponse::error(id, code, msg)
            }
        };
        send_response_tx(out_tx, &response).await?;
        return Ok(());
    }

    let stream_id = stream::next_stream_id();
    let start = StreamStartResponse {
        streaming: true,
        stream_id,
        size: actual_size,
        chunk_size: DEFAULT_CHUNK_SIZE,
    };

    match (offset, length) {
        (None, None) => {
            let reader = match session
                .backend
                .read_file_stream(path, MAX_BYTES_PER_FILE)
                .await
            {
                Ok(reader) => reader,
                Err(err) => {
                    let (code, msg) = map_fs_error(&err);
                    send_response_tx(out_tx, &WsResponse::error(id, code, msg)).await?;
                    return Ok(());
                }
            };

            send_response_tx(
                out_tx,
                &WsResponse::success(
                    id,
                    serde_json::to_value(start).unwrap_or_else(|_| json!({})),
                ),
            )
            .await?;
            stream_whole_file_tx(out_tx, id, stream_id, actual_size, reader).await?;
        }
        (Some(off), Some(_)) => {
            send_response_tx(
                out_tx,
                &WsResponse::success(
                    id,
                    serde_json::to_value(start).unwrap_or_else(|_| json!({})),
                ),
            )
            .await?;
            stream_file_range_tx(out_tx, session, id, path, stream_id, off, actual_size).await?;
        }
        _ => unreachable!("read size validation already checked offset/length pairing"),
    }

    Ok(())
}

fn compute_read_size(
    id: &str,
    file_size: u64,
    offset: Option<u64>,
    length: Option<usize>,
) -> Result<u64, WsResponse> {
    match (offset, length) {
        (None, None) => Ok(file_size),
        (Some(off), Some(len)) => {
            if len == 0 || off >= file_size {
                return Ok(0);
            }

            let requested_len = u64::try_from(len)
                .map_err(|_| WsResponse::error(id, WsErrorCode::Einval, "length exceeds u64"))?;
            Ok(requested_len.min(file_size - off))
        }
        _ => Err(WsResponse::error(
            id,
            WsErrorCode::Einval,
            "offset and length must be provided together",
        )),
    }
}

fn should_stream_read(requested_streaming: bool, actual_size: u64) -> bool {
    requested_streaming || actual_size >= STREAMING_THRESHOLD as u64
}

async fn stream_whole_file_tx(
    out_tx: &mpsc::Sender<Message>,
    id: &str,
    stream_id: u64,
    expected_size: u64,
    mut reader: Box<dyn AsyncBufRead + Unpin + Send>,
) -> Result<(), WsIoError> {
    let mut sent = 0u64;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; DEFAULT_CHUNK_SIZE];

    loop {
        let read = reader
            .read(&mut chunk)
            .await
            .map_err(|err| ws_io_error(format!("stream read failed: {err}")))?;
        if read == 0 {
            break;
        }

        sent += read as u64;
        hasher.update(&chunk[..read]);
        send_message_tx(
            out_tx,
            Message::Binary(stream::encode_binary_frame(stream_id, &chunk[..read])),
        )
        .await?;
    }

    if sent != expected_size {
        return Err(ws_io_error(format!(
            "streamed size mismatch: expected {expected_size}, sent {sent}"
        )));
    }

    send_stream_end_tx(out_tx, id, stream_id, hasher).await
}

async fn stream_file_range_tx(
    out_tx: &mpsc::Sender<Message>,
    session: &WsSession,
    id: &str,
    path: &str,
    stream_id: u64,
    offset: u64,
    expected_size: u64,
) -> Result<(), WsIoError> {
    let mut current_offset = offset;
    let mut remaining = expected_size;
    let mut hasher = Sha256::new();

    while remaining > 0 {
        let chunk_len = usize::try_from(remaining.min(DEFAULT_CHUNK_SIZE as u64))
            .map_err(|_| ws_io_error("range stream chunk exceeds usize"))?;
        let chunk = session
            .backend
            .read_file_at(path, current_offset, chunk_len)
            .await
            .map_err(|err| ws_io_error(format!("range stream read failed: {err}")))?;
        if chunk.is_empty() {
            return Err(ws_io_error(format!(
                "range stream ended early at offset {current_offset}"
            )));
        }

        hasher.update(&chunk);
        send_message_tx(
            out_tx,
            Message::Binary(stream::encode_binary_frame(stream_id, &chunk)),
        )
        .await?;

        current_offset = current_offset
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| ws_io_error("range stream offset overflow"))?;
        remaining = remaining.saturating_sub(chunk.len() as u64);
    }

    send_stream_end_tx(out_tx, id, stream_id, hasher).await
}

async fn send_stream_end_tx(
    out_tx: &mpsc::Sender<Message>,
    id: &str,
    stream_id: u64,
    hasher: Sha256,
) -> Result<(), WsIoError> {
    let end = StreamEnd {
        stream: "end".to_string(),
        stream_id,
        checksum: Some(format!("sha256:{}", hex::encode(hasher.finalize()))),
    };
    let end_resp = WsResponse::success(id, serde_json::to_value(end).unwrap_or_else(|_| json!({})));
    send_response_tx(out_tx, &end_resp).await
}

fn ws_io_error(message: impl Into<String>) -> WsIoError {
    WsIoError::Io(std::io::Error::other(message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_read_size_full_file() {
        assert_eq!(compute_read_size("r1", 4096, None, None).unwrap(), 4096);
    }

    #[test]
    fn test_compute_read_size_range_trims_to_eof() {
        assert_eq!(
            compute_read_size("r2", 1024, Some(900), Some(512)).unwrap(),
            124
        );
    }

    #[test]
    fn test_compute_read_size_rejects_partial_range_spec() {
        let err = compute_read_size("r3", 1024, Some(0), None).unwrap_err();
        assert_eq!(err.error.unwrap().code, WsErrorCode::Einval);
    }

    #[test]
    fn test_should_stream_read_for_explicit_request() {
        assert!(should_stream_read(true, 1));
    }

    #[test]
    fn test_should_stream_read_for_large_file() {
        assert!(should_stream_read(false, STREAMING_THRESHOLD as u64));
        assert!(!should_stream_read(false, (STREAMING_THRESHOLD - 1) as u64));
    }

    #[tokio::test]
    async fn test_configure_ws_tcp_keepalive_applies() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _conn = TcpStream::connect(addr).await.unwrap();
        let (server_stream, _) = listener.accept().await.unwrap();
        configure_ws_tcp_keepalive(&server_stream, 60_000).expect("set keepalive");
        let sock = SockRef::from(&server_stream);
        assert!(sock.keepalive().unwrap(), "keepalive should be enabled");
    }

    // ── Integration tests for the ping / idle-deadline select loop ──
    //
    // These tests drive a real WebSocket connection and verify the core
    // contract introduced by this PR:
    //   - Server-initiated pings are emitted periodically
    //   - Pings / Pongs do NOT extend the idle deadline
    //   - Client Text frames DO extend the idle deadline
    //   - A silent client is disconnected after IDLE_TIMEOUT_SECS
    //
    // We use tokio paused time to avoid waiting 300 real seconds.

    use tokio_tungstenite::tungstenite::protocol::CloseFrame;

    /// Helper: spin up a minimal WebSocket "server loop" that mirrors the
    /// production select! logic (ping interval + idle deadline + try_send)
    /// without needing auth or TiKV.  Returns when the loop ends.
    async fn run_test_ws_loop(
        stream: tokio::net::TcpStream,
        idle_timeout_secs: u64,
        ping_interval_secs: u64,
    ) -> &'static str {
        let ws = accept_async(stream).await.expect("ws handshake");
        let (mut ws_sink, mut ws_source) = ws.split();

        let (out_tx, mut out_rx) = mpsc::channel::<Message>(64);
        let writer = tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if ws_sink.send(msg).await.is_err() {
                    break;
                }
            }
        });

        let mut ping_interval = tokio::time::interval(Duration::from_secs(ping_interval_secs));
        ping_interval.reset();

        let idle_deadline = tokio::time::sleep(Duration::from_secs(idle_timeout_secs));
        tokio::pin!(idle_deadline);

        let reason = loop {
            let msg = tokio::select! {
                biased;
                result = ws_source.next() => {
                    match result {
                        Some(Ok(msg)) => msg,
                        Some(Err(_)) => break "ws_error",
                        None => break "stream_ended",
                    }
                }
                _ = &mut idle_deadline => {
                    break "idle_timeout";
                }
                _ = ping_interval.tick() => {
                    let _ = out_tx.try_send(Message::Ping(vec![]));
                    continue;
                }
            };

            if matches!(msg, Message::Text(_) | Message::Binary(_)) {
                idle_deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + Duration::from_secs(idle_timeout_secs));
            } else if idle_deadline.is_elapsed() {
                break "idle_timeout_control_frame";
            }

            // Echo text back so the client can verify round-trips
            if let Message::Text(ref t) = msg {
                let _ = out_tx.send(Message::Text(t.clone())).await;
            }
            if let Message::Close(_) = msg {
                break "client_close";
            }
        };

        writer.abort();
        reason
    }

    /// Silent client (sends nothing after connect) → server disconnects
    /// after idle timeout, despite server pings being sent.
    #[tokio::test(start_paused = true)]
    async fn test_silent_client_gets_idle_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            run_test_ws_loop(stream, 300, 60).await
        });

        // Client connects but sends nothing
        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}"))
                .await
                .unwrap();

        // Drain incoming messages (server pings → auto-pong by tungstenite)
        let drain = tokio::spawn(async move {
            while let Some(Ok(_msg)) = client.next().await {}
        });

        let reason = server.await.unwrap();
        assert_eq!(reason, "idle_timeout", "silent client should hit idle timeout");
        drain.abort();
    }

    /// Active client (sends Text periodically) → deadline keeps resetting,
    /// connection survives past the original idle timeout.
    #[tokio::test(start_paused = true)]
    async fn test_active_client_resets_idle_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            run_test_ws_loop(stream, 300, 60).await
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}"))
                .await
                .unwrap();

        // Send a Text frame every 200s — should keep resetting the 300s deadline
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(200)).await;
            client.send(Message::Text("ping".into())).await.unwrap();
            // Read echo
            let _ = client.next().await;
        }
        // We've now been "alive" for 600s (3×200), well past the 300s idle timeout.
        // Close cleanly.
        client
            .send(Message::Close(Some(CloseFrame {
                code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal,
                reason: "done".into(),
            })))
            .await
            .unwrap();

        let reason = server.await.unwrap();
        assert_eq!(
            reason, "client_close",
            "active client should NOT hit idle timeout"
        );
    }

    /// Client that sends only Pong frames (simulating auto-reply to server
    /// pings) should still be disconnected after idle timeout.
    #[tokio::test(start_paused = true)]
    async fn test_pong_only_client_still_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            run_test_ws_loop(stream, 300, 60).await
        });

        let (mut client, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}"))
                .await
                .unwrap();

        // Send Pong frames every 50s.  The server should still disconnect
        // at 300s because Pong does not reset the idle deadline.
        for i in 0..8 {
            tokio::time::advance(Duration::from_secs(50)).await;
            // After 300s the server may have already closed, so ignore send errors
            if client.send(Message::Pong(vec![])).await.is_err() {
                break;
            }
        }

        let reason = server.await.unwrap();
        assert!(
            reason == "idle_timeout" || reason == "idle_timeout_control_frame",
            "pong-only client should hit idle timeout, got: {reason}"
        );
    }
}
