use pgwire::api::Type;

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

pub(in crate::protocol::handler) fn infer_parameter_types(
    sql: &str,
    param_count: usize,
) -> Vec<Type> {
    // Default to TEXT: drivers can encode any value to TEXT, server does implicit conversion.
    // UNKNOWN (OID 705) breaks pgx/GORM which cannot encode time.Time to unknown type.
    let mut types = vec![Type::TEXT; param_count];
    if param_count == 0 {
        return types;
    }

    // Use ASCII-only case normalization to keep byte offsets stable. Full Unicode uppercasing can
    // change byte length and make `pos` invalid for slicing, potentially panicking on non-ASCII SQL.
    let sql_upper = sql.to_ascii_uppercase();
    let bytes = sql.as_bytes();

    let mut placeholder_positions: Vec<(usize, usize)> = Vec::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut dollar_delim: Option<Vec<u8>> = None;

    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(delim) = dollar_delim.as_ref() {
            let delim_len = delim.len();
            if i + delim_len <= bytes.len() && &bytes[i..i + delim_len] == delim.as_slice() {
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

        if !in_single_quote && !in_double_quote && b == b'$' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                if let Ok(num) = std::str::from_utf8(&bytes[i + 1..j])
                    .unwrap_or("0")
                    .parse::<usize>()
                {
                    placeholder_positions.push((i, num));
                }
                i = j;
                continue;
            }

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

    for (pos, param_num) in placeholder_positions {
        if param_num == 0 || param_num > param_count {
            continue;
        }
        let param_idx = param_num - 1;

        let before = &sql_upper[..pos];
        let before_trimmed = before.trim_end();

        if before_trimmed.ends_with("LIMIT") {
            types[param_idx] = Type::INT8;
            continue;
        }

        if before_trimmed.ends_with("OFFSET") {
            types[param_idx] = Type::INT8;
            continue;
        }

        if before_trimmed.ends_with("FIRST") || before_trimmed.ends_with("NEXT") {
            let keyword_start = if before_trimmed.ends_with("FIRST") {
                before_trimmed.len().saturating_sub(5)
            } else {
                before_trimmed.len().saturating_sub(4)
            };
            let even_before = before_trimmed[..keyword_start].trim_end();
            if even_before.ends_with("FETCH") {
                types[param_idx] = Type::INT8;
                continue;
            }
        }
    }

    types
}

fn is_ident_char(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
}

pub(super) fn is_ident_char_or_dollar(b: u8) -> bool {
    is_ident_char(b) || b == b'$'
}

#[allow(dead_code)]
pub(in crate::protocol::handler) fn find_keyword_outside_strings(
    query: &str,
    keyword: &str,
) -> Option<usize> {
    let bytes = query.as_bytes();
    let kw = keyword.as_bytes();
    if kw.is_empty() || bytes.len() < kw.len() {
        return None;
    }

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

        if !in_single_quote && !in_double_quote && b == b'$' {
            // Skip placeholders like $1 and keep scanning.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                i = j;
                continue;
            }

            // Track dollar-quoted strings ($tag$...$tag$ or $$...$$)
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

        if !in_single_quote && !in_double_quote && i + kw.len() <= bytes.len() {
            let before_ok = i == 0 || !is_ident_char(bytes[i - 1]);
            let after_ok = i + kw.len() == bytes.len() || !is_ident_char(bytes[i + kw.len()]);
            if before_ok && after_ok {
                let mut matched = true;
                for (j, kw_b) in kw.iter().enumerate() {
                    if bytes[i + j].to_ascii_uppercase() != kw_b.to_ascii_uppercase() {
                        matched = false;
                        break;
                    }
                }
                if matched {
                    return Some(i);
                }
            }
        }

        i += 1;
    }

    None
}

#[cfg(test)]
pub(in crate::protocol::handler) fn replace_placeholders_for_inference(query: &str) -> String {
    let mut result = String::with_capacity(query.len());
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut dollar_delim: Option<Vec<char>> = None;
    let chars: Vec<char> = query.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if let Some(ref delim) = dollar_delim {
            if i + delim.len() <= chars.len() && chars[i..i + delim.len()] == delim[..] {
                result.extend(delim);
                i += delim.len();
                dollar_delim = None;
            } else {
                result.push(chars[i]);
                i += 1;
            }
            continue;
        }

        let c = chars[i];

        if c == '\'' && !in_double_quote {
            if in_single_quote && i + 1 < chars.len() && chars[i + 1] == '\'' {
                result.push('\'');
                result.push('\'');
                i += 2;
                continue;
            }
            in_single_quote = !in_single_quote;
            result.push(c);
            i += 1;
            continue;
        } else if c == '"' && !in_single_quote {
            if in_double_quote && i + 1 < chars.len() && chars[i + 1] == '"' {
                result.push('"');
                result.push('"');
                i += 2;
                continue;
            }
            in_double_quote = !in_double_quote;
            result.push(c);
            i += 1;
            continue;
        }

        if !in_single_quote && !in_double_quote && c == '$' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 {
                result.push('1');
                i = j;
                continue;
            }

            // Handle PostgreSQL dollar-quoted strings ($tag$ ... $tag$ or $$ ... $$)
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '$' {
                if !(chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                    break;
                }
                j += 1;
            }
            if j < chars.len() && chars[j] == '$' {
                let delim: Vec<char> = chars[i..=j].to_vec();
                result.extend(&delim);
                dollar_delim = Some(delim);
                i = j + 1;
                continue;
            }
        }

        result.push(c);
        i += 1;
    }

    result
}
