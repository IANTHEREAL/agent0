//! TypedExpr rewrite helpers.
//!
//! Rewrites typed expression trees for executor-time materialization tasks.

use crate::model::Value;
use crate::sql::analyzer::types::{TypedExpr, TypedExprKind};
use crate::sql::expr::static_eval::eval_static_typed_expr;
use crate::sql::expr::traverse::{map_children_async, AsyncExprTransform};
use crate::sql::query_context::QueryContext;
use crate::sql::sequences;
use crate::sql::sequences::SequenceSession;
use crate::storage::TikvStore;
use anyhow::{anyhow, Result};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tikv_client::Transaction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
enum SequenceFunction {
    NextVal,
    CurrVal,
    SetVal,
}

struct SequenceMaterializeCtx<'a> {
    store: &'a Arc<TikvStore>,
    txn: &'a mut Transaction,
    db_id: u64,
    sequence_values: &'a mut SequenceSession,
    search_path: &'a [String],
    qctx: &'a QueryContext,
}

impl AsyncExprTransform for SequenceMaterializeCtx<'_> {
    fn transform_expr<'a>(
        &'a mut self,
        expr: &'a TypedExpr,
    ) -> Pin<Box<dyn Future<Output = Result<TypedExpr>> + Send + 'a>> {
        Box::pin(async move {
            let kind = match &expr.kind {
                // Leaves that never contain sequences: short-circuit
                TypedExprKind::Constant(_)
                | TypedExprKind::ColumnRef { .. }
                | TypedExprKind::ScalarSubquery(_)
                | TypedExprKind::ArraySubquery(_)
                | TypedExprKind::Exists { .. }
                | TypedExprKind::Default
                | TypedExprKind::Parameter { .. } => return Ok(expr.clone()),

                // FunctionCall: check for sequence functions
                TypedExprKind::FunctionCall { func, args, .. } => {
                    match sequence_function_kind(&func.name) {
                        Some(SequenceFunction::NextVal) => {
                            let seq_name = self.resolve_sequence_name_from_typed_args(args).await?;
                            let val = self
                                .store
                                .nextval_sequence(self.txn, self.db_id, &seq_name)
                                .await?;
                            self.sequence_values.record_nextval(seq_name, val);
                            TypedExprKind::Constant(Value::Int64(val))
                        }
                        Some(SequenceFunction::CurrVal) => {
                            let seq_name = self.resolve_sequence_name_from_typed_args(args).await?;
                            let val = self.sequence_values.currval(&seq_name)?;
                            TypedExprKind::Constant(Value::Int64(val))
                        }
                        Some(SequenceFunction::SetVal) => {
                            let seq_name = self.resolve_sequence_name_from_typed_args(args).await?;
                            if args.len() < 2 {
                                return Err(anyhow!("setval requires at least 2 arguments"));
                            }

                            let set_val = match eval_static_typed_expr(&args[1], self.qctx)? {
                                Value::Int32(n) => i64::from(n),
                                Value::Int64(n) => n,
                                other => {
                                    return Err(anyhow!(
                                        "setval: value must be integer, got {}",
                                        other
                                    ));
                                }
                            };

                            let is_called = if args.len() >= 3 {
                                parse_setval_is_called(eval_static_typed_expr(
                                    &args[2], self.qctx,
                                )?)?
                            } else {
                                true
                            };

                            self.store
                                .setval_sequence(
                                    self.txn, self.db_id, &seq_name, set_val, is_called,
                                )
                                .await?;
                            self.sequence_values
                                .record_setval(seq_name, set_val, is_called);
                            TypedExprKind::Constant(Value::Int64(set_val))
                        }
                        // Non-sequence function: canonical child recursion
                        None => map_children_async(expr, self).await?,
                    }
                }

                // Everything else: canonical async child recursion
                _ => map_children_async(expr, self).await?,
            };

            Ok(TypedExpr {
                kind,
                data_type: expr.data_type.clone(),
            })
        })
    }
}

impl<'a> SequenceMaterializeCtx<'a> {
    fn resolve_sequence_name_from_typed_args<'b>(
        &'b mut self,
        args: &'b [TypedExpr],
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'b>> {
        Box::pin(async move {
            if args.is_empty() {
                return Err(anyhow!("sequence function requires at least 1 argument"));
            }

            let name_val = eval_static_typed_expr(&args[0], self.qctx)?;
            sequences::resolve_sequence_full_name_from_value(
                self.store,
                self.txn,
                self.db_id,
                self.search_path,
                name_val,
            )
            .await
        })
    }
}

fn sequence_function_kind(name: &str) -> Option<SequenceFunction> {
    match name.to_ascii_uppercase().as_str() {
        "NEXTVAL" => Some(SequenceFunction::NextVal),
        "CURRVAL" => Some(SequenceFunction::CurrVal),
        "SETVAL" => Some(SequenceFunction::SetVal),
        _ => None,
    }
}

fn parse_setval_is_called(v: Value) -> Result<bool> {
    match v {
        Value::Boolean(b) => Ok(b),
        Value::Text(s) => Ok(matches!(
            s.to_lowercase().as_str(),
            "true" | "t" | "1" | "yes" | "y"
        )),
        other => Err(anyhow!("setval: is_called must be boolean, got {}", other)),
    }
}

/// Rewrite sequence function calls (`nextval`/`currval`/`setval`) into constants.
pub fn materialize_sequences_in_typed_expr<'a>(
    store: &'a Arc<TikvStore>,
    txn: &'a mut Transaction,
    db_id: u64,
    sequence_values: &'a mut SequenceSession,
    search_path: &'a [String],
    expr: &'a TypedExpr,
    qctx: &'a QueryContext,
) -> Pin<Box<dyn Future<Output = Result<TypedExpr>> + Send + 'a>> {
    Box::pin(async move {
        let mut ctx = SequenceMaterializeCtx {
            store,
            txn,
            db_id,
            sequence_values,
            search_path,
            qctx,
        };
        ctx.transform_expr(expr).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_function_kind_is_case_insensitive() {
        assert_eq!(
            sequence_function_kind("nextval"),
            Some(SequenceFunction::NextVal)
        );
        assert_eq!(
            sequence_function_kind("CURRVAL"),
            Some(SequenceFunction::CurrVal)
        );
        assert_eq!(
            sequence_function_kind("SetVal"),
            Some(SequenceFunction::SetVal)
        );
        assert_eq!(sequence_function_kind("abs"), None);
    }

    #[test]
    fn parse_setval_is_called_accepts_bool_and_text() {
        assert!(parse_setval_is_called(Value::Boolean(true)).unwrap());
        assert!(!parse_setval_is_called(Value::Boolean(false)).unwrap());
        assert!(parse_setval_is_called(Value::Text("YES".to_string())).unwrap());
        assert!(!parse_setval_is_called(Value::Text("n".to_string())).unwrap());
    }

    #[test]
    fn parse_setval_is_called_rejects_non_bool() {
        let err = parse_setval_is_called(Value::Int32(1)).unwrap_err();
        assert!(err.to_string().contains("is_called must be boolean"));
    }
}
