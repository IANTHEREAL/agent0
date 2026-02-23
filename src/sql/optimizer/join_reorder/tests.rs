//! Unit tests for join reordering.

use super::predicates::{
    classify_predicates, flatten_recursive, is_pure_equi_join_pred, lift_typed_expr, BaseRelation,
};
use super::*;
use crate::sql::analyzer::types::{BinaryOp, JoinCondition, JoinType, TypedExpr, TypedExprKind};
use crate::sql::optimizer::logical_plan::PlanSchema;
use crate::sql::optimizer::physical_plan::{PhysicalNode, PhysicalPlan};
use crate::sql::optimizer::physical_planner::PhysicalPlanner;
use crate::sql::optimizer::physical_planner::PlanningContext;
use crate::sql::optimizer::rewrite::{collect_column_indices, split_conjunction};
use crate::sql::optimizer::statistics::{ColumnStatistics, TableStatistics};
use crate::types::DataType;
use std::collections::HashMap;
use std::sync::Arc;

// ── Test helpers ─────────────────────────────────────────

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
        kind: TypedExprKind::Constant(crate::types::Value::Int64(v)),
        data_type: DataType::Int64,
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

fn and_expr_helper(left: TypedExpr, right: TypedExpr) -> TypedExpr {
    crate::sql::optimizer::rewrite::and_expr(left, right)
}

fn make_scan(name: &str, alias: Option<&str>, cols: Vec<(&str, DataType)>) -> LogicalPlan {
    let columns: Vec<(String, DataType)> = cols
        .into_iter()
        .map(|(n, dt)| (n.to_string(), dt))
        .collect();
    LogicalPlan::scan(
        name.to_string(),
        alias.map(|s| s.to_string()),
        PlanSchema::from_columns(columns),
    )
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

fn make_inner_join_on(left: LogicalPlan, right: LogicalPlan, on: TypedExpr) -> LogicalPlan {
    let mut combined = left.schema.columns.clone();
    combined.extend(right.schema.columns.clone());
    LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left),
            right: Box::new(right),
            join_type: JoinType::Inner,
            condition: JoinCondition::On(on),
        },
        schema: PlanSchema::from_columns(combined),
    }
}

fn make_left_join_on(left: LogicalPlan, right: LogicalPlan, on: TypedExpr) -> LogicalPlan {
    let mut combined = left.schema.columns.clone();
    combined.extend(right.schema.columns.clone());
    LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left),
            right: Box::new(right),
            join_type: JoinType::Left,
            condition: JoinCondition::On(on),
        },
        schema: PlanSchema::from_columns(combined),
    }
}

fn make_col_stats(null_fraction: f64, n_distinct: f64) -> ColumnStatistics {
    ColumnStatistics {
        null_fraction,
        n_distinct,
        avg_width: 4,
        most_common_vals: vec![],
        most_common_freqs: vec![],
        histogram_bounds: vec![],
        correlation: 0.0,
    }
}

fn make_table_stats(
    row_count: usize,
    columns: HashMap<String, ColumnStatistics>,
) -> Arc<TableStatistics> {
    Arc::new(TableStatistics {
        table_id: 1,
        row_count,
        last_analyzed: 1000,
        columns,
    })
}

fn ctx_with_stats(entries: Vec<(&str, usize, Vec<(&str, f64)>)>) -> PlanningContext {
    let mut ctx = PlanningContext::empty();
    for (name, rows, cols) in entries {
        let mut col_stats = HashMap::new();
        for (col_name, ndv) in cols {
            col_stats.insert(col_name.to_string(), make_col_stats(0.0, ndv));
        }
        ctx.table_stats
            .insert(name.to_string(), make_table_stats(rows, col_stats));
    }
    ctx
}

/// Collect all scan table names in DFS order.
fn collect_scan_names(plan: &LogicalPlan) -> Vec<String> {
    let mut names = Vec::new();
    collect_scan_names_inner(plan, &mut names);
    names
}

fn collect_scan_names_inner(plan: &LogicalPlan, names: &mut Vec<String>) {
    match &plan.node {
        LogicalNode::Scan { table_name, .. } => names.push(table_name.clone()),
        LogicalNode::Filter { input, .. }
        | LogicalNode::Project { input, .. }
        | LogicalNode::Sort { input, .. }
        | LogicalNode::Limit { input, .. }
        | LogicalNode::Distinct { input }
        | LogicalNode::DistinctOn { input, .. }
        | LogicalNode::Window { input, .. }
        | LogicalNode::Aggregate { input, .. } => collect_scan_names_inner(input, names),
        LogicalNode::Join { left, right, .. } | LogicalNode::SetOperation { left, right, .. } => {
            collect_scan_names_inner(left, names);
            collect_scan_names_inner(right, names);
        }
        LogicalNode::Subquery { subplan, .. } => collect_scan_names_inner(subplan, names),
        _ => {}
    }
}

// ── Test 1: Index lifting correctness ────────────────────

#[test]
fn test_lift_typed_expr() {
    let expr = col_ref(0, "a");
    let lifted = lift_typed_expr(&expr, 3);
    if let TypedExprKind::ColumnRef { column_index, .. } = &lifted.kind {
        assert_eq!(*column_index, 3, "lifting by 3 should produce index 3");
    } else {
        panic!("expected ColumnRef");
    }
}

#[test]
fn test_lift_preserves_correlated() {
    let expr = correlated_col_ref(0, "outer");
    let lifted = lift_typed_expr(&expr, 5);
    if let TypedExprKind::ColumnRef {
        scope_depth,
        column_index,
        ..
    } = &lifted.kind
    {
        assert_eq!(*scope_depth, 1, "scope_depth unchanged");
        assert_eq!(*column_index, 0, "correlated ref unchanged");
    }
}

#[test]
fn test_lift_compound_expr() {
    // eq(col(0), col(1)) lifted by 2 → eq(col(2), col(3))
    let expr = eq_expr(col_ref(0, "a"), col_ref(1, "b"));
    let lifted = lift_typed_expr(&expr, 2);
    let indices = collect_column_indices(&lifted);
    assert!(indices.contains(&2) && indices.contains(&3));
    assert!(!indices.contains(&0) && !indices.contains(&1));
}

// ── Test 2: Subquery safety gate ─────────────────────────

#[test]
fn test_subquery_safety_gate() {
    let a = make_scan("a", None, vec![("id", DataType::Int64)]);
    let b = make_scan("b", None, vec![("id", DataType::Int64)]);
    let join = make_inner_join(a, b);

    // Create an InSubquery expression in the ON condition
    let subquery_pred = TypedExpr {
        kind: TypedExprKind::InSubquery {
            expr: Box::new(col_ref(0, "id")),
            subquery: Box::new(crate::sql::analyzer::types::AnalyzedQuery {
                ctes: vec![],
                body: crate::sql::analyzer::types::AnalyzedQueryBody::Values(vec![]),
                order_by: vec![],
                limit: None,
                offset: None,
                output_schema: vec![],
            }),
            negated: false,
        },
        data_type: DataType::Boolean,
    };

    let plan = LogicalPlan {
        node: LogicalNode::Filter {
            predicate: subquery_pred,
            input: Box::new(join),
        },
        schema: PlanSchema::from_columns(vec![
            ("id".to_string(), DataType::Int64),
            ("id".to_string(), DataType::Int64),
        ]),
    };

    assert!(logical_has_unresolved_subquery(&plan));

    // Should skip reordering (no crash, returns plan unchanged in structure)
    let ctx = PlanningContext::empty();
    let result = reorder_joins(plan, &ctx);
    // Just verify it doesn't crash and preserves structure
    assert!(matches!(result.node, LogicalNode::Filter { .. }));
}

// ── Test 3: Correlated RHS barrier ───────────────────────

#[test]
fn test_correlated_rhs_barrier() {
    // Build a right subtree with a correlated ref
    let left = make_scan("a", None, vec![("id", DataType::Int64)]);
    let right_inner = LogicalPlan {
        node: LogicalNode::Filter {
            predicate: eq_expr(col_ref(0, "id"), correlated_col_ref(0, "outer_id")),
            input: Box::new(make_scan("b", None, vec![("id", DataType::Int64)])),
        },
        schema: PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
    };

    assert!(logical_has_correlated_refs(&right_inner));

    // When flattening, correlated RHS should cause entire join to be opaque
    let join = make_inner_join_on(
        left,
        right_inner,
        eq_expr(col_ref(0, "a_id"), col_ref(1, "b_id")),
    );
    let mut rels = Vec::new();
    let mut preds = Vec::new();
    flatten_recursive(join, &mut rels, &mut preds, 0);

    // Whole join is opaque base relation — 1 rel, no extracted predicates
    assert_eq!(rels.len(), 1, "correlated RHS makes whole join opaque");
    assert!(preds.is_empty(), "no preds extracted from opaque join");
}

/// Regression: correlated RHS through full reorder_joins must not stack-overflow.
/// The inner join is "flattenable" by type but the correlated RHS prevents
/// actual flattening → 1 opaque rel. The fix ensures we don't re-enter
/// reorder_recursive on the same node.
#[test]
fn test_correlated_rhs_no_infinite_recursion() {
    let left = make_scan("a", None, vec![("id", DataType::Int64)]);
    let right_inner = LogicalPlan {
        node: LogicalNode::Filter {
            predicate: eq_expr(col_ref(0, "id"), correlated_col_ref(0, "outer_id")),
            input: Box::new(make_scan("b", None, vec![("id", DataType::Int64)])),
        },
        schema: PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
    };
    let join = make_inner_join_on(
        left,
        right_inner,
        eq_expr(col_ref(0, "a_id"), col_ref(1, "b_id")),
    );

    let ctx = PlanningContext::empty();
    // This must terminate (previously caused infinite recursion / stack overflow)
    let result = reorder_joins(join, &ctx);
    // Structure preserved: still a Join at top
    assert!(matches!(result.node, LogicalNode::Join { .. }));
}

// ── Test 4: Conjunction splitting ────────────────────────

#[test]
fn test_conjunction_splitting() {
    let a = make_scan("a", None, vec![("id", DataType::Int64)]);
    let b = make_scan("b", None, vec![("id", DataType::Int64)]);
    let c = make_scan("c", None, vec![("id", DataType::Int64)]);

    // ON (a.id = b.id AND b.id > 5)
    let mixed_on = and_expr_helper(
        eq_expr(col_ref(0, "a_id"), col_ref(1, "b_id")),
        gt_expr(col_ref(1, "b_id"), const_int(5)),
    );

    let ab = make_inner_join_on(a, b, mixed_on);
    let abc = make_inner_join_on(ab, c, eq_expr(col_ref(1, "b_id"), col_ref(2, "c_id")));

    let mut rels = Vec::new();
    let mut preds = Vec::new();
    flatten_recursive(abc, &mut rels, &mut preds, 0);

    assert_eq!(rels.len(), 3, "should flatten to 3 base relations");
    // Flattening collects raw ON exprs (not split yet):
    // - mixed_on (AND of 2 conjuncts) as 1 raw pred
    // - outer ON as 1 raw pred
    assert_eq!(preds.len(), 2, "should have 2 raw ON predicates");

    // After split_conjunction, should expand to 3 conjuncts
    let all_conjuncts: Vec<TypedExpr> = preds.into_iter().flat_map(split_conjunction).collect();
    assert_eq!(
        all_conjuncts.len(),
        3,
        "should have 3 conjuncts after splitting"
    );
}

// ── Test 5: Pure equi detection ─────────────────────────

#[test]
fn test_pure_equi_detection() {
    let rels = vec![
        BaseRelation {
            id: 0,
            plan: make_scan("a", None, vec![("id", DataType::Int64)]),
            col_offset: 0,
            width: 1,
        },
        BaseRelation {
            id: 1,
            plan: make_scan("b", None, vec![("id", DataType::Int64)]),
            col_offset: 1,
            width: 1,
        },
    ];

    // Bare Var = Var across rels → pure equi
    let equi = eq_expr(col_ref(0, "a_id"), col_ref(1, "b_id"));
    assert!(is_pure_equi_join_pred(&equi, &rels));

    // Var = Const → not equi (single-table filter)
    let not_equi = eq_expr(col_ref(0, "a_id"), const_int(42));
    assert!(!is_pure_equi_join_pred(&not_equi, &rels));

    // Var > Var → not equi (wrong op)
    let gt = gt_expr(col_ref(0, "a_id"), col_ref(1, "b_id"));
    assert!(!is_pure_equi_join_pred(&gt, &rels));

    // Same-table Var = Var → not equi
    let same_table = eq_expr(col_ref(0, "a_id"), col_ref(0, "a_id2"));
    assert!(!is_pure_equi_join_pred(&same_table, &rels));

    // f(Var) = Var → not equi (not bare ColumnRef)
    let cast_expr = TypedExpr {
        kind: TypedExprKind::Cast {
            expr: Box::new(col_ref(0, "a_id")),
            target_type: DataType::Text,
            cast_context: crate::sql::types::CastContext::Explicit,
        },
        data_type: DataType::Text,
    };
    let cast_eq = eq_expr(cast_expr, col_ref(1, "b_id"));
    assert!(!is_pure_equi_join_pred(&cast_eq, &rels));
}

// ── Test 6: 1-rel conjunct attachment ────────────────────

#[test]
fn test_single_rel_attachment() {
    let rels = vec![
        BaseRelation {
            id: 0,
            plan: make_scan("a", None, vec![("id", DataType::Int64)]),
            col_offset: 0,
            width: 1,
        },
        BaseRelation {
            id: 1,
            plan: make_scan("b", None, vec![("id", DataType::Int64)]),
            col_offset: 1,
            width: 1,
        },
    ];

    let conjuncts = vec![
        eq_expr(col_ref(0, "a_id"), const_int(1)),       // 1-rel: a
        eq_expr(col_ref(0, "a_id"), col_ref(1, "b_id")), // 2-rel: edge
        gt_expr(col_ref(1, "b_id"), const_int(5)),       // 1-rel: b
    ];

    let classification = classify_predicates(&conjuncts, &rels);

    assert_eq!(classification.edges.len(), 1, "should have 1 edge");
    assert_eq!(
        classification
            .base_local_filters
            .get(&0)
            .map(|v| v.len())
            .unwrap_or(0),
        1,
        "rel 0 should have 1 local filter"
    );
    assert_eq!(
        classification
            .base_local_filters
            .get(&1)
            .map(|v| v.len())
            .unwrap_or(0),
        1,
        "rel 1 should have 1 local filter"
    );
    assert!(
        classification.remaining.is_empty(),
        "no remaining predicates"
    );
}

// ── Test 7: DPccp optimal order ─────────────────────────

#[test]
fn test_dpccp_optimal_order() {
    // 3 tables: small (10), medium (1000), big (10000)
    // Chain join: big.id = medium.id AND medium.id = small.id
    // big↔medium and medium↔small are connected; big↔small have NO direct edge.
    //
    // Input order (suboptimal): (big CROSS medium) JOIN small
    //   All predicates on the outer ON clause.
    // Optimal: (medium ⋈ small) ⋈ big — cost 0.3
    //   step 1: medium(1000) ⋈ small(10), sel=1/1000, rows=10, cost=0.1
    //   step 2: result(10) ⋈ big(10000), sel=1/5000, rows=20, cost=0.3
    // Suboptimal: (big ⋈ medium) ⋈ small — cost 20.2
    //   step 1: big(10000) ⋈ medium(1000), sel=1/5000, rows=2000, cost=20.0
    //   step 2: result(2000) ⋈ small(10), sel=1/1000, rows=20, cost=20.2
    //
    // The key: big_t should NOT appear in the first (deepest) join.
    let big = make_scan("big_t", None, vec![("id", DataType::Int64)]);
    let medium = make_scan("medium_t", None, vec![("id", DataType::Int64)]);
    let small = make_scan("small_t", None, vec![("id", DataType::Int64)]);

    // big(0) JOIN medium(1) — cross join (no predicate)
    let bm = make_inner_join(big, medium);
    // (big, medium)(0,1) JOIN small(2) ON big.id=medium.id AND medium.id=small.id
    let plan = make_inner_join_on(
        bm,
        small,
        and_expr_helper(
            eq_expr(col_ref(0, "big_id"), col_ref(1, "med_id")),
            eq_expr(col_ref(1, "med_id"), col_ref(2, "small_id")),
        ),
    );

    let ctx = ctx_with_stats(vec![
        ("small_t", 10, vec![("id", 10.0)]),
        ("big_t", 10000, vec![("id", 5000.0)]),
        ("medium_t", 1000, vec![("id", 1000.0)]),
    ]);

    let result = reorder_joins(plan, &ctx);

    let names = collect_scan_names(&result);
    assert_eq!(names.len(), 3, "should have 3 scans");

    // Find the deepest join's direct children.
    fn deepest_join_children(plan: &LogicalPlan) -> Option<(Vec<String>, Vec<String>)> {
        match &plan.node {
            LogicalNode::Join { left, right, .. } => {
                if let Some(deeper) = deepest_join_children(left) {
                    return Some(deeper);
                }
                if let Some(deeper) = deepest_join_children(right) {
                    return Some(deeper);
                }
                Some((collect_scan_names(left), collect_scan_names(right)))
            }
            LogicalNode::Filter { input, .. } | LogicalNode::Project { input, .. } => {
                deepest_join_children(input)
            }
            _ => None,
        }
    }

    let (left_names, right_names) =
        deepest_join_children(&result).expect("should find a join node");
    let first_join_tables: Vec<&str> = left_names
        .iter()
        .chain(right_names.iter())
        .map(|s| s.as_str())
        .collect();

    // The first (deepest) join should pair the two smaller tables,
    // NOT include big_t.
    assert!(
        !first_join_tables.contains(&"big_t"),
        "first join should not involve big_t (10000 rows), got {:?}",
        first_join_tables
    );
}

// ── Test 8: Disconnected graph stitching ─────────────────

#[test]
fn test_disconnected_graph_stitching() {
    // Two disconnected pairs: (a JOIN b) CROSS JOIN (c JOIN d)
    let a = make_scan("a", None, vec![("id", DataType::Int64)]);
    let b = make_scan("b", None, vec![("id", DataType::Int64)]);
    let c = make_scan("c", None, vec![("id", DataType::Int64)]);
    let d = make_scan("d", None, vec![("id", DataType::Int64)]);

    let ab = make_inner_join_on(a, b, eq_expr(col_ref(0, "id"), col_ref(1, "id")));
    let cd = make_inner_join_on(c, d, eq_expr(col_ref(0, "id"), col_ref(1, "id")));
    let plan = make_inner_join(ab, cd);

    let ctx = ctx_with_stats(vec![
        ("a", 100, vec![("id", 100.0)]),
        ("b", 100, vec![("id", 100.0)]),
        ("c", 50, vec![("id", 50.0)]),
        ("d", 50, vec![("id", 50.0)]),
    ]);

    let result = reorder_joins(plan, &ctx);
    let names = collect_scan_names(&result);
    assert_eq!(names.len(), 4, "should have 4 scans");
}

// ── Test 9: Column remap correctness ─────────────────────

#[test]
fn test_column_remap_3way() {
    // FROM a(id), b(id), c(id) WHERE a.id = c.id AND b.id = c.id
    // Original order: a(0), b(1), c(2)
    // If reordered to a, c, b — output schema must still match a, b, c
    let a = make_scan("a", None, vec![("a_id", DataType::Int64)]);
    let b = make_scan("b", None, vec![("b_id", DataType::Int64)]);
    let c = make_scan("c", None, vec![("c_id", DataType::Int64)]);

    let ab = make_inner_join(a, b);
    let abc = make_inner_join_on(
        ab,
        c,
        and_expr_helper(
            eq_expr(col_ref(0, "a_id"), col_ref(2, "c_id")),
            eq_expr(col_ref(1, "b_id"), col_ref(2, "c_id")),
        ),
    );

    let ctx = ctx_with_stats(vec![
        ("a", 100, vec![("a_id", 100.0)]),
        ("b", 1000, vec![("b_id", 1000.0)]),
        ("c", 10, vec![("c_id", 10.0)]),
    ]);

    let result = reorder_joins(abc, &ctx);
    // Output schema should have 3 columns
    assert_eq!(
        result.schema.columns.len(),
        3,
        "output should have 3 columns"
    );
}

// ── Test 10: Self-join alias stats ───────────────────────

#[test]
fn test_self_join_alias_stats() {
    let a = make_scan("t", Some("a"), vec![("id", DataType::Int64)]);
    let b = make_scan("t", Some("b"), vec![("id", DataType::Int64)]);
    let plan = make_inner_join_on(a, b, eq_expr(col_ref(0, "id"), col_ref(1, "id")));

    let mut ctx = PlanningContext::empty();
    let mut cols = HashMap::new();
    cols.insert("id".to_string(), make_col_stats(0.0, 100.0));
    // Both aliases should have distinct keys
    ctx.table_stats.insert(
        crate::sql::optimizer::schema_map_key("t", Some("a")),
        make_table_stats(1000, cols.clone()),
    );
    ctx.table_stats.insert(
        crate::sql::optimizer::schema_map_key("t", Some("b")),
        make_table_stats(500, cols),
    );

    let result = reorder_joins(plan, &ctx);
    assert_eq!(result.schema.columns.len(), 2);
}

// ── Test 11: No-op cases ─────────────────────────────────

#[test]
fn test_noop_single_table() {
    let a = make_scan("a", None, vec![("id", DataType::Int64)]);
    let ctx = PlanningContext::empty();
    let result = reorder_joins(a.clone(), &ctx);
    assert!(matches!(result.node, LogicalNode::Scan { .. }));
}

#[test]
fn test_noop_two_tables_already_optimal() {
    let a = make_scan("a", None, vec![("id", DataType::Int64)]);
    let b = make_scan("b", None, vec![("id", DataType::Int64)]);
    let plan = make_inner_join_on(a, b, eq_expr(col_ref(0, "id"), col_ref(1, "id")));

    // Both same size — order shouldn't change
    let ctx = ctx_with_stats(vec![
        ("a", 1000, vec![("id", 1000.0)]),
        ("b", 1000, vec![("id", 1000.0)]),
    ]);

    let result = reorder_joins(plan, &ctx);
    assert_eq!(result.schema.columns.len(), 2);
}

// ── Test 12: Physical plan build-side assertion ──────────

#[test]
fn test_physical_plan_build_side() {
    // Ensure that after reordering, HashJoin picks smaller side as build.
    // Input: big(10000) on left, small(10) on right — suboptimal.
    // After reorder, small should move to left and be the build side.
    let small = make_scan("small_t", None, vec![("id", DataType::Int64)]);
    let big = make_scan("big_t", None, vec![("id", DataType::Int64)]);

    // Intentionally put big on left
    let plan = make_inner_join_on(big, small, eq_expr(col_ref(0, "id"), col_ref(1, "id")));

    let ctx = ctx_with_stats(vec![
        ("small_t", 10, vec![("id", 10.0)]),
        ("big_t", 10000, vec![("id", 10000.0)]),
    ]);

    let reordered = reorder_joins(plan, &ctx);
    let physical = PhysicalPlanner::plan(&reordered, &ctx);

    // Walk the physical plan to find the HashJoin
    fn find_hash_join(plan: &PhysicalPlan) -> Option<bool> {
        match &plan.node {
            PhysicalNode::HashJoin { left_is_build, .. } => Some(*left_is_build),
            PhysicalNode::Filter { input, .. }
            | PhysicalNode::Project { input, .. }
            | PhysicalNode::Sort { input, .. }
            | PhysicalNode::Limit { input, .. } => find_hash_join(input),
            _ => None,
        }
    }

    let left_is_build = find_hash_join(&physical).expect("expected HashJoin in physical plan");
    // Physical planner sets left_is_build = (left_rows <= right_rows).
    // With 2 tables, DPccp keeps the enumeration order (big on left,
    // small on right). The physical planner independently picks the
    // smaller side as build: left_is_build = (10000 <= 10) = false,
    // meaning right/small is the build side — correct behavior.
    assert!(
        !left_is_build,
        "big(10000) on left, small(10) on right: left_is_build should be false \
         (right/small is build side); got left_is_build=true"
    );
}

// ── Test: Outer join unchanged ───────────────────────────

#[test]
fn test_outer_join_unchanged() {
    let a = make_scan("a", None, vec![("id", DataType::Int64)]);
    let b = make_scan("b", None, vec![("id", DataType::Int64)]);
    let plan = make_left_join_on(a, b, eq_expr(col_ref(0, "id"), col_ref(1, "id")));

    let ctx = PlanningContext::empty();
    let result = reorder_joins(plan, &ctx);

    // LEFT JOIN should not be flattened or reordered
    match &result.node {
        LogicalNode::Join { join_type, .. } => {
            assert_eq!(*join_type, JoinType::Left, "LEFT join preserved");
        }
        _ => panic!("expected Join node"),
    }
}

// ── Test: n>=64 preserves original join structure ─────────

#[test]
fn test_large_join_group_preserves_on_conditions() {
    // Build a chain of 65 inner joins with ON conditions.
    // count_flattenable_relations should detect >= 64 and skip reordering,
    // preserving every JoinCondition::On (no degradation to cross join).
    let mut plan = make_scan("t0", None, vec![("id", DataType::Int64)]);
    for i in 1..65 {
        let right = make_scan(&format!("t{}", i), None, vec![("id", DataType::Int64)]);
        // ON condition referencing left col 0 and right col (width of left)
        let left_width = plan.schema.columns.len();
        let on = eq_expr(col_ref(0, "id"), col_ref(left_width, "id"));
        plan = make_inner_join_on(plan, right, on);
    }

    // Verify the pre-check counts correctly
    assert_eq!(
        count_flattenable_relations(&plan),
        65,
        "should detect 65 flattenable relations"
    );

    let ctx = PlanningContext::empty();
    let result = reorder_joins(plan, &ctx);

    // Walk the result tree and verify no JoinCondition::None exists
    // at the top level (the n>=64 group should be intact).
    fn count_on_conditions(plan: &LogicalPlan) -> usize {
        match &plan.node {
            LogicalNode::Join {
                left,
                right,
                condition,
                ..
            } => {
                let here = if matches!(condition, JoinCondition::On(..)) {
                    1
                } else {
                    0
                };
                here + count_on_conditions(left) + count_on_conditions(right)
            }
            LogicalNode::Filter { input, .. } | LogicalNode::Project { input, .. } => {
                count_on_conditions(input)
            }
            _ => 0,
        }
    }

    // All 64 ON conditions must be preserved (not degraded to None)
    assert_eq!(
        count_on_conditions(&result),
        64,
        "all 64 ON conditions must be preserved, not degraded to cross joins"
    );
}

// ── Test: Values with unresolved subquery triggers safety gate ──

#[test]
fn test_values_unresolved_subquery_safety_gate() {
    // A Values node containing an InSubquery expression should be
    // detected by logical_has_unresolved_subquery.
    let subquery_expr = TypedExpr {
        kind: TypedExprKind::InSubquery {
            expr: Box::new(col_ref(0, "id")),
            subquery: Box::new(crate::sql::analyzer::types::AnalyzedQuery {
                ctes: vec![],
                body: crate::sql::analyzer::types::AnalyzedQueryBody::Values(vec![]),
                order_by: vec![],
                limit: None,
                offset: None,
                output_schema: vec![],
            }),
            negated: false,
        },
        data_type: DataType::Boolean,
    };

    let values_plan = LogicalPlan {
        node: LogicalNode::Values {
            rows: vec![vec![subquery_expr]],
        },
        schema: PlanSchema::from_columns(vec![("v".to_string(), DataType::Boolean)]),
    };

    assert!(
        logical_has_unresolved_subquery(&values_plan),
        "Values with InSubquery must be detected"
    );
}

// ── Test: TableFunction with unresolved subquery triggers safety gate ──

#[test]
fn test_table_function_unresolved_subquery_safety_gate() {
    let subquery_expr = TypedExpr {
        kind: TypedExprKind::InSubquery {
            expr: Box::new(col_ref(0, "id")),
            subquery: Box::new(crate::sql::analyzer::types::AnalyzedQuery {
                ctes: vec![],
                body: crate::sql::analyzer::types::AnalyzedQueryBody::Values(vec![]),
                order_by: vec![],
                limit: None,
                offset: None,
                output_schema: vec![],
            }),
            negated: false,
        },
        data_type: DataType::Boolean,
    };

    let tf_plan = LogicalPlan {
        node: LogicalNode::TableFunction {
            function_name: "generate_series".to_string(),
            args: vec![crate::sql::analyzer::types::TypedFunctionArg::Positional(
                subquery_expr,
            )],
            alias: None,
        },
        schema: PlanSchema::from_columns(vec![("v".to_string(), DataType::Int64)]),
    };

    assert!(
        logical_has_unresolved_subquery(&tf_plan),
        "TableFunction with InSubquery arg must be detected"
    );
}

// ── Test: Greedy disconnected stitching merges smallest-first ───

#[test]
fn test_greedy_disconnected_smallest_first() {
    // Build > 8 relations (to force greedy path) with two disconnected
    // groups. Verify the smallest component appears deepest (leftmost).
    //
    // Group 1: t0-t1-...-t8 (9 rels, star join centered on t0)
    // Group 2: s0 (1 rel, disconnected, tiny: 5 rows)
    // t0..t8 each have 100 rows.
    // Smallest-first stitching → s0 on the left of the cross join.
    let mut plan = make_scan("t0", None, vec![("id", DataType::Int64)]);
    for i in 1..9 {
        let right = make_scan(&format!("t{}", i), None, vec![("id", DataType::Int64)]);
        // Star: every table joins to t0.id (root col 0) via its own col
        let left_width = plan.schema.columns.len();
        let on = eq_expr(col_ref(0, "t0_id"), col_ref(left_width, "ti_id"));
        plan = make_inner_join_on(plan, right, on);
    }
    // Add disconnected s0 via cross join
    let s0 = make_scan("s0", None, vec![("id", DataType::Int64)]);
    plan = make_inner_join(plan, s0);

    let ctx = ctx_with_stats(vec![
        ("s0", 5, vec![("id", 5.0)]),
        ("t0", 100, vec![("id", 100.0)]),
        ("t1", 100, vec![("id", 100.0)]),
        ("t2", 100, vec![("id", 100.0)]),
        ("t3", 100, vec![("id", 100.0)]),
        ("t4", 100, vec![("id", 100.0)]),
        ("t5", 100, vec![("id", 100.0)]),
        ("t6", 100, vec![("id", 100.0)]),
        ("t7", 100, vec![("id", 100.0)]),
        ("t8", 100, vec![("id", 100.0)]),
    ]);

    let result = reorder_joins(plan, &ctx);
    let names = collect_scan_names(&result);
    assert_eq!(names.len(), 10, "should have 10 scans");

    // The smallest component (s0, 5 rows) should appear first in DFS order
    // because smallest-first stitching puts it on the left of the cross join.
    assert_eq!(
        names[0], "s0",
        "smallest component (s0) should be leftmost after smallest-first stitching, got {:?}",
        names
    );
}
