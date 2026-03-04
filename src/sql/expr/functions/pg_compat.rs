use crate::model::Value;
use crate::sql::pg_types;
use crate::sql::quoting;
use anyhow::Result;
use std::collections::HashMap;

use super::SqlFn;

pub fn register(map: &mut HashMap<&'static str, SqlFn>) {
    map.insert("PG_TYPEOF", pg_typeof);
    map.insert("PG_COLUMN_SIZE", pg_column_size);
    map.insert("FORMAT_TYPE", format_type);
    map.insert("TO_REGTYPE", to_regtype);
    map.insert("PG_IS_IN_RECOVERY", pg_is_in_recovery);
    map.insert("PG_TABLE_IS_VISIBLE", pg_table_is_visible);
    map.insert("PG_TYPE_IS_VISIBLE", pg_type_is_visible);
    map.insert("CLOCK_TIMESTAMP", clock_timestamp);
    // STATEMENT_TIMESTAMP and TRANSACTION_TIMESTAMP are handled as special cases
    // in eval_function (expr/mod.rs) because they need access to QueryContext.
    map.insert("TXID_CURRENT", txid_current);
    map.insert("PG_ENCODING_TO_CHAR", pg_encoding_to_char);
    map.insert("OBJ_DESCRIPTION", obj_description);
    map.insert("COL_DESCRIPTION", obj_description);
    map.insert("SHOBJ_DESCRIPTION", obj_description);
    map.insert("PG_GET_SERIAL_SEQUENCE", pg_get_serial_sequence);
    map.insert("PG_GET_EXPR", pg_get_expr);
    map.insert(
        "PG_GET_STATISTICSOBJDEF_COLUMNS",
        pg_get_statisticsobjdef_columns,
    );
    map.insert("HAS_SCHEMA_PRIVILEGE", has_privilege);
    map.insert("HAS_TABLE_PRIVILEGE", has_privilege);
    map.insert("HAS_DATABASE_PRIVILEGE", has_privilege);
    map.insert("PG_RELATION_IS_PUBLISHABLE", pg_relation_is_publishable);
    map.insert("PG_PARTITION_ANCESTORS", pg_partition_ancestors);
    // Binary send functions (bytea serialization)
    map.insert("INT4SEND", int4send);
    map.insert("INT8SEND", int8send);
    map.insert("UUID_SEND", uuid_send);
    // Bit manipulation on bytea
    map.insert("SET_BIT", set_bit_bytea);
    map.insert("GET_BIT", get_bit_bytea);
    map.insert("HASHTEXT", hashtext);
}

fn pg_typeof_name(val: &Value) -> String {
    match val {
        Value::Null => "unknown".to_string(),
        Value::Boolean(_) => "boolean".to_string(),
        Value::Int32(_) => "integer".to_string(),
        Value::Int64(_) => "bigint".to_string(),
        Value::Float64(_) => "double precision".to_string(),
        Value::Numeric(_) => "numeric".to_string(),
        Value::Text(_) => "text".to_string(),
        Value::Bytes(_) => "bytea".to_string(),
        Value::Timestamp(_) => "timestamp with time zone".to_string(),
        Value::Date(_) => "date".to_string(),
        Value::Time(_) => "time".to_string(),
        Value::Interval(_) => "interval".to_string(),
        Value::Uuid(_) => "uuid".to_string(),
        Value::Json(_) => "json".to_string(),
        Value::Jsonb(_) => "jsonb".to_string(),
        Value::Array(arr) => {
            let elem_type = arr
                .iter()
                .find(|v| !matches!(v, Value::Null))
                .map(pg_typeof_name)
                .unwrap_or_else(|| "unknown".to_string());
            format!("{}[]", elem_type)
        }
        Value::Vector(_) => "vector".to_string(),
        Value::Tsvector(_) => "tsvector".to_string(),
        Value::Tsquery(_) => "tsquery".to_string(),
    }
}

pub fn pg_typeof(args: Vec<Value>) -> Result<Value> {
    let val = args.into_iter().next().unwrap_or(Value::Null);
    Ok(Value::Text(pg_typeof_name(&val)))
}

pub fn pg_column_size(args: Vec<Value>) -> Result<Value> {
    let val = args.into_iter().next().unwrap_or(Value::Null);
    let size = match &val {
        Value::Null => 0,
        Value::Boolean(_) => 1,
        Value::Int32(_) => 4,
        Value::Int64(_) => 8,
        Value::Float64(_) => 8,
        Value::Numeric(_) => 16,
        Value::Text(s) => s.len() as i32 + 4,
        Value::Bytes(b) => b.len() as i32 + 4,
        Value::Timestamp(_) => 8,
        Value::Date(_) => 4,
        Value::Time(_) => 8,
        Value::Interval(_) => 16,
        Value::Uuid(_) => 16,
        Value::Json(s) | Value::Jsonb(s) => s.len() as i32 + 4,
        Value::Array(a) => a.len() as i32 * 8 + 4,
        Value::Vector(v) => v.len() as i32 * 4 + 4,
        Value::Tsvector(s) | Value::Tsquery(s) => s.len() as i32 + 4,
    };
    Ok(Value::Int32(size))
}

pub fn format_type(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let oid = match iter.next().unwrap_or(Value::Null) {
        Value::Int32(n) => n as i64,
        Value::Int64(n) => n,
        Value::Text(s) => s.trim().parse::<i64>().map_err(|_| {
            anyhow::anyhow!(
                "{}",
                crate::sql::error::SqlError::InvalidInputSyntax {
                    type_name: "oid".into(),
                    value: s.trim().to_string(),
                }
            )
        })?,
        Value::Null => return Ok(Value::Null),
        _ => 0,
    };
    let typmod = match iter.next() {
        Some(Value::Int32(n)) => n as i64,
        Some(Value::Int64(n)) => n,
        _ => -1,
    };
    // Handle types that need typmod for canonical name
    let formatted = match oid {
        pg_types::OID_VARCHAR => {
            if typmod > 0 {
                format!("character varying({})", typmod - 4)
            } else {
                "character varying".to_string()
            }
        }
        pg_types::OID_BPCHAR => {
            if typmod > 0 {
                format!("character({})", typmod - 4)
            } else {
                "character".to_string()
            }
        }
        pg_types::OID_NUMERIC => {
            if typmod > 0 {
                let precision = ((typmod - 4) >> 16) & 0xffff;
                let scale = (typmod - 4) & 0xffff;
                format!("numeric({},{})", precision, scale)
            } else {
                "numeric".to_string()
            }
        }
        _ => pg_types::format_type_name_for_oid(oid)
            .unwrap_or("text")
            .to_string(),
    };
    Ok(Value::Text(formatted))
}

/// Parsed identifier component of a regtype input.
/// Tracks whether the identifier was double-quoted, which controls
/// case-sensitivity: quoted = preserve case, unquoted = lowercased.
#[derive(Debug)]
struct RegTypeIdent {
    value: String,
    quoted: bool,
}

/// Collapse all runs of whitespace (spaces, tabs, newlines) to a single
/// ASCII space. Matches PostgreSQL's whitespace normalization for type
/// names like `interval  day   to   second` or `interval\tday`.
fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse a single identifier: quoted preserves case, unquoted lowercases.
fn parse_regtype_ident(raw: &str) -> RegTypeIdent {
    let trimmed = raw.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        RegTypeIdent {
            value: trimmed[1..trimmed.len() - 1].to_string(),
            quoted: true,
        }
    } else {
        RegTypeIdent {
            value: trimmed.to_lowercase(),
            quoted: false,
        }
    }
}

/// Parse a regtype input into (schema, name).
///
/// Splits at the last unquoted `.` for schema.name separation.
/// Each component follows PostgreSQL identifier rules:
///   - Quoted (double-quoted): strip outer quotes, preserve case
///   - Unquoted: lowercase
fn parse_regtype_input(raw: &str) -> (Option<RegTypeIdent>, RegTypeIdent) {
    let trimmed = raw.trim();

    // Find the last '.' outside double quotes to split schema.name.
    let mut in_quotes = false;
    let mut last_dot = None;
    for (i, c) in trimmed.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            '.' if !in_quotes => last_dot = Some(i),
            _ => {}
        }
    }

    let (schema_raw, name_raw) = match last_dot {
        Some(pos) => (Some(&trimmed[..pos]), &trimmed[pos + 1..]),
        None => (None, trimmed),
    };

    let schema = schema_raw.map(parse_regtype_ident);
    let name = parse_regtype_ident(name_raw);

    (schema, name)
}

pub(crate) fn strip_regtype_array_dims(raw: &str) -> (String, bool) {
    let mut name = raw.trim().to_string();
    let mut is_array = false;
    loop {
        let trimmed = name.trim_end();
        if let Some(stripped) = trimmed.strip_suffix("[]") {
            is_array = true;
            name = stripped.trim_end().to_string();
            continue;
        }
        break;
    }
    (name, is_array)
}

pub(crate) fn strip_regtype_typmod(raw: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed.ends_with(')') {
        return trimmed.to_string();
    }

    let mut depth = 0_i32;
    for (idx, ch) in trimmed.char_indices().rev() {
        match ch {
            ')' => depth += 1,
            '(' => {
                depth -= 1;
                if depth == 0 {
                    return trimmed[..idx].trim_end().to_string();
                }
            }
            _ => {}
        }
    }
    trimmed.to_string()
}

fn invalid_interval_type_name(original_name: &str) -> anyhow::Error {
    crate::sql::error::SqlError::SqlStructure(format!("invalid type name \"{}\"", original_name))
        .into()
}

/// Validate interval precision inside `(N)`.
///
/// `paren_str` must start with `(`.
/// PostgreSQL grammar uses `Iconst` here, so only unsigned decimal digits
/// fitting signed 32-bit range are accepted by the raw parser.
fn validate_interval_precision(paren_str: &str, original_name: &str) -> Result<String> {
    let close = paren_str.find(')').ok_or_else(|| {
        crate::sql::error::SqlError::SqlStructure(format!(
            "invalid type name \"{}\"",
            original_name
        ))
    })?;
    let inner = paren_str[1..close].trim();
    let after = paren_str[close + 1..].trim();
    if !after.is_empty() {
        return Err(invalid_interval_type_name(original_name));
    }
    // Match PostgreSQL Iconst syntax: only bare decimal digits.
    if inner.is_empty() || !inner.chars().all(|c| c.is_ascii_digit()) {
        return Err(invalid_interval_type_name(original_name));
    }
    // PG grammar: typmod is Iconst (non-negative integer). Negative values
    // fail the parser. Out-of-range values (>6) are clamped with a WARNING
    // but still return OID 1186. Values beyond i32 range are parser errors.
    inner
        .parse::<i32>()
        .map(|_| "interval".to_string())
        .map_err(|_| invalid_interval_type_name(original_name))
}

/// Normalize interval qualifier forms to bare "interval".
///
/// PostgreSQL accepts `interval`, `interval(3)`, `interval day to second`,
/// `interval hour`, `interval second(3)`, etc.  All resolve to OID 1186.
///
/// Invalid qualifiers (e.g. `interval garbage`) or precision on qualifiers
/// that don't accept it (e.g. `interval minute(3)`) are syntax errors —
/// PostgreSQL's raw parser rejects them before `to_regtype` can catch the
/// error, so the error propagates to the client.
///
/// Returns `Ok("interval")` for valid interval forms,
/// `Ok(other)` for non-interval types (pass through),
/// `Err` for invalid type names.
///
/// This function must be called on the ORIGINAL input (before
/// `strip_regtype_typmod`), because it validates precision placement.
pub(crate) fn normalize_interval_type(name: &str) -> Result<String> {
    // Normalize whitespace: collapse all runs (spaces, tabs, newlines) to
    // single space. PostgreSQL accepts `interval\tday`, `interval  (3)`, etc.
    let lower = normalize_whitespace(&name.trim().to_lowercase());
    if lower == "interval" {
        return Ok(lower);
    }
    if !lower.starts_with("interval") {
        return Ok(name.trim().to_string());
    }
    let rest = &lower["interval".len()..];
    if rest.starts_with('(') {
        // "interval(N)" — validate precision content
        return validate_interval_precision(rest, name.trim());
    }
    if !rest.starts_with(' ') {
        // e.g. "intervals" — not an interval type, return as-is
        return Ok(name.trim().to_string());
    }
    let rest = rest.trim();

    // "interval (3)" — bare precision with whitespace before `(`.
    if rest.starts_with('(') {
        return validate_interval_precision(rest, name.trim());
    }

    // Qualifiers that accept trailing precision `(N)` — only those ending
    // in SECOND, per the PostgreSQL grammar.
    const QUALIFIERS_WITH_PRECISION: &[&str] = &[
        "day to second",
        "hour to second",
        "minute to second",
        "second",
    ];

    const QUALIFIERS_NO_PRECISION: &[&str] = &[
        "year to month",
        "day to minute",
        "day to hour",
        "hour to minute",
        "year",
        "month",
        "day",
        "hour",
        "minute",
    ];

    // Check qualifiers that accept precision first (longer matches first).
    for &q in QUALIFIERS_WITH_PRECISION {
        if rest == q {
            return Ok("interval".to_string());
        }
        if let Some(suffix) = rest.strip_prefix(q) {
            let trimmed = suffix.trim_start();
            if trimmed.starts_with('(') {
                return validate_interval_precision(trimmed, name.trim());
            }
        }
    }

    // Check qualifiers that do NOT accept precision.
    for &q in QUALIFIERS_NO_PRECISION {
        if rest == q {
            return Ok("interval".to_string());
        }
        if let Some(suffix) = rest.strip_prefix(q) {
            if suffix.trim_start().starts_with('(') {
                // Precision on a qualifier that doesn't accept it.
                return Err(invalid_interval_type_name(name.trim()));
            }
        }
    }

    // Unknown qualifier.
    Err(invalid_interval_type_name(name.trim()))
}

fn regtype_array_oid(base_oid: i64) -> Option<i64> {
    match base_oid {
        pg_types::OID_BOOL => Some(pg_types::OID_BOOL_ARRAY),
        pg_types::OID_BYTEA => Some(pg_types::OID_BYTEA_ARRAY),
        pg_types::OID_NAME => Some(pg_types::OID_NAME_ARRAY),
        pg_types::OID_INT2 => Some(pg_types::OID_INT2_ARRAY),
        pg_types::OID_INT4 => Some(pg_types::OID_INT4_ARRAY),
        pg_types::OID_TEXT => Some(pg_types::OID_TEXT_ARRAY),
        pg_types::OID_BPCHAR => Some(pg_types::OID_BPCHAR_ARRAY),
        pg_types::OID_VARCHAR => Some(pg_types::OID_VARCHAR_ARRAY),
        pg_types::OID_INT8 => Some(pg_types::OID_INT8_ARRAY),
        pg_types::OID_FLOAT4 => Some(pg_types::OID_FLOAT4_ARRAY),
        pg_types::OID_FLOAT8 => Some(pg_types::OID_FLOAT8_ARRAY),
        pg_types::OID_OID => Some(pg_types::OID_OID_ARRAY),
        pg_types::OID_TIMESTAMP => Some(pg_types::OID_TIMESTAMP_ARRAY),
        pg_types::OID_DATE => Some(pg_types::OID_DATE_ARRAY),
        pg_types::OID_TIME => Some(pg_types::OID_TIME_ARRAY),
        pg_types::OID_TIMESTAMPTZ => Some(pg_types::OID_TIMESTAMPTZ_ARRAY),
        pg_types::OID_INTERVAL => Some(pg_types::OID_INTERVAL_ARRAY),
        pg_types::OID_NUMERIC => Some(pg_types::OID_NUMERIC_ARRAY),
        pg_types::OID_JSON => Some(pg_types::OID_JSON_ARRAY),
        pg_types::OID_UUID => Some(pg_types::OID_UUID_ARRAY),
        pg_types::OID_JSONB => Some(pg_types::OID_JSONB_ARRAY),
        pg_types::OID_HSTORE => Some(pg_types::OID_HSTORE_ARRAY),
        _ => None,
    }
}

fn pg_catalog_regtype_oid(name: &str) -> Option<i64> {
    match name {
        "bool" | "boolean" => Some(pg_types::OID_BOOL),
        "bytea" => Some(pg_types::OID_BYTEA),
        "name" => Some(pg_types::OID_NAME),
        "int2" | "smallint" => Some(pg_types::OID_INT2),
        "int4" | "integer" | "int" => Some(pg_types::OID_INT4),
        "int8" | "bigint" => Some(pg_types::OID_INT8),
        "text" => Some(pg_types::OID_TEXT),
        "oid" => Some(pg_types::OID_OID),
        "json" => Some(pg_types::OID_JSON),
        "float4" | "real" => Some(pg_types::OID_FLOAT4),
        "float8" | "double precision" => Some(pg_types::OID_FLOAT8),
        "bpchar" | "character" => Some(pg_types::OID_BPCHAR),
        "varchar" | "character varying" => Some(pg_types::OID_VARCHAR),
        "date" => Some(pg_types::OID_DATE),
        "time" | "time without time zone" => Some(pg_types::OID_TIME),
        "timestamp" | "timestamp without time zone" => Some(pg_types::OID_TIMESTAMP),
        "timestamptz" | "timestamp with time zone" => Some(pg_types::OID_TIMESTAMPTZ),
        "interval" => Some(pg_types::OID_INTERVAL),
        "numeric" | "decimal" => Some(pg_types::OID_NUMERIC),
        "uuid" => Some(pg_types::OID_UUID),
        "tsvector" => Some(pg_types::OID_TSVECTOR),
        "tsquery" => Some(pg_types::OID_TSQUERY),
        "jsonb" => Some(pg_types::OID_JSONB),
        "vector" => Some(pg_types::OID_VECTOR),
        _ => None,
    }
}

pub fn to_regtype(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let raw = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) | None => return Ok(Value::Null),
        _ => return Err(anyhow::anyhow!("function to_regtype(text) does not exist")),
    };

    // Step 1: Strip array suffix `[]`.
    let (without_array, is_array) = strip_regtype_array_dims(&raw);

    // Step 2: Parse into structured schema.name with quoting semantics.
    // Split BEFORE interval processing so that schema-qualified interval
    // types like `pg_catalog.interval day to second` are handled correctly.
    let (schema, name) = parse_regtype_input(&without_array);
    if name.value.is_empty() {
        return Ok(Value::Null);
    }

    // Step 3: Early-exit for unknown schemas.
    // If schema is present and doesn't match `pg_catalog`, the type cannot
    // resolve — return NULL.  However, PG's raw parser still catches syntax
    // errors regardless of schema: `noschema.interval(abc)` → NULL (valid
    // parse, schema not found), but `noschema.interval garbage` → ERROR
    // (bare word after type name is a parse error).
    //
    // For schema-qualified types PG uses the general `typename(typmod)`
    // grammar, so a trailing `(...)` is a valid typmod expression.  A bare
    // word after the identifier (without parens) is a syntax error.
    if let Some(ref s) = schema {
        if s.value != "pg_catalog" {
            if !name.quoted {
                let trimmed = name.value.trim();
                // Check for a bare-word suffix after the type identifier.
                if let Some(ws_pos) = trimmed.find(char::is_whitespace) {
                    let after = trimmed[ws_pos..].trim_start();
                    if !after.is_empty() && !after.starts_with('(') {
                        // Bare word after type name → syntax error.
                        // For interval types, normalize_interval_type validates
                        // qualifiers and returns Err for invalid ones.
                        // For non-interval types, any bare word is always a
                        // syntax error — PG's general `typename(typmod)` grammar
                        // only allows parenthesized typmods after the type name.
                        let ws_normalized = normalize_whitespace(trimmed);
                        let result = normalize_interval_type(&ws_normalized)?;
                        if result != "interval" {
                            return Err(invalid_interval_type_name(&raw));
                        }
                    }
                }
            }
            return Ok(Value::Null);
        }
    }

    // Step 4: Resolve the type name.
    // Quoted names: literal value (no interval processing, no typmod stripping).
    // Unquoted names: normalize whitespace, validate interval forms, strip typmod.
    let resolved_name = if name.quoted {
        name.value.clone()
    } else {
        let ws_normalized = normalize_whitespace(&name.value);
        let after_interval = normalize_interval_type(&ws_normalized)?;
        if after_interval == "interval" {
            after_interval
        } else {
            // Bare-word trailing junk check for non-interval unqualified types.
            // e.g. `to_regtype('int4 garbage')` → ERROR, matching PG behavior.
            if let Some(ws_pos) = after_interval.find(char::is_whitespace) {
                let after = after_interval[ws_pos..].trim_start();
                if !after.is_empty() && !after.starts_with('(') {
                    return Err(invalid_interval_type_name(&raw));
                }
            }
            strip_regtype_typmod(&after_interval)
        }
    };

    // Step 5: Look up OID with `_typename` alias folding.
    let base_oid = pg_catalog_regtype_oid(&resolved_name).or_else(|| {
        if name.quoted {
            return None;
        }
        resolved_name
            .strip_prefix('_')
            .and_then(pg_catalog_regtype_oid)
            .and_then(regtype_array_oid)
    });

    let oid = if is_array {
        base_oid.and_then(regtype_array_oid)
    } else {
        base_oid
    };

    Ok(oid.map(Value::Int64).unwrap_or(Value::Null))
}

pub fn pg_is_in_recovery(_args: Vec<Value>) -> Result<Value> {
    Ok(Value::Boolean(false))
}

pub fn pg_table_is_visible(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Null) | None => Ok(Value::Null),
        Some(_) => Ok(Value::Boolean(true)),
    }
}

/// Check if a type is visible in the current search_path.
///
/// In db9-server, all types within the keyspace are visible, so this always
/// returns true for non-NULL inputs (similar to pg_table_is_visible).
///
/// PostgreSQL signature: pg_type_is_visible(type_oid oid) → boolean
pub fn pg_type_is_visible(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Null) | None => Ok(Value::Null),
        Some(_) => Ok(Value::Boolean(true)),
    }
}

pub fn clock_timestamp(_args: Vec<Value>) -> Result<Value> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    Ok(Value::Timestamp(ts))
}

pub fn txid_current(_args: Vec<Value>) -> Result<Value> {
    use std::time::{SystemTime, UNIX_EPOCH};
    Ok(Value::Int64(
        std::process::id() as i64 * 1000000
            + SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_micros() as i64
                % 1000000,
    ))
}

pub fn pg_encoding_to_char(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    match iter.next() {
        Some(Value::Null) | None => Ok(Value::Null),
        Some(_) => Ok(Value::Text("UTF8".to_string())),
    }
}

pub fn obj_description(_args: Vec<Value>) -> Result<Value> {
    Ok(Value::Null)
}

pub fn pg_get_serial_sequence(_args: Vec<Value>) -> Result<Value> {
    fn parse_qname_token(token: &str) -> Option<(Option<String>, String)> {
        fn push_part(parts: &mut Vec<String>, raw: &str, quoted: bool) -> Option<()> {
            let trimmed = if quoted { raw } else { raw.trim() };
            if trimmed.is_empty() {
                return None;
            }
            if quoted {
                parts.push(trimmed.to_string());
            } else {
                parts.push(trimmed.to_lowercase());
            }
            Some(())
        }

        let mut parts: Vec<String> = Vec::new();
        let mut buf = String::new();
        let mut in_quotes = false;
        let mut part_quoted = false;

        let mut chars = token.trim().chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '"' => {
                    if in_quotes {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                            buf.push('"');
                        } else {
                            in_quotes = false;
                        }
                    } else {
                        in_quotes = true;
                        part_quoted = true;
                    }
                }
                '.' if !in_quotes => {
                    push_part(&mut parts, &buf, part_quoted)?;
                    buf.clear();
                    part_quoted = false;
                }
                _ => buf.push(ch),
            }
        }

        if in_quotes {
            return None;
        }
        push_part(&mut parts, &buf, part_quoted)?;

        if parts.len() >= 2 {
            Some((
                Some(parts[parts.len() - 2].clone()),
                parts[parts.len() - 1].clone(),
            ))
        } else {
            Some((None, parts[0].clone()))
        }
    }

    let mut iter = _args.into_iter();
    let table_name = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) | None => return Ok(Value::Null),
        Some(v) => v.to_string(),
    };
    let col_name = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) | None => return Ok(Value::Null),
        Some(v) => v.to_string(),
    };

    let (schema_opt, table) = match parse_qname_token(&table_name) {
        Some(parsed) => parsed,
        None => return Ok(Value::Null),
    };
    let (_, column) = match parse_qname_token(&col_name) {
        Some(parsed) => parsed,
        None => return Ok(Value::Null),
    };

    let schema = schema_opt.unwrap_or_else(|| "public".to_string());
    let seq_name = format!("{}_{}_seq", table, column);
    Ok(Value::Text(format!(
        "{}.{}",
        quoting::quote_ident(&schema),
        quoting::quote_ident(&seq_name)
    )))
}

pub fn pg_get_expr(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Text(s)) => Ok(Value::Text(s)),
        Some(Value::Null) | None => Ok(Value::Null),
        Some(v) => Ok(Value::Text(v.to_string())),
    }
}

/// Compatibility stub used by psql introspection (`\d`).
/// db9 currently has no extended stats definitions, so returning empty text is sufficient.
pub fn pg_get_statisticsobjdef_columns(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Null) | None => Ok(Value::Null),
        _ => Ok(Value::Text(String::new())),
    }
}

pub fn has_privilege(_args: Vec<Value>) -> Result<Value> {
    Ok(Value::Boolean(true))
}

pub fn pg_relation_is_publishable(args: Vec<Value>) -> Result<Value> {
    match args.into_iter().next() {
        Some(Value::Null) | None => Ok(Value::Null),
        // db9 has no logical replication publication support yet.
        Some(_) => Ok(Value::Boolean(false)),
    }
}

/// Compatibility shim for `pg_partition_ancestors(regclass)`.
///
/// db9 currently has no partition ancestry metadata, so introspection should
/// behave like "no ancestors".
pub fn pg_partition_ancestors(_args: Vec<Value>) -> Result<Value> {
    Ok(Value::Null)
}

/// int4send(integer) → bytea — 4-byte big-endian encoding
pub fn int4send(args: Vec<Value>) -> Result<Value> {
    let val = args.into_iter().next().unwrap_or(Value::Null);
    match val {
        Value::Null => Ok(Value::Null),
        Value::Int32(n) => Ok(Value::Bytes(n.to_be_bytes().to_vec())),
        Value::Int64(n) => Ok(Value::Bytes((n as i32).to_be_bytes().to_vec())),
        other => {
            let n: i32 = other
                .to_string()
                .parse()
                .map_err(|_| anyhow::anyhow!("function int4send(integer) does not exist"))?;
            Ok(Value::Bytes(n.to_be_bytes().to_vec()))
        }
    }
}

/// int8send(bigint) → bytea — 8-byte big-endian encoding
pub fn int8send(args: Vec<Value>) -> Result<Value> {
    let val = args.into_iter().next().unwrap_or(Value::Null);
    match val {
        Value::Null => Ok(Value::Null),
        Value::Int64(n) => Ok(Value::Bytes(n.to_be_bytes().to_vec())),
        Value::Int32(n) => Ok(Value::Bytes((n as i64).to_be_bytes().to_vec())),
        other => {
            let n: i64 = other
                .to_string()
                .parse()
                .map_err(|_| anyhow::anyhow!("function int8send(bigint) does not exist"))?;
            Ok(Value::Bytes(n.to_be_bytes().to_vec()))
        }
    }
}

/// uuid_send(uuid) → bytea — 16-byte binary representation
pub fn uuid_send(args: Vec<Value>) -> Result<Value> {
    let val = args.into_iter().next().unwrap_or(Value::Null);
    match val {
        Value::Null => Ok(Value::Null),
        Value::Uuid(bytes) => Ok(Value::Bytes(bytes.to_vec())),
        Value::Text(s) => {
            let u: uuid::Uuid = s.parse().map_err(|_| {
                anyhow::anyhow!(
                    "{}",
                    crate::sql::error::SqlError::InvalidInputSyntax {
                        type_name: "uuid".into(),
                        value: s,
                    }
                )
            })?;
            Ok(Value::Bytes(u.as_bytes().to_vec()))
        }
        _ => anyhow::bail!("function uuid_send(uuid) does not exist"),
    }
}

/// set_bit(bytea, n, newvalue) → bytea
///
/// PostgreSQL bytea bit indexing: bit `n` maps to byte `n / 8`, and within
/// that byte the bit position is `n % 8` (LSB-first: bit 0 = rightmost).
pub fn set_bit_bytea(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let bytes = match iter.next() {
        Some(Value::Bytes(b)) => b,
        Some(Value::Null) | None => return Ok(Value::Null),
        _ => anyhow::bail!("function set_bit(bytea, integer, integer) does not exist"),
    };
    let bit_n = match iter.next() {
        Some(Value::Int32(n)) => n as i64,
        Some(Value::Int64(n)) => n,
        _ => anyhow::bail!("function set_bit(bytea, integer, integer) does not exist"),
    };
    let new_val = match iter.next() {
        Some(Value::Int32(n)) => n,
        Some(Value::Int64(n)) => n as i32,
        _ => anyhow::bail!("function set_bit(bytea, integer, integer) does not exist"),
    };

    if new_val != 0 && new_val != 1 {
        anyhow::bail!("new bit must be 0 or 1");
    }

    let total_bits = bytes.len() as i64 * 8;
    if bit_n < 0 || bit_n >= total_bits {
        anyhow::bail!("index {} out of valid range, 0..{}", bit_n, total_bits - 1);
    }

    let mut result = bytes;
    let byte_idx = (bit_n / 8) as usize;
    let bit_idx = (bit_n % 8) as u32;
    if new_val == 1 {
        result[byte_idx] |= 1 << bit_idx;
    } else {
        result[byte_idx] &= !(1 << bit_idx);
    }
    Ok(Value::Bytes(result))
}

/// get_bit(bytea, n) → integer
///
/// PostgreSQL bytea bit indexing: bit `n` maps to byte `n / 8`, bit position
/// `n % 8` within that byte (LSB-first: bit 0 = rightmost).
pub fn get_bit_bytea(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let bytes = match iter.next() {
        Some(Value::Bytes(b)) => b,
        Some(Value::Null) | None => return Ok(Value::Null),
        _ => anyhow::bail!("function get_bit(bytea, integer) does not exist"),
    };
    let bit_n = match iter.next() {
        Some(Value::Int32(n)) => n as i64,
        Some(Value::Int64(n)) => n,
        _ => anyhow::bail!("function get_bit(bytea, integer) does not exist"),
    };

    let total_bits = bytes.len() as i64 * 8;
    if bit_n < 0 || bit_n >= total_bits {
        anyhow::bail!("index {} out of valid range, 0..{}", bit_n, total_bits - 1);
    }

    let byte_idx = (bit_n / 8) as usize;
    let bit_idx = (bit_n % 8) as u32;
    let bit_val = (bytes[byte_idx] >> bit_idx) & 1;
    Ok(Value::Int32(bit_val as i32))
}

pub fn hashtext(args: Vec<Value>) -> Result<Value> {
    let text = match args.into_iter().next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) | None => return Ok(Value::Null),
        Some(v) => v.to_string(),
    };
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    let h = hasher.finish();
    Ok(Value::Int32(h as i32))
}

#[cfg(test)]
mod tests {
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
        // Schema-qualified interval types
        assert_eq!(
            to_regtype(vec![Value::Text(
                "pg_catalog.interval day to second".into()
            )])
            .unwrap(),
            Value::Int64(pg_types::OID_INTERVAL)
        );
        // Unknown schema + invalid interval typmod → NULL (schema resolution
        // before interval validation: schema not found = NULL, no error).
        assert_eq!(
            to_regtype(vec![Value::Text("noschema.interval(abc)".into())]).unwrap(),
            Value::Null
        );
        // Bare word after type name → syntax error (PG's parser rejects it).
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
}
