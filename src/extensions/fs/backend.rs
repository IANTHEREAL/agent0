use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncBufRead;

use crate::extensions::fs::embedded::types::EmbeddedFsError;
use crate::extensions::fs::embedded::EmbeddedFsBackend;

const DEFAULT_PD_ENDPOINTS: &str = "127.0.0.1:2379";

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
                    if info.is_dir {
                        entries[idx] = Some(Err(anyhow!(EmbeddedFsError::is_directory(path))));
                        continue;
                    }

                    if info.is_symlink {
                        entries[idx] = Some(Err(anyhow!(EmbeddedFsError::InvalidInput(
                            "cannot read symlink as file; use readlink".to_string(),
                        ))));
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

fn legacy_juicefs_config_markers_with<F>(mut get_env: F) -> Vec<String>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut markers = Vec::new();

    if get_env("FS9_GRPC_PD_ENDPOINTS").is_some() {
        markers.push("FS9_GRPC_PD_ENDPOINTS".to_string());
    }

    if let Some(value) = get_env("FS9_BACKEND") {
        let normalized = value.trim().to_ascii_lowercase();
        if !normalized.is_empty() && normalized != "embedded" {
            markers.push(format!("FS9_BACKEND={value}"));
        }
    }

    markers
}

fn legacy_juicefs_config_markers() -> Vec<String> {
    legacy_juicefs_config_markers_with(crate::config::env_string)
}

fn parse_pd_endpoints(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|endpoint| !endpoint.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn pd_endpoints_from_env() -> Vec<String> {
    let raw = crate::config::env_string("PD_ENDPOINTS")
        .unwrap_or_else(|| DEFAULT_PD_ENDPOINTS.to_string());
    parse_pd_endpoints(&raw)
}

fn legacy_juicefs_keyspace_name(tenant_keyspace: &str) -> String {
    let tenant_id = tenant_keyspace
        .strip_prefix("db9_tenant_")
        .unwrap_or(tenant_keyspace);
    format!("jfs_t_{tenant_id}")
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
        let ca_pem = std::fs::read(&ca)
            .with_context(|| format!("failed to read PD CA cert for fs9 guard: {ca}"))?;
        let ca_cert = reqwest::tls::Certificate::from_pem(&ca_pem)
            .context("failed to parse PD CA cert for fs9 guard")?;

        let cert_pem = std::fs::read(&cert_path)
            .with_context(|| format!("failed to read PD client cert for fs9 guard: {cert_path}"))?;
        let key_pem = std::fs::read(&key_path)
            .with_context(|| format!("failed to read PD client key for fs9 guard: {key_path}"))?;
        let mut identity_pem = cert_pem;
        identity_pem.extend_from_slice(&key_pem);
        let identity = reqwest::tls::Identity::from_pem(&identity_pem)
            .context("failed to parse PD client identity for fs9 guard")?;

        builder = builder
            .add_root_certificate(ca_cert)
            .identity(identity)
            .danger_accept_invalid_certs(false);
    }

    builder
        .build()
        .context("failed to build PD HTTP client for fs9 guard")
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
        anyhow::bail!("no PD endpoints available for fs9 legacy keyspace probe");
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
        "failed to determine whether legacy JuiceFS keyspace `{}` exists via PD endpoints [{}]: {}",
        keyspace,
        pd_endpoints.join(", "),
        errors.join("; ")
    )
}

pub(crate) async fn ensure_embedded_backend_bootstrap_allowed(
    client: &Arc<tikv_client::TransactionClient>,
    tenant_keyspace: &str,
    pd_endpoints: Option<&[String]>,
) -> Result<()> {
    match crate::extensions::fs::embedded::pagefs::probe_superblock_readonly(client).await {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(err) => {
            anyhow::bail!(
                "fs9: refusing to initialize embedded PageFS for tenant `{}` because existing embedded PageFS metadata is invalid or unavailable: {err}",
                tenant_keyspace
            );
        }
    }

    let markers = legacy_juicefs_config_markers();
    if !markers.is_empty() {
        anyhow::bail!(
            "fs9: refusing to initialize embedded PageFS for tenant `{}` because legacy JuiceFS/fs-plane configuration is still present ({}). JuiceFS backend support was removed from db9-server; migrate/export the legacy fs9 data before enabling embedded PageFS.",
            tenant_keyspace,
            markers.join(", ")
        );
    }

    let endpoints: Cow<'_, [String]> = match pd_endpoints {
        Some(endpoints) if !endpoints.is_empty() => Cow::Borrowed(endpoints),
        _ => Cow::Owned(pd_endpoints_from_env()),
    };

    let legacy_keyspace = legacy_juicefs_keyspace_name(tenant_keyspace);
    match query_pd_keyspace_state(&endpoints, &legacy_keyspace).await {
        Ok(Some(state)) => {
            anyhow::bail!(
                "fs9: refusing to initialize embedded PageFS for tenant `{}` because legacy JuiceFS keyspace `{}` exists in PD with state `{}`. Migrate/export that volume before using embedded PageFS.",
                tenant_keyspace,
                legacy_keyspace,
                state
            );
        }
        Ok(None) => {}
        Err(err) => {
            anyhow::bail!(
                "fs9: refusing to initialize embedded PageFS for tenant `{}` because legacy JuiceFS keyspace probe for `{}` did not reach a definitive not-found result: {err}",
                tenant_keyspace,
                legacy_keyspace
            );
        }
    }

    Ok(())
}

async fn init_backend(tenant_keyspace: &str) -> Result<Arc<dyn FsBackend>> {
    let client = crate::extensions::context::tikv_client().ok_or_else(|| {
        anyhow!(
            "fs9: TiKV client not available in extension context. \
             Ensure the caller wraps this in with_context_opts()."
        )
    })?;

    let pd_endpoints = crate::extensions::context::pd_endpoints();
    ensure_embedded_backend_bootstrap_allowed(&client, tenant_keyspace, pd_endpoints.as_deref())
        .await?;

    EmbeddedFsBackend::new(client, tenant_keyspace.to_string())
        .await
        .map(|b| Arc::new(b) as Arc<dyn FsBackend>)
        .map_err(|e| anyhow!("fs9: failed to init embedded backend: {e}"))
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
    fn legacy_juicefs_config_detects_grpc_pd_endpoints() {
        let markers = legacy_juicefs_config_markers_with(|key| match key {
            "FS9_GRPC_PD_ENDPOINTS" => Some("pd:2379".to_string()),
            _ => None,
        });

        assert_eq!(markers, vec!["FS9_GRPC_PD_ENDPOINTS".to_string()]);
    }

    #[test]
    fn legacy_juicefs_config_detects_explicit_juicefs_backend() {
        let markers = legacy_juicefs_config_markers_with(|key| match key {
            "FS9_BACKEND" => Some("juicefs".to_string()),
            _ => None,
        });

        assert_eq!(markers, vec!["FS9_BACKEND=juicefs".to_string()]);
    }

    #[test]
    fn legacy_juicefs_config_allows_explicit_embedded_backend() {
        let markers = legacy_juicefs_config_markers_with(|key| match key {
            "FS9_BACKEND" => Some("embedded".to_string()),
            _ => None,
        });

        assert!(markers.is_empty());
    }

    #[test]
    fn parse_pd_endpoints_trims_empty_entries() {
        assert_eq!(
            parse_pd_endpoints(" pd1:2379, ,pd2:2379 "),
            vec!["pd1:2379".to_string(), "pd2:2379".to_string()]
        );
    }

    #[test]
    fn legacy_juicefs_keyspace_uses_tenant_suffix() {
        assert_eq!(legacy_juicefs_keyspace_name("tenant_a"), "jfs_t_tenant_a");
    }

    #[test]
    fn legacy_juicefs_keyspace_strips_db9_tenant_prefix() {
        assert_eq!(
            legacy_juicefs_keyspace_name("db9_tenant_tenant_a"),
            "jfs_t_tenant_a"
        );
    }

    #[test]
    fn pd_keyspace_state_response_returns_none_only_for_404() {
        let state = parse_pd_keyspace_state_response(
            "jfs_t_tenant_a",
            "pd:2379",
            StatusCode::NOT_FOUND,
            "not found",
        )
        .expect("404 should be classified as definitive not-found");

        assert_eq!(state, None);
    }

    #[test]
    fn pd_keyspace_state_response_treats_pd_missing_keyspace_500_as_not_found() {
        let state = parse_pd_keyspace_state_response(
            "jfs_t_default",
            "pd:2379",
            StatusCode::INTERNAL_SERVER_ERROR,
            "keyspace does not exist",
        )
        .expect("PD missing-keyspace 500 must be classified as definitive not-found");

        assert_eq!(state, None);
    }

    #[test]
    fn pd_keyspace_state_response_treats_json_string_missing_keyspace_500_as_not_found() {
        let state = parse_pd_keyspace_state_response(
            "jfs_t_default",
            "pd:2379",
            StatusCode::INTERNAL_SERVER_ERROR,
            r#""keyspace does not exist""#,
        )
        .expect("PD JSON-string missing-keyspace 500 must be definitive not-found");

        assert_eq!(state, None);
    }

    #[test]
    fn pd_keyspace_state_response_returns_state_for_success() {
        let state = parse_pd_keyspace_state_response(
            "jfs_t_tenant_a",
            "pd:2379",
            StatusCode::OK,
            r#"{"state":"ENABLED"}"#,
        )
        .expect("valid PD response should parse");

        assert_eq!(state, Some("ENABLED".to_string()));
    }

    #[test]
    fn pd_keyspace_state_response_errors_on_non_404_status() {
        let err = parse_pd_keyspace_state_response(
            "jfs_t_tenant_a",
            "pd:2379",
            StatusCode::INTERNAL_SERVER_ERROR,
            "boom",
        )
        .expect_err("500 must not be classified as not-found");

        assert!(
            err.to_string().contains("HTTP 500"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn pd_keyspace_state_response_errors_on_invalid_json() {
        let err = parse_pd_keyspace_state_response(
            "jfs_t_tenant_a",
            "pd:2379",
            StatusCode::OK,
            "not-json",
        )
        .expect_err("invalid JSON must not be classified as not-found");

        assert!(
            err.to_string().contains("invalid JSON"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn pd_keyspace_state_response_errors_when_state_missing() {
        let err = parse_pd_keyspace_state_response(
            "jfs_t_tenant_a",
            "pd:2379",
            StatusCode::OK,
            r#"{"name":"jfs_t_tenant_a"}"#,
        )
        .expect_err("missing state must not be classified as not-found");

        assert!(
            err.to_string().contains("no string `state`"),
            "unexpected error: {err}"
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
}
