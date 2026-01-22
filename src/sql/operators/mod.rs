//! Physical operators for Volcano-style query execution
//!
//! This module implements the iterator model for query execution where each
//! operator implements `open()`, `next()`, `close()` semantics.
//!
//! # Architecture
//!
//! ```text
//! SQL Query
//!     → Planner builds operator tree
//!     → Executor calls root.open()
//!     → Executor calls root.next() repeatedly until None
//!     → Executor calls root.close()
//! ```
//!
//! # Operators
//!
//! - `TableScan` - Full table scan from TiKV
//! - `IndexScan` - Index-based row lookup
//! - `Filter` - Row filtering with predicates
//! - `Project` - Column projection and expression evaluation
//! - `NestedLoopJoin` - Basic join implementation
//! - `HashAggregate` - GROUP BY with hash table
//! - `Sort` - ORDER BY implementation
//! - `Limit` - LIMIT/OFFSET handling

mod aggregate;
mod context;
mod cte;
mod distinct;
mod executor;
mod filter;
mod join;
mod limit;
mod planner;
mod project;
mod scan;
mod set_operation;
mod sort;
mod window;

pub use aggregate::*;
pub use context::*;
pub use cte::*;
pub use distinct::*;
pub use executor::*;
pub use filter::*;
pub use join::*;
pub use limit::*;
pub use planner::*;
pub use project::*;
pub use scan::*;
pub use set_operation::*;
pub use sort::*;
pub use window::*;

use crate::types::{Row, TableSchema};
use anyhow::Result;
use async_trait::async_trait;
use std::fmt::Debug;

/// Physical operator trait implementing the Volcano iterator model.
///
/// Each operator produces rows one at a time via the `next()` method.
/// This enables pipeline execution without full materialization.
///
/// # Lifecycle
///
/// 1. `open()` - Initialize operator state, open child operators
/// 2. `next()` - Called repeatedly to get rows, returns `None` when exhausted
/// 3. `close()` - Cleanup resources, close child operators
///
/// # Example
///
/// ```ignore
/// let mut scan = TableScanOperator::new(...);
/// scan.open(&mut ctx).await?;
/// while let Some(row) = scan.next(&mut ctx).await? {
///     // Process row
/// }
/// scan.close(&mut ctx).await?;
/// ```
#[async_trait]
pub trait PhysicalOperator: Send + Sync + Debug {
    /// Return the output schema of this operator.
    fn schema(&self) -> &TableSchema;

    /// Initialize the operator and its children.
    ///
    /// This is called once before any `next()` calls.
    /// Implementations should:
    /// - Initialize internal state
    /// - Call `open()` on child operators
    /// - Acquire any needed resources
    async fn open(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()>;

    /// Get the next output row.
    ///
    /// Returns `Ok(Some(row))` for each row, `Ok(None)` when exhausted.
    /// After returning `None`, subsequent calls should also return `None`.
    async fn next(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<Option<Row>>;

    /// Close the operator and release resources.
    ///
    /// This is called once after all rows have been consumed or on error.
    /// Implementations should:
    /// - Release internal resources
    /// - Call `close()` on child operators
    async fn close(&mut self, ctx: &mut ExecutionContext<'_>) -> Result<()>;

    /// Get child operators for tree traversal.
    ///
    /// Used for EXPLAIN and debugging.
    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![]
    }

    /// Get mutable references to child operators.
    ///
    /// Used internally for propagating open/close.
    fn children_mut(&mut self) -> Vec<&mut dyn PhysicalOperator> {
        vec![]
    }

    /// Estimated output row count for query planning.
    fn estimated_rows(&self) -> Option<usize> {
        None
    }

    /// Operator name for EXPLAIN output.
    fn name(&self) -> &'static str;

    /// Additional info for EXPLAIN output (e.g., "filter: id > 10").
    fn explain_info(&self) -> Option<String> {
        None
    }
}

/// Boxed operator type for dynamic dispatch.
pub type BoxedOperator = Box<dyn PhysicalOperator>;

/// Helper to collect all rows from an operator.
///
/// This is useful for operators that need to materialize their input
/// (e.g., Sort, HashAggregate) or for testing.
///
/// # Warning
///
/// This defeats the streaming benefits of the Volcano model.
/// Use sparingly and only when necessary.
pub async fn collect_all(
    op: &mut dyn PhysicalOperator,
    ctx: &mut ExecutionContext<'_>,
) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    while let Some(row) = op.next(ctx).await? {
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnDef, DataType, Value};

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
}
