use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio::io::{AsyncBufRead, AsyncReadExt, BufReader};

use serde::Deserialize;

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

    /// Remove a file or empty directory at the given path.
    async fn remove(&self, path: &str) -> Result<()>;

    fn as_any(&self) -> &dyn std::any::Any;
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

    async fn remove(&self, path: &str) -> Result<()> {
        let info = self.stat(path).await?;
        if info.is_dir {
            tokio::fs::remove_dir(path)
                .await
                .map_err(|err| anyhow!("fs9_remove: cannot remove directory '{path}': {err}"))?;
        } else {
            tokio::fs::remove_file(path)
                .await
                .map_err(|err| anyhow!("fs9_remove: cannot remove file '{path}': {err}"))?;
        }
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

static BACKEND: OnceLock<LocalFsBackend> = OnceLock::new();

pub(crate) fn local_backend() -> &'static LocalFsBackend {
    BACKEND.get_or_init(LocalFsBackend::new)
}

// ---------------------------------------------------------------------------
// Remote fs9-server backend
// ---------------------------------------------------------------------------

/// JSON response from fs9-server's /api/v1/stat and /api/v1/readdir.
#[derive(Deserialize)]
struct FileInfoResponse {
    path: String,
    size: u64,
    file_type: String,
    mode: u32,
    #[allow(dead_code)]
    uid: u32,
    #[allow(dead_code)]
    gid: u32,
    #[allow(dead_code)]
    atime: u64,
    mtime: u64,
    #[allow(dead_code)]
    ctime: u64,
    #[allow(dead_code)]
    etag: Option<String>,
    #[allow(dead_code)]
    symlink_target: Option<String>,
}

impl FileInfoResponse {
    fn into_file_info(self) -> FsFileInfo {
        FsFileInfo {
            path: self.path,
            is_dir: self.file_type == "directory",
            is_file: self.file_type == "regular",
            is_symlink: self.file_type == "symlink",
            size: self.size,
            mode: self.mode,
            mtime: self.mtime,
        }
    }
}

/// JSON error response from fs9-server.
#[derive(Deserialize)]
struct Fs9ErrorResponse {
    error: String,
    #[allow(dead_code)]
    code: Option<u16>,
}

pub(crate) struct Fs9HttpBackend {
    client: reqwest::Client,
    base_url: String,
    token: String,
}

impl Fs9HttpBackend {
    fn new(base_url: String, token: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            token,
        }
    }

    async fn check_error(&self, resp: reqwest::Response, path: &str) -> Result<reqwest::Response> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status().as_u16();
        let body = resp
            .json::<Fs9ErrorResponse>()
            .await
            .map(|e| e.error)
            .unwrap_or_else(|_| format!("HTTP {status}"));
        match status {
            404 => Err(anyhow!("fs9: file not found: {path}")),
            403 => Err(anyhow!("fs9: permission denied: {path}")),
            _ => Err(anyhow!("fs9: remote error for '{path}': {body}")),
        }
    }
}

#[async_trait]
impl FsBackend for Fs9HttpBackend {
    async fn stat(&self, path: &str) -> Result<FsFileInfo> {
        let resp = self
            .client
            .get(format!("{}/api/v1/stat", self.base_url))
            .bearer_auth(&self.token)
            .query(&[("path", path)])
            .send()
            .await
            .map_err(|e| anyhow!("fs9: cannot reach fs9-server: {e}"))?;
        let resp = self.check_error(resp, path).await?;
        let info: FileInfoResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("fs9: invalid response from fs9-server: {e}"))?;
        Ok(info.into_file_info())
    }

    async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>> {
        let resp = self
            .client
            .get(format!("{}/api/v1/readdir", self.base_url))
            .bearer_auth(&self.token)
            .query(&[("path", path)])
            .send()
            .await
            .map_err(|e| anyhow!("fs9: cannot reach fs9-server: {e}"))?;
        let resp = self.check_error(resp, path).await?;
        let entries: Vec<FileInfoResponse> = resp
            .json()
            .await
            .map_err(|e| anyhow!("fs9: invalid response from fs9-server: {e}"))?;
        let mut out: Vec<FsFileInfo> = entries.into_iter().map(|e| e.into_file_info()).collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let resp = self
            .client
            .get(format!("{}/api/v1/download", self.base_url))
            .bearer_auth(&self.token)
            .query(&[("path", path)])
            .send()
            .await
            .map_err(|e| anyhow!("fs9: cannot reach fs9-server: {e}"))?;
        let resp = self.check_error(resp, path).await?;

        // Check Content-Length header if present before downloading.
        if let Some(len) = resp.content_length() {
            if len > max_bytes as u64 {
                return Err(anyhow!(
                    "fs9: file too large: {path} ({len} bytes, max {max_bytes})"
                ));
            }
        }

        let data = resp
            .bytes()
            .await
            .map_err(|e| anyhow!("fs9: cannot download '{path}': {e}"))?;
        if data.len() > max_bytes {
            return Err(anyhow!(
                "fs9: file too large: {path} ({} bytes, max {max_bytes})",
                data.len()
            ));
        }
        Ok(data.to_vec())
    }

    async fn read_file_stream(
        &self,
        path: &str,
        max_bytes: usize,
    ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
        // Download the entire file into memory (max 10MB) and wrap in a Cursor.
        let data = self.read_file(path, max_bytes).await?;
        Ok(Box::new(std::io::Cursor::new(data)))
    }

    async fn remove(&self, path: &str) -> Result<()> {
        let resp = self
            .client
            .delete(format!("{}/api/v1/remove", self.base_url))
            .bearer_auth(&self.token)
            .query(&[("path", path)])
            .send()
            .await
            .map_err(|e| anyhow!("fs9: cannot reach fs9-server: {e}"))?;
        self.check_error(resp, path).await?;
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl Fs9HttpBackend {
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

// ---------------------------------------------------------------------------
// Backend factory
// ---------------------------------------------------------------------------

/// Returns `true` when the remote fs9-server backend is configured.
pub(crate) fn is_remote_configured() -> bool {
    std::env::var("FS9_SERVER_URL").is_ok() && std::env::var("FS9_JWT_SECRET").is_ok()
}

/// Build a backend for the given tenant keyspace.
///
/// When `FS9_SERVER_URL` and `FS9_JWT_SECRET` are set, returns an `Fs9HttpBackend`
/// that calls the remote fs9-server with a minted JWT. Otherwise falls back to
/// the local filesystem backend.
pub(crate) fn get_backend(tenant_keyspace: &str) -> Box<dyn FsBackend> {
    let (base_url, secret) = match (
        std::env::var("FS9_SERVER_URL"),
        std::env::var("FS9_JWT_SECRET"),
    ) {
        (Ok(url), Ok(secret)) => (url, secret),
        _ => return Box::new(LocalFsBackend::new()),
    };

    let tenant_id = tenant_keyspace
        .strip_prefix("tipg_tenant_")
        .unwrap_or(tenant_keyspace);

    let token = match mint_jwt(tenant_id, &secret) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("fs9: failed to mint JWT for tenant '{}': {}", tenant_id, e);
            return Box::new(LocalFsBackend::new());
        }
    };

    Box::new(Fs9HttpBackend::new(base_url, token))
}

fn mint_jwt(tenant_id: &str, secret: &str) -> Result<String> {
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use serde::Serialize;

    #[derive(Serialize)]
    struct Claims<'a> {
        sub: &'a str,
        ns: &'a str,
        roles: [&'a str; 1],
        exp: u64,
        iat: u64,
    }

    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| anyhow!("clock error: {e}"))?
        .as_secs();

    let claims = Claims {
        sub: "pgtikv",
        ns: tenant_id,
        roles: ["admin"],
        exp: now + 300, // 5 minutes
        iat: now,
    };

    let header = Header::new(Algorithm::HS256);
    let key = EncodingKey::from_secret(secret.as_bytes());
    encode(&header, &claims, &key).map_err(|e| anyhow!("JWT encode error: {e}"))
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
