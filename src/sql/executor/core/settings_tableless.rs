//! Tableless `set_config()` / `current_setting()` fast paths

use super::{
    check_reserved_guc_write, normalize_ident, normalize_search_path_entries,
    parse_search_path_guc_value, session_auth_different_user_error, try_parse_const_bool,
    try_parse_const_text, DataType, ExecuteResult, Expr, FunctionArg, FunctionArgExpr, Query, Row,
    SelectItem, Session, SetExpr, Value,
};
use crate::session_context;
use crate::sql::error::SqlError;
use crate::sql::session::SessionSettings;
use anyhow::{anyhow, Result};

pub(super) fn unwrap_top_level_cast<'a>(
    mut expr: &'a Expr,
) -> (&'a Expr, Option<&'a sqlparser::ast::DataType>) {
    let mut cast_to: Option<&'a sqlparser::ast::DataType> = None;
    loop {
        match expr {
            Expr::Cast {
                expr: inner,
                data_type,
                ..
            }
            | Expr::TryCast {
                expr: inner,
                data_type,
                ..
            }
            | Expr::SafeCast {
                expr: inner,
                data_type,
                ..
            } => {
                cast_to = Some(data_type);
                expr = inner.as_ref();
            }
            Expr::Nested(inner) => {
                expr = inner.as_ref();
            }
            _ => break,
        }
    }
    (expr, cast_to)
}

pub(super) fn cast_current_setting_value(value: Value, target_type: &DataType) -> Result<Value> {
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }

    let Value::Text(s) = value else {
        return Ok(value);
    };

    match target_type {
        DataType::Text => Ok(Value::Text(s)),
        DataType::Int32 => {
            let v: i32 = s.parse().map_err(|_| SqlError::InvalidInputSyntax {
                type_name: "integer".into(),
                value: s.clone(),
            })?;
            Ok(Value::Int32(v))
        }
        DataType::Int64 => {
            let v: i64 = s.parse().map_err(|_| SqlError::InvalidInputSyntax {
                type_name: "bigint".into(),
                value: s.clone(),
            })?;
            Ok(Value::Int64(v))
        }
        DataType::Boolean => {
            let v = s.as_str();
            if v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on") {
                Ok(Value::Boolean(true))
            } else if v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off") {
                Ok(Value::Boolean(false))
            } else {
                Err(SqlError::InvalidInputSyntax {
                    type_name: "boolean".into(),
                    value: s.clone(),
                }
                .into())
            }
        }
        _ => Ok(Value::Text(s)),
    }
}

pub(super) fn is_set_config_function(name: &sqlparser::ast::ObjectName) -> bool {
    match name.0.as_slice() {
        [ident] => ident.value.eq_ignore_ascii_case("set_config"),
        [schema, ident] => {
            schema.value.eq_ignore_ascii_case("pg_catalog")
                && ident.value.eq_ignore_ascii_case("set_config")
        }
        _ => false,
    }
}

pub(super) fn is_current_setting_function(name: &sqlparser::ast::ObjectName) -> bool {
    match name.0.as_slice() {
        [ident] => ident.value.eq_ignore_ascii_case("current_setting"),
        [schema, ident] => {
            schema.value.eq_ignore_ascii_case("pg_catalog")
                && ident.value.eq_ignore_ascii_case("current_setting")
        }
        _ => false,
    }
}

pub(super) async fn try_execute_set_config_select(
    session: &mut Session,
    query: &Query,
) -> Result<Option<ExecuteResult>> {
    if query.with.is_some() {
        return Ok(None);
    }
    if !query.locks.is_empty() || query.for_clause.is_some() {
        return Ok(None);
    }

    let SetExpr::Select(select) = query.body.as_ref() else {
        return Ok(None);
    };
    if select.into.is_some() || !select.lateral_views.is_empty() || !select.from.is_empty() {
        return Ok(None);
    }
    if select.projection.len() != 1 {
        return Ok(None);
    }

    let (expr, alias) = match &select.projection[0] {
        SelectItem::UnnamedExpr(expr) => (expr, None),
        SelectItem::ExprWithAlias { expr, alias } => (expr, Some(normalize_ident(alias))),
        _ => return Ok(None),
    };

    let Expr::Function(func) = expr else {
        return Ok(None);
    };
    if func.over.is_some()
        || func.filter.is_some()
        || func.distinct
        || !func.order_by.is_empty()
        || !is_set_config_function(&func.name)
    {
        return Ok(None);
    }

    let [a0, a1, a2] = func.args.as_slice() else {
        return Ok(None);
    };
    let FunctionArg::Unnamed(FunctionArgExpr::Expr(var_expr)) = a0 else {
        return Ok(None);
    };
    let FunctionArg::Unnamed(FunctionArgExpr::Expr(val_expr)) = a1 else {
        return Ok(None);
    };
    let FunctionArg::Unnamed(FunctionArgExpr::Expr(local_expr)) = a2 else {
        return Ok(None);
    };

    let Some(var_name) = try_parse_const_text(var_expr) else {
        return Ok(None);
    };
    let Some(new_value) = try_parse_const_text(val_expr) else {
        return Ok(None);
    };
    let Some(is_local) = try_parse_const_bool(local_expr) else {
        return Ok(None);
    };

    let var_name = var_name.to_lowercase();
    check_reserved_guc_write(&var_name)?;
    if var_name == "session_authorization" {
        let session_user = session.session_user().unwrap_or("postgres");
        if new_value == session_user {
            // Same-user set is a no-op; return the current value.
            let current = session
                .show_setting_value("session_authorization")
                .unwrap_or_else(|| "postgres".to_string());
            return Ok(Some(ExecuteResult::Select {
                columns: vec![alias.unwrap_or_else(|| "set_config".to_string())],
                column_types: Some(vec![DataType::Text]),
                rows: vec![Row::new(vec![Value::Text(current)])],
                timezone: session_context::current_timezone(),
            }));
        }
        // Different user: produce PG-parity error (role-not-found vs permission-denied).
        return Err(session_auth_different_user_error(&new_value, &session.store).await);
    }
    if var_name == "search_path" {
        let parsed = parse_search_path_guc_value(&new_value);
        let new_search_path = normalize_search_path_entries(parsed)?;
        if is_local && !session.is_in_transaction() {
            let display_value = SessionSettings::format_search_path_show(&new_search_path);
            return Ok(Some(ExecuteResult::Select {
                columns: vec![alias.unwrap_or_else(|| "set_config".to_string())],
                column_types: Some(vec![DataType::Text]),
                rows: vec![Row::new(vec![Value::Text(display_value)])],
                timezone: session_context::current_timezone(),
            }));
        }

        if is_local {
            session.set_local_search_path(new_search_path);
        } else {
            session.set_search_path(new_search_path);
        }
        let current = session
            .show_setting_value("search_path")
            .unwrap_or_else(|| "public".to_string());

        return Ok(Some(ExecuteResult::Select {
            columns: vec![alias.unwrap_or_else(|| "set_config".to_string())],
            column_types: Some(vec![DataType::Text]),
            // PostgreSQL set_config() returns the newly set value.
            rows: vec![Row::new(vec![Value::Text(current)])],
            timezone: session_context::current_timezone(),
        }));
    }

    if is_local && !session.is_in_transaction() {
        let display_value = SessionSettings::validate_and_normalize_value(&var_name, &new_value)?;
        return Ok(Some(ExecuteResult::Select {
            columns: vec![alias.unwrap_or_else(|| "set_config".to_string())],
            column_types: Some(vec![DataType::Text]),
            rows: vec![Row::new(vec![Value::Text(display_value)])],
            timezone: session_context::current_timezone(),
        }));
    }

    let applied = if is_local {
        session.set_local_setting(&var_name, new_value)?
    } else {
        session.set_known_setting(&var_name, new_value)?
    };

    if applied {
        let current = session.show_setting_value(&var_name).unwrap_or_default();
        return Ok(Some(ExecuteResult::Select {
            columns: vec![alias.unwrap_or_else(|| "set_config".to_string())],
            column_types: Some(vec![DataType::Text]),
            // PostgreSQL set_config() returns the newly set value.
            rows: vec![Row::new(vec![Value::Text(current)])],
            timezone: session_context::current_timezone(),
        }));
    }

    Ok(None)
}

pub(super) fn try_execute_current_setting_select(
    session: &mut Session,
    query: &Query,
) -> Result<Option<ExecuteResult>> {
    if query.with.is_some() {
        return Ok(None);
    }
    if !query.locks.is_empty() || query.for_clause.is_some() {
        return Ok(None);
    }

    let SetExpr::Select(select) = query.body.as_ref() else {
        return Ok(None);
    };
    if select.into.is_some() || !select.lateral_views.is_empty() || !select.from.is_empty() {
        return Ok(None);
    }
    if select.projection.len() != 1 {
        return Ok(None);
    }

    let (expr, alias) = match &select.projection[0] {
        SelectItem::UnnamedExpr(expr) => (expr, None),
        SelectItem::ExprWithAlias { expr, alias } => (expr, Some(normalize_ident(alias))),
        _ => return Ok(None),
    };

    let (expr, cast_to) = unwrap_top_level_cast(expr);

    let Expr::Function(func) = expr else {
        return Ok(None);
    };
    if func.over.is_some()
        || func.filter.is_some()
        || func.distinct
        || !func.order_by.is_empty()
        || !is_current_setting_function(&func.name)
    {
        return Ok(None);
    }

    let (var_expr, missing_ok_expr) = match func.args.as_slice() {
        [a0] => (a0, None),
        [a0, a1] => (a0, Some(a1)),
        _ => return Ok(None),
    };

    let FunctionArg::Unnamed(FunctionArgExpr::Expr(var_expr)) = var_expr else {
        return Ok(None);
    };
    let Some(var_name) = try_parse_const_text(var_expr) else {
        return Ok(None);
    };
    let missing_ok = match missing_ok_expr {
        None => false,
        Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(e))) => {
            let Some(v) = try_parse_const_bool(e) else {
                return Ok(None);
            };
            v
        }
        _ => return Ok(None),
    };

    let var_name = var_name.to_lowercase();
    let value = match session.show_setting_value(&var_name) {
        Some(v) => Value::Text(v),
        None if missing_ok => Value::Null,
        None => {
            return Err(anyhow!(
                "unrecognized configuration parameter \"{}\"",
                var_name
            ))
        }
    };

    let mut output_type = DataType::Text;
    if let Some(cast_to) = cast_to {
        if let Ok(t) = crate::sql::types::sql_datatype_to_internal_strict(cast_to) {
            output_type = t;
        } else {
            return Ok(None);
        }
    }
    let value = cast_current_setting_value(value, &output_type)?;

    Ok(Some(ExecuteResult::Select {
        columns: vec![alias.unwrap_or_else(|| "current_setting".to_string())],
        column_types: Some(vec![output_type]),
        rows: vec![Row::new(vec![value])],
        timezone: session_context::current_timezone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::parse_sql;
    use crate::storage::TikvStore;

    fn parse_query(sql: &str) -> Query {
        let mut stmts = parse_sql(sql).expect("parse sql");
        let stmt = stmts.remove(0);
        let sqlparser::ast::Statement::Query(q) = stmt else {
            panic!("expected query");
        };
        *q
    }

    fn make_session() -> Session {
        let store = TikvStore::new_stub();
        let observability = crate::observability::registry().tenant("settings_tableless_tests");
        Session::new_with_database(store, observability, 1, 1, "postgres".to_string(), 0, 0)
            .unwrap()
    }

    #[test]
    fn unwrap_top_level_cast_strips_nested_cast_layers() {
        let expr = Expr::Cast {
            expr: Box::new(Expr::TryCast {
                expr: Box::new(Expr::Nested(Box::new(Expr::Value(
                    sqlparser::ast::Value::SingleQuotedString("x".to_string()),
                )))),
                data_type: sqlparser::ast::DataType::Text,
                format: None,
            }),
            data_type: sqlparser::ast::DataType::Int(None),
            format: None,
        };

        let (inner, cast_to) = unwrap_top_level_cast(&expr);
        assert!(matches!(inner, Expr::Value(_)));
        assert!(cast_to.is_some());
    }

    #[test]
    fn cast_current_setting_value_parses_common_types_and_errors() {
        assert_eq!(
            cast_current_setting_value(Value::Text("42".to_string()), &DataType::Int32).unwrap(),
            Value::Int32(42)
        );
        assert_eq!(
            cast_current_setting_value(Value::Text("43".to_string()), &DataType::Int64).unwrap(),
            Value::Int64(43)
        );
        assert_eq!(
            cast_current_setting_value(Value::Text("on".to_string()), &DataType::Boolean).unwrap(),
            Value::Boolean(true)
        );
        assert!(
            cast_current_setting_value(Value::Text("abc".to_string()), &DataType::Boolean).is_err()
        );
    }

    #[test]
    fn function_name_detection_accepts_pg_catalog_and_bare_names() {
        let set_name = sqlparser::ast::ObjectName(vec![sqlparser::ast::Ident::new("set_config")]);
        let set_qualified = sqlparser::ast::ObjectName(vec![
            sqlparser::ast::Ident::new("pg_catalog"),
            sqlparser::ast::Ident::new("set_config"),
        ]);
        assert!(is_set_config_function(&set_name));
        assert!(is_set_config_function(&set_qualified));

        let current_name =
            sqlparser::ast::ObjectName(vec![sqlparser::ast::Ident::new("current_setting")]);
        let current_qualified = sqlparser::ast::ObjectName(vec![
            sqlparser::ast::Ident::new("pg_catalog"),
            sqlparser::ast::Ident::new("current_setting"),
        ]);
        assert!(is_current_setting_function(&current_name));
        assert!(is_current_setting_function(&current_qualified));
    }

    #[tokio::test]
    async fn set_config_fast_path_returns_select_result() {
        let mut session = make_session();
        let query = parse_query("SELECT set_config('statement_timeout', '1000', false)");
        let result = try_execute_set_config_select(&mut session, &query)
            .await
            .unwrap()
            .expect("should fast-path");

        let ExecuteResult::Select { rows, .. } = result else {
            panic!("expected select result");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values.len(), 1);
        assert_eq!(
            session.show_setting_value("statement_timeout").as_deref(),
            Some("1000ms")
        );
    }

    #[test]
    fn current_setting_fast_path_handles_present_and_missing_ok() {
        let mut session = make_session();
        session
            .set_known_setting("statement_timeout", "2500".to_string())
            .unwrap();

        let query = parse_query("SELECT current_setting('statement_timeout')");
        let result = try_execute_current_setting_select(&mut session, &query)
            .unwrap()
            .expect("should fast-path");
        let ExecuteResult::Select { rows, .. } = result else {
            panic!("expected select");
        };
        assert_eq!(rows[0].values, vec![Value::Text("2500ms".to_string())]);

        let query = parse_query("SELECT current_setting('no_such_setting', true)");
        let result = try_execute_current_setting_select(&mut session, &query)
            .unwrap()
            .expect("should fast-path with missing_ok");
        let ExecuteResult::Select { rows, .. } = result else {
            panic!("expected select");
        };
        assert_eq!(rows[0].values, vec![Value::Null]);
    }

    #[tokio::test]
    async fn set_config_rejects_is_superuser() {
        let mut session = make_session();
        let query = parse_query("SELECT set_config('is_superuser', 'on', false)");
        let err = try_execute_set_config_select(&mut session, &query)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("parameter \"is_superuser\" cannot be changed"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn set_config_allows_session_authorization_same_user() {
        let mut session = make_session();
        // Session user is "postgres" (from make_session's database arg).
        let query = parse_query("SELECT set_config('session_authorization', 'postgres', false)");
        let result = try_execute_set_config_select(&mut session, &query)
            .await
            .unwrap()
            .expect("should fast-path for same-user session_authorization");
        let ExecuteResult::Select { rows, .. } = result else {
            panic!("expected select result");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values, vec![Value::Text("postgres".to_string())]);
    }

    #[tokio::test]
    async fn set_config_rejects_session_authorization_different_user() {
        let mut session = make_session();
        let query = parse_query("SELECT set_config('session_authorization', 'evil_user', false)");
        let err = try_execute_set_config_select(&mut session, &query)
            .await
            .unwrap_err()
            .to_string();
        // Stub store falls back to permission-denied (no TiKV client for role lookup).
        // Error must include the target role name (PG parity).
        assert!(
            err.contains("permission denied to set session authorization \"evil_user\""),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn set_config_rejects_session_authorization_mixed_case() {
        let mut session = make_session();
        // "Postgres" (capital P) must NOT match "postgres" — case-sensitive (PG parity).
        let query = parse_query("SELECT set_config('session_authorization', 'Postgres', false)");
        let err = try_execute_set_config_select(&mut session, &query)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("permission denied to set session authorization \"Postgres\""),
            "mixed-case must be rejected: {err}"
        );
    }

    #[test]
    fn current_setting_fast_path_errors_when_missing_and_not_missing_ok() {
        let mut session = make_session();
        let query = parse_query("SELECT current_setting('definitely_missing_setting')");
        let err = try_execute_current_setting_select(&mut session, &query)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unrecognized configuration parameter"));
    }
}
