//! Observability query detection/gating

use super::{ControlFlow, Query, SetExpr, Statement, TableFactor, Visit, Visitor};

pub(super) const OBSERVABILITY_USER: &str = "_db9_sys_observer";

fn query_has_nested_queries(query: &Query) -> bool {
    struct NestedQueryVisitor {
        seen: bool,
        has_nested: bool,
    }

    impl Visitor for NestedQueryVisitor {
        type Break = ();

        fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
            if self.seen {
                self.has_nested = true;
                return ControlFlow::Break(());
            }
            self.seen = true;
            ControlFlow::Continue(())
        }
    }

    let mut visitor = NestedQueryVisitor {
        seen: false,
        has_nested: false,
    };
    let _ = query.visit(&mut visitor);
    visitor.has_nested
}

pub(super) fn is_observability_system_query(stmt: &Statement) -> bool {
    let Statement::Query(query) = stmt else {
        return false;
    };

    if query.with.is_some() {
        return false;
    };
    if !query.locks.is_empty() || query.for_clause.is_some() {
        return false;
    }
    if query_has_nested_queries(query) {
        return false;
    }

    let SetExpr::Select(select) = query.body.as_ref() else {
        return false;
    };
    if select.into.is_some() {
        return false;
    }
    if !select.lateral_views.is_empty() {
        return false;
    }
    if select.from.len() != 1 {
        return false;
    }
    if !select.from[0].joins.is_empty() {
        return false;
    }
    let TableFactor::Table { name, .. } = &select.from[0].relation else {
        return false;
    };

    let Some(base) = name.0.last() else {
        return false;
    };
    let base_upper = base.value.to_ascii_uppercase();
    if !matches!(
        base_upper.as_str(),
        "_DB9_SYS_OBSERVABILITY"
            | "_DB9_SYS_QUERY_SAMPLES"
            | "_DB9_SYS_STORAGE_STATS"
            | "FS9_CACHED_STORAGE_STATS"
    ) {
        return false;
    }

    true
}

pub(super) fn is_observability_tableless_query(stmt: &Statement) -> bool {
    let Statement::Query(query) = stmt else {
        return false;
    };

    if query.with.is_some() {
        return false;
    }
    if !query.locks.is_empty() || query.for_clause.is_some() {
        return false;
    }
    if query_has_nested_queries(query) {
        return false;
    }

    let SetExpr::Select(select) = query.body.as_ref() else {
        return false;
    };
    if select.into.is_some() {
        return false;
    }
    if !select.lateral_views.is_empty() {
        return false;
    }
    select.from.is_empty()
}

#[cfg(test)]
mod tests {
    use super::is_observability_system_query;
    use crate::sql::parse_sql;
    use sqlparser::ast::Statement;

    fn parse_stmt(sql: &str) -> Statement {
        let mut stmts = parse_sql(sql).expect("parse");
        stmts.remove(0)
    }

    #[test]
    fn allows_observability_tvf() {
        assert!(is_observability_system_query(&parse_stmt(
            "SELECT * FROM _db9_sys_observability()"
        )));
    }

    #[test]
    fn allows_query_samples() {
        assert!(is_observability_system_query(&parse_stmt(
            "SELECT * FROM _db9_sys_query_samples"
        )));
    }

    #[test]
    fn allows_storage_stats_virtual_table() {
        assert!(is_observability_system_query(&parse_stmt(
            "SELECT total_bytes FROM _db9_sys_storage_stats LIMIT 1"
        )));
    }

    #[test]
    fn allows_fs9_cached_storage_stats_tvf() {
        assert!(is_observability_system_query(&parse_stmt(
            "SELECT total_logical_bytes FROM extensions.fs9_cached_storage_stats() LIMIT 1"
        )));
    }

    #[test]
    fn denies_arbitrary_table() {
        assert!(!is_observability_system_query(&parse_stmt(
            "SELECT * FROM users"
        )));
    }

    #[test]
    fn denies_subquery() {
        assert!(!is_observability_system_query(&parse_stmt(
            "SELECT * FROM _db9_sys_observability() WHERE id IN (SELECT 1)"
        )));
    }

    #[test]
    fn denies_join() {
        assert!(!is_observability_system_query(&parse_stmt(
            "SELECT * FROM _db9_sys_observability() JOIN users ON true"
        )));
    }
}
