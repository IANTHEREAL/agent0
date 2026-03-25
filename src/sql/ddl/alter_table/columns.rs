//! Column-level ALTER TABLE operations: ADD COLUMN, DROP COLUMN, ALTER COLUMN SET DATA TYPE.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::ast::{ColumnOption, GeneratedAs, GeneratedExpressionMode};
use tikv_client::Transaction;

use crate::model::{DataType, IndexDef, Value};
use crate::sql::error::SqlError;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::generated_columns::compile_generated_column;
use crate::sql::names::normalize_ident;
use crate::sql::projection::fill_row_defaults;
use crate::sql::query_context::QueryContext;
use crate::sql::sequences;
use crate::sql::value_coercion::coerce_value_for_column;
use crate::storage::TikvStore;
use crate::txn::txn_put;

use super::super::{
    analyze_row_level_expr_with_udts, check_expr_references_column, coerce_value_for_type_change,
    eval_row_level_expr, index_prefix_range, resolve_alter_column_set_data_type,
    resolve_column_data_type, validate_column_default_expr, validate_generated_column_expr,
    AlterTableBudget, KvScanBatches, DDL_SCAN_BATCH_SIZE,
};
use super::{should_invalidate_stats_for_drop_column, should_invalidate_stats_for_type_change};

fn can_materialize_existing_rows_for_add_column(
    is_serial: bool,
    default_expr: Option<&str>,
    generation_expr: Option<&str>,
) -> bool {
    if crate::sql::sequences::is_serial_default_dropped_marker(default_expr) {
        return false;
    }

    is_serial || default_expr.is_some() || generation_expr.is_some()
}

fn generated_embed_add_column_nonempty_table_error(
    col_name: &str,
    table_name: &str,
) -> anyhow::Error {
    SqlError::Unsupported(format!(
        "cannot add generated column \"{}\" with EMBED_TEXT to non-empty table \"{}\"; use UPDATE after adding a regular/defaulted column",
        col_name, table_name
    ))
    .into()
}

/// ADD COLUMN: resolve type, validate NOT NULL + DEFAULT, append column, create
/// implicit sequence if serial.  Returns `true` when the schema was actually
/// mutated, `false` for a silent `IF NOT EXISTS` no-op.
pub(super) async fn alter_table_add_column(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    schema: &mut crate::model::TableSchema,
    column_def: &sqlparser::ast::ColumnDef,
    if_not_exists: bool,
) -> Result<bool> {
    let col_name = normalize_ident(&column_def.name);
    if schema.column_index(&col_name).is_some() {
        if if_not_exists {
            return Ok(false);
        }
        return Err(anyhow!("Column exists"));
    }
    let (data_type, mut is_serial) =
        resolve_column_data_type(store, txn, db_id, search_path, &column_def.data_type).await?;
    let mut nullable = true;
    let mut default_expr = None;
    let mut default_expr_ast = None;
    let mut identity_generated_as: Option<GeneratedAs> = None;
    let mut generation_expr_str: Option<String> = None;
    let mut generation_expr_authorized_by: Option<String> = None;
    for opt in &column_def.options {
        match &opt.option {
            ColumnOption::NotNull => nullable = false,
            ColumnOption::Default(expr) => {
                if generation_expr_str.is_some() {
                    return Err(SqlError::SqlStructure(format!(
                        "both default and generation expression specified for column \"{}\"",
                        col_name
                    ))
                    .into());
                }
                default_expr_ast = Some(expr.clone());
                default_expr = Some(expr.to_string());
            }
            ColumnOption::Generated {
                generated_as,
                generation_expr: None,
                ..
            } => {
                if matches!(generated_as, GeneratedAs::Always | GeneratedAs::ByDefault) {
                    is_serial = true;
                    identity_generated_as = Some(generated_as.clone());
                }
            }
            ColumnOption::Generated {
                generated_as: GeneratedAs::Always | GeneratedAs::ExpStored,
                generation_expr: Some(expr),
                ..
            } => {
                if default_expr.is_some() {
                    return Err(SqlError::SqlStructure(format!(
                        "both default and generation expression specified for column \"{}\"",
                        col_name
                    ))
                    .into());
                }
                generation_expr_str = Some(expr.to_string());
            }
            ColumnOption::Generated {
                generation_expr: Some(_),
                generation_expr_mode: Some(GeneratedExpressionMode::Virtual),
                ..
            } => {
                return Err(SqlError::Unsupported(format!(
                    "VIRTUAL generated columns are not supported; use STORED for column \"{}\"",
                    col_name
                ))
                .into());
            }
            _ => {}
        }
    }
    let table_name = schema
        .name
        .rsplit_once('.')
        .map_or(schema.name.as_str(), |(_, n)| n);
    if let Some(generated_as) = identity_generated_as.as_ref() {
        if default_expr.is_some() {
            return Err(SqlError::SqlStructure(format!(
                "both default and identity specified for column \"{}\" of table \"{}\"",
                col_name, table_name
            ))
            .into());
        }
        default_expr =
            crate::sql::sequences::identity_default_marker(generated_as).map(ToString::to_string);
    } else if is_serial && default_expr.is_some() {
        return Err(SqlError::SqlStructure(format!(
            "multiple default values specified for column \"{}\" of table \"{}\"",
            col_name, table_name
        ))
        .into());
    }
    if is_serial {
        nullable = false;
    }
    if !nullable
        && !can_materialize_existing_rows_for_add_column(
            is_serial,
            default_expr.as_deref(),
            generation_expr_str.as_deref(),
        )
    {
        let (start, end) = crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
        let range: tikv_client::BoundRange = (start..end).into();
        let existing_rows: Vec<_> = txn.scan(range, 1).await?.collect();
        if !existing_rows.is_empty() {
            return Err(anyhow!("Cannot add NOT NULL column without DEFAULT"));
        }
    }

    if generation_expr_str.is_some() {
        let mut candidate_schema = schema.clone();
        let mut candidate_col =
            crate::model::ColumnDef::new(col_name.clone(), data_type.clone(), nullable);
        candidate_col.is_serial = is_serial;
        candidate_col.default_expr = default_expr.clone();
        candidate_col.generation_expr = generation_expr_str.clone();
        candidate_schema.columns.push(candidate_col);
        let new_col_idx = candidate_schema.columns.len() - 1;
        generation_expr_authorized_by =
            validate_generated_column_expr(store, txn, db_id, &candidate_schema, new_col_idx)
                .await?;
        if generation_expr_authorized_by.is_some() {
            let (start, end) = crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
            let range: tikv_client::BoundRange = (start..end).into();
            let existing_rows: Vec<_> = txn.scan(range, 1).await?.collect();
            if !existing_rows.is_empty() {
                return Err(generated_embed_add_column_nonempty_table_error(
                    &col_name,
                    &schema.name,
                ));
            }
        }
    }

    let mut new_col = crate::model::ColumnDef::new(col_name, data_type, nullable);
    new_col.is_serial = is_serial;
    new_col.default_expr = default_expr;
    new_col.generation_expr = generation_expr_str;
    new_col.generation_expr_authorized_by = generation_expr_authorized_by;
    if let Some(default_expr_ast) = default_expr_ast.as_ref() {
        validate_column_default_expr(store, txn, default_expr_ast, &new_col, db_id, search_path)
            .await?;
    }
    schema.columns.push(new_col);
    if is_serial {
        let serial_col_name = schema
            .columns
            .last()
            .expect("column just pushed")
            .name
            .clone();
        let serial_col_type = schema
            .columns
            .last()
            .expect("column just pushed")
            .data_type
            .clone();
        let seq_name = super::super::allocate_implicit_sequence_name(
            store,
            txn,
            db_id,
            &schema.name,
            &serial_col_name,
            None,
        )
        .await?;
        let mut seq_def = sequences::build_implicit_sequence_def(
            &schema.name,
            &serial_col_name,
            &serial_col_type,
        );
        seq_def.name = seq_name;
        let seq_full_name = seq_def.full_name();
        store.create_sequence(txn, db_id, seq_def).await?;
        // Persist explicit nextval default (matching CREATE TABLE path).
        let last_col = schema.columns.last_mut().expect("column just pushed");
        if last_col.default_expr.is_none() {
            last_col.default_expr = Some(sequences::format_nextval_default(&seq_full_name));
        }

        let new_col_idx = schema.columns.len() - 1;
        let mut budget =
            AlterTableBudget::new(&schema.name, "ADD COLUMN (identity/serial backfill)");
        let (start, end) = crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
        let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
        while let Some(batch) = scanner.next_batch(txn).await? {
            for pair in batch {
                let (key, value): (tikv_client::Key, tikv_client::Value) = pair.into();
                let mut row = crate::storage::deserialize_row(&value)?;
                fill_row_defaults(&mut row, schema)?;
                let seq_val = store.nextval_sequence(txn, db_id, &seq_full_name).await?;
                row.values[new_col_idx] =
                    coerce_value_for_column(Value::Int64(seq_val), &schema.columns[new_col_idx])?;
                let row_data = crate::storage::serialize_row(&row)?;
                let key_vec: Vec<u8> = key.into();
                budget.track_write(key_vec.len(), row_data.len())?;
                txn_put(txn, key_vec, row_data).await?;
            }
        }
    }
    // Backfill existing rows for generated stored columns.
    if let Some(ref gen_expr_str) = schema
        .columns
        .last()
        .and_then(|c| c.generation_expr.clone())
    {
        let new_col_idx = schema.columns.len() - 1;
        let enum_validator = crate::sql::udt::load_enum_value_validator(
            store,
            txn,
            db_id,
            &schema.columns[new_col_idx].data_type,
        )
        .await?;
        let qctx = QueryContext::from_task_locals();
        let compiled = compile_generated_column(schema, new_col_idx, &qctx)?.ok_or_else(|| {
            anyhow!(
                "missing generated column expression for \"{}\"",
                gen_expr_str
            )
        })?;
        let mut budget =
            AlterTableBudget::new(&schema.name, "ADD COLUMN (generated column backfill)");
        let (start, end) = crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
        let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
        while let Some(batch) = scanner.next_batch(txn).await? {
            for pair in batch {
                let (key, value): (tikv_client::Key, tikv_client::Value) = pair.into();
                let mut row = crate::storage::deserialize_row(&value)?;
                fill_row_defaults(&mut row, schema)?;
                let val = if compiled.embedding_authorized {
                    crate::extensions::context::with_embedding_authorized(async {
                        eval_typed_expr(&compiled.expr, &row, &qctx)
                    })
                    .await??
                } else {
                    eval_typed_expr(&compiled.expr, &row, &qctx)?
                };
                row.values[new_col_idx] = crate::sql::udt::coerce_and_validate_value_for_column(
                    store,
                    txn,
                    db_id,
                    val,
                    &schema.columns[new_col_idx],
                    enum_validator.as_ref(),
                )
                .await?;
                let row_data = crate::storage::serialize_row(&row)?;
                let key_vec: Vec<u8> = key.into();
                budget.track_write(key_vec.len(), row_data.len())?;
                txn_put(txn, key_vec, row_data).await?;
            }
        }
    }
    schema.version += 1;
    store.update_schema(txn, db_id, schema.clone()).await?;
    Ok(true)
}

/// DROP COLUMN: validate dependencies, mark column as logically dropped.
///
/// Uses PostgreSQL-style `attisdropped` semantics: the column's physical
/// slot is preserved in existing rows but the column becomes invisible to
/// SQL queries. This is an O(1) metadata-only operation — no table rewrite.
/// Returns whether stats should be invalidated.
pub(super) async fn alter_table_drop_column(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &mut crate::model::TableSchema,
    column_name: &sqlparser::ast::Ident,
    if_exists: bool,
    cascade: bool,
) -> Result<bool> {
    if cascade {
        return Err(
            SqlError::Unsupported("DROP COLUMN ... CASCADE is not supported".into()).into(),
        );
    }

    let col_name = normalize_ident(column_name);
    let col_idx = schema.column_index(&col_name);
    let drop_changes_schema = should_invalidate_stats_for_drop_column(col_idx.is_some());
    match col_idx {
        Some(idx) => {
            if schema.pk_indices.contains(&idx) {
                return Err(anyhow!("Cannot drop primary key column '{}'", col_name));
            }
            for index in &schema.indexes {
                if index.columns.contains(&col_name) {
                    return Err(anyhow!(
                        "Cannot drop column '{}' used in index '{}'",
                        col_name,
                        index.name
                    ));
                }
                // Check expression indexes referencing this column.
                for expr_str in &index.expressions {
                    if check_expr_references_column(expr_str, &col_name)? {
                        return Err(anyhow!(
                            "Cannot drop column '{}' referenced in expression index '{}'",
                            col_name,
                            index.name
                        ));
                    }
                }
                // Check partial index predicates.
                if let Some(pred) = &index.predicate {
                    if check_expr_references_column(pred, &col_name)? {
                        return Err(anyhow!(
                            "Cannot drop column '{}' referenced in index predicate for '{}'",
                            col_name,
                            index.name
                        ));
                    }
                }
            }
            for fk in &schema.foreign_keys {
                if fk.columns.contains(&col_name) {
                    return Err(anyhow!(
                        "Cannot drop column '{}' used in foreign key '{}'",
                        col_name,
                        fk.name
                    ));
                }
            }
            for check in &schema.check_constraints {
                if check_expr_references_column(&check.expr, &col_name)? {
                    let name = check
                        .name
                        .as_deref()
                        .unwrap_or("<unnamed check constraint>");
                    return Err(anyhow!(
                        "Cannot drop column '{}' referenced by check constraint '{}'",
                        col_name,
                        name
                    ));
                }
            }
            // Check generated column dependencies.
            for (i, col) in schema.columns.iter().enumerate() {
                if i == idx || col.is_dropped {
                    continue;
                }
                if let Some(ref gen_expr) = col.generation_expr {
                    if check_expr_references_column(gen_expr, &col_name)? {
                        return Err(anyhow!(
                            "Cannot drop column '{}' because generated column '{}' depends on it",
                            col_name,
                            col.name
                        ));
                    }
                }
            }
            // Check view dependencies (RESTRICT semantics).
            // Parse view SQL to determine if the dropped column is actually used.
            let views = store.list_views(txn, db_id).await?;
            for view in &views {
                if view.deps.contains(&schema.name)
                    && view_sql_depends_on_column(&view.query, &schema.name, &col_name)
                {
                    return Err(anyhow!(
                        "cannot drop column \"{}\" of table \"{}\" because view \"{}\" depends on it",
                        col_name,
                        schema.name,
                        view.full_name()
                    ));
                }
            }
            // Same check for materialized views.
            let matviews = store.list_materialized_views(txn, db_id).await?;
            for mv in &matviews {
                if mv.deps.contains(&schema.name)
                    && view_sql_depends_on_column(&mv.query, &schema.name, &col_name)
                {
                    return Err(anyhow!(
                        "cannot drop column \"{}\" of table \"{}\" because materialized view \"{}\" depends on it",
                        col_name,
                        schema.name,
                        mv.full_name()
                    ));
                }
            }
            // Check RLS policy dependencies.
            let policies = store
                .list_policies_for_table(txn, db_id, schema.table_id)
                .await?;
            for policy in &policies {
                if let Some(using) = &policy.using_expr {
                    if check_expr_references_column(using, &col_name)? {
                        return Err(anyhow!(
                            "Cannot drop column '{}' referenced by RLS policy '{}'",
                            col_name,
                            policy.name
                        ));
                    }
                }
                if let Some(with_check) = &policy.with_check_expr {
                    if check_expr_references_column(with_check, &col_name)? {
                        return Err(anyhow!(
                            "Cannot drop column '{}' referenced by RLS policy '{}'",
                            col_name,
                            policy.name
                        ));
                    }
                }
            }

            // Drop ALL sequences owned by this column (not just serial ones).
            // Any sequence can be attached to any column via ALTER SEQUENCE ...
            // OWNED BY, not only implicit serial sequences. Matches the pattern
            // used by DROP TABLE at ddl/mod.rs:653 (drop_owned_sequences_for_table).
            {
                let seqs = store.list_sequences(txn, db_id).await?;
                for seq_def in &seqs {
                    let Some((owned_table, owned_col)) = &seq_def.owned_by else {
                        continue;
                    };
                    if owned_table == &schema.name && owned_col == &col_name {
                        let seq_name = seq_def.full_name();
                        store.drop_sequence(txn, db_id, &seq_name).await?;
                        store.release_relation_name(txn, db_id, &seq_name).await?;
                    }
                }
            }

            // Clear column-level comment so a future ADD COLUMN with the
            // same name doesn't inherit stale metadata.
            store
                .set_column_comment(txn, db_id, &schema.name, &col_name, None)
                .await?;

            // Logical drop: mark as dropped, preserve physical slot.
            // PostgreSQL uses tombstone names like "........pg.dropped.N........".
            // Clear all metadata so catalog views don't expose stale info.
            schema.columns[idx].is_dropped = true;
            schema.columns[idx].name = format!("........pg.dropped.{}........", idx + 1);
            schema.columns[idx].nullable = true;
            schema.columns[idx].is_serial = false;
            schema.columns[idx].unique = false;
            schema.columns[idx].default_expr = None;
            schema.columns[idx].generation_expr = None;
            schema.columns[idx].generation_expr_authorized_by = None;
            schema.version += 1;
            store.update_schema(txn, db_id, schema.clone()).await?;
            Ok(drop_changes_schema)
        }
        None => {
            if !if_exists {
                return Err(anyhow!("Column '{}' does not exist", col_name));
            }
            Ok(false)
        }
    }
}

/// ALTER COLUMN ... SET DATA TYPE: validate no PK/FK dependency, convert
/// existing data, rebuild affected indexes. Returns whether stats should
/// be invalidated.
pub(super) async fn alter_table_alter_column_set_data_type(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    schema: &mut crate::model::TableSchema,
    collations: &[crate::sql::collation::CollationDef],
    col_name: &str,
    col_idx: usize,
    data_type: &sqlparser::ast::DataType,
    using: &Option<sqlparser::ast::Expr>,
) -> Result<bool> {
    if schema.pk_indices.contains(&col_idx) {
        return Err(anyhow!(
            "Cannot alter type of primary key column '{}'",
            col_name
        ));
    }
    if schema
        .foreign_keys
        .iter()
        .any(|fk| fk.columns.contains(&col_name.to_string()))
    {
        return Err(anyhow!(
            "Cannot alter type of column '{}' used in foreign key",
            col_name
        ));
    }

    let (new_type, _) =
        resolve_alter_column_set_data_type(store, txn, db_id, search_path, data_type).await?;
    let type_changed =
        should_invalidate_stats_for_type_change(&schema.columns[col_idx].data_type, &new_type);
    if !type_changed {
        return Ok(false);
    }

    let affected_indexes: Vec<IndexDef> = schema
        .indexes
        .iter()
        .filter(|idx| idx.columns.contains(&col_name.to_string()))
        .cloned()
        .collect();

    // Validate USING expression BEFORE the rewrite safety guard so that
    // expression errors (bad column reference, type mismatch) are reported
    // instead of the generic "table too large" rejection.
    let mut target_col = schema.columns[col_idx].clone();
    target_col.data_type = new_type.clone();
    let enum_validator =
        crate::sql::udt::load_enum_value_validator(store, txn, db_id, &target_col.data_type)
            .await?;
    let typed_using_expr = if let Some(using_expr) = &using {
        Some(
            analyze_row_level_expr_with_udts(
                store,
                txn,
                using_expr,
                schema,
                db_id,
                search_path,
                collations,
            )
            .await?,
        )
    } else {
        None
    };

    let mut budget = AlterTableBudget::new(&schema.name, "ALTER COLUMN SET DATA TYPE");

    // Delete old index entries (count toward budget).
    for idx in &affected_indexes {
        let (idx_start, idx_end) = index_prefix_range(db_id, schema.table_id, idx.id);
        let mut idx_scanner = KvScanBatches::new(idx_start, idx_end, DDL_SCAN_BATCH_SIZE);
        while let Some(idx_batch) = idx_scanner.next_batch(txn).await? {
            for pair in &idx_batch {
                let k: &[u8] = pair.key().as_ref().into();
                budget.track_delete(k.len())?;
            }
            for pair in idx_batch {
                crate::txn::txn_delete(txn, pair.into_key().into()).await?;
            }
        }
    }

    let qctx = QueryContext::from_task_locals();

    let (start, end) = crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
    let data_key_prefix = start.clone();
    let pk_types: Vec<DataType> = if schema.pk_indices.is_empty() {
        vec![DataType::Uuid]
    } else {
        schema
            .pk_indices
            .iter()
            .map(|&idx| schema.columns[idx].data_type.clone())
            .collect()
    };
    let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
    while let Some(batch) = scanner.next_batch(txn).await? {
        for pair in batch {
            let (key, value): (tikv_client::Key, tikv_client::Value) = pair.into();
            let mut row = crate::storage::deserialize_row(&value)?;
            fill_row_defaults(&mut row, schema)?;

            let new_val = if let Some(using_expr) = &typed_using_expr {
                let result = eval_row_level_expr(using_expr, &row, &qctx)?;
                crate::sql::udt::coerce_and_validate_value_for_column(
                    store,
                    txn,
                    db_id,
                    result,
                    &target_col,
                    enum_validator.as_ref(),
                )
                .await?
            } else {
                let old_val = std::mem::replace(&mut row.values[col_idx], Value::Null);
                let coerced = coerce_value_for_type_change(old_val, &target_col)?;
                if let Some(validator) = &enum_validator {
                    validator.validate(&coerced)?;
                }
                coerced
            };
            row.values[col_idx] = new_val;

            let pk_values = if schema.pk_indices.is_empty() {
                let key_bytes: &[u8] = key.as_ref().into();
                let pk_bytes = key_bytes
                    .strip_prefix(data_key_prefix.as_slice())
                    .ok_or_else(|| {
                        anyhow!(
                            "corrupted row key while rebuilding indexes for '{}'",
                            schema.name
                        )
                    })?;
                crate::storage::decode_pk_from_index_suffix(pk_bytes, &pk_types)?
            } else {
                schema.get_pk_values(&row)
            };
            for idx in &affected_indexes {
                let idx_values = schema.get_index_values(idx, &row);
                let idx_bytes = store
                    .create_index_entry(
                        txn,
                        db_id,
                        schema.table_id,
                        idx.id,
                        &idx_values,
                        &pk_values,
                        idx.unique,
                    )
                    .await?;
                budget.track_write(idx_bytes, 0)?;
            }

            let row_data = crate::storage::serialize_row(&row)?;
            let key_vec: Vec<u8> = key.into();
            budget.track_write(key_vec.len(), row_data.len())?;
            txn_put(txn, key_vec, row_data).await?;
        }
    }

    schema.columns[col_idx].data_type = new_type;
    schema.version += 1;
    store.update_schema(txn, db_id, schema.clone()).await?;
    Ok(type_changed)
}

/// Check whether a view's SQL query depends on a specific column of a
/// specific table.
///
/// Resolves table aliases from FROM clauses so that qualified references
/// like `u.col` are only matched when the qualifier resolves to our table.
/// This avoids false positives in multi-table views where another table
/// happens to have a column with the same name.
///
/// Phase 1: Walk SELECT-item projections for `SELECT *` / `t.*` (not
///   function-arg wildcards like `count(*)`). Bare `*` only matches if
///   the SELECT's FROM clause references our table. `t.*` only matches
///   if `t` resolves to our table.
/// Phase 2: Walk all expressions and join constraints for identifier
///   matches on `col_name`.
///   Qualified `t.col` only matches if `t` resolves to our table.
///   Bare `col` still matches conservatively (unknown table).
fn view_sql_depends_on_column(view_sql: &str, table_full_name: &str, col_name: &str) -> bool {
    use sqlparser::ast::{
        Expr as AstExpr, GroupByExpr, NamedWindowDefinition, ObjectName, Offset, Query, Select,
        SelectItem, SetExpr, Statement as SqlStatement, TableFactor, TableWithJoins, Visit,
        Visitor,
    };
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    let dialect = PostgreSqlDialect {};
    let stmts = match Parser::parse_sql(&dialect, view_sql) {
        Ok(s) => s,
        Err(_) => return true, // parse failure → conservative block
    };

    // Extract the bare table name from a potentially schema-qualified name.
    // "public.users" → "users", "users" → "users".
    let (_, target_table) = table_full_name
        .rsplit_once('.')
        .unwrap_or(("", table_full_name));
    let target_full_lower = table_full_name.to_lowercase();
    let target_lower = target_table.to_lowercase();

    // Check if an ObjectName (e.g. `public.users` or `users`) matches
    // the target table.
    fn object_name_matches(name: &ObjectName, target_full: &str, target_bare: &str) -> bool {
        let parts: Vec<String> = name
            .0
            .iter()
            .map(crate::sql::names::normalize_ident)
            .collect();
        match parts.as_slice() {
            [] => false,
            [table] => table == target_bare,
            [schema, table] => format!("{schema}.{table}") == target_full,
            _ => {
                let schema = &parts[parts.len() - 2];
                let table = &parts[parts.len() - 1];
                format!("{schema}.{table}") == target_full
            }
        }
    }

    // Collect aliases that resolve to our table from a FROM clause.
    // Returns the set of names (aliases + bare table name) that refer to
    // our table within this SELECT scope.
    fn our_table_names(
        from: &[TableWithJoins],
        target_full: &str,
        target_bare: &str,
    ) -> Vec<String> {
        let mut names = Vec::new();
        fn visit_factor(
            factor: &TableFactor,
            target_full: &str,
            target_bare: &str,
            names: &mut Vec<String>,
        ) {
            match factor {
                TableFactor::Table { name, alias, .. } => {
                    if object_name_matches(name, target_full, target_bare) {
                        // The bare table name itself.
                        if let Some(last) = name.0.last() {
                            names.push(crate::sql::names::normalize_ident(last));
                        }
                        // Its alias, if any.
                        if let Some(a) = alias {
                            names.push(crate::sql::names::normalize_ident(&a.name));
                        }
                    }
                }
                TableFactor::NestedJoin {
                    table_with_joins, ..
                } => {
                    visit_twj(table_with_joins, target_full, target_bare, names);
                }
                _ => {}
            }
        }
        fn visit_twj(
            twj: &TableWithJoins,
            target_full: &str,
            target_bare: &str,
            names: &mut Vec<String>,
        ) {
            visit_factor(&twj.relation, target_full, target_bare, names);
            for join in &twj.joins {
                visit_factor(&join.relation, target_full, target_bare, names);
            }
        }
        for twj in from {
            visit_twj(twj, target_full, target_bare, &mut names);
        }
        names
    }

    /// Check whether any expression inside `v` references `col_lower`,
    /// using `scope_names` for qualifying `table.column` identifiers.
    /// Subquery boundaries are respected: when a `Query` node is reached
    /// the visitor delegates to `query_depends_on_column` (which builds
    /// a fresh scope from the inner FROM clause) and skips the subtree to
    /// prevent outer scope names from leaking in.
    fn visit_depends_on_column<V: Visit>(
        v: &V,
        scope_names: &[String],
        target_full: &str,
        target_bare: &str,
        col_lower: &str,
    ) -> bool {
        use std::ops::ControlFlow;

        struct DepCheck<'a> {
            scope_names: &'a [String],
            target_full: &'a str,
            target_bare: &'a str,
            col_lower: &'a str,
            /// Depth counter: > 0 means we are inside a subquery whose
            /// expressions have already been (or will be) checked with a
            /// fresh scope by `query_depends_on_column`.  While > 0 we
            /// must ignore every `Expr` the visitor encounters so that
            /// the outer `scope_names` is not applied to inner identifiers.
            skip_depth: usize,
        }

        impl Visitor for DepCheck<'_> {
            type Break = (); // `Break(())` ≡ "found a dependency"

            fn pre_visit_expr(&mut self, e: &AstExpr) -> ControlFlow<()> {
                if self.skip_depth > 0 {
                    // Inside a subquery handled by query_depends_on_column;
                    // do not check expressions with the outer scope.
                    return ControlFlow::Continue(());
                }
                match e {
                    // Bare column reference — conservatively matches any
                    // same-name column regardless of qualifier.
                    AstExpr::Identifier(ident) => {
                        if crate::sql::names::normalize_ident(ident) == self.col_lower {
                            return ControlFlow::Break(());
                        }
                    }
                    // Qualified column reference — only matches when the
                    // qualifier resolves to our target table.
                    AstExpr::CompoundIdentifier(parts) if parts.len() >= 2 => {
                        let col = crate::sql::names::normalize_ident(parts.last().unwrap());
                        if col == self.col_lower {
                            let qualifier =
                                crate::sql::names::normalize_ident(&parts[parts.len() - 2]);
                            if self.scope_names.iter().any(|s| s == &qualifier) {
                                return ControlFlow::Break(());
                            }
                        }
                    }
                    // All other expressions: the visitor recurses automatically.
                    // Subquery expressions (InSubquery, Exists, Subquery,
                    // ArraySubquery) contain Query nodes that are handled by
                    // pre_visit_query with scope-isolated recursion.
                    _ => {}
                }
                ControlFlow::Continue(())
            }

            fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<()> {
                // Every Query node (Expr-level subqueries AND TableFactor::Derived)
                // needs scope-isolated handling via query_depends_on_column.
                // We handle it here and skip the visitor's own recursion.
                if self.skip_depth == 0
                    && query_depends_on_column(
                        q,
                        self.target_full,
                        self.target_bare,
                        self.col_lower,
                    )
                {
                    return ControlFlow::Break(());
                }
                // Skip all inner nodes — query_depends_on_column already
                // recursed with proper scope.
                self.skip_depth += 1;
                ControlFlow::Continue(())
            }

            fn post_visit_query(&mut self, _q: &Query) -> ControlFlow<()> {
                self.skip_depth -= 1;
                ControlFlow::Continue(())
            }
        }

        let mut checker = DepCheck {
            scope_names,
            target_full,
            target_bare,
            col_lower,
            skip_depth: 0,
        };
        matches!(v.visit(&mut checker), ControlFlow::Break(()))
    }

    /// Thin wrapper: check a single expression tree for column dependency.
    fn expr_depends_on_column(
        expr: &AstExpr,
        scope_names: &[String],
        target_full: &str,
        target_bare: &str,
        col_lower: &str,
    ) -> bool {
        visit_depends_on_column(expr, scope_names, target_full, target_bare, col_lower)
    }

    fn join_constraint_depends_on_column(
        join_op: &sqlparser::ast::JoinOperator,
        scope_names: &[String],
        target_full: &str,
        target_bare: &str,
        col_lower: &str,
    ) -> bool {
        let constraint = match join_op {
            sqlparser::ast::JoinOperator::Inner(c)
            | sqlparser::ast::JoinOperator::LeftOuter(c)
            | sqlparser::ast::JoinOperator::RightOuter(c)
            | sqlparser::ast::JoinOperator::FullOuter(c)
            | sqlparser::ast::JoinOperator::LeftSemi(c)
            | sqlparser::ast::JoinOperator::RightSemi(c)
            | sqlparser::ast::JoinOperator::LeftAnti(c)
            | sqlparser::ast::JoinOperator::RightAnti(c) => c,
            sqlparser::ast::JoinOperator::CrossJoin
            | sqlparser::ast::JoinOperator::CrossApply
            | sqlparser::ast::JoinOperator::OuterApply => return false,
        };
        match constraint {
            sqlparser::ast::JoinConstraint::On(expr) => {
                expr_depends_on_column(expr, scope_names, target_full, target_bare, col_lower)
            }
            sqlparser::ast::JoinConstraint::Using(idents) => idents
                .iter()
                .any(|ident| crate::sql::names::normalize_ident(ident) == col_lower),
            sqlparser::ast::JoinConstraint::Natural => {
                // NATURAL JOIN behaves like USING on the shared column set.
                // Without catalog access here, conservatively block any
                // column drop from a referenced table participating in it.
                true
            }
            sqlparser::ast::JoinConstraint::None => false,
        }
    }

    fn select_depends_on_column(
        select: &Select,
        target_full: &str,
        target_bare: &str,
        col_lower: &str,
    ) -> bool {
        let scope_names = our_table_names(&select.from, target_full, target_bare);
        let contains_target = !scope_names.is_empty();

        if contains_target {
            for item in &select.projection {
                match item {
                    SelectItem::Wildcard(_) => return true,
                    SelectItem::QualifiedWildcard(obj_name, _) => {
                        if let Some(qualifier) = obj_name.0.last() {
                            let q = crate::sql::names::normalize_ident(qualifier);
                            if scope_names.contains(&q) {
                                return true;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // Check expressions inside FROM items (table function args,
        // derived subqueries, UNNEST, etc.) with empty scope — identifiers
        // in the FROM clause itself cannot reference sibling FROM aliases.
        // Also check join constraints (USING, NATURAL, ON).
        select.from.iter().any(|twj| {
            let twj_scope = our_table_names(std::slice::from_ref(twj), target_full, target_bare);
            // Visit all expressions in table factors (empty scope).
            // The visitor automatically handles derived subqueries via
            // pre_visit_query → query_depends_on_column.
            visit_depends_on_column(&twj.relation, &[], target_full, target_bare, col_lower)
                || twj.joins.iter().any(|join| {
                    visit_depends_on_column(
                        &join.relation,
                        &[],
                        target_full,
                        target_bare,
                        col_lower,
                    ) || join_constraint_depends_on_column(
                        &join.join_operator,
                        &twj_scope,
                        target_full,
                        target_bare,
                        col_lower,
                    )
                })
        }) || select.lateral_views.iter().any(|lv| {
            expr_depends_on_column(
                &lv.lateral_view,
                &scope_names,
                target_full,
                target_bare,
                col_lower,
            )
        }) || select.selection.as_ref().is_some_and(|expr| {
            expr_depends_on_column(expr, &scope_names, target_full, target_bare, col_lower)
        }) || select.projection.iter().any(|item| match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                expr_depends_on_column(expr, &scope_names, target_full, target_bare, col_lower)
            }
            SelectItem::QualifiedWildcard(_, _) | SelectItem::Wildcard(_) => false,
        }) || matches!(&select.group_by, GroupByExpr::Expressions(exprs) if exprs.iter().any(|expr| {
            expr_depends_on_column(expr, &scope_names, target_full, target_bare, col_lower)
        })) || select.cluster_by.iter().any(|expr| {
            expr_depends_on_column(expr, &scope_names, target_full, target_bare, col_lower)
        }) || select.distribute_by.iter().any(|expr| {
            expr_depends_on_column(expr, &scope_names, target_full, target_bare, col_lower)
        }) || select.sort_by.iter().any(|expr| {
            expr_depends_on_column(expr, &scope_names, target_full, target_bare, col_lower)
        }) || select.having.as_ref().is_some_and(|expr| {
            expr_depends_on_column(expr, &scope_names, target_full, target_bare, col_lower)
        }) || select
            .named_window
            .iter()
            .any(|NamedWindowDefinition(_, spec)| {
                spec.partition_by.iter().any(|expr| {
                    expr_depends_on_column(expr, &scope_names, target_full, target_bare, col_lower)
                }) || spec.order_by.iter().any(|ob| {
                    expr_depends_on_column(
                        &ob.expr,
                        &scope_names,
                        target_full,
                        target_bare,
                        col_lower,
                    )
                })
            })
            || select.qualify.as_ref().is_some_and(|expr| {
                expr_depends_on_column(expr, &scope_names, target_full, target_bare, col_lower)
            })
    }

    fn set_expr_depends_on_column(
        body: &SetExpr,
        target_full: &str,
        target_bare: &str,
        col_lower: &str,
    ) -> bool {
        match body {
            SetExpr::Select(select) => {
                select_depends_on_column(select, target_full, target_bare, col_lower)
            }
            SetExpr::Query(q) => query_depends_on_column(q, target_full, target_bare, col_lower),
            SetExpr::SetOperation { left, right, .. } => {
                set_expr_depends_on_column(left, target_full, target_bare, col_lower)
                    || set_expr_depends_on_column(right, target_full, target_bare, col_lower)
            }
            SetExpr::Values(values) => {
                values.rows.iter().flatten().any(|expr| {
                    expr_depends_on_column(expr, &[], target_full, target_bare, col_lower)
                })
            }
            SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Table(_) => false,
        }
    }

    fn query_depends_on_column(
        q: &Query,
        target_full: &str,
        target_bare: &str,
        col_lower: &str,
    ) -> bool {
        if let Some(ref with) = q.with {
            for cte in &with.cte_tables {
                if query_depends_on_column(&cte.query, target_full, target_bare, col_lower) {
                    return true;
                }
            }
        }
        set_expr_depends_on_column(&q.body, target_full, target_bare, col_lower)
            || q.order_by.iter().any(|ob| {
                expr_depends_on_column(&ob.expr, &[], target_full, target_bare, col_lower)
            })
            || q.limit.as_ref().is_some_and(|expr| {
                expr_depends_on_column(expr, &[], target_full, target_bare, col_lower)
            })
            || q.limit_by
                .iter()
                .any(|expr| expr_depends_on_column(expr, &[], target_full, target_bare, col_lower))
            || q.offset.as_ref().is_some_and(|Offset { value, .. }| {
                expr_depends_on_column(value, &[], target_full, target_bare, col_lower)
            })
            || q.fetch.as_ref().is_some_and(|fetch| {
                fetch.quantity.as_ref().is_some_and(|expr| {
                    expr_depends_on_column(expr, &[], target_full, target_bare, col_lower)
                })
            })
    }

    let col_lower = col_name.to_lowercase();
    for stmt in &stmts {
        if let SqlStatement::Query(q) = stmt {
            if query_depends_on_column(q, &target_full_lower, &target_lower, &col_lower) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{
        can_materialize_existing_rows_for_add_column,
        generated_embed_add_column_nonempty_table_error, view_sql_depends_on_column,
    };
    use crate::sql::error::SqlError;

    #[test]
    fn add_column_gate_accepts_identity_backfill_path() {
        assert!(can_materialize_existing_rows_for_add_column(
            true,
            Some("NULL /* db9_identity_by_default */"),
            None,
        ));
    }

    #[test]
    fn add_column_gate_accepts_generated_column_backfill_path() {
        assert!(can_materialize_existing_rows_for_add_column(
            false,
            None,
            Some("(x * 10)"),
        ));
    }

    #[test]
    fn add_column_gate_rejects_plain_not_null_column_without_value_path() {
        assert!(!can_materialize_existing_rows_for_add_column(
            false, None, None
        ));
    }

    #[test]
    fn add_column_gate_rejects_serial_default_drop_marker() {
        assert!(!can_materialize_existing_rows_for_add_column(
            false,
            Some("NULL /* db9_serial_default_dropped */"),
            None,
        ));
    }

    #[test]
    fn generated_embed_add_column_nonempty_table_error_is_unsupported() {
        let err = generated_embed_add_column_nonempty_table_error("vec", "public.docs");
        let sql = err
            .downcast_ref::<SqlError>()
            .expect("must be backed by SqlError");
        assert_eq!(sql.sqlstate(), "0A000");
        assert!(sql
            .to_string()
            .contains("cannot add generated column \"vec\" with EMBED_TEXT"));
    }

    #[test]
    fn view_dep_check_ignores_shadowed_alias_in_nested_subquery() {
        let sql =
            "SELECT 1 FROM public.users u WHERE EXISTS (SELECT 1 FROM orders u WHERE u.age > 0)";
        assert!(!view_sql_depends_on_column(sql, "public.users", "age"));
    }

    #[test]
    fn view_dep_check_ignores_other_schema_same_table_name() {
        let sql = "SELECT x.age FROM other.users x JOIN public.users u ON true";
        assert!(!view_sql_depends_on_column(sql, "public.users", "age"));
    }

    #[test]
    fn view_dep_check_still_matches_target_table_alias() {
        let sql = "SELECT u.age FROM public.users u";
        assert!(view_sql_depends_on_column(sql, "public.users", "age"));
    }

    #[test]
    fn view_dep_check_matches_join_using_column() {
        let sql = "SELECT 1 FROM public.users u JOIN public.orders o USING (age)";
        assert!(view_sql_depends_on_column(sql, "public.users", "age"));
    }

    #[test]
    fn view_dep_check_blocks_natural_join_conservatively() {
        let sql = "SELECT 1 FROM public.users u NATURAL JOIN public.orders o";
        assert!(view_sql_depends_on_column(sql, "public.users", "age"));
    }

    #[test]
    fn view_dep_check_matches_derived_subquery_in_from() {
        // Derived subquery (TableFactor::Derived) references the column.
        let sql = "SELECT d.age FROM (SELECT age FROM public.users) d";
        assert!(view_sql_depends_on_column(sql, "public.users", "age"));
    }

    #[test]
    fn view_dep_check_matches_join_on_clause() {
        // ON clause of a JOIN references the column.
        let sql = "SELECT 1 FROM public.users u JOIN public.orders o ON u.age = o.val";
        assert!(view_sql_depends_on_column(sql, "public.users", "age"));
    }
}
