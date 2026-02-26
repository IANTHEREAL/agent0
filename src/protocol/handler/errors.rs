use crate::sql::error::SqlError;
use pgwire::error::{ErrorInfo, PgWireError};

fn is_ident_char(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
}

fn is_tikv_lock_conflict(err: &anyhow::Error) -> bool {
    fn is_conflict(err: &tikv_client::Error) -> bool {
        match err {
            tikv_client::Error::KeyError(key_err) => {
                key_err.locked.is_some() || key_err.conflict.is_some() || key_err.deadlock.is_some()
            }
            tikv_client::Error::PessimisticLockError { inner, .. } => is_conflict(inner),
            tikv_client::Error::UndeterminedError(inner) => is_conflict(inner),
            tikv_client::Error::ExtractedErrors(errors)
            | tikv_client::Error::MultipleKeyErrors(errors) => {
                !errors.is_empty() && errors.iter().all(is_conflict)
            }
            _ => err.is_lock_conflict(),
        }
    }

    err.chain().any(|cause| {
        cause
            .downcast_ref::<tikv_client::Error>()
            .is_some_and(is_conflict)
    })
}

pub(super) fn sqlstate_for_executor_error(err: &anyhow::Error) -> &'static str {
    if let Some(sql_err) = err.downcast_ref::<SqlError>() {
        return sql_err.sqlstate();
    }
    if is_tikv_lock_conflict(err) {
        return "55P03";
    }
    "XX000"
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
        SqlError::InFailedTransaction.to_string(),
    )))
}

pub(super) fn syntax_error_pgwire_error(message: String) -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_owned(),
        "42601".to_owned(),
        message,
    )))
}

/// Build an `ErrorInfo` with severity "ERROR".
pub(super) fn error_info(sqlstate: &str, message: impl Into<String>) -> ErrorInfo {
    ErrorInfo::new("ERROR".to_string(), sqlstate.to_string(), message.into())
}

/// Build a `PgWireError::UserError` with severity "ERROR".
pub(super) fn user_error(sqlstate: &str, message: impl Into<String>) -> PgWireError {
    PgWireError::UserError(Box::new(error_info(sqlstate, message)))
}
