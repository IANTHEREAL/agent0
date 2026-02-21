//! Tests for logical plan rewrite rules.

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

fn make_inner_join_on(
    left: LogicalPlan,
    right: LogicalPlan,
    condition: JoinCondition,
) -> LogicalPlan {
    let mut combined = left.schema.columns.clone();
    combined.extend(right.schema.columns.clone());
    LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left),
            right: Box::new(right),
            join_type: JoinType::Inner,
            condition,
        },
        schema: PlanSchema::from_columns(combined),
    }
}

/// Extract join type from a Join node.
fn get_join_type(plan: &LogicalPlan) -> &JoinType {
    match &plan.node {
        LogicalNode::Join { join_type, .. } => join_type,
        _ => panic!("expected Join node"),
    }
}

/// Extract join condition from a Join node.
fn get_join_condition(plan: &LogicalPlan) -> &JoinCondition {
    match &plan.node {
        LogicalNode::Join { condition, .. } => condition,
        _ => panic!("expected Join node"),
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

// ── Cross-join elimination tests ──────────────────────

// Test 22: Cross join with equi filter → absorbed into Inner ON
#[test]
fn test_cross_join_with_equi_filter() {
    let join = make_cross_join(left_scan(), right_scan());
    // Filter(col0 = col2, Cross(A, B))
    let plan = join.filter(eq_expr(col_ref(0, "a"), col_ref(2, "c")));
    let result = eliminate_cross_joins(plan);

    // Should be: Inner(ON col0=col2, A, B) — no Filter above
    assert!(is_join(&result), "Top should be Join, not Filter");
    assert_eq!(get_join_type(&result), &JoinType::Inner);
    assert!(
        matches!(get_join_condition(&result), JoinCondition::On(_)),
        "Should have ON condition"
    );
}

// Test 23: Cross join with multi-key equi filter
#[test]
fn test_cross_join_multi_key() {
    let join = make_cross_join(left_scan(), right_scan());
    // Filter(col0=col2 AND col1=col3, Cross(A, B))
    let pred = and_expr(
        eq_expr(col_ref(0, "a"), col_ref(2, "c")),
        eq_expr(col_ref(1, "b"), col_ref(3, "d")),
    );
    let plan = join.filter(pred);
    let result = eliminate_cross_joins(plan);

    assert!(is_join(&result), "Top should be Join, not Filter");
    assert_eq!(get_join_type(&result), &JoinType::Inner);
    assert!(matches!(get_join_condition(&result), JoinCondition::On(_)));
}

// Test 24: Cross join with mixed predicates — equi absorbed, non-equi stays
#[test]
fn test_cross_join_mixed() {
    let join = make_cross_join(left_scan(), right_scan());
    // Filter(col0=col2 AND col0=const(1), Cross(A, B))
    let pred = and_expr(
        eq_expr(col_ref(0, "a"), col_ref(2, "c")),
        eq_expr(col_ref(0, "a"), const_int(1)),
    );
    let plan = join.filter(pred);
    let result = eliminate_cross_joins(plan);

    // Should be: Filter(col0=const, Inner(ON col0=col2, A, B))
    assert!(is_filter(&result), "Non-equi predicate stays as Filter");
    let inner_join = filter_input(&result);
    assert!(is_join(inner_join), "Inner should be Join");
    assert_eq!(get_join_type(inner_join), &JoinType::Inner);
    assert!(matches!(
        get_join_condition(inner_join),
        JoinCondition::On(_)
    ));
}

// Test 25: Cross join with non-equi only — unchanged
#[test]
fn test_cross_join_no_equi() {
    let join = make_cross_join(left_scan(), right_scan());
    // Filter(col0 > col2, Cross(A, B))
    let plan = join.filter(gt_expr(col_ref(0, "a"), col_ref(2, "c")));
    let result = eliminate_cross_joins(plan);

    // Should remain: Filter(col0 > col2, Cross(A, B))
    assert!(is_filter(&result), "Should still be Filter above");
    let inner = filter_input(&result);
    assert!(is_join(inner));
    assert_eq!(get_join_type(inner), &JoinType::Cross);
}

// Test 26: Left join not touched
#[test]
fn test_left_join_not_touched() {
    let join = make_left_join(left_scan(), right_scan());
    let plan = join.filter(eq_expr(col_ref(0, "a"), col_ref(2, "c")));
    let result = eliminate_cross_joins(plan);

    // Outer joins are skipped — should remain unchanged
    assert!(is_filter(&result), "Filter should stay above Left join");
    assert_eq!(get_join_type(filter_input(&result)), &JoinType::Left);
}

// Test 27: Nested cross joins — bottom-up absorption
#[test]
fn test_nested_cross_joins() {
    // FROM a, b, c WHERE a.id=b.id AND b.id=c.id
    // After pushdown: Filter(b.id=c.id, Cross(Filter(a.id=b.id, Cross(A, B)), C))
    let a = make_scan("a", vec![("a_id", DataType::Int64)]);
    let b = make_scan("b", vec![("b_id", DataType::Int64)]);
    let c = make_scan("c", vec![("c_id", DataType::Int64)]);

    // Inner cross join: Cross(A, B), width = 2 (a_id=0, b_id=1)
    let ab = make_cross_join(a, b);
    // Filter(a_id=b_id) over Cross(A, B)
    let filter_ab = ab.filter(eq_expr(col_ref(0, "a_id"), col_ref(1, "b_id")));
    // Outer cross join: Cross(filter_ab, C), width = 3 (a_id=0, b_id=1, c_id=2)
    let abc = make_cross_join(filter_ab, c);
    // Filter(b_id=c_id) over Cross(...)
    let plan = abc.filter(eq_expr(col_ref(1, "b_id"), col_ref(2, "c_id")));

    let result = eliminate_cross_joins(plan);

    // Should be: Inner(ON b_id=c_id, Inner(ON a_id=b_id, A, B), C)
    assert!(is_join(&result), "Top should be Join");
    assert_eq!(get_join_type(&result), &JoinType::Inner);
    assert!(matches!(get_join_condition(&result), JoinCondition::On(_)));

    let left_child = join_left(&result);
    assert!(is_join(left_child), "Left child should be Join");
    assert_eq!(get_join_type(left_child), &JoinType::Inner);
    assert!(matches!(
        get_join_condition(left_child),
        JoinCondition::On(_)
    ));
}

// Test 28: Inner join — absorb additional pure equi predicate
#[test]
fn test_inner_join_absorb_pure_equi() {
    // Inner(ON col0=col2, A, B) with Filter(col1=col3)
    let join = make_inner_join_on(
        left_scan(),
        right_scan(),
        JoinCondition::On(eq_expr(col_ref(0, "a"), col_ref(2, "c"))),
    );
    let plan = join.filter(eq_expr(col_ref(1, "b"), col_ref(3, "d")));
    let result = eliminate_cross_joins(plan);

    // Should be: Inner(ON col0=col2 AND col1=col3, A, B) — no Filter
    assert!(is_join(&result), "Top should be Join, not Filter");
    assert_eq!(get_join_type(&result), &JoinType::Inner);
    // The merged ON should have both keys
    if let JoinCondition::On(ref on_expr) = get_join_condition(&result) {
        let indices = collect_column_indices(on_expr);
        assert!(
            indices.contains(&0)
                && indices.contains(&1)
                && indices.contains(&2)
                && indices.contains(&3),
            "Merged ON should reference all 4 columns, got {:?}",
            indices
        );
    } else {
        panic!("Expected ON condition");
    }
}

// Test 28b: Cross-pair dedup must not false-positive on independent key membership
//
// Existing ON: (l.a=r.c AND l.b=r.d)  → pairs {(0,0),(1,1)}
// Filter:      l.a=r.d                 → pair  (0,1) — NOT a duplicate
// Bug (fixed): independent contains checks saw a∈{a,b} and d∈{c,d} → true → dropped predicate
#[test]
fn test_dedup_cross_pair_not_false_positive() {
    // ON col0=col2 AND col1=col3 (existing pairs: (0,0) and (1,1))
    let existing_on = and_expr(
        eq_expr(col_ref(0, "a"), col_ref(2, "c")),
        eq_expr(col_ref(1, "b"), col_ref(3, "d")),
    );
    let join = make_inner_join_on(left_scan(), right_scan(), JoinCondition::On(existing_on));
    // Filter: col0=col3 (pair (0,1) — cross-pair, not a duplicate)
    let plan = join.filter(eq_expr(col_ref(0, "a"), col_ref(3, "d")));
    let result = eliminate_cross_joins(plan);

    // The cross-pair (0,1) must be absorbed (not dropped), producing a 3-key ON
    assert!(is_join(&result), "Top should be Join (all equi → absorbed)");
    assert_eq!(get_join_type(&result), &JoinType::Inner);
    // Extract actual equi-key pairs and verify count + content.
    // Column-index collection alone can't prove the cross-pair was retained
    // because the existing ON already references all 4 columns.
    let condition = get_join_condition(&result);
    let (lk, rk) =
        join_keys::try_extract_equi_keys(condition, 2).expect("merged ON must be pure-equi");
    let pairs: Vec<(usize, usize)> = lk.into_iter().zip(rk).collect();
    assert_eq!(pairs.len(), 3, "should have 3 key pairs, got {:?}", pairs);
    assert!(
        pairs.contains(&(0, 0)) && pairs.contains(&(1, 1)) && pairs.contains(&(0, 1)),
        "expected pairs (0,0), (1,1), (0,1) — got {:?}",
        pairs
    );
}

// Test 29: Inner join — no absorb when merged ON would be mixed
#[test]
fn test_inner_join_no_absorb_mixed() {
    // Inner(ON col0 > col2, A, B) with Filter(col0=col2)
    let join = make_inner_join_on(
        left_scan(),
        right_scan(),
        JoinCondition::On(gt_expr(col_ref(0, "a"), col_ref(2, "c"))),
    );
    let plan = join.filter(eq_expr(col_ref(0, "a"), col_ref(2, "c")));
    let result = eliminate_cross_joins(plan);

    // Should remain unchanged — merged ON would be mixed (gt + eq) → skip
    assert!(
        is_filter(&result),
        "Filter should stay above (merged would be mixed)"
    );
    let inner = filter_input(&result);
    assert!(is_join(inner));
    assert_eq!(get_join_type(inner), &JoinType::Inner);
}

// Test 30: USING condition not modified
#[test]
fn test_using_not_modified() {
    use crate::sql::analyzer::types::ResolvedUsingColumn;
    let left = left_scan();
    let right = right_scan();
    let mut combined = left.schema.columns.clone();
    combined.extend(right.schema.columns.clone());
    let join = LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left),
            right: Box::new(right),
            join_type: JoinType::Inner,
            condition: JoinCondition::Using(vec![ResolvedUsingColumn {
                name: "id".to_string(),
                left_index: 0,
                right_index: 0,
                data_type: DataType::Int64,
                left_type: DataType::Int64,
                right_type: DataType::Int64,
            }]),
        },
        schema: PlanSchema::from_columns(combined),
    };
    let plan = join.filter(eq_expr(col_ref(1, "b"), col_ref(3, "d")));
    let result = eliminate_cross_joins(plan);

    // USING joins are skipped — should remain unchanged
    assert!(is_filter(&result), "Filter should stay above USING join");
    assert!(matches!(
        get_join_condition(filter_input(&result)),
        JoinCondition::Using(_)
    ));
}

// Test 31: Correlated ref not absorbed
#[test]
fn test_correlated_ref_not_absorbed() {
    let join = make_cross_join(left_scan(), right_scan());
    // Filter(correlated_col = col2, Cross(A, B))
    let plan = join.filter(eq_expr(correlated_col_ref(0, "outer"), col_ref(2, "c")));
    let result = eliminate_cross_joins(plan);

    // Correlated ref rejected by join_keys extractor → stays as Filter + Cross
    assert!(is_filter(&result), "Correlated ref stays as Filter");
    let inner = filter_input(&result);
    assert!(is_join(inner));
    assert_eq!(get_join_type(inner), &JoinType::Cross);
}

// Test 32: End-to-end — pushdown then elimination via apply_rewrites
#[test]
fn test_apply_rewrites_pushdown_then_elim() {
    let join = make_cross_join(left_scan(), right_scan());
    // Filter(a=1 AND a.col0=b.col2, Cross(A, B))
    let pred = and_expr(
        eq_expr(col_ref(0, "a"), const_int(1)),
        eq_expr(col_ref(0, "a"), col_ref(2, "c")),
    );
    let plan = join.filter(pred);
    let result = apply_rewrites(plan, None);

    // After pushdown: Filter(a.col0=b.col2, Cross(Filter(a=1, A), B))
    // After elim: Inner(ON a.col0=b.col2, Filter(a=1, A), B)
    assert!(is_join(&result), "Top should be Inner Join");
    assert_eq!(get_join_type(&result), &JoinType::Inner);
    assert!(matches!(get_join_condition(&result), JoinCondition::On(_)));
    assert!(
        is_filter(join_left(&result)),
        "Left child should have pushed-down Filter(a=1)"
    );
    assert!(
        !is_filter(join_right(&result)),
        "Right child should not have a Filter"
    );
}
