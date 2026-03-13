//! Integration tests for RLS predicate injection into AnalyzedQuery.

use super::inject::{inject_rls_predicates, RlsContext};
use crate::model::{ColumnDef, DataType, RlsCommand, RlsPolicy, TableSchema, Value};
use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedQuery, AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef,
    AnalyzedTableRefKind, TableRefSchema, TypedExprKind,
};
use crate::sql::query_context::QueryContext;
use std::collections::HashMap;
use std::sync::Arc;

/// Helper: build a minimal TableSchema with RLS flags.
fn test_schema(
    name: &str,
    table_id: u64,
    owner: &str,
    rls_enabled: bool,
    rls_force: bool,
) -> TableSchema {
    let mut schema = TableSchema::new(
        name.to_string(),
        table_id,
        vec![ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int32,
            nullable: false,
            primary_key: true,
            unique: false,
            is_serial: false,
            default_expr: None,
            generation_expr: None,
            generation_expr_authorized_by: None,
            collation: None,
        }],
        vec![0],
    );
    schema.owner = owner.to_string();
    schema.rls_enabled = rls_enabled;
    schema.rls_force = rls_force;
    schema
}

/// Helper: build a minimal AnalyzedQuery with a single table in FROM.
fn simple_select_query(table_name: &str) -> AnalyzedQuery {
    AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection: vec![],
            from: vec![AnalyzedTableRef {
                kind: AnalyzedTableRefKind::Table {
                    name: table_name.to_string(),
                    schema: TableRefSchema {
                        table_id: 1,
                        columns: vec![("id".to_string(), DataType::Int32, false)],
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
        output_schema: vec![],
    }
}

/// Helper: extract the WHERE clause from a security barrier subquery.
///
/// Expects the query to be `SELECT ... FROM (subquery) ...` where the subquery
/// contains the RLS predicate. Panics if the structure doesn't match.
fn extract_barrier_where(query: &AnalyzedQuery) -> &crate::sql::analyzer::types::TypedExpr {
    match &query.body {
        AnalyzedQueryBody::Select(select) => {
            // Outer WHERE should be None (user predicates stay outside).
            assert!(
                select.where_clause.is_none(),
                "outer WHERE should be None; RLS predicate should be inside barrier subquery"
            );
            assert_eq!(select.from.len(), 1, "expected single FROM entry");
            match &select.from[0].kind {
                AnalyzedTableRefKind::Subquery(subquery) => match &subquery.body {
                    AnalyzedQueryBody::Select(inner_select) => inner_select
                        .where_clause
                        .as_ref()
                        .expect("barrier subquery should have WHERE clause"),
                    other => panic!("expected Select inside barrier, got {:?}", other),
                },
                other => panic!(
                    "expected Subquery (security barrier) in FROM, got {:?}",
                    other
                ),
            }
        }
        _ => panic!("expected Select body"),
    }
}

/// Helper: build a test QueryContext.
fn test_qctx(role: &str) -> QueryContext {
    QueryContext::new(
        0,
        Arc::from("testdb"),
        Arc::from(role),
        1_700_000_000_000,
        1_700_000_000_000,
        Arc::from("UTC"),
    )
}

#[test]
fn inject_no_rls_enabled_is_noop() {
    let schema = test_schema("public.users", 1, "admin", false, false);
    let mut table_schemas: HashMap<String, &TableSchema> = HashMap::new();
    table_schemas.insert("public.users".to_string(), &schema);
    let policies: HashMap<u64, Vec<RlsPolicy>> = HashMap::new();
    let qctx = test_qctx("alice");

    let ctx = RlsContext {
        current_role: "alice",
        is_superuser: false,
        bypass_rls: false,
        table_schemas: &table_schemas,
        policies_by_table: &policies,
        qctx: &qctx,
        command: RlsCommand::Select,
        expr_cache: None,
    };

    let query = simple_select_query("public.users");
    let result = inject_rls_predicates(query, &ctx).unwrap();

    // No RLS enabled → WHERE clause should remain None.
    match &result.body {
        AnalyzedQueryBody::Select(select) => {
            assert!(select.where_clause.is_none(), "expected no WHERE clause");
        }
        _ => panic!("expected Select body"),
    }
}

#[test]
fn inject_superuser_bypass() {
    let schema = test_schema("public.users", 1, "admin", true, true);
    let mut table_schemas: HashMap<String, &TableSchema> = HashMap::new();
    table_schemas.insert("public.users".to_string(), &schema);
    let policies: HashMap<u64, Vec<RlsPolicy>> = HashMap::new();
    let qctx = test_qctx("superadmin");

    let ctx = RlsContext {
        current_role: "superadmin",
        is_superuser: true,
        bypass_rls: false,
        table_schemas: &table_schemas,
        policies_by_table: &policies,
        qctx: &qctx,
        command: RlsCommand::Select,
        expr_cache: None,
    };

    let query = simple_select_query("public.users");
    let result = inject_rls_predicates(query, &ctx).unwrap();

    // Superuser bypasses RLS → WHERE clause should remain None.
    match &result.body {
        AnalyzedQueryBody::Select(select) => {
            assert!(select.where_clause.is_none(), "superuser should bypass RLS");
        }
        _ => panic!("expected Select body"),
    }
}

#[test]
fn inject_owner_bypass_no_force() {
    let schema = test_schema("public.users", 1, "alice", true, false);
    let mut table_schemas: HashMap<String, &TableSchema> = HashMap::new();
    table_schemas.insert("public.users".to_string(), &schema);
    let policies: HashMap<u64, Vec<RlsPolicy>> = HashMap::new();
    let qctx = test_qctx("alice");

    let ctx = RlsContext {
        current_role: "alice",
        is_superuser: false,
        bypass_rls: false,
        table_schemas: &table_schemas,
        policies_by_table: &policies,
        qctx: &qctx,
        command: RlsCommand::Select,
        expr_cache: None,
    };

    let query = simple_select_query("public.users");
    let result = inject_rls_predicates(query, &ctx).unwrap();

    // Owner without FORCE → bypass.
    match &result.body {
        AnalyzedQueryBody::Select(select) => {
            assert!(
                select.where_clause.is_none(),
                "owner without FORCE should bypass"
            );
        }
        _ => panic!("expected Select body"),
    }
}

#[test]
fn inject_rls_no_policies_denies_all() {
    let schema = test_schema("public.users", 1, "admin", true, false);
    let mut table_schemas: HashMap<String, &TableSchema> = HashMap::new();
    table_schemas.insert("public.users".to_string(), &schema);
    // RLS enabled but no policies in the map → default-deny.
    let policies: HashMap<u64, Vec<RlsPolicy>> = HashMap::new();
    let qctx = test_qctx("bob");

    let ctx = RlsContext {
        current_role: "bob",
        is_superuser: false,
        bypass_rls: false,
        table_schemas: &table_schemas,
        policies_by_table: &policies,
        qctx: &qctx,
        command: RlsCommand::Select,
        expr_cache: None,
    };

    let query = simple_select_query("public.users");
    let result = inject_rls_predicates(query, &ctx).unwrap();

    // No policies → table wrapped in security barrier subquery with WHERE false.
    // Outer WHERE remains None; RLS predicate lives inside the subquery.
    let inner_where = extract_barrier_where(&result);
    match &inner_where.kind {
        TypedExprKind::Constant(Value::Boolean(false)) => {}
        other => panic!("expected WHERE false inside barrier, got {:?}", other),
    }
}

#[test]
fn inject_rls_with_permissive_policy() {
    let schema = test_schema("public.users", 1, "admin", true, false);
    let mut table_schemas: HashMap<String, &TableSchema> = HashMap::new();
    table_schemas.insert("public.users".to_string(), &schema);

    let policies = HashMap::from([(
        1u64,
        vec![RlsPolicy {
            oid: 100,
            name: "user_select".to_string(),
            table_id: 1,
            command: RlsCommand::Select,
            permissive: true,
            roles: vec![], // PUBLIC
            using_expr: Some("true".to_string()),
            with_check_expr: None,
        }],
    )]);
    let qctx = test_qctx("bob");

    let ctx = RlsContext {
        current_role: "bob",
        is_superuser: false,
        bypass_rls: false,
        table_schemas: &table_schemas,
        policies_by_table: &policies,
        qctx: &qctx,
        command: RlsCommand::Select,
        expr_cache: None,
    };

    let query = simple_select_query("public.users");
    let result = inject_rls_predicates(query, &ctx).unwrap();

    // With a permissive policy → table wrapped in security barrier subquery with RLS predicate.
    // Outer WHERE remains None; policy predicate is inside the barrier subquery.
    let inner_where = extract_barrier_where(&result);
    // The compiled expression for "true" should be a boolean constant.
    match &inner_where.kind {
        TypedExprKind::Constant(Value::Boolean(true)) => {}
        other => panic!(
            "expected WHERE true from permissive policy inside barrier, got {:?}",
            other
        ),
    }
}

#[test]
fn inject_rls_owner_with_force() {
    // Owner + FORCE RLS → should NOT bypass, should apply policies.
    let schema = test_schema("public.users", 1, "alice", true, true);
    let mut table_schemas: HashMap<String, &TableSchema> = HashMap::new();
    table_schemas.insert("public.users".to_string(), &schema);
    // No policies → default deny.
    let policies: HashMap<u64, Vec<RlsPolicy>> = HashMap::new();
    let qctx = test_qctx("alice");

    let ctx = RlsContext {
        current_role: "alice",
        is_superuser: false,
        bypass_rls: false,
        table_schemas: &table_schemas,
        policies_by_table: &policies,
        qctx: &qctx,
        command: RlsCommand::Select,
        expr_cache: None,
    };

    let query = simple_select_query("public.users");
    let result = inject_rls_predicates(query, &ctx).unwrap();

    // Owner + FORCE + no policies → security barrier with WHERE false.
    let inner_where = extract_barrier_where(&result);
    match &inner_where.kind {
        TypedExprKind::Constant(Value::Boolean(false)) => {}
        other => panic!(
            "expected WHERE false inside barrier for FORCE RLS with no policies, got {:?}",
            other
        ),
    }
}

#[test]
fn inject_rls_unmatched_table_no_injection() {
    // Table in FROM doesn't match any schema in table_schemas → no injection.
    let schema = test_schema("public.orders", 2, "admin", true, false);
    let mut table_schemas: HashMap<String, &TableSchema> = HashMap::new();
    table_schemas.insert("public.orders".to_string(), &schema);
    let policies: HashMap<u64, Vec<RlsPolicy>> = HashMap::new();
    let qctx = test_qctx("bob");

    let ctx = RlsContext {
        current_role: "bob",
        is_superuser: false,
        bypass_rls: false,
        table_schemas: &table_schemas,
        policies_by_table: &policies,
        qctx: &qctx,
        command: RlsCommand::Select,
        expr_cache: None,
    };

    // Query references "public.users" but schemas only have "public.orders".
    let query = simple_select_query("public.users");
    let result = inject_rls_predicates(query, &ctx).unwrap();

    match &result.body {
        AnalyzedQueryBody::Select(select) => {
            assert!(
                select.where_clause.is_none(),
                "unmatched table should not be injected"
            );
        }
        _ => panic!("expected Select body"),
    }
}

#[test]
fn inject_rls_security_barrier_preserves_user_where() {
    // Verifies security barrier semantics: user WHERE stays outside the barrier subquery,
    // RLS predicate lives inside it.
    use crate::sql::analyzer::types::{BinaryOp, TypedExpr, TypedExprKind};

    let schema = test_schema("public.users", 1, "admin", true, false);
    let mut table_schemas: HashMap<String, &TableSchema> = HashMap::new();
    table_schemas.insert("public.users".to_string(), &schema);

    let policies = HashMap::from([(
        1u64,
        vec![RlsPolicy {
            oid: 100,
            name: "user_select".to_string(),
            table_id: 1,
            command: RlsCommand::Select,
            permissive: true,
            roles: vec![],
            using_expr: Some("true".to_string()),
            with_check_expr: None,
        }],
    )]);
    let qctx = test_qctx("bob");

    let ctx = RlsContext {
        current_role: "bob",
        is_superuser: false,
        bypass_rls: false,
        table_schemas: &table_schemas,
        policies_by_table: &policies,
        qctx: &qctx,
        command: RlsCommand::Select,
        expr_cache: None,
    };

    // Build a query WITH a user-supplied WHERE clause.
    let user_pred = TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(TypedExpr::new(
                TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: 0,
                    column_name: "id".to_string(),
                },
                DataType::Int32,
            )),
            op: BinaryOp::Gt,
            right: Box::new(TypedExpr::new(
                TypedExprKind::Constant(Value::Int32(5)),
                DataType::Int32,
            )),
        },
        DataType::Boolean,
    );
    let mut query = simple_select_query("public.users");
    match &mut query.body {
        AnalyzedQueryBody::Select(select) => {
            select.where_clause = Some(user_pred);
        }
        _ => unreachable!(),
    }

    let result = inject_rls_predicates(query, &ctx).unwrap();

    // Verify: user WHERE stays in outer SELECT, RLS goes inside barrier.
    match &result.body {
        AnalyzedQueryBody::Select(select) => {
            // User predicate should remain in outer WHERE.
            let user_where = select
                .where_clause
                .as_ref()
                .expect("user WHERE should remain in outer SELECT");
            match &user_where.kind {
                TypedExprKind::BinaryOp { op, .. } => {
                    assert_eq!(*op, BinaryOp::Gt, "user predicate should be id > 5");
                }
                other => panic!("expected user BinaryOp predicate, got {:?}", other),
            }

            // FROM should be a security barrier subquery.
            assert_eq!(select.from.len(), 1);
            match &select.from[0].kind {
                AnalyzedTableRefKind::Subquery(subquery) => {
                    match &subquery.body {
                        AnalyzedQueryBody::Select(inner_select) => {
                            let rls_pred = inner_select
                                .where_clause
                                .as_ref()
                                .expect("barrier should have RLS WHERE");
                            match &rls_pred.kind {
                                TypedExprKind::Constant(Value::Boolean(true)) => {}
                                other => panic!(
                                    "expected RLS WHERE true inside barrier, got {:?}",
                                    other
                                ),
                            }
                        }
                        other => panic!("expected inner Select, got {:?}", other),
                    }
                }
                other => panic!("expected Subquery in FROM, got {:?}", other),
            }
        }
        _ => panic!("expected Select body"),
    }
}
