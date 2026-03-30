use crate::model::DataType;

pub(crate) const OID_BOOL: i64 = 16;
pub(crate) const OID_BYTEA: i64 = 17;
pub(crate) const OID_CHAR: i64 = 18;
pub(crate) const OID_NAME: i64 = 19;
pub(crate) const OID_INT8: i64 = 20;
pub(crate) const OID_INT2: i64 = 21;
pub(crate) const OID_INT4: i64 = 23;
pub(crate) const OID_TEXT: i64 = 25;
pub(crate) const OID_OID: i64 = 26;
pub(crate) const OID_TID: i64 = 27;
pub(crate) const OID_XID: i64 = 28;
pub(crate) const OID_CID: i64 = 29;
pub(crate) const OID_JSON: i64 = 114;
pub(crate) const OID_ANYARRAY: i64 = 2277;
pub(crate) const OID_REGCLASS: i64 = 2205;
pub(crate) const OID_REGTYPE: i64 = 2206;
pub(crate) const OID_FLOAT4: i64 = 700;
pub(crate) const OID_FLOAT8: i64 = 701;
pub(crate) const OID_BPCHAR: i64 = 1042;
pub(crate) const OID_VARCHAR: i64 = 1043;
pub(crate) const OID_DATE: i64 = 1082;
pub(crate) const OID_TIME: i64 = 1083;
pub(crate) const OID_TIMETZ: i64 = 1266;
pub(crate) const OID_TIMESTAMP: i64 = 1114;
pub(crate) const OID_TIMESTAMPTZ: i64 = 1184;
pub(crate) const OID_INTERVAL: i64 = 1186;
pub(crate) const OID_NUMERIC: i64 = 1700;
pub(crate) const OID_UUID: i64 = 2950;
pub(crate) const OID_TSVECTOR: i64 = 3614;
pub(crate) const OID_TSQUERY: i64 = 3615;
pub(crate) const OID_JSONB: i64 = 3802;
pub(crate) const OID_VECTOR: i64 = 16385;
pub(crate) const OID_HSTORE: i64 = 16386;
pub(crate) const OID_HSTORE_ARRAY: i64 = 16387;
pub(crate) const OID_BOOL_ARRAY: i64 = 1000;
pub(crate) const OID_BYTEA_ARRAY: i64 = 1001;
pub(crate) const OID_NAME_ARRAY: i64 = 1003;
pub(crate) const OID_INT2_ARRAY: i64 = 1005;
pub(crate) const OID_INT4_ARRAY: i64 = 1007;
pub(crate) const OID_TEXT_ARRAY: i64 = 1009;
pub(crate) const OID_BPCHAR_ARRAY: i64 = 1014;
pub(crate) const OID_VARCHAR_ARRAY: i64 = 1015;
pub(crate) const OID_INT8_ARRAY: i64 = 1016;
pub(crate) const OID_FLOAT4_ARRAY: i64 = 1021;
pub(crate) const OID_FLOAT8_ARRAY: i64 = 1022;
pub(crate) const OID_OID_ARRAY: i64 = 1028;
pub(crate) const OID_TIMESTAMP_ARRAY: i64 = 1115;
pub(crate) const OID_DATE_ARRAY: i64 = 1182;
pub(crate) const OID_TIME_ARRAY: i64 = 1183;
pub(crate) const OID_TIMETZ_ARRAY: i64 = 1270;
pub(crate) const OID_TIMESTAMPTZ_ARRAY: i64 = 1185;
pub(crate) const OID_INTERVAL_ARRAY: i64 = 1187;
pub(crate) const OID_NUMERIC_ARRAY: i64 = 1231;
pub(crate) const OID_JSON_ARRAY: i64 = 199;
pub(crate) const OID_UUID_ARRAY: i64 = 2951;
pub(crate) const OID_JSONB_ARRAY: i64 = 3807;

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
    BuiltinPgType { oid: OID_CHAR,        typname: "char",        typlen:  1, typbyval: "t", typtype: "b", typcategory: "Z", typcollation:   0 },
    BuiltinPgType { oid: OID_NAME,        typname: "name",        typlen: 64, typbyval: "f", typtype: "b", typcategory: "S", typcollation: 100 },
    BuiltinPgType { oid: OID_INT8,        typname: "int8",        typlen:  8, typbyval: "t", typtype: "b", typcategory: "N", typcollation:   0 },
    BuiltinPgType { oid: OID_INT2,        typname: "int2",        typlen:  2, typbyval: "t", typtype: "b", typcategory: "N", typcollation:   0 },
    BuiltinPgType { oid: OID_INT4,        typname: "int4",        typlen:  4, typbyval: "t", typtype: "b", typcategory: "N", typcollation:   0 },
    BuiltinPgType { oid: OID_TEXT,        typname: "text",        typlen: -1, typbyval: "f", typtype: "b", typcategory: "S", typcollation: 100 },
    BuiltinPgType { oid: OID_OID,         typname: "oid",         typlen:  4, typbyval: "t", typtype: "b", typcategory: "N", typcollation:   0 },
    BuiltinPgType { oid: OID_TID,         typname: "tid",         typlen:  6, typbyval: "f", typtype: "b", typcategory: "U", typcollation:   0 },
    BuiltinPgType { oid: OID_XID,         typname: "xid",         typlen:  4, typbyval: "t", typtype: "b", typcategory: "U", typcollation:   0 },
    BuiltinPgType { oid: OID_CID,         typname: "cid",         typlen:  4, typbyval: "t", typtype: "b", typcategory: "U", typcollation:   0 },
    BuiltinPgType { oid: OID_JSON,        typname: "json",        typlen: -1, typbyval: "f", typtype: "b", typcategory: "U", typcollation:   0 },
    BuiltinPgType { oid: OID_REGCLASS,    typname: "regclass",    typlen:  4, typbyval: "t", typtype: "b", typcategory: "N", typcollation:   0 },
    BuiltinPgType { oid: OID_REGTYPE,     typname: "regtype",     typlen:  4, typbyval: "t", typtype: "b", typcategory: "N", typcollation:   0 },
    BuiltinPgType { oid: OID_FLOAT4,      typname: "float4",      typlen:  4, typbyval: "t", typtype: "b", typcategory: "N", typcollation:   0 },
    BuiltinPgType { oid: OID_FLOAT8,      typname: "float8",      typlen:  8, typbyval: "t", typtype: "b", typcategory: "N", typcollation:   0 },
    BuiltinPgType { oid: OID_BPCHAR,      typname: "bpchar",      typlen: -1, typbyval: "f", typtype: "b", typcategory: "S", typcollation: 100 },
    BuiltinPgType { oid: OID_VARCHAR,     typname: "varchar",     typlen: -1, typbyval: "f", typtype: "b", typcategory: "S", typcollation: 100 },
    BuiltinPgType { oid: OID_DATE,        typname: "date",        typlen:  4, typbyval: "t", typtype: "b", typcategory: "D", typcollation:   0 },
    BuiltinPgType { oid: OID_TIME,        typname: "time",        typlen:  8, typbyval: "t", typtype: "b", typcategory: "D", typcollation:   0 },
    BuiltinPgType { oid: OID_TIMETZ,      typname: "timetz",      typlen: 12, typbyval: "f", typtype: "b", typcategory: "D", typcollation:   0 },
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

pub(crate) fn format_type_name_for_oid(oid: i64) -> Option<&'static str> {
    match oid {
        OID_TIME => Some("time without time zone"),
        OID_TIMETZ => Some("time with time zone"),
        OID_TID => Some("tid"),
        OID_XID => Some("xid"),
        OID_CID => Some("cid"),
        _ => typname_for_oid(oid),
    }
}

pub(crate) fn regtype_array_oid(base_oid: i64) -> Option<i64> {
    match base_oid {
        OID_BOOL => Some(OID_BOOL_ARRAY),
        OID_BYTEA => Some(OID_BYTEA_ARRAY),
        OID_NAME => Some(OID_NAME_ARRAY),
        OID_INT2 => Some(OID_INT2_ARRAY),
        OID_INT4 => Some(OID_INT4_ARRAY),
        OID_TEXT => Some(OID_TEXT_ARRAY),
        OID_BPCHAR => Some(OID_BPCHAR_ARRAY),
        OID_VARCHAR => Some(OID_VARCHAR_ARRAY),
        OID_INT8 => Some(OID_INT8_ARRAY),
        OID_FLOAT4 => Some(OID_FLOAT4_ARRAY),
        OID_FLOAT8 => Some(OID_FLOAT8_ARRAY),
        OID_OID => Some(OID_OID_ARRAY),
        OID_TIMESTAMP => Some(OID_TIMESTAMP_ARRAY),
        OID_DATE => Some(OID_DATE_ARRAY),
        OID_TIME => Some(OID_TIME_ARRAY),
        OID_TIMETZ => Some(OID_TIMETZ_ARRAY),
        OID_TIMESTAMPTZ => Some(OID_TIMESTAMPTZ_ARRAY),
        OID_INTERVAL => Some(OID_INTERVAL_ARRAY),
        OID_NUMERIC => Some(OID_NUMERIC_ARRAY),
        OID_JSON => Some(OID_JSON_ARRAY),
        OID_UUID => Some(OID_UUID_ARRAY),
        OID_JSONB => Some(OID_JSONB_ARRAY),
        OID_HSTORE => Some(OID_HSTORE_ARRAY),
        _ => None,
    }
}

pub(crate) fn pg_catalog_regtype_oid(name: &str) -> Option<i64> {
    match name {
        "bool" | "boolean" => Some(OID_BOOL),
        "bytea" => Some(OID_BYTEA),
        "name" => Some(OID_NAME),
        "int2" | "smallint" => Some(OID_INT2),
        "int4" | "integer" | "int" => Some(OID_INT4),
        "int8" | "bigint" => Some(OID_INT8),
        "text" => Some(OID_TEXT),
        "oid" => Some(OID_OID),
        "json" => Some(OID_JSON),
        "float4" | "real" => Some(OID_FLOAT4),
        "float8" | "double precision" => Some(OID_FLOAT8),
        "bpchar" | "character" => Some(OID_BPCHAR),
        "varchar" | "character varying" => Some(OID_VARCHAR),
        "date" => Some(OID_DATE),
        "time" | "time without time zone" => Some(OID_TIME),
        "timetz" | "time with time zone" => Some(OID_TIMETZ),
        "timestamp" | "timestamp without time zone" => Some(OID_TIMESTAMP),
        "timestamptz" | "timestamp with time zone" => Some(OID_TIMESTAMPTZ),
        "interval" => Some(OID_INTERVAL),
        "numeric" | "decimal" => Some(OID_NUMERIC),
        "uuid" => Some(OID_UUID),
        "tsvector" => Some(OID_TSVECTOR),
        "tsquery" => Some(OID_TSQUERY),
        "jsonb" => Some(OID_JSONB),
        "vector" => Some(OID_VECTOR),
        _ => None,
    }
}

pub(crate) fn actual_pg_catalog_regtype_oid(name: &str) -> Option<i64> {
    BUILTIN_PG_TYPES
        .iter()
        .find(|ty| ty.typname == name)
        .map(|ty| ty.oid)
        .or_else(|| {
            name.strip_prefix('_')
                .and_then(actual_pg_catalog_regtype_oid)
                .and_then(regtype_array_oid)
        })
}

pub(crate) fn visible_pg_catalog_regtype_oid(name: &str) -> Option<i64> {
    actual_pg_catalog_regtype_oid(name).or_else(|| pg_catalog_regtype_oid(name))
}

pub(crate) fn canonical_regtype_name(name: &str) -> String {
    let stripped = name.replace('"', "");
    let base_name = stripped.rsplit('.').next().unwrap_or(&stripped);

    match base_name.to_ascii_lowercase().as_str() {
        "int2" | "smallint" => "smallint".to_string(),
        "int4" | "integer" | "int" | "serial" => "integer".to_string(),
        "int8" | "bigint" | "bigserial" => "bigint".to_string(),
        "float4" | "real" => "real".to_string(),
        "float8" | "double precision" => "double precision".to_string(),
        "bool" | "boolean" => "boolean".to_string(),
        "varchar" | "character varying" => "character varying".to_string(),
        "bpchar" | "character" => "character".to_string(),
        "timestamp" | "timestamp without time zone" => "timestamp without time zone".to_string(),
        "timestamptz" | "timestamp with time zone" => "timestamp with time zone".to_string(),
        "time" | "time without time zone" => "time without time zone".to_string(),
        "timetz" | "time with time zone" => "time with time zone".to_string(),
        other => {
            if other == base_name {
                other.to_string()
            } else {
                base_name.to_string()
            }
        }
    }
}

pub(crate) fn regtype_text_for_oid(oid: i64) -> Option<&'static str> {
    match oid {
        OID_BOOL => Some("boolean"),
        OID_BYTEA => Some("bytea"),
        OID_CHAR => Some("char"),
        OID_NAME => Some("name"),
        OID_INT8 => Some("bigint"),
        OID_INT2 => Some("smallint"),
        OID_INT4 => Some("integer"),
        OID_TEXT => Some("text"),
        OID_OID => Some("oid"),
        OID_TID => Some("tid"),
        OID_XID => Some("xid"),
        OID_CID => Some("cid"),
        OID_JSON => Some("json"),
        OID_REGCLASS => Some("regclass"),
        OID_REGTYPE => Some("regtype"),
        OID_FLOAT4 => Some("real"),
        OID_FLOAT8 => Some("double precision"),
        OID_BPCHAR => Some("character"),
        OID_VARCHAR => Some("character varying"),
        OID_DATE => Some("date"),
        OID_TIME => Some("time without time zone"),
        OID_TIMETZ => Some("time with time zone"),
        OID_TIMESTAMP => Some("timestamp without time zone"),
        OID_TIMESTAMPTZ => Some("timestamp with time zone"),
        OID_INTERVAL => Some("interval"),
        OID_NUMERIC => Some("numeric"),
        OID_UUID => Some("uuid"),
        OID_TSVECTOR => Some("tsvector"),
        OID_TSQUERY => Some("tsquery"),
        OID_JSONB => Some("jsonb"),
        OID_VECTOR => Some("vector"),
        OID_BOOL_ARRAY => Some("boolean[]"),
        OID_BYTEA_ARRAY => Some("bytea[]"),
        OID_NAME_ARRAY => Some("name[]"),
        OID_INT2_ARRAY => Some("smallint[]"),
        OID_INT4_ARRAY => Some("integer[]"),
        OID_TEXT_ARRAY => Some("text[]"),
        OID_BPCHAR_ARRAY => Some("character[]"),
        OID_VARCHAR_ARRAY => Some("character varying[]"),
        OID_INT8_ARRAY => Some("bigint[]"),
        OID_FLOAT4_ARRAY => Some("real[]"),
        OID_FLOAT8_ARRAY => Some("double precision[]"),
        OID_OID_ARRAY => Some("oid[]"),
        OID_TIMESTAMP_ARRAY => Some("timestamp without time zone[]"),
        OID_DATE_ARRAY => Some("date[]"),
        OID_TIME_ARRAY => Some("time without time zone[]"),
        OID_TIMETZ_ARRAY => Some("time with time zone[]"),
        OID_TIMESTAMPTZ_ARRAY => Some("timestamp with time zone[]"),
        OID_INTERVAL_ARRAY => Some("interval[]"),
        OID_NUMERIC_ARRAY => Some("numeric[]"),
        OID_JSON_ARRAY => Some("json[]"),
        OID_UUID_ARRAY => Some("uuid[]"),
        OID_JSONB_ARRAY => Some("jsonb[]"),
        OID_HSTORE => Some("hstore"),
        OID_HSTORE_ARRAY => Some("hstore[]"),
        _ => None,
    }
}

pub(crate) fn oid_and_typlen_for_datatype(dt: &DataType) -> (i64, i32) {
    match dt {
        DataType::Boolean => (OID_BOOL, 1),
        DataType::Int32 => (OID_INT4, 4),
        DataType::Int64 => (OID_INT8, 8),
        DataType::Oid => (OID_OID, 4),
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
        DataType::Array(inner) => match inner.as_ref() {
            DataType::Boolean => (OID_BOOL_ARRAY, -1),
            DataType::Bytes => (OID_BYTEA_ARRAY, -1),
            DataType::Name => (OID_NAME_ARRAY, -1),
            DataType::Int32 => (OID_INT4_ARRAY, -1),
            DataType::Int64 => (OID_INT8_ARRAY, -1),
            DataType::Text => (OID_TEXT_ARRAY, -1),
            DataType::Varchar(_) => (OID_VARCHAR_ARRAY, -1),
            DataType::Float64 => (OID_FLOAT8_ARRAY, -1),
            DataType::Timestamp => (OID_TIMESTAMP_ARRAY, -1),
            DataType::TimestampTz => (OID_TIMESTAMPTZ_ARRAY, -1),
            DataType::Date => (OID_DATE_ARRAY, -1),
            DataType::Time => (OID_TIME_ARRAY, -1),
            DataType::Interval => (OID_INTERVAL_ARRAY, -1),
            DataType::Uuid => (OID_UUID_ARRAY, -1),
            DataType::Json => (OID_JSON_ARRAY, -1),
            DataType::Jsonb => (OID_JSONB_ARRAY, -1),
            DataType::Numeric { .. } => (OID_NUMERIC_ARRAY, -1),
            _ => (OID_TEXT_ARRAY, -1),
        },
        DataType::UserDefined(s) if s == "char" => (OID_CHAR, 1),
        DataType::UserDefined(s)
            if s.eq_ignore_ascii_case("regclass")
                || s.eq_ignore_ascii_case("pg_catalog.regclass") =>
        {
            (OID_REGCLASS, 4)
        }
        DataType::UserDefined(s)
            if s.eq_ignore_ascii_case("regtype")
                || s.eq_ignore_ascii_case("pg_catalog.regtype") =>
        {
            (OID_REGTYPE, 4)
        }
        DataType::Unknown => (705, -2),
        DataType::UserDefined(_) => (OID_TEXT, -1),
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

    #[test]
    fn format_type_name_maps_catalog_only_oids() {
        assert_eq!(format_type_name_for_oid(OID_CID), Some("cid"));
        assert_eq!(format_type_name_for_oid(OID_XID), Some("xid"));
        assert_eq!(format_type_name_for_oid(OID_TID), Some("tid"));
        assert_eq!(
            format_type_name_for_oid(OID_TIME),
            Some("time without time zone")
        );
    }

    #[test]
    fn typname_for_system_attribute_oids_exists() {
        assert_eq!(typname_for_oid(OID_TID), Some("tid"));
        assert_eq!(typname_for_oid(OID_XID), Some("xid"));
        assert_eq!(typname_for_oid(OID_CID), Some("cid"));
    }

    #[test]
    fn oid_and_typlen_maps_regclass_and_regtype() {
        assert_eq!(
            oid_and_typlen_for_datatype(&DataType::UserDefined("pg_catalog.regclass".to_string())),
            (OID_REGCLASS, 4)
        );
        assert_eq!(
            oid_and_typlen_for_datatype(&DataType::UserDefined("pg_catalog.regtype".to_string())),
            (OID_REGTYPE, 4)
        );
    }

    #[test]
    fn oid_and_typlen_maps_text_array() {
        assert_eq!(
            oid_and_typlen_for_datatype(&DataType::Array(Box::new(DataType::Text))),
            (OID_TEXT_ARRAY, -1)
        );
    }

    #[test]
    fn regtype_text_for_oid_uses_postgres_display_names() {
        assert_eq!(regtype_text_for_oid(OID_INT4), Some("integer"));
        assert_eq!(
            regtype_text_for_oid(OID_TIMETZ),
            Some("time with time zone")
        );
        assert_eq!(
            regtype_text_for_oid(OID_VARCHAR_ARRAY),
            Some("character varying[]")
        );
    }

    #[test]
    fn canonical_regtype_name_preserves_unknown_names_and_normalizes_aliases() {
        assert_eq!(canonical_regtype_name("pg_catalog.int4"), "integer");
        assert_eq!(
            canonical_regtype_name("\"pg_catalog\".\"timetz\""),
            "time with time zone"
        );
        assert_eq!(canonical_regtype_name("\"MixedCaseType\""), "MixedCaseType");
    }

    #[test]
    fn visible_pg_catalog_regtype_oid_matches_builtin_aliases_and_array_aliases() {
        assert_eq!(visible_pg_catalog_regtype_oid("text"), Some(OID_TEXT));
        assert_eq!(visible_pg_catalog_regtype_oid("integer"), Some(OID_INT4));
        assert_eq!(
            visible_pg_catalog_regtype_oid("_int4"),
            Some(OID_INT4_ARRAY)
        );
        assert_eq!(visible_pg_catalog_regtype_oid("not_a_builtin"), None);
    }
}
