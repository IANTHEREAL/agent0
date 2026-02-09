//! Misc executor helpers

pub(super) fn starts_with_ignore_ascii_case(haystack: &str, prefix: &str) -> bool {
    let haystack = haystack.as_bytes();
    let prefix = prefix.as_bytes();
    haystack
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn is_ident_char_or_dollar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// Split a SQL string into top-level statements by semicolons.
///
/// Semicolons inside quoted strings, dollar-quoted strings, comments, or `CREATE PROCEDURE ... AS BEGIN ... END`
/// bodies are ignored.
pub(super) fn split_sql_statements(sql: &str) -> Vec<&str> {
    #[derive(Copy, Clone, PartialEq, Eq)]
    enum CreateProcedureScanState {
        Start,
        SawCreate,
        SawCreateOr,
        SawCreateOrReplace,
        SawCreateProcedure,
        Other,
    }

    let bytes = sql.as_bytes();
    let mut statements = Vec::new();

    let mut in_single_quote = false;
    let mut in_escape_single_quote = false;
    let mut in_double_quote = false;
    let mut dollar_delim: Option<Vec<u8>> = None;
    let mut in_line_comment = false;
    let mut block_comment_depth = 0usize;

    let mut create_procedure_scan_state = CreateProcedureScanState::Start;
    let mut create_procedure_saw_as = false;
    let mut create_procedure_begin_depth = 0usize;
    let mut create_procedure_case_depth = 0usize;

    let mut start = 0usize;
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

        if in_line_comment {
            if bytes[i] == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }

        if block_comment_depth > 0 {
            if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                block_comment_depth += 1;
                i += 2;
                continue;
            }
            if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                block_comment_depth -= 1;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        let b = bytes[i];

        if b == b'\'' && !in_double_quote {
            if in_single_quote {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                if in_escape_single_quote {
                    let mut backslash_count = 0usize;
                    let mut k = i;
                    while k > 0 && bytes[k - 1] == b'\\' {
                        backslash_count += 1;
                        k -= 1;
                    }
                    if backslash_count % 2 == 1 {
                        i += 1;
                        continue;
                    }
                }
                in_single_quote = false;
                in_escape_single_quote = false;
            } else {
                in_single_quote = true;
                in_escape_single_quote = i > 0
                    && matches!(bytes[i - 1], b'e' | b'E')
                    && (i == 1 || !is_ident_char_or_dollar(bytes[i - 2]));
            }
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
            if b.is_ascii_alphabetic() {
                let token_start = i;
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                let token = &bytes[token_start..j];

                if create_procedure_begin_depth == 0 {
                    match create_procedure_scan_state {
                        CreateProcedureScanState::Start => {
                            if token.eq_ignore_ascii_case(b"CREATE") {
                                create_procedure_scan_state = CreateProcedureScanState::SawCreate;
                            } else {
                                create_procedure_scan_state = CreateProcedureScanState::Other;
                            }
                        }
                        CreateProcedureScanState::SawCreate => {
                            if token.eq_ignore_ascii_case(b"OR") {
                                create_procedure_scan_state = CreateProcedureScanState::SawCreateOr;
                            } else if token.eq_ignore_ascii_case(b"PROCEDURE") {
                                create_procedure_scan_state =
                                    CreateProcedureScanState::SawCreateProcedure;
                            } else {
                                create_procedure_scan_state = CreateProcedureScanState::Other;
                            }
                        }
                        CreateProcedureScanState::SawCreateOr => {
                            if token.eq_ignore_ascii_case(b"REPLACE") {
                                create_procedure_scan_state =
                                    CreateProcedureScanState::SawCreateOrReplace;
                            } else {
                                create_procedure_scan_state = CreateProcedureScanState::Other;
                            }
                        }
                        CreateProcedureScanState::SawCreateOrReplace => {
                            if token.eq_ignore_ascii_case(b"PROCEDURE") {
                                create_procedure_scan_state =
                                    CreateProcedureScanState::SawCreateProcedure;
                            } else {
                                create_procedure_scan_state = CreateProcedureScanState::Other;
                            }
                        }
                        CreateProcedureScanState::SawCreateProcedure
                        | CreateProcedureScanState::Other => {}
                    }

                    if create_procedure_scan_state == CreateProcedureScanState::SawCreateProcedure {
                        if create_procedure_saw_as {
                            if token.eq_ignore_ascii_case(b"BEGIN") {
                                create_procedure_begin_depth = 1;
                                create_procedure_case_depth = 0;
                            }
                            create_procedure_saw_as = false;
                        } else if token.eq_ignore_ascii_case(b"AS") {
                            create_procedure_saw_as = true;
                        }
                    }
                } else {
                    if token.eq_ignore_ascii_case(b"CASE") {
                        create_procedure_case_depth += 1;
                    } else if token.eq_ignore_ascii_case(b"BEGIN") {
                        create_procedure_begin_depth += 1;
                    } else if token.eq_ignore_ascii_case(b"END") {
                        if create_procedure_case_depth > 0 {
                            create_procedure_case_depth -= 1;
                        } else {
                            create_procedure_begin_depth =
                                create_procedure_begin_depth.saturating_sub(1);
                        }
                    }
                }

                i = j;
                continue;
            }

            if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
                in_line_comment = true;
                i += 2;
                continue;
            }
            if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                block_comment_depth = 1;
                i += 2;
                continue;
            }

            if b == b'$' {
                let before_ok = i == 0 || !is_ident_char_or_dollar(bytes[i - 1]);

                // Prepared-statement placeholder: $1, $2, ...
                if before_ok {
                    let mut j = i + 1;
                    let mut saw_digit = false;
                    while j < bytes.len() && bytes[j].is_ascii_digit() && j - i <= 10 {
                        saw_digit = true;
                        j += 1;
                    }
                    if saw_digit {
                        let after_ok = j == bytes.len() || !is_ident_char_or_dollar(bytes[j]);
                        if after_ok {
                            i = j;
                            continue;
                        }
                    }
                }

                // PostgreSQL dollar-quoted strings ($tag$...$tag$ or $$...$$)
                if before_ok {
                    let mut j = i + 1;
                    if j < bytes.len() && bytes[j] == b'$' {
                        dollar_delim = Some(b"$$".to_vec());
                        i += 2;
                        continue;
                    }

                    if j < bytes.len()
                        && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'_' || bytes[j] >= 0x80)
                    {
                        j += 1;
                        while j < bytes.len() && bytes[j] != b'$' {
                            if bytes[j].is_ascii_alphanumeric()
                                || bytes[j] == b'_'
                                || bytes[j] >= 0x80
                            {
                                j += 1;
                                continue;
                            }
                            break;
                        }
                        if j < bytes.len() && bytes[j] == b'$' {
                            dollar_delim = Some(bytes[i..=j].to_vec());
                            i = j + 1;
                            continue;
                        }
                    }
                }
            }

            if b == b';' {
                if create_procedure_begin_depth > 0 {
                    i += 1;
                    continue;
                }
                statements.push(&sql[start..i]);
                start = i + 1;
                create_procedure_scan_state = CreateProcedureScanState::Start;
                create_procedure_saw_as = false;
                create_procedure_begin_depth = 0;
                create_procedure_case_depth = 0;
                i += 1;
                continue;
            }
        }

        i += 1;
    }

    statements.push(&sql[start..]);
    statements
}

pub(super) fn get_skip_reason(sql_upper: &str) -> Option<String> {
    if sql_upper.starts_with('\\') {
        return Some("psql meta-command not supported".into());
    }
    if sql_upper.starts_with("COPY ") || sql_upper.contains(" FROM STDIN") {
        return Some("COPY not supported".into());
    }
    None
}

pub(super) fn get_unsupported_reason(sql_upper: &str) -> Option<String> {
    if sql_upper.starts_with("CREATE DOMAIN") {
        return Some("CREATE DOMAIN not supported".into());
    }
    if sql_upper.starts_with("CREATE AGGREGATE") {
        return Some("CREATE AGGREGATE not supported".into());
    }
    if sql_upper.starts_with("ALTER TYPE") {
        return Some("ALTER TYPE not supported".into());
    }
    if sql_upper.starts_with("ALTER DOMAIN") {
        return Some("ALTER DOMAIN not supported".into());
    }
    if sql_upper.starts_with("ALTER AGGREGATE") {
        return Some("ALTER AGGREGATE not supported".into());
    }
    if sql_upper.starts_with("ALTER FUNCTION") {
        if sql_upper.contains(" OWNER TO ") {
            return None;
        }
        return Some("ALTER FUNCTION not supported".into());
    }
    if sql_upper.starts_with("ALTER SEQUENCE") {
        if sql_upper.contains(" OWNER TO ") || sql_upper.contains(" OWNED BY ") {
            return None;
        }
        return Some("ALTER SEQUENCE not supported".into());
    }
    None
}
