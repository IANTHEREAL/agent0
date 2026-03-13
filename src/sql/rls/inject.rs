//! Post-Analyzer RLS predicate injection into `AnalyzedQuery`.
//!
//! Walks the analyzed query's FROM clause to find base tables with RLS enabled,
//! compiles applicable policies, combines them, and ANDs the result into WHERE.

use super::policy::{combine_rls_predicates, compile_applicable_using_policies, should_bypass_rls};
use crate::model::{DataType, RlsCommand, RlsPolicy, TableSchema};
use crate::sql::analyzer::types::{
    AnalyzedQuery, AnalyzedQueryBody, AnalyzedSelect, AnalyzedTableRef, AnalyzedTableRefKind,
    BinaryOp, TypedExpr, TypedExprKind,
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
    /// Table schemas indexed by table name (schema-qualified).
    pub table_schemas: &'a HashMap<String, &'a TableSchema>,
    /// RLS policies indexed by table_id.
    pub policies_by_table: &'a HashMap<u64, Vec<RlsPolicy>>,
    /// Query context for expression compilation.
    pub qctx: &'a QueryContext,
    /// Which DML command we're enforcing (SELECT, UPDATE, DELETE).
    pub command: RlsCommand,
}

/// Inject RLS predicates into an analyzed query.
///
/// For each base table in the FROM clause that has RLS enabled:
/// 1. Check bypass (superuser, owner)
/// 2. Load and compile applicable policies
/// 3. Combine (permissive OR + restrictive AND)
/// 4. AND into WHERE clause
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

/// Inject RLS predicates into a SELECT clause.
fn inject_into_select(select: &mut AnalyzedSelect, ctx: &RlsContext<'_>) -> Result<()> {
    // Collect RLS predicates from all base tables in FROM
    let mut rls_predicates: Vec<TypedExpr> = Vec::new();

    for table_ref in &select.from {
        collect_rls_predicates_from_table_ref(table_ref, ctx, &mut rls_predicates)?;
    }

    // AND all RLS predicates into the existing WHERE clause
    if !rls_predicates.is_empty() {
        let rls_combined = rls_predicates
            .into_iter()
            .reduce(|a, b| {
                TypedExpr::new(
                    TypedExprKind::BinaryOp {
                        left: Box::new(a),
                        op: BinaryOp::And,
                        right: Box::new(b),
                    },
                    DataType::Boolean,
                )
            })
            .unwrap();

        select.where_clause = match select.where_clause.take() {
            Some(existing) => Some(TypedExpr::new(
                TypedExprKind::BinaryOp {
                    left: Box::new(existing),
                    op: BinaryOp::And,
                    right: Box::new(rls_combined),
                },
                DataType::Boolean,
            )),
            None => Some(rls_combined),
        };
    }

    Ok(())
}

/// Recursively collect RLS predicates from a table reference.
///
/// For base tables: compile and combine policies.
/// For joins: recurse into both sides.
/// For subqueries: inject into the subquery (handled by top-level recursion).
fn collect_rls_predicates_from_table_ref(
    table_ref: &AnalyzedTableRef,
    ctx: &RlsContext<'_>,
    out: &mut Vec<TypedExpr>,
) -> Result<()> {
    match &table_ref.kind {
        AnalyzedTableRefKind::Table { name, .. } => {
            if let Some(schema) = ctx.table_schemas.get(name.as_str()) {
                if !should_bypass_rls(
                    ctx.is_superuser,
                    ctx.current_role,
                    &schema.owner,
                    schema.rls_enabled,
                    schema.rls_force,
                ) {
                    let policies = ctx.policies_by_table.get(&schema.table_id);
                    if let Some(policies) = policies {
                        // For UPDATE/DELETE, SELECT policies also apply to the read path
                        let mut all_compiled = compile_applicable_using_policies(
                            policies,
                            schema,
                            &ctx.command,
                            ctx.current_role,
                            ctx.qctx,
                        )?;

                        // If command is UPDATE or DELETE, also include SELECT policies
                        if ctx.command == RlsCommand::Update || ctx.command == RlsCommand::Delete {
                            let select_compiled = compile_applicable_using_policies(
                                policies,
                                schema,
                                &RlsCommand::Select,
                                ctx.current_role,
                                ctx.qctx,
                            )?;
                            all_compiled.extend(select_compiled);
                        }

                        if let Some(pred) = combine_rls_predicates(&all_compiled) {
                            out.push(pred);
                        }
                    } else {
                        // RLS enabled but no policies → deny all
                        out.push(TypedExpr::new(
                            TypedExprKind::Constant(crate::model::Value::Boolean(false)),
                            DataType::Boolean,
                        ));
                    }
                }
            }
        }
        AnalyzedTableRefKind::Join { left, right, .. } => {
            collect_rls_predicates_from_table_ref(left, ctx, out)?;
            collect_rls_predicates_from_table_ref(right, ctx, out)?;
        }
        AnalyzedTableRefKind::Subquery(_) | AnalyzedTableRefKind::Function { .. } => {
            // Subqueries are handled by top-level recursion in inject_rls_predicates.
            // Table functions don't have RLS.
        }
    }
    Ok(())
}
