//! `SET` / GUC parsing helpers

use super::{Expr, Result, SqlError};
use anyhow::anyhow;

pub(super) fn set_variable_value_to_string(value: &[Expr]) -> Result<String> {
    if value.len() != 1 {
        return Err(SqlError::Unsupported("Unsupported SET value list".into()).into());
    }
    let expr = &value[0];
    match expr {
        Expr::Value(sqlparser::ast::Value::Number(s, _)) => Ok(s.clone()),
        Expr::Value(sqlparser::ast::Value::SingleQuotedString(s))
        | Expr::Value(sqlparser::ast::Value::DoubleQuotedString(s))
        | Expr::Value(sqlparser::ast::Value::EscapedStringLiteral(s))
        | Expr::Value(sqlparser::ast::Value::RawStringLiteral(s))
        | Expr::Value(sqlparser::ast::Value::NationalStringLiteral(s))
        | Expr::Value(sqlparser::ast::Value::UnQuotedString(s)) => Ok(s.clone()),
        Expr::Value(sqlparser::ast::Value::Boolean(b)) => {
            Ok((if *b { "on" } else { "off" }).to_string())
        }
        Expr::Identifier(ident) => {
            let v = ident.value.as_str();
            if v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on") {
                Ok("on".to_string())
            } else if v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off") {
                Ok("off".to_string())
            } else {
                Ok(ident.value.clone())
            }
        }
        Expr::CompoundIdentifier(idents) if idents.len() == 1 => Ok(idents[0].value.clone()),
        Expr::Value(sqlparser::ast::Value::Null) => Ok(String::new()),
        Expr::Interval(interval) => {
            // Drivers commonly set `TimeZone` using an offset interval:
            // `SET TIME ZONE INTERVAL '+00:00' HOUR TO MINUTE`.
            // We accept hour-to-minute intervals and store the literal value (e.g. "+00:00")
            // for readback via `SHOW` / `current_setting`.
            if matches!(
                interval.leading_field,
                Some(sqlparser::ast::DateTimeField::Hour)
            ) && matches!(
                interval.last_field,
                Some(sqlparser::ast::DateTimeField::Minute)
            ) {
                let Some(s) = try_parse_const_text(interval.value.as_ref()) else {
                    return Err(
                        SqlError::Unsupported(format!("Unsupported SET value: {}", expr)).into(),
                    );
                };
                Ok(s)
            } else {
                Err(SqlError::Unsupported(format!("Unsupported SET value: {}", expr)).into())
            }
        }
        _ => Err(SqlError::Unsupported(format!("Unsupported SET value: {}", expr)).into()),
    }
}

pub(super) fn parse_search_path_guc_value(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in s.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let schema = if token.starts_with('\"') && token.ends_with('\"') && token.len() >= 2 {
            token[1..token.len() - 1].to_string()
        } else {
            token.to_lowercase()
        };
        out.push(schema);
    }
    out
}

pub(super) fn default_search_path_entries() -> Vec<String> {
    vec!["$user".to_string(), "public".to_string()]
}

pub(super) fn normalize_search_path_entries(mut entries: Vec<String>) -> Result<Vec<String>> {
    entries.retain(|s| !s.is_empty());
    if entries.len() == 1 && entries[0].eq_ignore_ascii_case("default") {
        return Ok(default_search_path_entries());
    }
    for schema in &entries {
        if schema.contains('.') {
            return Err(anyhow!("schema name '{}' must not contain '.'", schema));
        }
    }
    if entries.is_empty() {
        entries.push("public".to_string());
    }
    Ok(entries)
}

pub(super) fn try_parse_const_text(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Value(sqlparser::ast::Value::SingleQuotedString(s))
        | Expr::Value(sqlparser::ast::Value::DoubleQuotedString(s))
        | Expr::Value(sqlparser::ast::Value::EscapedStringLiteral(s))
        | Expr::Value(sqlparser::ast::Value::RawStringLiteral(s))
        | Expr::Value(sqlparser::ast::Value::NationalStringLiteral(s))
        | Expr::Value(sqlparser::ast::Value::UnQuotedString(s)) => Some(s.clone()),
        Expr::Cast { expr, .. } | Expr::TryCast { expr, .. } | Expr::SafeCast { expr, .. } => {
            try_parse_const_text(expr.as_ref())
        }
        _ => None,
    }
}

pub(super) fn try_parse_const_bool(expr: &Expr) -> Option<bool> {
    match expr {
        Expr::Value(sqlparser::ast::Value::Boolean(b)) => Some(*b),
        Expr::Identifier(ident) => {
            let v = ident.value.as_str();
            if v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on") {
                Some(true)
            } else if v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off") {
                Some(false)
            } else {
                None
            }
        }
        Expr::Cast { expr, .. } | Expr::TryCast { expr, .. } | Expr::SafeCast { expr, .. } => {
            try_parse_const_bool(expr.as_ref())
        }
        _ => None,
    }
}
