//! Logical plan rewrite framework + predicate pushdown.
//!
//! Rewrites transform a `LogicalPlan` into a semantically equivalent plan
//! with better expected performance. Each rule is applied once in sequence.
//!
//! Currently implemented:
//! - **PredicatePushdown**: pushes WHERE filter predicates below Join and Sort
//!   nodes to reduce the number of rows entering those operators.

use std::collections::HashSet;

use super::logical_plan::{LogicalNode, LogicalPlan, PlanSchema};
use crate::sql::analyzer::types::{
    reindex_typed_expr, BinaryOp, JoinCondition, JoinType, TypedExpr, TypedExprKind,
};
use crate::sql::expr::classify::{has_correlated_ref, has_unresolved_subquery, is_volatile};
use crate::types::DataType;

// ── Rewrite framework ──────────────────────────────────────────

/// A rewrite rule that transforms a logical plan.
trait LogicalRewriteRule {
    fn rewrite(&self, plan: LogicalPlan) -> LogicalPlan;
}

/// Apply all rewrite rules to a logical plan.
///
/// Called from `optimize()` between logical planning and physical planning.
pub fn apply_rewrites(plan: LogicalPlan) -> LogicalPlan {
    let rules: Vec<Box<dyn LogicalRewriteRule>> = vec![Box::new(PredicatePushdown)];
    let mut current = plan;
    for rule in &rules {
        current = rule.rewrite(current);
    }
    current
}

// ── Predicate pushdown ─────────────────────────────────────────

struct PredicatePushdown;

impl LogicalRewriteRule for PredicatePushdown {
    fn rewrite(&self, plan: LogicalPlan) -> LogicalPlan {
        rewrite_plan(plan)
    }
}

/// Bottom-up recursive rewrite: rewrite children first, then handle current node.
fn rewrite_plan(plan: LogicalPlan) -> LogicalPlan {
    // First, recursively rewrite children
    let plan = rewrite_children(plan);

    // Then, if current node is a Filter, try to push it down
    match plan.node {
        LogicalNode::Filter { predicate, input } => push_filter_down(predicate, *input),
        _ => plan,
    }
}

/// Recursively rewrite all children of a plan node.
fn rewrite_children(plan: LogicalPlan) -> LogicalPlan {
    let schema = plan.schema;
    let node = match plan.node {
        LogicalNode::Filter { predicate, input } => LogicalNode::Filter {
            predicate,
            input: Box::new(rewrite_plan(*input)),
        },
        LogicalNode::Project { projections, input } => LogicalNode::Project {
            projections,
            input: Box::new(rewrite_plan(*input)),
        },
        LogicalNode::Aggregate {
            group_by,
            projections,
            input,
        } => LogicalNode::Aggregate {
            group_by,
            projections,
            input: Box::new(rewrite_plan(*input)),
        },
        LogicalNode::Sort { order_by, input } => LogicalNode::Sort {
            order_by,
            input: Box::new(rewrite_plan(*input)),
        },
        LogicalNode::Limit {
            limit,
            offset,
            input,
        } => LogicalNode::Limit {
            limit,
            offset,
            input: Box::new(rewrite_plan(*input)),
        },
        LogicalNode::Distinct { input } => LogicalNode::Distinct {
            input: Box::new(rewrite_plan(*input)),
        },
        LogicalNode::DistinctOn { on_exprs, input } => LogicalNode::DistinctOn {
            on_exprs,
            input: Box::new(rewrite_plan(*input)),
        },
        LogicalNode::Window { input } => LogicalNode::Window {
            input: Box::new(rewrite_plan(*input)),
        },
        LogicalNode::Join {
            left,
            right,
            join_type,
            condition,
        } => LogicalNode::Join {
            left: Box::new(rewrite_plan(*left)),
            right: Box::new(rewrite_plan(*right)),
            join_type,
            condition,
        },
        LogicalNode::SetOperation {
            op,
            all,
            left,
            right,
        } => LogicalNode::SetOperation {
            op,
            all,
            left: Box::new(rewrite_plan(*left)),
            right: Box::new(rewrite_plan(*right)),
        },
        LogicalNode::Subquery { subplan, alias } => LogicalNode::Subquery {
            subplan: Box::new(rewrite_plan(*subplan)),
            alias,
        },
        // Leaf nodes — no children to rewrite
        node @ (LogicalNode::Scan { .. }
        | LogicalNode::Values { .. }
        | LogicalNode::TableFunction { .. }
        | LogicalNode::Empty) => node,
    };
    LogicalPlan { node, schema }
}

/// Try to push a filter predicate below the given plan node.
///
/// Returns the optimized plan — either with the filter pushed into children
/// or kept above as a Filter node.
fn push_filter_down(predicate: TypedExpr, input: LogicalPlan) -> LogicalPlan {
    match input.node {
        // Filter over Filter → merge predicates, recurse
        LogicalNode::Filter {
            predicate: inner_pred,
            input: inner_input,
        } => {
            let merged = and_expr(predicate, inner_pred);
            push_filter_down(merged, *inner_input)
        }

        // Filter over Join → classify and push conjuncts
        LogicalNode::Join {
            left,
            right,
            join_type,
            condition,
        } => push_filter_through_join(predicate, *left, *right, join_type, condition, input.schema),

        // Filter over Sort → push through (sort preserves rows)
        LogicalNode::Sort {
            order_by,
            input: sort_input,
        } => {
            let pushed = push_filter_down(predicate, *sort_input);
            let schema = pushed.schema.clone();
            LogicalPlan {
                node: LogicalNode::Sort {
                    order_by,
                    input: Box::new(pushed),
                },
                schema,
            }
        }

        // Barriers — never push through these
        LogicalNode::Project { .. }
        | LogicalNode::Aggregate { .. }
        | LogicalNode::Limit { .. }
        | LogicalNode::Distinct { .. }
        | LogicalNode::DistinctOn { .. }
        | LogicalNode::Window { .. }
        | LogicalNode::Scan { .. }
        | LogicalNode::Values { .. }
        | LogicalNode::TableFunction { .. }
        | LogicalNode::Empty
        | LogicalNode::SetOperation { .. }
        | LogicalNode::Subquery { .. } => {
            let schema = input.schema.clone();
            LogicalPlan {
                node: LogicalNode::Filter {
                    predicate,
                    input: Box::new(input),
                },
                schema,
            }
        }
    }
}

/// Push filter conjuncts through a Join node based on join type and column references.
fn push_filter_through_join(
    predicate: TypedExpr,
    left: LogicalPlan,
    right: LogicalPlan,
    join_type: JoinType,
    condition: JoinCondition,
    join_schema: PlanSchema,
) -> LogicalPlan {
    let left_width = left.schema.columns.len();
    let conjuncts = split_conjunction(predicate);

    let mut left_pushable = Vec::new();
    let mut right_pushable = Vec::new();
    let mut remaining = Vec::new();

    for conj in conjuncts {
        // Never push volatile, correlated, or subquery-containing predicates
        if is_volatile(&conj) || has_correlated_ref(&conj) || has_unresolved_subquery(&conj) {
            remaining.push(conj);
            continue;
        }

        let indices = collect_column_indices(&conj);

        // No column refs (e.g. constant true) — can push to either side
        if indices.is_empty() {
            // Push to left side by default for constant predicates
            match join_type {
                JoinType::Inner | JoinType::Cross => left_pushable.push(conj),
                JoinType::Left => left_pushable.push(conj),
                JoinType::Right => right_pushable.push(conj),
                JoinType::Full => remaining.push(conj),
            }
            continue;
        }

        let all_left = indices.iter().all(|&i| i < left_width);
        let all_right = !indices.is_empty() && indices.iter().all(|&i| i >= left_width);

        match join_type {
            JoinType::Inner | JoinType::Cross => {
                if all_left {
                    left_pushable.push(conj);
                } else if all_right {
                    right_pushable.push(conj);
                } else {
                    remaining.push(conj);
                }
            }
            JoinType::Left => {
                if all_left {
                    left_pushable.push(conj);
                } else {
                    // Right-only and cross-table stay above (nullable side)
                    remaining.push(conj);
                }
            }
            JoinType::Right => {
                if all_right {
                    right_pushable.push(conj);
                } else {
                    // Left-only and cross-table stay above (nullable side)
                    remaining.push(conj);
                }
            }
            JoinType::Full => {
                // Both sides are nullable — nothing can be pushed
                remaining.push(conj);
            }
        }
    }

    // Recursively push into children (handles nested joins in a single pass)
    let new_left = if left_pushable.is_empty() {
        left
    } else {
        let pred = conjuncts_to_predicate(left_pushable);
        push_filter_down(pred, left)
    };

    let new_right = if right_pushable.is_empty() {
        right
    } else {
        // Reindex right-side predicates: subtract left_width from column indices
        let reindexed: Vec<TypedExpr> = right_pushable
            .into_iter()
            .map(|e| reindex_typed_expr(&e, left_width))
            .collect();
        let pred = conjuncts_to_predicate(reindexed);
        push_filter_down(pred, right)
    };

    // Rebuild the join
    let mut result = LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(new_left),
            right: Box::new(new_right),
            join_type,
            condition,
        },
        schema: join_schema,
    };

    // Wrap in Filter if there are remaining predicates
    if !remaining.is_empty() {
        let pred = conjuncts_to_predicate(remaining);
        let schema = result.schema.clone();
        result = LogicalPlan {
            node: LogicalNode::Filter {
                predicate: pred,
                input: Box::new(result),
            },
            schema,
        };
    }

    result
}

// ── Helpers (private to this module) ───────────────────────────

/// Split an expression on AND into a flat list of conjuncts.
fn split_conjunction(expr: TypedExpr) -> Vec<TypedExpr> {
    match expr.kind {
        TypedExprKind::BinaryOp {
            left,
            op: BinaryOp::And,
            right,
        } => {
            let mut result = split_conjunction(*left);
            result.extend(split_conjunction(*right));
            result
        }
        _ => vec![expr],
    }
}

/// Recombine a list of conjuncts into a single AND expression.
///
/// Panics if the list is empty.
fn conjuncts_to_predicate(mut conjuncts: Vec<TypedExpr>) -> TypedExpr {
    assert!(!conjuncts.is_empty(), "conjuncts must be non-empty");
    let mut result = conjuncts.pop().unwrap();
    while let Some(next) = conjuncts.pop() {
        result = and_expr(next, result);
    }
    result
}

/// Create an AND binary op node.
fn and_expr(left: TypedExpr, right: TypedExpr) -> TypedExpr {
    TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(left),
            op: BinaryOp::And,
            right: Box::new(right),
        },
        data_type: DataType::Boolean,
    }
}

/// Collect all column indices referenced by an expression (scope_depth == 0 only).
///
/// Uses explicit recursive walk (not `expr_any`) because `expr_any` takes `&Fn`
/// which doesn't support the mutable `HashSet` capture needed for collection.
fn collect_column_indices(expr: &TypedExpr) -> HashSet<usize> {
    let mut indices = HashSet::new();
    collect_column_indices_inner(expr, &mut indices);
    indices
}

fn collect_column_indices_inner(expr: &TypedExpr, indices: &mut HashSet<usize>) {
    match &expr.kind {
        TypedExprKind::ColumnRef {
            scope_depth,
            column_index,
            ..
        } => {
            if *scope_depth == 0 {
                indices.insert(*column_index);
            }
        }
        TypedExprKind::Constant(_) | TypedExprKind::Default => {}
        TypedExprKind::BinaryOp { left, right, .. } => {
            collect_column_indices_inner(left, indices);
            collect_column_indices_inner(right, indices);
        }
        TypedExprKind::UnaryOp { operand, .. } => {
            collect_column_indices_inner(operand, indices);
        }
        TypedExprKind::Cast { expr, .. } | TypedExprKind::IsTest { expr, .. } => {
            collect_column_indices_inner(expr, indices);
        }
        TypedExprKind::Between {
            expr, low, high, ..
        } => {
            collect_column_indices_inner(expr, indices);
            collect_column_indices_inner(low, indices);
            collect_column_indices_inner(high, indices);
        }
        TypedExprKind::InList { expr, list, .. } => {
            collect_column_indices_inner(expr, indices);
            for item in list {
                collect_column_indices_inner(item, indices);
            }
        }
        TypedExprKind::Like {
            expr,
            pattern,
            escape,
            ..
        }
        | TypedExprKind::SimilarTo {
            expr,
            pattern,
            escape,
            ..
        } => {
            collect_column_indices_inner(expr, indices);
            collect_column_indices_inner(pattern, indices);
            if let Some(esc) = escape {
                collect_column_indices_inner(esc, indices);
            }
        }
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => {
            if let Some(op) = operand {
                collect_column_indices_inner(op, indices);
            }
            for (w, t) in when_clauses {
                collect_column_indices_inner(w, indices);
                collect_column_indices_inner(t, indices);
            }
            if let Some(e) = else_result {
                collect_column_indices_inner(e, indices);
            }
        }
        TypedExprKind::Coalesce(args)
        | TypedExprKind::MinMax { args, .. }
        | TypedExprKind::ArrayLiteral(args)
        | TypedExprKind::Row(args) => {
            for arg in args {
                collect_column_indices_inner(arg, indices);
            }
        }
        TypedExprKind::NullIf(a, b) => {
            collect_column_indices_inner(a, indices);
            collect_column_indices_inner(b, indices);
        }
        TypedExprKind::FunctionCall { args, filter, .. } => {
            for arg in args {
                collect_column_indices_inner(arg, indices);
            }
            if let Some(f) = filter {
                collect_column_indices_inner(f, indices);
            }
        }
        TypedExprKind::AggregateCall { args, filter, .. } => {
            for arg in args {
                collect_column_indices_inner(arg, indices);
            }
            if let Some(f) = filter {
                collect_column_indices_inner(f, indices);
            }
        }
        TypedExprKind::WindowCall {
            args,
            partition_by,
            order_by,
            ..
        } => {
            for arg in args {
                collect_column_indices_inner(arg, indices);
            }
            for pb in partition_by {
                collect_column_indices_inner(pb, indices);
            }
            for ob in order_by {
                collect_column_indices_inner(&ob.expr, indices);
            }
        }
        TypedExprKind::ArrayIndex { array, index } => {
            collect_column_indices_inner(array, indices);
            collect_column_indices_inner(index, indices);
        }
        TypedExprKind::JsonAccess { expr, path, .. } => {
            collect_column_indices_inner(expr, indices);
            collect_column_indices_inner(path, indices);
        }
        // Subquery nodes — don't descend into subqueries (different scope)
        TypedExprKind::ScalarSubquery(_)
        | TypedExprKind::Exists { .. }
        | TypedExprKind::InSubquery { .. }
        | TypedExprKind::AnyAll { .. }
        | TypedExprKind::ArraySubquery(_) => {}
    }
}

// ── Unit tests ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::analyzer::types::{FunctionKind, ResolvedFunction};
    use crate::sql::expr::typed_fold::{fold_typed_expr, is_volatile_or_side_effecting_builtin};
    use crate::sql::expr::typed_visit::expr_any;
    use crate::sql::query_context::QueryContext;
    use crate::types::Value;
    use std::sync::Arc;

    // ── Test helpers ───────────────────────────────────────

    fn col_ref(index: usize, name: &str) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: index,
                column_name: name.to_string(),
            },
            data_type: DataType::Int64,
        }
    }

    fn correlated_col_ref(index: usize, name: &str) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 1,
                column_index: index,
                column_name: name.to_string(),
            },
            data_type: DataType::Int64,
        }
    }

    fn const_int(v: i64) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::Constant(Value::Int64(v)),
            data_type: DataType::Int64,
        }
    }

    fn const_bool(v: bool) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::Constant(Value::Boolean(v)),
            data_type: DataType::Boolean,
        }
    }

    fn eq_expr(left: TypedExpr, right: TypedExpr) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(left),
                op: BinaryOp::Eq,
                right: Box::new(right),
            },
            data_type: DataType::Boolean,
        }
    }

    fn gt_expr(left: TypedExpr, right: TypedExpr) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(left),
                op: BinaryOp::Gt,
                right: Box::new(right),
            },
            data_type: DataType::Boolean,
        }
    }

    fn make_scan(name: &str, cols: Vec<(&str, DataType)>) -> LogicalPlan {
        let columns: Vec<(String, DataType)> = cols
            .into_iter()
            .map(|(n, dt)| (n.to_string(), dt))
            .collect();
        LogicalPlan::scan(name.to_string(), None, PlanSchema::from_columns(columns))
    }

    fn make_inner_join(left: LogicalPlan, right: LogicalPlan) -> LogicalPlan {
        let mut combined = left.schema.columns.clone();
        combined.extend(right.schema.columns.clone());
        LogicalPlan {
            node: LogicalNode::Join {
                left: Box::new(left),
                right: Box::new(right),
                join_type: JoinType::Inner,
                condition: JoinCondition::None,
            },
            schema: PlanSchema::from_columns(combined),
        }
    }

    fn make_left_join(left: LogicalPlan, right: LogicalPlan) -> LogicalPlan {
        let mut combined = left.schema.columns.clone();
        combined.extend(right.schema.columns.clone());
        LogicalPlan {
            node: LogicalNode::Join {
                left: Box::new(left),
                right: Box::new(right),
                join_type: JoinType::Left,
                condition: JoinCondition::None,
            },
            schema: PlanSchema::from_columns(combined),
        }
    }

    fn make_right_join(left: LogicalPlan, right: LogicalPlan) -> LogicalPlan {
        let mut combined = left.schema.columns.clone();
        combined.extend(right.schema.columns.clone());
        LogicalPlan {
            node: LogicalNode::Join {
                left: Box::new(left),
                right: Box::new(right),
                join_type: JoinType::Right,
                condition: JoinCondition::None,
            },
            schema: PlanSchema::from_columns(combined),
        }
    }

    fn make_full_join(left: LogicalPlan, right: LogicalPlan) -> LogicalPlan {
        let mut combined = left.schema.columns.clone();
        combined.extend(right.schema.columns.clone());
        LogicalPlan {
            node: LogicalNode::Join {
                left: Box::new(left),
                right: Box::new(right),
                join_type: JoinType::Full,
                condition: JoinCondition::None,
            },
            schema: PlanSchema::from_columns(combined),
        }
    }

    fn make_cross_join(left: LogicalPlan, right: LogicalPlan) -> LogicalPlan {
        let mut combined = left.schema.columns.clone();
        combined.extend(right.schema.columns.clone());
        LogicalPlan {
            node: LogicalNode::Join {
                left: Box::new(left),
                right: Box::new(right),
                join_type: JoinType::Cross,
                condition: JoinCondition::None,
            },
            schema: PlanSchema::from_columns(combined),
        }
    }

    fn test_qctx() -> QueryContext {
        QueryContext::new(
            7,
            Arc::from("db703"),
            Arc::from("postgres"),
            1_700_000_000_000,
            1_700_000_000_000,
            Arc::from("UTC"),
        )
    }

    /// Bare FunctionCall node (not wrapped in `> 0`) for direct fold testing.
    fn make_bare_function_call(name: &str, kind: FunctionKind) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: name.to_string(),
                    kind,
                    return_type: DataType::Float64,
                },
                args: vec![],
                order_by: vec![],
                filter: None,
            },
            data_type: DataType::Float64,
        }
    }

    fn make_function_call(name: &str, kind: FunctionKind) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::FunctionCall {
                func: ResolvedFunction {
                    name: name.to_string(),
                    kind,
                    return_type: DataType::Float64,
                },
                args: vec![],
                order_by: vec![],
                filter: None,
            },
            data_type: DataType::Float64,
        }
    }

    fn make_volatile_predicate(name: &str) -> TypedExpr {
        gt_expr(
            make_function_call(name, FunctionKind::Builtin),
            const_int(0),
        )
    }

    fn make_stable_predicate(name: &str) -> TypedExpr {
        gt_expr(
            make_function_call(name, FunctionKind::Builtin),
            const_int(0),
        )
    }

    fn left_scan() -> LogicalPlan {
        make_scan(
            "left_t",
            vec![("a", DataType::Int64), ("b", DataType::Int64)],
        )
    }

    fn right_scan() -> LogicalPlan {
        make_scan(
            "right_t",
            vec![("c", DataType::Int64), ("d", DataType::Int64)],
        )
    }

    /// Returns true if the top-level node is a Filter.
    fn is_filter(plan: &LogicalPlan) -> bool {
        matches!(plan.node, LogicalNode::Filter { .. })
    }

    /// Returns true if the top-level node is a Join.
    fn is_join(plan: &LogicalPlan) -> bool {
        matches!(plan.node, LogicalNode::Join { .. })
    }

    /// Unwrap the left child of a Join node.
    fn join_left(plan: &LogicalPlan) -> &LogicalPlan {
        match &plan.node {
            LogicalNode::Join { left, .. } => left,
            _ => panic!("expected Join node"),
        }
    }

    /// Unwrap the right child of a Join node.
    fn join_right(plan: &LogicalPlan) -> &LogicalPlan {
        match &plan.node {
            LogicalNode::Join { right, .. } => right,
            _ => panic!("expected Join node"),
        }
    }

    /// Unwrap the input of a Filter node.
    fn filter_input(plan: &LogicalPlan) -> &LogicalPlan {
        match &plan.node {
            LogicalNode::Filter { input, .. } => input,
            _ => panic!("expected Filter node"),
        }
    }

    // ── Tests ──────────────────────────────────────────────

    // Test 1: Filter over Scan stays in place (no join to push through)
    #[test]
    fn test_filter_over_scan_stays() {
        let scan = make_scan("t", vec![("x", DataType::Int64)]);
        let plan = scan.filter(eq_expr(col_ref(0, "x"), const_int(1)));
        let result = rewrite_plan(plan);
        assert!(is_filter(&result), "Filter over Scan should stay");
        assert!(matches!(
            filter_input(&result).node,
            LogicalNode::Scan { .. }
        ));
    }

    // Test 2: Left-only predicate pushed through INNER join
    #[test]
    fn test_left_only_through_inner_join() {
        let join = make_inner_join(left_scan(), right_scan());
        // a = 1 → col_index 0 (left side)
        let plan = join.filter(eq_expr(col_ref(0, "a"), const_int(1)));
        let result = rewrite_plan(plan);

        // Should be: Join(Filter(left_scan), right_scan)
        assert!(is_join(&result), "Top should be Join, not Filter");
        assert!(is_filter(join_left(&result)), "Left child should be Filter");
        assert!(
            !is_filter(join_right(&result)),
            "Right child should not be Filter"
        );
    }

    // Test 3: Right-only predicate pushed through INNER join (with reindexing)
    #[test]
    fn test_right_only_through_inner_join() {
        let join = make_inner_join(left_scan(), right_scan());
        // c = 1 → col_index 2 (right side, left_width=2)
        let plan = join.filter(eq_expr(col_ref(2, "c"), const_int(1)));
        let result = rewrite_plan(plan);

        // Should be: Join(left_scan, Filter(right_scan))
        assert!(is_join(&result), "Top should be Join");
        assert!(!is_filter(join_left(&result)), "Left should not be Filter");
        assert!(is_filter(join_right(&result)), "Right should be Filter");
    }

    // Test 4: Both sides pushed through INNER join
    #[test]
    fn test_both_sides_through_inner_join() {
        let join = make_inner_join(left_scan(), right_scan());
        // a = 1 AND c = 2
        let pred = and_expr(
            eq_expr(col_ref(0, "a"), const_int(1)),
            eq_expr(col_ref(2, "c"), const_int(2)),
        );
        let plan = join.filter(pred);
        let result = rewrite_plan(plan);

        assert!(is_join(&result), "Top should be Join");
        assert!(is_filter(join_left(&result)), "Left should be Filter");
        assert!(is_filter(join_right(&result)), "Right should be Filter");
    }

    // Test 5: Cross-table predicate stays above INNER join
    #[test]
    fn test_cross_table_stays_above_inner() {
        let join = make_inner_join(left_scan(), right_scan());
        // a = c → col_index 0 and 2 (crosses both sides)
        let plan = join.filter(eq_expr(col_ref(0, "a"), col_ref(2, "c")));
        let result = rewrite_plan(plan);

        assert!(is_filter(&result), "Cross-table should stay above join");
        assert!(is_join(filter_input(&result)));
    }

    // Test 6: Mixed conjuncts — partial push
    #[test]
    fn test_mixed_conjuncts_partial_push() {
        let join = make_inner_join(left_scan(), right_scan());
        // a = 1 AND a = c AND c = 2
        let pred = and_expr(
            eq_expr(col_ref(0, "a"), const_int(1)),
            and_expr(
                eq_expr(col_ref(0, "a"), col_ref(2, "c")),
                eq_expr(col_ref(2, "c"), const_int(2)),
            ),
        );
        let plan = join.filter(pred);
        let result = rewrite_plan(plan);

        // Top should be Filter (cross-table stays), with Join below
        assert!(is_filter(&result), "Cross-table predicate stays above");
        let join_node = filter_input(&result);
        assert!(is_join(join_node), "Join below remaining filter");
        assert!(is_filter(join_left(join_node)), "Left-only pushed to left");
        assert!(
            is_filter(join_right(join_node)),
            "Right-only pushed to right"
        );
    }

    // Test 7: Left-only through LEFT join — pushed
    #[test]
    fn test_left_only_through_left_join() {
        let join = make_left_join(left_scan(), right_scan());
        let plan = join.filter(eq_expr(col_ref(0, "a"), const_int(1)));
        let result = rewrite_plan(plan);

        assert!(is_join(&result), "Top should be Join");
        assert!(
            is_filter(join_left(&result)),
            "Left-only pushed to left child"
        );
    }

    // Test 8: Right-only on LEFT join — stays above
    #[test]
    fn test_right_only_on_left_join_stays() {
        let join = make_left_join(left_scan(), right_scan());
        // c = 1 → right side
        let plan = join.filter(eq_expr(col_ref(2, "c"), const_int(1)));
        let result = rewrite_plan(plan);

        assert!(is_filter(&result), "Right-only on LEFT join stays above");
        assert!(is_join(filter_input(&result)));
    }

    // Test 9: Right-only through RIGHT join — pushed
    #[test]
    fn test_right_only_through_right_join() {
        let join = make_right_join(left_scan(), right_scan());
        // c = 1 → col_index 2 (right side)
        let plan = join.filter(eq_expr(col_ref(2, "c"), const_int(1)));
        let result = rewrite_plan(plan);

        assert!(is_join(&result), "Top should be Join");
        assert!(
            is_filter(join_right(&result)),
            "Right-only pushed to right child"
        );
    }

    // Test 10: Left-only on RIGHT join — stays above
    #[test]
    fn test_left_only_on_right_join_stays() {
        let join = make_right_join(left_scan(), right_scan());
        let plan = join.filter(eq_expr(col_ref(0, "a"), const_int(1)));
        let result = rewrite_plan(plan);

        assert!(is_filter(&result), "Left-only on RIGHT join stays above");
        assert!(is_join(filter_input(&result)));
    }

    // Test 11: Any predicate on FULL join — stays above
    #[test]
    fn test_any_on_full_join_stays() {
        let join = make_full_join(left_scan(), right_scan());
        let plan = join.filter(eq_expr(col_ref(0, "a"), const_int(1)));
        let result = rewrite_plan(plan);

        assert!(is_filter(&result), "Any predicate on FULL join stays above");
        assert!(is_join(filter_input(&result)));
    }

    // Test 12: Volatile predicate not pushed
    #[test]
    fn test_volatile_not_pushed() {
        let join = make_inner_join(left_scan(), right_scan());
        // RANDOM() > 0 — volatile, should stay above
        let plan = join.filter(make_volatile_predicate("RANDOM"));
        let result = rewrite_plan(plan);

        assert!(is_filter(&result), "Volatile predicate stays above join");
    }

    // Test 13: Correlated predicate not pushed
    #[test]
    fn test_correlated_not_pushed() {
        let join = make_inner_join(left_scan(), right_scan());
        let pred = eq_expr(correlated_col_ref(0, "outer_col"), const_int(1));
        let plan = join.filter(pred);
        let result = rewrite_plan(plan);

        assert!(is_filter(&result), "Correlated predicate stays above join");
    }

    // Test 14: Filter merge (Filter over Filter)
    #[test]
    fn test_filter_merge() {
        let scan = make_scan("t", vec![("x", DataType::Int64)]);
        let plan = scan
            .filter(eq_expr(col_ref(0, "x"), const_int(1)))
            .filter(gt_expr(col_ref(0, "x"), const_int(0)));
        let result = rewrite_plan(plan);

        // Should merge into a single Filter with AND
        assert!(is_filter(&result));
        if let LogicalNode::Filter {
            ref predicate,
            ref input,
        } = result.node
        {
            // Merged predicate should be an AND
            assert!(
                matches!(
                    predicate.kind,
                    TypedExprKind::BinaryOp {
                        op: BinaryOp::And,
                        ..
                    }
                ),
                "Merged predicate should be AND"
            );
            // Input should be Scan (not another Filter)
            assert!(matches!(input.node, LogicalNode::Scan { .. }));
        }
    }

    // Test 15: Push through Sort then into join child
    #[test]
    fn test_push_through_sort_into_join() {
        let join = make_inner_join(left_scan(), right_scan());
        let sorted = LogicalPlan {
            node: LogicalNode::Sort {
                order_by: vec![],
                input: Box::new(join),
            },
            schema: PlanSchema::from_columns(vec![
                ("a".to_string(), DataType::Int64),
                ("b".to_string(), DataType::Int64),
                ("c".to_string(), DataType::Int64),
                ("d".to_string(), DataType::Int64),
            ]),
        };
        let plan = sorted.filter(eq_expr(col_ref(0, "a"), const_int(1)));
        let result = rewrite_plan(plan);

        // Should be: Sort → Join(Filter(left), right)
        assert!(
            matches!(result.node, LogicalNode::Sort { .. }),
            "Top should be Sort"
        );
        if let LogicalNode::Sort { input, .. } = &result.node {
            assert!(is_join(input), "Sort's child should be Join");
            assert!(is_filter(join_left(input)), "Left should be Filter");
        }
    }

    // Test 16: Not pushed through Project
    #[test]
    fn test_not_pushed_through_project() {
        let scan = make_scan("t", vec![("x", DataType::Int64)]);
        let projected = LogicalPlan {
            node: LogicalNode::Project {
                projections: vec![],
                input: Box::new(scan),
            },
            schema: PlanSchema::from_columns(vec![("x".to_string(), DataType::Int64)]),
        };
        let plan = projected.filter(eq_expr(col_ref(0, "x"), const_int(1)));
        let result = rewrite_plan(plan);

        assert!(is_filter(&result), "Filter stays above Project");
        assert!(matches!(
            filter_input(&result).node,
            LogicalNode::Project { .. }
        ));
    }

    // Test 17: Not pushed through Aggregate
    #[test]
    fn test_not_pushed_through_aggregate() {
        let scan = make_scan("t", vec![("x", DataType::Int64)]);
        let agg = LogicalPlan {
            node: LogicalNode::Aggregate {
                group_by: vec![],
                projections: vec![],
                input: Box::new(scan),
            },
            schema: PlanSchema::from_columns(vec![("cnt".to_string(), DataType::Int64)]),
        };
        let plan = agg.filter(eq_expr(col_ref(0, "cnt"), const_int(1)));
        let result = rewrite_plan(plan);

        assert!(is_filter(&result), "Filter stays above Aggregate");
        assert!(matches!(
            filter_input(&result).node,
            LogicalNode::Aggregate { .. }
        ));
    }

    // Test 18: Not pushed through Limit
    #[test]
    fn test_not_pushed_through_limit() {
        let scan = make_scan("t", vec![("x", DataType::Int64)]);
        let limited = scan.limit(Some(const_int(10)), None);
        let plan = limited.filter(eq_expr(col_ref(0, "x"), const_int(1)));
        let result = rewrite_plan(plan);

        assert!(is_filter(&result), "Filter stays above Limit");
        assert!(matches!(
            filter_input(&result).node,
            LogicalNode::Limit { .. }
        ));
    }

    // Test 19: Push through CROSS join
    #[test]
    fn test_push_through_cross_join() {
        let join = make_cross_join(left_scan(), right_scan());
        let plan = join.filter(eq_expr(col_ref(0, "a"), const_int(1)));
        let result = rewrite_plan(plan);

        assert!(is_join(&result), "Top should be Join (cross)");
        assert!(
            is_filter(join_left(&result)),
            "Left-only pushed to left child"
        );
    }

    // Test 20: Column reindexing correctness
    #[test]
    fn test_column_reindexing() {
        let left = make_scan(
            "left_t",
            vec![
                ("a", DataType::Int64),
                ("b", DataType::Int64),
                ("c", DataType::Int64),
            ],
        );
        let right = make_scan(
            "right_t",
            vec![("d", DataType::Int64), ("e", DataType::Int64)],
        );
        let join = make_inner_join(left, right);
        // d = 1 → col_index 3 (right side, left_width=3) → should become col_index 0
        // e = 2 → col_index 4 → should become col_index 1
        let pred = and_expr(
            eq_expr(col_ref(3, "d"), const_int(1)),
            eq_expr(col_ref(4, "e"), const_int(2)),
        );
        let plan = join.filter(pred);
        let result = rewrite_plan(plan);

        assert!(is_join(&result), "Top should be Join");
        let right_child = join_right(&result);
        assert!(is_filter(right_child), "Right should be Filter");

        // Verify reindexing: check that the filter predicate uses indices 0 and 1
        if let LogicalNode::Filter { ref predicate, .. } = right_child.node {
            let indices = collect_column_indices(predicate);
            assert!(
                indices.contains(&0) && indices.contains(&1),
                "Reindexed indices should be 0 and 1, got {:?}",
                indices
            );
            assert!(
                !indices.contains(&3) && !indices.contains(&4),
                "Original indices should not appear"
            );
        }
    }

    // Test 21: G2 volatile fold/pushdown parity
    //
    // Exercises THREE systems for each volatile function:
    //   (a) is_volatile_or_side_effecting_builtin — shared helper
    //   (b) fold_typed_expr — must NOT fold volatile calls to Constant
    //   (c) predicate pushdown — must NOT push volatile predicates through Join
    //
    // This catches drift where someone modifies fold_typed_expr to ignore the
    // volatile list, or where pushdown bypasses the is_volatile check.
    #[test]
    fn test_volatile_fold_and_pushdown_agree() {
        let qctx = test_qctx();

        let volatile_names = [
            "RANDOM",
            "NEXTVAL",
            "CURRVAL",
            "SETVAL",
            "SETSEED",
            "GEN_RANDOM_UUID",
            "UUID_GENERATE_V4",
            "UUIDV7",
            "CLOCK_TIMESTAMP",
            "TXID_CURRENT",
        ];

        for name in &volatile_names {
            // 1. is_volatile_or_side_effecting_builtin must return true
            assert!(
                is_volatile_or_side_effecting_builtin(name),
                "{}: should be in volatile list",
                name
            );

            // 2. fold_typed_expr must NOT fold volatile call to Constant
            let bare = make_bare_function_call(name, FunctionKind::Builtin);
            let folded = fold_typed_expr(&bare, &qctx);
            assert!(
                matches!(folded.kind, TypedExprKind::FunctionCall { .. }),
                "{}: fold_typed_expr must not fold volatile function to Constant, got {:?}",
                name,
                folded.kind
            );

            // 3. is_volatile must detect it in an expression tree
            let expr = make_volatile_predicate(name);
            assert!(
                is_volatile(&expr),
                "{}: is_volatile should return true",
                name
            );

            // 4. Predicate pushdown must NOT push volatile predicate through join
            let join = make_inner_join(left_scan(), right_scan());
            let plan = join.filter(make_volatile_predicate(name));
            let rewritten = rewrite_plan(plan);
            assert!(
                is_filter(&rewritten),
                "{}: volatile predicate must stay above join",
                name
            );
        }

        // Statement-stable functions are NOT volatile — positive path
        for name in &["NOW", "STATEMENT_TIMESTAMP", "CURRENT_TIMESTAMP"] {
            assert!(
                !is_volatile_or_side_effecting_builtin(name),
                "{}: should NOT be in volatile list",
                name
            );

            // fold_typed_expr should fold stable functions (they evaluate via qctx)
            let bare = make_bare_function_call(name, FunctionKind::Builtin);
            let folded = fold_typed_expr(&bare, &qctx);
            assert!(
                !matches!(folded.kind, TypedExprKind::FunctionCall { .. }),
                "{}: fold_typed_expr should fold stable function to Constant, got {:?}",
                name,
                folded.kind
            );

            let expr = make_stable_predicate(name);
            assert!(!is_volatile(&expr), "{} should NOT be volatile", name);

            // Stable function predicates with no column refs go to left side
            // (constant predicate behavior)
            let join = make_inner_join(left_scan(), right_scan());
            let plan = join.filter(make_stable_predicate(name));
            let rewritten = rewrite_plan(plan);
            assert!(
                !is_filter(&rewritten),
                "{}: stable function predicate should be pushed through join",
                name
            );
        }
    }
}
