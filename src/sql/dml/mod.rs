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

use crate::model::{Row, Value};

pub use insert::predicates_match_public;

pub type EnumLabelCache = HashMap<String, HashSet<String>>;

/// Resolved target selector for `ON CONFLICT DO UPDATE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictTarget {
    /// `ON CONFLICT (col1, col2, ...) [WHERE predicate]`
    Columns(Vec<String>, Option<String>),
    /// `ON CONFLICT ON CONSTRAINT constraint_name`
    Constraint(String),
}

/// How `execute_insert_row` should handle unique-key conflicts.
///
/// Replaces the previous `&Option<OnInsert>` parameter, removing the dependency
/// on raw sqlparser AST types from the typed execution path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictBehavior {
    /// No ON CONFLICT -- unique violations produce an error.
    Error,
    /// ON CONFLICT DO NOTHING -- skip the conflicting row.
    /// Contains an optional conflict target to restrict which conflict triggers the skip.
    DoNothing { target: Option<ConflictTarget> },
    /// ON CONFLICT DO UPDATE -- return the conflicting row for caller-side update.
    /// Contains an optional conflict target to restrict which conflict is matched.
    DoUpdate { target: Option<ConflictTarget> },
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
pub use delete::collect_deletion_keys;
// foreign_keys functions are used internally by executor/dml_analyzed
pub(crate) use foreign_keys::{
    build_fk_ref_schema_cache, collect_deferred_self_fk_checks, pk_to_hash_key,
    resolve_fk_ref_lookup, self_ref_fk_keys, validate_deferred_self_fk_refs,
    validate_foreign_keys_non_self_ref, validate_foreign_keys_with_cache, ConstraintId,
    FkDeleteContext, FkLockCache, FkRefLookup, FkRefSchemaCache, FkStoreCtx,
};
#[allow(unused_imports)]
pub use foreign_keys::{
    handle_foreign_key_on_delete, handle_foreign_key_on_update, validate_foreign_keys,
};
pub use insert::execute_insert_row_defer_hnsw;
pub(crate) use insert::validate_enum_values;
pub use insert::{build_enum_label_cache, execute_insert_row};
pub(crate) use update::execute_update_row_without_fk_update;
pub use update::{
    batch_maintain_hnsw_indexes, batch_maintain_hnsw_indexes_for_inserts,
    collect_update_new_btree_entries, collect_update_new_gin_mutations, collect_update_old_keys,
    encode_data_row_mutation, execute_update_row, execute_update_row_by_pk,
    execute_update_row_by_pk_defer_hnsw, execute_update_row_defer_hnsw,
};
