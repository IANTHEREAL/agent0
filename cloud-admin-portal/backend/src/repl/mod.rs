use pgtikv_admin::cli_common::ApiClient;
use rustyline::{error::ReadlineError, history::DefaultHistory, Config, Editor};

use crate::{make_auth_headers, require_token, OutputFormat};

pub mod commands;
pub mod exec;
pub mod output;

pub struct ReplState {
    pub pager_enabled: bool,
    pub pager_command: Option<String>,
}

impl Default for ReplState {
    fn default() -> Self {
        Self {
            pager_enabled: true,
            pager_command: None,
        }
    }
}

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

    let handle = tokio::runtime::Handle::current();
    let api = api.clone();
    let output = output.clone();
    let id = id.to_string();

    let _ = tokio::task::spawn_blocking(move || {
        let config_builder = match Config::builder().history_ignore_dups(true) {
            Ok(builder) => builder,
            Err(e) => {
                eprintln!("Failed to configure line editor: {e}");
                return;
            }
        };
        let config_builder = config_builder.history_ignore_space(true);
        let config = match config_builder.max_history_size(10_000) {
            Ok(builder) => builder.build(),
            Err(e) => {
                eprintln!("Failed to configure line editor: {e}");
                return;
            }
        };

        let mut rl = match Editor::<(), DefaultHistory>::with_config(config) {
            Ok(editor) => editor,
            Err(e) => {
                eprintln!("Failed to initialize line editor: {e}");
                return;
            }
        };

        let history_path = crate::ensure_config_dir().join("history");
        rl.load_history(&history_path).ok();

        let mut buffer = String::new();
        let mut show_timing = true;
        let mut repl_state = ReplState::default();

        loop {
            let prompt = if buffer.is_empty() {
                &prompt_main
            } else {
                &prompt_cont
            };

            let line = match rl.readline(prompt) {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => {
                    buffer.clear();
                    continue;
                }
                Err(ReadlineError::Eof) => {
                    if !buffer.is_empty() {
                        eprintln!();
                    }
                    break;
                }
                Err(e) => {
                    eprintln!("\nRead error: {e}");
                    break;
                }
            };

            let trimmed = line.trim();

            if !trimmed.is_empty() {
                rl.add_history_entry(line.as_str()).ok();
            }
            rl.save_history(&history_path).ok();

            if trimmed.is_empty() {
                if buffer.is_empty() {
                    continue;
                }
                buffer.push('\n');
                continue;
            }

            if trimmed.starts_with('\\') {
                buffer.clear();
                if handle.block_on(commands::dispatch(
                    &api,
                    &output,
                    &id,
                    &mut show_timing,
                    &mut repl_state,
                    trimmed,
                )) {
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
                handle.block_on(exec::repl_exec(
                    &api,
                    &output,
                    &id,
                    show_timing,
                    &repl_state,
                    &buffer,
                ));
                buffer.clear();
            }
        }
    })
    .await;

    eprintln!("Bye!");
}
