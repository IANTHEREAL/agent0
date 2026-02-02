use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use sqlparser::ast::{
    Expr, Function, FunctionArg, FunctionArgExpr, GroupByExpr, OrderByExpr, Query, SelectItem,
    SetExpr, UnaryOperator, Value as SqlValue,
};
use tikv_client::Transaction;

use super::super::expr::{coerce_text_literal_to_bool, eval_expr};
use super::super::helpers::infer_expr_type;
use super::super::operators::{
    execute_operator_tree, AggregateExpr, BoxedOperator, DistinctOperator, FilterOperator,
    HashAggregateOperator, JoinType, LimitOperator, NestedLoopJoinOperator, PhysicalPlanner,
    ProjectOperator, SortOperator, TableScanOperator, WindowFunctionExpr, WindowOperator,
};
use super::super::ExecuteResult;
use super::core::Executor;
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};
use sqlparser::ast::Ident;

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
                return Err(anyhow!("column reference \"{}\" is ambiguous", col_name));
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
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                        Ok(FunctionArg::Unnamed(FunctionArgExpr::Expr(
                            rewrite_join_expr_with_aliases(
                            &e,
                            left_alias,
                            right_alias,
                            left_schema,
                            right_schema,
                        )?,
                        )))
                    }
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
        Expr::Cast { expr: inner, data_type, format } => Ok(Expr::Cast {
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

fn eval_having_expr_for_operators(
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
                _ => Err(anyhow!("Unsupported unary operator in HAVING: {:?}", op)),
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
                expr: Box::new(super::super::helpers::value_to_sql_expr(&inner_val)),
                data_type: data_type.clone(),
                format: format.clone(),
            };
            eval_expr(&cast_expr, Some(row), Some(schema))
        }
        _ => eval_expr(expr, Some(row), Some(schema)),
    }
}

fn find_matching_aggregate(f: &Function, agg_exprs: &[AggregateExpr]) -> Option<usize> {
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

fn expr_has_function_call(expr: &Expr) -> bool {
    match expr {
        Expr::Function(_) => true,
        Expr::BinaryOp { left, right, .. } => {
            expr_has_function_call(left) || expr_has_function_call(right)
        }
        Expr::UnaryOp { expr, .. } => expr_has_function_call(expr),
        Expr::Nested(e) => expr_has_function_call(e),
        Expr::Between { expr, low, high, .. } => {
            expr_has_function_call(expr) || expr_has_function_call(low) || expr_has_function_call(high)
        }
        Expr::InList { expr, list, .. } => {
            expr_has_function_call(expr) || list.iter().any(expr_has_function_call)
        }
        Expr::IsNull(e) | Expr::IsNotNull(e) => expr_has_function_call(e),
        Expr::IsFalse(e) | Expr::IsTrue(e) | Expr::IsNotFalse(e) | Expr::IsNotTrue(e) => {
            expr_has_function_call(e)
        }
        Expr::Cast { expr, .. } => expr_has_function_call(expr),
        Expr::Case { operand, conditions, results, else_result, .. } => {
            operand.as_ref().map_or(false, |e| expr_has_function_call(e))
                || conditions.iter().any(expr_has_function_call)
                || results.iter().any(expr_has_function_call)
                || else_result.as_ref().map_or(false, |e| expr_has_function_call(e))
        }
        _ => false,
    }
}

impl Executor {
    fn is_simple_projection_expr(expr: &Expr) -> bool {
        match expr {
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => true,
            Expr::Cast { expr, .. } => Self::is_simple_projection_expr(expr),
            Expr::Nested(e) => Self::is_simple_projection_expr(e),
            Expr::Function(_)
            | Expr::AggregateExpressionWithFilter { .. }
            | Expr::ArrayAgg(_)
            | Expr::Subquery(_)
            | Expr::InSubquery { .. }
            | Expr::Exists { .. } => false,
            Expr::BinaryOp { left, right, .. } => {
                Self::is_simple_projection_expr(left) && Self::is_simple_projection_expr(right)
            }
            Expr::UnaryOp { expr, .. } => Self::is_simple_projection_expr(expr),
            Expr::Value(_) => true,
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
            } => {
                operand
                    .as_ref()
                    .map_or(true, |e| Self::is_simple_projection_expr(e))
                    && conditions.iter().all(Self::is_simple_projection_expr)
                    && results.iter().all(Self::is_simple_projection_expr)
                    && else_result
                        .as_ref()
                        .map_or(true, |e| Self::is_simple_projection_expr(e))
            }
            Expr::IsNull(e)
            | Expr::IsNotNull(e)
            | Expr::IsTrue(e)
            | Expr::IsFalse(e)
            | Expr::IsNotTrue(e)
            | Expr::IsNotFalse(e) => Self::is_simple_projection_expr(e),
            _ => false,
        }
    }

    fn validate_projection_columns(expr: &Expr, schema: &TableSchema) -> Result<()> {
        match expr {
            Expr::Identifier(ident) => {
                let col_name = &ident.value;
                if !schema
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(col_name))
                {
                    return Err(anyhow!("column \"{}\" does not exist", col_name));
                }
                Ok(())
            }
            Expr::CompoundIdentifier(parts) => {
                if let Some(col_ident) = parts.last() {
                    let col_name = &col_ident.value;
                    if !schema
                        .columns
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(col_name))
                    {
                        return Err(anyhow!("column \"{}\" does not exist", col_name));
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
            Expr::Value(_) => Ok(()),
            _ => Ok(()),
        }
    }

    pub(crate) fn is_simple_operator_query(query: &Query, select: &sqlparser::ast::Select) -> bool {
        if select.from.len() != 1 {
            return false;
        }
        if !select.from[0].joins.is_empty() {
            return false;
        }
        if query.with.is_some() {
            return false;
        }
        if !matches!(&*query.body, SetExpr::Select(_)) {
            return false;
        }

        if let Some(ref sel) = select.selection {
            if expr_has_function_call(sel) {
                return false;
            }
        }

        let is_valid_projection = select.projection.iter().all(|item| {
            match item {
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => true,
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                    Self::is_simple_projection_expr(e)
                }
            }
        });

        if !is_valid_projection {
            return false;
        }

        if !matches!(
            &select.group_by,
            GroupByExpr::Expressions(exprs) if exprs.is_empty()
        ) {
            return false;
        }

        if select.having.is_some() {
            return false;
        }

        if matches!(&select.distinct, Some(sqlparser::ast::Distinct::On(_))) {
            return false;
        }

        true
    }

    pub(crate) fn is_aggregate_operator_query(query: &Query, select: &sqlparser::ast::Select) -> bool {
        if select.from.len() != 1 {
            return false;
        }
        if !select.from[0].joins.is_empty() {
            return false;
        }
        if query.with.is_some() {
            return false;
        }
        if !matches!(&*query.body, SetExpr::Select(_)) {
            return false;
        }
        if select.distinct.is_some() {
            return false;
        }

        let has_window_funcs = select.projection.iter().any(|item| {
            if let SelectItem::UnnamedExpr(Expr::Function(f))
            | SelectItem::ExprWithAlias {
                expr: Expr::Function(f),
                ..
            } = item
            {
                f.over.is_some()
            } else {
                false
            }
        });

        if has_window_funcs {
            return false;
        }

        let group_by_exprs: Vec<String> = match &select.group_by {
            GroupByExpr::Expressions(exprs) => exprs
                .iter()
                .filter_map(|e| {
                    if let Expr::Identifier(id) = e {
                        Some(id.value.to_lowercase())
                    } else {
                        None
                    }
                })
                .collect(),
            GroupByExpr::All => Vec::new(),
        };

        let is_simple_agg_func = |f: &sqlparser::ast::Function| {
            if f.filter.is_some() {
                return false;
            }
            let func_name = f
                .name
                .0
                .last()
                .map(|n| n.value.to_uppercase())
                .unwrap_or_default();
            matches!(
                func_name.as_str(),
                "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
            )
        };

        let mut has_aggregates = false;
        for item in &select.projection {
            match item {
                SelectItem::UnnamedExpr(Expr::Function(f))
                | SelectItem::ExprWithAlias {
                    expr: Expr::Function(f),
                    ..
                } => {
                    if f.filter.is_some() {
                        return false;
                    }
                    if !is_simple_agg_func(f) {
                        return false;
                    }
                    has_aggregates = true;
                }
                SelectItem::UnnamedExpr(Expr::Identifier(id))
                | SelectItem::ExprWithAlias {
                    expr: Expr::Identifier(id),
                    ..
                } => {
                    if !group_by_exprs.contains(&id.value.to_lowercase()) {
                        return false;
                    }
                }
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                    return false;
                }
                _ => {
                    return false;
                }
            }
        }

        has_aggregates
    }

    pub(crate) fn is_window_operator_query(query: &Query, select: &sqlparser::ast::Select) -> bool {
        if select.from.len() != 1 {
            return false;
        }
        if !select.from[0].joins.is_empty() {
            return false;
        }
        if query.with.is_some() {
            return false;
        }
        if !matches!(&*query.body, SetExpr::Select(_)) {
            return false;
        }

        if !matches!(
            &select.group_by,
            GroupByExpr::Expressions(exprs) if exprs.is_empty()
        ) {
            return false;
        }

        if select.having.is_some() {
            return false;
        }

        let has_window_funcs = select.projection.iter().any(|item| {
            if let SelectItem::UnnamedExpr(Expr::Function(f))
            | SelectItem::ExprWithAlias {
                expr: Expr::Function(f),
                ..
            } = item
            {
                f.over.is_some()
            } else {
                false
            }
        });

        if !has_window_funcs {
            return false;
        }

        let is_supported_window_func = |f: &Function| -> bool {
            let func_name = f
                .name
                .0
                .last()
                .map(|n| n.value.to_lowercase())
                .unwrap_or_default();
            matches!(
                func_name.as_str(),
                "row_number"
                    | "rank"
                    | "dense_rank"
                    | "sum"
                    | "count"
                    | "avg"
                    | "min"
                    | "max"
                    | "lag"
                    | "lead"
                    | "first_value"
                    | "last_value"
            )
        };

        for item in &select.projection {
            match item {
                SelectItem::UnnamedExpr(Expr::Function(f))
                | SelectItem::ExprWithAlias {
                    expr: Expr::Function(f),
                    ..
                } => {
                    if f.over.is_some() && !is_supported_window_func(f) {
                        return false;
                    }
                }
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                    if !Self::is_simple_projection_expr(e) {
                        return false;
                    }
                }
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {}
            }
        }

        true
    }

    fn extract_aggregate_info(
        projection: &[SelectItem],
        schema: &TableSchema,
    ) -> (Vec<AggregateExpr>, Vec<String>, Vec<DataType>) {
        let mut agg_exprs = Vec::new();
        let mut agg_names = Vec::new();
        let mut agg_types = Vec::new();

        for item in projection {
            let (expr, alias) = match item {
                SelectItem::UnnamedExpr(e) => (e, None),
                SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.value.clone())),
                _ => continue,
            };

            if let Expr::Function(f) = expr {
                let func_name = f
                    .name
                    .0
                    .last()
                    .map(|n| n.value.to_uppercase())
                    .unwrap_or_default();

                if matches!(
                    func_name.as_str(),
                    "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
                ) {
                    let arg = f.args.first().and_then(|arg| match arg {
                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e.clone()),
                        FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => None,
                        _ => None,
                    });

                    let name = alias.unwrap_or_else(|| func_name.to_lowercase());

                    let data_type = match func_name.as_str() {
                        "COUNT" => DataType::Int64,
                        "SUM" => {
                            if let Some(ref a) = arg {
                                match infer_expr_type(a, schema) {
                                    DataType::Int32 | DataType::Int64 => DataType::Int64,
                                    DataType::Float64 | DataType::Numeric { .. } => {
                                        DataType::Numeric {
                                            precision: None,
                                            scale: None,
                                        }
                                    }
                                    _ => DataType::Numeric {
                                        precision: None,
                                        scale: None,
                                    },
                                }
                            } else {
                                DataType::Int64
                            }
                        }
                        "AVG" => DataType::Numeric {
                            precision: None,
                            scale: None,
                        },
                        "MIN" | "MAX" => {
                            if let Some(ref a) = arg {
                                infer_expr_type(a, schema)
                            } else {
                                DataType::Text
                            }
                        }
                        "STRING_AGG" => DataType::Text,
                        "ARRAY_AGG" => DataType::Json,
                        _ => DataType::Text,
                    };

                    agg_exprs.push(AggregateExpr {
                        func_name: func_name.clone(),
                        arg,
                        distinct: f.distinct,
                    });
                    agg_names.push(name);
                    agg_types.push(data_type);
                }
            }
        }

        (agg_exprs, agg_names, agg_types)
    }

    fn extract_group_by_info(
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
    ) -> Result<ExecuteResult> {
        let (group_by_exprs, group_by_names, group_by_types) =
            Self::extract_group_by_info(group_by, &schema);
        let (agg_exprs, agg_names, agg_types) = Self::extract_aggregate_info(projection, &schema);
        let group_by_count = group_by_names.len();

        let mut root: BoxedOperator = Box::new(TableScanOperator::new(schema.clone()));

        if let Some(filter_expr) = filter {
            root = Box::new(FilterOperator::new(root, filter_expr.clone()));
        }

        root = Box::new(HashAggregateOperator::new(
            root,
            group_by_exprs,
            agg_exprs.clone(),
            group_by_names.clone(),
            group_by_types.clone(),
            agg_names.clone(),
            agg_types.clone(),
        ));

        let rows = execute_operator_tree(
            &mut root,
            txn,
            self.store(),
            db_id,
            search_path,
            sequence_values,
        )
        .await?;

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

        let columns: Vec<String> = projection
            .iter()
            .map(super::super::helpers::get_select_item_name)
            .collect();

        #[derive(Clone, Copy, Debug)]
        enum ProjectionSource {
            Group(usize),
            Agg(usize),
        }

        let group_len = group_by_names.len();
        let mut sources: Vec<ProjectionSource> = Vec::with_capacity(projection.len());
        let mut column_types: Vec<DataType> = Vec::with_capacity(projection.len());

        let mut agg_idx = 0usize;
        for item in projection {
            match item {
                SelectItem::UnnamedExpr(Expr::Function(_))
                | SelectItem::ExprWithAlias {
                    expr: Expr::Function(_),
                    ..
                } => {
                    sources.push(ProjectionSource::Agg(agg_idx));
                    column_types.push(
                        agg_types
                            .get(agg_idx)
                            .cloned()
                            .ok_or_else(|| anyhow!("Aggregate column out of bounds"))?,
                    );
                    agg_idx += 1;
                }
                SelectItem::UnnamedExpr(Expr::Identifier(id))
                | SelectItem::ExprWithAlias {
                    expr: Expr::Identifier(id),
                    ..
                } => {
                    let group_idx = group_by_names
                        .iter()
                        .position(|n| n.eq_ignore_ascii_case(&id.value))
                        .ok_or_else(|| anyhow!("GROUP BY column '{}' not found", id.value))?;
                    sources.push(ProjectionSource::Group(group_idx));
                    column_types.push(
                        group_by_types
                            .get(group_idx)
                            .cloned()
                            .ok_or_else(|| anyhow!("Group column out of bounds"))?,
                    );
                }
                _ => {
                    return Err(anyhow!(
                        "Unsupported projection for aggregate operator execution"
                    ));
                }
            }
        }

        let mut projected_rows: Vec<Row> = Vec::with_capacity(rows.len());
        for row in rows {
            let mut values: Vec<Value> = Vec::with_capacity(sources.len());
            for source in &sources {
                let idx = match *source {
                    ProjectionSource::Group(i) => i,
                    ProjectionSource::Agg(i) => group_len + i,
                };
                values.push(row.values.get(idx).cloned().unwrap_or(Value::Null));
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

    pub(crate) fn is_simple_join_operator_query(
        query: &Query,
        select: &sqlparser::ast::Select,
    ) -> bool {
        if !use_operator_execution() {
            return false;
        }
        if select.from.len() != 1 {
            return false;
        }
        if select.from[0].joins.len() != 1 {
            return false;
        }
        if query.with.is_some() {
            return false;
        }
        if !matches!(&*query.body, SetExpr::Select(_)) {
            return false;
        }
        if select.distinct.is_some() {
            return false;
        }

        let has_aggregates = select.projection.iter().any(|item| {
            if let SelectItem::UnnamedExpr(Expr::Function(f))
            | SelectItem::ExprWithAlias {
                expr: Expr::Function(f),
                ..
            } = item
            {
                let func_name = f
                    .name
                    .0
                    .last()
                    .map(|n| n.value.to_uppercase())
                    .unwrap_or_default();
                matches!(
                    func_name.as_str(),
                    "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
                )
            } else {
                false
            }
        });

        if has_aggregates {
            return false;
        }

        let has_window_funcs = select.projection.iter().any(|item| {
            if let SelectItem::UnnamedExpr(Expr::Function(f))
            | SelectItem::ExprWithAlias {
                expr: Expr::Function(f),
                ..
            } = item
            {
                f.over.is_some()
            } else {
                false
            }
        });

        if has_window_funcs {
            return false;
        }

        if !matches!(
            &select.group_by,
            GroupByExpr::Expressions(exprs) if exprs.is_empty()
        ) {
            return false;
        }

        if select.having.is_some() {
            return false;
        }

        true
    }

    #[allow(dead_code)]
    pub(crate) async fn execute_join_with_operators(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        left_schema: TableSchema,
        left_rows: Vec<Row>,
        right_schema: TableSchema,
        right_rows: Vec<Row>,
        join_type: JoinType,
        join_condition: Option<Expr>,
        filter: Option<&Expr>,
        order_by: &[OrderByExpr],
        limit: Option<usize>,
        offset: usize,
        projection: &[SelectItem],
    ) -> Result<ExecuteResult> {
        let mut combined_columns: Vec<ColumnDef> = Vec::new();
        for col in &left_schema.columns {
            combined_columns.push(ColumnDef {
                name: col.name.clone(),
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
                name: col.name.clone(),
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

        let left_op: BoxedOperator =
            Box::new(TableScanOperator::new_with_rows(left_schema, left_rows));
        let right_op: BoxedOperator =
            Box::new(TableScanOperator::new_with_rows(right_schema, right_rows));

        let mut root: BoxedOperator = Box::new(NestedLoopJoinOperator::new(
            left_op,
            right_op,
            join_type,
            join_condition,
        ));

        if let Some(filter_expr) = filter {
            root = Box::new(FilterOperator::new(root, filter_expr.clone()));
        }

        if !order_by.is_empty() {
            root = Box::new(SortOperator::new(root, order_by.to_vec()));
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

        let columns: Vec<String> = projection
            .iter()
            .flat_map(|item| match item {
                SelectItem::Wildcard(_) => combined_schema
                    .columns
                    .iter()
                    .map(|c| c.name.clone())
                    .collect(),
                SelectItem::UnnamedExpr(_) => {
                    vec![super::super::helpers::get_select_item_name(item)]
                }
                SelectItem::ExprWithAlias { alias, .. } => vec![alias.value.clone()],
                SelectItem::QualifiedWildcard(_, _) => combined_schema
                    .columns
                    .iter()
                    .map(|c| c.name.clone())
                    .collect(),
            })
            .collect();

        let column_types: Vec<DataType> = projection
            .iter()
            .flat_map(|item| match item {
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => combined_schema
                    .columns
                    .iter()
                    .map(|c| c.data_type.clone())
                    .collect(),
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    vec![infer_expr_type(expr, &combined_schema)]
                }
            })
            .collect();

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows,
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

        let left_op: BoxedOperator = Box::new(TableScanOperator::new(left_schema.clone()));
        let right_op: BoxedOperator = Box::new(TableScanOperator::new(right_schema.clone()));

        let mut root: BoxedOperator = Box::new(NestedLoopJoinOperator::with_schema(
            left_op,
            right_op,
            join_type,
            rewritten_condition,
            combined_schema.clone(),
        ));

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

        let is_wildcard = projection.iter().any(|p| matches!(p, SelectItem::Wildcard(_)));

        let (columns, column_types, projected_rows) = if is_wildcard {
            let cols: Vec<String> = combined_schema
                .columns
                .iter()
                .map(|c| {
                    c.name
                        .split('.')
                        .last()
                        .unwrap_or(&c.name)
                        .to_string()
                })
                .collect();
            let types: Vec<DataType> = combined_schema
                .columns
                .iter()
                .map(|c| c.data_type.clone())
                .collect();
            (cols, types, rows)
        } else {
            let mut rewritten_projection: Vec<SelectItem> = Vec::with_capacity(projection.len());
            for item in projection {
                match item {
                    SelectItem::UnnamedExpr(expr) => rewritten_projection.push(
                        SelectItem::UnnamedExpr(rewrite_join_expr_with_aliases(
                            expr,
                            left_alias,
                            right_alias,
                            &left_schema,
                            &right_schema,
                        )?),
                    ),
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
                .map(|item| super::super::helpers::get_select_item_name(item))
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
        distinct: bool,
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
                    columns.push(super::super::helpers::get_select_item_name(item));
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
                order_by
                    .iter()
                    .map(|o| {
                        let expr = if let Expr::Value(SqlValue::Number(n, _)) = &o.expr {
                            if let Ok(pos) = n.parse::<usize>() {
                                if pos == 0 || pos > columns.len() {
                                    return Err(anyhow!(
                                        "ORDER BY position {} is not in select list",
                                        pos
                                    ));
                                }
                                Expr::Identifier(Ident::new(columns[pos - 1].clone()))
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

        let estimated_rows = 1000;

        if distinct && !is_wildcard_only {
            let scan_operator = planner.plan_simple_select(
                schema.clone(),
                filter,
                Vec::new(),
                None,
                0,
                estimated_rows,
            )?;

            let project_operator = Box::new(ProjectOperator::new(
                scan_operator,
                projection_exprs.clone(),
                columns.clone(),
                column_types.clone(),
            ));

            let distinct_operator = Box::new(DistinctOperator::new(project_operator));

            let mut operator: BoxedOperator = if !order_by.is_empty() {
                let rewritten_order_by = rewrite_order_by_for_post_projection_sort(order_by)?;
                let sort_operator =
                    Box::new(SortOperator::new(distinct_operator, rewritten_order_by));
                if limit.is_some() || offset > 0 {
                    Box::new(LimitOperator::new(sort_operator, limit, offset))
                } else {
                    sort_operator
                }
            } else if limit.is_some() || offset > 0 {
                Box::new(LimitOperator::new(distinct_operator, limit, offset))
            } else {
                distinct_operator
            };

            let rows = execute_operator_tree(
                &mut operator,
                txn,
                self.store(),
                db_id,
                search_path,
                sequence_values,
            )
            .await?;

            return Ok(ExecuteResult::Select {
                columns,
                column_types: Some(column_types),
                rows,
                timezone: crate::session_context::current_timezone(),
            });
        }

        let base_operator = planner.plan_simple_select(
            schema.clone(),
            filter,
            rewrite_order_by_for_pre_projection_sort(order_by)?,
            limit,
            offset,
            estimated_rows,
        )?;

        let mut operator: BoxedOperator = if distinct {
            Box::new(DistinctOperator::new(base_operator))
        } else {
            base_operator
        };

        let raw_rows = execute_operator_tree(
            &mut operator,
            txn,
            self.store(),
            db_id,
            search_path,
            sequence_values,
        )
        .await?;

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
    ) -> Result<ExecuteResult> {
        let planner = PhysicalPlanner::new(self.store(), search_path.to_vec());
        let estimated_rows = 1000;

        let scan_operator = planner.plan_simple_select(
            schema.clone(),
            filter,
            Vec::new(),
            None,
            0,
            estimated_rows,
        )?;

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

        let raw_rows = execute_operator_tree(
            &mut operator,
            txn,
            self.store(),
            db_id,
            search_path,
            sequence_values,
        )
        .await?;

        let (columns, column_types, projected_rows) =
            Self::project_window_results(projection, &schema, &window_funcs, raw_rows)?;

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows: projected_rows,
            timezone: crate::session_context::current_timezone(),
        })
    }

    fn extract_window_function_exprs(
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

    fn infer_window_func_type(
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

    fn project_window_results(
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
                    let name = super::super::helpers::get_select_item_name(item);
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

    #[test]
    fn test_is_simple_query_basic_select() {
        let query = parse_query("SELECT * FROM users WHERE id > 5");
        let select = get_select(&query);
        assert!(Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_simple_query_with_column_projection() {
        let query = parse_query("SELECT id, name FROM users WHERE id > 5");
        let select = get_select(&query);
        assert!(Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_simple_query_with_order_limit() {
        let query = parse_query("SELECT * FROM users ORDER BY id LIMIT 10 OFFSET 5");
        let select = get_select(&query);
        assert!(Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_extract_limit_offset_from_text_literals() {
        let query = parse_query("SELECT * FROM users LIMIT '10' OFFSET '5'");
        assert_eq!(extract_limit(&query), Some(10));
        assert_eq!(extract_offset(&query), 5);
    }

    #[test]
    fn test_is_not_simple_query_with_join() {
        let query = parse_query("SELECT * FROM users u JOIN orders o ON u.id = o.user_id");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_aggregate() {
        let query = parse_query("SELECT COUNT(*) FROM users");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_group_by() {
        let query = parse_query("SELECT status, COUNT(*) FROM users GROUP BY status");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_window() {
        let query = parse_query("SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM users");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_cte() {
        let query = parse_query("WITH t AS (SELECT * FROM users) SELECT * FROM t");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_simple_query_with_distinct() {
        // Plain DISTINCT is now supported via DistinctOperator
        let query = parse_query("SELECT DISTINCT name FROM users");
        let select = get_select(&query);
        assert!(Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_distinct_on() {
        // DISTINCT ON(...) is not yet supported in operator path
        let query = parse_query("SELECT DISTINCT ON (name) name, id FROM users");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_multiple_tables() {
        let query = parse_query("SELECT * FROM users, orders");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_aggregate_query_count() {
        let query = parse_query("SELECT COUNT(*) FROM users");
        let select = get_select(&query);
        assert!(Executor::is_aggregate_operator_query(&query, select));
    }

    #[test]
    fn test_is_aggregate_query_with_group_by() {
        let query = parse_query("SELECT status, COUNT(*) FROM users GROUP BY status");
        let select = get_select(&query);
        assert!(Executor::is_aggregate_operator_query(&query, select));
    }

    #[test]
    fn test_is_aggregate_query_sum_avg() {
        let query = parse_query("SELECT SUM(amount), AVG(amount) FROM orders");
        let select = get_select(&query);
        assert!(Executor::is_aggregate_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_aggregate_query_simple_select() {
        let query = parse_query("SELECT id, name FROM users");
        let select = get_select(&query);
        assert!(!Executor::is_aggregate_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_aggregate_query_with_join() {
        let query = parse_query("SELECT COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id");
        let select = get_select(&query);
        assert!(!Executor::is_aggregate_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_aggregate_query_with_window() {
        let query = parse_query("SELECT id, SUM(amount) OVER (ORDER BY id) FROM orders");
        let select = get_select(&query);
        assert!(!Executor::is_aggregate_operator_query(&query, select));
    }

    #[test]
    fn test_is_simple_join_query() {
        let query = parse_query("SELECT * FROM users u JOIN orders o ON u.id = o.user_id");
        let select = get_select(&query);
        assert!(Executor::is_simple_join_operator_query(&query, select));
    }

    #[test]
    fn test_is_simple_join_query_left_join() {
        let query =
            parse_query("SELECT * FROM users u LEFT JOIN orders o ON u.id = o.user_id");
        let select = get_select(&query);
        assert!(Executor::is_simple_join_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_join_multiple_joins() {
        let query = parse_query(
            "SELECT * FROM users u JOIN orders o ON u.id = o.user_id JOIN items i ON o.id = i.order_id",
        );
        let select = get_select(&query);
        assert!(!Executor::is_simple_join_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_join_with_aggregate() {
        let query =
            parse_query("SELECT u.id, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.id");
        let select = get_select(&query);
        assert!(!Executor::is_simple_join_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_join_no_join() {
        let query = parse_query("SELECT * FROM users");
        let select = get_select(&query);
        assert!(!Executor::is_simple_join_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_aggregate_query_bool_and() {
        let query = parse_query("SELECT BOOL_AND(flag) FROM flags");
        let select = get_select(&query);
        assert!(!Executor::is_aggregate_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_function_call() {
        let query = parse_query("SELECT BOOL_AND(flag) FROM flags");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_upper_function() {
        let query = parse_query("SELECT UPPER(name) FROM users");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_simple_query_with_array_agg() {
        let query = parse_query("SELECT ARRAY_AGG(value ORDER BY value) FROM test");
        let select = get_select(&query);
        assert!(!Executor::is_simple_operator_query(&query, select));
    }

    #[test]
    fn test_is_not_aggregate_query_with_filter_clause() {
        let query = parse_query("SELECT COUNT(*) FILTER (WHERE status = 'completed') FROM orders");
        let select = get_select(&query);
        assert!(!Executor::is_aggregate_operator_query(&query, select));
    }
}
