use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::time::{SystemTime, UNIX_EPOCH};
use tikv_client::Transaction;
use tokio::fs;
use tracing::warn;

use crate::extensions::fs::embedded::keys;
#[cfg(test)]
use crate::extensions::fs::embedded::types::FS9_SPOOL_LAYOUT_VERSION;
use crate::txn::txn_put;

const BUNDLE_TRAILER_MAGIC: &[u8; 8] = b"FS9PACK1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BundleManifestState {
    Active,
    PendingDelete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BundleManifest {
    pub(crate) fs_instance_id: [u8; 16],
    pub(crate) bundle_id: u64,
    pub(crate) key: String,
    pub(crate) created_at: i64,
    pub(crate) object_size: u64,
    pub(crate) footer_offset: u64,
    pub(crate) entry_count: u32,
    pub(crate) live_entries: u32,
    pub(crate) live_bytes: u64,
    pub(crate) stale_entries: u32,
    pub(crate) checksum: [u8; 32],
    pub(crate) state: BundleManifestState,
}

impl BundleManifest {
    pub(crate) fn retire_entry(&mut self, entry_len: u32) -> Result<()> {
        if self.live_entries == 0 {
            return Err(anyhow!(
                "fs9: bundle {} has no live entries left to retire",
                self.bundle_id
            ));
        }

        self.live_entries -= 1;
        self.stale_entries = self
            .stale_entries
            .checked_add(1)
            .ok_or_else(|| anyhow!("fs9: bundle stale entry count overflow"))?;
        self.live_bytes = self
            .live_bytes
            .checked_sub(u64::from(entry_len))
            .ok_or_else(|| anyhow!("fs9: bundle live byte count underflow"))?;
        if self.live_entries == 0 {
            self.state = BundleManifestState::PendingDelete;
        }
        Ok(())
    }

    pub(crate) fn needs_compaction(&self) -> bool {
        self.live_entries > 0 && self.stale_entries > 0
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BundleBuildInput {
    pub(crate) staging_inode_id: u64,
    pub(crate) data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuiltBundleEntry {
    pub(crate) staging_inode_id: u64,
    pub(crate) offset: u64,
    pub(crate) len: u32,
    pub(crate) checksum: [u8; 32],
}

#[derive(Debug, Clone)]
pub(crate) struct BuiltBundle {
    pub(crate) bytes: Bytes,
    pub(crate) footer_offset: u64,
    pub(crate) object_size: u64,
    pub(crate) checksum: [u8; 32],
    pub(crate) entries: Vec<BuiltBundleEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BundleJournal {
    pub(crate) keyspace: String,
    pub(crate) fs_instance_id: [u8; 16],
    pub(crate) bundle_id: u64,
    pub(crate) key: String,
    pub(crate) created_at: i64,
    pub(crate) object_size: u64,
    pub(crate) staging_inode_ids: Vec<u64>,
}

#[derive(Debug)]
pub(crate) struct BundleSpool {
    root: PathBuf,
    fs_instance_id: [u8; 16],
}

impl BundleSpool {
    pub(crate) fn new(root: PathBuf, fs_instance_id: [u8; 16]) -> Self {
        Self {
            root,
            fs_instance_id,
        }
    }

    pub(crate) async fn write_pending_bundle(
        &self,
        journal: &BundleJournal,
        bundle: &BuiltBundle,
    ) -> Result<()> {
        fs::create_dir_all(&self.root).await?;
        let bundle_path = self.bundle_path(journal.bundle_id);
        let journal_path = self.journal_path(journal.bundle_id);
        write_file_atomic(&bundle_path, &bundle.bytes).await?;
        if let Err(err) =
            write_file_atomic(&journal_path, &serde_json::to_vec_pretty(journal)?).await
        {
            let _ = remove_if_exists(&bundle_path).await;
            return Err(err);
        }
        Ok(())
    }

    pub(crate) async fn remove_bundle(&self, bundle_id: u64) -> Result<()> {
        remove_if_exists(&self.bundle_path(bundle_id)).await?;
        remove_if_exists(&self.journal_path(bundle_id)).await?;
        Ok(())
    }

    pub(crate) async fn load_journals(&self) -> Result<Vec<BundleJournal>> {
        let mut out = Vec::new();
        let mut dir = match fs::read_dir(&self.root).await {
            Ok(dir) => dir,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(err) => return Err(err.into()),
        };

        while let Some(entry) = dir.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }

            let data = match fs::read(&path).await {
                Ok(data) => data,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err.into()),
            };

            match serde_json::from_slice::<BundleJournal>(&data) {
                Ok(journal) if journal.fs_instance_id == self.fs_instance_id => out.push(journal),
                Ok(_) => {}
                Err(err) => {
                    let quarantined = quarantine_corrupt_journal(&path).await?;
                    warn!(
                        "fs9: quarantined malformed bundle journal {} -> {}: {}",
                        path.display(),
                        quarantined.display(),
                        err
                    );
                }
            }
        }

        out.sort_by_key(|journal| journal.bundle_id);
        Ok(out)
    }

    pub(crate) async fn scavenge_active_root(&self, grace_secs: i64) -> Result<()> {
        let mut entries = match fs::read_dir(&self.root).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err.into()),
        };

        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_file() {
                continue;
            }

            if !file_is_stale(&entry, grace_secs).await? {
                continue;
            }

            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();

            if name.contains(".tmp-") {
                remove_if_exists(&path).await?;
                continue;
            }

            if let Some(bundle_id) = orphan_pack_bundle_id(&path) {
                let journal_path = self.journal_path(bundle_id);
                if fs::metadata(&journal_path).await.is_err() {
                    remove_if_exists(&path).await?;
                }
            }
        }

        Ok(())
    }

    fn bundle_path(&self, bundle_id: u64) -> PathBuf {
        self.root.join(format!("{bundle_id}.pack"))
    }

    fn journal_path(&self, bundle_id: u64) -> PathBuf {
        self.root.join(format!("{bundle_id}.json"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct BundleSliceCacheKey {
    bundle_id: u64,
    offset: u64,
    len: usize,
}

#[derive(Debug, Clone)]
struct BundleSliceCacheEntry {
    stamp: u64,
    data: Bytes,
}

#[derive(Debug, Default)]
struct BundleSliceCacheInner {
    next_stamp: u64,
    total_bytes: usize,
    map: HashMap<BundleSliceCacheKey, BundleSliceCacheEntry>,
    order: VecDeque<(BundleSliceCacheKey, u64)>,
}

#[derive(Debug)]
pub(crate) struct BundleSliceCache {
    max_bytes: usize,
    inner: Mutex<BundleSliceCacheInner>,
}

impl BundleSliceCache {
    const ORDER_COMPACT_FACTOR: usize = 8;
    const ORDER_COMPACT_MIN: usize = 1024;

    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            inner: Mutex::new(BundleSliceCacheInner::default()),
        }
    }

    fn maybe_compact_order(inner: &mut BundleSliceCacheInner) {
        let map_len = inner.map.len();
        if map_len == 0 {
            inner.order.clear();
            return;
        }

        let limit = map_len
            .saturating_mul(Self::ORDER_COMPACT_FACTOR)
            .saturating_add(Self::ORDER_COMPACT_MIN);
        if inner.order.len() <= limit {
            return;
        }

        // `order` keeps a log of `(key, stamp)` pairs. Hot `get()` workloads can grow this log
        // unboundedly, even though `map` is byte-bounded. Compact `order` down to the newest live
        // record per cached key, preserving LRU ordering.
        let mut seen = HashSet::with_capacity(map_len);
        let mut compact = Vec::with_capacity(map_len);
        for (key, stamp) in inner.order.iter().rev() {
            if compact.len() == map_len {
                break;
            }
            if seen.contains(key) {
                continue;
            }
            let Some(entry) = inner.map.get(key) else {
                continue;
            };
            if entry.stamp != *stamp {
                continue;
            }
            seen.insert(key.clone());
            compact.push((key.clone(), *stamp));
        }

        if compact.len() != map_len {
            compact = inner
                .map
                .iter()
                .map(|(key, entry)| (key.clone(), entry.stamp))
                .collect();
            compact.sort_by_key(|(_, stamp)| *stamp);
        } else {
            compact.reverse();
        }
        inner.order = VecDeque::from(compact);
    }

    pub(crate) fn get(&self, bundle_id: u64, offset: u64, len: usize) -> Option<Bytes> {
        if self.max_bytes == 0 {
            return None;
        }

        let key = BundleSliceCacheKey {
            bundle_id,
            offset,
            len,
        };
        let mut inner = self.inner.lock();
        let stamp = inner.next_stamp;
        inner.next_stamp = inner.next_stamp.wrapping_add(1);
        let data = {
            let entry = inner.map.get_mut(&key)?;
            entry.stamp = stamp;
            entry.data.clone()
        };
        inner.order.push_back((key, stamp));
        Self::maybe_compact_order(&mut inner);
        Some(data)
    }

    pub(crate) fn insert(&self, bundle_id: u64, offset: u64, data: Bytes) {
        if self.max_bytes == 0 || data.is_empty() || data.len() > self.max_bytes {
            return;
        }

        let key = BundleSliceCacheKey {
            bundle_id,
            offset,
            len: data.len(),
        };
        let mut inner = self.inner.lock();
        let stamp = inner.next_stamp;
        inner.next_stamp = inner.next_stamp.wrapping_add(1);

        if let Some(existing) = inner.map.insert(
            key.clone(),
            BundleSliceCacheEntry {
                stamp,
                data: data.clone(),
            },
        ) {
            inner.total_bytes = inner.total_bytes.saturating_sub(existing.data.len());
        }
        inner.total_bytes = inner.total_bytes.saturating_add(data.len());
        inner.order.push_back((key, stamp));
        Self::maybe_compact_order(&mut inner);

        while inner.total_bytes > self.max_bytes {
            let Some((evict_key, evict_stamp)) = inner.order.pop_front() else {
                break;
            };
            let Some(entry) = inner.map.get(&evict_key) else {
                continue;
            };
            if entry.stamp != evict_stamp {
                continue;
            }
            let removed = inner
                .map
                .remove(&evict_key)
                .expect("cache entry must exist");
            inner.total_bytes = inner.total_bytes.saturating_sub(removed.data.len());
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct BundleFooter {
    format_version: u32,
    bundle_id: u64,
    entries: Vec<BundleFooterEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BundleFooterEntry {
    staging_inode_id: u64,
    offset: u64,
    len: u32,
    checksum: [u8; 32],
}

pub(crate) fn build_bundle(bundle_id: u64, inputs: &[BundleBuildInput]) -> Result<BuiltBundle> {
    let mut body = Vec::new();
    let mut entries = Vec::with_capacity(inputs.len());

    for input in inputs {
        let len = u32::try_from(input.data.len())
            .map_err(|_| anyhow!("fs9: bundle entry exceeds u32 length"))?;
        let offset =
            u64::try_from(body.len()).map_err(|_| anyhow!("fs9: bundle offset overflow"))?;
        let checksum: [u8; 32] = Sha256::digest(&input.data).into();
        body.extend_from_slice(&input.data);
        entries.push(BuiltBundleEntry {
            staging_inode_id: input.staging_inode_id,
            offset,
            len,
            checksum,
        });
    }

    let footer = BundleFooter {
        format_version: 1,
        bundle_id,
        entries: entries
            .iter()
            .map(|entry| BundleFooterEntry {
                staging_inode_id: entry.staging_inode_id,
                offset: entry.offset,
                len: entry.len,
                checksum: entry.checksum,
            })
            .collect(),
    };
    let footer_bytes = serde_json::to_vec(&footer)?;
    let footer_offset =
        u64::try_from(body.len()).map_err(|_| anyhow!("fs9: bundle footer offset overflow"))?;
    body.extend_from_slice(&footer_bytes);
    body.extend_from_slice(
        &u32::try_from(footer_bytes.len())
            .map_err(|_| anyhow!("fs9: bundle footer exceeds u32 length"))?
            .to_be_bytes(),
    );
    body.extend_from_slice(BUNDLE_TRAILER_MAGIC);

    let checksum: [u8; 32] = Sha256::digest(&body).into();
    let object_size =
        u64::try_from(body.len()).map_err(|_| anyhow!("fs9: bundle object size overflow"))?;
    Ok(BuiltBundle {
        bytes: Bytes::from(body),
        footer_offset,
        object_size,
        checksum,
        entries,
    })
}

pub(crate) async fn load_bundle_manifest(
    txn: &mut Transaction,
    bundle_id: u64,
) -> Result<Option<BundleManifest>> {
    let Some(data) = txn.get(keys::bundle_manifest_key(bundle_id)).await? else {
        return Ok(None);
    };
    let manifest: BundleManifest = serde_json::from_slice(&data).map_err(|err| {
        anyhow!("fs9: invalid bundle manifest json for bundle {bundle_id}: {err}")
    })?;
    Ok(Some(manifest))
}

pub(crate) async fn save_bundle_manifest(
    txn: &mut Transaction,
    manifest: &BundleManifest,
) -> Result<()> {
    txn_put(
        txn,
        keys::bundle_manifest_key(manifest.bundle_id),
        serde_json::to_vec(manifest)?,
    )
    .await?;
    Ok(())
}

pub(crate) async fn delete_bundle_manifest(txn: &mut Transaction, bundle_id: u64) -> Result<()> {
    txn.delete(keys::bundle_manifest_key(bundle_id)).await?;
    Ok(())
}

pub(crate) async fn scan_bundle_manifests(
    txn: &mut Transaction,
    limit: u32,
) -> Result<Vec<BundleManifest>> {
    let prefix = keys::bundle_manifest_prefix();
    let end = keys::scan_end_key(&prefix);
    let pairs = txn.scan(prefix.clone()..end, limit).await?;

    let mut out = Vec::new();
    for pair in pairs {
        let key: Vec<u8> = pair.0.into();
        let Some(bundle_id) = bundle_id_from_manifest_key(&key) else {
            continue;
        };
        let manifest: BundleManifest = serde_json::from_slice(&pair.1).map_err(|err| {
            anyhow!("fs9: invalid bundle manifest json for bundle {bundle_id}: {err}")
        })?;
        out.push(manifest);
    }
    Ok(out)
}

pub(crate) async fn retire_bundle_entry(
    txn: &mut Transaction,
    bundle_id: u64,
    entry_len: u32,
) -> Result<BundleManifest> {
    let mut manifest = load_bundle_manifest(txn, bundle_id)
        .await?
        .ok_or_else(|| anyhow!("fs9: bundle manifest {bundle_id} not found"))?;
    manifest.retire_entry(entry_len)?;
    save_bundle_manifest(txn, &manifest).await?;
    Ok(manifest)
}

pub(crate) fn bundle_id_from_manifest_key(key: &[u8]) -> Option<u64> {
    let prefix = keys::bundle_manifest_prefix();
    if !key.starts_with(&prefix) || key.len() != prefix.len() + 8 {
        return None;
    }
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&key[prefix.len()..]);
    Some(u64::from_be_bytes(bytes))
}

async fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

async fn file_is_stale(entry: &fs::DirEntry, grace_secs: i64) -> Result<bool> {
    if grace_secs <= 0 {
        return Ok(true);
    }

    let modified = entry.metadata().await?.modified()?;
    let modified_at = system_time_to_unix_secs(modified);
    Ok(current_unix_timestamp().saturating_sub(modified_at) >= grace_secs)
}

fn orphan_pack_bundle_id(path: &Path) -> Option<u64> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("pack") {
        return None;
    }
    path.file_stem()?.to_str()?.parse::<u64>().ok()
}

async fn write_file_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp_path = temporary_path_for(path);
    if let Err(err) = fs::write(&tmp_path, data).await {
        let _ = remove_if_exists(&tmp_path).await;
        return Err(err.into());
    }
    if let Err(err) = fs::rename(&tmp_path, path).await {
        let _ = remove_if_exists(&tmp_path).await;
        return Err(err.into());
    }
    Ok(())
}

async fn quarantine_corrupt_journal(path: &Path) -> Result<PathBuf> {
    let quarantine_path = corrupt_journal_path(path);
    let _ = remove_if_exists(&quarantine_path).await;
    fs::rename(path, &quarantine_path).await?;
    Ok(quarantine_path)
}

fn corrupt_journal_path(path: &Path) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("journal"));
    file_name.push(format!(".corrupt-{}", unique_temp_suffix()));
    match path.parent() {
        Some(parent) => parent.join(file_name),
        None => PathBuf::from(file_name),
    }
}

fn temporary_path_for(path: &Path) -> PathBuf {
    let mut file_name = path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("tmp"));
    file_name.push(format!(".tmp-{}", unique_temp_suffix()));
    match path.parent() {
        Some(parent) => parent.join(file_name),
        None => PathBuf::from(file_name),
    }
}

fn unique_temp_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

fn current_unix_timestamp() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

fn system_time_to_unix_secs(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn bundle_id_from_manifest_key_parses_expected() {
        let key = keys::bundle_manifest_key(42);
        assert_eq!(bundle_id_from_manifest_key(&key), Some(42));
        assert_eq!(bundle_id_from_manifest_key(b"_fs_Xxxxx"), None);
    }

    #[test]
    fn bundle_id_from_manifest_key_rejects_guessed_keyspace_prefix() {
        let key = keys::bundle_manifest_key(42);
        let mut prefixed = vec![b'x', 0, 0, 3];
        prefixed.extend_from_slice(&key);
        assert_eq!(bundle_id_from_manifest_key(&prefixed), None);
    }

    #[test]
    fn build_bundle_roundtrip_footer_shape_is_stable() {
        let built = build_bundle(
            17,
            &[
                BundleBuildInput {
                    staging_inode_id: 11,
                    data: b"alpha".to_vec(),
                },
                BundleBuildInput {
                    staging_inode_id: 12,
                    data: b"beta".to_vec(),
                },
            ],
        )
        .expect("build bundle");

        assert_eq!(built.entries.len(), 2);
        assert_eq!(built.entries[0].offset, 0);
        assert_eq!(built.entries[0].len, 5);
        assert_eq!(built.entries[1].offset, 5);
        assert_eq!(built.entries[1].len, 4);
        assert_eq!(&built.bytes[..5], b"alpha");
        assert_eq!(&built.bytes[5..9], b"beta");
        assert!(built.footer_offset >= 9);

        let trailer_start = built.bytes.len() - (BUNDLE_TRAILER_MAGIC.len() + 4);
        let footer_len = u32::from_be_bytes(
            built.bytes[trailer_start..trailer_start + 4]
                .try_into()
                .expect("footer len bytes"),
        ) as usize;
        assert_eq!(
            &built.bytes[trailer_start + 4..trailer_start + 12],
            BUNDLE_TRAILER_MAGIC
        );
        let footer_start = trailer_start - footer_len;
        let footer: BundleFooter =
            serde_json::from_slice(&built.bytes[footer_start..trailer_start]).expect("footer");
        assert_eq!(footer.bundle_id, 17);
        assert_eq!(footer.entries.len(), 2);
        assert_eq!(footer.entries[0].staging_inode_id, 11);
        assert_eq!(footer.entries[1].staging_inode_id, 12);
    }

    #[test]
    fn retire_entry_marks_manifest_pending_delete_when_last_reference_drops() {
        let mut manifest = BundleManifest {
            fs_instance_id: [7u8; 16],
            bundle_id: 9,
            key: "tenant/packs/9.pack".to_string(),
            created_at: 1,
            object_size: 128,
            footer_offset: 64,
            entry_count: 2,
            live_entries: 2,
            live_bytes: 96,
            stale_entries: 0,
            checksum: [7u8; 32],
            state: BundleManifestState::Active,
        };

        manifest.retire_entry(32).expect("retire first");
        assert_eq!(manifest.live_entries, 1);
        assert_eq!(manifest.live_bytes, 64);
        assert_eq!(manifest.stale_entries, 1);
        assert_eq!(manifest.state, BundleManifestState::Active);
        assert!(manifest.needs_compaction());

        manifest.retire_entry(64).expect("retire second");
        assert_eq!(manifest.live_entries, 0);
        assert_eq!(manifest.live_bytes, 0);
        assert_eq!(manifest.stale_entries, 2);
        assert_eq!(manifest.state, BundleManifestState::PendingDelete);
    }

    #[tokio::test]
    async fn bundle_spool_roundtrip_loads_and_removes_pending_journal() {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = PathBuf::from(format!(
            "/tmp/db9-fs9-bundle-spool-test-{}-{}-{}",
            std::process::id(),
            id,
            nanos
        ));
        let spool = BundleSpool::new(root.clone(), [7u8; 16]);

        let built = build_bundle(
            3,
            &[BundleBuildInput {
                staging_inode_id: 88,
                data: b"payload".to_vec(),
            }],
        )
        .expect("build");
        let journal = BundleJournal {
            keyspace: "tenant".to_string(),
            fs_instance_id: [7u8; 16],
            bundle_id: 3,
            key: "tenant/packs/3.pack".to_string(),
            created_at: 123,
            object_size: built.object_size,
            staging_inode_ids: vec![88],
        };

        spool
            .write_pending_bundle(&journal, &built)
            .await
            .expect("write spool");
        let journals = spool.load_journals().await.expect("load journals");
        assert_eq!(journals.len(), 1);
        assert_eq!(journals[0].bundle_id, 3);

        spool.remove_bundle(3).await.expect("remove spool");
        assert!(spool.load_journals().await.expect("reload").is_empty());
        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn bundle_spool_ignores_mismatched_instance_journals() {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = PathBuf::from(format!(
            "/tmp/db9-fs9-bundle-spool-test-{}-{}-{}",
            std::process::id(),
            id,
            nanos
        ));
        let spool = BundleSpool::new(root.clone(), [7u8; 16]);
        fs::create_dir_all(&root).await.expect("create root");

        let journal = BundleJournal {
            keyspace: "tenant".to_string(),
            fs_instance_id: [9u8; 16],
            bundle_id: 4,
            key: "tenant/packs/4.pack".to_string(),
            created_at: 123,
            object_size: 0,
            staging_inode_ids: vec![],
        };
        fs::write(
            root.join("4.json"),
            serde_json::to_vec_pretty(&journal).unwrap(),
        )
        .await
        .expect("write mismatched journal");

        assert!(
            spool
                .load_journals()
                .await
                .expect("load journals")
                .is_empty(),
            "mismatched instance journal must be ignored"
        );

        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn bundle_spool_quarantines_malformed_current_format_journal() {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = PathBuf::from(format!(
            "/tmp/db9-fs9-bundle-spool-malformed-test-{}-{}-{}",
            std::process::id(),
            id,
            nanos
        ));
        let spool = BundleSpool::new(root.clone(), [7u8; 16]);
        fs::create_dir_all(&root).await.expect("create root");
        let journal_path = root.join("7.json");
        fs::write(&journal_path, b"{not-json")
            .await
            .expect("write malformed journal");

        let journals = spool
            .load_journals()
            .await
            .expect("malformed helper journal must not block scan");
        assert!(
            journals.is_empty(),
            "malformed current-format journal must be skipped"
        );
        assert!(
            fs::metadata(&journal_path).await.is_err(),
            "malformed journal must be quarantined out of the active namespace"
        );
        let mut entries = fs::read_dir(&root).await.expect("read root");
        let mut saw_quarantined = false;
        while let Some(entry) = entries.next_entry().await.expect("next entry") {
            let name = entry.file_name();
            let rendered = name.to_string_lossy();
            if rendered.starts_with("7.json.corrupt-") {
                saw_quarantined = true;
                break;
            }
        }
        assert!(
            saw_quarantined,
            "malformed current-format journal must be preserved under a quarantine filename"
        );

        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn bundle_spool_scavenges_orphan_pack_and_tmp_files() {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = PathBuf::from(format!(
            "/tmp/db9-fs9-bundle-spool-scavenge-test-{}-{}-{}",
            std::process::id(),
            id,
            nanos
        ));
        let spool = BundleSpool::new(root.clone(), [7u8; 16]);
        fs::create_dir_all(&root).await.expect("create root");

        fs::write(root.join("11.pack"), b"orphan")
            .await
            .expect("write orphan pack");
        fs::write(root.join("leftover.pack.tmp-1"), b"tmp")
            .await
            .expect("write temp file");
        fs::write(root.join("12.pack"), b"live-pack")
            .await
            .expect("write live pack");
        fs::write(root.join("12.json"), b"{}")
            .await
            .expect("write live journal");

        spool
            .scavenge_active_root(0)
            .await
            .expect("scavenge active root");

        assert!(fs::metadata(root.join("11.pack")).await.is_err());
        assert!(fs::metadata(root.join("leftover.pack.tmp-1"))
            .await
            .is_err());
        assert!(fs::metadata(root.join("12.pack")).await.is_ok());
        assert!(fs::metadata(root.join("12.json")).await.is_ok());

        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn bundle_spool_ignores_other_storage_format_directories() {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = PathBuf::from(format!(
            "/tmp/db9-fs9-bundle-spool-format-test-{}-{}-{}",
            std::process::id(),
            id,
            nanos
        ));
        let current_root = root
            .join(format!("format-{FS9_SPOOL_LAYOUT_VERSION}"))
            .join("current");
        let old_root = root
            .join(format!("format-{}", FS9_SPOOL_LAYOUT_VERSION - 1))
            .join("legacy");
        let spool = BundleSpool::new(current_root.clone(), [7u8; 16]);

        fs::create_dir_all(&old_root)
            .await
            .expect("create old root");
        fs::write(old_root.join("stale.json"), b"{not-json")
            .await
            .expect("write old journal");

        assert!(spool
            .load_journals()
            .await
            .expect("load current-format journals")
            .is_empty());
        assert!(
            fs::metadata(old_root.join("stale.json")).await.is_ok(),
            "current-format spool scan must not touch older format directories"
        );

        let _ = fs::remove_dir_all(root).await;
    }

    #[test]
    fn bundle_slice_cache_evicts_old_entries_by_total_bytes() {
        let cache = BundleSliceCache::new(6);
        cache.insert(1, 0, Bytes::from_static(b"abc"));
        cache.insert(1, 3, Bytes::from_static(b"def"));
        assert_eq!(cache.get(1, 0, 3).unwrap(), Bytes::from_static(b"abc"));
        cache.insert(1, 6, Bytes::from_static(b"ghi"));
        assert!(cache.get(1, 3, 3).is_none());
        assert_eq!(cache.get(1, 0, 3).unwrap(), Bytes::from_static(b"abc"));
        assert_eq!(cache.get(1, 6, 3).unwrap(), Bytes::from_static(b"ghi"));
    }

    #[test]
    fn bundle_slice_cache_order_does_not_grow_unbounded_on_hot_gets() {
        let cache = BundleSliceCache::new(6);
        cache.insert(1, 0, Bytes::from_static(b"abc"));
        cache.insert(1, 3, Bytes::from_static(b"def"));

        for i in 0..5_000 {
            let offset = if i % 2 == 0 { 0 } else { 3 };
            assert!(cache.get(1, offset, 3).is_some());
        }

        let inner = cache.inner.lock();
        let limit = inner.map.len() * BundleSliceCache::ORDER_COMPACT_FACTOR
            + BundleSliceCache::ORDER_COMPACT_MIN;
        assert!(inner.order.len() <= limit);
    }
}
