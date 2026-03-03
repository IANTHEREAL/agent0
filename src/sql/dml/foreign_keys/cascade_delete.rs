//! FK ON DELETE cascade and enforcement.

use std::collections::HashSet;

use anyhow::Result;
use tikv_client::Transaction;

use crate::model::{ForeignKeyAction, Row, TableSchema, Value};
use crate::sql::error::SqlError;
use crate::sql::projection::eval_default_expr;

use super::super::update::execute_update_row;
use super::{
    fk_values_for_row, get_ref_values, pk_to_hash_key, ref_column_names, short_relation_name,
    FkDeleteContext, FkStoreCtx,
};

use crate::sql::dml::insert::build_enum_label_cache;

pub async fn handle_foreign_key_on_delete(
    ctx: &FkStoreCtx<'_>,
    txn: &mut Transaction,
    table_name: &str,
    schema: &TableSchema,
    row: &Row,
    stmt_deleting_pks: &HashSet<String>,
    fk_ctx: &mut FkDeleteContext,
) -> Result<()> {
    if fk_ctx.table_schemas.is_empty() {
        return Ok(());
    }

    // Pre-seed with the current row being deleted to prevent cyclic
    // self-cascades from re-visiting it.
    fk_ctx.mark_deleted_pk(table_name, &schema.get_pk_values(row));

    Box::pin(cascade_delete_recursive(
        ctx,
        txn,
        table_name,
        row,
        schema,
        fk_ctx,
        stmt_deleting_pks,
        table_name,
    ))
    .await
}

async fn cascade_delete_recursive(
    ctx: &FkStoreCtx<'_>,
    txn: &mut Transaction,
    table_name: &str,
    parent_row: &Row,
    parent_schema: &TableSchema,
    fk_ctx: &mut FkDeleteContext,
    stmt_deleting_pks: &HashSet<String>,
    stmt_target_table: &str,
) -> Result<()> {
    let referencing_entries: Vec<(String, usize)> = fk_ctx
        .referencing_by_parent
        .get(table_name)
        .cloned()
        .unwrap_or_default();

    for (other_table, fk_idx) in referencing_entries {
        let other_schema = fk_ctx.table_schemas[&other_table].clone();
        let fk = other_schema.foreign_keys[fk_idx].clone();

        let ref_values = get_ref_values(&fk, parent_schema, parent_row)?;

        // For self-referencing FKs, skip the row being deleted itself.
        let self_ref_parent_pk = if other_table.as_str() == table_name {
            Some(parent_schema.get_pk_values(parent_row))
        } else {
            None
        };

        // Targeted lookup: only rows where FK columns == ref_values.
        let matching_rows = super::find_referencing_rows(
            ctx.store,
            txn,
            ctx.db_id,
            &other_table,
            &other_schema,
            &fk,
            &ref_values,
        )
        .await?;

        let mut rows_to_cascade: Vec<Row> = Vec::new();
        let mut rows_to_update_pks: Vec<Vec<Value>> = Vec::new();

        for other_row in &matching_rows {
            let other_pk = other_schema.get_pk_values(other_row);
            if fk_ctx.is_pk_marked_deleted(&other_table, &other_pk) {
                continue;
            }

            // Statement-level deferral: skip referencing rows that are
            // also being deleted by the same DELETE statement.  Gives
            // correct PostgreSQL semantics for both NO ACTION and RESTRICT.
            // O(1) lookup via pre-built HashSet<String>.
            if other_table.as_str() == stmt_target_table
                && stmt_deleting_pks.contains(&pk_to_hash_key(&other_pk))
            {
                continue;
            }

            // Don't let the row being deleted RESTRICT/cascade against itself.
            if let Some(ref parent_pk) = self_ref_parent_pk {
                if other_pk == *parent_pk {
                    continue;
                }
            }

            let Some(fk_values) = fk_values_for_row(&fk, &other_schema, other_row) else {
                continue;
            };

            if fk_values == ref_values {
                match fk.on_delete {
                    ForeignKeyAction::Cascade => {
                        rows_to_cascade.push(other_row.clone());
                    }
                    ForeignKeyAction::SetNull => rows_to_update_pks.push(other_pk.clone()),
                    ForeignKeyAction::NoAction | ForeignKeyAction::Restrict => {
                        let cols = ref_column_names(&fk, parent_schema);
                        let ref_val_strs: Vec<String> =
                            ref_values.iter().map(|v| format!("{}", v)).collect();
                        let short_table = short_relation_name(table_name);
                        let short_other_table = short_relation_name(&other_table);
                        return Err(SqlError::ForeignKeyViolation {
                                constraint: fk.name.clone(),
                                message: format!(
                                    "update or delete on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"\n\
                                     DETAIL:  Key ({})=({}) is still referenced from table \"{}\".",
                                    short_table,
                                    fk.name,
                                    short_other_table,
                                    cols,
                                    ref_val_strs.join(", "),
                                    short_other_table
                                ),
                            }
                            .into());
                    }
                    ForeignKeyAction::SetDefault => rows_to_update_pks.push(other_pk.clone()),
                }
            }
        }

        for del_row in rows_to_cascade {
            let del_pk = other_schema.get_pk_values(&del_row);

            // Record as deleted BEFORE recursion to prevent infinite cycles
            // on self-referencing or mutually-referencing rows.
            fk_ctx.mark_deleted_pk(&other_table, &del_pk);

            Box::pin(cascade_delete_recursive(
                ctx,
                txn,
                &other_table,
                &del_row,
                &other_schema,
                fk_ctx,
                stmt_deleting_pks,
                stmt_target_table,
            ))
            .await?;

            super::super::delete::delete_row_storage_entries(
                ctx.store,
                txn,
                ctx.db_id,
                &other_table,
                &other_schema,
                &del_row,
            )
            .await?;
        }

        if !rows_to_update_pks.is_empty() {
            let enum_cache =
                build_enum_label_cache(ctx.store, txn, ctx.db_id, &other_schema).await?;
            for target_pk in rows_to_update_pks {
                let Some(current_row) = super::fetch_row_by_pk(
                    ctx.store,
                    txn,
                    ctx.db_id,
                    other_schema.table_id,
                    &other_schema,
                    target_pk,
                )
                .await?
                else {
                    // The row may have been deleted by an earlier CASCADE action in this frame.
                    continue;
                };

                let mut new_values = current_row.values.clone();
                match fk.on_delete {
                    ForeignKeyAction::SetNull => {
                        for col_name in &fk.columns {
                            if let Some(idx) = other_schema.column_index(col_name) {
                                new_values[idx] = Value::Null;
                            }
                        }
                    }
                    ForeignKeyAction::SetDefault => {
                        for col_name in &fk.columns {
                            if let Some(idx) = other_schema.column_index(col_name) {
                                let col = &other_schema.columns[idx];
                                let default_val = if let Some(ref def_expr) = col.default_expr {
                                    eval_default_expr(def_expr)?
                                } else {
                                    Value::Null
                                };
                                new_values[idx] = default_val;
                            }
                        }
                    }
                    _ => unreachable!("rows_to_update_pks only stores SET NULL/SET DEFAULT"),
                }

                let desired_row = Row::new(new_values);

                let updated_row = execute_update_row(
                    ctx.store,
                    txn,
                    ctx.db_id,
                    &other_table,
                    &other_schema,
                    &current_row,
                    desired_row,
                    &enum_cache,
                    Some(fk_ctx),
                    None,
                )
                .await?;

                // Root invariant for ON DELETE SET DEFAULT:
                // the final child FK values must not keep pointing to the
                // parent key being deleted by this delete-cascade frame.
                if matches!(fk.on_delete, ForeignKeyAction::SetDefault) {
                    if let Some(post_fk_values) =
                        fk_values_for_row(&fk, &other_schema, &updated_row)
                    {
                        if post_fk_values == ref_values {
                            let cols = ref_column_names(&fk, parent_schema);
                            let ref_val_strs: Vec<String> =
                                ref_values.iter().map(|v| format!("{}", v)).collect();
                            let short_parent = short_relation_name(table_name);
                            let short_child = short_relation_name(&other_table);
                            return Err(SqlError::ForeignKeyViolation {
                                    constraint: fk.name.clone(),
                                    message: format!(
                                        "update or delete on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"\n\
                                         DETAIL:  Key ({})=({}) is still referenced from table \"{}\".",
                                        short_parent,
                                        fk.name,
                                        short_child,
                                        cols,
                                        ref_val_strs.join(", "),
                                        short_child
                                    ),
                                }
                                .into());
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
