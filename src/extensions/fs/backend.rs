use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio::io::{AsyncBufRead, AsyncReadExt, BufReader};

/// Metadata about a filesystem entry.
#[derive(Debug, Clone)]
pub(crate) struct FsFileInfo {
    /// Full path to the entry.
    pub path: String,
    /// Whether this entry is a directory.
    pub is_dir: bool,
    /// Whether this entry is a regular file.
    pub is_file: bool,
    /// Whether this entry is a symbolic link.
    pub is_symlink: bool,
    /// File size in bytes (0 for directories).
    pub size: u64,
    /// Unix permission mode (e.g., 0o644). 0 on non-Unix platforms.
    pub mode: u32,
    /// Last modification time as Unix timestamp (seconds since epoch).
    pub mtime: u64,
}

/// Abstraction over filesystem operations.
/// `LocalFsBackend` uses `tokio::fs`; a future `Fs9HttpBackend` will call the remote fs9 server.
#[async_trait]
pub(crate) trait FsBackend: Send + Sync {
    /// Get metadata for a single path.
    async fn stat(&self, path: &str) -> Result<FsFileInfo>;
    /// List directory entries (non-recursive). Returns error if path is not a directory.
    async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>>;
    /// Read entire file contents. Returns error if file exceeds max_bytes.
    async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>>;
    /// Check if a path exists.
    async fn exists(&self, path: &str) -> Result<bool>;

    /// Open a file for streaming line-by-line reads.
    ///
    /// Returns a buffered async reader limited to `max_bytes`.
    /// The caller is responsible for reading lines from it.
    /// The path must be a regular file (not a directory or special file).
    async fn read_file_stream(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>>;
}

pub(crate) struct LocalFsBackend;

impl LocalFsBackend {
    pub(crate) fn new() -> Self {
        Self
    }
}

fn map_stat_error(path: &str, err: io::Error) -> anyhow::Error {
    match err.kind() {
        io::ErrorKind::NotFound => anyhow!("fs9: file not found: {path}"),
        io::ErrorKind::PermissionDenied => anyhow!("fs9: permission denied: {path}"),
        _ => anyhow!("fs9: cannot stat '{path}': {err}"),
    }
}

#[cfg(unix)]
fn metadata_mode(metadata: &std::fs::Metadata, _is_dir: bool) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn metadata_mode(_metadata: &std::fs::Metadata, is_dir: bool) -> u32 {
    if is_dir {
        0o755
    } else {
        0o644
    }
}

fn to_file_info(path: &str, metadata: std::fs::Metadata, is_symlink: bool) -> Result<FsFileInfo> {
    let is_dir = metadata.is_dir();
    let is_file = metadata.is_file();
    let mtime = metadata
        .modified()
        .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?
        .duration_since(UNIX_EPOCH)
        .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?
        .as_secs();

    Ok(FsFileInfo {
        path: path.to_string(),
        is_dir,
        is_file,
        is_symlink,
        size: metadata.len(),
        mode: metadata_mode(&metadata, is_dir),
        mtime,
    })
}

#[async_trait]
impl FsBackend for LocalFsBackend {
    async fn stat(&self, path: &str) -> Result<FsFileInfo> {
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|err| map_stat_error(path, err))?;
        to_file_info(path, metadata, false)
    }

    async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>> {
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|err| map_stat_error(path, err))?;
        if !metadata.is_dir() {
            return Err(anyhow!("fs9: not a directory: {path}"));
        }

        let mut rd = tokio::fs::read_dir(path)
            .await
            .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?;
        let mut out = Vec::new();

        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|err| anyhow!("fs9: cannot stat '{path}': {err}"))?
        {
            let entry_path: PathBuf = entry.path();
            let entry_path = entry_path.to_string_lossy().to_string();
            let is_symlink = entry
                .file_type()
                .await
                .map(|ft| ft.is_symlink())
                .unwrap_or(false);
            let metadata = tokio::fs::metadata(&entry_path)
                .await
                .map_err(|err| map_stat_error(&entry_path, err))?;
            out.push(to_file_info(&entry_path, metadata, is_symlink)?);
        }

        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let info = self.stat(path).await?;
        if info.is_dir {
            return Err(anyhow!("fs9: is a directory: {path}"));
        }
        if !info.is_file {
            return Err(anyhow!("fs9: not a regular file: {path}"));
        }
        if info.size > max_bytes as u64 {
            return Err(anyhow!(
                "fs9: file too large: {path} ({} bytes, max {max_bytes})",
                info.size
            ));
        }
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|err| anyhow!("fs9: cannot read file '{path}': {err}"))?;

        let mut buf = Vec::new();
        let mut limited = file.take(
            u64::try_from(max_bytes)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        );
        limited
            .read_to_end(&mut buf)
            .await
            .map_err(|err| anyhow!("fs9: cannot read file '{path}': {err}"))?;

        if buf.len() > max_bytes {
            return Err(anyhow!(
                "fs9: file too large: {path} (exceeded max {max_bytes} bytes)"
            ));
        }

        Ok(buf)
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        match tokio::fs::metadata(path).await {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(anyhow!("fs9: cannot stat '{path}': {err}")),
        }
    }

    async fn read_file_stream(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
        let info = self.stat(path).await?;
        if info.is_dir {
            return Err(anyhow!("fs9: is a directory: {path}"));
        }
        if !info.is_file {
            return Err(anyhow!("fs9: not a regular file: {path}"));
        }
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|err| anyhow!("fs9: cannot read file '{path}': {err}"))?;
        let limited = file.take(max_bytes as u64);
        Ok(Box::new(BufReader::new(limited)))
    }
}

static BACKEND: OnceLock<LocalFsBackend> = OnceLock::new();

pub(crate) fn local_backend() -> &'static LocalFsBackend {
    BACKEND.get_or_init(LocalFsBackend::new)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{FsBackend, LocalFsBackend};

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    fn unique_base(name: &str) -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = PathBuf::from(format!(
            "/tmp/pgtikv-fs9-backend-test-{name}-{}-{id}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn cleanup(path: &PathBuf) {
        let _ = fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn test_stat_existing_file() {
        let backend = LocalFsBackend::new();
        let dir = unique_base("stat-existing-file");
        let file = dir.join("hello.txt");
        fs::write(&file, b"hello").expect("write test file");

        let info = backend
            .stat(&file.to_string_lossy())
            .await
            .expect("stat file");
        assert!(!info.is_dir);
        assert!(info.size > 0);
        assert!(info.mtime > 0);
        assert_eq!(info.path, file.to_string_lossy());

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_stat_nonexistent() {
        let backend = LocalFsBackend::new();
        let path = format!(
            "/tmp/pgtikv-fs9-backend-missing-stat-{}",
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let err = backend
            .stat(&path)
            .await
            .expect_err("stat missing should fail");
        assert!(err.to_string().contains("fs9: file not found"));
    }

    #[tokio::test]
    async fn test_stat_directory() {
        let backend = LocalFsBackend::new();
        let info = backend.stat("/tmp").await.expect("stat /tmp");
        assert!(info.is_dir);
    }

    #[tokio::test]
    async fn test_readdir_directory() {
        let backend = LocalFsBackend::new();
        let dir = unique_base("readdir-directory");
        fs::write(dir.join("a.txt"), b"a").expect("write a");
        fs::write(dir.join("b.txt"), b"b").expect("write b");
        fs::create_dir_all(dir.join("subdir")).expect("create subdir");

        let entries = backend
            .readdir(&dir.to_string_lossy())
            .await
            .expect("readdir");
        assert_eq!(entries.len(), 3);
        let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                dir.join("a.txt").to_string_lossy().to_string(),
                dir.join("b.txt").to_string_lossy().to_string(),
                dir.join("subdir").to_string_lossy().to_string(),
            ]
        );

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_readdir_on_file() {
        let backend = LocalFsBackend::new();
        let dir = unique_base("readdir-on-file");
        let file = dir.join("file.txt");
        fs::write(&file, b"x").expect("write file");

        let err = backend
            .readdir(&file.to_string_lossy())
            .await
            .expect_err("readdir file should fail");
        assert!(err.to_string().contains("fs9: not a directory"));

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_read_file_existing() {
        let backend = LocalFsBackend::new();
        let dir = unique_base("read-file-existing");
        let file = dir.join("content.txt");
        fs::write(&file, b"hello world").expect("write content");

        let data = backend
            .read_file(&file.to_string_lossy(), 1024)
            .await
            .expect("read file");
        assert_eq!(data, b"hello world");

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_read_file_empty() {
        let backend = LocalFsBackend::new();
        let dir = unique_base("read-file-empty");
        let file = dir.join("empty.txt");
        fs::write(&file, b"").expect("write empty");

        let data = backend
            .read_file(&file.to_string_lossy(), 1024)
            .await
            .expect("read empty file");
        assert_eq!(data, Vec::<u8>::new());

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_read_file_too_large() {
        let backend = LocalFsBackend::new();
        let dir = unique_base("read-file-too-large");
        let file = dir.join("too-big.txt");
        fs::write(&file, b"ab").expect("write data");

        let err = backend
            .read_file(&file.to_string_lossy(), 1)
            .await
            .expect_err("expected too large error");
        assert!(err.to_string().contains("fs9: file too large"));

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_read_file_directory() {
        let backend = LocalFsBackend::new();
        let dir = unique_base("read-file-directory");

        let err = backend
            .read_file(&dir.to_string_lossy(), 1024)
            .await
            .expect_err("expected directory error");
        assert!(err.to_string().contains("fs9: is a directory"));

        cleanup(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_read_file_rejects_non_regular_files() {
        let backend = LocalFsBackend::new();
        let err = backend
            .read_file("/dev/null", 1024)
            .await
            .expect_err("expected non-regular file error");
        assert!(err.to_string().contains("fs9: not a regular file"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_read_file_enforces_max_bytes_when_metadata_len_is_zero() {
        // procfs files can report len=0 but still produce content. Ensure we still enforce max_bytes.
        let backend = LocalFsBackend::new();
        let err = backend
            .read_file("/proc/self/stat", 1)
            .await
            .expect_err("expected too large error");
        assert!(err.to_string().contains("fs9: file too large"));
    }

    #[tokio::test]
    async fn test_exists_true() {
        let backend = LocalFsBackend::new();
        assert!(backend.exists("/tmp").await.expect("exists /tmp"));
    }

    #[tokio::test]
    async fn test_exists_false() {
        let backend = LocalFsBackend::new();
        let path = format!(
            "/tmp/pgtikv-fs9-backend-missing-exists-{}",
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        assert!(!backend.exists(&path).await.expect("exists missing"));
    }

    #[tokio::test]
    async fn test_read_file_stream_basic() {
        use tokio::io::AsyncBufReadExt;

        let backend = LocalFsBackend::new();
        let dir = unique_base("stream-basic");
        let file = dir.join("lines.txt");
        fs::write(&file, b"line1\nline2\nline3\n").expect("write");

        let mut reader = backend
            .read_file_stream(&file.to_string_lossy(), 1024)
            .await
            .expect("stream file");

        let mut lines = Vec::new();
        let mut buf = String::new();
        while reader.read_line(&mut buf).await.expect("read") > 0 {
            lines.push(buf.trim_end_matches('\n').to_string());
            buf.clear();
        }
        assert_eq!(lines, vec!["line1", "line2", "line3"]);

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_read_file_stream_matches_batch() {
        use tokio::io::AsyncReadExt;

        let backend = LocalFsBackend::new();
        let dir = unique_base("stream-vs-batch");
        let file = dir.join("data.txt");
        let content = b"hello world\nfoo bar\n";
        fs::write(&file, content).expect("write");

        let batch = backend
            .read_file(&file.to_string_lossy(), 1024)
            .await
            .expect("batch read");

        let mut stream = backend
            .read_file_stream(&file.to_string_lossy(), 1024)
            .await
            .expect("stream read");
        let mut stream_buf = Vec::new();
        stream
            .read_to_end(&mut stream_buf)
            .await
            .expect("stream to end");

        assert_eq!(batch, stream_buf);

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_read_file_stream_respects_max_bytes() {
        use tokio::io::AsyncReadExt;

        let backend = LocalFsBackend::new();
        let dir = unique_base("stream-max-bytes");
        let file = dir.join("big.txt");
        fs::write(&file, b"abcdefghijklmnop").expect("write 16 bytes");

        let mut reader = backend
            .read_file_stream(&file.to_string_lossy(), 5)
            .await
            .expect("stream with limit");
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).await.expect("read");
        assert_eq!(buf.len(), 5);
        assert_eq!(&buf, b"abcde");

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_read_file_stream_rejects_directory() {
        let backend = LocalFsBackend::new();
        let dir = unique_base("stream-dir");

        match backend.read_file_stream(&dir.to_string_lossy(), 1024).await {
            Err(e) => assert!(e.to_string().contains("fs9: is a directory")),
            Ok(_) => panic!("expected error for directory"),
        }

        cleanup(&dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_read_file_stream_rejects_non_regular_files() {
        let backend = LocalFsBackend::new();
        match backend.read_file_stream("/dev/null", 1024).await {
            Err(e) => assert!(e.to_string().contains("fs9: not a regular file")),
            Ok(_) => panic!("expected error for non-regular file"),
        }
    }

    #[tokio::test]
    async fn test_read_file_stream_empty_file() {
        use tokio::io::AsyncReadExt;

        let backend = LocalFsBackend::new();
        let dir = unique_base("stream-empty");
        let file = dir.join("empty.txt");
        fs::write(&file, b"").expect("write empty");

        let mut reader = backend
            .read_file_stream(&file.to_string_lossy(), 1024)
            .await
            .expect("stream empty");
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).await.expect("read");
        assert!(buf.is_empty());

        cleanup(&dir);
    }
}
