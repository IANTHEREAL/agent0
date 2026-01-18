//! SQL executor

use super::ddl;
use super::executor_functions_triggers::strip_leading_sql_comments;
use super::explain;
use super::helpers::{
    eval_default_expr, fill_row_defaults, get_expr_name, get_skip_reason, get_unsupported_reason,
    normalize_ident, parse_value_for_copy,
};
use super::names;
use super::query;
use super::rbac;
use super::sequences;
use super::udt;
use super::{parse_sql, ExecuteResult, ExecuteResults, Session};
use crate::auth::AuthManager;
use crate::storage::TikvStore;
use crate::types::{DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::{
    Expr, FunctionArg, FunctionArgExpr, Query, SelectItem, SetExpr, SetOperator, SetQuantifier,
    Statement,
};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tikv_client::Transaction;
use tracing::debug;

pub struct Executor {
    store: Arc<TikvStore>,
    auth_manager: AuthManager,
}

impl Executor {
    pub fn new(store: Arc<TikvStore>) -> Self {
        Self {
            store,
            auth_manager: AuthManager::new(),
        }
    }

    pub fn store(&self) -> Arc<TikvStore> {
        self.store.clone()
    }

    #[allow(dead_code)]
    pub fn auth_manager(&self) -> &AuthManager {
        &self.auth_manager
    }

    /// Execute a SQL statement string using the provided session
    /// Supports multiple statements separated by semicolons (e.g., "BEGIN; UPDATE...; COMMIT;")
    /// Returns all results for proper PostgreSQL Simple Query Protocol compliance.
    pub async fn execute(&self, session: &mut Session, sql: &str) -> Result<ExecuteResults> {
        let savepoints = session.savepoints();
        crate::txn::with_savepoints(savepoints, async {
            let sql_stripped = strip_leading_sql_comments(sql);
            let sql_trimmed = sql_stripped.trim_start();
            let starts_with = |prefix: &str| {
                sql_trimmed.len() >= prefix.len()
                    && sql_trimmed[..prefix.len()].eq_ignore_ascii_case(prefix)
            };

            if starts_with("CREATE OR REPLACE FUNCTION") || starts_with("CREATE FUNCTION") {
                return self
                    .execute_create_function_cmd(session, sql)
                    .await
                    .map(ExecuteResults::single);
            }
            if starts_with("DROP FUNCTION") {
                return self
                    .execute_drop_function_cmd(session, sql)
                    .await
                    .map(ExecuteResults::single);
            }
            if starts_with("CREATE CONSTRAINT TRIGGER") || starts_with("CREATE TRIGGER") {
                return self
                    .execute_create_trigger_cmd(session, sql)
                    .await
                    .map(ExecuteResults::single);
            }
            if starts_with("DROP TRIGGER") {
                return self
                    .execute_drop_trigger_cmd(session, sql)
                    .await
                    .map(ExecuteResults::single);
            }

            let sql_upper = sql_trimmed.trim().to_uppercase();
            if let Some(reason) = get_skip_reason(&sql_upper) {
                return Ok(ExecuteResults::single(ExecuteResult::Skipped {
                    message: reason,
                }));
            }

            if sql_upper.starts_with("REFRESH MATERIALIZED VIEW") {
                return self
                    .execute_refresh_materialized_view_cmd(session, sql)
                    .await
                    .map(ExecuteResults::single);
            }

            if sql_upper.starts_with("DROP MATERIALIZED VIEW") {
                return self
                    .execute_drop_materialized_view_cmd(session, sql)
                    .await
                    .map(ExecuteResults::single);
            }

            if sql_upper.starts_with("CALL ") {
                return self
                    .execute_call_cmd(session, sql)
                    .await
                    .map(ExecuteResults::single);
            }

            if sql_upper.starts_with("DROP PROCEDURE") {
                return self
                    .execute_drop_procedure_cmd(session, sql)
                    .await
                    .map(ExecuteResults::single);
            }

            if sql_upper.starts_with("CREATE PROCEDURE") {
                return self
                    .execute_create_procedure_cmd(session, sql)
                    .await
                    .map(ExecuteResults::single);
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
                    return self
                        .execute_create_type_enum_cmd(session, sql)
                        .await
                        .map(ExecuteResults::single);
                }
            }

            if sql_upper.starts_with("DROP TYPE") {
                return self
                    .execute_drop_type_cmd(session, sql)
                    .await
                    .map(ExecuteResults::single);
            }

            let statements = match parse_sql(sql) {
                Ok(stmts) => stmts,
                Err(e) => {
                    if let Some(reason) = get_unsupported_reason(&sql_upper) {
                        return Ok(ExecuteResults::single(ExecuteResult::Skipped {
                            message: reason,
                        }));
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

                let mut stmt_results: Vec<ExecuteResult> = Vec::new();

                match stmt {
                    // Transaction Control
                    Statement::StartTransaction { .. } => {
                        session.begin().await?;
                        stmt_results.push(ExecuteResult::Empty);
                    }
                    Statement::Commit { .. } => {
                        session.commit().await?;
                        stmt_results.push(ExecuteResult::Empty);
                    }
                    Statement::Savepoint { name } => {
                        session.create_savepoint(normalize_ident(name))?;
                        stmt_results.push(ExecuteResult::Empty);
                    }
                    Statement::ReleaseSavepoint { name } => {
                        let sp = normalize_ident(name);
                        session.release_savepoint(&sp)?;
                        stmt_results.push(ExecuteResult::Empty);
                    }
                    Statement::Rollback {
                        savepoint: Some(name),
                        ..
                    } => {
                        let sp = normalize_ident(name);
                        session.rollback_to_savepoint(&sp).await?;
                        stmt_results.push(ExecuteResult::Empty);
                    }
                    Statement::Rollback {
                        savepoint: None, ..
                    } => {
                        session.rollback().await?;
                        stmt_results.push(ExecuteResult::Empty);
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
                                    Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) => {
                                        for token in s.split(',') {
                                            let token = token.trim();
                                            if token.is_empty() {
                                                continue;
                                            }
                                            let schema = if token.starts_with('"')
                                                && token.ends_with('"')
                                                && token.len() >= 2
                                            {
                                                token[1..token.len() - 1].to_string()
                                            } else {
                                                token.to_lowercase()
                                            };
                                            new_search_path.push(schema);
                                        }
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
                            if new_search_path.len() == 1 && new_search_path[0] == "default" {
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
                        }
                        stmt_results.push(ExecuteResult::Empty);
                    }
                    // DDL/DML - delegated to session transaction management
                    _ => {
                        let is_autocommit = !session.is_in_transaction();

                        if is_autocommit {
                            session.begin().await?;
                        }

                        let res = async {
                            let (txn, sequence_values, search_path) = session
                                .get_mut_txn_sequence_values_and_search_path()
                                .expect("Transaction must be active");
                            let notices = self
                                .collect_notices_before_statement(txn, search_path, stmt)
                                .await?;
                            let result = self
                                .execute_statement_on_txn(txn, sequence_values, search_path, stmt)
                                .await?;
                            Ok::<(Vec<ExecuteResult>, ExecuteResult), anyhow::Error>((
                                notices, result,
                            ))
                        }
                        .await;

                        if is_autocommit {
                            if res.is_ok() {
                                session.commit().await?;
                            } else {
                                session.rollback().await?;
                            }
                        }

                        let (notices, result) = res?;
                        stmt_results.extend(notices);
                        stmt_results.push(result);
                    }
                }
                results.extend(stmt_results);
            }

            Ok(ExecuteResults(results))
        })
        .await
    }

    async fn collect_notices_before_statement(
        &self,
        txn: &mut Transaction,
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
                    let exists = super::names::resolve_existing_table_name(
                        self.store.as_ref(),
                        txn,
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

    /// Execute a parsed SQL statement on a given transaction
    pub(crate) async fn execute_statement_on_txn(
        &self,
        txn: &mut Transaction,
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
                let idx_name_str = index_name.0.last().unwrap().value.as_str();
                self.execute_create_index(
                    txn,
                    search_path,
                    idx_name_str,
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
                        ddl::execute_drop_table(&self.store, txn, search_path, names, *if_exists)
                            .await
                    }
                    ObjectType::View => {
                        ddl::execute_drop_view(&self.store, txn, search_path, names, *if_exists)
                            .await
                    }
                    ObjectType::Index => {
                        self.execute_drop_index(txn, search_path, names, *if_exists)
                            .await
                    }
                    ObjectType::Role => {
                        rbac::execute_drop_role(&self.auth_manager, txn, names, *if_exists).await
                    }
                    ObjectType::Sequence => {
                        sequences::execute_drop_sequence(
                            &self.store,
                            txn,
                            search_path,
                            names,
                            *if_exists,
                        )
                        .await
                    }
                    ObjectType::Schema => {
                        self.execute_drop_schema(txn, search_path, names, *if_exists)
                            .await
                    }
                    _ => Ok(ExecuteResult::Empty),
                }
            }
            Statement::Truncate { table_name, .. } => {
                ddl::execute_truncate(&self.store, txn, search_path, table_name).await
            }
            Statement::AlterTable {
                name, operations, ..
            } => {
                for op in operations {
                    self.execute_alter_table(txn, search_path, name, op).await?;
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
                self.execute_query(txn, sequence_values, search_path, query)
                    .await
            }
            Statement::ShowTables { .. } => self.execute_show_tables(txn, search_path).await,
            Statement::SetVariable { .. }
            | Statement::SetTimeZone { .. }
            | Statement::SetNames { .. }
            | Statement::SetTransaction { .. } => Ok(ExecuteResult::Empty),
            Statement::CreateType {
                name,
                representation,
            } => {
                udt::execute_create_type(&self.store, txn, search_path, name, representation).await
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
                    .create_schema(txn, &schema, *if_not_exists)
                    .await?;
                Ok(ExecuteResult::Empty)
            }
            Statement::CreateFunction { .. } => Ok(ExecuteResult::Empty),
            Statement::CreateProcedure {
                name, params, body, ..
            } => {
                self.execute_create_procedure(txn, search_path, name, params.as_deref(), body)
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
                        search_path,
                        name,
                        query,
                        *or_replace,
                    )
                    .await
                }
            }
            Statement::AlterIndex { .. } => Ok(ExecuteResult::Empty),
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
            Statement::Comment { .. } => Ok(ExecuteResult::Empty),
            Statement::Copy { .. } => Ok(ExecuteResult::Empty),
            Statement::Explain {
                statement,
                analyze,
                verbose,
                ..
            } => {
                self.execute_explain(
                    txn,
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
                        func_name,
                        search_path,
                    )
                    .await?;
                    let func_full_name = match resolved {
                        Some(resolved) => resolved.full,
                        None => names::resolve_ddl_object_name(func_name, search_path)?.full,
                    };
                    last_name = Some(func_full_name.clone());
                    let dropped = self.store.drop_function(txn, &func_full_name).await?;
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
                sequence_values,
                search_path,
                expr,
                row,
                schema,
            )
            .await
        } else {
            super::expr::eval_expr(expr, row, schema)
        }
    }

    pub(crate) async fn eval_expr_join_maybe_sequence(
        &self,
        txn: &mut Transaction,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        expr: &Expr,
        join_ctx: &super::expr::JoinContext<'_>,
    ) -> Result<Value> {
        if sequences::expr_needs_async_eval(expr) {
            sequences::eval_expr_join_with_sequences(
                &self.store,
                txn,
                sequence_values,
                search_path,
                expr,
                join_ctx,
            )
            .await
        } else {
            super::expr::eval_expr_join(expr, join_ctx)
        }
    }

    pub(crate) async fn execute_query(
        &self,
        txn: &mut Transaction,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
    ) -> Result<ExecuteResult> {
        let ctes = self
            .build_cte_context(txn, sequence_values, search_path, query)
            .await?;
        self.execute_query_with_ctes(txn, sequence_values, search_path, query, &ctes)
            .await
    }

    pub(crate) async fn execute_tableless_query(
        &self,
        txn: &mut Transaction,
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

        let resolved_projection = self
            .resolve_projection_subqueries(txn, sequence_values, search_path, &select.projection, ctes)
            .await?;

        let mut cols = Vec::new();
        let mut values = Vec::new();
        let mut srf_positions: Vec<(usize, Vec<Value>)> = Vec::new();

        for item in &resolved_projection {
            match item {
                SelectItem::UnnamedExpr(expr) => {
                    cols.push(get_expr_name(expr));
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

        Ok(ExecuteResult::Select {
            column_types: None,
            columns: cols,
            rows,
        })
    }

    pub(crate) fn execute_set_operation<'a>(
        &'a self,
        txn: &'a mut Transaction,
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
                .execute_set_expr(txn, sequence_values, search_path, left, ctes)
                .await?;
            let right_result = self
                .execute_set_expr(txn, sequence_values, search_path, right, ctes)
                .await?;

            let (left_cols, left_rows) = match left_result {
                ExecuteResult::Select {
                    columns,
                    column_types: _,
                    rows,
                } => (columns, rows),
                _ => return Err(anyhow!("Left side of set operation must be SELECT")),
            };
            let (right_cols, right_rows) = match right_result {
                ExecuteResult::Select {
                    columns,
                    column_types: _,
                    rows,
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
            })
        })
    }

    fn execute_set_expr<'a>(
        &'a self,
        txn: &'a mut Transaction,
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
                    self.execute_query_with_ctes(txn, sequence_values, search_path, &query, ctes)
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
                .drop_schema_restrict(txn, &schema, if_exists)
                .await?;
        }
        Ok(ExecuteResult::Empty)
    }
    async fn execute_show_tables(
        &self,
        txn: &mut Transaction,
        search_path: &[String],
    ) -> Result<ExecuteResult> {
        let current_schema = names::default_schema(search_path);
        let mut tables = Vec::new();
        for full_name in self.store.list_tables(txn).await? {
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
                        .execute_query(txn, sequence_values, search_path, query)
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

        let tables = self.store.list_tables(txn).await?;
        let mut schemas: HashMap<String, TableSchema> = HashMap::new();
        for table_name in &tables {
            if let Ok(Some(schema)) = self.store.get_schema(txn, table_name).await {
                schemas.insert(table_name.clone(), schema);
            }
        }

        let schema_lookup =
            |table_name: &str| -> Option<TableSchema> { schemas.get(table_name).cloned() };

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
        })
    }

    pub(crate) async fn scan_and_fill(
        &self,
        txn: &mut Transaction,
        table_name: &str,
        schema: &TableSchema,
    ) -> Result<Vec<Row>> {
        let rows = self.store.scan(txn, table_name).await?;
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
            let txn = session.get_mut_txn().expect("Transaction must be active");
            let schema = self
                .store
                .get_schema(txn, table_name)
                .await?
                .ok_or_else(|| anyhow!("Table '{}' not found", table_name))?;

            let mut row_values = vec![Value::Null; schema.columns.len()];

            for (col_name, value) in col_values {
                if let Some(idx) = schema.column_index(&col_name) {
                    row_values[idx] = value;
                }
            }

            for (i, col) in schema.columns.iter().enumerate() {
                if matches!(row_values[i], Value::Null) {
                    if col.is_serial {
                        let next_id = self.store.next_sequence_value(txn, schema.table_id).await?;
                        row_values[i] = Value::Int32(next_id);
                    } else if let Some(ref default_expr) = col.default_expr {
                        row_values[i] = eval_default_expr(default_expr)?;
                    }
                }
            }

            let mut row = Row { values: row_values };
            fill_row_defaults(&mut row, &schema)?;

            self.store.insert(txn, &schema.name, row.clone()).await?;

            let pk_values = schema.get_pk_values(&row);
            for index in &schema.indexes {
                let idx_values = schema.get_index_values(index, &row);
                self.store
                    .create_index_entry(
                        txn,
                        schema.table_id,
                        index.id,
                        &idx_values,
                        &pk_values,
                        index.unique,
                    )
                    .await?;
            }

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
