//! GUC/SET handling: SET variable and SHOW ALL.

use super::super::*;

fn is_default_set_value(value: &[Expr]) -> bool {
    if value.len() != 1 {
        return false;
    }

    match &value[0] {
        Expr::Identifier(ident) => ident.value.eq_ignore_ascii_case("default"),
        Expr::CompoundIdentifier(idents) if idents.len() == 1 => {
            idents[0].value.eq_ignore_ascii_case("default")
        }
        Expr::Value(sqlparser::ast::Value::UnQuotedString(s)) => s.eq_ignore_ascii_case("default"),
        _ => false,
    }
}

fn set_local_outside_transaction_notice() -> Vec<ExecuteResult> {
    vec![
        ExecuteResult::Notice {
            message: "SET LOCAL can only be used in transaction blocks".to_string(),
            severity: "WARNING".to_string(),
        },
        ExecuteResult::CommandComplete { tag: "SET" },
    ]
}

/// Execute a `SET <variable>` statement synchronously.
pub(super) fn execute_set_variable(
    session: &mut Session,
    local: bool,
    variable: &sqlparser::ast::ObjectName,
    value: &[Expr],
) -> Result<Vec<ExecuteResult>> {
    let var_name = variable
        .0
        .iter()
        .map(normalize_ident)
        .collect::<Vec<_>>()
        .join(".")
        .to_lowercase();

    match check_reserved_guc_write(&var_name) {
        Ok(()) => {}
        Err(write_err) => {
            if is_default_set_value(value) {
                if local && !session.is_in_transaction() {
                    return Ok(set_local_outside_transaction_notice());
                }

                check_reserved_guc_reset(&var_name)?;
                session.reset_setting(&var_name);
                return Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }]);
            }
            return Err(write_err);
        }
    }

    if var_name == "search_path" {
        let mut new_search_path = Vec::new();
        for expr in value {
            match expr {
                Expr::Identifier(ident) => {
                    new_search_path.push(normalize_ident(ident));
                }
                Expr::CompoundIdentifier(idents) if idents.len() == 1 => {
                    new_search_path.push(normalize_ident(&idents[0]));
                }
                Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) => {
                    new_search_path.extend(parse_search_path_guc_value(s));
                }
                _ => {
                    return Err(anyhow!("Unsupported search_path value: {}", expr));
                }
            }
        }
        let new_search_path = normalize_search_path_entries(new_search_path)?;
        if local && !session.is_in_transaction() {
            return Ok(set_local_outside_transaction_notice());
        }

        if local {
            session.set_local_search_path(new_search_path);
        } else {
            session.set_search_path(new_search_path);
        }
    } else {
        let value = set_variable_value_to_string(value)?;
        if local && !session.is_in_transaction() {
            crate::sql::session::SessionSettings::validate_and_normalize_value(&var_name, &value)?;
            return Ok(set_local_outside_transaction_notice());
        }

        if local {
            session.set_local_setting(&var_name, value)?;
        } else {
            session.set_known_setting(&var_name, value)?;
        }
    }
    Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }])
}

/// Build a `SHOW ALL` result set: three columns (name, setting, description),
/// sorted alphabetically by name.
pub(super) fn build_show_all_result(session: &Session, timezone: Arc<str>) -> ExecuteResult {
    let all_settings = session.show_all_settings();
    let rows = all_settings
        .into_iter()
        .map(|(name, setting, description)| {
            Row::new(vec![
                Value::Text(name),
                Value::Text(setting),
                Value::Text(description),
            ])
        })
        .collect();
    ExecuteResult::Select {
        columns: vec![
            "name".to_string(),
            "setting".to_string(),
            "description".to_string(),
        ],
        column_types: Some(vec![DataType::Text, DataType::Text, DataType::Text]),
        rows,
        timezone,
    }
}

#[cfg(test)]
mod tests {
    use super::{build_show_all_result, execute_set_variable};
    use crate::model::DataType;
    use crate::sql::parse_sql;
    use crate::sql::{ExecuteResult, Session};
    use sqlparser::ast::{Expr, ObjectName, SetExpr, Statement, Value as SqlValue};

    fn make_session() -> Session {
        let store = crate::storage::TikvStore::new_stub();
        let obs = crate::observability::registry().tenant("dispatch_guc_tests");
        Session::new_with_database(store, obs, 1, 1, "postgres".to_string(), 0, 0)
    }

    fn parse_set(sql: &str) -> (bool, ObjectName, Vec<Expr>) {
        let mut stmts = parse_sql(sql).expect("parse");
        let stmt = stmts.remove(0);
        let Statement::SetVariable {
            local,
            variable,
            value,
            ..
        } = stmt
        else {
            panic!("expected set variable");
        };
        (local, variable, value)
    }

    #[test]
    fn execute_set_variable_updates_known_setting() {
        let mut session = make_session();
        let (local, variable, value) = parse_set("SET statement_timeout = 1500");
        let out = execute_set_variable(&mut session, local, &variable, &value).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(
            session.show_setting_value("statement_timeout").as_deref(),
            Some("1500ms")
        );
    }

    #[test]
    fn execute_set_variable_search_path_local_outside_txn_returns_warning_notice() {
        let mut session = make_session();
        let variable = ObjectName(vec![sqlparser::ast::Ident::new("search_path")]);
        let value = vec![Expr::Value(SqlValue::SingleQuotedString(
            "public, pg_catalog".to_string(),
        ))];

        let out = execute_set_variable(&mut session, true, &variable, &value).unwrap();
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0], ExecuteResult::Notice { .. }));
        assert!(matches!(
            out[1],
            ExecuteResult::CommandComplete { tag: "SET" }
        ));
    }

    #[test]
    fn execute_set_variable_rejects_unsupported_search_path_expr() {
        let mut session = make_session();
        let variable = ObjectName(vec![sqlparser::ast::Ident::new("search_path")]);
        let value = vec![Expr::BinaryOp {
            left: Box::new(Expr::Value(SqlValue::Number("1".to_string(), false))),
            op: sqlparser::ast::BinaryOperator::Plus,
            right: Box::new(Expr::Value(SqlValue::Number("2".to_string(), false))),
        }];

        let err = execute_set_variable(&mut session, false, &variable, &value)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unsupported search_path value"));
    }

    #[test]
    fn execute_set_variable_rejects_reserved_pseudo_guc_write() {
        let mut session = make_session();
        let (local, variable, value) = parse_set("SET session_authorization = 'evil_user'");
        let err = execute_set_variable(&mut session, local, &variable, &value)
            .unwrap_err()
            .to_string();
        assert!(err.contains("parameter \"session_authorization\" cannot be changed"));
    }

    #[test]
    fn execute_set_variable_allows_session_authorization_default_reset() {
        let mut session = make_session();
        let (local, variable, value) = parse_set("SET session_authorization TO DEFAULT");
        let out = execute_set_variable(&mut session, local, &variable, &value).unwrap();
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0],
            ExecuteResult::CommandComplete { tag: "SET" }
        ));
        assert_eq!(
            session
                .show_setting_value("session_authorization")
                .as_deref(),
            Some("postgres")
        );
    }

    #[test]
    fn build_show_all_result_emits_select_shape() {
        let mut session = make_session();
        let (local, variable, value) = parse_set("SET statement_timeout = 1000");
        let _ = execute_set_variable(&mut session, local, &variable, &value).unwrap();

        let out = build_show_all_result(&session, std::sync::Arc::from("UTC"));
        let ExecuteResult::Select {
            columns,
            column_types,
            rows,
            ..
        } = out
        else {
            panic!("expected select");
        };
        assert_eq!(columns, vec!["name", "setting", "description"]);
        assert_eq!(
            column_types,
            Some(vec![DataType::Text, DataType::Text, DataType::Text])
        );
        assert!(!rows.is_empty());
    }

    #[test]
    fn set_tableless_helpers_do_not_trigger_on_non_select_statement() {
        let mut stmts = parse_sql("VALUES (1)").expect("parse");
        let stmt = stmts.remove(0);
        let Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        assert!(!matches!(q.body.as_ref(), SetExpr::Select(_)));
    }
}
