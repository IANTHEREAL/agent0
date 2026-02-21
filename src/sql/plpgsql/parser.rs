//! PL/pgSQL body parsing: DECLARE block, BEGIN block, statement parsing,
//! SELECT INTO, FOR loops, IF/ELSIF/ELSE blocks.

use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};

use super::utils::parse_plpgsql_type;
use crate::types::DataType;

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
pub(super) fn parse_declare_block(
    body: &str,
) -> Result<(
    HashMap<String, Option<String>>,
    HashMap<String, DataType>,
    &str,
)> {
    let body_upper = body.to_uppercase();
    let mut var_defaults: HashMap<String, Option<String>> = HashMap::new();
    let mut types = HashMap::new();

    let declare_pos = body_upper.find("DECLARE");
    let begin_pos = body_upper
        .find("BEGIN")
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

                    let (type_str, default_expr) =
                        if let Some(def_pos) = rest.to_uppercase().find("DEFAULT") {
                            (
                                rest[..def_pos].trim(),
                                Some(rest[def_pos + 7..].trim().to_string()),
                            )
                        } else if let Some(assign_pos) = rest.find(":=") {
                            (
                                rest[..assign_pos].trim(),
                                Some(rest[assign_pos + 2..].trim().to_string()),
                            )
                        } else {
                            (rest, None)
                        };

                    let data_type = parse_plpgsql_type(type_str);
                    types.insert(var_name.clone(), data_type);
                    var_defaults.insert(var_name, default_expr);
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
    let body_upper = body.to_uppercase();
    let begin_pos = body_upper
        .find("BEGIN")
        .ok_or_else(|| anyhow!("Missing BEGIN"))?;
    let end_pos = find_matching_end(&body[begin_pos..])
        .ok_or_else(|| anyhow!("Missing END for BEGIN block"))?;

    let block_content = &body[begin_pos + 5..begin_pos + end_pos];
    parse_statements(block_content, declared_vars)
}

fn find_matching_end(s: &str) -> Option<usize> {
    let s_upper = s.to_uppercase();
    let mut depth = 0;
    let mut i = 0;
    let bytes = s_upper.as_bytes();

    while i < bytes.len() {
        if i + 5 <= bytes.len() && &s_upper[i..i + 5] == "BEGIN" {
            if i == 0 || !bytes[i - 1].is_ascii_alphanumeric() {
                if i + 5 == bytes.len() || !bytes[i + 5].is_ascii_alphanumeric() {
                    depth += 1;
                    i += 5;
                    continue;
                }
            }
        }
        if i + 2 <= bytes.len() && &s_upper[i..i + 2] == "IF" {
            if (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
                && (i + 2 == bytes.len() || !bytes[i + 2].is_ascii_alphanumeric())
            {
                depth += 1;
                i += 2;
                continue;
            }
        }
        if i + 8 <= bytes.len() && &s_upper[i..i + 8] == "END LOOP" {
            if i == 0 || !bytes[i - 1].is_ascii_alphanumeric() {
                depth -= 1;
                i += 8;
                continue;
            }
        }
        if i + 4 <= bytes.len() && &s_upper[i..i + 4] == "LOOP" {
            if (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
                && (i + 4 == bytes.len() || !bytes[i + 4].is_ascii_alphanumeric())
            {
                depth += 1;
                i += 4;
                continue;
            }
        }
        if i + 6 <= bytes.len() && &s_upper[i..i + 6] == "END IF" {
            if i == 0 || !bytes[i - 1].is_ascii_alphanumeric() {
                depth -= 1;
                i += 6;
                continue;
            }
        }
        if i + 3 <= bytes.len() && &s_upper[i..i + 3] == "END" {
            if (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
                && (i + 3 == bytes.len()
                    || !bytes[i + 3].is_ascii_alphanumeric()
                    || (i + 4 <= bytes.len() && bytes[i + 3] == b';'))
            {
                let rest = s_upper[i + 3..].trim_start();
                if !rest.starts_with("IF") {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    None
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

        let remaining_upper = remaining.to_uppercase();
        if remaining_upper.starts_with("IF ") || remaining_upper.starts_with("IF\n") {
            let (if_stmt, rest) = parse_if_statement(remaining, declared_vars)?;
            statements.push(if_stmt);
            remaining = rest;
            continue;
        }

        if remaining_upper.starts_with("ELSIF ") || remaining_upper.starts_with("ELSIF\n") {
            let synthetic_if = format!("IF{} END IF", &remaining[5..]);
            let (if_stmt, _rest) = parse_if_statement(&synthetic_if, declared_vars)?;
            statements.push(if_stmt);
            break;
        }

        if remaining_upper.starts_with("FOR ") || remaining_upper.starts_with("FOR\n") {
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
    let s_upper = s.to_uppercase();

    if s_upper == "NULL" {
        return Ok(PlpgsqlStatement::Null);
    }

    if s_upper.starts_with("RETURN ") || s_upper == "RETURN" {
        let expr = if s.len() > 7 { s[7..].trim() } else { "" };
        return Ok(PlpgsqlStatement::Return(expr.to_string()));
    }

    if s_upper.starts_with("CALL ") {
        return Err(anyhow!("CALL is not allowed in a function"));
    }

    if s_upper.starts_with("RAISE ") {
        let rest = s[6..].trim();
        let rest_upper = rest.to_uppercase();
        if rest_upper.starts_with("NOTICE ") {
            return Ok(PlpgsqlStatement::RaiseNotice(rest[7..].trim().to_string()));
        }
        if rest_upper.starts_with("EXCEPTION ") {
            return Ok(PlpgsqlStatement::RaiseException(
                rest[10..].trim().to_string(),
            ));
        }
        return Ok(PlpgsqlStatement::RaiseException(rest.to_string()));
    }

    if s_upper == "EXIT" || s_upper.starts_with("EXIT ") {
        return Ok(PlpgsqlStatement::Exit);
    }

    if s_upper.starts_with("PERFORM ") {
        let query = s[8..].trim();
        return Ok(PlpgsqlStatement::Perform(query.to_string()));
    }

    if s_upper.starts_with("SELECT ") {
        if let Some(stmt) = parse_select_into_statement(s, declared_vars)? {
            return Ok(stmt);
        }
    }

    if s_upper.starts_with("INSERT ")
        || s_upper.starts_with("UPDATE ")
        || s_upper.starts_with("DELETE ")
        || s_upper.starts_with("CREATE ")
        || s_upper.starts_with("DROP ")
        || s_upper.starts_with("ALTER ")
        || s_upper.starts_with("TRUNCATE ")
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

fn parse_select_into_statement(
    s: &str,
    declared_vars: &HashSet<String>,
) -> Result<Option<PlpgsqlStatement>> {
    let upper = s.to_uppercase();
    let Some(select_pos) = upper.find("SELECT") else {
        return Ok(None);
    };
    let Some(into_pos) = upper.find("INTO") else {
        return Ok(None);
    };
    if into_pos <= select_pos {
        return Ok(None);
    }

    let from_pos = upper.find("FROM");
    if let Some(fp) = from_pos {
        if into_pos >= fp {
            return Ok(None);
        }
    }

    let mut projection = s[select_pos + 6..into_pos].trim().to_string();
    let mut var_part = if let Some(fp) = from_pos {
        s[into_pos + 4..fp].trim()
    } else {
        s[into_pos + 4..].trim()
    };

    let strict = var_part.to_uppercase().starts_with("STRICT");
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
    let s_upper = s.to_uppercase();
    let bytes = s_upper.as_bytes();

    let mut loop_pos = None;
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if &s_upper[i..i + 4] == "LOOP"
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
    let in_pos = header_norm
        .to_uppercase()
        .find(" IN ")
        .ok_or_else(|| anyhow!("FOR without IN"))?;
    let variable = header_norm[..in_pos].trim().to_string();
    let mut in_expr = header_norm[in_pos + 4..].trim().to_string();

    let mut reverse = false;
    if in_expr.to_uppercase().starts_with("REVERSE ") {
        reverse = true;
        in_expr = in_expr[8..].trim().to_string();
    }

    let mut depth = 1;
    let mut end_loop_pos = None;
    i = loop_pos + 4;
    while i < bytes.len() {
        if i + 8 <= bytes.len() && &s_upper[i..i + 8] == "END LOOP" {
            if i == 0 || !bytes[i - 1].is_ascii_alphanumeric() {
                depth -= 1;
                if depth == 0 {
                    end_loop_pos = Some(i);
                    break;
                }
                i += 8;
                continue;
            }
        }

        if i + 4 <= bytes.len() && &s_upper[i..i + 4] == "LOOP" {
            if (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
                && (i + 4 == bytes.len() || !bytes[i + 4].is_ascii_alphanumeric())
            {
                depth += 1;
                i += 4;
                continue;
            }
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
        let end_and_by_upper = end_and_by.to_uppercase();
        let (end_expr, step_expr) = if let Some(by_pos) = end_and_by_upper.find(" BY ") {
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
    let s_upper = s.to_uppercase();

    let then_pos = s_upper
        .find(" THEN")
        .or_else(|| s_upper.find("\nTHEN"))
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
    let s_upper = s.to_uppercase();
    let mut depth = 1;
    let mut i = 0;
    let bytes = s_upper.as_bytes();
    let mut else_branch: Option<ElseBranchType> = None;
    let mut end_if_pos: Option<usize> = None;

    while i < bytes.len() {
        if i + 3 <= bytes.len()
            && &s_upper[i..i + 2] == "IF"
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            && !bytes[i + 2].is_ascii_alphanumeric()
        {
            depth += 1;
            i += 2;
            continue;
        }

        if depth == 1
            && i + 5 <= bytes.len()
            && &s_upper[i..i + 5] == "ELSIF"
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
            && &s_upper[i..i + 4] == "ELSE"
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
            && (i + 4 == bytes.len() || !bytes[i + 4].is_ascii_alphanumeric())
        {
            if else_branch.is_none() {
                else_branch = Some(ElseBranchType::Else(i));
            }
            i += 4;
            continue;
        }

        if i + 6 <= bytes.len() && &s_upper[i..i + 6] == "END IF" {
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
    let rest = if rest.starts_with(';') {
        &rest[1..]
    } else {
        rest
    };

    Ok((then_block.trim(), else_block.trim(), rest))
}
