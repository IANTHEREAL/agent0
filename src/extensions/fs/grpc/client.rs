use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::BytesMut;
use futures_util::Stream;
use std::error::Error as StdError;
use tokio::io::AsyncBufRead;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;
use tracing::{debug, info, warn};

use super::proto;
use super::proto::fs_plane_client::FsPlaneClient;
use crate::extensions::fs::backend::{
    FsBackend, FsCreateUpload, FsFileInfo, FsMultipartCompletedPart, FsPreparedDownload,
    FsPresignedRequest, FsWriteStream, FsWriteStreamOptions,
};
use crate::extensions::fs::config::fs9_config;
use crate::extensions::fs::notify::{FsEventBuilder, FsEventType};
use crate::extensions::fs::FsError;

/// Create a gRPC channel to the fs9 proxy.
/// Does NOT cache — each GrpcFsBackend::new() creates a fresh connection.
/// The backend itself is cached per-tenant in the WS session / SQL statement cache.
pub(crate) async fn create_channel() -> Result<Channel> {
    let cfg = fs9_config();
    let addr = cfg.grpc_socket.clone();
    info!(addr = %addr, "connecting to fs9 proxy");

    let channel: Channel = if addr.starts_with('/') || addr.starts_with("unix://") {
        let socket_path = addr.strip_prefix("unix://").unwrap_or(&addr).to_string();
        Endpoint::try_from("http://[::]:50051")
            .map_err(|e| anyhow!("invalid endpoint: {e}"))?
            .connect_timeout(std::time::Duration::from_secs(10))
            .http2_keep_alive_interval(std::time::Duration::from_secs(10))
            .keep_alive_timeout(std::time::Duration::from_secs(5))
            .keep_alive_while_idle(true)
            .connect_with_connector(service_fn(move |_: Uri| {
                let path = socket_path.clone();
                async move {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        tokio::net::UnixStream::connect(path),
                    )
                    .await
                    .map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "unix socket connect timeout",
                        )
                    })?
                }
            }))
            .await
            .map_err(|e| anyhow!("connect to fs9 proxy (unix): {e}"))?
    } else {
        let uri = if addr.starts_with("http") {
            addr.clone()
        } else {
            format!("http://{addr}")
        };
        Endpoint::try_from(uri.clone())
            .map_err(|e| anyhow!("invalid endpoint {uri}: {e}"))?
            .connect_timeout(std::time::Duration::from_secs(10))
            .http2_keep_alive_interval(std::time::Duration::from_secs(10))
            .keep_alive_timeout(std::time::Duration::from_secs(5))
            .keep_alive_while_idle(true)
            .connect()
            .await
            .map_err(|e| anyhow!("connect to fs9 proxy (tcp {uri}): {e}"))?
    };

    Ok(channel)
}

/// GrpcFsBackend implements FsBackend by forwarding all operations to the fs9 gRPC proxy.
/// Holds an EventRing for post-mutation event emission, same pattern as EmbeddedPageFs.
pub(crate) struct GrpcFsBackend {
    client: FsPlaneClient<Channel>,
    volume_id: String,
    keyspace: String,
    notify_ring: std::sync::Arc<crate::extensions::fs::notify::EventRing>,
}

impl GrpcFsBackend {
    /// Creates a new GrpcFsBackend for the given keyspace.
    /// Connects to the fs9 proxy and calls InitVolume.
    pub(crate) async fn new(keyspace: &str) -> Result<Self> {
        let channel = create_channel().await?;
        // gRPC per-message size limit. Individual streaming chunks are 4 MB.
        let mut client = FsPlaneClient::new(channel)
            .max_decoding_message_size(128 * 1024 * 1024)
            .max_encoding_message_size(128 * 1024 * 1024);

        let cfg = fs9_config();
        // db9 keyspace "db9_tenant_xxxx" → JuiceFS keyspace "jfs_t_xxxx"
        let tenant_id = keyspace.strip_prefix("db9_tenant_").unwrap_or(keyspace);
        let jfs_keyspace = format!("jfs_t_{tenant_id}");
        let meta_url = format!(
            "tikv://{}?keyspace={}&gc-interval=0",
            cfg.grpc_pd_endpoints, jfs_keyspace,
        );

        debug!(keyspace, meta_url, "initializing fs9 volume");
        let resp = client
            .init_volume(proto::InitVolumeRequest {
                volume_id: jfs_keyspace.clone(),
                meta_url,
                cache_size_mb: 16,
            })
            .await
            .map_err(|e| {
                warn!(
                    keyspace,
                    code = ?e.code(),
                    message = %e.message(),
                    source = ?StdError::source(&e),
                    "InitVolume gRPC call failed"
                );
                anyhow!("InitVolume: {e}")
            })?;

        let created = resp.into_inner().created;
        if created {
            info!(keyspace, "fs9 volume initialized");
        } else {
            debug!(keyspace, "fs9 volume already active");
        }

        let notify_ring = crate::extensions::fs::notify::get_or_create_event_ring(keyspace);

        Ok(Self {
            client,
            volume_id: jfs_keyspace,
            keyspace: keyspace.to_string(),
            notify_ring,
        })
    }

    /// Emit an advisory fs event after a successful mutation.
    /// Same pattern as EmbeddedPageFs::emit_event — post-commit, at-most-once.
    fn emit_event(&self, builder: crate::extensions::fs::notify::FsEventBuilder) {
        crate::extensions::fs::notify::enqueue_persist_events(
            &self.keyspace,
            vec![builder.clone()],
        );
        let metrics = crate::extensions::fs::notify::notify_metrics_for_keyspace(&self.keyspace);
        let event_type = builder.event_type;
        match self.notify_ring.push(builder) {
            Ok(_) => metrics.record_emit(&event_type),
            Err(_) => metrics.record_emit_error(),
        }
    }

    /// Depth-first recursive remove with safety limits.
    /// Max depth 100, max total entries 50_000 — prevents runaway traversal.
    fn remove_recursive_inner<'a>(
        &'a self,
        path: &'a str,
        depth: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64>> + Send + 'a>> {
        const MAX_DEPTH: u32 = 100;
        const MAX_ENTRIES: u64 = 50_000;

        Box::pin(async move {
            if depth > MAX_DEPTH {
                return Err(anyhow!(FsError::InvalidInput(format!(
                    "remove_recursive: depth limit ({MAX_DEPTH}) exceeded at {path}"
                ))));
            }

            let info = match self.stat(path).await {
                Ok(info) => info,
                Err(e) => {
                    if e.downcast_ref::<FsError>()
                        .is_some_and(|fe| matches!(fe, FsError::NotFound(_)))
                    {
                        return Ok(0);
                    }
                    return Err(e);
                }
            };

            if !info.is_dir {
                self.remove(path).await?;
                return Ok(1);
            }

            let entries = self.readdir(path).await?;
            let mut count: u64 = 0;
            for entry in &entries {
                count += self.remove_recursive_inner(&entry.path, depth + 1).await?;
                if count > MAX_ENTRIES {
                    return Err(anyhow!(FsError::TooLarge(format!(
                        "remove_recursive: entry limit ({MAX_ENTRIES}) exceeded at {path}"
                    ))));
                }
            }
            self.remove(path).await?;
            count += 1;
            Ok(count)
        })
    }
}

/// Extract numeric errno from the terminal suffix of the proxy error message.
/// Format: "path: strerror [errno=N]" — the [errno=N] MUST be at the very end
/// of the message to avoid spoofing from paths containing bracket text.
fn parse_errno(message: &str) -> Option<i32> {
    let message = message.trim_end();
    if !message.ends_with(']') {
        return None;
    }
    let marker = "[errno=";
    let start = message.rfind(marker)? + marker.len();
    let end = message.len() - 1; // position of the closing ']'
    message[start..end].parse().ok()
}

/// Streaming write adapter — pipes chunks to fs9 over gRPC via a background task.
///
/// Commit is an explicit in-band signal (`WireMsg::Commit`), decoupled from channel
/// lifetime. Dropping the stream without committing cancels the in-flight RPC and
/// lets fs9 clean up the partial upload. This matches the staging-then-publish
/// semantics of the embedded backends, so drop is safe across all `FsWriteStream`
/// implementations.
///
/// Memory is bounded by `GRPC_WRITE_CHUNK_SIZE + channel_capacity × message_size`,
/// independent of caller slice size or file size: `write_chunk` streams the caller's
/// input directly into `GRPC_WRITE_CHUNK_SIZE` pieces and stages only a sub-chunk
/// tail remainder in `self.buf`.
const GRPC_WRITE_CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// Runtime protocol between `GrpcWriteStream` (producer) and its background task
/// (consumer). `Commit` is the only signal that lets the task close the gRPC stream
/// cleanly — any other termination (channel drop, producer error) drops the RPC
/// future and triggers HTTP/2 cancellation. The header is not a variant here: it's
/// static construction-time metadata that the task receives via its spawn closure.
enum WireMsg {
    Data(Vec<u8>),
    Commit,
}

struct GrpcWriteStream {
    tx: mpsc::Sender<WireMsg>,
    task: JoinHandle<Result<proto::WriteFileResponse>>,
    buf: BytesMut,
    path: String,
    keyspace: String,
    notify_ring: std::sync::Arc<crate::extensions::fs::notify::EventRing>,
}

impl GrpcWriteStream {
    fn new(
        mut client: FsPlaneClient<Channel>,
        volume_id: String,
        path: String,
        mode: u32,
        keyspace: String,
        notify_ring: std::sync::Arc<crate::extensions::fs::notify::EventRing>,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel::<WireMsg>(2);

        let header = proto::WriteFileHeader {
            volume_id,
            path: path.clone(),
            mode,
            umask: 0o022,
            create_only: false,
            flush: true,
        };

        let grpc_path = path.clone();
        let task = tokio::spawn(async move {
            // Inner proto channel feeds tonic's request stream. Keeping two channels
            // (outer `WireMsg`, inner `proto::WriteFileRequest`) lets us drive the RPC
            // future concurrently with `rx.recv()` via `select!` — so server-side errors
            // surface through the task's own `Result` instead of being masked by a
            // generic "channel closed" on the producer side.
            let (data_tx, data_rx) = mpsc::channel::<proto::WriteFileRequest>(2);
            let rpc_future =
                client.write_file(tokio_stream::wrappers::ReceiverStream::new(data_rx));
            tokio::pin!(rpc_future);

            // First frame on the gRPC stream is the header. This runs in the same async
            // context as `rpc_future`, so there is no cross-context race to worry about.
            // If `rpc_future` has already errored (transport init failure, etc.) the inner
            // send returns Err and the select loop below surfaces the real cause on its
            // first iteration.
            let _ = data_tx
                .send(proto::WriteFileRequest {
                    header: Some(header),
                    data: Vec::new(),
                })
                .await;

            let mut committed = false;
            loop {
                tokio::select! {
                    biased;
                    // Priority: if the RPC future resolves first, something server-side
                    // happened (early error, connection drop). Surface it as the task's
                    // Result so the producer's next send() failure maps to the real cause.
                    result = &mut rpc_future => {
                        return result
                            .map(|r| r.into_inner())
                            .map_err(|e| grpc_err(e, &grpc_path));
                    }
                    msg = rx.recv() => match msg {
                        Some(WireMsg::Data(data)) => {
                            if data_tx
                                .send(proto::WriteFileRequest { header: None, data })
                                .await
                                .is_err()
                            {
                                // data_rx dropped = rpc_future finished. Loop once more so
                                // the rpc_future arm of select! fires with the real error.
                                continue;
                            }
                        }
                        Some(WireMsg::Commit) => {
                            committed = true;
                            break;
                        }
                        None => {
                            // Producer dropped its sender without Commit — implicit abort.
                            break;
                        }
                    }
                }
            }

            if committed {
                // Explicit commit: close the inner stream cleanly so fs9 commits the file,
                // then await the response.
                drop(data_tx);
                (&mut rpc_future)
                    .await
                    .map(|r| r.into_inner())
                    .map_err(|e| grpc_err(e, &grpc_path))
            } else {
                // Implicit abort: leaving this scope drops `rpc_future` mid-request,
                // which sends HTTP/2 RST_STREAM; fs9 rolls back the partial upload.
                Err(anyhow!("gRPC write stream aborted before commit"))
            }
        });

        Self {
            tx,
            task,
            buf: BytesMut::with_capacity(GRPC_WRITE_CHUNK_SIZE),
            path,
            keyspace,
            notify_ring,
        }
    }

    /// Send a data chunk; on channel closure, drain the task's real error.
    async fn send_data(&mut self, data: Vec<u8>) -> Result<()> {
        if self.tx.send(WireMsg::Data(data)).await.is_err() {
            return Err(self.drain_task_err().await);
        }
        Ok(())
    }

    /// Called when a producer-side send fails (receiver dropped ⇒ task has ended
    /// or is about to). Awaits the task to surface the underlying `tonic::Status`
    /// instead of the generic "channel closed" message.
    async fn drain_task_err(&mut self) -> anyhow::Error {
        match (&mut self.task).await {
            Ok(Ok(_)) => anyhow!("gRPC write stream closed before commit was sent"),
            Ok(Err(e)) => e,
            Err(e) if e.is_cancelled() => anyhow!("gRPC write task cancelled"),
            Err(e) => anyhow!("gRPC write task panicked: {e}"),
        }
    }
}

#[async_trait]
impl FsWriteStream for GrpcWriteStream {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        let mut rest = chunk;

        // If a sub-chunk tail was staged from the previous call, top it up to
        // GRPC_WRITE_CHUNK_SIZE before streaming new full chunks.
        if !self.buf.is_empty() {
            let need = GRPC_WRITE_CHUNK_SIZE - self.buf.len();
            let take = rest.len().min(need);
            self.buf.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.buf.len() == GRPC_WRITE_CHUNK_SIZE {
                let data = self.buf.split().freeze().to_vec();
                self.send_data(data).await?;
            }
        }

        // Stream full chunks directly from the caller's slice; memory stays bounded
        // regardless of how large `chunk` is.
        while rest.len() >= GRPC_WRITE_CHUNK_SIZE {
            let (head, tail) = rest.split_at(GRPC_WRITE_CHUNK_SIZE);
            self.send_data(head.to_vec()).await?;
            rest = tail;
        }

        // Stage the sub-chunk remainder for the next call.
        if !rest.is_empty() {
            self.buf.extend_from_slice(rest);
        }

        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> Result<usize> {
        // Flush any staged sub-chunk tail.
        if !self.buf.is_empty() {
            let data = self.buf.split().freeze().to_vec();
            self.send_data(data).await?;
        }

        // Explicit commit marker — only this lets the task close the RPC stream cleanly.
        if self.tx.send(WireMsg::Commit).await.is_err() {
            return Err(self.drain_task_err().await);
        }

        // Drop tx so the task's rx.recv() returns None after it processes Commit.
        drop(self.tx);

        let resp = match self.task.await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(e),
            Err(e) if e.is_cancelled() => return Err(anyhow!("gRPC write task cancelled")),
            Err(e) => return Err(anyhow!("gRPC write task panicked: {e}")),
        };

        // Emit advisory event — same pattern as GrpcFsBackend::write_file.
        let event = FsEventBuilder {
            event_type: FsEventType::Write,
            path: self.path.clone(),
            old_path: None,
            inode: 0,
            parent_inode: 0,
            generation: 0,
            is_dir: false,
            size: resp.new_size,
        };
        crate::extensions::fs::notify::enqueue_persist_events(&self.keyspace, vec![event.clone()]);
        let metrics = crate::extensions::fs::notify::notify_metrics_for_keyspace(&self.keyspace);
        match self.notify_ring.push(event) {
            Ok(_) => metrics.record_emit(&FsEventType::Write),
            Err(_) => metrics.record_emit_error(),
        }

        Ok(resp.new_size as usize)
    }

    async fn abort(self: Box<Self>) -> Result<()> {
        // Drop is the abort path. Dropping `self` drops `tx` — the task's `rx.recv()`
        // returns None without seeing Commit, and the scope exit drops `rpc_future`
        // mid-request, producing HTTP/2 RST_STREAM. fs9 rolls back the partial upload.
        //
        // The same drop path runs on panic or future cancellation, so callers that
        // forget to call `abort()` still get safe cleanup — commit requires an
        // explicit, deliberate Commit marker that can only originate from `finish()`.
        drop(self);
        Ok(())
    }
}

/// Adapter that wraps a tonic server-streaming ReadAt response as an AsyncRead.
/// Consumes chunks on-demand without buffering the entire file in memory.
struct GrpcReadStream {
    inner: tonic::Streaming<proto::ReadAtResponse>,
    buf: Vec<u8>,
    pos: usize,
}

impl GrpcReadStream {
    fn new(inner: tonic::Streaming<proto::ReadAtResponse>) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            pos: 0,
        }
    }
}

impl tokio::io::AsyncRead for GrpcReadStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // Serve from current buffer if available
        if self.pos < self.buf.len() {
            let n = buf.remaining().min(self.buf.len() - self.pos);
            buf.put_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            return std::task::Poll::Ready(Ok(()));
        }

        // Fetch next non-empty chunk from gRPC stream.
        // Skip empty chunks to avoid signaling false EOF (AsyncRead returns Ok(())
        // with 0 bytes written = EOF).
        loop {
            let msg = std::pin::Pin::new(&mut self.inner).poll_next(cx);
            match msg {
                std::task::Poll::Ready(Some(Ok(chunk))) => {
                    if chunk.data.is_empty() {
                        continue; // skip empty chunks, fetch next
                    }
                    self.buf = chunk.data;
                    self.pos = 0;
                    let n = buf.remaining().min(self.buf.len());
                    buf.put_slice(&self.buf[..n]);
                    self.pos = n;
                    return std::task::Poll::Ready(Ok(()));
                }
                std::task::Poll::Ready(Some(Err(e))) => {
                    return std::task::Poll::Ready(Err(std::io::Error::other(
                        e.message().to_string(),
                    )));
                }
                std::task::Poll::Ready(None) => return std::task::Poll::Ready(Ok(())), // real EOF
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

fn grpc_err(status: tonic::Status, context: &str) -> anyhow::Error {
    let msg = format!("{}: {}", context, status.message());

    // Use structured errno from proxy message "[errno=N]" for precise mapping.
    // Falls back to gRPC status code if errno is not present.
    if let Some(errno) = parse_errno(status.message()) {
        return match errno {
            2 => anyhow!(FsError::NotFound(msg)),           // ENOENT
            13 => anyhow!(FsError::PermissionDenied(msg)),  // EACCES
            17 => anyhow!(FsError::AlreadyExists(msg)),     // EEXIST
            20 => anyhow!(FsError::NotDirectory(msg)),      // ENOTDIR
            21 => anyhow!(FsError::IsDirectory(msg)),       // EISDIR
            22 => anyhow!(FsError::InvalidInput(msg)),      // EINVAL
            28 => anyhow!(FsError::TooLarge(msg)),          // ENOSPC
            39 => anyhow!(FsError::DirectoryNotEmpty(msg)), // ENOTEMPTY
            _ => anyhow!(FsError::Internal(msg)),
        };
    }

    // Fallback: map by gRPC status code when errno is not available.
    match status.code() {
        tonic::Code::NotFound => anyhow!(FsError::NotFound(msg)),
        tonic::Code::AlreadyExists => anyhow!(FsError::AlreadyExists(msg)),
        tonic::Code::PermissionDenied => anyhow!(FsError::PermissionDenied(msg)),
        tonic::Code::InvalidArgument => anyhow!(FsError::InvalidInput(msg)),
        tonic::Code::FailedPrecondition => anyhow!(FsError::InvalidInput(msg)),
        tonic::Code::ResourceExhausted => anyhow!(FsError::TooLarge(msg)),
        _ => anyhow!(FsError::Internal(msg)),
    }
}

#[async_trait]
impl FsBackend for GrpcFsBackend {
    async fn stat(&self, path: &str) -> Result<FsFileInfo> {
        let mut client = self.client.clone();
        let resp = client
            .stat(proto::StatRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
            })
            .await
            .map_err(|e| grpc_err(e, path))?
            .into_inner();

        Ok(FsFileInfo {
            path: path.to_string(),
            is_dir: resp.is_dir,
            is_symlink: false,
            size: resp.size,
            mode: resp.mode,
            generation: 0,
            mtime: (resp.mtime_ms.max(0) as u64) / 1000,
            storage: None,
            sealed: Some(false),
        })
    }

    async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>> {
        let mut client = self.client.clone();
        let resp = client
            .readdir(proto::ReaddirRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
            })
            .await
            .map_err(|e| grpc_err(e, path))?
            .into_inner();

        Ok(resp
            .entries
            .iter()
            .map(|e| {
                let child_path = if path == "/" {
                    format!("/{}", e.name)
                } else {
                    format!("{}/{}", path, e.name)
                };
                FsFileInfo {
                    path: child_path,
                    is_dir: e.is_dir,
                    is_symlink: false,
                    size: e.size,
                    mode: e.mode,
                    generation: 0,
                    mtime: (e.mtime_ms.max(0) as u64) / 1000,
                    storage: None,
                    sealed: Some(false),
                }
            })
            .collect())
    }

    async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
        if max_bytes == 0 {
            return Ok(Vec::new());
        }
        self.read_file_at(path, 0, max_bytes).await
    }

    async fn read_file_stream(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
        if max_bytes == 0 {
            return Ok(Box::new(std::io::Cursor::new(Vec::new())));
        }

        let mut client = self.client.clone();
        let grpc_stream = client
            .read_at(proto::ReadAtRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
                offset: 0,
                length: max_bytes as u64,
            })
            .await
            .map_err(|e| grpc_err(e, path))?
            .into_inner();

        Ok(Box::new(tokio::io::BufReader::new(GrpcReadStream::new(
            grpc_stream,
        ))))
    }

    async fn read_file_at(&self, path: &str, offset: u64, length: usize) -> Result<Vec<u8>> {
        let mut client = self.client.clone();
        let mut stream = client
            .read_at(proto::ReadAtRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
                offset,
                length: length as u64,
            })
            .await
            .map_err(|e| grpc_err(e, path))?
            .into_inner();

        let mut data = Vec::new();
        while let Some(chunk) = stream.message().await.map_err(|e| grpc_err(e, path))? {
            data.extend_from_slice(&chunk.data);
        }
        Ok(data)
    }

    async fn write_file(&self, path: &str, data: &[u8], mode: Option<u32>) -> Result<usize> {
        let mut client = self.client.clone();
        let header = proto::WriteFileHeader {
            volume_id: self.volume_id.clone(),
            path: path.to_string(),
            mode: mode.unwrap_or(0o644),
            umask: 0o022,
            create_only: false,
            flush: true,
        };

        const CHUNK_SIZE: usize = 4 * 1024 * 1024; // 4 MB per stream message
        let mut messages: Vec<proto::WriteFileRequest> = Vec::new();

        // First message carries header + first chunk of data
        let first_chunk_end = data.len().min(CHUNK_SIZE);
        messages.push(proto::WriteFileRequest {
            header: Some(header),
            data: data[..first_chunk_end].to_vec(),
        });

        // Remaining data in subsequent messages (no header)
        let mut offset = first_chunk_end;
        while offset < data.len() {
            let end = data.len().min(offset + CHUNK_SIZE);
            messages.push(proto::WriteFileRequest {
                header: None,
                data: data[offset..end].to_vec(),
            });
            offset = end;
        }

        let resp = client
            .write_file(tokio_stream::iter(messages))
            .await
            .map_err(|e| grpc_err(e, path))?
            .into_inner();

        self.emit_event(FsEventBuilder {
            event_type: FsEventType::Write,
            path: path.to_string(),
            old_path: None,
            inode: 0,
            parent_inode: 0,
            generation: 0,
            is_dir: false,
            size: resp.new_size,
        });
        Ok(resp.new_size as usize)
    }

    async fn begin_write_stream(
        &self,
        path: &str,
        opts: FsWriteStreamOptions,
    ) -> Result<Box<dyn FsWriteStream>> {
        Ok(Box::new(GrpcWriteStream::new(
            self.client.clone(),
            self.volume_id.clone(),
            path.to_string(),
            opts.mode.unwrap_or(0o644),
            self.keyspace.clone(),
            self.notify_ring.clone(),
        )))
    }

    async fn write_file_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize> {
        let mut client = self.client.clone();
        let resp = client
            .write_at(proto::WriteAtRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
                offset,
                data: data.to_vec(),
                flush: true,
            })
            .await
            .map_err(|e| grpc_err(e, path))?
            .into_inner();

        self.emit_event(FsEventBuilder {
            event_type: FsEventType::Write,
            path: path.to_string(),
            old_path: None,
            inode: 0,
            parent_inode: 0,
            generation: 0,
            is_dir: false,
            size: resp.new_size,
        });
        Ok(resp.bytes_written as usize)
    }

    async fn append_file(&self, path: &str, data: &[u8]) -> Result<usize> {
        let mut client = self.client.clone();
        let header = proto::AppendHeader {
            volume_id: self.volume_id.clone(),
            path: path.to_string(),
            flush: true,
        };

        const CHUNK_SIZE: usize = 4 * 1024 * 1024;
        let mut messages: Vec<proto::AppendRequest> = Vec::new();

        let first_chunk_end = data.len().min(CHUNK_SIZE);
        messages.push(proto::AppendRequest {
            header: Some(header),
            data: data[..first_chunk_end].to_vec(),
        });

        let mut offset = first_chunk_end;
        while offset < data.len() {
            let end = data.len().min(offset + CHUNK_SIZE);
            messages.push(proto::AppendRequest {
                header: None,
                data: data[offset..end].to_vec(),
            });
            offset = end;
        }

        let resp = client
            .append(tokio_stream::iter(messages))
            .await
            .map_err(|e| grpc_err(e, path))?
            .into_inner();

        self.emit_event(FsEventBuilder {
            event_type: FsEventType::Write,
            path: path.to_string(),
            old_path: None,
            inode: 0,
            parent_inode: 0,
            generation: 0,
            is_dir: false,
            size: resp.new_size,
        });
        Ok(resp.bytes_written as usize)
    }

    async fn truncate(&self, path: &str, size: u64) -> Result<()> {
        let mut client = self.client.clone();
        client
            .truncate(proto::TruncateRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
                size,
            })
            .await
            .map_err(|e| grpc_err(e, path))?;
        self.emit_event(FsEventBuilder {
            event_type: FsEventType::Write,
            path: path.to_string(),
            old_path: None,
            inode: 0,
            parent_inode: 0,
            generation: 0,
            is_dir: false,
            size,
        });
        Ok(())
    }

    async fn remove(&self, path: &str) -> Result<()> {
        let mut client = self.client.clone();
        client
            .delete(proto::DeleteRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
            })
            .await
            .map_err(|e| grpc_err(e, path))?;
        self.emit_event(FsEventBuilder {
            event_type: FsEventType::Delete,
            path: path.to_string(),
            old_path: None,
            inode: 0,
            parent_inode: 0,
            generation: 0,
            is_dir: false,
            size: 0,
        });
        Ok(())
    }

    async fn remove_recursive(&self, path: &str) -> Result<u64> {
        if path == "/" || path.is_empty() {
            return Err(anyhow!(FsError::PermissionDenied(
                "cannot remove root".to_string()
            )));
        }
        self.remove_recursive_inner(path, 0).await
    }

    async fn mkdir(&self, path: &str, recursive: bool, mode: Option<u32>) -> Result<()> {
        let mut client = self.client.clone();
        client
            .mkdir(proto::MkdirRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
                mode: mode.unwrap_or(0o755),
                umask: 0o022,
                recursive,
            })
            .await
            .map_err(|e| grpc_err(e, path))?;
        self.emit_event(FsEventBuilder {
            event_type: FsEventType::Mkdir,
            path: path.to_string(),
            old_path: None,
            inode: 0,
            parent_inode: 0,
            generation: 0,
            is_dir: true,
            size: 0,
        });
        Ok(())
    }

    async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        let mut client = self.client.clone();
        client
            .rename(proto::RenameRequest {
                volume_id: self.volume_id.clone(),
                old_path: old_path.to_string(),
                new_path: new_path.to_string(),
            })
            .await
            .map_err(|e| grpc_err(e, old_path))?;
        self.emit_event(FsEventBuilder {
            event_type: FsEventType::Rename,
            path: new_path.to_string(),
            old_path: Some(old_path.to_string()),
            inode: 0,
            parent_inode: 0,
            generation: 0,
            is_dir: false,
            size: 0,
        });
        Ok(())
    }

    async fn create_upload(
        &self,
        _path: &str,
        _expected_size: u64,
        _mode: Option<u32>,
        _checksum_algorithm: Option<&str>,
    ) -> Result<FsCreateUpload> {
        Err(anyhow!(FsError::Internal(
            "presigned upload not supported by gRPC backend".to_string()
        )))
    }

    async fn presign_upload_part(
        &self,
        _upload_token: &str,
        _part_number: i32,
        _checksum_crc32c: Option<&str>,
    ) -> Result<FsPresignedRequest> {
        Err(anyhow!(FsError::Internal(
            "presigned upload not supported by gRPC backend".to_string()
        )))
    }

    async fn complete_upload(
        &self,
        _upload_token: &str,
        _parts: Vec<FsMultipartCompletedPart>,
        _checksum: Option<[u8; 32]>,
    ) -> Result<usize> {
        Err(anyhow!(FsError::Internal(
            "presigned upload not supported by gRPC backend".to_string()
        )))
    }

    async fn abort_upload(&self, _upload_token: &str) -> Result<()> {
        Err(anyhow!(FsError::Internal(
            "presigned upload not supported by gRPC backend".to_string()
        )))
    }

    async fn prepare_download(&self, _path: &str) -> Result<FsPreparedDownload> {
        Err(anyhow!(FsError::Internal(
            "presigned download not supported by gRPC backend".to_string()
        )))
    }

    async fn symlink(&self, path: &str, target: &str) -> Result<()> {
        let mut client = self.client.clone();
        client
            .symlink(proto::SymlinkRequest {
                volume_id: self.volume_id.clone(),
                target: target.to_string(),
                link: path.to_string(),
            })
            .await
            .map_err(|e| grpc_err(e, path))?;
        self.emit_event(FsEventBuilder {
            event_type: FsEventType::Create,
            path: path.to_string(),
            old_path: None,
            inode: 0,
            parent_inode: 0,
            generation: 0,
            is_dir: false,
            size: 0,
        });
        Ok(())
    }

    async fn readlink(&self, path: &str) -> Result<String> {
        let mut client = self.client.clone();
        let resp = client
            .readlink(proto::ReadlinkRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
            })
            .await
            .map_err(|e| grpc_err(e, path))?
            .into_inner();
        Ok(resp.target)
    }

    async fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        let mut client = self.client.clone();
        client
            .chmod(proto::ChmodRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
                mode,
            })
            .await
            .map_err(|e| grpc_err(e, path))?;
        self.emit_event(FsEventBuilder {
            event_type: FsEventType::Write,
            path: path.to_string(),
            old_path: None,
            inode: 0,
            parent_inode: 0,
            generation: 0,
            is_dir: false,
            size: 0,
        });
        Ok(())
    }
}
