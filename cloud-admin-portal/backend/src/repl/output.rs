use pgtikv_admin::cli_common::print_json;
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

use crate::OutputFormat;

/// Pipe content to a pager process
fn pipe_to_pager(content: &str, pager_cmd: &str) -> bool {
    let parts: Vec<&str> = pager_cmd.split_whitespace().collect();
    if parts.is_empty() {
        return false;
    }

    match Command::new(parts[0])
        .args(&parts[1..])
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(content.as_bytes());
            }
            let _ = child.wait();
            true
        }
        Err(_) => false,
    }
}

/// Get terminal height using crossterm
fn get_terminal_height() -> usize {
    crossterm::terminal::size()
        .map(|(_, h)| h as usize)
        .unwrap_or(24)
}

/// Get pager command from environment or use defaults
fn get_pager_command() -> String {
    if let Ok(pager) = std::env::var("PAGER") {
        if !pager.is_empty() {
            return pager;
        }
    }
    // Default pager chain: less -RFX → more → direct print
    "less -RFX".to_string()
}

/// Print content with optional paging
pub fn print_with_pager(content: &str, pager_enabled: bool, pager_command: &Option<String>) {
    if !pager_enabled {
        print!("{}", content);
        return;
    }

    let terminal_height = get_terminal_height();
    let content_lines = content.lines().count();

    // Only page if content exceeds terminal height minus 4 (for prompt/status)
    if content_lines <= terminal_height.saturating_sub(4) {
        print!("{}", content);
        return;
    }

    let pager_cmd = pager_command
        .as_ref()
        .cloned()
        .unwrap_or_else(get_pager_command);

    // Try to pipe to pager; fall back to direct print if it fails
    if !pipe_to_pager(content, &pager_cmd) {
        print!("{}", content);
    }
}

pub fn print_sql_result(
    data: &Value,
    output: &OutputFormat,
    pager_enabled: bool,
    pager_command: &Option<String>,
) {
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
            // CSV format: do not page
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
            // Table format: collect output and page if needed
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

            let mut output_buf = String::new();

            let header: String = col_names
                .iter()
                .zip(&widths)
                .map(|(name, w)| format!("{:<width$}", name, width = w))
                .collect::<Vec<_>>()
                .join("  ");
            output_buf.push_str(&header);
            output_buf.push('\n');

            let sep: String = widths
                .iter()
                .map(|w| "─".repeat(*w))
                .collect::<Vec<_>>()
                .join("  ");
            output_buf.push_str(&sep);
            output_buf.push('\n');

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
                    output_buf.push_str(&line);
                    output_buf.push('\n');
                }
            }
            let n = rows.len();
            output_buf.push_str(&format!(
                "({} {})\n",
                n,
                if n == 1 { "row" } else { "rows" }
            ));

            print_with_pager(&output_buf, pager_enabled, pager_command);
        }
    }
}
