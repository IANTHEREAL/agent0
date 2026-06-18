//! Index evaluation helpers for partial and expression indexes.

use crate::model::{DataType, IndexDef, Row, TableSchema, Value};
use crate::sql::query_context::QueryContext;
use anyhow::Result;
use sqlparser::ast::Expr;

pub fn is_index_materializable(index: &IndexDef) -> bool {
    let is_btree = index
        .method
        .as_deref()
        .map(|m| m.eq_ignore_ascii_case("btree"))
        .unwrap_or(true);

    is_btree && (!index.columns.is_empty() || !index.expressions.is_empty())
}

/// Validate that a WHERE predicate expression for a partial index type-checks
/// and produces a boolean result. PostgreSQL performs this validation at DDL time.
pub fn validate_index_predicate(predicate: &Expr, schema: &TableSchema) -> Result<()> {
    let table_name = schema.name.rsplit('.').next().unwrap_or(&schema.name);
    let qctx = QueryContext::from_task_locals();
    let typed =
        super::expr::compile::compile_row_expr_for_table(predicate, schema, table_name, &qctx)?;

    match typed.data_type {
        DataType::Boolean => Ok(()),
        actual_type => Err(anyhow::anyhow!(
            "argument of WHERE must be type boolean, not type {}",
            actual_type
        )),
    }
}

pub fn eval_index_predicate(index: &IndexDef, schema: &TableSchema, row: &Row) -> Result<bool> {
    let Some(predicate) = &index.predicate else {
        return Ok(true);
    };

    let sql = format!("SELECT {}", predicate);
    let stmts = super::parse_sql(&sql)
        .map_err(|e| anyhow::anyhow!("failed to parse index predicate '{}': {}", predicate, e))?;

    let stmt = stmts
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty parse result for index predicate '{}'", predicate))?;

    let query = match stmt {
        sqlparser::ast::Statement::Query(q) => q,
        _ => anyhow::bail!("index predicate '{}' did not parse as a query", predicate),
    };

    let select = match *query.body {
        sqlparser::ast::SetExpr::Select(s) => s,
        _ => anyhow::bail!("index predicate '{}' did not parse as a SELECT", predicate),
    };

    let expr = match select.projection.into_iter().next() {
        Some(sqlparser::ast::SelectItem::UnnamedExpr(e)) => e,
        _ => anyhow::bail!(
            "index predicate '{}' did not produce an expression",
            predicate
        ),
    };

    let table_name = schema.name.rsplit('.').next().unwrap_or(&schema.name);
    let value = super::expr::bridge::eval_ast_expr_with_row(&expr, row, schema, table_name)?;
    match value {
        Value::Boolean(b) => Ok(b),
        Value::Null => Ok(false),
        _ => anyhow::bail!(
            "index predicate '{}' evaluated to non-boolean value: {:?}",
            predicate,
            value
        ),
    }
}

pub fn get_index_values_with_expressions(
    index: &IndexDef,
    schema: &TableSchema,
    row: &Row,
) -> Result<Vec<Value>> {
    let mut values = Vec::new();

    for col_name in &index.columns {
        if let Some(idx) = schema.column_index(col_name) {
            values.push(row.values[idx].clone());
        } else {
            values.push(Value::Null);
        }
    }

    let table_name = schema.name.rsplit('.').next().unwrap_or(&schema.name);
    for expr_str in &index.expressions {
        let sql = format!("SELECT {}", expr_str);
        if let Ok(stmts) = super::parse_sql(&sql) {
            if let Some(sqlparser::ast::Statement::Query(query)) = stmts.into_iter().next() {
                if let sqlparser::ast::SetExpr::Select(select) = *query.body {
                    if let Some(sqlparser::ast::SelectItem::UnnamedExpr(expr)) =
                        select.projection.into_iter().next()
                    {
                        let value = super::expr::bridge::eval_ast_expr_with_row(
                            &expr, row, schema, table_name,
                        )?;
                        values.push(value);
                        continue;
                    }
                }
            }
        }
        values.push(Value::Null);
    }

    Ok(values)
}

/// Returns true if the index values are unchanged between old and new rows,
/// meaning the index entry does not need to be deleted and recreated.
pub fn index_values_unchanged(
    index: &IndexDef,
    schema: &TableSchema,
    old_row: &Row,
    new_row: &Row,
) -> Result<bool> {
    if is_index_materializable(index) {
        let old_pred = eval_index_predicate(index, schema, old_row)?;
        let new_pred = eval_index_predicate(index, schema, new_row)?;
        if old_pred != new_pred {
            return Ok(false);
        }

        let old_vals = get_index_values_with_expressions(index, schema, old_row)?;
        let new_vals = get_index_values_with_expressions(index, schema, new_row)?;
        Ok(old_vals == new_vals)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_col(name: &str) -> crate::model::ColumnDef {
        crate::model::ColumnDef::new(name, crate::model::DataType::Text, true)
    }

    fn test_schema(columns: Vec<crate::model::ColumnDef>) -> TableSchema {
        let mut schema = TableSchema::new("public.t".to_string(), 1, columns, vec![]);
        schema.owner = "postgres".to_string();
        schema
    }

    #[test]
    fn test_is_index_materializable() {
        let btree_col_index = IndexDef {
            name: "idx".to_string(),
            id: 1,
            columns: vec!["a".to_string()],
            unique: false,
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: vec![],
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
            deferrable: false,
            initially_deferred: false,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        };
        assert!(is_index_materializable(&btree_col_index));

        let gin_index = IndexDef {
            name: "idx".to_string(),
            id: 2,
            columns: vec!["a".to_string()],
            unique: false,
            is_constraint: false,
            method: Some("gin".to_string()),
            predicate: None,
            expressions: vec![],
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
            deferrable: false,
            initially_deferred: false,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        };
        assert!(!is_index_materializable(&gin_index));

        let partial_index = IndexDef {
            name: "idx".to_string(),
            id: 3,
            columns: vec!["a".to_string()],
            unique: false,
            is_constraint: false,
            method: None,
            predicate: Some("a > 0".to_string()),
            expressions: vec![],
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
            deferrable: false,
            initially_deferred: false,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        };
        assert!(is_index_materializable(&partial_index));

        let expr_index = IndexDef {
            name: "idx".to_string(),
            id: 4,
            columns: vec![],
            unique: false,
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: vec!["lower(a)".to_string()],
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
            deferrable: false,
            initially_deferred: false,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        };
        assert!(is_index_materializable(&expr_index));
    }

    #[test]
    fn test_index_values_unchanged_simple_column() {
        let index = IndexDef {
            name: "idx_name".to_string(),
            id: 1,
            columns: vec!["name".to_string()],
            unique: false,
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: vec![],
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
            deferrable: false,
            initially_deferred: false,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        };
        let schema = test_schema(vec![test_col("name")]);
        let old_row = Row::new(vec![Value::Text("Alice".to_string())]);
        let new_row = Row::new(vec![Value::Text("Alice".to_string())]);

        assert!(index_values_unchanged(&index, &schema, &old_row, &new_row).unwrap());
    }

    #[test]
    fn test_index_values_changed_simple_column() {
        let index = IndexDef {
            name: "idx_name".to_string(),
            id: 1,
            columns: vec!["name".to_string()],
            unique: false,
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: vec![],
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
            deferrable: false,
            initially_deferred: false,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        };
        let schema = test_schema(vec![test_col("name")]);
        let old_row = Row::new(vec![Value::Text("Alice".to_string())]);
        let new_row = Row::new(vec![Value::Text("Bob".to_string())]);

        assert!(!index_values_unchanged(&index, &schema, &old_row, &new_row).unwrap());
    }

    #[test]
    fn test_index_values_unchanged_expression() {
        let index = IndexDef {
            name: "idx_lower_name".to_string(),
            id: 1,
            columns: vec![],
            unique: false,
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: vec!["lower(name)".to_string()],
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
            deferrable: false,
            initially_deferred: false,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        };
        let schema = test_schema(vec![test_col("name")]);
        let old_row = Row::new(vec![Value::Text("Alice".to_string())]);
        let new_row = Row::new(vec![Value::Text("Alice".to_string())]);

        assert!(index_values_unchanged(&index, &schema, &old_row, &new_row).unwrap());
    }

    #[test]
    fn test_index_values_unchanged_non_indexed_column_changed() {
        let index = IndexDef {
            name: "idx_name".to_string(),
            id: 1,
            columns: vec!["name".to_string()],
            unique: false,
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: vec![],
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
            deferrable: false,
            initially_deferred: false,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        };
        let schema = test_schema(vec![test_col("id"), test_col("name"), test_col("bio")]);
        let old_row = Row::new(vec![
            Value::Int32(1),
            Value::Text("Alice".to_string()),
            Value::Text("old bio".to_string()),
        ]);
        let new_row = Row::new(vec![
            Value::Int32(1),
            Value::Text("Alice".to_string()),
            Value::Text("new bio".to_string()),
        ]);

        assert!(index_values_unchanged(&index, &schema, &old_row, &new_row).unwrap());
    }

    #[test]
    fn test_index_values_unchanged_unique_safe() {
        let index = IndexDef {
            name: "idx_email_key".to_string(),
            id: 1,
            columns: vec!["email".to_string()],
            unique: true,
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: vec![],
            state: crate::worker::types::IndexState::Ready,
            cached_predicate_conjuncts: None,
            deferrable: false,
            initially_deferred: false,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        };
        let schema = test_schema(vec![test_col("email")]);
        let old_row = Row::new(vec![Value::Text("a@b.com".to_string())]);
        let new_row = Row::new(vec![Value::Text("a@b.com".to_string())]);

        assert!(index_values_unchanged(&index, &schema, &old_row, &new_row).unwrap());
    }

    fn parse_expr(sql_fragment: &str) -> Expr {
        let sql = format!("SELECT {}", sql_fragment);
        let stmts = super::super::parse_sql(&sql).unwrap();
        let stmt = stmts.into_iter().next().unwrap();
        match stmt {
            sqlparser::ast::Statement::Query(q) => match *q.body {
                sqlparser::ast::SetExpr::Select(s) => match s.projection.into_iter().next() {
                    Some(sqlparser::ast::SelectItem::UnnamedExpr(e)) => e,
                    other => panic!("unexpected projection: {:?}", other),
                },
                other => panic!("unexpected set expr: {:?}", other),
            },
            other => panic!("unexpected statement: {:?}", other),
        }
    }

    fn test_col_typed(name: &str, data_type: crate::model::DataType) -> crate::model::ColumnDef {
        crate::model::ColumnDef::new(name, data_type, true)
    }

    #[test]
    fn test_validate_index_predicate_accepts_boolean_expr() {
        let schema = test_schema(vec![test_col_typed(
            "active",
            crate::model::DataType::Boolean,
        )]);
        let expr = parse_expr("active");
        assert!(validate_index_predicate(&expr, &schema).is_ok());
    }

    #[test]
    fn test_validate_index_predicate_accepts_comparison() {
        let schema = test_schema(vec![test_col_typed("age", crate::model::DataType::Int32)]);
        let expr = parse_expr("age > 0");
        assert!(validate_index_predicate(&expr, &schema).is_ok());
    }

    #[test]
    fn test_validate_index_predicate_rejects_non_boolean() {
        let schema = test_schema(vec![test_col_typed("name", crate::model::DataType::Text)]);
        let expr = parse_expr("name");
        let err = validate_index_predicate(&expr, &schema).unwrap_err();
        assert!(
            err.to_string()
                .contains("argument of WHERE must be type boolean"),
            "expected boolean type error, got: {}",
            err
        );
    }

    #[test]
    fn test_validate_index_predicate_rejects_integer_expr() {
        let schema = test_schema(vec![test_col_typed("x", crate::model::DataType::Int32)]);
        let expr = parse_expr("x + 1");
        let err = validate_index_predicate(&expr, &schema).unwrap_err();
        assert!(
            err.to_string()
                .contains("argument of WHERE must be type boolean"),
            "expected boolean type error, got: {}",
            err
        );
    }
}
