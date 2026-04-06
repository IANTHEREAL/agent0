use crate::extensions::fs::{backend, sql_client::SqlFsClient};
use crate::model::Value;
use anyhow::{anyhow, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::SqlFn;

/// Global budget for concurrent fs9_read allocations (default 128MB).
/// Prevents OOM when many tenants read large files simultaneously.
/// Only covers the read window (fs::read allocation), not Value::Text lifetime.
const FS9_READ_BUDGET: usize = 128 * 1024 * 1024;
static FS9_READ_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// RAII guard that releases reserved bytes on drop, ensuring budget is
/// freed even if fs::read or UTF-8 conversion panics / errors.
struct ReadBudgetGuard(usize);

impl Drop for ReadBudgetGuard {
    fn drop(&mut self) {
        FS9_READ_IN_FLIGHT.fetch_sub(self.0, Ordering::Relaxed);
    }
}

/// Try to reserve `n` bytes from the global read budget.
/// Returns a guard that auto-releases on drop, or an error if budget exceeded.
fn reserve_read_budget(n: usize) -> Result<ReadBudgetGuard> {
    // CAS loop: only succeed if adding `n` stays within budget.
    loop {
        let current = FS9_READ_IN_FLIGHT.load(Ordering::Relaxed);
        if current + n > FS9_READ_BUDGET {
            return Err(anyhow!(
                "fs9_read: concurrent read budget exceeded ({} + {} > {} bytes). \
                 Try again later or use FROM extensions.fs9() for large files.",
                current,
                n,
                FS9_READ_BUDGET
            ));
        }
        if FS9_READ_IN_FLIGHT
            .compare_exchange_weak(current, current + n, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(ReadBudgetGuard(n));
        }
    }
}

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("FS9_READ", fs9_read);
    map.insert("FS9_WRITE", fs9_write);
    map.insert("FS9_EXISTS", fs9_exists);
    map.insert("FS9_SIZE", fs9_size);
    map.insert("FS9_MTIME", fs9_mtime);
    map.insert("FS9_REMOVE", fs9_remove);
    map.insert("FS9_MKDIR", fs9_mkdir);
    map.insert("FS9_READ_AT", fs9_read_at);
    map.insert("FS9_READ_BYTEA", fs9_read_bytea);
    map.insert("FS9_READ_AT_BYTEA", fs9_read_at_bytea);
    map.insert("FS9_WRITE_AT", fs9_write_at);
    map.insert("FS9_APPEND", fs9_append);
    map.insert("FS9_TRUNCATE", fs9_truncate);
}

fn ensure_permissions() -> Result<()> {
    if !backend::is_backend_available() {
        return Err(anyhow!("fs9: TiKV storage backend not available"));
    }
    if !crate::extensions::context::is_superuser() {
        return Err(anyhow!("fs9: permission denied (superuser required)"));
    }
    Ok(())
}

fn expect_text_arg(name: &str, arg: Value, position: usize) -> Result<Option<String>> {
    match arg {
        Value::Null => Ok(None),
        Value::Text(s) => Ok(Some(s)),
        other => Err(anyhow!(
            "{}: argument {} must be TEXT, got {}",
            name,
            position,
            other.type_display_name()
        )),
    }
}

fn expect_file_data_arg(name: &str, arg: Value, position: usize) -> Result<Option<Vec<u8>>> {
    match arg {
        Value::Null => Ok(None),
        Value::Text(s) => Ok(Some(s.into_bytes())),
        Value::Bytes(bytes) => Ok(Some(bytes)),
        other => Err(anyhow!(
            "{}: argument {} must be TEXT or BYTEA, got {}",
            name,
            position,
            other.type_display_name()
        )),
    }
}

/// Bridge sync SqlFn to async backend calls. Safe because scalar functions
/// run on the tokio worker pool and `block_in_place` is the documented pattern.
fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

fn get_client_sync() -> Result<SqlFsClient> {
    run_async(SqlFsClient::from_context())
}

fn checked_read_at_len(fn_name: &str, file_size: u64, offset: u64, length: usize) -> Result<usize> {
    if length == 0 || offset >= file_size {
        return Ok(0);
    }

    let requested_len =
        u64::try_from(length).map_err(|_| anyhow!("{fn_name}: length exceeds u64"))?;
    let actual_size = requested_len.min(file_size - offset);
    let max = u64::try_from(crate::extensions::fs::MAX_BYTES_PER_FILE)
        .map_err(|_| anyhow!("{fn_name}: max read size exceeds u64"))?;
    if actual_size > max {
        return Err(anyhow!(
            "{fn_name}: file too large: {} bytes exceeds limit {}",
            actual_size,
            crate::extensions::fs::MAX_BYTES_PER_FILE
        ));
    }

    usize::try_from(actual_size)
        .map_err(|_| anyhow!("{fn_name}: read length exceeds addressable memory"))
}

pub fn fs9_read(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let path = match expect_text_arg("fs9_read", args.first().cloned().unwrap_or(Value::Null), 0)? {
        Some(p) => p,
        None => return Ok(Value::Null),
    };
    let client = get_client_sync()?;
    let text = run_async(client.read_text(&path))?;
    let _budget = reserve_read_budget(text.len())?;
    Ok(Value::Text(text))
}

pub fn fs9_write(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let mut args_iter = args.into_iter();
    let path = expect_text_arg("fs9_write", args_iter.next().unwrap_or(Value::Null), 1)?;
    let content = expect_file_data_arg("fs9_write", args_iter.next().unwrap_or(Value::Null), 2)?;

    let (path, content) = match (path, content) {
        (Some(p), Some(c)) => (p, c),
        _ => return Ok(Value::Null),
    };

    if content.len() > crate::extensions::fs::MAX_BYTES_PER_FILE {
        return Err(anyhow!(
            "fs9_write: content too large: {} bytes (max {})",
            content.len(),
            crate::extensions::fs::MAX_BYTES_PER_FILE
        ));
    }

    let client = get_client_sync()?;
    let len = run_async(client.write_file(&path, &content, None))?;
    Ok(Value::Int64(len as i64))
}

pub fn fs9_exists(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let path = match expect_text_arg(
        "fs9_exists",
        args.into_iter().next().unwrap_or(Value::Null),
        1,
    )? {
        Some(p) => p,
        None => return Ok(Value::Null),
    };
    let client = get_client_sync()?;
    Ok(Value::Boolean(run_async(client.exists(&path))?))
}

pub fn fs9_size(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let path = match expect_text_arg(
        "fs9_size",
        args.into_iter().next().unwrap_or(Value::Null),
        1,
    )? {
        Some(p) => p,
        None => return Ok(Value::Null),
    };
    let client = get_client_sync()?;
    let info = run_async(client.stat(&path))?;
    Ok(Value::Int64(info.size as i64))
}

pub fn fs9_mtime(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let path = match expect_text_arg(
        "fs9_mtime",
        args.into_iter().next().unwrap_or(Value::Null),
        1,
    )? {
        Some(p) => p,
        None => return Ok(Value::Null),
    };
    let client = get_client_sync()?;
    let info = run_async(client.stat(&path))?;
    let dt =
        DateTime::<Utc>::from(std::time::UNIX_EPOCH + std::time::Duration::from_secs(info.mtime));
    Ok(Value::Text(dt.to_rfc3339_opts(SecondsFormat::Secs, true)))
}

pub fn fs9_remove(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let mut args_iter = args.into_iter();
    let path = match expect_text_arg("fs9_remove", args_iter.next().unwrap_or(Value::Null), 1)? {
        Some(p) => p,
        None => return Ok(Value::Null),
    };

    // Second argument: recursive (default false)
    let recursive = match args_iter.next() {
        Some(Value::Boolean(b)) => b,
        Some(Value::Null) | None => false,
        Some(other) => {
            return Err(anyhow!(
                "fs9_remove: argument 2 must be BOOLEAN, got {}",
                other.type_display_name()
            ))
        }
    };

    let client = get_client_sync()?;
    Ok(Value::Int64(run_async(client.remove(&path, recursive))?))
}

pub fn fs9_mkdir(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let mut args_iter = args.into_iter();
    let path = match expect_text_arg("fs9_mkdir", args_iter.next().unwrap_or(Value::Null), 1)? {
        Some(p) => p,
        None => return Ok(Value::Null),
    };

    // Second argument: recursive (default false)
    let recursive = match args_iter.next() {
        Some(Value::Boolean(b)) => b,
        Some(Value::Null) | None => false,
        Some(other) => {
            return Err(anyhow!(
                "fs9_mkdir: argument 2 must be BOOLEAN, got {}",
                other.type_display_name()
            ))
        }
    };

    let client = get_client_sync()?;
    run_async(client.mkdir(&path, recursive, None))?;
    Ok(Value::Boolean(true))
}

pub fn fs9_read_at(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let path = match expect_text_arg(
        "fs9_read_at",
        args.first().cloned().unwrap_or(Value::Null),
        0,
    )? {
        Some(p) => p,
        None => return Ok(Value::Null),
    };
    let offset = match args.get(1).unwrap_or(&Value::Null) {
        Value::Int64(v) => {
            if *v < 0 {
                return Err(anyhow!("fs9_read_at: offset must be non-negative"));
            }
            *v as u64
        }
        Value::Int32(v) => {
            if *v < 0 {
                return Err(anyhow!("fs9_read_at: offset must be non-negative"));
            }
            *v as u64
        }
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(anyhow!(
                "fs9_read_at: expected integer for offset, got {:?}",
                other
            ))
        }
    };
    let length = match args.get(2).unwrap_or(&Value::Null) {
        Value::Int64(v) => {
            if *v < 0 {
                return Err(anyhow!("fs9_read_at: length must be non-negative"));
            }
            *v as usize
        }
        Value::Int32(v) => {
            if *v < 0 {
                return Err(anyhow!("fs9_read_at: length must be non-negative"));
            }
            *v as usize
        }
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(anyhow!(
                "fs9_read_at: expected integer for length, got {:?}",
                other
            ))
        }
    };
    let client = get_client_sync()?;
    let info = run_async(client.stat(&path))?;
    let actual_len = checked_read_at_len("fs9_read_at", info.size, offset, length)?;
    let text = run_async(client.read_text_at(&path, offset, actual_len))?;
    let _budget = reserve_read_budget(text.len())?;
    Ok(Value::Text(text))
}

pub fn fs9_read_bytea(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let path = match expect_text_arg(
        "fs9_read_bytea",
        args.first().cloned().unwrap_or(Value::Null),
        0,
    )? {
        Some(p) => p,
        None => return Ok(Value::Null),
    };
    let client = get_client_sync()?;
    let bytes = run_async(client.read_bytes(&path))?;
    let _budget = reserve_read_budget(bytes.len())?;
    Ok(Value::Bytes(bytes))
}

pub fn fs9_read_at_bytea(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let path = match expect_text_arg(
        "fs9_read_at_bytea",
        args.first().cloned().unwrap_or(Value::Null),
        0,
    )? {
        Some(p) => p,
        None => return Ok(Value::Null),
    };
    let offset = match args.get(1).unwrap_or(&Value::Null) {
        Value::Int64(v) => {
            if *v < 0 {
                return Err(anyhow!("fs9_read_at_bytea: offset must be non-negative"));
            }
            *v as u64
        }
        Value::Int32(v) => {
            if *v < 0 {
                return Err(anyhow!("fs9_read_at_bytea: offset must be non-negative"));
            }
            *v as u64
        }
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(anyhow!(
                "fs9_read_at_bytea: expected integer for offset, got {:?}",
                other
            ))
        }
    };
    let length = match args.get(2).unwrap_or(&Value::Null) {
        Value::Int64(v) => {
            if *v < 0 {
                return Err(anyhow!("fs9_read_at_bytea: length must be non-negative"));
            }
            *v as usize
        }
        Value::Int32(v) => {
            if *v < 0 {
                return Err(anyhow!("fs9_read_at_bytea: length must be non-negative"));
            }
            *v as usize
        }
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(anyhow!(
                "fs9_read_at_bytea: expected integer for length, got {:?}",
                other
            ))
        }
    };
    let client = get_client_sync()?;
    let info = run_async(client.stat(&path))?;
    let actual_len = checked_read_at_len("fs9_read_at_bytea", info.size, offset, length)?;
    let bytes = run_async(client.read_bytes_at(&path, offset, actual_len))?;
    let _budget = reserve_read_budget(bytes.len())?;
    Ok(Value::Bytes(bytes))
}

pub fn fs9_write_at(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let path = match expect_text_arg(
        "fs9_write_at",
        args.first().cloned().unwrap_or(Value::Null),
        0,
    )? {
        Some(p) => p,
        None => return Ok(Value::Null),
    };
    let offset = match args.get(1).unwrap_or(&Value::Null) {
        Value::Int64(v) => {
            if *v < 0 {
                return Err(anyhow!("fs9_write_at: offset must be non-negative"));
            }
            *v as u64
        }
        Value::Int32(v) => {
            if *v < 0 {
                return Err(anyhow!("fs9_write_at: offset must be non-negative"));
            }
            *v as u64
        }
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(anyhow!(
                "fs9_write_at: expected integer for offset, got {:?}",
                other
            ))
        }
    };
    let data = match expect_file_data_arg(
        "fs9_write_at",
        args.get(2).cloned().unwrap_or(Value::Null),
        2,
    )? {
        Some(d) => d,
        None => return Ok(Value::Null),
    };
    if data.len() > crate::extensions::fs::MAX_BYTES_PER_FILE {
        return Err(anyhow!("fs9_write_at: data exceeds maximum file size"));
    }
    let client = get_client_sync()?;
    let written = run_async(client.write_file_at(&path, offset, &data))?;
    Ok(Value::Int64(written as i64))
}

pub fn fs9_append(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let path = match expect_text_arg(
        "fs9_append",
        args.first().cloned().unwrap_or(Value::Null),
        0,
    )? {
        Some(p) => p,
        None => return Ok(Value::Null),
    };
    let data =
        match expect_file_data_arg("fs9_append", args.get(1).cloned().unwrap_or(Value::Null), 1)? {
            Some(d) => d,
            None => return Ok(Value::Null),
        };
    if data.len() > crate::extensions::fs::MAX_BYTES_PER_FILE {
        return Err(anyhow!("fs9_append: data exceeds maximum file size"));
    }
    let client = get_client_sync()?;
    let written = run_async(client.append_file(&path, &data))?;
    Ok(Value::Int64(written as i64))
}

pub fn fs9_truncate(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let path = match expect_text_arg(
        "fs9_truncate",
        args.first().cloned().unwrap_or(Value::Null),
        0,
    )? {
        Some(p) => p,
        None => return Ok(Value::Null),
    };
    let size = match args.get(1).unwrap_or(&Value::Null) {
        Value::Int64(v) => {
            if *v < 0 {
                return Err(anyhow!("fs9_truncate: size must be non-negative"));
            }
            *v as u64
        }
        Value::Int32(v) => {
            if *v < 0 {
                return Err(anyhow!("fs9_truncate: size must be non-negative"));
            }
            *v as u64
        }
        Value::Null => return Ok(Value::Null),
        other => {
            return Err(anyhow!(
                "fs9_truncate: expected integer for size, got {:?}",
                other
            ))
        }
    };
    let client = get_client_sync()?;
    run_async(client.truncate(&path, size))?;
    Ok(Value::Boolean(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::context;
    use crate::extensions::fs::backend::{
        FsBackend, FsCreateUpload, FsFileInfo, FsMultipartCompletedPart, FsPreparedDownload,
        FsPresignedRequest, FsStorage, FsWriteStream, FsWriteStreamOptions,
    };
    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Arc;
    use tokio::io::{AsyncBufRead, BufReader};

    struct MockWriteStream;

    #[async_trait]
    impl FsWriteStream for MockWriteStream {
        async fn write_chunk(&mut self, _chunk: &[u8]) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn finish(self: Box<Self>) -> Result<usize> {
            anyhow::bail!("not implemented")
        }

        async fn abort(self: Box<Self>) -> Result<()> {
            anyhow::bail!("not implemented")
        }
    }

    #[derive(Clone)]
    struct MockFile {
        bytes: Vec<u8>,
        sealed: bool,
    }

    struct MockBackend {
        files: Mutex<HashMap<String, MockFile>>,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                files: Mutex::new(HashMap::new()),
            }
        }

        fn insert_file(&self, path: &str, bytes: Vec<u8>, sealed: bool) {
            self.files
                .lock()
                .insert(normalize_mock_path(path), MockFile { bytes, sealed });
        }
    }

    #[async_trait]
    impl FsBackend for MockBackend {
        async fn stat(&self, path: &str) -> Result<FsFileInfo> {
            let normalized = normalize_mock_path(path);
            if normalized == "/" {
                return Ok(FsFileInfo {
                    path: normalized,
                    is_dir: true,
                    is_symlink: false,
                    size: 0,
                    mode: 0o755,
                    generation: 1,
                    mtime: 0,
                    storage: None,
                    sealed: Some(false),
                });
            }

            let file = self
                .files
                .lock()
                .get(&normalized)
                .cloned()
                .ok_or_else(|| mock_not_found(path))?;
            Ok(FsFileInfo {
                path: normalized,
                is_dir: false,
                is_symlink: false,
                size: file.bytes.len() as u64,
                mode: 0o644,
                generation: 1,
                mtime: 0,
                storage: Some(if file.sealed {
                    FsStorage::Pack
                } else {
                    FsStorage::Inline
                }),
                sealed: Some(file.sealed),
            })
        }

        async fn readdir(&self, path: &str) -> Result<Vec<FsFileInfo>> {
            let normalized = normalize_mock_path(path);
            let prefix = if normalized == "/" {
                "/".to_string()
            } else {
                format!("{normalized}/")
            };

            let files = self.files.lock();
            let mut children = BTreeMap::new();
            for (file_path, file) in files.iter() {
                if !file_path.starts_with(&prefix) {
                    continue;
                }
                let suffix = &file_path[prefix.len()..];
                if suffix.is_empty() {
                    continue;
                }
                let name = suffix.split('/').next().unwrap_or_default();
                let child_path = if normalized == "/" {
                    format!("/{name}")
                } else {
                    format!("{normalized}/{name}")
                };
                let is_dir = suffix.contains('/');
                children
                    .entry(child_path.clone())
                    .or_insert_with(|| FsFileInfo {
                        path: child_path,
                        is_dir,
                        is_symlink: false,
                        size: if is_dir { 0 } else { file.bytes.len() as u64 },
                        mode: if is_dir { 0o755 } else { 0o644 },
                        generation: 1,
                        mtime: 0,
                        storage: if is_dir {
                            None
                        } else if file.sealed {
                            Some(FsStorage::Pack)
                        } else {
                            Some(FsStorage::Inline)
                        },
                        sealed: Some(if is_dir { false } else { file.sealed }),
                    });
            }
            Ok(children.into_values().collect())
        }

        async fn read_file(&self, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
            let normalized = normalize_mock_path(path);
            let file = self
                .files
                .lock()
                .get(&normalized)
                .cloned()
                .ok_or_else(|| mock_not_found(path))?;
            if file.bytes.len() > max_bytes {
                anyhow::bail!("too large");
            }
            Ok(file.bytes)
        }

        async fn read_file_stream(
            &self,
            _path: &str,
            _max_bytes: usize,
        ) -> Result<Box<dyn AsyncBufRead + Unpin + Send>> {
            Ok(Box::new(BufReader::new(tokio::io::empty())))
        }

        async fn remove(&self, path: &str) -> Result<()> {
            let normalized = normalize_mock_path(path);
            self.files
                .lock()
                .remove(&normalized)
                .ok_or_else(|| mock_not_found(path))?;
            Ok(())
        }

        async fn remove_recursive(&self, path: &str) -> Result<u64> {
            let normalized = normalize_mock_path(path);
            let prefix = if normalized == "/" {
                "/".to_string()
            } else {
                format!("{normalized}/")
            };
            let mut files = self.files.lock();
            let before = files.len();
            files
                .retain(|file_path, _| file_path != &normalized && !file_path.starts_with(&prefix));
            Ok((before - files.len()) as u64)
        }

        async fn mkdir(&self, _path: &str, _recursive: bool, _mode: Option<u32>) -> Result<()> {
            Ok(())
        }

        async fn write_file(&self, path: &str, data: &[u8], _mode: Option<u32>) -> Result<usize> {
            self.files.lock().insert(
                normalize_mock_path(path),
                MockFile {
                    bytes: data.to_vec(),
                    sealed: false,
                },
            );
            Ok(data.len())
        }

        async fn begin_write_stream(
            &self,
            _path: &str,
            _opts: FsWriteStreamOptions,
        ) -> Result<Box<dyn FsWriteStream>> {
            Ok(Box::new(MockWriteStream))
        }

        async fn read_file_at(&self, path: &str, offset: u64, length: usize) -> Result<Vec<u8>> {
            let normalized = normalize_mock_path(path);
            let file = self
                .files
                .lock()
                .get(&normalized)
                .cloned()
                .ok_or_else(|| mock_not_found(path))?;
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            if start >= file.bytes.len() || length == 0 {
                return Ok(Vec::new());
            }
            let end = start.saturating_add(length).min(file.bytes.len());
            Ok(file.bytes[start..end].to_vec())
        }

        async fn write_file_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<usize> {
            let normalized = normalize_mock_path(path);
            let mut files = self.files.lock();
            let file = files.entry(normalized).or_insert(MockFile {
                bytes: Vec::new(),
                sealed: false,
            });
            if file.sealed {
                anyhow::bail!("partial mutation is not supported for sealed files");
            }
            let start = usize::try_from(offset).map_err(|_| anyhow!("offset exceeds memory"))?;
            let end = start
                .checked_add(data.len())
                .ok_or_else(|| anyhow!("write range exceeds memory"))?;
            if file.bytes.len() < end {
                file.bytes.resize(end, 0);
            }
            file.bytes[start..end].copy_from_slice(data);
            Ok(data.len())
        }

        async fn append_file(&self, path: &str, data: &[u8]) -> Result<usize> {
            let normalized = normalize_mock_path(path);
            let mut files = self.files.lock();
            let file = files.entry(normalized).or_insert(MockFile {
                bytes: Vec::new(),
                sealed: false,
            });
            if file.sealed {
                anyhow::bail!("append is not supported for sealed files");
            }
            file.bytes.extend_from_slice(data);
            Ok(data.len())
        }

        async fn truncate(&self, path: &str, size: u64) -> Result<()> {
            let normalized = normalize_mock_path(path);
            let mut files = self.files.lock();
            let file = files
                .get_mut(&normalized)
                .ok_or_else(|| mock_not_found(path))?;
            if file.sealed {
                anyhow::bail!("truncate is not supported for sealed files");
            }
            let size = usize::try_from(size).map_err(|_| anyhow!("size exceeds memory"))?;
            if file.bytes.len() > size {
                file.bytes.truncate(size);
            } else {
                file.bytes.resize(size, 0);
            }
            Ok(())
        }

        async fn rename(&self, _old_path: &str, _new_path: &str) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn create_upload(
            &self,
            _path: &str,
            _expected_size: u64,
            _mode: Option<u32>,
            _checksum_algorithm: Option<&str>,
        ) -> Result<FsCreateUpload> {
            anyhow::bail!("not implemented")
        }

        async fn presign_upload_part(
            &self,
            _upload_token: &str,
            _part_number: i32,
            _checksum_crc32c: Option<&str>,
        ) -> Result<FsPresignedRequest> {
            anyhow::bail!("not implemented")
        }

        async fn complete_upload(
            &self,
            _upload_token: &str,
            _parts: Vec<FsMultipartCompletedPart>,
            _checksum: Option<[u8; 32]>,
        ) -> Result<usize> {
            anyhow::bail!("not implemented")
        }

        async fn abort_upload(&self, _upload_token: &str) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn prepare_download(&self, _path: &str) -> Result<FsPreparedDownload> {
            anyhow::bail!("not implemented")
        }

        async fn symlink(&self, _path: &str, _target: &str) -> Result<()> {
            anyhow::bail!("not implemented")
        }

        async fn readlink(&self, _path: &str) -> Result<String> {
            anyhow::bail!("not implemented")
        }

        async fn chmod(&self, _path: &str, _mode: u32) -> Result<()> {
            unreachable!("chmod is not used in these tests");
        }
    }

    fn normalize_mock_path(path: &str) -> String {
        let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        if parts.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", parts.join("/"))
        }
    }

    fn mock_not_found(path: &str) -> anyhow::Error {
        anyhow!(
            crate::extensions::fs::embedded::types::EmbeddedFsError::NotFound(normalize_mock_path(
                path
            ))
        )
    }

    #[test]
    fn expect_file_data_arg_accepts_text_and_bytes() {
        assert_eq!(
            expect_file_data_arg("fs9_write", Value::Text("abc".into()), 2).unwrap(),
            Some(b"abc".to_vec())
        );
        assert_eq!(
            expect_file_data_arg("fs9_write", Value::Bytes(vec![0xde, 0xad]), 2).unwrap(),
            Some(vec![0xde, 0xad])
        );
    }

    #[test]
    fn expect_file_data_arg_rejects_non_file_types() {
        let err = expect_file_data_arg("fs9_write", Value::Boolean(true), 2)
            .expect_err("boolean must be rejected");
        assert_eq!(
            err.to_string(),
            "fs9_write: argument 2 must be TEXT or BYTEA, got BOOLEAN"
        );
    }

    #[test]
    fn checked_read_at_len_trims_to_eof() {
        assert_eq!(
            checked_read_at_len("fs9_read_at", 1024, 900, 512).unwrap(),
            124
        );
    }

    #[test]
    fn checked_read_at_len_rejects_oversize_window() {
        let max = crate::extensions::fs::MAX_BYTES_PER_FILE;
        let err = checked_read_at_len("fs9_read_at_bytea", (max + 1) as u64, 0, max + 1)
            .expect_err("effective window above limit must fail");
        assert_eq!(
            err.to_string(),
            format!(
                "fs9_read_at_bytea: file too large: {} bytes exceeds limit {}",
                max + 1,
                max
            )
        );
    }

    #[test]
    fn checked_read_at_len_allows_large_request_trimmed_by_eof() {
        let max = crate::extensions::fs::MAX_BYTES_PER_FILE as u64;
        assert_eq!(
            checked_read_at_len(
                "fs9_read_at",
                max,
                max - 1,
                crate::extensions::fs::MAX_BYTES_PER_FILE + 123
            )
            .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn test_fs9_backend_unavailable_without_tikv_context() {
        let err = context::with_context(true, "", async {
            fs9_exists(vec![Value::Text("/tmp/anything".to_string())])
                .expect_err("missing tikv backend should fail")
        })
        .await;
        assert_eq!(err.to_string(), "fs9: TiKV storage backend not available");
    }

    #[test]
    fn test_fs9_permission_denied_without_context() {
        let err = fs9_exists(vec![Value::Text("/tmp/anything".to_string())])
            .expect_err("missing extension context should deny access");
        assert_eq!(err.to_string(), "fs9: TiKV storage backend not available");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fs9_scalar_functions_share_backend_namespace_with_ws_paths() {
        let backend = Arc::new(MockBackend::new());
        context::with_context(true, "tenant_a", async {
            let shared_backend: Arc<dyn FsBackend> = backend.clone();
            context::cache_fs_backend(shared_backend).expect("cache backend");

            let written = fs9_write(vec![
                Value::Text("/from_sql.txt".to_string()),
                Value::Text("written by sql".to_string()),
            ])
            .expect("sql write");
            assert_eq!(written, Value::Int64("written by sql".len() as i64));

            let root_entries = backend.readdir("/").await.expect("root readdir");
            assert!(
                root_entries
                    .iter()
                    .any(|entry| entry.path == "/from_sql.txt"),
                "backend readdir must surface SQL-written files: {root_entries:#?}"
            );
            let ws_read = backend
                .read_file("/from_sql.txt", crate::extensions::fs::MAX_BYTES_PER_FILE)
                .await
                .expect("backend read SQL-written file");
            assert_eq!(ws_read, b"written by sql".to_vec());

            backend
                .write_file("/from_ws.txt", b"written by ws", None)
                .await
                .expect("backend write");
            let sql_read = fs9_read(vec![Value::Text("/from_ws.txt".to_string())])
                .expect("sql read backend-written file");
            assert_eq!(sql_read, Value::Text("written by ws".to_string()));
            let exists = fs9_exists(vec![Value::Text("/from_ws.txt".to_string())])
                .expect("sql exists backend-written file");
            assert_eq!(exists, Value::Boolean(true));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fs9_write_at_preserves_sealed_file_contract() {
        let backend = Arc::new(MockBackend::new());
        backend.insert_file("/sealed.bin", b"abcdef".to_vec(), true);
        context::with_context(true, "tenant_a", async {
            let shared_backend: Arc<dyn FsBackend> = backend.clone();
            context::cache_fs_backend(shared_backend).expect("cache backend");

            let err = fs9_write_at(vec![
                Value::Text("/sealed.bin".to_string()),
                Value::Int64(0),
                Value::Text("Z".to_string()),
            ])
            .expect_err("sealed partial mutation must be rejected");
            assert!(
                err.to_string()
                    .contains("partial mutation is not supported for sealed files"),
                "unexpected error: {err}"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fs9_append_preserves_sealed_file_contract() {
        let backend = Arc::new(MockBackend::new());
        backend.insert_file("/sealed.bin", b"abcdef".to_vec(), true);
        context::with_context(true, "tenant_a", async {
            let shared_backend: Arc<dyn FsBackend> = backend.clone();
            context::cache_fs_backend(shared_backend).expect("cache backend");

            let err = fs9_append(vec![
                Value::Text("/sealed.bin".to_string()),
                Value::Text("Z".to_string()),
            ])
            .expect_err("sealed append must be rejected");
            assert!(
                err.to_string()
                    .contains("append is not supported for sealed files"),
                "unexpected error: {err}"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fs9_truncate_preserves_sealed_file_contract() {
        let backend = Arc::new(MockBackend::new());
        backend.insert_file("/sealed.bin", b"abcdef".to_vec(), true);
        context::with_context(true, "tenant_a", async {
            let shared_backend: Arc<dyn FsBackend> = backend.clone();
            context::cache_fs_backend(shared_backend).expect("cache backend");

            let err = fs9_truncate(vec![
                Value::Text("/sealed.bin".to_string()),
                Value::Int64(1),
            ])
            .expect_err("sealed truncate must be rejected");
            assert!(
                err.to_string()
                    .contains("truncate is not supported for sealed files"),
                "unexpected error: {err}"
            );
        })
        .await;
    }

    /// Design contract: `write_file` (full replace) is allowed on all storage
    /// classes including PackEntry and Object (sealed).  Only partial-mutation
    /// operations (`write_file_at`, `append_file`, `truncate`) are rejected on
    /// sealed files.  See §11.1 Mutation Rules in the fs9 v2 design doc.
    #[tokio::test(flavor = "multi_thread")]
    async fn fs9_write_full_replace_succeeds_on_sealed_file() {
        let backend = Arc::new(MockBackend::new());
        backend.insert_file("/sealed.bin", b"old content".to_vec(), true);
        context::with_context(true, "tenant_a", async {
            let shared_backend: Arc<dyn FsBackend> = backend.clone();
            context::cache_fs_backend(shared_backend).expect("cache backend");

            let written = fs9_write(vec![
                Value::Text("/sealed.bin".to_string()),
                Value::Text("new content".to_string()),
            ])
            .expect("full replace on sealed file must succeed per design contract");
            assert_eq!(written, Value::Int64("new content".len() as i64));

            let content = backend
                .read_file("/sealed.bin", 1024)
                .await
                .expect("read after replace");
            assert_eq!(content, b"new content");
        })
        .await;
    }
}
