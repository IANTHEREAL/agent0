//! Alpha-equivalence normalization for the Typed IR.
//!
//! The Analyzer's Typed IR contains binder-local names (column names, table
//! aliases, CTE names, projection output names) that are semantically
//! irrelevant after analysis — all references are positional. Two queries
//! that differ only in these names are *alpha-equivalent*.
//!
//! This module provides a normalization pass that replaces binder-local names
//! with canonical positional identifiers, producing a canonical form suitable
//! for structural equality checks.
//!
//! Key insight: CTE names must be normalized CONSISTENTLY. If the first CTE
//! is named "foo" and becomes "cte_0", then all Table references to "foo"
//! (with table_id==0) must also become "cte_0". This preserves which CTE is
//! referenced while making it position-based rather than name-based.

use super::{
    AnalyzedCte, AnalyzedDistinct, AnalyzedProjection, AnalyzedQuery, AnalyzedQueryBody,
    AnalyzedSelect, AnalyzedTableRef, AnalyzedTableRefKind, JoinCondition, ResolvedUsingColumn,
    TableRefSchema, TypedExpr, TypedExprKind, TypedOrderByExpr,
};
use crate::model::DataType;
use std::cell::Cell;
use std::collections::HashMap;

thread_local! {
    /// Set during structural comparison of already-normalized queries.
    /// Prevents re-normalization of nested `AnalyzedQuery` values which would
    /// collapse scope-qualified CTE names (e.g., `cte_1_0` → `cte_0_0`).
    static COMPARING_NORMALIZED: Cell<bool> = const { Cell::new(false) };
}

/// RAII guard that resets `COMPARING_NORMALIZED` on drop (even on panic).
struct CompareGuard;

impl Drop for CompareGuard {
    fn drop(&mut self) {
        COMPARING_NORMALIZED.with(|f| f.set(false));
    }
}

/// Returns `true` if we are inside a structural comparison of normalized queries.
///
/// Used by `PartialEq for AnalyzedQuery` to decide whether to normalize or
/// compare structurally.
pub(super) fn is_comparing_normalized() -> bool {
    COMPARING_NORMALIZED.with(|f| f.get())
}

/// Context for normalization, tracking CTE name mappings.
struct NormalizationContext {
    /// Maps original CTE name -> canonical name (e.g., "foo" -> "cte_0_0")
    cte_name_map: HashMap<String, String>,
    /// Current scope depth — incremented for each nested query scope.
    /// Used to disambiguate CTEs at different nesting levels that would
    /// otherwise collide (e.g., outer cte_0 vs inner cte_0).
    scope_depth: usize,
}

impl NormalizationContext {
    fn new(query: &AnalyzedQuery) -> Self {
        let mut cte_name_map = HashMap::new();
        let scope_depth = 0;
        for (i, cte) in query.ctes.iter().enumerate() {
            cte_name_map.insert(cte.name.clone(), format!("cte_{}_{}", scope_depth, i));
        }
        Self {
            cte_name_map,
            scope_depth,
        }
    }

    /// Create a child context that inherits outer CTE mappings and adds
    /// the inner query's own CTEs on top (inner names shadow outer ones).
    /// The child scope depth is incremented so inner CTEs get distinct
    /// canonical names from outer CTEs at the same positional index.
    fn child_for_query(&self, query: &AnalyzedQuery) -> Self {
        let child_depth = self.scope_depth + 1;
        let mut cte_name_map = self.cte_name_map.clone();
        for (i, cte) in query.ctes.iter().enumerate() {
            cte_name_map.insert(cte.name.clone(), format!("cte_{}_{}", child_depth, i));
        }
        Self {
            cte_name_map,
            scope_depth: child_depth,
        }
    }
}

/// Normalize an `AnalyzedQuery` to its alpha-canonical form.
///
/// All binder-local names are replaced with canonical positional identifiers:
/// - CTE names: "cte_0", "cte_1", ...
/// - CTE column names: "col_0", "col_1", ...
/// - Table aliases: "alias_0", "alias_1", ...
/// - Output column names: "out_0", "out_1", ...
/// - Function output columns: "fcol_0", "fcol_1", ...
/// - ResolvedUsingColumn names: "using_0", "using_1", ...
/// - ColumnRef column_name: "" (empty, as it's binder-local)
///
/// The returned query is semantically equivalent to the input — only
/// presentational names change.
pub fn normalize_query(query: &AnalyzedQuery) -> AnalyzedQuery {
    let ctx = NormalizationContext::new(query);
    normalize_query_with_ctx(query, &ctx)
}

fn normalize_query_with_ctx(query: &AnalyzedQuery, ctx: &NormalizationContext) -> AnalyzedQuery {
    // Normalize CTEs first (they define the canonical names)
    let normalized_ctes: Vec<AnalyzedCte> = query
        .ctes
        .iter()
        .enumerate()
        .map(|(i, cte)| normalize_cte_with_ctx(cte, i, ctx))
        .collect();

    // Normalize the query body with the CTE mapping
    let normalized_body = normalize_query_body_with_ctx(&query.body, ctx);

    // Normalize ORDER BY
    let normalized_order_by: Vec<TypedOrderByExpr> = query
        .order_by
        .iter()
        .map(|o| normalize_order_by_expr_with_ctx(o, ctx))
        .collect();

    // Normalize LIMIT and OFFSET
    let normalized_limit = query
        .limit
        .as_ref()
        .map(|e| normalize_expr_with_ctx(e, ctx));
    let normalized_offset = query
        .offset
        .as_ref()
        .map(|e| normalize_expr_with_ctx(e, ctx));

    // Normalize output schema (column names -> "out_0", "out_1", ...)
    let normalized_output_schema: Vec<(
        String,
        DataType,
        Option<crate::sql::collation::ResolvedCollation>,
    )> = query
        .output_schema
        .iter()
        .enumerate()
        .map(|(i, (_, dt, coll))| (format!("out_{}", i), dt.clone(), coll.clone()))
        .collect();

    AnalyzedQuery {
        ctes: normalized_ctes,
        body: normalized_body,
        order_by: normalized_order_by,
        limit: normalized_limit,
        offset: normalized_offset,
        output_schema: normalized_output_schema,
    }
}

/// Normalize a CTE, replacing its name and column names with canonical forms.
fn normalize_cte_with_ctx(
    cte: &AnalyzedCte,
    index: usize,
    ctx: &NormalizationContext,
) -> AnalyzedCte {
    // Create a child context that inherits outer CTE mappings
    let inner_ctx = ctx.child_for_query(&cte.query);

    AnalyzedCte {
        name: format!("cte_{}_{}", ctx.scope_depth, index),
        query: normalize_query_with_ctx(&cte.query, &inner_ctx),
        columns: cte
            .columns
            .iter()
            .enumerate()
            .map(|(i, (_, dt, coll))| (format!("col_{}", i), dt.clone(), coll.clone()))
            .collect(),
        materialized: cte.materialized,
    }
}

/// Normalize a query body (SELECT, VALUES, or set operation).
fn normalize_query_body_with_ctx(
    body: &AnalyzedQueryBody,
    ctx: &NormalizationContext,
) -> AnalyzedQueryBody {
    match body {
        AnalyzedQueryBody::Select(select) => {
            AnalyzedQueryBody::Select(normalize_select_with_ctx(select, ctx))
        }
        AnalyzedQueryBody::Values(rows) => AnalyzedQueryBody::Values(
            rows.iter()
                .map(|row| {
                    row.iter()
                        .map(|e| normalize_expr_with_ctx(e, ctx))
                        .collect()
                })
                .collect(),
        ),
        AnalyzedQueryBody::SetOperation {
            op,
            all,
            left,
            right,
        } => {
            // Each subquery inherits outer CTE context
            let left_ctx = ctx.child_for_query(left);
            let right_ctx = ctx.child_for_query(right);

            AnalyzedQueryBody::SetOperation {
                op: *op,
                all: *all,
                left: Box::new(normalize_query_with_ctx(left, &left_ctx)),
                right: Box::new(normalize_query_with_ctx(right, &right_ctx)),
            }
        }
    }
}

/// Normalize a SELECT clause.
fn normalize_select_with_ctx(
    select: &AnalyzedSelect,
    ctx: &NormalizationContext,
) -> AnalyzedSelect {
    AnalyzedSelect {
        projection: select
            .projection
            .iter()
            .map(|p| normalize_projection_with_ctx(p, ctx))
            .collect(),
        from: select
            .from
            .iter()
            .map(|t| normalize_table_ref_with_ctx(t, ctx))
            .collect(),
        where_clause: select
            .where_clause
            .as_ref()
            .map(|e| normalize_expr_with_ctx(e, ctx)),
        group_by: select
            .group_by
            .iter()
            .map(|e| normalize_expr_with_ctx(e, ctx))
            .collect(),
        having: select
            .having
            .as_ref()
            .map(|e| normalize_expr_with_ctx(e, ctx)),
        distinct: match &select.distinct {
            AnalyzedDistinct::DistinctOn(exprs) => AnalyzedDistinct::DistinctOn(
                exprs
                    .iter()
                    .map(|e| normalize_expr_with_ctx(e, ctx))
                    .collect(),
            ),
            other => other.clone(),
        },
    }
}

/// Normalize a projection (SELECT list item).
fn normalize_projection_with_ctx(
    proj: &AnalyzedProjection,
    ctx: &NormalizationContext,
) -> AnalyzedProjection {
    AnalyzedProjection {
        expr: normalize_expr_with_ctx(&proj.expr, ctx),
        output_name: String::new(), // Ignore output name (binder-local)
    }
}

/// Normalize a table reference, replacing aliases and CTE names with canonical forms.
fn normalize_table_ref_with_ctx(
    table_ref: &AnalyzedTableRef,
    ctx: &NormalizationContext,
) -> AnalyzedTableRef {
    let normalized_kind = normalize_table_ref_kind_with_ctx(&table_ref.kind, ctx);
    AnalyzedTableRef {
        kind: normalized_kind,
        alias: Some(String::new()), // Ignore alias (binder-local)
    }
}

/// Normalize a table reference kind.
fn normalize_table_ref_kind_with_ctx(
    kind: &AnalyzedTableRefKind,
    ctx: &NormalizationContext,
) -> AnalyzedTableRefKind {
    match kind {
        AnalyzedTableRefKind::Table { name, schema } => {
            // For CTE references (table_id == 0), look up the canonical name
            let normalized_name = if schema.table_id == 0 {
                // CTE reference - use canonical name from context
                ctx.cte_name_map
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| name.clone())
            } else {
                String::new() // Base table - name is presentational (identified by table_id)
            };

            AnalyzedTableRefKind::Table {
                name: normalized_name,
                schema: normalize_table_ref_schema(schema),
            }
        }
        AnalyzedTableRefKind::Subquery(query) => {
            let sub_ctx = ctx.child_for_query(query);
            AnalyzedTableRefKind::Subquery(Box::new(normalize_query_with_ctx(query, &sub_ctx)))
        }
        AnalyzedTableRefKind::Join {
            left,
            right,
            join_type,
            condition,
            left_col_start,
        } => AnalyzedTableRefKind::Join {
            left: Box::new(normalize_table_ref_with_ctx(left, ctx)),
            right: Box::new(normalize_table_ref_with_ctx(right, ctx)),
            join_type: *join_type,
            condition: normalize_join_condition_with_ctx(condition, ctx),
            left_col_start: *left_col_start,
        },
        AnalyzedTableRefKind::Function {
            func,
            args,
            output_columns,
        } => AnalyzedTableRefKind::Function {
            func: func.clone(), // Function name is semantic, not binder-local
            args: args
                .iter()
                .map(|arg| {
                    // TypedFunctionArg is an enum - handle both variants
                    match arg {
                        crate::sql::analyzer::types::TypedFunctionArg::Positional(expr) => {
                            crate::sql::analyzer::types::TypedFunctionArg::Positional(
                                normalize_expr_with_ctx(expr, ctx),
                            )
                        }
                        crate::sql::analyzer::types::TypedFunctionArg::Named { name, expr } => {
                            // Named arg names are semantic (function parameter names), keep them
                            crate::sql::analyzer::types::TypedFunctionArg::Named {
                                name: name.clone(),
                                expr: normalize_expr_with_ctx(expr, ctx),
                            }
                        }
                    }
                })
                .collect(),
            output_columns: output_columns
                .iter()
                .enumerate()
                .map(|(i, (_, dt))| (format!("fcol_{}", i), dt.clone()))
                .collect(),
        },
    }
}

/// Normalize a table reference schema.
fn normalize_table_ref_schema(schema: &TableRefSchema) -> TableRefSchema {
    TableRefSchema {
        table_id: schema.table_id,
        columns: schema
            .columns
            .iter()
            .enumerate()
            .map(|(i, (_, dt, nullable))| (format!("col_{}", i), dt.clone(), *nullable))
            .collect(),
    }
}

/// Normalize a join condition.
fn normalize_join_condition_with_ctx(
    condition: &JoinCondition,
    ctx: &NormalizationContext,
) -> JoinCondition {
    match condition {
        JoinCondition::On(expr) => JoinCondition::On(normalize_expr_with_ctx(expr, ctx)),
        JoinCondition::Using(cols) => JoinCondition::Using(
            cols.iter()
                .enumerate()
                .map(|(i, col)| normalize_using_column(col, i))
                .collect(),
        ),
        JoinCondition::None => JoinCondition::None,
    }
}

/// Normalize a USING column.
fn normalize_using_column(col: &ResolvedUsingColumn, index: usize) -> ResolvedUsingColumn {
    ResolvedUsingColumn {
        name: format!("using_{}", index),
        left_index: col.left_index,
        right_index: col.right_index,
        data_type: col.data_type.clone(),
        left_type: col.left_type.clone(),
        right_type: col.right_type.clone(),
    }
}

/// Normalize an ORDER BY expression.
fn normalize_order_by_expr_with_ctx(
    expr: &TypedOrderByExpr,
    ctx: &NormalizationContext,
) -> TypedOrderByExpr {
    TypedOrderByExpr {
        expr: normalize_expr_with_ctx(&expr.expr, ctx),
        asc: expr.asc,
        nulls_first: expr.nulls_first,
    }
}

/// Normalize a typed expression, replacing binder-local column names.
///
/// Subquery variants (`ScalarSubquery`, `Exists`, `InSubquery`, `AnyAll`,
/// `ArraySubquery`, `TupleInSubquery`) are handled explicitly because
/// `map_children()` treats their `AnalyzedQuery` payloads as opaque
/// boundaries and would just clone them without normalization.
///
/// The `ctx` parameter carries outer CTE name mappings so that subqueries
/// referencing outer WITH bindings are normalized consistently.
fn normalize_expr_with_ctx(expr: &TypedExpr, ctx: &NormalizationContext) -> TypedExpr {
    use crate::sql::expr::traverse::map_children;

    let kind = match &expr.kind {
        TypedExprKind::ColumnRef {
            scope_depth,
            column_index,
            ..
        } => TypedExprKind::ColumnRef {
            scope_depth: *scope_depth,
            column_index: *column_index,
            column_name: String::new(), // Ignore column name (binder-local)
        },

        // ── Subquery variants: normalize the inner AnalyzedQuery ──
        // Create a child context that inherits outer CTE mappings.
        TypedExprKind::ScalarSubquery(q) => {
            let child_ctx = ctx.child_for_query(q);
            TypedExprKind::ScalarSubquery(Box::new(normalize_query_with_ctx(q, &child_ctx)))
        }
        TypedExprKind::ArraySubquery(q) => {
            let child_ctx = ctx.child_for_query(q);
            TypedExprKind::ArraySubquery(Box::new(normalize_query_with_ctx(q, &child_ctx)))
        }
        TypedExprKind::Exists { subquery, negated } => {
            let child_ctx = ctx.child_for_query(subquery);
            TypedExprKind::Exists {
                subquery: Box::new(normalize_query_with_ctx(subquery, &child_ctx)),
                negated: *negated,
            }
        }
        TypedExprKind::InSubquery {
            expr: inner,
            subquery,
            negated,
        } => {
            let child_ctx = ctx.child_for_query(subquery);
            TypedExprKind::InSubquery {
                expr: Box::new(normalize_expr_with_ctx(inner, ctx)),
                subquery: Box::new(normalize_query_with_ctx(subquery, &child_ctx)),
                negated: *negated,
            }
        }
        TypedExprKind::TupleInSubquery {
            exprs,
            subquery,
            negated,
        } => {
            let child_ctx = ctx.child_for_query(subquery);
            TypedExprKind::TupleInSubquery {
                exprs: exprs
                    .iter()
                    .map(|e| normalize_expr_with_ctx(e, ctx))
                    .collect(),
                subquery: Box::new(normalize_query_with_ctx(subquery, &child_ctx)),
                negated: *negated,
            }
        }
        TypedExprKind::AnyAll {
            expr: inner,
            op,
            subquery,
            is_all,
        } => {
            let child_ctx = ctx.child_for_query(subquery);
            TypedExprKind::AnyAll {
                expr: Box::new(normalize_expr_with_ctx(inner, ctx)),
                op: op.clone(),
                subquery: Box::new(normalize_query_with_ctx(subquery, &child_ctx)),
                is_all: *is_all,
            }
        }

        _ => map_children(expr, &mut |child| normalize_expr_with_ctx(child, ctx)),
    };

    TypedExpr {
        kind,
        data_type: expr.data_type.clone(),
    }
}

/// Compare two `AnalyzedQuery` values for alpha-equivalence.
///
/// Normalizes both queries to canonical form (replacing binder-local names
/// with positional identifiers), then compares structurally using the derived
/// `PartialEq`. This avoids the re-normalization issue that would occur if
/// `PartialEq` itself normalized — nested `AnalyzedQuery` values (inside
/// subqueries, set operations, CTEs) would be re-normalized without their
/// outer scope context, collapsing scope-qualified CTE names.
pub fn alpha_eq(a: &AnalyzedQuery, b: &AnalyzedQuery) -> bool {
    let norm_a = normalize_query(a);
    let norm_b = normalize_query(b);
    COMPARING_NORMALIZED.with(|f| f.set(true));
    let _guard = CompareGuard;
    norm_a == norm_b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DataType, Value};
    use crate::sql::analyzer::types::{AnalyzedDistinct, AnalyzedQueryBody, AnalyzedSelect};

    #[test]
    fn alpha_equivalent_ctes_compare_equal() {
        // Create two queries that differ only in CTE names
        // Query 1: WITH a(x) AS (SELECT 1) SELECT x FROM a
        // Query 2: WITH b(y) AS (SELECT 1) SELECT y FROM b

        let cte1 = AnalyzedCte {
            name: "a".to_string(),
            query: AnalyzedQuery {
                ctes: vec![],
                body: AnalyzedQueryBody::Select(AnalyzedSelect {
                    projection: vec![AnalyzedProjection {
                        expr: TypedExpr {
                            kind: TypedExprKind::Constant(Value::Int32(1)),
                            data_type: DataType::Int32,
                        },
                        output_name: "x".to_string(),
                    }],
                    from: vec![],
                    where_clause: None,
                    group_by: vec![],
                    having: None,
                    distinct: AnalyzedDistinct::All,
                }),
                order_by: vec![],
                limit: None,
                offset: None,
                output_schema: vec![("x".to_string(), DataType::Int32, None)],
            },
            columns: vec![("x".to_string(), DataType::Int32, None)],
            materialized: None,
        };

        let query1 = AnalyzedQuery {
            ctes: vec![cte1.clone()],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: TypedExpr {
                        kind: TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 0,
                            column_name: "x".to_string(),
                        },
                        data_type: DataType::Int32,
                    },
                    output_name: String::new(),
                }],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "a".to_string(),
                        schema: TableRefSchema {
                            table_id: 0,
                            columns: vec![("x".to_string(), DataType::Int32, true)],
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
            output_schema: vec![("x".to_string(), DataType::Int32, None)],
        };

        // Query 2: same structure, different CTE/column names
        let mut cte2 = cte1.clone();
        cte2.name = "b".to_string();
        cte2.query.output_schema = vec![("y".to_string(), DataType::Int32, None)];
        cte2.columns = vec![("y".to_string(), DataType::Int32, None)];

        let query2 = AnalyzedQuery {
            ctes: vec![cte2],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![AnalyzedProjection {
                    expr: TypedExpr {
                        kind: TypedExprKind::ColumnRef {
                            scope_depth: 0,
                            column_index: 0,
                            column_name: "y".to_string(),
                        },
                        data_type: DataType::Int32,
                    },
                    output_name: String::new(),
                }],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "b".to_string(),
                        schema: TableRefSchema {
                            table_id: 0,
                            columns: vec![("y".to_string(), DataType::Int32, true)],
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
            output_schema: vec![("y".to_string(), DataType::Int32, None)],
        };

        // These should be alpha-equivalent
        assert!(
            alpha_eq(&query1, &query2),
            "Alpha-equivalent queries should compare equal"
        );
    }

    #[test]
    fn alpha_equivalent_dependent_ctes_compare_equal() {
        // Regression test: outer CTE context must propagate into inner CTE bodies.
        // Query 1: WITH a(x) AS (SELECT 1), b(z) AS (SELECT x FROM a) SELECT z FROM b
        // Query 2: WITH u(y) AS (SELECT 1), v(w) AS (SELECT y FROM u) SELECT w FROM v
        // These are alpha-equivalent — only binder-local names differ.

        fn make_dependent_cte_query(
            cte0_name: &str,
            cte0_col: &str,
            cte1_name: &str,
            cte1_col: &str,
        ) -> AnalyzedQuery {
            // CTE 0: SELECT 1
            let cte0 = AnalyzedCte {
                name: cte0_name.to_string(),
                query: AnalyzedQuery {
                    ctes: vec![],
                    body: AnalyzedQueryBody::Select(AnalyzedSelect {
                        projection: vec![AnalyzedProjection {
                            expr: TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int32(1)),
                                data_type: DataType::Int32,
                            },
                            output_name: cte0_col.to_string(),
                        }],
                        from: vec![],
                        where_clause: None,
                        group_by: vec![],
                        having: None,
                        distinct: AnalyzedDistinct::All,
                    }),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    output_schema: vec![(cte0_col.to_string(), DataType::Int32, None)],
                },
                columns: vec![(cte0_col.to_string(), DataType::Int32, None)],
                materialized: None,
            };

            // CTE 1: SELECT <col> FROM <cte0> (references CTE 0)
            let cte1 = AnalyzedCte {
                name: cte1_name.to_string(),
                query: AnalyzedQuery {
                    ctes: vec![],
                    body: AnalyzedQueryBody::Select(AnalyzedSelect {
                        projection: vec![AnalyzedProjection {
                            expr: TypedExpr {
                                kind: TypedExprKind::ColumnRef {
                                    scope_depth: 0,
                                    column_index: 0,
                                    column_name: cte0_col.to_string(),
                                },
                                data_type: DataType::Int32,
                            },
                            output_name: cte1_col.to_string(),
                        }],
                        from: vec![AnalyzedTableRef {
                            kind: AnalyzedTableRefKind::Table {
                                name: cte0_name.to_string(), // References CTE 0
                                schema: TableRefSchema {
                                    table_id: 0,
                                    columns: vec![(cte0_col.to_string(), DataType::Int32, true)],
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
                    output_schema: vec![(cte1_col.to_string(), DataType::Int32, None)],
                },
                columns: vec![(cte1_col.to_string(), DataType::Int32, None)],
                materialized: None,
            };

            // Main body: SELECT <col> FROM <cte1>
            AnalyzedQuery {
                ctes: vec![cte0, cte1],
                body: AnalyzedQueryBody::Select(AnalyzedSelect {
                    projection: vec![AnalyzedProjection {
                        expr: TypedExpr {
                            kind: TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: 0,
                                column_name: cte1_col.to_string(),
                            },
                            data_type: DataType::Int32,
                        },
                        output_name: cte1_col.to_string(),
                    }],
                    from: vec![AnalyzedTableRef {
                        kind: AnalyzedTableRefKind::Table {
                            name: cte1_name.to_string(),
                            schema: TableRefSchema {
                                table_id: 0,
                                columns: vec![(cte1_col.to_string(), DataType::Int32, true)],
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
                output_schema: vec![(cte1_col.to_string(), DataType::Int32, None)],
            }
        }

        let query1 = make_dependent_cte_query("a", "x", "b", "z");
        let query2 = make_dependent_cte_query("u", "y", "v", "w");

        assert!(
            alpha_eq(&query1, &query2),
            "Dependent CTE queries with renamed CTEs should be alpha-equivalent"
        );
    }

    #[test]
    fn different_ctes_with_same_schema_compare_unequal() {
        // Two queries with different CTEs (same schema) should NOT be equal
        // This tests that we don't conflate distinct CTEs

        let cte1 = AnalyzedCte {
            name: "c1".to_string(),
            query: AnalyzedQuery {
                ctes: vec![],
                body: AnalyzedQueryBody::Select(AnalyzedSelect {
                    projection: vec![AnalyzedProjection {
                        expr: TypedExpr {
                            kind: TypedExprKind::Constant(Value::Int32(1)),
                            data_type: DataType::Int32,
                        },
                        output_name: String::new(),
                    }],
                    from: vec![],
                    where_clause: None,
                    group_by: vec![],
                    having: None,
                    distinct: AnalyzedDistinct::All,
                }),
                order_by: vec![],
                limit: None,
                offset: None,
                output_schema: vec![],
            },
            columns: vec![],
            materialized: None,
        };

        let cte2 = AnalyzedCte {
            name: "c2".to_string(),
            query: AnalyzedQuery {
                ctes: vec![],
                body: AnalyzedQueryBody::Select(AnalyzedSelect {
                    projection: vec![AnalyzedProjection {
                        expr: TypedExpr {
                            kind: TypedExprKind::Constant(Value::Int32(2)),
                            data_type: DataType::Int32,
                        },
                        output_name: String::new(),
                    }],
                    from: vec![],
                    where_clause: None,
                    group_by: vec![],
                    having: None,
                    distinct: AnalyzedDistinct::All,
                }),
                order_by: vec![],
                limit: None,
                offset: None,
                output_schema: vec![],
            },
            columns: vec![],
            materialized: None,
        };

        // A query referencing c1 should not equal a query referencing c2
        let query1 = AnalyzedQuery {
            ctes: vec![cte1.clone()],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "c1".to_string(),
                        schema: TableRefSchema {
                            table_id: 0,
                            columns: vec![],
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
        };

        let query2 = AnalyzedQuery {
            ctes: vec![cte2.clone()],
            body: AnalyzedQueryBody::Select(AnalyzedSelect {
                projection: vec![],
                from: vec![AnalyzedTableRef {
                    kind: AnalyzedTableRefKind::Table {
                        name: "c2".to_string(),
                        schema: TableRefSchema {
                            table_id: 0,
                            columns: vec![],
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
        };

        // These should NOT be equal - they reference different CTEs
        assert!(
            !alpha_eq(&query1, &query2),
            "Queries referencing different CTEs should not compare equal"
        );
    }

    #[test]
    fn nested_dependent_ctes_alpha_equivalent() {
        // WITH a(x) AS (SELECT 1), b(z) AS (SELECT x FROM a) SELECT z FROM b
        // vs
        // WITH u(y) AS (SELECT 1), v(w) AS (SELECT y FROM u) SELECT w FROM v
        //
        // These should compare equal (alpha-equivalent) because the inner CTE
        // references the outer CTE by name, and both map to the same canonical form.

        fn make_nested_cte_query(
            cte0_name: &str,
            cte0_col: &str,
            cte1_name: &str,
            cte1_col: &str,
        ) -> AnalyzedQuery {
            // First CTE: SELECT 1
            let cte0 = AnalyzedCte {
                name: cte0_name.to_string(),
                query: AnalyzedQuery {
                    ctes: vec![],
                    body: AnalyzedQueryBody::Select(AnalyzedSelect {
                        projection: vec![AnalyzedProjection {
                            expr: TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int32(1)),
                                data_type: DataType::Int32,
                            },
                            output_name: cte0_col.to_string(),
                        }],
                        from: vec![],
                        where_clause: None,
                        group_by: vec![],
                        having: None,
                        distinct: AnalyzedDistinct::All,
                    }),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    output_schema: vec![(cte0_col.to_string(), DataType::Int32, None)],
                },
                columns: vec![(cte0_col.to_string(), DataType::Int32, None)],
                materialized: None,
            };

            // Second CTE: SELECT <col> FROM <cte0> (references the first CTE)
            let cte1 = AnalyzedCte {
                name: cte1_name.to_string(),
                query: AnalyzedQuery {
                    ctes: vec![],
                    body: AnalyzedQueryBody::Select(AnalyzedSelect {
                        projection: vec![AnalyzedProjection {
                            expr: TypedExpr {
                                kind: TypedExprKind::ColumnRef {
                                    scope_depth: 0,
                                    column_index: 0,
                                    column_name: cte0_col.to_string(),
                                },
                                data_type: DataType::Int32,
                            },
                            output_name: cte1_col.to_string(),
                        }],
                        from: vec![AnalyzedTableRef {
                            kind: AnalyzedTableRefKind::Table {
                                name: cte0_name.to_string(),
                                schema: TableRefSchema {
                                    table_id: 0,
                                    columns: vec![(cte0_col.to_string(), DataType::Int32, true)],
                                },
                            },
                            alias: Some(cte0_name.to_string()),
                        }],
                        where_clause: None,
                        group_by: vec![],
                        having: None,
                        distinct: AnalyzedDistinct::All,
                    }),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    output_schema: vec![(cte1_col.to_string(), DataType::Int32, None)],
                },
                columns: vec![(cte1_col.to_string(), DataType::Int32, None)],
                materialized: None,
            };

            // Main query: SELECT <cte1_col> FROM <cte1>
            AnalyzedQuery {
                ctes: vec![cte0, cte1],
                body: AnalyzedQueryBody::Select(AnalyzedSelect {
                    projection: vec![AnalyzedProjection {
                        expr: TypedExpr {
                            kind: TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: 0,
                                column_name: cte1_col.to_string(),
                            },
                            data_type: DataType::Int32,
                        },
                        output_name: cte1_col.to_string(),
                    }],
                    from: vec![AnalyzedTableRef {
                        kind: AnalyzedTableRefKind::Table {
                            name: cte1_name.to_string(),
                            schema: TableRefSchema {
                                table_id: 0,
                                columns: vec![(cte1_col.to_string(), DataType::Int32, true)],
                            },
                        },
                        alias: Some(cte1_name.to_string()),
                    }],
                    where_clause: None,
                    group_by: vec![],
                    having: None,
                    distinct: AnalyzedDistinct::All,
                }),
                order_by: vec![],
                limit: None,
                offset: None,
                output_schema: vec![(cte1_col.to_string(), DataType::Int32, None)],
            }
        }

        let query1 = make_nested_cte_query("a", "x", "b", "z");
        let query2 = make_nested_cte_query("u", "y", "v", "w");

        assert!(
            alpha_eq(&query1, &query2),
            "Nested dependent CTEs with different names should be alpha-equivalent"
        );
    }

    #[test]
    fn subquery_cte_names_normalized() {
        // Two queries that differ only in CTE names *inside* a scalar subquery.
        // Before the fix, map_children() treated subqueries as opaque, so the
        // inner AnalyzedQuery was cloned without normalization and the two
        // queries compared unequal.
        //
        // Query 1: SELECT (WITH a AS (SELECT 1) SELECT * FROM a)
        // Query 2: SELECT (WITH z AS (SELECT 1) SELECT * FROM z)

        fn make_scalar_subquery_with_cte(cte_name: &str) -> AnalyzedQuery {
            let inner_cte = AnalyzedCte {
                name: cte_name.to_string(),
                query: AnalyzedQuery {
                    ctes: vec![],
                    body: AnalyzedQueryBody::Select(AnalyzedSelect {
                        projection: vec![AnalyzedProjection {
                            expr: TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int32(1)),
                                data_type: DataType::Int32,
                            },
                            output_name: "c".to_string(),
                        }],
                        from: vec![],
                        where_clause: None,
                        group_by: vec![],
                        having: None,
                        distinct: AnalyzedDistinct::All,
                    }),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    output_schema: vec![("c".to_string(), DataType::Int32, None)],
                },
                columns: vec![("c".to_string(), DataType::Int32, None)],
                materialized: None,
            };

            let subquery = AnalyzedQuery {
                ctes: vec![inner_cte],
                body: AnalyzedQueryBody::Select(AnalyzedSelect {
                    projection: vec![AnalyzedProjection {
                        expr: TypedExpr {
                            kind: TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: 0,
                                column_name: "c".to_string(),
                            },
                            data_type: DataType::Int32,
                        },
                        output_name: "c".to_string(),
                    }],
                    from: vec![AnalyzedTableRef {
                        kind: AnalyzedTableRefKind::Table {
                            name: cte_name.to_string(),
                            schema: TableRefSchema {
                                table_id: 0,
                                columns: vec![("c".to_string(), DataType::Int32, true)],
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
                output_schema: vec![("c".to_string(), DataType::Int32, None)],
            };

            // Outer query: SELECT (scalar_subquery)
            AnalyzedQuery {
                ctes: vec![],
                body: AnalyzedQueryBody::Select(AnalyzedSelect {
                    projection: vec![AnalyzedProjection {
                        expr: TypedExpr {
                            kind: TypedExprKind::ScalarSubquery(Box::new(subquery)),
                            data_type: DataType::Int32,
                        },
                        output_name: "sub".to_string(),
                    }],
                    from: vec![],
                    where_clause: None,
                    group_by: vec![],
                    having: None,
                    distinct: AnalyzedDistinct::All,
                }),
                order_by: vec![],
                limit: None,
                offset: None,
                output_schema: vec![("sub".to_string(), DataType::Int32, None)],
            }
        }

        let q1 = make_scalar_subquery_with_cte("a");
        let q2 = make_scalar_subquery_with_cte("z");

        assert!(
            alpha_eq(&q1, &q2),
            "Queries differing only in CTE names inside a scalar subquery should be alpha-equivalent"
        );
    }

    #[test]
    fn exists_subquery_cte_names_normalized() {
        // Verify EXISTS subquery variant is also normalized.
        // SELECT EXISTS (WITH a AS (SELECT 1) SELECT * FROM a)
        // vs
        // SELECT EXISTS (WITH b AS (SELECT 1) SELECT * FROM b)

        fn make_exists_with_cte(cte_name: &str) -> AnalyzedQuery {
            let inner_cte = AnalyzedCte {
                name: cte_name.to_string(),
                query: AnalyzedQuery {
                    ctes: vec![],
                    body: AnalyzedQueryBody::Select(AnalyzedSelect {
                        projection: vec![AnalyzedProjection {
                            expr: TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int32(1)),
                                data_type: DataType::Int32,
                            },
                            output_name: "v".to_string(),
                        }],
                        from: vec![],
                        where_clause: None,
                        group_by: vec![],
                        having: None,
                        distinct: AnalyzedDistinct::All,
                    }),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    output_schema: vec![("v".to_string(), DataType::Int32, None)],
                },
                columns: vec![("v".to_string(), DataType::Int32, None)],
                materialized: None,
            };

            let subquery = AnalyzedQuery {
                ctes: vec![inner_cte],
                body: AnalyzedQueryBody::Select(AnalyzedSelect {
                    projection: vec![AnalyzedProjection {
                        expr: TypedExpr {
                            kind: TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: 0,
                                column_name: "v".to_string(),
                            },
                            data_type: DataType::Int32,
                        },
                        output_name: "v".to_string(),
                    }],
                    from: vec![AnalyzedTableRef {
                        kind: AnalyzedTableRefKind::Table {
                            name: cte_name.to_string(),
                            schema: TableRefSchema {
                                table_id: 0,
                                columns: vec![("v".to_string(), DataType::Int32, true)],
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
                output_schema: vec![("v".to_string(), DataType::Int32, None)],
            };

            AnalyzedQuery {
                ctes: vec![],
                body: AnalyzedQueryBody::Select(AnalyzedSelect {
                    projection: vec![AnalyzedProjection {
                        expr: TypedExpr {
                            kind: TypedExprKind::Exists {
                                subquery: Box::new(subquery),
                                negated: false,
                            },
                            data_type: DataType::Boolean,
                        },
                        output_name: "exists_col".to_string(),
                    }],
                    from: vec![],
                    where_clause: None,
                    group_by: vec![],
                    having: None,
                    distinct: AnalyzedDistinct::All,
                }),
                order_by: vec![],
                limit: None,
                offset: None,
                output_schema: vec![("exists_col".to_string(), DataType::Boolean, None)],
            }
        }

        let q1 = make_exists_with_cte("a");
        let q2 = make_exists_with_cte("b");

        assert!(
            alpha_eq(&q1, &q2),
            "Queries differing only in CTE names inside an EXISTS subquery should be alpha-equivalent"
        );
    }

    #[test]
    fn outer_cte_ref_inside_scalar_subquery_normalized() {
        // Regression: outer CTE context must propagate into expression subqueries.
        // Query 1: WITH a(x) AS (SELECT 1) SELECT (SELECT x FROM a)
        // Query 2: WITH b(y) AS (SELECT 1) SELECT (SELECT y FROM b)
        // These are alpha-equivalent — the scalar subquery references an outer CTE.

        fn make_outer_cte_scalar_subquery(cte_name: &str, col_name: &str) -> AnalyzedQuery {
            let cte = AnalyzedCte {
                name: cte_name.to_string(),
                query: AnalyzedQuery {
                    ctes: vec![],
                    body: AnalyzedQueryBody::Select(AnalyzedSelect {
                        projection: vec![AnalyzedProjection {
                            expr: TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int32(1)),
                                data_type: DataType::Int32,
                            },
                            output_name: col_name.to_string(),
                        }],
                        from: vec![],
                        where_clause: None,
                        group_by: vec![],
                        having: None,
                        distinct: AnalyzedDistinct::All,
                    }),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    output_schema: vec![(col_name.to_string(), DataType::Int32, None)],
                },
                columns: vec![(col_name.to_string(), DataType::Int32, None)],
                materialized: None,
            };

            // Inner scalar subquery: SELECT <col> FROM <cte> (references outer CTE)
            let scalar_subquery = AnalyzedQuery {
                ctes: vec![],
                body: AnalyzedQueryBody::Select(AnalyzedSelect {
                    projection: vec![AnalyzedProjection {
                        expr: TypedExpr {
                            kind: TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: 0,
                                column_name: col_name.to_string(),
                            },
                            data_type: DataType::Int32,
                        },
                        output_name: col_name.to_string(),
                    }],
                    from: vec![AnalyzedTableRef {
                        kind: AnalyzedTableRefKind::Table {
                            name: cte_name.to_string(), // References outer CTE
                            schema: TableRefSchema {
                                table_id: 0,
                                columns: vec![(col_name.to_string(), DataType::Int32, true)],
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
                output_schema: vec![(col_name.to_string(), DataType::Int32, None)],
            };

            // Outer query: WITH <cte> AS (...) SELECT (scalar_subquery)
            AnalyzedQuery {
                ctes: vec![cte],
                body: AnalyzedQueryBody::Select(AnalyzedSelect {
                    projection: vec![AnalyzedProjection {
                        expr: TypedExpr {
                            kind: TypedExprKind::ScalarSubquery(Box::new(scalar_subquery)),
                            data_type: DataType::Int32,
                        },
                        output_name: "sub".to_string(),
                    }],
                    from: vec![],
                    where_clause: None,
                    group_by: vec![],
                    having: None,
                    distinct: AnalyzedDistinct::All,
                }),
                order_by: vec![],
                limit: None,
                offset: None,
                output_schema: vec![("sub".to_string(), DataType::Int32, None)],
            }
        }

        let q1 = make_outer_cte_scalar_subquery("a", "x");
        let q2 = make_outer_cte_scalar_subquery("b", "y");

        assert!(
            alpha_eq(&q1, &q2),
            "Outer CTE references inside scalar subqueries should be normalized (alpha-equivalent)"
        );
    }

    #[test]
    fn nested_scope_cte_collision_disambiguated() {
        // Regression: outer CTE at index 0 and inner CTE at index 0 must NOT
        // normalize to the same canonical name (was "cte_0" for both).
        //
        // Query 1: WITH a(x) AS (SELECT 1) SELECT (WITH c(y) AS (SELECT 2) SELECT x FROM a)
        // Query 2: WITH a(x) AS (SELECT 1) SELECT (WITH c(y) AS (SELECT 2) SELECT y FROM c)
        //
        // These are NOT alpha-equivalent: the first reads from outer CTE "a",
        // the second reads from inner CTE "c". Without scope-qualified names
        // both CTEs map to "cte_0" and the queries falsely compare equal.

        fn make_nested_scope_query(read_from_outer: bool) -> AnalyzedQuery {
            let outer_cte = AnalyzedCte {
                name: "a".to_string(),
                query: AnalyzedQuery {
                    ctes: vec![],
                    body: AnalyzedQueryBody::Select(AnalyzedSelect {
                        projection: vec![AnalyzedProjection {
                            expr: TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int32(1)),
                                data_type: DataType::Int32,
                            },
                            output_name: "x".to_string(),
                        }],
                        from: vec![],
                        where_clause: None,
                        group_by: vec![],
                        having: None,
                        distinct: AnalyzedDistinct::All,
                    }),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    output_schema: vec![("x".to_string(), DataType::Int32, None)],
                },
                columns: vec![("x".to_string(), DataType::Int32, None)],
                materialized: None,
            };

            let inner_cte = AnalyzedCte {
                name: "c".to_string(),
                query: AnalyzedQuery {
                    ctes: vec![],
                    body: AnalyzedQueryBody::Select(AnalyzedSelect {
                        projection: vec![AnalyzedProjection {
                            expr: TypedExpr {
                                kind: TypedExprKind::Constant(Value::Int32(2)),
                                data_type: DataType::Int32,
                            },
                            output_name: "y".to_string(),
                        }],
                        from: vec![],
                        where_clause: None,
                        group_by: vec![],
                        having: None,
                        distinct: AnalyzedDistinct::All,
                    }),
                    order_by: vec![],
                    limit: None,
                    offset: None,
                    output_schema: vec![("y".to_string(), DataType::Int32, None)],
                },
                columns: vec![("y".to_string(), DataType::Int32, None)],
                materialized: None,
            };

            // Inner scalar subquery reads from either outer "a" or inner "c"
            let (ref_name, ref_col) = if read_from_outer {
                ("a", "x")
            } else {
                ("c", "y")
            };

            let scalar_subquery = AnalyzedQuery {
                ctes: vec![inner_cte],
                body: AnalyzedQueryBody::Select(AnalyzedSelect {
                    projection: vec![AnalyzedProjection {
                        expr: TypedExpr {
                            kind: TypedExprKind::ColumnRef {
                                scope_depth: 0,
                                column_index: 0,
                                column_name: ref_col.to_string(),
                            },
                            data_type: DataType::Int32,
                        },
                        output_name: ref_col.to_string(),
                    }],
                    from: vec![AnalyzedTableRef {
                        kind: AnalyzedTableRefKind::Table {
                            name: ref_name.to_string(),
                            schema: TableRefSchema {
                                table_id: 0,
                                columns: vec![(ref_col.to_string(), DataType::Int32, true)],
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
                output_schema: vec![(ref_col.to_string(), DataType::Int32, None)],
            };

            AnalyzedQuery {
                ctes: vec![outer_cte],
                body: AnalyzedQueryBody::Select(AnalyzedSelect {
                    projection: vec![AnalyzedProjection {
                        expr: TypedExpr {
                            kind: TypedExprKind::ScalarSubquery(Box::new(scalar_subquery)),
                            data_type: DataType::Int32,
                        },
                        output_name: "sub".to_string(),
                    }],
                    from: vec![],
                    where_clause: None,
                    group_by: vec![],
                    having: None,
                    distinct: AnalyzedDistinct::All,
                }),
                order_by: vec![],
                limit: None,
                offset: None,
                output_schema: vec![("sub".to_string(), DataType::Int32, None)],
            }
        }

        let reads_outer = make_nested_scope_query(true);
        let reads_inner = make_nested_scope_query(false);

        assert!(
            !alpha_eq(&reads_outer, &reads_inner),
            "Query reading outer CTE vs inner CTE at same index must NOT be equal"
        );
    }

    #[test]
    fn distinct_on_expressions_normalized() {
        // DISTINCT ON expressions must be normalized — column names inside
        // DistinctOn(Vec<TypedExpr>) are binder-local and should not affect equality.
        //
        // Query 1: SELECT DISTINCT ON (x) x FROM t
        // Query 2: SELECT DISTINCT ON (y) y FROM t  (same table_id, same column_index)

        fn make_distinct_on_query(col_name: &str) -> AnalyzedQuery {
            let col_ref = TypedExpr {
                kind: TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: 0,
                    column_name: col_name.to_string(),
                },
                data_type: DataType::Int32,
            };

            AnalyzedQuery {
                ctes: vec![],
                body: AnalyzedQueryBody::Select(AnalyzedSelect {
                    projection: vec![AnalyzedProjection {
                        expr: col_ref.clone(),
                        output_name: col_name.to_string(),
                    }],
                    from: vec![AnalyzedTableRef {
                        kind: AnalyzedTableRefKind::Table {
                            name: "t".to_string(),
                            schema: TableRefSchema {
                                table_id: 42,
                                columns: vec![(col_name.to_string(), DataType::Int32, true)],
                            },
                        },
                        alias: Some(col_name.to_string()),
                    }],
                    where_clause: None,
                    group_by: vec![],
                    having: None,
                    distinct: AnalyzedDistinct::DistinctOn(vec![col_ref]),
                }),
                order_by: vec![],
                limit: None,
                offset: None,
                output_schema: vec![(col_name.to_string(), DataType::Int32, None)],
            }
        }

        let q1 = make_distinct_on_query("x");
        let q2 = make_distinct_on_query("y");

        assert!(
            alpha_eq(&q1, &q2),
            "DISTINCT ON expressions differing only in column names should be alpha-equivalent"
        );
    }
}
