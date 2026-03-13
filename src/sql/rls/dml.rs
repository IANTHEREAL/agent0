//! DML-specific RLS enforcement helpers.
//!
//! Provides `RlsDmlContext` — a pre-compiled bundle of RLS policies for a single
//! DML statement. Created once per statement in `stmt_dml.rs`, then threaded into
//! `execute_analyzed_insert/update/delete` and COPY paths.

use super::policy::{
    compile_rls_policy_expr, should_bypass_rls, CompiledRlsPolicy,
};
use crate::model::{RlsCommand, RlsPolicy, Row, TableSchema, Value};
use crate::sql::analyzer::types::TypedExpr;
use crate::sql::error::SqlError;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::query_context::QueryContext;
use anyhow::{anyhow, Result};

// ── Policy filtering ──────────────────────────────────────

/// Filter policies applicable to a given command and role.
/// Unlike `policy::compile_applicable_using_policies`, this returns all
/// matching policies (including those without USING), since DML needs
/// both USING and WITH CHECK expressions.
fn filter_applicable_policies<'a>(
    policies: &'a [RlsPolicy],
    command: RlsCommand,
    current_role: &str,
) -> Vec<&'a RlsPolicy> {
    use super::policy::{command_matches, policy_applies_to_role};
    policies
        .iter()
        .filter(|p| command_matches(&p.command, &command) && policy_applies_to_role(p, current_role))
        .collect()
}

// ── Expression compilation for DML (per-row eval) ─────────

/// Compile applicable RLS policies into typed expressions for per-row evaluation.
/// Compiles both USING and WITH CHECK expressions (unlike the SELECT path which
/// only needs USING for WHERE injection).
fn compile_dml_policies(
    schema: &TableSchema,
    policies: &[RlsPolicy],
    qctx: &QueryContext,
) -> Result<Vec<CompiledRlsPolicy>> {
    let mut compiled = Vec::with_capacity(policies.len());
    for policy in policies {
        let using = match &policy.using_expr {
            Some(sql) => Some(compile_rls_policy_expr(sql, schema, qctx)?),
            None => None,
        };
        let with_check = match &policy.with_check_expr {
            Some(sql) => Some(compile_rls_policy_expr(sql, schema, qctx)?),
            None => None,
        };
        compiled.push(CompiledRlsPolicy {
            name: policy.name.clone(),
            permissive: policy.permissive,
            using_expr: using,
            with_check_expr: with_check,
        });
    }
    Ok(compiled)
}

// ── Per-row evaluation ────────────────────────────────────

/// Evaluate a single typed expression against a row, returning boolean.
/// NULL → false (PG semantics).
fn eval_rls_expr(expr: &TypedExpr, row: &Row, qctx: &QueryContext) -> Result<bool> {
    let result = eval_typed_expr(expr, row, qctx)?;
    match result {
        Value::Boolean(b) => Ok(b),
        Value::Null => Ok(false),
        other => Err(anyhow!("RLS policy expression must evaluate to boolean, got {:?}", other)),
    }
}

/// Get the relevant expression from a compiled policy.
fn policy_expr(p: &CompiledRlsPolicy, use_with_check: bool) -> Option<&TypedExpr> {
    if use_with_check {
        p.with_check_expr.as_ref().or(p.using_expr.as_ref())
    } else {
        p.using_expr.as_ref()
    }
}

/// Combine compiled policies per PG semantics (OR permissive, AND restrictive).
fn eval_combined_policies(
    policies: &[CompiledRlsPolicy],
    row: &Row,
    qctx: &QueryContext,
    use_with_check: bool,
) -> Result<bool> {
    let permissive: Vec<_> = policies.iter().filter(|p| p.permissive).collect();
    let restrictive: Vec<_> = policies.iter().filter(|p| !p.permissive).collect();

    if permissive.is_empty() {
        return Ok(false);
    }

    let mut any_permissive_pass = false;
    for p in &permissive {
        if let Some(expr) = policy_expr(p, use_with_check) {
            if eval_rls_expr(expr, row, qctx)? {
                any_permissive_pass = true;
                break;
            }
        } else {
            any_permissive_pass = true;
            break;
        }
    }

    if !any_permissive_pass {
        return Ok(false);
    }

    for p in &restrictive {
        if let Some(expr) = policy_expr(p, use_with_check) {
            if !eval_rls_expr(expr, row, qctx)? {
                return Ok(false);
            }
        }
    }

    Ok(true)
}

/// Validate row visibility through USING policies.
fn validate_rls_using(policies: &[CompiledRlsPolicy], row: &Row, qctx: &QueryContext) -> Result<bool> {
    eval_combined_policies(policies, row, qctx, false)
}

/// Validate row against WITH CHECK policies. Error 42501 on failure.
fn validate_rls_with_check(schema: &TableSchema, policies: &[CompiledRlsPolicy], row: &Row, qctx: &QueryContext) -> Result<()> {
    if !eval_combined_policies(policies, row, qctx, true)? {
        let short_table = schema.name.rsplit('.').next().unwrap_or(&schema.name);
        return Err(SqlError::InsufficientPrivilege {
            message: format!("new row violates row-level security policy for table \"{}\"", short_table),
        }.into());
    }
    Ok(())
}

/// Validate RETURNING rows against SELECT USING policies. Error 42501 on failure.
fn validate_rls_returning(schema: &TableSchema, select_policies: &[CompiledRlsPolicy], rows: &[Row], qctx: &QueryContext) -> Result<()> {
    for row in rows {
        if !validate_rls_using(select_policies, row, qctx)? {
            let short_table = schema.name.rsplit('.').next().unwrap_or(&schema.name);
            return Err(SqlError::InsufficientPrivilege {
                message: format!("new row violates row-level security policy for table \"{}\"", short_table),
            }.into());
        }
    }
    Ok(())
}

// should_enforce_rls is the inverse of policy::should_bypass_rls

/// Pre-compiled RLS context for a DML statement.
///
/// Created once per statement, reused across all rows.
/// The `compile_once_eval_many` pattern ensures no per-row parse/analyze overhead.
#[derive(Debug)]
pub struct RlsDmlContext {
    /// Compiled policies for the primary DML command (INSERT/UPDATE/DELETE).
    /// For INSERT: WITH CHECK policies.
    /// For UPDATE: USING + WITH CHECK policies.
    /// For DELETE: USING policies.
    pub command_policies: Vec<CompiledRlsPolicy>,

    /// Merged SELECT USING + command USING policies for row visibility.
    /// Used by UPDATE/DELETE `is_row_visible()` to enforce PG semantics:
    /// both SELECT/ALL and command-specific policies must allow visibility.
    /// `None` for INSERT (no pre-read visibility check).
    pub visibility_policies: Option<Vec<CompiledRlsPolicy>>,

    /// Compiled SELECT policies for RETURNING clause validation.
    /// Only populated when the statement has a RETURNING clause.
    pub select_policies: Option<Vec<CompiledRlsPolicy>>,

    /// For UPDATE: compiled USING combined predicate to inject into WHERE.
    /// Pre-combined as a single `TypedExpr` for efficient injection.
    /// TODO: Build as TypedExpr for injection into AnalyzedUpdate/Delete.where_clause.
    /// For now, per-row evaluation via `visibility_policies` handles correctness.
    pub using_predicate: Option<TypedExpr>,
}

impl RlsDmlContext {
    /// Build RLS context for an INSERT statement.
    ///
    /// Compiles INSERT WITH CHECK policies and (if RETURNING present) SELECT USING policies.
    pub fn for_insert(
        schema: &TableSchema,
        all_policies: &[RlsPolicy],
        current_role: &str,
        has_returning: bool,
        _has_on_conflict_update: bool,
        qctx: &QueryContext,
    ) -> Result<Self> {
        let insert_applicable: Vec<RlsPolicy> =
            filter_applicable_policies(all_policies, RlsCommand::Insert, current_role)
                .into_iter()
                .cloned()
                .collect();
        let command_policies = compile_dml_policies(schema, &insert_applicable, qctx)?;

        let select_policies = if has_returning {
            let select_applicable: Vec<RlsPolicy> =
                filter_applicable_policies(all_policies, RlsCommand::Select, current_role)
                    .into_iter()
                    .cloned()
                    .collect();
            Some(compile_dml_policies(schema, &select_applicable, qctx)?)
        } else {
            None
        };

        Ok(Self {
            command_policies,
            visibility_policies: None,
            select_policies,
            using_predicate: None,
        })
    }

    /// Build RLS context for an UPDATE statement.
    ///
    /// Compiles UPDATE USING policies (for row visibility / WHERE injection),
    /// UPDATE WITH CHECK policies (for post-update validation),
    /// and (if RETURNING present) SELECT USING policies.
    ///
    /// PG semantics: UPDATE's read path must also satisfy SELECT/ALL USING
    /// policies. The `visibility_policies` field merges both sets for
    /// `is_row_visible()`.
    pub fn for_update(
        schema: &TableSchema,
        all_policies: &[RlsPolicy],
        current_role: &str,
        has_returning: bool,
        qctx: &QueryContext,
    ) -> Result<Self> {
        let update_applicable: Vec<RlsPolicy> =
            filter_applicable_policies(all_policies, RlsCommand::Update, current_role)
                .into_iter()
                .cloned()
                .collect();
        let command_policies = compile_dml_policies(schema, &update_applicable, qctx)?;

        // SELECT/ALL policies also constrain UPDATE's read path (PG semantics).
        let select_applicable: Vec<RlsPolicy> =
            filter_applicable_policies(all_policies, RlsCommand::Select, current_role)
                .into_iter()
                .cloned()
                .collect();
        let select_compiled = compile_dml_policies(schema, &select_applicable, qctx)?;

        // Merge UPDATE USING + SELECT USING into visibility_policies.
        let mut visibility = Vec::with_capacity(command_policies.len() + select_compiled.len());
        visibility.extend(command_policies.iter().cloned());
        visibility.extend(select_compiled.iter().cloned());
        let visibility_policies = Some(visibility);

        // TODO: Build combined USING predicate as TypedExpr for WHERE injection.
        // For now, per-row evaluation via visibility_policies handles correctness.
        let using_predicate = None;

        let select_policies = if has_returning {
            Some(select_compiled)
        } else {
            None
        };

        Ok(Self {
            command_policies,
            visibility_policies,
            select_policies,
            using_predicate,
        })
    }

    /// Build RLS context for a DELETE statement.
    ///
    /// Compiles DELETE USING policies (for row visibility / WHERE injection)
    /// and (if RETURNING present) SELECT USING policies.
    ///
    /// PG semantics: DELETE's read path must also satisfy SELECT/ALL USING
    /// policies. The `visibility_policies` field merges both sets.
    pub fn for_delete(
        schema: &TableSchema,
        all_policies: &[RlsPolicy],
        current_role: &str,
        has_returning: bool,
        qctx: &QueryContext,
    ) -> Result<Self> {
        let delete_applicable: Vec<RlsPolicy> =
            filter_applicable_policies(all_policies, RlsCommand::Delete, current_role)
                .into_iter()
                .cloned()
                .collect();
        let command_policies = compile_dml_policies(schema, &delete_applicable, qctx)?;

        // SELECT/ALL policies also constrain DELETE's read path (PG semantics).
        let select_applicable: Vec<RlsPolicy> =
            filter_applicable_policies(all_policies, RlsCommand::Select, current_role)
                .into_iter()
                .cloned()
                .collect();
        let select_compiled = compile_dml_policies(schema, &select_applicable, qctx)?;

        // Merge DELETE USING + SELECT USING into visibility_policies.
        let mut visibility = Vec::with_capacity(command_policies.len() + select_compiled.len());
        visibility.extend(command_policies.iter().cloned());
        visibility.extend(select_compiled.iter().cloned());
        let visibility_policies = Some(visibility);

        // TODO: Build combined USING predicate as TypedExpr for WHERE injection.
        let using_predicate = None;

        // Reuse select_compiled for RETURNING (no re-compilation).
        let select_policies = if has_returning {
            Some(select_compiled)
        } else {
            None
        };

        Ok(Self {
            command_policies,
            visibility_policies,
            select_policies,
            using_predicate,
        })
    }

    /// Build RLS context for COPY FROM (same as INSERT WITH CHECK).
    pub fn for_copy_from(
        schema: &TableSchema,
        all_policies: &[RlsPolicy],
        current_role: &str,
        qctx: &QueryContext,
    ) -> Result<Self> {
        Self::for_insert(schema, all_policies, current_role, false, false, qctx)
    }

    /// Validate a new/modified row against WITH CHECK policies.
    /// Used by INSERT (new row), UPDATE (post-update row), COPY FROM.
    pub fn check_row(&self, schema: &TableSchema, row: &Row, qctx: &QueryContext) -> Result<()> {
        validate_rls_with_check(schema, &self.command_policies, row, qctx)
    }

    /// Check if a row is visible through USING policies.
    /// Used by UPDATE/DELETE to filter pre-existing rows.
    /// Evaluates merged SELECT USING + command USING policies (PG semantics).
    pub fn is_row_visible(&self, row: &Row, qctx: &QueryContext) -> Result<bool> {
        let policies = self.visibility_policies.as_deref().unwrap_or(&self.command_policies);
        validate_rls_using(policies, row, qctx)
    }

    /// Validate RETURNING clause rows against SELECT policies.
    /// Raises error 42501 if any returned row is not visible.
    pub fn check_returning(
        &self,
        schema: &TableSchema,
        rows: &[Row],
        qctx: &QueryContext,
    ) -> Result<()> {
        if let Some(ref select_policies) = self.select_policies {
            validate_rls_returning(schema, select_policies, rows, qctx)?;
        }
        Ok(())
    }
}

/// Determine whether RLS should be enforced for a DML statement on a given table,
/// and if so, build the compiled RLS context.
///
/// Returns `None` if RLS is not enabled or the user bypasses it.
///
/// `load_policies_fn` is called to fetch policies from TiKV via
/// `store.list_policies_for_table(txn, db_id, table_id)`.
pub async fn maybe_build_rls_context<F, Fut>(
    schema: &TableSchema,
    current_role: Option<&str>,
    is_superuser: bool,
    rls_enabled: bool,
    rls_force: bool,
    command: RlsCommand,
    has_returning: bool,
    has_on_conflict_update: bool,
    qctx: &QueryContext,
    load_policies_fn: F,
) -> Result<Option<RlsDmlContext>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<RlsPolicy>>>,
{
    let role = match current_role {
        Some(r) => r,
        None => return Ok(None), // No role → no RLS
    };

    if should_bypass_rls(is_superuser, role, &schema.owner, rls_enabled, rls_force) {
        return Ok(None);
    }

    let all_policies = load_policies_fn().await?;

    let ctx = match command {
        RlsCommand::Insert => {
            RlsDmlContext::for_insert(schema, &all_policies, role, has_returning, has_on_conflict_update, qctx)?
        }
        RlsCommand::Update => {
            RlsDmlContext::for_update(schema, &all_policies, role, has_returning, qctx)?
        }
        RlsCommand::Delete => {
            RlsDmlContext::for_delete(schema, &all_policies, role, has_returning, qctx)?
        }
        _ => return Ok(None),
    };

    Ok(Some(ctx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDef, DataType, Value};
    use std::sync::Arc;

    fn test_schema() -> TableSchema {
        TableSchema {
            name: "public.posts".to_string(),
            table_id: 1,
            columns: vec![
                ColumnDef {
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
                },
                ColumnDef {
                    name: "user_id".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                },
            ],
            version: 1,
            pk_constraint_name: Some("posts_pkey".to_string()),
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: "admin".to_string(),
            from_alias: None,
            rls_enabled: true,
            rls_force: false,
        }
    }

    fn test_qctx(user: &str) -> QueryContext {
        QueryContext::new(
            1,
            Arc::from("testdb"),
            Arc::from(user),
            1_700_000_000_000,
            1_700_000_000_000,
            Arc::from("UTC"),
        )
    }

    #[test]
    fn insert_context_check_row_passes_for_matching_user() {
        let schema = test_schema();
        let qctx = test_qctx("alice");
        let policies = vec![RlsPolicy {
            oid: 0,
            name: "insert_own".into(),
            table_id: 1,
            command: RlsCommand::Insert,
            permissive: true,
            roles: vec![],
            using_expr: None,
            with_check_expr: Some("user_id = 'alice'".into()),
        }];

        let ctx = RlsDmlContext::for_insert(&schema, &policies, "alice", false, false, &qctx).unwrap();

        let good_row = Row::new(vec![Value::Int32(1), Value::Text("alice".into())]);
        ctx.check_row(&schema, &good_row, &qctx).unwrap();

        let bad_row = Row::new(vec![Value::Int32(2), Value::Text("bob".into())]);
        let err = ctx.check_row(&schema, &bad_row, &qctx).unwrap_err();
        assert!(err.to_string().contains("row-level security policy"));
    }

    #[test]
    fn insert_context_check_returning_raises_error() {
        let schema = test_schema();
        let qctx = test_qctx("alice");
        let policies = vec![
            RlsPolicy {
                oid: 0,
                name: "insert_any".into(),
                table_id: 1,
                command: RlsCommand::Insert,
                permissive: true,
                roles: vec![],
                using_expr: None,
                with_check_expr: Some("true".into()),
            },
            RlsPolicy {
                oid: 0,
                name: "see_own".into(),
                table_id: 1,
                command: RlsCommand::Select,
                permissive: true,
                roles: vec![],
                using_expr: Some("user_id = 'alice'".into()),
                with_check_expr: None,
            },
        ];

        let ctx = RlsDmlContext::for_insert(&schema, &policies, "alice", true, false, &qctx).unwrap();

        // Returning alice's row → OK
        let alice_row = Row::new(vec![Value::Int32(1), Value::Text("alice".into())]);
        ctx.check_returning(&schema, &[alice_row], &qctx).unwrap();

        // Returning bob's row → error 42501
        let bob_row = Row::new(vec![Value::Int32(2), Value::Text("bob".into())]);
        let err = ctx.check_returning(&schema, &[bob_row], &qctx).unwrap_err();
        assert!(err.to_string().contains("row-level security policy"));
    }

    #[test]
    fn no_returning_skips_select_policy_compilation() {
        let schema = test_schema();
        let qctx = test_qctx("alice");
        let policies = vec![RlsPolicy {
            oid: 0,
            name: "insert_own".into(),
            table_id: 1,
            command: RlsCommand::Insert,
            permissive: true,
            roles: vec![],
            using_expr: None,
            with_check_expr: Some("true".into()),
        }];

        let ctx = RlsDmlContext::for_insert(&schema, &policies, "alice", false, false, &qctx).unwrap();
        assert!(ctx.select_policies.is_none());
        // check_returning is a no-op when select_policies is None
        let any_row = Row::new(vec![Value::Int32(1), Value::Text("bob".into())]);
        ctx.check_returning(&schema, &[any_row], &qctx).unwrap();
    }
}
