//! DDL (Data Definition Language) execution — CREATE, ALTER, DROP for tables,
//! indexes, views, materialized views, and constraints.
//!
//! This module is the dispatch point for all DDL operations and hosts shared
//! helper types / functions used across sub-modules.

mod alter_table;
mod create_index;
mod create_table;
mod drop;
#[cfg(test)]
mod tests;
mod view;

use std::collections::HashSet;
use std::sync::{Arc, Once};

use crate::sql::analyzer::types::TypedExpr;
use crate::sql::analyzer::{Analyzer, CatalogSnapshot, Scope};
use crate::sql::error::SqlError;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::expr::typed_fold::fold_typed_expr;
use crate::sql::query_context::QueryContext;
use anyhow::{anyhow, Result};
use sqlparser::ast::{DataType as SqlDataType, Expr, ObjectName};
use tikv_client::Transaction;

use super::names;
use super::names::normalize_ident;
use super::sequences;
use super::types::sql_datatype_to_internal_strict;
use super::value_coercion::coerce_value_for_column;

use crate::model::{
    CheckConstraint, ColumnDef, DataType, ForeignKeyAction, ForeignKeyConstraint, IndexDef, Row,
    TableSchema, Value,
};
use crate::storage::TikvStore;
use crate::txn::txn_delete;

// ── Re-exports (preserve pub(crate) surface) ───────────────────────────────

pub use create_table::check_relation_name_available;
pub use create_table::create_table_from_query_result;
pub use create_table::create_table_from_select_into;
pub use create_table::create_table_from_stream;
pub use create_table::execute_create_table;
// has_legacy_name_conflict is used internally by create_table, not re-exported

pub use create_index::backfill_index_by_name;
pub use create_index::execute_create_index;
pub use create_index::reconcile_index;
pub use create_index::update_index_state;

pub use view::execute_create_materialized_view;
pub use view::execute_create_view;
pub use view::execute_drop_materialized_view;
pub use view::execute_drop_view;
pub use view::execute_refresh_materialized_view;

pub use drop::execute_drop_index;
pub use drop::execute_drop_table;
pub use drop::execute_truncate;

pub use alter_table::execute_alter_table;

// ── Constants ───────────────────────────────────────────────────────────────

pub(super) const DDL_SCAN_BATCH_SIZE: u32 = 1024;
pub(super) const DDL_BACKFILL_COMMIT_SIZE: usize = 5000;
const LEGACY_RELNAME_CONFLICT_SCAN_SUNSET_DATE: &str = "2026-12-31";

// ── Shared helpers ──────────────────────────────────────────────────────────

pub(super) fn warn_legacy_relname_conflict_scan_once() {
    static WARN_ONCE: Once = Once::new();
    WARN_ONCE.call_once(|| {
        tracing::warn!(
            sunset_date = LEGACY_RELNAME_CONFLICT_SCAN_SUNSET_DATE,
            issue = "#775",
            "legacy relation-name conflict scan is active for pre-sys_relname_ clusters; remove after all clusters are migrated"
        );
    });
}

pub(super) fn analyze_row_level_expr(
    expr: &Expr,
    schema: &TableSchema,
    db_id: u64,
    search_path: &[String],
    collations: &[crate::sql::collation::CollationDef],
) -> Result<TypedExpr> {
    let mut catalog = CatalogSnapshot::new(search_path.to_vec(), db_id);
    let table_name = schema.name.rsplit('.').next().unwrap_or(&schema.name);
    catalog.add_table(table_name, schema.name.clone(), schema.clone());
    if let Some(alias) = &schema.from_alias {
        catalog.add_table(alias, schema.name.clone(), schema.clone());
    }
    for def in collations {
        catalog.add_collation(&def.name, def.clone());
    }

    let typed = Analyzer::analyze_expr_with_scope(
        &catalog,
        Scope::from_table_schema(table_name, schema),
        expr,
    )
    .map_err(SqlError::from)?;
    let qctx = QueryContext::from_task_locals();
    Ok(fold_typed_expr(&typed, &qctx))
}

pub(super) fn eval_row_level_expr(
    typed_expr: &TypedExpr,
    row: &Row,
    qctx: &QueryContext,
) -> Result<Value> {
    eval_typed_expr(typed_expr, row, qctx)
}

pub(super) async fn resolve_column_data_type(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    sql_type: &SqlDataType,
) -> Result<(DataType, bool)> {
    match sql_type {
        SqlDataType::Custom(name, _) => {
            let type_ident = name.0.last().ok_or_else(|| anyhow!("Invalid type name"))?;
            let type_name = type_ident.value.to_uppercase();

            match type_name.as_str() {
                "SERIAL" => Ok((DataType::Int32, true)),
                "BIGSERIAL" => Ok((DataType::Int64, true)),
                _ => {
                    let resolved_type = names::resolve_existing_type_name(
                        store.as_ref(),
                        txn,
                        db_id,
                        name,
                        search_path,
                    )
                    .await?;
                    let Some(resolved_type) = resolved_type else {
                        return Ok((sql_datatype_to_internal_strict(sql_type)?, false));
                    };
                    let full_name = resolved_type.full;
                    match store.get_type(txn, db_id, &full_name).await? {
                        Some(def) => match def.kind {
                            crate::model::UserTypeKind::Enum { .. } => {
                                Ok((DataType::UserDefined(full_name), false))
                            }
                            crate::model::UserTypeKind::Composite { .. } => {
                                Ok((DataType::UserDefined(full_name), false))
                            }
                        },
                        None => Ok((sql_datatype_to_internal_strict(sql_type)?, false)),
                    }
                }
            }
        }
        _ => Ok((sql_datatype_to_internal_strict(sql_type)?, false)),
    }
}

pub(super) async fn create_implicit_sequences_for_schema(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
) -> Result<()> {
    for col in &schema.columns {
        if col.is_serial {
            store
                .create_sequence(
                    txn,
                    db_id,
                    sequences::build_implicit_sequence_def(&schema.name, &col.name, &col.data_type),
                )
                .await?;
        }
    }
    Ok(())
}

pub(super) async fn advance_implicit_sequences_for_seeded_rows(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    row_count: usize,
) -> Result<()> {
    if row_count == 0 {
        return Ok(());
    }

    let last_value = i64::try_from(row_count)
        .map_err(|_| anyhow!("row count {} overflows sequence value", row_count))?;
    let (table_schema, table_name) = schema
        .name
        .rsplit_once('.')
        .unwrap_or(("public", schema.name.as_str()));

    for col in &schema.columns {
        if !col.is_serial {
            continue;
        }

        let seq_full_name = format!(
            "{}.{}",
            table_schema,
            sequences::implicit_sequence_name(table_name, &col.name)
        );
        store
            .setval_sequence(txn, db_id, &seq_full_name, last_value, true)
            .await?;
    }

    Ok(())
}

pub(super) async fn drop_owned_sequences_for_table(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
) -> Result<()> {
    let seqs = store.list_sequences(txn, db_id).await?;
    for def in seqs {
        let Some((owned_table, _)) = &def.owned_by else {
            continue;
        };
        if owned_table == table_name {
            store.drop_sequence(txn, db_id, &def.full_name()).await?;
        }
    }
    Ok(())
}

// ── KV scan batching ────────────────────────────────────────────────────────

pub(super) struct KvScanBatches {
    next_start: Option<Vec<u8>>,
    end: Vec<u8>,
    limit: u32,
}

impl KvScanBatches {
    pub(super) fn new(start: Vec<u8>, end: Vec<u8>, limit: u32) -> Self {
        Self {
            next_start: Some(start),
            end,
            limit,
        }
    }

    pub(super) async fn next_batch(
        &mut self,
        txn: &mut Transaction,
    ) -> Result<Option<Vec<tikv_client::KvPair>>> {
        let Some(start) = self.next_start.take() else {
            return Ok(None);
        };

        let range: tikv_client::BoundRange = (start..self.end.clone()).into();
        let pairs: Vec<tikv_client::KvPair> = txn.scan(range, self.limit).await?.collect();
        if pairs.is_empty() {
            self.next_start = None;
            return Ok(None);
        }

        let last_key: &[u8] = pairs.last().unwrap().key().as_ref().into();
        let mut next_start = last_key.to_vec();
        next_start.push(0x00);
        self.next_start = Some(next_start);

        Ok(Some(pairs))
    }
}

// ── Constraint utilities ────────────────────────────────────────────────────

pub(super) fn assign_generated_check_constraint_names(
    table: &str,
    pk_exists: bool,
    indexes: &[IndexDef],
    foreign_keys: &[ForeignKeyConstraint],
    checks: &mut Vec<CheckConstraint>,
) {
    let mut used_names: HashSet<String> = HashSet::new();
    if pk_exists {
        used_names.insert(format!("{}_pkey", table));
    }
    used_names.extend(indexes.iter().map(|idx| idx.name.clone()));
    used_names.extend(foreign_keys.iter().map(|fk| fk.name.clone()));
    used_names.extend(checks.iter().filter_map(|c| c.name.as_ref()).cloned());

    for check in checks.iter_mut() {
        if check.name.is_some() {
            continue;
        }

        // PostgreSQL convention: {table}_{first_column}_check
        // Extract the first identifier from the expression that looks like a column name.
        let first_col = extract_first_column_from_check_expr(&check.expr);
        let base = if let Some(col) = first_col {
            format!("{}_{}_check", table, col)
        } else {
            format!("{}_check", table)
        };

        // Ensure uniqueness by appending a suffix if needed
        if used_names.insert(base.clone()) {
            check.name = Some(base);
        } else {
            let mut suffix = 1usize;
            loop {
                let candidate = format!("{}{}", base, suffix);
                suffix += 1;
                if used_names.insert(candidate.clone()) {
                    check.name = Some(candidate);
                    break;
                }
            }
        }
    }
}

/// Extract the first column-like identifier from a check constraint expression.
/// E.g., `age > 0` → Some("age"), `salary >= 0` → Some("salary").
pub(super) fn extract_first_column_from_check_expr(expr: &str) -> Option<String> {
    // Simple extraction: find the first identifier token (letters/underscores)
    // that isn't a SQL keyword.
    static KEYWORDS: &[&str] = &[
        "and", "or", "not", "in", "is", "null", "true", "false", "between", "like", "ilike", "any",
        "all", "exists", "case", "when", "then", "else", "end", "check",
    ];
    for word in expr.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let w = word.trim();
        if w.is_empty() || w.chars().next().map_or(true, |c| c.is_ascii_digit()) {
            continue;
        }
        if KEYWORDS.contains(&w.to_lowercase().as_str()) {
            continue;
        }
        // Skip common type names and string literals
        if [
            "text", "integer", "numeric", "varchar", "int", "bigint", "boolean",
        ]
        .contains(&w.to_lowercase().as_str())
        {
            continue;
        }
        return Some(w.to_lowercase());
    }
    None
}

pub(super) fn check_constraint_effective_name(
    table: &str,
    _ordinal: usize,
    check: &CheckConstraint,
) -> String {
    check.name.clone().unwrap_or_else(|| {
        let first_col = extract_first_column_from_check_expr(&check.expr);
        if let Some(col) = first_col {
            format!("{}_{}_check", table, col)
        } else {
            format!("{}_check", table)
        }
    })
}

pub(super) fn find_check_constraint_index(
    schema: &TableSchema,
    table: &str,
    name: &str,
) -> Option<usize> {
    schema
        .check_constraints
        .iter()
        .enumerate()
        .find_map(|(i, c)| {
            let effective = check_constraint_effective_name(table, i, c);
            (effective == name).then_some(i)
        })
}

pub(super) fn constraint_name_exists(schema: &TableSchema, table: &str, name: &str) -> bool {
    if !schema.pk_indices.is_empty() {
        let default_pk_name;
        let pk_name = match schema.pk_constraint_name.as_deref() {
            Some(n) => n,
            None => {
                default_pk_name = format!("{}_pkey", table);
                &default_pk_name
            }
        };
        if name == pk_name {
            return true;
        }
    }

    if schema.foreign_keys.iter().any(|fk| fk.name == name) {
        return true;
    }
    if schema.indexes.iter().any(|idx| idx.name == name) {
        return true;
    }
    if schema
        .check_constraints
        .iter()
        .enumerate()
        .any(|(i, c)| check_constraint_effective_name(table, i, c) == name)
    {
        return true;
    }

    false
}

pub(super) fn check_expr_references_column(expr_str: &str, col_name: &str) -> Result<bool> {
    use core::ops::ControlFlow;
    use sqlparser::ast::{visit_expressions, Expr as AstExpr};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    let dialect = PostgreSqlDialect {};
    let expr = Parser::new(&dialect)
        .try_with_sql(expr_str)
        .and_then(|mut p| p.parse_expr())
        .map_err(|e| anyhow!("Invalid CHECK expression '{}': {}", expr_str, e))?;

    let mut found = false;
    let _ = visit_expressions(&expr, |e| {
        if found {
            return ControlFlow::Break(());
        }

        match e {
            AstExpr::Identifier(ident) => {
                if normalize_ident(ident) == col_name {
                    found = true;
                    return ControlFlow::Break(());
                }
            }
            AstExpr::CompoundIdentifier(parts) => {
                if let Some(last) = parts.last() {
                    if normalize_ident(last) == col_name {
                        found = true;
                        return ControlFlow::Break(());
                    }
                }
            }
            _ => {}
        }

        ControlFlow::<()>::Continue(())
    });
    Ok(found)
}

pub(super) fn rewrite_check_expr_column(
    expr_str: &str,
    old_col_name: &str,
    new_col_name: &str,
    new_quote_style: Option<char>,
) -> Result<String> {
    use core::ops::ControlFlow;
    use sqlparser::ast::{visit_expressions_mut, Expr as AstExpr};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    let dialect = PostgreSqlDialect {};
    let mut expr = Parser::new(&dialect)
        .try_with_sql(expr_str)
        .and_then(|mut p| p.parse_expr())
        .map_err(|e| anyhow!("Invalid CHECK expression '{}': {}", expr_str, e))?;

    let _ = visit_expressions_mut(&mut expr, |e| {
        match e {
            AstExpr::Identifier(ident) => {
                if normalize_ident(ident) == old_col_name {
                    ident.value = new_col_name.to_string();
                    ident.quote_style = new_quote_style;
                }
            }
            AstExpr::CompoundIdentifier(parts) => {
                if let Some(last) = parts.last_mut() {
                    if normalize_ident(last) == old_col_name {
                        last.value = new_col_name.to_string();
                        last.quote_style = new_quote_style;
                    }
                }
            }
            _ => {}
        }

        ControlFlow::<()>::Continue(())
    });

    Ok(expr.to_string())
}

// ── KV range utilities ──────────────────────────────────────────────────────

pub(super) fn prefix_end(mut key: Vec<u8>) -> Vec<u8> {
    for i in (0..key.len()).rev() {
        if key[i] != 0xFF {
            key[i] = key[i].wrapping_add(1);
            key.truncate(i + 1);
            return key;
        }
    }

    unreachable!("prefix_end called with all-0xFF prefix")
}

pub(super) fn index_prefix_range(db_id: u64, table_id: u64, index_id: u64) -> (Vec<u8>, Vec<u8>) {
    // Compute the end bound by incrementing the fixed-length (table_id, index_id) prefix,
    // so the range is independent of memcomparable-encoded index values.
    let mut prefix = Vec::with_capacity(2 + 8 + 1 + 2 + 8 + 1 + 8);
    prefix.extend_from_slice(b"d_");
    prefix.extend_from_slice(&db_id.to_be_bytes());
    prefix.push(b'_');
    prefix.extend_from_slice(b"i_");
    prefix.extend_from_slice(&table_id.to_be_bytes());
    prefix.push(b'_');
    prefix.extend_from_slice(&index_id.to_be_bytes());

    let mut start = prefix.clone();
    start.push(b'_');
    let end = prefix_end(prefix);

    (start, end)
}

pub(super) async fn delete_range(
    txn: &mut Transaction,
    start: Vec<u8>,
    end: Vec<u8>,
) -> Result<()> {
    let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
    while let Some(batch) = scanner.next_batch(txn).await? {
        for pair in batch {
            txn_delete(txn, pair.into_key().into()).await?;
        }
    }
    Ok(())
}

pub(super) async fn maybe_rotate_backfill_txn(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    current_batch_writes: &mut usize,
    has_committed_batches: &mut bool,
) -> Result<()> {
    if *current_batch_writes < DDL_BACKFILL_COMMIT_SIZE {
        return Ok(());
    }

    txn.commit().await?;
    *txn = store.begin().await?;
    *current_batch_writes = 0;
    *has_committed_batches = true;
    Ok(())
}

pub(super) fn coerce_value_for_type_change(val: Value, target_col: &ColumnDef) -> Result<Value> {
    let new_type = &target_col.data_type;
    if matches!(val, Value::Null) {
        return Ok(Value::Null);
    }

    if val.data_type().as_ref() == Some(new_type) {
        return Ok(val);
    }

    match (val, new_type) {
        (Value::Json(s), DataType::Text) => Ok(Value::Text(s)),
        (Value::Jsonb(s), DataType::Text) => {
            Ok(Value::Text(crate::sql::jsonb::format_jsonb_pg_str(&s)))
        }
        (Value::Uuid(bytes), DataType::Text) => {
            Ok(Value::Text(uuid::Uuid::from_bytes(bytes).to_string()))
        }
        (v, _) => {
            let coerced = coerce_value_for_column(v, target_col)?;
            if coerced.data_type().as_ref() == Some(new_type) {
                Ok(coerced)
            } else {
                Err(
                    SqlError::Unsupported(format!("Unsupported type conversion to {}", new_type))
                        .into(),
                )
            }
        }
    }
}

// ── Parse foreign key action (shared by create_table and alter_table) ───────

pub(super) fn parse_referential_action(
    action: &Option<sqlparser::ast::ReferentialAction>,
) -> ForeignKeyAction {
    match action {
        Some(sqlparser::ast::ReferentialAction::Cascade) => ForeignKeyAction::Cascade,
        Some(sqlparser::ast::ReferentialAction::SetNull) => ForeignKeyAction::SetNull,
        Some(sqlparser::ast::ReferentialAction::SetDefault) => ForeignKeyAction::SetDefault,
        Some(sqlparser::ast::ReferentialAction::Restrict) => ForeignKeyAction::Restrict,
        Some(sqlparser::ast::ReferentialAction::NoAction) | None => ForeignKeyAction::NoAction,
    }
}

// ── CASCADE helpers (shared by view/drop) ───────────────────────────────────

/// Check whether `name` (possibly unqualified) resolves to any entry in
/// `dropped` (a set of fully-qualified names).  Tries all schemas in the
/// search path for unqualified names.
pub(super) fn was_cascade_dropped(
    name: &ObjectName,
    search_path: &[String],
    dropped: &HashSet<String>,
) -> bool {
    let (schema_opt, obj) = match names::split_object_name(name) {
        Ok(r) => r,
        Err(_) => return false,
    };
    match schema_opt {
        Some(schema) => names::ResolvedName::new(schema, obj)
            .map(|r| dropped.contains(&r.full))
            .unwrap_or(false),
        None => {
            let default: Vec<String> = vec!["public".to_string()];
            let schemas = if search_path.is_empty() {
                &default
            } else {
                search_path
            };
            schemas.iter().any(|schema| {
                names::ResolvedName::new(schema.clone(), obj.clone())
                    .map(|r| dropped.contains(&r.full))
                    .unwrap_or(false)
            })
        }
    }
}

/// Drop all views and materialized views that depend on `target_name`,
/// then transitively drop anything that depended on the dropped objects.
///
/// Uses the `deps` field stored in each ViewDef / MatViewDef to determine
/// dependencies — no SQL re-parsing needed at drop time.  Iterates until
/// a fixed point so transitive chains (table -> v1 -> mv1 -> v2) are fully
/// resolved.
///
/// The root `target_name` is never dropped here — the caller is responsible
/// for dropping it.  This prevents cycles (e.g. v1 <-> v2) from removing
/// the target before the caller can, which would cause a spurious
/// "does not exist" error (#639).
///
/// Returns the set of fully-qualified names that were dropped, so the
/// caller can skip names already handled in multi-name DROP (#640).
pub(super) async fn drop_dependent_views(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    target_name: &str,
) -> Result<HashSet<String>> {
    let mut dropped = HashSet::new();
    // Names pending dependency resolution; starts with the dropped object.
    let mut pending: Vec<String> = vec![target_name.to_string()];

    // Iterate until no new dependents are found (fixed-point).
    while !pending.is_empty() {
        let mut next_pending: Vec<String> = Vec::new();

        // --- regular views ---
        let views = store.list_views(txn, db_id).await?;
        for view in &views {
            let full = view.full_name();
            if full == target_name {
                continue;
            }
            if pending.iter().any(|p| view.deps.contains(p)) {
                store.drop_view(txn, db_id, &full).await?;
                dropped.insert(full.clone());
                next_pending.push(full);
            }
        }

        // --- materialized views ---
        let matviews = store.list_materialized_views(txn, db_id).await?;
        for mv in &matviews {
            let full = mv.full_name();
            if full == target_name {
                continue;
            }
            if pending.iter().any(|p| mv.deps.contains(p)) {
                store.drop_materialized_view(txn, db_id, &full).await?;
                for trigger in store.list_triggers_for_table(txn, db_id, &full).await? {
                    let _ = store.drop_trigger(txn, db_id, &full, &trigger.name).await?;
                }
                drop_owned_sequences_for_table(store, txn, db_id, &full).await?;
                store.drop_table(txn, db_id, &full).await?;
                dropped.insert(full.clone());
                next_pending.push(full);
            }
        }

        pending = next_pending;
    }

    Ok(dropped)
}
