//! Post-Analyzer RLS predicate injection into `AnalyzedQuery`.
//!
//! Walks the analyzed query's FROM clause to find base tables with RLS enabled,
//! compiles applicable policies, combines them, and wraps each RLS-filtered
//! table in a **security barrier subquery**.
//!
//! ## Security barrier semantics
//!
//! PostgreSQL wraps RLS-filtered tables in security barrier subqueries to
//! prevent the optimizer from pushing user-supplied predicates below the RLS
//! filter.  Without this, a malicious user-defined function in a WHERE clause
//! could observe rows that should be hidden by RLS (information leak via
//! side-channel).
//!
//! We replicate this by converting:
//!   `FROM table WHERE user_pred`
//! into:
//!   `FROM (SELECT * FROM table WHERE rls_pred) AS table WHERE user_pred`
//!
//! The optimizer treats `Subquery` nodes as barriers — it never pushes
//! predicates through them — so user predicates stay above the RLS filter.

use super::cache::RlsPolicyCache;
use super::policy::{combine_rls_predicates, compile_applicable_using_policies, should_bypass_rls};
use crate::model::{DataType, RlsCommand, RlsPolicy, TableSchema};
use crate::sql::analyzer::types::{
    AnalyzedDistinct, AnalyzedProjection, AnalyzedQuery, AnalyzedQueryBody, AnalyzedSelect,
    AnalyzedTableRef, AnalyzedTableRefKind, TypedExpr, TypedExprKind,
};
use crate::sql::query_context::QueryContext;
use anyhow::Result;
use std::collections::HashMap;

/// Context needed for RLS predicate injection.
pub(crate) struct RlsContext<'a> {
    /// Current session role.
    pub current_role: &'a str,
    /// Whether the current role is a superuser.
    pub is_superuser: bool,
    /// Whether the current role has the BYPASSRLS attribute.
    pub bypass_rls: bool,
    /// Table schemas indexed by table name (schema-qualified).
    pub table_schemas: &'a HashMap<String, &'a TableSchema>,
    /// RLS policies indexed by table_id.
    pub policies_by_table: &'a HashMap<u64, Vec<RlsPolicy>>,
    /// Query context for expression compilation.
    pub qctx: &'a QueryContext,
    /// Which DML command we're enforcing (SELECT, UPDATE, DELETE).
    pub command: RlsCommand,
    /// Optional expression cache: (cache, db_id).
    /// Schema version is per-table, looked up from table_schemas.
    pub expr_cache: Option<(&'a RlsPolicyCache, u64)>,
}

/// Inject RLS predicates into an analyzed query.
///
/// For each base table in the FROM clause that has RLS enabled:
/// 1. Check bypass (superuser, owner)
/// 2. Load and compile applicable policies
/// 3. Combine (permissive OR + restrictive AND)
/// 4. Wrap the table in a security barrier subquery: `(SELECT * FROM t WHERE rls_pred)`
///
/// For SELECT, also applies SELECT policies.
/// For UPDATE/DELETE, applies both SELECT and command-specific policies.
pub(crate) fn inject_rls_predicates(
    mut query: AnalyzedQuery,
    ctx: &RlsContext<'_>,
) -> Result<AnalyzedQuery> {
    match &mut query.body {
        AnalyzedQueryBody::Select(select) => {
            inject_into_select(select, ctx)?;
        }
        AnalyzedQueryBody::SetOperation { left, right, .. } => {
            **left = inject_rls_predicates(*left.clone(), ctx)?;
            **right = inject_rls_predicates(*right.clone(), ctx)?;
        }
        AnalyzedQueryBody::Values(_) => {
            // VALUES clauses don't reference tables — nothing to inject.
        }
    }
    // Recurse into CTEs
    for cte in &mut query.ctes {
        cte.query = inject_rls_predicates(cte.query.clone(), ctx)?;
    }
    Ok(query)
}

/// Inject RLS predicates into a SELECT clause by wrapping each RLS-filtered
/// table in a security barrier subquery.
fn inject_into_select(select: &mut AnalyzedSelect, ctx: &RlsContext<'_>) -> Result<()> {
    for table_ref in &mut select.from {
        wrap_table_ref_with_rls(table_ref, ctx)?;
    }
    Ok(())
}

/// Recursively walk a table reference tree, wrapping base tables that have
/// RLS enabled in security barrier subqueries.
///
/// For base tables: compile policies and wrap as `(SELECT * FROM t WHERE rls_pred)`.
/// For joins: recurse into both sides.
/// For subqueries: handled by top-level recursion in `inject_rls_predicates`.
fn wrap_table_ref_with_rls(
    table_ref: &mut AnalyzedTableRef,
    ctx: &RlsContext<'_>,
) -> Result<()> {
    match &mut table_ref.kind {
        AnalyzedTableRefKind::Table { name, .. } => {
            let table_name = name.clone();
            if let Some(schema) = ctx.table_schemas.get(table_name.as_str()) {
                if !should_bypass_rls(
                    ctx.is_superuser,
                    ctx.bypass_rls,
                    ctx.current_role,
                    &schema.owner,
                    schema.rls_enabled,
                    schema.rls_force,
                ) {
                    let rls_pred = compile_rls_predicate_for_table(schema, ctx)?;
                    // Wrap: Table → Subquery(SELECT * FROM table WHERE rls_pred)
                    let original_kind = std::mem::replace(
                        &mut table_ref.kind,
                        // Temporary placeholder; replaced below.
                        AnalyzedTableRefKind::Subquery(Box::new(AnalyzedQuery {
                            ctes: vec![],
                            body: AnalyzedQueryBody::Values(vec![]),
                            order_by: vec![],
                            limit: None,
                            offset: None,
                            output_schema: vec![],
                        })),
                    );
                    let (inner_name, inner_schema) = match original_kind {
                        AnalyzedTableRefKind::Table { name, schema } => (name, schema),
                        _ => unreachable!(),
                    };
                    let subquery =
                        build_security_barrier_subquery(&inner_name, &inner_schema, rls_pred);
                    table_ref.kind = AnalyzedTableRefKind::Subquery(Box::new(subquery));
                }
            }
        }
        AnalyzedTableRefKind::Join { left, right, .. } => {
            wrap_table_ref_with_rls(left, ctx)?;
            wrap_table_ref_with_rls(right, ctx)?;
        }
        AnalyzedTableRefKind::Subquery(_) | AnalyzedTableRefKind::Function { .. } => {
            // Subqueries are handled by top-level recursion in inject_rls_predicates.
            // Table functions don't have RLS.
        }
    }
    Ok(())
}

/// Compile the combined RLS predicate for a table.
///
/// Returns `WHERE false` if RLS is enabled but no policies apply.
fn compile_rls_predicate_for_table(
    schema: &TableSchema,
    ctx: &RlsContext<'_>,
) -> Result<TypedExpr> {
    let policies = ctx.policies_by_table.get(&schema.table_id);
    if let Some(policies) = policies {
        let cache_args = ctx
            .expr_cache
            .map(|(cache, db_id)| (cache, db_id, schema.version));

        let mut all_compiled = compile_applicable_using_policies(
            policies,
            schema,
            &ctx.command,
            ctx.current_role,
            ctx.qctx,
            cache_args,
        )?;

        // If command is UPDATE or DELETE, also include SELECT policies
        if ctx.command == RlsCommand::Update || ctx.command == RlsCommand::Delete {
            let select_compiled = compile_applicable_using_policies(
                policies,
                schema,
                &RlsCommand::Select,
                ctx.current_role,
                ctx.qctx,
                cache_args,
            )?;
            all_compiled.extend(select_compiled);
        }

        Ok(combine_rls_predicates(&all_compiled).unwrap_or_else(|| {
            // All policies compiled but none have USING → deny all
            TypedExpr::new(
                TypedExprKind::Constant(crate::model::Value::Boolean(false)),
                DataType::Boolean,
            )
        }))
    } else {
        // RLS enabled but no policies → deny all
        Ok(TypedExpr::new(
            TypedExprKind::Constant(crate::model::Value::Boolean(false)),
            DataType::Boolean,
        ))
    }
}

/// Build a synthetic `AnalyzedQuery` representing:
///   `SELECT * FROM table WHERE rls_predicate`
///
/// This serves as a security barrier subquery — the optimizer will not
/// push user-supplied predicates through it.
fn build_security_barrier_subquery(
    table_name: &str,
    table_schema: &crate::sql::analyzer::types::TableRefSchema,
    rls_predicate: TypedExpr,
) -> AnalyzedQuery {
    // Build SELECT * projection: one ColumnRef per table column.
    let projection: Vec<AnalyzedProjection> = table_schema
        .columns
        .iter()
        .enumerate()
        .map(|(i, (col_name, col_type, _nullable))| AnalyzedProjection {
            output_name: col_name.clone(),
            expr: TypedExpr::new(
                TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: i,
                    column_name: col_name.clone(),
                },
                col_type.clone(),
            ),
        })
        .collect();

    // Output schema: (name, type, collation=None) for each column.
    let output_schema: Vec<(String, DataType, Option<crate::sql::collation::ResolvedCollation>)> =
        table_schema
            .columns
            .iter()
            .map(|(name, dt, _nullable)| (name.clone(), dt.clone(), None))
            .collect();

    // Inner FROM: the original table reference (no alias — alias lives on the outer Subquery).
    let inner_table_ref = AnalyzedTableRef {
        kind: AnalyzedTableRefKind::Table {
            name: table_name.to_string(),
            schema: table_schema.clone(),
        },
        alias: None,
    };

    AnalyzedQuery {
        ctes: vec![],
        body: AnalyzedQueryBody::Select(AnalyzedSelect {
            projection,
            from: vec![inner_table_ref],
            where_clause: Some(rls_predicate),
            group_by: vec![],
            having: None,
            distinct: AnalyzedDistinct::All,
        }),
        order_by: vec![],
        limit: None,
        offset: None,
        output_schema,
    }
}
