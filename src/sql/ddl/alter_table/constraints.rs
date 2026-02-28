//! Constraint-level ALTER TABLE operations: ADD/DROP PRIMARY KEY, UNIQUE,
//! FOREIGN KEY, CHECK constraints.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::ast::ObjectName;
use tikv_client::Transaction;

use crate::model::{CheckConstraint, DataType, ForeignKeyConstraint, IndexDef, Value};
use crate::sql::dml::{resolve_fk_ref_lookup, FkRefLookup};
use crate::sql::error::SqlError;
use crate::sql::names;
use crate::sql::names::normalize_ident;
use crate::sql::projection::fill_row_defaults;
use crate::sql::query_context::QueryContext;
use crate::sql::ExecuteResult;
use crate::storage::TikvStore;
use crate::worker::types::IndexState;

use super::super::create_table::check_relation_name_available;
use super::super::{
    analyze_row_level_expr, constraint_name_exists, delete_range, eval_row_level_expr,
    extract_first_column_from_check_expr, find_check_constraint_index, index_prefix_range,
    parse_referential_action, KvScanBatches, DDL_SCAN_BATCH_SIZE,
};

/// ADD CONSTRAINT ... PRIMARY KEY: validate columns, set pk_indices, reserve
/// constraint name.
pub(super) async fn alter_table_add_primary_key(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &mut crate::model::TableSchema,
    table_object_name: &str,
    full_table_name: &str,
    name: &Option<sqlparser::ast::Ident>,
    columns: &[sqlparser::ast::Ident],
) -> Result<()> {
    let pk_names: Vec<String> = columns.iter().map(normalize_ident).collect();
    let mut pk_indices = Vec::new();
    for pk_name in &pk_names {
        if let Some(idx) = schema.columns.iter().position(|c| c.name == *pk_name) {
            schema.columns[idx].primary_key = true;
            schema.columns[idx].nullable = false;
            pk_indices.push(idx);
        } else {
            return Err(anyhow!("Column '{}' does not exist", pk_name));
        }
    }
    schema.pk_indices = pk_indices;
    let pk_constraint = name
        .as_ref()
        .map(normalize_ident)
        .unwrap_or_else(|| format!("{}_pkey", table_object_name));
    // Reserve the PK constraint name in the schema-wide namespace.
    let owning_schema = full_table_name.split('.').next().unwrap_or("public");
    check_relation_name_available(
        store,
        txn,
        db_id,
        owning_schema,
        &pk_constraint,
        false,
        None,
    )
    .await?;
    schema.pk_constraint_name = Some(pk_constraint);
    schema.version += 1;
    store.update_schema(txn, db_id, schema.clone()).await?;
    Ok(())
}

/// ADD CONSTRAINT ... UNIQUE: validate columns, create unique index, backfill
/// index entries for existing rows.
pub(super) async fn alter_table_add_unique_constraint(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &mut crate::model::TableSchema,
    table_object_name: &str,
    full_table_name: &str,
    name: &Option<sqlparser::ast::Ident>,
    columns: &[sqlparser::ast::Ident],
) -> Result<()> {
    let col_names: Vec<String> = columns.iter().map(normalize_ident).collect();

    for col_name in &col_names {
        if schema.column_index(col_name).is_none() {
            return Err(anyhow!("Column '{}' does not exist", col_name));
        }
    }

    if col_names.len() == 1 {
        let idx = schema.column_index(&col_names[0]).expect("validated above");
        schema.columns[idx].unique = true;
    }

    let index_name = name
        .as_ref()
        .map(normalize_ident)
        .unwrap_or_else(|| format!("{}_{}_key", table_object_name, col_names.join("_")));

    // Schema-wide namespace uniqueness check.
    let owning_schema = full_table_name.split('.').next().unwrap_or("public");
    check_relation_name_available(store, txn, db_id, owning_schema, &index_name, false, None)
        .await?;

    let new_index = crate::model::IndexDef {
        id: schema
            .indexes
            .iter()
            .map(|i| i.id)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| anyhow!("Index id overflow"))?,
        name: index_name,
        columns: col_names,
        unique: true,
        is_constraint: true,
        method: None,
        predicate: None,
        expressions: Vec::new(),
        state: IndexState::Ready,
        cached_predicate_conjuncts: None,
    };

    let (start, end) = crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
    let data_key_prefix = start.clone();
    let pk_types: Vec<DataType> = if schema.pk_indices.is_empty() {
        vec![DataType::Uuid]
    } else {
        schema
            .pk_indices
            .iter()
            .map(|&idx| schema.columns[idx].data_type.clone())
            .collect()
    };
    let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
    while let Some(batch) = scanner.next_batch(txn).await? {
        for pair in batch {
            let mut row = crate::storage::deserialize_row(pair.value())?;
            fill_row_defaults(&mut row, schema)?;

            let idx_values = schema.get_index_values(&new_index, &row);
            let pk_values = if schema.pk_indices.is_empty() {
                let key: &[u8] = pair.key().as_ref().into();
                let pk_bytes = key
                    .strip_prefix(data_key_prefix.as_slice())
                    .ok_or_else(|| {
                        anyhow!(
                            "corrupted row key while backfilling constraint '{}'",
                            new_index.name
                        )
                    })?;
                crate::storage::decode_pk_from_index_suffix(pk_bytes, &pk_types)?
            } else {
                schema.get_pk_values(&row)
            };
            store
                .create_index_entry(
                    txn,
                    db_id,
                    schema.table_id,
                    new_index.id,
                    &idx_values,
                    &pk_values,
                    true,
                )
                .await?;
        }
    }

    schema.indexes.push(new_index);
    schema.version += 1;
    store.update_schema(txn, db_id, schema.clone()).await?;
    Ok(())
}

/// ADD CONSTRAINT ... FOREIGN KEY: resolve referenced table, validate columns,
/// check existing rows, push FK.
pub(super) async fn alter_table_add_foreign_key(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    schema: &mut crate::model::TableSchema,
    table_object_name: &str,
    constraint_name: &Option<sqlparser::ast::Ident>,
    columns: &[sqlparser::ast::Ident],
    foreign_table: &ObjectName,
    referred_columns: &[sqlparser::ast::Ident],
    on_delete: &Option<sqlparser::ast::ReferentialAction>,
    on_update: &Option<sqlparser::ast::ReferentialAction>,
) -> Result<()> {
    let fk_cols: Vec<String> = columns.iter().map(normalize_ident).collect();
    for col_name in &fk_cols {
        if schema.column_index(col_name).is_none() {
            return Err(anyhow!("Column '{}' does not exist", col_name));
        }
    }

    let ref_table =
        names::resolve_existing_table_name(store.as_ref(), txn, db_id, foreign_table, search_path)
            .await?
            .ok_or_else(|| SqlError::RelationNotFound(foreign_table.to_string()))?
            .full;
    let ref_schema = store
        .get_schema(txn, db_id, &ref_table)
        .await?
        .ok_or_else(|| SqlError::RelationNotFound(ref_table.clone()))?;
    let ref_cols: Vec<String> = referred_columns.iter().map(normalize_ident).collect();
    let fk_name = constraint_name
        .as_ref()
        .map(normalize_ident)
        .unwrap_or_else(|| format!("{}_{}_fkey", table_object_name, fk_cols.join("_")));

    if constraint_name_exists(schema, table_object_name, &fk_name) {
        return Err(anyhow!("Constraint '{}' already exists", fk_name));
    }

    let short_ref = ref_table.rsplit('.').next().unwrap_or(&ref_table);
    if ref_cols.is_empty() {
        if ref_schema.pk_indices.is_empty() {
            return Err(anyhow!(
                "there is no primary key for referenced table \"{}\"",
                short_ref
            ));
        }
        if fk_cols.len() != ref_schema.pk_indices.len() {
            return Err(anyhow!(
                "number of referencing and referenced columns for foreign key disagree"
            ));
        }
    } else if fk_cols.len() != ref_cols.len() {
        return Err(anyhow!(
            "number of referencing and referenced columns for foreign key disagree"
        ));
    }

    let fk_lookup = resolve_fk_ref_lookup(&ref_cols, &ref_schema)?;

    // PostgreSQL validates existing rows by default (unless NOT VALID).
    let (start, end) = crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
    let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
    while let Some(batch) = scanner.next_batch(txn).await? {
        for pair in batch {
            let mut row = crate::storage::deserialize_row(pair.value())?;
            fill_row_defaults(&mut row, schema)?;

            let mut fk_values: Vec<Value> = Vec::with_capacity(fk_cols.len());
            let mut any_null = false;
            for col_name in &fk_cols {
                let idx = schema.column_index(col_name).expect("validated above");
                let val = row.values[idx].clone();
                if val == Value::Null {
                    any_null = true;
                }
                fk_values.push(val);
            }

            // MATCH SIMPLE: skip FK check if any referencing column is NULL.
            if any_null {
                continue;
            }

            let parent_exists = match &fk_lookup {
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
                            *index_id,
                            &fk_values,
                            true,
                            pk_types,
                            Some(1),
                        )
                        .await?;
                    !pks.is_empty()
                }
            };
            if !parent_exists {
                let cols = fk_cols.join(", ");
                let vals: Vec<String> = fk_values.iter().map(|v| format!("{}", v)).collect();
                let short_table = schema.name.rsplit('.').next().unwrap_or(&schema.name);
                let short_ref = ref_table.rsplit('.').next().unwrap_or(&ref_table);
                return Err(SqlError::ForeignKeyViolation {
                    constraint: fk_name.clone(),
                    message: format!(
                        "insert or update on table \"{}\" violates foreign key constraint \"{}\"\n\
                         DETAIL:  Key ({})=({}) is not present in table \"{}\".",
                        short_table,
                        fk_name,
                        cols,
                        vals.join(", "),
                        short_ref
                    ),
                }
                .into());
            }
        }
    }

    schema.foreign_keys.push(ForeignKeyConstraint {
        name: fk_name,
        columns: fk_cols,
        ref_table,
        ref_columns: ref_cols,
        on_delete: parse_referential_action(on_delete),
        on_update: parse_referential_action(on_update),
    });
    schema.version += 1;
    store.update_schema(txn, db_id, schema.clone()).await?;
    Ok(())
}

/// ADD CONSTRAINT ... CHECK: generate name, validate existing rows, push
/// check constraint.
pub(super) async fn alter_table_add_check_constraint(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    schema: &mut crate::model::TableSchema,
    table_object_name: &str,
    collations: &[crate::sql::collation::CollationDef],
    constraint_name: &Option<sqlparser::ast::Ident>,
    expr: &sqlparser::ast::Expr,
) -> Result<()> {
    let expr_str = expr.to_string();
    let check_name = if let Some(n) = constraint_name.as_ref() {
        normalize_ident(n)
    } else {
        // PostgreSQL convention: {table}_{first_column}_check
        let first_col = extract_first_column_from_check_expr(&expr_str);
        let base = if let Some(col) = first_col {
            format!("{}_{}_check", table_object_name, col)
        } else {
            format!("{}_check", table_object_name)
        };
        if !constraint_name_exists(schema, table_object_name, &base) {
            base
        } else {
            let mut suffix = 1usize;
            loop {
                let candidate = format!("{}{}", base, suffix);
                if !constraint_name_exists(schema, table_object_name, &candidate) {
                    break candidate;
                }
                suffix += 1;
            }
        }
    };

    if constraint_name_exists(schema, table_object_name, &check_name) {
        return Err(anyhow!("Constraint '{}' already exists", check_name));
    }

    // PostgreSQL validates existing rows by default (unless NOT VALID).
    let typed_check_expr = analyze_row_level_expr(expr, schema, db_id, search_path, collations)?;
    let qctx = QueryContext::from_task_locals();
    let (start, end) = crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
    let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
    while let Some(batch) = scanner.next_batch(txn).await? {
        for pair in batch {
            let mut row = crate::storage::deserialize_row(pair.value())?;
            fill_row_defaults(&mut row, schema)?;

            let result = eval_row_level_expr(&typed_check_expr, &row, &qctx)?;
            match result {
                Value::Boolean(true) | Value::Null => {}
                Value::Boolean(false) => {
                    return Err(anyhow!(
                        "check constraint \"{}\" is violated by some row",
                        check_name
                    ));
                }
                other => {
                    return Err(anyhow!(
                        "CHECK constraint must evaluate to boolean, got {:?}",
                        other
                    ));
                }
            }
        }
    }

    schema.check_constraints.push(CheckConstraint {
        name: Some(check_name),
        expr: expr_str,
    });
    schema.version += 1;
    store.update_schema(txn, db_id, schema.clone()).await?;
    Ok(())
}

fn find_unique_constraint_index(
    schema: &crate::model::TableSchema,
    constraint_name: &str,
) -> Option<usize> {
    schema
        .indexes
        .iter()
        .position(|idx| idx.name == constraint_name && idx.unique && idx.is_constraint)
}

fn has_single_column_unique_constraint(indexes: &[IndexDef], col_name: &str) -> bool {
    indexes.iter().any(|idx| {
        idx.unique && idx.is_constraint && idx.columns.len() == 1 && idx.columns[0] == col_name
    })
}

/// DROP CONSTRAINT: try PK, FK, CHECK, unique-constraint backing index in
/// order. Returns
/// `Some(result)` when the constraint was found and handled (caller should
/// early-return), `None` when `if_exists` is true and no match was found.
pub(super) async fn alter_table_drop_constraint(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &mut crate::model::TableSchema,
    full_table_name: &str,
    table_object_name: &str,
    result_table_name: &str,
    if_exists: bool,
    name: &sqlparser::ast::Ident,
    cascade: bool,
) -> Result<Option<ExecuteResult>> {
    if cascade {
        return Err(
            SqlError::Unsupported("DROP CONSTRAINT ... CASCADE is not supported".into()).into(),
        );
    }

    let constraint_name = normalize_ident(name);
    if !schema.pk_indices.is_empty() {
        let default_pk_name;
        let pk_name = match schema.pk_constraint_name.as_deref() {
            Some(n) => n,
            None => {
                default_pk_name = format!("{}_pkey", table_object_name);
                &default_pk_name
            }
        };
        if constraint_name == pk_name {
            let (start, end) = crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
            let range: tikv_client::BoundRange = (start..end).into();
            let existing_rows: Vec<_> = txn.scan(range, 1).await?.collect();
            if !existing_rows.is_empty() {
                return Err(anyhow!(
                    "Cannot drop primary key constraint '{}' because it contains data",
                    constraint_name
                ));
            }

            for &idx in &schema.pk_indices {
                if let Some(col) = schema.columns.get_mut(idx) {
                    col.primary_key = false;
                }
            }
            schema.pk_indices.clear();
            // Release the PK constraint name reservation.
            let owning_schema = full_table_name.split('.').next().unwrap_or("public");
            let pk_full = format!("{}.{}", owning_schema, constraint_name);
            store.release_relation_name(txn, db_id, &pk_full).await?;
            schema.pk_constraint_name = None;
            schema.version += 1;
            store.update_schema(txn, db_id, schema.clone()).await?;
            return Ok(Some(ExecuteResult::AlterTable {
                table_name: result_table_name.to_string(),
            }));
        }
    }

    if let Some(pos) = schema
        .foreign_keys
        .iter()
        .position(|fk| fk.name == constraint_name)
    {
        schema.foreign_keys.remove(pos);
        schema.version += 1;
        store.update_schema(txn, db_id, schema.clone()).await?;
        return Ok(Some(ExecuteResult::AlterTable {
            table_name: result_table_name.to_string(),
        }));
    }

    if let Some(pos) = find_check_constraint_index(schema, table_object_name, &constraint_name) {
        schema.check_constraints.remove(pos);
        schema.version += 1;
        store.update_schema(txn, db_id, schema.clone()).await?;
        return Ok(Some(ExecuteResult::AlterTable {
            table_name: result_table_name.to_string(),
        }));
    }

    if let Some(pos) = find_unique_constraint_index(schema, &constraint_name) {
        let index = schema.indexes[pos].clone();

        let (start, end) = index_prefix_range(db_id, schema.table_id, index.id);
        delete_range(txn, start, end).await?;
        schema.indexes.remove(pos);

        // Release the constraint/index name reservation.
        let owning_schema = full_table_name.split('.').next().unwrap_or("public");
        let idx_full = format!("{}.{}", owning_schema, constraint_name);
        store.release_relation_name(txn, db_id, &idx_full).await?;

        if index.columns.len() == 1 {
            let col_name = &index.columns[0];
            let still_unique = has_single_column_unique_constraint(&schema.indexes, col_name);
            if !still_unique {
                if let Some(col_idx) = schema.column_index(col_name) {
                    schema.columns[col_idx].unique = false;
                }
            }
        }

        schema.version += 1;
        store.update_schema(txn, db_id, schema.clone()).await?;
        return Ok(Some(ExecuteResult::AlterTable {
            table_name: result_table_name.to_string(),
        }));
    }

    if !if_exists {
        return Err(anyhow!("Constraint '{}' does not exist", constraint_name));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::{find_unique_constraint_index, has_single_column_unique_constraint};
    use crate::model::IndexDef;
    use crate::worker::types::IndexState;

    fn index(name: &str, columns: &[&str], unique: bool, is_constraint: bool) -> IndexDef {
        IndexDef {
            name: name.to_string(),
            id: 1,
            columns: columns.iter().map(|c| c.to_string()).collect(),
            unique,
            is_constraint,
            method: None,
            predicate: None,
            expressions: vec![],
            state: IndexState::Ready,
            cached_predicate_conjuncts: None,
        }
    }

    #[test]
    fn drop_constraint_lookup_ignores_plain_unique_index() {
        let mut schema = crate::model::TableSchema::new("public.t".to_string(), 1, vec![], vec![]);
        schema.indexes.push(index("t_a_key", &["a"], true, false));

        assert_eq!(find_unique_constraint_index(&schema, "t_a_key"), None);
    }

    #[test]
    fn drop_constraint_lookup_matches_constraint_backing_index() {
        let mut schema = crate::model::TableSchema::new("public.t".to_string(), 1, vec![], vec![]);
        schema.indexes.push(index("t_a_key", &["a"], true, true));

        assert_eq!(find_unique_constraint_index(&schema, "t_a_key"), Some(0));
    }

    #[test]
    fn single_column_unique_constraint_check_ignores_plain_unique_indexes() {
        let indexes = vec![index("t_a_uix", &["a"], true, false)];
        assert!(!has_single_column_unique_constraint(&indexes, "a"));
    }
}
