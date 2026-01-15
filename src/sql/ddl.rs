use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use sqlparser::ast::{
    AlterColumnOperation, AlterTableOperation, ColumnDef as SqlColumnDef, ColumnOption, Expr,
    GeneratedAs, ObjectName, OrderByExpr, Query, TableConstraint,
};
use tikv_client::Transaction;

use super::helpers::{
    coerce_value_for_column, convert_data_type, fill_row_defaults, infer_data_type, is_serial_type,
    normalize_ident,
};
use super::{expr::eval_expr, ExecuteResult};
use crate::storage::TikvStore;
use crate::types::{
    CheckConstraint, ColumnDef, DataType, ForeignKeyAction, ForeignKeyConstraint, IndexDef, Row,
    TableSchema, Value,
};
use crate::txn::{txn_delete, txn_put};

const DDL_SCAN_BATCH_SIZE: u32 = 1024;

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
    used_names.extend(
        checks
            .iter()
            .filter_map(|c| c.name.as_ref())
            .cloned(),
    );

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
    let pk_name = format!("{}_pkey", table);
    if !schema.pk_indices.is_empty() && name == pk_name {
        return true;
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

fn index_prefix_range(table_id: u64, index_id: u64) -> (Vec<u8>, Vec<u8>) {
    // Compute the end bound by incrementing the fixed-length (table_id, index_id) prefix,
    // so the range is independent of memcomparable-encoded index values.
    let mut prefix = Vec::with_capacity(2 + 8 + 1 + 8);
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
                Err(anyhow!("Unsupported type conversion to {}", new_type))
            }
        }
    }
}

pub async fn execute_create_table(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    name: &ObjectName,
    columns: &[SqlColumnDef],
    constraints: &[TableConstraint],
    if_not_exists: bool,
) -> Result<ExecuteResult> {
    let table_name = name
        .0
        .last()
        .map(normalize_ident)
        .ok_or_else(|| anyhow!("Invalid table name"))?;

    if if_not_exists && store.table_exists(txn, &table_name).await? {
        return Ok(ExecuteResult::CreateTable { table_name });
    }

    let pk_columns: Vec<String> = constraints
        .iter()
        .filter_map(|c| match c {
            TableConstraint::Unique {
                columns,
                is_primary,
                ..
            } if *is_primary => Some(columns.iter().map(normalize_ident).collect::<Vec<_>>()),
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
        let (data_type, mut is_serial) = if is_serial_type(&col.data_type) {
            (DataType::Int32, true)
        } else {
            (convert_data_type(&col.data_type)?, false)
        };

        let mut is_pk = pk_columns.contains(&col_name);
        let mut nullable = true;
        let mut unique = false;
        let mut default_expr = None;

        for opt in &col.options {
            match &opt.option {
                ColumnOption::Unique { is_primary, .. } => {
                    if *is_primary {
                        is_pk = true;
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
                    let ref_table = foreign_table
                        .0
                        .last()
                        .map(normalize_ident)
                        .unwrap_or_default();
                    let ref_cols: Vec<String> =
                        referred_columns.iter().map(normalize_ident).collect();
                    let fk_name = format!("{}_{}_fkey", table_name, col_name);

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

    let table_id = store.next_table_id(txn).await?;

    let mut indexes = Vec::new();
    let mut next_index_id = 1u64;

    for col in col_defs.iter() {
        if col.unique && !col.primary_key {
            indexes.push(IndexDef {
                name: format!("{}_{}_key", table_name, col.name),
                id: next_index_id,
                columns: vec![col.name.clone()],
                unique: true,
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
                    let idx_name = name
                        .as_ref()
                        .map(|n| n.value.clone())
                        .unwrap_or_else(|| format!("{}_{}_key", table_name, col_names.join("_")));
                    indexes.push(IndexDef {
                        name: idx_name,
                        id: next_index_id,
                        columns: col_names,
                        unique: true,
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
                let ref_table = foreign_table
                    .0
                    .last()
                    .map(normalize_ident)
                    .unwrap_or_default();
                let ref_cols: Vec<String> = referred_columns.iter().map(normalize_ident).collect();
                let fk_name = name
                    .as_ref()
                    .map(|n| n.value.clone())
                    .unwrap_or_else(|| format!("{}_{}_fkey", table_name, fk_cols.join("_")));

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
        &table_name,
        !pk_indices.is_empty(),
        &indexes,
        &foreign_keys,
        &mut check_constraints,
    );

    let schema = TableSchema {
        name: table_name.clone(),
        table_id,
        columns: col_defs,
        version: 1,
        pk_indices,
        indexes,
        check_constraints,
        foreign_keys,
    };
    store.create_table(txn, schema).await?;

    Ok(ExecuteResult::CreateTable { table_name })
}

pub async fn create_table_from_query_result(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    table_name: &str,
    if_not_exists: bool,
    result_cols: Vec<String>,
    result_rows: Vec<Row>,
    explicit_columns: &[SqlColumnDef],
) -> Result<ExecuteResult> {
    if if_not_exists && store.table_exists(txn, table_name).await? {
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
        col_defs.extend(explicit_columns.iter().map(|col| {
            let data_type = convert_data_type(&col.data_type).unwrap_or(DataType::Text);
            ColumnDef {
                name: normalize_ident(&col.name),
                data_type,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }
        }));
    }

    let table_id = store.next_table_id(txn).await?;
    let schema = TableSchema {
        name: table_name.to_string(),
        table_id,
        columns: col_defs,
        version: 1,
        pk_indices: vec![0],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    };
    store.create_table(txn, schema.clone()).await?;

    let row_count = result_rows.len();
    for (i, row) in result_rows.into_iter().enumerate() {
        let mut values = vec![Value::Int64((i + 1) as i64)];
        values.extend(row.values);
        store.upsert(txn, table_name, Row::new(values)).await?;
    }

    if row_count > 0 {
        store
            .set_sequence_value(txn, schema.table_id, row_count as u64)
            .await?;
    }

    Ok(ExecuteResult::CreateTable {
        table_name: table_name.to_string(),
    })
}

pub async fn create_table_from_select_into(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    table_name: &str,
    result_cols: Vec<String>,
    result_rows: Vec<Row>,
) -> Result<ExecuteResult> {
    if store.table_exists(txn, table_name).await? {
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

    let table_id = store.next_table_id(txn).await?;
    let schema = TableSchema {
        name: table_name.to_string(),
        table_id,
        columns: col_defs,
        version: 1,
        pk_indices: vec![0],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
    };
    store.create_table(txn, schema.clone()).await?;

    let row_count = result_rows.len();
    for (i, row) in result_rows.into_iter().enumerate() {
        let mut values = vec![Value::Int64((i + 1) as i64)];
        values.extend(row.values);
        store.upsert(txn, table_name, Row::new(values)).await?;
    }

    if row_count > 0 {
        store
            .set_sequence_value(txn, schema.table_id, row_count as u64)
            .await?;
    }

    Ok(ExecuteResult::Insert {
        affected_rows: row_count as u64,
    })
}

pub async fn execute_create_index(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    idx_name: &str,
    table_name: &ObjectName,
    columns: &[OrderByExpr],
    unique: bool,
    if_not_exists: bool,
    rows: Vec<Row>,
) -> Result<ExecuteResult> {
    let idx_name_str = idx_name.to_lowercase();
    let tbl_name = table_name.0.last().map(normalize_ident).unwrap();

    let mut schema = store
        .get_schema(txn, &tbl_name)
        .await?
        .ok_or_else(|| anyhow!("Table not found"))?;

    if schema.indexes.iter().any(|i| i.name == idx_name_str) {
        if if_not_exists {
            return Ok(ExecuteResult::CreateIndex {
                index_name: idx_name_str,
            });
        }
        return Err(anyhow!("Index exists"));
    }

    let mut idx_cols = Vec::new();
    for col_expr in columns {
        if let Expr::Identifier(ident) = &col_expr.expr {
            let col_name = normalize_ident(ident);
            if schema.column_index(&col_name).is_none() {
                return Err(anyhow!("Column not found"));
            }
            idx_cols.push(col_name);
        } else {
            return Err(anyhow!("Index column must be identifier"));
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
    };

    for row in rows {
        let idx_values = schema.get_index_values(&new_index, &row);
        let pk_values = schema.get_pk_values(&row);
        store
            .create_index_entry(
                txn,
                schema.table_id,
                index_id,
                &idx_values,
                &pk_values,
                unique,
            )
            .await?;
    }

    schema.indexes.push(new_index);
    store.update_schema(txn, schema).await?;

    Ok(ExecuteResult::CreateIndex {
        index_name: idx_name_str,
    })
}

pub async fn execute_create_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    name: &ObjectName,
    query: &Query,
    or_replace: bool,
) -> Result<ExecuteResult> {
    let view_name = name
        .0
        .last()
        .map(normalize_ident)
        .ok_or_else(|| anyhow!("Invalid view name"))?;

    if store.get_view(txn, &view_name).await?.is_some() {
        if or_replace {
            store.drop_view(txn, &view_name).await?;
        } else {
            return Err(anyhow!("View '{}' already exists", view_name));
        }
    }

    let query_str = query.to_string();
    store.create_view(txn, &view_name, &query_str).await?;

    Ok(ExecuteResult::CreateView { view_name })
}

pub async fn execute_drop_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    names: &[ObjectName],
    if_exists: bool,
) -> Result<ExecuteResult> {
    let mut last = String::new();
    for name in names {
        let v = name.0.last().map(normalize_ident).unwrap();
        if !store.drop_view(txn, &v).await? && !if_exists {
            return Err(anyhow!("View '{}' does not exist", v));
        }
        last = v;
    }
    Ok(ExecuteResult::DropView { view_name: last })
}

pub async fn execute_create_materialized_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    name: &ObjectName,
    query: &Query,
    or_replace: bool,
    schema: TableSchema,
    rows: Vec<Row>,
) -> Result<ExecuteResult> {
    let view_name = name
        .0
        .last()
        .map(normalize_ident)
        .ok_or_else(|| anyhow!("Invalid materialized view name"))?;

    if store
        .get_materialized_view(txn, &view_name)
        .await?
        .is_some()
    {
        if or_replace {
            store.drop_materialized_view(txn, &view_name).await?;
            store.drop_table(txn, &view_name).await?;
        } else {
            return Err(anyhow!("Materialized view '{}' already exists", view_name));
        }
    }

    let query_str = query.to_string();
    store
        .create_materialized_view(txn, &view_name, &query_str)
        .await?;

    store.create_table(txn, schema).await?;
    for row in rows {
        store.insert(txn, &view_name, row).await?;
    }

    Ok(ExecuteResult::CreateMaterializedView {
        view_name: view_name.clone(),
    })
}

pub async fn execute_drop_materialized_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    names: &[ObjectName],
    if_exists: bool,
) -> Result<ExecuteResult> {
    let mut last = String::new();
    for name in names {
        let v = name.0.last().map(normalize_ident).unwrap();
        let exists = store.drop_materialized_view(txn, &v).await?;
        if !exists && !if_exists {
            return Err(anyhow!("Materialized view '{}' does not exist", v));
        }
        if exists {
            store.drop_table(txn, &v).await?;
        }
        last = v;
    }
    Ok(ExecuteResult::DropMaterializedView { view_name: last })
}

pub async fn execute_refresh_materialized_view(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    name: &str,
    rows: Vec<Row>,
) -> Result<ExecuteResult> {
    let name_lower = name.to_lowercase();
    if store
        .get_materialized_view(txn, &name_lower)
        .await?
        .is_none()
    {
        return Err(anyhow!("Materialized view '{}' does not exist", name));
    }

    store.truncate_table(txn, &name_lower).await?;
    for row in rows {
        store.insert(txn, &name_lower, row).await?;
    }

    Ok(ExecuteResult::RefreshMaterializedView {
        view_name: name_lower,
    })
}

pub async fn execute_drop_table(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    names: &[ObjectName],
    if_exists: bool,
) -> Result<ExecuteResult> {
    let mut last = String::new();
    for name in names {
        let t = name.0.last().map(normalize_ident).unwrap();
        if !store.drop_table(txn, &t).await? && !if_exists {
            return Err(anyhow!("Table '{}' does not exist", t));
        }
        last = t;
    }
    Ok(ExecuteResult::DropTable { table_name: last })
}

pub async fn execute_truncate(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    table_name: &ObjectName,
) -> Result<ExecuteResult> {
    let t = table_name.0.last().map(normalize_ident).unwrap();
    if !store.truncate_table(txn, &t).await? {
        return Err(anyhow!("Table '{}' does not exist", t));
    }
    Ok(ExecuteResult::TruncateTable { table_name: t })
}

pub async fn execute_drop_index(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    idx_name: &str,
    schema: &mut TableSchema,
    _table_name: &str,
    rows: Vec<Row>,
) -> Result<Option<String>> {
    if let Some(pos) = schema.indexes.iter().position(|i| i.name == idx_name) {
        let index = schema.indexes.remove(pos);
        for row in rows {
            let idx_values = schema.get_index_values(&index, &row);
            let pk_values = schema.get_pk_values(&row);
            store
                .delete_index_entry(
                    txn,
                    schema.table_id,
                    index.id,
                    &idx_values,
                    &pk_values,
                    index.unique,
                )
                .await?;
        }
        store.update_schema(txn, schema.clone()).await?;
        return Ok(Some(idx_name.to_string()));
    }
    Ok(None)
}

pub async fn execute_alter_table(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    name: &ObjectName,
    operation: &AlterTableOperation,
) -> Result<ExecuteResult> {
    let t = name.0.last().map(normalize_ident).unwrap();
    let mut result_table_name = t.clone();
    let mut schema = store
        .get_schema(txn, &t)
        .await?
        .ok_or_else(|| anyhow!("Table '{}' does not exist", t))?;
    assign_generated_check_constraint_names(
        &t,
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
            let data_type = convert_data_type(&column_def.data_type)?;
            let mut nullable = true;
            let mut default_expr = None;
            for opt in &column_def.options {
                match &opt.option {
                    ColumnOption::NotNull => nullable = false,
                    ColumnOption::Default(expr) => default_expr = Some(expr.to_string()),
                    _ => {}
                }
            }
            if !nullable && default_expr.is_none() {
                let (start, end) = crate::storage::encode_table_data_range(schema.table_id);
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
                is_serial: false,
                default_expr,
            });
            schema.version += 1;
            store.update_schema(txn, schema).await?;
        }
        AlterTableOperation::AddConstraint(constraint) => match constraint {
            TableConstraint::Unique {
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
                schema.version += 1;
                store.update_schema(txn, schema).await?;
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

                let index_name = name
                    .as_ref()
                    .map(normalize_ident)
                    .unwrap_or_else(|| format!("{}_{}_key", t, col_names.join("_")));

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
                };

                let (start, end) = crate::storage::encode_table_data_range(schema.table_id);
                let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
                while let Some(batch) = scanner.next_batch(txn).await? {
                    for pair in batch {
                        let mut row = crate::storage::deserialize_row(pair.value())?;
                        fill_row_defaults(&mut row, &schema)?;

                        let idx_values = schema.get_index_values(&new_index, &row);
                        let pk_values = schema.get_pk_values(&row);
                        store
                            .create_index_entry(
                                txn,
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
                store.update_schema(txn, schema).await?;
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

                let ref_table = foreign_table
                    .0
                    .last()
                    .map(normalize_ident)
                    .unwrap_or_default();
                let ref_schema = store.get_schema(txn, &ref_table).await?.ok_or_else(|| {
                    anyhow!(
                        "Referenced table '{}' not found for foreign key",
                        ref_table
                    )
                })?;
                let ref_cols: Vec<String> = referred_columns.iter().map(normalize_ident).collect();
                let fk_name = name
                    .as_ref()
                    .map(normalize_ident)
                    .unwrap_or_else(|| format!("{}_{}_fkey", t, fk_cols.join("_")));

                if constraint_name_exists(&schema, &t, &fk_name) {
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
                let (start, end) = crate::storage::encode_table_data_range(schema.table_id);
                let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
                while let Some(batch) = scanner.next_batch(txn).await? {
                    for pair in batch {
                        let mut row = crate::storage::deserialize_row(pair.value())?;
                        fill_row_defaults(&mut row, &schema)?;

                        let mut fk_values: Vec<Value> = Vec::with_capacity(fk_cols.len());
                        let mut all_null = true;
                        for col_name in &fk_cols {
                            let idx = schema
                                .column_index(col_name)
                                .expect("validated above");
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
                store.update_schema(txn, schema).await?;
            }
            TableConstraint::Check { name, expr } => {
                let expr_str = expr.to_string();
                let check_name = if let Some(n) = name.as_ref() {
                    normalize_ident(n)
                } else {
                    let mut suffix = 1usize;
                    loop {
                        let candidate = format!("{}_check{}", t, suffix);
                        if !constraint_name_exists(&schema, &t, &candidate) {
                            break candidate;
                        }
                        suffix += 1;
                    }
                };

                if constraint_name_exists(&schema, &t, &check_name) {
                    return Err(anyhow!("Constraint '{}' already exists", check_name));
                }

                // PostgreSQL validates existing rows by default (unless NOT VALID).
                let (start, end) = crate::storage::encode_table_data_range(schema.table_id);
                let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
                while let Some(batch) = scanner.next_batch(txn).await? {
                    for pair in batch {
                        let mut row = crate::storage::deserialize_row(pair.value())?;
                        fill_row_defaults(&mut row, &schema)?;

                        let result = eval_expr(expr, Some(&row), Some(&schema))?;
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
                store.update_schema(txn, schema).await?;
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
                return Err(anyhow!("DROP CONSTRAINT ... CASCADE is not supported"));
            }

            let constraint_name = normalize_ident(name);
            let pk_name = format!("{}_pkey", t);
            if !schema.pk_indices.is_empty() && constraint_name == pk_name {
                return Err(anyhow!(
                    "Cannot drop primary key constraint '{}'",
                    constraint_name
                ));
            }

            if let Some(pos) = schema
                .foreign_keys
                .iter()
                .position(|fk| fk.name == constraint_name)
            {
                schema.foreign_keys.remove(pos);
                schema.version += 1;
                store.update_schema(txn, schema).await?;
                return Ok(ExecuteResult::AlterTable {
                    table_name: result_table_name,
                });
            }

            if let Some(pos) = find_check_constraint_index(&schema, &t, &constraint_name) {
                schema.check_constraints.remove(pos);
                schema.version += 1;
                store.update_schema(txn, schema).await?;
                return Ok(ExecuteResult::AlterTable {
                    table_name: result_table_name,
                });
            }

            if let Some(pos) = schema.indexes.iter().position(|i| i.name == constraint_name) {
                let index = schema.indexes[pos].clone();
                if !index.unique {
                    if !if_exists {
                        return Err(anyhow!("Constraint '{}' does not exist", constraint_name));
                    }
                    return Ok(ExecuteResult::AlterTable {
                        table_name: result_table_name,
                    });
                }

                let (start, end) = index_prefix_range(schema.table_id, index.id);
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
                store.update_schema(txn, schema).await?;
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
                return Err(anyhow!("DROP COLUMN ... CASCADE is not supported"));
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

                    let (start, end) = crate::storage::encode_table_data_range(schema.table_id);
                    let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
                    while let Some(batch) = scanner.next_batch(txn).await? {
                        for pair in batch {
                            let (key, value): (tikv_client::Key, tikv_client::Value) =
                                pair.into();
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
                    store.update_schema(txn, schema).await?;
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
            store.update_schema(txn, schema).await?;
        }
        AlterTableOperation::RenameTable { table_name } => {
            let new_table = table_name
                .0
                .last()
                .map(normalize_ident)
                .ok_or_else(|| anyhow!("Invalid table name"))?;

            store
                .rename_table_schema(txn, &t, &new_table)
                .await?;
            result_table_name = new_table.clone();

            // Update referencing-side metadata (FKs store ref_table as a string).
            let tables = store.list_tables(txn).await?;
            for table in tables {
                let mut s = match store.get_schema(txn, &table).await? {
                    Some(s) => s,
                    None => continue,
                };
                let mut changed = false;
                for fk in &mut s.foreign_keys {
                    if fk.ref_table == t {
                        fk.ref_table = new_table.clone();
                        changed = true;
                    }
                }
                if changed {
                    s.version += 1;
                    store.update_schema(txn, s).await?;
                }
            }
        }
        AlterTableOperation::RenameConstraint { old_name, new_name } => {
            let old = normalize_ident(old_name);
            let new = normalize_ident(new_name);

            if constraint_name_exists(&schema, &t, &new) {
                return Err(anyhow!("Constraint '{}' already exists", new));
            }

            let pk_name = format!("{}_pkey", t);
            if !schema.pk_indices.is_empty() && old == pk_name {
                return Err(anyhow!("Cannot rename primary key constraint '{}'", old));
            }

            if let Some(fk) = schema.foreign_keys.iter_mut().find(|fk| fk.name == old) {
                fk.name = new;
                schema.version += 1;
                store.update_schema(txn, schema).await?;
                return Ok(ExecuteResult::AlterTable {
                    table_name: result_table_name,
                });
            }

            if let Some(pos) = find_check_constraint_index(&schema, &t, &old) {
                schema.check_constraints[pos].name = Some(new);
                schema.version += 1;
                store.update_schema(txn, schema).await?;
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
                store.update_schema(txn, schema).await?;
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
                    store.update_schema(txn, schema).await?;
                }
                AlterColumnOperation::DropDefault => {
                    schema.columns[col_idx].default_expr = None;
                    schema.version += 1;
                    store.update_schema(txn, schema).await?;
                }
                AlterColumnOperation::SetNotNull => {
                    if !schema.columns[col_idx].nullable {
                        return Ok(ExecuteResult::AlterTable {
                            table_name: result_table_name,
                        });
                    }

                    let (start, end) = crate::storage::encode_table_data_range(schema.table_id);
                    let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
                    while let Some(batch) = scanner.next_batch(txn).await? {
                        for pair in batch {
                            let mut row = crate::storage::deserialize_row(pair.value())?;
                            fill_row_defaults(&mut row, &schema)?;
                            if matches!(row.values[col_idx], Value::Null) {
                                return Err(anyhow!("Column '{}' cannot be null", col_name));
                            }
                        }
                    }

                    schema.columns[col_idx].nullable = false;
                    schema.version += 1;
                    store.update_schema(txn, schema).await?;
                }
                AlterColumnOperation::DropNotNull => {
                    schema.columns[col_idx].nullable = true;
                    schema.version += 1;
                    store.update_schema(txn, schema).await?;
                }
                AlterColumnOperation::SetDataType { data_type, using } => {
                    if using.is_some() {
                        return Err(anyhow!("ALTER COLUMN ... TYPE ... USING is not supported"));
                    }

                    if schema.pk_indices.contains(&col_idx) {
                        return Err(anyhow!("Cannot alter type of primary key column '{}'", col_name));
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

                    let new_type = convert_data_type(data_type)?;
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
                        let (start, end) = index_prefix_range(schema.table_id, idx.id);
                        delete_range(txn, start, end).await?;
                    }

                    let mut target_col = schema.columns[col_idx].clone();
                    target_col.data_type = new_type.clone();

                    let (start, end) = crate::storage::encode_table_data_range(schema.table_id);
                    let mut scanner = KvScanBatches::new(start, end, DDL_SCAN_BATCH_SIZE);
                    while let Some(batch) = scanner.next_batch(txn).await? {
                        for pair in batch {
                            let (key, value): (tikv_client::Key, tikv_client::Value) =
                                pair.into();
                            let mut row = crate::storage::deserialize_row(&value)?;
                            fill_row_defaults(&mut row, &schema)?;

                            let old_val = std::mem::replace(&mut row.values[col_idx], Value::Null);
                            let new_val = coerce_value_for_type_change(old_val, &target_col)?;
                            row.values[col_idx] = new_val;

                            let pk_values = schema.get_pk_values(&row);
                            for idx in &affected_indexes {
                                let idx_values = schema.get_index_values(idx, &row);
                                store
                                    .create_index_entry(
                                        txn,
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
                    store.update_schema(txn, schema).await?;
                }
            }
        }
        _ => return Err(anyhow!("Unsupported ALTER")),
    }

    Ok(ExecuteResult::AlterTable {
        table_name: result_table_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_expr_reference_ignores_string_literals() {
        assert!(!check_expr_references_column("note = 'age'", "age").unwrap());
        assert!(check_expr_references_column("age > 0 AND note = 'age'", "age").unwrap());
    }

    #[test]
    fn index_prefix_range_includes_all_index_entries() {
        let (start, end) = index_prefix_range(42, 7);

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

        let (next_start, _) = index_prefix_range(42, 8);
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
