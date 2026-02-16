use std::collections::HashSet;

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::Validator;
use rustyline::{Context, Helper, Result};

const SQL_KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "INSERT",
    "INSERT INTO",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "CREATE",
    "TABLE",
    "DROP",
    "ALTER",
    "ADD",
    "COLUMN",
    "INDEX",
    "VIEW",
    "SCHEMA",
    "DATABASE",
    "GRANT",
    "REVOKE",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "OUTER",
    "CROSS",
    "ON",
    "USING",
    "AS",
    "AND",
    "OR",
    "NOT",
    "IN",
    "EXISTS",
    "BETWEEN",
    "LIKE",
    "ILIKE",
    "IS",
    "NULL",
    "TRUE",
    "FALSE",
    "DISTINCT",
    "ALL",
    "ANY",
    "ORDER",
    "BY",
    "ASC",
    "DESC",
    "LIMIT",
    "OFFSET",
    "GROUP",
    "HAVING",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "CAST",
    "COALESCE",
    "NULLIF",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "SAVEPOINT",
    "TRANSACTION",
    "PRIMARY",
    "KEY",
    "FOREIGN",
    "REFERENCES",
    "UNIQUE",
    "CHECK",
    "DEFAULT",
    "CONSTRAINT",
    "SERIAL",
    "BIGSERIAL",
    "INTEGER",
    "BIGINT",
    "SMALLINT",
    "TEXT",
    "VARCHAR",
    "CHAR",
    "BOOLEAN",
    "TIMESTAMP",
    "DATE",
    "TIME",
    "INTERVAL",
    "NUMERIC",
    "DECIMAL",
    "REAL",
    "FLOAT",
    "DOUBLE",
    "PRECISION",
    "JSON",
    "JSONB",
    "UUID",
    "BYTEA",
    "ARRAY",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "CONCAT",
    "LENGTH",
    "LOWER",
    "UPPER",
    "TRIM",
    "SUBSTRING",
    "NOW",
    "CURRENT_TIMESTAMP",
    "CURRENT_DATE",
    "CURRENT_USER",
    "CURRENT_SCHEMA",
    "WITH",
    "RECURSIVE",
    "RETURNING",
    "CONFLICT",
    "DO",
    "NOTHING",
    "EXPLAIN",
    "ANALYZE",
    "VERBOSE",
    "TRUNCATE",
    "CASCADE",
    "RESTRICT",
    "IF",
    "TEMPORARY",
    "TEMP",
    "REPLACE",
    "OVER",
    "PARTITION",
    "WINDOW",
    "ROWS",
    "RANGE",
    "PRECEDING",
    "FOLLOWING",
    "UNBOUNDED",
    "CURRENT",
    "ROW",
    "FETCH",
    "FIRST",
    "NEXT",
    "ONLY",
    "LATERAL",
    "MATERIALIZED",
    "GENERATED",
    "ALWAYS",
    "IDENTITY",
    "ENABLE",
    "DISABLE",
    "TRIGGER",
    "PROCEDURE",
    "FUNCTION",
    "RETURNS",
    "LANGUAGE",
    "PLPGSQL",
];

pub struct SqlHelper {
    keywords: Vec<String>,
    table_names: Vec<String>,
}

impl SqlHelper {
    pub fn new() -> Self {
        Self {
            keywords: SQL_KEYWORDS.iter().map(|kw| (*kw).to_string()).collect(),
            table_names: Vec::new(),
        }
    }

    pub fn set_tables(&mut self, mut tables: Vec<String>) {
        tables.sort_unstable_by_key(|name| name.to_ascii_lowercase());
        tables.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        self.table_names = tables;
    }

    fn in_string_literal(line: &str, pos: usize) -> bool {
        let bytes = line.as_bytes();
        let mut i = 0;
        let mut in_string = false;

        while i < pos && i < bytes.len() {
            if bytes[i] == b'\'' {
                if i + 1 < pos && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_string = !in_string;
            }
            i += 1;
        }

        in_string
    }

    fn word_start(line: &str, pos: usize) -> usize {
        line[..pos]
            .char_indices()
            .rev()
            .find(|(_, ch)| !matches!(ch, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '$'))
            .map_or(0, |(idx, ch)| idx + ch.len_utf8())
    }
}

impl Default for SqlHelper {
    fn default() -> Self {
        Self::new()
    }
}

impl Helper for SqlHelper {}
impl Highlighter for SqlHelper {}

impl Hinter for SqlHelper {
    type Hint = String;
}

impl Validator for SqlHelper {}

impl Completer for SqlHelper {
    type Candidate = Pair;

    fn complete(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Result<(usize, Vec<Pair>)> {
        if pos > line.len() || Self::in_string_literal(line, pos) {
            return Ok((pos, Vec::new()));
        }

        let start = Self::word_start(line, pos);
        let prefix = &line[start..pos];
        if prefix.is_empty() {
            return Ok((start, Vec::new()));
        }

        let needle = prefix.to_ascii_lowercase();
        let mut seen = HashSet::new();
        let mut pairs = Vec::new();

        for keyword in &self.keywords {
            if keyword.to_ascii_lowercase().starts_with(&needle)
                && seen.insert(keyword.to_ascii_lowercase())
            {
                pairs.push(Pair {
                    display: keyword.clone(),
                    replacement: keyword.clone(),
                });
            }
        }

        for table in &self.table_names {
            if table.to_ascii_lowercase().starts_with(&needle)
                && seen.insert(table.to_ascii_lowercase())
            {
                pairs.push(Pair {
                    display: table.clone(),
                    replacement: table.clone(),
                });
            }
        }

        Ok((start, pairs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_for(helper: &SqlHelper, line: &str) -> Vec<String> {
        let history = history::DefaultHistory::new();
        let (_start, matches) = helper
            .complete(line, line.len(), &Context::new(&history))
            .expect("completion should succeed");
        matches.into_iter().map(|p| p.replacement).collect()
    }

    mod history {
        pub use rustyline::history::DefaultHistory;
    }

    #[test]
    fn test_keyword_completion() {
        let helper = SqlHelper::new();

        let sel = complete_for(&helper, "SEL");
        assert!(sel.iter().any(|candidate| candidate == "SELECT"));

        let ins = complete_for(&helper, "ins");
        assert!(ins.iter().any(|candidate| candidate == "INSERT INTO"));

        let mixed = complete_for(&helper, "sEl");
        assert!(mixed.iter().any(|candidate| candidate == "SELECT"));
    }

    #[test]
    fn test_no_completion_in_string_literal() {
        let helper = SqlHelper::new();
        let inside = complete_for(&helper, "SELECT 'SEL");
        assert!(inside.is_empty());
    }
}
