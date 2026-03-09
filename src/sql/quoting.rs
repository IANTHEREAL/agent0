/// SQL quoting helpers shared across the codebase.
///
/// These functions are intentionally small and allocation-light, and they mirror
/// the behavior used throughout the project (single quotes doubled inside
/// string literals; identifiers quoted when required).
pub(crate) fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub(crate) fn quote_ident(ident: &str) -> String {
    let needs_quote = ident.is_empty() || !is_simple_unquoted_ident(ident) || is_sql_keyword(ident);
    if needs_quote {
        format!("\"{}\"", ident.replace('"', "\"\""))
    } else {
        ident.to_string()
    }
}

fn is_simple_unquoted_ident(ident: &str) -> bool {
    let mut chars = ident.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    for ch in chars {
        if !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '$') {
            return false;
        }
    }
    true
}

/// Returns `true` when the identifier (given in lowercase) matches a
/// PostgreSQL keyword that is NOT in the UNRESERVED_KEYWORD category.
///
/// PostgreSQL `quote_ident()` quotes identifiers that fall into one of
/// these keyword categories: COL_NAME_KEYWORD, TYPE_FUNC_NAME_KEYWORD,
/// or RESERVED_KEYWORD.  Identifiers that are UNRESERVED_KEYWORD or
/// not a keyword at all are returned unquoted.
///
/// The list below is mechanically extracted from PostgreSQL 17's
/// `src/include/parser/kwlist.h` (164 entries, sorted for binary search).
fn is_sql_keyword(ident: &str) -> bool {
    let upper = ident.to_ascii_uppercase();
    PG_NON_UNRESERVED_KEYWORDS
        .binary_search(&upper.as_str())
        .is_ok()
}

/// PostgreSQL 17 non-unreserved keywords (COL_NAME_KEYWORD +
/// TYPE_FUNC_NAME_KEYWORD + RESERVED_KEYWORD).  Sorted for binary search.
/// Source: `src/include/parser/kwlist.h` from PostgreSQL REL_17_STABLE.
static PG_NON_UNRESERVED_KEYWORDS: &[&str] = &[
    "ALL",
    "ANALYSE",
    "ANALYZE",
    "AND",
    "ANY",
    "ARRAY",
    "AS",
    "ASC",
    "ASYMMETRIC",
    "AUTHORIZATION",
    "BETWEEN",
    "BIGINT",
    "BINARY",
    "BIT",
    "BOOLEAN",
    "BOTH",
    "CASE",
    "CAST",
    "CHAR",
    "CHARACTER",
    "CHECK",
    "COALESCE",
    "COLLATE",
    "COLLATION",
    "COLUMN",
    "CONCURRENTLY",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT_CATALOG",
    "CURRENT_DATE",
    "CURRENT_ROLE",
    "CURRENT_SCHEMA",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "DEC",
    "DECIMAL",
    "DEFAULT",
    "DEFERRABLE",
    "DESC",
    "DISTINCT",
    "DO",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "EXTRACT",
    "FALSE",
    "FETCH",
    "FLOAT",
    "FOR",
    "FOREIGN",
    "FREEZE",
    "FROM",
    "FULL",
    "GRANT",
    "GREATEST",
    "GROUP",
    "GROUPING",
    "HAVING",
    "ILIKE",
    "IN",
    "INITIALLY",
    "INNER",
    "INOUT",
    "INT",
    "INTEGER",
    "INTERSECT",
    "INTERVAL",
    "INTO",
    "IS",
    "ISNULL",
    "JOIN",
    "JSON",
    "JSON_ARRAY",
    "JSON_ARRAYAGG",
    "JSON_EXISTS",
    "JSON_OBJECT",
    "JSON_OBJECTAGG",
    "JSON_QUERY",
    "JSON_SCALAR",
    "JSON_SERIALIZE",
    "JSON_TABLE",
    "JSON_VALUE",
    "LATERAL",
    "LEADING",
    "LEAST",
    "LEFT",
    "LIKE",
    "LIMIT",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "MERGE_ACTION",
    "NATIONAL",
    "NATURAL",
    "NCHAR",
    "NONE",
    "NORMALIZE",
    "NOT",
    "NOTNULL",
    "NULL",
    "NULLIF",
    "NUMERIC",
    "OFFSET",
    "ON",
    "ONLY",
    "OR",
    "ORDER",
    "OUT",
    "OUTER",
    "OVERLAPS",
    "OVERLAY",
    "PLACING",
    "POSITION",
    "PRECISION",
    "PRIMARY",
    "REAL",
    "REFERENCES",
    "RETURNING",
    "RIGHT",
    "ROW",
    "SELECT",
    "SESSION_USER",
    "SETOF",
    "SIMILAR",
    "SMALLINT",
    "SOME",
    "SUBSTRING",
    "SYMMETRIC",
    "SYSTEM_USER",
    "TABLE",
    "TABLESAMPLE",
    "THEN",
    "TIME",
    "TIMESTAMP",
    "TO",
    "TRAILING",
    "TREAT",
    "TRIM",
    "TRUE",
    "UNION",
    "UNIQUE",
    "USER",
    "USING",
    "VALUES",
    "VARCHAR",
    "VARIADIC",
    "VERBOSE",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
    "XMLATTRIBUTES",
    "XMLCONCAT",
    "XMLELEMENT",
    "XMLEXISTS",
    "XMLFOREST",
    "XMLNAMESPACES",
    "XMLPARSE",
    "XMLPI",
    "XMLROOT",
    "XMLSERIALIZE",
    "XMLTABLE",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_literal_doubles_single_quotes() {
        assert_eq!(quote_literal("it's"), "'it''s'");
    }

    #[test]
    fn quote_ident_quotes_keywords() {
        assert_eq!(quote_ident("select"), "\"select\"");
    }

    #[test]
    fn quote_ident_allows_simple_unquoted_ident() {
        assert_eq!(quote_ident("my_table"), "my_table");
    }

    #[test]
    fn quote_ident_does_not_quote_non_reserved_keywords() {
        assert_eq!(quote_ident("tables"), "tables");
    }

    #[test]
    fn quote_ident_quotes_non_simple_ident() {
        assert_eq!(quote_ident("123"), "\"123\"");
        assert_eq!(quote_ident("HasCaps"), "\"HasCaps\"");
        assert_eq!(quote_ident("has-dash"), "\"has-dash\"");
    }

    // Boundary keyword tests from issue #1671 — validated against PG 17.
    #[test]
    fn quote_ident_pg_boundary_keywords() {
        // PG RESERVED_KEYWORD — must quote
        assert_eq!(quote_ident("column"), "\"column\"");
        // PG TYPE_FUNC_NAME_KEYWORD — must quote
        assert_eq!(quote_ident("cross"), "\"cross\"");
        // PG RESERVED_KEYWORD — must quote (existing, re-verified)
        assert_eq!(quote_ident("select"), "\"select\"");
        // Not a PG keyword — must NOT quote
        assert_eq!(quote_ident("tables"), "tables");
        // PG UNRESERVED_KEYWORD — must NOT quote
        assert_eq!(quote_ident("view"), "view");
        // Not a PG keyword — must NOT quote
        assert_eq!(quote_ident("name"), "name");
    }

    #[test]
    fn quote_ident_additional_pg_keyword_categories() {
        // More PG reserved keywords
        assert_eq!(quote_ident("table"), "\"table\"");
        assert_eq!(quote_ident("where"), "\"where\"");
        assert_eq!(quote_ident("from"), "\"from\"");
        // PG TYPE_FUNC_NAME keywords
        assert_eq!(quote_ident("full"), "\"full\"");
        assert_eq!(quote_ident("inner"), "\"inner\"");
        assert_eq!(quote_ident("left"), "\"left\"");
        assert_eq!(quote_ident("right"), "\"right\"");
        assert_eq!(quote_ident("outer"), "\"outer\"");
        // PG COL_NAME keywords
        assert_eq!(quote_ident("int"), "\"int\"");
        assert_eq!(quote_ident("boolean"), "\"boolean\"");
    }
}
