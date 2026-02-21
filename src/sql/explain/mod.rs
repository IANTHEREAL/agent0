//! EXPLAIN statement - PostgreSQL-compatible query plan output.
//!
//! This module provides:
//! - `PlanNode` / `PlanCost` — the display-oriented plan tree
//! - `physical_plan_to_plan_node` — converts optimizer output to `PlanNode`
//! - `format_plan_text` — renders a `PlanNode` tree as EXPLAIN text
//! - AST-based plan generation (test-only compatibility path)

mod format;
mod transform;

#[cfg(test)]
mod tests;

// Re-export public API
pub use format::format_plan_text;
pub(crate) use format::format_typed_expr;
pub use transform::physical_plan_to_plan_node;

#[cfg(test)]
use sqlparser::ast::{Expr, Query, Select, SetExpr, Statement, TableFactor, TableWithJoins};

#[cfg(test)]
use super::operators::HashJoinConfig;
#[cfg(test)]
use super::planner::ScanType;
#[cfg(test)]
use super::planner::{
    analyze_predicates, choose_best_access_path_for_filter, choose_join_algorithm,
    JoinAlgorithmChoice, PredicateInfo,
};
#[cfg(test)]
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
    SemiJoin {
        anti: bool,
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

// ── AST-based plan generation (test-only compatibility path) ──

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
fn estimate_row_width(schema: &TableSchema) -> usize {
    schema
        .columns
        .iter()
        .map(|c| c.data_type.estimated_size())
        .sum::<usize>()
        .max(8)
}

#[cfg(test)]
fn get_plan_cost(plan: &PlanNode) -> PlanCost {
    match plan {
        PlanNode::SeqScan { cost, .. } => cost.clone(),
        PlanNode::IndexScan { cost, .. } => cost.clone(),
        PlanNode::NestedLoop { cost, .. } => cost.clone(),
        PlanNode::HashJoin { cost, .. } => cost.clone(),
        PlanNode::Sort { cost, .. } => cost.clone(),
        PlanNode::Limit { cost, .. } => cost.clone(),
        PlanNode::Aggregate { cost, .. } => cost.clone(),
        PlanNode::SemiJoin { cost, .. } => cost.clone(),
        PlanNode::Filter { cost, .. } => cost.clone(),
        PlanNode::TableFunctionScan { cost, .. } => cost.clone(),
        PlanNode::Result { cost } => cost.clone(),
    }
}

#[cfg(test)]
fn extract_limit_value(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::Value(sqlparser::ast::Value::Number(s, _)) => s.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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
