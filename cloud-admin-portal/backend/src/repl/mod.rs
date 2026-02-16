use pgtikv_admin::cli_common::ApiClient;
use rustyline::{error::ReadlineError, history::DefaultHistory, Config, Editor};

use crate::{make_auth_headers, require_token, OutputFormat};

pub mod commands;
pub mod completer;
pub mod exec;
pub mod output;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandedMode {
    Off,
    On,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxState {
    Idle,          // normal: "mydb> "
    InTransaction, // after BEGIN: "mydb*> "
    Failed,        // error in tx: "mydb!> "
}

pub struct ReplState {
    pub pager_enabled: bool,
    pub pager_command: Option<String>,
    pub expanded: ExpandedMode,
    pub output_file: Option<String>,
    pub last_query: Option<String>,
    pub db_id: String,
    pub db_name: String,
    pub api_url: String,
    pub tx_state: TxState,
}

impl ReplState {
    pub fn new(db_id: String, db_name: String, api_url: String) -> Self {
        Self {
            pager_enabled: true,
            pager_command: None,
            expanded: ExpandedMode::Off,
            output_file: None,
            last_query: None,
            db_id,
            db_name,
            api_url,
            tx_state: TxState::Idle,
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
    let db_name = db_info["name"]
        .as_str()
        .unwrap_or(id)
        .to_string();
    let api_url = api.base_url().to_string();

    let get_prompts = |tx_state: TxState, db_name: &str| {
        let suffix = match tx_state {
            TxState::Idle => "> ",
            TxState::InTransaction => "*> ",
            TxState::Failed => "!> ",
        };
        let cont_suffix = match tx_state {
            TxState::Idle => "-> ",
            TxState::InTransaction => "*-> ",
            TxState::Failed => "!-> ",
        };
        (
            format!("{}{}", db_name, suffix),
            format!("{}{}", " ".repeat(db_name.len().saturating_sub(1)), cont_suffix),
        )
    };

    let (mut prompt_main, mut prompt_cont) = get_prompts(TxState::Idle, &db_name);

    eprintln!("db9 sql — connected to '{}' ({})", db_name, id);
    eprintln!("Type \\? for help, \\q to quit.\n");

    let handle = tokio::runtime::Handle::current();
    let api = api.clone();
    let output = output.clone();
    let id = id.to_string();

    let repl_task = tokio::task::spawn_blocking(move || {
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

        let mut rl = match Editor::<completer::SqlHelper, DefaultHistory>::with_config(config) {
            Ok(editor) => editor,
            Err(e) => {
                eprintln!("Failed to initialize line editor: {e}");
                return;
            }
        };
        rl.set_helper(Some(completer::SqlHelper::new()));

        if let Ok(tables) = handle.block_on(commands::fetch_table_names(&api, &id)) {
            if let Some(helper) = rl.helper_mut() {
                helper.set_tables(tables);
            }
        }

        let history_path = crate::ensure_config_dir().join("history");
        rl.load_history(&history_path).ok();

        let mut buffer = String::new();
        let mut show_timing = true;
        let mut repl_state = ReplState::new(id.clone(), db_name.clone(), api_url);

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
                match handle.block_on(commands::dispatch(
                    &api,
                    &output,
                    &repl_state.db_id.clone(),
                    &mut show_timing,
                    &mut repl_state,
                    trimmed,
                )) {
                    commands::DispatchResult::Exit => break,
                    commands::DispatchResult::RefreshedTables(tables) => {
                        if let Some(helper) = rl.helper_mut() {
                            helper.set_tables(tables);
                        }
                    }
                    commands::DispatchResult::SwitchedDatabase { id, name, tables } => {
                        repl_state.db_id = id;
                        repl_state.db_name = name.clone();
                        repl_state.tx_state = TxState::Idle;
                        (prompt_main, prompt_cont) = get_prompts(TxState::Idle, &name);
                        if let Some(helper) = rl.helper_mut() {
                            helper.set_tables(tables);
                        }
                    }
                    commands::DispatchResult::ExecuteQuery(sql) => {
                        let new_tx_state = handle.block_on(exec::repl_exec(
                            &api,
                            &output,
                            &repl_state.db_id,
                            show_timing,
                            &repl_state,
                            &sql,
                        ));
                        repl_state.tx_state = new_tx_state;
                        (prompt_main, prompt_cont) = get_prompts(new_tx_state, &repl_state.db_name);
                        repl_state.last_query = Some(sql);
                    }
                    commands::DispatchResult::Continue => {}
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
                let new_tx_state = handle.block_on(exec::repl_exec(
                    &api,
                    &output,
                    &repl_state.db_id,
                    show_timing,
                    &repl_state,
                    &buffer,
                ));
                repl_state.tx_state = new_tx_state;
                (prompt_main, prompt_cont) = get_prompts(new_tx_state, &repl_state.db_name);
                repl_state.last_query = Some(buffer.clone());
                buffer.clear();
            }
        }
    });

    if let Err(e) = repl_task.await {
        eprintln!("REPL terminated unexpectedly: {e}");
    }

    eprintln!("Bye!");
}
