//! BEFORE trigger execution for INSERT/UPDATE DML operations.

use crate::storage::TikvStore;
use crate::types::{FunctionDef, Row, TableSchema, TriggerDef, Value};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use tikv_client::Transaction;

use super::trigger_rewrite::substitute_row_references;

#[derive(Clone, Debug)]
enum TriggerStatement {
    ReturnNew,
    ReturnNull,
    ReturnOld,
    Assignment { column: String, expr_str: String },
    Skip,
}

#[derive(Clone, Debug)]
struct CompiledTriggerBody {
    statements: Vec<TriggerStatement>,
}

static COMPILED_BODY_CACHE: OnceLock<RwLock<HashMap<u32, CompiledTriggerBody>>> = OnceLock::new();

fn get_body_cache() -> &'static RwLock<HashMap<u32, CompiledTriggerBody>> {
    COMPILED_BODY_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

impl CompiledTriggerBody {
    fn compile(body: &str) -> Result<Self> {
        validate_trigger_body(body)?;

        let body_upper = body.to_ascii_uppercase();
        let begin_pos = match body_upper.find("BEGIN") {
            Some(p) => p + 5,
            None => return Ok(Self { statements: vec![] }),
        };
        let end_pos = body_upper.rfind("END").unwrap_or(body.len());
        let block_content = &body[begin_pos..end_pos];

        let normalized = block_content
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with("--"))
            .collect::<Vec<_>>()
            .join(" ");

        let mut statements = Vec::new();
        for stmt in normalized.split(';') {
            let line = stmt.trim();
            if line.is_empty() {
                continue;
            }

            let line_upper = line.to_uppercase();

            let parsed = if line_upper.starts_with("RETURN NEW") {
                TriggerStatement::ReturnNew
            } else if line_upper.starts_with("RETURN NULL") {
                TriggerStatement::ReturnNull
            } else if line_upper.starts_with("RETURN OLD") {
                TriggerStatement::ReturnOld
            } else if line_upper.starts_with("NEW.") {
                Self::parse_assignment(line)?
            } else {
                TriggerStatement::Skip
            };
            statements.push(parsed);
        }

        Ok(Self { statements })
    }

    fn parse_assignment(line: &str) -> Result<TriggerStatement> {
        let assignment = &line[4..];

        let (col_name, expr_str) = if let Some(pos) = assignment.find(":=") {
            (
                assignment[..pos].trim(),
                assignment[pos + 2..].trim().trim_end_matches(';'),
            )
        } else if let Some(pos) = assignment.find('=') {
            let before = assignment.chars().nth(pos.saturating_sub(1));
            let after = assignment.chars().nth(pos + 1);
            if before == Some(':')
                || before == Some('<')
                || before == Some('>')
                || before == Some('!')
                || after == Some('=')
            {
                return Ok(TriggerStatement::Skip);
            }
            (
                assignment[..pos].trim(),
                assignment[pos + 1..].trim().trim_end_matches(';'),
            )
        } else {
            return Ok(TriggerStatement::Skip);
        };

        Ok(TriggerStatement::Assignment {
            column: col_name.to_string(),
            expr_str: expr_str.to_string(),
        })
    }
}

fn get_or_compile_body(func_oid: u32, body: &str) -> Result<CompiledTriggerBody> {
    let cache = get_body_cache();

    {
        let guard = cache.read().expect("compiled body cache read lock");
        if let Some(compiled) = guard.get(&func_oid) {
            return Ok(compiled.clone());
        }
    }

    let compiled = CompiledTriggerBody::compile(body)?;
    {
        let mut guard = cache.write().expect("compiled body cache write lock");
        guard.insert(func_oid, compiled.clone());
    }
    Ok(compiled)
}

pub async fn prefetch_trigger_functions(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    triggers: &[TriggerDef],
    event: &str,
) -> Result<HashMap<String, FunctionDef>> {
    let mut func_cache = HashMap::new();
    let before_triggers: Vec<&TriggerDef> = triggers
        .iter()
        .filter(|t| {
            t.timing.eq_ignore_ascii_case("BEFORE")
                && t.events.iter().any(|e| e.eq_ignore_ascii_case(event))
        })
        .collect();

    for trigger in before_triggers {
        if func_cache.contains_key(&trigger.function) {
            continue;
        }
        if let Some(func_def) = store.get_function(txn, db_id, &trigger.function).await? {
            if let Ok(compiled) = get_or_compile_body(func_def.oid, &func_def.body) {
                let _ = compiled;
            }
            func_cache.insert(trigger.function.clone(), func_def);
        }
    }

    Ok(func_cache)
}

pub async fn apply_before_triggers_with_cache(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    triggers: &[TriggerDef],
    func_cache: &HashMap<String, FunctionDef>,
    schema: &TableSchema,
    event: &str,
    new_row: Row,
    old_row: Option<&Row>,
) -> Result<Option<Row>> {
    let before_triggers: Vec<&TriggerDef> = triggers
        .iter()
        .filter(|t| {
            t.timing.eq_ignore_ascii_case("BEFORE")
                && t.events.iter().any(|e| e.eq_ignore_ascii_case(event))
        })
        .collect();

    if before_triggers.is_empty() {
        return Ok(Some(new_row));
    }

    let mut current_row = new_row;

    for trigger in before_triggers {
        let func_def = match func_cache.get(&trigger.function) {
            Some(f) => f,
            None => continue,
        };

        let result = execute_trigger_function(
            store,
            txn,
            db_id,
            sequence_values,
            search_path,
            func_def,
            schema,
            &current_row,
            old_row,
        )
        .await?;

        match result {
            TriggerResult::Row(modified_row) => {
                current_row = modified_row;
            }
            TriggerResult::Null => {
                return Ok(None);
            }
            TriggerResult::Unchanged => {}
        }
    }

    Ok(Some(current_row))
}

#[allow(dead_code)]
pub async fn apply_before_triggers(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    triggers: &[TriggerDef],
    schema: &TableSchema,
    event: &str,
    new_row: Row,
    old_row: Option<&Row>,
) -> Result<Option<Row>> {
    let before_triggers: Vec<&TriggerDef> = triggers
        .iter()
        .filter(|t| {
            t.timing.eq_ignore_ascii_case("BEFORE")
                && t.events.iter().any(|e| e.eq_ignore_ascii_case(event))
        })
        .collect();

    if before_triggers.is_empty() {
        return Ok(Some(new_row));
    }

    let mut current_row = new_row;

    for trigger in before_triggers {
        let func_def = match store.get_function(txn, db_id, &trigger.function).await? {
            Some(f) => f,
            None => continue,
        };

        let result = execute_trigger_function(
            store,
            txn,
            db_id,
            sequence_values,
            search_path,
            &func_def,
            schema,
            &current_row,
            old_row,
        )
        .await?;

        match result {
            TriggerResult::Row(modified_row) => {
                current_row = modified_row;
            }
            TriggerResult::Null => {
                return Ok(None);
            }
            TriggerResult::Unchanged => {}
        }
    }

    Ok(Some(current_row))
}

enum TriggerResult {
    Row(Row),
    Null,
    Unchanged,
}

const UNSUPPORTED_FTS_FUNCTIONS: &[&str] = &[
    "tsvector_update_trigger",
    "websearch_to_tsquery",
    "phraseto_tsquery",
];

fn validate_trigger_body(body: &str) -> Result<()> {
    let body_lower = body.to_lowercase();
    for func in UNSUPPORTED_FTS_FUNCTIONS {
        if let Some(pos) = body_lower.find(func) {
            let after_pos = pos + func.len();
            if after_pos < body_lower.len() {
                let next_char = body_lower.as_bytes()[after_pos] as char;
                if next_char == '(' || next_char.is_whitespace() {
                    return Err(anyhow::anyhow!(
                        "Trigger uses unsupported function '{}'. \
                         Use to_tsvector/plainto_tsquery/ts_rank instead.",
                        func
                    ));
                }
            }
        }
    }
    Ok(())
}

async fn execute_trigger_function(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    func_def: &crate::types::FunctionDef,
    schema: &TableSchema,
    new_row: &Row,
    old_row: Option<&Row>,
) -> Result<TriggerResult> {
    let lang = func_def.language.to_lowercase();
    if lang != "plpgsql" && lang != "sql" {
        return Ok(TriggerResult::Unchanged);
    }

    execute_trigger_body_cached(
        store,
        txn,
        db_id,
        sequence_values,
        search_path,
        func_def.oid,
        &func_def.body,
        schema,
        new_row,
        old_row,
    )
    .await
}

async fn execute_trigger_body_cached(
    store: &Arc<TikvStore>,
    txn: &mut Transaction,
    db_id: u64,
    sequence_values: &mut HashMap<String, i64>,
    search_path: &[String],
    func_oid: u32,
    body: &str,
    schema: &TableSchema,
    new_row: &Row,
    old_row: Option<&Row>,
) -> Result<TriggerResult> {
    use super::expr::eval_expr;
    use super::sequences;

    let compiled = get_or_compile_body(func_oid, body)?;

    if compiled.statements.is_empty() {
        return Ok(TriggerResult::Unchanged);
    }

    let mut modified_values = new_row.values.clone();
    let mut was_modified = false;

    for stmt in &compiled.statements {
        match stmt {
            TriggerStatement::ReturnNew => {
                return if was_modified {
                    Ok(TriggerResult::Row(Row::new(modified_values)))
                } else {
                    Ok(TriggerResult::Row(new_row.clone()))
                };
            }
            TriggerStatement::ReturnNull => {
                return Ok(TriggerResult::Null);
            }
            TriggerStatement::ReturnOld => {
                return match old_row {
                    Some(old) => Ok(TriggerResult::Row(old.clone())),
                    None => Ok(TriggerResult::Null),
                };
            }
            TriggerStatement::Assignment { column, expr_str } => {
                let col_idx = schema
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(column));

                if let Some(idx) = col_idx {
                    let resolved_expr =
                        substitute_row_references(expr_str, schema, &modified_values, old_row);

                    let sql = format!("SELECT {}", resolved_expr);
                    if let Ok(stmts) = super::parse_sql(&sql) {
                        if let Some(sqlparser::ast::Statement::Query(query)) =
                            stmts.into_iter().next()
                        {
                            if let sqlparser::ast::SetExpr::Select(select) = *query.body {
                                if let Some(sqlparser::ast::SelectItem::UnnamedExpr(expr)) =
                                    select.projection.into_iter().next()
                                {
                                    let value = if sequences::expr_needs_async_eval(&expr) {
                                        sequences::eval_expr_with_sequences(
                                            store,
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            &expr,
                                            None,
                                            None,
                                        )
                                        .await?
                                    } else {
                                        eval_expr(&expr, None, None)?
                                    };

                                    let coerced = super::value_coercion::coerce_value_for_column(
                                        value,
                                        &schema.columns[idx],
                                    )?;

                                    modified_values[idx] = coerced;
                                    was_modified = true;
                                }
                            }
                        }
                    }
                }
            }
            TriggerStatement::Skip => {}
        }
    }

    if was_modified {
        Ok(TriggerResult::Row(Row::new(modified_values)))
    } else {
        Ok(TriggerResult::Unchanged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::trigger_rewrite::value_to_sql_literal;

    #[test]
    fn test_substitute_row_references() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![
                crate::types::ColumnDef {
                    name: "id".to_string(),
                    data_type: crate::types::DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                crate::types::ColumnDef {
                    name: "updated_at".to_string(),
                    data_type: crate::types::DataType::Timestamp,
                    nullable: true,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: Some("test_pkey".to_string()),
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let new_values = vec![Value::Int32(1), Value::Null];

        let result = substitute_row_references("NEW.id + 1", &schema, &new_values, None);
        assert_eq!(result, "1 + 1");

        let result = substitute_row_references("NOW()", &schema, &new_values, None);
        assert_eq!(result, "NOW()");
    }

    #[test]
    fn test_substitute_row_references_does_not_prefix_match() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![
                crate::types::ColumnDef {
                    name: "id".to_string(),
                    data_type: crate::types::DataType::Int32,
                    nullable: false,
                    primary_key: true,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                crate::types::ColumnDef {
                    name: "id2".to_string(),
                    data_type: crate::types::DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: Some("test_pkey".to_string()),
            pk_indices: vec![0],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let new_values = vec![Value::Int32(7), Value::Int32(3)];
        let old_row = Row::new(vec![Value::Int32(9), Value::Int32(11)]);

        let result = substitute_row_references("NEW.id2 + 1", &schema, &new_values, None);
        assert_eq!(result, "3 + 1");

        let result = substitute_row_references("NEW.id + NEW.id2", &schema, &new_values, None);
        assert_eq!(result, "7 + 3");

        let result =
            substitute_row_references("OLD.id2 + OLD.id", &schema, &new_values, Some(&old_row));
        assert_eq!(result, "11 + 9");
    }

    #[test]
    fn test_substitute_row_references_does_not_collide_a_aa() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![
                crate::types::ColumnDef {
                    name: "a".to_string(),
                    data_type: crate::types::DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                crate::types::ColumnDef {
                    name: "aa".to_string(),
                    data_type: crate::types::DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let new_values = vec![Value::Int32(1), Value::Int32(9)];
        let result = substitute_row_references("NEW.aa + 1", &schema, &new_values, None);
        assert_eq!(result, "9 + 1");
    }

    #[test]
    fn test_substitute_row_references_ignores_strings_and_comments() {
        let schema = TableSchema {
            name: "test".to_string(),
            table_id: 1,
            columns: vec![
                crate::types::ColumnDef {
                    name: "a".to_string(),
                    data_type: crate::types::DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                crate::types::ColumnDef {
                    name: "aa".to_string(),
                    data_type: crate::types::DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
            from_alias: None,
        };

        let new_values = vec![Value::Int32(1), Value::Int32(9)];
        let expr = "'NEW.aa' || NEW.aa::TEXT /* NEW.aa */ -- NEW.aa";
        let result = substitute_row_references(expr, &schema, &new_values, None);
        assert_eq!(result, "'NEW.aa' || 9::TEXT /* NEW.aa */ -- NEW.aa");
    }

    #[test]
    fn test_value_to_sql_literal() {
        assert_eq!(value_to_sql_literal(&Value::Null), "NULL");
        assert_eq!(value_to_sql_literal(&Value::Boolean(true)), "TRUE");
        assert_eq!(value_to_sql_literal(&Value::Int32(42)), "42");
        assert_eq!(
            value_to_sql_literal(&Value::Text("hello".to_string())),
            "'hello'"
        );
        assert_eq!(
            value_to_sql_literal(&Value::Text("it's".to_string())),
            "'it''s'"
        );
    }

    #[test]
    fn test_validate_trigger_body_allows_normal_triggers() {
        let body = r#"
            BEGIN
                NEW.updated_at := NOW();
                RETURN NEW;
            END;
        "#;
        assert!(validate_trigger_body(body).is_ok());
    }

    #[test]
    fn test_validate_trigger_body_allows_to_tsvector() {
        let body = r#"
            BEGIN
                NEW.search_vector := to_tsvector('english', NEW.title);
                RETURN NEW;
            END;
        "#;
        let result = validate_trigger_body(body);
        assert!(result.is_ok(), "to_tsvector should be supported now");
    }

    #[test]
    fn test_validate_trigger_body_allows_setweight() {
        let body = r#"
            BEGIN
                NEW.search_vector := setweight(to_tsvector('english', NEW.title), 'A');
                RETURN NEW;
            END;
        "#;
        let result = validate_trigger_body(body);
        assert!(result.is_ok(), "setweight should be supported now");
    }

    #[test]
    fn test_validate_trigger_body_detects_unsupported_fts() {
        let body = r#"
            BEGIN
                NEW.search_vector := tsvector_update_trigger();
                RETURN NEW;
            END;
        "#;
        let result = validate_trigger_body(body);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("tsvector_update_trigger"));
    }

    #[test]
    fn test_validate_trigger_body_ignores_similar_names() {
        let body = r#"
            BEGIN
                NEW.to_tsvector_count := 1;
                RETURN NEW;
            END;
        "#;
        assert!(validate_trigger_body(body).is_ok());
    }
}
