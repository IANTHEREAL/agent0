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
const DEFAULT_MAX_PARTS_PER_UPLOAD: u32 = 10_000;
const DEFAULT_WS_MAX_INFLIGHT_UPLOADS_PER_CONNECTION: usize = 16;
const DEFAULT_WS_MAX_INFLIGHT_REQUESTS_PER_CONNECTION: usize = 32;
const DEFAULT_BATCH_STAT_MAX_FILES: usize = 256;
const DEFAULT_BATCH_STAT_CONCURRENCY: usize = 16;
const DEFAULT_BATCH_INLINE_READ_MAX_FILES: usize = 256;
const DEFAULT_BATCH_INLINE_READ_MAX_TOTAL_BYTES: usize = 1024 * 1024;
const DEFAULT_BATCH_WRITE_MAX_FILES: usize = 32;
// `batch_write_max_total_bytes` is a post-decode safety limit (raw bytes written).
// `batch_write_max_encoded_bytes` is a pre-decode limit on the base64-encoded payload carried
// inside a JSON frame. Under the default WS JSON frame limit (`MAX_JSON_FRAME_BYTES`), the
// encoded limit is expected to trigger first; operators should tune these together if they want
// larger batches.
const DEFAULT_BATCH_WRITE_MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_BATCH_WRITE_MAX_ENCODED_BYTES: usize = 1024 * 1024;
const DEFAULT_PACK_CACHE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct Fs9Config {
    pub(crate) inline_max_bytes: usize,
    pub(crate) object_min_bytes: usize,
    pub(crate) s3: Option<Fs9S3Config>,
    pub(crate) tikv_commit_retry_attempts: u32,
    pub(crate) tikv_commit_retry_base_ms: u64,
    pub(crate) gc_interval_secs: u64,
    pub(crate) gc_initial_jitter_ms: u64,
    pub(crate) gc_max_backoff_secs: u64,
    pub(crate) presign_ttl_secs: u64,
    pub(crate) max_parts_per_upload: u32,
    pub(crate) upload_token_secret: Option<String>,
    pub(crate) ws_max_inflight_uploads_per_connection: usize,
    pub(crate) ws_max_inflight_requests_per_connection: usize,
    pub(crate) batch_stat_max_files: usize,
    pub(crate) batch_stat_concurrency: usize,
    pub(crate) batch_inline_read_max_files: usize,
    pub(crate) batch_inline_read_max_file_bytes: usize,
    pub(crate) batch_inline_read_max_total_bytes: usize,
    pub(crate) batch_write_max_files: usize,
    pub(crate) batch_write_max_total_bytes: usize,
    pub(crate) batch_write_max_encoded_bytes: usize,
    pub(crate) pack_spool_root: PathBuf,
    pub(crate) pack_cache_bytes: usize,
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
    pub(crate) fn from_env() -> Self {
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

        Self {
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
            pack_spool_root: config::env_string("FS9_PACK_SPOOL_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("db9-fs9-pack-spool")),
            pack_cache_bytes: config::env_string("FS9_PACK_CACHE_BYTES")
                .and_then(|v| parse_bytes(&v))
                .unwrap_or(DEFAULT_PACK_CACHE_BYTES),
        }
    }
}

static FS9_CONFIG: OnceLock<Fs9Config> = OnceLock::new();

pub(crate) fn fs9_config() -> &'static Fs9Config {
    FS9_CONFIG.get_or_init(Fs9Config::from_env)
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
}
