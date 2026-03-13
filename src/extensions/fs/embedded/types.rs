use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const PAGE_SIZE: usize = 16 * 1024;
pub(crate) const ROOT_INODE: u64 = 1;
pub(crate) const FS9_STORAGE_FORMAT_VERSION: u32 = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ObjectStoreBinding {
    pub(crate) bucket: String,
    #[serde(default)]
    pub(crate) region: Option<String>,
    #[serde(default)]
    pub(crate) endpoint: Option<String>,
    #[serde(default)]
    pub(crate) prefix: String,
    #[serde(default)]
    pub(crate) force_path_style: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FsInstanceIdentity {
    pub(crate) keyspace: String,
    pub(crate) fs_instance_id: [u8; 16],
}

impl FsInstanceIdentity {
    pub(crate) fn new(keyspace: String, fs_instance_id: [u8; 16]) -> Self {
        Self {
            keyspace,
            fs_instance_id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Superblock {
    #[serde(default)]
    pub(crate) format_version: u32,
    #[serde(default)]
    pub(crate) fs_instance_id: [u8; 16],
    #[serde(default)]
    pub(crate) object_store: Option<ObjectStoreBinding>,
}

impl Superblock {
    pub(crate) fn new(fs_instance_id: [u8; 16], object_store: Option<ObjectStoreBinding>) -> Self {
        Self {
            format_version: FS9_STORAGE_FORMAT_VERSION,
            fs_instance_id,
            object_store,
        }
    }
}

impl Default for Superblock {
    fn default() -> Self {
        Self::new([0u8; 16], None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum InodeType {
    File,
    Directory,
}

/// Durable reference to where file content is stored.
///
/// This is metadata-plane state (stored in the inode). Transitional lifecycle
/// state must live under `_fs_L{inode}` keys, not duplicated here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum DataRef {
    /// No data (empty file or directory).
    #[default]
    None,
    /// Data stored as a single TiKV KV pair (`_fs_B{inode}`).
    InlineBlob,
    /// Data stored as a slice within an immutable bundle/packfile in object storage.
    PackEntry {
        bundle_id: u64,
        offset: u64,
        len: u32,
        checksum: [u8; 32],
        generation: u64,
    },
    /// Data stored as a single object in object storage.
    Object {
        key: String,
        version: u64,
        checksum: [u8; 32],
    },
    /// Internal TiKV paged staging buffer (`_fs_P{inode}:{page}`).
    ///
    /// This is not a published file storage class. It exists only as an
    /// internal spool for stream writes before final routing to `InlineBlob`
    /// or `Object`.
    StagingPages,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Inode {
    pub(crate) id: u64,
    pub(crate) inode_type: InodeType,
    pub(crate) mode: u32,
    pub(crate) size: u64,
    /// Logical generation counter for this file's contents/metadata updates.
    ///
    /// This is used for path-level CAS (e.g. long-lived uploads) to detect
    /// intervening in-place mutations where inode id stays constant.
    #[serde(default)]
    pub(crate) generation: u64,
    #[serde(default)]
    pub(crate) data: DataRef,
    pub(crate) atime: i64,
    pub(crate) mtime: i64,
    pub(crate) ctime: i64,
    pub(crate) nlink: u32,
}

impl Inode {
    pub(crate) fn new_file(id: u64, mode: u32) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        Self {
            id,
            inode_type: InodeType::File,
            mode,
            size: 0,
            generation: 1,
            data: DataRef::None,
            atime: now,
            mtime: now,
            ctime: now,
            nlink: 1,
        }
    }

    pub(crate) fn new_directory(id: u64, mode: u32) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        Self {
            id,
            inode_type: InodeType::Directory,
            mode,
            size: 0,
            generation: 1,
            data: DataRef::None,
            atime: now,
            mtime: now,
            ctime: now,
            nlink: 2,
        }
    }

    pub(crate) fn is_directory(&self) -> bool {
        self.inode_type == InodeType::Directory
    }

    pub(crate) fn touch_mtime(&mut self) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        self.mtime = now;
        self.ctime = now;
    }

    pub(crate) fn touch_atime(&mut self) {
        self.atime = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
    }
}

#[derive(Debug)]
pub(crate) enum EmbeddedFsError {
    NotFound(String),
    AlreadyExists(String),
    IsDirectory(String),
    NotDirectory(String),
    DirectoryNotEmpty(String),
    PermissionDenied(String),
    Conflict(String),
    RestartRequired(String),
    InvalidInput(String),
    Internal(String),
}

impl EmbeddedFsError {
    pub(crate) fn not_found(path: &str) -> Self {
        Self::NotFound(path.to_string())
    }

    pub(crate) fn already_exists(path: &str) -> Self {
        Self::AlreadyExists(path.to_string())
    }

    pub(crate) fn is_directory(path: &str) -> Self {
        Self::IsDirectory(path.to_string())
    }

    pub(crate) fn not_directory(path: &str) -> Self {
        Self::NotDirectory(path.to_string())
    }

    pub(crate) fn directory_not_empty(path: &str) -> Self {
        Self::DirectoryNotEmpty(path.to_string())
    }

    pub(crate) fn conflict(msg: &str) -> Self {
        Self::Conflict(msg.to_string())
    }

    pub(crate) fn restart_required(msg: &str) -> Self {
        Self::RestartRequired(msg.to_string())
    }

    pub(crate) fn internal(msg: &str) -> Self {
        Self::Internal(msg.to_string())
    }
}

impl std::fmt::Display for EmbeddedFsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(msg) => write!(f, "embedded_fs: NotFound: {}", msg),
            Self::AlreadyExists(msg) => write!(f, "embedded_fs: AlreadyExists: {}", msg),
            Self::IsDirectory(msg) => write!(f, "embedded_fs: IsDirectory: {}", msg),
            Self::NotDirectory(msg) => write!(f, "embedded_fs: NotDirectory: {}", msg),
            Self::DirectoryNotEmpty(msg) => write!(f, "embedded_fs: DirectoryNotEmpty: {}", msg),
            Self::PermissionDenied(msg) => write!(f, "embedded_fs: PermissionDenied: {}", msg),
            Self::Conflict(msg) => write!(f, "embedded_fs: Conflict: {}", msg),
            Self::RestartRequired(msg) => write!(f, "embedded_fs: RestartRequired: {}", msg),
            Self::InvalidInput(msg) => write!(f, "embedded_fs: InvalidInput: {}", msg),
            Self::Internal(msg) => write!(f, "embedded_fs: Internal: {}", msg),
        }
    }
}

impl std::error::Error for EmbeddedFsError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superblock_default_matches_contract() {
        let sb = Superblock::default();
        assert_eq!(sb.format_version, FS9_STORAGE_FORMAT_VERSION);
        assert_eq!(sb.fs_instance_id, [0u8; 16]);
        assert_eq!(sb.object_store, None);
    }

    #[test]
    fn superblock_without_format_version_defaults_to_legacy_layout() {
        let superblock_json = r#"{}"#;

        let sb: Superblock =
            serde_json::from_str(superblock_json).expect("legacy superblock JSON must parse");
        assert_eq!(sb.format_version, 0);
        assert_eq!(sb.fs_instance_id, [0u8; 16]);
        assert_eq!(sb.object_store, None);
    }

    #[test]
    fn inode_new_file_initializes_expected_fields() {
        let inode = Inode::new_file(42, 0o644);
        assert_eq!(inode.id, 42);
        assert_eq!(inode.inode_type, InodeType::File);
        assert_eq!(inode.mode, 0o644);
        assert_eq!(inode.size, 0);
        assert_eq!(inode.generation, 1);
        assert_eq!(inode.data, DataRef::None);
        assert_eq!(inode.nlink, 1);
        assert!(!inode.is_directory());
        assert!(inode.atime > 0);
        assert!(inode.mtime > 0);
        assert!(inode.ctime > 0);
    }

    #[test]
    fn inode_new_directory_initializes_expected_fields() {
        let inode = Inode::new_directory(7, 0o755);
        assert_eq!(inode.id, 7);
        assert_eq!(inode.inode_type, InodeType::Directory);
        assert_eq!(inode.mode, 0o755);
        assert_eq!(inode.size, 0);
        assert_eq!(inode.generation, 1);
        assert_eq!(inode.data, DataRef::None);
        assert_eq!(inode.nlink, 2);
        assert!(inode.is_directory());
    }

    #[test]
    fn touch_methods_update_timestamps() {
        let mut inode = Inode::new_file(1, 0o600);

        inode.atime = 0;
        inode.mtime = 0;
        inode.ctime = 0;

        inode.touch_atime();
        assert!(inode.atime > 0);
        assert_eq!(inode.mtime, 0);
        assert_eq!(inode.ctime, 0);

        inode.touch_mtime();
        assert!(inode.mtime > 0);
        assert!(inode.ctime > 0);
    }

    #[test]
    fn embedded_fs_error_constructors_and_display_are_consistent() {
        let e = EmbeddedFsError::not_found("/a");
        assert_eq!(e.to_string(), "embedded_fs: NotFound: /a");

        let e = EmbeddedFsError::already_exists("/b");
        assert_eq!(e.to_string(), "embedded_fs: AlreadyExists: /b");

        let e = EmbeddedFsError::is_directory("/c");
        assert_eq!(e.to_string(), "embedded_fs: IsDirectory: /c");

        let e = EmbeddedFsError::not_directory("/d");
        assert_eq!(e.to_string(), "embedded_fs: NotDirectory: /d");

        let e = EmbeddedFsError::directory_not_empty("/e");
        assert_eq!(e.to_string(), "embedded_fs: DirectoryNotEmpty: /e");

        let e = EmbeddedFsError::internal("boom");
        assert_eq!(e.to_string(), "embedded_fs: Internal: boom");

        let e = EmbeddedFsError::PermissionDenied("/f".to_string());
        assert_eq!(e.to_string(), "embedded_fs: PermissionDenied: /f");

        let e = EmbeddedFsError::restart_required("restart db9-server");
        assert_eq!(
            e.to_string(),
            "embedded_fs: RestartRequired: restart db9-server"
        );
    }

    #[test]
    fn inode_json_without_data_ref_defaults_to_none() {
        let inode_json = r#"{
            "id": 1,
            "inode_type": "File",
            "mode": 420,
            "size": 123,
            "atime": 1,
            "mtime": 2,
            "ctime": 3,
            "nlink": 1
        }"#;

        let inode: Inode =
            serde_json::from_str(inode_json).expect("inode JSON without data field must parse");
        assert_eq!(inode.id, 1);
        assert_eq!(inode.size, 123);
        assert_eq!(inode.data, DataRef::None);
    }
}
