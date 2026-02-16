use pgtikv_admin::cli_common::ApiClient;
use rustyline::{error::ReadlineError, history::DefaultHistory, Config, Editor};

use crate::{make_auth_headers, require_token, OutputFormat};

pub mod commands;
pub mod completer;
pub mod config;
pub mod direct;
pub mod exec;
pub mod favorites;
pub mod output;

pub enum SqlExecutor {
    Api,
    Direct(direct::DirectExecutor),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandedMode {
    Off,
    On,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinestyleMode {
    Ascii,
    Unicode,
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
    pub null_display: String,
    pub border: u8,
    pub linestyle: LinestyleMode,
    pub format_override: Option<crate::OutputFormat>,
    pub highlight_enabled: bool,
    pub output_file: Option<String>,
    pub last_query: Option<String>,
    pub db_id: String,
    pub db_name: String,
    pub api_url: String,
    pub tx_state: TxState,
    pub favorites: favorites::Favorites,
    pub executor: SqlExecutor,
}

impl ReplState {
    pub fn new(db_id: String, db_name: String, api_url: String, executor: SqlExecutor) -> Self {
        Self {
            pager_enabled: true,
            pager_command: None,
            expanded: ExpandedMode::Off,
            null_display: "NULL".to_string(),
            border: 1,
            linestyle: LinestyleMode::Ascii,
            format_override: None,
            highlight_enabled: true,
            output_file: None,
            last_query: None,
            db_id,
            db_name,
            api_url,
            tx_state: TxState::Idle,
            favorites: favorites::Favorites::load(),
            executor,
        }
    }

    pub fn with_config(
        db_id: String,
        db_name: String,
        api_url: String,
        executor: SqlExecutor,
        cfg: &config::FileConfig,
    ) -> Self {
        let repl = cfg.repl.as_ref();
        Self {
            pager_enabled: repl.and_then(|r| r.pager).unwrap_or(true),
            pager_command: repl.and_then(|r| r.pager_command.clone()),
            expanded: match repl.and_then(|r| r.expanded.as_deref()) {
                Some("on") => ExpandedMode::On,
                Some("auto") => ExpandedMode::Auto,
                _ => ExpandedMode::Off,
            },
            null_display: repl
                .and_then(|r| r.null_display.clone())
                .unwrap_or_else(|| "NULL".to_string()),
            border: repl.and_then(|r| r.border).unwrap_or(1).min(2),
            linestyle: match repl.and_then(|r| r.linestyle.as_deref()) {
                Some("unicode") => LinestyleMode::Unicode,
                _ => LinestyleMode::Ascii,
            },
            format_override: None,
            highlight_enabled: repl.and_then(|r| r.highlight).unwrap_or(true),
            output_file: None,
            last_query: None,
            db_id,
            db_name,
            api_url,
            tx_state: TxState::Idle,
            favorites: favorites::Favorites::load(),
            executor,
        }
    }

    pub fn is_direct(&self) -> bool {
        matches!(self.executor, SqlExecutor::Direct(_))
    }
}

pub async fn run(api: &ApiClient, output: &OutputFormat, id: &str, executor: SqlExecutor) {
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
    let is_direct = matches!(executor, SqlExecutor::Direct(_));

    let get_prompts = |tx_state: TxState, db_name: &str, direct: bool| {
        let mode = if direct { "(direct)" } else { "" };
        let suffix = match tx_state {
            TxState::Idle => "=> ",
            TxState::InTransaction => "*=> ",
            TxState::Failed => "!=> ",
        };
        let cont_suffix = match tx_state {
            TxState::Idle => "-> ",
            TxState::InTransaction => "*-> ",
            TxState::Failed => "!-> ",
        };
        let prefix = format!("db9:{}{}", db_name, mode);
        let prefix_len = prefix.len();
        (
            format!("{}{}", prefix, suffix),
            format!("{}{}", " ".repeat(prefix_len.saturating_sub(1)), cont_suffix),
        )
    };

    let (mut prompt_main, mut prompt_cont) = get_prompts(TxState::Idle, &db_name, is_direct);

    let mode_label = if is_direct { " (direct pgwire)" } else { "" };
    eprintln!("db9 sql — connected to '{}' ({}){}", db_name, id, mode_label);
    eprintln!("Type \\? for help, \\q to quit.\n");

    let handle = tokio::runtime::Handle::current();
    let api = api.clone();
    let output = output.clone();
    let id = id.to_string();

    let file_config = config::load_config();

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
        let mut show_timing = file_config.repl.as_ref().and_then(|r| r.timing).unwrap_or(true);
        let mut repl_state =
            ReplState::with_config(id.clone(), db_name.clone(), api_url, executor, &file_config);

        if !repl_state.highlight_enabled {
            if let Some(helper) = rl.helper_mut() {
                helper.set_highlighting(false);
            }
        }

        if let Some(ref startup) = file_config.startup {
            if let Some(ref cmds) = startup.commands {
                for cmd_str in cmds {
                    let trimmed_cmd = cmd_str.trim();
                    if trimmed_cmd.starts_with('\\') {
                        handle.block_on(commands::dispatch(
                            &api,
                            &output,
                            &repl_state.db_id.clone(),
                            &mut show_timing,
                            &mut repl_state,
                            trimmed_cmd,
                        ));
                    }
                }
            }
        }

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

            // Handle \g and \gx as buffer terminators (BEFORE the backslash dispatch
            // which would clear the buffer)
            if trimmed == "\\g" || trimmed == "\\gx" {
                let is_expanded = trimmed == "\\gx";
                let sql = if !buffer.is_empty() {
                    buffer.clone()
                } else if let Some(ref q) = repl_state.last_query {
                    q.clone()
                } else {
                    eprintln!("No query to execute.");
                    continue;
                };

                // Temporarily override expanded mode for \gx
                let prev_expanded = repl_state.expanded;
                if is_expanded {
                    repl_state.expanded = ExpandedMode::On;
                }

                let new_tx_state = handle.block_on(exec::repl_exec(
                    &api,
                    &output,
                    &repl_state.db_id,
                    show_timing,
                    &repl_state,
                    &sql,
                ));

                // Restore expanded mode
                if is_expanded {
                    repl_state.expanded = prev_expanded;
                }

                repl_state.tx_state = new_tx_state;
                (prompt_main, prompt_cont) =
                    get_prompts(new_tx_state, &repl_state.db_name, is_direct);
                repl_state.last_query = Some(sql);
                buffer.clear();
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
                        (prompt_main, prompt_cont) = get_prompts(TxState::Idle, &name, is_direct);
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
                        (prompt_main, prompt_cont) = get_prompts(new_tx_state, &repl_state.db_name, is_direct);
                        repl_state.last_query = Some(sql);
                    }
                    commands::DispatchResult::HighlightChanged(enabled) => {
                        if let Some(helper) = rl.helper_mut() {
                            helper.set_highlighting(enabled);
                        }
                    }
                    commands::DispatchResult::Watch(secs) => {
                        if let Some(ref query) = repl_state.last_query {
                            let query = query.clone();
                            let _ = crossterm::terminal::enable_raw_mode();
                            loop {
                                print!("\x1b[2J\x1b[H");
                                let now = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| {
                                        let secs_total = d.as_secs();
                                        let hours = (secs_total % 86400) / 3600;
                                        let mins = (secs_total % 3600) / 60;
                                        let secs_r = secs_total % 60;
                                        format!("{:02}:{:02}:{:02} UTC", hours, mins, secs_r)
                                    })
                                    .unwrap_or_else(|_| "??:??:??".to_string());
                                eprintln!("\\watch every {}s  {}", secs, now);
                                eprintln!();

                                let new_tx_state = handle.block_on(exec::repl_exec(
                                    &api, &output, &repl_state.db_id, show_timing, &repl_state, &query,
                                ));
                                repl_state.tx_state = new_tx_state;

                                use crossterm::event::{poll, read};
                                match poll(std::time::Duration::from_secs(secs)) {
                                    Ok(true) => {
                                        let _ = read();
                                        break;
                                    }
                                    Ok(false) => {}
                                    Err(_) => break,
                                }
                            }
                            let _ = crossterm::terminal::disable_raw_mode();
                            (prompt_main, prompt_cont) =
                                get_prompts(repl_state.tx_state, &repl_state.db_name, is_direct);
                            eprintln!("Watch stopped.");
                        } else {
                            eprintln!("No query to execute. Run a query first, then use \\watch.");
                        }
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

            // Check for buffer terminators: ; or \g or \gx
            let (should_execute, is_expanded_override) = if trimmed.ends_with(';') {
                (true, false)
            } else if buffer.ends_with("\\gx") {
                // Strip \gx from buffer
                buffer.truncate(buffer.len() - 3);
                (true, true)
            } else if buffer.ends_with("\\g") {
                // Strip \g from buffer
                buffer.truncate(buffer.len() - 2);
                (true, false)
            } else {
                (false, false)
            };

            if should_execute {
                let sql = buffer.trim().to_string();
                if sql.is_empty() {
                    buffer.clear();
                    continue;
                }

                let prev_expanded = repl_state.expanded;
                if is_expanded_override {
                    repl_state.expanded = ExpandedMode::On;
                }

                let new_tx_state = handle.block_on(exec::repl_exec(
                    &api, &output, &repl_state.db_id, show_timing, &repl_state, &sql,
                ));

                if is_expanded_override {
                    repl_state.expanded = prev_expanded;
                }

                repl_state.tx_state = new_tx_state;
                (prompt_main, prompt_cont) = get_prompts(new_tx_state, &repl_state.db_name, is_direct);
                repl_state.last_query = Some(sql);
                buffer.clear();
            }
        }
    });

    if let Err(e) = repl_task.await {
        eprintln!("REPL terminated unexpectedly: {e}");
    }

    eprintln!("Bye!");
}
