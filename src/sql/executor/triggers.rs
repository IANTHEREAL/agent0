use super::super::names;
use super::super::plpgsql;
use super::super::{ExecuteResult, Session};
use super::core::Executor;
use crate::model::{FunctionDef, TriggerDef};
use crate::sql::error::SqlError;
use crate::sql::scanner::SqlCharScanner;
use anyhow::{anyhow, Result};
use sqlparser::ast::ObjectName;

fn object_name_from_token(token: &str) -> Result<ObjectName> {
    let token = token.trim().trim_end_matches(';');
    if token.is_empty() {
        return Err(anyhow!("Missing object name"));
    }
    let parts: Vec<&str> = token.split('.').collect();
    match parts.as_slice() {
        [name] if !name.is_empty() => Ok(ObjectName(vec![sqlparser::ast::Ident::new(*name)])),
        [schema, name] if !schema.is_empty() && !name.is_empty() => Ok(ObjectName(vec![
            sqlparser::ast::Ident::new(*schema),
            sqlparser::ast::Ident::new(*name),
        ])),
        _ => Err(anyhow!("Invalid object name '{}'", token)),
    }
}

use super::core::starts_with_ignore_ascii_case;

pub(crate) fn strip_leading_sql_comments(sql: &str) -> &str {
    let mut s = sql;
    loop {
        s = s.trim_start();
        if s.starts_with("--") {
            if let Some(pos) = s.find('\n') {
                s = &s[pos + 1..];
                continue;
            }
            return "";
        }
        if s.starts_with("/*") {
            if let Some(pos) = s.find("*/") {
                s = &s[pos + 2..];
                continue;
            }
            return "";
        }
        return s;
    }
}

fn is_ident_char(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
}

fn find_keyword_outside_quotes_and_dollar(haystack: &str, keyword: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let kw = keyword.as_bytes();
    if kw.is_empty() || bytes.len() < kw.len() {
        return None;
    }

    for ctx in SqlCharScanner::new(haystack) {
        if ctx.in_string() || ctx.in_comment() {
            continue;
        }
        let i = ctx.pos;
        if i + kw.len() <= bytes.len() {
            let before_ok = i == 0 || !is_ident_char(bytes[i - 1]);
            let after_ok = i + kw.len() == bytes.len() || !is_ident_char(bytes[i + kw.len()]);
            if before_ok && after_ok && bytes[i..i + kw.len()].eq_ignore_ascii_case(kw) {
                return Some(i);
            }
        }
    }

    None
}

fn find_matching_paren(s: &str, open_pos: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    if open_pos >= bytes.len() || bytes[open_pos] != b'(' {
        return None;
    }

    // Scanner starts at open_pos. The '(' there increments paren_depth to 1.
    // We look for ')' that brings it back to 0.
    let mut saw_open = false;
    for ctx in SqlCharScanner::with_offset(s, open_pos) {
        if ctx.in_string() || ctx.in_comment() {
            saw_open = true; // past the opening paren
            continue;
        }
        if ctx.byte == b'(' && !saw_open {
            saw_open = true;
            continue;
        }
        // ')' is emitted at pre-decrement depth; when outer ')' closes the
        // opening '(' at depth 1, the emitted paren_depth == 1.
        if ctx.byte == b')' && ctx.paren_depth == 1 {
            return Some(ctx.pos);
        }
    }
    None
}

fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;

    for ctx in SqlCharScanner::new(s) {
        if ctx.is_code() && ctx.paren_depth == 0 && ctx.byte == b',' {
            parts.push(&s[start..ctx.pos]);
            start = ctx.pos + 1;
        }
    }
    parts.push(&s[start..]);
    parts
}

fn strip_default_clause(arg: &str) -> &str {
    for ctx in SqlCharScanner::new(arg) {
        if ctx.is_code() && ctx.paren_depth == 0 && ctx.byte == b'=' {
            return arg[..ctx.pos].trim_end();
        }
    }

    if let Some(pos) = find_keyword_outside_quotes_and_dollar(arg, "DEFAULT") {
        return arg[..pos].trim_end();
    }
    arg
}

fn parse_arg_types(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let args = args.trim();
    if args.is_empty() {
        return out;
    }

    for raw in split_top_level_commas(args) {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }

        let raw = strip_default_clause(raw).trim();
        if raw.is_empty() {
            continue;
        }

        let mut tokens = raw
            .split_whitespace()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>();

        while matches!(
            tokens.first().map(|t| t.to_ascii_uppercase()).as_deref(),
            Some("IN") | Some("OUT") | Some("INOUT") | Some("VARIADIC")
        ) {
            tokens.remove(0);
        }

        if tokens.is_empty() {
            continue;
        }

        let arg_str = tokens.join(" ");
        out.push(arg_str.to_lowercase());
    }

    out
}

fn consume_keyword_token_ci<'a>(s: &'a str, keyword: &str) -> Option<&'a str> {
    if s.len() < keyword.len() || !s[..keyword.len()].eq_ignore_ascii_case(keyword) {
        return None;
    }
    let next = s[keyword.len()..].chars().next();
    if matches!(next, Some(ch) if !ch.is_ascii_whitespace()) {
        return None;
    }
    Some(&s[keyword.len()..])
}

fn parse_sql_string_or_dollar_literal_with_tail(s: &str) -> Result<(String, &str)> {
    let s = s.trim_start();
    if s.starts_with('$') {
        let bytes = s.as_bytes();
        let Some(end) = bytes[1..].iter().position(|&c| c == b'$') else {
            return Err(anyhow!("Invalid dollar-quoted string"));
        };
        let delim_end = 1 + end;
        let tag = &bytes[1..delim_end];
        if !tag.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_') {
            return Err(anyhow!("Invalid dollar-quote tag"));
        }
        let delim = &s[..=delim_end];
        let body_start = delim_end + 1;
        let Some(close_rel) = s[body_start..].find(delim) else {
            return Err(anyhow!("Unterminated dollar-quoted string"));
        };
        let body_end = body_start + close_rel;
        let tail_start = body_end + delim.len();
        return Ok((s[body_start..body_end].to_string(), &s[tail_start..]));
    }

    if s.starts_with('\'') {
        let bytes = s.as_bytes();
        let mut out = String::new();
        let mut i = 1usize;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    out.push('\'');
                    i += 2;
                    continue;
                }
                return Ok((out, &s[i + 1..]));
            }
            out.push(b as char);
            i += 1;
        }
        return Err(anyhow!("Unterminated string literal"));
    }

    Err(anyhow!("Expected string literal after AS"))
}

fn parse_sql_string_or_dollar_literal(s: &str) -> Result<String> {
    parse_sql_string_or_dollar_literal_with_tail(s).map(|(body, _tail)| body)
}

fn parse_create_function_sql(sql: &str) -> Result<(ObjectName, FunctionDef, bool)> {
    let sql = strip_leading_sql_comments(sql).trim();
    let sql = sql.trim_end_matches(';').trim_end();

    let (or_replace, rest) = if starts_with_ignore_ascii_case(sql, "create or replace function") {
        (true, sql["create or replace function".len()..].trim_start())
    } else if starts_with_ignore_ascii_case(sql, "create function") {
        (false, sql["create function".len()..].trim_start())
    } else {
        return Err(anyhow!("Invalid CREATE FUNCTION syntax"));
    };

    let open_paren = rest
        .find('(')
        .ok_or_else(|| anyhow!("CREATE FUNCTION requires argument list"))?;
    let close_paren = find_matching_paren(rest, open_paren)
        .ok_or_else(|| anyhow!("Unterminated function argument list"))?;

    let name_token = rest[..open_paren].trim();
    let name = object_name_from_token(name_token)?;

    let args_str = rest[open_paren + 1..close_paren].trim();
    let after_args = rest[close_paren + 1..].trim_start();

    let returns_pos = find_keyword_outside_quotes_and_dollar(after_args, "RETURNS")
        .ok_or_else(|| anyhow!("CREATE FUNCTION requires RETURNS"))?;
    let after_returns = after_args[returns_pos + "RETURNS".len()..].trim_start();

    let language_pos = find_keyword_outside_quotes_and_dollar(after_returns, "LANGUAGE");
    let as_pos = find_keyword_outside_quotes_and_dollar(after_returns, "AS");
    let type_end = match (language_pos, as_pos) {
        (Some(l), Some(a)) => l.min(a),
        (Some(l), None) => l,
        (None, Some(a)) => a,
        (None, None) => return Err(anyhow!("CREATE FUNCTION requires LANGUAGE and AS")),
    };

    let return_type = after_returns[..type_end].trim().to_lowercase();

    let language_pos = language_pos.ok_or_else(|| anyhow!("CREATE FUNCTION requires LANGUAGE"))?;
    let lang_tail = after_returns[language_pos + "LANGUAGE".len()..].trim_start();
    let language = lang_tail
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("Missing LANGUAGE name"))?
        .trim_matches('\'')
        .to_lowercase();

    let as_pos = as_pos.ok_or_else(|| anyhow!("CREATE FUNCTION requires AS"))?;
    let body_tail = after_returns[as_pos + "AS".len()..].trim_start();
    let body = parse_sql_string_or_dollar_literal(body_tail)?;

    let def = FunctionDef {
        oid: 0,
        schema: String::new(),
        name: String::new(),
        arg_types: parse_arg_types(args_str),
        return_type,
        language,
        body,
        owner: "postgres".to_string(),
    };

    Ok((name, def, or_replace))
}

/// Parsed result of a `DROP FUNCTION` statement.
struct DropFunctionParsed {
    if_exists: bool,
    cascade: bool,
    names: Vec<ObjectName>,
}

fn parse_drop_function_sql(sql: &str) -> Result<DropFunctionParsed> {
    let sql = strip_leading_sql_comments(sql).trim();
    let sql = sql.trim_end_matches(';').trim_end();

    let rest = if starts_with_ignore_ascii_case(sql, "drop function") {
        sql["drop function".len()..].trim_start()
    } else {
        return Err(anyhow!("Invalid DROP FUNCTION syntax"));
    };

    let if_exists = starts_with_ignore_ascii_case(rest, "if exists");
    let rest = if if_exists {
        rest["if exists".len()..].trim_start()
    } else {
        rest
    };

    let rest = rest.split_whitespace().collect::<Vec<_>>().join(" ");
    let rest_upper = rest.to_ascii_uppercase();
    let mut cascade = false;
    let rest = if let Some(pos) = rest_upper.rfind(" CASCADE") {
        cascade = true;
        rest[..pos].trim_end()
    } else if let Some(pos) = rest_upper.rfind(" RESTRICT") {
        rest[..pos].trim_end()
    } else {
        rest.as_str()
    };

    let mut names = Vec::new();
    for entry in split_top_level_commas(rest) {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let name_part = entry.split('(').next().unwrap_or(entry).trim();
        let name_token = name_part
            .split_whitespace()
            .next()
            .ok_or_else(|| anyhow!("Missing function name"))?;
        names.push(object_name_from_token(name_token)?);
    }

    Ok(DropFunctionParsed {
        if_exists,
        cascade,
        names,
    })
}

fn parse_create_trigger_sql(
    sql: &str,
) -> Result<(String, String, Vec<String>, ObjectName, ObjectName)> {
    let sql = strip_leading_sql_comments(sql).trim();
    let sql = sql.trim_end_matches(';').trim_end();

    let rest = if starts_with_ignore_ascii_case(sql, "create trigger") {
        sql["create trigger".len()..].trim_start()
    } else if starts_with_ignore_ascii_case(sql, "create constraint trigger") {
        sql["create constraint trigger".len()..].trim_start()
    } else {
        return Err(anyhow!("Invalid CREATE TRIGGER syntax"));
    };

    let mut iter = rest.split_whitespace();
    let trigger_name = iter
        .next()
        .ok_or_else(|| anyhow!("Missing trigger name"))?
        .trim_end_matches(';')
        .to_lowercase();

    let timing_token = iter
        .next()
        .ok_or_else(|| anyhow!("Missing trigger timing"))?;
    let timing = if timing_token.eq_ignore_ascii_case("instead") {
        let next = iter
            .next()
            .ok_or_else(|| anyhow!("Invalid trigger timing"))?;
        if !next.eq_ignore_ascii_case("of") {
            return Err(anyhow!("Invalid trigger timing"));
        }
        "INSTEAD OF".to_string()
    } else {
        timing_token.to_ascii_uppercase()
    };

    // Collect events until ON
    let mut events = Vec::new();
    loop {
        let tok = iter
            .next()
            .ok_or_else(|| anyhow!("Missing ON clause in CREATE TRIGGER"))?;
        if tok.eq_ignore_ascii_case("on") {
            break;
        }
        if tok.eq_ignore_ascii_case("or") {
            continue;
        }
        events.push(tok.to_ascii_uppercase());
    }

    let table_token = iter
        .next()
        .ok_or_else(|| anyhow!("Missing table name in CREATE TRIGGER"))?;
    let table = object_name_from_token(table_token)?;

    // Find EXECUTE ... <func>
    let mut func_token = None;
    while let Some(tok) = iter.next() {
        if tok.eq_ignore_ascii_case("execute") {
            // Optional FUNCTION / PROCEDURE
            if let Some(next) = iter.next() {
                let next_upper = next.to_ascii_uppercase();
                if next_upper == "FUNCTION" || next_upper == "PROCEDURE" {
                    func_token = iter.next().map(|s| s.to_string());
                } else {
                    func_token = Some(next.to_string());
                }
            }
            break;
        }
    }

    let func_token =
        func_token.ok_or_else(|| anyhow!("Missing EXECUTE clause in CREATE TRIGGER"))?;
    let func_name_token = func_token
        .trim_end_matches(';')
        .trim_end_matches(',')
        .split('(')
        .next()
        .unwrap_or(func_token.as_str())
        .trim();
    let function = object_name_from_token(func_name_token)?;

    Ok((trigger_name, timing, events, table, function))
}

fn parse_drop_trigger_sql(sql: &str) -> Result<(bool, String, ObjectName)> {
    let sql = strip_leading_sql_comments(sql).trim();
    let sql = sql.trim_end_matches(';').trim_end();

    let rest = if starts_with_ignore_ascii_case(sql, "drop trigger") {
        sql["drop trigger".len()..].trim_start()
    } else {
        return Err(anyhow!("Invalid DROP TRIGGER syntax"));
    };

    let if_exists = starts_with_ignore_ascii_case(rest, "if exists");
    let rest = if if_exists {
        rest["if exists".len()..].trim_start()
    } else {
        rest
    };

    let mut iter = rest.split_whitespace();
    let trigger_name = iter
        .next()
        .ok_or_else(|| anyhow!("Missing trigger name"))?
        .trim_end_matches(';')
        .to_lowercase();

    let on_tok = iter
        .next()
        .ok_or_else(|| anyhow!("DROP TRIGGER requires ON <table>"))?;
    if !on_tok.eq_ignore_ascii_case("on") {
        return Err(anyhow!("DROP TRIGGER requires ON <table>"));
    }

    let table_token = iter
        .next()
        .ok_or_else(|| anyhow!("Missing table name in DROP TRIGGER"))?;
    let table = object_name_from_token(table_token)?;

    Ok((if_exists, trigger_name, table))
}

/// Extract the PL/pgSQL body from a `DO` statement.
///
/// Accepts:
/// - `DO $$ ... $$`
/// - `DO $tag$ ... $tag$`
/// - `DO LANGUAGE plpgsql $$ ... $$`
/// - `DO LANGUAGE plpgsql $tag$ ... $tag$`
fn parse_do_block_body(sql: &str) -> Result<String> {
    let sql = strip_leading_sql_comments(sql).trim();
    let sql = sql.trim_end_matches(';').trim_end();

    if !starts_with_ignore_ascii_case(sql, "DO") {
        return Err(anyhow!("Invalid DO syntax"));
    }
    let after_do = sql[2..].trim_start();

    // Optional: LANGUAGE plpgsql (default if omitted)
    let body_start = if let Some(after_language_kw) = consume_keyword_token_ci(after_do, "LANGUAGE")
    {
        let rest = after_language_kw.trim_start();
        let lang = rest
            .split(|c: char| c.is_ascii_whitespace() || c == '$')
            .next()
            .unwrap_or("");
        if !lang.eq_ignore_ascii_case("plpgsql") {
            return Err(anyhow!(
                "DO: only LANGUAGE plpgsql is supported, got '{}'",
                lang
            ));
        }
        rest[lang.len()..].trim_start()
    } else {
        after_do
    };

    let (body, tail) = parse_sql_string_or_dollar_literal_with_tail(body_start)?;
    if !tail.trim().is_empty() {
        return Err(anyhow!("DO: unexpected tokens after block body"));
    }
    Ok(body)
}

/// Runs `$body` (a `Result<T>` expression, typically `async { … }.await`) inside an
/// autocommit transaction when no explicit transaction is currently active.
/// If the session already has an active transaction the body runs as-is and the
/// surrounding transaction state is left untouched.
macro_rules! autocommit_ddl {
    ($session:expr, $body:expr) => {{
        let is_autocommit = !$session.is_in_transaction();
        if is_autocommit {
            $session.begin().await?;
        }
        let result = $body;
        if is_autocommit {
            if result.is_ok() {
                $session.commit().await?;
            } else {
                $session.rollback().await?;
            }
        }
        result
    }};
}

impl Executor {
    pub(crate) async fn execute_do_block_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let body = parse_do_block_body(sql)?;
        plpgsql::validate_plpgsql_body(&body)?;

        let func_def = FunctionDef {
            oid: 0,
            schema: String::new(),
            name: "<DO block>".to_string(),
            arg_types: vec![],
            return_type: "void".to_string(),
            language: "plpgsql".to_string(),
            body,
            owner: session.current_user().unwrap_or("postgres").to_string(),
        };

        autocommit_ddl!(
            session,
            async {
                let db_id = session.current_database_id();
                let (txn, sequence_values, search_path) = session
                    .get_mut_txn_sequence_values_and_search_path()
                    .expect("Transaction must be active");

                plpgsql::execute_plpgsql_function(
                    &self.store(),
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &func_def,
                    vec![],
                    Some(self),
                )
                .await?;

                Ok(ExecuteResult::CommandComplete { tag: "DO" })
            }
            .await
        )
    }

    pub(crate) async fn execute_create_function_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let (name, mut def, or_replace) = parse_create_function_sql(sql)?;

        if def.language.to_lowercase() == "plpgsql" {
            plpgsql::validate_plpgsql_body(&def.body)?;
        }

        autocommit_ddl!(
            session,
            async {
                let db_id = session.current_database_id();
                let (txn, _sequence_values, search_path) = session
                    .get_mut_txn_sequence_values_and_search_path()
                    .expect("Transaction must be active");
                let resolved = names::resolve_ddl_object_name(&name, search_path)?;
                if !self
                    .store()
                    .schema_exists(txn, db_id, &resolved.schema)
                    .await?
                {
                    return Err(anyhow!("schema '{}' does not exist", resolved.schema));
                }
                def.schema = resolved.schema.clone();
                def.name = resolved.name.clone();

                if or_replace {
                    self.store().replace_function(txn, db_id, def).await?;
                    self.trigger_cache().invalidate_db(db_id);
                } else {
                    self.store().create_function(txn, db_id, def).await?;
                }
                Ok(ExecuteResult::CreateFunction {
                    func_name: resolved.full,
                })
            }
            .await
        )
    }

    pub(crate) async fn execute_drop_function_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let DropFunctionParsed {
            if_exists,
            cascade,
            names,
        } = parse_drop_function_sql(sql)?;

        autocommit_ddl!(
            session,
            async {
                let db_id = session.current_database_id();
                let (txn, _sequence_values, search_path) = session
                    .get_mut_txn_sequence_values_and_search_path()
                    .expect("Transaction must be active");

                let mut last_name = None;
                let mut any_dropped = false;
                for name in names {
                    let resolved = names::resolve_existing_function_name(
                        self.store().as_ref(),
                        txn,
                        db_id,
                        &name,
                        search_path,
                    )
                    .await?;
                    let func_full_name = match resolved {
                        Some(resolved) => resolved.full,
                        None => names::resolve_ddl_object_name(&name, search_path)?.full,
                    };
                    last_name = Some(func_full_name.clone());
                    let dropped = self
                        .store()
                        .drop_function(txn, db_id, &func_full_name, cascade)
                        .await?;
                    if !dropped && !if_exists {
                        let bare = func_full_name.rsplit('.').next().unwrap_or(&func_full_name);
                        return Err(anyhow!("function {}() does not exist", bare));
                    }
                    any_dropped |= dropped;
                }

                if any_dropped {
                    self.trigger_cache().invalidate_db(db_id);
                }

                Ok(ExecuteResult::DropFunction {
                    func_name: last_name.unwrap_or_else(|| "unknown".to_string()),
                })
            }
            .await
        )
    }

    pub(crate) async fn execute_create_trigger_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let (trigger_name, timing, events, table, function) = parse_create_trigger_sql(sql)?;

        autocommit_ddl!(
            session,
            async {
                let db_id = session.current_database_id();
                let (txn, _sequence_values, search_path) = session
                    .get_mut_txn_sequence_values_and_search_path()
                    .expect("Transaction must be active");

                let table_resolved = names::resolve_existing_table_name(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    &table,
                    search_path,
                )
                .await?
                .ok_or_else(|| SqlError::RelationNotFound(table.to_string()))?;

                let func_resolved = names::resolve_existing_function_name(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    &function,
                    search_path,
                )
                .await?;
                let func_full_name = match func_resolved {
                    Some(resolved) => resolved.full,
                    None => {
                        let (_schema, bare_name) = names::split_object_name(&function)?;
                        return Err(anyhow!("function {}() does not exist", bare_name));
                    }
                };

                let def = TriggerDef {
                    oid: 0,
                    schema: table_resolved.schema.clone(),
                    name: trigger_name.clone(),
                    table: table_resolved.full.clone(),
                    timing,
                    events,
                    function: func_full_name,
                };

                self.store().create_trigger(txn, db_id, def).await?;
                Ok(ExecuteResult::CreateTrigger {
                    trigger_name,
                    table_name: table_resolved.full,
                })
            }
            .await
        )
    }

    pub(crate) async fn execute_drop_trigger_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let (if_exists, trigger_name, table) = parse_drop_trigger_sql(sql)?;

        autocommit_ddl!(
            session,
            async {
                let db_id = session.current_database_id();
                let (txn, _sequence_values, search_path) = session
                    .get_mut_txn_sequence_values_and_search_path()
                    .expect("Transaction must be active");

                let table_resolved = names::resolve_existing_table_name(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    &table,
                    search_path,
                )
                .await?;
                let table_resolved = match table_resolved {
                    Some(resolved) => resolved,
                    None => {
                        if if_exists {
                            // PostgreSQL requires the relation to exist for DROP TRIGGER, but db9-server
                            // treats `IF EXISTS` as a fully idempotent no-op to support common
                            // migration patterns and keep scripts deterministic.
                            let resolved = names::resolve_ddl_object_name(&table, search_path)?;
                            return Ok(ExecuteResult::DropTrigger {
                                trigger_name,
                                table_name: resolved.full,
                            });
                        }
                        return Err(SqlError::RelationNotFound(table.to_string()).into());
                    }
                };

                let dropped = self
                    .store()
                    .drop_trigger(txn, db_id, &table_resolved.full, &trigger_name)
                    .await?;
                if !dropped && !if_exists {
                    return Err(anyhow!(
                        "Trigger '{}' does not exist on '{}'",
                        trigger_name,
                        table_resolved.full
                    ));
                }

                Ok(ExecuteResult::DropTrigger {
                    trigger_name,
                    table_name: table_resolved.full,
                })
            }
            .await
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_drop_function_basic() {
        let result = parse_drop_function_sql("DROP FUNCTION my_func();").unwrap();
        assert!(!result.if_exists);
        assert!(!result.cascade);
        assert_eq!(result.names.len(), 1);
        assert_eq!(result.names[0].to_string(), "my_func");
    }

    #[test]
    fn parse_drop_function_if_exists_cascade() {
        let result = parse_drop_function_sql("DROP FUNCTION IF EXISTS my_func() CASCADE;").unwrap();
        assert!(result.if_exists);
        assert!(result.cascade);
        assert_eq!(result.names.len(), 1);
    }

    #[test]
    fn parse_drop_function_multiple_names() {
        let result = parse_drop_function_sql("DROP FUNCTION func_a(), func_b();").unwrap();
        assert_eq!(result.names.len(), 2);
        assert_eq!(result.names[0].to_string(), "func_a");
        assert_eq!(result.names[1].to_string(), "func_b");
    }

    #[test]
    fn parse_drop_function_destructure() {
        let DropFunctionParsed {
            if_exists,
            cascade,
            names,
        } = parse_drop_function_sql("DROP FUNCTION IF EXISTS f() CASCADE;").unwrap();
        assert!(if_exists);
        assert!(cascade);
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn parse_do_block_dollar_quoted() {
        let body = parse_do_block_body("DO $$ BEGIN RAISE NOTICE 'hello'; END $$;").unwrap();
        assert_eq!(body.trim(), "BEGIN RAISE NOTICE 'hello'; END");
    }

    #[test]
    fn parse_do_block_tagged_dollar_quote() {
        let body = parse_do_block_body("DO $body$ BEGIN END $body$;").unwrap();
        assert_eq!(body.trim(), "BEGIN END");
    }

    #[test]
    fn parse_do_block_with_language() {
        let body = parse_do_block_body("DO LANGUAGE plpgsql $$ BEGIN END $$;").unwrap();
        assert_eq!(body.trim(), "BEGIN END");
    }

    #[test]
    fn parse_do_block_rejects_non_plpgsql_language() {
        let err = parse_do_block_body("DO LANGUAGE sql $$ SELECT 1 $$;");
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("plpgsql"));
    }

    #[test]
    fn parse_do_block_rejects_language_without_token_boundary() {
        let err = parse_do_block_body("DO LANGUAGEPLPGSQL $$ BEGIN END $$;");
        assert!(err.is_err());
    }

    #[test]
    fn parse_do_block_rejects_trailing_tokens() {
        let err = parse_do_block_body("DO $$ BEGIN END $$ garbage");
        assert!(err.is_err());
    }

    #[test]
    fn parse_do_block_with_newlines() {
        let body = parse_do_block_body("DO\n$$\nBEGIN\nEND\n$$").unwrap();
        assert!(body.contains("BEGIN"));
        assert!(body.contains("END"));
    }
}
