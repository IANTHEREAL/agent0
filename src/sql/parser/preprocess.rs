//! SQL preprocessing shims for sqlparser-rs compatibility.
//!
//! Each shim normalizes a PostgreSQL syntax form that sqlparser-rs cannot
//! currently parse into an equivalent form that it can. Every shim is
//! documented with a classification and exit condition so it can be removed
//! when the upstream parser adds support.

use regex::Regex;

use super::operator_rewrite::{
    rewrite_all_any_subquery_parse_compat, rewrite_at_time_zone_placeholders,
    rewrite_jsonb_exists_ops, rewrite_reset_role, rewrite_table_shorthand,
    rewrite_user_role_aliases, rewrite_vector_distance_ops,
};
use super::tokenizer::{tokenize_sql_for_rewrite, Token, TokenKind};

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

/// Rewrite `SELECT FROM` (empty projection) to `SELECT TRUE AS _exists FROM`.
///
/// Classification: parse-compat shim.
/// Context: All Activepieces occurrences are inside `EXISTS(SELECT FROM ...)` where
/// the injected column is unobservable. sqlparser 0.40 requires ≥1 SELECT item.
/// Exit condition: remove when sqlparser-rs supports empty SELECT lists.
fn preprocess_select_from(sql: &str) -> Option<String> {
    let re = Regex::new(r"(?i)\bSELECT\s+FROM\b").ok()?;
    if !re.is_match(sql) {
        return None;
    }
    Some(
        re.replace_all(sql, "SELECT TRUE AS _exists FROM")
            .into_owned(),
    )
}

/// Rewrite `<type> array` (PostgreSQL array type syntax) to `<type>[]`.
///
/// Classification: parse-normalization shim.
/// PostgreSQL accepts both `character varying array` and `character varying[]`
/// identically. sqlparser 0.40 only accepts the bracket form.
/// Exit condition: remove when sqlparser-rs supports `TYPE ARRAY` syntax.
fn preprocess_type_array(sql: &str) -> Option<String> {
    let re = Regex::new(
        r"(?i)\b(character\s+varying|varchar|integer|bigint|smallint|text|boolean|numeric|uuid|jsonb?|timestamp(?:\s+with(?:out)?\s+time\s+zone)?|double\s+precision|real)\s+array\b"
    ).ok()?;
    if !re.is_match(sql) {
        return None;
    }
    Some(re.replace_all(sql, "${1}[]").into_owned())
}

/// Strip `NOT VALID` constraint modifier from ALTER TABLE ADD CONSTRAINT.
///
/// Classification: parse-compat shim.
/// Semantic effect: constraint is validated immediately (stricter than PG which
/// defers validation). Safe for Activepieces where data invariants hold at
/// migration time. Known limitation for general dirty-data migrations.
/// Exit condition: remove when sqlparser-rs supports `NOT VALID`.
fn preprocess_not_valid(sql: &str) -> Option<String> {
    let re = Regex::new(r"(?i)\bNOT\s+VALID\b").ok()?;
    if !re.is_match(sql) {
        return None;
    }
    Some(re.replace_all(sql, "").into_owned())
}

/// Strip `CONCURRENTLY` from `DROP INDEX CONCURRENTLY`.
///
/// Classification: parse-compat shim.
/// In db9, all index drops are transactional via TiKV — `CONCURRENTLY` is
/// semantically a no-op.
/// Exit condition: remove when sqlparser-rs supports DROP INDEX CONCURRENTLY.
fn preprocess_drop_index_concurrently(sql: &str) -> Option<String> {
    let re = Regex::new(r"(?i)\bDROP\s+INDEX\s+CONCURRENTLY\b").ok()?;
    if !re.is_match(sql) {
        return None;
    }
    Some(re.replace_all(sql, "DROP INDEX").into_owned())
}

/// Rewrite `UPDATE ... FROM <table> JOIN ..., <table2> WHERE ...` comma-separated
/// FROM tables to `CROSS JOIN`.
///
/// Classification: parse-compat shim.
/// sqlparser 0.40's `parse_update` calls `parse_table_and_joins` once for FROM,
/// which parses up to the first comma then fails. PostgreSQL comma-separated FROM
/// elements are semantically equivalent to CROSS JOIN.
/// Exit condition: remove when sqlparser-rs supports multi-table UPDATE FROM.
fn preprocess_update_from_comma(sql: &str) -> Option<String> {
    let re_update = Regex::new(r"(?i)\bUPDATE\b").ok()?;
    let re_from = Regex::new(r"(?i)\bFROM\b").ok()?;
    if !re_update.is_match(sql) || !re_from.is_match(sql) {
        return None;
    }

    // Find SET keyword in UPDATE statement, then FROM at depth 0 after SET list.
    let update_pos = find_keyword_at_depth0(sql, "UPDATE")?;
    let update_end = update_pos + "UPDATE".len();
    let after_update = &sql[update_end..];
    let set_rel = find_keyword_at_depth0(after_update, "SET")?;
    let set_pos = update_end + set_rel;
    let set_end = set_pos + "SET".len();

    let from_search = &sql[set_end..];
    let from_rel = find_keyword_at_depth0(from_search, "FROM")?;
    let from_pos = set_end + from_rel + "FROM".len();

    // Find WHERE keyword at depth 0 after FROM
    let after_from = &sql[from_pos..];
    let where_pos = find_keyword_at_depth0(after_from, "WHERE")?;

    let from_clause = &sql[from_pos..from_pos + where_pos];

    // Check if there are any commas at depth 0
    if !has_comma_at_depth0(from_clause) {
        return None;
    }

    // Replace commas at depth 0 with CROSS JOIN
    let rewritten = replace_commas_at_depth0(from_clause);
    let mut result = String::with_capacity(sql.len() + 20);
    result.push_str(&sql[..from_pos]);
    result.push_str(&rewritten);
    result.push_str(&sql[from_pos + where_pos..]);
    Some(result)
}

/// Find the byte offset of a keyword at parenthesis depth 0.
fn find_keyword_at_depth0(s: &str, keyword: &str) -> Option<usize> {
    let mut depth: i32 = 0;
    let upper = s.to_uppercase();
    let kw_len = keyword.len();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
            }
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'\'' {
                            i += 1; // escaped quote
                        } else {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            }
            _ => {
                if depth == 0
                    && i + kw_len <= upper.len()
                    && upper[i..i + kw_len].eq_ignore_ascii_case(keyword)
                    && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_')
                    && (i + kw_len >= bytes.len()
                        || !bytes[i + kw_len].is_ascii_alphanumeric() && bytes[i + kw_len] != b'_')
                {
                    return Some(i);
                }
                i += 1;
            }
        }
    }
    None
}

/// Check if there is a comma at parenthesis depth 0 in the string.
fn has_comma_at_depth0(s: &str) -> bool {
    let mut depth: i32 = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
            }
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'\'' {
                            i += 1;
                        } else {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            }
            b',' if depth == 0 => return true,
            _ => {
                i += 1;
            }
        }
    }
    false
}

/// Replace commas at parenthesis depth 0 with ` CROSS JOIN `.
fn replace_commas_at_depth0(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 20);
    let mut depth: i32 = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                result.push('(');
                i += 1;
            }
            b')' => {
                depth -= 1;
                result.push(')');
                i += 1;
            }
            b'\'' => {
                result.push('\'');
                i += 1;
                while i < bytes.len() {
                    result.push(bytes[i] as char);
                    if bytes[i] == b'\'' {
                        i += 1;
                        if i < bytes.len() && bytes[i] == b'\'' {
                            result.push('\'');
                            i += 1;
                        } else {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'"' => {
                result.push('"');
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    result.push(bytes[i] as char);
                    i += 1;
                }
                if i < bytes.len() {
                    result.push('"');
                    i += 1;
                }
            }
            b',' if depth == 0 => {
                result.push_str(" CROSS JOIN ");
                i += 1;
            }
            _ => {
                result.push(bytes[i] as char);
                i += 1;
            }
        }
    }
    result
}

fn is_semicolon(tok: &Token) -> bool {
    tok.kind == TokenKind::Punct && tok.text == ";"
}

fn is_ignorable_statement_token(tok: &Token) -> bool {
    matches!(tok.kind, TokenKind::Whitespace | TokenKind::Comment)
}

fn is_word_eq(tok: &Token, expected: &str) -> bool {
    tok.kind == TokenKind::Word && tok.text.eq_ignore_ascii_case(expected)
}

fn statement_window(tokens: &[Token], offset: usize) -> Option<&[Token]> {
    let mut match_idx = None;
    for (idx, tok) in tokens.iter().enumerate() {
        if offset < tok.end {
            match_idx = Some(idx);
            break;
        }
    }
    let match_idx = match_idx?;

    let mut start = match_idx;
    while start > 0 && !is_semicolon(&tokens[start - 1]) {
        start -= 1;
    }

    let mut end = match_idx;
    while end < tokens.len() && !is_semicolon(&tokens[end]) {
        end += 1;
    }

    Some(&tokens[start..end])
}

fn is_create_index_statement(tokens: &[Token]) -> bool {
    let mut i = 0usize;
    while i < tokens.len() && is_ignorable_statement_token(&tokens[i]) {
        i += 1;
    }
    if i >= tokens.len() || !is_word_eq(&tokens[i], "CREATE") {
        return false;
    }
    i += 1;

    while i < tokens.len() && is_ignorable_statement_token(&tokens[i]) {
        i += 1;
    }
    if i < tokens.len() && is_word_eq(&tokens[i], "UNIQUE") {
        i += 1;
        while i < tokens.len() && is_ignorable_statement_token(&tokens[i]) {
            i += 1;
        }
    }

    i < tokens.len() && is_word_eq(&tokens[i], "INDEX")
}

fn mask_hnsw_rewrite_unsafe_regions(sql: &str, tokens: &[Token]) -> String {
    let mut masked = sql.as_bytes().to_vec();
    for tok in tokens {
        if matches!(
            tok.kind,
            TokenKind::StringLiteral | TokenKind::DollarString | TokenKind::Comment
        ) {
            for byte in &mut masked[tok.start..tok.end] {
                if !matches!(*byte, b'\n' | b'\r') {
                    *byte = b' ';
                }
            }
        }
    }
    String::from_utf8(masked).unwrap_or_else(|_| sql.to_string())
}

fn next_non_ignorable_token(tokens: &[Token], mut idx: usize, end: usize) -> Option<usize> {
    while idx < end {
        if !is_ignorable_statement_token(&tokens[idx]) {
            return Some(idx);
        }
        idx += 1;
    }
    None
}

fn find_matching_rparen(tokens: &[Token], open_idx: usize, end: usize) -> Option<usize> {
    if open_idx >= end || tokens[open_idx].kind != TokenKind::Punct || tokens[open_idx].text != "("
    {
        return None;
    }

    let mut depth = 0i32;
    for (idx, tok) in tokens
        .iter()
        .enumerate()
        .skip(open_idx)
        .take(end - open_idx)
    {
        if tok.kind != TokenKind::Punct {
            continue;
        }
        match tok.text.as_str() {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
                if depth < 0 {
                    return None;
                }
            }
            _ => {}
        }
    }
    None
}

#[derive(Debug, Default, Clone)]
struct CreateIndexWithAnalysis {
    rewrites: Vec<(usize, usize)>,
    // One entry per CREATE INDEX statement in source order.
    with_params_by_create_index: Vec<Option<String>>,
}

fn analyze_create_index_with_params(sql: &str) -> CreateIndexWithAnalysis {
    let tokens = tokenize_sql_for_rewrite(sql);
    if tokens.is_empty() {
        return CreateIndexWithAnalysis::default();
    }

    let mut analysis = CreateIndexWithAnalysis::default();
    let mut stmt_start = 0usize;

    while stmt_start < tokens.len() {
        let mut stmt_end = stmt_start;
        while stmt_end < tokens.len() && !is_semicolon(&tokens[stmt_end]) {
            stmt_end += 1;
        }

        let stmt_tokens = &tokens[stmt_start..stmt_end];
        if is_create_index_statement(stmt_tokens) {
            let mut depth = 0i32;
            let mut i = stmt_start;
            let mut stmt_with_params: Option<String> = None;

            while i < stmt_end {
                let tok = &tokens[i];

                if tok.kind == TokenKind::Punct {
                    match tok.text.as_str() {
                        "(" => depth += 1,
                        ")" => {
                            if depth > 0 {
                                depth -= 1;
                            }
                        }
                        _ => {}
                    }
                }

                if depth == 0 && is_word_eq(tok, "WITH") {
                    let Some(open_idx) = next_non_ignorable_token(&tokens, i + 1, stmt_end) else {
                        i += 1;
                        continue;
                    };
                    if tokens[open_idx].kind == TokenKind::Punct && tokens[open_idx].text == "(" {
                        let Some(close_idx) = find_matching_rparen(&tokens, open_idx, stmt_end)
                        else {
                            i += 1;
                            continue;
                        };
                        if stmt_with_params.is_none() {
                            let params = sql[tokens[open_idx].end..tokens[close_idx].start]
                                .trim()
                                .to_string();
                            stmt_with_params = Some(params);
                        }
                        analysis.rewrites.push((tok.start, tokens[close_idx].end));
                        i = close_idx + 1;
                        continue;
                    }
                }

                i += 1;
            }

            analysis.with_params_by_create_index.push(stmt_with_params);
        }

        stmt_start = if stmt_end < tokens.len() {
            stmt_end + 1
        } else {
            stmt_end
        };
    }

    analysis
}

pub(super) fn extract_create_index_with_params(sql: &str) -> Vec<Option<String>> {
    analyze_create_index_with_params(sql).with_params_by_create_index
}

/// Strip `WITH (...)` storage parameters from CREATE INDEX.
///
/// sqlparser 0.40 does not parse CREATE INDEX WITH parameters yet.
/// This parse-compat shim removes the WITH clause so statements can be parsed.
/// Extracted WITH payload is preserved by `extract_create_index_with_params()`
/// for executor-time validation/semantics.
///
/// Classification: parse-compat shim.
/// Exit condition: remove when sqlparser-rs supports CREATE INDEX WITH (...).
fn preprocess_create_index_with_params(sql: &str) -> Option<String> {
    let analysis = analyze_create_index_with_params(sql);
    if analysis.rewrites.is_empty() {
        return None;
    }

    let mut rewritten = sql.to_string();
    for (start, end) in analysis.rewrites.into_iter().rev() {
        rewritten.replace_range(start..end, "");
    }
    Some(rewritten)
}

/// Strip pgvector operator class from HNSW CREATE INDEX column list.
///
/// PostgreSQL+pgvector uses `CREATE INDEX ... USING hnsw (col vector_l2_ops)`.
/// sqlparser 0.40 cannot parse operator classes in index column lists.
/// This shim strips the opclass token and encodes the distance metric in the
/// method name: `hnsw` (L2 default, no opclass), `hnsw__l2` (explicit
/// `vector_l2_ops`), `hnsw__cosine`, `hnsw__ip`.
/// `execute_create_index` reads the suffix and normalizes back to `"hnsw"`
/// before storing in `IndexDef`.
///
/// Classification: parse-normalization shim.
/// Exit condition: remove when sqlparser-rs supports operator classes.
fn preprocess_hnsw_opclass(sql: &str) -> Option<String> {
    let re =
        Regex::new(r"(?i)(USING\s+hnsw)\s*\(\s*([^\s)]+)\s+(vector_(?:l2|cosine|ip)_ops)\s*\)")
            .ok()?;
    let tokens = tokenize_sql_for_rewrite(sql);
    if tokens.is_empty() {
        return None;
    }

    // Never match inside string literals / dollar strings / comments.
    let masked_sql = mask_hnsw_rewrite_unsafe_regions(sql, &tokens);
    let mut rewrites: Vec<(usize, usize, String)> = Vec::new();

    for caps in re.captures_iter(&masked_sql) {
        let Some(full) = caps.get(0) else {
            continue;
        };
        let Some(stmt_tokens) = statement_window(&tokens, full.start()) else {
            continue;
        };
        if !is_create_index_statement(stmt_tokens) {
            continue;
        }

        let Some(col_match) = caps.get(2) else {
            continue;
        };
        let Some(opclass_match) = caps.get(3) else {
            continue;
        };
        let col = sql[col_match.start()..col_match.end()].trim();
        let opclass = sql[opclass_match.start()..opclass_match.end()].to_ascii_lowercase();
        let suffix = match opclass.as_str() {
            "vector_l2_ops" => "__l2",
            "vector_cosine_ops" => "__cosine",
            "vector_ip_ops" => "__ip",
            _ => "",
        };
        rewrites.push((
            full.start(),
            full.end(),
            format!("USING hnsw{} ({})", suffix, col),
        ));
    }

    if rewrites.is_empty() {
        return None;
    }

    let mut rewritten = sql.to_string();
    for (start, end, replacement) in rewrites.into_iter().rev() {
        rewritten.replace_range(start..end, &replacement);
    }
    Some(rewritten)
}

/// Rewrite `pg_partition_ancestors(... ) WITH ORDINALITY` table factors to an
/// empty `unnest(regclass[]) WITH ORDINALITY` relation.
///
/// PostgreSQL's `psql \d+` emits:
/// `pg_catalog.pg_partition_ancestors(t.tgrelid) WITH ORDINALITY AS a(relid, depth)`.
/// sqlparser 0.40 cannot parse `WITH ORDINALITY` on general table functions.
///
/// db9 currently has no partition ancestry metadata, so replacing this source
/// with an empty relation preserves observable behavior for non-partitioned
/// tables while allowing the statement to parse.
///
/// Classification: parse-compat shim.
/// Exit condition: remove when sqlparser-rs supports `WITH ORDINALITY` on
/// generic table functions and db9 supports real partition ancestry metadata.
fn preprocess_partition_ancestors_with_ordinality(sql: &str) -> Option<String> {
    if !sql.to_ascii_uppercase().contains("PG_PARTITION_ANCESTORS")
        || !sql.to_ascii_uppercase().contains("WITH ORDINALITY")
    {
        return None;
    }

    let with_alias_re = Regex::new(
        r"(?is)(?:pg_catalog\.)?pg_partition_ancestors\s*\([^)]*\)\s+WITH\s+ORDINALITY\s+AS\s+([A-Za-z_][A-Za-z0-9_]*\s*\([^)]*\))",
    )
    .ok()?;
    let bare_re =
        Regex::new(r"(?is)(?:pg_catalog\.)?pg_partition_ancestors\s*\([^)]*\)\s+WITH\s+ORDINALITY")
            .ok()?;
    let tokens = tokenize_sql_for_rewrite(sql);
    if tokens.is_empty() {
        return None;
    }

    // Never match inside string literals / dollar strings / comments.
    let masked_sql = mask_hnsw_rewrite_unsafe_regions(sql, &tokens);
    let mut rewrites: Vec<(usize, usize, String)> = Vec::new();

    for caps in with_alias_re.captures_iter(&masked_sql) {
        let Some(full) = caps.get(0) else {
            continue;
        };
        let Some(alias) = caps.get(1) else {
            continue;
        };
        let alias_text = sql[alias.start()..alias.end()].trim();
        rewrites.push((
            full.start(),
            full.end(),
            format!(
                "UNNEST(ARRAY[]::pg_catalog.regclass[]) AS {} WITH OFFSET",
                alias_text
            ),
        ));
    }

    for caps in bare_re.captures_iter(&masked_sql) {
        let Some(full) = caps.get(0) else {
            continue;
        };
        let covered = rewrites
            .iter()
            .any(|(s, e, _)| full.start() >= *s && full.end() <= *e);
        if covered {
            continue;
        }
        rewrites.push((
            full.start(),
            full.end(),
            "UNNEST(ARRAY[]::pg_catalog.regclass[]) WITH OFFSET".to_string(),
        ));
    }

    if rewrites.is_empty() {
        return None;
    }
    rewrites.sort_by_key(|(start, _, _)| *start);

    let mut rewritten = sql.to_string();
    for (start, end, replacement) in rewrites.into_iter().rev() {
        rewritten.replace_range(start..end, &replacement);
    }
    Some(rewritten)
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
/// - `rewrite_user_role_aliases`
///   What: `CREATE/ALTER/DROP USER` -> `... ROLE` (and `CREATE USER` implies `LOGIN`).
///   Why: sqlparser-rs lacks USER alias support for role DDL.
///   Exit condition: sqlparser-rs supports USER aliases.
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
/// - `preprocess_select_from`
///   What: `SELECT FROM` -> `SELECT TRUE AS _exists FROM`.
///   Why: sqlparser 0.40 requires ≥1 SELECT item.
///   Exit condition: empty SELECT list support.
/// - `preprocess_type_array`
///   What: `<type> array` -> `<type>[]`.
///   Why: sqlparser 0.40 only accepts bracket form.
///   Exit condition: `TYPE ARRAY` syntax support.
/// - `preprocess_not_valid`
///   What: strip `NOT VALID` from constraints.
///   Why: sqlparser 0.40 has no NOT VALID support.
///   Exit condition: NOT VALID support added.
/// - `preprocess_drop_index_concurrently`
///   What: strip `CONCURRENTLY` from DROP INDEX.
///   Why: sqlparser 0.40 doesn't handle CONCURRENTLY in DROP INDEX.
///   Exit condition: DROP INDEX CONCURRENTLY support.
/// - `preprocess_update_from_comma`
///   What: comma-separated UPDATE FROM tables -> CROSS JOIN.
///   Why: sqlparser 0.40 only parses single table_and_joins in UPDATE FROM.
///   Exit condition: multi-table UPDATE FROM support.
/// - `preprocess_create_index_with_params`
///   What: strip `WITH (...)` from CREATE INDEX statements.
///   Why: sqlparser 0.40 doesn't parse CREATE INDEX storage parameters.
///   Exit condition: CREATE INDEX WITH(...) support in sqlparser-rs.
/// - `preprocess_hnsw_opclass`
///   What: strip pgvector opclass from HNSW index column list, encode metric
///   in method name (`hnsw__l2`, `hnsw__cosine`, `hnsw__ip`; default L2
///   without explicit opclass stays as `hnsw`).
///   Why: sqlparser 0.40 doesn't support operator classes in CREATE INDEX.
///   Exit condition: operator class support in sqlparser-rs.
/// - `preprocess_partition_ancestors_with_ordinality`
///   What: rewrite `pg_partition_ancestors(... ) WITH ORDINALITY` to an empty
///   `unnest(regclass[]) WITH ORDINALITY` source.
///   Why: sqlparser 0.40 cannot parse `WITH ORDINALITY` on non-UNNEST table
///   functions.
///   Exit condition: parser support for generic `WITH ORDINALITY` plus native
///   partition ancestry support.
pub(super) fn preprocess_sql(sql: &str) -> Result<String, String> {
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

    // Activepieces migration compat shims (#885)
    if let Some(rewritten) = preprocess_select_from(&result) {
        result = rewritten;
    }
    if let Some(rewritten) = preprocess_type_array(&result) {
        result = rewritten;
    }
    // Custom typed-string literals (mood 'happy') are handled natively by
    // our sqlparser fork (crates/sqlparser), not by preprocessing.
    if let Some(rewritten) = preprocess_not_valid(&result) {
        result = rewritten;
    }
    if let Some(rewritten) = preprocess_drop_index_concurrently(&result) {
        result = rewritten;
    }
    if let Some(rewritten) = preprocess_update_from_comma(&result) {
        result = rewritten;
    }
    if let Some(rewritten) = preprocess_create_index_with_params(&result) {
        result = rewritten;
    }
    if let Some(rewritten) = preprocess_hnsw_opclass(&result) {
        result = rewritten;
    }
    if let Some(rewritten) = preprocess_partition_ancestors_with_ordinality(&result) {
        result = rewritten;
    }

    // TABLE shorthand rewrite (parse-normalization shim)
    result = rewrite_table_shorthand(&result)?;

    // Parse-compat rewrites only: these normalize syntax for sqlparser-rs
    // limitations and must not perform semantic query-shape rewrites.
    result = rewrite_all_any_subquery_parse_compat(&result);
    result = rewrite_jsonb_exists_ops(&result);

    result = rewrite_vector_distance_ops(&result);

    // `CREATE/ALTER/DROP USER` are aliases of `... ROLE` in PostgreSQL.
    result = rewrite_user_role_aliases(&result);

    // sqlparser-rs doesn't support PostgreSQL's `RESET ROLE`, but it does support the equivalent
    // `SET ROLE NONE`. Non-ROLE RESET is handled by raw_sql::Reset in the executor.
    result = rewrite_reset_role(&result);

    // sqlparser-rs expects a quoted string after `AT TIME ZONE`, but PostgreSQL also allows
    // prepared statement placeholders (`$n`). This rewrite is only used for parse-time validation
    // (see `parse_sql()`), and the original SQL is preserved for execution/binding.
    result = rewrite_at_time_zone_placeholders(&result);

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preprocess_select_from_rewrites_empty_projection() {
        let input =
            "SELECT exists (SELECT FROM information_schema.tables WHERE table_name = 'flow')";
        let result = preprocess_select_from(input).unwrap();
        assert!(result.contains("SELECT TRUE AS _exists FROM information_schema.tables"));
    }

    #[test]
    fn preprocess_select_from_ignores_normal_select() {
        assert!(preprocess_select_from("SELECT 1 FROM t").is_none());
        assert!(preprocess_select_from("SELECT * FROM t").is_none());
    }

    #[test]
    fn preprocess_type_array_rewrites_character_varying() {
        let input = r#"ALTER TABLE "flow_run" ADD "tags" character varying array"#;
        let result = preprocess_type_array(input).unwrap();
        assert!(result.contains("character varying[]"));
        assert!(!result.to_lowercase().ends_with("array"));
    }

    #[test]
    fn preprocess_type_array_rewrites_multiple_types() {
        let input = r#""events" text array NOT NULL, "ids" uuid array"#;
        let result = preprocess_type_array(input).unwrap();
        assert!(result.contains("text[]"));
        assert!(result.contains("uuid[]"));
    }

    #[test]
    fn preprocess_type_array_ignores_bracket_form() {
        assert!(preprocess_type_array(r#"ADD "tags" varchar[]"#).is_none());
    }

    // Typed-string literal tests (mood 'happy') are in src/sql/parser/tests.rs
    // because they now test through parse_sql() → sqlparser fork, not preprocessing.

    #[test]
    fn preprocess_not_valid_strips() {
        let input =
            "ADD CONSTRAINT fk FOREIGN KEY (x) REFERENCES t(id) ON DELETE CASCADE NOT VALID";
        let result = preprocess_not_valid(input).unwrap();
        assert!(!result.contains("NOT VALID"));
        assert!(result.contains("ON DELETE CASCADE"));
    }

    #[test]
    fn preprocess_not_valid_ignores_when_absent() {
        assert!(
            preprocess_not_valid("ADD CONSTRAINT fk FOREIGN KEY (x) REFERENCES t(id)").is_none()
        );
    }

    #[test]
    fn preprocess_drop_index_concurrently_strips() {
        let input = r#"DROP INDEX CONCURRENTLY "idx_run_logs_file_id""#;
        let result = preprocess_drop_index_concurrently(input).unwrap();
        assert_eq!(result, r#"DROP INDEX "idx_run_logs_file_id""#);
    }

    #[test]
    fn preprocess_drop_index_concurrently_with_if_exists() {
        let input = r#"DROP INDEX CONCURRENTLY IF EXISTS "idx_audit""#;
        let result = preprocess_drop_index_concurrently(input).unwrap();
        assert_eq!(result, r#"DROP INDEX IF EXISTS "idx_audit""#);
    }

    #[test]
    fn preprocess_update_from_comma_rewrites() {
        let input = r#"UPDATE "flow_version" fv SET "updatedBy" = NULL FROM "flow" f JOIN "project" p ON p."id" = f."projectId", "user" u WHERE fv."flowId" = f."id""#;
        let result = preprocess_update_from_comma(input).unwrap();
        assert!(result.contains("CROSS JOIN"));
        assert!(!result.contains(r#", "user""#));
    }

    #[test]
    fn preprocess_update_from_comma_rewrites_with_newline_before_set() {
        let input = "UPDATE \"flow_version\" fv\nSET \"updatedBy\" = NULL\nFROM \"flow\" f JOIN \"project\" p ON p.\"id\" = f.\"projectId\", \"user\" u\nWHERE fv.\"flowId\" = f.\"id\"";
        let result = preprocess_update_from_comma(input).unwrap();
        assert!(result.contains("CROSS JOIN"));
        assert!(!result.contains(", \"user\" u"));
    }

    #[test]
    fn preprocess_update_from_comma_ignores_no_comma() {
        let input = r#"UPDATE t SET x = 1 FROM s WHERE t.id = s.id"#;
        assert!(preprocess_update_from_comma(input).is_none());
    }

    #[test]
    fn preprocess_update_from_comma_ignores_nested_commas() {
        // Commas inside subquery parens should not be rewritten
        let input = r#"UPDATE t SET x = 1 FROM (SELECT a, b FROM s) sub WHERE t.id = sub.a"#;
        assert!(preprocess_update_from_comma(input).is_none());
    }

    #[test]
    fn preprocess_create_index_with_params_strips_with_clause() {
        let input =
            "CREATE INDEX idx_hnsw_custom ON t USING hnsw (v vector_l2_ops) WITH (m = 32, ef_construction = 128)";
        let rewritten = preprocess_create_index_with_params(input).expect("expected rewrite");
        assert!(!rewritten.to_ascii_uppercase().contains("WITH ("));
        assert!(rewritten.contains("USING hnsw (v vector_l2_ops)"));
    }

    #[test]
    fn preprocess_create_index_with_params_keeps_non_index_statements() {
        let input = "CREATE TABLE t (id int) WITH (fillfactor = 70)";
        assert!(preprocess_create_index_with_params(input).is_none());
    }

    #[test]
    fn preprocess_create_index_with_params_preserves_where_tail() {
        let input = "CREATE INDEX idx_a ON t USING btree (a) WITH (fillfactor = 70) WHERE a > 0";
        let rewritten = preprocess_create_index_with_params(input).expect("expected rewrite");
        assert!(!rewritten.to_ascii_uppercase().contains("WITH ("));
        assert!(rewritten.contains("WHERE a > 0"));
    }

    #[test]
    fn extract_create_index_with_params_ignores_non_create_index_with_clauses() {
        let input = "CREATE TABLE t (id int) WITH (fillfactor = 70); CREATE INDEX idx_a ON t USING btree (id) WITH (fillfactor = 80); CREATE INDEX idx_b ON t USING btree (id)";
        let extracted = extract_create_index_with_params(input);
        assert_eq!(extracted, vec![Some("fillfactor = 80".to_string()), None]);
    }

    #[test]
    fn extract_create_index_with_params_captures_hnsw_options() {
        let input = "CREATE INDEX idx_h ON t USING hnsw (v vector_l2_ops) WITH (m = 32, ef_construction = 128)";
        let extracted = extract_create_index_with_params(input);
        assert_eq!(
            extracted,
            vec![Some("m = 32, ef_construction = 128".to_string())]
        );
    }

    #[test]
    fn preprocess_hnsw_opclass_l2() {
        let input = "CREATE INDEX idx ON t USING hnsw (v vector_l2_ops)";
        let result = preprocess_hnsw_opclass(input).unwrap();
        assert_eq!(result, "CREATE INDEX idx ON t USING hnsw__l2 (v)");
    }

    #[test]
    fn preprocess_hnsw_opclass_cosine() {
        let input = "CREATE INDEX idx ON t USING hnsw (v vector_cosine_ops)";
        let result = preprocess_hnsw_opclass(input).unwrap();
        assert_eq!(result, "CREATE INDEX idx ON t USING hnsw__cosine (v)");
    }

    #[test]
    fn preprocess_hnsw_opclass_ip() {
        let input = "CREATE INDEX idx ON t USING hnsw (v vector_ip_ops)";
        let result = preprocess_hnsw_opclass(input).unwrap();
        assert_eq!(result, "CREATE INDEX idx ON t USING hnsw__ip (v)");
    }

    #[test]
    fn preprocess_hnsw_opclass_no_opclass() {
        let input = "CREATE INDEX idx ON t USING hnsw (v)";
        assert!(preprocess_hnsw_opclass(input).is_none());
    }

    #[test]
    fn preprocess_hnsw_opclass_case_insensitive() {
        let input = "CREATE INDEX idx ON t USING HNSW (v VECTOR_COSINE_OPS)";
        let result = preprocess_hnsw_opclass(input).unwrap();
        assert_eq!(result, "CREATE INDEX idx ON t USING hnsw__cosine (v)");
    }

    #[test]
    fn preprocess_hnsw_opclass_skips_string_literals() {
        // P0: must not rewrite inside string literals — only CREATE statements
        // are candidates, so SELECT/INSERT with quoted USING hnsw must be skipped.
        let input = "SELECT 'USING hnsw (v vector_cosine_ops)' AS s";
        assert!(preprocess_hnsw_opclass(input).is_none());

        let input2 =
            "INSERT INTO t (sql) VALUES ('CREATE INDEX idx ON t USING hnsw (v vector_cosine_ops)')";
        assert!(preprocess_hnsw_opclass(input2).is_none());
    }

    #[test]
    fn preprocess_hnsw_opclass_skips_create_table_literal_default() {
        let input = "CREATE TABLE __lit_create(s text DEFAULT 'USING hnsw (v vector_cosine_ops)')";
        assert!(preprocess_hnsw_opclass(input).is_none());
    }

    #[test]
    fn preprocess_hnsw_opclass_rewrites_only_create_index_statements() {
        let input = "CREATE TABLE t (s text DEFAULT 'USING hnsw (v vector_cosine_ops)'); CREATE INDEX idx ON t USING hnsw (v vector_cosine_ops)";
        let rewritten = preprocess_hnsw_opclass(input).expect("expected HNSW rewrite");
        assert!(rewritten.contains("DEFAULT 'USING hnsw (v vector_cosine_ops)'"));
        assert!(rewritten.contains("CREATE INDEX idx ON t USING hnsw__cosine (v)"));
    }

    #[test]
    fn preprocess_partition_ancestors_with_ordinality_rewrites_psql_pattern() {
        let input = "SELECT u.tgrelid::pg_catalog.regclass FROM pg_catalog.pg_trigger AS u, pg_catalog.pg_partition_ancestors(t.tgrelid) WITH ORDINALITY AS a(relid, depth) WHERE u.tgname = 't'";
        let rewritten = preprocess_partition_ancestors_with_ordinality(input)
            .expect("expected pg_partition_ancestors WITH ORDINALITY rewrite");
        assert!(rewritten
            .contains("UNNEST(ARRAY[]::pg_catalog.regclass[]) AS a(relid, depth) WITH OFFSET"));
        assert!(!rewritten.contains("pg_partition_ancestors(t.tgrelid) WITH ORDINALITY"));
    }

    #[test]
    fn preprocess_partition_ancestors_with_ordinality_ignores_scalar_usage() {
        let input = "SELECT pg_catalog.pg_partition_ancestors('42')";
        assert!(preprocess_partition_ancestors_with_ordinality(input).is_none());
    }
}
