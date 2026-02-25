//! Unit tests for subquery analysis helpers (correlation detection, substitution,
//! WHERE clause splitting).

use super::*;
use crate::model::DataType;
use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedProjection, AnalyzedQueryBody, AnalyzedTableRef,
    AnalyzedTableRefKind, BinaryOp, FunctionKind, ResolvedFunction, TypedFunctionArg,
};

fn int_const(v: i32) -> TypedExpr {
    TypedExpr::new(TypedExprKind::Constant(Value::Int32(v)), DataType::Int32)
}

fn scalar_values_query(v: i32) -> AnalyzedQuery {
    AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Values(vec![vec![int_const(v)]]),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![("v".to_string(), DataType::Int32, None)],
    }
}

#[test]
fn has_outer_ref_recurses_into_any_all_lhs() {
    let expr = TypedExpr::new(
        TypedExprKind::AnyAll {
            expr: Box::new(TypedExpr::new(
                TypedExprKind::ColumnRef {
                    scope_depth: 1,
                    column_index: 0,
                    column_name: "x".to_string(),
                },
                DataType::Int32,
            )),
            op: BinaryOp::Eq,
            subquery: Box::new(scalar_values_query(1)),
            is_all: false,
        },
        DataType::Boolean,
    );

    assert!(has_outer_ref(&expr));
}

#[test]
fn is_correlated_query_detects_outer_ref_inside_any_all() {
    let where_expr = TypedExpr::new(
        TypedExprKind::AnyAll {
            expr: Box::new(TypedExpr::new(
                TypedExprKind::ColumnRef {
                    scope_depth: 1,
                    column_index: 0,
                    column_name: "outer_x".to_string(),
                },
                DataType::Int32,
            )),
            op: BinaryOp::Eq,
            subquery: Box::new(scalar_values_query(1)),
            is_all: false,
        },
        DataType::Boolean,
    );

    let query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![AnalyzedProjection {
                expr: int_const(1),
                output_name: "?column?".to_string(),
            }],
            from: vec![],
            where_clause: Some(where_expr),
            group_by: vec![],
            having: None,
            distinct: AnalyzedDistinct::All,
        }),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![("?column?".to_string(), DataType::Int32, None)],
    };

    assert!(is_correlated_query(&query));
}

#[test]
fn is_correlated_query_detects_outer_ref_in_table_function_args() {
    let query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![AnalyzedProjection {
                expr: int_const(1),
                output_name: "?column?".to_string(),
            }],
            from: vec![AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Function {
                    func: ResolvedFunction {
                        name: "generate_series".to_string(),
                        kind: FunctionKind::Builtin,
                        return_type: DataType::Int32,
                    },
                    args: vec![
                        TypedFunctionArg::Positional(int_const(1)),
                        TypedFunctionArg::Positional(TypedExpr::new(
                            TypedExprKind::ColumnRef {
                                scope_depth: 1,
                                column_index: 0,
                                column_name: "outer_n".to_string(),
                            },
                            DataType::Int32,
                        )),
                    ],
                    output_columns: vec![("generate_series".to_string(), DataType::Int32)],
                },
                alias: Some("gs".to_string()),
            }],
            where_clause: None,
            group_by: vec![],
            having: None,
            distinct: AnalyzedDistinct::All,
        }),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![("?column?".to_string(), DataType::Int32, None)],
    };

    assert!(is_correlated_query(&query));
}

fn correlated_values_query() -> AnalyzedQuery {
    AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![AnalyzedProjection {
                expr: TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 1,
                        column_index: 0,
                        column_name: "outer_x".to_string(),
                    },
                    DataType::Int32,
                ),
                output_name: "outer_x".to_string(),
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
        output_schema: vec![("outer_x".to_string(), DataType::Int32, None)],
    }
}

#[test]
fn is_correlated_query_detects_scalar_subquery_correlation() {
    assert!(is_correlated_query(&correlated_values_query()));
}

#[test]
fn is_correlated_query_does_not_treat_nested_correlation_as_outer_ref() {
    let expr = TypedExpr::new(
        TypedExprKind::ScalarSubquery(Box::new(correlated_values_query())),
        DataType::Int32,
    );
    let where_expr = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(expr),
            op: BinaryOp::Eq,
            right: Box::new(int_const(1)),
        },
        DataType::Boolean,
    );

    let query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![AnalyzedProjection {
                expr: int_const(1),
                output_name: "?column?".to_string(),
            }],
            from: vec![],
            where_clause: Some(where_expr),
            group_by: vec![],
            having: None,
            distinct: AnalyzedDistinct::All,
        }),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![("?column?".to_string(), DataType::Int32, None)],
    };

    assert!(!is_correlated_query(&query));
}

#[test]
fn is_correlated_query_detects_nested_ref_beyond_current_scope() {
    let nested = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![AnalyzedProjection {
                expr: TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 2,
                        column_index: 0,
                        column_name: "grand_outer".to_string(),
                    },
                    DataType::Int32,
                ),
                output_name: "grand_outer".to_string(),
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
        output_schema: vec![("grand_outer".to_string(), DataType::Int32, None)],
    };

    let expr = TypedExpr::new(
        TypedExprKind::ScalarSubquery(Box::new(nested)),
        DataType::Int32,
    );

    let where_expr = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(TypedExpr::new(
                TypedExprKind::Constant(Value::Boolean(false)),
                DataType::Boolean,
            )),
            op: BinaryOp::Or,
            right: Box::new(TypedExpr::new(
                TypedExprKind::BinaryOp {
                    left: Box::new(expr),
                    op: BinaryOp::Eq,
                    right: Box::new(int_const(1)),
                },
                DataType::Boolean,
            )),
        },
        DataType::Boolean,
    );

    let query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![AnalyzedProjection {
                expr: int_const(1),
                output_name: "?column?".to_string(),
            }],
            from: vec![],
            where_clause: Some(where_expr),
            group_by: vec![],
            having: None,
            distinct: AnalyzedDistinct::All,
        }),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![("?column?".to_string(), DataType::Int32, None)],
    };

    assert!(is_correlated_query(&query));
}

#[test]
fn substitute_outer_refs_in_query_rewrites_table_function_args() {
    let query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![AnalyzedProjection {
                expr: int_const(1),
                output_name: "?column?".to_string(),
            }],
            from: vec![AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Function {
                    func: ResolvedFunction {
                        name: "generate_series".to_string(),
                        kind: FunctionKind::Builtin,
                        return_type: DataType::Int32,
                    },
                    args: vec![
                        TypedFunctionArg::Positional(int_const(1)),
                        TypedFunctionArg::Positional(TypedExpr::new(
                            TypedExprKind::ColumnRef {
                                scope_depth: 1,
                                column_index: 1,
                                column_name: "outer_n".to_string(),
                            },
                            DataType::Int32,
                        )),
                    ],
                    output_columns: vec![("generate_series".to_string(), DataType::Int32)],
                },
                alias: Some("gs".to_string()),
            }],
            where_clause: None,
            group_by: vec![],
            having: None,
            distinct: AnalyzedDistinct::All,
        }),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![("?column?".to_string(), DataType::Int32, None)],
    };
    let outer_row = Row::new(vec![Value::Int32(7), Value::Int32(5)]);

    let substituted = substitute_outer_refs_in_query(&query, &outer_row);
    let AnalyzedQueryBody::Select(select) = substituted.body else {
        panic!("expected select body");
    };
    let Some(AnalyzedTableRef {
        kind: AnalyzedTableRefKind::Function { args, .. },
        ..
    }) = select.from.first()
    else {
        panic!("expected function table ref");
    };

    let TypedFunctionArg::Positional(expr) = &args[1] else {
        panic!("expected positional argument");
    };
    assert!(matches!(
        expr.kind,
        TypedExprKind::Constant(Value::Int32(5))
    ));
}

// ── Semantic change regression tests ──────────────────────────

#[test]
fn has_outer_ref_detects_outer_ref_in_like_escape() {
    // Previously missed: escape field in Like was not traversed
    let expr = TypedExpr::new(
        TypedExprKind::Like {
            expr: Box::new(int_const(1)),
            pattern: Box::new(int_const(2)),
            escape: Some(Box::new(TypedExpr::new(
                TypedExprKind::ColumnRef {
                    scope_depth: 1,
                    column_index: 0,
                    column_name: "esc_col".to_string(),
                },
                DataType::Text,
            ))),
            case_insensitive: false,
            negated: false,
        },
        DataType::Boolean,
    );
    assert!(has_outer_ref(&expr));
}

#[test]
fn has_outer_ref_detects_outer_ref_in_function_order_by() {
    // Previously missed: order_by exprs in FunctionCall
    let expr = TypedExpr::new(
        TypedExprKind::FunctionCall {
            func: ResolvedFunction {
                name: "array_agg".to_string(),
                kind: FunctionKind::Builtin,
                return_type: DataType::Int32,
            },
            args: vec![int_const(1)],
            order_by: vec![TypedOrderByExpr {
                expr: TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 1,
                        column_index: 0,
                        column_name: "sort_col".to_string(),
                    },
                    DataType::Int32,
                ),
                asc: true,
                nulls_first: false,
            }],
            filter: None,
        },
        DataType::Int32,
    );
    assert!(has_outer_ref(&expr));
}

#[test]
fn has_outer_ref_detects_outer_ref_in_json_path() {
    // Previously missed: path field in JsonAccess
    let expr = TypedExpr::new(
        TypedExprKind::JsonAccess {
            expr: Box::new(int_const(1)),
            path: Box::new(TypedExpr::new(
                TypedExprKind::ColumnRef {
                    scope_depth: 1,
                    column_index: 0,
                    column_name: "path_col".to_string(),
                },
                DataType::Text,
            )),
            operator: crate::sql::analyzer::types::JsonAccessOp::Arrow,
        },
        DataType::Text,
    );
    assert!(has_outer_ref(&expr));
}

#[test]
fn substitute_outer_refs_recurses_into_similar_to() {
    // Previously the catch-all `_ => expr.clone()` skipped SimilarTo
    let expr = TypedExpr::new(
        TypedExprKind::SimilarTo {
            expr: Box::new(TypedExpr::new(
                TypedExprKind::ColumnRef {
                    scope_depth: 1,
                    column_index: 0,
                    column_name: "outer_col".to_string(),
                },
                DataType::Text,
            )),
            pattern: Box::new(int_const(1)),
            escape: None,
            negated: false,
        },
        DataType::Boolean,
    );
    let outer_row = Row::new(vec![Value::Text("hello".to_string())]);
    let result = substitute_outer_refs_in_expr(&expr, &outer_row);
    if let TypedExprKind::SimilarTo { expr: inner, .. } = &result.kind {
        assert!(matches!(
            inner.kind,
            TypedExprKind::Constant(Value::Text(_))
        ));
    } else {
        panic!("expected SimilarTo, got {:?}", result.kind);
    }
}

#[test]
fn substitute_outer_refs_recurses_into_min_max() {
    // Previously the catch-all `_ => expr.clone()` skipped MinMax
    let expr = TypedExpr::new(
        TypedExprKind::MinMax {
            args: vec![
                TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 1,
                        column_index: 0,
                        column_name: "a".to_string(),
                    },
                    DataType::Int32,
                ),
                int_const(5),
            ],
            is_greatest: true,
        },
        DataType::Int32,
    );
    let outer_row = Row::new(vec![Value::Int32(10)]);
    let result = substitute_outer_refs_in_expr(&expr, &outer_row);
    if let TypedExprKind::MinMax { args, .. } = &result.kind {
        assert!(matches!(
            args[0].kind,
            TypedExprKind::Constant(Value::Int32(10))
        ));
    } else {
        panic!("expected MinMax, got {:?}", result.kind);
    }
}

/// Regression test for derived-subquery scope boundary.
///
/// A FROM subquery (derived table) with scope_depth=1 refs points to its
/// enclosing query's scope — NOT beyond. The enclosing query itself should
/// NOT be classified as correlated just because its FROM subquery references
/// the enclosing scope.
///
/// Example: `SELECT * FROM (SELECT t1.x FROM t2) AS sub`
///   - Inside the derived subquery, `t1.x` has scope_depth=1 (one scope up)
///   - This makes the derived subquery correlated with its parent
///   - But the parent query is NOT correlated with any outer scope
#[test]
fn derived_subquery_scope_depth_1_does_not_make_parent_correlated() {
    // Build: SELECT 1 FROM (SELECT outer_col FROM t2) AS sub
    // where outer_col has scope_depth=1 inside the derived subquery
    let derived_subquery = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![AnalyzedProjection {
                expr: TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 1, // references enclosing query's scope
                        column_index: 0,
                        column_name: "outer_col".to_string(),
                    },
                    DataType::Int32,
                ),
                output_name: "outer_col".to_string(),
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
        output_schema: vec![("outer_col".to_string(), DataType::Int32, None)],
    };

    let parent_query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![AnalyzedProjection {
                expr: int_const(1),
                output_name: "?column?".to_string(),
            }],
            from: vec![AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Subquery(Box::new(derived_subquery)),
                alias: Some("sub".to_string()),
            }],
            where_clause: None,
            group_by: vec![],
            having: None,
            distinct: AnalyzedDistinct::All,
        }),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![("?column?".to_string(), DataType::Int32, None)],
    };

    // The parent query should NOT be classified as correlated — the derived
    // subquery's scope_depth=1 ref points to the parent's own scope, not beyond.
    assert!(
        !is_correlated_query(&parent_query),
        "parent query should not be correlated: derived subquery's scope_depth=1 \
         refs point to the parent scope, not beyond"
    );
}

/// Derived subquery with scope_depth=2 DOES make the parent correlated
/// (the ref points beyond the parent to a grandparent scope).
#[test]
fn derived_subquery_scope_depth_2_makes_parent_correlated() {
    let derived_subquery = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![AnalyzedProjection {
                expr: TypedExpr::new(
                    TypedExprKind::ColumnRef {
                        scope_depth: 2, // references grandparent scope (beyond parent)
                        column_index: 0,
                        column_name: "grandparent_col".to_string(),
                    },
                    DataType::Int32,
                ),
                output_name: "gp".to_string(),
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
        output_schema: vec![("gp".to_string(), DataType::Int32, None)],
    };

    let parent_query = AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![AnalyzedProjection {
                expr: int_const(1),
                output_name: "?column?".to_string(),
            }],
            from: vec![AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Subquery(Box::new(derived_subquery)),
                alias: Some("sub".to_string()),
            }],
            where_clause: None,
            group_by: vec![],
            having: None,
            distinct: AnalyzedDistinct::All,
        }),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema: vec![("?column?".to_string(), DataType::Int32, None)],
    };

    assert!(
        is_correlated_query(&parent_query),
        "parent query should be correlated: derived subquery's scope_depth=2 \
         ref points beyond the parent scope"
    );
}
