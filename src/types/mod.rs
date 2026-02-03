//! Data types for the SQL engine

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};

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
        Ok(Decimal::from_parts(
            parts.lo,
            parts.mid,
            parts.hi,
            parts.negative,
            parts.scale,
        ))
    }
}

/// Supported column data types
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Currently backed by `rust_decimal` (max 28 digits, scale 0-28).
    /// precision: total number of digits (<= 28, default unlimited)
    /// scale: digits after decimal point (0-28, default 0)
    Numeric {
        precision: Option<u32>,
        scale: Option<u32>,
    },
    TimestampTz,
    Tsvector,
    Tsquery,
}

impl DataType {
    pub fn estimated_size(&self) -> usize {
        match self {
            DataType::Boolean => 1,
            DataType::Int32 => 4,
            DataType::Int64 => 8,
            DataType::Float64 => 8,
            DataType::Text => 32,
            DataType::Bytes => 32,
            DataType::Timestamp => 8,
            DataType::Interval => 8,
            DataType::Uuid => 16,
            DataType::Array(_) => 64,
            DataType::Vector(dim) => (*dim as usize) * 8,
            DataType::Json => 64,
            DataType::Jsonb => 64,
            DataType::Time => 8,
            DataType::UserDefined(_) => 32,
            DataType::Date => 4,
            DataType::Numeric { .. } => 16,
            DataType::TimestampTz => 8,
            DataType::Tsvector => 64,
            DataType::Tsquery => 32,
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Boolean => write!(f, "BOOLEAN"),
            DataType::Int32 => write!(f, "INT"),
            DataType::Int64 => write!(f, "BIGINT"),
            DataType::Float64 => write!(f, "DOUBLE"),
            DataType::Text => write!(f, "TEXT"),
            DataType::Bytes => write!(f, "BYTEA"),
            DataType::Timestamp => write!(f, "TIMESTAMP"),
            DataType::Interval => write!(f, "INTERVAL"),
            DataType::Uuid => write!(f, "UUID"),
            DataType::Array(elem_type) => write!(f, "{}[]", elem_type),
            DataType::Vector(dim) => write!(f, "vector({})", dim),
            DataType::Json => write!(f, "JSON"),
            DataType::Jsonb => write!(f, "JSONB"),
            DataType::Time => write!(f, "TIME"),
            DataType::UserDefined(name) => write!(f, "{name}"),
            DataType::Date => write!(f, "DATE"),
            DataType::Numeric {
                precision: Some(p),
                scale: Some(s),
            } => write!(f, "NUMERIC({},{})", p, s),
            DataType::Numeric {
                precision: Some(p),
                scale: None,
            } => write!(f, "NUMERIC({})", p),
            DataType::Numeric { .. } => write!(f, "NUMERIC"),
            DataType::TimestampTz => write!(f, "TIMESTAMPTZ"),
            DataType::Tsvector => write!(f, "TSVECTOR"),
            DataType::Tsquery => write!(f, "TSQUERY"),
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
    pub fn to_millis_approx(&self) -> i64 {
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
            let hours = remaining / (1000 * 60 * 60);
            let mins = (remaining % (1000 * 60 * 60)) / (1000 * 60);
            let secs = (remaining % (1000 * 60)) / 1000;
            parts.push(format!("{:02}:{:02}:{:02}", hours, mins, secs));
        }
        write!(f, "{}", parts.join(" "))
    }
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
    #[allow(dead_code)]
    pub fn as_bytea(&self) -> Result<&[u8]> {
        match self {
            Value::Bytes(bytes) => Ok(bytes),
            _ => Err(anyhow!("expected bytea")),
        }
    }

    /// Returns the value as a `uuid::Uuid`.
    ///
    /// This is a cheap conversion (16 bytes); it does not allocate.
    #[allow(dead_code)]
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
            Value::Boolean(b) => write!(f, "{}", b),
            Value::Int32(i) => write!(f, "{}", i),
            Value::Int64(i) => write!(f, "{}", i),
            Value::Float64(v) => write!(f, "{}", v),
            Value::Text(s) => write!(f, "{}", s),
            Value::Bytes(b) => write!(f, "{:?}", b),
            Value::Timestamp(ts) => write!(f, "{}", ts),
            Value::Interval(iv) => write!(f, "{}", iv),
            Value::Time(micros) => {
                let total_secs = *micros / 1_000_000;
                let hours = total_secs / 3600;
                let mins = (total_secs % 3600) / 60;
                let secs = total_secs % 60;
                let frac = *micros % 1_000_000;
                if frac > 0 {
                    write!(f, "{:02}:{:02}:{:02}.{:06}", hours, mins, secs, frac)
                } else {
                    write!(f, "{:02}:{:02}:{:02}", hours, mins, secs)
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
                        v => write!(f, "{}", v)?,
                    }
                }
                write!(f, "}}")
            }
            Value::Vector(vec) => {
                write!(f, "[")?;
                for (i, v) in vec.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "{}", v)?;
                }
                write!(f, "]")
            }
            Value::Json(s) => write!(f, "{}", s),
            Value::Jsonb(s) => write!(f, "{}", s),
            Value::Date(days) => match date::format_date_days(*days) {
                Ok(s) => write!(f, "{s}"),
                Err(_) => write!(f, "{days}"),
            },
            Value::Numeric(d) => write!(f, "{}", d),
            Value::Tsvector(s) => write!(f, "{}", s),
            Value::Tsquery(s) => write!(f, "{}", s),
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
}

/// Index definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexDef {
    pub name: String,
    pub id: u64,
    pub columns: Vec<String>,
    pub unique: bool,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub predicate: Option<String>,
    #[serde(default)]
    pub expressions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckConstraint {
    pub name: Option<String>,
    pub expr: String,
}

/// Keyspace-local PostgreSQL database metadata (storage format v2).
///
/// In pg-tikv, a TiKV keyspace maps to a tenant. Within a tenant, multiple logical
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
}

impl TableSchema {
    #[allow(dead_code)]
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
        }
    }
}

impl TableSchema {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewDef {
    #[serde(default)]
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub query: String,
}

impl ViewDef {
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
}

fn default_owner() -> String {
    "postgres".to_string()
}
