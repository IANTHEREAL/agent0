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

/// A single relation reference bound inside one query scope.
///
/// `qualifier` is the visible table qualifier for column references:
/// - explicit alias (`FROM t AS x` -> `x`)
/// - otherwise the relation name part (`FROM s.t` -> `t`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueryRelationRef {
    pub(crate) dep: RelationDep,
    pub(crate) qualifier: String,
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
    /// Extracted relation references in deterministic traversal order.
    relation_refs: Vec<RelationDep>,
    /// Per-query relation references (query pre-order).
    query_relation_scopes: Vec<Vec<QueryRelationRef>>,
    /// Stack of active query indices into `query_relation_scopes`.
    query_stack: Vec<usize>,
}

impl Binder {
    pub(crate) fn new() -> Self {
        Self {
            scopes: Vec::new(),
            deps: HashSet::new(),
            relation_refs: Vec::new(),
            query_relation_scopes: Vec::new(),
            query_stack: Vec::new(),
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
    fn check_relation(
        &mut self,
        name: &sqlparser::ast::ObjectName,
        alias: Option<&sqlparser::ast::Ident>,
    ) {
        let parts: Vec<String> = name.0.iter().map(names::normalize_ident).collect();

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
                let dep = RelationDep::Unqualified {
                    name: parts[0].clone(),
                };
                self.deps.insert(dep.clone());
                self.relation_refs.push(dep.clone());
                self.push_query_relation_ref(dep, alias, parts.last());
            }
            2 => {
                let dep = RelationDep::Qualified {
                    schema: parts[0].clone(),
                    name: parts[1].clone(),
                };
                self.deps.insert(dep.clone());
                self.relation_refs.push(dep.clone());
                self.push_query_relation_ref(dep, alias, parts.last());
            }
            n if n >= 3 => {
                let dep = RelationDep::Qualified {
                    schema: parts[n - 2].clone(),
                    name: parts[n - 1].clone(),
                };
                self.deps.insert(dep.clone());
                self.relation_refs.push(dep.clone());
                self.push_query_relation_ref(dep, alias, parts.last());
            }
            _ => {}
        }
    }

    fn push_query_relation_ref(
        &mut self,
        dep: RelationDep,
        alias: Option<&sqlparser::ast::Ident>,
        fallback_qualifier: Option<&String>,
    ) {
        let Some(&query_idx) = self.query_stack.last() else {
            return;
        };

        let qualifier = alias
            .map(names::normalize_ident)
            .or_else(|| fallback_qualifier.cloned())
            .unwrap_or_default();
        self.query_relation_scopes[query_idx].push(QueryRelationRef { dep, qualifier });
    }

    /// Check whether a CTE body's top-level FROM clauses reference the
    /// given `cte_name`.  Used to distinguish truly recursive CTEs (where
    /// `FROM t` is the working table) from non-recursive CTEs that happen
    /// to be inside a `WITH RECURSIVE` block.
    /// Uses the same binder relation-extraction path to avoid drift in
    /// table-factor handling and CTE scoping semantics.
    pub(crate) fn cte_body_references_name(body: &sqlparser::ast::SetExpr, cte_name: &str) -> bool {
        let query = sqlparser::ast::Query {
            with: None,
            body: Box::new(body.clone()),
            order_by: vec![],
            limit: None,
            offset: None,
            fetch: None,
            locks: vec![],
            limit_by: vec![],
            for_clause: None,
        };

        extract_relation_references_from_query(&query)
            .into_iter()
            .any(|dep| matches!(dep, RelationDep::Unqualified { name } if name == cte_name))
    }
}

// ── Public API ─────────────────────────────────────────────────────────

/// Extract all relation dependencies from SQL, with correct CTE scoping.
#[cfg(test)]
pub(crate) fn extract_dependencies(sql: &str) -> anyhow::Result<HashSet<RelationDep>> {
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    let dialect = PostgreSqlDialect {};
    let stmts = Parser::parse_sql(&dialect, sql)?;
    let mut binder = Binder::new();
    for stmt in &stmts {
        binder.walk_statement(stmt);
    }
    Ok(binder.deps)
}

/// Extract relation references from a parsed query in deterministic traversal
/// order. Includes duplicates.
pub(crate) fn extract_relation_references_from_query(
    query: &sqlparser::ast::Query,
) -> Vec<RelationDep> {
    let mut binder = Binder::new();
    binder.walk_query(query);
    binder.relation_refs
}

/// Extract per-query relation references (query pre-order).
///
/// Each outer vector entry corresponds to one query node, and the inner vector
/// stores table references visible in that query's own FROM scope, with
/// qualifier information.
pub(crate) fn extract_query_relation_refs_from_query(
    query: &sqlparser::ast::Query,
) -> Vec<Vec<QueryRelationRef>> {
    let mut binder = Binder::new();
    binder.walk_query(query);
    binder.query_relation_scopes
}
