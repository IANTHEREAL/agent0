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

use crate::sql::value_coercion::coerce_value_for_column;

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

/// Extract child FK column values from a row.
/// Returns `None` for MATCH SIMPLE-null rows or malformed schemas.
fn fk_values_for_row(
    fk: &ForeignKeyConstraint,
    child_schema: &TableSchema,
    child_row: &Row,
) -> Option<Vec<Value>> {
    let mut values = Vec::with_capacity(fk.columns.len());
    for col_name in &fk.columns {
        let idx = child_schema.column_index(col_name)?;
        let val = child_row.values[idx].clone();
        if val == Value::Null {
            return None;
        }
        values.push(val);
    }
    Some(values)
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

/// Statement-scoped context for FK enforcement during DELETE.
///
/// Built once per statement and reused for every row in that DELETE statement.
#[derive(Default)]
pub(crate) struct FkDeleteContext {
    /// Snapshot rows for tables that define foreign keys.
    /// This is kept in sync as cascades mutate data.
    pub table_rows: HashMap<String, Vec<Row>>,
    /// Schemas for tables that define foreign keys.
    pub table_schemas: HashMap<String, TableSchema>,
    /// Per-table set of deleted PK hash keys, used to prevent redundant work
    /// and infinite cascade cycles.
    pub deleted_pks: HashMap<String, HashSet<String>>,
}

impl FkDeleteContext {
    pub async fn build(store: &Arc<TikvStore>, txn: &mut Transaction, db_id: u64) -> Result<Self> {
        let table_names = store.list_tables(txn, db_id).await?;
        let mut table_rows = HashMap::new();
        let mut table_schemas = HashMap::new();

        for table_name in &table_names {
            if let Some(schema) = store.get_schema(txn, db_id, table_name).await? {
                if schema.foreign_keys.is_empty() {
                    continue;
                }
                let rows = store.scan(txn, db_id, table_name, None).await?;
                table_rows.insert(table_name.clone(), rows);
                table_schemas.insert(table_name.clone(), schema);
            }
        }

        Ok(Self {
            table_rows,
            table_schemas,
            deleted_pks: HashMap::new(),
        })
    }

    fn mark_deleted_pk(&mut self, table_name: &str, pk: &[Value]) {
        self.deleted_pks
            .entry(table_name.to_string())
            .or_default()
            .insert(pk_to_hash_key(pk));
    }

    fn is_pk_marked_deleted(&self, table_name: &str, pk: &[Value]) -> bool {
        let Some(marked) = self.deleted_pks.get(table_name) else {
            return false;
        };
        marked.contains(&pk_to_hash_key(pk))
    }

    fn remove_row_from_snapshot(&mut self, table_name: &str, schema: &TableSchema, row: &Row) {
        let pk = schema.get_pk_values(row);
        let pk_key = pk_to_hash_key(&pk);
        if let Some(rows) = self.table_rows.get_mut(table_name) {
            rows.retain(|r| pk_to_hash_key(&schema.get_pk_values(r)) != pk_key);
        }
    }

    fn replace_row_in_snapshot(
        &mut self,
        table_name: &str,
        schema: &TableSchema,
        old_row: &Row,
        new_row: Row,
    ) {
        let old_pk_key = pk_to_hash_key(&schema.get_pk_values(old_row));
        if let Some(rows) = self.table_rows.get_mut(table_name) {
            if let Some(existing) = rows
                .iter_mut()
                .find(|r| pk_to_hash_key(&schema.get_pk_values(r)) == old_pk_key)
            {
                *existing = new_row;
            }
        }
    }

    pub(crate) fn on_statement_row_deleted(
        &mut self,
        table_name: &str,
        schema: &TableSchema,
        row: &Row,
    ) {
        self.remove_row_from_snapshot(table_name, schema, row);
    }
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
    fk_ctx: &mut FkDeleteContext,
) -> Result<()> {
    if fk_ctx.table_schemas.is_empty() {
        return Ok(());
    }

    // Pre-seed with the current row being deleted to prevent cyclic
    // self-cascades from re-visiting it.
    fk_ctx.mark_deleted_pk(table_name, &schema.get_pk_values(row));

    Box::pin(cascade_delete_recursive(
        store,
        txn,
        db_id,
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
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    parent_row: &Row,
    parent_schema: &TableSchema,
    fk_ctx: &mut FkDeleteContext,
    stmt_deleting_pks: &HashSet<String>,
    stmt_target_table: &str,
) -> Result<()> {
    use crate::types::ForeignKeyAction;

    let table_schema_entries: Vec<(String, TableSchema)> = fk_ctx
        .table_schemas
        .iter()
        .map(|(name, schema)| (name.clone(), schema.clone()))
        .collect();

    for (other_table, other_schema) in table_schema_entries {
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

            let all_rows = fk_ctx
                .table_rows
                .get(&other_table)
                .cloned()
                .unwrap_or_default();

            let mut rows_to_cascade: Vec<Row> = Vec::new();
            let mut rows_to_update: Vec<(Row, Row)> = Vec::new();

            for other_row in &all_rows {
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

                let Some(fk_values) = fk_values_for_row(fk, &other_schema, other_row) else {
                    continue;
                };

                if fk_values == ref_values {
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
                            let cols = ref_column_names(fk, parent_schema);
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

                // Record as deleted BEFORE recursion to prevent infinite cycles
                // on self-referencing or mutually-referencing rows.
                fk_ctx.mark_deleted_pk(&other_table, &del_pk);

                Box::pin(cascade_delete_recursive(
                    store,
                    txn,
                    db_id,
                    &other_table,
                    &del_row,
                    &other_schema,
                    fk_ctx,
                    stmt_deleting_pks,
                    stmt_target_table,
                ))
                .await?;

                super::delete::delete_row_storage_entries(
                    store,
                    txn,
                    db_id,
                    &other_table,
                    &other_schema,
                    &del_row,
                )
                .await?;

                // Keep in-memory snapshot in sync for later parent-row processing.
                fk_ctx.remove_row_from_snapshot(&other_table, &other_schema, &del_row);
            }

            if !rows_to_update.is_empty() {
                let enum_cache = build_enum_label_cache(store, txn, db_id, &other_schema).await?;
                for (old_row, new_row) in rows_to_update {
                    let updated_row = execute_update_row(
                        store,
                        txn,
                        db_id,
                        &other_table,
                        &other_schema,
                        &old_row,
                        new_row,
                        &enum_cache,
                        Some(fk_ctx),
                    )
                    .await?;

                    // Root invariant for ON DELETE SET DEFAULT:
                    // the final child FK values must not keep pointing to the
                    // parent key being deleted by this delete-cascade frame.
                    if matches!(fk.on_delete, ForeignKeyAction::SetDefault) {
                        if let Some(post_fk_values) =
                            fk_values_for_row(fk, &other_schema, &updated_row)
                        {
                            if post_fk_values == ref_values {
                                let cols = ref_column_names(fk, parent_schema);
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

                    // Use the applied row returned by UPDATE to preserve exact
                    // in-memory snapshot parity with storage writes.
                    fk_ctx.replace_row_in_snapshot(
                        &other_table,
                        &other_schema,
                        &old_row,
                        updated_row,
                    );
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
    fk_ctx: Option<&mut FkDeleteContext>,
) -> Result<()> {
    if let Some(ctx) = fk_ctx {
        return handle_foreign_key_on_update_with_ctx(
            store, txn, db_id, table_name, schema, old_row, new_row, ctx,
        )
        .await;
    }
    handle_foreign_key_on_update_no_ctx(store, txn, db_id, table_name, schema, old_row, new_row)
        .await
}

async fn handle_foreign_key_on_update_with_ctx(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    schema: &TableSchema,
    old_row: &Row,
    new_row: &Row,
    fk_ctx: &mut FkDeleteContext,
) -> Result<()> {
    if fk_ctx.table_schemas.is_empty() {
        return Ok(());
    }

    let fk_tables: Vec<String> = fk_ctx.table_schemas.keys().cloned().collect();
    for other_table in fk_tables {
        let Some(other_schema) = fk_ctx.table_schemas.get(&other_table).cloned() else {
            continue;
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

            let all_rows = fk_ctx
                .table_rows
                .get(&other_table)
                .cloned()
                .unwrap_or_default();
            let rows_to_update = collect_fk_update_rows(
                fk,
                schema,
                table_name,
                &other_table,
                &other_schema,
                &all_rows,
                &old_ref_values,
                &new_ref_values,
            )?;

            if !rows_to_update.is_empty() {
                let enum_cache = build_enum_label_cache(store, txn, db_id, &other_schema).await?;
                for (old_child_row, new_child_row) in rows_to_update {
                    let updated_row = Box::pin(execute_update_row(
                        store,
                        txn,
                        db_id,
                        &other_table,
                        &other_schema,
                        &old_child_row,
                        new_child_row,
                        &enum_cache,
                        Some(fk_ctx),
                    ))
                    .await?;
                    fk_ctx.replace_row_in_snapshot(
                        &other_table,
                        &other_schema,
                        &old_child_row,
                        updated_row,
                    );
                }
            }
        }
    }

    Ok(())
}

async fn handle_foreign_key_on_update_no_ctx(
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

            match fk.on_update {
                ForeignKeyAction::NoAction | ForeignKeyAction::Restrict => {
                    // Single scan: error on first child still referencing old parent key.
                    let all_rows = store.scan(txn, db_id, other_table, None).await?;
                    for other_row in &all_rows {
                        let Some(fk_values) = fk_values_for_row(fk, &other_schema, other_row)
                        else {
                            continue;
                        };
                        if fk_values == old_ref_values {
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

                ForeignKeyAction::Cascade
                | ForeignKeyAction::SetNull
                | ForeignKeyAction::SetDefault => {
                    // Fixpoint: scan → find first match → update → repeat.
                    // Each iteration re-reads from storage to see prior side-effects,
                    // eliminating the intra-batch staleness bug (#924 parity for UPDATE).
                    let enum_cache =
                        build_enum_label_cache(store, txn, db_id, &other_schema).await?;
                    loop {
                        let all_rows = store.scan(txn, db_id, other_table, None).await?;
                        let child_row = all_rows.iter().find_map(|r| {
                            let fk_vals = fk_values_for_row(fk, &other_schema, r)?;
                            if fk_vals == old_ref_values {
                                Some(r.clone())
                            } else {
                                None
                            }
                        });
                        let child_row = match child_row {
                            None => break,
                            Some(r) => r,
                        };

                        let mut new_values = child_row.values.clone();
                        match fk.on_update {
                            ForeignKeyAction::Cascade => {
                                for (i, col_name) in fk.columns.iter().enumerate() {
                                    if let Some(idx) = other_schema.column_index(col_name) {
                                        if i < new_ref_values.len() {
                                            new_values[idx] = new_ref_values[i].clone();
                                        }
                                    }
                                }
                            }
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

                        // Convergence check: if new FK values still match
                        // old_ref_values (e.g. SET DEFAULT where default ==
                        // old parent key), the child still references the
                        // departed parent value. Produce PG-parity parent-side
                        // error before execute_update_row tries child-side
                        // validate_foreign_keys.
                        let mut post_fk_values: Vec<Value> = Vec::new();
                        for col_name in &fk.columns {
                            if let Some(idx) = other_schema.column_index(col_name) {
                                let col = &other_schema.columns[idx];
                                post_fk_values
                                    .push(coerce_value_for_column(new_values[idx].clone(), col)?);
                            }
                        }
                        if post_fk_values == old_ref_values {
                            let cols = ref_column_names(fk, schema);
                            let ref_val_strs: Vec<String> =
                                old_ref_values.iter().map(|v| format!("{}", v)).collect();
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

                        Box::pin(execute_update_row(
                            store,
                            txn,
                            db_id,
                            other_table,
                            &other_schema,
                            &child_row,
                            Row::new(new_values),
                            &enum_cache,
                            None,
                        ))
                        .await?;
                    }
                }
            }
        }
    }

    Ok(())
}

fn collect_fk_update_rows(
    fk: &ForeignKeyConstraint,
    schema: &TableSchema,
    table_name: &str,
    other_table: &str,
    other_schema: &TableSchema,
    all_rows: &[Row],
    old_ref_values: &[Value],
    new_ref_values: &[Value],
) -> Result<Vec<(Row, Row)>> {
    use crate::types::ForeignKeyAction;

    let mut rows_to_update: Vec<(Row, Row)> = Vec::new();
    for other_row in all_rows {
        let Some(fk_values) = fk_values_for_row(fk, other_schema, other_row) else {
            continue;
        };
        if fk_values != old_ref_values {
            continue;
        }

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

    Ok(rows_to_update)
}
