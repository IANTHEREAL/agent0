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
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Once};

use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};
use crate::sql::analyzer::{Analyzer, Catalog, CatalogSnapshot, Scope};
use crate::sql::error::SqlError;
use crate::sql::expr::classify::is_volatile;
use crate::sql::expr::static_eval::{
    eval_static_typed_expr, is_row_dependent, needs_async_materialization,
};
use crate::sql::expr::traverse::visit_any;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::expr::typed_fold::fold_typed_expr;
use crate::sql::generated_columns::compile_generated_column;
use crate::sql::query_context::QueryContext;
use crate::sql::types::cast::CastContext;
use crate::sql::types::coercion::is_assignment_compatible;
use anyhow::{anyhow, Result};
use sqlparser::ast::{DataType as SqlDataType, Expr, ObjectName};
use tikv_client::{TimestampExt, Transaction};

use super::names;
use super::names::normalize_ident;
use super::sequences;
use super::types::sql_datatype_to_internal_strict;
use super::types::{resolve_custom_type_with_catalog, TypeResolutionContext};
use super::value_coercion::coerce_value_for_column;

use crate::model::{
    CheckConstraint, ColumnDef, DataType, ForeignKeyAction, ForeignKeyConstraint, IndexDef, Row,
    TableSchema, UserTypeKind, Value,
};
use crate::storage::TikvStore;
use crate::txn::txn_delete;

// ── Re-exports (preserve pub(crate) surface) ───────────────────────────────

pub use create_table::check_relation_name_available;
pub use create_table::create_table_from_query_result;
pub use create_table::create_table_from_select_into;
pub use create_table::create_table_from_stream;
pub use create_table::execute_create_table;
pub use create_table::RelationKind;
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
    validate_static_enum_subexpressions(&typed, &catalog, &qctx)?;
    Ok(fold_typed_expr(&typed, &qctx))
}

async fn build_udt_catalog_snapshot(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
) -> Result<CatalogSnapshot> {
    let mut catalog = CatalogSnapshot::new(search_path.to_vec(), db_id);
    for udt in store.list_types(txn, db_id).await? {
        catalog.add_schema(&udt.schema);
        let full_name = format!("{}.{}", udt.schema, udt.name);
        catalog.add_type(&full_name, udt);
    }
    Ok(catalog)
}

pub(crate) fn coerce_ddl_expr_to_column(
    expr: TypedExpr,
    col: &ColumnDef,
    expr_kind: &str,
) -> Result<TypedExpr> {
    if expr.data_type == col.data_type {
        return Ok(expr);
    }
    if expr.is_null_constant() {
        return Ok(TypedExpr::null(col.data_type.clone()));
    }
    if !is_assignment_compatible(&expr.data_type, &col.data_type) {
        return Err(SqlError::DataTypeMismatch {
            message: format!(
                "column \"{}\" is of type {} but {} is of type {}",
                col.name, col.data_type, expr_kind, expr.data_type
            ),
        }
        .into());
    }

    Ok(TypedExpr::new(
        TypedExprKind::Cast {
            expr: Box::new(expr),
            target_type: col.data_type.clone(),
            cast_context: CastContext::Assignment,
        },
        col.data_type.clone(),
    ))
}

fn enum_udt_full_name(data_type: &DataType) -> Option<&str> {
    match data_type {
        DataType::UserDefined(name) => Some(name.as_str()),
        DataType::Array(inner) => enum_udt_full_name(inner.as_ref()),
        _ => None,
    }
}

fn validate_static_enum_subexpressions(
    expr: &TypedExpr,
    catalog: &dyn Catalog,
    qctx: &QueryContext,
) -> Result<()> {
    let mut validation_err = None;

    visit_any(expr, |node| {
        let Some(full_name) = enum_udt_full_name(&node.data_type) else {
            return false;
        };
        let Ok((schema, type_name)) = names::parse_full_name(full_name) else {
            return false;
        };
        if is_row_dependent(node) || is_volatile(node) || needs_async_materialization(node) {
            return false;
        }

        let Some(def) = (match catalog.resolve_type(&type_name, Some(&schema)) {
            Ok(def) => def,
            Err(err) => {
                validation_err = Some(anyhow!(err.to_string()));
                return true;
            }
        }) else {
            return false;
        };
        let UserTypeKind::Enum { labels } = def.kind else {
            return false;
        };

        let value = match eval_static_typed_expr(node, qctx) {
            Ok(value) => value,
            Err(err) => {
                validation_err = Some(err);
                return true;
            }
        };
        let labels: HashSet<String> = labels.into_iter().collect();
        if let Err(err) = crate::sql::udt::validate_enum_value_against_labels(
            &node.data_type,
            &value,
            &labels,
            &type_name,
        ) {
            validation_err = Some(err);
            return true;
        }

        false
    });

    if let Some(err) = validation_err {
        return Err(err);
    }
    Ok(())
}

pub(super) async fn analyze_row_level_expr_with_udts(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    expr: &Expr,
    schema: &TableSchema,
    db_id: u64,
    search_path: &[String],
    collations: &[crate::sql::collation::CollationDef],
) -> Result<TypedExpr> {
    let mut catalog = build_udt_catalog_snapshot(store, txn, db_id, search_path).await?;

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
    validate_static_enum_subexpressions(&typed, &catalog, &qctx)?;
    Ok(fold_typed_expr(&typed, &qctx))
}

pub(super) async fn validate_column_default_expr(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    expr: &Expr,
    col: &ColumnDef,
    db_id: u64,
    search_path: &[String],
) -> Result<TypedExpr> {
    let catalog = build_udt_catalog_snapshot(store, txn, db_id, search_path).await?;
    let qctx = QueryContext::from_task_locals();
    let typed =
        Analyzer::analyze_expr_with_scope(&catalog, Scope::new(), expr).map_err(SqlError::from)?;
    let typed = coerce_ddl_expr_to_column(typed, col, "default expression")?;

    // Validate immutable enum-typed subexpressions with catalog label sets so
    // explicit casts like 'bogus'::mood fail during DDL, even when nested
    // inside a larger expression whose runtime evaluator preserves UDT text.
    validate_static_enum_subexpressions(&typed, &catalog, &qctx)?;
    let typed = fold_typed_expr(&typed, &qctx);

    // PostgreSQL validates constant default expressions at DDL time, but must
    // not execute volatile, async, or side-effecting defaults like nextval().
    if !is_volatile(&typed) && !needs_async_materialization(&typed) {
        let _ = eval_static_typed_expr(&typed, &qctx)?;
    }

    Ok(typed)
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
    resolve_nested_column_data_type(
        store,
        txn,
        db_id,
        search_path,
        sql_type,
        TypeResolutionContext::DdlColumn,
    )
    .await
}

pub(super) async fn resolve_alter_column_set_data_type(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    sql_type: &SqlDataType,
) -> Result<(DataType, bool)> {
    resolve_nested_column_data_type(
        store,
        txn,
        db_id,
        search_path,
        sql_type,
        TypeResolutionContext::DdlOther,
    )
    .await
}

#[allow(clippy::type_complexity)]
fn resolve_nested_column_data_type<'a>(
    store: &'a Arc<TikvStore>,
    txn: &'a mut Transaction,
    db_id: u64,
    search_path: &'a [String],
    sql_type: &'a SqlDataType,
    context: TypeResolutionContext,
) -> Pin<Box<dyn Future<Output = Result<(DataType, bool)>> + Send + 'a>> {
    Box::pin(async move {
        match sql_type {
            SqlDataType::Array(inner) => {
                let inner_type = match inner {
                    sqlparser::ast::ArrayElemTypeDef::AngleBracket(inner_type)
                    | sqlparser::ast::ArrayElemTypeDef::SquareBracket(inner_type) => {
                        // Array elements are never in DDL column context for serial expansion.
                        resolve_nested_column_data_type(
                            store,
                            txn,
                            db_id,
                            search_path,
                            inner_type,
                            TypeResolutionContext::DdlOther,
                        )
                        .await
                        .map_err(|e| {
                            crate::sql::types::wrap_undefined_object_for_sql_type(sql_type, e)
                        })?
                        .0
                    }
                    sqlparser::ast::ArrayElemTypeDef::None => DataType::Text,
                };
                Ok((DataType::Array(Box::new(inner_type)), false))
            }
            SqlDataType::Custom(name, modifiers) => {
                resolve_custom_type_with_catalog(
                    context,
                    store,
                    txn,
                    db_id,
                    search_path,
                    name,
                    modifiers,
                )
                .await
            }
            _ => Ok((sql_datatype_to_internal_strict(sql_type)?, false)),
        }
    })
}

pub(super) async fn validate_generated_column_expr(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    column_idx: usize,
) -> Result<Option<String>> {
    let col = schema
        .columns
        .get(column_idx)
        .ok_or_else(|| anyhow!("generated column index {} out of bounds", column_idx))?;
    if col.generation_expr.is_none() {
        return Ok(None);
    }

    let qctx = QueryContext::from_task_locals();
    let compiled = compile_generated_column(schema, column_idx, &qctx)?.ok_or_else(|| {
        anyhow!(
            "missing generated column expression for column \"{}\"",
            col.name
        )
    })?;

    if !compiled.uses_embedding {
        return Ok(None);
    }

    if store
        .get_extension(txn, db_id, "embedding")
        .await?
        .is_none()
    {
        return Err(crate::extensions::embedding::embedding_function_not_found(
            "embed_text(text, text)",
        ));
    }

    if !crate::extensions::context::is_superuser() {
        return Err(SqlError::PermissionDenied {
            object_type: "generated column".into(),
            object_name: format!("column \"{}\"", col.name),
        }
        .into());
    }

    Ok(Some(qctx.current_user.to_string()))
}

pub(super) async fn create_implicit_sequences_for_schema(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &mut TableSchema,
    exclude_table: Option<&str>,
) -> Result<()> {
    let schema_name = schema.name.clone();
    let mut any_updated = false;

    for col in &mut schema.columns {
        if col.is_serial {
            let seq_name = allocate_implicit_sequence_name(
                store,
                txn,
                db_id,
                &schema_name,
                &col.name,
                exclude_table,
            )
            .await?;
            let mut seq_def =
                sequences::build_implicit_sequence_def(&schema_name, &col.name, &col.data_type);
            seq_def.name = seq_name;
            let seq_full_name = seq_def.full_name();
            store.create_sequence(txn, db_id, seq_def).await?;
            // Persist explicit nextval default so classify_serial_default
            // can dispatch correctly after ALTER COLUMN SET/DROP DEFAULT.
            // Skip if default_expr is already set (identity markers, etc.).
            if col.default_expr.is_none() {
                col.default_expr = Some(sequences::format_nextval_default(&seq_full_name));
                any_updated = true;
            }
        }
    }
    if any_updated {
        schema.version += 1;
        store.update_schema(txn, db_id, schema.clone()).await?;
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
    let sequence_defs = store.list_sequences(txn, db_id).await?;
    let (table_schema, table_name) = schema
        .name
        .rsplit_once('.')
        .unwrap_or(("public", schema.name.as_str()));

    for col in &schema.columns {
        if !col.is_serial {
            continue;
        }

        let seq_full_name = match sequences::find_owned_sequence_full_name(
            &sequence_defs,
            &schema.name,
            &col.name,
        )? {
            Some(name) => name,
            None => format!(
                "{}.{}",
                table_schema,
                sequences::implicit_sequence_name(table_name, &col.name)
            ),
        };
        store
            .setval_sequence(txn, db_id, &seq_full_name, last_value, true)
            .await?;
    }

    Ok(())
}

pub(super) async fn allocate_implicit_sequence_name(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_full_name: &str,
    column_name: &str,
    exclude_table: Option<&str>,
) -> Result<String> {
    let (schema_name, table_name) = table_full_name
        .rsplit_once('.')
        .unwrap_or(("public", table_full_name));
    let base_name = sequences::implicit_sequence_name(table_name, column_name);

    // Use check_relation_name_available with if_not_exists=true so taken names
    // return Ok(false) without error. Ok(true) means available AND reservation
    // key written (TOCTOU-safe via TiKV pessimistic write-write detection).
    // Note: Ok(false) returns BEFORE reserve_relation_name — no orphaned keys.
    if create_table::check_relation_name_available(
        store,
        txn,
        db_id,
        schema_name,
        &base_name,
        create_table::RelationKind::Sequence,
        true,
        exclude_table,
    )
    .await?
    {
        return Ok(base_name);
    }

    for suffix in 1_u32..=u32::MAX {
        let candidate =
            sequences::implicit_sequence_name_with_suffix(table_name, column_name, suffix);
        if create_table::check_relation_name_available(
            store,
            txn,
            db_id,
            schema_name,
            &candidate,
            create_table::RelationKind::Sequence,
            true,
            exclude_table,
        )
        .await?
        {
            return Ok(candidate);
        }
    }

    Err(anyhow!(
        "could not allocate implicit sequence name for {}.{}",
        table_full_name,
        column_name
    ))
}

pub(super) async fn drop_owned_sequences_for_table(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
) -> Result<Vec<String>> {
    let seqs = store.list_sequences(txn, db_id).await?;
    let mut dropped = Vec::new();
    for def in seqs {
        let Some((owned_table, _)) = &def.owned_by else {
            continue;
        };
        if owned_table == table_name {
            let name = def.full_name();
            store.drop_sequence(txn, db_id, &name).await?;
            // Release unified namespace reservation key (no-op if missing).
            store.release_relation_name(txn, db_id, &name).await?;
            dropped.push(name);
        }
    }
    Ok(dropped)
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
    checks: &mut [CheckConstraint],
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
        if w.is_empty() || w.chars().next().is_none_or(|c| c.is_ascii_digit()) {
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
    txn_guard: &mut Option<crate::worker::active_txn_registry::ActiveTxnGuard>,
    current_batch_writes: &mut usize,
    has_committed_batches: &mut bool,
) -> Result<()> {
    if *current_batch_writes < DDL_BACKFILL_COMMIT_SIZE {
        return Ok(());
    }

    txn.commit().await?;
    *txn_guard = None;
    *txn = store.begin().await?;
    *txn_guard = track_active_worker_txn(txn);
    *current_batch_writes = 0;
    *has_committed_batches = true;
    Ok(())
}

pub(super) fn track_active_worker_txn(
    txn: &Transaction,
) -> Option<crate::worker::active_txn_registry::ActiveTxnGuard> {
    crate::worker::active_txn_registry::global_registry()
        .map(|registry| registry.track_worker_txn(txn.start_timestamp().version()))
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
/// Returns `(dropped_views, dropped_sequences)`:
/// - the set of fully-qualified view/matview names that were dropped, so the
///   caller can skip names already handled in multi-name DROP (#640).
/// - the list of fully-qualified sequence names that were dropped as a
///   side-effect of dropping materialized views that owned sequences.
pub(super) async fn drop_dependent_views(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    target_name: &str,
) -> Result<(HashSet<String>, Vec<String>)> {
    let mut dropped = HashSet::new();
    let mut dropped_sequences = Vec::new();
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
                store.release_relation_name(txn, db_id, &full).await?;
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
                let seqs = drop_owned_sequences_for_table(store, txn, db_id, &full).await?;
                dropped_sequences.extend(seqs);
                store.drop_table(txn, db_id, &full).await?;
                store.release_relation_name(txn, db_id, &full).await?;
                dropped.insert(full.clone());
                next_pending.push(full);
            }
        }

        pending = next_pending;
    }

    Ok((dropped, dropped_sequences))
}
