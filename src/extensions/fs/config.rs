use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::sync::OnceLock;
use tracing::warn;

use crate::config;

const DEFAULT_INLINE_MAX_BYTES: usize = 64 * 1024;
const DEFAULT_OBJECT_MIN_BYTES: usize = 1024 * 1024;
const MIN_S3_MULTIPART_PART_BYTES: usize = 5 * 1024 * 1024;

const DEFAULT_S3_PREFIX: &str = "fs9";
const DEFAULT_S3_MULTIPART_PART_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_S3_HEAD_RETRY_ATTEMPTS: u32 = 5;
const DEFAULT_S3_HEAD_RETRY_BASE_MS: u64 = 50;

const DEFAULT_TIKV_COMMIT_RETRY_ATTEMPTS: u32 = 5;
const DEFAULT_TIKV_COMMIT_RETRY_BASE_MS: u64 = 30;

const DEFAULT_GC_INTERVAL_SECS: u64 = 30;
const DEFAULT_GC_INITIAL_JITTER_MS: u64 = 5_000;
const DEFAULT_GC_MAX_BACKOFF_SECS: u64 = 10 * 60;
const DEFAULT_PRESIGN_TTL_SECS: u64 = 15 * 60;
const DEFAULT_UPLOAD_TOKEN_TTL_SECS: u64 = 4 * 60 * 60;
const MAX_UPLOAD_TOKEN_TTL_SECS: u64 = 24 * 60 * 60;
const DEFAULT_MAX_PARTS_PER_UPLOAD: u32 = 10_000;
const DEFAULT_WS_MAX_INFLIGHT_UPLOADS_PER_CONNECTION: usize = 16;
const DEFAULT_WS_MAX_INFLIGHT_REQUESTS_PER_CONNECTION: usize = 32;
const DEFAULT_BATCH_PRESIGN_MAX_PARTS: usize = 500;
const DEFAULT_BATCH_STAT_MAX_FILES: usize = 256;
const DEFAULT_BATCH_STAT_CONCURRENCY: usize = 16;
const DEFAULT_BATCH_INLINE_READ_MAX_FILES: usize = 256;
const DEFAULT_BATCH_INLINE_READ_MAX_TOTAL_BYTES: usize = 1024 * 1024;
const DEFAULT_BATCH_WRITE_MAX_FILES: usize = 32;
const DEFAULT_READDIR_RECURSIVE_MAX_DEPTH: usize = 20;
const DEFAULT_READDIR_RECURSIVE_MAX_ENTRIES: usize = 50_000;
const DEFAULT_READDIR_RECURSIVE_TIMEOUT_SECS: u64 = 30;
const DEFAULT_READDIR_RECURSIVE_MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
// `batch_write_max_total_bytes` is a post-decode safety limit (raw bytes written).
// `batch_write_max_encoded_bytes` is a pre-decode limit on the base64-encoded payload carried
// inside a JSON frame. Under the default WS JSON frame limit (`MAX_JSON_FRAME_BYTES`), the
// encoded limit is expected to trigger first; operators should tune these together if they want
// larger batches.
const DEFAULT_BATCH_WRITE_MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_BATCH_WRITE_MAX_ENCODED_BYTES: usize = 1024 * 1024;
const DEFAULT_PACK_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// Which backend actually serves a tenant's fs9 I/O.
///
/// The choice is determined per-tenant by `resolve_backend_type_core` in
/// `backend.rs`: if the tenant's TiKV keyspace already holds any
/// embedded fs9 metadata (checked via
/// [`embedded::pagefs::probe_keyspace_has_embedded_data`], which mirrors
/// the embedded backend's own re-init guard) it's Embedded; otherwise
/// (and when the gRPC proxy is configured) it's JuiceFs. There is
/// deliberately no operator-facing knob — explicit `FS9_BACKEND=juicefs`
/// once silently routed tenants with existing PACK-format embedded data
/// to JuiceFs, producing `fs9_size` hits plus `fs9_read_at` I/O errors
/// because JuiceFs looked in `chunks/...` for data that lived at
/// `<hex-tenant>/<volume>/objects/...`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fs9BackendType {
    /// Direct TiKV storage (InlineBlob/PackEntry/Object). The original embedded backend.
    Embedded,
    /// JuiceFS via the fs9 gRPC proxy (db9-ai/fs9).
    JuiceFs,
}

#[derive(Debug, Clone)]
pub(crate) struct Fs9Config {
    pub(crate) grpc_socket: String,
    pub(crate) grpc_pd_endpoints: String,

    /// mTLS config for outgoing fs9 gRPC client.
    ///
    /// * `Ok(Some(_))` — TLS enabled and every contract satisfied.
    /// * `Ok(None)` — TLS disabled (backward compatible with pre-mTLS fs9).
    /// * `Err(message)` — `FS9_TLS=true` was set but something about the
    ///   config violates the fail-closed contract (missing path,
    ///   unreadable file, http:// scheme, UDS addr, …). The error is
    ///   stored rather than panicked so it only surfaces when gRPC is
    ///   actually used — operators running Embedded-only backends with a
    ///   misconfigured `FS9_TLS=true` don't get their whole server
    ///   crashed, but any fs9 gRPC connect returns this error verbatim.
    ///
    /// Paired with fs9 PR db9-ai/fs9#26 which adds the gRPC server
    /// mTLS listener. Both sides ship with their TLS flags off;
    /// flip via env after both images are deployed.
    pub(crate) grpc_tls: std::result::Result<Option<Fs9GrpcTls>, String>,
    pub(crate) inline_max_bytes: usize,
    pub(crate) object_min_bytes: usize,
    pub(crate) s3: Option<Fs9S3Config>,
    pub(crate) tikv_commit_retry_attempts: u32,
    pub(crate) tikv_commit_retry_base_ms: u64,
    pub(crate) gc_interval_secs: u64,
    pub(crate) gc_initial_jitter_ms: u64,
    pub(crate) gc_max_backoff_secs: u64,
    pub(crate) presign_ttl_secs: u64,
    pub(crate) upload_token_ttl_secs: u64,
    pub(crate) max_parts_per_upload: u32,
    pub(crate) upload_token_secret: Option<String>,
    pub(crate) ws_max_inflight_uploads_per_connection: usize,
    pub(crate) ws_max_inflight_requests_per_connection: usize,
    pub(crate) batch_presign_max_parts: usize,
    pub(crate) batch_stat_max_files: usize,
    pub(crate) batch_stat_concurrency: usize,
    pub(crate) batch_inline_read_max_files: usize,
    pub(crate) batch_inline_read_max_file_bytes: usize,
    pub(crate) batch_inline_read_max_total_bytes: usize,
    pub(crate) batch_write_max_files: usize,
    pub(crate) batch_write_max_total_bytes: usize,
    pub(crate) batch_write_max_encoded_bytes: usize,
    /// Maximum files per directory subgroup in batch_write_atomic.
    /// Files sharing a parent dir are committed atomically in a single TiKV txn.
    /// Large directory groups are split into chunks of this size.
    /// Default: 32 (per micro-benchmark on local single-node TiKV).
    pub(crate) grouped_write_subgroup_size: usize,
    pub(crate) readdir_recursive_max_depth: usize,
    pub(crate) readdir_recursive_max_entries: usize,
    pub(crate) readdir_recursive_timeout_secs: u64,
    pub(crate) readdir_recursive_max_response_bytes: usize,
    pub(crate) pack_spool_root: PathBuf,
    pub(crate) pack_cache_bytes: usize,
}

/// mTLS material for the outgoing fs9 gRPC client. db9-server reuses its
/// TiKV client identity (`TIKV_CA_PATH` / `TIKV_CERT_PATH` / `TIKV_KEY_PATH`)
/// — the cluster CA signs both directions, fs9's `CAPath` defaults to the
/// same `/tls/ca.crt` mount, and a db9-server pod's outbound identity is
/// the identity it already presents to TiKV.
#[derive(Debug, Clone)]
pub(crate) struct Fs9GrpcTls {
    pub(crate) ca_path: String,
    pub(crate) cert_path: String,
    pub(crate) key_path: String,
    /// Server SNI / ServerName to verify fs9's cert against. Must
    /// match one of the SANs in fs9-server-tls (fs9 / fs9.db9 /
    /// fs9.db9.svc / fs9.db9.svc.cluster.local). Default "fs9" since
    /// staging dials by short service name.
    pub(crate) server_name: String,
}

impl Fs9GrpcTls {
    /// Pure constructor — encodes the fail-closed mTLS contract:
    ///
    /// * `FS9_TLS=false` → `Ok(None)` regardless of other inputs.
    /// * `FS9_TLS=true` requires all three cert paths to be set and
    ///   readable; missing any → `Err`.
    /// * Rejects `http://` addrs when TLS is on (tonic routes on URI
    ///   scheme, so `http://…` with `tls_config` attached would still
    ///   go plaintext — silent downgrade).
    /// * Rejects Unix-socket addrs when TLS is on — fs9's gRPC server
    ///   shares one `grpc.Server` across UDS and TCP listeners, so
    ///   `--tls=true` on the server means the UDS client must also
    ///   present a cert. This client's UDS branch does not wire TLS,
    ///   so the combination cannot succeed; fail fast with a clear
    ///   error instead of a handshake failure at first RPC.
    fn build(
        tls_enabled: bool,
        grpc_socket: &str,
        ca_path: Option<&str>,
        cert_path: Option<&str>,
        key_path: Option<&str>,
        server_name: Option<&str>,
    ) -> Result<Option<Self>> {
        if !tls_enabled {
            return Ok(None);
        }
        let ca_path = require_path("TIKV_CA_PATH", ca_path)?;
        let cert_path = require_path("TIKV_CERT_PATH", cert_path)?;
        let key_path = require_path("TIKV_KEY_PATH", key_path)?;

        // Compare scheme prefixes case-insensitively — `HTTP://…` and
        // `Http://…` are the same downgrade risk as `http://…`.
        let socket_lc = grpc_socket.to_ascii_lowercase();
        if socket_lc.starts_with("http://") {
            return Err(anyhow!(
                "FS9_TLS=true but FS9_GRPC_ADDR has http:// scheme \
                 (`{grpc_socket}`); tonic would dial plaintext despite \
                 the TLS config. Use `https://host:port` or `host:port`."
            ));
        }
        if grpc_socket.starts_with('/') || socket_lc.starts_with("unix://") {
            return Err(anyhow!(
                "FS9_TLS=true but FS9_GRPC_ADDR is a Unix socket \
                 (`{grpc_socket}`); fs9's server shares one grpc.Server \
                 across UDS and TCP, so TLS-on requires UDS clients to \
                 present a cert too — which this client does not wire. \
                 Use a TCP address."
            ));
        }

        // Eager readability check — matches TiKV SecurityManager's
        // load-time pattern so mTLS misconfig fails at startup, not at
        // first RPC.
        check_pem_readable("TIKV_CA_PATH", ca_path)?;
        check_pem_readable("TIKV_CERT_PATH", cert_path)?;
        check_pem_readable("TIKV_KEY_PATH", key_path)?;

        Ok(Some(Self {
            ca_path: ca_path.to_string(),
            cert_path: cert_path.to_string(),
            key_path: key_path.to_string(),
            server_name: server_name
                .filter(|s| !s.is_empty())
                .unwrap_or("fs9")
                .to_string(),
        }))
    }
}

fn require_path<'a>(env: &str, val: Option<&'a str>) -> Result<&'a str> {
    val.filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("FS9_TLS=true requires {env} to be set"))
}

fn check_pem_readable(env: &str, path: &str) -> Result<()> {
    // `File::open` is enough to prove read access; avoids loading the
    // full file at config parse time (a symlink to a huge file would
    // otherwise OOM the process).
    std::fs::File::open(path)
        .map(drop)
        .map_err(|e| anyhow!("FS9_TLS: {env}={path} is not readable: {e}"))
}

#[derive(Debug, Clone)]
pub(crate) struct Fs9S3Config {
    pub(crate) bucket: String,
    pub(crate) region: Option<String>,
    pub(crate) endpoint: Option<String>,
    pub(crate) prefix: String,
    pub(crate) force_path_style: bool,
    pub(crate) multipart_part_bytes: usize,
    pub(crate) head_retry_attempts: u32,
    pub(crate) head_retry_base_ms: u64,
}

impl Fs9Config {
    pub(crate) fn from_env() -> Result<Self> {
        // Backend selection is per-tenant, probed via
        // pagefs::probe_keyspace_has_embedded_data; see Fs9BackendType
        // docstring. FS9_BACKEND env var is intentionally no longer
        // read — operator-facing override invited silent mis-routing of
        // tenants with existing embedded data.
        if let Some(val) = config::env_string("FS9_BACKEND") {
            warn!(
                "FS9_BACKEND={val:?} is set but ignored — backend is resolved per-tenant \
                 by probing TiKV. Remove the env var to silence this warning."
            );
        }
        // Accepts Unix socket path (/var/run/fs9/fs9.sock), unix:// URI, or TCP (http://host:port, host:port)
        let grpc_socket = config::env_string("FS9_GRPC_ADDR")
            .or_else(|| config::env_string("FS9_GRPC_SOCKET"))
            .unwrap_or_else(|| "/var/run/fs9/fs9.sock".to_string());
        let grpc_pd_endpoints = config::env_string("FS9_GRPC_PD_ENDPOINTS").unwrap_or_default();

        // mTLS for outgoing fs9 gRPC client. Fail-closed: when
        // FS9_TLS=true, any missing / unreadable cert path is a hard
        // error — stored and surfaced from `create_channel()` instead
        // of panicked here, so an Embedded-only operator with an
        // accidental `FS9_TLS=true` doesn't crash the whole server.
        // Uses the same `TIKV_CA_PATH` / `TIKV_CERT_PATH` / `TIKV_KEY_PATH`
        // mount the TiKV client already consumes — db9-server's
        // outbound identity to fs9 is its outbound identity to TiKV.
        let grpc_tls = Fs9GrpcTls::build(
            config::env_bool("FS9_TLS"),
            &grpc_socket,
            config::env_string("TIKV_CA_PATH").as_deref(),
            config::env_string("TIKV_CERT_PATH").as_deref(),
            config::env_string("TIKV_KEY_PATH").as_deref(),
            config::env_string("FS9_TLS_SERVER_NAME").as_deref(),
        )
        .map_err(|e| format!("{e:#}"));

        let inline_max_bytes = config::env_string("FS9_INLINE_MAX")
            .and_then(|v| parse_bytes(&v))
            .unwrap_or(DEFAULT_INLINE_MAX_BYTES);

        let object_min_bytes = config::env_string("FS9_OBJECT_MIN")
            .and_then(|v| parse_bytes(&v))
            .unwrap_or(DEFAULT_OBJECT_MIN_BYTES);

        let batch_inline_read_max_file_bytes =
            config::env_string("FS9_BATCH_INLINE_READ_MAX_FILE_BYTES")
                .and_then(|v| parse_bytes(&v))
                .unwrap_or(inline_max_bytes);

        let s3 = config::env_string("FS9_S3_BUCKET").map(|bucket| {
            let prefix = config::env_string("FS9_S3_PREFIX")
                .unwrap_or_else(|| DEFAULT_S3_PREFIX.to_string());
            let prefix = prefix.trim_matches('/').to_string();
            let multipart_part_bytes = config::env_string("FS9_S3_MULTIPART_PART")
                .and_then(|v| parse_bytes(&v))
                .unwrap_or(DEFAULT_S3_MULTIPART_PART_BYTES);
            let multipart_part_bytes =
                normalize_s3_multipart_part_bytes(multipart_part_bytes, "FS9_S3_MULTIPART_PART");

            Fs9S3Config {
                bucket,
                region: config::env_string("FS9_S3_REGION")
                    .or_else(|| config::env_string("AWS_REGION"))
                    .or_else(|| config::env_string("AWS_DEFAULT_REGION")),
                endpoint: config::env_string("FS9_S3_ENDPOINT"),
                prefix,
                force_path_style: config::env_bool("FS9_S3_FORCE_PATH_STYLE"),
                multipart_part_bytes,
                head_retry_attempts: config::env_string("FS9_S3_HEAD_RETRY_ATTEMPTS")
                    .and_then(|v| v.parse::<u32>().ok())
                    .filter(|v| *v > 0)
                    .unwrap_or(DEFAULT_S3_HEAD_RETRY_ATTEMPTS),
                head_retry_base_ms: config::env_string("FS9_S3_HEAD_RETRY_BASE_MS")
                    .and_then(|v| v.parse::<u64>().ok())
                    .filter(|v| *v > 0)
                    .unwrap_or(DEFAULT_S3_HEAD_RETRY_BASE_MS),
            }
        });

        Ok(Self {
            grpc_socket,
            grpc_pd_endpoints,
            grpc_tls,
            inline_max_bytes,
            object_min_bytes,
            s3,
            tikv_commit_retry_attempts: config::env_string("FS9_TIKV_COMMIT_RETRY_ATTEMPTS")
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_TIKV_COMMIT_RETRY_ATTEMPTS),
            tikv_commit_retry_base_ms: config::env_string("FS9_TIKV_COMMIT_RETRY_BASE_MS")
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_TIKV_COMMIT_RETRY_BASE_MS),
            gc_interval_secs: config::env_string("FS9_GC_INTERVAL_SECS")
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_GC_INTERVAL_SECS),
            gc_initial_jitter_ms: config::env_string("FS9_GC_INITIAL_JITTER_MS")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(DEFAULT_GC_INITIAL_JITTER_MS),
            gc_max_backoff_secs: config::env_string("FS9_GC_MAX_BACKOFF_SECS")
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_GC_MAX_BACKOFF_SECS),
            presign_ttl_secs: config::env_string("FS9_PRESIGN_TTL_SECS")
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_PRESIGN_TTL_SECS),
            upload_token_ttl_secs: config::env_string("FS9_UPLOAD_TOKEN_TTL_SECS")
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|v| *v > 0)
                .map(|v| v.min(MAX_UPLOAD_TOKEN_TTL_SECS))
                .unwrap_or(DEFAULT_UPLOAD_TOKEN_TTL_SECS),
            max_parts_per_upload: config::env_string("FS9_MAX_PARTS_PER_UPLOAD")
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|v| *v > 0)
                .map(|v| v.min(10_000))
                .unwrap_or(DEFAULT_MAX_PARTS_PER_UPLOAD),
            upload_token_secret: config::env_string("FS9_UPLOAD_TOKEN_SECRET"),
            ws_max_inflight_uploads_per_connection: config::env_string(
                "FS9_WS_MAX_INFLIGHT_UPLOADS_PER_CONNECTION",
            )
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_WS_MAX_INFLIGHT_UPLOADS_PER_CONNECTION),
            ws_max_inflight_requests_per_connection: config::env_string(
                "FS9_WS_MAX_INFLIGHT_REQUESTS_PER_CONNECTION",
            )
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_WS_MAX_INFLIGHT_REQUESTS_PER_CONNECTION),
            batch_presign_max_parts: config::env_string("FS9_BATCH_PRESIGN_MAX_PARTS")
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_BATCH_PRESIGN_MAX_PARTS),
            batch_stat_max_files: config::env_string("FS9_BATCH_STAT_MAX_FILES")
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_BATCH_STAT_MAX_FILES),
            batch_stat_concurrency: config::env_string("FS9_BATCH_STAT_CONCURRENCY")
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_BATCH_STAT_CONCURRENCY),
            batch_inline_read_max_files: config::env_string("FS9_BATCH_INLINE_READ_MAX_FILES")
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_BATCH_INLINE_READ_MAX_FILES),
            batch_inline_read_max_file_bytes,
            batch_inline_read_max_total_bytes: config::env_string(
                "FS9_BATCH_INLINE_READ_MAX_TOTAL_BYTES",
            )
            .and_then(|v| parse_bytes(&v))
            .unwrap_or(DEFAULT_BATCH_INLINE_READ_MAX_TOTAL_BYTES),
            batch_write_max_files: config::env_string("FS9_BATCH_WRITE_MAX_FILES")
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_BATCH_WRITE_MAX_FILES),
            batch_write_max_total_bytes: config::env_string("FS9_BATCH_WRITE_MAX_TOTAL_BYTES")
                .and_then(|v| parse_bytes(&v))
                .unwrap_or(DEFAULT_BATCH_WRITE_MAX_TOTAL_BYTES),
            batch_write_max_encoded_bytes: config::env_string("FS9_BATCH_WRITE_MAX_ENCODED_BYTES")
                .and_then(|v| parse_bytes(&v))
                .unwrap_or(DEFAULT_BATCH_WRITE_MAX_ENCODED_BYTES),
            grouped_write_subgroup_size: config::env_string("FS9_GROUPED_WRITE_SUBGROUP_SIZE")
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(32),
            readdir_recursive_max_depth: config::env_string("FS9_READDIR_RECURSIVE_MAX_DEPTH")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(DEFAULT_READDIR_RECURSIVE_MAX_DEPTH),
            readdir_recursive_max_entries: config::env_string("FS9_READDIR_RECURSIVE_MAX_ENTRIES")
                .and_then(|v| v.parse::<usize>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(DEFAULT_READDIR_RECURSIVE_MAX_ENTRIES),
            readdir_recursive_timeout_secs: config::env_string(
                "FS9_READDIR_RECURSIVE_TIMEOUT_SECS",
            )
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_READDIR_RECURSIVE_TIMEOUT_SECS),
            readdir_recursive_max_response_bytes: config::env_string(
                "FS9_READDIR_RECURSIVE_MAX_RESPONSE_BYTES",
            )
            .and_then(|v| parse_bytes(&v))
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_READDIR_RECURSIVE_MAX_RESPONSE_BYTES),
            pack_spool_root: config::env_string("FS9_PACK_SPOOL_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("db9-fs9-pack-spool")),
            pack_cache_bytes: config::env_string("FS9_PACK_CACHE_BYTES")
                .and_then(|v| parse_bytes(&v))
                .unwrap_or(DEFAULT_PACK_CACHE_BYTES),
        })
    }
}

static FS9_CONFIG: OnceLock<Fs9Config> = OnceLock::new();

/// Returns the process-global fs9 config, parsing it lazily on first
/// call. Non-TLS config errors propagate via `.expect`; TLS-specific
/// misconfig is captured in `grpc_tls: Result<_,_>` and surfaced only
/// at `create_channel()` so Embedded-only users aren't crashed by an
/// accidental `FS9_TLS=true`.
pub(crate) fn fs9_config() -> &'static Fs9Config {
    FS9_CONFIG.get_or_init(|| Fs9Config::from_env().expect("fs9 configuration error (non-TLS)"))
}

fn normalize_s3_multipart_part_bytes(value: usize, env_name: &str) -> usize {
    if value < MIN_S3_MULTIPART_PART_BYTES {
        warn!(
            "{env_name}={} is below the S3 multipart minimum of {} bytes; clamping to {} bytes",
            value, MIN_S3_MULTIPART_PART_BYTES, MIN_S3_MULTIPART_PART_BYTES
        );
        MIN_S3_MULTIPART_PART_BYTES
    } else {
        value
    }
}

fn parse_bytes(raw: &str) -> Option<usize> {
    let mut s = raw.trim().to_ascii_lowercase();
    s.retain(|c| !c.is_whitespace());
    if s.is_empty() {
        return None;
    }

    let mut split = 0usize;
    for (idx, c) in s.chars().enumerate() {
        if !(c.is_ascii_digit() || c == '_') {
            split = idx;
            break;
        }
    }

    let (num_raw, suffix) = if split == 0 {
        // Entire string is digits/underscores or starts with a suffix (invalid).
        if s.chars().all(|c| c.is_ascii_digit() || c == '_') {
            (s.as_str(), "")
        } else {
            return None;
        }
    } else {
        (&s[..split], &s[split..])
    };

    let num_clean: String = num_raw.chars().filter(|c| *c != '_').collect();
    let value: u64 = num_clean.parse().ok()?;

    let multiplier: u64 = match suffix {
        "" | "b" => 1,
        "k" | "kb" => 1_000,
        "ki" | "kib" => 1024,
        "m" | "mb" => 1_000_000,
        "mi" | "mib" => 1024 * 1024,
        "g" | "gb" => 1_000_000_000,
        "gi" | "gib" => 1024 * 1024 * 1024,
        _ => return None,
    };

    value
        .checked_mul(multiplier)
        .and_then(|v| usize::try_from(v).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bytes_accepts_plain_integer() {
        assert_eq!(parse_bytes("0"), Some(0));
        assert_eq!(parse_bytes("123"), Some(123));
        assert_eq!(parse_bytes("65_536"), Some(65536));
    }

    #[test]
    fn parse_bytes_accepts_suffixes_case_insensitive() {
        assert_eq!(parse_bytes("64KiB"), Some(64 * 1024));
        assert_eq!(parse_bytes("4MiB"), Some(4 * 1024 * 1024));
        assert_eq!(parse_bytes("1gib"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_bytes("10MB"), Some(10 * 1_000_000));
    }

    #[test]
    fn parse_bytes_rejects_invalid_values() {
        assert_eq!(parse_bytes(""), None);
        assert_eq!(parse_bytes("kib"), None);
        assert_eq!(parse_bytes("12xy"), None);
    }

    #[test]
    fn normalize_s3_multipart_part_bytes_clamps_too_small_values() {
        assert_eq!(
            normalize_s3_multipart_part_bytes(128 * 1024, "FS9_S3_MULTIPART_PART"),
            MIN_S3_MULTIPART_PART_BYTES
        );
        assert_eq!(
            normalize_s3_multipart_part_bytes(MIN_S3_MULTIPART_PART_BYTES, "X"),
            MIN_S3_MULTIPART_PART_BYTES
        );
        assert_eq!(
            normalize_s3_multipart_part_bytes(6 * 1024 * 1024, "X"),
            6 * 1024 * 1024
        );
    }

    // ---------------------------------------------------------------
    // Fs9GrpcTls::build — fail-closed mTLS contract coverage.
    //
    // Every test drives `Fs9GrpcTls::build` with explicit arguments, so
    // no process-wide env mutation is needed and tests are safe to run
    // in parallel.
    // ---------------------------------------------------------------

    /// Write three readable dummy PEM files in a unique temp dir and
    /// return their paths. Lives only as long as the caller keeps the
    /// returned `_Guard` — we clean up on drop.
    struct TlsPaths {
        dir: PathBuf,
        ca: String,
        cert: String,
        key: String,
    }
    impl Drop for TlsPaths {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
    fn write_dummy_pems() -> TlsPaths {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let idx = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("db9-fs9tls-{nanos}-{idx}"));
        std::fs::create_dir_all(&dir).unwrap();
        let ca = dir.join("ca.crt");
        let cert = dir.join("tls.crt");
        let key = dir.join("tls.key");
        std::fs::write(&ca, b"ca").unwrap();
        std::fs::write(&cert, b"cert").unwrap();
        std::fs::write(&key, b"key").unwrap();
        TlsPaths {
            ca: ca.to_string_lossy().into_owned(),
            cert: cert.to_string_lossy().into_owned(),
            key: key.to_string_lossy().into_owned(),
            dir,
        }
    }

    #[test]
    fn tls_disabled_returns_none_regardless_of_paths() {
        let p = write_dummy_pems();
        let out = Fs9GrpcTls::build(
            false,
            "fs9:50051",
            Some(&p.ca),
            Some(&p.cert),
            Some(&p.key),
            None,
        )
        .unwrap();
        assert!(out.is_none());
        // Even with the kitchen sink of bad inputs, TLS-off is a no-op.
        let out = Fs9GrpcTls::build(false, "http://bad", None, None, None, None).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn tls_enabled_succeeds_with_readable_paths_and_tcp_addr() {
        let p = write_dummy_pems();
        let out = Fs9GrpcTls::build(
            true,
            "fs9:50051",
            Some(&p.ca),
            Some(&p.cert),
            Some(&p.key),
            Some("fs9"),
        )
        .unwrap();
        let tls = out.expect("grpc_tls populated");
        assert_eq!(tls.ca_path, p.ca);
        assert_eq!(tls.cert_path, p.cert);
        assert_eq!(tls.key_path, p.key);
        assert_eq!(tls.server_name, "fs9");
    }

    #[test]
    fn tls_enabled_defaults_server_name_to_fs9() {
        let p = write_dummy_pems();
        let tls = Fs9GrpcTls::build(
            true,
            "fs9:50051",
            Some(&p.ca),
            Some(&p.cert),
            Some(&p.key),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(tls.server_name, "fs9");
    }

    #[test]
    fn tls_enabled_missing_any_path_errors() {
        let p = write_dummy_pems();
        for (ca, cert, key, which) in [
            (
                None,
                Some(p.cert.as_str()),
                Some(p.key.as_str()),
                "TIKV_CA_PATH",
            ),
            (
                Some(p.ca.as_str()),
                None,
                Some(p.key.as_str()),
                "TIKV_CERT_PATH",
            ),
            (
                Some(p.ca.as_str()),
                Some(p.cert.as_str()),
                None,
                "TIKV_KEY_PATH",
            ),
        ] {
            let err = Fs9GrpcTls::build(true, "fs9:50051", ca, cert, key, None).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains(which),
                "expected error to mention {which}, got: {msg}"
            );
        }
    }

    #[test]
    fn tls_enabled_empty_string_treated_as_missing() {
        let p = write_dummy_pems();
        let err = Fs9GrpcTls::build(
            true,
            "fs9:50051",
            Some(""),
            Some(&p.cert),
            Some(&p.key),
            None,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("TIKV_CA_PATH"));
    }

    #[test]
    fn tls_enabled_rejects_http_scheme() {
        let p = write_dummy_pems();
        let err = Fs9GrpcTls::build(
            true,
            "http://fs9:50051",
            Some(&p.ca),
            Some(&p.cert),
            Some(&p.key),
            None,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("http://"));
    }

    #[test]
    fn tls_enabled_rejects_unix_socket_path() {
        let p = write_dummy_pems();
        let err = Fs9GrpcTls::build(
            true,
            "/var/run/fs9/fs9.sock",
            Some(&p.ca),
            Some(&p.cert),
            Some(&p.key),
            None,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("Unix socket"));
    }

    #[test]
    fn tls_enabled_rejects_unix_uri() {
        let p = write_dummy_pems();
        let err = Fs9GrpcTls::build(
            true,
            "unix:///var/run/fs9/fs9.sock",
            Some(&p.ca),
            Some(&p.cert),
            Some(&p.key),
            None,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("Unix socket"));
    }

    #[test]
    fn tls_enabled_rejects_unreadable_cert_file() {
        let err = Fs9GrpcTls::build(
            true,
            "fs9:50051",
            Some("/definitely/does/not/exist/ca.crt"),
            Some("/definitely/does/not/exist/tls.crt"),
            Some("/definitely/does/not/exist/tls.key"),
            None,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not readable"),
            "expected readability error, got: {msg}"
        );
    }

    #[test]
    fn tls_enabled_rejects_uppercase_http_scheme() {
        // Defense-in-depth: a careless operator writing `HTTP://` must
        // not slip past the config gate just because of case.
        let p = write_dummy_pems();
        for bad in ["HTTP://fs9:50051", "Http://fs9:50051", "hTTp://fs9:50051"] {
            let err = Fs9GrpcTls::build(true, bad, Some(&p.ca), Some(&p.cert), Some(&p.key), None)
                .unwrap_err();
            assert!(
                format!("{err:#}").contains("http://"),
                "expected http-scheme rejection for {bad}"
            );
        }
    }

    #[test]
    fn tls_enabled_rejects_uppercase_unix_uri() {
        let p = write_dummy_pems();
        let err = Fs9GrpcTls::build(
            true,
            "UNIX:///var/run/fs9/fs9.sock",
            Some(&p.ca),
            Some(&p.cert),
            Some(&p.key),
            None,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("Unix socket"));
    }

    #[test]
    fn tls_enabled_accepts_https_prefix() {
        // https:// is a legitimate operator form; only http:// is rejected.
        let p = write_dummy_pems();
        let tls = Fs9GrpcTls::build(
            true,
            "https://fs9:50051",
            Some(&p.ca),
            Some(&p.cert),
            Some(&p.key),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(tls.ca_path, p.ca);
    }
}
