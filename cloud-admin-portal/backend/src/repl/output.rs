use db9_admin::cli_common::print_json;
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

use super::{ExpandedMode, LinestyleMode};
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
    null_display: &str,
    _border: u8,
    linestyle: LinestyleMode,
) {
    let content = format_expanded(data, null_display, linestyle);
    print_with_pager(&content, pager_enabled, pager_command);
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
    null_display: &str,
    border: u8,
    linestyle: LinestyleMode,
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
            let null_len = null_display.len().max(1);

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
                                            Value::Null => null_len,
                                            Value::String(s) => s.len(),
                                            other => other.to_string().len(),
                                        })
                                        .unwrap_or(0)
                                })
                                .max()
                                .unwrap_or(0);
                            name.len().max(max_val)
                        })
                        .collect();
                    let n = widths.len();
                    let total_width: usize = widths.iter().sum::<usize>()
                        + match border {
                            0 => n.saturating_sub(1),
                            2 => 3 * n + 1,
                            _ => {
                                if n > 0 {
                                    3 * n - 1
                                } else {
                                    0
                                }
                            }
                        };
                    total_width > get_terminal_width()
                }
            };

            let content = if use_expanded {
                format_expanded(data, null_display, linestyle)
            } else {
                format_table(data, null_display, border, linestyle)
            };
            print_with_pager(&content, pager_enabled, pager_command);
        }
    }
}

pub fn format_sql_result(
    data: &Value,
    output: &OutputFormat,
    expanded: ExpandedMode,
    null_display: &str,
    border: u8,
    linestyle: LinestyleMode,
) -> String {
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
            let null_len = null_display.len().max(1);

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
                                            Value::Null => null_len,
                                            Value::String(s) => s.len(),
                                            other => other.to_string().len(),
                                        })
                                        .unwrap_or(0)
                                })
                                .max()
                                .unwrap_or(0);
                            name.len().max(max_val)
                        })
                        .collect();
                    let n = widths.len();
                    let total_width: usize = widths.iter().sum::<usize>()
                        + match border {
                            0 => n.saturating_sub(1),
                            2 => 3 * n + 1,
                            _ => {
                                if n > 0 {
                                    3 * n - 1
                                } else {
                                    0
                                }
                            }
                        };
                    total_width > get_terminal_width()
                }
            };

            if use_expanded {
                format_expanded(data, null_display, linestyle)
            } else {
                format_table(data, null_display, border, linestyle)
            }
        }
    }
}

fn format_expanded(data: &Value, null_display: &str, linestyle: LinestyleMode) -> String {
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

    let sep_char = match linestyle {
        LinestyleMode::Ascii => "-",
        LinestyleMode::Unicode => "─",
    };

    let mut buf = String::new();
    for (record_num, row) in rows.iter().enumerate() {
        if let Some(vals) = row.as_array() {
            let sep_dashes = sep_char.repeat(max_col_len + 3);
            buf.push_str(&format!("-[ RECORD {} ]{}\n", record_num + 1, sep_dashes));
            for (i, col_name) in col_names.iter().enumerate() {
                let val_str = vals
                    .get(i)
                    .map(|v| match v {
                        Value::Null => null_display.to_string(),
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_else(|| null_display.to_string());
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

fn format_table(data: &Value, null_display: &str, border: u8, linestyle: LinestyleMode) -> String {
    let columns = match data["columns"].as_array() {
        Some(cols) if !cols.is_empty() => cols,
        _ => return format!("{}\n", data["command"].as_str().unwrap_or("OK")),
    };
    let rows = data["rows"].as_array().map(|r| r.as_slice()).unwrap_or(&[]);

    let col_names: Vec<String> = columns
        .iter()
        .map(|c| c["name"].as_str().unwrap_or("?").to_string())
        .collect();

    let null_len = null_display.len();
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
                            Value::Null => null_len,
                            Value::String(s) => s.len(),
                            other => other.to_string().len(),
                        })
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0);
            name.len().max(max_val)
        })
        .collect();

    // Line drawing characters based on linestyle
    let (h, v, cross, tl, tr, bl, br, td, tu, tright, tleft) = match linestyle {
        LinestyleMode::Ascii => ("-", "|", "+", "+", "+", "+", "+", "+", "+", "+", "+"),
        LinestyleMode::Unicode => ("─", "│", "┼", "┌", "┐", "└", "┘", "┬", "┴", "├", "┤"),
    };

    let mut buf = String::new();

    match border {
        0 => {
            // No borders, columns separated by single space
            let header: String = col_names
                .iter()
                .zip(&widths)
                .map(|(name, w)| format!("{:<width$}", name, width = w))
                .collect::<Vec<_>>()
                .join(" ");
            buf.push_str(&header);
            buf.push('\n');

            for row in rows {
                if let Some(vals) = row.as_array() {
                    let line: String = vals
                        .iter()
                        .enumerate()
                        .map(|(i, vl)| {
                            let w = widths.get(i).copied().unwrap_or(1);
                            let s = match vl {
                                Value::Null => null_display.to_string(),
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            format!("{:<width$}", s, width = w)
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    buf.push_str(&line);
                    buf.push('\n');
                }
            }
        }
        2 => {
            // Full box border
            // Top: ┌──┬──┐ or +--+--+
            let segs: Vec<String> = widths.iter().map(|w| h.repeat(w + 2)).collect();
            buf.push_str(&format!("{}{}{}\n", tl, segs.join(td), tr));

            // Header: │ col │ col │ or | col | col |
            let hdr: Vec<String> = col_names
                .iter()
                .zip(&widths)
                .map(|(name, w)| format!(" {:<width$} ", name, width = w))
                .collect();
            buf.push_str(&format!("{}{}{}\n", v, hdr.join(v), v));

            // Header sep: ├──┼──┤ or +--+--+
            buf.push_str(&format!("{}{}{}\n", tright, segs.join(cross), tleft));

            // Data rows: │ val │ val │
            for row in rows {
                if let Some(vals) = row.as_array() {
                    let cells: Vec<String> = vals
                        .iter()
                        .enumerate()
                        .map(|(i, vl)| {
                            let w = widths.get(i).copied().unwrap_or(1);
                            let s = match vl {
                                Value::Null => null_display.to_string(),
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            format!(" {:<width$} ", s, width = w)
                        })
                        .collect();
                    buf.push_str(&format!("{}{}{}\n", v, cells.join(v), v));
                }
            }

            // Bottom: └──┴──┘ or +--+--+
            buf.push_str(&format!("{}{}{}\n", bl, segs.join(tu), br));
        }
        _ => {
            // border=1 (default): header separator, column separators
            let hdr: String = col_names
                .iter()
                .zip(&widths)
                .map(|(name, w)| format!(" {:<width$} ", name, width = w))
                .collect::<Vec<_>>()
                .join(v);
            buf.push_str(&hdr);
            buf.push('\n');

            // Separator
            let sep: String = widths
                .iter()
                .map(|w| h.repeat(w + 2))
                .collect::<Vec<_>>()
                .join(cross);
            buf.push_str(&sep);
            buf.push('\n');

            // Data rows
            for row in rows {
                if let Some(vals) = row.as_array() {
                    let line: String = vals
                        .iter()
                        .enumerate()
                        .map(|(i, vl)| {
                            let w = widths.get(i).copied().unwrap_or(1);
                            let s = match vl {
                                Value::Null => null_display.to_string(),
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            format!(" {:<width$} ", s, width = w)
                        })
                        .collect::<Vec<_>>()
                        .join(v);
                    buf.push_str(&line);
                    buf.push('\n');
                }
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

        print_sql_result_expanded(&data, false, &None, "NULL", 1, LinestyleMode::Ascii);
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

        print_sql_result_expanded(&data, false, &None, "NULL", 1, LinestyleMode::Ascii);
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

        print_sql_result_expanded(&data, false, &None, "NULL", 1, LinestyleMode::Ascii);
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

    #[test]
    fn test_format_sql_result_json_mode() {
        let data = serde_json::json!({
            "columns": [{"name": "id"}],
            "rows": [[1]],
            "command": "SELECT 1"
        });
        let result = format_sql_result(
            &data,
            &crate::OutputFormat::Json,
            ExpandedMode::Off,
            "NULL",
            1,
            LinestyleMode::Ascii,
        );
        assert!(result.contains("\"id\""));
        assert!(result.contains("1"));
    }

    #[test]
    fn test_format_sql_result_command_only() {
        let data = serde_json::json!({
            "columns": [],
            "rows": [],
            "command": "CREATE TABLE"
        });
        let result = format_sql_result(
            &data,
            &crate::OutputFormat::Table,
            ExpandedMode::Off,
            "NULL",
            1,
            LinestyleMode::Ascii,
        );
        assert!(result.contains("CREATE TABLE"));
    }

    #[test]
    fn test_format_sql_result_with_nulls() {
        let data = serde_json::json!({
            "columns": [{"name": "a"}, {"name": "b"}],
            "rows": [[1, null], [null, "hello"]],
            "command": "SELECT 2"
        });
        let result = format_sql_result(
            &data,
            &crate::OutputFormat::Table,
            ExpandedMode::Off,
            "NULL",
            1,
            LinestyleMode::Ascii,
        );
        assert!(result.contains("NULL"));
    }

    #[test]
    fn test_expanded_mode_auto_narrow_data() {
        let data = serde_json::json!({
            "columns": [{"name": "id"}],
            "rows": [[1]],
            "command": "SELECT 1"
        });
        print_sql_result(
            &data,
            &crate::OutputFormat::Table,
            false,
            &None,
            ExpandedMode::Auto,
            "NULL",
            1,
            LinestyleMode::Ascii,
        );
    }

    #[test]
    fn test_print_sql_result_empty_rows() {
        let data = serde_json::json!({
            "columns": [{"name": "id"}, {"name": "name"}],
            "rows": [],
            "command": "SELECT 0"
        });
        print_sql_result(
            &data,
            &crate::OutputFormat::Table,
            false,
            &None,
            ExpandedMode::Off,
            "NULL",
            1,
            LinestyleMode::Ascii,
        );
    }

    #[test]
    fn test_expanded_output_empty_rows() {
        let data = serde_json::json!({
            "columns": [{"name": "id"}],
            "rows": [],
            "command": "SELECT 0"
        });
        print_sql_result_expanded(&data, false, &None, "NULL", 1, LinestyleMode::Ascii);
    }

    #[test]
    fn test_format_table_border_0() {
        let data = serde_json::json!({
            "columns": [{"name": "id"}, {"name": "name"}],
            "rows": [[1, "Alice"], [2, "Bob"]],
        });
        let result = format_table(&data, "NULL", 0, LinestyleMode::Ascii);
        assert!(result.contains("id name"));
        assert!(!result.contains("|"));
        assert!(!result.contains("-"));
        assert!(result.contains("Alice"));
        assert!(result.contains("(2 rows)"));
    }

    #[test]
    fn test_format_table_border_2() {
        let data = serde_json::json!({
            "columns": [{"name": "id"}, {"name": "name"}],
            "rows": [[1, "Alice"]],
        });
        let result = format_table(&data, "NULL", 2, LinestyleMode::Ascii);
        assert!(result.contains("+"));
        assert!(result.contains("|"));
        assert!(result.contains("-"));
        let lines: Vec<&str> = result.lines().collect();
        assert!(lines.len() >= 5);
        assert!(lines[0].starts_with('+'));
        assert!(lines[0].ends_with('+'));
    }

    #[test]
    fn test_format_table_border_2_unicode() {
        let data = serde_json::json!({
            "columns": [{"name": "id"}, {"name": "name"}],
            "rows": [[1, "Alice"]],
        });
        let result = format_table(&data, "NULL", 2, LinestyleMode::Unicode);
        assert!(result.contains("┌"));
        assert!(result.contains("┐"));
        assert!(result.contains("└"));
        assert!(result.contains("┘"));
        assert!(result.contains("│"));
        assert!(result.contains("─"));
    }

    #[test]
    fn test_format_table_custom_null() {
        let data = serde_json::json!({
            "columns": [{"name": "a"}, {"name": "b"}],
            "rows": [[1, null], [null, "hello"]],
        });
        let result = format_table(&data, "(empty)", 1, LinestyleMode::Ascii);
        assert!(result.contains("(empty)"));
        assert!(!result.contains("NULL"));
    }

    #[test]
    fn test_format_table_unicode_linestyle() {
        let data = serde_json::json!({
            "columns": [{"name": "id"}, {"name": "name"}],
            "rows": [[1, "Alice"], [2, "Bob"]],
        });
        let result = format_table(&data, "NULL", 1, LinestyleMode::Unicode);
        assert!(result.contains("│"));
        assert!(result.contains("─"));
        assert!(result.contains("┼"));
        assert!(!result.contains("|"));
        assert!(!result.contains("+"));
    }

    #[test]
    fn test_format_table_ascii_linestyle() {
        let data = serde_json::json!({
            "columns": [{"name": "id"}, {"name": "name"}],
            "rows": [[1, "Alice"]],
        });
        let result = format_table(&data, "NULL", 1, LinestyleMode::Ascii);
        assert!(result.contains("|"));
        assert!(result.contains("-"));
        assert!(result.contains("+"));
    }
}
