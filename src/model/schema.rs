use serde::{Deserialize, Serialize};

use super::data_type::DataType;
use super::default_owner;
use super::value::Value;
use crate::worker::types::IndexState;

/// Column definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    pub primary_key: bool,
    pub unique: bool,
    pub is_serial: bool,
    pub default_expr: Option<String>,
    #[serde(default)]
    pub generation_expr: Option<String>,
    #[serde(default)]
    pub generation_expr_authorized_by: Option<String>,
    #[serde(default)]
    pub collation: Option<String>,
    /// Logical DROP COLUMN marker (PostgreSQL `attisdropped`).
    /// When true, the column's physical slot is preserved in rows but
    /// the column is invisible to SQL queries.
    #[serde(default)]
    pub is_dropped: bool,
}

impl ColumnDef {
    /// Create a column with the three semantically-required fields; all other
    /// flags default to `false` / `None`.
    ///
    /// `nullable` is an explicit parameter because its correct value depends on
    /// context: catalog views typically use `true`, virtual-table system columns
    /// use `false`, and DDL paths take the value from the parsed AST.
    ///
    /// Use the chainable modifiers ([`primary_key`](Self::primary_key),
    /// [`serial`](Self::serial), etc.) to set non-default fields.
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
            generation_expr: None,
            generation_expr_authorized_by: None,
            collation: None,
            is_dropped: false,
        }
    }

    /// Mark the column as a primary-key member.
    ///
    /// Implies `NOT NULL` (overrides the `nullable` parameter passed to
    /// [`new`](Self::new)), matching PostgreSQL semantics.
    pub fn primary_key(mut self) -> Self {
        self.primary_key = true;
        self.nullable = false;
        self
    }

    /// Mark the column as `UNIQUE`.
    ///
    /// Does **not** imply `NOT NULL` — PostgreSQL UNIQUE columns accept
    /// multiple NULL values.
    pub fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    /// Mark the column as `SERIAL` (auto-increment).
    ///
    /// Implies `NOT NULL` and clears any previously-set `default_expr`,
    /// matching PostgreSQL semantics where SERIAL overrides an explicit
    /// DEFAULT.
    pub fn serial(mut self) -> Self {
        self.is_serial = true;
        self.nullable = false;
        self.default_expr = None;
        self
    }

    /// Set a column-level `DEFAULT` expression.
    #[allow(dead_code)]
    pub fn default_expr(mut self, expr: impl Into<String>) -> Self {
        self.default_expr = Some(expr.into());
        self
    }

    /// Set the column collation (e.g. `"en_US"`).
    pub fn collation(mut self, collation: impl Into<String>) -> Self {
        self.collation = Some(collation.into());
        self
    }

    /// Set a `GENERATED ALWAYS AS (…) STORED` expression.
    #[allow(dead_code)]
    pub fn generation_expr(mut self, expr: impl Into<String>) -> Self {
        self.generation_expr = Some(expr.into());
        self
    }

    #[allow(dead_code)]
    /// Set the role that authorised a generated-column expression
    /// (used by the auto-embedding feature).
    pub fn generation_expr_authorized_by(mut self, author: impl Into<String>) -> Self {
        self.generation_expr_authorized_by = Some(author.into());
        self
    }
}

/// Index definition
#[derive(Debug, Clone, Serialize)]
pub struct IndexDef {
    pub name: String,
    pub id: u64,
    pub columns: Vec<String>,
    pub unique: bool,
    pub is_constraint: bool,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub predicate: Option<String>,
    #[serde(default)]
    pub expressions: Vec<String>,
    #[serde(default)]
    pub state: IndexState,
    #[serde(skip, default)]
    pub cached_predicate_conjuncts: Option<Vec<String>>,
    /// HNSW: max connections per node (default: 16)
    #[serde(default)]
    pub hnsw_m: Option<u16>,
    /// HNSW: build beam width (default: 64)
    #[serde(default)]
    pub hnsw_ef_construction: Option<u16>,
    /// HNSW: distance metric ("l2", "cosine", "ip")
    #[serde(default)]
    pub hnsw_distance_metric: Option<String>,
}

#[derive(Deserialize)]
struct IndexDefSerde {
    name: String,
    id: u64,
    columns: Vec<String>,
    unique: bool,
    #[serde(default)]
    is_constraint: Option<bool>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    predicate: Option<String>,
    #[serde(default)]
    expressions: Vec<String>,
    #[serde(default)]
    state: IndexState,
    #[serde(default)]
    hnsw_m: Option<u16>,
    #[serde(default)]
    hnsw_ef_construction: Option<u16>,
    #[serde(default)]
    hnsw_distance_metric: Option<String>,
}

impl<'de> Deserialize<'de> for IndexDef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = IndexDefSerde::deserialize(deserializer)?;
        let is_constraint = raw
            .is_constraint
            .unwrap_or(raw.unique && !raw.columns.is_empty());
        Ok(Self {
            name: raw.name,
            id: raw.id,
            columns: raw.columns,
            unique: raw.unique,
            is_constraint,
            method: raw.method,
            predicate: raw.predicate,
            expressions: raw.expressions,
            state: raw.state,
            cached_predicate_conjuncts: None,
            hnsw_m: raw.hnsw_m,
            hnsw_ef_construction: raw.hnsw_ef_construction,
            hnsw_distance_metric: raw.hnsw_distance_metric,
        })
    }
}

pub fn build_predicate_conjunct_cache(predicate: Option<&str>) -> Option<Vec<String>> {
    let predicate = predicate?.trim();
    if predicate.is_empty() {
        return None;
    }

    let sql = format!("SELECT {}", predicate);
    let stmts = crate::sql::parse_sql(&sql).ok()?;
    let sqlparser::ast::Statement::Query(query) = stmts.into_iter().next()? else {
        return None;
    };
    let sqlparser::ast::SetExpr::Select(select) = *query.body else {
        return None;
    };
    let sqlparser::ast::SelectItem::UnnamedExpr(expr) = select.projection.into_iter().next()?
    else {
        return None;
    };

    Some(
        extract_predicate_conjuncts(&expr)
            .into_iter()
            .map(|conjunct| normalize_predicate_conjunct(conjunct.to_string()))
            .collect(),
    )
}

fn extract_predicate_conjuncts(expr: &sqlparser::ast::Expr) -> Vec<sqlparser::ast::Expr> {
    match expr {
        sqlparser::ast::Expr::BinaryOp {
            left,
            op: sqlparser::ast::BinaryOperator::And,
            right,
        } => {
            let mut result = extract_predicate_conjuncts(left);
            result.extend(extract_predicate_conjuncts(right));
            result
        }
        sqlparser::ast::Expr::Nested(inner) => extract_predicate_conjuncts(inner),
        other => vec![other.clone()],
    }
}

fn normalize_predicate_conjunct(mut input: String) -> String {
    input = input.trim().to_string();
    while has_wrapping_parentheses(&input) {
        input = input[1..input.len().saturating_sub(1)].trim().to_string();
    }
    input
        .replace('"', "")
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn has_wrapping_parentheses(input: &str) -> bool {
    if input.len() < 2 || !input.starts_with('(') || !input.ends_with(')') {
        return false;
    }

    let mut depth = 0_i32;
    for (idx, ch) in input.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && idx + 1 < input.len() {
                    return false;
                }
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }

    depth == 0
}

impl IndexDef {
    /// Returns true if this is an HNSW index.
    pub fn is_hnsw(&self) -> bool {
        self.method.as_deref() == Some("hnsw")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckConstraint {
    pub name: Option<String>,
    pub expr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKeyConstraint {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_delete: ForeignKeyAction,
    pub on_update: ForeignKeyAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub enum ForeignKeyAction {
    #[default]
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl ForeignKeyAction {
    /// Returns `true` for actions that require the child table to have a PK
    /// so that db9's KV storage can re-derive the storage key for affected
    /// rows during cascade/set operations.  NO ACTION and RESTRICT never
    /// mutate child rows, so they are safe without a PK.
    pub fn requires_child_pk(&self) -> bool {
        matches!(self, Self::Cascade | Self::SetNull | Self::SetDefault)
    }
}

impl ForeignKeyConstraint {
    /// Returns `true` when either the ON DELETE or ON UPDATE action requires
    /// the child table to have a primary key.
    pub fn requires_child_pk(&self) -> bool {
        self.on_delete.requires_child_pk() || self.on_update.requires_child_pk()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TableSchema {
    pub name: String,
    pub table_id: u64,
    pub columns: Vec<ColumnDef>,
    pub version: u64,
    /// Primary key constraint name (e.g. `table_pkey` or a user-specified `CONSTRAINT` name).
    #[serde(default)]
    pub pk_constraint_name: Option<String>,
    pub pk_indices: Vec<usize>,
    pub indexes: Vec<IndexDef>,
    #[serde(default)]
    pub check_constraints: Vec<CheckConstraint>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKeyConstraint>,
    /// Owner role/user name for this table (metadata only; no permission enforcement yet).
    #[serde(default = "default_owner")]
    pub owner: String,
    /// Whether row-level security is enabled on this table.
    #[serde(default)]
    pub rls_enabled: bool,
    /// Whether RLS is forced even for the table owner.
    #[serde(default)]
    pub rls_force: bool,
    /// Runtime-only FROM alias (e.g., `FROM foo_tbl AS bar` → alias = "bar").
    /// Not serialized; used only during query evaluation for whole-row references
    /// and qualified column resolution.
    #[serde(skip)]
    pub from_alias: Option<String>,
}

impl TableSchema {
    pub fn new(
        name: String,
        table_id: u64,
        columns: Vec<ColumnDef>,
        pk_indices: Vec<usize>,
    ) -> Self {
        let pk_constraint_name = if pk_indices.is_empty() {
            None
        } else {
            let short = name.rsplit('.').next().unwrap_or(&name);
            Some(format!("{}_pkey", short))
        };
        Self {
            name,
            table_id,
            columns,
            version: 1,
            pk_constraint_name,
            pk_indices,
            indexes: Vec::new(),
            check_constraints: Vec::new(),
            foreign_keys: Vec::new(),
            owner: default_owner(),
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        }
    }

    /// Create a virtual / catalog table schema (`table_id = 0`).
    ///
    /// Intended for pg_catalog views, information_schema views, extension
    /// output schemas, operator output schemas, and any other context where
    /// the schema describes ephemeral or system-defined rows with no indexes,
    /// constraints, or owner.
    ///
    /// Uses `owner: ""` (empty), matching all 50+ existing catalog view
    /// definitions.  Do **not** use this for user-owned persistent tables —
    /// use [`TableSchema::new`] instead.
    pub fn virtual_table(name: impl Into<String>, columns: Vec<ColumnDef>) -> Self {
        Self {
            name: name.into(),
            table_id: 0,
            columns,
            version: 1,
            pk_constraint_name: None,
            pk_indices: Vec::new(),
            indexes: Vec::new(),
            check_constraints: Vec::new(),
            foreign_keys: Vec::new(),
            owner: String::new(),
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        }
    }
}

impl TableSchema {
    /// Iterate only SQL-visible columns (excluding logically dropped ones).
    /// Returns `(physical_index, &ColumnDef)` so callers keep correct row positions.
    ///
    /// Use this for any SQL-facing surface: name resolution, DDL export,
    /// information_schema, DML target lists, wildcard expansion.
    /// Use `.columns` directly only when you need the physical row layout
    /// (storage encoding, fill_row_defaults, scope-with-gaps, pg_attribute).
    #[allow(dead_code)]
    pub fn visible_columns(&self) -> impl Iterator<Item = (usize, &ColumnDef)> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.is_dropped)
    }

    /// Count of SQL-visible columns (excluding dropped).
    #[allow(dead_code)]
    pub fn visible_column_count(&self) -> usize {
        self.columns.iter().filter(|c| !c.is_dropped).count()
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| !c.is_dropped && c.name == name)
    }

    pub fn get_pk_values(&self, row: &Row) -> Vec<Value> {
        if self.pk_indices.is_empty() {
            vec![Value::Uuid(*uuid::Uuid::new_v4().as_bytes())]
        } else {
            self.pk_indices
                .iter()
                .map(|&idx| row.values[idx].clone())
                .collect()
        }
    }

    // Helper to get Index values
    pub fn get_index_values(&self, index: &IndexDef, row: &Row) -> Vec<Value> {
        let mut values = Vec::new();
        for col_name in &index.columns {
            if let Some(idx) = self.column_index(col_name) {
                values.push(row.values[idx].clone());
            } else {
                // Should not happen if index validated
                values.push(Value::Null);
            }
        }
        values
    }

    pub fn hydrate_runtime_caches(&mut self) {
        for index in &mut self.indexes {
            index.cached_predicate_conjuncts =
                build_predicate_conjunct_cache(index.predicate.as_deref());
        }
    }
}

/// A row of data
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Row {
    pub values: Vec<Value>,
}

impl Row {
    pub fn new(values: Vec<Value>) -> Self {
        Self { values }
    }
}

/// Infer column types by scanning all rows, returning the first non-NULL type per column.
/// Falls back to Text for columns that are NULL in all rows (PostgreSQL-compatible).
pub fn infer_column_types_from_rows(rows: &[Row], col_count: usize) -> Vec<DataType> {
    (0..col_count)
        .map(|col_idx| {
            rows.iter()
                .find_map(|row| row.values.get(col_idx).and_then(|v| v.data_type()))
                // INTENTIONAL: all-NULL column defaults to Text (PostgreSQL-compatible)
                .unwrap_or(DataType::Text)
        })
        .collect()
}
