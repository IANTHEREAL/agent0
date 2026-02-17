//! Logical planner: `AnalyzedQuery → LogicalPlan`.
//!
//! Pure structural translation — no optimization. Each SQL clause maps
//! to exactly one logical node in a fixed order.
//!
//! **Non-aggregate path** (Sort on full-width rows, then narrow):
//! ```text
//! FROM       → Scan / Join / Subquery
//! WHERE      → Filter
//! ORDER BY   → Sort          ← on full scan-scope rows
//! SELECT     → Project       ← narrows to output columns
//! DISTINCT   → Distinct / DistinctOn
//! ```
//!
//! **Aggregate path** (rewrite ORDER BY / HAVING for post-agg schema):
//! ```text
//! FROM       → Scan / Join / Subquery
//! WHERE      → Filter
//! GROUP BY   → Aggregate
//! HAVING     → Filter (rewritten)
//! ORDER BY   → Sort   (rewritten)
//! DISTINCT   → Distinct
//! ```
//!
//! Precondition: the caller has verified `is_optimizer_eligible()`, which
//! guarantees that all ORDER BY / HAVING rewrites will succeed.  This
//! function therefore always returns a plan (no `Option`).

use super::logical_plan::{LogicalNode, LogicalPlan, PlanSchema};
use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef, AnalyzedTableRefKind,
    TypedExpr, TypedExprKind, TypedOrderByExpr,
};
use crate::sql::analyzer::AnalyzedQuery;
use crate::sql::operators::AggregateExpr;
use crate::types::DataType;

/// Builds a [`LogicalPlan`] from an [`AnalyzedQuery`].
pub struct LogicalPlanner;

impl LogicalPlanner {
    /// Build a logical plan from an analyzed query.
    ///
    /// Precondition: `is_optimizer_eligible()` has been checked by the caller.
    /// This guarantees all aggregate rewrites succeed, so this function always
    /// returns a plan.
    pub fn build(query: &AnalyzedQuery) -> LogicalPlan {
        let mut plan = Self::build_body(&query.body, &query.output_schema, &query.order_by);

        // LIMIT / OFFSET
        if query.limit.is_some() || query.offset.is_some() {
            plan = plan.limit(query.limit.clone(), query.offset.clone());
        }

        plan
    }

    fn build_body(
        body: &AnalyzedQueryBody,
        output_schema: &[(String, DataType)],
        order_by: &[TypedOrderByExpr],
    ) -> LogicalPlan {
        match body {
            AnalyzedQueryBody::Select(select) => {
                Self::build_select(select, output_schema, order_by)
            }
            AnalyzedQueryBody::Values(rows) => {
                let schema = PlanSchema::from_columns(output_schema.to_vec());
                let mut plan = LogicalPlan {
                    node: LogicalNode::Values { rows: rows.clone() },
                    schema,
                };
                // For Values / SetOperation, Sort stays on top (original behavior).
                if !order_by.is_empty() {
                    plan = plan.sort(order_by.to_vec());
                }
                plan
            }
            AnalyzedQueryBody::SetOperation {
                op,
                all,
                left,
                right,
            } => {
                let left_plan = Self::build(left);
                let right_plan = Self::build(right);
                let schema = PlanSchema::from_columns(output_schema.to_vec());
                let mut plan = LogicalPlan {
                    node: LogicalNode::SetOperation {
                        op: *op,
                        all: *all,
                        left: Box::new(left_plan),
                        right: Box::new(right_plan),
                    },
                    schema,
                };
                // For Values / SetOperation, Sort stays on top (original behavior).
                if !order_by.is_empty() {
                    plan = plan.sort(order_by.to_vec());
                }
                plan
            }
        }
    }

    fn build_select(
        select: &AnalyzedSelect,
        output_schema: &[(String, DataType)],
        order_by: &[TypedOrderByExpr],
    ) -> LogicalPlan {
        // 1. FROM clause → base plan
        let mut plan = Self::build_from(&select.from);

        // 2. WHERE → Filter
        if let Some(predicate) = &select.where_clause {
            plan = plan.filter(predicate.clone());
        }

        let proj_schema = PlanSchema::from_columns(output_schema.to_vec());
        if !select.group_by.is_empty()
            || has_aggregates(&select.projection)
            || select.having.is_some()
        {
            // ── Aggregate path ──
            // Sort and HAVING must be rewritten to reference post-aggregate
            // column positions instead of scan-scope column indices.
            plan = plan.aggregate(
                select.group_by.clone(),
                select.projection.clone(),
                proj_schema,
            );

            // Extract aggregate metadata needed for rewriting.
            let group_by = &select.group_by;
            let group_by_count = group_by.len();
            let aggregate_exprs = Self::collect_aggregate_exprs(&select.projection);

            // HAVING → Filter (rewritten)
            if let Some(having) = &select.having {
                let rewritten = super::build::rewrite_post_aggregate_expr(
                    having,
                    group_by,
                    group_by_count,
                    &aggregate_exprs,
                )
                .expect("eligibility gate guarantees HAVING rewrite succeeds");
                plan = plan.filter(rewritten);
            }

            // ORDER BY (rewritten)
            if !order_by.is_empty() {
                let rewritten_order: Vec<TypedOrderByExpr> = order_by
                    .iter()
                    .map(|ob| {
                        let expr = super::build::rewrite_post_aggregate_expr(
                            &ob.expr,
                            group_by,
                            group_by_count,
                            &aggregate_exprs,
                        )
                        .expect("eligibility gate guarantees ORDER BY rewrite succeeds");
                        TypedOrderByExpr {
                            expr,
                            asc: ob.asc,
                            nulls_first: ob.nulls_first,
                        }
                    })
                    .collect();
                plan = plan.sort(rewritten_order);
            }

            // DISTINCT (on post-aggregate rows)
            match &select.distinct {
                AnalyzedDistinct::All => {}
                AnalyzedDistinct::Distinct => {
                    plan = plan.distinct();
                }
                AnalyzedDistinct::DistinctOn(_) => {
                    // DISTINCT ON is rejected by eligibility gate.
                    unreachable!("DISTINCT ON rejected by eligibility gate");
                }
            }
        } else {
            // ── Non-aggregate path ──
            // Sort on full-width rows (before projection narrows them).
            if !order_by.is_empty() {
                plan = plan.sort(order_by.to_vec());
            }

            // Project (narrows columns)
            plan = plan.project(select.projection.clone(), proj_schema);

            // DISTINCT (on projected rows)
            match &select.distinct {
                AnalyzedDistinct::All => {}
                AnalyzedDistinct::Distinct => {
                    plan = plan.distinct();
                }
                AnalyzedDistinct::DistinctOn(_) => {
                    // DISTINCT ON is rejected by eligibility gate.
                    unreachable!("DISTINCT ON rejected by eligibility gate");
                }
            }
        }

        plan
    }

    /// Collect unique AggregateExpr from projection list (for rewriting).
    fn collect_aggregate_exprs(
        projections: &[crate::sql::analyzer::types::AnalyzedProjection],
    ) -> Vec<AggregateExpr> {
        let mut agg_exprs = Vec::new();
        let mut agg_names = Vec::new();
        let mut agg_types = Vec::new();
        for proj in projections {
            super::build::collect_agg_exprs_from(
                &proj.expr,
                &proj.output_name,
                &mut agg_exprs,
                &mut agg_names,
                &mut agg_types,
            );
        }
        agg_exprs
    }

    fn build_from(from: &[AnalyzedTableRef]) -> LogicalPlan {
        if from.is_empty() {
            return LogicalPlan::empty(PlanSchema::from_columns(vec![]));
        }

        let mut plan = Self::build_table_ref(&from[0]);

        // Additional FROM items → cross joins
        for table_ref in from.iter().skip(1) {
            let right = Self::build_table_ref(table_ref);
            let mut combined_cols = plan.schema.columns.clone();
            combined_cols.extend(right.schema.columns.clone());
            let schema = PlanSchema::from_columns(combined_cols);
            plan = LogicalPlan {
                node: LogicalNode::Join {
                    left: Box::new(plan),
                    right: Box::new(right),
                    join_type: crate::sql::analyzer::types::JoinType::Cross,
                    condition: crate::sql::analyzer::types::JoinCondition::None,
                },
                schema,
            };
        }

        plan
    }

    fn build_table_ref(table_ref: &AnalyzedTableRef) -> LogicalPlan {
        match &table_ref.kind {
            AnalyzedTableRefKind::Table { name, schema } => {
                let plan_schema = PlanSchema::from_columns(
                    schema
                        .columns
                        .iter()
                        .map(|(name, dt, _nullable)| (name.clone(), dt.clone()))
                        .collect(),
                );
                LogicalPlan::scan(name.clone(), table_ref.alias.clone(), plan_schema)
            }
            AnalyzedTableRefKind::Subquery(subquery) => {
                let subplan = Self::build(subquery);
                let schema = subplan.schema.clone();
                LogicalPlan {
                    node: LogicalNode::Subquery {
                        subplan: Box::new(subplan),
                        alias: table_ref.alias.clone(),
                    },
                    schema,
                }
            }
            AnalyzedTableRefKind::Join {
                left,
                right,
                join_type,
                condition,
                left_col_start,
            } => {
                let left_plan = Self::build_table_ref(left);
                let right_plan = Self::build_table_ref(right);
                let mut combined_cols = left_plan.schema.columns.clone();
                combined_cols.extend(right_plan.schema.columns.clone());
                let schema = PlanSchema::from_columns(combined_cols);
                // Normalize ON condition indices from global (analyzer scope) to local
                // (relative to this join's combined schema). left_col_start is the global
                // offset where this join's left child begins.
                let normalized_condition =
                    crate::sql::analyzer::types::reindex_join_condition(condition, *left_col_start);
                LogicalPlan {
                    node: LogicalNode::Join {
                        left: Box::new(left_plan),
                        right: Box::new(right_plan),
                        join_type: *join_type,
                        condition: normalized_condition,
                    },
                    schema,
                }
            }
            AnalyzedTableRefKind::Function {
                func,
                args,
                output_columns,
            } => {
                let plan_schema = PlanSchema::from_columns(output_columns.clone());
                LogicalPlan {
                    node: LogicalNode::TableFunction {
                        function_name: func.name.clone(),
                        args: args.clone(),
                        alias: table_ref.alias.clone(),
                    },
                    schema: plan_schema,
                }
            }
        }
    }
}

/// Check if any projection item contains an aggregate function call.
fn has_aggregates(projections: &[crate::sql::analyzer::types::AnalyzedProjection]) -> bool {
    projections.iter().any(|p| expr_has_aggregate(&p.expr))
}

pub(crate) fn expr_has_aggregate(expr: &TypedExpr) -> bool {
    match &expr.kind {
        TypedExprKind::AggregateCall { .. } => true,
        TypedExprKind::BinaryOp { left, right, .. } => {
            expr_has_aggregate(left) || expr_has_aggregate(right)
        }
        TypedExprKind::UnaryOp { operand, .. } => expr_has_aggregate(operand),
        TypedExprKind::Cast { expr, .. } => expr_has_aggregate(expr),
        TypedExprKind::FunctionCall { args, .. } => args.iter().any(expr_has_aggregate),
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            operand.as_ref().map_or(false, |e| expr_has_aggregate(e))
                || when_clauses
                    .iter()
                    .any(|(w, t)| expr_has_aggregate(w) || expr_has_aggregate(t))
                || else_result
                    .as_ref()
                    .map_or(false, |e| expr_has_aggregate(e))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::*;
    use crate::types::DataType;

    fn simple_column(name: &str, dt: DataType) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: 0,
                column_name: name.to_string(),
            },
            data_type: dt.clone(),
        }
    }

    fn simple_constant(v: crate::types::Value, dt: DataType) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::Constant(v),
            data_type: dt,
        }
    }

    fn simple_projection(name: &str, dt: DataType) -> AnalyzedProjection {
        AnalyzedProjection {
            expr: simple_column(name, dt),
            output_name: name.to_string(),
        }
    }

    /// Single-table SELECT: SELECT id, name FROM users WHERE id = 1
    #[test]
    fn test_single_table_select() {
        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![
                    simple_projection("id", DataType::Int64),
                    simple_projection("name", DataType::Text),
                ],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "users".to_string(),
                        schema: TableRefSchema {
                            table_id: 1,
                            columns: vec![
                                ("id".to_string(), DataType::Int64, false),
                                ("name".to_string(), DataType::Text, true),
                            ],
                        },
                    },
                    alias: None,
                }],
                where_clause: Some(TypedExpr {
                    kind: TypedExprKind::BinaryOp {
                        left: Box::new(simple_column("id", DataType::Int64)),
                        op: BinaryOp::Eq,
                        right: Box::new(simple_constant(
                            crate::types::Value::Int64(1),
                            DataType::Int64,
                        )),
                    },
                    data_type: DataType::Boolean,
                }),
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![
                ("id".to_string(), DataType::Int64),
                ("name".to_string(), DataType::Text),
            ],
        };

        let plan = LogicalPlanner::build(&query);

        // Should be: Project → Filter → Scan
        assert!(matches!(plan.node, LogicalNode::Project { .. }));
        if let LogicalNode::Project { input, .. } = &plan.node {
            assert!(matches!(input.node, LogicalNode::Filter { .. }));
            if let LogicalNode::Filter { input, .. } = &input.node {
                assert!(matches!(input.node, LogicalNode::Scan { .. }));
            }
        }
        assert_eq!(plan.schema.num_columns(), 2);
    }

    /// Tableless query: SELECT 1
    #[test]
    fn test_tableless_select() {
        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: simple_constant(crate::types::Value::Int32(1), DataType::Int32),
                    output_name: "?column?".to_string(),
                }],
                from: vec![],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("?column?".to_string(), DataType::Int32)],
        };

        let plan = LogicalPlanner::build(&query);

        // Should be: Project → Empty
        assert!(matches!(plan.node, LogicalNode::Project { .. }));
        if let LogicalNode::Project { input, .. } = &plan.node {
            assert!(matches!(input.node, LogicalNode::Empty));
        }
    }

    /// Query with ORDER BY and LIMIT: SELECT * FROM t ORDER BY id LIMIT 10
    #[test]
    fn test_order_by_limit() {
        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![simple_projection("id", DataType::Int64)],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "t".to_string(),
                        schema: TableRefSchema {
                            table_id: 1,
                            columns: vec![("id".to_string(), DataType::Int64, false)],
                        },
                    },
                    alias: None,
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![TypedOrderByExpr {
                expr: simple_column("id", DataType::Int64),
                asc: true,
                nulls_first: false,
            }],
            limit: Some(simple_constant(
                crate::types::Value::Int64(10),
                DataType::Int64,
            )),
            offset: None,
            output_schema: vec![("id".to_string(), DataType::Int64)],
        };

        let plan = LogicalPlanner::build(&query);

        // Should be: Limit → Project → Sort → Scan
        assert!(matches!(plan.node, LogicalNode::Limit { .. }));
        if let LogicalNode::Limit { input, .. } = &plan.node {
            assert!(
                matches!(input.node, LogicalNode::Project { .. }),
                "expected Project, got {:?}",
                std::mem::discriminant(&input.node)
            );
            if let LogicalNode::Project { input, .. } = &input.node {
                assert!(
                    matches!(input.node, LogicalNode::Sort { .. }),
                    "expected Sort, got {:?}",
                    std::mem::discriminant(&input.node)
                );
            }
        }
    }

    /// Set operation: SELECT id FROM a UNION SELECT id FROM b
    #[test]
    fn test_set_operation() {
        let left = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![simple_projection("id", DataType::Int64)],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "a".to_string(),
                        schema: TableRefSchema {
                            table_id: 1,
                            columns: vec![("id".to_string(), DataType::Int64, false)],
                        },
                    },
                    alias: None,
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("id".to_string(), DataType::Int64)],
        };
        let right = left.clone();

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::SetOperation {
                op: SetOpKind::Union,
                all: false,
                left: Box::new(left),
                right: Box::new(right),
            },
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("id".to_string(), DataType::Int64)],
        };

        let plan = LogicalPlanner::build(&query);
        assert!(matches!(plan.node, LogicalNode::SetOperation { .. }));
    }

    /// Query with GROUP BY: SELECT status, count(*) FROM orders GROUP BY status
    #[test]
    fn test_group_by() {
        let count_agg = TypedExpr {
            kind: TypedExprKind::AggregateCall {
                func: ResolvedFunction {
                    name: "count".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![],
                distinct: false,
                filter: None,
                order_by: vec![],
            },
            data_type: DataType::Int64,
        };

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![
                    simple_projection("status", DataType::Text),
                    AnalyzedProjection {
                        expr: count_agg,
                        output_name: "count".to_string(),
                    },
                ],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "orders".to_string(),
                        schema: TableRefSchema {
                            table_id: 1,
                            columns: vec![
                                ("id".to_string(), DataType::Int64, false),
                                ("status".to_string(), DataType::Text, false),
                            ],
                        },
                    },
                    alias: None,
                }],
                where_clause: None,
                group_by: vec![simple_column("status", DataType::Text)],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![
                ("status".to_string(), DataType::Text),
                ("count".to_string(), DataType::Int64),
            ],
        };

        let plan = LogicalPlanner::build(&query);

        // Should be: Aggregate → Scan (no separate Project when GROUP BY is present)
        assert!(matches!(plan.node, LogicalNode::Aggregate { .. }));
        if let LogicalNode::Aggregate { input, .. } = &plan.node {
            assert!(matches!(input.node, LogicalNode::Scan { .. }));
        }
    }

    /// DISTINCT query
    #[test]
    fn test_distinct() {
        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![simple_projection("name", DataType::Text)],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "t".to_string(),
                        schema: TableRefSchema {
                            table_id: 1,
                            columns: vec![("name".to_string(), DataType::Text, true)],
                        },
                    },
                    alias: None,
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::Distinct,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("name".to_string(), DataType::Text)],
        };

        let plan = LogicalPlanner::build(&query);

        // Should be: Distinct → Project → Scan
        assert!(matches!(plan.node, LogicalNode::Distinct { .. }));
    }

    /// Aggregate + ORDER BY: rewrite ORDER BY to post-aggregate indices
    #[test]
    fn test_aggregate_order_by_rewrite() {
        let count_agg = TypedExpr {
            kind: TypedExprKind::AggregateCall {
                func: ResolvedFunction {
                    name: "count".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![],
                distinct: false,
                filter: None,
                order_by: vec![],
            },
            data_type: DataType::Int64,
        };

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![
                    simple_projection("status", DataType::Text),
                    AnalyzedProjection {
                        expr: count_agg.clone(),
                        output_name: "count".to_string(),
                    },
                ],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "orders".to_string(),
                        schema: TableRefSchema {
                            table_id: 1,
                            columns: vec![
                                ("id".to_string(), DataType::Int64, false),
                                ("status".to_string(), DataType::Text, false),
                            ],
                        },
                    },
                    alias: None,
                }],
                where_clause: None,
                group_by: vec![simple_column("status", DataType::Text)],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            // ORDER BY count(*) DESC — uses scan-scope aggregate expr
            order_by: vec![TypedOrderByExpr {
                expr: count_agg,
                asc: false,
                nulls_first: false,
            }],
            limit: None,
            offset: None,
            output_schema: vec![
                ("status".to_string(), DataType::Text),
                ("count".to_string(), DataType::Int64),
            ],
        };

        let plan = LogicalPlanner::build(&query);

        // Should be: Sort(rewritten) → Aggregate → Scan
        assert!(matches!(plan.node, LogicalNode::Sort { .. }));
        if let LogicalNode::Sort {
            order_by, input, ..
        } = &plan.node
        {
            assert_eq!(order_by.len(), 1);
            // The rewritten ORDER BY should be a ColumnRef to post-aggregate index 1
            // (group_by_count=1, agg_index=0 → 1+0 = 1)
            if let TypedExprKind::ColumnRef { column_index, .. } = &order_by[0].expr.kind {
                assert_eq!(*column_index, 1);
            } else {
                panic!("expected ColumnRef in rewritten ORDER BY");
            }
            assert!(matches!(input.node, LogicalNode::Aggregate { .. }));
        }
    }

    /// Aggregate + HAVING: rewrite HAVING to post-aggregate indices
    #[test]
    fn test_aggregate_having_rewrite() {
        let count_agg = TypedExpr {
            kind: TypedExprKind::AggregateCall {
                func: ResolvedFunction {
                    name: "count".to_string(),
                    kind: FunctionKind::Builtin,
                    return_type: DataType::Int64,
                },
                args: vec![],
                distinct: false,
                filter: None,
                order_by: vec![],
            },
            data_type: DataType::Int64,
        };

        let having_expr = TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(count_agg.clone()),
                op: BinaryOp::Gt,
                right: Box::new(simple_constant(
                    crate::types::Value::Int64(5),
                    DataType::Int64,
                )),
            },
            data_type: DataType::Boolean,
        };

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![
                    simple_projection("status", DataType::Text),
                    AnalyzedProjection {
                        expr: count_agg,
                        output_name: "count".to_string(),
                    },
                ],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "orders".to_string(),
                        schema: TableRefSchema {
                            table_id: 1,
                            columns: vec![
                                ("id".to_string(), DataType::Int64, false),
                                ("status".to_string(), DataType::Text, false),
                            ],
                        },
                    },
                    alias: None,
                }],
                where_clause: None,
                group_by: vec![simple_column("status", DataType::Text)],
                having: Some(having_expr),
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![
                ("status".to_string(), DataType::Text),
                ("count".to_string(), DataType::Int64),
            ],
        };

        let plan = LogicalPlanner::build(&query);

        // Should be: Filter(rewritten HAVING) → Aggregate → Scan
        assert!(matches!(plan.node, LogicalNode::Filter { .. }));
        if let LogicalNode::Filter {
            predicate, input, ..
        } = &plan.node
        {
            // HAVING predicate should be rewritten: COUNT(*) → ColumnRef(1)
            if let TypedExprKind::BinaryOp { left, .. } = &predicate.kind {
                if let TypedExprKind::ColumnRef { column_index, .. } = &left.kind {
                    assert_eq!(*column_index, 1);
                } else {
                    panic!("expected ColumnRef in rewritten HAVING left side");
                }
            } else {
                panic!("expected BinaryOp in rewritten HAVING");
            }
            assert!(matches!(input.node, LogicalNode::Aggregate { .. }));
        }
    }
}
