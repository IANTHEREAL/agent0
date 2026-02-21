//! ALTER TABLE and all sub-operations (ADD/DROP COLUMN, ADD/DROP CONSTRAINT,
//! RENAME COLUMN/TABLE/CONSTRAINT, ALTER COLUMN SET/DROP DEFAULT/NOT NULL/TYPE).

use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::ast::{
    AlterColumnOperation, AlterTableOperation, ColumnOption, GeneratedAs, ObjectName,
    TableConstraint,
};
use tikv_client::Transaction;

use crate::sql::error::SqlError;
use crate::sql::names;
use crate::sql::names::normalize_ident;
use crate::sql::projection::fill_row_defaults;
use crate::sql::query_context::QueryContext;
use crate::sql::sequences;
use crate::sql::ExecuteResult;
use crate::storage::TikvStore;
use crate::txn::txn_put;
use crate::types::{CheckConstraint, DataType, ForeignKeyConstraint, IndexDef, Value};
use crate::worker::types::IndexState;

use super::create_table::check_relation_name_available;
use super::{
    analyze_row_level_expr, assign_generated_check_constraint_names, check_expr_references_column,
    coerce_value_for_type_change, constraint_name_exists, delete_range, eval_row_level_expr,
    extract_first_column_from_check_expr, find_check_constraint_index, index_prefix_range,
    parse_referential_action, resolve_column_data_type, rewrite_check_expr_column, KvScanBatches,
    DDL_SCAN_BATCH_SIZE,
};
use crate::sql::value_coercion::coerce_value_for_column;

/// Execute an ALTER TABLE operation.
///
/// Returns `(result, invalidate_table_id)` -- `invalidate_table_id` is
/// `Some(table_id)` when the operation structurally changed the table in
/// a way that invalidates ANALYZE statistics (column added/dropped/renamed/
/// retyped), `None` otherwise.
pub async fn execute_alter_table(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    name: &ObjectName,
    operation: &AlterTableOperation,
) -> Result<(ExecuteResult, Option<u64>)> {
    let resolved =
        names::resolve_existing_table_name(store.as_ref(), txn, db_id, name, search_path)
            .await?
            .ok_or_else(|| anyhow!("Table '{}' does not exist", name))?;
    let table_object_name = resolved.name.clone();
    let t = resolved.full;
    let mut result_table_name = t.clone();
    let mut schema = store
        .get_schema(txn, db_id, &t)
        .await?
        .ok_or_else(|| anyhow!("Table '{}' does not exist", t))?;

    assign_generated_check_constraint_names(
        &table_object_name,
        !schema.pk_indices.is_empty(),
        &schema.indexes,
        &schema.foreign_keys,
        &mut schema.check_constraints,
    );

    let table_id = schema.table_id;
    // Set to true at mutation points that structurally change the table
    // (column added/dropped/renamed/retyped) to signal stats invalidation.
    let mut invalidate_stats = false;

    match operation {
        AlterTableOperation::AddColumn { column_def, .. } => {
            let col_name = normalize_ident(&column_def.name);
            if schema.column_index(&col_name).is_some() {
                return Err(anyhow!("Column exists"));
            }
            let (data_type, mut is_serial) =
                resolve_column_data_type(store, txn, db_id, search_path, &column_def.data_type)
                    .await?;
            let mut nullable = true;
            let mut default_expr = None;
            for opt in &column_def.options {
                match &opt.option {
                    ColumnOption::NotNull => nullable = false,
                    ColumnOption::Default(expr) => default_expr = Some(expr.to_string()),
                    ColumnOption::Generated {
                        generated_as,
                        generation_expr: None,
                        ..
                    } => {
                        if matches!(generated_as, GeneratedAs::Always | GeneratedAs::ByDefault) {
                            is_serial = true;
                        }
                    }
                    _ => {}
                }
            }
            if is_serial {
                nullable = false;
            }
            if !nullable && default_expr.is_none() {
                let (start, end) =
                    crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
                let range: tikv_client::BoundRange = (start..end).into();
                let existing_rows: Vec<_> = txn.scan(range, 1).await?.collect();
                if !existing_rows.is_empty() {
                    return Err(anyhow!("Cannot add NOT NULL column without DEFAULT"));
                }
            }
            schema.columns.push(crate::types::ColumnDef {
                name: col_name,
                data_type,
                nullable,
                primary_key: false,
                unique: false,
                is_serial,
                default_expr,
            });
            if is_serial {
                store
                    .create_sequence(
                        txn,
                        db_id,
                        sequences::build_implicit_sequence_def(
                            &schema.name,
                            schema
                                .columns
                                .last()
                                .expect("column just pushed")
                                .name
                                .as_str(),
                            &schema.columns.last().expect("column just pushed").data_type,
                        ),
                    )
                    .await?;
            }
            schema.version += 1;
            store.update_schema(txn, db_id, schema).await?;
            invalidate_stats = true;
        }
        AlterTableOperation::AddConstraint(constraint) => match constraint {
            TableConstraint::Unique {
                name,
                columns,
                is_primary,
                ..
            } if *is_primary => {
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
                let owning_schema = t.splitn(2, '.').next().unwrap_or("public");
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
                store.update_schema(txn, db_id, schema).await?;
            }
            TableConstraint::Unique { columns, name, .. } => {
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

                let index_name = name.as_ref().map(normalize_ident).unwrap_or_else(|| {
                    format!("{}_{}_key", table_object_name, col_names.join("_"))
                });

                // Schema-wide namespace uniqueness check.
                let owning_schema = t.splitn(2, '.').next().unwrap_or("public");
                check_relation_name_available(
                    store,
                    txn,
                    db_id,
                    owning_schema,
                    &index_name,
                    false,
                    None,
                )
                .await?;

                let new_index = crate::types::IndexDef {
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
                    method: None,
                    predicate: None,
                    expressions: Vec::new(),
                    state: IndexState::Ready,
                };

                let (start, end) =
                    crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
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
                        fill_row_defaults(&mut row, &schema)?;

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
                store.update_schema(txn, db_id, schema).await?;
            }
            TableConstraint::ForeignKey {
                name,
                columns,
                foreign_table,
                referred_columns,
                on_delete,
                on_update,
                ..
            } => {
                let fk_cols: Vec<String> = columns.iter().map(normalize_ident).collect();
                for col_name in &fk_cols {
                    if schema.column_index(col_name).is_none() {
                        return Err(anyhow!("Column '{}' does not exist", col_name));
                    }
                }

                let ref_table = names::resolve_existing_table_name(
                    store.as_ref(),
                    txn,
                    db_id,
                    foreign_table,
                    search_path,
                )
                .await?
                .ok_or_else(|| SqlError::RelationNotFound(foreign_table.to_string()))?
                .full;
                let ref_schema = store
                    .get_schema(txn, db_id, &ref_table)
                    .await?
                    .ok_or_else(|| SqlError::RelationNotFound(ref_table.clone()))?;
                let ref_cols: Vec<String> = referred_columns.iter().map(normalize_ident).collect();
                let fk_name = name
                    .as_ref()
                    .map(normalize_ident)
                    .unwrap_or_else(|| format!("{}_{}_fkey", table_object_name, fk_cols.join("_")));

                if constraint_name_exists(&schema, &table_object_name, &fk_name) {
                    return Err(anyhow!("Constraint '{}' already exists", fk_name));
                }

                if ref_schema.pk_indices.is_empty() {
                    return Err(anyhow!(
                        "Unsupported foreign key '{}': referenced table has no primary key",
                        fk_name
                    ));
                }
                if fk_cols.len() != ref_schema.pk_indices.len() {
                    return Err(anyhow!(
                        "Unsupported foreign key '{}': must reference primary key columns",
                        fk_name
                    ));
                }

                // PostgreSQL validates existing rows by default (unless NOT VALID).
                let (start, end) =
                    crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
                let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
                while let Some(batch) = scanner.next_batch(txn).await? {
                    for pair in batch {
                        let mut row = crate::storage::deserialize_row(pair.value())?;
                        fill_row_defaults(&mut row, &schema)?;

                        let mut fk_values: Vec<Value> = Vec::with_capacity(fk_cols.len());
                        let mut all_null = true;
                        for col_name in &fk_cols {
                            let idx = schema.column_index(col_name).expect("validated above");
                            let val = row.values[idx].clone();
                            if val != Value::Null {
                                all_null = false;
                            }
                            fk_values.push(val);
                        }

                        if all_null {
                            continue;
                        }

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
                            let cols = fk_cols.join(", ");
                            let vals: Vec<String> =
                                fk_values.iter().map(|v| format!("{}", v)).collect();
                            return Err(SqlError::ForeignKeyViolation {
                                constraint: fk_name.clone(),
                                message: format!(
                                    "insert or update on table \"{}\" violates foreign key constraint \"{}\"\n\
                                     DETAIL:  Key ({})=({}) is not present in table \"{}\".",
                                    schema.name,
                                    fk_name,
                                    cols,
                                    vals.join(", "),
                                    ref_table
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
                store.update_schema(txn, db_id, schema).await?;
            }
            TableConstraint::Check { name, expr } => {
                let expr_str = expr.to_string();
                let check_name = if let Some(n) = name.as_ref() {
                    normalize_ident(n)
                } else {
                    // PostgreSQL convention: {table}_{first_column}_check
                    let first_col = extract_first_column_from_check_expr(&expr_str);
                    let base = if let Some(col) = first_col {
                        format!("{}_{}_check", table_object_name, col)
                    } else {
                        format!("{}_check", table_object_name)
                    };
                    if !constraint_name_exists(&schema, &table_object_name, &base) {
                        base
                    } else {
                        let mut suffix = 1usize;
                        loop {
                            let candidate = format!("{}{}", base, suffix);
                            if !constraint_name_exists(&schema, &table_object_name, &candidate) {
                                break candidate;
                            }
                            suffix += 1;
                        }
                    }
                };

                if constraint_name_exists(&schema, &table_object_name, &check_name) {
                    return Err(anyhow!("Constraint '{}' already exists", check_name));
                }

                // PostgreSQL validates existing rows by default (unless NOT VALID).
                let typed_check_expr = analyze_row_level_expr(expr, &schema, db_id, search_path)?;
                let qctx = QueryContext::from_task_locals();
                let (start, end) =
                    crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
                let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
                while let Some(batch) = scanner.next_batch(txn).await? {
                    for pair in batch {
                        let mut row = crate::storage::deserialize_row(pair.value())?;
                        fill_row_defaults(&mut row, &schema)?;

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
                store.update_schema(txn, db_id, schema).await?;
            }
            _ => {
                return Err(anyhow!(
                    "Unsupported constraint in ALTER TABLE ... ADD CONSTRAINT (supported: PRIMARY KEY, UNIQUE, FOREIGN KEY, CHECK)"
                ));
            }
        },
        AlterTableOperation::DropConstraint {
            if_exists,
            name,
            cascade,
        } => {
            if *cascade {
                return Err(SqlError::Unsupported(
                    "DROP CONSTRAINT ... CASCADE is not supported".into(),
                )
                .into());
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
                    let (start, end) =
                        crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
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
                    let owning_schema = t.splitn(2, '.').next().unwrap_or("public");
                    let pk_full = format!("{}.{}", owning_schema, constraint_name);
                    store.release_relation_name(txn, db_id, &pk_full).await?;
                    schema.pk_constraint_name = None;
                    schema.version += 1;
                    store.update_schema(txn, db_id, schema).await?;
                    return Ok((
                        ExecuteResult::AlterTable {
                            table_name: result_table_name,
                        },
                        None,
                    ));
                }
            }

            if let Some(pos) = schema
                .foreign_keys
                .iter()
                .position(|fk| fk.name == constraint_name)
            {
                schema.foreign_keys.remove(pos);
                schema.version += 1;
                store.update_schema(txn, db_id, schema).await?;
                return Ok((
                    ExecuteResult::AlterTable {
                        table_name: result_table_name,
                    },
                    None,
                ));
            }

            if let Some(pos) =
                find_check_constraint_index(&schema, &table_object_name, &constraint_name)
            {
                schema.check_constraints.remove(pos);
                schema.version += 1;
                store.update_schema(txn, db_id, schema).await?;
                return Ok((
                    ExecuteResult::AlterTable {
                        table_name: result_table_name,
                    },
                    None,
                ));
            }

            if let Some(pos) = schema
                .indexes
                .iter()
                .position(|i| i.name == constraint_name)
            {
                let index = schema.indexes[pos].clone();
                if !index.unique {
                    if !if_exists {
                        return Err(anyhow!("Constraint '{}' does not exist", constraint_name));
                    }
                    return Ok((
                        ExecuteResult::AlterTable {
                            table_name: result_table_name,
                        },
                        None,
                    ));
                }

                let (start, end) = index_prefix_range(db_id, schema.table_id, index.id);
                delete_range(txn, start, end).await?;
                schema.indexes.remove(pos);

                // Release the constraint/index name reservation.
                let owning_schema = t.splitn(2, '.').next().unwrap_or("public");
                let idx_full = format!("{}.{}", owning_schema, constraint_name);
                store.release_relation_name(txn, db_id, &idx_full).await?;

                if index.columns.len() == 1 {
                    let col_name = &index.columns[0];
                    let still_unique = schema.indexes.iter().any(|idx| {
                        idx.unique && idx.columns.len() == 1 && idx.columns[0] == *col_name
                    });
                    if !still_unique {
                        if let Some(col_idx) = schema.column_index(col_name) {
                            schema.columns[col_idx].unique = false;
                        }
                    }
                }

                schema.version += 1;
                store.update_schema(txn, db_id, schema).await?;
                return Ok((
                    ExecuteResult::AlterTable {
                        table_name: result_table_name,
                    },
                    None,
                ));
            }

            if !if_exists {
                return Err(anyhow!("Constraint '{}' does not exist", constraint_name));
            }
        }
        AlterTableOperation::DropColumn {
            column_name,
            if_exists,
            cascade,
            ..
        } => {
            if *cascade {
                return Err(SqlError::Unsupported(
                    "DROP COLUMN ... CASCADE is not supported".into(),
                )
                .into());
            }

            let col_name = normalize_ident(column_name);
            let col_idx = schema.column_index(&col_name);
            let drop_changes_schema = should_invalidate_stats_for_drop_column(col_idx.is_some());
            match col_idx {
                Some(idx) => {
                    if schema.pk_indices.contains(&idx) {
                        return Err(anyhow!("Cannot drop primary key column '{}'", col_name));
                    }
                    for index in &schema.indexes {
                        if index.columns.contains(&col_name) {
                            return Err(anyhow!(
                                "Cannot drop column '{}' used in index '{}'",
                                col_name,
                                index.name
                            ));
                        }
                    }
                    for fk in &schema.foreign_keys {
                        if fk.columns.contains(&col_name) {
                            return Err(anyhow!(
                                "Cannot drop column '{}' used in foreign key '{}'",
                                col_name,
                                fk.name
                            ));
                        }
                    }
                    for check in &schema.check_constraints {
                        if check_expr_references_column(&check.expr, &col_name)? {
                            let name = check
                                .name
                                .as_deref()
                                .unwrap_or("<unnamed check constraint>");
                            return Err(anyhow!(
                                "Cannot drop column '{}' referenced by check constraint '{}'",
                                col_name,
                                name
                            ));
                        }
                    }

                    let (start, end) =
                        crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
                    let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
                    while let Some(batch) = scanner.next_batch(txn).await? {
                        for pair in batch {
                            let (key, value): (tikv_client::Key, tikv_client::Value) = pair.into();
                            let mut row = crate::storage::deserialize_row(&value)?;
                            fill_row_defaults(&mut row, &schema)?;
                            row.values.remove(idx);
                            let row_data = crate::storage::serialize_row(&row)?;
                            txn_put(txn, key.into(), row_data).await?;
                        }
                    }
                    schema.columns.remove(idx);
                    for pk_idx in &mut schema.pk_indices {
                        if *pk_idx > idx {
                            *pk_idx -= 1;
                        }
                    }
                    schema.version += 1;
                    store.update_schema(txn, db_id, schema).await?;
                    invalidate_stats = drop_changes_schema;
                }
                None => {
                    if !if_exists {
                        return Err(anyhow!("Column '{}' does not exist", col_name));
                    }
                }
            }
        }
        AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => {
            let old_name = normalize_ident(old_column_name);
            let new_name = normalize_ident(new_column_name);
            let new_quote_style = new_column_name.quote_style;
            let col_idx = schema
                .column_index(&old_name)
                .ok_or_else(|| anyhow!("Column '{}' does not exist", old_name))?;
            if schema.column_index(&new_name).is_some() {
                return Err(anyhow!("Column '{}' already exists", new_name));
            }
            schema.columns[col_idx].name = new_name.clone();
            for index in &mut schema.indexes {
                for col in &mut index.columns {
                    if *col == old_name {
                        *col = new_name.clone();
                    }
                }
            }
            for fk in &mut schema.foreign_keys {
                for col in &mut fk.columns {
                    if *col == old_name {
                        *col = new_name.clone();
                    }
                }
            }
            for check in &mut schema.check_constraints {
                if check_expr_references_column(&check.expr, &old_name)? {
                    check.expr = rewrite_check_expr_column(
                        &check.expr,
                        &old_name,
                        &new_name,
                        new_quote_style,
                    )?;
                }
            }
            schema.version += 1;
            store.update_schema(txn, db_id, schema).await?;
            store
                .rename_column_metadata(txn, db_id, &t, &old_name, &new_name)
                .await?;
            invalidate_stats = true;
        }
        AlterTableOperation::RenameTable { table_name } => {
            let new_table = table_name
                .0
                .last()
                .map(normalize_ident)
                .ok_or_else(|| anyhow!("Invalid table name"))?;

            let (schema_name, _) = names::parse_full_name(&t)?;
            let new_full = format!("{}.{}", schema_name, new_table);
            store.rename_table_schema(txn, db_id, &t, &new_full).await?;
            store
                .rename_table_metadata(txn, db_id, &t, &new_full)
                .await?;
            result_table_name = new_full.clone();

            // Update referencing-side metadata (FKs store ref_table as a string).
            let tables = store.list_tables(txn, db_id).await?;
            for table in tables {
                let mut s = match store.get_schema(txn, db_id, &table).await? {
                    Some(s) => s,
                    None => continue,
                };
                let mut changed = false;
                for fk in &mut s.foreign_keys {
                    if fk.ref_table == t {
                        fk.ref_table = new_full.clone();
                        changed = true;
                    }
                }
                if changed {
                    s.version += 1;
                    store.update_schema(txn, db_id, s).await?;
                }
            }
        }
        AlterTableOperation::RenameConstraint { old_name, new_name } => {
            let old = normalize_ident(old_name);
            let new = normalize_ident(new_name);

            if constraint_name_exists(&schema, &table_object_name, &new) {
                return Err(SqlError::DuplicateRelation(new.clone()).into());
            }

            if !schema.pk_indices.is_empty() {
                let default_pk_name;
                let pk_name = match schema.pk_constraint_name.as_deref() {
                    Some(n) => n,
                    None => {
                        default_pk_name = format!("{}_pkey", table_object_name);
                        &default_pk_name
                    }
                };
                if old == pk_name {
                    return Err(anyhow!("Cannot rename primary key constraint '{}'", old));
                }
            }

            if let Some(fk) = schema.foreign_keys.iter_mut().find(|fk| fk.name == old) {
                fk.name = new;
                schema.version += 1;
                store.update_schema(txn, db_id, schema).await?;
                return Ok((
                    ExecuteResult::AlterTable {
                        table_name: result_table_name,
                    },
                    None,
                ));
            }

            if let Some(pos) = find_check_constraint_index(&schema, &table_object_name, &old) {
                schema.check_constraints[pos].name = Some(new);
                schema.version += 1;
                store.update_schema(txn, db_id, schema).await?;
                return Ok((
                    ExecuteResult::AlterTable {
                        table_name: result_table_name,
                    },
                    None,
                ));
            }

            return Err(anyhow!(
                "constraint \"{}\" for table \"{}\" does not exist",
                old,
                table_object_name
            ));
        }
        AlterTableOperation::AlterColumn { column_name, op } => {
            let col_name = normalize_ident(column_name);
            let col_idx = schema
                .column_index(&col_name)
                .ok_or_else(|| anyhow!("Column '{}' does not exist", col_name))?;

            match op {
                AlterColumnOperation::SetDefault { value } => {
                    schema.columns[col_idx].default_expr = Some(value.to_string());
                    schema.version += 1;
                    store.update_schema(txn, db_id, schema).await?;
                }
                AlterColumnOperation::DropDefault => {
                    schema.columns[col_idx].default_expr = None;
                    schema.version += 1;
                    store.update_schema(txn, db_id, schema).await?;
                }
                AlterColumnOperation::SetNotNull => {
                    if !schema.columns[col_idx].nullable {
                        return Ok((
                            ExecuteResult::AlterTable {
                                table_name: result_table_name,
                            },
                            None,
                        ));
                    }

                    let (start, end) =
                        crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
                    let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
                    while let Some(batch) = scanner.next_batch(txn).await? {
                        for pair in batch {
                            let mut row = crate::storage::deserialize_row(pair.value())?;
                            fill_row_defaults(&mut row, &schema)?;
                            if matches!(row.values[col_idx], Value::Null) {
                                let short_table =
                                    schema.name.rsplit('.').next().unwrap_or(&schema.name);
                                return Err(anyhow!(
                                    "column \"{}\" of relation \"{}\" contains null values",
                                    col_name,
                                    short_table
                                ));
                            }
                        }
                    }

                    schema.columns[col_idx].nullable = false;
                    schema.version += 1;
                    store.update_schema(txn, db_id, schema).await?;
                }
                AlterColumnOperation::DropNotNull => {
                    schema.columns[col_idx].nullable = true;
                    schema.version += 1;
                    store.update_schema(txn, db_id, schema).await?;
                }
                AlterColumnOperation::SetDataType { data_type, using } => {
                    if schema.pk_indices.contains(&col_idx) {
                        return Err(anyhow!(
                            "Cannot alter type of primary key column '{}'",
                            col_name
                        ));
                    }
                    if schema
                        .foreign_keys
                        .iter()
                        .any(|fk| fk.columns.contains(&col_name))
                    {
                        return Err(anyhow!(
                            "Cannot alter type of column '{}' used in foreign key",
                            col_name
                        ));
                    }

                    let (new_type, _) =
                        resolve_column_data_type(store, txn, db_id, search_path, data_type).await?;
                    let type_changed = should_invalidate_stats_for_type_change(
                        &schema.columns[col_idx].data_type,
                        &new_type,
                    );
                    if !type_changed {
                        return Ok((
                            ExecuteResult::AlterTable {
                                table_name: result_table_name,
                            },
                            None,
                        ));
                    }

                    let affected_indexes: Vec<IndexDef> = schema
                        .indexes
                        .iter()
                        .filter(|idx| idx.columns.contains(&col_name))
                        .cloned()
                        .collect();

                    for idx in &affected_indexes {
                        let (start, end) = index_prefix_range(db_id, schema.table_id, idx.id);
                        delete_range(txn, start, end).await?;
                    }

                    let mut target_col = schema.columns[col_idx].clone();
                    target_col.data_type = new_type.clone();
                    let typed_using_expr = if let Some(using_expr) = &using {
                        Some(analyze_row_level_expr(
                            using_expr,
                            &schema,
                            db_id,
                            search_path,
                        )?)
                    } else {
                        None
                    };
                    let qctx = QueryContext::from_task_locals();

                    let (start, end) =
                        crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
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
                            let (key, value): (tikv_client::Key, tikv_client::Value) = pair.into();
                            let mut row = crate::storage::deserialize_row(&value)?;
                            fill_row_defaults(&mut row, &schema)?;

                            let new_val = if let Some(using_expr) = &typed_using_expr {
                                let result = eval_row_level_expr(using_expr, &row, &qctx)?;
                                coerce_value_for_column(result, &target_col)?
                            } else {
                                let old_val =
                                    std::mem::replace(&mut row.values[col_idx], Value::Null);
                                coerce_value_for_type_change(old_val, &target_col)?
                            };
                            row.values[col_idx] = new_val;

                            let pk_values = if schema.pk_indices.is_empty() {
                                let key_bytes: &[u8] = key.as_ref().into();
                                let pk_bytes = key_bytes
                                    .strip_prefix(data_key_prefix.as_slice())
                                    .ok_or_else(|| {
                                    anyhow!(
                                        "corrupted row key while rebuilding indexes for '{}'",
                                        schema.name
                                    )
                                })?;
                                crate::storage::decode_pk_from_index_suffix(pk_bytes, &pk_types)?
                            } else {
                                schema.get_pk_values(&row)
                            };
                            for idx in &affected_indexes {
                                let idx_values = schema.get_index_values(idx, &row);
                                store
                                    .create_index_entry(
                                        txn,
                                        db_id,
                                        schema.table_id,
                                        idx.id,
                                        &idx_values,
                                        &pk_values,
                                        idx.unique,
                                    )
                                    .await?;
                            }

                            let row_data = crate::storage::serialize_row(&row)?;
                            txn_put(txn, key.into(), row_data).await?;
                        }
                    }

                    schema.columns[col_idx].data_type = new_type;
                    schema.version += 1;
                    store.update_schema(txn, db_id, schema).await?;
                    invalidate_stats = type_changed;
                }
            }
        }
        _ => return Err(SqlError::Unsupported("Unsupported ALTER".into()).into()),
    }

    let invalidate_table = if invalidate_stats {
        Some(table_id)
    } else {
        None
    };
    Ok((
        ExecuteResult::AlterTable {
            table_name: result_table_name,
        },
        invalidate_table,
    ))
}

#[inline]
pub(super) fn should_invalidate_stats_for_drop_column(column_exists: bool) -> bool {
    column_exists
}

#[inline]
pub(super) fn should_invalidate_stats_for_type_change(
    old_type: &DataType,
    new_type: &DataType,
) -> bool {
    old_type != new_type
}
