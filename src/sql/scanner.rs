//! Shared SQL character-level scanner for tracking lexical context.
//!
//! `SqlCharScanner` iterates over a SQL string byte-by-byte, maintaining state
//! for single-quoted strings, double-quoted identifiers, dollar-quoted strings,
//! line comments (`--`), nested block comments (`/* */`), parenthesis depth,
//! and `E'...'` escape string literals.
//!
//! Consumers iterate and apply their own domain logic (keyword search, comma
//! splitting, paren matching, parameter counting) on top of the context.

/// Context for each byte position yielded by the scanner.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct CharContext {
    pub pos: usize,
    pub byte: u8,
    pub in_single_quote: bool,
    pub in_double_quote: bool,
    pub in_dollar_quote: bool,
    pub in_line_comment: bool,
    pub in_block_comment: bool,
    pub paren_depth: usize,
    /// True when inside an `E'...'` escape string literal.
    #[cfg(test)]
    pub in_escape_string: bool,
}

impl CharContext {
    /// True when inside any quoted context (single, double, or dollar).
    #[inline]
    pub fn in_string(self) -> bool {
        self.in_single_quote || self.in_double_quote || self.in_dollar_quote
    }

    /// True when inside any comment (line or block).
    #[inline]
    pub fn in_comment(self) -> bool {
        self.in_line_comment || self.in_block_comment
    }

    /// True when not inside any string or comment (but may be inside parens).
    #[inline]
    pub fn is_code(self) -> bool {
        !self.in_string() && !self.in_comment()
    }
}

/// Tracks SQL lexical state while iterating over bytes.
pub(crate) struct SqlCharScanner<'a> {
    bytes: &'a [u8],
    pos: usize,
    // Lexical state
    in_single_quote: bool,
    in_escape_single_quote: bool,
    in_double_quote: bool,
    dollar_delim: Option<Vec<u8>>,
    paren_depth: usize,
    in_line_comment: bool,
    block_comment_depth: usize,
    /// Callback to check if a byte before `'` is part of an identifier.
    /// Used to correctly detect `E'...'` escape strings.
    is_ident_char_fn: fn(u8) -> bool,
}

fn default_is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

fn is_ident_char_or_dollar(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_') || b == b'$' || b >= 0x80
}

/// Count the maximum positional parameter index (`$1`, `$2`, ...) referenced
/// by a SQL statement while ignoring strings, comments, and identifiers.
pub(crate) fn count_sql_parameters(sql: &str) -> usize {
    let bytes = sql.as_bytes();
    let mut max_param = 0usize;

    for ctx in SqlCharScanner::new(sql) {
        if ctx.in_string() || ctx.in_comment() || ctx.byte != b'$' {
            continue;
        }

        let i = ctx.pos;
        let mut j = i + 1;
        let mut saw_digit = false;
        let mut num = 0usize;
        while j < bytes.len() && bytes[j].is_ascii_digit() && j - i <= 10 {
            saw_digit = true;
            num = num
                .saturating_mul(10)
                .saturating_add((bytes[j] - b'0') as usize);
            j += 1;
        }
        if !saw_digit {
            continue;
        }

        let before_ok = i == 0 || !is_ident_char_or_dollar(bytes[i - 1]);
        let after_ok = j == bytes.len() || !is_ident_char_or_dollar(bytes[j]);
        if before_ok && after_ok {
            max_param = max_param.max(num);
        }
    }

    max_param
}

impl<'a> SqlCharScanner<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            bytes: input.as_bytes(),
            pos: 0,
            in_single_quote: false,
            in_escape_single_quote: false,
            in_double_quote: false,
            dollar_delim: None,
            paren_depth: 0,
            in_line_comment: false,
            block_comment_depth: 0,
            is_ident_char_fn: default_is_ident_char,
        }
    }

    /// Start scanning from an offset (e.g. for `find_matching_paren`).
    pub fn with_offset(input: &'a str, offset: usize) -> Self {
        let mut s = Self::new(input);
        s.pos = offset;
        s
    }

    fn emit(&self, pos: usize, byte: u8) -> CharContext {
        CharContext {
            pos,
            byte,
            in_single_quote: self.in_single_quote,
            in_double_quote: self.in_double_quote,
            in_dollar_quote: self.dollar_delim.is_some(),
            in_line_comment: self.in_line_comment,
            in_block_comment: self.block_comment_depth > 0,
            paren_depth: self.paren_depth,
            #[cfg(test)]
            in_escape_string: self.in_escape_single_quote,
        }
    }
}

impl<'a> Iterator for SqlCharScanner<'a> {
    type Item = CharContext;

    fn next(&mut self) -> Option<CharContext> {
        if self.pos >= self.bytes.len() {
            return None;
        }

        let i = self.pos;
        let b = self.bytes[i];

        // === Inside dollar-quoted string ===
        if let Some(delim) = self.dollar_delim.as_ref() {
            let delim_len = delim.len();
            if i + delim_len <= self.bytes.len()
                && &self.bytes[i..i + delim_len] == delim.as_slice()
            {
                // Closing delimiter found — emit first byte of closing delimiter,
                // advance past it, and clear state.
                let ctx = self.emit(i, b);
                self.pos = i + delim_len;
                self.dollar_delim = None;
                return Some(ctx);
            }
            let ctx = self.emit(i, b);
            self.pos = i + 1;
            return Some(ctx);
        }

        // === Inside line comment ===
        if self.in_line_comment {
            let ctx = self.emit(i, b);
            if b == b'\n' {
                self.in_line_comment = false;
            }
            self.pos = i + 1;
            return Some(ctx);
        }

        // === Inside block comment ===
        if self.block_comment_depth > 0 {
            if b == b'/' && i + 1 < self.bytes.len() && self.bytes[i + 1] == b'*' {
                self.block_comment_depth += 1;
                let ctx = self.emit(i, b);
                self.pos = i + 2;
                return Some(ctx);
            }
            if b == b'*' && i + 1 < self.bytes.len() && self.bytes[i + 1] == b'/' {
                // Emit first so the `*` of closing `*/` sees depth > 0.
                let ctx = self.emit(i, b);
                self.block_comment_depth -= 1;
                self.pos = i + 2;
                return Some(ctx);
            }
            let ctx = self.emit(i, b);
            self.pos = i + 1;
            return Some(ctx);
        }

        // === Inside single-quoted string ===
        if self.in_single_quote {
            if b == b'\'' {
                // Check for escaped quote ''
                if i + 1 < self.bytes.len() && self.bytes[i + 1] == b'\'' {
                    let ctx = self.emit(i, b);
                    self.pos = i + 2;
                    return Some(ctx);
                }
                // Check for backslash escape in E'...' strings
                if self.in_escape_single_quote {
                    let mut backslash_count = 0usize;
                    let mut k = i;
                    while k > 0 && self.bytes[k - 1] == b'\\' {
                        backslash_count += 1;
                        k -= 1;
                    }
                    if backslash_count % 2 == 1 {
                        let ctx = self.emit(i, b);
                        self.pos = i + 1;
                        return Some(ctx);
                    }
                }
                // Closing quote
                let ctx = self.emit(i, b);
                self.in_single_quote = false;
                self.in_escape_single_quote = false;
                self.pos = i + 1;
                return Some(ctx);
            }
            let ctx = self.emit(i, b);
            self.pos = i + 1;
            return Some(ctx);
        }

        // === Inside double-quoted identifier ===
        if self.in_double_quote {
            if b == b'"' {
                // Check for escaped ""
                if i + 1 < self.bytes.len() && self.bytes[i + 1] == b'"' {
                    let ctx = self.emit(i, b);
                    self.pos = i + 2;
                    return Some(ctx);
                }
                let ctx = self.emit(i, b);
                self.in_double_quote = false;
                self.pos = i + 1;
                return Some(ctx);
            }
            let ctx = self.emit(i, b);
            self.pos = i + 1;
            return Some(ctx);
        }

        // === Code context: not inside any string or comment ===

        // Single quote opens
        if b == b'\'' {
            self.in_single_quote = true;
            // Check for E'...' prefix
            self.in_escape_single_quote = i > 0
                && matches!(self.bytes[i - 1], b'e' | b'E')
                && (i == 1 || !(self.is_ident_char_fn)(self.bytes[i - 2]));
            let ctx = self.emit(i, b);
            self.pos = i + 1;
            return Some(ctx);
        }

        // Double quote opens
        if b == b'"' {
            self.in_double_quote = true;
            let ctx = self.emit(i, b);
            self.pos = i + 1;
            return Some(ctx);
        }

        // Line comment
        if b == b'-' && i + 1 < self.bytes.len() && self.bytes[i + 1] == b'-' {
            self.in_line_comment = true;
            let ctx = self.emit(i, b);
            self.pos = i + 2;
            return Some(ctx);
        }

        // Block comment
        if b == b'/' && i + 1 < self.bytes.len() && self.bytes[i + 1] == b'*' {
            self.block_comment_depth = 1;
            let ctx = self.emit(i, b);
            self.pos = i + 2;
            return Some(ctx);
        }

        // Dollar sign — could be dollar-quoted string or placeholder
        if b == b'$' {
            let before_ok = i == 0 || !(self.is_ident_char_fn)(self.bytes[i - 1]);

            if before_ok {
                // Check $$ (empty tag)
                if i + 1 < self.bytes.len() && self.bytes[i + 1] == b'$' {
                    self.dollar_delim = Some(b"$$".to_vec());
                    let ctx = self.emit(i, b);
                    self.pos = i + 2;
                    return Some(ctx);
                }

                // Check $tag$ (named tag)
                let mut j = i + 1;
                if j < self.bytes.len()
                    && (self.bytes[j].is_ascii_alphabetic()
                        || self.bytes[j] == b'_'
                        || self.bytes[j] >= 0x80)
                {
                    j += 1;
                    while j < self.bytes.len() && self.bytes[j] != b'$' {
                        if self.bytes[j].is_ascii_alphanumeric()
                            || self.bytes[j] == b'_'
                            || self.bytes[j] >= 0x80
                        {
                            j += 1;
                            continue;
                        }
                        break;
                    }
                    if j < self.bytes.len() && self.bytes[j] == b'$' {
                        self.dollar_delim = Some(self.bytes[i..=j].to_vec());
                        let ctx = self.emit(i, b);
                        self.pos = j + 1;
                        return Some(ctx);
                    }
                }
            }
        }

        // Parentheses
        if b == b'(' {
            self.paren_depth += 1;
            let ctx = self.emit(i, b);
            self.pos = i + 1;
            return Some(ctx);
        }
        if b == b')' {
            let ctx = self.emit(i, b);
            self.paren_depth = self.paren_depth.saturating_sub(1);
            self.pos = i + 1;
            return Some(ctx);
        }

        // Default: regular code byte
        let ctx = self.emit(i, b);
        self.pos = i + 1;
        Some(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_contexts(input: &str) -> Vec<CharContext> {
        SqlCharScanner::new(input).collect()
    }

    #[test]
    fn test_plain_code() {
        let ctxs = collect_contexts("SELECT 1");
        assert!(ctxs.iter().all(|c| c.is_code()));
        assert!(ctxs.iter().all(|c| !c.in_string()));
        assert!(ctxs.iter().all(|c| !c.in_comment()));
    }

    #[test]
    fn test_single_quoted_string() {
        let ctxs = collect_contexts("SELECT 'hello' FROM t");
        // The ' at pos 7 opens the string, chars inside are in_single_quote
        let in_str: Vec<usize> = ctxs
            .iter()
            .filter(|c| c.in_single_quote)
            .map(|c| c.pos)
            .collect();
        assert!(!in_str.is_empty());
        // Chars outside the string are not in_single_quote
        assert!(!ctxs[0].in_single_quote); // 'S'
    }

    #[test]
    fn test_escaped_single_quote() {
        let ctxs = collect_contexts("SELECT 'it''s' x");
        // The '' inside should not end the string prematurely
        let last = ctxs.last().unwrap();
        assert!(!last.in_single_quote); // 'x' is outside string
    }

    #[test]
    fn test_double_quoted_identifier() {
        let ctxs = collect_contexts(r#"SELECT "My Col" FROM t"#);
        let in_dq: Vec<usize> = ctxs
            .iter()
            .filter(|c| c.in_double_quote)
            .map(|c| c.pos)
            .collect();
        assert!(!in_dq.is_empty());
        assert!(!ctxs.last().unwrap().in_double_quote);
    }

    #[test]
    fn test_escaped_double_quote() {
        let ctxs = collect_contexts(r#"SELECT """x""" FROM t"#);
        let last = ctxs.last().unwrap();
        assert!(!last.in_double_quote);
    }

    #[test]
    fn test_dollar_quoted_string() {
        let ctxs = collect_contexts("SELECT $$ hello; world $$ FROM t");
        // The semicolon inside $$ should be in_dollar_quote
        let semi = ctxs.iter().find(|c| c.byte == b';').unwrap();
        assert!(semi.in_dollar_quote);
        assert!(semi.in_string());
        // The final 't' should not be in any string
        let last = ctxs.last().unwrap();
        assert!(!last.in_string());
    }

    #[test]
    fn test_tagged_dollar_quote() {
        let ctxs = collect_contexts("SELECT $fn$ body; here $fn$ FROM t");
        let semi = ctxs.iter().find(|c| c.byte == b';').unwrap();
        assert!(semi.in_dollar_quote);
        let last = ctxs.last().unwrap();
        assert!(!last.in_string());
    }

    #[test]
    fn test_line_comment() {
        let ctxs = collect_contexts("SELECT 1 -- comment\nSELECT 2");
        let comment_chars: Vec<_> = ctxs.iter().filter(|c| c.in_line_comment).collect();
        assert!(!comment_chars.is_empty());
        // The newline ends the comment; chars after should be code
        let last = ctxs.last().unwrap();
        assert!(!last.in_comment());
    }

    #[test]
    fn test_nested_block_comment() {
        let ctxs = collect_contexts("SELECT /* outer /* inner */ still */ 1");
        // The '1' at the end should not be in a comment
        let last = ctxs.last().unwrap();
        assert!(!last.in_comment());
        // The 'i' of inner should be in a block comment
        let inner_i = ctxs.iter().find(|c| c.byte == b'i').unwrap();
        assert!(inner_i.in_block_comment);
        // The LAST '*' in the output must be the outermost closing '*/' — verify it has in_block_comment: true
        let last_star = ctxs.iter().rev().find(|c| c.byte == b'*').unwrap();
        assert!(
            last_star.in_block_comment,
            "outermost closing `*` of `*/` must have in_block_comment: true"
        );
    }

    #[test]
    fn test_paren_depth() {
        let ctxs = collect_contexts("SELECT (a, (b)) x");
        // Find the 'b' — should be at paren_depth 2
        let b_ctx = ctxs.iter().find(|c| c.byte == b'b').unwrap();
        assert_eq!(b_ctx.paren_depth, 2);
        // ')' is emitted at its pre-decrement depth; the trailing 'x' is at depth 0
        let last = ctxs.last().unwrap();
        assert_eq!(last.paren_depth, 0);
    }

    #[test]
    fn test_escape_string_literal() {
        let ctxs = collect_contexts(r"SELECT E'it\'s' FROM t");
        // The semicolon-like backslash-quote should not end the string
        // After E'...' ends, we should be back in code
        let last = ctxs.last().unwrap();
        assert!(!last.in_string());
        // The 's' inside the escape string should be marked
        let escape_chars: Vec<_> = ctxs.iter().filter(|c| c.in_escape_string).collect();
        assert!(!escape_chars.is_empty());
    }

    #[test]
    fn test_dollar_not_opened_after_identifier() {
        // a$$ should NOT start a dollar-quote because $ follows an identifier char
        let ctxs = collect_contexts("SELECT a$$ FROM t");
        let last = ctxs.last().unwrap();
        assert!(!last.in_string());
        assert!(ctxs.iter().all(|c| !c.in_dollar_quote));
    }

    #[test]
    fn test_mixed_context() {
        let ctxs = collect_contexts("SELECT * FROM t WHERE x IN ('a', $$b$$) -- comment");
        // comma between 'a' and $$b$$ should be code
        let commas: Vec<_> = ctxs
            .iter()
            .filter(|c| c.byte == b',' && c.is_code())
            .collect();
        assert_eq!(commas.len(), 1);
        // 'b' inside $$ should be in dollar quote
        let b_ctx = ctxs.iter().find(|c| c.byte == b'b').unwrap();
        assert!(b_ctx.in_dollar_quote);
    }

    #[test]
    fn test_placeholder_dollar_not_confused_with_dollar_quote() {
        // $1 should NOT open a dollar-quote
        let ctxs = collect_contexts("SELECT $1, $2 FROM t");
        assert!(ctxs.iter().all(|c| !c.in_dollar_quote));
    }
}
