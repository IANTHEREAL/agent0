//! Foreign key constraint validation and cascade operations (DELETE/UPDATE).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::Result;
use tikv_client::Transaction;

use crate::sql::error::SqlError;
use crate::sql::projection::eval_default_expr;
use crate::storage::TikvStore;
use crate::types::{Row, TableSchema, Value};

use super::insert::build_enum_label_cache;
use super::update::execute_update_row;

fn short_relation_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

pub async fn validate_foreign_keys(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    row: &Row,
) -> Result<()> {
    for fk in &schema.foreign_keys {
        let mut fk_values: Vec<Value> = Vec::with_capacity(fk.columns.len());
        let mut all_null = true;

        for col_name in &fk.columns {
            let col_idx = schema
                .column_index(col_name)
                .ok_or_else(|| anyhow!("FK column '{}' not found in schema", col_name))?;
            let val = row.values[col_idx].clone();
            if val != Value::Null {
                all_null = false;
            }
            fk_values.push(val);
        }

        if all_null {
            continue;
        }

        let ref_schema = store
            .get_schema(txn, db_id, &fk.ref_table)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "Referenced table '{}' not found for foreign key '{}'",
                    fk.ref_table,
                    fk.name
                )
            })?;

        let ref_rows = store
            .batch_get_rows(
                txn,
                db_id,
                ref_schema.table_id,
                vec![fk_values.clone()],
                &ref_schema,
            )
            .await?;

        if ref_rows.is_empty() {
            let cols = fk.columns.join(", ");
            let vals: Vec<String> = fk_values.iter().map(|v| format!("{}", v)).collect();
            let short_table = short_relation_name(&schema.name);
            let short_ref_table = short_relation_name(&fk.ref_table);
            return Err(SqlError::ForeignKeyViolation {
                constraint: fk.name.clone(),
                message: format!(
                    "insert or update on table \"{}\" violates foreign key constraint \"{}\"\n\
                     DETAIL:  Key ({})=({}) is not present in table \"{}\".",
                    short_table,
                    fk.name,
                    cols,
                    vals.join(", "),
                    short_ref_table
                ),
            }
            .into());
        }
    }
    Ok(())
}

pub async fn handle_foreign_key_on_delete(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    row: &Row,
) -> Result<()> {
    let pk_values = schema.get_pk_values(row);

    let table_names = store.list_tables(txn, db_id).await?;
    let mut table_rows: HashMap<String, Vec<Row>> = HashMap::new();
    let mut table_schemas: HashMap<String, TableSchema> = HashMap::new();
    for t in &table_names {
        if let Some(s) = store.get_schema(txn, db_id, t).await? {
            if !s.foreign_keys.is_empty() {
                let rows = store.scan(txn, db_id, t, None).await?;
                table_rows.insert(t.clone(), rows);
                table_schemas.insert(t.clone(), s);
            }
        }
    }

    let mut deleted_pks: HashMap<String, Vec<Vec<Value>>> = HashMap::new();

    Box::pin(cascade_delete_recursive(
        store,
        txn,
        db_id,
        table_name,
        &pk_values,
        &table_rows,
        &table_schemas,
        &mut deleted_pks,
    ))
    .await
}

async fn cascade_delete_recursive(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    pk_values: &[Value],
    table_rows: &HashMap<String, Vec<Row>>,
    table_schemas: &HashMap<String, TableSchema>,
    deleted_pks: &mut HashMap<String, Vec<Vec<Value>>>,
) -> Result<()> {
    use crate::types::ForeignKeyAction;

    for (other_table, other_schema) in table_schemas {
        if other_table == table_name {
            continue;
        }

        for fk in &other_schema.foreign_keys {
            if fk.ref_table != table_name {
                continue;
            }

            let all_rows = table_rows.get(other_table).cloned().unwrap_or_default();
            let deleted_in_table = deleted_pks.entry(other_table.clone()).or_default();

            let mut rows_to_cascade: Vec<Row> = Vec::new();
            let mut rows_to_update: Vec<(Row, Row)> = Vec::new();

            for other_row in &all_rows {
                let other_pk = other_schema.get_pk_values(other_row);
                if deleted_in_table.contains(&other_pk) {
                    continue;
                }

                let mut fk_values: Vec<Value> = Vec::new();
                let mut all_null = true;

                for col_name in &fk.columns {
                    if let Some(idx) = other_schema.column_index(col_name) {
                        let val = other_row.values[idx].clone();
                        if val != Value::Null {
                            all_null = false;
                        }
                        fk_values.push(val);
                    }
                }

                if all_null {
                    continue;
                }

                if fk_values == pk_values {
                    match fk.on_delete {
                        ForeignKeyAction::Cascade => {
                            rows_to_cascade.push(other_row.clone());
                        }
                        ForeignKeyAction::SetNull => {
                            let mut new_values = other_row.values.clone();
                            for col_name in &fk.columns {
                                if let Some(idx) = other_schema.column_index(col_name) {
                                    new_values[idx] = Value::Null;
                                }
                            }
                            rows_to_update.push((other_row.clone(), Row::new(new_values)));
                        }
                        ForeignKeyAction::NoAction | ForeignKeyAction::Restrict => {
                            let cols = fk.ref_columns.join(", ");
                            let pk_val_strs: Vec<String> =
                                pk_values.iter().map(|v| format!("{}", v)).collect();
                            let short_table = short_relation_name(table_name);
                            let short_other_table = short_relation_name(other_table);
                            return Err(SqlError::ForeignKeyViolation {
                                constraint: fk.name.clone(),
                                message: format!(
                                    "update or delete on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"\n\
                                     DETAIL:  Key ({})=({}) is still referenced from table \"{}\".",
                                    short_table,
                                    fk.name,
                                    short_other_table,
                                    cols,
                                    pk_val_strs.join(", "),
                                    short_other_table
                                ),
                            }
                            .into());
                        }
                        ForeignKeyAction::SetDefault => {
                            let mut new_values = other_row.values.clone();
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
                            rows_to_update.push((other_row.clone(), Row::new(new_values)));
                        }
                    }
                }
            }

            for del_row in rows_to_cascade {
                let del_pk = other_schema.get_pk_values(&del_row);

                Box::pin(cascade_delete_recursive(
                    store,
                    txn,
                    db_id,
                    other_table,
                    &del_pk,
                    table_rows,
                    table_schemas,
                    deleted_pks,
                ))
                .await?;

                deleted_pks
                    .entry(other_table.clone())
                    .or_default()
                    .push(del_pk.clone());

                super::delete::delete_row_storage_entries(
                    store,
                    txn,
                    db_id,
                    other_table,
                    other_schema,
                    &del_row,
                )
                .await?;
            }

            let enum_cache = build_enum_label_cache(store, txn, db_id, other_schema).await?;
            for (old_row, new_row) in rows_to_update {
                execute_update_row(
                    store,
                    txn,
                    db_id,
                    other_table,
                    other_schema,
                    &old_row,
                    new_row,
                    &enum_cache,
                )
                .await?;
            }
        }
    }
    Ok(())
}

pub async fn handle_foreign_key_on_update(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    old_row: &Row,
    new_row: &Row,
) -> Result<()> {
    use crate::types::ForeignKeyAction;

    let old_pk_values = schema.get_pk_values(old_row);
    let new_pk_values = schema.get_pk_values(new_row);

    if old_pk_values == new_pk_values {
        return Ok(());
    }

    let table_names = store.list_tables(txn, db_id).await?;

    for other_table in &table_names {
        if other_table == table_name {
            continue;
        }

        let other_schema = match store.get_schema(txn, db_id, other_table).await? {
            Some(s) => s,
            None => continue,
        };

        for fk in &other_schema.foreign_keys {
            if fk.ref_table != table_name {
                continue;
            }

            let all_rows = store.scan(txn, db_id, other_table, None).await?;
            let mut rows_to_update: Vec<(Row, Row)> = Vec::new();

            for other_row in &all_rows {
                let mut fk_values: Vec<Value> = Vec::new();
                let mut all_null = true;

                for col_name in &fk.columns {
                    if let Some(idx) = other_schema.column_index(col_name) {
                        let val = other_row.values[idx].clone();
                        if val != Value::Null {
                            all_null = false;
                        }
                        fk_values.push(val);
                    }
                }

                if all_null {
                    continue;
                }

                if fk_values == old_pk_values {
                    match fk.on_update {
                        ForeignKeyAction::Cascade => {
                            let mut new_values = other_row.values.clone();
                            for (i, col_name) in fk.columns.iter().enumerate() {
                                if let Some(idx) = other_schema.column_index(col_name) {
                                    if i < new_pk_values.len() {
                                        new_values[idx] = new_pk_values[i].clone();
                                    }
                                }
                            }
                            rows_to_update.push((other_row.clone(), Row::new(new_values)));
                        }
                        ForeignKeyAction::SetNull => {
                            let mut new_values = other_row.values.clone();
                            for col_name in &fk.columns {
                                if let Some(idx) = other_schema.column_index(col_name) {
                                    new_values[idx] = Value::Null;
                                }
                            }
                            rows_to_update.push((other_row.clone(), Row::new(new_values)));
                        }
                        ForeignKeyAction::SetDefault => {
                            let mut new_values = other_row.values.clone();
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
                            rows_to_update.push((other_row.clone(), Row::new(new_values)));
                        }
                        ForeignKeyAction::NoAction | ForeignKeyAction::Restrict => {
                            let cols = fk.ref_columns.join(", ");
                            let pk_val_strs: Vec<String> =
                                old_pk_values.iter().map(|v| format!("{}", v)).collect();
                            let short_table = short_relation_name(table_name);
                            let short_other_table = short_relation_name(other_table);
                            return Err(SqlError::ForeignKeyViolation {
                                constraint: fk.name.clone(),
                                message: format!(
                                    "update or delete on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"\n\
                                     DETAIL:  Key ({})=({}) is still referenced from table \"{}\".",
                                    short_table,
                                    fk.name,
                                    short_other_table,
                                    cols,
                                    pk_val_strs.join(", "),
                                    short_other_table
                                ),
                            }
                            .into());
                        }
                    }
                }
            }

            let enum_cache = build_enum_label_cache(store, txn, db_id, &other_schema).await?;
            for (old_child_row, new_child_row) in rows_to_update {
                Box::pin(execute_update_row(
                    store,
                    txn,
                    db_id,
                    other_table,
                    &other_schema,
                    &old_child_row,
                    new_child_row,
                    &enum_cache,
                ))
                .await?;
            }
        }
    }
    Ok(())
}
