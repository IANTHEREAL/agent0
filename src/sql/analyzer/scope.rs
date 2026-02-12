//! Scope chain for column resolution during analysis.
//!
//! The Analyzer maintains a stack of `Scope` frames. Each query level (main
//! query, subquery, CTE body) pushes a scope. Column resolution walks from the
//! innermost scope outward, and the depth difference becomes `scope_depth` on
//! the `ColumnRef` node — enabling correlated subquery evaluation without
//! `SubstituteVisitor`.

use crate::types::DataType;
use std::collections::HashMap;

use super::error::AnalyzerError;

/// A column visible in the current scope.
#[derive(Debug, Clone)]
pub struct ScopeColumn {
    /// Table alias (e.g. "t1"), or `None` for unaliased single-table queries.
    pub table_alias: Option<String>,
    /// Column name (case-preserved from catalog).
    pub column_name: String,
    /// Absolute position in the flattened row.
    pub column_index: usize,
    /// Resolved data type.
    pub data_type: DataType,
    /// Whether the column is nullable.
    pub nullable: bool,
}

/// Result of resolving a column reference.
#[derive(Debug, Clone)]
pub struct ResolvedColumnRef {
    /// Scope depth: 0 = current scope, 1+ = outer scope.
    pub scope_depth: u32,
    /// Absolute column index in the scope's flattened row.
    pub column_index: usize,
    /// Column name (for display).
    pub column_name: String,
    /// Resolved data type.
    pub data_type: DataType,
}

/// A single scope frame in the scope stack.
///
/// Each scope contains the columns visible at that query level.
/// Multiple tables in a FROM clause are flattened into a single scope.
#[derive(Debug, Clone)]
pub struct Scope {
    /// All columns visible in this scope, in flattened-row order.
    columns: Vec<ScopeColumn>,

    /// Index from lowercase column name → list of positions in `columns`.
    /// Multiple entries indicate ambiguity (same column name in different tables).
    column_index: HashMap<String, Vec<usize>>,

    /// Index from (lowercase_table_alias, lowercase_column_name) → position.
    qualified_index: HashMap<(String, String), usize>,

    /// CTE schemas visible from this scope (name → output columns).
    cte_schemas: HashMap<String, Vec<(String, DataType)>>,

    /// Whether aggregate functions are allowed in expressions at this level.
    pub allow_aggregates: bool,

    /// Whether window functions are allowed in expressions at this level.
    pub allow_windows: bool,
}

impl Scope {
    /// Create an empty scope.
    pub fn new() -> Self {
        Self {
            columns: Vec::new(),
            column_index: HashMap::new(),
            qualified_index: HashMap::new(),
            cte_schemas: HashMap::new(),
            allow_aggregates: false,
            allow_windows: false,
        }
    }

    /// Add columns from a table to this scope.
    ///
    /// `alias` is the table alias (or real name if no alias). Columns are
    /// appended to the flattened row, with `column_index` set to the absolute
    /// position starting from the current column count.
    pub fn add_table(&mut self, alias: &str, columns: &[(String, DataType, bool)]) {
        let base_offset = self.columns.len();
        for (idx, (name, data_type, nullable)) in columns.iter().enumerate() {
            let abs_index = base_offset + idx;
            let col = ScopeColumn {
                table_alias: Some(alias.to_string()),
                column_name: name.clone(),
                column_index: abs_index,
                data_type: data_type.clone(),
                nullable: *nullable,
            };

            // Add to unqualified index (for ambiguity detection)
            self.column_index
                .entry(name.to_lowercase())
                .or_default()
                .push(abs_index);

            // Add to qualified index
            self.qualified_index
                .insert((alias.to_lowercase(), name.to_lowercase()), abs_index);

            self.columns.push(col);
        }
    }

    /// Add a single column to the scope (e.g. for subquery output columns).
    pub fn add_column(
        &mut self,
        alias: Option<&str>,
        name: &str,
        data_type: DataType,
        nullable: bool,
    ) {
        let abs_index = self.columns.len();
        let col = ScopeColumn {
            table_alias: alias.map(String::from),
            column_name: name.to_string(),
            column_index: abs_index,
            data_type,
            nullable,
        };

        self.column_index
            .entry(name.to_lowercase())
            .or_default()
            .push(abs_index);

        if let Some(a) = alias {
            self.qualified_index
                .insert((a.to_lowercase(), name.to_lowercase()), abs_index);
        }

        self.columns.push(col);
    }

    /// Register a CTE's output schema so it can be resolved as a table.
    pub fn add_cte(&mut self, name: &str, columns: Vec<(String, DataType)>) {
        self.cte_schemas.insert(name.to_lowercase(), columns);
    }

    /// Look up a CTE by name.
    pub fn get_cte(&self, name: &str) -> Option<&Vec<(String, DataType)>> {
        self.cte_schemas.get(&name.to_lowercase())
    }

    /// Resolve an unqualified column name within this scope.
    ///
    /// Returns `None` if not found, `Err` if ambiguous.
    pub fn resolve_unqualified(&self, name: &str) -> Result<Option<&ScopeColumn>, AnalyzerError> {
        let lower = name.to_lowercase();
        match self.column_index.get(&lower) {
            None => Ok(None),
            Some(positions) if positions.len() > 1 => {
                let tables: Vec<String> = positions
                    .iter()
                    .filter_map(|&pos| self.columns[pos].table_alias.clone())
                    .collect();
                Err(AnalyzerError::AmbiguousColumn {
                    name: name.to_string(),
                    tables,
                })
            }
            Some(positions) => Ok(Some(&self.columns[positions[0]])),
        }
    }

    /// Resolve a qualified column reference (`table.column`).
    pub fn resolve_qualified(&self, table: &str, column: &str) -> Option<&ScopeColumn> {
        let key = (table.to_lowercase(), column.to_lowercase());
        self.qualified_index
            .get(&key)
            .map(|&pos| &self.columns[pos])
    }

    /// Return all column names visible in this scope (for error hints).
    pub fn available_columns(&self) -> Vec<String> {
        self.columns
            .iter()
            .map(|c| match &c.table_alias {
                Some(alias) => format!("{}.{}", alias, c.column_name),
                None => c.column_name.clone(),
            })
            .collect()
    }

    /// Return the number of columns in the flattened row.
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Get a column by its absolute index.
    pub fn get_column(&self, index: usize) -> Option<&ScopeColumn> {
        self.columns.get(index)
    }

    /// Return all columns in this scope.
    pub fn columns(&self) -> &[ScopeColumn] {
        &self.columns
    }
}

/// The scope stack used by the Analyzer.
///
/// Supports pushing/popping scope frames and resolving columns across scopes
/// (for correlated subqueries).
#[derive(Debug)]
pub struct ScopeStack {
    scopes: Vec<Scope>,
}

impl ScopeStack {
    pub fn new() -> Self {
        Self { scopes: Vec::new() }
    }

    /// Push a new scope frame.
    pub fn push(&mut self, scope: Scope) {
        self.scopes.push(scope);
    }

    /// Pop the innermost scope frame.
    pub fn pop(&mut self) -> Option<Scope> {
        self.scopes.pop()
    }

    /// Get a mutable reference to the current (innermost) scope.
    pub fn current_mut(&mut self) -> &mut Scope {
        self.scopes.last_mut().expect("ScopeStack: no active scope")
    }

    /// Get a reference to the current (innermost) scope.
    pub fn current(&self) -> &Scope {
        self.scopes.last().expect("ScopeStack: no active scope")
    }

    /// Resolve a column reference, searching from innermost to outermost scope.
    ///
    /// Returns the resolved column with its `scope_depth` (0 = current).
    /// For correlated subqueries, depth > 0 means "outer reference".
    pub fn resolve_column(&self, name: &str) -> Result<ResolvedColumnRef, AnalyzerError> {
        for (i, scope) in self.scopes.iter().rev().enumerate() {
            match scope.resolve_unqualified(name)? {
                Some(col) => {
                    return Ok(ResolvedColumnRef {
                        scope_depth: i as u32,
                        column_index: col.column_index,
                        column_name: col.column_name.clone(),
                        data_type: col.data_type.clone(),
                    });
                }
                None => continue,
            }
        }

        // Not found in any scope
        let available = if let Some(scope) = self.scopes.last() {
            scope.available_columns()
        } else {
            vec![]
        };

        Err(AnalyzerError::ColumnNotFound {
            name: name.to_string(),
            available,
        })
    }

    /// Resolve a qualified column reference (`table.column`).
    pub fn resolve_qualified_column(
        &self,
        table: &str,
        column: &str,
    ) -> Result<ResolvedColumnRef, AnalyzerError> {
        for (i, scope) in self.scopes.iter().rev().enumerate() {
            if let Some(col) = scope.resolve_qualified(table, column) {
                return Ok(ResolvedColumnRef {
                    scope_depth: i as u32,
                    column_index: col.column_index,
                    column_name: col.column_name.clone(),
                    data_type: col.data_type.clone(),
                });
            }
        }

        let available = if let Some(scope) = self.scopes.last() {
            scope.available_columns()
        } else {
            vec![]
        };

        Err(AnalyzerError::ColumnNotFound {
            name: format!("{}.{}", table, column),
            available,
        })
    }

    /// Look up a CTE by name in any scope (innermost first).
    pub fn resolve_cte(&self, name: &str) -> Option<Vec<(String, DataType)>> {
        for scope in self.scopes.iter().rev() {
            if let Some(cols) = scope.get_cte(name) {
                return Some(cols.clone());
            }
        }
        None
    }

    /// Current nesting depth (number of scopes on the stack).
    pub fn depth(&self) -> usize {
        self.scopes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int_col(name: &str) -> (String, DataType, bool) {
        (name.to_string(), DataType::Int32, false)
    }

    fn text_col(name: &str) -> (String, DataType, bool) {
        (name.to_string(), DataType::Text, true)
    }

    #[test]
    fn single_table_resolution() {
        let mut scope = Scope::new();
        scope.add_table("users", &[int_col("id"), text_col("name")]);

        let col = scope.resolve_unqualified("id").unwrap().unwrap();
        assert_eq!(col.column_index, 0);
        assert_eq!(col.data_type, DataType::Int32);

        let col = scope.resolve_unqualified("name").unwrap().unwrap();
        assert_eq!(col.column_index, 1);
        assert_eq!(col.data_type, DataType::Text);

        assert!(scope.resolve_unqualified("nonexistent").unwrap().is_none());
    }

    #[test]
    fn case_insensitive_resolution() {
        let mut scope = Scope::new();
        scope.add_table("t", &[int_col("Age")]);

        assert!(scope.resolve_unqualified("age").unwrap().is_some());
        assert!(scope.resolve_unqualified("AGE").unwrap().is_some());
        assert!(scope.resolve_unqualified("Age").unwrap().is_some());
    }

    #[test]
    fn ambiguous_column() {
        let mut scope = Scope::new();
        scope.add_table("a", &[int_col("id"), text_col("x")]);
        scope.add_table("b", &[int_col("id"), text_col("y")]);

        let err = scope.resolve_unqualified("id").unwrap_err();
        assert!(matches!(err, AnalyzerError::AmbiguousColumn { .. }));

        // Non-ambiguous columns still resolve
        let col = scope.resolve_unqualified("x").unwrap().unwrap();
        assert_eq!(col.column_index, 1);
    }

    #[test]
    fn qualified_resolution() {
        let mut scope = Scope::new();
        scope.add_table("a", &[int_col("id")]);
        scope.add_table("b", &[int_col("id")]);

        let col = scope.resolve_qualified("a", "id").unwrap();
        assert_eq!(col.column_index, 0);

        let col = scope.resolve_qualified("b", "id").unwrap();
        assert_eq!(col.column_index, 1);

        assert!(scope.resolve_qualified("c", "id").is_none());
    }

    #[test]
    fn flattened_row_offsets() {
        let mut scope = Scope::new();
        scope.add_table("a", &[int_col("x"), int_col("y")]);
        scope.add_table("b", &[int_col("z")]);

        // a.x=0, a.y=1, b.z=2
        let col = scope.resolve_qualified("b", "z").unwrap();
        assert_eq!(col.column_index, 2);
    }

    #[test]
    fn scope_stack_correlated_subquery() {
        let mut stack = ScopeStack::new();

        // Outer scope: users(id, name)
        let mut outer = Scope::new();
        outer.add_table("users", &[int_col("id"), text_col("name")]);
        stack.push(outer);

        // Inner scope: orders(order_id, user_id)
        let mut inner = Scope::new();
        inner.add_table("orders", &[int_col("order_id"), int_col("user_id")]);
        stack.push(inner);

        // Resolve inner column → depth 0
        let resolved = stack.resolve_column("order_id").unwrap();
        assert_eq!(resolved.scope_depth, 0);
        assert_eq!(resolved.column_index, 0);

        // Resolve outer column → depth 1
        let resolved = stack.resolve_column("name").unwrap();
        assert_eq!(resolved.scope_depth, 1);
        assert_eq!(resolved.column_index, 1);

        // Ambiguous across scopes: inner "id" doesn't exist, outer "id" does
        // (no ambiguity because they're in different scopes)
        let resolved = stack.resolve_column("id").unwrap();
        assert_eq!(resolved.scope_depth, 1);
        assert_eq!(resolved.column_index, 0);
    }

    #[test]
    fn cte_resolution() {
        let mut stack = ScopeStack::new();
        let mut scope = Scope::new();
        scope.add_cte(
            "my_cte",
            vec![
                ("a".to_string(), DataType::Int32),
                ("b".to_string(), DataType::Text),
            ],
        );
        stack.push(scope);

        let cols = stack.resolve_cte("my_cte").unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0], ("a".to_string(), DataType::Int32));

        assert!(stack.resolve_cte("nonexistent").is_none());
    }
}
