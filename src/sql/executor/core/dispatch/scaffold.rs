//! Statement dispatch scaffolding: pre-computed context for a single SQL statement.
//!
//! [`DispatchContext`] is built once at the top of `execute_single` and threaded
//! through every dispatch phase (failed-txn precheck → raw dispatch → parse/AST
//! dispatch). All fields are immutable after construction.

use super::super::*;
use crate::sql::raw_sql::RawSqlKind;

/// Immutable context computed once at the start of `execute_single`.
///
/// Contains the pre-processed SQL text and classification results that drive
/// the dispatch decision tree. All fields are derived from the raw SQL input
/// and session state at dispatch entry.
pub(super) struct DispatchContext {
    /// SQL with leading comments stripped and leading whitespace trimmed.
    pub sql_trimmed: String,
    /// Fully uppercased + trimmed SQL for prefix matching.
    pub sql_upper: String,
    /// SQL string used for observability recording.
    pub sql_for_observability: String,
    /// Raw-SQL classification (if any).
    pub raw_kind: Option<RawSqlKind>,
    /// Whether the current user is the observability user (non-superuser).
    pub is_observability_user: bool,
}

impl DispatchContext {
    /// Build context from the raw SQL string and current session state.
    pub fn new(sql: &str, session: &Session) -> Self {
        let sql_stripped = strip_leading_sql_comments(sql);
        let sql_trimmed = sql_stripped.trim_start().to_string();
        let is_observability_user =
            session.current_user() == Some(OBSERVABILITY_USER) && !session.is_superuser();
        let sql_upper = sql_trimmed.trim().to_ascii_uppercase();
        let sql_for_observability = sql_trimmed.clone();
        let raw_kind = crate::sql::raw_sql::classify(&sql_upper);

        Self {
            sql_trimmed,
            sql_upper,
            sql_for_observability,
            raw_kind,
            is_observability_user,
        }
    }

    /// Case-insensitive prefix check on the trimmed SQL.
    pub fn starts_with(&self, prefix: &str) -> bool {
        starts_with_ignore_ascii_case(&self.sql_trimmed, prefix)
    }
}
