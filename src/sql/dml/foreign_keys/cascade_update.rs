//! FK ON UPDATE cascade and enforcement.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use tikv_client::Transaction;

use crate::sql::error::SqlError;
use crate::sql::projection::eval_default_expr;
use crate::sql::value_coercion::coerce_value_for_column;
use crate::storage::TikvStore;
use crate::types::{ForeignKeyAction, ForeignKeyConstraint, Row, TableSchema, Value};

use super::{
    fk_values_for_row, get_ref_values, pk_to_hash_key, ref_column_names, short_relation_name,
    validate_foreign_keys, FkDeleteContext, FkStoreCtx,
};

use super::super::update::execute_update_row_without_fk_update;
use crate::sql::dml::insert::build_enum_label_cache;

pub async fn handle_foreign_key_on_update(
    ctx: &FkStoreCtx<'_>,
    txn: &mut Transaction,
    table_name: &str,
    schema: &TableSchema,
    old_row: &Row,
    new_row: &Row,
    fk_ctx: Option<&mut FkDeleteContext>,
) -> Result<()> {
    if let Some(del_ctx) = fk_ctx {
        return handle_foreign_key_on_update_with_ctx(
            ctx, txn, table_name, schema, old_row, new_row, del_ctx,
        )
        .await;
    }
    handle_foreign_key_on_update_no_ctx(ctx, txn, table_name, schema, old_row, new_row).await
}

async fn handle_foreign_key_on_update_with_ctx(
    ctx: &FkStoreCtx<'_>,
    txn: &mut Transaction,
    table_name: &str,
    schema: &TableSchema,
    old_row: &Row,
    new_row: &Row,
    fk_ctx: &mut FkDeleteContext,
) -> Result<()> {
    if fk_ctx.table_schemas.is_empty() {
        return Ok(());
    }

    let short_parent = short_relation_name(table_name);
    let fk_tables: Vec<String> = fk_ctx.table_schemas.keys().cloned().collect();
    for other_table in fk_tables {
        let Some(other_schema) = fk_ctx.table_schemas.get(&other_table).cloned() else {
            continue;
        };

        if !other_schema
            .foreign_keys
            .iter()
            .any(|fk| fk.ref_table == table_name)
        {
            continue;
        }

        let rules = build_fk_update_rules(&other_schema, table_name, schema, old_row, new_row)?;
        if rules.is_empty() {
            continue;
        }

        let mut enum_cache = None;
        let mut pending_propagations: Vec<(Row, Row)> = Vec::new();
        let mut touched_pk_keys: HashSet<String> = HashSet::new();
        let mut touched_pks: Vec<Vec<Value>> = Vec::new();
        loop {
            let all_rows = fk_ctx
                .table_rows
                .get(&other_table)
                .cloned()
                .unwrap_or_default();
            let mut target_update: Option<(Row, Row)> = None;
            for child_row in all_rows {
                let Some(new_child_row) = build_merged_fk_update_row(
                    &rules,
                    short_parent,
                    &other_table,
                    schema,
                    &other_schema,
                    &child_row,
                )?
                else {
                    continue;
                };
                target_update = Some((child_row, new_child_row));
                break;
            }

            let Some((old_child_row, new_child_row)) = target_update else {
                break;
            };

            if enum_cache.is_none() {
                enum_cache =
                    Some(build_enum_label_cache(ctx.store, txn, ctx.db_id, &other_schema).await?);
            }
            let enum_cache = enum_cache.as_ref().expect("enum cache initialized");
            let updated_row = Box::pin(execute_update_row_without_fk_update(
                ctx.store,
                txn,
                ctx.db_id,
                &other_table,
                &other_schema,
                &old_child_row,
                new_child_row,
                enum_cache,
                Some(fk_ctx),
            ))
            .await?;
            fk_ctx.replace_row_in_snapshot(
                &other_table,
                &other_schema,
                &old_child_row,
                updated_row.clone(),
            );
            let updated_pk = other_schema.get_pk_values(&updated_row);
            let updated_pk_key = pk_to_hash_key(&updated_pk);
            if touched_pk_keys.insert(updated_pk_key) {
                touched_pks.push(updated_pk);
            }
            pending_propagations.push((old_child_row, updated_row));
        }

        for (old_child_row, updated_row) in pending_propagations {
            Box::pin(handle_foreign_key_on_update(
                ctx,
                txn,
                &other_table,
                &other_schema,
                &old_child_row,
                &updated_row,
                Some(fk_ctx),
            ))
            .await?;
        }

        for pk in touched_pks {
            if let Some(current_row) = fetch_row_by_pk(
                ctx.store,
                txn,
                ctx.db_id,
                other_schema.table_id,
                &other_schema,
                pk,
            )
            .await?
            {
                validate_foreign_keys(ctx.store, txn, ctx.db_id, &other_schema, &current_row)
                    .await?;
            }
        }
    }

    Ok(())
}

async fn handle_foreign_key_on_update_no_ctx(
    ctx: &FkStoreCtx<'_>,
    txn: &mut Transaction,
    table_name: &str,
    schema: &TableSchema,
    old_row: &Row,
    new_row: &Row,
) -> Result<()> {
    let table_names = ctx.store.list_tables(txn, ctx.db_id).await?;
    let short_parent = short_relation_name(table_name);

    for other_table in &table_names {
        let other_schema = match ctx.store.get_schema(txn, ctx.db_id, other_table).await? {
            Some(s) => s,
            None => continue,
        };

        if !other_schema
            .foreign_keys
            .iter()
            .any(|fk| fk.ref_table == table_name)
        {
            continue;
        }

        let rules = build_fk_update_rules(&other_schema, table_name, schema, old_row, new_row)?;
        if rules.is_empty() {
            continue;
        }

        let mut enum_cache = None;
        let mut pending_propagations: Vec<(Row, Row)> = Vec::new();
        let mut touched_pk_keys: HashSet<String> = HashSet::new();
        let mut touched_pks: Vec<Vec<Value>> = Vec::new();
        loop {
            let all_rows = ctx.store.scan(txn, ctx.db_id, other_table, None).await?;
            let mut target_update: Option<(Row, Row)> = None;
            for child_row in all_rows {
                let Some(new_child_row) = build_merged_fk_update_row(
                    &rules,
                    short_parent,
                    other_table,
                    schema,
                    &other_schema,
                    &child_row,
                )?
                else {
                    continue;
                };
                target_update = Some((child_row, new_child_row));
                break;
            }

            let Some((old_child_row, new_child_row)) = target_update else {
                break;
            };

            if enum_cache.is_none() {
                enum_cache =
                    Some(build_enum_label_cache(ctx.store, txn, ctx.db_id, &other_schema).await?);
            }
            let enum_cache = enum_cache.as_ref().expect("enum cache initialized");
            let updated_row = Box::pin(execute_update_row_without_fk_update(
                ctx.store,
                txn,
                ctx.db_id,
                other_table,
                &other_schema,
                &old_child_row,
                new_child_row,
                enum_cache,
                None,
            ))
            .await?;
            let updated_pk = other_schema.get_pk_values(&updated_row);
            let updated_pk_key = pk_to_hash_key(&updated_pk);
            if touched_pk_keys.insert(updated_pk_key) {
                touched_pks.push(updated_pk);
            }
            pending_propagations.push((old_child_row, updated_row));
        }

        for (old_child_row, updated_row) in pending_propagations {
            Box::pin(handle_foreign_key_on_update(
                ctx,
                txn,
                other_table,
                &other_schema,
                &old_child_row,
                &updated_row,
                None,
            ))
            .await?;
        }

        for pk in touched_pks {
            if let Some(current_row) = fetch_row_by_pk(
                ctx.store,
                txn,
                ctx.db_id,
                other_schema.table_id,
                &other_schema,
                pk,
            )
            .await?
            {
                validate_foreign_keys(ctx.store, txn, ctx.db_id, &other_schema, &current_row)
                    .await?;
            }
        }
    }

    Ok(())
}

#[derive(Clone)]
struct FkUpdateRule {
    fk: ForeignKeyConstraint,
    old_ref_values: Vec<Value>,
    new_ref_values: Vec<Value>,
}

fn build_fk_update_rules(
    child_schema: &TableSchema,
    parent_table_name: &str,
    parent_schema: &TableSchema,
    old_row: &Row,
    new_row: &Row,
) -> Result<Vec<FkUpdateRule>> {
    let mut rules = Vec::new();
    for fk in &child_schema.foreign_keys {
        if fk.ref_table != parent_table_name {
            continue;
        }
        let old_ref_values = get_ref_values(fk, parent_schema, old_row)?;
        let new_ref_values = get_ref_values(fk, parent_schema, new_row)?;
        if old_ref_values == new_ref_values {
            continue;
        }
        rules.push(FkUpdateRule {
            fk: fk.clone(),
            old_ref_values,
            new_ref_values,
        });
    }
    Ok(rules)
}

fn build_merged_fk_update_row(
    rules: &[FkUpdateRule],
    short_parent: &str,
    child_table_name: &str,
    parent_schema: &TableSchema,
    child_schema: &TableSchema,
    child_row: &Row,
) -> Result<Option<Row>> {
    let mut new_values = child_row.values.clone();
    let mut matched_any = false;
    let mut matched_set_default_rules: Vec<&FkUpdateRule> = Vec::new();

    for rule in rules {
        let Some(fk_values) = fk_values_for_row(&rule.fk, child_schema, child_row) else {
            continue;
        };
        if fk_values != rule.old_ref_values {
            continue;
        }
        matched_any = true;

        match rule.fk.on_update {
            ForeignKeyAction::Cascade => {
                for (i, col_name) in rule.fk.columns.iter().enumerate() {
                    if let Some(idx) = child_schema.column_index(col_name) {
                        if i < rule.new_ref_values.len() {
                            new_values[idx] = rule.new_ref_values[i].clone();
                        }
                    }
                }
            }
            ForeignKeyAction::SetNull => {
                for col_name in &rule.fk.columns {
                    if let Some(idx) = child_schema.column_index(col_name) {
                        new_values[idx] = Value::Null;
                    }
                }
            }
            ForeignKeyAction::SetDefault => {
                for col_name in &rule.fk.columns {
                    if let Some(idx) = child_schema.column_index(col_name) {
                        let col = &child_schema.columns[idx];
                        let default_val = if let Some(ref def_expr) = col.default_expr {
                            eval_default_expr(def_expr)?
                        } else {
                            Value::Null
                        };
                        new_values[idx] = default_val;
                    }
                }
                matched_set_default_rules.push(rule);
            }
            ForeignKeyAction::NoAction | ForeignKeyAction::Restrict => {
                return Err(parent_fk_update_violation(
                    &rule.fk,
                    short_parent,
                    child_table_name,
                    parent_schema,
                    &rule.old_ref_values,
                ));
            }
        }
    }

    if !matched_any {
        return Ok(None);
    }

    let candidate = Row::new(new_values);

    // For SET DEFAULT, detect non-convergent updates where default resolves
    // to the old parent key; this must surface as a parent-side violation.
    for rule in matched_set_default_rules {
        let Some(post_fk_values) = coerced_fk_values_for_row(&rule.fk, child_schema, &candidate)?
        else {
            continue;
        };
        if post_fk_values == rule.old_ref_values {
            return Err(parent_fk_update_violation(
                &rule.fk,
                short_parent,
                child_table_name,
                parent_schema,
                &rule.old_ref_values,
            ));
        }
    }

    if candidate.values == child_row.values {
        return Ok(None);
    }
    Ok(Some(candidate))
}

fn coerced_fk_values_for_row(
    fk: &ForeignKeyConstraint,
    child_schema: &TableSchema,
    row: &Row,
) -> Result<Option<Vec<Value>>> {
    let mut values = Vec::with_capacity(fk.columns.len());
    for col_name in &fk.columns {
        let Some(idx) = child_schema.column_index(col_name) else {
            return Ok(None);
        };
        let val = row.values[idx].clone();
        if val == Value::Null {
            return Ok(None);
        }
        let col = &child_schema.columns[idx];
        values.push(coerce_value_for_column(val, col)?);
    }
    Ok(Some(values))
}

fn parent_fk_update_violation(
    fk: &ForeignKeyConstraint,
    short_parent: &str,
    child_table_name: &str,
    parent_schema: &TableSchema,
    old_ref_values: &[Value],
) -> anyhow::Error {
    let cols = ref_column_names(fk, parent_schema);
    let ref_val_strs: Vec<String> = old_ref_values.iter().map(|v| format!("{}", v)).collect();
    let short_child = short_relation_name(child_table_name);
    SqlError::ForeignKeyViolation {
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
    .into()
}

async fn fetch_row_by_pk(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    schema: &TableSchema,
    pk: Vec<Value>,
) -> Result<Option<Row>> {
    let rows = store
        .batch_get_rows(txn, db_id, table_id, vec![pk], schema)
        .await?;
    Ok(rows.into_iter().next())
}
