//! ALTER TABLE and all sub-operations (ADD/DROP COLUMN, ADD/DROP CONSTRAINT,
//! RENAME COLUMN/TABLE/CONSTRAINT, ALTER COLUMN SET/DROP DEFAULT/NOT NULL/TYPE).

mod columns;
mod constraints;

use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::ast::{AlterColumnOperation, AlterTableOperation, TableConstraint};
use tikv_client::Transaction;

use crate::model::{DataType, Value};
use crate::sql::error::SqlError;
use crate::sql::names;
use crate::sql::names::normalize_ident;
use crate::sql::projection::fill_row_defaults;
use crate::sql::ExecuteResult;
use crate::storage::TikvStore;

use super::{
    assign_generated_check_constraint_names, check_expr_references_column, constraint_name_exists,
    find_check_constraint_index, rewrite_check_expr_column, KvScanBatches, DDL_SCAN_BATCH_SIZE,
};

use columns::{
    alter_table_add_column, alter_table_alter_column_set_data_type, alter_table_drop_column,
};
use constraints::{
    alter_table_add_check_constraint, alter_table_add_foreign_key, alter_table_add_primary_key,
    alter_table_add_unique_constraint, alter_table_drop_constraint,
};

/// Execute an ALTER TABLE operation.
///
/// Returns `(result, invalidate_table_id)` -- `invalidate_table_id` is
/// `Some(table_id)` when the operation structurally changed the table in
/// a way that invalidates ANALYZE statistics (column added/dropped/renamed/
/// retyped), `None` otherwise.
pub async fn execute_alter_table(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    name: &sqlparser::ast::ObjectName,
    operation: &AlterTableOperation,
) -> Result<(ExecuteResult, Option<u64>)> {
    let resolved =
        names::resolve_existing_table_name(store.as_ref(), txn, db_id, name, search_path)
            .await?
            .ok_or_else(|| anyhow!("Table '{}' does not exist", name))?;
    let table_object_name = resolved.name.clone();
    let t = resolved.full;
    let mut result_table_name = t.clone();
    let mut schema = store
        .get_schema(txn, db_id, &t)
        .await?
        .ok_or_else(|| anyhow!("Table '{}' does not exist", t))?;

    assign_generated_check_constraint_names(
        &table_object_name,
        !schema.pk_indices.is_empty(),
        &schema.indexes,
        &schema.foreign_keys,
        &mut schema.check_constraints,
    );

    let table_id = schema.table_id;
    // Set to true at mutation points that structurally change the table
    // (column added/dropped/renamed/retyped) to signal stats invalidation.
    let mut invalidate_stats = false;

    // Prefetch user-defined collations for row-level expression analysis.
    let collations = store.list_collations(txn, db_id).await?;

    match operation {
        AlterTableOperation::AddColumn {
            column_def,
            if_not_exists,
            ..
        } => {
            alter_table_add_column(
                store,
                txn,
                db_id,
                search_path,
                &mut schema,
                column_def,
                *if_not_exists,
            )
            .await?;
            invalidate_stats = true;
        }
        AlterTableOperation::AddConstraint(constraint) => match constraint {
            TableConstraint::Unique {
                name,
                columns,
                is_primary,
                ..
            } if *is_primary => {
                alter_table_add_primary_key(
                    store,
                    txn,
                    db_id,
                    &mut schema,
                    &table_object_name,
                    &t,
                    name,
                    columns,
                )
                .await?;
            }
            TableConstraint::Unique { columns, name, .. } => {
                alter_table_add_unique_constraint(
                    store,
                    txn,
                    db_id,
                    &mut schema,
                    &table_object_name,
                    &t,
                    name,
                    columns,
                )
                .await?;
            }
            TableConstraint::ForeignKey {
                name,
                columns,
                foreign_table,
                referred_columns,
                on_delete,
                on_update,
                ..
            } => {
                alter_table_add_foreign_key(
                    store,
                    txn,
                    db_id,
                    search_path,
                    &mut schema,
                    &table_object_name,
                    name,
                    columns,
                    foreign_table,
                    referred_columns,
                    on_delete,
                    on_update,
                )
                .await?;
            }
            TableConstraint::Check { name, expr } => {
                alter_table_add_check_constraint(
                    store,
                    txn,
                    db_id,
                    search_path,
                    &mut schema,
                    &table_object_name,
                    &collations,
                    name,
                    expr,
                )
                .await?;
            }
            _ => {
                return Err(anyhow!(
                    "Unsupported constraint in ALTER TABLE ... ADD CONSTRAINT (supported: PRIMARY KEY, UNIQUE, FOREIGN KEY, CHECK)"
                ));
            }
        },
        AlterTableOperation::DropConstraint {
            if_exists,
            name,
            cascade,
        } => {
            if let Some(result) = alter_table_drop_constraint(
                store,
                txn,
                db_id,
                &mut schema,
                &t,
                &table_object_name,
                &result_table_name,
                *if_exists,
                name,
                *cascade,
            )
            .await?
            {
                return Ok((result, None));
            }
        }
        AlterTableOperation::DropColumn {
            column_name,
            if_exists,
            cascade,
            ..
        } => {
            invalidate_stats = alter_table_drop_column(
                store,
                txn,
                db_id,
                &mut schema,
                column_name,
                *if_exists,
                *cascade,
            )
            .await?;
        }
        AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => {
            let old_name = normalize_ident(old_column_name);
            let new_name = normalize_ident(new_column_name);
            let new_quote_style = new_column_name.quote_style;
            let col_idx = schema
                .column_index(&old_name)
                .ok_or_else(|| anyhow!("Column '{}' does not exist", old_name))?;
            if schema.column_index(&new_name).is_some() {
                return Err(anyhow!("Column '{}' already exists", new_name));
            }
            schema.columns[col_idx].name = new_name.clone();
            for index in &mut schema.indexes {
                for col in &mut index.columns {
                    if *col == old_name {
                        *col = new_name.clone();
                    }
                }
            }
            for fk in &mut schema.foreign_keys {
                for col in &mut fk.columns {
                    if *col == old_name {
                        *col = new_name.clone();
                    }
                }
            }
            for check in &mut schema.check_constraints {
                if check_expr_references_column(&check.expr, &old_name)? {
                    check.expr = rewrite_check_expr_column(
                        &check.expr,
                        &old_name,
                        &new_name,
                        new_quote_style,
                    )?;
                }
            }
            schema.version += 1;
            store.update_schema(txn, db_id, schema).await?;
            store
                .rename_column_metadata(txn, db_id, &t, &old_name, &new_name)
                .await?;
            invalidate_stats = true;
        }
        AlterTableOperation::RenameTable { table_name } => {
            let new_table = table_name
                .0
                .last()
                .map(normalize_ident)
                .ok_or_else(|| anyhow!("Invalid table name"))?;
            let (schema_name, _) = names::parse_full_name(&t)?;
            let new_full = format!("{}.{}", schema_name, new_table);
            store.rename_table_schema(txn, db_id, &t, &new_full).await?;
            store
                .rename_table_metadata(txn, db_id, &t, &new_full)
                .await?;
            result_table_name = new_full.clone();

            // Update referencing-side metadata (FKs store ref_table as a string).
            let tables = store.list_tables(txn, db_id).await?;
            for table in tables {
                let mut s = match store.get_schema(txn, db_id, &table).await? {
                    Some(s) => s,
                    None => continue,
                };
                let mut changed = false;
                for fk in &mut s.foreign_keys {
                    if fk.ref_table == t {
                        fk.ref_table = new_full.clone();
                        changed = true;
                    }
                }
                if changed {
                    s.version += 1;
                    store.update_schema(txn, db_id, s).await?;
                }
            }
        }
        AlterTableOperation::RenameConstraint { old_name, new_name } => {
            let old = normalize_ident(old_name);
            let new = normalize_ident(new_name);

            if constraint_name_exists(&schema, &table_object_name, &new) {
                return Err(SqlError::DuplicateRelation(new.clone()).into());
            }

            if !schema.pk_indices.is_empty() {
                let default_pk_name;
                let pk_name = match schema.pk_constraint_name.as_deref() {
                    Some(n) => n,
                    None => {
                        default_pk_name = format!("{}_pkey", table_object_name);
                        &default_pk_name
                    }
                };
                if old == pk_name {
                    return Err(anyhow!("Cannot rename primary key constraint '{}'", old));
                }
            }

            if let Some(fk) = schema.foreign_keys.iter_mut().find(|fk| fk.name == old) {
                fk.name = new;
                schema.version += 1;
                store.update_schema(txn, db_id, schema).await?;
                return Ok((
                    ExecuteResult::AlterTable {
                        table_name: result_table_name,
                    },
                    None,
                ));
            }

            if let Some(pos) = find_check_constraint_index(&schema, &table_object_name, &old) {
                schema.check_constraints[pos].name = Some(new);
                schema.version += 1;
                store.update_schema(txn, db_id, schema).await?;
                return Ok((
                    ExecuteResult::AlterTable {
                        table_name: result_table_name,
                    },
                    None,
                ));
            }

            return Err(anyhow!(
                "constraint \"{}\" for table \"{}\" does not exist",
                old,
                table_object_name
            ));
        }
        AlterTableOperation::AlterColumn { column_name, op } => {
            let col_name = normalize_ident(column_name);
            let col_idx = schema
                .column_index(&col_name)
                .ok_or_else(|| anyhow!("Column '{}' does not exist", col_name))?;

            match op {
                AlterColumnOperation::SetDefault { value } => {
                    schema.columns[col_idx].default_expr = Some(value.to_string());
                    schema.version += 1;
                    store.update_schema(txn, db_id, schema).await?;
                }
                AlterColumnOperation::DropDefault => {
                    schema.columns[col_idx].default_expr = None;
                    schema.version += 1;
                    store.update_schema(txn, db_id, schema).await?;
                }
                AlterColumnOperation::SetNotNull => {
                    if !schema.columns[col_idx].nullable {
                        return Ok((
                            ExecuteResult::AlterTable {
                                table_name: result_table_name,
                            },
                            None,
                        ));
                    }

                    let (start, end) =
                        crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
                    let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
                    while let Some(batch) = scanner.next_batch(txn).await? {
                        for pair in batch {
                            let mut row = crate::storage::deserialize_row(pair.value())?;
                            fill_row_defaults(&mut row, &schema)?;
                            if matches!(row.values[col_idx], Value::Null) {
                                let short_table =
                                    schema.name.rsplit('.').next().unwrap_or(&schema.name);
                                return Err(anyhow!(
                                    "column \"{}\" of relation \"{}\" contains null values",
                                    col_name,
                                    short_table
                                ));
                            }
                        }
                    }

                    schema.columns[col_idx].nullable = false;
                    schema.version += 1;
                    store.update_schema(txn, db_id, schema).await?;
                }
                AlterColumnOperation::DropNotNull => {
                    schema.columns[col_idx].nullable = true;
                    schema.version += 1;
                    store.update_schema(txn, db_id, schema).await?;
                }
                AlterColumnOperation::SetDataType { data_type, using } => {
                    invalidate_stats = alter_table_alter_column_set_data_type(
                        store,
                        txn,
                        db_id,
                        search_path,
                        &mut schema,
                        &collations,
                        &col_name,
                        col_idx,
                        data_type,
                        using,
                    )
                    .await?;
                }
            }
        }
        _ => return Err(SqlError::Unsupported("Unsupported ALTER".into()).into()),
    }

    let invalidate_table = if invalidate_stats {
        Some(table_id)
    } else {
        None
    };
    Ok((
        ExecuteResult::AlterTable {
            table_name: result_table_name,
        },
        invalidate_table,
    ))
}

#[inline]
pub(super) fn should_invalidate_stats_for_drop_column(column_exists: bool) -> bool {
    column_exists
}

#[inline]
pub(super) fn should_invalidate_stats_for_type_change(
    old_type: &DataType,
    new_type: &DataType,
) -> bool {
    old_type != new_type
}
