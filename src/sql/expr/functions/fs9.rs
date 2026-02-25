use crate::extensions::fs::{backend, backend::FsBackend, glob};
use crate::types::Value;
use anyhow::{anyhow, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
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
}

/// Check permissions. In remote mode, only superuser is required (remote backend
/// handles auth via JWT). In local mode, both allow_local_fs and superuser are needed.
fn ensure_permissions() -> Result<()> {
    if backend::is_remote_configured() {
        if !crate::extensions::context::is_superuser() {
            return Err(anyhow!("fs9: permission denied (superuser required)"));
        }
        return Ok(());
    }
    if !crate::extensions::context::allow_local_fs() || !crate::extensions::context::is_superuser()
    {
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

/// Bridge sync SqlFn to async backend calls. Safe because scalar functions
/// run on the tokio worker pool and `block_in_place` is the documented pattern.
fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

/// Get the fs9 backend using the tenant keyspace from extension context.
fn get_remote_backend() -> Result<Box<dyn backend::FsBackend>> {
    let tenant = crate::extensions::context::tenant_keyspace()
        .ok_or_else(|| anyhow!("fs9: tenant keyspace not available in extension context"))?;
    Ok(run_async(backend::get_backend(&tenant)))
}

// ---------------------------------------------------------------------------
// Remote-mode implementations
// ---------------------------------------------------------------------------

fn fs9_read_remote(path: &str) -> Result<Value> {
    let bk = get_remote_backend()?;
    let max = crate::extensions::fs::MAX_BYTES_PER_FILE;
    let bytes = run_async(bk.read_file(path, max))?;
    let _budget = reserve_read_budget(bytes.len())?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(Value::Text(text))
}

fn fs9_write_remote(path: &str, content: &[u8]) -> Result<Value> {
    let bk = get_remote_backend()?;
    let len = run_async(bk.write_file(path, content))?;
    Ok(Value::Int64(len as i64))
}

fn fs9_exists_remote(path: &str) -> Result<Value> {
    let bk = get_remote_backend()?;
    match run_async(bk.stat(path)) {
        Ok(_) => Ok(Value::Boolean(true)),
        Err(e) if { let msg = e.to_string(); msg.contains("not found") || msg.contains("NotFound") || msg.contains("404") } => Ok(Value::Boolean(false)),
        Err(e) => Err(e),
    }
}

fn fs9_size_remote(path: &str) -> Result<Value> {
    let bk = get_remote_backend()?;
    let info = run_async(bk.stat(path))?;
    Ok(Value::Int64(info.size as i64))
}

fn fs9_mtime_remote(path: &str) -> Result<Value> {
    let bk = get_remote_backend()?;
    let info = run_async(bk.stat(path))?;
    let dt =
        DateTime::<Utc>::from(std::time::UNIX_EPOCH + std::time::Duration::from_secs(info.mtime));
    Ok(Value::Text(dt.to_rfc3339_opts(SecondsFormat::Secs, true)))
}

fn fs9_remove_remote(path: &str, recursive: bool) -> Result<Value> {
    let _tenant = crate::extensions::context::tenant_keyspace()
        .ok_or_else(|| anyhow!("fs9: tenant keyspace not available in extension context"))?;
    let bk = get_remote_backend()?;

    // Check if glob pattern
    if glob::is_glob_pattern(path) {
        let files = run_async(glob::expand_glob(
            &*bk,
            path,
            crate::extensions::fs::MAX_FILES_PER_GLOB,
            None,
        ))?;
        let mut count = 0i64;
        for file in files {
            if recursive {
                count += run_async(bk.remove_recursive(&file))? as i64;
            } else {
                run_async(bk.remove(&file))?;
                count += 1;
            }
        }
        return Ok(Value::Int64(count));
    }

    // Single path
    if recursive {
        let count = run_async(bk.remove_recursive(path))?;
        Ok(Value::Int64(count as i64))
    } else {
        run_async(bk.remove(path))?;
        Ok(Value::Int64(1))
    }
}

fn fs9_mkdir_remote(path: &str, recursive: bool) -> Result<Value> {
    let bk = get_remote_backend()?;
    run_async(bk.mkdir(path, recursive))?;
    Ok(Value::Boolean(true))
}

// ---------------------------------------------------------------------------
// Local-mode implementations (original behaviour)
// ---------------------------------------------------------------------------

fn fs9_read_local(path: &str) -> Result<Value> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!("fs9_read: file not found: {path}"));
        }
        Err(err) => return Err(anyhow!("fs9_read: {err}")),
    };

    if metadata.is_dir() {
        return Err(anyhow!("fs9_read: is a directory: {path}"));
    }

    let file_len = metadata.len() as usize;
    if file_len > crate::extensions::fs::MAX_BYTES_PER_FILE {
        return Err(anyhow!(
            "fs9_read: file too large: {} bytes (max {})",
            file_len,
            crate::extensions::fs::MAX_BYTES_PER_FILE
        ));
    }

    let _budget = reserve_read_budget(file_len)?;
    let bytes = fs::read(path).map_err(|err| anyhow!("fs9_read: {err}"))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    drop(bytes);
    drop(_budget);

    Ok(Value::Text(text))
}

fn fs9_write_local(path: &str, content: &[u8]) -> Result<Value> {
    let file_path = Path::new(path);
    if let Some(parent) = file_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| anyhow!("fs9_write: {err}"))?
        }
    }

    let len = content.len();
    fs::write(file_path, content).map_err(|err| anyhow!("fs9_write: {err}"))?;
    Ok(Value::Int64(len as i64))
}

fn fs9_exists_local(path: &str) -> Result<Value> {
    Ok(Value::Boolean(Path::new(path).exists()))
}

fn fs9_size_local(path: &str) -> Result<Value> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!("fs9_size: file not found: {path}"));
        }
        Err(err) => return Err(anyhow!("fs9_size: {err}")),
    };
    Ok(Value::Int64(metadata.len() as i64))
}

fn fs9_mtime_local(path: &str) -> Result<Value> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!("fs9_mtime: file not found: {path}"));
        }
        Err(err) => return Err(anyhow!("fs9_mtime: {err}")),
    };

    let modified = metadata
        .modified()
        .map_err(|err| anyhow!("fs9_mtime: {err}"))?;
    let mtime = DateTime::<Utc>::from(modified).to_rfc3339_opts(SecondsFormat::Secs, true);
    Ok(Value::Text(mtime))
}

fn fs9_remove_local(path: &str, recursive: bool) -> Result<Value> {
    let local_backend = backend::local_backend();

    // Check if glob pattern
    if glob::is_glob_pattern(path) {
        let files = run_async(glob::expand_glob(
            local_backend,
            path,
            crate::extensions::fs::MAX_FILES_PER_GLOB,
            None,
        ))?;
        let mut count = 0i64;
        for file in files {
            if recursive {
                count += run_async(local_backend.remove_recursive(&file))? as i64;
            } else {
                run_async(local_backend.remove(&file))?;
                count += 1;
            }
        }
        return Ok(Value::Int64(count));
    }

    // Single path
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!("fs9_remove: file not found: {path}"));
        }
        Err(err) => return Err(anyhow!("fs9_remove: {err}")),
    };

    if metadata.is_dir() {
        if recursive {
            let count = run_async(local_backend.remove_recursive(path))?;
            Ok(Value::Int64(count as i64))
        } else {
            fs::remove_dir(path).map_err(|err| anyhow!("fs9_remove: {err}"))?;
            Ok(Value::Int64(1))
        }
    } else {
        fs::remove_file(path).map_err(|err| anyhow!("fs9_remove: {err}"))?;
        Ok(Value::Int64(1))
    }
}

fn fs9_mkdir_local(path: &str, recursive: bool) -> Result<Value> {
    if recursive {
        fs::create_dir_all(path).map_err(|err| anyhow!("fs9_mkdir: {err}"))?;
    } else {
        fs::create_dir(path).map_err(|err| anyhow!("fs9_mkdir: {err}"))?;
    }
    Ok(Value::Boolean(true))
}

// ---------------------------------------------------------------------------
// Public entry points — dispatch to remote or local
// ---------------------------------------------------------------------------

pub fn fs9_read(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let path = match expect_text_arg(
        "fs9_read",
        args.into_iter().next().unwrap_or(Value::Null),
        1,
    )? {
        Some(p) => p,
        None => return Ok(Value::Null),
    };
    if backend::is_remote_configured() {
        fs9_read_remote(&path)
    } else {
        fs9_read_local(&path)
    }
}

pub fn fs9_write(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let mut args_iter = args.into_iter();
    let path = expect_text_arg("fs9_write", args_iter.next().unwrap_or(Value::Null), 1)?;
    let content = expect_text_arg("fs9_write", args_iter.next().unwrap_or(Value::Null), 2)?;

    let (path, content) = match (path, content) {
        (Some(p), Some(c)) => (p, c),
        _ => return Ok(Value::Null),
    };

    let content_bytes = content.as_bytes();
    if content_bytes.len() > crate::extensions::fs::MAX_BYTES_PER_FILE {
        return Err(anyhow!(
            "fs9_write: content too large: {} bytes (max {})",
            content_bytes.len(),
            crate::extensions::fs::MAX_BYTES_PER_FILE
        ));
    }

    if backend::is_remote_configured() {
        fs9_write_remote(&path, content_bytes)
    } else {
        fs9_write_local(&path, content_bytes)
    }
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
    if backend::is_remote_configured() {
        fs9_exists_remote(&path)
    } else {
        fs9_exists_local(&path)
    }
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
    if backend::is_remote_configured() {
        fs9_size_remote(&path)
    } else {
        fs9_size_local(&path)
    }
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
    if backend::is_remote_configured() {
        fs9_mtime_remote(&path)
    } else {
        fs9_mtime_local(&path)
    }
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

    if backend::is_remote_configured() {
        fs9_remove_remote(&path, recursive)
    } else {
        fs9_remove_local(&path, recursive)
    }
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

    if backend::is_remote_configured() {
        fs9_mkdir_remote(&path, recursive)
    } else {
        fs9_mkdir_local(&path, recursive)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::extensions::context;

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    fn unique_base(name: &str) -> PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = PathBuf::from(format!(
            "/tmp/db9-fs9-scalar-test-{name}-{}-{id}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn cleanup(path: &PathBuf) {
        let _ = fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn test_fs9_read() {
        let dir = unique_base("read");
        let file = dir.join("hello.txt");
        fs::write(&file, b"hello world").expect("write test file");

        let value = context::with_context(true, "", async {
            fs9_read(vec![Value::Text(file.to_string_lossy().into_owned())]).expect("read file")
        })
        .await;
        assert_eq!(value, Value::Text("hello world".to_string()));

        let null_value = context::with_context(true, "", async {
            fs9_read(vec![Value::Null]).expect("null input should pass")
        })
        .await;
        assert_eq!(null_value, Value::Null);

        let bad_type_err = context::with_context(true, "", async {
            fs9_read(vec![Value::Int64(1)]).expect_err("non-text should fail")
        })
        .await;
        assert!(bad_type_err.to_string().contains("must be TEXT"));

        let missing_path = dir.join("missing.txt").to_string_lossy().into_owned();
        let missing_err = context::with_context(true, "", async {
            fs9_read(vec![Value::Text(missing_path)]).expect_err("missing file should fail")
        })
        .await;
        assert!(missing_err.to_string().contains("fs9_read: file not found"));

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_fs9_write() {
        let dir = unique_base("write");
        let file = dir.join("nested/path/file.txt");
        let file_path = file.to_string_lossy().into_owned();

        let bytes = context::with_context(true, "", async {
            fs9_write(vec![
                Value::Text(file_path.clone()),
                Value::Text("abc".to_string()),
            ])
            .expect("write file")
        })
        .await;
        assert_eq!(bytes, Value::Int64(3));
        assert_eq!(fs::read_to_string(&file).expect("read written file"), "abc");

        let null_value = context::with_context(true, "", async {
            fs9_write(vec![Value::Null, Value::Text("x".to_string())]).expect("null input")
        })
        .await;
        assert_eq!(null_value, Value::Null);

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_fs9_exists() {
        let dir = unique_base("exists");
        let file = dir.join("exists.txt");
        fs::write(&file, b"x").expect("write test file");

        let exists = context::with_context(true, "", async {
            fs9_exists(vec![Value::Text(file.to_string_lossy().into_owned())]).expect("exists")
        })
        .await;
        assert_eq!(exists, Value::Boolean(true));

        let missing = context::with_context(true, "", async {
            fs9_exists(vec![Value::Text(
                dir.join("missing.txt").to_string_lossy().into_owned(),
            )])
            .expect("exists on missing")
        })
        .await;
        assert_eq!(missing, Value::Boolean(false));

        let null_value = context::with_context(true, "", async {
            fs9_exists(vec![Value::Null]).expect("null input")
        })
        .await;
        assert_eq!(null_value, Value::Null);

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_fs9_size() {
        let dir = unique_base("size");
        let file = dir.join("size.txt");
        fs::write(&file, b"hello").expect("write test file");

        let size = context::with_context(true, "", async {
            fs9_size(vec![Value::Text(file.to_string_lossy().into_owned())]).expect("size")
        })
        .await;
        assert_eq!(size, Value::Int64(5));

        let missing_err = context::with_context(true, "", async {
            fs9_size(vec![Value::Text(
                dir.join("missing.txt").to_string_lossy().into_owned(),
            )])
            .expect_err("missing should fail")
        })
        .await;
        assert!(missing_err.to_string().contains("fs9_size: file not found"));

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_fs9_mtime() {
        let dir = unique_base("mtime");
        let file = dir.join("mtime.txt");
        fs::write(&file, b"x").expect("write test file");

        let mtime = context::with_context(true, "", async {
            fs9_mtime(vec![Value::Text(file.to_string_lossy().into_owned())]).expect("mtime")
        })
        .await;
        let mtime_text = match mtime {
            Value::Text(s) => s,
            other => panic!("expected text mtime, got {other:?}"),
        };
        assert!(chrono::DateTime::parse_from_rfc3339(&mtime_text).is_ok());

        let missing_err = context::with_context(true, "", async {
            fs9_mtime(vec![Value::Text(
                dir.join("missing.txt").to_string_lossy().into_owned(),
            )])
            .expect_err("missing should fail")
        })
        .await;
        assert!(missing_err
            .to_string()
            .contains("fs9_mtime: file not found"));

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_fs9_remove() {
        let dir = unique_base("remove");
        let file = dir.join("to_delete.txt");
        fs::write(&file, b"delete me").expect("write test file");

        // Verify file exists
        assert!(file.exists());

        // Remove the file
        let result = context::with_context(true, "", async {
            fs9_remove(vec![Value::Text(file.to_string_lossy().into_owned())]).expect("remove")
        })
        .await;
        assert_eq!(result, Value::Int64(1));

        // Verify file no longer exists
        assert!(!file.exists());

        // Removing missing file should error
        let missing_err = context::with_context(true, "", async {
            fs9_remove(vec![Value::Text(
                dir.join("missing.txt").to_string_lossy().into_owned(),
            )])
            .expect_err("missing should fail")
        })
        .await;
        assert!(missing_err
            .to_string()
            .contains("fs9_remove: file not found"));

        // Test removing empty directory
        let subdir = dir.join("empty_dir");
        fs::create_dir(&subdir).expect("create empty dir");
        let dir_result = context::with_context(true, "", async {
            fs9_remove(vec![Value::Text(subdir.to_string_lossy().into_owned())])
                .expect("remove dir")
        })
        .await;
        assert_eq!(dir_result, Value::Int64(1));
        assert!(!subdir.exists());

        // Null input should return null
        let null_value = context::with_context(true, "", async {
            fs9_remove(vec![Value::Null]).expect("null input")
        })
        .await;
        assert_eq!(null_value, Value::Null);

        cleanup(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_fs9_remove_recursive() {
        let dir = unique_base("remove-recursive");

        // Create nested directory structure
        let subdir = dir.join("subdir");
        fs::create_dir_all(&subdir).expect("create subdir");
        fs::write(dir.join("file1.txt"), b"1").expect("write file1");
        fs::write(subdir.join("file2.txt"), b"2").expect("write file2");
        fs::write(subdir.join("file3.txt"), b"3").expect("write file3");

        // Non-recursive should fail on non-empty directory
        let err = context::with_context(true, "", async {
            fs9_remove(vec![Value::Text(dir.to_string_lossy().into_owned())])
                .expect_err("non-recursive on non-empty dir should fail")
        })
        .await;
        assert!(err.to_string().contains("fs9_remove"));

        // Recursive should succeed
        let result = context::with_context(true, "", async {
            fs9_remove(vec![
                Value::Text(dir.to_string_lossy().into_owned()),
                Value::Boolean(true),
            ])
            .expect("recursive remove")
        })
        .await;
        // Should return count of files removed (3 files)
        assert!(matches!(result, Value::Int64(n) if n >= 3));
        assert!(!dir.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_fs9_remove_glob() {
        let dir = unique_base("remove-glob");

        // Create files
        fs::write(dir.join("file1.txt"), b"1").expect("write file1");
        fs::write(dir.join("file2.txt"), b"2").expect("write file2");
        fs::write(dir.join("keep.csv"), b"keep").expect("write keep");

        // Remove all .txt files using glob
        let pattern = format!("{}/*.txt", dir.display());
        let result = context::with_context(true, "", async {
            fs9_remove(vec![Value::Text(pattern)]).expect("glob remove")
        })
        .await;
        assert_eq!(result, Value::Int64(2));

        // .txt files should be gone, .csv should remain
        assert!(!dir.join("file1.txt").exists());
        assert!(!dir.join("file2.txt").exists());
        assert!(dir.join("keep.csv").exists());

        cleanup(&dir);
    }

    #[tokio::test]
    async fn test_fs9_mkdir() {
        let dir = unique_base("mkdir");

        // Create single directory
        let subdir = dir.join("newdir");
        let result = context::with_context(true, "", async {
            fs9_mkdir(vec![Value::Text(subdir.to_string_lossy().into_owned())]).expect("mkdir")
        })
        .await;
        assert_eq!(result, Value::Boolean(true));
        assert!(subdir.exists() && subdir.is_dir());

        // Non-recursive should fail for nested path
        let nested = dir.join("a/b/c");
        let err = context::with_context(true, "", async {
            fs9_mkdir(vec![Value::Text(nested.to_string_lossy().into_owned())])
                .expect_err("non-recursive nested should fail")
        })
        .await;
        assert!(err.to_string().contains("fs9_mkdir"));

        // Recursive should succeed for nested path
        let result = context::with_context(true, "", async {
            fs9_mkdir(vec![
                Value::Text(nested.to_string_lossy().into_owned()),
                Value::Boolean(true),
            ])
            .expect("recursive mkdir")
        })
        .await;
        assert_eq!(result, Value::Boolean(true));
        assert!(nested.exists() && nested.is_dir());

        // Null input should return null
        let null_value = context::with_context(true, "", async {
            fs9_mkdir(vec![Value::Null]).expect("null input")
        })
        .await;
        assert_eq!(null_value, Value::Null);

        cleanup(&dir);
    }

    #[test]
    fn test_fs9_permission_denied_without_context() {
        let err = fs9_exists(vec![Value::Text("/tmp/anything".to_string())])
            .expect_err("missing extension context should deny access");
        assert_eq!(
            err.to_string(),
            "fs9: permission denied (superuser required)"
        );
    }
}
