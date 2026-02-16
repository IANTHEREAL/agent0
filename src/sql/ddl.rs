use std::collections::HashSet;
use std::sync::Arc;

use crate::sql::analyzer::types::TypedExpr;
use crate::sql::analyzer::{Analyzer, CatalogSnapshot, Scope};
use crate::sql::error::SqlError;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::expr::typed_fold::fold_typed_expr;
use crate::sql::query_context::QueryContext;
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    AlterColumnOperation, AlterTableOperation, ColumnDef as SqlColumnDef, ColumnOption,
    DataType as SqlDataType, Expr, GeneratedAs, ObjectName, OrderByExpr, Query, TableConstraint,
};
use tikv_client::Transaction;

use super::dml;
use super::gin::{extract_gin_token_hashes_from_row, supported_gin_index_column};
use super::index_helpers;
use super::names;
use super::names::normalize_ident;
use super::projection::fill_row_defaults;
use super::sequences;
use super::types::sql_datatype_to_internal_strict;
use super::value_coercion::{coerce_value_for_column, infer_data_type};
use super::ExecuteResult;

use crate::storage::TikvStore;
use crate::txn::{txn_delete, txn_put};
use crate::types::{
    CheckConstraint, ColumnDef, DataType, ForeignKeyAction, ForeignKeyConstraint, IndexDef, Row,
    TableSchema, Value,
};

const DDL_SCAN_BATCH_SIZE: u32 = 1024;
const DDL_BACKFILL_COMMIT_SIZE: usize = 5000;

fn analyze_row_level_expr(
    expr: &Expr,
    schema: &TableSchema,
    db_id: u64,
    search_path: &[String],
) -> Result<TypedExpr> {
    let mut catalog = CatalogSnapshot::new(search_path.to_vec(), db_id);
    let table_name = schema.name.rsplit('.').next().unwrap_or(&schema.name);
    catalog.add_table(table_name, schema.name.clone(), schema.clone());
    if let Some(alias) = &schema.from_alias {
        catalog.add_table(alias, schema.name.clone(), schema.clone());
    }

    let typed = Analyzer::analyze_expr_with_scope(
        &catalog,
        Scope::from_table_schema(table_name, schema),
        expr,
    )
    .map_err(SqlError::from)?;
    let qctx = QueryContext::from_task_locals();
    Ok(fold_typed_expr(&typed, &qctx))
}

fn eval_row_level_expr(typed_expr: &TypedExpr, row: &Row, qctx: &QueryContext) -> Result<Value> {
    eval_typed_expr(typed_expr, row, qctx)
}

async fn resolve_column_data_type(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    sql_type: &SqlDataType,
) -> Result<(DataType, bool)> {
    match sql_type {
        SqlDataType::Custom(name, _) => {
            let type_ident = name.0.last().ok_or_else(|| anyhow!("Invalid type name"))?;
            let type_name = type_ident.value.to_uppercase();

            match type_name.as_str() {
                "SERIAL" => Ok((DataType::Int32, true)),
                "BIGSERIAL" => Ok((DataType::Int64, true)),
                _ => {
                    let resolved_type = names::resolve_existing_type_name(
                        store.as_ref(),
                        txn,
                        db_id,
                        name,
                        search_path,
                    )
                    .await?;
                    let Some(resolved_type) = resolved_type else {
                        return Ok((sql_datatype_to_internal_strict(sql_type)?, false));
                    };
                    let full_name = resolved_type.full;
                    match store.get_type(txn, db_id, &full_name).await? {
                        Some(def) => match def.kind {
                            crate::types::UserTypeKind::Enum { .. } => {
                                Ok((DataType::UserDefined(full_name), false))
                            }
                            crate::types::UserTypeKind::Composite { .. } => Err(anyhow!(
                                "composite type '{}' cannot be used as a column type",
                                full_name
                            )),
                        },
                        None => Ok((sql_datatype_to_internal_strict(sql_type)?, false)),
                    }
                }
            }
        }
        _ => Ok((sql_datatype_to_internal_strict(sql_type)?, false)),
    }
}

async fn create_implicit_sequences_for_schema(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
) -> Result<()> {
    for col in &schema.columns {
        if col.is_serial {
            store
                .create_sequence(
                    txn,
                    db_id,
                    sequences::build_implicit_sequence_def(&schema.name, &col.name, &col.data_type),
                )
                .await?;
        }
    }
    Ok(())
}

async fn advance_implicit_sequences_for_seeded_rows(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    schema: &TableSchema,
    row_count: usize,
) -> Result<()> {
    if row_count == 0 {
        return Ok(());
    }

    let last_value = i64::try_from(row_count)
        .map_err(|_| anyhow!("row count {} overflows sequence value", row_count))?;
    let (table_schema, table_name) = schema
        .name
        .rsplit_once('.')
        .unwrap_or(("public", schema.name.as_str()));

    for col in &schema.columns {
        if !col.is_serial {
            continue;
        }

        let seq_full_name = format!(
            "{}.{}",
            table_schema,
            sequences::implicit_sequence_name(table_name, &col.name)
        );
        store
            .setval_sequence(txn, db_id, &seq_full_name, last_value, true)
            .await?;
    }

    Ok(())
}

async fn drop_owned_sequences_for_table(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    table_name: &str,
) -> Result<()> {
    let seqs = store.list_sequences(txn, db_id).await?;
    for def in seqs {
        let Some((owned_table, _)) = &def.owned_by else {
            continue;
        };
        if owned_table == table_name {
            store.drop_sequence(txn, db_id, &def.full_name()).await?;
        }
    }
    Ok(())
}

struct KvScanBatches {
    next_start: Option<Vec<u8>>,
    end: Vec<u8>,
    limit: u32,
}

impl KvScanBatches {
    fn new(start: Vec<u8>, end: Vec<u8>, limit: u32) -> Self {
        Self {
            next_start: Some(start),
            end,
            limit,
        }
    }

    async fn next_batch(
        &mut self,
        txn: &mut Transaction,
    ) -> Result<Option<Vec<tikv_client::KvPair>>> {
        let Some(start) = self.next_start.take() else {
            return Ok(None);
        };

        let range: tikv_client::BoundRange = (start..self.end.clone()).into();
        let pairs: Vec<tikv_client::KvPair> = txn.scan(range, self.limit).await?.collect();
        if pairs.is_empty() {
            self.next_start = None;
            return Ok(None);
        }

        let last_key: &[u8] = pairs.last().unwrap().key().as_ref().into();
        let mut next_start = last_key.to_vec();
        next_start.push(0x00);
        self.next_start = Some(next_start);

        Ok(Some(pairs))
    }
}

fn assign_generated_check_constraint_names(
    table: &str,
    pk_exists: bool,
    indexes: &[IndexDef],
    foreign_keys: &[ForeignKeyConstraint],
    checks: &mut Vec<CheckConstraint>,
) {
    let mut used_names: HashSet<String> = HashSet::new();
    if pk_exists {
        used_names.insert(format!("{}_pkey", table));
    }
    used_names.extend(indexes.iter().map(|idx| idx.name.clone()));
    used_names.extend(foreign_keys.iter().map(|fk| fk.name.clone()));
    used_names.extend(checks.iter().filter_map(|c| c.name.as_ref()).cloned());

    for (ordinal, check) in checks.iter_mut().enumerate() {
        if check.name.is_some() {
            continue;
        }

        let mut suffix = ordinal + 1;
        loop {
            let candidate = format!("{}_check{}", table, suffix);
            suffix += 1;
            if used_names.insert(candidate.clone()) {
                check.name = Some(candidate);
                break;
            }
        }
    }
}

fn check_constraint_effective_name(table: &str, ordinal: usize, check: &CheckConstraint) -> String {
    check
        .name
        .clone()
        .unwrap_or_else(|| format!("{}_check{}", table, ordinal + 1))
}

fn find_check_constraint_index(schema: &TableSchema, table: &str, name: &str) -> Option<usize> {
    schema
        .check_constraints
        .iter()
        .enumerate()
        .find_map(|(i, c)| {
            let effective = check_constraint_effective_name(table, i, c);
            (effective == name).then_some(i)
        })
}

fn constraint_name_exists(schema: &TableSchema, table: &str, name: &str) -> bool {
    if !schema.pk_indices.is_empty() {
        let default_pk_name;
        let pk_name = match schema.pk_constraint_name.as_deref() {
            Some(n) => n,
            None => {
                default_pk_name = format!("{}_pkey", table);
                &default_pk_name
            }
        };
        if name == pk_name {
            return true;
        }
    }

    if schema.foreign_keys.iter().any(|fk| fk.name == name) {
        return true;
    }
    if schema.indexes.iter().any(|idx| idx.name == name) {
        return true;
    }
    if schema
        .check_constraints
        .iter()
        .enumerate()
        .any(|(i, c)| check_constraint_effective_name(table, i, c) == name)
    {
        return true;
    }

    false
}

fn check_expr_references_column(expr_str: &str, col_name: &str) -> Result<bool> {
    use core::ops::ControlFlow;
    use sqlparser::ast::{visit_expressions, Expr as AstExpr};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    let dialect = PostgreSqlDialect {};
    let expr = Parser::new(&dialect)
        .try_with_sql(expr_str)
        .and_then(|mut p| p.parse_expr())
        .map_err(|e| anyhow!("Invalid CHECK expression '{}': {}", expr_str, e))?;

    let mut found = false;
    let _ = visit_expressions(&expr, |e| {
        if found {
            return ControlFlow::Break(());
        }

        match e {
            AstExpr::Identifier(ident) => {
                if normalize_ident(ident) == col_name {
                    found = true;
                    return ControlFlow::Break(());
                }
            }
            AstExpr::CompoundIdentifier(parts) => {
                if let Some(last) = parts.last() {
                    if normalize_ident(last) == col_name {
                        found = true;
                        return ControlFlow::Break(());
                    }
                }
            }
            _ => {}
        }

        ControlFlow::<()>::Continue(())
    });
    Ok(found)
}

fn rewrite_check_expr_column(
    expr_str: &str,
    old_col_name: &str,
    new_col_name: &str,
    new_quote_style: Option<char>,
) -> Result<String> {
    use core::ops::ControlFlow;
    use sqlparser::ast::{visit_expressions_mut, Expr as AstExpr};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    let dialect = PostgreSqlDialect {};
    let mut expr = Parser::new(&dialect)
        .try_with_sql(expr_str)
        .and_then(|mut p| p.parse_expr())
        .map_err(|e| anyhow!("Invalid CHECK expression '{}': {}", expr_str, e))?;

    let _ = visit_expressions_mut(&mut expr, |e| {
        match e {
            AstExpr::Identifier(ident) => {
                if normalize_ident(ident) == old_col_name {
                    ident.value = new_col_name.to_string();
                    ident.quote_style = new_quote_style;
                }
            }
            AstExpr::CompoundIdentifier(parts) => {
                if let Some(last) = parts.last_mut() {
                    if normalize_ident(last) == old_col_name {
                        last.value = new_col_name.to_string();
                        last.quote_style = new_quote_style;
                    }
                }
            }
            _ => {}
        }

        ControlFlow::<()>::Continue(())
    });

    Ok(expr.to_string())
}

fn prefix_end(mut key: Vec<u8>) -> Vec<u8> {
    for i in (0..key.len()).rev() {
        if key[i] != 0xFF {
            key[i] = key[i].wrapping_add(1);
            key.truncate(i + 1);
            return key;
        }
    }

    unreachable!("prefix_end called with all-0xFF prefix")
}

fn index_prefix_range(db_id: u64, table_id: u64, index_id: u64) -> (Vec<u8>, Vec<u8>) {
    // Compute the end bound by incrementing the fixed-length (table_id, index_id) prefix,
    // so the range is independent of memcomparable-encoded index values.
    let mut prefix = Vec::with_capacity(2 + 8 + 1 + 2 + 8 + 1 + 8);
    prefix.extend_from_slice(b"d_");
    prefix.extend_from_slice(&db_id.to_be_bytes());
    prefix.push(b'_');
    prefix.extend_from_slice(b"i_");
    prefix.extend_from_slice(&table_id.to_be_bytes());
    prefix.push(b'_');
    prefix.extend_from_slice(&index_id.to_be_bytes());

    let mut start = prefix.clone();
    start.push(b'_');
    let end = prefix_end(prefix);

    (start, end)
}

async fn delete_range(txn: &mut Transaction, start: Vec<u8>, end: Vec<u8>) -> Result<()> {
    let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
    while let Some(batch) = scanner.next_batch(txn).await? {
        for pair in batch {
            txn_delete(txn, pair.into_key().into()).await?;
        }
    }
    Ok(())
}

async fn maybe_rotate_backfill_txn(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    current_batch_writes: &mut usize,
    has_committed_batches: &mut bool,
) -> Result<()> {
    if *current_batch_writes < DDL_BACKFILL_COMMIT_SIZE {
        return Ok(());
    }

    txn.commit().await?;
    *txn = store.begin().await?;
    *current_batch_writes = 0;
    *has_committed_batches = true;
    Ok(())
}

fn coerce_value_for_type_change(val: Value, target_col: &ColumnDef) -> Result<Value> {
    let new_type = &target_col.data_type;
    if matches!(val, Value::Null) {
        return Ok(Value::Null);
    }

    if val.data_type().as_ref() == Some(new_type) {
        return Ok(val);
    }

    match (val, new_type) {
        (Value::Json(s), DataType::Text) => Ok(Value::Text(s)),
        (Value::Jsonb(s), DataType::Text) => Ok(Value::Text(s)),
        (Value::Uuid(bytes), DataType::Text) => {
            Ok(Value::Text(uuid::Uuid::from_bytes(bytes).to_string()))
        }
        (v, _) => {
            let coerced = coerce_value_for_column(v, target_col)?;
            if coerced.data_type().as_ref() == Some(new_type) {
                Ok(coerced)
            } else {
                Err(
                    SqlError::Unsupported(format!("Unsupported type conversion to {}", new_type))
                        .into(),
                )
            }
        }
    }
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
                        name: None,
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

                    let parse_action =
                        |action: &Option<sqlparser::ast::ReferentialAction>| -> ForeignKeyAction {
                            match action {
                                Some(sqlparser::ast::ReferentialAction::Cascade) => {
                                    ForeignKeyAction::Cascade
                                }
                                Some(sqlparser::ast::ReferentialAction::SetNull) => {
                                    ForeignKeyAction::SetNull
                                }
                                Some(sqlparser::ast::ReferentialAction::SetDefault) => {
                                    ForeignKeyAction::SetDefault
                                }
                                Some(sqlparser::ast::ReferentialAction::Restrict) => {
                                    ForeignKeyAction::Restrict
                                }
                                Some(sqlparser::ast::ReferentialAction::NoAction) | None => {
                                    ForeignKeyAction::NoAction
                                }
                            }
                        };

                    foreign_keys.push(ForeignKeyConstraint {
                        name: fk_name,
                        columns: vec![col_name.clone()],
                        ref_table,
                        ref_columns: ref_cols,
                        on_delete: parse_action(on_delete),
                        on_update: parse_action(on_update),
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

                let parse_action =
                    |action: &Option<sqlparser::ast::ReferentialAction>| -> ForeignKeyAction {
                        match action {
                            Some(sqlparser::ast::ReferentialAction::Cascade) => {
                                ForeignKeyAction::Cascade
                            }
                            Some(sqlparser::ast::ReferentialAction::SetNull) => {
                                ForeignKeyAction::SetNull
                            }
                            Some(sqlparser::ast::ReferentialAction::SetDefault) => {
                                ForeignKeyAction::SetDefault
                            }
                            Some(sqlparser::ast::ReferentialAction::Restrict) => {
                                ForeignKeyAction::Restrict
                            }
                            Some(sqlparser::ast::ReferentialAction::NoAction) | None => {
                                ForeignKeyAction::NoAction
                            }
                        }
                    };

                foreign_keys.push(ForeignKeyConstraint {
                    name: fk_name,
                    columns: fk_cols,
                    ref_table,
                    ref_columns: ref_cols,
                    on_delete: parse_action(on_delete),
                    on_update: parse_action(on_update),
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
    store.create_table(txn, db_id, schema.clone()).await?;
    create_implicit_sequences_for_schema(store, txn, db_id, &schema).await?;

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
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        );
    }

    let table_id = store.next_table_id(txn, db_id).await?;
    let pk_constraint_name = Some(format!(
        "{}_pkey",
        table_name.rsplit('.').next().unwrap_or(table_name)
    ));
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
        }
    }));

    let table_id = store.next_table_id(txn, db_id).await?;
    let pk_constraint_name = Some(format!(
        "{}_pkey",
        table_name.rsplit('.').next().unwrap_or(table_name)
    ));
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

pub async fn execute_create_index(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    idx_name: &str,
    table_name: &str,
    using: Option<&sqlparser::ast::Ident>,
    columns: &[OrderByExpr],
    unique: bool,
    if_not_exists: bool,
    predicate: Option<&Expr>,
    rows: Vec<Row>,
) -> Result<ExecuteResult> {
    let idx_name_str = idx_name.to_string();
    let tbl_name = table_name;

    let mut schema = store
        .get_schema(txn, db_id, tbl_name)
        .await?
        .ok_or_else(|| SqlError::RelationNotFound(tbl_name.to_string()))?;

    if schema.indexes.iter().any(|i| i.name == idx_name_str) {
        if if_not_exists {
            return Ok(ExecuteResult::CreateIndex {
                index_name: idx_name_str,
            });
        }
        return Err(anyhow!("Index exists"));
    }

    let method = using.map(|m| m.value.to_lowercase());

    if let Some(pred_expr) = predicate {
        index_helpers::validate_index_predicate(pred_expr, &schema)?;
    }

    let predicate_str = predicate.map(|p| p.to_string());

    let mut idx_cols = Vec::new();
    let mut idx_exprs = Vec::new();
    for col_expr in columns {
        let mut expr = &col_expr.expr;
        while let Expr::Nested(inner) = expr {
            expr = inner.as_ref();
        }

        match expr {
            Expr::Identifier(ident) => {
                let col_name = normalize_ident(ident);
                if schema.column_index(&col_name).is_none() {
                    return Err(anyhow!("Column not found"));
                }
                idx_cols.push(col_name);
            }
            Expr::CompoundIdentifier(parts) => {
                let Some(last) = parts.last() else {
                    return Err(anyhow!("Index column must be identifier"));
                };
                let col_name = normalize_ident(last);
                if schema.column_index(&col_name).is_none() {
                    return Err(anyhow!("Column not found"));
                }
                idx_cols.push(col_name);
            }
            _ => {
                idx_exprs.push(expr.to_string());
            }
        }
    }

    let index_id = schema
        .indexes
        .iter()
        .map(|i| i.id)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| anyhow!("Index id overflow"))?;
    let new_index = IndexDef {
        name: idx_name_str.clone(),
        id: index_id,
        columns: idx_cols,
        unique,
        method,
        predicate: predicate_str,
        expressions: idx_exprs,
    };

    let mut current_batch_writes = 0usize;
    let mut has_committed_batches = false;

    let create_result: Result<()> = async {
        if index_helpers::is_index_materializable(&new_index) {
            if !rows.is_empty() {
                if schema.pk_indices.is_empty() {
                    let pk_types: Vec<DataType> = vec![DataType::Uuid];
                    let (start, end) =
                        crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
                    let data_key_prefix = start.clone();
                    let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
                    while let Some(batch) = scanner.next_batch(txn).await? {
                        for pair in batch {
                            let key: &[u8] = pair.key().as_ref().into();
                            let pk_bytes = key
                                .strip_prefix(data_key_prefix.as_slice())
                                .ok_or_else(|| {
                                    anyhow!(
                                        "corrupted row key while backfilling index '{}'",
                                        idx_name_str
                                    )
                                })?;
                            let pk_values =
                                crate::storage::decode_pk_from_index_suffix(pk_bytes, &pk_types)?;

                            let mut row = crate::storage::deserialize_row(pair.value())?;
                            fill_row_defaults(&mut row, &schema)?;

                            if !index_helpers::eval_index_predicate(&new_index, &schema, &row)? {
                                continue;
                            }
                            let idx_values = index_helpers::get_index_values_with_expressions(
                                &new_index, &schema, &row,
                            )?;
                            store
                                .create_index_entry(
                                    txn,
                                    db_id,
                                    schema.table_id,
                                    index_id,
                                    &idx_values,
                                    &pk_values,
                                    unique,
                                )
                                .await?;
                            current_batch_writes += 1;
                            maybe_rotate_backfill_txn(
                                store,
                                txn,
                                &mut current_batch_writes,
                                &mut has_committed_batches,
                            )
                            .await?;
                        }
                    }
                } else {
                    for row in rows {
                        if !index_helpers::eval_index_predicate(&new_index, &schema, &row)? {
                            continue;
                        }
                        let idx_values = index_helpers::get_index_values_with_expressions(
                            &new_index, &schema, &row,
                        )?;
                        let pk_values = schema.get_pk_values(&row);
                        store
                            .create_index_entry(
                                txn,
                                db_id,
                                schema.table_id,
                                index_id,
                                &idx_values,
                                &pk_values,
                                unique,
                            )
                            .await?;
                        current_batch_writes += 1;
                        maybe_rotate_backfill_txn(
                            store,
                            txn,
                            &mut current_batch_writes,
                            &mut has_committed_batches,
                        )
                        .await?;
                    }
                }
            }
        } else if supported_gin_index_column(&schema, &new_index).is_some() {
            if !rows.is_empty() {
                if schema.pk_indices.is_empty() {
                    let pk_types: Vec<DataType> = vec![DataType::Uuid];
                    let (start, end) =
                        crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
                    let data_key_prefix = start.clone();
                    let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
                    while let Some(batch) = scanner.next_batch(txn).await? {
                        for pair in batch {
                            let key: &[u8] = pair.key().as_ref().into();
                            let pk_bytes = key
                                .strip_prefix(data_key_prefix.as_slice())
                                .ok_or_else(|| {
                                    anyhow!(
                                        "corrupted row key while backfilling index '{}'",
                                        idx_name_str
                                    )
                                })?;
                            let pk_values =
                                crate::storage::decode_pk_from_index_suffix(pk_bytes, &pk_types)?;

                            let mut row = crate::storage::deserialize_row(pair.value())?;
                            fill_row_defaults(&mut row, &schema)?;

                            let hashes =
                                extract_gin_token_hashes_from_row(&schema, &new_index, &row)?;
                            if hashes.is_empty() {
                                continue;
                            }
                            store
                                .create_gin_index_entries(
                                    txn,
                                    db_id,
                                    schema.table_id,
                                    index_id,
                                    &hashes,
                                    &pk_values,
                                )
                                .await?;
                            current_batch_writes += 1;
                            maybe_rotate_backfill_txn(
                                store,
                                txn,
                                &mut current_batch_writes,
                                &mut has_committed_batches,
                            )
                            .await?;
                        }
                    }
                } else {
                    for row in rows {
                        let hashes = extract_gin_token_hashes_from_row(&schema, &new_index, &row)?;
                        if hashes.is_empty() {
                            continue;
                        }
                        let pk_values = schema.get_pk_values(&row);
                        store
                            .create_gin_index_entries(
                                txn,
                                db_id,
                                schema.table_id,
                                index_id,
                                &hashes,
                                &pk_values,
                            )
                            .await?;
                        current_batch_writes += 1;
                        maybe_rotate_backfill_txn(
                            store,
                            txn,
                            &mut current_batch_writes,
                            &mut has_committed_batches,
                        )
                        .await?;
                    }
                }
            }
        }

        schema.indexes.push(new_index.clone());
        store.update_schema(txn, db_id, schema.clone()).await?;
        Ok(())
    }
    .await;

    if let Err(err) = create_result {
        if has_committed_batches {
            // Backfill commits can succeed before schema update. On failure after that point,
            // remove committed entries so CREATE INDEX does not leave orphaned index KV data.
            let _ = txn.rollback().await;
            let (start, end) = index_prefix_range(db_id, schema.table_id, index_id);
            let cleanup_result: Result<()> = async {
                let mut cleanup_txn = store.begin().await?;
                delete_range(&mut cleanup_txn, start, end).await?;
                cleanup_txn.commit().await?;
                Ok(())
            }
            .await;

            *txn = store.begin().await?;

            if let Err(cleanup_err) = cleanup_result {
                return Err(err.context(format!(
                    "failed to cleanup partially backfilled index '{}': {}",
                    idx_name_str, cleanup_err
                )));
            }
        }
        return Err(err);
    }

    Ok(ExecuteResult::CreateIndex {
        index_name: idx_name_str,
    })
}

/// Resolve raw `RelationDep`s into fully-qualified dependency names.
///
/// - `Qualified { schema, name }` → `"{schema}.{name}"` directly.
/// - `Unqualified { name }` → search each schema in `search_path` order,
///   checking tables, views, and materialized views.  First hit wins.
///   Fallback: `"{view_schema}.{name}"`.
async fn resolve_view_deps(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    view_schema: &str,
    search_path: &[String],
    raw_deps: std::collections::HashSet<super::binder::RelationDep>,
) -> Result<Vec<String>> {
    use super::binder::RelationDep;
    let mut resolved = Vec::with_capacity(raw_deps.len());
    for dep in raw_deps {
        match dep {
            RelationDep::Qualified { schema, name } => {
                resolved.push(format!("{}.{}", schema, name));
            }
            RelationDep::Unqualified { name } => {
                let mut found = false;
                for schema in search_path {
                    let full = format!("{}.{}", schema, name);
                    if store.table_exists(txn, db_id, &full).await?
                        || store.get_view(txn, db_id, &full).await?.is_some()
                        || store
                            .get_materialized_view(txn, db_id, &full)
                            .await?
                            .is_some()
                    {
                        resolved.push(full);
                        found = true;
                        break;
                    }
                }
                if !found {
                    resolved.push(format!("{}.{}", view_schema, name));
                }
            }
        }
    }
    resolved.sort();
    resolved.dedup();
    Ok(resolved)
}

pub async fn execute_create_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    name: &ObjectName,
    query: &Query,
    or_replace: bool,
) -> Result<ExecuteResult> {
    let resolved = names::resolve_ddl_object_name(name, search_path)?;
    if !store.schema_exists(txn, db_id, &resolved.schema).await? {
        return Err(anyhow!("schema '{}' does not exist", resolved.schema));
    }
    let view_name = resolved.full;

    let query_str = query.to_string();
    let deps = match super::binder::extract_dependencies(&query_str) {
        Ok(raw) => resolve_view_deps(store, txn, db_id, &resolved.schema, search_path, raw).await?,
        Err(_) => Vec::new(),
    };
    store
        .create_view(txn, db_id, &view_name, &query_str, deps, or_replace)
        .await?;

    Ok(ExecuteResult::CreateView { view_name })
}

pub async fn execute_drop_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    names: &[ObjectName],
    if_exists: bool,
    cascade: bool,
) -> Result<ExecuteResult> {
    // Track names already dropped by CASCADE dependency resolution so that
    // multi-name DROP statements (e.g. DROP VIEW v1, v2 CASCADE where v2
    // depends on v1) don't error when a later name was already removed.
    let mut cascade_dropped: HashSet<String> = HashSet::new();
    let mut last = String::new();
    for name in names {
        let resolved =
            match names::resolve_existing_view_name(store.as_ref(), txn, db_id, name, search_path)
                .await?
            {
                Some(resolved) => resolved,
                None => {
                    // Already dropped as a transitive dependent — not an error.
                    if cascade && was_cascade_dropped(name, search_path, &cascade_dropped) {
                        continue;
                    }
                    if !if_exists {
                        return Err(anyhow!("View '{}' does not exist", name));
                    }
                    continue;
                }
            };

        // CASCADE: drop views that depend on this view.
        if cascade {
            let dropped = drop_dependent_views(store, txn, db_id, &resolved.full).await?;
            cascade_dropped.extend(dropped);
        }

        if !store.drop_view(txn, db_id, &resolved.full).await? && !if_exists {
            return Err(anyhow!("View '{}' does not exist", resolved.full));
        }
        last = resolved.full;
    }
    Ok(ExecuteResult::DropView { view_name: last })
}

pub async fn execute_create_materialized_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    name: &ObjectName,
    query: &Query,
    or_replace: bool,
    mut schema: TableSchema,
    rows: Vec<Row>,
) -> Result<ExecuteResult> {
    let resolved = names::resolve_ddl_object_name(name, search_path)?;
    if !store.schema_exists(txn, db_id, &resolved.schema).await? {
        return Err(anyhow!("schema '{}' does not exist", resolved.schema));
    }
    let view_name = resolved.full;
    schema.name = view_name.clone();

    if store
        .get_materialized_view(txn, db_id, &view_name)
        .await?
        .is_some()
    {
        if or_replace {
            for trigger in store
                .list_triggers_for_table(txn, db_id, &view_name)
                .await?
            {
                let _ = store
                    .drop_trigger(txn, db_id, &view_name, &trigger.name)
                    .await?;
            }
            drop_owned_sequences_for_table(store, txn, db_id, &view_name).await?;
            store.drop_materialized_view(txn, db_id, &view_name).await?;
            store.drop_table(txn, db_id, &view_name).await?;
        } else {
            return Err(anyhow!("Materialized view '{}' already exists", view_name));
        }
    }

    let query_str = query.to_string();
    let deps = match super::binder::extract_dependencies(&query_str) {
        Ok(raw) => resolve_view_deps(store, txn, db_id, &resolved.schema, search_path, raw).await?,
        Err(_) => Vec::new(),
    };
    store
        .create_materialized_view(txn, db_id, &view_name, &query_str, deps)
        .await?;

    let row_count = rows.len();
    store.create_table(txn, db_id, schema.clone()).await?;
    create_implicit_sequences_for_schema(store, txn, db_id, &schema).await?;
    for row in rows {
        store.insert(txn, db_id, &view_name, row).await?;
    }
    advance_implicit_sequences_for_seeded_rows(store, txn, db_id, &schema, row_count).await?;

    Ok(ExecuteResult::CreateMaterializedView { view_name })
}

pub async fn execute_drop_materialized_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    names: &[ObjectName],
    if_exists: bool,
    cascade: bool,
) -> Result<ExecuteResult> {
    let mut cascade_dropped: HashSet<String> = HashSet::new();
    let mut last = String::new();
    for name in names {
        let resolved = match names::resolve_existing_materialized_view_name(
            store.as_ref(),
            txn,
            db_id,
            name,
            search_path,
        )
        .await?
        {
            Some(resolved) => resolved,
            None => {
                if cascade && was_cascade_dropped(name, search_path, &cascade_dropped) {
                    continue;
                }
                if !if_exists {
                    return Err(anyhow!("Materialized view '{}' does not exist", name));
                }
                continue;
            }
        };

        // CASCADE: drop views/matviews that depend on this materialized view.
        if cascade {
            let dropped = drop_dependent_views(store, txn, db_id, &resolved.full).await?;
            cascade_dropped.extend(dropped);
        }

        let exists = store
            .drop_materialized_view(txn, db_id, &resolved.full)
            .await?;
        if !exists && !if_exists {
            return Err(anyhow!(
                "Materialized view '{}' does not exist",
                resolved.full
            ));
        }
        if exists {
            for trigger in store
                .list_triggers_for_table(txn, db_id, &resolved.full)
                .await?
            {
                let _ = store
                    .drop_trigger(txn, db_id, &resolved.full, &trigger.name)
                    .await?;
            }
            drop_owned_sequences_for_table(store, txn, db_id, &resolved.full).await?;
            store.drop_table(txn, db_id, &resolved.full).await?;
        }
        last = resolved.full;
    }
    Ok(ExecuteResult::DropMaterializedView { view_name: last })
}

pub async fn execute_refresh_materialized_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    name: &str,
    rows: Vec<Row>,
) -> Result<ExecuteResult> {
    if store
        .get_materialized_view(txn, db_id, name)
        .await?
        .is_none()
    {
        return Err(anyhow!("Materialized view '{}' does not exist", name));
    }

    let schema = store
        .get_schema(txn, db_id, name)
        .await?
        .ok_or_else(|| anyhow!("Materialized view '{}' does not exist", name))?;

    if !store.truncate_table(txn, db_id, name).await? {
        return Err(anyhow!("Materialized view '{}' does not exist", name));
    }
    let enum_cache = dml::build_enum_label_cache(store, txn, db_id, &schema).await?;
    let row_count = rows.len();
    for row in rows {
        dml::execute_insert_row(
            store,
            txn,
            db_id,
            name,
            &schema,
            row,
            dml::ConflictBehavior::Error,
            &enum_cache,
        )
        .await?;
    }
    advance_implicit_sequences_for_seeded_rows(store, txn, db_id, &schema, row_count).await?;

    Ok(ExecuteResult::RefreshMaterializedView {
        view_name: name.to_string(),
    })
}

pub async fn execute_drop_table(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    names: &[ObjectName],
    if_exists: bool,
    cascade: bool,
) -> Result<ExecuteResult> {
    let mut last = String::new();
    for name in names {
        let resolved =
            match names::resolve_existing_table_name(store.as_ref(), txn, db_id, name, search_path)
                .await?
            {
                Some(resolved) => resolved,
                None => {
                    if !if_exists {
                        return Err(anyhow!("Table '{}' does not exist", name));
                    }
                    continue;
                }
            };

        // CASCADE: drop views that depend on this table.
        if cascade {
            let _dropped = drop_dependent_views(store, txn, db_id, &resolved.full).await?;
        }

        for trigger in store
            .list_triggers_for_table(txn, db_id, &resolved.full)
            .await?
        {
            let _ = store
                .drop_trigger(txn, db_id, &resolved.full, &trigger.name)
                .await?;
        }
        drop_owned_sequences_for_table(store, txn, db_id, &resolved.full).await?;
        store.drop_table(txn, db_id, &resolved.full).await?;
        last = resolved.full;
    }
    Ok(ExecuteResult::DropTable { table_name: last })
}

/// Check whether `name` (possibly unqualified) resolves to any entry in
/// `dropped` (a set of fully-qualified names).  Tries all schemas in the
/// search path for unqualified names.
fn was_cascade_dropped(
    name: &ObjectName,
    search_path: &[String],
    dropped: &HashSet<String>,
) -> bool {
    let (schema_opt, obj) = match names::split_object_name(name) {
        Ok(r) => r,
        Err(_) => return false,
    };
    match schema_opt {
        Some(schema) => names::ResolvedName::new(schema, obj)
            .map(|r| dropped.contains(&r.full))
            .unwrap_or(false),
        None => {
            let default: Vec<String> = vec!["public".to_string()];
            let schemas = if search_path.is_empty() {
                &default
            } else {
                search_path
            };
            schemas.iter().any(|schema| {
                names::ResolvedName::new(schema.clone(), obj.clone())
                    .map(|r| dropped.contains(&r.full))
                    .unwrap_or(false)
            })
        }
    }
}

/// Drop all views and materialized views that depend on `target_name`,
/// then transitively drop anything that depended on the dropped objects.
///
/// Uses the `deps` field stored in each ViewDef / MatViewDef to determine
/// dependencies — no SQL re-parsing needed at drop time.  Iterates until
/// a fixed point so transitive chains (table → v1 → mv1 → v2) are fully
/// resolved.
///
/// The root `target_name` is never dropped here — the caller is responsible
/// for dropping it.  This prevents cycles (e.g. v1 ↔ v2) from removing
/// the target before the caller can, which would cause a spurious
/// "does not exist" error (#639).
///
/// Returns the set of fully-qualified names that were dropped, so the
/// caller can skip names already handled in multi-name DROP (#640).
async fn drop_dependent_views(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    target_name: &str,
) -> Result<HashSet<String>> {
    let mut dropped = HashSet::new();
    // Names pending dependency resolution; starts with the dropped object.
    let mut pending: Vec<String> = vec![target_name.to_string()];

    // Iterate until no new dependents are found (fixed-point).
    while !pending.is_empty() {
        let mut next_pending: Vec<String> = Vec::new();

        // --- regular views ---
        let views = store.list_views(txn, db_id).await?;
        for view in &views {
            let full = view.full_name();
            if full == target_name {
                continue;
            }
            if pending.iter().any(|p| view.deps.contains(p)) {
                store.drop_view(txn, db_id, &full).await?;
                dropped.insert(full.clone());
                next_pending.push(full);
            }
        }

        // --- materialized views ---
        let matviews = store.list_materialized_views(txn, db_id).await?;
        for mv in &matviews {
            let full = mv.full_name();
            if full == target_name {
                continue;
            }
            if pending.iter().any(|p| mv.deps.contains(p)) {
                store.drop_materialized_view(txn, db_id, &full).await?;
                for trigger in store.list_triggers_for_table(txn, db_id, &full).await? {
                    let _ = store.drop_trigger(txn, db_id, &full, &trigger.name).await?;
                }
                drop_owned_sequences_for_table(store, txn, db_id, &full).await?;
                store.drop_table(txn, db_id, &full).await?;
                dropped.insert(full.clone());
                next_pending.push(full);
            }
        }

        pending = next_pending;
    }

    Ok(dropped)
}

pub async fn execute_truncate(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    table_name: &ObjectName,
) -> Result<ExecuteResult> {
    let resolved =
        names::resolve_existing_table_name(store.as_ref(), txn, db_id, table_name, search_path)
            .await?
            .ok_or_else(|| anyhow!("Table '{}' does not exist", table_name))?;
    let t = resolved.full;
    if !store.truncate_table(txn, db_id, &t).await? {
        return Err(anyhow!("Table '{}' does not exist", t));
    }
    Ok(ExecuteResult::TruncateTable { table_name: t })
}

pub async fn execute_drop_index(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    idx_name: &str,
    schema: &mut TableSchema,
    _table_name: &str,
    rows: Vec<Row>,
) -> Result<Option<String>> {
    if let Some(pos) = schema.indexes.iter().position(|i| i.name == idx_name) {
        let index = schema.indexes.remove(pos);
        if schema.pk_indices.is_empty() {
            let pk_types: Vec<DataType> = vec![DataType::Uuid];
            let (start, end) = crate::storage::encode_table_data_range_v2(db_id, schema.table_id);
            let data_key_prefix = start.clone();
            let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
            while let Some(batch) = scanner.next_batch(txn).await? {
                for pair in batch {
                    let key: &[u8] = pair.key().as_ref().into();
                    let pk_bytes =
                        key.strip_prefix(data_key_prefix.as_slice())
                            .ok_or_else(|| {
                                anyhow!("corrupted row key while dropping index '{}'", idx_name)
                            })?;
                    let pk_values =
                        crate::storage::decode_pk_from_index_suffix(pk_bytes, &pk_types)?;

                    let mut row = crate::storage::deserialize_row(pair.value())?;
                    fill_row_defaults(&mut row, schema)?;

                    let gin_hashes = extract_gin_token_hashes_from_row(schema, &index, &row)?;
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

                    if !index_helpers::is_index_materializable(&index) {
                        continue;
                    }

                    if !index_helpers::eval_index_predicate(&index, schema, &row)? {
                        continue;
                    }
                    let idx_values =
                        index_helpers::get_index_values_with_expressions(&index, schema, &row)?;
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
            }
        } else {
            for row in rows {
                let pk_values = schema.get_pk_values(&row);

                let gin_hashes = extract_gin_token_hashes_from_row(schema, &index, &row)?;
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

                if !index_helpers::is_index_materializable(&index) {
                    continue;
                }

                if !index_helpers::eval_index_predicate(&index, schema, &row)? {
                    continue;
                }
                let idx_values =
                    index_helpers::get_index_values_with_expressions(&index, schema, &row)?;
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
        }
        store.update_schema(txn, db_id, schema.clone()).await?;
        return Ok(Some(idx_name.to_string()));
    }
    Ok(None)
}

pub async fn execute_alter_table(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    search_path: &[String],
    name: &ObjectName,
    operation: &AlterTableOperation,
) -> Result<ExecuteResult> {
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
            schema.columns.push(ColumnDef {
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
                schema.pk_constraint_name = Some(
                    name.as_ref()
                        .map(normalize_ident)
                        .unwrap_or_else(|| format!("{}_pkey", table_object_name)),
                );
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

                if schema.indexes.iter().any(|i| i.name == index_name) {
                    return Err(anyhow!("Index exists"));
                }

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

                let parse_action =
                    |action: &Option<sqlparser::ast::ReferentialAction>| -> ForeignKeyAction {
                        match action {
                            Some(sqlparser::ast::ReferentialAction::Cascade) => {
                                ForeignKeyAction::Cascade
                            }
                            Some(sqlparser::ast::ReferentialAction::SetNull) => {
                                ForeignKeyAction::SetNull
                            }
                            Some(sqlparser::ast::ReferentialAction::SetDefault) => {
                                ForeignKeyAction::SetDefault
                            }
                            Some(sqlparser::ast::ReferentialAction::Restrict) => {
                                ForeignKeyAction::Restrict
                            }
                            Some(sqlparser::ast::ReferentialAction::NoAction) | None => {
                                ForeignKeyAction::NoAction
                            }
                        }
                    };

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
                            return Err(anyhow!(
                                "insert or update on table \"{}\" violates foreign key constraint \"{}\"\n\
                                 DETAIL:  Key ({})=({}) is not present in table \"{}\".",
                                schema.name,
                                fk_name,
                                cols,
                                vals.join(", "),
                                ref_table
                            ));
                        }
                    }
                }

                schema.foreign_keys.push(ForeignKeyConstraint {
                    name: fk_name,
                    columns: fk_cols,
                    ref_table,
                    ref_columns: ref_cols,
                    on_delete: parse_action(on_delete),
                    on_update: parse_action(on_update),
                });
                schema.version += 1;
                store.update_schema(txn, db_id, schema).await?;
            }
            TableConstraint::Check { name, expr } => {
                let expr_str = expr.to_string();
                let check_name = if let Some(n) = name.as_ref() {
                    normalize_ident(n)
                } else {
                    let mut suffix = 1usize;
                    loop {
                        let candidate = format!("{}_check{}", table_object_name, suffix);
                        if !constraint_name_exists(&schema, &table_object_name, &candidate) {
                            break candidate;
                        }
                        suffix += 1;
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
                    schema.pk_constraint_name = None;
                    schema.version += 1;
                    store.update_schema(txn, db_id, schema).await?;
                    return Ok(ExecuteResult::AlterTable {
                        table_name: result_table_name,
                    });
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
                return Ok(ExecuteResult::AlterTable {
                    table_name: result_table_name,
                });
            }

            if let Some(pos) =
                find_check_constraint_index(&schema, &table_object_name, &constraint_name)
            {
                schema.check_constraints.remove(pos);
                schema.version += 1;
                store.update_schema(txn, db_id, schema).await?;
                return Ok(ExecuteResult::AlterTable {
                    table_name: result_table_name,
                });
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
                    return Ok(ExecuteResult::AlterTable {
                        table_name: result_table_name,
                    });
                }

                let (start, end) = index_prefix_range(db_id, schema.table_id, index.id);
                delete_range(txn, start, end).await?;
                schema.indexes.remove(pos);

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
                return Ok(ExecuteResult::AlterTable {
                    table_name: result_table_name,
                });
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
                return Err(anyhow!("Constraint '{}' already exists", new));
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
                return Ok(ExecuteResult::AlterTable {
                    table_name: result_table_name,
                });
            }

            if let Some(pos) = find_check_constraint_index(&schema, &table_object_name, &old) {
                schema.check_constraints[pos].name = Some(new);
                schema.version += 1;
                store.update_schema(txn, db_id, schema).await?;
                return Ok(ExecuteResult::AlterTable {
                    table_name: result_table_name,
                });
            }

            if let Some(idx) = schema
                .indexes
                .iter_mut()
                .find(|idx| idx.name == old && idx.unique)
            {
                idx.name = new;
                schema.version += 1;
                store.update_schema(txn, db_id, schema).await?;
                return Ok(ExecuteResult::AlterTable {
                    table_name: result_table_name,
                });
            }

            return Err(anyhow!("Constraint '{}' does not exist", old));
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
                        return Ok(ExecuteResult::AlterTable {
                            table_name: result_table_name,
                        });
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
                    if schema.columns[col_idx].data_type == new_type {
                        return Ok(ExecuteResult::AlterTable {
                            table_name: result_table_name,
                        });
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
                }
            }
        }
        _ => return Err(SqlError::Unsupported("Unsupported ALTER".into()).into()),
    }

    Ok(ExecuteResult::AlterTable {
        table_name: result_table_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_table_default_current_timestamp_precision_is_preserved() {
        use sqlparser::dialect::PostgreSqlDialect;
        use sqlparser::parser::Parser;

        let dialect = PostgreSqlDialect {};
        let ast = Parser::parse_sql(
            &dialect,
            "CREATE TABLE ts_precision (id INT PRIMARY KEY, ts TIMESTAMP DEFAULT CURRENT_TIMESTAMP(0));",
        )
        .unwrap();

        let sqlparser::ast::Statement::CreateTable { columns, .. } = &ast[0] else {
            panic!("expected CREATE TABLE");
        };

        let ts_col = columns
            .iter()
            .find(|c| c.name.value.eq_ignore_ascii_case("ts"))
            .expect("ts column must exist");

        let mut default_expr = None;
        for opt in &ts_col.options {
            if let sqlparser::ast::ColumnOption::Default(expr) = &opt.option {
                if let sqlparser::ast::Expr::Function(func) = expr {
                    assert_eq!(func.args.len(), 1);
                }
                default_expr = Some(expr.to_string());
            }
        }

        assert_eq!(default_expr.unwrap(), "CURRENT_TIMESTAMP(0)");
    }

    #[test]
    fn check_expr_reference_ignores_string_literals() {
        assert!(!check_expr_references_column("note = 'age'", "age").unwrap());
        assert!(check_expr_references_column("age > 0 AND note = 'age'", "age").unwrap());
    }

    #[test]
    fn index_prefix_range_includes_all_index_entries() {
        let (start, end) = index_prefix_range(5, 42, 7);

        for suffix in [
            &[0x00][..],
            &[0x01][..],
            &[0x5F][..],
            &[0x60][..],
            &[0x7F][..],
            &[0xFF][..],
            &[0xFF, 0x00][..],
        ] {
            let mut key = start.clone();
            key.extend_from_slice(suffix);
            assert!(key >= start);
            assert!(key < end);
        }

        let (next_start, _) = index_prefix_range(5, 42, 8);
        assert!(next_start >= end);
    }

    #[test]
    fn rewrite_check_expr_column_rewrites_identifiers_only() {
        let out = rewrite_check_expr_column(
            "age > 0 AND note = 'age' AND t.age < 10",
            "age",
            "years",
            None,
        )
        .unwrap();
        assert!(out.contains("years > 0"));
        assert!(out.contains("t.years"));
        assert!(out.contains("'age'"));
    }

    #[test]
    fn rewrite_check_expr_column_respects_quote_style() {
        let out = rewrite_check_expr_column("age > 0", "age", "Years", Some('"')).unwrap();
        assert!(out.contains("\"Years\""));
    }
}
