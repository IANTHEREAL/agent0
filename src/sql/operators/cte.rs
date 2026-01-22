use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{ExecutionContext, PhysicalOperator};
use crate::types::{Row, TableSchema};

#[derive(Debug)]
pub struct CTEScanOperator {
    schema: TableSchema,
    rows: Vec<Row>,
    position: usize,
    opened: bool,
}

impl CTEScanOperator {
    pub fn new(schema: TableSchema, rows: Vec<Row>) -> Self {
        Self {
            schema,
            rows,
            position: 0,
            opened: false,
        }
    }
}

#[async_trait]
impl PhysicalOperator for CTEScanOperator {
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
        self.opened = false;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "CTEScan"
    }

    fn explain_info(&self) -> Option<String> {
        Some(format!("cte={}", self.schema.name))
    }

    fn estimated_rows(&self) -> Option<usize> {
        Some(self.rows.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnDef, DataType, Value};

    fn test_schema() -> TableSchema {
        TableSchema {
            name: "my_cte".to_string(),
            table_id: 0,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
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
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        }
    }

    #[test]
    fn test_cte_scan_creation() {
        let schema = test_schema();
        let rows = vec![
            Row::new(vec![Value::Int32(1), Value::Text("Alice".to_string())]),
            Row::new(vec![Value::Int32(2), Value::Text("Bob".to_string())]),
        ];

        let op = CTEScanOperator::new(schema, rows);

        assert_eq!(op.name(), "CTEScan");
        assert_eq!(op.estimated_rows(), Some(2));
        assert!(op.explain_info().unwrap().contains("my_cte"));
    }
}
