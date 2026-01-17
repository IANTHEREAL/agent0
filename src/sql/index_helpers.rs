//! Index evaluation helpers for partial and expression indexes.

use crate::types::{IndexDef, Row, TableSchema, Value};
use anyhow::Result;

pub fn is_index_materializable(index: &IndexDef) -> bool {
    let is_btree = index
        .method
        .as_deref()
        .map(|m| m.eq_ignore_ascii_case("btree"))
        .unwrap_or(true);

    is_btree && (!index.columns.is_empty() || !index.expressions.is_empty())
}

pub fn eval_index_predicate(index: &IndexDef, schema: &TableSchema, row: &Row) -> Result<bool> {
    let Some(predicate) = &index.predicate else {
        return Ok(true);
    };

    let sql = format!("SELECT {}", predicate);
    if let Ok(stmts) = super::parse_sql(&sql) {
        if let Some(sqlparser::ast::Statement::Query(query)) = stmts.into_iter().next() {
            if let sqlparser::ast::SetExpr::Select(select) = *query.body {
                if let Some(sqlparser::ast::SelectItem::UnnamedExpr(expr)) =
                    select.projection.into_iter().next()
                {
                    let value = super::expr::eval_expr(&expr, Some(row), Some(schema))?;
                    return match value {
                        Value::Boolean(b) => Ok(b),
                        Value::Null => Ok(false),
                        _ => Ok(true),
                    };
                }
            }
        }
    }

    Ok(true)
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

    for expr_str in &index.expressions {
        let sql = format!("SELECT {}", expr_str);
        if let Ok(stmts) = super::parse_sql(&sql) {
            if let Some(sqlparser::ast::Statement::Query(query)) = stmts.into_iter().next() {
                if let sqlparser::ast::SetExpr::Select(select) = *query.body {
                    if let Some(sqlparser::ast::SelectItem::UnnamedExpr(expr)) =
                        select.projection.into_iter().next()
                    {
                        let value = super::expr::eval_expr(&expr, Some(row), Some(schema))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_index_materializable() {
        let btree_col_index = IndexDef {
            name: "idx".to_string(),
            id: 1,
            columns: vec!["a".to_string()],
            unique: false,
            method: None,
            predicate: None,
            expressions: vec![],
        };
        assert!(is_index_materializable(&btree_col_index));

        let gin_index = IndexDef {
            name: "idx".to_string(),
            id: 2,
            columns: vec!["a".to_string()],
            unique: false,
            method: Some("gin".to_string()),
            predicate: None,
            expressions: vec![],
        };
        assert!(!is_index_materializable(&gin_index));

        let partial_index = IndexDef {
            name: "idx".to_string(),
            id: 3,
            columns: vec!["a".to_string()],
            unique: false,
            method: None,
            predicate: Some("a > 0".to_string()),
            expressions: vec![],
        };
        assert!(is_index_materializable(&partial_index));

        let expr_index = IndexDef {
            name: "idx".to_string(),
            id: 4,
            columns: vec![],
            unique: false,
            method: None,
            predicate: None,
            expressions: vec!["lower(a)".to_string()],
        };
        assert!(is_index_materializable(&expr_index));
    }
}
