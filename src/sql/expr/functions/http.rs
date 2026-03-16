use crate::extensions::context;
use crate::extensions::http::{self, HttpTableFunctionCall};
use crate::model::Value;
use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("HTTP_GET", http_get);
    map.insert("HTTP_HEAD", http_head);
    map.insert("HTTP_DELETE", http_delete);
    map.insert("HTTP_POST", http_post);
    map.insert("HTTP_PUT", http_put);
    map.insert("HTTP_PATCH", http_patch);
    map.insert("HTTP", http_universal);
}

/// Bridge sync SqlFn to async HTTP calls (same pattern as fs9).
fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

fn ensure_permissions() -> Result<()> {
    if !context::is_superuser() {
        return Err(SqlError::PermissionDenied {
            object_type: "extension".into(),
            object_name: "\"http\"".into(),
        }
        .into());
    }
    Ok(())
}

fn expect_text(name: &str, val: &Value, position: usize) -> Result<String> {
    match val {
        Value::Text(s) => Ok(s.clone()),
        Value::Null => Err(anyhow!("{name}: argument {position} must not be NULL")),
        other => Err(anyhow!(
            "{}: argument {} must be TEXT, got {}",
            name,
            position,
            other.type_display_name()
        )),
    }
}

/// Extract optional headers argument. Accepts TEXT, JSONB, or NULL.
/// Returns an error on type mismatch (instead of silently ignoring).
fn optional_headers(name: &str, val: &Value, position: usize) -> Result<Option<String>> {
    match val {
        Value::Text(s) => Ok(Some(s.clone())),
        Value::Jsonb(s) => Ok(Some(s.clone())),
        Value::Null => Ok(None),
        other => Err(anyhow!(
            "{}: argument {} must be TEXT or JSONB, got {}",
            name,
            position,
            other.type_display_name()
        )),
    }
}

/// Execute an HTTP call and return the response as JSONB:
/// `{"status": 200, "content_type": "...", "headers": [...], "content": "..."}`
fn execute_and_return_jsonb(call: HttpTableFunctionCall) -> Result<Value> {
    let tenant = context::tenant_keyspace()
        .ok_or_else(|| anyhow!("http: extension context not available"))?;
    // Verify CREATE EXTENSION http has been run (table-function path checks
    // this in the executor layer; scalar path must check here).
    run_async(http::check_extension_installed())?;
    let (_schema, rows) = run_async(http::execute_table_function(&tenant, call))?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("http: no response row"))?;
    let vals = row.values;

    // Column order matches http_response_schema(): status(0), content_type(1), headers(2), content(3)
    let status = match vals.first() {
        Some(Value::Int32(n)) => serde_json::Value::Number((*n).into()),
        _ => serde_json::Value::Null,
    };
    let content_type = match vals.get(1) {
        Some(Value::Text(s)) => serde_json::Value::String(s.clone()),
        _ => serde_json::Value::Null,
    };
    let headers = match vals.get(2) {
        Some(Value::Jsonb(s)) => serde_json::from_str(s)
            .map_err(|e| anyhow!("http: invalid headers JSON in response: {e}"))?,
        _ => serde_json::Value::Null,
    };
    let content = match vals.get(3) {
        Some(Value::Text(s)) => serde_json::Value::String(s.clone()),
        _ => serde_json::Value::Null,
    };

    let obj = serde_json::json!({
        "status": status,
        "content_type": content_type,
        "headers": headers,
        "content": content,
    });
    Ok(Value::Jsonb(obj.to_string()))
}

// ── Scalar function implementations ──────────────────────────────

pub fn http_get(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let url = expect_text("http_get", args.first().unwrap_or(&Value::Null), 1)?;
    let headers = args
        .get(1)
        .map(|v| optional_headers("http_get", v, 2))
        .transpose()?
        .flatten();
    execute_and_return_jsonb(HttpTableFunctionCall::Get { url, headers })
}

pub fn http_head(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let url = expect_text("http_head", args.first().unwrap_or(&Value::Null), 1)?;
    let headers = args
        .get(1)
        .map(|v| optional_headers("http_head", v, 2))
        .transpose()?
        .flatten();
    execute_and_return_jsonb(HttpTableFunctionCall::Head { url, headers })
}

pub fn http_delete(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let url = expect_text("http_delete", args.first().unwrap_or(&Value::Null), 1)?;
    let headers = args
        .get(1)
        .map(|v| optional_headers("http_delete", v, 2))
        .transpose()?
        .flatten();
    execute_and_return_jsonb(HttpTableFunctionCall::Delete { url, headers })
}

pub fn http_post(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let url = expect_text("http_post", args.first().unwrap_or(&Value::Null), 1)?;
    let body = expect_text("http_post", args.get(1).unwrap_or(&Value::Null), 2)?;
    let content_type = expect_text("http_post", args.get(2).unwrap_or(&Value::Null), 3)?;
    let headers = args
        .get(3)
        .map(|v| optional_headers("http_post", v, 4))
        .transpose()?
        .flatten();
    execute_and_return_jsonb(HttpTableFunctionCall::Post {
        url,
        body,
        content_type,
        headers,
    })
}

pub fn http_put(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let url = expect_text("http_put", args.first().unwrap_or(&Value::Null), 1)?;
    let body = expect_text("http_put", args.get(1).unwrap_or(&Value::Null), 2)?;
    let content_type = expect_text("http_put", args.get(2).unwrap_or(&Value::Null), 3)?;
    let headers = args
        .get(3)
        .map(|v| optional_headers("http_put", v, 4))
        .transpose()?
        .flatten();
    execute_and_return_jsonb(HttpTableFunctionCall::Put {
        url,
        body,
        content_type,
        headers,
    })
}

pub fn http_patch(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let url = expect_text("http_patch", args.first().unwrap_or(&Value::Null), 1)?;
    let body = expect_text("http_patch", args.get(1).unwrap_or(&Value::Null), 2)?;
    let content_type = expect_text("http_patch", args.get(2).unwrap_or(&Value::Null), 3)?;
    let headers = args
        .get(3)
        .map(|v| optional_headers("http_patch", v, 4))
        .transpose()?
        .flatten();
    execute_and_return_jsonb(HttpTableFunctionCall::Universal {
        method: "PATCH".to_string(),
        url,
        headers,
        content_type: Some(content_type),
        body: Some(body),
    })
}

pub fn http_universal(args: Vec<Value>) -> Result<Value> {
    ensure_permissions()?;
    let method = expect_text("http", args.first().unwrap_or(&Value::Null), 1)?;
    let url = expect_text("http", args.get(1).unwrap_or(&Value::Null), 2)?;
    let headers = args
        .get(2)
        .map(|v| optional_headers("http", v, 3))
        .transpose()?
        .flatten();
    let content_type = match args.get(3) {
        Some(Value::Text(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(other) => {
            return Err(anyhow!(
                "http: content_type must be TEXT, got {}",
                other.type_display_name()
            ))
        }
    };
    let body = match args.get(4) {
        Some(Value::Text(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(other) => {
            return Err(anyhow!(
                "http: content must be TEXT, got {}",
                other.type_display_name()
            ))
        }
    };
    execute_and_return_jsonb(HttpTableFunctionCall::Universal {
        method,
        url,
        headers,
        content_type,
        body,
    })
}
