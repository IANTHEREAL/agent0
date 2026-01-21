use super::executor::Executor;
use super::names;
use super::plpgsql;
use super::{ExecuteResult, Session};
use crate::types::{FunctionDef, TriggerDef};
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

fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

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

    let mut in_single = false;
    let mut in_double = false;
    let mut dollar_delim: Option<Vec<u8>> = None;

    let mut i = 0;
    while i < bytes.len() {
        if let Some(delim) = dollar_delim.as_ref() {
            let delim_len = delim.len();
            let matches =
                i + delim_len <= bytes.len() && &bytes[i..i + delim_len] == delim.as_slice();
            if matches {
                dollar_delim = None;
                i += delim_len;
            } else {
                i += 1;
            }
            continue;
        }

        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }

        match b {
            b'\'' => {
                in_single = true;
                i += 1;
                continue;
            }
            b'"' => {
                in_double = true;
                i += 1;
                continue;
            }
            b'$' => {
                if let Some(end) = bytes[i + 1..].iter().position(|&c| c == b'$') {
                    let tag_end = i + 1 + end;
                    let tag = &bytes[i + 1..tag_end];
                    if tag.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_') {
                        dollar_delim = Some(bytes[i..=tag_end].to_vec());
                        i = tag_end + 1;
                        continue;
                    }
                }
            }
            _ => {}
        }

        if i + kw.len() <= bytes.len() {
            let before_ok = i == 0 || !is_ident_char(bytes[i - 1]);
            let after_ok = i + kw.len() == bytes.len() || !is_ident_char(bytes[i + kw.len()]);
            if before_ok && after_ok {
                let mut matched = true;
                for (j, kw_b) in kw.iter().enumerate() {
                    if bytes[i + j].to_ascii_uppercase() != kw_b.to_ascii_uppercase() {
                        matched = false;
                        break;
                    }
                }
                if matched {
                    return Some(i);
                }
            }
        }

        i += 1;
    }

    None
}

fn find_matching_paren(s: &str, open_pos: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    if open_pos >= bytes.len() || bytes[open_pos] != b'(' {
        return None;
    }

    let mut depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut i = open_pos;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }

        match b {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'(' => depth += 1,
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_top_level_commas(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;

    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }

        match b {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&s[start..]);
    parts
}

fn strip_default_clause(arg: &str) -> &str {
    let bytes = arg.as_bytes();
    let mut depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'=' if depth == 0 => return arg[..i].trim_end(),
            _ => {}
        }
        i += 1;
    }

    if let Some(pos) = find_keyword_outside_quotes_and_dollar(arg, "DEFAULT") {
        return arg[..pos].trim_end();
    }
    arg
}

fn looks_like_type_keyword(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "bool"
            | "boolean"
            | "int"
            | "integer"
            | "int2"
            | "smallint"
            | "int4"
            | "int8"
            | "bigint"
            | "serial"
            | "bigserial"
            | "float"
            | "float4"
            | "float8"
            | "double"
            | "real"
            | "numeric"
            | "decimal"
            | "text"
            | "varchar"
            | "character"
            | "char"
            | "timestamp"
            | "timestamptz"
            | "date"
            | "time"
            | "interval"
            | "uuid"
            | "json"
            | "jsonb"
            | "bytea"
    )
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

fn parse_sql_string_or_dollar_literal(s: &str) -> Result<String> {
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
        return Ok(s[body_start..body_end].to_string());
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
                return Ok(out);
            }
            out.push(b as char);
            i += 1;
        }
        return Err(anyhow!("Unterminated string literal"));
    }

    Err(anyhow!("Expected string literal after AS"))
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
    };

    Ok((name, def, or_replace))
}

fn parse_drop_function_sql(sql: &str) -> Result<(bool, Vec<ObjectName>)> {
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
    let rest = if let Some(pos) = rest_upper.rfind(" CASCADE") {
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

    Ok((if_exists, names))
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

impl Executor {
    pub(crate) async fn execute_create_function_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let (name, mut def, or_replace) = parse_create_function_sql(sql)?;

        if def.language.to_lowercase() == "plpgsql" {
            plpgsql::validate_plpgsql_body(&def.body)?;
        }

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");
            let resolved = names::resolve_ddl_object_name(&name, search_path)?;
            if !self.store().schema_exists(txn, &resolved.schema).await? {
                return Err(anyhow!("schema '{}' does not exist", resolved.schema));
            }
            def.schema = resolved.schema.clone();
            def.name = resolved.name.clone();

            if or_replace {
                self.store().replace_function(txn, def).await?;
            } else {
                self.store().create_function(txn, def).await?;
            }
            Ok(ExecuteResult::CreateFunction {
                func_name: resolved.full,
            })
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }

    pub(crate) async fn execute_drop_function_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let (if_exists, names) = parse_drop_function_sql(sql)?;

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let mut last_name = None;
            for name in names {
                let resolved = names::resolve_existing_function_name(
                    self.store().as_ref(),
                    txn,
                    &name,
                    search_path,
                )
                .await?;
                let func_full_name = match resolved {
                    Some(resolved) => resolved.full,
                    None => names::resolve_ddl_object_name(&name, search_path)?.full,
                };
                last_name = Some(func_full_name.clone());
                let dropped = self.store().drop_function(txn, &func_full_name).await?;
                if !dropped && !if_exists {
                    return Err(anyhow!("Function '{}' does not exist", func_full_name));
                }
            }

            Ok(ExecuteResult::DropFunction {
                func_name: last_name.unwrap_or_else(|| "unknown".to_string()),
            })
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }

    pub(crate) async fn execute_create_trigger_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let (trigger_name, timing, events, table, function) = parse_create_trigger_sql(sql)?;

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let table_resolved =
                names::resolve_existing_table_name(self.store().as_ref(), txn, &table, search_path)
                    .await?
                    .ok_or_else(|| anyhow!("Table '{}' not found", table))?;

            let func_resolved = names::resolve_existing_function_name(
                self.store().as_ref(),
                txn,
                &function,
                search_path,
            )
            .await?;
            let func_full_name = match func_resolved {
                Some(resolved) => resolved.full,
                None => names::resolve_ddl_object_name(&function, search_path)?.full,
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

            self.store().create_trigger(txn, def).await?;
            Ok(ExecuteResult::CreateTrigger {
                trigger_name,
                table_name: table_resolved.full,
            })
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }

    pub(crate) async fn execute_drop_trigger_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let (if_exists, trigger_name, table) = parse_drop_trigger_sql(sql)?;

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let table_resolved =
                names::resolve_existing_table_name(self.store().as_ref(), txn, &table, search_path)
                    .await?;
            let table_resolved = match table_resolved {
                Some(resolved) => resolved,
                None => {
                    if if_exists {
                        // PostgreSQL requires the relation to exist for DROP TRIGGER, but pg-tikv
                        // treats `IF EXISTS` as a fully idempotent no-op to support common
                        // migration patterns and keep scripts deterministic.
                        let resolved = names::resolve_ddl_object_name(&table, search_path)?;
                        return Ok(ExecuteResult::DropTrigger {
                            trigger_name,
                            table_name: resolved.full,
                        });
                    }
                    return Err(anyhow!("Table '{}' not found", table));
                }
            };

            let dropped = self
                .store()
                .drop_trigger(txn, &table_resolved.full, &trigger_name)
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
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }
}
