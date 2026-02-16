use pgtikv_admin::cli_common::print_json;
use serde_json::Value;

use crate::OutputFormat;

pub fn print_sql_result(data: &Value, output: &OutputFormat) {
    if matches!(output, OutputFormat::Json) {
        print_json(data);
        return;
    }

    let columns = match data["columns"].as_array() {
        Some(cols) if !cols.is_empty() => cols,
        _ => {
            println!("{}", data["command"].as_str().unwrap_or("OK"));
            return;
        }
    };
    let rows = data["rows"].as_array().map(|r| r.as_slice()).unwrap_or(&[]);

    match output {
        OutputFormat::Csv => {
            let header: Vec<&str> = columns.iter().filter_map(|c| c["name"].as_str()).collect();
            println!("{}", header.join(","));
            for row in rows {
                if let Some(vals) = row.as_array() {
                    let line: Vec<String> = vals
                        .iter()
                        .map(|v| match v {
                            Value::Null => "".to_string(),
                            Value::String(s) => {
                                if s.contains(',') || s.contains('"') || s.contains('\n') {
                                    format!("\"{}\"", s.replace('"', "\"\""))
                                } else {
                                    s.clone()
                                }
                            }
                            other => other.to_string(),
                        })
                        .collect();
                    println!("{}", line.join(","));
                }
            }
        }
        _ => {
            let col_names: Vec<String> = columns
                .iter()
                .map(|c| c["name"].as_str().unwrap_or("?").to_string())
                .collect();

            let widths: Vec<usize> = col_names
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let max_val = rows
                        .iter()
                        .map(|row| {
                            row.as_array()
                                .and_then(|arr| arr.get(i))
                                .map(|v| match v {
                                    Value::Null => 4,
                                    Value::String(s) => s.len(),
                                    other => other.to_string().len(),
                                })
                                .unwrap_or(0)
                        })
                        .max()
                        .unwrap_or(0);
                    name.len().max(max_val).max(4)
                })
                .collect();

            let header: String = col_names
                .iter()
                .zip(&widths)
                .map(|(name, w)| format!("{:<width$}", name, width = w))
                .collect::<Vec<_>>()
                .join("  ");
            println!("{header}");

            let sep: String = widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("  ");
            println!("{sep}");

            for row in rows {
                if let Some(vals) = row.as_array() {
                    let line: String = vals
                        .iter()
                        .enumerate()
                        .map(|(i, v)| {
                            let w = widths.get(i).copied().unwrap_or(4);
                            let s = match v {
                                Value::Null => "NULL".to_string(),
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            format!("{:<width$}", s, width = w)
                        })
                        .collect::<Vec<_>>()
                        .join("  ");
                    println!("{line}");
                }
            }
            let n = rows.len();
            println!("({} {})", n, if n == 1 { "row" } else { "rows" });
        }
    }
}
