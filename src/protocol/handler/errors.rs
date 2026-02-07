use crate::sql::InFailedSqlTransaction;
use pgwire::error::{ErrorInfo, PgWireError};

fn is_ident_char(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
}

pub(super) fn sqlstate_for_executor_error(err: &anyhow::Error) -> &'static str {
    // Try typed SqlError first (new structured error path)
    if let Some(sql_err) = err.downcast_ref::<crate::sql::error::SqlError>() {
        return sql_err.sqlstate();
    }
    // Legacy check
    if err.is::<InFailedSqlTransaction>() {
        return "25P02";
    }
    // String-match fallback for errors not yet migrated to SqlError
    let msg = err.to_string();
    if msg.starts_with("invalid input syntax for type") {
        "22P02"
    } else if msg.starts_with("column \"") && msg.ends_with("\" does not exist") {
        "42703"
    } else if msg.contains("is ambiguous") {
        "42702"
    } else if msg.starts_with("relation \"") && msg.contains("does not exist") {
        "42P01"
    } else if msg.contains("duplicate key value violates unique constraint") {
        "23505"
    } else if msg.contains("violates not-null constraint") {
        "23502"
    } else if msg.contains("violates check constraint") {
        "23514"
    } else if msg.contains("Division by zero") {
        "22012"
    } else if msg.starts_with("permission denied") {
        "42501"
    } else if msg.contains("does not exist") && msg.starts_with("function") {
        "42883"
    } else {
        "XX000"
    }
}

fn find_unqualified_identifier_position(query: &str, ident: &str) -> Option<usize> {
    if ident.is_empty() {
        return None;
    }
    let query_lower = query.to_ascii_lowercase();
    let ident_lower = ident.to_ascii_lowercase();
    let haystack = query_lower.as_bytes();
    let needle = ident_lower.as_bytes();

    if needle.len() > haystack.len() {
        return None;
    }

    for i in 0..=haystack.len().saturating_sub(needle.len()) {
        if &haystack[i..i + needle.len()] != needle {
            continue;
        }

        let prev = i.checked_sub(1).map(|idx| haystack[idx]);
        if prev.is_some_and(|b| b == b'.' || is_ident_char(b)) {
            continue;
        }

        let next = haystack.get(i + needle.len()).copied();
        if next.is_some_and(is_ident_char) {
            continue;
        }

        return Some(i + 1);
    }

    None
}

pub(super) fn ambiguous_column_error_with_position(
    query: &str,
    message: &str,
) -> Option<(String, usize)> {
    let trimmed = message.trim();
    let prefix = "column reference \"";
    let suffix = "\" is ambiguous";
    let col_name = trimmed.strip_prefix(prefix)?.strip_suffix(suffix)?;
    let pos = find_unqualified_identifier_position(query, col_name)?;
    Some((col_name.to_string(), pos))
}

pub(super) fn in_failed_sql_transaction_pgwire_error() -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_string(),
        "25P02".to_string(),
        InFailedSqlTransaction.to_string(),
    )))
}

pub(super) fn syntax_error_pgwire_error(message: String) -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_owned(),
        "42601".to_owned(),
        message,
    )))
}
