//! Scope chain for column resolution during analysis.
//!
//! The Analyzer maintains a stack of `Scope` frames. Each query level (main
//! query, subquery, CTE body) pushes a scope. Column resolution walks from the
//! innermost scope outward, and the depth difference becomes `scope_depth` on
//! the `ColumnRef` node — enabling correlated subquery evaluation without
//! `SubstituteVisitor`.

use crate::model::DataType;
use std::collections::HashMap;
use std::ops::Range;

use super::error::AnalyzerError;
use sqlparser::ast::Ident;

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
    #[allow(dead_code)] // framework: scope resolution field
    pub nullable: bool,
    /// Whether this column is hidden from SELECT * expansion.
    /// Used for USING join columns: the right-side duplicate is hidden.
    pub hidden: bool,
    /// Collation name (if column has a declared collation).
    pub collation: Option<String>,
}

/// Metadata for an unqualified column merged by JOIN ... USING / NATURAL JOIN.
///
/// The merged output column is represented as `COALESCE(left_col, right_col)` for
/// unqualified references (`SELECT id` / `SELECT *`), while qualified references
/// (`left.id`, `right.id`) still target the physical side-specific columns.
#[derive(Debug, Clone)]
pub struct UsingMergedColumn {
    pub column_name: String,
    pub left_index: usize,
    pub right_index: usize,
    pub data_type: DataType,
    pub left_type: DataType,
    pub right_type: DataType,
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
    /// JOIN ... USING / NATURAL merged-column metadata for unqualified refs.
    pub merged_using: Option<UsingMergedColumn>,
    /// Collation name (if column has a declared collation).
    pub collation: Option<String>,
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

    /// Metadata for merged USING/NATURAL columns keyed by left-side column index.
    using_merged_by_left: HashMap<usize, UsingMergedColumn>,

    /// CTE schemas visible from this scope (name → output columns with optional collation name).
    cte_schemas: HashMap<String, Vec<(String, DataType, Option<String>)>>,

    /// Source schemas for each table alias (normalized alias → list of (schema, column_range)).
    /// Keys are stored as-is from the caller (already normalized via `normalize_ident`:
    /// quoted identifiers preserve case, unquoted are lowercased).
    /// A Vec is used because the same unaliased table name can appear from
    /// multiple schemas (e.g. `FROM s1.t, s2.t`). The column range tracks
    /// which scope columns belong to each schema binding, enabling
    /// schema-qualified wildcards to expand only the correct table's columns.
    table_source_schemas: HashMap<String, Vec<(String, Range<usize>)>>,

    /// Source relation names for each table alias (normalized alias →
    /// list of (qualified_relation_name, column_range)).
    ///
    /// Used for targeted compatibility behaviors that depend on relation
    /// identity (for example, pg_namespace system-column handling) without
    /// changing row-width accounting for unrelated queries.
    table_source_relations: HashMap<String, Vec<(String, Range<usize>)>>,

    /// Whether aggregate functions are allowed in expressions at this level.
    pub allow_aggregates: bool,

    /// Whether window functions are allowed in expressions at this level.
    pub allow_windows: bool,

    /// Whether `add_table` should append default synthetic system columns.
    /// Disabled by default; enabled only for DML scopes that need PostgreSQL
    /// compatibility for table-qualified `alias.ctid`.
    add_system_columns: bool,
}

impl Scope {
    fn ident_matches_name(name: &str, ident: &Ident) -> bool {
        if ident.quote_style.is_some() {
            name == ident.value
        } else {
            name == ident.value.to_lowercase()
        }
    }

    /// Create an empty scope.
    pub fn new() -> Self {
        Self {
            columns: Vec::new(),
            column_index: HashMap::new(),
            qualified_index: HashMap::new(),
            using_merged_by_left: HashMap::new(),
            cte_schemas: HashMap::new(),
            table_source_schemas: HashMap::new(),
            table_source_relations: HashMap::new(),
            allow_aggregates: false,
            allow_windows: false,
            add_system_columns: false,
        }
    }

    /// Enable/disable synthetic system column registration for subsequent `add_table` calls.
    pub fn set_add_system_columns(&mut self, enabled: bool) {
        self.add_system_columns = enabled;
    }

    /// Record the source schema and column range for a table alias.
    /// `col_range` is the range of column indices in this scope that belong
    /// to this particular table binding.
    /// Multiple schemas can be recorded for the same alias when the same table
    /// name appears from different schemas (e.g. `FROM s1.t, s2.t`).
    pub fn set_table_source_schema(&mut self, alias: &str, schema: &str, col_range: Range<usize>) {
        self.table_source_schemas
            .entry(alias.to_string())
            .or_default()
            .push((schema.to_lowercase(), col_range));
    }

    /// Look up the source schema entries for a table alias.
    /// Returns `None` if no schema was recorded (CTE, subquery, aliased table).
    /// Returns `Some(slice)` with one or more (schema, column_range) entries otherwise.
    pub fn source_schemas_for_alias(&self, alias: &str) -> Option<&[(String, Range<usize>)]> {
        self.table_source_schemas.get(alias).map(|v| v.as_slice())
    }

    /// Record the source relation and column range for a table alias.
    pub fn set_table_source_relation(
        &mut self,
        alias: &str,
        relation: &str,
        col_range: Range<usize>,
    ) {
        self.table_source_relations
            .entry(alias.to_string())
            .or_default()
            .push((relation.to_lowercase(), col_range));
    }

    fn alias_matches_relation_ident(&self, table: &Ident, relation: &str) -> bool {
        let rel_lower = relation.to_lowercase();
        self.table_source_relations.iter().any(|(alias, entries)| {
            Self::ident_matches_name(alias, table)
                && entries
                    .iter()
                    .any(|(bound_rel, _range)| bound_rel == &rel_lower)
        })
    }

    /// Whether this scope contains any table alias matching `table`.
    fn has_matching_alias(&self, table: &Ident) -> bool {
        self.table_source_relations
            .keys()
            .any(|alias| Self::ident_matches_name(alias, table))
    }

    fn relation_aliases(&self, relation: &str) -> Vec<String> {
        let rel_lower = relation.to_lowercase();
        let mut result = Vec::new();
        for (alias, entries) in &self.table_source_relations {
            let count = entries
                .iter()
                .filter(|(bound_rel, _)| bound_rel == &rel_lower)
                .count();
            for _ in 0..count {
                result.push(alias.clone());
            }
        }
        result
    }

    /// Build a scope from a `TableSchema`, using the table name (or alias) as qualifier.
    ///
    /// Convenience for DML contexts where the target table is already known as a
    /// `TableSchema`. Columns are added in schema order with their catalog types.
    pub fn from_table_schema(alias: &str, schema: &crate::model::TableSchema) -> Self {
        let mut scope = Self::new();
        let cols: Vec<(String, DataType, bool, Option<String>)> = schema
            .columns
            .iter()
            .map(|c| {
                (
                    c.name.clone(),
                    c.data_type.clone(),
                    c.nullable,
                    c.collation.clone(),
                )
            })
            .collect();
        scope.add_table(alias, &cols);
        scope
    }

    /// Add columns from a table to this scope.
    ///
    /// `alias` is the table alias (or real name if no alias). Columns are
    /// appended to the flattened row, with `column_index` set to the absolute
    /// position starting from the current column count.
    pub fn add_table(&mut self, alias: &str, columns: &[(String, DataType, bool, Option<String>)]) {
        self.add_table_internal(alias, columns, self.add_system_columns);
    }

    /// Add a table but force-disable synthetic system columns even when the
    /// scope is in DML mode. Used for derived tables/CTEs/functions whose rows
    /// do not carry physical tuple metadata like `ctid`.
    pub fn add_table_without_system_columns(
        &mut self,
        alias: &str,
        columns: &[(String, DataType, bool, Option<String>)],
    ) {
        self.add_table_internal(alias, columns, false);
    }

    /// Append a hidden, qualified-only column to the current flattened scope row.
    ///
    /// Hidden columns are excluded from unqualified resolution and `SELECT *`
    /// expansion, but remain resolvable via `alias.column`.
    pub fn add_hidden_qualified_column(&mut self, alias: &str, name: &str, data_type: DataType) {
        let abs_index = self.columns.len();
        self.qualified_index
            .insert((alias.to_lowercase(), name.to_lowercase()), abs_index);
        self.columns.push(ScopeColumn {
            table_alias: Some(alias.to_string()),
            column_name: name.to_string(),
            column_index: abs_index,
            data_type,
            nullable: false,
            hidden: true,
            collation: None,
        });
    }

    fn add_table_internal(
        &mut self,
        alias: &str,
        columns: &[(String, DataType, bool, Option<String>)],
        include_system_columns: bool,
    ) {
        let base_offset = self.columns.len();
        for (idx, (name, data_type, nullable, collation)) in columns.iter().enumerate() {
            let abs_index = base_offset + idx;
            let col = ScopeColumn {
                table_alias: Some(alias.to_string()),
                column_name: name.clone(),
                column_index: abs_index,
                data_type: data_type.clone(),
                nullable: *nullable,
                hidden: false,
                collation: collation.clone(),
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

        if include_system_columns {
            // Add synthetic ctid system column (hidden, only accessible via table.ctid).
            self.add_hidden_qualified_column(alias, "ctid", DataType::Int64);
        }
    }

    /// Add a single column to the scope (e.g. for subquery output columns).
    pub fn add_column(
        &mut self,
        alias: Option<&str>,
        name: &str,
        data_type: DataType,
        nullable: bool,
        collation: Option<String>,
    ) {
        let abs_index = self.columns.len();
        let col = ScopeColumn {
            table_alias: alias.map(String::from),
            column_name: name.to_string(),
            column_index: abs_index,
            data_type,
            nullable,
            hidden: false,
            collation,
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
    pub fn add_cte(&mut self, name: &str, columns: Vec<(String, DataType, Option<String>)>) {
        self.cte_schemas.insert(name.to_lowercase(), columns);
    }

    /// Look up a CTE by name.
    pub fn get_cte(&self, name: &str) -> Option<&Vec<(String, DataType, Option<String>)>> {
        self.cte_schemas.get(&name.to_lowercase())
    }

    /// Resolve an unqualified column with SQL identifier semantics.
    ///
    /// Quoted identifiers are exact-match; unquoted identifiers are normalized
    /// to lowercase before matching.
    pub fn resolve_unqualified_with_ident(
        &self,
        ident: &Ident,
    ) -> Result<Option<ResolvedColumnRef>, AnalyzerError> {
        let positions: Vec<usize> = self
            .columns
            .iter()
            // Hidden columns (right-side USING/NATURAL duplicates) are excluded
            // from unqualified resolution, matching PostgreSQL merged-column
            // semantics while preserving qualified access.
            .filter(|c| !c.hidden && Self::ident_matches_name(&c.column_name, ident))
            .map(|c| c.column_index)
            .collect();

        match positions.as_slice() {
            [] => Ok(None),
            [only] => {
                let col = &self.columns[*only];
                Ok(Some(ResolvedColumnRef {
                    scope_depth: 0,
                    column_index: col.column_index,
                    column_name: col.column_name.clone(),
                    data_type: self
                        .using_merged_by_left
                        .get(&col.column_index)
                        .map(|m| m.data_type.clone())
                        .unwrap_or_else(|| col.data_type.clone()),
                    merged_using: self.using_merged_by_left.get(&col.column_index).cloned(),
                    collation: col.collation.clone(),
                }))
            }
            _ => {
                let tables: Vec<String> = positions
                    .iter()
                    .filter_map(|&pos| self.columns[pos].table_alias.clone())
                    .collect();
                Err(AnalyzerError::AmbiguousColumn {
                    name: ident.value.clone(),
                    tables,
                })
            }
        }
    }

    /// Resolve a qualified column with SQL identifier semantics.
    pub fn resolve_qualified_idents(&self, table: &Ident, column: &Ident) -> Option<&ScopeColumn> {
        self.columns.iter().find(|c| {
            c.table_alias
                .as_ref()
                .map(|a| Self::ident_matches_name(a, table))
                .unwrap_or(false)
                && Self::ident_matches_name(&c.column_name, column)
        })
    }

    /// Return all columns for a table alias identified by SQL identifier rules.
    pub fn columns_for_table_alias_ident(&self, table: &Ident) -> Vec<&ScopeColumn> {
        self.columns
            .iter()
            .filter(|c| {
                c.table_alias
                    .as_ref()
                    .map(|a| Self::ident_matches_name(a, table))
                    .unwrap_or(false)
            })
            .collect()
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

    /// Return all columns in this scope.
    pub fn columns(&self) -> &[ScopeColumn] {
        &self.columns
    }

    /// Mark a USING join right-side column as hidden.
    ///
    /// This hides the column from SELECT * expansion and removes it from
    /// the unqualified index (so `id` resolves unambiguously to the left side).
    pub fn hide_using_column(&mut self, index: usize) {
        if let Some(col) = self.columns.get_mut(index) {
            col.hidden = true;
            // Remove from unqualified index to avoid ambiguity.
            let lower = col.column_name.to_lowercase();
            if let Some(positions) = self.column_index.get_mut(&lower) {
                positions.retain(|&pos| pos != index);
            }
        }
    }

    /// Register a USING/NATURAL merged column.
    ///
    /// - Hides the right-side duplicate from unqualified `SELECT *`
    /// - Keeps both sides addressable through qualified refs
    /// - Records metadata so unqualified refs can be analyzed as COALESCE(left, right)
    pub fn register_using_column(
        &mut self,
        name: &str,
        left_index: usize,
        right_index: usize,
        data_type: DataType,
        left_type: DataType,
        right_type: DataType,
    ) {
        self.hide_using_column(right_index);
        self.using_merged_by_left.insert(
            left_index,
            UsingMergedColumn {
                column_name: name.to_string(),
                left_index,
                right_index,
                data_type,
                left_type,
                right_type,
            },
        );
    }

    pub fn using_column_for_left(&self, left_index: usize) -> Option<&UsingMergedColumn> {
        self.using_merged_by_left.get(&left_index)
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

    /// Resolve a column reference with SQL identifier semantics.
    pub fn resolve_column_ident(&self, ident: &Ident) -> Result<ResolvedColumnRef, AnalyzerError> {
        for (i, scope) in self.scopes.iter().rev().enumerate() {
            match scope.resolve_unqualified_with_ident(ident)? {
                Some(mut resolved) => {
                    resolved.scope_depth = i as u32;
                    return Ok(ResolvedColumnRef {
                        scope_depth: resolved.scope_depth,
                        column_index: resolved.column_index,
                        column_name: resolved.column_name,
                        data_type: resolved.data_type,
                        merged_using: resolved.merged_using,
                        collation: resolved.collation,
                    });
                }
                None => continue,
            }
        }

        let available = if let Some(scope) = self.scopes.last() {
            scope.available_columns()
        } else {
            vec![]
        };

        Err(AnalyzerError::ColumnNotFound {
            name: ident.value.clone(),
            available,
        })
    }

    pub fn resolve_qualified_column_idents(
        &self,
        table: &Ident,
        column: &Ident,
    ) -> Result<ResolvedColumnRef, AnalyzerError> {
        for (i, scope) in self.scopes.iter().rev().enumerate() {
            if let Some(col) = scope.resolve_qualified_idents(table, column) {
                return Ok(ResolvedColumnRef {
                    scope_depth: i as u32,
                    column_index: col.column_index,
                    column_name: col.column_name.clone(),
                    data_type: col.data_type.clone(),
                    merged_using: None,
                    collation: col.collation.clone(),
                });
            }
        }

        let available = if let Some(scope) = self.scopes.last() {
            scope.available_columns()
        } else {
            vec![]
        };

        Err(AnalyzerError::ColumnNotFound {
            name: format!("{}.{}", table.value, column.value),
            available,
        })
    }

    /// Resolve table alias (row variable) columns by SQL identifier semantics.
    pub fn resolve_table_alias_columns(&self, table: &Ident) -> Option<(u32, Vec<ScopeColumn>)> {
        for (i, scope) in self.scopes.iter().rev().enumerate() {
            let cols = scope.columns_for_table_alias_ident(table);
            if !cols.is_empty() {
                return Some((i as u32, cols.into_iter().cloned().collect()));
            }
        }
        None
    }

    /// Look up a CTE by name in any scope (innermost first).
    pub fn resolve_cte(&self, name: &str) -> Option<Vec<(String, DataType, Option<String>)>> {
        for scope in self.scopes.iter().rev() {
            if let Some(cols) = scope.get_cte(name) {
                return Some(cols.clone());
            }
        }
        None
    }

    /// Return whether `table` resolves to `relation` in the nearest scope
    /// that contains a matching alias. Stops at the first scope with the
    /// alias so that inner-scope aliases shadow outer ones.
    pub fn table_ident_matches_relation(&self, table: &Ident, relation: &str) -> bool {
        for scope in self.scopes.iter().rev() {
            if scope.has_matching_alias(table) {
                return scope.alias_matches_relation_ident(table, relation);
            }
        }
        false
    }

    /// Resolve relation-backed synthetic unqualified columns at the nearest scope.
    ///
    /// Returns:
    /// - `Ok(Some(depth))` when exactly one matching relation binding exists
    ///   in the nearest scope that has any such binding.
    /// - `Err(AmbiguousColumn)` when multiple bindings exist in that nearest scope.
    /// - `Ok(None)` when no matching relation exists in visible scopes.
    pub fn resolve_unqualified_relation_column_scope_depth(
        &self,
        ident_name: &str,
        relation: &str,
    ) -> Result<Option<u32>, AnalyzerError> {
        for (depth, scope) in self.scopes.iter().rev().enumerate() {
            let aliases = scope.relation_aliases(relation);
            if aliases.is_empty() {
                continue;
            }
            if aliases.len() > 1 {
                return Err(AnalyzerError::AmbiguousColumn {
                    name: ident_name.to_string(),
                    tables: aliases,
                });
            }
            return Ok(Some(depth as u32));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int_col(name: &str) -> (String, DataType, bool, Option<String>) {
        (name.to_string(), DataType::Int32, false, None)
    }

    fn text_col(name: &str) -> (String, DataType, bool, Option<String>) {
        (name.to_string(), DataType::Text, true, None)
    }

    #[test]
    fn single_table_ident_resolution() {
        use sqlparser::ast::Ident;

        let mut scope = Scope::new();
        scope.add_table("users", &[int_col("id"), text_col("name")]);

        let resolved = scope
            .resolve_unqualified_with_ident(&Ident::new("id"))
            .unwrap()
            .unwrap();
        assert_eq!(resolved.column_index, 0);
        assert_eq!(resolved.data_type, DataType::Int32);

        let resolved = scope
            .resolve_unqualified_with_ident(&Ident::new("name"))
            .unwrap()
            .unwrap();
        assert_eq!(resolved.column_index, 1);
        assert_eq!(resolved.data_type, DataType::Text);

        assert!(scope
            .resolve_unqualified_with_ident(&Ident::new("nonexistent"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn case_insensitive_ident_resolution() {
        use sqlparser::ast::Ident;

        let mut scope = Scope::new();
        scope.add_table("t", &[int_col("age")]);

        // Unquoted idents are case-insensitive
        assert!(scope
            .resolve_unqualified_with_ident(&Ident::new("age"))
            .unwrap()
            .is_some());
        assert!(scope
            .resolve_unqualified_with_ident(&Ident::new("AGE"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn system_columns_are_opt_in() {
        use sqlparser::ast::Ident;

        let mut scope = Scope::new();
        scope.add_table("t", &[int_col("id")]);
        assert!(scope
            .resolve_qualified_idents(&Ident::new("t"), &Ident::new("ctid"))
            .is_none());

        let mut dml_scope = Scope::new();
        dml_scope.set_add_system_columns(true);
        dml_scope.add_table("t", &[int_col("id")]);
        let ctid = dml_scope
            .resolve_qualified_idents(&Ident::new("t"), &Ident::new("ctid"))
            .expect("ctid should be visible when system columns are enabled");
        assert_eq!(ctid.column_name, "ctid");
        assert_eq!(ctid.column_index, 1);
        assert_eq!(ctid.data_type, DataType::Int64);
    }

    #[test]
    fn hidden_qualified_column_is_not_visible_to_unqualified_or_star() {
        use sqlparser::ast::Ident;

        let mut scope = Scope::new();
        scope.add_table("n", &[int_col("oid"), text_col("nspname")]);
        scope.add_hidden_qualified_column("n", "xmin", DataType::Int64);

        // Hidden system column is available via alias-qualified access.
        let xmin = scope
            .resolve_qualified_idents(&Ident::new("n"), &Ident::new("xmin"))
            .expect("qualified hidden system column should resolve");
        assert_eq!(xmin.column_index, 2);
        assert_eq!(xmin.data_type, DataType::Int64);

        // Hidden system column is not available to unqualified resolution.
        assert!(scope
            .resolve_unqualified_with_ident(&Ident::new("xmin"))
            .unwrap()
            .is_none());

        // Hidden columns must stay excluded from the visible column set.
        let names: Vec<String> = scope
            .columns
            .iter()
            .filter(|c| !c.hidden)
            .map(|c| c.column_name.clone())
            .collect();
        assert_eq!(names, vec!["oid".to_string(), "nspname".to_string()]);
    }

    #[test]
    fn ambiguous_ident_column() {
        use sqlparser::ast::Ident;

        let mut scope = Scope::new();
        scope.add_table("a", &[int_col("id"), text_col("x")]);
        scope.add_table("b", &[int_col("id"), text_col("y")]);

        let err = scope
            .resolve_unqualified_with_ident(&Ident::new("id"))
            .unwrap_err();
        assert!(matches!(err, AnalyzerError::AmbiguousColumn { .. }));

        // Non-ambiguous columns still resolve
        let col = scope
            .resolve_unqualified_with_ident(&Ident::new("x"))
            .unwrap()
            .unwrap();
        assert_eq!(col.column_index, 1);
    }

    #[test]
    fn qualified_ident_resolution() {
        use sqlparser::ast::Ident;

        let mut scope = Scope::new();
        scope.add_table("a", &[int_col("id")]);
        scope.add_table("b", &[int_col("id")]);

        let col = scope
            .resolve_qualified_idents(&Ident::new("a"), &Ident::new("id"))
            .unwrap();
        assert_eq!(col.column_index, 0);

        let col = scope
            .resolve_qualified_idents(&Ident::new("b"), &Ident::new("id"))
            .unwrap();
        assert_eq!(col.column_index, 1);

        assert!(scope
            .resolve_qualified_idents(&Ident::new("c"), &Ident::new("id"))
            .is_none());
    }

    #[test]
    fn flattened_row_offsets() {
        use sqlparser::ast::Ident;

        let mut scope = Scope::new();
        scope.add_table("a", &[int_col("x"), int_col("y")]);
        scope.add_table("b", &[int_col("z")]);

        // a.x=0, a.y=1, b.z=2
        let col = scope
            .resolve_qualified_idents(&Ident::new("b"), &Ident::new("z"))
            .unwrap();
        assert_eq!(col.column_index, 2);
    }

    #[test]
    fn scope_stack_ident_correlated_subquery() {
        use sqlparser::ast::Ident;

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
        let resolved = stack.resolve_column_ident(&Ident::new("order_id")).unwrap();
        assert_eq!(resolved.scope_depth, 0);
        assert_eq!(resolved.column_index, 0);

        // Resolve outer column → depth 1
        let resolved = stack.resolve_column_ident(&Ident::new("name")).unwrap();
        assert_eq!(resolved.scope_depth, 1);
        assert_eq!(resolved.column_index, 1);

        // Resolve from outer scope when inner doesn't have it
        let resolved = stack.resolve_column_ident(&Ident::new("id")).unwrap();
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
                ("a".to_string(), DataType::Int32, None),
                ("b".to_string(), DataType::Text, None),
            ],
        );
        stack.push(scope);

        let cols = stack.resolve_cte("my_cte").unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0], ("a".to_string(), DataType::Int32, None));

        assert!(stack.resolve_cte("nonexistent").is_none());
    }

    #[test]
    fn using_hidden_column_not_ambiguous_for_ident_resolution() {
        use sqlparser::ast::Ident;

        let mut scope = Scope::new();
        scope.add_table("l", &[int_col("id"), text_col("x")]);
        scope.add_table("r", &[int_col("id"), text_col("y")]);

        // Simulate JOIN ... USING(id): hide right duplicate and register merge.
        scope.register_using_column(
            "id",
            0,
            2,
            DataType::Int32,
            DataType::Int32,
            DataType::Int32,
        );

        let resolved = scope
            .resolve_unqualified_with_ident(&Ident::new("id"))
            .expect("resolution should not error")
            .expect("id should resolve");

        assert_eq!(resolved.column_index, 0);
        assert!(resolved.merged_using.is_some());
        assert_eq!(resolved.data_type, DataType::Int32);
    }

    #[test]
    fn source_schema_tracks_table_origin() {
        let mut scope = Scope::new();
        scope.add_table("t", &[int_col("id")]);
        scope.set_table_source_schema("t", "myschema", 0..1);

        let entries = scope.source_schemas_for_alias("t").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "myschema");
        assert_eq!(entries[0].1, 0..1);

        // Keys are stored as-is (caller is responsible for normalization).
        // "T" (uppercase) does not match key "t" (lowercase).
        assert!(scope.source_schemas_for_alias("T").is_none());
        assert_eq!(scope.source_schemas_for_alias("other"), None);
    }

    #[test]
    fn source_schema_quoted_case_preserved() {
        // Quoted "T" and unquoted t are distinct alias keys.
        let mut scope = Scope::new();
        scope.add_table("T", &[int_col("a"), text_col("b")]);
        scope.set_table_source_schema("T", "public", 0..2);
        scope.add_table("t", &[int_col("x")]);
        scope.set_table_source_schema("t", "public", 2..3);

        // "T" and "t" must not be conflated.
        let upper = scope
            .source_schemas_for_alias("T")
            .expect("quoted T should have its own entry");
        assert_eq!(upper.len(), 1);
        assert_eq!(upper[0].1, 0..2);

        let lower = scope
            .source_schemas_for_alias("t")
            .expect("unquoted t should have its own entry");
        assert_eq!(lower.len(), 1);
        assert_eq!(lower[0].1, 2..3);
    }

    #[test]
    fn source_schema_multi_value_tracks_all_schemas() {
        let mut scope = Scope::new();
        scope.add_table("t", &[int_col("id")]);
        scope.set_table_source_schema("t", "s1", 0..1);
        scope.add_table("t", &[int_col("val")]);
        scope.set_table_source_schema("t", "s2", 1..2);

        let entries = scope
            .source_schemas_for_alias("t")
            .expect("should have schemas");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "s1");
        assert_eq!(entries[0].1, 0..1);
        assert_eq!(entries[1].0, "s2");
        assert_eq!(entries[1].1, 1..2);
    }

    #[test]
    fn scope_shadowing_stops_at_nearest_alias() {
        use sqlparser::ast::Ident;

        let mut stack = ScopeStack::new();

        // Outer scope: alias "n" bound to pg_catalog.pg_namespace
        let mut outer = Scope::new();
        outer.add_table("n", &[int_col("oid"), text_col("nspname")]);
        outer.set_table_source_relation("n", "pg_catalog.pg_namespace", 0..2);
        stack.push(outer);

        // Inner scope: alias "n" bound to a different relation (subquery)
        let mut inner = Scope::new();
        inner.add_table("n", &[int_col("oid")]);
        inner.set_table_source_relation("n", "subquery", 0..1);
        stack.push(inner);

        // "n" should NOT match pg_namespace — inner scope shadows the outer.
        assert!(!stack.table_ident_matches_relation(&Ident::new("n"), "pg_catalog.pg_namespace"));

        // "n" should match the inner relation.
        assert!(stack.table_ident_matches_relation(&Ident::new("n"), "subquery"));
    }

    #[test]
    fn same_alias_multi_binding_is_ambiguous() {
        let mut scope = Scope::new();
        // Simulate FROM s1.pg_namespace, s2.pg_namespace (no explicit aliases)
        scope.add_table("pg_namespace", &[int_col("oid")]);
        scope.set_table_source_relation("pg_namespace", "pg_catalog.pg_namespace", 0..1);
        scope.add_table("pg_namespace", &[int_col("oid")]);
        scope.set_table_source_relation("pg_namespace", "pg_catalog.pg_namespace", 1..2);

        let aliases = scope.relation_aliases("pg_catalog.pg_namespace");
        assert_eq!(
            aliases.len(),
            2,
            "should count both bindings, not just unique keys"
        );

        // The ambiguity check in resolve_unqualified_relation_column_scope_depth
        // relies on aliases.len() > 1.
        let mut stack = ScopeStack::new();
        stack.push(scope);
        let result = stack
            .resolve_unqualified_relation_column_scope_depth("xmin", "pg_catalog.pg_namespace");
        assert!(
            matches!(result, Err(AnalyzerError::AmbiguousColumn { .. })),
            "same alias with multiple relation bindings should be ambiguous"
        );
    }

    #[test]
    fn derived_table_alias_shadows_outer_relation() {
        use sqlparser::ast::Ident;

        let mut stack = ScopeStack::new();

        // Outer scope: alias "n" bound to pg_catalog.pg_namespace
        let mut outer = Scope::new();
        outer.add_table("n", &[int_col("oid"), text_col("nspname")]);
        outer.set_table_source_relation("n", "pg_catalog.pg_namespace", 0..2);
        stack.push(outer);

        // Inner scope: alias "n" from a derived table (empty sentinel relation)
        let mut inner = Scope::new();
        inner.add_table_without_system_columns("n", &[int_col("oid")]);
        inner.set_table_source_relation("n", "", 0..1);
        stack.push(inner);

        // Inner "n" (derived table) should shadow outer "n" (pg_namespace).
        assert!(
            !stack.table_ident_matches_relation(&Ident::new("n"), "pg_catalog.pg_namespace"),
            "derived table alias must shadow outer pg_namespace binding"
        );

        // Inner "n" should NOT match pg_namespace relation.
        assert!(
            stack.table_ident_matches_relation(&Ident::new("n"), ""),
            "derived table alias should match its empty sentinel relation"
        );
    }
}
