//! Foreign key constraint validation and cascade operations (DELETE/UPDATE).

mod cascade_delete;
mod cascade_update;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::Result;
use tikv_client::Transaction;

use crate::sql::error::SqlError;
use crate::sql::expr::compare_values;
use crate::storage::TikvStore;
use crate::types::{DataType, ForeignKeyConstraint, Row, TableSchema, Value};
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

pub(super) fn short_relation_name(name: &str) -> &str {
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

    pub(super) fn remove_row_from_snapshot(
        &mut self,
        table_name: &str,
        schema: &TableSchema,
        row: &Row,
    ) {
        let pk = schema.get_pk_values(row);
        let pk_key = pk_to_hash_key(&pk);
        if let Some(rows) = self.table_rows.get_mut(table_name) {
            rows.retain(|r| pk_to_hash_key(&schema.get_pk_values(r)) != pk_key);
        }
    }

    pub(super) fn replace_row_in_snapshot(
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

    pub(super) fn find_row_in_snapshot(
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
