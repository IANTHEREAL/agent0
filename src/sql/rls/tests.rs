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
        table_schemas: &table_schemas,
        policies_by_table: &policies,
        qctx: &qctx,
        command: RlsCommand::Select,
        expr_cache: None,
    };

    let query = simple_select_query("public.users");
    let result = inject_rls_predicates(query, &ctx).unwrap();

    // No policies → WHERE false (deny all).
    match &result.body {
        AnalyzedQueryBody::Select(select) => {
            let where_clause = select.where_clause.as_ref().expect("expected WHERE false");
            match &where_clause.kind {
                TypedExprKind::Constant(Value::Boolean(false)) => {}
                other => panic!("expected WHERE false, got {:?}", other),
            }
        }
        _ => panic!("expected Select body"),
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
        table_schemas: &table_schemas,
        policies_by_table: &policies,
        qctx: &qctx,
        command: RlsCommand::Select,
        expr_cache: None,
    };

    let query = simple_select_query("public.users");
    let result = inject_rls_predicates(query, &ctx).unwrap();

    // With a permissive policy → WHERE clause should be injected (not None, not false).
    match &result.body {
        AnalyzedQueryBody::Select(select) => {
            let where_clause = select
                .where_clause
                .as_ref()
                .expect("expected WHERE clause from RLS policy");
            // The compiled expression for "true" should be a boolean constant.
            match &where_clause.kind {
                TypedExprKind::Constant(Value::Boolean(true)) => {}
                other => panic!(
                    "expected WHERE true from permissive policy, got {:?}",
                    other
                ),
            }
        }
        _ => panic!("expected Select body"),
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
        table_schemas: &table_schemas,
        policies_by_table: &policies,
        qctx: &qctx,
        command: RlsCommand::Select,
        expr_cache: None,
    };

    let query = simple_select_query("public.users");
    let result = inject_rls_predicates(query, &ctx).unwrap();

    // Owner + FORCE + no policies → WHERE false.
    match &result.body {
        AnalyzedQueryBody::Select(select) => {
            let where_clause = select.where_clause.as_ref().expect("expected WHERE false");
            match &where_clause.kind {
                TypedExprKind::Constant(Value::Boolean(false)) => {}
                other => panic!(
                    "expected WHERE false for FORCE RLS with no policies, got {:?}",
                    other
                ),
            }
        }
        _ => panic!("expected Select body"),
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
