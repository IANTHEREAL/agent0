use super::ddl;
use super::executor::Executor;
use super::helpers::infer_data_type;
use super::names;
use super::{parse_sql, ExecuteResult, Session};
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use sqlparser::ast::{ObjectName, Query, Statement};
use std::collections::HashMap;
use tikv_client::Transaction;

fn object_name_from_token(token: &str) -> Result<ObjectName> {
    let token = token.trim().trim_end_matches(';');
    if token.is_empty() {
        return Err(anyhow!("Missing object name"));
    }
    let parts: Vec<&str> = token.split('.').collect();
    match parts.as_slice() {
        [name] if !name.is_empty() => Ok(ObjectName(vec![sqlparser::ast::Ident::new(*name)])),
        [schema, name] if !schema.is_empty() && !name.is_empty() => Ok(ObjectName(vec![
            sqlparser::ast::Ident::new(*schema),
            sqlparser::ast::Ident::new(*name),
        ])),
        _ => Err(anyhow!("Invalid object name '{}'", token)),
    }
}

impl Executor {
    pub(crate) async fn execute_create_materialized_view(
        &self,
        txn: &mut Transaction,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        name: &ObjectName,
        query: &Query,
        or_replace: bool,
    ) -> Result<ExecuteResult> {
        let result = self
            .execute_query_with_ctes(txn, sequence_values, search_path, query, &HashMap::new())
            .await?;
        let (columns, rows) = match result {
            ExecuteResult::Select {
                columns,
                column_types: _,
                rows,
            } => (columns, rows),
            _ => return Err(anyhow!("Materialized view must be a SELECT query")),
        };

        let resolved = names::resolve_ddl_object_name(name, search_path)?;
        if !self.store().schema_exists(txn, &resolved.schema).await? {
            return Err(anyhow!("schema '{}' does not exist", resolved.schema));
        }
        let view_name = resolved.full;

        let table_id = self.store().next_table_id(txn).await?;
        let mut col_defs: Vec<ColumnDef> = vec![ColumnDef {
            name: "_mv_rowid".to_string(),
            data_type: DataType::Int64,
            nullable: false,
            primary_key: true,
            default_expr: None,
            is_serial: true,
            unique: true,
        }];
        col_defs.extend(columns.iter().enumerate().map(|(i, col_name)| {
            let data_type = if rows.is_empty() {
                DataType::Text
            } else {
                infer_data_type(&rows[0].values[i])
            };
            ColumnDef {
                name: col_name.clone(),
                data_type,
                nullable: true,
                primary_key: false,
                default_expr: None,
                is_serial: false,
                unique: false,
            }
        }));

        let rows_with_rowid: Vec<Row> = rows
            .into_iter()
            .enumerate()
            .map(|(i, mut row)| {
                let mut values = vec![Value::Int64((i + 1) as i64)];
                values.append(&mut row.values);
                Row::new(values)
            })
            .collect();

        let schema = TableSchema {
            table_id,
            name: view_name.clone(),
            columns: col_defs,
            version: 1,
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
        };

        ddl::execute_create_materialized_view(
            &self.store(),
            txn,
            search_path,
            name,
            query,
            or_replace,
            schema,
            rows_with_rowid,
        )
        .await
    }

    pub(crate) async fn execute_refresh_materialized_view_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let sql_upper = sql.trim().to_uppercase();
        let rest = sql_upper
            .strip_prefix("REFRESH MATERIALIZED VIEW")
            .ok_or_else(|| anyhow!("Invalid REFRESH MATERIALIZED VIEW syntax"))?
            .trim();

        let view_name = rest
            .split_whitespace()
            .next()
            .ok_or_else(|| anyhow!("Missing view name"))?
            .trim_end_matches(';')
            .to_lowercase();

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let (txn, sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let view_obj = object_name_from_token(&view_name)?;
            let resolved = names::resolve_existing_materialized_view_name(
                self.store().as_ref(),
                txn,
                &view_obj,
                search_path,
            )
            .await?
            .ok_or_else(|| anyhow!("Materialized view '{}' does not exist", view_name))?;
            let view_full_name = resolved.full;

            let query_str: String = self
                .store()
                .get_materialized_view(txn, &view_full_name)
                .await?
                .ok_or_else(|| anyhow!("Materialized view '{}' does not exist", view_full_name))?;

            let ast = parse_sql(&query_str)?;
            let query = match ast.into_iter().next() {
                Some(Statement::Query(q)) => q,
                _ => return Err(anyhow!("Invalid materialized view query")),
            };

            let result = self
                .execute_query_with_ctes(txn, sequence_values, search_path, &query, &HashMap::new())
                .await?;
            let rows = match result {
                ExecuteResult::Select { rows, .. } => rows,
                _ => return Err(anyhow!("Materialized view must be a SELECT query")),
            };

            let rows_with_rowid: Vec<Row> = rows
                .into_iter()
                .enumerate()
                .map(|(i, mut row)| {
                    let mut values = vec![Value::Int64((i + 1) as i64)];
                    values.append(&mut row.values);
                    Row::new(values)
                })
                .collect();

            ddl::execute_refresh_materialized_view(
                &self.store(),
                txn,
                &view_full_name,
                rows_with_rowid,
            )
            .await
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

    pub(crate) async fn execute_drop_materialized_view_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let sql_upper = sql.trim().to_uppercase();
        let rest = sql_upper
            .strip_prefix("DROP MATERIALIZED VIEW")
            .ok_or_else(|| anyhow!("Invalid DROP MATERIALIZED VIEW syntax"))?
            .trim();

        let if_exists = rest.starts_with("IF EXISTS");
        let name_part = if if_exists {
            rest.strip_prefix("IF EXISTS").unwrap().trim()
        } else {
            rest
        };

        let view_name = name_part
            .split_whitespace()
            .next()
            .ok_or_else(|| anyhow!("Missing view name"))?
            .trim_end_matches(';')
            .to_lowercase();

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");
            let name = object_name_from_token(&view_name)?;
            ddl::execute_drop_materialized_view(&self.store(), txn, search_path, &[name], if_exists)
                .await
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

    pub(crate) async fn execute_create_procedure(
        &self,
        txn: &mut Transaction,
        search_path: &[String],
        name: &ObjectName,
        params: Option<&[sqlparser::ast::ProcedureParam]>,
        body: &[Statement],
    ) -> Result<ExecuteResult> {
        let resolved = names::resolve_ddl_object_name(name, search_path)?;
        if !self.store().schema_exists(txn, &resolved.schema).await? {
            return Err(anyhow!("schema '{}' does not exist", resolved.schema));
        }
        let proc_name = resolved.full;

        let param_defs: Vec<String> = params
            .map(|p| {
                p.iter()
                    .map(|param| format!("{} {}", param.name.value, param.data_type))
                    .collect()
            })
            .unwrap_or_default();

        let body_stmts: Vec<String> = body.iter().map(|s| s.to_string()).collect();

        let definition = format!(
            "PARAMS:{}\nBODY:{}",
            param_defs.join(","),
            body_stmts.join(";")
        );

        self.store()
            .create_procedure(txn, &proc_name, &definition)
            .await?;

        Ok(ExecuteResult::CreateProcedure { proc_name })
    }

    pub(crate) async fn execute_call_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let sql_trimmed = sql.trim();
        let rest = sql_trimmed
            .strip_prefix("CALL ")
            .or_else(|| sql_trimmed.strip_prefix("call "))
            .ok_or_else(|| anyhow!("Invalid CALL syntax"))?
            .trim();

        let paren_pos = rest.find('(').unwrap_or(rest.len());
        let proc_name = rest[..paren_pos].trim().to_lowercase();

        let args_str = if let Some(start) = rest.find('(') {
            let end = rest.rfind(')').unwrap_or(rest.len());
            rest[start + 1..end].trim()
        } else {
            ""
        };

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let (txn, sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let proc_obj = object_name_from_token(&proc_name)?;
            let resolved = names::resolve_existing_procedure_name(
                self.store().as_ref(),
                txn,
                &proc_obj,
                search_path,
            )
            .await?
            .ok_or_else(|| anyhow!("Procedure '{}' does not exist", proc_name))?;
            let proc_full_name = resolved.full;

            let definition: String = self
                .store()
                .get_procedure(txn, &proc_full_name)
                .await?
                .ok_or_else(|| anyhow!("Procedure '{}' does not exist", proc_full_name))?;

            let parts: Vec<&str> = definition.splitn(2, "\nBODY:").collect();
            if parts.len() != 2 {
                return Err(anyhow!("Invalid procedure definition"));
            }

            let param_str = parts[0].strip_prefix("PARAMS:").unwrap_or("");
            let body_str = parts[1];

            let param_defs: Vec<(&str, &str)> = if param_str.is_empty() {
                vec![]
            } else {
                param_str
                    .split(',')
                    .filter_map(|p| {
                        let parts: Vec<&str> = p.trim().splitn(2, ' ').collect();
                        if parts.len() == 2 {
                            Some((parts[0], parts[1]))
                        } else {
                            None
                        }
                    })
                    .collect()
            };

            let call_args: Vec<String> = if args_str.is_empty() {
                vec![]
            } else {
                args_str.split(',').map(|s| s.trim().to_string()).collect()
            };

            if call_args.len() != param_defs.len() {
                return Err(anyhow!(
                    "Procedure '{}' expects {} arguments, got {}",
                    proc_full_name,
                    param_defs.len(),
                    call_args.len()
                ));
            }

            let mut param_map: HashMap<String, (String, String)> = HashMap::new();
            for (i, (name, data_type)) in param_defs.iter().enumerate() {
                param_map.insert(
                    name.to_string(),
                    (call_args[i].clone(), data_type.to_string()),
                );
            }

            let body_statements: Vec<&str> = body_str
                .split(';')
                .filter(|s| !s.trim().is_empty())
                .collect();

            for stmt_str in body_statements {
                let mut expanded_stmt = stmt_str.to_string();
                for (name, (value, data_type)) in &param_map {
                    let dt_lower = data_type.to_lowercase();
                    let formatted_value = if dt_lower.contains("int")
                        || dt_lower.contains("float")
                        || dt_lower.contains("real")
                        || dt_lower.contains("numeric")
                        || dt_lower.contains("decimal")
                        || dt_lower.contains("double")
                    {
                        value.clone()
                    } else if value.starts_with('\'') && value.ends_with('\'') {
                        value.clone()
                    } else {
                        format!("'{}'", value)
                    };
                    expanded_stmt = expanded_stmt.replace(name, &formatted_value);
                }

                let stmts = parse_sql(&expanded_stmt)?;
                for stmt in stmts {
                    self.execute_statement_on_txn(txn, sequence_values, search_path, &stmt)
                        .await?;
                }
            }

            Ok(ExecuteResult::Call)
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

    pub(crate) async fn execute_create_procedure_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let sql_trimmed = sql.trim();
        let sql_upper = sql_trimmed.to_uppercase();
        let is_or_replace = sql_upper.starts_with("CREATE OR REPLACE PROCEDURE");
        let rest = sql_trimmed
            .strip_prefix("CREATE PROCEDURE")
            .or_else(|| sql_trimmed.strip_prefix("CREATE OR REPLACE PROCEDURE"))
            .or_else(|| sql_trimmed.strip_prefix("create procedure"))
            .or_else(|| sql_trimmed.strip_prefix("create or replace procedure"))
            .ok_or_else(|| anyhow!("Invalid CREATE PROCEDURE syntax"))?
            .trim();

        let (name_and_params, body) = rest
            .split_once("AS BEGIN")
            .or_else(|| rest.split_once("as begin"))
            .or_else(|| rest.split_once("AS\nBEGIN"))
            .or_else(|| rest.split_once("as\nbegin"))
            .ok_or_else(|| anyhow!("CREATE PROCEDURE requires AS BEGIN ... END syntax"))?;

        let body = body
            .strip_suffix("END;")
            .or_else(|| body.strip_suffix("END"))
            .or_else(|| body.strip_suffix("end;"))
            .or_else(|| body.strip_suffix("end"))
            .unwrap_or(body)
            .trim();

        let name_and_params = name_and_params.trim();
        let (proc_name, params_str) = if let Some(paren_pos) = name_and_params.find('(') {
            let name = name_and_params[..paren_pos].trim().to_lowercase();
            let params = name_and_params[paren_pos..]
                .trim_start_matches('(')
                .trim_end_matches(')')
                .trim();
            (name, params)
        } else {
            (name_and_params.to_lowercase(), "")
        };

        let definition = format!("PARAMS:{}\nBODY:{}", params_str, body);

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");
            let proc_obj = object_name_from_token(&proc_name)?;
            let resolved = names::resolve_ddl_object_name(&proc_obj, search_path)?;
            if !self.store().schema_exists(txn, &resolved.schema).await? {
                return Err(anyhow!("schema '{}' does not exist", resolved.schema));
            }
            let proc_full_name = resolved.full;
            if is_or_replace {
                self.store()
                    .replace_procedure(txn, &proc_full_name, &definition)
                    .await?;
            } else {
                self.store()
                    .create_procedure(txn, &proc_full_name, &definition)
                    .await?;
            }
            Ok(ExecuteResult::CreateProcedure {
                proc_name: proc_full_name,
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

    pub(crate) async fn execute_drop_procedure_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let sql_upper = sql.trim().to_uppercase();
        let rest = sql_upper
            .strip_prefix("DROP PROCEDURE")
            .ok_or_else(|| anyhow!("Invalid DROP PROCEDURE syntax"))?
            .trim();

        let if_exists = rest.starts_with("IF EXISTS");
        let name_part = if if_exists {
            rest.strip_prefix("IF EXISTS").unwrap().trim()
        } else {
            rest
        };

        let proc_name = name_part
            .split(|c: char| c.is_whitespace() || c == '(' || c == ';')
            .next()
            .ok_or_else(|| anyhow!("Missing procedure name"))?
            .to_lowercase();

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");
            let proc_obj = object_name_from_token(&proc_name)?;
            let resolved = names::resolve_existing_procedure_name(
                self.store().as_ref(),
                txn,
                &proc_obj,
                search_path,
            )
            .await?;

            let proc_full_name = match resolved {
                Some(resolved) => resolved.full,
                None => names::resolve_ddl_object_name(&proc_obj, search_path)?.full,
            };

            let dropped = self.store().drop_procedure(txn, &proc_full_name).await?;
            if !dropped && !if_exists {
                return Err(anyhow!("Procedure '{}' does not exist", proc_full_name));
            }
            Ok(ExecuteResult::DropProcedure {
                proc_name: proc_full_name,
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
}
