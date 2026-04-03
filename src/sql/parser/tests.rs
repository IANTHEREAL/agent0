//! Tests for the SQL parser module.

use super::*;
use operator_rewrite::{
    rewrite_jsonb_exists_ops, rewrite_table_shorthand, rewrite_vector_distance_ops,
};
use preprocess::{preprocess_create_sequence, preprocess_sql};
use tokenizer::{tokenize_sql_for_rewrite, TokenKind};

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
fn test_parse_custom_typed_string_select() {
    let stmts = parse_sql("SELECT mood 'happy'").unwrap();
    assert_eq!(stmts.len(), 1);

    let Statement::Query(query) = &stmts[0] else {
        panic!("expected query statement");
    };
    let sqlparser::ast::SetExpr::Select(select) = query.body.as_ref() else {
        panic!("expected SELECT query body");
    };
    // The sqlparser fork produces TypedString for custom types, but parse_sql()
    // normalizes Custom TypedStrings to Cast for downstream compatibility.
    let Some(sqlparser::ast::SelectItem::UnnamedExpr(sqlparser::ast::Expr::Cast {
        expr,
        data_type,
        ..
    })) = select.projection.first()
    else {
        panic!("expected Cast projection, got {:?}", select.projection);
    };

    assert!(matches!(
        expr.as_ref(),
        sqlparser::ast::Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) if s == "happy"
    ));
    assert!(matches!(
        data_type,
        sqlparser::ast::DataType::Custom(name, _) if name.to_string() == "mood"
    ));
}

#[test]
fn test_parse_custom_typed_string_default_expression() {
    let stmts =
        parse_sql("CREATE TABLE t (c mood DEFAULT coalesce(mood 'happy', mood 'sad'))").unwrap();
    assert_eq!(stmts.len(), 1);
    assert!(matches!(stmts[0], Statement::CreateTable { .. }));
}

#[test]
fn test_parse_custom_typed_string_dollar_quoted() {
    let stmts = parse_sql("SELECT mood $$happy$$").unwrap();
    assert_eq!(stmts.len(), 1);
}

#[test]
fn test_parse_custom_typed_string_escape_prefix() {
    let stmts = parse_sql("SELECT mood E'happy'").unwrap();
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
    assert_eq!(preprocess_sql("RESET ROLE").unwrap(), "SET ROLE NONE");
    assert_eq!(
        preprocess_sql("SELECT 'RESET ROLE'").unwrap(),
        "SELECT 'RESET ROLE'"
    );
    assert_eq!(
        preprocess_sql("-- RESET ROLE\nSELECT 1").unwrap(),
        "-- RESET ROLE\nSELECT 1"
    );
    assert_eq!(
        preprocess_sql("RESET ROLE; SELECT 'RESET ROLE';").unwrap(),
        "SET ROLE NONE; SELECT 'RESET ROLE';"
    );
}

#[test]
fn test_rewrite_does_not_touch_non_role_reset() {
    // Non-ROLE RESET is handled by the executor's raw SQL path (RawSqlKind::Reset),
    // not by parser rewrite. The parser should leave them unchanged.
    assert_eq!(preprocess_sql("RESET timezone").unwrap(), "RESET timezone");
    assert_eq!(preprocess_sql("RESET ALL").unwrap(), "RESET ALL");
    // RESET ROLE is still rewritten
    assert_eq!(preprocess_sql("RESET ROLE").unwrap(), "SET ROLE NONE");
}

#[test]
fn test_rewrite_user_role_aliases_is_statement_aware() {
    assert_eq!(
        preprocess_sql("CREATE USER bob").unwrap(),
        "CREATE ROLE bob WITH LOGIN"
    );
    assert_eq!(
        preprocess_sql("CREATE USER bob WITH PASSWORD 'test123'").unwrap(),
        "CREATE ROLE bob WITH LOGIN PASSWORD 'test123'"
    );
    assert_eq!(
        preprocess_sql("ALTER USER bob WITH LOGIN").unwrap(),
        "ALTER ROLE bob WITH LOGIN"
    );
    assert_eq!(preprocess_sql("DROP USER bob").unwrap(), "DROP ROLE bob");
    assert_eq!(
        preprocess_sql("SELECT 'CREATE USER bob'").unwrap(),
        "SELECT 'CREATE USER bob'"
    );
    assert_eq!(
        preprocess_sql("-- CREATE USER bob\nSELECT 1").unwrap(),
        "-- CREATE USER bob\nSELECT 1"
    );
    assert_eq!(
        preprocess_sql("CREATE USER bob; SELECT 'DROP USER bob';").unwrap(),
        "CREATE ROLE bob WITH LOGIN; SELECT 'DROP USER bob';"
    );
}

#[test]
fn test_rewrite_user_role_aliases_does_not_touch_user_mapping() {
    let sql = "CREATE USER MAPPING FOR CURRENT_USER SERVER s";
    assert_eq!(preprocess_sql(sql).unwrap(), sql);
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
        preprocess_sql("EXPLAIN (ANALYZE) SELECT 1").unwrap(),
        "EXPLAIN ANALYZE SELECT 1"
    );
    assert_eq!(
        preprocess_sql("EXPLAIN (ANALYZE, VERBOSE) SELECT 1").unwrap(),
        "EXPLAIN ANALYZE  VERBOSE SELECT 1"
    );
    assert_eq!(
        preprocess_sql("EXPLAIN ANALYZE SELECT 1").unwrap(),
        "EXPLAIN ANALYZE SELECT 1"
    );
    assert_eq!(preprocess_sql("SELECT 1").unwrap(), "SELECT 1");
}

#[test]
fn test_all_any_subqueries_parse_via_parse_compat_wrapper() {
    let all_sql = "SELECT 1 = ALL (SELECT x FROM t)";
    let all_preprocessed = preprocess_sql(all_sql).unwrap();
    assert_eq!(all_preprocessed, "SELECT 1 = ALL (ARRAY(SELECT x FROM t))");
    let stmts = parse_sql(all_sql).unwrap();
    assert_eq!(stmts.len(), 1);

    let any_sql = "SELECT 1 > ANY (SELECT x FROM t)";
    let any_preprocessed = preprocess_sql(any_sql).unwrap();
    assert_eq!(any_preprocessed, "SELECT 1 > ANY (ARRAY(SELECT x FROM t))");
    let stmts = parse_sql(any_sql).unwrap();
    assert_eq!(stmts.len(), 1);
}

#[test]
fn test_preprocess_keeps_all_any_inside_literals_and_comments() {
    assert_eq!(
        preprocess_sql("SELECT '1 = ALL (SELECT x FROM t)'").unwrap(),
        "SELECT '1 = ALL (SELECT x FROM t)'"
    );
    assert_eq!(
        preprocess_sql("-- 1 > ANY (SELECT x FROM t)\nSELECT 1").unwrap(),
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

// ── TABLE shorthand rewrite tests ──

#[test]
fn test_table_shorthand_basic() {
    assert_eq!(
        rewrite_table_shorthand("TABLE mytable").unwrap(),
        "SELECT * FROM mytable"
    );
}

#[test]
fn test_table_shorthand_only() {
    assert_eq!(
        rewrite_table_shorthand("TABLE ONLY t ORDER BY id LIMIT 5").unwrap(),
        "SELECT * FROM t ORDER BY id LIMIT 5"
    );
}

#[test]
fn test_table_shorthand_star() {
    assert_eq!(
        rewrite_table_shorthand("TABLE t * ORDER BY id LIMIT 5").unwrap(),
        "SELECT * FROM t ORDER BY id LIMIT 5"
    );
}

#[test]
fn test_table_shorthand_only_star_rejected() {
    // PG rejects TABLE ONLY <rel> * — ONLY and * are mutually exclusive.
    let err = rewrite_table_shorthand("TABLE ONLY t *").unwrap_err();
    assert!(
        err.contains("at or near"),
        "expected position hint, got: {err}"
    );
}

#[test]
fn test_table_shorthand_schema_qualified() {
    assert_eq!(
        rewrite_table_shorthand("TABLE myschema.mytable").unwrap(),
        "SELECT * FROM myschema.mytable"
    );
}

#[test]
fn test_table_shorthand_quoted() {
    assert_eq!(
        rewrite_table_shorthand(r#"TABLE "MyTable""#).unwrap(),
        r#"SELECT * FROM "MyTable""#
    );
}

#[test]
fn test_table_shorthand_case_insensitive() {
    assert_eq!(
        rewrite_table_shorthand("table mytable").unwrap(),
        "SELECT * FROM mytable"
    );
}

#[test]
fn test_table_shorthand_ignores_ddl() {
    let sql = "CREATE TABLE t (id int)";
    assert_eq!(rewrite_table_shorthand(sql).unwrap(), sql);
}

#[test]
fn test_table_shorthand_ignores_alter() {
    let sql = "ALTER TABLE t ADD COLUMN c INT";
    assert_eq!(rewrite_table_shorthand(sql).unwrap(), sql);
}

#[test]
fn test_table_shorthand_ignores_drop() {
    let sql = "DROP TABLE t";
    assert_eq!(rewrite_table_shorthand(sql).unwrap(), sql);
}

#[test]
fn test_table_shorthand_explain() {
    assert_eq!(
        rewrite_table_shorthand("EXPLAIN TABLE t").unwrap(),
        "EXPLAIN SELECT * FROM t"
    );
}

#[test]
fn test_table_shorthand_explain_analyze() {
    assert_eq!(
        rewrite_table_shorthand("EXPLAIN ANALYZE TABLE t").unwrap(),
        "EXPLAIN ANALYZE SELECT * FROM t"
    );
}

#[test]
fn test_table_shorthand_multi_stmt() {
    assert_eq!(
        rewrite_table_shorthand("TABLE t1; CREATE TABLE t2 (id int)").unwrap(),
        "SELECT * FROM t1; CREATE TABLE t2 (id int)"
    );
}

#[test]
fn test_table_shorthand_trailing_offset_fetch() {
    assert_eq!(
        rewrite_table_shorthand("TABLE t OFFSET 5 FETCH NEXT 10 ROWS ONLY").unwrap(),
        "SELECT * FROM t OFFSET 5 FETCH NEXT 10 ROWS ONLY"
    );
}

#[test]
fn test_table_shorthand_for_update() {
    assert_eq!(
        rewrite_table_shorthand("TABLE t FOR UPDATE").unwrap(),
        "SELECT * FROM t FOR UPDATE"
    );
}

#[test]
fn test_table_shorthand_ignores_string_literal() {
    let sql = "SELECT 'TABLE mytable'";
    assert_eq!(rewrite_table_shorthand(sql).unwrap(), sql);
}

#[test]
fn test_table_shorthand_ignores_comment() {
    let sql = "-- TABLE mytable\nSELECT 1";
    assert_eq!(rewrite_table_shorthand(sql).unwrap(), sql);
}

// ── Regression guards (prevents `relation "only" does not exist`) ──

#[test]
fn test_table_shorthand_only_not_relation() {
    let sql = "TABLE ONLY t ORDER BY id LIMIT 5";
    let stmts = parse_sql(sql).unwrap();
    assert_eq!(stmts.len(), 1);
    assert!(matches!(stmts[0], Statement::Query(_)));
}

#[test]
fn test_table_shorthand_star_not_relation() {
    let sql = "TABLE t * ORDER BY id";
    let stmts = parse_sql(sql).unwrap();
    assert_eq!(stmts.len(), 1);
    assert!(matches!(stmts[0], Statement::Query(_)));
}

#[test]
fn test_table_shorthand_only_star_rejected_parse() {
    // PG rejects TABLE ONLY <rel> * at parse time.
    assert!(parse_sql("TABLE ONLY t *").is_err());
}

// ── Malformed-syntax negatives ──

#[test]
fn test_table_shorthand_bare_only_fails() {
    assert!(parse_sql("TABLE ONLY").is_err());
}

#[test]
fn test_table_shorthand_bare_table_fails() {
    assert!(parse_sql("TABLE").is_err());
}

#[test]
fn test_table_shorthand_basic_parses() {
    let stmts = parse_sql("TABLE mytable").unwrap();
    assert_eq!(stmts.len(), 1);
    assert!(matches!(stmts[0], Statement::Query(_)));
}

#[test]
fn test_table_shorthand_via_preprocess_sql() {
    assert_eq!(
        preprocess_sql("TABLE mytable").unwrap(),
        "SELECT * FROM mytable"
    );
    assert_eq!(
        preprocess_sql("TABLE ONLY t ORDER BY id DESC LIMIT 1").unwrap(),
        "SELECT * FROM t ORDER BY id DESC LIMIT 1"
    );
    assert_eq!(
        preprocess_sql("TABLE t * ORDER BY id DESC LIMIT 1").unwrap(),
        "SELECT * FROM t ORDER BY id DESC LIMIT 1"
    );
    assert_eq!(
        preprocess_sql("CREATE TABLE t (id int)").unwrap(),
        "CREATE TABLE t (id int)"
    );
}

#[test]
fn test_table_shorthand_invalid_tail_rejected() {
    // PG rejects non-TABLE clauses after TABLE <relation>.
    assert!(rewrite_table_shorthand("TABLE t WHERE x = 1").is_err());
    assert!(rewrite_table_shorthand("TABLE t GROUP BY id").is_err());
    assert!(parse_sql("TABLE t WHERE x = 1").is_err());
}

// ── P1 blocker regression tests ──

#[test]
fn test_table_shorthand_as_alias_not_rewritten() {
    // P1: SELECT 1 AS table must NOT be rewritten — AS is not a TABLE shorthand position.
    let sql = r#"SELECT 1 AS "table""#;
    assert_eq!(rewrite_table_shorthand(sql).unwrap(), sql);
}

#[test]
fn test_table_shorthand_as_alias_parses() {
    // Ensure SELECT ... AS table_alias round-trips through parser.
    let stmts = parse_sql(r#"SELECT 1 AS "table""#).unwrap();
    assert_eq!(stmts.len(), 1);
}

#[test]
fn test_table_shorthand_with_cte() {
    // P1: WITH cte AS (...) TABLE cte must be recognized as TABLE shorthand.
    assert_eq!(
        rewrite_table_shorthand("WITH cte AS (SELECT 1 AS x) TABLE cte").unwrap(),
        "WITH cte AS (SELECT 1 AS x) SELECT * FROM cte"
    );
}

#[test]
fn test_table_shorthand_with_cte_parses() {
    let stmts = parse_sql("WITH cte AS (SELECT 1 AS x) TABLE cte").unwrap();
    assert_eq!(stmts.len(), 1);
    assert!(matches!(stmts[0], Statement::Query(_)));
}

#[test]
fn test_table_shorthand_union_distinct() {
    // P2: UNION DISTINCT TABLE must be recognized.
    assert_eq!(
        rewrite_table_shorthand("SELECT 1 UNION DISTINCT TABLE t").unwrap(),
        "SELECT 1 UNION DISTINCT SELECT * FROM t"
    );
}

#[test]
fn test_table_shorthand_except_distinct() {
    assert_eq!(
        rewrite_table_shorthand("SELECT 1 EXCEPT DISTINCT TABLE t").unwrap(),
        "SELECT 1 EXCEPT DISTINCT SELECT * FROM t"
    );
}

#[test]
fn test_table_shorthand_explain_format_text() {
    // P2: EXPLAIN (FORMAT TEXT) TABLE t after preprocess_explain becomes
    // EXPLAIN FORMAT TEXT TABLE t — TEXT must be recognized as EXPLAIN context.
    assert_eq!(
        rewrite_table_shorthand("EXPLAIN FORMAT TEXT TABLE t").unwrap(),
        "EXPLAIN FORMAT TEXT SELECT * FROM t"
    );
}

#[test]
fn test_table_shorthand_explain_costs_off() {
    assert_eq!(
        rewrite_table_shorthand("EXPLAIN COSTS OFF TABLE t").unwrap(),
        "EXPLAIN COSTS OFF SELECT * FROM t"
    );
}

#[test]
fn test_table_shorthand_explain_options_via_preprocess() {
    // Full pipeline: EXPLAIN (FORMAT TEXT) TABLE t
    assert_eq!(
        preprocess_sql("EXPLAIN (FORMAT TEXT) TABLE t").unwrap(),
        "EXPLAIN FORMAT TEXT SELECT * FROM t"
    );
}

// ---------------------------------------------------------------------------
// UTF-8 / multibyte safety tests for tokenizer and parse_sql
// ---------------------------------------------------------------------------

// --- Tokenizer-level: tokenize_sql_for_rewrite must not panic on multibyte ---

#[test]
fn test_tokenizer_chinese_characters_no_panic() {
    // Bare Chinese text outside quotes — must tokenize without panic
    let tokens = tokenize_sql_for_rewrite("SELECT 模型能力总结 FROM t");
    assert!(!tokens.is_empty());
}

#[test]
fn test_tokenizer_japanese_characters_no_panic() {
    let tokens = tokenize_sql_for_rewrite("SELECT テスト FROM t");
    assert!(!tokens.is_empty());
}

#[test]
fn test_tokenizer_korean_characters_no_panic() {
    let tokens = tokenize_sql_for_rewrite("SELECT 테스트 FROM t");
    assert!(!tokens.is_empty());
}

#[test]
fn test_tokenizer_emoji_no_panic() {
    let tokens = tokenize_sql_for_rewrite("SELECT 🚀 FROM t");
    assert!(!tokens.is_empty());
}

#[test]
fn test_tokenizer_mixed_ascii_and_multibyte() {
    // Multibyte chars adjacent to ASCII keywords/operators
    let tokens = tokenize_sql_for_rewrite("SELECT id, 名前 FROM users WHERE name = '日本語'");
    assert!(!tokens.is_empty());
    // 'SELECT' should be a Word token
    let first_word = tokens.iter().find(|t| t.kind == TokenKind::Word).unwrap();
    assert_eq!(first_word.text, "SELECT");
}

#[test]
fn test_tokenizer_multibyte_as_word_tokens() {
    // After the fix, multibyte chars should be consumed as Word tokens (PG/TiDB behavior)
    let tokens = tokenize_sql_for_rewrite("表名");
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].kind, TokenKind::Word);
    assert_eq!(tokens[0].text, "表名");
}

#[test]
fn test_tokenizer_multibyte_between_ascii_words() {
    let tokens = tokenize_sql_for_rewrite("SELECT 名前 FROM t");
    let words: Vec<&str> = tokens
        .iter()
        .filter(|t| t.kind == TokenKind::Word)
        .map(|t| t.text.as_str())
        .collect();
    assert_eq!(words, vec!["SELECT", "名前", "FROM", "t"]);
}

#[test]
fn test_tokenizer_multibyte_adjacent_to_ascii_ident() {
    // e.g. "abc中文def" — all bytes form one contiguous Word token
    let tokens = tokenize_sql_for_rewrite("abc中文def");
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].kind, TokenKind::Word);
    assert_eq!(tokens[0].text, "abc中文def");
}

#[test]
fn test_tokenizer_multibyte_in_string_literal() {
    let tokens = tokenize_sql_for_rewrite("SELECT '日本語テスト'");
    let strings: Vec<&str> = tokens
        .iter()
        .filter(|t| t.kind == TokenKind::StringLiteral)
        .map(|t| t.text.as_str())
        .collect();
    assert_eq!(strings, vec!["'日本語テスト'"]);
}

#[test]
fn test_tokenizer_multibyte_in_double_quoted_ident() {
    let tokens = tokenize_sql_for_rewrite(r#"SELECT "列名" FROM t"#);
    let quoted: Vec<&str> = tokens
        .iter()
        .filter(|t| t.kind == TokenKind::QuotedIdent)
        .map(|t| t.text.as_str())
        .collect();
    assert_eq!(quoted, vec![r#""列名""#]);
}

#[test]
fn test_tokenizer_multibyte_in_dollar_string() {
    let tokens = tokenize_sql_for_rewrite("$$日本語テスト$$");
    let dollar: Vec<&str> = tokens
        .iter()
        .filter(|t| t.kind == TokenKind::DollarString)
        .map(|t| t.text.as_str())
        .collect();
    assert_eq!(dollar, vec!["$$日本語テスト$$"]);
}

#[test]
fn test_tokenizer_multibyte_in_line_comment() {
    let tokens = tokenize_sql_for_rewrite("-- 这是注释\nSELECT 1");
    assert_eq!(tokens[0].kind, TokenKind::Comment);
    assert!(tokens[0].text.contains("这是注释"));
}

#[test]
fn test_tokenizer_multibyte_in_block_comment() {
    let tokens = tokenize_sql_for_rewrite("/* コメント */ SELECT 1");
    assert_eq!(tokens[0].kind, TokenKind::Comment);
    assert!(tokens[0].text.contains("コメント"));
}

#[test]
fn test_tokenizer_two_byte_utf8() {
    // Latin extended: é is 2-byte UTF-8 (0xC3 0xA9)
    let tokens = tokenize_sql_for_rewrite("SELECT café FROM t");
    let words: Vec<&str> = tokens
        .iter()
        .filter(|t| t.kind == TokenKind::Word)
        .map(|t| t.text.as_str())
        .collect();
    assert_eq!(words, vec!["SELECT", "café", "FROM", "t"]);
}

#[test]
fn test_tokenizer_four_byte_utf8() {
    // Emoji: 🎉 is 4-byte UTF-8
    let tokens = tokenize_sql_for_rewrite("SELECT 🎉 FROM t");
    assert!(!tokens.is_empty());
    let words: Vec<&str> = tokens
        .iter()
        .filter(|t| t.kind == TokenKind::Word)
        .map(|t| t.text.as_str())
        .collect();
    assert!(words.contains(&"SELECT"));
    assert!(words.contains(&"FROM"));
}

#[test]
fn test_tokenizer_byte_offsets_correct_with_multibyte() {
    let sql = "SELECT 名前 FROM t";
    let tokens = tokenize_sql_for_rewrite(sql);
    // Every token's start..end must slice back to token text
    for tok in &tokens {
        assert_eq!(
            &sql[tok.start..tok.end],
            tok.text,
            "offset mismatch for {:?}",
            tok
        );
    }
}

#[test]
fn test_tokenizer_issue_2299_repro() {
    // Exact reproduction case from issue #2299: bare Chinese text in invalid SQL
    let sql = "SELECT id, message FROM agent_discussion WHERE id > gemma4:26b 模型能力总结";
    let tokens = tokenize_sql_for_rewrite(sql);
    assert!(!tokens.is_empty());
    // Verify all token offsets are valid UTF-8 boundaries
    for tok in &tokens {
        assert_eq!(
            &sql[tok.start..tok.end],
            tok.text,
            "offset mismatch for {:?}",
            tok
        );
    }
}

// --- parse_sql()-level: multibyte in invalid SQL must return Err, not panic ---

#[test]
fn test_parse_sql_bare_chinese_is_valid_identifier() {
    // Bare Chinese text is a valid identifier in PostgreSQL (same as PG/TiDB behavior).
    // It parses successfully — semantic errors (column not found) happen later in analysis.
    let result = parse_sql("SELECT id FROM t WHERE x > 模型能力总结");
    assert!(result.is_ok());
}

#[test]
fn test_parse_sql_issue_2299_full_repro() {
    let result =
        parse_sql("SELECT id, message FROM agent_discussion WHERE id > gemma4:26b 模型能力总结：Gemma4在多项基准测试中展现出色的多模态理解能力");
    assert!(result.is_err());
}

#[test]
fn test_parse_sql_chinese_in_string_literal_ok() {
    // Valid SQL with Chinese inside a string literal — must parse successfully
    let result = parse_sql("SELECT '中文测试' AS label");
    assert!(result.is_ok());
}

#[test]
fn test_parse_sql_chinese_in_double_quoted_ident_ok() {
    // Valid SQL with Chinese inside double-quoted identifier
    let result = parse_sql(r#"SELECT "列名" FROM t"#);
    assert!(result.is_ok());
}

// --- Rewrite-level: rewrite functions must not panic on multibyte input ---

#[test]
fn test_rewrite_all_any_with_multibyte_no_panic() {
    use operator_rewrite::rewrite_all_any_subquery_parse_compat;
    let result =
        rewrite_all_any_subquery_parse_compat("SELECT * FROM t WHERE 模型 = ANY(ARRAY[1])");
    assert!(result.contains("ANY"));
}

#[test]
fn test_rewrite_jsonb_exists_with_multibyte_no_panic() {
    let result = rewrite_jsonb_exists_ops("SELECT * FROM t WHERE data ? '模型'");
    assert!(!result.is_empty());
}

#[test]
fn test_rewrite_vector_distance_with_multibyte_no_panic() {
    let result = rewrite_vector_distance_ops("SELECT 名前 FROM t ORDER BY embedding <-> '[1,2,3]'");
    assert!(!result.is_empty());
}
