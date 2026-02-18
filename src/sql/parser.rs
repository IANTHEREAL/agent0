//! SQL parser wrapper using sqlparser-rs

use anyhow::{anyhow, Result};
use regex::Regex;
use sqlparser::ast::Statement;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

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

fn preprocess_create_sequence(sql: &str) -> Option<String> {
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

fn preprocess_sql(sql: &str) -> String {
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

fn preprocess_reset_role(sql: &str) -> Option<String> {
    let trimmed = sql.trim();
    let trimmed = trimmed.trim_end_matches(';').trim();
    if trimmed.eq_ignore_ascii_case("RESET ROLE") {
        Some("SET ROLE NONE".to_string())
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    Word,
    Whitespace,
    StringLiteral,
    QuotedIdent,
    DollarString,
    Comment,
    Punct,
    Operator,
    Other,
}

#[derive(Debug, Clone)]
struct Token {
    kind: TokenKind,
    start: usize,
    end: usize,
    text: String,
}

fn tokenize_sql_for_rewrite(sql: &str) -> Vec<Token> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let start = i;

        // Line comment: -- ...
        if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Comment,
                start,
                end: i,
                text: sql[start..i].to_string(),
            });
            continue;
        }

        // Block comment: /* ... */
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < bytes.len() {
                i += 2;
            }
            tokens.push(Token {
                kind: TokenKind::Comment,
                start,
                end: i,
                text: sql[start..i].to_string(),
            });
            continue;
        }

        // Whitespace
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Whitespace,
                start,
                end: i,
                text: sql[start..i].to_string(),
            });
            continue;
        }

        // Single-quoted string literal
        if bytes[i] == b'\'' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            tokens.push(Token {
                kind: TokenKind::StringLiteral,
                start,
                end: i,
                text: sql[start..i].to_string(),
            });
            continue;
        }

        // Double-quoted identifier
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            tokens.push(Token {
                kind: TokenKind::QuotedIdent,
                start,
                end: i,
                text: sql[start..i].to_string(),
            });
            continue;
        }

        // Dollar-quoted string: $tag$...$tag$ or $$...$$
        if bytes[i] == b'$' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'$' {
                if !(bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    break;
                }
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'$' {
                let delim = &sql[i..=j];
                i = j + 1;
                if let Some(end_pos) = sql[i..].find(delim) {
                    let end_idx = i + end_pos + delim.len();
                    tokens.push(Token {
                        kind: TokenKind::DollarString,
                        start,
                        end: end_idx,
                        text: sql[start..end_idx].to_string(),
                    });
                    i = end_idx;
                    continue;
                }
            }
        }

        // Words (keywords/identifiers/numbers)
        if is_ident_char(bytes[i]) {
            i += 1;
            while i < bytes.len() && is_ident_char(bytes[i]) {
                i += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Word,
                start,
                end: i,
                text: sql[start..i].to_string(),
            });
            continue;
        }

        // Punctuation
        if matches!(bytes[i], b'(' | b')' | b'[' | b']' | b',' | b';') {
            i += 1;
            tokens.push(Token {
                kind: TokenKind::Punct,
                start,
                end: i,
                text: sql[start..i].to_string(),
            });
            continue;
        }

        // Operators we care about: ?  ?|  ?&
        if bytes[i] == b'?' {
            if i + 1 < bytes.len() && (bytes[i + 1] == b'|' || bytes[i + 1] == b'&') {
                i += 2;
                tokens.push(Token {
                    kind: TokenKind::Operator,
                    start,
                    end: i,
                    text: sql[start..i].to_string(),
                });
                continue;
            }
            i += 1;
            tokens.push(Token {
                kind: TokenKind::Operator,
                start,
                end: i,
                text: sql[start..i].to_string(),
            });
            continue;
        }

        // pgvector distance operators: <->  <#>  <=>
        if bytes[i] == b'<' && i + 2 < bytes.len() {
            let next = bytes[i + 1];
            let after = bytes[i + 2];
            if (next == b'-' || next == b'#' || next == b'=') && after == b'>' {
                i += 3;
                tokens.push(Token {
                    kind: TokenKind::Operator,
                    start,
                    end: i,
                    text: sql[start..i].to_string(),
                });
                continue;
            }
        }

        // JSON access operators: ->  ->>
        if bytes[i] == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'>' {
            if i + 2 < bytes.len() && bytes[i + 2] == b'>' {
                i += 3;
            } else {
                i += 2;
            }
            tokens.push(Token {
                kind: TokenKind::Operator,
                start,
                end: i,
                text: sql[start..i].to_string(),
            });
            continue;
        }

        // Fallback: single char
        i += 1;
        tokens.push(Token {
            kind: TokenKind::Other,
            start,
            end: i,
            text: sql[start..i].to_string(),
        });
    }
    tokens
}

fn is_rewrite_boundary_keyword(token_upper: &str) -> bool {
    matches!(
        token_upper,
        "AS" | "SELECT"
            | "FROM"
            | "WHERE"
            | "GROUP"
            | "ORDER"
            | "BY"
            | "HAVING"
            | "LIMIT"
            | "OFFSET"
            | "UNION"
            | "INTERSECT"
            | "EXCEPT"
            | "AND"
            | "OR"
            | "WHEN"
            | "THEN"
            | "ELSE"
            | "END"
            | "ASC"
            | "DESC"
            | "NULLS"
            | "ON"
            | "JOIN"
            | "INNER"
            | "LEFT"
            | "RIGHT"
            | "FULL"
            | "OUTER"
            | "CROSS"
            | "NATURAL"
            | "RETURNING"
            | "INTO"
            | "SET"
            | "CASE"
            | "NOT"
            | "IN"
            | "BETWEEN"
            | "LIKE"
            | "ILIKE"
            | "IS"
    )
}

fn is_comparison_operator_char(tok: &Token) -> bool {
    tok.kind == TokenKind::Other && matches!(tok.text.as_str(), "<" | ">" | "=" | "!")
}

fn find_left_expr_start(tokens: &[Token], op_idx: usize) -> usize {
    let mut depth_paren = 0i32;
    let mut depth_bracket = 0i32;
    for idx in (0..op_idx).rev() {
        let tok = &tokens[idx];
        if matches!(tok.kind, TokenKind::Whitespace | TokenKind::Comment) {
            continue;
        }
        match tok.text.as_str() {
            ")" => depth_paren += 1,
            "(" => {
                if depth_paren > 0 {
                    depth_paren -= 1;
                } else if depth_bracket == 0 {
                    return idx + 1;
                }
            }
            "]" => depth_bracket += 1,
            "[" => {
                if depth_bracket > 0 {
                    depth_bracket -= 1;
                } else if depth_paren == 0 {
                    return idx + 1;
                }
            }
            "," | ";" => {
                if depth_paren == 0 && depth_bracket == 0 {
                    return idx + 1;
                }
            }
            _ => {
                if depth_paren == 0 && depth_bracket == 0 {
                    if is_comparison_operator_char(tok) {
                        return idx + 1;
                    }
                    if tok.kind == TokenKind::Word
                        && is_rewrite_boundary_keyword(&tok.text.to_uppercase())
                    {
                        return idx + 1;
                    }
                }
            }
        }
    }
    0
}

fn find_right_expr_end(tokens: &[Token], op_idx: usize) -> usize {
    let mut depth_paren = 0i32;
    let mut depth_bracket = 0i32;
    for idx in op_idx + 1..tokens.len() {
        let tok = &tokens[idx];
        if matches!(tok.kind, TokenKind::Whitespace | TokenKind::Comment) {
            continue;
        }
        match tok.text.as_str() {
            "(" => depth_paren += 1,
            ")" => {
                if depth_paren > 0 {
                    depth_paren -= 1;
                } else if depth_bracket == 0 {
                    return idx.saturating_sub(1);
                }
            }
            "[" => depth_bracket += 1,
            "]" => {
                if depth_bracket > 0 {
                    depth_bracket -= 1;
                } else if depth_paren == 0 {
                    return idx.saturating_sub(1);
                }
            }
            "," | ";" => {
                if depth_paren == 0 && depth_bracket == 0 {
                    return idx.saturating_sub(1);
                }
            }
            _ => {
                if depth_paren == 0 && depth_bracket == 0 {
                    if is_comparison_operator_char(tok) {
                        return idx.saturating_sub(1);
                    }
                    if tok.kind == TokenKind::Word
                        && is_rewrite_boundary_keyword(&tok.text.to_uppercase())
                    {
                        return idx.saturating_sub(1);
                    }
                }
            }
        }
    }
    tokens.len().saturating_sub(1)
}

fn skip_ws_comments_forward(tokens: &[Token], mut idx: usize, stop: usize) -> usize {
    while idx < stop && matches!(tokens[idx].kind, TokenKind::Whitespace | TokenKind::Comment) {
        idx += 1;
    }
    idx
}

fn skip_ws_comments_backward(tokens: &[Token], mut idx: usize, start: usize) -> usize {
    while idx > start && matches!(tokens[idx].kind, TokenKind::Whitespace | TokenKind::Comment) {
        idx -= 1;
    }
    idx
}

/// Parse-compat rewrite for `ANY/ALL (SELECT ...)`.
///
/// sqlparser-rs doesn't parse the direct PostgreSQL subquery form, but it does
/// parse `ANY/ALL (ARRAY(SELECT ...))`. We only wrap the subquery shape here;
/// semantic handling remains in Analyzer/Rewriter on typed IR.
fn rewrite_all_any_subquery_parse_compat(sql: &str) -> String {
    let mut current = sql.to_string();
    loop {
        let next = rewrite_all_any_subquery_parse_compat_once(&current);
        if next == current {
            return current;
        }
        current = next;
    }
}

fn rewrite_all_any_subquery_parse_compat_once(sql: &str) -> String {
    let tokens = tokenize_sql_for_rewrite(sql);
    if tokens.is_empty() {
        return sql.to_string();
    }

    let mut replacements: Vec<(usize, usize, String)> = Vec::new();
    let mut idx = 0usize;
    while idx < tokens.len() {
        let tok = &tokens[idx];
        if tok.kind != TokenKind::Word
            || (!tok.text.eq_ignore_ascii_case("ANY") && !tok.text.eq_ignore_ascii_case("ALL"))
        {
            idx += 1;
            continue;
        }

        let mut j = skip_ws_comments_forward(&tokens, idx + 1, tokens.len());
        if j >= tokens.len() || tokens[j].kind != TokenKind::Punct || tokens[j].text != "(" {
            idx += 1;
            continue;
        }

        j = skip_ws_comments_forward(&tokens, j + 1, tokens.len());
        if j >= tokens.len()
            || tokens[j].kind != TokenKind::Word
            || !tokens[j].text.eq_ignore_ascii_case("SELECT")
        {
            idx += 1;
            continue;
        }

        let select_start = tokens[j].start;
        if let Some((subquery, end_pos)) = extract_subquery(sql, select_start) {
            replacements.push((select_start, end_pos, format!("ARRAY({}))", subquery)));
            while idx < tokens.len() && tokens[idx].start < end_pos {
                idx += 1;
            }
            continue;
        }

        idx += 1;
    }

    if replacements.is_empty() {
        return sql.to_string();
    }

    replacements.sort_by_key(|(s, _, _)| *s);
    let mut out = sql.to_string();
    for (start, end, repl) in replacements.into_iter().rev() {
        out.replace_range(start..end, &repl);
    }
    out
}

/// Extract a subquery starting at `start` (position of `SELECT`) and return:
/// - subquery text (without the closing `)`)
/// - end position just after that closing `)`
fn extract_subquery(sql: &str, start: usize) -> Option<(String, usize)> {
    let bytes = sql.as_bytes();
    let mut depth = 1; // caller is inside `(...` already
    let mut pos = start;
    while pos < bytes.len() {
        match bytes[pos] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((sql[start..pos].to_string(), pos + 1));
                }
            }
            b'\'' => {
                pos += 1;
                while pos < bytes.len() && bytes[pos] != b'\'' {
                    if bytes[pos] == b'\\' {
                        pos += 1;
                    }
                    pos += 1;
                }
            }
            b'"' => {
                pos += 1;
                while pos < bytes.len() && bytes[pos] != b'"' {
                    pos += 1;
                }
            }
            _ => {}
        }
        pos += 1;
    }
    None
}

fn rewrite_jsonb_exists_ops(sql: &str) -> String {
    let tokens = tokenize_sql_for_rewrite(sql);
    if tokens.is_empty() {
        return sql.to_string();
    }

    let mut replacements: Vec<(usize, usize, String)> = Vec::new();

    for (op_idx, tok) in tokens.iter().enumerate() {
        if tok.kind != TokenKind::Operator {
            continue;
        }
        let func = match tok.text.as_str() {
            "?" => "JSONB_EXISTS",
            "?|" => "JSONB_EXISTS_ANY",
            "?&" => "JSONB_EXISTS_ALL",
            _ => continue,
        };

        let left_start_idx = find_left_expr_start(&tokens, op_idx);
        let left_start_idx = skip_ws_comments_forward(&tokens, left_start_idx, op_idx);

        if left_start_idx >= op_idx {
            continue;
        }

        let mut right_start_idx = op_idx + 1;
        right_start_idx = skip_ws_comments_forward(&tokens, right_start_idx, tokens.len());

        if right_start_idx >= tokens.len() {
            continue;
        }

        let mut right_end_idx = find_right_expr_end(&tokens, op_idx);
        right_end_idx = skip_ws_comments_backward(&tokens, right_end_idx, right_start_idx);

        if right_end_idx < right_start_idx {
            continue;
        }

        let replace_start = tokens[left_start_idx].start;
        let replace_end = tokens[right_end_idx].end;

        let left_expr = sql[replace_start..tok.start].trim();
        let right_expr = sql[tokens[right_start_idx].start..replace_end].trim();

        if left_expr.is_empty() || right_expr.is_empty() {
            continue;
        }

        let replacement = format!("({}({}, {}))", func, left_expr, right_expr);
        replacements.push((replace_start, replace_end, replacement));
    }

    if replacements.is_empty() {
        return sql.to_string();
    }

    // Apply replacements from right to left so offsets remain valid.
    replacements.sort_by_key(|(s, _, _)| *s);
    let mut out = sql.to_string();
    for (start, end, repl) in replacements.into_iter().rev() {
        out.replace_range(start..end, &repl);
    }
    out
}

fn is_ident_char(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
}

fn rewrite_vector_distance_ops(sql: &str) -> String {
    let tokens = tokenize_sql_for_rewrite(sql);
    if tokens.is_empty() {
        return sql.to_string();
    }

    let mut replacements: Vec<(usize, usize, String)> = Vec::new();

    for (op_idx, tok) in tokens.iter().enumerate() {
        if tok.kind != TokenKind::Operator {
            continue;
        }
        let func = match tok.text.as_str() {
            "<->" => "l2_distance",
            "<#>" => "inner_product",
            "<=>" => "cosine_distance",
            _ => continue,
        };

        let left_start_idx = find_left_expr_start(&tokens, op_idx);
        let left_start_idx = skip_ws_comments_forward(&tokens, left_start_idx, op_idx);

        if left_start_idx >= op_idx {
            continue;
        }

        let mut right_start_idx = op_idx + 1;
        right_start_idx = skip_ws_comments_forward(&tokens, right_start_idx, tokens.len());

        if right_start_idx >= tokens.len() {
            continue;
        }

        let mut right_end_idx = find_right_expr_end(&tokens, op_idx);
        right_end_idx = skip_ws_comments_backward(&tokens, right_end_idx, right_start_idx);

        if right_end_idx < right_start_idx {
            continue;
        }

        let replace_start = tokens[left_start_idx].start;
        let replace_end = tokens[right_end_idx].end;

        let left_expr = sql[replace_start..tok.start].trim();
        let right_expr = sql[tokens[right_start_idx].start..replace_end].trim();

        if left_expr.is_empty() || right_expr.is_empty() {
            continue;
        }

        let replacement = format!("({}({}, {}))", func, left_expr, right_expr);
        replacements.push((replace_start, replace_end, replacement));
    }

    if replacements.is_empty() {
        return sql.to_string();
    }

    replacements.sort_by_key(|(s, _, _)| *s);
    let mut out = sql.to_string();
    for (start, end, repl) in replacements.into_iter().rev() {
        out.replace_range(start..end, &repl);
    }
    out
}

fn rewrite_at_time_zone_placeholders(sql: &str) -> String {
    let tokens = tokenize_sql_for_rewrite(sql);
    if tokens.is_empty() {
        return sql.to_string();
    }

    let mut replacements: Vec<(usize, usize, String)> = Vec::new();
    let mut idx = 0usize;

    while idx < tokens.len() {
        let tok = &tokens[idx];
        if tok.kind != TokenKind::Word || !tok.text.eq_ignore_ascii_case("AT") {
            idx += 1;
            continue;
        }

        let mut j = idx + 1;
        j = skip_ws_comments_forward(&tokens, j, tokens.len());
        if j >= tokens.len()
            || tokens[j].kind != TokenKind::Word
            || !tokens[j].text.eq_ignore_ascii_case("TIME")
        {
            idx += 1;
            continue;
        }

        j += 1;
        j = skip_ws_comments_forward(&tokens, j, tokens.len());
        if j >= tokens.len()
            || tokens[j].kind != TokenKind::Word
            || !tokens[j].text.eq_ignore_ascii_case("ZONE")
        {
            idx += 1;
            continue;
        }

        j += 1;
        j = skip_ws_comments_forward(&tokens, j, tokens.len());
        if j + 1 >= tokens.len() {
            idx += 1;
            continue;
        }

        let is_placeholder = tokens[j].kind == TokenKind::Other
            && tokens[j].text == "$"
            && tokens[j + 1].kind == TokenKind::Word
            && tokens[j + 1].text.chars().all(|c| c.is_ascii_digit());

        if !is_placeholder {
            idx += 1;
            continue;
        }

        replacements.push((
            tok.start,
            tokens[j + 1].end,
            "AT TIME ZONE 'UTC'".to_string(),
        ));
        idx = j + 2;
    }

    if replacements.is_empty() {
        return sql.to_string();
    }

    // Apply replacements from right to left so offsets remain valid.
    replacements.sort_by_key(|(s, _, _)| *s);
    let mut out = sql.to_string();
    for (start, end, repl) in replacements.into_iter().rev() {
        out.replace_range(start..end, &repl);
    }
    out
}

fn rewrite_reset_role(sql: &str) -> String {
    let tokens = tokenize_sql_for_rewrite(sql);
    if tokens.is_empty() {
        return sql.to_string();
    }

    let mut replacements: Vec<(usize, usize, String)> = Vec::new();

    let mut stmt_start = 0usize;
    for (idx, tok) in tokens.iter().enumerate() {
        if tok.kind == TokenKind::Punct && tok.text == ";" {
            collect_reset_role_rewrite(&tokens, stmt_start, idx, &mut replacements);
            stmt_start = idx + 1;
        }
    }
    collect_reset_role_rewrite(&tokens, stmt_start, tokens.len(), &mut replacements);

    if replacements.is_empty() {
        return sql.to_string();
    }

    // Apply replacements from right to left so offsets remain valid.
    replacements.sort_by_key(|(s, _, _)| *s);
    let mut out = sql.to_string();
    for (start, end, repl) in replacements.into_iter().rev() {
        out.replace_range(start..end, &repl);
    }
    out
}

fn collect_reset_role_rewrite(
    tokens: &[Token],
    stmt_start: usize,
    stmt_end: usize,
    replacements: &mut Vec<(usize, usize, String)>,
) {
    let idx = skip_ws_comments_forward(tokens, stmt_start, stmt_end);
    if idx >= stmt_end
        || tokens[idx].kind != TokenKind::Word
        || !tokens[idx].text.eq_ignore_ascii_case("RESET")
    {
        return;
    }

    let mut j = idx + 1;
    j = skip_ws_comments_forward(tokens, j, stmt_end);
    if j >= stmt_end
        || tokens[j].kind != TokenKind::Word
        || !tokens[j].text.eq_ignore_ascii_case("ROLE")
    {
        return;
    }

    let k = skip_ws_comments_forward(tokens, j + 1, stmt_end);
    if k != stmt_end {
        return;
    }

    replacements.push((
        tokens[idx].start,
        tokens[j].end,
        "SET ROLE NONE".to_string(),
    ));
}
fn find_keyword_outside_strings(query: &str, keyword: &str) -> Option<usize> {
    let bytes = query.as_bytes();
    let kw = keyword.as_bytes();
    if kw.is_empty() || bytes.len() < kw.len() {
        return None;
    }

    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut dollar_delim: Option<Vec<u8>> = None;

    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(delim) = dollar_delim.as_ref() {
            let delim_len = delim.len();
            let matches =
                i + delim_len <= bytes.len() && &bytes[i..i + delim_len] == delim.as_slice();
            if matches {
                dollar_delim = None;
                i += delim_len;
            } else {
                i += 1;
            }
            continue;
        }

        let b = bytes[i];
        if b == b'\'' && !in_double_quote {
            if in_single_quote && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                i += 2;
                continue;
            }
            in_single_quote = !in_single_quote;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single_quote {
            if in_double_quote && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                i += 2;
                continue;
            }
            in_double_quote = !in_double_quote;
            i += 1;
            continue;
        }

        if !in_single_quote && !in_double_quote && b == b'$' {
            // Skip placeholders like $1 and keep scanning.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                i = j;
                continue;
            }

            // Track dollar-quoted strings ($tag$...$tag$ or $$...$$)
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'$' {
                if !(bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    break;
                }
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'$' {
                dollar_delim = Some(bytes[i..=j].to_vec());
                i = j + 1;
                continue;
            }
        }

        if !in_single_quote && !in_double_quote && i + kw.len() <= bytes.len() {
            let before_ok = i == 0 || !is_ident_char(bytes[i - 1]);
            let after_ok = i + kw.len() == bytes.len() || !is_ident_char(bytes[i + kw.len()]);
            if before_ok && after_ok {
                let mut matched = true;
                for (j, kw_b) in kw.iter().enumerate() {
                    if bytes[i + j].to_ascii_uppercase() != kw_b.to_ascii_uppercase() {
                        matched = false;
                        break;
                    }
                }
                if matched {
                    return Some(i);
                }
            }
        }

        i += 1;
    }

    None
}

fn parse_returning_items(
    dialect: &PostgreSqlDialect,
    clause: &str,
) -> Result<Vec<sqlparser::ast::SelectItem>> {
    let sql = format!("SELECT {}", clause);
    let mut stmts = Parser::parse_sql(dialect, &sql)
        .map_err(|e| anyhow!("Failed to parse RETURNING clause: {}", e))?;
    if stmts.len() != 1 {
        return Err(anyhow!("Failed to parse RETURNING clause: {}", clause));
    }

    let stmt = stmts.remove(0);
    let Statement::Query(query) = stmt else {
        return Err(anyhow!("Failed to parse RETURNING clause: {}", clause));
    };

    if let sqlparser::ast::SetExpr::Select(select) = query.body.as_ref() {
        Ok(select.projection.clone())
    } else {
        Err(anyhow!("Failed to parse RETURNING clause: {}", clause))
    }
}

fn try_parse_statement_with_returning_fallback(
    dialect: &PostgreSqlDialect,
    sql: &str,
) -> Option<Vec<Statement>> {
    let pos = find_keyword_outside_strings(sql, "RETURNING")?;
    let head = sql.get(..pos)?.trim_end();
    let tail = sql.get(pos + "RETURNING".len()..)?.trim();
    let clause = tail.trim_end_matches(';').trim();
    if clause.is_empty() {
        return None;
    }

    let returning_items = parse_returning_items(dialect, clause).ok()?;
    let mut stmts = Parser::parse_sql(dialect, head).ok()?;
    if stmts.len() != 1 {
        return None;
    }
    match &mut stmts[0] {
        Statement::Insert { returning, .. }
        | Statement::Update { returning, .. }
        | Statement::Delete { returning, .. } => {
            *returning = Some(returning_items);
            Some(stmts)
        }
        _ => None,
    }
}

/// Parse a SQL string into AST statements
pub fn parse_sql(sql: &str) -> Result<Vec<Statement>> {
    let dialect = PostgreSqlDialect {};
    let preprocessed = preprocess_sql(sql);
    match Parser::parse_sql(&dialect, &preprocessed) {
        Ok(stmts) => Ok(stmts),
        Err(e) => {
            if let Some(stmts) =
                try_parse_statement_with_returning_fallback(&dialect, preprocessed.trim())
            {
                return Ok(stmts);
            }
            Err(anyhow!("SQL parse error: {}", e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_select() {
        let stmts = parse_sql("SELECT * FROM users").unwrap();
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn test_parse_reset_role_rewrite() {
        let stmts = parse_sql("RESET ROLE").unwrap();
        assert_eq!(stmts.len(), 1);
        assert!(matches!(
            stmts[0],
            Statement::SetRole {
                role_name: None,
                ..
            }
        ));
    }

    #[test]
    fn test_parse_create_table() {
        let stmts = parse_sql("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)").unwrap();
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn test_parse_reset_role() {
        let stmts = parse_sql("RESET ROLE").unwrap();
        assert_eq!(stmts.len(), 1);
        match &stmts[0] {
            Statement::SetRole { role_name, .. } => assert!(role_name.is_none()),
            other => panic!("expected SET ROLE, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_insert() {
        let stmts = parse_sql("INSERT INTO users (id, name) VALUES (1, 'Alice')").unwrap();
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn test_parse_single_digit_placeholders() {
        let stmts = parse_sql("INSERT INTO users (a, b, c) VALUES ($1, $2, $3)").unwrap();
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn test_parse_double_digit_placeholders() {
        let result = parse_sql("INSERT INTO users (a, b, c) VALUES ($10, $11, $12)");
        match result {
            Ok(stmts) => {
                assert_eq!(stmts.len(), 1);
                println!("Double-digit placeholders parsed successfully!");
            }
            Err(e) => {
                println!("Failed to parse double-digit placeholders: {}", e);
                panic!("sqlparser-rs doesn't support double-digit placeholders");
            }
        }
    }

    #[test]
    fn test_parse_at_time_zone_placeholder() {
        let stmts = parse_sql("SELECT TIMESTAMP '2024-01-15 10:00:00' AT TIME ZONE $1").unwrap();
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn test_parse_reset_role_via_rewrite() {
        let stmts = parse_sql("RESET ROLE").unwrap();
        assert_eq!(stmts.len(), 1);
        assert!(matches!(
            stmts[0],
            Statement::SetRole {
                role_name: None,
                ..
            }
        ));
    }

    #[test]
    fn test_rewrite_reset_role_is_statement_aware() {
        assert_eq!(preprocess_sql("RESET ROLE"), "SET ROLE NONE");
        assert_eq!(preprocess_sql("SELECT 'RESET ROLE'"), "SELECT 'RESET ROLE'");
        assert_eq!(
            preprocess_sql("-- RESET ROLE\nSELECT 1"),
            "-- RESET ROLE\nSELECT 1"
        );
        assert_eq!(
            preprocess_sql("RESET ROLE; SELECT 'RESET ROLE';"),
            "SET ROLE NONE; SELECT 'RESET ROLE';"
        );
    }

    #[test]
    fn test_rewrite_does_not_touch_non_role_reset() {
        // Non-ROLE RESET is handled by the executor's raw SQL path (RawSqlKind::Reset),
        // not by parser rewrite. The parser should leave them unchanged.
        assert_eq!(preprocess_sql("RESET timezone"), "RESET timezone");
        assert_eq!(preprocess_sql("RESET ALL"), "RESET ALL");
        // RESET ROLE is still rewritten
        assert_eq!(preprocess_sql("RESET ROLE"), "SET ROLE NONE");
    }

    #[test]
    fn test_parse_multi_row_double_digit() {
        let sql = "INSERT INTO users (a, b, c) VALUES ($1, $2, $3), ($4, $5, $6), ($7, $8, $9), ($10, $11, $12)";
        let result = parse_sql(sql);
        match result {
            Ok(stmts) => {
                assert_eq!(stmts.len(), 1);
                println!("Multi-row with double-digit placeholders parsed successfully!");
            }
            Err(e) => {
                println!(
                    "Failed to parse multi-row with double-digit placeholders: {}",
                    e
                );
                panic!("sqlparser-rs issue with double-digit placeholders in multi-row INSERT");
            }
        }
    }

    #[test]
    fn test_parse_create_sequence_out_of_order_pg_dump_style() {
        // `pg_dump` commonly emits START/INCREMENT/NO MINVALUE/NO MAXVALUE out of sqlparser-rs'
        // expected order. `parse_sql` should normalize it for compatibility.
        let sql = r#"
            CREATE SEQUENCE public.task_id_sequence
                START WITH 1
                INCREMENT BY 1
                NO MINVALUE
                NO MAXVALUE
                CACHE 1;
        "#;
        let stmts = parse_sql(sql).unwrap();
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn test_default_in_on_conflict() {
        let sql = r#"INSERT INTO t(a, b) VALUES (1, 2) ON CONFLICT (a) DO UPDATE SET b = DEFAULT"#;
        let statements = parse_sql(sql).unwrap();
        println!("Parsed: {:#?}", statements);
    }

    #[test]
    fn test_explain_analyze_with_parens() {
        let sql = "EXPLAIN (ANALYZE) SELECT * FROM t";
        let stmts = parse_sql(sql).unwrap();
        assert_eq!(stmts.len(), 1);
        if let Statement::Explain { analyze, .. } = &stmts[0] {
            assert!(*analyze);
        } else {
            panic!("Expected EXPLAIN statement");
        }
    }

    #[test]
    fn test_explain_analyze_verbose_with_parens() {
        let sql = "EXPLAIN (ANALYZE, VERBOSE) SELECT 1";
        let stmts = parse_sql(sql).unwrap();
        assert_eq!(stmts.len(), 1);
        if let Statement::Explain {
            analyze, verbose, ..
        } = &stmts[0]
        {
            assert!(*analyze);
            assert!(*verbose);
        } else {
            panic!("Expected EXPLAIN statement");
        }
    }

    #[test]
    fn test_preprocess_explain() {
        assert_eq!(
            preprocess_sql("EXPLAIN (ANALYZE) SELECT 1"),
            "EXPLAIN ANALYZE SELECT 1"
        );
        assert_eq!(
            preprocess_sql("EXPLAIN (ANALYZE, VERBOSE) SELECT 1"),
            "EXPLAIN ANALYZE  VERBOSE SELECT 1"
        );
        assert_eq!(
            preprocess_sql("EXPLAIN ANALYZE SELECT 1"),
            "EXPLAIN ANALYZE SELECT 1"
        );
        assert_eq!(preprocess_sql("SELECT 1"), "SELECT 1");
    }

    #[test]
    fn test_all_any_subqueries_parse_via_parse_compat_wrapper() {
        let all_sql = "SELECT 1 = ALL (SELECT x FROM t)";
        let all_preprocessed = preprocess_sql(all_sql);
        assert_eq!(all_preprocessed, "SELECT 1 = ALL (ARRAY(SELECT x FROM t))");
        let stmts = parse_sql(all_sql).unwrap();
        assert_eq!(stmts.len(), 1);

        let any_sql = "SELECT 1 > ANY (SELECT x FROM t)";
        let any_preprocessed = preprocess_sql(any_sql);
        assert_eq!(any_preprocessed, "SELECT 1 > ANY (ARRAY(SELECT x FROM t))");
        let stmts = parse_sql(any_sql).unwrap();
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn test_preprocess_keeps_all_any_inside_literals_and_comments() {
        assert_eq!(
            preprocess_sql("SELECT '1 = ALL (SELECT x FROM t)'"),
            "SELECT '1 = ALL (SELECT x FROM t)'"
        );
        assert_eq!(
            preprocess_sql("-- 1 > ANY (SELECT x FROM t)\nSELECT 1"),
            "-- 1 > ANY (SELECT x FROM t)\nSELECT 1"
        );
    }

    #[test]
    fn test_create_sequence_options() {
        let cases = [
            ("no options", "CREATE SEQUENCE s1"),
            ("start only", "CREATE SEQUENCE s1 START 1"),
            ("start with", "CREATE SEQUENCE s1 START WITH 1"),
            ("minvalue", "CREATE SEQUENCE s1 MINVALUE 1"),
            ("maxvalue", "CREATE SEQUENCE s1 MAXVALUE 100"),
            ("cycle", "CREATE SEQUENCE s1 CYCLE"),
            ("no cycle", "CREATE SEQUENCE s1 NO CYCLE"),
            ("increment only", "CREATE SEQUENCE s1 INCREMENT 1"),
            ("increment by", "CREATE SEQUENCE s1 INCREMENT BY 1"),
            (
                "start + increment (order 1)",
                "CREATE SEQUENCE s1 START WITH 1 INCREMENT BY 1",
            ),
            (
                "start + increment (order 2)",
                "CREATE SEQUENCE s1 INCREMENT BY 1 START WITH 1",
            ),
            (
                "increment + start (no BY/WITH)",
                "CREATE SEQUENCE s1 INCREMENT 1 START 1",
            ),
        ];
        for (desc, sql) in cases {
            let result = parse_sql(sql);
            println!("{}: {:?}", desc, result.is_ok());
            if let Err(e) = &result {
                println!("  Error: {}", e);
            }
            assert!(result.is_ok(), "{} should parse: {}", desc, sql);
        }
    }

    #[test]
    fn test_preprocess_create_sequence() {
        assert_eq!(
            preprocess_create_sequence("CREATE SEQUENCE s1 START WITH 1 INCREMENT BY 2"),
            Some("CREATE SEQUENCE s1 INCREMENT BY 2 START WITH 1".to_string())
        );
        assert_eq!(
            preprocess_create_sequence("CREATE SEQUENCE s1 INCREMENT BY 2 START WITH 1"),
            None
        );
        assert_eq!(
            preprocess_create_sequence("CREATE SEQUENCE s1 START 1"),
            None
        );
        println!("Testing with semicolon:");
        let result =
            preprocess_create_sequence("CREATE SEQUENCE test_inc START WITH 1 INCREMENT BY 1;");
        println!("Result: {:?}", result);

        println!("Testing multi-statement:");
        let result = preprocess_create_sequence(
            "CREATE SEQUENCE s1 START WITH 1 INCREMENT BY 1;\nSELECT 1;",
        );
        println!("Result: {:?}", result);
        assert_eq!(
            result,
            Some("CREATE SEQUENCE s1 INCREMENT BY 1 START WITH 1;\nSELECT 1;".to_string())
        );
    }

    #[test]
    fn test_rewrite_vector_distance_l2() {
        let result = rewrite_vector_distance_ops("SELECT v <-> '[1,0,0]' FROM vec_test");
        assert!(result.contains("l2_distance(v, '[1,0,0]')"));
    }

    #[test]
    fn test_rewrite_vector_distance_in_order_by() {
        let result = rewrite_vector_distance_ops("SELECT * FROM t ORDER BY v <#> '[1,0,0]' ASC");
        assert!(result.contains("inner_product(v, '[1,0,0]')"));
    }

    #[test]
    fn test_rewrite_vector_distance_cosine() {
        let result = rewrite_vector_distance_ops("SELECT v <=> '[0,1,0]' FROM vec_test");
        assert!(result.contains("cosine_distance(v, '[0,1,0]')"));
    }

    #[test]
    fn test_rewrite_vector_distance_with_comparison() {
        let result = rewrite_vector_distance_ops(
            "SELECT * FROM t WHERE a <#> b < -0.2 ORDER BY a <#> b ASC",
        );
        assert!(
            result.contains("(inner_product(a, b)) < -0.2"),
            "got: {}",
            result
        );
        assert!(
            result.contains("(inner_product(a, b)) ASC"),
            "got: {}",
            result
        );
    }

    #[test]
    fn test_rewrite_vector_distance_comparison_boundary() {
        let result = rewrite_vector_distance_ops("SELECT a <-> b > 5 FROM t");
        assert!(
            result.contains("(l2_distance(a, b)) > 5"),
            "got: {}",
            result
        );
    }

    #[test]
    fn test_rewrite_vector_distance_join_on_boundary() {
        let result = rewrite_vector_distance_ops("SELECT * FROM t1 JOIN t2 ON t1.v <-> t2.v < 1");
        assert!(
            result.contains("(l2_distance(t1.v, t2.v)) < 1"),
            "JOIN/ON should be boundaries; got: {}",
            result
        );
    }

    #[test]
    fn test_rewrite_vector_distance_left_join_on() {
        let result = rewrite_vector_distance_ops(
            "SELECT * FROM t1 LEFT JOIN t2 ON t1.v <=> t2.v < 0.5 ORDER BY t1.v <-> t2.v",
        );
        assert!(
            result.contains("(cosine_distance(t1.v, t2.v)) < 0.5"),
            "LEFT JOIN ON should be boundaries; got: {}",
            result
        );
        assert!(
            result.contains("(l2_distance(t1.v, t2.v))"),
            "ORDER BY rewrite should work; got: {}",
            result
        );
    }

    #[test]
    fn test_rewrite_vector_distance_no_false_positive_in_strings() {
        let result = rewrite_vector_distance_ops("SELECT '<->' FROM t");
        assert_eq!(result, "SELECT '<->' FROM t");
    }

    #[test]
    fn test_rewrite_jsonb_exists_ops_keeps_arrow_left_expr() {
        let sql = "SELECT 1 WHERE data->'tags' ? 'sale'";
        let rewritten = rewrite_jsonb_exists_ops(sql);
        assert_eq!(
            rewritten,
            "SELECT 1 WHERE (JSONB_EXISTS(data->'tags', 'sale'))"
        );
    }

    #[test]
    fn test_rewrite_vector_distance_parses() {
        let sql = "SELECT v <-> '[1,0,0]'::vector(3) FROM vec_test";
        let stmts = parse_sql(sql).unwrap();
        assert_eq!(stmts.len(), 1);
    }
}
