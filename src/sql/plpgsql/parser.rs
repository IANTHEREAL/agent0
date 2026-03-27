//! PL/pgSQL body parsing: DECLARE block, BEGIN block, statement parsing,
//! SELECT INTO, FOR loops, IF/ELSIF/ELSE blocks.

use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};

use super::utils::{parse_plpgsql_type, plpgsql_outer_block_range_strict};
use crate::model::DataType;

/// Represents a single PL/pgSQL statement.
#[derive(Debug)]
pub(super) enum PlpgsqlStatement {
    Return(String),
    Assignment(String, String),
    If(String, Vec<PlpgsqlStatement>, Vec<PlpgsqlStatement>),
    RaiseNotice(String),
    RaiseException(String),
    Sql(String),
    Perform(String),
    /// INSERT/UPDATE/DELETE ... RETURNING expr_list INTO var_list
    DmlReturningInto {
        /// The DML SQL with RETURNING clause but without the INTO var_list part
        sql: String,
        /// Target PL/pgSQL variables to receive the RETURNING values
        variables: Vec<String>,
    },
    SelectInto {
        variables: Vec<String>,
        query: String,
        strict: bool,
    },
    ForQuery {
        variable: String,
        query: String,
        body: Vec<PlpgsqlStatement>,
    },
    ForRange {
        variable: String,
        start_expr: String,
        end_expr: String,
        step_expr: Option<String>,
        reverse: bool,
        body: Vec<PlpgsqlStatement>,
    },
    Exit,
    Null,
}

/// Parse the DECLARE block, returning variable defaults, types, and the remaining body
/// starting from BEGIN.
#[allow(clippy::type_complexity)]
pub(super) fn parse_declare_block(
    body: &str,
) -> Result<(
    Vec<(String, Option<String>)>,
    HashMap<String, DataType>,
    &str,
)> {
    let body_bytes = body.as_bytes();
    let mut var_defaults: Vec<(String, Option<String>)> = Vec::new();
    let mut types = HashMap::new();

    let declare_pos = find_ascii_keyword(body_bytes, b"DECLARE");
    let begin_pos = find_ascii_keyword(body_bytes, b"BEGIN")
        .ok_or_else(|| anyhow!("PL/pgSQL function must have BEGIN block"))?;

    let rest = if let Some(decl_pos) = declare_pos {
        if decl_pos < begin_pos {
            let decl_section = &body[decl_pos + 7..begin_pos];
            for line in decl_section.lines() {
                let line = line.trim().trim_end_matches(';');
                if line.is_empty() {
                    continue;
                }
                let parts: Vec<&str> = line.splitn(2, char::is_whitespace).collect();
                if parts.len() >= 2 {
                    let var_name = parts[0].trim().to_lowercase();
                    let rest = parts[1].trim();

                    // Check := first (unambiguous), then DEFAULT with word-boundary
                    // awareness to avoid matching inside string literals like 'default'.
                    let (type_str, default_expr) = if let Some(assign_pos) = rest.find(":=") {
                        (
                            rest[..assign_pos].trim(),
                            Some(rest[assign_pos + 2..].trim().to_string()),
                        )
                    } else if let Some(def_pos) =
                        find_word_boundary_keyword(rest.as_bytes(), b"DEFAULT")
                    {
                        (
                            rest[..def_pos].trim(),
                            Some(rest[def_pos + 7..].trim().to_string()),
                        )
                    } else {
                        (rest, None)
                    };

                    let data_type = parse_plpgsql_type(type_str);
                    types.insert(var_name.clone(), data_type);
                    var_defaults.push((var_name, default_expr));
                }
            }
        }
        &body[begin_pos..]
    } else {
        &body[begin_pos..]
    };

    Ok((var_defaults, types, rest))
}

/// Parse the BEGIN..END block, returning a list of statements.
pub(super) fn parse_begin_block(
    body: &str,
    declared_vars: &HashSet<String>,
) -> Result<Vec<PlpgsqlStatement>> {
    let (block_start, end_pos) = plpgsql_outer_block_range_strict(body)
        .ok_or_else(|| anyhow!("Missing END for BEGIN block"))?;

    let block_content = &body[block_start..end_pos];
    parse_statements(block_content, declared_vars)
}

pub(super) fn parse_statements(
    content: &str,
    declared_vars: &HashSet<String>,
) -> Result<Vec<PlpgsqlStatement>> {
    let mut statements = Vec::new();
    let content = content.trim();

    if content.is_empty() {
        return Ok(statements);
    }

    let mut remaining = content;
    while !remaining.trim().is_empty() {
        remaining = remaining.trim();

        let rem_bytes = remaining.as_bytes();
        if ascii_keyword_at(rem_bytes, 0, b"IF ") || ascii_keyword_at(rem_bytes, 0, b"IF\n") {
            let (if_stmt, rest) = parse_if_statement(remaining, declared_vars)?;
            statements.push(if_stmt);
            remaining = rest;
            continue;
        }

        if ascii_keyword_at(rem_bytes, 0, b"ELSIF ") || ascii_keyword_at(rem_bytes, 0, b"ELSIF\n") {
            let synthetic_if = format!("IF{} END IF", &remaining[5..]);
            let (if_stmt, _rest) = parse_if_statement(&synthetic_if, declared_vars)?;
            statements.push(if_stmt);
            break;
        }

        if ascii_keyword_at(rem_bytes, 0, b"FOR ") || ascii_keyword_at(rem_bytes, 0, b"FOR\n") {
            let (for_stmt, rest) = parse_for_statement(remaining, declared_vars)?;
            statements.push(for_stmt);
            remaining = rest;
            continue;
        }

        if let Some(semi_pos) = find_statement_end(remaining) {
            let stmt_str = remaining[..semi_pos].trim();
            if !stmt_str.is_empty() {
                statements.push(parse_single_statement(stmt_str, declared_vars)?);
            }
            remaining = &remaining[semi_pos + 1..];
        } else {
            if !remaining.is_empty() {
                statements.push(parse_single_statement(remaining, declared_vars)?);
            }
            break;
        }
    }

    Ok(statements)
}

fn find_statement_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut in_string = false;
    let mut i = 0;

    while i < bytes.len() {
        if in_string {
            if bytes[i] == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_string = false;
            }
        } else {
            match bytes[i] {
                b'\'' => in_string = true,
                b';' => return Some(i),
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn parse_single_statement(s: &str, declared_vars: &HashSet<String>) -> Result<PlpgsqlStatement> {
    let s = s.trim();
    let s_bytes = s.as_bytes();

    if ascii_keyword_at(s_bytes, 0, b"NULL") && s_bytes.len() == 4 {
        return Ok(PlpgsqlStatement::Null);
    }

    if ascii_keyword_at(s_bytes, 0, b"RETURN ")
        || (ascii_keyword_at(s_bytes, 0, b"RETURN") && s_bytes.len() == 6)
    {
        let expr = if s.len() > 7 { s[7..].trim() } else { "" };
        return Ok(PlpgsqlStatement::Return(expr.to_string()));
    }

    if ascii_keyword_at(s_bytes, 0, b"CALL ") {
        return Err(anyhow!("CALL is not allowed in a function"));
    }

    if ascii_keyword_at(s_bytes, 0, b"RAISE ") {
        let rest = s[6..].trim();
        let rest_bytes = rest.as_bytes();
        if ascii_keyword_at(rest_bytes, 0, b"NOTICE ") {
            return Ok(PlpgsqlStatement::RaiseNotice(rest[7..].trim().to_string()));
        }
        if ascii_keyword_at(rest_bytes, 0, b"EXCEPTION ") {
            return Ok(PlpgsqlStatement::RaiseException(
                rest[10..].trim().to_string(),
            ));
        }
        return Ok(PlpgsqlStatement::RaiseException(rest.to_string()));
    }

    if (ascii_keyword_at(s_bytes, 0, b"EXIT") && s_bytes.len() == 4)
        || ascii_keyword_at(s_bytes, 0, b"EXIT ")
    {
        return Ok(PlpgsqlStatement::Exit);
    }

    if ascii_keyword_at(s_bytes, 0, b"PERFORM ") {
        let query = s[8..].trim();
        return Ok(PlpgsqlStatement::Perform(query.to_string()));
    }

    if ascii_keyword_at(s_bytes, 0, b"SELECT ") {
        if let Some(stmt) = parse_select_into_statement(s, declared_vars)? {
            return Ok(stmt);
        }
    }

    if ascii_keyword_at(s_bytes, 0, b"INSERT ")
        || ascii_keyword_at(s_bytes, 0, b"UPDATE ")
        || ascii_keyword_at(s_bytes, 0, b"DELETE ")
    {
        // Check for RETURNING ... INTO var_list pattern
        if let Some(stmt) = parse_dml_returning_into(s, declared_vars)? {
            return Ok(stmt);
        }
        return Ok(PlpgsqlStatement::Sql(s.to_string()));
    }

    if ascii_keyword_at(s_bytes, 0, b"CREATE ")
        || ascii_keyword_at(s_bytes, 0, b"DROP ")
        || ascii_keyword_at(s_bytes, 0, b"ALTER ")
        || ascii_keyword_at(s_bytes, 0, b"TRUNCATE ")
    {
        return Ok(PlpgsqlStatement::Sql(s.to_string()));
    }

    if let Some(assign_pos) = s.find(":=") {
        let var_name = s[..assign_pos].trim().to_string();
        let expr = s[assign_pos + 2..].trim().to_string();
        return Ok(PlpgsqlStatement::Assignment(var_name, expr));
    }

    Ok(PlpgsqlStatement::Sql(s.to_string()))
}

/// Parse `INSERT/UPDATE/DELETE ... RETURNING expr_list INTO var_list`.
///
/// Scans backwards from the end of the statement to find the last `INTO` keyword
/// that appears after a `RETURNING` keyword. The portion between RETURNING and INTO
/// is the expression list, and everything after INTO is the variable list.
fn parse_dml_returning_into(
    s: &str,
    declared_vars: &HashSet<String>,
) -> Result<Option<PlpgsqlStatement>> {
    let s_bytes = s.as_bytes();

    // Find the last RETURNING keyword (case-insensitive, word-boundary).
    let mut returning_pos = None;
    let returning_kw = b"RETURNING";
    let kw_len = returning_kw.len();
    if s_bytes.len() >= kw_len {
        let mut i = s_bytes.len() - kw_len;
        loop {
            if ascii_keyword_at(s_bytes, i, returning_kw)
                && (i == 0 || !s_bytes[i - 1].is_ascii_alphanumeric())
                && (i + kw_len == s_bytes.len() || !s_bytes[i + kw_len].is_ascii_alphanumeric())
            {
                returning_pos = Some(i);
                break;
            }
            if i == 0 {
                break;
            }
            i -= 1;
        }
    }

    let returning_pos = match returning_pos {
        Some(p) => p,
        None => return Ok(None),
    };

    // After RETURNING, look for INTO keyword
    let after_returning = &s[returning_pos + kw_len..];
    let after_bytes = after_returning.as_bytes();
    let into_pos = find_ascii_keyword(after_bytes, b"INTO");

    let into_pos = match into_pos {
        Some(p) => p,
        None => return Ok(None),
    };

    // Verify INTO is at a word boundary
    let abs_into = returning_pos + kw_len + into_pos;
    if abs_into > 0 && s_bytes[abs_into - 1].is_ascii_alphanumeric() {
        return Ok(None);
    }
    if abs_into + 4 < s_bytes.len() && s_bytes[abs_into + 4].is_ascii_alphanumeric() {
        return Ok(None);
    }

    // The variable list is after INTO
    let var_str = after_returning[into_pos + 4..].trim();
    if var_str.is_empty() {
        return Ok(None);
    }

    let variables: Vec<String> = var_str
        .split(',')
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect();

    if variables.is_empty() {
        return Ok(None);
    }

    // Verify at least one target is a declared variable (avoid false positives
    // where INTO is part of an INSERT INTO subquery in the RETURNING clause).
    let any_declared = variables
        .iter()
        .any(|v| declared_vars.contains(&v.to_lowercase()));
    if !any_declared {
        return Ok(None);
    }

    // The DML SQL is everything up to and including the RETURNING expr_list,
    // but excluding the INTO var_list part.
    let sql = s[..abs_into].trim().to_string();

    Ok(Some(PlpgsqlStatement::DmlReturningInto { sql, variables }))
}

fn parse_select_into_statement(
    s: &str,
    declared_vars: &HashSet<String>,
) -> Result<Option<PlpgsqlStatement>> {
    let s_bytes = s.as_bytes();
    let Some(select_pos) = find_ascii_keyword(s_bytes, b"SELECT") else {
        return Ok(None);
    };
    let Some(into_pos) = find_ascii_keyword(s_bytes, b"INTO") else {
        return Ok(None);
    };
    if into_pos <= select_pos {
        return Ok(None);
    }

    let from_pos = find_ascii_keyword(s_bytes, b"FROM");
    if let Some(fp) = from_pos {
        if into_pos >= fp {
            return Ok(None);
        }
    }

    let projection = s[select_pos + 6..into_pos].trim().to_string();
    let mut var_part = if let Some(fp) = from_pos {
        s[into_pos + 4..fp].trim()
    } else {
        s[into_pos + 4..].trim()
    };

    let strict = ascii_keyword_at(var_part.as_bytes(), 0, b"STRICT");
    if strict {
        var_part = var_part[6..].trim();
    }

    // Form 2: SELECT INTO var1[, var2, ...] expr1[, expr2, ...] FROM ...
    // When projection is empty (INTO immediately after SELECT), use declared
    // variable names to split `var_part` into target variables vs. projection.
    if projection.is_empty() && !declared_vars.is_empty() {
        let (vars, remaining_proj) = split_into_targets(var_part, declared_vars);
        if !vars.is_empty() {
            let variables = vars;
            if remaining_proj.is_empty() && from_pos.is_none() {
                return Err(anyhow!("SELECT INTO requires a SELECT expression"));
            }
            let query = if remaining_proj.is_empty() {
                format!("SELECT * {}", s[from_pos.unwrap()..].trim())
            } else if let Some(fp) = from_pos {
                format!("SELECT {} {}", remaining_proj, s[fp..].trim())
            } else {
                format!("SELECT {}", remaining_proj)
            };
            return Ok(Some(PlpgsqlStatement::SelectInto {
                variables,
                query,
                strict,
            }));
        }
    }

    if projection.is_empty() {
        return Ok(None);
    }

    let variables: Vec<String> = var_part
        .split(',')
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .collect();

    if variables.is_empty() {
        return Ok(None);
    }

    let query = if let Some(fp) = from_pos {
        format!("SELECT {} {}", projection, s[fp..].trim())
    } else {
        format!("SELECT {}", projection)
    };

    Ok(Some(PlpgsqlStatement::SelectInto {
        variables,
        query,
        strict,
    }))
}

fn split_into_targets(text: &str, declared_vars: &HashSet<String>) -> (Vec<String>, String) {
    let mut vars = Vec::new();
    let mut pos = 0;
    let bytes = text.as_bytes();
    let len = text.len();

    loop {
        while pos < len && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= len {
            break;
        }

        let word_start = pos;
        while pos < len && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'_') {
            pos += 1;
        }
        let word = &text[word_start..pos];

        if word.is_empty() || !declared_vars.contains(&word.to_lowercase()) {
            pos = word_start;
            break;
        }

        vars.push(word.to_string());

        while pos < len && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos < len && bytes[pos] == b',' {
            pos += 1;
        } else {
            break;
        }
    }

    let remaining = text[pos..].trim().to_string();
    (vars, remaining)
}

fn parse_for_statement<'a>(
    s: &'a str,
    declared_vars: &HashSet<String>,
) -> Result<(PlpgsqlStatement, &'a str)> {
    let bytes = s.as_bytes();

    let mut loop_pos = None;
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if ascii_keyword_at(bytes, i, b"LOOP")
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            && (i + 4 == bytes.len() || !bytes[i + 4].is_ascii_alphanumeric())
        {
            loop_pos = Some(i);
            break;
        }
        i += 1;
    }
    let loop_pos = loop_pos.ok_or_else(|| anyhow!("FOR without LOOP"))?;

    let header_raw = s[3..loop_pos].trim();
    let header_norm: String = header_raw
        .chars()
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect();
    let in_pos = find_ascii_keyword(header_norm.as_bytes(), b" IN ")
        .ok_or_else(|| anyhow!("FOR without IN"))?;
    let variable = header_norm[..in_pos].trim().to_string();
    let mut in_expr = header_norm[in_pos + 4..].trim().to_string();

    let mut reverse = false;
    if ascii_keyword_at(in_expr.as_bytes(), 0, b"REVERSE ") {
        reverse = true;
        in_expr = in_expr[8..].trim().to_string();
    }

    let mut depth = 1;
    let mut end_loop_pos = None;
    i = loop_pos + 4;
    while i < bytes.len() {
        if ascii_keyword_at(bytes, i, b"END LOOP")
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
        {
            depth -= 1;
            if depth == 0 {
                end_loop_pos = Some(i);
                break;
            }
            i += 8;
            continue;
        }

        if ascii_keyword_at(bytes, i, b"LOOP")
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            && (i + 4 == bytes.len() || !bytes[i + 4].is_ascii_alphanumeric())
        {
            depth += 1;
            i += 4;
            continue;
        }

        i += 1;
    }

    let end_loop_pos = end_loop_pos.ok_or_else(|| anyhow!("FOR without END LOOP"))?;
    let body_str = s[loop_pos + 4..end_loop_pos].trim();
    let body = parse_statements(body_str, declared_vars)?;

    let rest_start = end_loop_pos + 8;
    let rest = s[rest_start..].trim_start();
    let rest = if let Some(stripped) = rest.strip_prefix(';') {
        stripped
    } else {
        rest
    };

    if let Some(range_pos) = in_expr.find("..") {
        let start_expr = in_expr[..range_pos].trim().to_string();
        let end_and_by = in_expr[range_pos + 2..].trim();
        let (end_expr, step_expr) =
            if let Some(by_pos) = find_ascii_keyword(end_and_by.as_bytes(), b" BY ") {
                (
                    end_and_by[..by_pos].trim().to_string(),
                    Some(end_and_by[by_pos + 4..].trim().to_string()),
                )
            } else {
                (end_and_by.to_string(), None)
            };

        return Ok((
            PlpgsqlStatement::ForRange {
                variable,
                start_expr,
                end_expr,
                step_expr,
                reverse,
                body,
            },
            rest,
        ));
    }

    Ok((
        PlpgsqlStatement::ForQuery {
            variable,
            query: in_expr,
            body,
        },
        rest,
    ))
}

fn parse_if_statement<'a>(
    s: &'a str,
    declared_vars: &HashSet<String>,
) -> Result<(PlpgsqlStatement, &'a str)> {
    let s_bytes = s.as_bytes();
    let then_pos = find_ascii_keyword(s_bytes, b" THEN")
        .or_else(|| find_ascii_keyword(s_bytes, b"\nTHEN"))
        .ok_or_else(|| anyhow!("IF without THEN"))?;

    let condition = s[2..then_pos].trim().to_string();
    let after_then = &s[then_pos + 5..];

    let (then_block, else_block, rest) = find_if_blocks(after_then)?;

    let then_stmts = parse_statements(then_block, declared_vars)?;
    let else_stmts = if else_block.is_empty() {
        Vec::new()
    } else {
        parse_statements(else_block, declared_vars)?
    };

    Ok((
        PlpgsqlStatement::If(condition, then_stmts, else_stmts),
        rest,
    ))
}

enum ElseBranchType {
    Elsif(usize),
    Else(usize),
}

fn find_if_blocks(s: &str) -> Result<(&str, &str, &str)> {
    let mut depth = 1;
    let mut i = 0;
    let bytes = s.as_bytes();
    let mut else_branch: Option<ElseBranchType> = None;
    let mut end_if_pos: Option<usize> = None;

    while i < bytes.len() {
        if i + 3 <= bytes.len()
            && ascii_keyword_at(bytes, i, b"IF")
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            && !bytes[i + 2].is_ascii_alphanumeric()
        {
            depth += 1;
            i += 2;
            continue;
        }

        if depth == 1
            && i + 5 <= bytes.len()
            && ascii_keyword_at(bytes, i, b"ELSIF")
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
        {
            if else_branch.is_none() {
                else_branch = Some(ElseBranchType::Elsif(i));
            }
            i += 5;
            continue;
        }

        if depth == 1
            && i + 4 <= bytes.len()
            && ascii_keyword_at(bytes, i, b"ELSE")
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            && (i + 4 == bytes.len() || !bytes[i + 4].is_ascii_alphanumeric())
        {
            if else_branch.is_none() {
                else_branch = Some(ElseBranchType::Else(i));
            }
            i += 4;
            continue;
        }

        if ascii_keyword_at(bytes, i, b"END IF") {
            if i == 0 || !bytes[i - 1].is_ascii_alphanumeric() {
                depth -= 1;
                if depth == 0 {
                    end_if_pos = Some(i);
                    break;
                }
            }
            i += 6;
            continue;
        }

        i += 1;
    }

    let end_if_pos = end_if_pos.ok_or_else(|| anyhow!("IF without END IF"))?;

    let (then_block, else_block) = match else_branch {
        Some(ElseBranchType::Elsif(pos)) => {
            let then_block = &s[..pos];
            let elsif_rest = &s[pos..end_if_pos];
            (then_block, elsif_rest)
        }
        Some(ElseBranchType::Else(pos)) => (&s[..pos], &s[pos + 4..end_if_pos]),
        None => (&s[..end_if_pos], ""),
    };

    let rest_start = end_if_pos + 6;
    let rest = s[rest_start..].trim_start();
    let rest = rest.strip_prefix(';').unwrap_or(rest);

    Ok((then_block.trim(), else_block.trim(), rest))
}

fn ascii_keyword_at(bytes: &[u8], pos: usize, keyword: &[u8]) -> bool {
    if pos + keyword.len() > bytes.len() {
        return false;
    }
    bytes[pos..pos + keyword.len()]
        .iter()
        .zip(keyword.iter())
        .all(|(a, b)| a.to_ascii_uppercase() == *b)
}

/// Like `find_ascii_keyword` but requires word boundaries: the character before
/// must be whitespace (or start-of-string) and the character after must be
/// whitespace (or end-of-string). This prevents matching keywords inside string
/// literals like 'default'.
fn find_word_boundary_keyword(bytes: &[u8], keyword: &[u8]) -> Option<usize> {
    if keyword.is_empty() || keyword.len() > bytes.len() {
        return None;
    }
    (0..=bytes.len() - keyword.len()).find(|&i| {
        ascii_keyword_at(bytes, i, keyword)
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            && (i + keyword.len() == bytes.len()
                || !bytes[i + keyword.len()].is_ascii_alphanumeric())
    })
}

fn find_ascii_keyword(bytes: &[u8], keyword: &[u8]) -> Option<usize> {
    if keyword.is_empty() || keyword.len() > bytes.len() {
        return None;
    }
    (0..=bytes.len() - keyword.len()).find(|&i| ascii_keyword_at(bytes, i, keyword))
}
