use pgtikv_admin::cli_common::ApiClient;

use crate::{make_auth_headers, require_token, OutputFormat};

use super::{output::print_sql_result, ReplState};

pub async fn repl_exec(
    api: &ApiClient,
    output: &OutputFormat,
    id: &str,
    timing: bool,
    repl_state: &ReplState,
    sql: &str,
) {
    let token = require_token();
    let headers = make_auth_headers(&token);
    let body = serde_json::json!({ "query": sql });

    let start = std::time::Instant::now();
    match api
        .try_request(
            "POST",
            &format!("/customer/databases/{id}/sql"),
            Some(&body),
            Some(&headers),
        )
        .await
    {
        Ok(data) => {
            print_sql_result(
                &data,
                output,
                repl_state.pager_enabled,
                &repl_state.pager_command,
            );
            if timing {
                eprintln!("Time: {:.3}s", start.elapsed().as_secs_f64());
            }
        }
        Err((_status, detail)) => {
            eprintln!("ERROR: {detail}");
        }
    }
}
