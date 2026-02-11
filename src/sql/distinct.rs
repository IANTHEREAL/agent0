//! DISTINCT, deduplication, and offset/limit/fetch helpers
//!
//! This module contains functions for handling DISTINCT operations,
//! row deduplication, and OFFSET/LIMIT/FETCH clause processing.

use std::collections::HashSet;

use anyhow::Result;
#[cfg(test)]
use sqlparser::ast::Expr;
use sqlparser::ast::Query;

use super::expr::eval_expr;
use super::value_key::serialize_values_for_key;
#[cfg(test)]
use crate::types::TableSchema;
use crate::types::{Row, Value};

/// Deduplicate rows based on their serialized values
pub fn dedup_rows(rows: Vec<Row>) -> Result<Vec<Row>> {
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut result = Vec::new();
    for row in rows {
        let key = serialize_values_for_key(&row.values)?;
        if seen.insert(key) {
            result.push(row);
        }
    }
    Ok(result)
}

#[cfg(test)]
pub fn distinct_on_rows_join_with_indices(
    rows: Vec<Row>,
    on_exprs: &[Expr],
    column_offsets: &std::collections::HashMap<String, usize>,
    combined_schema: &TableSchema,
    merged_column_offsets: Option<&std::collections::HashMap<String, Vec<usize>>>,
) -> Result<(Vec<Row>, Vec<usize>)> {
    use super::expr::{eval_join_expr, JoinEvalContext};
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut result = Vec::new();
    let mut indices = Vec::new();

    for (idx, row) in rows.into_iter().enumerate() {
        let ctx =
            JoinEvalContext::new(column_offsets, merged_column_offsets, &row, combined_schema);
        let key_values: Vec<Value> = on_exprs
            .iter()
            .map(|expr| eval_join_expr(&ctx, expr))
            .collect::<Result<Vec<_>>>()?;
        let key = serialize_values_for_key(&key_values)?;
        if seen.insert(key) {
            indices.push(idx);
            result.push(row);
        }
    }
    Ok((result, indices))
}

pub fn apply_offset_limit_fetch(mut rows: Vec<Row>, query: &Query) -> Vec<Row> {
    let value_to_usize = |v: Value| -> Option<usize> {
        match v {
            Value::Int64(n) => usize::try_from(n).ok(),
            Value::Int32(n) => usize::try_from(n).ok(),
            Value::Text(s) => s
                .trim()
                .parse::<i64>()
                .ok()
                .and_then(|n| usize::try_from(n).ok()),
            _ => None,
        }
    };
    if let Some(offset) = &query.offset {
        if let Ok(v) = eval_expr(&offset.value, None, None) {
            let n = value_to_usize(v).unwrap_or(0);
            rows = rows.into_iter().skip(n).collect();
        }
    }
    if let Some(limit) = &query.limit {
        if let Ok(v) = eval_expr(limit, None, None) {
            let n = value_to_usize(v).unwrap_or(usize::MAX);
            rows = rows.into_iter().take(n).collect();
        }
    }

    if let Some(fetch) = &query.fetch {
        if let Some(quantity) = &fetch.quantity {
            if let Ok(v) = eval_expr(quantity, None, None) {
                let n = value_to_usize(v).unwrap_or(1);
                rows = rows.into_iter().take(n).collect();
            }
        } else {
            rows = rows.into_iter().take(1).collect();
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnDef, DataType};
    use sqlparser::dialect::PostgreSqlDialect;
    use sqlparser::parser::Parser;
    use std::collections::HashMap;

    #[test]
    fn test_dedup_rows() {
        let nan1 = f64::from_bits(0x7ff8_0000_0000_0001);
        let nan2 = f64::from_bits(0x7ff8_0000_0000_0002);

        let rows = vec![
            Row {
                values: vec![Value::Int32(1), Value::Text("a".to_string())],
            },
            Row {
                values: vec![Value::Int32(1), Value::Text("a".to_string())],
            },
            Row {
                values: vec![Value::Int32(2), Value::Text("b".to_string())],
            },
            Row {
                values: vec![Value::Float64(nan1)],
            },
            Row {
                values: vec![Value::Float64(nan2)],
            },
            Row {
                values: vec![Value::Float64(-0.0)],
            },
            Row {
                values: vec![Value::Float64(0.0)],
            },
        ];
        let result = dedup_rows(rows).unwrap();
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn test_distinct_on_rows_join_with_indices() {
        let schema = TableSchema::new(
            "t".to_string(),
            1,
            vec![ColumnDef {
                name: "a".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            vec![],
        );
        let mut offsets = HashMap::new();
        offsets.insert("a".to_string(), 0);

        let rows = vec![
            Row::new(vec![Value::Int32(1)]),
            Row::new(vec![Value::Int32(1)]),
            Row::new(vec![Value::Int32(2)]),
        ];
        let on_exprs = vec![sqlparser::ast::Expr::Identifier(
            sqlparser::ast::Ident::new("a"),
        )];
        let (result, indices) =
            distinct_on_rows_join_with_indices(rows, &on_exprs, &offsets, &schema, None).unwrap();
        assert_eq!(indices, vec![0, 2]);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_distinct_on_rows_join_with_indices_propagates_eval_error() {
        let schema = TableSchema::new(
            "t".to_string(),
            1,
            vec![ColumnDef {
                name: "a".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
            }],
            vec![],
        );
        let mut offsets = HashMap::new();
        offsets.insert("a".to_string(), 0);

        let rows = vec![
            Row::new(vec![Value::Int32(1)]),
            Row::new(vec![Value::Int32(2)]),
        ];
        let on_exprs = vec![sqlparser::ast::Expr::Identifier(
            sqlparser::ast::Ident::new("missing_col"),
        )];

        let result = distinct_on_rows_join_with_indices(rows, &on_exprs, &offsets, &schema, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_offset_limit_fetch() {
        let dialect = PostgreSqlDialect {};
        let sql = "SELECT 1 LIMIT 2 OFFSET 1";
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        let query = match &statements[0] {
            sqlparser::ast::Statement::Query(q) => q.as_ref(),
            _ => panic!("expected query"),
        };

        let rows = vec![
            Row::new(vec![Value::Int32(10)]),
            Row::new(vec![Value::Int32(11)]),
            Row::new(vec![Value::Int32(12)]),
        ];
        let result = apply_offset_limit_fetch(rows, query);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].values[0], Value::Int32(11));
        assert_eq!(result[1].values[0], Value::Int32(12));
    }

    #[test]
    fn test_apply_offset_limit_fetch_with_quoted_numeric_literals() {
        let dialect = PostgreSqlDialect {};
        let rows = vec![
            Row::new(vec![Value::Int32(10)]),
            Row::new(vec![Value::Int32(11)]),
            Row::new(vec![Value::Int32(12)]),
        ];

        let statements = Parser::parse_sql(&dialect, "SELECT 1 LIMIT '2' OFFSET '1'").unwrap();
        let query = match &statements[0] {
            sqlparser::ast::Statement::Query(q) => q.as_ref(),
            _ => panic!("expected query"),
        };
        let result = apply_offset_limit_fetch(rows.clone(), query);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].values[0], Value::Int32(11));
        assert_eq!(result[1].values[0], Value::Int32(12));

        let statements = Parser::parse_sql(&dialect, "SELECT 1 FETCH FIRST '2' ROWS ONLY").unwrap();
        let query = match &statements[0] {
            sqlparser::ast::Statement::Query(q) => q.as_ref(),
            _ => panic!("expected query"),
        };
        let result = apply_offset_limit_fetch(rows, query);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].values[0], Value::Int32(10));
        assert_eq!(result[1].values[0], Value::Int32(11));
    }
}
