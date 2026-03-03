//! Foreign key constraint validation and cascade operations (DELETE/UPDATE).

mod cascade_delete;
mod cascade_update;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::Result;
use tikv_client::Transaction;

use crate::model::{DataType, ForeignKeyConstraint, IndexDef, Row, TableSchema, Value};
use crate::sql::error::SqlError;
use crate::sql::expr::compare_values;
use crate::storage::TikvStore;
use crate::worker::types::IndexState;

// ── Re-exports from submodules ──────────────────────────────────────────────
pub use cascade_delete::handle_foreign_key_on_delete;
pub use cascade_update::handle_foreign_key_on_update;

/// Read-only context grouping the immutable store reference and database ID
/// for FK operations. Keeps `txn: &mut Transaction` separate to avoid
/// borrow-checker complications in recursive async functions.
pub(crate) struct FkStoreCtx<'a> {
    pub store: &'a Arc<TikvStore>,
    pub db_id: u64,
}

pub(crate) type ConstraintId = usize;

/// Statement-scoped cache of referenced table schemas for FK validation.
/// Built once per statement/batch and reused for every row, eliminating
/// per-row `store.get_schema` calls from the hot path.
pub(crate) type FkRefSchemaCache = HashMap<String, TableSchema>;

/// Prefetch schemas for all distinct FK-referenced tables.
///
/// When `skip_self_ref` is true, self-referencing FK targets are excluded
/// (used by COPY's non-self-ref validation pass).
pub(crate) async fn build_fk_ref_schema_cache(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    skip_self_ref: bool,
) -> Result<FkRefSchemaCache> {
    let mut cache = FkRefSchemaCache::new();
    for fk in &schema.foreign_keys {
        if skip_self_ref && fk.ref_table == schema.name {
            continue;
        }
        if cache.contains_key(&fk.ref_table) {
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
        cache.insert(fk.ref_table.clone(), ref_schema);
    }
    Ok(cache)
}

pub(super) fn short_relation_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// Collision-resistant string key from a PK value vector for HashSet/HashMap
/// membership.  Uses `Debug` format (which includes variant tags and escapes
/// string content) joined by NUL bytes so that composite text values with
/// embedded commas can never collide:
///
///   `[Text("a"), Text("b, c")]` → `Text("a")\0Text("b, c")`
///   `[Text("a, b"), Text("c")]` → `Text("a, b")\0Text("c")`
///
/// This function is **internal only** — never expose its output to users.
/// For user-facing DETAIL messages, use [`format_fk_detail_values`].
pub(crate) fn pk_to_hash_key(pk: &[Value]) -> String {
    pk.iter()
        .map(|v| format!("{:?}", v))
        .collect::<Vec<_>>()
        .join("\0")
}

/// Format FK column names and values for user-facing PG-compatible DETAIL
/// strings.  Produces output like `Key (col1, col2)=(val1, val2)`.
pub(crate) fn format_fk_detail_values(cols: &[String], vals: &[Value]) -> String {
    let col_str = cols.join(", ");
    let val_str: String = vals
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!("({})=({})", col_str, val_str)
}

/// Extract referenced-column values from a parent row for a given FK.
/// Falls back to PK values when ref_columns is empty (implicit PK reference).
pub(super) fn get_ref_values(
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
pub(super) fn fk_values_for_row(
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
pub(super) fn ref_column_names(fk: &ForeignKeyConstraint, parent_schema: &TableSchema) -> String {
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
/// Loads only schemas — no row preloading. All row lookups are lazy per-PK
/// targeted scans via `find_referencing_rows`.
#[derive(Default)]
pub(crate) struct FkDeleteContext {
    /// Schemas for tables that define foreign keys.
    pub table_schemas: HashMap<String, TableSchema>,
    /// Per-table set of deleted PK hash keys, used to prevent redundant work
    /// and infinite cascade cycles.
    pub deleted_pks: HashMap<String, HashSet<String>>,
}

impl FkDeleteContext {
    pub async fn build(store: &Arc<TikvStore>, txn: &mut Transaction, db_id: u64) -> Result<Self> {
        let table_names = store.list_tables(txn, db_id).await?;
        let mut table_schemas = HashMap::new();

        for table_name in &table_names {
            if let Some(schema) = store.get_schema(txn, db_id, table_name).await? {
                if schema.foreign_keys.is_empty() {
                    continue;
                }
                table_schemas.insert(table_name.clone(), schema);
            }
        }

        Ok(Self {
            table_schemas,
            deleted_pks: HashMap::new(),
        })
    }

    pub(super) fn mark_deleted_pk(&mut self, table_name: &str, pk: &[Value]) {
        self.deleted_pks
            .entry(table_name.to_string())
            .or_default()
            .insert(pk_to_hash_key(pk));
    }

    pub(super) fn is_pk_marked_deleted(&self, table_name: &str, pk: &[Value]) -> bool {
        let Some(marked) = self.deleted_pks.get(table_name) else {
            return false;
        };
        marked.contains(&pk_to_hash_key(pk))
    }
}

/// Find a btree index on `child_schema` whose leading columns cover `fk_columns`.
///
/// Returns `None` if no eligible index exists (triggers batched-filter fallback).
/// Eligible: Ready state, btree method (or NULL = btree default), no expression
/// columns, no partial-index predicate, leading columns positionally match FK columns.
pub(super) fn find_fk_covering_index<'a>(
    child_schema: &'a TableSchema,
    fk_columns: &[String],
) -> Option<&'a IndexDef> {
    child_schema.indexes.iter().find(|idx| {
        idx.state == IndexState::Ready
            && idx
                .method
                .as_ref()
                .is_none_or(|m| m.eq_ignore_ascii_case("btree"))
            && idx.expressions.is_empty()
            && idx.predicate.is_none()
            && idx.columns.len() >= fk_columns.len()
            && idx.columns[..fk_columns.len()]
                .iter()
                .zip(fk_columns)
                .all(|(a, b)| a == b)
    })
}

/// Find all rows in `child_table` where FK column values == `ref_values`.
///
/// Strategy:
///   1. If a btree index covers the FK columns → scan_index → batch_get_rows
///      Complexity: O(matching rows), not O(table size).
///   2. Otherwise → scan_with_row_filter (bounded-batch scan, 1024 rows/batch,
///      per-batch client-side filter).
///      Memory: O(matching_rows + 1024), NOT O(table_size).
///
/// NEVER calls store.scan(txn, ..., None).
pub(super) async fn find_referencing_rows(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    child_table_name: &str,
    child_schema: &TableSchema,
    fk: &ForeignKeyConstraint,
    ref_values: &[Value],
) -> Result<Vec<Row>> {
    if let Some(index) = find_fk_covering_index(child_schema, &fk.columns) {
        // ── Index path ──
        let pk_types: Vec<DataType> = if child_schema.pk_indices.is_empty() {
            vec![DataType::Uuid]
        } else {
            child_schema
                .pk_indices
                .iter()
                .map(|&i| child_schema.columns[i].data_type.clone())
                .collect()
        };

        let matching_pks = if fk.columns.len() == index.columns.len() {
            store
                .scan_index(
                    txn,
                    db_id,
                    child_schema.table_id,
                    index.id,
                    ref_values,
                    index.unique,
                    &pk_types,
                    None,
                )
                .await?
        } else {
            let index_col_types: Vec<DataType> = index
                .columns
                .iter()
                .filter_map(|c| {
                    child_schema
                        .column_index(c)
                        .map(|i| child_schema.columns[i].data_type.clone())
                })
                .collect();
            store
                .scan_index_prefix(
                    txn,
                    db_id,
                    child_schema.table_id,
                    index.id,
                    ref_values,
                    index.unique,
                    &index_col_types,
                    &pk_types,
                    None,
                )
                .await?
        };

        if matching_pks.is_empty() {
            return Ok(Vec::new());
        }

        store
            .batch_get_rows(
                txn,
                db_id,
                child_schema.table_id,
                matching_pks,
                child_schema,
            )
            .await
    } else {
        // ── Non-indexed fallback: bounded-batch scan with filter ──
        let fk_clone = fk.clone();
        let schema_clone = child_schema.clone();
        let ref_values_owned: Vec<Value> = ref_values.to_vec();

        store
            .scan_with_row_filter(txn, db_id, child_table_name, |row| {
                fk_values_for_row(&fk_clone, &schema_clone, row)
                    .is_some_and(|fk_vals| fk_vals == ref_values_owned)
            })
            .await
    }
}

/// Point-lookup a single row by primary key.
pub(super) async fn fetch_row_by_pk(
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
    let cache = build_fk_ref_schema_cache(store, txn, db_id, schema, false).await?;
    validate_foreign_keys_inner(
        store,
        txn,
        db_id,
        schema,
        row,
        &HashMap::new(),
        false,
        &cache,
    )
    .await
}

/// Like [`validate_foreign_keys`] but accepts a pre-built ref-schema cache.
///
/// Use this variant when validating multiple rows in a loop so that
/// referenced table schemas are loaded once per statement instead of once
/// per row.
pub(crate) async fn validate_foreign_keys_with_cache(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    row: &Row,
    ref_schema_cache: &FkRefSchemaCache,
) -> Result<()> {
    validate_foreign_keys_inner(
        store,
        txn,
        db_id,
        schema,
        row,
        &HashMap::new(),
        false,
        ref_schema_cache,
    )
    .await
}

/// Validate only non-self-referencing foreign keys for a row.
/// Self-referencing FK validation is deferred to CopyDone for COPY.
///
/// Accepts a pre-built ref-schema cache built with `skip_self_ref=true`.
pub(crate) async fn validate_foreign_keys_non_self_ref(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    row: &Row,
    ref_schema_cache: &FkRefSchemaCache,
) -> Result<()> {
    validate_foreign_keys_inner(
        store,
        txn,
        db_id,
        schema,
        row,
        &HashMap::new(),
        true,
        ref_schema_cache,
    )
    .await
}

/// Collect unresolved self-referencing FK checks for deferred CopyDone validation.
/// Returns `(constraint_id, hash_key, display_values)`:
///   - `hash_key`: collision-resistant key for set membership (from [`pk_to_hash_key`])
///   - `display_values`: PG-like `(col)=(val)` string for error DETAIL
///
/// Any reference already resolvable in storage is not deferred.
pub(crate) async fn collect_deferred_self_fk_checks(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    row: &Row,
) -> Result<Vec<(ConstraintId, String, String)>> {
    let mut checks = Vec::new();
    for (constraint_id, fk) in schema.foreign_keys.iter().enumerate() {
        if fk.ref_table != schema.name {
            continue;
        }
        let mut fk_values = Vec::with_capacity(fk.columns.len());
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
        // MATCH SIMPLE: skip if any FK column is NULL.
        if any_null {
            continue;
        }
        // Skip self-identity reference (row references itself).
        let self_ref_vals = get_ref_values(fk, schema, row)?;
        if fk_values.len() == self_ref_vals.len()
            && fk_values
                .iter()
                .zip(&self_ref_vals)
                .all(|(a, b)| compare_values(a, b).is_ok_and(|c| c == 0))
        {
            continue;
        }

        let lookup = resolve_fk_ref_lookup(&fk.ref_columns, schema)?;
        let parent_exists = match lookup {
            FkRefLookup::Pk => {
                let ref_rows = store
                    .batch_get_rows(txn, db_id, schema.table_id, vec![fk_values.clone()], schema)
                    .await?;
                !ref_rows.is_empty()
            }
            FkRefLookup::UniqueIndex { index_id, pk_types } => {
                let pks = store
                    .scan_index(
                        txn,
                        db_id,
                        schema.table_id,
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
            let hash_key = pk_to_hash_key(&fk_values);
            let display = format_fk_detail_values(&fk.columns, &fk_values);
            checks.push((constraint_id, hash_key, display));
        }
    }
    Ok(checks)
}

/// Validate deferred self-referencing FK constraints at CopyDone.
/// Each deferred check stores `(constraint_id, hash_key, display_values)`:
///   - `hash_key` is checked against `pending_pk_keys` for set membership
///   - `display_values` is used in the user-facing DETAIL message
pub(crate) fn validate_deferred_self_fk_refs(
    schema: &TableSchema,
    deferred_refs: &[(ConstraintId, String, String)],
    pending_pk_keys: &HashMap<String, HashSet<String>>,
) -> Result<()> {
    for (constraint_id, hash_key, display_values) in deferred_refs {
        let fk = schema
            .foreign_keys
            .get(*constraint_id)
            .ok_or_else(|| anyhow!("FK constraint id '{}' not found in schema", constraint_id))?;

        // Parent was inserted during this COPY (possibly in a later batch).
        if pending_pk_keys
            .get(&fk.name)
            .is_some_and(|keys| keys.contains(hash_key))
        {
            continue;
        }

        let short_table = short_relation_name(&schema.name);
        let short_ref_table = short_relation_name(&fk.ref_table);
        return Err(SqlError::ForeignKeyViolation {
            constraint: fk.name.clone(),
            message: format!(
                "insert or update on table \"{}\" violates foreign key constraint \"{}\"\n\
                 DETAIL:  Key {} is not present in table \"{}\".",
                short_table, fk.name, display_values, short_ref_table
            ),
        }
        .into());
    }
    Ok(())
}

/// Collect hash keys for every self-referencing FK's referenced columns in
/// `row`.  The caller accumulates these into a `HashSet<String>` so that
/// later rows in the same COPY batch can resolve intra-batch references
/// without a storage round-trip.
pub(crate) fn self_ref_fk_keys(schema: &TableSchema, row: &Row) -> Result<Vec<(String, String)>> {
    let mut keys = Vec::new();
    for fk in &schema.foreign_keys {
        if fk.ref_table != schema.name {
            continue;
        }
        // For self-referencing FKs the parent schema IS the child schema.
        let ref_vals = get_ref_values(fk, schema, row)?;
        keys.push((fk.name.clone(), pk_to_hash_key(&ref_vals)));
    }
    Ok(keys)
}

async fn validate_foreign_keys_inner(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    row: &Row,
    pending_ref_keys: &HashMap<String, HashSet<String>>,
    skip_self_ref: bool,
    ref_schema_cache: &FkRefSchemaCache,
) -> Result<()> {
    for fk in &schema.foreign_keys {
        if skip_self_ref && fk.ref_table == schema.name {
            continue;
        }
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

        let ref_schema = ref_schema_cache.get(&fk.ref_table).ok_or_else(|| {
            anyhow!(
                "Referenced table '{}' not found for foreign key '{}'",
                fk.ref_table,
                fk.name
            )
        })?;

        let lookup = resolve_fk_ref_lookup(&fk.ref_columns, ref_schema)?;

        // Self-referencing FK: if the row's FK column values match its own
        // referenced column values, the constraint is trivially satisfied once
        // the row is written.  Skip the storage lookup.
        // (PostgreSQL validates at statement end where the row is visible.)
        // Use compare_values() for PG-like equality semantics (e.g. NaN = NaN).
        if fk.ref_table == schema.name {
            let self_ref_vals = get_ref_values(fk, ref_schema, row)?;
            if fk_values.len() == self_ref_vals.len()
                && fk_values
                    .iter()
                    .zip(&self_ref_vals)
                    .all(|(a, b)| compare_values(a, b).is_ok_and(|c| c == 0))
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
                        ref_schema,
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

        // For self-referencing FKs, also check the pending batch rows that
        // have been prepared but not yet flushed to storage.
        // Scoped per FK constraint name to prevent cross-FK contamination.
        let parent_exists = parent_exists
            || (fk.ref_table == schema.name
                && pending_ref_keys
                    .get(&fk.name)
                    .is_some_and(|keys| keys.contains(&pk_to_hash_key(&fk_values))));

        if !parent_exists {
            let detail_kv = format_fk_detail_values(&fk.columns, &fk_values);
            let short_table = short_relation_name(&schema.name);
            let short_ref_table = short_relation_name(&fk.ref_table);
            return Err(SqlError::ForeignKeyViolation {
                constraint: fk.name.clone(),
                message: format!(
                    "insert or update on table \"{}\" violates foreign key constraint \"{}\"\n\
                     DETAIL:  Key {} is not present in table \"{}\".",
                    short_table, fk.name, detail_kv, short_ref_table
                ),
            }
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDef, DataType, ForeignKeyAction, IndexDef};

    fn self_ref_schema() -> TableSchema {
        TableSchema {
            name: "public.items".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
                ColumnDef {
                    name: "parent_id".to_string(),
                    data_type: DataType::Int32,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
            ],
            pk_indices: vec![0],
            foreign_keys: vec![ForeignKeyConstraint {
                name: "items_parent_id_fkey".to_string(),
                columns: vec!["parent_id".to_string()],
                ref_table: "public.items".to_string(),
                ref_columns: vec!["id".to_string()],
                on_delete: ForeignKeyAction::NoAction,
                on_update: ForeignKeyAction::NoAction,
            }],
            ..TableSchema::default()
        }
    }

    #[test]
    fn self_ref_fk_keys_extracts_parent_ref_column_values() {
        let schema = self_ref_schema();
        let row = Row {
            values: vec![Value::Int32(1), Value::Null],
        };
        let keys = self_ref_fk_keys(&schema, &row).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].0, "items_parent_id_fkey");
        assert_eq!(keys[0].1, pk_to_hash_key(&[Value::Int32(1)]));
    }

    #[test]
    fn pending_ref_key_matches_child_fk_value_hash() {
        let schema = self_ref_schema();

        // Parent row: (id=1, parent_id=NULL)
        let parent_row = Row {
            values: vec![Value::Int32(1), Value::Null],
        };
        let keys = self_ref_fk_keys(&schema, &parent_row).unwrap();
        let mut pending: HashMap<String, HashSet<String>> = HashMap::new();
        for (fk_name, k) in keys {
            pending.entry(fk_name).or_default().insert(k);
        }

        // Child row: (id=2, parent_id=1) — FK value is 1
        let child_fk_values = vec![Value::Int32(1)];
        assert!(
            pending
                .get("items_parent_id_fkey")
                .is_some_and(|keys| keys.contains(&pk_to_hash_key(&child_fk_values))),
            "pending ref keys from parent row (id=1) must match child FK value (parent_id=1)"
        );
    }

    #[test]
    fn pending_ref_key_rejects_missing_parent() {
        let schema = self_ref_schema();

        let parent_row = Row {
            values: vec![Value::Int32(1), Value::Null],
        };
        let keys = self_ref_fk_keys(&schema, &parent_row).unwrap();
        let mut pending: HashMap<String, HashSet<String>> = HashMap::new();
        for (fk_name, k) in keys {
            pending.entry(fk_name).or_default().insert(k);
        }

        // FK value 999 is not in pending
        let child_fk_values = vec![Value::Int32(999)];
        assert!(
            !pending
                .get("items_parent_id_fkey")
                .is_some_and(|keys| keys.contains(&pk_to_hash_key(&child_fk_values))),
            "FK value 999 must not be satisfied by pending key for id=1"
        );
    }

    #[test]
    fn pk_to_hash_key_deterministic() {
        let a = pk_to_hash_key(&[Value::Int32(42)]);
        let b = pk_to_hash_key(&[Value::Int32(42)]);
        assert_eq!(a, b);
    }

    #[test]
    fn pk_to_hash_key_uses_debug_format() {
        let key = pk_to_hash_key(&[Value::Int32(1), Value::Int32(2)]);
        // Debug format includes variant tags, joined by NUL
        assert_eq!(key, "Int32(1)\0Int32(2)");
    }

    /// Regression test: composite text FK values that differ only by delimiter
    /// placement MUST NOT collide in the internal hash key.
    #[test]
    fn pk_to_hash_key_no_collision_on_embedded_delimiter() {
        let key_a = pk_to_hash_key(&[Value::Text("a".into()), Value::Text("b, c".into())]);
        let key_b = pk_to_hash_key(&[Value::Text("a, b".into()), Value::Text("c".into())]);
        assert_ne!(
            key_a, key_b,
            "composite text keys with embedded commas must not collide"
        );
    }

    #[test]
    fn format_fk_detail_values_produces_pg_format() {
        let cols = vec!["col1".to_string(), "col2".to_string()];
        let vals = vec![Value::Int32(1), Value::Text("hello".into())];
        let detail = format_fk_detail_values(&cols, &vals);
        assert_eq!(detail, "(col1, col2)=(1, hello)");
    }

    // ── Structural assertion: no unbounded scan in FK cascade path ──

    #[test]
    fn no_unbounded_scan_in_fk_cascade_path() {
        let cascade_delete_src = include_str!("cascade_delete.rs");
        let cascade_update_src = include_str!("cascade_update.rs");

        for (file, src) in [
            ("cascade_delete.rs", cascade_delete_src),
            ("cascade_update.rs", cascade_update_src),
        ] {
            assert!(
                !src.contains(".scan(txn,"),
                "{file} must not directly call store.scan() — \
                 use find_referencing_rows instead"
            );
        }
    }

    // ── find_fk_covering_index unit tests ──

    fn make_child_schema_with_indexes(indexes: Vec<IndexDef>) -> TableSchema {
        TableSchema {
            name: "public.child".to_string(),
            table_id: 2,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
                ColumnDef {
                    name: "parent_id".to_string(),
                    data_type: DataType::Int32,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
                ColumnDef {
                    name: "unrelated".to_string(),
                    data_type: DataType::Int32,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    collation: None,
                },
            ],
            pk_indices: vec![0],
            indexes,
            ..TableSchema::default()
        }
    }

    fn btree_index(
        name: &str,
        id: u64,
        columns: Vec<&str>,
        unique: bool,
        state: IndexState,
    ) -> IndexDef {
        IndexDef {
            name: name.to_string(),
            id,
            columns: columns.into_iter().map(String::from).collect(),
            unique,
            is_constraint: false,
            method: None, // NULL = btree default
            predicate: None,
            expressions: Vec::new(),
            state,
            cached_predicate_conjuncts: None,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        }
    }

    #[test]
    fn fk_covering_index_exact_match() {
        let schema = make_child_schema_with_indexes(vec![btree_index(
            "idx_parent_id",
            10,
            vec!["parent_id"],
            false,
            IndexState::Ready,
        )]);
        let fk_columns = vec!["parent_id".to_string()];
        let result = find_fk_covering_index(&schema, &fk_columns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "idx_parent_id");
    }

    #[test]
    fn fk_covering_index_prefix_match() {
        let schema = make_child_schema_with_indexes(vec![btree_index(
            "idx_composite",
            11,
            vec!["parent_id", "name"],
            false,
            IndexState::Ready,
        )]);
        let fk_columns = vec!["parent_id".to_string()];
        let result = find_fk_covering_index(&schema, &fk_columns);
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "idx_composite");
    }

    #[test]
    fn fk_covering_index_no_match() {
        let schema = make_child_schema_with_indexes(vec![btree_index(
            "idx_unrelated",
            12,
            vec!["unrelated"],
            false,
            IndexState::Ready,
        )]);
        let fk_columns = vec!["parent_id".to_string()];
        let result = find_fk_covering_index(&schema, &fk_columns);
        assert!(result.is_none());
    }

    #[test]
    fn fk_covering_index_skips_non_btree() {
        let schema = make_child_schema_with_indexes(vec![IndexDef {
            name: "idx_hnsw".to_string(),
            id: 13,
            columns: vec!["parent_id".to_string()],
            unique: false,
            is_constraint: false,
            method: Some("hnsw".to_string()),
            predicate: None,
            expressions: Vec::new(),
            state: IndexState::Ready,
            cached_predicate_conjuncts: None,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        }]);
        let fk_columns = vec!["parent_id".to_string()];
        assert!(find_fk_covering_index(&schema, &fk_columns).is_none());
    }

    #[test]
    fn fk_covering_index_skips_partial() {
        let schema = make_child_schema_with_indexes(vec![IndexDef {
            name: "idx_partial".to_string(),
            id: 14,
            columns: vec!["parent_id".to_string()],
            unique: false,
            is_constraint: false,
            method: None,
            predicate: Some("parent_id > 0".to_string()),
            expressions: Vec::new(),
            state: IndexState::Ready,
            cached_predicate_conjuncts: None,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        }]);
        let fk_columns = vec!["parent_id".to_string()];
        assert!(find_fk_covering_index(&schema, &fk_columns).is_none());
    }

    #[test]
    fn fk_covering_index_skips_expression() {
        let schema = make_child_schema_with_indexes(vec![IndexDef {
            name: "idx_expr".to_string(),
            id: 15,
            columns: vec!["parent_id".to_string()],
            unique: false,
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: vec!["lower(name)".to_string()],
            state: IndexState::Ready,
            cached_predicate_conjuncts: None,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        }]);
        let fk_columns = vec!["parent_id".to_string()];
        assert!(find_fk_covering_index(&schema, &fk_columns).is_none());
    }

    #[test]
    fn fk_covering_index_skips_not_ready() {
        let schema = make_child_schema_with_indexes(vec![btree_index(
            "idx_building",
            16,
            vec!["parent_id"],
            false,
            IndexState::Building,
        )]);
        let fk_columns = vec!["parent_id".to_string()];
        assert!(find_fk_covering_index(&schema, &fk_columns).is_none());
    }
}
