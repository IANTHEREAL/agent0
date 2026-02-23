//! GUC/SET handling: SET variable and SHOW ALL.

use super::super::*;

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
            return Ok(vec![
                ExecuteResult::Notice {
                    message: "SET LOCAL can only be used in transaction blocks".to_string(),
                    severity: "WARNING".to_string(),
                },
                ExecuteResult::CommandComplete { tag: "SET" },
            ]);
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
            return Ok(vec![
                ExecuteResult::Notice {
                    message: "SET LOCAL can only be used in transaction blocks".to_string(),
                    severity: "WARNING".to_string(),
                },
                ExecuteResult::CommandComplete { tag: "SET" },
            ]);
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
