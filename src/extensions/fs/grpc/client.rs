use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::BytesMut;
use futures_util::{stream, Stream, StreamExt};
use std::error::Error as StdError;
use tokio::io::AsyncBufRead;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity, Uri};
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

    // Surface any TLS misconfiguration (`FS9_TLS=true` + missing/unreadable
    // cert, scheme mismatch, UDS-with-TLS) at the first gRPC connect
    // attempt. Fail-closed semantics apply only where TLS actually
    // matters — Embedded backends that never call create_channel aren't
    // affected by a stray `FS9_TLS=true`.
    let grpc_tls = cfg
        .grpc_tls
        .as_ref()
        .map_err(|e| anyhow!("fs9 mTLS misconfigured: {e}"))?
        .as_ref();

    let channel: Channel = if addr.starts_with('/') || addr.starts_with("unix://") {
        // UDS path — always plaintext (pod-local, no wire to protect).
        // Config parse rejects FS9_TLS=true + UDS up-front (fs9's server
        // shares one grpc.Server across UDS and TCP, so TLS-on would
        // require UDS clients to present a cert too), so reaching this
        // branch implies grpc_tls is None.
        info!(addr = %addr, tls_mode = "plaintext", "connecting to fs9 proxy");
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
        // TCP path — apply mTLS when Fs9GrpcTls is configured.
        // https scheme when TLS on, http otherwise (tonic routes on scheme).
        // Config parse rejects http:// + TLS-on up-front, so the
        // starts_with("http") short-circuit cannot produce a silent
        // plaintext URI while TLS config is attached.
        let tls_mode = if grpc_tls.is_some() {
            "mTLS"
        } else {
            "plaintext"
        };
        info!(addr = %addr, tls_mode, "connecting to fs9 proxy");
        let scheme = if grpc_tls.is_some() { "https" } else { "http" };
        let uri = if addr.to_ascii_lowercase().starts_with("http") {
            addr.clone()
        } else {
            format!("{scheme}://{addr}")
        };
        // h2 flow control windows. Tonic/h2 default (64 KiB/stream, 64 KiB/conn)
        // was a non-issue while fs9 ran as a same-pod UDS sidecar (zero RTT made
        // WINDOW_UPDATE frames effectively free) but caps single-stream
        // throughput at ~64 KiB/RTT once fs9 moved cross-pod on TCP+mTLS. Raise
        // to 8 MiB/stream + 16 MiB/connection — these values are *initial*; h2's
        // BDP auto-tuner grows them further under measured load. A stalled
        // stream holds at most window_size of pre-acked bytes, so peak memory
        // per connection is bounded by the connection window, not unbounded
        // fan-in.
        //
        // Not env-configurable: round-3 DATA-CHALLENGER showed tx_queue peaks
        // at 0.21% of the window during real uploads, meaning the window is
        // never the binding clamp post-raise. Exposing an env knob would
        // invite operators to over-tune a non-bottleneck.
        let mut endpoint = Endpoint::try_from(uri.clone())
            .map_err(|e| anyhow!("invalid endpoint {uri}: {e}"))?
            .connect_timeout(std::time::Duration::from_secs(10))
            .http2_keep_alive_interval(std::time::Duration::from_secs(10))
            .keep_alive_timeout(std::time::Duration::from_secs(5))
            .keep_alive_while_idle(true)
            .initial_stream_window_size(8 * 1024 * 1024)
            .initial_connection_window_size(16 * 1024 * 1024);

        if let Some(tls) = grpc_tls {
            // Mirrors vendor/tikv-client/src/common/security.rs::tls_channel
            // — same ClientTlsConfig shape that already works against TiKV.
            let ca_pem = std::fs::read(&tls.ca_path)
                .map_err(|e| anyhow!("read fs9 CA {}: {e}", tls.ca_path))?;
            let cert_pem = std::fs::read(&tls.cert_path)
                .map_err(|e| anyhow!("read fs9 cert {}: {e}", tls.cert_path))?;
            let key_pem = std::fs::read(&tls.key_path)
                .map_err(|e| anyhow!("read fs9 key {}: {e}", tls.key_path))?;

            let tls_config = ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(ca_pem))
                .identity(Identity::from_pem(cert_pem, key_pem))
                .domain_name(tls.server_name.clone());
            endpoint = endpoint
                .tls_config(tls_config)
                .map_err(|e| anyhow!("fs9 tls config: {e}"))?;
            info!(
                server_name = %tls.server_name,
                "fs9 gRPC client mTLS configured"
            );
        }

        endpoint
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

/// Default chunk size for streaming multipart writes.
///
/// The gRPC `WritePartRequest` payload ceiling is 4 MiB; `WriteParts` is designed
/// around that cadence. Individual `write_chunk` calls stage a sub-chunk tail in
/// `self.buf` to keep wire frames aligned to this size.
const GRPC_WRITE_CHUNK_SIZE: usize = 4 * 1024 * 1024;

/// Low-level adapter over the fs9 `BeginWrite → WriteParts → CommitWrite` multipart
/// protocol.
///
/// Two usage modes on top of the same upload id:
/// 1. Inline (caller knows all bytes): `begin` → `put_all_from_iter(chunks)` → `commit`.
/// 2. Producer/consumer (streaming caller): `begin` → `open_stream()` pushes parts
///    through an `mpsc`-backed client-streaming RPC → `close()` the stream handle →
///    `commit`.
///
/// Safety: Drop fire-and-forges an `AbortWrite` on the runtime when the upload was
/// neither committed nor explicitly aborted. The server's staging TTL is the real
/// backstop — the spawn is best-effort cleanup only.
struct GrpcMultipartUpload {
    client: FsPlaneClient<Channel>,
    upload_id: String,
    path: String,
    bytes_sent: u64,
    committed_or_aborted: bool,
    /// Drop-bomb guard (must be last field). Marks the upload as properly
    /// consumed via `commit` or `abort` — catches direct Drops that bypass
    /// the single-entry consumer `write_chunks_then_terminate`.
    guard: crate::extensions::fs::termination_guard::TerminationGuard,
}

impl GrpcMultipartUpload {
    async fn begin(
        mut client: FsPlaneClient<Channel>,
        volume_id: String,
        path: String,
        file_mode: u32,
        umask: u32,
        write_mode: proto::WriteMode,
        expected_size: Option<u64>,
    ) -> Result<Self> {
        let resp = client
            .begin_write(proto::BeginWriteRequest {
                volume_id,
                path: path.clone(),
                mode: write_mode as i32,
                file_mode,
                umask,
                expected_size,
            })
            .await
            .map_err(|e| grpc_err(e, &path))?
            .into_inner();

        Ok(Self {
            client,
            upload_id: resp.upload_id,
            path,
            bytes_sent: 0,
            committed_or_aborted: false,
            guard: crate::extensions::fs::termination_guard::TerminationGuard::new(
                "GrpcMultipartUpload",
            ),
        })
    }

    /// Send all chunks in one client-streaming RPC. Accumulates `bytes_sent` eagerly
    /// as the stream is built; on error we return early and Drop cleans up.
    ///
    /// Note: `bytes_sent` counts bytes we *enqueued* — on mid-stream failure the
    /// client side may overcount relative to server `bytes_received`. We don't retry
    /// in v1, so this is only used to report a failure and abort.
    async fn put_all_from_iter<I>(&mut self, chunks: I) -> Result<()>
    where
        I: IntoIterator<Item = Vec<u8>>,
        I::IntoIter: Send + 'static,
    {
        // Build a lazy iterator of WritePartRequest. `bytes_sent` tracks the
        // rolling offset the server expects; each part's `offset` must equal the
        // server's cumulative `bytes_received`, which for a single strict-in-order
        // stream is the same as our own cumulative `bytes_sent` *before* the part.
        let upload_id = self.upload_id.clone();
        let mut offset = self.bytes_sent;
        let requests = chunks.into_iter().map(move |data| {
            let this_offset = offset;
            offset = offset.saturating_add(data.len() as u64);
            proto::WritePartRequest {
                upload_id: upload_id.clone(),
                offset: this_offset,
                data,
            }
        });

        // Collect the iterator AND accumulate bytes_sent for caller visibility.
        // tokio_stream::iter is eager over the iterator but doesn't require Vec;
        // we map through a plain iterator adapter. The caller's `chunks` iterator
        // decides how much memory is resident at any point.
        //
        // We still need to know `bytes_sent` after the RPC completes. The simplest
        // correct choice: peek the server's ACK. We update self.bytes_sent from
        // the returned WritePartsResponse.bytes_received (authoritative).
        let resp = self
            .client
            .write_parts(tokio_stream::iter(requests))
            .await
            .map_err(|e| grpc_err(e, &self.path))?
            .into_inner();
        self.bytes_sent = resp.bytes_received;
        Ok(())
    }

    /// Open a live `WriteParts` client-streaming RPC backed by an mpsc channel.
    /// Caller pushes parts through the returned handle, then calls `close()` to
    /// finish the stream and get the authoritative `bytes_received` from the ACK.
    fn open_stream(&self) -> GrpcMultipartUploadStream {
        // MPSC_CAPACITY = 8 × GRPC_WRITE_CHUNK_SIZE (4 MiB) = 32 MiB per stream.
        //
        // Previous value (2) was a manual mirror of h2's legacy 64 KiB default
        // window, translated into our 4 MiB chunks — i.e. 8 MiB ≈ 2 chunks. But
        // h2's BDP auto-tuner and our own raised initial window already own the
        // wire-side flow control on client-streaming uploads. The mpsc was the
        // REMAINING producer-side clamp. Live measurements (round 3 DATA-
        // CHALLENGER): tx_queue peaked at 0.21% of the 8 MiB h2 window during a
        // 10 GiB upload, and fs9's `object_request_uploading` sat at mean 3 /
        // max 5 against a 200-slot pool (98% S3 slack). fs9 BufferSize usage
        // held at 8 MiB / 128 MiB. The only clamp with no slack was this
        // channel — raising it unblocks the producer; downstream auto-scales.
        //
        // Multi-tenant memory bound:
        //   per-stream peak = 8 × 4 MiB = 32 MiB (transient, only when the
        //   consumer stalls; at steady state h2/BDP keeps it near-empty).
        //   per-tenant peak = DEFAULT_WS_MAX_INFLIGHT_UPLOADS_PER_CONNECTION
        //                     (16 today) × 32 MiB = 512 MiB under adversarial
        //                     stall. N tenants × 512 MiB is the pod budget the
        //                     operator sizes against ws_max_inflight_uploads.
        //
        // Do NOT raise past 8 without first raising or justifying the
        // ws_max_inflight_uploads cap — they compose multiplicatively.
        const MPSC_CAPACITY: usize = 8;
        let (tx, rx) = mpsc::channel::<proto::WritePartRequest>(MPSC_CAPACITY);
        let mut client = self.client.clone();
        let path = self.path.clone();
        let task: JoinHandle<Result<u64>> = tokio::spawn(async move {
            let resp = client
                .write_parts(ReceiverStream::new(rx))
                .await
                .map_err(|e| grpc_err(e, &path))?
                .into_inner();
            Ok(resp.bytes_received)
        });

        GrpcMultipartUploadStream {
            tx: Some(tx),
            task,
            upload_id: self.upload_id.clone(),
            path: self.path.clone(),
            cursor: self.bytes_sent,
        }
    }

    async fn commit(mut self) -> Result<u64> {
        // Mark terminated BEFORE the RPC: if commit_write succeeds server-side
        // but the response is dropped (network blip) and we return Err, Drop
        // must not spawn an AbortWrite against an already-committed upload.
        // Mirrors the "commit intent before RPC" pattern in `abort`.
        self.committed_or_aborted = true;
        self.guard.mark_terminated();
        let resp = self
            .client
            .commit_write(proto::CommitWriteRequest {
                upload_id: self.upload_id.clone(),
                total_size: self.bytes_sent,
            })
            .await
            .map_err(|e| grpc_err(e, &self.path))?
            .into_inner();
        Ok(resp.new_size)
    }

    /// Explicit, ordered, observable abort. INTERNAL — the only external
    /// consumer (`write_chunks_then_terminate`) and `GrpcWriteStream::terminate`
    /// call this via the single-entry `write_chunks_then_terminate` / the
    /// terminate(Err) branch. Direct callers must not exist — they reintroduce
    /// the commit-vs-abort oscillation that `terminate` was built to prevent.
    async fn abort(mut self) -> Result<()> {
        if self.upload_id.is_empty() {
            self.committed_or_aborted = true;
            self.guard.mark_terminated();
            return Ok(());
        }
        // Commit intent before the RPC: move upload_id out and mark aborted.
        // If the RPC fails (or panics), Drop sees an empty upload_id /
        // committed_or_aborted = true and skips the retry spawn.
        let upload_id = std::mem::take(&mut self.upload_id);
        self.committed_or_aborted = true;
        self.guard.mark_terminated();
        self.client
            .abort_write(proto::AbortWriteRequest { upload_id })
            .await
            .map_err(|e| grpc_err(e, &self.path))?;
        Ok(())
    }

    /// Single-entry consumer: push all chunks, then commit on success or
    /// abort on error — ordered wire sequence, no caller-side choice
    /// between commit and abort. This is the ONLY external consumer;
    /// `commit` and `abort` are internal implementation details reached
    /// via this method.
    async fn write_chunks_then_terminate<I>(mut self, chunks: I) -> Result<u64>
    where
        I: IntoIterator<Item = Vec<u8>>,
        I::IntoIter: Send + 'static,
    {
        match self.put_all_from_iter(chunks).await {
            Ok(()) => self.commit().await,
            Err(caller_err) => {
                let path = self.path.clone();
                if let Err(cleanup_err) = self.abort().await {
                    warn!(
                        path = %path,
                        error = %cleanup_err,
                        "AbortWrite failed after caller error; TTL reclaims"
                    );
                }
                Err(caller_err)
            }
        }
    }
}

impl Drop for GrpcMultipartUpload {
    fn drop(&mut self) {
        if self.committed_or_aborted || self.upload_id.is_empty() {
            return;
        }
        // Advisory backstop — reached only on panic unwind or runtime
        // shutdown, since the ONLY external consumer is
        // `write_chunks_then_terminate` which always invokes commit or abort
        // through a single ordered entry. Server TTL reclaims otherwise.
        let mut client = self.client.clone();
        let upload_id = std::mem::take(&mut self.upload_id);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let _ = client
                        .abort_write(proto::AbortWriteRequest { upload_id })
                        .await;
                });
            }
            Err(_) => {
                // No runtime (e.g. drop during teardown). TTL will reap.
                warn!(
                    upload_id = %upload_id,
                    "GrpcMultipartUpload dropped outside tokio runtime; \
                     relying on fs9 staging TTL to reclaim orphaned upload"
                );
            }
        }
    }
}

/// Live handle over a `WriteParts` client-streaming RPC. Each `push_part` sends a
/// single `WritePartRequest`; `close` drops the sender (so the server sees EOS),
/// awaits the joined task, and returns the authoritative `bytes_received`.
struct GrpcMultipartUploadStream {
    tx: Option<mpsc::Sender<proto::WritePartRequest>>,
    task: JoinHandle<Result<u64>>,
    upload_id: String,
    path: String,
    /// Rolling cumulative offset consumed so far — MUST match the server's
    /// `bytes_received` for the next `WritePartRequest.offset`.
    cursor: u64,
}

impl GrpcMultipartUploadStream {
    async fn push_part(&mut self, data: Vec<u8>) -> Result<()> {
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| anyhow!("gRPC write stream already closed"))?;
        let len = data.len() as u64;
        let req = proto::WritePartRequest {
            upload_id: self.upload_id.clone(),
            offset: self.cursor,
            data,
        };
        // Instrument mpsc send latency as the primary `mpsc_bound` signal:
        // a non-trivial send_seconds p99 with free workers means the channel
        // is backpressuring the writer against fs9's WriteParts consumer.
        // A separate queue-depth gauge would be last-writer-wins across
        // concurrent uploads sharing the same series, so the histogram
        // owns the backpressure signal on its own.
        let send_start = std::time::Instant::now();
        let send_result = tx.send(req).await;
        ::metrics::histogram!(
            "db9_upload_mpsc_send_seconds",
            "channel" => "fs9_write_parts",
        )
        .record(send_start.elapsed().as_secs_f64());
        if send_result.is_err() {
            return Err(self.drain_task_err().await);
        }
        self.cursor = self.cursor.saturating_add(len);
        Ok(())
    }

    /// Close the sender and await the RPC task. Returns the server's authoritative
    /// cumulative `bytes_received`.
    async fn close(mut self) -> Result<u64> {
        // Dropping the sender signals EOS to the server's request stream.
        drop(self.tx.take());
        match self.task.await {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(e)) => Err(e),
            Err(e) if e.is_cancelled() => Err(anyhow!("gRPC write task cancelled")),
            Err(e) => Err(anyhow!("gRPC write task panicked: {e}")),
        }
    }

    async fn drain_task_err(&mut self) -> anyhow::Error {
        // Drop sender first so the task can settle.
        drop(self.tx.take());
        // SAFETY: after this, self.task is moved out via take. But we're called
        // from &mut self — move via mem::replace with a sentinel.
        // We only need the error path; replace with a never-resolving dummy task.
        let path = self.path.clone();
        let fake: JoinHandle<Result<u64>> =
            tokio::spawn(async move { Err(anyhow!("replaced: {path}")) });
        let real = std::mem::replace(&mut self.task, fake);
        match real.await {
            Ok(Ok(_)) => anyhow!("gRPC write stream closed before close() was called"),
            Ok(Err(e)) => e,
            Err(e) if e.is_cancelled() => anyhow!("gRPC write task cancelled"),
            Err(e) => anyhow!("gRPC write task panicked: {e}"),
        }
    }
}

/// Streaming `FsWriteStream` adapter — a thin shell over `GrpcMultipartUpload` plus
/// a live `WriteParts` stream handle. Aligns caller chunks to 4 MiB wire frames via
/// `self.buf` staging tail and only keeps one in-flight chunk plus the mpsc channel
/// buffer in memory at a time.
struct GrpcWriteStream {
    upload: Option<GrpcMultipartUpload>,
    stream: Option<GrpcMultipartUploadStream>,
    buf: BytesMut,
    path: String,
    keyspace: String,
    notify_ring: std::sync::Arc<crate::extensions::fs::notify::EventRing>,
    /// Drop-bomb guard. Must be the LAST field (field drop order runs the
    /// host's advisory cleanup before the guard's debug_assert). See
    /// `termination_guard` module for the invariant.
    guard: crate::extensions::fs::termination_guard::TerminationGuard,
}

impl GrpcWriteStream {
    fn new(
        upload: GrpcMultipartUpload,
        path: String,
        keyspace: String,
        notify_ring: std::sync::Arc<crate::extensions::fs::notify::EventRing>,
    ) -> Self {
        let stream = upload.open_stream();
        Self {
            upload: Some(upload),
            stream: Some(stream),
            buf: BytesMut::with_capacity(GRPC_WRITE_CHUNK_SIZE),
            path,
            keyspace,
            notify_ring,
            guard: crate::extensions::fs::termination_guard::TerminationGuard::new(
                "GrpcWriteStream",
            ),
        }
    }

    async fn push(&mut self, data: Vec<u8>) -> Result<()> {
        let len = data.len() as u64;
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| anyhow!("gRPC write stream already closed"))?;
        stream.push_part(data).await?;
        if let Some(upload) = self.upload.as_mut() {
            upload.bytes_sent = upload.bytes_sent.saturating_add(len);
        }
        Ok(())
    }
}

#[async_trait]
impl FsWriteStream for GrpcWriteStream {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        let mut rest = chunk;

        // Top up any staged sub-chunk tail to GRPC_WRITE_CHUNK_SIZE before
        // streaming fresh full chunks from the caller's slice.
        if !self.buf.is_empty() {
            let need = GRPC_WRITE_CHUNK_SIZE - self.buf.len();
            let take = rest.len().min(need);
            self.buf.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.buf.len() == GRPC_WRITE_CHUNK_SIZE {
                let data = self.buf.split().freeze().to_vec();
                self.push(data).await?;
            }
        }

        // Stream full chunks directly from the caller's slice — memory stays
        // bounded regardless of caller slice size.
        while rest.len() >= GRPC_WRITE_CHUNK_SIZE {
            let (head, tail) = rest.split_at(GRPC_WRITE_CHUNK_SIZE);
            self.push(head.to_vec()).await?;
            rest = tail;
        }

        if !rest.is_empty() {
            self.buf.extend_from_slice(rest);
        }

        Ok(())
    }

    async fn terminate(mut self: Box<Self>, outcome: Result<()>) -> Result<usize> {
        // Drop-bomb flag MUST be set synchronously before any .await so a
        // cancelled terminate future does not trip the debug_assert.
        self.guard.mark_terminated();

        match outcome {
            Err(caller_err) => {
                // Cancellation-safe close-then-abort.
                //
                // Both fields are taken synchronously, then the ordered wire
                // sequence runs on a detached task. If the caller's future
                // is cancelled (e.g. wrapped in `tokio::time::timeout`),
                // dropping the returned JoinHandle does NOT cancel the
                // spawned task (tokio semantics) — so close-then-abort still
                // reaches the server in order. Server's staging TTL is the
                // authoritative backstop.
                let stream = self.stream.take();
                let upload = self.upload.take();
                let Ok(handle) = tokio::runtime::Handle::try_current() else {
                    // No runtime: per-field Drops log + TTL reclaims.
                    // Caller's error still propagates.
                    return Err(caller_err);
                };
                let join = handle.spawn(async move {
                    if let Some(s) = stream {
                        let _ = s.close().await;
                    }
                    if let Some(u) = upload {
                        let _ = u.abort().await;
                    }
                });
                match join.await {
                    Ok(()) => {}
                    Err(e) if e.is_cancelled() => {
                        // is_cancelled() here means the tokio runtime itself
                        // is shutting down and dropped the spawned task
                        // before it finished — NOT that a caller timeout
                        // elapsed (a caller timeout drops the outer future
                        // at `join.await` and the spawned task keeps
                        // running on the runtime). TTL reclaims in both
                        // cases.
                        debug!("GrpcWriteStream::terminate cleanup cancelled by runtime shutdown");
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            "GrpcWriteStream::terminate cleanup task panicked; TTL reclaims"
                        );
                    }
                }
                // Caller's error is always authoritative on the Err arm.
                Err(caller_err)
            }
            Ok(()) => {
                // NOT cancel-safe — callers must not wrap this in a timeout
                // (see trait doc). Flush + close + commit run in the caller's
                // future; cancellation mid-sequence leaves fields partially
                // taken, which Drop handles via the advisory spawn.
                if !self.buf.is_empty() {
                    let data = self.buf.split().freeze().to_vec();
                    self.push(data).await?;
                }

                let stream = self
                    .stream
                    .take()
                    .ok_or_else(|| anyhow!("gRPC write stream already closed"))?;
                let bytes_received = stream.close().await?;

                let mut upload = self
                    .upload
                    .take()
                    .ok_or_else(|| anyhow!("gRPC upload state missing"))?;
                upload.bytes_sent = bytes_received;
                let new_size = upload.commit().await?;

                let event = FsEventBuilder {
                    event_type: FsEventType::Write,
                    path: self.path.clone(),
                    old_path: None,
                    inode: 0,
                    parent_inode: 0,
                    generation: 0,
                    is_dir: false,
                    size: new_size,
                };
                crate::extensions::fs::notify::enqueue_persist_events(
                    &self.keyspace,
                    vec![event.clone()],
                );
                let metrics =
                    crate::extensions::fs::notify::notify_metrics_for_keyspace(&self.keyspace);
                match self.notify_ring.push(event) {
                    Ok(_) => metrics.record_emit(&FsEventType::Write),
                    Err(_) => metrics.record_emit_error(),
                }

                Ok(new_size as usize)
            }
        }
    }
}

impl Drop for GrpcWriteStream {
    fn drop(&mut self) {
        // Host-side Drop: emit context-rich warn when terminate was forgotten,
        // then run advisory cleanup. The debug_assert lives in the guard's
        // Drop, which runs AFTER this (guard is declared last).
        if !self.guard.is_terminated() && !std::thread::panicking() {
            warn!(
                path = %self.path,
                "GrpcWriteStream dropped without terminate(); \
                 advisory cleanup spawned, fs9 staging TTL reclaims"
            );
        }
        // Advisory cleanup runs whenever fields are still present — true
        // for both forgot-terminate AND terminate(Err) mid-cancellation.
        // Taking both fields moves ownership into ONE spawned future that
        // performs close-then-abort in wire order, avoiding the
        // field-drop-order race (AbortWrite arriving before WriteParts tail).
        let stream = self.stream.take();
        let upload = self.upload.take();
        if stream.is_none() && upload.is_none() {
            return;
        }

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let path = self.path.clone();
                handle.spawn(async move {
                    if let Some(stream) = stream {
                        if let Err(err) = stream.close().await {
                            debug!(path = %path, "GrpcWriteStream drop: stream close: {err:#}");
                        }
                    }
                    if let Some(upload) = upload {
                        if let Err(err) = upload.abort().await {
                            debug!(path = %path, "GrpcWriteStream drop: upload abort: {err:#}");
                        }
                    }
                });
            }
            Err(_) => {
                warn!(
                    path = %self.path,
                    "GrpcWriteStream dropped outside tokio runtime; \
                     relying on fs9 staging TTL to reclaim orphaned upload"
                );
                // stream and upload fall out of scope here. GrpcMultipartUpload::Drop
                // will also hit the no-runtime branch and log; the wire-order race
                // is irrelevant because no RPC will be sent from this thread anyway.
            }
        }
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

    /// Concurrent batch stat over the gRPC backend.
    ///
    /// The `FsBackend::batch_stat` default impl at backend.rs:183 is a plain
    /// sequential `for path in paths { stat(path).await }` loop — zero
    /// concurrency. The embedded backend has always used
    /// `batch_stat_concurrency` from config to parallelise its batch reads
    /// (embedded/pagefs/read_impl.rs:232); the gRPC backend never did,
    /// because this override was missing. That is the single largest
    /// contributor to the measured 200-file-stat regression vs the embedded
    /// baseline (~10 s observed here vs 306 ms on embedded prod) — every
    /// cold FUSE lookup batch unspooled serially against fs9.
    ///
    /// `buffered` (not `buffer_unordered`) preserves the Result-Vec order
    /// to match the input `paths` slice — several callers of batch_stat
    /// index the result by the same position as the input. Concurrency is
    /// capped at `batch_stat_concurrency` (default 16; config.rs:31).
    async fn batch_stat(&self, paths: &[String]) -> Result<Vec<Result<FsFileInfo>>> {
        let concurrency = fs9_config().batch_stat_concurrency.max(1);
        let results = stream::iter(paths.iter().cloned())
            .map(|p| async move { self.stat(&p).await })
            .buffered(concurrency)
            .collect::<Vec<_>>()
            .await;
        Ok(results)
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
        let file_mode = mode.unwrap_or(0o644);

        // Fast path: single PutFile RPC for payloads that fit inside one 4 MiB frame.
        if data.len() <= GRPC_WRITE_CHUNK_SIZE {
            let resp = self
                .client
                .clone()
                .put_file(proto::PutFileRequest {
                    volume_id: self.volume_id.clone(),
                    path: path.to_string(),
                    mode: proto::WriteMode::Replace as i32,
                    file_mode,
                    umask: 0o022,
                    data: data.to_vec(),
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
            return Ok(resp.new_size as usize);
        }

        // Slow path: BeginWrite → WriteParts (single client-streaming RPC) → CommitWrite.
        // `write_chunks_then_terminate` is the single consumer — no caller-side
        // choice between commit/abort; an error on WriteParts triggers ordered abort.
        let upload = GrpcMultipartUpload::begin(
            self.client.clone(),
            self.volume_id.clone(),
            path.to_string(),
            file_mode,
            0o022,
            proto::WriteMode::Replace,
            Some(data.len() as u64),
        )
        .await?;
        let chunks: Vec<Vec<u8>> = data
            .chunks(GRPC_WRITE_CHUNK_SIZE)
            .map(|c| c.to_vec())
            .collect();
        let new_size = upload.write_chunks_then_terminate(chunks).await?;

        self.emit_event(FsEventBuilder {
            event_type: FsEventType::Write,
            path: path.to_string(),
            old_path: None,
            inode: 0,
            parent_inode: 0,
            generation: 0,
            is_dir: false,
            size: new_size,
        });
        Ok(new_size as usize)
    }

    async fn begin_write_stream(
        &self,
        path: &str,
        opts: FsWriteStreamOptions,
    ) -> Result<Box<dyn FsWriteStream>> {
        let upload = GrpcMultipartUpload::begin(
            self.client.clone(),
            self.volume_id.clone(),
            path.to_string(),
            opts.mode.unwrap_or(0o644),
            0o022,
            proto::WriteMode::Replace,
            opts.expected_size,
        )
        .await?;
        Ok(Box::new(GrpcWriteStream::new(
            upload,
            path.to_string(),
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
        // Fast path: single PutFile RPC with Append mode for payloads that fit
        // inside one 4 MiB frame.
        if data.len() <= GRPC_WRITE_CHUNK_SIZE {
            let resp = self
                .client
                .clone()
                .put_file(proto::PutFileRequest {
                    volume_id: self.volume_id.clone(),
                    path: path.to_string(),
                    mode: proto::WriteMode::Append as i32,
                    file_mode: 0o644,
                    umask: 0o022,
                    data: data.to_vec(),
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
            // `bytes_written` is not exposed by PutFileResponse; the caller only
            // uses the return value to know "how many bytes did my append add,"
            // which for successful replace/append is `data.len()`.
            return Ok(data.len());
        }

        // Slow path: BeginWrite(Append) → WriteParts → CommitWrite.
        // Single-entry consumer — see `write_chunks_then_terminate`.
        let upload = GrpcMultipartUpload::begin(
            self.client.clone(),
            self.volume_id.clone(),
            path.to_string(),
            0o644,
            0o022,
            proto::WriteMode::Append,
            Some(data.len() as u64),
        )
        .await?;
        let chunks: Vec<Vec<u8>> = data
            .chunks(GRPC_WRITE_CHUNK_SIZE)
            .map(|c| c.to_vec())
            .collect();
        let new_size = upload.write_chunks_then_terminate(chunks).await?;

        self.emit_event(FsEventBuilder {
            event_type: FsEventType::Write,
            path: path.to_string(),
            old_path: None,
            inode: 0,
            parent_inode: 0,
            generation: 0,
            is_dir: false,
            size: new_size,
        });
        Ok(data.len())
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
