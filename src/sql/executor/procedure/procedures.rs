//! CREATE, CALL, and DROP PROCEDURE execution.
//!
//! Implements `execute_create_procedure`, `execute_call_cmd`,
//! `execute_create_procedure_cmd`, and `execute_drop_procedure_cmd`
//! on `Executor`.

use super::super::super::names;
use super::super::super::{extract_create_index_with_params, parse_sql, ExecuteResult, Session};
use super::super::core::Executor;
use super::{object_name_from_token, parse_call_arguments, substitute_parameters_in_statement};
use anyhow::{anyhow, Result};
use sqlparser::ast::{ObjectName, Statement};
use std::collections::HashMap;
use tikv_client::Transaction;

impl Executor {
    pub(crate) async fn execute_create_procedure(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        name: &ObjectName,
        params: Option<&[sqlparser::ast::ProcedureParam]>,
        body: &[Statement],
    ) -> Result<ExecuteResult> {
        let resolved = names::resolve_ddl_object_name(name, search_path)?;
        if !self
            .store()
            .schema_exists(txn, db_id, &resolved.schema)
            .await?
        {
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
            .create_procedure(txn, db_id, &proc_name, &definition)
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

        let current_role = session.current_user().map(|u| u.to_string());
        let session_user = session.session_user().map(|u| u.to_string());
        let result = async {
            let db_id = session.current_database_id();
            let (txn, sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let proc_obj = object_name_from_token(&proc_name)?;
            let resolved = names::resolve_existing_procedure_name(
                self.store().as_ref(),
                txn,
                db_id,
                &proc_obj,
                search_path,
            )
            .await?
            .ok_or_else(|| anyhow!("Procedure '{}' does not exist", proc_name))?;
            let proc_full_name = resolved.full;

            let definition: String = self
                .store()
                .get_procedure(txn, db_id, &proc_full_name)
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

            let call_args = parse_call_arguments(args_str)?;

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
                let expanded_stmt = substitute_parameters_in_statement(stmt_str, &param_map)?;

                let stmts = parse_sql(&expanded_stmt)?;
                let mut create_index_with_params =
                    extract_create_index_with_params(&expanded_stmt).into_iter();
                for stmt in stmts {
                    let with_params = if matches!(&stmt, Statement::CreateIndex { .. }) {
                        create_index_with_params.next().flatten()
                    } else {
                        None
                    };
                    self.execute_statement_on_txn_with_create_index_with_params(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &stmt,
                        with_params.as_deref(),
                        current_role.as_deref(),
                        session_user.as_deref(),
                    )
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

        let body_trimmed = body.trim_end();
        let body_trimmed = body_trimmed
            .strip_suffix(';')
            .unwrap_or(body_trimmed)
            .trim_end();
        if body_trimmed.len() < 3
            || !body_trimmed.as_bytes()[body_trimmed.len() - 3..].eq_ignore_ascii_case(b"END")
        {
            return Err(anyhow!("CREATE PROCEDURE requires AS BEGIN ... END syntax"));
        }
        let body = body_trimmed[..body_trimmed.len() - 3].trim();

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
            let db_id = session.current_database_id();
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");
            let proc_obj = object_name_from_token(&proc_name)?;
            let resolved = names::resolve_ddl_object_name(&proc_obj, search_path)?;
            if !self
                .store()
                .schema_exists(txn, db_id, &resolved.schema)
                .await?
            {
                return Err(anyhow!("schema '{}' does not exist", resolved.schema));
            }
            let proc_full_name = resolved.full;
            if is_or_replace {
                self.store()
                    .replace_procedure(txn, db_id, &proc_full_name, &definition)
                    .await?;
            } else {
                self.store()
                    .create_procedure(txn, db_id, &proc_full_name, &definition)
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
            let db_id = session.current_database_id();
            let (txn, _sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");
            let proc_obj = object_name_from_token(&proc_name)?;
            let resolved = names::resolve_existing_procedure_name(
                self.store().as_ref(),
                txn,
                db_id,
                &proc_obj,
                search_path,
            )
            .await?;

            let proc_full_name = match resolved {
                Some(resolved) => resolved.full,
                None => names::resolve_ddl_object_name(&proc_obj, search_path)?.full,
            };

            let dropped = self
                .store()
                .drop_procedure(txn, db_id, &proc_full_name)
                .await?;
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
