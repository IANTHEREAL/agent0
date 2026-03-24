//! HNSW S3 client for offloading graph blobs to object storage.
//!
//! When the `HNSW_S3_BUCKET` env var is set, HNSW graph data is stored in S3
//! (or an S3-compatible service) in addition to TiKV. This acts as a warm
//! cache tier between TiKV page storage and the process-level in-memory cache,
//! enabling fast graph loads without reading all pages from TiKV.
//!
//! S3 key format:
//!   `{prefix}/{hex(keyspace)}/{db_id}/{table_id}/{index_id}/graph_v{version}.usearch`
//!
//! Multi-tenancy: the keyspace is hex-encoded to ensure safe S3 key characters
//! and provide per-tenant isolation in the object namespace.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, Context};
use aws_config::BehaviorVersion;
use aws_sdk_s3::config::timeout::TimeoutConfig;
use aws_sdk_s3::config::Builder as S3ConfigBuilder;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use bytes::Bytes;
use tracing::{debug, info, warn};

use crate::config;

/// Default S3 key prefix for HNSW graph objects.
const DEFAULT_PREFIX: &str = "hnsw";

/// Maximum number of objects per DeleteObjects request (S3 limit is 1000).
#[allow(dead_code)] // Used by GC sweep (Phase 4), not yet wired
const DELETE_BATCH_SIZE: usize = 1000;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Parsed HNSW S3 configuration from environment variables.
pub(crate) struct HnswS3Config {
    pub bucket: String,
    pub region: Option<String>,
    pub endpoint: Option<String>,
    pub prefix: String,
    pub force_path_style: bool,
}

impl HnswS3Config {
    /// Read configuration from env vars.
    ///
    /// Returns `None` if `HNSW_S3_BUCKET` is not set (S3 offload disabled).
    fn from_env() -> Option<Self> {
        let bucket = config::env_string("HNSW_S3_BUCKET")?;
        let prefix =
            config::env_string("HNSW_S3_PREFIX").unwrap_or_else(|| DEFAULT_PREFIX.to_string());
        let prefix = prefix.trim_matches('/').to_string();

        Some(Self {
            bucket,
            region: config::env_string("HNSW_S3_REGION")
                .or_else(|| config::env_string("AWS_REGION"))
                .or_else(|| config::env_string("AWS_DEFAULT_REGION")),
            endpoint: config::env_string("HNSW_S3_ENDPOINT"),
            prefix,
            force_path_style: config::env_bool("HNSW_S3_FORCE_PATH_STYLE"),
        })
    }
}

// ---------------------------------------------------------------------------
// S3ObjectInfo (for GC listing)
// ---------------------------------------------------------------------------

/// Metadata about an S3 object, returned by [`HnswS3Client::list_objects`].
#[allow(dead_code)] // Used by GC sweep (Phase 4), not yet wired
pub(crate) struct S3ObjectInfo {
    pub key: String,
    pub last_modified: SystemTime,
    pub size: u64,
}

// ---------------------------------------------------------------------------
// Process-level LRU cache for HNSW graph files
// ---------------------------------------------------------------------------

/// Default maximum number of cached graph files.
const DEFAULT_CACHE_MAX_ENTRIES: usize = 64;

/// Monotonic counter for generating unique temp file names during cache insert.
static CACHE_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Cache key: (keyspace, db_id, table_id, index_id).
/// Keyspace is required because IDs are per-keyspace in multi-tenant mode.
type CacheKey = (String, u64, u64, u64);

/// Process-level LRU cache for HNSW graph files on disk.
///
/// Stores persistent files on disk, keyed by `(keyspace, db_id, table_id, index_id)`.
/// This is the L2 cache (disk); the L1 cache is [`HnswIndexCache`] which stores
/// loaded `Index` objects in memory, shared across concurrent queries.
pub(crate) struct HnswGraphCache {
    entries: Mutex<HashMap<CacheKey, CacheEntry>>,
    cache_dir: PathBuf,
    max_entries: usize,
}

struct CacheEntry {
    graph_version: u64,
    path: PathBuf,
    last_access: Instant,
}

impl HnswGraphCache {
    /// Create a new cache and clean up any stale files from a prior process.
    fn new(cache_dir: PathBuf, max_entries: usize) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&cache_dir).with_context(|| {
            format!(
                "hnsw-cache: failed to create cache directory {:?}",
                cache_dir
            )
        })?;

        // Startup cleanup: delete leftover .usearch cache files from prior runs.
        // Pattern: *_v*.usearch (matches our persistent naming scheme).
        let mut cleaned = 0u32;
        if let Ok(entries) = std::fs::read_dir(&cache_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.ends_with(".usearch")
                    && name_str.contains("_v")
                    && std::fs::remove_file(entry.path()).is_ok()
                {
                    cleaned += 1;
                }
            }
        }
        if cleaned > 0 {
            info!(cleaned, dir = %cache_dir.display(), "hnsw-cache: cleaned stale files on startup");
        }

        Ok(Self {
            entries: Mutex::new(HashMap::new()),
            cache_dir,
            max_entries,
        })
    }

    /// Look up a cached graph file. Returns the path if the cached version matches
    /// `expected_version`. Evicts stale entries (version mismatch).
    pub(crate) fn lookup(
        &self,
        keyspace: &str,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        expected_version: u64,
    ) -> Option<PathBuf> {
        let key = (keyspace.to_string(), db_id, table_id, index_id);
        let mut entries = self.entries.lock().unwrap();

        match entries.get_mut(&key) {
            Some(entry) if entry.graph_version == expected_version => {
                entry.last_access = Instant::now();
                let path = entry.path.clone();
                debug!(
                    keyspace,
                    db_id,
                    table_id,
                    index_id,
                    version = expected_version,
                    "hnsw-cache: hit"
                );
                Some(path)
            }
            Some(entry) => {
                // Version mismatch: evict and delete the stale file.
                let stale_path = entry.path.clone();
                let stale_version = entry.graph_version;
                entries.remove(&key);
                if let Err(e) = std::fs::remove_file(&stale_path) {
                    debug!(
                        path = %stale_path.display(),
                        error = %e,
                        "hnsw-cache: failed to remove stale file (non-fatal)"
                    );
                }
                debug!(
                    keyspace,
                    db_id,
                    table_id,
                    index_id,
                    stale_version,
                    expected_version,
                    "hnsw-cache: evicted stale version"
                );
                None
            }
            None => None,
        }
    }

    /// Insert graph data into the cache. Writes to a temp file, renames to the
    /// persistent path, and evicts the LRU entry if the cache is full.
    /// Returns the persistent file path.
    pub(crate) fn insert(
        &self,
        keyspace: &str,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        version: u64,
        data: &[u8],
    ) -> anyhow::Result<PathBuf> {
        let ks_hex = hex::encode(keyspace.as_bytes());
        let persistent_name = format!("{ks_hex}_{db_id}_{table_id}_{index_id}_v{version}.usearch");
        let persistent_path = self.cache_dir.join(&persistent_name);

        // Write to a temp file first, then atomically rename.
        let seq = CACHE_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_name = format!("{persistent_name}.tmp.{seq}");
        let tmp_path = self.cache_dir.join(&tmp_name);

        std::fs::write(&tmp_path, data)
            .with_context(|| format!("hnsw-cache: failed to write temp file {:?}", tmp_path))?;

        std::fs::rename(&tmp_path, &persistent_path).with_context(|| {
            format!(
                "hnsw-cache: failed to rename {:?} -> {:?}",
                tmp_path, persistent_path
            )
        })?;

        let key = (keyspace.to_string(), db_id, table_id, index_id);
        let entry = CacheEntry {
            graph_version: version,
            path: persistent_path.clone(),
            last_access: Instant::now(),
        };

        let mut entries = self.entries.lock().unwrap();

        // If there was a prior entry for this key, remove its file.
        if let Some(old) = entries.insert(key, entry) {
            if old.path != persistent_path {
                let _ = std::fs::remove_file(&old.path);
            }
        }

        // Evict LRU entry if over capacity.
        if entries.len() > self.max_entries {
            if let Some(lru_key) = entries
                .iter()
                .min_by_key(|(_, e)| e.last_access)
                .map(|(k, _)| k.clone())
            {
                if let Some(evicted) = entries.remove(&lru_key) {
                    let _ = std::fs::remove_file(&evicted.path);
                    debug!(
                        keyspace = %lru_key.0,
                        db_id = lru_key.1,
                        table_id = lru_key.2,
                        index_id = lru_key.3,
                        "hnsw-cache: evicted LRU entry"
                    );
                }
            }
        }

        debug!(
            keyspace,
            db_id,
            table_id,
            index_id,
            version,
            size_bytes = data.len(),
            "hnsw-cache: inserted"
        );
        Ok(persistent_path)
    }

    /// Explicitly remove a cache entry for an index (e.g., on DROP INDEX).
    /// Prevents stale cache hits when index_id is reused after DROP + CREATE.
    pub(crate) fn evict(&self, keyspace: &str, db_id: u64, table_id: u64, index_id: u64) {
        let key = (keyspace.to_string(), db_id, table_id, index_id);
        let mut entries = self.entries.lock().unwrap();
        if let Some(old) = entries.remove(&key) {
            // Delay file deletion: an in-flight index.load() may still
            // have the file open. 30s is generous (load takes ~15ms).
            let path = old.path;
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let _ = std::fs::remove_file(&path);
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Cache global singleton
// ---------------------------------------------------------------------------

static HNSW_CACHE: OnceLock<HnswGraphCache> = OnceLock::new();

/// Returns a reference to the global HNSW graph cache.
///
/// Panics if `init_hnsw_cache()` has not been called.
pub(crate) fn hnsw_graph_cache() -> &'static HnswGraphCache {
    HNSW_CACHE
        .get()
        .expect("hnsw_graph_cache() called before init_hnsw_cache()")
}

/// Initialize the global HNSW graph file cache.
///
/// Must be called once at startup. Reads `HNSW_CACHE_MAX_ENTRIES` (default 64)
/// and `HNSW_CACHE_DIR` (default `std::env::temp_dir()`) from environment.
/// Cleans up leftover `.usearch` files from prior process runs.
pub(crate) fn init_hnsw_cache() -> anyhow::Result<()> {
    let max_entries: usize = config::env_string("HNSW_CACHE_MAX_ENTRIES")
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CACHE_MAX_ENTRIES);

    let cache_dir = config::env_string("HNSW_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("db9_hnsw_cache");

    info!(
        max_entries,
        dir = %cache_dir.display(),
        "hnsw-cache: initializing"
    );

    let cache = HnswGraphCache::new(cache_dir, max_entries)?;

    HNSW_CACHE
        .set(cache)
        .map_err(|_| anyhow!("hnsw-cache: OnceLock already initialized"))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Process-level in-memory index cache (shared read-only indexes)
// ---------------------------------------------------------------------------

use super::storage::HnswIndexHandle;

/// A shared, read-only loaded HNSW index.
///
/// usearch `search()` is thread-safe: it uses an internal per-thread context pool
/// (`available_threads_` with `hardware_concurrency()` slots). Multiple concurrent
/// queries can safely call `search()` on the same `Index` without external locking.
///
/// The prior comment claiming "usearch Index is not thread-safe" was overly
/// conservative — it applies to concurrent `add()` + `search()` on the same
/// scratch buffers, but the punned_dense wrapper's `thread_lock_()` mechanism
/// handles this internally.
pub(crate) struct SharedHnswIndex {
    pub index: HnswIndexHandle,
    pub estimated_memory_bytes: usize,
}

/// Cache key: (keyspace, db_id, table_id, index_id, cache_version).
///
/// For S3 graphs (graph_version > 0): `cache_version = graph_version`.
/// Version is monotonically increasing (TSO-based), so DDL cycles cannot collide.
///
/// For TiKV graphs (graph_version == 0): `cache_version = meta_fingerprint(count, capacity)`.
/// This disambiguates across DROP+CREATE cycles (which reuse index_id with
/// graph_version=0) and naturally invalidates after merge (count/capacity change).
type IndexCacheKey = (String, u64, u64, u64, u64);

struct IndexCacheEntry {
    index: Arc<SharedHnswIndex>,
    last_access: Instant,
}

/// Process-level cache for loaded, read-only HNSW indexes.
///
/// Each entry is an `Arc<SharedHnswIndex>` that can be cloned cheaply by
/// concurrent queries. When a version changes (after merge), the old entry
/// stays alive via `Arc` until the last query holding it finishes.
pub(crate) struct HnswIndexCache {
    entries: Mutex<HashMap<IndexCacheKey, IndexCacheEntry>>,
    total_memory_bytes: AtomicUsize,
    max_memory_bytes: usize,
}

/// Default memory budget: 2 GB.
const DEFAULT_INDEX_CACHE_MEMORY: usize = 2 * 1024 * 1024 * 1024;

impl HnswIndexCache {
    fn new(max_memory_bytes: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            total_memory_bytes: AtomicUsize::new(0),
            max_memory_bytes,
        }
    }

    /// Look up a cached loaded index by exact version.
    pub(crate) fn lookup(
        &self,
        keyspace: &str,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        graph_version: u64,
    ) -> Option<Arc<SharedHnswIndex>> {
        let key = (
            keyspace.to_string(),
            db_id,
            table_id,
            index_id,
            graph_version,
        );
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get_mut(&key) {
            entry.last_access = Instant::now();
            debug!(
                keyspace,
                db_id, table_id, index_id, graph_version, "hnsw-index-cache: hit"
            );
            Some(Arc::clone(&entry.index))
        } else {
            None
        }
    }

    /// Insert a loaded index into the cache.
    /// Evicts LRU entries if memory budget is exceeded.
    pub(crate) fn insert(
        &self,
        keyspace: &str,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        graph_version: u64,
        index: HnswIndexHandle,
        estimated_memory_bytes: usize,
    ) -> Arc<SharedHnswIndex> {
        let shared = Arc::new(SharedHnswIndex {
            index,
            estimated_memory_bytes,
        });

        // Admission control: if a single index exceeds the entire cache budget,
        // return it for the current query but don't cache it. This prevents a
        // single oversized graph from evicting all other entries and exceeding
        // the memory budget permanently.
        if self.max_memory_bytes > 0 && estimated_memory_bytes > self.max_memory_bytes {
            warn!(
                keyspace,
                db_id,
                table_id,
                index_id,
                graph_version,
                estimated_memory_bytes,
                max_memory_bytes = self.max_memory_bytes,
                "hnsw-index-cache: index too large to cache, serving without caching"
            );
            return shared;
        }

        let key = (
            keyspace.to_string(),
            db_id,
            table_id,
            index_id,
            graph_version,
        );

        let mut entries = self.entries.lock().unwrap();

        // Evict older versions of the same index (different graph_version).
        let same_index_keys: Vec<IndexCacheKey> = entries
            .keys()
            .filter(|(ks, did, tid, iid, _)| {
                ks == keyspace && *did == db_id && *tid == table_id && *iid == index_id
            })
            .cloned()
            .collect();
        for old_key in same_index_keys {
            if old_key.4 != graph_version {
                if let Some(evicted) = entries.remove(&old_key) {
                    self.total_memory_bytes
                        .fetch_sub(evicted.index.estimated_memory_bytes, Ordering::Relaxed);
                    debug!(
                        keyspace,
                        db_id,
                        table_id,
                        index_id,
                        old_version = old_key.4,
                        new_version = graph_version,
                        "hnsw-index-cache: evicted stale version"
                    );
                }
            }
        }

        // Evict LRU entries while over memory budget.
        let new_total = self.total_memory_bytes.load(Ordering::Relaxed) + estimated_memory_bytes;
        if self.max_memory_bytes > 0 && new_total > self.max_memory_bytes {
            let mut to_free = new_total - self.max_memory_bytes;
            while to_free > 0 {
                let lru_key = entries
                    .iter()
                    .filter(|(k, _)| *k != &key) // don't evict what we're about to insert
                    .min_by_key(|(_, e)| e.last_access)
                    .map(|(k, _)| k.clone());
                match lru_key {
                    Some(lru) => {
                        if let Some(evicted) = entries.remove(&lru) {
                            let freed = evicted.index.estimated_memory_bytes;
                            self.total_memory_bytes.fetch_sub(freed, Ordering::Relaxed);
                            to_free = to_free.saturating_sub(freed);
                            debug!(
                                keyspace = %lru.0, db_id = lru.1,
                                table_id = lru.2, index_id = lru.3,
                                version = lru.4, freed_bytes = freed,
                                "hnsw-index-cache: evicted LRU entry for memory budget"
                            );
                        }
                    }
                    None => break, // no more entries to evict
                }
            }
        }

        // Insert the new entry. If a prior entry exists for the same key
        // (concurrent cache miss race), subtract its memory before adding ours.
        self.total_memory_bytes
            .fetch_add(estimated_memory_bytes, Ordering::Relaxed);
        if let Some(replaced) = entries.insert(
            key,
            IndexCacheEntry {
                index: Arc::clone(&shared),
                last_access: Instant::now(),
            },
        ) {
            self.total_memory_bytes
                .fetch_sub(replaced.index.estimated_memory_bytes, Ordering::Relaxed);
        }

        debug!(
            keyspace,
            db_id,
            table_id,
            index_id,
            graph_version,
            estimated_memory_bytes,
            total_cached_bytes = self.total_memory_bytes.load(Ordering::Relaxed),
            "hnsw-index-cache: inserted"
        );

        shared
    }

    /// Evict all entries for a specific index (e.g., on DROP INDEX).
    #[allow(dead_code)] // wired by DROP INDEX path
    pub(crate) fn evict(&self, keyspace: &str, db_id: u64, table_id: u64, index_id: u64) {
        let mut entries = self.entries.lock().unwrap();
        let keys_to_remove: Vec<IndexCacheKey> = entries
            .keys()
            .filter(|(ks, did, tid, iid, _)| {
                ks == keyspace && *did == db_id && *tid == table_id && *iid == index_id
            })
            .cloned()
            .collect();
        for key in keys_to_remove {
            if let Some(evicted) = entries.remove(&key) {
                self.total_memory_bytes
                    .fetch_sub(evicted.index.estimated_memory_bytes, Ordering::Relaxed);
            }
        }
    }
}

static HNSW_INDEX_CACHE: OnceLock<HnswIndexCache> = OnceLock::new();

/// Maximum size of a single HNSW index that can be loaded into memory.
/// Defaults to the cache memory budget. Set via HNSW_MAX_INDEX_MEMORY env var.
/// 0 = unlimited (not recommended).
pub(crate) fn hnsw_max_index_memory() -> usize {
    static VAL: OnceLock<usize> = OnceLock::new();
    *VAL.get_or_init(|| {
        config::env_string("HNSW_MAX_INDEX_MEMORY")
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                // Default to cache budget — a single index shouldn't exceed the cache.
                config::env_string("HNSW_INDEX_CACHE_MEMORY")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DEFAULT_INDEX_CACHE_MEMORY)
            })
    })
}

/// Returns a reference to the global in-memory index cache.
pub(crate) fn hnsw_index_cache() -> &'static HnswIndexCache {
    HNSW_INDEX_CACHE.get_or_init(|| {
        let max_bytes: usize = config::env_string("HNSW_INDEX_CACHE_MEMORY")
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_INDEX_CACHE_MEMORY);
        info!(
            max_memory_bytes = max_bytes,
            "hnsw-index-cache: initializing"
        );
        HnswIndexCache::new(max_bytes)
    })
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Thin wrapper around the AWS S3 client for HNSW graph storage.
///
/// All methods take `keyspace` for multi-tenant isolation. The keyspace is
/// hex-encoded and used as a path component in the S3 key.
#[derive(Clone)]
pub(crate) struct HnswS3Client {
    bucket: String,
    prefix: String,
    client: aws_sdk_s3::Client,
}

impl HnswS3Client {
    /// Construct a new client from the given configuration.
    pub async fn new(config: &HnswS3Config) -> anyhow::Result<Self> {
        let mut loader = aws_config::defaults(BehaviorVersion::latest());
        if let Some(region) = config.region.as_deref() {
            loader = loader.region(Region::new(region.to_string()));
        }
        let shared = loader.load().await;

        let timeout = TimeoutConfig::builder()
            .operation_timeout(Duration::from_secs(30))
            .operation_attempt_timeout(Duration::from_secs(10))
            .build();

        let mut builder = S3ConfigBuilder::from(&shared).timeout_config(timeout);
        if let Some(endpoint) = config.endpoint.as_deref() {
            builder = builder.endpoint_url(endpoint);
        }
        if config.force_path_style {
            builder = builder.force_path_style(true);
        }
        let s3_config = builder.build();

        Ok(Self {
            bucket: config.bucket.clone(),
            prefix: config.prefix.clone(),
            client: aws_sdk_s3::Client::from_conf(s3_config),
        })
    }

    // -- key helpers --------------------------------------------------------

    /// Build the S3 key for a specific graph version.
    fn graph_key(
        &self,
        keyspace: &str,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        version: u64,
    ) -> String {
        let ks_hex = hex::encode(keyspace.as_bytes());
        format!(
            "{}/{}/{}/{}/{}/graph_v{}.usearch",
            self.prefix, ks_hex, db_id, table_id, index_id, version
        )
    }

    /// Build the S3 prefix for all versions of a specific index.
    #[allow(dead_code)] // Used by GC/delete_prefix
    fn index_prefix(&self, keyspace: &str, db_id: u64, table_id: u64, index_id: u64) -> String {
        let ks_hex = hex::encode(keyspace.as_bytes());
        format!(
            "{}/{}/{}/{}/{}/",
            self.prefix, ks_hex, db_id, table_id, index_id
        )
    }

    /// Build the S3 prefix for all indexes in a database.
    #[allow(dead_code)] // Used by GC list_objects
    fn db_prefix(&self, keyspace: &str, db_id: u64) -> String {
        let ks_hex = hex::encode(keyspace.as_bytes());
        format!("{}/{}/{}/", self.prefix, ks_hex, db_id)
    }

    // -- operations ---------------------------------------------------------

    /// Upload a serialized HNSW graph blob to S3.
    pub async fn put_graph(
        &self,
        keyspace: &str,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        version: u64,
        data: Bytes,
    ) -> anyhow::Result<()> {
        let key = self.graph_key(keyspace, db_id, table_id, index_id, version);
        let size = data.len();

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(ByteStream::from(data))
            .send()
            .await
            .with_context(|| {
                format!("hnsw-s3: PutObject failed for s3://{}/{}", self.bucket, key)
            })?;

        debug!(
            key = %key,
            size_bytes = size,
            "hnsw-s3: uploaded graph"
        );
        Ok(())
    }

    /// Download a serialized HNSW graph blob from S3.
    ///
    /// Returns `Ok(None)` if the object does not exist (NoSuchKey).
    pub async fn get_graph(
        &self,
        keyspace: &str,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        version: u64,
    ) -> anyhow::Result<Option<Bytes>> {
        let key = self.graph_key(keyspace, db_id, table_id, index_id, version);

        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await;

        match result {
            Ok(output) => {
                // Check Content-Length before downloading to prevent
                // unbounded memory allocation from oversized S3 objects.
                if let Some(content_length) = output.content_length() {
                    let max_download = hnsw_max_index_memory();
                    if max_download > 0 && (content_length as usize) > max_download {
                        return Err(anyhow!(
                            "hnsw-s3: graph s3://{}/{} is {} bytes, exceeds \
                             HNSW_MAX_INDEX_MEMORY ({} bytes)",
                            self.bucket,
                            key,
                            content_length,
                            max_download
                        ));
                    }
                }
                let agg = output.body.collect().await.with_context(|| {
                    format!(
                        "hnsw-s3: failed to read GetObject body for s3://{}/{}",
                        self.bucket, key
                    )
                })?;
                let bytes = agg.into_bytes();
                debug!(
                    key = %key,
                    size_bytes = bytes.len(),
                    "hnsw-s3: downloaded graph"
                );
                Ok(Some(bytes))
            }
            Err(SdkError::ServiceError(service_err)) if service_err.err().is_no_such_key() => {
                debug!(key = %key, "hnsw-s3: graph not found (NoSuchKey)");
                Ok(None)
            }
            Err(err) => Err(anyhow!(err).context(format!(
                "hnsw-s3: GetObject failed for s3://{}/{}",
                self.bucket, key
            ))),
        }
    }

    /// Delete a specific graph version from S3.
    ///
    /// Returns `Err` on failure so the caller (GC) can retain the retry
    /// marker and try again on the next sweep cycle. S3 DeleteObject is
    /// idempotent — deleting a non-existent key is not an error.
    #[allow(dead_code)] // Used by GC sweep
    pub async fn delete_graph(
        &self,
        keyspace: &str,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        version: u64,
    ) -> anyhow::Result<()> {
        let key = self.graph_key(keyspace, db_id, table_id, index_id, version);

        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|err| {
                warn!(
                    key = %key,
                    error = %err,
                    "hnsw-s3: DeleteObject failed"
                );
                anyhow::anyhow!("hnsw-s3: DeleteObject failed for {}: {}", key, err)
            })?;

        debug!(key = %key, "hnsw-s3: deleted graph");
        Ok(())
    }

    /// Delete all S3 objects for an entire database (used by DROP DATABASE).
    /// Deletes everything under `{prefix}/{hex(keyspace)}/{db_id}/`.
    pub async fn delete_db_prefix(&self, keyspace: &str, db_id: u64) -> anyhow::Result<u64> {
        let prefix = self.db_prefix(keyspace, db_id);
        self.delete_objects_by_prefix(&prefix).await
    }

    /// Delete all objects under a specific index prefix (for GC when an index
    /// is dropped).
    ///
    /// Returns the number of objects deleted. Logs warnings on individual
    /// batch failures but continues processing.
    #[allow(dead_code)] // Used by GC sweep
    pub async fn delete_prefix(
        &self,
        keyspace: &str,
        db_id: u64,
        table_id: u64,
        index_id: u64,
    ) -> anyhow::Result<u64> {
        let prefix = self.index_prefix(keyspace, db_id, table_id, index_id);
        self.delete_objects_by_prefix(&prefix).await
    }

    /// List all objects under a database prefix (for batched GC).
    #[allow(dead_code)] // Used by GC sweep
    pub async fn list_objects(
        &self,
        keyspace: &str,
        db_id: u64,
    ) -> anyhow::Result<Vec<S3ObjectInfo>> {
        let prefix = self.db_prefix(keyspace, db_id);
        let mut results = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&prefix);

            if let Some(token) = continuation_token.take() {
                req = req.continuation_token(token);
            }

            let output = req.send().await.with_context(|| {
                format!(
                    "hnsw-s3: ListObjectsV2 failed for s3://{}/{}",
                    self.bucket, prefix
                )
            })?;

            for obj in output.contents() {
                let key = match obj.key() {
                    Some(k) => k.to_string(),
                    None => continue,
                };
                let last_modified = obj
                    .last_modified()
                    .and_then(|dt| SystemTime::try_from(*dt).ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                let size = obj.size().and_then(|s| u64::try_from(s).ok()).unwrap_or(0);

                results.push(S3ObjectInfo {
                    key,
                    last_modified,
                    size,
                });
            }

            match output.next_continuation_token() {
                Some(token) if output.is_truncated() == Some(true) => {
                    continuation_token = Some(token.to_string());
                }
                _ => break,
            }
        }

        debug!(
            prefix = %prefix,
            count = results.len(),
            "hnsw-s3: listed objects"
        );
        Ok(results)
    }

    // -- internal helpers ---------------------------------------------------

    /// List and batch-delete all objects under the given S3 prefix.
    #[allow(dead_code)] // Used by delete_prefix
    async fn delete_objects_by_prefix(&self, prefix: &str) -> anyhow::Result<u64> {
        let mut deleted_count: u64 = 0;
        let mut continuation_token: Option<String> = None;

        loop {
            let mut req = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);

            if let Some(token) = continuation_token.take() {
                req = req.continuation_token(token);
            }

            let output = req.send().await.with_context(|| {
                format!(
                    "hnsw-s3: ListObjectsV2 failed for s3://{}/{}",
                    self.bucket, prefix
                )
            })?;

            let keys: Vec<String> = output
                .contents()
                .iter()
                .filter_map(|obj| obj.key().map(|k| k.to_string()))
                .collect();

            if !keys.is_empty() {
                // Batch delete in chunks of DELETE_BATCH_SIZE (S3 limit: 1000).
                for chunk in keys.chunks(DELETE_BATCH_SIZE) {
                    let identifiers: Vec<ObjectIdentifier> = chunk
                        .iter()
                        .map(|k| {
                            ObjectIdentifier::builder()
                                .key(k.as_str())
                                .build()
                                .expect("ObjectIdentifier requires key")
                        })
                        .collect();

                    let delete = Delete::builder()
                        .set_objects(Some(identifiers))
                        .quiet(true)
                        .build()
                        .map_err(|e| anyhow!("hnsw-s3: failed to build Delete request: {}", e))?;

                    match self
                        .client
                        .delete_objects()
                        .bucket(&self.bucket)
                        .delete(delete)
                        .send()
                        .await
                    {
                        Ok(output) => {
                            let errors = output.errors();
                            if errors.is_empty() {
                                deleted_count += chunk.len() as u64;
                            } else {
                                // Some objects in this batch failed to delete.
                                // Count only successful deletes and return Err
                                // so the caller (GC) does NOT delete the
                                // tombstone meta — it must retry next cycle.
                                let failed = errors.len();
                                for err in errors {
                                    warn!(
                                        prefix = %prefix,
                                        key = ?err.key(),
                                        code = ?err.code(),
                                        message = ?err.message(),
                                        "hnsw-s3: DeleteObjects per-object failure"
                                    );
                                }
                                return Err(anyhow!(
                                    "hnsw-s3: {} of {} objects failed to delete under {}",
                                    failed,
                                    chunk.len(),
                                    prefix
                                ));
                            }
                        }
                        Err(err) => {
                            warn!(
                                prefix = %prefix,
                                batch_size = chunk.len(),
                                error = %err,
                                "hnsw-s3: DeleteObjects batch failed (non-fatal)"
                            );
                            return Err(anyhow!(err).context(format!(
                                "hnsw-s3: DeleteObjects failed for prefix {}",
                                prefix
                            )));
                        }
                    }
                }
            }

            match output.next_continuation_token() {
                Some(token) if output.is_truncated() == Some(true) => {
                    continuation_token = Some(token.to_string());
                }
                _ => break,
            }
        }

        debug!(
            prefix = %prefix,
            deleted = deleted_count,
            "hnsw-s3: deleted objects by prefix"
        );
        Ok(deleted_count)
    }
}

// ---------------------------------------------------------------------------
// S3 key parsing helpers (used by GC sweep)
// ---------------------------------------------------------------------------

/// Parse (table_id, index_id, version) from an S3 key.
///
/// Expected key format:
///   `{prefix}/{hex(keyspace)}/{db_id}/{table_id}/{index_id}/graph_v{version}.usearch`
///
/// Returns `None` if the key does not match the expected format.
pub(crate) fn parse_s3_key(key: &str) -> Option<(u64, u64, u64)> {
    // Split by '/' and take the last 3 meaningful segments:
    //   ../{table_id}/{index_id}/graph_v{version}.usearch
    let parts: Vec<&str> = key.rsplitn(4, '/').collect();
    if parts.len() < 3 {
        return None;
    }
    // parts[0] = "graph_v{version}.usearch"
    // parts[1] = "{index_id}"
    // parts[2] = "{table_id}"
    let filename = parts[0];
    let index_id: u64 = parts[1].parse().ok()?;
    let table_id: u64 = parts[2].parse().ok()?;

    let version = parse_version_from_filename(filename)?;
    Some((table_id, index_id, version))
}

/// Parse graph version from filename like `graph_v{version}.usearch`.
pub(crate) fn parse_version_from_filename(filename: &str) -> Option<u64> {
    let rest = filename.strip_prefix("graph_v")?;
    let version_str = rest.strip_suffix(".usearch")?;
    version_str.parse().ok()
}

// ---------------------------------------------------------------------------
// Global singleton
// ---------------------------------------------------------------------------

static HNSW_S3: OnceLock<Option<HnswS3Client>> = OnceLock::new();

/// Returns a reference to the global HNSW S3 client, if S3 offload is enabled.
///
/// Returns `None` if `HNSW_S3_BUCKET` was not set at startup.
pub(crate) fn hnsw_s3_client() -> Option<&'static HnswS3Client> {
    HNSW_S3.get().and_then(|opt| opt.as_ref())
}

/// Initialize the global HNSW S3 client from environment variables.
///
/// Must be called once at startup (from `main.rs`). If `HNSW_S3_BUCKET` is
/// not set, the client is not created and S3 offload is disabled.
///
/// Also initializes the process-level graph file cache (always, regardless
/// of whether S3 is enabled — the cache is useful for any graph loading path).
///
/// This function blocks the current thread to run the async client
/// construction on the tokio runtime.
pub(crate) fn init_hnsw_s3() -> anyhow::Result<()> {
    // Initialize the graph file cache (always, even without S3).
    init_hnsw_cache()?;

    let config = match HnswS3Config::from_env() {
        Some(c) => c,
        None => {
            HNSW_S3
                .set(None)
                .map_err(|_| anyhow!("hnsw-s3: OnceLock already initialized"))?;
            debug!("hnsw-s3: HNSW_S3_BUCKET not set, S3 offload disabled");
            return Ok(());
        }
    };

    info!(
        bucket = %config.bucket,
        prefix = %config.prefix,
        region = ?config.region,
        endpoint = ?config.endpoint,
        force_path_style = config.force_path_style,
        "hnsw-s3: initializing S3 client"
    );

    let rt = tokio::runtime::Handle::try_current()
        .map_err(|_| anyhow!("hnsw-s3: no tokio runtime available for S3 client init"))?;

    // Use block_in_place to allow block_on inside an async context
    // (direct block_on panics with "Cannot start a runtime from within a runtime").
    let client = tokio::task::block_in_place(|| rt.block_on(HnswS3Client::new(&config)))?;

    HNSW_S3
        .set(Some(client))
        .map_err(|_| anyhow!("hnsw-s3: OnceLock already initialized"))?;

    info!("hnsw-s3: S3 client initialized");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_key_format() {
        let client = HnswS3Client {
            bucket: "test-bucket".to_string(),
            prefix: "hnsw".to_string(),
            client: build_dummy_client(),
        };
        let key = client.graph_key("tenant_abc", 1, 2, 3, 42);
        assert_eq!(key, "hnsw/74656e616e745f616263/1/2/3/graph_v42.usearch");
    }

    #[test]
    fn index_prefix_format() {
        let client = HnswS3Client {
            bucket: "test-bucket".to_string(),
            prefix: "hnsw".to_string(),
            client: build_dummy_client(),
        };
        let pfx = client.index_prefix("ks", 10, 20, 30);
        assert_eq!(pfx, "hnsw/6b73/10/20/30/");
    }

    #[test]
    fn db_prefix_format() {
        let client = HnswS3Client {
            bucket: "test-bucket".to_string(),
            prefix: "hnsw".to_string(),
            client: build_dummy_client(),
        };
        let pfx = client.db_prefix("ks", 10);
        assert_eq!(pfx, "hnsw/6b73/10/");
    }

    #[test]
    fn custom_prefix_trims_slashes() {
        let client = HnswS3Client {
            bucket: "test-bucket".to_string(),
            prefix: "custom/prefix".to_string(),
            client: build_dummy_client(),
        };
        let key = client.graph_key("t", 1, 2, 3, 1);
        assert_eq!(key, "custom/prefix/74/1/2/3/graph_v1.usearch");
    }

    /// Build a dummy S3 client for unit tests that only exercise key formatting.
    /// No actual S3 calls are made.
    fn build_dummy_client() -> aws_sdk_s3::Client {
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .build();
        aws_sdk_s3::Client::from_conf(config)
    }

    // -- S3 key parsing tests -----------------------------------------------

    #[test]
    fn parse_s3_key_valid() {
        let key = "hnsw/6b73/10/20/30/graph_v42.usearch";
        let result = parse_s3_key(key);
        assert_eq!(result, Some((20, 30, 42)));
    }

    #[test]
    fn parse_s3_key_with_custom_prefix() {
        let key = "custom/prefix/abc123/5/100/200/graph_v7.usearch";
        let result = parse_s3_key(key);
        assert_eq!(result, Some((100, 200, 7)));
    }

    #[test]
    fn parse_s3_key_invalid_filename() {
        assert_eq!(parse_s3_key("hnsw/ks/1/2/3/not_graph.bin"), None);
    }

    #[test]
    fn parse_s3_key_missing_segments() {
        assert_eq!(parse_s3_key("graph_v1.usearch"), None);
    }

    #[test]
    fn parse_version_from_filename_valid() {
        assert_eq!(parse_version_from_filename("graph_v0.usearch"), Some(0));
        assert_eq!(parse_version_from_filename("graph_v999.usearch"), Some(999));
    }

    #[test]
    fn parse_version_from_filename_invalid() {
        assert_eq!(parse_version_from_filename("graph_v.usearch"), None);
        assert_eq!(parse_version_from_filename("other_file.bin"), None);
        assert_eq!(parse_version_from_filename("graph_vabc.usearch"), None);
    }

    // -- Cache tests --------------------------------------------------------

    #[test]
    fn cache_insert_and_lookup() {
        let dir = std::env::temp_dir().join("db9_hnsw_cache_test_insert_lookup");
        let _ = std::fs::remove_dir_all(&dir);
        let cache = HnswGraphCache::new(dir.clone(), 10).unwrap();

        let data = b"fake graph data";
        let path = cache
            .insert("ks1", 1, 2, 3, 5, data)
            .expect("insert should succeed");
        assert!(path.exists());

        // Lookup with correct version: hit.
        let result = cache.lookup("ks1", 1, 2, 3, 5);
        assert_eq!(result, Some(path.clone()));

        // Lookup with wrong version: miss + eviction.
        let result = cache.lookup("ks1", 1, 2, 3, 6);
        assert_eq!(result, None);
        assert!(!path.exists(), "stale file should be deleted");

        // Lookup again: miss (entry was evicted).
        let result = cache.lookup("ks1", 1, 2, 3, 5);
        assert_eq!(result, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_evicts_lru_when_full() {
        let dir = std::env::temp_dir().join("db9_hnsw_cache_test_lru");
        let _ = std::fs::remove_dir_all(&dir);
        let cache = HnswGraphCache::new(dir.clone(), 2).unwrap();

        let data = b"data";
        let p1 = cache.insert("ks", 1, 1, 1, 1, data).unwrap();
        // Small sleep to ensure different Instants for LRU ordering.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let _p2 = cache.insert("ks", 1, 2, 2, 1, data).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Insert a third entry: should evict (ks, 1, 1, 1) as LRU.
        let _p3 = cache.insert("ks", 1, 3, 3, 1, data).unwrap();

        assert!(!p1.exists(), "LRU entry file should be deleted");
        assert_eq!(cache.lookup("ks", 1, 1, 1, 1), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_not_found_returns_none() {
        let dir = std::env::temp_dir().join("db9_hnsw_cache_test_not_found");
        let _ = std::fs::remove_dir_all(&dir);
        let cache = HnswGraphCache::new(dir.clone(), 10).unwrap();

        assert_eq!(cache.lookup("ks", 1, 2, 3, 1), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_startup_cleanup() {
        let dir = std::env::temp_dir().join("db9_hnsw_cache_test_cleanup");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Create stale files.
        std::fs::write(dir.join("abc_1_2_3_v1.usearch"), b"stale").unwrap();
        std::fs::write(dir.join("def_4_5_6_v99.usearch"), b"stale").unwrap();
        // Non-matching file should NOT be deleted.
        std::fs::write(dir.join("other.txt"), b"keep").unwrap();

        let _cache = HnswGraphCache::new(dir.clone(), 10).unwrap();

        assert!(!dir.join("abc_1_2_3_v1.usearch").exists());
        assert!(!dir.join("def_4_5_6_v99.usearch").exists());
        assert!(dir.join("other.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
