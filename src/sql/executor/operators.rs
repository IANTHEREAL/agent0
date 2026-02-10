use std::collections::HashMap;
use std::sync::OnceLock;

use crate::sql::error::SqlError;
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    Distinct, Expr, Function, FunctionArg, FunctionArgExpr, GroupByExpr, OrderByExpr, Query,
    SelectItem, UnaryOperator, Value as SqlValue,
};
use tikv_client::Transaction;

use super::super::expr::{coerce_text_literal_to_bool, eval_expr};
use super::super::operators::{
    execute_operator_tree, execute_operator_tree_with_ctes, AggregateExpr, BoxedOperator,
    DistinctOnOperator, DistinctOperator, FilterOperator, HashAggregateOperator, HashJoinConfig,
    HashJoinOperator, HashJoinType, JoinType, LimitOperator, NestedLoopJoinOperator,
    PhysicalPlanner, ProjectOperator, SortOperator, TableScanOperator, WindowFunctionExpr,
    WindowOperator,
};
use super::super::planner::{choose_join_algorithm, JoinAlgorithmChoice};
use super::super::projection::{get_expr_name, get_select_item_name, infer_expr_type};
use super::super::ExecuteResult;
use super::core::Executor;
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};
use sqlparser::ast::Ident;

pub(crate) fn rewrite_expr_for_multi_join(
    expr: &Expr,
    table_aliases: &[(String, TableSchema)],
) -> Result<Expr> {
    match expr {
        Expr::Identifier(ident) => {
            let col_name = &ident.value;
            let col_lower = col_name.to_lowercase();
            let mut found: Option<&str> = None;
            for (alias, schema) in table_aliases {
                if schema.column_index(col_name).is_some()
                    || schema
                        .columns
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(col_name))
                {
                    if found.is_some() {
                        return Err(SqlError::AmbiguousColumn(col_name.to_string()).into());
                    }
                    found = Some(alias);
                }
                if schema.columns.iter().any(|c| {
                    c.name.to_lowercase() == format!("{}.{}", alias.to_lowercase(), col_lower)
                }) {
                    if found.is_some() {
                        return Err(SqlError::AmbiguousColumn(col_name.to_string()).into());
                    }
                    found = Some(alias);
                }
            }
            if let Some(alias) = found {
                Ok(Expr::CompoundIdentifier(vec![
                    Ident::new(alias),
                    Ident::new(col_name.clone()),
                ]))
            } else {
                Ok(expr.clone())
            }
        }
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            let table_ref = parts[0].value.to_lowercase();
            let col_name = &parts[1].value;
            for (alias, schema) in table_aliases {
                let alias_lower = alias.to_lowercase();
                let table_lower = schema.name.to_lowercase();
                if table_ref == alias_lower || table_ref == table_lower {
                    return Ok(Expr::CompoundIdentifier(vec![
                        Ident::new(alias.clone()),
                        Ident::new(col_name.clone()),
                    ]));
                }
            }
            Ok(expr.clone())
        }
        Expr::BinaryOp { left, op, right } => Ok(Expr::BinaryOp {
            left: Box::new(rewrite_expr_for_multi_join(left, table_aliases)?),
            op: op.clone(),
            right: Box::new(rewrite_expr_for_multi_join(right, table_aliases)?),
        }),
        Expr::UnaryOp { op, expr: inner } => Ok(Expr::UnaryOp {
            op: op.clone(),
            expr: Box::new(rewrite_expr_for_multi_join(inner, table_aliases)?),
        }),
        Expr::Nested(inner) => Ok(Expr::Nested(Box::new(rewrite_expr_for_multi_join(
            inner,
            table_aliases,
        )?))),
        Expr::IsNull(inner) => Ok(Expr::IsNull(Box::new(rewrite_expr_for_multi_join(
            inner,
            table_aliases,
        )?))),
        Expr::IsNotNull(inner) => Ok(Expr::IsNotNull(Box::new(rewrite_expr_for_multi_join(
            inner,
            table_aliases,
        )?))),
        Expr::Function(func) => {
            let mut new_args = Vec::with_capacity(func.args.len());
            for arg in &func.args {
                match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                        new_args.push(FunctionArg::Unnamed(FunctionArgExpr::Expr(
                            rewrite_expr_for_multi_join(e, table_aliases)?,
                        )));
                    }
                    other => new_args.push(other.clone()),
                }
            }
            Ok(Expr::Function(Function {
                args: new_args,
                ..func.clone()
            }))
        }
        Expr::Cast {
            expr: inner,
            data_type,
            format,
        } => Ok(Expr::Cast {
            expr: Box::new(rewrite_expr_for_multi_join(inner, table_aliases)?),
            data_type: data_type.clone(),
            format: format.clone(),
        }),
        Expr::InList {
            expr: inner,
            list,
            negated,
        } => Ok(Expr::InList {
            expr: Box::new(rewrite_expr_for_multi_join(inner, table_aliases)?),
            list: list
                .iter()
                .map(|e| rewrite_expr_for_multi_join(e, table_aliases))
                .collect::<Result<Vec<_>>>()?,
            negated: *negated,
        }),
        Expr::Between {
            expr: inner,
            negated,
            low,
            high,
        } => Ok(Expr::Between {
            expr: Box::new(rewrite_expr_for_multi_join(inner, table_aliases)?),
            negated: *negated,
            low: Box::new(rewrite_expr_for_multi_join(low, table_aliases)?),
            high: Box::new(rewrite_expr_for_multi_join(high, table_aliases)?),
        }),
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => Ok(Expr::Case {
            operand: operand
                .as_ref()
                .map(|o| rewrite_expr_for_multi_join(o, table_aliases))
                .transpose()?
                .map(Box::new),
            conditions: conditions
                .iter()
                .map(|c| rewrite_expr_for_multi_join(c, table_aliases))
                .collect::<Result<Vec<_>>>()?,
            results: results
                .iter()
                .map(|r| rewrite_expr_for_multi_join(r, table_aliases))
                .collect::<Result<Vec<_>>>()?,
            else_result: else_result
                .as_ref()
                .map(|e| rewrite_expr_for_multi_join(e, table_aliases))
                .transpose()?
                .map(Box::new),
        }),
        Expr::Like {
            negated,
            expr: inner,
            pattern,
            escape_char,
        } => Ok(Expr::Like {
            negated: *negated,
            expr: Box::new(rewrite_expr_for_multi_join(inner, table_aliases)?),
            pattern: Box::new(rewrite_expr_for_multi_join(pattern, table_aliases)?),
            escape_char: *escape_char,
        }),
        Expr::ILike {
            negated,
            expr: inner,
            pattern,
            escape_char,
        } => Ok(Expr::ILike {
            negated: *negated,
            expr: Box::new(rewrite_expr_for_multi_join(inner, table_aliases)?),
            pattern: Box::new(rewrite_expr_for_multi_join(pattern, table_aliases)?),
            escape_char: *escape_char,
        }),
        Expr::IsTrue(inner) => Ok(Expr::IsTrue(Box::new(rewrite_expr_for_multi_join(
            inner,
            table_aliases,
        )?))),
        Expr::IsFalse(inner) => Ok(Expr::IsFalse(Box::new(rewrite_expr_for_multi_join(
            inner,
            table_aliases,
        )?))),
        Expr::IsNotTrue(inner) => Ok(Expr::IsNotTrue(Box::new(rewrite_expr_for_multi_join(
            inner,
            table_aliases,
        )?))),
        Expr::IsNotFalse(inner) => Ok(Expr::IsNotFalse(Box::new(rewrite_expr_for_multi_join(
            inner,
            table_aliases,
        )?))),
        Expr::TryCast {
            expr: inner,
            data_type,
            format,
        } => Ok(Expr::TryCast {
            expr: Box::new(rewrite_expr_for_multi_join(inner, table_aliases)?),
            data_type: data_type.clone(),
            format: format.clone(),
        }),
        Expr::Subquery(_) => Ok(expr.clone()),
        Expr::Exists { .. } => Ok(expr.clone()),
        _ => Ok(expr.clone()),
    }
}

fn rewrite_join_expr_with_aliases(
    expr: &Expr,
    left_alias: &str,
    right_alias: &str,
    left_schema: &TableSchema,
    right_schema: &TableSchema,
) -> Result<Expr> {
    match expr {
        Expr::Identifier(ident) => {
            let col_name = &ident.value;
            let in_left = left_schema.column_index(col_name).is_some();
            let in_right = right_schema.column_index(col_name).is_some();

            if in_left && in_right {
                return Err(SqlError::AmbiguousColumn(col_name.to_string()).into());
            }

            if in_left {
                Ok(Expr::CompoundIdentifier(vec![
                    Ident::new(left_alias),
                    Ident::new(col_name.clone()),
                ]))
            } else if in_right {
                Ok(Expr::CompoundIdentifier(vec![
                    Ident::new(right_alias),
                    Ident::new(col_name.clone()),
                ]))
            } else {
                Ok(expr.clone())
            }
        }
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            let table_ref = parts[0].value.to_lowercase();
            let col_name = &parts[1].value;

            let left_lower = left_alias.to_lowercase();
            let right_lower = right_alias.to_lowercase();
            let left_table_lower = left_schema.name.to_lowercase();
            let right_table_lower = right_schema.name.to_lowercase();

            if table_ref == left_lower || table_ref == left_table_lower {
                Ok(Expr::CompoundIdentifier(vec![
                    Ident::new(left_alias),
                    Ident::new(col_name.clone()),
                ]))
            } else if table_ref == right_lower || table_ref == right_table_lower {
                Ok(Expr::CompoundIdentifier(vec![
                    Ident::new(right_alias),
                    Ident::new(col_name.clone()),
                ]))
            } else {
                Ok(expr.clone())
            }
        }
        Expr::BinaryOp { left, op, right } => Ok(Expr::BinaryOp {
            left: Box::new(rewrite_join_expr_with_aliases(
                left,
                left_alias,
                right_alias,
                left_schema,
                right_schema,
            )?),
            op: op.clone(),
            right: Box::new(rewrite_join_expr_with_aliases(
                right,
                left_alias,
                right_alias,
                left_schema,
                right_schema,
            )?),
        }),
        Expr::UnaryOp { op, expr: inner } => Ok(Expr::UnaryOp {
            op: op.clone(),
            expr: Box::new(rewrite_join_expr_with_aliases(
                inner,
                left_alias,
                right_alias,
                left_schema,
                right_schema,
            )?),
        }),
        Expr::Nested(inner) => Ok(Expr::Nested(Box::new(rewrite_join_expr_with_aliases(
            inner,
            left_alias,
            right_alias,
            left_schema,
            right_schema,
        )?))),
        Expr::IsNull(inner) => Ok(Expr::IsNull(Box::new(rewrite_join_expr_with_aliases(
            inner,
            left_alias,
            right_alias,
            left_schema,
            right_schema,
        )?))),
        Expr::IsNotNull(inner) => Ok(Expr::IsNotNull(Box::new(rewrite_join_expr_with_aliases(
            inner,
            left_alias,
            right_alias,
            left_schema,
            right_schema,
        )?))),
        Expr::InList {
            expr: inner,
            list,
            negated,
        } => Ok(Expr::InList {
            expr: Box::new(rewrite_join_expr_with_aliases(
                inner,
                left_alias,
                right_alias,
                left_schema,
                right_schema,
            )?),
            list: list
                .iter()
                .map(|e| {
                    rewrite_join_expr_with_aliases(
                        e,
                        left_alias,
                        right_alias,
                        left_schema,
                        right_schema,
                    )
                })
                .collect::<Result<Vec<_>>>()?,
            negated: *negated,
        }),
        Expr::Between {
            expr: inner,
            negated,
            low,
            high,
        } => Ok(Expr::Between {
            expr: Box::new(rewrite_join_expr_with_aliases(
                inner,
                left_alias,
                right_alias,
                left_schema,
                right_schema,
            )?),
            negated: *negated,
            low: Box::new(rewrite_join_expr_with_aliases(
                low,
                left_alias,
                right_alias,
                left_schema,
                right_schema,
            )?),
            high: Box::new(rewrite_join_expr_with_aliases(
                high,
                left_alias,
                right_alias,
                left_schema,
                right_schema,
            )?),
        }),
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            let operand = match operand.as_ref() {
                Some(o) => Some(Box::new(rewrite_join_expr_with_aliases(
                    o,
                    left_alias,
                    right_alias,
                    left_schema,
                    right_schema,
                )?)),
                None => None,
            };

            let conditions = conditions
                .iter()
                .map(|c| {
                    rewrite_join_expr_with_aliases(
                        c,
                        left_alias,
                        right_alias,
                        left_schema,
                        right_schema,
                    )
                })
                .collect::<Result<Vec<_>>>()?;

            let results = results
                .iter()
                .map(|r| {
                    rewrite_join_expr_with_aliases(
                        r,
                        left_alias,
                        right_alias,
                        left_schema,
                        right_schema,
                    )
                })
                .collect::<Result<Vec<_>>>()?;

            let else_result = match else_result.as_ref() {
                Some(e) => Some(Box::new(rewrite_join_expr_with_aliases(
                    e,
                    left_alias,
                    right_alias,
                    left_schema,
                    right_schema,
                )?)),
                None => None,
            };

            Ok(Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            })
        }
        Expr::Function(f) => {
            let rewritten_args = f
                .args
                .clone()
                .into_iter()
                .map(|arg| match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Ok(FunctionArg::Unnamed(
                        FunctionArgExpr::Expr(rewrite_join_expr_with_aliases(
                            &e,
                            left_alias,
                            right_alias,
                            left_schema,
                            right_schema,
                        )?),
                    )),
                    other => Ok(other),
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Expr::Function(Function {
                name: f.name.clone(),
                args: rewritten_args,
                filter: f.filter.clone(),
                null_treatment: f.null_treatment.clone(),
                over: f.over.clone(),
                distinct: f.distinct,
                special: f.special,
                order_by: f.order_by.clone(),
            }))
        }
        Expr::Cast {
            expr: inner,
            data_type,
            format,
        } => Ok(Expr::Cast {
            expr: Box::new(rewrite_join_expr_with_aliases(
                inner,
                left_alias,
                right_alias,
                left_schema,
                right_schema,
            )?),
            data_type: data_type.clone(),
            format: format.clone(),
        }),
        _ => Ok(expr.clone()),
    }
}

pub(crate) fn eval_having_expr_for_operators(
    expr: &Expr,
    row: &Row,
    schema: &TableSchema,
    agg_exprs: &[AggregateExpr],
    group_by_count: usize,
) -> Result<Value> {
    match expr {
        Expr::BinaryOp { left, op, right } => {
            let left_val =
                eval_having_expr_for_operators(left, row, schema, agg_exprs, group_by_count)?;
            let right_val =
                eval_having_expr_for_operators(right, row, schema, agg_exprs, group_by_count)?;
            super::super::expr::eval_binary_op_public(left_val, op, right_val)
        }
        Expr::UnaryOp { op, expr: inner } => {
            let val =
                eval_having_expr_for_operators(inner, row, schema, agg_exprs, group_by_count)?;
            match op {
                UnaryOperator::Not => match val {
                    Value::Boolean(b) => Ok(Value::Boolean(!b)),
                    _ => Err(anyhow!("NOT requires boolean operand")),
                },
                UnaryOperator::Minus => match val {
                    Value::Int32(n) => Ok(Value::Int32(-n)),
                    Value::Int64(n) => Ok(Value::Int64(-n)),
                    Value::Float64(n) => Ok(Value::Float64(-n)),
                    _ => Err(anyhow!("Unary minus requires numeric operand")),
                },
                _ => Err(SqlError::Unsupported(format!(
                    "Unsupported unary operator in HAVING: {:?}",
                    op
                ))
                .into()),
            }
        }
        Expr::Nested(inner) => {
            eval_having_expr_for_operators(inner, row, schema, agg_exprs, group_by_count)
        }
        Expr::Function(f) => {
            let func_name = f
                .name
                .0
                .last()
                .map(|i| i.value.to_uppercase())
                .unwrap_or_default();

            if matches!(
                func_name.as_str(),
                "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" | "STRING_AGG" | "ARRAY_AGG"
            ) {
                if let Some(agg_idx) = find_matching_aggregate(f, agg_exprs) {
                    let col_idx = group_by_count + agg_idx;
                    return row
                        .values
                        .get(col_idx)
                        .cloned()
                        .ok_or_else(|| anyhow!("Aggregate column index out of bounds"));
                }
                return Err(anyhow!(
                    "Aggregate function {} in HAVING not found in SELECT",
                    func_name
                ));
            }
            eval_expr(expr, Some(row), Some(schema))
        }
        Expr::Identifier(id) => {
            if let Some(col_idx) = schema.columns.iter().position(|c| c.name == id.value) {
                row.values
                    .get(col_idx)
                    .cloned()
                    .ok_or_else(|| anyhow!("Column index out of bounds"))
            } else {
                Err(anyhow!("Column {} not found", id.value))
            }
        }
        Expr::Value(_) | Expr::TypedString { .. } => eval_expr(expr, Some(row), Some(schema)),
        Expr::Cast {
            expr: inner,
            data_type,
            format,
        } => {
            let inner_val =
                eval_having_expr_for_operators(inner, row, schema, agg_exprs, group_by_count)?;
            let cast_expr = Expr::Cast {
                expr: Box::new(super::super::value_coercion::value_to_sql_expr(&inner_val)),
                data_type: data_type.clone(),
                format: format.clone(),
            };
            eval_expr(&cast_expr, Some(row), Some(schema))
        }
        _ => eval_expr(expr, Some(row), Some(schema)),
    }
}

pub(crate) fn find_matching_aggregate(f: &Function, agg_exprs: &[AggregateExpr]) -> Option<usize> {
    let func_name = f
        .name
        .0
        .last()
        .map(|i| i.value.to_uppercase())
        .unwrap_or_default();

    let f_arg = f.args.first().and_then(|arg| match arg {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
        FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => None,
        _ => None,
    });

    let f_is_wildcard = matches!(
        f.args.first(),
        Some(FunctionArg::Unnamed(FunctionArgExpr::Wildcard))
    ) || f.args.is_empty();

    for (i, agg) in agg_exprs.iter().enumerate() {
        if agg.func_name != func_name {
            continue;
        }
        if agg.distinct != f.distinct {
            continue;
        }

        let agg_is_wildcard = agg.arg.is_none();
        if f_is_wildcard && agg_is_wildcard {
            return Some(i);
        }
        if let (Some(f_e), Some(agg_e)) = (f_arg, &agg.arg) {
            if format!("{}", f_e) == format!("{}", agg_e) {
                return Some(i);
            }
        }
    }
    None
}

pub fn use_operator_execution() -> bool {
    static USE_OPERATORS: OnceLock<bool> = OnceLock::new();
    *USE_OPERATORS.get_or_init(|| {
        std::env::var("PGTIKV_USE_OPERATORS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(true)
    })
}

pub fn extract_limit(query: &Query) -> Option<usize> {
    if let Some(limit_expr) = &query.limit {
        if let Ok(v) = eval_expr(limit_expr, None, None) {
            return match v {
                Value::Int64(n) if n >= 0 => Some(n as usize),
                Value::Int32(n) if n >= 0 => Some(n as usize),
                Value::Text(s) => s
                    .trim()
                    .parse::<i64>()
                    .ok()
                    .filter(|n| *n >= 0)
                    .map(|n| n as usize),
                _ => None,
            };
        }
    }
    if let Some(fetch) = &query.fetch {
        if let Some(quantity) = &fetch.quantity {
            if let Ok(v) = eval_expr(quantity, None, None) {
                return match v {
                    Value::Int64(n) if n >= 0 => Some(n as usize),
                    Value::Int32(n) if n >= 0 => Some(n as usize),
                    Value::Text(s) => s
                        .trim()
                        .parse::<i64>()
                        .ok()
                        .filter(|n| *n >= 0)
                        .map(|n| n as usize)
                        .or(Some(1)),
                    _ => Some(1),
                };
            }
        }
        return Some(1);
    }
    None
}

pub fn extract_offset(query: &Query) -> usize {
    if let Some(offset) = &query.offset {
        if let Ok(v) = eval_expr(&offset.value, None, None) {
            return match v {
                Value::Int64(n) if n >= 0 => n as usize,
                Value::Int32(n) if n >= 0 => n as usize,
                Value::Text(s) => s
                    .trim()
                    .parse::<i64>()
                    .ok()
                    .filter(|n| *n >= 0)
                    .map(|n| n as usize)
                    .unwrap_or(0),
                _ => 0,
            };
        }
    }
    0
}

static AGGREGATE_FUNC_NAMES: &[&str] = &[
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "STRING_AGG",
    "ARRAY_AGG",
    "BOOL_AND",
    "BOOL_OR",
    "EVERY",
];

pub(crate) fn is_aggregate_func(f: &Function) -> bool {
    let name = f
        .name
        .0
        .last()
        .map(|n| n.value.to_uppercase())
        .unwrap_or_default();
    AGGREGATE_FUNC_NAMES.contains(&name.as_str())
}

static EVAL_FUNCTION_MATCH_NAMES: &[&str] = &[
    "NULLIF",
    "GREATEST",
    "LEAST",
    "GET_BIT",
    "SET_BIT",
    "INT8SEND",
    "INT4SEND",
    "UUID_SEND",
    "SUBSTR",
    "FORMAT",
    "NOW",
    "CURRENT_TIMESTAMP",
    "CURRENT_DATE",
    "DATE_TRUNC",
    "DATE",
    "TO_CHAR",
    "AGE",
    "GENERATE_SERIES",
    "NEXTVAL",
    "CURRVAL",
    "SETVAL",
    "SET_CONFIG",
    "PG_BACKEND_PID",
    "VERSION",
    "CURRENT_DATABASE",
    "CURRENT_SCHEMA",
    "CURRENT_USER",
    "SESSION_USER",
    "USER",
    "PG_GET_USERBYID",
    "PG_GET_INDEXDEF",
    "PG_GET_CONSTRAINTDEF",
    "PG_GET_EXPR",
    "FORMAT_TYPE",
    "PG_CATALOG.SET_CONFIG",
    "L2_DISTANCE",
    "COSINE_DISTANCE",
    "INNER_PRODUCT",
    "VECTOR_DIMS",
    "VECTOR_NORM",
    "SUBSTRING",
    "POSITION",
    "OVERLAY",
    "COALESCE",
    "ROW_NUMBER",
    "RANK",
    "DENSE_RANK",
    "LAG",
    "LEAD",
    "FIRST_VALUE",
    "LAST_VALUE",
    "NTH_VALUE",
    "NTILE",
    "CUME_DIST",
    "PERCENT_RANK",
    "CURRENT_SETTING",
];

fn is_known_builtin_function(name: &str) -> bool {
    let upper = name.to_uppercase();
    if AGGREGATE_FUNC_NAMES.contains(&upper.as_str()) {
        return true;
    }
    if EVAL_FUNCTION_MATCH_NAMES.contains(&upper.as_str()) {
        return true;
    }
    crate::sql::expr::functions::get_registry().contains_key(upper.as_str())
}

fn expr_may_have_udf(expr: &Expr) -> bool {
    match expr {
        Expr::Function(f) => {
            let name = f.name.0.last().map(|n| n.value.clone()).unwrap_or_default();
            if !is_known_builtin_function(&name) {
                return true;
            }
            for arg in &f.args {
                if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(e),
                    ..
                } = arg
                {
                    if expr_may_have_udf(e) {
                        return true;
                    }
                }
            }
            false
        }
        Expr::BinaryOp { left, right, .. } => expr_may_have_udf(left) || expr_may_have_udf(right),
        Expr::UnaryOp { expr: inner, .. } | Expr::Nested(inner) => expr_may_have_udf(inner),
        Expr::Cast { expr: inner, .. } | Expr::TryCast { expr: inner, .. } => {
            expr_may_have_udf(inner)
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            operand.as_ref().map_or(false, |o| expr_may_have_udf(o))
                || conditions.iter().any(expr_may_have_udf)
                || results.iter().any(expr_may_have_udf)
                || else_result.as_ref().map_or(false, |e| expr_may_have_udf(e))
        }
        Expr::InList { expr: e, list, .. } => {
            expr_may_have_udf(e) || list.iter().any(expr_may_have_udf)
        }
        Expr::Between {
            expr, low, high, ..
        } => expr_may_have_udf(expr) || expr_may_have_udf(low) || expr_may_have_udf(high),
        Expr::IsNull(e) | Expr::IsNotNull(e) | Expr::IsTrue(e) | Expr::IsFalse(e) => {
            expr_may_have_udf(e)
        }
        _ => false,
    }
}

pub(crate) fn expr_may_have_udf_pub(expr: &Expr) -> bool {
    expr_may_have_udf(expr)
}

pub(crate) fn projection_may_have_udf(projection: &[SelectItem]) -> bool {
    projection.iter().any(|item| match item {
        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
            expr_may_have_udf(e)
        }
        _ => false,
    })
}

pub(crate) fn collect_nested_aggregates(expr: &Expr) -> Vec<&Function> {
    let mut result = Vec::new();
    collect_nested_aggregates_inner(expr, &mut result);
    result
}

fn collect_nested_aggregates_inner<'a>(expr: &'a Expr, out: &mut Vec<&'a Function>) {
    match expr {
        Expr::Function(f) if is_aggregate_func(f) => {
            out.push(f);
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_nested_aggregates_inner(left, out);
            collect_nested_aggregates_inner(right, out);
        }
        Expr::UnaryOp { expr: inner, .. } => {
            collect_nested_aggregates_inner(inner, out);
        }
        Expr::Nested(inner) => {
            collect_nested_aggregates_inner(inner, out);
        }
        Expr::Cast { expr: inner, .. } | Expr::TryCast { expr: inner, .. } => {
            collect_nested_aggregates_inner(inner, out);
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(op) = operand {
                collect_nested_aggregates_inner(op, out);
            }
            for cond in conditions {
                collect_nested_aggregates_inner(cond, out);
            }
            for res in results {
                collect_nested_aggregates_inner(res, out);
            }
            if let Some(el) = else_result {
                collect_nested_aggregates_inner(el, out);
            }
        }
        Expr::Function(f) => {
            for arg in &f.args {
                if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(e),
                    ..
                } = arg
                {
                    collect_nested_aggregates_inner(e, out);
                }
            }
        }
        Expr::IsNull(e)
        | Expr::IsNotNull(e)
        | Expr::IsTrue(e)
        | Expr::IsFalse(e)
        | Expr::IsNotTrue(e)
        | Expr::IsNotFalse(e) => {
            collect_nested_aggregates_inner(e, out);
        }
        Expr::InList { expr: e, list, .. } => {
            collect_nested_aggregates_inner(e, out);
            for item in list {
                collect_nested_aggregates_inner(item, out);
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_nested_aggregates_inner(expr, out);
            collect_nested_aggregates_inner(low, out);
            collect_nested_aggregates_inner(high, out);
        }
        _ => {}
    }
}

pub(crate) fn agg_func_signature(f: &Function) -> String {
    let name = f
        .name
        .0
        .last()
        .map(|n| n.value.to_uppercase())
        .unwrap_or_default();
    let distinct_prefix = if f.distinct { "DISTINCT " } else { "" };
    let args_str: Vec<String> = f
        .args
        .iter()
        .map(|a| match a {
            FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => "*".to_string(),
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => format!("{}", e),
            _ => format!("{:?}", a),
        })
        .collect();
    let filter_suffix = f
        .filter
        .as_ref()
        .map_or(String::new(), |flt| format!(" filter(where {})", flt));
    format!(
        "{}({}{}){}",
        name,
        distinct_prefix,
        args_str.join(", "),
        filter_suffix
    )
    .to_lowercase()
}

pub(crate) fn rewrite_agg_refs_to_columns(
    expr: &Expr,
    agg_column_map: &HashMap<String, String>,
    group_by_names: &[String],
) -> Expr {
    match expr {
        Expr::Function(f) if is_aggregate_func(f) => {
            let sig = agg_func_signature(f);
            if let Some(col_name) = agg_column_map.get(&sig) {
                Expr::Identifier(Ident::new(col_name.clone()))
            } else {
                expr.clone()
            }
        }
        Expr::ArrayAgg(_) => {
            let sig = format!("{}", expr).to_lowercase();
            if let Some(col_name) = agg_column_map.get(&sig) {
                Expr::Identifier(Ident::new(col_name.clone()))
            } else {
                expr.clone()
            }
        }
        Expr::Identifier(id) => {
            if group_by_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(&id.value))
            {
                expr.clone()
            } else {
                expr.clone()
            }
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(rewrite_agg_refs_to_columns(
                left,
                agg_column_map,
                group_by_names,
            )),
            op: op.clone(),
            right: Box::new(rewrite_agg_refs_to_columns(
                right,
                agg_column_map,
                group_by_names,
            )),
        },
        Expr::UnaryOp { op, expr: inner } => Expr::UnaryOp {
            op: op.clone(),
            expr: Box::new(rewrite_agg_refs_to_columns(
                inner,
                agg_column_map,
                group_by_names,
            )),
        },
        Expr::Nested(inner) => Expr::Nested(Box::new(rewrite_agg_refs_to_columns(
            inner,
            agg_column_map,
            group_by_names,
        ))),
        Expr::Cast {
            expr: inner,
            data_type,
            format,
        } => Expr::Cast {
            expr: Box::new(rewrite_agg_refs_to_columns(
                inner,
                agg_column_map,
                group_by_names,
            )),
            data_type: data_type.clone(),
            format: format.clone(),
        },
        Expr::TryCast {
            expr: inner,
            data_type,
            format,
        } => Expr::TryCast {
            expr: Box::new(rewrite_agg_refs_to_columns(
                inner,
                agg_column_map,
                group_by_names,
            )),
            data_type: data_type.clone(),
            format: format.clone(),
        },
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => Expr::Case {
            operand: operand.as_ref().map(|o| {
                Box::new(rewrite_agg_refs_to_columns(
                    o,
                    agg_column_map,
                    group_by_names,
                ))
            }),
            conditions: conditions
                .iter()
                .map(|c| rewrite_agg_refs_to_columns(c, agg_column_map, group_by_names))
                .collect(),
            results: results
                .iter()
                .map(|r| rewrite_agg_refs_to_columns(r, agg_column_map, group_by_names))
                .collect(),
            else_result: else_result.as_ref().map(|e| {
                Box::new(rewrite_agg_refs_to_columns(
                    e,
                    agg_column_map,
                    group_by_names,
                ))
            }),
        },
        Expr::Function(f) => {
            let new_args: Vec<FunctionArg> =
                f.args
                    .iter()
                    .map(|a| match a {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(
                                rewrite_agg_refs_to_columns(e, agg_column_map, group_by_names),
                            ))
                        }
                        other => other.clone(),
                    })
                    .collect();
            Expr::Function(Function {
                name: f.name.clone(),
                args: new_args,
                filter: f.filter.clone(),
                null_treatment: f.null_treatment.clone(),
                over: f.over.clone(),
                distinct: f.distinct,
                special: f.special,
                order_by: f.order_by.clone(),
            })
        }
        Expr::IsNull(e) => Expr::IsNull(Box::new(rewrite_agg_refs_to_columns(
            e,
            agg_column_map,
            group_by_names,
        ))),
        Expr::IsNotNull(e) => Expr::IsNotNull(Box::new(rewrite_agg_refs_to_columns(
            e,
            agg_column_map,
            group_by_names,
        ))),
        _ => expr.clone(),
    }
}

impl Executor {
    pub(crate) fn add_pg_get_indexdef_support_to_group_by(
        projection: &[SelectItem],
        group_by_exprs: &mut Vec<Expr>,
        group_by_names: &mut Vec<String>,
        group_by_types: &mut Vec<DataType>,
        schema: &TableSchema,
    ) {
        if group_by_exprs.is_empty() {
            return;
        }

        let mut group_by_set: std::collections::HashSet<String> = group_by_exprs
            .iter()
            .map(|expr| format!("{}", expr).to_lowercase())
            .collect();

        for item in projection {
            let expr = match item {
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                _ => continue,
            };

            let Expr::Function(func) = expr else {
                continue;
            };

            let func_name = func
                .name
                .0
                .last()
                .map(|ident| ident.value.as_str())
                .unwrap_or("");
            if !func_name.eq_ignore_ascii_case("pg_get_indexdef") {
                continue;
            }

            let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(arg_expr))) = func.args.first()
            else {
                continue;
            };

            let arg_key = format!("{}", arg_expr).to_lowercase();
            if !group_by_set.contains(&arg_key) {
                continue;
            }

            let def_expr = match arg_expr {
                Expr::CompoundIdentifier(parts) if !parts.is_empty() => {
                    let mut def_parts = parts.clone();
                    *def_parts.last_mut().expect("checked non-empty") = Ident::new("indexdef");
                    Expr::CompoundIdentifier(def_parts)
                }
                Expr::Identifier(_) => Expr::Identifier(Ident::new("indexdef")),
                _ => continue,
            };

            let def_name = format!("{}", def_expr);
            if schema.column_index(&def_name).is_none() {
                continue;
            }

            let def_key = def_name.to_lowercase();
            if group_by_set.contains(&def_key) {
                continue;
            }

            group_by_set.insert(def_key);
            group_by_names.push(match &def_expr {
                Expr::Identifier(ident) => ident.value.clone(),
                _ => def_name.clone(),
            });
            group_by_types.push(infer_expr_type(&def_expr, schema));
            group_by_exprs.push(def_expr);
        }
    }

    fn validate_projection_columns(expr: &Expr, schema: &TableSchema) -> Result<()> {
        match expr {
            Expr::Identifier(ident) => {
                let col_name = &ident.value;
                if schema
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(col_name))
                {
                    return Ok(());
                }

                // Whole-row reference: `SELECT t_alias` (composite value), only if it doesn't
                // resolve to a column name.
                let schema_short_name = schema.name.rsplit('.').next().unwrap_or(&schema.name);
                if schema_short_name.eq_ignore_ascii_case(col_name) {
                    return Ok(());
                }
                let prefix = format!("{}.", col_name.to_lowercase());
                if schema
                    .columns
                    .iter()
                    .any(|c| c.name.to_lowercase().starts_with(&prefix))
                {
                    return Ok(());
                }

                Err(SqlError::ColumnNotFound {
                    column: col_name.to_string(),
                }
                .into())
            }
            Expr::CompoundIdentifier(parts) => {
                if let Some(col_ident) = parts.last() {
                    let col_name = &col_ident.value;
                    if !schema
                        .columns
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(col_name))
                    {
                        return Err(SqlError::ColumnNotFound {
                            column: col_name.to_string(),
                        }
                        .into());
                    }
                }
                Ok(())
            }
            Expr::BinaryOp { left, right, .. } => {
                Self::validate_projection_columns(left, schema)?;
                Self::validate_projection_columns(right, schema)
            }
            Expr::UnaryOp { expr, .. } => Self::validate_projection_columns(expr, schema),
            Expr::Cast { expr, .. } => Self::validate_projection_columns(expr, schema),
            Expr::Nested(e) => Self::validate_projection_columns(e, schema),
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => {
                if let Some(op) = operand {
                    Self::validate_projection_columns(op, schema)?;
                }
                for cond in conditions {
                    Self::validate_projection_columns(cond, schema)?;
                }
                for res in results {
                    Self::validate_projection_columns(res, schema)?;
                }
                if let Some(el) = else_result {
                    Self::validate_projection_columns(el, schema)?;
                }
                Ok(())
            }
            Expr::IsNull(e)
            | Expr::IsNotNull(e)
            | Expr::IsTrue(e)
            | Expr::IsFalse(e)
            | Expr::IsNotTrue(e)
            | Expr::IsNotFalse(e) => Self::validate_projection_columns(e, schema),
            Expr::Function(f) => {
                for arg in &f.args {
                    match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
                        | FunctionArg::Named {
                            arg: FunctionArgExpr::Expr(e),
                            ..
                        } => {
                            Self::validate_projection_columns(e, schema)?;
                        }
                        _ => {}
                    }
                }
                Ok(())
            }
            Expr::Value(_) => Ok(()),
            _ => Ok(()),
        }
    }

    fn infer_agg_type(func_name: &str, arg: &Option<Expr>, schema: &TableSchema) -> DataType {
        match func_name {
            "COUNT" => DataType::Int64,
            "SUM" => {
                if let Some(ref a) = arg {
                    match infer_expr_type(a, schema) {
                        DataType::Int32 | DataType::Int64 => DataType::Int64,
                        DataType::Float64 => DataType::Float64,
                        DataType::Numeric { .. } => DataType::Numeric {
                            precision: None,
                            scale: None,
                        },
                        _ => DataType::Numeric {
                            precision: None,
                            scale: None,
                        },
                    }
                } else {
                    DataType::Int64
                }
            }
            "AVG" => {
                if let Some(ref a) = arg {
                    match infer_expr_type(a, schema) {
                        DataType::Float64 => DataType::Float64,
                        _ => DataType::Numeric {
                            precision: None,
                            scale: None,
                        },
                    }
                } else {
                    DataType::Numeric {
                        precision: None,
                        scale: None,
                    }
                }
            }
            "MIN" | "MAX" => {
                if let Some(ref a) = arg {
                    infer_expr_type(a, schema)
                } else {
                    DataType::Text
                }
            }
            "STRING_AGG" => DataType::Text,
            "ARRAY_AGG" => {
                let elem_type = arg
                    .as_ref()
                    .map(|a| infer_expr_type(a, schema))
                    .unwrap_or(DataType::Text);
                DataType::Array(Box::new(elem_type))
            }
            "BOOL_AND" | "BOOL_OR" | "EVERY" => DataType::Boolean,
            _ => DataType::Text,
        }
    }

    pub(crate) fn add_aggregate_from_function(
        f: &Function,
        alias: Option<String>,
        schema: &TableSchema,
        agg_exprs: &mut Vec<AggregateExpr>,
        agg_names: &mut Vec<String>,
        agg_types: &mut Vec<DataType>,
        seen_sigs: &mut std::collections::HashSet<String>,
    ) {
        let func_name = f
            .name
            .0
            .last()
            .map(|n| n.value.to_uppercase())
            .unwrap_or_default();

        let sig = agg_func_signature(f);
        if seen_sigs.contains(&sig) {
            return;
        }
        seen_sigs.insert(sig);

        let arg = f.args.first().and_then(|arg| match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e.clone()),
            FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => None,
            _ => None,
        });

        let delimiter = if func_name == "STRING_AGG" {
            f.args.get(1).and_then(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(
                    SqlValue::SingleQuotedString(s),
                ))) => Some(s.clone()),
                _ => None,
            })
        } else {
            None
        };

        let name = alias.unwrap_or_else(|| func_name.to_lowercase());
        let data_type = Self::infer_agg_type(&func_name, &arg, schema);
        let filter = f.filter.as_ref().map(|f| *f.clone());

        agg_exprs.push(AggregateExpr {
            func_name,
            arg,
            distinct: f.distinct,
            delimiter,
            filter,
            order_by: vec![],
        });
        agg_names.push(name);
        agg_types.push(data_type);
    }

    pub(crate) fn extract_aggregate_info(
        projection: &[SelectItem],
        schema: &TableSchema,
    ) -> (Vec<AggregateExpr>, Vec<String>, Vec<DataType>) {
        let mut agg_exprs = Vec::new();
        let mut agg_names = Vec::new();
        let mut agg_types = Vec::new();
        let mut seen_sigs = std::collections::HashSet::new();

        for item in projection {
            let (expr, alias) = match item {
                SelectItem::UnnamedExpr(e) => (e, None),
                SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.value.clone())),
                _ => continue,
            };

            if let Expr::Function(f) = expr {
                if is_aggregate_func(f) {
                    Self::add_aggregate_from_function(
                        f,
                        alias,
                        schema,
                        &mut agg_exprs,
                        &mut agg_names,
                        &mut agg_types,
                        &mut seen_sigs,
                    );
                    continue;
                }
            }

            if let Expr::ArrayAgg(arr) = expr {
                let name = alias.unwrap_or_else(|| "array_agg".to_string());
                let sig = format!("{}", expr).to_lowercase();
                if !seen_sigs.contains(&sig) {
                    seen_sigs.insert(sig);
                    let arg = Some((*arr.expr).clone());
                    agg_exprs.push(AggregateExpr {
                        func_name: "ARRAY_AGG".to_string(),
                        arg: arg.clone(),
                        distinct: arr.distinct,
                        delimiter: None,
                        filter: None,
                        order_by: arr.order_by.clone().unwrap_or_default(),
                    });
                    agg_names.push(name);
                    agg_types.push(DataType::Array(Box::new(infer_expr_type(
                        arr.expr.as_ref(),
                        schema,
                    ))));
                }
                continue;
            }

            let nested = collect_nested_aggregates(expr);
            for f in nested {
                Self::add_aggregate_from_function(
                    f,
                    None,
                    schema,
                    &mut agg_exprs,
                    &mut agg_names,
                    &mut agg_types,
                    &mut seen_sigs,
                );
            }
        }

        (agg_exprs, agg_names, agg_types)
    }

    pub(crate) fn extract_group_by_info(
        group_by: &GroupByExpr,
        schema: &TableSchema,
    ) -> (Vec<Expr>, Vec<String>, Vec<DataType>) {
        let exprs = match group_by {
            GroupByExpr::Expressions(exprs) => exprs.clone(),
            GroupByExpr::All => Vec::new(),
        };

        let mut names = Vec::new();
        let mut types = Vec::new();

        for expr in &exprs {
            let name = match expr {
                Expr::Identifier(id) => id.value.clone(),
                // Canonicalize qualified column references like `t.col` to the unqualified
                // column name. This keeps aggregate output schema compatible with projections
                // that reference the column as `col` (common in ORMs).
                Expr::CompoundIdentifier(parts) => parts
                    .last()
                    .map(|p| p.value.clone())
                    .unwrap_or_else(|| format!("{}", expr)),
                _ => format!("{}", expr),
            };
            let data_type = infer_expr_type(expr, schema);
            names.push(name);
            types.push(data_type);
        }

        (exprs, names, types)
    }

    pub(crate) async fn execute_aggregate_with_operators(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        schema: TableSchema,
        filter: Option<&Expr>,
        group_by: &GroupByExpr,
        having: Option<&Expr>,
        order_by: &[OrderByExpr],
        limit: Option<usize>,
        offset: usize,
        projection: &[SelectItem],
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        preloaded_rows: Option<Vec<Row>>,
    ) -> Result<ExecuteResult> {
        let (mut group_by_exprs, mut group_by_names, mut group_by_types) =
            Self::extract_group_by_info(group_by, &schema);
        let (mut agg_exprs, mut agg_names, mut agg_types) =
            Self::extract_aggregate_info(projection, &schema);

        Self::add_pg_get_indexdef_support_to_group_by(
            projection,
            &mut group_by_exprs,
            &mut group_by_names,
            &mut group_by_types,
            &schema,
        );

        if let Some(having_expr) = having {
            let mut seen_sigs: std::collections::HashSet<String> = agg_exprs
                .iter()
                .map(|a| {
                    let distinct_prefix = if a.distinct { "DISTINCT " } else { "" };
                    let arg_str = a.arg.as_ref().map_or("*".to_string(), |e| format!("{}", e));
                    let filter_suffix = a
                        .filter
                        .as_ref()
                        .map_or(String::new(), |flt| format!(" filter(where {})", flt));
                    let mut s = format!(
                        "{}({}{}){}",
                        a.func_name, distinct_prefix, arg_str, filter_suffix
                    )
                    .to_lowercase();
                    if let Some(ref delim) = a.delimiter {
                        s = format!(
                            "{}({}{}, '{}'){}",
                            a.func_name, distinct_prefix, arg_str, delim, filter_suffix
                        )
                        .to_lowercase();
                    }
                    s
                })
                .collect();
            let having_aggs = collect_nested_aggregates(having_expr);
            for f in having_aggs {
                Self::add_aggregate_from_function(
                    f,
                    None,
                    &schema,
                    &mut agg_exprs,
                    &mut agg_names,
                    &mut agg_types,
                    &mut seen_sigs,
                );
            }
        }

        let group_by_count = group_by_names.len();

        let mut root: BoxedOperator = if let Some(rows) = preloaded_rows {
            Box::new(TableScanOperator::new_with_rows(schema.clone(), rows))
        } else {
            Box::new(TableScanOperator::new(schema.clone()))
        };

        if let Some(filter_expr) = filter {
            root = Box::new(FilterOperator::new(root, filter_expr.clone()));
        }

        let group_by_exprs_clone = group_by_exprs.clone();
        root = Box::new(HashAggregateOperator::new(
            root,
            group_by_exprs,
            agg_exprs.clone(),
            group_by_names.clone(),
            group_by_types.clone(),
            agg_names.clone(),
            agg_types.clone(),
        ));

        let rows = if ctes.is_empty() {
            execute_operator_tree(
                &mut root,
                txn,
                self.store(),
                db_id,
                search_path,
                sequence_values,
            )
            .await?
        } else {
            execute_operator_tree_with_ctes(
                &mut root,
                txn,
                self.store(),
                db_id,
                search_path,
                sequence_values,
                ctes,
            )
            .await?
        };

        let agg_output_schema = root.schema().clone();

        // Apply HAVING filter after aggregation
        let rows = if let Some(having_expr) = having {
            let mut filtered_rows = Vec::with_capacity(rows.len());
            for row in rows {
                let having_val = eval_having_expr_for_operators(
                    having_expr,
                    &row,
                    &agg_output_schema,
                    &agg_exprs,
                    group_by_count,
                )?;
                let having_val = coerce_text_literal_to_bool(having_expr, having_val)?;
                match having_val {
                    Value::Boolean(true) => filtered_rows.push(row),
                    Value::Boolean(false) | Value::Null => {}
                    other => {
                        return Err(anyhow!(
                            "HAVING clause must evaluate to boolean, got {:?}",
                            other
                        ));
                    }
                }
            }
            filtered_rows
        } else {
            rows
        };

        let agg_column_map: HashMap<String, String> = {
            let mut map = HashMap::new();
            for (i, agg) in agg_exprs.iter().enumerate() {
                let distinct_prefix = if agg.distinct { "DISTINCT " } else { "" };
                let arg_str = agg
                    .arg
                    .as_ref()
                    .map_or("*".to_string(), |e| format!("{}", e));
                let filter_suffix = agg
                    .filter
                    .as_ref()
                    .map_or(String::new(), |flt| format!(" filter(where {})", flt));
                let mut sig = format!(
                    "{}({}{}){}",
                    agg.func_name, distinct_prefix, arg_str, filter_suffix
                )
                .to_lowercase();
                if let Some(ref delim) = agg.delimiter {
                    sig = format!(
                        "{}({}{}, '{}'){}",
                        agg.func_name, distinct_prefix, arg_str, delim, filter_suffix
                    )
                    .to_lowercase();
                }
                map.insert(sig, agg_names[i].clone());
            }
            for item in projection {
                let expr = match item {
                    SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                    _ => continue,
                };
                let (func, name) = match expr {
                    Expr::Function(f) if is_aggregate_func(f) => {
                        (Some(f), get_select_item_name(item))
                    }
                    _ => (None, String::new()),
                };
                if let Some(f) = func {
                    let sig = agg_func_signature(f);
                    if !map.contains_key(&sig) {
                        map.insert(sig, name);
                    }
                }
                if let Expr::ArrayAgg(_) = expr {
                    let sig = format!("{}", expr).to_lowercase();
                    let name = get_select_item_name(item);
                    if !map.contains_key(&sig) {
                        map.insert(sig, name);
                    }
                }
            }
            map
        };

        let group_by_expr_map: HashMap<String, String> = group_by_exprs_clone
            .iter()
            .zip(group_by_names.iter())
            .map(|(expr, name)| (format!("{}", expr).to_lowercase(), name.clone()))
            .collect();

        let mut columns: Vec<String> = Vec::new();
        let mut column_types: Vec<DataType> = Vec::new();
        let mut projection_exprs: Vec<Expr> = Vec::new();

        for item in projection {
            match item {
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                    for col in &agg_output_schema.columns {
                        columns.push(col.name.clone());
                        column_types.push(col.data_type.clone());
                        projection_exprs.push(Expr::Identifier(Ident::new(col.name.clone())));
                    }
                }
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    columns.push(get_select_item_name(item));
                    let expr_str = format!("{}", expr).to_lowercase();
                    if let Some(gb_col) = group_by_expr_map.get(&expr_str) {
                        let rewritten = Expr::Identifier(Ident::new(gb_col.clone()));
                        column_types.push(infer_expr_type(&rewritten, &agg_output_schema));
                        projection_exprs.push(rewritten);
                    } else {
                        let rewritten =
                            rewrite_agg_refs_to_columns(expr, &agg_column_map, &group_by_names);
                        column_types.push(infer_expr_type(&rewritten, &agg_output_schema));
                        projection_exprs.push(rewritten);
                    }
                }
            }
        }

        let mut projected_rows: Vec<Row> = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut values: Vec<Value> = Vec::with_capacity(projection_exprs.len());
            for expr in &projection_exprs {
                let val = eval_expr(expr, Some(row), Some(&agg_output_schema))?;
                values.push(val);
            }
            projected_rows.push(Row::new(values));
        }

        if !order_by.is_empty() {
            projected_rows = self.apply_order_by_for_aggregate(projected_rows, order_by, &columns);
        }

        if offset > 0 {
            projected_rows = projected_rows.into_iter().skip(offset).collect();
        }

        if let Some(limit) = limit {
            projected_rows.truncate(limit);
        }

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows: projected_rows,
            timezone: crate::session_context::current_timezone(),
        })
    }

    pub(crate) async fn execute_simple_join_with_operators(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        left_schema: TableSchema,
        right_schema: TableSchema,
        left_alias: &str,
        right_alias: &str,
        join_type: JoinType,
        join_condition: Option<Expr>,
        filter: Option<&Expr>,
        order_by: &[OrderByExpr],
        limit: Option<usize>,
        offset: usize,
        projection: &[SelectItem],
        left_preloaded: Option<Vec<Row>>,
        right_preloaded: Option<Vec<Row>>,
    ) -> Result<ExecuteResult> {
        let mut combined_columns: Vec<ColumnDef> = Vec::new();
        for col in &left_schema.columns {
            combined_columns.push(ColumnDef {
                name: format!("{}.{}", left_alias, col.name),
                data_type: col.data_type.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            });
        }
        for col in &right_schema.columns {
            combined_columns.push(ColumnDef {
                name: format!("{}.{}", right_alias, col.name),
                data_type: col.data_type.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            });
        }

        let combined_schema = TableSchema {
            name: "join_result".to_string(),
            table_id: 0,
            columns: combined_columns,
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        let left_estimate = left_preloaded.as_ref().map_or(1000, |r| r.len());
        let right_estimate = right_preloaded.as_ref().map_or(1000, |r| r.len());
        let join_algo = choose_join_algorithm(
            join_condition.as_ref(),
            &left_schema,
            &right_schema,
            left_estimate,
            right_estimate,
            &HashJoinConfig::default(),
        );

        let rewritten_condition = match join_condition {
            Some(cond) => Some(rewrite_join_expr_with_aliases(
                &cond,
                left_alias,
                right_alias,
                &left_schema,
                &right_schema,
            )?),
            None => None,
        };

        let rewritten_filter = match filter {
            Some(f) => Some(rewrite_join_expr_with_aliases(
                f,
                left_alias,
                right_alias,
                &left_schema,
                &right_schema,
            )?),
            None => None,
        };

        let mut projection_exprs_for_order_by: Vec<Expr> = Vec::new();
        let mut alias_exprs_for_order_by: HashMap<String, Expr> = HashMap::new();
        for item in projection {
            match item {
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                    projection_exprs_for_order_by.extend(
                        combined_schema
                            .columns
                            .iter()
                            .map(|col| Expr::Identifier(Ident::new(col.name.clone()))),
                    );
                }
                SelectItem::UnnamedExpr(expr) => {
                    projection_exprs_for_order_by.push(rewrite_join_expr_with_aliases(
                        expr,
                        left_alias,
                        right_alias,
                        &left_schema,
                        &right_schema,
                    )?);
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    let rewritten = rewrite_join_expr_with_aliases(
                        expr,
                        left_alias,
                        right_alias,
                        &left_schema,
                        &right_schema,
                    )?;
                    alias_exprs_for_order_by.insert(alias.value.to_lowercase(), rewritten.clone());
                    projection_exprs_for_order_by.push(rewritten);
                }
            }
        }

        let mut rewritten_order_by: Vec<OrderByExpr> = Vec::with_capacity(order_by.len());
        for o in order_by {
            let expr = if let Expr::Identifier(ident) = &o.expr {
                if let Some(expr) = alias_exprs_for_order_by.get(&ident.value.to_lowercase()) {
                    expr.clone()
                } else {
                    rewrite_join_expr_with_aliases(
                        &o.expr,
                        left_alias,
                        right_alias,
                        &left_schema,
                        &right_schema,
                    )?
                }
            } else if let Expr::Value(SqlValue::Number(n, _)) = &o.expr {
                if let Ok(pos) = n.parse::<usize>() {
                    if pos == 0 || pos > projection_exprs_for_order_by.len() {
                        return Err(anyhow!("ORDER BY position {} is not in select list", pos));
                    }
                    projection_exprs_for_order_by[pos - 1].clone()
                } else {
                    rewrite_join_expr_with_aliases(
                        &o.expr,
                        left_alias,
                        right_alias,
                        &left_schema,
                        &right_schema,
                    )?
                }
            } else {
                rewrite_join_expr_with_aliases(
                    &o.expr,
                    left_alias,
                    right_alias,
                    &left_schema,
                    &right_schema,
                )?
            };

            rewritten_order_by.push(OrderByExpr {
                expr,
                asc: o.asc,
                nulls_first: o.nulls_first,
            });
        }

        let left_op: BoxedOperator = if let Some(rows) = left_preloaded {
            Box::new(TableScanOperator::new_with_rows(left_schema.clone(), rows))
        } else {
            Box::new(TableScanOperator::new(left_schema.clone()))
        };
        let right_op: BoxedOperator = if let Some(rows) = right_preloaded {
            Box::new(TableScanOperator::new_with_rows(right_schema.clone(), rows))
        } else {
            Box::new(TableScanOperator::new(right_schema.clone()))
        };

        let mut root: BoxedOperator = match join_algo {
            JoinAlgorithmChoice::HashJoin {
                left_is_build,
                left_key_indices,
                right_key_indices,
            } => {
                let hash_join_type = match join_type {
                    JoinType::Inner => HashJoinType::Inner,
                    JoinType::Left => HashJoinType::Left,
                    JoinType::Right => HashJoinType::Right,
                    JoinType::Full => HashJoinType::Full,
                    JoinType::Cross => HashJoinType::Inner,
                };
                Box::new(
                    HashJoinOperator::new(
                        left_op,
                        right_op,
                        hash_join_type,
                        left_key_indices,
                        right_key_indices,
                        left_is_build,
                        None,
                        HashJoinConfig::default(),
                    )
                    .with_output_schema(combined_schema.clone()),
                )
            }
            JoinAlgorithmChoice::NestedLoop => Box::new(NestedLoopJoinOperator::with_schema(
                left_op,
                right_op,
                join_type,
                rewritten_condition,
                combined_schema.clone(),
            )),
        };

        if let Some(filter_expr) = rewritten_filter {
            root = Box::new(FilterOperator::new(root, filter_expr));
        }

        if !rewritten_order_by.is_empty() {
            root = Box::new(SortOperator::new(root, rewritten_order_by));
        }

        if limit.is_some() || offset > 0 {
            root = Box::new(LimitOperator::new(root, limit, offset));
        }

        let rows = execute_operator_tree(
            &mut root,
            txn,
            self.store(),
            db_id,
            search_path,
            sequence_values,
        )
        .await?;

        let has_wildcard = projection.iter().any(|p| {
            matches!(
                p,
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _)
            )
        });

        enum ProjectionSource {
            ColumnIndex(usize),
            Expr(Expr),
        }

        let (columns, column_types, projected_rows) = if has_wildcard {
            let mut cols: Vec<String> = Vec::new();
            let mut types: Vec<DataType> = Vec::new();
            let mut sources: Vec<ProjectionSource> = Vec::new();

            for item in projection {
                match item {
                    SelectItem::Wildcard(_) => {
                        for (idx, c) in combined_schema.columns.iter().enumerate() {
                            let unqualified = c.name.split('.').last().unwrap_or(&c.name);
                            if unqualified.starts_with("__tipg_subquery_") {
                                continue;
                            }
                            cols.push(c.name.split('.').last().unwrap_or(&c.name).to_string());
                            types.push(c.data_type.clone());
                            sources.push(ProjectionSource::ColumnIndex(idx));
                        }
                    }
                    SelectItem::QualifiedWildcard(obj, _) => {
                        let qualifier = obj.0.last().map(|i| i.value.clone()).unwrap_or_default();
                        let mut matched = false;
                        for (idx, c) in combined_schema.columns.iter().enumerate() {
                            let prefix = c.name.split('.').next().unwrap_or(&c.name);
                            if prefix.eq_ignore_ascii_case(&qualifier) {
                                let unqualified = c.name.split('.').last().unwrap_or(&c.name);
                                if unqualified.starts_with("__tipg_subquery_") {
                                    continue;
                                }
                                matched = true;
                                cols.push(c.name.split('.').last().unwrap_or(&c.name).to_string());
                                types.push(c.data_type.clone());
                                sources.push(ProjectionSource::ColumnIndex(idx));
                            }
                        }
                        if !matched {
                            return Err(anyhow!(
                                "Qualified wildcard {}.* not found in join output",
                                qualifier
                            ));
                        }
                    }
                    SelectItem::UnnamedExpr(expr) => {
                        let original_name = get_select_item_name(item);
                        let rewritten = rewrite_join_expr_with_aliases(
                            expr,
                            left_alias,
                            right_alias,
                            &left_schema,
                            &right_schema,
                        )?;
                        cols.push(original_name);
                        types.push(infer_expr_type(&rewritten, &combined_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                    SelectItem::ExprWithAlias { expr, alias } => {
                        let rewritten = rewrite_join_expr_with_aliases(
                            expr,
                            left_alias,
                            right_alias,
                            &left_schema,
                            &right_schema,
                        )?;
                        cols.push(alias.value.clone());
                        types.push(infer_expr_type(&rewritten, &combined_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                }
            }

            let mut projected = Vec::with_capacity(rows.len());
            for row in rows {
                let mut values = Vec::with_capacity(sources.len());
                for src in &sources {
                    let val = match src {
                        ProjectionSource::ColumnIndex(idx) => {
                            row.values.get(*idx).cloned().unwrap_or(Value::Null)
                        }
                        ProjectionSource::Expr(expr) => {
                            eval_expr(expr, Some(&row), Some(&combined_schema))?
                        }
                    };
                    values.push(val);
                }
                projected.push(Row::new(values));
            }

            (cols, types, projected)
        } else {
            let mut rewritten_projection: Vec<SelectItem> = Vec::with_capacity(projection.len());
            for item in projection {
                match item {
                    SelectItem::UnnamedExpr(expr) => {
                        let original_name = get_select_item_name(item);
                        let rewritten = rewrite_join_expr_with_aliases(
                            expr,
                            left_alias,
                            right_alias,
                            &left_schema,
                            &right_schema,
                        )?;
                        let rewritten_name =
                            get_select_item_name(&SelectItem::UnnamedExpr(rewritten.clone()));
                        if rewritten_name != original_name {
                            rewritten_projection.push(SelectItem::ExprWithAlias {
                                expr: rewritten,
                                alias: Ident::new(original_name),
                            });
                        } else {
                            rewritten_projection.push(SelectItem::UnnamedExpr(rewritten));
                        }
                    }
                    SelectItem::ExprWithAlias { expr, alias } => {
                        rewritten_projection.push(SelectItem::ExprWithAlias {
                            expr: rewrite_join_expr_with_aliases(
                                expr,
                                left_alias,
                                right_alias,
                                &left_schema,
                                &right_schema,
                            )?,
                            alias: alias.clone(),
                        });
                    }
                    other => rewritten_projection.push(other.clone()),
                }
            }

            let cols: Vec<String> = rewritten_projection
                .iter()
                .map(|item| get_select_item_name(item))
                .collect();

            let types: Vec<DataType> = rewritten_projection
                .iter()
                .map(|item| match item {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        infer_expr_type(expr, &combined_schema)
                    }
                    _ => DataType::Text,
                })
                .collect();

            let mut projected = Vec::with_capacity(rows.len());
            for row in rows {
                let mut values = Vec::with_capacity(rewritten_projection.len());
                for item in &rewritten_projection {
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                        _ => continue,
                    };
                    let val = eval_expr(expr, Some(&row), Some(&combined_schema))?;
                    values.push(val);
                }
                projected.push(Row::new(values));
            }
            (cols, types, projected)
        };

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows: projected_rows,
            timezone: crate::session_context::current_timezone(),
        })
    }

    pub(crate) async fn execute_with_operators(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        schema: TableSchema,
        filter: Option<&Expr>,
        order_by: &[OrderByExpr],
        limit: Option<usize>,
        offset: usize,
        projection: &[SelectItem],
        distinct: Option<&Distinct>,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        preloaded_rows: Option<Vec<Row>>,
    ) -> Result<ExecuteResult> {
        let planner = PhysicalPlanner::new(self.store(), search_path.to_vec());

        let is_wildcard_only = projection.iter().all(|item| {
            matches!(
                item,
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _)
            )
        });

        let mut columns: Vec<String> = Vec::new();
        let mut column_types: Vec<DataType> = Vec::new();
        let mut projection_exprs: Vec<Expr> = Vec::new();

        for item in projection {
            match item {
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                    for col in &schema.columns {
                        columns.push(col.name.clone());
                        column_types.push(col.data_type.clone());
                        projection_exprs.push(Expr::Identifier(sqlparser::ast::Ident::new(
                            col.name.clone(),
                        )));
                    }
                }
                SelectItem::UnnamedExpr(expr) => {
                    Self::validate_projection_columns(expr, &schema)?;
                    columns.push(get_select_item_name(item));
                    column_types.push(infer_expr_type(expr, &schema));
                    projection_exprs.push(expr.clone());
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    Self::validate_projection_columns(expr, &schema)?;
                    columns.push(alias.value.clone());
                    column_types.push(infer_expr_type(expr, &schema));
                    projection_exprs.push(expr.clone());
                }
            }
        }

        let mut alias_exprs_for_order_by: HashMap<String, Expr> = HashMap::new();
        for item in projection {
            if let SelectItem::ExprWithAlias { expr, alias } = item {
                alias_exprs_for_order_by.insert(alias.value.to_lowercase(), expr.clone());
            }
        }

        let rewrite_order_by_for_pre_projection_sort =
            |order_by: &[OrderByExpr]| -> Result<Vec<OrderByExpr>> {
                order_by
                    .iter()
                    .map(|o| {
                        let expr = if let Expr::Identifier(ident) = &o.expr {
                            alias_exprs_for_order_by
                                .get(&ident.value.to_lowercase())
                                .cloned()
                                .unwrap_or_else(|| o.expr.clone())
                        } else if let Expr::Value(SqlValue::Number(n, _)) = &o.expr {
                            if let Ok(pos) = n.parse::<usize>() {
                                if pos == 0 || pos > projection_exprs.len() {
                                    return Err(anyhow!(
                                        "ORDER BY position {} is not in select list",
                                        pos
                                    ));
                                }
                                projection_exprs[pos - 1].clone()
                            } else {
                                o.expr.clone()
                            }
                        } else {
                            o.expr.clone()
                        };

                        Ok(OrderByExpr {
                            expr,
                            asc: o.asc,
                            nulls_first: o.nulls_first,
                        })
                    })
                    .collect()
            };

        let rewrite_order_by_for_post_projection_sort =
            |order_by: &[OrderByExpr]| -> Result<Vec<OrderByExpr>> {
                let mut projection_indices_by_expr_name: HashMap<String, Vec<usize>> =
                    HashMap::new();
                let mut projection_indices_by_expr_display: HashMap<String, Vec<usize>> =
                    HashMap::new();
                for (idx, expr) in projection_exprs.iter().enumerate() {
                    projection_indices_by_expr_name
                        .entry(get_expr_name(expr).to_lowercase())
                        .or_default()
                        .push(idx);
                    projection_indices_by_expr_display
                        .entry(format!("{}", expr).to_lowercase())
                        .or_default()
                        .push(idx);
                }

                let resolve_unique_index = |indices: &[usize], what: &str, key: &str| {
                    if indices.is_empty() {
                        return Ok(None);
                    }
                    if indices.len() > 1 {
                        return Err(anyhow!(
                            "ORDER BY {} reference '{}' is ambiguous",
                            what,
                            key
                        ));
                    }
                    Ok(Some(indices[0]))
                };

                order_by
                    .iter()
                    .map(|o| {
                        let expr = match &o.expr {
                            Expr::Value(SqlValue::Number(n, _)) => {
                                let Ok(pos) = n.parse::<usize>() else {
                                    return Ok(OrderByExpr {
                                        expr: o.expr.clone(),
                                        asc: o.asc,
                                        nulls_first: o.nulls_first,
                                    });
                                };
                                if pos == 0 || pos > columns.len() {
                                    return Err(anyhow!(
                                        "ORDER BY position {} is not in select list",
                                        pos
                                    ));
                                }
                                Expr::Identifier(Ident::new(columns[pos - 1].clone()))
                            }
                            Expr::Identifier(ident) => {
                                let matching_cols: Vec<usize> = columns
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, c)| c.eq_ignore_ascii_case(&ident.value))
                                    .map(|(idx, _)| idx)
                                    .collect();
                                if let Some(idx) =
                                    resolve_unique_index(&matching_cols, "column", &ident.value)?
                                {
                                    Expr::Identifier(Ident::new(columns[idx].clone()))
                                } else {
                                    let key = ident.value.to_lowercase();
                                    let idxs = projection_indices_by_expr_name
                                        .get(&key)
                                        .map(|v| v.as_slice())
                                        .unwrap_or(&[]);
                                    if let Some(idx) =
                                        resolve_unique_index(idxs, "expression", &ident.value)?
                                    {
                                        Expr::Identifier(Ident::new(columns[idx].clone()))
                                    } else {
                                        return Err(anyhow!(
                                            "for SELECT DISTINCT, ORDER BY expressions must appear in select list: '{}'",
                                            ident.value
                                        ));
                                    }
                                }
                            }
                            Expr::CompoundIdentifier(parts) => {
                                let expr_name = get_expr_name(&o.expr).to_lowercase();
                                let idxs = projection_indices_by_expr_name
                                    .get(&expr_name)
                                    .map(|v| v.as_slice())
                                    .unwrap_or(&[]);
                                if let Some(idx) =
                                    resolve_unique_index(idxs, "expression", &parts.last().map(|p| p.value.as_str()).unwrap_or_default())?
                                {
                                    Expr::Identifier(Ident::new(columns[idx].clone()))
                                } else {
                                    let key = format!("{}", o.expr).to_lowercase();
                                    let idxs = projection_indices_by_expr_display
                                        .get(&key)
                                        .map(|v| v.as_slice())
                                        .unwrap_or(&[]);
                                    if let Some(idx) =
                                        resolve_unique_index(idxs, "expression", &key)?
                                    {
                                        Expr::Identifier(Ident::new(columns[idx].clone()))
                                    } else {
                                        return Err(anyhow!(
                                            "for SELECT DISTINCT, ORDER BY expressions must appear in select list: '{}'",
                                            format!("{}", o.expr)
                                        ));
                                    }
                                }
                            }
                            _ => {
                                let key = format!("{}", o.expr).to_lowercase();
                                let idxs = projection_indices_by_expr_display
                                    .get(&key)
                                    .map(|v| v.as_slice())
                                    .unwrap_or(&[]);
                                if let Some(idx) =
                                    resolve_unique_index(idxs, "expression", &key)?
                                {
                                    Expr::Identifier(Ident::new(columns[idx].clone()))
                                } else {
                                    return Err(anyhow!(
                                        "for SELECT DISTINCT, ORDER BY expressions must appear in select list: '{}'",
                                        format!("{}", o.expr)
                                    ));
                                }
                            }
                        };

                        Ok(OrderByExpr {
                            expr,
                            asc: o.asc,
                            nulls_first: o.nulls_first,
                        })
                    })
                    .collect()
            };

        let estimated_rows = 1000;

        let is_distinct = matches!(distinct, Some(Distinct::Distinct));
        let is_distinct_on = matches!(distinct, Some(Distinct::On(_)));

        if (is_distinct && !is_wildcard_only) || is_distinct_on {
            let scan_operator = if let Some(rows) = preloaded_rows {
                let mut op: BoxedOperator =
                    Box::new(TableScanOperator::new_with_rows(schema.clone(), rows));
                if let Some(filter_expr) = filter {
                    op = Box::new(FilterOperator::new(op, filter_expr.clone()));
                }
                op
            } else {
                planner.plan_simple_select(
                    db_id,
                    schema.clone(),
                    filter,
                    Vec::new(),
                    None,
                    0,
                    estimated_rows,
                )?
            };

            let post_project: BoxedOperator = if is_distinct_on {
                if let Some(Distinct::On(on_exprs)) = distinct {
                    // DISTINCT ON requires ORDER BY evaluation against the pre-projection schema
                    // (Postgres allows ordering by columns not in the SELECT list).
                    let rewritten_order_by_for_sort =
                        rewrite_order_by_for_pre_projection_sort(order_by)?;
                    let sorted: BoxedOperator = if !rewritten_order_by_for_sort.is_empty() {
                        Box::new(SortOperator::new(
                            scan_operator,
                            rewritten_order_by_for_sort,
                        ))
                    } else {
                        scan_operator
                    };
                    let distincted: BoxedOperator =
                        Box::new(DistinctOnOperator::new(sorted, on_exprs.clone()));
                    Box::new(ProjectOperator::new(
                        distincted,
                        projection_exprs.clone(),
                        columns.clone(),
                        column_types.clone(),
                    ))
                } else {
                    unreachable!()
                }
            } else {
                let project_operator = Box::new(ProjectOperator::new(
                    scan_operator,
                    projection_exprs.clone(),
                    columns.clone(),
                    column_types.clone(),
                ));
                Box::new(DistinctOperator::new(project_operator))
            };

            let mut operator: BoxedOperator = if !is_distinct_on && !order_by.is_empty() {
                let rewritten_order_by = rewrite_order_by_for_post_projection_sort(order_by)?;
                let sort_operator = Box::new(SortOperator::new(post_project, rewritten_order_by));
                if limit.is_some() || offset > 0 {
                    Box::new(LimitOperator::new(sort_operator, limit, offset))
                } else {
                    sort_operator
                }
            } else if limit.is_some() || offset > 0 {
                Box::new(LimitOperator::new(post_project, limit, offset))
            } else {
                post_project
            };

            let rows = if ctes.is_empty() {
                execute_operator_tree(
                    &mut operator,
                    txn,
                    self.store(),
                    db_id,
                    search_path,
                    sequence_values,
                )
                .await?
            } else {
                execute_operator_tree_with_ctes(
                    &mut operator,
                    txn,
                    self.store(),
                    db_id,
                    search_path,
                    sequence_values,
                    ctes,
                )
                .await?
            };

            return Ok(ExecuteResult::Select {
                columns,
                column_types: Some(column_types),
                rows,
                timezone: crate::session_context::current_timezone(),
            });
        }

        let base_operator = if let Some(rows) = preloaded_rows {
            let mut op: BoxedOperator =
                Box::new(TableScanOperator::new_with_rows(schema.clone(), rows));
            if let Some(filter_expr) = filter {
                op = Box::new(FilterOperator::new(op, filter_expr.clone()));
            }
            let rewritten_order_by = rewrite_order_by_for_pre_projection_sort(order_by)?;
            if !rewritten_order_by.is_empty() {
                op = Box::new(SortOperator::new(op, rewritten_order_by));
            }
            if limit.is_some() || offset > 0 {
                op = Box::new(LimitOperator::new(op, limit, offset));
            }
            op
        } else {
            planner.plan_simple_select(
                db_id,
                schema.clone(),
                filter,
                rewrite_order_by_for_pre_projection_sort(order_by)?,
                limit,
                offset,
                estimated_rows,
            )?
        };

        let mut operator: BoxedOperator = if is_distinct {
            Box::new(DistinctOperator::new(base_operator))
        } else {
            base_operator
        };

        let raw_rows = if ctes.is_empty() {
            execute_operator_tree(
                &mut operator,
                txn,
                self.store(),
                db_id,
                search_path,
                sequence_values,
            )
            .await?
        } else {
            execute_operator_tree_with_ctes(
                &mut operator,
                txn,
                self.store(),
                db_id,
                search_path,
                sequence_values,
                ctes,
            )
            .await?
        };

        if is_wildcard_only {
            return Ok(ExecuteResult::Select {
                columns,
                column_types: Some(column_types),
                rows: raw_rows,
                timezone: crate::session_context::current_timezone(),
            });
        }

        let mut rows: Vec<Row> = Vec::with_capacity(raw_rows.len());
        for row in raw_rows {
            let mut values: Vec<Value> = Vec::with_capacity(projection_exprs.len());
            for expr in &projection_exprs {
                values.push(eval_expr(expr, Some(&row), Some(&schema))?);
            }
            rows.push(Row::new(values));
        }

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows,
            timezone: crate::session_context::current_timezone(),
        })
    }

    pub(crate) async fn execute_window_with_operators(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        schema: TableSchema,
        filter: Option<&Expr>,
        order_by: &[OrderByExpr],
        limit: Option<usize>,
        offset: usize,
        projection: &[SelectItem],
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        preloaded_rows: Option<Vec<Row>>,
    ) -> Result<ExecuteResult> {
        let scan_operator: BoxedOperator = if let Some(rows) = preloaded_rows {
            let mut op: BoxedOperator =
                Box::new(TableScanOperator::new_with_rows(schema.clone(), rows));
            if let Some(filter_expr) = filter {
                op = Box::new(FilterOperator::new(op, filter_expr.clone()));
            }
            op
        } else {
            let planner = PhysicalPlanner::new(self.store(), search_path.to_vec());
            let estimated_rows = 1000;
            planner.plan_simple_select(
                db_id,
                schema.clone(),
                filter,
                Vec::new(),
                None,
                0,
                estimated_rows,
            )?
        };

        let window_funcs = Self::extract_window_function_exprs(projection, &schema);

        let window_operator = Box::new(WindowOperator::new(scan_operator, window_funcs.clone()));

        let mut operator: BoxedOperator = if !order_by.is_empty() {
            let sort_op = Box::new(SortOperator::new(window_operator, order_by.to_vec()));
            if limit.is_some() || offset > 0 {
                Box::new(LimitOperator::new(sort_op, limit, offset))
            } else {
                sort_op
            }
        } else if limit.is_some() || offset > 0 {
            Box::new(LimitOperator::new(window_operator, limit, offset))
        } else {
            window_operator
        };

        let raw_rows = if ctes.is_empty() {
            execute_operator_tree(
                &mut operator,
                txn,
                self.store(),
                db_id,
                search_path,
                sequence_values,
            )
            .await?
        } else {
            execute_operator_tree_with_ctes(
                &mut operator,
                txn,
                self.store(),
                db_id,
                search_path,
                sequence_values,
                ctes,
            )
            .await?
        };

        let (columns, column_types, projected_rows) =
            Self::project_window_results(projection, &schema, &window_funcs, raw_rows)?;

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows: projected_rows,
            timezone: crate::session_context::current_timezone(),
        })
    }

    pub(crate) fn extract_window_function_exprs(
        projection: &[SelectItem],
        schema: &TableSchema,
    ) -> Vec<WindowFunctionExpr> {
        use sqlparser::ast::WindowType;

        let mut result = Vec::new();
        for (idx, item) in projection.iter().enumerate() {
            let (func, alias) = match item {
                SelectItem::UnnamedExpr(Expr::Function(f)) => (Some(f), None),
                SelectItem::ExprWithAlias {
                    expr: Expr::Function(f),
                    alias,
                } => (Some(f), Some(alias.value.clone())),
                _ => (None, None),
            };

            if let Some(f) = func {
                if let Some(WindowType::WindowSpec(spec)) = &f.over {
                    let func_name = f
                        .name
                        .0
                        .last()
                        .map(|i| i.value.to_lowercase())
                        .unwrap_or_default();

                    let extract_arg = |index: usize| -> Option<Expr> {
                        f.args.get(index).and_then(|a| match a {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e.clone()),
                            _ => None,
                        })
                    };

                    let arg_expr = extract_arg(0);
                    let offset_expr = extract_arg(1);
                    let default_value_expr = extract_arg(2);

                    let output_name = alias.unwrap_or_else(|| format!("window_{}", idx));
                    let output_type = Self::infer_window_func_type(&func_name, &arg_expr, schema);

                    result.push(WindowFunctionExpr {
                        func_name,
                        arg_expr,
                        partition_by: spec.partition_by.clone(),
                        order_by: spec.order_by.clone(),
                        offset_expr,
                        default_value_expr,
                        window_frame: spec.window_frame.clone(),
                        output_name,
                        output_type,
                    });
                }
            }
        }
        result
    }

    pub(crate) fn infer_window_func_type(
        func_name: &str,
        arg_expr: &Option<Expr>,
        schema: &TableSchema,
    ) -> DataType {
        match func_name {
            "row_number" | "rank" | "dense_rank" | "count" => DataType::Int64,
            "sum" | "avg" => DataType::Numeric {
                precision: None,
                scale: None,
            },
            "min" | "max" | "lag" | "lead" | "first_value" | "last_value" => {
                if let Some(expr) = arg_expr {
                    infer_expr_type(expr, schema)
                } else {
                    DataType::Int64
                }
            }
            _ => DataType::Int64,
        }
    }

    pub(crate) fn project_window_results(
        projection: &[SelectItem],
        schema: &TableSchema,
        window_funcs: &[WindowFunctionExpr],
        rows: Vec<Row>,
    ) -> Result<(Vec<String>, Vec<DataType>, Vec<Row>)> {
        let mut columns = Vec::new();
        let mut column_types = Vec::new();
        let mut proj_info: Vec<(String, DataType, ProjectionSource)> = Vec::new();

        let input_col_count = schema.columns.len();
        let mut window_idx = 0;

        for item in projection {
            match item {
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                    for col in &schema.columns {
                        proj_info.push((
                            col.name.clone(),
                            col.data_type.clone(),
                            ProjectionSource::InputColumn(col.name.clone()),
                        ));
                    }
                }
                SelectItem::UnnamedExpr(Expr::Function(f))
                | SelectItem::ExprWithAlias {
                    expr: Expr::Function(f),
                    ..
                } if f.over.is_some() => {
                    if window_idx < window_funcs.len() {
                        let wf = &window_funcs[window_idx];
                        proj_info.push((
                            wf.output_name.clone(),
                            wf.output_type.clone(),
                            ProjectionSource::WindowColumn(input_col_count + window_idx),
                        ));
                        window_idx += 1;
                    }
                }
                SelectItem::UnnamedExpr(e) => {
                    let name = get_select_item_name(item);
                    let dtype = infer_expr_type(e, schema);
                    proj_info.push((name, dtype, ProjectionSource::Expression(e.clone())));
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    let dtype = infer_expr_type(expr, schema);
                    proj_info.push((
                        alias.value.clone(),
                        dtype,
                        ProjectionSource::Expression(expr.clone()),
                    ));
                }
            }
        }

        for (name, dtype, _) in &proj_info {
            columns.push(name.clone());
            column_types.push(dtype.clone());
        }

        let mut projected_rows = Vec::with_capacity(rows.len());
        for row in rows {
            let mut values = Vec::with_capacity(proj_info.len());
            for (_, _, source) in &proj_info {
                let value = match source {
                    ProjectionSource::InputColumn(name) => {
                        if let Some(idx) = schema.column_index(name) {
                            row.values.get(idx).cloned().unwrap_or(Value::Null)
                        } else {
                            Value::Null
                        }
                    }
                    ProjectionSource::WindowColumn(idx) => {
                        row.values.get(*idx).cloned().unwrap_or(Value::Null)
                    }
                    ProjectionSource::Expression(expr) => {
                        eval_expr(expr, Some(&row), Some(schema))?
                    }
                };
                values.push(value);
            }
            projected_rows.push(Row::new(values));
        }

        Ok((columns, column_types, projected_rows))
    }
}

enum ProjectionSource {
    InputColumn(String),
    WindowColumn(usize),
    Expression(Expr),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ColumnDef;
    use pgwire::api::Type;
    use sqlparser::ast::SetExpr;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn parse_query(sql: &str) -> Query {
        let dialect = PostgreSqlDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        match statements.into_iter().next().unwrap() {
            sqlparser::ast::Statement::Query(q) => *q,
            _ => panic!("Expected query"),
        }
    }

    fn get_select(query: &Query) -> &sqlparser::ast::Select {
        match &*query.body {
            SetExpr::Select(s) => s,
            _ => panic!("Expected select"),
        }
    }

    fn schema_with_column(name: &str, data_type: DataType) -> TableSchema {
        TableSchema {
            name: "t".to_string(),
            table_id: 0,
            columns: vec![ColumnDef {
                name: name.to_string(),
                data_type,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        }
    }

    fn pg_oid_for_datatype(dt: &DataType) -> u32 {
        match dt {
            DataType::Float64 => Type::FLOAT8.oid(),
            DataType::Numeric { .. } => Type::NUMERIC.oid(),
            _ => Type::TEXT.oid(),
        }
    }

    #[test]
    fn test_pg_get_indexdef_projection_keeps_indexdef_in_group_by_schema() {
        let query = parse_query("SELECT pg_get_indexdef(ix.indexrelid) FROM t");
        let select = get_select(&query);

        let schema = make_schema(
            "join_result",
            &[
                ("ix.indexrelid", DataType::Int64),
                ("ix.indexdef", DataType::Text),
            ],
        );

        let mut group_by_exprs = vec![Expr::CompoundIdentifier(vec![
            Ident::new("ix"),
            Ident::new("indexrelid"),
        ])];
        let mut group_by_names = vec!["ix.indexrelid".to_string()];
        let mut group_by_types = vec![DataType::Int64];

        Executor::add_pg_get_indexdef_support_to_group_by(
            &select.projection,
            &mut group_by_exprs,
            &mut group_by_names,
            &mut group_by_types,
            &schema,
        );

        assert!(group_by_exprs
            .iter()
            .any(|expr| format!("{}", expr) == "ix.indexdef"));

        let def_idx = group_by_names
            .iter()
            .position(|name| name == "ix.indexdef")
            .expect("group_by should include ix.indexdef");
        assert_eq!(group_by_types[def_idx], DataType::Text);

        // Idempotent: running again should not add duplicates.
        let original_len = group_by_exprs.len();
        Executor::add_pg_get_indexdef_support_to_group_by(
            &select.projection,
            &mut group_by_exprs,
            &mut group_by_names,
            &mut group_by_types,
            &schema,
        );
        assert_eq!(group_by_exprs.len(), original_len);
    }

    #[test]
    fn test_extract_limit_offset_from_text_literals() {
        let query = parse_query("SELECT * FROM users LIMIT '10' OFFSET '5'");
        assert_eq!(extract_limit(&query), Some(10));
        assert_eq!(extract_offset(&query), 5);
    }

    #[test]
    fn test_extract_aggregate_info_sum_avg_float8_types() {
        let query = parse_query("SELECT SUM(x) AS s, AVG(x) AS a FROM t");
        let select = get_select(&query);
        let schema = schema_with_column("x", DataType::Float64);

        let (_, names, types) = Executor::extract_aggregate_info(&select.projection, &schema);
        assert_eq!(names, vec!["s".to_string(), "a".to_string()]);
        assert_eq!(types, vec![DataType::Float64, DataType::Float64]);
        assert_eq!(
            types.iter().map(pg_oid_for_datatype).collect::<Vec<_>>(),
            vec![701, 701]
        );
    }

    fn make_schema(table_name: &str, cols: &[(&str, DataType)]) -> TableSchema {
        TableSchema {
            name: table_name.to_string(),
            table_id: 0,
            columns: cols
                .iter()
                .map(|(name, dt)| ColumnDef {
                    name: name.to_string(),
                    data_type: dt.clone(),
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                })
                .collect(),
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        }
    }

    #[test]
    fn test_rewrite_expr_unambiguous_column() {
        let users = make_schema(
            "users",
            &[("id", DataType::Int32), ("name", DataType::Text)],
        );
        let orders = make_schema(
            "orders",
            &[("id", DataType::Int32), ("user_id", DataType::Int32)],
        );
        let aliases = vec![("u".to_string(), users), ("o".to_string(), orders)];

        let expr = Expr::Identifier(Ident::new("name"));
        let result = rewrite_expr_for_multi_join(&expr, &aliases).unwrap();
        match result {
            Expr::CompoundIdentifier(parts) => {
                assert_eq!(parts[0].value, "u");
                assert_eq!(parts[1].value, "name");
            }
            other => panic!("expected CompoundIdentifier, got {other:?}"),
        }
    }

    #[test]
    fn test_rewrite_expr_ambiguous_column() {
        let users = make_schema("users", &[("id", DataType::Int32)]);
        let orders = make_schema("orders", &[("id", DataType::Int32)]);
        let aliases = vec![("u".to_string(), users), ("o".to_string(), orders)];

        let expr = Expr::Identifier(Ident::new("id"));
        let result = rewrite_expr_for_multi_join(&expr, &aliases);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("ambiguous"),
            "expected ambiguous column error"
        );
    }

    #[test]
    fn test_rewrite_expr_compound_identifier() {
        let users = make_schema("users", &[("id", DataType::Int32)]);
        let orders = make_schema("orders", &[("user_id", DataType::Int32)]);
        let aliases = vec![("u".to_string(), users), ("o".to_string(), orders)];

        let expr = Expr::CompoundIdentifier(vec![Ident::new("o"), Ident::new("user_id")]);
        let result = rewrite_expr_for_multi_join(&expr, &aliases).unwrap();
        match result {
            Expr::CompoundIdentifier(parts) => {
                assert_eq!(parts[0].value, "o");
                assert_eq!(parts[1].value, "user_id");
            }
            other => panic!("expected CompoundIdentifier, got {other:?}"),
        }
    }

    #[test]
    fn test_rewrite_expr_binary_op_recurses() {
        let users = make_schema("users", &[("id", DataType::Int32)]);
        let orders = make_schema("orders", &[("user_id", DataType::Int32)]);
        let aliases = vec![("u".to_string(), users), ("o".to_string(), orders)];

        let expr = Expr::BinaryOp {
            left: Box::new(Expr::CompoundIdentifier(vec![
                Ident::new("u"),
                Ident::new("id"),
            ])),
            op: sqlparser::ast::BinaryOperator::Eq,
            right: Box::new(Expr::CompoundIdentifier(vec![
                Ident::new("o"),
                Ident::new("user_id"),
            ])),
        };
        let result = rewrite_expr_for_multi_join(&expr, &aliases).unwrap();
        match result {
            Expr::BinaryOp { left, right, .. } => {
                match *left {
                    Expr::CompoundIdentifier(ref parts) => {
                        assert_eq!(parts[0].value, "u");
                        assert_eq!(parts[1].value, "id");
                    }
                    other => panic!("expected CompoundIdentifier left, got {other:?}"),
                }
                match *right {
                    Expr::CompoundIdentifier(ref parts) => {
                        assert_eq!(parts[0].value, "o");
                        assert_eq!(parts[1].value, "user_id");
                    }
                    other => panic!("expected CompoundIdentifier right, got {other:?}"),
                }
            }
            other => panic!("expected BinaryOp, got {other:?}"),
        }
    }

    // ── Epic A tests: aggregate projection rewrite ──

    fn parse_expr(sql_fragment: &str) -> Expr {
        // Parse as "SELECT <fragment> FROM t" and extract the expression
        let sql = format!("SELECT {} FROM t", sql_fragment);
        let query = parse_query(&sql);
        let select = get_select(&query);
        match &select.projection[0] {
            SelectItem::UnnamedExpr(e) => e.clone(),
            SelectItem::ExprWithAlias { expr, .. } => expr.clone(),
            other => panic!("expected expression, got {:?}", other),
        }
    }

    #[test]
    fn test_agg_func_signature_count_star() {
        let expr = parse_expr("COUNT(*)");
        if let Expr::Function(f) = &expr {
            assert_eq!(agg_func_signature(f), "count(*)");
        } else {
            panic!("expected Function, got {:?}", expr);
        }
    }

    #[test]
    fn test_agg_func_signature_sum_column() {
        let expr = parse_expr("SUM(salary)");
        if let Expr::Function(f) = &expr {
            assert_eq!(agg_func_signature(f), "sum(salary)");
        } else {
            panic!("expected Function, got {:?}", expr);
        }
    }

    #[test]
    fn test_agg_func_signature_count_distinct() {
        let expr = parse_expr("COUNT(DISTINCT name)");
        if let Expr::Function(f) = &expr {
            assert_eq!(agg_func_signature(f), "count(distinct name)");
        } else {
            panic!("expected Function, got {:?}", expr);
        }
    }

    #[test]
    fn test_collect_nested_aggregates_bare_count() {
        let expr = parse_expr("COUNT(*)");
        let aggs = collect_nested_aggregates(&expr);
        assert_eq!(aggs.len(), 1);
        assert_eq!(agg_func_signature(aggs[0]), "count(*)");
    }

    #[test]
    fn test_collect_nested_aggregates_in_binary_op() {
        // 'count=' || COUNT(*)
        let expr = parse_expr("'count=' || COUNT(*)");
        let aggs = collect_nested_aggregates(&expr);
        assert_eq!(aggs.len(), 1);
        assert_eq!(agg_func_signature(aggs[0]), "count(*)");
    }

    #[test]
    fn test_collect_nested_aggregates_in_cast() {
        // CAST(AVG(salary) AS INT)
        let expr = parse_expr("CAST(AVG(salary) AS INT)");
        let aggs = collect_nested_aggregates(&expr);
        assert_eq!(aggs.len(), 1);
        assert_eq!(agg_func_signature(aggs[0]), "avg(salary)");
    }

    #[test]
    fn test_collect_nested_aggregates_in_case() {
        // CASE WHEN COUNT(*) > 5 THEN 'many' ELSE 'few' END
        let expr = parse_expr("CASE WHEN COUNT(*) > 5 THEN 'many' ELSE 'few' END");
        let aggs = collect_nested_aggregates(&expr);
        assert_eq!(aggs.len(), 1);
        assert_eq!(agg_func_signature(aggs[0]), "count(*)");
    }

    #[test]
    fn test_collect_nested_aggregates_multiple() {
        // ROUND(AVG(salary), 2) has AVG nested inside ROUND
        let expr = parse_expr("ROUND(AVG(salary), 2)");
        let aggs = collect_nested_aggregates(&expr);
        assert_eq!(aggs.len(), 1);
        assert_eq!(agg_func_signature(aggs[0]), "avg(salary)");
    }

    #[test]
    fn test_collect_nested_aggregates_no_aggs() {
        let expr = parse_expr("1 + 2");
        let aggs = collect_nested_aggregates(&expr);
        assert!(aggs.is_empty());
    }

    #[test]
    fn test_rewrite_agg_refs_bare_count() {
        let expr = parse_expr("COUNT(*)");
        let mut agg_map = HashMap::new();
        agg_map.insert("count(*)".to_string(), "count".to_string());
        let group_by: Vec<String> = vec![];

        let rewritten = rewrite_agg_refs_to_columns(&expr, &agg_map, &group_by);
        match rewritten {
            Expr::Identifier(id) => assert_eq!(id.value, "count"),
            other => panic!("expected Identifier('count'), got {:?}", other),
        }
    }

    #[test]
    fn test_rewrite_agg_refs_concat_with_count() {
        // 'count=' || COUNT(*) → 'count=' || Identifier("count")
        let expr = parse_expr("'count=' || COUNT(*)");
        let mut agg_map = HashMap::new();
        agg_map.insert("count(*)".to_string(), "count".to_string());
        let group_by: Vec<String> = vec![];

        let rewritten = rewrite_agg_refs_to_columns(&expr, &agg_map, &group_by);
        match rewritten {
            Expr::BinaryOp { right, .. } => match *right {
                Expr::Identifier(id) => assert_eq!(id.value, "count"),
                other => panic!("expected Identifier('count') on right, got {:?}", other),
            },
            other => panic!("expected BinaryOp, got {:?}", other),
        }
    }

    #[test]
    fn test_rewrite_agg_refs_case_with_count() {
        // CASE WHEN COUNT(*) > 5 THEN 'many' ELSE 'few' END
        let expr = parse_expr("CASE WHEN COUNT(*) > 5 THEN 'many' ELSE 'few' END");
        let mut agg_map = HashMap::new();
        agg_map.insert("count(*)".to_string(), "count".to_string());
        let group_by: Vec<String> = vec![];

        let rewritten = rewrite_agg_refs_to_columns(&expr, &agg_map, &group_by);
        // The COUNT(*) inside "COUNT(*) > 5" should be rewritten
        match rewritten {
            Expr::Case { conditions, .. } => {
                // The condition is "COUNT(*) > 5" → "Identifier('count') > 5"
                match &conditions[0] {
                    Expr::BinaryOp { left, .. } => match left.as_ref() {
                        Expr::Identifier(id) => assert_eq!(id.value, "count"),
                        other => panic!("expected Identifier in condition, got {:?}", other),
                    },
                    other => panic!("expected BinaryOp condition, got {:?}", other),
                }
            }
            other => panic!("expected Case, got {:?}", other),
        }
    }

    #[test]
    fn test_rewrite_agg_refs_round_avg() {
        // ROUND(AVG(salary), 2) — AVG is nested inside non-agg ROUND
        let expr = parse_expr("ROUND(AVG(salary), 2)");
        let mut agg_map = HashMap::new();
        agg_map.insert("avg(salary)".to_string(), "avg".to_string());
        let group_by: Vec<String> = vec![];

        let rewritten = rewrite_agg_refs_to_columns(&expr, &agg_map, &group_by);
        // Should become ROUND(Identifier("avg"), 2)
        match rewritten {
            Expr::Function(f) => {
                assert_eq!(f.name.to_string().to_uppercase(), "ROUND");
                // First arg should be rewritten to Identifier("avg")
                match &f.args[0] {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Identifier(id))) => {
                        assert_eq!(id.value, "avg");
                    }
                    other => panic!("expected Identifier arg, got {:?}", other),
                }
            }
            other => panic!("expected Function(ROUND), got {:?}", other),
        }
    }

    #[test]
    fn test_rewrite_agg_refs_cast_avg() {
        // CAST(AVG(salary) AS INT)
        let expr = parse_expr("CAST(AVG(salary) AS INT)");
        let mut agg_map = HashMap::new();
        agg_map.insert("avg(salary)".to_string(), "avg".to_string());
        let group_by: Vec<String> = vec![];

        let rewritten = rewrite_agg_refs_to_columns(&expr, &agg_map, &group_by);
        match rewritten {
            Expr::Cast { expr: inner, .. } => match *inner {
                Expr::Identifier(id) => assert_eq!(id.value, "avg"),
                other => panic!("expected Identifier inside Cast, got {:?}", other),
            },
            other => panic!("expected Cast, got {:?}", other),
        }
    }

    #[test]
    fn test_rewrite_agg_refs_preserves_group_by_ident() {
        // SELECT dept, COUNT(*) FROM t GROUP BY dept — dept should be untouched
        let expr = parse_expr("dept");
        let mut agg_map = HashMap::new();
        agg_map.insert("count(*)".to_string(), "count".to_string());
        let group_by = vec!["dept".to_string()];

        let rewritten = rewrite_agg_refs_to_columns(&expr, &agg_map, &group_by);
        match rewritten {
            Expr::Identifier(id) => assert_eq!(id.value, "dept"),
            other => panic!("expected Identifier('dept'), got {:?}", other),
        }
    }

    #[test]
    fn test_rewrite_agg_refs_literal_unchanged() {
        let expr = parse_expr("'Total'");
        let agg_map = HashMap::new();
        let group_by: Vec<String> = vec![];

        let rewritten = rewrite_agg_refs_to_columns(&expr, &agg_map, &group_by);
        // Should remain a Value literal
        match rewritten {
            Expr::Value(_) => {} // OK
            other => panic!("expected Value literal, got {:?}", other),
        }
    }

    #[test]
    fn test_extract_aggregate_info_nested_round_avg() {
        let query = parse_query("SELECT ROUND(AVG(x), 2) AS rounded_avg FROM t");
        let select = get_select(&query);
        let schema = schema_with_column("x", DataType::Float64);

        let (agg_exprs, names, _types) =
            Executor::extract_aggregate_info(&select.projection, &schema);
        // Should find AVG(x) as a nested aggregate
        assert_eq!(agg_exprs.len(), 1);
        assert!(
            names[0].contains("avg") || names[0].contains("AVG") || names[0] == "rounded_avg",
            "expected aggregate name to relate to avg, got: {:?}",
            names
        );
    }

    #[test]
    fn test_extract_aggregate_info_concat_with_count() {
        let query = parse_query("SELECT 'count=' || COUNT(*) FROM t");
        let select = get_select(&query);
        let schema = schema_with_column("x", DataType::Int32);

        let (agg_exprs, _names, _types) =
            Executor::extract_aggregate_info(&select.projection, &schema);
        // Should find COUNT(*) nested in the BinaryOp
        assert!(
            !agg_exprs.is_empty(),
            "expected at least one aggregate from nested COUNT(*)"
        );
    }
}
