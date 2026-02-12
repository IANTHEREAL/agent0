//! Scope-chain binder for name resolution.
//!
//! Walks a parsed SQL AST and extracts relation dependencies with correct
//! CTE scoping (recursive / non-recursive, nested WITH, sequential
//! declaration order).  Replaces the heuristic `ScopeAwareChecker` in
//! `ddl.rs` and fixes #643, #644, #653, #654.

mod walk;

#[cfg(test)]
mod tests;

use std::collections::HashSet;

use anyhow::Result;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

use super::names;

// ── Types ──────────────────────────────────────────────────────────────

/// A relation dependency extracted from a view's SQL body.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum RelationDep {
    /// Schema-qualified reference: `FROM schema.table`
    Qualified { schema: String, name: String },
    /// Unqualified reference: `FROM table`
    /// Needs search_path resolution to match against targets.
    Unqualified { name: String },
}

/// A single scope frame in the name resolution chain.
/// Each `Query` node in the AST produces its own scope frame.
pub(crate) struct BindScope {
    /// CTE names visible at this scope level.
    /// Built incrementally during CTE traversal (key: normalized name).
    ctes: HashSet<String>,
}

impl BindScope {
    fn new() -> Self {
        Self {
            ctes: HashSet::new(),
        }
    }
}

/// Walks a SQL AST performing scope-aware name resolution.
pub(crate) struct Binder {
    /// Stack of scope frames (last = innermost).
    scopes: Vec<BindScope>,
    /// Extracted relation dependencies.
    deps: HashSet<RelationDep>,
}

impl Binder {
    pub(crate) fn new() -> Self {
        Self {
            scopes: Vec::new(),
            deps: HashSet::new(),
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(BindScope::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn current_scope_mut(&mut self) -> &mut BindScope {
        self.scopes.last_mut().expect("scope stack empty")
    }

    /// Record a relation reference, checking CTE shadows first.
    fn check_relation(&mut self, name: &sqlparser::ast::ObjectName) {
        let parts: Vec<String> = name.0.iter().map(|id| names::normalize_ident(id)).collect();

        // Only unqualified (1-part) names can be shadowed by CTEs.
        // Schema-qualified names (`FROM schema.table`) are never CTE references.
        if parts.len() == 1 {
            for scope in self.scopes.iter().rev() {
                if scope.ctes.contains(&parts[0]) {
                    return; // shadowed by CTE
                }
            }
        }

        // Not shadowed → record as dependency.
        match parts.len() {
            1 => {
                self.deps.insert(RelationDep::Unqualified {
                    name: parts[0].clone(),
                });
            }
            2 => {
                self.deps.insert(RelationDep::Qualified {
                    schema: parts[0].clone(),
                    name: parts[1].clone(),
                });
            }
            n if n >= 3 => {
                self.deps.insert(RelationDep::Qualified {
                    schema: parts[n - 2].clone(),
                    name: parts[n - 1].clone(),
                });
            }
            _ => {}
        }
    }

    /// Check whether a CTE body's top-level FROM clauses reference the
    /// given `cte_name`.  Used to distinguish truly recursive CTEs (where
    /// `FROM t` is the working table) from non-recursive CTEs that happen
    /// to be inside a `WITH RECURSIVE` block.
    ///
    /// Only inspects the immediate SetExpr level — nested subqueries have
    /// their own scope and are not checked.
    fn cte_body_references_name(body: &sqlparser::ast::SetExpr, cte_name: &str) -> bool {
        match body {
            sqlparser::ast::SetExpr::Select(select) => {
                for twj in &select.from {
                    if Self::table_factor_has_name(&twj.relation, cte_name) {
                        return true;
                    }
                    for join in &twj.joins {
                        if Self::table_factor_has_name(&join.relation, cte_name) {
                            return true;
                        }
                    }
                }
                false
            }
            sqlparser::ast::SetExpr::SetOperation { left, right, .. } => {
                Self::cte_body_references_name(left, cte_name)
                    || Self::cte_body_references_name(right, cte_name)
            }
            sqlparser::ast::SetExpr::Query(q) => {
                Self::cte_body_references_name(&q.body, cte_name)
            }
            _ => false,
        }
    }

    fn table_factor_has_name(
        factor: &sqlparser::ast::TableFactor,
        cte_name: &str,
    ) -> bool {
        match factor {
            sqlparser::ast::TableFactor::Table { name, .. } => {
                if let Some(last) = name.0.last() {
                    name.0.len() == 1
                        && names::normalize_ident(last) == cte_name
                }  else {
                    false
                }
            }
            sqlparser::ast::TableFactor::Derived { subquery, .. } => {
                // Don't descend into subqueries — they have their own scope.
                // But do check the subquery's top-level body.
                Self::cte_body_references_name(&subquery.body, cte_name)
            }
            _ => false,
        }
    }
}

// ── Public API ─────────────────────────────────────────────────────────

/// Extract all relation dependencies from SQL, with correct CTE scoping.
pub(crate) fn extract_dependencies(sql: &str) -> Result<HashSet<RelationDep>> {
    let dialect = PostgreSqlDialect {};
    let stmts = Parser::parse_sql(&dialect, sql)?;
    let mut binder = Binder::new();
    for stmt in &stmts {
        binder.walk_statement(stmt);
    }
    Ok(binder.deps)
}

/// Check whether `view_sql` references any of the fully-qualified names in
/// `targets`.
///
/// Drop-in replacement for the old `view_references_any()` in `ddl.rs`.
/// Cross-schema unqualified deps are a known limitation until a proper
/// dependency catalog is implemented (see #666).
pub(crate) fn view_references_any(
    view_sql: &str,
    view_schema: &str,
    targets: &[String],
) -> bool {
    let deps = match extract_dependencies(view_sql) {
        Ok(d) => d,
        Err(_) => return false,
    };
    deps.iter().any(|dep| {
        targets
            .iter()
            .any(|t| dep_matches_target(dep, t, view_schema))
    })
}

/// Check whether a single dependency matches a single target.
fn dep_matches_target(dep: &RelationDep, target: &str, view_schema: &str) -> bool {
    let (target_schema, target_name) = target.split_once('.').unwrap_or(("public", target));
    match dep {
        RelationDep::Qualified { schema, name } => {
            schema == target_schema && name == target_name
        }
        RelationDep::Unqualified { name } => {
            if name != target_name {
                return false;
            }
            // Without a dependency catalog (pg_depend), we cannot resolve
            // unqualified names across schemas — the creation-time search_path
            // is lost.  Conservative: only match the view's own schema.
            // Cross-schema unqualified deps are a known limitation until a
            // proper dependency catalog is implemented (see #666).
            target_schema == view_schema || target_schema == "public"
        }
    }
}
