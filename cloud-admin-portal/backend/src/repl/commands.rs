use pgtikv_admin::cli_common::ApiClient;

use crate::OutputFormat;

use super::{exec::repl_exec, ReplState};

pub async fn dispatch(
    api: &ApiClient,
    output: &OutputFormat,
    id: &str,
    show_timing: &mut bool,
    repl_state: &mut ReplState,
    trimmed: &str,
) -> bool {
    let (cmd, arg) = match trimmed.find(char::is_whitespace) {
        Some(pos) => (&trimmed[..pos], trimmed[pos..].trim()),
        None => (trimmed, ""),
    };

    match cmd {
        "\\q" | "\\quit" => true,
        "\\?" | "\\help" => {
            repl_help();
            false
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
            false
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
            false
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
            false
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
            false
        }
        "\\timing" => {
            *show_timing = !*show_timing;
            eprintln!("Timing is {}.", if *show_timing { "on" } else { "off" });
            false
        }
        "\\pager" => {
            handle_pager_command(repl_state, arg);
            false
        }
        _ => {
            eprintln!("Unknown command: {cmd}. Type \\? for help.");
            false
        }
    }
}

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

fn repl_help() {
    eprintln!("Meta-commands:");
    eprintln!("  \\d [TABLE]    Describe table columns, or list all tables");
    eprintln!("  \\dt           List tables");
    eprintln!("  \\dn           List schemas");
    eprintln!("  \\di           List indexes");
    eprintln!("  \\timing       Toggle query timing");
    eprintln!("  \\pager [CMD]  Control paging (on/off/CMD)");
    eprintln!("  \\q            Quit");
    eprintln!("  \\?            Show this help");
    eprintln!();
    eprintln!("Enter SQL terminated by semicolon (;) to execute.");
    eprintln!("Multi-line input is supported.");
}
