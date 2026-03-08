//! Tests for physical planner.

use super::*;
use crate::model::DataType;
use crate::sql::analyzer::types::*;
use crate::sql::optimizer::logical_plan::{LogicalPlan, PlanSchema};
use crate::sql::optimizer::logical_planner::LogicalPlanner;
use crate::sql::optimizer::statistics::ColumnStatistics;

fn simple_column(name: &str, dt: DataType) -> TypedExpr {
    TypedExpr {
        kind: TypedExprKind::ColumnRef {
            scope_depth: 0,
            column_index: 0,
            column_name: name.to_string(),
        },
        data_type: dt,
    }
}

fn simple_constant(v: crate::model::Value, dt: DataType) -> TypedExpr {
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

// ── Test 35: SeqScan with stats ──────────────────────

#[test]
fn test_seqscan_with_stats() {
    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("users".to_string(), make_table_stats(5000, HashMap::new()));
    let scan = LogicalPlan::scan(
        "users".to_string(),
        None,
        PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
    );
    let physical = PhysicalPlanner::plan(&scan, &ctx);
    assert_eq!(physical.cost.rows, 5000);
}

// ── Test 36: SeqScan without stats ───────────────────

#[test]
fn test_seqscan_without_stats() {
    let ctx = PlanningContext::empty();
    let scan = LogicalPlan::scan(
        "users".to_string(),
        None,
        PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
    );
    let physical = PhysicalPlanner::plan(&scan, &ctx);
    assert_eq!(physical.cost.rows, DEFAULT_ESTIMATED_ROWS);
}

// ── Test 37: Filter with stats ───────────────────────

#[test]
fn test_filter_with_stats() {
    let mut cols = HashMap::new();
    cols.insert(
        "id".to_string(),
        ColumnStatistics {
            null_fraction: 0.0,
            n_distinct: 10000.0,
            avg_width: 4,
            most_common_vals: vec![],
            most_common_freqs: vec![],
            histogram_bounds: vec![],
            correlation: 0.0,
        },
    );
    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("t".to_string(), make_table_stats(10000, cols));

    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
    );
    let predicate = TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(simple_column("id", DataType::Int64)),
            op: BinaryOp::Eq,
            right: Box::new(simple_constant(
                crate::model::Value::Int32(1),
                DataType::Int64,
            )),
        },
        data_type: DataType::Boolean,
    };
    let filter = scan.filter(predicate);
    let physical = PhysicalPlanner::plan(&filter, &ctx);
    // sel = 1/10000 = 0.0001, rows = ceil(10000 * 0.0001) = 1
    assert_eq!(
        physical.cost.rows, 1,
        "filter with stats: got {}",
        physical.cost.rows
    );
}

// ── Test 38: Filter without stats ────────────────────

#[test]
fn test_filter_without_stats() {
    let ctx = PlanningContext::empty();
    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
    );
    let predicate = TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(simple_column("id", DataType::Int64)),
            op: BinaryOp::Eq,
            right: Box::new(simple_constant(
                crate::model::Value::Int32(1),
                DataType::Int64,
            )),
        },
        data_type: DataType::Boolean,
    };
    let filter = scan.filter(predicate);
    let physical = PhysicalPlanner::plan(&filter, &ctx);
    // Legacy: rows/3 = 1000/3 = 333
    assert_eq!(
        physical.cost.rows, 333,
        "filter without stats: got {}",
        physical.cost.rows
    );
}

// ── Test 39: HAVING filter (Filter above Aggregate) ──

#[test]
fn test_having_filter_uses_legacy() {
    let mut cols = HashMap::new();
    cols.insert("id".to_string(), make_col_stats(0.0, 10000.0));
    cols.insert("status".to_string(), make_col_stats(0.0, 50.0));
    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("t".to_string(), make_table_stats(10000, cols));

    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![
            ("id".to_string(), DataType::Int64),
            ("status".to_string(), DataType::Text),
        ]),
    );
    let agg = scan.aggregate(
        vec![simple_column("status", DataType::Text)],
        vec![simple_projection("status", DataType::Text)],
        PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
    );
    // HAVING filter sits above Aggregate
    let having_pred = TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(simple_column("cnt", DataType::Int64)),
            op: BinaryOp::Gt,
            right: Box::new(simple_constant(
                crate::model::Value::Int32(5),
                DataType::Int64,
            )),
        },
        data_type: DataType::Boolean,
    };
    let having = agg.filter(having_pred);
    let physical = PhysicalPlanner::plan(&having, &ctx);

    // Aggregate should use stats (50 groups), but HAVING filter should use
    // legacy /3 because resolve_stats returns None through Aggregate.
    // Aggregate rows = 50, HAVING rows = 50/3 = 16
    assert_eq!(
        physical.cost.rows,
        50_usize / 3,
        "HAVING: got {}",
        physical.cost.rows
    );
}

// ── Test 40: Aggregate with stats ────────────────────

#[test]
fn test_aggregate_with_stats() {
    let mut cols = HashMap::new();
    cols.insert("status".to_string(), make_col_stats(0.0, 50.0));
    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("t".to_string(), make_table_stats(10000, cols));

    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
    );
    let agg = scan.aggregate(
        vec![simple_column("status", DataType::Text)],
        vec![simple_projection("status", DataType::Text)],
        PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
    );
    let physical = PhysicalPlanner::plan(&agg, &ctx);
    assert_eq!(
        physical.cost.rows, 50,
        "agg with stats: got {}",
        physical.cost.rows
    );
}

// ── Test 41: Aggregate null-group ────────────────────

#[test]
fn test_aggregate_null_group() {
    let mut cols = HashMap::new();
    cols.insert("status".to_string(), make_col_stats(0.1, 50.0));
    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("t".to_string(), make_table_stats(10000, cols));

    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
    );
    let agg = scan.aggregate(
        vec![simple_column("status", DataType::Text)],
        vec![simple_projection("status", DataType::Text)],
        PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
    );
    let physical = PhysicalPlanner::plan(&agg, &ctx);
    // 50 non-null groups + 1 null group = 51
    assert_eq!(
        physical.cost.rows, 51,
        "agg null group: got {}",
        physical.cost.rows
    );
}

// ── Test 42: Aggregate all-NULL column ───────────────

#[test]
fn test_aggregate_all_null() {
    let mut cols = HashMap::new();
    cols.insert("status".to_string(), make_col_stats(1.0, 0.0));
    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("t".to_string(), make_table_stats(10000, cols));

    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
    );
    let agg = scan.aggregate(
        vec![simple_column("status", DataType::Text)],
        vec![simple_projection("status", DataType::Text)],
        PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
    );
    let physical = PhysicalPlanner::plan(&agg, &ctx);
    // n_distinct=0, null_frac=1.0 → 0 non-null groups + 1 null group = 1
    assert_eq!(
        physical.cost.rows, 1,
        "agg all-null: got {}",
        physical.cost.rows
    );
}

// ── Test 43: Aggregate without stats ─────────────────

#[test]
fn test_aggregate_without_stats() {
    let ctx = PlanningContext::empty();
    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
    );
    let agg = scan.aggregate(
        vec![simple_column("status", DataType::Text)],
        vec![simple_projection("status", DataType::Text)],
        PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
    );
    let physical = PhysicalPlanner::plan(&agg, &ctx);
    // Legacy: 1000/10 = 100
    assert_eq!(
        physical.cost.rows, 100,
        "agg no stats: got {}",
        physical.cost.rows
    );
}

// ── Test 44: End-to-end with stats ───────────────────

#[test]
fn test_end_to_end_with_stats() {
    let mut cols = HashMap::new();
    cols.insert(
        "id".to_string(),
        ColumnStatistics {
            null_fraction: 0.0,
            n_distinct: 5000.0,
            avg_width: 4,
            most_common_vals: vec![],
            most_common_freqs: vec![],
            histogram_bounds: vec![],
            correlation: 0.0,
        },
    );

    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("users".to_string(), make_table_stats(5000, cols));

    let query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![simple_projection("id", DataType::Int64)],
            from: vec![AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Table {
                    name: "users".to_string(),
                    schema: TableRefSchema {
                        table_id: 1,
                        columns: vec![("id".to_string(), DataType::Int64, false)],
                    },
                },
                alias: None,
            }],
            where_clause: Some(TypedExpr {
                kind: TypedExprKind::BinaryOp {
                    left: Box::new(simple_column("id", DataType::Int64)),
                    op: BinaryOp::Eq,
                    right: Box::new(simple_constant(
                        crate::model::Value::Int32(42),
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
        output_schema: vec![("id".to_string(), DataType::Int64, None)],
    };

    let logical = LogicalPlanner::build(&query).unwrap();
    let physical = PhysicalPlanner::plan(&logical, &ctx);

    // Scan should have 5000 rows (from stats)
    // Filter (eq on 5000 distinct) → sel = 1/5000 → 1 row
    // Verify the plan is sensible
    assert!(
        physical.cost.rows < 100,
        "e2e: rows should be small, got {}",
        physical.cost.rows
    );
}

// ── Test 45: Existing tests still pass with empty ctx ─

#[test]
fn test_single_table_physical() {
    let query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![simple_projection("id", DataType::Int64)],
            from: vec![AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Table {
                    name: "users".to_string(),
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
        output_schema: vec![("id".to_string(), DataType::Int64, None)],
    };

    let logical = LogicalPlanner::build(&query).unwrap();
    let physical = PhysicalPlanner::plan(&logical, &PlanningContext::empty());

    assert!(matches!(physical.node, PhysicalNode::Project { .. }));
    if let PhysicalNode::Project { input, .. } = &physical.node {
        assert!(matches!(input.node, PhysicalNode::SeqScan { .. }));
    }
    assert!(physical.cost.total > 0.0);
}

#[test]
fn test_topn_optimization() {
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
            crate::model::Value::Int64(10),
            DataType::Int64,
        )),
        offset: None,
        output_schema: vec![("id".to_string(), DataType::Int64, None)],
    };

    let logical = LogicalPlanner::build(&query).unwrap();
    let physical = PhysicalPlanner::plan(&logical, &PlanningContext::empty());

    fn has_topn(plan: &PhysicalPlan) -> bool {
        match &plan.node {
            PhysicalNode::TopNSort { .. } => true,
            PhysicalNode::Limit { input, .. }
            | PhysicalNode::Filter { input, .. }
            | PhysicalNode::Project { input, .. }
            | PhysicalNode::Sort { input, .. } => has_topn(input),
            _ => false,
        }
    }
    assert!(has_topn(&physical), "expected TopNSort for small LIMIT");
}

#[test]
fn test_hash_aggregate() {
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
            ("status".to_string(), DataType::Text, None),
            ("count".to_string(), DataType::Int64, None),
        ],
    };

    let logical = LogicalPlanner::build(&query).unwrap();
    let physical = PhysicalPlanner::plan(&logical, &PlanningContext::empty());

    assert!(matches!(physical.node, PhysicalNode::HashAggregate { .. }));
}

// ── Gate tests ───────────────────────────────────────

#[test]
fn test_gate1_stats_improve_estimates() {
    let mut cols = HashMap::new();
    cols.insert(
        "id".to_string(),
        ColumnStatistics {
            null_fraction: 0.0,
            n_distinct: 100.0,
            avg_width: 4,
            most_common_vals: vec![],
            most_common_freqs: vec![],
            histogram_bounds: vec![],
            correlation: 0.0,
        },
    );
    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("t".to_string(), make_table_stats(10000, cols));

    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
    );
    let predicate = TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(simple_column("id", DataType::Int64)),
            op: BinaryOp::Eq,
            right: Box::new(simple_constant(
                crate::model::Value::Int32(1),
                DataType::Int64,
            )),
        },
        data_type: DataType::Boolean,
    };
    let filter = scan.filter(predicate);
    let physical = PhysicalPlanner::plan(&filter, &ctx);
    // sel = 1/100 = 0.01, rows = ceil(10000 * 0.01) = 100
    assert_eq!(physical.cost.rows, 100, "gate1: got {}", physical.cost.rows);
}

#[test]
fn test_gate2_no_stats_exact_legacy() {
    let ctx = PlanningContext::empty();
    // Scan
    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
    );
    let scan_phys = PhysicalPlanner::plan(&scan, &ctx);
    assert_eq!(scan_phys.cost.rows, 1000);

    // Filter
    let scan2 = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
    );
    let filter = scan2.filter(TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(simple_column("id", DataType::Int64)),
            op: BinaryOp::Eq,
            right: Box::new(simple_constant(
                crate::model::Value::Int32(1),
                DataType::Int64,
            )),
        },
        data_type: DataType::Boolean,
    });
    let filter_phys = PhysicalPlanner::plan(&filter, &ctx);
    assert_eq!(filter_phys.cost.rows, 333);

    // Aggregate
    let scan3 = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
    );
    let agg = scan3.aggregate(
        vec![simple_column("status", DataType::Text)],
        vec![simple_projection("status", DataType::Text)],
        PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
    );
    let agg_phys = PhysicalPlanner::plan(&agg, &ctx);
    assert_eq!(agg_phys.cost.rows, 100);
}

#[test]
fn test_gate3_selectivity_bounds() {
    let mut cols = HashMap::new();
    cols.insert("id".to_string(), make_col_stats(0.0, 100.0));
    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("t".to_string(), make_table_stats(100, cols));

    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
    );
    let filter = scan.filter(TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(simple_column("id", DataType::Int64)),
            op: BinaryOp::Eq,
            right: Box::new(simple_constant(
                crate::model::Value::Int32(1),
                DataType::Int64,
            )),
        },
        data_type: DataType::Boolean,
    });
    let physical = PhysicalPlanner::plan(&filter, &ctx);
    // Rows can be 0 — no forced .max(1)
    assert!(physical.cost.rows <= 100);
}

#[test]
fn test_gate4_negative_n_distinct() {
    let mut cols = HashMap::new();
    cols.insert(
        "id".to_string(),
        ColumnStatistics {
            null_fraction: 0.0,
            n_distinct: -0.5,
            avg_width: 4,
            most_common_vals: vec![],
            most_common_freqs: vec![],
            histogram_bounds: vec![],
            correlation: 0.0,
        },
    );
    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("t".to_string(), make_table_stats(2000, cols));

    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
    );
    let filter = scan.filter(TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(simple_column("id", DataType::Int64)),
            op: BinaryOp::Eq,
            right: Box::new(simple_constant(
                crate::model::Value::Int32(1),
                DataType::Int64,
            )),
        },
        data_type: DataType::Boolean,
    });
    let physical = PhysicalPlanner::plan(&filter, &ctx);
    // eff = 0.5 * 2000 = 1000, sel = 1/1000, rows = ceil(2000 * 0.001) = 2
    assert_eq!(physical.cost.rows, 2, "gate4: got {}", physical.cost.rows);
}

#[test]
fn test_gate5_having_isolation() {
    let mut cols = HashMap::new();
    cols.insert("status".to_string(), make_col_stats(0.0, 50.0));
    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("t".to_string(), make_table_stats(10000, cols));

    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
    );
    let agg = scan.aggregate(
        vec![simple_column("status", DataType::Text)],
        vec![simple_projection("status", DataType::Text)],
        PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
    );
    let agg_phys = PhysicalPlanner::plan(&agg, &ctx);
    assert_eq!(agg_phys.cost.rows, 50, "agg uses stats");

    // HAVING filter above aggregate uses legacy
    let having = agg.filter(TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(simple_column("cnt", DataType::Int64)),
            op: BinaryOp::Gt,
            right: Box::new(simple_constant(
                crate::model::Value::Int32(5),
                DataType::Int64,
            )),
        },
        data_type: DataType::Boolean,
    });
    let having_phys = PhysicalPlanner::plan(&having, &ctx);
    assert_eq!(having_phys.cost.rows, 50_usize / 3, "HAVING uses legacy /3");
}

#[test]
fn test_gate6_null_constant_zero_selectivity() {
    let mut cols = HashMap::new();
    cols.insert("id".to_string(), make_col_stats(0.1, 100.0));
    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("t".to_string(), make_table_stats(1000, cols));

    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
    );
    let filter = scan.filter(TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(simple_column("id", DataType::Int64)),
            op: BinaryOp::Eq,
            right: Box::new(simple_constant(crate::model::Value::Null, DataType::Int64)),
        },
        data_type: DataType::Boolean,
    });
    let physical = PhysicalPlanner::plan(&filter, &ctx);
    assert_eq!(physical.cost.rows, 0, "NULL eq = 0 rows");
}

#[test]
fn test_gate7_group_by_null_group() {
    let mut cols = HashMap::new();
    cols.insert("status".to_string(), make_col_stats(1.0, 0.0));
    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("t".to_string(), make_table_stats(10000, cols));

    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
    );
    let agg = scan.aggregate(
        vec![simple_column("status", DataType::Text)],
        vec![simple_projection("status", DataType::Text)],
        PlanSchema::from_columns(vec![("status".to_string(), DataType::Text)]),
    );
    let physical = PhysicalPlanner::plan(&agg, &ctx);
    assert_eq!(
        physical.cost.rows, 1,
        "null-group: got {}",
        physical.cost.rows
    );
}

#[test]
fn test_gate8_negated_predicates_null_safe() {
    let mut cols = HashMap::new();
    cols.insert(
        "id".to_string(),
        ColumnStatistics {
            null_fraction: 0.2,
            n_distinct: 100.0,
            avg_width: 4,
            most_common_vals: vec![crate::model::Value::Int32(1)],
            most_common_freqs: vec![0.1],
            histogram_bounds: vec![],
            correlation: 0.0,
        },
    );
    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("t".to_string(), make_table_stats(1000, cols));

    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
    );
    let filter = scan.filter(TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(simple_column("id", DataType::Int64)),
            op: BinaryOp::NotEq,
            right: Box::new(simple_constant(
                crate::model::Value::Int32(1),
                DataType::Int64,
            )),
        },
        data_type: DataType::Boolean,
    });
    let physical = PhysicalPlanner::plan(&filter, &ctx);
    // sel ≈ (1.0 - 0.2) - 0.1 ≈ 0.7, rows ≈ 700 (ceil may round up by 1
    // due to IEEE 754 intermediate rounding)
    assert!(
        (700..=701).contains(&physical.cost.rows),
        "gate8: got {}",
        physical.cost.rows
    );
}

// ── Join algorithm selection + cardinality tests ─────

use crate::sql::optimizer::logical_plan::LogicalNode;

fn make_join_plan(
    left_rows: usize,
    right_rows: usize,
    join_type: JoinType,
    condition: JoinCondition,
) -> (LogicalPlan, PlanningContext) {
    let left = LogicalPlan::scan(
        "left_t".to_string(),
        None,
        PlanSchema::from_columns(vec![
            ("id".to_string(), DataType::Int64),
            ("val".to_string(), DataType::Text),
        ]),
    );
    let right = LogicalPlan::scan(
        "right_t".to_string(),
        None,
        PlanSchema::from_columns(vec![
            ("id".to_string(), DataType::Int64),
            ("name".to_string(), DataType::Text),
        ]),
    );
    let mut schema_cols = left.schema.columns.clone();
    schema_cols.extend(right.schema.columns.clone());
    let join = LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left),
            right: Box::new(right),
            join_type,
            condition,
        },
        schema: PlanSchema::from_columns(schema_cols),
    };
    let mut ctx = PlanningContext::empty();
    ctx.table_stats.insert(
        "left_t".to_string(),
        make_table_stats(left_rows, HashMap::new()),
    );
    ctx.table_stats.insert(
        "right_t".to_string(),
        make_table_stats(right_rows, HashMap::new()),
    );
    (join, ctx)
}

fn equi_on_condition() -> JoinCondition {
    // col[0] = col[2] (left.id = right.id, left_width=2)
    JoinCondition::On(TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(TypedExpr {
                kind: TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: 0,
                    column_name: "id".to_string(),
                },
                data_type: DataType::Int64,
            }),
            op: BinaryOp::Eq,
            right: Box::new(TypedExpr {
                kind: TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: 2,
                    column_name: "id".to_string(),
                },
                data_type: DataType::Int64,
            }),
        },
        data_type: DataType::Boolean,
    })
}

fn non_equi_on_condition() -> JoinCondition {
    // col[0] > col[2]
    JoinCondition::On(TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(TypedExpr {
                kind: TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: 0,
                    column_name: "id".to_string(),
                },
                data_type: DataType::Int64,
            }),
            op: BinaryOp::Gt,
            right: Box::new(TypedExpr {
                kind: TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: 2,
                    column_name: "id".to_string(),
                },
                data_type: DataType::Int64,
            }),
        },
        data_type: DataType::Boolean,
    })
}

fn mixed_on_condition() -> JoinCondition {
    // col[0] = col[2] AND col[1] > col[3]
    JoinCondition::On(TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(TypedExpr {
                kind: TypedExprKind::BinaryOp {
                    left: Box::new(TypedExpr {
                        kind: TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 0,
                            column_name: "id".to_string(),
                        },
                        data_type: DataType::Int64,
                    }),
                    op: BinaryOp::Eq,
                    right: Box::new(TypedExpr {
                        kind: TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 2,
                            column_name: "id".to_string(),
                        },
                        data_type: DataType::Int64,
                    }),
                },
                data_type: DataType::Boolean,
            }),
            op: BinaryOp::And,
            right: Box::new(TypedExpr {
                kind: TypedExprKind::BinaryOp {
                    left: Box::new(TypedExpr {
                        kind: TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 1,
                            column_name: "val".to_string(),
                        },
                        data_type: DataType::Text,
                    }),
                    op: BinaryOp::Gt,
                    right: Box::new(TypedExpr {
                        kind: TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 3,
                            column_name: "name".to_string(),
                        },
                        data_type: DataType::Text,
                    }),
                },
                data_type: DataType::Boolean,
            }),
        },
        data_type: DataType::Boolean,
    })
}

fn correlated_array_subquery_arg() -> TypedExpr {
    let subquery = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Values(vec![vec![TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 1,
                column_index: 0,
                column_name: "id".to_string(),
            },
            data_type: DataType::Int64,
        }]]),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![("v".to_string(), DataType::Int64, None)],
    };
    TypedExpr {
        kind: TypedExprKind::ArraySubquery(Box::new(subquery)),
        data_type: DataType::Array(Box::new(DataType::Int64)),
    }
}

#[test]
fn test_hash_join_for_equi() {
    let (join, ctx) = make_join_plan(1000, 1000, JoinType::Inner, equi_on_condition());
    let physical = PhysicalPlanner::plan(&join, &ctx);
    assert!(
        matches!(physical.node, PhysicalNode::HashJoin { .. }),
        "equi-join should produce HashJoin, got {:?}",
        std::mem::discriminant(&physical.node)
    );
}

#[test]
fn test_nlj_for_non_equi() {
    let (join, ctx) = make_join_plan(1000, 1000, JoinType::Inner, non_equi_on_condition());
    let physical = PhysicalPlanner::plan(&join, &ctx);
    assert!(
        matches!(physical.node, PhysicalNode::NestedLoopJoin { .. }),
        "non-equi should produce NLJ"
    );
}

#[test]
fn test_hash_join_for_mixed_equi_and_residual() {
    let (join, ctx) = make_join_plan(1000, 1000, JoinType::Inner, mixed_on_condition());
    let physical = PhysicalPlanner::plan(&join, &ctx);
    assert!(
        matches!(physical.node, PhysicalNode::HashJoin { .. }),
        "mixed (equi + residual) should produce HashJoin with residual filter"
    );
}

#[test]
fn test_nlj_for_equi_when_table_function_arg_has_correlated_subquery() {
    let left = LogicalPlan::scan(
        "left_t".to_string(),
        None,
        PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
    );
    let right = LogicalPlan {
        node: LogicalNode::TableFunction {
            function_name: "unnest".to_string(),
            args: vec![TypedFunctionArg::Positional(correlated_array_subquery_arg())],
            alias: Some("u".to_string()),
        },
        schema: PlanSchema::from_columns(vec![("val".to_string(), DataType::Int64)]),
    };
    let mut schema_cols = left.schema.columns.clone();
    schema_cols.extend(right.schema.columns.clone());
    let join = LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left),
            right: Box::new(right),
            join_type: JoinType::Inner,
            condition: JoinCondition::On(TypedExpr {
                kind: TypedExprKind::BinaryOp {
                    left: Box::new(TypedExpr {
                        kind: TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 0,
                            column_name: "id".to_string(),
                        },
                        data_type: DataType::Int64,
                    }),
                    op: BinaryOp::Eq,
                    right: Box::new(TypedExpr {
                        kind: TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 1,
                            column_name: "val".to_string(),
                        },
                        data_type: DataType::Int64,
                    }),
                },
                data_type: DataType::Boolean,
            }),
        },
        schema: PlanSchema::from_columns(schema_cols),
    };

    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("left_t".to_string(), make_table_stats(1000, HashMap::new()));

    let physical = PhysicalPlanner::plan(&join, &ctx);
    assert!(
        matches!(physical.node, PhysicalNode::NestedLoopJoin { .. }),
        "correlated table-function RHS must force NLJ"
    );
}

#[test]
fn test_nlj_for_cross() {
    let (join, ctx) = make_join_plan(1000, 1000, JoinType::Cross, JoinCondition::None);
    let physical = PhysicalPlanner::plan(&join, &ctx);
    assert!(
        matches!(physical.node, PhysicalNode::NestedLoopJoin { .. }),
        "cross join should produce NLJ"
    );
}

#[test]
fn test_cardinality_with_stats() {
    // Left=10000 rows, NDV(id)=100. Right=5000 rows, NDV(id)=200.
    // sel = 1/max(100,200) = 1/200, rows = ceil(10000 * 5000 / 200) = 250000.
    let left = LogicalPlan::scan(
        "left_t".to_string(),
        None,
        PlanSchema::from_columns(vec![
            ("id".to_string(), DataType::Int64),
            ("val".to_string(), DataType::Text),
        ]),
    );
    let right = LogicalPlan::scan(
        "right_t".to_string(),
        None,
        PlanSchema::from_columns(vec![
            ("id".to_string(), DataType::Int64),
            ("name".to_string(), DataType::Text),
        ]),
    );
    let mut schema_cols = left.schema.columns.clone();
    schema_cols.extend(right.schema.columns.clone());
    let join = LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left),
            right: Box::new(right),
            join_type: JoinType::Inner,
            condition: equi_on_condition(),
        },
        schema: PlanSchema::from_columns(schema_cols),
    };

    let mut left_cols = HashMap::new();
    left_cols.insert("id".to_string(), make_col_stats(0.0, 100.0));
    let mut right_cols = HashMap::new();
    right_cols.insert("id".to_string(), make_col_stats(0.0, 200.0));

    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("left_t".to_string(), make_table_stats(10000, left_cols));
    ctx.table_stats
        .insert("right_t".to_string(), make_table_stats(5000, right_cols));

    let physical = PhysicalPlanner::plan(&join, &ctx);
    assert_eq!(
        physical.cost.rows, 250000,
        "cardinality with stats: got {}",
        physical.cost.rows
    );
}

#[test]
fn test_cardinality_no_stats() {
    // Both sides default 1000 rows, no stats → DEFAULT_JOIN_SEL = 0.1
    // rows = ceil(1000 * 1000 * 0.1) = 100000
    let (join, ctx) = make_join_plan(1000, 1000, JoinType::Inner, equi_on_condition());
    let physical = PhysicalPlanner::plan(&join, &ctx);
    assert_eq!(
        physical.cost.rows, 100000,
        "cardinality no stats: got {}",
        physical.cost.rows
    );
}

#[test]
fn test_left_join_lower_bound() {
    // LEFT JOIN: L=1000, R=10 with NDV(id)=1000 → inner_est = ceil(1000*10/1000) = 10.
    // But LEFT JOIN must return >= left_rows=1000.
    let left = LogicalPlan::scan(
        "left_t".to_string(),
        None,
        PlanSchema::from_columns(vec![
            ("id".to_string(), DataType::Int64),
            ("val".to_string(), DataType::Text),
        ]),
    );
    let right = LogicalPlan::scan(
        "right_t".to_string(),
        None,
        PlanSchema::from_columns(vec![
            ("id".to_string(), DataType::Int64),
            ("name".to_string(), DataType::Text),
        ]),
    );
    let mut schema_cols = left.schema.columns.clone();
    schema_cols.extend(right.schema.columns.clone());
    let join = LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left),
            right: Box::new(right),
            join_type: JoinType::Left,
            condition: equi_on_condition(),
        },
        schema: PlanSchema::from_columns(schema_cols),
    };

    let mut left_cols = HashMap::new();
    left_cols.insert("id".to_string(), make_col_stats(0.0, 1000.0));
    let mut right_cols = HashMap::new();
    right_cols.insert("id".to_string(), make_col_stats(0.0, 1000.0));

    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("left_t".to_string(), make_table_stats(1000, left_cols));
    ctx.table_stats
        .insert("right_t".to_string(), make_table_stats(10, right_cols));

    let physical = PhysicalPlanner::plan(&join, &ctx);
    assert!(
        physical.cost.rows >= 1000,
        "LEFT JOIN lower bound: got {}",
        physical.cost.rows
    );
}

#[test]
fn test_right_join_lower_bound() {
    // RIGHT JOIN: must return >= right_rows.
    let left = LogicalPlan::scan(
        "left_t".to_string(),
        None,
        PlanSchema::from_columns(vec![
            ("id".to_string(), DataType::Int64),
            ("val".to_string(), DataType::Text),
        ]),
    );
    let right = LogicalPlan::scan(
        "right_t".to_string(),
        None,
        PlanSchema::from_columns(vec![
            ("id".to_string(), DataType::Int64),
            ("name".to_string(), DataType::Text),
        ]),
    );
    let mut schema_cols = left.schema.columns.clone();
    schema_cols.extend(right.schema.columns.clone());
    let join = LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left),
            right: Box::new(right),
            join_type: JoinType::Right,
            condition: equi_on_condition(),
        },
        schema: PlanSchema::from_columns(schema_cols),
    };

    let mut left_cols = HashMap::new();
    left_cols.insert("id".to_string(), make_col_stats(0.0, 1000.0));
    let mut right_cols = HashMap::new();
    right_cols.insert("id".to_string(), make_col_stats(0.0, 1000.0));

    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("left_t".to_string(), make_table_stats(10, left_cols));
    ctx.table_stats
        .insert("right_t".to_string(), make_table_stats(1000, right_cols));

    let physical = PhysicalPlanner::plan(&join, &ctx);
    assert!(
        physical.cost.rows >= 1000,
        "RIGHT JOIN lower bound: got {}",
        physical.cost.rows
    );
}

#[test]
fn test_full_join_lower_bound() {
    // FULL JOIN: must return >= max(left_rows, right_rows).
    let left = LogicalPlan::scan(
        "left_t".to_string(),
        None,
        PlanSchema::from_columns(vec![
            ("id".to_string(), DataType::Int64),
            ("val".to_string(), DataType::Text),
        ]),
    );
    let right = LogicalPlan::scan(
        "right_t".to_string(),
        None,
        PlanSchema::from_columns(vec![
            ("id".to_string(), DataType::Int64),
            ("name".to_string(), DataType::Text),
        ]),
    );
    let mut schema_cols = left.schema.columns.clone();
    schema_cols.extend(right.schema.columns.clone());
    let join = LogicalPlan {
        node: LogicalNode::Join {
            left: Box::new(left),
            right: Box::new(right),
            join_type: JoinType::Full,
            condition: equi_on_condition(),
        },
        schema: PlanSchema::from_columns(schema_cols),
    };

    let mut left_cols = HashMap::new();
    left_cols.insert("id".to_string(), make_col_stats(0.0, 1000.0));
    let mut right_cols = HashMap::new();
    right_cols.insert("id".to_string(), make_col_stats(0.0, 1000.0));

    let mut ctx = PlanningContext::empty();
    ctx.table_stats
        .insert("left_t".to_string(), make_table_stats(500, left_cols));
    ctx.table_stats
        .insert("right_t".to_string(), make_table_stats(800, right_cols));

    let physical = PhysicalPlanner::plan(&join, &ctx);
    assert!(
        physical.cost.rows >= 800,
        "FULL JOIN lower bound: got {}",
        physical.cost.rows
    );
}

#[test]
fn test_cross_join_cartesian() {
    // Cross join: no selectivity reduction → rows = L * R
    let (join, ctx) = make_join_plan(100, 200, JoinType::Cross, JoinCondition::None);
    let physical = PhysicalPlanner::plan(&join, &ctx);
    assert_eq!(
        physical.cost.rows, 20000,
        "cross join cartesian: got {}",
        physical.cost.rows
    );
}

#[test]
fn test_build_side_smaller() {
    // L=100, R=10000 → left is smaller → left_is_build = true
    let (join, ctx) = make_join_plan(100, 10000, JoinType::Inner, equi_on_condition());
    let physical = PhysicalPlanner::plan(&join, &ctx);
    if let PhysicalNode::HashJoin { left_is_build, .. } = &physical.node {
        assert!(
            *left_is_build,
            "smaller left should be build side, got left_is_build=false"
        );
    } else {
        panic!("expected HashJoin");
    }
}

// ── Access-path selection tests ─────────────────────

use crate::model::{ColumnDef, IndexDef};

fn make_schema_with_index() -> TableSchema {
    let mut schema = TableSchema::new(
        "t".to_string(),
        1,
        vec![
            ColumnDef {
                name: "id".to_string(),
                data_type: DataType::Int64,
                nullable: false,
                primary_key: true,
                unique: true,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
            },
            ColumnDef {
                name: "name".to_string(),
                data_type: DataType::Text,
                nullable: true,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                generation_expr: None,
                generation_expr_authorized_by: None,
                collation: None,
            },
        ],
        vec![0],
    );
    schema.indexes.push(IndexDef {
        name: "idx_t_id".to_string(),
        id: 100,
        columns: vec!["id".to_string()],
        unique: true,
        is_constraint: false,
        method: Some("btree".to_string()),
        predicate: None,
        expressions: vec![],
        state: crate::worker::types::IndexState::Ready,
        cached_predicate_conjuncts: None,
        hnsw_m: None,
        hnsw_ef_construction: None,
        hnsw_distance_metric: None,
    });
    schema
}

#[test]
fn test_filter_above_scan_selects_index() {
    // Filter(id = 42) above Scan("t") with btree index on id
    // → should produce IndexScan, not SeqScan
    let schema = make_schema_with_index();
    let mut ctx = PlanningContext::empty();
    ctx.table_schemas.insert("t".to_string(), schema);
    // Also add stats so row estimates are realistic
    ctx.table_stats
        .insert("t".to_string(), make_table_stats(10000, HashMap::new()));

    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![
            ("id".to_string(), DataType::Int64),
            ("name".to_string(), DataType::Text),
        ]),
    );
    let predicate = TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(simple_column("id", DataType::Int64)),
            op: BinaryOp::Eq,
            right: Box::new(simple_constant(
                crate::model::Value::Int32(42),
                DataType::Int64,
            )),
        },
        data_type: DataType::Boolean,
    };
    let filter = scan.filter(predicate);
    let physical = PhysicalPlanner::plan(&filter, &ctx);

    // Outermost should be Filter
    if let PhysicalNode::Filter { input, .. } = &physical.node {
        assert!(
            matches!(input.node, PhysicalNode::IndexScan { .. }),
            "expected IndexScan under Filter, got {:?}",
            std::mem::discriminant(&input.node)
        );
        if let PhysicalNode::IndexScan { scan_type, .. } = &input.node {
            assert!(
                matches!(scan_type, crate::sql::planner::ScanType::IndexScan { .. }),
                "expected point-lookup IndexScan, got {:?}",
                scan_type
            );
        }
    } else {
        panic!(
            "expected Filter, got {:?}",
            std::mem::discriminant(&physical.node)
        );
    }
}

#[test]
fn test_filter_above_scan_no_schema_stays_seqscan() {
    // Filter above Scan without schema in context → stays SeqScan
    let ctx = PlanningContext::empty();
    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
    );
    let predicate = TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(simple_column("id", DataType::Int64)),
            op: BinaryOp::Eq,
            right: Box::new(simple_constant(
                crate::model::Value::Int32(42),
                DataType::Int64,
            )),
        },
        data_type: DataType::Boolean,
    };
    let filter = scan.filter(predicate);
    let physical = PhysicalPlanner::plan(&filter, &ctx);

    if let PhysicalNode::Filter { input, .. } = &physical.node {
        assert!(
            matches!(input.node, PhysicalNode::SeqScan { .. }),
            "expected SeqScan without schema metadata"
        );
    } else {
        panic!("expected Filter");
    }
}

#[test]
fn test_filter_above_scan_no_matching_index_stays_seqscan() {
    // Filter on column 'name' but only index on 'id' → SeqScan
    let schema = make_schema_with_index();
    let mut ctx = PlanningContext::empty();
    ctx.table_schemas.insert("t".to_string(), schema);

    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![
            ("id".to_string(), DataType::Int64),
            ("name".to_string(), DataType::Text),
        ]),
    );
    let predicate = TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(simple_column("name", DataType::Text)),
            op: BinaryOp::Eq,
            right: Box::new(simple_constant(
                crate::model::Value::Text("alice".to_string()),
                DataType::Text,
            )),
        },
        data_type: DataType::Boolean,
    };
    let filter = scan.filter(predicate);
    let physical = PhysicalPlanner::plan(&filter, &ctx);

    if let PhysicalNode::Filter { input, .. } = &physical.node {
        assert!(
            matches!(input.node, PhysicalNode::SeqScan { .. }),
            "expected SeqScan when no index matches filter column"
        );
    } else {
        panic!("expected Filter");
    }
}

#[test]
fn test_filter_above_scan_range_predicate() {
    // Filter(id > 100) with btree index → should produce IndexScan (range)
    let schema = make_schema_with_index();
    let mut ctx = PlanningContext::empty();
    ctx.table_schemas.insert("t".to_string(), schema);
    ctx.table_stats
        .insert("t".to_string(), make_table_stats(10000, HashMap::new()));

    let scan = LogicalPlan::scan(
        "t".to_string(),
        None,
        PlanSchema::from_columns(vec![("id".to_string(), DataType::Int64)]),
    );
    let predicate = TypedExpr {
        kind: TypedExprKind::BinaryOp {
            left: Box::new(simple_column("id", DataType::Int64)),
            op: BinaryOp::Gt,
            right: Box::new(simple_constant(
                crate::model::Value::Int32(100),
                DataType::Int64,
            )),
        },
        data_type: DataType::Boolean,
    };
    let filter = scan.filter(predicate);
    let physical = PhysicalPlanner::plan(&filter, &ctx);

    if let PhysicalNode::Filter { input, .. } = &physical.node {
        assert!(
            matches!(input.node, PhysicalNode::IndexScan { .. }),
            "expected IndexScan for range predicate, got {:?}",
            std::mem::discriminant(&input.node)
        );
    } else {
        panic!("expected Filter");
    }
}
