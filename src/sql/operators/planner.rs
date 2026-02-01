//! Physical query planner that builds operator trees from SQL AST
//!
//! This module bridges the gap between the SQL parser and the physical operators.
//! It takes a parsed SELECT query and produces an operator tree that can be executed.
//!
//! # Supported Query Patterns
//!
//! Currently supports simple SELECT queries:
//! - Single table (no JOINs)
//! - Optional WHERE clause (with index selection)
//! - Optional ORDER BY
//! - Optional LIMIT/OFFSET
//!
//! # Example
//!
//! ```ignore
//! let planner = PhysicalPlanner::new(store, search_path);
//! let operator = planner.plan_simple_select(&schema, filter, order_by, limit, offset).await?;
//! ```

use anyhow::Result;
use sqlparser::ast::{Expr, OrderByExpr};
use std::sync::Arc;

use super::{
    BoxedOperator, FilterOperator, IndexScanOperator, LimitOperator, ProjectOperator, SortOperator,
    TableScanOperator,
};
use crate::sql::planner::{choose_best_access_path_for_filter, ScanType};
use crate::storage::TikvStore;
use crate::types::{DataType, TableSchema};

/// Physical query planner that builds operator trees.
///
/// The planner uses cost-based optimization to choose access paths (full scan vs index scan)
/// and constructs a tree of physical operators that implement the Volcano iterator model.
pub struct PhysicalPlanner {
    store: Arc<TikvStore>,
    search_path: Vec<String>,
}

impl std::fmt::Debug for PhysicalPlanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalPlanner")
            .field("search_path", &self.search_path)
            .finish_non_exhaustive()
    }
}

impl PhysicalPlanner {
    /// Create a new physical planner.
    pub fn new(store: Arc<TikvStore>, search_path: Vec<String>) -> Self {
        Self { store, search_path }
    }

    /// Plan a simple SELECT query (single table, no JOINs).
    ///
    /// This builds an operator tree for queries of the form:
    /// ```sql
    /// SELECT * FROM table WHERE filter ORDER BY cols LIMIT n OFFSET m
    /// ```
    ///
    /// The operator tree is built bottom-up:
    /// 1. Scan operator (TableScan or IndexScan based on cost)
    /// 2. Filter operator (if WHERE clause present and not fully handled by index)
    /// 3. Sort operator (if ORDER BY present)
    /// 4. Limit operator (if LIMIT/OFFSET present)
    ///
    /// # Arguments
    ///
    /// * `schema` - Table schema
    /// * `filter` - Optional WHERE clause expression
    /// * `order_by` - ORDER BY expressions (can be empty)
    /// * `limit` - Optional LIMIT value
    /// * `offset` - OFFSET value (0 if not specified)
    /// * `estimated_rows` - Estimated table row count for cost estimation
    ///
    /// # Returns
    ///
    /// A boxed operator that is the root of the execution tree.
    pub fn plan_simple_select(
        &self,
        schema: TableSchema,
        filter: Option<&Expr>,
        order_by: Vec<OrderByExpr>,
        limit: Option<usize>,
        offset: usize,
        estimated_rows: usize,
    ) -> Result<BoxedOperator> {
        let access_path = choose_best_access_path_for_filter(&schema, filter, estimated_rows);

        let scan_upper_bound = if filter.is_none() && order_by.is_empty() {
            match limit {
                Some(0) => Some(0),
                Some(n) => Some(offset.saturating_add(n)),
                None => None,
            }
        } else {
            None
        };

        let mut root: BoxedOperator = match access_path.scan_type {
            ScanType::FullTableScan => {
                Box::new(TableScanOperator::new_with_scan_limit(schema.clone(), scan_upper_bound))
            }
            ScanType::IndexScan {
                index_id,
                index_name,
                values,
                ..
            } => Box::new(IndexScanOperator::new(
                schema.clone(),
                index_id,
                index_name,
                values,
            )),
            ScanType::IndexRangeScan {
                index_id,
                index_name,
                prefix_values,
                ..
            } => Box::new(IndexScanOperator::new(
                schema.clone(),
                index_id,
                index_name,
                prefix_values,
            )),
            ScanType::GinIndexScan { .. } => {
                Box::new(TableScanOperator::new_with_scan_limit(schema.clone(), scan_upper_bound))
            }
        };

        if let Some(filter_expr) = filter {
            root = Box::new(FilterOperator::new(root, filter_expr.clone()));
        }

        if !order_by.is_empty() {
            root = Box::new(SortOperator::new(root, order_by));
        }

        if limit.is_some() || offset > 0 {
            root = Box::new(LimitOperator::new(root, limit, offset));
        }

        Ok(root)
    }

    /// Get the store reference.
    #[allow(dead_code)]
    pub fn store(&self) -> &Arc<TikvStore> {
        &self.store
    }

    /// Get the search path.
    #[allow(dead_code)]
    pub fn search_path(&self) -> &[String] {
        &self.search_path
    }
}

/// Builder for constructing operator trees programmatically.
///
/// This provides a fluent API for building operator trees, useful for testing
/// and for more complex query patterns.
#[derive(Debug)]
pub struct OperatorBuilder {
    root: BoxedOperator,
}

impl OperatorBuilder {
    /// Start building with a scan operator.
    pub fn scan(schema: TableSchema) -> Self {
        Self {
            root: Box::new(TableScanOperator::new(schema)),
        }
    }

    /// Start building with an index scan operator.
    pub fn index_scan(
        schema: TableSchema,
        index_id: u64,
        index_name: String,
        lookup_values: Vec<crate::types::Value>,
    ) -> Self {
        Self {
            root: Box::new(IndexScanOperator::new(
                schema,
                index_id,
                index_name,
                lookup_values,
            )),
        }
    }

    /// Add a filter operator.
    pub fn filter(self, predicate: Expr) -> Self {
        Self {
            root: Box::new(FilterOperator::new(self.root, predicate)),
        }
    }

    /// Add a sort operator.
    pub fn sort(self, order_by: Vec<OrderByExpr>) -> Self {
        Self {
            root: Box::new(SortOperator::new(self.root, order_by)),
        }
    }

    /// Add a limit operator.
    pub fn limit(self, limit: Option<usize>, offset: usize) -> Self {
        Self {
            root: Box::new(LimitOperator::new(self.root, limit, offset)),
        }
    }

    /// Add a project operator.
    pub fn project(
        self,
        expressions: Vec<Expr>,
        output_names: Vec<String>,
        output_types: Vec<DataType>,
    ) -> Self {
        Self {
            root: Box::new(ProjectOperator::new(
                self.root,
                expressions,
                output_names,
                output_types,
            )),
        }
    }

    /// Build the final operator tree.
    pub fn build(self) -> BoxedOperator {
        self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ColumnDef, DataType, IndexDef, Value};
    use sqlparser::ast::{BinaryOperator, Ident};

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
                ColumnDef {
                    name: "age".to_string(),
                    data_type: DataType::Int32,
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
            indexes: vec![IndexDef {
                id: 1,
                name: "idx_name".to_string(),
                columns: vec!["name".to_string()],
                unique: false,
                method: None,
                predicate: None,
                expressions: Vec::new(),
            }],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        }
    }

    fn make_eq_expr(col: &str, val: i32) -> Expr {
        Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new(col))),
            op: BinaryOperator::Eq,
            right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
                val.to_string(),
                false,
            ))),
        }
    }

    #[test]
    fn test_operator_builder_scan_only() {
        let schema = test_schema();
        let op = OperatorBuilder::scan(schema).build();

        assert_eq!(op.name(), "TableScan");
        assert!(op.children().is_empty());
    }

    #[test]
    fn test_operator_builder_with_filter() {
        let schema = test_schema();
        let predicate = make_eq_expr("id", 42);

        let op = OperatorBuilder::scan(schema).filter(predicate).build();

        assert_eq!(op.name(), "Filter");
        assert_eq!(op.children().len(), 1);
        assert_eq!(op.children()[0].name(), "TableScan");
    }

    #[test]
    fn test_operator_builder_with_sort() {
        let schema = test_schema();
        let order_by = vec![OrderByExpr {
            expr: Expr::Identifier(Ident::new("name")),
            asc: Some(true),
            nulls_first: None,
        }];

        let op = OperatorBuilder::scan(schema).sort(order_by).build();

        assert_eq!(op.name(), "Sort");
        assert_eq!(op.children().len(), 1);
        assert_eq!(op.children()[0].name(), "TableScan");
    }

    #[test]
    fn test_operator_builder_with_limit() {
        let schema = test_schema();

        let op = OperatorBuilder::scan(schema).limit(Some(10), 5).build();

        assert_eq!(op.name(), "Limit");
        assert_eq!(op.children().len(), 1);
        assert_eq!(op.children()[0].name(), "TableScan");
    }

    #[test]
    fn test_operator_builder_full_chain() {
        let schema = test_schema();
        let predicate = make_eq_expr("id", 42);
        let order_by = vec![OrderByExpr {
            expr: Expr::Identifier(Ident::new("name")),
            asc: Some(true),
            nulls_first: None,
        }];

        let op = OperatorBuilder::scan(schema)
            .filter(predicate)
            .sort(order_by)
            .limit(Some(10), 0)
            .build();

        assert_eq!(op.name(), "Limit");
        let sort = op.children()[0];
        assert_eq!(sort.name(), "Sort");
        let filter = sort.children()[0];
        assert_eq!(filter.name(), "Filter");
        let scan = filter.children()[0];
        assert_eq!(scan.name(), "TableScan");
    }

    #[test]
    fn test_operator_builder_index_scan() {
        let schema = test_schema();

        let op = OperatorBuilder::index_scan(
            schema,
            1,
            "idx_name".to_string(),
            vec![Value::Text("Alice".to_string())],
        )
        .build();

        assert_eq!(op.name(), "IndexScan");
    }

    #[test]
    fn test_operator_builder_with_project() {
        let schema = test_schema();
        let expressions = vec![
            Expr::Identifier(Ident::new("id")),
            Expr::Identifier(Ident::new("name")),
        ];

        let op = OperatorBuilder::scan(schema)
            .project(
                expressions,
                vec!["id".to_string(), "name".to_string()],
                vec![DataType::Int32, DataType::Text],
            )
            .build();

        assert_eq!(op.name(), "Project");
        assert_eq!(op.children().len(), 1);
        assert_eq!(op.children()[0].name(), "TableScan");
        assert_eq!(op.schema().columns.len(), 2);
    }
}
