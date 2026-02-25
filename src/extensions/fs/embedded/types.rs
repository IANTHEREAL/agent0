use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const PAGE_SIZE: usize = 16 * 1024;
pub(crate) const ROOT_INODE: u64 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Superblock {
    pub(crate) next_inode: u64,
    pub(crate) page_size: usize,
    pub(crate) total_pages: u64,
    pub(crate) used_pages: u64,
}

impl Default for Superblock {
    fn default() -> Self {
        Self {
            next_inode: ROOT_INODE + 1,
            page_size: PAGE_SIZE,
            total_pages: 1_000_000,
            used_pages: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum InodeType {
    File,
    Directory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Inode {
    pub(crate) id: u64,
    pub(crate) inode_type: InodeType,
    pub(crate) mode: u32,
    pub(crate) size: u64,
    pub(crate) page_count: u64,
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
            page_count: 0,
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
            page_count: 0,
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
            Self::Internal(msg) => write!(f, "embedded_fs: Internal: {}", msg),
        }
    }
}

impl std::error::Error for EmbeddedFsError {}


