use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::{Hint, Hinter};
use rustyline::history::SearchDirection;
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
    columns: HashMap<String, Vec<String>>,
    highlighting_enabled: bool,
}

impl SqlHelper {
    pub fn new() -> Self {
        let highlighting_enabled = std::env::var("NO_COLOR").is_err();
        Self {
            keywords: SQL_KEYWORDS.iter().map(|kw| (*kw).to_string()).collect(),
            table_names: Vec::new(),
            columns: HashMap::new(),
            highlighting_enabled,
        }
    }

    pub fn set_highlighting(&mut self, enabled: bool) {
        self.highlighting_enabled = enabled;
    }

    pub fn set_tables(&mut self, mut tables: Vec<String>) {
        tables.sort_unstable_by_key(|name| name.to_ascii_lowercase());
        tables.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        self.table_names = tables;
    }

    pub fn set_columns(&mut self, columns: HashMap<String, Vec<String>>) {
        self.columns = columns;
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

    fn detect_table_context(&self, line: &str, pos: usize) -> Option<(usize, Vec<Pair>)> {
        let before_cursor = &line[..pos];

        // 1. Check for dot notation: "tablename." or "tablename.prefix"
        if let Some(dot_pos) = before_cursor.rfind('.') {
            let before_dot = &before_cursor[..dot_pos];
            let word_start = before_dot
                .rfind(|c: char| !c.is_alphanumeric() && c != '_')
                .map(|p| p + 1)
                .unwrap_or(0);
            let table_name = &before_dot[word_start..];
            if !table_name.is_empty() {
                let table_lower = table_name.to_lowercase();
                if let Some(cols) = self.columns.get(&table_lower) {
                    let after_dot = &before_cursor[dot_pos + 1..];
                    let after_dot_lower = after_dot.to_lowercase();
                    let pairs: Vec<Pair> = cols
                        .iter()
                        .filter(|c| c.to_lowercase().starts_with(&after_dot_lower))
                        .map(|c| Pair {
                            display: c.clone(),
                            replacement: c.clone(),
                        })
                        .collect();
                    return Some((dot_pos + 1, pairs));
                }
            }
        }

        // 2. Scan the full line for FROM/JOIN to find table context
        let full_lower = line.to_lowercase();
        let before_lower = before_cursor.to_lowercase();
        let mut table_key: Option<String> = None;
        for keyword in &["from ", "join "] {
            if let Some(kw_pos) = full_lower.rfind(keyword) {
                let after_kw_start = kw_pos + keyword.len();
                let after_kw = &full_lower[after_kw_start..];
                let trimmed = after_kw.trim_start();
                let table_end = trimmed
                    .find(|c: char| !c.is_alphanumeric() && c != '_')
                    .unwrap_or(trimmed.len());
                let table = &trimmed[..table_end];
                if !table.is_empty() && self.columns.contains_key(table) {
                    table_key = Some(table.to_string());
                    break;
                }
            }
        }

        let table_name = table_key?;
        let cols = self.columns.get(&table_name)?;

        // Don't suggest columns if cursor is right after FROM/JOIN (table position)
        let cursor_word_start = before_cursor
            .rfind(|c: char| c.is_whitespace())
            .map(|p| p + 1)
            .unwrap_or(0);
        let before_word = before_lower[..cursor_word_start].trim_end();
        if before_word.ends_with("from") || before_word.ends_with("join") {
            return None;
        }

        let typing = &before_cursor[cursor_word_start..];
        let typing_lower = typing.to_lowercase();

        let pairs: Vec<Pair> = cols
            .iter()
            .filter(|c| c.to_lowercase().starts_with(&typing_lower))
            .map(|c| Pair {
                display: c.clone(),
                replacement: c.clone(),
            })
            .collect();

        if pairs.is_empty() {
            return None;
        }

        Some((cursor_word_start, pairs))
    }
}

impl Default for SqlHelper {
    fn default() -> Self {
        Self::new()
    }
}

// ── ANSI color codes ────────────────────────────────────────────
const ANSI_RESET: &str = "\x1b[0m";
const ANSI_KEYWORD: &str = "\x1b[1;34m"; // bold blue
const ANSI_STRING: &str = "\x1b[32m"; // green
const ANSI_NUMBER: &str = "\x1b[33m"; // yellow
const ANSI_COMMENT: &str = "\x1b[90m"; // gray
const ANSI_OPERATOR: &str = "\x1b[36m"; // cyan

fn is_keyword(word: &str) -> bool {
    let upper = word.to_ascii_uppercase();
    SQL_KEYWORDS.iter().any(|kw| *kw == upper)
}

pub fn highlight_sql(line: &str) -> String {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + 128);
    let mut i = 0;

    while i < len {
        let b = bytes[i];

        if b == b'-' && i + 1 < len && bytes[i + 1] == b'-' {
            out.push_str(ANSI_COMMENT);
            out.push_str(&line[i..]);
            out.push_str(ANSI_RESET);
            return out;
        }

        if b == b'\'' {
            out.push_str(ANSI_STRING);
            out.push(b as char);
            i += 1;
            while i < len {
                let c = bytes[i];
                out.push(c as char);
                if c == b'\'' {
                    if i + 1 < len && bytes[i + 1] == b'\'' {
                        i += 1;
                        out.push(bytes[i] as char);
                    } else {
                        break;
                    }
                }
                i += 1;
            }
            out.push_str(ANSI_RESET);
            i += 1;
            continue;
        }

        if b.is_ascii_digit() || (b == b'.' && i + 1 < len && bytes[i + 1].is_ascii_digit()) {
            out.push_str(ANSI_NUMBER);
            while i < len && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                out.push(bytes[i] as char);
                i += 1;
            }
            out.push_str(ANSI_RESET);
            continue;
        }

        if matches!(b, b'=' | b'<' | b'>' | b'!') {
            out.push_str(ANSI_OPERATOR);
            if b == b'!' && i + 1 < len && bytes[i + 1] == b'=' {
                out.push_str("!=");
                i += 2;
            } else if b == b'<' && i + 1 < len && bytes[i + 1] == b'>' {
                out.push_str("<>");
                i += 2;
            } else if b == b'<' && i + 1 < len && bytes[i + 1] == b'=' {
                out.push_str("<=");
                i += 2;
            } else if b == b'>' && i + 1 < len && bytes[i + 1] == b'=' {
                out.push_str(">=");
                i += 2;
            } else {
                out.push(b as char);
                i += 1;
            }
            out.push_str(ANSI_RESET);
            continue;
        }

        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &line[start..i];
            if is_keyword(word) {
                out.push_str(ANSI_KEYWORD);
                out.push_str(word);
                out.push_str(ANSI_RESET);
            } else {
                out.push_str(word);
            }
            continue;
        }

        out.push(b as char);
        i += 1;
    }

    out
}

impl Helper for SqlHelper {}

impl Highlighter for SqlHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if !self.highlighting_enabled {
            return Cow::Borrowed(line);
        }
        Cow::Owned(highlight_sql(line))
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _forced: bool) -> bool {
        self.highlighting_enabled
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        if !self.highlighting_enabled {
            return Cow::Borrowed(hint);
        }
        Cow::Owned(format!("\x1b[90m{}\x1b[0m", hint))
    }
}

pub struct SqlHint(String);

impl Hint for SqlHint {
    fn display(&self) -> &str {
        &self.0
    }

    fn completion(&self) -> Option<&str> {
        Some(&self.0)
    }
}

impl Hinter for SqlHelper {
    type Hint = SqlHint;

    fn hint(&self, line: &str, pos: usize, ctx: &Context<'_>) -> Option<Self::Hint> {
        if line.len() < 3 || pos < line.len() {
            return None;
        }

        let lower = line.to_lowercase();

        for idx in (0..ctx.history().len()).rev() {
            if let Ok(Some(entry)) = ctx.history().get(idx, SearchDirection::Forward) {
                let entry_str = entry.entry.as_ref();
                if entry_str.to_lowercase().starts_with(&lower) && entry_str.len() > line.len() {
                    let suffix = entry_str[line.len()..].to_string();
                    return Some(SqlHint(suffix));
                }
            }
        }

        None
    }
}

impl Validator for SqlHelper {}

impl Completer for SqlHelper {
    type Candidate = Pair;

    fn complete(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Result<(usize, Vec<Pair>)> {
        if pos > line.len() || Self::in_string_literal(line, pos) {
            return Ok((pos, Vec::new()));
        }

        if let Some((start_pos, pairs)) = self.detect_table_context(line, pos) {
            if !pairs.is_empty() {
                return Ok((start_pos, pairs));
            }
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

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut in_escape = false;
        for ch in s.chars() {
            if ch == '\x1b' {
                in_escape = true;
            } else if in_escape {
                if ch == 'm' {
                    in_escape = false;
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    #[test]
    fn highlight_preserves_text_content() {
        let input = "SELECT name FROM users WHERE id = 1";
        let highlighted = highlight_sql(input);
        assert_eq!(strip_ansi(&highlighted), input);
    }

    #[test]
    fn highlight_keywords_get_colored() {
        let highlighted = highlight_sql("SELECT");
        assert!(highlighted.contains(ANSI_KEYWORD));
        assert!(highlighted.contains(ANSI_RESET));
    }

    #[test]
    fn highlight_string_literal() {
        let highlighted = highlight_sql("'hello world'");
        assert!(highlighted.contains(ANSI_STRING));
        assert_eq!(strip_ansi(&highlighted), "'hello world'");
    }

    #[test]
    fn highlight_keyword_inside_string_not_colored() {
        let highlighted = highlight_sql("'SELECT'");
        assert!(highlighted.contains(ANSI_STRING));
        assert!(!highlighted.contains(ANSI_KEYWORD));
    }

    #[test]
    fn highlight_numbers() {
        let highlighted = highlight_sql("42");
        assert!(highlighted.contains(ANSI_NUMBER));
        assert_eq!(strip_ansi(&highlighted), "42");

        let decimal = highlight_sql("3.14");
        assert!(decimal.contains(ANSI_NUMBER));
        assert_eq!(strip_ansi(&decimal), "3.14");
    }

    #[test]
    fn highlight_operators() {
        for op in &["=", "<", ">", "!=", "<=", ">=", "<>"] {
            let highlighted = highlight_sql(op);
            assert!(
                highlighted.contains(ANSI_OPERATOR),
                "operator {op} not highlighted"
            );
            assert_eq!(strip_ansi(&highlighted), *op);
        }
    }

    #[test]
    fn highlight_line_comment() {
        let highlighted = highlight_sql("-- this is a comment");
        assert!(highlighted.contains(ANSI_COMMENT));
        assert_eq!(strip_ansi(&highlighted), "-- this is a comment");
    }

    #[test]
    fn highlight_mixed_statement() {
        let input = "SELECT id, name FROM users WHERE age >= 18 AND name = 'Alice' -- filter";
        let highlighted = highlight_sql(input);
        assert_eq!(strip_ansi(&highlighted), input);
        assert!(highlighted.contains(ANSI_KEYWORD));
        assert!(highlighted.contains(ANSI_STRING));
        assert!(highlighted.contains(ANSI_NUMBER));
        assert!(highlighted.contains(ANSI_OPERATOR));
        assert!(highlighted.contains(ANSI_COMMENT));
    }

    #[test]
    fn highlight_empty_input() {
        assert_eq!(highlight_sql(""), "");
    }

    #[test]
    fn highlight_non_keyword_identifiers() {
        let highlighted = highlight_sql("mytable");
        assert!(!highlighted.contains(ANSI_KEYWORD));
        assert_eq!(highlighted, "mytable");
    }

    #[test]
    fn highlight_case_insensitive_keywords() {
        for kw in &["select", "Select", "SELECT", "sElEcT"] {
            let highlighted = highlight_sql(kw);
            assert!(
                highlighted.contains(ANSI_KEYWORD),
                "{kw} not recognized as keyword"
            );
        }
    }

    #[test]
    fn highlight_escaped_quote_in_string() {
        let input = "'it''s a test'";
        let highlighted = highlight_sql(input);
        assert_eq!(strip_ansi(&highlighted), input);
        assert!(highlighted.contains(ANSI_STRING));
        assert!(!highlighted.contains(ANSI_KEYWORD));
    }

    // ── table completion tests ──────────────────────────────────

    #[test]
    fn test_table_name_completion() {
        let mut helper = SqlHelper::new();
        helper.set_tables(vec!["users".into(), "orders".into(), "user_roles".into()]);
        let results = complete_for(&helper, "us");
        assert!(results.iter().any(|r| r == "users"));
        assert!(results.iter().any(|r| r == "user_roles"));
        assert!(!results.iter().any(|r| r == "orders"));
    }

    #[test]
    fn test_table_and_keyword_mixed() {
        let mut helper = SqlHelper::new();
        helper.set_tables(vec!["settings".into()]);
        let results = complete_for(&helper, "se");
        assert!(results.iter().any(|r| r == "SELECT"));
        assert!(results.iter().any(|r| r == "settings"));
    }

    #[test]
    fn test_empty_prefix_no_completion() {
        let helper = SqlHelper::new();
        let results = complete_for(&helper, "");
        assert!(results.is_empty());
    }

    #[test]
    fn test_completion_deduplication() {
        let mut helper = SqlHelper::new();
        helper.set_tables(vec!["select".into()]);
        let results = complete_for(&helper, "sel");
        let count = results
            .iter()
            .filter(|r| r.to_lowercase() == "select")
            .count();
        assert!(count >= 1);
    }

    // ── in_string_literal tests ─────────────────────────────────

    #[test]
    fn test_in_string_literal_basic() {
        assert!(!SqlHelper::in_string_literal("SELECT", 3));
        assert!(SqlHelper::in_string_literal("SELECT 'hello", 10));
        assert!(!SqlHelper::in_string_literal("SELECT 'hello'", 14));
    }

    #[test]
    fn test_in_string_literal_escaped_quote() {
        assert!(SqlHelper::in_string_literal("'it''s a test'", 8));
        assert!(!SqlHelper::in_string_literal("'it''s a test'", 14));
    }

    #[test]
    fn test_in_string_literal_empty() {
        assert!(!SqlHelper::in_string_literal("", 0));
    }

    #[test]
    fn test_no_completion_inside_escaped_string() {
        let helper = SqlHelper::new();
        let results = complete_for(&helper, "SELECT 'it''s SEL");
        assert!(results.is_empty());
    }

    // ── word_start tests ────────────────────────────────────────

    #[test]
    fn test_word_start_beginning() {
        assert_eq!(SqlHelper::word_start("SELECT", 3), 0);
    }

    #[test]
    fn test_word_start_after_space() {
        assert_eq!(SqlHelper::word_start("SELECT name", 10), 7);
    }

    #[test]
    fn test_word_start_after_dot() {
        assert_eq!(SqlHelper::word_start("schema.tab", 10), 7);
    }

    // ── Highlighter trait tests ─────────────────────────────────

    #[test]
    fn test_highlighter_disabled_returns_borrowed() {
        let mut helper = SqlHelper::new();
        helper.set_highlighting(false);
        let result = helper.highlight("SELECT 1", 0);
        assert_eq!(&*result, "SELECT 1");
        assert!(!result.contains('\x1b'));
    }

    #[test]
    fn test_highlighter_enabled_adds_ansi() {
        let mut helper = SqlHelper::new();
        helper.set_highlighting(true);
        let result = helper.highlight("SELECT", 0);
        assert!(result.contains('\x1b'));
    }

    #[test]
    fn test_highlight_char_follows_setting() {
        let mut helper = SqlHelper::new();
        helper.set_highlighting(true);
        assert!(helper.highlight_char("x", 0, false));
        helper.set_highlighting(false);
        assert!(!helper.highlight_char("x", 0, false));
    }

    #[test]
    fn test_highlight_hint_adds_gray() {
        let mut helper = SqlHelper::new();
        helper.set_highlighting(true);
        let result = helper.highlight_hint("suggestion");
        assert!(result.contains("\x1b[90m"));
    }

    // ── highlight_sql edge cases ────────────────────────────────

    #[test]
    fn highlight_unterminated_string() {
        let input = "SELECT 'unterminated";
        let highlighted = highlight_sql(input);
        assert_eq!(strip_ansi(&highlighted), input);
    }

    #[test]
    fn highlight_dot_not_number() {
        let highlighted = highlight_sql("a.b");
        assert_eq!(strip_ansi(&highlighted), "a.b");
        assert!(!highlighted.contains(ANSI_NUMBER));
    }

    #[test]
    fn highlight_comment_after_code() {
        let input = "SELECT 1 -- comment";
        let highlighted = highlight_sql(input);
        assert_eq!(strip_ansi(&highlighted), input);
        assert!(highlighted.contains(ANSI_KEYWORD));
        assert!(highlighted.contains(ANSI_NUMBER));
        assert!(highlighted.contains(ANSI_COMMENT));
    }

    #[test]
    fn highlight_multiple_strings() {
        let input = "'a' || 'b'";
        let highlighted = highlight_sql(input);
        assert_eq!(strip_ansi(&highlighted), input);
    }

    #[test]
    fn highlight_all_operator_pairs() {
        for op in &["!=", "<=", ">=", "<>"] {
            let highlighted = highlight_sql(op);
            assert!(
                highlighted.contains(ANSI_OPERATOR),
                "{op} not highlighted as operator"
            );
            assert_eq!(strip_ansi(&highlighted), *op, "{op} text mangled");
        }
    }

    #[test]
    fn highlight_underscore_identifier() {
        let highlighted = highlight_sql("_private_col");
        assert!(!highlighted.contains(ANSI_KEYWORD));
        assert_eq!(strip_ansi(&highlighted), "_private_col");
    }

    #[test]
    fn highlight_whitespace_only() {
        let highlighted = highlight_sql("   ");
        assert_eq!(highlighted, "   ");
    }

    // ── column completion tests ─────────────────────────────────

    fn helper_with_columns() -> SqlHelper {
        let mut helper = SqlHelper::new();
        helper.set_tables(vec!["users".into(), "orders".into()]);
        let mut columns = HashMap::new();
        columns.insert(
            "users".to_string(),
            vec![
                "id".into(),
                "name".into(),
                "email".into(),
                "created_at".into(),
            ],
        );
        columns.insert(
            "orders".to_string(),
            vec!["id".into(), "user_id".into(), "total".into()],
        );
        helper.set_columns(columns);
        helper
    }

    fn complete_at(helper: &SqlHelper, line: &str, pos: usize) -> Vec<String> {
        let history = history::DefaultHistory::new();
        let (_start, matches) = helper
            .complete(line, pos, &Context::new(&history))
            .expect("completion should succeed");
        matches.into_iter().map(|p| p.replacement).collect()
    }

    #[test]
    fn test_column_completion_dot_notation() {
        let helper = helper_with_columns();
        let results = complete_for(&helper, "users.");
        assert!(results.contains(&"id".to_string()));
        assert!(results.contains(&"name".to_string()));
        assert!(results.contains(&"email".to_string()));
        assert!(results.contains(&"created_at".to_string()));
    }

    #[test]
    fn test_column_completion_dot_with_prefix() {
        let helper = helper_with_columns();
        let results = complete_for(&helper, "users.na");
        assert_eq!(results, vec!["name".to_string()]);
    }

    #[test]
    fn test_column_completion_where_context() {
        let helper = helper_with_columns();
        let line = "SELECT * FROM users WHERE ";
        let results = complete_at(&helper, line, line.len());
        assert!(results.contains(&"id".to_string()));
        assert!(results.contains(&"name".to_string()));
        assert!(results.contains(&"email".to_string()));
    }

    #[test]
    fn test_column_completion_where_with_prefix() {
        let helper = helper_with_columns();
        let results = complete_for(&helper, "SELECT * FROM users WHERE na");
        assert_eq!(results, vec!["name".to_string()]);
    }

    #[test]
    fn test_column_completion_unknown_table_dot() {
        let helper = helper_with_columns();
        let results = complete_for(&helper, "unknown.");
        assert!(results.is_empty());
    }

    #[test]
    fn test_column_completion_no_columns_loaded() {
        let mut helper = SqlHelper::new();
        helper.set_tables(vec!["users".into()]);
        let line = "SELECT * FROM users WHERE na";
        let results = complete_at(&helper, line, line.len());
        assert!(!results.contains(&"name".to_string()));
        assert!(results.iter().all(|r| r.to_lowercase().starts_with("na")));
    }

    #[test]
    fn test_column_completion_case_insensitive() {
        let helper = helper_with_columns();
        let results = complete_for(&helper, "USERS.");
        assert!(results.contains(&"id".to_string()));
        assert!(results.contains(&"name".to_string()));
    }

    #[test]
    fn test_column_completion_from_suggests_tables() {
        let helper = helper_with_columns();
        let results = complete_for(&helper, "SELECT * FROM us");
        assert!(results.contains(&"users".to_string()));
        assert!(!results.contains(&"id".to_string()));
    }

    #[test]
    fn test_column_completion_select_context() {
        let helper = helper_with_columns();
        let line = "SELECT em FROM users";
        let results = complete_at(&helper, line, 9);
        assert!(results.contains(&"email".to_string()));
    }
}
