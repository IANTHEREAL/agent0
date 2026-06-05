//! Parse-time operator rewrites for sqlparser-rs compatibility.
//!
//! Rewrites PostgreSQL operators that sqlparser-rs cannot parse into equivalent
//! function-call or expression forms:
//!
//! - `ANY/ALL(SELECT ...)` -> `ANY/ALL(ARRAY(SELECT ...))`
//! - JSONB existence operators (`?`, `?|`, `?&`) -> function calls
//! - pgvector distance operators (`<->`, `<#>`, `<=>`) -> function calls
//! - `AT TIME ZONE $n` -> `AT TIME ZONE 'UTC'` (parse-time placeholder)
//! - `RESET ROLE` -> `SET ROLE NONE`
//! - `CREATE/ALTER/DROP USER` -> `... ROLE` (with `CREATE USER` implying `LOGIN`)

use super::tokenizer::{
    find_left_expr_start, find_right_expr_end, skip_ws_comments_backward, skip_ws_comments_forward,
    tokenize_sql_for_rewrite, Token, TokenKind,
};

/// Parse-compat rewrite for `ANY/ALL (SELECT ...)`.
///
/// sqlparser-rs doesn't parse the direct PostgreSQL subquery form, but it does
/// parse `ANY/ALL (ARRAY(SELECT ...))`. We only wrap the subquery shape here;
/// semantic handling remains in Analyzer/Rewriter on typed IR.
///
/// Classification: parse-compatibility shim.
/// Exit condition: remove when sqlparser-rs supports direct `ANY/ALL(SELECT ...)`.
pub(super) fn rewrite_all_any_subquery_parse_compat(sql: &str) -> String {
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

/// Rewrite JSONB existence operators (`?`, `?|`, `?&`) into internal function calls.
///
/// Classification: parse-compatibility shim.
/// Exit condition: remove when sqlparser-rs can parse these operators in PostgreSQL mode.
pub(super) fn rewrite_jsonb_exists_ops(sql: &str) -> String {
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

/// Rewrite vector distance operators (`<->`, `<#>`, `<=>`) into function calls.
///
/// Classification: parse-compatibility shim.
/// Exit condition: remove when sqlparser-rs supports extension custom operators.
pub(super) fn rewrite_vector_distance_ops(sql: &str) -> String {
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
            "<#>" => "vector_negative_inner_product",
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

/// Rewrite `AT TIME ZONE $n` placeholders to a literal for parse-time validation.
///
/// Classification: parse-compatibility shim.
/// Exit condition: remove when sqlparser-rs accepts expression/placeholder form.
pub(super) fn rewrite_at_time_zone_placeholders(sql: &str) -> String {
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

/// Rewrite standalone `RESET ROLE` statements to `SET ROLE NONE`.
///
/// Classification: parse-compatibility shim.
/// Exit condition: remove when sqlparser-rs supports `RESET ROLE`.
pub(super) fn rewrite_reset_role(sql: &str) -> String {
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

/// Rewrite `CREATE/ALTER/DROP USER` to the PostgreSQL-equivalent `... ROLE`.
///
/// PostgreSQL treats USER as an alias of ROLE for these statements, with one
/// semantic tweak: `CREATE USER` implies `LOGIN` by default (unlike
/// `CREATE ROLE`, which defaults to `NOLOGIN`).
///
/// This rewrite intentionally excludes `... USER MAPPING ...` statements (FDW
/// user mappings), which are unrelated and must not be rewritten.
///
/// Classification: parse-compatibility shim.
/// Exit condition: remove when sqlparser-rs supports USER aliases.
pub(super) fn rewrite_user_role_aliases(sql: &str) -> String {
    let tokens = tokenize_sql_for_rewrite(sql);
    if tokens.is_empty() {
        return sql.to_string();
    }

    let mut replacements: Vec<(usize, usize, String)> = Vec::new();

    let mut stmt_start = 0usize;
    for (idx, tok) in tokens.iter().enumerate() {
        if tok.kind == TokenKind::Punct && tok.text == ";" {
            collect_user_role_alias_rewrite(&tokens, stmt_start, idx, &mut replacements);
            stmt_start = idx + 1;
        }
    }
    collect_user_role_alias_rewrite(&tokens, stmt_start, tokens.len(), &mut replacements);

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

fn collect_user_role_alias_rewrite(
    tokens: &[Token],
    stmt_start: usize,
    stmt_end: usize,
    replacements: &mut Vec<(usize, usize, String)>,
) {
    let idx = skip_ws_comments_forward(tokens, stmt_start, stmt_end);
    if idx >= stmt_end || tokens[idx].kind != TokenKind::Word {
        return;
    }

    let verb = tokens[idx].text.as_str();
    if !verb.eq_ignore_ascii_case("CREATE")
        && !verb.eq_ignore_ascii_case("ALTER")
        && !verb.eq_ignore_ascii_case("DROP")
    {
        return;
    }

    let user_idx = skip_ws_comments_forward(tokens, idx + 1, stmt_end);
    if user_idx >= stmt_end
        || tokens[user_idx].kind != TokenKind::Word
        || !tokens[user_idx].text.eq_ignore_ascii_case("USER")
    {
        return;
    }

    let after_user = skip_ws_comments_forward(tokens, user_idx + 1, stmt_end);
    if after_user < stmt_end
        && tokens[after_user].kind == TokenKind::Word
        && tokens[after_user].text.eq_ignore_ascii_case("MAPPING")
    {
        return;
    }

    // Rewrite USER -> ROLE for CREATE/ALTER/DROP.
    replacements.push((
        tokens[user_idx].start,
        tokens[user_idx].end,
        "ROLE".to_string(),
    ));

    // CREATE USER additionally implies LOGIN.
    if !verb.eq_ignore_ascii_case("CREATE") {
        return;
    }

    // Skip optional IF NOT EXISTS.
    let mut name_idx = after_user;
    if name_idx < stmt_end
        && tokens[name_idx].kind == TokenKind::Word
        && tokens[name_idx].text.eq_ignore_ascii_case("IF")
    {
        let not_idx = skip_ws_comments_forward(tokens, name_idx + 1, stmt_end);
        let exists_idx = skip_ws_comments_forward(tokens, not_idx + 1, stmt_end);
        if exists_idx < stmt_end
            && tokens[not_idx].kind == TokenKind::Word
            && tokens[not_idx].text.eq_ignore_ascii_case("NOT")
            && tokens[exists_idx].kind == TokenKind::Word
            && tokens[exists_idx].text.eq_ignore_ascii_case("EXISTS")
        {
            name_idx = skip_ws_comments_forward(tokens, exists_idx + 1, stmt_end);
        }
    }

    if name_idx >= stmt_end {
        return;
    }

    // Consume one or more role names separated by commas.
    let mut cursor = name_idx;
    loop {
        if cursor >= stmt_end
            || !matches!(
                tokens[cursor].kind,
                TokenKind::Word | TokenKind::QuotedIdent
            )
        {
            break;
        }
        cursor = skip_ws_comments_forward(tokens, cursor + 1, stmt_end);
        if cursor < stmt_end
            && tokens[cursor].kind == TokenKind::Punct
            && tokens[cursor].text == ","
        {
            cursor = skip_ws_comments_forward(tokens, cursor + 1, stmt_end);
            continue;
        }
        break;
    }

    // Insert LOGIN after an existing WITH, otherwise inject `WITH LOGIN`.
    let opts_idx = cursor;
    if opts_idx < stmt_end
        && tokens[opts_idx].kind == TokenKind::Word
        && tokens[opts_idx].text.eq_ignore_ascii_case("WITH")
    {
        replacements.push((
            tokens[opts_idx].end,
            tokens[opts_idx].end,
            " LOGIN".to_string(),
        ));
        return;
    }

    let insert_at = if opts_idx < stmt_end {
        tokens[opts_idx].start
    } else if stmt_end > stmt_start {
        let last = skip_ws_comments_backward(tokens, stmt_end - 1, stmt_start);
        tokens[last].end
    } else {
        return;
    };

    let suffix = if opts_idx < stmt_end {
        " WITH LOGIN ".to_string()
    } else {
        " WITH LOGIN".to_string()
    };
    replacements.push((insert_at, insert_at, suffix));
}

/// Rewrite PostgreSQL TABLE <relation> shorthand to SELECT * FROM <relation>.
///
/// Handles three forms:
/// - `TABLE <relation>` -> `SELECT * FROM <relation>`
/// - `TABLE ONLY <relation>` -> `SELECT * FROM <relation>` (ONLY stripped)
/// - `TABLE <relation> *` -> `SELECT * FROM <relation>` (* stripped)
///
/// Trailing clauses (ORDER BY, LIMIT, OFFSET, FETCH, FOR) are preserved verbatim.
/// DDL statements (CREATE TABLE, ALTER TABLE, DROP TABLE) are not affected.
///
/// Classification: parse-normalization shim.
/// Exit condition: remove when sqlparser-rs supports TABLE shorthand natively.
pub(super) fn rewrite_table_shorthand(sql: &str) -> Result<String, String> {
    let tokens = tokenize_sql_for_rewrite(sql);
    if tokens.is_empty() {
        return Ok(sql.to_string());
    }

    let mut mods: Vec<(usize, usize, String)> = Vec::new();

    for (i, tok) in tokens.iter().enumerate() {
        if tok.kind != TokenKind::Word || !tok.text.eq_ignore_ascii_case("TABLE") {
            continue;
        }
        if !is_table_shorthand_position(&tokens, i) {
            continue;
        }

        let next = skip_ws_comments_forward(&tokens, i + 1, tokens.len());
        if next >= tokens.len() {
            continue;
        }

        // Detect ONLY keyword.
        let has_only =
            tokens[next].kind == TokenKind::Word && tokens[next].text.eq_ignore_ascii_case("ONLY");

        let rel_start = if has_only {
            skip_ws_comments_forward(&tokens, next + 1, tokens.len())
        } else {
            next
        };
        if rel_start >= tokens.len() {
            continue;
        }

        // Relation must begin with an identifier.
        if !matches!(
            tokens[rel_start].kind,
            TokenKind::Word | TokenKind::QuotedIdent
        ) {
            continue;
        }

        // Consume schema-qualified parts: schema.table
        let mut rel_end = rel_start;
        let mut pos = skip_ws_comments_forward(&tokens, rel_start + 1, tokens.len());
        while pos < tokens.len() && tokens[pos].text == "." {
            let after_dot = skip_ws_comments_forward(&tokens, pos + 1, tokens.len());
            if after_dot < tokens.len()
                && matches!(
                    tokens[after_dot].kind,
                    TokenKind::Word | TokenKind::QuotedIdent
                )
            {
                rel_end = after_dot;
                pos = skip_ws_comments_forward(&tokens, after_dot + 1, tokens.len());
            } else {
                break;
            }
        }

        // Check for trailing `*` (inheritance wildcard - stripped since we don't
        // support table inheritance).
        let after_rel = skip_ws_comments_forward(&tokens, rel_end + 1, tokens.len());
        let has_star = after_rel < tokens.len() && tokens[after_rel].text == "*";

        // P1: TABLE ONLY <relation> * is a syntax error in PostgreSQL —
        // ONLY and * are mutually exclusive inheritance modifiers.
        if has_only && has_star {
            return Err("syntax error at or near \"*\"".to_string());
        }

        // P2: Validate tail grammar — only TABLE-valid clauses are allowed
        // after the relation (ORDER BY, LIMIT, OFFSET, FETCH, FOR, set ops,
        // statement terminators). Reject non-PG tails like WHERE, GROUP BY, etc.
        let tail_start = if has_star {
            skip_ws_comments_forward(&tokens, after_rel + 1, tokens.len())
        } else {
            after_rel
        };
        if tail_start < tokens.len() {
            let tail_tok = &tokens[tail_start];
            let valid_tail = match tail_tok.kind {
                TokenKind::Punct => matches!(tail_tok.text.as_str(), ";" | ")"),
                TokenKind::Word => {
                    let upper = tail_tok.text.to_uppercase();
                    matches!(
                        upper.as_str(),
                        "ORDER"
                            | "LIMIT"
                            | "OFFSET"
                            | "FETCH"
                            | "FOR"
                            | "UNION"
                            | "INTERSECT"
                            | "EXCEPT"
                    )
                }
                _ => false,
            };
            if !valid_tail {
                return Err(format!("syntax error at or near \"{}\"", tail_tok.text));
            }
        }

        // Replace range: TABLE [ONLY] -> SELECT * FROM
        let replace_end = if has_only { tokens[next].end } else { tok.end };
        mods.push((tok.start, replace_end, "SELECT * FROM".to_string()));

        // Strip trailing inheritance `*`.
        if has_star {
            mods.push((tokens[rel_end].end, tokens[after_rel].end, String::new()));
        }
    }

    if mods.is_empty() {
        return Ok(sql.to_string());
    }

    // Apply in reverse byte order so earlier indices stay valid.
    mods.sort_by_key(|m| std::cmp::Reverse(m.0));
    let mut result = sql.to_string();
    for (start, end, replacement) in mods {
        result.replace_range(start..end, &replacement);
    }
    Ok(result)
}

/// Returns `true` when the `TABLE` token at `table_idx` is at a position
/// where PostgreSQL allows the `TABLE <relation>` shorthand.
fn is_table_shorthand_position(tokens: &[Token], table_idx: usize) -> bool {
    let mut prev_idx = table_idx;
    loop {
        if prev_idx == 0 {
            return true;
        }
        prev_idx -= 1;
        if !matches!(
            tokens[prev_idx].kind,
            TokenKind::Whitespace | TokenKind::Comment
        ) {
            break;
        }
    }

    let prev = &tokens[prev_idx];

    // Statement separator, subquery open paren, or CTE close paren.
    if matches!(prev.text.as_str(), ";" | "(" | ")") {
        return true;
    }

    if prev.kind == TokenKind::Word {
        let upper = prev.text.to_uppercase();

        // Set operations: UNION TABLE, INTERSECT TABLE, EXCEPT TABLE.
        if matches!(upper.as_str(), "UNION" | "INTERSECT" | "EXCEPT") {
            return true;
        }

        // Set operations with ALL or DISTINCT: UNION ALL TABLE, EXCEPT DISTINCT TABLE.
        if matches!(upper.as_str(), "ALL" | "DISTINCT") {
            let pp = prev_non_ws(tokens, prev_idx);
            if let Some(pp) = pp {
                if tokens[pp].kind == TokenKind::Word {
                    let pp_upper = tokens[pp].text.to_uppercase();
                    return matches!(pp_upper.as_str(), "UNION" | "INTERSECT" | "EXCEPT");
                }
            }
        }

        // EXPLAIN context: EXPLAIN TABLE, EXPLAIN ANALYZE TABLE,
        // and after preprocess_explain normalization, e.g.
        // EXPLAIN FORMAT TEXT TABLE (was EXPLAIN (FORMAT TEXT) TABLE).
        if is_in_explain_context(tokens, prev_idx) {
            return true;
        }
    }

    false
}

/// Walk backward from `from_idx` through known EXPLAIN option keywords/values
/// to determine if we are inside an EXPLAIN statement.
fn is_in_explain_context(tokens: &[Token], from_idx: usize) -> bool {
    let mut idx = from_idx;
    loop {
        if tokens[idx].kind != TokenKind::Word {
            return false;
        }
        let upper = tokens[idx].text.to_uppercase();
        if upper == "EXPLAIN" {
            return true;
        }
        // Known EXPLAIN option keywords and their values (post preprocess_explain).
        if !matches!(
            upper.as_str(),
            "ANALYZE"
                | "VERBOSE"
                | "FORMAT"
                | "COSTS"
                | "BUFFERS"
                | "TIMING"
                | "SUMMARY"
                | "SETTINGS"
                | "WAL"
                | "TEXT"
                | "JSON"
                | "YAML"
                | "XML"
                | "ON"
                | "OFF"
                | "TRUE"
                | "FALSE"
        ) {
            return false;
        }
        match prev_non_ws(tokens, idx) {
            Some(prev) => idx = prev,
            None => return false,
        }
    }
}

/// Skip whitespace/comments backward from `idx - 1`, returning the index
/// of the previous non-trivial token, or `None` if none exists.
fn prev_non_ws(tokens: &[Token], idx: usize) -> Option<usize> {
    let mut i = idx;
    loop {
        if i == 0 {
            return None;
        }
        i -= 1;
        if !matches!(tokens[i].kind, TokenKind::Whitespace | TokenKind::Comment) {
            return Some(i);
        }
    }
}
