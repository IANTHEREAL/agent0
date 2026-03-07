//! Tests for the SQL parser module.

use super::*;
use operator_rewrite::{rewrite_jsonb_exists_ops, rewrite_vector_distance_ops};
use preprocess::{preprocess_create_sequence, preprocess_sql};

#[test]
fn test_parse_select() {
    let stmts = parse_sql("SELECT * FROM users").unwrap();
    assert_eq!(stmts.len(), 1);
}

#[test]
fn test_parse_table_function_named_args_with_colon_equals() {
    let stmts = parse_sql(
        "SELECT * FROM extensions.fs9('/tmp/path.csv', format := 'csv', delimiter := '|')",
    )
    .unwrap();
    assert_eq!(stmts.len(), 1);

    let stmt = &stmts[0];
    let Statement::Query(query) = stmt else {
        panic!("expected query statement, got {stmt:?}");
    };
    let sqlparser::ast::SetExpr::Select(select) = query.body.as_ref() else {
        panic!("expected SELECT query body");
    };
    let from = select.from.first().expect("expected FROM clause");
    match &from.relation {
        sqlparser::ast::TableFactor::Table {
            name,
            args: Some(args),
            ..
        } => {
            assert_eq!(name.to_string(), "extensions.fs9");
            assert_eq!(args.len(), 3);
            assert!(matches!(args[1], sqlparser::ast::FunctionArg::Named { .. }));
            assert!(matches!(args[2], sqlparser::ast::FunctionArg::Named { .. }));
        }
        other => panic!("expected table-valued function call, got {other:?}"),
    }
}

#[test]
fn test_parse_reset_role_rewrite() {
    let stmts = parse_sql("RESET ROLE").unwrap();
    assert_eq!(stmts.len(), 1);
    assert!(matches!(
        stmts[0],
        Statement::SetRole {
            role_name: None,
            ..
        }
    ));
}

#[test]
fn test_parse_create_table() {
    let stmts = parse_sql("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)").unwrap();
    assert_eq!(stmts.len(), 1);
}

#[test]
fn test_parse_create_user_aliases_to_create_role_with_login() {
    let stmts = parse_sql("CREATE USER readonly WITH PASSWORD 'test123'").unwrap();
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Statement::CreateRole {
            names,
            login,
            password,
            ..
        } => {
            assert_eq!(names.len(), 1);
            assert_eq!(names[0].to_string(), "readonly");
            assert_eq!(*login, Some(true));
            assert!(password.is_some());
        }
        other => panic!("expected CREATE ROLE, got {:?}", other),
    }
}

#[test]
fn test_parse_create_schema_named_authorization() {
    let stmts = parse_sql("CREATE SCHEMA s1 AUTHORIZATION owner1").unwrap();
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Statement::CreateSchema {
            schema_name,
            if_not_exists,
        } => {
            assert!(!*if_not_exists);
            match schema_name {
                sqlparser::ast::SchemaName::NamedAuthorization(name, owner) => {
                    assert_eq!(name.to_string(), "s1");
                    assert_eq!(owner.value, "owner1");
                }
                other => panic!("expected named authorization, got {:?}", other),
            }
        }
        other => panic!("expected CREATE SCHEMA, got {:?}", other),
    }
}

#[test]
fn test_parse_reset_role() {
    let stmts = parse_sql("RESET ROLE").unwrap();
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Statement::SetRole { role_name, .. } => assert!(role_name.is_none()),
        other => panic!("expected SET ROLE, got {:?}", other),
    }
}

#[test]
fn test_parse_insert() {
    let stmts = parse_sql("INSERT INTO users (id, name) VALUES (1, 'Alice')").unwrap();
    assert_eq!(stmts.len(), 1);
}

#[test]
fn test_parse_update_from_comma_with_newline_before_set() {
    let sql = "UPDATE \"flow_version\" fv\nSET \"updatedBy\" = NULL\nFROM \"flow\" f JOIN \"project\" p ON p.\"id\" = f.\"projectId\", \"user\" u\nWHERE fv.\"flowId\" = f.\"id\"";
    let stmts = parse_sql(sql).unwrap();
    assert_eq!(stmts.len(), 1);
    assert!(matches!(stmts[0], Statement::Update { .. }));
}

#[test]
fn test_parse_insert_select_returning_wildcard() {
    let stmts = parse_sql(
        "INSERT INTO insert_test (id, name, value)
         SELECT id + 100, UPPER(name), value * 2 FROM insert_source
         RETURNING *",
    )
    .unwrap();
    assert_eq!(stmts.len(), 1);
    match &stmts[0] {
        Statement::Insert { returning, .. } => {
            let returning = returning.as_ref().expect("expected RETURNING");
            assert!(matches!(returning.as_slice(), [SelectItem::Wildcard(_)]));
        }
        other => panic!("expected INSERT, got {:?}", other),
    }
}

#[test]
fn test_parse_single_digit_placeholders() {
    let stmts = parse_sql("INSERT INTO users (a, b, c) VALUES ($1, $2, $3)").unwrap();
    assert_eq!(stmts.len(), 1);
}

#[test]
fn test_parse_double_digit_placeholders() {
    let result = parse_sql("INSERT INTO users (a, b, c) VALUES ($10, $11, $12)");
    match result {
        Ok(stmts) => {
            assert_eq!(stmts.len(), 1);
            println!("Double-digit placeholders parsed successfully!");
        }
        Err(e) => {
            println!("Failed to parse double-digit placeholders: {}", e);
            panic!("sqlparser-rs doesn't support double-digit placeholders");
        }
    }
}

#[test]
fn test_parse_at_time_zone_placeholder() {
    let stmts = parse_sql("SELECT TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE $1").unwrap();
    assert_eq!(stmts.len(), 1);
}

#[test]
fn test_parse_reset_role_via_rewrite() {
    let stmts = parse_sql("RESET ROLE").unwrap();
    assert_eq!(stmts.len(), 1);
    assert!(matches!(
        stmts[0],
        Statement::SetRole {
            role_name: None,
            ..
        }
    ));
}

#[test]
fn test_rewrite_reset_role_is_statement_aware() {
    assert_eq!(preprocess_sql("RESET ROLE"), "SET ROLE NONE");
    assert_eq!(preprocess_sql("SELECT 'RESET ROLE'"), "SELECT 'RESET ROLE'");
    assert_eq!(
        preprocess_sql("-- RESET ROLE\nSELECT 1"),
        "-- RESET ROLE\nSELECT 1"
    );
    assert_eq!(
        preprocess_sql("RESET ROLE; SELECT 'RESET ROLE';"),
        "SET ROLE NONE; SELECT 'RESET ROLE';"
    );
}

#[test]
fn test_rewrite_does_not_touch_non_role_reset() {
    // Non-ROLE RESET is handled by the executor's raw SQL path (RawSqlKind::Reset),
    // not by parser rewrite. The parser should leave them unchanged.
    assert_eq!(preprocess_sql("RESET timezone"), "RESET timezone");
    assert_eq!(preprocess_sql("RESET ALL"), "RESET ALL");
    // RESET ROLE is still rewritten
    assert_eq!(preprocess_sql("RESET ROLE"), "SET ROLE NONE");
}

#[test]
fn test_rewrite_user_role_aliases_is_statement_aware() {
    assert_eq!(
        preprocess_sql("CREATE USER bob"),
        "CREATE ROLE bob WITH LOGIN"
    );
    assert_eq!(
        preprocess_sql("CREATE USER bob WITH PASSWORD 'test123'"),
        "CREATE ROLE bob WITH LOGIN PASSWORD 'test123'"
    );
    assert_eq!(
        preprocess_sql("ALTER USER bob WITH LOGIN"),
        "ALTER ROLE bob WITH LOGIN"
    );
    assert_eq!(preprocess_sql("DROP USER bob"), "DROP ROLE bob");
    assert_eq!(
        preprocess_sql("SELECT 'CREATE USER bob'"),
        "SELECT 'CREATE USER bob'"
    );
    assert_eq!(
        preprocess_sql("-- CREATE USER bob\nSELECT 1"),
        "-- CREATE USER bob\nSELECT 1"
    );
    assert_eq!(
        preprocess_sql("CREATE USER bob; SELECT 'DROP USER bob';"),
        "CREATE ROLE bob WITH LOGIN; SELECT 'DROP USER bob';"
    );
}

#[test]
fn test_rewrite_user_role_aliases_does_not_touch_user_mapping() {
    let sql = "CREATE USER MAPPING FOR CURRENT_USER SERVER s";
    assert_eq!(preprocess_sql(sql), sql);
}

#[test]
fn test_parse_multi_row_double_digit() {
    let sql = "INSERT INTO users (a, b, c) VALUES ($1, $2, $3), ($4, $5, $6), ($7, $8, $9), ($10, $11, $12)";
    let result = parse_sql(sql);
    match result {
        Ok(stmts) => {
            assert_eq!(stmts.len(), 1);
            println!("Multi-row with double-digit placeholders parsed successfully!");
        }
        Err(e) => {
            println!(
                "Failed to parse multi-row with double-digit placeholders: {}",
                e
            );
            panic!("sqlparser-rs issue with double-digit placeholders in multi-row INSERT");
        }
    }
}

#[test]
fn test_parse_create_sequence_out_of_order_pg_dump_style() {
    // `pg_dump` commonly emits START/INCREMENT/NO MINVALUE/NO MAXVALUE out of sqlparser-rs'
    // expected order. `parse_sql` should normalize it for compatibility.
    let sql = r#"
        CREATE SEQUENCE public.task_id_sequence
            START WITH 1
            INCREMENT BY 1
            NO MINVALUE
            NO MAXVALUE
            CACHE 1;
    "#;
    let stmts = parse_sql(sql).unwrap();
    assert_eq!(stmts.len(), 1);
}

#[test]
fn test_default_in_on_conflict() {
    let sql = r#"INSERT INTO t(a, b) VALUES (1, 2) ON CONFLICT (a) DO UPDATE SET b = DEFAULT"#;
    let statements = parse_sql(sql).unwrap();
    println!("Parsed: {:#?}", statements);
}

#[test]
fn test_explain_analyze_with_parens() {
    let sql = "EXPLAIN (ANALYZE) SELECT * FROM t";
    let stmts = parse_sql(sql).unwrap();
    assert_eq!(stmts.len(), 1);
    if let Statement::Explain { analyze, .. } = &stmts[0] {
        assert!(*analyze);
    } else {
        panic!("Expected EXPLAIN statement");
    }
}

#[test]
fn test_explain_analyze_verbose_with_parens() {
    let sql = "EXPLAIN (ANALYZE, VERBOSE) SELECT 1";
    let stmts = parse_sql(sql).unwrap();
    assert_eq!(stmts.len(), 1);
    if let Statement::Explain {
        analyze, verbose, ..
    } = &stmts[0]
    {
        assert!(*analyze);
        assert!(*verbose);
    } else {
        panic!("Expected EXPLAIN statement");
    }
}

#[test]
fn test_preprocess_explain() {
    assert_eq!(
        preprocess_sql("EXPLAIN (ANALYZE) SELECT 1"),
        "EXPLAIN ANALYZE SELECT 1"
    );
    assert_eq!(
        preprocess_sql("EXPLAIN (ANALYZE, VERBOSE) SELECT 1"),
        "EXPLAIN ANALYZE  VERBOSE SELECT 1"
    );
    assert_eq!(
        preprocess_sql("EXPLAIN ANALYZE SELECT 1"),
        "EXPLAIN ANALYZE SELECT 1"
    );
    assert_eq!(preprocess_sql("SELECT 1"), "SELECT 1");
}

#[test]
fn test_all_any_subqueries_parse_via_parse_compat_wrapper() {
    let all_sql = "SELECT 1 = ALL (SELECT x FROM t)";
    let all_preprocessed = preprocess_sql(all_sql);
    assert_eq!(all_preprocessed, "SELECT 1 = ALL (ARRAY(SELECT x FROM t))");
    let stmts = parse_sql(all_sql).unwrap();
    assert_eq!(stmts.len(), 1);

    let any_sql = "SELECT 1 > ANY (SELECT x FROM t)";
    let any_preprocessed = preprocess_sql(any_sql);
    assert_eq!(any_preprocessed, "SELECT 1 > ANY (ARRAY(SELECT x FROM t))");
    let stmts = parse_sql(any_sql).unwrap();
    assert_eq!(stmts.len(), 1);
}

#[test]
fn test_preprocess_keeps_all_any_inside_literals_and_comments() {
    assert_eq!(
        preprocess_sql("SELECT '1 = ALL (SELECT x FROM t)'"),
        "SELECT '1 = ALL (SELECT x FROM t)'"
    );
    assert_eq!(
        preprocess_sql("-- 1 > ANY (SELECT x FROM t)\nSELECT 1"),
        "-- 1 > ANY (SELECT x FROM t)\nSELECT 1"
    );
}

#[test]
fn test_create_sequence_options() {
    let cases = [
        ("no options", "CREATE SEQUENCE s1"),
        ("start only", "CREATE SEQUENCE s1 START 1"),
        ("start with", "CREATE SEQUENCE s1 START WITH 1"),
        ("minvalue", "CREATE SEQUENCE s1 MINVALUE 1"),
        ("maxvalue", "CREATE SEQUENCE s1 MAXVALUE 100"),
        ("cycle", "CREATE SEQUENCE s1 CYCLE"),
        ("no cycle", "CREATE SEQUENCE s1 NO CYCLE"),
        ("increment only", "CREATE SEQUENCE s1 INCREMENT 1"),
        ("increment by", "CREATE SEQUENCE s1 INCREMENT BY 1"),
        (
            "start + increment (order 1)",
            "CREATE SEQUENCE s1 START WITH 1 INCREMENT BY 1",
        ),
        (
            "start + increment (order 2)",
            "CREATE SEQUENCE s1 INCREMENT BY 1 START WITH 1",
        ),
        (
            "increment + start (no BY/WITH)",
            "CREATE SEQUENCE s1 INCREMENT 1 START 1",
        ),
    ];
    for (desc, sql) in cases {
        let result = parse_sql(sql);
        println!("{}: {:?}", desc, result.is_ok());
        if let Err(e) = &result {
            println!("  Error: {}", e);
        }
        assert!(result.is_ok(), "{} should parse: {}", desc, sql);
    }
}

#[test]
fn test_preprocess_create_sequence() {
    assert_eq!(
        preprocess_create_sequence("CREATE SEQUENCE s1 START WITH 1 INCREMENT BY 2"),
        Some("CREATE SEQUENCE s1 INCREMENT BY 2 START WITH 1".to_string())
    );
    assert_eq!(
        preprocess_create_sequence("CREATE SEQUENCE s1 INCREMENT BY 2 START WITH 1"),
        None
    );
    assert_eq!(
        preprocess_create_sequence("CREATE SEQUENCE s1 START 1"),
        None
    );
    println!("Testing with semicolon:");
    let result =
        preprocess_create_sequence("CREATE SEQUENCE test_inc START WITH 1 INCREMENT BY 1;");
    println!("Result: {:?}", result);

    println!("Testing multi-statement:");
    let result =
        preprocess_create_sequence("CREATE SEQUENCE s1 START WITH 1 INCREMENT BY 1;\nSELECT 1;");
    println!("Result: {:?}", result);
    assert_eq!(
        result,
        Some("CREATE SEQUENCE s1 INCREMENT BY 1 START WITH 1;\nSELECT 1;".to_string())
    );
}

#[test]
fn test_rewrite_vector_distance_l2() {
    let result = rewrite_vector_distance_ops("SELECT v <-> '[1,0,0]' FROM vec_test");
    assert!(result.contains("l2_distance(v, '[1,0,0]')"));
}

#[test]
fn test_rewrite_vector_distance_in_order_by() {
    let result = rewrite_vector_distance_ops("SELECT * FROM t ORDER BY v <#> '[1,0,0]' ASC");
    assert!(result.contains("inner_product(v, '[1,0,0]')"));
}

#[test]
fn test_rewrite_vector_distance_cosine() {
    let result = rewrite_vector_distance_ops("SELECT v <=> '[0,1,0]' FROM vec_test");
    assert!(result.contains("cosine_distance(v, '[0,1,0]')"));
}

#[test]
fn test_rewrite_vector_distance_with_comparison() {
    let result =
        rewrite_vector_distance_ops("SELECT * FROM t WHERE a <#> b < -0.2 ORDER BY a <#> b ASC");
    assert!(
        result.contains("(inner_product(a, b)) < -0.2"),
        "got: {}",
        result
    );
    assert!(
        result.contains("(inner_product(a, b)) ASC"),
        "got: {}",
        result
    );
}

#[test]
fn test_rewrite_vector_distance_comparison_boundary() {
    let result = rewrite_vector_distance_ops("SELECT a <-> b > 5 FROM t");
    assert!(
        result.contains("(l2_distance(a, b)) > 5"),
        "got: {}",
        result
    );
}

#[test]
fn test_rewrite_vector_distance_join_on_boundary() {
    let result = rewrite_vector_distance_ops("SELECT * FROM t1 JOIN t2 ON t1.v <-> t2.v < 1");
    assert!(
        result.contains("(l2_distance(t1.v, t2.v)) < 1"),
        "JOIN/ON should be boundaries; got: {}",
        result
    );
}

#[test]
fn test_rewrite_vector_distance_left_join_on() {
    let result = rewrite_vector_distance_ops(
        "SELECT * FROM t1 LEFT JOIN t2 ON t1.v <=> t2.v < 0.5 ORDER BY t1.v <-> t2.v",
    );
    assert!(
        result.contains("(cosine_distance(t1.v, t2.v)) < 0.5"),
        "LEFT JOIN ON should be boundaries; got: {}",
        result
    );
    assert!(
        result.contains("(l2_distance(t1.v, t2.v))"),
        "ORDER BY rewrite should work; got: {}",
        result
    );
}

#[test]
fn test_rewrite_vector_distance_no_false_positive_in_strings() {
    let result = rewrite_vector_distance_ops("SELECT '<->' FROM t");
    assert_eq!(result, "SELECT '<->' FROM t");
}

#[test]
fn test_rewrite_jsonb_exists_ops_keeps_arrow_left_expr() {
    let sql = "SELECT 1 WHERE data->'tags' ? 'sale'";
    let rewritten = rewrite_jsonb_exists_ops(sql);
    assert_eq!(
        rewritten,
        "SELECT 1 WHERE (JSONB_EXISTS(data->'tags', 'sale'))"
    );
}

#[test]
fn test_rewrite_vector_distance_parses() {
    let sql = "SELECT v <-> '[1,0,0]'::vector(3) FROM vec_test";
    let stmts = parse_sql(sql).unwrap();
    assert_eq!(stmts.len(), 1);
}
