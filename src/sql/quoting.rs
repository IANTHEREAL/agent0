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

fn is_sql_keyword(ident: &str) -> bool {
    let upper = ident.to_ascii_uppercase();
    sqlparser::keywords::ALL_KEYWORDS
        .binary_search(&upper.as_str())
        .is_ok()
}

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
    fn quote_ident_quotes_non_simple_ident() {
        assert_eq!(quote_ident("123"), "\"123\"");
        assert_eq!(quote_ident("HasCaps"), "\"HasCaps\"");
        assert_eq!(quote_ident("has-dash"), "\"has-dash\"");
    }
}
