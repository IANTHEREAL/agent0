//! SELECT query execution

use super::super::aggregate::{collect_having_agg_funcs, eval_having_expr, AggExpr};
use super::super::distinct::{apply_offset_limit_fetch, dedup_rows, distinct_on_rows_with_indices};
use super::super::gin;
use super::super::names;
use super::super::operators::{
    execute_operator_tree, BoxedOperator, FilterOperator, HashAggregateOperator, HashJoinConfig,
    HashJoinOperator, HashJoinType, JoinType, LimitOperator, NestedLoopJoinOperator,
    PhysicalPlanner, SortOperator, TableScanOperator, WindowOperator,
};
use super::super::planner::{self, choose_join_algorithm, JoinAlgorithmChoice, ScanType};
use super::super::projection::{fill_row_defaults, get_select_item_name, infer_expr_type};
use super::super::sequences;
use super::super::value_key::{serialize_value_for_key, serialize_values_for_key};
use super::super::wildcard::build_join_wildcard_plan;
use super::super::window::{compute_window_functions, extract_window_functions, WindowFuncInfo};
use super::super::{
    expr::{coerce_text_literal_to_bool, eval_expr, validate_bool_expr_in_boolean_context},
    Aggregator, ExecuteResult,
};
use super::core::Executor;
use super::operators::rewrite_expr_for_multi_join;
use super::operators::{
    eval_having_expr_for_operators, extract_limit, extract_offset, is_aggregate_func,
    projection_may_have_udf, rewrite_agg_refs_to_columns, use_operator_execution,
};
use crate::sql::error::SqlError;
use crate::sql::information_schema::VirtualTableFilter;
use crate::types::{DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    BinaryOperator, Distinct, Expr, Function, FunctionArg, FunctionArgExpr, GroupByExpr, Ident,
    JoinConstraint, LockType, NonBlock, ObjectName, Query, SelectItem, SetExpr, TableFactor,
    Value as SqlValue,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use tikv_client::Transaction;
use tracing::debug;

static NESTED_JOIN_ALIAS_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn next_nested_join_alias() -> String {
    let id = NESTED_JOIN_ALIAS_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("__tipg_nested_join_{}", id)
}

#[derive(Debug, Clone)]
struct TransparentNestedJoinInfo {
    derived_alias: String,
    inner_aliases: Vec<String>,
    duplicate_cols_lower: HashSet<String>,
}

fn collect_visible_aliases_in_table_with_joins(
    table_with_joins: &sqlparser::ast::TableWithJoins,
) -> Vec<String> {
    fn exposed_name_for_object(name: &ObjectName) -> String {
        names::split_object_name(name)
            .map(|(_, obj)| obj)
            .unwrap_or_else(|_| {
                name.0
                    .last()
                    .map(|ident| ident.value.clone())
                    .unwrap_or_default()
            })
    }

    fn collect_from_factor(factor: &TableFactor, out: &mut Vec<String>) {
        match factor {
            TableFactor::Table { name, alias, .. } => {
                let exposed = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| exposed_name_for_object(name));
                if !exposed.is_empty() {
                    out.push(exposed);
                }
            }
            TableFactor::Derived { alias, .. } => {
                if let Some(a) = alias.as_ref() {
                    out.push(a.name.value.clone());
                }
            }
            TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => {
                if let Some(a) = alias.as_ref() {
                    // An aliased NestedJoin is a derived table; inner aliases must not leak.
                    out.push(a.name.value.clone());
                } else {
                    collect_from_table_with_joins(table_with_joins, out);
                }
            }
            _ => {}
        }
    }

    fn collect_from_table_with_joins(twj: &sqlparser::ast::TableWithJoins, out: &mut Vec<String>) {
        collect_from_factor(&twj.relation, out);
        for join in &twj.joins {
            collect_from_factor(&join.relation, out);
        }
    }

    let mut aliases = Vec::new();
    collect_from_table_with_joins(table_with_joins, &mut aliases);

    let mut seen: HashSet<String> = HashSet::new();
    aliases.retain(|a| seen.insert(a.to_lowercase()));
    aliases
}

fn duplicate_column_names_lowercase(schema: &TableSchema) -> HashSet<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for col in &schema.columns {
        *counts.entry(col.name.to_lowercase()).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .filter_map(|(name, count)| (count > 1).then_some(name))
        .collect()
}

fn extract_virtual_table_filter(expr: &Expr) -> VirtualTableFilter {
    fn column_ref_name(expr: &Expr) -> Option<&str> {
        match expr {
            Expr::Identifier(ident) => Some(ident.value.as_str()),
            Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.as_str()),
            _ => None,
        }
    }

    fn string_literal(expr: &Expr) -> Option<&str> {
        match expr {
            Expr::Value(SqlValue::SingleQuotedString(s))
            | Expr::Value(SqlValue::DoubleQuotedString(s))
            | Expr::Value(SqlValue::NationalStringLiteral(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    fn merge_string_slot(slot: &mut Option<String>, val: &str) {
        match slot {
            None => *slot = Some(val.to_string()),
            Some(existing) => {
                if !existing.eq_ignore_ascii_case(val) {
                    *slot = None;
                }
            }
        }
    }

    fn visit(expr: &Expr, out: &mut VirtualTableFilter, saw_or: &mut bool) {
        match expr {
            Expr::BinaryOp { left, op, right } => {
                if matches!(op, BinaryOperator::Or) {
                    *saw_or = true;
                    return;
                }

                if matches!(op, BinaryOperator::And) {
                    visit(left, out, saw_or);
                    visit(right, out, saw_or);
                    return;
                }

                if matches!(op, BinaryOperator::Eq) {
                    let (col, lit) = if let (Some(col), Some(lit)) =
                        (column_ref_name(left), string_literal(right))
                    {
                        (col, lit)
                    } else if let (Some(col), Some(lit)) =
                        (column_ref_name(right), string_literal(left))
                    {
                        (col, lit)
                    } else {
                        return;
                    };

                    if col.eq_ignore_ascii_case("table_name") || col.eq_ignore_ascii_case("relname")
                    {
                        merge_string_slot(&mut out.table_name, lit);
                    } else if col.eq_ignore_ascii_case("table_schema") {
                        merge_string_slot(&mut out.table_schema, lit);
                    }
                }
            }
            Expr::Nested(inner)
            | Expr::UnaryOp { expr: inner, .. }
            | Expr::Cast { expr: inner, .. }
            | Expr::TryCast { expr: inner, .. } => {
                visit(inner, out, saw_or);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                visit(expr, out, saw_or);
                visit(low, out, saw_or);
                visit(high, out, saw_or);
            }
            Expr::InList { expr, list, .. } => {
                visit(expr, out, saw_or);
                for item in list {
                    visit(item, out, saw_or);
                }
            }
            _ => {}
        }
    }

    let mut out = VirtualTableFilter::default();
    let mut saw_or = false;
    visit(expr, &mut out, &mut saw_or);
    if saw_or {
        VirtualTableFilter::default()
    } else {
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GenerateSeriesOffsetLimitPushdownPlan {
    offset: usize,
    limit: Option<usize>,
    /// When true, the executor must clear query-level OFFSET/LIMIT/FETCH so the slice
    /// is not applied twice.
    clear_query_offset_limit_fetch: bool,
}

fn select_item_expr(item: &SelectItem) -> Option<&Expr> {
    match item {
        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => Some(e),
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => None,
    }
}

/// Recursively check if an expression contains an aggregate function call
/// that is NOT a window function (i.e., no OVER clause).
fn expr_has_non_window_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function(f) => {
            if is_aggregate_func(f) && f.over.is_none() {
                return true;
            }
            f.args.iter().any(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(e),
                    ..
                } => expr_has_non_window_aggregate(e),
                _ => false,
            })
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_has_non_window_aggregate(left) || expr_has_non_window_aggregate(right)
        }
        Expr::UnaryOp { expr: inner, .. }
        | Expr::Nested(inner)
        | Expr::Cast { expr: inner, .. }
        | Expr::TryCast { expr: inner, .. }
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsFalse(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsNotFalse(inner) => expr_has_non_window_aggregate(inner),
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            operand
                .as_ref()
                .map_or(false, |o| expr_has_non_window_aggregate(o))
                || conditions.iter().any(|c| expr_has_non_window_aggregate(c))
                || results.iter().any(|r| expr_has_non_window_aggregate(r))
                || else_result
                    .as_ref()
                    .map_or(false, |e| expr_has_non_window_aggregate(e))
        }
        Expr::InList { expr: e, list, .. } => {
            expr_has_non_window_aggregate(e)
                || list.iter().any(|i| expr_has_non_window_aggregate(i))
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_has_non_window_aggregate(expr)
                || expr_has_non_window_aggregate(low)
                || expr_has_non_window_aggregate(high)
        }
        Expr::ArrayAgg(_) => true,
        _ => false,
    }
}

fn projection_has_non_window_aggregate(projection: &[SelectItem]) -> bool {
    projection
        .iter()
        .filter_map(select_item_expr)
        .any(expr_has_non_window_aggregate)
}

/// Recursively check if an expression contains a subquery (for routing decisions).
fn expr_has_subquery(expr: &Expr) -> bool {
    match expr {
        Expr::Subquery(_) => true,
        Expr::BinaryOp { left, right, .. } => expr_has_subquery(left) || expr_has_subquery(right),
        Expr::UnaryOp { expr: inner, .. }
        | Expr::Nested(inner)
        | Expr::Cast { expr: inner, .. }
        | Expr::TryCast { expr: inner, .. }
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsFalse(inner) => expr_has_subquery(inner),
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            operand.as_ref().map_or(false, |o| expr_has_subquery(o))
                || conditions.iter().any(|c| expr_has_subquery(c))
                || results.iter().any(|r| expr_has_subquery(r))
                || else_result.as_ref().map_or(false, |e| expr_has_subquery(e))
        }
        Expr::Function(f) => f.args.iter().any(|arg| match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
            | FunctionArg::Named {
                arg: FunctionArgExpr::Expr(e),
                ..
            } => expr_has_subquery(e),
            _ => false,
        }),
        Expr::InList { expr: e, list, .. } => {
            expr_has_subquery(e) || list.iter().any(|i| expr_has_subquery(i))
        }
        Expr::InSubquery { .. } => true,
        Expr::Exists { .. } => true,
        _ => false,
    }
}

/// Tracks a column merged by USING or NATURAL JOIN.
/// Per SQL standard, unqualified references resolve to COALESCE(left.col, right.col, ...).
struct UsingMergeColumn {
    col_name: String,
    source_aliases: Vec<String>,
}

/// Build `COALESCE(t1.col, t2.col, ...)` AST expression for a merge column.
fn build_coalesce_for_merge(mc: &UsingMergeColumn) -> Expr {
    let args: Vec<FunctionArg> = mc
        .source_aliases
        .iter()
        .map(|alias| {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::CompoundIdentifier(vec![
                Ident::new(alias.clone()),
                Ident::new(mc.col_name.clone()),
            ])))
        })
        .collect();
    Expr::Function(Function {
        name: ObjectName(vec![Ident::new("COALESCE")]),
        args,
        filter: None,
        null_treatment: None,
        over: None,
        distinct: false,
        special: false,
        order_by: vec![],
    })
}

/// Replace bare-identifier references to merge columns with COALESCE expressions.
/// Qualified references (e.g. `t.col`) are left untouched.
fn replace_using_merge_refs(expr: &Expr, merge_columns: &[UsingMergeColumn]) -> Expr {
    if merge_columns.is_empty() {
        return expr.clone();
    }
    match expr {
        Expr::Identifier(ident) => {
            for mc in merge_columns {
                if mc.col_name.eq_ignore_ascii_case(&ident.value) {
                    return build_coalesce_for_merge(mc);
                }
            }
            expr.clone()
        }
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(replace_using_merge_refs(left, merge_columns)),
            op: op.clone(),
            right: Box::new(replace_using_merge_refs(right, merge_columns)),
        },
        Expr::UnaryOp { op, expr: inner } => Expr::UnaryOp {
            op: op.clone(),
            expr: Box::new(replace_using_merge_refs(inner, merge_columns)),
        },
        Expr::Nested(inner) => {
            Expr::Nested(Box::new(replace_using_merge_refs(inner, merge_columns)))
        }
        Expr::IsNull(inner) => {
            Expr::IsNull(Box::new(replace_using_merge_refs(inner, merge_columns)))
        }
        Expr::IsNotNull(inner) => {
            Expr::IsNotNull(Box::new(replace_using_merge_refs(inner, merge_columns)))
        }
        Expr::Function(f) => {
            let new_args = f
                .args
                .iter()
                .map(|arg| match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => FunctionArg::Unnamed(
                        FunctionArgExpr::Expr(replace_using_merge_refs(e, merge_columns)),
                    ),
                    other => other.clone(),
                })
                .collect();
            let new_order_by = f
                .order_by
                .iter()
                .map(|o| sqlparser::ast::OrderByExpr {
                    expr: replace_using_merge_refs(&o.expr, merge_columns),
                    asc: o.asc,
                    nulls_first: o.nulls_first,
                })
                .collect();
            Expr::Function(Function {
                args: new_args,
                order_by: new_order_by,
                ..f.clone()
            })
        }
        Expr::Cast {
            expr: inner,
            data_type,
            format,
        } => Expr::Cast {
            expr: Box::new(replace_using_merge_refs(inner, merge_columns)),
            data_type: data_type.clone(),
            format: format.clone(),
        },
        Expr::InList {
            expr: inner,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(replace_using_merge_refs(inner, merge_columns)),
            list: list
                .iter()
                .map(|e| replace_using_merge_refs(e, merge_columns))
                .collect(),
            negated: *negated,
        },
        Expr::Between {
            expr: inner,
            negated,
            low,
            high,
        } => Expr::Between {
            expr: Box::new(replace_using_merge_refs(inner, merge_columns)),
            negated: *negated,
            low: Box::new(replace_using_merge_refs(low, merge_columns)),
            high: Box::new(replace_using_merge_refs(high, merge_columns)),
        },
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => Expr::Case {
            operand: operand
                .as_ref()
                .map(|e| Box::new(replace_using_merge_refs(e, merge_columns))),
            conditions: conditions
                .iter()
                .map(|e| replace_using_merge_refs(e, merge_columns))
                .collect(),
            results: results
                .iter()
                .map(|e| replace_using_merge_refs(e, merge_columns))
                .collect(),
            else_result: else_result
                .as_ref()
                .map(|e| Box::new(replace_using_merge_refs(e, merge_columns))),
        },
        _ => expr.clone(),
    }
}

fn rewrite_for_using_join(
    expr: &Expr,
    table_aliases: &[(String, TableSchema)],
    merge_columns: &[UsingMergeColumn],
) -> Result<Expr> {
    if !merge_columns.is_empty() {
        check_using_comma_ambiguity(expr, table_aliases, merge_columns)?;
    }
    let processed = replace_using_merge_refs(expr, merge_columns);
    rewrite_expr_for_multi_join(&processed, table_aliases)
}

fn check_using_comma_ambiguity(
    expr: &Expr,
    table_aliases: &[(String, TableSchema)],
    merge_columns: &[UsingMergeColumn],
) -> Result<()> {
    match expr {
        Expr::Identifier(ident) => {
            let col_name = &ident.value;
            let is_merge = merge_columns
                .iter()
                .any(|mc| mc.col_name.eq_ignore_ascii_case(col_name));
            if is_merge {
                let merge_alias_set: HashSet<String> = merge_columns
                    .iter()
                    .filter(|mc| mc.col_name.eq_ignore_ascii_case(col_name))
                    .flat_map(|mc| mc.source_aliases.iter().cloned())
                    .map(|a| a.to_lowercase())
                    .collect();
                for (alias, schema) in table_aliases {
                    if merge_alias_set.contains(&alias.to_lowercase()) {
                        continue;
                    }
                    if schema
                        .columns
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(col_name))
                    {
                        return Err(SqlError::AmbiguousColumn(col_name.to_string()).into());
                    }
                }
            }
            Ok(())
        }
        Expr::BinaryOp { left, right, .. } => {
            check_using_comma_ambiguity(left, table_aliases, merge_columns)?;
            check_using_comma_ambiguity(right, table_aliases, merge_columns)
        }
        Expr::UnaryOp { expr: inner, .. }
        | Expr::Nested(inner)
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner) => {
            check_using_comma_ambiguity(inner, table_aliases, merge_columns)
        }
        Expr::Function(f) => {
            for arg in &f.args {
                if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                    check_using_comma_ambiguity(e, table_aliases, merge_columns)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn expr_is_select_list_srf(expr: &Expr) -> bool {
    let Expr::Function(f) = expr else {
        return false;
    };
    let Some(name) = f.name.0.last() else {
        return false;
    };
    name.value.eq_ignore_ascii_case("UNNEST")
        || name.value.eq_ignore_ascii_case("REGEXP_SPLIT_TO_TABLE")
        || name.value.eq_ignore_ascii_case("REGEXP_MATCHES")
        || name.value.eq_ignore_ascii_case("JSONB_OBJECT_KEYS")
        || name.value.eq_ignore_ascii_case("JSONB_ARRAY_ELEMENTS")
        || name.value.eq_ignore_ascii_case("JSONB_ARRAY_ELEMENTS_TEXT")
        || name.value.eq_ignore_ascii_case("JSONB_EACH")
        || name.value.eq_ignore_ascii_case("JSONB_EACH_TEXT")
}

fn projection_has_select_list_srf(projection: &[SelectItem]) -> bool {
    projection
        .iter()
        .filter_map(select_item_expr)
        .any(expr_is_select_list_srf)
}

fn expr_is_trivial_projection(expr: &Expr) -> bool {
    match expr {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::Value(_) => true,
        Expr::Nested(inner) => expr_is_trivial_projection(inner),
        _ => false,
    }
}

fn projection_is_trivial(projection: &[SelectItem]) -> bool {
    projection.iter().all(|item| match item {
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => true,
        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
            expr_is_trivial_projection(expr)
        }
    })
}

fn generate_series_offset_limit_pushdown_eligible(
    query: &Query,
    select: &sqlparser::ast::Select,
) -> bool {
    let group_by_is_empty = matches!(
        &select.group_by,
        GroupByExpr::Expressions(exprs) if exprs.is_empty()
    );
    let has_window_funcs = !extract_window_functions(&select.projection).is_empty();
    let has_agg_funcs = {
        let extra_start = select.projection.len();
        let mut agg_funcs: Vec<(usize, AggExpr)> = Vec::new();
        for item in &select.projection {
            match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    collect_having_agg_funcs(expr, &mut agg_funcs, extra_start);
                }
                _ => {}
            }
        }
        !agg_funcs.is_empty()
    };

    let eligible_shape = select.selection.is_none()
        && select.distinct.is_none()
        && group_by_is_empty
        && select.having.is_none()
        && query.order_by.is_empty()
        && !has_window_funcs
        && !has_agg_funcs
        && (query.offset.is_some() || query.limit.is_some() || query.fetch.is_some());
    eligible_shape && !projection_has_select_list_srf(&select.projection)
}

fn normalize_query_offset_limit_fetch_expressions(query: &Query) -> Query {
    let value_to_usize = |v: Value| -> Option<usize> {
        match v {
            Value::Int64(n) if n >= 0 => usize::try_from(n).ok(),
            Value::Int32(n) if n >= 0 => usize::try_from(n).ok(),
            Value::Text(s) => s
                .trim()
                .parse::<i64>()
                .ok()
                .and_then(|n| usize::try_from(n).ok()),
            _ => None,
        }
    };

    let mut q = query.clone();

    if let Some(offset) = q.offset.as_mut() {
        if let Ok(v) = eval_expr(&offset.value, None, None) {
            if let Some(n) = value_to_usize(v) {
                offset.value = Expr::Value(SqlValue::Number(n.to_string(), false));
            }
        }
    }

    if let Some(limit) = q.limit.as_mut() {
        if let Ok(v) = eval_expr(limit, None, None) {
            if let Some(n) = value_to_usize(v) {
                *limit = Expr::Value(SqlValue::Number(n.to_string(), false));
            }
        }
    }

    if let Some(fetch) = q.fetch.as_mut() {
        if let Some(quantity) = fetch.quantity.as_mut() {
            if let Ok(v) = eval_expr(quantity, None, None) {
                if let Some(n) = value_to_usize(v) {
                    *quantity = Expr::Value(SqlValue::Number(n.to_string(), false));
                }
            }
        }
    }

    q
}

fn plan_generate_series_offset_limit_pushdown(
    query: &Query,
    select: &sqlparser::ast::Select,
) -> GenerateSeriesOffsetLimitPushdownPlan {
    if !generate_series_offset_limit_pushdown_eligible(query, select) {
        return GenerateSeriesOffsetLimitPushdownPlan {
            offset: 0,
            limit: None,
            clear_query_offset_limit_fetch: false,
        };
    }

    let value_to_usize = |v: Value| -> Option<usize> {
        match v {
            Value::Int64(n) if n >= 0 => usize::try_from(n).ok(),
            Value::Int32(n) if n >= 0 => usize::try_from(n).ok(),
            Value::Text(s) => s
                .trim()
                .parse::<i64>()
                .ok()
                .and_then(|n| usize::try_from(n).ok()),
            _ => None,
        }
    };

    let mut offset = 0usize;
    if let Some(offset_expr) = &query.offset {
        if let Ok(v) = eval_expr(&offset_expr.value, None, None) {
            offset = value_to_usize(v).unwrap_or(0);
        }
    }

    let mut limit_n = usize::MAX;
    if let Some(limit_expr) = &query.limit {
        if let Ok(v) = eval_expr(limit_expr, None, None) {
            limit_n = value_to_usize(v).unwrap_or(usize::MAX);
        }
    }

    let mut fetch_n = usize::MAX;
    if let Some(fetch) = &query.fetch {
        if let Some(quantity) = &fetch.quantity {
            if let Ok(v) = eval_expr(quantity, None, None) {
                fetch_n = value_to_usize(v).unwrap_or(1);
            }
        } else {
            fetch_n = 1;
        }
    }

    let effective_limit = limit_n.min(fetch_n);

    // OFFSET pushdown is only safe when the projection is trivial, because SQL applies
    // OFFSET after projection and skipped rows must still be evaluated (errors/side effects).
    // Keep this conservative: only allow OFFSET pushdown for identifiers / wildcards /
    // literal values.
    let offset_pushdown_safe = offset == 0 || projection_is_trivial(&select.projection);

    if offset > 0 && !offset_pushdown_safe {
        let limit = if effective_limit == usize::MAX {
            None
        } else {
            Some(offset.saturating_add(effective_limit))
        };
        return GenerateSeriesOffsetLimitPushdownPlan {
            offset: 0,
            limit,
            clear_query_offset_limit_fetch: false,
        };
    }

    let limit = if effective_limit == usize::MAX {
        None
    } else {
        Some(effective_limit)
    };
    GenerateSeriesOffsetLimitPushdownPlan {
        offset,
        limit,
        clear_query_offset_limit_fetch: offset != 0 || limit.is_some(),
    }
}

const JOIN_LOCKING_CLAUSE_UNSUPPORTED: &str =
    "SELECT ... FOR UPDATE/SHARE with JOIN or multiple FROM items is not supported yet";

fn ensure_no_locking_clauses_for_join(query: &Query) -> Result<()> {
    if query.locks.is_empty() {
        return Ok(());
    }
    Err(anyhow!(JOIN_LOCKING_CLAUSE_UNSUPPORTED))
}

fn expand_projection_exprs_for_positional_order_by(
    resolved_projection: &[SelectItem],
    schema: &TableSchema,
) -> Vec<Expr> {
    let mut exprs = Vec::new();
    for item in resolved_projection {
        match item {
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => {
                exprs.extend(
                    schema
                        .columns
                        .iter()
                        .map(|col| Expr::Identifier(sqlparser::ast::Ident::new(col.name.clone()))),
                );
            }
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                exprs.push(expr.clone());
            }
        }
    }
    exprs
}

fn resolve_order_by_exprs_for_non_agg(
    order_by: &[sqlparser::ast::OrderByExpr],
    resolved_projection: &[SelectItem],
    schema: &TableSchema,
) -> Result<Vec<Expr>> {
    let output_exprs = expand_projection_exprs_for_positional_order_by(resolved_projection, schema);

    order_by
        .iter()
        .map(|order_expr| {
            if let Expr::Identifier(ref ident) = order_expr.expr {
                for item in resolved_projection {
                    if let SelectItem::ExprWithAlias { expr, alias } = item {
                        if alias.value.eq_ignore_ascii_case(&ident.value) {
                            return Ok(expr.clone());
                        }
                    }
                }
            }

            if let Expr::Value(SqlValue::Number(n, _)) = &order_expr.expr {
                if let Ok(pos) = n.parse::<usize>() {
                    if pos == 0 || pos > output_exprs.len() {
                        return Err(anyhow!("ORDER BY position {} is not in select list", pos));
                    }
                    return Ok(output_exprs[pos - 1].clone());
                }
            }

            Ok(order_expr.expr.clone())
        })
        .collect()
}

fn resolve_group_by_exprs(
    group_by: &[Expr],
    resolved_projection: &[SelectItem],
    schema: &TableSchema,
) -> Result<Vec<Expr>> {
    let output_exprs = expand_projection_exprs_for_positional_order_by(resolved_projection, schema);

    group_by
        .iter()
        .map(|expr| {
            // Match PostgreSQL-ish behavior: if the name resolves to an input column, prefer it.
            // Otherwise, allow referencing select-list aliases (e.g. `SELECT ... AS day GROUP BY day`).
            if let Expr::Identifier(ref ident) = expr {
                let exists_in_schema = schema
                    .columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(&ident.value));

                if !exists_in_schema {
                    for item in resolved_projection {
                        if let SelectItem::ExprWithAlias { expr, alias } = item {
                            if alias.value.eq_ignore_ascii_case(&ident.value) {
                                return Ok(expr.clone());
                            }
                        }
                    }
                }
            }

            // Positional GROUP BY (e.g. `GROUP BY 1`).
            if let Expr::Value(SqlValue::Number(n, _)) = expr {
                if let Ok(pos) = n.parse::<usize>() {
                    if pos == 0 || pos > output_exprs.len() {
                        return Err(anyhow!("GROUP BY position {} is not in select list", pos));
                    }
                    return Ok(output_exprs[pos - 1].clone());
                }
            }

            Ok(expr.clone())
        })
        .collect()
}

impl Executor {
    pub(crate) fn execute_query_with_outer_ctes<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        query: &'a Query,
        outer_ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExecuteResult>> + Send + 'a>>
    {
        Box::pin(async move {
            if query.with.is_none() {
                return self
                    .execute_query_with_ctes(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        query,
                        outer_ctes,
                    )
                    .await;
            }

            let merged_ctes = self
                .build_cte_context_with_base(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    query,
                    outer_ctes,
                )
                .await?;
            self.execute_query_with_ctes(
                txn,
                db_id,
                sequence_values,
                search_path,
                query,
                &merged_ctes,
            )
            .await
        })
    }

    pub(crate) async fn execute_query_with_ctes(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        if let SetExpr::SetOperation {
            op,
            set_quantifier,
            left,
            right,
        } = &*query.body
        {
            let result = self
                .execute_set_operation(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    op,
                    set_quantifier,
                    left,
                    right,
                    ctes,
                )
                .await?;

            // ORDER BY / LIMIT / OFFSET apply to the full set-operation result.
            if let ExecuteResult::Select {
                columns,
                column_types,
                rows,
                timezone,
            } = result
            {
                let mut rows = rows;
                if !query.order_by.is_empty() {
                    rows = self.apply_order_by_for_aggregate(rows, &query.order_by, &columns);
                }
                rows = apply_offset_limit_fetch(rows, query);
                return Ok(ExecuteResult::Select {
                    columns,
                    column_types,
                    rows,
                    timezone,
                });
            }

            return Ok(result);
        }

        if let SetExpr::Values(values) = &*query.body {
            let store = self.store();
            let mut column_count: Option<usize> = None;
            let mut rows = Vec::with_capacity(values.rows.len());
            for expr_row in &values.rows {
                let expr_len = expr_row.len();
                if let Some(expected) = column_count {
                    if expr_len != expected {
                        return Err(anyhow!("VALUES lists must all be the same length"));
                    }
                } else {
                    column_count = Some(expr_len);
                }

                let mut row_values = Vec::with_capacity(expr_len);
                for expr in expr_row {
                    let resolved = self
                        .resolve_subqueries(txn, db_id, sequence_values, search_path, expr, ctes)
                        .await?;
                    let value = sequences::eval_expr_with_sequences(
                        &store,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &resolved,
                        None,
                        None,
                    )
                    .await?;
                    row_values.push(value);
                }
                rows.push(Row::new(row_values));
            }

            let column_count = column_count.unwrap_or(0);
            let columns: Vec<String> = (1..=column_count)
                .map(|idx| format!("column{}", idx))
                .collect();
            let mut rows = rows;

            if !query.order_by.is_empty() {
                rows = self.apply_order_by_for_aggregate(rows, &query.order_by, &columns);
            }
            rows = apply_offset_limit_fetch(rows, query);

            return Ok(ExecuteResult::Select {
                column_types: None,
                columns,
                rows,
                timezone: crate::session_context::current_timezone(),
            });
        }

        let select = match &*query.body {
            SetExpr::Select(s) => s,
            _ => return Err(anyhow!("Only SELECT supported")),
        };

        let select_into_target = select
            .into
            .as_ref()
            .map(|into| (into.name.clone(), into.temporary));

        if select.from.is_empty() {
            let result = self
                .execute_tableless_query(txn, db_id, sequence_values, search_path, select, ctes)
                .await?;
            if let Some((target_name, _temp)) = select_into_target {
                return self
                    .create_table_from_result(txn, db_id, search_path, &target_name, result)
                    .await;
            }
            return Ok(result);
        }

        let has_joins = !select.from[0].joins.is_empty() || select.from.len() > 1;

        if has_joins {
            ensure_no_locking_clauses_for_join(query)?;
            if let Some(result) = self
                .try_execute_simple_join_with_operators(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    query,
                    select,
                    ctes,
                )
                .await?
            {
                if let Some((target_name, _temp)) = select_into_target {
                    return self
                        .create_table_from_result(txn, db_id, search_path, &target_name, result)
                        .await;
                }
                return Ok(result);
            }

            return Err(anyhow!(
                "JOIN query could not be executed: unsupported table factor or schema not found"
            ));
        }

        let mut generate_series_offset_limit_pushed_down = false;
        let mut query_with_evaluated_offset_limit_fetch: Option<Query> = None;
        let (t, outer_alias, schema, all_rows_base, is_virtual, rows_loaded) = match &select.from[0]
            .relation
        {
            TableFactor::Table {
                name, alias, args, ..
            } => {
                let (schema_opt, obj_name) = names::split_object_name(name)?;
                let tbl_upper = obj_name.to_uppercase();

                // Handle GENERATE_SERIES as a table-valued function
                if tbl_upper == "GENERATE_SERIES" {
                    if let Some(func_args) = args {
                        let als = alias
                            .as_ref()
                            .map(|a| a.name.value.clone())
                            .unwrap_or_else(|| obj_name.clone());
                        let query_for_pushdown =
                            if generate_series_offset_limit_pushdown_eligible(query, select) {
                                query_with_evaluated_offset_limit_fetch =
                                    Some(normalize_query_offset_limit_fetch_expressions(query));
                                query_with_evaluated_offset_limit_fetch.as_ref().unwrap()
                            } else {
                                query
                            };
                        let pushdown =
                            plan_generate_series_offset_limit_pushdown(query_for_pushdown, select);
                        let offset = pushdown.offset;
                        let limit = pushdown.limit;
                        generate_series_offset_limit_pushed_down =
                            pushdown.clear_query_offset_limit_fetch;

                        let (schema, rows) = self
                            .execute_generate_series(func_args, &als, alias.as_ref(), offset, limit)
                            .await?;
                        (schema.name.clone(), als, schema, rows, true, true)
                    } else {
                        return Err(anyhow!("generate_series requires at least 2 arguments"));
                    }
                } else {
                    if let Some(func_args) = args {
                        if let Some((schema, rows)) = self
                            .try_execute_extension_table_function(
                                txn,
                                db_id,
                                search_path,
                                name,
                                func_args,
                                alias.as_ref(),
                            )
                            .await?
                        {
                            let alias_str = alias
                                .as_ref()
                                .map(|a| a.name.value.clone())
                                .unwrap_or_else(|| obj_name.clone());
                            (schema.name.clone(), alias_str, schema, rows, true, true)
                        } else if let Some((schema, rows)) = self
                            .try_execute_user_table_function(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                name,
                                func_args,
                                alias.as_ref(),
                            )
                            .await?
                        {
                            let alias_str = alias
                                .as_ref()
                                .map(|a| a.name.value.clone())
                                .unwrap_or_else(|| obj_name.clone());
                            (schema.name.clone(), alias_str, schema, rows, true, true)
                        } else {
                            let lookup_name = match schema_opt {
                                Some(schema) => format!("{}.{}", schema, obj_name),
                                None => obj_name.clone(),
                            };
                            let (schema, rows) = self
                                .get_table_data(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &lookup_name,
                                    ctes,
                                )
                                .await?;
                            let is_virtual = schema.table_id == 0;
                            let alias_str = alias
                                .as_ref()
                                .map(|a| a.name.value.clone())
                                .unwrap_or_else(|| obj_name.clone());
                            (
                                schema.name.clone(),
                                alias_str,
                                schema,
                                rows,
                                is_virtual,
                                true,
                            )
                        }
                    } else {
                        let alias_str = alias
                            .as_ref()
                            .map(|a| a.name.value.clone())
                            .unwrap_or_else(|| obj_name.clone());

                        // Prefer loading schema only for base tables, so we can use indexes
                        // without first scanning the full table.
                        let cte_key = obj_name.to_lowercase();
                        if let Some((cte_schema, cte_rows)) = ctes.get(&cte_key) {
                            (
                                cte_schema.name.clone(),
                                alias_str,
                                cte_schema.clone(),
                                cte_rows.clone(),
                                true,
                                true,
                            )
                        } else if let Some(resolved) = names::resolve_existing_table_name(
                            self.store().as_ref(),
                            txn,
                            db_id,
                            name,
                            search_path,
                        )
                        .await?
                        {
                            let schema = self
                                .store()
                                .get_schema(txn, db_id, &resolved.full)
                                .await?
                                .ok_or_else(|| SqlError::RelationNotFound(resolved.full.clone()))?;
                            (
                                schema.name.clone(),
                                alias_str,
                                schema,
                                Vec::new(),
                                false,
                                false,
                            )
                        } else {
                            let lookup_name = match schema_opt {
                                Some(schema) => format!("{}.{}", schema, obj_name),
                                None => obj_name.clone(),
                            };
                            let (schema, rows) = self
                                .get_table_data(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &lookup_name,
                                    ctes,
                                )
                                .await?;
                            let is_virtual = schema.table_id == 0;
                            (
                                schema.name.clone(),
                                alias_str,
                                schema,
                                rows,
                                is_virtual,
                                true,
                            )
                        }
                    }
                }
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let alias_name = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| "subquery".to_string());
                let alias_columns = alias.as_ref().map(|a| a.columns.as_slice()).unwrap_or(&[]);
                let (schema, rows) = self
                    .execute_derived_table(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        subquery,
                        &alias_name,
                        alias_columns,
                        ctes,
                    )
                    .await?;
                (alias_name.clone(), alias_name, schema, rows, true, true)
            }
            _ => return Err(SqlError::Unsupported("Unsupported table".into()).into()),
        };

        let is_from_cte = ctes.contains_key(&t.to_lowercase());
        let (is_virtual, rows_loaded) = if is_from_cte && use_operator_execution() {
            (false, false)
        } else {
            (is_virtual, rows_loaded)
        };

        let has_correlated_exists = select
            .selection
            .as_ref()
            .map(|sel| self.expr_has_correlated_exists(sel, &outer_alias))
            .unwrap_or(false);

        let resolved_selection = if let Some(sel) = &select.selection {
            if has_correlated_exists {
                Some(sel.clone())
            } else {
                Some(
                    self.resolve_subqueries(txn, db_id, sequence_values, search_path, sel, ctes)
                        .await?,
                )
            }
        } else {
            None
        };

        if let Some(sel) = resolved_selection.as_ref() {
            validate_bool_expr_in_boolean_context(
                sel,
                &schema,
                "Filter predicate must evaluate to boolean",
            )?;
        }

        let resolved_projection = self
            .resolve_projection_subqueries_with_outer_context(
                txn,
                db_id,
                sequence_values,
                search_path,
                &select.projection,
                &outer_alias,
                ctes,
            )
            .await?;

        let has_for_update = query
            .locks
            .iter()
            .any(|l| matches!(l.lock_type, LockType::Update));

        // Detect grouping sets (CUBE/ROLLUP/GROUPING SETS) early — not yet supported by operators.
        let group_by_exprs_for_grouping_sets_check = match &select.group_by {
            GroupByExpr::Expressions(exprs) => exprs.as_slice(),
            GroupByExpr::All => &[][..],
        };
        let has_grouping_sets =
            extract_grouping_sets(group_by_exprs_for_grouping_sets_check).is_some();

        let has_udf = projection_may_have_udf(&resolved_projection)
            || resolved_selection
                .as_ref()
                .map_or(false, |sel| super::operators::expr_may_have_udf_pub(sel));

        let has_scalar_subquery = resolved_projection.iter().any(|item| match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                expr_has_subquery(e)
            }
            _ => false,
        }) || resolved_selection
            .as_ref()
            .map_or(false, |sel| expr_has_subquery(sel));

        if use_operator_execution()
            && !has_correlated_exists
            && !has_scalar_subquery
            && !has_udf
            && query.locks.is_empty()
            && !(has_for_update && schema.pk_indices.is_empty())
            && select_into_target.is_none()
            && !has_grouping_sets
            && matches!(&*query.body, SetExpr::Select(_))
        {
            // Preloaded rows: virtual tables, materialized views, CTEs, derived tables,
            // generate_series results already have all rows in memory.
            let preloaded_rows = if is_virtual || rows_loaded {
                Some(all_rows_base)
            } else {
                None
            };

            if has_for_update {
                let planner = PhysicalPlanner::new(self.store(), search_path.to_vec());
                let estimated_rows = 1000;
                let mut lock_operator = planner.plan_simple_select(
                    schema.clone(),
                    resolved_selection.as_ref(),
                    Vec::new(),
                    None,
                    0,
                    estimated_rows,
                )?;
                let lock_rows = execute_operator_tree(
                    &mut lock_operator,
                    txn,
                    self.store(),
                    db_id,
                    search_path,
                    sequence_values,
                )
                .await?;
                if !lock_rows.is_empty() {
                    self.store().lock_rows(txn, db_id, &t, &lock_rows).await?;
                }
            }

            let has_window = extract_window_functions(&resolved_projection)
                .iter()
                .any(|_| true);
            let has_agg_or_group_by = projection_has_non_window_aggregate(&resolved_projection)
                || !matches!(
                    &select.group_by,
                    GroupByExpr::Expressions(exprs) if exprs.is_empty()
                )
                || select.having.is_some();

            if has_window && !has_agg_or_group_by {
                return self
                    .execute_window_with_operators(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        schema,
                        resolved_selection.as_ref(),
                        &query.order_by,
                        super::operators::extract_limit(query),
                        super::operators::extract_offset(query),
                        &resolved_projection,
                        ctes,
                        preloaded_rows,
                    )
                    .await;
            } else if has_agg_or_group_by {
                let resolved_group_by = match &select.group_by {
                    GroupByExpr::Expressions(exprs) => GroupByExpr::Expressions(
                        resolve_group_by_exprs(exprs, &resolved_projection, &schema)?,
                    ),
                    GroupByExpr::All => GroupByExpr::All,
                };
                return self
                    .execute_aggregate_with_operators(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        schema,
                        resolved_selection.as_ref(),
                        &resolved_group_by,
                        select.having.as_ref(),
                        &query.order_by,
                        super::operators::extract_limit(query),
                        super::operators::extract_offset(query),
                        &resolved_projection,
                        ctes,
                        preloaded_rows,
                    )
                    .await;
            } else {
                return self
                    .execute_with_operators(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        schema,
                        resolved_selection.as_ref(),
                        &query.order_by,
                        super::operators::extract_limit(query),
                        super::operators::extract_offset(query),
                        &resolved_projection,
                        select.distinct.as_ref(),
                        ctes,
                        preloaded_rows,
                    )
                    .await;
            }
        }

        let pkless_for_update = has_for_update && schema.pk_indices.is_empty();
        if pkless_for_update && (is_virtual || rows_loaded) {
            return Err(anyhow!(
                "FOR UPDATE not supported for '{}': table has no primary key",
                t
            ));
        }

        let mut pkless_row_keys: Option<Vec<Vec<u8>>> = None;

        let all_rows = if is_virtual {
            all_rows_base
        } else if rows_loaded {
            // Rows are already materialized (CTE, derived table, view, etc). Index scans
            // are only applicable to base tables.
            all_rows_base
        } else if pkless_for_update {
            let mut rows_with_keys = self.store().scan_with_keys(txn, db_id, &t).await?;
            for (_, row) in &mut rows_with_keys {
                fill_row_defaults(row, &schema)?;
            }
            let (keys, rows) = rows_with_keys.into_iter().unzip();
            pkless_row_keys = Some(keys);
            rows
        } else {
            let estimated_rows = 1000;

            match &resolved_selection {
                None => {
                    let scan_upper_bound = {
                        let limit = super::operators::extract_limit(query);
                        let offset = super::operators::extract_offset(query);
                        match limit {
                            Some(0) => Some(0),
                            Some(n) => Some(offset.saturating_add(n)),
                            None => None,
                        }
                    };

                    let has_for_update = query
                        .locks
                        .iter()
                        .any(|l| matches!(l.lock_type, LockType::Update));

                    let can_pushdown_scan_limit = scan_upper_bound.is_some()
                        && !has_for_update
                        && select.distinct.is_none()
                        && query.order_by.is_empty()
                        && matches!(
                            &select.group_by,
                            GroupByExpr::Expressions(exprs) if exprs.is_empty()
                        )
                        && select.having.is_none()
                        && select.from.len() == 1
                        && select.from[0].joins.is_empty()
                        && extract_window_functions(&select.projection).is_empty();

                    let scan_upper_bound = if can_pushdown_scan_limit {
                        scan_upper_bound
                    } else {
                        None
                    };

                    self.scan_and_fill_with_limit(txn, db_id, &t, &schema, scan_upper_bound)
                        .await?
                }
                Some(sel) => {
                    let access_path = planner::choose_best_access_path_for_filter(
                        &schema,
                        Some(sel),
                        estimated_rows,
                    );

                    match access_path.scan_type {
                        ScanType::GinIndexScan {
                            index_id,
                            ref index_name,
                            ref column,
                            ref pattern,
                            ..
                        } => {
                            let index = schema.indexes.iter().find(|i| i.id == index_id);
                            let Some(idx) = index else {
                                return Err(anyhow!("GIN index not found"));
                            };

                            let gin_col_type = schema
                                .columns
                                .iter()
                                .find(|c| c.name.eq_ignore_ascii_case(column))
                                .map(|c| &c.data_type);

                            let token_hashes = match &pattern {
                                Value::Null => Vec::new(),
                                Value::Array(arr) => gin::extract_array_gin_tokens(arr),
                                Value::Tsquery(s) => gin::extract_tsquery_gin_tokens(s),
                                Value::Tsvector(s) => gin::extract_tsvector_gin_tokens(s),
                                Value::Json(s) | Value::Jsonb(s) => {
                                    let pattern_json: serde_json::Value = serde_json::from_str(s)
                                        .map_err(|e| {
                                        anyhow!("Invalid JSONB pattern for @>: {}", e)
                                    })?;
                                    gin::extract_gin_tokens(&pattern_json).into_scan_hashes()
                                }
                                Value::Text(s) => match gin_col_type {
                                    Some(DataType::Tsvector) => gin::extract_tsquery_gin_tokens(s),
                                    Some(DataType::Array(_)) => {
                                        let parsed: Vec<Value> =
                                            serde_json::from_str(s).unwrap_or_default();
                                        gin::extract_array_gin_tokens(&parsed)
                                    }
                                    _ => {
                                        let pattern_json: serde_json::Value =
                                            serde_json::from_str(s).map_err(|e| {
                                                anyhow!("Invalid JSONB pattern for @>: {}", e)
                                            })?;
                                        gin::extract_gin_tokens(&pattern_json).into_scan_hashes()
                                    }
                                },
                                other => {
                                    return Err(anyhow!(
                                        "GIN pattern must be array, json/jsonb, or tsquery, got {}",
                                        other.data_type().unwrap_or(DataType::Text)
                                    ));
                                }
                            };

                            if token_hashes.is_empty() {
                                debug!(
                                    "GIN predicate yields no tokens; falling back to full scan (index: {})",
                                    index_name
                                );
                                self.scan_and_fill(txn, db_id, &t, &schema).await?
                            } else {
                                debug!(
                                    "Using GIN Index Scan on {} (cost: {:.2})",
                                    index_name, access_path.cost
                                );
                                let pk_keys = self
                                    .store()
                                    .scan_gin_index_intersection(
                                        txn,
                                        db_id,
                                        schema.table_id,
                                        idx.id,
                                        &token_hashes,
                                    )
                                    .await?;
                                let mut rows = self
                                    .store()
                                    .batch_get_rows_by_pk_keys(txn, db_id, schema.table_id, pk_keys)
                                    .await?;
                                for r in &mut rows {
                                    fill_row_defaults(r, &schema)?;
                                }
                                rows
                            }
                        }
                        ScanType::IndexScan {
                            index_id,
                            ref index_name,
                            ref values,
                            ..
                        } => {
                            let pk_types: Vec<DataType> = if schema.pk_indices.is_empty() {
                                vec![DataType::Uuid]
                            } else {
                                schema
                                    .pk_indices
                                    .iter()
                                    .map(|&idx| schema.columns[idx].data_type.clone())
                                    .collect()
                            };

                            let index = schema.indexes.iter().find(|i| i.id == index_id);
                            let Some(idx) = index else {
                                return Err(anyhow!("Index not found"));
                            };

                            debug!(
                                "Using Index Scan on {} (cost: {:.2})",
                                index_name, access_path.cost
                            );

                            let pks = self
                                .store()
                                .scan_index(
                                    txn,
                                    db_id,
                                    schema.table_id,
                                    idx.id,
                                    values,
                                    idx.unique,
                                    &pk_types,
                                    None,
                                )
                                .await?;
                            let mut rows = self
                                .store()
                                .batch_get_rows(txn, db_id, schema.table_id, pks, &schema)
                                .await?;
                            for r in &mut rows {
                                fill_row_defaults(r, &schema)?;
                            }
                            rows
                        }
                        ScanType::IndexRangeScan {
                            index_id,
                            ref index_name,
                            ref prefix_values,
                            ..
                        } => {
                            let pk_types: Vec<DataType> = if schema.pk_indices.is_empty() {
                                vec![DataType::Uuid]
                            } else {
                                schema
                                    .pk_indices
                                    .iter()
                                    .map(|&idx| schema.columns[idx].data_type.clone())
                                    .collect()
                            };

                            let index = schema.indexes.iter().find(|i| i.id == index_id);
                            let Some(idx) = index else {
                                return Err(anyhow!("Index not found"));
                            };

                            let index_column_types: Vec<_> = idx
                                .columns
                                .iter()
                                .map(|col| {
                                    schema
                                        .columns
                                        .iter()
                                        .find(|c| c.name.eq_ignore_ascii_case(col))
                                        .map(|c| c.data_type.clone())
                                        .ok_or_else(|| anyhow!("Index column '{}' not found", col))
                                })
                                .collect::<Result<Vec<_>>>()?;

                            debug!(
                                "Using Index Range Scan on {} (cost: {:.2})",
                                index_name, access_path.cost
                            );

                            let pks = self
                                .store()
                                .scan_index_prefix(
                                    txn,
                                    db_id,
                                    schema.table_id,
                                    idx.id,
                                    prefix_values,
                                    idx.unique,
                                    &index_column_types,
                                    &pk_types,
                                    None,
                                )
                                .await?;
                            let mut rows = self
                                .store()
                                .batch_get_rows(txn, db_id, schema.table_id, pks, &schema)
                                .await?;
                            for r in &mut rows {
                                fill_row_defaults(r, &schema)?;
                            }
                            rows
                        }
                        ScanType::FullTableScan => {
                            debug!("Using Full Table Scan (cost: {:.2})", access_path.cost);
                            self.scan_and_fill(txn, db_id, &t, &schema).await?
                        }
                    }
                }
            }
        };

        let (filtered_rows, lock_keys) = if pkless_for_update {
            let all_keys = pkless_row_keys
                .take()
                .ok_or_else(|| anyhow!("missing row keys for FOR UPDATE"))?;

            if let Some(ref sel) = resolved_selection {
                let mut rows = Vec::new();
                let mut keys = Vec::new();
                if has_correlated_exists {
                    for (key, r) in all_keys.into_iter().zip(all_rows.into_iter()) {
                        let result = self
                            .eval_selection_with_correlated_exists(
                                txn,
                                db_id,
                                sequence_values,
                                sel,
                                search_path,
                                &outer_alias,
                                &schema,
                                &r,
                            )
                            .await?;
                        let result = coerce_text_literal_to_bool(sel, result)?;
                        match result {
                            Value::Boolean(true) => {
                                rows.push(r);
                                keys.push(key);
                            }
                            Value::Boolean(false) | Value::Null => {}
                            other => {
                                return Err(anyhow!(
                                    "Filter predicate must evaluate to boolean, got {:?}",
                                    other
                                ));
                            }
                        }
                    }
                } else {
                    for (key, r) in all_keys.into_iter().zip(all_rows.into_iter()) {
                        let result = self
                            .eval_expr_maybe_sequence(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                sel,
                                Some(&r),
                                Some(&schema),
                            )
                            .await?;
                        let result = coerce_text_literal_to_bool(sel, result)?;
                        match result {
                            Value::Boolean(true) => {
                                rows.push(r);
                                keys.push(key);
                            }
                            Value::Boolean(false) | Value::Null => {}
                            other => {
                                return Err(anyhow!(
                                    "Filter predicate must evaluate to boolean, got {:?}",
                                    other
                                ));
                            }
                        }
                    }
                }
                (rows, keys)
            } else {
                (all_rows, all_keys)
            }
        } else if let Some(ref sel) = resolved_selection {
            let mut v = Vec::new();
            if has_correlated_exists {
                for r in all_rows {
                    let result = self
                        .eval_selection_with_correlated_exists(
                            txn,
                            db_id,
                            sequence_values,
                            sel,
                            search_path,
                            &outer_alias,
                            &schema,
                            &r,
                        )
                        .await?;
                    let result = coerce_text_literal_to_bool(sel, result)?;
                    match result {
                        Value::Boolean(true) => v.push(r),
                        Value::Boolean(false) | Value::Null => {}
                        other => {
                            return Err(anyhow!(
                                "Filter predicate must evaluate to boolean, got {:?}",
                                other
                            ));
                        }
                    }
                }
            } else {
                for r in all_rows {
                    let result = self
                        .eval_expr_maybe_sequence(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            sel,
                            Some(&r),
                            Some(&schema),
                        )
                        .await?;
                    let result = coerce_text_literal_to_bool(sel, result)?;
                    match result {
                        Value::Boolean(true) => v.push(r),
                        Value::Boolean(false) | Value::Null => {}
                        other => {
                            return Err(anyhow!(
                                "Filter predicate must evaluate to boolean, got {:?}",
                                other
                            ));
                        }
                    }
                }
            }
            (v, Vec::new())
        } else {
            (all_rows, Vec::new())
        };

        if has_for_update && pkless_for_update && !filtered_rows.is_empty() {
            txn.lock_keys(lock_keys).await.map_err(|e| anyhow!(e))?;
        }

        let group_keys_exprs = match &select.group_by {
            GroupByExpr::Expressions(exprs) => exprs,
            GroupByExpr::All => {
                return Err(SqlError::Unsupported("GROUP BY ALL not supported".into()).into())
            }
        };

        let resolved_group_keys_exprs =
            resolve_group_by_exprs(group_keys_exprs, &resolved_projection, &schema)?;
        let group_keys_exprs = resolved_group_keys_exprs.as_slice();

        let grouping_sets = extract_grouping_sets(group_keys_exprs);
        let has_grouping_sets = grouping_sets.is_some();

        let window_funcs = extract_window_functions(&select.projection);

        let mut agg_funcs: Vec<(usize, AggExpr)> = Vec::new();
        let extra_start = select.projection.len();
        for (i, item) in select.projection.iter().enumerate() {
            match item {
                SelectItem::UnnamedExpr(Expr::Function(f))
                | SelectItem::ExprWithAlias {
                    expr: Expr::Function(f),
                    ..
                } => {
                    if f.over.is_none() {
                        let func_name = f
                            .name
                            .0
                            .last()
                            .map(|n| n.value.to_uppercase())
                            .unwrap_or_default();
                        if matches!(
                            func_name.as_str(),
                            "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" | "STRING_AGG" | "ARRAY_AGG"
                        ) {
                            agg_funcs.push((i, AggExpr::Function(f.clone())));
                        } else {
                            collect_having_agg_funcs(
                                &Expr::Function(f.clone()),
                                &mut agg_funcs,
                                extra_start,
                            );
                        }
                    }
                }
                SelectItem::UnnamedExpr(Expr::ArrayAgg(arr))
                | SelectItem::ExprWithAlias {
                    expr: Expr::ArrayAgg(arr),
                    ..
                } => {
                    agg_funcs.push((i, AggExpr::ArrayAgg(arr.clone())));
                }
                // Handle expressions containing nested aggregates (e.g., 'X=' || count(*))
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    collect_having_agg_funcs(expr, &mut agg_funcs, extra_start);
                }
                _ => {}
            }
        }

        if let Some(having_expr) = &select.having {
            collect_having_agg_funcs(having_expr, &mut agg_funcs, extra_start);
        }

        let is_agg = !group_keys_exprs.is_empty() || !agg_funcs.is_empty();

        if is_agg {
            if has_grouping_sets {
                return self
                    .execute_grouping_sets_query(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        query,
                        select,
                        &schema,
                        filtered_rows,
                        grouping_sets.unwrap(),
                        agg_funcs,
                        &resolved_projection,
                        select_into_target,
                    )
                    .await;
            }
            return self
                .execute_aggregate_query(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    query,
                    select,
                    &schema,
                    filtered_rows,
                    group_keys_exprs,
                    agg_funcs,
                    &resolved_projection,
                    select_into_target,
                )
                .await;
        }

        let window_results = if !window_funcs.is_empty() {
            Some(compute_window_functions(
                &filtered_rows,
                &schema,
                &window_funcs,
            )?)
        } else {
            None
        };

        let output_exprs_for_order_by =
            expand_projection_exprs_for_positional_order_by(&resolved_projection, &schema);

        let order_by_references_correlated_subquery = query.order_by.iter().any(|order_expr| {
            if let Expr::Identifier(ref ident) = order_expr.expr {
                for item in &resolved_projection {
                    if let SelectItem::ExprWithAlias { expr, alias } = item {
                        if alias.value.eq_ignore_ascii_case(&ident.value) {
                            if let Expr::Subquery(_) = expr {
                                return true;
                            }
                        }
                    }
                }
                return false;
            }

            if let Expr::Value(SqlValue::Number(n, _)) = &order_expr.expr {
                if let Ok(pos) = n.parse::<usize>() {
                    if pos > 0 {
                        if let Some(expr) = output_exprs_for_order_by.get(pos - 1) {
                            return matches!(expr, Expr::Subquery(_));
                        }
                    }
                }
                return false;
            }

            false
        });

        let (filtered_rows, window_results) =
            if !query.order_by.is_empty() && !order_by_references_correlated_subquery {
                self.apply_order_by(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    filtered_rows,
                    window_results,
                    &query.order_by,
                    &resolved_projection,
                    &schema,
                )
                .await?
            } else {
                (filtered_rows, window_results)
            };

        let has_window_funcs = !window_funcs.is_empty();
        let wildcard = select
            .projection
            .iter()
            .any(|p| matches!(p, SelectItem::Wildcard(_)));
        let pure_wildcard = wildcard && select.projection.len() == 1;

        let (mut rows_for_projection, mut window_results) = match &select.distinct {
            Some(Distinct::On(on_exprs)) => {
                let (rows, indices) =
                    distinct_on_rows_with_indices(filtered_rows, on_exprs, Some(&schema))?;
                let window_results =
                    window_results.map(|wr| super::super::query::reorder_by_indices(&wr, &indices));
                (rows, window_results)
            }
            _ => (filtered_rows, window_results),
        };

        if has_for_update && !pkless_for_update && !rows_for_projection.is_empty() {
            let for_update_nonblock = query
                .locks
                .iter()
                .find(|l| matches!(l.lock_type, LockType::Update))
                .and_then(|l| l.nonblock);

            let query_base_for_offset_limit_fetch = query_with_evaluated_offset_limit_fetch
                .as_ref()
                .unwrap_or(query);
            let query_for_offset_limit_fetch = if generate_series_offset_limit_pushed_down {
                let mut q = query_base_for_offset_limit_fetch.clone();
                q.offset = None;
                q.limit = None;
                q.fetch = None;
                q
            } else {
                query_base_for_offset_limit_fetch.clone()
            };

            let offset = extract_offset(&query_for_offset_limit_fetch);
            let max_lock = match extract_limit(&query_for_offset_limit_fetch) {
                Some(0) => Some(0),
                Some(n) => Some(offset.saturating_add(n)),
                None => None,
            };

            match for_update_nonblock {
                Some(NonBlock::SkipLocked) => {
                    let locked_indices = self
                        .store()
                        .lock_rows_skip_locked(txn, db_id, &t, &rows_for_projection, max_lock)
                        .await?;
                    rows_for_projection = locked_indices
                        .iter()
                        .map(|&idx| rows_for_projection[idx].clone())
                        .collect();
                    window_results = window_results
                        .map(|wr| super::super::query::reorder_by_indices(&wr, &locked_indices));
                }
                _ => {
                    let lock_count = max_lock.unwrap_or(rows_for_projection.len());
                    let lock_count = lock_count.min(rows_for_projection.len());
                    if lock_count > 0 {
                        self.store()
                            .lock_rows(txn, db_id, &t, &rows_for_projection[..lock_count])
                            .await?;
                    }
                }
            }
        }

        let (cols, result_rows) = if pure_wildcard && !has_window_funcs {
            let cols: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
            (cols, rows_for_projection)
        } else {
            self.project_rows(
                txn,
                db_id,
                sequence_values,
                search_path,
                select,
                &schema,
                &outer_alias,
                rows_for_projection,
                &resolved_projection,
                &window_funcs,
                window_results.as_ref(),
            )
            .await?
        };

        let mut result_rows = result_rows;
        if order_by_references_correlated_subquery && !query.order_by.is_empty() {
            self.sort_by_correlated_subquery(&mut result_rows, &cols, &query.order_by);
        }

        if matches!(&select.distinct, Some(Distinct::Distinct)) {
            result_rows = dedup_rows(result_rows);
        }

        let query_base_for_offset_limit_fetch = query_with_evaluated_offset_limit_fetch
            .as_ref()
            .unwrap_or(query);
        let query_no_offset_limit = if generate_series_offset_limit_pushed_down {
            let mut q = query_base_for_offset_limit_fetch.clone();
            q.offset = None;
            q.limit = None;
            q.fetch = None;
            Some(q)
        } else {
            None
        };
        let query_for_offset_limit_fetch = query_no_offset_limit
            .as_ref()
            .unwrap_or(query_base_for_offset_limit_fetch);
        result_rows = apply_offset_limit_fetch(result_rows, query_for_offset_limit_fetch);

        let column_types = Some(
            select
                .projection
                .iter()
                .flat_map(|item| match item {
                    SelectItem::Wildcard(_) => schema
                        .columns
                        .iter()
                        .map(|c| c.data_type.clone())
                        .collect::<Vec<_>>(),
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        vec![infer_expr_type(expr, &schema)]
                    }
                    _ => vec![DataType::Text],
                })
                .collect(),
        );

        let result = ExecuteResult::Select {
            column_types,
            columns: cols,
            rows: result_rows,
            timezone: crate::session_context::current_timezone(),
        };
        if let Some((target_name, _temp)) = select_into_target {
            return self
                .create_table_from_result(txn, db_id, search_path, &target_name, result)
                .await;
        }
        Ok(result)
    }

    async fn execute_aggregate_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        select: &sqlparser::ast::Select,
        schema: &TableSchema,
        filtered_rows: Vec<Row>,
        group_keys_exprs: &[Expr],
        agg_funcs: Vec<(usize, AggExpr)>,
        resolved_projection: &[SelectItem],
        select_into_target: Option<(ObjectName, bool)>,
    ) -> Result<ExecuteResult> {
        let mut groups: HashMap<Vec<u8>, Vec<Aggregator>> = HashMap::new();
        let mut group_rows: HashMap<Vec<u8>, Row> = HashMap::new();
        // Track seen values for DISTINCT aggregates: group_key -> (agg_idx -> seen_values)
        let mut seen_distinct: HashMap<Vec<u8>, Vec<HashSet<Vec<u8>>>> = HashMap::new();

        for row in filtered_rows {
            let mut key = Vec::new();
            for expr in group_keys_exprs {
                key.push(
                    self.eval_expr_maybe_sequence(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        expr,
                        Some(&row),
                        Some(schema),
                    )
                    .await?,
                );
            }
            let key_bytes = serialize_values_for_key(&key).unwrap();

            if !groups.contains_key(&key_bytes) {
                let mut aggs = Vec::new();
                for (_, agg_expr) in &agg_funcs {
                    match agg_expr {
                        AggExpr::Function(f) => {
                            let name = f.name.0.last().unwrap().value.to_uppercase();
                            if name == "STRING_AGG" {
                                let delimiter = if f.args.len() >= 2 {
                                    match &f.args[1] {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                                            match self
                                                .eval_expr_maybe_sequence(
                                                    txn,
                                                    db_id,
                                                    sequence_values,
                                                    search_path,
                                                    e,
                                                    Some(&row),
                                                    Some(schema),
                                                )
                                                .await?
                                            {
                                                Value::Text(s) => s,
                                                _ => ",".to_string(),
                                            }
                                        }
                                        _ => ",".to_string(),
                                    }
                                } else {
                                    ",".to_string()
                                };
                                aggs.push(Aggregator::new_string_agg(delimiter));
                            } else {
                                aggs.push(Aggregator::new(&name)?);
                            }
                        }
                        AggExpr::ArrayAgg(_) => {
                            aggs.push(Aggregator::new_array_agg());
                        }
                    }
                }
                let distinct_sets: Vec<HashSet<Vec<u8>>> =
                    agg_funcs.iter().map(|_| HashSet::new()).collect();
                seen_distinct.insert(key_bytes.clone(), distinct_sets);
                groups.insert(key_bytes.clone(), aggs);
                group_rows.insert(key_bytes.clone(), row.clone());
            }

            let aggs = groups.get_mut(&key_bytes).unwrap();
            for (agg_idx, (_, agg_expr)) in agg_funcs.iter().enumerate() {
                let (filter_expr, arg_expr) = match agg_expr {
                    AggExpr::Function(f) => {
                        let filter = f.filter.as_ref().map(|e| e.as_ref());
                        let arg = if f.args.is_empty() {
                            None
                        } else {
                            match &f.args[0] {
                                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => None,
                                _ => {
                                    return Err(
                                        SqlError::Unsupported("Unsupported arg".into()).into()
                                    )
                                }
                            }
                        };
                        (filter, arg)
                    }
                    AggExpr::ArrayAgg(arr) => (None, Some(arr.expr.as_ref())),
                };

                if let Some(filter) = filter_expr {
                    let filter_val = self
                        .eval_expr_maybe_sequence(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            filter,
                            Some(&row),
                            Some(schema),
                        )
                        .await?;
                    let filter_val = coerce_text_literal_to_bool(filter, filter_val)?;
                    match filter_val {
                        Value::Boolean(true) => {}
                        Value::Boolean(false) | Value::Null => continue,
                        other => {
                            return Err(anyhow!(
                                "FILTER clause must evaluate to boolean, got {:?}",
                                other
                            ));
                        }
                    }
                }

                let val = if let Some(e) = arg_expr {
                    self.eval_expr_maybe_sequence(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        e,
                        Some(&row),
                        Some(schema),
                    )
                    .await?
                } else {
                    Value::Int32(1)
                };

                let is_distinct = matches!(agg_expr, AggExpr::Function(f) if f.distinct);
                if is_distinct {
                    let val_bytes = serialize_value_for_key(&val).unwrap_or_default();
                    let distinct_sets = seen_distinct.get_mut(&key_bytes).unwrap();
                    if !distinct_sets[agg_idx].insert(val_bytes) {
                        continue;
                    }
                }

                aggs[agg_idx].update(&val)?;
            }
        }

        let mut final_rows = Vec::new();
        let col_names: Vec<String> = select.projection.iter().map(get_select_item_name).collect();

        if groups.is_empty() && group_keys_exprs.is_empty() && !agg_funcs.is_empty() {
            let mut default_aggs = Vec::new();
            for (_, agg_expr) in &agg_funcs {
                match agg_expr {
                    AggExpr::Function(f) => {
                        let name = f.name.0.last().unwrap().value.to_uppercase();
                        if name == "STRING_AGG" {
                            default_aggs.push(Aggregator::new_string_agg(",".to_string()));
                        } else {
                            default_aggs.push(Aggregator::new(&name)?);
                        }
                    }
                    AggExpr::ArrayAgg(_) => {
                        default_aggs.push(Aggregator::new_array_agg());
                    }
                }
            }
            let mut row_values = Vec::new();
            let empty_row = Row::new(vec![]);
            let empty_schema = TableSchema::default();
            for (i, item) in resolved_projection.iter().enumerate() {
                if let Some(agg_pos) = agg_funcs.iter().position(|(idx, _)| *idx == i) {
                    row_values.push(default_aggs[agg_pos].result());
                } else {
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                        _ => {
                            row_values.push(Value::Null);
                            continue;
                        }
                    };
                    row_values.push(eval_having_expr(
                        expr,
                        &empty_row,
                        &empty_schema,
                        &agg_funcs,
                        &default_aggs,
                    )?);
                }
            }
            final_rows.push(Row::new(row_values));
        }

        for (key_bytes, aggs) in groups {
            let representative = &group_rows[&key_bytes];

            if let Some(having_expr) = &select.having {
                let having_expr = if sequences::expr_needs_async_eval(having_expr) {
                    sequences::replace_sequence_functions(
                        &self.store(),
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        having_expr,
                        Some(representative),
                        Some(schema),
                    )
                    .await?
                } else {
                    having_expr.clone()
                };
                let having_val =
                    eval_having_expr(&having_expr, representative, schema, &agg_funcs, &aggs)?;
                let having_val = coerce_text_literal_to_bool(&having_expr, having_val)?;
                match having_val {
                    Value::Boolean(true) => {}
                    Value::Boolean(false) | Value::Null => continue,
                    other => {
                        return Err(anyhow!(
                            "HAVING clause must evaluate to boolean, got {:?}",
                            other
                        ));
                    }
                }
            }

            let mut row_values = Vec::new();

            for (i, item) in resolved_projection.iter().enumerate() {
                if let Some(agg_pos) = agg_funcs.iter().position(|(idx, _)| *idx == i) {
                    row_values.push(aggs[agg_pos].result());
                } else {
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                        _ => return Err(SqlError::Unsupported("Unsupported item".into()).into()),
                    };
                    let expr = if sequences::expr_needs_async_eval(expr) {
                        sequences::replace_sequence_functions(
                            &self.store(),
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            expr,
                            Some(representative),
                            Some(schema),
                        )
                        .await?
                    } else {
                        expr.clone()
                    };
                    row_values.push(eval_having_expr(
                        &expr,
                        representative,
                        schema,
                        &agg_funcs,
                        &aggs,
                    )?);
                }
            }
            final_rows.push(Row::new(row_values));
        }

        let final_rows = if !query.order_by.is_empty() {
            self.apply_order_by_for_aggregate(final_rows, &query.order_by, &col_names)
        } else {
            final_rows
        };

        let final_rows = apply_offset_limit_fetch(final_rows, query);

        let result = ExecuteResult::Select {
            column_types: Some(
                resolved_projection
                    .iter()
                    .map(|item| match item {
                        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                            infer_expr_type(expr, schema)
                        }
                        _ => DataType::Text,
                    })
                    .collect(),
            ),
            columns: col_names,
            rows: final_rows,
            timezone: crate::session_context::current_timezone(),
        };
        if let Some((target_name, _temp)) = select_into_target {
            return self
                .create_table_from_result(txn, db_id, search_path, &target_name, result)
                .await;
        }
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_grouping_sets_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        select: &sqlparser::ast::Select,
        schema: &TableSchema,
        filtered_rows: Vec<Row>,
        grouping_sets: Vec<Vec<Expr>>,
        agg_funcs: Vec<(usize, AggExpr)>,
        resolved_projection: &[SelectItem],
        select_into_target: Option<(ObjectName, bool)>,
    ) -> Result<ExecuteResult> {
        let col_names: Vec<String> = resolved_projection
            .iter()
            .map(get_select_item_name)
            .collect();

        let all_group_cols: Vec<Expr> = grouping_sets
            .iter()
            .flatten()
            .cloned()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let mut all_final_rows = Vec::new();

        for grouping_set in &grouping_sets {
            let mut groups: HashMap<Vec<u8>, Vec<Aggregator>> = HashMap::new();
            let mut group_rows: HashMap<Vec<u8>, Row> = HashMap::new();

            for row in &filtered_rows {
                let mut key = Vec::new();
                for expr in grouping_set {
                    key.push(
                        self.eval_expr_maybe_sequence(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            expr,
                            Some(row),
                            Some(schema),
                        )
                        .await?,
                    );
                }
                let key_bytes = serialize_values_for_key(&key).unwrap();

                if !groups.contains_key(&key_bytes) {
                    let mut aggs = Vec::new();
                    for (_, agg_expr) in &agg_funcs {
                        match agg_expr {
                            AggExpr::Function(f) => {
                                let name = f.name.0.last().unwrap().value.to_uppercase();
                                if name == "STRING_AGG" {
                                    let delimiter = if f.args.len() >= 2 {
                                        match &f.args[1] {
                                            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                                                match self
                                                    .eval_expr_maybe_sequence(
                                                        txn,
                                                        db_id,
                                                        sequence_values,
                                                        search_path,
                                                        e,
                                                        Some(row),
                                                        Some(schema),
                                                    )
                                                    .await?
                                                {
                                                    Value::Text(s) => s,
                                                    _ => ",".to_string(),
                                                }
                                            }
                                            _ => ",".to_string(),
                                        }
                                    } else {
                                        ",".to_string()
                                    };
                                    aggs.push(Aggregator::new_string_agg(delimiter));
                                } else {
                                    aggs.push(Aggregator::new(&name)?);
                                }
                            }
                            AggExpr::ArrayAgg(_) => {
                                aggs.push(Aggregator::new_array_agg());
                            }
                        }
                    }
                    groups.insert(key_bytes.clone(), aggs);
                    group_rows.insert(key_bytes.clone(), row.clone());
                }

                let aggs = groups.get_mut(&key_bytes).unwrap();
                for (agg_idx, (_, agg_expr)) in agg_funcs.iter().enumerate() {
                    let (filter_expr, arg_expr) = match agg_expr {
                        AggExpr::Function(f) => {
                            let filter = f.filter.as_ref().map(|e| e.as_ref());
                            let arg = if f.args.is_empty() {
                                None
                            } else {
                                match &f.args[0] {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => None,
                                    _ => {
                                        return Err(
                                            SqlError::Unsupported("Unsupported arg".into()).into()
                                        )
                                    }
                                }
                            };
                            (filter, arg)
                        }
                        AggExpr::ArrayAgg(arr) => (None, Some(arr.expr.as_ref())),
                    };

                    if let Some(filter) = filter_expr {
                        let filter_val = self
                            .eval_expr_maybe_sequence(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                filter,
                                Some(row),
                                Some(schema),
                            )
                            .await?;
                        let filter_val = coerce_text_literal_to_bool(filter, filter_val)?;
                        match filter_val {
                            Value::Boolean(true) => {}
                            Value::Boolean(false) | Value::Null => continue,
                            other => {
                                return Err(anyhow!(
                                    "FILTER clause must evaluate to boolean, got {:?}",
                                    other
                                ));
                            }
                        }
                    }

                    let val = if let Some(e) = arg_expr {
                        self.eval_expr_maybe_sequence(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            e,
                            Some(row),
                            Some(schema),
                        )
                        .await?
                    } else {
                        Value::Int32(1)
                    };
                    aggs[agg_idx].update(&val)?;
                }
            }

            for (key_bytes, aggs) in groups {
                let representative = &group_rows[&key_bytes];

                let mut group_eval_row = representative.clone();
                for group_expr in &all_group_cols {
                    let is_in_current_set = grouping_set
                        .iter()
                        .any(|gs_expr| expr_matches(gs_expr, group_expr));
                    if is_in_current_set {
                        continue;
                    }

                    let col_name = match group_expr {
                        Expr::Identifier(ident) => Some(ident.value.as_str()),
                        Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.as_str()),
                        _ => None,
                    };
                    if let Some(col_name) = col_name {
                        if let Some(idx) = schema
                            .columns
                            .iter()
                            .position(|c| c.name.eq_ignore_ascii_case(col_name))
                        {
                            if idx < group_eval_row.values.len() {
                                group_eval_row.values[idx] = Value::Null;
                            }
                        }
                    }
                }

                if let Some(having_expr) = &select.having {
                    let having_expr = if sequences::expr_needs_async_eval(having_expr) {
                        sequences::replace_sequence_functions(
                            &self.store(),
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            having_expr,
                            Some(&group_eval_row),
                            Some(schema),
                        )
                        .await?
                    } else {
                        having_expr.clone()
                    };
                    let having_val =
                        eval_having_expr(&having_expr, &group_eval_row, schema, &agg_funcs, &aggs)?;
                    let having_val = coerce_text_literal_to_bool(&having_expr, having_val)?;
                    match having_val {
                        Value::Boolean(true) => {}
                        Value::Boolean(false) | Value::Null => continue,
                        other => {
                            return Err(anyhow!(
                                "HAVING clause must evaluate to boolean, got {:?}",
                                other
                            ));
                        }
                    }
                }

                let mut row_values = Vec::new();

                for (i, item) in resolved_projection.iter().enumerate() {
                    if let Some(agg_pos) = agg_funcs.iter().position(|(idx, _)| *idx == i) {
                        row_values.push(aggs[agg_pos].result());
                    } else {
                        let expr = match item {
                            SelectItem::UnnamedExpr(e)
                            | SelectItem::ExprWithAlias { expr: e, .. } => e,
                            _ => {
                                return Err(SqlError::Unsupported("Unsupported item".into()).into())
                            }
                        };

                        if let Expr::Function(func) = expr {
                            let func_name = func
                                .name
                                .0
                                .last()
                                .map(|i| i.value.to_uppercase())
                                .unwrap_or_default();
                            if func_name == "GROUPING" && func.args.len() == 1 {
                                let arg_expr = match &func.args[0] {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                };
                                if let Some(arg_expr) = arg_expr {
                                    let is_in_current_set = grouping_set
                                        .iter()
                                        .any(|gs_expr| expr_matches(gs_expr, arg_expr));
                                    row_values.push(Value::Int32(if is_in_current_set {
                                        0
                                    } else {
                                        1
                                    }));
                                    continue;
                                }
                            }
                        }

                        let is_in_current_set = grouping_set
                            .iter()
                            .any(|gs_expr| expr_matches(gs_expr, expr));
                        if is_in_current_set {
                            let expr = if sequences::expr_needs_async_eval(expr) {
                                sequences::replace_sequence_functions(
                                    &self.store(),
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    expr,
                                    Some(&group_eval_row),
                                    Some(schema),
                                )
                                .await?
                            } else {
                                expr.clone()
                            };
                            row_values.push(eval_having_expr(
                                &expr,
                                &group_eval_row,
                                schema,
                                &agg_funcs,
                                &aggs,
                            )?);
                        } else {
                            row_values.push(Value::Null);
                        }
                    }
                }
                all_final_rows.push(Row::new(row_values));
            }
        }

        let final_rows = if !query.order_by.is_empty() {
            self.apply_order_by_for_aggregate(all_final_rows, &query.order_by, &col_names)
        } else {
            all_final_rows
        };

        let final_rows = apply_offset_limit_fetch(final_rows, query);

        let result = ExecuteResult::Select {
            column_types: Some(
                resolved_projection
                    .iter()
                    .map(|item| match item {
                        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                            infer_expr_type(expr, schema)
                        }
                        _ => DataType::Text,
                    })
                    .collect(),
            ),
            columns: col_names,
            rows: final_rows,
            timezone: crate::session_context::current_timezone(),
        };
        if let Some((target_name, _temp)) = select_into_target {
            return self
                .create_table_from_result(txn, db_id, search_path, &target_name, result)
                .await;
        }
        Ok(result)
    }

    pub(crate) fn apply_order_by_for_aggregate(
        &self,
        rows: Vec<Row>,
        order_by: &[sqlparser::ast::OrderByExpr],
        col_names: &[String],
    ) -> Vec<Row> {
        let mut indexed: Vec<(usize, Row)> = rows.into_iter().enumerate().collect();
        indexed.sort_by(|(idx_a, a), (idx_b, b)| {
            for order_expr in order_by {
                let col_idx = match &order_expr.expr {
                    Expr::Identifier(ident) => col_names
                        .iter()
                        .position(|n| n.eq_ignore_ascii_case(&ident.value)),
                    Expr::CompoundIdentifier(parts) => parts.last().and_then(|ident| {
                        col_names
                            .iter()
                            .position(|n| n.eq_ignore_ascii_case(&ident.value))
                    }),
                    Expr::Value(SqlValue::Number(n, _)) => {
                        n.parse::<usize>().ok().map(|i| i.saturating_sub(1))
                    }
                    _ => None,
                };

                let (val_a, val_b) = if let Some(idx) = col_idx {
                    (a.values.get(idx).cloned(), b.values.get(idx).cloned())
                } else {
                    (None, None)
                };

                let val_a = val_a.unwrap_or(Value::Null);
                let val_b = val_b.unwrap_or(Value::Null);

                let asc = order_expr.asc.unwrap_or(true);
                let nulls_first = order_expr.nulls_first.unwrap_or(!asc);

                match (&val_a, &val_b) {
                    (Value::Null, Value::Null) => continue,
                    (Value::Null, _) => {
                        return if nulls_first {
                            std::cmp::Ordering::Less
                        } else {
                            std::cmp::Ordering::Greater
                        }
                    }
                    (_, Value::Null) => {
                        return if nulls_first {
                            std::cmp::Ordering::Greater
                        } else {
                            std::cmp::Ordering::Less
                        }
                    }
                    _ => {}
                }

                let cmp = super::super::expr::compare_values(&val_a, &val_b).unwrap_or(0);
                if cmp != 0 {
                    return if asc {
                        if cmp > 0 {
                            std::cmp::Ordering::Greater
                        } else {
                            std::cmp::Ordering::Less
                        }
                    } else if cmp > 0 {
                        std::cmp::Ordering::Less
                    } else {
                        std::cmp::Ordering::Greater
                    };
                }
            }

            // Deterministic tie-breaker to match PostgreSQL's stable-looking output:
            // compare full output rows when ORDER BY keys are equal.
            let max_cols = a.values.len().max(b.values.len());
            for i in 0..max_cols {
                let va = a.values.get(i).unwrap_or(&Value::Null);
                let vb = b.values.get(i).unwrap_or(&Value::Null);
                let cmp = super::super::expr::compare_values(va, vb).unwrap_or(0);
                if cmp != 0 {
                    return if cmp > 0 {
                        std::cmp::Ordering::Greater
                    } else {
                        std::cmp::Ordering::Less
                    };
                }
            }

            idx_a.cmp(idx_b)
        });
        indexed.into_iter().map(|(_, r)| r).collect()
    }

    async fn apply_order_by(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        filtered_rows: Vec<Row>,
        window_results: Option<Vec<Vec<Value>>>,
        order_by: &[sqlparser::ast::OrderByExpr],
        resolved_projection: &[SelectItem],
        schema: &TableSchema,
    ) -> Result<(Vec<Row>, Option<Vec<Vec<Value>>>)> {
        let resolved_order_exprs =
            resolve_order_by_exprs_for_non_agg(order_by, resolved_projection, schema)?;

        let order_by_uses_sequences = resolved_order_exprs
            .iter()
            .any(|e| sequences::expr_needs_async_eval(e));

        if order_by_uses_sequences {
            let mut rows_with_keys: Vec<(usize, Row, Vec<Value>)> =
                Vec::with_capacity(filtered_rows.len());
            for (orig_idx, row) in filtered_rows.into_iter().enumerate() {
                let mut keys = Vec::with_capacity(order_by.len());
                for actual_expr in &resolved_order_exprs {
                    let val = if sequences::expr_needs_async_eval(actual_expr) {
                        self.eval_expr_maybe_sequence(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            actual_expr,
                            Some(&row),
                            Some(schema),
                        )
                        .await?
                    } else {
                        eval_expr(actual_expr, Some(&row), Some(schema)).unwrap_or(Value::Null)
                    };
                    keys.push(val);
                }
                rows_with_keys.push((orig_idx, row, keys));
            }

            rows_with_keys.sort_by(|(_, _, a_keys), (_, _, b_keys)| {
                for (idx, order_expr) in order_by.iter().enumerate() {
                    let val_a = a_keys.get(idx).cloned().unwrap_or(Value::Null);
                    let val_b = b_keys.get(idx).cloned().unwrap_or(Value::Null);
                    let asc = order_expr.asc.unwrap_or(true);
                    let nulls_first = order_expr.nulls_first.unwrap_or(!asc);
                    let ord = super::super::expr::compare_order_by_values(
                        &val_a,
                        &val_b,
                        asc,
                        nulls_first,
                    );
                    if !matches!(ord, std::cmp::Ordering::Equal) {
                        return ord;
                    }
                }
                std::cmp::Ordering::Equal
            });

            let reordered_wr = window_results.map(|wr| {
                rows_with_keys
                    .iter()
                    .map(|(orig_idx, _, _)| wr[*orig_idx].clone())
                    .collect()
            });
            let reordered_rows: Vec<Row> = rows_with_keys.into_iter().map(|(_, r, _)| r).collect();
            Ok((reordered_rows, reordered_wr))
        } else {
            let mut indexed: Vec<(usize, Row)> = filtered_rows.into_iter().enumerate().collect();
            indexed.sort_by(|(_, a), (_, b)| {
                for (idx, order_expr) in order_by.iter().enumerate() {
                    let actual_expr = &resolved_order_exprs[idx];
                    let val_a =
                        eval_expr(actual_expr, Some(a), Some(schema)).unwrap_or(Value::Null);
                    let val_b =
                        eval_expr(actual_expr, Some(b), Some(schema)).unwrap_or(Value::Null);
                    let asc = order_expr.asc.unwrap_or(true);
                    let nulls_first = order_expr.nulls_first.unwrap_or(!asc);
                    let ord = super::super::expr::compare_order_by_values(
                        &val_a,
                        &val_b,
                        asc,
                        nulls_first,
                    );
                    if !matches!(ord, std::cmp::Ordering::Equal) {
                        return ord;
                    }
                }
                std::cmp::Ordering::Equal
            });
            let reordered_wr = window_results.map(|wr| {
                indexed
                    .iter()
                    .map(|(orig_idx, _)| wr[*orig_idx].clone())
                    .collect()
            });
            let reordered_rows: Vec<Row> = indexed.into_iter().map(|(_, r)| r).collect();
            Ok((reordered_rows, reordered_wr))
        }
    }

    async fn project_rows(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        select: &sqlparser::ast::Select,
        schema: &TableSchema,
        outer_alias: &str,
        rows_for_projection: Vec<Row>,
        resolved_projection: &[SelectItem],
        window_funcs: &[WindowFuncInfo],
        window_results: Option<&Vec<Vec<Value>>>,
    ) -> Result<(Vec<String>, Vec<Row>)> {
        let mut cols = Vec::new();
        for item in &select.projection {
            match item {
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => {
                    for c in &schema.columns {
                        cols.push(c.name.clone());
                    }
                }
                _ => {
                    let col_name = get_select_item_name(item);
                    cols.push(col_name);
                }
            }
        }

        fn validate_projection_expr(expr: &Expr, schema: &TableSchema) -> Result<()> {
            match expr {
                Expr::Identifier(ident) => {
                    if schema
                        .columns
                        .iter()
                        .all(|c| !c.name.eq_ignore_ascii_case(&ident.value))
                    {
                        return Err(anyhow!("Column '{}' not found", ident.value));
                    }
                    Ok(())
                }
                Expr::CompoundIdentifier(parts) => {
                    if let Some(last) = parts.last() {
                        if schema
                            .columns
                            .iter()
                            .all(|c| !c.name.eq_ignore_ascii_case(&last.value))
                        {
                            return Err(anyhow!("Column '{}' not found", last.value));
                        }
                    }
                    Ok(())
                }
                _ => Ok(()),
            }
        }

        for item in resolved_projection {
            match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    validate_projection_expr(expr, schema)?;
                }
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => {}
            }
        }

        #[derive(Copy, Clone)]
        enum SrfKind {
            Unnest,
            RegexpSplitToTable,
            RegexpMatches,
            EvalFunctionArray,
        }

        fn srf_kind(expr: &Expr) -> Option<SrfKind> {
            let Expr::Function(f) = expr else {
                return None;
            };
            let Some(name) = f.name.0.last() else {
                return None;
            };
            match name.value.to_ascii_uppercase().as_str() {
                "UNNEST" => Some(SrfKind::Unnest),
                "REGEXP_SPLIT_TO_TABLE" => Some(SrfKind::RegexpSplitToTable),
                "REGEXP_MATCHES" => Some(SrfKind::RegexpMatches),
                "JSONB_OBJECT_KEYS"
                | "JSONB_ARRAY_ELEMENTS"
                | "JSONB_ARRAY_ELEMENTS_TEXT"
                | "JSONB_EACH"
                | "JSONB_EACH_TEXT" => Some(SrfKind::EvalFunctionArray),
                _ => None,
            }
        }

        fn regexp_captures_to_values(caps: &regex::Captures<'_>) -> Vec<Value> {
            if caps.len() > 1 {
                (1..caps.len())
                    .map(|idx| match caps.get(idx) {
                        Some(m) => Value::Text(m.as_str().to_string()),
                        None => Value::Null,
                    })
                    .collect()
            } else {
                caps.get(0)
                    .map(|m| vec![Value::Text(m.as_str().to_string())])
                    .unwrap_or_default()
            }
        }

        let has_srf = resolved_projection.iter().any(|item| match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                srf_kind(e).is_some()
            }
            _ => false,
        });

        let mut result_rows = Vec::new();
        for (row_idx, row) in rows_for_projection.iter().enumerate() {
            let mut row_values = Vec::new();
            let mut srf_outputs: Vec<(usize, Vec<Value>)> = Vec::new();

            for (proj_idx, item) in resolved_projection.iter().enumerate() {
                if let Some(wf_pos) = window_funcs.iter().position(|wf| wf.proj_idx == proj_idx) {
                    if let Some(wr) = window_results {
                        row_values.push(wr[row_idx][wf_pos].clone());
                    } else {
                        row_values.push(Value::Null);
                    }
                } else {
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) => e,
                        SelectItem::ExprWithAlias { expr: e, .. } => e,
                        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => {
                            row_values.extend(row.values.clone());
                            continue;
                        }
                    };

                    if has_srf {
                        if let Some(kind) = srf_kind(expr) {
                            let Expr::Function(f) = expr else {
                                row_values.push(Value::Null);
                                continue;
                            };

                            let outputs = match kind {
                                SrfKind::Unnest => {
                                    let arg_expr = f.args.first().and_then(|arg| match arg {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                        _ => None,
                                    });
                                    if let Some(arg_expr) = arg_expr {
                                        match self
                                            .eval_expr_maybe_sequence(
                                                txn,
                                                db_id,
                                                sequence_values,
                                                search_path,
                                                arg_expr,
                                                Some(row),
                                                Some(schema),
                                            )
                                            .await?
                                        {
                                            Value::Array(arr) => arr,
                                            Value::Null => Vec::new(),
                                            other => vec![other],
                                        }
                                    } else {
                                        Vec::new()
                                    }
                                }
                                SrfKind::RegexpSplitToTable => {
                                    let arg0 = f.args.get(0).and_then(|arg| match arg {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                        _ => None,
                                    });
                                    let arg1 = f.args.get(1).and_then(|arg| match arg {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                        _ => None,
                                    });
                                    let arg2 = f.args.get(2).and_then(|arg| match arg {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                        _ => None,
                                    });
                                    let (Some(arg0), Some(arg1)) = (arg0, arg1) else {
                                        return Err(anyhow!(
                                            "regexp_split_to_table requires at least 2 arguments"
                                        ));
                                    };

                                    let source_val = self
                                        .eval_expr_maybe_sequence(
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            arg0,
                                            Some(row),
                                            Some(schema),
                                        )
                                        .await?;
                                    let source = match source_val {
                                        Value::Text(s) => Some(s),
                                        Value::Null => None,
                                        v => Some(v.to_string()),
                                    };

                                    let pattern_val = self
                                        .eval_expr_maybe_sequence(
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            arg1,
                                            Some(row),
                                            Some(schema),
                                        )
                                        .await?;
                                    let pattern = match pattern_val {
                                        Value::Text(s) => Some(s),
                                        Value::Null => None,
                                        v => Some(v.to_string()),
                                    };

                                    match (source, pattern) {
                                        (Some(source), Some(pattern)) => {
                                            let flags = if let Some(arg2) = arg2 {
                                                match self
                                                    .eval_expr_maybe_sequence(
                                                        txn,
                                                        db_id,
                                                        sequence_values,
                                                        search_path,
                                                        arg2,
                                                        Some(row),
                                                        Some(schema),
                                                    )
                                                    .await?
                                                {
                                                    Value::Text(s) => s,
                                                    Value::Null => String::new(),
                                                    v => v.to_string(),
                                                }
                                            } else {
                                                String::new()
                                            };
                                            let case_insensitive =
                                                flags.to_ascii_lowercase().contains('i');
                                            let regex_pattern = if case_insensitive {
                                                format!("(?i){}", pattern)
                                            } else {
                                                pattern
                                            };
                                            let re =
                                                regex::Regex::new(&regex_pattern).map_err(|e| {
                                                    anyhow!("Invalid regex pattern: {}", e)
                                                })?;

                                            let mut parts = Vec::new();
                                            let mut last_end = 0usize;
                                            for m in re.find_iter(&source) {
                                                parts.push(Value::Text(
                                                    source[last_end..m.start()].to_string(),
                                                ));
                                                last_end = m.end();
                                            }
                                            parts.push(Value::Text(source[last_end..].to_string()));
                                            parts
                                        }
                                        _ => Vec::new(),
                                    }
                                }
                                SrfKind::RegexpMatches => {
                                    let arg0 = f.args.get(0).and_then(|arg| match arg {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                        _ => None,
                                    });
                                    let arg1 = f.args.get(1).and_then(|arg| match arg {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                        _ => None,
                                    });
                                    let arg2 = f.args.get(2).and_then(|arg| match arg {
                                        FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                        _ => None,
                                    });
                                    let (Some(arg0), Some(arg1)) = (arg0, arg1) else {
                                        return Err(anyhow!(
                                            "regexp_matches requires at least 2 arguments"
                                        ));
                                    };
                                    let source_val = self
                                        .eval_expr_maybe_sequence(
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            arg0,
                                            Some(row),
                                            Some(schema),
                                        )
                                        .await?;
                                    let source = match source_val {
                                        Value::Text(s) => Some(s),
                                        Value::Null => None,
                                        v => Some(v.to_string()),
                                    };

                                    let pattern_val = self
                                        .eval_expr_maybe_sequence(
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            arg1,
                                            Some(row),
                                            Some(schema),
                                        )
                                        .await?;
                                    let pattern = match pattern_val {
                                        Value::Text(s) => Some(s),
                                        Value::Null => None,
                                        v => Some(v.to_string()),
                                    };

                                    match (source, pattern) {
                                        (Some(source), Some(pattern)) => {
                                            let flags = if let Some(arg2) = arg2 {
                                                match self
                                                    .eval_expr_maybe_sequence(
                                                        txn,
                                                        db_id,
                                                        sequence_values,
                                                        search_path,
                                                        arg2,
                                                        Some(row),
                                                        Some(schema),
                                                    )
                                                    .await?
                                                {
                                                    Value::Text(s) => s,
                                                    Value::Null => String::new(),
                                                    v => v.to_string(),
                                                }
                                            } else {
                                                String::new()
                                            };
                                            let global = flags.to_ascii_lowercase().contains('g');
                                            let case_insensitive =
                                                flags.to_ascii_lowercase().contains('i');
                                            let regex_pattern = if case_insensitive {
                                                format!("(?i){}", pattern)
                                            } else {
                                                pattern
                                            };
                                            let re =
                                                regex::Regex::new(&regex_pattern).map_err(|e| {
                                                    anyhow!("Invalid regex pattern: {}", e)
                                                })?;

                                            let mut out = Vec::new();
                                            if global {
                                                for caps in re.captures_iter(&source) {
                                                    out.push(Value::Array(
                                                        regexp_captures_to_values(&caps),
                                                    ));
                                                }
                                            } else if let Some(caps) = re.captures(&source) {
                                                out.push(Value::Array(regexp_captures_to_values(
                                                    &caps,
                                                )));
                                            }
                                            out
                                        }
                                        _ => Vec::new(),
                                    }
                                }
                                SrfKind::EvalFunctionArray => {
                                    match self
                                        .eval_expr_maybe_sequence(
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            expr,
                                            Some(row),
                                            Some(schema),
                                        )
                                        .await?
                                    {
                                        Value::Array(arr) => arr,
                                        Value::Null => Vec::new(),
                                        other => vec![other],
                                    }
                                }
                            };

                            srf_outputs.push((row_values.len(), outputs));
                            row_values.push(Value::Null);
                            continue;
                        }
                    } else {
                        let value = if let Expr::Subquery(subquery) = expr {
                            self.eval_correlated_subquery(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                subquery,
                                outer_alias,
                                schema,
                                row,
                            )
                            .await?
                        } else {
                            self.eval_expr_maybe_sequence(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                expr,
                                Some(row),
                                Some(schema),
                            )
                            .await?
                        };
                        row_values.push(value);
                    }
                }
            }

            if !srf_outputs.is_empty() {
                let max_len = srf_outputs
                    .iter()
                    .map(|(_, out)| out.len())
                    .max()
                    .unwrap_or(0);
                for i in 0..max_len {
                    let mut expanded_row = row_values.clone();
                    for (col_idx, out) in &srf_outputs {
                        expanded_row[*col_idx] = out.get(i).cloned().unwrap_or(Value::Null);
                    }
                    result_rows.push(Row::new(expanded_row));
                }
            } else {
                result_rows.push(Row::new(row_values));
            }
        }
        Ok((cols, result_rows))
    }

    fn sort_by_correlated_subquery(
        &self,
        result_rows: &mut [Row],
        cols: &[String],
        order_by: &[sqlparser::ast::OrderByExpr],
    ) {
        result_rows.sort_by(|a, b| {
            for order_expr in order_by {
                let col_idx = if let Expr::Identifier(ref ident) = order_expr.expr {
                    cols.iter()
                        .position(|c| c.eq_ignore_ascii_case(&ident.value))
                } else if let Expr::Value(SqlValue::Number(n, _)) = &order_expr.expr {
                    n.parse::<usize>().ok().map(|i| i.saturating_sub(1))
                } else {
                    None
                };

                if let Some(idx) = col_idx {
                    let val_a = a.values.get(idx).cloned().unwrap_or(Value::Null);
                    let val_b = b.values.get(idx).cloned().unwrap_or(Value::Null);
                    let cmp = super::super::expr::compare_values(&val_a, &val_b).unwrap_or(0);
                    if cmp != 0 {
                        let asc = order_expr.asc.unwrap_or(true);
                        return if asc {
                            if cmp > 0 {
                                std::cmp::Ordering::Greater
                            } else {
                                std::cmp::Ordering::Less
                            }
                        } else if cmp > 0 {
                            std::cmp::Ordering::Less
                        } else {
                            std::cmp::Ordering::Greater
                        };
                    }
                }
            }
            std::cmp::Ordering::Equal
        });
    }

    async fn resolve_join_table_factor(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        table_factor: &TableFactor,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        virtual_filter: &VirtualTableFilter,
    ) -> Result<Option<(String, TableSchema, Option<Vec<Row>>)>> {
        match table_factor {
            TableFactor::Table {
                name, alias, args, ..
            } => {
                let (schema_opt, obj_name) = names::split_object_name(name)?;
                let alias_str = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| obj_name.clone());

                if let Some(func_args) = args {
                    let tbl_upper = obj_name.to_uppercase();
                    if tbl_upper == "GENERATE_SERIES" {
                        let (schema, rows) = self
                            .execute_generate_series(func_args, &alias_str, alias.as_ref(), 0, None)
                            .await?;
                        return Ok(Some((alias_str, schema, Some(rows))));
                    }
                    if let Some((schema, rows)) = self
                        .try_execute_extension_table_function(
                            txn,
                            db_id,
                            search_path,
                            name,
                            func_args,
                            alias.as_ref(),
                        )
                        .await?
                    {
                        return Ok(Some((alias_str, schema, Some(rows))));
                    }
                    if let Some((schema, rows)) = self
                        .try_execute_user_table_function(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            name,
                            func_args,
                            alias.as_ref(),
                        )
                        .await?
                    {
                        return Ok(Some((alias_str, schema, Some(rows))));
                    }
                }

                let cte_key = obj_name.to_lowercase();
                if let Some((cte_schema, cte_rows)) = ctes.get(&cte_key) {
                    return Ok(Some((
                        alias_str,
                        cte_schema.clone(),
                        Some(cte_rows.clone()),
                    )));
                }
                if let Some(resolved) = names::resolve_existing_table_name(
                    self.store().as_ref(),
                    txn,
                    db_id,
                    name,
                    search_path,
                )
                .await?
                {
                    if let Some(s) = self.store().get_schema(txn, db_id, &resolved.full).await? {
                        return Ok(Some((alias_str, s, None)));
                    }
                }
                let lookup_name = match schema_opt {
                    Some(schema) => format!("{}.{}", schema, obj_name),
                    None => obj_name.clone(),
                };
                match self
                    .get_table_data_filtered(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &lookup_name,
                        ctes,
                        virtual_filter,
                    )
                    .await
                {
                    Ok((schema, rows)) => Ok(Some((alias_str, schema, Some(rows)))),
                    Err(_) => Ok(None),
                }
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let alias_name = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| "subquery".to_string());
                let alias_columns = alias.as_ref().map(|a| a.columns.as_slice()).unwrap_or(&[]);
                let (schema, rows) = self
                    .execute_derived_table(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        subquery,
                        &alias_name,
                        alias_columns,
                        ctes,
                    )
                    .await?;
                Ok(Some((alias_name, schema, Some(rows))))
            }
            TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => {
                // KISS fallback: materialize the nested join as a derived table.
                // This avoids hard errors for `TableFactor::NestedJoin` without having to teach the
                // operator join planner about every nested join shape up-front.
                let alias_name = if let Some(a) = alias.as_ref() {
                    a.name.value.clone()
                } else {
                    next_nested_join_alias()
                };
                let alias_columns = alias.as_ref().map(|a| a.columns.as_slice()).unwrap_or(&[]);

                debug!(
                    alias = %alias_name,
                    nested_join = %table_with_joins,
                    "Materializing nested join as derived table"
                );

                let nested_query = Query {
                    with: None,
                    body: Box::new(SetExpr::Select(Box::new(sqlparser::ast::Select {
                        distinct: None,
                        top: None,
                        projection: vec![SelectItem::Wildcard(Default::default())],
                        into: None,
                        from: vec![(*table_with_joins.as_ref()).clone()],
                        lateral_views: vec![],
                        selection: None,
                        group_by: GroupByExpr::Expressions(vec![]),
                        cluster_by: vec![],
                        distribute_by: vec![],
                        sort_by: vec![],
                        having: None,
                        named_window: vec![],
                        qualify: None,
                    }))),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    fetch: None,
                    locks: vec![],
                    limit_by: vec![],
                    for_clause: None,
                };

                let (schema, rows) = self
                    .execute_derived_table(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &nested_query,
                        &alias_name,
                        alias_columns,
                        ctes,
                    )
                    .await?;

                let derived_cols: Vec<String> =
                    schema.columns.iter().map(|c| c.name.clone()).collect();
                debug!(
                    alias = %alias_name,
                    derived_cols = ?derived_cols,
                    "Nested join derived table output schema"
                );
                Ok(Some((alias_name, schema, Some(rows))))
            }
            _ => Ok(None),
        }
    }

    async fn try_execute_simple_join_with_operators(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        select: &sqlparser::ast::Select,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<Option<ExecuteResult>> {
        use crate::types::ColumnDef;

        let virtual_filter = select
            .selection
            .as_ref()
            .map(extract_virtual_table_filter)
            .unwrap_or_default();

        let resolved_selection = if let Some(sel) = &select.selection {
            Some(
                self.resolve_subqueries(txn, db_id, sequence_values, search_path, sel, ctes)
                    .await?,
            )
        } else {
            None
        };

        let resolved_projection = self
            .resolve_projection_subqueries(
                txn,
                db_id,
                sequence_values,
                search_path,
                &select.projection,
                ctes,
            )
            .await?;

        let has_lateral = select.from.iter().any(|from_item| {
            from_item
                .joins
                .iter()
                .any(|j| matches!(&j.relation, TableFactor::Derived { lateral: true, .. }))
        });

        if has_lateral {
            return self
                .execute_lateral_join(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    query,
                    select,
                    &resolved_projection,
                    resolved_selection.as_ref(),
                    ctes,
                )
                .await
                .map(Some);
        }

        let is_implicit_join = select.from.len() > 1;

        struct TableInfo {
            alias: String,
            schema: TableSchema,
            preloaded_rows: Option<Vec<Row>>,
        }

        let mut tables: Vec<TableInfo> = Vec::new();
        struct JoinStep {
            right_idx: usize,
            join_type: JoinType,
            condition: Option<Expr>,
        }
        let mut join_steps: Vec<JoinStep> = Vec::new();
        let mut merge_columns: Vec<UsingMergeColumn> = Vec::new();

        // Unified path: process all FROM items and their explicit JOINs.
        // For `FROM a, b JOIN c ON ...`, sqlparser gives:
        //   from[0] = {relation: a, joins: []}
        //   from[1] = {relation: b, joins: [{relation: c, ON: b.id = c.id}]}
        //
        // Explicit JOINs bind tighter than comma: `FROM a, b RIGHT JOIN c`
        // means `a CROSS (b RIGHT JOIN c)`. We process FROM items with explicit
        // JOINs first, then cross-join the standalone FROM items.
        // Phase 1: process FROM items that have explicit JOINs (they bind tighter)
        let mut transparent_nested_joins: Vec<TransparentNestedJoinInfo> = Vec::new();
        for from_item in &select.from {
            if from_item.joins.is_empty() {
                continue;
            }
            let resolved_table = self
                .resolve_join_table_factor(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &from_item.relation,
                    ctes,
                    &virtual_filter,
                )
                .await?;
            let (alias, schema, preloaded_rows) = match resolved_table {
                Some(t) => t,
                None => return Ok(None),
            };
            if let TableFactor::NestedJoin {
                table_with_joins,
                alias: nested_alias,
            } = &from_item.relation
            {
                if nested_alias.is_none() {
                    let inner_aliases =
                        collect_visible_aliases_in_table_with_joins(table_with_joins);
                    if !inner_aliases.is_empty() {
                        transparent_nested_joins.push(TransparentNestedJoinInfo {
                            derived_alias: alias.clone(),
                            inner_aliases,
                            duplicate_cols_lower: duplicate_column_names_lowercase(&schema),
                        });
                    }
                }
            }
            tables.push(TableInfo {
                alias,
                schema,
                preloaded_rows,
            });

            for join in &from_item.joins {
                let resolved_join = self
                    .resolve_join_table_factor(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &join.relation,
                        ctes,
                        &virtual_filter,
                    )
                    .await?;
                let (alias, right_schema, right_preloaded) = match resolved_join {
                    Some(t) => t,
                    None => return Ok(None),
                };
                if let TableFactor::NestedJoin {
                    table_with_joins,
                    alias: nested_alias,
                } = &join.relation
                {
                    if nested_alias.is_none() {
                        let inner_aliases =
                            collect_visible_aliases_in_table_with_joins(table_with_joins);
                        if !inner_aliases.is_empty() {
                            transparent_nested_joins.push(TransparentNestedJoinInfo {
                                derived_alias: alias.clone(),
                                inner_aliases,
                                duplicate_cols_lower: duplicate_column_names_lowercase(
                                    &right_schema,
                                ),
                            });
                        }
                    }
                }
                let jt = JoinType::from(&join.join_operator);
                let condition = match &join.join_operator {
                    sqlparser::ast::JoinOperator::Inner(JoinConstraint::On(expr))
                    | sqlparser::ast::JoinOperator::LeftOuter(JoinConstraint::On(expr))
                    | sqlparser::ast::JoinOperator::RightOuter(JoinConstraint::On(expr))
                    | sqlparser::ast::JoinOperator::FullOuter(JoinConstraint::On(expr)) => {
                        Some(expr.clone())
                    }
                    sqlparser::ast::JoinOperator::CrossJoin => None,
                    sqlparser::ast::JoinOperator::Inner(JoinConstraint::Using(cols))
                    | sqlparser::ast::JoinOperator::LeftOuter(JoinConstraint::Using(cols))
                    | sqlparser::ast::JoinOperator::RightOuter(JoinConstraint::Using(cols))
                    | sqlparser::ast::JoinOperator::FullOuter(JoinConstraint::Using(cols)) => {
                        let mut conditions: Vec<Expr> = Vec::new();
                        for col in cols {
                            let col_name = names::normalize_ident(col);
                            let left_expr = if let Some(idx) = merge_columns
                                .iter()
                                .position(|mc| mc.col_name.eq_ignore_ascii_case(&col_name))
                            {
                                // Chained USING JOIN must compare against the *merged* key from
                                // the left relation (COALESCE semantics), not any single prior
                                // table alias. Important for OUTER JOIN chains.
                                let expr = build_coalesce_for_merge(&merge_columns[idx]);
                                merge_columns[idx].source_aliases.push(alias.clone());
                                expr
                            } else {
                                let left_alias = tables
                                    .iter()
                                    .rev()
                                    .find(|t| {
                                        t.schema
                                            .columns
                                            .iter()
                                            .any(|c| c.name.eq_ignore_ascii_case(&col_name))
                                    })
                                    .map(|t| t.alias.clone());
                                let left_alias = match left_alias {
                                    Some(a) => a,
                                    None => return Ok(None),
                                };
                                merge_columns.push(UsingMergeColumn {
                                    col_name: col_name.clone(),
                                    source_aliases: vec![left_alias.clone(), alias.clone()],
                                });
                                Expr::CompoundIdentifier(vec![
                                    Ident::new(left_alias),
                                    Ident::new(col_name.clone()),
                                ])
                            };
                            conditions.push(Expr::BinaryOp {
                                left: Box::new(left_expr),
                                op: BinaryOperator::Eq,
                                right: Box::new(Expr::CompoundIdentifier(vec![
                                    Ident::new(alias.clone()),
                                    Ident::new(col_name),
                                ])),
                            });
                        }
                        conditions.into_iter().reduce(|a, b| Expr::BinaryOp {
                            left: Box::new(a),
                            op: BinaryOperator::And,
                            right: Box::new(b),
                        })
                    }
                    sqlparser::ast::JoinOperator::Inner(JoinConstraint::Natural)
                    | sqlparser::ast::JoinOperator::LeftOuter(JoinConstraint::Natural)
                    | sqlparser::ast::JoinOperator::RightOuter(JoinConstraint::Natural)
                    | sqlparser::ast::JoinOperator::FullOuter(JoinConstraint::Natural) => {
                        let mut common_idents: Vec<Ident> = Vec::new();
                        let mut seen = HashSet::new();
                        for right_col in &right_schema.columns {
                            let col_lower = right_col.name.to_lowercase();
                            if seen.contains(&col_lower) {
                                continue;
                            }
                            for left_table in &tables {
                                if left_table
                                    .schema
                                    .columns
                                    .iter()
                                    .any(|c| c.name.eq_ignore_ascii_case(&col_lower))
                                {
                                    common_idents.push(Ident::new(col_lower.clone()));
                                    seen.insert(col_lower.clone());
                                    break;
                                }
                            }
                        }
                        if common_idents.is_empty() {
                            None
                        } else {
                            let mut conditions: Vec<Expr> = Vec::new();
                            for col_ident in &common_idents {
                                let col_name = &col_ident.value;
                                let left_expr = if let Some(idx) = merge_columns
                                    .iter()
                                    .position(|mc| mc.col_name.eq_ignore_ascii_case(col_name))
                                {
                                    let expr = build_coalesce_for_merge(&merge_columns[idx]);
                                    merge_columns[idx].source_aliases.push(alias.clone());
                                    expr
                                } else {
                                    let left_alias = tables
                                        .iter()
                                        .rev()
                                        .find(|t| {
                                            t.schema
                                                .columns
                                                .iter()
                                                .any(|c| c.name.eq_ignore_ascii_case(col_name))
                                        })
                                        .map(|t| t.alias.clone())
                                        .unwrap_or_else(|| tables[0].alias.clone());
                                    merge_columns.push(UsingMergeColumn {
                                        col_name: col_name.clone(),
                                        source_aliases: vec![left_alias.clone(), alias.clone()],
                                    });
                                    Expr::CompoundIdentifier(vec![
                                        Ident::new(left_alias),
                                        Ident::new(col_name.clone()),
                                    ])
                                };
                                conditions.push(Expr::BinaryOp {
                                    left: Box::new(left_expr),
                                    op: BinaryOperator::Eq,
                                    right: Box::new(Expr::CompoundIdentifier(vec![
                                        Ident::new(alias.clone()),
                                        Ident::new(col_name.clone()),
                                    ])),
                                });
                            }
                            conditions.into_iter().reduce(|a, b| Expr::BinaryOp {
                                left: Box::new(a),
                                op: BinaryOperator::And,
                                right: Box::new(b),
                            })
                        }
                    }
                    _ => return Ok(None),
                };
                let idx = tables.len();
                tables.push(TableInfo {
                    alias,
                    schema: right_schema,
                    preloaded_rows: right_preloaded,
                });
                join_steps.push(JoinStep {
                    right_idx: idx,
                    join_type: jt,
                    condition,
                });
            }
        }

        // Phase 2: add standalone FROM items (no explicit JOINs) as CROSS JOINs
        for from_item in &select.from {
            if !from_item.joins.is_empty() {
                continue;
            }
            let resolved_table = self
                .resolve_join_table_factor(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &from_item.relation,
                    ctes,
                    &virtual_filter,
                )
                .await?;
            let (alias, schema, preloaded_rows) = match resolved_table {
                Some(t) => t,
                None => return Ok(None),
            };
            let idx = tables.len();
            let need_cross = idx > 0;
            if let TableFactor::NestedJoin {
                table_with_joins,
                alias: nested_alias,
            } = &from_item.relation
            {
                if nested_alias.is_none() {
                    let inner_aliases =
                        collect_visible_aliases_in_table_with_joins(table_with_joins);
                    if !inner_aliases.is_empty() {
                        transparent_nested_joins.push(TransparentNestedJoinInfo {
                            derived_alias: alias.clone(),
                            inner_aliases,
                            duplicate_cols_lower: duplicate_column_names_lowercase(&schema),
                        });
                    }
                }
            }
            tables.push(TableInfo {
                alias,
                schema,
                preloaded_rows,
            });
            if need_cross {
                join_steps.push(JoinStep {
                    right_idx: idx,
                    join_type: JoinType::Cross,
                    condition: None,
                });
            }
        }

        if tables.len() < 2 {
            return Ok(None);
        }

        // Resolve JOIN-condition scalar subqueries.
        //
        // - Uncorrelated subqueries are resolved eagerly to literals.
        // - Correlated scalar subqueries are materialized as hidden computed columns on the
        //   referenced outer table (preloading rows if needed), and the JOIN condition is
        //   rewritten to reference that column instead of `Expr::Subquery`.
        let mut correlated_subquery_counter: usize = 0;

        fn rewrite_join_condition_subqueries<'a>(
            exec: &'a Executor,
            txn: &'a mut Transaction,
            db_id: u64,
            sequence_values: &'a mut HashMap<String, i64>,
            search_path: &'a [String],
            expr: &'a Expr,
            ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
            tables: &'a mut Vec<TableInfo>,
            counter: &'a mut usize,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Expr>> + Send + 'a>>
        {
            Box::pin(async move {
                match expr {
                    Expr::Subquery(subquery) => {
                        let mut referenced_aliases: Vec<String> = Vec::new();
                        for t in tables.iter() {
                            if super::subquery::query_has_outer_reference(subquery, &t.alias) {
                                referenced_aliases.push(t.alias.clone());
                            }
                        }

                        if referenced_aliases.is_empty() {
                            return exec
                                .resolve_subqueries(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    expr,
                                    ctes,
                                )
                                .await;
                        }

                        if referenced_aliases.len() > 1 {
                            return Err(anyhow!(
                                "Correlated scalar subquery in JOIN condition references multiple outer tables: {:?}",
                                referenced_aliases
                            ));
                        }

                        let outer_alias = referenced_aliases
                            .pop()
                            .unwrap_or_else(|| "outer".to_string());

                        let table_idx = tables
                            .iter()
                            .position(|t| t.alias.eq_ignore_ascii_case(&outer_alias))
                            .ok_or_else(|| {
                                anyhow!(
                                    "Correlated scalar subquery references unknown outer table alias '{}'",
                                    outer_alias
                                )
                            })?;

                        if tables[table_idx].preloaded_rows.is_none() {
                            let (schema, rows) = exec
                                .get_table_data(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &tables[table_idx].schema.name,
                                    ctes,
                                )
                                .await?;
                            tables[table_idx].schema = schema;
                            tables[table_idx].preloaded_rows = Some(rows);
                        }

                        let computed_col = format!("__tipg_subquery_{}", *counter);
                        *counter = counter.saturating_add(1);

                        let outer_schema = tables[table_idx].schema.clone();
                        let outer_rows =
                            tables[table_idx].preloaded_rows.take().unwrap_or_default();

                        let mut inferred_type: Option<DataType> = None;
                        let mut new_rows: Vec<Row> = Vec::with_capacity(outer_rows.len());
                        for row in outer_rows {
                            let val = exec
                                .eval_correlated_subquery(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    subquery,
                                    &outer_alias,
                                    &outer_schema,
                                    &row,
                                )
                                .await?;
                            if inferred_type.is_none() {
                                inferred_type = val.data_type();
                            }
                            let mut values = row.values;
                            values.push(val);
                            new_rows.push(Row::new(values));
                        }

                        tables[table_idx]
                            .schema
                            .columns
                            .push(crate::types::ColumnDef {
                                name: computed_col.clone(),
                                data_type: inferred_type.unwrap_or(DataType::Text),
                                nullable: true,
                                primary_key: false,
                                unique: false,
                                is_serial: false,
                                default_expr: None,
                            });
                        tables[table_idx].preloaded_rows = Some(new_rows);

                        Ok(Expr::CompoundIdentifier(vec![
                            Ident::new(outer_alias),
                            Ident::new(computed_col),
                        ]))
                    }
                    Expr::BinaryOp { left, op, right } => {
                        let left = rewrite_join_condition_subqueries(
                            exec,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            left,
                            ctes,
                            tables,
                            counter,
                        )
                        .await?;
                        let right = rewrite_join_condition_subqueries(
                            exec,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            right,
                            ctes,
                            tables,
                            counter,
                        )
                        .await?;
                        Ok(Expr::BinaryOp {
                            left: Box::new(left),
                            op: op.clone(),
                            right: Box::new(right),
                        })
                    }
                    Expr::UnaryOp { op, expr: inner } => {
                        let inner = rewrite_join_condition_subqueries(
                            exec,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            inner,
                            ctes,
                            tables,
                            counter,
                        )
                        .await?;
                        Ok(Expr::UnaryOp {
                            op: op.clone(),
                            expr: Box::new(inner),
                        })
                    }
                    Expr::Nested(inner) => {
                        let inner = rewrite_join_condition_subqueries(
                            exec,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            inner,
                            ctes,
                            tables,
                            counter,
                        )
                        .await?;
                        Ok(Expr::Nested(Box::new(inner)))
                    }
                    Expr::Function(f) => {
                        let mut args = Vec::with_capacity(f.args.len());
                        for arg in &f.args {
                            let rewritten = match arg {
                                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(
                                        rewrite_join_condition_subqueries(
                                            exec,
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            e,
                                            ctes,
                                            tables,
                                            counter,
                                        )
                                        .await?,
                                    ))
                                }
                                other => other.clone(),
                            };
                            args.push(rewritten);
                        }
                        Ok(Expr::Function(Function {
                            name: f.name.clone(),
                            args,
                            filter: f.filter.clone(),
                            null_treatment: f.null_treatment.clone(),
                            over: f.over.clone(),
                            distinct: f.distinct,
                            special: f.special,
                            order_by: f.order_by.clone(),
                        }))
                    }
                    _ => Ok(expr.clone()),
                }
            })
        }

        for step in &mut join_steps {
            if let Some(cond) = &step.condition {
                step.condition = Some(
                    rewrite_join_condition_subqueries(
                        self,
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        cond,
                        ctes,
                        &mut tables,
                        &mut correlated_subquery_counter,
                    )
                    .await?,
                );
            }
        }

        let has_aggregates = projection_has_non_window_aggregate(&resolved_projection);
        let has_group_by = !matches!(
            &select.group_by,
            GroupByExpr::Expressions(exprs) if exprs.is_empty()
        );
        let needs_aggregation = has_aggregates || has_group_by || select.having.is_some();

        let has_distinct = select.distinct.is_some();

        // Build (alias, schema) pairs for expression rewriting, with optional alias-less NestedJoin
        // transparency mapping (Sequelize relies on inner aliases remaining visible).
        let mut table_aliases: Vec<(String, TableSchema)> = tables
            .iter()
            .map(|t| (t.alias.clone(), t.schema.clone()))
            .collect();

        let mut requires_transparent_nested_join_mapping = false;
        if !transparent_nested_joins.is_empty() {
            #[derive(Debug, Clone)]
            struct InnerAliasTarget {
                inner_alias: String,
                derived_alias: String,
                duplicate_cols_lower: HashSet<String>,
            }

            let mut inner_alias_targets: HashMap<String, InnerAliasTarget> = HashMap::new();
            let mut ambiguous_inner_aliases: HashSet<String> = HashSet::new();
            for info in &transparent_nested_joins {
                for inner_alias in &info.inner_aliases {
                    let key = inner_alias.to_lowercase();
                    if let Some(existing) = inner_alias_targets.get(&key) {
                        if !existing
                            .derived_alias
                            .eq_ignore_ascii_case(&info.derived_alias)
                        {
                            ambiguous_inner_aliases.insert(key);
                        }
                    } else {
                        inner_alias_targets.insert(
                            key,
                            InnerAliasTarget {
                                inner_alias: inner_alias.clone(),
                                derived_alias: info.derived_alias.clone(),
                                duplicate_cols_lower: info.duplicate_cols_lower.clone(),
                            },
                        );
                    }
                }
            }

            // Guard-rail: Qualified wildcard (inner_alias.*) is not transparent today because it is
            // expanded from `tables` rather than `table_aliases`.
            for item in &resolved_projection {
                if let SelectItem::QualifiedWildcard(obj, _) = item {
                    let qualifier = obj.0.last().map(|i| i.value.clone()).unwrap_or_default();
                    if inner_alias_targets.contains_key(&qualifier.to_lowercase()) {
                        return Err(SqlError::Unsupported(format!(
                            "Qualified wildcard {}.* over nested join grouping is not supported",
                            qualifier
                        ))
                        .into());
                    }
                }
            }

            let mut used_inner_aliases: HashSet<String> = HashSet::new();

            fn visit_expr_for_inner_alias_refs(
                expr: &Expr,
                targets: &HashMap<String, InnerAliasTarget>,
                ambiguous: &HashSet<String>,
                used: &mut HashSet<String>,
            ) -> Result<()> {
                match expr {
                    Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
                        let table_ref = parts[0].value.to_lowercase();
                        if targets.contains_key(&table_ref) {
                            if ambiguous.contains(&table_ref) {
                                return Err(SqlError::Unsupported(format!(
                                    "Ambiguous nested join inner alias {}",
                                    parts[0].value
                                ))
                                .into());
                            }
                            used.insert(table_ref.clone());

                            let Some(target) = targets.get(&table_ref) else {
                                return Ok(());
                            };
                            let col_lower = parts[1].value.to_lowercase();
                            if target.duplicate_cols_lower.contains(&col_lower) {
                                return Err(SqlError::Unsupported(format!(
                                    "NestedJoin materialization cannot safely resolve {}.{} because derived output contains duplicate column name {}",
                                    parts[0].value,
                                    parts[1].value,
                                    parts[1].value
                                ))
                                .into());
                            }
                        }
                        Ok(())
                    }
                    Expr::BinaryOp { left, right, .. } => {
                        visit_expr_for_inner_alias_refs(left, targets, ambiguous, used)?;
                        visit_expr_for_inner_alias_refs(right, targets, ambiguous, used)
                    }
                    Expr::UnaryOp { expr: inner, .. }
                    | Expr::Nested(inner)
                    | Expr::IsNull(inner)
                    | Expr::IsNotNull(inner) => {
                        visit_expr_for_inner_alias_refs(inner, targets, ambiguous, used)
                    }
                    Expr::Function(f) => {
                        for arg in &f.args {
                            if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                                visit_expr_for_inner_alias_refs(e, targets, ambiguous, used)?;
                            }
                        }
                        Ok(())
                    }
                    Expr::Cast { expr: inner, .. } | Expr::TryCast { expr: inner, .. } => {
                        visit_expr_for_inner_alias_refs(inner, targets, ambiguous, used)
                    }
                    Expr::InList {
                        expr: inner, list, ..
                    } => {
                        visit_expr_for_inner_alias_refs(inner, targets, ambiguous, used)?;
                        for item in list {
                            visit_expr_for_inner_alias_refs(item, targets, ambiguous, used)?;
                        }
                        Ok(())
                    }
                    Expr::Between {
                        expr: inner,
                        low,
                        high,
                        ..
                    } => {
                        visit_expr_for_inner_alias_refs(inner, targets, ambiguous, used)?;
                        visit_expr_for_inner_alias_refs(low, targets, ambiguous, used)?;
                        visit_expr_for_inner_alias_refs(high, targets, ambiguous, used)
                    }
                    Expr::Case {
                        operand,
                        conditions,
                        results,
                        else_result,
                    } => {
                        if let Some(op) = operand.as_deref() {
                            visit_expr_for_inner_alias_refs(op, targets, ambiguous, used)?;
                        }
                        for c in conditions {
                            visit_expr_for_inner_alias_refs(c, targets, ambiguous, used)?;
                        }
                        for r in results {
                            visit_expr_for_inner_alias_refs(r, targets, ambiguous, used)?;
                        }
                        if let Some(e) = else_result.as_deref() {
                            visit_expr_for_inner_alias_refs(e, targets, ambiguous, used)?;
                        }
                        Ok(())
                    }
                    Expr::Subquery(_) | Expr::Exists { .. } => Ok(()),
                    _ => Ok(()),
                }
            }

            for step in &join_steps {
                if let Some(cond) = &step.condition {
                    visit_expr_for_inner_alias_refs(
                        cond,
                        &inner_alias_targets,
                        &ambiguous_inner_aliases,
                        &mut used_inner_aliases,
                    )?;
                }
            }
            if let Some(sel) = resolved_selection.as_ref() {
                visit_expr_for_inner_alias_refs(
                    sel,
                    &inner_alias_targets,
                    &ambiguous_inner_aliases,
                    &mut used_inner_aliases,
                )?;
            }
            for item in &resolved_projection {
                match item {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        visit_expr_for_inner_alias_refs(
                            expr,
                            &inner_alias_targets,
                            &ambiguous_inner_aliases,
                            &mut used_inner_aliases,
                        )?;
                    }
                    _ => {}
                }
            }
            for o in &query.order_by {
                visit_expr_for_inner_alias_refs(
                    &o.expr,
                    &inner_alias_targets,
                    &ambiguous_inner_aliases,
                    &mut used_inner_aliases,
                )?;
            }
            if let GroupByExpr::Expressions(exprs) = &select.group_by {
                for e in exprs {
                    visit_expr_for_inner_alias_refs(
                        e,
                        &inner_alias_targets,
                        &ambiguous_inner_aliases,
                        &mut used_inner_aliases,
                    )?;
                }
            }
            if let Some(having) = select.having.as_ref() {
                visit_expr_for_inner_alias_refs(
                    having,
                    &inner_alias_targets,
                    &ambiguous_inner_aliases,
                    &mut used_inner_aliases,
                )?;
            }
            if let Some(qualify) = select.qualify.as_ref() {
                visit_expr_for_inner_alias_refs(
                    qualify,
                    &inner_alias_targets,
                    &ambiguous_inner_aliases,
                    &mut used_inner_aliases,
                )?;
            }

            for inner_key in &used_inner_aliases {
                if let Some(target) = inner_alias_targets.get(inner_key) {
                    table_aliases.push((
                        target.derived_alias.clone(),
                        TableSchema {
                            name: target.inner_alias.clone(),
                            table_id: 0,
                            columns: Vec::new(),
                            version: 1,
                            pk_constraint_name: None,
                            pk_indices: Vec::new(),
                            indexes: Vec::new(),
                            check_constraints: Vec::new(),
                            foreign_keys: Vec::new(),
                            owner: String::new(),
                        },
                    ));
                }
            }
            requires_transparent_nested_join_mapping = !used_inner_aliases.is_empty();
        }

        if tables.len() == 2
            && !is_implicit_join
            && !needs_aggregation
            && !has_distinct
            && !requires_transparent_nested_join_mapping
            && merge_columns.is_empty()
            && tables[0].preloaded_rows.is_none()
            && tables[1].preloaded_rows.is_none()
        {
            let limit = extract_limit(query);
            let offset = extract_offset(query);
            let left_preloaded = tables[0].preloaded_rows.take();
            let right_preloaded = tables[1].preloaded_rows.take();
            let result = self
                .execute_simple_join_with_operators(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    tables[0].schema.clone(),
                    tables[1].schema.clone(),
                    &tables[0].alias,
                    &tables[1].alias,
                    join_steps[0].join_type,
                    join_steps[0].condition.clone(),
                    resolved_selection.as_ref(),
                    &query.order_by,
                    limit,
                    offset,
                    &resolved_projection,
                    left_preloaded,
                    right_preloaded,
                )
                .await?;
            return Ok(Some(result));
        }

        // Multi-JOIN: build a left-deep operator tree
        // Build combined schema incrementally
        let mut combined_columns: Vec<ColumnDef> = Vec::new();
        for col in &tables[0].schema.columns {
            combined_columns.push(ColumnDef {
                name: format!("{}.{}", tables[0].alias, col.name),
                data_type: col.data_type.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            });
        }

        let mut running_op: BoxedOperator = if let Some(rows) = tables[0].preloaded_rows.take() {
            Box::new(TableScanOperator::new_with_rows(
                tables[0].schema.clone(),
                rows,
            ))
        } else {
            Box::new(TableScanOperator::new(tables[0].schema.clone()))
        };

        for step in &join_steps {
            let right_preloaded_rows = tables[step.right_idx].preloaded_rows.take();
            let right = &tables[step.right_idx];
            for col in &right.schema.columns {
                combined_columns.push(ColumnDef {
                    name: format!("{}.{}", right.alias, col.name),
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
                columns: combined_columns.clone(),
                version: 1,
                pk_constraint_name: None,
                pk_indices: vec![],
                indexes: vec![],
                check_constraints: vec![],
                foreign_keys: vec![],
                owner: String::new(),
            };

            let right_op: BoxedOperator = if let Some(rows) = right_preloaded_rows {
                Box::new(TableScanOperator::new_with_rows(right.schema.clone(), rows))
            } else {
                Box::new(TableScanOperator::new(right.schema.clone()))
            };

            let rewritten_condition = match &step.condition {
                Some(cond) => Some(rewrite_expr_for_multi_join(cond, &table_aliases)?),
                None => None,
            };

            // Try hash join for equi-join conditions
            let join_algo = choose_join_algorithm(
                step.condition.as_ref(),
                running_op.schema(),
                &right.schema,
                1000,
                1000,
                &HashJoinConfig::default(),
            );

            running_op = match join_algo {
                JoinAlgorithmChoice::HashJoin {
                    left_is_build,
                    left_key_indices,
                    right_key_indices,
                } => {
                    let hash_join_type = match step.join_type {
                        JoinType::Inner => HashJoinType::Inner,
                        JoinType::Left => HashJoinType::Left,
                        JoinType::Right => HashJoinType::Right,
                        JoinType::Full => HashJoinType::Full,
                        JoinType::Cross => HashJoinType::Inner,
                    };
                    Box::new(
                        HashJoinOperator::new(
                            running_op,
                            right_op,
                            hash_join_type,
                            left_key_indices,
                            right_key_indices,
                            left_is_build,
                            None,
                            HashJoinConfig::default(),
                        )
                        .with_output_schema(combined_schema),
                    )
                }
                JoinAlgorithmChoice::NestedLoop => Box::new(NestedLoopJoinOperator::with_schema(
                    running_op,
                    right_op,
                    step.join_type,
                    rewritten_condition,
                    combined_schema,
                )),
            };
        }

        if let Some(filter) = &resolved_selection {
            let rewritten = rewrite_for_using_join(filter, &table_aliases, &merge_columns)?;
            running_op = Box::new(FilterOperator::new(running_op, rewritten));
        }

        if needs_aggregation {
            return self
                .execute_join_aggregate_path(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    running_op,
                    query,
                    select,
                    &resolved_projection,
                    &table_aliases,
                    &merge_columns,
                )
                .await;
        }

        let has_window_funcs = resolved_projection.iter().any(|item| {
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
            return self
                .execute_join_window_path(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    running_op,
                    query,
                    &resolved_projection,
                    &table_aliases,
                    &merge_columns,
                )
                .await;
        }

        let final_schema = running_op.schema().clone();
        let mut rewritten_order_by: Vec<sqlparser::ast::OrderByExpr> = Vec::new();

        let wildcard_plan = if !merge_columns.is_empty() {
            let source_schemas: Vec<&TableSchema> = tables.iter().map(|t| &t.schema).collect();
            build_join_wildcard_plan(select, &source_schemas)
        } else {
            None
        };

        let source_offsets: Vec<usize> = {
            let mut offsets = Vec::with_capacity(tables.len());
            let mut offset = 0;
            for t in &tables {
                offsets.push(offset);
                offset += t.schema.columns.len();
            }
            offsets
        };

        let mut projection_exprs: Vec<Expr> = Vec::new();
        let mut alias_exprs: HashMap<String, Expr> = HashMap::new();
        for item in &resolved_projection {
            match item {
                SelectItem::Wildcard(_) => {
                    if let Some(ref plan) = wildcard_plan {
                        for wc in &plan.columns {
                            let mc = merge_columns
                                .iter()
                                .find(|mc| mc.col_name.eq_ignore_ascii_case(&wc.name));
                            let is_merge_source = mc.as_ref().map_or(false, |mc| {
                                let wc_alias = tables
                                    .get(wc.source_idx)
                                    .map(|t| t.alias.as_str())
                                    .unwrap_or("");
                                mc.source_aliases
                                    .iter()
                                    .any(|a| a.eq_ignore_ascii_case(wc_alias))
                            });
                            if let Some(mc) = mc.filter(|_| is_merge_source) {
                                projection_exprs.push(build_coalesce_for_merge(mc));
                            } else {
                                let combined_idx = source_offsets[wc.source_idx] + wc.col_idx;
                                projection_exprs.push(Expr::Identifier(Ident::new(
                                    final_schema.columns[combined_idx].name.clone(),
                                )));
                            }
                        }
                    } else {
                        for col in &final_schema.columns {
                            projection_exprs.push(Expr::Identifier(Ident::new(col.name.clone())));
                        }
                    }
                }
                SelectItem::QualifiedWildcard(obj, _) => {
                    let qualifier = obj.0.last().map(|i| i.value.clone()).unwrap_or_default();
                    let (table_idx, table) = tables
                        .iter()
                        .enumerate()
                        .find(|(_, t)| t.alias.eq_ignore_ascii_case(&qualifier))
                        .ok_or_else(|| {
                            anyhow!(
                                "Qualified wildcard {}.* not found in join output",
                                qualifier
                            )
                        })?;

                    for (col_idx, col) in table.schema.columns.iter().enumerate() {
                        if col.name.starts_with("__tipg_subquery_") {
                            continue;
                        }
                        let mc = merge_columns
                            .iter()
                            .find(|mc| mc.col_name.eq_ignore_ascii_case(&col.name));
                        let is_merge_source = mc.as_ref().map_or(false, |mc| {
                            mc.source_aliases
                                .iter()
                                .any(|a| a.eq_ignore_ascii_case(&table.alias))
                        });
                        if let Some(mc) = mc.filter(|_| is_merge_source) {
                            projection_exprs.push(build_coalesce_for_merge(mc));
                        } else {
                            let combined_idx = source_offsets[table_idx] + col_idx;
                            projection_exprs.push(Expr::Identifier(Ident::new(
                                final_schema.columns[combined_idx].name.clone(),
                            )));
                        }
                    }
                }
                SelectItem::UnnamedExpr(expr) => {
                    projection_exprs.push(rewrite_for_using_join(
                        expr,
                        &table_aliases,
                        &merge_columns,
                    )?);
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    let rewritten = rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
                    alias_exprs.insert(alias.value.to_lowercase(), rewritten.clone());
                    projection_exprs.push(rewritten);
                }
            }
        }

        for o in &query.order_by {
            let expr = if let Expr::Identifier(ident) = &o.expr {
                if let Some(e) = alias_exprs.get(&ident.value.to_lowercase()) {
                    e.clone()
                } else {
                    rewrite_for_using_join(&o.expr, &table_aliases, &merge_columns)?
                }
            } else if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = &o.expr {
                if let Ok(pos) = n.parse::<usize>() {
                    if pos == 0 || pos > projection_exprs.len() {
                        return Err(anyhow!("ORDER BY position {} is not in select list", pos));
                    }
                    projection_exprs[pos - 1].clone()
                } else {
                    rewrite_for_using_join(&o.expr, &table_aliases, &merge_columns)?
                }
            } else {
                rewrite_for_using_join(&o.expr, &table_aliases, &merge_columns)?
            };
            rewritten_order_by.push(sqlparser::ast::OrderByExpr {
                expr,
                asc: o.asc,
                nulls_first: o.nulls_first,
            });
        }

        if !rewritten_order_by.is_empty() {
            running_op = Box::new(SortOperator::new(running_op, rewritten_order_by));
        }

        let limit = extract_limit(query);
        let offset = extract_offset(query);
        if limit.is_some() || offset > 0 {
            running_op = Box::new(LimitOperator::new(running_op, limit, offset));
        }

        let rows = execute_operator_tree(
            &mut running_op,
            txn,
            self.store(),
            db_id,
            search_path,
            sequence_values,
        )
        .await?;

        let has_unqualified_wildcard = resolved_projection
            .iter()
            .any(|p| matches!(p, SelectItem::Wildcard(_)));
        let has_qualified_wildcard = resolved_projection
            .iter()
            .any(|p| matches!(p, SelectItem::QualifiedWildcard(_, _)));
        let has_wildcard = has_unqualified_wildcard || has_qualified_wildcard;

        enum ProjectionSource {
            ColumnIndex(usize),
            Expr(Expr),
            CoalesceColumn(Vec<usize>),
        }

        let (columns, column_types, projected_rows) = if has_unqualified_wildcard
            && wildcard_plan.is_some()
            && !has_qualified_wildcard
        {
            let plan = wildcard_plan.as_ref().unwrap();
            let cols: Vec<String> = plan.columns.iter().map(|c| c.name.clone()).collect();
            let types: Vec<DataType> = plan.columns.iter().map(|c| c.data_type.clone()).collect();
            let projected: Vec<Row> = rows
                .iter()
                .map(|row| {
                    let values: Vec<Value> = plan
                        .columns
                        .iter()
                        .map(|wc| {
                            let mc = merge_columns
                                .iter()
                                .find(|mc| mc.col_name.eq_ignore_ascii_case(&wc.name));
                            let is_merge_source = mc.as_ref().map_or(false, |mc| {
                                let wc_alias = tables
                                    .get(wc.source_idx)
                                    .map(|t| t.alias.as_str())
                                    .unwrap_or("");
                                mc.source_aliases
                                    .iter()
                                    .any(|a| a.eq_ignore_ascii_case(wc_alias))
                            });
                            if let Some(mc) = mc.filter(|_| is_merge_source) {
                                for sa in &mc.source_aliases {
                                    if let Some(ti) =
                                        tables.iter().position(|t| t.alias.eq_ignore_ascii_case(sa))
                                    {
                                        if let Some(ci) = tables[ti]
                                            .schema
                                            .columns
                                            .iter()
                                            .position(|c| c.name.eq_ignore_ascii_case(&wc.name))
                                        {
                                            let idx = source_offsets[ti] + ci;
                                            if let Some(val) = row.values.get(idx) {
                                                if *val != Value::Null {
                                                    return val.clone();
                                                }
                                            }
                                        }
                                    }
                                }
                                Value::Null
                            } else {
                                let idx = source_offsets[wc.source_idx] + wc.col_idx;
                                row.values.get(idx).cloned().unwrap_or(Value::Null)
                            }
                        })
                        .collect();
                    Row::new(values)
                })
                .collect();
            (cols, types, projected)
        } else if has_wildcard && merge_columns.is_empty() {
            let mut cols: Vec<String> = Vec::new();
            let mut types: Vec<DataType> = Vec::new();
            let mut sources: Vec<ProjectionSource> = Vec::new();

            for item in &resolved_projection {
                match item {
                    SelectItem::Wildcard(_) => {
                        for (idx, c) in final_schema.columns.iter().enumerate() {
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
                        for (idx, c) in final_schema.columns.iter().enumerate() {
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
                        let rewritten =
                            rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
                        cols.push(original_name);
                        types.push(infer_expr_type(&rewritten, &final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                    SelectItem::ExprWithAlias { expr, alias } => {
                        let rewritten =
                            rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
                        cols.push(alias.value.clone());
                        types.push(infer_expr_type(&rewritten, &final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                }
            }

            let mut projected: Vec<Row> = Vec::with_capacity(rows.len());
            for row in rows {
                let mut values: Vec<Value> = Vec::with_capacity(sources.len());
                for src in &sources {
                    let val = match src {
                        ProjectionSource::ColumnIndex(idx) => {
                            row.values.get(*idx).cloned().unwrap_or(Value::Null)
                        }
                        ProjectionSource::Expr(expr) => {
                            eval_expr(expr, Some(&row), Some(&final_schema))?
                        }
                        ProjectionSource::CoalesceColumn(indices) => {
                            let mut out = Value::Null;
                            for idx in indices {
                                if let Some(val) = row.values.get(*idx) {
                                    if *val != Value::Null {
                                        out = val.clone();
                                        break;
                                    }
                                }
                            }
                            out
                        }
                    };
                    values.push(val);
                }
                projected.push(Row::new(values));
            }

            (cols, types, projected)
        } else if has_qualified_wildcard {
            let mut cols: Vec<String> = Vec::new();
            let mut types: Vec<DataType> = Vec::new();
            let mut sources: Vec<ProjectionSource> = Vec::new();

            for item in &resolved_projection {
                match item {
                    SelectItem::Wildcard(_) => {
                        if let Some(plan) = wildcard_plan.as_ref() {
                            for wc in &plan.columns {
                                cols.push(wc.name.clone());
                                types.push(wc.data_type.clone());

                                let mc = merge_columns
                                    .iter()
                                    .find(|mc| mc.col_name.eq_ignore_ascii_case(&wc.name));
                                let is_merge_source = mc.as_ref().map_or(false, |mc| {
                                    let wc_alias = tables
                                        .get(wc.source_idx)
                                        .map(|t| t.alias.as_str())
                                        .unwrap_or("");
                                    mc.source_aliases
                                        .iter()
                                        .any(|a| a.eq_ignore_ascii_case(wc_alias))
                                });

                                if let Some(mc) = mc.filter(|_| is_merge_source) {
                                    let mut indices: Vec<usize> =
                                        Vec::with_capacity(mc.source_aliases.len());
                                    for sa in &mc.source_aliases {
                                        if let Some(ti) = tables
                                            .iter()
                                            .position(|t| t.alias.eq_ignore_ascii_case(sa))
                                        {
                                            if let Some(ci) =
                                                tables[ti].schema.columns.iter().position(|c| {
                                                    c.name.eq_ignore_ascii_case(&wc.name)
                                                })
                                            {
                                                indices.push(source_offsets[ti] + ci);
                                            }
                                        }
                                    }
                                    sources.push(ProjectionSource::CoalesceColumn(indices));
                                } else {
                                    let idx = source_offsets[wc.source_idx] + wc.col_idx;
                                    sources.push(ProjectionSource::ColumnIndex(idx));
                                }
                            }
                        } else {
                            for (idx, c) in final_schema.columns.iter().enumerate() {
                                let unqualified = c.name.split('.').last().unwrap_or(&c.name);
                                if unqualified.starts_with("__tipg_subquery_") {
                                    continue;
                                }
                                cols.push(unqualified.to_string());
                                types.push(c.data_type.clone());
                                sources.push(ProjectionSource::ColumnIndex(idx));
                            }
                        }
                    }
                    SelectItem::QualifiedWildcard(obj, _) => {
                        let qualifier = obj.0.last().map(|i| i.value.clone()).unwrap_or_default();
                        let mut matched = false;
                        for (idx, c) in final_schema.columns.iter().enumerate() {
                            let prefix = c.name.split('.').next().unwrap_or(&c.name);
                            if prefix.eq_ignore_ascii_case(&qualifier) {
                                let unqualified = c.name.split('.').last().unwrap_or(&c.name);
                                if unqualified.starts_with("__tipg_subquery_") {
                                    continue;
                                }
                                matched = true;
                                cols.push(unqualified.to_string());
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
                        let rewritten =
                            rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
                        cols.push(original_name);
                        types.push(infer_expr_type(&rewritten, &final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                    SelectItem::ExprWithAlias { expr, alias } => {
                        let rewritten =
                            rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
                        cols.push(alias.value.clone());
                        types.push(infer_expr_type(&rewritten, &final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                }
            }

            let mut projected: Vec<Row> = Vec::with_capacity(rows.len());
            for row in rows {
                let mut values: Vec<Value> = Vec::with_capacity(sources.len());
                for src in &sources {
                    let val = match src {
                        ProjectionSource::ColumnIndex(idx) => {
                            row.values.get(*idx).cloned().unwrap_or(Value::Null)
                        }
                        ProjectionSource::Expr(expr) => {
                            eval_expr(expr, Some(&row), Some(&final_schema))?
                        }
                        ProjectionSource::CoalesceColumn(indices) => {
                            let mut out = Value::Null;
                            for idx in indices {
                                if let Some(val) = row.values.get(*idx) {
                                    if *val != Value::Null {
                                        out = val.clone();
                                        break;
                                    }
                                }
                            }
                            out
                        }
                    };
                    values.push(val);
                }
                projected.push(Row::new(values));
            }

            (cols, types, projected)
        } else if has_qualified_wildcard && !has_unqualified_wildcard && !merge_columns.is_empty() {
            let mut cols: Vec<String> = Vec::new();
            let mut types: Vec<DataType> = Vec::new();
            let mut sources: Vec<ProjectionSource> = Vec::new();

            for item in &resolved_projection {
                match item {
                    SelectItem::QualifiedWildcard(obj, _) => {
                        let qualifier = obj.0.last().map(|i| i.value.clone()).unwrap_or_default();
                        let (table_idx, table) = tables
                            .iter()
                            .enumerate()
                            .find(|(_, t)| t.alias.eq_ignore_ascii_case(&qualifier))
                            .ok_or_else(|| {
                                anyhow!(
                                    "Qualified wildcard {}.* not found in join output",
                                    qualifier
                                )
                            })?;

                        for (col_idx, col) in table.schema.columns.iter().enumerate() {
                            if col.name.starts_with("__tipg_subquery_") {
                                continue;
                            }

                            cols.push(col.name.clone());
                            types.push(col.data_type.clone());

                            let mc = merge_columns
                                .iter()
                                .find(|mc| mc.col_name.eq_ignore_ascii_case(&col.name));
                            let is_merge_source = mc.as_ref().map_or(false, |mc| {
                                mc.source_aliases
                                    .iter()
                                    .any(|a| a.eq_ignore_ascii_case(&table.alias))
                            });
                            if let Some(mc) = mc.filter(|_| is_merge_source) {
                                sources.push(ProjectionSource::Expr(build_coalesce_for_merge(mc)));
                            } else {
                                let idx = source_offsets[table_idx] + col_idx;
                                sources.push(ProjectionSource::ColumnIndex(idx));
                            }
                        }
                    }
                    SelectItem::UnnamedExpr(expr) => {
                        let original_name = get_select_item_name(item);
                        let rewritten =
                            rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
                        cols.push(original_name);
                        types.push(infer_expr_type(&rewritten, &final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                    SelectItem::ExprWithAlias { expr, alias } => {
                        let rewritten =
                            rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
                        cols.push(alias.value.clone());
                        types.push(infer_expr_type(&rewritten, &final_schema));
                        sources.push(ProjectionSource::Expr(rewritten));
                    }
                    SelectItem::Wildcard(_) => {
                        return Err(anyhow!(
                            "internal error: expected qualified wildcard handling only"
                        ));
                    }
                }
            }

            let mut projected: Vec<Row> = Vec::with_capacity(rows.len());
            for row in rows {
                let mut values: Vec<Value> = Vec::with_capacity(sources.len());
                for src in &sources {
                    let val = match src {
                        ProjectionSource::ColumnIndex(idx) => {
                            row.values.get(*idx).cloned().unwrap_or(Value::Null)
                        }
                        ProjectionSource::Expr(expr) => {
                            eval_expr(expr, Some(&row), Some(&final_schema))?
                        }
                        ProjectionSource::CoalesceColumn(indices) => {
                            let mut out = Value::Null;
                            for idx in indices {
                                if let Some(val) = row.values.get(*idx) {
                                    if *val != Value::Null {
                                        out = val.clone();
                                        break;
                                    }
                                }
                            }
                            out
                        }
                    };
                    values.push(val);
                }
                projected.push(Row::new(values));
            }

            (cols, types, projected)
        } else if has_unqualified_wildcard {
            let cols: Vec<String> = final_schema
                .columns
                .iter()
                .map(|c| c.name.split('.').last().unwrap_or(&c.name).to_string())
                .collect();
            let types: Vec<DataType> = final_schema
                .columns
                .iter()
                .map(|c| c.data_type.clone())
                .collect();
            (cols, types, rows)
        } else {
            let mut rewritten_projection: Vec<SelectItem> =
                Vec::with_capacity(resolved_projection.len());
            for item in &resolved_projection {
                match item {
                    SelectItem::UnnamedExpr(expr) => {
                        let original_name = get_select_item_name(item);
                        let rewritten =
                            rewrite_for_using_join(expr, &table_aliases, &merge_columns)?;
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
                            expr: rewrite_for_using_join(expr, &table_aliases, &merge_columns)?,
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
                        infer_expr_type(expr, &final_schema)
                    }
                    _ => DataType::Text,
                })
                .collect();

            fn unnest_arg_expr<'a>(expr: &'a Expr) -> Option<&'a Expr> {
                match expr {
                    Expr::Function(f) => {
                        let Some(name) = f.name.0.last() else {
                            return None;
                        };
                        if !name.value.eq_ignore_ascii_case("UNNEST") {
                            return None;
                        }
                        f.args.first().and_then(|arg| match arg {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                            _ => None,
                        })
                    }
                    Expr::Nested(inner) => unnest_arg_expr(inner),
                    _ => None,
                }
            }

            let mut projected = Vec::with_capacity(rows.len());
            for row in rows {
                let mut row_values = Vec::with_capacity(rewritten_projection.len());
                let mut srf_outputs: Vec<(usize, Vec<Value>)> = Vec::new();

                for item in &rewritten_projection {
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                        _ => continue,
                    };

                    if let Some(arg_expr) = unnest_arg_expr(expr) {
                        let outputs = match eval_expr(arg_expr, Some(&row), Some(&final_schema))? {
                            Value::Array(arr) => arr,
                            Value::Null => Vec::new(),
                            other => vec![other],
                        };
                        srf_outputs.push((row_values.len(), outputs));
                        row_values.push(Value::Null);
                        continue;
                    }

                    let val = eval_expr(expr, Some(&row), Some(&final_schema))?;
                    row_values.push(val);
                }

                if srf_outputs.is_empty() {
                    projected.push(Row::new(row_values));
                    continue;
                }

                let max_len = srf_outputs
                    .iter()
                    .map(|(_, outputs)| outputs.len())
                    .max()
                    .unwrap_or(0);
                for idx in 0..max_len {
                    let mut expanded = row_values.clone();
                    for (col_idx, outputs) in &srf_outputs {
                        expanded[*col_idx] = outputs.get(idx).cloned().unwrap_or(Value::Null);
                    }
                    projected.push(Row::new(expanded));
                }
            }
            (cols, types, projected)
        };

        let projected_rows = if matches!(&select.distinct, Some(Distinct::Distinct)) {
            dedup_rows(projected_rows)
        } else {
            projected_rows
        };

        Ok(Some(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows: projected_rows,
            timezone: crate::session_context::current_timezone(),
        }))
    }

    async fn execute_lateral_join(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        select: &sqlparser::ast::Select,
        resolved_projection: &[SelectItem],
        resolved_selection: Option<&Expr>,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        use super::subquery::substitute_outer_values_in_query;
        use crate::types::ColumnDef;

        let virtual_filter = resolved_selection
            .map(extract_virtual_table_filter)
            .unwrap_or_default();

        let from_item = &select.from[0];
        let resolved_outer = self
            .resolve_join_table_factor(
                txn,
                db_id,
                sequence_values,
                search_path,
                &from_item.relation,
                ctes,
                &virtual_filter,
            )
            .await?;
        let (outer_alias, outer_schema, outer_preloaded) = match resolved_outer {
            Some(t) => t,
            None => return Err(anyhow!("LATERAL JOIN: could not resolve outer table")),
        };

        let outer_rows = if let Some(rows) = outer_preloaded {
            rows
        } else {
            let (_, rows) = self
                .get_table_data(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &outer_schema.name,
                    ctes,
                )
                .await?;
            rows
        };

        let mut combined_rows: Vec<Row>;
        let mut table_aliases: Vec<(String, TableSchema)> =
            vec![(outer_alias.clone(), outer_schema.clone())];
        let mut combined_columns: Vec<ColumnDef> = Vec::new();
        for col in &outer_schema.columns {
            combined_columns.push(ColumnDef {
                name: format!("{}.{}", outer_alias, col.name),
                data_type: col.data_type.clone(),
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            });
        }

        let mut current_rows: Vec<Row> = outer_rows.clone();

        for join in &from_item.joins {
            let jt = JoinType::from(&join.join_operator);

            if let TableFactor::Derived {
                lateral: true,
                subquery,
                alias,
                ..
            } = &join.relation
            {
                let lateral_alias_name = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| "lateral".to_string());
                let alias_cols = alias
                    .as_ref()
                    .map(|a| a.columns.clone())
                    .unwrap_or_default();

                let mut lateral_schema: Option<TableSchema> = None;
                let mut new_rows: Vec<Row> = Vec::new();

                for left_row in &current_rows {
                    let outer_row =
                        Row::new(left_row.values[..outer_schema.columns.len()].to_vec());
                    let substituted = substitute_outer_values_in_query(
                        subquery,
                        &outer_alias,
                        &outer_schema,
                        &outer_row,
                    );

                    let result = self
                        .execute_query_with_outer_ctes(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &substituted,
                            ctes,
                        )
                        .await?;

                    let (cols, _col_types, sub_rows) = match result {
                        ExecuteResult::Select {
                            columns,
                            rows,
                            column_types,
                            ..
                        } => (columns, column_types, rows),
                        _ => continue,
                    };

                    if lateral_schema.is_none() {
                        let col_names: Vec<String> = if alias_cols.is_empty() {
                            cols
                        } else {
                            alias_cols.iter().map(|c| c.value.clone()).collect()
                        };
                        let inferred_types: Vec<DataType> = if let Some(first) = sub_rows.first() {
                            first
                                .values
                                .iter()
                                .map(|v| v.data_type().unwrap_or(DataType::Text))
                                .collect()
                        } else {
                            vec![DataType::Text; col_names.len()]
                        };
                        lateral_schema = Some(TableSchema {
                            table_id: 0,
                            name: lateral_alias_name.clone(),
                            columns: col_names
                                .iter()
                                .zip(inferred_types.iter())
                                .map(|(n, dt)| ColumnDef {
                                    name: n.clone(),
                                    data_type: dt.clone(),
                                    nullable: true,
                                    primary_key: false,
                                    unique: false,
                                    is_serial: false,
                                    default_expr: None,
                                })
                                .collect(),
                            pk_constraint_name: None,
                            pk_indices: vec![],
                            indexes: vec![],
                            version: 1,
                            check_constraints: vec![],
                            foreign_keys: vec![],
                            owner: String::new(),
                        });
                    }

                    if sub_rows.is_empty() {
                        if matches!(jt, JoinType::Left) {
                            let lat_cols = lateral_schema
                                .as_ref()
                                .map(|s| s.columns.len())
                                .unwrap_or(0);
                            let mut values = left_row.values.clone();
                            values.extend(std::iter::repeat(Value::Null).take(lat_cols));
                            new_rows.push(Row::new(values));
                        }
                    } else {
                        for sub_row in &sub_rows {
                            let mut values = left_row.values.clone();
                            values.extend(sub_row.values.iter().cloned());
                            new_rows.push(Row::new(values));
                        }
                    }
                }

                let lat_schema = lateral_schema.unwrap_or_else(|| TableSchema {
                    table_id: 0,
                    name: lateral_alias_name.clone(),
                    columns: vec![],
                    pk_constraint_name: None,
                    pk_indices: vec![],
                    indexes: vec![],
                    version: 1,
                    check_constraints: vec![],
                    foreign_keys: vec![],
                    owner: String::new(),
                });

                for col in &lat_schema.columns {
                    combined_columns.push(ColumnDef {
                        name: format!("{}.{}", lat_schema.name, col.name),
                        data_type: col.data_type.clone(),
                        nullable: true,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    });
                }
                table_aliases.push((lat_schema.name.clone(), lat_schema));
                current_rows = new_rows;
            } else {
                let resolved = self
                    .resolve_join_table_factor(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &join.relation,
                        ctes,
                        &virtual_filter,
                    )
                    .await?;
                let (right_alias, right_schema, right_preloaded) = match resolved {
                    Some(t) => t,
                    None => {
                        return Err(anyhow!(
                            "LATERAL JOIN: could not resolve table in join chain"
                        ))
                    }
                };

                let right_rows = if let Some(rows) = right_preloaded {
                    rows
                } else {
                    let (_, rows) = self
                        .get_table_data(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &right_schema.name,
                            ctes,
                        )
                        .await?;
                    rows
                };

                let condition = match &join.join_operator {
                    sqlparser::ast::JoinOperator::Inner(JoinConstraint::On(expr))
                    | sqlparser::ast::JoinOperator::LeftOuter(JoinConstraint::On(expr))
                    | sqlparser::ast::JoinOperator::RightOuter(JoinConstraint::On(expr))
                    | sqlparser::ast::JoinOperator::FullOuter(JoinConstraint::On(expr)) => {
                        Some(expr.clone())
                    }
                    sqlparser::ast::JoinOperator::CrossJoin => None,
                    _ => None,
                };

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

                let temp_combined_schema = TableSchema {
                    name: "join_result".to_string(),
                    table_id: 0,
                    columns: combined_columns.clone(),
                    version: 1,
                    pk_constraint_name: None,
                    pk_indices: vec![],
                    indexes: vec![],
                    check_constraints: vec![],
                    foreign_keys: vec![],
                    owner: String::new(),
                };

                let mut temp_table_aliases = table_aliases.clone();
                temp_table_aliases.push((right_alias.clone(), right_schema.clone()));

                let rewritten_condition = condition.as_ref().map(|cond| {
                    rewrite_expr_for_multi_join(cond, &temp_table_aliases)
                        .unwrap_or_else(|_| cond.clone())
                });

                let mut new_rows: Vec<Row> = Vec::new();
                for left_row in &current_rows {
                    let mut matched = false;
                    for right_row in &right_rows {
                        let mut combined = left_row.values.clone();
                        combined.extend(right_row.values.iter().cloned());
                        let combined_row = Row::new(combined);

                        let passes = match &rewritten_condition {
                            Some(cond) => matches!(
                                eval_expr(cond, Some(&combined_row), Some(&temp_combined_schema)),
                                Ok(Value::Boolean(true))
                            ),
                            None => true,
                        };

                        if passes {
                            matched = true;
                            new_rows.push(combined_row);
                        }
                    }
                    if !matched && matches!(jt, JoinType::Left) {
                        let mut values = left_row.values.clone();
                        values.extend(
                            std::iter::repeat(Value::Null).take(right_schema.columns.len()),
                        );
                        new_rows.push(Row::new(values));
                    }
                }

                table_aliases.push((right_alias, right_schema));
                current_rows = new_rows;
            }
        }

        combined_rows = current_rows;

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

        let merge_columns: Vec<UsingMergeColumn> = Vec::new();

        if let Some(filter) = resolved_selection {
            let rewritten = rewrite_for_using_join(filter, &table_aliases, &merge_columns)?;
            combined_rows.retain(|row| {
                matches!(
                    eval_expr(&rewritten, Some(row), Some(&combined_schema)),
                    Ok(Value::Boolean(true))
                )
            });
        }

        let rewritten_projection: Vec<SelectItem> = resolved_projection
            .iter()
            .map(|item| match item {
                SelectItem::UnnamedExpr(e) => {
                    let rewritten = rewrite_for_using_join(e, &table_aliases, &merge_columns)
                        .unwrap_or_else(|_| e.clone());
                    SelectItem::UnnamedExpr(rewritten)
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    let rewritten = rewrite_for_using_join(expr, &table_aliases, &merge_columns)
                        .unwrap_or_else(|_| expr.clone());
                    SelectItem::ExprWithAlias {
                        expr: rewritten,
                        alias: alias.clone(),
                    }
                }
                SelectItem::Wildcard(_) => {
                    SelectItem::Wildcard(sqlparser::ast::WildcardAdditionalOptions::default())
                }
                other => other.clone(),
            })
            .collect();

        if !query.order_by.is_empty() {
            let order_exprs: Vec<_> = query
                .order_by
                .iter()
                .map(|o| {
                    let rewritten = rewrite_for_using_join(&o.expr, &table_aliases, &merge_columns)
                        .unwrap_or_else(|_| o.expr.clone());
                    (rewritten, o.asc.unwrap_or(true))
                })
                .collect();

            combined_rows.sort_by(|a, b| {
                for (expr, asc) in &order_exprs {
                    let va =
                        eval_expr(expr, Some(a), Some(&combined_schema)).unwrap_or(Value::Null);
                    let vb =
                        eval_expr(expr, Some(b), Some(&combined_schema)).unwrap_or(Value::Null);
                    let cmp_val = super::super::expr::compare_values(&va, &vb).unwrap_or(0);
                    let cmp = match cmp_val {
                        n if n < 0 => std::cmp::Ordering::Less,
                        0 => std::cmp::Ordering::Equal,
                        _ => std::cmp::Ordering::Greater,
                    };
                    let cmp = if *asc { cmp } else { cmp.reverse() };
                    if cmp != std::cmp::Ordering::Equal {
                        return cmp;
                    }
                }
                std::cmp::Ordering::Equal
            });
        }

        let offset = extract_offset(query);
        let limit = extract_limit(query);
        if offset > 0 {
            combined_rows = combined_rows.into_iter().skip(offset).collect();
        }
        if let Some(lim) = limit {
            combined_rows.truncate(lim);
        }

        let is_wildcard = rewritten_projection
            .iter()
            .any(|item| matches!(item, SelectItem::Wildcard(_)));

        let (columns, column_types, final_rows) = if is_wildcard {
            let cols: Vec<String> = combined_schema
                .columns
                .iter()
                .map(|c| c.name.split('.').last().unwrap_or(&c.name).to_string())
                .collect();
            let types: Vec<DataType> = combined_schema
                .columns
                .iter()
                .map(|c| c.data_type.clone())
                .collect();
            (cols, types, combined_rows)
        } else {
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
            let mut projected = Vec::with_capacity(combined_rows.len());
            for row in &combined_rows {
                let mut values = Vec::with_capacity(rewritten_projection.len());
                for item in &rewritten_projection {
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                        _ => continue,
                    };
                    let val = eval_expr(expr, Some(row), Some(&combined_schema))?;
                    values.push(val);
                }
                projected.push(Row::new(values));
            }
            (cols, types, projected)
        };

        Ok(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows: final_rows,
            timezone: crate::session_context::current_timezone(),
        })
    }

    async fn execute_join_aggregate_path(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        mut running_op: BoxedOperator,
        query: &Query,
        select: &sqlparser::ast::Select,
        resolved_projection: &[SelectItem],
        table_aliases: &[(String, TableSchema)],
        merge_columns: &[UsingMergeColumn],
    ) -> Result<Option<ExecuteResult>> {
        let join_schema = running_op.schema().clone();

        let rewritten_projection: Vec<SelectItem> = resolved_projection
            .iter()
            .map(|item| match item {
                SelectItem::UnnamedExpr(expr) => {
                    let original_name = get_select_item_name(item);
                    let rewritten = rewrite_for_using_join(expr, table_aliases, merge_columns)?;
                    let rewritten_name =
                        get_select_item_name(&SelectItem::UnnamedExpr(rewritten.clone()));
                    if rewritten_name != original_name {
                        Ok(SelectItem::ExprWithAlias {
                            expr: rewritten,
                            alias: Ident::new(original_name),
                        })
                    } else {
                        Ok(SelectItem::UnnamedExpr(rewritten))
                    }
                }
                SelectItem::ExprWithAlias { expr, alias } => Ok(SelectItem::ExprWithAlias {
                    expr: rewrite_for_using_join(expr, table_aliases, merge_columns)?,
                    alias: alias.clone(),
                }),
                other => Ok(other.clone()),
            })
            .collect::<Result<Vec<_>>>()?;

        let (mut group_by_exprs, mut group_by_names, mut group_by_types) = {
            let raw_exprs = match &select.group_by {
                GroupByExpr::Expressions(exprs) => exprs.clone(),
                GroupByExpr::All => Vec::new(),
            };
            let mut exprs = Vec::new();
            let mut names = Vec::new();
            let mut types = Vec::new();
            for expr in &raw_exprs {
                let rewritten = rewrite_for_using_join(expr, table_aliases, merge_columns)?;
                let name = match &rewritten {
                    Expr::Identifier(id) => id.value.clone(),
                    Expr::CompoundIdentifier(parts) => parts
                        .iter()
                        .map(|p| p.value.as_str())
                        .collect::<Vec<_>>()
                        .join("."),
                    _ => format!("{}", rewritten),
                };
                let data_type = infer_expr_type(&rewritten, &join_schema);
                exprs.push(rewritten);
                names.push(name);
                types.push(data_type);
            }
            (exprs, names, types)
        };

        Executor::add_pg_get_indexdef_support_to_group_by(
            &rewritten_projection,
            &mut group_by_exprs,
            &mut group_by_names,
            &mut group_by_types,
            &join_schema,
        );

        let (mut agg_exprs, mut agg_names, mut agg_types) =
            Executor::extract_aggregate_info(&rewritten_projection, &join_schema);

        if let Some(having_expr) = &select.having {
            let rewritten_having_for_agg =
                rewrite_for_using_join(having_expr, table_aliases, merge_columns)?;
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
            let having_aggs =
                super::operators::collect_nested_aggregates(&rewritten_having_for_agg);
            for f in having_aggs {
                Executor::add_aggregate_from_function(
                    f,
                    None,
                    &join_schema,
                    &mut agg_exprs,
                    &mut agg_names,
                    &mut agg_types,
                    &mut seen_sigs,
                );
            }
        }

        let group_by_count = group_by_names.len();

        let group_by_exprs_clone = group_by_exprs.clone();
        running_op = Box::new(HashAggregateOperator::new(
            running_op,
            group_by_exprs,
            agg_exprs.clone(),
            group_by_names.clone(),
            group_by_types.clone(),
            agg_names.clone(),
            agg_types.clone(),
        ));

        let rows = execute_operator_tree(
            &mut running_op,
            txn,
            self.store(),
            db_id,
            search_path,
            sequence_values,
        )
        .await?;

        let agg_output_schema = running_op.schema().clone();

        let rows = if let Some(having_expr) = &select.having {
            let rewritten_having =
                rewrite_for_using_join(having_expr, table_aliases, merge_columns)?;
            let mut filtered_rows = Vec::with_capacity(rows.len());
            for row in rows {
                let having_val = eval_having_expr_for_operators(
                    &rewritten_having,
                    &row,
                    &agg_output_schema,
                    &agg_exprs,
                    group_by_count,
                )?;
                let having_val = coerce_text_literal_to_bool(&rewritten_having, having_val)?;
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
            for item in &rewritten_projection {
                let expr = match item {
                    SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                    _ => continue,
                };
                if let Expr::Function(f) = expr {
                    if is_aggregate_func(f) {
                        let sig = super::operators::agg_func_signature(f);
                        let name = get_select_item_name(item);
                        if !map.contains_key(&sig) {
                            map.insert(sig, name);
                        }
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

        for item in &rewritten_projection {
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

        if !query.order_by.is_empty() {
            projected_rows =
                self.apply_order_by_for_aggregate(projected_rows, &query.order_by, &columns);
        }

        let offset = extract_offset(query);
        if offset > 0 {
            projected_rows = projected_rows.into_iter().skip(offset).collect();
        }

        if let Some(limit) = extract_limit(query) {
            projected_rows.truncate(limit);
        }

        Ok(Some(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows: projected_rows,
            timezone: crate::session_context::current_timezone(),
        }))
    }

    async fn execute_join_window_path(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        running_op: BoxedOperator,
        query: &Query,
        resolved_projection: &[SelectItem],
        table_aliases: &[(String, TableSchema)],
        merge_columns: &[UsingMergeColumn],
    ) -> Result<Option<ExecuteResult>> {
        let join_schema = running_op.schema().clone();

        let rewritten_projection: Vec<SelectItem> = resolved_projection
            .iter()
            .map(|item| match item {
                SelectItem::UnnamedExpr(expr) => {
                    let original_name = get_select_item_name(item);
                    let rewritten = rewrite_for_using_join(expr, table_aliases, merge_columns)?;
                    let rewritten_name =
                        get_select_item_name(&SelectItem::UnnamedExpr(rewritten.clone()));
                    if rewritten_name != original_name {
                        Ok(SelectItem::ExprWithAlias {
                            expr: rewritten,
                            alias: Ident::new(original_name),
                        })
                    } else {
                        Ok(SelectItem::UnnamedExpr(rewritten))
                    }
                }
                SelectItem::ExprWithAlias { expr, alias } => Ok(SelectItem::ExprWithAlias {
                    expr: rewrite_for_using_join(expr, table_aliases, merge_columns)?,
                    alias: alias.clone(),
                }),
                other => Ok(other.clone()),
            })
            .collect::<Result<Vec<_>>>()?;

        let mut window_funcs =
            Executor::extract_window_function_exprs(&rewritten_projection, &join_schema);

        for wf in &mut window_funcs {
            wf.partition_by = wf
                .partition_by
                .iter()
                .map(|e| rewrite_for_using_join(e, table_aliases, merge_columns))
                .collect::<Result<Vec<_>>>()?;
            wf.order_by = wf
                .order_by
                .iter()
                .map(|o| {
                    Ok(sqlparser::ast::OrderByExpr {
                        expr: rewrite_for_using_join(&o.expr, table_aliases, merge_columns)?,
                        asc: o.asc,
                        nulls_first: o.nulls_first,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if let Some(ref arg) = wf.arg_expr {
                wf.arg_expr = Some(rewrite_for_using_join(arg, table_aliases, merge_columns)?);
            }
            if let Some(ref offset) = wf.offset_expr {
                wf.offset_expr = Some(rewrite_for_using_join(
                    offset,
                    table_aliases,
                    merge_columns,
                )?);
            }
            if let Some(ref default_val) = wf.default_value_expr {
                wf.default_value_expr = Some(rewrite_for_using_join(
                    default_val,
                    table_aliases,
                    merge_columns,
                )?);
            }
        }

        let window_operator = Box::new(WindowOperator::new(running_op, window_funcs.clone()));

        let mut rewritten_order_by: Vec<sqlparser::ast::OrderByExpr> = Vec::new();
        let mut projection_exprs: Vec<Expr> = Vec::new();
        let mut alias_exprs: HashMap<String, Expr> = HashMap::new();
        for item in &rewritten_projection {
            match item {
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                    for col in &join_schema.columns {
                        projection_exprs.push(Expr::Identifier(Ident::new(col.name.clone())));
                    }
                }
                SelectItem::UnnamedExpr(expr) => {
                    projection_exprs.push(expr.clone());
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    alias_exprs.insert(alias.value.to_lowercase(), expr.clone());
                    projection_exprs.push(expr.clone());
                }
            }
        }

        for o in &query.order_by {
            let expr = if let Expr::Identifier(ident) = &o.expr {
                if let Some(e) = alias_exprs.get(&ident.value.to_lowercase()) {
                    e.clone()
                } else {
                    rewrite_for_using_join(&o.expr, table_aliases, merge_columns)?
                }
            } else if let Expr::Value(sqlparser::ast::Value::Number(n, _)) = &o.expr {
                if let Ok(pos) = n.parse::<usize>() {
                    if pos == 0 || pos > projection_exprs.len() {
                        return Err(anyhow!("ORDER BY position {} is not in select list", pos));
                    }
                    projection_exprs[pos - 1].clone()
                } else {
                    rewrite_for_using_join(&o.expr, table_aliases, merge_columns)?
                }
            } else {
                rewrite_for_using_join(&o.expr, table_aliases, merge_columns)?
            };
            rewritten_order_by.push(sqlparser::ast::OrderByExpr {
                expr,
                asc: o.asc,
                nulls_first: o.nulls_first,
            });
        }

        let mut operator: BoxedOperator = if !rewritten_order_by.is_empty() {
            let sort_op = Box::new(SortOperator::new(window_operator, rewritten_order_by));
            let limit = extract_limit(query);
            let offset = extract_offset(query);
            if limit.is_some() || offset > 0 {
                Box::new(LimitOperator::new(sort_op, limit, offset))
            } else {
                sort_op
            }
        } else {
            let limit = extract_limit(query);
            let offset = extract_offset(query);
            if limit.is_some() || offset > 0 {
                Box::new(LimitOperator::new(window_operator, limit, offset))
            } else {
                window_operator
            }
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

        let (columns, column_types, projected_rows) = Executor::project_window_results(
            &rewritten_projection,
            &join_schema,
            &window_funcs,
            raw_rows,
        )?;

        Ok(Some(ExecuteResult::Select {
            columns,
            column_types: Some(column_types),
            rows: projected_rows,
            timezone: crate::session_context::current_timezone(),
        }))
    }
}

#[cfg(test)]
mod join_locking_clause_tests {
    use super::*;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn parse_query(sql: &str) -> Query {
        let dialect = PostgreSqlDialect {};
        let mut statements = Parser::parse_sql(&dialect, sql).expect("parse SQL");
        assert_eq!(statements.len(), 1);
        match statements.remove(0) {
            sqlparser::ast::Statement::Query(query) => *query,
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn rejects_for_update_on_join_queries() {
        let query =
            parse_query("SELECT a.* FROM a JOIN b ON b.a_id = a.id WHERE a.id = 1 FOR UPDATE");
        let select = match &*query.body {
            SetExpr::Select(select) => select,
            other => panic!("expected Select, got {other:?}"),
        };
        let has_joins = !select.from[0].joins.is_empty() || select.from.len() > 1;
        assert!(has_joins);

        let err = ensure_no_locking_clauses_for_join(&query).unwrap_err();
        assert_eq!(err.to_string(), JOIN_LOCKING_CLAUSE_UNSUPPORTED);
    }

    #[test]
    fn allows_join_queries_without_locking_clauses() {
        let query = parse_query("SELECT a.* FROM a JOIN b ON b.a_id = a.id WHERE a.id = 1");
        let select = match &*query.body {
            SetExpr::Select(select) => select,
            other => panic!("expected Select, got {other:?}"),
        };
        let has_joins = !select.from[0].joins.is_empty() || select.from.len() > 1;
        assert!(has_joins);

        ensure_no_locking_clauses_for_join(&query).expect("no lock clauses");
    }
}

fn extract_grouping_sets(exprs: &[Expr]) -> Option<Vec<Vec<Expr>>> {
    for expr in exprs {
        match expr {
            Expr::GroupingSets(sets) => {
                return Some(sets.iter().map(|s| s.clone()).collect());
            }
            Expr::Rollup(cols) => {
                let mut sets = Vec::new();
                for i in 0..=cols.len() {
                    let mut subset = Vec::new();
                    for group in cols.iter().take(cols.len() - i) {
                        subset.extend(group.iter().cloned());
                    }
                    sets.push(subset);
                }
                return Some(sets);
            }
            Expr::Cube(cols) => {
                let n = cols.len();
                let mut sets = Vec::new();
                for mask in 0..(1usize << n) {
                    let mut subset = Vec::new();
                    for (i, group) in cols.iter().enumerate() {
                        if (mask & (1usize << i)) != 0 {
                            subset.extend(group.iter().cloned());
                        }
                    }
                    sets.push(subset);
                }
                return Some(sets);
            }
            _ => {}
        }
    }
    None
}

fn expr_matches(pattern: &Expr, target: &Expr) -> bool {
    match (pattern, target) {
        (Expr::Identifier(a), Expr::Identifier(b)) => a.value.eq_ignore_ascii_case(&b.value),
        (Expr::CompoundIdentifier(a), Expr::CompoundIdentifier(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|(x, y)| x.value.eq_ignore_ascii_case(&y.value))
        }
        (Expr::Identifier(a), Expr::CompoundIdentifier(b)) => b
            .last()
            .map(|i| i.value.eq_ignore_ascii_case(&a.value))
            .unwrap_or(false),
        (Expr::CompoundIdentifier(a), Expr::Identifier(b)) => a
            .last()
            .map(|i| i.value.eq_ignore_ascii_case(&b.value))
            .unwrap_or(false),
        _ => format!("{}", pattern) == format!("{}", target),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn make_schema(col_names: &[&str]) -> TableSchema {
        TableSchema {
            name: "t".to_string(),
            table_id: 0,
            columns: col_names
                .iter()
                .map(|name| crate::types::ColumnDef {
                    name: (*name).to_string(),
                    data_type: DataType::Int32,
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

    fn ident(name: &str) -> Expr {
        Expr::Identifier(sqlparser::ast::Ident::new(name))
    }

    fn parse_query(sql: &str) -> Box<Query> {
        let dialect = PostgreSqlDialect {};
        let mut statements = Parser::parse_sql(&dialect, sql).unwrap();
        match statements.remove(0) {
            sqlparser::ast::Statement::Query(q) => q,
            other => panic!("expected query, got {other:?}"),
        }
    }

    #[test]
    fn resolves_order_by_positional_for_select_projection() {
        let query = parse_query("SELECT a, b FROM t ORDER BY 2");
        let SetExpr::Select(select) = &*query.body else {
            panic!("Expected SELECT");
        };

        let schema = make_schema(&["a", "b"]);
        let resolved =
            resolve_order_by_exprs_for_non_agg(&query.order_by, &select.projection, &schema)
                .unwrap();

        assert_eq!(resolved.len(), 1);
        assert!(
            matches!(&resolved[0], Expr::Identifier(id) if id.value == "b"),
            "unexpected resolved expr: {:?}",
            resolved[0]
        );
    }

    #[test]
    fn resolves_order_by_positional_for_select_wildcard() {
        let query = parse_query("SELECT * FROM t ORDER BY 2");
        let SetExpr::Select(select) = &*query.body else {
            panic!("Expected SELECT");
        };

        let schema = make_schema(&["a", "b", "c"]);
        let resolved =
            resolve_order_by_exprs_for_non_agg(&query.order_by, &select.projection, &schema)
                .unwrap();

        assert_eq!(resolved.len(), 1);
        assert!(
            matches!(&resolved[0], Expr::Identifier(id) if id.value == "b"),
            "unexpected resolved expr: {:?}",
            resolved[0]
        );
    }

    #[test]
    fn rejects_out_of_range_order_by_position() {
        let query = parse_query("SELECT a FROM t ORDER BY 2");
        let SetExpr::Select(select) = &*query.body else {
            panic!("Expected SELECT");
        };

        let schema = make_schema(&["a"]);
        let err = resolve_order_by_exprs_for_non_agg(&query.order_by, &select.projection, &schema)
            .unwrap_err();
        assert!(err.to_string().contains("ORDER BY position 2"));
    }

    #[test]
    fn cube_treats_grouped_items_as_units() {
        let a = ident("a");
        let b = ident("b");
        let c = ident("c");
        let cube = Expr::Cube(vec![vec![a.clone(), b.clone()], vec![c.clone()]]);

        let sets = extract_grouping_sets(&[cube]).unwrap();
        assert_eq!(sets.len(), 4);

        for set in &sets {
            let has_a = set
                .iter()
                .any(|e| matches!(e, Expr::Identifier(id) if id.value == "a"));
            let has_b = set
                .iter()
                .any(|e| matches!(e, Expr::Identifier(id) if id.value == "b"));
            assert_eq!(has_a, has_b, "unexpected set: {:?}", set);
        }

        assert!(sets.iter().any(|s| s.is_empty()));
        assert!(sets
            .iter()
            .any(|s| s.len() == 1 && matches!(&s[0], Expr::Identifier(id) if id.value == "c")));
        assert!(sets.iter().any(|s| s.len() == 2));
        assert!(sets.iter().any(|s| s.len() == 3));
    }

    #[test]
    fn rollup_treats_grouped_items_as_units() {
        let a = ident("a");
        let b = ident("b");
        let c = ident("c");
        let rollup = Expr::Rollup(vec![vec![a.clone(), b.clone()], vec![c.clone()]]);

        let sets = extract_grouping_sets(&[rollup]).unwrap();
        assert_eq!(sets.len(), 3);

        for set in &sets {
            let has_a = set
                .iter()
                .any(|e| matches!(e, Expr::Identifier(id) if id.value == "a"));
            let has_b = set
                .iter()
                .any(|e| matches!(e, Expr::Identifier(id) if id.value == "b"));
            assert_eq!(has_a, has_b, "unexpected set: {:?}", set);
        }

        assert!(sets.iter().any(|s| s.is_empty()));
        assert!(sets.iter().any(|s| s.len() == 2));
        assert!(sets.iter().any(|s| s.len() == 3));
    }

    #[test]
    fn generate_series_pushdown_is_disabled_with_select_list_srf() {
        let query = parse_query("SELECT unnest(ARRAY[1,2]) FROM generate_series(1, 10) LIMIT 1");
        let SetExpr::Select(select) = &*query.body else {
            panic!("expected SELECT");
        };

        let plan = plan_generate_series_offset_limit_pushdown(query.as_ref(), select.as_ref());
        assert_eq!(
            plan,
            GenerateSeriesOffsetLimitPushdownPlan {
                offset: 0,
                limit: None,
                clear_query_offset_limit_fetch: false,
            }
        );
    }

    #[test]
    fn generate_series_offset_is_not_pushed_down_with_volatile_projection() {
        let query =
            parse_query("SELECT nextval('s') FROM generate_series(1, 100) OFFSET 10 LIMIT 5");
        let SetExpr::Select(select) = &*query.body else {
            panic!("expected SELECT");
        };

        let plan = plan_generate_series_offset_limit_pushdown(query.as_ref(), select.as_ref());
        assert_eq!(
            plan,
            GenerateSeriesOffsetLimitPushdownPlan {
                offset: 0,
                limit: Some(15),
                clear_query_offset_limit_fetch: false,
            }
        );
    }

    #[test]
    fn generate_series_offset_is_not_pushed_down_with_nontrivial_projection() {
        let query =
            parse_query("SELECT 1/(n-1) FROM generate_series(1, 100) AS g(n) OFFSET 10 LIMIT 5");
        let SetExpr::Select(select) = &*query.body else {
            panic!("expected SELECT");
        };

        let plan = plan_generate_series_offset_limit_pushdown(query.as_ref(), select.as_ref());
        assert_eq!(
            plan,
            GenerateSeriesOffsetLimitPushdownPlan {
                offset: 0,
                limit: Some(15),
                clear_query_offset_limit_fetch: false,
            }
        );
    }

    #[test]
    fn generate_series_offset_limit_are_pushed_down_for_simple_projection() {
        let query = parse_query("SELECT * FROM generate_series(1, 100) OFFSET 10 LIMIT 5");
        let SetExpr::Select(select) = &*query.body else {
            panic!("expected SELECT");
        };

        let plan = plan_generate_series_offset_limit_pushdown(query.as_ref(), select.as_ref());
        assert_eq!(
            plan,
            GenerateSeriesOffsetLimitPushdownPlan {
                offset: 10,
                limit: Some(5),
                clear_query_offset_limit_fetch: true,
            }
        );
    }

    #[test]
    fn generate_series_limit_is_pushed_down_without_offset() {
        let query = parse_query("SELECT random() FROM generate_series(1, 100) LIMIT 5");
        let SetExpr::Select(select) = &*query.body else {
            panic!("expected SELECT");
        };

        let plan = plan_generate_series_offset_limit_pushdown(query.as_ref(), select.as_ref());
        assert_eq!(
            plan,
            GenerateSeriesOffsetLimitPushdownPlan {
                offset: 0,
                limit: Some(5),
                clear_query_offset_limit_fetch: true,
            }
        );
    }

    #[test]
    fn generate_series_pushdown_normalizes_offset_limit_expressions() {
        let query = parse_query(
            "SELECT nextval('s') FROM generate_series(1, 100) OFFSET (txid_current() % 5 + 1) LIMIT 1",
        );
        let SetExpr::Select(select) = &*query.body else {
            panic!("expected SELECT");
        };

        assert!(generate_series_offset_limit_pushdown_eligible(
            query.as_ref(),
            select.as_ref()
        ));

        let normalized = normalize_query_offset_limit_fetch_expressions(query.as_ref());
        let offset_expr = &normalized.offset.as_ref().unwrap().value;
        let Expr::Value(SqlValue::Number(n_str, _)) = offset_expr else {
            panic!("expected numeric OFFSET expr, got {offset_expr:?}");
        };
        let offset_n: usize = n_str.parse().unwrap();

        let plan = plan_generate_series_offset_limit_pushdown(&normalized, select.as_ref());
        assert_eq!(
            plan,
            GenerateSeriesOffsetLimitPushdownPlan {
                offset: 0,
                limit: Some(offset_n + 1),
                clear_query_offset_limit_fetch: false,
            }
        );

        let rows: Vec<Row> = (0..10).map(|i| Row::new(vec![Value::Int32(i)])).collect();
        let result = apply_offset_limit_fetch(rows, &normalized);
        assert_eq!(result.len(), 1);
    }
}

#[cfg(test)]
mod using_merge_tests {
    use super::*;

    #[test]
    fn build_coalesce_two_sources() {
        let mc = UsingMergeColumn {
            col_name: "id".to_string(),
            source_aliases: vec!["a".to_string(), "b".to_string()],
        };
        let expr = build_coalesce_for_merge(&mc);
        let s = format!("{}", expr);
        assert!(s.contains("COALESCE"), "expected COALESCE, got: {}", s);
        assert!(s.contains("a.id"), "expected a.id, got: {}", s);
        assert!(s.contains("b.id"), "expected b.id, got: {}", s);
    }

    #[test]
    fn build_coalesce_three_sources_chained() {
        let mc = UsingMergeColumn {
            col_name: "id".to_string(),
            source_aliases: vec!["a".to_string(), "b".to_string(), "c".to_string()],
        };
        let expr = build_coalesce_for_merge(&mc);
        let s = format!("{}", expr);
        assert!(
            s.contains("c.id"),
            "expected c.id in chained merge, got: {}",
            s
        );
    }

    #[test]
    fn replace_merge_refs_bare_identifier() {
        let mc = vec![UsingMergeColumn {
            col_name: "a".to_string(),
            source_aliases: vec!["t1".to_string(), "t2".to_string()],
        }];
        let expr = Expr::Identifier(Ident::new("a"));
        let result = replace_using_merge_refs(&expr, &mc);
        let s = format!("{}", result);
        assert!(
            s.contains("COALESCE"),
            "bare 'a' should become COALESCE: {}",
            s
        );
    }

    #[test]
    fn replace_merge_refs_qualified_untouched() {
        let mc = vec![UsingMergeColumn {
            col_name: "a".to_string(),
            source_aliases: vec!["t1".to_string(), "t2".to_string()],
        }];
        let expr = Expr::CompoundIdentifier(vec![Ident::new("t1"), Ident::new("a")]);
        let result = replace_using_merge_refs(&expr, &mc);
        assert_eq!(
            format!("{}", result),
            format!("{}", expr),
            "qualified ref should be untouched"
        );
    }

    #[test]
    fn replace_merge_refs_non_merge_column_untouched() {
        let mc = vec![UsingMergeColumn {
            col_name: "a".to_string(),
            source_aliases: vec!["t1".to_string(), "t2".to_string()],
        }];
        let expr = Expr::Identifier(Ident::new("b"));
        let result = replace_using_merge_refs(&expr, &mc);
        assert_eq!(
            format!("{}", result),
            "b",
            "non-merge column should stay as-is"
        );
    }

    #[test]
    fn replace_merge_refs_in_binary_op() {
        let mc = vec![UsingMergeColumn {
            col_name: "x".to_string(),
            source_aliases: vec!["l".to_string(), "r".to_string()],
        }];
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("x"))),
            op: BinaryOperator::Gt,
            right: Box::new(Expr::Value(SqlValue::Number("5".to_string(), false))),
        };
        let result = replace_using_merge_refs(&expr, &mc);
        let s = format!("{}", result);
        assert!(
            s.contains("COALESCE"),
            "x in binary op should become COALESCE: {}",
            s
        );
        assert!(s.contains("> 5"), "comparison should be preserved: {}", s);
    }

    #[test]
    fn replace_merge_refs_empty_merge_columns_is_noop() {
        let expr = Expr::Identifier(Ident::new("a"));
        let result = replace_using_merge_refs(&expr, &[]);
        assert_eq!(format!("{}", result), "a");
    }
}

#[cfg(test)]
mod window_routing_tests {
    use super::*;
    use sqlparser::ast::{
        Function, FunctionArg, FunctionArgExpr, Ident, ObjectName, WindowSpec, WindowType,
    };

    #[test]
    fn window_function_not_detected_as_bare_aggregate() {
        let window_func = Function {
            name: ObjectName(vec![Ident::new("sum")]),
            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(
                Expr::Identifier(Ident::new("salary")),
            ))],
            over: Some(WindowType::WindowSpec(WindowSpec {
                partition_by: vec![],
                order_by: vec![],
                window_frame: None,
            })),
            filter: None,
            null_treatment: None,
            distinct: false,
            special: false,
            order_by: vec![],
        };

        let projection = vec![
            SelectItem::UnnamedExpr(Expr::Identifier(Ident::new("id"))),
            SelectItem::ExprWithAlias {
                expr: Expr::Function(window_func),
                alias: Ident::new("running_total"),
            },
        ];

        let has_bare_agg = projection_has_non_window_aggregate(&projection);

        assert!(
            !has_bare_agg,
            "SUM(salary) OVER (...) should NOT be detected as bare aggregate"
        );
    }

    #[test]
    fn bare_aggregate_still_detected() {
        let bare_agg = Function {
            name: ObjectName(vec![Ident::new("count")]),
            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Wildcard)],
            over: None,
            filter: None,
            null_treatment: None,
            distinct: false,
            special: false,
            order_by: vec![],
        };

        let projection = vec![SelectItem::UnnamedExpr(Expr::Function(bare_agg))];

        let has_bare_agg = projection_has_non_window_aggregate(&projection);

        assert!(
            has_bare_agg,
            "COUNT(*) without OVER should be detected as bare aggregate"
        );
    }

    #[test]
    fn nested_aggregate_detected_in_projection() {
        let count_star = Function {
            name: ObjectName(vec![Ident::new("count")]),
            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Wildcard)],
            over: None,
            filter: None,
            null_treatment: None,
            distinct: false,
            special: false,
            order_by: vec![],
        };

        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Value(SqlValue::SingleQuotedString(
                "TYPE_LEFT=".to_string(),
            ))),
            op: BinaryOperator::StringConcat,
            right: Box::new(Expr::Function(count_star)),
        };

        let projection = vec![SelectItem::UnnamedExpr(expr)];

        assert!(
            projection_has_non_window_aggregate(&projection),
            "Nested COUNT(*) in expression should force aggregation routing"
        );
    }
}
