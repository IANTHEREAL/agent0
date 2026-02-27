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

// ── #1022: cte_body_references_name direct tests ───────────────────────

/// Parse a full WITH RECURSIVE statement and call `cte_body_references_name`
/// on the body of the named CTE. Panics if the CTE is not found.
fn references_name(sql: &str, cte_name: &str) -> bool {
    use sqlparser::ast::Statement;
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;

    let stmts = Parser::parse_sql(&GenericDialect {}, sql).expect("parse failed");
    let query = match &stmts[0] {
        Statement::Query(q) => q,
        _ => panic!("not a query"),
    };
    let with = query.with.as_ref().expect("no WITH clause");
    let cte = with
        .cte_tables
        .iter()
        .find(|c| c.alias.name.value == cte_name)
        .expect("CTE not found");
    super::Binder::cte_body_references_name(&cte.query.body, cte_name)
}

#[test]
fn cte_body_references_name_true_for_top_level_ref() {
    // counter references itself at top level in UNION ALL arm → recursive
    assert!(references_name(
        "WITH RECURSIVE counter(n) AS (
             SELECT 1
             UNION ALL
             SELECT n+1 FROM counter WHERE n < 5
         ) SELECT * FROM counter",
        "counter"
    ));
}

#[test]
fn cte_body_references_name_false_when_only_inner_shadow() {
    // 'shadow' is referenced only inside an inner WITH shadow AS (...) that shadows it.
    // cte_body_references_name must return FALSE — this is the exact #1022 regression.
    assert!(!references_name(
        "WITH RECURSIVE shadow AS (
             SELECT 0
             UNION ALL
             SELECT sub.v FROM (
                 WITH shadow AS (SELECT 1 AS v)
                 SELECT v FROM shadow
             ) sub WHERE sub.v > 100
         ) SELECT * FROM shadow",
        "shadow"
    ));
}

#[test]
fn cte_body_references_name_false_for_non_recursive() {
    // Simple non-recursive CTE — name never appears in body
    assert!(!references_name(
        "WITH RECURSIVE simple AS (SELECT 42) SELECT * FROM simple",
        "simple"
    ));
}

// ── #1022: recursive CTE with nested WITH shadowing ────────────────────

#[test]
fn recursive_cte_nested_shadow_still_recursive() {
    // #1022: Recursive CTE whose recursive arm also contains a nested WITH
    // clause that shadows the outer CTE name inside a derived subquery.
    // cte_body_references_name must detect the top-level `FROM counter`
    // reference and classify the CTE as recursive despite the inner shadow.
    // All appearances of `counter` resolve to either the outer working table
    // or the inner CTE shadow — no real table dependencies.
    assert_eq!(
        deps(
            "WITH RECURSIVE counter AS (\
             SELECT 1 AS n \
             UNION ALL \
             SELECT counter.n + 1 FROM counter \
             JOIN (WITH counter AS (SELECT 100 AS shadow_n) \
                   SELECT shadow_n FROM counter) shadow_sub \
             ON shadow_sub.shadow_n > 0 \
             WHERE counter.n < 5) \
             SELECT n FROM counter ORDER BY n"
        ),
        HashSet::new()
    );
}

#[test]
fn non_recursive_body_has_real_table_dep_not_self_ref() {
    // #1022: CTE 'shadow_only' in WITH RECURSIVE where every reference to the
    // CTE name inside the body is enclosed in a nested WITH that shadows it.
    // cte_body_references_name returns false (non-recursive), so the body is
    // walked before shadow_only enters scope.
    // The body references 'real_t' at the top level → recorded as external dep.
    // The body also references 'shadow_only' inside an inner WITH that shadows
    // it → NOT recorded (inner shadow resolves it before it can escape).
    // Expected: {real_t} — non-empty, so a regression that drops real_t or
    // spuriously adds shadow_only would be caught.
    assert_eq!(
        deps(
            "WITH RECURSIVE shadow_only AS (\
               SELECT * FROM real_t \
               UNION ALL \
               SELECT v FROM (WITH shadow_only AS (SELECT 0 AS v) \
                              SELECT v FROM shadow_only) sub \
               WHERE sub.v > 100\
             ) SELECT * FROM shadow_only"
        ),
        HashSet::from([u("real_t")])
    );
}

#[test]
fn non_recursive_cte_body_only_shadowed_refs() {
    // #1022: CTE in WITH RECURSIVE block where every reference to the CTE
    // name inside the body is enclosed within a nested WITH that shadows it.
    // cte_body_references_name returns false → classified as non-recursive.
    // Main query also resolves `non_recursive_shadow` to the CTE shadow —
    // no real table dependencies.
    assert_eq!(
        deps(
            "WITH RECURSIVE non_recursive_shadow AS (\
             SELECT 42 AS val \
             UNION ALL \
             SELECT inner_q.val + 1 \
             FROM (WITH non_recursive_shadow AS (SELECT 0 AS val) \
                   SELECT val FROM non_recursive_shadow) inner_q \
             WHERE inner_q.val > 100) \
             SELECT val FROM non_recursive_shadow ORDER BY val"
        ),
        HashSet::new()
    );
}
