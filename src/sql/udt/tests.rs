use super::*;

use std::collections::{HashMap, HashSet};

use crate::sql::analyzer::Catalog;
use crate::types::{CheckConstraint, ColumnDef, DataType, IndexDef, TableSchema};
use crate::worker::types::IndexState;

use enum_rewrite::{
    build_catalog_snapshot_for_view_bindings, rewrite_query_enum_literals_with_catalog,
    RelationColumnInfo, RelationEnumCatalog,
};
use enum_values::update_schema_enum_literal;
use helpers::{expr_has_unqualified_type_cast, query_has_unqualified_type_cast};
use rename::{parse_stored_query, rewrite_expr_type_casts};

fn col(name: &str, data_type: DataType) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type,
        nullable: true,
        primary_key: false,
        unique: false,
        is_serial: false,
        default_expr: None,
        collation: None,
    }
}

fn test_schema() -> TableSchema {
    TableSchema::new("public.t".to_string(), 1, vec![], vec![])
}

#[test]
fn type_cast_rewrite_is_identifier_safe() {
    let no_change =
        rewrite_expr_type_casts("'x'::mood2", "public.mood", "public.feeling", true).unwrap();
    assert!(no_change.is_none());

    let rewritten =
        rewrite_expr_type_casts("'x'::mood", "public.mood", "public.feeling", true).unwrap();
    let rewritten = rewritten.expect("cast should be rewritten");
    assert!(rewritten.contains("feeling"));
    assert!(!rewritten.contains("mood2"));
}

#[test]
fn type_cast_rewrite_requires_safe_unqualified_match() {
    let no_change =
        rewrite_expr_type_casts("'x'::mood", "public.mood", "public.feeling", false).unwrap();
    assert!(no_change.is_none());

    let qualified =
        rewrite_expr_type_casts("'x'::public.mood", "public.mood", "public.feeling", false)
            .unwrap()
            .expect("qualified cast should still be rewritten");
    assert!(qualified.contains("public.feeling"));
}

#[test]
fn ambiguous_unqualified_cast_detection_is_precise() {
    assert!(expr_has_unqualified_type_cast("'x'::mood", "mood").unwrap());
    assert!(!expr_has_unqualified_type_cast("'x'::public.mood", "mood").unwrap());
    assert!(query_has_unqualified_type_cast("SELECT 'x'::mood", "mood").unwrap());
    assert!(!query_has_unqualified_type_cast("SELECT 'x'::public.mood", "mood").unwrap());
}

#[test]
fn enum_literal_rewrite_preserves_unrelated_text_literals() {
    let mut schema = test_schema();
    schema.columns = vec![
        col("txt", DataType::Text),
        col("state", DataType::UserDefined("public.status".to_string())),
    ];
    schema.check_constraints = vec![CheckConstraint {
        name: Some("chk".to_string()),
        expr: "txt <> 'active' AND state <> 'active'".to_string(),
    }];
    schema.indexes = vec![IndexDef {
        name: "idx_partial".to_string(),
        id: 1,
        columns: vec![],
        unique: false,
        method: None,
        predicate: Some("state = 'active' AND txt <> 'active'".to_string()),
        expressions: vec!["CASE WHEN state = 'active' THEN 1 ELSE 0 END".to_string()],
        state: IndexState::Ready,
    }];

    let changed =
        update_schema_enum_literal(&mut schema, "public.status", "active", "enabled", true)
            .unwrap();
    assert!(changed);

    let check = &schema.check_constraints[0].expr;
    assert!(check.contains("txt <> 'active'"));
    assert!(check.contains("state <> 'enabled'"));

    let pred = schema.indexes[0].predicate.as_deref().unwrap();
    assert!(pred.contains("txt <> 'active'"));
    assert!(pred.contains("state = 'enabled'"));

    let expr = &schema.indexes[0].expressions[0];
    assert!(expr.contains("state = 'enabled'"));
}

#[test]
fn enum_default_bare_literal_is_rewritten() {
    let mut schema = test_schema();
    let mut state_col = col("state", DataType::UserDefined("public.status".to_string()));
    state_col.default_expr = Some("'active'".to_string());
    schema.columns = vec![state_col];

    let changed =
        update_schema_enum_literal(&mut schema, "public.status", "active", "enabled", true)
            .unwrap();
    assert!(changed);
    assert_eq!(schema.columns[0].default_expr.as_deref(), Some("'enabled'"));
}

#[test]
fn enum_casts_are_rewritten_even_without_enum_typed_columns() {
    let mut schema = test_schema();
    let mut txt_col = col("txt", DataType::Text);
    txt_col.default_expr = Some("('active'::status)::text".to_string());
    schema.columns = vec![txt_col];
    schema.check_constraints = vec![CheckConstraint {
        name: Some("chk_cast".to_string()),
        expr: "('active'::status)::text <> txt".to_string(),
    }];

    let changed =
        update_schema_enum_literal(&mut schema, "public.status", "active", "enabled", true)
            .unwrap();
    assert!(changed);
    let default_expr = schema.columns[0]
        .default_expr
        .as_deref()
        .expect("default expr should exist");
    assert!(default_expr.contains("enabled"));
    assert!(default_expr.contains("status"));
    assert!(
        schema.check_constraints[0].expr.contains("enabled")
            && schema.check_constraints[0].expr.contains("status")
    );
}

#[test]
fn view_query_rewrite_supports_column_context_literals() {
    let mut catalog = RelationEnumCatalog::default();
    catalog.insert(
        "public.t".to_string(),
        RelationColumnInfo {
            all_columns: ["state", "txt"]
                .into_iter()
                .map(ToString::to_string)
                .collect(),
            enum_columns: ["state"].into_iter().map(ToString::to_string).collect(),
        },
    );

    let rewritten = rewrite_query_enum_literals_with_catalog(
        "SELECT state, txt FROM t WHERE state = 'active' AND txt = 'active'",
        &["public.t".to_string()],
        &catalog,
        None,
        "public.status",
        "active",
        "enabled",
        true,
    )
    .unwrap()
    .expect("query should be rewritten");

    assert!(rewritten.contains("state = 'enabled'"));
    assert!(rewritten.contains("txt = 'active'"));
}

#[test]
fn view_query_rewrite_avoids_non_enum_same_name_columns() {
    let mut catalog = RelationEnumCatalog::default();
    catalog.insert(
        "public.t_enum".to_string(),
        RelationColumnInfo {
            all_columns: ["state"].into_iter().map(ToString::to_string).collect(),
            enum_columns: ["state"].into_iter().map(ToString::to_string).collect(),
        },
    );
    catalog.insert(
        "public.t_text".to_string(),
        RelationColumnInfo {
            all_columns: ["state"].into_iter().map(ToString::to_string).collect(),
            enum_columns: HashSet::new(),
        },
    );

    let rewritten = rewrite_query_enum_literals_with_catalog(
        "SELECT 1 FROM t_enum e JOIN t_text t ON e.state <> t.state WHERE e.state = 'active' AND t.state = 'active'",
        &["public.t_enum".to_string(), "public.t_text".to_string()],
        &catalog,
        None,
        "public.status",
        "active",
        "enabled",
        true,
    )
    .unwrap()
    .expect("query should be rewritten");

    assert!(rewritten.contains("e.state = 'enabled'"));
    assert!(rewritten.contains("t.state = 'active'"));
}

#[test]
fn view_query_rewrite_mixed_qualified_unqualified_same_bare_name() {
    let mut catalog = RelationEnumCatalog::default();
    catalog.insert(
        "a.t".to_string(),
        RelationColumnInfo {
            all_columns: ["state", "id"]
                .into_iter()
                .map(ToString::to_string)
                .collect(),
            enum_columns: ["state"].into_iter().map(ToString::to_string).collect(),
        },
    );
    catalog.insert(
        "z.t".to_string(),
        RelationColumnInfo {
            all_columns: ["state", "id"]
                .into_iter()
                .map(ToString::to_string)
                .collect(),
            enum_columns: HashSet::new(),
        },
    );

    let rewritten = rewrite_query_enum_literals_with_catalog(
        "SELECT 1 FROM a.t qa JOIN t qz ON qa.state <> qz.state WHERE qa.state = 'active' AND qz.state = 'active'",
        &["a.t".to_string(), "z.t".to_string()],
        &catalog,
        None,
        "public.status",
        "active",
        "enabled",
        true,
    )
    .unwrap()
    .expect("query should be rewritten");

    assert!(rewritten.contains("qa.state = 'enabled'"));
    assert!(rewritten.contains("qz.state = 'active'"));
}

#[test]
fn view_query_rewrite_handles_derived_table_alias_columns() {
    let mut catalog = RelationEnumCatalog::default();
    catalog.insert(
        "public.t".to_string(),
        RelationColumnInfo {
            all_columns: ["state", "txt"]
                .into_iter()
                .map(ToString::to_string)
                .collect(),
            enum_columns: ["state"].into_iter().map(ToString::to_string).collect(),
        },
    );

    let mut dep_schemas: HashMap<String, TableSchema> = HashMap::new();
    let mut t = test_schema();
    t.name = "public.t".to_string();
    t.columns = vec![
        col("state", DataType::UserDefined("public.status".to_string())),
        col("txt", DataType::Text),
    ];
    dep_schemas.insert("public.t".to_string(), t);

    let query = parse_stored_query(
        "SELECT 1 FROM (SELECT state, txt FROM t) s WHERE s.state = 'active' AND s.txt = 'active'",
    )
    .expect("query should parse");
    let relation_bindings = vec!["public.t".to_string()];
    let snapshot =
        build_catalog_snapshot_for_view_bindings(42, &query, &relation_bindings, &dep_schemas)
            .expect("snapshot should build");

    let rewritten = rewrite_query_enum_literals_with_catalog(
        "SELECT 1 FROM (SELECT state, txt FROM t) s WHERE s.state = 'active' AND s.txt = 'active'",
        &relation_bindings,
        &catalog,
        Some(&snapshot),
        "public.status",
        "active",
        "enabled",
        true,
    )
    .unwrap()
    .expect("query should be rewritten");

    assert!(rewritten.contains("s.state = 'enabled'"));
    assert!(rewritten.contains("s.txt = 'active'"));
}

#[test]
fn view_query_rewrite_handles_derived_alias_from_outer_cte() {
    let mut catalog = RelationEnumCatalog::default();
    catalog.insert(
        "public.t".to_string(),
        RelationColumnInfo {
            all_columns: ["state", "txt"]
                .into_iter()
                .map(ToString::to_string)
                .collect(),
            enum_columns: ["state"].into_iter().map(ToString::to_string).collect(),
        },
    );

    let mut dep_schemas: HashMap<String, TableSchema> = HashMap::new();
    let mut t = test_schema();
    t.name = "public.t".to_string();
    t.columns = vec![
        col("state", DataType::UserDefined("public.status".to_string())),
        col("txt", DataType::Text),
    ];
    dep_schemas.insert("public.t".to_string(), t);

    let sql = "WITH c AS (SELECT state, txt FROM t) SELECT 1 FROM (SELECT state, txt FROM c) s WHERE s.state = 'active' AND s.txt = 'active'";
    let query = parse_stored_query(sql).expect("query should parse");
    let relation_bindings = vec!["public.t".to_string()];
    let snapshot =
        build_catalog_snapshot_for_view_bindings(42, &query, &relation_bindings, &dep_schemas)
            .expect("snapshot should build");

    let rewritten = rewrite_query_enum_literals_with_catalog(
        sql,
        &relation_bindings,
        &catalog,
        Some(&snapshot),
        "public.status",
        "active",
        "enabled",
        true,
    )
    .unwrap()
    .expect("query should be rewritten");

    assert!(rewritten.contains("s.state = 'enabled'"));
    assert!(rewritten.contains("s.txt = 'active'"));
}

#[test]
fn view_query_rewrite_handles_derived_alias_from_recursive_outer_cte() {
    let mut catalog = RelationEnumCatalog::default();
    catalog.insert(
        "public.t".to_string(),
        RelationColumnInfo {
            all_columns: ["state", "txt"]
                .into_iter()
                .map(ToString::to_string)
                .collect(),
            enum_columns: ["state"].into_iter().map(ToString::to_string).collect(),
        },
    );

    let mut dep_schemas: HashMap<String, TableSchema> = HashMap::new();
    let mut t = test_schema();
    t.name = "public.t".to_string();
    t.columns = vec![
        col("state", DataType::UserDefined("public.status".to_string())),
        col("txt", DataType::Text),
    ];
    dep_schemas.insert("public.t".to_string(), t);

    let sql = "WITH RECURSIVE c(state, txt) AS (SELECT state, txt FROM t UNION ALL SELECT c.state, c.txt FROM c WHERE false) SELECT 1 FROM (SELECT state, txt FROM c) s WHERE s.state = 'active' AND s.txt = 'active'";
    let query = parse_stored_query(sql).expect("query should parse");
    let relation_bindings = vec!["public.t".to_string()];
    let snapshot =
        build_catalog_snapshot_for_view_bindings(42, &query, &relation_bindings, &dep_schemas)
            .expect("snapshot should build");

    let rewritten = rewrite_query_enum_literals_with_catalog(
        sql,
        &relation_bindings,
        &catalog,
        Some(&snapshot),
        "public.status",
        "active",
        "enabled",
        true,
    )
    .unwrap()
    .expect("query should be rewritten");

    assert!(rewritten.contains("s.state = 'enabled'"));
    assert!(rewritten.contains("s.txt = 'active'"));
}

#[test]
fn view_query_rewrite_handles_recursive_outer_cte_nested_join_self_reference() {
    let mut catalog = RelationEnumCatalog::default();
    catalog.insert(
        "public.t".to_string(),
        RelationColumnInfo {
            all_columns: ["state", "txt"]
                .into_iter()
                .map(ToString::to_string)
                .collect(),
            enum_columns: ["state"].into_iter().map(ToString::to_string).collect(),
        },
    );

    let mut dep_schemas: HashMap<String, TableSchema> = HashMap::new();
    let mut t = test_schema();
    t.name = "public.t".to_string();
    t.columns = vec![
        col("state", DataType::UserDefined("public.status".to_string())),
        col("txt", DataType::Text),
    ];
    dep_schemas.insert("public.t".to_string(), t);

    let sql = "WITH RECURSIVE c(state, txt) AS (SELECT state, txt FROM t UNION ALL SELECT cj.state, cj.txt FROM (c JOIN (SELECT 1) d ON true) cj WHERE false) SELECT 1 FROM (SELECT state, txt FROM c) s WHERE s.state = 'active' AND s.txt = 'active'";
    let query = parse_stored_query(sql).expect("query should parse");
    let relation_bindings = vec!["public.t".to_string()];
    let snapshot =
        build_catalog_snapshot_for_view_bindings(42, &query, &relation_bindings, &dep_schemas)
            .expect("snapshot should build");

    let rewritten = rewrite_query_enum_literals_with_catalog(
        sql,
        &relation_bindings,
        &catalog,
        Some(&snapshot),
        "public.status",
        "active",
        "enabled",
        true,
    )
    .unwrap()
    .expect("query should be rewritten");

    assert!(rewritten.contains("s.state = 'enabled'"));
    assert!(rewritten.contains("s.txt = 'active'"));
}

#[test]
fn binding_snapshot_preserves_explicit_unqualified_resolution() {
    let mut dep_schemas: HashMap<String, TableSchema> = HashMap::new();

    let mut a_v = test_schema();
    a_v.name = "a.v".to_string();
    a_v.columns = vec![col(
        "state",
        DataType::UserDefined("public.status".to_string()),
    )];
    dep_schemas.insert("a.v".to_string(), a_v);

    let mut z_v = test_schema();
    z_v.name = "z.v".to_string();
    z_v.columns = vec![col("state", DataType::Text)];
    dep_schemas.insert("z.v".to_string(), z_v);

    let query = parse_stored_query("SELECT 1 FROM a.v qa JOIN v qz ON qa.state <> qz.state")
        .expect("query should parse");
    let relation_bindings = vec!["a.v".to_string(), "z.v".to_string()];
    let snapshot =
        build_catalog_snapshot_for_view_bindings(42, &query, &relation_bindings, &dep_schemas)
            .expect("snapshot should build");

    assert!(snapshot.resolve_table("v", None).unwrap().is_some());
    assert!(snapshot.resolve_table("v", Some("a")).unwrap().is_some());
    assert!(snapshot.resolve_table("v", Some("z")).unwrap().is_some());
}
