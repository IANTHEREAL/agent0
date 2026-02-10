//! DML operations (INSERT, UPDATE, DELETE) for the SQL executor

use super::super::dml;
use super::super::expr::{
    coerce_text_literal_to_bool, validate_bool_expr_in_boolean_context, JoinEvalContext,
};
use super::super::names;
use super::super::names::normalize_ident;
use super::super::trigger_queue::TriggerOp;
use super::super::trigger_worker;
use super::super::triggers;
use super::super::ExecuteResult;
use super::core::Executor;
use crate::sql::error::SqlError;
use crate::types::{Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    Assignment, DataType as SqlDataType, Expr, Ident, ObjectName, OnInsert, Query, SelectItem,
    SetExpr, TimezoneInfo, Value as SqlValue, Values,
};
use std::collections::HashMap;
use tikv_client::Transaction;

const BOOLEAN_CONTEXT_ERR_MSG: &str = "Filter predicate must evaluate to boolean";

fn build_type_infer_schema_for_two_table_join(
    left_alias: &str,
    left_schema: &TableSchema,
    right_alias: &str,
    right_schema: &TableSchema,
) -> TableSchema {
    let mut columns = Vec::new();
    columns.extend(left_schema.columns.clone());
    columns.extend(right_schema.columns.clone());

    for col in &left_schema.columns {
        let mut qualified = col.clone();
        qualified.name = format!("{}.{}", left_alias, col.name);
        columns.push(qualified);
    }

    for col in &right_schema.columns {
        let mut qualified = col.clone();
        qualified.name = format!("{}.{}", right_alias, col.name);
        columns.push(qualified);
    }

    TableSchema {
        name: "joined_infer".to_string(),
        table_id: 0,
        columns,
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        from_alias: None,
    }
}

fn predicate_value_to_bool(expr: &Expr, value: Value) -> Result<bool> {
    let value = coerce_text_literal_to_bool(expr, value)?;
    match value {
        Value::Boolean(b) => Ok(b),
        Value::Null => Ok(false),
        other => Err(anyhow!("{}, got {:?}", BOOLEAN_CONTEXT_ERR_MSG, other)),
    }
}

fn value_to_expr(val: Value, _col_name: Option<&str>) -> Result<Expr> {
    Ok(match val {
        Value::Null => Expr::Value(SqlValue::Null),
        Value::Boolean(b) => Expr::Value(SqlValue::Boolean(b)),
        Value::Int32(n) => Expr::Value(SqlValue::Number(n.to_string(), false)),
        Value::Int64(n) => Expr::Value(SqlValue::Number(n.to_string(), false)),
        Value::Float64(n) => Expr::Value(SqlValue::Number(n.to_string(), false)),
        Value::Numeric(d) => Expr::Value(SqlValue::Number(d.to_string(), false)),
        Value::Text(s) => Expr::Value(SqlValue::SingleQuotedString(s)),
        Value::Bytes(b) => Expr::Value(SqlValue::HexStringLiteral(hex::encode(b))),
        Value::Timestamp(ts) => {
            use chrono::{TimeZone, Utc};
            let dt = Utc
                .timestamp_millis_opt(ts)
                .single()
                .ok_or_else(|| anyhow!("timestamp out of range: {}", ts))?;
            Expr::TypedString {
                data_type: SqlDataType::Timestamp(None, TimezoneInfo::WithTimeZone),
                value: dt.to_rfc3339(),
            }
        }
        Value::Date(days) => {
            use chrono::NaiveDate;
            let date = NaiveDate::from_num_days_from_ce_opt(days + 719163).unwrap_or_default();
            Expr::Value(SqlValue::SingleQuotedString(
                date.format("%Y-%m-%d").to_string(),
            ))
        }
        Value::Time(t) => Expr::Value(SqlValue::SingleQuotedString(format!("{}", t))),
        Value::Interval(iv) => Expr::Value(SqlValue::SingleQuotedString(format!(
            "{} months {} ms",
            iv.months, iv.millis
        ))),
        Value::Uuid(bytes) => {
            let uuid = uuid::Uuid::from_bytes(bytes);
            Expr::Value(SqlValue::SingleQuotedString(uuid.to_string()))
        }
        Value::Json(s) | Value::Jsonb(s) => Expr::Value(SqlValue::SingleQuotedString(s)),
        Value::Array(arr) => {
            let elements: Vec<Expr> = arr
                .into_iter()
                .map(|v| value_to_expr(v, None))
                .collect::<Result<_>>()?;
            Expr::Array(sqlparser::ast::Array {
                elem: elements,
                named: false,
            })
        }
        Value::Vector(v) => Expr::Value(SqlValue::SingleQuotedString(format!(
            "[{}]",
            v.iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ))),
        Value::Tsvector(s) | Value::Tsquery(s) => Expr::Value(SqlValue::SingleQuotedString(s)),
    })
}

impl Executor {
    pub(crate) async fn execute_insert(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        table_name: &ObjectName,
        columns: &[Ident],
        source: &Option<Box<Query>>,
        returning: &Option<Vec<SelectItem>>,
        on_conflict: &Option<OnInsert>,
    ) -> Result<ExecuteResult> {
        let resolved = names::resolve_existing_table_name(
            self.store().as_ref(),
            txn,
            db_id,
            table_name,
            search_path,
        )
        .await?
        .ok_or_else(|| anyhow!("Table '{}' does not exist", table_name))?;
        let t = resolved.full;
        let schema = self
            .store()
            .get_schema(txn, db_id, &t)
            .await?
            .ok_or_else(|| anyhow!("Table '{}' does not exist", t))?;
        let enum_cache = dml::build_enum_label_cache(&self.store(), txn, db_id, &schema).await?;
        let trigger_defs = self.store().list_triggers_for_table(txn, db_id, &t).await?;
        let trigger_func_cache_insert = triggers::prefetch_trigger_functions(
            &self.store(),
            txn,
            db_id,
            &trigger_defs,
            "INSERT",
        )
        .await?;
        let trigger_func_cache_update = triggers::prefetch_trigger_functions(
            &self.store(),
            txn,
            db_id,
            &trigger_defs,
            "UPDATE",
        )
        .await?;

        let mut affected = 0;
        let mut inserted = 0usize;
        let mut ret_rows = Vec::new();
        let ret_cols = dml::build_returning_columns(returning, &schema)?;

        let source_rows: Vec<Vec<Expr>> = match source.as_ref() {
            None => {
                // sqlparser represents INSERT ... DEFAULT VALUES with source=None.
                let count = if columns.is_empty() {
                    schema.columns.len()
                } else {
                    columns.len()
                };
                let defaults = (0..count)
                    .map(|_| Expr::Identifier(Ident::new("DEFAULT")))
                    .collect();
                vec![defaults]
            }
            Some(source) => match &*source.body {
                SetExpr::Values(Values { rows, .. }) => rows.clone(),
                SetExpr::Select(_) => {
                    let select_result = self
                        .execute_query(txn, db_id, sequence_values, search_path, source)
                        .await?;
                    match select_result {
                        super::super::ExecuteResult::Select {
                            rows,
                            columns: select_cols,
                            ..
                        } => {
                            let insert_columns: Vec<String> = if select_cols.is_empty() {
                                schema.columns.iter().map(|c| c.name.clone()).collect()
                            } else {
                                select_cols.iter().map(|c| c.to_lowercase()).collect()
                            };

                            rows.into_iter()
                                .map(|row| {
                                    row.values
                                        .into_iter()
                                        .enumerate()
                                        .map(|(i, val)| {
                                            value_to_expr(
                                                val,
                                                insert_columns.get(i).map(|s| s.as_str()),
                                            )
                                        })
                                        .collect::<Result<Vec<Expr>>>()
                                })
                                .collect::<Result<Vec<Vec<Expr>>>>()?
                        }
                        _ => return Err(anyhow!("INSERT...SELECT source must return rows")),
                    }
                }
                _ => return Err(anyhow!("INSERT source must be VALUES or SELECT")),
            },
        };

        for exprs in &source_rows {
            let (mut row_vals, indices) = dml::prepare_insert_row(
                &self.store(),
                txn,
                db_id,
                sequence_values,
                search_path,
                &schema,
                columns,
                exprs,
            )
            .await?;
            dml::fill_missing_columns(
                &self.store(),
                txn,
                db_id,
                sequence_values,
                search_path,
                &schema,
                &mut row_vals,
                &indices,
            )
            .await?;
            dml::coerce_row_values_allow_null(&schema, &mut row_vals)?;
            let row = Row::new(row_vals);

            let row = match triggers::apply_before_triggers_with_cache(
                &self.store(),
                txn,
                db_id,
                sequence_values,
                search_path,
                &trigger_defs,
                &trigger_func_cache_insert,
                &schema,
                "INSERT",
                row,
                None,
            )
            .await?
            {
                Some(r) => r,
                None => continue,
            };

            let mut final_vals = row.values;
            dml::coerce_row_values(&schema, &mut final_vals)?;
            let row = Row::new(final_vals);
            dml::validate_check_constraints(&schema, &row)?;

            let result = dml::execute_insert_row(
                &self.store(),
                txn,
                db_id,
                &t,
                &schema,
                row,
                on_conflict,
                &enum_cache,
            )
            .await?;
            match result {
                dml::InsertRowResult::Inserted(final_row) => {
                    trigger_worker::enqueue_after_triggers(
                        txn,
                        db_id,
                        self.tenant_keyspace(),
                        &t,
                        TriggerOp::Insert,
                        None,
                        Some(&final_row),
                        &trigger_defs,
                        &self.store(),
                        self,
                        sequence_values,
                        search_path,
                    )
                    .await?;
                    affected += 1;
                    inserted += 1;
                    if let Some(ret_row) = dml::eval_returning_row(
                        &self.store(),
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        returning,
                        &final_row,
                        &schema,
                    )
                    .await?
                    {
                        ret_rows.push(ret_row);
                    }
                }
                dml::InsertRowResult::Skipped => continue,
                dml::InsertRowResult::Conflicted {
                    existing_pk,
                    existing_row,
                    excluded_row,
                } => {
                    let assignments = match on_conflict {
                        Some(OnInsert::OnConflict(oc)) => match &oc.action {
                            sqlparser::ast::OnConflictAction::DoUpdate(do_update) => {
                                do_update.assignments.as_slice()
                            }
                            _ => {
                                return Err(anyhow!(
                                    "Conflicted insert returned from non-DO UPDATE ON CONFLICT"
                                ));
                            }
                        },
                        Some(OnInsert::DuplicateKeyUpdate(assignments)) => assignments.as_slice(),
                        _ => {
                            return Err(anyhow!(
                                "Conflicted insert returned without ON CONFLICT DO UPDATE"
                            ));
                        }
                    };

                    let mut updated_vals = existing_row.values.clone();
                    for assignment in assignments {
                        let col_name = assignment.id.last().unwrap().value.clone();
                        let col_idx = schema
                            .column_index(&col_name)
                            .ok_or_else(|| anyhow!("Unknown column in DO UPDATE: {}", col_name))?;
                        let raw_val = dml::eval_upsert_expr(
                            &assignment.value,
                            &existing_row,
                            &excluded_row,
                            &schema,
                            schema.columns.get(col_idx),
                        )?;
                        let col = &schema.columns[col_idx];
                        updated_vals[col_idx] =
                            super::super::value_coercion::coerce_value_for_column(raw_val, col)?;
                    }
                    let updated_row = Row::new(updated_vals);

                    let updated_row = match triggers::apply_before_triggers_with_cache(
                        &self.store(),
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &trigger_defs,
                        &trigger_func_cache_update,
                        &schema,
                        "UPDATE",
                        updated_row,
                        Some(&existing_row),
                    )
                    .await?
                    {
                        Some(row) => row,
                        None => continue,
                    };

                    let mut final_vals = updated_row.values;
                    dml::coerce_row_values(&schema, &mut final_vals)?;
                    let updated_row = Row::new(final_vals);
                    dml::validate_check_constraints(&schema, &updated_row)?;

                    let updated_row = if schema.pk_indices.is_empty() {
                        dml::execute_update_row_by_pk(
                            &self.store(),
                            txn,
                            db_id,
                            &t,
                            &schema,
                            &existing_pk,
                            &existing_row,
                            updated_row,
                            &enum_cache,
                        )
                        .await?
                    } else {
                        dml::execute_update_row(
                            &self.store(),
                            txn,
                            db_id,
                            &t,
                            &schema,
                            &existing_row,
                            updated_row,
                            &enum_cache,
                        )
                        .await?
                    };

                    trigger_worker::enqueue_after_triggers(
                        txn,
                        db_id,
                        self.tenant_keyspace(),
                        &t,
                        TriggerOp::Update,
                        Some(&existing_row),
                        Some(&updated_row),
                        &trigger_defs,
                        &self.store(),
                        self,
                        sequence_values,
                        search_path,
                    )
                    .await?;

                    affected += 1;
                    if let Some(ret_row) = dml::eval_returning_row(
                        &self.store(),
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        returning,
                        &updated_row,
                        &schema,
                    )
                    .await?
                    {
                        ret_rows.push(ret_row);
                    }
                }
            }
        }

        if inserted > 0 {
            crate::sql::stats::bump_row_count_estimate(db_id, schema.table_id, inserted as isize);
        }

        if returning.is_some() {
            let column_types = Some(dml::build_returning_types(returning, &schema)?);
            Ok(ExecuteResult::Select {
                column_types,
                columns: ret_cols,
                rows: ret_rows,
                timezone: crate::session_context::current_timezone(),
            })
        } else {
            Ok(ExecuteResult::Insert {
                affected_rows: affected,
            })
        }
    }

    pub(crate) async fn execute_delete(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        from: &[sqlparser::ast::TableWithJoins],
        using: &[sqlparser::ast::TableWithJoins],
        selection: &Option<Expr>,
        returning: &Option<Vec<SelectItem>>,
    ) -> Result<ExecuteResult> {
        let ctes_ctx: HashMap<String, (TableSchema, Vec<Row>)> = HashMap::new();
        let (resolved_target, table_alias) = match &from[0].relation {
            sqlparser::ast::TableFactor::Table { name, alias, .. } => {
                let resolved = names::resolve_existing_table_name(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    name,
                    search_path,
                )
                .await?
                .ok_or_else(|| anyhow!("Table '{}' does not exist", name))?;
                let alias = alias
                    .as_ref()
                    .map(|a| normalize_ident(&a.name))
                    .unwrap_or_else(|| resolved.name.clone());
                (resolved, alias)
            }
            _ => return Err(SqlError::Unsupported("Unsupported DELETE target".into()).into()),
        };
        let t = resolved_target.full.clone();
        let schema = self
            .store()
            .get_schema(txn, db_id, &t)
            .await?
            .ok_or_else(|| SqlError::RelationNotFound(t.clone()))?;
        let trigger_defs = self.store().list_triggers_for_table(txn, db_id, &t).await?;
        if schema.pk_indices.is_empty() {
            return Err(anyhow!("No PK"));
        }
        let resolved_selection = if let Some(sel) = selection {
            Some(
                self.resolve_subqueries(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    sel,
                    &ctes_ctx,
                    &[],
                )
                .await?,
            )
        } else {
            None
        };
        let rows = self.scan_and_fill(txn, db_id, &t, &schema).await?;
        let mut cnt = 0;
        let mut ret_rows = Vec::new();
        let ret_cols = dml::build_returning_columns(returning, &schema)?;
        let using_data = if using.is_empty() {
            None
        } else {
            if using.len() != 1 {
                return Err(SqlError::Unsupported(
                    "DELETE ... USING multiple tables not supported".into(),
                )
                .into());
            }
            let using_table = &using[0];
            let (using_resolved, using_alias) = match &using_table.relation {
                sqlparser::ast::TableFactor::Table { name, alias, .. } => {
                    let resolved = names::resolve_existing_table_name(
                        self.store().as_ref(),
                        txn,
                        db_id,
                        name,
                        search_path,
                    )
                    .await?
                    .ok_or_else(|| SqlError::RelationNotFound(name.to_string()))?;
                    let alias = alias
                        .as_ref()
                        .map(|a| normalize_ident(&a.name))
                        .unwrap_or_else(|| resolved.name.clone());
                    (resolved, alias)
                }
                _ => return Err(SqlError::Unsupported("Unsupported USING table".into()).into()),
            };
            let using_schema = self
                .store()
                .get_schema(txn, db_id, &using_resolved.full)
                .await?
                .ok_or_else(|| SqlError::RelationNotFound(using_resolved.full.clone()))?;
            let using_rows = self
                .scan_and_fill(txn, db_id, &using_resolved.full, &using_schema)
                .await?;
            Some((using_schema, using_rows, using_alias))
        };

        let validation_schema = using_data.as_ref().map(|(using_schema, _, using_alias)| {
            build_type_infer_schema_for_two_table_join(
                &table_alias,
                &schema,
                using_alias,
                using_schema,
            )
        });
        if let Some(sel) = resolved_selection.as_ref() {
            match validation_schema.as_ref() {
                Some(schema) => {
                    validate_bool_expr_in_boolean_context(sel, schema, BOOLEAN_CONTEXT_ERR_MSG)?
                }
                None => {
                    validate_bool_expr_in_boolean_context(sel, &schema, BOOLEAN_CONTEXT_ERR_MSG)?
                }
            }
        }

        for r in rows {
            let should_delete = if let Some(ref e) = resolved_selection {
                if let Some((ref using_schema, ref using_rows, ref using_alias)) = using_data {
                    let mut matched = false;
                    for using_row in using_rows {
                        let (combined_schema, combined_row, column_offsets) =
                            dml::build_update_join_context(
                                &schema,
                                &table_alias,
                                using_schema,
                                using_alias,
                                &r,
                                using_row,
                            );
                        let ctx = JoinEvalContext::new(
                            &column_offsets,
                            None,
                            &combined_row,
                            &combined_schema,
                        );
                        let value = self
                            .eval_expr_join_maybe_sequence(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                e,
                                &ctx,
                            )
                            .await?;
                        if predicate_value_to_bool(e, value)? {
                            matched = true;
                            break;
                        }
                    }
                    matched
                } else {
                    let value = self
                        .eval_expr_maybe_sequence(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            e,
                            Some(&r),
                            Some(&schema),
                        )
                        .await?;
                    predicate_value_to_bool(e, value)?
                }
            } else {
                if let Some((_, ref using_rows, _)) = using_data {
                    !using_rows.is_empty()
                } else {
                    true
                }
            };

            if !should_delete {
                continue;
            }
            if let Some(ret_row) = dml::eval_returning_row(
                &self.store(),
                txn,
                db_id,
                sequence_values,
                search_path,
                returning,
                &r,
                &schema,
            )
            .await?
            {
                ret_rows.push(ret_row);
            }
            dml::execute_delete_row(&self.store(), txn, db_id, &t, &schema, &r).await?;
            trigger_worker::enqueue_after_triggers(
                txn,
                db_id,
                self.tenant_keyspace(),
                &t,
                TriggerOp::Delete,
                Some(&r),
                None,
                &trigger_defs,
                &self.store(),
                self,
                sequence_values,
                search_path,
            )
            .await?;
            cnt += 1;
        }

        if returning.is_some() {
            let column_types = Some(dml::build_returning_types(returning, &schema)?);
            Ok(ExecuteResult::Select {
                column_types,
                columns: ret_cols,
                rows: ret_rows,
                timezone: crate::session_context::current_timezone(),
            })
        } else {
            Ok(ExecuteResult::Delete { affected_rows: cnt })
        }
    }

    pub(crate) async fn execute_update(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        table: &sqlparser::ast::TableWithJoins,
        assignments: &[Assignment],
        from: &Option<sqlparser::ast::TableWithJoins>,
        selection: &Option<Expr>,
        returning: &Option<Vec<SelectItem>>,
    ) -> Result<ExecuteResult> {
        let ctes_ctx: HashMap<String, (TableSchema, Vec<Row>)> = HashMap::new();
        let resolved_target = match &table.relation {
            sqlparser::ast::TableFactor::Table { name, .. } => names::resolve_existing_table_name(
                self.store().as_ref(),
                txn,
                db_id,
                name,
                search_path,
            )
            .await?
            .ok_or_else(|| anyhow!("Table '{}' does not exist", name))?,
            _ => return Err(SqlError::Unsupported("Unsupported".into()).into()),
        };
        let t = resolved_target.full.clone();
        let table_alias = match &table.relation {
            sqlparser::ast::TableFactor::Table { alias, .. } => alias
                .as_ref()
                .map(|a| normalize_ident(&a.name))
                .unwrap_or_else(|| resolved_target.name.clone()),
            _ => resolved_target.name.clone(),
        };
        let schema = self
            .store()
            .get_schema(txn, db_id, &t)
            .await?
            .ok_or_else(|| SqlError::RelationNotFound(t.clone()))?;
        let enum_cache = dml::build_enum_label_cache(&self.store(), txn, db_id, &schema).await?;
        let trigger_defs = self.store().list_triggers_for_table(txn, db_id, &t).await?;
        let trigger_func_cache_update = triggers::prefetch_trigger_functions(
            &self.store(),
            txn,
            db_id,
            &trigger_defs,
            "UPDATE",
        )
        .await?;
        if schema.pk_indices.is_empty() {
            return Err(anyhow!("No PK"));
        }
        let resolved_selection = if let Some(sel) = selection {
            Some(
                self.resolve_subqueries(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    sel,
                    &ctes_ctx,
                    &[],
                )
                .await?,
            )
        } else {
            None
        };
        let update_info = dml::validate_update_columns(&schema, assignments)?;
        let indices = &update_info.indices;

        let (from_schema, from_rows, from_alias) = if let Some(from_table) = from {
            let from_resolved = match &from_table.relation {
                sqlparser::ast::TableFactor::Table { name, .. } => {
                    names::resolve_existing_table_name(
                        self.store().as_ref(),
                        txn,
                        db_id,
                        name,
                        search_path,
                    )
                    .await?
                    .ok_or_else(|| SqlError::RelationNotFound(name.to_string()))?
                }
                _ => return Err(SqlError::Unsupported("Unsupported FROM table".into()).into()),
            };
            let from_name = from_resolved.full.clone();
            let from_alias_str = match &from_table.relation {
                sqlparser::ast::TableFactor::Table { alias, .. } => alias
                    .as_ref()
                    .map(|a| normalize_ident(&a.name))
                    .unwrap_or_else(|| from_resolved.name.clone()),
                _ => from_name.clone(),
            };
            let fs = self
                .store()
                .get_schema(txn, db_id, &from_name)
                .await?
                .ok_or_else(|| SqlError::RelationNotFound(from_name.clone()))?;
            let fr = self.scan_and_fill(txn, db_id, &from_name, &fs).await?;
            (Some(fs), Some(fr), Some(from_alias_str))
        } else {
            (None, None, None)
        };

        let validation_schema = match (&from_schema, &from_alias) {
            (Some(from_schema), Some(from_alias)) => {
                Some(build_type_infer_schema_for_two_table_join(
                    &table_alias,
                    &schema,
                    from_alias,
                    from_schema,
                ))
            }
            _ => None,
        };
        if let Some(sel) = resolved_selection.as_ref() {
            match validation_schema.as_ref() {
                Some(schema) => {
                    validate_bool_expr_in_boolean_context(sel, schema, BOOLEAN_CONTEXT_ERR_MSG)?
                }
                None => {
                    validate_bool_expr_in_boolean_context(sel, &schema, BOOLEAN_CONTEXT_ERR_MSG)?
                }
            }
        }

        let rows = self.scan_and_fill(txn, db_id, &t, &schema).await?;
        let mut cnt = 0;
        let mut ret_rows = Vec::new();
        let ret_cols = dml::build_returning_columns(returning, &schema)?;

        for r in &rows {
            let matching_from_rows: Vec<&Row> = if let (Some(ref fs), Some(ref fr), Some(ref fa)) =
                (&from_schema, &from_rows, &from_alias)
            {
                if fr.is_empty() {
                    Vec::new()
                } else {
                    let (combined_schema, _, column_offsets) =
                        dml::build_update_join_context(&schema, &table_alias, fs, fa, r, &fr[0]);

                    let mut matches = Vec::new();
                    for from_row in fr {
                        if let Some(ref sel) = resolved_selection {
                            let mut combined_values = r.values.clone();
                            combined_values.extend(from_row.values.clone());
                            let combined_row = Row::new(combined_values);
                            let ctx = JoinEvalContext::new(
                                &column_offsets,
                                None,
                                &combined_row,
                                &combined_schema,
                            );
                            let value = self
                                .eval_expr_join_maybe_sequence(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    sel,
                                    &ctx,
                                )
                                .await?;
                            if predicate_value_to_bool(sel, value)? {
                                matches.push(from_row);
                            }
                        } else {
                            matches.push(from_row);
                        }
                    }
                    matches
                }
            } else {
                if let Some(ref e) = resolved_selection {
                    let value = self
                        .eval_expr_maybe_sequence(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            e,
                            Some(r),
                            Some(&schema),
                        )
                        .await?;
                    if !predicate_value_to_bool(e, value)? {
                        continue;
                    }
                }
                vec![r]
            };

            if matching_from_rows.is_empty() && from.is_some() {
                continue;
            }

            let new_vals = if let (Some(ref fs), Some(ref fa)) = (&from_schema, &from_alias) {
                if let Some(first_from) = matching_from_rows.first() {
                    let (combined_schema, combined_row, _) = dml::build_update_join_context(
                        &schema,
                        &table_alias,
                        fs,
                        fa,
                        r,
                        first_from,
                    );
                    dml::compute_update_values(
                        &self.store(),
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &schema,
                        r,
                        assignments,
                        &indices,
                        Some((&combined_row, &combined_schema)),
                    )
                    .await?
                } else {
                    dml::compute_update_values(
                        &self.store(),
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &schema,
                        r,
                        assignments,
                        &indices,
                        None,
                    )
                    .await?
                }
            } else {
                dml::compute_update_values(
                    &self.store(),
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &schema,
                    r,
                    assignments,
                    &indices,
                    None,
                )
                .await?
            };

            let new_row = Row::new(new_vals);
            let new_row = match triggers::apply_before_triggers_with_cache(
                &self.store(),
                txn,
                db_id,
                sequence_values,
                search_path,
                &trigger_defs,
                &trigger_func_cache_update,
                &schema,
                "UPDATE",
                new_row,
                Some(r),
            )
            .await?
            {
                Some(row) => row,
                None => continue,
            };

            let mut final_vals = new_row.values;
            dml::coerce_row_values(&schema, &mut final_vals)?;
            let new_row = Row::new(final_vals);
            dml::validate_check_constraints(&schema, &new_row)?;

            let updated_row = dml::execute_update_row(
                &self.store(),
                txn,
                db_id,
                &t,
                &schema,
                r,
                new_row,
                &enum_cache,
            )
            .await?;

            trigger_worker::enqueue_after_triggers(
                txn,
                db_id,
                self.tenant_keyspace(),
                &t,
                TriggerOp::Update,
                Some(r),
                Some(&updated_row),
                &trigger_defs,
                &self.store(),
                self,
                sequence_values,
                search_path,
            )
            .await?;

            if let Some(ret_row) = dml::eval_returning_row(
                &self.store(),
                txn,
                db_id,
                sequence_values,
                search_path,
                returning,
                &updated_row,
                &schema,
            )
            .await?
            {
                ret_rows.push(ret_row);
            }
            cnt += 1;
        }

        if returning.is_some() {
            let column_types = Some(dml::build_returning_types(returning, &schema)?);

            Ok(ExecuteResult::Select {
                column_types,
                columns: ret_cols,
                rows: ret_rows,
                timezone: crate::session_context::current_timezone(),
            })
        } else {
            Ok(ExecuteResult::Update { affected_rows: cnt })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_to_expr_roundtrips_bytes() {
        let expr = value_to_expr(Value::Bytes(vec![0, 1, 2, 255]), None).unwrap();
        let val = crate::sql::expr::eval_expr(&expr, None, None).unwrap();
        assert_eq!(val, Value::Bytes(vec![0, 1, 2, 255]));
    }

    #[test]
    fn value_to_expr_roundtrips_timestamp() {
        let expr = value_to_expr(Value::Timestamp(0), None).unwrap();
        let val = crate::sql::expr::eval_expr(&expr, None, None).unwrap();
        assert_eq!(val, Value::Timestamp(0));
    }
}
