use super::*;
use crate::model::{ColumnDef, DataType, Value};

/// A simple test operator that yields predefined rows.
#[derive(Debug)]
struct MockOperator {
    schema: TableSchema,
    rows: Vec<Row>,
    position: usize,
    opened: bool,
    closed: bool,
}

impl MockOperator {
    fn new(schema: TableSchema, rows: Vec<Row>) -> Self {
        Self {
            schema,
            rows,
            position: 0,
            opened: false,
            closed: false,
        }
    }
}

#[async_trait]
impl PhysicalOperator for MockOperator {
    fn schema(&self) -> &TableSchema {
        &self.schema
    }

    async fn open(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.opened = true;
        self.position = 0;
        Ok(())
    }

    async fn next(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>> {
        if self.position < self.rows.len() {
            let row = self.rows[self.position].clone();
            self.position += 1;
            Ok(Some(row))
        } else {
            Ok(None)
        }
    }

    async fn close(&mut self, _ctx: &mut ExecutionContext<'_>) -> Result<()> {
        self.closed = true;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "Mock"
    }

    fn estimated_rows(&self) -> Option<usize> {
        Some(self.rows.len())
    }
}

fn test_schema() -> TableSchema {
    TableSchema {
        name: "test".to_string(),
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
                is_dropped: false,
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
                is_dropped: false,
            },
        ],
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![0],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
        rls_enabled: false,
        rls_force: false,
        from_alias: None,
    }
}

fn test_rows() -> Vec<Row> {
    vec![
        Row::new(vec![Value::Int32(1), Value::Text("Alice".to_string())]),
        Row::new(vec![Value::Int32(2), Value::Text("Bob".to_string())]),
        Row::new(vec![Value::Int32(3), Value::Text("Charlie".to_string())]),
    ]
}

#[tokio::test]
async fn test_mock_operator_lifecycle() {
    let schema = test_schema();
    let rows = test_rows();
    let op = MockOperator::new(schema, rows.clone());

    assert!(!op.opened);
    assert!(!op.closed);
    assert_eq!(op.position, 0);
    assert_eq!(op.name(), "Mock");
    assert_eq!(op.estimated_rows(), Some(3));
    assert_eq!(op.schema().name, "test");
}

#[test]
fn test_operator_schema() {
    let schema = test_schema();
    let rows = test_rows();
    let op = MockOperator::new(schema.clone(), rows);

    assert_eq!(op.schema().name, "test");
    assert_eq!(op.schema().columns.len(), 2);
    assert_eq!(op.schema().columns[0].name, "id");
    assert_eq!(op.schema().columns[1].name, "name");
}
