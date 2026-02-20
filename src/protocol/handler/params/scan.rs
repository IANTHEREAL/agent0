/// Count the number of parameter placeholders ($1, $2, ...) in a SQL query.
/// Returns the maximum placeholder number found, which indicates how many parameters are expected.
pub(in crate::protocol::handler) fn count_sql_parameters(sql: &str) -> usize {
    let bytes = sql.as_bytes();
    let mut max_param = 0usize;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut dollar_delim: Option<Vec<u8>> = None;

    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(delim) = dollar_delim.as_ref() {
            let delim_len = delim.len();
            let matches =
                i + delim_len <= bytes.len() && &bytes[i..i + delim_len] == delim.as_slice();
            if matches {
                dollar_delim = None;
                i += delim_len;
            } else {
                i += 1;
            }
            continue;
        }

        let b = bytes[i];

        if b == b'\'' && !in_double_quote {
            if in_single_quote && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                i += 2;
                continue;
            }
            in_single_quote = !in_single_quote;
            i += 1;
            continue;
        }
        if b == b'"' && !in_single_quote {
            if in_double_quote && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                i += 2;
                continue;
            }
            in_double_quote = !in_double_quote;
            i += 1;
            continue;
        }

        if !in_single_quote && !in_double_quote {
            // SQL comments
            if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                i += 2;
                let mut depth = 1usize;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                        continue;
                    }
                    i += 1;
                }
                continue;
            }
        }

        if !in_single_quote && !in_double_quote && b == b'$' {
            // Prepared-statement placeholder: $1, $2, ...
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
                    i = j;
                    continue;
                }
            }

            // PostgreSQL dollar-quoted strings ($tag$...$tag$ or $$...$$)
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'$' {
                if !(bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    break;
                }
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'$' {
                dollar_delim = Some(bytes[i..=j].to_vec());
                i = j + 1;
                continue;
            }
        }

        i += 1;
    }

    max_param
}

pub(super) fn is_ident_char_or_dollar(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_') || b == b'$'
}
