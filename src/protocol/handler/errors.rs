use crate::sql::error::SqlError;
use crate::sql::executor::core::timeout::StatementTimeoutError;
use crate::storage::StorageError;
use pgwire::error::{ErrorInfo, PgWireError};

fn is_ident_char(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_') || b >= 0x80
}

/// Check if a TiKV error contains a write conflict (KeyError with `conflict` field set).
/// PostgreSQL equivalent: `40001 serialization_failure` — the client should retry.
fn is_tikv_write_conflict(err: &anyhow::Error) -> bool {
    fn has_conflict(err: &tikv_client::Error) -> bool {
        match err {
            tikv_client::Error::KeyError(key_err) => key_err.conflict.is_some(),
            tikv_client::Error::PessimisticLockError { inner, .. } => has_conflict(inner),
            tikv_client::Error::UndeterminedError(inner) => has_conflict(inner),
            tikv_client::Error::ExtractedErrors(errors)
            | tikv_client::Error::MultipleKeyErrors(errors) => errors.iter().any(has_conflict),
            _ => false,
        }
    }
    err.chain().any(|cause| {
        cause
            .downcast_ref::<tikv_client::Error>()
            .is_some_and(has_conflict)
    })
}

/// Check if a TiKV error contains a deadlock (KeyError with `deadlock` field set).
/// PostgreSQL equivalent: `40P01 deadlock_detected`.
fn is_tikv_deadlock(err: &anyhow::Error) -> bool {
    fn has_deadlock(err: &tikv_client::Error) -> bool {
        match err {
            tikv_client::Error::KeyError(key_err) => key_err.deadlock.is_some(),
            tikv_client::Error::PessimisticLockError { inner, .. } => has_deadlock(inner),
            tikv_client::Error::UndeterminedError(inner) => has_deadlock(inner),
            tikv_client::Error::ExtractedErrors(errors)
            | tikv_client::Error::MultipleKeyErrors(errors) => errors.iter().any(has_deadlock),
            _ => false,
        }
    }
    err.chain().any(|cause| {
        cause
            .downcast_ref::<tikv_client::Error>()
            .is_some_and(has_deadlock)
    })
}

/// Check if a TiKV error is a lock-resolution failure or locked-key error.
/// These are transient MVCC lock encounters — mapped to `40001` because the
/// client should retry the transaction (not `55P03`, which is NOWAIT-only).
fn is_tikv_lock_resolution_failure(err: &anyhow::Error) -> bool {
    fn has_lock_failure(err: &tikv_client::Error) -> bool {
        match err {
            tikv_client::Error::ResolveLockError(_) => true,
            tikv_client::Error::KeyError(key_err) => key_err.locked.is_some(),
            tikv_client::Error::PessimisticLockError { inner, .. } => has_lock_failure(inner),
            tikv_client::Error::UndeterminedError(inner) => has_lock_failure(inner),
            tikv_client::Error::ExtractedErrors(errors)
            | tikv_client::Error::MultipleKeyErrors(errors) => errors.iter().any(has_lock_failure),
            _ => err.is_lock_conflict(),
        }
    }
    err.chain().any(|cause| {
        cause
            .downcast_ref::<tikv_client::Error>()
            .is_some_and(has_lock_failure)
    })
}

fn storage_error(err: &anyhow::Error) -> Option<&StorageError> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<StorageError>())
}

pub(super) fn sqlstate_for_executor_error(err: &anyhow::Error) -> &'static str {
    if let Some(sql_err) = err.downcast_ref::<SqlError>() {
        return sql_err.sqlstate();
    }
    // Statement timeout → 57014 (query_canceled), matching PostgreSQL.
    if err.is::<StatementTimeoutError>() {
        return "57014";
    }
    // StorageError → the staged facade SQLSTATE surface from issue #2523.
    if let Some(storage_err) = storage_error(err) {
        return storage_err.sqlstate();
    }
    // WriteConflict → 40001 (serialization_failure): client should retry the txn.
    if is_tikv_write_conflict(err) {
        return "40001";
    }
    // Deadlock → 40P01 (deadlock_detected).
    if is_tikv_deadlock(err) {
        return "40P01";
    }
    // Lock resolution failure → 40001 (serialization_failure): transient,
    // client should retry.  NOT 55P03 — that is reserved for NOWAIT/SKIP LOCKED
    // (routed through SqlError::LockNotAvailable / SqlError::LockTimeout).
    if is_tikv_lock_resolution_failure(err) {
        return "40001";
    }
    "XX000"
}

pub(super) fn pg_error_hint(err: &anyhow::Error) -> Option<&'static str> {
    let sql_err = err.downcast_ref::<SqlError>()?;
    match sql_err {
        SqlError::InvalidEscapeString { message } if message == "invalid escape string" => {
            Some("Escape string must be empty or one character.")
        }
        _ => None,
    }
}

/// Produce a PostgreSQL-compatible error message for executor errors.
///
/// TiKV transaction errors are replaced with standard PostgreSQL messages
/// (e.g. "could not serialize access due to concurrent update") to avoid
/// leaking internal implementation details to clients. Non-TiKV errors
/// preserve their original message text.
pub(super) fn pg_error_message(err: &anyhow::Error, sqlstate: &str) -> String {
    if let Some(storage_err) = storage_error(err) {
        if let Some(message) = storage_err.pg_message() {
            return message.to_string();
        }
    }

    match sqlstate {
        "40001" => {
            if is_tikv_write_conflict(err) || is_tikv_lock_resolution_failure(err) {
                return "could not serialize access due to concurrent update".to_string();
            }
            err.to_string()
        }
        "40P01" => "deadlock detected".to_string(),
        _ => err.to_string(),
    }
}

pub(super) fn executor_error_info(err: &anyhow::Error) -> ErrorInfo {
    let sqlstate = sqlstate_for_executor_error(err);
    let mut error_info = ErrorInfo::new(
        "ERROR".to_string(),
        sqlstate.to_string(),
        pg_error_message(err, sqlstate),
    );
    if let Some(hint) = pg_error_hint(err) {
        error_info.hint = Some(hint.to_string());
    }
    error_info
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
