//! `GrpcFsBackend` — implements `FsBackend` by translating each method
//! into one or more `fsplane.v2.FsPlane` RPCs.
//!
//! # One backend per (tenant, role)
//!
//! db9-server's existing `acquire_statement_backend` caches a single
//! `Arc<dyn FsBackend>` per statement / per WS session. We instantiate a
//! fresh `GrpcFsBackend` for each *(tenant_id, role)* combination because:
//!
//! - the token cache is keyed by `(tenant_id, role)` — sharing a backend
//!   across roles would let a read-only session reuse an rw token;
//! - the volume_id (`jfs_t_<tid>`) is fixed per tenant and embedded on
//!   every request, so per-tenant backends keep that constant cheap;
//! - the underlying tonic Channel is process-shared (see
//!   `super::connector::shared_channel`) so per-backend creation is
//!   essentially a struct allocation.
//!
//! # Token injection
//!
//! tonic's interceptor surface is a synchronous `FnMut(Request) -> Request`,
//! so a `mint_or_reuse(...)` `.await` doesn't fit inside it. Instead,
//! each FsBackend method acquires a token first (cache hit is cheap;
//! miss does one HTTP exchange), then constructs the `tonic::Request`
//! manually and inserts the `authorization` metadata. The token-fetch
//! step is the only `.await` between caller intent and the RPC, so
//! statement timeouts remain crisp.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio::io::AsyncBufRead;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tracing::warn;

use crate::auth::fs_plane_token::{Auth9MintConfig, Fs9PlaneTokenCache};
use crate::extensions::fs::backend::{
    ensure_readable_as_regular_file, FsBackend, FsBatchWriteEntry, FsBatchWriteFile,
    FsCreateUpload, FsFileInfo, FsMultipartCompletedPart, FsPreparedDownload, FsPresignedRequest,
    FsWriteStream, FsWriteStreamOptions,
};
use crate::extensions::fs::embedded::types::EmbeddedFsError;
use crate::extensions::fs::grpc::errors::{fs_error_to_anyhow, status_to_anyhow};
use crate::extensions::fs::grpc::meta::file_meta_to_fs_file_info;
use crate::extensions::fs::grpc::proto::fs_plane_client::FsPlaneClient;
use crate::extensions::fs::grpc::proto::{
    abort_write_response, batch_stat_entry, batch_stat_response, begin_write_response,
    chmod_response, commit_write_response, delete_response, mkdir_response, put_file_response,
    read_at_response, readdir_response, readlink_response, rename_response, stat_response,
    symlink_response, truncate_response, write_at_response, write_parts_response,
    AbortWriteRequest, AbortWriteResponse, BatchStatRequest, BatchStatResponse, BeginWriteRequest,
    BeginWriteResponse, ChmodRequest, ChmodResponse, CommitWriteRequest, CommitWriteResponse,
    DeleteRequest, DeleteResponse, MkdirRequest, MkdirResponse, PutFileRequest, PutFileResponse,
    ReadAtRequest, ReadAtResponse, ReaddirRequest, ReaddirResponse, ReadlinkRequest,
    ReadlinkResponse, RenameRequest, RenameResponse, StatRequest, StatResponse, SymlinkRequest,
    SymlinkResponse, TruncateRequest, TruncateResponse, WriteAtRequest, WriteAtResponse, WriteMode,
    WritePartRequest, WritePartsResponse,
};
use crate::extensions::fs::termination_guard::TerminationGuard;

/// fs9's PutFile cap (proto §PutFileRequest).
const PUT_FILE_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Resolves a fresh `aud="fs-plane"` JWT each time it's called. The
/// implementation is owned by the GrpcFsBackend's constructor so the
/// backend itself is agnostic of how tokens are produced — the
/// production wiring is the `Auth9MintTokenProvider` below; tests
/// inject a static one.
#[async_trait]
pub(crate) trait TokenProvider: Send + Sync {
    async fn fs_plane_token(&self) -> Result<String>;
}

/// Production provider: asks auth9 `POST /v1/jwt/sign` for an
/// `aud="fs-plane"` token, through the process-wide cache. db9-server's
/// `X-API-Key` is the only authority needed — the customer's PG-session
/// authentication has already been validated by db9-server itself.
pub(crate) struct Auth9MintTokenProvider {
    cache: Arc<Fs9PlaneTokenCache>,
    cfg: Auth9MintConfig,
    tenant_id: String,
    role: String,
    access: crate::auth::fs_plane_token::Fs9Access,
}

impl Auth9MintTokenProvider {
    pub(crate) fn new(
        cache: Arc<Fs9PlaneTokenCache>,
        cfg: Auth9MintConfig,
        tenant_id: String,
        role: String,
        access: crate::auth::fs_plane_token::Fs9Access,
    ) -> Self {
        Self {
            cache,
            cfg,
            tenant_id,
            role,
            access,
        }
    }
}

#[async_trait]
impl TokenProvider for Auth9MintTokenProvider {
    async fn fs_plane_token(&self) -> Result<String> {
        let tok = crate::auth::fs_plane_token::mint_or_reuse(
            self.cache.as_ref(),
            &self.cfg,
            &self.tenant_id,
            &self.role,
            self.access,
        )
        .await?;
        Ok(tok.token.as_ref().to_string())
    }
}

/// Static-token provider — tests only.
#[cfg(test)]
pub(crate) struct StaticTokenProvider {
    token: String,
}

#[cfg(test)]
impl StaticTokenProvider {
    pub(crate) fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }
}

#[cfg(test)]
#[async_trait]
impl TokenProvider for StaticTokenProvider {
    async fn fs_plane_token(&self) -> Result<String> {
        Ok(self.token.clone())
    }
}

/// Backend instance. Cheap to clone (no inner Mutex, no per-call state);
/// reusable across an entire statement / WS session.
pub(crate) struct GrpcFsBackend {
    channel: Channel,
    volume_id: String,
    token_provider: Arc<dyn TokenProvider>,
}

impl GrpcFsBackend {
    pub(crate) fn new(
        channel: Channel,
        tenant_id: &str,
        token_provider: Arc<dyn TokenProvider>,
    ) -> Self {
        let volume_id = crate::extensions::fs::jfs_volume_id(tenant_id);
        Self {
            channel,
            volume_id,
            token_provider,
        }
    }

    /// Build a `tonic::Request` carrying the fs-plane bearer token in the
    /// `authorization` metadata. Single source of truth — every RPC
    /// method funnels through this so we can't accidentally ship a
    /// request without auth.
    async fn authorized<T>(&self, payload: T) -> Result<tonic::Request<T>> {
        authorize_request(&self.token_provider, payload).await
    }

    fn client(&self) -> FsPlaneClient<Channel> {
        fs_plane_client(self.channel.clone())
    }

    /// Single PutFile RPC. Caller picks `WriteMode::Replace` (full
    /// overwrite, returns server's post-write size) or `WriteMode::Append`
    /// (atomic append). Caller must ensure `data.len() <= PUT_FILE_MAX_BYTES`.
    async fn put_file_oneshot(
        &self,
        path: &str,
        data: &[u8],
        write_mode: WriteMode,
        file_mode: u32,
    ) -> Result<u64> {
        let req = self
            .authorized(PutFileRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
                mode: write_mode as i32,
                file_mode,
                umask: 0o022,
                data: data.to_vec(),
                sha256: None,
            })
            .await?;
        let resp: PutFileResponse = self
            .client()
            .put_file(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("put_file {path}")))?
            .into_inner();
        match resp.result {
            Some(put_file_response::Result::Meta(meta)) => Ok(meta.size),
            Some(put_file_response::Result::Error(err)) => {
                Err(fs_error_to_anyhow(err, &format!("put_file {path}")))
            }
            None => Err(anyhow!(EmbeddedFsError::Internal(format!(
                "fs9 put_file {path}: response envelope empty"
            )))),
        }
    }

    /// Slow-path write: `BeginWrite → WriteParts → CommitWrite/AbortWrite`
    /// over arbitrary-size `data`. Caller chunks at the 4 MiB
    /// `WritePartRequest.data` ceiling; the single-entry consumer
    /// commits on success and ordered-aborts on error. Returns server's
    /// post-write total size.
    async fn multipart_write_full(
        &self,
        path: &str,
        data: &[u8],
        write_mode: WriteMode,
        file_mode: u32,
    ) -> Result<u64> {
        let upload = GrpcMultipartUpload::begin(
            self.channel.clone(),
            self.token_provider.clone(),
            self.volume_id.clone(),
            path.to_string(),
            file_mode,
            0o022,
            write_mode,
            Some(data.len() as u64),
        )
        .await?;
        let chunks: Vec<Vec<u8>> = data
            .chunks(GRPC_WRITE_CHUNK_SIZE)
            .map(<[u8]>::to_vec)
            .collect();
        upload.write_chunks_then_terminate(chunks).await
    }

    /// Client-side DFS walker for `remove_recursive`. fs9 v2 has no
    /// server-side recursive delete. Boxed future because async recursion
    /// needs heap allocation for the call frame.
    fn remove_recursive_inner<'a>(
        &'a self,
        path: &'a str,
        depth: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64>> + Send + 'a>> {
        const MAX_DEPTH: u32 = 100;
        const MAX_ENTRIES: u64 = 50_000;

        Box::pin(async move {
            if depth > MAX_DEPTH {
                return Err(anyhow!(EmbeddedFsError::InvalidInput(format!(
                    "fs9 remove_recursive: depth limit ({MAX_DEPTH}) exceeded at {path}"
                ))));
            }

            let info = match self.stat(path).await {
                Ok(info) => info,
                Err(e) => {
                    if crate::extensions::fs::backend::is_not_found_error(&e) {
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
                    return Err(anyhow!(EmbeddedFsError::TooLarge(format!(
                        "fs9 remove_recursive: entry limit ({MAX_ENTRIES}) exceeded at {path}"
                    ))));
                }
            }
            self.remove(path).await?;
            count += 1;
            Ok(count)
        })
    }

    /// Raw `ReadAt` streaming. Bypasses the regular-file gate; only
    /// safe to call after the caller has already stat'd `path` and
    /// passed it through `ensure_readable_as_regular_file`. Public
    /// `read_file_at` (trait impl) does the gate; `read_file` shares
    /// this path so it doesn't pay for a second stat round-trip after
    /// the one its own size check already needs.
    async fn read_file_at_unchecked(
        &self,
        path: &str,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        let req = self
            .authorized(ReadAtRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
                offset,
                length: length as u64,
            })
            .await?;
        let mut stream: tonic::Streaming<ReadAtResponse> = self
            .client()
            .read_at(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("read_at {path}")))?
            .into_inner();

        let mut data = Vec::new();
        while let Some(frame) = stream
            .message()
            .await
            .map_err(|s| status_to_anyhow(s, &format!("read_at {path} stream")))?
        {
            match frame.frame {
                Some(read_at_response::Frame::Chunk(chunk)) => {
                    if data.len().saturating_add(chunk.data.len()) > length {
                        let want = length - data.len();
                        data.extend_from_slice(&chunk.data[..want]);
                        return Ok(data);
                    }
                    data.extend_from_slice(&chunk.data);
                }
                Some(read_at_response::Frame::Error(err)) => {
                    return Err(fs_error_to_anyhow(err, &format!("read_at {path}")));
                }
                None => {
                    return Err(anyhow!(EmbeddedFsError::Internal(format!(
                        "fs9 read_at {path}: stream frame envelope empty"
                    ))));
                }
            }
        }
        Ok(data)
    }
}

/// Single source of truth for the tonic message-size ceiling on both
/// directions. fs9 v2 chunks at 4 MiB, but the prost wire frame adds
/// envelope bytes on top, so a 4 MiB chunk arrives as ~4 MiB +
/// envelope. Tonic's default decode limit is 4 MiB; bumping to 16 MiB
/// gives generous headroom for future chunk-size tuning without
/// per-call adjustment.
fn fs_plane_client(channel: Channel) -> FsPlaneClient<Channel> {
    const MESSAGE_SIZE_CEILING: usize = 16 * 1024 * 1024;
    FsPlaneClient::new(channel)
        .max_decoding_message_size(MESSAGE_SIZE_CEILING)
        .max_encoding_message_size(MESSAGE_SIZE_CEILING)
}

/// Build a `tonic::Request<T>` carrying the fs-plane bearer in the
/// `authorization` metadata. Shared by `GrpcFsBackend` (unary RPCs) and
/// `GrpcMultipartUpload` (the streaming-write pipeline) so every RPC
/// the backend issues goes through one auth path.
async fn authorize_request<T>(
    provider: &Arc<dyn TokenProvider>,
    payload: T,
) -> Result<tonic::Request<T>> {
    let token = provider.fs_plane_token().await?;
    let value = MetadataValue::try_from(format!("Bearer {token}")).map_err(|e| {
        anyhow!(EmbeddedFsError::Internal(format!(
            "fs9: token contains invalid metadata bytes: {e}"
        )))
    })?;
    let mut req = tonic::Request::new(payload);
    req.metadata_mut().insert("authorization", value);
    Ok(req)
}

// ============================================================================
// Multipart upload pipeline
//
// Maps the trait's `write_file (>4 MiB) / append_file (>4 MiB) /
// begin_write_stream` surfaces onto the fs9 v2 `BeginWrite → WriteParts →
// CommitWrite / AbortWrite` protocol. Caller-visible chunk size is fixed at
// 4 MiB to match the v2 server's per-`WritePartRequest.data` ceiling.
//
// Termination discipline:
//   * The guard is marked terminated synchronously before every commit/abort
//     RPC, so a dropped future after a successful server commit never spawns
//     an `AbortWrite` against an already-published upload.
//   * Single external entry points: `write_chunks_then_terminate` for bulk
//     inline writes and `FsWriteStream::terminate` for the streaming adapter
//     — both pair commit/abort by outcome, eliminating any caller-side
//     abort-vs-commit choice.
//   * `Drop` fire-and-forgets one best-effort `AbortWrite`; fs9's staging TTL
//     is the authoritative backstop.
// ============================================================================

/// Per-`WritePartRequest.data` ceiling enforced by fs9 v2.
const GRPC_WRITE_CHUNK_SIZE: usize = PUT_FILE_MAX_BYTES;

/// Low-level adapter over `BeginWrite → WriteParts → CommitWrite/AbortWrite`.
///
/// Two usage modes:
/// 1. Inline (caller has all bytes): `begin → write_chunks_then_terminate`.
/// 2. Streaming (caller streams chunks): `begin → open_stream` then
///    `GrpcWriteStream` pushes through an mpsc-backed client-streaming RPC.
struct GrpcMultipartUpload {
    channel: Channel,
    token_provider: Arc<dyn TokenProvider>,
    volume_id: String,
    upload_id: String,
    path: String,
    bytes_sent: u64,
    /// Drop-bomb. MUST be the last field — see `termination_guard` module.
    /// Also serves as the commit/abort flag: `mark_terminated()` is called
    /// synchronously before every `commit_write` / `abort_write` RPC so the
    /// Drop backstop never spawns a duplicate `AbortWrite`.
    guard: TerminationGuard,
}

impl GrpcMultipartUpload {
    async fn begin(
        channel: Channel,
        token_provider: Arc<dyn TokenProvider>,
        volume_id: String,
        path: String,
        file_mode: u32,
        umask: u32,
        write_mode: WriteMode,
        expected_size: Option<u64>,
    ) -> Result<Self> {
        let req = authorize_request(
            &token_provider,
            BeginWriteRequest {
                volume_id: volume_id.clone(),
                path: path.clone(),
                mode: write_mode as i32,
                file_mode,
                umask,
                expected_size,
            },
        )
        .await?;
        let resp: BeginWriteResponse = fs_plane_client(channel.clone())
            .begin_write(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("begin_write {path}")))?
            .into_inner();
        let upload_id = match resp.result {
            Some(begin_write_response::Result::Success(s)) => s.upload_id,
            Some(begin_write_response::Result::Error(err)) => {
                return Err(fs_error_to_anyhow(err, &format!("begin_write {path}")));
            }
            None => {
                return Err(anyhow!(EmbeddedFsError::Internal(format!(
                    "fs9 begin_write {path}: response envelope empty"
                ))));
            }
        };
        Ok(Self {
            channel,
            token_provider,
            volume_id,
            upload_id,
            path,
            bytes_sent: 0,
            guard: TerminationGuard::new("GrpcMultipartUpload"),
        })
    }

    /// Build the iterator of `WritePartRequest`s for an inline write.
    ///
    /// fs9 v2 wants `volume_id` only on the FIRST message of the stream;
    /// subsequent messages may leave it empty (proto §WritePartRequest).
    /// We honour that — fewer wire bytes per part.
    fn build_part_requests(
        upload_id: String,
        volume_id: String,
        starting_offset: u64,
        chunks: Vec<Vec<u8>>,
    ) -> Vec<WritePartRequest> {
        let mut offset = starting_offset;
        let mut out = Vec::with_capacity(chunks.len());
        let mut first = true;
        for data in chunks {
            let this_offset = offset;
            offset = offset.saturating_add(data.len() as u64);
            out.push(WritePartRequest {
                volume_id: if first {
                    first = false;
                    volume_id.clone()
                } else {
                    String::new()
                },
                upload_id: upload_id.clone(),
                offset: this_offset,
                data,
            });
        }
        out
    }

    /// Send all chunks as a single client-streaming `WriteParts` RPC.
    /// `bytes_sent` is reset to the server's authoritative `bytes_received`.
    async fn put_all_chunks(&mut self, chunks: Vec<Vec<u8>>) -> Result<()> {
        let parts = Self::build_part_requests(
            self.upload_id.clone(),
            self.volume_id.clone(),
            self.bytes_sent,
            chunks,
        );
        let req = authorize_request(&self.token_provider, tokio_stream::iter(parts)).await?;
        let resp: WritePartsResponse = fs_plane_client(self.channel.clone())
            .write_parts(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("write_parts {}", self.path)))?
            .into_inner();
        match resp.result {
            Some(write_parts_response::Result::Success(s)) => {
                self.bytes_sent = s.bytes_received;
                Ok(())
            }
            Some(write_parts_response::Result::Error(err)) => Err(fs_error_to_anyhow(
                err,
                &format!("write_parts {}", self.path),
            )),
            None => Err(anyhow!(EmbeddedFsError::Internal(format!(
                "fs9 write_parts {}: response envelope empty",
                self.path
            )))),
        }
    }

    /// Open a live `WriteParts` client-streaming RPC backed by an mpsc
    /// channel. The caller pushes parts through the returned handle; the
    /// background task drives the RPC and returns the authoritative
    /// `bytes_received` on EOS.
    async fn open_stream(&self) -> Result<GrpcMultipartUploadStream> {
        // 8 × 4 MiB = 32 MiB per-stream transient ceiling. Tuned against
        // a 10 GiB upload: mean 3 / max 5 against fs9's 200-slot upload
        // pool, with h2's BDP auto-tuner carrying the wire-side flow
        // control. Raising past 8 needs `ws_max_inflight_uploads` raised
        // first — they compose multiplicatively.
        const MPSC_CAPACITY: usize = 8;
        let (tx, rx) = mpsc::channel::<WritePartRequest>(MPSC_CAPACITY);
        let token_provider = self.token_provider.clone();
        let channel = self.channel.clone();
        let path = self.path.clone();
        let req = authorize_request(&token_provider, ReceiverStream::new(rx)).await?;
        let task: JoinHandle<Result<u64>> = tokio::spawn(async move {
            let resp: WritePartsResponse = fs_plane_client(channel)
                .write_parts(req)
                .await
                .map_err(|s| status_to_anyhow(s, &format!("write_parts {path}")))?
                .into_inner();
            match resp.result {
                Some(write_parts_response::Result::Success(s)) => Ok(s.bytes_received),
                Some(write_parts_response::Result::Error(err)) => {
                    Err(fs_error_to_anyhow(err, &format!("write_parts {path}")))
                }
                None => Err(anyhow!(EmbeddedFsError::Internal(format!(
                    "fs9 write_parts {path}: response envelope empty"
                )))),
            }
        });
        Ok(GrpcMultipartUploadStream {
            tx: Some(tx),
            task,
            volume_id: self.volume_id.clone(),
            upload_id: self.upload_id.clone(),
            path: self.path.clone(),
            cursor: self.bytes_sent,
            first_part_sent: false,
        })
    }

    async fn commit(mut self) -> Result<u64> {
        // Mark terminated BEFORE the RPC: if commit_write succeeds
        // server-side but the response is dropped (network blip) and we
        // return Err, Drop must NOT spawn an AbortWrite against an
        // already-committed upload.
        self.guard.mark_terminated();
        let req = authorize_request(
            &self.token_provider,
            CommitWriteRequest {
                volume_id: self.volume_id.clone(),
                upload_id: self.upload_id.clone(),
                total_size: self.bytes_sent,
                sha256: None,
            },
        )
        .await?;
        let resp: CommitWriteResponse = fs_plane_client(self.channel.clone())
            .commit_write(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("commit_write {}", self.path)))?
            .into_inner();
        match resp.result {
            Some(commit_write_response::Result::Meta(meta)) => Ok(meta.size),
            Some(commit_write_response::Result::Error(err)) => Err(fs_error_to_anyhow(
                err,
                &format!("commit_write {}", self.path),
            )),
            None => Err(anyhow!(EmbeddedFsError::Internal(format!(
                "fs9 commit_write {}: response envelope empty",
                self.path
            )))),
        }
    }

    /// Explicit, ordered abort. INTERNAL — only `write_chunks_then_terminate`
    /// and `GrpcWriteStream::terminate(Err)` reach this; direct callers
    /// would reintroduce the commit-vs-abort oscillation `terminate` was
    /// built to prevent.
    async fn abort(mut self) -> Result<()> {
        if self.upload_id.is_empty() {
            self.guard.mark_terminated();
            return Ok(());
        }
        // Commit intent before the RPC: move upload_id out and mark the
        // guard terminated. If the RPC fails (or panics), Drop sees an
        // empty upload_id / terminated guard and skips the retry spawn.
        let upload_id = std::mem::take(&mut self.upload_id);
        self.guard.mark_terminated();
        let req = authorize_request(
            &self.token_provider,
            AbortWriteRequest {
                volume_id: self.volume_id.clone(),
                upload_id: upload_id.clone(),
            },
        )
        .await?;
        let resp: AbortWriteResponse = fs_plane_client(self.channel.clone())
            .abort_write(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("abort_write {}", self.path)))?
            .into_inner();
        match resp.result {
            Some(abort_write_response::Result::Success(_)) | None => Ok(()),
            Some(abort_write_response::Result::Error(err)) => Err(fs_error_to_anyhow(
                err,
                &format!("abort_write {}", self.path),
            )),
        }
    }

    /// Single-entry consumer for the inline path: send chunks, then commit
    /// on success or ordered-abort on error. The caller never picks
    /// commit-vs-abort.
    async fn write_chunks_then_terminate(mut self, chunks: Vec<Vec<u8>>) -> Result<u64> {
        match self.put_all_chunks(chunks).await {
            Ok(()) => self.commit().await,
            Err(caller_err) => {
                let path = self.path.clone();
                if let Err(cleanup_err) = self.abort().await {
                    warn!(
                        path = %path,
                        error = %cleanup_err,
                        "fs9 AbortWrite failed after caller error; staging TTL reclaims"
                    );
                }
                Err(caller_err)
            }
        }
    }
}

impl Drop for GrpcMultipartUpload {
    fn drop(&mut self) {
        if self.guard.is_terminated() || self.upload_id.is_empty() {
            return;
        }
        // Advisory backstop — reached only when the host stream is dropped
        // before the single-entry consumer ran (panic unwind, runtime
        // teardown). Server staging TTL is the authoritative cleanup.
        let channel = self.channel.clone();
        let token_provider = self.token_provider.clone();
        let volume_id = self.volume_id.clone();
        let upload_id = std::mem::take(&mut self.upload_id);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let req = match authorize_request(
                        &token_provider,
                        AbortWriteRequest {
                            volume_id,
                            upload_id,
                        },
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(_) => return,
                    };
                    let _ = fs_plane_client(channel).abort_write(req).await;
                });
            }
            Err(_) => {
                warn!(
                    "GrpcMultipartUpload dropped outside tokio runtime; \
                     relying on fs9 staging TTL to reclaim orphaned upload"
                );
            }
        }
    }
}

/// Live handle over a `WriteParts` client-streaming RPC. Each `push_part`
/// sends one `WritePartRequest`; `close` drops the sender (server sees EOS),
/// awaits the joined task, and returns the authoritative `bytes_received`.
struct GrpcMultipartUploadStream {
    tx: Option<mpsc::Sender<WritePartRequest>>,
    task: JoinHandle<Result<u64>>,
    volume_id: String,
    upload_id: String,
    path: String,
    cursor: u64,
    /// First WritePartRequest carries `volume_id`; subsequent leave it
    /// empty (proto §WritePartRequest).
    first_part_sent: bool,
}

impl GrpcMultipartUploadStream {
    async fn push_part(&mut self, data: Vec<u8>) -> Result<()> {
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| anyhow!("gRPC write stream already closed"))?;
        let len = data.len() as u64;
        let req = WritePartRequest {
            volume_id: if self.first_part_sent {
                String::new()
            } else {
                self.volume_id.clone()
            },
            upload_id: self.upload_id.clone(),
            offset: self.cursor,
            data,
        };
        let send_start = std::time::Instant::now();
        let send_result = tx.send(req).await;
        crate::metrics::record_upload_mpsc_send_latency(send_start.elapsed());
        if send_result.is_err() {
            return Err(self.drain_task_err().await);
        }
        self.first_part_sent = true;
        self.cursor = self.cursor.saturating_add(len);
        Ok(())
    }

    /// Drop the sender (EOS to the server's request stream) and await the
    /// RPC task. Returns the server's authoritative cumulative bytes.
    async fn close(mut self) -> Result<u64> {
        drop(self.tx.take());
        match self.task.await {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(e)) => Err(e),
            Err(e) if e.is_cancelled() => Err(anyhow!("gRPC write task cancelled")),
            Err(e) => Err(anyhow!("gRPC write task panicked: {e}")),
        }
    }

    async fn drain_task_err(&mut self) -> anyhow::Error {
        drop(self.tx.take());
        // We need to take ownership of self.task while keeping &mut self
        // valid; replace with a sentinel that resolves to an error.
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

/// Streaming `FsWriteStream` adapter: a thin shell over `GrpcMultipartUpload`
/// plus a live `WriteParts` stream handle. Caller chunks of any size are
/// aligned to 4 MiB wire frames via `buf`. Only one in-flight chunk plus the
/// mpsc capacity sits in memory at a time.
///
/// No explicit `Drop`: when the host's `terminate` is bypassed (panic unwind,
/// runtime cancellation), the inner `GrpcMultipartUpload`'s `Drop` spawns the
/// advisory `AbortWrite`; the stream and mpsc sender then field-drop in order.
struct GrpcWriteStream {
    upload: Option<GrpcMultipartUpload>,
    stream: Option<GrpcMultipartUploadStream>,
    /// Pending sub-chunk tail. On flush we `mem::replace` it with a fresh
    /// `Vec::with_capacity(GRPC_WRITE_CHUNK_SIZE)` and move the old buffer
    /// straight into the next `WritePartRequest.data` — no extra memcpy.
    buf: Vec<u8>,
    path: String,
    /// Drop-bomb. MUST be the last field — see `termination_guard` module.
    guard: TerminationGuard,
}

impl GrpcWriteStream {
    async fn new(upload: GrpcMultipartUpload, path: String) -> Result<Self> {
        // The `WriteParts` RPC stays unopened until the first chunk
        // pushes. Empty writes (`BeginWrite` → `terminate(Ok)` without
        // any data) skip `WriteParts` entirely and commit directly
        // with `total_size = 0` because fs9's `DoWriteParts` rejects
        // client streams that hit EOF before the first frame
        // (multipart_handler.go §"empty stream: first message
        // required to bind upload_id"). Without the laziness, every
        // zero-byte WS streaming write on a JuiceFS tenant would fail
        // even though inline `write_file` handles empties via
        // `PutFile`.
        Ok(Self {
            upload: Some(upload),
            stream: None,
            buf: Vec::with_capacity(GRPC_WRITE_CHUNK_SIZE),
            path,
            guard: TerminationGuard::new("GrpcWriteStream"),
        })
    }

    async fn push(&mut self, data: Vec<u8>) -> Result<()> {
        let len = data.len() as u64;
        // Lazy-open on first push so an empty write never starts the
        // `WriteParts` RPC.
        if self.stream.is_none() {
            let upload = self
                .upload
                .as_ref()
                .ok_or_else(|| anyhow!("gRPC multipart upload missing during lazy stream open"))?;
            let stream = upload.open_stream().await?;
            self.stream = Some(stream);
        }
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

        // Top up the staged tail to GRPC_WRITE_CHUNK_SIZE before streaming
        // fresh full chunks from the caller's slice.
        if !self.buf.is_empty() {
            let need = GRPC_WRITE_CHUNK_SIZE - self.buf.len();
            let take = rest.len().min(need);
            self.buf.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.buf.len() == GRPC_WRITE_CHUNK_SIZE {
                let data =
                    std::mem::replace(&mut self.buf, Vec::with_capacity(GRPC_WRITE_CHUNK_SIZE));
                self.push(data).await?;
            }
        }

        // Stream full chunks directly from the caller's slice — memory
        // stays bounded regardless of caller slice size.
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
                // Close the stream first so the server stops waiting on
                // the request stream, then abort the upload. Order matters
                // — calling abort while WriteParts is still in flight
                // races with the in-flight RPC.
                if let Some(stream) = self.stream.take() {
                    let _ = stream.close().await; // bytes_received not needed on the abort path
                }
                if let Some(upload) = self.upload.take() {
                    if let Err(cleanup_err) = upload.abort().await {
                        warn!(
                            path = %self.path,
                            error = %cleanup_err,
                            "fs9 AbortWrite failed after caller error; staging TTL reclaims"
                        );
                    }
                }
                Err(caller_err)
            }
            Ok(()) => {
                // Flush any staged sub-chunk tail. `push` lazy-opens
                // the `WriteParts` stream, so a non-empty tail also
                // opens the RPC. If nothing was ever pushed the
                // stream stays `None` and we commit a zero-byte file
                // directly — fs9's `DoCommitWrite` accepts
                // `total_size == BytesReceived == 0`.
                if !self.buf.is_empty() {
                    let data = std::mem::take(&mut self.buf);
                    self.push(data).await?;
                }
                let bytes = match self.stream.take() {
                    Some(stream) => stream.close().await?,
                    None => 0,
                };
                let mut upload = match self.upload.take() {
                    Some(u) => u,
                    None => return Err(anyhow!("gRPC multipart upload missing on terminate(Ok)")),
                };
                upload.bytes_sent = bytes;
                let new_size = upload.commit().await?;
                Ok(new_size as usize)
            }
        }
    }
}

// ============================================================================
// Streaming read adapter
//
// Adapts `tonic::Streaming<ReadAtResponse>` to `tokio::io::AsyncRead` so a
// large file download stays bounded at one chunk in memory instead of buffering
// the entire payload before the first byte goes to the caller. Wrap in
// `BufReader` to satisfy the `AsyncBufRead` trait the WS large-read path
// expects.
// ============================================================================
struct GrpcReadStream {
    inner: tonic::Streaming<ReadAtResponse>,
    /// Current chunk we're handing out to the caller; advances `pos` as
    /// each `poll_read` consumes part of it.
    buf: Vec<u8>,
    pos: usize,
}

impl GrpcReadStream {
    fn new(inner: tonic::Streaming<ReadAtResponse>) -> Self {
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
        use futures::Stream;

        // Drain whatever's still in the current chunk before pulling the
        // next wire frame.
        if self.pos < self.buf.len() {
            let n = buf.remaining().min(self.buf.len() - self.pos);
            buf.put_slice(&self.buf[self.pos..self.pos + n]);
            self.pos += n;
            return std::task::Poll::Ready(Ok(()));
        }

        // Pull next non-empty chunk. Empty chunks are skipped: `AsyncRead`
        // signals EOF with `Ok(())` and zero bytes written, so handing an
        // empty data frame straight back would lie about EOF.
        loop {
            match std::pin::Pin::new(&mut self.inner).poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(frame))) => match frame.frame {
                    Some(read_at_response::Frame::Chunk(chunk)) => {
                        if chunk.data.is_empty() {
                            continue;
                        }
                        self.buf = chunk.data;
                        self.pos = 0;
                        let n = buf.remaining().min(self.buf.len());
                        buf.put_slice(&self.buf[..n]);
                        self.pos = n;
                        return std::task::Poll::Ready(Ok(()));
                    }
                    Some(read_at_response::Frame::Error(err)) => {
                        return std::task::Poll::Ready(Err(std::io::Error::other(format!(
                            "fs9 read stream: {}",
                            err.message
                        ))));
                    }
                    None => {
                        return std::task::Poll::Ready(Err(std::io::Error::other(
                            "fs9 read stream: frame envelope empty",
                        )));
                    }
                },
                std::task::Poll::Ready(Some(Err(status))) => {
                    let msg = status.message().to_string();
                    return std::task::Poll::Ready(Err(std::io::Error::other(format!(
                        "fs9 read stream transport: {msg}"
                    ))));
                }
                std::task::Poll::Ready(None) => return std::task::Poll::Ready(Ok(())),
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
    }
}

#[async_trait]
impl FsBackend for GrpcFsBackend {
    fn backend_kind(&self) -> &'static str {
        "grpc"
    }

    // ---------------------------------------------------------------
    // Reads
    // ---------------------------------------------------------------
    async fn stat(&self, path: &str) -> Result<FsFileInfo> {
        let req = self
            .authorized(StatRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
            })
            .await?;
        let resp: StatResponse = self
            .client()
            .stat(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("stat {path}")))?
            .into_inner();
        match resp.result {
            Some(stat_response::Result::Meta(meta)) => {
                Ok(file_meta_to_fs_file_info(path.to_string(), meta))
            }
            Some(stat_response::Result::Error(err)) => {
                Err(fs_error_to_anyhow(err, &format!("stat {path}")))
            }
            None => Err(anyhow!(EmbeddedFsError::Internal(format!(
                "fs9 stat {path}: response envelope empty"
            )))),
        }
    }

    async fn batch_stat(&self, paths: &[String]) -> Result<Vec<Result<FsFileInfo>>> {
        // fs9 v2 has a native BatchStat that preserves order and bounds
        // server-side fan-out. Prefer it over the trait's serial default.
        let req = self
            .authorized(BatchStatRequest {
                volume_id: self.volume_id.clone(),
                paths: paths.to_vec(),
            })
            .await?;
        let resp: BatchStatResponse = self
            .client()
            .batch_stat(req)
            .await
            .map_err(|s| status_to_anyhow(s, "batch_stat"))?
            .into_inner();
        match resp.result {
            Some(batch_stat_response::Result::Payload(payload)) => {
                if payload.entries.len() != paths.len() {
                    return Err(anyhow!(EmbeddedFsError::Internal(format!(
                        "fs9 batch_stat: server returned {} entries for {} paths",
                        payload.entries.len(),
                        paths.len()
                    ))));
                }
                Ok(payload
                    .entries
                    .into_iter()
                    .zip(paths.iter())
                    .map(|(entry, path)| match entry.result {
                        Some(batch_stat_entry::Result::Meta(m)) => {
                            Ok(file_meta_to_fs_file_info(path.clone(), m))
                        }
                        Some(batch_stat_entry::Result::Error(e)) => {
                            Err(fs_error_to_anyhow(e, &format!("batch_stat {path}")))
                        }
                        None => Err(anyhow!(EmbeddedFsError::Internal(format!(
                            "fs9 batch_stat {path}: entry envelope empty"
                        )))),
                    })
                    .collect())
            }
            Some(batch_stat_response::Result::Error(err)) => {
                Err(fs_error_to_anyhow(err, "batch_stat"))
            }
            None => Err(anyhow!(EmbeddedFsError::Internal(
                "fs9 batch_stat: response envelope empty".to_string()
            ))),
        }
    }

    async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>> {
        let req = self
            .authorized(ReaddirRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
            })
            .await?;
        let resp: ReaddirResponse = self
            .client()
            .readdir(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("readdir {path}")))?
            .into_inner();
        match resp.result {
            Some(readdir_response::Result::Payload(payload)) => Ok(payload
                .entries
                .into_iter()
                .filter_map(|entry| {
                    // DirEntry has `name + Option<FileMeta>`. If meta is
                    // None the server is buggy; skip silently rather than
                    // surface a generation=0 sentinel that looks like a
                    // valid entry.
                    let meta = entry.meta?;
                    let child_path = if path == "/" {
                        format!("/{}", entry.name)
                    } else {
                        format!("{}/{}", path.trim_end_matches('/'), entry.name)
                    };
                    Some(file_meta_to_fs_file_info(child_path, meta))
                })
                .collect()),
            Some(readdir_response::Result::Error(err)) => {
                Err(fs_error_to_anyhow(err, &format!("readdir {path}")))
            }
            None => Err(anyhow!(EmbeddedFsError::Internal(format!(
                "fs9 readdir {path}: response envelope empty"
            )))),
        }
    }

    async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
        if max_bytes == 0 {
            return Ok(Vec::new());
        }
        // fs9 v2 `Stat` is `Lstat` while `ReadAt` follows symlinks, so
        // a stat-then-read sequence is not symlink-safe by itself. The
        // gate codifies the same "regular file or error" invariant
        // embedded `pagefs` enforces at every read entry point; reusing
        // the helper keeps the contract in one named place rather than
        // duplicated per read method.
        let info = self.stat(path).await?;
        ensure_readable_as_regular_file(&info, path)?;
        if info.size > max_bytes as u64 {
            return Err(anyhow!(EmbeddedFsError::TooLarge(format!(
                "fs9 read_file: {path} ({} bytes) exceeds max {max_bytes} bytes",
                info.size
            ))));
        }
        self.read_file_at_unchecked(path, 0, max_bytes).await
    }

    async fn read_file_stream(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
        if max_bytes == 0 {
            return Ok(Box::new(std::io::Cursor::new(Vec::new())));
        }
        // Same gate as `read_file` — fs9 v2 stat is Lstat, ReadAt
        // follows, so the symlink reject is what prevents a short
        // symlink from streaming a cap-sized prefix of a huge target.
        // CSV/TSV/JSON line decoders downstream expect "full file or
        // error" (`fs/file_stream.rs:69`, `fs/glob_stream.rs:186`,
        // `sql/executor/extensions.rs:747`).
        let info = self.stat(path).await?;
        ensure_readable_as_regular_file(&info, path)?;
        if info.size > max_bytes as u64 {
            return Err(anyhow!(EmbeddedFsError::TooLarge(format!(
                "fs9 read_file_stream: {path} ({} bytes) exceeds max {max_bytes} bytes",
                info.size
            ))));
        }
        let req = self
            .authorized(ReadAtRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
                offset: 0,
                length: max_bytes as u64,
            })
            .await?;
        let stream: tonic::Streaming<ReadAtResponse> = self
            .client()
            .read_at(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("read_file_stream {path}")))?
            .into_inner();
        Ok(Box::new(tokio::io::BufReader::new(GrpcReadStream::new(
            stream,
        ))))
    }

    async fn read_file_at(&self, path: &str, offset: u64, length: usize) -> Result<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        // Gate the read against fs9's Stat-is-Lstat semantics: without
        // this stat the call would silently read through a symlink to
        // a different target. Embedded enforces the same invariant
        // inside `plan_file_range_read`.
        let info = self.stat(path).await?;
        ensure_readable_as_regular_file(&info, path)?;
        self.read_file_at_unchecked(path, offset, length).await
    }

    // ---------------------------------------------------------------
    // Mutates
    // ---------------------------------------------------------------
    async fn remove(&self, path: &str) -> Result<()> {
        crate::extensions::fs::reject_root_path_op(path, "remove")?;
        let req = self
            .authorized(DeleteRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
            })
            .await?;
        let resp: DeleteResponse = self
            .client()
            .delete(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("delete {path}")))?
            .into_inner();
        match resp.result {
            Some(delete_response::Result::Success(_)) => Ok(()),
            Some(delete_response::Result::Error(err)) => {
                Err(fs_error_to_anyhow(err, &format!("delete {path}")))
            }
            None => Err(anyhow!(EmbeddedFsError::Internal(format!(
                "fs9 delete {path}: response envelope empty"
            )))),
        }
    }

    async fn remove_recursive(&self, path: &str) -> Result<u64> {
        crate::extensions::fs::reject_root_path_op(path, "remove")?;
        self.remove_recursive_inner(path, 0).await
    }

    async fn mkdir(&self, path: &str, recursive: bool, mode: Option<u32>) -> Result<()> {
        let req = self
            .authorized(MkdirRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
                mode: mode.unwrap_or(0o755),
                umask: 0o022,
                recursive,
            })
            .await?;
        let resp: MkdirResponse = self
            .client()
            .mkdir(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("mkdir {path}")))?
            .into_inner();
        match resp.result {
            Some(mkdir_response::Result::Meta(_)) => Ok(()),
            Some(mkdir_response::Result::Error(err)) => {
                Err(fs_error_to_anyhow(err, &format!("mkdir {path}")))
            }
            None => Err(anyhow!(EmbeddedFsError::Internal(format!(
                "fs9 mkdir {path}: response envelope empty"
            )))),
        }
    }

    async fn write_file(&self, path: &str, data: &[u8], mode: Option<u32>) -> Result<usize> {
        let file_mode = mode.unwrap_or(0o644);
        let new_size = if data.len() <= PUT_FILE_MAX_BYTES {
            self.put_file_oneshot(path, data, WriteMode::Replace, file_mode)
                .await?
        } else {
            self.multipart_write_full(path, data, WriteMode::Replace, file_mode)
                .await?
        };
        Ok(new_size as usize)
    }

    async fn begin_write_stream(
        &self,
        path: &str,
        opts: FsWriteStreamOptions,
    ) -> Result<Box<dyn FsWriteStream>> {
        let upload = GrpcMultipartUpload::begin(
            self.channel.clone(),
            self.token_provider.clone(),
            self.volume_id.clone(),
            path.to_string(),
            opts.mode.unwrap_or(0o644),
            0o022,
            WriteMode::Replace,
            opts.expected_size,
        )
        .await?;
        let stream = GrpcWriteStream::new(upload, path.to_string()).await?;
        Ok(Box::new(stream))
    }

    async fn write_file_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize> {
        // `WriteAt` is a unary RPC with no multipart variant, so the
        // request payload must fit a single gRPC message. Tonic's
        // ceiling here is 16 MiB (`MESSAGE_SIZE_CEILING`), but we keep
        // the public contract at 4 MiB so a future bump to the wire
        // ceiling doesn't silently change the per-call cap that SQL,
        // WS, and any other caller has been written against. Enforce
        // at the backend impl rather than at each call site — SQL's
        // `fs9_write_at` and WS `pwrite` already decode the payload
        // before calling here, and without this guard a >4 MiB WS
        // `pwrite` would allocate the full buffer, then fail inside
        // tonic with a confusing encode error instead of the explicit
        // ceiling diagnostic.
        if data.len() > crate::extensions::fs::MAX_BYTES_PER_OFFSET_WRITE {
            return Err(anyhow!(EmbeddedFsError::TooLarge(format!(
                "fs9 write_at: data ({} bytes) exceeds the per-call ceiling \
                 ({} bytes). WriteAt is a unary RPC; use fs9_write or \
                 fs9_append for larger payloads.",
                data.len(),
                crate::extensions::fs::MAX_BYTES_PER_OFFSET_WRITE
            ))));
        }
        let req = self
            .authorized(WriteAtRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
                offset,
                data: data.to_vec(),
                // v2 requires flush=true. We don't expose a buffered
                // mode to upstream callers; tightening to durability
                // matches embedded backend semantics.
                flush: true,
            })
            .await?;
        let resp: WriteAtResponse = self
            .client()
            .write_at(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("write_at {path}")))?
            .into_inner();
        match resp.result {
            Some(write_at_response::Result::Success(success)) => Ok(success.bytes_written as usize),
            Some(write_at_response::Result::Error(err)) => {
                Err(fs_error_to_anyhow(err, &format!("write_at {path}")))
            }
            None => Err(anyhow!(EmbeddedFsError::Internal(format!(
                "fs9 write_at {path}: response envelope empty"
            )))),
        }
    }

    async fn append_file(&self, path: &str, data: &[u8]) -> Result<usize> {
        // The trait contract is "bytes appended on success" — for a
        // whole-write semantic this equals `data.len()`, regardless of
        // whether we went through the unary fast path or multipart slow
        // path.
        if data.len() <= PUT_FILE_MAX_BYTES {
            self.put_file_oneshot(path, data, WriteMode::Append, 0o644)
                .await?;
        } else {
            self.multipart_write_full(path, data, WriteMode::Append, 0o644)
                .await?;
        }
        Ok(data.len())
    }

    async fn truncate(&self, path: &str, size: u64) -> Result<()> {
        let req = self
            .authorized(TruncateRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
                size,
            })
            .await?;
        let resp: TruncateResponse = self
            .client()
            .truncate(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("truncate {path}")))?
            .into_inner();
        match resp.result {
            Some(truncate_response::Result::Meta(_)) => Ok(()),
            Some(truncate_response::Result::Error(err)) => {
                Err(fs_error_to_anyhow(err, &format!("truncate {path}")))
            }
            None => Err(anyhow!(EmbeddedFsError::Internal(format!(
                "fs9 truncate {path}: response envelope empty"
            )))),
        }
    }

    async fn rename(&self, old_path: &str, new_path: &str) -> Result<()> {
        crate::extensions::fs::reject_root_path_op(old_path, "rename")?;
        let req = self
            .authorized(RenameRequest {
                volume_id: self.volume_id.clone(),
                old_path: old_path.to_string(),
                new_path: new_path.to_string(),
            })
            .await?;
        let resp: RenameResponse = self
            .client()
            .rename(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("rename {old_path} -> {new_path}")))?
            .into_inner();
        match resp.result {
            Some(rename_response::Result::Success(_)) => Ok(()),
            Some(rename_response::Result::Error(err)) => Err(fs_error_to_anyhow(
                err,
                &format!("rename {old_path} -> {new_path}"),
            )),
            None => Err(anyhow!(EmbeddedFsError::Internal(format!(
                "fs9 rename {old_path} -> {new_path}: response envelope empty"
            )))),
        }
    }

    // ---------------------------------------------------------------
    // Presigned uploads are not on db9-server's data path: fs9 issues
    // presigned S3 URLs directly to cli/FUSE clients. These stay stubbed.
    // ---------------------------------------------------------------
    async fn create_upload(
        &self,
        _path: &str,
        _expected_size: u64,
        _mode: Option<u32>,
        _checksum_algorithm: Option<&str>,
    ) -> Result<FsCreateUpload> {
        Err(anyhow!(EmbeddedFsError::Internal(
            "fs9 v2: presigned upload not yet wired on the gRPC backend".to_string()
        )))
    }

    async fn presign_upload_part(
        &self,
        _upload_token: &str,
        _part_number: i32,
        _checksum_crc32c: Option<&str>,
    ) -> Result<FsPresignedRequest> {
        Err(anyhow!(EmbeddedFsError::Internal(
            "fs9 v2: presigned upload not yet wired on the gRPC backend".to_string()
        )))
    }

    async fn complete_upload(
        &self,
        _upload_token: &str,
        _parts: Vec<FsMultipartCompletedPart>,
        _checksum: Option<[u8; 32]>,
    ) -> Result<usize> {
        Err(anyhow!(EmbeddedFsError::Internal(
            "fs9 v2: presigned upload not yet wired on the gRPC backend".to_string()
        )))
    }

    async fn abort_upload(&self, _upload_token: &str) -> Result<()> {
        Err(anyhow!(EmbeddedFsError::Internal(
            "fs9 v2: presigned upload not yet wired on the gRPC backend".to_string()
        )))
    }

    async fn prepare_download(&self, _path: &str) -> Result<FsPreparedDownload> {
        Err(anyhow!(EmbeddedFsError::Internal(
            "fs9 v2: presigned download not yet wired on the gRPC backend".to_string()
        )))
    }

    // ---------------------------------------------------------------
    // Metadata mutations
    // ---------------------------------------------------------------
    async fn symlink(&self, path: &str, target: &str) -> Result<()> {
        let req = self
            .authorized(SymlinkRequest {
                volume_id: self.volume_id.clone(),
                target: target.to_string(),
                link: path.to_string(),
            })
            .await?;
        let resp: SymlinkResponse = self
            .client()
            .symlink(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("symlink {path} -> {target}")))?
            .into_inner();
        match resp.result {
            Some(symlink_response::Result::Success(_)) => Ok(()),
            Some(symlink_response::Result::Error(err)) => Err(fs_error_to_anyhow(
                err,
                &format!("symlink {path} -> {target}"),
            )),
            None => Err(anyhow!(EmbeddedFsError::Internal(format!(
                "fs9 symlink {path}: response envelope empty"
            )))),
        }
    }

    async fn readlink(&self, path: &str) -> Result<String> {
        let req = self
            .authorized(ReadlinkRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
            })
            .await?;
        let resp: ReadlinkResponse = self
            .client()
            .readlink(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("readlink {path}")))?
            .into_inner();
        match resp.result {
            Some(readlink_response::Result::Success(success)) => Ok(success.target),
            Some(readlink_response::Result::Error(err)) => {
                Err(fs_error_to_anyhow(err, &format!("readlink {path}")))
            }
            None => Err(anyhow!(EmbeddedFsError::Internal(format!(
                "fs9 readlink {path}: response envelope empty"
            )))),
        }
    }

    async fn chmod(&self, path: &str, mode: u32) -> Result<()> {
        let req = self
            .authorized(ChmodRequest {
                volume_id: self.volume_id.clone(),
                path: path.to_string(),
                mode,
            })
            .await?;
        let resp: ChmodResponse = self
            .client()
            .chmod(req)
            .await
            .map_err(|s| status_to_anyhow(s, &format!("chmod {path}")))?
            .into_inner();
        match resp.result {
            Some(chmod_response::Result::Meta(_)) => Ok(()),
            Some(chmod_response::Result::Error(err)) => {
                Err(fs_error_to_anyhow(err, &format!("chmod {path}")))
            }
            None => Err(anyhow!(EmbeddedFsError::Internal(format!(
                "fs9 chmod {path}: response envelope empty"
            )))),
        }
    }

    // Overrides for the trait's default impls. batch_write is the only
    // one with a non-trivial default we want to KEEP letting flow through
    // to per-file write_file calls until the native BatchWrite RPC is
    // wired up — leaving it unimplemented here intentionally inherits
    // the trait default.
    #[allow(unused)]
    fn supports_batch_write_atomic(&self) -> bool {
        // The v2 server supports atomic batch writes only via the
        // multipart upload + S3 path; the trait-default fan-out via
        // per-file write_file is what we want until that path is wired
        // up. Leave at false so WS / SQL callers don't attempt the
        // grouped path against an unported backend.
        false
    }

    #[allow(unused)]
    fn supports_presigned(&self) -> bool {
        // Set to false until the presigned RPCs above are implemented.
        // WS streaming-uploads still work via begin_write_stream once
        // it's wired (slice 3c).
        false
    }

    /// Override to delegate to fs9's native batch_write RPC when ported
    /// (currently inherits the trait-default fan-out via write_file —
    /// which will error with not_yet until write_file is implemented).
    async fn batch_write(&self, files: Vec<FsBatchWriteFile>) -> Result<Vec<FsBatchWriteEntry>> {
        // Trait default fans out via write_file. write_file is stubbed
        // for now, so this method will produce per-entry not_yet errors.
        // Behaviour is preserved; once write_file is wired the trait
        // default does the right thing automatically.
        let mut entries = Vec::with_capacity(files.len());
        for file in files {
            let path = file.path.clone();
            let result = self.write_file(&path, &file.data, file.mode).await;
            entries.push(FsBatchWriteEntry {
                path,
                result,
                failure_category: None,
            });
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::fs::grpc::proto::FileMeta;

    /// Unit-test the metadata injection: build an authorized Request and
    /// assert the bearer landed in the `authorization` slot exactly once.
    #[tokio::test]
    async fn authorized_request_attaches_bearer_metadata() {
        // We need a Channel-shaped placeholder. tonic doesn't expose a
        // public "uninitialized channel" constructor; use a deferred
        // lazy_static is overkill. Skip building a full GrpcFsBackend —
        // exercise the helper directly via a stripped struct that holds
        // the token_provider only. The authorization-injection logic
        // doesn't touch the Channel.
        struct Stub {
            tp: Arc<dyn TokenProvider>,
        }
        impl Stub {
            async fn authorized<T>(&self, payload: T) -> Result<tonic::Request<T>> {
                let token = self.tp.fs_plane_token().await?;
                let value = MetadataValue::try_from(format!("Bearer {token}"))?;
                let mut req = tonic::Request::new(payload);
                req.metadata_mut().insert("authorization", value);
                Ok(req)
            }
        }
        let s = Stub {
            tp: Arc::new(StaticTokenProvider::new("hello.jwt.token")),
        };
        let req = s
            .authorized(StatRequest {
                volume_id: "jfs_t_x".into(),
                path: "/a".into(),
            })
            .await
            .expect("authorized should succeed");
        let auth = req
            .metadata()
            .get("authorization")
            .expect("authorization metadata must be present");
        assert_eq!(auth.to_str().unwrap(), "Bearer hello.jwt.token");
    }

    /// Sanity-check the stat envelope decoder. We can't run the real
    /// `stat()` without a tonic server fixture, so this exercises only
    /// the post-RPC decoding path that maps oneof → FsFileInfo / error.
    #[test]
    fn stat_response_meta_arm_decodes_to_file_info() {
        let meta = FileMeta {
            size: 13,
            mtime_ns: 2_000_000_000,
            mode: 0o644,
            is_dir: false,
            is_symlink: false,
            version: 0x1234,
        };
        let info = file_meta_to_fs_file_info("/a".to_string(), meta);
        assert_eq!(info.size, 13);
        assert_eq!(info.mtime, 2);
        assert_eq!(info.mode, 0o644);
        assert!(!info.is_dir);
    }

    /// `#[ignore]` so it never runs in CI. Manually invoked when a
    /// developer wants to verify the gRPC backend works against a live
    /// fs9 v2 instance. Reads endpoint / TLS / token from env so the
    /// secrets never live in the source tree.
    ///
    /// To reproduce the staging live-fire:
    ///
    /// ```bash
    /// # Port-forward fs9 staging (one terminal):
    /// kubectl -n db9 port-forward svc/fs9-public 15481:5481
    ///
    /// # Dump the cluster CA + sign a token (other terminal):
    /// kubectl -n cert-manager get secret tidb-serverless-ca-secret \
    ///   -o jsonpath='{.data.tls\.crt}' | base64 -d > /tmp/ca.pem
    /// kubectl -n cloud-admin-portal get secret auth9-secret \
    ///   -o jsonpath='{.data.AUTH9_JWT_PRIVATE_KEY}' | base64 -d > /tmp/priv.pem
    /// TOKEN=$(python3 /tmp/poc-staging/sign_token_auth9.py /tmp/priv.pem \
    ///         auth9-staging-1 0aj28rojeig3)
    ///
    /// FS9_GRPC_ENDPOINT=127.0.0.1:15481 \
    /// FS9_GRPC_TLS_SERVER_NAME=fs9.staging.db9.io \
    /// FS9_GRPC_CA_PATH=/tmp/ca.pem \
    /// FS9_LIVE_FIRE_TOKEN=$TOKEN \
    /// FS9_LIVE_FIRE_TENANT=0aj28rojeig3 \
    /// cargo test --bin db9-server -- --ignored stat_against_staging --nocapture
    /// ```
    #[ignore]
    #[tokio::test]
    async fn stat_against_staging() {
        let endpoint = match std::env::var("FS9_GRPC_ENDPOINT") {
            Ok(v) if !v.is_empty() => v,
            _ => {
                eprintln!("skipped: FS9_GRPC_ENDPOINT not set (see test docstring for setup)");
                return;
            }
        };
        let token = match std::env::var("FS9_LIVE_FIRE_TOKEN") {
            Ok(v) if !v.is_empty() => v,
            _ => {
                eprintln!("skipped: FS9_LIVE_FIRE_TOKEN not set");
                return;
            }
        };
        let tenant =
            std::env::var("FS9_LIVE_FIRE_TENANT").unwrap_or_else(|_| "0aj28rojeig3".to_string());

        // Build a channel the same way the shared connector would, but
        // honour overrides from the test env.
        let cfg = super::super::connector::ConnectorConfig::from_env()
            .expect("connector env should be complete for live-fire test");
        // Bypass the OnceLock so each test run starts fresh.
        let channel = {
            use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};
            let mut tls = ClientTlsConfig::new().domain_name(cfg.server_name.clone());
            if let Some(pem) = cfg.ca_pem.as_deref() {
                tls = tls.ca_certificate(Certificate::from_pem(pem));
            }
            let uri = if endpoint.starts_with("http") {
                endpoint.clone()
            } else {
                format!("https://{endpoint}")
            };
            Endpoint::try_from(uri)
                .expect("endpoint")
                .tls_config(tls)
                .expect("tls")
                .connect()
                .await
                .expect("connect")
        };

        let provider: Arc<dyn TokenProvider> = Arc::new(StaticTokenProvider::new(token));
        let backend = GrpcFsBackend::new(channel, &tenant, provider);

        // Smoke: stat("/") should succeed for any active JuiceFS tenant.
        match backend.stat("/").await {
            Ok(info) => {
                eprintln!("stat / OK: {info:?}");
                assert!(info.is_dir);
            }
            Err(e) => panic!("stat / failed: {e:#}"),
        }

        // Smoke: readdir("/"). May be empty for a fresh volume; assert
        // shape only.
        match backend.readdir("/").await {
            Ok(entries) => {
                eprintln!("readdir / OK: {} entries", entries.len());
            }
            Err(e) => panic!("readdir / failed: {e:#}"),
        }
    }
}
