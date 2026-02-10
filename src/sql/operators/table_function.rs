use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{ExecutionContext, PhysicalOperator};
use crate::types::{Row, TableSchema};

/// Physical operator that streams rows from a table function (e.g., fs9).
///
/// Instead of materializing all rows into a Vec, this operator yields rows
/// one at a time from the underlying streaming decoder.
#[derive(Debug)]
pub struct TableFunctionScanOperator {
    schema: TableSchema,
    rows: Vec<Row>, // Placeholder: will be replaced by streaming decoder
    position: usize,
    opened: bool,
}

impl TableFunctionScanOperator {
    /// Create a new operator with preloaded rows (for testing / batch fallback).
    pub fn new_with_rows(schema: TableSchema, rows: Vec<Row>) -> Self {
        Self {
            schema,
            rows,
            position: 0,
            opened: false,
        }
    }
}

#[async_trait]
impl PhysicalOperator for TableFunctionScanOperator {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    async fn open(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.position = 0;
        self.opened = true;
        Ok(())
    }

    async fn next(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }
        if self.position < self.rows.len() {
            let row = self.rows[self.position].clone();
            self.position += 1;
            Ok(Some(row))
        } else {
            Ok(None)
        }
    }

    async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.rows.clear();
        self.opened = false;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "TableFunctionScan"
    }

    fn explain_info(&self) -> Option<String> {
        Some("function=fs9".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnDef, DataType, Value};

    fn test_schema() -> TableSchema {
        TableSchema {
            name: "fs9_result".to_string(),
            table_id: 0,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        }
    }

    fn test_rows(n: usize) -> Vec<Row> {
        (0..n)
            .map(|i| {
                Row::new(vec![
                    Value::Int32(i as i32),
                    Value::Text(format!("row_{i}")),
                ])
            })
            .collect()
    }

    #[test]
    fn test_operator_schema_matches() {
        let schema = test_schema();
        let op = TableFunctionScanOperator::new_with_rows(schema.clone(), vec![]);
        assert_eq!(op.schema().name, "fs9_result");
        assert_eq!(op.schema().columns.len(), 2);
        assert_eq!(op.schema().columns[0].name, "id");
        assert_eq!(op.schema().columns[1].name, "name");
    }

    #[test]
    fn test_operator_name_and_explain() {
        let op = TableFunctionScanOperator::new_with_rows(test_schema(), vec![]);
        assert_eq!(op.name(), "TableFunctionScan");
        let info = op.explain_info().expect("explain_info should return Some");
        assert!(info.contains("fs9"), "explain_info should mention fs9");
    }

    #[test]
    fn test_operator_initial_state() {
        let rows = test_rows(3);
        let op = TableFunctionScanOperator::new_with_rows(test_schema(), rows);
        assert!(!op.opened);
        assert_eq!(op.position, 0);
        assert_eq!(op.rows.len(), 3);
    }

    // Tests below require ExecutionContext (TiKV Transaction).
    // They are written as stubs that document expected behavior for Task 6.

    #[tokio::test]
    async fn test_operator_yields_rows_one_at_a_time() {
        // TODO: requires ExecutionContext mock — enable after Task 6
        // Should: create op with 5 rows, open(), call next() 5 times,
        // verify each row matches, 6th call returns None.
        todo!("requires ExecutionContext to call open()/next()");
    }

    #[tokio::test]
    async fn test_operator_returns_none_after_exhausted() {
        // TODO: requires ExecutionContext mock — enable after Task 6
        // Should: after all rows consumed, multiple next() calls return None.
        todo!("requires ExecutionContext to call open()/next()");
    }

    #[tokio::test]
    async fn test_operator_not_opened_error() {
        // TODO: requires ExecutionContext mock — enable after Task 6
        // Should: call next() without open(), expect Err.
        todo!("requires ExecutionContext to call next()");
    }

    #[tokio::test]
    async fn test_operator_close_clears_state() {
        // TODO: requires ExecutionContext mock — enable after Task 6
        // Should: after close(), rows is empty and opened is false.
        todo!("requires ExecutionContext to call open()/close()");
    }
}
