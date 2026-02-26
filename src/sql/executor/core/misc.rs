//! Misc executor helpers

use crate::sql::scanner::SqlCharScanner;

pub(crate) fn starts_with_ignore_ascii_case(haystack: &str, prefix: &str) -> bool {
    let haystack = haystack.as_bytes();
    let prefix = prefix.as_bytes();
    haystack
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
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

    let mut create_procedure_scan_state = CreateProcedureScanState::Start;
    let mut create_procedure_saw_as = false;
    let mut create_procedure_begin_depth = 0usize;
    let mut create_procedure_case_depth = 0usize;

    let mut start = 0usize;
    // Tracks the end of the last word we extracted, so we skip over its
    // remaining bytes as the scanner yields them one-by-one.
    let mut word_end = 0usize;

    for ctx in SqlCharScanner::new(sql) {
        if ctx.in_string() || ctx.in_comment() {
            continue;
        }

        // Skip bytes that are part of a word we already processed.
        if ctx.pos < word_end {
            continue;
        }

        let b = ctx.byte;

        // Extract full word tokens for the CREATE PROCEDURE FSM.
        if b.is_ascii_alphabetic() {
            let token_start = ctx.pos;
            let mut j = ctx.pos + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            word_end = j;
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
            } else if token.eq_ignore_ascii_case(b"CASE") {
                create_procedure_case_depth += 1;
            } else if token.eq_ignore_ascii_case(b"BEGIN") {
                create_procedure_begin_depth += 1;
            } else if token.eq_ignore_ascii_case(b"END") {
                if create_procedure_case_depth > 0 {
                    create_procedure_case_depth -= 1;
                } else {
                    create_procedure_begin_depth = create_procedure_begin_depth.saturating_sub(1);
                }
            }

            continue;
        }

        if b == b';' {
            if create_procedure_begin_depth > 0 {
                continue;
            }
            statements.push(&sql[start..ctx.pos]);
            start = ctx.pos + 1;
            create_procedure_scan_state = CreateProcedureScanState::Start;
            create_procedure_saw_as = false;
            create_procedure_begin_depth = 0;
            create_procedure_case_depth = 0;
        }
    }

    statements.push(&sql[start..]);
    statements
}

pub(super) fn get_skip_reason(sql_upper: &str) -> Option<String> {
    crate::sql::raw_sql::skip_reason(sql_upper).map(|reason| reason.to_string())
}

pub(super) fn get_unsupported_reason(sql_upper: &str) -> Option<String> {
    crate::sql::raw_sql::unsupported_reason(sql_upper).map(|reason| reason.to_string())
}
