use std::io::Write;

use pgtikv_admin::cli_common::ApiClient;

use crate::{make_auth_headers, require_token, OutputFormat};

use super::{output::print_sql_result, ReplState, SqlExecutor, TxState};

pub async fn repl_exec(
    api: &ApiClient,
    output: &OutputFormat,
    id: &str,
    timing: bool,
    repl_state: &ReplState,
    sql: &str,
) -> TxState {
    let start = std::time::Instant::now();

    let result = match &repl_state.executor {
        SqlExecutor::Direct(executor) => executor.execute(sql).await.map_err(|e| e),
        SqlExecutor::Api => {
            let token = require_token();
            let headers = make_auth_headers(&token);
            let body = serde_json::json!({ "query": sql });
            api.try_request(
                "POST",
                &format!("/customer/databases/{id}/sql"),
                Some(&body),
                Some(&headers),
            )
            .await
            .map_err(|(_status, detail)| detail)
        }
    };

    match result {
        Ok(data) => {
            if let Some(err) = data.get("error").and_then(|v| v.as_str()) {
                let new_tx_state = if repl_state.tx_state == TxState::InTransaction {
                    TxState::Failed
                } else {
                    TxState::Idle
                };
                print_error_with_hints(err);
                if timing {
                    eprintln!("Time: {:.3}s", start.elapsed().as_secs_f64());
                }
                return new_tx_state;
            }

            let new_tx_state = detect_tx_state_change(&data, repl_state.tx_state);

            let effective_output = repl_state.format_override.as_ref().unwrap_or(output);

            if let Some(ref path) = repl_state.output_file {
                let formatted = super::output::format_sql_result(
                    &data,
                    effective_output,
                    repl_state.expanded,
                    &repl_state.null_display,
                    repl_state.border,
                    repl_state.linestyle,
                );
                match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    Ok(mut file) => {
                        if let Err(e) = file.write_all(formatted.as_bytes()) {
                            eprintln!("ERROR: Failed to write to '{}': {}", path, e);
                        }
                    }
                    Err(e) => {
                        eprintln!("ERROR: Cannot open '{}': {}", path, e);
                    }
                }
            } else {
                print_sql_result(
                    &data,
                    effective_output,
                    repl_state.pager_enabled,
                    &repl_state.pager_command,
                    repl_state.expanded,
                    &repl_state.null_display,
                    repl_state.border,
                    repl_state.linestyle,
                );
            }
            if timing {
                eprintln!("Time: {:.3}s", start.elapsed().as_secs_f64());
            }
            new_tx_state
        }
        Err(detail) => {
            let new_tx_state = if repl_state.tx_state == TxState::InTransaction {
                TxState::Failed
            } else {
                TxState::Idle
            };

            print_error_with_hints(&detail);
            if timing {
                eprintln!("Time: {:.3}s", start.elapsed().as_secs_f64());
            }
            new_tx_state
        }
    }
}

fn detect_tx_state_change(data: &serde_json::Value, current_state: TxState) -> TxState {
    if let Some(command) = data["command"].as_str() {
        match command.to_uppercase().as_str() {
            "BEGIN" => TxState::InTransaction,
            "COMMIT" | "ROLLBACK" => TxState::Idle,
            _ => current_state,
        }
    } else {
        current_state
    }
}

fn print_error_with_hints(detail: &str) {
    eprintln!("\x1b[31mERROR:\x1b[0m {}", detail);
    
    if detail.contains("relation") && detail.contains("does not exist") {
        eprintln!("Hint: Run \\dt to see available tables");
    } else if detail.contains("column") && detail.contains("does not exist") {
        eprintln!("Hint: Run \\d <table> to see columns");
    } else if detail.contains("syntax error") {
        eprintln!("Hint: Check SQL syntax near the error position");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_tx_state_begin() {
        let data = serde_json::json!({
            "command": "BEGIN",
            "columns": [],
            "rows": []
        });
        let new_state = detect_tx_state_change(&data, TxState::Idle);
        assert_eq!(new_state, TxState::InTransaction);
    }

    #[test]
    fn test_detect_tx_state_commit() {
        let data = serde_json::json!({
            "command": "COMMIT",
            "columns": [],
            "rows": []
        });
        let new_state = detect_tx_state_change(&data, TxState::InTransaction);
        assert_eq!(new_state, TxState::Idle);
    }

    #[test]
    fn test_detect_tx_state_rollback() {
        let data = serde_json::json!({
            "command": "ROLLBACK",
            "columns": [],
            "rows": []
        });
        let new_state = detect_tx_state_change(&data, TxState::InTransaction);
        assert_eq!(new_state, TxState::Idle);
    }

    #[test]
    fn test_detect_tx_state_select_preserves_state() {
        let data = serde_json::json!({
            "command": "SELECT",
            "columns": [{"name": "id"}],
            "rows": [[1]]
        });
        let new_state = detect_tx_state_change(&data, TxState::InTransaction);
        assert_eq!(new_state, TxState::InTransaction);
    }

    #[test]
    fn test_detect_tx_state_case_insensitive() {
        let data = serde_json::json!({
            "command": "begin",
            "columns": [],
            "rows": []
        });
        let new_state = detect_tx_state_change(&data, TxState::Idle);
        assert_eq!(new_state, TxState::InTransaction);
    }

    #[test]
    fn test_error_state_in_transaction() {
        let current_state = TxState::InTransaction;
        let new_state = if current_state == TxState::InTransaction {
            TxState::Failed
        } else {
            TxState::Idle
        };
        assert_eq!(new_state, TxState::Failed);
    }

    #[test]
    fn test_error_state_idle() {
        let current_state = TxState::Idle;
        let new_state = if current_state == TxState::InTransaction {
            TxState::Failed
        } else {
            TxState::Idle
        };
        assert_eq!(new_state, TxState::Idle);
    }

    #[test]
    fn test_detect_tx_state_missing_command() {
        let data = serde_json::json!({
            "columns": [],
            "rows": []
        });
        let new_state = detect_tx_state_change(&data, TxState::InTransaction);
        assert_eq!(new_state, TxState::InTransaction);
    }

    #[test]
    fn test_detect_tx_state_null_command() {
        let data = serde_json::json!({
            "command": null,
            "columns": [],
            "rows": []
        });
        let new_state = detect_tx_state_change(&data, TxState::Idle);
        assert_eq!(new_state, TxState::Idle);
    }

    #[test]
    fn test_detect_tx_state_savepoint_preserves() {
        let data = serde_json::json!({
            "command": "SAVEPOINT",
            "columns": [],
            "rows": []
        });
        let new_state = detect_tx_state_change(&data, TxState::InTransaction);
        assert_eq!(new_state, TxState::InTransaction);
    }

    #[test]
    fn test_detect_tx_state_rollback_from_failed() {
        let data = serde_json::json!({
            "command": "ROLLBACK",
            "columns": [],
            "rows": []
        });
        let new_state = detect_tx_state_change(&data, TxState::Failed);
        assert_eq!(new_state, TxState::Idle);
    }

    #[test]
    fn test_print_error_relation_hint() {
        print_error_with_hints("ERROR: relation \"foo\" does not exist");
    }

    #[test]
    fn test_print_error_column_hint() {
        print_error_with_hints("ERROR: column \"bar\" does not exist");
    }

    #[test]
    fn test_print_error_syntax_hint() {
        print_error_with_hints("ERROR: syntax error at or near \"SELEC\"");
    }

    #[test]
    fn test_print_error_generic() {
        print_error_with_hints("ERROR: something unexpected happened");
    }
}
