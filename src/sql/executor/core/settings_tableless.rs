//! Tableless `set_config()` / `current_setting()` fast paths

use super::{
    normalize_ident, normalize_search_path_entries, parse_search_path_guc_value,
    try_parse_const_bool, try_parse_const_text, DataType, ExecuteResult, Expr, FunctionArg,
    FunctionArgExpr, Query, Row, SelectItem, Session, SetExpr, Value,
};
use crate::session_context;
use crate::sql::error::SqlError;
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

pub(super) fn try_execute_set_config_select(
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
    let Some(_is_local) = try_parse_const_bool(local_expr) else {
        return Ok(None);
    };

    let var_name = var_name.to_lowercase();
    if var_name == "search_path" {
        let parsed = parse_search_path_guc_value(&new_value);
        let new_search_path = normalize_search_path_entries(parsed)?;
        session.set_search_path(new_search_path);
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

    if session.set_known_setting(&var_name, new_value)? {
        let current = session
            .show_setting_value(&var_name)
            .unwrap_or_else(|| "".to_string());
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
