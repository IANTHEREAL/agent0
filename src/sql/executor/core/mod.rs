//! SQL executor core

mod alter;
mod copy;
mod dispatch;
mod guc;
mod misc;
mod observability;
mod query_exec;
mod retry;
mod scan;
mod settings_tableless;
mod statement;
mod timeout;

#[cfg(test)]
mod tests;

use guc::{
    parse_search_path_guc_value, set_variable_value_to_string, try_parse_const_bool,
    try_parse_const_text,
};
use misc::{
    get_skip_reason, get_unsupported_reason, split_sql_statements, starts_with_ignore_ascii_case,
};
use observability::{
    is_observability_system_query, is_observability_tableless_query, OBSERVABILITY_USER,
};
use retry::is_retryable_tikv_error;
use settings_tableless::{
    cast_current_setting_value, is_current_setting_function, is_set_config_function,
    try_execute_current_setting_select, try_execute_set_config_select, unwrap_top_level_cast,
};
use timeout::StatementTimeoutError;

use super::super::alter_owner;
use super::super::alter_sequence_owned_by;
use super::super::comment_on;
use super::super::ddl;
use super::super::dml;
use super::super::explain;
use super::super::names;
use super::super::names::normalize_ident;
use super::super::projection::{fill_row_defaults, get_expr_name, infer_expr_type};
use super::super::query;
use super::super::rbac;
use super::super::sequences;
use super::super::statement_time;
use super::super::udt;
use super::super::value_coercion::parse_value_for_copy;
use super::super::{parse_sql, ExecuteResult, ExecuteResults, InFailedSqlTransaction, Session};
use super::triggers::strip_leading_sql_comments;
use crate::auth::AuthManager;
use crate::observability::TenantObservability;
use crate::session_context;
use crate::sql::error::SqlError;
use crate::storage::{with_kv_read_stats, KvReadStatsSnapshot, TikvStore};
use crate::types::{DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use rust_decimal::prelude::ToPrimitive;
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, Query, ReferentialAction, SelectItem, SetExpr, SetOperator,
    SetQuantifier, Statement, TableFactor, Visit, Visitor,
};

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tikv_client::Transaction;
use tracing::debug;

pub struct Executor {
    store: Arc<TikvStore>,
    auth_manager: AuthManager,
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

    pub fn tenant_keyspace(&self) -> &str {
        &self.tenant_keyspace
    }

    pub fn observability(&self) -> &Arc<TenantObservability> {
        &self.observability
    }

    #[allow(dead_code)] // accessor for future permission checks
    pub fn auth_manager(&self) -> &AuthManager {
        &self.auth_manager
    }
}
