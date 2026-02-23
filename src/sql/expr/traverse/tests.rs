//\! Tests for TypedExpr tree traversal primitives.

use super::*;
use crate::sql::analyzer::types::{BinaryOp, FunctionKind, ResolvedFunction, WindowFrameUnits};
use crate::types::{DataType, Value};

fn int_const(v: i32) -> TypedExpr {
    TypedExpr::new(TypedExprKind::Constant(Value::Int32(v)), DataType::Int32)
}

fn col_ref(idx: usize, name: &str) -> TypedExpr {
    TypedExpr::new(
        TypedExprKind::ColumnRef {
            scope_depth: 0,
            column_index: idx,
            column_name: name.to_string(),
        },
        DataType::Int32,
    )
}

fn binary_add(left: TypedExpr, right: TypedExpr) -> TypedExpr {
    TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(left),
            op: BinaryOp::Add,
            right: Box::new(right),
        },
        DataType::Int32,
    )
}

#[test]
fn map_children_round_trip_identity() {
    // map_children with clone should produce equivalent kind
    let expr = binary_add(int_const(1), col_ref(0, "x"));
    let kind = map_children(&expr, &mut |child| child.clone());
    let rebuilt = TypedExpr {
        kind,
        data_type: expr.data_type.clone(),
    };
    assert!(matches!(rebuilt.kind, TypedExprKind::BinaryOp { .. }));
    if let TypedExprKind::BinaryOp { left, right, .. } = &rebuilt.kind {
        assert!(matches!(
            left.kind,
            TypedExprKind::Constant(Value::Int32(1))
        ));
        assert!(matches!(
            right.kind,
            TypedExprKind::ColumnRef {
                column_index: 0,
                ..
            }
        ));
    }
}

#[test]
fn visit_any_finds_nested_node() {
    let expr = binary_add(int_const(1), binary_add(int_const(2), col_ref(0, "x")));
    assert!(visit_any(&expr, |e| matches!(
        e.kind,
        TypedExprKind::ColumnRef { .. }
    )));
    assert!(!visit_any(&expr, |e| matches!(
        e.kind,
        TypedExprKind::Default
    )));
}

#[test]
fn visit_any_left_to_right_order() {
    // Verify that visit_any visits nodes in left-to-right DFS order
    let expr = binary_add(int_const(1), int_const(2));
    let mut visited = Vec::new();
    visit_any(&expr, |e| {
        if let TypedExprKind::Constant(Value::Int32(v)) = &e.kind {
            visited.push(*v);
        }
        false // never short-circuit
    });
    assert_eq!(visited, vec![1, 2]);
}

#[test]
fn visit_any_deep_tree_no_stack_overflow() {
    let mut expr = int_const(0);
    for i in 1..2048 {
        expr = binary_add(expr, int_const(i));
    }
    // Should not stack overflow
    assert!(!visit_any(&expr, |_| false));
}

#[test]
fn for_each_child_visits_window_frame_bound() {
    let expr = TypedExpr::new(
        TypedExprKind::WindowCall {
            func: ResolvedFunction {
                name: "sum".to_string(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Int64,
            },
            args: vec![int_const(1)],
            partition_by: vec![],
            order_by: vec![],
            window_frame: Some(WindowFrame {
                units: WindowFrameUnits::Rows,
                start: WindowFrameBound::Preceding(Some(Box::new(col_ref(0, "n")))),
                end: None,
            }),
        },
        DataType::Int64,
    );

    let mut found_col_ref = false;
    for_each_child(&expr, &mut |child| {
        if matches!(child.kind, TypedExprKind::ColumnRef { .. }) {
            found_col_ref = true;
        }
    });
    assert!(found_col_ref);
}

#[test]
fn for_each_child_subquery_opaque() {
    use crate::sql::analyzer::types::AnalyzedQueryBody;
    use crate::sql::analyzer::AnalyzedQuery;

    let subquery = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Values(vec![vec![int_const(42)]]),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![("v".to_string(), DataType::Int32, None)],
    };
    let expr = TypedExpr::new(
        TypedExprKind::ScalarSubquery(Box::new(subquery)),
        DataType::Int32,
    );

    let mut count = 0;
    for_each_child(&expr, &mut |_| count += 1);
    assert_eq!(count, 0, "ScalarSubquery should yield no children");
}

#[test]
fn for_each_child_like_escape() {
    let expr = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(col_ref(0, "x")),
            pattern: Box::new(int_const(1)),
            escape: Some(Box::new(int_const(2))),
            case_insensitive: false,
            negated: false,
        },
        DataType::Boolean,
    );
    let mut count = 0;
    for_each_child(&expr, &mut |_| count += 1);
    assert_eq!(count, 3, "Like with escape should yield 3 children");
}

#[test]
fn for_each_child_function_order_by_and_filter() {
    let expr = TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "array_agg".to_string(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Int32,
            },
            args: vec![col_ref(0, "x")],
            order_by: vec![TypedOrderByExpr {
                expr: col_ref(1, "y"),
                asc: true,
                nulls_first: false,
            }],
            filter: Some(Box::new(col_ref(2, "z"))),
        },
        DataType::Int32,
    );
    let mut children = Vec::new();
    for_each_child(&expr, &mut |child| {
        if let TypedExprKind::ColumnRef { column_name, .. } = &child.kind {
            children.push(column_name.clone());
        }
    });
    assert_eq!(children, vec!["x", "y", "z"]);
}
