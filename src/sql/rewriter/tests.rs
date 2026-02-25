//! Unit tests for the query rewriter (subquery flattening).

#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use crate::model::{DataType, Value};
    use crate::sql::analyzer::types::{
        AnalyzedDistinct, AnalyzedProjection, AnalyzedQuery, AnalyzedQueryBody, AnalyzedSelect,
        AnalyzedTableRef, AnalyzedTableRefKind, BinaryOp, TableRefSchema, TypedExpr, TypedExprKind,
        TypedOrderByExpr,
    };
    use crate::sql::rewriter::rewrite_query;

    // -- Test helpers --

    fn col_ref(index: usize, name: &str, dt: DataType) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: index,
                column_name: name.to_string(),
            },
            dt,
        )
    }

    fn projection(index: usize, name: &str, dt: DataType) -> AnalyzedProjection {
        AnalyzedProjection {
            expr: col_ref(index, name, dt.clone()),
            output_name: name.to_string(),
        }
    }

    fn table_ref(name: &str) -> AnalyzedTableRef {
        AnalyzedTableRef {
            kind: AnalyzedTableRefKind::Table {
                name: name.to_string(),
                schema: TableRefSchema {
                    table_id: 1,
                    columns: vec![],
                },
            },
            alias: None,
        }
    }

    fn simple_inner_query(
        table: &str,
        projections: Vec<AnalyzedProjection>,
        where_clause: Option<TypedExpr>,
    ) -> AnalyzedQuery {
        AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: projections,
                from: vec![table_ref(table)],
                where_clause,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![],
        }
    }

    fn wrap_as_outer(
        inner: AnalyzedQuery,
        alias: &str,
        outer_projection: Vec<AnalyzedProjection>,
        outer_where: Option<TypedExpr>,
    ) -> AnalyzedQuery {
        AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: outer_projection,
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Subquery(Box::new(inner)),
                    alias: Some(alias.to_string()),
                }],
                where_clause: outer_where,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![
                ("id".to_string(), DataType::Int64, None),
                ("name".to_string(), DataType::Text, None),
            ],
        }
    }

    fn is_table_from(query: &AnalyzedQuery, expected_table: &str) -> bool {
        if let AnalyzedQueryBody::Select(ref s) = query.body {
            if let Some(from) = s.from.first() {
                if let AnalyzedTableRefKind::Table { ref name, .. } = from.kind {
                    return name == expected_table;
                }
            }
        }
        false
    }

    fn is_subquery_from(query: &AnalyzedQuery) -> bool {
        if let AnalyzedQueryBody::Select(ref s) = query.body {
            if let Some(from) = s.from.first() {
                return matches!(from.kind, AnalyzedTableRefKind::Subquery(_));
            }
        }
        false
    }

    // -- Positive test: basic flattening --

    #[test]
    fn test_flatten_simple_view() {
        // Inner: SELECT id(col0), name(col1) FROM users WHERE active(col2) = true
        let inner_where = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(col_ref(2, "active", DataType::Boolean)),
                op: BinaryOp::Eq,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Boolean(true)),
                    DataType::Boolean,
                )),
            },
            DataType::Boolean,
        );
        let inner = simple_inner_query(
            "users",
            vec![
                projection(0, "id", DataType::Int64),
                projection(1, "name", DataType::Text),
            ],
            Some(inner_where),
        );

        // Outer: SELECT * FROM (inner) AS v WHERE id > 5
        let outer_where = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(col_ref(0, "id", DataType::Int64)),
                op: BinaryOp::Gt,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int64(5)),
                    DataType::Int64,
                )),
            },
            DataType::Boolean,
        );
        let query = wrap_as_outer(
            inner,
            "v",
            vec![
                projection(0, "id", DataType::Int64),
                projection(1, "name", DataType::Text),
            ],
            Some(outer_where),
        );

        let result = rewrite_query(query);

        // Should be flattened: FROM users, not subquery
        assert!(is_table_from(&result, "users"));

        // WHERE should be merged: (inner IS TRUE) AND outer
        let AnalyzedQueryBody::Select(ref s) = result.body else {
            panic!("expected Select");
        };
        let where_clause = s.where_clause.as_ref().expect("should have WHERE");
        assert!(matches!(
            where_clause.kind,
            TypedExprKind::BinaryOp {
                op: BinaryOp::And,
                ..
            }
        ));
    }

    // -- Negative tests: non-flattenable cases --

    #[test]
    fn test_no_flatten_group_by() {
        let inner = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![projection(0, "id", DataType::Int64)],
                from: vec![table_ref("users")],
                where_clause: None,
                group_by: vec![col_ref(0, "id", DataType::Int64)],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![],
        };

        let query = wrap_as_outer(inner, "v", vec![projection(0, "id", DataType::Int64)], None);
        let result = rewrite_query(query);
        assert!(is_subquery_from(&result));
    }

    #[test]
    fn test_no_flatten_limit() {
        let inner = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![projection(0, "id", DataType::Int64)],
                from: vec![table_ref("users")],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: Some(TypedExpr::new(
                TypedExprKind::Constant(Value::Int64(10)),
                DataType::Int64,
            )),
            offset: None,
            output_schema: vec![],
        };

        let query = wrap_as_outer(inner, "v", vec![projection(0, "id", DataType::Int64)], None);
        let result = rewrite_query(query);
        assert!(is_subquery_from(&result));
    }

    #[test]
    fn test_no_flatten_distinct() {
        let inner = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![projection(0, "id", DataType::Int64)],
                from: vec![table_ref("users")],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::Distinct,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![],
        };

        let query = wrap_as_outer(inner, "v", vec![projection(0, "id", DataType::Int64)], None);
        let result = rewrite_query(query);
        assert!(is_subquery_from(&result));
    }

    #[test]
    fn test_no_flatten_computed_projection() {
        // Inner projection has a BinaryOp, not a plain ColumnRef
        let inner = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: TypedExpr::new(
                        TypedExprKind::BinaryOp {
                            left: Box::new(col_ref(0, "a", DataType::Int64)),
                            op: BinaryOp::Add,
                            right: Box::new(TypedExpr::new(
                                TypedExprKind::Constant(Value::Int64(1)),
                                DataType::Int64,
                            )),
                        },
                        DataType::Int64,
                    ),
                    output_name: "a_plus_one".to_string(),
                }],
                from: vec![table_ref("users")],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![],
        };

        let query = wrap_as_outer(
            inner,
            "v",
            vec![projection(0, "a_plus_one", DataType::Int64)],
            None,
        );
        let result = rewrite_query(query);
        assert!(is_subquery_from(&result));
    }

    #[test]
    fn test_no_flatten_multi_from() {
        // Outer has 2 FROM sources
        let inner = simple_inner_query("users", vec![projection(0, "id", DataType::Int64)], None);

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![projection(0, "id", DataType::Int64)],
                from: vec![
                    AnalyzedTableRef {
                        kind: AnalyzedTableRefKind::Subquery(Box::new(inner)),
                        alias: Some("v".to_string()),
                    },
                    table_ref("other"),
                ],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![],
        };

        let result = rewrite_query(query);
        // Not flattened because outer has 2 FROM sources
        if let AnalyzedQueryBody::Select(ref s) = result.body {
            assert_eq!(s.from.len(), 2);
        } else {
            panic!("expected Select");
        }
    }

    // -- Column remap tests --

    #[test]
    fn test_column_remap_reorder() {
        // Inner: SELECT col2 AS a, col0 AS b FROM users
        // mapping = [2, 0] -- outer col0 -> base col2, outer col1 -> base col0
        let inner = simple_inner_query(
            "users",
            vec![
                projection(2, "c", DataType::Text),
                projection(0, "a", DataType::Int64),
            ],
            None,
        );

        // Outer: SELECT col0, col1 FROM (inner) AS v
        let query = wrap_as_outer(
            inner,
            "v",
            vec![
                projection(0, "a", DataType::Text),
                projection(1, "b", DataType::Int64),
            ],
            None,
        );

        let result = rewrite_query(query);
        assert!(is_table_from(&result, "users"));

        // Check that projection column indices are remapped
        let AnalyzedQueryBody::Select(ref s) = result.body else {
            panic!("expected Select");
        };
        if let TypedExprKind::ColumnRef { column_index, .. } = &s.projection[0].expr.kind {
            assert_eq!(*column_index, 2, "outer col0 should map to base col2");
        } else {
            panic!("expected ColumnRef");
        }
        if let TypedExprKind::ColumnRef { column_index, .. } = &s.projection[1].expr.kind {
            assert_eq!(*column_index, 0, "outer col1 should map to base col0");
        } else {
            panic!("expected ColumnRef");
        }
    }

    #[test]
    fn test_where_merge_both_is_true_guard() {
        // Inner has WHERE, outer has WHERE -> merged as (inner IS TRUE) AND outer.
        // IS TRUE is required: the evaluator evaluates RHS when LHS is NULL
        // (typed_eval.rs:378), so without IS TRUE a NULL inner predicate would
        // cause outer evaluation on rows the original subquery discarded.
        let inner_where = col_ref(2, "active", DataType::Boolean);
        let inner = simple_inner_query(
            "users",
            vec![projection(0, "id", DataType::Int64)],
            Some(inner_where),
        );

        let outer_where = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(col_ref(0, "id", DataType::Int64)),
                op: BinaryOp::Gt,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int64(5)),
                    DataType::Int64,
                )),
            },
            DataType::Boolean,
        );

        let query = wrap_as_outer(
            inner,
            "v",
            vec![projection(0, "id", DataType::Int64)],
            Some(outer_where),
        );

        let result = rewrite_query(query);
        let AnalyzedQueryBody::Select(ref s) = result.body else {
            panic!("expected Select");
        };
        let w = s.where_clause.as_ref().expect("should have WHERE");

        // Top-level: AND
        let TypedExprKind::BinaryOp {
            ref left,
            op: BinaryOp::And,
            ..
        } = w.kind
        else {
            panic!("expected AND at top level, got: {:?}", w.kind);
        };

        // LHS: IS TRUE wrapping inner WHERE -- prevents NULL from leaking to RHS
        assert!(
            matches!(
                left.kind,
                TypedExprKind::IsTest {
                    test: crate::sql::analyzer::types::IsTestKind::True,
                    negated: false,
                    ..
                }
            ),
            "LHS should be IS TRUE wrapping inner WHERE"
        );
    }

    #[test]
    fn test_where_merge_inner_only() {
        // Only inner WHERE -> becomes outer WHERE directly (NOT wrapped in IS TRUE,
        // because IS TRUE suppresses planner predicate extraction for index selection).
        let inner_where = col_ref(2, "active", DataType::Boolean);
        let inner = simple_inner_query(
            "users",
            vec![projection(0, "id", DataType::Int64)],
            Some(inner_where),
        );

        let query = wrap_as_outer(
            inner,
            "v",
            vec![projection(0, "id", DataType::Int64)],
            None, // no outer WHERE
        );

        let result = rewrite_query(query);
        let AnalyzedQueryBody::Select(ref s) = result.body else {
            panic!("expected Select");
        };
        let w = s.where_clause.as_ref().expect("should have WHERE");
        // Inner WHERE should be used directly -- a bare ColumnRef, not IS TRUE wrapped.
        assert!(
            matches!(
                w.kind,
                TypedExprKind::ColumnRef {
                    column_index: 2,
                    ..
                }
            ),
            "inner-only WHERE should be used directly without IS TRUE wrapping"
        );
    }

    #[test]
    fn test_order_by_remap() {
        // Inner: SELECT col1 AS a FROM users -> mapping = [1]
        let inner = simple_inner_query("users", vec![projection(1, "b", DataType::Int64)], None);

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![projection(0, "a", DataType::Int64)],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Subquery(Box::new(inner)),
                    alias: Some("v".to_string()),
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![TypedOrderByExpr {
                expr: col_ref(0, "a", DataType::Int64),
                asc: true,
                nulls_first: false,
            }],
            limit: None,
            offset: None,
            output_schema: vec![("a".to_string(), DataType::Int64, None)],
        };

        let result = rewrite_query(query);
        assert!(is_table_from(&result, "users"));

        if let TypedExprKind::ColumnRef { column_index, .. } = &result.order_by[0].expr.kind {
            assert_eq!(*column_index, 1, "ORDER BY col0 should remap to base col1");
        } else {
            panic!("expected ColumnRef in ORDER BY");
        }
    }

    #[test]
    fn test_outer_group_by_remap() {
        // Inner: SELECT col2 AS x FROM users -> mapping = [2]
        let inner = simple_inner_query("users", vec![projection(2, "c", DataType::Text)], None);

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![projection(0, "x", DataType::Text)],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Subquery(Box::new(inner)),
                    alias: Some("v".to_string()),
                }],
                where_clause: None,
                group_by: vec![col_ref(0, "x", DataType::Text)],
                having: None,
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("x".to_string(), DataType::Text, None)],
        };

        let result = rewrite_query(query);
        assert!(is_table_from(&result, "users"));

        let AnalyzedQueryBody::Select(ref s) = result.body else {
            panic!("expected Select");
        };
        if let TypedExprKind::ColumnRef { column_index, .. } = &s.group_by[0].kind {
            assert_eq!(*column_index, 2, "GROUP BY col0 should remap to base col2");
        } else {
            panic!("expected ColumnRef in GROUP BY");
        }
    }

    #[test]
    fn test_outer_having_remap() {
        // Inner: SELECT col1 AS x FROM users -> mapping = [1]
        let inner = simple_inner_query("users", vec![projection(1, "b", DataType::Int64)], None);

        let having = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(col_ref(0, "x", DataType::Int64)),
                op: BinaryOp::Gt,
                right: Box::new(TypedExpr::new(
                    TypedExprKind::Constant(Value::Int64(10)),
                    DataType::Int64,
                )),
            },
            DataType::Boolean,
        );

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![projection(0, "x", DataType::Int64)],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Subquery(Box::new(inner)),
                    alias: Some("v".to_string()),
                }],
                where_clause: None,
                group_by: vec![col_ref(0, "x", DataType::Int64)],
                having: Some(having),
                distinct: AnalyzedDistinct::All,
            }),
            order_by: vec![],
            limit: None,
            offset: None,
            output_schema: vec![("x".to_string(), DataType::Int64, None)],
        };

        let result = rewrite_query(query);
        assert!(is_table_from(&result, "users"));

        let AnalyzedQueryBody::Select(ref s) = result.body else {
            panic!("expected Select");
        };
        let h = s.having.as_ref().expect("should have HAVING");
        if let TypedExprKind::BinaryOp { ref left, .. } = h.kind {
            if let TypedExprKind::ColumnRef { column_index, .. } = &left.kind {
                assert_eq!(*column_index, 1, "HAVING col0 should remap to base col1");
            } else {
                panic!("expected ColumnRef in HAVING");
            }
        } else {
            panic!("expected BinaryOp in HAVING");
        }
    }

    #[test]
    fn test_outer_distinct_on_remap() {
        // Inner: SELECT col3 AS x FROM users -> mapping = [3]
        let inner = simple_inner_query("users", vec![projection(3, "d", DataType::Text)], None);

        let query = AnalyzedQuery {
            ctes: vec![],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![projection(0, "x", DataType::Text)],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Subquery(Box::new(inner)),
                    alias: Some("v".to_string()),
                }],
                where_clause: None,
                group_by: vec![],
                having: None,
                distinct: AnalyzedDistinct::DistinctOn(vec![col_ref(0, "x", DataType::Text)]),
            }),
            order_by: vec![TypedOrderByExpr {
                expr: col_ref(0, "x", DataType::Text),
                asc: true,
                nulls_first: false,
            }],
            limit: None,
            offset: None,
            output_schema: vec![("x".to_string(), DataType::Text, None)],
        };

        let result = rewrite_query(query);
        assert!(is_table_from(&result, "users"));

        let AnalyzedQueryBody::Select(ref s) = result.body else {
            panic!("expected Select");
        };
        if let AnalyzedDistinct::DistinctOn(ref exprs) = s.distinct {
            if let TypedExprKind::ColumnRef { column_index, .. } = &exprs[0].kind {
                assert_eq!(
                    *column_index, 3,
                    "DISTINCT ON col0 should remap to base col3"
                );
            } else {
                panic!("expected ColumnRef in DISTINCT ON");
            }
        } else {
            panic!("expected DistinctOn");
        }
    }

    #[test]
    fn test_no_flatten_outer_subquery_expr() {
        // Outer WHERE has a ScalarSubquery -- flattening must be skipped because
        // subquery bodies may contain correlated refs (scope_depth > 0) whose
        // column_index references the outer row layout. Column remap does not
        // descend into subquery bodies, so after non-identity remap those
        // correlated refs would read the wrong outer column.
        let inner = simple_inner_query(
            "users",
            vec![
                projection(2, "c", DataType::Int64), // non-identity mapping = [2]
            ],
            None,
        );

        let scalar_subquery_expr = TypedExpr::new(
            TypedExprKind::ScalarSubquery(Box::new(simple_inner_query(
                "other",
                vec![projection(0, "x", DataType::Int64)],
                None,
            ))),
            DataType::Int64,
        );

        let outer_where = TypedExpr::new(
            TypedExprKind::BinaryOp {
                left: Box::new(col_ref(0, "a", DataType::Int64)),
                op: BinaryOp::Eq,
                right: Box::new(scalar_subquery_expr),
            },
            DataType::Boolean,
        );

        let query = wrap_as_outer(
            inner,
            "v",
            vec![projection(0, "a", DataType::Int64)],
            Some(outer_where),
        );

        let result = rewrite_query(query);
        // Must NOT flatten -- outer contains subquery expression
        assert!(
            is_subquery_from(&result),
            "should bail out when outer has subquery expressions"
        );
    }

    #[test]
    fn test_no_flatten_outer_exists_expr() {
        // Outer WHERE has an Exists subquery -- must not flatten.
        let inner = simple_inner_query("users", vec![projection(0, "id", DataType::Int64)], None);

        let exists_expr = TypedExpr::new(
            TypedExprKind::Exists {
                subquery: Box::new(simple_inner_query(
                    "other",
                    vec![projection(0, "x", DataType::Int64)],
                    None,
                )),
                negated: false,
            },
            DataType::Boolean,
        );

        let query = wrap_as_outer(
            inner,
            "v",
            vec![projection(0, "id", DataType::Int64)],
            Some(exists_expr),
        );

        let result = rewrite_query(query);
        assert!(
            is_subquery_from(&result),
            "should bail out when outer has EXISTS expression"
        );
    }

    #[test]
    fn test_no_flatten_outer_in_subquery_expr() {
        // Outer WHERE has IN (subquery) -- must not flatten.
        let inner = simple_inner_query("users", vec![projection(0, "id", DataType::Int64)], None);

        let in_subquery_expr = TypedExpr::new(
            TypedExprKind::InSubquery {
                expr: Box::new(col_ref(0, "id", DataType::Int64)),
                subquery: Box::new(simple_inner_query(
                    "other",
                    vec![projection(0, "x", DataType::Int64)],
                    None,
                )),
                negated: false,
            },
            DataType::Boolean,
        );

        let query = wrap_as_outer(
            inner,
            "v",
            vec![projection(0, "id", DataType::Int64)],
            Some(in_subquery_expr),
        );

        let result = rewrite_query(query);
        assert!(
            is_subquery_from(&result),
            "should bail out when outer has IN (subquery) expression"
        );
    }

    #[test]
    fn test_column_name_remapped_for_alias() {
        // Inner: SELECT a AS x FROM users -> base column name is "a", alias is "x"
        // After flatten, outer ColumnRef should get column_name "a" (base), not "x" (alias)
        let inner = simple_inner_query(
            "users",
            vec![AnalyzedProjection {
                expr: col_ref(0, "a", DataType::Int64),
                output_name: "x".to_string(), // alias
            }],
            None,
        );

        let query = wrap_as_outer(
            inner,
            "v",
            vec![AnalyzedProjection {
                expr: col_ref(0, "x", DataType::Int64), // outer sees alias "x"
                output_name: "x".to_string(),
            }],
            None,
        );

        let result = rewrite_query(query);
        assert!(is_table_from(&result, "users"));

        let AnalyzedQueryBody::Select(ref s) = result.body else {
            panic!("expected Select");
        };
        if let TypedExprKind::ColumnRef {
            column_name,
            column_index,
            ..
        } = &s.projection[0].expr.kind
        {
            assert_eq!(column_name, "a", "should use base column name, not alias");
            assert_eq!(*column_index, 0);
        } else {
            panic!("expected ColumnRef in projection");
        }
    }

    // -- Defensive bounds check --

    #[test]
    fn test_out_of_bounds_column_ref_no_panic() {
        // Inner projects 1 column but outer refs col1 (out of bounds)
        let inner = simple_inner_query("users", vec![projection(0, "id", DataType::Int64)], None);

        let query = wrap_as_outer(
            inner,
            "v",
            vec![
                projection(0, "id", DataType::Int64),
                projection(1, "oops", DataType::Text), // col1 is out of bounds (only 1 inner proj)
            ],
            None,
        );

        let result = rewrite_query(query);
        // Should NOT flatten -- defensive guard kicks in
        assert!(
            is_subquery_from(&result),
            "should bail out on out-of-bounds ref"
        );
    }
}
