use serde_json::Value;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

pub struct DirectExecutor {
    client: Client,
}

impl DirectExecutor {
    pub async fn connect(dsn: &str) -> Result<Self, String> {
        let (client, connection) = tokio_postgres::connect(dsn, NoTls)
            .await
            .map_err(|e| {
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
}

// ── helpers ─────────────────────────────────────────────────────

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
        assert_eq!(infer_command("INSERT INTO t VALUES (1); SELECT * FROM t;"), "SELECT");
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
}
