use crate::extensions::fs::embedded::keys;
use crate::extensions::fs::embedded::types::DataRef;
use crate::txn::txn_put;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tikv_client::Transaction;

/// Transitional lifecycle sidecar stored at `_fs_L{inode}`.
///
/// Absence of `_fs_L` implies the file is `Clean`.
///
/// This is intentionally separate from the inode to avoid split-brain between
/// reader-visible metadata and GC-visible transitional state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UploadReservation {
    pub(crate) fs_instance_id: [u8; 16],
    pub(crate) path: String,
    pub(crate) path_hash: [u8; 32],
    #[serde(default)]
    pub(crate) expected_parent_inode: Option<u64>,
    #[serde(default)]
    pub(crate) expected_prior_inode: Option<u64>,
    #[serde(default)]
    pub(crate) expected_prior_generation: Option<u64>,
    #[serde(default)]
    pub(crate) expected_size: u64,
    pub(crate) nonce: u64,
    pub(crate) expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum FileLifecycle {
    Uploading {
        fs_instance_id: [u8; 16],
        /// Multipart upload ID (if applicable). Missing implies a non-multipart write path.
        #[serde(default)]
        upload_id: Option<String>,
        /// Best-effort heartbeat for staleness detection.
        #[serde(default)]
        updated_at: i64,
        #[serde(default)]
        reservation: Option<UploadReservation>,
    },
    Committing {
        fs_instance_id: [u8; 16],
        #[serde(default)]
        updated_at: i64,
        #[serde(default)]
        reservation: Option<UploadReservation>,
    },
    Packing {
        fs_instance_id: [u8; 16],
        #[serde(default)]
        bundle_id: u64,
        #[serde(default)]
        updated_at: i64,
    },
    Deleting {
        fs_instance_id: [u8; 16],
        data_ref: DataRef,
    },
}

impl FileLifecycle {
    pub(crate) fn fs_instance_id(&self) -> [u8; 16] {
        match self {
            FileLifecycle::Uploading { fs_instance_id, .. }
            | FileLifecycle::Committing { fs_instance_id, .. }
            | FileLifecycle::Packing { fs_instance_id, .. }
            | FileLifecycle::Deleting { fs_instance_id, .. } => *fs_instance_id,
        }
    }
}

#[allow(dead_code)]
pub(crate) async fn load_lifecycle(
    txn: &mut Transaction,
    inode_id: u64,
) -> Result<Option<FileLifecycle>> {
    let Some(data) = txn.get(keys::lifecycle_key(inode_id)).await? else {
        return Ok(None);
    };
    let v = parse_lifecycle_bytes(inode_id, &data)?;
    Ok(Some(v))
}

#[allow(dead_code)]
pub(crate) async fn save_lifecycle(
    txn: &mut Transaction,
    inode_id: u64,
    lifecycle: &FileLifecycle,
) -> Result<()> {
    let data = serde_json::to_vec(lifecycle)?;
    txn_put(txn, keys::lifecycle_key(inode_id), data).await?;
    Ok(())
}

#[allow(dead_code)]
pub(crate) async fn clear_lifecycle(txn: &mut Transaction, inode_id: u64) -> Result<()> {
    txn.delete(keys::lifecycle_key(inode_id)).await?;
    Ok(())
}

#[allow(dead_code)]
pub(crate) async fn scan_lifecycle(
    txn: &mut Transaction,
    limit: u32,
) -> Result<Vec<(u64, FileLifecycle)>> {
    let prefix = keys::lifecycle_prefix();
    let end = keys::scan_end_key(&prefix);
    let pairs = txn.scan(prefix..end, limit).await?;

    let mut out = Vec::new();
    for pair in pairs {
        let key: Vec<u8> = pair.0.into();
        let Some(inode_id) = inode_id_from_lifecycle_key(&key) else {
            continue;
        };
        let lifecycle = parse_lifecycle_bytes(inode_id, &pair.1)?;
        out.push((inode_id, lifecycle));
    }
    Ok(out)
}

fn parse_lifecycle_bytes(inode_id: u64, data: &[u8]) -> Result<FileLifecycle> {
    serde_json::from_slice(data).map_err(|err| {
        anyhow!(
            "fs9: invalid lifecycle json for inode {inode_id}: {err}. \
             Current-format fs9 metadata is corrupt."
        )
    })
}

pub(crate) fn inode_id_from_lifecycle_key(key: &[u8]) -> Option<u64> {
    let prefix = keys::lifecycle_prefix();
    if !key.starts_with(&prefix) || key.len() != prefix.len() + 8 {
        return None;
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&key[prefix.len()..]);
    Some(u64::from_be_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_json_roundtrip() {
        let cases = vec![
            FileLifecycle::Uploading {
                fs_instance_id: [7u8; 16],
                upload_id: None,
                updated_at: 0,
                reservation: Some(UploadReservation {
                    fs_instance_id: [7u8; 16],
                    path: "/upload.bin".to_string(),
                    path_hash: [1u8; 32],
                    expected_parent_inode: Some(1),
                    expected_prior_inode: None,
                    expected_prior_generation: None,
                    expected_size: 123,
                    nonce: 9,
                    expires_at: 77,
                }),
            },
            FileLifecycle::Committing {
                fs_instance_id: [7u8; 16],
                updated_at: 0,
                reservation: None,
            },
            FileLifecycle::Packing {
                fs_instance_id: [7u8; 16],
                bundle_id: 7,
                updated_at: 0,
            },
            FileLifecycle::Deleting {
                fs_instance_id: [7u8; 16],
                data_ref: DataRef::InlineBlob,
            },
        ];

        for c in cases {
            let data = serde_json::to_vec(&c).expect("serialize");
            let got: FileLifecycle = serde_json::from_slice(&data).expect("deserialize");
            assert_eq!(got, c);
        }
    }

    #[test]
    fn inode_id_from_lifecycle_key_parses_expected() {
        let key = keys::lifecycle_key(42);
        assert_eq!(inode_id_from_lifecycle_key(&key), Some(42));

        // Wrong prefix.
        assert_eq!(inode_id_from_lifecycle_key(b"_fs_Xxxxx"), None);
    }

    #[test]
    fn inode_id_from_lifecycle_key_rejects_guessed_keyspace_prefix() {
        let key = keys::lifecycle_key(42);
        let mut prefixed = vec![b'x', 0, 0, 11];
        prefixed.extend_from_slice(&key);
        assert_eq!(inode_id_from_lifecycle_key(&prefixed), None);
    }
}
