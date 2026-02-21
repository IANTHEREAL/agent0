//! Foreign key constraint validation and cascade operations (DELETE/UPDATE).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::Result;
use tikv_client::Transaction;

use crate::sql::error::SqlError;
use crate::sql::expr::compare_values;
use crate::sql::projection::eval_default_expr;
use crate::storage::TikvStore;
use crate::types::{DataType, ForeignKeyConstraint, Row, TableSchema, Value};
use crate::worker::types::IndexState;

use super::insert::build_enum_label_cache;
use super::update::execute_update_row;

fn short_relation_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// Deterministic string key from a PK value vector for HashSet membership.
pub(crate) fn pk_to_hash_key(pk: &[Value]) -> String {
    use std::fmt::Write;
    let mut key = String::new();
    for (i, v) in pk.iter().enumerate() {
        if i > 0 {
            key.push('\x00');
        }
        let _ = write!(key, "{:?}", v);
    }
    key
}

/// Extract referenced-column values from a parent row for a given FK.
/// Falls back to PK values when ref_columns is empty (implicit PK reference).
fn get_ref_values(
    fk: &ForeignKeyConstraint,
    parent_schema: &TableSchema,
    parent_row: &Row,
) -> Result<Vec<Value>> {
    if fk.ref_columns.is_empty() {
        Ok(parent_schema.get_pk_values(parent_row))
    } else {
        fk.ref_columns
            .iter()
            .map(|col_name| {
                let idx = parent_schema.column_index(col_name).ok_or_else(|| {
                    anyhow!(
                        "FK ref_column '{}' not found in parent schema '{}'",
                        col_name,
                        parent_schema.name
                    )
                })?;
                Ok(parent_row.values[idx].clone())
            })
            .collect()
    }
}

/// Return the referenced column names for error messages.
/// Falls back to PK column names when ref_columns is empty.
fn ref_column_names(fk: &ForeignKeyConstraint, parent_schema: &TableSchema) -> String {
    if fk.ref_columns.is_empty() {
        parent_schema
            .pk_indices
            .iter()
            .map(|&i| parent_schema.columns[i].name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        fk.ref_columns.join(", ")
    }
}

/// Lookup strategy for FK parent row existence checks.
#[derive(Debug)]
pub(crate) enum FkRefLookup {
    /// `ref_columns` maps to parent primary key columns.
    Pk,
    /// `ref_columns` maps to an eligible unique index on the parent.
    UniqueIndex {
        index_id: u64,
        pk_types: Vec<DataType>,
    },
}

/// Resolve lookup strategy for referenced columns against parent schema.
///
/// Rules:
/// - empty `ref_columns` => parent PK (error if parent has no PK)
/// - exact positional PK column match => PK lookup
/// - eligible unique index match => index lookup
/// - otherwise => unique-constraint-missing error
pub(crate) fn resolve_fk_ref_lookup(
    ref_columns: &[String],
    ref_schema: &TableSchema,
) -> Result<FkRefLookup> {
    let pk_col_names: Vec<&str> = ref_schema
        .pk_indices
        .iter()
        .map(|&i| ref_schema.columns[i].name.as_str())
        .collect();

    if ref_columns.is_empty() {
        if ref_schema.pk_indices.is_empty() {
            return Err(anyhow!(
                "there is no primary key for referenced table \"{}\"",
                short_relation_name(&ref_schema.name)
            ));
        }
        return Ok(FkRefLookup::Pk);
    }

    if ref_columns.len() == pk_col_names.len()
        && ref_columns.iter().zip(&pk_col_names).all(|(a, b)| a == *b)
    {
        return Ok(FkRefLookup::Pk);
    }

    for index in &ref_schema.indexes {
        if !index.unique || index.state != IndexState::Ready {
            continue;
        }
        if index.predicate.is_some() || !index.expressions.is_empty() {
            continue;
        }
        if let Some(method) = &index.method {
            if !method.eq_ignore_ascii_case("btree") {
                continue;
            }
        }
        if index.columns.len() == ref_columns.len()
            && index.columns.iter().zip(ref_columns).all(|(a, b)| a == b)
        {
            let pk_types = if ref_schema.pk_indices.is_empty() {
                vec![DataType::Uuid]
            } else {
                ref_schema
                    .pk_indices
                    .iter()
                    .map(|&i| ref_schema.columns[i].data_type.clone())
                    .collect()
            };
            return Ok(FkRefLookup::UniqueIndex {
                index_id: index.id,
                pk_types,
            });
        }
    }

    Err(anyhow!(
        "there is no unique constraint matching given keys for referenced table \"{}\"",
        short_relation_name(&ref_schema.name)
    ))
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
        let mut any_null = false;

        for col_name in &fk.columns {
            let col_idx = schema
                .column_index(col_name)
                .ok_or_else(|| anyhow!("FK column '{}' not found in schema", col_name))?;
            let val = row.values[col_idx].clone();
            if val == Value::Null {
                any_null = true;
            }
            fk_values.push(val);
        }

        // MATCH SIMPLE: skip FK check if any referencing column is NULL.
        if any_null {
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

        let lookup = resolve_fk_ref_lookup(&fk.ref_columns, &ref_schema)?;

        // Self-referencing FK: if the row's FK column values match its own
        // referenced column values, the constraint is trivially satisfied once
        // the row is written.  Skip the storage lookup.
        // (PostgreSQL validates at statement end where the row is visible.)
        // Use compare_values() for PG-like equality semantics (e.g. NaN = NaN).
        if fk.ref_table == schema.name {
            let self_ref_vals = get_ref_values(fk, &ref_schema, row)?;
            if fk_values.len() == self_ref_vals.len()
                && fk_values
                    .iter()
                    .zip(&self_ref_vals)
                    .all(|(a, b)| compare_values(a, b).map_or(false, |c| c == 0))
            {
                continue;
            }
        }

        let parent_exists = match lookup {
            FkRefLookup::Pk => {
                let ref_rows = store
                    .batch_get_rows(
                        txn,
                        db_id,
                        ref_schema.table_id,
                        vec![fk_values.clone()],
                        &ref_schema,
                    )
                    .await?;
                !ref_rows.is_empty()
            }
            FkRefLookup::UniqueIndex { index_id, pk_types } => {
                let pks = store
                    .scan_index(
                        txn,
                        db_id,
                        ref_schema.table_id,
                        index_id,
                        &fk_values,
                        true,
                        &pk_types,
                        Some(1),
                    )
                    .await?;
                !pks.is_empty()
            }
        };

        if !parent_exists {
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
    stmt_deleting_pks: &HashSet<String>,
) -> Result<()> {
    let table_names = store.list_tables(txn, db_id).await?;
    let mut table_schemas: HashMap<String, TableSchema> = HashMap::new();
    for t in &table_names {
        if let Some(s) = store.get_schema(txn, db_id, t).await? {
            if !s.foreign_keys.is_empty() {
                table_schemas.insert(t.clone(), s);
            }
        }
    }

    let mut deleted_pks: HashMap<String, Vec<Vec<Value>>> = HashMap::new();
    // Pre-seed with the current row being deleted to prevent cyclic
    // self-cascades from re-visiting it.
    deleted_pks
        .entry(table_name.to_string())
        .or_default()
        .push(schema.get_pk_values(row));

    Box::pin(cascade_delete_recursive(
        store,
        txn,
        db_id,
        table_name,
        row,
        schema,
        &table_schemas,
        &mut deleted_pks,
        stmt_deleting_pks,
        table_name,
    ))
    .await
}

/// Find the first child row whose FK columns match `ref_values`.
/// Skips NULL FK columns (MATCH SIMPLE), already-deleted rows, the self-ref parent,
/// and rows being deleted by the same statement (statement-level deferral).
/// Returns an owned `Row` (clone) to avoid borrow-checker conflicts with `deleted_pks`
/// mutation in the caller.
fn find_first_fk_match(
    rows: &[Row],
    other_schema: &TableSchema,
    fk: &ForeignKeyConstraint,
    ref_values: &[Value],
    deleted_in_table: Option<&Vec<Vec<Value>>>,
    self_ref_parent_pk: Option<&Vec<Value>>,
    stmt_deleting_pks: &HashSet<String>,
    is_stmt_target_table: bool,
) -> Option<Row> {
    for row in rows {
        let pk = other_schema.get_pk_values(row);
        if let Some(deleted) = deleted_in_table {
            if deleted.contains(&pk) {
                continue;
            }
        }
        // Statement-level deferral: skip referencing rows that are
        // also being deleted by the same DELETE statement.
        if is_stmt_target_table && stmt_deleting_pks.contains(&pk_to_hash_key(&pk)) {
            continue;
        }
        if let Some(parent_pk) = self_ref_parent_pk {
            if pk == *parent_pk {
                continue;
            }
        }
        let mut fk_values: Vec<Value> = Vec::new();
        let mut any_null = false;
        for col_name in &fk.columns {
            if let Some(idx) = other_schema.column_index(col_name) {
                let val = row.values[idx].clone();
                if val == Value::Null {
                    any_null = true;
                }
                fk_values.push(val);
            }
        }
        if any_null {
            continue;
        }
        if fk_values == ref_values {
            return Some(row.clone());
        }
    }
    None
}

/// Fixpoint-based cascade delete: processes one matching child row at a time,
/// re-scanning from storage before each iteration to see the latest transactional state.
/// This eliminates all staleness vectors (cross-FK, intra-batch, PK mutation).
async fn cascade_delete_recursive(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    parent_row: &Row,
    parent_schema: &TableSchema,
    table_schemas: &HashMap<String, TableSchema>,
    deleted_pks: &mut HashMap<String, Vec<Vec<Value>>>,
    stmt_deleting_pks: &HashSet<String>,
    stmt_target_table: &str,
) -> Result<()> {
    use crate::types::ForeignKeyAction;

    for (other_table, other_schema) in table_schemas {
        for fk in &other_schema.foreign_keys {
            if fk.ref_table != table_name {
                continue;
            }

            let ref_values = get_ref_values(fk, parent_schema, parent_row)?;

            // For self-referencing FKs, skip the row being deleted itself.
            let self_ref_parent_pk = if other_table.as_str() == table_name {
                Some(parent_schema.get_pk_values(parent_row))
            } else {
                None
            };

            let is_stmt_target = other_table.as_str() == stmt_target_table;

            match fk.on_delete {
                ForeignKeyAction::NoAction | ForeignKeyAction::Restrict => {
                    // One-shot scan: error if any child still references this parent.
                    let all_rows = store.scan(txn, db_id, other_table, None).await?;
                    if find_first_fk_match(
                        &all_rows,
                        other_schema,
                        fk,
                        &ref_values,
                        deleted_pks.get(other_table),
                        self_ref_parent_pk.as_ref(),
                        stmt_deleting_pks,
                        is_stmt_target,
                    )
                    .is_some()
                    {
                        let cols = ref_column_names(fk, parent_schema);
                        let ref_val_strs: Vec<String> =
                            ref_values.iter().map(|v| format!("{}", v)).collect();
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
                                ref_val_strs.join(", "),
                                short_other_table
                            ),
                        }
                        .into());
                    }
                }

                ForeignKeyAction::Cascade => {
                    // Fixpoint: scan → find first match → mark deleted → recurse → delete → repeat.
                    loop {
                        let all_rows = store.scan(txn, db_id, other_table, None).await?;
                        let matched = find_first_fk_match(
                            &all_rows,
                            other_schema,
                            fk,
                            &ref_values,
                            deleted_pks.get(other_table),
                            self_ref_parent_pk.as_ref(),
                            stmt_deleting_pks,
                            is_stmt_target,
                        );
                        let del_row = match matched {
                            None => break,
                            Some(r) => r,
                        };
                        let del_pk = other_schema.get_pk_values(&del_row);
                        deleted_pks
                            .entry(other_table.clone())
                            .or_default()
                            .push(del_pk);

                        Box::pin(cascade_delete_recursive(
                            store,
                            txn,
                            db_id,
                            other_table,
                            &del_row,
                            other_schema,
                            table_schemas,
                            deleted_pks,
                            stmt_deleting_pks,
                            stmt_target_table,
                        ))
                        .await?;

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
                }

                ForeignKeyAction::SetNull | ForeignKeyAction::SetDefault => {
                    // Fixpoint: scan → find first match → compute new values → update → repeat.
                    // Each iteration re-reads from storage to see all prior side-effects.
                    let enum_cache =
                        build_enum_label_cache(store, txn, db_id, other_schema).await?;
                    loop {
                        let all_rows = store.scan(txn, db_id, other_table, None).await?;
                        let matched = find_first_fk_match(
                            &all_rows,
                            other_schema,
                            fk,
                            &ref_values,
                            deleted_pks.get(other_table),
                            self_ref_parent_pk.as_ref(),
                            stmt_deleting_pks,
                            is_stmt_target,
                        );
                        let fresh_row = match matched {
                            None => break,
                            Some(r) => r,
                        };

                        let mut new_values = fresh_row.values.clone();
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
                                        new_values[idx] =
                                            if let Some(ref def_expr) = col.default_expr {
                                                eval_default_expr(def_expr)?
                                            } else {
                                                Value::Null
                                            };
                                    }
                                }
                            }
                            _ => unreachable!(),
                        }

                        let updated_row = execute_update_row(
                            store,
                            txn,
                            db_id,
                            other_table,
                            other_schema,
                            &fresh_row,
                            Row::new(new_values),
                            &enum_cache,
                        )
                        .await?;

                        // Post-update convergence check: extract FK column values from
                        // the coerced result and verify they no longer match ref_values.
                        // If they still match, SET DEFAULT produced the deleted parent key
                        // (possibly after type coercion, e.g. Int32→Int64). The child
                        // still references a non-existent parent — produce the PG-compatible
                        // parent-side "still referenced" error and terminate the fixpoint.
                        let mut post_fk_values: Vec<Value> = Vec::new();
                        for col_name in &fk.columns {
                            if let Some(idx) = other_schema.column_index(col_name) {
                                post_fk_values.push(updated_row.values[idx].clone());
                            }
                        }
                        if post_fk_values == ref_values {
                            let cols = ref_column_names(fk, parent_schema);
                            let ref_val_strs: Vec<String> =
                                ref_values.iter().map(|v| format!("{}", v)).collect();
                            let short_parent = short_relation_name(table_name);
                            let short_child = short_relation_name(other_table);
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

    let table_names = store.list_tables(txn, db_id).await?;

    for other_table in &table_names {
        let other_schema = match store.get_schema(txn, db_id, other_table).await? {
            Some(s) => s,
            None => continue,
        };

        for fk in &other_schema.foreign_keys {
            if fk.ref_table != table_name {
                continue;
            }

            let old_ref_values = get_ref_values(fk, schema, old_row)?;
            let new_ref_values = get_ref_values(fk, schema, new_row)?;

            if old_ref_values == new_ref_values {
                continue;
            }

            let all_rows = store.scan(txn, db_id, other_table, None).await?;
            let mut rows_to_update: Vec<(Row, Row)> = Vec::new();

            for other_row in &all_rows {
                let mut fk_values: Vec<Value> = Vec::new();
                let mut any_null = false;

                for col_name in &fk.columns {
                    if let Some(idx) = other_schema.column_index(col_name) {
                        let val = other_row.values[idx].clone();
                        if val == Value::Null {
                            any_null = true;
                        }
                        fk_values.push(val);
                    }
                }

                if any_null {
                    continue;
                }

                if fk_values == old_ref_values {
                    match fk.on_update {
                        ForeignKeyAction::Cascade => {
                            let mut new_values = other_row.values.clone();
                            for (i, col_name) in fk.columns.iter().enumerate() {
                                if let Some(idx) = other_schema.column_index(col_name) {
                                    if i < new_ref_values.len() {
                                        new_values[idx] = new_ref_values[i].clone();
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
                            let cols = ref_column_names(fk, schema);
                            let ref_val_strs: Vec<String> =
                                old_ref_values.iter().map(|v| format!("{}", v)).collect();
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
                                    ref_val_strs.join(", "),
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
