//! Unit tests for the binder module.

use std::collections::HashSet;

use super::{extract_dependencies, RelationDep};

/// Helper: create an Unqualified dep.
fn u(name: &str) -> RelationDep {
    RelationDep::Unqualified {
        name: name.to_string(),
    }
}

/// Helper: create a Qualified dep.
fn q(schema: &str, name: &str) -> RelationDep {
    RelationDep::Qualified {
        schema: schema.to_string(),
        name: name.to_string(),
    }
}

fn deps(sql: &str) -> HashSet<RelationDep> {
    extract_dependencies(sql).expect("parse failed")
}

// ── extract_dependencies tests ─────────────────────────────────────────

#[test]
fn basic_dep() {
    assert_eq!(deps("SELECT * FROM t"), HashSet::from([u("t")]));
}

#[test]
fn schema_qualified() {
    assert_eq!(deps("SELECT * FROM s.t"), HashSet::from([q("s", "t")]));
}

#[test]
fn cte_shadow_simple() {
    // CTE `t` shadows any real table `t` in the main query.
    assert_eq!(
        deps("WITH t AS (SELECT 42) SELECT * FROM t"),
        HashSet::new()
    );
}

#[test]
fn cte_wrapping_real_table() {
    // Non-recursive CTE: name NOT visible in own body.
    // `FROM t` inside CTE body refers to real table.
    // `FROM t` in main query resolves to CTE.
    assert_eq!(
        deps("WITH t AS (SELECT * FROM t) SELECT * FROM t"),
        HashSet::from([u("t")])
    );
}

#[test]
fn recursive_cte_self_ref() {
    // #643: Recursive CTE self-reference → name visible in own body.
    // `FROM t` inside body is the working table, not a real table.
    assert_eq!(
        deps("WITH RECURSIVE t AS (SELECT 1 AS n UNION ALL SELECT n+1 FROM t WHERE n<10) SELECT * FROM t"),
        HashSet::new()
    );
}

#[test]
fn later_cte_shadows() {
    // #654: CTEs added sequentially. When `a`'s body is walked, CTE `t`
    // is not yet in scope → `FROM t` in `a`'s body is a real table ref.
    assert_eq!(
        deps("WITH a AS (SELECT * FROM t), t AS (SELECT 1) SELECT * FROM a"),
        HashSet::from([u("t")])
    );
}

#[test]
fn nested_with_reuse() {
    // #644: Each Query gets its own scope. Inner CTE `a` doesn't affect
    // outer CTE `a`'s body walk. Outer `a` references real table `t`.
    assert_eq!(
        deps(
            "WITH a AS (SELECT * FROM t) SELECT * FROM (WITH a AS (SELECT 1) SELECT * FROM a) sub"
        ),
        HashSet::from([u("t")])
    );
}

#[test]
fn subquery_dep() {
    assert_eq!(
        deps("SELECT * FROM (SELECT * FROM t) sub"),
        HashSet::from([u("t")])
    );
}

#[test]
fn expr_subquery() {
    assert_eq!(
        deps("SELECT (SELECT x FROM t) FROM s"),
        HashSet::from([u("t"), u("s")])
    );
}

#[test]
fn exists_subquery() {
    assert_eq!(
        deps("SELECT * FROM a WHERE EXISTS (SELECT 1 FROM b)"),
        HashSet::from([u("a"), u("b")])
    );
}

#[test]
fn non_recursive_in_recursive_block() {
    // Non-self-referencing CTEs inside WITH RECURSIVE are still CTEs.
    // `a` doesn't self-reference → name added after body walk.
    // `b` self-references → name added before body walk.
    // Both shadow in the main query.
    assert_eq!(
        deps("WITH RECURSIVE a AS (SELECT 1), b AS (SELECT 1 UNION ALL SELECT n FROM b) SELECT * FROM a, b"),
        HashSet::new()
    );
}

#[test]
fn qualified_not_shadowed() {
    // Schema-qualified names are never shadowed by CTEs.
    assert_eq!(
        deps("WITH t AS (SELECT 1) SELECT * FROM public.t"),
        HashSet::from([q("public", "t")])
    );
}

#[test]
fn self_referential_view() {
    // #639: A view referencing itself → just a normal dependency.
    // Cycle detection is in `drop_dependent_views`, not the binder.
    assert_eq!(deps("SELECT * FROM v"), HashSet::from([u("v")]));
}

#[test]
fn join_deps() {
    assert_eq!(
        deps("SELECT * FROM a JOIN b ON a.id = b.id"),
        HashSet::from([u("a"), u("b")])
    );
}

#[test]
fn cte_plus_real_table() {
    assert_eq!(
        deps("WITH c AS (SELECT 1) SELECT * FROM c, real_t"),
        HashSet::from([u("real_t")])
    );
}

#[test]
fn function_in_from() {
    // Functions in FROM (e.g. generate_series) are NOT table deps.
    assert_eq!(
        deps("SELECT * FROM generate_series(1, 10) AS g"),
        HashSet::new()
    );
}

#[test]
fn cte_in_join() {
    assert_eq!(
        deps("WITH c AS (SELECT 1 AS id) SELECT * FROM t JOIN c ON t.id = c.id"),
        HashSet::from([u("t")])
    );
}

#[test]
fn cte_in_union() {
    assert_eq!(
        deps("WITH c AS (SELECT * FROM t) SELECT * FROM c UNION SELECT * FROM s"),
        HashSet::from([u("t"), u("s")])
    );
}

#[test]
fn values_clause() {
    assert_eq!(
        deps("SELECT * FROM (VALUES (1), (2)) AS v(id)"),
        HashSet::new()
    );
}
