use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncBufRead;

use crate::extensions::fs::embedded::types::EmbeddedFsError;
use crate::extensions::fs::normalizing::{
    DatabaseLifecycleAdmission, FsLifecycleAdmission, NormalizingFsBackend,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FsStorage {
    Inline,
    Pack,
    Object,
}

#[derive(Debug, Clone)]
pub(crate) struct FsFileInfo {
    pub path: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub mode: u32,
    pub generation: u64,
    pub mtime: u64,
    // Optional fs9 storage metadata. Unset for directories and empty files.
    pub storage: Option<FsStorage>,
    pub sealed: Option<bool>,
}

#[derive(Debug, Clone)]
pub(crate) struct FsBatchWriteFile {
    pub path: String,
    pub data: Vec<u8>,
    pub mode: Option<u32>,
}

#[derive(Debug)]
pub(crate) struct FsBatchWriteEntry {
    pub path: String,
    pub result: Result<usize>,
    /// Stable machine-readable failure category for execution-level errors.
    /// Set by the backend when a subgroup commit fails (e.g., "execution.txn_conflict").
    /// `None` for successful entries or planner-level errors (handled separately).
    pub failure_category: Option<&'static str>,
}

/// Result of a grouped atomic batch write, including execution metadata.
#[derive(Debug)]
pub(crate) struct FsBatchWriteGroupedResult {
    pub entries: Vec<FsBatchWriteEntry>,
    /// Number of subgroup transactions actually executed by the backend.
    /// This accounts for chunking within large directory groups.
    pub actual_subgroup_count: usize,
    /// Total number of server-side retries across all subgroups (txn_conflict).
    pub total_retries: usize,
    /// Number of subgroups where retries were exhausted (still failed after all attempts).
    pub retries_exhausted: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct FsRecursiveReaddirOptions {
    pub max_depth: usize,
    pub max_entries: usize,
    pub exclude_set: Option<Arc<globset::GlobSet>>,
}

#[derive(Debug, Clone)]
pub(crate) struct FsRecursiveReaddirResult {
    pub entries: Vec<FsFileInfo>,
    pub truncated: bool,
    pub total_dirs_scanned: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct FsReaddirResult {
    pub entries: Vec<FsFileInfo>,
    pub dir_version: Option<u64>,
}

pub(crate) fn batch_inline_read_entry_too_large_error(
    size: u64,
    max_file_bytes: usize,
) -> anyhow::Error {
    anyhow!(EmbeddedFsError::too_large(format!(
        "file too large for batch_inline_read: {} bytes exceeds limit {}",
        size, max_file_bytes
    )))
}

/// Pre-read gate every `FsBackend` read path must apply between
/// `stat(path)` and the first byte read. Encodes the contract that
/// embedded `pagefs` enforces at every read entry point: a read
/// requires a regular file — not a directory, not a symlink. Without
/// a shared, named statement of this contract the gRPC backend
/// silently diverges, because fs9 v2 `Stat` is `Lstat` (returns the
/// symlink's own info) while `ReadAt` follows the link — a short
/// symlink can smuggle a cap-sized prefix of a huge target past a
/// client-side size check. PR #2547 review #5.
///
/// Callers should `stat(path)` themselves so this helper is a pure
/// in-process check that piggy-backs on data already on the wire (no
/// extra RPC for code paths that were stat'ing anyway).
pub(crate) fn ensure_readable_as_regular_file(info: &FsFileInfo, path: &str) -> Result<()> {
    if info.is_dir {
        return Err(anyhow!(EmbeddedFsError::is_directory(path)));
    }
    if info.is_symlink {
        return Err(anyhow!(EmbeddedFsError::InvalidInput(
            "cannot read symlink as file; use readlink".to_string()
        )));
    }
    Ok(())
}

pub(crate) fn batch_inline_read_payload_too_large_error(
    _total_planned: u64,
    max_total_bytes: usize,
) -> anyhow::Error {
    anyhow!(EmbeddedFsError::too_large(format!(
        "batch_inline_read raw payload exceeds limit {} bytes",
        max_total_bytes
    )))
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FsWriteStreamOptions {
    pub expected_size: Option<u64>,
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FsPresignedRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FsCreateUpload {
    pub upload_token: String,
    pub upload_id: String,
    pub part_size: usize,
    pub expires_at: i64,
    pub checksum_algorithm: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FsMultipartCompletedPart {
    pub part_number: i32,
    pub etag: String,
    pub checksum_crc32c: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FsPreparedDownload {
    pub request: FsPresignedRequest,
    pub size: u64,
    pub storage: FsStorage,
    pub range_supported: bool,
}

#[async_trait]
pub(crate) trait FsWriteStream: Send {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()>;

    /// Consume the writer with a caller-declared outcome.
    ///
    /// - `outcome = Ok(())`  → commit; return bytes written.
    /// - `outcome = Err(e)` → ordered, observable abort; return `e`
    ///   (cleanup-path failures are logged, not returned — the caller's
    ///   error is always authoritative).
    ///
    /// This is the ONLY consumer; there is no `finish` / `abort` pair
    /// for callers to choose between. The choice that produced years of
    /// abort-vs-drop oscillation is eliminated at the type level.
    ///
    /// # Cancellation
    /// The `Err` arm is cancel-safe: the wire sequence runs on a detached
    /// task; dropping the `terminate` future (e.g. `tokio::time::timeout`
    /// elapsing) does NOT truncate the work, and close-then-abort still
    /// reaches the server in order.
    ///
    /// The `Ok` arm is NOT cancel-safe — callers MUST NOT wrap
    /// `terminate(Ok(()))` in a timeout.
    ///
    /// # Implementor contract
    /// Implementations MUST set their `terminated` flag synchronously,
    /// before any `.await` in `terminate`, so a cancelled `terminate`
    /// future (constructed but never polled past the first await point)
    /// does not trip the drop-bomb. Advisory cleanup in `Drop` still
    /// runs on such cancellations — the flag gates only the debug_assert.
    ///
    /// # Drop semantics
    /// Dropping the stream without calling `terminate` is a bug:
    /// - debug builds: `debug_assert!` fires, so CI catches forgot-terminate
    /// - release builds: `warn!` log + best-effort advisory cleanup spawn
    /// - server TTL remains the authoritative backstop in all cases
    async fn terminate(self: Box<Self>, outcome: Result<()>) -> Result<usize>;
}

#[async_trait]
pub(crate) trait FsBackend: Send + Sync {
    /// Stable low-cardinality backend kind used as a Prometheus label.
    fn backend_kind(&self) -> &'static str {
        "unknown"
    }

    async fn stat(&self, path: &str) -> Result<FsFileInfo>;
    async fn batch_stat(&self, paths: &[String]) -> Result<Vec<Result<FsFileInfo>>> {
        let mut entries = Vec::with_capacity(paths.len());
        for path in paths {
            entries.push(self.stat(path).await);
        }
        Ok(entries)
    }
    async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>>;
    async fn readdir_with_meta(&self, path: &str) -> Result<FsReaddirResult> {
        Ok(FsReaddirResult {
            entries: self.readdir(path).await?,
            dir_version: None,
        })
    }
    async fn batch_readdir(&self, paths: &[String]) -> Result<Vec<Result<Vec<FsFileInfo>>>> {
        let mut entries = Vec::with_capacity(paths.len());
        for path in paths {
            entries.push(self.readdir(path).await);
        }
        Ok(entries)
    }
    async fn readdir_recursive(
        &self,
        path: &str,
        opts: FsRecursiveReaddirOptions,
    ) -> Result<FsRecursiveReaddirResult> {
        if opts.max_entries == 0 {
            return Ok(FsRecursiveReaddirResult {
                entries: Vec::new(),
                truncated: false,
                total_dirs_scanned: 0,
            });
        }

        let mut entries = Vec::new();
        let mut truncated = false;
        let mut depth_exhausted = false;
        let mut total_dirs_scanned = 0usize;
        let root = normalize_readdir_path(path);
        let mut frontier = VecDeque::from([(root.clone(), 0usize)]);
        let mut visited = HashSet::from([root]);

        while let Some((_, depth)) = frontier.front() {
            if entries.len() >= opts.max_entries {
                truncated = true;
                break;
            }

            let current_depth = *depth;
            let mut level_paths = Vec::new();
            while matches!(frontier.front(), Some((_, level_depth)) if *level_depth == current_depth)
            {
                let (dir_path, dir_depth) = frontier
                    .pop_front()
                    .expect("frontier entry must exist while draining current level");
                level_paths.push((dir_path, dir_depth));
            }

            let paths = level_paths
                .iter()
                .map(|(dir_path, _)| dir_path.clone())
                .collect::<Vec<_>>();
            let results = self.batch_readdir(&paths).await?;
            if results.len() != level_paths.len() {
                return Err(anyhow!(EmbeddedFsError::internal(&format!(
                    "batch_readdir returned {} results for {} input paths",
                    results.len(),
                    level_paths.len()
                ))));
            }

            total_dirs_scanned = total_dirs_scanned.saturating_add(level_paths.len());
            for ((_, dir_depth), result) in level_paths.into_iter().zip(results) {
                let dir_entries = result?;
                for entry in dir_entries {
                    if opts.exclude_set.as_deref().is_some_and(|set| {
                        crate::extensions::fs::glob::path_matches_exclude(&entry.path, set)
                    }) {
                        continue;
                    }

                    if entry.is_dir && !entry.is_symlink {
                        if dir_depth < opts.max_depth && visited.insert(entry.path.clone()) {
                            frontier.push_back((entry.path.clone(), dir_depth + 1));
                        } else if dir_depth >= opts.max_depth {
                            depth_exhausted = true;
                        }
                    }

                    if entries.len() >= opts.max_entries {
                        truncated = true;
                        break;
                    }

                    entries.push(entry);
                }

                if truncated {
                    break;
                }
            }

            if truncated {
                break;
            }
        }

        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(FsRecursiveReaddirResult {
            entries,
            truncated: truncated || depth_exhausted,
            total_dirs_scanned,
        })
    }
    async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>>;
    async fn batch_inline_read(
        &self,
        paths: &[String],
        max_file_bytes: usize,
        max_total_bytes: usize,
    ) -> Result<Vec<Result<Vec<u8>>>> {
        let mut entries: Vec<Option<Result<Vec<u8>>>> =
            std::iter::repeat_with(|| None).take(paths.len()).collect();
        let mut eligible = Vec::new();
        let mut total_planned = 0u64;

        for (idx, path) in paths.iter().enumerate() {
            match self.stat(path).await {
                Ok(info) => {
                    if let Err(err) = ensure_readable_as_regular_file(&info, path) {
                        entries[idx] = Some(Err(err));
                        continue;
                    }

                    if info.size > max_file_bytes as u64 {
                        entries[idx] = Some(Err(batch_inline_read_entry_too_large_error(
                            info.size,
                            max_file_bytes,
                        )));
                        continue;
                    }

                    total_planned = total_planned.saturating_add(info.size);
                    eligible.push(idx);
                }
                Err(err) => entries[idx] = Some(Err(err)),
            }
        }

        if total_planned > max_total_bytes as u64 {
            return Err(batch_inline_read_payload_too_large_error(
                total_planned,
                max_total_bytes,
            ));
        }

        for idx in eligible {
            let path = &paths[idx];
            entries[idx] = Some(self.read_file(path, max_file_bytes).await);
        }

        Ok(entries
            .into_iter()
            .map(|entry| {
                entry.expect("batch_inline_read default implementation must fill every entry")
            })
            .collect())
    }
    async fn read_file_stream(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>>;
    async fn remove(&self, path: &str) -> Result<()>;
    async fn remove_recursive(&self, path: &str) -> Result<u64>;
    async fn mkdir(&self, path: &str, recursive: bool, mode: Option<u32>) -> Result<()>;
    async fn write_file(&self, path: &str, data: &[u8], mode: Option<u32>) -> Result<usize>;
    async fn batch_write(&self, files: Vec<FsBatchWriteFile>) -> Result<Vec<FsBatchWriteEntry>> {
        let mut entries = Vec::with_capacity(files.len());
        for file in files {
            let path = file.path;
            let result = self.write_file(&path, &file.data, file.mode).await;
            entries.push(FsBatchWriteEntry {
                path,
                result,
                failure_category: None,
            });
        }
        Ok(entries)
    }
    /// Whether this backend supports the `batch_write_atomic` operation with
    /// per-subgroup atomic semantics. Backends that return `false` must not
    /// have `batch_write_grouped` called on them.
    fn supports_batch_write_atomic(&self) -> bool {
        false
    }

    /// Whether this backend supports presigned S3 URLs for direct upload/download.
    /// When false, the client must use WS streaming for all file sizes.
    fn supports_presigned(&self) -> bool {
        false
    }
    /// Grouped atomic batch write: files are grouped by parent directory and
    /// each subgroup (bounded by `grouped_write_subgroup_size`) is committed
    /// in a single transaction. Per-subgroup atomic semantics.
    /// Only call this if `supports_batch_write_atomic()` returns true.
    async fn batch_write_grouped(
        &self,
        files: Vec<FsBatchWriteFile>,
    ) -> Result<FsBatchWriteGroupedResult> {
        let _ = files;
        Err(anyhow!(
            "batch_write_grouped is not supported by this backend"
        ))
    }
    async fn begin_write_stream(
        &self,
        path: &str,
        opts: FsWriteStreamOptions,
    ) -> Result<Box<dyn FsWriteStream>>;
    async fn read_file_at(&self, path: &str, offset: u64, length: usize) -> Result<Vec<u8>>;
    async fn write_file_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize>;
    /// Append `data` to the end of `path`. Returns the number of bytes
    /// appended.
    ///
    /// ## Whole-write assumption
    ///
    /// All current backends implement append as whole-write — on success,
    /// `data.len()` bytes are durably appended; on failure, zero. There is no
    /// partial-append success case. Callers of this method may assume the
    /// returned count equals `data.len()` when the result is `Ok`.
    ///
    /// If a future backend adopts a different contract (e.g. S3-style short
    /// writes, quota-based truncation), it MUST update both this doc and the
    /// trait method's return type to carry the server-reported delta.
    async fn append_file(&self, path: &str, data: &[u8]) -> Result<usize>;
    async fn truncate(&self, path: &str, size: u64) -> Result<()>;
    async fn rename(&self, old_path: &str, new_path: &str) -> Result<()>;
    async fn create_upload(
        &self,
        path: &str,
        expected_size: u64,
        mode: Option<u32>,
        checksum_algorithm: Option<&str>,
    ) -> Result<FsCreateUpload>;
    async fn presign_upload_part(
        &self,
        upload_token: &str,
        part_number: i32,
        checksum_crc32c: Option<&str>,
    ) -> Result<FsPresignedRequest>;
    async fn complete_upload(
        &self,
        upload_token: &str,
        parts: Vec<FsMultipartCompletedPart>,
        checksum: Option<[u8; 32]>,
    ) -> Result<usize>;
    async fn abort_upload(&self, upload_token: &str) -> Result<()>;
    async fn prepare_download(&self, path: &str) -> Result<FsPreparedDownload>;
    async fn symlink(&self, path: &str, target: &str) -> Result<()>;
    async fn readlink(&self, path: &str) -> Result<String>;
    async fn chmod(&self, path: &str, mode: u32) -> Result<()>;
}

pub(crate) fn is_backend_available() -> bool {
    crate::extensions::context::tikv_client().is_some()
        || crate::extensions::context::cached_fs_backend().is_some()
}

pub(crate) fn is_not_found_error(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<EmbeddedFsError>(),
        Some(EmbeddedFsError::NotFound(_))
    )
}

fn normalize_readdir_path(path: &str) -> String {
    if path.is_empty() || path == "/" {
        "/".to_string()
    } else {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        }
    }
}

// ============================================================================
// Tenant backend initialization
//
// db9-server no longer probes PD for `jfs_t_*` keyspaces and no longer
// accepts a process-wide embedded fallback. Control-plane backend_type owns
// tenant intent; the server data path opens JuiceFS and materializes a missing
// volume through FsPlaneAdmin before constructing the data-plane backend.
// The one PD read retained here is a non-cached lifecycle guard:
// absent/ENABLED may proceed, while DISABLED/ARCHIVED/TOMBSTONE fail closed
// before InitVolume can resurrect a tenant under teardown.
// ============================================================================

pub(crate) async fn ensure_juicefs_lifecycle_allows_init(tenant_keyspace: &str) -> Result<()> {
    let jfs_keyspace = juicefs_lifecycle_keyspace_name(tenant_keyspace);
    let pd_endpoints = crate::extensions::fs::juicefs_pd_endpoints();
    let state = match query_pd_keyspace_state(&pd_endpoints, &jfs_keyspace).await {
        Ok(state) => state,
        Err(err) => {
            crate::metrics::record_fs9_pd_lifecycle_probe(tenant_keyspace, "err");
            return Err(err);
        }
    };
    match validate_juicefs_lifecycle_state_for_metrics(
        tenant_keyspace,
        &jfs_keyspace,
        state.as_deref(),
    ) {
        Ok(result) => {
            crate::metrics::record_fs9_pd_lifecycle_probe(tenant_keyspace, result);
            Ok(())
        }
        Err((result, err)) => {
            crate::metrics::record_fs9_pd_lifecycle_probe(tenant_keyspace, result);
            Err(err)
        }
    }
}

pub(crate) async fn ensure_fs9_sql_surface_allowed(tenant_keyspace: &str) -> Result<()> {
    if !is_backend_available() {
        anyhow::bail!("fs9: TiKV storage backend not available");
    }
    if !crate::extensions::context::is_superuser() {
        anyhow::bail!("fs9: permission denied (superuser required)");
    }
    ensure_juicefs_lifecycle_allows_init(tenant_keyspace).await
}

fn validate_juicefs_lifecycle_state(
    tenant_keyspace: &str,
    jfs_keyspace: &str,
    state: Option<&str>,
) -> Result<()> {
    match state {
        None | Some("ENABLED") => Ok(()),
        Some(other) => anyhow::bail!(
            "fs9: JuiceFS keyspace `{jfs_keyspace}` is in state `{other}` \
             (expected absent or ENABLED). Tenant `{tenant_keyspace}` is being \
             torn down; refusing to initialize or use the JuiceFS backend."
        ),
    }
}

fn validate_juicefs_lifecycle_state_for_metrics(
    tenant_keyspace: &str,
    jfs_keyspace: &str,
    state: Option<&str>,
) -> std::result::Result<&'static str, (&'static str, anyhow::Error)> {
    match validate_juicefs_lifecycle_state(tenant_keyspace, jfs_keyspace, state) {
        Ok(()) => Ok("ok"),
        Err(err) => Err(("blocked", err)),
    }
}

fn juicefs_lifecycle_keyspace_name(tenant_keyspace: &str) -> String {
    let tenant_id =
        crate::auth::tenant_id_from_keyspace(tenant_keyspace).unwrap_or(tenant_keyspace);
    crate::extensions::fs::jfs_volume_id(tenant_id)
}

fn pd_base_url(pd_endpoint: &str) -> String {
    let endpoint = pd_endpoint.trim().trim_end_matches('/');
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return endpoint.to_string();
    }

    if std::env::var("TIKV_CA_PATH").is_ok() {
        format!("https://{endpoint}")
    } else {
        format!("http://{endpoint}")
    }
}

fn build_pd_probe_client() -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(5));

    if let (Ok(ca), Ok(cert_path), Ok(key_path)) = (
        std::env::var("TIKV_CA_PATH"),
        std::env::var("TIKV_CERT_PATH"),
        std::env::var("TIKV_KEY_PATH"),
    ) {
        let tls_config = build_pd_rustls_config(&ca, &cert_path, &key_path)?;
        builder = builder.use_preconfigured_tls(tls_config);
    }

    builder
        .build()
        .context("failed to build PD HTTP client for fs9 lifecycle guard")
}

/// Build the rustls `ClientConfig` for mTLS to PD directly, bypassing
/// reqwest's `Identity::from_pem` (which on the `rustls-tls` feature only
/// accepts PKCS#8 private keys). `rustls_pemfile::private_key` accepts
/// PKCS#1, PKCS#8 and SEC1.
fn build_pd_rustls_config(
    ca_path: &str,
    cert_path: &str,
    key_path: &str,
) -> Result<rustls::ClientConfig> {
    use rustls_pki_types::CertificateDer;

    let ca_pem = std::fs::read(ca_path)
        .with_context(|| format!("failed to read PD CA cert for fs9 lifecycle guard: {ca_path}"))?;
    let mut ca_reader = std::io::BufReader::new(ca_pem.as_slice());
    let ca_certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut ca_reader)
        .collect::<std::result::Result<_, _>>()
        .context("failed to parse PD CA cert for fs9 lifecycle guard")?;
    if ca_certs.is_empty() {
        anyhow::bail!("no CA certificates found in {ca_path}");
    }
    let mut root_store = rustls::RootCertStore::empty();
    for c in ca_certs {
        root_store
            .add(c)
            .context("failed to register PD CA cert in rustls root store")?;
    }

    let cert_pem = std::fs::read(cert_path).with_context(|| {
        format!("failed to read PD client cert for fs9 lifecycle guard: {cert_path}")
    })?;
    let mut cert_reader = std::io::BufReader::new(cert_pem.as_slice());
    let client_certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<_, _>>()
        .context("failed to parse PD client cert for fs9 lifecycle guard")?;
    if client_certs.is_empty() {
        anyhow::bail!("no client certificates found in {cert_path}");
    }

    let key_pem = std::fs::read(key_path).with_context(|| {
        format!("failed to read PD client key for fs9 lifecycle guard: {key_path}")
    })?;
    let mut key_reader = std::io::BufReader::new(key_pem.as_slice());
    let key = rustls_pemfile::private_key(&mut key_reader)
        .context("failed to parse PD client key for fs9 lifecycle guard")?
        .ok_or_else(|| anyhow!("no private key found in {key_path}"))?;

    rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(client_certs, key)
        .context("failed to build rustls ClientConfig for fs9 lifecycle guard")
}

fn parse_pd_keyspace_state_response(
    keyspace: &str,
    endpoint: &str,
    status: StatusCode,
    body: &str,
) -> Result<Option<String>> {
    if status == StatusCode::NOT_FOUND
        || (status == StatusCode::INTERNAL_SERVER_ERROR && pd_body_is_missing_keyspace(body))
    {
        return Ok(None);
    }

    if status.is_success() {
        let body: serde_json::Value = serde_json::from_str(body).with_context(|| {
            format!("PD keyspace probe for `{keyspace}` at `{endpoint}` returned invalid JSON")
        })?;
        let state = body
            .get("state")
            .and_then(|state| state.as_str())
            .ok_or_else(|| {
                anyhow!(
                    "PD keyspace probe for `{keyspace}` at `{endpoint}` returned no string `state`"
                )
            })?;
        return Ok(Some(state.to_string()));
    }

    Err(anyhow!(
        "PD keyspace probe for `{keyspace}` at `{endpoint}` returned HTTP {status}: {body}"
    ))
}

fn pd_body_is_missing_keyspace(body: &str) -> bool {
    let body = body.trim();
    body == "keyspace does not exist" || body == r#""keyspace does not exist""#
}

async fn query_pd_keyspace_state(
    pd_endpoints: &[String],
    keyspace: &str,
) -> Result<Option<String>> {
    if pd_endpoints.is_empty() {
        anyhow::bail!("no PD endpoints available for fs9 lifecycle guard");
    }

    let client = build_pd_probe_client()?;
    let mut errors = Vec::new();
    for endpoint in pd_endpoints {
        let url = format!("{}/pd/api/v2/keyspaces/{}", pd_base_url(endpoint), keyspace);
        match client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                let body = match resp.text().await {
                    Ok(body) => body,
                    Err(err) => {
                        errors.push(format!(
                            "`{endpoint}` failed to read PD response body for `{keyspace}`: {err}"
                        ));
                        continue;
                    }
                };
                match parse_pd_keyspace_state_response(keyspace, endpoint, status, &body) {
                    Ok(state) => return Ok(state),
                    Err(err) => errors.push(err.to_string()),
                }
            }
            Err(err) => errors.push(format!(
                "`{endpoint}` failed to query PD keyspace `{keyspace}`: {err}"
            )),
        }
    }

    anyhow::bail!(
        "failed to probe PD for keyspace `{}` via endpoints [{}]: {}",
        keyspace,
        pd_endpoints.join(", "),
        errors.join("; ")
    )
}

async fn init_backend(tenant_keyspace: &str) -> Result<Arc<dyn FsBackend>> {
    let client = crate::extensions::context::tikv_client().ok_or_else(|| {
        anyhow!(
            "fs9: TiKV client not available in extension context. \
             Ensure the caller wraps this in with_context_opts()."
        )
    })?;
    let principal = crate::extensions::context::effective_fs_plane_principal();
    init_backend_with_args(tenant_keyspace, client, principal).await
}

/// Backend construction exposed to non-SQL callers (notably the WebSocket
/// handler, which authenticates outside the ExtensionContext task-local).
///
/// db9-server routes fs9 to JuiceFS only. A missing JuiceFS volume is
/// materialized through FsPlaneAdmin before data-plane RPCs run; it is never
/// treated as a signal to open embedded PageFS metadata.
pub(crate) async fn init_backend_with_args(
    tenant_keyspace: &str,
    tikv_client: Arc<tikv_client::TransactionClient>,
    authenticated_principal: Option<crate::auth::fs_plane_token::Fs9Principal>,
) -> Result<Arc<dyn FsBackend>> {
    let db_id = crate::session_context::current_database_id();
    let lifecycle_database_id = (db_id != 0).then_some(db_id);
    init_backend_with_args_for_database(
        tenant_keyspace,
        tikv_client,
        authenticated_principal,
        lifecycle_database_id,
    )
    .await
}

pub(crate) async fn init_backend_with_args_for_database(
    tenant_keyspace: &str,
    tikv_client: Arc<tikv_client::TransactionClient>,
    authenticated_principal: Option<crate::auth::fs_plane_token::Fs9Principal>,
    lifecycle_database_id: Option<u64>,
) -> Result<Arc<dyn FsBackend>> {
    // Every leaf backend constructed here is wrapped in
    // `NormalizingFsBackend` so callers cannot bypass path shaping by
    // grabbing the inner directly. `acquire_statement_backend` caches
    // the wrapped instance, so the cache and every fs9 entry point
    // share one normalized contract.
    let inner: Arc<dyn FsBackend> = {
        #[cfg(fsplane_v2_generated)]
        {
            init_juicefs_backend(tenant_keyspace, authenticated_principal).await?
        }
        #[cfg(not(fsplane_v2_generated))]
        {
            let _ = &authenticated_principal;
            anyhow::bail!(
                "fs9: tenant `{tenant_keyspace}` requires JuiceFS but this db9-server \
                 binary was built without the fs9 v2 gRPC backend (protoc missing at \
                 build time and DB9_REQUIRE_PROTOC not set). Rebuild with protoc \
                 installed."
            );
        }
    };
    let Some(db_id) = lifecycle_database_id.filter(|db_id| *db_id != 0) else {
        return Ok(
            Arc::new(NormalizingFsBackend::new(tenant_keyspace, inner)) as Arc<dyn FsBackend>
        );
    };

    let store = Arc::new(crate::storage::TikvStore::from_transaction_client(
        tikv_client,
        Some(tenant_keyspace.to_string()),
    ));
    let admission: Arc<dyn FsLifecycleAdmission> =
        Arc::new(DatabaseLifecycleAdmission::new(store, db_id));
    Ok(Arc::new(NormalizingFsBackend::new_with_lifecycle_admission(
        tenant_keyspace,
        inner,
        admission,
    )) as Arc<dyn FsBackend>)
}

/// Instantiate the fs9 v2 gRPC backend for a tenant. This function has no
/// "fall back to embedded" path. Any configuration / session gap is a hard
/// error.
#[cfg(fsplane_v2_generated)]
async fn init_juicefs_backend(
    tenant_keyspace: &str,
    authenticated_principal: Option<crate::auth::fs_plane_token::Fs9Principal>,
) -> Result<Arc<dyn FsBackend>> {
    use crate::auth::fs_plane_token::Auth9MintConfig;
    use crate::extensions::fs::grpc::admin::ensure_juicefs_volume;
    use crate::extensions::fs::grpc::connector::shared_channel;

    // All four v2 env vars are required for JuiceFS tenants. Missing
    // any of them is a hard error — silently degrading would route
    // JuiceFS data to the embedded backend.
    let endpoint = crate::config::env_string("FS9_GRPC_ENDPOINT");
    let server_name = crate::config::env_string("FS9_GRPC_TLS_SERVER_NAME");
    let sign_url = crate::config::env_string("AUTH9_SIGN_URL");
    let api_key = crate::config::env_string("DB9_AUTH9_SERVICE_API_KEY");

    let mut missing = Vec::new();
    if endpoint.is_none() {
        missing.push("FS9_GRPC_ENDPOINT");
    }
    if server_name.is_none() {
        missing.push("FS9_GRPC_TLS_SERVER_NAME");
    }
    if sign_url.is_none() {
        missing.push("AUTH9_SIGN_URL");
    }
    if api_key.is_none() {
        missing.push("DB9_AUTH9_SERVICE_API_KEY");
    }
    if !missing.is_empty() {
        anyhow::bail!(
            "fs9: tenant `{tenant_keyspace}` is JuiceFS-backed but the v2 gRPC \
             configuration is incomplete: missing {}. Set all of \
             {{FS9_GRPC_ENDPOINT, FS9_GRPC_TLS_SERVER_NAME, AUTH9_SIGN_URL, \
             DB9_AUTH9_SERVICE_API_KEY}} on the db9-server deployment.",
            missing.join(", ")
        );
    }

    let mint_cfg = Auth9MintConfig::new(sign_url.unwrap(), api_key.unwrap());
    let _ = (endpoint, server_name); // consumed by shared_channel via env

    let channel = shared_channel().await?;
    init_juicefs_backend_from_parts(
        tenant_keyspace,
        authenticated_principal,
        channel,
        mint_cfg,
        |tenant_keyspace| async move { ensure_juicefs_lifecycle_allows_init(&tenant_keyspace).await },
        |channel, mint_cfg, tenant_id| async move {
            ensure_juicefs_volume(channel, &mint_cfg, &tenant_id).await
        },
    )
    .await
}

#[cfg(fsplane_v2_generated)]
async fn init_juicefs_backend_from_parts<L, LFut, E, EFut>(
    tenant_keyspace: &str,
    authenticated_principal: Option<crate::auth::fs_plane_token::Fs9Principal>,
    channel: tonic::transport::Channel,
    mint_cfg: crate::auth::fs_plane_token::Auth9MintConfig,
    lifecycle_check: L,
    ensure_volume: E,
) -> Result<Arc<dyn FsBackend>>
where
    L: FnOnce(String) -> LFut,
    LFut: std::future::Future<Output = Result<()>>,
    E: FnOnce(
        tonic::transport::Channel,
        crate::auth::fs_plane_token::Auth9MintConfig,
        String,
    ) -> EFut,
    EFut: std::future::Future<Output = Result<()>>,
{
    use crate::extensions::fs::grpc::client::{Auth9MintTokenProvider, GrpcFsBackend};

    let principal = authenticated_principal.ok_or_else(|| {
        anyhow!(
            "fs9: tenant `{tenant_keyspace}` is JuiceFS-backed and requires \
             an authenticated principal (role + access) to derive the \
             fs-plane scp; caller did not provide one."
        )
    })?;
    let tenant_id = crate::auth::tenant_id_from_keyspace(tenant_keyspace)
        .ok_or_else(|| {
            anyhow!(
                "fs9: tenant keyspace '{tenant_keyspace}' lacks the db9_tenant_ \
                 prefix; cannot derive tid claim."
            )
        })?
        .to_string();

    lifecycle_check(tenant_keyspace.to_string()).await?;
    ensure_volume(channel.clone(), mint_cfg.clone(), tenant_id.clone()).await?;

    let cache = fs9_plane_token_cache();
    let provider = Arc::new(Auth9MintTokenProvider::new(
        cache,
        mint_cfg,
        tenant_id.clone(),
        principal.role,
        principal.access,
    ));
    let backend = GrpcFsBackend::new(channel, &tenant_id, provider);
    Ok(Arc::new(backend) as Arc<dyn FsBackend>)
}

#[cfg(fsplane_v2_generated)]
fn fs9_plane_token_cache() -> Arc<crate::auth::fs_plane_token::Fs9PlaneTokenCache> {
    use crate::auth::fs_plane_token::Fs9PlaneTokenCache;
    static CACHE: std::sync::OnceLock<Arc<Fs9PlaneTokenCache>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| Arc::new(Fs9PlaneTokenCache::new()))
        .clone()
}

/// Acquire the authoritative fs9 backend for the current statement.
///
/// All SQL fs9 entry points must reuse this helper so a statement either
/// shares one bound backend or fails under one consistent contract.
pub(crate) async fn acquire_statement_backend(tenant_keyspace: &str) -> Result<Arc<dyn FsBackend>> {
    if let Some(backend) = crate::extensions::context::cached_fs_backend() {
        return Ok(backend);
    }

    let backend = init_backend(tenant_keyspace).await?;
    crate::extensions::context::cache_fs_backend(backend.clone())?;
    Ok(backend)
}

#[cfg(test)]
mod tests {
    // Test helpers collect call-order via std::sync::Mutex<Vec<_>>; the
    // disallowed_types lint targets production poisoning risk, not test fixtures.
    // (Sweeps into the parking_lot migration tracked in #2335.)
    #![allow(clippy::disallowed_types)]
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use tokio::io::{empty, AsyncBufRead};

    struct BatchInlineReadTestBackend {
        stats: HashMap<String, Result<FsFileInfo>>,
        files: HashMap<String, Result<Vec<u8>>>,
    }

    struct RecursiveReaddirTestBackend {
        dirs: HashMap<String, Result<Vec<FsFileInfo>>>,
    }

    impl BatchInlineReadTestBackend {
        fn new(
            stats: HashMap<String, Result<FsFileInfo>>,
            files: HashMap<String, Result<Vec<u8>>>,
        ) -> Self {
            Self { stats, files }
        }
    }

    impl RecursiveReaddirTestBackend {
        fn new(dirs: HashMap<String, Result<Vec<FsFileInfo>>>) -> Self {
            Self { dirs }
        }
    }

    #[async_trait]
    impl FsBackend for BatchInlineReadTestBackend {
        async fn stat(&self, path: &str) -> Result<FsFileInfo> {
            match self.stats.get(path) {
                Some(Ok(info)) => Ok(info.clone()),
                Some(Err(err)) => Err(anyhow!(err.to_string())),
                None => Err(anyhow!(EmbeddedFsError::not_found(path))),
            }
        }

        async fn readdir(&self, _path: &str) -> Result<Vec<FsFileInfo>> {
            unreachable!("readdir is not used in these tests");
        }

        async fn read_file(&self, path: &str, _max_bytes: usize) -> Result<Vec<u8>> {
            match self.files.get(path) {
                Some(Ok(data)) => Ok(data.clone()),
                Some(Err(err)) => Err(anyhow!(err.to_string())),
                None => Err(anyhow!(EmbeddedFsError::not_found(path))),
            }
        }

        async fn read_file_stream(
            &self,
            _path: &str,
            _max_bytes: usize,
        ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
            Ok(Box::new(empty()))
        }

        async fn remove(&self, _path: &str) -> Result<()> {
            unreachable!("remove is not used in these tests");
        }

        async fn remove_recursive(&self, _path: &str) -> Result<u64> {
            unreachable!("remove_recursive is not used in these tests");
        }

        async fn mkdir(&self, _path: &str, _recursive: bool, _mode: Option<u32>) -> Result<()> {
            unreachable!("mkdir is not used in these tests");
        }

        async fn write_file(&self, _path: &str, _data: &[u8], _mode: Option<u32>) -> Result<usize> {
            unreachable!("write_file is not used in these tests");
        }

        async fn begin_write_stream(
            &self,
            _path: &str,
            _opts: FsWriteStreamOptions,
        ) -> Result<Box<dyn FsWriteStream>> {
            unreachable!("begin_write_stream is not used in these tests");
        }

        async fn read_file_at(&self, _path: &str, _offset: u64, _length: usize) -> Result<Vec<u8>> {
            unreachable!("read_file_at is not used in these tests");
        }

        async fn write_file_at(&self, _path: &str, _offset: u64, _data: &[u8]) -> Result<usize> {
            unreachable!("write_file_at is not used in these tests");
        }

        async fn append_file(&self, _path: &str, _data: &[u8]) -> Result<usize> {
            unreachable!("append_file is not used in these tests");
        }

        async fn truncate(&self, _path: &str, _size: u64) -> Result<()> {
            unreachable!("truncate is not used in these tests");
        }

        async fn rename(&self, _old_path: &str, _new_path: &str) -> Result<()> {
            unreachable!("rename is not used in these tests");
        }

        async fn create_upload(
            &self,
            _path: &str,
            _expected_size: u64,
            _mode: Option<u32>,
            _checksum_algorithm: Option<&str>,
        ) -> Result<FsCreateUpload> {
            unreachable!("create_upload is not used in these tests");
        }

        async fn presign_upload_part(
            &self,
            _upload_token: &str,
            _part_number: i32,
            _checksum_crc32c: Option<&str>,
        ) -> Result<FsPresignedRequest> {
            unreachable!("presign_upload_part is not used in these tests");
        }

        async fn complete_upload(
            &self,
            _upload_token: &str,
            _parts: Vec<FsMultipartCompletedPart>,
            _checksum: Option<[u8; 32]>,
        ) -> Result<usize> {
            unreachable!("complete_upload is not used in these tests");
        }

        async fn abort_upload(&self, _upload_token: &str) -> Result<()> {
            unreachable!("abort_upload is not used in these tests");
        }

        async fn prepare_download(&self, _path: &str) -> Result<FsPreparedDownload> {
            unreachable!("prepare_download is not used in these tests");
        }

        async fn symlink(&self, _path: &str, _target: &str) -> Result<()> {
            unreachable!("symlink is not used in these tests");
        }

        async fn readlink(&self, _path: &str) -> Result<String> {
            unreachable!("readlink is not used in these tests");
        }

        async fn chmod(&self, _path: &str, _mode: u32) -> Result<()> {
            unreachable!("chmod is not used in these tests");
        }
    }

    #[async_trait]
    impl FsBackend for RecursiveReaddirTestBackend {
        async fn stat(&self, _path: &str) -> Result<FsFileInfo> {
            unreachable!("stat is not used in these tests");
        }

        async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>> {
            match self.dirs.get(path) {
                Some(Ok(entries)) => Ok(entries.clone()),
                Some(Err(err)) => Err(anyhow!(err.to_string())),
                None => Err(anyhow!(EmbeddedFsError::not_found(path))),
            }
        }

        async fn read_file(&self, _path: &str, _max_bytes: usize) -> Result<Vec<u8>> {
            unreachable!("read_file is not used in these tests");
        }

        async fn read_file_stream(
            &self,
            _path: &str,
            _max_bytes: usize,
        ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
            Ok(Box::new(empty()))
        }

        async fn remove(&self, _path: &str) -> Result<()> {
            unreachable!("remove is not used in these tests");
        }

        async fn remove_recursive(&self, _path: &str) -> Result<u64> {
            unreachable!("remove_recursive is not used in these tests");
        }

        async fn mkdir(&self, _path: &str, _recursive: bool, _mode: Option<u32>) -> Result<()> {
            unreachable!("mkdir is not used in these tests");
        }

        async fn write_file(&self, _path: &str, _data: &[u8], _mode: Option<u32>) -> Result<usize> {
            unreachable!("write_file is not used in these tests");
        }

        async fn begin_write_stream(
            &self,
            _path: &str,
            _opts: FsWriteStreamOptions,
        ) -> Result<Box<dyn FsWriteStream>> {
            unreachable!("begin_write_stream is not used in these tests");
        }

        async fn read_file_at(&self, _path: &str, _offset: u64, _length: usize) -> Result<Vec<u8>> {
            unreachable!("read_file_at is not used in these tests");
        }

        async fn write_file_at(&self, _path: &str, _offset: u64, _data: &[u8]) -> Result<usize> {
            unreachable!("write_file_at is not used in these tests");
        }

        async fn append_file(&self, _path: &str, _data: &[u8]) -> Result<usize> {
            unreachable!("append_file is not used in these tests");
        }

        async fn truncate(&self, _path: &str, _size: u64) -> Result<()> {
            unreachable!("truncate is not used in these tests");
        }

        async fn rename(&self, _old_path: &str, _new_path: &str) -> Result<()> {
            unreachable!("rename is not used in these tests");
        }

        async fn create_upload(
            &self,
            _path: &str,
            _expected_size: u64,
            _mode: Option<u32>,
            _checksum_algorithm: Option<&str>,
        ) -> Result<FsCreateUpload> {
            unreachable!("create_upload is not used in these tests");
        }

        async fn presign_upload_part(
            &self,
            _upload_token: &str,
            _part_number: i32,
            _checksum_crc32c: Option<&str>,
        ) -> Result<FsPresignedRequest> {
            unreachable!("presign_upload_part is not used in these tests");
        }

        async fn complete_upload(
            &self,
            _upload_token: &str,
            _parts: Vec<FsMultipartCompletedPart>,
            _checksum: Option<[u8; 32]>,
        ) -> Result<usize> {
            unreachable!("complete_upload is not used in these tests");
        }

        async fn abort_upload(&self, _upload_token: &str) -> Result<()> {
            unreachable!("abort_upload is not used in these tests");
        }

        async fn prepare_download(&self, _path: &str) -> Result<FsPreparedDownload> {
            unreachable!("prepare_download is not used in these tests");
        }

        async fn symlink(&self, _path: &str, _target: &str) -> Result<()> {
            unreachable!("symlink is not used in these tests");
        }

        async fn readlink(&self, _path: &str) -> Result<String> {
            unreachable!("readlink is not used in these tests");
        }

        async fn chmod(&self, _path: &str, _mode: u32) -> Result<()> {
            unreachable!("chmod is not used in these tests");
        }
    }

    #[tokio::test]
    async fn acquire_statement_backend_without_context_returns_error() {
        match acquire_statement_backend("tenant_a").await {
            Ok(_) => panic!("missing extension context must return error"),
            Err(err) => assert!(
                err.to_string().contains("TiKV client not available"),
                "unexpected error: {err}"
            ),
        }
    }

    #[test]
    fn juicefs_lifecycle_guard_allows_absent_and_enabled() {
        for state in [None, Some("ENABLED")] {
            validate_juicefs_lifecycle_state("db9_tenant_abc", "jfs_t_abc", state)
                .expect("absent and ENABLED states should allow JuiceFS init");
        }
    }

    #[test]
    fn juicefs_lifecycle_guard_rejects_teardown_states() {
        for state in ["DISABLED", "ARCHIVED", "TOMBSTONE"] {
            let err = validate_juicefs_lifecycle_state("db9_tenant_abc", "jfs_t_abc", Some(state))
                .expect_err("teardown states must fail closed");
            let msg = err.to_string();
            assert!(msg.contains(state), "error should name state: {msg}");
            assert!(
                msg.contains("refusing to initialize"),
                "error should explain fail-closed behavior: {msg}"
            );
        }
    }

    #[test]
    fn juicefs_lifecycle_metric_result_uses_final_validation_outcome() {
        assert_eq!(
            validate_juicefs_lifecycle_state_for_metrics("db9_tenant_abc", "jfs_t_abc", None)
                .expect("absent lifecycle state should pass"),
            "ok"
        );
        assert_eq!(
            validate_juicefs_lifecycle_state_for_metrics(
                "db9_tenant_abc",
                "jfs_t_abc",
                Some("ENABLED"),
            )
            .expect("enabled lifecycle state should pass"),
            "ok"
        );
        let err = validate_juicefs_lifecycle_state_for_metrics(
            "db9_tenant_abc",
            "jfs_t_abc",
            Some("DISABLED"),
        )
        .expect_err("blocked lifecycle state should fail");
        assert_eq!(err.0, "blocked");
        assert!(err.1.to_string().contains("DISABLED"));
    }

    #[test]
    fn pd_keyspace_state_parser_treats_missing_as_absent() {
        let parsed = parse_pd_keyspace_state_response(
            "jfs_t_abc",
            "pd:2379",
            StatusCode::INTERNAL_SERVER_ERROR,
            r#""keyspace does not exist""#,
        )
        .expect("missing keyspace body should parse");
        assert_eq!(parsed, None);
    }

    #[cfg(fsplane_v2_generated)]
    #[tokio::test]
    async fn juicefs_backend_init_runs_lifecycle_then_materializes_volume() {
        use crate::auth::fs_plane_token::{Auth9MintConfig, Fs9Access, Fs9Principal};
        use std::sync::{Arc as StdArc, Mutex as StdMutex};

        let calls = StdArc::new(StdMutex::new(Vec::new()));
        let lifecycle_calls = calls.clone();
        let ensure_calls = calls.clone();

        let channel = tonic::transport::Channel::from_static("http://127.0.0.1:1").connect_lazy();
        let principal = Fs9Principal {
            role: "admin".to_string(),
            access: Fs9Access::ReadWrite,
        };
        let mint_cfg =
            Auth9MintConfig::new("https://auth.example/v1/jwt/sign".into(), "secret".into());

        let _backend = init_juicefs_backend_from_parts(
            "db9_tenant_abc",
            Some(principal),
            channel,
            mint_cfg,
            move |tenant_keyspace| {
                let lifecycle_calls = lifecycle_calls.clone();
                async move {
                    lifecycle_calls
                        .lock()
                        .unwrap()
                        .push(format!("lifecycle:{tenant_keyspace}"));
                    Ok(())
                }
            },
            move |_channel, mint_cfg, tenant_id| {
                let ensure_calls = ensure_calls.clone();
                async move {
                    ensure_calls
                        .lock()
                        .unwrap()
                        .push(format!("ensure:{tenant_id}:{}", mint_cfg.sign_url));
                    Ok(())
                }
            },
        )
        .await
        .expect("backend init should succeed with injected hooks");

        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[
                "lifecycle:db9_tenant_abc".to_string(),
                "ensure:abc:https://auth.example/v1/jwt/sign".to_string(),
            ]
        );
    }

    #[cfg(fsplane_v2_generated)]
    #[tokio::test]
    async fn juicefs_backend_init_stops_before_materialize_when_lifecycle_rejects() {
        use crate::auth::fs_plane_token::{Auth9MintConfig, Fs9Access, Fs9Principal};
        use std::sync::{Arc as StdArc, Mutex as StdMutex};

        let calls = StdArc::new(StdMutex::new(Vec::new()));
        let lifecycle_calls = calls.clone();
        let ensure_calls = calls.clone();

        let channel = tonic::transport::Channel::from_static("http://127.0.0.1:1").connect_lazy();
        let principal = Fs9Principal {
            role: "admin".to_string(),
            access: Fs9Access::ReadWrite,
        };
        let mint_cfg =
            Auth9MintConfig::new("https://auth.example/v1/jwt/sign".into(), "secret".into());

        let result = init_juicefs_backend_from_parts(
            "db9_tenant_abc",
            Some(principal),
            channel,
            mint_cfg,
            move |tenant_keyspace| {
                let lifecycle_calls = lifecycle_calls.clone();
                async move {
                    lifecycle_calls
                        .lock()
                        .unwrap()
                        .push(format!("lifecycle:{tenant_keyspace}"));
                    anyhow::bail!("teardown state")
                }
            },
            move |_channel, _mint_cfg, tenant_id| {
                let ensure_calls = ensure_calls.clone();
                async move {
                    ensure_calls
                        .lock()
                        .unwrap()
                        .push(format!("ensure:{tenant_id}"));
                    Ok(())
                }
            },
        )
        .await;
        let err = match result {
            Ok(_) => panic!("lifecycle rejection must fail backend init"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("teardown state"));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &["lifecycle:db9_tenant_abc".to_string()]
        );
    }

    #[test]
    fn is_not_found_error_matches_embedded_error_type() {
        let err = anyhow!(EmbeddedFsError::NotFound("/missing".to_string()));
        assert!(is_not_found_error(&err));

        let other = anyhow!(EmbeddedFsError::InvalidInput("bad".to_string()));
        assert!(!is_not_found_error(&other));
    }

    #[tokio::test]
    async fn init_backend_without_context_returns_error() {
        match init_backend("tenant_a").await {
            Ok(_) => panic!("missing extension context must return error"),
            Err(err) => assert!(
                err.to_string().contains("TiKV client not available"),
                "unexpected error: {err}"
            ),
        }
    }

    #[tokio::test]
    async fn default_batch_inline_read_preserves_mixed_entry_results() {
        let backend = BatchInlineReadTestBackend::new(
            HashMap::from([
                (
                    "/ok".to_string(),
                    Ok(FsFileInfo {
                        path: "/ok".to_string(),
                        is_dir: false,
                        is_symlink: false,
                        size: 5,
                        mode: 0o644,
                        generation: 1,
                        mtime: 0,
                        storage: Some(FsStorage::Inline),
                        sealed: Some(false),
                    }),
                ),
                (
                    "/dir".to_string(),
                    Ok(FsFileInfo {
                        path: "/dir".to_string(),
                        is_dir: true,
                        is_symlink: false,
                        size: 0,
                        mode: 0o755,
                        generation: 1,
                        mtime: 0,
                        storage: None,
                        sealed: Some(false),
                    }),
                ),
                (
                    "/large".to_string(),
                    Ok(FsFileInfo {
                        path: "/large".to_string(),
                        is_dir: false,
                        is_symlink: false,
                        size: 99,
                        mode: 0o644,
                        generation: 1,
                        mtime: 0,
                        storage: Some(FsStorage::Inline),
                        sealed: Some(false),
                    }),
                ),
                (
                    "/missing".to_string(),
                    Err(anyhow!(EmbeddedFsError::not_found("/missing"))),
                ),
            ]),
            HashMap::from([("/ok".to_string(), Ok(b"hello".to_vec()))]),
        );

        let results = backend
            .batch_inline_read(
                &[
                    "/ok".to_string(),
                    "/dir".to_string(),
                    "/large".to_string(),
                    "/missing".to_string(),
                ],
                8,
                64,
            )
            .await
            .expect("batch_inline_read should succeed");

        assert_eq!(results.len(), 4);
        assert_eq!(results[0].as_ref().unwrap(), b"hello");
        assert!(results[1]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("IsDirectory"));
        let too_large = results[2]
            .as_ref()
            .unwrap_err()
            .downcast_ref::<EmbeddedFsError>()
            .expect("entry too large should be typed");
        match too_large {
            EmbeddedFsError::TooLarge(msg) => assert_eq!(
                msg,
                "file too large for batch_inline_read: 99 bytes exceeds limit 8"
            ),
            other => panic!("expected typed too-large error, got {other}"),
        }
        assert!(results[3]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("NotFound"));
    }

    #[tokio::test]
    async fn default_batch_inline_read_rejects_symlink_entries() {
        let backend = BatchInlineReadTestBackend::new(
            HashMap::from([
                (
                    "/link".to_string(),
                    Ok(FsFileInfo {
                        path: "/link".to_string(),
                        is_dir: false,
                        is_symlink: true,
                        size: 11,
                        mode: 0o777,
                        generation: 1,
                        mtime: 0,
                        storage: None,
                        sealed: Some(false),
                    }),
                ),
                (
                    "/ok".to_string(),
                    Ok(FsFileInfo {
                        path: "/ok".to_string(),
                        is_dir: false,
                        is_symlink: false,
                        size: 5,
                        mode: 0o644,
                        generation: 1,
                        mtime: 0,
                        storage: Some(FsStorage::Inline),
                        sealed: Some(false),
                    }),
                ),
            ]),
            HashMap::from([
                ("/link".to_string(), Ok(b"target-path".to_vec())),
                ("/ok".to_string(), Ok(b"hello".to_vec())),
            ]),
        );

        let results = backend
            .batch_inline_read(&["/link".to_string(), "/ok".to_string()], 32, 64)
            .await
            .expect("batch_inline_read should succeed with per-entry errors");

        assert_eq!(results.len(), 2);
        let symlink_err = results[0]
            .as_ref()
            .unwrap_err()
            .downcast_ref::<EmbeddedFsError>()
            .expect("symlink entry should remain a typed error");
        assert!(matches!(
            symlink_err,
            EmbeddedFsError::InvalidInput(msg) if msg == "cannot read symlink as file; use readlink"
        ));
        assert_eq!(results[1].as_ref().unwrap(), b"hello");
    }

    #[tokio::test]
    async fn default_batch_inline_read_rejects_total_payload_over_limit() {
        let backend = BatchInlineReadTestBackend::new(
            HashMap::from([
                (
                    "/a".to_string(),
                    Ok(FsFileInfo {
                        path: "/a".to_string(),
                        is_dir: false,
                        is_symlink: false,
                        size: 4,
                        mode: 0o644,
                        generation: 1,
                        mtime: 0,
                        storage: Some(FsStorage::Inline),
                        sealed: Some(false),
                    }),
                ),
                (
                    "/b".to_string(),
                    Ok(FsFileInfo {
                        path: "/b".to_string(),
                        is_dir: false,
                        is_symlink: false,
                        size: 5,
                        mode: 0o644,
                        generation: 1,
                        mtime: 0,
                        storage: Some(FsStorage::Inline),
                        sealed: Some(false),
                    }),
                ),
            ]),
            HashMap::from([
                ("/a".to_string(), Ok(vec![1, 2, 3, 4])),
                ("/b".to_string(), Ok(vec![5, 6, 7, 8, 9])),
            ]),
        );

        let err = backend
            .batch_inline_read(&["/a".to_string(), "/b".to_string()], 8, 8)
            .await
            .expect_err("batch_inline_read must reject payloads that exceed the total cap");
        let fs_err = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("payload over limit should be typed");
        assert!(matches!(fs_err, EmbeddedFsError::TooLarge(_)));
        assert_eq!(
            fs_err.to_string(),
            "fs: TooLarge: batch_inline_read raw payload exceeds limit 8 bytes"
        );
    }

    #[tokio::test]
    async fn default_batch_readdir_preserves_per_path_results() {
        let backend = RecursiveReaddirTestBackend::new(HashMap::from([
            (
                "/".to_string(),
                Ok(vec![FsFileInfo {
                    path: "/root.txt".to_string(),
                    is_dir: false,
                    is_symlink: false,
                    size: 4,
                    mode: 0o644,
                    generation: 1,
                    mtime: 0,
                    storage: Some(FsStorage::Inline),
                    sealed: Some(false),
                }]),
            ),
            (
                "/missing".to_string(),
                Err(anyhow!(EmbeddedFsError::not_found("/missing"))),
            ),
        ]));

        let results = backend
            .batch_readdir(&["/".to_string(), "/missing".to_string()])
            .await
            .expect("batch_readdir should succeed");

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().unwrap().len(), 1);
        assert!(results[1]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("NotFound"));
    }

    #[tokio::test]
    async fn default_readdir_with_meta_returns_none_dir_version() {
        let backend = RecursiveReaddirTestBackend::new(HashMap::from([(
            "/".to_string(),
            Ok(vec![FsFileInfo {
                path: "/root.txt".to_string(),
                is_dir: false,
                is_symlink: false,
                size: 4,
                mode: 0o644,
                generation: 1,
                mtime: 0,
                storage: Some(FsStorage::Inline),
                sealed: Some(false),
            }]),
        )]));

        let result = backend
            .readdir_with_meta("/")
            .await
            .expect("readdir_with_meta should succeed");

        assert_eq!(result.dir_version, None);
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].path, "/root.txt");
    }

    #[tokio::test]
    async fn default_readdir_recursive_walks_frontiers_and_caps_entries() {
        let backend = RecursiveReaddirTestBackend::new(HashMap::from([
            (
                "/".to_string(),
                Ok(vec![
                    FsFileInfo {
                        path: "/alpha.txt".to_string(),
                        is_dir: false,
                        is_symlink: false,
                        size: 5,
                        mode: 0o644,
                        generation: 1,
                        mtime: 0,
                        storage: Some(FsStorage::Inline),
                        sealed: Some(false),
                    },
                    FsFileInfo {
                        path: "/dir".to_string(),
                        is_dir: true,
                        is_symlink: false,
                        size: 0,
                        mode: 0o755,
                        generation: 1,
                        mtime: 0,
                        storage: None,
                        sealed: Some(false),
                    },
                ]),
            ),
            (
                "/dir".to_string(),
                Ok(vec![
                    FsFileInfo {
                        path: "/dir/bravo.txt".to_string(),
                        is_dir: false,
                        is_symlink: false,
                        size: 5,
                        mode: 0o644,
                        generation: 1,
                        mtime: 0,
                        storage: Some(FsStorage::Inline),
                        sealed: Some(false),
                    },
                    FsFileInfo {
                        path: "/dir/nested".to_string(),
                        is_dir: true,
                        is_symlink: false,
                        size: 0,
                        mode: 0o755,
                        generation: 1,
                        mtime: 0,
                        storage: None,
                        sealed: Some(false),
                    },
                ]),
            ),
            (
                "/dir/nested".to_string(),
                Ok(vec![FsFileInfo {
                    path: "/dir/nested/charlie.txt".to_string(),
                    is_dir: false,
                    is_symlink: false,
                    size: 7,
                    mode: 0o644,
                    generation: 1,
                    mtime: 0,
                    storage: Some(FsStorage::Inline),
                    sealed: Some(false),
                }]),
            ),
        ]));

        let result = backend
            .readdir_recursive(
                "/",
                FsRecursiveReaddirOptions {
                    max_depth: 8,
                    max_entries: 3,
                    exclude_set: None,
                },
            )
            .await
            .expect("readdir_recursive should succeed");

        let paths = result
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["/alpha.txt", "/dir", "/dir/bravo.txt"]);
        assert!(result.truncated);
        assert_eq!(result.total_dirs_scanned, 2);
    }

    #[tokio::test]
    async fn default_readdir_recursive_prunes_excluded_directories() {
        let backend = RecursiveReaddirTestBackend::new(HashMap::from([
            (
                "/".to_string(),
                Ok(vec![
                    FsFileInfo {
                        path: "/keep".to_string(),
                        is_dir: true,
                        is_symlink: false,
                        size: 0,
                        mode: 0o755,
                        generation: 1,
                        mtime: 0,
                        storage: None,
                        sealed: Some(false),
                    },
                    FsFileInfo {
                        path: "/skip".to_string(),
                        is_dir: true,
                        is_symlink: false,
                        size: 0,
                        mode: 0o755,
                        generation: 1,
                        mtime: 0,
                        storage: None,
                        sealed: Some(false),
                    },
                ]),
            ),
            (
                "/keep".to_string(),
                Ok(vec![FsFileInfo {
                    path: "/keep/visible.txt".to_string(),
                    is_dir: false,
                    is_symlink: false,
                    size: 7,
                    mode: 0o644,
                    generation: 1,
                    mtime: 0,
                    storage: Some(FsStorage::Inline),
                    sealed: Some(false),
                }]),
            ),
            (
                "/skip".to_string(),
                Ok(vec![FsFileInfo {
                    path: "/skip/hidden.txt".to_string(),
                    is_dir: false,
                    is_symlink: false,
                    size: 6,
                    mode: 0o644,
                    generation: 1,
                    mtime: 0,
                    storage: Some(FsStorage::Inline),
                    sealed: Some(false),
                }]),
            ),
        ]));
        let exclude_set =
            crate::extensions::fs::glob::build_exclude_globset(Some("skip/**")).unwrap();

        let result = backend
            .readdir_recursive(
                "/",
                FsRecursiveReaddirOptions {
                    max_depth: 8,
                    max_entries: 10,
                    exclude_set,
                },
            )
            .await
            .expect("readdir_recursive should succeed");

        let paths = result
            .entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["/keep", "/keep/visible.txt"]);
        assert_eq!(result.total_dirs_scanned, 2);
    }

    fn make_info(is_dir: bool, is_symlink: bool, size: u64) -> FsFileInfo {
        FsFileInfo {
            path: "/x".to_string(),
            is_dir,
            is_symlink,
            size,
            mode: 0o644,
            generation: 1,
            mtime: 0,
            storage: if is_dir || is_symlink {
                None
            } else {
                Some(FsStorage::Inline)
            },
            sealed: Some(false),
        }
    }

    #[test]
    fn ensure_readable_helper_accepts_regular_file() {
        let info = make_info(false, false, 42);
        super::ensure_readable_as_regular_file(&info, "/x").expect("regular file must be readable");
    }

    #[test]
    fn ensure_readable_helper_rejects_directory_with_typed_error() {
        let info = make_info(true, false, 0);
        let err = super::ensure_readable_as_regular_file(&info, "/d").unwrap_err();
        let typed = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("typed EmbeddedFsError expected");
        assert!(matches!(typed, EmbeddedFsError::IsDirectory(_)));
    }

    /// PR #2547 review #5 — the gate must reject symlinks so a short
    /// symlink can't smuggle a prefix of a huge target past the size
    /// cap on fs9 v2 (Stat is Lstat, ReadAt follows).
    #[test]
    fn ensure_readable_helper_rejects_symlink_with_typed_error() {
        let info = make_info(false, true, 11);
        let err = super::ensure_readable_as_regular_file(&info, "/link").unwrap_err();
        let typed = err
            .downcast_ref::<EmbeddedFsError>()
            .expect("typed EmbeddedFsError expected");
        assert!(matches!(
            typed,
            EmbeddedFsError::InvalidInput(msg) if msg == "cannot read symlink as file; use readlink"
        ));
    }
}
