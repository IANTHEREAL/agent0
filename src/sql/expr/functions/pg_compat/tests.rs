use crate::sql::expr::functions::string::{quote_ident, quote_literal, quote_nullable};

use super::*;

#[test]
fn test_pg_typeof() {
    assert_eq!(
        pg_typeof(vec![Value::Int32(42)]).unwrap(),
        Value::Text("integer".into())
    );
    assert_eq!(
        pg_typeof(vec![Value::Text("hello".into())]).unwrap(),
        Value::Text("text".into())
    );
    assert_eq!(
        pg_typeof(vec![Value::Null]).unwrap(),
        Value::Text("unknown".into())
    );
}

#[test]
fn test_quote_ident() {
    assert_eq!(
        quote_ident(vec![Value::Text("simple".into())]).unwrap(),
        Value::Text("simple".into())
    );
    assert_eq!(
        quote_ident(vec![Value::Text("SELECT".into())]).unwrap(),
        Value::Text("\"SELECT\"".into())
    );
    assert_eq!(
        quote_ident(vec![Value::Text("has space".into())]).unwrap(),
        Value::Text("\"has space\"".into())
    );
}

#[test]
fn test_quote_literal() {
    assert_eq!(
        quote_literal(vec![Value::Text("hello".into())]).unwrap(),
        Value::Text("'hello'".into())
    );
    assert_eq!(
        quote_literal(vec![Value::Text("it's".into())]).unwrap(),
        Value::Text("'it''s'".into())
    );
    assert_eq!(
        quote_literal(vec![Value::Text("a\\b".into())]).unwrap(),
        Value::Text(r"E'a\\b'".into())
    );
}

#[test]
fn test_quote_nullable() {
    assert_eq!(
        quote_nullable(vec![Value::Null]).unwrap(),
        Value::Text("NULL".into())
    );
    assert_eq!(
        quote_nullable(vec![Value::Text("hello".into())]).unwrap(),
        Value::Text("'hello'".into())
    );
    assert_eq!(
        quote_nullable(vec![Value::Text("a\\b".into())]).unwrap(),
        Value::Text(r"E'a\\b'".into())
    );
}

#[test]
fn test_pg_column_size() {
    assert_eq!(
        pg_column_size(vec![Value::Int32(42)]).unwrap(),
        Value::Int32(4)
    );
    assert_eq!(
        pg_column_size(vec![Value::Int64(42)]).unwrap(),
        Value::Int32(8)
    );
}

#[test]
fn test_format_type() {
    assert_eq!(
        format_type(vec![Value::Int32(23)]).unwrap(),
        Value::Text("int4".into())
    );
    assert_eq!(
        format_type(vec![Value::Int32(25)]).unwrap(),
        Value::Text("text".into())
    );
    assert_eq!(
        format_type(vec![Value::Int32(1700)]).unwrap(),
        Value::Text("numeric".into())
    );
    assert_eq!(
        format_type(vec![Value::Int32(1083)]).unwrap(),
        Value::Text("time without time zone".into())
    );
    assert_eq!(
        format_type(vec![Value::Int32(1186)]).unwrap(),
        Value::Text("interval".into())
    );
    // 2-arg form: VARCHAR with typmod
    assert_eq!(
        format_type(vec![Value::Int32(1043), Value::Int32(7)]).unwrap(),
        Value::Text("character varying(3)".into())
    );
    // 2-arg form: VARCHAR without typmod
    assert_eq!(
        format_type(vec![Value::Int32(1043), Value::Int32(-1)]).unwrap(),
        Value::Text("character varying".into())
    );
}

#[test]
fn test_to_regtype() {
    assert_eq!(
        to_regtype(vec![Value::Text("hstore".into())]).unwrap(),
        Value::Null
    );
    assert_eq!(
        to_regtype(vec![Value::Text("hstore[]".into())]).unwrap(),
        Value::Null
    );
    assert_eq!(
        to_regtype(vec![Value::Text("public.hstore".into())]).unwrap(),
        Value::Null
    );
    assert_eq!(
        to_regtype(vec![Value::Text("\"public\".\"hstore\"".into())]).unwrap(),
        Value::Null
    );
    assert_eq!(
        to_regtype(vec![Value::Text("integer".into())]).unwrap(),
        Value::Int64(pg_types::OID_INT4)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("integer[]".into())]).unwrap(),
        Value::Int64(pg_types::OID_INT4_ARRAY)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("varchar(5)".into())]).unwrap(),
        Value::Int64(pg_types::OID_VARCHAR)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("numeric(10,2)".into())]).unwrap(),
        Value::Int64(pg_types::OID_NUMERIC)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("pg_catalog.int4".into())]).unwrap(),
        Value::Int64(pg_types::OID_INT4)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("serial".into())]).unwrap(),
        Value::Null
    );
    assert_eq!(
        to_regtype(vec![Value::Text("bigserial".into())]).unwrap(),
        Value::Null
    );
    assert_eq!(
        to_regtype(vec![Value::Text("not_a_type".into())]).unwrap(),
        Value::Null
    );
    assert_eq!(to_regtype(vec![Value::Null]).unwrap(), Value::Null);
    // --- timetz support (PR #1447) ---
    assert_eq!(
        to_regtype(vec![Value::Text("timetz".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIMETZ)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("time with time zone".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIMETZ)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("pg_catalog.timetz".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIMETZ)
    );
    // Valid timetz precision (0-6)
    assert_eq!(
        to_regtype(vec![Value::Text("pg_catalog.timetz(3)".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIMETZ)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("timetz(0)".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIMETZ)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("timetz(6)".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIMETZ)
    );
    // Precision > 6 is clamped (PG returns OID with a warning)
    assert_eq!(
        to_regtype(vec![Value::Text("pg_catalog.timetz(7)".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIMETZ)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("timetz(7)".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIMETZ)
    );
    // Negative precision → error (PG: TIME(-1) WITH TIME ZONE precision must not be negative)
    let err = to_regtype(vec![Value::Text("pg_catalog.timetz(-1)".into())]).unwrap_err();
    assert!(
        err.to_string()
            .contains("TIME(-1) WITH TIME ZONE precision must not be negative"),
        "unexpected error: {err}"
    );
    // Non-integer precision → error
    assert!(to_regtype(vec![Value::Text("pg_catalog.timetz(foo)".into())]).is_err());
    // Other temporal types: precision > 6 also returns OID (clamped)
    assert_eq!(
        to_regtype(vec![Value::Text("time(7)".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIME)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("timestamp(7)".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIMESTAMP)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("timestamptz(7)".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIMESTAMPTZ)
    );
    // Bare timetz(-1) → specific precision error (PG parity)
    let err = to_regtype(vec![Value::Text("timetz(-1)".into())]).unwrap_err();
    assert!(
        err.to_string()
            .contains("TIME(-1) WITH TIME ZONE precision must not be negative"),
        "unexpected error: {err}"
    );
    // Unknown schema + temporal typmod → NULL (no validation, no error)
    assert_eq!(
        to_regtype(vec![Value::Text("foo.timetz(-1)".into())]).unwrap(),
        Value::Null
    );
    assert_eq!(
        to_regtype(vec![Value::Text("foo.time(foo)".into())]).unwrap(),
        Value::Null
    );
    // --- regtype parity (PR #1425) ---
    // Quoted schema case-sensitivity: quoted "PG_CATALOG" ≠ pg_catalog → NULL
    assert_eq!(
        to_regtype(vec![Value::Text("\"PG_CATALOG\".int4".into())]).unwrap(),
        Value::Null
    );
    // Quoted "pg_catalog" (exact case) → resolves normally
    assert_eq!(
        to_regtype(vec![Value::Text("\"pg_catalog\".int4".into())]).unwrap(),
        Value::Int64(pg_types::OID_INT4)
    );
    // Multi-word PG type names must resolve, not error as trailing junk.
    assert_eq!(
        to_regtype(vec![Value::Text("double precision".into())]).unwrap(),
        Value::Int64(pg_types::OID_FLOAT8)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("character varying".into())]).unwrap(),
        Value::Int64(pg_types::OID_VARCHAR)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("character varying(255)".into())]).unwrap(),
        Value::Int64(pg_types::OID_VARCHAR)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("timestamp with time zone".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIMESTAMPTZ)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("timestamp without time zone".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIMESTAMP)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("time without time zone".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIME)
    );
    // Multi-word types not in our OID table → NULL (not error)
    assert_eq!(
        to_regtype(vec![Value::Text("bit varying".into())]).unwrap(),
        Value::Null
    );
    // Case insensitive (unquoted identifiers are lowercased)
    assert_eq!(
        to_regtype(vec![Value::Text("Double Precision".into())]).unwrap(),
        Value::Int64(pg_types::OID_FLOAT8)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("CHARACTER VARYING".into())]).unwrap(),
        Value::Int64(pg_types::OID_VARCHAR)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("TIMESTAMP WITH TIME ZONE".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIMESTAMPTZ)
    );
    // Schema-qualified multi-word forms are syntax errors in PG.
    assert!(to_regtype(vec![Value::Text("pg_catalog.double precision".into())]).is_err());
    assert!(to_regtype(vec![Value::Text("pg_catalog.character varying".into())]).is_err());
    assert!(to_regtype(vec![Value::Text(
        "pg_catalog.timestamp with time zone".into()
    )])
    .is_err());
    // Schema-qualified typmods on compatible types resolve.
    assert_eq!(
        to_regtype(vec![Value::Text("pg_catalog.varchar(5)".into())]).unwrap(),
        Value::Int64(pg_types::OID_VARCHAR)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("pg_catalog.numeric(10,2)".into())]).unwrap(),
        Value::Int64(pg_types::OID_NUMERIC)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("pg_catalog.timestamp(3)".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIMESTAMP)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("pg_catalog.time(3)".into())]).unwrap(),
        Value::Int64(pg_types::OID_TIME)
    );
    // Schema-qualified typmods on incompatible builtins are errors.
    assert!(to_regtype(vec![Value::Text("pg_catalog.interval(3)".into())]).is_err());
    assert!(to_regtype(vec![Value::Text("pg_catalog.int4(1)".into())]).is_err());
    assert!(to_regtype(vec![Value::Text("pg_catalog.text(3)".into())]).is_err());
    // Unknown type in known schema + typmod syntax resolves to NULL.
    assert_eq!(
        to_regtype(vec![Value::Text("pg_catalog.foo(abc)".into())]).unwrap(),
        Value::Null
    );
    // Typmod semantic bounds: varchar/character length >= 1
    assert!(to_regtype(vec![Value::Text("varchar(0)".into())]).is_err());
    assert!(to_regtype(vec![Value::Text("character(0)".into())]).is_err());
    assert!(to_regtype(vec![Value::Text("pg_catalog.varchar(0)".into())]).is_err());
    // "character" is a parser alias, not a real pg_catalog type.
    // PG returns NULL for pg_catalog.character(...) regardless of typmod.
    assert_eq!(
        to_regtype(vec![Value::Text("pg_catalog.character(0)".into())]).unwrap(),
        Value::Null
    );
    assert_eq!(
        to_regtype(vec![Value::Text("pg_catalog.character(5)".into())]).unwrap(),
        Value::Null
    );
    // "decimal" is a parser alias, not a real pg_catalog type.
    assert_eq!(
        to_regtype(vec![Value::Text("pg_catalog.decimal(10,2)".into())]).unwrap(),
        Value::Null
    );
    assert_eq!(
        to_regtype(vec![Value::Text("varchar(1)".into())]).unwrap(),
        Value::Int64(pg_types::OID_VARCHAR)
    );
    // Typmod semantic bounds: numeric precision 1-1000
    assert!(to_regtype(vec![Value::Text("numeric(0)".into())]).is_err());
    assert!(to_regtype(vec![Value::Text("numeric(1001)".into())]).is_err());
    assert!(to_regtype(vec![Value::Text("pg_catalog.numeric(0)".into())]).is_err());
    assert!(to_regtype(vec![Value::Text("pg_catalog.numeric(1001)".into())]).is_err());
    assert_eq!(
        to_regtype(vec![Value::Text("numeric(1)".into())]).unwrap(),
        Value::Int64(pg_types::OID_NUMERIC)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("numeric(1000)".into())]).unwrap(),
        Value::Int64(pg_types::OID_NUMERIC)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("numeric(10,10)".into())]).unwrap(),
        Value::Int64(pg_types::OID_NUMERIC)
    );
    // PG accepts scale > precision and negative scale
    assert_eq!(
        to_regtype(vec![Value::Text("numeric(10,11)".into())]).unwrap(),
        Value::Int64(pg_types::OID_NUMERIC)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("pg_catalog.numeric(10,11)".into())]).unwrap(),
        Value::Int64(pg_types::OID_NUMERIC)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("numeric(10,-2)".into())]).unwrap(),
        Value::Int64(pg_types::OID_NUMERIC)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("pg_catalog.numeric(10,-2)".into())]).unwrap(),
        Value::Int64(pg_types::OID_NUMERIC)
    );
    // Negative precision for bare temporal types (PG parity):
    // time/timestamp → "invalid type name", timestamptz/timetz → precision error.
    assert!(to_regtype(vec![Value::Text("time(-1)".into())]).is_err());
    assert!(to_regtype(vec![Value::Text("timestamp(-1)".into())]).is_err());
    let err = to_regtype(vec![Value::Text("timestamptz(-1)".into())]).unwrap_err();
    assert!(
        err.to_string()
            .contains("TIMESTAMP(-1) WITH TIME ZONE precision must not be negative"),
        "{err}"
    );
    // Negative precision for schema-qualified temporal types → specific error (PG parity)
    let err = to_regtype(vec![Value::Text("pg_catalog.time(-1)".into())]).unwrap_err();
    assert!(
        err.to_string()
            .contains("TIME(-1) precision must not be negative"),
        "{err}"
    );
    let err = to_regtype(vec![Value::Text("pg_catalog.timestamp(-1)".into())]).unwrap_err();
    assert!(
        err.to_string()
            .contains("TIMESTAMP(-1) precision must not be negative"),
        "{err}"
    );
    let err = to_regtype(vec![Value::Text("pg_catalog.timestamptz(-1)".into())]).unwrap_err();
    assert!(
        err.to_string()
            .contains("TIMESTAMP(-1) WITH TIME ZONE precision must not be negative"),
        "{err}"
    );
    // Empty typmod (parens present, no arguments) → error (PG parity)
    assert!(to_regtype(vec![Value::Text("pg_catalog.character()".into())]).is_err());
    assert!(to_regtype(vec![Value::Text("pg_catalog.decimal()".into())]).is_err());
}

#[test]
fn test_pg_type_is_visible() {
    assert_eq!(
        pg_type_is_visible(vec![Value::Int32(12345)]).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(pg_type_is_visible(vec![Value::Null]).unwrap(), Value::Null);
    assert_eq!(pg_type_is_visible(vec![]).unwrap(), Value::Null);
}

#[test]
fn test_pg_table_is_visible() {
    assert_eq!(
        pg_table_is_visible(vec![Value::Int32(12345)]).unwrap(),
        Value::Boolean(true)
    );
    assert_eq!(pg_table_is_visible(vec![Value::Null]).unwrap(), Value::Null);
    assert_eq!(pg_table_is_visible(vec![]).unwrap(), Value::Null);
}

#[test]
fn test_to_regtype_array_aliases() {
    // Gap 1: generic _typename → array OID
    assert_eq!(
        to_regtype(vec![Value::Text("_int4".into())]).unwrap(),
        Value::Int64(pg_types::OID_INT4_ARRAY)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("_bool".into())]).unwrap(),
        Value::Int64(pg_types::OID_BOOL_ARRAY)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("_text".into())]).unwrap(),
        Value::Int64(pg_types::OID_TEXT_ARRAY)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("_float8".into())]).unwrap(),
        Value::Int64(pg_types::OID_FLOAT8_ARRAY)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("_varchar".into())]).unwrap(),
        Value::Int64(pg_types::OID_VARCHAR_ARRAY)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("_numeric".into())]).unwrap(),
        Value::Int64(pg_types::OID_NUMERIC_ARRAY)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("_uuid".into())]).unwrap(),
        Value::Int64(pg_types::OID_UUID_ARRAY)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("_jsonb".into())]).unwrap(),
        Value::Int64(pg_types::OID_JSONB_ARRAY)
    );
    // _hstore is extension-defined (not pg_catalog builtin) in db9.
    assert_eq!(
        to_regtype(vec![Value::Text("_hstore".into())]).unwrap(),
        Value::Null
    );
    // Schema-qualified _typename
    assert_eq!(
        to_regtype(vec![Value::Text("pg_catalog._int4".into())]).unwrap(),
        Value::Int64(pg_types::OID_INT4_ARRAY)
    );
    // Unknown base type → NULL
    assert_eq!(
        to_regtype(vec![Value::Text("_nonexistent".into())]).unwrap(),
        Value::Null
    );
    // Array-of-array not valid
    assert_eq!(
        to_regtype(vec![Value::Text("_int4[]".into())]).unwrap(),
        Value::Null
    );
    // Quoted _typename aliases must NOT resolve (case-sensitive, no alias folding).
    assert_eq!(
        to_regtype(vec![Value::Text("\"_INT4\"".into())]).unwrap(),
        Value::Null
    );
    assert_eq!(
        to_regtype(vec![Value::Text("pg_catalog.\"_INT4\"".into())]).unwrap(),
        Value::Null
    );
}

#[test]
fn test_to_regtype_interval_qualifiers() {
    // Gap 2: interval qualifier forms → OID 1186
    assert_eq!(
        to_regtype(vec![Value::Text("interval day to second".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("interval hour".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("interval year to month".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    // Precision on SECOND is valid
    assert_eq!(
        to_regtype(vec![Value::Text("interval second(3)".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("interval day to second(3)".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("interval(3)".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    // Edge: precision 0 and 6 are valid bounds
    assert_eq!(
        to_regtype(vec![Value::Text("interval(0)".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("interval(6)".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    // Case-insensitive
    assert_eq!(
        to_regtype(vec![Value::Text("INTERVAL DAY TO SECOND".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    // Precision on non-SECOND qualifier → error (C2, C3)
    assert!(to_regtype(vec![Value::Text("interval minute(3)".into())]).is_err());
    assert!(to_regtype(vec![Value::Text("interval year(2)".into())]).is_err());
    // Unknown qualifier → error (C1)
    assert!(to_regtype(vec![Value::Text("interval garbage".into())]).is_err());
    // Malformed typmod content → error (C8)
    assert!(to_regtype(vec![Value::Text("interval(abc)".into())]).is_err());
    // Non-negative out-of-range precision → OID (PG clamps, C9)
    assert_eq!(
        to_regtype(vec![Value::Text("interval(999)".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("interval(7)".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("interval(2147483647)".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    assert!(to_regtype(vec![Value::Text("interval(2147483648)".into())]).is_err());
    // Negative precision → error (PG Iconst rejects '-', C10)
    assert!(to_regtype(vec![Value::Text("interval(-1)".into())]).is_err());
    assert!(to_regtype(vec![Value::Text("interval(+1)".into())]).is_err());
    // Not a word-boundary match → NULL (unknown type, not an interval) (C6)
    assert_eq!(
        to_regtype(vec![Value::Text("intervals".into())]).unwrap(),
        Value::Null
    );
    // Nonexistent type → NULL (C7)
    assert_eq!(
        to_regtype(vec![Value::Text("nonexistent_type".into())]).unwrap(),
        Value::Null
    );
    // Whitespace normalization: multi-space, tab, space before precision
    assert_eq!(
        to_regtype(vec![Value::Text("interval  day   to   second".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("interval\tday".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("interval (3)".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("interval  (3)".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("interval\t(3)".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    assert_eq!(
        to_regtype(vec![Value::Text("interval  second  (3)".into())]).unwrap(),
        Value::Int64(pg_types::OID_INTERVAL)
    );
    // Schema-qualified interval with qualifier → ERROR (PG parity).
    // PG treats trailing qualifier as invalid type name junk.
    assert!(to_regtype(vec![Value::Text(
        "pg_catalog.interval day to second".into()
    )])
    .is_err());
    // Unknown schema + invalid interval typmod → NULL (schema resolution
    // before interval validation: schema not found = NULL, no error).
    assert_eq!(
        to_regtype(vec![Value::Text("noschema.interval(abc)".into())]).unwrap(),
        Value::Null
    );
    // Bare word after type name → syntax error (PG's parser rejects it).
    assert!(to_regtype(vec![Value::Text("noschema.interval day to second".into())]).is_err());
    assert!(to_regtype(vec![Value::Text("noschema.interval garbage".into())]).is_err());
    // Non-interval types with trailing junk → syntax error.
    assert!(to_regtype(vec![Value::Text("noschema.int4 garbage".into())]).is_err());
    assert!(to_regtype(vec![Value::Text("noschema.foo garbage".into())]).is_err());
    // Unqualified trailing junk → syntax error (PG parity).
    assert!(to_regtype(vec![Value::Text("int4 garbage".into())]).is_err());
    assert!(to_regtype(vec![Value::Text("text garbage".into())]).is_err());
    // Quoted schema case-mismatch + invalid interval typmod → NULL
    assert_eq!(
        to_regtype(vec![Value::Text("\"PG_CATALOG\".interval(abc)".into())]).unwrap(),
        Value::Null
    );
}

#[test]
fn test_to_regtype_quoted_identifier_errors() {
    let err = to_regtype(vec![Value::Text("\"text".into())]).unwrap_err();
    assert!(
        err.to_string()
            .contains("unterminated quoted identifier at or near \"\"text\""),
        "{err}"
    );

    let err = to_regtype(vec![Value::Text("\"\"".into())]).unwrap_err();
    assert!(
        err.to_string()
            .contains("zero-length delimited identifier at or near \"\"\"\""),
        "{err}"
    );

    let err = to_regtype(vec![Value::Text("\"text\"(3)".into())]).unwrap_err();
    assert!(
        err.to_string()
            .contains("type modifier is not allowed for type \"text\""),
        "{err}"
    );

    assert_eq!(
        to_regtype(vec![Value::Text("\"TEXT\"(3)".into())]).unwrap(),
        Value::Null
    );

    let err = to_regtype(vec![Value::Text("a.b.c".into())]).unwrap_err();
    assert!(
        err.to_string()
            .contains("cross-database references are not implemented: a.b.c"),
        "{err}"
    );
}

#[test]
fn test_parse_regtype_lookup_unescapes_quoted_identifiers() {
    let lookup = parse_regtype_lookup("\"a\"\"b\"").unwrap().unwrap();
    assert_eq!(lookup.name, "a\"b");
    assert!(lookup.quoted);
    assert_eq!(lookup.kind, ParsedRegtypeLookupKind::SearchPath);
}

#[test]
fn test_normalize_interval_type() {
    assert_eq!(normalize_interval_type("interval").unwrap(), "interval");
    assert_eq!(
        normalize_interval_type("interval day to second").unwrap(),
        "interval"
    );
    assert_eq!(
        normalize_interval_type("interval hour").unwrap(),
        "interval"
    );
    assert_eq!(
        normalize_interval_type("interval second(3)").unwrap(),
        "interval"
    );
    assert_eq!(
        normalize_interval_type("interval day to second(3)").unwrap(),
        "interval"
    );
    assert_eq!(
        normalize_interval_type("INTERVAL YEAR TO MONTH").unwrap(),
        "interval"
    );
    assert_eq!(normalize_interval_type("interval(0)").unwrap(), "interval");
    assert_eq!(normalize_interval_type("interval(6)").unwrap(), "interval");
    // Precision on non-SECOND qualifier → error
    assert!(normalize_interval_type("interval minute(3)").is_err());
    assert!(normalize_interval_type("interval year(2)").is_err());
    // Unknown qualifier → error
    assert!(normalize_interval_type("interval garbage").is_err());
    // Malformed precision content → error
    assert!(normalize_interval_type("interval(abc)").is_err());
    // Non-negative out-of-range precision → Some (PG clamps, returns OID)
    assert_eq!(
        normalize_interval_type("interval(999)").unwrap(),
        "interval"
    );
    assert_eq!(normalize_interval_type("interval(7)").unwrap(), "interval");
    // Iconst upper bound is signed 32-bit.
    assert_eq!(
        normalize_interval_type("interval(2147483647)").unwrap(),
        "interval"
    );
    assert!(normalize_interval_type("interval(2147483648)").is_err());
    // Negative precision → error (PG Iconst rejects '-')
    assert!(normalize_interval_type("interval(-1)").is_err());
    assert!(normalize_interval_type("interval(+1)").is_err());
    // Not interval types — returned as-is
    assert_eq!(normalize_interval_type("intervals").unwrap(), "intervals");
    assert_eq!(normalize_interval_type("integer").unwrap(), "integer");
    // Whitespace normalization: multi-space, tab, space before precision
    assert_eq!(
        normalize_interval_type("interval  day   to   second").unwrap(),
        "interval"
    );
    assert_eq!(
        normalize_interval_type("interval\tday").unwrap(),
        "interval"
    );
    assert_eq!(normalize_interval_type("interval (3)").unwrap(), "interval");
    assert_eq!(
        normalize_interval_type("interval  (3)").unwrap(),
        "interval"
    );
    assert_eq!(
        normalize_interval_type("interval\t(3)").unwrap(),
        "interval"
    );
    assert_eq!(
        normalize_interval_type("interval  second  (3)").unwrap(),
        "interval"
    );
}

#[test]
fn test_pg_partition_ancestors_stub() {
    assert_eq!(
        pg_partition_ancestors(vec![Value::Int64(12345)]).unwrap(),
        Value::Null
    );
    assert_eq!(
        pg_partition_ancestors(vec![Value::Null]).unwrap(),
        Value::Null
    );
}

#[test]
fn test_int4send() {
    // int4send(16909060) → \x01020304 (big-endian)
    assert_eq!(
        int4send(vec![Value::Int32(16909060)]).unwrap(),
        Value::Bytes(vec![0x01, 0x02, 0x03, 0x04])
    );
    assert_eq!(int4send(vec![Value::Null]).unwrap(), Value::Null);
}

#[test]
fn test_int8send() {
    // int8send(72623859790382856) → \x0102030405060708 (big-endian)
    assert_eq!(
        int8send(vec![Value::Int64(72623859790382856)]).unwrap(),
        Value::Bytes(vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08])
    );
    assert_eq!(int8send(vec![Value::Null]).unwrap(), Value::Null);
}

#[test]
fn test_set_bit_bytea() {
    // PG 17.7: set_bit('\x00'::bytea, 0, 1) → \x01 (bit 0 = LSB)
    assert_eq!(
        set_bit_bytea(vec![
            Value::Bytes(vec![0x00]),
            Value::Int32(0),
            Value::Int32(1)
        ])
        .unwrap(),
        Value::Bytes(vec![0x01])
    );
    // PG 17.7: set_bit('\x00'::bytea, 7, 1) → \x80 (bit 7 = MSB)
    assert_eq!(
        set_bit_bytea(vec![
            Value::Bytes(vec![0x00]),
            Value::Int32(7),
            Value::Int32(1)
        ])
        .unwrap(),
        Value::Bytes(vec![0x80])
    );
}

#[test]
fn test_get_bit_bytea() {
    // PG 17.7: get_bit('\x80'::bytea, 0) → 0 (bit 0 = LSB)
    assert_eq!(
        get_bit_bytea(vec![Value::Bytes(vec![0x80]), Value::Int32(0)]).unwrap(),
        Value::Int32(0)
    );
    // PG 17.7: get_bit('\x80'::bytea, 7) → 1 (bit 7 = MSB)
    assert_eq!(
        get_bit_bytea(vec![Value::Bytes(vec![0x80]), Value::Int32(7)]).unwrap(),
        Value::Int32(1)
    );
}

#[test]
fn test_hashtext() {
    let h1 = hashtext(vec![Value::Text("hello".into())]).unwrap();
    let h2 = hashtext(vec![Value::Text("hello".into())]).unwrap();
    assert_eq!(h1, h2);

    let h3 = hashtext(vec![Value::Text("world".into())]).unwrap();
    assert_ne!(h1, h3);

    assert_eq!(hashtext(vec![Value::Null]).unwrap(), Value::Null);
}
