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
}

fn ensure_permissions() -> Result<()> {
    if !crate::extensions::context::allow_local_fs() || !crate::extensions::context::is_superuser() {
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

pub fn fs9_read(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;

    let path = match expect_text_arg("fs9_read", args.into_iter().next().unwrap_or(Value::Null), 1)? {
        Some(path) => path,
        None => return Ok(Value::Null),
    };

    let metadata = match fs::metadata(&path) {
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
    let bytes = fs::read(&path).map_err(|err| anyhow!("fs9_read: {err}"))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    drop(bytes);
    drop(_budget);

    Ok(Value::Text(text))
}

pub fn fs9_write(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;

    let mut args_iter = args.into_iter();
    let path = expect_text_arg("fs9_write", args_iter.next().unwrap_or(Value::Null), 1)?;
    let content = expect_text_arg("fs9_write", args_iter.next().unwrap_or(Value::Null), 2)?;

    let (path, content) = match (path, content) {
        (Some(path), Some(content)) => (path, content),
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

    let file_path = Path::new(&path);
    if let Some(parent) = file_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|err| anyhow!("fs9_write: {err}"))?;
        }
    }

    fs::write(file_path, content_bytes).map_err(|err| anyhow!("fs9_write: {err}"))?;
    Ok(Value::Int64(content_bytes.len() as i64))
}

pub fn fs9_exists(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;

    let path = match expect_text_arg("fs9_exists", args.into_iter().next().unwrap_or(Value::Null), 1)? {
        Some(path) => path,
        None => return Ok(Value::Null),
    };

    Ok(Value::Boolean(Path::new(&path).exists()))
}

pub fn fs9_size(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;

    let path = match expect_text_arg("fs9_size", args.into_iter().next().unwrap_or(Value::Null), 1)? {
        Some(path) => path,
        None => return Ok(Value::Null),
    };

    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!("fs9_size: file not found: {path}"));
        }
        Err(err) => return Err(anyhow!("fs9_size: {err}")),
    };

    Ok(Value::Int64(metadata.len() as i64))
}

pub fn fs9_mtime(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;

    let path = match expect_text_arg("fs9_mtime", args.into_iter().next().unwrap_or(Value::Null), 1)? {
        Some(path) => path,
        None => return Ok(Value::Null),
    };

    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!("fs9_mtime: file not found: {path}"));
        }
        Err(err) => return Err(anyhow!("fs9_mtime: {err}")),
    };

    let modified = metadata.modified().map_err(|err| anyhow!("fs9_mtime: {err}"))?;
    let mtime = DateTime::<Utc>::from(modified).to_rfc3339_opts(SecondsFormat::Secs, true);
    Ok(Value::Text(mtime))
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
            "/tmp/pgtikv-fs9-scalar-test-{name}-{}-{id}-{nanos}",
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

        let value = context::with_context(true, async {
            fs9_read(vec![Value::Text(file.to_string_lossy().into_owned())]).expect("read file")
        })
        .await;
        assert_eq!(value, Value::Text("hello world".to_string()));

        let null_value = context::with_context(true, async {
            fs9_read(vec![Value::Null]).expect("null input should pass")
        })
        .await;
        assert_eq!(null_value, Value::Null);

        let bad_type_err = context::with_context(true, async {
            fs9_read(vec![Value::Int64(1)]).expect_err("non-text should fail")
        })
        .await;
        assert!(bad_type_err.to_string().contains("must be TEXT"));

        let missing_path = dir.join("missing.txt").to_string_lossy().into_owned();
        let missing_err = context::with_context(true, async {
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

        let bytes = context::with_context(true, async {
            fs9_write(vec![
                Value::Text(file_path.clone()),
                Value::Text("abc".to_string()),
            ])
            .expect("write file")
        })
        .await;
        assert_eq!(bytes, Value::Int64(3));
        assert_eq!(fs::read_to_string(&file).expect("read written file"), "abc");

        let null_value = context::with_context(true, async {
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

        let exists = context::with_context(true, async {
            fs9_exists(vec![Value::Text(file.to_string_lossy().into_owned())]).expect("exists")
        })
        .await;
        assert_eq!(exists, Value::Boolean(true));

        let missing = context::with_context(true, async {
            fs9_exists(vec![Value::Text(
                dir.join("missing.txt").to_string_lossy().into_owned(),
            )])
            .expect("exists on missing")
        })
        .await;
        assert_eq!(missing, Value::Boolean(false));

        let null_value = context::with_context(true, async {
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

        let size = context::with_context(true, async {
            fs9_size(vec![Value::Text(file.to_string_lossy().into_owned())]).expect("size")
        })
        .await;
        assert_eq!(size, Value::Int64(5));

        let missing_err = context::with_context(true, async {
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

        let mtime = context::with_context(true, async {
            fs9_mtime(vec![Value::Text(file.to_string_lossy().into_owned())]).expect("mtime")
        })
        .await;
        let mtime_text = match mtime {
            Value::Text(s) => s,
            other => panic!("expected text mtime, got {other:?}"),
        };
        assert!(chrono::DateTime::parse_from_rfc3339(&mtime_text).is_ok());

        let missing_err = context::with_context(true, async {
            fs9_mtime(vec![Value::Text(
                dir.join("missing.txt").to_string_lossy().into_owned(),
            )])
            .expect_err("missing should fail")
        })
        .await;
        assert!(missing_err.to_string().contains("fs9_mtime: file not found"));

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
