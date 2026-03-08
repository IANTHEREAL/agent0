use anyhow::{anyhow, Result};
use async_trait::async_trait;

use super::{ExecutionContext, PhysicalOperator};
use crate::model::{Row, TableSchema, Value};
use crate::sql::projection::fill_row_defaults;

const OPERATOR_BATCH_FETCH_SIZE: usize = 256;

/// Fallback PK type for tables without an explicit primary key.
///
/// When `pk_indices` is empty the storage layer synthesizes a UUID-based
/// row identifier, so index scans must decode PK entries as `DataType::Uuid`.
pub(super) const IMPLICIT_PK_TYPE: crate::model::DataType = crate::model::DataType::Uuid;

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
    #[cfg(test)]
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
        if !self.preloaded {
            self.buffer.clear();
        }
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

// ── Index scan shared infrastructure ─────────────────────────────

/// Shared state and methods for all index scan operators.
///
/// Each concrete index scan variant (point, range, in-list) holds an
/// `IndexScanBase` plus variant-specific fields. The base handles the
/// common pk_queue → batch_get_rows → row_buffer pipeline.
#[derive(Debug)]
struct IndexScanBase {
    schema: TableSchema,
    index_id: u64,
    index_name: String,
    scan_limit: Option<usize>,
    pk_queue: Vec<Vec<Value>>,
    row_buffer: Vec<Row>,
    position: usize,
    opened: bool,
}

impl IndexScanBase {
    fn new(
        schema: TableSchema,
        index_id: u64,
        index_name: String,
        scan_limit: Option<usize>,
    ) -> Self {
        Self {
            schema,
            index_id,
            index_name,
            scan_limit,
            pk_queue: Vec::new(),
            row_buffer: Vec::new(),
            position: 0,
            opened: false,
        }
    }

    /// Reset state and mark as opened. Called at the start of each open().
    fn reset_and_open(&mut self) {
        self.pk_queue.clear();
        self.row_buffer.clear();
        self.position = 0;
        self.opened = true;
    }

    /// Resolve the index metadata and compute PK types from the schema.
    fn resolve_index_meta(&self) -> Result<(&crate::model::IndexDef, Vec<crate::model::DataType>)> {
        let index = self
            .schema
            .indexes
            .iter()
            .find(|i| i.id == self.index_id)
            .ok_or_else(|| anyhow!("Index {} not found", self.index_name))?;

        let pk_types: Vec<_> = if self.schema.pk_indices.is_empty() {
            vec![IMPLICIT_PK_TYPE]
        } else {
            self.schema
                .pk_indices
                .iter()
                .map(|&idx| self.schema.columns[idx].data_type.clone())
                .collect()
        };

        Ok((index, pk_types))
    }

    /// Resolve index column types (needed by scan_index_prefix and scan_index_range).
    fn resolve_index_column_types(
        &self,
        index: &crate::model::IndexDef,
    ) -> Result<Vec<crate::model::DataType>> {
        index
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
            .collect::<Result<Vec<_>>>()
    }

    /// Fetch the next batch of rows from the pk_queue via batch_get_rows.
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

    /// Return the next row from the buffer, fetching a new batch if needed.
    async fn next_row(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
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

    /// Release buffers and mark as closed.
    fn close(&mut self) {
        self.pk_queue.clear();
        self.row_buffer.clear();
        self.opened = false;
    }
}

// ── IndexScanOperator (point / prefix lookup) ────────────────────

#[derive(Debug)]
pub struct IndexScanOperator {
    base: IndexScanBase,
    lookup_values: Vec<Value>,
}

impl IndexScanOperator {
    pub fn new_with_scan_limit(
        schema: TableSchema,
        index_id: u64,
        index_name: String,
        lookup_values: Vec<Value>,
        scan_limit: Option<usize>,
    ) -> Self {
        Self {
            base: IndexScanBase::new(schema, index_id, index_name, scan_limit),
            lookup_values,
        }
    }
}

#[async_trait]
impl PhysicalOperator for IndexScanOperator {
    fn schema(&self) -> &TableSchema {
        &self.base.schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.base.reset_and_open();

        let (index, pk_types) = self.base.resolve_index_meta()?;
        let index_column_types = self.base.resolve_index_column_types(index)?;

        let pks = if self.lookup_values.len() < index.columns.len() {
            ctx.store
                .scan_index_prefix(
                    ctx.txn,
                    ctx.db_id,
                    self.base.schema.table_id,
                    self.base.index_id,
                    &self.lookup_values,
                    index.unique,
                    &index_column_types,
                    &pk_types,
                    self.base.scan_limit,
                )
                .await?
        } else {
            ctx.store
                .scan_index(
                    ctx.txn,
                    ctx.db_id,
                    self.base.schema.table_id,
                    self.base.index_id,
                    &self.lookup_values,
                    index.unique,
                    &pk_types,
                    self.base.scan_limit,
                )
                .await?
        };

        self.base.pk_queue = pks;
        self.base.load_next_batch(ctx).await?;

        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        self.base.next_row(ctx).await
    }

    async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.base.close();
        Ok(())
    }

    fn name(&self) -> &'static str {
        "IndexScan"
    }

    fn explain_info(&self) -> Option<String> {
        Some(format!(
            "table={}, index={}",
            self.base.schema.name, self.base.index_name
        ))
    }
}

// ── RangeIndexScanOperator ───────────────────────────────────────

#[derive(Debug)]
pub struct RangeIndexScanOperator {
    base: IndexScanBase,
    prefix_values: Vec<Value>,
    range_start: Option<Value>,
    start_inclusive: bool,
    range_end: Option<Value>,
    end_inclusive: bool,
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
            base: IndexScanBase::new(schema, index_id, index_name, None),
            prefix_values,
            range_start,
            start_inclusive,
            range_end,
            end_inclusive,
        }
    }
}

#[async_trait]
impl PhysicalOperator for RangeIndexScanOperator {
    fn schema(&self) -> &TableSchema {
        &self.base.schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.base.reset_and_open();

        let (index, pk_types) = self.base.resolve_index_meta()?;
        let index_column_types = self.base.resolve_index_column_types(index)?;

        let pks = ctx
            .store
            .scan_index_range(
                ctx.txn,
                ctx.db_id,
                self.base.schema.table_id,
                self.base.index_id,
                &self.prefix_values,
                self.range_start.as_ref(),
                self.start_inclusive,
                self.range_end.as_ref(),
                self.end_inclusive,
                index.unique,
                &index_column_types,
                &pk_types,
                self.base.scan_limit,
            )
            .await?;

        self.base.pk_queue = pks;
        self.base.load_next_batch(ctx).await?;

        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        self.base.next_row(ctx).await
    }

    async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.base.close();
        Ok(())
    }

    fn name(&self) -> &'static str {
        "RangeIndexScan"
    }

    fn explain_info(&self) -> Option<String> {
        Some(format!(
            "table={}, index={}",
            self.base.schema.name, self.base.index_name
        ))
    }
}

// ── InListScanOperator ───────────────────────────────────────────

#[derive(Debug)]
pub struct InListScanOperator {
    base: IndexScanBase,
    column_values: Vec<Vec<Value>>,
}

impl InListScanOperator {
    pub fn new(
        schema: TableSchema,
        index_id: u64,
        index_name: String,
        column_values: Vec<Vec<Value>>,
    ) -> Self {
        Self {
            base: IndexScanBase::new(schema, index_id, index_name, None),
            column_values,
        }
    }
}

#[async_trait]
impl PhysicalOperator for InListScanOperator {
    fn schema(&self) -> &TableSchema {
        &self.base.schema
    }

    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.base.reset_and_open();

        let (index, pk_types) = self.base.resolve_index_meta()?;

        let mut all_pks = Vec::new();
        for values in &self.column_values {
            let pks = ctx
                .store
                .scan_index(
                    ctx.txn,
                    ctx.db_id,
                    self.base.schema.table_id,
                    self.base.index_id,
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
        if let Some(limit) = self.base.scan_limit {
            deduped_pks.truncate(limit);
        }

        self.base.pk_queue = deduped_pks;
        self.base.load_next_batch(ctx).await?;

        Ok(())
    }

    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        self.base.next_row(ctx).await
    }

    async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.base.close();
        Ok(())
    }

    fn name(&self) -> &'static str {
        "InListScan"
    }

    fn explain_info(&self) -> Option<String> {
        Some(format!(
            "table={}, index={}",
            self.base.schema.name, self.base.index_name
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDef, DataType, IndexDef};

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
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: DataType::Text,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                    generation_expr: None,
                    generation_expr_authorized_by: None,
                    collation: None,
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

    fn test_schema_with_index(index_columns: Vec<&str>) -> TableSchema {
        let mut schema = test_schema();
        schema.indexes = vec![IndexDef {
            name: "idx_users_name".to_string(),
            id: 42,
            columns: index_columns.into_iter().map(|c| c.to_string()).collect(),
            unique: false,
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: vec![],
            state: Default::default(),
            cached_predicate_conjuncts: None,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        }];
        schema
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

    #[test]
    fn test_index_scan_base_reset_and_close_state() {
        let schema = test_schema_with_index(vec!["name"]);
        let mut base = IndexScanBase::new(schema, 42, "idx_users_name".to_string(), Some(5));
        base.pk_queue = vec![vec![Value::Int32(1)]];
        base.row_buffer = vec![Row::new(vec![
            Value::Int32(1),
            Value::Text("Alice".to_string()),
        ])];
        base.position = 9;
        base.opened = false;

        base.reset_and_open();
        assert!(base.opened);
        assert_eq!(base.position, 0);
        assert!(base.pk_queue.is_empty());
        assert!(base.row_buffer.is_empty());

        base.pk_queue = vec![vec![Value::Int32(2)]];
        base.row_buffer = vec![Row::new(vec![
            Value::Int32(2),
            Value::Text("Bob".to_string()),
        ])];
        base.close();
        assert!(!base.opened);
        assert!(base.pk_queue.is_empty());
        assert!(base.row_buffer.is_empty());
    }

    #[test]
    fn test_index_scan_base_resolve_index_meta_uses_pk_types() {
        let schema = test_schema_with_index(vec!["name"]);
        let base = IndexScanBase::new(schema, 42, "idx_users_name".to_string(), None);
        let (index, pk_types) = base.resolve_index_meta().unwrap();

        assert_eq!(index.name, "idx_users_name");
        assert_eq!(pk_types, vec![DataType::Int32]);
    }

    #[test]
    fn test_index_scan_base_resolve_index_meta_defaults_uuid_without_pk() {
        let mut schema = test_schema_with_index(vec!["name"]);
        schema.pk_indices.clear();

        let base = IndexScanBase::new(schema, 42, "idx_users_name".to_string(), None);
        let (_, pk_types) = base.resolve_index_meta().unwrap();

        assert_eq!(pk_types, vec![DataType::Uuid]);
    }

    #[test]
    fn test_index_scan_base_resolve_index_meta_missing_index() {
        let schema = test_schema_with_index(vec!["name"]);
        let base = IndexScanBase::new(schema, 99, "idx_missing".to_string(), None);

        let err = base.resolve_index_meta().unwrap_err();
        assert!(err.to_string().contains("Index idx_missing not found"));
    }

    #[test]
    fn test_index_scan_base_resolve_index_column_types_case_insensitive() {
        let schema = test_schema_with_index(vec!["NAME"]);
        let base = IndexScanBase::new(schema, 42, "idx_users_name".to_string(), None);
        let (index, _) = base.resolve_index_meta().unwrap();

        let types = base.resolve_index_column_types(index).unwrap();
        assert_eq!(types, vec![DataType::Text]);
    }

    #[test]
    fn test_index_scan_base_resolve_index_column_types_missing_column() {
        let schema = test_schema_with_index(vec!["missing_column"]);
        let base = IndexScanBase::new(schema, 42, "idx_users_name".to_string(), None);
        let (index, _) = base.resolve_index_meta().unwrap();

        let err = base.resolve_index_column_types(index).unwrap_err();
        assert!(err
            .to_string()
            .contains("Index column 'missing_column' not found"));
    }

    #[test]
    fn test_index_scan_wrappers_use_base_fields() {
        let schema = test_schema_with_index(vec!["name"]);

        let point = IndexScanOperator::new_with_scan_limit(
            schema.clone(),
            42,
            "idx_users_name".to_string(),
            vec![Value::Text("Alice".to_string())],
            Some(3),
        );
        assert_eq!(point.name(), "IndexScan");
        assert_eq!(
            point.explain_info(),
            Some("table=users, index=idx_users_name".to_string())
        );
        assert_eq!(point.base.scan_limit, Some(3));

        let range = RangeIndexScanOperator::new(
            schema.clone(),
            42,
            "idx_users_name".to_string(),
            vec![Value::Text("A".to_string())],
            Some(Value::Text("A".to_string())),
            true,
            Some(Value::Text("M".to_string())),
            false,
        );
        assert_eq!(range.name(), "RangeIndexScan");
        assert_eq!(
            range.explain_info(),
            Some("table=users, index=idx_users_name".to_string())
        );
        assert_eq!(range.base.scan_limit, None);

        let in_list = InListScanOperator::new(
            schema,
            42,
            "idx_users_name".to_string(),
            vec![
                vec![Value::Text("Alice".to_string())],
                vec![Value::Text("Bob".to_string())],
            ],
        );
        assert_eq!(in_list.name(), "InListScan");
        assert_eq!(
            in_list.explain_info(),
            Some("table=users, index=idx_users_name".to_string())
        );
        assert_eq!(in_list.base.scan_limit, None);
    }
}
