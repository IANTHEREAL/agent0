//! Catalog interface and snapshot implementation.
//!
//! The Analyzer uses a synchronous `Catalog` trait for name resolution. The
//! `CatalogSnapshot` implementation is built by pre-fetching all referenced
//! relations from TiKV before analysis begins (async fetch → sync analysis).

use crate::model::{ColumnDef, DataType, FunctionDef, TableSchema, UserTypeDef, ViewDef};
use crate::sql::collation::CollationDef;
use std::collections::{HashMap, HashSet};

// ── Catalog trait ───────────────────────────────────────────

/// Error from catalog operations.
#[derive(Debug, Clone)]
pub enum CatalogError {
    /// Catalog data is inconsistent or corrupted.
    #[allow(dead_code)] // framework: catalog trait API
    Internal(String),
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Internal(msg) => write!(f, "catalog error: {}", msg),
        }
    }
}

impl std::error::Error for CatalogError {}

/// Synchronous catalog interface for the Analyzer.
///
/// All methods are sync — they operate on pre-fetched data. The actual async
/// fetching from TiKV happens before the Analyzer runs (see `CatalogSnapshot`).
pub trait Catalog: Send + Sync {
    /// Resolve a table by name, optionally schema-qualified.
    /// Returns (qualified_name, schema) if found.
    fn resolve_table(
        &self,
        name: &str,
        schema: Option<&str>,
    ) -> Result<Option<(String, TableSchema)>, CatalogError>;

    /// Resolve a view by name, optionally schema-qualified.
    #[allow(dead_code)] // framework: catalog trait API
    fn resolve_view(
        &self,
        name: &str,
        schema: Option<&str>,
    ) -> Result<Option<ViewDef>, CatalogError>;

    /// Resolve a user-defined function by name and argument types.
    fn resolve_function(
        &self,
        name: &str,
        schema: Option<&str>,
        arg_types: &[DataType],
    ) -> Result<Option<FunctionDef>, CatalogError>;

    /// Resolve a user-defined type by name.
    #[allow(dead_code)] // framework: catalog trait API
    fn resolve_type(
        &self,
        name: &str,
        schema: Option<&str>,
    ) -> Result<Option<UserTypeDef>, CatalogError>;

    /// Resolve the schema for a table-valued function call used in FROM.
    ///
    /// The key is a stable signature string computed from the raw sqlparser AST
    /// (function name + argument expressions). It is populated during the async
    /// prefetch phase so the Analyzer can stay synchronous.
    fn resolve_table_function(&self, key: &str) -> Option<&TableSchema>;

    /// The current search path (ordered list of schema names).
    #[allow(dead_code)] // framework: catalog trait API
    fn search_path(&self) -> &[String];

    /// The current database ID (for scope isolation).
    #[allow(dead_code)] // framework: catalog trait API
    fn database_id(&self) -> u64;

    /// Get a collation definition by name.
    fn get_collation(&self, name: &str) -> Option<CollationDef>;
}

// ── CatalogSnapshot ─────────────────────────────────────────

/// In-memory catalog snapshot built by pre-fetching referenced relations.
///
/// This is the primary `Catalog` implementation used during analysis.
/// The pre-fetch phase walks the raw AST to extract all referenced names,
/// batch-fetches their schemas from TiKV, and populates this snapshot.
///
/// All lookups are O(1) hash map operations.
#[derive(Debug, Clone)]
pub struct CatalogSnapshot {
    tables: HashMap<String, (String, TableSchema)>,
    table_functions: HashMap<String, TableSchema>,
    #[allow(dead_code)] // forward-compat: view-aware Analyzer path
    views: HashMap<String, ViewDef>,
    functions: HashMap<String, FunctionDef>,
    #[allow(dead_code)] // forward-compat: user-defined type resolution
    types: HashMap<String, UserTypeDef>,
    collations: HashMap<String, CollationDef>,
    search_path: Vec<String>,
    #[allow(dead_code)] // forward-compat: cross-database query isolation
    database_id: u64,
    /// Qualified names that are NOT base tables (CTEs, virtual catalog tables).
    /// Used by `base_table_full_names()` to exclude non-privileged entries.
    non_base_names: HashSet<String>,
}

impl CatalogSnapshot {
    /// Create a new empty snapshot with the given context.
    pub fn new(search_path: Vec<String>, database_id: u64) -> Self {
        Self {
            tables: HashMap::new(),
            table_functions: HashMap::new(),
            views: HashMap::new(),
            functions: HashMap::new(),
            types: HashMap::new(),
            collations: HashMap::new(),
            search_path,
            database_id,
            non_base_names: HashSet::new(),
        }
    }

    /// Add a table to the snapshot.
    pub fn add_table(&mut self, name: &str, qualified_name: String, schema: TableSchema) {
        self.tables
            .insert(name.to_lowercase(), (qualified_name, schema));
    }

    /// Add a table function schema under a stable signature key.
    pub fn add_table_function(&mut self, key: &str, schema: TableSchema) {
        self.table_functions.insert(key.to_string(), schema);
    }

    /// Add a view to the snapshot.
    #[allow(dead_code)] // forward-compat: view-aware Analyzer path
    pub fn add_view(&mut self, name: &str, view: ViewDef) {
        self.views.insert(name.to_lowercase(), view);
    }

    /// Add a user-defined function to the snapshot.
    #[allow(dead_code)] // forward-compat: UDF prefetch
    pub fn add_function(&mut self, name: &str, func: FunctionDef) {
        self.functions.insert(name.to_lowercase(), func);
    }

    /// Add a user-defined type to the snapshot.
    #[allow(dead_code)] // forward-compat: user-defined type resolution
    pub fn add_type(&mut self, name: &str, udt: UserTypeDef) {
        self.types.insert(name.to_lowercase(), udt);
    }

    /// Add a user-defined collation to the snapshot.
    pub fn add_collation(&mut self, name: &str, def: CollationDef) {
        self.collations.insert(name.to_lowercase(), def);
    }

    /// Check if a table name is already in the snapshot.
    pub fn has_table(&self, name: &str) -> bool {
        self.tables.contains_key(&name.to_lowercase())
    }

    /// Mark a qualified name as non-base (CTE alias or virtual catalog table).
    /// These entries are visible for analysis but excluded from privilege checks.
    pub fn mark_non_base(&mut self, qualified_name: &str) {
        self.non_base_names.insert(qualified_name.to_string());
    }

    /// Return deduplicated fully-qualified names of all real (non-virtual) tables
    /// in the snapshot.  Used for privilege checking — callers can iterate
    /// the result and call `require_table_privilege()` per entry.
    pub fn base_table_full_names(&self) -> Vec<&str> {
        let mut seen = HashSet::new();
        let mut result = Vec::new();
        for (qualified_name, _schema) in self.tables.values() {
            // Skip entries explicitly marked as non-base (CTEs, virtual tables).
            if self.non_base_names.contains(qualified_name.as_str()) {
                continue;
            }
            // Skip schema-prefixed virtual catalog tables.
            if qualified_name.starts_with("information_schema.")
                || qualified_name.starts_with("pg_catalog.")
            {
                continue;
            }
            if seen.insert(qualified_name.as_str()) {
                result.push(qualified_name.as_str());
            }
        }
        result
    }

    /// Return deduplicated `(qualified_name, table_id, schema_version)` for all
    /// real (non-virtual) base tables in the snapshot.
    ///
    /// The `table_id` is stable across schema mutations and used by the plan
    /// cache for invalidation (not name-only matching, which breaks across
    /// DROP+CREATE of the same table name).
    pub fn base_table_versions(&self) -> Vec<(String, u64, u64)> {
        let mut seen = HashSet::new();
        let mut result = Vec::new();
        for (qualified_name, schema) in self.tables.values() {
            // Skip entries explicitly marked as non-base (CTEs, virtual tables).
            if self.non_base_names.contains(qualified_name.as_str()) {
                continue;
            }
            // Skip schema-prefixed virtual catalog tables.
            if qualified_name.starts_with("information_schema.")
                || qualified_name.starts_with("pg_catalog.")
            {
                continue;
            }
            if seen.insert(qualified_name.as_str()) {
                result.push((qualified_name.clone(), schema.table_id, schema.version));
            }
        }
        result
    }

    /// Merge tables from another snapshot into this one (for DML + subquery).
    pub fn merge_from(&mut self, other: &CatalogSnapshot) {
        for (key, (qualified, schema)) in &other.tables {
            self.tables
                .entry(key.clone())
                .or_insert_with(|| (qualified.clone(), schema.clone()));
        }
        for (key, schema) in &other.table_functions {
            self.table_functions
                .entry(key.clone())
                .or_insert_with(|| schema.clone());
        }
    }

    /// Resolve a name against the search path, returning the first matching key.
    ///
    /// If `schema` is provided, only `schema.name` is tried (PostgreSQL semantics:
    /// explicit schema bypasses search_path). If `schema` is None, tries the bare
    /// name first, then each schema in search_path order.
    fn resolve_name<'a, V>(
        &self,
        map: &'a HashMap<String, V>,
        name: &str,
        schema: Option<&str>,
    ) -> Option<&'a V> {
        let lower = name.to_lowercase();

        if let Some(schema_name) = schema {
            // Schema-qualified: ONLY try "schema.name" — do not fall through.
            let key = format!("{}.{}", schema_name.to_lowercase(), lower);
            return map.get(&key);
        }

        // Unqualified: try bare name first
        if let Some(v) = map.get(&lower) {
            return Some(v);
        }

        // Then try each schema in search_path
        for sp in &self.search_path {
            let key = format!("{}.{}", sp.to_lowercase(), lower);
            if let Some(v) = map.get(&key) {
                return Some(v);
            }
        }

        None
    }
}

impl Catalog for CatalogSnapshot {
    fn resolve_table(
        &self,
        name: &str,
        schema: Option<&str>,
    ) -> Result<Option<(String, TableSchema)>, CatalogError> {
        Ok(self.resolve_name(&self.tables, name, schema).cloned())
    }

    fn resolve_view(
        &self,
        name: &str,
        schema: Option<&str>,
    ) -> Result<Option<ViewDef>, CatalogError> {
        Ok(self.resolve_name(&self.views, name, schema).cloned())
    }

    fn resolve_function(
        &self,
        name: &str,
        schema: Option<&str>,
        _arg_types: &[DataType],
    ) -> Result<Option<FunctionDef>, CatalogError> {
        Ok(self.resolve_name(&self.functions, name, schema).cloned())
    }

    fn resolve_type(
        &self,
        name: &str,
        schema: Option<&str>,
    ) -> Result<Option<UserTypeDef>, CatalogError> {
        Ok(self.resolve_name(&self.types, name, schema).cloned())
    }

    fn resolve_table_function(&self, key: &str) -> Option<&TableSchema> {
        self.table_functions.get(key)
    }

    fn search_path(&self) -> &[String] {
        &self.search_path
    }

    fn database_id(&self) -> u64 {
        self.database_id
    }

    fn get_collation(&self, name: &str) -> Option<CollationDef> {
        let lower = name.to_lowercase();
        // Built-in collations
        match lower.as_str() {
            "c" => Some(CollationDef {
                name: "C".to_string(),
                provider: "c".to_string(),
                locale: Some("C".to_string()),
                deterministic: true,
            }),
            "posix" => Some(CollationDef {
                name: "POSIX".to_string(),
                provider: "c".to_string(),
                locale: Some("POSIX".to_string()),
                deterministic: true,
            }),
            "default" => Some(CollationDef {
                name: "default".to_string(),
                provider: "d".to_string(),
                locale: None,
                deterministic: true,
            }),
            _ => self.collations.get(&lower).cloned(),
        }
    }
}

// ── NullCatalog ─────────────────────────────────────────────

/// A catalog that resolves nothing — for evaluating constant expressions.
///
/// Used by the bridge module to evaluate AST expressions that have no
/// table/function context (literals, arithmetic, casts, etc.).
#[derive(Debug, Clone, Copy)]
pub struct NullCatalog;

impl Catalog for NullCatalog {
    fn resolve_table(
        &self,
        _name: &str,
        _schema: Option<&str>,
    ) -> Result<Option<(String, TableSchema)>, CatalogError> {
        Ok(None)
    }

    fn resolve_view(
        &self,
        _name: &str,
        _schema: Option<&str>,
    ) -> Result<Option<ViewDef>, CatalogError> {
        Ok(None)
    }

    fn resolve_function(
        &self,
        _name: &str,
        _schema: Option<&str>,
        _arg_types: &[DataType],
    ) -> Result<Option<FunctionDef>, CatalogError> {
        Ok(None)
    }

    fn resolve_type(
        &self,
        _name: &str,
        _schema: Option<&str>,
    ) -> Result<Option<UserTypeDef>, CatalogError> {
        Ok(None)
    }

    fn resolve_table_function(&self, _key: &str) -> Option<&TableSchema> {
        None
    }

    fn search_path(&self) -> &[String] {
        &[]
    }

    fn database_id(&self) -> u64 {
        0
    }

    fn get_collation(&self, _name: &str) -> Option<CollationDef> {
        None
    }
}

// ── MockCatalog (for testing) ───────────────────────────────

/// A simple in-memory catalog for unit testing.
///
/// Build with `MockCatalog::builder()` to fluently add tables.
// Test infrastructure -- will be wired up when analyzer tests expand.
#[allow(dead_code)] // test: mock catalog implementation
#[derive(Debug, Clone)]
pub struct MockCatalog {
    snapshot: CatalogSnapshot,
}

#[allow(dead_code)] // test: mock catalog implementation
impl MockCatalog {
    pub fn builder() -> MockCatalogBuilder {
        MockCatalogBuilder {
            snapshot: CatalogSnapshot::new(vec!["public".to_string()], 1),
        }
    }

    /// Create an empty mock catalog.
    pub fn empty() -> Self {
        Self {
            snapshot: CatalogSnapshot::new(vec!["public".to_string()], 1),
        }
    }
}

impl Catalog for MockCatalog {
    fn resolve_table(
        &self,
        name: &str,
        schema: Option<&str>,
    ) -> Result<Option<(String, TableSchema)>, CatalogError> {
        self.snapshot.resolve_table(name, schema)
    }

    fn resolve_view(
        &self,
        name: &str,
        schema: Option<&str>,
    ) -> Result<Option<ViewDef>, CatalogError> {
        self.snapshot.resolve_view(name, schema)
    }

    fn resolve_function(
        &self,
        name: &str,
        schema: Option<&str>,
        arg_types: &[DataType],
    ) -> Result<Option<FunctionDef>, CatalogError> {
        self.snapshot.resolve_function(name, schema, arg_types)
    }

    fn resolve_type(
        &self,
        name: &str,
        schema: Option<&str>,
    ) -> Result<Option<UserTypeDef>, CatalogError> {
        self.snapshot.resolve_type(name, schema)
    }

    fn resolve_table_function(&self, key: &str) -> Option<&TableSchema> {
        self.snapshot.resolve_table_function(key)
    }

    fn search_path(&self) -> &[String] {
        self.snapshot.search_path()
    }

    fn database_id(&self) -> u64 {
        self.snapshot.database_id()
    }

    fn get_collation(&self, name: &str) -> Option<CollationDef> {
        self.snapshot.get_collation(name)
    }
}

/// Builder for `MockCatalog`.
// Test infrastructure -- will be wired up when analyzer tests expand.
#[allow(dead_code)] // test: mock catalog implementation
pub struct MockCatalogBuilder {
    snapshot: CatalogSnapshot,
}

#[allow(dead_code)] // test: mock catalog implementation
impl MockCatalogBuilder {
    /// Add a table with given columns: `(name, type, nullable)`.
    pub fn table(mut self, name: &str, columns: Vec<(&str, DataType, bool)>) -> Self {
        let col_defs: Vec<ColumnDef> = columns
            .iter()
            .map(|(n, dt, nullable)| ColumnDef {
                name: n.to_string(),
                data_type: dt.clone(),
                nullable: *nullable,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            })
            .collect();

        let schema = TableSchema::new(
            format!("public.{}", name),
            1, // dummy table_id
            col_defs,
            vec![], // no pk for test
        );

        self.snapshot
            .add_table(name, format!("public.{}", name), schema);
        self
    }

    /// Add a table in a specific schema (e.g. schema "s1", name "t" → key "s1.t").
    pub fn table_in_schema(
        mut self,
        schema_name: &str,
        name: &str,
        columns: Vec<(&str, DataType, bool)>,
    ) -> Self {
        let col_defs: Vec<ColumnDef> = columns
            .iter()
            .map(|(n, dt, nullable)| ColumnDef {
                name: n.to_string(),
                data_type: dt.clone(),
                nullable: *nullable,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            })
            .collect();

        let qualified_name = format!("{}.{}", schema_name, name);
        let schema = TableSchema::new(
            qualified_name.clone(),
            1, // dummy table_id
            col_defs,
            vec![], // no pk for test
        );

        self.snapshot
            .add_table(&qualified_name, qualified_name.clone(), schema);
        self
    }

    /// Add a table with columns that may have collation:
    /// `(name, type, nullable, collation)`.
    pub fn table_with_collations(
        mut self,
        name: &str,
        columns: Vec<(&str, DataType, bool, Option<&str>)>,
    ) -> Self {
        let col_defs: Vec<ColumnDef> = columns
            .iter()
            .map(|(n, dt, nullable, coll)| ColumnDef {
                name: n.to_string(),
                data_type: dt.clone(),
                nullable: *nullable,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: coll.map(|s| s.to_string()),
            })
            .collect();

        let schema = TableSchema::new(
            format!("public.{}", name),
            1, // dummy table_id
            col_defs,
            vec![], // no pk for test
        );

        self.snapshot
            .add_table(name, format!("public.{}", name), schema);
        self
    }

    /// Register a user-defined collation.
    pub fn collation(mut self, name: &str, locale: &str) -> Self {
        self.snapshot.add_collation(
            name,
            crate::sql::collation::CollationDef {
                name: name.to_string(),
                provider: "icu".to_string(),
                locale: Some(locale.to_string()),
                deterministic: true,
            },
        );
        self
    }

    pub fn build(self) -> MockCatalog {
        MockCatalog {
            snapshot: self.snapshot,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_catalog_basic() {
        let catalog = MockCatalog::builder()
            .table(
                "users",
                vec![
                    ("id", DataType::Int32, false),
                    ("name", DataType::Text, true),
                ],
            )
            .build();

        let (qname, schema) = catalog
            .resolve_table("users", None)
            .unwrap()
            .expect("table should exist");

        assert_eq!(qname, "public.users");
        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.columns[0].name, "id");
        assert_eq!(schema.columns[0].data_type, DataType::Int32);
    }

    #[test]
    fn catalog_snapshot_search_path() {
        let mut snapshot = CatalogSnapshot::new(vec!["public".to_string()], 1);

        let schema = TableSchema::new(
            "public.test".to_string(),
            1,
            vec![ColumnDef {
                name: "x".to_string(),
                data_type: DataType::Int32,
                nullable: false,
                primary_key: false,
                unique: false,
                is_serial: false,
                default_expr: None,
                collation: None,
            }],
            vec![],
        );

        snapshot.add_table("public.test", "public.test".to_string(), schema);

        // Resolve via search_path
        let result = snapshot.resolve_table("test", None).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn catalog_not_found() {
        let catalog = MockCatalog::empty();
        assert!(catalog
            .resolve_table("nonexistent", None)
            .unwrap()
            .is_none());
    }
}
