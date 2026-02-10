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
//! let operator = planner.plan_simple_select(db_id, schema, filter, order_by, limit, offset, 1000)?;
//! ```

use anyhow::Result;
use sqlparser::ast::{BinaryOperator, Expr, OrderByExpr};
use std::collections::HashMap;
use std::sync::Arc;

use super::{
    BoxedOperator, FilterOperator, InListScanOperator, IndexScanOperator, LimitOperator,
    ProjectOperator, RangeIndexScanOperator, SortOperator, TableScanOperator,
};
use crate::sql::expr::eval_expr;
use crate::sql::planner::{choose_best_access_path_for_filter, ScanType};
use crate::sql::value_coercion::coerce_value_for_column;
use crate::storage::TikvStore;
use crate::types::{DataType, TableSchema, Value};

fn extract_column_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(ident.value.to_lowercase()),
        Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.to_lowercase()),
        Expr::Nested(inner) => extract_column_name(inner),
        _ => None,
    }
}

fn collect_eq_predicates(expr: &Expr, out: &mut HashMap<String, Value>) -> Option<()> {
    match expr {
        Expr::Nested(inner) => collect_eq_predicates(inner, out),
        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::And => {
                collect_eq_predicates(left, out)?;
                collect_eq_predicates(right, out)?;
                Some(())
            }
            BinaryOperator::Eq => {
                let (col, val) = if let Some(col) = extract_column_name(left) {
                    (col, eval_expr(right, None, None).ok()?)
                } else if let Some(col) = extract_column_name(right) {
                    (col, eval_expr(left, None, None).ok()?)
                } else {
                    return None;
                };

                if let Some(existing) = out.get(&col) {
                    if existing != &val {
                        return None;
                    }
                    return Some(());
                }

                out.insert(col, val);
                Some(())
            }
            _ => None,
        },
        _ => None,
    }
}

fn filter_is_exact_index_lookup(
    filter: &Expr,
    schema: &TableSchema,
    index_id: u64,
    lookup_values: &[Value],
) -> bool {
    let Some(index) = schema.indexes.iter().find(|i| i.id == index_id) else {
        return false;
    };

    if lookup_values.is_empty() || lookup_values.len() > index.columns.len() {
        return false;
    }

    let mut predicates: HashMap<String, Value> = HashMap::new();
    if collect_eq_predicates(filter, &mut predicates).is_none() {
        return false;
    }

    if predicates.len() != lookup_values.len() {
        return false;
    }

    for (i, col) in index.columns.iter().take(lookup_values.len()).enumerate() {
        let key = col.to_lowercase();
        let Some(pred_value) = predicates.get(&key) else {
            return false;
        };

        let coerced = if let Some(col_def) = schema
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(col))
        {
            coerce_value_for_column(pred_value.clone(), col_def)
                .unwrap_or_else(|_| pred_value.clone())
        } else {
            pred_value.clone()
        };

        if coerced != lookup_values[i] {
            return false;
        }
    }

    true
}

/// Physical query planner that builds operator trees.
///
/// The planner uses cost-based optimization to choose access paths (full scan vs index scan)
/// and constructs a tree of physical operators that implement the Volcano iterator model.
pub struct PhysicalPlanner {
    #[allow(dead_code)] // used by future index-aware planning
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
        db_id: u64,
        schema: TableSchema,
        filter: Option<&Expr>,
        order_by: Vec<OrderByExpr>,
        limit: Option<usize>,
        offset: usize,
        estimated_rows: usize,
    ) -> Result<BoxedOperator> {
        let access_path =
            choose_best_access_path_for_filter(db_id, &schema, filter, estimated_rows);

        let scan_upper_bound = match limit {
            Some(0) => Some(0),
            Some(n) => Some(offset.saturating_add(n)),
            None => None,
        };

        let mut root: BoxedOperator = match access_path.scan_type {
            ScanType::FullTableScan => {
                let scan_limit = if filter.is_none() && order_by.is_empty() {
                    scan_upper_bound
                } else {
                    None
                };
                Box::new(TableScanOperator::new_with_scan_limit(
                    schema.clone(),
                    scan_limit,
                ))
            }
            ScanType::IndexScan {
                index_id,
                index_name,
                values,
                ..
            } => {
                let scan_limit = if order_by.is_empty()
                    && scan_upper_bound.is_some()
                    && filter.is_some_and(|f| {
                        filter_is_exact_index_lookup(f, &schema, index_id, &values)
                    }) {
                    scan_upper_bound
                } else {
                    None
                };
                Box::new(IndexScanOperator::new_with_scan_limit(
                    schema.clone(),
                    index_id,
                    index_name,
                    values,
                    scan_limit,
                ))
            }
            ScanType::IndexRangeScan {
                index_id,
                index_name,
                prefix_values,
                ..
            } => {
                let scan_limit = if order_by.is_empty()
                    && scan_upper_bound.is_some()
                    && filter.is_some_and(|f| {
                        filter_is_exact_index_lookup(f, &schema, index_id, &prefix_values)
                    }) {
                    scan_upper_bound
                } else {
                    None
                };
                Box::new(IndexScanOperator::new_with_scan_limit(
                    schema.clone(),
                    index_id,
                    index_name,
                    prefix_values,
                    scan_limit,
                ))
            }
            ScanType::IndexBoundedRangeScan {
                index_id,
                index_name,
                prefix_values,
                range_start,
                start_inclusive,
                range_end,
                end_inclusive,
                ..
            } => Box::new(RangeIndexScanOperator::new(
                schema.clone(),
                index_id,
                index_name,
                prefix_values,
                range_start,
                start_inclusive,
                range_end,
                end_inclusive,
            )),
            ScanType::InListScan {
                index_id,
                index_name,
                column_values,
                ..
            } => Box::new(InListScanOperator::new(
                schema.clone(),
                index_id,
                index_name,
                column_values,
            )),
            ScanType::GinIndexScan { .. } => Box::new(TableScanOperator::new(schema.clone())),
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

    #[allow(dead_code)] // accessor for future index-aware planning
    pub fn store(&self) -> &Arc<TikvStore> {
        &self.store
    }

    #[allow(dead_code)] // accessor for future index-aware planning
    pub fn search_path(&self) -> &[String] {
        &self.search_path
    }
}

/// Builder for constructing operator trees programmatically.
///
/// This provides a fluent API for building operator trees, useful for testing
/// and for more complex query patterns.
#[derive(Debug)]
#[allow(dead_code)] // Operator framework — fluent builder API for future use
pub struct OperatorBuilder {
    root: BoxedOperator,
}

#[allow(dead_code)] // Operator framework
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
            from_alias: None,
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
    fn test_collect_eq_predicates_single() {
        let expr = make_eq_expr("id", 42);
        let mut out = HashMap::new();
        assert!(super::collect_eq_predicates(&expr, &mut out).is_some());
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out.get("id"),
            Some(Value::Int32(42)) | Some(Value::Int64(42))
        ));
    }

    #[test]
    fn test_collect_eq_predicates_and_conjunction() {
        let expr = Expr::BinaryOp {
            left: Box::new(make_eq_expr("a", 1)),
            op: BinaryOperator::And,
            right: Box::new(make_eq_expr("b", 2)),
        };
        let mut out = HashMap::new();
        assert!(super::collect_eq_predicates(&expr, &mut out).is_some());
        assert_eq!(out.len(), 2);
        assert!(out.contains_key("a"));
        assert!(out.contains_key("b"));
    }

    #[test]
    fn test_collect_eq_predicates_non_eq_returns_none() {
        let expr = Expr::BinaryOp {
            left: Box::new(Expr::Identifier(Ident::new("id"))),
            op: BinaryOperator::Gt,
            right: Box::new(Expr::Value(sqlparser::ast::Value::Number(
                "5".to_string(),
                false,
            ))),
        };
        let mut out = HashMap::new();
        assert!(super::collect_eq_predicates(&expr, &mut out).is_none());
        assert!(out.is_empty());
    }

    #[test]
    fn test_collect_eq_predicates_conflicting_values() {
        let expr = Expr::BinaryOp {
            left: Box::new(make_eq_expr("id", 1)),
            op: BinaryOperator::And,
            right: Box::new(make_eq_expr("id", 2)),
        };
        let mut out = HashMap::new();
        assert!(super::collect_eq_predicates(&expr, &mut out).is_none());
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
