//! Unit tests for the PL/pgSQL module.

use super::{
    ast_bind::bind_sql_statements, utils::replace_identifier, validate_plpgsql_body, PlpgsqlContext,
};
use crate::model::{DataType, Value};

#[test]
fn test_replace_identifier_respects_word_boundaries() {
    assert_eq!(replace_identifier("n + 1", "n", "5"), "5 + 1");
    assert_eq!(replace_identifier("nn + n", "n", "5"), "nn + 5");
}

#[test]
fn test_replace_identifier_preserves_utf8() {
    let input = "'你好' || n";
    let output = replace_identifier(input, "n", "5");
    assert_eq!(output, "'你好' || 5");
}

#[test]
fn test_replace_identifier_skips_string_literals() {
    assert_eq!(
        replace_identifier("'negative' || n", "n", "5"),
        "'negative' || 5"
    );
    assert_eq!(
        replace_identifier("CASE WHEN n < 0 THEN 'negative' END", "n", "-5"),
        "CASE WHEN -5 < 0 THEN 'negative' END"
    );
    assert_eq!(
        replace_identifier("'it''s a test' || n", "n", "5"),
        "'it''s a test' || 5"
    );
}

#[test]
fn test_validate_plpgsql_body_with_non_ascii() {
    let body = "BEGIN\n    -- cafe ≈ naive\n    RETURN 1;\nEND;";
    assert!(validate_plpgsql_body(body).is_ok());
}

#[test]
fn test_validate_plpgsql_body_with_unicode_in_strings() {
    let body = "DECLARE\n    v TEXT;\nBEGIN\n    v := '日本語テスト';\n    RETURN v;\nEND;";
    assert!(validate_plpgsql_body(body).is_ok());
}

#[test]
fn test_validate_plpgsql_body_with_emoji() {
    let body = "BEGIN\n    -- 🎉 celebration\n    RETURN 42;\nEND;";
    assert!(validate_plpgsql_body(body).is_ok());
}

#[test]
fn test_validate_plpgsql_body_with_create_table_if_not_exists() {
    let body = r#"
BEGIN
    CREATE TABLE IF NOT EXISTS t_ddl_breaks(id int);
    RETURN 'ok';
END;
"#;
    assert!(validate_plpgsql_body(body).is_ok());
}

#[test]
fn test_validate_plpgsql_body_requires_outer_end() {
    let body = "BEGIN\n    RETURN 1;\n";
    let err = validate_plpgsql_body(body).expect_err("missing END must be rejected");
    assert!(err.to_string().contains("Missing END for BEGIN block"));
}

// ── Tests for function_name.param_name qualified parameter binding ──

fn make_ctx_with_function(function_name: &str, vars: &[(&str, Value, DataType)]) -> PlpgsqlContext {
    let mut ctx = PlpgsqlContext::new();
    ctx.function_name = function_name.to_lowercase();
    for (name, val, dt) in vars {
        ctx.variables.insert(name.to_string(), val.clone());
        ctx.variable_types.insert(name.to_string(), dt.clone());
    }
    ctx
}

#[test]
fn test_qualified_param_binding_resolves_function_dot_param() {
    // `my_func.project_id` should resolve to the parameter value
    let ctx = make_ctx_with_function(
        "my_func",
        &[("project_id", Value::Text("abc".into()), DataType::Text)],
    );
    let stmts = bind_sql_statements(
        "SELECT * FROM t WHERE t.project_id = my_func.project_id",
        &ctx,
    )
    .unwrap();
    let sql = stmts[0].to_string();
    // my_func.project_id → 'abc', but t.project_id stays as column ref
    assert!(sql.contains("'abc'"), "expected bound value, got: {sql}");
    assert!(
        sql.contains("t.project_id"),
        "table.col should remain, got: {sql}"
    );
}

#[test]
fn test_qualified_param_binding_ignores_other_qualifiers() {
    // `other_func.project_id` should NOT be resolved
    let ctx = make_ctx_with_function(
        "my_func",
        &[("project_id", Value::Text("abc".into()), DataType::Text)],
    );
    let stmts = bind_sql_statements("SELECT other_func.project_id FROM t", &ctx).unwrap();
    let sql = stmts[0].to_string();
    assert!(
        sql.contains("other_func.project_id"),
        "non-matching qualifier should remain, got: {sql}"
    );
}

#[test]
fn test_qualified_param_binding_case_insensitive() {
    // Function name matching should be case-insensitive
    let ctx = make_ctx_with_function(
        "swarm_try_claim",
        &[("agent_id", Value::Text("w1".into()), DataType::Text)],
    );
    let stmts = bind_sql_statements(
        "SELECT * FROM t WHERE t.agent_id = Swarm_Try_Claim.agent_id",
        &ctx,
    )
    .unwrap();
    let sql = stmts[0].to_string();
    assert!(
        sql.contains("'w1'"),
        "case-insensitive match failed, got: {sql}"
    );
}

#[test]
fn test_qualified_param_unknown_param_left_alone() {
    // `my_func.unknown_var` — qualifier matches but param doesn't exist
    let ctx = make_ctx_with_function(
        "my_func",
        &[("project_id", Value::Text("abc".into()), DataType::Text)],
    );
    let stmts = bind_sql_statements("SELECT my_func.unknown_var FROM t", &ctx).unwrap();
    let sql = stmts[0].to_string();
    assert!(
        sql.contains("my_func.unknown_var"),
        "unknown param should remain, got: {sql}"
    );
}

#[test]
fn test_qualified_param_in_where_clause_with_comparison() {
    // Real-world pattern from swarm_try_claim: WHERE t.project_id = swarm_try_claim.project_id
    let ctx = make_ctx_with_function(
        "swarm_try_claim",
        &[
            ("project_id", Value::Text("demo".into()), DataType::Text),
            ("task_id", Value::Text("T1".into()), DataType::Text),
        ],
    );
    let stmts = bind_sql_statements(
        "UPDATE swarm_tasks SET status = 'claimed' WHERE project_id = swarm_try_claim.project_id AND task_id = swarm_try_claim.task_id",
        &ctx,
    )
    .unwrap();
    let sql = stmts[0].to_string();
    assert!(sql.contains("'demo'"), "project_id not bound, got: {sql}");
    assert!(sql.contains("'T1'"), "task_id not bound, got: {sql}");
}

#[test]
fn test_qualified_param_int_type() {
    let ctx = make_ctx_with_function("my_func", &[("priority", Value::Int32(9), DataType::Int32)]);
    let stmts =
        bind_sql_statements("SELECT * FROM t WHERE t.priority = my_func.priority", &ctx).unwrap();
    let sql = stmts[0].to_string();
    assert!(sql.contains(" 9"), "int param not bound, got: {sql}");
}

#[test]
fn test_qualified_param_bool_type() {
    let ctx = make_ctx_with_function(
        "my_func",
        &[("flag", Value::Boolean(true), DataType::Boolean)],
    );
    let stmts = bind_sql_statements("SELECT * FROM t WHERE my_func.flag", &ctx).unwrap();
    let sql = stmts[0].to_string();
    assert!(
        sql.to_lowercase().contains("true"),
        "bool param not bound, got: {sql}"
    );
}

#[test]
fn test_qualified_param_in_exists_subquery() {
    // Real-world pattern from swarm_try_claim:
    // IF NOT EXISTS (SELECT 1 FROM swarm_tasks t WHERE t.project_id = swarm_try_claim.project_id)
    let ctx = make_ctx_with_function(
        "swarm_try_claim",
        &[
            ("project_id", Value::Text("demo".into()), DataType::Text),
            ("task_id", Value::Text("T1".into()), DataType::Text),
        ],
    );
    let stmts = bind_sql_statements(
        "SELECT NOT EXISTS (SELECT 1 FROM swarm_tasks t WHERE t.project_id = swarm_try_claim.project_id AND t.task_id = swarm_try_claim.task_id)",
        &ctx,
    )
    .unwrap();
    let sql = stmts[0].to_string();
    assert!(
        sql.contains("'demo'"),
        "project_id inside EXISTS subquery not bound, got: {sql}"
    );
    assert!(
        sql.contains("'T1'"),
        "task_id inside EXISTS subquery not bound, got: {sql}"
    );
    // The table alias t.project_id should remain
    assert!(
        sql.contains("t.project_id"),
        "table.col should remain in subquery, got: {sql}"
    );
}

#[test]
fn test_qualified_param_in_scalar_subquery() {
    // Scalar subquery: (SELECT count(*) FROM t WHERE t.id = func.id)
    let ctx = make_ctx_with_function(
        "my_func",
        &[("id", Value::Text("abc".into()), DataType::Text)],
    );
    let stmts = bind_sql_statements(
        "SELECT (SELECT count(*) FROM t WHERE t.id = my_func.id)",
        &ctx,
    )
    .unwrap();
    let sql = stmts[0].to_string();
    assert!(
        sql.contains("'abc'"),
        "param inside scalar subquery not bound, got: {sql}"
    );
}

#[test]
fn test_qualified_param_in_subquery() {
    // IN subquery: WHERE x IN (SELECT id FROM t WHERE t.pid = func.pid)
    let ctx = make_ctx_with_function(
        "my_func",
        &[("pid", Value::Text("p1".into()), DataType::Text)],
    );
    let stmts = bind_sql_statements(
        "SELECT * FROM s WHERE s.id IN (SELECT id FROM t WHERE t.pid = my_func.pid)",
        &ctx,
    )
    .unwrap();
    let sql = stmts[0].to_string();
    assert!(
        sql.contains("'p1'"),
        "param inside IN subquery not bound, got: {sql}"
    );
}

#[test]
fn test_qualified_param_three_part_identifier_untouched() {
    // schema.table.column should never be touched even if middle part matches function name
    let ctx = make_ctx_with_function(
        "my_func",
        &[("col", Value::Text("x".into()), DataType::Text)],
    );
    let stmts = bind_sql_statements("SELECT public.my_func.col FROM t", &ctx).unwrap();
    let sql = stmts[0].to_string();
    // 3-part identifiers have len() == 3, so our len() == 2 guard skips them
    assert!(
        sql.contains("public.my_func.col"),
        "3-part identifier should be untouched, got: {sql}"
    );
}
