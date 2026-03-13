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
    let len = run_async(client.write_file(&path, &content))?;
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
    run_async(client.mkdir(&path, recursive))?;
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
}
