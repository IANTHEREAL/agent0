//! Key encoding functions for the embedded filesystem.
//!
//! All keys use the `_fs_` prefix to avoid collision with existing pg-tikv system keys.
//! This namespace is isolated within the TiKV keyspace per tenant.
//!
//! Key format:
//! - Superblock: `_fs_S`
//! - Inode allocator: `_fs_AI`
//! - Bundle allocator: `_fs_AB`
//! - Inode: `_fs_I` + inode_id (big-endian u64)
//! - Directory entry: `_fs_D` + parent_inode (big-endian u64) + `:` + name
//! - Directory prefix: `_fs_D` + parent_inode (big-endian u64) + `:`
//! - Inline blob: `_fs_B` + inode_id (big-endian u64)
//! - Lifecycle sidecar: `_fs_L` + inode_id (big-endian u64)
//! - Bundle manifest: `_fs_M` + bundle_id (big-endian u64)
//! - Page: `_fs_P` + inode_id (big-endian u64) + `:` + page_num (big-endian u64)
//! - Page prefix: `_fs_P` + inode_id (big-endian u64) + `:`
//! - Staging write marker: `_fs_T` + inode_id (big-endian u64)
//! - Orphan cleanup marker: `_fs_O` + inode_id (big-endian u64)

pub(crate) const FS_NAMESPACE_PREFIX: &[u8] = b"_fs_";

/// Superblock key: `_fs_S`
///
/// The superblock stores filesystem identity/binding metadata.
pub(crate) fn superblock_key() -> Vec<u8> {
    b"_fs_S".to_vec()
}

/// Inode allocator key: `_fs_AI`
pub(crate) fn inode_allocator_key() -> Vec<u8> {
    b"_fs_AI".to_vec()
}

/// Bundle allocator key: `_fs_AB`
pub(crate) fn bundle_allocator_key() -> Vec<u8> {
    b"_fs_AB".to_vec()
}

/// Inode prefix for scanning all inodes: `_fs_I`
///
/// Used for range scans to iterate over all inode metadata entries.
pub(crate) fn inode_prefix() -> Vec<u8> {
    b"_fs_I".to_vec()
}

/// Inode key: `_fs_I` + inode_id (big-endian u64)
///
/// Each inode stores file/directory metadata (size, mode, timestamps, etc.).
pub(crate) fn inode_key(inode_id: u64) -> Vec<u8> {
    let mut key = b"_fs_I".to_vec();
    key.extend_from_slice(&inode_id.to_be_bytes());
    key
}

/// Inline blob key: `_fs_B` + inode_id (big-endian u64)
pub(crate) fn blob_key(inode_id: u64) -> Vec<u8> {
    let mut key = b"_fs_B".to_vec();
    key.extend_from_slice(&inode_id.to_be_bytes());
    key
}

/// Lifecycle sidecar key: `_fs_L` + inode_id (big-endian u64)
pub(crate) fn lifecycle_key(inode_id: u64) -> Vec<u8> {
    let mut key = b"_fs_L".to_vec();
    key.extend_from_slice(&inode_id.to_be_bytes());
    key
}

/// Lifecycle prefix: `_fs_L`
pub(crate) fn lifecycle_prefix() -> Vec<u8> {
    b"_fs_L".to_vec()
}

/// Bundle manifest key: `_fs_M` + bundle_id (big-endian u64)
pub(crate) fn bundle_manifest_key(bundle_id: u64) -> Vec<u8> {
    let mut key = b"_fs_M".to_vec();
    key.extend_from_slice(&bundle_id.to_be_bytes());
    key
}

/// Bundle manifest prefix: `_fs_M`
pub(crate) fn bundle_manifest_prefix() -> Vec<u8> {
    b"_fs_M".to_vec()
}

/// Compute an exclusive scan end key for a given prefix.
///
/// This is used to construct `start..end` ranges for TiKV scans.
pub(crate) fn scan_end_key(prefix: &[u8]) -> Vec<u8> {
    // TiKV's client treats an empty end key as "unbounded above". Returning an empty
    // Vec here is therefore the correct exclusive "no upper bound" representation.
    //
    // This happens when:
    // - the caller wants to scan the entire keyspace (`prefix` is empty), or
    // - the prefix is all-0xFF bytes, so there is no exclusive successor prefix.
    if prefix.is_empty() {
        return Vec::new();
    }
    let mut end = prefix.to_vec();
    for i in (0..end.len()).rev() {
        if end[i] != u8::MAX {
            end[i] += 1;
            end.truncate(i + 1);
            return end;
        }
    }
    Vec::new()
}

/// Directory entry key: `_fs_D` + parent_inode (big-endian u64) + `:` + name
///
/// Maps a filename within a directory to its inode ID.
pub(crate) fn dir_entry_key(parent_inode: u64, name: &str) -> Vec<u8> {
    let mut key = b"_fs_D".to_vec();
    key.extend_from_slice(&parent_inode.to_be_bytes());
    key.push(b':');
    key.extend_from_slice(name.as_bytes());
    key
}

/// Directory prefix for scanning all entries: `_fs_D` + parent_inode (big-endian u64) + `:`
///
/// Used for range scans to retrieve all directory entries under a parent inode.
pub(crate) fn dir_prefix(parent_inode: u64) -> Vec<u8> {
    let mut key = b"_fs_D".to_vec();
    key.extend_from_slice(&parent_inode.to_be_bytes());
    key.push(b':');
    key
}

/// Page key: `_fs_P` + inode_id (big-endian u64) + `:` + page_num (big-endian u64)
///
/// Stores a 16KB page of file content. Pages are numbered sequentially from 0.
pub(crate) fn page_key(inode_id: u64, page_num: u64) -> Vec<u8> {
    let mut key = b"_fs_P".to_vec();
    key.extend_from_slice(&inode_id.to_be_bytes());
    key.push(b':');
    key.extend_from_slice(&page_num.to_be_bytes());
    key
}

/// Page prefix for scanning all pages of an inode: `_fs_P` + inode_id (big-endian u64) + `:`
///
/// Used for range scans to retrieve all pages belonging to a file.
pub(crate) fn page_prefix(inode_id: u64) -> Vec<u8> {
    let mut key = b"_fs_P".to_vec();
    key.extend_from_slice(&inode_id.to_be_bytes());
    key.push(b':');
    key
}

/// Staging write marker key: `_fs_T` + inode_id (big-endian u64)
///
/// Marks an upload staging inode that is not yet published at a filesystem path.
pub(crate) fn staging_write_key(inode_id: u64) -> Vec<u8> {
    let mut key = b"_fs_T".to_vec();
    key.extend_from_slice(&inode_id.to_be_bytes());
    key
}

/// Prefix for scanning staging write markers.
pub(crate) fn staging_write_prefix() -> Vec<u8> {
    b"_fs_T".to_vec()
}

/// Orphan cleanup marker key: `_fs_O` + inode_id (big-endian u64)
///
/// Marks a file inode that has been unpublished and is waiting for page cleanup.
pub(crate) fn orphan_inode_key(inode_id: u64) -> Vec<u8> {
    let mut key = b"_fs_O".to_vec();
    key.extend_from_slice(&inode_id.to_be_bytes());
    key
}

/// Prefix for scanning orphan cleanup markers.
pub(crate) fn orphan_inode_prefix() -> Vec<u8> {
    b"_fs_O".to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_superblock_key() {
        let key = superblock_key();
        assert_eq!(key, b"_fs_S");
    }

    #[test]
    fn test_inode_key() {
        let key = inode_key(1);
        assert_eq!(&key[0..5], b"_fs_I");
        assert_eq!(&key[5..13], &1u64.to_be_bytes());
        assert_eq!(key.len(), 13);
    }

    #[test]
    fn test_inode_key_large() {
        let key = inode_key(256);
        assert_eq!(&key[0..5], b"_fs_I");
        // 256 in big-endian is [0, 0, 0, 0, 0, 0, 1, 0]
        assert_eq!(&key[5..13], &[0, 0, 0, 0, 0, 0, 1, 0]);
        assert_eq!(key.len(), 13);
    }

    #[test]
    fn test_blob_key() {
        let key = blob_key(7);
        assert_eq!(&key[0..5], b"_fs_B");
        assert_eq!(&key[5..13], &7u64.to_be_bytes());
        assert_eq!(key.len(), 13);
    }

    #[test]
    fn test_allocator_keys() {
        assert_eq!(inode_allocator_key(), b"_fs_AI");
        assert_eq!(bundle_allocator_key(), b"_fs_AB");
    }

    #[test]
    fn test_lifecycle_key_and_prefix() {
        let key = lifecycle_key(7);
        let prefix = lifecycle_prefix();
        assert_eq!(&key[0..5], b"_fs_L");
        assert_eq!(&key[5..13], &7u64.to_be_bytes());
        assert_eq!(key.len(), 13);
        assert!(key.starts_with(&prefix));
    }

    #[test]
    fn test_bundle_manifest_key_and_prefix() {
        let key = bundle_manifest_key(9);
        let prefix = bundle_manifest_prefix();
        assert_eq!(&key[0..5], b"_fs_M");
        assert_eq!(&key[5..13], &9u64.to_be_bytes());
        assert_eq!(key.len(), 13);
        assert!(key.starts_with(&prefix));
    }

    #[test]
    fn test_dir_entry_key() {
        let key = dir_entry_key(1, "hello.txt");
        assert_eq!(&key[0..5], b"_fs_D");
        assert_eq!(&key[5..13], &1u64.to_be_bytes());
        assert_eq!(key[13], b':');
        assert_eq!(&key[14..], b"hello.txt");
    }

    #[test]
    fn test_dir_prefix() {
        let prefix = dir_prefix(1);
        assert_eq!(&prefix[0..5], b"_fs_D");
        assert_eq!(&prefix[5..13], &1u64.to_be_bytes());
        assert_eq!(prefix[13], b':');
        assert_eq!(prefix.len(), 14);
    }

    #[test]
    fn test_dir_prefix_is_prefix_of_entry() {
        let prefix = dir_prefix(1);
        let entry = dir_entry_key(1, "anything");
        assert!(entry.starts_with(&prefix));
    }

    #[test]
    fn test_page_key() {
        let key = page_key(5, 0);
        assert_eq!(&key[0..5], b"_fs_P");
        assert_eq!(&key[5..13], &5u64.to_be_bytes());
        assert_eq!(key[13], b':');
        assert_eq!(&key[14..22], &0u64.to_be_bytes());
        assert_eq!(key.len(), 22);
    }

    #[test]
    fn test_page_key_multiple_pages() {
        let key0 = page_key(5, 0);
        let key1 = page_key(5, 1);
        assert_eq!(&key0[0..14], &key1[0..14]); // Same prefix
        assert_ne!(&key0[14..22], &key1[14..22]); // Different page numbers
    }

    #[test]
    fn test_page_prefix() {
        let prefix = page_prefix(5);
        assert_eq!(&prefix[0..5], b"_fs_P");
        assert_eq!(&prefix[5..13], &5u64.to_be_bytes());
        assert_eq!(prefix[13], b':');
        assert_eq!(prefix.len(), 14);
    }

    #[test]
    fn test_page_prefix_is_prefix_of_key() {
        let prefix = page_prefix(5);
        let key0 = page_key(5, 0);
        let key1 = page_key(5, 1);
        assert!(key0.starts_with(&prefix));
        assert!(key1.starts_with(&prefix));
    }

    #[test]
    fn test_staging_write_key() {
        let key = staging_write_key(42);
        assert_eq!(&key[0..5], b"_fs_T");
        assert_eq!(&key[5..13], &42u64.to_be_bytes());
        assert_eq!(key.len(), 13);
    }

    #[test]
    fn test_orphan_inode_key() {
        let key = orphan_inode_key(99);
        assert_eq!(&key[0..5], b"_fs_O");
        assert_eq!(&key[5..13], &99u64.to_be_bytes());
        assert_eq!(key.len(), 13);
    }

    #[test]
    fn test_key_namespace_isolation() {
        // Verify that all keys start with _fs_ to avoid collision
        assert!(superblock_key().starts_with(b"_fs_"));
        assert!(inode_key(1).starts_with(b"_fs_"));
        assert!(blob_key(1).starts_with(b"_fs_"));
        assert!(lifecycle_key(1).starts_with(b"_fs_"));
        assert!(lifecycle_prefix().starts_with(b"_fs_"));
        assert!(dir_entry_key(1, "test").starts_with(b"_fs_"));
        assert!(dir_prefix(1).starts_with(b"_fs_"));
        assert!(page_key(1, 0).starts_with(b"_fs_"));
        assert!(page_prefix(1).starts_with(b"_fs_"));
        assert!(staging_write_key(1).starts_with(b"_fs_"));
        assert!(orphan_inode_key(1).starts_with(b"_fs_"));
    }

    #[test]
    fn test_big_endian_ordering() {
        // Verify that big-endian encoding preserves lexicographic order
        let key1 = inode_key(1);
        let key2 = inode_key(2);
        assert!(key1 < key2, "inode_key(1) should be < inode_key(2)");

        let page_key_0 = page_key(1, 0);
        let page_key_1 = page_key(1, 1);
        assert!(
            page_key_0 < page_key_1,
            "page_key(1, 0) should be < page_key(1, 1)"
        );
    }

    #[test]
    fn test_dir_entry_ordering() {
        // Directory entries with the same parent should be ordered by name
        let entry_a = dir_entry_key(1, "a.txt");
        let entry_b = dir_entry_key(1, "b.txt");
        assert!(
            entry_a < entry_b,
            "dir_entry_key(1, 'a.txt') should be < dir_entry_key(1, 'b.txt')"
        );
    }

    #[test]
    fn test_different_parents_different_keys() {
        let entry1 = dir_entry_key(1, "file.txt");
        let entry2 = dir_entry_key(2, "file.txt");
        assert_ne!(entry1, entry2);
        assert!(entry1 < entry2, "parent 1 should sort before parent 2");
    }
}
