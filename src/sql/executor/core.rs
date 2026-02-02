//! SQL executor

use super::super::ddl;
use super::super::dml;
use super::super::alter_owner;
use super::super::alter_sequence_owned_by;
use super::super::comment_on;
use super::triggers::strip_leading_sql_comments;
use super::super::explain;
use super::super::helpers::{
    fill_row_defaults, get_expr_name, get_skip_reason, get_unsupported_reason, infer_expr_type,
    normalize_ident, parse_value_for_copy,
};
use super::super::names;
use super::super::query;
use super::super::rbac;
use super::super::sequences;
use super::super::statement_time;
use super::super::udt;
use super::super::{parse_sql, ExecuteResult, ExecuteResults, InFailedSqlTransaction, Session};
use crate::auth::AuthManager;
use crate::observability::TenantObservability;
use crate::session_context;
use crate::storage::TikvStore;
use crate::types::{DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use rust_decimal::prelude::ToPrimitive;
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, Query, SelectItem, SetExpr, SetOperator, SetQuantifier,
    Statement, TableFactor, Visit, Visitor,
};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{ops::ControlFlow};
use tikv_client::Transaction;
use tracing::debug;

const OBSERVABILITY_USER: &str = "_pgtikv_sys_observer";

fn starts_with_ignore_ascii_case(haystack: &str, prefix: &str) -> bool {
    let haystack = haystack.as_bytes();
    let prefix = prefix.as_bytes();
    haystack
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

#[derive(Debug)]
struct StatementTimeoutError;

impl std::fmt::Display for StatementTimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "canceling statement due to statement timeout")
    }
}

impl std::error::Error for StatementTimeoutError {}

fn is_retryable_tikv_error(err: &anyhow::Error) -> bool {
    fn contains_write_conflict(err: &tikv_client::Error) -> bool {
        // Retry on ANY WriteConflict, not just PessimisticRetry.
        // This emulates PostgreSQL's row-lock wait behavior: when concurrent
        // transactions UPDATE the same row, the second should wait and retry
        // rather than immediately failing.
        //
        // WriteConflict reasons (from kvrpcpb.proto):
        //   0 = Unknown
        //   1 = Optimistic (optimistic txn conflict)
        //   2 = PessimisticRetry (lock wait wakeup or newer version)
        //   3 = SelfRolledBack (txn rolled back during prewrite)
        //   4 = RcCheckTs (RC isolation check failure)
        //   5 = LazyUniquenessCheck (pessimistic unique constraint)
        //
        // We retry all of these to maximize compatibility with PostgreSQL
        // semantics where concurrent UPDATEs on the same row succeed
        // (second waits for first to commit).
        match err {
            tikv_client::Error::PessimisticLockError { inner, .. } => {
                contains_write_conflict(inner)
            }
            tikv_client::Error::UndeterminedError(inner) => contains_write_conflict(inner),
            tikv_client::Error::ExtractedErrors(errors)
            | tikv_client::Error::MultipleKeyErrors(errors) => {
                errors.iter().any(contains_write_conflict)
            }
            tikv_client::Error::KeyError(key_error) => key_error.conflict.is_some(),
            _ => false,
        }
    }

    err.chain().any(|cause| {
        cause
            .downcast_ref::<tikv_client::Error>()
            .is_some_and(contains_write_conflict)
    })
}

fn set_variable_value_to_string(value: &[Expr]) -> Result<String> {
    if value.len() != 1 {
        return Err(anyhow!("Unsupported SET value list"));
    }
    let expr = &value[0];
    match expr {
        Expr::Value(sqlparser::ast::Value::Number(s, _)) => Ok(s.clone()),
        Expr::Value(sqlparser::ast::Value::SingleQuotedString(s))
        | Expr::Value(sqlparser::ast::Value::DoubleQuotedString(s))
        | Expr::Value(sqlparser::ast::Value::EscapedStringLiteral(s))
        | Expr::Value(sqlparser::ast::Value::RawStringLiteral(s))
        | Expr::Value(sqlparser::ast::Value::NationalStringLiteral(s))
        | Expr::Value(sqlparser::ast::Value::UnQuotedString(s)) => Ok(s.clone()),
        Expr::Value(sqlparser::ast::Value::Boolean(b)) => {
            Ok((if *b { "on" } else { "off" }).to_string())
        }
        Expr::Identifier(ident) => {
            let v = ident.value.as_str();
            if v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on") {
                Ok("on".to_string())
            } else if v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off") {
                Ok("off".to_string())
            } else {
                Ok(ident.value.clone())
            }
        }
        Expr::CompoundIdentifier(idents) if idents.len() == 1 => Ok(idents[0].value.clone()),
        Expr::Value(sqlparser::ast::Value::Null) => Ok(String::new()),
        Expr::Interval(interval) => {
            // Drivers commonly set `TimeZone` using an offset interval:
            // `SET TIME ZONE INTERVAL '+00:00' HOUR TO MINUTE`.
            // We accept hour-to-minute intervals and store the literal value (e.g. "+00:00")
            // for readback via `SHOW` / `current_setting`.
            if matches!(interval.leading_field, Some(sqlparser::ast::DateTimeField::Hour))
                && matches!(
                    interval.last_field,
                    Some(sqlparser::ast::DateTimeField::Minute)
                )
            {
                let Some(s) = try_parse_const_text(interval.value.as_ref()) else {
                    return Err(anyhow!("Unsupported SET value: {}", expr));
                };
                Ok(s)
            } else {
                Err(anyhow!("Unsupported SET value: {}", expr))
            }
        }
        _ => Err(anyhow!("Unsupported SET value: {}", expr)),
    }
}

fn parse_search_path_guc_value(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in s.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let schema = if token.starts_with('"') && token.ends_with('"') && token.len() >= 2 {
            token[1..token.len() - 1].to_string()
        } else {
            token.to_lowercase()
        };
        out.push(schema);
    }
    out
}

fn try_parse_const_text(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Value(sqlparser::ast::Value::SingleQuotedString(s))
        | Expr::Value(sqlparser::ast::Value::DoubleQuotedString(s))
        | Expr::Value(sqlparser::ast::Value::EscapedStringLiteral(s))
        | Expr::Value(sqlparser::ast::Value::RawStringLiteral(s))
        | Expr::Value(sqlparser::ast::Value::NationalStringLiteral(s))
        | Expr::Value(sqlparser::ast::Value::UnQuotedString(s)) => Some(s.clone()),
        Expr::Cast { expr, .. } | Expr::TryCast { expr, .. } | Expr::SafeCast { expr, .. } => {
            try_parse_const_text(expr.as_ref())
        }
        _ => None,
    }
}

fn try_parse_const_bool(expr: &Expr) -> Option<bool> {
    match expr {
        Expr::Value(sqlparser::ast::Value::Boolean(b)) => Some(*b),
        Expr::Identifier(ident) => {
            let v = ident.value.as_str();
            if v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on") {
                Some(true)
            } else if v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off") {
                Some(false)
            } else {
                None
            }
        }
        Expr::Cast { expr, .. } | Expr::TryCast { expr, .. } | Expr::SafeCast { expr, .. } => {
            try_parse_const_bool(expr.as_ref())
        }
        _ => None,
    }
}

fn unwrap_top_level_cast<'a>(
    mut expr: &'a Expr,
) -> (&'a Expr, Option<&'a sqlparser::ast::DataType>) {
    let mut cast_to: Option<&'a sqlparser::ast::DataType> = None;
    loop {
        match expr {
            Expr::Cast { expr: inner, data_type, .. }
            | Expr::TryCast { expr: inner, data_type, .. }
            | Expr::SafeCast { expr: inner, data_type, .. } => {
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

fn cast_current_setting_value(value: Value, target_type: &DataType) -> Result<Value> {
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }

    let Value::Text(s) = value else {
        return Ok(value);
    };

    match target_type {
        DataType::Text => Ok(Value::Text(s)),
        DataType::Int32 => {
            let v: i32 = s.parse().map_err(|_| {
                anyhow!("invalid input syntax for type integer: \"{}\"", s)
            })?;
            Ok(Value::Int32(v))
        }
        DataType::Int64 => {
            let v: i64 = s.parse().map_err(|_| {
                anyhow!("invalid input syntax for type bigint: \"{}\"", s)
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
                Err(anyhow!("invalid input syntax for type boolean: \"{}\"", s))
            }
        }
        _ => Ok(Value::Text(s)),
    }
}

fn is_set_config_function(name: &sqlparser::ast::ObjectName) -> bool {
    match name.0.as_slice() {
        [ident] => ident.value.eq_ignore_ascii_case("set_config"),
        [schema, ident] => {
            schema.value.eq_ignore_ascii_case("pg_catalog") && ident.value.eq_ignore_ascii_case("set_config")
        }
        _ => false,
    }
}

fn is_current_setting_function(name: &sqlparser::ast::ObjectName) -> bool {
    match name.0.as_slice() {
        [ident] => ident.value.eq_ignore_ascii_case("current_setting"),
        [schema, ident] => {
            schema.value.eq_ignore_ascii_case("pg_catalog")
                && ident.value.eq_ignore_ascii_case("current_setting")
        }
        _ => false,
    }
}

fn try_execute_set_config_select(session: &mut Session, query: &Query) -> Result<Option<ExecuteResult>> {
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
        let prev = session
            .show_setting_value("search_path")
            .unwrap_or_else(|| "public".to_string());

        let mut new_search_path = parse_search_path_guc_value(&new_value);
        new_search_path.retain(|s| !s.is_empty() && s != "$user");
        if new_search_path.len() == 1 && new_search_path[0] == "default" {
            new_search_path = vec!["public".to_string()];
        }
        for schema in &new_search_path {
            if schema.contains('.') {
                return Err(anyhow!("schema name '{}' must not contain '.'", schema));
            }
        }
        if new_search_path.is_empty() {
            new_search_path.push("public".to_string());
        }
        session.set_search_path(new_search_path);

        return Ok(Some(ExecuteResult::Select {
            columns: vec![alias.unwrap_or_else(|| "set_config".to_string())],
            column_types: Some(vec![DataType::Text]),
            rows: vec![Row::new(vec![Value::Text(prev)])],
            timezone: session_context::current_timezone(),
        }));
    }

    let prev = session.show_setting_value(&var_name);
    if session.set_known_setting(&var_name, new_value)? {
        let prev = prev.unwrap_or_else(|| "0".to_string());
        return Ok(Some(ExecuteResult::Select {
            columns: vec![alias.unwrap_or_else(|| "set_config".to_string())],
            column_types: Some(vec![DataType::Text]),
            rows: vec![Row::new(vec![Value::Text(prev)])],
            timezone: session_context::current_timezone(),
        }));
    }

    Ok(None)
}

fn try_execute_current_setting_select(
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
        if let Ok(t) = crate::sql::helpers::convert_data_type(cast_to) {
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

fn query_has_nested_queries(query: &Query) -> bool {
    struct NestedQueryVisitor {
        seen: bool,
        has_nested: bool,
    }

    impl Visitor for NestedQueryVisitor {
        type Break = ();

        fn pre_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
            if self.seen {
                self.has_nested = true;
                return ControlFlow::Break(());
            }
            self.seen = true;
            ControlFlow::Continue(())
        }
    }

    let mut visitor = NestedQueryVisitor {
        seen: false,
        has_nested: false,
    };
    let _ = query.visit(&mut visitor);
    visitor.has_nested
}

fn is_observability_system_query(stmt: &Statement) -> bool {
    let Statement::Query(query) = stmt else {
        return false;
    };

    if query.with.is_some() {
        return false;
    };
    if !query.locks.is_empty() || query.for_clause.is_some() {
        return false;
    }
    if query_has_nested_queries(query) {
        return false;
    }

    let SetExpr::Select(select) = query.body.as_ref() else {
        return false;
    };
    if select.into.is_some() {
        return false;
    }
    if !select.lateral_views.is_empty() {
        return false;
    }
    if select.from.len() != 1 {
        return false;
    }
    if !select.from[0].joins.is_empty() {
        return false;
    }
    let TableFactor::Table { name, .. } = &select.from[0].relation else {
        return false;
    };

    let Some(base) = name.0.last() else {
        return false;
    };
    let base_upper = base.value.to_ascii_uppercase();
    if base_upper != "_PGTIKV_SYS_OBSERVABILITY" && base_upper != "_PGTIKV_SYS_QUERY_SAMPLES" {
        return false;
    }

    true
}

fn is_observability_tableless_query(stmt: &Statement) -> bool {
    let Statement::Query(query) = stmt else {
        return false;
    };

    if query.with.is_some() {
        return false;
    }
    if !query.locks.is_empty() || query.for_clause.is_some() {
        return false;
    }
    if query_has_nested_queries(query) {
        return false;
    }

    let SetExpr::Select(select) = query.body.as_ref() else {
        return false;
    };
    if select.into.is_some() {
        return false;
    }
    if !select.lateral_views.is_empty() {
        return false;
    }
    select.from.is_empty()
}

pub struct Executor {
    store: Arc<TikvStore>,
    auth_manager: AuthManager,
    #[allow(dead_code)]
    tenant_keyspace: String,
    observability: Arc<TenantObservability>,
}

impl Executor {
    pub fn new(
        store: Arc<TikvStore>,
        tenant_keyspace: String,
        observability: Arc<TenantObservability>,
    ) -> Self {
        Self {
            store,
            auth_manager: AuthManager::new(),
            tenant_keyspace,
            observability,
        }
    }

    pub fn store(&self) -> Arc<TikvStore> {
        self.store.clone()
    }

    #[allow(dead_code)]
    pub fn tenant_keyspace(&self) -> &str {
        &self.tenant_keyspace
    }

    pub fn observability(&self) -> &Arc<TenantObservability> {
        &self.observability
    }

    #[allow(dead_code)]
    pub fn auth_manager(&self) -> &AuthManager {
        &self.auth_manager
    }

    /// Execute a SQL statement string using the provided session
    /// Supports multiple statements separated by semicolons (e.g., "BEGIN; UPDATE...; COMMIT;")
    /// Returns all results for proper PostgreSQL Simple Query Protocol compliance.
    pub async fn execute(&self, session: &mut Session, sql: &str) -> Result<ExecuteResults> {
        let statement_ts = statement_time::now_timestamp_millis();
        let savepoints = session.savepoints();
        let connection_id = session.connection_id();
        let database_name = session.current_database_name_arc();
        super::super::expr::with_query_context(
            connection_id,
            database_name,
            statement_time::with_statement_timestamp_millis(
                statement_ts,
                crate::txn::with_savepoints(savepoints, async {
                let sql_stripped = strip_leading_sql_comments(sql);
                let sql_trimmed = sql_stripped.trim_start();
                let is_observability_user =
                    session.current_user() == Some(OBSERVABILITY_USER) && !session.is_superuser();
                let starts_with = |prefix: &str| starts_with_ignore_ascii_case(sql_trimmed, prefix);

                if session.is_transaction_failed()
                    && !sql_trimmed.trim().is_empty()
                    && !starts_with("ROLLBACK")
                    && !starts_with("COMMIT")
                    && !starts_with("END")
                {
                    if !is_observability_user {
                        self.observability.record_statement(
                            Duration::from_millis(0),
                            false,
                            || sql_trimmed.to_string(),
                        );
                    }
                    return Err(anyhow::Error::new(InFailedSqlTransaction));
                }

                if !is_observability_user {
                    if starts_with("CREATE DATABASE") {
                        let start = Instant::now();
                        let res = self.execute_create_database_cmd(session, sql).await;
                        if res.is_err() && session.is_in_transaction() {
                            session.mark_transaction_failed();
                        }
                        self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                            sql_trimmed.to_string()
                        });
                        return res;
                    }
                    if starts_with("DROP DATABASE") {
                        let start = Instant::now();
                        let res = self.execute_drop_database_cmd(session, sql).await;
                        if res.is_err() && session.is_in_transaction() {
                            session.mark_transaction_failed();
                        }
                        self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                            sql_trimmed.to_string()
                        });
                        return res;
                    }
                    if starts_with("ALTER DATABASE") {
                        let start = Instant::now();
                        let res = self.execute_alter_database_cmd(session, sql).await;
                        if res.is_err() && session.is_in_transaction() {
                            session.mark_transaction_failed();
                        }
                        self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                            sql_trimmed.to_string()
                        });
                        return res;
                    }
                    if starts_with("CREATE EXTENSION") {
                        let start = Instant::now();
                        let res = self.execute_create_extension_cmd(session, sql).await;
                        if res.is_err() && session.is_in_transaction() {
                            session.mark_transaction_failed();
                        }
                        self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                            sql_trimmed.to_string()
                        });
                        return res.map(ExecuteResults::single);
                    }
                if starts_with("DROP EXTENSION") {
                    let start = Instant::now();
                    let res = self.execute_drop_extension_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }
                if starts_with("COMMENT ON") {
                    let start = Instant::now();
                    let res = self.execute_comment_on_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }
                if starts_with("CREATE OR REPLACE FUNCTION") || starts_with("CREATE FUNCTION") {
                    let start = Instant::now();
                    let res = self.execute_create_function_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }
                if starts_with("DROP FUNCTION") {
                    let start = Instant::now();
                    let res = self.execute_drop_function_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }
                if starts_with("CREATE CONSTRAINT TRIGGER") || starts_with("CREATE TRIGGER") {
                    let start = Instant::now();
                    let res = self.execute_create_trigger_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }
                if starts_with("DROP TRIGGER") {
                    let start = Instant::now();
                    let res = self.execute_drop_trigger_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }
            }

            let sql_upper = sql_trimmed.trim().to_uppercase();
            if !is_observability_user {
                if let Some(reason) = get_skip_reason(&sql_upper) {
                    return Ok(ExecuteResults::single(ExecuteResult::Skipped {
                        message: reason,
                    }));
                }
            }

            if !is_observability_user {
                if (sql_upper.starts_with("ALTER TABLE")
                    || sql_upper.starts_with("ALTER SEQUENCE")
                    || sql_upper.starts_with("ALTER FUNCTION"))
                    && sql_upper.contains(" OWNER TO ")
                {
                    let start = Instant::now();
                    let res = self.execute_alter_owner_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                if sql_upper.starts_with("ALTER SEQUENCE") && sql_upper.contains("OWNED") {
                    let start = Instant::now();
                    let res = self.execute_alter_sequence_owned_by_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                let mut words = sql_upper.split_whitespace();
                let is_refresh_materialized_view = matches!(
                    (words.next(), words.next(), words.next()),
                    (Some("REFRESH"), Some("MATERIALIZED"), Some("VIEW"))
                );
                if is_refresh_materialized_view {
                    let start = Instant::now();
                    let res = self.execute_refresh_materialized_view_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                let mut words = sql_upper.split_whitespace();
                let is_drop_materialized_view = matches!(
                    (words.next(), words.next(), words.next()),
                    (Some("DROP"), Some("MATERIALIZED"), Some("VIEW"))
                );
                if is_drop_materialized_view {
                    let start = Instant::now();
                    let res = self.execute_drop_materialized_view_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                if sql_upper.starts_with("CALL ") {
                    let start = Instant::now();
                    let res = self.execute_call_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                if sql_upper.starts_with("DROP PROCEDURE") {
                    let start = Instant::now();
                    let res = self.execute_drop_procedure_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                if sql_upper.starts_with("CREATE PROCEDURE") {
                    let start = Instant::now();
                    let res = self.execute_create_procedure_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }

                if sql_upper.starts_with("CREATE TYPE") {
                    let mut prev = "";
                    let mut is_enum = false;
                    for token in sql_upper.split_whitespace() {
                        if prev == "AS" && token.starts_with("ENUM") {
                            is_enum = true;
                            break;
                        }
                        prev = token;
                    }
                    if is_enum {
                        let start = Instant::now();
                        let res = self.execute_create_type_enum_cmd(session, sql).await;
                        if res.is_err() && session.is_in_transaction() {
                            session.mark_transaction_failed();
                        }
                        self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                            sql_trimmed.to_string()
                        });
                        return res.map(ExecuteResults::single);
                    }
                }

                if sql_upper.starts_with("DROP TYPE") {
                    let start = Instant::now();
                    let res = self.execute_drop_type_cmd(session, sql).await;
                    if res.is_err() && session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    self.observability.record_statement(start.elapsed(), res.is_ok(), || {
                        sql_trimmed.to_string()
                    });
                    return res.map(ExecuteResults::single);
                }
            }

            let statements = match parse_sql(sql) {
                Ok(stmts) => stmts,
                Err(e) => {
                    if !is_observability_user {
                        if let Some(reason) = get_unsupported_reason(&sql_upper) {
                            return Ok(ExecuteResults::single(ExecuteResult::Skipped {
                                message: reason,
                            }));
                        }
                        // Parse error counts as a statement attempt (for error rate / p99, etc).
                        self.observability.record_statement(
                            Duration::from_millis(0),
                            false,
                            || sql_trimmed.to_string(),
                        );
                    }
                    if session.is_in_transaction() {
                        session.mark_transaction_failed();
                    }
                    return Err(e);
                }
            };

            if statements.is_empty() {
                return Ok(ExecuteResults::single(ExecuteResult::Empty));
            }

            let mut results: Vec<ExecuteResult> = Vec::with_capacity(statements.len());

            for stmt in &statements {
                debug!("Executing statement: {:?}", stmt);
                let is_observability_query = is_observability_user
                    && (is_observability_system_query(stmt) || is_observability_tableless_query(stmt));
                if is_observability_user {
                    match stmt {
                        // For observability user, allow common utility statements and return
                        // semantically correct protocol responses (never `EmptyQueryResponse` for
                        // non-empty SQL).
                        Statement::StartTransaction { .. } => {
                            session.begin().await?;
                            results.push(ExecuteResult::TransactionStart { tag: "BEGIN" });
                            continue;
                        }
                        Statement::Commit { .. } => {
                            let tag = if session.is_transaction_failed() {
                                "ROLLBACK"
                            } else {
                                "COMMIT"
                            };
                            session.commit().await?;
                            results.push(ExecuteResult::TransactionEnd { tag });
                            continue;
                        }
                        Statement::Savepoint { name } => {
                            session.create_savepoint(normalize_ident(name))?;
                            results.push(ExecuteResult::CommandComplete { tag: "SAVEPOINT" });
                            continue;
                        }
                        Statement::ReleaseSavepoint { name } => {
                            let sp = normalize_ident(name);
                            session.release_savepoint(&sp)?;
                            results.push(ExecuteResult::CommandComplete { tag: "RELEASE" });
                            continue;
                        }
                        Statement::Rollback {
                            savepoint: Some(name),
                            ..
                        } => {
                            let sp = normalize_ident(name);
                            session.rollback_to_savepoint(&sp).await?;
                            results.push(ExecuteResult::CommandComplete { tag: "ROLLBACK" });
                            continue;
                        }
                        Statement::Rollback {
                            savepoint: None, ..
                        } => {
                            session.rollback().await?;
                            results.push(ExecuteResult::TransactionEnd { tag: "ROLLBACK" });
                            continue;
                        }
                        Statement::SetVariable { .. }
                        | Statement::SetTimeZone { .. }
                        | Statement::SetNames { .. }
                        | Statement::SetTransaction { .. } => {
                            results.push(ExecuteResult::CommandComplete { tag: "SET" });
                            continue;
                        }
                        Statement::ShowVariable { variable } => {
                            let var_name = variable
                                .iter()
                                .map(normalize_ident)
                                .collect::<Vec<_>>()
                                .join(".")
                                .to_lowercase();
                            let value = match session.show_setting_value(&var_name) {
                                Some(value) => value,
                                None => {
                                    let err = anyhow!(
                                        "unrecognized configuration parameter \"{}\"",
                                        var_name
                                    );
                                    if session.is_in_transaction() {
                                        session.mark_transaction_failed();
                                    }
                                    return Err(err);
                                }
                            };
                            let timezone = Arc::from(
                                session
                                    .show_setting_value("timezone")
                                    .unwrap_or_else(|| "UTC".to_string()),
                            );

                            results.push(ExecuteResult::Select {
                                columns: vec![var_name],
                                column_types: Some(vec![DataType::Text]),
                                rows: vec![Row::new(vec![Value::Text(value)])],
                                timezone,
                            });
                            continue;
                        }
                        Statement::Query(_) => {
                            if !is_observability_query {
                                if session.is_in_transaction() {
                                    session.mark_transaction_failed();
                                }
                                return Err(anyhow!(
                                    "permission denied for role '{}'",
                                    OBSERVABILITY_USER
                                ));
                            }
                        }
                        _ => {
                            if session.is_in_transaction() {
                                session.mark_transaction_failed();
                            }
                            return Err(anyhow!(
                                "permission denied for role '{}'",
                                OBSERVABILITY_USER
                            ));
                        }
                    }
                }
                let start = Instant::now();
                let is_superuser = session.is_superuser();
                let timezone = Arc::from(
                    session
                        .show_setting_value("timezone")
                        .unwrap_or_else(|| "UTC".to_string()),
                );
                let stmt_exec: Result<Vec<ExecuteResult>> = session_context::with_timezone(
                    timezone,
                    crate::extensions::context::with_context(is_superuser, async {
                        match stmt {
                            // Transaction Control
                            Statement::StartTransaction { .. } => {
                                session.begin().await?;
                                Ok(vec![ExecuteResult::TransactionStart { tag: "BEGIN" }])
                            }
                            Statement::Commit { .. } => {
                                let tag = if session.is_transaction_failed() {
                                    "ROLLBACK"
                                } else {
                                    "COMMIT"
                                };
                                session.commit().await?;
                                Ok(vec![ExecuteResult::TransactionEnd { tag }])
                            }
                            Statement::Savepoint { name } => {
                                session.create_savepoint(normalize_ident(name))?;
                                Ok(vec![ExecuteResult::CommandComplete { tag: "SAVEPOINT" }])
                            }
                            Statement::ReleaseSavepoint { name } => {
                                let sp = normalize_ident(name);
                                session.release_savepoint(&sp)?;
                                Ok(vec![ExecuteResult::CommandComplete { tag: "RELEASE" }])
                            }
                            Statement::Rollback {
                                savepoint: Some(name),
                                ..
                            } => {
                                let sp = normalize_ident(name);
                                session.rollback_to_savepoint(&sp).await?;
                                Ok(vec![ExecuteResult::CommandComplete { tag: "ROLLBACK" }])
                            }
                            Statement::Rollback {
                                savepoint: None, ..
                            } => {
                                session.rollback().await?;
                                Ok(vec![ExecuteResult::TransactionEnd { tag: "ROLLBACK" }])
                            }
                            Statement::SetVariable {
                                variable, value, ..
                            } => {
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
                                            Expr::Value(sqlparser::ast::Value::SingleQuotedString(
                                                s,
                                            )) => {
                                                new_search_path.extend(parse_search_path_guc_value(s));
                                            }
                                            _ => {
                                                return Err(anyhow!(
                                                    "Unsupported search_path value: {}",
                                                    expr
                                                ));
                                            }
                                        }
                                    }

                                    new_search_path.retain(|s| !s.is_empty() && s != "$user");
                                    if new_search_path.len() == 1
                                        && new_search_path[0] == "default"
                                    {
                                        new_search_path = vec!["public".to_string()];
                                    }
                                    for schema in &new_search_path {
                                        if schema.contains('.') {
                                            return Err(anyhow!(
                                                "schema name '{}' must not contain '.'",
                                                schema
                                            ));
                                        }
                                    }
                                    if new_search_path.is_empty() {
                                        new_search_path.push("public".to_string());
                                    }
                                    session.set_search_path(new_search_path);
                                } else if matches!(
                                    var_name.as_str(),
                                    "statement_timeout"
                                        | "lock_timeout"
                                        | "idle_in_transaction_session_timeout"
                                        | "timezone"
                                        | "application_name"
                                        | "client_encoding"
                                        | "standard_conforming_strings"
                                        | "check_function_bodies"
                                        | "xmloption"
                                        | "client_min_messages"
                                        | "row_security"
                                        | "default_tablespace"
                                        | "default_table_access_method"
                                ) {
                                    let value = set_variable_value_to_string(value)?;
                                    session.set_known_setting(&var_name, value)?;
                                }
                                Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }])
                            }
                            Statement::SetTimeZone { value, .. } => {
                                let value = set_variable_value_to_string(std::slice::from_ref(
                                    value,
                                ))?;
                                session.set_known_setting("timezone", value)?;
                                Ok(vec![ExecuteResult::CommandComplete { tag: "SET" }])
                            }
                            Statement::ShowVariable { variable } => {
                                let var_name = variable
                                    .iter()
                                    .map(normalize_ident)
                                    .collect::<Vec<_>>()
                                    .join(".")
                                    .to_lowercase();
                                let value = session.show_setting_value(&var_name).ok_or_else(|| {
                                    anyhow!("unrecognized configuration parameter \"{}\"", var_name)
                                })?;

                                Ok(vec![ExecuteResult::Select {
                                    columns: vec![var_name],
                                    column_types: Some(vec![DataType::Text]),
                                    rows: vec![Row::new(vec![Value::Text(value)])],
                                    timezone: session_context::current_timezone(),
                                }])
                            }
                            // DDL/DML - delegated to session transaction management
                            _ => {
                                if let Statement::Query(query) = stmt {
                                    if let Some(result) =
                                        try_execute_set_config_select(session, query.as_ref())?
                                    {
                                        return Ok(vec![result]);
                                    }
                                    if let Some(result) =
                                        try_execute_current_setting_select(session, query.as_ref())?
                                    {
                                        return Ok(vec![result]);
                                    }
                                }

                                let is_autocommit = !session.is_in_transaction();
                                let db_id = session.current_database_id();

                                // Retry up to 10 times for autocommit to handle concurrent conflicts
                                let max_attempts = if is_autocommit { 10usize } else { 1usize };

                                for attempt in 0..max_attempts {
                                    if is_autocommit {
                                        session.begin().await?;
                                    }

                                    let timeout = session.statement_timeout();
                                    let fut = async {
                                        let (txn, sequence_values, search_path) = session
                                            .get_mut_txn_sequence_values_and_search_path()
                                            .expect("Transaction must be active");
                                        let notices = self
                                            .collect_notices_before_statement(
                                                txn,
                                                db_id,
                                                search_path,
                                                stmt,
                                            )
                                            .await?;
                                        let result = self
                                            .execute_statement_on_txn(
                                                txn,
                                                db_id,
                                                sequence_values,
                                                search_path,
                                                stmt,
                                            )
                                            .await?;
                                        Ok::<(Vec<ExecuteResult>, ExecuteResult), anyhow::Error>((
                                            notices, result,
                                        ))
                                    };

                                    let res = match timeout {
                                        Some(timeout) => match tokio::time::timeout(timeout, fut).await {
                                            Ok(res) => res,
                                            Err(_) => Err(anyhow::Error::new(StatementTimeoutError)),
                                        },
                                        None => fut.await,
                                    };

                                    if res
                                        .as_ref()
                                        .err()
                                        .is_some_and(|e| e.is::<StatementTimeoutError>())
                                        && !is_autocommit
                                    {
                                        // pg-tikv does not currently implement PostgreSQL's "failed
                                        // transaction" state. To avoid leaving an open transaction in
                                        // an unknown partial state, abort it on statement timeout.
                                        session.rollback().await?;
                                    }

                                    if is_autocommit {
                                        match res {
                                            Ok((notices, result)) => {
                                                if is_observability_query {
                                                    session.rollback().await?;
                                                } else {
                                                    session.commit().await?;
                                                }
                                                let mut stmt_results = notices;
                                                stmt_results.push(result);
                                                return Ok(stmt_results);
                                            }
                                            Err(err) => {
                                                session.rollback().await?;
                                                let should_retry = attempt + 1 < max_attempts
                                                    && is_retryable_tikv_error(&err);
                                                if should_retry {
                                                    // Exponential backoff with jitter to reduce contention
                                                    let base_ms = 5u64.saturating_mul(1u64 << attempt.min(6));
                                                    let jitter_ms = rand::random::<u64>() % (base_ms + 1);
                                                    let backoff_ms = base_ms + jitter_ms;
                                                    tokio::time::sleep(Duration::from_millis(backoff_ms))
                                                        .await;
                                                    continue;
                                                }
                                                return Err(err);
                                            }
                                        }
                                    } else {
                                        let (notices, result) = res?;
                                        let mut stmt_results = notices;
                                        stmt_results.push(result);
                                        return Ok(stmt_results);
                                    }
                                }

                                unreachable!("retry loop must return")
                            }
                        }
                    }),
                )
                .await;

                if stmt_exec.is_err() && session.is_in_transaction() {
                    session.mark_transaction_failed();
                }

                if !is_observability_query {
                    self.observability
                        .record_statement(start.elapsed(), stmt_exec.is_ok(), || stmt.to_string());
                }

                results.extend(stmt_exec?);
            }

            Ok(ExecuteResults(results))
                }),
            ),
        )
        .await
    }

    async fn collect_notices_before_statement(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        stmt: &Statement,
    ) -> Result<Vec<ExecuteResult>> {
        use sqlparser::ast::ObjectType;

        match stmt {
            Statement::Drop {
                object_type: ObjectType::Table,
                names: drop_names,
                if_exists: true,
                ..
            } => {
                let mut notices = Vec::new();
                for name in drop_names {
                    let exists = super::super::names::resolve_existing_table_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        name,
                        search_path,
                    )
                    .await?
                    .is_some();

                    if exists {
                        continue;
                    }

                    let base = name
                        .0
                        .last()
                        .map(|ident| ident.value.as_str())
                        .unwrap_or("?");
                    notices.push(ExecuteResult::Notice {
                        message: format!("table \"{}\" does not exist, skipping", base),
                    });
                }
                Ok(notices)
            }
            _ => Ok(Vec::new()),
        }
    }

    pub(crate) async fn execute_alter_owner_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let alter_owner::AlterOwnerCommand {
            kind,
            if_exists,
            name,
            new_owner,
        } = alter_owner::parse_alter_owner_sql(sql)?;

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let db_id = session.current_database_id();
        let result = async {
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            match kind {
                alter_owner::AlterOwnerKind::Table => {
                    let resolved = names::resolve_existing_table_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        &name,
                        search_path,
                    )
                    .await?;
                    let resolved = match resolved {
                        Some(r) => r,
                        None if if_exists => {
                            return Ok(ExecuteResult::CommandComplete { tag: "ALTER TABLE" });
                        }
                        None => return Err(anyhow!("Table '{}' does not exist", name)),
                    };

                    let mut schema = self
                        .store
                        .get_schema(txn, db_id, &resolved.full)
                        .await?
                        .ok_or_else(|| anyhow!("Table '{}' does not exist", resolved.full))?;
                    schema.owner = new_owner;
                    self.store.update_schema(txn, db_id, schema).await?;
                    Ok(ExecuteResult::AlterTable {
                        table_name: resolved.full,
                    })
                }
                alter_owner::AlterOwnerKind::Sequence => {
                    let resolved = names::resolve_existing_sequence_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        &name,
                        search_path,
                    )
                    .await?;
                    let resolved = match resolved {
                        Some(r) => r,
                        None if if_exists => {
                            return Ok(ExecuteResult::CommandComplete {
                                tag: "ALTER SEQUENCE",
                            });
                        }
                        None => return Err(anyhow!("Sequence '{}' does not exist", name)),
                    };

                    let mut seq = self
                        .store
                        .get_sequence(txn, db_id, &resolved.full)
                        .await?
                        .ok_or_else(|| anyhow!("Sequence '{}' does not exist", resolved.full))?;
                    seq.owner = new_owner;
                    self.store.update_sequence_def(txn, db_id, &seq).await?;
                    Ok(ExecuteResult::AlterSequence {
                        sequence_name: resolved.full,
                    })
                }
                alter_owner::AlterOwnerKind::Function => {
                    let resolved = names::resolve_existing_function_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        &name,
                        search_path,
                    )
                    .await?;
                    let resolved = match resolved {
                        Some(r) => r,
                        None if if_exists => {
                            return Ok(ExecuteResult::CommandComplete {
                                tag: "ALTER FUNCTION",
                            });
                        }
                        None => return Err(anyhow!("Function '{}' does not exist", name)),
                    };

                    let mut func = self
                        .store
                        .get_function(txn, db_id, &resolved.full)
                        .await?
                        .ok_or_else(|| anyhow!("Function '{}' does not exist", resolved.full))?;
                    func.owner = new_owner;
                    self.store.replace_function(txn, db_id, func).await?;
                    Ok(ExecuteResult::AlterFunction {
                        function_name: resolved.full,
                    })
                }
            }
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }

    pub(crate) async fn execute_alter_sequence_owned_by_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let alter_sequence_owned_by::AlterSequenceOwnedByCommand {
            if_exists,
            sequence_name,
            owned_by,
        } = alter_sequence_owned_by::parse_alter_sequence_owned_by_sql(sql)?;

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let db_id = session.current_database_id();
        let result = async {
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let resolved = names::resolve_existing_sequence_name(
                self.store.as_ref(),
                txn,
                db_id,
                &sequence_name,
                search_path,
            )
            .await?;
            let resolved = match resolved {
                Some(r) => r,
                None if if_exists => {
                    return Ok(ExecuteResult::CommandComplete { tag: "ALTER SEQUENCE" });
                }
                None => return Err(anyhow!("Sequence '{}' does not exist", sequence_name)),
            };

            let mut seq = self
                .store
                .get_sequence(txn, db_id, &resolved.full)
                .await?
                .ok_or_else(|| anyhow!("Sequence '{}' does not exist", resolved.full))?;

            seq.owned_by = match owned_by {
                None => None,
                Some((table_name, column_name)) => {
                    let resolved_table = names::resolve_existing_table_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        &table_name,
                        search_path,
                    )
                    .await?
                    .ok_or_else(|| anyhow!("Table '{}' does not exist", table_name))?;
                    let schema = self
                        .store
                        .get_schema(txn, db_id, &resolved_table.full)
                        .await?
                        .ok_or_else(|| anyhow!("Table '{}' does not exist", resolved_table.full))?;

                    if schema.column_index(&column_name).is_none() {
                        return Err(anyhow!(
                            "column \"{}\" of relation \"{}\" does not exist",
                            column_name,
                            resolved_table.name
                        ));
                    }

                    Some((resolved_table.full, column_name))
                }
            };

            self.store.update_sequence_def(txn, db_id, &seq).await?;
            Ok(ExecuteResult::AlterSequence {
                sequence_name: resolved.full,
            })
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }

    pub(crate) async fn execute_comment_on_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let comment_on::CommentOnCommand { target, comment } = comment_on::parse_comment_on_sql(sql)?;

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let db_id = session.current_database_id();
        let result = async {
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            match target {
                comment_on::CommentOnTarget::Extension { name } => {
                    if self.store.get_extension(txn, db_id, &name).await?.is_none() {
                        return Err(anyhow!("extension \"{}\" does not exist", name));
                    }
                    self.store
                        .set_extension_comment(txn, db_id, &name, comment.as_deref())
                        .await?;
                }
                comment_on::CommentOnTarget::Function { name } => {
                    let resolved = names::resolve_existing_function_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        &name,
                        search_path,
                    )
                    .await?
                    .ok_or_else(|| anyhow!("Function '{}' does not exist", name))?;

                    self.store
                        .set_function_comment(txn, db_id, &resolved.full, comment.as_deref())
                        .await?;
                }
                comment_on::CommentOnTarget::Table { name } => {
                    let resolved = names::resolve_existing_table_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        &name,
                        search_path,
                    )
                    .await?
                    .ok_or_else(|| anyhow!("Table '{}' does not exist", name))?;

                    self.store
                        .set_table_comment(txn, db_id, &resolved.full, comment.as_deref())
                        .await?;
                }
                comment_on::CommentOnTarget::Column { table, column } => {
                    let resolved_table = names::resolve_existing_table_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        &table,
                        search_path,
                    )
                    .await?
                    .ok_or_else(|| anyhow!("Table '{}' does not exist", table))?;
                    let schema = self
                        .store
                        .get_schema(txn, db_id, &resolved_table.full)
                        .await?
                        .ok_or_else(|| anyhow!("Table '{}' does not exist", resolved_table.full))?;

                    if schema.column_index(&column).is_none() {
                        return Err(anyhow!(
                            "column \"{}\" of relation \"{}\" does not exist",
                            column,
                            resolved_table.name
                        ));
                    }

                    self.store
                        .set_column_comment(txn, db_id, &resolved_table.full, &column, comment.as_deref())
                        .await?;
                }
            }

            Ok(ExecuteResult::CommandComplete { tag: "COMMENT" })
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }

    /// Execute a parsed SQL statement on a given transaction
    pub(crate) async fn execute_statement_on_txn(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        stmt: &Statement,
    ) -> Result<ExecuteResult> {
        match stmt {
            Statement::CreateTable {
                name,
                columns,
                constraints,
                if_not_exists,
                query,
                temporary,
                ..
            } => {
                if let Some(q) = query {
                    self.execute_create_table_as(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        name,
                        q,
                        columns,
                        *if_not_exists,
                        *temporary,
                    )
                    .await
                } else {
                    ddl::execute_create_table(
                        &self.store,
                        txn,
                        db_id,
                        search_path,
                        name,
                        columns,
                        constraints,
                        *if_not_exists,
                    )
                    .await
                }
            }
            Statement::CreateIndex {
                name,
                table_name,
                using,
                columns,
                unique,
                if_not_exists,
                predicate,
                ..
            } => {
                let index_name = name
                    .as_ref()
                    .ok_or_else(|| anyhow!("Index name required"))?;
                self.execute_create_index(
                    txn,
                    db_id,
                    search_path,
                    index_name,
                    table_name,
                    using.as_ref(),
                    columns,
                    *unique,
                    *if_not_exists,
                    predicate.as_ref(),
                )
                .await
            }
            Statement::Drop {
                object_type,
                names,
                if_exists,
                ..
            } => {
                use sqlparser::ast::ObjectType;
                match object_type {
                    ObjectType::Table => {
                        ddl::execute_drop_table(&self.store, txn, db_id, search_path, names, *if_exists)
                            .await
                    }
                    ObjectType::View => {
                        ddl::execute_drop_view(&self.store, txn, db_id, search_path, names, *if_exists)
                            .await
                    }
                    ObjectType::Index => {
                        self.execute_drop_index(txn, db_id, search_path, names, *if_exists)
                            .await
                    }
                    ObjectType::Role => {
                        rbac::execute_drop_role(&self.auth_manager, txn, names, *if_exists).await
                    }
                    ObjectType::Sequence => {
                        sequences::execute_drop_sequence(
                            &self.store,
                            txn,
                            db_id,
                            search_path,
                            names,
                            *if_exists,
                        )
                        .await
                    }
                    ObjectType::Schema => {
                        self.execute_drop_schema(txn, db_id, search_path, names, *if_exists)
                            .await
                    }
                    _ => Ok(ExecuteResult::Empty),
                }
            }
            Statement::Truncate { table_name, .. } => {
                ddl::execute_truncate(&self.store, txn, db_id, search_path, table_name).await
            }
            Statement::AlterTable {
                name, operations, ..
            } => {
                for op in operations {
                    self.execute_alter_table(txn, db_id, search_path, name, op).await?;
                }
                let table_name = name.0.last().unwrap().value.clone();
                Ok(ExecuteResult::AlterTable { table_name })
            }
            Statement::Insert {
                table_name,
                columns,
                source,
                returning,
                on,
                ..
            } => {
                self.execute_insert(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    table_name,
                    columns,
                    source,
                    returning,
                    on,
                )
                .await
            }
            Statement::Delete {
                from,
                using,
                selection,
                returning,
                ..
            } => {
                let using = using.as_deref().unwrap_or(&[]);
                self.execute_delete(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    from,
                    using,
                    selection,
                    returning,
                )
                .await
            }
            Statement::Update {
                table,
                assignments,
                from,
                selection,
                returning,
                ..
            } => {
                self.execute_update(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    table,
                    assignments,
                    from,
                    selection,
                    returning,
                )
                .await
            }
            Statement::Query(query) => {
                self.execute_query(txn, db_id, sequence_values, search_path, query)
                    .await
            }
            Statement::ShowTables { .. } => self.execute_show_tables(txn, db_id, search_path).await,
            Statement::SetVariable { .. }
            | Statement::SetTimeZone { .. }
            | Statement::SetNames { .. }
            | Statement::SetTransaction { .. } => Ok(ExecuteResult::CommandComplete { tag: "SET" }),
            Statement::CreateType {
                name,
                representation,
            } => {
                udt::execute_create_type(&self.store, txn, db_id, search_path, name, representation).await
            }
            Statement::CreateSchema {
                schema_name,
                if_not_exists,
            } => {
                use sqlparser::ast::SchemaName;
                let schema_obj = match schema_name {
                    SchemaName::Simple(name) => name,
                    SchemaName::NamedAuthorization(name, _) => name,
                    SchemaName::UnnamedAuthorization(_) => {
                        return Err(anyhow!("Unsupported CREATE SCHEMA syntax"));
                    }
                };
                let (schema_prefix, schema) = names::split_object_name(schema_obj)?;
                if schema_prefix.is_some() {
                    return Err(anyhow!("Invalid schema name '{}'", schema_obj));
                }
                self.store
                    .create_schema(txn, db_id, &schema, *if_not_exists)
                    .await?;
                Ok(ExecuteResult::CommandComplete { tag: "CREATE SCHEMA" })
            }
            Statement::CreateFunction { .. } => Ok(ExecuteResult::Empty),
            Statement::CreateProcedure {
                name, params, body, ..
            } => {
                self.execute_create_procedure(txn, db_id, search_path, name, params.as_deref(), body)
                    .await
            }
            Statement::CreateSequence {
                name,
                if_not_exists,
                sequence_options,
                ..
            } => {
                sequences::execute_create_sequence(
                    &self.store,
                    txn,
                    db_id,
                    search_path,
                    name,
                    *if_not_exists,
                    sequence_options,
                )
                .await
            }
            Statement::CreateView {
                name,
                query,
                or_replace,
                materialized,
                ..
            } => {
                if *materialized {
                    self.execute_create_materialized_view(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        name,
                        query,
                        *or_replace,
                    )
                    .await
                } else {
                    ddl::execute_create_view(
                        &self.store,
                        txn,
                        db_id,
                        search_path,
                        name,
                        query,
                        *or_replace,
                    )
                    .await
                }
            }
            Statement::AlterIndex { name, .. } => Ok(ExecuteResult::AlterIndex {
                index_name: name.to_string(),
            }),
            Statement::CreateRole {
                names,
                if_not_exists,
                login,
                password,
                superuser,
                create_db,
                create_role,
                ..
            } => {
                rbac::execute_create_role(
                    &self.auth_manager,
                    txn,
                    names,
                    *if_not_exists,
                    login,
                    password,
                    superuser,
                    create_db,
                    create_role,
                )
                .await
            }
            Statement::AlterRole { name, operation } => {
                rbac::execute_alter_role(&self.auth_manager, txn, name, operation).await
            }
            Statement::Grant {
                privileges,
                objects,
                grantees,
                with_grant_option,
                ..
            } => {
                rbac::execute_grant(
                    &self.auth_manager,
                    txn,
                    privileges,
                    &Some(objects.clone()),
                    grantees,
                    *with_grant_option,
                )
                .await
            }
            Statement::Revoke {
                privileges,
                objects,
                grantees,
                ..
            } => {
                rbac::execute_revoke(
                    &self.auth_manager,
                    txn,
                    privileges,
                    &Some(objects.clone()),
                    grantees,
                )
                .await
            }
            Statement::Comment { .. } => Ok(ExecuteResult::CommandComplete { tag: "COMMENT" }),
            Statement::Copy { .. } => Ok(ExecuteResult::Empty),
            Statement::Explain {
                statement,
                analyze,
                verbose,
                ..
            } => {
                self.execute_explain(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    statement,
                    *analyze,
                    *verbose,
                )
                .await
            }
            Statement::DropFunction {
                if_exists,
                func_desc,
                ..
            } => {
                let mut last_name = None;
                for desc in func_desc {
                    let func_name = &desc.name;
                    let resolved = names::resolve_existing_function_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        func_name,
                        search_path,
                    )
                    .await?;
                    let func_full_name = match resolved {
                        Some(resolved) => resolved.full,
                        None => names::resolve_ddl_object_name(func_name, search_path)?.full,
                    };
                    last_name = Some(func_full_name.clone());
                    let dropped = self.store.drop_function(txn, db_id, &func_full_name).await?;
                    if !dropped && !if_exists {
                        return Err(anyhow!("Function '{}' does not exist", func_full_name));
                    }
                }
                Ok(ExecuteResult::DropFunction {
                    func_name: last_name.unwrap_or_else(|| "unknown".to_string()),
                })
            }
            _ => Err(anyhow!("Unsupported statement: {:?}", stmt)),
        }
    }

    pub(crate) async fn eval_expr_maybe_sequence(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        expr: &Expr,
        row: Option<&Row>,
        schema: Option<&TableSchema>,
    ) -> Result<Value> {
        if sequences::expr_needs_async_eval(expr) {
            sequences::eval_expr_with_sequences(
                &self.store,
                txn,
                db_id,
                sequence_values,
                search_path,
                expr,
                row,
                schema,
            )
            .await
        } else {
            super::super::expr::eval_expr(expr, row, schema)
        }
    }

    pub(crate) async fn eval_expr_join_maybe_sequence(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        expr: &Expr,
        join_ctx: &super::super::expr::JoinContext<'_>,
    ) -> Result<Value> {
        if sequences::expr_needs_async_eval(expr) {
            sequences::eval_expr_join_with_sequences(
                &self.store,
                txn,
                db_id,
                sequence_values,
                search_path,
                expr,
                join_ctx,
            )
            .await
        } else {
            super::super::expr::eval_expr_join(expr, join_ctx)
        }
    }

    pub(crate) fn eval_scalar_subquery_in_join<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        subquery: &'a Query,
        join_ctx: &'a super::super::expr::JoinContext<'a>,
        outer_ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            let substituted_query = super::super::helpers::substitute_join_context_values_in_query(
                subquery,
                join_ctx.column_offsets,
                join_ctx.combined_row,
            );
            let result = self
                .execute_query_with_outer_ctes(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &substituted_query,
                    outer_ctes,
                )
                .await?;
            match result {
                ExecuteResult::Select { rows, .. } => {
                    if rows.is_empty() {
                        Ok(Value::Null)
                    } else if rows.len() == 1 {
                        Ok(rows[0].values.first().cloned().unwrap_or(Value::Null))
                    } else {
                        Err(anyhow!("Scalar subquery returned more than one row"))
                    }
                }
                _ => Err(anyhow!("Subquery must return a SELECT result")),
            }
        })
    }

    pub(crate) async fn execute_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
    ) -> Result<ExecuteResult> {
        let ctes = self
            .build_cte_context(txn, db_id, sequence_values, search_path, query)
            .await?;
        self.execute_query_with_ctes(txn, db_id, sequence_values, search_path, query, &ctes)
            .await
    }

    pub(crate) async fn execute_tableless_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        select: &sqlparser::ast::Select,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        #[derive(Copy, Clone)]
        enum SrfKind {
            Unnest,
            RegexpSplitToTable,
            RegexpMatches,
            EvalFunctionArray,
        }

        fn srf_kind(expr: &Expr) -> Option<SrfKind> {
            let Expr::Function(f) = expr else {
                return None;
            };
            let Some(name) = f.name.0.last() else {
                return None;
            };
            match name.value.to_ascii_uppercase().as_str() {
                "UNNEST" => Some(SrfKind::Unnest),
                "REGEXP_SPLIT_TO_TABLE" => Some(SrfKind::RegexpSplitToTable),
                "REGEXP_MATCHES" => Some(SrfKind::RegexpMatches),
                "JSONB_OBJECT_KEYS"
                | "JSONB_ARRAY_ELEMENTS"
                | "JSONB_ARRAY_ELEMENTS_TEXT"
                | "JSONB_EACH"
                | "JSONB_EACH_TEXT" => Some(SrfKind::EvalFunctionArray),
                _ => None,
            }
        }

        fn regexp_captures_to_values(caps: &regex::Captures<'_>) -> Vec<Value> {
            if caps.len() > 1 {
                (1..caps.len())
                    .map(|idx| match caps.get(idx) {
                        Some(m) => Value::Text(m.as_str().to_string()),
                        None => Value::Null,
                    })
                    .collect()
            } else {
                caps.get(0)
                    .map(|m| vec![Value::Text(m.as_str().to_string())])
                    .unwrap_or_default()
            }
        }

        async fn try_pg_sleep(
            store: &Arc<TikvStore>,
            txn: &mut Transaction,
            db_id: u64,
            sequence_values: &mut HashMap<String, i64>,
            search_path: &[String],
            expr: &Expr,
        ) -> Result<Option<Value>> {
            let Expr::Function(f) = expr else {
                return Ok(None);
            };
            let Some(name) = f.name.0.last() else {
                return Ok(None);
            };
            if !name.value.eq_ignore_ascii_case("pg_sleep") {
                return Ok(None);
            }

            let arg_expr = f.args.first().and_then(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                _ => None,
            });
            let seconds_val = if let Some(arg_expr) = arg_expr {
                sequences::eval_expr_with_sequences(
                    store,
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    arg_expr,
                    None,
                    None,
                )
                .await?
            } else {
                Value::Float64(0.0)
            };

            let seconds = match seconds_val {
                Value::Int32(n) => n as f64,
                Value::Int64(n) => n as f64,
                Value::Float64(f) => f,
                Value::Numeric(d) => d.to_f64().unwrap_or(0.0),
                Value::Text(s) => s.parse::<f64>().unwrap_or(0.0),
                _ => 0.0,
            }
            .max(0.0);

            if seconds > 0.0 {
                tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
            }

            // Match PostgreSQL's void-like output: an empty field.
            Ok(Some(Value::Text(String::new())))
        }

        let resolved_projection = self
            .resolve_projection_subqueries(
                txn,
                db_id,
                sequence_values,
                search_path,
                &select.projection,
                ctes,
            )
            .await?;

        let mut cols = Vec::new();
        let mut values = Vec::new();
        let mut srf_positions: Vec<(usize, Vec<Value>)> = Vec::new();

        for item in &resolved_projection {
            match item {
                SelectItem::UnnamedExpr(expr) => {
                    cols.push(get_expr_name(expr));
                    if let Some(val) =
                        try_pg_sleep(&self.store, txn, db_id, sequence_values, search_path, expr)
                            .await?
                    {
                        values.push(val);
                        continue;
                    }
                    if let Some(kind) = srf_kind(expr) {
                        let Expr::Function(f) = expr else {
                            values.push(Value::Null);
                            continue;
                        };
                        let output_values = match kind {
                            SrfKind::Unnest => {
                                let arg_expr = f.args.first().and_then(|arg| match arg {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                });
                                if let Some(arg_expr) = arg_expr {
                                    match sequences::eval_expr_with_sequences(
                                        &self.store,
                                        txn,
                                        db_id,
                                        sequence_values,
                                        search_path,
                                        arg_expr,
                                        None,
                                        None,
                                    )
                                    .await?
                                    {
                                        Value::Array(arr) => arr,
                                        Value::Null => Vec::new(),
                                        other => vec![other],
                                    }
                                } else {
                                    Vec::new()
                                }
                            }
                            SrfKind::RegexpSplitToTable => {
                                let arg0 = f.args.get(0).and_then(|arg| match arg {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                });
                                let arg1 = f.args.get(1).and_then(|arg| match arg {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                });
                                let arg2 = f.args.get(2).and_then(|arg| match arg {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                });
                                let (Some(arg0), Some(arg1)) = (arg0, arg1) else {
                                    return Err(anyhow!(
                                        "regexp_split_to_table requires at least 2 arguments"
                                    ));
                                };
                                let source_val = sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    arg0,
                                    None,
                                    None,
                                )
                                .await?;
                                let source = match source_val {
                                    Value::Text(s) => Some(s),
                                    Value::Null => None,
                                    v => Some(v.to_string()),
                                };
                                let pattern_val = sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    arg1,
                                    None,
                                    None,
                                )
                                .await?;
                                let pattern = match pattern_val {
                                    Value::Text(s) => Some(s),
                                    Value::Null => None,
                                    v => Some(v.to_string()),
                                };
                                match (source, pattern) {
                                    (Some(source), Some(pattern)) => {
                                    let flags = if let Some(arg2) = arg2 {
                                        match sequences::eval_expr_with_sequences(
                                            &self.store,
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            arg2,
                                            None,
                                            None,
                                        )
                                        .await?
                                        {
                                            Value::Text(s) => s,
                                            Value::Null => String::new(),
                                            v => v.to_string(),
                                        }
                                    } else {
                                        String::new()
                                    };
                                    let case_insensitive =
                                        flags.to_ascii_lowercase().contains('i');
                                    let regex_pattern = if case_insensitive {
                                        format!("(?i){}", pattern)
                                    } else {
                                        pattern
                                    };
                                    let re = regex::Regex::new(&regex_pattern)
                                        .map_err(|e| anyhow!("Invalid regex pattern: {}", e))?;
                                    let mut parts = Vec::new();
                                    let mut last_end = 0usize;
                                    for m in re.find_iter(&source) {
                                        parts.push(Value::Text(
                                            source[last_end..m.start()].to_string(),
                                        ));
                                        last_end = m.end();
                                    }
                                    parts.push(Value::Text(source[last_end..].to_string()));
                                    parts
                                    }
                                    _ => Vec::new(),
                                }
                            }
                            SrfKind::RegexpMatches => {
                                let arg0 = f.args.get(0).and_then(|arg| match arg {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                });
                                let arg1 = f.args.get(1).and_then(|arg| match arg {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                });
                                let arg2 = f.args.get(2).and_then(|arg| match arg {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                });
                                let (Some(arg0), Some(arg1)) = (arg0, arg1) else {
                                    return Err(anyhow!(
                                        "regexp_matches requires at least 2 arguments"
                                    ));
                                };
                                let source_val = sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    arg0,
                                    None,
                                    None,
                                )
                                .await?;
                                let source = match source_val {
                                    Value::Text(s) => Some(s),
                                    Value::Null => None,
                                    v => Some(v.to_string()),
                                };
                                let pattern_val = sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    arg1,
                                    None,
                                    None,
                                )
                                .await?;
                                let pattern = match pattern_val {
                                    Value::Text(s) => Some(s),
                                    Value::Null => None,
                                    v => Some(v.to_string()),
                                };
                                match (source, pattern) {
                                    (Some(source), Some(pattern)) => {
                                    let flags = if let Some(arg2) = arg2 {
                                        match sequences::eval_expr_with_sequences(
                                            &self.store,
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            arg2,
                                            None,
                                            None,
                                        )
                                        .await?
                                        {
                                            Value::Text(s) => s,
                                            Value::Null => String::new(),
                                            v => v.to_string(),
                                        }
                                    } else {
                                        String::new()
                                    };
                                    let global = flags.to_ascii_lowercase().contains('g');
                                    let case_insensitive =
                                        flags.to_ascii_lowercase().contains('i');
                                    let regex_pattern = if case_insensitive {
                                        format!("(?i){}", pattern)
                                    } else {
                                        pattern
                                    };
                                    let re = regex::Regex::new(&regex_pattern)
                                        .map_err(|e| anyhow!("Invalid regex pattern: {}", e))?;
                                    let mut out = Vec::new();
                                    if global {
                                        for caps in re.captures_iter(&source) {
                                            out.push(Value::Array(regexp_captures_to_values(&caps)));
                                        }
                                    } else if let Some(caps) = re.captures(&source) {
                                        out.push(Value::Array(regexp_captures_to_values(&caps)));
                                    }
                                    out
                                    }
                                    _ => Vec::new(),
                                }
                            }
                            SrfKind::EvalFunctionArray => {
                                match sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    expr,
                                    None,
                                    None,
                                )
                                .await?
                                {
                                    Value::Array(arr) => arr,
                                    Value::Null => Vec::new(),
                                    other => vec![other],
                                }
                            }
                        };

                        srf_positions.push((values.len(), output_values));
                        values.push(Value::Null);
                    } else {
                        let val = sequences::eval_expr_with_sequences(
                            &self.store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            expr,
                            None,
                            None,
                        )
                        .await?;
                        values.push(val);
                    }
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    cols.push(alias.value.clone());
                    if let Some(val) =
                        try_pg_sleep(&self.store, txn, db_id, sequence_values, search_path, expr)
                            .await?
                    {
                        values.push(val);
                        continue;
                    }
                    if let Some(kind) = srf_kind(expr) {
                        let Expr::Function(f) = expr else {
                            values.push(Value::Null);
                            continue;
                        };
                        let output_values = match kind {
                            SrfKind::Unnest => {
                                let arg_expr = f.args.first().and_then(|arg| match arg {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                });
                                if let Some(arg_expr) = arg_expr {
                                    match sequences::eval_expr_with_sequences(
                                        &self.store,
                                        txn,
                                        db_id,
                                        sequence_values,
                                        search_path,
                                        arg_expr,
                                        None,
                                        None,
                                    )
                                    .await?
                                    {
                                        Value::Array(arr) => arr,
                                        Value::Null => Vec::new(),
                                        other => vec![other],
                                    }
                                } else {
                                    Vec::new()
                                }
                            }
                            SrfKind::RegexpSplitToTable => {
                                let arg0 = f.args.get(0).and_then(|arg| match arg {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                });
                                let arg1 = f.args.get(1).and_then(|arg| match arg {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                });
                                let arg2 = f.args.get(2).and_then(|arg| match arg {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                });
                                let (Some(arg0), Some(arg1)) = (arg0, arg1) else {
                                    return Err(anyhow!(
                                        "regexp_split_to_table requires at least 2 arguments"
                                    ));
                                };
                                let source_val = sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    arg0,
                                    None,
                                    None,
                                )
                                .await?;
                                let source = match source_val {
                                    Value::Text(s) => Some(s),
                                    Value::Null => None,
                                    v => Some(v.to_string()),
                                };
                                let pattern_val = sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    arg1,
                                    None,
                                    None,
                                )
                                .await?;
                                let pattern = match pattern_val {
                                    Value::Text(s) => Some(s),
                                    Value::Null => None,
                                    v => Some(v.to_string()),
                                };
                                match (source, pattern) {
                                    (Some(source), Some(pattern)) => {
                                    let flags = if let Some(arg2) = arg2 {
                                        match sequences::eval_expr_with_sequences(
                                            &self.store,
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            arg2,
                                            None,
                                            None,
                                        )
                                        .await?
                                        {
                                            Value::Text(s) => s,
                                            Value::Null => String::new(),
                                            v => v.to_string(),
                                        }
                                    } else {
                                        String::new()
                                    };
                                    let case_insensitive =
                                        flags.to_ascii_lowercase().contains('i');
                                    let regex_pattern = if case_insensitive {
                                        format!("(?i){}", pattern)
                                    } else {
                                        pattern
                                    };
                                    let re = regex::Regex::new(&regex_pattern)
                                        .map_err(|e| anyhow!("Invalid regex pattern: {}", e))?;
                                    let mut parts = Vec::new();
                                    let mut last_end = 0usize;
                                    for m in re.find_iter(&source) {
                                        parts.push(Value::Text(
                                            source[last_end..m.start()].to_string(),
                                        ));
                                        last_end = m.end();
                                    }
                                    parts.push(Value::Text(source[last_end..].to_string()));
                                    parts
                                    }
                                    _ => Vec::new(),
                                }
                            }
                            SrfKind::RegexpMatches => {
                                let arg0 = f.args.get(0).and_then(|arg| match arg {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                });
                                let arg1 = f.args.get(1).and_then(|arg| match arg {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                });
                                let arg2 = f.args.get(2).and_then(|arg| match arg {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    _ => None,
                                });
                                let (Some(arg0), Some(arg1)) = (arg0, arg1) else {
                                    return Err(anyhow!(
                                        "regexp_matches requires at least 2 arguments"
                                    ));
                                };
                                let source_val = sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    arg0,
                                    None,
                                    None,
                                )
                                .await?;
                                let source = match source_val {
                                    Value::Text(s) => Some(s),
                                    Value::Null => None,
                                    v => Some(v.to_string()),
                                };
                                let pattern_val = sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    arg1,
                                    None,
                                    None,
                                )
                                .await?;
                                let pattern = match pattern_val {
                                    Value::Text(s) => Some(s),
                                    Value::Null => None,
                                    v => Some(v.to_string()),
                                };
                                match (source, pattern) {
                                    (Some(source), Some(pattern)) => {
                                    let flags = if let Some(arg2) = arg2 {
                                        match sequences::eval_expr_with_sequences(
                                            &self.store,
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            arg2,
                                            None,
                                            None,
                                        )
                                        .await?
                                        {
                                            Value::Text(s) => s,
                                            Value::Null => String::new(),
                                            v => v.to_string(),
                                        }
                                    } else {
                                        String::new()
                                    };
                                    let global = flags.to_ascii_lowercase().contains('g');
                                    let case_insensitive =
                                        flags.to_ascii_lowercase().contains('i');
                                    let regex_pattern = if case_insensitive {
                                        format!("(?i){}", pattern)
                                    } else {
                                        pattern
                                    };
                                    let re = regex::Regex::new(&regex_pattern)
                                        .map_err(|e| anyhow!("Invalid regex pattern: {}", e))?;
                                    let mut out = Vec::new();
                                    if global {
                                        for caps in re.captures_iter(&source) {
                                            out.push(Value::Array(regexp_captures_to_values(&caps)));
                                        }
                                    } else if let Some(caps) = re.captures(&source) {
                                        out.push(Value::Array(regexp_captures_to_values(&caps)));
                                    }
                                    out
                                    }
                                    _ => Vec::new(),
                                }
                            }
                            SrfKind::EvalFunctionArray => {
                                match sequences::eval_expr_with_sequences(
                                    &self.store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    expr,
                                    None,
                                    None,
                                )
                                .await?
                                {
                                    Value::Array(arr) => arr,
                                    Value::Null => Vec::new(),
                                    other => vec![other],
                                }
                            }
                        };

                        srf_positions.push((values.len(), output_values));
                        values.push(Value::Null);
                    } else {
                        let val = sequences::eval_expr_with_sequences(
                            &self.store,
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            expr,
                            None,
                            None,
                        )
                        .await?;
                        values.push(val);
                    }
                }
                _ => return Err(anyhow!("Unsupported select item in tableless query")),
            }
        }

        let rows = if !srf_positions.is_empty() {
            let max_len = srf_positions
                .iter()
                .map(|(_, arr)| arr.len())
                .max()
                .unwrap_or(0);
            let mut result_rows = Vec::new();
            for i in 0..max_len {
                let mut row_values = values.clone();
                for (col_idx, arr) in &srf_positions {
                    row_values[*col_idx] = arr.get(i).cloned().unwrap_or(Value::Null);
                }
                result_rows.push(Row::new(row_values));
            }
            result_rows
        } else {
            vec![Row::new(values)]
        };

        let empty_schema = TableSchema::default();
        let mut column_types: Vec<DataType> = rows
            .first()
            .map(|row| {
                row.values
                    .iter()
                    .map(|v| v.data_type().unwrap_or(DataType::Text))
                    .collect()
            })
            .unwrap_or_else(|| vec![DataType::Text; cols.len()]);

        // Refine timestamp-typed values that are actually `timestamptz` per SQL semantics.
        for (idx, item) in select.projection.iter().enumerate() {
            if !matches!(column_types.get(idx), Some(DataType::Timestamp)) {
                continue;
            }
            let expr = match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
                _ => continue,
            };
            if matches!(infer_expr_type(expr, &empty_schema), DataType::TimestampTz) {
                column_types[idx] = DataType::TimestampTz;
            }
        }

        Ok(ExecuteResult::Select {
            column_types: Some(column_types),
            columns: cols,
            rows,
            timezone: session_context::current_timezone(),
        })
    }

    pub(crate) fn execute_set_operation<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        op: &'a SetOperator,
        quantifier: &'a SetQuantifier,
        left: &'a SetExpr,
        right: &'a SetExpr,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExecuteResult>> + Send + 'a>>
    {
        Box::pin(async move {
            let left_result = self
                .execute_set_expr(txn, db_id, sequence_values, search_path, left, ctes)
                .await?;
            let right_result = self
                .execute_set_expr(txn, db_id, sequence_values, search_path, right, ctes)
                .await?;

            let (left_cols, left_rows) = match left_result {
                ExecuteResult::Select {
                    columns,
                    column_types: _,
                    rows,
                    timezone: _,
                } => (columns, rows),
                _ => return Err(anyhow!("Left side of set operation must be SELECT")),
            };
            let (right_cols, right_rows) = match right_result {
                ExecuteResult::Select {
                    columns,
                    column_types: _,
                    rows,
                    timezone: _,
                } => (columns, rows),
                _ => return Err(anyhow!("Right side of set operation must be SELECT")),
            };

            if left_cols.len() != right_cols.len() {
                return Err(anyhow!("Column count mismatch in set operation"));
            }

            let is_all = query::is_set_quantifier_all(quantifier);
            let rows = match op {
                SetOperator::Union => query::apply_union(left_rows, right_rows, is_all),
                SetOperator::Intersect => query::apply_intersect(left_rows, right_rows, is_all),
                SetOperator::Except => query::apply_except(left_rows, right_rows, is_all),
            };

            Ok(ExecuteResult::Select {
                column_types: None,
                columns: left_cols,
                rows,
                timezone: session_context::current_timezone(),
            })
        })
    }

    fn execute_set_expr<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        expr: &'a SetExpr,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ExecuteResult>> + Send + 'a>>
    {
        Box::pin(async move {
            match expr {
                SetExpr::Select(s) => {
                    let query = Query {
                        with: None,
                        body: Box::new(SetExpr::Select(s.clone())),
                        order_by: vec![],
                        limit: None,
                        offset: None,
                        fetch: None,
                        locks: vec![],
                        limit_by: vec![],
                        for_clause: None,
                    };
                    self.execute_query_with_ctes(txn, db_id, sequence_values, search_path, &query, ctes)
                        .await
                }
                SetExpr::SetOperation {
                    op,
                    set_quantifier,
                    left,
                    right,
                } => {
                    self.execute_set_operation(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        op,
                        set_quantifier,
                        left,
                        right,
                        ctes,
                    )
                    .await
                }
                _ => Err(anyhow!("Unsupported set expression")),
            }
        })
    }

    async fn execute_drop_schema(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        _search_path: &[String],
        names: &[sqlparser::ast::ObjectName],
        if_exists: bool,
    ) -> Result<ExecuteResult> {
        for name in names {
            let (schema_prefix, schema) = names::split_object_name(name)?;
            if schema_prefix.is_some() {
                return Err(anyhow!("Invalid schema name '{}'", name));
            }
            self.store
                .drop_schema_restrict(txn, db_id, &schema, if_exists)
                .await?;
        }
        Ok(ExecuteResult::CommandComplete { tag: "DROP SCHEMA" })
    }
    async fn execute_show_tables(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
    ) -> Result<ExecuteResult> {
        let current_schema = names::default_schema(search_path);
        let mut tables = Vec::new();
        for full_name in self.store.list_tables(txn, db_id).await? {
            match names::parse_full_name(&full_name) {
                Ok((schema, name)) => {
                    if schema == current_schema {
                        tables.push(name);
                    }
                }
                Err(_) => tables.push(full_name),
            }
        }
        Ok(ExecuteResult::ShowTables { tables })
    }

    async fn execute_explain(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        statement: &Statement,
        analyze: bool,
        _verbose: bool,
    ) -> Result<ExecuteResult> {
        let (actual_rows, execution_time_ms) = if analyze {
            match statement {
                Statement::Query(query) => {
                    let start = Instant::now();
                    let result = self
                        .execute_query(txn, db_id, sequence_values, search_path, query)
                        .await?;
                    let elapsed = start.elapsed();
                    let actual_rows = match result {
                        ExecuteResult::Select { rows, .. } => rows.len(),
                        _ => 0,
                    };
                    (Some(actual_rows), Some(elapsed.as_secs_f64() * 1000.0))
                }
                _ => {
                    return Err(anyhow!(
                        "EXPLAIN (ANALYZE) is only supported for SELECT/WITH statements"
                    ));
                }
            }
        } else {
            (None, None)
        };

        let tables = self.store.list_tables(txn, db_id).await?;
        let mut schemas_by_full: HashMap<String, TableSchema> = HashMap::new();
        let mut schemas_by_short: HashMap<String, Option<TableSchema>> = HashMap::new();
        for table_name in &tables {
            if let Ok(Some(schema)) = self.store.get_schema(txn, db_id, table_name).await {
                schemas_by_full.insert(table_name.clone(), schema.clone());

                // EXPLAIN queries often refer to tables without schema qualification.
                // Provide a short-name lookup when the name is unambiguous.
                let short = table_name
                    .rsplit('.')
                    .next()
                    .unwrap_or(table_name.as_str())
                    .to_string();
                match schemas_by_short.get(&short) {
                    None => {
                        schemas_by_short.insert(short, Some(schema));
                    }
                    Some(Some(_)) => {
                        schemas_by_short.insert(short, None);
                    }
                    Some(None) => {}
                }
            }
        }

        let schema_lookup = |table_name: &str| -> Option<TableSchema> {
            if let Some(schema) = schemas_by_full.get(table_name) {
                return Some(schema.clone());
            }
            schemas_by_short.get(table_name).and_then(|s| s.clone())
        };

        let row_count_lookup = |_table_name: &str| -> usize { 1000 };

        let plan = explain::generate_plan(statement, schema_lookup, row_count_lookup);
        let mut plan_text = explain::format_plan_text(&plan, 0);
        if let (Some(actual_rows), Some(execution_time_ms)) = (actual_rows, execution_time_ms) {
            use std::fmt::Write;
            writeln!(&mut plan_text, "Actual Rows: {}", actual_rows).unwrap();
            writeln!(
                &mut plan_text,
                "Execution Time: {:.3} ms",
                execution_time_ms
            )
            .unwrap();
        }

        let lines: Vec<Row> = plan_text
            .lines()
            .map(|line| Row::new(vec![Value::Text(line.to_string())]))
            .collect();

        Ok(ExecuteResult::Select {
            column_types: None,
            columns: vec!["QUERY PLAN".to_string()],
            rows: lines,
            timezone: session_context::current_timezone(),
        })
    }

    pub(crate) async fn scan_and_fill(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        schema: &TableSchema,
    ) -> Result<Vec<Row>> {
        self.scan_and_fill_with_limit(txn, db_id, table_name, schema, None)
            .await
    }

    pub(crate) async fn scan_and_fill_with_limit(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        schema: &TableSchema,
        limit: Option<usize>,
    ) -> Result<Vec<Row>> {
        let rows = self.store.scan(txn, db_id, table_name, limit).await?;
        let mut filled_rows = Vec::with_capacity(rows.len());
        for mut row in rows {
            fill_row_defaults(&mut row, schema)?;
            filled_rows.push(row);
        }
        Ok(filled_rows)
    }

    pub fn parse_value_for_copy(&self, val: &str, data_type: &DataType) -> Value {
        parse_value_for_copy(val, data_type)
    }

    pub async fn execute_copy_insert(
        &self,
        session: &mut Session,
        table_name: &str,
        col_values: Vec<(String, Value)>,
    ) -> Result<()> {
        let is_autocommit = !session.is_in_transaction();

        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let db_id = session.current_database_id();
            let (txn, sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .ok_or_else(|| anyhow!("Transaction must be active"))?;
            let schema = self
                .store
                .get_schema(txn, db_id, table_name)
                .await?
                .ok_or_else(|| anyhow!("Table '{}' not found", table_name))?;

            let enum_cache = dml::build_enum_label_cache(&self.store, txn, db_id, &schema).await?;

            let mut row_values = vec![Value::Null; schema.columns.len()];
            let mut indices: Vec<usize> = Vec::with_capacity(col_values.len());

            for (col_name, value) in col_values {
                if let Some(idx) = schema.column_index(&col_name) {
                    row_values[idx] = value;
                    indices.push(idx);
                }
            }
            indices.sort_unstable();
            indices.dedup();

            dml::fill_missing_columns(
                &self.store,
                txn,
                db_id,
                sequence_values,
                search_path,
                &schema,
                &mut row_values,
                &indices,
            )
            .await?;
            dml::coerce_row_values(&schema, &mut row_values)?;
            let row = Row { values: row_values };
            dml::validate_check_constraints(&schema, &row)?;

            let on_conflict = None;
            let _ = dml::execute_insert_row(
                &self.store,
                txn,
                db_id,
                table_name,
                &schema,
                row,
                &on_conflict,
                &enum_cache,
            )
            .await?;

            Ok::<(), anyhow::Error>(())
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::{
        cast_current_setting_value, is_current_setting_function, is_set_config_function,
        parse_search_path_guc_value, set_variable_value_to_string, starts_with_ignore_ascii_case,
        try_parse_const_bool, try_parse_const_text, unwrap_top_level_cast,
    };
    use crate::types::Value;
    use sqlparser::ast::{DataType, DateTimeField, Expr, Ident, Interval, ObjectName, Value as SqlValue};

    #[test]
    fn test_starts_with_ignore_ascii_case_is_byte_safe() {
        assert!(starts_with_ignore_ascii_case("rollback;", "ROLLBACK"));
        assert!(!starts_with_ignore_ascii_case("é💩€ROLLBACK", "ROLLBACK"));
    }

    #[test]
    fn test_set_variable_value_to_string_basic() {
        assert_eq!(
            set_variable_value_to_string(&[Expr::Value(SqlValue::Number(
                "0".to_string(),
                false
            ))])
            .unwrap(),
            "0"
        );
        assert_eq!(
            set_variable_value_to_string(&[Expr::Value(SqlValue::SingleQuotedString(
                "UTF8".to_string()
            ))])
            .unwrap(),
            "UTF8"
        );
        assert_eq!(
            set_variable_value_to_string(&[Expr::Value(SqlValue::Boolean(false))]).unwrap(),
            "off"
        );
        assert_eq!(
            set_variable_value_to_string(&[Expr::Identifier(Ident::new("on"))]).unwrap(),
            "on"
        );
        assert_eq!(
            set_variable_value_to_string(&[Expr::Identifier(Ident::new("OFF"))]).unwrap(),
            "off"
        );
    }

    #[test]
    fn test_set_variable_value_to_string_timezone_interval() {
        let expr = Expr::Interval(Interval {
            value: Box::new(Expr::Value(SqlValue::SingleQuotedString("+00:00".to_string()))),
            leading_field: Some(DateTimeField::Hour),
            leading_precision: None,
            last_field: Some(DateTimeField::Minute),
            fractional_seconds_precision: None,
        });
        assert_eq!(set_variable_value_to_string(&[expr]).unwrap(), "+00:00");
    }

    #[test]
    fn test_parse_search_path_guc_value() {
        let parsed = parse_search_path_guc_value("public, \"$user\", \"MySchema\", foo");
        assert_eq!(
            parsed,
            vec![
                "public".to_string(),
                "$user".to_string(),
                "MySchema".to_string(),
                "foo".to_string()
            ]
        );
    }

    #[test]
    fn test_try_parse_const_text_and_bool() {
        let expr = Expr::Value(SqlValue::SingleQuotedString("search_path".to_string()));
        assert_eq!(try_parse_const_text(&expr).as_deref(), Some("search_path"));

        let cast_expr = Expr::Cast {
            expr: Box::new(Expr::Value(SqlValue::SingleQuotedString("x".to_string()))),
            data_type: DataType::Text,
            format: None,
        };
        assert_eq!(try_parse_const_text(&cast_expr).as_deref(), Some("x"));

        assert_eq!(
            try_parse_const_bool(&Expr::Identifier(Ident::new("true"))),
            Some(true)
        );
        assert_eq!(
            try_parse_const_bool(&Expr::Value(SqlValue::Boolean(false))),
            Some(false)
        );
    }

    #[test]
    fn test_is_set_config_function() {
        assert!(is_set_config_function(&ObjectName(vec![Ident::new(
            "set_config"
        )])));
        assert!(is_set_config_function(&ObjectName(vec![
            Ident::new("pg_catalog"),
            Ident::new("set_config")
        ])));
        assert!(!is_set_config_function(&ObjectName(vec![Ident::new(
            "other"
        )])));
    }

    #[test]
    fn test_is_current_setting_function() {
        assert!(is_current_setting_function(&ObjectName(vec![Ident::new(
            "current_setting"
        )])));
        assert!(is_current_setting_function(&ObjectName(vec![
            Ident::new("pg_catalog"),
            Ident::new("current_setting")
        ])));
        assert!(!is_current_setting_function(&ObjectName(vec![Ident::new(
            "other"
        )])));
    }

    #[test]
    fn test_unwrap_top_level_cast() {
        let inner = Expr::Identifier(Ident::new("x"));
        let expr = Expr::Cast {
            expr: Box::new(Expr::Nested(Box::new(inner.clone()))),
            data_type: DataType::Int(None),
            format: None,
        };
        let (unwrapped, cast_to) = unwrap_top_level_cast(&expr);
        assert!(matches!(unwrapped, Expr::Identifier(_)));
        assert!(cast_to.is_some());
    }

    #[test]
    fn test_cast_current_setting_value_integer() {
        let v = cast_current_setting_value(Value::Text("160000".to_string()), &crate::types::DataType::Int32)
            .unwrap();
        assert_eq!(v, Value::Int32(160000));
    }

    mod write_conflict_retry_tests {
        use super::super::is_retryable_tikv_error;

        #[test]
        fn test_unrelated_tikv_error_not_retryable() {
            let tikv_err = tikv_client::Error::DuplicateKeyInsertion;
            let anyhow_err = anyhow::Error::new(tikv_err);
            assert!(!is_retryable_tikv_error(&anyhow_err));
        }

        #[test]
        fn test_non_tikv_error_not_retryable() {
            let anyhow_err = anyhow::anyhow!("some random error");
            assert!(!is_retryable_tikv_error(&anyhow_err));
        }

        #[test]
        fn test_region_error_not_retryable() {
            let tikv_err = tikv_client::Error::RegionForKeyNotFound { key: vec![1, 2, 3] };
            let anyhow_err = anyhow::Error::new(tikv_err);
            assert!(!is_retryable_tikv_error(&anyhow_err));
        }

        #[test]
        fn test_operation_after_commit_not_retryable() {
            let tikv_err = tikv_client::Error::OperationAfterCommitError;
            let anyhow_err = anyhow::Error::new(tikv_err);
            assert!(!is_retryable_tikv_error(&anyhow_err));
        }

        #[test]
        fn test_pessimistic_lock_with_non_conflict_inner_not_retryable() {
            let inner_err = tikv_client::Error::DuplicateKeyInsertion;
            let tikv_err = tikv_client::Error::PessimisticLockError {
                inner: Box::new(inner_err),
                success_keys: vec![],
            };
            let anyhow_err = anyhow::Error::new(tikv_err);
            assert!(!is_retryable_tikv_error(&anyhow_err));
        }

        #[test]
        fn test_undetermined_with_non_conflict_inner_not_retryable() {
            let inner_err = tikv_client::Error::DuplicateKeyInsertion;
            let tikv_err = tikv_client::Error::UndeterminedError(Box::new(inner_err));
            let anyhow_err = anyhow::Error::new(tikv_err);
            assert!(!is_retryable_tikv_error(&anyhow_err));
        }

        #[test]
        fn test_multiple_key_errors_all_non_conflict_not_retryable() {
            let err1 = tikv_client::Error::DuplicateKeyInsertion;
            let err2 = tikv_client::Error::NoPrimaryKey;
            let tikv_err = tikv_client::Error::MultipleKeyErrors(vec![err1, err2]);
            let anyhow_err = anyhow::Error::new(tikv_err);
            assert!(!is_retryable_tikv_error(&anyhow_err));
        }

        #[test]
        fn test_extracted_errors_all_non_conflict_not_retryable() {
            let err1 = tikv_client::Error::DuplicateKeyInsertion;
            let tikv_err = tikv_client::Error::ExtractedErrors(vec![err1]);
            let anyhow_err = anyhow::Error::new(tikv_err);
            assert!(!is_retryable_tikv_error(&anyhow_err));
        }

        #[test]
        fn test_nested_pessimistic_with_non_conflict_not_retryable() {
            let inner_err = tikv_client::Error::NoPrimaryKey;
            let multi_err = tikv_client::Error::MultipleKeyErrors(vec![inner_err]);
            let pessimistic_err = tikv_client::Error::PessimisticLockError {
                inner: Box::new(multi_err),
                success_keys: vec![],
            };
            let anyhow_err = anyhow::Error::new(pessimistic_err);
            assert!(!is_retryable_tikv_error(&anyhow_err));
        }
    }
}
