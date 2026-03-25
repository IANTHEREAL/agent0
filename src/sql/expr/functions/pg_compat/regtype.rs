use super::*;
use crate::sql::pg_types;

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
    if inner.is_empty() || !inner.chars().all(|c| c.is_ascii_digit()) {
        return Err(invalid_interval_type_name(original_name));
    }
    inner
        .parse::<i32>()
        .map(|_| "interval".to_string())
        .map_err(|_| invalid_interval_type_name(original_name))
}

pub(crate) fn normalize_interval_type(name: &str) -> Result<String> {
    let lower = normalize_whitespace(&name.trim().to_lowercase());
    if lower == "interval" {
        return Ok(lower);
    }
    if !lower.starts_with("interval") {
        return Ok(name.trim().to_string());
    }
    let rest = &lower["interval".len()..];
    if rest.starts_with('(') {
        return validate_interval_precision(rest, name.trim());
    }
    if !rest.starts_with(' ') {
        return Ok(name.trim().to_string());
    }
    let rest = rest.trim();

    if rest.starts_with('(') {
        return validate_interval_precision(rest, name.trim());
    }

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

    for &q in QUALIFIERS_NO_PRECISION {
        if rest == q {
            return Ok("interval".to_string());
        }
        if let Some(suffix) = rest.strip_prefix(q) {
            if suffix.trim_start().starts_with('(') {
                return Err(invalid_interval_type_name(name.trim()));
            }
        }
    }

    Err(invalid_type_name(name.trim()))
}
