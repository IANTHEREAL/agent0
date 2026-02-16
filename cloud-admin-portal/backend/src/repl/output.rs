use pgtikv_admin::cli_common::print_json;
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

use super::ExpandedMode;
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

/// Get terminal width using crossterm
fn get_terminal_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(80)
}

/// Print SQL result in expanded (vertical) format
pub fn print_sql_result_expanded(
    data: &Value,
    pager_enabled: bool,
    pager_command: &Option<String>,
) {
    let columns = match data["columns"].as_array() {
        Some(cols) if !cols.is_empty() => cols,
        _ => {
            println!("{}", data["command"].as_str().unwrap_or("OK"));
            return;
        }
    };
    let rows = data["rows"].as_array().map(|r| r.as_slice()).unwrap_or(&[]);

    let col_names: Vec<String> = columns
        .iter()
        .map(|c| c["name"].as_str().unwrap_or("?").to_string())
        .collect();

    // Find max column name length for alignment
    let max_col_len = col_names.iter().map(|n| n.len()).max().unwrap_or(0);

    let mut output_buf = String::new();

    for (record_num, row) in rows.iter().enumerate() {
        if let Some(vals) = row.as_array() {
            // Record separator
            let sep_dashes = "─".repeat(max_col_len + 3);
            output_buf.push_str(&format!("-[ RECORD {} ]{}\n", record_num + 1, sep_dashes));

            // Key-value pairs
            for (i, col_name) in col_names.iter().enumerate() {
                let val_str = vals
                    .get(i)
                    .map(|v| match v {
                        Value::Null => "(null)".to_string(),
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_else(|| "(null)".to_string());

                output_buf.push_str(&format!(
                    "{:<width$} | {}\n",
                    col_name,
                    val_str,
                    width = max_col_len
                ));
            }
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
    expanded: ExpandedMode,
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

            let use_expanded = match expanded {
                ExpandedMode::On => true,
                ExpandedMode::Off => false,
                ExpandedMode::Auto => {
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
                    let total_width: usize = widths.iter().sum::<usize>() + (widths.len() - 1) * 2;
                    total_width > get_terminal_width()
                }
            };

            if use_expanded {
                print_sql_result_expanded(data, pager_enabled, pager_command);
            } else {
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
}

pub fn format_sql_result(data: &Value, output: &OutputFormat, expanded: ExpandedMode) -> String {
    if matches!(output, OutputFormat::Json) {
        return serde_json::to_string_pretty(data).unwrap_or_default() + "\n";
    }

    let columns = match data["columns"].as_array() {
        Some(cols) if !cols.is_empty() => cols,
        _ => {
            return format!("{}\n", data["command"].as_str().unwrap_or("OK"));
        }
    };
    let rows = data["rows"].as_array().map(|r| r.as_slice()).unwrap_or(&[]);

    match output {
        OutputFormat::Csv => {
            let mut buf = String::new();
            let header: Vec<&str> = columns.iter().filter_map(|c| c["name"].as_str()).collect();
            buf.push_str(&header.join(","));
            buf.push('\n');
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
                    buf.push_str(&line.join(","));
                    buf.push('\n');
                }
            }
            buf
        }
        _ => {
            let col_names: Vec<String> = columns
                .iter()
                .map(|c| c["name"].as_str().unwrap_or("?").to_string())
                .collect();

            let use_expanded = match expanded {
                ExpandedMode::On => true,
                ExpandedMode::Off => false,
                ExpandedMode::Auto => {
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
                    let total_width: usize = widths.iter().sum::<usize>() + (widths.len() - 1) * 2;
                    total_width > get_terminal_width()
                }
            };

            if use_expanded {
                format_expanded(data)
            } else {
                format_table(data)
            }
        }
    }
}

fn format_expanded(data: &Value) -> String {
    let columns = match data["columns"].as_array() {
        Some(cols) if !cols.is_empty() => cols,
        _ => return format!("{}\n", data["command"].as_str().unwrap_or("OK")),
    };
    let rows = data["rows"].as_array().map(|r| r.as_slice()).unwrap_or(&[]);

    let col_names: Vec<String> = columns
        .iter()
        .map(|c| c["name"].as_str().unwrap_or("?").to_string())
        .collect();
    let max_col_len = col_names.iter().map(|n| n.len()).max().unwrap_or(0);

    let mut buf = String::new();
    for (record_num, row) in rows.iter().enumerate() {
        if let Some(vals) = row.as_array() {
            let sep_dashes = "─".repeat(max_col_len + 3);
            buf.push_str(&format!("-[ RECORD {} ]{}\n", record_num + 1, sep_dashes));
            for (i, col_name) in col_names.iter().enumerate() {
                let val_str = vals
                    .get(i)
                    .map(|v| match v {
                        Value::Null => "(null)".to_string(),
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_else(|| "(null)".to_string());
                buf.push_str(&format!(
                    "{:<width$} | {}\n",
                    col_name,
                    val_str,
                    width = max_col_len
                ));
            }
        }
    }
    let n = rows.len();
    buf.push_str(&format!(
        "({} {})\n",
        n,
        if n == 1 { "row" } else { "rows" }
    ));
    buf
}

fn format_table(data: &Value) -> String {
    let columns = match data["columns"].as_array() {
        Some(cols) if !cols.is_empty() => cols,
        _ => return format!("{}\n", data["command"].as_str().unwrap_or("OK")),
    };
    let rows = data["rows"].as_array().map(|r| r.as_slice()).unwrap_or(&[]);

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

    let mut buf = String::new();

    let header: String = col_names
        .iter()
        .zip(&widths)
        .map(|(name, w)| format!("{:<width$}", name, width = w))
        .collect::<Vec<_>>()
        .join("  ");
    buf.push_str(&header);
    buf.push('\n');

    let sep: String = widths
        .iter()
        .map(|w| "─".repeat(*w))
        .collect::<Vec<_>>()
        .join("  ");
    buf.push_str(&sep);
    buf.push('\n');

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
            buf.push_str(&line);
            buf.push('\n');
        }
    }
    let n = rows.len();
    buf.push_str(&format!(
        "({} {})\n",
        n,
        if n == 1 { "row" } else { "rows" }
    ));
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expanded_output_single_row() {
        let data = serde_json::json!({
            "columns": [
                {"name": "id"},
                {"name": "name"},
                {"name": "email"}
            ],
            "rows": [
                [1, "Alice", "alice@example.com"]
            ]
        });

        let pager_enabled = false;
        let pager_command = None;

        print_sql_result_expanded(&data, pager_enabled, &pager_command);
    }

    #[test]
    fn test_expanded_output_multiple_rows() {
        let data = serde_json::json!({
            "columns": [
                {"name": "id"},
                {"name": "name"},
                {"name": "email"}
            ],
            "rows": [
                [1, "Alice", "alice@example.com"],
                [2, "Bob", "bob@example.com"]
            ]
        });

        print_sql_result_expanded(&data, false, &None);
    }

    #[test]
    fn test_expanded_output_with_nulls() {
        let data = serde_json::json!({
            "columns": [
                {"name": "id"},
                {"name": "name"},
                {"name": "email"}
            ],
            "rows": [
                [1, "Alice", Value::Null],
                [2, Value::Null, "bob@example.com"]
            ]
        });

        print_sql_result_expanded(&data, false, &None);
    }

    #[test]
    fn test_expanded_mode_toggle() {
        let mut mode = ExpandedMode::Off;
        assert_eq!(mode, ExpandedMode::Off);

        mode = ExpandedMode::On;
        assert_eq!(mode, ExpandedMode::On);

        mode = ExpandedMode::Auto;
        assert_eq!(mode, ExpandedMode::Auto);
    }

    #[test]
    fn test_repl_state_new() {
        let state = super::super::ReplState::new(
            "test_id".to_string(),
            "test_db".to_string(),
            "http://localhost".to_string(),
            super::super::SqlExecutor::Api,
        );
        assert_eq!(state.expanded, ExpandedMode::Off);
        assert!(state.pager_enabled);
        assert!(!state.is_direct());
    }
}
