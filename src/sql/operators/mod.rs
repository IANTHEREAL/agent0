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
//! - `HashJoin` - Equi-join hash join
//! - `HashAggregate` - GROUP BY with hash table
//! - `Sort` - ORDER BY implementation
//! - `Limit` - LIMIT/OFFSET handling

mod aggregate;
mod context;
mod cte;
mod distinct;
mod executor;
mod filter;
mod gin_scan;
mod hash_join;
mod hash_semi_join;
mod hnsw_scan;
mod join;
pub(crate) mod key_encoding;
mod limit;
mod project;
mod scan;
mod set_operation;
mod sort;
mod table_function;
mod window;

pub use aggregate::*;
pub use context::*;
#[allow(unused_imports)] // Operator framework — re-exported for future use
pub use cte::*;
pub use distinct::*;
pub use executor::*;
pub use filter::*;
pub use gin_scan::*;
pub use hash_join::*;
pub use hash_semi_join::*;
#[allow(unused_imports)] // Operator framework — re-exported for future use
pub use hnsw_scan::*;
pub use join::*;
pub use limit::*;
pub use project::*;
pub use scan::*;
pub use set_operation::*;
pub use sort::*;
pub use table_function::*;
pub use window::*;

use crate::model::{Row, TableSchema};
use crate::pool::try_grow_statement_memory_scope;
use crate::sql::memory::estimate_row_size;
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

    #[allow(dead_code)] // framework: operator trait API for EXPLAIN support
    fn children(&self) -> Vec<&dyn PhysicalOperator> {
        vec![]
    }

    #[allow(dead_code)] // framework: operator trait API for EXPLAIN support
    fn children_mut(&mut self) -> Vec<&mut dyn PhysicalOperator> {
        vec![]
    }

    /// Estimated output row count for query planning.
    fn estimated_rows(&self) -> Option<usize> {
        None
    }

    #[allow(dead_code)] // framework: operator trait API for EXPLAIN support
    fn name(&self) -> &'static str;

    #[allow(dead_code)] // framework: operator trait API for EXPLAIN support
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
        try_grow_statement_memory_scope("operators.collect_all.rows", estimate_row_size(&row))?;
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests;
