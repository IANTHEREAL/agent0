use serde::{Deserialize, Serialize};
use std::fmt;

pub(crate) mod decimal_serde {
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
            DataType::Varchar(0) => write!(f, "VARCHAR"),
            DataType::Varchar(n) => write!(f, "VARCHAR({})", n),
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

    #[cfg(test)]
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
            let hours = remaining / (1000 * 60 * 60);
            let mins = (remaining % (1000 * 60 * 60)) / (1000 * 60);
            let secs = (remaining % (1000 * 60)) / 1000;
            parts.push(format!("{:02}:{:02}:{:02}", hours, mins, secs));
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
            write!(out, "{:.0}", v).unwrap();
        } else {
            write!(out, "{}", v).unwrap();
        }
    }
    out.push(']');
    out
}
