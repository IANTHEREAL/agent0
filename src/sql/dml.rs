use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use tikv_client::Transaction;

use super::gin::extract_gin_token_hashes_from_row;
use super::index_helpers;
use super::projection::eval_default_expr;
use super::sequences;
use super::value_coercion::coerce_value_for_column;
use crate::storage::TikvStore;
use crate::types::{DataType, Row, TableSchema, Value};

pub type EnumLabelCache = HashMap<String, HashSet<String>>;

/// How `execute_insert_row` should handle unique-key conflicts.
///
/// Replaces the previous `&Option<OnInsert>` parameter, removing the dependency
/// on raw sqlparser AST types from the typed execution path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictBehavior {
    /// No ON CONFLICT — unique violations produce an error.
    Error,
    /// ON CONFLICT DO NOTHING — skip the conflicting row.
    DoNothing,
    /// ON CONFLICT DO UPDATE — return the conflicting row for caller-side update.
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

pub async fn build_enum_label_cache(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
) -> Result<EnumLabelCache> {
    let mut required_types: HashSet<&str> = HashSet::new();
    for col in &schema.columns {
        if let DataType::UserDefined(udt_name) = &col.data_type {
            required_types.insert(udt_name.as_str());
        }
    }
    if required_types.is_empty() {
        return Ok(HashMap::new());
    }

    let mut cache: EnumLabelCache = HashMap::new();
    for udt_name in required_types {
        let def = store
            .get_type(txn, db_id, udt_name)
            .await?
            .ok_or_else(|| anyhow!("Type '{}' does not exist", udt_name))?;
        match def.kind {
            crate::types::UserTypeKind::Enum { labels } => {
                cache.insert(udt_name.to_string(), labels.into_iter().collect());
            }
            crate::types::UserTypeKind::Composite { .. } => {
                return Err(anyhow!(
                    "composite type '{}' cannot be used as a column type",
                    udt_name
                ));
            }
        }
    }

    Ok(cache)
}

fn validate_enum_values(schema: &TableSchema, row: &Row, cache: &EnumLabelCache) -> Result<()> {
    if cache.is_empty() {
        return Ok(());
    }

    for (idx, col) in schema.columns.iter().enumerate() {
        let DataType::UserDefined(udt_name) = &col.data_type else {
            continue;
        };

        let value = row
            .values
            .get(idx)
            .ok_or_else(|| anyhow!("Row value missing for column '{}'", col.name))?;
        if matches!(value, Value::Null) {
            continue;
        }

        let labels = cache
            .get(udt_name)
            .ok_or_else(|| anyhow!("Type '{}' does not exist", udt_name))?;

        match value {
            Value::Text(s) => {
                if !labels.contains(s) {
                    return Err(anyhow!(
                        "invalid input value for enum {}: \"{}\"",
                        udt_name,
                        s
                    ));
                }
            }
            other => {
                return Err(anyhow!(
                    "invalid input value for enum {}: {}",
                    udt_name,
                    other
                ));
            }
        }
    }

    Ok(())
}

pub async fn execute_insert_row(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    row: Row,
    on_conflict: ConflictBehavior,
    enum_cache: &EnumLabelCache,
) -> Result<InsertRowResult> {
    let mut row_values = row.values;
    coerce_row_values(schema, &mut row_values)?;
    let row = Row::new(row_values);

    validate_enum_values(schema, &row, enum_cache)?;

    let pk_types: Vec<DataType> = if schema.pk_indices.is_empty() {
        vec![DataType::Uuid]
    } else {
        schema
            .pk_indices
            .iter()
            .map(|&idx| schema.columns[idx].data_type.clone())
            .collect()
    };

    if !schema.foreign_keys.is_empty() {
        validate_foreign_keys(store, txn, db_id, schema, &row).await?;
    }

    let insert_result = store.insert(txn, db_id, table_name, row.clone()).await;
    match insert_result {
        Ok(pk_values) => {
            let mut created_index_entries: Vec<(u64, Vec<Value>, bool)> = Vec::new();
            for index in &schema.indexes {
                if !index_helpers::is_index_materializable(index) {
                    continue;
                }
                if !index_helpers::eval_index_predicate(index, schema, &row)? {
                    continue;
                }
                let idx_values =
                    index_helpers::get_index_values_with_expressions(index, schema, &row)?;
                let result = store
                    .create_index_entry(
                        txn,
                        db_id,
                        schema.table_id,
                        index.id,
                        &idx_values,
                        &pk_values,
                        index.unique,
                    )
                    .await;
                if let Err(e) = result {
                    if e.to_string().contains("Duplicate entry") {
                        match on_conflict {
                            ConflictBehavior::DoNothing => {
                                rollback_inserted_index_entries(
                                    store,
                                    txn,
                                    db_id,
                                    schema,
                                    &pk_values,
                                    &created_index_entries,
                                )
                                .await?;
                                store
                                    .delete_by_pk(txn, db_id, table_name, &pk_values)
                                    .await?;
                                return Ok(InsertRowResult::Skipped);
                            }
                            ConflictBehavior::DoUpdate => {
                                let pks = store
                                    .scan_index(
                                        txn,
                                        db_id,
                                        schema.table_id,
                                        index.id,
                                        &idx_values,
                                        true,
                                        &pk_types,
                                        None,
                                    )
                                    .await?;
                                if pks.is_empty() {
                                    return Err(anyhow!(
                                        "Failed to find conflicting row in unique index"
                                    ));
                                }
                                let existing_pk = pks.into_iter().next().unwrap();

                                let existing_rows = store
                                    .batch_get_rows(
                                        txn,
                                        db_id,
                                        schema.table_id,
                                        vec![existing_pk.clone()],
                                        schema,
                                    )
                                    .await?;
                                let existing_row =
                                    existing_rows.into_iter().next().ok_or_else(|| {
                                        anyhow!("Failed to fetch existing row for upsert")
                                    })?;

                                rollback_inserted_index_entries(
                                    store,
                                    txn,
                                    db_id,
                                    schema,
                                    &pk_values,
                                    &created_index_entries,
                                )
                                .await?;
                                store
                                    .delete_by_pk(txn, db_id, table_name, &pk_values)
                                    .await?;

                                return Ok(InsertRowResult::Conflicted {
                                    existing_pk,
                                    existing_row,
                                    excluded_row: row,
                                });
                            }
                            ConflictBehavior::Error => {
                                let cols = index.columns.join(", ");
                                let vals: Vec<String> =
                                    idx_values.iter().map(|v| format!("{}", v)).collect();
                                return Err(SqlError::UniqueViolation {
                                    constraint: index.name.clone(),
                                    message: format!(
                                        "duplicate key value violates unique constraint \"{}\"\nDETAIL:  Key ({})=({}) already exists.",
                                        index.name, cols, vals.join(", ")
                                    ),
                                }.into());
                            }
                        }
                    }
                    return Err(e);
                }
                created_index_entries.push((index.id, idx_values, index.unique));
            }
            // Materialize supported GIN indexes only after B-Tree indexes succeed, so
            // ON CONFLICT paths don't need additional cleanup.
            for index in &schema.indexes {
                let hashes = extract_gin_token_hashes_from_row(schema, index, &row)?;
                if hashes.is_empty() {
                    continue;
                }
                store
                    .create_gin_index_entries(
                        txn,
                        db_id,
                        schema.table_id,
                        index.id,
                        &hashes,
                        &pk_values,
                    )
                    .await?;
            }
            Ok(InsertRowResult::Inserted(row))
        }
        Err(e)
            if e.to_string()
                .contains("duplicate key value violates unique constraint") =>
        {
            if schema.pk_indices.is_empty() {
                return Err(e);
            }
            let pk_values = schema.get_pk_values(&row);
            match on_conflict {
                ConflictBehavior::DoNothing => Ok(InsertRowResult::Skipped),
                ConflictBehavior::DoUpdate => {
                    let existing_rows = store
                        .batch_get_rows(
                            txn,
                            db_id,
                            schema.table_id,
                            vec![pk_values.clone()],
                            schema,
                        )
                        .await?;
                    let existing_row = existing_rows
                        .into_iter()
                        .next()
                        .ok_or_else(|| anyhow!("Failed to fetch existing row for upsert"))?;
                    Ok(InsertRowResult::Conflicted {
                        existing_pk: pk_values,
                        existing_row,
                        excluded_row: row,
                    })
                }
                ConflictBehavior::Error => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

pub async fn execute_update_row_by_pk(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    pk_values: &[Value],
    old_row: &Row,
    new_row: Row,
    enum_cache: &EnumLabelCache,
) -> Result<Row> {
    let mut new_row_values = new_row.values;
    coerce_row_values(schema, &mut new_row_values)?;
    let new_row = Row::new(new_row_values);

    validate_enum_values(schema, &new_row, enum_cache)?;
    if !schema.foreign_keys.is_empty() {
        validate_foreign_keys(store, txn, db_id, schema, &new_row).await?;
    }

    update_row_indexes(store, txn, db_id, schema, pk_values, old_row, &new_row).await?;
    store
        .upsert_by_pk(txn, db_id, table_name, pk_values, new_row.clone())
        .await?;
    Ok(new_row)
}

async fn update_row_indexes(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    pk_values: &[Value],
    old_row: &Row,
    new_row: &Row,
) -> Result<()> {
    for index in &schema.indexes {
        if index_helpers::index_values_unchanged(index, schema, old_row, new_row)? {
            continue;
        }

        let old_gin_hashes = extract_gin_token_hashes_from_row(schema, index, old_row)?;
        let new_gin_hashes = extract_gin_token_hashes_from_row(schema, index, new_row)?;
        if !old_gin_hashes.is_empty() || !new_gin_hashes.is_empty() {
            if !old_gin_hashes.is_empty() {
                store
                    .delete_gin_index_entries(
                        txn,
                        db_id,
                        schema.table_id,
                        index.id,
                        &old_gin_hashes,
                        &pk_values,
                    )
                    .await?;
            }
            if !new_gin_hashes.is_empty() {
                store
                    .create_gin_index_entries(
                        txn,
                        db_id,
                        schema.table_id,
                        index.id,
                        &new_gin_hashes,
                        &pk_values,
                    )
                    .await?;
            }
            continue;
        }

        if !index_helpers::is_index_materializable(index) {
            continue;
        }

        let old_matches = index_helpers::eval_index_predicate(index, schema, old_row)?;
        if old_matches {
            let old_idx = index_helpers::get_index_values_with_expressions(index, schema, old_row)?;
            store
                .delete_index_entry(
                    txn,
                    db_id,
                    schema.table_id,
                    index.id,
                    &old_idx,
                    &pk_values,
                    index.unique,
                )
                .await?;
        }

        let new_matches = index_helpers::eval_index_predicate(index, schema, new_row)?;
        if new_matches {
            let new_idx = index_helpers::get_index_values_with_expressions(index, schema, new_row)?;
            store
                .create_index_entry(
                    txn,
                    db_id,
                    schema.table_id,
                    index.id,
                    &new_idx,
                    &pk_values,
                    index.unique,
                )
                .await?;
        }
    }
    Ok(())
}

async fn rollback_inserted_index_entries(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    pk_values: &[Value],
    created_index_entries: &[(u64, Vec<Value>, bool)],
) -> Result<()> {
    for (index_id, idx_values, unique) in created_index_entries.iter().rev() {
        store
            .delete_index_entry(
                txn,
                db_id,
                schema.table_id,
                *index_id,
                idx_values,
                pk_values,
                *unique,
            )
            .await?;
    }
    Ok(())
}

async fn delete_row_storage_entries(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    row: &Row,
) -> Result<()> {
    let pk_values = schema.get_pk_values(row);
    store
        .delete_by_pk(txn, db_id, table_name, &pk_values)
        .await?;

    for index in &schema.indexes {
        let gin_hashes = extract_gin_token_hashes_from_row(schema, index, row)?;
        if !gin_hashes.is_empty() {
            store
                .delete_gin_index_entries(
                    txn,
                    db_id,
                    schema.table_id,
                    index.id,
                    &gin_hashes,
                    &pk_values,
                )
                .await?;
            continue;
        }

        if !index_helpers::is_index_materializable(index) {
            continue;
        }

        if !index_helpers::eval_index_predicate(index, schema, row)? {
            continue;
        }

        let idx_values = index_helpers::get_index_values_with_expressions(index, schema, row)?;
        store
            .delete_index_entry(
                txn,
                db_id,
                schema.table_id,
                index.id,
                &idx_values,
                &pk_values,
                index.unique,
            )
            .await?;
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
                            return Err(anyhow!(
                                "update or delete on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"\n\
                                 DETAIL:  Key ({})=({}) is still referenced from table \"{}\".",
                                table_name,
                                fk.name,
                                other_table,
                                cols,
                                pk_val_strs.join(", "),
                                other_table
                            ));
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

                delete_row_storage_entries(store, txn, db_id, other_table, other_schema, &del_row)
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
                            return Err(anyhow!(
                                "update or delete on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"\n\
                                 DETAIL:  Key ({})=({}) is still referenced from table \"{}\".",
                                table_name,
                                fk.name,
                                other_table,
                                cols,
                                pk_val_strs.join(", "),
                                other_table
                            ));
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

pub async fn execute_delete_row(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    row: &Row,
) -> Result<()> {
    handle_foreign_key_on_delete(store, txn, db_id, table_name, schema, row).await?;

    delete_row_storage_entries(store, txn, db_id, table_name, schema, row).await
}

pub async fn execute_update_row(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    old_row: &Row,
    new_row: Row,
    enum_cache: &EnumLabelCache,
) -> Result<Row> {
    let mut new_row_values = new_row.values;
    coerce_row_values(schema, &mut new_row_values)?;
    let new_row = Row::new(new_row_values);

    validate_enum_values(schema, &new_row, enum_cache)?;

    let old_pks = schema.get_pk_values(old_row);
    let new_pks = schema.get_pk_values(&new_row);
    let pk_changed = old_pks != new_pks;

    if !schema.foreign_keys.is_empty() {
        validate_foreign_keys(store, txn, db_id, schema, &new_row).await?;
    }

    if pk_changed {
        let existing = store
            .batch_get_rows(txn, db_id, schema.table_id, vec![new_pks.clone()], schema)
            .await?;
        if !existing.is_empty() {
            let pk_cols: Vec<_> = schema
                .pk_indices
                .iter()
                .map(|&i| schema.columns[i].name.clone())
                .collect();
            let pk_vals: Vec<_> = new_pks.iter().map(|v| format!("{}", v)).collect();
            let default_pk_name = format!(
                "{}_pkey",
                schema.name.rsplit('.').next().unwrap_or(&schema.name)
            );
            let pk_constraint_name = schema.pk_constraint_name.clone().unwrap_or(default_pk_name);
            return Err(SqlError::UniqueViolation {
                constraint: pk_constraint_name.clone(),
                message: format!(
                    "duplicate key value violates unique constraint \"{}\"\nDETAIL:  Key ({})=({}) already exists.",
                    pk_constraint_name, pk_cols.join(", "), pk_vals.join(", ")
                ),
            }.into());
        }
    }

    for index in &schema.indexes {
        if !pk_changed && index_helpers::index_values_unchanged(index, schema, old_row, &new_row)? {
            continue;
        }

        let gin_hashes = extract_gin_token_hashes_from_row(schema, index, old_row)?;
        if !gin_hashes.is_empty() {
            store
                .delete_gin_index_entries(
                    txn,
                    db_id,
                    schema.table_id,
                    index.id,
                    &gin_hashes,
                    &old_pks,
                )
                .await?;
            continue;
        }

        if !index_helpers::is_index_materializable(index) {
            continue;
        }
        let old_matches = index_helpers::eval_index_predicate(index, schema, old_row)?;
        if old_matches {
            let old_idx = index_helpers::get_index_values_with_expressions(index, schema, old_row)?;
            store
                .delete_index_entry(
                    txn,
                    db_id,
                    schema.table_id,
                    index.id,
                    &old_idx,
                    &old_pks,
                    index.unique,
                )
                .await?;
        }
    }

    if pk_changed {
        store.delete_by_pk(txn, db_id, table_name, &old_pks).await?;
    }

    store
        .upsert(txn, db_id, table_name, new_row.clone())
        .await?;

    for index in &schema.indexes {
        if !pk_changed && index_helpers::index_values_unchanged(index, schema, old_row, &new_row)? {
            continue;
        }

        let gin_hashes = extract_gin_token_hashes_from_row(schema, index, &new_row)?;
        if !gin_hashes.is_empty() {
            store
                .create_gin_index_entries(
                    txn,
                    db_id,
                    schema.table_id,
                    index.id,
                    &gin_hashes,
                    &new_pks,
                )
                .await?;
            continue;
        }

        if !index_helpers::is_index_materializable(index) {
            continue;
        }
        let new_matches = index_helpers::eval_index_predicate(index, schema, &new_row)?;
        if new_matches {
            let new_idx =
                index_helpers::get_index_values_with_expressions(index, schema, &new_row)?;
            store
                .create_index_entry(
                    txn,
                    db_id,
                    schema.table_id,
                    index.id,
                    &new_idx,
                    &new_pks,
                    index.unique,
                )
                .await?;
        }
    }

    handle_foreign_key_on_update(store, txn, db_id, table_name, schema, old_row, &new_row).await?;
    Ok(new_row)
}

async fn eval_default_expr_maybe_sequence(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    expr_str: &str,
) -> Result<Value> {
    let sql = format!("SELECT {}", expr_str);
    let dialect = PostgreSqlDialect {};
    let ast = Parser::parse_sql(&dialect, &sql)
        .map_err(|e| anyhow!("Failed to parse default expr: {}", e))?;

    if let Some(sqlparser::ast::Statement::Query(q)) = ast.into_iter().next() {
        if let sqlparser::ast::SetExpr::Select(s) = *q.body {
            if let Some(sqlparser::ast::SelectItem::UnnamedExpr(e)) =
                s.projection.into_iter().next()
            {
                return if sequences::expr_needs_async_eval(&e) {
                    sequences::eval_expr_with_sequences(
                        store,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &e,
                        None,
                        None,
                    )
                    .await
                } else {
                    super::expr::bridge::eval_const_ast_expr(&e)
                };
            }
        }
    }

    Ok(Value::Text(expr_str.to_string()))
}

async fn eval_column_default_or_null_inner(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    schema: &TableSchema,
    column_idx: usize,
    sequence_defs: Option<&[crate::types::SequenceDef]>,
) -> Result<Value> {
    let column = schema
        .columns
        .get(column_idx)
        .ok_or_else(|| anyhow!("Column index {} out of bounds", column_idx))?;

    if column.is_serial {
        let (table_schema, table_name) = schema
            .name
            .rsplit_once('.')
            .unwrap_or(("public", schema.name.as_str()));

        let seq_full_name = match sequence_defs {
            Some(defs) => {
                match sequences::find_owned_sequence_full_name(defs, &schema.name, &column.name)? {
                    Some(full_name) => full_name,
                    None => format!(
                        "{}.{}",
                        table_schema,
                        sequences::implicit_sequence_name(table_name, &column.name)
                    ),
                }
            }
            None => {
                let defs = store.list_sequences(txn, db_id).await?;
                match sequences::find_owned_sequence_full_name(&defs, &schema.name, &column.name)? {
                    Some(full_name) => full_name,
                    None => format!(
                        "{}.{}",
                        table_schema,
                        sequences::implicit_sequence_name(table_name, &column.name)
                    ),
                }
            }
        };

        let seq_val = store.nextval_sequence(txn, db_id, &seq_full_name).await?;
        sequence_values.insert(seq_full_name, seq_val);
        return match column.data_type {
            DataType::Int64 => Ok(Value::Int64(seq_val)),
            _ => Ok(Value::Int32(seq_val.try_into().map_err(|_| {
                anyhow!(
                    "serial sequence value {} overflows INT4 for column \"{}\"",
                    seq_val,
                    column.name
                )
            })?)),
        };
    }

    if let Some(def) = &column.default_expr {
        return eval_default_expr_maybe_sequence(
            store,
            txn,
            db_id,
            sequence_values,
            search_path,
            def,
        )
        .await;
    }

    Ok(Value::Null)
}

pub async fn eval_column_default_or_null(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    schema: &TableSchema,
    column_idx: usize,
) -> Result<Value> {
    eval_column_default_or_null_inner(
        store,
        txn,
        db_id,
        sequence_values,
        search_path,
        schema,
        column_idx,
        None,
    )
    .await
}

pub async fn fill_missing_columns(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    schema: &TableSchema,
    row_vals: &mut Vec<Value>,
    indices: &[usize],
) -> Result<()> {
    let sequence_defs = if schema
        .columns
        .iter()
        .enumerate()
        .any(|(i, col)| col.is_serial && !indices.contains(&i))
    {
        Some(store.list_sequences(txn, db_id).await?)
    } else {
        None
    };

    for (i, _) in schema.columns.iter().enumerate() {
        if indices.contains(&i) {
            continue;
        }

        row_vals[i] = eval_column_default_or_null_inner(
            store,
            txn,
            db_id,
            sequence_values,
            search_path,
            schema,
            i,
            sequence_defs.as_deref(),
        )
        .await?;
    }
    Ok(())
}

pub fn coerce_row_values(schema: &TableSchema, row_vals: &mut Vec<Value>) -> Result<()> {
    for (i, c) in schema.columns.iter().enumerate() {
        let coerced = coerce_value_for_column(row_vals[i].clone(), c)?;
        if coerced == Value::Null && !c.nullable {
            let short_table = schema.name.rsplit('.').next().unwrap_or(&schema.name);
            let row_str = row_vals
                .iter()
                .map(|v| format!("{}", v))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(SqlError::NotNullViolation {
                column: c.name.clone(),
                relation: short_table.to_string(),
                message: format!(
                    "null value in column \"{}\" of relation \"{}\" violates not-null constraint\nDETAIL:  Failing row contains ({}).",
                    c.name, short_table, row_str
                ),
            }.into());
        }
        row_vals[i] = coerced;
    }
    Ok(())
}

pub fn coerce_row_values_allow_null(schema: &TableSchema, row_vals: &mut Vec<Value>) -> Result<()> {
    for (i, c) in schema.columns.iter().enumerate() {
        row_vals[i] = coerce_value_for_column(row_vals[i].clone(), c)?;
    }
    Ok(())
}

pub fn validate_check_constraints(schema: &TableSchema, row: &Row) -> Result<()> {
    let dialect = PostgreSqlDialect {};
    let table_name = schema.name.rsplit('.').next().unwrap_or(&schema.name);
    for check in &schema.check_constraints {
        let expr = Parser::new(&dialect)
            .try_with_sql(&check.expr)
            .and_then(|mut p| p.parse_expr())
            .map_err(|e| anyhow!("Invalid CHECK expression '{}': {}", check.expr, e))?;

        let result = super::expr::bridge::eval_ast_expr_with_row(&expr, row, schema, table_name)?;

        match result {
            Value::Boolean(true) => {}
            Value::Boolean(false) => {
                let name = check
                    .name
                    .as_ref()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| format!("({})", check.expr));
                let short_table = schema.name.rsplit('.').next().unwrap_or(&schema.name);
                let row_str = row
                    .values
                    .iter()
                    .map(|v| format!("{}", v))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(SqlError::CheckViolation {
                    table: short_table.to_string(),
                    constraint: name,
                    detail: row_str,
                }
                .into());
            }
            Value::Null => {}
            _ => {
                return Err(anyhow!(
                    "CHECK constraint must evaluate to boolean, got {:?}",
                    result
                ));
            }
        }
    }
    Ok(())
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
            return Err(anyhow!(
                "insert or update on table \"{}\" violates foreign key constraint \"{}\"\n\
                 DETAIL:  Key ({})=({}) is not present in table \"{}\".",
                schema.name,
                fk.name,
                cols,
                vals.join(", "),
                fk.ref_table
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ColumnDef;

    fn enum_schema() -> TableSchema {
        TableSchema {
            name: "t".to_string(),
            table_id: 1,
            columns: vec![ColumnDef {
                name: "r".to_string(),
                data_type: DataType::UserDefined("public.role".to_string()),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    fn enum_cache() -> EnumLabelCache {
        let mut cache: EnumLabelCache = HashMap::new();
        cache.insert(
            "public.role".to_string(),
            ["USER", "ADMIN"]
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
        );
        cache
    }

    #[test]
    fn enum_validation_allows_null() {
        let schema = enum_schema();
        let cache = enum_cache();
        let row = Row::new(vec![Value::Null]);
        validate_enum_values(&schema, &row, &cache).unwrap();
    }

    #[test]
    fn enum_validation_rejects_unknown_label() {
        let schema = enum_schema();
        let cache = enum_cache();
        let row = Row::new(vec![Value::Text("INVALID".to_string())]);
        let err = validate_enum_values(&schema, &row, &cache).unwrap_err();
        assert!(err
            .to_string()
            .contains("invalid input value for enum public.role"));
    }

    #[test]
    fn enum_validation_is_case_sensitive() {
        let schema = enum_schema();
        let cache = enum_cache();
        let row = Row::new(vec![Value::Text("user".to_string())]);
        let err = validate_enum_values(&schema, &row, &cache).unwrap_err();
        assert!(err
            .to_string()
            .contains("invalid input value for enum public.role"));
    }

    #[test]
    fn enum_validation_requires_text() {
        let schema = enum_schema();
        let cache = enum_cache();
        let row = Row::new(vec![Value::Int32(1)]);
        let err = validate_enum_values(&schema, &row, &cache).unwrap_err();
        assert!(err
            .to_string()
            .contains("invalid input value for enum public.role"));
    }
}
