use std::collections::HashMap;

use anyhow::{anyhow, Result};
use sqlparser::ast::{FunctionArg, FunctionArgExpr};
use tikv_client::Transaction;

use super::core::Executor;
use crate::model::{DataType, Row, TableSchema, Value};
use crate::sql::analyzer::types::{
    FunctionKind, ResolvedFunction, TypedExpr, TypedExprKind, TypedFunctionArg,
};
use crate::sql::error::SqlError;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::names;
use crate::sql::operators::execute_operator_tree_with_ctes;
use crate::sql::query_context::QueryContext;
use crate::sql::sequences::SequenceSession;
use crate::sql::table_functions::is_virtual_table_backed_system_function;

#[derive(Debug, Clone)]
pub(crate) struct EvaluatedTableFunctionArg {
    pub(crate) name: Option<String>,
    pub(crate) value: Value,
}

fn bridge_function_args(args: &[EvaluatedTableFunctionArg]) -> Vec<FunctionArg> {
    args.iter()
        .map(|arg| {
            let sql_expr = crate::sql::value_coercion::value_to_sql_expr(&arg.value);
            match &arg.name {
                None => FunctionArg::Unnamed(FunctionArgExpr::Expr(sql_expr)),
                Some(name) => FunctionArg::Named {
                    name: sqlparser::ast::Ident::new(name),
                    arg: FunctionArgExpr::Expr(sql_expr),
                },
            }
        })
        .collect()
}

pub(crate) fn chunk_text_rows(args: &[EvaluatedTableFunctionArg]) -> Result<Vec<Row>> {
    use crate::sql::chunker::{
        chunk_document, format_for_embedding, ChunkOptions, DEFAULT_MAX_CHARS,
        DEFAULT_OVERLAP_CHARS,
    };

    if args.is_empty() {
        return Err(anyhow!(
            "chunk_text requires at least 1 argument (content TEXT)"
        ));
    }

    let content = match &args[0].value {
        Value::Text(s) => s.as_str(),
        Value::Null => return Ok(Vec::new()),
        other => {
            return Err(anyhow!(
                "chunk_text: first argument must be TEXT, got {}",
                other
            ))
        }
    };

    // Parse optional max_chars (positional arg 2 or named "max_chars")
    let max_chars = args
        .get(1)
        .and_then(|a| match (&a.name, &a.value) {
            (None, Value::Int32(n)) if *n > 0 => Some(*n as usize),
            (None, Value::Int64(n)) if *n > 0 => Some(*n as usize),
            _ => None,
        })
        .or_else(|| {
            args.iter().find_map(|a| match (&a.name, &a.value) {
                (Some(n), Value::Int32(v)) if n == "max_chars" && *v > 0 => Some(*v as usize),
                (Some(n), Value::Int64(v)) if n == "max_chars" && *v > 0 => Some(*v as usize),
                _ => None,
            })
        });

    // Parse optional overlap_chars (positional arg 3 or named "overlap_chars")
    let overlap_chars = args
        .get(2)
        .and_then(|a| match (&a.name, &a.value) {
            (None, Value::Int32(n)) if *n >= 0 => Some(*n as usize),
            (None, Value::Int64(n)) if *n >= 0 => Some(*n as usize),
            _ => None,
        })
        .or_else(|| {
            args.iter().find_map(|a| match (&a.name, &a.value) {
                (Some(n), Value::Int32(v)) if n == "overlap_chars" && *v >= 0 => Some(*v as usize),
                (Some(n), Value::Int64(v)) if n == "overlap_chars" && *v >= 0 => Some(*v as usize),
                _ => None,
            })
        });

    // Parse optional title (positional arg 4 or named "title")
    let title_arg = args
        .get(3)
        .and_then(|a| match (&a.name, &a.value) {
            (None, Value::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .or_else(|| {
            args.iter().find_map(|a| match (&a.name, &a.value) {
                (Some(n), Value::Text(v)) if n == "title" => Some(v.clone()),
                _ => None,
            })
        });

    let opts = ChunkOptions {
        max_chars: max_chars.unwrap_or(DEFAULT_MAX_CHARS),
        overlap_chars: overlap_chars.unwrap_or(DEFAULT_OVERLAP_CHARS),
    };

    let chunks = chunk_document(content, &opts).map_err(|e| anyhow!("{}", e))?;

    let title = title_arg.as_deref();

    let rows = chunks
        .into_iter()
        .map(|chunk| {
            let text = if let Some(t) = title {
                format_for_embedding(&chunk.text, t)
            } else {
                chunk.text
            };
            Row::new(vec![
                Value::Int32(chunk.index as i32),
                Value::Text(text),
                Value::Int32(chunk.pos as i32),
            ])
        })
        .collect();

    Ok(rows)
}

pub(crate) fn json_table_function_rows(
    func_upper: &str,
    args: &[EvaluatedTableFunctionArg],
) -> Result<Vec<Row>> {
    let Some(arg0) = args.first() else {
        return Err(anyhow!("{func_upper} requires at least 1 argument"));
    };
    if args.len() != 1 {
        return Err(anyhow!("{func_upper} requires exactly 1 argument"));
    }

    let json_str = match &arg0.value {
        Value::Text(s) | Value::Json(s) | Value::Jsonb(s) => Some(s.as_str()),
        Value::Null => None,
        other => {
            return Err(anyhow!(
                "{func_upper} requires json/jsonb argument, got {other}"
            ))
        }
    };
    let Some(json_str) = json_str else {
        return Ok(Vec::new());
    };

    let json_val: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| anyhow!("Invalid JSON: {e}"))?;

    let is_jsonb = func_upper.starts_with("JSONB_");
    match func_upper {
        "JSONB_OBJECT_KEYS" | "JSON_OBJECT_KEYS" => match json_val {
            serde_json::Value::Object(obj) => {
                let mut entries: Vec<String> = obj.into_iter().map(|(k, _)| k).collect();
                if is_jsonb {
                    entries.sort();
                }
                Ok(entries
                    .into_iter()
                    .map(|k| Row::new(vec![Value::Text(k)]))
                    .collect())
            }
            serde_json::Value::Array(_) => Err(anyhow!(
                "cannot call {} on an array",
                func_upper.to_lowercase()
            )),
            _ => Err(anyhow!(
                "cannot call {} on a scalar",
                func_upper.to_lowercase()
            )),
        },
        "JSONB_ARRAY_ELEMENTS" | "JSON_ARRAY_ELEMENTS" => match json_val {
            serde_json::Value::Array(arr) => Ok(arr
                .into_iter()
                .map(|v| {
                    let s = v.to_string();
                    let val = if func_upper == "JSON_ARRAY_ELEMENTS" {
                        Value::Json(s)
                    } else {
                        Value::Jsonb(s)
                    };
                    Row::new(vec![val])
                })
                .collect()),
            _ => Err(anyhow!("cannot extract elements from a non-array")),
        },
        "JSONB_ARRAY_ELEMENTS_TEXT" | "JSON_ARRAY_ELEMENTS_TEXT" => match json_val {
            serde_json::Value::Array(arr) => Ok(arr
                .into_iter()
                .map(|v| {
                    let val = match v {
                        serde_json::Value::String(s) => Value::Text(s),
                        serde_json::Value::Null => Value::Null,
                        other => Value::Text(other.to_string()),
                    };
                    Row::new(vec![val])
                })
                .collect()),
            _ => Err(anyhow!("cannot extract elements from a non-array")),
        },
        "JSONB_EACH" | "JSON_EACH" => match json_val {
            serde_json::Value::Object(obj) => {
                let mut entries: Vec<(String, serde_json::Value)> = obj.into_iter().collect();
                if is_jsonb {
                    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
                }
                Ok(entries
                    .into_iter()
                    .map(|(k, v)| {
                        let val = if func_upper == "JSON_EACH" {
                            Value::Json(v.to_string())
                        } else {
                            Value::Jsonb(v.to_string())
                        };
                        Row::new(vec![Value::Text(k), val])
                    })
                    .collect())
            }
            _ if is_jsonb => Err(anyhow!(
                "cannot call {} on a non-object",
                func_upper.to_lowercase()
            )),
            serde_json::Value::Array(_) => Err(anyhow!("cannot deconstruct an array as an object")),
            _ => Err(anyhow!("cannot deconstruct a scalar")),
        },
        "JSONB_EACH_TEXT" | "JSON_EACH_TEXT" => match json_val {
            serde_json::Value::Object(obj) => {
                let mut entries: Vec<(String, serde_json::Value)> = obj.into_iter().collect();
                if is_jsonb {
                    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
                }
                Ok(entries
                    .into_iter()
                    .map(|(k, v)| {
                        let val = match v {
                            serde_json::Value::String(s) => Value::Text(s),
                            serde_json::Value::Null => Value::Null,
                            other => Value::Text(other.to_string()),
                        };
                        Row::new(vec![Value::Text(k), val])
                    })
                    .collect())
            }
            _ if is_jsonb => Err(anyhow!(
                "cannot call {} on a non-object",
                func_upper.to_lowercase()
            )),
            serde_json::Value::Array(_) => Err(anyhow!("cannot deconstruct an array as an object")),
            _ => Err(anyhow!("cannot deconstruct a scalar")),
        },
        _ => Err(anyhow!("unsupported json table function: {func_upper}")),
    }
}

impl Executor {
    async fn evaluate_table_function_args_for_row(
        &self,
        args: &[TypedFunctionArg],
        row: &Row,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
        search_path: &[String],
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        qctx: &QueryContext,
    ) -> Result<Vec<EvaluatedTableFunctionArg>> {
        let mut evaluated = Vec::with_capacity(args.len());
        for arg in args {
            let (name_opt, typed_expr) = match arg {
                TypedFunctionArg::Positional(expr) => (None, expr),
                TypedFunctionArg::Named { name, expr } => (Some(name.clone()), expr),
            };
            let materialized = self
                .materialize_expr_for_row(
                    typed_expr,
                    row,
                    None,
                    None,
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    ctes,
                    qctx,
                )
                .await?;
            evaluated.push(EvaluatedTableFunctionArg {
                name: name_opt,
                value: eval_typed_expr(&materialized, row, qctx)?,
            });
        }
        Ok(evaluated)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn execute_table_function_rows(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
        search_path: &[String],
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        function_name: &str,
        args: &[TypedFunctionArg],
        row: &Row,
        output_schema: &TableSchema,
        qctx: &QueryContext,
        display_name: &str,
    ) -> Result<Vec<Row>> {
        let evaluated_args = self
            .evaluate_table_function_args_for_row(
                args,
                row,
                txn,
                db_id,
                sequence_values,
                search_path,
                ctes,
                qctx,
            )
            .await?;
        let func_upper = function_name.to_ascii_uppercase();

        let rows = if func_upper == "UNNEST" {
            let mut columns: Vec<Vec<Value>> = Vec::with_capacity(evaluated_args.len());
            for arg in &evaluated_args {
                let arr = match &arg.value {
                    Value::Array(a) => a.clone(),
                    Value::Null => vec![],
                    _ => return Err(anyhow!("UNNEST argument must be an array")),
                };
                columns.push(arr);
            }

            let max_len = columns.iter().map(|c| c.len()).max().unwrap_or(0);
            let has_ordinality = output_schema.columns.len() > columns.len();
            let mut rows = Vec::with_capacity(max_len);
            for i in 0..max_len {
                let mut values: Vec<Value> = columns
                    .iter()
                    .map(|col| col.get(i).cloned().unwrap_or(Value::Null))
                    .collect();
                if has_ordinality {
                    values.push(Value::Int64((i + 1) as i64));
                }
                rows.push(Row::new(values));
            }
            rows
        } else {
            let bridge_args = bridge_function_args(&evaluated_args);
            if func_upper == "GENERATE_SERIES" {
                let limit = crate::session_context::current_dml_limit_cap();
                let (_, rows) = self
                    .execute_generate_series(
                        &bridge_args,
                        function_name,
                        None,
                        0,
                        if limit > 0 { Some(limit) } else { None },
                    )
                    .await?;
                rows
            } else if func_upper == "_DB9_SYS_RECORD_MIGRATION" {
                let (_, rows) = self.execute_record_migration(txn, &bridge_args).await?;
                rows
            } else if func_upper == "CHUNK_TEXT" {
                chunk_text_rows(&evaluated_args)?
            } else if matches!(
                func_upper.as_str(),
                "JSONB_OBJECT_KEYS"
                    | "JSON_OBJECT_KEYS"
                    | "JSONB_ARRAY_ELEMENTS"
                    | "JSON_ARRAY_ELEMENTS"
                    | "JSONB_ARRAY_ELEMENTS_TEXT"
                    | "JSON_ARRAY_ELEMENTS_TEXT"
                    | "JSONB_EACH"
                    | "JSON_EACH"
                    | "JSONB_EACH_TEXT"
                    | "JSON_EACH_TEXT"
            ) {
                json_table_function_rows(&func_upper, &evaluated_args)?
            } else if bridge_args.is_empty()
                && is_virtual_table_backed_system_function(function_name)
            {
                let (_, rows) = self
                    .get_table_data(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        function_name,
                        ctes,
                    )
                    .await?;
                rows
            } else {
                let obj_name = names::object_name_from_str(function_name)?;
                if let Some(result) = self
                    .try_execute_extension_table_function(
                        txn,
                        db_id,
                        search_path,
                        &obj_name,
                        &bridge_args,
                        None,
                    )
                    .await?
                {
                    match result {
                        super::extensions::ExtensionTableFunctionResult::Batch(_, rows) => rows,
                        super::extensions::ExtensionTableFunctionResult::Streaming(_, mut op) => {
                            execute_operator_tree_with_ctes(
                                self,
                                &mut op,
                                txn,
                                self.store(),
                                db_id,
                                search_path,
                                sequence_values,
                                ctes,
                            )
                            .await?
                        }
                    }
                } else if let Some((_, rows)) = self
                    .try_execute_user_table_function(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &obj_name,
                        &bridge_args,
                        None,
                    )
                    .await?
                {
                    rows
                } else {
                    let scalar_arg_values: Vec<Value> =
                        evaluated_args.iter().map(|arg| arg.value.clone()).collect();
                    if let Some(result) = crate::sql::executor::execute_cron_scalar_function(
                        &self.store(),
                        txn,
                        db_id,
                        qctx.current_user.as_ref(),
                        qctx.database_name.as_ref(),
                        crate::extensions::context::is_superuser(),
                        function_name,
                        &scalar_arg_values,
                        self.tenant_keyspace(),
                    )
                    .await
                    {
                        vec![Row::new(vec![result?])]
                    } else {
                        let typed_expr = TypedExpr {
                            kind: TypedExprKind::FunctionCall {
                                func: ResolvedFunction {
                                    name: function_name.to_string(),
                                    kind: FunctionKind::Builtin,
                                    return_type: output_schema
                                        .columns
                                        .first()
                                        .map(|col| col.data_type.clone())
                                        .unwrap_or(DataType::Text),
                                },
                                args: args
                                    .iter()
                                    .map(|arg| match arg {
                                        TypedFunctionArg::Positional(expr) => expr.clone(),
                                        TypedFunctionArg::Named { expr, .. } => expr.clone(),
                                    })
                                    .collect(),
                                order_by: vec![],
                                filter: None,
                            },
                            data_type: output_schema
                                .columns
                                .first()
                                .map(|col| col.data_type.clone())
                                .unwrap_or(DataType::Text),
                        };
                        let val = eval_typed_expr(&typed_expr, row, qctx)?;
                        vec![Row::new(vec![val])]
                    }
                }
            }
        };

        let row_cap = crate::session_context::current_dml_limit_cap();
        if row_cap > 0 && rows.len() >= row_cap {
            return Err(SqlError::DmlTableScanTooLarge {
                message: format!(
                    "table function \"{}\" returned more than {} rows, \
                     exceeding the UPDATE FROM / DELETE USING auxiliary \
                     table limit (set db9.dml_table_scan_max_rows to \
                     adjust or 0 to disable)",
                    display_name,
                    row_cap.saturating_sub(1)
                ),
            }
            .into());
        }

        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::{bridge_function_args, json_table_function_rows, EvaluatedTableFunctionArg};
    use crate::model::Value;
    use sqlparser::ast::{FunctionArg, FunctionArgExpr};

    #[test]
    fn bridge_function_args_preserves_named_and_unnamed_shape() {
        let evaluated = vec![
            EvaluatedTableFunctionArg {
                name: None,
                value: Value::Int32(42),
            },
            EvaluatedTableFunctionArg {
                name: Some("step".to_string()),
                value: Value::Int32(5),
            },
        ];

        let bridged = bridge_function_args(&evaluated);
        assert_eq!(bridged.len(), 2);
        match &bridged[0] {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => {
                assert_eq!(expr.to_string(), "42");
            }
            other => panic!("expected unnamed arg, got {:?}", other),
        }
        match &bridged[1] {
            FunctionArg::Named { name, arg } => {
                assert_eq!(name.value, "step");
                match arg {
                    FunctionArgExpr::Expr(expr) => assert_eq!(expr.to_string(), "5"),
                    _ => panic!("expected named expression arg"),
                }
            }
            other => panic!("expected named arg, got {:?}", other),
        }
    }

    #[test]
    fn json_table_function_rows_jsonb_each_expands_object() {
        let args = vec![EvaluatedTableFunctionArg {
            name: None,
            value: Value::Jsonb(r#"{"a":1,"b":2}"#.to_string()),
        }];
        let rows = json_table_function_rows("JSONB_EACH", &args).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].values,
            vec![Value::Text("a".into()), Value::Jsonb("1".into())]
        );
        assert_eq!(
            rows[1].values,
            vec![Value::Text("b".into()), Value::Jsonb("2".into())]
        );
    }

    #[test]
    fn json_table_function_rows_jsonb_each_text_null_and_empty_string_distinct() {
        let args = vec![EvaluatedTableFunctionArg {
            name: None,
            value: Value::Jsonb(r#"{"empty":"","nullv":null}"#.to_string()),
        }];
        let rows = json_table_function_rows("JSONB_EACH_TEXT", &args).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].values,
            vec![Value::Text("empty".into()), Value::Text("".into())]
        );
        assert_eq!(
            rows[1].values,
            vec![Value::Text("nullv".into()), Value::Null]
        );
    }
}
