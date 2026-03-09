use crate::model::{DataType, Value};
use crate::sql::pg_types;
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

pub(crate) fn pg_typeof_name_for_datatype(dt: &DataType) -> String {
    match dt {
        DataType::Boolean => "boolean".to_string(),
        DataType::Int32 => "integer".to_string(),
        DataType::Int64 => "bigint".to_string(),
        DataType::Float64 => "double precision".to_string(),
        DataType::Numeric { .. } => "numeric".to_string(),
        DataType::Text => "text".to_string(),
        DataType::Bytes => "bytea".to_string(),
        DataType::Timestamp => "timestamp without time zone".to_string(),
        DataType::TimestampTz => "timestamp with time zone".to_string(),
        DataType::Date => "date".to_string(),
        DataType::Time => "time without time zone".to_string(),
        DataType::Interval => "interval".to_string(),
        DataType::Uuid => "uuid".to_string(),
        DataType::Json => "json".to_string(),
        DataType::Jsonb => "jsonb".to_string(),
        DataType::Array(inner) => format!("{}[]", pg_typeof_name_for_datatype(inner)),
        DataType::Vector(_) => "vector".to_string(),
        DataType::Tsvector => "tsvector".to_string(),
        DataType::Tsquery => "tsquery".to_string(),
        DataType::Name => "name".to_string(),
        DataType::Varchar(_) => "character varying".to_string(),
        DataType::UserDefined(name) if name.eq_ignore_ascii_case("regclass") => {
            "regclass".to_string()
        }
        DataType::UserDefined(name) if name.eq_ignore_ascii_case("pg_catalog.regclass") => {
            "regclass".to_string()
        }
        DataType::UserDefined(name) if name.eq_ignore_ascii_case("regtype") => {
            "regtype".to_string()
        }
        DataType::UserDefined(name) if name.eq_ignore_ascii_case("pg_catalog.regtype") => {
            "regtype".to_string()
        }
        DataType::UserDefined(name) => name
            .strip_prefix("pg_catalog.")
            .unwrap_or(name.as_str())
            .to_string(),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParsedRegtypeLookupKind {
    SearchPath,
    SpecialBuiltin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedRegtypeLookup {
    pub(crate) schema: Option<String>,
    pub(crate) name: String,
    pub(crate) is_array: bool,
    pub(crate) quoted: bool,
    pub(crate) typmod: Option<String>,
    pub(crate) display_name: String,
    pub(crate) kind: ParsedRegtypeLookupKind,
}

/// Collapse all runs of whitespace (spaces, tabs, newlines) to a single
/// ASCII space. Matches PostgreSQL's whitespace normalization for type
/// names like `interval  day   to   second` or `interval\tday`.
fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn regtype_display_name(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(schema) => format!("{schema}.{name}"),
        None => name.to_string(),
    }
}

fn invalid_type_name(original_name: &str) -> anyhow::Error {
    crate::sql::error::SqlError::SqlStructure(format!("invalid type name \"{}\"", original_name))
        .into()
}

fn unterminated_quoted_identifier(original_name: &str) -> anyhow::Error {
    crate::sql::error::SqlError::SqlStructure(format!(
        "unterminated quoted identifier at or near \"{}\"",
        original_name
    ))
    .into()
}

fn zero_length_delimited_identifier(original_name: &str) -> anyhow::Error {
    crate::sql::error::SqlError::SqlStructure(format!(
        "zero-length delimited identifier at or near \"{}\"",
        original_name
    ))
    .into()
}

fn cross_database_reference(original_name: &str) -> anyhow::Error {
    crate::sql::error::SqlError::Unsupported(format!(
        "cross-database references are not implemented: {}",
        original_name
    ))
    .into()
}

fn type_modifier_not_allowed(display_name: &str) -> anyhow::Error {
    crate::sql::error::SqlError::SqlStructure(format!(
        "type modifier is not allowed for type \"{}\"",
        display_name
    ))
    .into()
}

fn invalid_interval_type_modifier() -> anyhow::Error {
    crate::sql::error::SqlError::SqlStructure("invalid INTERVAL type modifier".to_string()).into()
}

/// Parse a single identifier: quoted preserves case, unquoted lowercases.
fn parse_regtype_ident(raw: &str, original_name: &str) -> Result<RegTypeIdent> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(RegTypeIdent {
            value: String::new(),
            quoted: false,
        });
    }

    if trimmed.starts_with('"') {
        if trimmed.len() < 2 || !trimmed.ends_with('"') {
            return Err(unterminated_quoted_identifier(original_name));
        }

        let mut value = String::new();
        let mut chars = trimmed[1..trimmed.len() - 1].chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    value.push('"');
                } else {
                    return Err(invalid_type_name(original_name));
                }
            } else {
                value.push(ch);
            }
        }

        if value.is_empty() {
            return Err(zero_length_delimited_identifier(original_name));
        }

        return Ok(RegTypeIdent {
            value,
            quoted: true,
        });
    }

    if trimmed.contains('"') {
        return Err(invalid_type_name(original_name));
    }

    Ok(RegTypeIdent {
        value: trimmed.to_lowercase(),
        quoted: false,
    })
}

fn split_regtype_input_parts(raw: &str) -> Result<Vec<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let mut parts = Vec::new();
    let mut start = 0usize;
    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    let mut in_quotes = false;

    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                if in_quotes && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                    i += 2;
                } else {
                    in_quotes = !in_quotes;
                    i += 1;
                }
            }
            b'.' if !in_quotes => {
                parts.push(trimmed[start..i].to_string());
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }

    if in_quotes {
        return Err(unterminated_quoted_identifier(trimmed));
    }

    parts.push(trimmed[start..].to_string());
    Ok(parts)
}

fn contains_unquoted_paren(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let mut i = 0usize;
    let mut in_quotes = false;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                if in_quotes && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                    i += 2;
                } else {
                    in_quotes = !in_quotes;
                    i += 1;
                }
            }
            b'(' | b')' if !in_quotes => return true,
            _ => i += 1,
        }
    }
    false
}

fn split_regtype_typmod_parts(raw: &str) -> Result<(String, Option<String>)> {
    let trimmed = raw.trim();
    if !trimmed.ends_with(')') {
        return Ok((trimmed.to_string(), None));
    }

    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    let mut in_quotes = false;
    let mut depth = 0i32;
    let mut typmod_start = None;

    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                if in_quotes && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                    i += 2;
                } else {
                    in_quotes = !in_quotes;
                    i += 1;
                }
            }
            b'(' if !in_quotes => {
                if depth == 0 {
                    typmod_start = Some(i);
                }
                depth += 1;
                i += 1;
            }
            b')' if !in_quotes => {
                if depth == 0 {
                    return Err(invalid_type_name(trimmed));
                }
                depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }

    if in_quotes || depth != 0 {
        return Err(invalid_type_name(trimmed));
    }

    let Some(typmod_start) = typmod_start else {
        return Ok((trimmed.to_string(), None));
    };
    let base = trimmed[..typmod_start].trim_end().to_string();
    let inner = trimmed[typmod_start + 1..trimmed.len() - 1]
        .trim()
        .to_string();
    Ok((base, Some(inner)))
}

fn split_temporal_typmod<'a>(
    raw: &'a str,
    original_name: &str,
) -> Result<(Option<String>, &'a str)> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with('(') {
        return Ok((None, trimmed));
    }

    let close = trimmed
        .find(')')
        .ok_or_else(|| invalid_type_name(original_name))?;
    let inner = trimmed[1..close].trim().to_string();
    if inner.is_empty() {
        return Err(invalid_type_name(original_name));
    }

    Ok((Some(inner), trimmed[close + 1..].trim_start()))
}

fn parse_nonnegative_typmod_int_list(typmod: &str, original_name: &str) -> Result<Vec<i32>> {
    let mut out = Vec::new();
    for part in typmod.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_digit()) {
            return Err(invalid_type_name(original_name));
        }
        out.push(
            trimmed
                .parse::<i32>()
                .map_err(|_| invalid_type_name(original_name))?,
        );
    }
    if out.is_empty() {
        return Err(invalid_type_name(original_name));
    }
    Ok(out)
}

fn parse_temporal_special_lookup(
    raw: &str,
    keyword: &str,
) -> Result<Option<(String, Option<String>, String)>> {
    let normalized = normalize_whitespace(raw);
    let lower = normalized.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix(keyword) else {
        return Ok(None);
    };

    if !rest.is_empty() {
        let first = rest.chars().next().unwrap_or_default();
        if !first.is_whitespace() && first != '(' {
            return Ok(None);
        }
    }

    let (typmod, rest) = split_temporal_typmod(&normalized[keyword.len()..], raw)?;
    let rest = normalize_whitespace(rest).to_ascii_lowercase();

    let canonical = match (keyword, rest.as_str()) {
        ("time", "") | ("time", "without time zone") => "time",
        ("time", "with time zone") => "timetz",
        ("timestamp", "") | ("timestamp", "without time zone") => "timestamp",
        ("timestamp", "with time zone") => "timestamptz",
        _ => return Err(invalid_type_name(raw)),
    };

    if let Some(tm) = typmod.as_deref() {
        let args = parse_nonnegative_typmod_int_list(tm, raw)?;
        if args.len() != 1 {
            return Err(invalid_type_name(raw));
        }
    }

    Ok(Some((
        canonical.to_string(),
        typmod,
        normalize_whitespace(raw).to_ascii_lowercase(),
    )))
}

fn parse_special_builtin_lookup(raw: &str, is_array: bool) -> Result<Option<ParsedRegtypeLookup>> {
    let normalized = normalize_whitespace(raw);
    let normalized_lower = normalized.to_ascii_lowercase();

    if let Some((name, typmod, display_name)) = parse_temporal_special_lookup(raw, "time")? {
        return Ok(Some(ParsedRegtypeLookup {
            schema: None,
            name,
            is_array,
            quoted: false,
            typmod,
            display_name,
            kind: ParsedRegtypeLookupKind::SpecialBuiltin,
        }));
    }

    if let Some((name, typmod, display_name)) = parse_temporal_special_lookup(raw, "timestamp")? {
        return Ok(Some(ParsedRegtypeLookup {
            schema: None,
            name,
            is_array,
            quoted: false,
            typmod,
            display_name,
            kind: ParsedRegtypeLookupKind::SpecialBuiltin,
        }));
    }

    if normalized_lower.starts_with("interval") && normalize_interval_type(raw)? == "interval" {
        return Ok(Some(ParsedRegtypeLookup {
            schema: None,
            name: "interval".to_string(),
            is_array,
            quoted: false,
            typmod: None,
            display_name: "interval".to_string(),
            kind: ParsedRegtypeLookupKind::SpecialBuiltin,
        }));
    }

    let (base, typmod) = split_regtype_typmod_parts(&normalized_lower)?;
    if typmod.as_deref().is_some_and(str::is_empty) {
        return Err(invalid_type_name(raw));
    }

    if base.contains(char::is_whitespace)
        && !matches!(
            base.as_str(),
            "double precision" | "character varying" | "bit varying"
        )
    {
        return Err(invalid_type_name(raw));
    }

    let name = match base.as_str() {
        "boolean" => Some("bool"),
        "smallint" => Some("int2"),
        "integer" | "int" => Some("int4"),
        "bigint" => Some("int8"),
        "real" => Some("float4"),
        "double precision" => {
            if typmod.is_some() {
                return Err(invalid_type_name(raw));
            }
            Some("float8")
        }
        "character varying" | "varchar" => Some("varchar"),
        "character" => Some("bpchar"),
        "numeric" | "decimal" => Some("numeric"),
        "bit varying" => Some("bit varying"),
        _ => None,
    };

    match base.as_str() {
        "boolean" | "smallint" | "integer" | "int" | "bigint" | "real" | "double precision" => {
            if typmod.is_some() {
                return Err(invalid_type_name(raw));
            }
        }
        "character varying" | "varchar" | "character" | "bit varying" => {
            if let Some(tm) = typmod.as_deref() {
                let args = parse_nonnegative_typmod_int_list(tm, raw)?;
                if args.len() != 1 {
                    return Err(invalid_type_name(raw));
                }
            }
        }
        _ => {}
    }

    Ok(name.map(|name| ParsedRegtypeLookup {
        schema: None,
        name: name.to_string(),
        is_array,
        quoted: false,
        typmod,
        display_name: base,
        kind: ParsedRegtypeLookupKind::SpecialBuiltin,
    }))
}

pub(crate) fn resolve_builtin_regtype_lookup(lookup: &ParsedRegtypeLookup) -> Option<i64> {
    let base_oid = match lookup.kind {
        ParsedRegtypeLookupKind::SpecialBuiltin => pg_types::pg_catalog_regtype_oid(&lookup.name),
        ParsedRegtypeLookupKind::SearchPath => {
            pg_types::actual_pg_catalog_regtype_oid(&lookup.name)
        }
    }?;

    if lookup.is_array {
        pg_types::regtype_array_oid(base_oid)
    } else {
        Some(base_oid)
    }
}

pub(crate) fn validate_resolved_regtype_typmod(
    lookup: &ParsedRegtypeLookup,
    resolved_oid: Option<i64>,
    is_user_defined: bool,
) -> Result<()> {
    let Some(typmod) = lookup.typmod.as_deref() else {
        return Ok(());
    };
    let Some(oid) = resolved_oid else {
        return Ok(());
    };

    if is_user_defined {
        return Err(type_modifier_not_allowed(&lookup.display_name));
    }

    match oid {
        pg_types::OID_VARCHAR => {
            let args = parse_resolved_typmod_int_list(typmod, &lookup.display_name)?;
            if args.len() != 1 {
                return Err(invalid_type_name(&lookup.display_name));
            }
            validate_typmod_bounds("varchar", &args)
        }
        pg_types::OID_BPCHAR => {
            let args = parse_resolved_typmod_int_list(typmod, &lookup.display_name)?;
            if args.len() != 1 {
                return Err(invalid_type_name(&lookup.display_name));
            }
            validate_typmod_bounds("bpchar", &args)
        }
        pg_types::OID_NUMERIC => {
            let args = parse_resolved_typmod_int_list(typmod, &lookup.display_name)?;
            if !(1..=2).contains(&args.len()) {
                return Err(invalid_type_name(&lookup.display_name));
            }
            validate_typmod_bounds("numeric", &args)
        }
        pg_types::OID_TIME
        | pg_types::OID_TIMETZ
        | pg_types::OID_TIMESTAMP
        | pg_types::OID_TIMESTAMPTZ => {
            let args = parse_resolved_typmod_int_list(typmod, &lookup.display_name)?;
            if args.len() != 1 {
                return Err(invalid_type_name(&lookup.display_name));
            }
            if args[0] < 0 {
                let type_display = match oid {
                    pg_types::OID_TIMESTAMPTZ => {
                        format!("TIMESTAMP({}) WITH TIME ZONE", args[0])
                    }
                    pg_types::OID_TIMETZ => format!("TIME({}) WITH TIME ZONE", args[0]),
                    pg_types::OID_TIMESTAMP => format!("TIMESTAMP({})", args[0]),
                    _ => format!("TIME({})", args[0]),
                };
                return Err(crate::sql::error::SqlError::SqlStructure(format!(
                    "{} precision must not be negative",
                    type_display
                ))
                .into());
            }
            Ok(())
        }
        pg_types::OID_INTERVAL => Err(invalid_interval_type_modifier()),
        _ => Err(type_modifier_not_allowed(&lookup.display_name)),
    }
}

pub(crate) fn parse_regtype_lookup(raw: &str) -> Result<Option<ParsedRegtypeLookup>> {
    let trimmed = raw.trim();
    let (without_array, is_array) = strip_regtype_array_dims(trimmed);
    let parts = split_regtype_input_parts(&without_array)?;

    match parts.as_slice() {
        [] => Ok(None),
        [single] => {
            if single.trim().is_empty() {
                return Ok(None);
            }

            if !single.trim_start().starts_with('"') {
                if let Some(special) = parse_special_builtin_lookup(single, is_array)? {
                    return Ok(Some(special));
                }
            }

            let (base, typmod) = split_regtype_typmod_parts(single)?;
            if typmod.as_deref().is_some_and(str::is_empty) {
                return Err(invalid_type_name(trimmed));
            }
            if contains_unquoted_paren(&base) {
                return Err(invalid_type_name(trimmed));
            }

            let ident = parse_regtype_ident(&base, trimmed)?;
            if ident.value.is_empty() {
                return Ok(None);
            }
            if !ident.quoted && base.contains(char::is_whitespace) {
                return Err(invalid_type_name(trimmed));
            }

            Ok(Some(ParsedRegtypeLookup {
                schema: None,
                name: ident.value.clone(),
                is_array,
                quoted: ident.quoted,
                typmod,
                display_name: regtype_display_name(None, &ident.value),
                kind: ParsedRegtypeLookupKind::SearchPath,
            }))
        }
        [schema_raw, name_raw] => {
            let schema = parse_regtype_ident(schema_raw, trimmed)?;
            let (base, typmod) = split_regtype_typmod_parts(name_raw)?;
            if typmod.as_deref().is_some_and(str::is_empty) {
                return Err(invalid_type_name(trimmed));
            }
            if contains_unquoted_paren(&base) {
                return Err(invalid_type_name(trimmed));
            }

            let name = parse_regtype_ident(&base, trimmed)?;
            if schema.value.is_empty() || name.value.is_empty() {
                return Ok(None);
            }
            if !name.quoted && base.contains(char::is_whitespace) {
                return Err(invalid_type_name(trimmed));
            }

            Ok(Some(ParsedRegtypeLookup {
                schema: Some(schema.value.clone()),
                name: name.value.clone(),
                is_array,
                quoted: name.quoted,
                typmod,
                display_name: regtype_display_name(Some(&schema.value), &name.value),
                kind: ParsedRegtypeLookupKind::SearchPath,
            }))
        }
        _ => Err(cross_database_reference(trimmed)),
    }
}

pub fn to_regtype(args: Vec<Value>) -> Result<Value> {
    let mut iter = args.into_iter();
    let raw = match iter.next() {
        Some(Value::Text(s)) => s,
        Some(Value::Null) | None => return Ok(Value::Null),
        _ => return Err(anyhow::anyhow!("function to_regtype(text) does not exist")),
    };

    let Some(lookup) = parse_regtype_lookup(&raw)? else {
        return Ok(Value::Null);
    };

    let oid = match lookup.kind {
        ParsedRegtypeLookupKind::SpecialBuiltin => resolve_builtin_regtype_lookup(&lookup),
        ParsedRegtypeLookupKind::SearchPath => match lookup.schema.as_deref() {
            Some("pg_catalog") => resolve_builtin_regtype_lookup(&lookup),
            Some(_) => None,
            None => resolve_builtin_regtype_lookup(&lookup),
        },
    };

    validate_resolved_regtype_typmod(&lookup, oid, false)?;
    Ok(oid.map(Value::Int64).unwrap_or(Value::Null))
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

fn parse_resolved_typmod_int_list(typmod: &str, original_name: &str) -> Result<Vec<i32>> {
    let mut out = Vec::new();
    for part in typmod.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            return Err(invalid_type_name(original_name));
        }
        out.push(trimmed.parse::<i32>().map_err(|_| {
            crate::sql::error::SqlError::InvalidInputSyntax {
                type_name: "integer".to_string(),
                value: trimmed.to_string(),
            }
        })?);
    }
    if out.is_empty() {
        return Err(invalid_type_name(original_name));
    }
    Ok(out)
}

/// Validate semantic bounds for typmod values of known PostgreSQL types.
///
/// PostgreSQL enforces these ranges at type-name parse time (including
/// inside `to_regtype`):
///   - varchar / character varying: length >= 1
///   - bpchar / character: length >= 1
///   - numeric / decimal: precision 1–1000 (scale is unchecked — PG accepts
///     scale > precision and negative scale in `to_regtype`)
fn validate_typmod_bounds(base: &str, args: &[i32]) -> Result<()> {
    match base {
        "varchar" | "character varying" => {
            if args.len() == 1 && args[0] < 1 {
                return Err(crate::sql::error::SqlError::SqlStructure(
                    "length for type varchar must be at least 1".to_string(),
                )
                .into());
            }
        }
        "bpchar" | "character" => {
            if args.len() == 1 && args[0] < 1 {
                return Err(crate::sql::error::SqlError::SqlStructure(
                    "length for type char must be at least 1".to_string(),
                )
                .into());
            }
        }
        "numeric" | "decimal" => {
            if !args.is_empty() {
                let precision = args[0];
                if !(1..=1000).contains(&precision) {
                    return Err(crate::sql::error::SqlError::SqlStructure(format!(
                        "NUMERIC precision {} must be between 1 and 1000",
                        precision
                    ))
                    .into());
                }
            }
        }
        _ => {}
    }
    Ok(())
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
    Err(invalid_type_name(name.trim()))
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
}
