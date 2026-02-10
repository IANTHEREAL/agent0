use std::collections::HashMap;
use std::process;

use serde_json::Value;

// ── HTTP helper ──────────────────────────────────────────────────

pub struct ApiClient {
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl ApiClient {
    pub fn new(base_url: &str, api_key: Option<&str>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.map(|s| s.to_string()),
            client: reqwest::Client::new(),
        }
    }

    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        extra_headers: Option<&HashMap<String, String>>,
    ) -> Value {
        let url = format!("{}{path}", self.base_url);
        let mut req = match method {
            "GET" => self.client.get(&url),
            "POST" => self.client.post(&url),
            "PUT" => self.client.put(&url),
            "DELETE" => self.client.delete(&url),
            _ => self.client.get(&url),
        };

        req = req.header("Content-Type", "application/json");

        if let Some(key) = &self.api_key {
            req = req.header("X-API-Key", key);
        }
        if let Some(hdrs) = extra_headers {
            for (k, v) in hdrs {
                req = req.header(k.as_str(), v.as_str());
            }
        }
        if let Some(b) = body {
            req = req.json(b);
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if status.is_success() {
                    serde_json::from_str(&text).unwrap_or(Value::Null)
                } else {
                    let err: Value = serde_json::from_str(&text).unwrap_or_default();
                    let detail = err
                        .get("message")
                        .or_else(|| err.get("detail"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown error");
                    eprintln!("Error {}: {detail}", status.as_u16());
                    process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("Connection failed: {e}");
                process::exit(1);
            }
        }
    }
}

// ── Table printer ────────────────────────────────────────────────

pub fn print_table(rows: &[Value], columns: &[(&str, &str, usize)]) {
    if rows.is_empty() {
        println!("(empty)");
        return;
    }

    let widths: Vec<usize> = columns
        .iter()
        .map(|(header, key, min_w)| {
            let max_val = rows
                .iter()
                .map(|r| format_val(r.get(key)).len())
                .max()
                .unwrap_or(0);
            header.len().max(max_val).max(*min_w)
        })
        .collect();

    // Header
    let header: String = columns
        .iter()
        .zip(&widths)
        .map(|((h, _, _), w)| format!("{:<width$}", h, width = w))
        .collect::<Vec<_>>()
        .join("  ");
    println!("{header}");

    // Separator
    let sep: String = widths
        .iter()
        .map(|w| "─".repeat(*w))
        .collect::<Vec<_>>()
        .join("  ");
    println!("{sep}");

    // Rows
    for row in rows {
        let line: String = columns
            .iter()
            .zip(&widths)
            .map(|((_, key, _), w)| {
                let v = format_val(row.get(key));
                format!("{:<width$}", v, width = w)
            })
            .collect::<Vec<_>>()
            .join("  ");
        println!("{line}");
    }
}

pub fn format_val(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "-".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Array(arr)) => {
            if arr.is_empty() {
                "-".to_string()
            } else {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        }
        Some(other) => other.to_string(),
    }
}

pub fn format_time(v: Option<&Value>) -> String {
    match v.and_then(|v| v.as_str()) {
        None => "-".to_string(),
        Some(s) => {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                dt.format("%Y-%m-%d %H:%M").to_string()
            } else {
                s.to_string()
            }
        }
    }
}

pub fn print_json(data: &Value) {
    println!("{}", serde_json::to_string_pretty(data).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_val_string() {
        let v = json!("hello");
        assert_eq!(format_val(Some(&v)), "hello");
    }

    #[test]
    fn format_val_null() {
        assert_eq!(format_val(None), "-");
        assert_eq!(format_val(Some(&Value::Null)), "-");
    }

    #[test]
    fn format_val_number() {
        let v = json!(42);
        assert_eq!(format_val(Some(&v)), "42");
    }

    #[test]
    fn format_val_bool() {
        let v = json!(true);
        assert_eq!(format_val(Some(&v)), "true");
    }

    #[test]
    fn format_val_empty_array() {
        let v = json!([]);
        assert_eq!(format_val(Some(&v)), "-");
    }

    #[test]
    fn format_val_string_array() {
        let v = json!(["a", "b", "c"]);
        assert_eq!(format_val(Some(&v)), "a, b, c");
    }

    #[test]
    fn format_val_object_falls_through() {
        let v = json!({"key": "val"});
        let result = format_val(Some(&v));
        assert!(result.contains("key"));
    }

    #[test]
    fn format_time_valid_rfc3339() {
        let v = json!("2026-02-10T19:30:00+00:00");
        assert_eq!(format_time(Some(&v)), "2026-02-10 19:30");
    }

    #[test]
    fn format_time_null() {
        assert_eq!(format_time(None), "-");
    }

    #[test]
    fn format_time_invalid() {
        let v = json!("not-a-date");
        assert_eq!(format_time(Some(&v)), "not-a-date");
    }

    #[test]
    fn format_time_json_null() {
        assert_eq!(format_time(Some(&Value::Null)), "-");
    }
}
