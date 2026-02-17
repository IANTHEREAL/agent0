//! EXPLAIN statement - PostgreSQL-compatible query plan output

use std::fmt::Write;

use sqlparser::ast::{Expr, Query, Select, SetExpr, Statement, TableFactor, TableWithJoins};

use super::analyzer::types::{
    AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef, AnalyzedTableRefKind, BinaryOp,
    JoinCondition, JoinType as AnalyzedJoinType, TypedExpr, TypedExprKind,
};
use super::analyzer::AnalyzedQuery;
use super::operators::HashJoinConfig;
use super::optimizer::logical_planner::expr_has_aggregate;
use super::planner::{
    analyze_predicates, analyze_typed_predicates, choose_best_access_path_for_filter,
    choose_best_access_path_for_typed_filter, choose_join_algorithm, JoinAlgorithmChoice,
    PredicateInfo, ScanType,
};
use crate::types::TableSchema;

const DEFAULT_ROW_WIDTH: usize = 40;

#[derive(Debug, Clone)]
pub enum PlanNode {
    SeqScan {
        table_name: String,
        alias: Option<String>,
        filter: Option<String>,
        cost: PlanCost,
    },
    IndexScan {
        table_name: String,
        alias: Option<String>,
        index_name: String,
        index_cond: Option<String>,
        filter: Option<String>,
        cost: PlanCost,
    },
    NestedLoop {
        join_type: String,
        cost: PlanCost,
        children: Vec<PlanNode>,
    },
    HashJoin {
        join_type: String,
        hash_cond: Option<String>,
        cost: PlanCost,
        children: Vec<PlanNode>,
    },
    Sort {
        sort_key: Vec<String>,
        cost: PlanCost,
        child: Box<PlanNode>,
    },
    Limit {
        #[allow(dead_code)] // EXPLAIN plan representation
        count: usize,
        cost: PlanCost,
        child: Box<PlanNode>,
    },
    Aggregate {
        strategy: String,
        keys: Vec<String>,
        cost: PlanCost,
        child: Box<PlanNode>,
    },
    TableFunctionScan {
        function_name: String,
        alias: Option<String>,
        cost: PlanCost,
    },
    Filter {
        condition: String,
        cost: PlanCost,
        child: Box<PlanNode>,
    },
    Result {
        cost: PlanCost,
    },
}

#[derive(Debug, Clone)]
pub struct PlanCost {
    pub startup: f64,
    pub total: f64,
    pub rows: usize,
    pub width: usize,
}

impl Default for PlanCost {
    fn default() -> Self {
        Self {
            startup: 0.0,
            total: 0.0,
            rows: 1,
            width: DEFAULT_ROW_WIDTH,
        }
    }
}

pub fn generate_plan(
    stmt: &Statement,
    schema_lookup: impl Fn(&str) -> Option<TableSchema>,
    row_count_lookup: impl Fn(&str) -> usize,
) -> PlanNode {
    match stmt {
        Statement::Query(query) => generate_query_plan(query, &schema_lookup, &row_count_lookup),
        _ => PlanNode::Result {
            cost: PlanCost::default(),
        },
    }
}

// ── Analyzed (TypedExpr) plan generation ──────────────────────────────

/// Generate a plan from an [`AnalyzedQuery`], using the typed planner path.
///
/// This ensures EXPLAIN sees the same query tree as execution (views expanded,
/// types resolved, etc.).  Uses `choose_best_access_path_for_typed_filter`
/// for index selection — full parity with the execution path.
pub fn generate_plan_from_analyzed(
    query: &AnalyzedQuery,
    schema_lookup: &impl Fn(&str) -> Option<TableSchema>,
    row_count_lookup: &impl Fn(&str) -> usize,
) -> PlanNode {
    let mut plan = generate_analyzed_body(&query.body, schema_lookup, row_count_lookup);

    // ORDER BY
    if !query.order_by.is_empty() {
        let sort_keys: Vec<String> = query
            .order_by
            .iter()
            .map(|o| format_typed_expr(&o.expr))
            .collect();
        let child_cost = get_plan_cost(&plan);
        let sort_cost =
            child_cost.total + (child_cost.rows as f64 * (child_cost.rows as f64).log2().max(1.0));
        plan = PlanNode::Sort {
            sort_key: sort_keys,
            cost: PlanCost {
                startup: sort_cost,
                total: sort_cost,
                rows: child_cost.rows,
                width: child_cost.width,
            },
            child: Box::new(plan),
        };
    }

    // LIMIT
    if let Some(limit_expr) = &query.limit {
        if let Some(limit_val) = extract_typed_limit_value(limit_expr) {
            let child_cost = get_plan_cost(&plan);
            let limited_rows = limit_val.min(child_cost.rows);
            plan = PlanNode::Limit {
                count: limit_val,
                cost: PlanCost {
                    startup: child_cost.startup,
                    total: child_cost.startup + (limited_rows as f64 * 0.01),
                    rows: limited_rows,
                    width: child_cost.width,
                },
                child: Box::new(plan),
            };
        }
    }

    plan
}

fn generate_analyzed_body(
    body: &AnalyzedQueryBody,
    schema_lookup: &impl Fn(&str) -> Option<TableSchema>,
    row_count_lookup: &impl Fn(&str) -> usize,
) -> PlanNode {
    match body {
        AnalyzedQueryBody::Select(select) => {
            generate_analyzed_select_plan(select, schema_lookup, row_count_lookup)
        }
        AnalyzedQueryBody::Values(_) | AnalyzedQueryBody::SetOperation { .. } => PlanNode::Result {
            cost: PlanCost::default(),
        },
    }
}

fn generate_analyzed_select_plan(
    select: &AnalyzedSelect,
    schema_lookup: &impl Fn(&str) -> Option<TableSchema>,
    row_count_lookup: &impl Fn(&str) -> usize,
) -> PlanNode {
    if select.from.is_empty() {
        return PlanNode::Result {
            cost: PlanCost {
                startup: 0.0,
                total: 0.01,
                rows: 1,
                width: 4,
            },
        };
    }

    let predicates = select
        .where_clause
        .as_ref()
        .map(|expr| analyze_typed_predicates(expr))
        .unwrap_or_default();

    // Build plan from the first FROM item
    let mut plan = generate_analyzed_table_ref_plan(
        &select.from[0],
        &predicates,
        select.where_clause.as_ref(),
        schema_lookup,
        row_count_lookup,
    );

    // Additional FROM items (implicit cross joins)
    for table_ref in select.from.iter().skip(1) {
        let right_plan =
            generate_analyzed_table_ref_plan(table_ref, &[], None, schema_lookup, row_count_lookup);
        let left_cost = get_plan_cost(&plan);
        let right_cost = get_plan_cost(&right_plan);
        let total_rows = left_cost.rows.saturating_mul(right_cost.rows);
        let total_cost = left_cost.total + right_cost.total + (total_rows as f64 * 0.01);
        plan = PlanNode::NestedLoop {
            join_type: "Inner".to_string(),
            cost: PlanCost {
                startup: 0.0,
                total: total_cost,
                rows: total_rows.max(1),
                width: DEFAULT_ROW_WIDTH,
            },
            children: vec![plan, right_plan],
        };
    }

    // GROUP BY / aggregate-only queries
    let has_aggregates = select
        .projection
        .iter()
        .any(|p| expr_has_aggregate(&p.expr));
    if !select.group_by.is_empty() || has_aggregates {
        let keys: Vec<String> = select
            .group_by
            .iter()
            .map(|e| format_typed_expr(e))
            .collect();
        let strategy = if select.group_by.is_empty() {
            "Aggregate"
        } else {
            "HashAggregate"
        };
        let child_cost = get_plan_cost(&plan);
        let agg_rows = if select.group_by.is_empty() {
            1
        } else {
            (child_cost.rows / 10).max(1)
        };
        plan = PlanNode::Aggregate {
            strategy: strategy.to_string(),
            keys,
            cost: PlanCost {
                startup: child_cost.total,
                total: child_cost.total + (agg_rows as f64 * 0.1),
                rows: agg_rows,
                width: child_cost.width,
            },
            child: Box::new(plan),
        };

        // HAVING → Filter after aggregate
        if let Some(having) = &select.having {
            let filter_str = format_typed_expr(having);
            let agg_cost = get_plan_cost(&plan);
            let filtered_rows = (agg_cost.rows / 3).max(1);
            plan = PlanNode::Filter {
                condition: filter_str,
                cost: PlanCost {
                    startup: agg_cost.startup,
                    total: agg_cost.total + (filtered_rows as f64 * 0.01),
                    rows: filtered_rows,
                    width: agg_cost.width,
                },
                child: Box::new(plan),
            };
        }
    }

    plan
}

fn generate_analyzed_table_ref_plan(
    table_ref: &AnalyzedTableRef,
    predicates: &[PredicateInfo],
    filter_expr: Option<&TypedExpr>,
    schema_lookup: &impl Fn(&str) -> Option<TableSchema>,
    row_count_lookup: &impl Fn(&str) -> usize,
) -> PlanNode {
    let alias_name = table_ref.alias.clone();

    match &table_ref.kind {
        AnalyzedTableRefKind::Table { name, .. } => {
            let table_name = name.rsplit('.').next().unwrap_or(name.as_str());
            let estimated_rows = row_count_lookup(table_name);

            if let Some(schema) = schema_lookup(table_name).or_else(|| schema_lookup(name)) {
                let access_path = choose_best_access_path_for_typed_filter(
                    0,
                    &schema,
                    filter_expr,
                    estimated_rows,
                );

                generate_scan_node(
                    table_name,
                    alias_name,
                    &access_path.scan_type,
                    &predicates,
                    filter_expr.map(|e| format_typed_expr(e)),
                    estimated_rows,
                    &schema,
                )
            } else {
                let filter = filter_expr.map(|e| format_typed_expr(e));
                PlanNode::SeqScan {
                    table_name: table_name.to_string(),
                    alias: alias_name,
                    filter,
                    cost: PlanCost {
                        startup: 0.0,
                        total: estimated_rows as f64 * 0.01 + 1.0,
                        rows: estimated_rows.max(1),
                        width: DEFAULT_ROW_WIDTH,
                    },
                }
            }
        }
        AnalyzedTableRefKind::Subquery(subquery) => {
            generate_plan_from_analyzed(subquery, schema_lookup, row_count_lookup)
        }
        AnalyzedTableRefKind::Join {
            left,
            right,
            join_type,
            condition,
            ..
        } => {
            let left_plan =
                generate_analyzed_table_ref_plan(left, &[], None, schema_lookup, row_count_lookup);
            let right_plan =
                generate_analyzed_table_ref_plan(right, &[], None, schema_lookup, row_count_lookup);
            let left_cost = get_plan_cost(&left_plan);
            let right_cost = get_plan_cost(&right_plan);
            let total_rows = left_cost.rows.saturating_mul(right_cost.rows);
            let total_cost = left_cost.total + right_cost.total + (total_rows as f64 * 0.01);

            let jt = match join_type {
                AnalyzedJoinType::Inner => "Inner",
                AnalyzedJoinType::Left => "Left",
                AnalyzedJoinType::Right => "Right",
                AnalyzedJoinType::Full => "Full",
                AnalyzedJoinType::Cross => "Cross",
            }
            .to_string();

            let cost = PlanCost {
                startup: 0.0,
                total: total_cost,
                rows: total_rows.max(1),
                width: DEFAULT_ROW_WIDTH,
            };

            // Detect equi-join to show HashJoin vs NestedLoop (matching executor)
            let left_col_count = count_table_ref_columns(left);
            let is_hash_eligible = has_typed_equi_join_keys(condition, left_col_count);

            if is_hash_eligible && !matches!(join_type, AnalyzedJoinType::Cross) {
                let hash_cond = match condition {
                    JoinCondition::On(expr) => Some(format_typed_expr(expr)),
                    JoinCondition::Using(cols) => Some(format!(
                        "USING ({})",
                        cols.iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                    JoinCondition::None => None,
                };
                PlanNode::HashJoin {
                    join_type: jt,
                    hash_cond,
                    cost,
                    children: vec![left_plan, right_plan],
                }
            } else {
                PlanNode::NestedLoop {
                    join_type: jt,
                    cost,
                    children: vec![left_plan, right_plan],
                }
            }
        }
        AnalyzedTableRefKind::Function { func, .. } => PlanNode::TableFunctionScan {
            function_name: func.name.clone(),
            alias: alias_name,
            cost: PlanCost {
                startup: 0.0,
                total: 11.0,
                rows: 1000,
                width: DEFAULT_ROW_WIDTH,
            },
        },
    }
}

/// Count the number of output columns from an analyzed table reference.
fn count_table_ref_columns(table_ref: &AnalyzedTableRef) -> usize {
    match &table_ref.kind {
        AnalyzedTableRefKind::Table { schema, .. } => schema.columns.len(),
        AnalyzedTableRefKind::Join { left, right, .. } => {
            count_table_ref_columns(left) + count_table_ref_columns(right)
        }
        AnalyzedTableRefKind::Subquery(q) => q.output_schema.len(),
        AnalyzedTableRefKind::Function { output_columns, .. } => output_columns.len(),
    }
}

/// Check if a typed join condition contains equi-join keys (for EXPLAIN display).
fn has_typed_equi_join_keys(condition: &JoinCondition, left_col_count: usize) -> bool {
    match condition {
        JoinCondition::Using(cols) => cols.iter().all(|c| c.left_type == c.right_type),
        JoinCondition::On(expr) => check_equi_join_expr(expr, left_col_count),
        JoinCondition::None => false,
    }
}

/// Check if an expression tree contains at least one `col_left = col_right` equi-join key.
fn check_equi_join_expr(expr: &TypedExpr, left_col_count: usize) -> bool {
    match &expr.kind {
        TypedExprKind::BinaryOp { op, left, right } => {
            if matches!(op, BinaryOp::Eq) {
                if let (
                    TypedExprKind::ColumnRef {
                        column_index: li, ..
                    },
                    TypedExprKind::ColumnRef {
                        column_index: ri, ..
                    },
                ) = (&left.kind, &right.kind)
                {
                    return (*li < left_col_count && *ri >= left_col_count)
                        || (*ri < left_col_count && *li >= left_col_count);
                }
            }
            if matches!(op, BinaryOp::And) {
                return check_equi_join_expr(left, left_col_count)
                    || check_equi_join_expr(right, left_col_count);
            }
            false
        }
        _ => false,
    }
}

/// Generate a PlanNode from a resolved ScanType.
///
/// Shared between AST and analyzed paths to avoid duplicating the match arms.
fn generate_scan_node(
    table_name: &str,
    alias: Option<String>,
    scan_type: &ScanType,
    predicates: &[PredicateInfo],
    filter_str: Option<String>,
    estimated_rows: usize,
    schema: &TableSchema,
) -> PlanNode {
    match scan_type {
        ScanType::IndexScan {
            index_name,
            estimated_rows: est_rows,
            ..
        }
        | ScanType::IndexRangeScan {
            index_name,
            estimated_rows: est_rows,
            ..
        }
        | ScanType::IndexBoundedRangeScan {
            index_name,
            estimated_rows: est_rows,
            ..
        }
        | ScanType::InListScan {
            index_name,
            estimated_rows: est_rows,
            ..
        } => {
            let index_cond = predicates
                .iter()
                .map(|p| format_predicate(p))
                .collect::<Vec<_>>()
                .join(" AND ");
            PlanNode::IndexScan {
                table_name: table_name.to_string(),
                alias,
                index_name: index_name.clone(),
                index_cond: if index_cond.is_empty() {
                    None
                } else {
                    Some(index_cond)
                },
                filter: None,
                cost: PlanCost {
                    startup: 0.15,
                    total: 0.15 + (*est_rows as f64 * 0.01),
                    rows: (*est_rows).max(1),
                    width: estimate_row_width(schema),
                },
            }
        }
        ScanType::GinIndexScan {
            index_name,
            estimated_rows: est_rows,
            ..
        } => PlanNode::IndexScan {
            table_name: table_name.to_string(),
            alias,
            index_name: index_name.clone(),
            index_cond: filter_str.clone(),
            filter: None,
            cost: PlanCost {
                startup: 0.15,
                total: 0.15 + (*est_rows as f64 * 0.01),
                rows: (*est_rows).max(1),
                width: estimate_row_width(schema),
            },
        },
        ScanType::FullTableScan => PlanNode::SeqScan {
            table_name: table_name.to_string(),
            alias,
            filter: filter_str,
            cost: PlanCost {
                startup: 0.0,
                total: estimated_rows as f64 * 0.01 + 1.0,
                rows: estimated_rows.max(1),
                width: estimate_row_width(schema),
            },
        },
    }
}

/// Format a typed expression for EXPLAIN output.
fn format_typed_expr(expr: &TypedExpr) -> String {
    format!("{}", expr)
}

/// Extract a limit value from a typed constant expression.
fn extract_typed_limit_value(expr: &TypedExpr) -> Option<usize> {
    match &expr.kind {
        TypedExprKind::Constant(crate::types::Value::Int32(v)) => Some(*v as usize),
        TypedExprKind::Constant(crate::types::Value::Int64(v)) => Some(*v as usize),
        _ => None,
    }
}

// ── AST-based plan generation (legacy, kept for non-SELECT statements) ──

fn generate_query_plan(
    query: &Query,
    schema_lookup: &impl Fn(&str) -> Option<TableSchema>,
    row_count_lookup: &impl Fn(&str) -> usize,
) -> PlanNode {
    let mut plan = match &*query.body {
        SetExpr::Select(select) => generate_select_plan(select, schema_lookup, row_count_lookup),
        _ => PlanNode::Result {
            cost: PlanCost::default(),
        },
    };

    if !query.order_by.is_empty() {
        let sort_keys: Vec<String> = query
            .order_by
            .iter()
            .map(|o| format_expr(&o.expr))
            .collect();
        let child_cost = get_plan_cost(&plan);
        let sort_cost =
            child_cost.total + (child_cost.rows as f64 * (child_cost.rows as f64).log2().max(1.0));
        plan = PlanNode::Sort {
            sort_key: sort_keys,
            cost: PlanCost {
                startup: sort_cost,
                total: sort_cost,
                rows: child_cost.rows,
                width: child_cost.width,
            },
            child: Box::new(plan),
        };
    }

    if let Some(limit_expr) = &query.limit {
        if let Some(limit_val) = extract_limit_value(limit_expr) {
            let child_cost = get_plan_cost(&plan);
            let limited_rows = limit_val.min(child_cost.rows);
            plan = PlanNode::Limit {
                count: limit_val,
                cost: PlanCost {
                    startup: child_cost.startup,
                    total: child_cost.startup + (limited_rows as f64 * 0.01),
                    rows: limited_rows,
                    width: child_cost.width,
                },
                child: Box::new(plan),
            };
        }
    }

    plan
}

fn generate_select_plan(
    select: &Select,
    schema_lookup: &impl Fn(&str) -> Option<TableSchema>,
    row_count_lookup: &impl Fn(&str) -> usize,
) -> PlanNode {
    if select.from.is_empty() {
        return PlanNode::Result {
            cost: PlanCost {
                startup: 0.0,
                total: 0.01,
                rows: 1,
                width: 4,
            },
        };
    }

    let predicates = select
        .selection
        .as_ref()
        .map(|expr| analyze_predicates(expr))
        .unwrap_or_default();

    let mut plan = generate_table_plan(
        &select.from[0],
        &predicates,
        select.selection.as_ref(),
        schema_lookup,
        row_count_lookup,
    );

    // Prefer a more accurate plan for the common 2-table join case so EXPLAIN can show
    // "Hash Join" when applicable.
    if select.from.len() == 1 && select.from[0].joins.len() == 1 {
        let join = &select.from[0].joins[0];
        let right_plan =
            generate_table_factor_plan(&join.relation, &[], None, schema_lookup, row_count_lookup);

        let join_type = match &join.join_operator {
            sqlparser::ast::JoinOperator::LeftOuter(_) => "Left",
            sqlparser::ast::JoinOperator::RightOuter(_) => "Right",
            sqlparser::ast::JoinOperator::FullOuter(_) => "Full",
            sqlparser::ast::JoinOperator::CrossJoin => "Cross",
            _ => "Inner",
        }
        .to_string();

        let join_condition = match &join.join_operator {
            sqlparser::ast::JoinOperator::Inner(sqlparser::ast::JoinConstraint::On(expr))
            | sqlparser::ast::JoinOperator::LeftOuter(sqlparser::ast::JoinConstraint::On(expr))
            | sqlparser::ast::JoinOperator::RightOuter(sqlparser::ast::JoinConstraint::On(expr))
            | sqlparser::ast::JoinOperator::FullOuter(sqlparser::ast::JoinConstraint::On(expr)) => {
                Some(expr)
            }
            _ => None,
        };

        let left_cost = get_plan_cost(&plan);
        let right_cost = get_plan_cost(&right_plan);
        let total_rows: usize = left_cost.rows.saturating_mul(right_cost.rows);
        let total_cost: f64 = left_cost.total + right_cost.total + (total_rows as f64 * 0.01);
        let cost = PlanCost {
            startup: 0.0,
            total: total_cost,
            rows: total_rows.max(1),
            width: DEFAULT_ROW_WIDTH,
        };

        let left_schema = match &select.from[0].relation {
            TableFactor::Table { name, .. } => name.0.last().and_then(|i| schema_lookup(&i.value)),
            _ => None,
        };
        let right_schema = match &join.relation {
            TableFactor::Table { name, .. } => name.0.last().and_then(|i| schema_lookup(&i.value)),
            _ => None,
        };

        let alg = match (join_condition, left_schema.as_ref(), right_schema.as_ref()) {
            (Some(cond), Some(l), Some(r)) => choose_join_algorithm(
                Some(cond),
                l,
                r,
                left_cost.rows,
                right_cost.rows,
                &HashJoinConfig::default(),
            ),
            _ => JoinAlgorithmChoice::NestedLoop,
        };

        plan = match alg {
            JoinAlgorithmChoice::HashJoin { .. } => PlanNode::HashJoin {
                join_type,
                hash_cond: join_condition.map(format_expr),
                cost,
                children: vec![plan, right_plan],
            },
            JoinAlgorithmChoice::NestedLoop => PlanNode::NestedLoop {
                join_type,
                cost,
                children: vec![plan, right_plan],
            },
        };
    } else if select.from.len() > 1 || !select.from[0].joins.is_empty() {
        let mut children = vec![plan.clone()];

        for join in &select.from[0].joins {
            let join_plan = generate_table_factor_plan(
                &join.relation,
                &[],
                None,
                schema_lookup,
                row_count_lookup,
            );
            children.push(join_plan);
        }

        for table_with_joins in select.from.iter().skip(1) {
            let table_plan =
                generate_table_plan(table_with_joins, &[], None, schema_lookup, row_count_lookup);
            children.push(table_plan);
        }

        if children.len() > 1 {
            let total_rows: usize = children.iter().map(|c| get_plan_cost(c).rows).product();
            let total_cost: f64 = children.iter().map(|c| get_plan_cost(c).total).sum::<f64>()
                + (total_rows as f64 * 0.01);
            plan = PlanNode::NestedLoop {
                join_type: "Inner".to_string(),
                cost: PlanCost {
                    startup: 0.0,
                    total: total_cost,
                    rows: total_rows.max(1),
                    width: DEFAULT_ROW_WIDTH,
                },
                children,
            };
        }
    }

    let group_by_exprs = match &select.group_by {
        sqlparser::ast::GroupByExpr::Expressions(exprs) => exprs.clone(),
        sqlparser::ast::GroupByExpr::All => vec![],
    };
    if !group_by_exprs.is_empty() {
        let keys: Vec<String> = group_by_exprs.iter().map(|e| format_expr(e)).collect();
        let child_cost = get_plan_cost(&plan);
        let agg_rows = (child_cost.rows / 10).max(1);
        plan = PlanNode::Aggregate {
            strategy: "HashAggregate".to_string(),
            keys,
            cost: PlanCost {
                startup: child_cost.total,
                total: child_cost.total + (agg_rows as f64 * 0.1),
                rows: agg_rows,
                width: child_cost.width,
            },
            child: Box::new(plan),
        };
    }

    plan
}

fn generate_table_plan(
    table_with_joins: &TableWithJoins,
    predicates: &[PredicateInfo],
    filter_expr: Option<&Expr>,
    schema_lookup: &impl Fn(&str) -> Option<TableSchema>,
    row_count_lookup: &impl Fn(&str) -> usize,
) -> PlanNode {
    generate_table_factor_plan(
        &table_with_joins.relation,
        predicates,
        filter_expr,
        schema_lookup,
        row_count_lookup,
    )
}

fn generate_table_factor_plan(
    table_factor: &TableFactor,
    predicates: &[PredicateInfo],
    filter_expr: Option<&Expr>,
    schema_lookup: &impl Fn(&str) -> Option<TableSchema>,
    row_count_lookup: &impl Fn(&str) -> usize,
) -> PlanNode {
    match table_factor {
        TableFactor::Table {
            name, alias, args, ..
        } => {
            let table_name = name.0.last().map(|i| i.value.as_str()).unwrap_or("");
            let alias_name = alias.as_ref().map(|a| a.name.value.clone());

            if args.is_some() {
                return PlanNode::TableFunctionScan {
                    function_name: table_name.to_string(),
                    alias: alias_name,
                    cost: PlanCost {
                        startup: 0.0,
                        total: 11.0,
                        rows: 1000,
                        width: DEFAULT_ROW_WIDTH,
                    },
                };
            }

            let estimated_rows = row_count_lookup(table_name);

            if let Some(schema) = schema_lookup(table_name) {
                let access_path =
                    choose_best_access_path_for_filter(0, &schema, filter_expr, estimated_rows);

                match access_path.scan_type {
                    ScanType::IndexScan {
                        index_name,
                        estimated_rows: est_rows,
                        ..
                    } => {
                        let index_cond = predicates
                            .iter()
                            .map(|p| format_predicate(p))
                            .collect::<Vec<_>>()
                            .join(" AND ");
                        PlanNode::IndexScan {
                            table_name: table_name.to_string(),
                            alias: alias_name,
                            index_name,
                            index_cond: if index_cond.is_empty() {
                                None
                            } else {
                                Some(index_cond)
                            },
                            filter: None,
                            cost: PlanCost {
                                startup: 0.15,
                                total: 0.15 + (est_rows as f64 * 0.01),
                                rows: est_rows.max(1),
                                width: estimate_row_width(&schema),
                            },
                        }
                    }
                    ScanType::IndexRangeScan {
                        index_name,
                        estimated_rows: est_rows,
                        ..
                    } => {
                        let index_cond = predicates
                            .iter()
                            .map(|p| format_predicate(p))
                            .collect::<Vec<_>>()
                            .join(" AND ");
                        PlanNode::IndexScan {
                            table_name: table_name.to_string(),
                            alias: alias_name,
                            index_name,
                            index_cond: if index_cond.is_empty() {
                                None
                            } else {
                                Some(index_cond)
                            },
                            filter: None,
                            cost: PlanCost {
                                startup: 0.15,
                                total: 0.15 + (est_rows as f64 * 0.01),
                                rows: est_rows.max(1),
                                width: estimate_row_width(&schema),
                            },
                        }
                    }
                    ScanType::IndexBoundedRangeScan {
                        index_name,
                        estimated_rows: est_rows,
                        ..
                    } => {
                        let index_cond = predicates
                            .iter()
                            .map(|p| format_predicate(p))
                            .collect::<Vec<_>>()
                            .join(" AND ");
                        PlanNode::IndexScan {
                            table_name: table_name.to_string(),
                            alias: alias_name,
                            index_name,
                            index_cond: if index_cond.is_empty() {
                                None
                            } else {
                                Some(index_cond)
                            },
                            filter: None,
                            cost: PlanCost {
                                startup: 0.15,
                                total: 0.15 + (est_rows as f64 * 0.01),
                                rows: est_rows.max(1),
                                width: estimate_row_width(&schema),
                            },
                        }
                    }
                    ScanType::InListScan {
                        index_name,
                        estimated_rows: est_rows,
                        ..
                    } => {
                        let index_cond = predicates
                            .iter()
                            .map(|p| format_predicate(p))
                            .collect::<Vec<_>>()
                            .join(" AND ");
                        PlanNode::IndexScan {
                            table_name: table_name.to_string(),
                            alias: alias_name,
                            index_name,
                            index_cond: if index_cond.is_empty() {
                                None
                            } else {
                                Some(index_cond)
                            },
                            filter: None,
                            cost: PlanCost {
                                startup: 0.15,
                                total: 0.15 + (est_rows as f64 * 0.01),
                                rows: est_rows.max(1),
                                width: estimate_row_width(&schema),
                            },
                        }
                    }
                    ScanType::GinIndexScan {
                        index_name,
                        estimated_rows: est_rows,
                        ..
                    } => {
                        let index_cond = filter_expr.map(|e| format_expr(e));
                        PlanNode::IndexScan {
                            table_name: table_name.to_string(),
                            alias: alias_name,
                            index_name,
                            index_cond,
                            filter: None,
                            cost: PlanCost {
                                startup: 0.15,
                                total: 0.15 + (est_rows as f64 * 0.01),
                                rows: est_rows.max(1),
                                width: estimate_row_width(&schema),
                            },
                        }
                    }
                    ScanType::FullTableScan => {
                        let filter = filter_expr.map(|e| format_expr(e));
                        PlanNode::SeqScan {
                            table_name: table_name.to_string(),
                            alias: alias_name,
                            filter,
                            cost: PlanCost {
                                startup: 0.0,
                                total: estimated_rows as f64 * 0.01 + 1.0,
                                rows: estimated_rows.max(1),
                                width: estimate_row_width(&schema),
                            },
                        }
                    }
                }
            } else {
                let filter = filter_expr.map(|e| format_expr(e));
                PlanNode::SeqScan {
                    table_name: table_name.to_string(),
                    alias: alias_name,
                    filter,
                    cost: PlanCost {
                        startup: 0.0,
                        total: estimated_rows as f64 * 0.01 + 1.0,
                        rows: estimated_rows.max(1),
                        width: DEFAULT_ROW_WIDTH,
                    },
                }
            }
        }
        TableFactor::Derived {
            subquery, alias: _, ..
        } => {
            let subquery_plan = generate_query_plan(subquery, schema_lookup, row_count_lookup);
            subquery_plan
        }
        _ => PlanNode::Result {
            cost: PlanCost::default(),
        },
    }
}

fn estimate_row_width(schema: &TableSchema) -> usize {
    schema
        .columns
        .iter()
        .map(|c| c.data_type.estimated_size())
        .sum::<usize>()
        .max(8)
}

fn get_plan_cost(plan: &PlanNode) -> PlanCost {
    match plan {
        PlanNode::SeqScan { cost, .. } => cost.clone(),
        PlanNode::IndexScan { cost, .. } => cost.clone(),
        PlanNode::NestedLoop { cost, .. } => cost.clone(),
        PlanNode::HashJoin { cost, .. } => cost.clone(),
        PlanNode::Sort { cost, .. } => cost.clone(),
        PlanNode::Limit { cost, .. } => cost.clone(),
        PlanNode::Aggregate { cost, .. } => cost.clone(),
        PlanNode::Filter { cost, .. } => cost.clone(),
        PlanNode::TableFunctionScan { cost, .. } => cost.clone(),
        PlanNode::Result { cost } => cost.clone(),
    }
}

fn extract_limit_value(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::Value(sqlparser::ast::Value::Number(s, _)) => s.parse().ok(),
        _ => None,
    }
}

fn format_expr(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(ident) => ident.value.clone(),
        Expr::CompoundIdentifier(idents) => idents
            .iter()
            .map(|i| i.value.as_str())
            .collect::<Vec<_>>()
            .join("."),
        Expr::Value(v) => format!("{}", v),
        Expr::BinaryOp { left, op, right } => {
            format!("({} {} {})", format_expr(left), op, format_expr(right))
        }
        Expr::UnaryOp { op, expr } => format!("{} {}", op, format_expr(expr)),
        Expr::IsNull(e) => format!("({} IS NULL)", format_expr(e)),
        Expr::IsNotNull(e) => format!("({} IS NOT NULL)", format_expr(e)),
        Expr::Nested(e) => format!("({})", format_expr(e)),
        Expr::Function(f) => {
            let args: Vec<String> = f.args.iter().map(|a| format!("{}", a)).collect();
            format!("{}({})", f.name, args.join(", "))
        }
        _ => format!("{}", expr),
    }
}

fn format_predicate(pred: &PredicateInfo) -> String {
    let op_str = match pred.op {
        super::planner::PredicateOp::Eq => "=",
        super::planner::PredicateOp::Ne => "<>",
        super::planner::PredicateOp::Lt => "<",
        super::planner::PredicateOp::Le => "<=",
        super::planner::PredicateOp::Gt => ">",
        super::planner::PredicateOp::Ge => ">=",
        super::planner::PredicateOp::In => "IN",
        super::planner::PredicateOp::IsNull => "IS NULL",
        super::planner::PredicateOp::IsNotNull => "IS NOT NULL",
    };

    match pred.op {
        super::planner::PredicateOp::IsNull | super::planner::PredicateOp::IsNotNull => {
            format!("({} {})", pred.column, op_str)
        }
        super::planner::PredicateOp::In => {
            let values = if pred.in_values.is_empty() {
                vec![pred.value.clone()]
            } else {
                pred.in_values.clone()
            };
            let list = values
                .iter()
                .map(format_value)
                .collect::<Vec<_>>()
                .join(", ");
            format!("({} IN ({}))", pred.column, list)
        }
        _ => format!("({} {} {})", pred.column, op_str, format_value(&pred.value)),
    }
}

fn format_value(value: &crate::types::Value) -> String {
    match value {
        crate::types::Value::Null => "NULL".to_string(),
        crate::types::Value::Boolean(b) => b.to_string(),
        crate::types::Value::Int32(i) => i.to_string(),
        crate::types::Value::Int64(i) => i.to_string(),
        crate::types::Value::Float64(f) => f.to_string(),
        crate::types::Value::Text(s) => format!("'{}'", s),
        crate::types::Value::Bytes(b) => format!("'\\x{}'", hex::encode(b)),
        crate::types::Value::Timestamp(ts) => format!("'{}'", ts),
        crate::types::Value::Interval(i) => format!("'{}'", i),
        crate::types::Value::Uuid(u) => {
            let uuid = uuid::Uuid::from_bytes(*u);
            format!("'{}'", uuid)
        }
        crate::types::Value::Json(j) => format!("'{}'", j),
        crate::types::Value::Jsonb(j) => format!("'{}'", j),
        crate::types::Value::Array(arr) => {
            let elems: Vec<String> = arr.iter().map(format_value).collect();
            format!("ARRAY[{}]", elems.join(", "))
        }
        crate::types::Value::Vector(vec) => {
            let elems: Vec<String> = vec.iter().map(|f| f.to_string()).collect();
            format!("'[{}]'", elems.join(","))
        }
        crate::types::Value::Time(micros) => {
            let total_secs = micros / 1_000_000;
            let hours = total_secs / 3600;
            let mins = (total_secs % 3600) / 60;
            let secs = total_secs % 60;
            format!("'{:02}:{:02}:{:02}'", hours, mins, secs)
        }
        crate::types::Value::Date(days) => {
            let s =
                crate::types::date::format_date_days(*days).unwrap_or_else(|_| days.to_string());
            format!("'{}'", s)
        }
        crate::types::Value::Numeric(d) => d.to_string(),
        crate::types::Value::Tsvector(s) | crate::types::Value::Tsquery(s) => format!("'{}'", s),
    }
}

// ── PhysicalPlan → PlanNode translator ──────────────────────────────
//
// Used when the optimizer path is active (GUC on + eligible). Translates the
// optimizer's PhysicalPlan to the display-oriented PlanNode tree so EXPLAIN
// shows the same plan that execution actually uses.

/// Convert a PhysicalPlan tree into a PlanNode tree for EXPLAIN display.
pub fn physical_plan_to_plan_node(
    phys: &crate::sql::optimizer::physical_plan::PhysicalPlan,
) -> PlanNode {
    use crate::sql::optimizer::physical_plan::PhysicalNode;

    let cost = PlanCost {
        startup: phys.cost.startup,
        total: phys.cost.total,
        rows: phys.cost.rows,
        width: phys.schema.columns.len().saturating_mul(DEFAULT_ROW_WIDTH),
    };

    match &phys.node {
        PhysicalNode::SeqScan { table_name, alias } => PlanNode::SeqScan {
            table_name: table_name.clone(),
            alias: alias.clone(),
            filter: None,
            cost,
        },
        PhysicalNode::IndexScan {
            table_name,
            alias,
            scan_type,
        } => {
            let index_name = match scan_type {
                ScanType::IndexScan { index_name, .. }
                | ScanType::IndexRangeScan { index_name, .. }
                | ScanType::IndexBoundedRangeScan { index_name, .. }
                | ScanType::InListScan { index_name, .. } => index_name.clone(),
                _ => "unknown".to_string(),
            };
            PlanNode::IndexScan {
                table_name: table_name.clone(),
                alias: alias.clone(),
                index_name,
                index_cond: None,
                filter: None,
                cost,
            }
        }
        PhysicalNode::Filter { predicate, input } => {
            let child = physical_plan_to_plan_node(input);
            PlanNode::Filter {
                condition: format_typed_expr(predicate),
                cost,
                child: Box::new(child),
            }
        }
        PhysicalNode::Project { input, .. } => {
            // Project is implicit in EXPLAIN — show the child directly.
            physical_plan_to_plan_node(input)
        }
        PhysicalNode::NestedLoopJoin {
            left,
            right,
            join_type,
            ..
        } => {
            let left_node = physical_plan_to_plan_node(left);
            let right_node = physical_plan_to_plan_node(right);
            PlanNode::NestedLoop {
                join_type: format!("{:?}", join_type),
                cost,
                children: vec![left_node, right_node],
            }
        }
        PhysicalNode::HashJoin {
            left,
            right,
            join_type,
            condition,
            ..
        } => {
            let left_node = physical_plan_to_plan_node(left);
            let right_node = physical_plan_to_plan_node(right);
            let cond_str = match condition {
                JoinCondition::On(expr) => Some(format_typed_expr(expr)),
                JoinCondition::Using(cols) => Some(
                    cols.iter()
                        .map(|c| c.name.clone())
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
                JoinCondition::None => None,
            };
            PlanNode::HashJoin {
                join_type: format!("{:?}", join_type),
                hash_cond: cond_str,
                cost,
                children: vec![left_node, right_node],
            }
        }
        PhysicalNode::Sort { order_by, input } => {
            let child = physical_plan_to_plan_node(input);
            let sort_keys: Vec<String> = order_by
                .iter()
                .map(|ob| {
                    let dir = if ob.asc { "ASC" } else { "DESC" };
                    format!("{} {}", format_typed_expr(&ob.expr), dir)
                })
                .collect();
            PlanNode::Sort {
                sort_key: sort_keys,
                cost,
                child: Box::new(child),
            }
        }
        PhysicalNode::TopNSort {
            order_by,
            limit,
            input,
        } => {
            let child = physical_plan_to_plan_node(input);
            let sort_keys: Vec<String> = order_by
                .iter()
                .map(|ob| {
                    let dir = if ob.asc { "ASC" } else { "DESC" };
                    format!("{} {}", format_typed_expr(&ob.expr), dir)
                })
                .collect();
            let sorted = PlanNode::Sort {
                sort_key: sort_keys,
                cost: cost.clone(),
                child: Box::new(child),
            };
            PlanNode::Limit {
                count: *limit,
                cost,
                child: Box::new(sorted),
            }
        }
        PhysicalNode::Limit { input, .. } => {
            let child = physical_plan_to_plan_node(input);
            PlanNode::Limit {
                count: phys.cost.rows,
                cost,
                child: Box::new(child),
            }
        }
        PhysicalNode::HashAggregate {
            group_by, input, ..
        } => {
            let child = physical_plan_to_plan_node(input);
            let keys: Vec<String> = group_by.iter().map(format_typed_expr).collect();
            let strategy = if keys.is_empty() {
                "Plain".to_string()
            } else {
                "HashAggregate".to_string()
            };
            PlanNode::Aggregate {
                strategy,
                keys,
                cost,
                child: Box::new(child),
            }
        }
        PhysicalNode::StreamAggregate {
            group_by, input, ..
        } => {
            let child = physical_plan_to_plan_node(input);
            let keys: Vec<String> = group_by.iter().map(format_typed_expr).collect();
            PlanNode::Aggregate {
                strategy: "GroupAggregate".to_string(),
                keys,
                cost,
                child: Box::new(child),
            }
        }
        PhysicalNode::Distinct { input } | PhysicalNode::DistinctOn { input, .. } => {
            let child = physical_plan_to_plan_node(input);
            PlanNode::Aggregate {
                strategy: "Unique".to_string(),
                keys: vec![],
                cost,
                child: Box::new(child),
            }
        }
        PhysicalNode::Window { input } => {
            // Window functions don't have a dedicated PlanNode — show child.
            physical_plan_to_plan_node(input)
        }
        PhysicalNode::SetOperation { left, right, .. } => {
            // Approximate as nested loop for display purposes.
            let left_node = physical_plan_to_plan_node(left);
            let right_node = physical_plan_to_plan_node(right);
            PlanNode::NestedLoop {
                join_type: "SetOperation".to_string(),
                cost,
                children: vec![left_node, right_node],
            }
        }
        PhysicalNode::Empty | PhysicalNode::Values { .. } => PlanNode::Result { cost },
        PhysicalNode::TableFunction {
            function_name,
            alias,
            ..
        } => PlanNode::TableFunctionScan {
            function_name: function_name.clone(),
            alias: alias.clone(),
            cost,
        },
        PhysicalNode::Subquery { subplan, .. } => physical_plan_to_plan_node(subplan),
    }
}

pub fn format_plan_text(plan: &PlanNode, indent: usize) -> String {
    let mut output = String::new();
    format_plan_node(&mut output, plan, indent, true);
    output
}

fn format_plan_node(output: &mut String, plan: &PlanNode, indent: usize, is_first: bool) {
    let prefix = if is_first {
        " ".repeat(indent)
    } else {
        format!("{}->  ", " ".repeat(indent.saturating_sub(4)))
    };

    match plan {
        PlanNode::SeqScan {
            table_name,
            alias,
            filter,
            cost,
        } => {
            let table_display = if let Some(a) = alias {
                format!("{} {}", table_name, a)
            } else {
                table_name.clone()
            };
            writeln!(
                output,
                "{}Seq Scan on {}  (cost={:.2}..{:.2} rows={} width={})",
                prefix, table_display, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            if let Some(f) = filter {
                writeln!(output, "{}  Filter: {}", " ".repeat(indent), f).unwrap();
            }
        }
        PlanNode::IndexScan {
            table_name,
            alias,
            index_name,
            index_cond,
            filter,
            cost,
        } => {
            let table_display = if let Some(a) = alias {
                format!("{} {}", table_name, a)
            } else {
                table_name.clone()
            };
            writeln!(
                output,
                "{}Index Scan using {} on {}  (cost={:.2}..{:.2} rows={} width={})",
                prefix, index_name, table_display, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            if let Some(cond) = index_cond {
                writeln!(output, "{}  Index Cond: {}", " ".repeat(indent), cond).unwrap();
            }
            if let Some(f) = filter {
                writeln!(output, "{}  Filter: {}", " ".repeat(indent), f).unwrap();
            }
        }
        PlanNode::NestedLoop {
            join_type,
            cost,
            children,
        } => {
            writeln!(
                output,
                "{}Nested Loop {}  (cost={:.2}..{:.2} rows={} width={})",
                prefix, join_type, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            for (i, child) in children.iter().enumerate() {
                format_plan_node(output, child, indent + 6, i == 0);
            }
        }
        PlanNode::HashJoin {
            join_type,
            hash_cond,
            cost,
            children,
        } => {
            writeln!(
                output,
                "{}Hash Join {}  (cost={:.2}..{:.2} rows={} width={})",
                prefix, join_type, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            if let Some(cond) = hash_cond {
                writeln!(output, "{}  Hash Cond: {}", " ".repeat(indent), cond).unwrap();
            }
            for (i, child) in children.iter().enumerate() {
                format_plan_node(output, child, indent + 6, i == 0);
            }
        }
        PlanNode::Sort {
            sort_key,
            cost,
            child,
        } => {
            writeln!(
                output,
                "{}Sort  (cost={:.2}..{:.2} rows={} width={})",
                prefix, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            writeln!(
                output,
                "{}  Sort Key: {}",
                " ".repeat(indent),
                sort_key.join(", ")
            )
            .unwrap();
            format_plan_node(output, child, indent + 6, false);
        }
        PlanNode::Limit {
            count: _,
            cost,
            child,
        } => {
            writeln!(
                output,
                "{}Limit  (cost={:.2}..{:.2} rows={} width={})",
                prefix, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            format_plan_node(output, child, indent + 6, false);
        }
        PlanNode::Aggregate {
            strategy,
            keys,
            cost,
            child,
        } => {
            let key_display = if keys.is_empty() {
                String::new()
            } else {
                format!("  Group Key: {}", keys.join(", "))
            };
            writeln!(
                output,
                "{}{}  (cost={:.2}..{:.2} rows={} width={})",
                prefix, strategy, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            if !key_display.is_empty() {
                writeln!(output, "{}{}", " ".repeat(indent), key_display).unwrap();
            }
            format_plan_node(output, child, indent + 6, false);
        }
        PlanNode::Filter {
            condition,
            cost,
            child,
        } => {
            writeln!(
                output,
                "{}Filter  (cost={:.2}..{:.2} rows={} width={})",
                prefix, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
            writeln!(output, "{}  Filter: {}", " ".repeat(indent), condition).unwrap();
            format_plan_node(output, child, indent + 6, false);
        }
        PlanNode::TableFunctionScan {
            function_name,
            alias,
            cost,
        } => {
            let display = if let Some(a) = alias {
                format!("{} {}", function_name, a)
            } else {
                function_name.clone()
            };
            writeln!(
                output,
                "{}Function Scan on {}  (cost={:.2}..{:.2} rows={} width={})",
                prefix, display, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
        }
        PlanNode::Result { cost } => {
            writeln!(
                output,
                "{}Result  (cost={:.2}..{:.2} rows={} width={})",
                prefix, cost.startup, cost.total, cost.rows, cost.width
            )
            .unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnDef, DataType, IndexDef};

    fn dummy_schema_lookup(name: &str) -> Option<TableSchema> {
        if name == "users" {
            Some(TableSchema {
                name: "users".to_string(),
                table_id: 1,
                columns: vec![
                    ColumnDef {
                        name: "id".to_string(),
                        data_type: DataType::Int32,
                        nullable: false,
                        primary_key: true,
                        unique: true,
                        is_serial: false,
                        default_expr: None,
                    },
                    ColumnDef {
                        name: "name".to_string(),
                        data_type: DataType::Text,
                        nullable: true,
                        primary_key: false,
                        unique: false,
                        is_serial: false,
                        default_expr: None,
                    },
                ],
                version: 1,
                pk_constraint_name: Some("users_pkey".to_string()),
                pk_indices: vec![0],
                indexes: vec![IndexDef {
                    id: 1,
                    name: "users_pkey".to_string(),
                    columns: vec!["id".to_string()],
                    unique: true,
                    method: None,
                    predicate: None,
                    expressions: Vec::new(),
                }],
                check_constraints: vec![],
                foreign_keys: vec![],
                owner: String::new(),
                from_alias: None,
            })
        } else {
            None
        }
    }

    fn dummy_row_count(_name: &str) -> usize {
        1000
    }

    #[test]
    fn test_seq_scan_plan() {
        let sql = "SELECT * FROM users WHERE name = 'Alice'";
        let dialect = sqlparser::dialect::PostgreSqlDialect {};
        let ast = sqlparser::parser::Parser::parse_sql(&dialect, sql).unwrap();

        let plan = generate_plan(&ast[0], dummy_schema_lookup, dummy_row_count);
        let output = format_plan_text(&plan, 0);

        assert!(output.contains("Seq Scan on users"));
        assert!(output.contains("Filter:"));
    }

    #[test]
    fn test_index_scan_plan() {
        let sql = "SELECT * FROM users WHERE id = 1";
        let dialect = sqlparser::dialect::PostgreSqlDialect {};
        let ast = sqlparser::parser::Parser::parse_sql(&dialect, sql).unwrap();

        let plan = generate_plan(&ast[0], dummy_schema_lookup, dummy_row_count);
        let output = format_plan_text(&plan, 0);

        assert!(output.contains("Index Scan using users_pkey on users"));
        assert!(output.contains("Index Cond:"));
    }

    #[test]
    fn test_table_function_plan() {
        let sql = "SELECT * FROM extensions.fs9('./*.rs')";
        let dialect = sqlparser::dialect::PostgreSqlDialect {};
        let ast = sqlparser::parser::Parser::parse_sql(&dialect, sql).unwrap();

        let plan = generate_plan(&ast[0], dummy_schema_lookup, dummy_row_count);
        let output = format_plan_text(&plan, 0);

        assert!(
            output.contains("Function Scan on fs9"),
            "EXPLAIN should show Function Scan for table functions, got: {}",
            output
        );
    }

    #[test]
    fn test_simple_select_plan() {
        let sql = "SELECT 1";
        let dialect = sqlparser::dialect::PostgreSqlDialect {};
        let ast = sqlparser::parser::Parser::parse_sql(&dialect, sql).unwrap();

        let plan = generate_plan(&ast[0], dummy_schema_lookup, dummy_row_count);
        let output = format_plan_text(&plan, 0);

        assert!(output.contains("Result"));
    }
}
