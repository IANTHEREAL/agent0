//! Unit tests for SELECT executor

use super::join::JOIN_LOCKING_CLAUSE_UNSUPPORTED;
use super::pushdown::GenerateSeriesOffsetLimitPushdownPlan;
use super::*;

#[cfg(test)]
mod join_locking_clause_tests {
    use super::*;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn parse_query(sql: &str) -> Query {
        let dialect = PostgreSqlDialect {};
        let mut statements = Parser::parse_sql(&dialect, sql).expect("parse SQL");
        assert_eq!(statements.len(), 1);
        match statements.remove(0) {
            sqlparser::ast::Statement::Query(query) => *query,
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn rejects_for_update_on_join_queries() {
        let query =
            parse_query("SELECT a.* FROM a JOIN b ON b.a_id = a.id WHERE a.id = 1 FOR UPDATE");
        let select = match &*query.body {
            SetExpr::Select(select) => select,
            other => panic!("expected Select, got {other:?}"),
        };
        let has_joins = !select.from[0].joins.is_empty() || select.from.len() > 1;
        assert!(has_joins);

        let err = ensure_no_locking_clauses_for_join(&query).unwrap_err();
        assert_eq!(err.to_string(), JOIN_LOCKING_CLAUSE_UNSUPPORTED);
    }

    #[test]
    fn allows_join_queries_without_locking_clauses() {
        let query = parse_query("SELECT a.* FROM a JOIN b ON b.a_id = a.id WHERE a.id = 1");
        let select = match &*query.body {
            SetExpr::Select(select) => select,
            other => panic!("expected Select, got {other:?}"),
        };
        let has_joins = !select.from[0].joins.is_empty() || select.from.len() > 1;
        assert!(has_joins);

        ensure_no_locking_clauses_for_join(&query).expect("no lock clauses");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;

    fn make_schema(col_names: &[&str]) -> TableSchema {
        TableSchema {
            name: "t".to_string(),
            table_id: 0,
            columns: col_names
                .iter()
                .map(|name| crate::types::ColumnDef {
                    name: (*name).to_string(),
                    data_type: DataType::Int32,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                })
                .collect(),
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        }
    }

    fn ident(name: &str) -> Expr {
        Expr::Identifier(sqlparser::ast::Ident::new(name))
    }

    fn parse_query(sql: &str) -> Box<Query> {
        let dialect = PostgreSqlDialect {};
        let mut statements = Parser::parse_sql(&dialect, sql).unwrap();
        match statements.remove(0) {
            sqlparser::ast::Statement::Query(q) => q,
            other => panic!("expected query, got {other:?}"),
        }
    }

    #[test]
    fn resolves_order_by_positional_for_select_projection() {
        let query = parse_query("SELECT a, b FROM t ORDER BY 2");
        let SetExpr::Select(select) = &*query.body else {
            panic!("Expected SELECT");
        };

        let schema = make_schema(&["a", "b"]);
        let resolved =
            resolve_order_by_exprs_for_non_agg(&query.order_by, &select.projection, &schema)
                .unwrap();

        assert_eq!(resolved.len(), 1);
        assert!(
            matches!(&resolved[0], Expr::Identifier(id) if id.value == "b"),
            "unexpected resolved expr: {:?}",
            resolved[0]
        );
    }

    #[test]
    fn resolves_order_by_positional_for_select_wildcard() {
        let query = parse_query("SELECT * FROM t ORDER BY 2");
        let SetExpr::Select(select) = &*query.body else {
            panic!("Expected SELECT");
        };

        let schema = make_schema(&["a", "b", "c"]);
        let resolved =
            resolve_order_by_exprs_for_non_agg(&query.order_by, &select.projection, &schema)
                .unwrap();

        assert_eq!(resolved.len(), 1);
        assert!(
            matches!(&resolved[0], Expr::Identifier(id) if id.value == "b"),
            "unexpected resolved expr: {:?}",
            resolved[0]
        );
    }

    #[test]
    fn rejects_out_of_range_order_by_position() {
        let query = parse_query("SELECT a FROM t ORDER BY 2");
        let SetExpr::Select(select) = &*query.body else {
            panic!("Expected SELECT");
        };

        let schema = make_schema(&["a"]);
        let err = resolve_order_by_exprs_for_non_agg(&query.order_by, &select.projection, &schema)
            .unwrap_err();
        assert!(err.to_string().contains("ORDER BY position 2"));
    }

    #[test]
    fn cube_treats_grouped_items_as_units() {
        let a = ident("a");
        let b = ident("b");
        let c = ident("c");
        let cube = Expr::Cube(vec![vec![a.clone(), b.clone()], vec![c.clone()]]);

        let sets = extract_grouping_sets(&[cube]).unwrap();
        assert_eq!(sets.len(), 4);

        for set in &sets {
            let has_a = set
                .iter()
                .any(|e| matches!(e, Expr::Identifier(id) if id.value == "a"));
            let has_b = set
                .iter()
                .any(|e| matches!(e, Expr::Identifier(id) if id.value == "b"));
            assert_eq!(has_a, has_b, "unexpected set: {:?}", set);
        }

        assert!(sets.iter().any(|s| s.is_empty()));
        assert!(sets
            .iter()
            .any(|s| s.len() == 1 && matches!(&s[0], Expr::Identifier(id) if id.value == "c")));
        assert!(sets.iter().any(|s| s.len() == 2));
        assert!(sets.iter().any(|s| s.len() == 3));
    }

    #[test]
    fn rollup_treats_grouped_items_as_units() {
        let a = ident("a");
        let b = ident("b");
        let c = ident("c");
        let rollup = Expr::Rollup(vec![vec![a.clone(), b.clone()], vec![c.clone()]]);

        let sets = extract_grouping_sets(&[rollup]).unwrap();
        assert_eq!(sets.len(), 3);

        for set in &sets {
            let has_a = set
                .iter()
                .any(|e| matches!(e, Expr::Identifier(id) if id.value == "a"));
            let has_b = set
                .iter()
                .any(|e| matches!(e, Expr::Identifier(id) if id.value == "b"));
            assert_eq!(has_a, has_b, "unexpected set: {:?}", set);
        }

        assert!(sets.iter().any(|s| s.is_empty()));
        assert!(sets.iter().any(|s| s.len() == 2));
        assert!(sets.iter().any(|s| s.len() == 3));
    }

    #[test]
    fn generate_series_pushdown_is_disabled_with_select_list_srf() {
        let query = parse_query("SELECT unnest(ARRAY[1,2]) FROM generate_series(1, 10) LIMIT 1");
        let SetExpr::Select(select) = &*query.body else {
            panic!("expected SELECT");
        };

        let plan = plan_generate_series_offset_limit_pushdown(query.as_ref(), select.as_ref());
        assert_eq!(
            plan,
            GenerateSeriesOffsetLimitPushdownPlan {
                offset: 0,
                limit: None,
                clear_query_offset_limit_fetch: false,
            }
        );
    }

    #[test]
    fn generate_series_offset_is_not_pushed_down_with_volatile_projection() {
        let query =
            parse_query("SELECT nextval('s') FROM generate_series(1, 100) OFFSET 10 LIMIT 5");
        let SetExpr::Select(select) = &*query.body else {
            panic!("expected SELECT");
        };

        let plan = plan_generate_series_offset_limit_pushdown(query.as_ref(), select.as_ref());
        assert_eq!(
            plan,
            GenerateSeriesOffsetLimitPushdownPlan {
                offset: 0,
                limit: Some(15),
                clear_query_offset_limit_fetch: false,
            }
        );
    }

    #[test]
    fn generate_series_offset_is_not_pushed_down_with_nontrivial_projection() {
        let query =
            parse_query("SELECT 1/(n-1) FROM generate_series(1, 100) AS g(n) OFFSET 10 LIMIT 5");
        let SetExpr::Select(select) = &*query.body else {
            panic!("expected SELECT");
        };

        let plan = plan_generate_series_offset_limit_pushdown(query.as_ref(), select.as_ref());
        assert_eq!(
            plan,
            GenerateSeriesOffsetLimitPushdownPlan {
                offset: 0,
                limit: Some(15),
                clear_query_offset_limit_fetch: false,
            }
        );
    }

    #[test]
    fn generate_series_offset_limit_are_pushed_down_for_simple_projection() {
        let query = parse_query("SELECT * FROM generate_series(1, 100) OFFSET 10 LIMIT 5");
        let SetExpr::Select(select) = &*query.body else {
            panic!("expected SELECT");
        };

        let plan = plan_generate_series_offset_limit_pushdown(query.as_ref(), select.as_ref());
        assert_eq!(
            plan,
            GenerateSeriesOffsetLimitPushdownPlan {
                offset: 10,
                limit: Some(5),
                clear_query_offset_limit_fetch: true,
            }
        );
    }

    #[test]
    fn generate_series_limit_is_pushed_down_without_offset() {
        let query = parse_query("SELECT random() FROM generate_series(1, 100) LIMIT 5");
        let SetExpr::Select(select) = &*query.body else {
            panic!("expected SELECT");
        };

        let plan = plan_generate_series_offset_limit_pushdown(query.as_ref(), select.as_ref());
        assert_eq!(
            plan,
            GenerateSeriesOffsetLimitPushdownPlan {
                offset: 0,
                limit: Some(5),
                clear_query_offset_limit_fetch: true,
            }
        );
    }

    #[test]
    fn generate_series_pushdown_normalizes_offset_limit_expressions() {
        let query = parse_query(
            "SELECT nextval('s') FROM generate_series(1, 100) OFFSET (txid_current() % 5 + 1) LIMIT 1",
        );
        let SetExpr::Select(select) = &*query.body else {
            panic!("expected SELECT");
        };

        assert!(generate_series_offset_limit_pushdown_eligible(
            query.as_ref(),
            select.as_ref()
        ));

        let normalized = normalize_query_offset_limit_fetch_expressions(query.as_ref());
        let offset_expr = &normalized.offset.as_ref().unwrap().value;
        let Expr::Value(SqlValue::Number(n_str, _)) = offset_expr else {
            panic!("expected numeric OFFSET expr, got {offset_expr:?}");
        };
        let offset_n: usize = n_str.parse().unwrap();

        let plan = plan_generate_series_offset_limit_pushdown(&normalized, select.as_ref());
        assert_eq!(
            plan,
            GenerateSeriesOffsetLimitPushdownPlan {
                offset: 0,
                limit: Some(offset_n + 1),
                clear_query_offset_limit_fetch: false,
            }
        );

        let rows: Vec<Row> = (0..10).map(|i| Row::new(vec![Value::Int32(i)])).collect();
        let result = apply_offset_limit_fetch(rows, &normalized);
        assert_eq!(result.len(), 1);
    }
}

#[cfg(test)]
mod using_merge_tests {
    use super::join::using_merge::{
        build_coalesce_for_merge, replace_using_merge_refs, UsingMergeColumn,
    };
    use super::*;

    #[test]
    fn build_coalesce_two_sources() {
        let mc = UsingMergeColumn {
            col_name: "id".to_string(),
            source_aliases: vec!["a".to_string(), "b".to_string()],
        };
        let expr = build_coalesce_for_merge(&mc);
        let s = format!("{}", expr);
        assert!(s.contains("COALESCE"), "expected COALESCE, got: {}", s);
        assert!(s.contains("a.id"), "expected a.id, got: {}", s);
        assert!(s.contains("b.id"), "expected b.id, got: {}", s);
    }

    #[test]
    fn build_coalesce_three_sources_chained() {
        let mc = UsingMergeColumn {
            col_name: "id".to_string(),
            source_aliases: vec!["a".to_string(), "b".to_string(), "c".to_string()],
        };
        let expr = build_coalesce_for_merge(&mc);
        let s = format!("{}", expr);
        assert!(
            s.contains("c.id"),
            "expected c.id in chained merge, got: {}",
            s
        );
    }

    #[test]
    fn replace_merge_refs_bare_identifier() {
        let mc = vec![UsingMergeColumn {
            col_name: "a".to_string(),
            source_aliases: vec!["t1".to_string(), "t2".to_string()],
        }];
        let expr = Expr::Identifier(Ident::new("a"));
        let result = replace_using_merge_refs(&expr, &mc);
        let s = format!("{}", result);
        assert!(
            s.contains("COALESCE"),
            "bare 'a' should become COALESCE: {}",
            s
        );
    }

    #[test]
    fn replace_merge_refs_qualified_untouched() {
        let mc = vec![UsingMergeColumn {
            col_name: "a".to_string(),
            source_aliases: vec!["t1".to_string(), "t2".to_string()],
        }];
        let expr = Expr::CompoundIdentifier(vec![Ident::new("t1"), Ident::new("a")]);
        let result = replace_using_merge_refs(&expr, &mc);
        assert_eq!(
            format!("{}", result),
            format!("{}", expr),
            "qualified ref should be untouched"
        );
    }

    #[test]
    fn replace_merge_refs_non_merge_column_untouched() {
        let mc = vec![UsingMergeColumn {
            col_name: "a".to_string(),
            source_aliases: vec!["t1".to_string(), "t2".to_string()],
        }];
        let expr = Expr::Identifier(Ident::new("b"));
        let result = replace_using_merge_refs(&expr, &mc);
        assert_eq!(
            format!("{}", result),
            "b",
            "non-merge column should stay as-is"
        );
    }

    #[test]
    fn replace_merge_refs_in_binary_op() {
        let mc = vec![UsingMergeColumn {
            col_name: "x".to_string(),
            source_aliases: vec!["l".to_string(), "r".to_string()],
        }];
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("x"))),
            op: BinaryOperator::Gt,
            right: Box::new(Expr::Value(SqlValue::Number("5".to_string(), false))),
        };
        let result = replace_using_merge_refs(&expr, &mc);
        let s = format!("{}", result);
        assert!(
            s.contains("COALESCE"),
            "x in binary op should become COALESCE: {}",
            s
        );
        assert!(s.contains("> 5"), "comparison should be preserved: {}", s);
    }

    #[test]
    fn replace_merge_refs_empty_merge_columns_is_noop() {
        let expr = Expr::Identifier(Ident::new("a"));
        let result = replace_using_merge_refs(&expr, &[]);
        assert_eq!(format!("{}", result), "a");
    }
}

#[cfg(test)]
mod window_routing_tests {
    use super::*;
    use sqlparser::ast::{
        Function, FunctionArg, FunctionArgExpr, Ident, ObjectName, WindowSpec, WindowType,
    };

    #[test]
    fn window_function_not_detected_as_bare_aggregate() {
        let window_func = Function {
            name: ObjectName(vec![Ident::new("sum")]),
            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(
                Expr::Identifier(Ident::new("salary")),
            ))],
            over: Some(WindowType::WindowSpec(WindowSpec {
                partition_by: vec![],
                order_by: vec![],
                window_frame: None,
            })),
            filter: None,
            null_treatment: None,
            distinct: false,
            special: false,
            order_by: vec![],
        };

        let projection = vec![
            SelectItem::UnnamedExpr(Expr::Identifier(Ident::new("id"))),
            SelectItem::ExprWithAlias {
                expr: Expr::Function(window_func),
                alias: Ident::new("running_total"),
            },
        ];

        let has_bare_agg = projection_has_non_window_aggregate(&projection);

        assert!(
            !has_bare_agg,
            "SUM(salary) OVER (...) should NOT be detected as bare aggregate"
        );
    }

    #[test]
    fn bare_aggregate_still_detected() {
        let bare_agg = Function {
            name: ObjectName(vec![Ident::new("count")]),
            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Wildcard)],
            over: None,
            filter: None,
            null_treatment: None,
            distinct: false,
            special: false,
            order_by: vec![],
        };

        let projection = vec![SelectItem::UnnamedExpr(Expr::Function(bare_agg))];

        let has_bare_agg = projection_has_non_window_aggregate(&projection);

        assert!(
            has_bare_agg,
            "COUNT(*) without OVER should be detected as bare aggregate"
        );
    }

    #[test]
    fn nested_aggregate_detected_in_projection() {
        let count_star = Function {
            name: ObjectName(vec![Ident::new("count")]),
            args: vec![FunctionArg::Unnamed(FunctionArgExpr::Wildcard)],
            over: None,
            filter: None,
            null_treatment: None,
            distinct: false,
            special: false,
            order_by: vec![],
        };

        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Value(SqlValue::SingleQuotedString(
                "TYPE_LEFT=".to_string(),
            ))),
            op: BinaryOperator::StringConcat,
            right: Box::new(Expr::Function(count_star)),
        };

        let projection = vec![SelectItem::UnnamedExpr(expr)];

        assert!(
            projection_has_non_window_aggregate(&projection),
            "Nested COUNT(*) in expression should force aggregation routing"
        );
    }
}
