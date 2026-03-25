//! Shared wildcard expansion logic for JOINs.
//!
//! This module exists to keep pgwire `Describe` inference and executor wildcard projection
//! aligned, especially for chained NATURAL/USING joins where merged columns must be
//! de-duplicated and ordered like Postgres.

use std::collections::HashSet;

use sqlparser::ast::{JoinConstraint, JoinOperator, Select, TableWithJoins};

use crate::model::{DataType, TableSchema};

use super::names::normalize_ident;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JoinWildcardColumn {
    pub(crate) name: String,
    pub(crate) source_idx: usize,
    pub(crate) col_idx: usize,
    pub(crate) data_type: DataType,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JoinWildcardPlan {
    pub(crate) columns: Vec<JoinWildcardColumn>,
    pub(crate) any_merge: bool,
}

fn is_internal_column_name(name: &str) -> bool {
    name.rsplit('.')
        .next()
        .unwrap_or(name)
        .starts_with("__db9_subquery_")
}

fn build_plan_for_table_with_joins(
    twj: &TableWithJoins,
    start_idx: usize,
    sources: &[&TableSchema],
) -> Option<(Vec<JoinWildcardColumn>, bool)> {
    let base_schema = sources.get(start_idx)?;
    let mut out: Vec<JoinWildcardColumn> = base_schema
        .columns
        .iter()
        .enumerate()
        .filter_map(|(col_idx, col)| {
            if is_internal_column_name(&col.name) {
                return None;
            }
            Some(JoinWildcardColumn {
                name: col.name.clone(),
                source_idx: start_idx,
                col_idx,
                data_type: col.data_type.clone(),
            })
        })
        .collect();

    let mut any_merge = false;

    for (join_idx, join) in twj.joins.iter().enumerate() {
        let right_idx = start_idx.saturating_add(1 + join_idx);
        let right_schema = sources.get(right_idx)?;

        let join_cols: Vec<String> = match &join.join_operator {
            JoinOperator::Inner(JoinConstraint::Natural)
            | JoinOperator::LeftOuter(JoinConstraint::Natural)
            | JoinOperator::RightOuter(JoinConstraint::Natural)
            | JoinOperator::FullOuter(JoinConstraint::Natural) => {
                let right_set: HashSet<&str> = right_schema
                    .columns
                    .iter()
                    .filter(|c| !is_internal_column_name(&c.name))
                    .map(|c| c.name.as_str())
                    .collect();
                let mut seen: HashSet<String> = HashSet::new();
                out.iter()
                    .filter_map(|c| {
                        if right_set.contains(c.name.as_str()) && seen.insert(c.name.clone()) {
                            Some(c.name.clone())
                        } else {
                            None
                        }
                    })
                    .collect()
            }
            JoinOperator::Inner(JoinConstraint::Using(cols))
            | JoinOperator::LeftOuter(JoinConstraint::Using(cols))
            | JoinOperator::RightOuter(JoinConstraint::Using(cols))
            | JoinOperator::FullOuter(JoinConstraint::Using(cols)) => {
                let mut dedup: HashSet<String> = HashSet::new();
                cols.iter()
                    .map(normalize_ident)
                    .filter(|c| !is_internal_column_name(c))
                    .filter(|c| dedup.insert(c.clone()))
                    .collect()
            }
            _ => Vec::new(),
        };

        if join_cols.is_empty() {
            out.extend(
                right_schema
                    .columns
                    .iter()
                    .enumerate()
                    .filter_map(|(col_idx, col)| {
                        if is_internal_column_name(&col.name) {
                            return None;
                        }
                        Some(JoinWildcardColumn {
                            name: col.name.clone(),
                            source_idx: right_idx,
                            col_idx,
                            data_type: col.data_type.clone(),
                        })
                    }),
            );
            continue;
        }

        any_merge = true;
        let join_set: HashSet<&str> = join_cols.iter().map(|c| c.as_str()).collect();

        let mut merged: Vec<JoinWildcardColumn> = Vec::new();
        for col_name in &join_cols {
            if let Some(left_col) = out.iter().find(|c| c.name == *col_name) {
                merged.push(left_col.clone());
            }
        }

        merged.extend(
            out.into_iter()
                .filter(|c| !join_set.contains(c.name.as_str())),
        );
        merged.extend(
            right_schema
                .columns
                .iter()
                .enumerate()
                .filter_map(|(col_idx, col)| {
                    if is_internal_column_name(&col.name) {
                        return None;
                    }
                    if join_set.contains(col.name.as_str()) {
                        None
                    } else {
                        Some(JoinWildcardColumn {
                            name: col.name.clone(),
                            source_idx: right_idx,
                            col_idx,
                            data_type: col.data_type.clone(),
                        })
                    }
                }),
        );

        out = merged;
    }

    Some((out, any_merge))
}

/// Builds the wildcard output plan for `SELECT *` when NATURAL/USING joins are present.
///
/// The `sources` slice must correspond 1:1 with `select.from` flattened as:
/// for each `TableWithJoins`: `[relation, join_0.relation, join_1.relation, ...]`.
pub(crate) fn build_join_wildcard_plan(
    select: &Select,
    sources: &[&TableSchema],
) -> Option<JoinWildcardPlan> {
    if select.from.is_empty() {
        return Some(JoinWildcardPlan {
            columns: Vec::new(),
            any_merge: false,
        });
    }

    let mut from_source_starts = Vec::with_capacity(select.from.len());
    let mut next_idx = 0usize;
    for twj in &select.from {
        from_source_starts.push(next_idx);
        next_idx = next_idx.saturating_add(1 + twj.joins.len());
    }
    if next_idx != sources.len() {
        return None;
    }

    let mut all_columns: Vec<JoinWildcardColumn> = Vec::new();
    let mut any_merge = false;

    for (from_idx, twj) in select.from.iter().enumerate() {
        let start_idx = from_source_starts[from_idx];
        let (cols, merged) = build_plan_for_table_with_joins(twj, start_idx, sources)?;
        all_columns.extend(cols);
        any_merge |= merged;
    }

    Some(JoinWildcardPlan {
        columns: all_columns,
        any_merge,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use sqlparser::ast::{SetExpr, Statement};

    use crate::model::ColumnDef;

    fn test_column(name: &str, data_type: DataType) -> ColumnDef {
        ColumnDef::new(name, data_type, false)
    }

    fn test_schema(name: &str, columns: Vec<ColumnDef>) -> TableSchema {
        TableSchema::virtual_table(name, columns)
    }

    fn parse_select(sql: &str) -> sqlparser::ast::Select {
        let stmts = crate::sql::parse_sql(sql).expect("parse");
        let stmt = stmts.first().expect("stmt");
        let Statement::Query(query) = stmt else {
            panic!("expected query");
        };
        let SetExpr::Select(select) = query.body.as_ref() else {
            panic!("expected select");
        };
        (**select).clone()
    }

    #[test]
    fn join_wildcard_plan_multiway_natural_join_dedups_common_columns() {
        let select = parse_select("SELECT * FROM a NATURAL JOIN b NATURAL JOIN c");

        let schema_a = test_schema(
            "a",
            vec![
                test_column("id", DataType::Int32),
                test_column("a1", DataType::Text),
            ],
        );
        let schema_b = test_schema(
            "b",
            vec![
                test_column("id", DataType::Int32),
                test_column("b1", DataType::Text),
            ],
        );
        let schema_c = test_schema(
            "c",
            vec![
                test_column("id", DataType::Int32),
                test_column("c1", DataType::Text),
            ],
        );

        let sources: Vec<&TableSchema> = vec![&schema_a, &schema_b, &schema_c];
        let plan = build_join_wildcard_plan(&select, &sources).expect("plan");
        assert!(plan.any_merge);

        let names: Vec<String> = plan.columns.into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["id", "a1", "b1", "c1"]);
    }

    #[test]
    fn join_wildcard_plan_chained_using_join_reorders_and_dedups_across_chain() {
        let select = parse_select("SELECT * FROM a JOIN b USING (id) JOIN c USING (foo)");

        let schema_a = test_schema(
            "a",
            vec![
                test_column("name", DataType::Text),
                test_column("id", DataType::Int32),
                test_column("foo", DataType::Text),
                test_column("a1", DataType::Text),
            ],
        );
        let schema_b = test_schema(
            "b",
            vec![
                test_column("id", DataType::Int32),
                test_column("b1", DataType::Text),
            ],
        );
        let schema_c = test_schema(
            "c",
            vec![
                test_column("foo", DataType::Text),
                test_column("c1", DataType::Text),
            ],
        );

        let sources: Vec<&TableSchema> = vec![&schema_a, &schema_b, &schema_c];
        let plan = build_join_wildcard_plan(&select, &sources).expect("plan");
        assert!(plan.any_merge);

        let names: Vec<String> = plan.columns.into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["foo", "id", "name", "a1", "b1", "c1"]);
    }

    #[test]
    fn join_wildcard_plan_does_not_drop_same_named_columns_from_unjoined_tables() {
        let select = parse_select("SELECT * FROM a JOIN b USING (id) JOIN c USING (foo)");

        let schema_a = test_schema(
            "a",
            vec![
                test_column("id", DataType::Int32),
                test_column("foo", DataType::Text),
                test_column("a1", DataType::Text),
            ],
        );
        let schema_b = test_schema(
            "b",
            vec![
                test_column("id", DataType::Int32),
                test_column("b1", DataType::Text),
            ],
        );
        let schema_c = test_schema(
            "c",
            vec![
                test_column("foo", DataType::Text),
                test_column("id", DataType::Int32),
                test_column("c1", DataType::Text),
            ],
        );

        let sources: Vec<&TableSchema> = vec![&schema_a, &schema_b, &schema_c];
        let plan = build_join_wildcard_plan(&select, &sources).expect("plan");
        assert!(plan.any_merge);

        let names: Vec<String> = plan.columns.into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["foo", "id", "a1", "b1", "id", "c1"]);
    }
}
