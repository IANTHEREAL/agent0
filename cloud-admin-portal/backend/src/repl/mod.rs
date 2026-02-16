use std::io::{self, Write};

use pgtikv_admin::cli_common::ApiClient;

use crate::{make_auth_headers, require_token, OutputFormat};

pub mod commands;
pub mod exec;
pub mod output;

pub async fn run(api: &ApiClient, output: &OutputFormat, id: &str) {
    let token = require_token();
    let headers = make_auth_headers(&token);
    let db_info = api
        .request(
            "GET",
            &format!("/customer/databases/{id}"),
            None,
            Some(&headers),
        )
        .await;
    let db_name = db_info["name"].as_str().unwrap_or(id);

    let prompt_main = format!("{}> ", db_name);
    let prompt_cont = format!("{}-> ", " ".repeat(db_name.len().saturating_sub(1)));

    eprintln!("db9 sql — connected to '{}' ({})", db_name, id);
    eprintln!("Type \\? for help, \\q to quit.\n");

    let stdin = io::stdin();
    let mut buffer = String::new();
    let mut show_timing = true;

    loop {
        if buffer.is_empty() {
            eprint!("{}", prompt_main);
        } else {
            eprint!("{}", prompt_cont);
        }
        io::stderr().flush().ok();

        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => {
                if !buffer.is_empty() {
                    eprintln!();
                }
                break;
            }
            Err(e) => {
                eprintln!("\nRead error: {e}");
                break;
            }
            _ => {}
        }

        let trimmed = line.trim();

        if trimmed.is_empty() {
            if buffer.is_empty() {
                continue;
            }
            buffer.push('\n');
            continue;
        }

        if trimmed.starts_with('\\') {
            buffer.clear();
            if commands::dispatch(api, output, id, &mut show_timing, trimmed).await {
                break;
            }
            continue;
        }

        if buffer.is_empty()
            && (trimmed.eq_ignore_ascii_case("exit") || trimmed.eq_ignore_ascii_case("quit"))
        {
            break;
        }

        if !buffer.is_empty() {
            buffer.push('\n');
        }
        buffer.push_str(trimmed);

        if trimmed.ends_with(';') {
            exec::repl_exec(api, output, id, show_timing, &buffer).await;
            buffer.clear();
        }
    }

    eprintln!("Bye!");
}
