use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{ExecutionContext, PhysicalOperator};
use crate::sql::projection::fill_row_defaults;
use crate::types::{Row, TableSchema, Value};

const OPERATOR_BATCH_FETCH_SIZE: usize = 256;

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
            let table_name_lower = self.schema.name.to_lowercase();
            if let Some((cte_schema, cte_rows)) = ctx.cte_tables.get(&table_name_lower) {
                self.schema = cte_schema.clone();
                self.buffer = cte_rows.clone();
            } else {
                let rows = ctx
                    .store
                    .scan(ctx.txn, ctx.db_id, &self.schema.name, self.scan_limit)
                    .await?;
                self.buffer = rows
                    .into_iter()
                    .map(|r| fill_row_defaults_scan(r, &self.schema))
                    .collect::<Result<Vec<_>>>()?;
            }
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
    scan_limit: Option<usize>,
    pk_queue: Vec<Vec<Value>>,
    row_buffer: Vec<Row>,
    position: usize,
    opened: bool,
}

impl IndexScanOperator {
    #[allow(dead_code)] // Operator framework
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
            scan_limit: None,
            pk_queue: Vec::new(),
            row_buffer: Vec::new(),
            position: 0,
            opened: false,
        }
    }

    #[allow(dead_code)] // reserved for future limit pushdown
    pub fn new_with_scan_limit(
        schema: TableSchema,
        index_id: u64,
        index_name: String,
        lookup_values: Vec<Value>,
        scan_limit: Option<usize>,
    ) -> Self {
        Self {
            schema,
            index_id,
            index_name,
            lookup_values,
            scan_limit,
            pk_queue: Vec::new(),
            row_buffer: Vec::new(),
            position: 0,
            opened: false,
        }
    }

    async fn load_next_batch(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.row_buffer.clear();
        self.position = 0;

        while self.row_buffer.is_empty() && !self.pk_queue.is_empty() {
            let batch_size = OPERATOR_BATCH_FETCH_SIZE.min(self.pk_queue.len());
            let batch_pks: Vec<Vec<Value>> = self.pk_queue.drain(..batch_size).collect();
            let rows = ctx
                .store
                .batch_get_rows(
                    ctx.txn,
                    ctx.db_id,
                    self.schema.table_id,
                    batch_pks,
                    &self.schema,
                )
                .await?;

            self.row_buffer = rows
                .into_iter()
                .map(|r| fill_row_defaults_scan(r, &self.schema))
                .collect::<Result<Vec<_>>>()?;
        }

        Ok(())
    }
}

#[async_trait]
impl PhysicalOperator for IndexScanOperator {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.pk_queue.clear();
        self.row_buffer.clear();
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
                    self.scan_limit,
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
                    self.scan_limit,
                )
                .await?
        };

        self.pk_queue = pks;
        self.load_next_batch(ctx).await?;

        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        if self.position < self.row_buffer.len() {
            let row = self.row_buffer[self.position].clone();
            self.position += 1;
            Ok(Some(row))
        } else {
            self.load_next_batch(ctx).await?;
            if self.position < self.row_buffer.len() {
                let row = self.row_buffer[self.position].clone();
                self.position += 1;
                Ok(Some(row))
            } else {
                Ok(None)
            }
        }
    }

    async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.pk_queue.clear();
        self.row_buffer.clear();
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

#[derive(Debug)]
pub struct RangeIndexScanOperator {
    schema: TableSchema,
    index_id: u64,
    index_name: String,
    prefix_values: Vec<Value>,
    range_start: Option<Value>,
    start_inclusive: bool,
    range_end: Option<Value>,
    end_inclusive: bool,
    scan_limit: Option<usize>,
    pk_queue: Vec<Vec<Value>>,
    row_buffer: Vec<Row>,
    position: usize,
    opened: bool,
}

impl RangeIndexScanOperator {
    pub fn new(
        schema: TableSchema,
        index_id: u64,
        index_name: String,
        prefix_values: Vec<Value>,
        range_start: Option<Value>,
        start_inclusive: bool,
        range_end: Option<Value>,
        end_inclusive: bool,
    ) -> Self {
        Self {
            schema,
            index_id,
            index_name,
            prefix_values,
            range_start,
            start_inclusive,
            range_end,
            end_inclusive,
            scan_limit: None,
            pk_queue: Vec::new(),
            row_buffer: Vec::new(),
            position: 0,
            opened: false,
        }
    }

    #[allow(dead_code)] // reserved for future limit pushdown
    pub fn new_with_scan_limit(
        schema: TableSchema,
        index_id: u64,
        index_name: String,
        prefix_values: Vec<Value>,
        range_start: Option<Value>,
        start_inclusive: bool,
        range_end: Option<Value>,
        end_inclusive: bool,
        scan_limit: Option<usize>,
    ) -> Self {
        Self {
            schema,
            index_id,
            index_name,
            prefix_values,
            range_start,
            start_inclusive,
            range_end,
            end_inclusive,
            scan_limit,
            pk_queue: Vec::new(),
            row_buffer: Vec::new(),
            position: 0,
            opened: false,
        }
    }

    async fn load_next_batch(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.row_buffer.clear();
        self.position = 0;

        while self.row_buffer.is_empty() && !self.pk_queue.is_empty() {
            let batch_size = OPERATOR_BATCH_FETCH_SIZE.min(self.pk_queue.len());
            let batch_pks: Vec<Vec<Value>> = self.pk_queue.drain(..batch_size).collect();
            let rows = ctx
                .store
                .batch_get_rows(
                    ctx.txn,
                    ctx.db_id,
                    self.schema.table_id,
                    batch_pks,
                    &self.schema,
                )
                .await?;

            self.row_buffer = rows
                .into_iter()
                .map(|r| fill_row_defaults_scan(r, &self.schema))
                .collect::<Result<Vec<_>>>()?;
        }

        Ok(())
    }
}

#[async_trait]
impl PhysicalOperator for RangeIndexScanOperator {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.pk_queue.clear();
        self.row_buffer.clear();
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

        let pks = ctx
            .store
            .scan_index_range(
                ctx.txn,
                ctx.db_id,
                self.schema.table_id,
                self.index_id,
                &self.prefix_values,
                self.range_start.as_ref(),
                self.start_inclusive,
                self.range_end.as_ref(),
                self.end_inclusive,
                index.unique,
                &index_column_types,
                &pk_types,
                self.scan_limit,
            )
            .await?;

        self.pk_queue = pks;
        self.load_next_batch(ctx).await?;

        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        if self.position < self.row_buffer.len() {
            let row = self.row_buffer[self.position].clone();
            self.position += 1;
            Ok(Some(row))
        } else {
            self.load_next_batch(ctx).await?;
            if self.position < self.row_buffer.len() {
                let row = self.row_buffer[self.position].clone();
                self.position += 1;
                Ok(Some(row))
            } else {
                Ok(None)
            }
        }
    }

    async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.pk_queue.clear();
        self.row_buffer.clear();
        self.opened = false;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "RangeIndexScan"
    }

    fn explain_info(&self) -> Option<String> {
        Some(format!(
            "table={}, index={}",
            self.schema.name, self.index_name
        ))
    }
}

#[derive(Debug)]
pub struct InListScanOperator {
    schema: TableSchema,
    index_id: u64,
    index_name: String,
    column_values: Vec<Vec<Value>>,
    scan_limit: Option<usize>,
    pk_queue: Vec<Vec<Value>>,
    row_buffer: Vec<Row>,
    position: usize,
    opened: bool,
}

impl InListScanOperator {
    pub fn new(
        schema: TableSchema,
        index_id: u64,
        index_name: String,
        column_values: Vec<Vec<Value>>,
    ) -> Self {
        Self {
            schema,
            index_id,
            index_name,
            column_values,
            scan_limit: None,
            pk_queue: Vec::new(),
            row_buffer: Vec::new(),
            position: 0,
            opened: false,
        }
    }

    #[allow(dead_code)] // reserved for future limit pushdown
    pub fn new_with_scan_limit(
        schema: TableSchema,
        index_id: u64,
        index_name: String,
        column_values: Vec<Vec<Value>>,
        scan_limit: Option<usize>,
    ) -> Self {
        Self {
            schema,
            index_id,
            index_name,
            column_values,
            scan_limit,
            pk_queue: Vec::new(),
            row_buffer: Vec::new(),
            position: 0,
            opened: false,
        }
    }

    async fn load_next_batch(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.row_buffer.clear();
        self.position = 0;

        while self.row_buffer.is_empty() && !self.pk_queue.is_empty() {
            let batch_size = OPERATOR_BATCH_FETCH_SIZE.min(self.pk_queue.len());
            let batch_pks: Vec<Vec<Value>> = self.pk_queue.drain(..batch_size).collect();
            let rows = ctx
                .store
                .batch_get_rows(
                    ctx.txn,
                    ctx.db_id,
                    self.schema.table_id,
                    batch_pks,
                    &self.schema,
                )
                .await?;

            self.row_buffer = rows
                .into_iter()
                .map(|r| fill_row_defaults_scan(r, &self.schema))
                .collect::<Result<Vec<_>>>()?;
        }

        Ok(())
    }
}

#[async_trait]
impl PhysicalOperator for InListScanOperator {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.pk_queue.clear();
        self.row_buffer.clear();
        self.position = 0;
        self.opened = true;

        let index = self
            .schema
            .indexes
            .iter()
            .find(|i| i.id == self.index_id)
            .ok_or_else(|| anyhow!("Index {} not found", self.index_name))?;

        let pk_types: Vec<_> = if self.schema.pk_indices.is_empty() {
            vec![crate::types::DataType::Uuid]
        } else {
            self.schema
                .pk_indices
                .iter()
                .map(|&idx| self.schema.columns[idx].data_type.clone())
                .collect()
        };

        let mut all_pks = Vec::new();
        for values in &self.column_values {
            let pks = ctx
                .store
                .scan_index(
                    ctx.txn,
                    ctx.db_id,
                    self.schema.table_id,
                    self.index_id,
                    values,
                    index.unique,
                    &pk_types,
                    None,
                )
                .await?;
            all_pks.extend(pks);
        }

        let mut deduped_pks = Vec::with_capacity(all_pks.len());
        for pk in all_pks {
            if !deduped_pks.contains(&pk) {
                deduped_pks.push(pk);
            }
        }
        if let Some(limit) = self.scan_limit {
            deduped_pks.truncate(limit);
        }

        self.pk_queue = deduped_pks;
        self.load_next_batch(ctx).await?;

        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }

        if self.position < self.row_buffer.len() {
            let row = self.row_buffer[self.position].clone();
            self.position += 1;
            Ok(Some(row))
        } else {
            self.load_next_batch(ctx).await?;
            if self.position < self.row_buffer.len() {
                let row = self.row_buffer[self.position].clone();
                self.position += 1;
                Ok(Some(row))
            } else {
                Ok(None)
            }
        }
    }

    async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.pk_queue.clear();
        self.row_buffer.clear();
        self.opened = false;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "InListScan"
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
            from_alias: None,
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
