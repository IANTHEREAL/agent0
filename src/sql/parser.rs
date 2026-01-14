//! SQL parser wrapper using sqlparser-rs

use anyhow::{anyhow, Result};
use sqlparser::ast::Statement;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

/// Parse a SQL string into AST statements
pub fn parse_sql(sql: &str) -> Result<Vec<Statement>> {
    let dialect = PostgreSqlDialect {};
    Parser::parse_sql(&dialect, sql).map_err(|e| anyhow!("SQL parse error: {}", e))
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
}

#[cfg(test)]
mod parser_tests {
    use super::*;

    #[test]
    fn test_default_in_on_conflict() {
        let sql = r#"INSERT INTO t(a, b) VALUES (1, 2) ON CONFLICT (a) DO UPDATE SET b = DEFAULT"#;
        let statements = parse_sql(sql).unwrap();
        println!("Parsed: {:#?}", statements);
    }
}
