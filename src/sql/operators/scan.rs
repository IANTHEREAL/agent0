use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{ExecutionContext, PhysicalOperator};
use crate::sql::helpers::fill_row_defaults;
use crate::types::{Row, TableSchema, Value};

#[derive(Debug)]
pub struct TableScanOperator {
    schema: TableSchema,
    scan_limit: Option<usize>,
    buffer: Vec<Row>,
    position: usize,
    opened: bool,
    preloaded: bool,
}

impl TableScanOperator {
    pub fn new(schema: TableSchema) -> Self {
        Self {
            schema,
            scan_limit: None,
            buffer: Vec::new(),
            position: 0,
            opened: false,
            preloaded: false,
        }
    }

    pub fn new_with_scan_limit(schema: TableSchema, scan_limit: Option<usize>) -> Self {
        Self {
            schema,
            scan_limit,
            buffer: Vec::new(),
            position: 0,
            opened: false,
            preloaded: false,
        }
    }

    #[allow(dead_code)]
    pub fn new_with_rows(schema: TableSchema, rows: Vec<Row>) -> Self {
        Self {
            schema,
            scan_limit: None,
            buffer: rows,
            position: 0,
            opened: false,
            preloaded: true,
        }
    }
}

fn fill_row_defaults_scan(mut row: Row, schema: &TableSchema) -> Result<Row> {
    fill_row_defaults(&mut row, schema)?;
    while row.values.len() < schema.columns.len() {
        row.values.push(Value::Null);
    }
    Ok(row)
}

#[async_trait]
impl PhysicalOperator for TableScanOperator {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.position = 0;
        self.opened = true;

        if !self.preloaded {
            self.buffer.clear();
            let rows = ctx
                .store
                .scan(ctx.txn, ctx.db_id, &self.schema.name, self.scan_limit)
                .await?;
            self.buffer = rows
                .into_iter()
                .map(|r| fill_row_defaults_scan(r, &self.schema))
                .collect::<Result<Vec<_>>>()?;
        }

        Ok(())
    }

    async fn next(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        if self.position < self.buffer.len() {
            let row = self.buffer[self.position].clone();
            self.position += 1;
            Ok(Some(row))
        } else {
            Ok(None)
        }
    }

    async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.buffer.clear();
        self.opened = false;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "TableScan"
    }

    fn explain_info(&self) -> Option<String> {
        Some(format!("table={}", self.schema.name))
    }

    fn estimated_rows(&self) -> Option<usize> {
        None
    }
}

#[derive(Debug)]
pub struct IndexScanOperator {
    schema: TableSchema,
    index_id: u64,
    index_name: String,
    lookup_values: Vec<Value>,
    buffer: Vec<Row>,
    position: usize,
    opened: bool,
}

impl IndexScanOperator {
    pub fn new(
        schema: TableSchema,
        index_id: u64,
        index_name: String,
        lookup_values: Vec<Value>,
    ) -> Self {
        Self {
            schema,
            index_id,
            index_name,
            lookup_values,
            buffer: Vec::new(),
            position: 0,
            opened: false,
        }
    }
}

#[async_trait]
impl PhysicalOperator for IndexScanOperator {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.buffer.clear();
        self.position = 0;
        self.opened = true;

        let index = self
            .schema
            .indexes
            .iter()
            .find(|i| i.id == self.index_id)
            .ok_or_else(|| anyhow!("Index {} not found", self.index_name))?;

        let index_column_types: Vec<_> = index
            .columns
            .iter()
            .map(|col| {
                self.schema
                    .columns
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(col))
                    .map(|c| c.data_type.clone())
                    .ok_or_else(|| anyhow!("Index column '{}' not found", col))
            })
            .collect::<Result<Vec<_>>>()?;

        let pk_types: Vec<_> = if self.schema.pk_indices.is_empty() {
            vec![crate::types::DataType::Uuid]
        } else {
            self.schema
                .pk_indices
                .iter()
                .map(|&idx| self.schema.columns[idx].data_type.clone())
                .collect()
        };

        let pks = if self.lookup_values.len() < index.columns.len() {
            ctx.store
                .scan_index_prefix(
                    ctx.txn,
                    ctx.db_id,
                    self.schema.table_id,
                    self.index_id,
                    &self.lookup_values,
                    index.unique,
                    &index_column_types,
                    &pk_types,
                )
                .await?
        } else {
            ctx.store
                .scan_index(
                    ctx.txn,
                    ctx.db_id,
                    self.schema.table_id,
                    self.index_id,
                    &self.lookup_values,
                    index.unique,
                    &pk_types,
                )
                .await?
        };

        let rows = ctx
            .store
            .batch_get_rows(ctx.txn, ctx.db_id, self.schema.table_id, pks, &self.schema)
            .await?;

        self.buffer = rows
            .into_iter()
            .map(|r| fill_row_defaults_scan(r, &self.schema))
            .collect::<Result<Vec<_>>>()?;

        Ok(())
    }

    async fn next(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        if self.position < self.buffer.len() {
            let row = self.buffer[self.position].clone();
            self.position += 1;
            Ok(Some(row))
        } else {
            Ok(None)
        }
    }

    async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.buffer.clear();
        self.opened = false;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "IndexScan"
    }

    fn explain_info(&self) -> Option<String> {
        Some(format!(
            "table={}, index={}",
            self.schema.name, self.index_name
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnDef, DataType};

    fn test_schema() -> TableSchema {
        TableSchema {
            name: "users".to_string(),
            table_id: 1,
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

    #[test]
    fn test_table_scan_creation() {
        let schema = test_schema();
        let scan = TableScanOperator::new(schema);

        assert_eq!(scan.name(), "TableScan");
        assert!(!scan.opened);
        assert!(scan.buffer.is_empty());
    }

    #[test]
    fn test_table_scan_explain_info() {
        let schema = test_schema();
        let scan = TableScanOperator::new(schema);

        assert_eq!(scan.explain_info(), Some("table=users".to_string()));
    }

    #[test]
    fn test_fill_row_defaults_scan() {
        let schema = test_schema();
        let row = Row::new(vec![Value::Int32(1)]);

        let filled = fill_row_defaults_scan(row, &schema).unwrap();

        assert_eq!(filled.values.len(), 2);
        assert_eq!(filled.values[0], Value::Int32(1));
        assert_eq!(filled.values[1], Value::Null);
    }
}
