//! Column-level ALTER TABLE operations: ADD COLUMN, DROP COLUMN, ALTER COLUMN SET DATA TYPE.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::ast::{ColumnOption, GeneratedAs};
use tikv_client::Transaction;

use crate::sql::error::SqlError;
use crate::sql::names::normalize_ident;
use crate::sql::projection::fill_row_defaults;
use crate::sql::query_context::QueryContext;
use crate::sql::sequences;
use crate::sql::value_coercion::coerce_value_for_column;
use crate::storage::TikvStore;
use crate::txn::txn_put;
use crate::types::{DataType, IndexDef, Value};

use super::super::{
    analyze_row_level_expr, check_expr_references_column, coerce_value_for_type_change,
    delete_range, eval_row_level_expr, index_prefix_range, resolve_column_data_type, KvScanBatches,
    DDL_SCAN_BATCH_SIZE,
};
use super::{should_invalidate_stats_for_drop_column, should_invalidate_stats_for_type_change};

/// ADD COLUMN: resolve type, validate NOT NULL + DEFAULT, append column, create
/// implicit sequence if serial.
pub(super) async fn alter_table_add_column(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    schema: &mut crate::types::TableSchema,
    column_def: &sqlparser::ast::ColumnDef,
) -> Result<()> {
    let col_name = normalize_ident(&column_def.name);
    if schema.column_index(&col_name).is_some() {
        return Err(anyhow!("Column exists"));
    }
    let (data_type, mut is_serial) =
        resolve_column_data_type(store, txn, db_id, search_path, &column_def.data_type).await?;
    let mut nullable = true;
    let mut default_expr = None;
    for opt in &column_def.options {
        match &opt.option {
            ColumnOption::NotNull => nullable = false,
            ColumnOption::Default(expr) => default_expr = Some(expr.to_string()),
            ColumnOption::Generated {
                generated_as,
                generation_expr: None,
                ..
            } => {
                if matches!(generated_as, GeneratedAs::Always | GeneratedAs::ByDefault) {
                    is_serial = true;
                }
            }
            _ => {}
        }
    }
    if is_serial {
        nullable = false;
    }
    if !nullable && default_expr.is_none() {
        let (start, end) = crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
        let range: tikv_client::BoundRange = (start..end).into();
        let existing_rows: Vec<_> = txn.scan(range, 1).await?.collect();
        if !existing_rows.is_empty() {
            return Err(anyhow!("Cannot add NOT NULL column without DEFAULT"));
        }
    }
    schema.columns.push(crate::types::ColumnDef {
        name: col_name,
        data_type,
        nullable,
        primary_key: false,
        unique: false,
        is_serial,
        default_expr,
        collation: None,
    });
    if is_serial {
        store
            .create_sequence(
                txn,
                db_id,
                sequences::build_implicit_sequence_def(
                    &schema.name,
                    schema
                        .columns
                        .last()
                        .expect("column just pushed")
                        .name
                        .as_str(),
                    &schema.columns.last().expect("column just pushed").data_type,
                ),
            )
            .await?;
    }
    schema.version += 1;
    store.update_schema(txn, db_id, schema.clone()).await?;
    Ok(())
}

/// DROP COLUMN: validate dependencies, rewrite rows, remove column from schema.
/// Returns whether stats should be invalidated.
pub(super) async fn alter_table_drop_column(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &mut crate::types::TableSchema,
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

            let (start, end) = crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
            let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
            while let Some(batch) = scanner.next_batch(txn).await? {
                for pair in batch {
                    let (key, value): (tikv_client::Key, tikv_client::Value) = pair.into();
                    let mut row = crate::storage::deserialize_row(&value)?;
                    fill_row_defaults(&mut row, schema)?;
                    row.values.remove(idx);
                    let row_data = crate::storage::serialize_row(&row)?;
                    txn_put(txn, key.into(), row_data).await?;
                }
            }
            schema.columns.remove(idx);
            for pk_idx in &mut schema.pk_indices {
                if *pk_idx > idx {
                    *pk_idx -= 1;
                }
            }
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
    schema: &mut crate::types::TableSchema,
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

    let (new_type, _) = resolve_column_data_type(store, txn, db_id, search_path, data_type).await?;
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

    for idx in &affected_indexes {
        let (start, end) = index_prefix_range(db_id, schema.table_id, idx.id);
        delete_range(txn, start, end).await?;
    }

    let mut target_col = schema.columns[col_idx].clone();
    target_col.data_type = new_type.clone();
    let typed_using_expr = if let Some(using_expr) = &using {
        Some(analyze_row_level_expr(
            using_expr,
            schema,
            db_id,
            search_path,
            collations,
        )?)
    } else {
        None
    };
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
                coerce_value_for_column(result, &target_col)?
            } else {
                let old_val = std::mem::replace(&mut row.values[col_idx], Value::Null);
                coerce_value_for_type_change(old_val, &target_col)?
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
                store
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
            }

            let row_data = crate::storage::serialize_row(&row)?;
            txn_put(txn, key.into(), row_data).await?;
        }
    }

    schema.columns[col_idx].data_type = new_type;
    schema.version += 1;
    store.update_schema(txn, db_id, schema.clone()).await?;
    Ok(type_changed)
}
