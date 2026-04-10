//! Data types for the SQL engine

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum IndexState {
    #[default]
    Ready,
    Building,
    Invalid,
    WriteOnly,
}

pub mod date;
pub mod timestamp;

mod decimal_serde {
    use rust_decimal::Decimal;
    use serde::{de::Deserializer, ser::Serializer, Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct DecimalParts {
        lo: u32,
        mid: u32,
        hi: u32,
        negative: bool,
        scale: u32,
    }

    pub fn serialize<S: Serializer>(d: &Decimal, serializer: S) -> Result<S::Ok, S::Error> {
        let unpacked = d.unpack();
        let parts = DecimalParts {
            lo: unpacked.lo,
            mid: unpacked.mid,
            hi: unpacked.hi,
            negative: unpacked.negative,
            scale: unpacked.scale,
        };
        parts.serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Decimal, D::Error> {
        let parts = DecimalParts::deserialize(deserializer)?;
        let scale = parts.scale.min(Decimal::MAX_SCALE);
        Ok(Decimal::from_parts(
            parts.lo,
            parts.mid,
            parts.hi,
            parts.negative,
            scale,
        ))
    }
}

/// Supported column data types
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DataType {
    Boolean,
    Int32,
    Int64,
    Float64,
    Text,
    Bytes,
    Timestamp,
    Interval,
    Uuid,
    Array(Box<DataType>),
    Vector(u32), // dimension count
    // NOTE: Do not reorder variants; append only to preserve bincode compatibility.
    Json,
    Jsonb,
    Time, // Time of day (microseconds since midnight)
    UserDefined(String),
    Date, // Date without time zone (days since 1970-01-01)
    /// NUMERIC/DECIMAL with optional precision and scale
    /// DDL typmod accepts PostgreSQL-compatible precision metadata.
    /// Runtime arithmetic/coercion is currently backed by `rust_decimal` (effective scale <= 28).
    /// precision: total number of digits (typmod metadata)
    /// scale: digits after decimal point (typmod metadata)
    Numeric {
        precision: Option<u32>,
        scale: Option<u32>,
    },
    TimestampTz,
    Tsvector,
    Tsquery,
    Name,
    Varchar(u64),
    /// PostgreSQL OID type (OID 26).  Semantically distinct from INT8 on the
    /// wire (INT8 is OID 20, OID is OID 26), but stored identically as a 64-bit
    /// integer (`Value::Int64`).  OID alias types (regclass, regtype) remain as
    /// `UserDefined`; this variant represents the base `oid` type that
    /// unresolved parameters infer to when the context is an OID alias.
    Oid,
    /// PostgreSQL's UNKNOWN type (OID 705).  Bare string literals and NULL
    /// start with this type; context (comparison, assignment, function call)
    /// resolves it to a concrete type.  **Invariant: Unknown must never escape
    /// the Analyzer — it must be resolved before reaching the executor,
    /// storage, or wire protocol layers.**
    Unknown,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Boolean => write!(f, "BOOLEAN"),
            DataType::Int32 => write!(f, "INTEGER"),
            DataType::Int64 => write!(f, "BIGINT"),
            DataType::Float64 => write!(f, "DOUBLE"),
            DataType::Text => write!(f, "TEXT"),
            DataType::Name => write!(f, "NAME"),
            DataType::Bytes => write!(f, "BYTEA"),
            DataType::Timestamp => write!(f, "TIMESTAMP"),
            DataType::Interval => write!(f, "INTERVAL"),
            DataType::Uuid => write!(f, "UUID"),
            DataType::Array(elem_type) => write!(f, "{elem_type}[]"),
            DataType::Vector(dim) => write!(f, "vector({dim})"),
            DataType::Json => write!(f, "JSON"),
            DataType::Jsonb => write!(f, "JSONB"),
            DataType::Time => write!(f, "TIME"),
            DataType::UserDefined(name) => write!(f, "{name}"),
            DataType::Date => write!(f, "DATE"),
            DataType::Numeric {
                precision: Some(p),
                scale: Some(s),
            } => write!(f, "NUMERIC({p},{s})"),
            DataType::Numeric {
                precision: Some(p),
                scale: None,
            } => write!(f, "NUMERIC({p})"),
            DataType::Numeric { .. } => write!(f, "NUMERIC"),
            DataType::TimestampTz => write!(f, "TIMESTAMPTZ"),
            DataType::Tsvector => write!(f, "TSVECTOR"),
            DataType::Tsquery => write!(f, "TSQUERY"),
            DataType::Varchar(0) => write!(f, "VARCHAR"),
            DataType::Varchar(n) => write!(f, "VARCHAR({n})"),
            DataType::Oid => write!(f, "OID"),
            DataType::Unknown => write!(f, "unknown"),
        }
    }
}

impl DataType {
    /// Return the PostgreSQL-canonical lowercase type name for use in error
    /// messages (e.g. `FunctionNotFound`).  This matches the names PostgreSQL
    /// itself emits in diagnostic messages, which differ from the internal OID
    /// names (int4, float8, …) and from our `Display` impl (uppercase).
    pub fn pg_display_name(&self) -> String {
        match self {
            DataType::Boolean => "boolean".to_string(),
            DataType::Int32 => "integer".to_string(),
            DataType::Int64 => "bigint".to_string(),
            DataType::Float64 => "double precision".to_string(),
            DataType::Numeric { .. } => "numeric".to_string(),
            DataType::Oid => "oid".to_string(),
            DataType::Array(elem_type) => format!("{}[]", elem_type.pg_display_name()),
            DataType::Varchar(_) => "character varying".to_string(),
            _ => self.to_string().to_lowercase(),
        }
    }
}

/// Interval representation with separate months and milliseconds.
/// This matches PostgreSQL's behavior where months are calendar-aware.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct IntervalValue {
    /// Number of months (for calendar-aware arithmetic)
    pub months: i32,
    /// Milliseconds (for sub-month precision: days, hours, minutes, seconds)
    pub millis: i64,
}

impl IntervalValue {
    pub fn new(months: i32, millis: i64) -> Self {
        Self { months, millis }
    }

    pub fn from_millis(millis: i64) -> Self {
        Self { months: 0, millis }
    }

    pub fn from_months(months: i32) -> Self {
        Self { months, millis: 0 }
    }

    /// Convert to total milliseconds (approximate, for legacy compat)
    /// Uses 30 days per month approximation
    pub fn to_millis_approx(self) -> i64 {
        (self.months as i64) * 30 * 24 * 60 * 60 * 1000 + self.millis
    }
}

impl std::ops::Add for IntervalValue {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self {
            months: self.months + rhs.months,
            millis: self.millis + rhs.millis,
        }
    }
}

impl std::ops::Sub for IntervalValue {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self {
            months: self.months - rhs.months,
            millis: self.millis - rhs.millis,
        }
    }
}

impl fmt::Display for IntervalValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if self.months != 0 {
            let years = self.months / 12;
            let mons = self.months % 12;
            if years != 0 {
                parts.push(format!(
                    "{} year{}",
                    years,
                    if years.abs() != 1 { "s" } else { "" }
                ));
            }
            if mons != 0 {
                parts.push(format!(
                    "{} mon{}",
                    mons,
                    if mons.abs() != 1 { "s" } else { "" }
                ));
            }
        }
        let ms = self.millis;
        let days = ms / (1000 * 60 * 60 * 24);
        let remaining = ms % (1000 * 60 * 60 * 24);
        if days != 0 {
            parts.push(format!(
                "{} day{}",
                days,
                if days.abs() != 1 { "s" } else { "" }
            ));
        }
        if remaining != 0 || parts.is_empty() {
            let sign = if remaining < 0 { "-" } else { "" };
            let abs_rem = remaining.unsigned_abs();
            let hours = abs_rem / (1000 * 60 * 60);
            let mins = (abs_rem % (1000 * 60 * 60)) / (1000 * 60);
            let secs = (abs_rem % (1000 * 60)) / 1000;
            parts.push(format!("{sign}{hours:02}:{mins:02}:{secs:02}"));
        }
        write!(f, "{}", parts.join(" "))
    }
}

/// pgvector text format: `[1,2,3]`. Integer-valued floats omit `.0`.
pub fn format_vector_pg_text(vec: &[f64]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(2 + vec.len() * 4);
    out.push('[');
    for (i, v) in vec.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        if v.fract() == 0.0 && v.is_finite() {
            write!(out, "{v:.0}").unwrap();
        } else {
            write!(out, "{v}").unwrap();
        }
    }
    out.push(']');
    out
}

/// A single value
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    Null,
    Boolean(bool),
    Int32(i32),
    Int64(i64),
    Float64(f64),
    Text(String),
    Bytes(Vec<u8>),
    Timestamp(i64),
    Interval(IntervalValue),
    Uuid([u8; 16]),
    Array(Vec<Value>),
    Vector(Vec<f64>),
    Json(String),
    Jsonb(String),
    Time(i64),
    Date(i32),
    Numeric(#[serde(with = "decimal_serde")] Decimal),
    Tsvector(String),
    Tsquery(String),
}

impl Value {
    pub fn type_display_name(&self) -> String {
        self.data_type()
            .map(|dt| dt.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }

    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Value::Null => None,
            Value::Boolean(_) => Some(DataType::Boolean),
            Value::Int32(_) => Some(DataType::Int32),
            Value::Int64(_) => Some(DataType::Int64),
            Value::Float64(_) => Some(DataType::Float64),
            Value::Text(_) => Some(DataType::Text),
            Value::Bytes(_) => Some(DataType::Bytes),
            Value::Timestamp(_) => Some(DataType::Timestamp),
            Value::Interval(_) => Some(DataType::Interval),
            Value::Uuid(_) => Some(DataType::Uuid),
            Value::Array(elems) => {
                let elem_type = elems.first().and_then(|v| v.data_type());
                Some(DataType::Array(Box::new(
                    // INTENTIONAL: empty array defaults element type to Text (PG-compatible)
                    elem_type.unwrap_or(DataType::Text),
                )))
            }
            Value::Vector(vec) => Some(DataType::Vector(vec.len() as u32)),
            Value::Json(_) => Some(DataType::Json),
            Value::Jsonb(_) => Some(DataType::Jsonb),
            Value::Time(_) => Some(DataType::Time),
            Value::Date(_) => Some(DataType::Date),
            Value::Numeric(d) => Some(DataType::Numeric {
                precision: None,
                scale: Some(d.scale()),
            }),
            Value::Tsvector(_) => Some(DataType::Tsvector),
            Value::Tsquery(_) => Some(DataType::Tsquery),
        }
    }

    /// Returns the underlying `BYTEA` contents as a borrowed byte slice.
    ///
    /// This is a zero-copy accessor; it does not allocate.
    #[allow(dead_code)] // framework: value conversion API
    pub fn as_bytea(&self) -> Result<&[u8]> {
        match self {
            Value::Bytes(bytes) => Ok(bytes),
            _ => Err(anyhow!("expected bytea")),
        }
    }

    /// Returns the value as a `uuid::Uuid`.
    ///
    /// This is a cheap conversion (16 bytes); it does not allocate.
    #[allow(dead_code)] // framework: value conversion API
    pub fn as_uuid(&self) -> Result<uuid::Uuid> {
        match self {
            Value::Uuid(bytes) => Ok(uuid::Uuid::from_bytes(*bytes)),
            _ => Err(anyhow!("expected uuid")),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Boolean(b) => write!(f, "{b}"),
            Value::Int32(i) => write!(f, "{i}"),
            Value::Int64(i) => write!(f, "{i}"),
            Value::Float64(v) => write!(f, "{v}"),
            Value::Text(s) => write!(f, "{s}"),
            Value::Bytes(b) => write!(f, "{b:?}"),
            Value::Timestamp(ts) => write!(f, "{ts}"),
            Value::Interval(iv) => write!(f, "{iv}"),
            Value::Time(micros) => {
                let total_secs = *micros / 1_000_000;
                let hours = total_secs / 3600;
                let mins = (total_secs % 3600) / 60;
                let secs = total_secs % 60;
                let frac = *micros % 1_000_000;
                if frac > 0 {
                    write!(f, "{hours:02}:{mins:02}:{secs:02}.{frac:06}")
                } else {
                    write!(f, "{hours:02}:{mins:02}:{secs:02}")
                }
            }
            Value::Uuid(bytes) => {
                write!(
                    f,
                    "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
                    u16::from_be_bytes([bytes[4], bytes[5]]),
                    u16::from_be_bytes([bytes[6], bytes[7]]),
                    u16::from_be_bytes([bytes[8], bytes[9]]),
                    u64::from_be_bytes([
                        0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
                    ])
                )
            }
            Value::Array(elems) => {
                write!(f, "{{")?;
                for (i, elem) in elems.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    match elem {
                        Value::Text(s) => write!(f, "\"{}\"", s.replace('"', "\\\""))?,
                        v => write!(f, "{v}")?,
                    }
                }
                write!(f, "}}")
            }
            Value::Vector(vec) => write!(f, "{}", format_vector_pg_text(vec)),
            Value::Json(s) => write!(f, "{s}"),
            Value::Jsonb(s) => write!(f, "{s}"),
            Value::Date(days) => match date::format_date_days(*days) {
                Ok(s) => write!(f, "{s}"),
                Err(_) => write!(f, "{days}"),
            },
            Value::Numeric(d) => write!(f, "{d}"),
            Value::Tsvector(s) => write!(f, "{s}"),
            Value::Tsquery(s) => write!(f, "{s}"),
        }
    }
}

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
}

impl ColumnDef {
    /// Set the column collation (e.g. `"en_US"`).
    pub fn collation(mut self, collation: impl Into<String>) -> Self {
        self.collation = Some(collation.into());
        self
    }

    /// Set a `GENERATED ALWAYS AS (…) STORED` expression.
    pub fn generation_expr(mut self, expr: impl Into<String>) -> Self {
        self.generation_expr = Some(expr.into());
        self
    }

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

/// Keyspace-local PostgreSQL database metadata (storage format v2).
///
/// In db9-server, a TiKV keyspace maps to a tenant. Within a tenant, multiple logical
/// PostgreSQL databases are supported by partitioning all database-local keys
/// under a fixed `database_id` prefix.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseDef {
    /// Database ID (used as the key prefix for all data within this database).
    pub id: u64,
    /// Database name (e.g. "postgres", "myapp").
    pub name: String,
    /// Database OID for `pg_catalog.pg_database` compatibility.
    pub oid: u32,
    /// Owner role/user name (metadata only; no permission enforcement yet).
    pub owner: String,
    /// Encoding name (always UTF8 for now).
    pub encoding: String,
    /// Creation timestamp in milliseconds since Unix epoch.
    pub created_at: i64,
    /// Template database flag (reserved).
    pub is_template: bool,
    /// Allow connections flag (reserved).
    pub allow_conn: bool,
}

impl DatabaseDef {
    pub fn new(id: u64, name: String, owner: String) -> Self {
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Self {
            id,
            name,
            oid: u32::try_from(id).unwrap_or(u32::MAX),
            owner,
            encoding: "UTF8".to_string(),
            created_at: i64::try_from(created_at).unwrap_or(i64::MAX),
            is_template: false,
            allow_conn: true,
        }
    }

    pub fn default_postgres(id: u64, owner: String) -> Self {
        Self::new(id, "postgres".to_string(), owner)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationRecord {
    pub name: String,
    pub applied_at: String,
    pub checksum: String,
    #[serde(default)]
    pub sql_preview: String,
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
            Some(format!("{short}_pkey"))
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UserTypeKind {
    Enum { labels: Vec<String> },
    Composite { fields: Vec<(String, DataType)> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserTypeDef {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub kind: UserTypeKind,
    pub owner: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SequenceState {
    pub last_value: i64,
    pub is_called: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SequenceBacking {
    /// Bridge to the existing per-table autoincrement key (`_sys_seq_ + table_id`).
    TableId(u64),
    /// A standalone sequence whose state is stored in `SequenceState`.
    Standalone(SequenceState),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SequenceDef {
    #[serde(default)]
    pub oid: u32,
    pub schema: String,
    pub name: String,
    #[serde(default)]
    pub start_value: i64,
    pub increment: i64,
    pub min_value: i64,
    pub max_value: i64,
    #[serde(default)]
    pub cache_size: i64,
    pub is_cycled: bool,
    pub owned_by: Option<(String, String)>,
    pub owner: String,
    pub backing: SequenceBacking,
}

impl SequenceDef {
    pub fn full_name(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDef {
    #[serde(default)]
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub arg_types: Vec<String>,
    pub return_type: String,
    pub language: String,
    pub body: String,
    /// Owner role/user name for this function (metadata only).
    #[serde(default = "default_owner")]
    pub owner: String,
    /// Whether this function runs with the privileges of the definer (owner)
    /// rather than the invoker (caller). Corresponds to PostgreSQL's
    /// `SECURITY DEFINER` attribute (pg_proc.prosecdef).
    #[serde(default)]
    pub security_definer: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerDef {
    #[serde(default)]
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub table: String,
    pub timing: String,
    pub events: Vec<String>,
    pub function: String,
}

/// RLS policy command scope (matches PostgreSQL `pg_policy.polcmd`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RlsCommand {
    All,
    Select,
    Insert,
    Update,
    Delete,
}

impl RlsCommand {
    /// Return the single-character code used by `pg_policy.polcmd`.
    pub fn pg_polcmd(&self) -> &'static str {
        match self {
            RlsCommand::All => "*",
            RlsCommand::Select => "r",
            RlsCommand::Insert => "a",
            RlsCommand::Update => "w",
            RlsCommand::Delete => "d",
        }
    }

    /// Return the human-readable command name used by `pg_policies` view.
    pub fn pg_cmd_display(&self) -> &'static str {
        match self {
            RlsCommand::All => "ALL",
            RlsCommand::Select => "SELECT",
            RlsCommand::Insert => "INSERT",
            RlsCommand::Update => "UPDATE",
            RlsCommand::Delete => "DELETE",
        }
    }

    /// Whether this command scope applies to a given DML operation.
    #[allow(dead_code)]
    pub fn applies_to_select(&self) -> bool {
        matches!(self, RlsCommand::All | RlsCommand::Select)
    }

    #[allow(dead_code)]
    pub fn applies_to_insert(&self) -> bool {
        matches!(self, RlsCommand::All | RlsCommand::Insert)
    }

    #[allow(dead_code)]
    pub fn applies_to_update(&self) -> bool {
        matches!(self, RlsCommand::All | RlsCommand::Update)
    }

    #[allow(dead_code)]
    pub fn applies_to_delete(&self) -> bool {
        matches!(self, RlsCommand::All | RlsCommand::Delete)
    }
}

/// A row-level security policy stored in TiKV.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RlsPolicy {
    /// Unique OID for `pg_policy.oid` catalog compatibility.
    pub oid: u32,
    /// Policy name (unique per table).
    pub name: String,
    /// Table this policy applies to (by table_id).
    pub table_id: u64,
    /// Which DML command(s) the policy applies to.
    pub command: RlsCommand,
    /// `true` = PERMISSIVE (OR'd), `false` = RESTRICTIVE (AND'd).
    pub permissive: bool,
    /// Roles this policy applies to. Empty or `["public"]` means all roles.
    pub roles: Vec<String>,
    /// SQL expression for row visibility (SELECT/UPDATE/DELETE).
    pub using_expr: Option<String>,
    /// SQL expression for new-row validation (INSERT/UPDATE).
    pub with_check_expr: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewDef {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    #[serde(default = "default_owner")]
    pub owner: String,
    pub query: String,
    /// Fully-qualified names of relations this view depends on.
    /// Resolved at CREATE time using the active search_path.
    pub deps: Vec<String>,
    /// Whether this view uses SECURITY DEFINER semantics: RLS policies
    /// are evaluated using the view owner's identity, not the caller's.
    #[serde(default)]
    pub security_definer: bool,
}

impl ViewDef {
    pub fn full_name(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatViewDef {
    pub schema: String,
    pub name: String,
    pub query: String,
    /// Fully-qualified names of relations this materialized view depends on.
    pub deps: Vec<String>,
}

impl MatViewDef {
    pub fn full_name(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_as_bytea() {
        let v = Value::Bytes(vec![1, 2, 3]);
        assert_eq!(v.as_bytea().unwrap(), &[1, 2, 3]);
        assert!(Value::Int32(1).as_bytea().is_err());
    }

    #[test]
    fn value_as_uuid() {
        let uuid = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let v = Value::Uuid(*uuid.as_bytes());
        assert_eq!(v.as_uuid().unwrap(), uuid);
        assert!(Value::Text("x".into()).as_uuid().is_err());
    }

    #[test]
    fn format_vector_pg_text_integers() {
        assert_eq!(format_vector_pg_text(&[1.0, 2.0, 3.0]), "[1,2,3]");
    }

    #[test]
    fn format_vector_pg_text_mixed() {
        assert_eq!(format_vector_pg_text(&[1.0, 2.5, 3.0]), "[1,2.5,3]");
    }

    #[test]
    fn format_vector_pg_text_empty() {
        assert_eq!(format_vector_pg_text(&[]), "[]");
    }

    #[test]
    fn database_def_defaults() {
        let db = DatabaseDef::default_postgres(1, "admin".to_string());
        assert_eq!(db.id, 1);
        assert_eq!(db.name, "postgres");
        assert_eq!(db.owner, "admin");
        assert_eq!(db.encoding, "UTF8");
        assert!(db.created_at >= 0);
        assert!(db.allow_conn);
        assert!(!db.is_template);
    }

    #[test]
    fn pg_display_name_preserves_character_varying_canonical_name() {
        assert_eq!(DataType::Varchar(0).pg_display_name(), "character varying");
        assert_eq!(DataType::Varchar(42).pg_display_name(), "character varying");
        assert_eq!(
            DataType::Array(Box::new(DataType::Varchar(3))).pg_display_name(),
            "character varying[]"
        );
    }

    // ── ColumnDef::new() + modifiers ──────────────────────────────────

    #[test]
    fn columndef_new_defaults() {
        let c = ColumnDef::new("id", DataType::Int64, true);
        assert_eq!(c.name, "id");
        assert_eq!(c.data_type, DataType::Int64);
        assert!(c.nullable);
        assert!(!c.primary_key);
        assert!(!c.unique);
        assert!(!c.is_serial);
        assert!(c.default_expr.is_none());
        assert!(c.generation_expr.is_none());
        assert!(c.generation_expr_authorized_by.is_none());
        assert!(c.collation.is_none());
        assert!(!c.is_dropped);
    }

    #[test]
    fn columndef_new_not_null() {
        let c = ColumnDef::new("x", DataType::Int32, false);
        assert!(!c.nullable);
    }

    #[test]
    fn columndef_primary_key_implies_not_null() {
        let c = ColumnDef::new("id", DataType::Int64, true).primary_key();
        assert!(c.primary_key);
        assert!(!c.nullable); // PK overrides nullable
    }

    #[test]
    fn columndef_serial_implies_not_null_and_clears_default() {
        let c = ColumnDef::new("id", DataType::Int64, true)
            .default_expr("42")
            .serial();
        assert!(c.is_serial);
        assert!(!c.nullable);
        assert!(c.default_expr.is_none()); // serial clears default
    }

    #[test]
    fn columndef_unique_does_not_change_nullable() {
        let c = ColumnDef::new("email", DataType::Text, true).unique();
        assert!(c.unique);
        assert!(c.nullable); // UNIQUE allows NULLs in PG
    }

    #[test]
    fn columndef_flag_modifiers_are_order_independent() {
        let a = ColumnDef::new("id", DataType::Int64, true)
            .serial()
            .primary_key()
            .unique();
        let b = ColumnDef::new("id", DataType::Int64, true)
            .unique()
            .primary_key()
            .serial();
        assert_eq!(a.primary_key, b.primary_key);
        assert_eq!(a.is_serial, b.is_serial);
        assert_eq!(a.unique, b.unique);
        assert_eq!(a.nullable, b.nullable);
        assert_eq!(a.default_expr, b.default_expr);
    }

    #[test]
    fn columndef_serial_clears_default_expr() {
        // .serial() clears default_expr — order matters when combining
        // with .default_expr(). Always call .serial() BEFORE .default_expr()
        // if both are needed.
        let cleared = ColumnDef::new("id", DataType::Int64, true)
            .default_expr("42")
            .serial();
        assert!(
            cleared.default_expr.is_none(),
            "serial() must clear prior default_expr"
        );

        let preserved = ColumnDef::new("id", DataType::Int64, true)
            .serial()
            .default_expr("42");
        assert_eq!(preserved.default_expr.as_deref(), Some("42"));
    }

    #[test]
    fn columndef_optional_setters() {
        let c = ColumnDef::new("bio", DataType::Text, true)
            .default_expr("''")
            .collation("en_US")
            .generation_expr("lower(name)")
            .generation_expr_authorized_by("admin");
        assert_eq!(c.default_expr.as_deref(), Some("''"));
        assert_eq!(c.collation.as_deref(), Some("en_US"));
        assert_eq!(c.generation_expr.as_deref(), Some("lower(name)"));
        assert_eq!(c.generation_expr_authorized_by.as_deref(), Some("admin"));
    }

    #[test]
    fn columndef_equivalence_with_struct_literal() {
        let via_new = ColumnDef::new("_rowid", DataType::Int64, false)
            .primary_key()
            .unique()
            .serial();
        let via_literal = ColumnDef {
            name: "_rowid".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
            unique: true,
            is_serial: true,
            default_expr: None,
            generation_expr: None,
            generation_expr_authorized_by: None,
            collation: None,
            is_dropped: false,
        };
        assert_eq!(via_new.name, via_literal.name);
        assert_eq!(via_new.data_type, via_literal.data_type);
        assert_eq!(via_new.nullable, via_literal.nullable);
        assert_eq!(via_new.primary_key, via_literal.primary_key);
        assert_eq!(via_new.unique, via_literal.unique);
        assert_eq!(via_new.is_serial, via_literal.is_serial);
        assert_eq!(via_new.default_expr, via_literal.default_expr);
        assert_eq!(via_new.generation_expr, via_literal.generation_expr);
        assert_eq!(
            via_new.generation_expr_authorized_by,
            via_literal.generation_expr_authorized_by
        );
        assert_eq!(via_new.collation, via_literal.collation);
        assert_eq!(via_new.is_dropped, via_literal.is_dropped);
    }

    // ── TableSchema::virtual_table() ─────────────────────────────────

    #[test]
    fn virtual_table_defaults() {
        let s = TableSchema::virtual_table(
            "pg_class",
            vec![ColumnDef::new("relname", DataType::Text, true)],
        );
        assert_eq!(s.name, "pg_class");
        assert_eq!(s.table_id, 0);
        assert_eq!(s.version, 1);
        assert_eq!(s.columns.len(), 1);
        assert!(s.pk_constraint_name.is_none());
        assert!(s.pk_indices.is_empty());
        assert!(s.indexes.is_empty());
        assert!(s.check_constraints.is_empty());
        assert!(s.foreign_keys.is_empty());
        assert_eq!(s.owner, ""); // virtual tables use empty owner
        assert!(!s.rls_enabled);
        assert!(!s.rls_force);
        assert!(s.from_alias.is_none());
    }

    #[test]
    fn virtual_table_equivalence_with_struct_literal() {
        let cols = vec![ColumnDef::new("oid", DataType::Int64, false)];
        let via_factory = TableSchema::virtual_table("pg_type", cols.clone());
        let via_literal = TableSchema {
            table_id: 0,
            name: "pg_type".to_string(),
            columns: cols,
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            rls_enabled: false,
            rls_force: false,
            from_alias: None,
        };
        assert_eq!(via_factory.name, via_literal.name);
        assert_eq!(via_factory.table_id, via_literal.table_id);
        assert_eq!(via_factory.version, via_literal.version);
        assert_eq!(via_factory.owner, via_literal.owner);
        assert_eq!(via_factory.columns.len(), via_literal.columns.len());
    }
}

#[allow(clippy::items_after_test_module)]
pub fn default_owner() -> String {
    "postgres".to_string()
}
