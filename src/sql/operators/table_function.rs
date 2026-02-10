use std::fmt;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio::sync::mpsc;

use super::{ExecutionContext, PhysicalOperator};
use crate::types::{Row, TableSchema};

pub struct TableFunctionScanOperator {
    schema: TableSchema,
    source: RowSource,
    opened: bool,
}

// SAFETY: Accessed exclusively via &mut self in PhysicalOperator methods.
// The mpsc::Receiver in RowSource::Channel is Send but !Sync; we never share
// the operator across threads — it's owned by a single executor task.
unsafe impl Sync for TableFunctionScanOperator {}

enum RowSource {
    Preloaded { rows: Vec<Row>, position: usize },
    Channel { receiver: mpsc::Receiver<Row> },
}

impl TableFunctionScanOperator {
    pub fn new_with_rows(schema: TableSchema, rows: Vec<Row>) -> Self {
        Self {
            schema,
            source: RowSource::Preloaded { rows, position: 0 },
            opened: false,
        }
    }

    pub fn new_with_channel(schema: TableSchema, receiver: mpsc::Receiver<Row>) -> Self {
        Self {
            schema,
            source: RowSource::Channel { receiver },
            opened: false,
        }
    }
}

impl fmt::Debug for TableFunctionScanOperator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let source_desc = match &self.source {
            RowSource::Preloaded { rows, position } => {
                format!("Preloaded({}/{})", position, rows.len())
            }
            RowSource::Channel { .. } => "Channel(streaming)".to_string(),
        };
        f.debug_struct("TableFunctionScanOperator")
            .field("schema", &self.schema.name)
            .field("source", &source_desc)
            .field("opened", &self.opened)
            .finish()
    }
}

#[async_trait]
impl PhysicalOperator for TableFunctionScanOperator {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    async fn open(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        if let RowSource::Preloaded { position, .. } = &mut self.source {
            *position = 0;
        }
        self.opened = true;
        Ok(())
    }

    async fn next(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if !self.opened {
            return Err(anyhow!("Operator not opened"));
        }
        match &mut self.source {
            RowSource::Preloaded { rows, position } => {
                if *position < rows.len() {
                    let row = rows[*position].clone();
                    *position += 1;
                    Ok(Some(row))
                } else {
                    Ok(None)
                }
            }
            RowSource::Channel { receiver } => Ok(receiver.recv().await),
        }
    }

    async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        match &mut self.source {
            RowSource::Preloaded { rows, .. } => rows.clear(),
            RowSource::Channel { receiver } => receiver.close(),
        }
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
        let op = TableFunctionScanOperator::new_with_rows(test_schema(), test_rows(3));
        assert!(!op.opened);
        assert!(matches!(&op.source, RowSource::Preloaded { rows, .. } if rows.len() == 3));
    }

    #[tokio::test]
    async fn test_channel_source_yields_rows() {
        let (tx, rx) = mpsc::channel(16);
        let rows = test_rows(3);
        for row in rows.clone() {
            tx.send(row).await.unwrap();
        }
        drop(tx);

        let mut op = TableFunctionScanOperator::new_with_channel(test_schema(), rx);

        let mut collected = Vec::new();
        op.opened = true;
        loop {
            match &mut op.source {
                RowSource::Channel { receiver } => match receiver.recv().await {
                    Some(row) => collected.push(row),
                    None => break,
                },
                _ => unreachable!(),
            }
        }

        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0].values[0], Value::Int32(0));
        assert_eq!(collected[2].values[1], Value::Text("row_2".to_string()));
    }

    #[test]
    fn test_channel_source_debug_format() {
        let (_tx, rx) = mpsc::channel::<Row>(1);
        let op = TableFunctionScanOperator::new_with_channel(test_schema(), rx);
        let debug = format!("{:?}", op);
        assert!(debug.contains("Channel(streaming)"));
        assert!(debug.contains("fs9_result"));
    }
}
