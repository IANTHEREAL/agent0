//! CREATE TABLE, CREATE TABLE AS (CTAS), SELECT INTO, and relation-name
//! availability checking.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::ast::{
    ColumnDef as SqlColumnDef, ColumnOption, GeneratedAs, ObjectName, TableConstraint,
};
use tikv_client::Transaction;

use crate::sql::dml::resolve_fk_ref_lookup;
use crate::sql::error::SqlError;
use crate::sql::names;
use crate::sql::names::normalize_ident;
use crate::sql::types::sql_datatype_to_internal_strict;
use crate::sql::value_coercion::infer_data_type;
use crate::sql::ExecuteResult;
use crate::storage::TikvStore;
use crate::types::{
    CheckConstraint, ColumnDef, DataType, ForeignKeyConstraint, IndexDef, Row, TableSchema, Value,
};
use crate::worker::types::IndexState;

use super::{
    advance_implicit_sequences_for_seeded_rows, assign_generated_check_constraint_names,
    create_implicit_sequences_for_schema, parse_referential_action, resolve_column_data_type,
    warn_legacy_relname_conflict_scan_once,
};

fn short_relation_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

pub async fn execute_create_table(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    name: &ObjectName,
    columns: &[SqlColumnDef],
    constraints: &[TableConstraint],
    if_not_exists: bool,
) -> Result<ExecuteResult> {
    let names::ResolvedName {
        schema: table_schema_name,
        name: table_object_name,
        full: table_full_name,
    } = names::resolve_ddl_object_name(name, search_path)?;
    if !store.schema_exists(txn, db_id, &table_schema_name).await? {
        return Err(anyhow!("schema '{}' does not exist", table_schema_name));
    }

    if if_not_exists && store.table_exists(txn, db_id, &table_full_name).await? {
        return Ok(ExecuteResult::CreateTable {
            table_name: table_full_name,
        });
    }

    let mut pk_constraint_name: Option<String> = None;
    let pk_columns: Vec<String> = constraints
        .iter()
        .filter_map(|c| match c {
            TableConstraint::Unique {
                name,
                columns,
                is_primary,
                ..
            } if *is_primary => {
                if pk_constraint_name.is_none() {
                    pk_constraint_name = name.as_ref().map(normalize_ident);
                }
                Some(columns.iter().map(normalize_ident).collect::<Vec<_>>())
            }
            _ => None,
        })
        .flatten()
        .collect();

    let mut check_constraints: Vec<CheckConstraint> = constraints
        .iter()
        .filter_map(|c| match c {
            TableConstraint::Check { name, expr } => Some(CheckConstraint {
                name: name.as_ref().map(normalize_ident),
                expr: expr.to_string(),
            }),
            _ => None,
        })
        .collect();

    let mut foreign_keys: Vec<ForeignKeyConstraint> = Vec::new();
    let mut col_defs = Vec::new();
    for col in columns {
        let col_name = normalize_ident(&col.name);
        let (data_type, mut is_serial) =
            resolve_column_data_type(store, txn, db_id, search_path, &col.data_type).await?;

        let mut is_pk = pk_columns.contains(&col_name);
        let mut nullable = true;
        let mut unique = false;
        let mut default_expr = None;
        let collation: Option<String> = col.collation.as_ref().map(|c| c.to_string());

        for opt in &col.options {
            match &opt.option {
                ColumnOption::Unique { is_primary, .. } => {
                    if *is_primary {
                        is_pk = true;
                        if pk_constraint_name.is_none() {
                            pk_constraint_name = opt.name.as_ref().map(normalize_ident);
                        }
                    } else {
                        unique = true;
                    }
                }
                ColumnOption::NotNull => nullable = false,
                ColumnOption::Default(expr) => default_expr = Some(expr.to_string()),
                ColumnOption::Check(expr) => {
                    check_constraints.push(CheckConstraint {
                        name: opt.name.as_ref().map(normalize_ident),
                        expr: expr.to_string(),
                    });
                }
                ColumnOption::Generated {
                    generated_as,
                    generation_expr: None,
                    ..
                } => {
                    if matches!(generated_as, GeneratedAs::Always | GeneratedAs::ByDefault) {
                        is_serial = true;
                    }
                }
                ColumnOption::ForeignKey {
                    foreign_table,
                    referred_columns,
                    on_delete,
                    on_update,
                    ..
                } => {
                    // Handle inline REFERENCES clause
                    let (ref_schema_opt, ref_table_name) = names::split_object_name(foreign_table)?;
                    let ref_table = if ref_table_name == table_object_name
                        && ref_schema_opt
                            .as_deref()
                            .map(|s| s == table_schema_name)
                            .unwrap_or(true)
                    {
                        table_full_name.clone()
                    } else {
                        names::resolve_existing_table_name(
                            store.as_ref(),
                            txn,
                            db_id,
                            foreign_table,
                            search_path,
                        )
                        .await?
                        .ok_or_else(|| SqlError::RelationNotFound(foreign_table.to_string()))?
                        .full
                    };
                    let ref_cols: Vec<String> =
                        referred_columns.iter().map(normalize_ident).collect();
                    let fk_name = format!("{}_{}_fkey", table_object_name, col_name);

                    if ref_table != table_full_name {
                        let ref_table_schema = store
                            .get_schema(txn, db_id, &ref_table)
                            .await?
                            .ok_or_else(|| SqlError::RelationNotFound(ref_table.clone()))?;
                        if ref_cols.is_empty() {
                            if ref_table_schema.pk_indices.is_empty() {
                                return Err(anyhow!(
                                    "there is no primary key for referenced table \"{}\"",
                                    short_relation_name(&ref_table)
                                ));
                            }
                            if ref_table_schema.pk_indices.len() != 1 {
                                return Err(anyhow!(
                                    "number of referencing and referenced columns for foreign key disagree"
                                ));
                            }
                        } else if ref_cols.len() != 1 {
                            return Err(anyhow!(
                                "number of referencing and referenced columns for foreign key disagree"
                            ));
                        }
                        resolve_fk_ref_lookup(&ref_cols, &ref_table_schema)?;
                    }

                    foreign_keys.push(ForeignKeyConstraint {
                        name: fk_name,
                        columns: vec![col_name.clone()],
                        ref_table,
                        ref_columns: ref_cols,
                        on_delete: parse_referential_action(on_delete),
                        on_update: parse_referential_action(on_update),
                    });
                }
                _ => {}
            }
        }

        if is_serial {
            nullable = false;
        }
        if is_pk {
            nullable = false;
        }

        col_defs.push(ColumnDef {
            name: col_name,
            data_type,
            nullable,
            primary_key: is_pk,
            unique,
            is_serial,
            default_expr,
            collation,
        });
    }

    let mut pk_indices = Vec::new();
    if !pk_columns.is_empty() {
        for pk_name in &pk_columns {
            if let Some(idx) = col_defs.iter().position(|c| c.name == *pk_name) {
                pk_indices.push(idx);
            }
        }
    } else {
        for (i, col) in col_defs.iter().enumerate() {
            if col.primary_key {
                pk_indices.push(i);
            }
        }
    }

    let pk_constraint_name = if pk_indices.is_empty() {
        None
    } else {
        Some(pk_constraint_name.unwrap_or_else(|| format!("{}_pkey", table_object_name)))
    };

    let table_id = store.next_table_id(txn, db_id).await?;

    let mut indexes = Vec::new();
    let mut next_index_id = 1u64;

    for col in col_defs.iter() {
        if col.unique && !col.primary_key {
            indexes.push(IndexDef {
                name: format!("{}_{}_key", table_object_name, col.name),
                id: next_index_id,
                columns: vec![col.name.clone()],
                unique: true,
                method: None,
                predicate: None,
                expressions: Vec::new(),
                state: IndexState::Ready,
            });
            next_index_id += 1;
        }
    }

    // Process table-level constraints for foreign keys
    for constraint in constraints {
        match constraint {
            TableConstraint::Unique {
                name,
                columns,
                is_primary,
                ..
            } => {
                if !*is_primary {
                    let col_names: Vec<String> = columns.iter().map(normalize_ident).collect();
                    let idx_name = name.as_ref().map(|n| n.value.clone()).unwrap_or_else(|| {
                        format!("{}_{}_key", table_object_name, col_names.join("_"))
                    });
                    indexes.push(IndexDef {
                        name: idx_name,
                        id: next_index_id,
                        columns: col_names,
                        unique: true,
                        method: None,
                        predicate: None,
                        expressions: Vec::new(),
                        state: IndexState::Ready,
                    });
                    next_index_id += 1;
                }
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
                let (ref_schema_opt, ref_table_name) = names::split_object_name(foreign_table)?;
                let ref_table = if ref_table_name == table_object_name
                    && ref_schema_opt
                        .as_deref()
                        .map(|s| s == table_schema_name)
                        .unwrap_or(true)
                {
                    table_full_name.clone()
                } else {
                    names::resolve_existing_table_name(
                        store.as_ref(),
                        txn,
                        db_id,
                        foreign_table,
                        search_path,
                    )
                    .await?
                    .ok_or_else(|| SqlError::RelationNotFound(foreign_table.to_string()))?
                    .full
                };
                let ref_cols: Vec<String> = referred_columns.iter().map(normalize_ident).collect();
                let fk_name = name
                    .as_ref()
                    .map(|n| n.value.clone())
                    .unwrap_or_else(|| format!("{}_{}_fkey", table_object_name, fk_cols.join("_")));

                if ref_table != table_full_name {
                    let ref_table_schema = store
                        .get_schema(txn, db_id, &ref_table)
                        .await?
                        .ok_or_else(|| SqlError::RelationNotFound(ref_table.clone()))?;
                    if ref_cols.is_empty() {
                        if ref_table_schema.pk_indices.is_empty() {
                            return Err(anyhow!(
                                "there is no primary key for referenced table \"{}\"",
                                short_relation_name(&ref_table)
                            ));
                        }
                        if fk_cols.len() != ref_table_schema.pk_indices.len() {
                            return Err(anyhow!(
                                "number of referencing and referenced columns for foreign key disagree"
                            ));
                        }
                    } else if fk_cols.len() != ref_cols.len() {
                        return Err(anyhow!(
                            "number of referencing and referenced columns for foreign key disagree"
                        ));
                    }
                    resolve_fk_ref_lookup(&ref_cols, &ref_table_schema)?;
                }

                foreign_keys.push(ForeignKeyConstraint {
                    name: fk_name,
                    columns: fk_cols,
                    ref_table,
                    ref_columns: ref_cols,
                    on_delete: parse_referential_action(on_delete),
                    on_update: parse_referential_action(on_update),
                });
            }
            _ => {}
        }
    }

    assign_generated_check_constraint_names(
        &table_object_name,
        !pk_indices.is_empty(),
        &indexes,
        &foreign_keys,
        &mut check_constraints,
    );

    let schema = TableSchema {
        name: table_full_name.clone(),
        table_id,
        columns: col_defs,
        version: 1,
        pk_constraint_name,
        pk_indices,
        indexes,
        check_constraints,
        foreign_keys,
        owner: "postgres".to_string(),
        from_alias: None,
    };

    // Validate self-referencing FK constraints against the final built schema.
    for fk in &schema.foreign_keys {
        if fk.ref_table != table_full_name {
            continue;
        }
        if fk.ref_columns.is_empty() {
            if schema.pk_indices.is_empty() {
                return Err(anyhow!(
                    "there is no primary key for referenced table \"{}\"",
                    table_object_name
                ));
            }
            if fk.columns.len() != schema.pk_indices.len() {
                return Err(anyhow!(
                    "number of referencing and referenced columns for foreign key disagree"
                ));
            }
        } else if fk.columns.len() != fk.ref_columns.len() {
            return Err(anyhow!(
                "number of referencing and referenced columns for foreign key disagree"
            ));
        }

        resolve_fk_ref_lookup(&fk.ref_columns, &schema)?;
    }

    store.create_table(txn, db_id, schema.clone()).await?;
    create_implicit_sequences_for_schema(store, txn, db_id, &schema).await?;

    // Reserve relation names for PK constraint and unique indexes so that
    // CREATE INDEX cannot reuse these names in the same schema.
    // Pass exclude_table to skip the just-created table in the legacy scan.
    if let Some(pk_name) = &schema.pk_constraint_name {
        check_relation_name_available(
            store,
            txn,
            db_id,
            &table_schema_name,
            pk_name,
            false,
            Some(&table_full_name),
        )
        .await?;
    }
    for idx in &schema.indexes {
        check_relation_name_available(
            store,
            txn,
            db_id,
            &table_schema_name,
            &idx.name,
            false,
            Some(&table_full_name),
        )
        .await?;
    }

    Ok(ExecuteResult::CreateTable {
        table_name: table_full_name,
    })
}

pub async fn create_table_from_query_result(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    if_not_exists: bool,
    result_cols: Vec<String>,
    result_rows: Vec<Row>,
    explicit_columns: &[SqlColumnDef],
) -> Result<ExecuteResult> {
    if if_not_exists && store.table_exists(txn, db_id, table_name).await? {
        return Ok(ExecuteResult::CreateTable {
            table_name: table_name.to_string(),
        });
    }

    // Add synthetic _rowid column as primary key (allows UPDATE/DELETE on tables without explicit PK)
    let mut col_defs: Vec<ColumnDef> = vec![ColumnDef {
        name: "_rowid".to_string(),
        data_type: DataType::Int64,
        nullable: false,
        primary_key: true,
        unique: true,
        is_serial: true,
        default_expr: None,
        collation: None,
    }];

    if explicit_columns.is_empty() {
        col_defs.extend(result_cols.iter().enumerate().map(|(i, col_name)| {
            let data_type = if !result_rows.is_empty() {
                infer_data_type(&result_rows[0].values[i])
            } else {
                DataType::Text
            };
            ColumnDef {
                name: col_name.clone(),
                data_type,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            }
        }));
    } else {
        col_defs.extend(
            explicit_columns
                .iter()
                .map(|col| {
                    let data_type = sql_datatype_to_internal_strict(&col.data_type)?;
                    Ok(ColumnDef {
                        name: normalize_ident(&col.name),
                        data_type,
                        nullable: true,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                        collation: None,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        );
    }

    let table_id = store.next_table_id(txn, db_id).await?;
    // CTAS uses an internal synthetic row-id PK for storage only; PostgreSQL
    // does not expose/reserve a user-visible "<table>_pkey" constraint name.
    // Use empty-name sentinel (not None) so legacy fallback checks do not
    // synthesize "<table>_pkey" from pk_indices.
    let pk_constraint_name = Some(String::new());
    let schema = TableSchema {
        name: table_name.to_string(),
        table_id,
        columns: col_defs,
        version: 1,
        pk_constraint_name,
        pk_indices: vec![0],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: "postgres".to_string(),
        from_alias: None,
    };
    store.create_table(txn, db_id, schema.clone()).await?;
    create_implicit_sequences_for_schema(store, txn, db_id, &schema).await?;

    let row_count = result_rows.len();
    for (i, row) in result_rows.into_iter().enumerate() {
        let mut values = vec![Value::Int64((i + 1) as i64)];
        values.extend(row.values);
        store
            .upsert(txn, db_id, table_name, Row::new(values))
            .await?;
    }

    advance_implicit_sequences_for_seeded_rows(store, txn, db_id, &schema, row_count).await?;

    Ok(ExecuteResult::CreateTable {
        table_name: table_name.to_string(),
    })
}

pub async fn create_table_from_stream(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    if_not_exists: bool,
    result_cols: Vec<String>,
    column_types: Vec<DataType>,
    stream: crate::sql::result::RowStream,
) -> Result<ExecuteResult> {
    if if_not_exists && store.table_exists(txn, db_id, table_name).await? {
        return Ok(ExecuteResult::CreateTable {
            table_name: table_name.to_string(),
        });
    }

    // Add synthetic _rowid column as primary key (allows UPDATE/DELETE on tables without explicit PK)
    let mut col_defs: Vec<ColumnDef> = vec![ColumnDef {
        name: "_rowid".to_string(),
        data_type: DataType::Int64,
        nullable: false,
        primary_key: true,
        unique: true,
        is_serial: true,
        default_expr: None,
        collation: None,
    }];

    for (i, col_name) in result_cols.iter().enumerate() {
        // INTENTIONAL: Text is the safe fallback for column types in streaming CTAS
        let data_type = column_types.get(i).cloned().unwrap_or(DataType::Text);
        col_defs.push(ColumnDef {
            name: col_name.clone(),
            data_type,
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            collation: None,
        });
    }

    let table_id = store.next_table_id(txn, db_id).await?;
    // Streaming CTAS uses an internal synthetic row-id PK for storage only;
    // same convention as batch CTAS — empty-name sentinel.
    let pk_constraint_name = Some(String::new());
    let schema = TableSchema {
        name: table_name.to_string(),
        table_id,
        columns: col_defs,
        version: 1,
        pk_constraint_name,
        pk_indices: vec![0],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: "postgres".to_string(),
        from_alias: None,
    };
    store.create_table(txn, db_id, schema.clone()).await?;
    create_implicit_sequences_for_schema(store, txn, db_id, &schema).await?;

    use futures::StreamExt;
    let mut row_stream = stream.0;
    let mut row_count: usize = 0;
    let mut batch_writes: usize = 0;
    let mut has_committed_batches = false;

    while let Some(row_result) = row_stream.next().await {
        let row = row_result?;
        row_count += 1;
        let mut values = vec![Value::Int64(row_count as i64)];
        values.extend(row.values);
        store
            .upsert(txn, db_id, table_name, Row::new(values))
            .await?;
        batch_writes += 1;
        super::maybe_rotate_backfill_txn(store, txn, &mut batch_writes, &mut has_committed_batches)
            .await?;
    }

    advance_implicit_sequences_for_seeded_rows(store, txn, db_id, &schema, row_count).await?;

    Ok(ExecuteResult::CreateTable {
        table_name: table_name.to_string(),
    })
}

pub async fn create_table_from_select_into(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
    result_cols: Vec<String>,
    result_rows: Vec<Row>,
) -> Result<ExecuteResult> {
    if store.table_exists(txn, db_id, table_name).await? {
        return Err(anyhow!("relation \"{}\" already exists", table_name));
    }

    // Add synthetic _rowid column as primary key (allows UPDATE/DELETE on tables without explicit PK)
    let mut col_defs: Vec<ColumnDef> = vec![ColumnDef {
        name: "_rowid".to_string(),
        data_type: DataType::Int64,
        nullable: false,
        primary_key: true,
        unique: true,
        is_serial: true,
        default_expr: None,
        collation: None,
    }];

    col_defs.extend(result_cols.iter().enumerate().map(|(i, col_name)| {
        let data_type = if !result_rows.is_empty() {
            infer_data_type(&result_rows[0].values[i])
        } else {
            DataType::Text
        };
        ColumnDef {
            name: col_name.clone(),
            data_type,
            nullable: true,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            collation: None,
        }
    }));

    let table_id = store.next_table_id(txn, db_id).await?;
    // SELECT INTO uses an internal synthetic row-id PK for storage only; it
    // must not reserve a user-visible "<table>_pkey" relation name.
    // Use empty-name sentinel (not None) so legacy fallback checks do not
    // synthesize "<table>_pkey" from pk_indices.
    let pk_constraint_name = Some(String::new());
    let schema = TableSchema {
        name: table_name.to_string(),
        table_id,
        columns: col_defs,
        version: 1,
        pk_constraint_name,
        pk_indices: vec![0],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: "postgres".to_string(),
        from_alias: None,
    };
    store.create_table(txn, db_id, schema.clone()).await?;
    create_implicit_sequences_for_schema(store, txn, db_id, &schema).await?;

    let row_count = result_rows.len();
    for (i, row) in result_rows.into_iter().enumerate() {
        let mut values = vec![Value::Int64((i + 1) as i64)];
        values.extend(row.values);
        store
            .upsert(txn, db_id, table_name, Row::new(values))
            .await?;
    }

    advance_implicit_sequences_for_seeded_rows(store, txn, db_id, &schema, row_count).await?;

    Ok(ExecuteResult::Insert {
        affected_rows: row_count as u64,
    })
}

/// Returns `true` if any table schema in the given namespace contains
/// an index or PK constraint named `name`.
///
/// This is the legacy-safe fallback for clusters upgraded before
/// `sys_relname_` reservation-key enforcement (issue #775).
/// The effective PK name logic mirrors `constraint_name_exists`:
/// `pk_constraint_name.unwrap_or(<table>_pkey)`.
///
/// `exclude_table` skips a specific table (used by CREATE TABLE to avoid
/// matching the table that was just created in the same transaction).
pub(crate) fn has_legacy_name_conflict<'a>(
    table_schemas: impl Iterator<Item = (&'a str, &'a TableSchema)>,
    schema_name: &str,
    name: &str,
    exclude_table: Option<&str>,
) -> bool {
    let schema_prefix = format!("{}.", schema_name);
    for (table_name, tbl_schema) in table_schemas {
        if !table_name.starts_with(&schema_prefix) {
            continue;
        }
        if exclude_table.is_some_and(|t| t == table_name) {
            continue;
        }
        // PK check — effective name = pk_constraint_name or {short}_pkey
        if !tbl_schema.pk_indices.is_empty() {
            let short = table_name.splitn(2, '.').nth(1).unwrap_or(table_name);
            let default_pk;
            let pk_name = match tbl_schema.pk_constraint_name.as_deref() {
                Some(n) => n,
                None => {
                    default_pk = format!("{}_pkey", short);
                    &default_pk
                }
            };
            if pk_name == name {
                return true;
            }
        }
        // Index check
        if tbl_schema.indexes.iter().any(|idx| idx.name == name) {
            return true;
        }
    }
    false
}

/// Check whether a relation name is available in the schema-wide namespace.
///
/// PostgreSQL requires all relation names (tables, views, matviews, sequences,
/// indexes, PK constraints) to be unique within a schema.  This helper performs
/// point-read checks against tables, views, matviews, and sequences, then
/// attempts a reservation-key write for the index/PK namespace.
///
/// Returns `Ok(true)` if the name is available (and the reservation key was
/// written).  Returns `Ok(false)` when `if_not_exists` is true and the name
/// is already taken (caller should return silently).  Returns an error with
/// `SqlError::DuplicateRelation` when the name is taken and `if_not_exists`
/// is false.
pub async fn check_relation_name_available(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema_name: &str,
    name: &str,
    if_not_exists: bool,
    exclude_table: Option<&str>,
) -> Result<bool> {
    let full_name = format!("{}.{}", schema_name, name);

    // 1. Table
    if store.table_exists(txn, db_id, &full_name).await? {
        if if_not_exists {
            return Ok(false);
        }
        return Err(SqlError::DuplicateRelation(name.to_string()).into());
    }

    // 2. View
    if store.get_view(txn, db_id, &full_name).await?.is_some() {
        if if_not_exists {
            return Ok(false);
        }
        return Err(SqlError::DuplicateRelation(name.to_string()).into());
    }

    // 3. Materialized view
    if store
        .get_materialized_view(txn, db_id, &full_name)
        .await?
        .is_some()
    {
        if if_not_exists {
            return Ok(false);
        }
        return Err(SqlError::DuplicateRelation(name.to_string()).into());
    }

    // 4. Sequence
    if store.get_sequence(txn, db_id, &full_name).await?.is_some() {
        if if_not_exists {
            return Ok(false);
        }
        return Err(SqlError::DuplicateRelation(name.to_string()).into());
    }

    // 5. Legacy-safe: scan all table schemas in this namespace for
    //    indexes / PK constraints with the same name.  Covers data
    //    created before sys_relname_ enforcement (issue #775).
    //    Sunset policy: remove after 2026-12-31 once all clusters have
    //    completed migration and reservation backfill.
    warn_legacy_relname_conflict_scan_once();
    let schema_prefix = format!("{}.", schema_name);
    let mut table_schemas: Vec<(String, TableSchema)> = Vec::new();
    for table_name in store.list_tables(txn, db_id).await? {
        if !table_name.starts_with(&schema_prefix) {
            continue;
        }
        if let Some(tbl_schema) = store.get_schema(txn, db_id, &table_name).await? {
            table_schemas.push((table_name, tbl_schema));
        }
    }
    if has_legacy_name_conflict(
        table_schemas.iter().map(|(n, s)| (n.as_str(), s)),
        schema_name,
        name,
        exclude_table,
    ) {
        if if_not_exists {
            return Ok(false);
        }
        return Err(SqlError::DuplicateRelation(name.to_string()).into());
    }

    // 6. Reservation key — covers index-vs-index and index-vs-PK conflicts,
    //    and simultaneously reserves the name via TiKV write-write detection.
    match store.reserve_relation_name(txn, db_id, &full_name).await {
        Ok(()) => Ok(true),
        Err(e) => {
            if if_not_exists
                && e.downcast_ref::<SqlError>()
                    .is_some_and(|se| matches!(se, SqlError::DuplicateRelation(_)))
            {
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}
