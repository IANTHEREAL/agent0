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
use super::update::{execute_update_row, execute_update_row_without_fk_update};

/// Read-only context grouping the immutable store reference and database ID
/// for FK operations. Keeps `txn: &mut Transaction` separate to avoid
/// borrow-checker complications in recursive async functions.
pub(crate) struct FkStoreCtx<'a> {
    pub store: &'a Arc<TikvStore>,
    pub db_id: u64,
}

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

    fn find_row_in_snapshot(
        &self,
        table_name: &str,
        schema: &TableSchema,
        pk: &[Value],
    ) -> Option<Row> {
        let target_pk = pk_to_hash_key(pk);
        self.table_rows.get(table_name).and_then(|rows| {
            rows.iter()
                .find(|r| pk_to_hash_key(&schema.get_pk_values(r)) == target_pk)
                .cloned()
        })
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
            let fk = fk.clone();

            let ref_values = get_ref_values(&fk, parent_schema, parent_row)?;

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
            let mut rows_to_update_pks: Vec<Vec<Value>> = Vec::new();

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

                super::delete::delete_row_storage_entries(
                    ctx.store,
                    txn,
                    ctx.db_id,
                    &other_table,
                    &other_schema,
                    &del_row,
                )
                .await?;

                // Keep in-memory snapshot in sync for later parent-row processing.
                fk_ctx.remove_row_from_snapshot(&other_table, &other_schema, &del_row);
            }

            if !rows_to_update_pks.is_empty() {
                let enum_cache =
                    build_enum_label_cache(ctx.store, txn, ctx.db_id, &other_schema).await?;
                for target_pk in rows_to_update_pks {
                    let Some(current_row) =
                        fk_ctx.find_row_in_snapshot(&other_table, &other_schema, &target_pk)
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

                    // Use the applied row returned by UPDATE to preserve exact
                    // in-memory snapshot parity with storage writes.
                    fk_ctx.replace_row_in_snapshot(
                        &other_table,
                        &other_schema,
                        &current_row,
                        updated_row,
                    );
                }
            }
        }
    }
    Ok(())
}

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
    use crate::types::ForeignKeyAction;

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
