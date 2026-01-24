use super::core::Executor;
use super::triggers::strip_leading_sql_comments;
use super::super::helpers::normalize_ident;
use super::super::names;
use super::super::expr::eval_expr;
use super::super::Session;
use crate::extensions::{descriptor, InstalledExtension};
use anyhow::{anyhow, Result};
use sqlparser::ast::{Expr, FunctionArg, FunctionArgExpr, ObjectName, TableAlias};
use tikv_client::Transaction;

use crate::extensions::EXTENSIONS_SCHEMA;
use crate::types::{Row, TableSchema, Value};

use crate::extensions::http::{self, HttpTableFunctionCall};

fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn parse_extension_name_token(token: &str) -> Result<String> {
    let token = token.trim().trim_end_matches(';');
    if token.is_empty() {
        return Err(anyhow!("Missing extension name"));
    }
    let name = token
        .split('.')
        .last()
        .ok_or_else(|| anyhow!("Missing extension name"))?
        .trim_matches('"')
        .to_lowercase();
    if name.is_empty() {
        return Err(anyhow!("Missing extension name"));
    }
    Ok(name)
}

fn parse_create_extension_sql(sql: &str) -> Result<(bool, String)> {
    let sql = strip_leading_sql_comments(sql).trim();
    let sql = sql.trim_end_matches(';').trim_end();

    let rest = if starts_with_ignore_ascii_case(sql, "create extension") {
        sql["create extension".len()..].trim_start()
    } else {
        return Err(anyhow!("Invalid CREATE EXTENSION syntax"));
    };

    let if_not_exists = starts_with_ignore_ascii_case(rest, "if not exists");
    let rest = if if_not_exists {
        rest["if not exists".len()..].trim_start()
    } else {
        rest
    };

    let mut iter = rest.split_whitespace();
    let ext_tok = iter
        .next()
        .ok_or_else(|| anyhow!("Missing extension name"))?;
    let ext_name = parse_extension_name_token(ext_tok)?;
    Ok((if_not_exists, ext_name))
}

fn parse_drop_extension_sql(sql: &str) -> Result<(bool, String)> {
    let sql = strip_leading_sql_comments(sql).trim();
    let sql = sql.trim_end_matches(';').trim_end();

    let rest = if starts_with_ignore_ascii_case(sql, "drop extension") {
        sql["drop extension".len()..].trim_start()
    } else {
        return Err(anyhow!("Invalid DROP EXTENSION syntax"));
    };

    let if_exists = starts_with_ignore_ascii_case(rest, "if exists");
    let rest = if if_exists {
        rest["if exists".len()..].trim_start()
    } else {
        rest
    };

    let mut iter = rest.split_whitespace();
    let ext_tok = iter
        .next()
        .ok_or_else(|| anyhow!("Missing extension name"))?;
    let ext_name = parse_extension_name_token(ext_tok)?;
    Ok((if_exists, ext_name))
}

impl Executor {
    pub(crate) async fn try_execute_extension_table_function(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        name: &ObjectName,
        args: &[FunctionArg],
        alias: Option<&TableAlias>,
    ) -> Result<Option<(TableSchema, Vec<Row>)>> {
        let (schema_opt, func_name) = names::split_object_name(name)?;

        let schema = match schema_opt {
            Some(schema) => schema,
            None => {
                if search_path
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(EXTENSIONS_SCHEMA))
                {
                    EXTENSIONS_SCHEMA.to_string()
                } else {
                    return Ok(None);
                }
            }
        };

        if !schema.eq_ignore_ascii_case(EXTENSIONS_SCHEMA) {
            return Ok(None);
        }

        fn extract_expr_arg(arg: &FunctionArg) -> Result<&Expr> {
            match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Ok(e),
                _ => Err(anyhow!("http: requires expression arguments")),
            }
        }

        fn expect_text(v: Value, what: &str) -> Result<String> {
            match v {
                Value::Text(s) => Ok(s),
                Value::Null => Err(anyhow!("http: {} must not be NULL", what)),
                other => Err(anyhow!("http: {} must be TEXT, got {}", what, other.data_type().unwrap_or(crate::types::DataType::Text))),
            }
        }

        let func_upper = func_name.to_ascii_uppercase();
        let call = match func_upper.as_str() {
            "HTTP_GET" => {
                if args.len() != 1 {
                    return Err(anyhow!("http_get(url text) requires 1 argument"));
                }
                let url = expect_text(eval_expr(extract_expr_arg(&args[0])?, None, None)?, "url")?;
                HttpTableFunctionCall::Get { url }
            }
            "HTTP_HEAD" => {
                if args.len() != 1 {
                    return Err(anyhow!("http_head(url text) requires 1 argument"));
                }
                let url = expect_text(eval_expr(extract_expr_arg(&args[0])?, None, None)?, "url")?;
                HttpTableFunctionCall::Head { url }
            }
            "HTTP_DELETE" => {
                if args.len() != 1 {
                    return Err(anyhow!("http_delete(url text) requires 1 argument"));
                }
                let url = expect_text(eval_expr(extract_expr_arg(&args[0])?, None, None)?, "url")?;
                HttpTableFunctionCall::Delete { url }
            }
            "HTTP_POST" => {
                if args.len() != 3 {
                    return Err(anyhow!(
                        "http_post(url text, body text, content_type text) requires 3 arguments"
                    ));
                }
                let url = expect_text(eval_expr(extract_expr_arg(&args[0])?, None, None)?, "url")?;
                let body =
                    expect_text(eval_expr(extract_expr_arg(&args[1])?, None, None)?, "body")?;
                let content_type = expect_text(
                    eval_expr(extract_expr_arg(&args[2])?, None, None)?,
                    "content_type",
                )?;
                HttpTableFunctionCall::Post {
                    url,
                    body,
                    content_type,
                }
            }
            "HTTP_PUT" => {
                if args.len() != 3 {
                    return Err(anyhow!(
                        "http_put(url text, body text, content_type text) requires 3 arguments"
                    ));
                }
                let url = expect_text(eval_expr(extract_expr_arg(&args[0])?, None, None)?, "url")?;
                let body =
                    expect_text(eval_expr(extract_expr_arg(&args[1])?, None, None)?, "body")?;
                let content_type = expect_text(
                    eval_expr(extract_expr_arg(&args[2])?, None, None)?,
                    "content_type",
                )?;
                HttpTableFunctionCall::Put {
                    url,
                    body,
                    content_type,
                }
            }
            _ => return Ok(None),
        };

        let installed = self.store().get_extension(txn, db_id, "http").await?;
        let Some(installed) = installed else {
            return Err(anyhow!("extension \"http\" is not installed"));
        };
        if !installed.enabled {
            return Err(anyhow!("extension \"http\" is disabled"));
        }

        let (mut schema, rows) = http::execute_table_function(self.tenant_keyspace(), call).await?;

        if let Some(alias) = alias {
            if !alias.columns.is_empty() {
                if alias.columns.len() != schema.columns.len() {
                    return Err(anyhow!(
                        "Table function alias column count mismatch: expected {}, got {}",
                        schema.columns.len(),
                        alias.columns.len()
                    ));
                }
                for (i, ident) in alias.columns.iter().enumerate() {
                    schema.columns[i].name = normalize_ident(ident);
                }
            }
        }

        Ok(Some((schema, rows)))
    }

    pub(crate) async fn execute_create_extension_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<super::super::ExecuteResult> {
        let (if_not_exists, ext_name) = parse_create_extension_sql(sql)?;

        if !session.is_superuser() {
            return Err(anyhow!("permission denied to create extension"));
        }

        let desc = descriptor(&ext_name)
            .ok_or_else(|| anyhow!("Extension '{}' is not available", ext_name))?;

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let db_id = session.current_database_id();
            let (txn, _sequence_values, _search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            if !self
                .store()
                .schema_exists(txn, db_id, desc.default_schema)
                .await?
            {
                return Err(anyhow!("schema '{}' does not exist", desc.default_schema));
            }

            if self
                .store()
                .get_extension(txn, db_id, &ext_name)
                .await?
                .is_some()
            {
                if if_not_exists {
                    return Ok(super::super::ExecuteResult::CreateExtension {
                        ext_name: ext_name.clone(),
                    });
                }
                return Err(anyhow!("extension \"{}\" already exists", ext_name));
            }

            let ext = InstalledExtension::new(desc);
            self.store().put_extension(txn, db_id, &ext).await?;

            Ok(super::super::ExecuteResult::CreateExtension {
                ext_name: ext_name.clone(),
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

    pub(crate) async fn execute_drop_extension_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<super::super::ExecuteResult> {
        let (if_exists, ext_name) = parse_drop_extension_sql(sql)?;

        if !session.is_superuser() {
            return Err(anyhow!("permission denied to drop extension"));
        }

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let db_id = session.current_database_id();
            let (txn, _sequence_values, _search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let dropped = self.store().drop_extension(txn, db_id, &ext_name).await?;
            if !dropped && !if_exists {
                return Err(anyhow!("extension \"{}\" does not exist", ext_name));
            }

            Ok(super::super::ExecuteResult::DropExtension {
                ext_name: ext_name.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_create_extension_basic() {
        let (if_not_exists, name) = parse_create_extension_sql("CREATE EXTENSION http;").unwrap();
        assert!(!if_not_exists);
        assert_eq!(name, "http");
    }

    #[test]
    fn test_parse_create_extension_if_not_exists_case_insensitive() {
        let (if_not_exists, name) =
            parse_create_extension_sql("create extension IF NOT EXISTS \"http\"").unwrap();
        assert!(if_not_exists);
        assert_eq!(name, "http");
    }

    #[test]
    fn test_parse_drop_extension_if_exists_and_schema_prefix() {
        let (if_exists, name) =
            parse_drop_extension_sql("DROP EXTENSION IF EXISTS extensions.http").unwrap();
        assert!(if_exists);
        assert_eq!(name, "http");
    }
}
