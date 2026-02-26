use crate::sql::scanner::SqlCharScanner;

/// Count the number of parameter placeholders ($1, $2, ...) in a SQL query.
/// Returns the maximum placeholder number found, which indicates how many parameters are expected.
pub(in crate::protocol::handler) fn count_sql_parameters(sql: &str) -> usize {
    let bytes = sql.as_bytes();
    let mut max_param = 0usize;

    for ctx in SqlCharScanner::new(sql) {
        if ctx.in_string() || ctx.in_comment() {
            continue;
        }

        // Look for parameter placeholders: $1, $2, ...
        if ctx.byte == b'$' {
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
            if saw_digit {
                let before_ok = i == 0 || !is_ident_char_or_dollar(bytes[i - 1]);
                let after_ok = j == bytes.len() || !is_ident_char_or_dollar(bytes[j]);
                if before_ok && after_ok {
                    max_param = max_param.max(num);
                }
            }
        }
    }

    max_param
}

pub(super) fn is_ident_char_or_dollar(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_') || b == b'$'
}
