//! DML (Data Manipulation Language) row-level operations: INSERT, UPDATE, DELETE.
//!
//! Sub-modules handle individual operations and shared concerns:
//! - `insert`: INSERT row execution with conflict resolution and index materialization.
//! - `update`: UPDATE row execution with index maintenance and PK change detection.
//! - `delete`: DELETE row execution with storage entry cleanup.
//! - `foreign_keys`: FK constraint validation and cascade operations.
//! - `defaults`: Default expression evaluation, column filling, and value coercion.

mod defaults;
mod delete;
mod foreign_keys;
pub(crate) mod insert;
mod update;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

use crate::types::{Row, Value};

pub type EnumLabelCache = HashMap<String, HashSet<String>>;

/// How `execute_insert_row` should handle unique-key conflicts.
///
/// Replaces the previous `&Option<OnInsert>` parameter, removing the dependency
/// on raw sqlparser AST types from the typed execution path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictBehavior {
    /// No ON CONFLICT -- unique violations produce an error.
    Error,
    /// ON CONFLICT DO NOTHING -- skip the conflicting row.
    DoNothing,
    /// ON CONFLICT DO UPDATE -- return the conflicting row for caller-side update.
    DoUpdate,
}

pub enum InsertRowResult {
    Inserted(Row),
    Skipped,
    Conflicted {
        existing_pk: Vec<Value>,
        existing_row: Row,
        excluded_row: Row,
    },
}

// ── Re-exports ──────────────────────────────────────────────────────────────

pub use defaults::{
    coerce_row_values, coerce_row_values_allow_null, eval_column_default_or_null,
    fill_missing_columns,
};
pub use delete::execute_delete_row;
// foreign_keys functions are used internally by executor/dml_analyzed
#[allow(unused_imports)]
pub use foreign_keys::{
    handle_foreign_key_on_delete, handle_foreign_key_on_update, validate_foreign_keys,
};
pub(crate) use foreign_keys::{
    pk_to_hash_key, resolve_fk_ref_lookup, FkDeleteContext, FkRefLookup,
};
pub use insert::{build_enum_label_cache, execute_insert_row};
pub(crate) use update::execute_update_row_without_fk_update;
pub use update::{execute_update_row, execute_update_row_by_pk};
