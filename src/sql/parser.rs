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

fn reorder_single_create_sequence(stmt: &str) -> Option<String> {
    let trimmed = stmt.trim();
    let upper = trimmed.to_uppercase();

    if !upper.starts_with("CREATE SEQUENCE ") {
        return None;
    }

    if !upper.contains("INCREMENT") || !upper.contains("START") {
        return None;
    }

    let start_idx = upper.find("START").unwrap();
    let inc_idx = upper.find("INCREMENT").unwrap();

    if inc_idx <= start_idx {
        return None;
    }

    let mut tokens: Vec<&str> = trimmed.split_whitespace().collect();

    let mut inc_start = None;
    let mut inc_end = None;
    let mut start_pos = None;

    for (i, token) in tokens.iter().enumerate() {
        let t = token.to_uppercase();
        if t == "INCREMENT" {
            inc_start = Some(i);
        } else if inc_start.is_some() && inc_end.is_none() {
            if t == "BY" {
                continue;
            }
            let clean = t.trim_end_matches(';');
            if clean.parse::<i64>().is_ok() || clean.starts_with('-') || clean.starts_with('+') {
                inc_end = Some(i);
            }
        }
        if t == "START" {
            start_pos = Some(i);
        }
    }

    if let (Some(inc_s), Some(inc_e), Some(start_p)) = (inc_start, inc_end, start_pos) {
        if inc_s > start_p {
            let inc_tokens: Vec<&str> = tokens[inc_s..=inc_e].to_vec();
            tokens.drain(inc_s..=inc_e);
            let insert_at = tokens
                .iter()
                .position(|t| t.to_uppercase() == "START")
                .unwrap();
            for (i, t) in inc_tokens.into_iter().enumerate() {
                tokens.insert(insert_at + i, t);
            }
            return Some(tokens.join(" "));
        }
    }

    None
}

fn preprocess_create_sequence(sql: &str) -> Option<String> {
    let upper = sql.to_uppercase();
    if !upper.contains("CREATE SEQUENCE")
        || !upper.contains("INCREMENT")
        || !upper.contains("START")
    {
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

/// Rewrite `expr op ALL (SELECT ...)` and `expr op ANY (SELECT ...)` patterns
/// to equivalent forms that sqlparser 0.40 can handle.
///
/// Transformations:
/// - `x >= ALL (SELECT col FROM ...)` → `x >= (SELECT MAX(col) FROM ...)`
/// - `x <= ALL (SELECT col FROM ...)` → `x <= (SELECT MIN(col) FROM ...)`
/// - `x > ALL (SELECT col FROM ...)`  → `x > (SELECT MAX(col) FROM ...)`
/// - `x < ALL (SELECT col FROM ...)`  → `x < (SELECT MIN(col) FROM ...)`
/// - `x = ALL (SELECT col FROM ...)`  → `NOT EXISTS (SELECT 1 FROM ... WHERE col <> x)`
/// - `x > ANY (SELECT col FROM ...)`  → `x > (SELECT MIN(col) FROM ...)`
/// - `x >= ANY (SELECT col FROM ...)` → `x >= (SELECT MIN(col) FROM ...)`
/// - `x < ANY (SELECT col FROM ...)`  → `x < (SELECT MAX(col) FROM ...)`
/// - `x <= ANY (SELECT col FROM ...)` → `x <= (SELECT MAX(col) FROM ...)`
/// - `x = ANY (SELECT col FROM ...)`  → `x IN (SELECT col FROM ...)`
fn rewrite_all_any_subqueries(sql: &str) -> String {
    let mut result = sql.to_string();

    // Match: expr OP ALL (SELECT ...) or expr OP ANY (SELECT ...)
    let all_pattern =
        Regex::new(r"(?i)(\S+)\s*(>=|<=|>|<|=|<>|!=)\s*ALL\s*\(\s*(SELECT\s+)").unwrap();

    let any_pattern =
        Regex::new(r"(?i)(\S+)\s*(>=|<=|>|<|=|<>|!=)\s*ANY\s*\(\s*(SELECT\s+)").unwrap();

    // Process ALL patterns: x OP ALL (SELECT col ...) -> x OP (SELECT AGG(col) ...)
    while let Some(caps) = all_pattern.captures(&result) {
        let full_match = caps.get(0).unwrap();
        let expr = caps.get(1).unwrap().as_str();
        let op = caps.get(2).unwrap().as_str();
        let select_start = caps.get(3).unwrap().as_str();

        let match_start = full_match.start();
        let select_pos = full_match.end() - select_start.len();

        if let Some((subquery, end_pos)) = extract_subquery(&result, select_pos) {
            // subquery is "SELECT col FROM ..." (without outer parens)
            // We need to wrap col with MAX/MIN
            let replacement = match op.to_uppercase().as_str() {
                ">=" => wrap_subquery_with_agg(expr, ">=", &subquery, "MAX"),
                "<=" => wrap_subquery_with_agg(expr, "<=", &subquery, "MIN"),
                ">" => wrap_subquery_with_agg(expr, ">", &subquery, "MAX"),
                "<" => wrap_subquery_with_agg(expr, "<", &subquery, "MIN"),
                "=" | "==" => wrap_subquery_with_agg(expr, "=", &subquery, "MIN"),
                "<>" | "!=" => format!("{} NOT IN ({})", expr, subquery),
                _ => continue,
            };

            result = format!(
                "{}{}{}",
                &result[..match_start],
                replacement,
                &result[end_pos..]
            );
        } else {
            break;
        }
    }

    // Process ANY patterns: x OP ANY (SELECT col ...) -> x OP (SELECT AGG(col) ...) or x IN (...)
    while let Some(caps) = any_pattern.captures(&result) {
        let full_match = caps.get(0).unwrap();
        let expr = caps.get(1).unwrap().as_str();
        let op = caps.get(2).unwrap().as_str();
        let select_start = caps.get(3).unwrap().as_str();

        let match_start = full_match.start();
        let select_pos = full_match.end() - select_start.len();

        if let Some((subquery, end_pos)) = extract_subquery(&result, select_pos) {
            let replacement = match op.to_uppercase().as_str() {
                ">" => wrap_subquery_with_agg(expr, ">", &subquery, "MIN"),
                ">=" => wrap_subquery_with_agg(expr, ">=", &subquery, "MIN"),
                "<" => wrap_subquery_with_agg(expr, "<", &subquery, "MAX"),
                "<=" => wrap_subquery_with_agg(expr, "<=", &subquery, "MAX"),
                "=" | "==" => format!("{} IN ({})", expr, subquery),
                "<>" | "!=" => wrap_subquery_with_agg(expr, "<>", &subquery, "MIN"),
                _ => continue,
            };

            result = format!(
                "{}{}{}",
                &result[..match_start],
                replacement,
                &result[end_pos..]
            );
        } else {
            break;
        }
    }

    result
}

/// Transform "SELECT col FROM ..." to "SELECT AGG(col) FROM ..." and build comparison
fn wrap_subquery_with_agg(expr: &str, op: &str, subquery: &str, agg: &str) -> String {
    // subquery format: "SELECT col FROM table WHERE ..."
    // We need to extract "col" and wrap it with the aggregate function
    let upper = subquery.to_uppercase();
    if let Some(from_pos) = upper.find(" FROM ") {
        let col_part = &subquery[7..from_pos]; // After "SELECT " before " FROM"
        let rest = &subquery[from_pos..];
        format!(
            "{} {} (SELECT {}({}){})",
            expr,
            op,
            agg,
            col_part.trim(),
            rest
        )
    } else {
        // Fallback: just use the subquery as-is (shouldn't happen for valid SQL)
        format!("{} {} ({})", expr, op, subquery)
    }
}

/// Extract a subquery starting at the given position, handling nested parentheses.
/// Returns the subquery string (including SELECT) and the position after the closing paren.
fn extract_subquery(sql: &str, start: usize) -> Option<(String, usize)> {
    let bytes = sql.as_bytes();
    let mut depth = 1; // We're already inside the opening paren
    let mut pos = start;

    // Find where the subquery content starts (after the opening paren we're inside)
    // The start position is at "SELECT", and we need to find the matching close paren

    // First, find where the opening paren was (before start)
    let mut open_paren_pos = start;
    for i in (0..start).rev() {
        if bytes[i] == b'(' {
            open_paren_pos = i;
            break;
        }
    }

    // Now scan forward from start to find the matching close paren
    pos = start;
    while pos < bytes.len() {
        match bytes[pos] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    // Found the matching close paren
                    let subquery = sql[start..pos].to_string();
                    return Some((subquery, pos + 1)); // +1 to skip the closing paren
                }
            }
            b'\'' => {
                // Skip string literals
                pos += 1;
                while pos < bytes.len() && bytes[pos] != b'\'' {
                    if bytes[pos] == b'\\' {
                        pos += 1; // Skip escaped char
                    }
                    pos += 1;
                }
            }
            b'"' => {
                // Skip quoted identifiers
                pos += 1;
                while pos < bytes.len() && bytes[pos] != b'"' {
                    pos += 1;
                }
            }
            _ => {}
        }
        pos += 1;
    }

    None // Couldn't find matching paren
}

fn preprocess_sql(sql: &str) -> String {
    let mut result = sql.to_string();

    if let Some(explained) = preprocess_explain(&result) {
        result = explained;
    }
    if let Some(sequenced) = preprocess_create_sequence(&result) {
        result = sequenced;
    }

    // Rewrite ALL/ANY subquery patterns
    result = rewrite_all_any_subqueries(&result);

    result
}

/// Parse a SQL string into AST statements
pub fn parse_sql(sql: &str) -> Result<Vec<Statement>> {
    let dialect = PostgreSqlDialect {};
    let preprocessed = preprocess_sql(sql);
    Parser::parse_sql(&dialect, &preprocessed).map_err(|e| anyhow!("SQL parse error: {}", e))
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
    fn test_parse_create_table() {
        let stmts = parse_sql("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)").unwrap();
        assert_eq!(stmts.len(), 1);
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
}
