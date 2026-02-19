use bytes::Bytes;
use futures_util::{SinkExt, TryStreamExt};
use serde_json::Value;
use std::io::{BufRead, Write};
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

pub struct DirectExecutor {
    client: Client,
}

impl DirectExecutor {
    pub async fn connect(dsn: &str) -> Result<Self, String> {
        let (client, connection) = tokio_postgres::connect(dsn, NoTls).await.map_err(|e| {
            let msg = e.to_string();
            if msg.contains("authentication") {
                format!("Authentication failed: {msg}")
            } else if msg.contains("Connection refused") || msg.contains("connect") {
                format!("Connection refused: {msg}\nHint: Is pg-tikv running?")
            } else {
                format!("Connection failed: {msg}")
            }
        })?;

        // tokio-postgres requires the connection future to be driven
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("pgwire connection error: {e}");
            }
        });

        Ok(Self { client })
    }

    pub async fn execute(&self, sql: &str) -> Result<Value, String> {
        let messages = self
            .client
            .simple_query(sql)
            .await
            .map_err(|e| e.to_string())?;

        let mut last_columns: Vec<Value> = Vec::new();
        let mut last_rows: Vec<Value> = Vec::new();
        let mut last_row_count: u64 = 0;

        let mut cur_columns: Vec<Value> = Vec::new();
        let mut cur_rows: Vec<Value> = Vec::new();

        for msg in &messages {
            match msg {
                SimpleQueryMessage::Row(row) => {
                    if cur_columns.is_empty() {
                        cur_columns = row
                            .columns()
                            .iter()
                            .map(|c| serde_json::json!({ "name": c.name() }))
                            .collect();
                    }
                    let vals: Vec<Value> = (0..row.columns().len())
                        .map(|i| match row.get(i) {
                            Some(s) => infer_typed_value(s),
                            None => Value::Null,
                        })
                        .collect();
                    cur_rows.push(Value::Array(vals));
                }
                SimpleQueryMessage::CommandComplete(n) => {
                    last_columns = std::mem::take(&mut cur_columns);
                    last_rows = std::mem::take(&mut cur_rows);
                    last_row_count = *n;
                }
                _ => {}
            }
        }

        let command = infer_command(sql);
        let row_count = if !last_columns.is_empty() {
            last_rows.len() as u64
        } else {
            last_row_count
        };

        Ok(serde_json::json!({
            "columns": last_columns,
            "rows": last_rows,
            "command": command,
            "row_count": row_count,
        }))
    }
    pub async fn copy_in(
        &self,
        table: &str,
        file_path: &str,
        csv: bool,
        header: bool,
    ) -> Result<u64, String> {
        if !std::path::Path::new(file_path).exists() {
            return Err(format!("\\copy: file not found: {}", file_path));
        }

        let stmt = build_copy_stmt(table, "FROM STDIN", csv, header);

        let sink = self
            .client
            .copy_in::<_, Bytes>(stmt.as_str())
            .await
            .map_err(|e| e.to_string())?;
        let mut sink = std::pin::pin!(sink);

        let file = std::fs::File::open(file_path).map_err(|e| format!("\\copy: {}", e))?;
        let reader = std::io::BufReader::new(file);
        const CHUNK_SIZE: usize = 65536;
        let mut buf = Vec::with_capacity(CHUNK_SIZE);

        for line in reader.lines() {
            let line = line.map_err(|e| format!("\\copy: read error: {}", e))?;
            buf.extend_from_slice(line.as_bytes());
            buf.push(b'\n');
            if buf.len() >= CHUNK_SIZE {
                sink.send(Bytes::from(std::mem::take(&mut buf)))
                    .await
                    .map_err(|e| e.to_string())?;
                buf = Vec::with_capacity(CHUNK_SIZE);
            }
        }
        if !buf.is_empty() {
            sink.send(Bytes::from(buf))
                .await
                .map_err(|e| e.to_string())?;
        }

        let rows = sink.finish().await.map_err(|e| e.to_string())?;
        Ok(rows)
    }

    pub async fn copy_out(
        &self,
        table: &str,
        file_path: &str,
        csv: bool,
        header: bool,
    ) -> Result<u64, String> {
        if let Some(parent) = std::path::Path::new(file_path).parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                return Err(format!("\\copy: directory not found: {}", parent.display()));
            }
        }

        let stmt = build_copy_stmt(table, "TO STDOUT", csv, header);

        let stream = self
            .client
            .copy_out(stmt.as_str())
            .await
            .map_err(|e| e.to_string())?;
        let mut stream = std::pin::pin!(stream);

        let mut file = std::fs::File::create(file_path)
            .map_err(|e| format!("\\copy: cannot create '{}': {}", file_path, e))?;

        let mut row_count: u64 = 0;
        while let Some(chunk) = stream.try_next().await.map_err(|e| e.to_string())? {
            file.write_all(&chunk)
                .map_err(|e| format!("\\copy: write error: {}", e))?;
            row_count += chunk.iter().filter(|&&b| b == b'\n').count() as u64;
        }

        if header && row_count > 0 {
            row_count -= 1;
        }

        Ok(row_count)
    }
}

// ── helpers ─────────────────────────────────────────────────────

fn build_copy_stmt(table: &str, direction: &str, csv: bool, header: bool) -> String {
    let mut stmt = format!("COPY {} {}", table, direction);
    let mut opts = Vec::new();
    if csv {
        opts.push("FORMAT CSV");
    }
    if header {
        opts.push("HEADER");
    }
    if !opts.is_empty() {
        stmt.push_str(&format!(" WITH ({})", opts.join(", ")));
    }
    stmt
}

// simple_query returns all values as text; infer JSON types heuristically:
//   "t"/"f" (pg boolean text repr) → bool
//   integer-parseable              → number
//   float-parseable                → number
//   everything else                → string
fn infer_typed_value(s: &str) -> Value {
    if s == "t" {
        return Value::Bool(true);
    }
    if s == "f" {
        return Value::Bool(false);
    }
    if let Ok(n) = s.parse::<i64>() {
        return Value::Number(n.into());
    }
    if let Ok(n) = s.parse::<f64>() {
        if let Some(num) = serde_json::Number::from_f64(n) {
            return Value::Number(num);
        }
    }
    Value::String(s.to_string())
}

fn infer_command(sql: &str) -> String {
    let last_stmt = sql
        .rsplit(';')
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or(sql.trim());

    last_stmt
        .split_whitespace()
        .next()
        .unwrap_or("OK")
        .to_uppercase()
}

// ── tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_command_select() {
        assert_eq!(infer_command("SELECT 1"), "SELECT");
    }

    #[test]
    fn test_infer_command_begin() {
        assert_eq!(infer_command("BEGIN;"), "BEGIN");
    }

    #[test]
    fn test_infer_command_multi() {
        assert_eq!(
            infer_command("INSERT INTO t VALUES (1); SELECT * FROM t;"),
            "SELECT"
        );
    }

    #[test]
    fn test_infer_command_empty() {
        assert_eq!(infer_command(""), "OK");
    }

    #[test]
    fn test_infer_int() {
        assert_eq!(infer_typed_value("42"), Value::Number(42.into()));
    }

    #[test]
    fn test_infer_negative_int() {
        assert_eq!(infer_typed_value("-7"), Value::Number((-7).into()));
    }

    #[test]
    fn test_infer_bool_true() {
        assert_eq!(infer_typed_value("t"), Value::Bool(true));
    }

    #[test]
    fn test_infer_bool_false() {
        assert_eq!(infer_typed_value("f"), Value::Bool(false));
    }

    #[test]
    fn test_infer_text() {
        assert_eq!(
            infer_typed_value("hello"),
            Value::String("hello".to_string())
        );
    }

    #[test]
    fn test_infer_float() {
        assert!(infer_typed_value("3.14").is_number());
    }

    #[test]
    fn test_infer_command_update() {
        assert_eq!(infer_command("UPDATE users SET name = 'x'"), "UPDATE");
    }

    #[test]
    fn test_infer_command_delete() {
        assert_eq!(infer_command("DELETE FROM users WHERE id = 1"), "DELETE");
    }

    #[test]
    fn test_infer_command_create_table() {
        assert_eq!(infer_command("CREATE TABLE foo (id INT)"), "CREATE");
    }

    #[test]
    fn test_infer_command_whitespace() {
        assert_eq!(infer_command("  SELECT 1  "), "SELECT");
    }

    #[test]
    fn test_infer_command_trailing_semicolons() {
        assert_eq!(infer_command("SELECT 1;;;"), "SELECT");
    }

    #[test]
    fn test_infer_command_case_insensitive() {
        assert_eq!(infer_command("select 1"), "SELECT");
        assert_eq!(infer_command("Insert INTO t VALUES (1)"), "INSERT");
    }

    #[test]
    fn test_infer_typed_value_large_int() {
        assert_eq!(
            infer_typed_value("9999999999999"),
            Value::Number(9999999999999i64.into())
        );
    }

    #[test]
    fn test_infer_typed_value_zero() {
        assert_eq!(infer_typed_value("0"), Value::Number(0.into()));
    }

    #[test]
    fn test_infer_typed_value_negative_float() {
        let val = infer_typed_value("-3.14");
        assert!(val.is_number());
    }

    #[test]
    fn test_infer_typed_value_uuid_string() {
        let val = infer_typed_value("550e8400-e29b-41d4-a716-446655440000");
        assert!(val.is_string());
    }

    #[test]
    fn test_infer_typed_value_timestamp_string() {
        let val = infer_typed_value("2026-02-15 12:00:00");
        assert!(val.is_string());
    }

    #[test]
    fn test_infer_typed_value_empty_string() {
        assert_eq!(infer_typed_value(""), Value::String("".to_string()));
    }

    #[test]
    fn test_infer_typed_value_json_like_string() {
        let val = infer_typed_value("{\"key\": \"value\"}");
        assert!(val.is_string());
    }
}
