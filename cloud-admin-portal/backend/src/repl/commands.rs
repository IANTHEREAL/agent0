use std::process::Command;

use pgtikv_admin::cli_common::ApiClient;
use serde_json::Value;

use crate::{make_auth_headers, require_token, OutputFormat};

use super::{exec::repl_exec, ExpandedMode, LinestyleMode, ReplState};

pub enum DispatchResult {
    Continue,
    Exit,
    RefreshedTables(Vec<String>),
    SwitchedDatabase {
        id: String,
        name: String,
        tables: Vec<String>,
    },
    ExecuteQuery(String),
    HighlightChanged(bool),
    Watch(u64),
}

pub async fn fetch_table_names(api: &ApiClient, id: &str) -> Result<Vec<String>, String> {
    let token = require_token();
    let headers = make_auth_headers(&token);
    let body = serde_json::json!({
        "query": "SELECT table_name FROM information_schema.tables \
                  WHERE table_schema NOT IN ('pg_catalog','information_schema') \
                  ORDER BY table_name"
    });

    match api
        .try_request(
            "POST",
            &format!("/customer/databases/{id}/sql"),
            Some(&body),
            Some(&headers),
        )
        .await
    {
        Ok(data) => Ok(extract_table_names(&data)),
        Err((_status, detail)) => Err(detail),
    }
}

fn extract_table_names(data: &Value) -> Vec<String> {
    let mut tables = Vec::new();

    if let Some(rows) = data["rows"].as_array() {
        for row in rows {
            if let Some(name) = row
                .as_array()
                .and_then(|vals| vals.first())
                .and_then(|v| v.as_str())
            {
                tables.push(name.to_string());
            }
        }
    }

    tables
}

pub async fn dispatch(
    api: &ApiClient,
    output: &OutputFormat,
    id: &str,
    show_timing: &mut bool,
    repl_state: &mut ReplState,
    trimmed: &str,
) -> DispatchResult {
    let (cmd, arg) = match trimmed.find(char::is_whitespace) {
        Some(pos) => (&trimmed[..pos], trimmed[pos..].trim()),
        None => (trimmed, ""),
    };

    match cmd {
        "\\fs" => {
            handle_save_favorite(repl_state, arg);
            DispatchResult::Continue
        }
        "\\f" => {
            if arg.is_empty() {
                eprintln!("Usage: \\f <favorite-name>");
                DispatchResult::Continue
            } else {
                handle_execute_favorite(repl_state, arg)
            }
        }
        "\\fd" => {
            if arg.is_empty() {
                eprintln!("Usage: \\fd <favorite-name>");
                DispatchResult::Continue
            } else {
                handle_delete_favorite(repl_state, arg);
                DispatchResult::Continue
            }
        }
        "\\fl" => {
            handle_list_favorites(repl_state);
            DispatchResult::Continue
        }
        "\\q" | "\\quit" => DispatchResult::Exit,
        "\\?" | "\\help" => {
            repl_help();
            DispatchResult::Continue
        }
        "\\dt" => {
            repl_exec(
                api,
                output,
                id,
                *show_timing,
                repl_state,
                "SELECT table_schema, table_name FROM information_schema.tables \
                 WHERE table_schema NOT IN ('pg_catalog','information_schema') \
                 ORDER BY table_schema, table_name",
            )
            .await;
            DispatchResult::Continue
        }
        "\\dn" => {
            repl_exec(
                api,
                output,
                id,
                *show_timing,
                repl_state,
                "SELECT schema_name FROM information_schema.schemata \
                 WHERE schema_name NOT IN ('pg_catalog','information_schema') \
                 ORDER BY schema_name",
            )
            .await;
            DispatchResult::Continue
        }
        "\\di" => {
            repl_exec(
                api,
                output,
                id,
                *show_timing,
                repl_state,
                "SELECT schemaname, tablename, indexname FROM pg_indexes \
                 WHERE schemaname NOT IN ('pg_catalog','information_schema') \
                 ORDER BY schemaname, tablename, indexname",
            )
            .await;
            DispatchResult::Continue
        }
        "\\dv" => {
            repl_exec(
                api,
                output,
                id,
                *show_timing,
                repl_state,
                "SELECT table_schema, table_name FROM information_schema.tables \
                 WHERE table_type = 'VIEW' \
                 AND table_schema NOT IN ('pg_catalog','information_schema') \
                 ORDER BY table_schema, table_name",
            )
            .await;
            DispatchResult::Continue
        }
        "\\d" => {
            if arg.is_empty() {
                repl_exec(
                    api,
                    output,
                    id,
                    *show_timing,
                    repl_state,
                    "SELECT table_schema, table_name, table_type FROM information_schema.tables \
                     WHERE table_schema NOT IN ('pg_catalog','information_schema') \
                     ORDER BY table_schema, table_name",
                )
                .await;
            } else {
                let safe = arg.replace('\'', "''");
                repl_exec(
                    api,
                    output,
                    id,
                    *show_timing,
                    repl_state,
                    &format!(
                        "SELECT column_name, data_type, is_nullable, column_default \
                         FROM information_schema.columns \
                         WHERE table_name = '{}' ORDER BY ordinal_position",
                        safe
                    ),
                )
                .await;
            }
            DispatchResult::Continue
        }
        "\\du" => {
            handle_list_users(api, id).await;
            DispatchResult::Continue
        }
        "\\l" => {
            handle_list_databases(api).await;
            DispatchResult::Continue
        }
        "\\conninfo" => {
            handle_conninfo(repl_state);
            DispatchResult::Continue
        }
        "\\i" => {
            if arg.is_empty() {
                eprintln!("Usage: \\i <filename>");
            } else {
                handle_include_file(api, output, id, *show_timing, repl_state, arg).await;
            }
            DispatchResult::Continue
        }
        "\\o" => {
            handle_output_redirect(repl_state, arg);
            DispatchResult::Continue
        }
        "\\e" => handle_edit(repl_state),
        "\\c" => {
            if arg.is_empty() {
                eprintln!("Usage: \\c <database-id>");
                DispatchResult::Continue
            } else {
                handle_connect(api, arg).await
            }
        }
        "\\refresh" => match fetch_table_names(api, id).await {
            Ok(tables) => {
                eprintln!("Refreshed {} table name(s).", tables.len());
                DispatchResult::RefreshedTables(tables)
            }
            Err(detail) => {
                eprintln!("ERROR: {detail}");
                DispatchResult::Continue
            }
        },
        "\\timing" => {
            *show_timing = !*show_timing;
            eprintln!("Timing is {}.", if *show_timing { "on" } else { "off" });
            DispatchResult::Continue
        }
        "\\pager" => {
            handle_pager_command(repl_state, arg);
            DispatchResult::Continue
        }
        "\\x" => {
            handle_expanded_command(repl_state, arg);
            DispatchResult::Continue
        }
        "\\highlight" => handle_highlight_command(arg),
        "\\pset" => {
            handle_pset_command(repl_state, arg);
            DispatchResult::Continue
        }
        "\\watch" => handle_watch_command(arg),
        "\\!" => handle_shell_command(arg),
        _ => {
            eprintln!("Unknown command: {cmd}. Type \\? for help.");
            DispatchResult::Continue
        }
    }
}

// ── \du — list users ────────────────────────────────────────────

async fn handle_list_users(api: &ApiClient, id: &str) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    match api
        .try_request(
            "GET",
            &format!("/customer/databases/{id}/users"),
            None,
            Some(&headers),
        )
        .await
    {
        Ok(data) => {
            let items = data.as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                eprintln!("No users found.");
                return;
            }

            let mut w_name = 8usize;
            let mut w_super = 9usize;
            let mut w_login = 9usize;
            for item in &items {
                w_name = w_name.max(item["name"].as_str().unwrap_or("").len());
                w_super = w_super.max(format_bool_field(item.get("is_superuser")).len());
                w_login = w_login.max(format_bool_field(item.get("can_login")).len());
            }

            println!(
                "{:<w_name$}  {:<w_super$}  {:<w_login$}",
                "USERNAME", "SUPERUSER", "CAN_LOGIN"
            );
            println!(
                "{}  {}  {}",
                "─".repeat(w_name),
                "─".repeat(w_super),
                "─".repeat(w_login)
            );
            for item in &items {
                println!(
                    "{:<w_name$}  {:<w_super$}  {:<w_login$}",
                    item["name"].as_str().unwrap_or(""),
                    format_bool_field(item.get("is_superuser")),
                    format_bool_field(item.get("can_login")),
                );
            }
            println!(
                "({} {})",
                items.len(),
                if items.len() == 1 { "row" } else { "rows" }
            );
        }
        Err((_status, detail)) => {
            eprintln!("ERROR: {detail}");
        }
    }
}

fn format_bool_field(v: Option<&Value>) -> String {
    match v {
        Some(Value::Bool(b)) => if *b { "yes" } else { "no" }.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "".to_string(),
    }
}

// ── \l — list databases ─────────────────────────────────────────

async fn handle_list_databases(api: &ApiClient) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    match api
        .try_request("GET", "/customer/databases", None, Some(&headers))
        .await
    {
        Ok(data) => {
            let items = data.as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                eprintln!("No databases found.");
                return;
            }

            let mut w_id = 2usize;
            let mut w_name = 4usize;
            let mut w_state = 5usize;
            let mut w_region = 6usize;

            for item in &items {
                w_id = w_id.max(item["id"].as_str().unwrap_or("").len());
                w_name = w_name.max(item["name"].as_str().unwrap_or("").len());
                w_state = w_state.max(item["state"].as_str().unwrap_or("").len());
                w_region = w_region.max(item["region"].as_str().unwrap_or("").len());
            }

            println!(
                "{:<w_id$}  {:<w_name$}  {:<w_state$}  {:<w_region$}",
                "ID", "NAME", "STATE", "REGION"
            );
            println!(
                "{}  {}  {}  {}",
                "─".repeat(w_id),
                "─".repeat(w_name),
                "─".repeat(w_state),
                "─".repeat(w_region)
            );
            for item in &items {
                println!(
                    "{:<w_id$}  {:<w_name$}  {:<w_state$}  {:<w_region$}",
                    item["id"].as_str().unwrap_or(""),
                    item["name"].as_str().unwrap_or(""),
                    item["state"].as_str().unwrap_or(""),
                    item["region"].as_str().unwrap_or(""),
                );
            }
            println!(
                "({} {})",
                items.len(),
                if items.len() == 1 {
                    "database"
                } else {
                    "databases"
                }
            );
        }
        Err((_status, detail)) => {
            eprintln!("ERROR: {detail}");
        }
    }
}

// ── \conninfo — show connection info ────────────────────────────

fn handle_conninfo(repl_state: &ReplState) {
    eprintln!("Connection information:");
    eprintln!("  Database:  {}", repl_state.db_name);
    eprintln!("  DB ID:     {}", repl_state.db_id);
    eprintln!("  API URL:   {}", repl_state.api_url);
    eprintln!(
        "  Mode:      {}",
        if repl_state.is_direct() {
            "direct (pgwire)"
        } else {
            "API"
        }
    );
    eprintln!(
        "  Expanded:  {}",
        match repl_state.expanded {
            ExpandedMode::Off => "off",
            ExpandedMode::On => "on",
            ExpandedMode::Auto => "auto",
        }
    );
    eprintln!(
        "  Pager:     {}",
        if repl_state.pager_enabled {
            "on"
        } else {
            "off"
        }
    );
    if let Some(ref f) = repl_state.output_file {
        eprintln!("  Output:    {}", f);
    }
}

// ── \i — execute SQL from file ──────────────────────────────────

async fn handle_include_file(
    api: &ApiClient,
    output: &OutputFormat,
    id: &str,
    timing: bool,
    repl_state: &ReplState,
    path: &str,
) {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let sql = contents.trim();
            if sql.is_empty() {
                eprintln!("File '{}' is empty.", path);
                return;
            }
            repl_exec(api, output, id, timing, repl_state, sql).await;
        }
        Err(err) => {
            eprintln!("ERROR: Failed to read '{}': {}", path, err);
        }
    }
}

// ── \o — redirect output ────────────────────────────────────────

fn handle_output_redirect(repl_state: &mut ReplState, arg: &str) {
    if arg.is_empty() {
        if repl_state.output_file.is_some() {
            repl_state.output_file = None;
            eprintln!("Output reset to stdout.");
        } else {
            eprintln!("Output is stdout.");
        }
    } else {
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(arg)
        {
            Ok(_) => {
                repl_state.output_file = Some(arg.to_string());
                eprintln!("Output redirected to '{}'.", arg);
            }
            Err(err) => {
                eprintln!("ERROR: Cannot open '{}': {}", arg, err);
            }
        }
    }
}

// ── \e — edit in $EDITOR ────────────────────────────────────────

fn handle_edit(repl_state: &ReplState) -> DispatchResult {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());

    let tmpdir = std::env::temp_dir();
    let tmpfile = tmpdir.join(format!("db9_edit_{}.sql", std::process::id()));

    let original = repl_state.last_query.as_deref().unwrap_or("");
    if let Err(e) = std::fs::write(&tmpfile, original) {
        eprintln!("ERROR: Failed to create temp file: {e}");
        return DispatchResult::Continue;
    }

    let status = Command::new(&editor).arg(&tmpfile).status();

    match status {
        Ok(s) if s.success() => match std::fs::read_to_string(&tmpfile) {
            Ok(contents) => {
                let _ = std::fs::remove_file(&tmpfile);
                let sql = contents.trim().to_string();
                if sql.is_empty() {
                    eprintln!("No query to execute (empty file).");
                    return DispatchResult::Continue;
                }
                if sql == original.trim() {
                    eprintln!("Query unchanged; not executed.");
                    return DispatchResult::Continue;
                }
                DispatchResult::ExecuteQuery(sql)
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmpfile);
                eprintln!("ERROR: Failed to read temp file: {e}");
                DispatchResult::Continue
            }
        },
        Ok(s) => {
            let _ = std::fs::remove_file(&tmpfile);
            eprintln!("Editor exited with status: {}", s);
            DispatchResult::Continue
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmpfile);
            eprintln!("ERROR: Failed to launch editor '{}': {}", editor, e);
            DispatchResult::Continue
        }
    }
}

// ── \c — switch database ────────────────────────────────────────

async fn handle_connect(api: &ApiClient, new_id: &str) -> DispatchResult {
    let token = require_token();
    let headers = make_auth_headers(&token);

    match api
        .try_request(
            "GET",
            &format!("/customer/databases/{new_id}"),
            None,
            Some(&headers),
        )
        .await
    {
        Ok(data) => {
            let new_name = data["name"].as_str().unwrap_or(new_id).to_string();
            let tables = fetch_table_names(api, new_id).await.unwrap_or_default();

            eprintln!(
                "You are now connected to database \"{}\" ({}).",
                new_name, new_id
            );

            DispatchResult::SwitchedDatabase {
                id: new_id.to_string(),
                name: new_name,
                tables,
            }
        }
        Err((_status, detail)) => {
            eprintln!("ERROR: {detail}");
            DispatchResult::Continue
        }
    }
}

// ── existing helpers ────────────────────────────────────────────

fn handle_pager_command(repl_state: &mut ReplState, arg: &str) {
    if arg.is_empty() {
        let status = if repl_state.pager_enabled { "on" } else { "off" };
        let cmd = repl_state
            .pager_command
            .as_ref()
            .map(|s| s.as_str())
            .unwrap_or("(default)");
        eprintln!("Pager is {} ({})", status, cmd);
    } else if arg.eq_ignore_ascii_case("on") {
        repl_state.pager_enabled = true;
        repl_state.pager_command = None;
        eprintln!("Pager enabled (using default)");
    } else if arg.eq_ignore_ascii_case("off") {
        repl_state.pager_enabled = false;
        eprintln!("Pager disabled");
    } else {
        repl_state.pager_enabled = true;
        repl_state.pager_command = Some(arg.to_string());
        eprintln!("Pager set to: {}", arg);
    }
}

fn handle_expanded_command(repl_state: &mut ReplState, arg: &str) {
    if arg.is_empty() {
        repl_state.expanded = match repl_state.expanded {
            ExpandedMode::Off => ExpandedMode::On,
            ExpandedMode::On => ExpandedMode::Off,
            ExpandedMode::Auto => ExpandedMode::Off,
        };
    } else if arg.eq_ignore_ascii_case("on") {
        repl_state.expanded = ExpandedMode::On;
    } else if arg.eq_ignore_ascii_case("off") {
        repl_state.expanded = ExpandedMode::Off;
    } else if arg.eq_ignore_ascii_case("auto") {
        repl_state.expanded = ExpandedMode::Auto;
    } else {
        eprintln!("Invalid expanded mode: {}. Use 'on', 'off', or 'auto'.", arg);
        return;
    }

    let status = match repl_state.expanded {
        ExpandedMode::Off => "off",
        ExpandedMode::On => "on",
        ExpandedMode::Auto => "auto",
    };
    eprintln!("Expanded display is {}.", status);
}

fn handle_pset_command(repl_state: &mut ReplState, arg: &str) {
    if arg.is_empty() {
        eprintln!("border     {}", repl_state.border);
        eprintln!("null       \"{}\"", repl_state.null_display);
        eprintln!(
            "format     {}",
            match repl_state.format_override {
                Some(OutputFormat::Json) => "json",
                Some(OutputFormat::Csv) => "csv",
                _ => "table",
            }
        );
        eprintln!(
            "linestyle  {}",
            match repl_state.linestyle {
                LinestyleMode::Ascii => "ascii",
                LinestyleMode::Unicode => "unicode",
            }
        );
        eprintln!(
            "expanded   {}",
            match repl_state.expanded {
                ExpandedMode::Off => "off",
                ExpandedMode::On => "on",
                ExpandedMode::Auto => "auto",
            }
        );
        eprintln!(
            "pager      {}",
            if repl_state.pager_enabled {
                "on"
            } else {
                "off"
            }
        );
        return;
    }

    let parts: Vec<&str> = arg.splitn(2, char::is_whitespace).collect();
    let option = parts[0];
    let value = parts.get(1).map(|s| s.trim()).unwrap_or("");

    match option {
        "border" => {
            if value.is_empty() {
                eprintln!("border     {}", repl_state.border);
                return;
            }
            match value.parse::<u8>() {
                Ok(b) if b <= 2 => {
                    repl_state.border = b;
                    eprintln!("Border style is {}.", b);
                }
                _ => {
                    eprintln!("Invalid border value: {}. Use 0, 1, or 2.", value);
                }
            }
        }
        "null" => {
            repl_state.null_display = value.to_string();
            eprintln!("Null display is \"{}\".", repl_state.null_display);
        }
        "format" => {
            if value.is_empty() {
                let label = match repl_state.format_override {
                    Some(OutputFormat::Json) => "json",
                    Some(OutputFormat::Csv) => "csv",
                    _ => "table",
                };
                eprintln!("format     {}", label);
                return;
            }
            match value.to_lowercase().as_str() {
                "table" | "aligned" => {
                    repl_state.format_override = Some(OutputFormat::Table);
                    eprintln!("Output format is table.");
                }
                "csv" => {
                    repl_state.format_override = Some(OutputFormat::Csv);
                    eprintln!("Output format is csv.");
                }
                "json" => {
                    repl_state.format_override = Some(OutputFormat::Json);
                    eprintln!("Output format is json.");
                }
                _ => {
                    eprintln!(
                        "Invalid format: {}. Use table, csv, or json.",
                        value
                    );
                }
            }
        }
        "linestyle" => {
            if value.is_empty() {
                let label = match repl_state.linestyle {
                    LinestyleMode::Ascii => "ascii",
                    LinestyleMode::Unicode => "unicode",
                };
                eprintln!("linestyle  {}", label);
                return;
            }
            match value.to_lowercase().as_str() {
                "ascii" => {
                    repl_state.linestyle = LinestyleMode::Ascii;
                    eprintln!("Line style is ascii.");
                }
                "unicode" => {
                    repl_state.linestyle = LinestyleMode::Unicode;
                    eprintln!("Line style is unicode.");
                }
                _ => {
                    eprintln!(
                        "Invalid linestyle: {}. Use ascii or unicode.",
                        value
                    );
                }
            }
        }
        "expanded" => {
            handle_expanded_command(repl_state, value);
        }
        "pager" => {
            handle_pager_command(repl_state, value);
        }
        _ => {
            eprintln!(
                "Unknown pset option: {}. Valid: border, null, format, linestyle, expanded, pager",
                option
            );
        }
    }
}

// ── \fs — save favorite query ────────────────────────────────────

fn handle_save_favorite(repl_state: &mut ReplState, arg: &str) {
    if arg.is_empty() {
        eprintln!("Usage: \\fs <name> [query]");
        eprintln!("  If no query given, saves last_query");
        return;
    }

    let (name, query) = if let Some(space_pos) = arg.find(char::is_whitespace) {
        let name = &arg[..space_pos];
        let query = arg[space_pos..].trim();
        (name, query.to_string())
    } else {
        let name = arg;
        match &repl_state.last_query {
            Some(q) => (name, q.clone()),
            None => {
                eprintln!("ERROR: No last query to save. Provide query explicitly: \\fs <name> <query>");
                return;
            }
        }
    };

    match repl_state.favorites.add(name, &query) {
        Ok(_) => eprintln!("Saved favorite: {}", name),
        Err(e) => eprintln!("ERROR: {}", e),
    }
}

// ── \f — execute favorite query ──────────────────────────────────

fn handle_execute_favorite(repl_state: &ReplState, name: &str) -> DispatchResult {
    match repl_state.favorites.get(name) {
        Some(query) => DispatchResult::ExecuteQuery(query),
        None => {
            eprintln!("ERROR: Favorite '{}' not found. Use \\fl to list.", name);
            DispatchResult::Continue
        }
    }
}

// ── \fd — delete favorite query ──────────────────────────────────

fn handle_delete_favorite(repl_state: &mut ReplState, name: &str) {
    match repl_state.favorites.delete(name) {
        Ok(true) => eprintln!("Deleted favorite: {}", name),
        Ok(false) => eprintln!("ERROR: Favorite '{}' not found.", name),
        Err(e) => eprintln!("ERROR: {}", e),
    }
}

// ── \fl — list all favorites ────────────────────────────────────

fn handle_list_favorites(repl_state: &ReplState) {
    let favorites = repl_state.favorites.list();
    if favorites.is_empty() {
        eprintln!("No saved favorites.");
        return;
    }

    let mut w_name = 4usize;
    for (name, _) in &favorites {
        w_name = w_name.max(name.len());
    }

    eprintln!("{:<w_name$}  QUERY", "NAME");
    eprintln!("{}  {}", "─".repeat(w_name), "─".repeat(40));

    for (name, query) in &favorites {
        let truncated = if query.len() > 40 {
            format!("{}...", &query[..37])
        } else {
            query.clone()
        };
        eprintln!("{:<w_name$}  {}", name, truncated);
    }

    eprintln!("({} {})", favorites.len(), if favorites.len() == 1 { "favorite" } else { "favorites" });
}

fn handle_highlight_command(arg: &str) -> DispatchResult {
    if arg.is_empty() || arg.eq_ignore_ascii_case("on") {
        eprintln!("Syntax highlighting is on.");
        DispatchResult::HighlightChanged(true)
    } else if arg.eq_ignore_ascii_case("off") {
        eprintln!("Syntax highlighting is off.");
        DispatchResult::HighlightChanged(false)
    } else {
        eprintln!("Usage: \\highlight [on|off]");
        DispatchResult::Continue
    }
}

// ── \! — execute shell command ──────────────────────────────

fn handle_shell_command(arg: &str) -> DispatchResult {
    if arg.is_empty() {
        // Bare \! — spawn interactive shell
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        match Command::new(&shell).status() {
            Ok(status) => {
                if !status.success() {
                    eprintln!("Shell exited with status: {}", status);
                }
            }
            Err(e) => {
                eprintln!("ERROR: Failed to launch shell '{}': {}", shell, e);
            }
        }
    } else {
        // \! <command> — run command via sh -c
        match Command::new("sh").arg("-c").arg(arg).status() {
            Ok(status) => {
                if !status.success() {
                    eprintln!("Command exited with status: {}", status);
                }
            }
            Err(e) => {
                eprintln!("ERROR: Failed to execute command: {}", e);
            }
        }
    }
    DispatchResult::Continue
}

// ── \watch — periodic query re-execution ────────────────────────

fn handle_watch_command(arg: &str) -> DispatchResult {
    if arg.is_empty() {
        return DispatchResult::Watch(2); // default 2 seconds
    }
    match arg.parse::<u64>() {
        Ok(0) => {
            eprintln!("Watch interval must be at least 1 second.");
            DispatchResult::Continue
        }
        Ok(secs) => DispatchResult::Watch(secs),
        Err(_) => {
            eprintln!("Invalid interval: '{}'. Usage: \\watch [seconds]", arg);
            DispatchResult::Continue
        }
    }
}

fn repl_help() {
    eprintln!("Meta-commands:");
    eprintln!("  \\d [TABLE]    Describe table columns, or list all tables");
    eprintln!("  \\dt           List tables");
    eprintln!("  \\dv           List views");
    eprintln!("  \\dn           List schemas");
    eprintln!("  \\di           List indexes");
    eprintln!("  \\du           List database users");
    eprintln!("  \\l            List all databases");
    eprintln!("  \\c <ID>       Switch to a different database");
    eprintln!("  \\conninfo     Show connection info");
    eprintln!("  \\i <FILE>     Execute SQL from a file");
    eprintln!("  \\o [FILE]     Redirect output to file (no arg = reset to stdout)");
    eprintln!("  \\e            Edit last query in $EDITOR and execute");
    eprintln!("  \\! [COMMAND]  Execute shell command, or start interactive shell");
    eprintln!("  \\refresh      Refresh SQL completion table cache");
    eprintln!("  \\timing       Toggle query timing");
    eprintln!("  \\pset [OPT]   Set output option (border/null/format/linestyle/expanded/pager)");
    eprintln!("  \\pager [CMD]  Control paging (on/off/CMD)");
    eprintln!("  \\x [MODE]     Toggle expanded display (on/off/auto)");
    eprintln!("  \\highlight    Toggle SQL syntax highlighting (on/off)");
    eprintln!("  \\watch [N]    Re-execute last query every N seconds (default 2)");
    eprintln!("  \\g            Execute query (like ;), or re-execute last query");
    eprintln!("  \\gx           Execute query in expanded mode");
    eprintln!("  \\fs <N> [Q]   Save favorite query (Q defaults to last query)");
    eprintln!("  \\f <NAME>     Execute saved favorite query");
    eprintln!("  \\fd <NAME>    Delete saved favorite query");
    eprintln!("  \\fl           List all saved favorite queries");
    eprintln!("  \\q            Quit");
    eprintln!("  \\?            Show this help");
    eprintln!();
    eprintln!("Enter SQL terminated by semicolon (;), \\g, or \\gx to execute.");
    eprintln!("Multi-line input is supported.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::SqlExecutor;

    #[test]
    fn test_extract_table_names_normal() {
        let data = serde_json::json!({
            "columns": [{"name": "table_name"}],
            "rows": [["users"], ["orders"], ["products"]]
        });
        let names = extract_table_names(&data);
        assert_eq!(names, vec!["users", "orders", "products"]);
    }

    #[test]
    fn test_extract_table_names_empty() {
        let data = serde_json::json!({
            "columns": [{"name": "table_name"}],
            "rows": []
        });
        assert!(extract_table_names(&data).is_empty());
    }

    #[test]
    fn test_extract_table_names_no_rows_key() {
        let data = serde_json::json!({"columns": []});
        assert!(extract_table_names(&data).is_empty());
    }

    #[test]
    fn test_format_bool_field_true() {
        assert_eq!(format_bool_field(Some(&Value::Bool(true))), "yes");
    }

    #[test]
    fn test_format_bool_field_false() {
        assert_eq!(format_bool_field(Some(&Value::Bool(false))), "no");
    }

    #[test]
    fn test_format_bool_field_string() {
        assert_eq!(format_bool_field(Some(&Value::String("custom".into()))), "custom");
    }

    #[test]
    fn test_format_bool_field_none() {
        assert_eq!(format_bool_field(None), "");
    }

    #[test]
    fn test_handle_expanded_toggle() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        assert_eq!(state.expanded, ExpandedMode::Off);
        handle_expanded_command(&mut state, "");
        assert_eq!(state.expanded, ExpandedMode::On);
        handle_expanded_command(&mut state, "");
        assert_eq!(state.expanded, ExpandedMode::Off);
    }

    #[test]
    fn test_handle_expanded_explicit() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        handle_expanded_command(&mut state, "on");
        assert_eq!(state.expanded, ExpandedMode::On);
        handle_expanded_command(&mut state, "auto");
        assert_eq!(state.expanded, ExpandedMode::Auto);
        handle_expanded_command(&mut state, "off");
        assert_eq!(state.expanded, ExpandedMode::Off);
    }

    #[test]
    fn test_handle_expanded_invalid() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        handle_expanded_command(&mut state, "invalid");
        assert_eq!(state.expanded, ExpandedMode::Off);
    }

    #[test]
    fn test_handle_pager_toggle() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        assert!(state.pager_enabled);
        handle_pager_command(&mut state, "off");
        assert!(!state.pager_enabled);
        handle_pager_command(&mut state, "on");
        assert!(state.pager_enabled);
        assert!(state.pager_command.is_none());
    }

    #[test]
    fn test_handle_pager_custom_command() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        handle_pager_command(&mut state, "more");
        assert!(state.pager_enabled);
        assert_eq!(state.pager_command, Some("more".to_string()));
    }

    #[test]
    fn test_handle_highlight_command_on() {
        match handle_highlight_command("on") {
            DispatchResult::HighlightChanged(true) => {}
            _ => panic!("Expected HighlightChanged(true)"),
        }
    }

    #[test]
    fn test_handle_highlight_command_off() {
        match handle_highlight_command("off") {
            DispatchResult::HighlightChanged(false) => {}
            _ => panic!("Expected HighlightChanged(false)"),
        }
    }

    #[test]
    fn test_handle_highlight_command_default_on() {
        match handle_highlight_command("") {
            DispatchResult::HighlightChanged(true) => {}
            _ => panic!("Expected HighlightChanged(true) for empty arg"),
        }
    }

    #[test]
    fn test_handle_highlight_command_invalid() {
        match handle_highlight_command("maybe") {
            DispatchResult::Continue => {}
            _ => panic!("Expected Continue for invalid arg"),
        }
    }

    #[test]
    fn test_handle_save_favorite_with_query() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        handle_save_favorite(&mut state, "myq SELECT 1");
        assert_eq!(state.favorites.get("myq"), Some("SELECT 1".to_string()));
    }

    #[test]
    fn test_handle_save_favorite_from_last_query() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        state.last_query = Some("SELECT 42".to_string());
        handle_save_favorite(&mut state, "last");
        assert_eq!(state.favorites.get("last"), Some("SELECT 42".to_string()));
    }

    #[test]
    fn test_handle_save_favorite_no_last_query() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        handle_save_favorite(&mut state, "name_only");
        assert_eq!(state.favorites.get("name_only"), None);
    }

    #[test]
    fn test_handle_execute_favorite_found() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        state.favorites.add("run_me", "SELECT 99").unwrap();
        match handle_execute_favorite(&state, "run_me") {
            DispatchResult::ExecuteQuery(q) => assert_eq!(q, "SELECT 99"),
            _ => panic!("Expected ExecuteQuery"),
        }
    }

    #[test]
    fn test_handle_execute_favorite_not_found() {
        let state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        match handle_execute_favorite(&state, "nonexistent") {
            DispatchResult::Continue => {}
            _ => panic!("Expected Continue for missing favorite"),
        }
    }

    #[test]
    fn test_handle_delete_favorite() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        state.favorites.add("del_me", "SELECT 1").unwrap();
        handle_delete_favorite(&mut state, "del_me");
        assert_eq!(state.favorites.get("del_me"), None);
    }

    #[test]
    fn test_handle_output_redirect_set_and_reset() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        assert!(state.output_file.is_none());
        handle_output_redirect(&mut state, "/tmp/db9_test_output.txt");
        assert_eq!(state.output_file, Some("/tmp/db9_test_output.txt".to_string()));
        handle_output_redirect(&mut state, "");
        assert!(state.output_file.is_none());
        let _ = std::fs::remove_file("/tmp/db9_test_output.txt");
    }

    #[test]
    fn test_handle_shell_command_returns_continue() {
        match handle_shell_command("echo test") {
            DispatchResult::Continue => {}
            _ => panic!("Expected Continue"),
        }
    }

    #[test]
    fn test_handle_shell_command_bare_returns_continue() {
        match handle_shell_command("") {
            DispatchResult::Continue => {}
            _ => panic!("Expected Continue for bare \\!"),
        }
    }

    #[test]
    fn test_repl_help_includes_shell_command() {
        match handle_shell_command("true") {
            DispatchResult::Continue => {}
            _ => panic!("Expected Continue"),
        }
    }

    #[test]
    fn test_handle_watch_default_interval() {
        match handle_watch_command("") {
            DispatchResult::Watch(2) => {}
            _ => panic!("Expected Watch(2) for empty arg"),
        }
    }

    #[test]
    fn test_handle_watch_custom_interval() {
        match handle_watch_command("5") {
            DispatchResult::Watch(5) => {}
            _ => panic!("Expected Watch(5)"),
        }
    }

    #[test]
    fn test_handle_watch_zero_rejected() {
        match handle_watch_command("0") {
            DispatchResult::Continue => {}
            _ => panic!("Expected Continue for zero interval"),
        }
    }

    #[test]
    fn test_handle_watch_invalid_rejected() {
        match handle_watch_command("abc") {
            DispatchResult::Continue => {}
            _ => panic!("Expected Continue for non-numeric arg"),
        }
    }

    #[test]
    fn test_handle_watch_large_interval() {
        match handle_watch_command("60") {
            DispatchResult::Watch(60) => {}
            _ => panic!("Expected Watch(60)"),
        }
    }

    #[test]
    fn test_handle_pset_border_valid() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        assert_eq!(state.border, 1);
        handle_pset_command(&mut state, "border 0");
        assert_eq!(state.border, 0);
        handle_pset_command(&mut state, "border 1");
        assert_eq!(state.border, 1);
        handle_pset_command(&mut state, "border 2");
        assert_eq!(state.border, 2);
    }

    #[test]
    fn test_handle_pset_border_invalid() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        handle_pset_command(&mut state, "border 5");
        assert_eq!(state.border, 1);
        handle_pset_command(&mut state, "border abc");
        assert_eq!(state.border, 1);
    }

    #[test]
    fn test_handle_pset_null_custom() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        assert_eq!(state.null_display, "NULL");
        handle_pset_command(&mut state, "null (empty)");
        assert_eq!(state.null_display, "(empty)");
        handle_pset_command(&mut state, "null ");
        assert_eq!(state.null_display, "");
    }

    #[test]
    fn test_handle_pset_format() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        assert!(state.format_override.is_none());
        handle_pset_command(&mut state, "format json");
        assert_eq!(state.format_override, Some(OutputFormat::Json));
        handle_pset_command(&mut state, "format csv");
        assert_eq!(state.format_override, Some(OutputFormat::Csv));
        handle_pset_command(&mut state, "format table");
        assert_eq!(state.format_override, Some(OutputFormat::Table));
    }

    #[test]
    fn test_handle_pset_linestyle() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        assert_eq!(state.linestyle, LinestyleMode::Ascii);
        handle_pset_command(&mut state, "linestyle unicode");
        assert_eq!(state.linestyle, LinestyleMode::Unicode);
        handle_pset_command(&mut state, "linestyle ascii");
        assert_eq!(state.linestyle, LinestyleMode::Ascii);
    }

    #[test]
    fn test_handle_pset_unknown_option() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        let before_border = state.border;
        handle_pset_command(&mut state, "nosuchoption value");
        assert_eq!(state.border, before_border);
    }

    #[test]
    fn test_handle_pset_expanded_delegates() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        assert_eq!(state.expanded, ExpandedMode::Off);
        handle_pset_command(&mut state, "expanded on");
        assert_eq!(state.expanded, ExpandedMode::On);
        handle_pset_command(&mut state, "expanded auto");
        assert_eq!(state.expanded, ExpandedMode::Auto);
    }

    #[test]
    fn test_handle_pset_pager_delegates() {
        let mut state = ReplState::new("id".into(), "db".into(), "http://x".into(), SqlExecutor::Api);
        assert!(state.pager_enabled);
        handle_pset_command(&mut state, "pager off");
        assert!(!state.pager_enabled);
        handle_pset_command(&mut state, "pager on");
        assert!(state.pager_enabled);
    }
}
