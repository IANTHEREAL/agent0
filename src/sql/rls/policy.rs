//! RLS policy compilation, combination, and bypass logic.
//!
//! Data model types (`RlsPolicy`, `RlsCommand`) live in `crate::model`.
//! This module provides the query-time logic: expression compilation,
//! policy combination (PG semantics), and bypass checks.

use crate::model::{DataType, RlsCommand, RlsPolicy, TableSchema};
use crate::sql::analyzer::types::{BinaryOp, TypedExpr, TypedExprKind};
use crate::sql::expr::compile::{analyze_row_expr_for_table, compile_row_expr_for_table};
use crate::sql::expr::typed_fold::fold_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::sql::rls::cache::RlsPolicyCache;
use anyhow::{anyhow, Result};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

// ── Compiled policy ────────────────────────────────────────

/// A policy with its USING/WITH CHECK expressions compiled to `TypedExpr`.
#[derive(Debug, Clone)]
pub struct CompiledRlsPolicy {
    pub permissive: bool,
    pub using_expr: Option<TypedExpr>,
    #[allow(dead_code)] // Used by DML enforcement (PR #1814)
    pub with_check_expr: Option<TypedExpr>,
}

// ── Matching helpers ───────────────────────────────────────

/// Check if a policy's command scope matches the given target command.
pub(super) fn command_matches(policy_cmd: &RlsCommand, target: &RlsCommand) -> bool {
    match target {
        RlsCommand::All => true,
        RlsCommand::Select => policy_cmd.applies_to_select(),
        RlsCommand::Insert => policy_cmd.applies_to_insert(),
        RlsCommand::Update => policy_cmd.applies_to_update(),
        RlsCommand::Delete => policy_cmd.applies_to_delete(),
    }
}

/// Check if a policy applies to the given role.
/// Empty roles list or `["public"]` means applies to all.
pub(super) fn policy_applies_to_role(policy: &RlsPolicy, role: &str) -> bool {
    policy.roles.is_empty()
        || policy
            .roles
            .iter()
            .any(|r| r == role || r.eq_ignore_ascii_case("public"))
}

// ── Expression compilation ─────────────────────────────────

/// Compile a raw SQL policy expression string into a `TypedExpr`.
///
/// Uses the same pattern as CHECK constraint compilation:
/// parse SQL → analyze against table's column scope → TypedExpr.
pub fn compile_rls_policy_expr(
    expr_sql: &str,
    schema: &TableSchema,
    qctx: &QueryContext,
) -> Result<TypedExpr> {
    let dialect = PostgreSqlDialect {};
    let expr = Parser::new(&dialect)
        .try_with_sql(expr_sql)
        .and_then(|mut p| p.parse_expr())
        .map_err(|e| anyhow!("Invalid RLS policy expression '{}': {}", expr_sql, e))?;
    let table_alias = schema.name.rsplit('.').next().unwrap_or(&schema.name);
    compile_row_expr_for_table(&expr, schema, table_alias, qctx)
}

/// Parse and analyze a policy expression without folding.
///
/// Returns a pre-fold `TypedExpr` that can be cached across queries.
/// The caller must apply `fold_typed_expr()` before evaluation.
fn analyze_rls_policy_expr(expr_sql: &str, schema: &TableSchema) -> Result<TypedExpr> {
    let dialect = PostgreSqlDialect {};
    let expr = Parser::new(&dialect)
        .try_with_sql(expr_sql)
        .and_then(|mut p| p.parse_expr())
        .map_err(|e| anyhow!("Invalid RLS policy expression '{}': {}", expr_sql, e))?;
    let table_alias = schema.name.rsplit('.').next().unwrap_or(&schema.name);
    analyze_row_expr_for_table(&expr, schema, table_alias)
}

/// Compile a policy expression with caching.
///
/// Checks the cache for a pre-fold `TypedExpr`; on miss, parses + analyzes
/// the SQL expression and caches the result. Then applies `fold_typed_expr`
/// with the current `QueryContext` (cheap tree walk).
pub fn compile_rls_policy_expr_cached(
    expr_sql: &str,
    schema: &TableSchema,
    qctx: &QueryContext,
    cache: &RlsPolicyCache,
    db_id: u64,
    policy_oid: u32,
    schema_version: u64,
    is_using: bool,
) -> Result<TypedExpr> {
    // Check cache for pre-fold analyzed expr.
    let cached = if is_using {
        cache.get_using_expr(db_id, policy_oid, schema_version)
    } else {
        cache.get_with_check_expr(db_id, policy_oid, schema_version)
    };

    let analyzed = match cached {
        Some(expr) => expr,
        None => {
            let expr = analyze_rls_policy_expr(expr_sql, schema)?;
            if is_using {
                cache.put_using_expr(db_id, policy_oid, schema_version, expr.clone());
            } else {
                cache.put_with_check_expr(db_id, policy_oid, schema_version, expr.clone());
            }
            expr
        }
    };

    // Fold with per-query context (cheap: resolves current_user, now(), etc.)
    Ok(fold_typed_expr(&analyzed, qctx))
}

/// Compile all applicable policies for a table + command + role.
///
/// Filters policies by command and role, then compiles their USING expressions.
/// Returns only policies with a USING clause (INSERT policies have no USING).
pub fn compile_applicable_using_policies(
    policies: &[RlsPolicy],
    schema: &TableSchema,
    command: &RlsCommand,
    role: &str,
    qctx: &QueryContext,
    cache: Option<(&RlsPolicyCache, u64, u64)>,
) -> Result<Vec<CompiledRlsPolicy>> {
    let mut compiled = Vec::new();
    for p in policies {
        if !command_matches(&p.command, command) || !policy_applies_to_role(p, role) {
            continue;
        }
        let using = match &p.using_expr {
            Some(sql) => {
                if let Some((cache, db_id, schema_version)) = cache {
                    Some(compile_rls_policy_expr_cached(
                        sql,
                        schema,
                        qctx,
                        cache,
                        db_id,
                        p.oid,
                        schema_version,
                        true,
                    )?)
                } else {
                    Some(compile_rls_policy_expr(sql, schema, qctx)?)
                }
            }
            None => None,
        };
        // Skip policies with no USING for SELECT/UPDATE/DELETE injection
        if using.is_none() {
            continue;
        }
        compiled.push(CompiledRlsPolicy {
            permissive: p.permissive,
            using_expr: using,
            with_check_expr: None, // WITH CHECK compiled separately for DML
        });
    }
    Ok(compiled)
}

// ── Policy combination (PG semantics) ──────────────────────

/// Combine compiled RLS policy predicates following PostgreSQL semantics:
///
/// 1. All PERMISSIVE policies are OR'd together
/// 2. All RESTRICTIVE policies are AND'd together
/// 3. Final predicate = (permissive_combined) AND (restrictive_combined)
///
/// If no PERMISSIVE policies exist but RLS is enabled → return `false` (deny all).
/// If no RESTRICTIVE policies exist → only permissive applies.
pub fn combine_rls_predicates(policies: &[CompiledRlsPolicy]) -> Option<TypedExpr> {
    let mut permissive_exprs: Vec<TypedExpr> = Vec::new();
    let mut restrictive_exprs: Vec<TypedExpr> = Vec::new();

    for p in policies {
        if let Some(ref using) = p.using_expr {
            if p.permissive {
                permissive_exprs.push(using.clone());
            } else {
                restrictive_exprs.push(using.clone());
            }
        }
    }

    // No permissive policies → deny all (WHERE false)
    if permissive_exprs.is_empty() {
        return Some(TypedExpr::new(
            TypedExprKind::Constant(crate::model::Value::Boolean(false)),
            DataType::Boolean,
        ));
    }

    // OR all permissive policies
    let permissive_combined = fold_or(permissive_exprs);

    // AND all restrictive policies
    if restrictive_exprs.is_empty() {
        return permissive_combined;
    }
    let restrictive_combined = fold_and(restrictive_exprs);

    match (permissive_combined, restrictive_combined) {
        (Some(p), Some(r)) => Some(and_expr(p, r)),
        (Some(p), None) => Some(p),
        (None, Some(r)) => Some(r),
        (None, None) => None,
    }
}

// ── Bypass logic ───────────────────────────────────────────

/// Determine if a role should bypass RLS for the given table.
///
/// PG rules:
/// - Superuser always bypasses
/// - Role with BYPASSRLS attribute bypasses
/// - Table owner bypasses unless FORCE ROW LEVEL SECURITY is set
/// - If RLS is not enabled, bypass (no filtering)
pub fn should_bypass_rls(
    is_superuser: bool,
    bypass_rls: bool,
    current_role: &str,
    table_owner: &str,
    rls_enabled: bool,
    rls_force: bool,
) -> bool {
    if !rls_enabled {
        return true;
    }
    if is_superuser {
        return true;
    }
    if bypass_rls {
        return true;
    }
    if current_role == table_owner && !rls_force {
        return true;
    }
    false
}

// ── Helpers ────────────────────────────────────────────────

/// Create `left AND right`.
fn and_expr(left: TypedExpr, right: TypedExpr) -> TypedExpr {
    TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(left),
            op: BinaryOp::And,
            right: Box::new(right),
        },
        DataType::Boolean,
    )
}

/// Create `left OR right`.
fn or_expr(left: TypedExpr, right: TypedExpr) -> TypedExpr {
    TypedExpr::new(
        TypedExprKind::BinaryOp {
            left: Box::new(left),
            op: BinaryOp::Or,
            right: Box::new(right),
        },
        DataType::Boolean,
    )
}

/// Fold a list of expressions with OR. Returns None if empty.
fn fold_or(exprs: Vec<TypedExpr>) -> Option<TypedExpr> {
    exprs.into_iter().reduce(or_expr)
}

/// Fold a list of expressions with AND. Returns None if empty.
fn fold_and(exprs: Vec<TypedExpr>) -> Option<TypedExpr> {
    exprs.into_iter().reduce(and_expr)
}

// ── Tests ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;

    #[test]
    fn rls_command_matches() {
        assert!(command_matches(&RlsCommand::All, &RlsCommand::Select));
        assert!(command_matches(&RlsCommand::All, &RlsCommand::Insert));
        assert!(command_matches(&RlsCommand::Select, &RlsCommand::Select));
        assert!(!command_matches(&RlsCommand::Select, &RlsCommand::Insert));
        assert!(!command_matches(&RlsCommand::Insert, &RlsCommand::Delete));
    }

    #[test]
    fn test_policy_role_matching() {
        let public_policy = RlsPolicy {
            oid: 1,
            name: String::new(),
            table_id: 1,
            command: RlsCommand::Select,
            permissive: true,
            roles: vec![],
            using_expr: Some("true".into()),
            with_check_expr: None,
        };
        assert!(super::policy_applies_to_role(&public_policy, "alice"));
        assert!(super::policy_applies_to_role(&public_policy, "bob"));

        let alice_policy = RlsPolicy {
            roles: vec!["alice".into()],
            ..public_policy.clone()
        };
        assert!(super::policy_applies_to_role(&alice_policy, "alice"));
        assert!(!super::policy_applies_to_role(&alice_policy, "bob"));
    }

    #[test]
    fn bypass_rls_superuser() {
        assert!(should_bypass_rls(true, false, "alice", "bob", true, true));
    }

    #[test]
    fn bypass_rls_owner_no_force() {
        assert!(should_bypass_rls(
            false, false, "alice", "alice", true, false
        ));
    }

    #[test]
    fn no_bypass_rls_owner_force() {
        assert!(!should_bypass_rls(
            false, false, "alice", "alice", true, true
        ));
    }

    #[test]
    fn no_bypass_rls_regular_user() {
        assert!(!should_bypass_rls(
            false, false, "bob", "alice", true, false
        ));
    }

    #[test]
    fn bypass_rls_disabled() {
        assert!(should_bypass_rls(
            false, false, "bob", "alice", false, false
        ));
    }

    #[test]
    fn bypass_rls_attribute() {
        // Non-superuser with BYPASSRLS attribute should bypass even with FORCE
        assert!(should_bypass_rls(false, true, "bob", "alice", true, true));
    }

    #[test]
    fn no_bypass_rls_attribute_disabled() {
        // NOBYPASSRLS regular user should not bypass
        assert!(!should_bypass_rls(
            false, false, "bob", "alice", true, false
        ));
    }

    #[test]
    fn combine_no_permissive_returns_false() {
        // RLS enabled but no permissive policies → deny all
        let policies = vec![CompiledRlsPolicy {
            permissive: false,
            using_expr: Some(TypedExpr::new(
                TypedExprKind::Constant(Value::Boolean(true)),
                DataType::Boolean,
            )),
            with_check_expr: None,
        }];
        let combined = combine_rls_predicates(&policies).unwrap();
        // Should be `false` because no permissive policies
        match &combined.kind {
            TypedExprKind::Constant(Value::Boolean(false)) => {}
            other => panic!("expected false constant, got {:?}", other),
        }
    }

    #[test]
    fn combine_single_permissive() {
        let expr = TypedExpr::new(
            TypedExprKind::Constant(Value::Boolean(true)),
            DataType::Boolean,
        );
        let policies = vec![CompiledRlsPolicy {
            permissive: true,
            using_expr: Some(expr.clone()),
            with_check_expr: None,
        }];
        let combined = combine_rls_predicates(&policies).unwrap();
        match &combined.kind {
            TypedExprKind::Constant(Value::Boolean(true)) => {}
            other => panic!("expected true constant, got {:?}", other),
        }
    }

    #[test]
    fn combine_permissive_or() {
        let t = TypedExpr::new(
            TypedExprKind::Constant(Value::Boolean(true)),
            DataType::Boolean,
        );
        let f = TypedExpr::new(
            TypedExprKind::Constant(Value::Boolean(false)),
            DataType::Boolean,
        );
        let policies = vec![
            CompiledRlsPolicy {
                permissive: true,
                using_expr: Some(t),
                with_check_expr: None,
            },
            CompiledRlsPolicy {
                permissive: true,
                using_expr: Some(f),
                with_check_expr: None,
            },
        ];
        let combined = combine_rls_predicates(&policies).unwrap();
        // Should be OR(true, false)
        match &combined.kind {
            TypedExprKind::BinaryOp { op, .. } => assert_eq!(*op, BinaryOp::Or),
            other => panic!("expected BinaryOp::Or, got {:?}", other),
        }
    }

    #[test]
    fn cached_expr_folds_differently_per_query_context() {
        use crate::model::ColumnDef;
        use crate::sql::rls::cache::RlsPolicyCache;
        use std::sync::Arc;

        // Schema with a text column for the policy expression to reference.
        let schema = {
            let mut s = TableSchema::new(
                "public.posts".to_string(),
                1,
                vec![ColumnDef::new("owner", DataType::Text, false)],
                vec![0],
            );
            s.pk_constraint_name = None;
            s.owner = "admin".to_string();
            s.rls_enabled = true;
            s
        };

        let cache = RlsPolicyCache::new();

        // Policy: owner = current_user
        // current_user is resolved during fold, not during analysis.
        let expr_sql = "owner = current_user";

        let qctx_alice = QueryContext::new(
            1,
            Arc::from("testdb"),
            Arc::from("alice"),
            1_700_000_000_000,
            1_700_000_000_000,
            Arc::from("UTC"),
        );
        let qctx_bob = QueryContext::new(
            1,
            Arc::from("testdb"),
            Arc::from("bob"),
            1_700_000_000_000,
            1_700_000_000_000,
            Arc::from("UTC"),
        );

        // First call: cache miss → analyze + cache + fold for alice.
        let result_alice =
            compile_rls_policy_expr_cached(expr_sql, &schema, &qctx_alice, &cache, 1, 42, 1, true)
                .unwrap();

        // Second call: cache hit → fold for bob (different current_user).
        let result_bob =
            compile_rls_policy_expr_cached(expr_sql, &schema, &qctx_bob, &cache, 1, 42, 1, true)
                .unwrap();

        // Both should be BinaryOp(Eq), but the RHS constant should differ.
        // After folding, current_user resolves to a Constant("alice") or Constant("bob").
        assert_ne!(
            format!("{:?}", result_alice),
            format!("{:?}", result_bob),
            "same cached analyzed expr must fold differently for different current_user"
        );

        // Verify the cache was actually hit (only one entry for this key).
        assert!(cache.get_using_expr(1, 42, 1).is_some());
    }

    #[test]
    fn combine_permissive_and_restrictive() {
        let t = TypedExpr::new(
            TypedExprKind::Constant(Value::Boolean(true)),
            DataType::Boolean,
        );
        let policies = vec![
            CompiledRlsPolicy {
                permissive: true,
                using_expr: Some(t.clone()),
                with_check_expr: None,
            },
            CompiledRlsPolicy {
                permissive: false,
                using_expr: Some(t),
                with_check_expr: None,
            },
        ];
        let combined = combine_rls_predicates(&policies).unwrap();
        // Should be AND(permissive, restrictive)
        match &combined.kind {
            TypedExprKind::BinaryOp { op, .. } => assert_eq!(*op, BinaryOp::And),
            other => panic!("expected BinaryOp::And, got {:?}", other),
        }
    }
}
