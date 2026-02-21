//! SQL preprocessing shims for sqlparser-rs compatibility.
//!
//! Each shim normalizes a PostgreSQL syntax form that sqlparser-rs cannot
//! currently parse into an equivalent form that it can. Every shim is
//! documented with a classification and exit condition so it can be removed
//! when the upstream parser adds support.

use regex::Regex;

use super::operator_rewrite::{
    rewrite_all_any_subquery_parse_compat, rewrite_at_time_zone_placeholders,
    rewrite_jsonb_exists_ops, rewrite_reset_role, rewrite_vector_distance_ops,
};

/// Normalize PostgreSQL's `EXPLAIN (...)` option list into sqlparser-rs'
/// keyword-form `EXPLAIN ANALYZE VERBOSE ...`.
///
/// Classification: parse-normalization shim.
/// Exit condition: remove when sqlparser-rs supports parenthetical EXPLAIN options.
fn preprocess_explain(sql: &str) -> Option<String> {
    let trimmed = sql.trim();
    let upper = trimmed.to_uppercase();

    if !upper.starts_with("EXPLAIN ") {
        return None;
    }

    let after_explain = trimmed[7..].trim_start();
    if !after_explain.starts_with('(') {
        return None;
    }

    if let Some(close_paren) = after_explain.find(')') {
        let options = &after_explain[1..close_paren];
        let options_no_comma = options.replace(',', " ");
        let rest = &after_explain[close_paren + 1..];
        return Some(format!(
            "EXPLAIN {} {}",
            options_no_comma.trim(),
            rest.trim()
        ));
    }

    None
}

fn is_sequence_option_keyword(token_upper: &str) -> bool {
    matches!(
        token_upper,
        "INCREMENT" | "MINVALUE" | "MAXVALUE" | "START" | "CACHE" | "CYCLE" | "OWNED" | "NO"
    )
}

/// sqlparser-rs' `CREATE SEQUENCE` parser expects options in a fixed order:
/// `INCREMENT`, `MINVALUE`, `MAXVALUE`, `START`, `CACHE`, `[NO] CYCLE`, then optional `OWNED BY`.
///
/// PostgreSQL allows options in any order and `pg_dump` frequently emits
/// `START ... INCREMENT ... NO MINVALUE NO MAXVALUE ...`, which fails to parse because
/// sqlparser's `[ [ NO ] CYCLE ]` logic consumes a standalone `NO` even when it isn't
/// followed by `CYCLE`.
///
/// To keep compatibility without expanding sqlparser-rs, we normalize common
/// `CREATE SEQUENCE` option orderings into the expected order.
///
/// Classification: parse-normalization shim.
/// Exit condition: remove when sqlparser-rs supports PostgreSQL option ordering.
fn reorder_single_create_sequence(stmt: &str) -> Option<String> {
    let trimmed = stmt.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Cheap prefix check: only attempt to rewrite CREATE ... SEQUENCE statements.
    let upper = trimmed.to_uppercase();
    if !upper.starts_with("CREATE ") || !upper.contains(" SEQUENCE") {
        return None;
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.len() < 3 {
        return None;
    }
    if !tokens[0].eq_ignore_ascii_case("CREATE") {
        return None;
    }

    let seq_pos = tokens
        .iter()
        .position(|t| t.eq_ignore_ascii_case("SEQUENCE"))?;

    // Position right after sequence name (and optional IF NOT EXISTS / AS <type>).
    let mut name_pos = seq_pos + 1;
    if name_pos + 2 < tokens.len()
        && tokens[name_pos].eq_ignore_ascii_case("IF")
        && tokens[name_pos + 1].eq_ignore_ascii_case("NOT")
        && tokens[name_pos + 2].eq_ignore_ascii_case("EXISTS")
    {
        name_pos += 3;
    }
    if name_pos >= tokens.len() {
        return None;
    }

    let mut opt_pos = name_pos + 1;
    if opt_pos < tokens.len() && tokens[opt_pos].eq_ignore_ascii_case("AS") {
        opt_pos += 1;
        while opt_pos < tokens.len() {
            let t_upper = tokens[opt_pos].trim_end_matches(';').to_uppercase();
            if is_sequence_option_keyword(t_upper.as_str()) {
                break;
            }
            opt_pos += 1;
        }
    }

    let mut increment: Option<Vec<&str>> = None;
    let mut minvalue: Option<Vec<&str>> = None;
    let mut maxvalue: Option<Vec<&str>> = None;
    let mut start: Option<Vec<&str>> = None;
    let mut cache: Option<Vec<&str>> = None;
    let mut cycle: Option<Vec<&str>> = None;
    let mut owned_by: Option<Vec<&str>> = None;

    let mut i = opt_pos;
    while i < tokens.len() {
        let token = tokens[i];
        let token_upper = token.trim_end_matches(';').to_uppercase();
        match token_upper.as_str() {
            "INCREMENT" => {
                let mut seg = vec![token];
                i += 1;
                if i < tokens.len() && tokens[i].eq_ignore_ascii_case("BY") {
                    seg.push(tokens[i]);
                    i += 1;
                }
                if i >= tokens.len() {
                    return None;
                }
                seg.push(tokens[i]);
                i += 1;
                increment = Some(seg);
            }
            "MINVALUE" => {
                if i + 1 >= tokens.len() {
                    return None;
                }
                minvalue = Some(vec![tokens[i], tokens[i + 1]]);
                i += 2;
            }
            "MAXVALUE" => {
                if i + 1 >= tokens.len() {
                    return None;
                }
                maxvalue = Some(vec![tokens[i], tokens[i + 1]]);
                i += 2;
            }
            "START" => {
                let mut seg = vec![token];
                i += 1;
                if i < tokens.len() && tokens[i].eq_ignore_ascii_case("WITH") {
                    seg.push(tokens[i]);
                    i += 1;
                }
                if i >= tokens.len() {
                    return None;
                }
                seg.push(tokens[i]);
                i += 1;
                start = Some(seg);
            }
            "CACHE" => {
                if i + 1 >= tokens.len() {
                    return None;
                }
                cache = Some(vec![tokens[i], tokens[i + 1]]);
                i += 2;
            }
            "CYCLE" => {
                cycle = Some(vec![tokens[i]]);
                i += 1;
            }
            "NO" => {
                if i + 1 >= tokens.len() {
                    return None;
                }
                let next_upper = tokens[i + 1].trim_end_matches(';').to_uppercase();
                match next_upper.as_str() {
                    "MINVALUE" => {
                        minvalue = Some(vec![tokens[i], tokens[i + 1]]);
                        i += 2;
                    }
                    "MAXVALUE" => {
                        maxvalue = Some(vec![tokens[i], tokens[i + 1]]);
                        i += 2;
                    }
                    "CYCLE" => {
                        cycle = Some(vec![tokens[i], tokens[i + 1]]);
                        i += 2;
                    }
                    _ => return None,
                }
            }
            "OWNED" => {
                if i + 1 >= tokens.len() || !tokens[i + 1].eq_ignore_ascii_case("BY") {
                    return None;
                }
                owned_by = Some(tokens[i..].to_vec());
                break;
            }
            _ => return None,
        }
    }

    // If there are no options to normalize, leave the statement untouched.
    if increment.is_none()
        && minvalue.is_none()
        && maxvalue.is_none()
        && start.is_none()
        && cache.is_none()
        && cycle.is_none()
        && owned_by.is_none()
    {
        return None;
    }

    let mut out: Vec<&str> = Vec::with_capacity(tokens.len());
    out.extend_from_slice(&tokens[..opt_pos]);
    if let Some(seg) = increment.as_ref() {
        out.extend_from_slice(seg);
    }
    if let Some(seg) = minvalue.as_ref() {
        out.extend_from_slice(seg);
    }
    if let Some(seg) = maxvalue.as_ref() {
        out.extend_from_slice(seg);
    }
    if let Some(seg) = start.as_ref() {
        out.extend_from_slice(seg);
    }
    if let Some(seg) = cache.as_ref() {
        out.extend_from_slice(seg);
    }
    if let Some(seg) = cycle.as_ref() {
        out.extend_from_slice(seg);
    }
    if let Some(seg) = owned_by.as_ref() {
        out.extend_from_slice(seg);
    }

    let normalized = out.join(" ");
    let input_normalized = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized == input_normalized {
        None
    } else {
        Some(normalized)
    }
}

/// Apply CREATE SEQUENCE option-order normalization to all statements in SQL text.
///
/// Classification: parse-normalization shim.
/// Exit condition: remove when sqlparser-rs supports PostgreSQL option ordering.
pub(super) fn preprocess_create_sequence(sql: &str) -> Option<String> {
    if !sql.to_uppercase().contains("CREATE SEQUENCE") {
        return None;
    }

    let mut result_parts = Vec::new();
    let mut modified = false;

    for part in sql.split(';') {
        if let Some(reordered) = reorder_single_create_sequence(part) {
            result_parts.push(reordered);
            modified = true;
        } else {
            result_parts.push(part.to_string());
        }
    }

    if modified {
        Some(result_parts.join(";"))
    } else {
        None
    }
}

/// Remove PostgreSQL CTE materialization hints (`AS MATERIALIZED` /
/// `AS NOT MATERIALIZED`) that sqlparser-rs cannot parse.
///
/// Classification: parse-normalization shim.
/// Exit condition: remove when sqlparser-rs supports CTE materialization hints.
fn preprocess_cte_materialized(sql: &str) -> Option<String> {
    let re_not = Regex::new(r"(?i)\bAS\s+NOT\s+MATERIALIZED\s*\(").ok()?;
    let re_yes = Regex::new(r"(?i)\bAS\s+MATERIALIZED\s*\(").ok()?;
    if !re_not.is_match(sql) && !re_yes.is_match(sql) {
        return None;
    }
    let tmp = re_not.replace_all(sql, "AS (");
    let out = re_yes.replace_all(&tmp, "AS (");
    Some(out.into_owned())
}

/// Convert standalone `RESET ROLE` to its equivalent `SET ROLE NONE`.
///
/// Classification: parse-compatibility shim.
/// Exit condition: remove when sqlparser-rs supports `RESET ROLE`.
fn preprocess_reset_role(sql: &str) -> Option<String> {
    let trimmed = sql.trim();
    let trimmed = trimmed.trim_end_matches(';').trim();
    if trimmed.eq_ignore_ascii_case("RESET ROLE") {
        Some("SET ROLE NONE".to_string())
    } else {
        None
    }
}

/// SQL preprocessor shim registry (all parse-time compatibility only).
///
/// Shim inventory:
/// - `preprocess_explain`
///   What: `EXPLAIN (ANALYZE, VERBOSE)` -> keyword form.
///   Why: sqlparser-rs parse limitation.
///   Exit condition: parenthetical EXPLAIN options supported.
/// - `preprocess_create_sequence`
///   What: normalize `CREATE SEQUENCE` option order.
///   Why: sqlparser-rs expects fixed option order.
///   Exit condition: arbitrary PostgreSQL option order supported.
/// - `preprocess_cte_materialized`
///   What: strip `[NOT] MATERIALIZED` CTE hints.
///   Why: sqlparser-rs parse limitation.
///   Exit condition: CTE materialization hints supported.
/// - `preprocess_reset_role` / `rewrite_reset_role`
///   What: `RESET ROLE` -> `SET ROLE NONE`.
///   Why: sqlparser-rs lacks RESET ROLE support.
///   Exit condition: RESET ROLE statement support added.
/// - `rewrite_all_any_subquery_parse_compat`
///   What: `ANY/ALL(SELECT ...)` -> `ANY/ALL(ARRAY(SELECT ...))`.
///   Why: sqlparser-rs can't parse direct subquery form.
///   Exit condition: direct `ANY/ALL(SELECT ...)` parse support.
/// - `rewrite_jsonb_exists_ops`
///   What: `?`, `?|`, `?&` operators -> function calls.
///   Why: sqlparser-rs operator parse gap for these PostgreSQL operators.
///   Exit condition: parser supports these operators without conflicting with placeholders.
/// - `rewrite_vector_distance_ops`
///   What: `<->`, `<#>`, `<=>` -> function calls.
///   Why: sqlparser-rs does not parse extension custom operators.
///   Exit condition: parser supports custom operators.
/// - `rewrite_at_time_zone_placeholders`
///   What: `AT TIME ZONE $n` -> `AT TIME ZONE 'UTC'` (parse-time only).
///   Why: sqlparser-rs expects string literal in this position.
///   Exit condition: expression/placeholder support after `AT TIME ZONE`.
pub(super) fn preprocess_sql(sql: &str) -> String {
    let mut result = sql.to_string();

    if let Some(reset) = preprocess_reset_role(&result) {
        result = reset;
    }

    if let Some(explained) = preprocess_explain(&result) {
        result = explained;
    }
    if let Some(sequenced) = preprocess_create_sequence(&result) {
        result = sequenced;
    }
    if let Some(materialized) = preprocess_cte_materialized(&result) {
        result = materialized;
    }

    // Parse-compat rewrites only: these normalize syntax for sqlparser-rs
    // limitations and must not perform semantic query-shape rewrites.
    result = rewrite_all_any_subquery_parse_compat(&result);
    result = rewrite_jsonb_exists_ops(&result);

    result = rewrite_vector_distance_ops(&result);

    // sqlparser-rs doesn't support PostgreSQL's `RESET ROLE`, but it does support the equivalent
    // `SET ROLE NONE`. Non-ROLE RESET is handled by raw_sql::Reset in the executor.
    result = rewrite_reset_role(&result);

    // sqlparser-rs expects a quoted string after `AT TIME ZONE`, but PostgreSQL also allows
    // prepared statement placeholders (`$n`). This rewrite is only used for parse-time validation
    // (see `parse_sql()`), and the original SQL is preserved for execution/binding.
    result = rewrite_at_time_zone_placeholders(&result);

    result
}
