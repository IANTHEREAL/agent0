use crate::types::DataType;

pub(crate) const OID_BOOL: i64 = 16;
pub(crate) const OID_BYTEA: i64 = 17;
pub(crate) const OID_NAME: i64 = 19;
pub(crate) const OID_INT8: i64 = 20;
pub(crate) const OID_INT2: i64 = 21;
pub(crate) const OID_INT4: i64 = 23;
pub(crate) const OID_TEXT: i64 = 25;
pub(crate) const OID_OID: i64 = 26;
pub(crate) const OID_JSON: i64 = 114;
pub(crate) const OID_FLOAT4: i64 = 700;
pub(crate) const OID_FLOAT8: i64 = 701;
pub(crate) const OID_BPCHAR: i64 = 1042;
pub(crate) const OID_VARCHAR: i64 = 1043;
pub(crate) const OID_DATE: i64 = 1082;
pub(crate) const OID_TIME: i64 = 1083;
pub(crate) const OID_TIMESTAMP: i64 = 1114;
pub(crate) const OID_TIMESTAMPTZ: i64 = 1184;
pub(crate) const OID_INTERVAL: i64 = 1186;
pub(crate) const OID_NUMERIC: i64 = 1700;
pub(crate) const OID_UUID: i64 = 2950;
pub(crate) const OID_TSVECTOR: i64 = 3614;
pub(crate) const OID_TSQUERY: i64 = 3615;
pub(crate) const OID_JSONB: i64 = 3802;
pub(crate) const OID_VECTOR: i64 = 16385;

#[derive(Debug, Clone, Copy)]
pub(crate) struct BuiltinPgType {
    pub(crate) oid: i64,
    pub(crate) typname: &'static str,
    pub(crate) typlen: i32,
    pub(crate) typbyval: &'static str,
    pub(crate) typtype: &'static str,
    pub(crate) typcategory: &'static str,
    pub(crate) typcollation: i64,
}

#[rustfmt::skip]
pub(crate) const BUILTIN_PG_TYPES: &[BuiltinPgType] = &[
    BuiltinPgType { oid: OID_BOOL,        typname: "bool",        typlen:  1, typbyval: "t", typtype: "b", typcategory: "B", typcollation:   0 },
    BuiltinPgType { oid: OID_BYTEA,       typname: "bytea",       typlen: -1, typbyval: "f", typtype: "b", typcategory: "U", typcollation:   0 },
    BuiltinPgType { oid: OID_NAME,        typname: "name",        typlen: 64, typbyval: "f", typtype: "b", typcategory: "S", typcollation: 100 },
    BuiltinPgType { oid: OID_INT8,        typname: "int8",        typlen:  8, typbyval: "t", typtype: "b", typcategory: "N", typcollation:   0 },
    BuiltinPgType { oid: OID_INT2,        typname: "int2",        typlen:  2, typbyval: "t", typtype: "b", typcategory: "N", typcollation:   0 },
    BuiltinPgType { oid: OID_INT4,        typname: "int4",        typlen:  4, typbyval: "t", typtype: "b", typcategory: "N", typcollation:   0 },
    BuiltinPgType { oid: OID_TEXT,        typname: "text",        typlen: -1, typbyval: "f", typtype: "b", typcategory: "S", typcollation: 100 },
    BuiltinPgType { oid: OID_OID,         typname: "oid",         typlen:  4, typbyval: "t", typtype: "b", typcategory: "N", typcollation:   0 },
    BuiltinPgType { oid: OID_JSON,        typname: "json",        typlen: -1, typbyval: "f", typtype: "b", typcategory: "U", typcollation:   0 },
    BuiltinPgType { oid: OID_FLOAT4,      typname: "float4",      typlen:  4, typbyval: "t", typtype: "b", typcategory: "N", typcollation:   0 },
    BuiltinPgType { oid: OID_FLOAT8,      typname: "float8",      typlen:  8, typbyval: "t", typtype: "b", typcategory: "N", typcollation:   0 },
    BuiltinPgType { oid: OID_BPCHAR,      typname: "bpchar",      typlen: -1, typbyval: "f", typtype: "b", typcategory: "S", typcollation: 100 },
    BuiltinPgType { oid: OID_VARCHAR,     typname: "varchar",     typlen: -1, typbyval: "f", typtype: "b", typcategory: "S", typcollation: 100 },
    BuiltinPgType { oid: OID_DATE,        typname: "date",        typlen:  4, typbyval: "t", typtype: "b", typcategory: "D", typcollation:   0 },
    BuiltinPgType { oid: OID_TIME,        typname: "time",        typlen:  8, typbyval: "t", typtype: "b", typcategory: "D", typcollation:   0 },
    BuiltinPgType { oid: OID_TIMESTAMP,   typname: "timestamp",   typlen:  8, typbyval: "t", typtype: "b", typcategory: "D", typcollation:   0 },
    BuiltinPgType { oid: OID_TIMESTAMPTZ, typname: "timestamptz", typlen:  8, typbyval: "t", typtype: "b", typcategory: "D", typcollation:   0 },
    BuiltinPgType { oid: OID_INTERVAL,    typname: "interval",    typlen: 16, typbyval: "f", typtype: "b", typcategory: "T", typcollation:   0 },
    BuiltinPgType { oid: OID_NUMERIC,     typname: "numeric",     typlen: -1, typbyval: "f", typtype: "b", typcategory: "N", typcollation:   0 },
    BuiltinPgType { oid: OID_UUID,        typname: "uuid",        typlen: 16, typbyval: "f", typtype: "b", typcategory: "U", typcollation:   0 },
    BuiltinPgType { oid: OID_TSVECTOR,    typname: "tsvector",    typlen: -1, typbyval: "f", typtype: "b", typcategory: "U", typcollation:   0 },
    BuiltinPgType { oid: OID_TSQUERY,     typname: "tsquery",     typlen: -1, typbyval: "f", typtype: "b", typcategory: "U", typcollation:   0 },
    BuiltinPgType { oid: OID_JSONB,       typname: "jsonb",       typlen: -1, typbyval: "f", typtype: "b", typcategory: "U", typcollation:   0 },
    BuiltinPgType { oid: OID_VECTOR,      typname: "vector",      typlen: -1, typbyval: "f", typtype: "b", typcategory: "A", typcollation:   0 },
];

pub(crate) fn typname_for_oid(oid: i64) -> Option<&'static str> {
    BUILTIN_PG_TYPES
        .iter()
        .find(|t| t.oid == oid)
        .map(|t| t.typname)
}

pub(crate) fn oid_and_typlen_for_datatype(dt: &DataType) -> (i64, i32) {
    match dt {
        DataType::Boolean => (OID_BOOL, 1),
        DataType::Int32 => (OID_INT4, 4),
        DataType::Int64 => (OID_INT8, 8),
        DataType::Float64 => (OID_FLOAT8, 8),
        DataType::Text => (OID_TEXT, -1),
        DataType::Bytes => (OID_BYTEA, -1),
        DataType::Timestamp => (OID_TIMESTAMP, 8),
        DataType::TimestampTz => (OID_TIMESTAMPTZ, 8),
        DataType::Date => (OID_DATE, 4),
        DataType::Time => (OID_TIME, 8),
        DataType::Interval => (OID_INTERVAL, 16),
        DataType::Uuid => (OID_UUID, 16),
        DataType::Json => (OID_JSON, -1),
        DataType::Jsonb => (OID_JSONB, -1),
        DataType::Numeric { .. } => (OID_NUMERIC, -1),
        DataType::Vector(_) => (OID_VECTOR, -1),
        DataType::Tsvector => (OID_TSVECTOR, -1),
        DataType::Tsquery => (OID_TSQUERY, -1),
        DataType::Name => (OID_NAME, 64),
        DataType::Varchar(_) => (OID_VARCHAR, -1),
        DataType::Array(_) | DataType::UserDefined(_) => (OID_TEXT, -1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oid_and_typlen_distinguishes_text_and_varchar() {
        assert_eq!(
            oid_and_typlen_for_datatype(&DataType::Text),
            (OID_TEXT, -1),
            "TEXT should map to OID_TEXT"
        );
        assert_eq!(
            oid_and_typlen_for_datatype(&DataType::Varchar(3)),
            (OID_VARCHAR, -1),
            "VARCHAR(n) should map to OID_VARCHAR"
        );
    }
}
