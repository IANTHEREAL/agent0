//! SQL tokenizer for parse-time operator rewrites.
//!
//! Provides a lightweight tokenizer (`tokenize_sql_for_rewrite`) that classifies
//! SQL text into tokens (words, strings, operators, etc.) for use by the
//! operator rewrite passes. This is intentionally separate from sqlparser-rs'
//! tokenizer because it needs to recognise PostgreSQL-specific operators (`?`,
//! `?|`, `?&`, `<->`, `<#>`, `<=>`) that sqlparser-rs cannot parse.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenKind {
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
pub(crate) struct Token {
    pub(crate) kind: TokenKind,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) text: String,
}

pub(crate) fn tokenize_sql_for_rewrite(sql: &str) -> Vec<Token> {
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

pub(crate) fn is_ident_char(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
}

pub(crate) fn is_rewrite_boundary_keyword(token_upper: &str) -> bool {
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

pub(crate) fn is_comparison_operator_char(tok: &Token) -> bool {
    tok.kind == TokenKind::Other && matches!(tok.text.as_str(), "<" | ">" | "=" | "!")
}

pub(crate) fn find_left_expr_start(tokens: &[Token], op_idx: usize) -> usize {
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

pub(crate) fn find_right_expr_end(tokens: &[Token], op_idx: usize) -> usize {
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

pub(crate) fn skip_ws_comments_forward(tokens: &[Token], mut idx: usize, stop: usize) -> usize {
    while idx < stop && matches!(tokens[idx].kind, TokenKind::Whitespace | TokenKind::Comment) {
        idx += 1;
    }
    idx
}

pub(crate) fn skip_ws_comments_backward(tokens: &[Token], mut idx: usize, start: usize) -> usize {
    while idx > start && matches!(tokens[idx].kind, TokenKind::Whitespace | TokenKind::Comment) {
        idx -= 1;
    }
    idx
}
